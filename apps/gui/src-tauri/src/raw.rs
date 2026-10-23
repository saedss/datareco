//! Raw recovery: scanning a source for files by their contents, the
//! signature list (editable in the app's config folder), and the volume of
//! files found.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use datareco_io::SourceSpec;
use datareco_scan::raw::{
    DEFAULT_SIGNATURES, RawProgress, RawScan, RawVolume, SavedRawScan, Signature,
    default_signatures, load_signatures,
};
use serde::Serialize;
use tauri::State;

use crate::{CommandError, OpenSources};

/// Bytes each step examines before reporting back.
const STEP: u64 = 64 << 20;

/// Raw scans by source id.
pub struct RawScans {
    sessions: Mutex<HashMap<String, Session>>,
    /// The user's signature list; the built-in one is used until it exists.
    signatures: PathBuf,
}

struct Session {
    scan: RawScan,
    /// Counts up with each new or reopened scan of the source.
    generation: u64,
    /// Counts up each time the found files are looked at again (a paused
    /// scan finds more when resumed), so each look is its own volume.
    snapshot: u64,
    volume: Option<Arc<RawVolume>>,
}

/// How a raw scan is going, and (when done or paused) what it found.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawStatus {
    progress: RawProgress,
    /// The volume key of the files found: `raw:<generation>:<snapshot>`.
    volume: Option<String>,
    /// Files found by extension.
    counts: Vec<(String, usize)>,
}

/// The signature list in use.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignaturesInfo {
    /// Where the user's list is (or would be).
    path: String,
    /// Whether that file exists (else the built-in list is used).
    custom: bool,
    count: usize,
    /// What's wrong with the user's list, if anything.
    problem: Option<String>,
}

impl RawScans {
    pub fn new(signatures: PathBuf) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            signatures,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The user's list if there is one, else the built-in one.
    fn signatures(&self) -> Result<Vec<Signature>, CommandError> {
        if self.signatures.exists() {
            load_signatures(&self.signatures).map_err(|e| CommandError::new("signatures", e))
        } else {
            Ok(default_signatures())
        }
    }

    /// The files a scan found, for the file browser.
    pub fn volume(
        &self,
        source: &str,
        generation: u64,
        snapshot: u64,
    ) -> Result<Arc<RawVolume>, CommandError> {
        self.lock()
            .get(source)
            .filter(|s| s.generation == generation && s.snapshot == snapshot)
            .and_then(|s| s.volume.clone())
            .ok_or_else(|| {
                CommandError::new(
                    "not_found",
                    "Those files belong to an older scan; look at the new one.",
                )
            })
    }

    fn start(&self, key: String, scan: RawScan) {
        let mut sessions = self.lock();
        let generation = sessions.get(&key).map_or(1, |s| s.generation + 1);
        sessions.insert(
            key,
            Session {
                scan,
                generation,
                snapshot: 0,
                volume: None,
            },
        );
    }
}

fn key(id: &str) -> String {
    SourceSpec::parse(id).id()
}

/// Takes a fresh look at what the scan found, as a new volume.
fn status(session: &mut Session, disk: Arc<datareco_io::Disk>, look: bool) -> RawStatus {
    let progress = session.scan.progress();
    if look {
        let volume = Arc::new(session.scan.volume(disk));
        session.snapshot += 1;
        session.volume = Some(volume);
    }
    RawStatus {
        progress,
        volume: session
            .volume
            .as_ref()
            .map(|_| format!("raw:{}:{}", session.generation, session.snapshot)),
        counts: session
            .volume
            .as_ref()
            .map(|v| v.counts())
            .unwrap_or_default(),
    }
}

/// Starts a raw scan of the whole source (replacing an earlier one) and
/// does a first step.
#[tauri::command(async)]
pub fn start_raw_scan(
    sources: State<'_, OpenSources>,
    raws: State<'_, RawScans>,
    id: String,
) -> Result<RawStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let signatures = raws.signatures()?;
    raws.start(key(&id), RawScan::new(&disk, signatures, None));
    continue_raw_scan(sources, raws, id)
}

/// Scans one more step; call again until `progress.done`.
#[tauri::command(async)]
pub fn continue_raw_scan(
    sources: State<'_, OpenSources>,
    raws: State<'_, RawScans>,
    id: String,
) -> Result<RawStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let mut sessions = raws.lock();
    let session = sessions
        .get_mut(&key(&id))
        .ok_or_else(|| CommandError::new("not_found", "No raw scan of this source is running."))?;
    let progress = session.scan.step(&disk, STEP)?;
    Ok(status(session, disk, progress.done))
}

/// What a (paused) raw scan has found so far.
#[tauri::command(async)]
pub fn raw_results(
    sources: State<'_, OpenSources>,
    raws: State<'_, RawScans>,
    id: String,
) -> Result<RawStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let mut sessions = raws.lock();
    let session = sessions
        .get_mut(&key(&id))
        .ok_or_else(|| CommandError::new("not_found", "This source hasn't had a raw scan."))?;
    Ok(status(session, disk, true))
}

#[tauri::command(async)]
pub fn save_raw_scan(
    sources: State<'_, OpenSources>,
    raws: State<'_, RawScans>,
    id: String,
    path: PathBuf,
) -> Result<(), CommandError> {
    let disk = sources.get_or_open(&id)?;
    let sessions = raws.lock();
    let session = sessions
        .get(&key(&id))
        .ok_or_else(|| CommandError::new("not_found", "This source hasn't had a raw scan."))?;
    session
        .scan
        .save(&key(&id), &disk)
        .write_to(&path)
        .map_err(|e| CommandError::new("io", format!("Saving the scan failed: {e}")))
}

#[tauri::command(async)]
pub fn open_saved_raw_scan(
    sources: State<'_, OpenSources>,
    raws: State<'_, RawScans>,
    id: String,
    path: PathBuf,
) -> Result<RawStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let saved = SavedRawScan::read_from(&path)?;
    let scan = RawScan::restore(&disk, raws.signatures()?, &saved)?;
    raws.start(key(&id), scan);
    raw_results(sources, raws, id)
}

/// The signature list in use, and any problem with the user's one.
#[tauri::command]
pub fn signatures_info(raws: State<'_, RawScans>) -> SignaturesInfo {
    let custom = raws.signatures.exists();
    let (count, problem) = match raws.signatures() {
        Ok(list) => (list.len(), None),
        Err(error) => (0, Some(error.message)),
    };
    SignaturesInfo {
        path: raws.signatures.display().to_string(),
        custom,
        count,
        problem,
    }
}

/// Shows the user's signature list in the file manager, writing the built-in
/// one there first if there's none yet.
#[tauri::command]
pub fn open_signatures(raws: State<'_, RawScans>) -> Result<(), CommandError> {
    let path = &raws.signatures;
    if !path.exists() {
        if let Some(folder) = path.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        std::fs::write(path, DEFAULT_SIGNATURES).map_err(|e| {
            CommandError::new("io", format!("couldn't write {}: {e}", path.display()))
        })?;
    }
    #[cfg(windows)]
    let result = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(not(windows))]
    let result = std::process::Command::new(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    })
    .arg(path.parent().unwrap_or(path))
    .spawn();
    result
        .map(|_| ())
        .map_err(|e| CommandError::new("io", format!("couldn't show {}: {e}", path.display())))
}
