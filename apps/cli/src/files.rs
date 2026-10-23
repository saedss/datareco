//! `ls` and `cat`: browse FAT, exFAT and NTFS volumes, deleted files included.

use std::collections::HashSet;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;

use datareco_fs::{Entry, EntryId, FileSystem, FsError, FsKind, resolve};
use datareco_io::{Disk, SourceSpec};
use datareco_partitions::read_table;

use crate::CliError;
use crate::format::{format_size, format_time};

/// Folders deeper than this aren't listed by `ls -r` (damaged volumes can
/// have folders that contain themselves).
const MAX_DEPTH: usize = 64;

impl From<FsError> for CliError {
    fn from(error: FsError) -> Self {
        match error {
            FsError::Disk(error) => CliError::Disk(error),
            other => CliError::Message(other.to_string()),
        }
    }
}

/// Which volume of a disk to open: a partition number, a byte offset, or
/// (neither) the only one there is.
pub struct VolumeChoice {
    pub partition: Option<u32>,
    pub offset: Option<u64>,
}

fn readable(kind: FsKind) -> bool {
    matches!(
        kind,
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::Exfat | FsKind::Ntfs
    )
}

fn open_volume(source: &str, choice: &VolumeChoice) -> Result<Box<dyn FileSystem>, CliError> {
    let disk = Arc::new(Disk::open(&SourceSpec::parse(source))?);
    let offset = match (choice.offset, choice.partition) {
        (Some(offset), _) => offset,
        (None, Some(number)) => {
            let table = read_table(&disk)?;
            let partition = table
                .partitions
                .iter()
                .find(|p| p.number == number)
                .ok_or_else(|| {
                    CliError::Message(format!(
                        "There's no partition {number}; `datareco partitions {source}` lists them."
                    ))
                })?;
            partition.start_lba * u64::from(table.sector_size)
        }
        (None, None) => {
            let table = read_table(&disk)?;
            let candidates: Vec<_> = table
                .partitions
                .iter()
                .filter(|p| p.file_system.as_ref().is_some_and(|fs| readable(fs.kind)))
                .collect();
            match candidates.as_slice() {
                _ if table.whole_disk.is_some() || table.partitions.is_empty() => 0,
                [only] => only.start_lba * u64::from(table.sector_size),
                [] => {
                    return Err(CliError::Message(
                        "No partition has a file system DataReco can read (FAT, exFAT or NTFS)."
                            .to_owned(),
                    ));
                }
                several => {
                    let numbers: Vec<String> =
                        several.iter().map(|p| p.number.to_string()).collect();
                    return Err(CliError::Message(format!(
                        "This disk has {} volumes DataReco can read; pick one with --partition ({}).",
                        several.len(),
                        numbers.join(", ")
                    )));
                }
            }
        }
    };
    Ok(datareco_fs::open(disk, offset)?)
}

fn describe(fs: &dyn FileSystem) -> String {
    let info = fs.info();
    let label = info
        .label
        .as_ref()
        .map(|l| format!(" \"{l}\""))
        .unwrap_or_default();
    format!(
        "{}{label}, {}, cluster size {}",
        info.name,
        format_size(info.size),
        format_size(info.cluster_size)
    )
}

fn folder_id(fs: &dyn FileSystem, path: &str) -> Result<EntryId, CliError> {
    if path.trim_matches(['/', '\\']).is_empty() {
        return Ok(fs.root());
    }
    let entry = resolve(fs, path, false)?;
    if !entry.is_dir {
        return Err(CliError::Message(format!(
            "\"{path}\" is a file; `datareco cat` shows files."
        )));
    }
    Ok(entry.id)
}

pub fn ls(
    source: &str,
    path: &str,
    choice: &VolumeChoice,
    deleted_only: bool,
    recursive: bool,
) -> Result<(), CliError> {
    let fs = open_volume(source, choice)?;
    let folder = folder_id(fs.as_ref(), path)?;
    println!("{}", describe(fs.as_ref()));
    println!();
    println!(
        "  {:<16}  {:>10}  {:<7}  Name",
        "Modified", "Size", "Status"
    );
    let mut listing = Listing {
        fs: fs.as_ref(),
        deleted_only,
        recursive,
        visited: HashSet::new(),
        files: 0,
        folders: 0,
        deleted: 0,
    };
    listing.folder(&folder, "", 0)?;
    println!();
    println!(
        "{} file(s), {} folder(s); {} deleted.",
        listing.files, listing.folders, listing.deleted
    );
    Ok(())
}

struct Listing<'a> {
    fs: &'a dyn FileSystem,
    deleted_only: bool,
    recursive: bool,
    visited: HashSet<EntryId>,
    files: usize,
    folders: usize,
    deleted: usize,
}

impl Listing<'_> {
    fn folder(&mut self, id: &EntryId, prefix: &str, depth: usize) -> Result<(), CliError> {
        if !self.visited.insert(id.clone()) {
            return Ok(());
        }
        let entries = match self.fs.list(id) {
            Ok(entries) => entries,
            // A damaged subfolder shouldn't end a recursive listing.
            Err(error) if depth > 0 => {
                eprintln!("warning: can't list {prefix}: {error}");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        };
        for entry in &entries {
            if !self.deleted_only || entry.deleted {
                print_entry(entry, prefix);
                if entry.is_dir {
                    self.folders += 1;
                } else {
                    self.files += 1;
                }
                self.deleted += usize::from(entry.deleted);
            }
            if self.recursive && entry.is_dir && depth < MAX_DEPTH {
                self.folder(&entry.id, &format!("{prefix}{}/", entry.name), depth + 1)?;
            }
        }
        Ok(())
    }
}

fn print_entry(entry: &Entry, prefix: &str) {
    let modified = entry.modified.map(format_time).unwrap_or_default();
    let size = if entry.is_dir {
        "-".to_owned()
    } else {
        format_size(entry.size)
    };
    let status = if entry.deleted { "deleted" } else { "" };
    let slash = if entry.is_dir { "/" } else { "" };
    let streams = if entry.streams.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = entry.streams.iter().map(|s| s.name.as_str()).collect();
        format!("  [streams: {}]", names.join(", "))
    };
    println!(
        "  {modified:<16}  {size:>10}  {status:<7}  {prefix}{}{slash}{streams}",
        entry.name
    );
    // Only notes that change what the file is worth get a line.
    if let Some(note) = entry
        .note
        .as_deref()
        .filter(|n| n.contains("overwritten") || n.contains("Encrypted"))
    {
        println!(
            "  {:<16}  {:>10}  {:<7}    {}",
            "",
            "",
            "",
            note.trim_start_matches("Deleted. ")
        );
    }
}

pub fn cat(
    source: &str,
    path: &str,
    choice: &VolumeChoice,
    stream: Option<&str>,
    deleted: bool,
) -> Result<(), CliError> {
    let fs = open_volume(source, choice)?;
    let entry = resolve(fs.as_ref(), path, deleted)?;
    if entry.is_dir && stream.is_none() {
        return Err(CliError::Message(format!(
            "\"{path}\" is a folder; `datareco ls` lists folders."
        )));
    }
    if let Some(note) = &entry.note {
        eprintln!("note: {note}");
    }
    let stdout = io::stdout();
    let terminal = stdout.is_terminal();
    let mut out = stdout.lock();
    let mut buf = vec![0; 1 << 20];
    let mut offset = 0;
    loop {
        let n = fs.read(&entry.id, stream, offset, &mut buf)?;
        if n == 0 {
            break;
        }
        // A terminal can only show text; binary data has to go to a file.
        if terminal
            && offset == 0
            && std::str::from_utf8(&buf[..n]).is_err_and(|e| e.error_len().is_some())
        {
            return Err(CliError::Message(format!(
                "\"{path}\" isn't text. Redirect it to a file on another disk, e.g. \
                 datareco cat {source} \"{path}\" > recovered.bin (in cmd or PowerShell 7)"
            )));
        }
        match out.write_all(&buf[..n]) {
            Ok(()) => {}
            // `| more` and friends may stop reading early.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => {
                return Err(CliError::Message(format!(
                    "Writing the output failed: {error}"
                )));
            }
        }
        offset += n as u64;
    }
    match out.flush() {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(CliError::Message(format!(
            "Writing the output failed: {error}"
        ))),
        _ => Ok(()),
    }
}
