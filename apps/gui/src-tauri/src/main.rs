//! DataReco desktop app: a Tauri shell around the engine crates.

// Release builds run without a console window on Windows. Do not remove.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod structures;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use datareco_io::{DeviceList, Disk, Error, SearchProgress, SourceSpec};
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::structures::Templates;

/// Most bytes one `read_bytes` call returns.
const MAX_BYTES_PER_READ: u64 = 256 * 1024;
/// Bytes one `find` call examines before reporting progress.
const SEARCH_STEP: u64 = 64 << 20;

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

/// An error as the frontend sees it.
#[derive(Debug, Serialize)]
struct CommandError {
    /// See `datareco_io::Error::kind`, e.g. "access_denied", or "template".
    kind: &'static str,
    message: String,
}

impl CommandError {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl From<Error> for CommandError {
    fn from(error: Error) -> Self {
        Self::new(error.kind(), error.to_string())
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

/// Bytes read from a source.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ByteData {
    offset: u64,
    data: Vec<u8>,
    /// Sectors in the range that couldn't be read; their bytes are zeros.
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

/// Reads up to `length` bytes at `offset`, fewer at the end of the source.
fn read_range(disk: &Disk, offset: u64, length: u64) -> Result<ByteData, CommandError> {
    let length = length.min(disk.size().saturating_sub(offset));
    let mut data =
        vec![0; usize::try_from(length).map_err(|_| CommandError::new("io", "read too large"))?];
    let outcome = disk.read_at(offset, &mut data)?;
    Ok(ByteData {
        offset,
        data,
        unreadable: outcome.unreadable,
    })
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
fn read_bytes(
    sources: State<'_, OpenSources>,
    id: String,
    offset: u64,
    length: u64,
) -> Result<ByteData, CommandError> {
    let disk = sources.get_or_open(&id)?;
    read_range(&disk, offset, length.min(MAX_BYTES_PER_READ))
}

/// Searches one step (64 MiB) from `from`; the frontend calls again with
/// `next` until something is found, showing progress in between.
#[tauri::command(async)]
fn find(
    sources: State<'_, OpenSources>,
    id: String,
    pattern: Vec<u8>,
    from: u64,
    ignore_case: bool,
) -> Result<SearchProgress, CommandError> {
    let disk = sources.get_or_open(&id)?;
    Ok(disk.find(&pattern, from, SEARCH_STEP, ignore_case)?)
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
        .setup(|app| {
            let folder = app.path().app_config_dir()?.join("templates");
            app.manage(Templates::load(folder));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            list_devices,
            open_source,
            read_bytes,
            find,
            restart_as_admin,
            structures::list_templates,
            structures::reload_templates,
            structures::detect_templates,
            structures::decode_template,
            structures::open_templates_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running DataReco");
}
