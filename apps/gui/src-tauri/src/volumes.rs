//! Formatting, wiping, labels and drive letters, from the Disk view.
//! Formatting and wiping show what they'd do first, and write only once the
//! disk's name is typed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use datareco_format::{
    FormatKind, FormatOptions, FormatSummary, Place, SpotInfo, Wipe, WipePattern, WipeProgress,
    locate, plan_format, write_format,
};
use datareco_io::manage::{format_with_windows, set_drive_letter, set_volume_label};
use datareco_io::{DeviceList, Disk, SectorTarget, SourceSpec, VolumeInfo, list_devices};
use serde::Serialize;
use tauri::State;

use crate::copies::Copies;
use crate::files::{FileSystems, Recoveries};
use crate::health::DiskTests;
use crate::partitions::Searches;
use crate::tables::{Busy, check_not_busy};
use crate::{CommandError, OpenSources};

/// How long each wipe step writes before reporting back.
const STEP: Duration = Duration::from_millis(400);

fn refused(message: impl Into<String>) -> CommandError {
    CommandError::new("refused", message)
}

/// The partition (first sector, sectors), or the whole source with `None`.
fn range(start_lba: Option<u64>, sectors: Option<u64>) -> Option<(u64, u64)> {
    start_lba.zip(sectors)
}

/// What formatting would make, before anything is written.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatPreview {
    spot: SpotInfo,
    /// For FAT32 and exFAT; NTFS is Windows' to work out.
    summary: Option<FormatSummary>,
    /// Why it can't be done as asked.
    problem: Option<String>,
}

fn kind_of(file_system: &str) -> Result<Option<FormatKind>, CommandError> {
    match file_system {
        "fat32" => Ok(Some(FormatKind::Fat32)),
        "exfat" => Ok(Some(FormatKind::Exfat)),
        "ntfs" => Ok(None),
        other => Err(refused(format!("DataReco doesn't format {other}."))),
    }
}

#[tauri::command(async)]
pub fn preview_format(
    id: String,
    start_lba: Option<u64>,
    sectors: Option<u64>,
    file_system: String,
    label: Option<String>,
    cluster_size: Option<u32>,
) -> Result<FormatPreview, CommandError> {
    let spec = SourceSpec::parse(&id);
    let disk = Disk::open(&spec)?;
    let devices = list_devices().ok();
    let spot = locate(&spec, &disk, range(start_lba, sectors), devices.as_ref())?;
    let mut preview = FormatPreview {
        spot: spot.info(),
        summary: None,
        problem: None,
    };
    match kind_of(&file_system)? {
        Some(kind) => {
            let options = FormatOptions {
                kind,
                label,
                cluster_size,
                serial: None,
                partition_offset: spot.start_lba,
            };
            match plan_format(spot.size, spot.sector_size, &options) {
                Ok(plan) => preview.summary = Some(plan.summary),
                Err(problem) => preview.problem = Some(problem),
            }
        }
        None => {
            preview.problem = match &spot.place {
                Place::Volume(_) => None,
                Place::Image(_) => Some("Windows formats NTFS only on disks; for an image file, use exFAT or FAT32.".into()),
                Place::Disk(_) => Some(
                    "Windows hasn't mounted this partition (a type it doesn't use, or hidden), so it \
                     can't format it as NTFS. Change its type to NTFS first, or use exFAT or FAT32."
                        .into(),
                ),
            };
        }
    }
    Ok(preview)
}

/// Formats a partition once `confirmation` is the disk's name.
#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)] // Tauri hands over each piece of state
pub fn format_partition(
    sources: State<'_, OpenSources>,
    file_systems: State<'_, FileSystems>,
    searches: State<'_, Searches>,
    recoveries: State<'_, Recoveries>,
    copies: State<'_, Copies>,
    tests: State<'_, DiskTests>,
    wipes: State<'_, Wipes>,
    id: String,
    start_lba: Option<u64>,
    sectors: Option<u64>,
    file_system: String,
    label: Option<String>,
    cluster_size: Option<u32>,
    full: bool,
    confirmation: String,
) -> Result<String, CommandError> {
    let spec = SourceSpec::parse(&id);
    check_not_busy(
        &spec,
        &Busy {
            searches,
            recoveries,
            copies,
            tests,
            wipes,
        },
    )?;
    let disk = Disk::open(&spec)?;
    let devices = list_devices().ok();
    let spot = locate(&spec, &disk, range(start_lba, sectors), devices.as_ref())?;
    let result = match kind_of(&file_system)? {
        Some(kind) => {
            let options = FormatOptions {
                kind,
                label,
                cluster_size,
                serial: None,
                partition_offset: spot.start_lba,
            };
            let plan = plan_format(spot.size, spot.sector_size, &options).map_err(refused)?;
            let mut target = spot.open(&confirmation)?;
            write_format(&plan, target.as_mut(), spot.offset)
                .map_err(|e| CommandError::new("io", format!("Writing failed: {e}")))?;
            drop(target);
            let start = spot.start_lba * u64::from(spot.sector_size);
            let fs = datareco_fs::open(Arc::new(Disk::open(&spec)?), start).map_err(|e| {
                CommandError::new("io", format!("The new file system doesn't read back: {e}"))
            })?;
            Ok(format!(
                "Formatted as {}{}.",
                fs.info().name,
                fs.info()
                    .label
                    .as_deref()
                    .map_or_else(String::new, |l| format!(" \"{l}\""))
            ))
        }
        None => {
            let Place::Volume(check) = &spot.place else {
                return Err(refused(
                    "Windows formats NTFS only on volumes it has mounted.",
                ));
            };
            if !confirmation.trim().eq_ignore_ascii_case(&spot.confirmation) {
                return Err(refused(format!(
                    "Type {} to confirm that everything on {} may be erased.",
                    spot.confirmation, check.name
                )));
            }
            format_with_windows(&check.path, "NTFS", label.as_deref(), !full)?;
            Ok("Formatted as NTFS.".to_owned())
        }
    };
    sources.clear();
    file_systems.clear();
    result
}

// ---------------------------------------------------------------- wiping

/// A wipe and the target it writes to (closed when it's dropped).
type Wiping = (Wipe, Box<dyn SectorTarget>);

/// Wipes running, by source id.
#[derive(Default)]
pub struct Wipes(Mutex<HashMap<String, Wiping>>);

impl Wipes {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Wiping>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn sources_in_use(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }
}

/// What wiping would write over, before anything is written.
#[tauri::command(async)]
pub fn preview_wipe(
    id: String,
    start_lba: Option<u64>,
    sectors: Option<u64>,
) -> Result<SpotInfo, CommandError> {
    let spec = SourceSpec::parse(&id);
    let disk = Disk::open(&spec)?;
    let devices = list_devices().ok();
    Ok(locate(&spec, &disk, range(start_lba, sectors), devices.as_ref())?.info())
}

/// Starts wiping once `confirmation` is the disk's name, and does a first step.
#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)] // Tauri hands over each piece of state
pub fn start_wipe(
    file_systems: State<'_, FileSystems>,
    sources: State<'_, OpenSources>,
    searches: State<'_, Searches>,
    recoveries: State<'_, Recoveries>,
    copies: State<'_, Copies>,
    tests: State<'_, DiskTests>,
    wipes: State<'_, Wipes>,
    id: String,
    start_lba: Option<u64>,
    sectors: Option<u64>,
    pattern: WipePattern,
    confirmation: String,
) -> Result<WipeProgress, CommandError> {
    let spec = SourceSpec::parse(&id);
    if wipes.lock().contains_key(&id) {
        return Err(refused("This disk is already being wiped."));
    }
    let disk = Disk::open(&spec)?;
    let devices = list_devices().ok();
    let spot = locate(&spec, &disk, range(start_lba, sectors), devices.as_ref())?;
    {
        let busy = Busy {
            searches,
            recoveries,
            copies,
            tests,
            wipes,
        };
        check_not_busy(&spec, &busy)?;
        let target = spot.open(&confirmation)?;
        let wipe = Wipe::new(spot.offset, spot.offset + spot.size, pattern);
        busy.wipes.lock().insert(id.clone(), (wipe, target));
        sources.clear();
        file_systems.clear();
        step(&busy.wipes, &id)
    }
}

fn step(wipes: &Wipes, id: &str) -> Result<WipeProgress, CommandError> {
    let mut all = wipes.lock();
    let (wipe, target) = all
        .get_mut(id)
        .ok_or_else(|| CommandError::new("io", "Nothing is being wiped here."))?;
    let result = wipe.step(target.as_mut(), STEP);
    match result {
        Ok(progress) => {
            if progress.finished {
                all.remove(id); // closes the target: Windows reads the disk again
            }
            Ok(progress)
        }
        Err(e) => {
            all.remove(id);
            Err(CommandError::new("io", format!("Writing failed: {e}")))
        }
    }
}

#[tauri::command(async)]
pub fn continue_wipe(wipes: State<'_, Wipes>, id: String) -> Result<WipeProgress, CommandError> {
    step(&wipes, &id)
}

/// Stops a wipe; what's written stays written. Returns how far it got.
#[tauri::command]
pub fn stop_wipe(wipes: State<'_, Wipes>, id: String) -> Option<WipeProgress> {
    wipes.lock().remove(&id).map(|(wipe, _)| wipe.progress())
}

// ---------------------------------------------------------------- labels and letters

/// The Windows volume on a source: the volume itself, or the one on a
/// disk's partition starting at `start_lba`.
fn volume_of<'a>(
    spec: &SourceSpec,
    start_lba: Option<u64>,
    devices: &'a DeviceList,
) -> Result<&'a VolumeInfo, CommandError> {
    let found = match (spec, start_lba) {
        (SourceSpec::Volume(name), _) => devices.volume(name),
        (SourceSpec::PhysicalDisk(number), Some(start)) => {
            let sector = devices
                .disks
                .iter()
                .find(|d| d.number == *number)
                .map_or(512, |d| u64::from(d.sector_size));
            devices.volumes.iter().find(|v| {
                v.extents
                    .iter()
                    .any(|e| e.disk == *number && e.offset == start * sector)
            })
        }
        _ => None,
    };
    found.ok_or_else(|| refused("Windows has no volume there (a type it doesn't use, or hidden)."))
}

#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value)] // Tauri hands over owned state
pub fn set_label(
    sources: State<'_, OpenSources>,
    id: String,
    start_lba: Option<u64>,
    label: String,
) -> Result<(), CommandError> {
    let devices = list_devices()?;
    let volume = volume_of(&SourceSpec::parse(&id), start_lba, &devices)?;
    set_volume_label(&volume.path, Some(&label))?;
    sources.clear();
    Ok(())
}

#[tauri::command(async)]
pub fn set_letter(
    id: String,
    start_lba: Option<u64>,
    letter: Option<String>,
) -> Result<(), CommandError> {
    let devices = list_devices()?;
    let volume = volume_of(&SourceSpec::parse(&id), start_lba, &devices)?;
    let letter = letter.and_then(|l| l.chars().next());
    set_drive_letter(&volume.path, &volume.mount_points, letter)?;
    Ok(())
}
