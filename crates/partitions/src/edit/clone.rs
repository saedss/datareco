//! Copying partitions onto another disk: one partition into free space
//! there, or every partition of a disk onto another disk. Only the tables:
//! the data is copied first (`datareco-clone` does both, in that order).

use datareco_io::{Disk, Result};
use serde::Serialize;

use super::backup::SectorWrite;
use super::guid::{new_guid, new_signature};
use super::ops::Step;
use super::plan::{Plan, StepError, finish};
use super::types::{gpt_to_mbr, is_basic_data, mbr_to_gpt};
use super::{EditableTable, Part, Style};
use crate::FsKind;
use crate::MBR_SECTOR_LIMIT;
use crate::table::human_size;

/// A partition on the source disk, and where its copy goes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacedPart {
    /// Its number on the source.
    pub number: u32,
    /// For people: "partition 2 (NTFS, 100.0 GiB)".
    pub what: String,
    pub from_lba: u64,
    pub to_lba: u64,
    pub sectors: u64,
    pub logical: bool,
    pub fs: Option<FsKind>,
}

/// What copying one partition into free space on another disk would do to
/// that disk's table.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionCopy {
    /// The target's table change (one step), as for any table change.
    pub plan: Plan,
    /// The partition and where its copy goes, when it can be done.
    pub part: Option<PlacedPart>,
}

/// Plans copying the partition starting at `source_start` on `source` to
/// start at `target_start` on `target`, in free space there. The copy gets
/// a new GPT partition GUID; MBR copies aren't made active.
pub fn plan_partition_copy(
    source: &Disk,
    source_start: u64,
    target: &Disk,
    target_start: u64,
) -> Result<PartitionCopy> {
    let mut plan = Plan {
        blocked: None,
        style_before: None,
        style: None,
        steps: Vec::new(),
        error: None,
        writes: Vec::new(),
        sectors_written: 0,
    };
    let refuse = |mut plan: Plan, reason: String| {
        plan.blocked = Some(reason);
        Ok(PartitionCopy { plan, part: None })
    };
    let from = match EditableTable::load(source)? {
        Ok(table) => table,
        Err(reason) => return refuse(plan, format!("{}: {reason}", source.name())),
    };
    let mut to = match EditableTable::load(target)? {
        Ok(table) => table,
        Err(reason) => return refuse(plan, format!("{}: {reason}", target.name())),
    };
    plan.style_before = to.style;
    if from.sector_size != to.sector_size {
        return refuse(plan, sector_size_problem(source, target));
    }
    let Some(style) = to.style else {
        return refuse(
            plan,
            format!(
                "{} has no partition table yet: initialize it first (MBR or GPT).",
                target.name()
            ),
        );
    };
    let Some(original) = from.parts.iter().find(|p| p.start == source_start) else {
        return refuse(
            plan,
            format!(
                "There's no partition starting at sector {source_start} on {}.",
                source.name()
            ),
        );
    };
    let what = from.describe(original);
    let mut changed = to.clone();
    if let Err(message) = changed.add_copy(&from, original, target_start, style) {
        plan.error = Some(StepError { step: 0, message });
        return Ok(PartitionCopy { plan, part: None });
    }
    to = changed;
    if let Err(problem) = to.check_layout() {
        plan.error = Some(StepError {
            step: 0,
            message: format!("That would leave the table invalid: {problem}"),
        });
        return Ok(PartitionCopy { plan, part: None });
    }
    let placed = to
        .parts
        .iter()
        .find(|p| p.start == target_start)
        .cloned()
        .unwrap_or_else(|| Part::blank(target_start, original.sectors));
    plan.steps.push(Step {
        description: format!(
            "Copy {what} from {} to sector {target_start} of {}{}",
            source.name(),
            target.name(),
            if placed.logical {
                ", as a logical partition"
            } else {
                ""
            }
        ),
        warnings: Vec::new(),
    });
    let part = PlacedPart {
        number: from.number(original),
        what,
        from_lba: source_start,
        to_lba: target_start,
        sectors: original.sectors,
        logical: placed.logical,
        fs: original.fs,
    };
    Ok(PartitionCopy {
        plan: finish(plan, &to, target)?,
        part: Some(part),
    })
}

fn sector_size_problem(source: &Disk, target: &Disk) -> String {
    format!(
        "{} has {}-byte sectors and {} has {}-byte sectors. File systems are made for one \
         sector size, so a copy wouldn't open on the other disk.",
        source.name(),
        source.sector_size(),
        target.name(),
        target.sector_size()
    )
}

impl EditableTable {
    /// Adds an entry for a copy of `original` (from `from`'s table) at
    /// `start`, converting its type to this table's style.
    fn add_copy(
        &mut self,
        from: &EditableTable,
        original: &Part,
        start: u64,
        style: Style,
    ) -> std::result::Result<(), String> {
        let sectors = original.sectors;
        let end = start
            .checked_add(sectors)
            .filter(|&end| end <= self.disk_sectors)
            .ok_or("The copy would go past the end of the disk.")?;
        let mut part = Part::blank(start, sectors);
        part.fs = original.fs;
        match style {
            Style::Gpt => {
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
                if from.style == Some(Style::Gpt) {
                    part.gpt_type = original.gpt_type;
                    part.attributes = original.attributes;
                    part.name.clone_from(&original.name);
                } else {
                    part.gpt_type = mbr_to_gpt(original.mbr_type).ok_or_else(|| {
                        format!(
                            "{} partitions (type 0x{:02X}) have no GPT type to become.",
                            crate::mbr::type_name(original.mbr_type),
                            original.mbr_type
                        )
                    })?;
                    if is_basic_data(&part.gpt_type) {
                        "Basic data partition".clone_into(&mut part.name);
                    }
                }
                part.guid = new_guid();
                part.raw = vec![0; self.entry_size as usize];
            }
            Style::Mbr => {
                part.mbr_type = if from.style == Some(Style::Mbr) {
                    original.mbr_type
                } else {
                    gpt_to_mbr(&original.gpt_type, original.fs).ok_or_else(|| {
                        format!(
                            "{} partitions only exist on GPT disks.",
                            crate::gpt::type_name(&crate::gpt::guid(&original.gpt_type))
                        )
                    })?
                };
                if start == 0 || start > u64::from(u32::MAX) || sectors > u64::from(u32::MAX) {
                    return Err(format!(
                        "An MBR table can't describe that: it reaches only the first {}.",
                        human_size(MBR_SECTOR_LIMIT * u64::from(self.sector_size))
                    ));
                }
                let inside = self
                    .extended
                    .as_ref()
                    .is_some_and(|e| start >= e.start && end <= e.end());
                if inside {
                    self.place_logical(&mut part)?;
                } else {
                    if self
                        .extended
                        .as_ref()
                        .is_some_and(|e| start < e.end() && end > e.start)
                    {
                        return Err(
                            "The copy would reach into the extended partition: put it wholly \
                             inside or outside it."
                                .into(),
                        );
                    }
                    self.check_free(start, end, false)?;
                    part.slot = self.free_slot().ok_or(
                        "An MBR disk holds at most four primary partitions, counting the \
                         extended one: put the copy inside the extended partition, or convert \
                         the disk to GPT.",
                    )?;
                }
            }
        }
        self.parts.push(part);
        Ok(())
    }
}

// ---------------------------------------------------------------- whole disks

/// How to lay a disk's partitions out on another disk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskCopyOptions {
    /// Keep the disk's IDs (GPT disk and partition GUIDs, the MBR disk
    /// signature), as Windows needs to start from the copy. Otherwise the
    /// copy gets new ones, so it can be used next to the original.
    pub keep_ids: bool,
    /// Keep every partition where it starts now: Windows on an MBR disk
    /// finds its partitions by where they start.
    pub same_places: bool,
    /// Leave the target's spare room right after this partition (by where
    /// it starts on the source), putting the ones after it at the disk's
    /// end, so that one can be grown into it later.
    pub space_after: Option<u64>,
}

/// What copying every partition of a disk onto another disk would do.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskCopy {
    /// Why it can't be done at all.
    pub blocked: Option<String>,
    /// Why it can't be done onto this disk (too small), and what would do.
    pub problem: Option<String>,
    pub style: Option<Style>,
    pub parts: Vec<PlacedPart>,
    /// The partitions were moved closer together to fit, or to leave the
    /// spare room where asked; otherwise each starts where it does now.
    pub packed: bool,
    pub keep_ids: bool,
    /// MBR disks: the sectors after sector 0, before the first partition,
    /// copied as they are (boot loaders keep code there).
    pub boot_sectors: u64,
    /// The smallest disk this layout fits, in sectors.
    pub needed_sectors: u64,
    pub warnings: Vec<String>,
    /// The target's new table (all of it: the target is written over).
    #[serde(skip)]
    pub writes: Vec<SectorWrite>,
}

/// One partition, or the extended partition with its logical ones, in disk
/// order.
enum Item {
    Part(usize),
    Extended(Vec<usize>),
}

/// Plans copying every partition of `source` onto a disk of
/// `target_sectors` sectors of `target_sector_size` bytes. Each partition
/// keeps its place when they all fit (and always with `same_places`);
/// otherwise they're moved together, 1 MiB apart.
pub fn plan_disk_copy(
    source: &Disk,
    target_name: &str,
    target_sectors: u64,
    target_sector_size: u32,
    options: &DiskCopyOptions,
) -> Result<DiskCopy> {
    let mut copy = DiskCopy {
        blocked: None,
        problem: None,
        style: None,
        parts: Vec::new(),
        packed: false,
        keep_ids: options.keep_ids,
        boot_sectors: 0,
        needed_sectors: 0,
        warnings: Vec::new(),
        writes: Vec::new(),
    };
    let original = match EditableTable::load(source)? {
        Ok(table) => table,
        Err(reason) => {
            copy.blocked = Some(reason);
            return Ok(copy);
        }
    };
    copy.style = original.style;
    if original.style.is_none() || original.parts.is_empty() {
        copy.blocked = Some(format!(
            "{} has no partitions to copy one by one. To copy it whole, use Copy (top right).",
            source.name()
        ));
        return Ok(copy);
    }
    if original.sector_size != target_sector_size {
        copy.blocked = Some(format!(
            "{} has {}-byte sectors and {target_name} has {target_sector_size}-byte sectors. \
             File systems are made for one sector size, so a copy wouldn't open on the other \
             disk.",
            source.name(),
            original.sector_size
        ));
        return Ok(copy);
    }
    let mut table = original.clone();
    table.disk_sectors = target_sectors;
    table.clear.clear();
    let fits_as_is = table.check_layout().is_ok();
    let pack = options.space_after.is_some() || !fits_as_is;
    if pack && options.same_places {
        copy.needed_sectors = needed(&original, &table, false);
        copy.problem = Some(format!(
            "The partitions have to stay where they are, and they don't fit on {target_name}: it \
             needs to hold {}. Shrink the last partition first (Resize), or pick a bigger disk.",
            human_size(copy.needed_sectors * u64::from(target_sector_size))
        ));
        return Ok(copy);
    }
    if pack {
        if let Err(problem) = table.pack(options.space_after) {
            copy.problem = Some(problem);
            return Ok(copy);
        }
        copy.packed = true;
    }
    copy.needed_sectors = needed(&original, &table, pack);
    if let Err(problem) = table.check_layout() {
        copy.problem = Some(if copy.needed_sectors > target_sectors {
            format!(
                "The partitions don't fit on {target_name}: it needs to hold at least {}. \
                 Shrink a partition first (Resize), or pick a bigger disk.",
                human_size(copy.needed_sectors * u64::from(target_sector_size))
            )
        } else {
            format!("The partitions can't be laid out on {target_name}: {problem}")
        });
        return Ok(copy);
    }
    if !options.keep_ids {
        table.mbr_signature = new_signature();
        table.disk_guid = new_guid();
        for part in &mut table.parts {
            part.guid = new_guid();
        }
    }
    // An MBR copy's old GPT backup header (the target's last sector) would
    // confuse some tools, unless a partition reaches it.
    let last = target_sectors.saturating_sub(1);
    if table.style == Some(Style::Mbr) && !table.parts.iter().any(|p| p.end() > last) {
        table.clear.push(last);
    }
    if table.style == Some(Style::Mbr) {
        let first = |t: &EditableTable| {
            t.parts
                .iter()
                .map(|p| if p.logical { p.ebr } else { p.start })
                .chain(t.extended.as_ref().map(|e| e.start))
                .min()
                .unwrap_or(1)
        };
        copy.boot_sectors = first(&original).min(first(&table)).saturating_sub(1);
    }
    for (old, new) in original.parts.iter().zip(&table.parts) {
        copy.parts.push(PlacedPart {
            number: original.number(old),
            what: original.describe(old),
            from_lba: old.start,
            to_lba: new.start,
            sectors: old.sectors,
            logical: old.logical,
            fs: old.fs,
        });
    }
    copy.parts.sort_by_key(|p| p.from_lba);
    // Where the spare room is: after the partition asked for, or at the end.
    let after = options.space_after.and_then(|start| {
        let index = original.parts.iter().position(|p| p.start == start)?;
        let end = table.parts[index].end();
        let next = table
            .parts
            .iter()
            .map(|p| if p.logical { p.ebr } else { p.start })
            .chain(table.extended.as_ref().map(|e| e.start))
            .filter(|&s| s >= end)
            .min()?;
        Some((original.describe(&original.parts[index]), next - end))
    });
    let (place, spare) = match after {
        Some((what, gap)) => (format!(", right after {what}"), gap),
        None => (
            ", at the end".to_owned(),
            target_sectors.saturating_sub(copy.needed_sectors),
        ),
    };
    if spare >= table.align() {
        copy.warnings.push(format!(
            "{} of {target_name} is left unused{place}; Resize can grow a partition into it \
             later.",
            human_size(spare * u64::from(target_sector_size)),
        ));
    }
    copy.writes = table.encode();
    Ok(copy)
}

/// The smallest disk, in sectors, that holds `table`'s partitions where
/// they are (GPT: with its backup copy after them).
fn needed(original: &EditableTable, table: &EditableTable, packed: bool) -> u64 {
    let source = if packed { table } else { original };
    let end = source
        .parts
        .iter()
        .map(Part::end)
        .chain(source.extended.as_ref().map(super::Extended::end))
        .max()
        .unwrap_or(0);
    match table.style {
        Some(Style::Gpt) => end + 1 + table.array_sectors(),
        _ => end,
    }
}

impl EditableTable {
    /// Moves the partitions together from the start of the disk, 1 MiB
    /// apart, in the order they're in. With `space_after`, the ones after
    /// that partition go to the end of the disk instead.
    fn pack(&mut self, space_after: Option<u64>) -> std::result::Result<(), String> {
        let align = self.align();
        let up = |v: u64| v.div_ceil(align) * align;
        let down = |v: u64| v / align * align;
        let (first, end) = match self.style {
            Some(Style::Gpt) => {
                let (first, last) = self.usable();
                (first, last + 1)
            }
            _ => (1, self.disk_sectors.min(MBR_SECTOR_LIMIT)),
        };
        // Disk order: primary partitions, and the extended one with its
        // logical partitions in order.
        let mut items: Vec<(u64, Item)> = self
            .parts
            .iter()
            .enumerate()
            .filter(|(_, p)| !p.logical)
            .map(|(i, p)| (p.start, Item::Part(i)))
            .collect();
        if let Some(e) = &self.extended {
            let mut logical: Vec<usize> = (0..self.parts.len())
                .filter(|&i| self.parts[i].logical)
                .collect();
            logical.sort_by_key(|&i| self.parts[i].start);
            items.push((e.start, Item::Extended(logical)));
        }
        items.sort_by_key(|(start, _)| *start);
        let split =
            match space_after {
                Some(start) => items
                    .iter()
                    .position(
                        |(_, item)| matches!(item, Item::Part(i) if self.parts[*i].start == start),
                    )
                    .ok_or(
                        "Spare room can go after a partition that isn't logical: pick another \
                         one.",
                    )?
                    + 1,
                None => items.len(),
            };
        // Forwards from the start…
        let mut cursor = first;
        for (_, item) in &items[..split] {
            match item {
                Item::Part(i) => {
                    let start = up(cursor);
                    self.parts[*i].start = start;
                    cursor = start + self.parts[*i].sectors;
                }
                Item::Extended(logical) => {
                    let e_start = up(cursor);
                    // An empty extended partition keeps 1 MiB.
                    cursor = e_start + if logical.is_empty() { align } else { 0 };
                    for (n, &i) in logical.iter().enumerate() {
                        let ebr = if n == 0 { e_start } else { up(cursor) };
                        self.parts[i].ebr = ebr;
                        self.parts[i].start = ebr + align;
                        cursor = self.parts[i].end();
                    }
                    if let Some(e) = &mut self.extended {
                        e.start = e_start;
                        e.sectors = cursor - e_start;
                    }
                }
            }
        }
        // …and backwards from the end.
        let mut back = down(end);
        for (_, item) in items[split..].iter().rev() {
            match item {
                Item::Part(i) => {
                    let start = down(back.saturating_sub(self.parts[*i].sectors));
                    self.parts[*i].start = start;
                    back = start;
                }
                Item::Extended(logical) => {
                    let e_end = back;
                    if logical.is_empty() {
                        back = down(back.saturating_sub(align));
                    }
                    for &i in logical.iter().rev() {
                        let start = down(back.saturating_sub(self.parts[i].sectors));
                        self.parts[i].start = start;
                        self.parts[i].ebr = start.saturating_sub(align);
                        back = self.parts[i].ebr;
                    }
                    if let Some(e) = &mut self.extended {
                        e.start = back;
                        e.sectors = e_end - back;
                    }
                }
            }
        }
        if split < items.len() && back < cursor {
            return Err(format!(
                "The partitions don't fit on the disk with spare room in the middle: they need \
                 {} more.",
                human_size((cursor - back) * u64::from(self.sector_size))
            ));
        }
        Ok(())
    }
}
