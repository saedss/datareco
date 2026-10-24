//! Reading a disk's partition table and checking it for problems.

use std::collections::HashSet;

use datareco_io::{Disk, Result};
use serde::Serialize;

use crate::boot::{FileSystem, IDENTIFY_BYTES, identify, identify_bytes, identify_far};
use crate::bytes::has;
use crate::{gpt, mbr};

/// Logical partitions after this many are not followed (a loop or garbage).
const MAX_LOGICAL: u32 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TableKind {
    Mbr,
    Gpt,
    /// No partition table: a disk that's one volume, or empty or damaged.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PartitionKind {
    Primary,
    /// The MBR container for logical partitions.
    Extended,
    Logical,
    Gpt,
}

/// A partition as the partition table describes it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Partition {
    /// MBR: 1–4 for primary partitions, 5 and up for logical ones. GPT: entry number from 1.
    pub number: u32,
    pub kind: PartitionKind,
    pub start_lba: u64,
    pub sectors: u64,
    /// "0x07" for MBR, the type GUID for GPT.
    pub type_code: String,
    pub type_name: String,
    /// GPT partition name.
    pub name: Option<String>,
    /// GPT unique partition GUID.
    pub guid: Option<String>,
    /// Marked active (MBR) or legacy-BIOS-bootable (GPT).
    pub bootable: bool,
    /// The file system found at the partition's start, if any.
    pub file_system: Option<FileSystem>,
}

impl Partition {
    pub fn end_lba(&self) -> u64 {
        self.start_lba.saturating_add(self.sectors)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub severity: Severity,
    pub message: String,
}

fn problem(severity: Severity, message: impl Into<String>) -> Problem {
    Problem {
        severity,
        message: message.into(),
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionTable {
    pub kind: TableKind,
    pub sector_size: u32,
    pub disk_sectors: u64,
    /// The MBR disk signature or GPT disk GUID.
    pub disk_id: Option<String>,
    pub partitions: Vec<Partition>,
    /// With no partition table: the file system that fills the disk, if any.
    pub whole_disk: Option<FileSystem>,
    /// GPT: the sectors the header lets partitions use.
    pub usable: Option<UsableSectors>,
    pub problems: Vec<Problem>,
}

/// A range of sectors, first and last included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct UsableSectors {
    pub first: u64,
    pub last: u64,
}

/// Reads and checks the partition table. Never writes.
pub fn read_table(disk: &Disk) -> Result<PartitionTable> {
    let sector_size = u64::from(disk.sector_size());
    let start = read(disk, 0, IDENTIFY_BYTES)?;
    // A file system at sector 0 means no partition table ("superfloppy").
    let whole_disk = match identify_bytes(&start) {
        Some(found) => Some(found),
        // btrfs leaves its first 64 KiB empty, so a partition table there
        // isn't ruled out: only count it without one.
        None if start.get(510..512) != Some(&[0x55, 0xAA]) => identify_far(disk, 0)?,
        None => None,
    };
    let mbr = if whole_disk.is_some() {
        None
    } else {
        start.get(..512).and_then(mbr::parse)
    };
    let protective = mbr
        .as_ref()
        .is_some_and(|m| m.entries.iter().any(|e| e.type_code == 0xEE));

    let mut table = PartitionTable {
        kind: TableKind::None,
        sector_size: disk.sector_size(),
        disk_sectors: disk.sector_count(),
        disk_id: None,
        partitions: Vec::new(),
        whole_disk: None,
        usable: None,
        problems: Vec::new(),
    };
    let mut usable = None;

    if protective || (mbr.is_none() && whole_disk.is_none()) {
        if let Some((header, entries)) = read_gpt(disk, &mut table.problems)? {
            table.kind = TableKind::Gpt;
            table.disk_id = Some(gpt::guid(&header.disk_guid));
            usable = Some((header.first_usable, header.last_usable));
            table.partitions = entries.iter().map(gpt_partition).collect();
        } else if protective {
            table.problems.push(problem(
                Severity::Error,
                "The MBR marks this as a GPT disk, but neither GPT header (LBA 1 or the last sector) is valid. \
                 Search for lost partitions to find them.",
            ));
        }
    }

    if table.kind == TableKind::None {
        if let Some(mbr) = &mbr {
            table.kind = TableKind::Mbr;
            table.disk_id = Some(format!("{:08X}", mbr.disk_signature));
            read_mbr_partitions(disk, mbr, &mut table)?;
        } else if let Some(fs) = whole_disk {
            table.problems.push(problem(
                Severity::Info,
                format!(
                    "No partition table: the whole disk is one {} volume.",
                    fs.name
                ),
            ));
            table.whole_disk = Some(fs);
        } else if start.iter().take(512).all(|&b| b == 0) {
            table.problems.push(problem(
                Severity::Info,
                "Sector 0 is empty, so there's no partition table. Search for lost partitions to find any that were here.",
            ));
        } else {
            table.problems.push(problem(
                Severity::Info,
                "No partition table or file system found at the start of the disk.",
            ));
        }
    }

    for partition in &mut table.partitions {
        if partition.kind != PartitionKind::Extended && partition.start_lba < table.disk_sectors {
            partition.file_system = identify(disk, partition.start_lba * sector_size)?;
        }
    }
    let problems = check(&table, usable);
    table.problems.extend(problems);
    table.usable = usable.map(|(first, last)| UsableSectors { first, last });
    Ok(table)
}

fn read(disk: &Disk, offset: u64, len: u64) -> Result<Vec<u8>> {
    let len = len.min(disk.size().saturating_sub(offset));
    let mut data = vec![0; usize::try_from(len).unwrap_or(0)];
    disk.read_at(offset, &mut data)?;
    Ok(data)
}

fn read_header(disk: &Disk, lba: u64) -> Result<Option<gpt::Header>> {
    if lba >= disk.sector_count() {
        return Ok(None);
    }
    let sector = read(
        disk,
        lba * u64::from(disk.sector_size()),
        u64::from(disk.sector_size()),
    )?;
    Ok(gpt::parse_header(&sector).filter(|h| h.crc_ok && h.my_lba == lba))
}

/// The usable GPT header (primary, else backup) and its entries.
fn read_gpt(
    disk: &Disk,
    problems: &mut Vec<Problem>,
) -> Result<Option<(gpt::Header, Vec<gpt::Entry>)>> {
    let last = disk.sector_count().saturating_sub(1);
    let primary = read_header(disk, 1)?;
    let backup_lba = primary.as_ref().map_or(last, |h| h.alternate_lba);
    let backup = read_header(disk, backup_lba)?;

    let header = match (primary, backup) {
        (Some(primary), backup) => {
            if backup.is_none() {
                problems.push(problem(
                    Severity::Warning,
                    format!("The backup GPT header (LBA {backup_lba}) is missing or damaged. The primary header is fine."),
                ));
            }
            if primary.alternate_lba != last {
                problems.push(problem(
                    Severity::Warning,
                    format!(
                        "The backup GPT header should be in the disk's last sector (LBA {last}), \
                         but the primary header puts it at LBA {}. The disk may have been resized or copied.",
                        primary.alternate_lba
                    ),
                ));
            }
            primary
        }
        (None, Some(backup)) => {
            problems.push(problem(
                Severity::Error,
                format!("The primary GPT header (LBA 1) is missing or damaged. Using the backup header from LBA {last}."),
            ));
            backup
        }
        (None, None) => return Ok(None),
    };

    let (entries, crc_ok) = gpt::read_entries(disk, &header)?;
    if !crc_ok {
        problems.push(problem(
            Severity::Error,
            format!(
                "The partition entries at LBA {} don't match their checksum, so they may be damaged.",
                header.entries_lba
            ),
        ));
    }
    Ok(Some((header, entries)))
}

fn gpt_partition(entry: &gpt::Entry) -> Partition {
    let type_code = gpt::guid(&entry.type_guid);
    Partition {
        number: u32::try_from(entry.index + 1).unwrap_or(u32::MAX),
        kind: PartitionKind::Gpt,
        start_lba: entry.first_lba,
        sectors: entry
            .last_lba
            .checked_sub(entry.first_lba)
            .map_or(0, |n| n + 1),
        type_name: gpt::type_name(&type_code).to_owned(),
        type_code,
        name: entry.name.clone(),
        guid: Some(gpt::guid(&entry.unique_guid)),
        bootable: entry.attributes & 0x4 != 0,
        file_system: None,
    }
}

fn mbr_partition(number: u32, kind: PartitionKind, entry: &mbr::Entry, start: u64) -> Partition {
    Partition {
        number,
        kind,
        start_lba: start,
        sectors: entry.sectors,
        type_code: format!("0x{:02X}", entry.type_code),
        type_name: mbr::type_name(entry.type_code).to_owned(),
        name: None,
        guid: None,
        bootable: entry.status == 0x80,
        file_system: None,
    }
}

fn read_mbr_partitions(disk: &Disk, mbr: &mbr::Mbr, table: &mut PartitionTable) -> Result<()> {
    let mut next_logical = 5;
    for (number, entry) in (1..).zip(&mbr.entries) {
        if entry.is_empty() {
            continue;
        }
        if entry.is_extended() {
            table.partitions.push(mbr_partition(
                number,
                PartitionKind::Extended,
                entry,
                entry.start,
            ));
            read_logical_partitions(disk, entry, &mut next_logical, table)?;
        } else {
            table.partitions.push(mbr_partition(
                number,
                PartitionKind::Primary,
                entry,
                entry.start,
            ));
        }
    }
    Ok(())
}

/// Follows the chain of extended boot records inside an extended partition.
fn read_logical_partitions(
    disk: &Disk,
    extended: &mbr::Entry,
    next_number: &mut u32,
    table: &mut PartitionTable,
) -> Result<()> {
    let sector_size = u64::from(disk.sector_size());
    let mut ebr_lba = extended.start;
    let mut seen = HashSet::new();
    for _ in 0..MAX_LOGICAL {
        if !seen.insert(ebr_lba) {
            table.problems.push(problem(
                Severity::Error,
                format!("The chain of extended boot records loops back to LBA {ebr_lba}."),
            ));
            return Ok(());
        }
        if ebr_lba >= table.disk_sectors {
            table.problems.push(problem(
                Severity::Error,
                format!("An extended boot record points past the end of the disk (LBA {ebr_lba})."),
            ));
            return Ok(());
        }
        let sector = read(disk, ebr_lba * sector_size, 512)?;
        let Some(ebr) = mbr::parse(&sector) else {
            let empty = has(&sector, 510, &[0x55, 0xAA])
                && sector
                    .get(446..510)
                    .is_some_and(|e| e.iter().all(|&b| b == 0));
            if !empty {
                table.problems.push(problem(
                    Severity::Error,
                    format!(
                        "The extended boot record at LBA {ebr_lba} is missing or damaged, \
                         so the logical partitions from there on can't be read."
                    ),
                ));
            }
            return Ok(());
        };
        let [first, link, ..] = &ebr.entries;
        if !first.is_empty() {
            let start = ebr_lba.saturating_add(first.start);
            table.partitions.push(mbr_partition(
                *next_number,
                PartitionKind::Logical,
                first,
                start,
            ));
            *next_number += 1;
        }
        if link.is_empty() || !link.is_extended() {
            return Ok(());
        }
        ebr_lba = extended.start.saturating_add(link.start);
    }
    table.problems.push(problem(
        Severity::Warning,
        format!("Stopped after {MAX_LOGICAL} logical partitions."),
    ));
    Ok(())
}

fn check(table: &PartitionTable, usable: Option<(u64, u64)>) -> Vec<Problem> {
    let mut problems = Vec::new();
    let sector_size = u64::from(table.sector_size);
    let extended: Vec<&Partition> = table
        .partitions
        .iter()
        .filter(|p| p.kind == PartitionKind::Extended)
        .collect();

    for p in &table.partitions {
        let n = p.number;
        if p.sectors == 0 {
            problems.push(problem(
                Severity::Warning,
                format!("Partition {n} has a size of 0."),
            ));
            continue;
        }
        if p.start_lba == 0 {
            problems.push(problem(
                Severity::Error,
                format!("Partition {n} starts at sector 0, on top of the partition table."),
            ));
        }
        if p.end_lba() > table.disk_sectors {
            problems.push(problem(
                Severity::Error,
                format!(
                    "Partition {n} ends at sector {}, past the end of the disk ({} sectors).",
                    p.end_lba() - 1,
                    table.disk_sectors
                ),
            ));
        }
        if let Some((first, last)) = usable
            && (p.start_lba < first || p.end_lba() - 1 > last)
        {
            problems.push(problem(
                Severity::Warning,
                format!("Partition {n} lies outside the area the GPT header marks as usable (LBA {first}–{last})."),
            ));
        }
        if p.kind == PartitionKind::Logical
            && !extended
                .iter()
                .any(|e| p.start_lba >= e.start_lba && p.end_lba() <= e.end_lba())
        {
            problems.push(problem(
                Severity::Warning,
                format!("Logical partition {n} isn't inside the extended partition."),
            ));
        }
        if let Some(fs) = &p.file_system
            && let Some(fs_size) = fs.size
            && fs_size > p.sectors.saturating_mul(sector_size)
        {
            problems.push(problem(
                Severity::Warning,
                format!(
                    "Partition {n} is {}, but its {} file system says it's {}. \
                     The partition may have been shrunk or its table damaged.",
                    human_size(p.sectors.saturating_mul(sector_size)),
                    fs.name,
                    human_size(fs_size)
                ),
            ));
        }
    }

    let mut volumes: Vec<&Partition> = table
        .partitions
        .iter()
        .filter(|p| p.kind != PartitionKind::Extended && p.sectors > 0)
        .collect();
    volumes.sort_by_key(|p| p.start_lba);
    for pair in volumes.windows(2) {
        if let [a, b] = pair
            && b.start_lba < a.end_lba()
        {
            problems.push(problem(
                Severity::Error,
                format!(
                    "Partitions {} and {} overlap (sectors {}–{}).",
                    a.number,
                    b.number,
                    b.start_lba,
                    a.end_lba().min(b.end_lba()) - 1
                ),
            ));
        }
    }

    if table.kind == TableKind::Mbr && table.partitions.iter().filter(|p| p.bootable).count() > 1 {
        problems.push(problem(
            Severity::Warning,
            "More than one partition is marked active (bootable).",
        ));
    }
    problems
}

/// `1536` → `1.5 KiB`.
pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    #[allow(clippy::cast_precision_loss)] // display only
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}
