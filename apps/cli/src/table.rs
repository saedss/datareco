//! `table`: changing a partition table. Every change is shown first; it's
//! written only with `--apply` and the disk's name, after the sectors it
//! replaces are saved to a backup that `table … undo` puts back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Subcommand;
use datareco_io::{Disk, Error};
use datareco_io::{
    DiskTarget, ImageTarget, SectorTarget, SourceSpec, check_disk_for_table_changes, list_devices,
};
use datareco_partitions::edit::{
    Change, Operation, Plan, Style, TableBackup, apply_writes, default_backup_dir, list_backups,
    next_to_undo, plan, preview, read_backup, type_choices, type_for_file_system, undo_writes,
    verify_writes,
};
use datareco_partitions::{SegmentKind, disk_map, identify, layout, read_table};

use crate::CliError;
use crate::format::{format_size, format_time};
use crate::health::print_map;

#[derive(Subcommand)]
pub enum TableAction {
    /// Give a disk with no partition table an empty one.
    Init {
        /// mbr or gpt.
        #[arg(value_parser = parse_style)]
        style: Style,
    },
    /// Make a new (unformatted) partition in free space.
    Create {
        /// Its size: 500MiB, 20GiB, 1.5TiB … or "max" for all of the free space.
        #[arg(long, short)]
        size: String,
        /// Its first sector (default: the start of the first free space it fits, on a 1 MiB boundary).
        #[arg(long)]
        start: Option<u64>,
        /// Its type: ntfs, fat32, linux, efi … (`datareco table x types` lists them) or a code.
        #[arg(long, short = 't', default_value = "ntfs")]
        r#type: String,
        /// MBR: a logical partition, in the extended partition (made if there's none).
        #[arg(long)]
        logical: bool,
        /// GPT: its name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Remove a partition from the table (its data stays on the disk).
    Delete {
        /// Its number in `datareco partitions`.
        partition: u32,
    },
    /// MBR: make it the active (boot) partition. GPT: mark it legacy BIOS bootable.
    Active {
        partition: u32,
        /// Unmark it instead.
        #[arg(long)]
        off: bool,
    },
    /// Hide a partition from Windows (no drive letter).
    Hide { partition: u32 },
    /// Show a hidden partition again.
    Unhide { partition: u32 },
    /// Change a partition's type (its data isn't changed).
    Type {
        partition: u32,
        /// ntfs, fat32, linux, efi … or a code (0x07, or a GPT type GUID).
        r#type: String,
    },
    /// GPT: name a partition.
    Name { partition: u32, name: String },
    /// Rewrite the table as MBR or GPT, keeping every partition (data disks).
    Convert {
        #[arg(value_parser = parse_style)]
        style: Style,
    },
    /// MBR: make a primary partition next to the extended one logical.
    Logical { partition: u32 },
    /// MBR: make the first or last logical partition primary.
    Primary { partition: u32 },
    /// Put a lost partition back in the table (see `datareco find-partitions`).
    Restore {
        /// Its first sector.
        #[arg(long)]
        start: u64,
        /// Its size in sectors.
        #[arg(long)]
        sectors: u64,
        /// Its type (default: from the file system found there).
        #[arg(long, short = 't')]
        r#type: Option<String>,
    },
    /// Put back the table as it was before a change, from its backup.
    Undo {
        /// The backup to undo (default: the newest for this disk).
        #[arg(long)]
        backup: Option<PathBuf>,
    },
    /// List the backups of this disk's table changes.
    Backups,
    /// List the partition types `create` and `type` know by name.
    Types,
}

pub struct TableRequest<'a> {
    pub source: &'a str,
    pub action: TableAction,
    pub apply: Option<&'a str>,
    pub backup_dir: Option<&'a Path>,
}

fn parse_style(text: &str) -> Result<Style, String> {
    match text.to_ascii_lowercase().as_str() {
        "mbr" => Ok(Style::Mbr),
        "gpt" => Ok(Style::Gpt),
        _ => Err("use mbr or gpt".to_owned()),
    }
}

pub fn table(request: TableRequest<'_>) -> Result<(), CliError> {
    let spec = SourceSpec::parse(request.source);
    if let SourceSpec::Volume(_) = spec {
        return Err(CliError::Message(
            "Partition tables belong to disks: give the disk (disk1), not a volume on it.".into(),
        ));
    }
    let backup_dir = request
        .backup_dir
        .map_or_else(default_backup_dir, Path::to_path_buf);
    let disk = Arc::new(Disk::open(&spec)?);
    let op = match request.action {
        TableAction::Types => {
            for style in [Style::Mbr, Style::Gpt] {
                println!("{}:", style.name());
                for choice in type_choices(style) {
                    println!("  {:<12} {}", choice.code, choice.name);
                }
            }
            println!("Or a code: 0x07 on MBR disks, a type GUID on GPT disks.");
            return Ok(());
        }
        TableAction::Backups => {
            let found = list_backups(&backup_dir, &spec.id());
            if found.is_empty() {
                println!(
                    "No backups of {}'s table in {}.",
                    disk.name(),
                    backup_dir.display()
                );
            }
            for backup in found {
                println!(
                    "{}  {}",
                    format_time(i64::try_from(backup.created / 1000).unwrap_or(0)),
                    backup.path.display()
                );
                for change in &backup.changes {
                    println!("    {change}");
                }
            }
            return Ok(());
        }
        TableAction::Undo { backup } => {
            return undo(&disk, &spec, backup.as_deref(), request.apply, &backup_dir);
        }
        action => operation(&disk, action)?,
    };
    let plan = plan(&disk, std::slice::from_ref(&op))?;
    if let Some(reason) = &plan.blocked {
        return Err(CliError::Message(reason.clone()));
    }
    if let Some(error) = &plan.error {
        return Err(CliError::Message(error.message.clone()));
    }
    show_plan(&disk, &spec, &plan);
    if plan.writes.is_empty() {
        println!("Nothing to change.");
        return Ok(());
    }
    let change = Change {
        source: &spec.id(),
        changes: &plan.descriptions(),
        writes: &plan.writes,
        undoes: None,
    };
    write(&disk, &spec, &change, request.apply, &backup_dir)
}

/// The operation an action asks for, with partition numbers turned into
/// where those partitions start.
fn operation(disk: &Disk, action: TableAction) -> Result<Operation, CliError> {
    let table = read_table(disk)?;
    let start_of = |number: u32| -> Result<u64, CliError> {
        table
            .partitions
            .iter()
            .find(|p| p.number == number)
            .map(|p| p.start_lba)
            .ok_or_else(|| {
                CliError::Message(format!(
                    "There's no partition {number}; `datareco partitions {}` lists them.",
                    disk.name()
                ))
            })
    };
    Ok(match action {
        TableAction::Init { style } => Operation::Initialize { style },
        TableAction::Create {
            size,
            start,
            r#type,
            logical,
            name,
        } => {
            let (start_lba, sectors) = place(disk, &table, &size, start, logical)?;
            Operation::Create {
                start_lba,
                sectors,
                type_code: r#type,
                logical,
                name,
            }
        }
        TableAction::Delete { partition } => Operation::Delete {
            start_lba: start_of(partition)?,
        },
        TableAction::Active { partition, off } => Operation::SetActive {
            start_lba: start_of(partition)?,
            active: !off,
        },
        TableAction::Hide { partition } => Operation::SetHidden {
            start_lba: start_of(partition)?,
            hidden: true,
        },
        TableAction::Unhide { partition } => Operation::SetHidden {
            start_lba: start_of(partition)?,
            hidden: false,
        },
        TableAction::Type { partition, r#type } => Operation::SetType {
            start_lba: start_of(partition)?,
            type_code: r#type,
        },
        TableAction::Name { partition, name } => Operation::SetName {
            start_lba: start_of(partition)?,
            name,
        },
        TableAction::Convert { style } => Operation::Convert { style },
        TableAction::Logical { partition } => Operation::MakeLogical {
            start_lba: start_of(partition)?,
        },
        TableAction::Primary { partition } => Operation::MakePrimary {
            start_lba: start_of(partition)?,
        },
        TableAction::Restore {
            start,
            sectors,
            r#type,
        } => {
            let type_code = match r#type {
                Some(code) => code,
                None => identify(disk, start * u64::from(disk.sector_size()))?
                    .map_or("ntfs", |fs| type_for_file_system(fs.kind))
                    .to_owned(),
            };
            Operation::Create {
                start_lba: start,
                sectors,
                type_code,
                logical: false,
                name: None,
            }
        }
        TableAction::Undo { .. } | TableAction::Backups | TableAction::Types => {
            unreachable!("handled before")
        }
    })
}

/// Where a new partition goes: at `start`, or at the first 1 MiB boundary
/// of the first free space (in the extended partition, for logical ones)
/// it fits.
fn place(
    disk: &Disk,
    table: &datareco_partitions::PartitionTable,
    size: &str,
    start: Option<u64>,
    logical: bool,
) -> Result<(u64, u64), CliError> {
    let sector = u64::from(disk.sector_size());
    let align = (1 << 20) / sector;
    let wanted = parse_size(size)?.map(|bytes| bytes.div_ceil(sector));
    let segments = layout(table);
    let mut free: Vec<_> = segments
        .iter()
        .filter(|s| s.kind == SegmentKind::Free)
        .collect();
    // Logical ones prefer the extended partition's free space.
    free.sort_by_key(|s| (s.logical != logical, s.start_lba));
    for segment in free {
        let first = if segment.logical || logical {
            segment.start_lba + 1
        } else {
            segment.start_lba
        };
        let from = start.unwrap_or_else(|| first.next_multiple_of(align));
        if from < segment.start_lba || from >= segment.end_lba() {
            continue;
        }
        let room = segment.end_lba() - from;
        let sectors = wanted.unwrap_or(room);
        if sectors > 0 && sectors <= room {
            return Ok((from, sectors));
        }
    }
    Err(CliError::Message(match (start, wanted) {
        (Some(at), _) => format!("Sector {at} isn't in free space with room for that."),
        (None, Some(sectors)) => format!(
            "No free space has room for {}; `datareco map {}` shows the free space.",
            format_size(sectors * sector),
            disk.name()
        ),
        (None, None) => "There's no free space.".into(),
    }))
}

/// "20GiB", "500M", "1.5T" (binary units) → bytes; "max" → `None`.
fn parse_size(text: &str) -> Result<Option<u64>, CliError> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("max") || text.eq_ignore_ascii_case("all") {
        return Ok(None);
    }
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let shift = match unit.trim().to_ascii_lowercase().as_str() {
        "k" | "kib" | "kb" => 10,
        "m" | "mib" | "mb" => 20,
        "g" | "gib" | "gb" => 30,
        "t" | "tib" | "tb" => 40,
        _ => {
            return Err(CliError::Message(format!(
                "\"{text}\" isn't a size: use a unit, as in 500MiB or 20GiB, or max."
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
    let bytes = (value * (1u64 << shift) as f64) as u64;
    if bytes == 0 {
        return Err(CliError::Message("A partition needs a size.".into()));
    }
    Ok(Some(bytes))
}

fn show_plan(disk: &Arc<Disk>, spec: &SourceSpec, plan: &Plan) {
    println!("Changes to {}:", disk.name());
    for step in &plan.steps {
        println!("  - {}", step.description);
        for warning in &step.warnings {
            println!("      note: {warning}");
        }
    }
    let after = preview(Arc::clone(disk), &plan.writes);
    let devices = list_devices().ok();
    if let Ok(map) = disk_map(&after, spec, devices.as_ref()) {
        println!();
        println!("After the changes:");
        print_map(after.name(), &map);
    }
    println!();
}

fn undo(
    disk: &Arc<Disk>,
    spec: &SourceSpec,
    path: Option<&Path>,
    apply: Option<&str>,
    backup_dir: &Path,
) -> Result<(), CliError> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => next_to_undo(backup_dir, &spec.id())
            .map(|b| b.path)
            .ok_or_else(|| {
                CliError::Message(format!(
                    "No changes to {}'s table are left to undo (backups are in {}).",
                    disk.name(),
                    backup_dir.display()
                ))
            })?,
    };
    let backup: TableBackup = read_backup(&path).map_err(CliError::Message)?;
    let writes = undo_writes(disk, &backup)?.map_err(CliError::Message)?;
    println!(
        "Undoing the change of {} ({}):",
        format_time(i64::try_from(backup.created / 1000).unwrap_or(0)),
        path.display()
    );
    for change in &backup.changes {
        println!("  - {change}");
    }
    let after = preview(Arc::clone(disk), &writes);
    let devices = list_devices().ok();
    if let Ok(map) = disk_map(&after, spec, devices.as_ref()) {
        println!();
        println!("After undoing it:");
        print_map(after.name(), &map);
    }
    println!();
    let changes: Vec<String> = backup
        .changes
        .iter()
        .map(|c| format!("Undo: {c}"))
        .collect();
    let change = Change {
        source: &spec.id(),
        changes: &changes,
        writes: &writes,
        undoes: Some(&path),
    };
    write(disk, spec, &change, apply, backup_dir)
}

/// Writes after the person typed the disk's name with `--apply`.
fn write(
    disk: &Disk,
    spec: &SourceSpec,
    change: &Change<'_>,
    apply: Option<&str>,
    backup_dir: &Path,
) -> Result<(), CliError> {
    let (mut target, confirmation, closing): (Box<dyn SectorTarget>, String, Vec<String>) =
        match spec {
            SourceSpec::PhysicalDisk(number) => {
                let devices = list_devices()?;
                let check = check_disk_for_table_changes(*number, &devices)?;
                if devices.disks_of_path(backup_dir).contains(number) {
                    return Err(CliError::Message(format!(
                        "The backup folder {} is on {}; pick one on another disk with --backup-dir.",
                        backup_dir.display(),
                        check.name
                    )));
                }
                let Some(typed) = apply else {
                    return not_yet(&check.confirmation);
                };
                let volumes = check.volumes.clone();
                let target = DiskTarget::open(&check, typed)?;
                (Box::new(target), check.confirmation, volumes)
            }
            SourceSpec::Image(path) => {
                let name = spec.display_name();
                let Some(typed) = apply else {
                    return not_yet(&name);
                };
                if !typed.trim().eq_ignore_ascii_case(&name) {
                    return Err(CliError::Message(format!(
                        "Type --apply {name} to confirm changing that image's partition table."
                    )));
                }
                let target = ImageTarget::resume(path, disk.size())?;
                (Box::new(target), name, Vec::new())
            }
            SourceSpec::Volume(_) => unreachable!("refused before"),
        };
    if !closing.is_empty() {
        println!("Closing {} while the table is written.", closing.join(", "));
    }
    let backup = apply_writes(disk, change, target.as_mut(), backup_dir)?;
    drop(target); // closes the disk: Windows reads the new table
    let fresh = Disk::open(spec)?;
    let wrong = verify_writes(&fresh, change.writes)?;
    if !wrong.is_empty() {
        return Err(CliError::Disk(Error::Io {
            name: fresh.name().to_owned(),
            source: std::io::Error::other(format!(
                "sectors {wrong:?} don't read back as written; the backup is {}",
                backup.display()
            )),
        }));
    }
    println!("Written. The old table is saved in {}.", backup.display());
    println!(
        "To put it back: datareco table {} undo --apply {confirmation}",
        source_arg(spec)
    );
    Ok(())
}

fn not_yet(confirmation: &str) -> Result<(), CliError> {
    println!(
        "Nothing was written. To write these changes, run the same command with --apply {confirmation}"
    );
    Ok(())
}

fn source_arg(spec: &SourceSpec) -> String {
    match spec {
        SourceSpec::PhysicalDisk(n) => format!("disk{n}"),
        SourceSpec::Image(path) => format!("\"{}\"", path.display()),
        SourceSpec::Volume(v) => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("20GiB").unwrap(), Some(20 << 30));
        assert_eq!(parse_size("500M").unwrap(), Some(500 << 20));
        assert_eq!(parse_size("1.5 TiB").unwrap(), Some(3 << 39));
        assert_eq!(parse_size("max").unwrap(), None);
        assert!(parse_size("20").is_err());
        assert!(parse_size("0MiB").is_err());
        assert!(parse_size("lots").is_err());
    }
}
