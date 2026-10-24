//! Moving partitions (with a journal to carry on after a power cut) and
//! resizing NTFS ones through Windows, from the Disk view.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use datareco_io::manage::{partition_size_limits, resize_with_windows};
use datareco_io::{
    Disk, DiskTarget, FileSource, ImageTarget, SectorTarget, SourceSpec, check_disk_for_changes,
    list_devices,
};
use datareco_partitions::edit::{default_backup_dir, plan_move};
use datareco_relocate::{MoveProgress, Mover, align_target, default_journal_dir, unfinished};
use serde::Serialize;
use tauri::State;

use crate::copies::Copies;
use crate::files::{FileSystems, Recoveries};
use crate::health::DiskTests;
use crate::partitions::Searches;
use crate::tables::{Busy, check_not_busy, check_nothing_running};
use crate::volumes::Wipes;
use crate::{CommandError, OpenSources};

/// How long each move step runs before reporting back.
const STEP: Duration = Duration::from_millis(400);

fn refused(message: impl Into<String>) -> CommandError {
    CommandError::new("refused", message)
}

/// A move under way: the mover, and the disk open to read and to write.
struct Session {
    mover: Mover,
    source: FileSource,
    target: Box<dyn SectorTarget>,
}

/// Moves running, by source id.
#[derive(Default)]
pub struct Moves(Mutex<HashMap<String, Session>>);

impl Moves {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn sources_in_use(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }
}

/// What moving would do, before anything is written.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovePreview {
    /// What the move does, or why it can't be done.
    description: Option<String>,
    problem: Option<String>,
    bytes: u64,
    /// What to type to confirm, and the volumes closed while moving.
    confirmation: Option<String>,
    closing: Vec<String>,
    journal_dir: PathBuf,
}

/// What the person types, and the volumes closed: or why the disk can't
/// be written.
fn writable(spec: &SourceSpec) -> Result<(String, Vec<String>), CommandError> {
    match spec {
        SourceSpec::PhysicalDisk(number) => {
            let check = check_disk_for_changes(*number, &list_devices()?)?;
            Ok((check.confirmation, check.volumes))
        }
        SourceSpec::Image(_) => Ok((spec.display_name(), Vec::new())),
        SourceSpec::Volume(_) => Err(refused("Pick the disk this volume is on.")),
    }
}

#[tauri::command(async)]
pub fn preview_move(
    id: String,
    start_lba: u64,
    new_start_lba: u64,
) -> Result<MovePreview, CommandError> {
    let spec = SourceSpec::parse(&id);
    let disk = Disk::open(&spec)?;
    let plan = plan_move(&disk, start_lba, new_start_lba)?;
    let problem = plan
        .blocked
        .clone()
        .or_else(|| plan.error.as_ref().map(|e| e.message.clone()));
    let bytes = datareco_partitions::read_table(&disk)?
        .partitions
        .iter()
        .find(|p| p.start_lba == start_lba)
        .map_or(0, |p| p.sectors * u64::from(disk.sector_size()));
    let (confirmation, closing, problem) = match writable(&spec) {
        Ok((confirmation, closing)) => (Some(confirmation), closing, problem),
        Err(e) => (None, Vec::new(), problem.or(Some(e.message))),
    };
    Ok(MovePreview {
        description: plan.steps.first().map(|s| s.description.clone()),
        problem,
        bytes,
        confirmation,
        closing,
        journal_dir: default_journal_dir(),
    })
}

/// Where the partition would go to start on a 1 MiB boundary, if it has
/// room; its own start if it's already there.
#[tauri::command(async)]
pub fn align_partition_target(id: String, start_lba: u64) -> Result<Option<u64>, CommandError> {
    let disk = Disk::open(&SourceSpec::parse(&id))?;
    Ok(align_target(&disk, start_lba)?)
}

/// An unfinished move of this source: how far it got, or why it can't
/// carry on (its partition table changed since, say).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Unfinished {
    journal: PathBuf,
    progress: Option<MoveProgress>,
    problem: Option<String>,
}

#[tauri::command(async)]
pub fn unfinished_move(id: String) -> Result<Option<Unfinished>, CommandError> {
    let spec = SourceSpec::parse(&id);
    let Some(journal) = unfinished(&default_journal_dir(), &spec.id()) else {
        return Ok(None);
    };
    let disk = Disk::open(&spec)?;
    Ok(Some(match Mover::resume(&journal, &disk) {
        Ok(mover) => Unfinished {
            journal,
            progress: Some(mover.progress()),
            problem: None,
        },
        Err(e) => Unfinished {
            journal,
            progress: None,
            problem: Some(e.to_string()),
        },
    }))
}

/// Opens the disk to write and to read once `confirmation` matches, and
/// starts (or carries on) a move on it.
fn open_session(
    spec: &SourceSpec,
    disk: &Disk,
    mover: Mover,
    confirmation: &str,
) -> Result<Session, CommandError> {
    let target: Box<dyn SectorTarget> = match spec {
        SourceSpec::PhysicalDisk(number) => {
            let devices = list_devices()?;
            if devices
                .disks_of_path(&default_journal_dir())
                .contains(number)
            {
                return Err(refused(
                    "The move's journal would be on the disk being changed.",
                ));
            }
            let check = check_disk_for_changes(*number, &devices)?;
            Box::new(DiskTarget::open(&check, confirmation)?)
        }
        SourceSpec::Image(path) => {
            if !confirmation
                .trim()
                .eq_ignore_ascii_case(&spec.display_name())
            {
                return Err(refused(format!("Type {} to confirm.", spec.display_name())));
            }
            Box::new(ImageTarget::resume(path, disk.size())?)
        }
        SourceSpec::Volume(_) => return Err(refused("Pick the disk this volume is on.")),
    };
    Ok(Session {
        mover,
        source: FileSource::open(spec)?,
        target,
    })
}

#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)] // Tauri hands over each piece of state
pub fn start_move(
    sources: State<'_, OpenSources>,
    file_systems: State<'_, FileSystems>,
    searches: State<'_, Searches>,
    recoveries: State<'_, Recoveries>,
    copies: State<'_, Copies>,
    tests: State<'_, DiskTests>,
    wipes: State<'_, Wipes>,
    moves: State<'_, Moves>,
    id: String,
    start_lba: Option<u64>,
    new_start_lba: Option<u64>,
    confirmation: String,
) -> Result<MoveProgress, CommandError> {
    let spec = SourceSpec::parse(&id);
    if moves.lock().contains_key(&id) {
        return Err(refused("A move on this disk is already running."));
    }
    let busy = Busy {
        searches,
        recoveries,
        copies,
        tests,
        wipes,
        moves,
    };
    // Not `check_not_busy`: that refuses while a move is unfinished, and
    // this carries it on (`Mover::begin` refuses a second one).
    check_nothing_running(&spec, &busy)?;
    let disk = Disk::open(&spec)?;
    // Without a place to move to: carry on the unfinished move.
    let mover = match start_lba.zip(new_start_lba) {
        Some((from, to)) => Mover::begin(
            &disk,
            &spec.id(),
            from,
            to,
            &default_journal_dir(),
            &default_backup_dir(),
        )?,
        None => {
            let journal = unfinished(&default_journal_dir(), &spec.id())
                .ok_or_else(|| refused("There's no unfinished move on this disk."))?;
            Mover::resume(&journal, &disk)?
        }
    };
    let session = open_session(&spec, &disk, mover, &confirmation)?;
    busy.moves.lock().insert(id.clone(), session);
    sources.clear();
    file_systems.clear();
    step(&busy.moves, &id)
}

fn step(moves: &Moves, id: &str) -> Result<MoveProgress, CommandError> {
    let mut all = moves.lock();
    let session = all
        .get_mut(id)
        .ok_or_else(|| CommandError::new("io", "No move is running here."))?;
    let Session {
        mover,
        source,
        target,
    } = session;
    match mover.step(source, target.as_mut(), STEP) {
        Ok(progress) => {
            if progress.finished {
                all.remove(id); // closes the disk: Windows reads its new table
            }
            Ok(progress)
        }
        Err(e) => {
            all.remove(id);
            Err(CommandError::new(
                "io",
                format!("The move stopped: {e}. Its journal is kept, so it can carry on."),
            ))
        }
    }
}

#[tauri::command(async)]
pub fn continue_move(moves: State<'_, Moves>, id: String) -> Result<MoveProgress, CommandError> {
    step(&moves, &id)
}

/// Stops a move; its journal stays, to carry it on later.
#[tauri::command]
pub fn stop_move(moves: State<'_, Moves>, id: String) -> Option<MoveProgress> {
    moves.lock().remove(&id).map(|s| s.mover.progress())
}

// ---------------------------------------------------------------- resizing

/// How far Windows can resize an NTFS partition, in bytes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResizeLimits {
    min: u64,
    max: u64,
    now: u64,
    confirmation: String,
}

fn disk_number(id: &str) -> Result<u32, CommandError> {
    match SourceSpec::parse(id) {
        SourceSpec::PhysicalDisk(n) => Ok(n),
        _ => Err(refused(
            "Windows resizes partitions on disks, not image files.",
        )),
    }
}

#[tauri::command(async)]
pub fn resize_limits(id: String, start_lba: u64) -> Result<ResizeLimits, CommandError> {
    let number = disk_number(&id)?;
    let disk = Disk::open(&SourceSpec::parse(&id))?;
    let sector = u64::from(disk.sector_size());
    let now = datareco_partitions::read_table(&disk)?
        .partitions
        .iter()
        .find(|p| p.start_lba == start_lba)
        .map_or(0, |p| p.sectors * sector);
    let (min, max) = partition_size_limits(number, start_lba * sector)?;
    Ok(ResizeLimits {
        min,
        max,
        now,
        confirmation: format!("disk{number}"),
    })
}

#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)] // Tauri hands over each piece of state
pub fn resize_partition(
    sources: State<'_, OpenSources>,
    file_systems: State<'_, FileSystems>,
    searches: State<'_, Searches>,
    recoveries: State<'_, Recoveries>,
    copies: State<'_, Copies>,
    tests: State<'_, DiskTests>,
    wipes: State<'_, Wipes>,
    moves: State<'_, Moves>,
    id: String,
    start_lba: u64,
    size: u64,
    confirmation: String,
) -> Result<(), CommandError> {
    let number = disk_number(&id)?;
    if !confirmation
        .trim()
        .eq_ignore_ascii_case(&format!("disk{number}"))
    {
        return Err(refused(format!("Type disk{number} to confirm.")));
    }
    let spec = SourceSpec::parse(&id);
    check_not_busy(
        &spec,
        &Busy {
            searches,
            recoveries,
            copies,
            tests,
            wipes,
            moves,
        },
    )?;
    let disk = Disk::open(&spec)?;
    let offset = start_lba * u64::from(disk.sector_size());
    resize_with_windows(number, offset, size)?;
    sources.clear();
    file_systems.clear();
    Ok(())
}
