//! The disk view: the disk map, drive health, the surface test and the
//! read speed test. All of them only read.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use datareco_health::{HealthReport, SpeedStatus, SpeedTest, SurfaceProgress, SurfaceTest, assess};
use datareco_io::{FileSource, SectorSource, SourceSpec, list_devices, read_health_data};
use datareco_partitions::{DiskMap, disk_map as map_disk, read_table};
use serde::Serialize;
use tauri::State;

use crate::{CommandError, OpenSources};

/// How long each test step reads before reporting back.
const STEP: Duration = Duration::from_millis(400);

#[tauri::command(async)]
pub fn disk_map(sources: State<'_, OpenSources>, id: String) -> Result<DiskMap, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let devices = list_devices().ok();
    Ok(map_disk(&disk, &SourceSpec::parse(&id), devices.as_ref())?)
}

/// A drive's health, and which physical disk it is.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskHealth {
    /// The physical disk checked (the one under a volume); none for images.
    disk: Option<u32>,
    report: HealthReport,
}

#[tauri::command(async)]
pub fn disk_health(id: String) -> Result<DiskHealth, CommandError> {
    let spec = SourceSpec::parse(&id);
    let unknown = |summary: String| {
        let mut report = assess(&datareco_io::HealthData::Unavailable { reason: summary });
        report.source = None;
        DiskHealth { disk: None, report }
    };
    let number = match &spec {
        SourceSpec::PhysicalDisk(number) => *number,
        SourceSpec::Image(_) => {
            return Ok(unknown(
                "Image files have no health data: it comes from drives.".to_owned(),
            ));
        }
        SourceSpec::Volume(_) => match list_devices()?.disks_of_source(&spec)[..] {
            [number] => number,
            [] => {
                return Ok(unknown(
                    "This volume isn't on a disk DataReco can see.".to_owned(),
                ));
            }
            _ => {
                return Ok(unknown(
                    "This volume spans several disks; check each disk on its own.".to_owned(),
                ));
            }
        },
    };
    Ok(DiskHealth {
        disk: Some(number),
        report: assess(&read_health_data(number)?),
    })
}

// ---------------------------------------------------------------- tests

/// Tests running, by source id. Each has its own handle on the source,
/// without the cache, so read times are the disk's own.
#[derive(Default)]
pub struct DiskTests {
    surface: Mutex<HashMap<String, (SurfaceTest, FileSource)>>,
    speed: Mutex<HashMap<String, (SpeedTest, FileSource)>>,
}

impl DiskTests {
    /// The sources being tested.
    pub fn sources_in_use(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock(&self.surface).keys().cloned().collect();
        ids.extend(lock(&self.speed).keys().cloned());
        ids
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts a surface test of the whole source, or of one partition
/// (replacing any running one), and does a first step.
#[tauri::command(async)]
pub fn start_surface_test(
    sources: State<'_, OpenSources>,
    tests: State<'_, DiskTests>,
    id: String,
    partition: Option<u32>,
) -> Result<SurfaceProgress, CommandError> {
    let spec = SourceSpec::parse(&id);
    let source = FileSource::open(&spec)?;
    let test = match partition {
        None => SurfaceTest::whole(&source),
        Some(number) => {
            let disk = sources.get_or_open(&id)?;
            let table = read_table(&disk)?;
            let p = table
                .partitions
                .iter()
                .find(|p| p.number == number)
                .ok_or_else(|| {
                    CommandError::new("io", format!("There's no partition {number}."))
                })?;
            SurfaceTest::new(source.sector_size(), p.start_lba, p.sectors)
        }
    };
    lock(&tests.surface).insert(id.clone(), (test, source));
    continue_surface_test(tests, id)
}

/// Tests one more step; call again until `done`.
#[tauri::command(async)]
pub fn continue_surface_test(
    tests: State<'_, DiskTests>,
    id: String,
) -> Result<SurfaceProgress, CommandError> {
    let mut all = lock(&tests.surface);
    let (test, source) = all
        .get_mut(&id)
        .ok_or_else(|| CommandError::new("io", "No surface test is running for this source."))?;
    let progress = test.step(source, STEP);
    if progress.done {
        all.remove(&id);
    }
    Ok(progress)
}

/// Ends a surface test; returns how far it got.
#[tauri::command]
pub fn stop_surface_test(tests: State<'_, DiskTests>, id: String) -> Option<SurfaceProgress> {
    lock(&tests.surface)
        .remove(&id)
        .map(|(test, _)| test.progress())
}

#[tauri::command(async)]
pub fn start_speed_test(
    tests: State<'_, DiskTests>,
    id: String,
) -> Result<SpeedStatus, CommandError> {
    let source = FileSource::open(&SourceSpec::parse(&id))?;
    let test = SpeedTest::new(&source);
    lock(&tests.speed).insert(id.clone(), (test, source));
    continue_speed_test(tests, id)
}

#[tauri::command(async)]
pub fn continue_speed_test(
    tests: State<'_, DiskTests>,
    id: String,
) -> Result<SpeedStatus, CommandError> {
    let mut all = lock(&tests.speed);
    let (test, source) = all
        .get_mut(&id)
        .ok_or_else(|| CommandError::new("io", "No speed test is running for this source."))?;
    let status = test.step(source, STEP);
    if status.done {
        all.remove(&id);
    }
    Ok(status)
}

#[tauri::command]
pub fn stop_speed_test(tests: State<'_, DiskTests>, id: String) {
    lock(&tests.speed).remove(&id);
}
