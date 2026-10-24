//! Copying partitions onto other disks from the Disk view: one partition
//! into free space on another disk, or every partition of a disk (used
//! space only; Windows' own disk included, which makes a copy Windows
//! starts from).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use datareco_clone::{
    CloneError, CloneProgress, Cloner, Job, Prepared, Reading, estimate, plan_reading, prepare,
    runs_windows,
};
use datareco_io::{
    CopySource, DeviceList, Disk, DiskTarget, DiskTargetCheck, SectorTarget, SourceSpec,
    check_disk_target, list_devices,
};
use datareco_partitions::edit::{
    DiskCopy, DiskCopyOptions, PlacedPart, default_backup_dir, plan_disk_copy, plan_partition_copy,
};
use datareco_partitions::{SegmentKind, TableKind, layout, read_table};
use serde::Serialize;
use tauri::State;

use crate::copies::Copies;
use crate::files::{FileSystems, Recoveries};
use crate::health::DiskTests;
use crate::partitions::Searches;
use crate::relocations::Moves;
use crate::tables::{Busy, check_not_busy};
use crate::volumes::Wipes;
use crate::{CommandError, OpenSources};

/// How long each step copies before reporting back.
const STEP: Duration = Duration::from_millis(400);

impl From<CloneError> for CommandError {
    fn from(error: CloneError) -> Self {
        match error {
            CloneError::Io(error) => error.into(),
            CloneError::Refused(message) => CommandError::new("refused", message),
            other => CommandError::new("io", other.to_string()),
        }
    }
}

fn refused(message: impl Into<String>) -> CommandError {
    CommandError::new("refused", message)
}

/// A copy under way: the source held still, the target open.
struct Session {
    cloner: Cloner,
    prepared: Prepared,
    target: Box<dyn SectorTarget>,
    target_id: String,
}

/// Copies running, by source id.
#[derive(Default)]
pub struct Clones(Mutex<HashMap<String, Session>>);

impl Clones {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The disks being copied, and those being copied onto.
    pub fn sources_in_use(&self) -> Vec<String> {
        self.lock()
            .iter()
            .flat_map(|(id, session)| [id.clone(), session.target_id.clone()])
            .collect()
    }
}

/// A disk a copy could go onto.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneTarget {
    number: u32,
    name: String,
    model: String,
    size: u64,
    sector_size: u32,
    /// Why it can't be copied onto, if it can't.
    problem: Option<String>,
}

/// The disks a copy of this source could go onto, and whether it's the
/// disk Windows runs from.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CloneInfo {
    windows: bool,
    targets: Vec<CloneTarget>,
}

fn source_spec(id: &str) -> Result<SourceSpec, CommandError> {
    let spec = SourceSpec::parse(id);
    if let SourceSpec::Volume(_) = spec {
        return Err(refused("Open the disk this volume is on to copy it."));
    }
    Ok(spec)
}

#[tauri::command(async)]
pub fn clone_info(id: String) -> Result<CloneInfo, CommandError> {
    let spec = source_spec(&id)?;
    let devices = list_devices()?;
    let source = Disk::open(&spec)?;
    let targets = devices
        .disks
        .iter()
        .filter(|d| !devices.disks_of_source(&spec).contains(&d.number))
        .map(|d| {
            let check = target_check(&spec, d.number, 0, source.sector_size(), &devices);
            let problem = match check {
                Err(e) => Some(e.message),
                Ok(_) if d.sector_size != source.sector_size() => Some(format!(
                    "It has {}-byte sectors and this disk {}-byte ones.",
                    d.sector_size,
                    source.sector_size()
                )),
                Ok(_) => None,
            };
            CloneTarget {
                number: d.number,
                name: SourceSpec::PhysicalDisk(d.number).display_name(),
                model: d.model.clone(),
                size: d.size,
                sector_size: d.sector_size,
                problem,
            }
        })
        .collect();
    Ok(CloneInfo {
        windows: runs_windows(&spec, &devices),
        targets,
    })
}

/// The target disk's checks for a copy needing `needed` bytes: what to type
/// and the volumes closed, or why it can't be. A disk holding the source
/// image file or the table backups is refused.
fn target_check(
    spec: &SourceSpec,
    target: u32,
    needed: u64,
    sector_size: u32,
    devices: &DeviceList,
) -> Result<DiskTargetCheck, CommandError> {
    let backups = default_backup_dir();
    let mut keep = vec![backups.as_path()];
    if let SourceSpec::Image(path) = spec {
        keep.push(path);
    }
    Ok(check_disk_target(
        target,
        &CopySource {
            spec,
            size: needed,
            sector_size,
        },
        &keep,
        devices,
    )?)
}

// ---------------------------------------------------------------- whole disks

/// What copying the disk onto `target` would do.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskCopyPreview {
    copy: DiskCopy,
    /// Bytes it would copy (used space only).
    bytes: u64,
    reading: Reading,
    windows: bool,
    keep_ids: bool,
    /// What to type, and the target's volumes closed and erased.
    confirmation: Option<String>,
    erasing: Vec<String>,
    /// Why it can't be done, if it can't.
    problem: Option<String>,
}

fn plan_disk(
    spec: &SourceSpec,
    source: &Disk,
    target: u32,
    keep_ids: Option<bool>,
    space_after: Option<u64>,
    devices: &DeviceList,
) -> Result<(DiskCopy, DiskCopyOptions, bool), CommandError> {
    let disk = devices
        .disks
        .iter()
        .find(|d| d.number == target)
        .ok_or_else(|| refused("That disk isn't there any more."))?;
    let windows = runs_windows(spec, devices);
    let options = DiskCopyOptions {
        keep_ids: keep_ids.unwrap_or(windows),
        // Windows on an MBR disk finds its partitions by where they start.
        same_places: windows && read_table(source)?.kind == TableKind::Mbr,
        space_after,
    };
    let copy = plan_disk_copy(
        source,
        &SourceSpec::PhysicalDisk(target).display_name(),
        disk.size / u64::from(disk.sector_size),
        disk.sector_size,
        &options,
    )?;
    Ok((copy, options, windows))
}

#[tauri::command(async)]
pub fn preview_disk_copy(
    id: String,
    target: u32,
    keep_ids: Option<bool>,
    space_after: Option<u64>,
) -> Result<DiskCopyPreview, CommandError> {
    let spec = source_spec(&id)?;
    let devices = list_devices()?;
    let source = Arc::new(Disk::open(&spec)?);
    let (copy, options, windows) =
        plan_disk(&spec, &source, target, keep_ids, space_after, &devices)?;
    let reading = plan_reading(&spec, &source, &copy.parts, &devices);
    let needed = copy.needed_sectors * u64::from(source.sector_size());
    let (confirmation, erasing, refusal) =
        match target_check(&spec, target, needed, source.sector_size(), &devices) {
            Ok(check) => (Some(check.confirmation), check.volumes, None),
            Err(e) => (None, Vec::new(), Some(e.message)),
        };
    let problem = copy
        .blocked
        .clone()
        .or_else(|| copy.problem.clone())
        .or_else(|| reading.problem.clone())
        .or(refusal);
    Ok(DiskCopyPreview {
        bytes: estimate(&source, &copy.parts),
        copy,
        reading,
        windows,
        keep_ids: options.keep_ids,
        confirmation,
        erasing,
        problem,
    })
}

#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)] // Tauri hands over each piece of state
pub fn start_disk_copy(
    sources: State<'_, OpenSources>,
    file_systems: State<'_, FileSystems>,
    searches: State<'_, Searches>,
    recoveries: State<'_, Recoveries>,
    copies: State<'_, Copies>,
    tests: State<'_, DiskTests>,
    wipes: State<'_, Wipes>,
    moves: State<'_, Moves>,
    clones: State<'_, Clones>,
    id: String,
    target: u32,
    keep_ids: Option<bool>,
    space_after: Option<u64>,
    confirmation: String,
) -> Result<CloneProgress, CommandError> {
    let spec = source_spec(&id)?;
    let busy = Busy {
        searches,
        recoveries,
        copies,
        tests,
        wipes,
        moves,
        clones,
    };
    check_not_busy(&spec, &busy)?;
    let target_spec = SourceSpec::PhysicalDisk(target);
    check_not_busy(&target_spec, &busy)?;
    let devices = list_devices()?;
    let source = Arc::new(Disk::open(&spec)?);
    let (copy, _, windows) = plan_disk(&spec, &source, target, keep_ids, space_after, &devices)?;
    if let Some(reason) = copy.blocked.as_ref().or(copy.problem.as_ref()) {
        return Err(refused(reason.clone()));
    }
    let sector_size = source.sector_size();
    let check = target_check(
        &spec,
        target,
        copy.needed_sectors * u64::from(sector_size),
        sector_size,
        &devices,
    )?;
    // Before closing anything on the source.
    if !confirmation
        .trim()
        .eq_ignore_ascii_case(&check.confirmation)
    {
        return Err(refused(format!(
            "Type {} to confirm that everything on {} may be erased.",
            check.confirmation, check.name
        )));
    }
    let prepared = prepare(&spec, &source, &copy.parts, &devices)?;
    let out = DiskTarget::open(&check, &confirmation)?;
    let allocations = prepared.allocations(&copy.parts, sector_size);
    let what = if windows {
        format!("Move Windows from {} to {}", source.name(), check.name)
    } else {
        format!("Copy {} onto {}", source.name(), check.name)
    };
    let job = Job::disk(&copy, &allocations, sector_size, what)?;
    begin(
        &busy.clones,
        &sources,
        &file_systems,
        &id,
        Session {
            cloner: Cloner::new(job),
            prepared,
            target: Box::new(out),
            target_id: target_spec.id(),
        },
    )
}

fn begin(
    clones: &Clones,
    sources: &OpenSources,
    file_systems: &FileSystems,
    id: &str,
    session: Session,
) -> Result<CloneProgress, CommandError> {
    let key = SourceSpec::parse(id).id();
    clones.lock().insert(key.clone(), session);
    sources.clear();
    file_systems.clear();
    step(clones, &key)
}

// ---------------------------------------------------------------- one partition

/// Free space on the target disk, from a 1 MiB boundary.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FreeSpace {
    start_lba: u64,
    sectors: u64,
    /// Inside the extended partition: the copy would be a logical partition.
    logical: bool,
}

/// What copying one partition into free space on `target` would do.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionCopyPreview {
    description: Option<String>,
    part: Option<PlacedPart>,
    /// Where on the target it would start.
    at: Option<u64>,
    bytes: u64,
    reading: Reading,
    /// The target's free spaces big enough for it.
    free: Vec<FreeSpace>,
    confirmation: Option<String>,
    /// The target's volumes, closed while it copies.
    closing: Vec<String>,
    problem: Option<String>,
}

fn free_spaces(disk: &Disk, sectors: u64) -> Result<Vec<FreeSpace>, CommandError> {
    let table = read_table(disk)?;
    let align = (1 << 20) / u64::from(disk.sector_size()).max(1);
    Ok(layout(&table)
        .iter()
        .filter(|s| s.kind == SegmentKind::Free)
        .filter_map(|s| {
            let start = s.start_lba.div_ceil(align) * align;
            let room = s.end_lba().saturating_sub(start);
            (room >= sectors).then_some(FreeSpace {
                start_lba: start,
                sectors: room,
                logical: s.logical,
            })
        })
        .collect())
}

#[tauri::command(async)]
pub fn preview_partition_copy(
    id: String,
    start_lba: u64,
    target: u32,
    at: Option<u64>,
) -> Result<PartitionCopyPreview, CommandError> {
    let spec = source_spec(&id)?;
    let devices = list_devices()?;
    let source = Arc::new(Disk::open(&spec)?);
    let target_disk = Disk::open(&SourceSpec::PhysicalDisk(target))?;
    let sectors = read_table(&source)?
        .partitions
        .iter()
        .find(|p| p.start_lba == start_lba)
        .map(|p| p.sectors)
        .ok_or_else(|| refused("That partition isn't there any more."))?;
    let free = free_spaces(&target_disk, sectors)?;
    let mut preview = PartitionCopyPreview {
        description: None,
        part: None,
        at: at.or_else(|| free.first().map(|f| f.start_lba)),
        bytes: 0,
        reading: Reading::default(),
        free,
        confirmation: None,
        closing: Vec::new(),
        problem: None,
    };
    let Some(place) = preview.at else {
        preview.problem = Some(format!(
            "{} has no free space big enough for it.",
            target_disk.name()
        ));
        return Ok(preview);
    };
    let copy = plan_partition_copy(&source, start_lba, &target_disk, place)?;
    preview.description = copy.plan.steps.first().map(|s| s.description.clone());
    preview.problem = copy
        .plan
        .blocked
        .clone()
        .or_else(|| copy.plan.error.as_ref().map(|e| e.message.clone()));
    if let Some(part) = &copy.part {
        preview.bytes = estimate(&source, std::slice::from_ref(part));
        preview.reading = plan_reading(&spec, &source, std::slice::from_ref(part), &devices);
        preview.problem = preview.problem.or_else(|| preview.reading.problem.clone());
        let bytes = part.sectors * u64::from(source.sector_size());
        match target_check(&spec, target, bytes, source.sector_size(), &devices) {
            Ok(check) => {
                preview.confirmation = Some(check.confirmation);
                preview.closing = check.volumes;
            }
            Err(e) => preview.problem = preview.problem.or(Some(e.message)),
        }
    }
    preview.part = copy.part;
    Ok(preview)
}

#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)] // Tauri hands over each piece of state
pub fn start_partition_copy(
    sources: State<'_, OpenSources>,
    file_systems: State<'_, FileSystems>,
    searches: State<'_, Searches>,
    recoveries: State<'_, Recoveries>,
    copies: State<'_, Copies>,
    tests: State<'_, DiskTests>,
    wipes: State<'_, Wipes>,
    moves: State<'_, Moves>,
    clones: State<'_, Clones>,
    id: String,
    start_lba: u64,
    target: u32,
    at: u64,
    confirmation: String,
) -> Result<CloneProgress, CommandError> {
    let spec = source_spec(&id)?;
    let busy = Busy {
        searches,
        recoveries,
        copies,
        tests,
        wipes,
        moves,
        clones,
    };
    check_not_busy(&spec, &busy)?;
    let target_spec = SourceSpec::PhysicalDisk(target);
    check_not_busy(&target_spec, &busy)?;
    let devices = list_devices()?;
    let source = Arc::new(Disk::open(&spec)?);
    let copy = plan_partition_copy(&source, start_lba, &Disk::open(&target_spec)?, at)?;
    let part = copy
        .part
        .clone()
        .ok_or_else(|| refused("There's nothing to copy."))?;
    let sector_size = source.sector_size();
    let check = target_check(
        &spec,
        target,
        part.sectors * u64::from(sector_size),
        sector_size,
        &devices,
    )?;
    if !confirmation
        .trim()
        .eq_ignore_ascii_case(&check.confirmation)
    {
        return Err(refused(format!("Type {} to confirm.", check.confirmation)));
    }
    let parts = std::slice::from_ref(&part);
    let prepared = prepare(&spec, &source, parts, &devices)?;
    let out = DiskTarget::open(&check, &confirmation)?;
    let used = prepared.allocations(parts, sector_size).pop().flatten();
    // The target's table sectors, read afresh now nothing else changes them.
    let fresh = Disk::open(&target_spec)?;
    let job = Job::partition(
        &copy,
        &fresh,
        &target_spec.id(),
        &default_backup_dir(),
        used.as_ref(),
    )?;
    begin(
        &busy.clones,
        &sources,
        &file_systems,
        &id,
        Session {
            cloner: Cloner::new(job),
            prepared,
            target: Box::new(out),
            target_id: target_spec.id(),
        },
    )
}

// ---------------------------------------------------------------- running

fn step(clones: &Clones, key: &str) -> Result<CloneProgress, CommandError> {
    let mut all = clones.lock();
    let session = all
        .get_mut(key)
        .ok_or_else(|| CommandError::new("not_found", "No copy of this disk is running."))?;
    let Session {
        cloner,
        prepared,
        target,
        ..
    } = session;
    match cloner.step(&mut prepared.sources, target.as_mut(), STEP) {
        Ok(progress) => {
            if progress.finished {
                // Closes the target (Windows reads its new table), opens the
                // source's volumes again, deletes the snapshots.
                all.remove(key);
            }
            Ok(progress)
        }
        Err(e) => {
            all.remove(key);
            Err(CommandError::new(
                "io",
                format!("The copy stopped: {e}. The source wasn't changed; start it again."),
            ))
        }
    }
}

#[tauri::command(async)]
pub fn continue_clone(
    clones: State<'_, Clones>,
    id: String,
) -> Result<CloneProgress, CommandError> {
    step(&clones, &SourceSpec::parse(&id).id())
}

/// Stops a copy. The target keeps what was copied so far, but no table
/// points at it; the source wasn't changed. Off the main thread: deleting a
/// snapshot takes Windows a few seconds.
#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value)] // Tauri hands over owned state
pub fn stop_clone(clones: State<'_, Clones>, id: String) -> bool {
    clones.lock().remove(&SourceSpec::parse(&id).id()).is_some()
}
