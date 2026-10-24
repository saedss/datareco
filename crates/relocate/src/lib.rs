//! Moving a partition to another place on its disk: its data, block by
//! block; then its file system's note of where it starts; then the
//! partition table. A journal on another disk records how far it got, so
//! a move stopped at any moment (a power cut included) carries on from
//! there.
//!
//! Data is copied in the direction that never writes over data not yet
//! copied: from the front when moving towards the disk's start, from the
//! back when moving towards its end. When the new place overlaps the old
//! one by less than a batch, each batch's data is saved to the journal
//! before it's written, since writing it destroys part of its own source.
//!
//! Nothing here opens a disk: the caller opens the disk to read and to
//! write (after the person confirmed) and keeps them open while moving.

mod boot;
pub use boot::fix as fix_boot_sector;
#[cfg(test)]
mod tests;

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use datareco_io::{Disk, Error, Result, SectorSource, SectorTarget};
use datareco_partitions::edit::{Change, DiskFacts, SectorWrite, apply_known_writes, plan_move};
use datareco_partitions::read_table;
use serde::{Deserialize, Serialize};

/// Bytes moved at a time.
const BATCH: u64 = 16 << 20;
const JOURNAL_VERSION: u32 = 1;

/// Where a move is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MovePhase {
    Copying,
    FixingBoot,
    WritingTable,
    Done,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableSector {
    lba: u64,
    before: String,
    after: String,
}

/// What the journal records.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct State {
    version: u32,
    source: String,
    disk: DiskFacts,
    from_lba: u64,
    to_lba: u64,
    sectors: u64,
    /// Bytes copied: from the partition's start when moving towards the
    /// disk's start, from its end otherwise.
    done: u64,
    /// A batch saved in the journal's data file and being written: its
    /// offset in the partition and length, in bytes.
    pending: Option<(u64, u64)>,
    phase: MovePhase,
    /// What the move does, for people.
    description: String,
    /// The partition table's sectors, as they were and as the move leaves them.
    table: Vec<TableSector>,
    backup_dir: PathBuf,
}

/// How a move is going.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveProgress {
    pub phase: MovePhase,
    pub description: String,
    /// Bytes to copy, and copied.
    pub total: u64,
    pub done: u64,
    pub bytes_per_second: u64,
    pub finished: bool,
    pub journal: PathBuf,
    /// The partition table's backup, once the table is written.
    pub backup: Option<PathBuf>,
}

/// A move under way.
pub struct Mover {
    state: State,
    journal: PathBuf,
    buffer: Vec<u8>,
    elapsed: Duration,
    copied: u64,
    backup: Option<PathBuf>,
}

fn refused(reason: impl Into<String>) -> Error {
    Error::Refused {
        reason: reason.into(),
    }
}

fn journal_error(path: &Path, e: io::Error) -> Error {
    Error::Io {
        name: format!("the move's journal ({})", path.display()),
        source: e,
    }
}

impl Mover {
    /// Plans moving the partition starting at `from_lba` of `disk` (the
    /// source `source`, e.g. `disk:2`) so it starts at `to_lba`, and starts
    /// its journal in `journal_dir`, which must be on another disk. Refuses
    /// while another move of the same disk is unfinished.
    pub fn begin(
        disk: &Disk,
        source: &str,
        from_lba: u64,
        to_lba: u64,
        journal_dir: &Path,
        backup_dir: &Path,
    ) -> Result<Mover> {
        if let Some(unfinished) = unfinished(journal_dir, source) {
            return Err(refused(format!(
                "A move on {} hasn't finished ({}). Carry it on first.",
                disk.name(),
                unfinished.display()
            )));
        }
        let plan = plan_move(disk, from_lba, to_lba)?;
        if let Some(reason) = plan.blocked {
            return Err(refused(reason));
        }
        if let Some(error) = plan.error {
            return Err(refused(error.message));
        }
        let sectors = read_table(disk)?
            .partitions
            .iter()
            .find(|p| p.start_lba == from_lba)
            .map(|p| p.sectors)
            .ok_or_else(|| {
                refused(format!(
                    "There's no partition starting at sector {from_lba}."
                ))
            })?;
        let sector_size = u64::from(disk.sector_size());
        let mut table = Vec::new();
        for write in &plan.writes {
            let mut before = vec![0u8; write.data.len()];
            disk.read_at(write.lba * sector_size, &mut before)?;
            table.push(TableSector {
                lba: write.lba,
                before: to_hex(&before),
                after: to_hex(&write.data),
            });
        }
        let state = State {
            version: JOURNAL_VERSION,
            source: source.to_owned(),
            disk: DiskFacts {
                name: disk.name().to_owned(),
                size: disk.size(),
                sector_size: disk.sector_size(),
            },
            from_lba,
            to_lba,
            sectors,
            done: 0,
            pending: None,
            phase: MovePhase::Copying,
            description: plan.descriptions().join("; "),
            table,
            backup_dir: backup_dir.to_owned(),
        };
        fs::create_dir_all(journal_dir).map_err(|e| journal_error(journal_dir, e))?;
        let stem: String = disk
            .name()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let journal = journal_dir.join(format!("{stem}-{stamp}.drmove"));
        let mover = Mover {
            state,
            journal,
            buffer: Vec::new(),
            elapsed: Duration::ZERO,
            copied: 0,
            backup: None,
        };
        mover.save()?;
        Ok(mover)
    }

    /// Carries on a move from its journal. Refuses if `disk` isn't the
    /// disk it was moving on, or its partition table changed since.
    pub fn resume(journal: &Path, disk: &Disk) -> Result<Mover> {
        let text = fs::read_to_string(journal).map_err(|e| journal_error(journal, e))?;
        let state: State = serde_json::from_str(&text).map_err(|e| {
            refused(format!(
                "{} isn't a DataReco move journal: {e}",
                journal.display()
            ))
        })?;
        if state.version != JOURNAL_VERSION {
            return Err(refused(
                "The journal was made by another version of DataReco.",
            ));
        }
        if state.disk.size != disk.size() || state.disk.sector_size != disk.sector_size() {
            return Err(refused(format!(
                "This journal is for a move on another disk ({}).",
                state.disk.name
            )));
        }
        // Each table sector holds what it did, or (if writing the table was
        // under way when it stopped) what the move writes there.
        let sector_size = u64::from(disk.sector_size());
        let ss = disk.sector_size() as usize;
        for sector in &state.table {
            let (Some(before), Some(after)) = (from_hex(&sector.before), from_hex(&sector.after))
            else {
                return Err(refused("The journal is damaged."));
            };
            let mut now = vec![0u8; before.len()];
            disk.read_at(sector.lba * sector_size, &mut now)?;
            let writing = state.phase == MovePhase::WritingTable;
            let changed = now
                .chunks(ss)
                .zip(before.chunks(ss).zip(after.chunks(ss)))
                .position(|(now, (before, after))| now != before && !(writing && now == after));
            if let Some(index) = changed {
                return Err(refused(format!(
                    "{}'s partition table changed since the move started (sector {}). The move \
                     can't safely carry on; its journal is {}.",
                    state.disk.name,
                    sector.lba + index as u64,
                    journal.display()
                )));
            }
        }
        Ok(Mover {
            state,
            journal: journal.to_owned(),
            buffer: Vec::new(),
            elapsed: Duration::ZERO,
            copied: 0,
            backup: None,
        })
    }

    fn data_file(&self) -> PathBuf {
        self.journal.with_extension("drmove-data")
    }

    /// Writes the journal's state: to a new file, then over the old one,
    /// so it's always one or the other whole.
    fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.state)
            .map_err(|e| journal_error(&self.journal, io::Error::other(e)))?;
        let temporary = self.journal.with_extension("drmove-new");
        let write = || -> io::Result<()> {
            let mut file = File::create(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.journal)
        };
        write().map_err(|e| journal_error(&self.journal, e))
    }

    fn total(&self) -> u64 {
        self.state.sectors * u64::from(self.state.disk.sector_size)
    }

    pub fn progress(&self) -> MoveProgress {
        let seconds = self.elapsed.as_secs_f64();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )] // a rate for people
        let rate = if seconds > 0.0 {
            (self.copied as f64 / seconds) as u64
        } else {
            0
        };
        MoveProgress {
            phase: self.state.phase,
            description: self.state.description.clone(),
            total: self.total(),
            done: self.state.done,
            bytes_per_second: rate,
            finished: self.state.phase == MovePhase::Done,
            journal: self.journal.clone(),
            backup: self.backup.clone(),
        }
    }

    /// Moves for about `budget`, then reports. `source` reads the disk and
    /// `target` writes it (the same disk). On an error the journal is
    /// left as it is: fix what's wrong and resume.
    pub fn step(
        &mut self,
        source: &mut dyn SectorSource,
        target: &mut dyn SectorTarget,
        budget: Duration,
    ) -> Result<MoveProgress> {
        let began = Instant::now();
        let name = self.state.disk.name.clone();
        let io_error = |e: io::Error| Error::Io {
            name: name.clone(),
            source: e,
        };
        let sector = u64::from(self.state.disk.sector_size);
        let from = self.state.from_lba * sector;
        let to = self.state.to_lba * sector;
        let total = self.total();
        while self.state.phase != MovePhase::Done && began.elapsed() < budget {
            match self.state.phase {
                MovePhase::Copying => {
                    if let Some((offset, len)) = self.state.pending {
                        // Stopped while writing a batch saved in the
                        // journal: write it again from there.
                        let data = fs::read(self.data_file())
                            .map_err(|e| journal_error(&self.journal, e))?;
                        let len = usize::try_from(len).unwrap_or(usize::MAX);
                        let data = data
                            .get(..len)
                            .ok_or_else(|| refused("The journal's data is incomplete."))?;
                        target.write_at(to + offset, data).map_err(io_error)?;
                        target.flush().map_err(io_error)?;
                        self.state.done += len as u64;
                        self.state.pending = None;
                        self.save()?;
                        continue;
                    }
                    if self.state.done >= total {
                        self.state.phase = MovePhase::FixingBoot;
                        self.save()?;
                        continue;
                    }
                    let n = BATCH.min(total - self.state.done);
                    let offset = if to < from {
                        self.state.done
                    } else {
                        total - self.state.done - n
                    };
                    let len = usize::try_from(n).unwrap_or(usize::MAX);
                    self.buffer.resize(len, 0);
                    let started = Instant::now();
                    source
                        .read_sectors((from + offset) / sector, &mut self.buffer[..len])
                        .map_err(|e| Error::Io {
                            name: name.clone(),
                            source: io::Error::new(
                                e.kind(),
                                format!(
                                    "sectors {}–{} can't be read ({e}); nothing has been lost, and \
                                     the move can carry on once they can",
                                    (from + offset) / sector,
                                    (from + offset + n) / sector - 1
                                ),
                            ),
                        })?;
                    if to.abs_diff(from) < n {
                        // Writing this batch destroys part of its own
                        // source: keep a copy first.
                        let save_data = || -> io::Result<()> {
                            let mut file = File::create(self.data_file())?;
                            file.write_all(&self.buffer[..len])?;
                            file.sync_all()
                        };
                        save_data().map_err(|e| journal_error(&self.journal, e))?;
                        self.state.pending = Some((offset, n));
                        self.save()?;
                    }
                    target
                        .write_at(to + offset, &self.buffer[..len])
                        .map_err(io_error)?;
                    target.flush().map_err(io_error)?;
                    self.elapsed += started.elapsed();
                    self.copied += n;
                    self.state.done += n;
                    self.state.pending = None;
                    self.save()?;
                }
                MovePhase::FixingBoot => {
                    let ss = self.state.disk.sector_size;
                    let mut read = |lba: u64, buf: &mut [u8]| source.read_sectors(lba, buf);
                    let mut write =
                        |lba: u64, data: &[u8]| target.write_at(lba * u64::from(ss), data);
                    boot::fix(
                        &mut read,
                        &mut write,
                        self.state.to_lba,
                        self.state.sectors,
                        ss,
                    )
                    .map_err(io_error)?;
                    target.flush().map_err(io_error)?;
                    self.state.phase = MovePhase::WritingTable;
                    self.save()?;
                }
                MovePhase::WritingTable => {
                    let mut writes = Vec::new();
                    let mut befores = Vec::new();
                    for sector in &self.state.table {
                        let (Some(before), Some(after)) =
                            (from_hex(&sector.before), from_hex(&sector.after))
                        else {
                            return Err(refused("The journal is damaged."));
                        };
                        befores.push(before);
                        writes.push(SectorWrite {
                            lba: sector.lba,
                            data: after,
                        });
                    }
                    let changes = [self.state.description.clone()];
                    let change = Change {
                        source: &self.state.source,
                        changes: &changes,
                        writes: &writes,
                        undoes: None,
                        undoable: false,
                    };
                    let backup = apply_known_writes(
                        &self.state.disk,
                        &change,
                        &befores,
                        target,
                        &self.state.backup_dir,
                    )?;
                    self.backup = Some(backup);
                    self.state.phase = MovePhase::Done;
                    self.save()?;
                    // Done: the journal isn't needed any more.
                    let _ = fs::remove_file(self.data_file());
                    let _ = fs::remove_file(&self.journal);
                }
                MovePhase::Done => {}
            }
        }
        Ok(self.progress())
    }
}

/// Where the partition starting at `start_lba` would start on the nearest
/// 1 MiB boundary it has room to move to (towards the disk's start first),
/// or `None` if neither neighbouring boundary is free. Already aligned:
/// `Some(start_lba)`.
pub fn align_target(disk: &Disk, start_lba: u64) -> Result<Option<u64>> {
    let align = ((1 << 20) / u64::from(disk.sector_size())).max(1);
    if start_lba.is_multiple_of(align) {
        return Ok(Some(start_lba));
    }
    let down = start_lba / align * align;
    for to in [down, down + align] {
        let plan = plan_move(disk, start_lba, to)?;
        if plan.blocked.is_none() && plan.error.is_none() {
            return Ok(Some(to));
        }
    }
    Ok(None)
}

/// An unfinished move's journal in `dir` for `source`, if there is one.
pub fn unfinished(dir: &Path, source: &str) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "drmove"))
        .find(|path| {
            fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_json::from_str::<State>(&text).ok())
                .is_some_and(|state| state.source == source && state.phase != MovePhase::Done)
        })
}

/// Where move journals go unless told otherwise, beside table backups:
/// `%LOCALAPPDATA%\DataReco\moves` on Windows.
pub fn default_journal_dir() -> PathBuf {
    datareco_partitions::edit::default_backup_dir()
        .parent()
        .map_or_else(|| PathBuf::from("moves"), |p| p.join("moves"))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}
