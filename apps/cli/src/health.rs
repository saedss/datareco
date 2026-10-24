//! `map`, `health`, `surface` and `speed`: the disk map and health checks.
//! All of them only read.

use std::io::Write;
use std::time::Duration;

use datareco_health::{
    AttributeState, HealthReport, Level, SURFACE_BLOCK_BYTES, SpeedTest, SurfaceProgress,
    SurfaceTest, Verdict, assess,
};
use datareco_io::{Disk, FileSource, SectorSource, SourceSpec, list_devices, read_health_data};
use datareco_partitions::{DiskMap, SegmentKind, TableKind, disk_map, read_table};

use crate::CliError;
use crate::format::format_size;
use crate::partitions::file_system_text;

/// How long each step reads before showing progress.
const STEP: Duration = Duration::from_millis(500);

pub fn map(source: &str) -> Result<(), CliError> {
    let spec = SourceSpec::parse(source);
    let disk = Disk::open(&spec)?;
    let devices = list_devices().ok();
    let map = disk_map(&disk, &spec, devices.as_ref())?;
    print_map(disk.name(), &map);
    Ok(())
}

pub(crate) fn print_map(name: &str, map: &DiskMap) {
    let table = &map.table;
    let sector = u64::from(table.sector_size);
    let kind = match table.kind {
        TableKind::Mbr => "MBR",
        TableKind::Gpt => "GPT",
        TableKind::None => "no partition table",
    };
    println!(
        "{name}: {kind}, {} ({} sectors of {} bytes)",
        format_size(table.disk_sectors * sector),
        table.disk_sectors,
        table.sector_size
    );
    println!();
    println!(
        "  {:>3}  {:>13}  {:>10}  What's there",
        "#", "Start LBA", "Size"
    );
    for segment in &map.segments {
        let number = segment
            .partition
            .map_or_else(|| "-".to_owned(), |n| n.to_string());
        let partition = segment
            .partition
            .and_then(|n| table.partitions.iter().find(|p| p.number == n));
        let what = match segment.kind {
            SegmentKind::Partition => partition.map_or_else(String::new, |p| {
                let fs = p
                    .file_system
                    .as_ref()
                    .map(|fs| format!(", {}", file_system_text(Some(fs))))
                    .unwrap_or_default();
                format!("{}{fs}", p.type_name)
            }),
            SegmentKind::Free if segment.logical => {
                "Free space (in the extended partition)".to_owned()
            }
            SegmentKind::Free => "Free space".to_owned(),
            SegmentKind::Unusable => "Unusable space".to_owned(),
            SegmentKind::WholeDisk => format!(
                "The whole disk is one volume: {}",
                file_system_text(table.whole_disk.as_ref())
            ),
        };
        let indent = if segment.logical { "  " } else { "" };
        let volume = segment
            .volume
            .as_ref()
            .map(|v| {
                let place = v
                    .mount_points
                    .first()
                    .map_or("mounted, no letter", |m| m.trim_end_matches('\\'));
                match v.free {
                    Some(free) => format!("  [{place}, {} free]", format_size(free)),
                    None => format!("  [{place}]"),
                }
            })
            .unwrap_or_default();
        println!(
            "  {number:>3}  {:>13}  {:>10}  {indent}{what}{volume}",
            segment.start_lba,
            format_size(segment.sectors * sector)
        );
        if let Some(note) = &segment.note {
            println!("  {:>31}  {indent}{note}", "");
        }
    }
    for problem in &table.problems {
        println!();
        println!("  {:?}: {}", problem.severity, problem.message);
    }
}

pub fn health(source: &str, json: bool) -> Result<(), CliError> {
    let number = disk_number(source)?;
    let report = assess(&read_health_data(number)?);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).expect("health reports serialize")
        );
        return Ok(());
    }
    print_health(&SourceSpec::PhysicalDisk(number).display_name(), &report);
    Ok(())
}

/// The physical disk a source is: itself, or the one disk under a volume.
fn disk_number(source: &str) -> Result<u32, CliError> {
    let spec = SourceSpec::parse(source);
    match &spec {
        SourceSpec::PhysicalDisk(number) => Ok(*number),
        SourceSpec::Image(_) => Err(CliError::Message(
            "Health data comes from drives; image files have none. Give a disk (disk1) or a \
             volume on it (E:)."
                .to_owned(),
        )),
        SourceSpec::Volume(_) => match list_devices()?.disks_of_source(&spec)[..] {
            [number] => Ok(number),
            [] => Err(CliError::Message(format!(
                "{} isn't on a disk DataReco can see.",
                spec.display_name()
            ))),
            _ => Err(CliError::Message(format!(
                "{} spans several disks; ask about each disk (datareco disks lists them).",
                spec.display_name()
            ))),
        },
    }
}

fn print_health(name: &str, report: &HealthReport) {
    let verdict = match report.verdict {
        Verdict::Good => "Good",
        Verdict::Caution => "Caution",
        Verdict::Bad => "Bad",
        Verdict::Unknown => "Unknown",
    };
    println!("{name}: {verdict}");
    if let Some(source) = &report.source {
        println!("  (from {source})");
    }
    println!("{}", report.summary);
    if !report.findings.is_empty() {
        println!();
        for finding in &report.findings {
            let level = match finding.level {
                Level::Bad => "bad",
                Level::Caution => "caution",
                Level::Info => "note",
            };
            println!("  {level:>7}: {}", finding.message);
        }
    }
    let mut facts = Vec::new();
    if let Some(t) = report.temperature {
        facts.push(format!("Temperature: {t} °C"));
    }
    if let Some(hours) = report.power_on_hours {
        facts.push(format!(
            "Powered on: {hours} hours ({:.1} years)",
            hours as f64 / 8766.0
        ));
    }
    if let Some(cycles) = report.power_cycles {
        facts.push(format!("Power cycles: {cycles}"));
    }
    if let Some(used) = report.life_used {
        facts.push(format!("Rated life used: {used}%"));
    }
    if let Some(bytes) = report.bytes_written {
        facts.push(format!("Written: {}", format_size(bytes)));
    }
    if let Some(bytes) = report.bytes_read {
        facts.push(format!("Read: {}", format_size(bytes)));
    }
    match (report.solid_state, report.rpm) {
        (Some(true), _) => facts.push("Solid-state drive".to_owned()),
        (_, Some(rpm)) => facts.push(format!("Spins at {rpm} rpm")),
        _ => {}
    }
    if !facts.is_empty() {
        println!();
        for fact in facts {
            println!("  {fact}");
        }
    }
    if !report.attributes.is_empty() {
        println!();
        println!(
            "  {:>3}  {:<32} {:>5} {:>5} {:>9}  {:>14}",
            "ID", "Attribute", "Value", "Worst", "Threshold", "Raw"
        );
        for a in &report.attributes {
            let threshold = a
                .threshold
                .map_or_else(|| "-".to_owned(), |t| t.to_string());
            let mark = match a.state {
                AttributeState::Ok => "",
                AttributeState::Watch => "  <- watch",
                AttributeState::FailedBefore => "  <- failed before",
                AttributeState::Failing => "  <- FAILING",
            };
            println!(
                "  {:>3}  {:<32} {:>5} {:>5} {threshold:>9}  {:>14}{mark}",
                a.id, a.name, a.current, a.worst, a.raw
            );
        }
    }
    if let Some(nvme) = &report.nvme {
        println!();
        println!(
            "  Available spare: {}% (warns below {}%)",
            nvme.available_spare, nvme.spare_threshold
        );
        println!("  Unsafe shutdowns: {}", nvme.unsafe_shutdowns);
        println!("  Media and data integrity errors: {}", nvme.media_errors);
        println!("  Error log entries: {}", nvme.error_log_entries);
        println!("  Critical warning bits: {:#04x}", nvme.critical_warning);
    }
}

/// Opens a source for reading straight through, without the cache, so read
/// times are the disk's own.
fn open_direct(spec: &SourceSpec) -> Result<FileSource, CliError> {
    Ok(FileSource::open(spec)?)
}

pub fn surface(source: &str, partition: Option<u32>) -> Result<(), CliError> {
    let spec = SourceSpec::parse(source);
    let mut device = open_direct(&spec)?;
    let mut test = match partition {
        None => SurfaceTest::whole(&device),
        Some(number) => {
            let table = read_table(&Disk::open(&spec)?)?;
            let p = table
                .partitions
                .iter()
                .find(|p| p.number == number)
                .ok_or_else(|| {
                    CliError::Message(format!(
                        "There's no partition {number}; `datareco partitions {source}` lists them."
                    ))
                })?;
            SurfaceTest::new(device.sector_size(), p.start_lba, p.sectors)
        }
    };
    let sector = u64::from(device.sector_size());
    eprintln!(
        "Reading every sector of {} once and timing it. Nothing is written.",
        spec.display_name()
    );
    let progress = loop {
        let progress = test.step(&mut device, STEP);
        eprint!(
            "\r  {} of {} tested ({:.1}%), {} slow, {} unreadable, {}/s        ",
            format_size(progress.tested * sector),
            format_size(progress.sectors * sector),
            percent(progress.tested, progress.sectors),
            progress.counts.slow + progress.counts.very_slow,
            progress.counts.unreadable,
            format_size(progress.bytes_per_second),
        );
        let _ = std::io::stderr().flush();
        if progress.done {
            break progress;
        }
    };
    eprintln!();
    print_surface(&progress, sector);
    Ok(())
}

fn percent(part: u64, whole: u64) -> f64 {
    #[allow(clippy::cast_precision_loss)] // for people
    if whole == 0 {
        100.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

fn print_surface(progress: &SurfaceProgress, sector: u64) {
    if let Some(reason) = &progress.stopped {
        println!("Stopped: {reason}");
    }
    let c = &progress.counts;
    println!(
        "Blocks of {}: {} fast (under 20 ms), {} normal (under 100 ms), {} slow (under 500 ms), \
         {} very slow, {} unreadable.",
        format_size(SURFACE_BLOCK_BYTES),
        c.fast,
        c.normal,
        c.slow,
        c.very_slow,
        c.unreadable
    );
    let line: String = progress
        .picture
        .chars()
        .collect::<Vec<_>>()
        .chunks(10)
        .map(|square| {
            let worst = square
                .iter()
                .map(|&symbol| match symbol {
                    'x' => 5,
                    'v' => 4,
                    's' => 3,
                    'n' => 2,
                    'f' => 1,
                    _ => 0,
                })
                .max()
                .unwrap_or(0);
            ['.', 'f', 'n', 's', 'v', 'x'][worst]
        })
        .collect();
    println!();
    for row in line.as_bytes().chunks(50) {
        println!("  {}", String::from_utf8_lossy(row));
    }
    println!("  (f fast, n normal, s slow, v very slow, x unreadable, . not tested)");
    if progress.bad_range_count > 0 {
        println!();
        println!(
            "{} unreadable ({} sectors) in {} places:",
            format_size(progress.unreadable_sectors * sector),
            progress.unreadable_sectors,
            progress.bad_range_count
        );
        for range in &progress.bad_ranges {
            println!(
                "  sectors {}–{} ({})",
                range.start_lba,
                range.start_lba + range.sectors - 1,
                format_size(range.sectors * sector)
            );
        }
        if progress.bad_range_count > progress.bad_ranges.len() {
            println!(
                "  … and {} more",
                progress.bad_range_count - progress.bad_ranges.len()
            );
        }
        println!();
        println!(
            "Copy this disk before recovering from it (datareco image), and recover from the copy."
        );
    }
}

pub fn speed(source: &str) -> Result<(), CliError> {
    let spec = SourceSpec::parse(source);
    if matches!(spec, SourceSpec::Image(_)) {
        eprintln!(
            "note: image files are read through Windows' file cache, so this measures the file, \
             not a disk"
        );
    }
    let mut device = open_direct(&spec)?;
    let mut test = SpeedTest::new(&device);
    eprintln!(
        "Reading {} for about 12 seconds. Nothing is written.",
        spec.display_name()
    );
    let status = loop {
        let status = test.step(&mut device, STEP);
        eprint!("\r  {}%   ", status.percent);
        let _ = std::io::stderr().flush();
        if status.done {
            break status;
        }
    };
    eprintln!();
    if let Some(reason) = &status.stopped {
        println!("Stopped: {reason}");
    }
    println!("Reading 1 MiB at a time:");
    for result in &status.sequential {
        println!(
            "  {:<7} {:>12}/s",
            result.place,
            format_size(result.bytes_per_second)
        );
    }
    if let Some(random) = &status.random {
        #[allow(clippy::cast_precision_loss)] // for people
        let each = if random.average_micros < 100 {
            format!("{} µs", random.average_micros)
        } else {
            format!("{:.2} ms", random.average_micros as f64 / 1000.0)
        };
        println!(
            "Reading 4 KiB at random places: {} reads/s ({}/s), {each} each",
            random.reads_per_second,
            format_size(random.bytes_per_second)
        );
    }
    if status.errors > 0 {
        println!(
            "{} reads failed: run a surface test to see where.",
            status.errors
        );
    }
    println!("(One request at a time; SSD makers quote speeds with many at once.)");
    Ok(())
}
