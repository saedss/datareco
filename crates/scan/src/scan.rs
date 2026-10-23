//! Going over a disk, collecting clues to where volumes are.

use std::path::Path;
use std::sync::Arc;

use datareco_fs::FsKind;
use datareco_fs::clues;
use datareco_io::Disk;
use datareco_partitions::{ScanMode, ScanState};
use serde::{Deserialize, Serialize};

use crate::volumes::{self, FoundVolume};

/// Bytes a full scan reads at a time.
const READ_CHUNK: u64 = 8 << 20;
/// Clues kept at most of each kind (a disk full of disk images could have
/// hundreds of millions).
const MAX_RECORDS: usize = 20_000_000;
const MAX_FOLDERS: usize = 5_000_000;
const MAX_TABLES: usize = 10_000;
/// Clues are looked for at every 512 bytes, whatever the sector size.
const STEP: usize = 512;
/// The saved-scan format this version writes.
const FORMAT: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error(transparent)]
    Disk(#[from] datareco_io::Error),
    #[error(transparent)]
    Fs(#[from] datareco_fs::FsError),
    #[error("{0}")]
    Saved(String),
}

impl ScanError {
    /// A short, stable name for the kind of error, for the desktop app.
    pub fn kind(&self) -> &'static str {
        match self {
            ScanError::Disk(error) => error.kind(),
            ScanError::Fs(error) => error.kind(),
            ScanError::Saved(_) => "saved_scan",
        }
    }
}

/// An MFT record seen on the disk: byte offset, record number, record size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RecordClue(pub u64, pub u64, pub u32);

/// A sector of FAT folder entries: byte offset, and for a folder's first
/// sector its own first cluster and its parent's (0, 0 otherwise).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FolderClue(pub u64, pub u32, pub u32);

impl FolderClue {
    pub fn offset(self) -> u64 {
        self.0
    }

    /// The folder's own first cluster, if this is its first sector.
    pub fn own(self) -> Option<u32> {
        (self.1 != 0).then_some(self.1)
    }

    pub fn parent(self) -> u32 {
        self.2
    }
}

/// The start of a FAT: byte offset and which FAT it looks like.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TableClue(pub u64, pub FsKind);

/// How a scan is going.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    pub mode: ScanMode,
    /// Bytes looked at so far.
    pub position: u64,
    pub total: u64,
    pub done: bool,
    /// Boot sectors, partition tables and the like found so far.
    pub volume_clues: usize,
    /// MFT records found so far (full scans).
    pub mft_records: usize,
    /// Sectors of FAT folder entries found so far (full scans).
    pub folders: usize,
}

/// A scan in progress. Call [`Scan::step`] until it's done; [`Scan::results`]
/// can be read at any point.
pub struct Scan {
    mode: ScanMode,
    partitions: datareco_partitions::Scan,
    sector_size: u64,
    size: u64,
    /// Bytes a full scan has looked at.
    position: u64,
    done: bool,
    records: Vec<RecordClue>,
    folders: Vec<FolderClue>,
    tables: Vec<TableClue>,
}

/// A scan saved to carry on later, or to look at its results again without
/// scanning. It belongs to one disk: loading checks the size matches.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedScan {
    pub format: u32,
    /// The source, as it was opened (`disk:1`, `image:D:\disk.img`, …).
    pub source: String,
    pub disk_size: u64,
    pub sector_size: u32,
    pub mode: ScanMode,
    pub position: u64,
    pub done: bool,
    partitions: ScanState,
    records: Vec<RecordClue>,
    folders: Vec<FolderClue>,
    tables: Vec<TableClue>,
}

impl Scan {
    pub fn new(disk: &Disk, mode: ScanMode) -> Scan {
        Scan {
            mode,
            partitions: datareco_partitions::Scan::new(disk, mode),
            sector_size: u64::from(disk.sector_size()),
            size: disk.size(),
            position: 0,
            done: false,
            records: Vec::new(),
            folders: Vec::new(),
            tables: Vec::new(),
        }
    }

    pub fn mode(&self) -> ScanMode {
        self.mode
    }

    pub fn progress(&self) -> ScanProgress {
        let partitions = self.partitions.progress();
        ScanProgress {
            mode: self.mode,
            position: match self.mode {
                ScanMode::Quick => partitions.position,
                ScanMode::Full => self.position,
            },
            total: self.size,
            done: self.done,
            volume_clues: partitions.clues,
            mft_records: self.records.len(),
            folders: self.folders.len(),
        }
    }

    /// Looks at about `limit` more bytes of the disk.
    pub fn step(&mut self, disk: &Disk, limit: u64) -> Result<ScanProgress, ScanError> {
        if self.done {
            return Ok(self.progress());
        }
        match self.mode {
            ScanMode::Quick => {
                self.done = self.partitions.step(disk, limit)?.done;
            }
            ScanMode::Full => {
                let end = self.position.saturating_add(limit.max(1)).min(self.size);
                while self.position < end {
                    let len = READ_CHUNK.min(end - self.position);
                    // Whole sectors only.
                    let len = (len / self.sector_size).max(1) * self.sector_size;
                    let len = len.min(self.size - self.position);
                    let mut data = vec![0; usize::try_from(len).unwrap_or(0)];
                    disk.read_at(self.position, &mut data)?;
                    self.look_at(self.position, &data);
                    self.partitions
                        .feed(self.position / self.sector_size, &data);
                    self.position += len;
                }
                if self.position >= self.size {
                    self.partitions.finish(disk)?;
                    self.done = true;
                }
            }
        }
        Ok(self.progress())
    }

    /// Runs a whole scan and returns what it found.
    pub fn run(disk: &Arc<Disk>, mode: ScanMode) -> Result<Vec<FoundVolume>, ScanError> {
        let mut scan = Scan::new(disk, mode);
        while !scan.step(disk, 64 << 20)?.done {}
        scan.results(disk)
    }

    /// Notes the clues in `data`, read from byte `offset` of the disk.
    fn look_at(&mut self, offset: u64, data: &[u8]) {
        for (i, sector) in data.as_chunks::<STEP>().0.iter().enumerate() {
            let at = offset + (i * STEP) as u64;
            if let Some(record) = clues::mft_record(sector.as_slice()) {
                if self.records.len() < MAX_RECORDS {
                    let size = u32::try_from(record.size).unwrap_or(0);
                    self.records.push(RecordClue(at, record.number, size));
                }
                continue;
            }
            if let Some(folder) = clues::folder_sector(sector.as_slice()) {
                if self.folders.len() < MAX_FOLDERS {
                    let (own, parent) = folder.dot.unwrap_or((0, 0));
                    self.folders.push(FolderClue(at, own, parent));
                }
                continue;
            }
            if let Some(kind) = clues::fat_table_start(sector.as_slice())
                && self.tables.len() < MAX_TABLES
            {
                self.tables.push(TableClue(at, kind));
            }
        }
    }

    /// The volumes found so far, best first.
    pub fn results(&self, disk: &Arc<Disk>) -> Result<Vec<FoundVolume>, ScanError> {
        let table = datareco_partitions::read_table(disk).ok();
        let partitions = self.partitions.results(disk, table.as_ref())?;
        volumes::build(
            disk,
            &volumes::Clues {
                partitions: &partitions,
                table: table.as_ref(),
                records: &self.records,
                folders: &self.folders,
                tables: &self.tables,
            },
        )
    }

    pub fn save(&self, source: &str) -> SavedScan {
        SavedScan {
            format: FORMAT,
            source: source.to_owned(),
            disk_size: self.size,
            sector_size: u32::try_from(self.sector_size).unwrap_or(512),
            mode: self.mode,
            position: self.position,
            done: self.done,
            partitions: self.partitions.state(),
            records: self.records.clone(),
            folders: self.folders.clone(),
            tables: self.tables.clone(),
        }
    }

    /// Carries on with a saved scan of `disk`.
    pub fn restore(disk: &Disk, saved: &SavedScan) -> Result<Scan, ScanError> {
        if saved.format != FORMAT {
            return Err(ScanError::Saved(format!(
                "This scan was saved by another version of DataReco (format {}).",
                saved.format
            )));
        }
        if saved.disk_size != disk.size()
            || u64::from(saved.sector_size) != u64::from(disk.sector_size())
        {
            return Err(ScanError::Saved(format!(
                "This scan is of a different disk ({} bytes in {}-byte sectors; this one has {} bytes in {}-byte sectors).",
                saved.disk_size,
                saved.sector_size,
                disk.size(),
                disk.sector_size()
            )));
        }
        Ok(Scan {
            mode: saved.mode,
            partitions: datareco_partitions::Scan::restore(disk, &saved.partitions)?,
            sector_size: u64::from(disk.sector_size()),
            size: disk.size(),
            position: saved.position.min(disk.size()),
            done: saved.done,
            records: saved.records.clone(),
            folders: saved.folders.clone(),
            tables: saved.tables.clone(),
        })
    }
}

impl SavedScan {
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    pub fn read_from(path: &Path) -> Result<SavedScan, ScanError> {
        let data = std::fs::read(path).map_err(|e| {
            ScanError::Saved(format!(
                "Couldn't read the saved scan {}: {e}",
                path.display()
            ))
        })?;
        serde_json::from_slice(&data)
            .map_err(|e| ScanError::Saved(format!("{} isn't a DataReco scan: {e}", path.display())))
    }
}
