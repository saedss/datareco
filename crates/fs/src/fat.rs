//! FAT12, FAT16 and FAT32. Layout: Microsoft "FAT: General Overview of On-Disk Format".
//!
//! A volume whose boot sector and FAT are gone can still be read from its
//! folders: a scan works out where the clusters are ([`FatLayout`]), and
//! files are then taken to be in one piece, like deleted ones.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::{Deserialize, Serialize};

use crate::bytes::{bytes, text, u8_at, u16_at, u32_at};
use crate::clusters::{self, ClusterMap};
use crate::error::FsError;
use crate::time;
use crate::volume::Volume;
use crate::{Entry, EntryId, Extent, FileSystem, FsKind, VolumeInfo, attributes};

/// The largest folder FAT allows: 65,536 entries of 32 bytes.
const MAX_DIR_BYTES: usize = 65_536 * 32;
/// Cluster chains kept in memory.
const CHAIN_CACHE: usize = 256;
/// Folders walked when looking for lost ones.
const MAX_WALK: usize = 200_000;

const NO_FAT_NOTE: &str = "The FAT is gone, so this file's clusters are taken to follow one \
                           another from the first one, which is right unless it was fragmented.";

/// Where a FAT volume's pieces are when its boot sector is gone, as a scan
/// worked it out from the folders it found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FatLayout {
    /// FAT12, FAT16 or FAT32.
    pub kind: FsKind,
    pub cluster_size: u64,
    /// Where cluster 2 starts, in bytes from the volume's start.
    pub data_start: u64,
    /// How many clusters there are.
    pub clusters: u32,
    pub root: FatRoot,
    /// Where the first FAT starts, from the volume's start, if one survived.
    /// Without it, every file is taken to be in one piece.
    pub fat_start: Option<u64>,
    /// Clusters that hold folder entries, found by the scan. Ones no folder
    /// leads to any more are listed under "Lost files".
    pub folders: Vec<u32>,
}

/// Where a FAT volume's root folder is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FatRoot {
    /// FAT32: the root folder's first cluster.
    Cluster { cluster: u32 },
    /// FAT12/16: a fixed area before the clusters (bytes from the volume's start).
    Fixed { offset: u64, bytes: u64 },
}

pub(crate) struct Fat {
    volume: Volume,
    info: VolumeInfo,
    kind: FsKind,
    cluster_size: u64,
    fat_start: u64,
    /// Whether the FAT can be read; without it chains are guessed.
    table: bool,
    root_start: u64,
    root_bytes: u64,
    data_start: u64,
    cluster_count: u32,
    root_cluster: u32,
    chains: Mutex<HashMap<u32, Arc<Vec<u32>>>>,
    /// Folder clusters a scan found (rebuilt volumes only).
    folders: Vec<u32>,
    lost: OnceLock<Lost>,
}

/// Folder clusters no folder reached from the root leads to.
#[derive(Default)]
struct Lost {
    /// The rest of a folder whose start is lost: their entries are listed
    /// directly in "Lost files".
    pieces: Vec<u32>,
    /// Whole folders (starting with "." and "..") whose entry is lost.
    folders: Vec<u32>,
}

impl Fat {
    pub fn open(volume: Volume) -> Result<Fat, FsError> {
        let boot = volume.read_vec(0, 512)?;
        Self::open_with_boot(volume, &boot)
    }

    /// Opens the volume using `boot` as its boot sector (a backup copy).
    pub fn open_with_boot(volume: Volume, boot: &[u8]) -> Result<Fat, FsError> {
        let damaged = || FsError::damaged("The FAT boot sector is damaged.");
        let field16 = |at| u16_at(boot, at).map(u64::from).ok_or_else(damaged);
        let field32 = |at| u32_at(boot, at).map(u64::from).ok_or_else(damaged);
        let bytes_per_sector = field16(11)?;
        let sectors_per_cluster = u64::from(u8_at(boot, 13).ok_or_else(damaged)?);
        let reserved = field16(14)?;
        let fats = u64::from(u8_at(boot, 16).ok_or_else(damaged)?);
        let root_entries = field16(17)?;
        let fat_sectors = match field16(22)? {
            0 => field32(36)?,
            n => n,
        };
        let total = match field16(19)? {
            0 => field32(32)?,
            n => n,
        };
        let plausible = matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096)
            && sectors_per_cluster.is_power_of_two()
            && sectors_per_cluster <= 128
            && reserved >= 1
            && (1..=2).contains(&fats)
            && fat_sectors > 0
            && total > 0;
        if !plausible {
            return Err(damaged());
        }
        let root_bytes = root_entries * 32;
        let data_sector = reserved + fats * fat_sectors + root_bytes.div_ceil(bytes_per_sector);
        let clusters = total.checked_sub(data_sector).ok_or_else(damaged)? / sectors_per_cluster;
        let kind = match clusters {
            0..4085 => FsKind::Fat12,
            4085..65525 => FsKind::Fat16,
            _ => FsKind::Fat32,
        };
        let label_at = if kind == FsKind::Fat32 { 71 } else { 43 };
        let boot_label = bytes(boot, label_at, 11)
            .and_then(text)
            .filter(|l| l != "NO NAME");
        let mut fat = Fat {
            info: VolumeInfo {
                kind,
                name: kind.name().to_owned(),
                label: boot_label,
                offset: volume.start(),
                size: total * bytes_per_sector,
                cluster_size: bytes_per_sector * sectors_per_cluster,
            },
            kind,
            cluster_size: bytes_per_sector * sectors_per_cluster,
            fat_start: reserved * bytes_per_sector,
            table: true,
            root_start: (reserved + fats * fat_sectors) * bytes_per_sector,
            root_bytes,
            data_start: data_sector * bytes_per_sector,
            cluster_count: u32::try_from(clusters).map_err(|_| damaged())?,
            root_cluster: if kind == FsKind::Fat32 {
                u32_at(boot, 44).ok_or_else(damaged)?
            } else {
                0
            },
            chains: Mutex::new(HashMap::new()),
            folders: Vec::new(),
            lost: OnceLock::new(),
            volume,
        };
        fat.read_label();
        Ok(fat)
    }

    /// Opens a volume whose boot sector is gone, from what a scan worked out.
    pub fn rebuilt(volume: Volume, layout: &FatLayout) -> Result<Fat, FsError> {
        let plausible = matches!(layout.kind, FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32)
            && layout.cluster_size.is_power_of_two()
            && (512..=256 * 1024).contains(&layout.cluster_size)
            && layout.data_start < volume.size();
        if !plausible {
            return Err(FsError::damaged("The FAT volume's layout makes no sense."));
        }
        let (root_start, root_bytes, root_cluster) = match layout.root {
            FatRoot::Cluster { cluster } => (0, 0, cluster),
            FatRoot::Fixed { offset, bytes } => (offset, bytes, 0),
        };
        let mut folders = layout.folders.clone();
        folders.sort_unstable();
        folders.dedup();
        let mut fat = Fat {
            info: VolumeInfo {
                kind: layout.kind,
                name: layout.kind.name().to_owned(),
                label: None,
                offset: volume.start(),
                size: volume.size(),
                cluster_size: layout.cluster_size,
            },
            kind: layout.kind,
            cluster_size: layout.cluster_size,
            fat_start: layout.fat_start.unwrap_or(0),
            table: layout.fat_start.is_some(),
            root_start,
            root_bytes,
            data_start: layout.data_start,
            cluster_count: layout.clusters,
            root_cluster,
            chains: Mutex::new(HashMap::new()),
            folders,
            lost: OnceLock::new(),
            volume,
        };
        fat.read_label();
        Ok(fat)
    }

    /// The label in the root folder is the one Windows shows.
    fn read_label(&mut self) {
        if let Ok(root) = self.dir_bytes(&self.root())
            && let Some(label) = volume_label(&root)
        {
            self.info.label = Some(label);
        }
    }

    /// The FAT entry for a cluster: the next cluster, end-of-chain, bad or free (0).
    fn entry(&self, cluster: u32) -> Result<u32, FsError> {
        if !self.table {
            return Ok(0);
        }
        let c = u64::from(cluster);
        match self.kind {
            FsKind::Fat12 => {
                let mut raw = [0u8; 2];
                self.volume.read(self.fat_start + c + c / 2, &mut raw)?;
                let value = u16::from_le_bytes(raw);
                Ok(u32::from(if cluster & 1 == 1 {
                    value >> 4
                } else {
                    value & 0x0FFF
                }))
            }
            FsKind::Fat16 => {
                let mut raw = [0u8; 2];
                self.volume.read(self.fat_start + 2 * c, &mut raw)?;
                Ok(u32::from(u16::from_le_bytes(raw)))
            }
            _ => {
                let mut raw = [0u8; 4];
                self.volume.read(self.fat_start + 4 * c, &mut raw)?;
                Ok(u32::from_le_bytes(raw) & 0x0FFF_FFFF)
            }
        }
    }

    fn in_use(&self, cluster: u32) -> bool {
        self.table && self.entry(cluster).is_ok_and(|value| value != 0)
    }

    /// A folder's raw entries.
    fn dir_bytes(&self, dir: &EntryId) -> Result<Vec<u8>, FsError> {
        Ok(self.dir_clusters(dir)?.0)
    }

    /// A folder's raw entries and the clusters they came from.
    fn dir_clusters(&self, dir: &EntryId) -> Result<(Vec<u8>, Vec<u32>), FsError> {
        match dir {
            EntryId::FixedRoot if self.kind != FsKind::Fat32 => Ok((
                self.volume.read_vec(self.root_start, self.root_bytes)?,
                Vec::new(),
            )),
            EntryId::Clusters {
                cluster,
                chain: true,
                ..
            } => {
                let mut data = Vec::new();
                let chain = self.chain(*cluster)?;
                for &c in chain.iter() {
                    if data.len() >= MAX_DIR_BYTES {
                        break;
                    }
                    let offset = self
                        .cluster_offset(c)
                        .ok_or_else(|| FsError::damaged("A folder's cluster chain is damaged."))?;
                    data.extend(self.volume.read_vec(offset, self.cluster_size)?);
                }
                Ok((data, chain.to_vec()))
            }
            EntryId::Clusters {
                cluster,
                chain: false,
                ..
            } => {
                // No chain to follow (a deleted folder, or no FAT): the
                // clusters are taken to follow one another up to the
                // end-of-folder marker, as long as they look like a folder.
                let mut data = Vec::new();
                let mut used = Vec::new();
                let mut c = *cluster;
                while let Some(offset) = self.cluster_offset(c)
                    && data.len() < MAX_DIR_BYTES
                {
                    let chunk = self.volume.read_vec(offset, self.cluster_size)?;
                    if !used.is_empty() && !plausible_entries(&chunk) {
                        break;
                    }
                    let ended = chunk.as_chunks::<32>().0.iter().any(|slot| slot[0] == 0);
                    data.extend(chunk);
                    used.push(c);
                    if ended {
                        break;
                    }
                    c += 1;
                }
                Ok((data, used))
            }
            _ => Err(FsError::NotFound("That folder".to_owned())),
        }
    }

    fn parse_dir(&self, data: &[u8]) -> Vec<Entry> {
        let mut entries = Vec::new();
        let mut long_parts: Vec<&[u8; 32]> = Vec::new();
        for slot in data.as_chunks::<32>().0 {
            let first = slot[0];
            if first == 0 {
                break; // nothing was ever stored past here
            }
            let attributes = slot[11];
            if attributes & 0x3F == 0x0F {
                long_parts.push(slot);
                continue;
            }
            let parts = std::mem::take(&mut long_parts);
            let deleted = first == 0xE5;
            if attributes & 0x08 != 0 || (!deleted && first == b'.') {
                continue; // volume label, or "." and ".."
            }
            entries.push(self.entry_from(slot, &parts, deleted));
        }
        entries
    }

    fn entry_from(&self, slot: &[u8; 32], long_parts: &[&[u8; 32]], deleted: bool) -> Entry {
        let field16 = |at| u16_at(slot, at).unwrap_or(0);
        let short: [u8; 11] = *slot.first_chunk::<11>().expect("a slot is 32 bytes");
        let attributes = slot[11];
        let name = long_name(long_parts, &short, deleted)
            .unwrap_or_else(|| short_name(&short, slot[12], deleted));
        let high = if self.kind == FsKind::Fat32 {
            field16(20)
        } else {
            0
        };
        let cluster = (u32::from(high) << 16) | u32::from(field16(26));
        let is_dir = attributes & 0x10 != 0;
        let size = if is_dir {
            0
        } else {
            u64::from(u32_at(slot, 28).unwrap_or(0))
        };
        let note = if deleted {
            let (used, total) = clusters::reused(self, cluster, size, |c| self.in_use(c));
            Some(clusters::deleted_note(!is_dir || size > 0, used, total))
        } else {
            (!self.table && size > 0).then(|| NO_FAT_NOTE.to_owned())
        };
        Entry {
            id: EntryId::Clusters {
                cluster,
                length: size,
                chain: !deleted && self.table,
            },
            name,
            is_dir,
            size,
            deleted,
            created: time::fat(field16(16), field16(14), slot[13]),
            modified: time::fat(field16(24), field16(22), 0),
            accessed: time::fat(field16(18), 0, 0),
            attributes: u32::from(attributes),
            streams: Vec::new(),
            note,
        }
    }

    /// Folder clusters the scan found that no folder from the root leads to.
    fn lost(&self) -> &Lost {
        self.lost.get_or_init(|| {
            if self.folders.is_empty() {
                return Lost::default();
            }
            let mut reached = HashSet::new();
            self.walk(self.root(), &mut reached);
            let (mut starts, mut pieces) = (Vec::new(), Vec::new());
            for &c in &self.folders {
                if reached.contains(&c) {
                    continue;
                }
                match self.folder_start(c) {
                    Some(true) => starts.push(c),
                    Some(false) => pieces.push(c),
                    None => {}
                }
            }
            // Pieces first: what they list isn't lost.
            let mut lost = Lost::default();
            for c in pieces {
                if !reached.contains(&c) {
                    self.walk(self.dir_id(c), &mut reached);
                    lost.pieces.push(c);
                }
            }
            for c in starts {
                if !reached.contains(&c) {
                    self.walk(self.dir_id(c), &mut reached);
                    lost.folders.push(c);
                }
            }
            lost
        })
    }

    fn dir_id(&self, cluster: u32) -> EntryId {
        EntryId::Clusters {
            cluster,
            length: 0,
            chain: false,
        }
    }

    /// Whether a found folder cluster starts a folder ("." first), or
    /// continues one; `None` if it can't be read.
    fn folder_start(&self, cluster: u32) -> Option<bool> {
        let data = self
            .volume
            .read_vec(self.cluster_offset(cluster)?, 32)
            .ok()?;
        Some(data.starts_with(b".          "))
    }

    /// Marks the clusters of `dir` and every folder below it as reached.
    fn walk(&self, dir: EntryId, reached: &mut HashSet<u32>) {
        let mut queue = VecDeque::from([dir]);
        let mut walked = 0;
        while let Some(dir) = queue.pop_front() {
            walked += 1;
            if walked > MAX_WALK {
                break;
            }
            let Ok((data, used)) = self.dir_clusters(&dir) else {
                continue;
            };
            reached.extend(used);
            for entry in self.parse_dir(&data) {
                if let EntryId::Clusters { cluster, .. } = entry.id
                    && entry.is_dir
                    && !reached.contains(&cluster)
                {
                    reached.insert(cluster);
                    queue.push_back(EntryId::Clusters {
                        cluster,
                        length: 0,
                        chain: false,
                    });
                }
            }
        }
    }

    fn lost_entries(&self) -> Result<Vec<Entry>, FsError> {
        let lost = self.lost();
        let mut entries = Vec::new();
        for &c in &lost.pieces {
            let data = self.dir_bytes(&self.dir_id(c))?;
            entries.extend(self.parse_dir(&data));
        }
        for &c in &lost.folders {
            let mut folder = lost_folder(self.dir_id(c), format!("Folder at cluster {c}"));
            folder.note = Some(
                "This folder's entry in its parent folder is lost, so its name is unknown."
                    .to_owned(),
            );
            entries.push(folder);
        }
        Ok(entries)
    }
}

impl ClusterMap for Fat {
    fn volume(&self) -> &Volume {
        &self.volume
    }

    fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    fn cluster_offset(&self, cluster: u32) -> Option<u64> {
        (cluster >= 2 && cluster - 2 < self.cluster_count)
            .then(|| self.data_start + u64::from(cluster - 2) * self.cluster_size)
    }

    fn chain(&self, start: u32) -> Result<Arc<Vec<u32>>, FsError> {
        let mut cache = self.chains.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(chain) = cache.get(&start) {
            return Ok(Arc::clone(chain));
        }
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cluster = start;
        // Stops at end-of-chain, free or bad entries, numbers that aren't
        // clusters, and loops.
        while self.cluster_offset(cluster).is_some() && seen.insert(cluster) {
            chain.push(cluster);
            cluster = self.entry(cluster)?;
        }
        if cache.len() >= CHAIN_CACHE {
            cache.clear();
        }
        let chain = Arc::new(chain);
        cache.insert(start, Arc::clone(&chain));
        Ok(chain)
    }
}

impl FileSystem for Fat {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn allocation(&self) -> Option<crate::Allocation> {
        if !self.table {
            return None;
        }
        let count = u64::from(self.cluster_count);
        let mut ranges = crate::Ranges::default();
        // The boot sector, the FATs and FAT12/16's root folder come first.
        ranges.add(0, self.data_start);
        // Free (0) and bad clusters aren't copied; the FAT says which.
        let (bits, bad) = match self.kind {
            FsKind::Fat12 => (12, 0xFF7),
            FsKind::Fat16 => (16, 0xFFF7),
            _ => (32, 0x0FFF_FFF7),
        };
        let mut add = |cluster: u64, value: u32| {
            if value != 0 && value != bad {
                let at = self.data_start + (cluster - 2) * self.cluster_size;
                ranges.add(at, at + self.cluster_size);
            }
        };
        if bits == 32 {
            // FAT32's table can be hundreds of MiB: a piece at a time.
            const PIECE: u64 = 1 << 20;
            let mut cluster = 2;
            while cluster < count + 2 {
                let n = PIECE.min(count + 2 - cluster);
                let raw = self
                    .volume
                    .read_vec(self.fat_start + 4 * cluster, 4 * n)
                    .ok()?;
                for (i, entry) in raw.as_chunks::<4>().0.iter().enumerate() {
                    add(cluster + i as u64, u32::from_le_bytes(*entry) & 0x0FFF_FFFF);
                }
                cluster += n;
            }
        } else {
            let length = ((count + 2) * bits).div_ceil(8);
            let raw = self.volume.read_vec(self.fat_start, length).ok()?;
            for cluster in 2..count + 2 {
                let value = if bits == 16 {
                    let at = usize::try_from(2 * cluster).ok()?;
                    u32::from(u16::from_le_bytes([raw[at], raw[at + 1]]))
                } else {
                    let at = usize::try_from(cluster + cluster / 2).ok()?;
                    let pair = u16::from_le_bytes([raw[at], *raw.get(at + 1).unwrap_or(&0)]);
                    u32::from(if cluster & 1 == 1 {
                        pair >> 4
                    } else {
                        pair & 0x0FFF
                    })
                };
                add(cluster, value);
            }
        }
        Some(ranges.finish(self.data_start + count * self.cluster_size))
    }

    fn root(&self) -> EntryId {
        if self.kind == FsKind::Fat32 {
            EntryId::Clusters {
                cluster: self.root_cluster,
                length: 0,
                chain: self.table,
            }
        } else {
            EntryId::FixedRoot
        }
    }

    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError> {
        if *dir == EntryId::Orphans {
            return self.lost_entries();
        }
        let mut entries = self.parse_dir(&self.dir_bytes(dir)?);
        if *dir == self.root() {
            let lost = self.lost();
            if !lost.pieces.is_empty() || !lost.folders.is_empty() {
                let mut folder = lost_folder(EntryId::Orphans, crate::LOST_FILES.to_owned());
                folder.note = Some(
                    "Files and folders found on the disk that no folder leads to any more."
                        .to_owned(),
                );
                entries.push(folder);
            }
        }
        Ok(entries)
    }

    fn read(
        &self,
        file: &EntryId,
        stream: Option<&str>,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        if stream.is_some() {
            return Err(FsError::NotFound(
                "FAT files have no named streams; that stream".to_owned(),
            ));
        }
        clusters::read(self, file, offset, buf)
    }

    fn extents(&self, file: &EntryId, _stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        clusters::extents(self, file)
    }
}

fn lost_folder(id: EntryId, name: String) -> Entry {
    Entry {
        id,
        name,
        is_dir: true,
        size: 0,
        deleted: false,
        created: None,
        modified: None,
        accessed: None,
        attributes: attributes::DIRECTORY,
        streams: Vec::new(),
        note: None,
    }
}

/// The label stored as a root-folder entry.
fn volume_label(root: &[u8]) -> Option<String> {
    root.as_chunks::<32>()
        .0
        .iter()
        .take_while(|slot| slot[0] != 0)
        .find(|slot| slot[11] & 0x3F != 0x0F && slot[11] & 0x08 != 0 && slot[0] != 0xE5)
        .and_then(|slot| text(slot.first_chunk::<11>()?))
}

/// What a sector that holds FAT folder entries tells a scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderSector {
    /// For the first sector of a folder: its own first cluster (from the "."
    /// entry) and its parent's (from ".."; 0 for the root).
    pub dot: Option<(u32, u32)>,
    /// The first clusters of the folders listed in it (not "." or "..").
    pub subfolders: Vec<u32>,
}

/// Whether `sector` (512 bytes or more) holds FAT folder entries, checked
/// strictly enough that file contents rarely pass.
pub fn folder_sector(sector: &[u8]) -> Option<FolderSector> {
    let sector = sector.get(..512)?;
    let slots = sector.as_chunks::<32>().0;
    let mut short_entries = 0;
    let mut subfolders = Vec::new();
    let cluster = |slot: &[u8; 32]| {
        (u32::from(u16_at(slot, 20).unwrap_or(0)) << 16) | u32::from(u16_at(slot, 26).unwrap_or(0))
    };
    for (i, slot) in slots.iter().enumerate() {
        if slot[0] == 0 {
            // The end of the folder: nothing is stored after it.
            if sector[i * 32..].iter().any(|&b| b != 0) {
                return None;
            }
            break;
        }
        match slot_kind(slot, i)? {
            Slot::Short => {
                short_entries += 1;
                if slot[11] & 0x10 != 0 && slot[0] != b'.' && cluster(slot) >= 2 {
                    subfolders.push(cluster(slot));
                }
            }
            Slot::Long => {}
        }
    }
    if short_entries == 0 {
        return None;
    }
    let dot = (slots[0].starts_with(b".          ")
        && slots[0][11] & 0x10 != 0
        && slots[1].starts_with(b"..         ")
        && slots[1][11] & 0x10 != 0)
        .then(|| (cluster(&slots[0]), cluster(&slots[1])));
    Some(FolderSector { dot, subfolders })
}

/// Whether a cluster's worth of bytes could all be folder entries (used to
/// tell where a folder without a FAT chain ends).
fn plausible_entries(data: &[u8]) -> bool {
    let slots = data.as_chunks::<32>().0;
    let mut any = false;
    for (i, slot) in slots.iter().enumerate() {
        if slot[0] == 0 {
            return any || i > 0 || data.iter().all(|&b| b == 0);
        }
        if slot_kind(slot, i + 2).is_none() {
            return false;
        }
        any = true;
    }
    any
}

enum Slot {
    Short,
    Long,
}

/// What a 32-byte folder slot is, or `None` if it can't be one. `index`
/// is its place in the folder ("." and ".." may only be the first two).
fn slot_kind(slot: &[u8; 32], index: usize) -> Option<Slot> {
    let attributes = slot[11];
    if attributes & 0xC0 != 0 {
        return None;
    }
    if attributes == 0x0F {
        // A long-name part: sequence 1-20 (+0x40 on the last), or deleted.
        let sequence = slot[0];
        let ok_sequence =
            sequence == 0xE5 || (1..=20).contains(&(sequence & 0x1F)) && sequence & 0xA0 == 0;
        return (ok_sequence && slot[12] == 0 && slot[26] == 0 && slot[27] == 0)
            .then_some(Slot::Long);
    }
    let name = &slot[..11];
    let dots = match index {
        0 => name == b".          ",
        1 => name == b"..         ",
        _ => false,
    };
    if !dots {
        let first = name[0];
        if first == b' ' || (first != 0xE5 && first != 0x05 && !short_name_byte(first)) {
            return None;
        }
        if !name[1..].iter().all(|&b| short_name_byte(b)) {
            return None;
        }
    }
    // Only the "lower-case" flags may be set in the case byte.
    if slot[12] & !0x18 != 0 || slot[13] > 199 {
        return None;
    }
    let date_ok = |at: usize| {
        let date = u16_at(slot, at).unwrap_or(0);
        let (month, day) = ((date >> 5) & 0x0F, date & 0x1F);
        date == 0 || ((1..=12).contains(&month) && day >= 1)
    };
    if !(date_ok(16) && date_ok(18) && date_ok(24)) {
        return None;
    }
    if attributes & 0x10 != 0 && u32_at(slot, 28) != Some(0) {
        return None; // folders have no size
    }
    Some(Slot::Short)
}

/// A byte allowed in a stored 8.3 name (upper case, no punctuation FAT bans).
fn short_name_byte(b: u8) -> bool {
    b >= 0x20 && !b.is_ascii_lowercase() && !b"\"*+,./:;<=>?[\\]|\x7F".contains(&b)
}

/// The start of a FAT (its first two entries: the media byte, then all
/// ones), and which FAT it looks like.
pub fn table_start(sector: &[u8]) -> Option<FsKind> {
    let media = *sector.first()?;
    if !(media == 0xF0 || media >= 0xF8) {
        return None;
    }
    match sector.get(1..8)? {
        [0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, last] if last & 0x0F == 0x0F || *last == 0x07 => {
            Some(FsKind::Fat32)
        }
        [0xFF, 0xFF, 0xFF, ..] => Some(FsKind::Fat16),
        [0xFF, 0xFF, ..] => Some(FsKind::Fat12),
        _ => None,
    }
}

/// The checksum of a short name that its long-name entries carry.
pub(crate) fn lfn_checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |sum, &b| {
        ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(b)
    })
}

/// Joins long-name entries (stored last part first) into the name, if they
/// belong to this short name.
fn long_name(parts: &[&[u8; 32]], short: &[u8; 11], deleted: bool) -> Option<String> {
    let checksum = parts.first()?[13];
    if parts.iter().any(|part| part[13] != checksum) {
        return None;
    }
    let mut units = Vec::new();
    for part in parts.iter().rev() {
        for (start, len) in [(1, 10), (14, 12), (28, 4)] {
            let chars = bytes(*part, start, len)?;
            units.extend(
                chars
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair)),
            );
        }
    }
    let end = units
        .iter()
        .position(|&u| u == 0 || u == 0xFFFF)
        .unwrap_or(units.len());
    let name = String::from_utf16_lossy(units.get(..end)?);
    if name.is_empty() {
        return None;
    }

    let belongs = if deleted {
        // Deleting overwrote the short name's first byte (and the parts'
        // sequence numbers). The checksum pins down which byte it was: it
        // must be what Windows or Linux would have put there.
        let original = (0..=255u8).find(|&b| {
            let mut candidate = *short;
            candidate[0] = b;
            lfn_checksum(&candidate) == checksum
        });
        let first = name.chars().next().map(|c| c.to_ascii_uppercase());
        original.is_some_and(|b| Some(char::from(b)) == first || b == b'_')
    } else {
        let sequence_ok = parts[0][0] & 0x40 != 0 && usize::from(parts[0][0] & 0x1F) == parts.len();
        sequence_ok && lfn_checksum(short) == checksum
    };
    belongs.then_some(name)
}

/// "README  TXT" → "README.TXT", lower-cased as the case flags say. A deleted
/// entry's lost first letter shows as `?`.
fn short_name(short: &[u8; 11], case_flags: u8, deleted: bool) -> String {
    let mut base: Vec<u8> = short[..8].to_vec();
    if deleted {
        base[0] = b'?';
    } else if base[0] == 0x05 {
        base[0] = 0xE5; // a name really starting with E5 (Japanese)
    }
    let latin1 = |b: &[u8]| -> String {
        b.iter()
            .map(|&c| char::from(c))
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let mut base = latin1(&base);
    let mut extension = latin1(&short[8..]);
    if case_flags & 0x08 != 0 {
        base = base.to_lowercase();
    }
    if case_flags & 0x10 != 0 {
        extension = extension.to_lowercase();
    }
    if extension.is_empty() {
        base
    } else {
        format!("{base}.{extension}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(short_name(b"README  TXT", 0, false), "README.TXT");
        assert_eq!(short_name(b"README  TXT", 0x18, false), "readme.txt");
        assert_eq!(short_name(b"DOCS       ", 0, false), "DOCS");
        assert_eq!(short_name(b"\xE5EADME  TXT", 0, true), "?EADME.TXT");
    }

    #[test]
    fn checksum_matches_the_spec_example() {
        // Computed with the algorithm from Microsoft's FAT specification.
        let sum = lfn_checksum(b"README  TXT");
        let manual = b"README  TXT"
            .iter()
            .fold(0u8, |s, &b| (s >> 1 | (s & 1) << 7).wrapping_add(b));
        assert_eq!(sum, manual);
    }

    #[test]
    fn folder_sectors_and_text_are_told_apart() {
        let mut sector = [0u8; 512];
        sector[..11].copy_from_slice(b".          ");
        sector[11] = 0x10;
        sector[26] = 9;
        sector[32..43].copy_from_slice(b"..         ");
        sector[43] = 0x10;
        sector[64..75].copy_from_slice(b"NOTES   TXT");
        sector[75] = 0x20;
        sector[88..90].copy_from_slice(&((44u16 << 9) | (1 << 5) | 15).to_le_bytes());
        assert_eq!(
            folder_sector(&sector),
            Some(FolderSector {
                dot: Some((9, 0)),
                subfolders: Vec::new()
            })
        );

        // Something after the end marker, lower case, or text: not a folder.
        let mut after_end = sector;
        after_end[200] = 1;
        assert_eq!(folder_sector(&after_end), None);
        let mut lower = sector;
        lower[64] = b'n';
        assert_eq!(folder_sector(&lower), None);
        let text: Vec<u8> = b"Some text that goes on and on, line after line.\n"
            .iter()
            .cycle()
            .take(512)
            .copied()
            .collect();
        assert_eq!(folder_sector(&text), None);
        assert_eq!(folder_sector(&[0u8; 512]), None);
    }

    #[test]
    fn fat_starts() {
        assert_eq!(
            table_start(&[0xF8, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F]),
            Some(FsKind::Fat32)
        );
        assert_eq!(
            table_start(&[0xF8, 0xFF, 0xFF, 0xFF, 0, 0, 0, 0]),
            Some(FsKind::Fat16)
        );
        assert_eq!(
            table_start(&[0xF0, 0xFF, 0xFF, 0x03, 0x40, 0, 0, 0]),
            Some(FsKind::Fat12)
        );
        assert_eq!(table_start(b"FILE0\0\x03\0"), None);
    }
}
