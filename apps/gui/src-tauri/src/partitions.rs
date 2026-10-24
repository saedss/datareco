//! Partition tables and the lost-partition search.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use datareco_partitions::{
    FoundPartition, PartitionTable, Scan, ScanMode, ScanProgress, read_table,
};
use serde::Serialize;
use tauri::State;

use crate::{CommandError, OpenSources};

/// Bytes each search step examines before reporting back.
const SEARCH_STEP: u64 = 128 << 20;

/// Searches in progress, by source id.
#[derive(Default)]
pub struct Searches(Mutex<HashMap<String, Search>>);

impl Searches {
    /// The sources being searched.
    pub fn sources_in_use(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }
}

pub struct Search {
    scan: Scan,
    table: PartitionTable,
}

/// How a search is going, and what it has found so far.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchStatus {
    mode: ScanMode,
    progress: ScanProgress,
    found: Vec<FoundPartition>,
}

#[tauri::command(async)]
pub fn read_partitions(
    sources: State<'_, OpenSources>,
    id: String,
) -> Result<PartitionTable, CommandError> {
    let disk = sources.get_or_open(&id)?;
    Ok(read_table(&disk)?)
}

/// Starts a search (replacing any running one for this source) and does a first step.
#[tauri::command(async)]
pub fn start_partition_search(
    sources: State<'_, OpenSources>,
    searches: State<'_, Searches>,
    id: String,
    mode: ScanMode,
) -> Result<SearchStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let search = Search {
        scan: Scan::new(&disk, mode),
        table: read_table(&disk)?,
    };
    searches
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id.clone(), search);
    continue_partition_search(sources, searches, id)
}

/// Searches one more step; call again until `progress.done`.
#[tauri::command(async)]
pub fn continue_partition_search(
    sources: State<'_, OpenSources>,
    searches: State<'_, Searches>,
    id: String,
) -> Result<SearchStatus, CommandError> {
    let disk = sources.get_or_open(&id)?;
    let mut all = searches.0.lock().unwrap_or_else(PoisonError::into_inner);
    let search = all.get_mut(&id).ok_or_else(|| {
        CommandError::new("io", "No partition search is running for this source.")
    })?;
    let progress = search.scan.step(&disk, SEARCH_STEP)?;
    let found = search.scan.results(&disk, Some(&search.table))?;
    let status = SearchStatus {
        mode: search.scan.mode(),
        progress,
        found,
    };
    if status.progress.done {
        all.remove(&id);
    }
    Ok(status)
}

#[tauri::command]
pub fn stop_partition_search(searches: State<'_, Searches>, id: String) {
    searches
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&id);
}
