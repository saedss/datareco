//! Searching a disk for lost partitions.
//!
//! The search collects clues (boot sectors and their backups, ext
//! superblocks and their copies, GPT headers, partition tables and extended
//! boot records), turns each into the partition it implies, merges clues that
//! agree, and checks every candidate against the structures it says are
//! there (the MFT, the first FAT entry, the superblock) before showing it.

use std::collections::BTreeMap;

use datareco_io::{Disk, Result};
use serde::{Deserialize, Serialize};

use crate::boot::{self, Boot, Ext, FileSystem, FsKind, identify};
use crate::bytes::{has, u8_at};
use crate::table::{PartitionTable, human_size};
use crate::{gpt, mbr};

/// Sectors read at each place a quick search looks: enough to cover a
/// volume's boot sector, ext superblock and FAT32/exFAT backup boot sectors.
const QUICK_WINDOW: u64 = 16;
/// Sectors read per pass of a full search.
const FULL_CHUNK: u64 = 2048;
/// Clues kept at most (a disk full of disk images could have millions).
const MAX_CLUES: usize = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScanMode {
    /// Only where partitions usually start (each MiB, old cylinder
    /// boundaries) and the disk's last sectors. Fast.
    Quick,
    /// Every sector. Slow on big disks, but finds anything.
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

/// A partition the search found.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundPartition {
    pub start_lba: u64,
    pub sectors: u64,
    pub file_system: Option<FileSystem>,
    /// Partition type, when a partition table or EBR described it.
    pub type_code: Option<String>,
    pub type_name: Option<String>,
    /// GPT partition name.
    pub name: Option<String>,
    pub confidence: Confidence,
    /// Why DataReco thinks the partition is there, one clue per line.
    pub evidence: Vec<String>,
    /// The current partition table already has this partition.
    pub in_table: bool,
}

/// Where a search got to and what it found, to save it and carry on later
/// with [`Scan::restore`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanState {
    pub mode: ScanMode,
    /// The next sector to look at.
    pub next: u64,
    pub tail_done: bool,
    /// Sectors where clues were found; restoring reads them again.
    pub clue_sectors: Vec<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    /// Bytes examined so far.
    pub position: u64,
    pub total: u64,
    pub done: bool,
    /// Clues found so far.
    pub clues: usize,
}

/// Something on disk that says where a volume starts. Offsets in bytes.
#[derive(Clone, Debug)]
enum Clue {
    Boot { offset: u64, boot: Boot },
    Ext { offset: u64, ext: Ext },
    Gpt { offset: u64, header: gpt::Header },
    Table { offset: u64, table: mbr::Mbr },
}

impl Clue {
    fn offset(&self) -> u64 {
        match self {
            Clue::Boot { offset, .. }
            | Clue::Ext { offset, .. }
            | Clue::Gpt { offset, .. }
            | Clue::Table { offset, .. } => *offset,
        }
    }
}

/// A search in progress. Call [`Scan::step`] until it's done, then
/// [`Scan::results`]; results can also be read at any point along the way.
pub struct Scan {
    mode: ScanMode,
    sector_size: u64,
    sectors: u64,
    /// Next sector to look at.
    next: u64,
    /// Sectors before this have been examined (quick windows can overlap).
    examined_until: u64,
    tail_done: bool,
    clues: Vec<Clue>,
}

impl Scan {
    pub fn new(disk: &Disk, mode: ScanMode) -> Scan {
        Scan {
            mode,
            sector_size: u64::from(disk.sector_size()),
            sectors: disk.sector_count(),
            next: 0,
            examined_until: 0,
            tail_done: false,
            clues: Vec::new(),
        }
    }

    pub fn mode(&self) -> ScanMode {
        self.mode
    }

    pub fn progress(&self) -> ScanProgress {
        let total = self.sectors * self.sector_size;
        ScanProgress {
            position: (self.next * self.sector_size).min(total),
            total,
            done: self.tail_done,
            clues: self.clues.len(),
        }
    }

    /// Examines about `limit` bytes' worth of the disk.
    pub fn step(&mut self, disk: &Disk, limit: u64) -> Result<ScanProgress> {
        let mut budget = (limit / self.sector_size).max(1);
        while budget > 0 && self.next < self.sectors {
            let (start, count) = match self.mode {
                ScanMode::Full => (self.next, FULL_CHUNK.min(self.sectors - self.next)),
                ScanMode::Quick => {
                    let place = next_quick_place(self.next);
                    if place >= self.sectors {
                        self.next = self.sectors;
                        break;
                    }
                    (place, QUICK_WINDOW.min(self.sectors - place))
                }
            };
            self.examine(disk, start, count)?;
            self.next = match self.mode {
                ScanMode::Full => start + count,
                ScanMode::Quick => start + 1,
            };
            budget = budget.saturating_sub(count);
        }
        if self.next >= self.sectors && !self.tail_done {
            // Backup GPT headers and NTFS backup boot sectors live at the very end.
            let count = QUICK_WINDOW.min(self.sectors);
            self.examined_until = self.examined_until.min(self.sectors - count);
            self.examine(disk, self.sectors - count, count)?;
            self.tail_done = true;
        }
        Ok(self.progress())
    }

    /// Runs the whole search and returns what it found.
    pub fn run(
        disk: &Disk,
        mode: ScanMode,
        table: Option<&PartitionTable>,
    ) -> Result<Vec<FoundPartition>> {
        let mut scan = Scan::new(disk, mode);
        while !scan.step(disk, 64 << 20)?.done {}
        scan.results(disk, table)
    }

    fn examine(&mut self, disk: &Disk, start: u64, count: u64) -> Result<()> {
        let first = start.max(self.examined_until);
        let end = start + count;
        if first >= end {
            return Ok(());
        }
        let mut data = vec![0; usize::try_from((end - first) * self.sector_size).unwrap_or(0)];
        disk.read_at(first * self.sector_size, &mut data)?;
        self.classify_sectors(first, &data);
        self.examined_until = end;
        Ok(())
    }

    fn classify_sectors(&mut self, first: u64, data: &[u8]) {
        let sector_size = usize::try_from(self.sector_size).unwrap_or(512);
        for (lba, sector) in (first..).zip(data.chunks_exact(sector_size)) {
            if self.clues.len() >= MAX_CLUES {
                break;
            }
            let offset = lba * self.sector_size;
            self.clues
                .extend(classify(sector, offset, self.sector_size));
        }
    }

    /// Looks at sectors the caller has read itself, as a full search would:
    /// `data` holds whole sectors starting at sector `lba`. Feeding them in
    /// order lets one pass over a disk serve several searches; finish with
    /// [`Scan::finish`].
    pub fn feed(&mut self, lba: u64, data: &[u8]) {
        let count = data.len() as u64 / self.sector_size;
        let end = lba.saturating_add(count).min(self.sectors);
        let first = lba.max(self.examined_until);
        if first < end {
            let skip = usize::try_from((first - lba) * self.sector_size).unwrap_or(usize::MAX);
            let take = usize::try_from((end - first) * self.sector_size).unwrap_or(0);
            if let Some(sectors) = data.get(skip..skip.saturating_add(take)) {
                self.classify_sectors(first, sectors);
            }
            self.examined_until = end;
        }
        self.next = self.next.max(end);
    }

    /// Ends a search fed with [`Scan::feed`]: looks at the disk's last
    /// sectors (backup GPT headers, NTFS backup boot sectors).
    pub fn finish(&mut self, disk: &Disk) -> Result<ScanProgress> {
        self.next = self.sectors;
        self.step(disk, 0)
    }

    /// Where the search got to, to save it.
    pub fn state(&self) -> ScanState {
        let mut clue_sectors: Vec<u64> = self
            .clues
            .iter()
            .map(|clue| clue.offset() / self.sector_size)
            .collect();
        clue_sectors.sort_unstable();
        clue_sectors.dedup();
        ScanState {
            mode: self.mode,
            next: self.next,
            tail_done: self.tail_done,
            clue_sectors,
        }
    }

    /// Carries on with a saved search, reading its clues' sectors again.
    pub fn restore(disk: &Disk, state: &ScanState) -> Result<Scan> {
        let mut scan = Scan::new(disk, state.mode);
        let sector_size = usize::try_from(scan.sector_size).unwrap_or(512);
        for &lba in &state.clue_sectors {
            if lba >= scan.sectors {
                continue;
            }
            let mut sector = vec![0; sector_size];
            disk.read_at(lba * scan.sector_size, &mut sector)?;
            scan.classify_sectors(lba, &sector);
        }
        scan.next = state.next.min(scan.sectors);
        scan.examined_until = scan.next;
        scan.tail_done = state.tail_done;
        Ok(scan)
    }

    /// The partitions the clues so far point to, checked against the disk,
    /// best first at each position. `table` marks ones it already has.
    pub fn results(
        &self,
        disk: &Disk,
        table: Option<&PartitionTable>,
    ) -> Result<Vec<FoundPartition>> {
        // Clues agree when they give the same start, size and file system. A
        // reformatted volume can hold two file systems at the same place, so
        // the file system is part of the key.
        let mut candidates: BTreeMap<Key, Candidate> = BTreeMap::new();
        for clue in &self.clues {
            for candidate in self.candidates_from(disk, clue)? {
                candidates
                    .entry(candidate.key())
                    .and_modify(|existing| existing.absorb(&candidate))
                    .or_insert(candidate);
            }
        }
        absorb_file_systems_into_table_entries(&mut candidates);

        let mut found = Vec::new();
        for candidate in candidates.into_values() {
            if candidate.sectors == 0 || candidate.start >= self.sectors {
                continue;
            }
            if let Some(partition) = self.check(disk, candidate, table)? {
                found.push(partition);
            }
        }
        found.sort_by(|a, b| {
            a.start_lba
                .cmp(&b.start_lba)
                .then(b.confidence.cmp(&a.confidence))
        });
        Ok(found)
    }

    fn candidates_from(&self, disk: &Disk, clue: &Clue) -> Result<Vec<Candidate>> {
        let ss = self.sector_size;
        let mut out = Vec::new();
        match clue {
            Clue::Boot { offset, boot } => {
                let lba = offset / ss;
                let fs = boot.file_system();
                let sectors = fs.size.unwrap_or(0).div_ceil(ss);
                let name = fs.name.clone();
                out.push(Candidate::from_structure(
                    lba,
                    sectors,
                    boot.clone().into(),
                    format!("{name} boot sector at LBA {lba}"),
                ));
                // The same sector could be the backup copy; where would the primary be?
                let backup_distance = match boot {
                    Boot::Ntfs(ntfs) => Some(ntfs.total_sectors * ntfs.bytes_per_sector / ss),
                    Boot::Fat(fat) => fat.backup_boot.map(|b| b * fat.bytes_per_sector / ss),
                    Boot::Exfat(exfat) => Some(12 * exfat.bytes_per_sector / ss),
                };
                if let Some(distance) = backup_distance.filter(|&d| d > 0 && d <= lba) {
                    out.push(Candidate::from_structure(
                        lba - distance,
                        sectors,
                        boot.clone().into(),
                        format!("{name} backup boot sector at LBA {lba}"),
                    ));
                }
            }
            Clue::Ext { offset, ext } => {
                if let Some(start) = ext
                    .offset_in_volume()
                    .and_then(|within| offset.checked_sub(within))
                    && start % ss == 0
                {
                    let lba = offset / ss;
                    let what = if ext.group == 0 {
                        format!("{} superblock at LBA {lba}", ext.kind.name())
                    } else {
                        format!(
                            "{} backup superblock (block group {}) at LBA {lba}",
                            ext.kind.name(),
                            ext.group
                        )
                    };
                    out.push(Candidate::from_structure(
                        start / ss,
                        ext.size().div_ceil(ss),
                        Hint::Ext(ext.clone()),
                        what,
                    ));
                }
            }
            Clue::Gpt { offset, header } => {
                let lba = offset / ss;
                let which = if header.my_lba == 1 {
                    "primary"
                } else {
                    "backup"
                };
                let (entries, _) = gpt::read_entries(disk, header)?;
                for entry in entries {
                    let type_code = gpt::guid(&entry.type_guid);
                    out.push(Candidate::from_table(
                        entry.first_lba,
                        entry
                            .last_lba
                            .checked_sub(entry.first_lba)
                            .map_or(0, |n| n + 1),
                        gpt::type_name(&type_code),
                        type_code,
                        entry.name.clone(),
                        format!("GPT {which} header at LBA {lba}, entry {}", entry.index + 1),
                    ));
                }
            }
            Clue::Table { offset, table } => {
                let lba = offset / ss;
                if lba == 0 {
                    for (number, entry) in (1..).zip(&table.entries) {
                        if !entry.is_empty() && !entry.is_extended() && entry.type_code != 0xEE {
                            out.push(Candidate::from_mbr(
                                entry.start,
                                entry,
                                format!("MBR entry {number}"),
                            ));
                        }
                    }
                } else if let [first, ..] = &table.entries
                    && !first.is_empty()
                    && !first.is_extended()
                {
                    out.push(Candidate::from_mbr(
                        lba + first.start,
                        first,
                        format!("extended boot record at LBA {lba}"),
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Checks a candidate against the disk; `None` if it doesn't hold up.
    fn check(
        &self,
        disk: &Disk,
        candidate: Candidate,
        table: Option<&PartitionTable>,
    ) -> Result<Option<FoundPartition>> {
        let start = candidate.start * self.sector_size;
        let at_start = identify(disk, start)?;
        // Check the file system the clues describe. Only a bare table entry
        // falls back to whatever is at its start now.
        let hint = match &candidate.hint {
            Some(hint) => Some(hint.clone()),
            None => hint_at(disk, start)?,
        };
        let verified = match &hint {
            Some(hint) => verify(disk, start, hint)?,
            None => false,
        };
        let file_system = match (&hint, &at_start) {
            (Some(hint), Some(found)) if hint.kind() == found.kind => Some(found.clone()),
            (Some(hint), _) => Some(hint.file_system()),
            (None, found) => found.clone(),
        };
        let from_table = candidate.type_code.is_some();
        let confidence = if verified && (candidate.evidence.len() >= 2 || from_table) {
            Confidence::High
        } else if verified || (from_table && at_start.is_some()) {
            Confidence::Medium
        } else {
            Confidence::Low
        };
        // Unverified guesses from a single boot sector are noise (often the
        // "what if this was a backup" reading of a primary). Table entries stay.
        if confidence == Confidence::Low && !from_table {
            return Ok(None);
        }
        let mut evidence = candidate.evidence;
        if candidate.start.saturating_add(candidate.sectors) > self.sectors {
            evidence.push(format!(
                "runs past the end of the disk (needs {}, the disk ends sooner)",
                human_size(candidate.sectors * self.sector_size)
            ));
        }
        let in_table = table.is_some_and(|t| {
            t.partitions
                .iter()
                .any(|p| p.start_lba == candidate.start && p.sectors == candidate.sectors)
        });
        Ok(Some(FoundPartition {
            start_lba: candidate.start,
            sectors: candidate.sectors,
            file_system,
            type_code: candidate.type_code,
            type_name: candidate.type_name,
            name: candidate.name,
            confidence,
            evidence,
            in_table,
        }))
    }
}

/// The next place a quick search looks, at or after `from`: each MiB
/// (modern alignment), each cylinder of 255 × 63 sectors, and 63 sectors into
/// each cylinder (older Windows and DOS layouts).
fn next_quick_place(from: u64) -> u64 {
    let round_up = |n: u64, step: u64| n.div_ceil(step).saturating_mul(step);
    let mib = round_up(from, 2048);
    let cylinder = round_up(from, 16_065);
    let track = if from <= 63 {
        63
    } else {
        round_up(from - 63, 16_065).saturating_add(63)
    };
    mib.min(cylinder).min(track)
}

/// What a sector at `offset` (bytes) tells us, if anything.
fn classify(sector: &[u8], offset: u64, sector_size: u64) -> Vec<Clue> {
    if let Some(header) = gpt::parse_header(sector)
        && header.crc_ok
        && header.my_lba.checked_mul(sector_size) == Some(offset)
    {
        return vec![Clue::Gpt { offset, header }];
    }
    if let Some(boot) = boot::parse_boot(sector) {
        return vec![Clue::Boot { offset, boot }];
    }
    let mut clues = Vec::new();
    if let Some(ext) = boot::parse_ext(sector) {
        clues.push(Clue::Ext { offset, ext });
    }
    // With 4 KiB sectors the primary superblock is partway into sector 0.
    if sector_size >= 2048
        && let Some(ext) = sector.get(1024..).and_then(boot::parse_ext)
    {
        clues.push(Clue::Ext {
            offset: offset + 1024,
            ext,
        });
    }
    if clues.is_empty()
        && let Some(table) = mbr::parse(sector)
    {
        clues.push(Clue::Table { offset, table });
    }
    clues
}

/// The structure a candidate was found from, to check it with.
#[derive(Clone, Debug)]
enum Hint {
    Boot(Boot),
    Ext(Ext),
}

impl From<Boot> for Hint {
    fn from(boot: Boot) -> Self {
        Hint::Boot(boot)
    }
}

impl Hint {
    fn file_system(&self) -> FileSystem {
        match self {
            Hint::Boot(boot) => boot.file_system(),
            Hint::Ext(ext) => ext.file_system(),
        }
    }

    fn kind(&self) -> FsKind {
        match self {
            Hint::Boot(boot) => boot.file_system().kind,
            Hint::Ext(ext) => ext.kind,
        }
    }
}

/// Start, size, and the file system if a clue named one.
type Key = (u64, u64, Option<FsKind>);

struct Candidate {
    start: u64,
    sectors: u64,
    evidence: Vec<String>,
    hint: Option<Hint>,
    type_code: Option<String>,
    type_name: Option<String>,
    name: Option<String>,
}

impl Candidate {
    fn from_structure(start: u64, sectors: u64, hint: Hint, evidence: String) -> Self {
        Self {
            start,
            sectors,
            evidence: vec![evidence],
            hint: Some(hint),
            type_code: None,
            type_name: None,
            name: None,
        }
    }

    fn from_table(
        start: u64,
        sectors: u64,
        type_name: &str,
        type_code: String,
        name: Option<String>,
        evidence: String,
    ) -> Self {
        Self {
            start,
            sectors,
            evidence: vec![evidence],
            hint: None,
            type_code: Some(type_code),
            type_name: Some(type_name.to_owned()),
            name,
        }
    }

    fn from_mbr(start: u64, entry: &mbr::Entry, evidence: String) -> Self {
        Self::from_table(
            start,
            entry.sectors,
            mbr::type_name(entry.type_code),
            format!("0x{:02X}", entry.type_code),
            None,
            evidence,
        )
    }

    fn key(&self) -> Key {
        (self.start, self.sectors, self.hint.as_ref().map(Hint::kind))
    }

    fn absorb(&mut self, other: &Candidate) {
        for evidence in &other.evidence {
            if !self.evidence.contains(evidence) {
                self.evidence.push(evidence.clone());
            }
        }
        if self.hint.is_none() {
            self.hint.clone_from(&other.hint);
        }
        if self.type_code.is_none() {
            self.type_code.clone_from(&other.type_code);
            self.type_name.clone_from(&other.type_name);
            self.name.clone_from(&other.name);
        }
    }
}

/// Joins partition-table clues (an MBR, EBR or GPT entry) with the file
/// system found at the same start. A file system often doesn't fill its
/// partition exactly (ext4 rounds down to whole blocks), so it only has to
/// fit. If two different file systems start there, the entry stays apart.
fn absorb_file_systems_into_table_entries(candidates: &mut BTreeMap<Key, Candidate>) {
    let table_keys: Vec<Key> = candidates
        .keys()
        .filter(|key| key.2.is_none())
        .copied()
        .collect();
    for table_key in table_keys {
        let (start, sectors, _) = table_key;
        let fitting: Vec<Key> = candidates
            .keys()
            .filter(|key| key.0 == start && key.1 <= sectors && key.2.is_some())
            .copied()
            .collect();
        let mut kinds: Vec<FsKind> = fitting.iter().filter_map(|key| key.2).collect();
        kinds.dedup();
        let [kind] = kinds[..] else { continue };
        let Some(mut entry) = candidates.remove(&table_key) else {
            continue;
        };
        for key in &fitting {
            if let Some(fs) = candidates.remove(key) {
                entry.absorb(&fs);
            }
        }
        candidates
            .entry((start, sectors, Some(kind)))
            .and_modify(|existing| existing.absorb(&entry))
            .or_insert(entry);
    }
}

/// Reads `len` bytes at `offset`, or `None` if that's past the end of the disk.
fn read_small(disk: &Disk, offset: u64, len: usize) -> Result<Option<Vec<u8>>> {
    if offset.saturating_add(len as u64) > disk.size() {
        return Ok(None);
    }
    let mut data = vec![0; len];
    disk.read_at(offset, &mut data)?;
    Ok(Some(data))
}

/// The boot sector or superblock at the start of a volume, parsed.
fn hint_at(disk: &Disk, start: u64) -> Result<Option<Hint>> {
    let Some(data) = read_small(disk, start, 2048)? else {
        return Ok(None);
    };
    Ok(boot::parse_boot(&data)
        .map(Hint::Boot)
        .or_else(|| data.get(1024..).and_then(boot::parse_ext).map(Hint::Ext)))
}

/// Whether the volume's other structures are where its boot sector or
/// superblock says they are.
fn verify(disk: &Disk, start: u64, hint: &Hint) -> Result<bool> {
    let at = |relative: u64| start.saturating_add(relative);
    match hint {
        Hint::Boot(Boot::Ntfs(ntfs)) => {
            for lcn in [ntfs.mft_lcn, ntfs.mft_mirror_lcn] {
                if let Some(record) =
                    read_small(disk, at(lcn.saturating_mul(ntfs.cluster_size)), 4)?
                    && (record == b"FILE" || record == b"BAAD")
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Hint::Boot(Boot::Fat(fat)) => {
            // The first FAT entry repeats the media byte, then all ones.
            let first = read_small(
                disk,
                at(fat.reserved_sectors.saturating_mul(fat.bytes_per_sector)),
                2,
            )?;
            Ok(first.is_some_and(|entry| {
                u8_at(&entry, 0) == Some(fat.media) && u8_at(&entry, 1) == Some(0xFF)
            }))
        }
        Hint::Boot(Boot::Exfat(exfat)) => {
            let first = read_small(
                disk,
                at(exfat.fat_offset.saturating_mul(exfat.bytes_per_sector)),
                8,
            )?;
            Ok(first.is_some_and(|entries| {
                has(
                    &entries,
                    0,
                    &[0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
                )
            }))
        }
        Hint::Ext(ext) => {
            // The primary superblock, or the first backup, with the same UUID.
            let backup = ext
                .blocks_per_group
                .saturating_mul(ext.block_size)
                .max(2048);
            for offset in [1024, backup] {
                if let Some(data) = read_small(disk, at(offset), 1024)?
                    && boot::parse_ext(&data).is_some_and(|found| found.uuid == ext.uuid)
                {
                    return Ok(true);
                }
            }
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_search_places() {
        assert_eq!(next_quick_place(0), 0);
        assert_eq!(next_quick_place(1), 63);
        assert_eq!(next_quick_place(64), 2048);
        assert_eq!(next_quick_place(2049), 4096);
        assert_eq!(next_quick_place(15_000), 16_065);
        assert_eq!(next_quick_place(16_066), 16_128);
        assert_eq!(next_quick_place(16_129), 16_384);
    }
}
