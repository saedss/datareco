//! `move`, `align` and `resize`. Each shows what it would do; it only
//! writes with `--apply` and the disk's name.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use datareco_io::manage::{partition_size_limits, resize_with_windows};
use datareco_io::{
    Disk, DiskTarget, FileSource, ImageTarget, SectorTarget, SourceSpec, check_disk_for_changes,
    list_devices,
};
use datareco_partitions::edit::{default_backup_dir, plan_move};
use datareco_partitions::{FsKind, Partition, PartitionKind, read_table};
use datareco_relocate::{MovePhase, Mover, align_target, default_journal_dir, unfinished};

use crate::CliError;
use crate::format::format_size;

/// How long each move step runs before showing progress.
const STEP: Duration = Duration::from_millis(500);

pub struct MoveRequest<'a> {
    pub source: &'a str,
    pub partition: Option<u32>,
    /// The new first sector.
    pub to_sector: Option<u64>,
    /// Or how far: "+100MiB", "-1GiB".
    pub by: Option<&'a str>,
    pub resume: bool,
    pub journal_dir: Option<&'a Path>,
    pub backup_dir: Option<&'a Path>,
    pub apply: Option<&'a str>,
}

fn partition(disk: &Disk, source: &str, number: u32) -> Result<Partition, CliError> {
    read_table(disk)?
        .partitions
        .into_iter()
        .find(|p| p.number == number && p.kind != PartitionKind::Extended)
        .ok_or_else(|| {
            CliError::Message(format!(
                "There's no partition {number}; `datareco partitions {source}` lists them."
            ))
        })
}

/// "+100MiB", "-1GiB", "+2048s" → a signed number of sectors.
fn parse_by(text: &str, sector: u64) -> Result<i64, CliError> {
    let text = text.trim();
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.trim_start_matches('+')),
    };
    let lower = rest.to_ascii_lowercase();
    let (number, bytes) = if let Some(n) = lower.strip_suffix('s') {
        (n, sector)
    } else if let Some(n) = lower
        .strip_suffix("mib")
        .or_else(|| lower.strip_suffix('m'))
    {
        (n, 1 << 20)
    } else if let Some(n) = lower
        .strip_suffix("gib")
        .or_else(|| lower.strip_suffix('g'))
    {
        (n, 1 << 30)
    } else {
        return Err(CliError::Message(format!(
            "\"{text}\" isn't a distance: use +100MiB, -1GiB or +2048s (sectors)."
        )));
    };
    let count: i64 = number
        .trim()
        .parse()
        .map_err(|_| CliError::Message(format!("\"{text}\" isn't a distance.")))?;
    let total = count
        .checked_mul(i64::try_from(bytes).unwrap_or(i64::MAX))
        .ok_or_else(|| CliError::Message("That's too far.".into()))?;
    if total % i64::try_from(sector).unwrap_or(512) != 0 {
        return Err(CliError::Message("Move by whole sectors.".into()));
    }
    Ok(sign * total / i64::try_from(sector).unwrap_or(512))
}

pub fn move_partition(request: &MoveRequest<'_>) -> Result<(), CliError> {
    let spec = SourceSpec::parse(request.source);
    if let SourceSpec::Volume(_) = spec {
        return Err(CliError::Message(
            "Give the disk (disk1) and the partition with -p.".into(),
        ));
    }
    let disk = Disk::open(&spec)?;
    let journal_dir = request
        .journal_dir
        .map_or_else(default_journal_dir, Path::to_path_buf);
    if request.resume {
        let journal = unfinished(&journal_dir, &spec.id()).ok_or_else(|| {
            CliError::Message(format!(
                "There's no unfinished move on {} (journals are in {}).",
                disk.name(),
                journal_dir.display()
            ))
        })?;
        println!("Carrying on the move recorded in {}.", journal.display());
        let mover = Mover::resume(&journal, &disk)?;
        return run(&spec, &disk, mover, &journal_dir, request.apply, true);
    }
    let number = request
        .partition
        .ok_or_else(|| CliError::Message("Which partition? Give it with -p.".into()))?;
    let p = partition(&disk, request.source, number)?;
    let to = match (request.to_sector, request.by) {
        (Some(to), _) => to,
        (None, Some(by)) => {
            let delta = parse_by(by, u64::from(disk.sector_size()))?;
            p.start_lba
                .checked_add_signed(delta)
                .ok_or_else(|| CliError::Message("That's before the disk's start.".into()))?
        }
        (None, None) => {
            return Err(CliError::Message(
                "Where to? Give --to-sector, or --by (+100MiB, -1GiB …).".into(),
            ));
        }
    };
    let backup_dir = request
        .backup_dir
        .map_or_else(default_backup_dir, Path::to_path_buf);
    start(
        &spec,
        &disk,
        &p,
        to,
        (&journal_dir, &backup_dir),
        request.apply,
    )
}

/// Moves partition `p` to start at sector `to`: shows it, then (with
/// `--apply`) does it.
fn start(
    spec: &SourceSpec,
    disk: &Disk,
    p: &Partition,
    to: u64,
    (journal_dir, backup_dir): (&Path, &Path),
    apply: Option<&str>,
) -> Result<(), CliError> {
    let plan = plan_move(disk, p.start_lba, to)?;
    if let Some(reason) = plan.blocked {
        return Err(CliError::Message(reason));
    }
    if let Some(error) = plan.error {
        return Err(CliError::Message(error.message));
    }
    let bytes = p.sectors * u64::from(disk.sector_size());
    println!("{}.", plan.descriptions().join("; "));
    println!(
        "Copies {} block by block (at 100 MiB/s, {}). A journal in {} lets it carry on after a \
         power cut or Ctrl+C.",
        format_size(bytes),
        match (bytes >> 20) / 100 / 60 {
            0 => "under a minute".to_owned(),
            1 => "about a minute".to_owned(),
            n => format!("about {n} minutes"),
        },
        journal_dir.display()
    );
    let Some(_) = apply else {
        let confirmation = confirmation(spec)?;
        println!(
            "Nothing was written. To move it, run the same command with --apply {confirmation}"
        );
        return Ok(());
    };
    check_journal_place(spec, journal_dir)?;
    let mover = Mover::begin(disk, &spec.id(), p.start_lba, to, journal_dir, backup_dir)?;
    run(spec, disk, mover, journal_dir, apply, false)
}

/// What the person types to confirm: the disk's name, or the image's file name.
fn confirmation(spec: &SourceSpec) -> Result<String, CliError> {
    Ok(match spec {
        SourceSpec::PhysicalDisk(n) => format!("disk{n}"),
        _ => spec.display_name(),
    })
}

/// The journal must be on another disk: the one being moved on is closed.
fn check_journal_place(spec: &SourceSpec, journal_dir: &Path) -> Result<(), CliError> {
    if let SourceSpec::PhysicalDisk(n) = spec
        && let Ok(devices) = list_devices()
        && devices.disks_of_path(journal_dir).contains(n)
    {
        return Err(CliError::Message(format!(
            "The journal folder {} is on the disk being changed; pick another with --journal-dir.",
            journal_dir.display()
        )));
    }
    Ok(())
}

fn run(
    spec: &SourceSpec,
    disk: &Disk,
    mut mover: Mover,
    journal_dir: &Path,
    apply: Option<&str>,
    resuming: bool,
) -> Result<(), CliError> {
    let Some(typed) = apply else {
        let confirmation = confirmation(spec)?;
        println!(
            "Nothing was written. To carry it on, run the same command with --apply {confirmation}"
        );
        return Ok(());
    };
    check_journal_place(spec, journal_dir)?;
    let mut target: Box<dyn SectorTarget> = match spec {
        SourceSpec::PhysicalDisk(number) => {
            let devices = list_devices()?;
            let check = check_disk_for_changes(*number, &devices)?;
            if !check.volumes.is_empty() {
                println!(
                    "Closing {} while the partition moves.",
                    check.volumes.join(", ")
                );
            }
            Box::new(DiskTarget::open(&check, typed)?)
        }
        SourceSpec::Image(path) => {
            if !typed.trim().eq_ignore_ascii_case(&spec.display_name()) {
                return Err(CliError::Message(format!(
                    "Type --apply {} to confirm.",
                    spec.display_name()
                )));
            }
            Box::new(ImageTarget::resume(path, disk.size())?)
        }
        SourceSpec::Volume(_) => unreachable!("refused before"),
    };
    let mut source = FileSource::open(spec)?;
    if resuming {
        println!("{}", mover.progress().description);
    }
    let progress = loop {
        let progress = match mover.step(&mut source, target.as_mut(), STEP) {
            Ok(progress) => progress,
            Err(e) => {
                eprintln!();
                return Err(CliError::Message(format!(
                    "The move stopped: {e}. Its journal is {}; once that's sorted out, carry it \
                     on with: datareco move {} --resume --apply {}",
                    mover.progress().journal.display(),
                    source_arg(spec),
                    confirmation(spec)?
                )));
            }
        };
        let what = match progress.phase {
            MovePhase::Copying => "copied",
            MovePhase::FixingBoot => "copied; updating the boot sector",
            MovePhase::WritingTable | MovePhase::Done => "copied; writing the partition table",
        };
        #[allow(clippy::cast_precision_loss)] // for people
        let percent = progress.done as f64 * 100.0 / progress.total.max(1) as f64;
        eprint!(
            "\r  {} of {} {what} ({percent:.1}%), {}/s        ",
            format_size(progress.done),
            format_size(progress.total),
            format_size(progress.bytes_per_second)
        );
        let _ = std::io::stderr().flush();
        if progress.finished {
            break progress;
        }
    };
    eprintln!();
    drop(target);
    println!(
        "Moved. The old partition table is saved in {} (a move can't be undone from it: move the \
         partition back instead).",
        progress
            .backup
            .as_ref()
            .map_or_else(String::new, |b| b.display().to_string())
    );
    Ok(())
}

/// Refuses to change a disk while a move on it is left unfinished:
/// changing it first could stop the move carrying on.
pub fn check_no_unfinished_move(spec: &SourceSpec) -> Result<(), CliError> {
    let mut ids = vec![spec.id()];
    if let Ok(devices) = list_devices() {
        ids.extend(
            devices
                .disks_of_source(spec)
                .into_iter()
                .map(|d| SourceSpec::PhysicalDisk(d).id()),
        );
    }
    let journals = default_journal_dir();
    for id in ids {
        if let Some(journal) = unfinished(&journals, &id) {
            let moved = SourceSpec::parse(&id);
            return Err(CliError::Message(format!(
                "A partition move on {} didn't finish (its journal is {}). Carry it on first: \
                 datareco move {} --resume --apply {}",
                moved.display_name(),
                journal.display(),
                source_arg(&moved),
                confirmation(&moved)?
            )));
        }
    }
    Ok(())
}

fn source_arg(spec: &SourceSpec) -> String {
    match spec {
        SourceSpec::PhysicalDisk(n) => format!("disk{n}"),
        SourceSpec::Image(path) => format!("\"{}\"", path.display()),
        SourceSpec::Volume(v) => v.clone(),
    }
}

/// Moves a partition to the nearest 1 MiB boundary it has room for.
pub fn align(
    source: &str,
    number: u32,
    (journal_dir, backup_dir): (Option<&Path>, Option<&Path>),
    apply: Option<&str>,
) -> Result<(), CliError> {
    let spec = SourceSpec::parse(source);
    let disk = Disk::open(&spec)?;
    let p = partition(&disk, source, number)?;
    let sector = u64::from(disk.sector_size());
    let align = (1 << 20) / sector;
    if p.start_lba.is_multiple_of(align) {
        println!(
            "Partition {number} already starts on a 1 MiB boundary (sector {}).",
            p.start_lba
        );
        return Ok(());
    }
    if (p.start_lba * sector).is_multiple_of(4096) {
        println!(
            "note: it's already on a 4 KiB boundary, which is what drives with 4 KiB sectors need."
        );
    }
    let down = p.start_lba / align * align;
    let to = align_target(&disk, p.start_lba)?.ok_or_else(|| {
        CliError::Message(format!(
            "There's no room to move partition {number} to a 1 MiB boundary (sector {down} or {}).",
            down + align
        ))
    })?;
    let journal_dir: PathBuf = journal_dir.map_or_else(default_journal_dir, Path::to_path_buf);
    let backup_dir: PathBuf = backup_dir.map_or_else(default_backup_dir, Path::to_path_buf);
    start(&spec, &disk, &p, to, (&journal_dir, &backup_dir), apply)
}

/// Shrinks or grows an NTFS partition through Windows.
pub fn resize(source: &str, number: u32, size: &str, apply: Option<&str>) -> Result<(), CliError> {
    let spec = SourceSpec::parse(source);
    let SourceSpec::PhysicalDisk(disk_number) = spec else {
        return Err(CliError::Message(
            "Windows resizes partitions on disks: give the disk (disk1) and -p.".into(),
        ));
    };
    let disk = Disk::open(&spec)?;
    let p = partition(&disk, source, number)?;
    if !matches!(
        p.file_system.as_ref().map(|f| f.kind),
        Some(FsKind::Ntfs | FsKind::Refs)
    ) {
        return Err(CliError::Message(
            "Windows resizes NTFS (and ReFS) partitions; this one isn't. Moving works for any \
             partition."
                .into(),
        ));
    }
    let sector = u64::from(disk.sector_size());
    let offset = p.start_lba * sector;
    let now = p.sectors * sector;
    println!("Asking Windows how far partition {number} can shrink and grow…");
    let (min, max) = partition_size_limits(disk_number, offset)?;
    println!(
        "Partition {number} is {}; Windows can make it {} to {}.",
        format_size(now),
        format_size(min),
        format_size(max)
    );
    let wanted = match size.trim().to_ascii_lowercase().as_str() {
        "max" => max,
        "min" => min,
        other => parse_bytes(other)?,
    };
    if wanted < min || wanted > max {
        return Err(CliError::Message(format!(
            "{} is outside what Windows can do ({} to {}).",
            format_size(wanted),
            format_size(min),
            format_size(max)
        )));
    }
    println!(
        "{} it to {}{}.",
        if wanted < now { "Shrink" } else { "Grow" },
        format_size(wanted),
        if wanted < now {
            " (Windows moves files out of the way first)"
        } else {
            ""
        }
    );
    let Some(typed) = apply else {
        println!(
            "Nothing was changed. To do it, run the same command with --apply disk{disk_number}"
        );
        return Ok(());
    };
    if !typed
        .trim()
        .eq_ignore_ascii_case(&format!("disk{disk_number}"))
    {
        return Err(CliError::Message(format!(
            "Type --apply disk{disk_number} to confirm."
        )));
    }
    check_no_unfinished_move(&spec)?;
    resize_with_windows(disk_number, offset, wanted)?;
    println!("Resized.");
    Ok(())
}

/// "20GiB", "500M" → bytes.
fn parse_bytes(text: &str) -> Result<u64, CliError> {
    let lower = text.trim().to_ascii_lowercase();
    let split = lower
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(lower.len());
    let (number, unit) = lower.split_at(split);
    let shift = match unit.trim() {
        "k" | "kib" => 10,
        "m" | "mib" => 20,
        "g" | "gib" => 30,
        "t" | "tib" => 40,
        _ => {
            return Err(CliError::Message(format!(
                "\"{text}\" isn't a size: use 50GiB, 500MiB, min or max."
            )));
        }
    };
    let value: f64 = number
        .parse()
        .map_err(|_| CliError::Message(format!("\"{text}\" isn't a size.")))?;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    Ok((value * (1u64 << shift) as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(parse_by("+100MiB", 512).unwrap(), 204_800);
        assert_eq!(parse_by("-1GiB", 512).unwrap(), -2_097_152);
        assert_eq!(parse_by("2048s", 512).unwrap(), 2048);
        assert_eq!(parse_by("+1M", 4096).unwrap(), 256);
        assert!(parse_by("100", 512).is_err());
        assert!(parse_by("+1s", 4096).is_ok());
        assert_eq!(parse_bytes("1.5GiB").unwrap(), 3 << 29);
    }
}
