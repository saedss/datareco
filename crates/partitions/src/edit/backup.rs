//! Writing a plan's sectors, after saving the ones they replace to a backup
//! file, and undoing it from that file.
//!
//! A backup holds each sector written, as it was before and as it was
//! written. Undo writes the "before" copies back, but only while the disk
//! still holds the "after" ones: if something changed the table since,
//! putting old sectors back could do damage, so it refuses.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use datareco_io::{Disk, Error, Result, SectorTarget};
use serde::{Deserialize, Serialize};

/// Sectors to write at `lba`: one or more whole sectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectorWrite {
    pub lba: u64,
    pub data: Vec<u8>,
}

const BACKUP_VERSION: u32 = 1;

/// A backup of the sectors a change wrote, to undo it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableBackup {
    pub version: u32,
    /// The source id it was made for (`disk:1`, `image:…`).
    pub source: String,
    /// The source's name for people ("Disk 1").
    pub disk: String,
    pub size: u64,
    pub sector_size: u32,
    /// Milliseconds since 1970 (UTC).
    pub created: u64,
    /// What was changed, for people.
    pub changes: Vec<String>,
    /// For an undo: the file name of the backup it undid.
    #[serde(default)]
    pub undoes: Option<String>,
    /// False for a change that moved data too (a partition move): putting
    /// back only its table would point at data that has moved.
    #[serde(default = "yes")]
    pub undoable: bool,
    pub sectors: Vec<SavedSectors>,
}

fn yes() -> bool {
    true
}

/// Sectors as they were and as they were written, in hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSectors {
    pub lba: u64,
    pub before: String,
    pub after: String,
}

/// A backup file, for listing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInfo {
    pub path: PathBuf,
    pub source: String,
    pub disk: String,
    /// Milliseconds since 1970 (UTC).
    pub created: u64,
    pub changes: Vec<String>,
    /// For an undo: the file name of the backup it undid.
    pub undoes: Option<String>,
    /// Whether a later undo put this change back.
    pub undone: bool,
    /// Whether undo can put it back (not for partition moves).
    pub undoable: bool,
}

/// Where backups go unless told otherwise: `%LOCALAPPDATA%\DataReco\
/// table-backups` on Windows, `~/.local/share/datareco/table-backups`
/// elsewhere. The command-line tool and the desktop app share it.
pub fn default_backup_dir() -> PathBuf {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join("DataReco").join("table-backups");
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    home.join(".local/share/datareco/table-backups")
}

/// A change to write.
pub struct Change<'a> {
    /// The source id (`disk:1`, `image:…`), to find its backups by.
    pub source: &'a str,
    /// What it does, for people.
    pub changes: &'a [String],
    pub writes: &'a [SectorWrite],
    /// For an undo: the backup file it undoes.
    pub undoes: Option<&'a Path>,
    /// False when data moved too, so undoing the table alone would do harm.
    pub undoable: bool,
}

/// Saves the sectors a change replaces to a new backup file in
/// `backup_dir`, then writes them to `target` in order and flushes it.
/// Returns the backup file. `disk` is the same disk as `target`, opened for
/// reading, fresh (so no old cached sectors are backed up).
pub fn apply_writes(
    disk: &Disk,
    change: &Change<'_>,
    target: &mut dyn SectorTarget,
    backup_dir: &Path,
) -> Result<PathBuf> {
    let sector_size = u64::from(disk.sector_size());
    let mut befores = Vec::new();
    for write in change.writes {
        let mut before = vec![0u8; write.data.len()];
        disk.read_at(write.lba * sector_size, &mut before)?;
        befores.push(before);
    }
    let facts = DiskFacts {
        name: disk.name().to_owned(),
        size: disk.size(),
        sector_size: disk.sector_size(),
    };
    apply_known_writes(&facts, change, &befores, target, backup_dir)
}

/// What a backup records about its disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskFacts {
    pub name: String,
    pub size: u64,
    pub sector_size: u32,
}

/// Like [`apply_writes`], with the sectors' old contents already known
/// (`befores`, one per write): read when a long operation started and
/// checked since.
pub fn apply_known_writes(
    facts: &DiskFacts,
    change: &Change<'_>,
    befores: &[Vec<u8>],
    target: &mut dyn SectorTarget,
    backup_dir: &Path,
) -> Result<PathBuf> {
    let writes = change.writes;
    if target.sector_size() != facts.sector_size {
        return Err(Error::Refused {
            reason: "The disk opened for writing has a different sector size.".into(),
        });
    }
    let sector_size = u64::from(facts.sector_size);
    let saved = writes
        .iter()
        .zip(befores)
        .map(|(write, before)| SavedSectors {
            lba: write.lba,
            before: to_hex(before),
            after: to_hex(&write.data),
        })
        .collect();
    let backup = TableBackup {
        version: BACKUP_VERSION,
        source: change.source.to_owned(),
        disk: facts.name.clone(),
        size: facts.size,
        sector_size: facts.sector_size,
        created: now(),
        changes: change.changes.to_vec(),
        undoes: change
            .undoes
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned()),
        undoable: change.undoable,
        sectors: saved,
    };
    let path = save(&backup, backup_dir).map_err(|e| Error::Io {
        name: format!("the backup in {}", backup_dir.display()),
        source: e,
    })?;
    let name = facts.name.clone();
    for write in writes {
        target
            .write_at(write.lba * sector_size, &write.data)
            .map_err(|e| Error::Io {
                name: name.clone(),
                source: e,
            })?;
    }
    target.flush().map_err(|e| Error::Io { name, source: e })?;
    Ok(path)
}

/// The sectors of `writes` that don't read back as written (read from a
/// freshly opened disk).
pub fn verify_writes(disk: &Disk, writes: &[SectorWrite]) -> Result<Vec<u64>> {
    let sector_size = u64::from(disk.sector_size());
    let mut wrong = Vec::new();
    for write in writes {
        let mut now = vec![0u8; write.data.len()];
        disk.read_at(write.lba * sector_size, &mut now)?;
        if now != write.data {
            wrong.push(write.lba);
        }
    }
    Ok(wrong)
}

/// The writes that undo a backup: its "before" sectors. Refuses (with the
/// reason for people) if the disk isn't the one backed up, or no longer
/// holds what was written.
pub fn undo_writes(
    disk: &Disk,
    backup: &TableBackup,
) -> Result<std::result::Result<Vec<SectorWrite>, String>> {
    if !backup.undoable {
        return Ok(Err(
            "This change moved a partition's data as well as its table entry, so putting back \
             only the table would point at data that has moved. Move the partition back instead."
                .into(),
        ));
    }
    if backup.size != disk.size() || backup.sector_size != disk.sector_size() {
        return Ok(Err(format!(
            "This backup is of a {}-byte disk with {}-byte sectors; {} is different.",
            backup.size,
            backup.sector_size,
            disk.name()
        )));
    }
    let sector_size = u64::from(disk.sector_size());
    let sector_bytes = disk.sector_size() as usize;
    let mut writes = Vec::new();
    for saved in &backup.sectors {
        let (Some(before), Some(after)) = (from_hex(&saved.before), from_hex(&saved.after)) else {
            return Ok(Err("The backup file is damaged.".into()));
        };
        if before.len() != after.len() || !before.len().is_multiple_of(sector_bytes) {
            return Ok(Err("The backup file is damaged.".into()));
        }
        let mut now = vec![0u8; after.len()];
        disk.read_at(saved.lba * sector_size, &mut now)?;
        if now != after {
            return Ok(Err(format!(
                "Sector {} has changed since this backup was made, so putting the old table back \
                 could undo something newer. Undo the later changes first.",
                saved.lba
            )));
        }
        writes.push(SectorWrite {
            lba: saved.lba,
            data: before,
        });
    }
    // Undo in the opposite order to the change, so the table is readable
    // at each step.
    writes.reverse();
    Ok(Ok(writes))
}

pub fn read_backup(path: &Path) -> std::result::Result<TableBackup, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("Can't read {}: {e}", path.display()))?;
    let backup: TableBackup = serde_json::from_str(&text)
        .map_err(|e| format!("{} isn't a DataReco table backup: {e}", path.display()))?;
    if backup.version != BACKUP_VERSION {
        return Err(format!(
            "{} was made by another version of DataReco.",
            path.display()
        ));
    }
    Ok(backup)
}

/// Backups in `dir` made for `source`, newest first.
pub fn list_backups(dir: &Path, source: &str) -> Vec<BackupInfo> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<BackupInfo> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "drtable"))
        .filter_map(|path| {
            let backup = read_backup(&path).ok()?;
            (backup.source == source).then_some(BackupInfo {
                path,
                source: backup.source,
                disk: backup.disk,
                created: backup.created,
                changes: backup.changes,
                undoes: backup.undoes,
                undone: false,
                undoable: backup.undoable,
            })
        })
        .collect();
    let undone: Vec<String> = found.iter().filter_map(|b| b.undoes.clone()).collect();
    for backup in &mut found {
        let name = backup
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        backup.undone = name.is_some_and(|n| undone.contains(&n));
    }
    found.sort_by_key(|b| std::cmp::Reverse(b.created));
    found
}

/// The change `undo` puts back by default: the newest one that isn't an
/// undo itself and hasn't been undone.
pub fn next_to_undo(dir: &Path, source: &str) -> Option<BackupInfo> {
    list_backups(dir, source)
        .into_iter()
        .find(|b| b.undoes.is_none() && !b.undone && b.undoable)
}

fn save(backup: &TableBackup, dir: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let stem: String = backup
        .disk
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let stamp = timestamp(backup.created / 1000);
    let text = serde_json::to_string_pretty(backup).map_err(io::Error::other)?;
    for n in 0..1000 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let path = dir.join(format!("{stem}-{stamp}{suffix}.drtable"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                File::sync_all(&file)?;
                return Ok(path);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("too many backups made this second"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `20261024-115013` (UTC).
fn timestamp(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let rest = seconds % 86_400;
    // Days since 1970 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        text.push(char::from(DIGITS[usize::from(b >> 4)]));
        text.push(char::from(DIGITS[usize::from(b & 0xF)]));
    }
    text
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 0xAB, 0xFF];
        assert_eq!(to_hex(&bytes), "0001abff");
        assert_eq!(from_hex("0001abff").as_deref(), Some(&bytes[..]));
        assert_eq!(from_hex("0g"), None);
        assert_eq!(from_hex("abc"), None);
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0), "19700101-000000");
        assert_eq!(timestamp(1_000_000_000), "20010909-014640");
        assert_eq!(timestamp(951_782_400), "20000229-000000", "a leap day");
    }
}
