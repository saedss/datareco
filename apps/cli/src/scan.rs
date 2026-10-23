//! `scan` and `recover`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use datareco_fs::{Entry, FileSystem, resolve};
use datareco_io::{Disk, SourceSpec};
use datareco_scan::{
    Control, FoundVolume, RecoverError, RecoverOptions, SavedScan, Scan, ScanError, ScanMode,
    VolumeState,
};

use crate::CliError;
use crate::files::{VolumeChoice, describe, open_volume};
use crate::format::format_size;

/// Bytes each scan step examines before printing progress.
const STEP: u64 = 256 << 20;
/// How often a scan being saved is written out.
const SAVE_EVERY: Duration = Duration::from_secs(15);

impl From<ScanError> for CliError {
    fn from(error: ScanError) -> Self {
        match error {
            ScanError::Disk(error) => CliError::Disk(error),
            other => CliError::Message(other.to_string()),
        }
    }
}

impl From<RecoverError> for CliError {
    fn from(error: RecoverError) -> Self {
        CliError::Message(error.to_string())
    }
}

pub fn scan(
    source: &str,
    full: bool,
    save: Option<&Path>,
    resume: Option<&Path>,
) -> Result<(), CliError> {
    let spec = SourceSpec::parse(source);
    let disk = Arc::new(Disk::open(&spec)?);
    let mut scan = match resume {
        Some(path) => {
            let saved = SavedScan::read_from(path)?;
            if saved.source != spec.id() {
                eprintln!("note: this scan was made of {}", saved.source);
            }
            Scan::restore(&disk, &saved)?
        }
        None => Scan::new(
            &disk,
            if full {
                ScanMode::Full
            } else {
                ScanMode::Quick
            },
        ),
    };
    // A resumed scan keeps saving where it came from.
    let save_to: Option<PathBuf> = save.or(resume).map(Path::to_path_buf);
    let mut saved_at = Instant::now();
    loop {
        let progress = scan.step(&disk, STEP)?;
        let mut line = format!(
            "\rScanning… {} of {}, {} volume clues",
            format_size(progress.position),
            format_size(progress.total),
            progress.volume_clues
        );
        if progress.mode == ScanMode::Full {
            line.push_str(&format!(
                ", {} MFT records, {} folder sectors",
                progress.mft_records, progress.folders
            ));
        }
        eprint!("{line}   ");
        if let Some(path) = &save_to
            && (progress.done || saved_at.elapsed() >= SAVE_EVERY)
        {
            scan.save(&spec.id())
                .write_to(path)
                .map_err(|e| CliError::Message(format!("Saving the scan failed: {e}")))?;
            saved_at = Instant::now();
        }
        if progress.done {
            eprintln!();
            break;
        }
    }
    let found = scan.results(&disk)?;
    print_found(&found, scan.mode());
    if let Some(path) = &save_to {
        println!();
        println!(
            "The scan is saved in {}. To look inside a volume:",
            path.display()
        );
        println!(
            "  datareco ls {source} --scan \"{}\" --volume 1",
            path.display()
        );
    }
    Ok(())
}

/// What a saved scan found, for `--scan FILE --volume N`.
pub fn saved_results(
    disk: &Arc<Disk>,
    source: &str,
    path: &Path,
) -> Result<Vec<FoundVolume>, CliError> {
    let saved = SavedScan::read_from(path)?;
    if saved.source != SourceSpec::parse(source).id() {
        eprintln!("note: this scan was made of {}", saved.source);
    }
    if !saved.done {
        eprintln!("note: this scan didn't finish; it has what it found up to then");
    }
    Ok(Scan::restore(disk, &saved)?.results(disk)?)
}

fn print_found(found: &[FoundVolume], mode: ScanMode) {
    let mode_name = if mode == ScanMode::Full {
        "full"
    } else {
        "quick"
    };
    if found.is_empty() {
        println!("No volumes found ({mode_name} scan).");
        if mode == ScanMode::Quick {
            println!(
                "A full scan (--full) looks at every sector and finds more, but takes longer."
            );
        }
        return;
    }
    println!(
        "Found {} volume(s) ({mode_name} scan), best first:",
        found.len()
    );
    println!();
    println!(
        "  {:>2}  {:<24}  {:>14}  {:>10}  {:>5}  How",
        "#", "File system", "Starts at byte", "Size", "Score"
    );
    for (i, volume) in found.iter().enumerate() {
        let name = match &volume.label {
            Some(label) => format!("{} \"{label}\"", volume.name),
            None => volume.name.clone(),
        };
        let how = match volume.state {
            VolumeState::Intact if volume.in_table => "opens normally",
            VolumeState::Intact => "opens normally; a lost partition (not in the partition table)",
            VolumeState::FromBackup => "opens with its backup boot sector",
            VolumeState::Rebuilt => "rebuilt from the folders found",
        };
        println!(
            "  {:>2}  {name:<24}  {:>14}  {:>10}  {:>5}  {how}",
            i + 1,
            volume.offset,
            format_size(volume.size),
            volume.score
        );
        for clue in &volume.evidence {
            println!("  {:>2}    - {clue}", "");
        }
    }
}

pub struct RecoverRequest<'a> {
    pub source: &'a str,
    pub paths: &'a [String],
    pub to: &'a Path,
    pub choice: &'a VolumeChoice,
    pub options: RecoverOptions,
    pub prefer_deleted: bool,
}

pub fn recover(request: &RecoverRequest) -> Result<(), CliError> {
    let spec = SourceSpec::parse(request.source);
    datareco_scan::check_destination(&spec, request.to)?;
    let fs = open_volume(request.source, request.choice)?;
    let items: Vec<Entry> = if request.paths.is_empty() {
        // Everything, but not NTFS's own system files ($MFT and the like).
        fs.list(&fs.root())?
            .into_iter()
            .filter(|e| !datareco_scan::is_system_file(e))
            .collect()
    } else {
        request
            .paths
            .iter()
            .map(|path| resolve(fs.as_ref(), path, request.prefer_deleted))
            .collect::<Result<_, _>>()?
    };
    let source_text = format!("{}, {}", spec.display_name(), describe(fs.as_ref()));
    save_items(
        fs.as_ref(),
        &source_text,
        &items,
        request.to,
        request.options,
    )
}

/// Saves `items` from `fs` into `to`, showing progress, then says how it went.
pub fn save_items(
    fs: &dyn FileSystem,
    source_text: &str,
    items: &[Entry],
    to: &Path,
    options: RecoverOptions,
) -> Result<(), CliError> {
    let mut shown = Instant::now();
    let summary = datareco_scan::recover(fs, source_text, items, to, options, &mut |p| {
        if shown.elapsed() >= Duration::from_millis(250) {
            let current: String = p
                .current
                .chars()
                .rev()
                .take(40)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            eprint!(
                "\rSaving… {} files, {} folders, {}  {current:<40}",
                p.files,
                p.folders,
                format_size(p.bytes)
            );
            shown = Instant::now();
        }
        Control::Continue
    })?;
    eprintln!("\r{:<100}", "");
    let p = &summary.progress;
    println!(
        "Saved {} ({}) to {}.",
        counted(p.files, p.folders),
        format_size(p.bytes),
        summary.destination.display()
    );
    if p.incomplete > 0 || p.failed > 0 {
        println!(
            "{} saved only in part and {} not saved: the log says which and why.",
            p.incomplete, p.failed
        );
    }
    println!("Log: {}", summary.log.display());
    println!("Checksums: {}", summary.checksums.display());
    Ok(())
}

/// "3 files and 1 folder", "1 file", "2 folders", "nothing".
pub fn counted(files: u64, folders: u64) -> String {
    let one = |n: u64, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
    match (files, folders) {
        (0, 0) => "nothing".to_owned(),
        (_, 0) => one(files, "file"),
        (0, _) => one(folders, "folder"),
        _ => format!("{} and {}", one(files, "file"), one(folders, "folder")),
    }
}
