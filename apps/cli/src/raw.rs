//! `raw` and `signatures`: finding files by their contents.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use datareco_fs::FileSystem;
use datareco_io::{Disk, SourceSpec};
use datareco_partitions::read_table;
use datareco_scan::RecoverOptions;
use datareco_scan::raw::{
    DEFAULT_SIGNATURES, End, RawScan, SavedRawScan, Signature, default_signatures, load_signatures,
};

use crate::CliError;
use crate::format::format_size;

/// Bytes each step examines before printing progress.
const STEP: u64 = 256 << 20;
/// How often a scan being saved is written out.
const SAVE_EVERY: Duration = Duration::from_secs(15);

pub fn signatures(file: Option<&Path>, write: Option<&Path>) -> Result<(), CliError> {
    if let Some(path) = write {
        if path.exists() {
            return Err(CliError::Message(format!(
                "{} already exists; pick a new file to write the built-in list to.",
                path.display()
            )));
        }
        std::fs::write(path, DEFAULT_SIGNATURES)
            .map_err(|e| CliError::Message(format!("Writing {} failed: {e}", path.display())))?;
        println!("Wrote the built-in signatures to {}.", path.display());
        println!("Edit it (the comments at the top explain how), then use it with --signatures.");
        return Ok(());
    }
    let list = signature_list(file)?;
    println!(
        "  {:<44}  {:<6}  {:<30}  Where it ends",
        "Name", "Saved", "Starts with"
    );
    for s in &list {
        let header: Vec<String> = s
            .header
            .iter()
            .map(|b| b.map_or_else(|| "??".to_owned(), |b| format!("{b:02X}")))
            .collect();
        let mut header = header.join(" ");
        if s.header_offset > 0 {
            header = format!("{header} (at {})", s.header_offset);
        }
        let end = match &s.end {
            End::Parser(parser) => format!("read as {}", format!("{parser:?}").to_lowercase()),
            End::Footer(footer) => {
                let hex: Vec<String> = footer.iter().map(|b| format!("{b:02X}")).collect();
                format!("after {}", hex.join(" "))
            }
            End::Field { offset, bytes, .. } => format!("size at byte {offset} ({bytes} bytes)"),
            End::Unknown => "at the next file".to_owned(),
        };
        println!(
            "  {:<44}  .{:<5}  {header:<30}  {end}, at most {}",
            s.name,
            s.extension,
            format_size(s.max_size)
        );
    }
    println!();
    println!(
        "{} signatures. To change them: datareco signatures --write my-signatures.toml",
        list.len()
    );
    Ok(())
}

fn signature_list(file: Option<&Path>) -> Result<Vec<Signature>, CliError> {
    match file {
        Some(path) => load_signatures(path).map_err(CliError::Message),
        None => Ok(default_signatures()),
    }
}

/// What to scan: a partition, from a byte offset, or (neither) the whole disk.
pub struct RawRequest<'a> {
    pub source: &'a str,
    pub partition: Option<u32>,
    pub offset: Option<u64>,
    pub signatures: Option<&'a Path>,
    pub save: Option<&'a Path>,
    pub resume: Option<&'a Path>,
    pub to: Option<&'a Path>,
}

pub fn raw(request: &RawRequest) -> Result<(), CliError> {
    let spec = SourceSpec::parse(request.source);
    if let Some(to) = request.to {
        datareco_scan::check_destination(&spec, to)?;
    }
    let disk = Arc::new(Disk::open(&spec)?);
    let signatures = signature_list(request.signatures)?;
    let mut scan = match request.resume {
        Some(path) => {
            let saved = SavedRawScan::read_from(path)?;
            if saved.source != spec.id() {
                eprintln!("note: this scan was made of {}", saved.source);
            }
            RawScan::restore(&disk, signatures, &saved)?
        }
        None => {
            let range = range(&disk, request.source, request.partition, request.offset)?;
            RawScan::new(&disk, signatures, range)
        }
    };
    let save_to: Option<PathBuf> = request.save.or(request.resume).map(Path::to_path_buf);
    let mut saved_at = Instant::now();
    loop {
        let p = scan.step(&disk, STEP)?;
        eprint!(
            "\rLooking for files… {} of {}, {} found   ",
            format_size(p.position - p.start),
            format_size(p.end - p.start),
            p.files
        );
        if let Some(path) = &save_to
            && (p.done || saved_at.elapsed() >= SAVE_EVERY)
        {
            scan.save(&spec.id(), &disk)
                .write_to(path)
                .map_err(|e| CliError::Message(format!("Saving the scan failed: {e}")))?;
            saved_at = Instant::now();
        }
        if p.done {
            eprintln!();
            break;
        }
    }
    let volume = scan.volume(Arc::clone(&disk));
    let count = volume.file_count() as u64;
    if count == 0 {
        println!("No files found by their contents.");
    } else {
        println!(
            "Found {} by their contents:",
            crate::scan::counted(count, 0)
        );
    }
    for (extension, count) in volume.counts() {
        println!("  {extension:<8} {count:>8}");
    }
    if let Some(to) = request.to {
        println!();
        let items = volume.list(&volume.root())?;
        let source = format!("{}, files found by their contents", spec.display_name());
        crate::scan::save_items(&volume, &source, &items, to, RecoverOptions::default())?;
    } else if volume.file_count() > 0 {
        println!();
        match &save_to {
            Some(path) => println!(
                "To save them: datareco recover {} --raw \"{}\" --to FOLDER",
                request.source,
                path.display()
            ),
            None => {
                println!("To save them, run again with --to FOLDER (a folder on another disk).")
            }
        }
    }
    Ok(())
}

/// The byte range to scan.
fn range(
    disk: &Disk,
    source: &str,
    partition: Option<u32>,
    offset: Option<u64>,
) -> Result<Option<(u64, u64)>, CliError> {
    if let Some(offset) = offset {
        return Ok(Some((offset, disk.size())));
    }
    let Some(number) = partition else {
        return Ok(None);
    };
    let table = read_table(disk)?;
    let p = table
        .partitions
        .iter()
        .find(|p| p.number == number)
        .ok_or_else(|| {
            CliError::Message(format!(
                "There's no partition {number}; `datareco partitions {source}` lists them."
            ))
        })?;
    let sector = u64::from(table.sector_size);
    Ok(Some((
        p.start_lba * sector,
        (p.start_lba + p.sectors) * sector,
    )))
}

/// What a saved raw scan found, as a volume (for `--raw FILE`).
pub fn saved_volume(
    disk: Arc<Disk>,
    source: &str,
    path: &Path,
) -> Result<Box<dyn FileSystem>, CliError> {
    let saved = SavedRawScan::read_from(path)?;
    if saved.source != SourceSpec::parse(source).id() {
        eprintln!("note: this scan was made of {}", saved.source);
    }
    if !saved.done {
        eprintln!("note: this scan didn't finish; it has what it found up to then");
    }
    let scan = RawScan::restore(&disk, default_signatures(), &saved)?;
    Ok(Box::new(scan.volume(disk)))
}
