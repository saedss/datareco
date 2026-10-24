//! Changing partition tables: an editable model of a disk's table, the
//! operations people apply to it, the sectors those turn into, and backups
//! to undo them.
//!
//! Nothing here writes on its own. [`plan`] works out what a list of
//! operations would change (and [`preview`] shows the result as the disk
//! would read afterwards); [`apply_writes`] writes a plan's sectors to a
//! target the caller opened, after saving the sectors it replaces to a
//! backup file that [`undo_writes`] turns back into a plan.
//!
//! Partitions are named by where they start, not by number: numbers of
//! MBR logical partitions shift when one before them goes.

mod backup;
mod encode;
mod guid;
mod ops;
mod plan;
#[cfg(test)]
mod tests;
mod types;

use std::collections::HashSet;

use datareco_io::{Disk, Result};
use serde::{Deserialize, Serialize};

use crate::FsKind;
use crate::bytes::{array16, has, u8_at, u32_at, u64_at, utf16_text};
use crate::crc32::crc32;
use crate::table::{TableKind, read, read_header, read_table};

pub use backup::{
    BackupInfo, Change, SectorWrite, TableBackup, apply_writes, default_backup_dir, list_backups,
    next_to_undo, read_backup, undo_writes, verify_writes,
};
pub use ops::{Operation, Step};
pub use plan::{Plan, StepError, plan, preview};
pub use types::{TypeChoice, type_choices, type_for_file_system};

/// A partition table style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    Mbr,
    Gpt,
}

impl Style {
    pub fn name(self) -> &'static str {
        match self {
            Style::Mbr => "MBR",
            Style::Gpt => "GPT",
        }
    }
}

/// GPT entries in a new table, and their size: the usual 16 KiB array.
const GPT_ENTRIES: u32 = 128;
const GPT_ENTRY_BYTES: u32 = 128;
/// A GPT partition name holds at most 36 UTF-16 units.
const GPT_NAME_UNITS: usize = 36;
/// Logical partitions after this many are not followed.
const MAX_LOGICAL: usize = 128;

/// A partition in the editable table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Part {
    pub start: u64,
    pub sectors: u64,
    /// MBR: a logical partition, inside the extended one.
    pub logical: bool,
    /// MBR primary: entry 0–3. GPT: entry index. Unused for logical ones.
    pub slot: usize,
    /// MBR logical: where its extended boot record is.
    pub ebr: u64,
    pub mbr_type: u8,
    pub active: bool,
    pub gpt_type: [u8; 16],
    pub guid: [u8; 16],
    pub attributes: u64,
    pub name: String,
    /// GPT: the entry as it was, so bytes past the known fields are kept.
    pub raw: Vec<u8>,
    /// The file system at its start, if one was recognized. Not written.
    pub fs: Option<FsKind>,
}

impl Part {
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.sectors)
    }

    fn blank(start: u64, sectors: u64) -> Part {
        Part {
            start,
            sectors,
            logical: false,
            slot: 0,
            ebr: 0,
            mbr_type: 0,
            active: false,
            gpt_type: [0; 16],
            guid: [0; 16],
            attributes: 0,
            name: String::new(),
            raw: Vec::new(),
            fs: None,
        }
    }
}

/// The MBR container for logical partitions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Extended {
    pub start: u64,
    pub sectors: u64,
    pub slot: usize,
    pub mbr_type: u8,
}

impl Extended {
    pub fn end(&self) -> u64 {
        self.start.saturating_add(self.sectors)
    }
}

/// A disk's partition table, in a form that can be changed and written.
#[derive(Clone, Debug)]
pub struct EditableTable {
    pub(crate) sector_size: u32,
    pub(crate) disk_sectors: u64,
    /// `None`: no partition table yet.
    pub(crate) style: Option<Style>,
    /// Sector 0 as it was; its boot code is kept.
    pub(crate) sector0: Vec<u8>,
    pub(crate) mbr_signature: u32,
    pub(crate) extended: Option<Extended>,
    pub(crate) disk_guid: [u8; 16],
    pub(crate) entry_count: u32,
    pub(crate) entry_size: u32,
    pub(crate) parts: Vec<Part>,
    /// No table, but a file system fills the disk.
    pub(crate) whole_disk: bool,
    /// Sectors to fill with zeros when written (old GPT headers after
    /// converting to MBR).
    pub(crate) clear: Vec<u64>,
}

impl EditableTable {
    pub fn style(&self) -> Option<Style> {
        self.style
    }

    /// Reads the disk's table. `Ok(Err(reason))` when it can't be changed
    /// safely (damaged, dynamic, unreadable chain), with the reason for people.
    pub fn load(disk: &Disk) -> Result<std::result::Result<EditableTable, String>> {
        let table = read_table(disk)?;
        let sector_size = disk.sector_size();
        let sector0 = read(disk, 0, u64::from(sector_size))?;
        let mut model = EditableTable {
            sector_size,
            disk_sectors: disk.sector_count(),
            style: None,
            sector0,
            mbr_signature: 0,
            extended: None,
            disk_guid: [0; 16],
            entry_count: GPT_ENTRIES,
            entry_size: GPT_ENTRY_BYTES,
            parts: Vec::new(),
            whole_disk: table.whole_disk.is_some(),
            clear: Vec::new(),
        };
        if !disk.size().is_multiple_of(u64::from(sector_size)) {
            return Ok(Err(
                "The image doesn't end on a whole sector, so its table can't be rewritten.".into(),
            ));
        }
        let loaded = match table.kind {
            TableKind::None => {
                let protective =
                    (0..4).any(|slot| u8_at(&model.sector0, 446 + 16 * slot + 4) == Some(0xEE));
                if protective && has(&model.sector0, 510, &[0x55, 0xAA]) {
                    Err("The disk is marked as GPT, but neither GPT header can be read. Search for \
                         lost partitions instead."
                        .to_owned())
                } else {
                    Ok(())
                }
            }
            TableKind::Mbr => model.load_mbr(disk)?,
            TableKind::Gpt => model.load_gpt(disk)?,
        };
        if let Err(reason) = loaded {
            return Ok(Err(reason));
        }
        for part in &mut model.parts {
            part.fs = table
                .partitions
                .iter()
                .find(|p| p.start_lba == part.start && p.kind != crate::PartitionKind::Extended)
                .and_then(|p| p.file_system.as_ref())
                .map(|fs| fs.kind);
        }
        if let Err(problem) = model.check_layout() {
            return Ok(Err(format!(
                "The partition table has a problem DataReco won't build on: {problem} Search for \
                 lost partitions, or copy the disk first."
            )));
        }
        Ok(Ok(model))
    }

    fn load_mbr(&mut self, disk: &Disk) -> Result<std::result::Result<(), String>> {
        self.style = Some(Style::Mbr);
        self.mbr_signature = u32_at(&self.sector0, 440).unwrap_or(0);
        for slot in 0..4 {
            let Some((status, kind, start, sectors)) = mbr_entry(&self.sector0, 446 + 16 * slot)
            else {
                continue;
            };
            if kind == 0x42 {
                return Ok(Err(
                    "This is a dynamic disk, which DataReco doesn't change.".into(),
                ));
            }
            if matches!(kind, 0x05 | 0x0F | 0x85) {
                if self.extended.is_some() {
                    return Ok(Err("The table has two extended partitions.".into()));
                }
                self.extended = Some(Extended {
                    start,
                    sectors,
                    slot,
                    mbr_type: kind,
                });
                continue;
            }
            let mut part = Part::blank(start, sectors);
            part.slot = slot;
            part.mbr_type = kind;
            part.active = status == 0x80;
            self.parts.push(part);
        }
        let Some(extended) = self.extended.clone() else {
            return Ok(Ok(()));
        };
        let sector_size = u64::from(self.sector_size);
        let mut ebr = extended.start;
        let mut seen = HashSet::new();
        for _ in 0..MAX_LOGICAL {
            if !seen.insert(ebr) || ebr >= self.disk_sectors {
                return Ok(Err("The chain of logical partitions is damaged.".into()));
            }
            let sector = read(disk, ebr * sector_size, sector_size)?;
            if !has(&sector, 510, &[0x55, 0xAA]) {
                // An extended partition with nothing in it may have no EBR yet.
                if ebr == extended.start && sector.iter().all(|&b| b == 0) {
                    return Ok(Ok(()));
                }
                return Ok(Err(format!(
                    "The boot record of a logical partition (sector {ebr}) is damaged."
                )));
            }
            if let Some((status, kind, relative, sectors)) = mbr_entry(&sector, 446) {
                let mut part = Part::blank(ebr + relative, sectors);
                part.logical = true;
                part.ebr = ebr;
                part.mbr_type = kind;
                part.active = status == 0x80;
                self.parts.push(part);
            }
            match mbr_entry(&sector, 462) {
                Some((_, 0x05 | 0x0F | 0x85, relative, _)) => ebr = extended.start + relative,
                _ => return Ok(Ok(())),
            }
        }
        Ok(Err(
            "There are too many logical partitions to follow.".into()
        ))
    }

    fn load_gpt(&mut self, disk: &Disk) -> Result<std::result::Result<(), String>> {
        self.style = Some(Style::Gpt);
        let last = disk.sector_count().saturating_sub(1);
        let header = match read_header(disk, 1)? {
            Some(header) => header,
            None => match read_header(disk, last)? {
                Some(header) => header,
                None => return Ok(Err("Neither GPT header can be read.".into())),
            },
        };
        let (Ok(count), Ok(size)) = (
            u32::try_from(header.entry_count),
            u32::try_from(header.entry_size),
        ) else {
            return Ok(Err("The GPT header is damaged.".into()));
        };
        let sector_size = u64::from(self.sector_size);
        let len = header.entry_count * header.entry_size;
        let Some(offset) = header
            .entries_lba
            .checked_mul(sector_size)
            .filter(|&o| o.saturating_add(len) <= disk.size())
        else {
            return Ok(Err(
                "The GPT partition entries are past the end of the disk.".into(),
            ));
        };
        let array = read(disk, offset, len)?;
        if crc32(&array) != header.entries_crc {
            return Ok(Err(
                "The GPT partition entries don't match their checksum, so they may be damaged."
                    .into(),
            ));
        }
        self.disk_guid = header.disk_guid;
        self.entry_count = count;
        self.entry_size = size;
        let entry_size = size as usize;
        for (slot, raw) in array.chunks(entry_size).enumerate() {
            let Some(kind) = array16(raw, 0).filter(|g| *g != [0; 16]) else {
                continue;
            };
            if types::is_dynamic_gpt(&kind) {
                return Ok(Err(
                    "This is a dynamic disk or a Storage Spaces disk, which DataReco doesn't change."
                        .into(),
                ));
            }
            let first = u64_at(raw, 32).unwrap_or(0);
            let last = u64_at(raw, 40).unwrap_or(0);
            let mut part = Part::blank(first, (last + 1).saturating_sub(first));
            part.slot = slot;
            part.gpt_type = kind;
            part.guid = array16(raw, 16).unwrap_or([0; 16]);
            part.attributes = u64_at(raw, 48).unwrap_or(0);
            part.name = crate::bytes::bytes(raw, 56, 72)
                .and_then(utf16_text)
                .unwrap_or_default();
            part.raw = raw.to_vec();
            self.parts.push(part);
        }
        Ok(Ok(()))
    }

    /// GPT: sectors of the entry array.
    pub(crate) fn array_sectors(&self) -> u64 {
        (u64::from(self.entry_count) * u64::from(self.entry_size))
            .div_ceil(u64::from(self.sector_size))
    }

    /// GPT: the first and last sectors partitions may use.
    pub(crate) fn usable(&self) -> (u64, u64) {
        let array = self.array_sectors();
        (2 + array, self.disk_sectors.saturating_sub(2 + array))
    }

    /// 1 MiB in sectors: where new partitions start.
    pub(crate) fn align(&self) -> u64 {
        (crate::ALIGNMENT_BYTES / u64::from(self.sector_size)).max(1)
    }

    /// Logical partitions in disk order.
    pub(crate) fn logicals(&self) -> Vec<&Part> {
        let mut logical: Vec<&Part> = self.parts.iter().filter(|p| p.logical).collect();
        logical.sort_by_key(|p| p.start);
        logical
    }

    /// The number people know a partition by: MBR 1–4 for primary ones and
    /// 5 on for logical ones in disk order; GPT the entry number.
    pub(crate) fn number(&self, part: &Part) -> u32 {
        let n = if part.logical {
            5 + self
                .logicals()
                .iter()
                .position(|p| p.start == part.start)
                .unwrap_or(0)
        } else {
            part.slot + 1
        };
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Checks the table is one DataReco can write: everything on the disk,
    /// nothing overlapping, logical partitions inside the extended one with
    /// room for their boot records, entries within their limits.
    pub(crate) fn check_layout(&self) -> std::result::Result<(), String> {
        let Some(style) = self.style else {
            return Ok(());
        };
        let (first, last) = match style {
            Style::Gpt => self.usable(),
            Style::Mbr => (1, self.disk_sectors.saturating_sub(1)),
        };
        let mut slots = HashSet::new();
        let mut top: Vec<(u64, u64)> = Vec::new();
        for p in &self.parts {
            if p.sectors == 0 {
                return Err(format!("A partition at sector {} has no size.", p.start));
            }
            if p.start < first || p.end() - 1 > last {
                return Err(format!(
                    "The partition at sector {} isn't inside the usable area (sectors {first}–{last}).",
                    p.start
                ));
            }
            if style == Style::Mbr
                && (p.start > u64::from(u32::MAX) || p.sectors > u64::from(u32::MAX))
            {
                return Err(format!(
                    "The partition at sector {} is past what an MBR can describe (2 TiB with 512-byte sectors).",
                    p.start
                ));
            }
            if !p.logical {
                if !slots.insert(p.slot) {
                    return Err("Two partitions share a table entry.".into());
                }
                top.push((p.start, p.end()));
            }
        }
        match style {
            Style::Gpt => {
                if self
                    .parts
                    .iter()
                    .any(|p| p.slot >= self.entry_count as usize)
                {
                    return Err("A partition is past the end of the entry array.".into());
                }
            }
            Style::Mbr => {
                if let Some(e) = &self.extended {
                    if !slots.insert(e.slot) {
                        return Err("The extended partition shares a table entry.".into());
                    }
                    if e.start < 1 || e.end() > self.disk_sectors || e.sectors == 0 {
                        return Err("The extended partition isn't on the disk.".into());
                    }
                    top.push((e.start, e.end()));
                }
                if slots.iter().any(|&s| s > 3) {
                    return Err("An MBR has four entries.".into());
                }
                let logicals = self.logicals();
                if !logicals.is_empty() {
                    let Some(e) = &self.extended else {
                        return Err(
                            "There are logical partitions but no extended partition.".into()
                        );
                    };
                    let mut previous_end = e.start;
                    for (i, l) in logicals.iter().enumerate() {
                        let ebr_ok = if i == 0 {
                            l.ebr == e.start && e.start < l.start
                        } else {
                            l.ebr >= previous_end && l.ebr < l.start
                        };
                        if !ebr_ok {
                            return Err(format!(
                                "The logical partition at sector {} has no room for its boot record.",
                                l.start
                            ));
                        }
                        if l.end() > e.end() {
                            return Err(format!(
                                "The logical partition at sector {} goes past the extended partition.",
                                l.start
                            ));
                        }
                        previous_end = l.end();
                    }
                }
            }
        }
        top.sort_unstable();
        if top.windows(2).any(|w| w[1].0 < w[0].1) {
            return Err("Two partitions overlap.".into());
        }
        Ok(())
    }
}

/// An MBR or EBR entry at `at`: (status, type, start, sectors), or `None`
/// when it's empty.
fn mbr_entry(sector: &[u8], at: usize) -> Option<(u8, u8, u64, u64)> {
    let status = u8_at(sector, at)?;
    let kind = u8_at(sector, at + 4)?;
    let start = u64::from(u32_at(sector, at + 8)?);
    let sectors = u64::from(u32_at(sector, at + 12)?);
    (kind != 0 && sectors != 0).then_some((status, kind, start, sectors))
}
