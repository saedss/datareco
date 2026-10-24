//! Copying a source into an image file or onto another disk, a step at a
//! time, with a map file to carry on later.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use datareco_imaging::{Copier, CopyError, CopyOptions, CopyProgress, Map, MapError, Totals};
use datareco_io::{
    CopySource, DeviceList, DiskTarget, DiskTargetCheck, FileSource, ImageTarget, SectorSource,
    SectorTarget, SourceSpec, check_disk_target, check_outside_source, full_path, list_devices,
    same_path,
};
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::{CommandError, OpenSources};

/// How long each step copies before reporting back.
const STEP: Duration = Duration::from_millis(400);
/// How often the map is saved while copying.
const SAVE_EVERY: Duration = Duration::from_secs(3);
/// Squares in the picture of the map.
const PICTURE: usize = 960;

impl From<CopyError> for CommandError {
    fn from(error: CopyError) -> Self {
        CommandError::new("copy", error.to_string())
    }
}

impl From<MapError> for CommandError {
    fn from(error: MapError) -> Self {
        CommandError::new("copy", error.to_string())
    }
}

/// Copies running, by source id.
#[derive(Default)]
pub struct Copies(Mutex<HashMap<String, Session>>);

impl Copies {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Session>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The sources being copied, and the disks being copied onto.
    pub fn sources_in_use(&self) -> Vec<String> {
        self.lock()
            .iter()
            .flat_map(|(id, session)| {
                std::iter::once(id.clone()).chain(session.copier.map().target.clone())
            })
            .collect()
    }
}

struct Session {
    copier: Copier,
    source: FileSource,
    /// Dropping it closes the file, or unlocks the disk's volumes.
    target: Box<dyn SectorTarget>,
    map_path: PathBuf,
    saved_at: Instant,
    started: Instant,
    copied_at_start: u64,
}

impl Session {
    /// Stores what was written, then the map that says so.
    fn save(&mut self) -> Result<(), CommandError> {
        self.target
            .flush()
            .map_err(|e| CommandError::new("copy", format!("Writing to the target failed: {e}")))?;
        self.copier.map().save(&self.map_path)?;
        self.saved_at = Instant::now();
        Ok(())
    }

    fn status(&self, progress: CopyProgress, running: bool) -> CopyStatus {
        let seconds = self.started.elapsed().as_secs();
        CopyStatus {
            rate: (progress.totals.copied.saturating_sub(self.copied_at_start))
                .checked_div(seconds),
            picture: self.copier.map().picture(PICTURE),
            map_path: self.map_path.display().to_string(),
            progress,
            running,
        }
    }
}

/// Where to copy to.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CopyTarget {
    Image { path: PathBuf },
    Disk { number: u32 },
}

/// How a copy is going.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyStatus {
    progress: CopyProgress,
    /// The map in squares, one symbol each: `+` copied, `?` not tried, `*`
    /// and `/` failed and being worked on, `-` unreadable.
    picture: String,
    map_path: String,
    /// Bytes copied per second since the copy was started or carried on.
    rate: Option<u64>,
    /// Whether the copy is still open: not finished, stopped or failed.
    running: bool,
}

/// A disk the copy could go onto.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskChoice {
    number: u32,
    name: String,
    model: String,
    size: u64,
    /// What writing to it would do, when it may be written to.
    check: Option<DiskTargetCheck>,
    /// Why it can't be, otherwise.
    problem: Option<String>,
}

/// What a map file says about a copy.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MapInfo {
    source: Option<String>,
    target: Option<String>,
    size: u64,
    totals: Totals,
    finished: bool,
    picture: String,
}

fn copy_source<'a>(spec: &'a SourceSpec, source: &FileSource) -> CopySource<'a> {
    CopySource {
        spec,
        size: source.size(),
        sector_size: source.sector_size(),
    }
}

/// The disks this source could be copied onto, and why the others can't.
#[tauri::command(async)]
pub fn copy_disk_choices(
    sources: State<'_, OpenSources>,
    id: String,
) -> Result<Vec<DiskChoice>, CommandError> {
    let spec = SourceSpec::parse(&id);
    let disk = sources.get_or_open(&id)?;
    let devices = list_devices()?;
    let source = CopySource {
        spec: &spec,
        size: disk.size(),
        sector_size: disk.sector_size(),
    };
    let keep: Vec<&Path> = match &spec {
        SourceSpec::Image(path) => vec![path.as_path()],
        _ => Vec::new(),
    };
    Ok(devices
        .disks
        .iter()
        .map(|d| {
            let (check, problem) = match check_disk_target(d.number, &source, &keep, &devices) {
                Ok(check) => (Some(check), None),
                Err(error) => (None, Some(error.to_string())),
            };
            DiskChoice {
                number: d.number,
                name: SourceSpec::PhysicalDisk(d.number).display_name(),
                model: d.model.clone(),
                size: d.size,
                check,
                problem,
            }
        })
        .collect())
}

/// What a map file says, to carry on its copy.
#[tauri::command(async)]
pub fn read_copy_map(path: PathBuf) -> Result<MapInfo, CommandError> {
    let map = Map::load(&path)?;
    let totals = map.totals();
    Ok(MapInfo {
        source: map.source.clone(),
        target: map.target.clone(),
        size: map.size(),
        totals,
        finished: map.phase == datareco_imaging::Phase::Finished,
        picture: map.picture(PICTURE),
    })
}

fn refused(message: impl Into<String>) -> CommandError {
    CommandError::new("refused", message)
}

/// Starts a copy, or carries one on when `map_path` exists. Writing to a
/// disk needs its `confirmation` typed (`disk2`). Doesn't copy yet: call
/// `continue_copy`.
#[tauri::command(async)]
pub fn start_copy(
    copies: State<'_, Copies>,
    id: String,
    target: CopyTarget,
    map_path: PathBuf,
    retries: u32,
    skip: bool,
    confirmation: Option<String>,
) -> Result<CopyStatus, CommandError> {
    let spec = SourceSpec::parse(&id);
    let key = spec.id();
    // Long, absolute paths: file dialogs can hand back short 8.3 names.
    let map_path = full_path(&map_path);
    let target = match target {
        CopyTarget::Image { path } => CopyTarget::Image {
            path: full_path(&path),
        },
        disk => disk,
    };
    if copies.lock().contains_key(&key) {
        return Err(refused("This source is being copied already."));
    }
    let devices = list_devices().unwrap_or_else(|_| DeviceList::default());
    check_outside_source(&spec, &map_path, &devices)?;
    let source = FileSource::open(&spec)?;
    let size = source.size();
    let resuming = map_path.exists();
    let mut map = if resuming {
        let map = Map::load(&map_path)?;
        if map.size() != size {
            return Err(refused(format!(
                "The map is for a source of {} bytes, but this one has {size} bytes. Is it the \
                 right map?",
                map.size()
            )));
        }
        map
    } else {
        let mut map = Map::new(size);
        map.source = Some(key.clone());
        map
    };
    let (target, fill_bad): (Box<dyn SectorTarget>, bool) = match &target {
        CopyTarget::Image { path } => {
            if same_path(path, &map_path)
                || matches!(&spec, SourceSpec::Image(source) if same_path(source, path))
            {
                return Err(refused(
                    "The image, the map and the source must be three different files.",
                ));
            }
            check_outside_source(&spec, path, &devices)?;
            map.target = Some(SourceSpec::Image(path.clone()).id());
            let image = if resuming {
                if !path.exists() {
                    return Err(refused(format!(
                        "The map is for a copy into {}, which isn't there.",
                        path.display()
                    )));
                }
                ImageTarget::resume(path, size)?
            } else {
                ImageTarget::create(path, size)?
            };
            (Box::new(image), false)
        }
        CopyTarget::Disk { number } => {
            let mut keep: Vec<&Path> = vec![&map_path];
            if let SourceSpec::Image(path) = &spec {
                keep.push(path);
            }
            let check = check_disk_target(*number, &copy_source(&spec, &source), &keep, &devices)?;
            map.target = Some(SourceSpec::PhysicalDisk(*number).id());
            let disk = DiskTarget::open(&check, confirmation.as_deref().unwrap_or_default())?;
            (Box::new(disk), true)
        }
    };
    let options = CopyOptions {
        skip,
        retries,
        fill_bad,
        ..CopyOptions::default()
    };
    let copier = Copier::new(map, source.sector_size(), options)?;
    let copied_at_start = copier.map().totals().copied;
    let mut session = Session {
        copier,
        source,
        target,
        map_path,
        saved_at: Instant::now(),
        started: Instant::now(),
        copied_at_start,
    };
    session.save()?;
    let progress = session.copier.progress();
    let running = !progress.finished;
    let status = session.status(progress, running);
    if running {
        copies.lock().insert(key, session);
    }
    Ok(status)
}

/// Copies for a moment and says how it's going. When the copy finishes or
/// fails, its map is saved and the target closed.
#[tauri::command(async)]
pub fn continue_copy(copies: State<'_, Copies>, id: String) -> Result<CopyStatus, CommandError> {
    let key = SourceSpec::parse(&id).id();
    // Taken out while it works, so other copies aren't held up.
    let mut session = copies
        .lock()
        .remove(&key)
        .ok_or_else(|| CommandError::new("not_found", "No copy of this source is running."))?;
    let Session {
        copier,
        source,
        target,
        ..
    } = &mut session;
    match copier.step(source, target.as_mut(), STEP) {
        Ok(progress) => {
            if progress.finished || session.saved_at.elapsed() >= SAVE_EVERY {
                session.save()?;
            }
            let running = !progress.finished;
            let status = session.status(progress, running);
            if running {
                copies.lock().insert(key, session);
            }
            Ok(status)
        }
        Err(error) => {
            let _ = session.save();
            Err(error.into())
        }
    }
}

/// Stops a copy: saves its map and closes the target. Carry it on with
/// `start_copy` and the same map.
#[tauri::command(async)]
pub fn stop_copy(
    copies: State<'_, Copies>,
    id: String,
) -> Result<Option<CopyStatus>, CommandError> {
    let key = SourceSpec::parse(&id).id();
    let Some(mut session) = copies.lock().remove(&key) else {
        return Ok(None);
    };
    session.save()?;
    let progress = session.copier.progress();
    Ok(Some(session.status(progress, false)))
}
