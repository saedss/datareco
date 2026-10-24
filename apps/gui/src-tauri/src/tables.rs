//! Changing partition tables from the Disk view: a preview of pending
//! changes, writing them once the disk's name is typed (after backing up
//! the sectors they replace), and undo.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datareco_io::{
    DeviceList, Disk, DiskTarget, ImageTarget, SectorTarget, SourceSpec, check_disk_for_changes,
    list_devices,
};
use datareco_partitions::edit::{
    BackupInfo, Change, Operation, Plan, Style, TypeChoice, apply_writes, default_backup_dir,
    list_backups, plan, preview, read_backup, type_choices, undo_writes, verify_writes,
};
use datareco_partitions::{DiskMap, disk_map};
use serde::Serialize;
use tauri::State;

use crate::copies::Copies;
use crate::files::{FileSystems, Recoveries};
use crate::health::DiskTests;
use crate::partitions::Searches;
use crate::volumes::Wipes;
use crate::{CommandError, OpenSources};

/// The Disk view's table: as it would be after the pending changes.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TableView {
    plan: Plan,
    /// The disk's map after the changes (as it is, with none).
    map: DiskMap,
    /// What to type to confirm writing, or why changes can't be written.
    confirmation: Option<String>,
    not_editable: Option<String>,
    /// The volumes Windows has mounted on the disk, closed while writing.
    volumes: Vec<String>,
    types_mbr: Vec<TypeChoice>,
    types_gpt: Vec<TypeChoice>,
    /// Earlier changes' backups, newest first.
    backups: Vec<BackupInfo>,
}

/// The state the commands that write need.
pub struct Busy<'a> {
    pub searches: State<'a, Searches>,
    pub recoveries: State<'a, Recoveries>,
    pub copies: State<'a, Copies>,
    pub tests: State<'a, DiskTests>,
    pub wipes: State<'a, Wipes>,
}

/// The source opened anew (and its cache entry dropped), so what's read
/// is what the disk holds now.
fn fresh(sources: &OpenSources, spec: &SourceSpec) -> Result<Arc<Disk>, CommandError> {
    sources.forget(&spec.id());
    Ok(Arc::new(Disk::open(spec)?))
}

fn refused(message: impl Into<String>) -> CommandError {
    CommandError::new("refused", message)
}

/// Works out what `ops` would do, without writing.
#[tauri::command(async)]
pub fn plan_table(
    sources: State<'_, OpenSources>,
    id: String,
    ops: Vec<Operation>,
) -> Result<TableView, CommandError> {
    let spec = SourceSpec::parse(&id);
    // Read fresh: the cache may hold sectors from before a change made
    // outside DataReco.
    let disk = fresh(&sources, &spec)?;
    let plan = plan(&disk, &ops)?;
    let devices = list_devices().ok();
    let after = preview(Arc::clone(&disk), &plan.writes);
    let map = disk_map(&after, &spec, devices.as_ref())?;
    let (confirmation, not_editable, volumes) = match &spec {
        SourceSpec::Volume(_) => (
            None,
            Some("Partition tables belong to disks: pick the disk this volume is on, in the list on the left.".to_owned()),
            Vec::new(),
        ),
        SourceSpec::Image(_) => (Some(spec.display_name()), None, Vec::new()),
        SourceSpec::PhysicalDisk(number) => match devices
            .as_ref()
            .map(|d| check_disk_for_changes(*number, d))
        {
            Some(Ok(check)) => (Some(check.confirmation), None, check.volumes),
            Some(Err(e)) => (None, Some(e.to_string()), Vec::new()),
            None => (None, Some("The disks on this computer can't be listed.".into()), Vec::new()),
        },
    };
    Ok(TableView {
        plan,
        map,
        confirmation,
        not_editable,
        volumes,
        types_mbr: type_choices(Style::Mbr),
        types_gpt: type_choices(Style::Gpt),
        backups: list_backups(&default_backup_dir(), &spec.id()),
    })
}

/// What a change wrote, for the person.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    /// The backup file, to undo it.
    backup: PathBuf,
}

/// Writes `ops` to the disk, once `confirmation` is its name.
#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)] // Tauri hands over each piece of state
pub fn apply_table(
    sources: State<'_, OpenSources>,
    file_systems: State<'_, FileSystems>,
    searches: State<'_, Searches>,
    recoveries: State<'_, Recoveries>,
    copies: State<'_, Copies>,
    tests: State<'_, DiskTests>,
    wipes: State<'_, Wipes>,
    id: String,
    ops: Vec<Operation>,
    confirmation: String,
) -> Result<Applied, CommandError> {
    let spec = SourceSpec::parse(&id);
    let busy = Busy {
        searches,
        recoveries,
        copies,
        tests,
        wipes,
    };
    check_not_busy(&spec, &busy)?;
    // Planned again on a freshly opened disk: what's written must be what
    // the disk holds now, not what a cache remembers.
    let disk = Disk::open(&spec)?;
    let plan = plan(&disk, &ops)?;
    if let Some(reason) = plan.blocked {
        return Err(refused(reason));
    }
    if let Some(error) = plan.error {
        return Err(refused(error.message));
    }
    if plan.writes.is_empty() {
        return Err(refused("There's nothing to change."));
    }
    let changes = plan.descriptions();
    let change = Change {
        source: &spec.id(),
        changes: &changes,
        writes: &plan.writes,
        undoes: None,
    };
    let backup = write(&spec, &disk, &change, &confirmation);
    sources.clear();
    file_systems.clear();
    Ok(Applied { backup: backup? })
}

/// Undoing a backed-up change: what it would put back, or why it can't.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoView {
    backup: PathBuf,
    changes: Vec<String>,
    created: u64,
    /// The map once undone.
    map: Option<DiskMap>,
    refusal: Option<String>,
}

#[tauri::command(async)]
pub fn preview_undo(
    sources: State<'_, OpenSources>,
    id: String,
    backup: String,
) -> Result<UndoView, CommandError> {
    let spec = SourceSpec::parse(&id);
    let disk = fresh(&sources, &spec)?;
    let path = PathBuf::from(&backup);
    let saved = read_backup(&path).map_err(refused)?;
    let mut view = UndoView {
        backup: path,
        changes: saved.changes.clone(),
        created: saved.created,
        map: None,
        refusal: None,
    };
    match undo_writes(&disk, &saved)? {
        Ok(writes) => {
            let after = preview(Arc::clone(&disk), &writes);
            let devices = list_devices().ok();
            view.map = Some(disk_map(&after, &spec, devices.as_ref())?);
        }
        Err(reason) => view.refusal = Some(reason),
    }
    Ok(view)
}

#[tauri::command(async)]
#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)] // Tauri hands over each piece of state
pub fn undo_table(
    sources: State<'_, OpenSources>,
    file_systems: State<'_, FileSystems>,
    searches: State<'_, Searches>,
    recoveries: State<'_, Recoveries>,
    copies: State<'_, Copies>,
    tests: State<'_, DiskTests>,
    wipes: State<'_, Wipes>,
    id: String,
    backup: String,
    confirmation: String,
) -> Result<Applied, CommandError> {
    let spec = SourceSpec::parse(&id);
    let busy = Busy {
        searches,
        recoveries,
        copies,
        tests,
        wipes,
    };
    check_not_busy(&spec, &busy)?;
    let disk = Disk::open(&spec)?;
    let path = PathBuf::from(&backup);
    let saved = read_backup(&path).map_err(refused)?;
    let writes = undo_writes(&disk, &saved)?.map_err(refused)?;
    let changes: Vec<String> = saved.changes.iter().map(|c| format!("Undo: {c}")).collect();
    let change = Change {
        source: &spec.id(),
        changes: &changes,
        writes: &writes,
        undoes: Some(&path),
    };
    let result = write(&spec, &disk, &change, &confirmation);
    sources.clear();
    file_systems.clear();
    Ok(Applied { backup: result? })
}

/// Refuses while something else uses the disk: a copy to or from it, a
/// recovery, a lost-partition search, a surface or speed test, a wipe.
pub(crate) fn check_not_busy(spec: &SourceSpec, busy: &Busy<'_>) -> Result<(), CommandError> {
    let devices = list_devices().unwrap_or_default();
    let disks = devices.disks_of_source(spec);
    let uses = |ids: Vec<String>| {
        ids.iter().any(|id| {
            let other = SourceSpec::parse(id);
            other == *spec
                || devices
                    .disks_of_source(&other)
                    .iter()
                    .any(|d| disks.contains(d))
        })
    };
    let what = if uses(busy.copies.sources_in_use()) {
        Some("A copy")
    } else if uses(busy.recoveries.sources_in_use()) {
        Some("Recovering files")
    } else if uses(busy.searches.sources_in_use()) {
        Some("A lost-partition search")
    } else if uses(busy.tests.sources_in_use()) {
        Some("A surface or speed test")
    } else if uses(busy.wipes.sources_in_use()) {
        Some("Wiping")
    } else {
        None
    };
    match what {
        Some(what) => Err(refused(format!(
            "{what} is using {}. Stop it first, then try again.",
            spec.display_name()
        ))),
        None => Ok(()),
    }
}

/// Opens the disk (or image) for writing once `confirmation` matches,
/// writes the change after backing up what it replaces, and checks it
/// reads back.
fn write(
    spec: &SourceSpec,
    disk: &Disk,
    change: &Change<'_>,
    confirmation: &str,
) -> Result<PathBuf, CommandError> {
    let backup_dir = default_backup_dir();
    let mut target: Box<dyn SectorTarget> = match spec {
        SourceSpec::PhysicalDisk(number) => {
            let devices: DeviceList = list_devices()?;
            let check = check_disk_for_changes(*number, &devices)?;
            refuse_backups_on(&devices, *number, &backup_dir)?;
            Box::new(DiskTarget::open(&check, confirmation)?)
        }
        SourceSpec::Image(path) => {
            let name = spec.display_name();
            if !confirmation.trim().eq_ignore_ascii_case(&name) {
                return Err(refused(format!(
                    "Type {name} to confirm changing this image's partition table."
                )));
            }
            Box::new(ImageTarget::resume(path, disk.size())?)
        }
        SourceSpec::Volume(_) => {
            return Err(refused("Partition tables belong to disks: pick the disk."));
        }
    };
    let backup = apply_writes(disk, change, target.as_mut(), &backup_dir)?;
    // Closing the disk unlocks its volumes, and Windows reads the new table.
    drop(target);
    let wrong = verify_writes(&Disk::open(spec)?, change.writes)?;
    if !wrong.is_empty() {
        return Err(CommandError::new(
            "io",
            format!(
                "Sectors {wrong:?} didn't read back as written. The old table is saved in {}.",
                backup.display()
            ),
        ));
    }
    Ok(backup)
}

fn refuse_backups_on(devices: &DeviceList, number: u32, dir: &Path) -> Result<(), CommandError> {
    if devices.disks_of_path(dir).contains(&number) {
        return Err(refused(format!(
            "The backup folder {} is on the disk being changed.",
            dir.display()
        )));
    }
    Ok(())
}
