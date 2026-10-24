//! Working out what a list of operations would do, and showing the disk as
//! it would read afterwards.

use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;

use datareco_io::{Disk, Result, SectorSource};
use serde::Serialize;

use super::backup::SectorWrite;
use super::{EditableTable, Operation, Step, Style};

/// Where a list of operations stopped, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepError {
    /// Index of the operation that can't be done.
    pub step: usize,
    pub message: String,
}

/// What a list of operations would change.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// Why the disk's table can't be changed at all, if it can't.
    pub blocked: Option<String>,
    /// The table style now, and after the operations.
    pub style_before: Option<Style>,
    pub style: Option<Style>,
    /// One per operation done, in order.
    pub steps: Vec<Step>,
    /// The first operation that can't be done; the ones after it aren't tried.
    pub error: Option<StepError>,
    /// The sectors that would change (not shown to people).
    #[serde(skip)]
    pub writes: Vec<SectorWrite>,
    /// How many sectors would be written.
    pub sectors_written: u64,
}

impl Plan {
    /// Whether applying it would do everything asked, and something.
    pub fn is_ready(&self) -> bool {
        self.blocked.is_none() && self.error.is_none() && !self.writes.is_empty()
    }

    /// The steps' descriptions, for backups and logs.
    pub fn descriptions(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.description.clone()).collect()
    }
}

/// Works out what `ops` would do to the disk's table, without writing.
pub fn plan(disk: &Disk, ops: &[Operation]) -> Result<Plan> {
    let mut plan = Plan {
        blocked: None,
        style_before: None,
        style: None,
        steps: Vec::new(),
        error: None,
        writes: Vec::new(),
        sectors_written: 0,
    };
    let mut table = match EditableTable::load(disk)? {
        Ok(table) => table,
        Err(reason) => {
            plan.blocked = Some(reason);
            return Ok(plan);
        }
    };
    plan.style_before = table.style;
    for (index, op) in ops.iter().enumerate() {
        match table.apply(op, disk) {
            Ok(step) => plan.steps.push(step),
            Err(message) => {
                plan.error = Some(StepError {
                    step: index,
                    message,
                });
                break;
            }
        }
    }
    finish(plan, &table, disk)
}

/// Fills in what a changed table would write: only the sectors that would
/// really change (they're what gets backed up).
pub(super) fn finish(mut plan: Plan, table: &EditableTable, disk: &Disk) -> Result<Plan> {
    plan.style = table.style;
    let sector_size = u64::from(disk.sector_size());
    for write in table.encode() {
        let mut current = vec![0u8; write.data.len()];
        disk.read_at(write.lba * sector_size, &mut current)?;
        if current != write.data {
            plan.sectors_written += write.data.len() as u64 / sector_size;
            plan.writes.push(write);
        }
    }
    Ok(plan)
}

/// What moving the partition at `start_lba` so it starts at `new_start_lba`
/// does to the table. Only the table: the partition's data has to be moved
/// first (`datareco-relocate` does both, in that order).
pub fn plan_move(disk: &Disk, start_lba: u64, new_start_lba: u64) -> Result<Plan> {
    let mut plan = Plan {
        blocked: None,
        style_before: None,
        style: None,
        steps: Vec::new(),
        error: None,
        writes: Vec::new(),
        sectors_written: 0,
    };
    let mut table = match EditableTable::load(disk)? {
        Ok(table) => table,
        Err(reason) => {
            plan.blocked = Some(reason);
            return Ok(plan);
        }
    };
    plan.style_before = table.style;
    let mut changed = table.clone();
    match changed
        .move_partition(start_lba, new_start_lba)
        .and_then(|step| {
            changed
                .check_layout()
                .map(|()| step)
                .map_err(|p| format!("That would leave the table invalid: {p}"))
        }) {
        Ok(step) => {
            plan.steps.push(step);
            table = changed;
        }
        Err(message) => plan.error = Some(StepError { step: 0, message }),
    }
    finish(plan, &table, disk)
}

/// The disk as it would read once `writes` are written: the same reads,
/// with those sectors replaced. Pass it to `read_table` or `disk_map`.
pub fn preview(disk: Arc<Disk>, writes: &[SectorWrite]) -> Disk {
    let sector_size = disk.sector_size() as usize;
    let mut sectors = BTreeMap::new();
    for write in writes {
        for (i, chunk) in write.data.chunks(sector_size).enumerate() {
            sectors.insert(write.lba + i as u64, chunk.to_vec());
        }
    }
    let name = format!("{} (after the changes)", disk.name());
    Disk::new(name, Box::new(Overlay { disk, sectors }))
}

struct Overlay {
    disk: Arc<Disk>,
    sectors: BTreeMap<u64, Vec<u8>>,
}

impl SectorSource for Overlay {
    fn sector_size(&self) -> u32 {
        self.disk.sector_size()
    }

    fn size(&self) -> u64 {
        self.disk.size()
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        let sector_size = u64::from(self.disk.sector_size());
        let offset = lba * sector_size;
        // An image file may end partway through its last sector.
        let available = usize::try_from(self.disk.size().saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let (data, past_end) = buf.split_at_mut(available);
        self.disk.read_at(offset, data).map_err(io::Error::other)?;
        past_end.fill(0);
        let count = buf.len() as u64 / sector_size;
        for (&at, data) in self.sectors.range(lba..lba + count) {
            let offset = usize::try_from((at - lba) * sector_size).unwrap_or(0);
            buf[offset..offset + data.len()].copy_from_slice(data);
        }
        Ok(())
    }
}
