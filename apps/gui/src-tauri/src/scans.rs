//! Scans for volumes (quick and full), saved scans, and the volumes they find.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use datareco_io::SourceSpec;
use datareco_scan::{FoundVolume, SavedScan, Scan, ScanError, ScanMode, ScanProgress};
use serde::Serialize;
use tauri::State;

use crate::{CommandError, OpenSources};

/// Bytes each scan step examines before reporting back.
const STEP: u64 = 64 << 20;

impl From<ScanError> for CommandError {
    fn from(error: ScanError) -> Self {
        CommandError::new(error.kind(), error.to_string())
    }
}

/// Scans by source id.
#[derive(Default)]
pub struct Scans(Mutex<HashMap<String, Session>>);

pub struct Session {
    scan: Scan,
    /// Counts up with each new or reopened scan of the source, so volume
    /// keys from an older one aren't mixed up with the new results.
    generation: u64,
    /// The volumes found, as last worked out.
    volumes: Vec<FoundVolume>,
}

/// How a scan is going; `volumes` once it's done (or paused and asked for).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStatus {
    progress: ScanProgress,
    generation: u64,
    volumes: Option<Vec<FoundVolume>>,
}

impl Scans {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A volume of a scan, for the file browser.
    pub fn volume(
        &self,
        source: &str,
        generation: u64,
        index: usize,
    ) -> Result<FoundVolume, CommandError> {
        let scans = self.lock();
        scans
            .get(source)
            .filter(|s| s.generation == generation)
            .and_then(|s| s.volumes.get(index))
            .cloned()
            .ok_or_else(|| {
                CommandError::new(
                    "not_found",
                    "That volume belongs to a scan that's been replaced; scan again.",
                )
            })
    }

    fn start(&self, key: String, scan: Scan) -> u64 {
        let mut scans = self.lock();
        let generation = scans.get(&key).map_or(1, |s| s.generation + 1);
        scans.insert(
            key,
            Session {
                scan,
                generation,
                volumes: Vec::new(),
            },
        );
        generation
    }
}

fn key(id: &str) -> String {
    SourceSpec::parse(id).id()
}

/// Starts a scan (replacing any earlier one of this source) and does a first step.
#[tauri::command(async)]
pub fn start_scan(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    id: String,
    mode: ScanMode,
) -> Result<ScanStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    scans.start(key(&id), Scan::new(&disk, mode));
    continue_scan(sources, scans, id)
}

/// Scans one more step; call again until `progress.done`. Not calling it
/// pauses the scan.
#[tauri::command(async)]
pub fn continue_scan(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    id: String,
) -> Result<ScanStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let mut all = scans.lock();
    let session = all
        .get_mut(&key(&id))
        .ok_or_else(|| CommandError::new("not_found", "No scan of this source is running."))?;
    let progress = session.scan.step(&disk, STEP)?;
    let volumes = if progress.done {
        session.volumes = session.scan.results(&disk)?;
        Some(session.volumes.clone())
    } else {
        None
    };
    Ok(ScanStatus {
        progress,
        generation: session.generation,
        volumes,
    })
}

/// What a scan has found so far (for a paused scan).
#[tauri::command(async)]
pub fn scan_results(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    id: String,
) -> Result<ScanStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let mut all = scans.lock();
    let session = all
        .get_mut(&key(&id))
        .ok_or_else(|| CommandError::new("not_found", "This source hasn't been scanned."))?;
    session.volumes = session.scan.results(&disk)?;
    Ok(ScanStatus {
        progress: session.scan.progress(),
        generation: session.generation,
        volumes: Some(session.volumes.clone()),
    })
}

/// Saves the source's scan to a file, to carry on or look at it later.
#[tauri::command(async)]
pub fn save_scan(scans: State<'_, Scans>, id: String, path: PathBuf) -> Result<(), CommandError> {
    let all = scans.lock();
    let session = all
        .get(&key(&id))
        .ok_or_else(|| CommandError::new("not_found", "This source hasn't been scanned."))?;
    session
        .scan
        .save(&key(&id))
        .write_to(&path)
        .map_err(|e| CommandError::new("io", format!("Saving the scan failed: {e}")))
}

/// Opens a saved scan of this source, with what it had found.
#[tauri::command(async)]
pub fn open_saved_scan(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    id: String,
    path: PathBuf,
) -> Result<ScanStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let saved = SavedScan::read_from(&path)?;
    let scan = Scan::restore(&disk, &saved)?;
    scans.start(key(&id), scan);
    scan_results(sources, scans, id)
}
