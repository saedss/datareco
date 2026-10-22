//! DataReco desktop app: a Tauri shell around the engine crates.

// Release builds run without a console window on Windows. Do not remove.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use datareco_io::{DeviceList, Disk, Error, SourceSpec};
use serde::Serialize;
use tauri::{AppHandle, State};

/// Most sectors one `read_sectors` call returns.
const MAX_SECTORS_PER_READ: u32 = 64;

/// Basic facts about the running app, shown in the status bar.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    name: &'static str,
    version: &'static str,
    os: &'static str,
    arch: &'static str,
    /// Whether the app runs with administrator rights, which reading disks needs.
    elevated: bool,
    /// Source to show at startup, from `--open <source>`.
    start_source: Option<String>,
}

/// The `--open <source>` command-line argument, as a source id. Used by
/// "Restart as administrator" to come back to the same disk.
fn start_source() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--open" {
            return args.next().map(|source| SourceSpec::parse(&source).id());
        }
    }
    None
}

/// An engine error as the frontend sees it.
#[derive(Debug, Serialize)]
struct CommandError {
    /// See `datareco_io::Error::kind`, e.g. "access_denied".
    kind: &'static str,
    message: String,
}

impl From<Error> for CommandError {
    fn from(error: Error) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

/// An opened disk, volume or image.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SourceInfo {
    id: String,
    name: String,
    size: u64,
    sector_size: u32,
    sector_count: u64,
}

/// Sectors read from a source.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SectorData {
    lba: u64,
    data: Vec<u8>,
    /// Sectors that couldn't be read; their bytes are zeros.
    unreadable: Vec<u64>,
}

/// Sources opened so far, by id, so each is opened (and cached) once.
#[derive(Default)]
struct OpenSources(Mutex<HashMap<String, Arc<Disk>>>);

impl OpenSources {
    fn get_or_open(&self, id: &str) -> Result<Arc<Disk>, Error> {
        let spec = SourceSpec::parse(id);
        let key = spec.id();
        if let Some(disk) = self.lock().get(&key) {
            return Ok(Arc::clone(disk));
        }
        // Opened outside the lock: opening a slow device mustn't block reads
        // from the others.
        let disk = Arc::new(Disk::open(&spec)?);
        Ok(Arc::clone(self.lock().entry(key).or_insert(disk)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Disk>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        name: "DataReco",
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        elevated: datareco_io::is_elevated(),
        start_source: start_source(),
    }
}

// Commands that touch devices run off the main thread (`async`), so a slow or
// failing disk never freezes the window.

#[tauri::command(async)]
fn list_devices() -> Result<DeviceList, CommandError> {
    Ok(datareco_io::list_devices()?)
}

#[tauri::command(async)]
fn open_source(sources: State<'_, OpenSources>, id: String) -> Result<SourceInfo, CommandError> {
    let disk = sources.get_or_open(&id)?;
    Ok(SourceInfo {
        id: SourceSpec::parse(&id).id(),
        name: disk.name().to_owned(),
        size: disk.size(),
        sector_size: disk.sector_size(),
        sector_count: disk.sector_count(),
    })
}

#[tauri::command(async)]
fn read_sectors(
    sources: State<'_, OpenSources>,
    id: String,
    lba: u64,
    count: u32,
) -> Result<SectorData, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let (data, outcome) = disk.read_sectors(lba, count.clamp(1, MAX_SECTORS_PER_READ))?;
    Ok(SectorData {
        lba,
        data,
        unreadable: outcome.unreadable,
    })
}

/// Restarts the app with administrator rights (Windows shows its UAC prompt),
/// reopening `open` if given, then closes this instance.
#[tauri::command]
fn restart_as_admin(app: AppHandle, open: Option<String>) -> Result<(), CommandError> {
    let arguments: Vec<String> = open
        .into_iter()
        .flat_map(|id| ["--open".to_owned(), id])
        .collect();
    datareco_io::relaunch_elevated(&arguments)?;
    app.exit(0);
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(OpenSources::default())
        .invoke_handler(tauri::generate_handler![
            app_info,
            list_devices,
            open_source,
            read_sectors,
            restart_as_admin
        ])
        .run(tauri::generate_context!())
        .expect("error while running DataReco");
}
