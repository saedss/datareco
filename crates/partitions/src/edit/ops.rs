//! The changes people make to a partition table, one at a time, each
//! checked against the table as the changes before it left it.

use datareco_io::Disk;
use serde::{Deserialize, Serialize};

use super::guid::{new_guid, new_signature};
use super::types::{self, gpt_to_mbr, gpt_type, hide_mbr, mbr_to_gpt, mbr_type, unhide_mbr};
use super::{EditableTable, Extended, GPT_ENTRIES, GPT_ENTRY_BYTES, GPT_NAME_UNITS, Part, Style};
use crate::MBR_SECTOR_LIMIT;
use crate::table::{human_size, read};

/// One change to a partition table. Partitions are named by their first
/// sector (`start_lba`), as the disk map shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Operation {
    /// Gives a disk with no partition table an empty one.
    Initialize {
        style: Style,
    },
    /// A new partition, unformatted. `type_code` is a name from
    /// [`super::type_choices`] or a raw code; `logical` is for MBR disks.
    Create {
        start_lba: u64,
        sectors: u64,
        type_code: String,
        #[serde(default)]
        logical: bool,
        #[serde(default)]
        name: Option<String>,
    },
    /// Removes a partition from the table. Its data stays on the disk.
    Delete {
        start_lba: u64,
    },
    /// MBR: the partition the BIOS boots (only one). GPT: the legacy BIOS
    /// bootable attribute.
    SetActive {
        start_lba: u64,
        active: bool,
    },
    /// MBR: the hidden variant of a Windows type. GPT: the hidden and
    /// no-drive-letter attributes.
    SetHidden {
        start_lba: u64,
        hidden: bool,
    },
    SetType {
        start_lba: u64,
        type_code: String,
    },
    /// GPT only.
    SetName {
        start_lba: u64,
        name: String,
    },
    /// Rewrites the table in the other style, keeping every partition.
    Convert {
        style: Style,
    },
    /// MBR: makes a primary partition next to the extended one logical.
    MakeLogical {
        start_lba: u64,
    },
    /// MBR: makes the first or last logical partition primary.
    MakePrimary {
        start_lba: u64,
    },
}

/// What an operation does, for people.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub description: String,
    /// Things worth knowing before applying it.
    pub warnings: Vec<String>,
}

type Outcome = Result<Step, String>;

fn step(description: String) -> Step {
    Step {
        description,
        warnings: Vec::new(),
    }
}

/// Bit 2: legacy BIOS bootable. Bits 62 and 63 (basic data): hidden, no
/// drive letter.
const GPT_BOOTABLE: u64 = 1 << 2;
const GPT_HIDDEN: u64 = (1 << 62) | (1 << 63);

impl EditableTable {
    /// Applies one operation, or says why it can't be done. `disk` is read
    /// to check that initializing or converting overwrites nothing.
    pub(crate) fn apply(&mut self, op: &Operation, disk: &Disk) -> Outcome {
        let mut changed = self.clone();
        let outcome = match op {
            Operation::Initialize { style } => changed.initialize(*style, disk),
            Operation::Create {
                start_lba,
                sectors,
                type_code,
                logical,
                name,
            } => changed.create(*start_lba, *sectors, type_code, *logical, name.as_deref()),
            Operation::Delete { start_lba } => changed.delete(*start_lba),
            Operation::SetActive { start_lba, active } => changed.set_active(*start_lba, *active),
            Operation::SetHidden { start_lba, hidden } => changed.set_hidden(*start_lba, *hidden),
            Operation::SetType {
                start_lba,
                type_code,
            } => changed.set_type(*start_lba, type_code),
            Operation::SetName { start_lba, name } => changed.set_name(*start_lba, name),
            Operation::Convert { style } => changed.convert(*style, disk),
            Operation::MakeLogical { start_lba } => changed.make_logical(*start_lba),
            Operation::MakePrimary { start_lba } => changed.make_primary(*start_lba),
        }?;
        // A guard against mistakes here: never hand on a table that isn't valid.
        changed
            .check_layout()
            .map_err(|problem| format!("That would leave the table invalid: {problem}"))?;
        *self = changed;
        Ok(outcome)
    }

    fn style_or_explain(&self) -> Result<Style, String> {
        self.style.ok_or_else(|| {
            "The disk has no partition table yet: initialize it first (MBR or GPT).".to_owned()
        })
    }

    fn find(&self, start: u64) -> Result<usize, String> {
        self.parts
            .iter()
            .position(|p| p.start == start)
            .ok_or_else(|| format!("There's no partition starting at sector {start}."))
    }

    /// "partition 3 (NTFS, 100.0 GiB)".
    pub(crate) fn describe(&self, part: &Part) -> String {
        let what = match (part.fs, self.style) {
            (Some(fs), _) => fs.name().to_owned(),
            (None, Some(Style::Gpt)) => {
                crate::gpt::type_name(&crate::gpt::guid(&part.gpt_type)).to_owned()
            }
            (None, _) => crate::mbr::type_name(part.mbr_type).to_owned(),
        };
        format!(
            "partition {} ({what}, {})",
            self.number(part),
            self.size(part.sectors)
        )
    }

    fn size(&self, sectors: u64) -> String {
        human_size(sectors.saturating_mul(u64::from(self.sector_size)))
    }

    // ------------------------------------------------------------ initialize

    fn initialize(&mut self, style: Style, disk: &Disk) -> Outcome {
        if self.style.is_some() {
            return Err("The disk already has a partition table.".into());
        }
        if self.whole_disk {
            return Err(
                "A file system fills this disk without a partition table; initializing \
                        would hide it. Copy what's on it first."
                    .into(),
            );
        }
        self.entry_count = GPT_ENTRIES;
        self.entry_size = GPT_ENTRY_BYTES;
        let minimum = 2 * (2 + self.array_sectors()) + 2 * self.align();
        if self.disk_sectors < minimum {
            return Err("The disk is too small for a partition table.".into());
        }
        // Only empty sectors are written over: on a disk whose table was
        // wiped, they may be all that's left of its partitions.
        let mut areas = vec![(0u64, 1u64)];
        if style == Style::Gpt {
            let array = self.array_sectors();
            areas.push((1, 2 + array));
            areas.push((self.disk_sectors - 1 - array, self.disk_sectors));
        }
        for (from, to) in areas {
            let (offset, len) = if from == 0 {
                (440, 72) // the boot code area is kept as it is
            } else {
                (
                    from * u64::from(self.sector_size),
                    (to - from) * u64::from(self.sector_size),
                )
            };
            let data = read(disk, offset, len).map_err(|e| e.to_string())?;
            if data.iter().any(|&b| b != 0) {
                let place = if from == 0 {
                    "Sector 0".to_owned()
                } else {
                    format!("Sectors {from}–{}", to - 1)
                };
                return Err(format!(
                    "{place} hold data, so initializing could write over something you want \
                     back. Search for lost partitions first."
                ));
            }
        }
        self.style = Some(style);
        let mut warnings = Vec::new();
        match style {
            Style::Mbr => {
                self.mbr_signature = new_signature();
                if self.disk_sectors > MBR_SECTOR_LIMIT {
                    warnings.push(format!(
                        "An MBR table can only use the first {} of this disk; GPT can use all of it.",
                        self.size(MBR_SECTOR_LIMIT)
                    ));
                }
            }
            Style::Gpt => self.disk_guid = new_guid(),
        }
        Ok(Step {
            description: format!(
                "Initialize the disk with an empty {} partition table",
                style.name()
            ),
            warnings,
        })
    }

    // ------------------------------------------------------------ create

    fn create(
        &mut self,
        start: u64,
        sectors: u64,
        type_code: &str,
        logical: bool,
        name: Option<&str>,
    ) -> Outcome {
        let style = self.style_or_explain()?;
        if sectors == 0 {
            return Err("A partition needs a size.".into());
        }
        let end = start
            .checked_add(sectors)
            .filter(|&end| end <= self.disk_sectors)
            .ok_or("That goes past the end of the disk.")?;
        let mut part = Part::blank(start, sectors);
        match style {
            Style::Gpt => {
                if logical {
                    return Err("GPT disks have no logical partitions.".into());
                }
                let (first, last) = self.usable();
                if start < first || end - 1 > last {
                    return Err(format!(
                        "Partitions on this disk must be within sectors {first}–{last}."
                    ));
                }
                self.check_free(start, end, false)?;
                part.slot = (0..self.entry_count as usize)
                    .find(|slot| !self.parts.iter().any(|p| p.slot == *slot))
                    .ok_or("Every entry of the GPT is in use.")?;
                part.gpt_type = gpt_type(type_code)?;
                part.guid = new_guid();
                part.raw = vec![0; self.entry_size as usize];
                part.name = match name {
                    Some(name) => check_name(name)?,
                    None if types::is_basic_data(&part.gpt_type) => {
                        "Basic data partition".to_owned()
                    }
                    None => String::new(),
                };
            }
            Style::Mbr => {
                part.mbr_type = mbr_type(type_code)?;
                if name.is_some_and(|n| !n.is_empty()) {
                    return Err("MBR partitions have no names.".into());
                }
                if start == 0 || start > u64::from(u32::MAX) || sectors > u64::from(u32::MAX) {
                    return Err(format!(
                        "An MBR table can't describe that: it reaches only the first {}.",
                        self.size(MBR_SECTOR_LIMIT)
                    ));
                }
                if logical {
                    self.place_logical(&mut part)?;
                } else {
                    if self
                        .extended
                        .as_ref()
                        .is_some_and(|e| start < e.end() && end > e.start)
                    {
                        return Err(
                            "That space is inside the extended partition: create a logical \
                                    partition there."
                                .into(),
                        );
                    }
                    self.check_free(start, end, false)?;
                    part.slot = self.free_slot().ok_or(
                        "An MBR disk holds at most four primary partitions, counting the extended \
                         one. Create a logical partition instead, or convert the disk to GPT.",
                    )?;
                }
            }
        }
        let kind = if part.logical { "logical " } else { "" };
        let type_name = match style {
            Style::Gpt => crate::gpt::type_name(&crate::gpt::guid(&part.gpt_type)).to_owned(),
            Style::Mbr => crate::mbr::type_name(part.mbr_type).to_owned(),
        };
        self.parts.push(part);
        Ok(Step {
            description: format!(
                "Create a {} {kind}partition at sector {start}, type {type_name}",
                self.size(sectors)
            ),
            warnings: vec![
                "New partitions aren't formatted: Windows offers to format them once they're there."
                    .into(),
            ],
        })
    }

    /// Fails if any partition (and, with `logical_only`, only logical ones)
    /// uses sectors `start..end`.
    fn check_free(&self, start: u64, end: u64, logical_only: bool) -> Result<(), String> {
        let taken = self
            .parts
            .iter()
            .filter(|p| !logical_only || p.logical)
            .find(|p| start < p.end() && end > p.start);
        match taken {
            Some(p) => Err(format!("That space overlaps {}.", self.describe(p))),
            None => Ok(()),
        }
    }

    fn free_slot(&self) -> Option<usize> {
        (0..4).find(|slot| {
            !self.parts.iter().any(|p| !p.logical && p.slot == *slot)
                && self.extended.as_ref().is_none_or(|e| e.slot != *slot)
        })
    }

    /// Puts a new logical partition in the chain, making an extended
    /// partition around it when there's none.
    fn place_logical(&mut self, part: &mut Part) -> Result<(), String> {
        let (start, end) = (part.start, part.end());
        part.logical = true;
        let Some(e) = self.extended.clone() else {
            // An extended partition filling the free space around it.
            let (from, to) = self.free_around(start, end)?;
            if from >= start {
                return Err(
                    "A logical partition needs a free sector before it for its boot record.".into(),
                );
            }
            let slot = self.free_slot().ok_or(
                "All four MBR entries are in use, so there's no room for an extended partition.",
            )?;
            self.extended = Some(Extended {
                start: from,
                sectors: to - from,
                slot,
                mbr_type: 0x0F,
            });
            part.ebr = from;
            return Ok(());
        };
        if start < e.start || end > e.end() {
            return Err(
                "Logical partitions go inside the extended partition (the free space \
                        marked as in the extended partition)."
                    .into(),
            );
        }
        self.check_free(start, end, true)?;
        let logicals = self.logicals();
        let previous = logicals.iter().rev().find(|l| l.end() <= start);
        let next_start = logicals.iter().find(|l| l.start >= end).map(|l| l.start);
        part.ebr = previous.map_or(e.start, |p| p.end());
        if part.ebr >= start {
            return Err(
                "A logical partition needs a free sector before it for its boot record.".into(),
            );
        }
        // The first logical partition's boot record is the extended
        // partition's first sector; the one that was first needs another.
        if previous.is_none()
            && let Some(next_start) = next_start
        {
            if end >= next_start {
                return Err(
                    "There's no free sector left for the next partition's boot record.".into(),
                );
            }
            if let Some(next) = self
                .parts
                .iter_mut()
                .find(|p| p.logical && p.start == next_start)
                && next.ebr < end
            {
                next.ebr = end;
            }
        }
        Ok(())
    }

    /// The free stretch (outside any partition) holding `start..end`.
    fn free_around(&self, start: u64, end: u64) -> Result<(u64, u64), String> {
        self.check_free(start, end, false)?;
        let limit = self.disk_sectors.min(MBR_SECTOR_LIMIT);
        let from = self
            .parts
            .iter()
            .map(Part::end)
            .filter(|&e| e <= start)
            .max()
            .unwrap_or(1)
            .max(1);
        let to = self
            .parts
            .iter()
            .map(|p| p.start)
            .filter(|&s| s >= end)
            .min()
            .unwrap_or(limit)
            .min(limit);
        if end > to {
            return Err("That goes past what an MBR can reach.".into());
        }
        Ok((from, to))
    }

    // ------------------------------------------------------------ delete

    fn delete(&mut self, start: u64) -> Outcome {
        self.style_or_explain()?;
        let Some(index) = self.parts.iter().position(|p| p.start == start) else {
            // The extended partition itself, when it's empty.
            if self.extended.as_ref().is_some_and(|e| e.start == start) {
                if self.parts.iter().any(|p| p.logical) {
                    return Err(
                        "The extended partition still holds logical partitions: delete \
                                those first."
                            .into(),
                    );
                }
                self.extended = None;
                return Ok(step("Delete the empty extended partition".to_owned()));
            }
            return Err(format!("There's no partition starting at sector {start}."));
        };
        let description = format!("Delete {}", self.describe(&self.parts[index]));
        let removed = self.parts.remove(index);
        let mut text = description;
        if removed.logical {
            let extended_start = self.extended.as_ref().map(|e| e.start);
            let mut rest: Vec<&mut Part> = self.parts.iter_mut().filter(|p| p.logical).collect();
            rest.sort_by_key(|p| p.start);
            match rest.first_mut() {
                Some(first) => {
                    if let Some(extended_start) = extended_start {
                        first.ebr = extended_start;
                    }
                }
                None => {
                    self.extended = None;
                    text.push_str(", and the extended partition that held it");
                }
            }
        }
        Ok(Step {
            description: text,
            warnings: vec![
                "Its files stay on the disk until something overwrites them: Undo, or the \
                 lost-partition search, can bring it back."
                    .into(),
            ],
        })
    }

    // ------------------------------------------------------------ flags, type, name

    fn set_active(&mut self, start: u64, active: bool) -> Outcome {
        let style = self.style_or_explain()?;
        let index = self.find(start)?;
        let what = self.describe(&self.parts[index]);
        match style {
            Style::Mbr => {
                if self.parts[index].logical {
                    return Err("Only primary partitions can be active.".into());
                }
                for p in &mut self.parts {
                    p.active = false;
                }
                self.parts[index].active = active;
                Ok(step(if active {
                    format!("Make {what} the active partition")
                } else {
                    format!("Make {what} inactive")
                }))
            }
            Style::Gpt => {
                let p = &mut self.parts[index];
                if active {
                    p.attributes |= GPT_BOOTABLE;
                } else {
                    p.attributes &= !GPT_BOOTABLE;
                }
                Ok(step(format!(
                    "{} {what} as legacy BIOS bootable",
                    if active { "Mark" } else { "Unmark" }
                )))
            }
        }
    }

    fn set_hidden(&mut self, start: u64, hidden: bool) -> Outcome {
        let style = self.style_or_explain()?;
        let index = self.find(start)?;
        let what = self.describe(&self.parts[index]);
        let p = &mut self.parts[index];
        match style {
            Style::Mbr => {
                let changed = if hidden {
                    hide_mbr(p.mbr_type)
                } else {
                    unhide_mbr(p.mbr_type)
                };
                p.mbr_type = changed.ok_or(if hidden {
                    "Only FAT and NTFS partitions can be hidden on an MBR disk (or it already is)."
                } else {
                    "That partition isn't hidden."
                })?;
            }
            Style::Gpt => {
                if hidden {
                    p.attributes |= GPT_HIDDEN;
                } else {
                    p.attributes &= !GPT_HIDDEN;
                }
            }
        }
        Ok(Step {
            description: format!("{} {what}", if hidden { "Hide" } else { "Unhide" }),
            warnings: if hidden {
                vec!["Windows won't give it a drive letter or show it while it's hidden.".into()]
            } else {
                Vec::new()
            },
        })
    }

    fn set_type(&mut self, start: u64, type_code: &str) -> Outcome {
        let style = self.style_or_explain()?;
        let index = self.find(start)?;
        let what = self.describe(&self.parts[index]);
        let name = match style {
            Style::Mbr => {
                let code = mbr_type(type_code)?;
                self.parts[index].mbr_type = code;
                format!("{} (0x{code:02X})", crate::mbr::type_name(code))
            }
            Style::Gpt => {
                let guid = gpt_type(type_code)?;
                self.parts[index].gpt_type = guid;
                crate::gpt::type_name(&crate::gpt::guid(&guid)).to_owned()
            }
        };
        Ok(Step {
            description: format!("Change the type of {what} to {name}"),
            warnings: vec![
                "The type tells systems what's in a partition; it doesn't change the data.".into(),
            ],
        })
    }

    fn set_name(&mut self, start: u64, name: &str) -> Outcome {
        if self.style_or_explain()? != Style::Gpt {
            return Err("Only GPT partitions have names.".into());
        }
        let index = self.find(start)?;
        let what = self.describe(&self.parts[index]);
        self.parts[index].name = check_name(name)?;
        Ok(step(format!("Name {what} \"{}\"", name.trim())))
    }

    // ------------------------------------------------------------ convert

    fn convert(&mut self, style: Style, disk: &Disk) -> Outcome {
        let from = self.style_or_explain()?;
        if from == style {
            return Err(format!("The disk is already {}.", style.name()));
        }
        match style {
            Style::Gpt => self.convert_to_gpt(disk),
            Style::Mbr => self.convert_to_mbr(),
        }
    }

    fn convert_to_gpt(&mut self, disk: &Disk) -> Outcome {
        self.entry_count = GPT_ENTRIES;
        self.entry_size = GPT_ENTRY_BYTES;
        let (first, last) = self.usable();
        let mut parts = self.parts.clone();
        parts.sort_by_key(|p| p.start);
        for p in &parts {
            if p.start < first {
                return Err(format!(
                    "GPT needs sectors 1–{} for itself, but {} starts at sector {}.",
                    first - 1,
                    self.describe(p),
                    p.start
                ));
            }
            if p.end() - 1 > last {
                return Err(format!(
                    "GPT keeps a copy of itself in the disk's last {} sectors, where {} ends. \
                     Shrink it by 1 MiB first.",
                    self.disk_sectors - 1 - last,
                    self.describe(p)
                ));
            }
        }
        let mut converted = Vec::new();
        for (slot, p) in parts.iter().enumerate() {
            let mut gpt = Part::blank(p.start, p.sectors);
            gpt.slot = slot;
            gpt.fs = p.fs;
            gpt.gpt_type = mbr_to_gpt(p.mbr_type).ok_or_else(|| {
                format!(
                    "{} has type 0x{:02X}, which has no GPT equivalent DataReco knows.",
                    self.describe(p),
                    p.mbr_type
                )
            })?;
            if unhide_mbr(p.mbr_type).is_some() {
                gpt.attributes |= GPT_HIDDEN;
            }
            if p.active {
                gpt.attributes |= GPT_BOOTABLE;
            }
            gpt.guid = new_guid();
            gpt.raw = vec![0; GPT_ENTRY_BYTES as usize];
            if types::is_basic_data(&gpt.gpt_type) {
                gpt.name = "Basic data partition".into();
            }
            converted.push(gpt);
        }
        let mut warnings = vec![
            "Partitions keep their place and data, but their numbers follow disk order now."
                .to_owned(),
            "A computer that starts from this disk the old (BIOS) way won't start from it as GPT."
                .to_owned(),
        ];
        // Sectors 1–33 (and the end) hold nothing on most disks, but some
        // BIOS boot loaders live there.
        let front = read(
            disk,
            u64::from(self.sector_size),
            (first - 1) * u64::from(self.sector_size),
        )
        .map_err(|e| e.to_string())?;
        if front.iter().any(|&b| b != 0) {
            warnings.push(format!(
                "Sectors 1–{} hold data (often a boot loader); GPT writes over them. Undo puts \
                 them back.",
                first - 1
            ));
        }
        let tail = self.disk_sectors - 1 - last;
        let back = read(
            disk,
            (last + 1) * u64::from(self.sector_size),
            tail * u64::from(self.sector_size),
        )
        .map_err(|e| e.to_string())?;
        if back.iter().any(|&b| b != 0) {
            warnings.push(format!(
                "The disk's last {tail} sectors hold data (some RAID setups keep their \
                 information there); GPT writes over them. Undo puts them back."
            ));
        }
        self.style = Some(Style::Gpt);
        self.parts = converted;
        self.extended = None;
        self.disk_guid = new_guid();
        Ok(Step {
            description: "Convert the disk to GPT, keeping every partition".to_owned(),
            warnings,
        })
    }

    fn convert_to_mbr(&mut self) -> Outcome {
        let mut warnings = Vec::new();
        let mut parts = self.parts.clone();
        parts.sort_by_key(|p| p.start);
        let mut kept = Vec::new();
        for p in &parts {
            if types::is_msr(&p.gpt_type) {
                warnings.push(format!(
                    "The Microsoft reserved partition ({}) is dropped: it holds no files and MBR \
                     disks don't use one.",
                    self.size(p.sectors)
                ));
                continue;
            }
            let code = gpt_to_mbr(&p.gpt_type, p.fs)
                .ok_or_else(|| format!("{} has a type MBR disks don't have.", self.describe(p)))?;
            if p.start > u64::from(u32::MAX) || p.sectors > u64::from(u32::MAX) {
                return Err(format!(
                    "{} is past the first {}, which is all an MBR can reach.",
                    self.describe(p),
                    self.size(MBR_SECTOR_LIMIT)
                ));
            }
            let mut mbr = Part::blank(p.start, p.sectors);
            mbr.mbr_type = if p.attributes & (1 << 62) != 0 {
                hide_mbr(code).unwrap_or(code)
            } else {
                code
            };
            mbr.fs = p.fs;
            mbr.active = p.attributes & GPT_BOOTABLE != 0 && !kept.iter().any(|k: &Part| k.active);
            kept.push(mbr);
        }
        if kept.len() > 4 {
            return Err(format!(
                "The disk has {} partitions, and an MBR holds four.",
                kept.len()
            ));
        }
        for (slot, p) in kept.iter_mut().enumerate() {
            p.slot = slot;
        }
        warnings.push(
            "A computer that starts from this disk the UEFI way won't start from it as MBR.".into(),
        );
        // The old GPT headers go, so nothing mistakes the disk for GPT.
        self.clear = vec![1, self.disk_sectors - 1];
        self.style = Some(Style::Mbr);
        self.parts = kept;
        self.extended = None;
        self.mbr_signature = new_signature();
        Ok(Step {
            description: "Convert the disk to MBR, keeping every partition".to_owned(),
            warnings,
        })
    }

    // ------------------------------------------------------------ move

    /// Points the partition at `start` to `new_start` instead, keeping its
    /// size. Its new place must be free (or its own old place); logical
    /// partitions stay inside the extended one, with room for their boot
    /// records.
    pub(crate) fn move_partition(&mut self, start: u64, new_start: u64) -> Outcome {
        let style = self.style_or_explain()?;
        let index = self.find(start)?;
        if start == new_start {
            return Err("It already starts there.".into());
        }
        let part = self.parts[index].clone();
        let what = self.describe(&part);
        let new_end = new_start
            .checked_add(part.sectors)
            .filter(|&end| end <= self.disk_sectors)
            .ok_or("That goes past the end of the disk.")?;
        if let Some(other) = self
            .parts
            .iter()
            .enumerate()
            .find(|(i, p)| *i != index && new_start < p.end() && new_end > p.start)
            .map(|(_, p)| p)
        {
            return Err(format!("There it would overlap {}.", self.describe(other)));
        }
        match style {
            Style::Gpt => {
                let (first, last) = self.usable();
                if new_start < first || new_end - 1 > last {
                    return Err(format!(
                        "Partitions on this disk must be within sectors {first}–{last}."
                    ));
                }
            }
            Style::Mbr if !part.logical => {
                if new_start == 0 || new_start > u64::from(u32::MAX) {
                    return Err("An MBR table can't describe that place.".into());
                }
                if self
                    .extended
                    .as_ref()
                    .is_some_and(|e| new_start < e.end() && new_end > e.start)
                {
                    return Err("That's inside the extended partition.".into());
                }
            }
            Style::Mbr => {
                let e = self
                    .extended
                    .clone()
                    .ok_or("There's no extended partition.")?;
                if new_start <= e.start || new_end > e.end() {
                    return Err("Logical partitions move within the extended partition.".into());
                }
                let others: Vec<Part> = self
                    .logicals()
                    .into_iter()
                    .filter(|l| l.start != start)
                    .cloned()
                    .collect();
                let previous_end = others
                    .iter()
                    .map(Part::end)
                    .filter(|&e| e <= new_start)
                    .max();
                let ebr = match previous_end {
                    None => e.start,
                    Some(end) => end.max(new_start.saturating_sub(self.align())),
                };
                if ebr >= new_start {
                    return Err("There's no free sector before it for its boot record.".into());
                }
                self.parts[index].ebr = ebr;
                // The one after it needs its boot record clear of it.
                if let Some(next) = others
                    .iter()
                    .filter(|l| l.start >= new_end)
                    .min_by_key(|l| l.start)
                    && next.ebr < new_end
                {
                    if new_end >= next.start {
                        return Err(
                            "There's no free sector left for the next partition's boot record."
                                .into(),
                        );
                    }
                    if let Some(p) = self
                        .parts
                        .iter_mut()
                        .find(|p| p.logical && p.start == next.start)
                    {
                        p.ebr = new_end;
                    }
                }
            }
        }
        self.parts[index].start = new_start;
        self.first_logical_at_extended_start();
        let distance = new_start.abs_diff(start);
        Ok(Step {
            description: format!(
                "Move {what} {} {} (to sector {new_start})",
                self.size(distance),
                if new_start < start {
                    "towards the start"
                } else {
                    "towards the end"
                }
            ),
            warnings: vec![
                "Its data is copied to the new place first; if the power goes, the move carries on \
                 from its journal."
                    .into(),
            ],
        })
    }

    /// The first logical partition's boot record is the extended
    /// partition's first sector.
    fn first_logical_at_extended_start(&mut self) {
        let Some(e) = self.extended.clone() else {
            return;
        };
        let first = self.logicals().first().map(|l| l.start);
        if let Some(first) = first
            && let Some(p) = self
                .parts
                .iter_mut()
                .find(|p| p.logical && p.start == first)
        {
            p.ebr = e.start;
        }
    }

    // ------------------------------------------------------------ primary ↔ logical

    fn make_logical(&mut self, start: u64) -> Outcome {
        if self.style_or_explain()? != Style::Mbr {
            return Err("Only MBR disks have logical partitions.".into());
        }
        let index = self.find(start)?;
        let part = self.parts[index].clone();
        if part.logical {
            return Err("It's already a logical partition.".into());
        }
        if part.active {
            return Err("The active partition must stay primary.".into());
        }
        let what = self.describe(&part);
        let primary_between = |from: u64, to: u64| {
            self.parts
                .iter()
                .any(|p| !p.logical && p.start != start && p.start < to && p.end() > from)
        };
        let previous_end = self
            .parts
            .iter()
            .filter(|p| !p.logical && p.start != start)
            .map(Part::end)
            .filter(|&e| e <= start)
            .max()
            .unwrap_or(1)
            .max(1);
        let no_room =
            "There's no free sector before it for the boot record a logical partition needs.";
        let ebr = match self.extended.clone() {
            None => {
                // A new extended partition around it, in the entry it frees.
                let ebr = previous_end.max(start.saturating_sub(self.align()));
                if ebr >= start {
                    return Err(no_room.into());
                }
                self.extended = Some(Extended {
                    start: ebr,
                    sectors: part.end() - ebr,
                    slot: part.slot,
                    mbr_type: 0x0F,
                });
                ebr
            }
            Some(e) if start >= e.end() => {
                // Right after the extended partition, which grows to hold it.
                if primary_between(e.end(), start) {
                    return Err(
                        "Only a partition right next to the extended one can become logical."
                            .into(),
                    );
                }
                let ebr = self.logicals().last().map_or(e.start, |l| l.end());
                if ebr >= start {
                    return Err(no_room.into());
                }
                if let Some(x) = self.extended.as_mut() {
                    x.sectors = part.end() - x.start;
                }
                ebr
            }
            Some(e) if part.end() <= e.start => {
                // Right before it: the extended partition grows down to a
                // boot record in front of it. The logical partition that was
                // first keeps its boot record at the old start, which stays
                // free.
                if primary_between(part.end(), e.start) {
                    return Err(
                        "Only a partition right next to the extended one can become logical."
                            .into(),
                    );
                }
                let ebr = previous_end.max(start.saturating_sub(self.align()));
                if ebr >= start {
                    return Err(no_room.into());
                }
                if let Some(x) = self.extended.as_mut() {
                    x.sectors = x.end() - ebr;
                    x.start = ebr;
                }
                ebr
            }
            Some(_) => return Err("It overlaps the extended partition.".into()),
        };
        let p = &mut self.parts[index];
        p.logical = true;
        p.ebr = ebr;
        Ok(step(format!("Make {what} a logical partition")))
    }

    fn make_primary(&mut self, start: u64) -> Outcome {
        if self.style_or_explain()? != Style::Mbr {
            return Err("Only MBR disks have logical partitions.".into());
        }
        let index = self.find(start)?;
        if !self.parts[index].logical {
            return Err("It's already a primary partition.".into());
        }
        let what = self.describe(&self.parts[index]);
        let logicals: Vec<u64> = self.logicals().iter().map(|l| l.start).collect();
        let e = self
            .extended
            .clone()
            .ok_or("There's no extended partition.")?;
        if logicals.len() == 1 {
            // The only one: the extended partition goes, and it takes its entry.
            self.extended = None;
            let p = &mut self.parts[index];
            p.logical = false;
            p.slot = e.slot;
            return Ok(step(format!(
                "Make {what} a primary partition (the extended partition that held it goes)"
            )));
        }
        let slot = self.free_slot().ok_or(
            "All four MBR entries are in use, so there's no room for another primary partition.",
        )?;
        if logicals.last() == Some(&start) {
            let ebr = self.parts[index].ebr;
            let x = self.extended.as_mut().expect("checked above");
            x.sectors = ebr - x.start;
        } else if logicals.first() == Some(&start) {
            let next = logicals[1];
            let next_ebr = self
                .parts
                .iter()
                .find(|p| p.start == next)
                .map_or(next, |p| p.ebr);
            let x = self.extended.as_mut().expect("checked above");
            let end = x.end();
            x.start = next_ebr;
            x.sectors = end - next_ebr;
        } else {
            return Err(
                "Only the first or last logical partition can become primary: the others \
                        sit between logical ones."
                    .into(),
            );
        }
        let p = &mut self.parts[index];
        p.logical = false;
        p.slot = slot;
        Ok(step(format!("Make {what} a primary partition")))
    }
}

fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.encode_utf16().count() > GPT_NAME_UNITS {
        return Err(format!(
            "A GPT partition name holds at most {GPT_NAME_UNITS} characters."
        ));
    }
    Ok(name.to_owned())
}
