//! Browsing FAT, exFAT and NTFS volumes (deleted entries included), and
//! saving files and folders from them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use datareco_fs::{Entry, EntryId, Extent, FileSystem, FsError, FsKind, VolumeInfo, can_read};
use datareco_io::SourceSpec;
use datareco_partitions::read_table;
use datareco_scan::{Control, RecoverError, RecoverOptions, RecoverProgress, RecoverSummary};
use serde::Serialize;
use tauri::State;
use tauri::ipc::{Channel, Response};

use crate::raw::RawScans;
use crate::scans::Scans;
use crate::{CommandError, OpenSources};

/// Most bytes one `read_file` call returns (enough to preview a photo).
const MAX_FILE_READ: u64 = 16 << 20;

impl From<FsError> for CommandError {
    fn from(error: FsError) -> Self {
        CommandError::new(error.kind(), error.to_string())
    }
}

impl From<RecoverError> for CommandError {
    fn from(error: RecoverError) -> Self {
        CommandError::new(error.kind(), error.to_string())
    }
}

/// Opened file systems, by source id and volume key. Opening NTFS reads the
/// whole MFT on the first listing, so it's done once.
///
/// Volume keys: `at:<offset>` for a volume in the partition table (or one
/// filling the source), `scan:<generation>:<index>` for one a scan found.
#[derive(Default)]
pub struct FileSystems(Mutex<HashMap<(String, String), Arc<dyn FileSystem>>>);

impl FileSystems {
    fn get_or_open(
        &self,
        sources: &OpenSources,
        scans: (&Scans, &RawScans),
        id: &str,
        volume: &str,
    ) -> Result<Arc<dyn FileSystem>, CommandError> {
        let (scans, raws) = scans;
        let source = SourceSpec::parse(id).id();
        let key = (source.clone(), volume.to_owned());
        if let Some(fs) = self.lock().get(&key) {
            return Ok(Arc::clone(fs));
        }
        let disk = sources.get_or_open(id)?;
        let bad_key = || CommandError::new("not_found", format!("No volume called {volume}."));
        let fs: Arc<dyn FileSystem> = if let Some(offset) = volume.strip_prefix("at:") {
            let offset = offset.parse().map_err(|_| bad_key())?;
            Arc::from(datareco_fs::open(disk, offset)?)
        } else if let Some(rest) = volume.strip_prefix("scan:") {
            let (generation, index) = rest.split_once(':').ok_or_else(bad_key)?;
            let found = scans.volume(
                &source,
                generation.parse().map_err(|_| bad_key())?,
                index.parse().map_err(|_| bad_key())?,
            )?;
            Arc::from(datareco_scan::open_found(disk, &found)?)
        } else if let Some(rest) = volume.strip_prefix("raw:") {
            let (generation, snapshot) = rest.split_once(':').ok_or_else(bad_key)?;
            raws.volume(
                &source,
                generation.parse().map_err(|_| bad_key())?,
                snapshot.parse().map_err(|_| bad_key())?,
            )?
        } else {
            return Err(bad_key());
        };
        Ok(Arc::clone(self.lock().entry(key).or_insert(fs)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), Arc<dyn FileSystem>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A volume in the source's partition table that the file browser can offer.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeChoice {
    /// What the other commands call it.
    key: String,
    /// Byte offset of the volume on the source.
    offset: u64,
    /// Its partition number; `None` for a volume that fills the source.
    partition: Option<u32>,
    kind: FsKind,
    /// The file system's name, e.g. "NTFS".
    name: String,
    label: Option<String>,
    size: Option<u64>,
    /// Whether DataReco can read this file system yet.
    readable: bool,
}

/// The volumes on a source, from its partition table.
#[tauri::command(async)]
pub fn list_volumes(
    sources: State<'_, OpenSources>,
    id: String,
) -> Result<Vec<VolumeChoice>, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let table = read_table(&disk)?;
    let mut volumes = Vec::new();
    if let Some(fs) = &table.whole_disk {
        volumes.push(VolumeChoice {
            key: "at:0".to_owned(),
            offset: 0,
            partition: None,
            kind: fs.kind,
            name: fs.name.clone(),
            label: fs.label.clone(),
            size: fs.size,
            readable: can_read(fs.kind),
        });
    }
    for partition in &table.partitions {
        let Some(fs) = &partition.file_system else {
            continue;
        };
        let offset = partition.start_lba * u64::from(table.sector_size);
        volumes.push(VolumeChoice {
            key: format!("at:{offset}"),
            offset,
            partition: Some(partition.number),
            kind: fs.kind,
            name: fs.name.clone(),
            label: fs.label.clone(),
            size: Some(partition.sectors * u64::from(table.sector_size)),
            readable: can_read(fs.kind),
        });
    }
    Ok(volumes)
}

/// What the file browser needs to start: the volume and its top folder.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedVolume {
    info: VolumeInfo,
    root: EntryId,
}

#[tauri::command(async)]
pub fn open_file_system(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    raws: State<'_, RawScans>,
    systems: State<'_, FileSystems>,
    id: String,
    volume: String,
) -> Result<OpenedVolume, CommandError> {
    let fs = systems.get_or_open(&sources, (&scans, &raws), &id, &volume)?;
    Ok(OpenedVolume {
        info: fs.info().clone(),
        root: fs.root(),
    })
}

/// A folder's entries, deleted ones included.
#[tauri::command(async)]
pub fn list_directory(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    raws: State<'_, RawScans>,
    systems: State<'_, FileSystems>,
    id: String,
    volume: String,
    dir: EntryId,
) -> Result<Vec<Entry>, CommandError> {
    let fs = systems.get_or_open(&sources, (&scans, &raws), &id, &volume)?;
    Ok(fs.list(&dir)?)
}

/// Up to `length` bytes of a file (or NTFS stream) from `start`, as raw bytes.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)] // each is one field of the request
pub fn read_file(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    raws: State<'_, RawScans>,
    systems: State<'_, FileSystems>,
    id: String,
    volume: String,
    file: EntryId,
    stream: Option<String>,
    start: u64,
    length: u64,
) -> Result<Response, CommandError> {
    let fs = systems.get_or_open(&sources, (&scans, &raws), &id, &volume)?;
    let length = usize::try_from(length.min(MAX_FILE_READ)).unwrap_or(0);
    let mut data = vec![0; length];
    let mut done = 0;
    while done < length {
        let n = fs.read(
            &file,
            stream.as_deref(),
            start + done as u64,
            &mut data[done..],
        )?;
        if n == 0 {
            break;
        }
        done += n;
    }
    data.truncate(done);
    Ok(Response::new(data))
}

/// Where a file's data is on the source.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)] // each is one field of the request
pub fn file_extents(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    raws: State<'_, RawScans>,
    systems: State<'_, FileSystems>,
    id: String,
    volume: String,
    file: EntryId,
    stream: Option<String>,
) -> Result<Vec<Extent>, CommandError> {
    let fs = systems.get_or_open(&sources, (&scans, &raws), &id, &volume)?;
    Ok(fs.extents(&file, stream.as_deref())?)
}

/// Recoveries running, by source id: set to stop one.
#[derive(Default)]
pub struct Recoveries(Mutex<HashMap<String, Arc<AtomicBool>>>);

impl Recoveries {
    /// The sources files are being recovered from.
    pub fn sources_in_use(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

impl FileSystems {
    /// Forgets every open file system, after a partition table changed.
    pub fn clear(&self) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

/// Saves `items` (or, without them, everything on the volume but NTFS's own
/// system files) into `destination`, reporting progress on `progress`.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)] // each is one field of the request
pub fn recover_files(
    sources: State<'_, OpenSources>,
    scans: State<'_, Scans>,
    raws: State<'_, RawScans>,
    systems: State<'_, FileSystems>,
    recoveries: State<'_, Recoveries>,
    id: String,
    volume: String,
    items: Option<Vec<Entry>>,
    destination: PathBuf,
    deleted: bool,
    progress: Channel<RecoverProgress>,
) -> Result<RecoverSummary, CommandError> {
    let spec = SourceSpec::parse(&id);
    datareco_scan::check_destination(&spec, &destination)?;
    let fs = systems.get_or_open(&sources, (&scans, &raws), &id, &volume)?;
    let items = match items {
        Some(items) => items,
        None => fs
            .list(&fs.root())?
            .into_iter()
            .filter(|e| !datareco_scan::is_system_file(e))
            .collect(),
    };
    let stop = Arc::new(AtomicBool::new(false));
    recoveries
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(spec.id(), Arc::clone(&stop));
    let info = fs.info();
    let label = info
        .label
        .as_ref()
        .map(|l| format!(" \"{l}\""))
        .unwrap_or_default();
    let describe = format!(
        "{}, {}{label} at byte {}",
        spec.display_name(),
        info.name,
        info.offset
    );
    let mut sent = Instant::now();
    let result = datareco_scan::recover(
        fs.as_ref(),
        &describe,
        &items,
        &destination,
        RecoverOptions {
            deleted,
            streams: true,
        },
        &mut |p| {
            if sent.elapsed() >= Duration::from_millis(150) {
                let _ = progress.send(p.clone());
                sent = Instant::now();
            }
            if stop.load(Ordering::Relaxed) {
                Control::Stop
            } else {
                Control::Continue
            }
        },
    );
    recoveries
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&spec.id());
    Ok(result?)
}

/// Stops the source's recovery after the file it's on.
#[tauri::command]
pub fn stop_recovery(recoveries: State<'_, Recoveries>, id: String) {
    if let Some(stop) = recoveries
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&SourceSpec::parse(&id).id())
    {
        stop.store(true, Ordering::Relaxed);
    }
}

/// Shows a folder in the file manager (where files were recovered to).
#[tauri::command]
pub fn show_folder(path: PathBuf) -> Result<(), CommandError> {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| CommandError::new("io", format!("couldn't open {}: {e}", path.display())))
}
