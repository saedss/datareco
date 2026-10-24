//! `format`, `wipe`, `label` and `letter`. Formatting and wiping show what
//! they'd do and only write with `--apply` and the disk's name.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use datareco_format::{
    FormatKind, FormatOptions, Place, Spot, Wipe, WipePattern, locate, plan_format, write_format,
};
use datareco_io::manage::{format_with_windows, set_drive_letter, set_volume_label};
use datareco_io::{DeviceList, Disk, SourceSpec, VolumeInfo, list_devices};
use datareco_partitions::{PartitionKind, TableKind, read_table};

use crate::CliError;
use crate::format::format_size;

/// How long each wipe step writes before showing progress.
const STEP: Duration = Duration::from_millis(500);

pub struct FormatRequest<'a> {
    pub source: &'a str,
    pub partition: Option<u32>,
    pub file_system: &'a str,
    pub label: Option<&'a str>,
    pub cluster: Option<&'a str>,
    pub full: bool,
    pub apply: Option<&'a str>,
}

/// Why Windows won't show a partition once it's formatted, if it won't: it's
/// hidden, or its type isn't a FAT or NTFS one (MBR) or basic data (GPT).
fn type_note(disk: &Disk, source: &str, number: u32) -> Option<String> {
    let table = read_table(disk).ok()?;
    let p = table.partitions.iter().find(|p| p.number == number)?;
    let windows_type = ["0x01", "0x04", "0x06", "0x07", "0x0B", "0x0C", "0x0E"]
        .contains(&p.type_code.as_str())
        || p.type_code == "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
    if p.hidden {
        Some(format!(
            "partition {number} is hidden, so Windows won't show it until it's unhidden \
             (datareco table {source} unhide {number})."
        ))
    } else if !windows_type {
        Some(format!(
            "partition {number}'s type isn't one Windows uses, so Windows won't show it until \
             it's changed (datareco table {source} type {number} ntfs)."
        ))
    } else {
        None
    }
}

/// The partition (first sector, sectors) a request names, and how to name
/// it for people; `None` for the whole source, which needs to have no
/// partition table.
fn range(
    disk: &Disk,
    source: &str,
    partition: Option<u32>,
) -> Result<(Option<(u64, u64)>, String), CliError> {
    let table = read_table(disk)?;
    match partition {
        Some(number) => {
            let p = table
                .partitions
                .iter()
                .find(|p| p.number == number && p.kind != PartitionKind::Extended)
                .ok_or_else(|| {
                    CliError::Message(format!(
                        "There's no partition {number}; `datareco partitions {source}` lists them."
                    ))
                })?;
            let fs = p
                .file_system
                .as_ref()
                .map_or_else(String::new, |f| format!(", {}", f.name));
            Ok((
                Some((p.start_lba, p.sectors)),
                format!(
                    "partition {number} ({}{fs})",
                    format_size(p.sectors * u64::from(disk.sector_size()))
                ),
            ))
        }
        None if table.kind != TableKind::None => Err(CliError::Message(format!(
            "{} has a partition table: pick a partition with -p (`datareco partitions {source}` \
             lists them).",
            disk.name()
        ))),
        None => Ok((None, format!("all of {}", disk.name()))),
    }
}

fn parse_cluster(text: &str) -> Result<u32, CliError> {
    let lower = text.trim().to_ascii_lowercase();
    let (number, shift) = if let Some(n) = lower
        .strip_suffix("kib")
        .or_else(|| lower.strip_suffix('k'))
    {
        (n, 10)
    } else if let Some(n) = lower
        .strip_suffix("mib")
        .or_else(|| lower.strip_suffix('m'))
    {
        (n, 20)
    } else {
        (lower.as_str(), 0)
    };
    number
        .trim()
        .parse::<u32>()
        .ok()
        .and_then(|n| n.checked_mul(1 << shift))
        .ok_or_else(|| {
            CliError::Message(format!(
                "\"{text}\" isn't a cluster size: use 4K, 32K, 128K …"
            ))
        })
}

pub fn format(request: &FormatRequest<'_>) -> Result<(), CliError> {
    let spec = SourceSpec::parse(request.source);
    let disk = Arc::new(Disk::open(&spec)?);
    let (range, what) = range(&disk, request.source, request.partition)?;
    let devices = list_devices().ok();
    let spot = locate(&spec, &disk, range, devices.as_ref())?;
    if let (SourceSpec::PhysicalDisk(_), Some(number)) = (&spec, request.partition)
        && let Some(note) = type_note(&disk, request.source, number)
    {
        println!("note: {note}");
    }
    let kind = match request.file_system.to_ascii_lowercase().as_str() {
        "fat32" => Some(FormatKind::Fat32),
        "exfat" => Some(FormatKind::Exfat),
        "ntfs" => None,
        other => {
            return Err(CliError::Message(format!(
                "\"{other}\" isn't a file system DataReco formats: use fat32, exfat or ntfs."
            )));
        }
    };
    match kind {
        Some(kind) => {
            let options = FormatOptions {
                kind,
                label: request.label.map(str::to_owned),
                cluster_size: request.cluster.map(parse_cluster).transpose()?,
                serial: None,
                partition_offset: spot.start_lba,
            };
            let plan =
                plan_format(spot.size, spot.sector_size, &options).map_err(CliError::Message)?;
            let s = &plan.summary;
            println!(
                "Format {what} as {}{}: clusters of {}, {} for files.",
                kind.name(),
                s.label
                    .as_ref()
                    .map_or_else(String::new, |l| format!(" \"{l}\"")),
                format_size(u64::from(s.cluster_size)),
                format_size(s.capacity)
            );
            println!(
                "Everything on it is erased (a scan may still find old files until they're overwritten)."
            );
            if request.full {
                println!("note: --full is for NTFS; FAT32 and exFAT are formatted quickly.");
            }
            let Some(typed) = confirm(&spot, request.apply)? else {
                return Ok(());
            };
            let mut target = spot.open(typed)?;
            write_format(&plan, target.as_mut(), spot.offset)
                .map_err(|e| CliError::Message(format!("Writing failed: {e}")))?;
            drop(target);
            // Read back with DataReco's own reader.
            let fresh = Arc::new(Disk::open(&spec)?);
            let start = spot.start_lba * u64::from(spot.sector_size);
            let fs = datareco_fs::open(fresh, start).map_err(|e| {
                CliError::Message(format!("The new file system doesn't read back: {e}"))
            })?;
            println!(
                "Formatted: {} {}.",
                fs.info().name,
                fs.info()
                    .label
                    .as_deref()
                    .map_or(String::new(), |l| format!("\"{l}\""))
            );
        }
        None => {
            if let Place::Image(_) = spot.place {
                return Err(CliError::Message(
                    "Windows formats NTFS only on disks. For an image file, use exfat or fat32."
                        .into(),
                ));
            }
            let Place::Volume(check) = &spot.place else {
                return Err(CliError::Message(
                    "NTFS is formatted by Windows, which needs a volume it has mounted: a \
                     partition of a type it uses (ntfs, or basic data on GPT), not hidden. \
                     FAT32 and exFAT work anywhere."
                        .into(),
                ));
            };
            println!(
                "Format {what} as NTFS{} with Windows' formatter ({} format).",
                request
                    .label
                    .map_or_else(String::new, |l| format!(" \"{l}\"")),
                if request.full {
                    "full: every sector is read and checked"
                } else {
                    "quick"
                }
            );
            println!(
                "Everything on it is erased (a scan may still find old files until they're overwritten)."
            );
            let Some(typed) = confirm(&spot, request.apply)? else {
                return Ok(());
            };
            if !typed.trim().eq_ignore_ascii_case(&spot.confirmation) {
                return Err(CliError::Message(format!(
                    "Type --apply {} to confirm that everything on {} may be erased.",
                    spot.confirmation, check.name
                )));
            }
            format_with_windows(&check.path, "NTFS", request.label, !request.full)?;
            println!("Formatted as NTFS.");
        }
    }
    Ok(())
}

/// Shows what's closed while writing, then the typed confirmation, or
/// says how to give it.
fn confirm<'a>(spot: &Spot, apply: Option<&'a str>) -> Result<Option<&'a str>, CliError> {
    if !spot.closing.is_empty() {
        println!("Closed while it's written: {}.", spot.closing.join(", "));
    }
    match apply {
        Some(typed) => Ok(Some(typed)),
        None => {
            println!(
                "Nothing was written. To do it, run the same command with --apply {}",
                spot.confirmation
            );
            Ok(None)
        }
    }
}

pub fn wipe(
    source: &str,
    partition: Option<u32>,
    random: bool,
    apply: Option<&str>,
) -> Result<(), CliError> {
    let spec = SourceSpec::parse(source);
    let disk = Disk::open(&spec)?;
    let table = read_table(&disk)?;
    let (range, what) = match partition {
        Some(_) => range(&disk, source, partition)?,
        None => (None, format!("all of {}", disk.name())),
    };
    let devices = list_devices().ok();
    let spot = locate(&spec, &disk, range, devices.as_ref())?;
    println!(
        "Wipe {what}: write {} over its {}.",
        if random { "random bytes" } else { "zeros" },
        format_size(spot.size)
    );
    if partition.is_none() && table.kind != TableKind::None {
        println!("This erases the partition table too: the disk will show as empty.");
    }
    println!(
        "Nothing on it can be recovered afterwards, by DataReco or anything else. (On SSDs and \
         USB sticks, spare blocks the computer can't reach may keep old data.)"
    );
    let Some(typed) = confirm(&spot, apply)? else {
        return Ok(());
    };
    let mut target = spot.open(typed)?;
    let pattern = if random {
        WipePattern::Random
    } else {
        WipePattern::Zeros
    };
    let mut wipe = Wipe::new(spot.offset, spot.offset + spot.size, pattern);
    loop {
        let progress = wipe
            .step(target.as_mut(), STEP)
            .map_err(|e| CliError::Message(format!("Writing failed: {e}")))?;
        #[allow(clippy::cast_precision_loss)] // for people
        let percent = progress.done as f64 * 100.0 / progress.total.max(1) as f64;
        eprint!(
            "\r  {} of {} written ({percent:.1}%), {}/s        ",
            format_size(progress.done),
            format_size(progress.total),
            format_size(progress.bytes_per_second)
        );
        let _ = std::io::stderr().flush();
        if progress.finished {
            break;
        }
    }
    eprintln!();
    println!("Wiped.");
    Ok(())
}

/// The Windows volume a source names: a drive letter or volume path, or a
/// disk's partition with `-p`.
fn volume<'a>(
    spec: &SourceSpec,
    partition: Option<u32>,
    devices: &'a DeviceList,
) -> Result<&'a VolumeInfo, CliError> {
    match (spec, partition) {
        (SourceSpec::Volume(name), _) => devices
            .volume(name)
            .ok_or_else(|| CliError::Message(format!("{} wasn't found.", spec.display_name()))),
        (SourceSpec::PhysicalDisk(number), Some(p)) => {
            let disk = Disk::open(spec)?;
            let table = read_table(&disk)?;
            let part = table
                .partitions
                .iter()
                .find(|x| x.number == p)
                .ok_or_else(|| CliError::Message(format!("There's no partition {p}.")))?;
            let offset = part.start_lba * u64::from(disk.sector_size());
            devices
                .volumes
                .iter()
                .find(|v| v.extents.iter().any(|e| e.disk == *number && e.offset == offset))
                .ok_or_else(|| {
                    CliError::Message(format!(
                        "Windows has no volume for partition {p} (a type it doesn't use, or hidden)."
                    ))
                })
        }
        _ => Err(CliError::Message(
            "Give a volume (E:), or a disk and its partition (disk1 -p 2).".into(),
        )),
    }
}

pub fn label(source: &str, partition: Option<u32>, label: Option<&str>) -> Result<(), CliError> {
    let devices = list_devices()?;
    let volume = volume(&SourceSpec::parse(source), partition, &devices)?;
    set_volume_label(&volume.path, label)?;
    match label.map(str::trim).filter(|l| !l.is_empty()) {
        Some(label) => println!("Labelled \"{label}\"."),
        None => println!("Label removed."),
    }
    Ok(())
}

pub fn letter(source: &str, partition: Option<u32>, letter: &str) -> Result<(), CliError> {
    let devices = list_devices()?;
    let volume = volume(&SourceSpec::parse(source), partition, &devices)?;
    let wanted = match letter
        .trim()
        .trim_end_matches(':')
        .to_ascii_lowercase()
        .as_str()
    {
        "none" | "" => None,
        l if l.len() == 1 => l.chars().next(),
        _ => {
            return Err(CliError::Message(format!(
                "\"{letter}\" isn't a drive letter: use one letter (F), or none."
            )));
        }
    };
    set_drive_letter(&volume.path, &volume.mount_points, wanted)?;
    match wanted {
        Some(l) => println!("It's {}: now.", l.to_ascii_uppercase()),
        None => println!("It has no drive letter now."),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_sizes() {
        assert_eq!(parse_cluster("32K").unwrap(), 32 * 1024);
        assert_eq!(parse_cluster("128KiB").unwrap(), 128 * 1024);
        assert_eq!(parse_cluster("1M").unwrap(), 1 << 20);
        assert_eq!(parse_cluster("4096").unwrap(), 4096);
        assert!(parse_cluster("big").is_err());
    }
}
