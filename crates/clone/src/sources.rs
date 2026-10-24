//! Keeping the source still while it's copied. A disk's volumes are closed
//! for the copy; on the disk Windows runs from (which can't be closed), its
//! NTFS and ReFS volumes are read from snapshots Windows takes, and the rest
//! (the EFI system partition, a recovery partition) as they are.

use std::collections::HashMap;
use std::sync::Arc;

use datareco_fs::Allocation;
use datareco_io::{
    ClosedVolumes, DeviceList, Disk, FileSource, SectorSource, Snapshot, SourceSpec, VolumeInfo,
    close_volumes, volume_name,
};
use datareco_partitions::edit::PlacedPart;
use serde::Serialize;

use crate::{CloneError, CloneSources, allocation, refused};

/// How a copy reads its source, to show before it starts.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Reading {
    /// Volumes closed while it copies ("E: (Photos)").
    pub closed: Vec<String>,
    /// Volumes read from a snapshot Windows takes ("C:").
    pub snapshots: Vec<String>,
    /// Volumes Windows keeps using, read as they are.
    pub live: Vec<String>,
    /// Why the source can't be copied safely, if it can't.
    pub problem: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum How {
    /// Nothing mounted there: read from the disk.
    Disk,
    Close,
    Snapshot,
    Live,
}

/// Whether `spec` is the disk Windows runs from.
pub fn runs_windows(spec: &SourceSpec, devices: &DeviceList) -> bool {
    matches!(spec, SourceSpec::PhysicalDisk(n) if devices.system_disks().contains(n))
}

/// The volume Windows has mounted on each partition, and how to read it.
fn decide<'a>(
    spec: &SourceSpec,
    parts: &[PlacedPart],
    sector_size: u32,
    devices: &'a DeviceList,
) -> Vec<(How, Option<&'a VolumeInfo>)> {
    let SourceSpec::PhysicalDisk(number) = spec else {
        return parts.iter().map(|_| (How::Disk, None)).collect();
    };
    let system = devices.system_disks().contains(number);
    parts
        .iter()
        .map(|part| {
            let offset = part.from_lba * u64::from(sector_size);
            let volume = devices.volumes.iter().find(|v| {
                v.extents
                    .iter()
                    .any(|e| e.disk == *number && e.offset == offset)
            });
            let how = match volume {
                None => How::Disk,
                Some(_) if !system => How::Close,
                Some(v) => {
                    let snapshots = v.file_system.as_deref().is_some_and(|fs| {
                        fs.eq_ignore_ascii_case("NTFS") || fs.eq_ignore_ascii_case("ReFS")
                    });
                    if snapshots && !v.mount_points.is_empty() {
                        How::Snapshot
                    } else {
                        How::Live
                    }
                }
            };
            (how, volume)
        })
        .collect()
}

/// How the copy will read `parts` of `source`, and whether it safely can.
pub fn plan_reading(
    spec: &SourceSpec,
    source: &Disk,
    parts: &[PlacedPart],
    devices: &DeviceList,
) -> Reading {
    let sector_size = source.sector_size();
    let mut reading = Reading::default();
    for ((how, volume), part) in decide(spec, parts, sector_size, devices)
        .into_iter()
        .zip(parts)
    {
        let Some(volume) = volume else { continue };
        // Volumes without a drive letter go by their partition.
        let name = if volume.mount_points.is_empty() {
            part.what.clone()
        } else {
            volume_name(volume)
        };
        match how {
            How::Close => reading.closed.push(name),
            How::Snapshot => {
                // A snapshot reads BitLocker volumes decrypted, which the
                // encrypted copy's boot sector wouldn't match.
                let mut boot = [0u8; 11];
                let encrypted = source
                    .read_at(part.from_lba * u64::from(sector_size), &mut boot)
                    .is_ok()
                    && &boot[3..11] == b"-FVE-FS-";
                if encrypted {
                    reading.problem = Some(format!(
                        "{name} is encrypted with BitLocker. Turn BitLocker off for it \
                         (Windows decrypts the drive), then copy it."
                    ));
                }
                reading.snapshots.push(name);
            }
            How::Live => reading.live.push(name),
            How::Disk => {}
        }
    }
    reading
}

/// What the copy reads from, held while it runs: the source open, its
/// volumes closed, snapshots taken. Dropping it opens the volumes again
/// and deletes the snapshots.
pub struct Prepared {
    pub sources: CloneSources,
    pub reading: Reading,
    spec: SourceSpec,
    /// By piece. Declared after `sources`, so their handles close first.
    snapshots: HashMap<usize, Snapshot>,
    _closed: Option<ClosedVolumes>,
}

/// Gets `spec` ready to copy `parts` from: closes its volumes, or takes
/// snapshots of those Windows can't close, and opens it.
pub fn prepare(
    spec: &SourceSpec,
    source: &Disk,
    parts: &[PlacedPart],
    devices: &DeviceList,
) -> Result<Prepared, CloneError> {
    let reading = plan_reading(spec, source, parts, devices);
    if let Some(problem) = &reading.problem {
        return Err(refused(problem.clone()));
    }
    let decided = decide(spec, parts, source.sector_size(), devices);
    let to_close: Vec<&VolumeInfo> = decided
        .iter()
        .filter(|(how, _)| *how == How::Close)
        .filter_map(|(_, volume)| *volume)
        .collect();
    let closed = if to_close.is_empty() {
        None
    } else {
        Some(close_volumes(&to_close, &spec.display_name())?)
    };
    let mut snapshots = HashMap::new();
    let mut readers: HashMap<usize, Box<dyn SectorSource>> = HashMap::new();
    for (index, (how, volume)) in decided.iter().enumerate() {
        if let (How::Snapshot, Some(volume)) = (how, volume) {
            let snapshot = Snapshot::take(volume)?;
            readers.insert(index, Box::new(FileSource::open(&snapshot.source())?));
            snapshots.insert(index, snapshot);
        }
    }
    Ok(Prepared {
        sources: CloneSources {
            disk: Box::new(FileSource::open(spec)?),
            snapshots: readers,
        },
        reading,
        spec: spec.clone(),
        snapshots,
        _closed: closed,
    })
}

impl Prepared {
    /// What each of `parts` holds, read the way the copy reads it (so from
    /// the snapshots where there are some).
    pub fn allocations(&self, parts: &[PlacedPart], sector_size: u32) -> Vec<Option<Allocation>> {
        let disk = Disk::open(&self.spec).ok().map(Arc::new);
        parts
            .iter()
            .enumerate()
            .map(|(index, part)| match self.snapshots.get(&index) {
                Some(snapshot) => {
                    let volume = Disk::open(&snapshot.source()).ok()?;
                    allocation(Arc::new(volume), 0)
                }
                None => allocation(
                    Arc::clone(disk.as_ref()?),
                    part.from_lba * u64::from(sector_size),
                ),
            })
            .collect()
    }
}
