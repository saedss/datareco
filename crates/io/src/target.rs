//! Where copies write: image files and whole disks.
//!
//! This is the only part of the crate that writes. Image files are only ever
//! created new, or reopened to carry on a copy into them. A disk can only be
//! written after [`check_disk_target`] has passed it and the person has typed
//! its name ([`DiskTarget::open`]).

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::aligned::AlignedBuf;
use crate::devices::DeviceList;
use crate::error::{Error, Result};
use crate::spec::SourceSpec;

/// Somewhere sectors are copied to.
pub trait SectorTarget: Send {
    /// Writes start on a multiple of this many bytes.
    fn sector_size(&self) -> u32;

    /// How many bytes fit, or `None` for targets that grow (files).
    fn capacity(&self) -> Option<u64>;

    /// Writes `data` at byte `offset`, a multiple of [`SectorTarget::sector_size`].
    /// Disks pad a partial last sector with zeros.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;

    /// Makes sure everything written so far is stored.
    fn flush(&mut self) -> io::Result<()>;
}

// ---------------------------------------------------------------- image files

/// A disk image file being written.
pub struct ImageTarget {
    file: File,
    path: PathBuf,
}

impl ImageTarget {
    /// Creates a new image file `size` bytes long. Never replaces a file.
    /// Parts not written yet read as zeros and take no space where the file
    /// system supports it (NTFS sparse files).
    pub fn create(path: &Path, size: u64) -> Result<ImageTarget> {
        let name = path.display().to_string();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|e| match e.kind() {
                io::ErrorKind::AlreadyExists => Error::refused(format!(
                    "{name} already exists. Pick a new file name, or delete that file first."
                )),
                _ => Error::from_io(&name, e),
            })?;
        // Without this, NTFS writes zeros over everything before the first
        // write, which takes hours for a big disk copied backwards.
        let _ = crate::platform::make_sparse(&file);
        file.set_len(size).map_err(|e| Error::from_io(&name, e))?;
        Ok(ImageTarget {
            file,
            path: path.to_owned(),
        })
    }

    /// Opens an existing image file to carry on copying into it, keeping
    /// what it holds.
    pub fn resume(path: &Path, size: u64) -> Result<ImageTarget> {
        let name = path.display().to_string();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| Error::from_io(&name, e))?;
        let length = file.metadata().map_err(|e| Error::from_io(&name, e))?.len();
        if length != size {
            file.set_len(size).map_err(|e| Error::from_io(&name, e))?;
        }
        Ok(ImageTarget {
            file,
            path: path.to_owned(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SectorTarget for ImageTarget {
    fn sector_size(&self) -> u32 {
        512
    }

    fn capacity(&self) -> Option<u64> {
        None
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_data()
    }
}

// ---------------------------------------------------------------- memory, for tests

/// A target in memory, for tests. Fixed-size ones behave like disks.
pub struct MemoryTarget {
    data: Vec<u8>,
    sector_size: u32,
    fixed: bool,
}

impl MemoryTarget {
    /// A target that grows, like an image file.
    pub fn growing(sector_size: u32) -> MemoryTarget {
        MemoryTarget {
            data: Vec::new(),
            sector_size,
            fixed: false,
        }
    }

    /// A disk of `size` bytes, every byte `old` to start with.
    pub fn disk(size: usize, sector_size: u32, old: u8) -> MemoryTarget {
        MemoryTarget {
            data: vec![old; size],
            sector_size,
            fixed: true,
        }
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

impl SectorTarget for MemoryTarget {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn capacity(&self) -> Option<u64> {
        self.fixed.then_some(self.data.len() as u64)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let sector = u64::from(self.sector_size);
        if !offset.is_multiple_of(sector) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write not on a sector boundary",
            ));
        }
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let mut end = start + data.len();
        if self.fixed {
            end = end.next_multiple_of(self.sector_size as usize);
            if end > self.data.len() {
                return Err(io::Error::other("write past the end of the disk"));
            }
            self.data[start + data.len()..end].fill(0);
        } else if end > self.data.len() {
            self.data.resize(end, 0);
        }
        self.data[start..start + data.len()].copy_from_slice(data);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- disks

/// A disk that passed the safety checks for being written over, and what
/// writing to it will do. Show it to the person before asking them to
/// confirm.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskTargetCheck {
    pub number: u32,
    /// "Disk 2".
    pub name: String,
    pub model: String,
    pub size: u64,
    pub sector_size: u32,
    /// The volumes on it, which will be closed and overwritten, as people
    /// know them: "E: (Photos)", or their size when they have no letter.
    pub volumes: Vec<String>,
    /// Things worth knowing that don't stop the copy.
    pub warnings: Vec<String>,
    /// What the person types to confirm: "disk2".
    pub confirmation: String,
    #[serde(skip)]
    volume_paths: Vec<String>,
}

/// What is being copied, for checking a target disk against it.
#[derive(Clone, Debug)]
pub struct CopySource<'a> {
    pub spec: &'a SourceSpec,
    pub size: u64,
    pub sector_size: u32,
}

/// Checks that disk `number` may be written over with a copy of `source`.
///
/// Refuses the disk being copied from, the disk Windows runs from, disks
/// holding any of `keep` (the source image file, the map file), disks too
/// small for the copy, and disks with larger sectors than the source (the
/// copy couldn't line up).
pub fn check_disk_target(
    number: u32,
    source: &CopySource<'_>,
    keep: &[&Path],
    devices: &DeviceList,
) -> Result<DiskTargetCheck> {
    let name = SourceSpec::PhysicalDisk(number).display_name();
    let disk = devices
        .disks
        .iter()
        .find(|d| d.number == number)
        .ok_or_else(|| Error::NotFound { name: name.clone() })?;
    if devices.disks_of_source(source.spec).contains(&number) {
        return Err(Error::refused(format!(
            "{name} is the disk you're copying from. Pick another disk to copy to."
        )));
    }
    if devices.system_disks().contains(&number) {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        return Err(Error::refused(format!(
            "{name} holds Windows ({drive}). DataReco won't write over the disk this computer \
             runs from."
        )));
    }
    for path in keep {
        if devices.disks_of_path(path).contains(&number) {
            return Err(Error::refused(format!(
                "{name} holds {}, which this copy needs. Put it on another disk.",
                path.display()
            )));
        }
    }
    if disk.size < source.size {
        return Err(Error::refused(format!(
            "{name} ({}) is smaller than what you're copying ({}).",
            human_size(disk.size),
            human_size(source.size)
        )));
    }
    if disk.sector_size > source.sector_size {
        return Err(Error::refused(format!(
            "{name} has {}-byte sectors and the source has {}-byte sectors, so the copy can't \
             line up with them.",
            disk.sector_size, source.sector_size
        )));
    }
    let mut warnings = Vec::new();
    if disk.sector_size < source.sector_size {
        warnings.push(format!(
            "{name} has {}-byte sectors and the source has {}-byte sectors: Windows may not find \
             the partitions on the copy.",
            disk.sector_size, source.sector_size
        ));
    }
    if disk.size > source.size {
        warnings.push(format!(
            "{name} is {} larger than the source; that space keeps what it holds now.",
            human_size(disk.size - source.size)
        ));
    }
    let on_disk: Vec<_> = devices
        .volumes
        .iter()
        .filter(|v| v.extents.iter().any(|e| e.disk == number))
        .collect();
    let volumes = on_disk
        .iter()
        .map(|v| {
            let place = v.mount_points.first().map_or_else(
                || format!("A {} volume", v.size.map_or_else(String::new, human_size)),
                |m| m.trim_end_matches('\\').to_owned(),
            );
            match &v.label {
                Some(label) => format!("{place} ({label})"),
                None => place,
            }
        })
        .collect();
    Ok(DiskTargetCheck {
        number,
        name,
        model: disk.model.clone(),
        size: disk.size,
        sector_size: disk.sector_size,
        volumes,
        warnings,
        confirmation: format!("disk{number}"),
        volume_paths: on_disk.iter().map(|v| v.path.clone()).collect(),
    })
}

/// Refuses a file (an image or map file) on the disk being copied from:
/// writing there could overwrite what the copy is trying to save.
pub fn check_outside_source(source: &SourceSpec, path: &Path, devices: &DeviceList) -> Result<()> {
    let source_disks = devices.disks_of_source(source);
    if devices
        .disks_of_path(path)
        .iter()
        .any(|disk| source_disks.contains(disk))
    {
        return Err(Error::refused(format!(
            "{} is on the disk you're copying from. Writing there could overwrite what you're \
             trying to save: pick a folder on another disk.",
            path.display()
        )));
    }
    Ok(())
}

/// A whole disk open for writing. Its volumes stay locked (closed to
/// Windows and every program) until it is dropped; then Windows reads its
/// new partitions.
pub struct DiskTarget {
    file: File,
    _locks: Vec<File>,
    size: u64,
    sector_size: u32,
    buffer: AlignedBuf,
}

/// Bytes written to a disk at a time.
const DISK_WRITE_CHUNK: usize = 1024 * 1024;

impl DiskTarget {
    /// Opens a checked disk for writing, once the person has typed its
    /// confirmation (`disk2`). Closes its volumes first: refuses if a
    /// program has files open on them. Needs administrator rights.
    pub fn open(check: &DiskTargetCheck, typed: &str) -> Result<DiskTarget> {
        if !typed.trim().eq_ignore_ascii_case(&check.confirmation) {
            return Err(Error::refused(format!(
                "Type {} to confirm that everything on {} may be erased.",
                check.confirmation, check.name
            )));
        }
        let opened = crate::platform::open_disk_for_writing(check.number, &check.volume_paths)?;
        if opened.size != check.size || opened.sector_size != check.sector_size {
            return Err(Error::refused(format!(
                "{} changed since it was checked. Check it again.",
                check.name
            )));
        }
        Ok(DiskTarget {
            file: opened.file,
            _locks: opened.locks,
            size: opened.size,
            sector_size: opened.sector_size,
            buffer: AlignedBuf::zeroed(DISK_WRITE_CHUNK),
        })
    }
}

/// A disk opened for writing, with its volumes locked.
pub(crate) struct OpenedDisk {
    pub(crate) file: File,
    pub(crate) locks: Vec<File>,
    pub(crate) size: u64,
    pub(crate) sector_size: u32,
}

impl SectorTarget for DiskTarget {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn capacity(&self) -> Option<u64> {
        Some(self.size)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let sector = self.sector_size as usize;
        if !offset.is_multiple_of(u64::from(self.sector_size)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "write not on a sector boundary",
            ));
        }
        let mut position = offset;
        for chunk in data.chunks(DISK_WRITE_CHUNK) {
            let padded = chunk.len().next_multiple_of(sector);
            let end = position
                .checked_add(padded as u64)
                .filter(|&end| end <= self.size)
                .ok_or_else(|| io::Error::other("write past the end of the disk"))?;
            self.buffer[..chunk.len()].copy_from_slice(chunk);
            self.buffer[chunk.len()..padded].fill(0);
            self.file.seek(SeekFrom::Start(position))?;
            self.file.write_all(&self.buffer[..padded])?;
            position = end;
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}

impl Drop for DiskTarget {
    fn drop(&mut self) {
        let _ = self.file.sync_all();
        crate::platform::update_disk_properties(&self.file);
    }
}

/// "1.5 GiB", for messages.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut whole = bytes;
    while whole >= 1024 && unit + 1 < UNITS.len() {
        whole /= 1024;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} bytes");
    }
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / (1u64 << (10 * unit)) as f64;
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::PhysicalDiskInfo;
    use crate::devices::tests::{devices, volume};

    fn disk(number: u32, size: u64, sector_size: u32) -> PhysicalDiskInfo {
        PhysicalDiskInfo {
            number,
            path: format!(r"\\.\PhysicalDrive{number}"),
            model: format!("Model {number}"),
            serial: None,
            bus: "USB".to_owned(),
            removable: false,
            size,
            sector_size,
            physical_sector_size: None,
            firmware: None,
            solid_state: None,
        }
    }

    /// Disk 0 holds C: (Windows), disk 1 holds D:, disk 2 is mounted in a
    /// C: folder, disk 3 is empty, disk 4 has 4096-byte sectors, disk 5 is small.
    fn computer() -> DeviceList {
        let mut devices = devices();
        devices.volumes.push(volume(r"\\?\Volume{e}", &[], 1));
        devices.disks = vec![
            disk(0, 1 << 40, 512),
            disk(1, 1 << 40, 512),
            disk(2, 1 << 40, 512),
            disk(3, 1 << 40, 512),
            disk(4, 1 << 40, 4096),
            disk(5, 1 << 20, 512),
        ];
        devices
    }

    fn source(spec: &SourceSpec) -> CopySource<'_> {
        CopySource {
            spec,
            size: 1 << 30,
            sector_size: 512,
        }
    }

    fn refusal(result: Result<DiskTargetCheck>) -> String {
        match result {
            Err(Error::Refused { reason }) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn refuses_unsafe_disks() {
        let devices = computer();
        let disk1 = SourceSpec::PhysicalDisk(1);
        let reason = refusal(check_disk_target(1, &source(&disk1), &[], &devices));
        assert!(reason.contains("copying from"), "{reason}");
        let d = SourceSpec::Volume("D:".to_owned());
        let reason = refusal(check_disk_target(1, &source(&d), &[], &devices));
        assert!(reason.contains("copying from"), "{reason}");
        let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        if system.eq_ignore_ascii_case("C:") {
            let reason = refusal(check_disk_target(0, &source(&disk1), &[], &devices));
            assert!(reason.contains("holds Windows"), "{reason}");
        }
        if cfg!(windows) {
            let image = SourceSpec::Image(PathBuf::from(r"C:\Mounted\disk.img"));
            let map = Path::new(r"C:\Mounted\disk.map");
            let reason = refusal(check_disk_target(2, &source(&image), &[map], &devices));
            assert!(reason.contains(r"C:\Mounted\disk.map"), "{reason}");
        }
        let reason = refusal(check_disk_target(4, &source(&disk1), &[], &devices));
        assert!(reason.contains("4096-byte sectors"), "{reason}");
        let reason = refusal(check_disk_target(5, &source(&disk1), &[], &devices));
        assert!(reason.contains("smaller"), "{reason}");
        assert!(matches!(
            check_disk_target(9, &source(&disk1), &[], &devices),
            Err(Error::NotFound { .. })
        ));
    }

    #[test]
    fn passes_a_safe_disk_and_lists_what_it_holds() {
        let devices = computer();
        let d = SourceSpec::Volume("D:".to_owned());
        let check = check_disk_target(3, &source(&d), &[], &devices).unwrap();
        assert_eq!(check.confirmation, "disk3");
        assert!(check.volumes.is_empty());
        assert_eq!(check.warnings.len(), 1, "{:?}", check.warnings);

        let disk3 = SourceSpec::PhysicalDisk(3);
        let check = check_disk_target(1, &source(&disk3), &[], &devices).unwrap();
        assert_eq!(check.volumes, ["D:", "A 1.0 GiB volume"]);
    }

    #[test]
    fn disks_need_their_name_typed() {
        let devices = computer();
        let d = SourceSpec::Volume("D:".to_owned());
        let check = check_disk_target(3, &source(&d), &[], &devices).unwrap();
        for typed in ["", "yes", "disk 3", "disk4"] {
            let reason = refusal(DiskTarget::open(&check, typed).map(|_| check.clone()));
            assert!(reason.contains("Type disk3"), "{reason}");
        }
    }

    #[test]
    fn memory_disks_pad_and_refuse_writes_past_their_end() {
        let mut target = MemoryTarget::disk(2048, 512, 0xEE);
        target.write_at(512, &[1; 700]).unwrap();
        assert_eq!(target.data()[..512], [0xEE; 512]);
        assert_eq!(target.data()[512..1212], [1; 700]);
        assert_eq!(target.data()[1212..1536], [0; 324]);
        assert_eq!(target.data()[1536..], [0xEE; 512]);
        assert!(target.write_at(1536, &[1; 513]).is_err());
        assert!(target.write_at(100, &[1; 10]).is_err());
    }

    #[test]
    fn image_files_are_created_new_and_resumed() {
        let path = std::env::temp_dir().join(format!("datareco-target-{}.img", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut image = ImageTarget::create(&path, 4096).unwrap();
        image.write_at(1024, b"hello").unwrap();
        image.flush().unwrap();
        drop(image);
        assert!(matches!(
            ImageTarget::create(&path, 4096),
            Err(Error::Refused { .. })
        ));
        let mut image = ImageTarget::resume(&path, 4096).unwrap();
        image.write_at(0, b"start").unwrap();
        drop(image);
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 4096);
        assert_eq!(&data[..5], b"start");
        assert_eq!(&data[1024..1029], b"hello");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sizes_for_people() {
        assert_eq!(human_size(100), "100 bytes");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(1 << 30), "1.0 GiB");
    }
}
