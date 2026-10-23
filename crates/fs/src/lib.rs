//! File-system readers: browse folders, read files, and find deleted entries.
//! NTFS, FAT12/16/32 and exFAT for now; ext, HFS+, APFS, ReFS and btrfs later.
//!
//! [`open`] recognizes the file system at an offset of a [`Disk`] and returns
//! a [`FileSystem`]: list folders (deleted entries included), read files and
//! NTFS streams, and find where their data is on the disk.
//!
//! Everything only reads, and disk data is untrusted: parsers use
//! bounds-checked reads and report damage as errors, never panics.
//!
//! Damaged volumes: [`open_rebuilt`] opens one from what a scan of the disk
//! found ([`Rebuild`]): a backup boot sector, MFT records found elsewhere,
//! or a FAT volume's layout worked out from its folders. The [`clues`]
//! module says what a sector shows, for such a scan.

mod bytes;
mod clusters;
mod error;
mod exfat;
mod fat;
mod ntfs;
mod time;
mod volume;

#[cfg(test)]
mod tests_exfat;
#[cfg(test)]
mod tests_fat;
#[cfg(test)]
mod tests_ntfs;
#[cfg(test)]
mod tests_rebuild;
/// Builders for small in-memory FAT, exFAT and NTFS volumes, for tests.
#[cfg(any(test, feature = "testutil"))]
#[doc(hidden)]
pub mod testutil;

use std::collections::BTreeMap;
use std::sync::Arc;

use datareco_io::Disk;
pub use datareco_partitions::FsKind;
use serde::{Deserialize, Serialize};

pub use error::FsError;
pub use fat::{FatLayout, FatRoot};

use crate::volume::Volume;

/// The virtual folder listing files whose folder is gone.
pub(crate) const LOST_FILES: &str = "Lost files";

/// Identifies a file or folder within one file system. It round-trips
/// through JSON, so the desktop app can hand it back.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum EntryId {
    /// FAT or exFAT data starting at `cluster`. With `chain`, the FAT says
    /// which clusters follow; without it (deleted entries, and exFAT files
    /// stored in one piece) the clusters follow one another. `length` is the
    /// size in bytes; 0 for folders whose size isn't recorded.
    Clusters {
        cluster: u32,
        length: u64,
        chain: bool,
    },
    /// The fixed-size root folder of FAT12 and FAT16.
    FixedRoot,
    /// An NTFS MFT record.
    Record { number: u64 },
    /// "Lost files": files and folders whose folder no longer exists.
    Orphans,
    /// NTFS: a folder whose own record is gone, standing for what's in it.
    LostFolder { number: u64 },
    /// A file found by its contents: `length` bytes at byte `offset` of the disk.
    Span { offset: u64, length: u64 },
    /// A folder of a listing that isn't on the disk (raw recovery's file types).
    Group { index: u32 },
}

/// Windows file attribute bits, as `Entry::attributes` uses them.
pub mod attributes {
    pub const READ_ONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const SYSTEM: u32 = 0x4;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const SPARSE: u32 = 0x200;
    pub const COMPRESSED: u32 = 0x800;
    pub const ENCRYPTED: u32 = 0x4000;
}

/// A named data stream of a file (NTFS alternate data streams).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamInfo {
    pub name: String,
    pub size: u64,
}

/// A file or folder in a listing. The desktop app hands entries back to
/// recover them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: EntryId,
    pub name: String,
    pub is_dir: bool,
    /// Size in bytes (0 for folders).
    pub size: u64,
    pub deleted: bool,
    /// Times in seconds since 1970-01-01 UTC. FAT stores local time without
    /// a time zone; it is taken as UTC.
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    /// Windows attribute bits (see [`attributes`]).
    pub attributes: u32,
    /// Named data streams besides the main one.
    pub streams: Vec<StreamInfo>,
    /// Something to know before trusting the contents (a deleted file's
    /// clusters are a guess, some are in use again, it's encrypted, …).
    pub note: Option<String>,
}

/// A stretch of a file's data on the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Extent {
    /// Byte offset from the start of the disk.
    pub offset: u64,
    pub length: u64,
    /// No data is stored here (an NTFS sparse run); it reads as zeros.
    pub sparse: bool,
}

/// What kind of file system a volume has, and its size and label.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeInfo {
    pub kind: FsKind,
    /// `kind` for people, e.g. "NTFS".
    pub name: String,
    pub label: Option<String>,
    /// Byte offset of the volume from the start of the disk.
    pub offset: u64,
    pub size: u64,
    pub cluster_size: u64,
}

/// A mounted-for-reading file system.
pub trait FileSystem: Send + Sync {
    fn info(&self) -> &VolumeInfo;

    /// The root folder.
    fn root(&self) -> EntryId;

    /// A folder's entries, deleted ones included (marked `deleted`).
    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError>;

    /// Reads from a file's main data (`stream: None`) or a named stream,
    /// starting at `offset`. Returns the bytes read; 0 at the end.
    fn read(
        &self,
        file: &EntryId,
        stream: Option<&str>,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError>;

    /// Where the file's data is on the disk, in order.
    fn extents(&self, file: &EntryId, stream: Option<&str>) -> Result<Vec<Extent>, FsError>;
}

/// Opens the file system starting at byte `offset` of `disk`.
pub fn open(disk: Arc<Disk>, offset: u64) -> Result<Box<dyn FileSystem>, FsError> {
    let found = datareco_partitions::identify(&disk, offset)?.ok_or_else(|| {
        FsError::Unsupported("No file system DataReco recognizes starts here.".to_owned())
    })?;
    let size = found.size.unwrap_or(disk.size().saturating_sub(offset));
    let volume = Volume::new(disk, offset, size);
    match found.kind {
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => Ok(Box::new(fat::Fat::open(volume)?)),
        FsKind::Exfat => Ok(Box::new(exfat::Exfat::open(volume)?)),
        FsKind::Ntfs => Ok(Box::new(ntfs::Ntfs::open(volume)?)),
        other => Err(FsError::Unsupported(format!(
            "Reading {} volumes isn't supported yet.",
            other.name()
        ))),
    }
}

/// What a scan found that helps open a damaged volume.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rebuild {
    /// A boot sector to use instead of the one at the volume's start, by its
    /// byte offset on the disk: a backup copy (NTFS keeps one at the end,
    /// FAT32 in sector 6, exFAT in sector 12).
    pub boot_sector: Option<u64>,
    /// NTFS: MFT records found on the disk, record number → byte offset on
    /// the disk. They stand in for records the MFT has no readable copy of.
    pub mft_records: BTreeMap<u64, u64>,
    /// FAT without a boot sector: where its clusters are.
    pub fat: Option<FatLayout>,
}

/// Opens a `kind` file system at byte `offset` of `disk`, `size` bytes long,
/// with help from what a scan found.
pub fn open_rebuilt(
    disk: Arc<Disk>,
    kind: FsKind,
    offset: u64,
    size: u64,
    rebuild: &Rebuild,
) -> Result<Box<dyn FileSystem>, FsError> {
    let boot = match rebuild.boot_sector {
        Some(at) => {
            let mut sector = vec![0; 512];
            disk.read_at(at, &mut sector)?;
            Some(sector)
        }
        None => None,
    };
    let volume = Volume::new(disk, offset, size);
    match kind {
        FsKind::Ntfs => {
            let end = offset.saturating_add(volume.size());
            let found = rebuild
                .mft_records
                .iter()
                .filter(|&(_, &at)| at >= offset && at < end)
                .map(|(&number, &at)| (number, at - offset))
                .collect();
            Ok(Box::new(ntfs::Ntfs::open_with(
                volume,
                ntfs::Options { boot, found },
            )?))
        }
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 => match (&rebuild.fat, boot) {
            (Some(layout), _) => Ok(Box::new(fat::Fat::rebuilt(volume, layout)?)),
            (None, Some(boot)) => Ok(Box::new(fat::Fat::open_with_boot(volume, &boot)?)),
            (None, None) => Ok(Box::new(fat::Fat::open(volume)?)),
        },
        FsKind::Exfat => Ok(Box::new(exfat::Exfat::open_with_boot(volume, boot)?)),
        other => Err(FsError::Unsupported(format!(
            "Reading {} volumes isn't supported yet.",
            other.name()
        ))),
    }
}

/// What one sector shows about where a file system's pieces are, for a
/// scan of a whole disk.
pub mod clues {
    pub use crate::fat::{FolderSector, folder_sector, table_start as fat_table_start};
    pub use crate::ntfs::record::{MftRecord, mft_record};
}

/// Reads a whole file or stream, up to `limit` bytes.
pub fn read_to_end(
    fs: &dyn FileSystem,
    file: &EntryId,
    stream: Option<&str>,
    limit: u64,
) -> Result<Vec<u8>, FsError> {
    let mut data = Vec::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let room = usize::try_from(limit.saturating_sub(data.len() as u64))
            .unwrap_or(usize::MAX)
            .min(buf.len());
        if room == 0 {
            return Ok(data);
        }
        let n = fs.read(file, stream, data.len() as u64, &mut buf[..room])?;
        if n == 0 {
            return Ok(data);
        }
        data.extend_from_slice(&buf[..n]);
    }
}

/// Finds an entry by path, like `docs/notes.txt` (`/` or `\`, any case).
/// Live entries win over deleted ones with the same name, unless
/// `prefer_deleted`.
pub fn resolve(fs: &dyn FileSystem, path: &str, prefer_deleted: bool) -> Result<Entry, FsError> {
    let mut current: Option<Entry> = None;
    let mut dir = fs.root();
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    for (i, part) in parts.iter().enumerate() {
        let mut matches: Vec<Entry> = fs
            .list(&dir)?
            .into_iter()
            .filter(|e| e.name.eq_ignore_ascii_case(part) || e.name == *part)
            .collect();
        matches.sort_by_key(|e| e.deleted != prefer_deleted);
        let entry = matches
            .into_iter()
            .next()
            .ok_or_else(|| FsError::NotFound(parts[..=i].join("/")))?;
        dir = entry.id.clone();
        current = Some(entry);
    }
    current.ok_or_else(|| FsError::NotFound("an empty path".to_owned()))
}
