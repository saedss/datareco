//! `clone-partition` and `migrate`: copying a partition into free space on
//! another disk, or every partition of a disk onto another disk (used space
//! only, so a smaller disk will do; for the disk Windows runs from, a copy
//! Windows starts from). Each shows what it would do, and only writes with
//! `--apply` and the target's name.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use datareco_clone::{
    CloneProgress, Cloner, Job, Prepared, Reading, estimate, plan_reading, prepare, runs_windows,
};
use datareco_io::{
    CopySource, DeviceList, Disk, DiskTarget, ImageTarget, SectorTarget, SourceSpec,
    check_disk_target, list_devices,
};
use datareco_partitions::edit::{
    DiskCopyOptions, default_backup_dir, plan_disk_copy, plan_partition_copy,
};
use datareco_partitions::{SegmentKind, TableKind, layout, read_table};

use crate::CliError;
use crate::format::format_size;
use crate::relocate::partition;

/// How long each step copies before showing progress.
const STEP: Duration = Duration::from_millis(500);

pub struct PartitionRequest<'a> {
    pub source: &'a str,
    pub partition: u32,
    pub to: &'a str,
    pub at_sector: Option<u64>,
    pub backup_dir: Option<&'a Path>,
    pub apply: Option<&'a str>,
}

pub struct MigrateRequest<'a> {
    pub source: &'a str,
    pub to: &'a str,
    /// Keep the disk's IDs (`Some(true)`), give the copy new ones
    /// (`Some(false)`), or decide: kept for the disk Windows runs from.
    pub keep_ids: Option<bool>,
    /// Leave the spare room after this partition.
    pub space_after: Option<u32>,
    pub apply: Option<&'a str>,
}

/// The disk or image file being copied onto.
struct Target {
    spec: SourceSpec,
    name: String,
    sectors: u64,
    sector_size: u32,
}

fn target(to: &str, source: &SourceSpec, devices: &DeviceList) -> Result<Target, CliError> {
    let spec = SourceSpec::parse(to);
    if spec == *source {
        return Err(CliError::Message(
            "That's the disk being copied: copy it onto another disk.".into(),
        ));
    }
    let (size, sector_size) = match &spec {
        SourceSpec::PhysicalDisk(number) => {
            let disk = devices
                .disks
                .iter()
                .find(|d| d.number == *number)
                .ok_or_else(|| CliError::Message(format!("There's no {}.", spec.display_name())))?;
            (disk.size, disk.sector_size)
        }
        SourceSpec::Image(path) => {
            let size = std::fs::metadata(path)
                .map_err(|e| CliError::Message(format!("{}: {e}", path.display())))?
                .len();
            (size, 512)
        }
        SourceSpec::Volume(_) => {
            return Err(CliError::Message(
                "Copy onto a disk (disk2) or an image file, not a volume.".into(),
            ));
        }
    };
    Ok(Target {
        name: spec.display_name(),
        spec,
        sectors: size / u64::from(sector_size),
        sector_size,
    })
}

/// What the person types to confirm: the disk's name, or the image's file name.
fn confirmation(spec: &SourceSpec) -> String {
    match spec {
        SourceSpec::PhysicalDisk(n) => format!("disk{n}"),
        _ => spec.display_name(),
    }
}

fn source_disk(text: &str) -> Result<(SourceSpec, Arc<Disk>), CliError> {
    let spec = SourceSpec::parse(text);
    if let SourceSpec::Volume(_) = spec {
        return Err(CliError::Message(
            "Give the disk (disk1); partitions are picked with -p.".into(),
        ));
    }
    let disk = Arc::new(Disk::open(&spec)?);
    Ok((spec, disk))
}

/// Opens the target for writing once `typed` matches: a disk after its
/// checks (it mustn't be the source, the disk Windows runs from, or hold
/// `keep` or the source image file), or an existing image file.
fn open_target(
    target: &Target,
    source: &SourceSpec,
    needed: u64,
    keep: &[&Path],
    typed: &str,
    devices: &DeviceList,
) -> Result<Box<dyn SectorTarget>, CliError> {
    match &target.spec {
        SourceSpec::PhysicalDisk(number) => {
            // Nor the disk holding the image file being copied.
            let mut keep = keep.to_vec();
            if let SourceSpec::Image(path) = source {
                keep.push(path);
            }
            let check = check_disk_target(
                *number,
                &CopySource {
                    spec: source,
                    size: needed,
                    sector_size: target.sector_size,
                },
                &keep,
                devices,
            )?;
            if !check.volumes.is_empty() {
                println!(
                    "Closing {} on {} while it copies.",
                    check.volumes.join(", "),
                    target.name
                );
            }
            Ok(Box::new(DiskTarget::open(&check, typed)?))
        }
        SourceSpec::Image(path) => {
            if !typed.trim().eq_ignore_ascii_case(&target.name) {
                return Err(CliError::Message(format!(
                    "Type --apply {} to confirm.",
                    target.name
                )));
            }
            Ok(Box::new(ImageTarget::resume(
                path,
                target.sectors * u64::from(target.sector_size),
            )?))
        }
        SourceSpec::Volume(_) => unreachable!("refused before"),
    }
}

fn show_reading(reading: &Reading) -> Result<(), CliError> {
    if let Some(problem) = &reading.problem {
        return Err(CliError::Message(problem.clone()));
    }
    if !reading.closed.is_empty() {
        println!("Closes {} while it copies.", reading.closed.join(", "));
    }
    if !reading.snapshots.is_empty() {
        println!(
            "Reads {} from a snapshot Windows takes, so it's copied as it was at one moment.",
            reading.snapshots.join(", ")
        );
    }
    if !reading.live.is_empty() {
        println!(
            "Reads {} as it is: Windows keeps using it.",
            reading.live.join(", ")
        );
    }
    Ok(())
}

fn time_for(bytes: u64) -> String {
    match (bytes >> 20) / 100 / 60 {
        0 => "under a minute".to_owned(),
        1 => "about a minute".to_owned(),
        n => format!("about {n} minutes"),
    }
}

/// Copies to the end, showing progress.
fn run(
    mut cloner: Cloner,
    prepared: &mut Prepared,
    target: &mut dyn SectorTarget,
) -> Result<CloneProgress, CliError> {
    loop {
        let progress = cloner
            .step(&mut prepared.sources, target, STEP)
            .map_err(|e| {
                eprintln!();
                CliError::Message(format!(
                    "The copy stopped: {e}. The source wasn't changed; run the same command \
                     again to start over."
                ))
            })?;
        #[allow(clippy::cast_precision_loss)] // for people
        let percent = progress.done as f64 * 100.0 / progress.total.max(1) as f64;
        eprint!(
            "\r  {}: {} of {} ({percent:.1}%), {}/s        ",
            progress.description,
            format_size(progress.done),
            format_size(progress.total),
            format_size(progress.bytes_per_second)
        );
        let _ = std::io::stderr().flush();
        if progress.finished {
            eprintln!();
            if progress.unreadable > 0 {
                println!(
                    "{} couldn't be read from the source; those parts are zeros on the copy.",
                    format_size(progress.unreadable)
                );
            }
            return Ok(progress);
        }
    }
}

pub fn clone_partition(request: &PartitionRequest<'_>) -> Result<(), CliError> {
    let (spec, source) = source_disk(request.source)?;
    let devices = list_devices().unwrap_or_default();
    let target = target(request.to, &spec, &devices)?;
    let target_disk = Disk::open(&target.spec)?;
    let p = partition(&source, request.source, request.partition)?;
    let at = match request.at_sector {
        Some(at) => at,
        None => free_place(&target_disk, p.sectors)?.ok_or_else(|| {
            CliError::Message(format!(
                "{} has no free space for partition {} (it needs {}).",
                target.name,
                p.number,
                format_size(p.sectors * u64::from(source.sector_size()))
            ))
        })?,
    };
    let copy = plan_partition_copy(&source, p.start_lba, &target_disk, at)?;
    if let Some(reason) = &copy.plan.blocked {
        return Err(CliError::Message(reason.clone()));
    }
    if let Some(error) = &copy.plan.error {
        return Err(CliError::Message(error.message.clone()));
    }
    let part = copy
        .part
        .clone()
        .ok_or_else(|| CliError::Message("There's nothing to copy.".into()))?;
    println!("{}.", copy.plan.descriptions().join("; "));
    let bytes = estimate(&source, std::slice::from_ref(&part));
    println!(
        "Copies {} (its used space, of {}; at 100 MiB/s {}), then adds it to {}'s partition \
         table. The table is backed up first, so Undo can take the copy off again.",
        format_size(bytes),
        format_size(part.sectors * u64::from(source.sector_size())),
        time_for(bytes),
        target.name
    );
    show_reading(&plan_reading(
        &spec,
        &source,
        std::slice::from_ref(&part),
        &devices,
    ))?;
    let Some(typed) = request.apply else {
        println!(
            "Nothing was written. To copy it, run the same command with --apply {}",
            confirmation(&target.spec)
        );
        return Ok(());
    };
    let backup_dir = request
        .backup_dir
        .map_or_else(default_backup_dir, Path::to_path_buf);
    let mut prepared = prepare(&spec, &source, std::slice::from_ref(&part), &devices)
        .map_err(|e| CliError::Message(e.to_string()))?;
    let mut out = open_target(
        &target,
        &spec,
        part.sectors * u64::from(source.sector_size()),
        &[backup_dir.as_path()],
        typed,
        &devices,
    )?;
    let used = prepared
        .allocations(std::slice::from_ref(&part), source.sector_size())
        .pop()
        .flatten();
    // Read the target's table sectors afresh, now nothing else changes it.
    let fresh = Disk::open(&target.spec)?;
    let job = Job::partition(&copy, &fresh, &target.spec.id(), &backup_dir, used.as_ref())
        .map_err(|e| CliError::Message(e.to_string()))?;
    let progress = run(Cloner::new(job), &mut prepared, out.as_mut())?;
    drop(out);
    println!(
        "Copied. {}'s old partition table is saved in {}; Undo (datareco table {} undo) takes the \
         copy off the table again.",
        target.name,
        progress
            .backup
            .as_ref()
            .map_or_else(String::new, |b| b.display().to_string()),
        confirmation(&target.spec)
    );
    Ok(())
}

/// The first free space on `disk` that holds `sectors`, from a 1 MiB
/// boundary.
fn free_place(disk: &Disk, sectors: u64) -> Result<Option<u64>, CliError> {
    let table = read_table(disk)?;
    let align = (1 << 20) / u64::from(disk.sector_size()).max(1);
    Ok(layout(&table)
        .iter()
        .filter(|s| s.kind == SegmentKind::Free)
        .find_map(|s| {
            let start = s.start_lba.div_ceil(align) * align;
            (start + sectors <= s.end_lba()).then_some(start)
        }))
}

pub fn migrate(request: &MigrateRequest<'_>) -> Result<(), CliError> {
    let (spec, source) = source_disk(request.source)?;
    let devices = list_devices().unwrap_or_default();
    let target = target(request.to, &spec, &devices)?;
    let windows = runs_windows(&spec, &devices);
    let table = read_table(&source)?;
    let space_after = match request.space_after {
        Some(number) => Some(partition(&source, request.source, number)?.start_lba),
        None => None,
    };
    let options = DiskCopyOptions {
        keep_ids: request.keep_ids.unwrap_or(windows),
        // Windows on an MBR disk finds its partitions by where they start.
        same_places: windows && table.kind == TableKind::Mbr,
        space_after,
    };
    let copy = plan_disk_copy(
        &source,
        &target.name,
        target.sectors,
        target.sector_size,
        &options,
    )?;
    if let Some(reason) = copy.blocked.as_ref().or(copy.problem.as_ref()) {
        return Err(CliError::Message(reason.clone()));
    }
    println!(
        "Copy {}'s partitions onto {}{}:",
        source.name(),
        target.name,
        if windows {
            ", which Windows can then start from"
        } else {
            ""
        }
    );
    for part in &copy.parts {
        let place = if part.from_lba == part.to_lba {
            "same place".to_owned()
        } else {
            format!("to sector {}", part.to_lba)
        };
        println!("  {}: {place}", part.what);
    }
    for warning in &copy.warnings {
        println!("{warning}");
    }
    let bytes = estimate(&source, &copy.parts);
    println!(
        "Copies {} (used space only; at 100 MiB/s {}). Everything on {} is erased.",
        format_size(bytes),
        time_for(bytes),
        target.name
    );
    if options.keep_ids {
        println!(
            "The copy keeps {}'s disk IDs{}, so while both disks are connected Windows keeps one \
             of them offline.",
            source.name(),
            if windows {
                " (Windows needs them to start from it)"
            } else {
                ""
            }
        );
    }
    show_reading(&plan_reading(&spec, &source, &copy.parts, &devices))?;
    let Some(typed) = request.apply else {
        println!(
            "Nothing was written. To copy it, run the same command with --apply {}",
            confirmation(&target.spec)
        );
        return Ok(());
    };
    let mut prepared = prepare(&spec, &source, &copy.parts, &devices)
        .map_err(|e| CliError::Message(e.to_string()))?;
    let needed = copy.needed_sectors * u64::from(target.sector_size);
    let mut out = open_target(&target, &spec, needed, &[], typed, &devices)?;
    let allocations = prepared.allocations(&copy.parts, source.sector_size());
    let job = Job::disk(
        &copy,
        &allocations,
        source.sector_size(),
        format!("Copy {} onto {}", source.name(), target.name),
    )
    .map_err(|e| CliError::Message(e.to_string()))?;
    println!(
        "If it stops, run it again: {} isn't changed.",
        source.name()
    );
    run(Cloner::new(job), &mut prepared, out.as_mut())?;
    drop(out);
    drop(prepared);
    if windows {
        println!(
            "Copied. To start Windows from {0}, turn the computer off, then start it from {0}: \
             swap the disks, or pick it in the computer's boot menu.",
            target.name
        );
    } else {
        println!("Copied.");
    }
    Ok(())
}
