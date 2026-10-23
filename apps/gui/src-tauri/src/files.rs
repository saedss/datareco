//! Browsing FAT, exFAT and NTFS volumes: folders, files, deleted entries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use datareco_fs::{Entry, EntryId, Extent, FileSystem, FsError, FsKind, VolumeInfo};
use datareco_partitions::read_table;
use serde::Serialize;
use tauri::State;
use tauri::ipc::Response;

use crate::{CommandError, OpenSources};

/// Most bytes one `read_file` call returns (enough to preview a photo).
const MAX_FILE_READ: u64 = 16 << 20;

impl From<FsError> for CommandError {
    fn from(error: FsError) -> Self {
        CommandError::new(error.kind(), error.to_string())
    }
}

/// Opened file systems, by source id and offset. Opening NTFS reads the
/// whole MFT on the first listing, so it's done once.
#[derive(Default)]
pub struct FileSystems(Mutex<HashMap<(String, u64), Arc<dyn FileSystem>>>);

impl FileSystems {
    fn get_or_open(
        &self,
        sources: &OpenSources,
        id: &str,
        offset: u64,
    ) -> Result<Arc<dyn FileSystem>, CommandError> {
        let key = (datareco_io::SourceSpec::parse(id).id(), offset);
        if let Some(fs) = self.lock().get(&key) {
            return Ok(Arc::clone(fs));
        }
        let disk = sources.get_or_open(id)?;
        let fs: Arc<dyn FileSystem> = Arc::from(datareco_fs::open(disk, offset)?);
        Ok(Arc::clone(self.lock().entry(key).or_insert(fs)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, u64), Arc<dyn FileSystem>>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A volume on a source that the file browser can offer.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeChoice {
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

fn readable(kind: FsKind) -> bool {
    matches!(
        kind,
        FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32 | FsKind::Exfat | FsKind::Ntfs
    )
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
            offset: 0,
            partition: None,
            kind: fs.kind,
            name: fs.name.clone(),
            label: fs.label.clone(),
            size: fs.size,
            readable: readable(fs.kind),
        });
    }
    for partition in &table.partitions {
        let Some(fs) = &partition.file_system else {
            continue;
        };
        volumes.push(VolumeChoice {
            offset: partition.start_lba * u64::from(table.sector_size),
            partition: Some(partition.number),
            kind: fs.kind,
            name: fs.name.clone(),
            label: fs.label.clone(),
            size: Some(partition.sectors * u64::from(table.sector_size)),
            readable: readable(fs.kind),
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
    systems: State<'_, FileSystems>,
    id: String,
    offset: u64,
) -> Result<OpenedVolume, CommandError> {
    let fs = systems.get_or_open(&sources, &id, offset)?;
    Ok(OpenedVolume {
        info: fs.info().clone(),
        root: fs.root(),
    })
}

/// A folder's entries, deleted ones included.
#[tauri::command(async)]
pub fn list_directory(
    sources: State<'_, OpenSources>,
    systems: State<'_, FileSystems>,
    id: String,
    offset: u64,
    dir: EntryId,
) -> Result<Vec<Entry>, CommandError> {
    let fs = systems.get_or_open(&sources, &id, offset)?;
    Ok(fs.list(&dir)?)
}

/// Up to `length` bytes of a file (or NTFS stream) from `start`, as raw bytes.
#[tauri::command(async)]
#[allow(clippy::too_many_arguments)] // each is one field of the request
pub fn read_file(
    sources: State<'_, OpenSources>,
    systems: State<'_, FileSystems>,
    id: String,
    offset: u64,
    file: EntryId,
    stream: Option<String>,
    start: u64,
    length: u64,
) -> Result<Response, CommandError> {
    let fs = systems.get_or_open(&sources, &id, offset)?;
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
pub fn file_extents(
    sources: State<'_, OpenSources>,
    systems: State<'_, FileSystems>,
    id: String,
    offset: u64,
    file: EntryId,
    stream: Option<String>,
) -> Result<Vec<Extent>, CommandError> {
    let fs = systems.get_or_open(&sources, &id, offset)?;
    Ok(fs.extents(&file, stream.as_deref())?)
}
