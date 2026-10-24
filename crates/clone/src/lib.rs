//! Copying partitions onto other disks, used space only: one partition into
//! free space on another disk, or every partition of a disk onto another
//! disk, which may be smaller as long as the partitions fit. Moving Windows
//! to another disk is copying the disk it runs from: its volumes are read
//! from snapshots, and the copy keeps the disk's IDs so Windows starts from
//! it.
//!
//! The source is only ever read. On the target the data goes first, then
//! each copied file system's note of where it starts, then the partition
//! table, so the copies show up only once they're whole. A copy that stops
//! is started again: the source hasn't changed.
//!
//! The tables are worked out by `datareco_partitions::edit`
//! ([`plan_partition_copy`](datareco_partitions::edit::plan_partition_copy),
//! [`plan_disk_copy`](datareco_partitions::edit::plan_disk_copy)); a
//! [`Job`] adds what each partition holds, and a [`Cloner`] carries it out
//! a step at a time.

mod sources;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use datareco_fs::Allocation;
use datareco_imaging::{Area, Copier, CopyError, CopyOptions, Map};
use datareco_io::{Disk, SectorSource, SectorTarget};
use datareco_partitions::edit::{
    Change, DiskCopy, DiskFacts, PartitionCopy, PlacedPart, SectorWrite, apply_known_writes,
};
use serde::Serialize;

pub use sources::{Prepared, Reading, plan_reading, prepare, runs_windows};

/// Free stretches shorter than this between used ones are copied too: one
/// read is quicker than two with a seek between.
const MERGE_GAP: u64 = 1 << 20;

/// Why a copy can't be made, or stopped.
#[derive(Debug, thiserror::Error)]
pub enum CloneError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Io(#[from] datareco_io::Error),
    #[error(transparent)]
    Copy(#[from] CopyError),
    #[error("Reading the source failed: {0}")]
    Read(#[source] io::Error),
    #[error("Writing to the target failed: {0}")]
    Write(#[source] io::Error),
    #[error("Updating a copy's boot sector failed: {0}")]
    Boot(#[source] io::Error),
}

fn refused(message: impl Into<String>) -> CloneError {
    CloneError::Refused(message.into())
}

/// Which parts of the file system starting `offset` bytes into `disk` are
/// in use, when it keeps track (NTFS, FAT, exFAT).
pub fn allocation(disk: Arc<Disk>, offset: u64) -> Option<Allocation> {
    datareco_fs::open(disk, offset).ok()?.allocation()
}

/// The byte ranges of a partition of `size` bytes to copy: what
/// `allocation` says is in use (all of it without one) and everything past
/// what it speaks for, in whole sectors, with short free stretches between
/// used ones copied too.
pub fn ranges_to_copy(
    allocation: Option<&Allocation>,
    size: u64,
    sector_size: u32,
) -> Vec<(u64, u64)> {
    let sector = u64::from(sector_size.max(1));
    let Some(allocation) = allocation else {
        return vec![(0, size)];
    };
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let tail = (allocation.covered.min(size), size);
    for &(start, end) in allocation.used.iter().chain([&tail]) {
        let start = start / sector * sector;
        let end = end.min(size).next_multiple_of(sector).min(size);
        if start >= end {
            continue;
        }
        match ranges.last_mut() {
            Some(last) if start <= last.1 + MERGE_GAP => last.1 = last.1.max(end),
            _ => ranges.push((start, end)),
        }
    }
    ranges
}

/// Bytes a copy of `parts` would read, from what their file systems say
/// now (an estimate: they may change before the copy starts).
pub fn estimate(disk: &Arc<Disk>, parts: &[PlacedPart]) -> u64 {
    let sector_size = disk.sector_size();
    parts
        .iter()
        .map(|p| {
            let used = allocation(Arc::clone(disk), p.from_lba * u64::from(sector_size));
            ranges_to_copy(
                used.as_ref(),
                p.sectors * u64::from(sector_size),
                sector_size,
            )
            .iter()
            .map(|(start, end)| end - start)
            .sum::<u64>()
        })
        .sum()
}

/// One partition to copy: where it goes, and which parts of it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Piece {
    pub part: PlacedPart,
    /// Byte ranges from the partition's start; the rest isn't copied.
    pub ranges: Vec<(u64, u64)>,
    /// Bytes in `ranges`.
    pub bytes: u64,
}

impl Piece {
    fn new(part: PlacedPart, allocation: Option<&Allocation>, sector_size: u32) -> Piece {
        let size = part.sectors * u64::from(sector_size);
        let ranges = ranges_to_copy(allocation, size, sector_size);
        let bytes = ranges.iter().map(|(start, end)| end - start).sum();
        Piece {
            part,
            ranges,
            bytes,
        }
    }
}

/// The target's partition table, written last.
#[derive(Clone, Debug)]
pub enum TableWrite {
    /// A whole new table, for a disk being copied onto.
    Whole(Vec<SectorWrite>),
    /// One more entry in the target's table: backed up first, so it can be
    /// undone like any table change.
    Change {
        writes: Vec<SectorWrite>,
        /// What those sectors hold now.
        befores: Vec<Vec<u8>>,
        facts: DiskFacts,
        /// The target's source id (`disk:2`), to find the backup by.
        target: String,
        changes: Vec<String>,
        backup_dir: PathBuf,
    },
}

/// Everything a copy does.
#[derive(Clone, Debug)]
pub struct Job {
    /// What it does, for people.
    pub description: String,
    pub pieces: Vec<Piece>,
    /// MBR disks: the sectors after sector 0 copied as they are.
    pub boot_sectors: u64,
    pub table: TableWrite,
    pub sector_size: u32,
}

impl Job {
    /// Copying one partition into free space on `target`, as `copy` planned
    /// it. `allocation` says what the partition holds.
    pub fn partition(
        copy: &PartitionCopy,
        target: &Disk,
        target_id: &str,
        backup_dir: &Path,
        allocation: Option<&Allocation>,
    ) -> Result<Job, CloneError> {
        if let Some(reason) = &copy.plan.blocked {
            return Err(refused(reason.clone()));
        }
        if let Some(error) = &copy.plan.error {
            return Err(refused(error.message.clone()));
        }
        let part = copy
            .part
            .clone()
            .ok_or_else(|| refused("There's nothing to copy."))?;
        let sector_size = target.sector_size();
        let mut befores = Vec::new();
        for write in &copy.plan.writes {
            let mut before = vec![0u8; write.data.len()];
            target.read_at(write.lba * u64::from(sector_size), &mut before)?;
            befores.push(before);
        }
        let changes = copy.plan.descriptions();
        Ok(Job {
            description: changes.first().cloned().unwrap_or_default(),
            pieces: vec![Piece::new(part, allocation, sector_size)],
            boot_sectors: 0,
            table: TableWrite::Change {
                writes: copy.plan.writes.clone(),
                befores,
                facts: DiskFacts {
                    name: target.name().to_owned(),
                    size: target.size(),
                    sector_size,
                },
                target: target_id.to_owned(),
                changes,
                backup_dir: backup_dir.to_owned(),
            },
            sector_size,
        })
    }

    /// Copying every partition of a disk, as `copy` planned it.
    /// `allocations` says what each of `copy.parts` holds.
    pub fn disk(
        copy: &DiskCopy,
        allocations: &[Option<Allocation>],
        sector_size: u32,
        description: String,
    ) -> Result<Job, CloneError> {
        if let Some(reason) = copy.blocked.as_ref().or(copy.problem.as_ref()) {
            return Err(refused(reason.clone()));
        }
        let pieces = copy
            .parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                let allocation = allocations.get(i).and_then(Option::as_ref);
                Piece::new(part.clone(), allocation, sector_size)
            })
            .collect();
        Ok(Job {
            description,
            pieces,
            boot_sectors: copy.boot_sectors,
            table: TableWrite::Whole(copy.writes.clone()),
            sector_size,
        })
    }

    /// Bytes the copy reads and writes.
    pub fn total(&self) -> u64 {
        self.pieces.iter().map(|p| p.bytes).sum::<u64>()
            + self.boot_sectors * u64::from(self.sector_size)
    }
}

/// Where the source is read from: the disk, and for some partitions a
/// snapshot of their volume instead.
pub struct CloneSources {
    pub disk: Box<dyn SectorSource>,
    /// By piece: a snapshot to read that partition from, from its start.
    pub snapshots: HashMap<usize, Box<dyn SectorSource>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClonePhase {
    Copying,
    FixingBoot,
    WritingTable,
    Done,
}

/// How a copy is going.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneProgress {
    pub phase: ClonePhase,
    /// What it's doing now, for people.
    pub description: String,
    /// Bytes to copy, and copied.
    pub total: u64,
    pub done: u64,
    pub bytes_per_second: u64,
    /// Bytes that couldn't be read: zeros on the copy.
    pub unreadable: u64,
    pub finished: bool,
    /// The target's table backup (a partition copied into free space).
    pub backup: Option<PathBuf>,
}

/// A copy under way. Call [`Cloner::step`] until it's finished.
pub struct Cloner {
    job: Job,
    phase: ClonePhase,
    boot_done: bool,
    /// The piece being copied, its copy, and the bytes its map counted as
    /// copied without copying them (free space).
    index: usize,
    copier: Option<Copier>,
    preset: u64,
    /// Bytes copied in the pieces before it.
    done: u64,
    unreadable: u64,
    elapsed: Duration,
    backup: Option<PathBuf>,
}

impl Cloner {
    pub fn new(job: Job) -> Cloner {
        Cloner {
            job,
            phase: ClonePhase::Copying,
            boot_done: false,
            index: 0,
            copier: None,
            preset: 0,
            done: 0,
            unreadable: 0,
            elapsed: Duration::ZERO,
            backup: None,
        }
    }

    pub fn job(&self) -> &Job {
        &self.job
    }

    pub fn progress(&self) -> CloneProgress {
        let current = self
            .copier
            .as_ref()
            .map_or(0, |c| c.map().totals().copied.saturating_sub(self.preset));
        let done = (self.done + current).min(self.job.total());
        let description = match self.phase {
            ClonePhase::Copying => match self.job.pieces.get(self.index) {
                Some(piece) => format!(
                    "Copying {} ({} of {})",
                    piece.part.what,
                    self.index + 1,
                    self.job.pieces.len()
                ),
                None => "Copying".to_owned(),
            },
            ClonePhase::FixingBoot => "Updating the copies' boot sectors".to_owned(),
            ClonePhase::WritingTable => "Writing the partition table".to_owned(),
            ClonePhase::Done => "Finished".to_owned(),
        };
        let seconds = self.elapsed.as_secs_f64();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let bytes_per_second = if seconds > 0.5 {
            (done as f64 / seconds) as u64
        } else {
            0
        };
        CloneProgress {
            phase: self.phase,
            description,
            total: self.job.total(),
            done,
            bytes_per_second,
            unreadable: self.unreadable,
            finished: self.phase == ClonePhase::Done,
            backup: self.backup.clone(),
        }
    }

    /// Copies for about `budget`, then says how it's going.
    pub fn step(
        &mut self,
        sources: &mut CloneSources,
        target: &mut dyn SectorTarget,
        budget: Duration,
    ) -> Result<CloneProgress, CloneError> {
        let started = Instant::now();
        let deadline = started + budget;
        let result = self.work(sources, target, deadline);
        self.elapsed += started.elapsed();
        result.map(|()| self.progress())
    }

    fn work(
        &mut self,
        sources: &mut CloneSources,
        target: &mut dyn SectorTarget,
        deadline: Instant,
    ) -> Result<(), CloneError> {
        let sector = u64::from(self.job.sector_size);
        if u64::from(target.sector_size()) != sector {
            return Err(refused(
                "The disk being copied onto has a different sector size.",
            ));
        }
        loop {
            match self.phase {
                ClonePhase::Copying => {
                    if !self.boot_done {
                        self.copy_boot_sectors(sources, target)?;
                    }
                    let Some(piece) = self.job.pieces.get(self.index) else {
                        target.flush().map_err(CloneError::Write)?;
                        self.phase = ClonePhase::FixingBoot;
                        continue;
                    };
                    let copier = match &mut self.copier {
                        Some(copier) => copier,
                        None => {
                            let (copier, preset) = start_piece(piece, self.job.sector_size)?;
                            self.preset = preset;
                            self.copier.insert(copier)
                        }
                    };
                    let snapshot = sources.snapshots.get_mut(&self.index);
                    let mut source = PartSource::new(
                        sources.disk.as_mut(),
                        snapshot.map(|s| s.as_mut() as &mut dyn SectorSource),
                        &piece.part,
                        sector,
                    );
                    let mut out = OffsetTarget {
                        inner: target,
                        offset: piece.part.to_lba * sector,
                        size: piece.part.sectors * sector,
                    };
                    let left = deadline
                        .saturating_duration_since(Instant::now())
                        .max(Duration::from_millis(1));
                    let progress = copier.step(&mut source, &mut out, left)?;
                    if progress.finished {
                        self.done += progress.totals.copied.saturating_sub(self.preset);
                        self.unreadable += progress.totals.bad;
                        self.copier = None;
                        self.preset = 0;
                        self.index += 1;
                    }
                }
                ClonePhase::FixingBoot => {
                    for (index, piece) in self.job.pieces.iter().enumerate() {
                        let snapshot = sources.snapshots.get_mut(&index);
                        let mut source = PartSource::new(
                            sources.disk.as_mut(),
                            snapshot.map(|s| s.as_mut() as &mut dyn SectorSource),
                            &piece.part,
                            sector,
                        );
                        let to = piece.part.to_lba;
                        let mut read = |lba: u64, buf: &mut [u8]| {
                            let at = lba.checked_sub(to).ok_or_else(|| {
                                io::Error::other("a read before the partition's start")
                            })?;
                            source.read_sectors(at, buf)
                        };
                        let mut write = |lba: u64, data: &[u8]| target.write_at(lba * sector, data);
                        datareco_relocate::fix_boot_sector(
                            &mut read,
                            &mut write,
                            to,
                            piece.part.sectors,
                            self.job.sector_size,
                        )
                        .map_err(CloneError::Boot)?;
                    }
                    target.flush().map_err(CloneError::Write)?;
                    self.phase = ClonePhase::WritingTable;
                }
                ClonePhase::WritingTable => {
                    self.write_table(target)?;
                    self.phase = ClonePhase::Done;
                }
                ClonePhase::Done => return Ok(()),
            }
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }

    fn copy_boot_sectors(
        &mut self,
        sources: &mut CloneSources,
        target: &mut dyn SectorTarget,
    ) -> Result<(), CloneError> {
        let sector = u64::from(self.job.sector_size);
        if let TableWrite::Whole(_) = &self.job.table {
            // The disk's old table goes first: a copy stopped partway then
            // leaves an empty disk, not partitions over half-copied data.
            // Sector 0 and GPT's primary copy; GPT's backup copy at the end.
            let zeros = vec![0u8; usize::try_from(34 * sector).unwrap_or(0)];
            target.write_at(0, &zeros).map_err(CloneError::Write)?;
            if let Some(end) = target.capacity().map(|c| c / sector)
                && end > 68
            {
                let backup = &zeros[..usize::try_from(33 * sector).unwrap_or(0)];
                target
                    .write_at((end - 33) * sector, backup)
                    .map_err(CloneError::Write)?;
            }
            target.flush().map_err(CloneError::Write)?;
        }
        if self.job.boot_sectors > 0 {
            let length = usize::try_from(self.job.boot_sectors * sector)
                .map_err(|_| refused("Too many boot sectors."))?;
            let mut data = vec![0u8; length];
            sources
                .disk
                .read_sectors(1, &mut data)
                .map_err(CloneError::Read)?;
            target.write_at(sector, &data).map_err(CloneError::Write)?;
        }
        self.boot_done = true;
        Ok(())
    }

    fn write_table(&mut self, target: &mut dyn SectorTarget) -> Result<(), CloneError> {
        let sector = u64::from(self.job.sector_size);
        match &self.job.table {
            TableWrite::Whole(writes) => {
                for write in writes {
                    target
                        .write_at(write.lba * sector, &write.data)
                        .map_err(CloneError::Write)?;
                }
                target.flush().map_err(CloneError::Write)?;
            }
            TableWrite::Change {
                writes,
                befores,
                facts,
                target: id,
                changes,
                backup_dir,
            } => {
                let change = Change {
                    source: id,
                    changes,
                    writes,
                    undoes: None,
                    undoable: true,
                };
                self.backup = Some(apply_known_writes(
                    facts, &change, befores, target, backup_dir,
                )?);
            }
        }
        Ok(())
    }
}

/// A copy of one piece: its map counts what isn't to be copied as done
/// already. Returns the copy and how many bytes that is.
fn start_piece(piece: &Piece, sector_size: u32) -> Result<(Copier, u64), CloneError> {
    let size = piece.part.sectors * u64::from(sector_size);
    let mut map = Map::new(size);
    let mut preset = 0;
    let mut at = 0;
    for &(start, end) in piece.ranges.iter().chain([&(size, size)]) {
        if start > at {
            map.set(at, start, Area::Copied);
            preset += start - at;
        }
        at = at.max(end);
    }
    let options = CopyOptions {
        skip: true,
        retries: 1,
        // Unreadable sectors become zeros, not what the target held.
        fill_bad: true,
        ..CopyOptions::default()
    };
    Ok((Copier::new(map, sector_size, options)?, preset))
}

/// One partition of the source, read from its start: from a snapshot of
/// its volume when there is one (and from the disk past the snapshot's
/// end), otherwise from the disk.
struct PartSource<'a> {
    disk: &'a mut dyn SectorSource,
    snapshot: Option<&'a mut dyn SectorSource>,
    start: u64,
    sectors: u64,
    sector: u64,
}

impl<'a> PartSource<'a> {
    fn new(
        disk: &'a mut dyn SectorSource,
        snapshot: Option<&'a mut dyn SectorSource>,
        part: &PlacedPart,
        sector: u64,
    ) -> PartSource<'a> {
        PartSource {
            disk,
            snapshot,
            start: part.from_lba,
            sectors: part.sectors,
            sector,
        }
    }
}

impl SectorSource for PartSource<'_> {
    fn sector_size(&self) -> u32 {
        u32::try_from(self.sector).unwrap_or(512)
    }

    fn size(&self) -> u64 {
        self.sectors * self.sector
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        let count = buf.len() as u64 / self.sector;
        if lba + count > self.sectors {
            return Err(io::Error::other("a read past the partition's end"));
        }
        let from_snapshot = match &self.snapshot {
            Some(snapshot) => (snapshot.size() / self.sector)
                .saturating_sub(lba)
                .min(count),
            None => 0,
        };
        let split = usize::try_from(from_snapshot * self.sector).unwrap_or(buf.len());
        let (head, tail) = buf.split_at_mut(split);
        if let Some(snapshot) = &mut self.snapshot
            && !head.is_empty()
        {
            snapshot.read_sectors(lba, head)?;
        }
        if !tail.is_empty() {
            self.disk
                .read_sectors(self.start + lba + from_snapshot, tail)?;
        }
        Ok(())
    }
}

/// A partition's place on the target: offsets from its start.
struct OffsetTarget<'a> {
    inner: &'a mut dyn SectorTarget,
    offset: u64,
    size: u64,
}

impl SectorTarget for OffsetTarget<'_> {
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn capacity(&self) -> Option<u64> {
        Some(self.size)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        if offset + data.len() as u64 > self.size {
            return Err(io::Error::other("a write past the partition's end"));
        }
        self.inner.write_at(self.offset + offset, data)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
