//! exFAT. Layout: Microsoft "exFAT file system specification".

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use crate::bytes::{u8_at, u16_at, u32_at, u64_at, utf16_text};
use crate::clusters::{self, ClusterMap};
use crate::error::FsError;
use crate::time;
use crate::volume::Volume;
use crate::{Entry, EntryId, Extent, FileSystem, FsKind, VolumeInfo};

/// exFAT folders are at most 256 MiB.
const MAX_DIR_BYTES: u64 = 256 << 20;
const CHAIN_CACHE: usize = 256;

const TYPE_BITMAP: u8 = 0x81;
const TYPE_LABEL: u8 = 0x83;
const TYPE_FILE: u8 = 0x85;
const TYPE_STREAM: u8 = 0xC0;
const TYPE_NAME: u8 = 0xC1;
/// Entry types with this bit cleared are deleted.
const IN_USE: u8 = 0x80;

pub(crate) struct Exfat {
    volume: Volume,
    info: VolumeInfo,
    cluster_size: u64,
    fat_start: u64,
    heap_start: u64,
    cluster_count: u32,
    root_cluster: u32,
    /// The allocation bitmap: one bit per cluster, from cluster 2.
    bitmap: Option<Vec<u8>>,
    chains: Mutex<HashMap<u32, Arc<Vec<u32>>>>,
}

impl Exfat {
    pub fn open(volume: Volume) -> Result<Exfat, FsError> {
        let boot = volume.read_vec(0, 512)?;
        let damaged = || FsError::damaged("The exFAT boot sector is damaged.");
        let sector_shift = u8_at(&boot, 108).ok_or_else(damaged)?;
        let cluster_shift = u8_at(&boot, 109).ok_or_else(damaged)?;
        if !(9..=12).contains(&sector_shift)
            || u32::from(sector_shift) + u32::from(cluster_shift) > 25
        {
            return Err(damaged());
        }
        let sector = 1u64 << sector_shift;
        let field = |at| u32_at(&boot, at).map(u64::from).ok_or_else(damaged);
        let fat_offset = field(80)?;
        let fat_length = field(84)?;
        let flags = u16_at(&boot, 106).ok_or_else(damaged)?;
        let fats = u8_at(&boot, 110).ok_or_else(damaged)?;
        // With two FATs, a flag says which one is in use.
        let active = u64::from(flags & 1 == 1 && fats == 2);
        let mut exfat = Exfat {
            info: VolumeInfo {
                kind: FsKind::Exfat,
                name: FsKind::Exfat.name().to_owned(),
                label: None,
                offset: volume.start(),
                size: u64_at(&boot, 72)
                    .ok_or_else(damaged)?
                    .saturating_mul(sector),
                cluster_size: sector << cluster_shift,
            },
            cluster_size: sector << cluster_shift,
            fat_start: (fat_offset + active * fat_length) * sector,
            heap_start: field(88)? * sector,
            cluster_count: u32_at(&boot, 92).ok_or_else(damaged)?,
            root_cluster: u32_at(&boot, 96).ok_or_else(damaged)?,
            bitmap: None,
            chains: Mutex::new(HashMap::new()),
            volume,
        };
        let root = exfat.dir_bytes(&exfat.root())?;
        for slot in root
            .as_chunks::<32>()
            .0
            .iter()
            .take_while(|slot| slot[0] != 0)
        {
            match slot[0] {
                TYPE_LABEL => {
                    let chars = usize::from(slot[1].min(11));
                    exfat.info.label = slot.get(2..2 + 2 * chars).and_then(utf16_text);
                }
                // The bitmap of the active FAT (bit 0 of its flags says which).
                TYPE_BITMAP if u64::from(slot[1] & 1) == active => {
                    let first = u32_at(slot, 20).unwrap_or(0);
                    let length = u64_at(slot, 24).unwrap_or(0).min(MAX_DIR_BYTES);
                    let id = EntryId::Clusters {
                        cluster: first,
                        length,
                        chain: true,
                    };
                    exfat.bitmap = crate::read_to_end(&exfat, &id, None, length).ok();
                }
                _ => {}
            }
        }
        Ok(exfat)
    }

    fn entry(&self, cluster: u32) -> Result<u32, FsError> {
        let mut raw = [0u8; 4];
        self.volume
            .read(self.fat_start + 4 * u64::from(cluster), &mut raw)?;
        Ok(u32::from_le_bytes(raw))
    }

    fn allocated(&self, cluster: u32) -> bool {
        let Some(bitmap) = &self.bitmap else {
            return false;
        };
        let Some(index) = cluster.checked_sub(2) else {
            return false;
        };
        bitmap
            .get(usize::try_from(index / 8).unwrap_or(usize::MAX))
            .is_some_and(|byte| byte & (1 << (index % 8)) != 0)
    }

    fn dir_bytes(&self, dir: &EntryId) -> Result<Vec<u8>, FsError> {
        let EntryId::Clusters {
            cluster,
            length,
            chain,
        } = dir
        else {
            return Err(FsError::NotFound("That folder".to_owned()));
        };
        let clusters: Vec<u32> = if *chain {
            self.chain(*cluster)?.to_vec()
        } else {
            let count = length
                .div_ceil(self.cluster_size)
                .clamp(1, MAX_DIR_BYTES / self.cluster_size);
            (0..count)
                .map_while(|i| u32::try_from(u64::from(*cluster) + i).ok())
                .collect()
        };
        let mut data = Vec::new();
        for c in clusters {
            if data.len() as u64 >= MAX_DIR_BYTES {
                break;
            }
            let offset = self
                .cluster_offset(c)
                .ok_or_else(|| FsError::damaged("A folder's clusters are damaged."))?;
            data.extend(self.volume.read_vec(offset, self.cluster_size)?);
        }
        Ok(data)
    }

    fn parse_dir(&self, data: &[u8]) -> Vec<Entry> {
        let slots = data.as_chunks::<32>().0;
        let mut entries = Vec::new();
        let mut i = 0;
        while let Some(slot) = slots.get(i) {
            if slot[0] == 0 {
                break; // end of the folder
            }
            if slot[0] & !IN_USE == TYPE_FILE & !IN_USE
                && let Some((entry, used)) = self.entry_set(&slots[i..])
            {
                entries.push(entry);
                i += used;
            } else {
                i += 1;
            }
        }
        entries
    }

    /// A file's entry set: the file entry, a stream entry, then name
    /// entries. Returns the entry and how many slots the set used.
    fn entry_set(&self, slots: &[[u8; 32]]) -> Option<(Entry, usize)> {
        let file = slots.first()?;
        let deleted = file[0] & IN_USE == 0;
        let secondary = usize::from(file[1]);
        let stream = slots.get(1)?;
        let same_state =
            |slot: &[u8; 32], kind: u8| slot[0] == if deleted { kind & !IN_USE } else { kind };
        if secondary < 2 || !same_state(stream, TYPE_STREAM) {
            return None;
        }
        let name_length = usize::from(stream[3]);
        let mut units: Vec<u16> = Vec::new();
        for slot in slots
            .iter()
            .skip(2)
            .take(secondary - 1)
            .take_while(|s| same_state(s, TYPE_NAME))
        {
            units.extend(
                slot[2..32]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| u16::from_le_bytes(*pair)),
            );
        }
        units.truncate(name_length);
        let name = String::from_utf16_lossy(&units);
        if name.is_empty() {
            return None;
        }

        let attributes = u32::from(u16_at(file, 4).unwrap_or(0));
        let is_dir = attributes & crate::attributes::DIRECTORY != 0;
        let no_fat_chain = stream[1] & 2 != 0;
        let first = u32_at(stream, 20).unwrap_or(0);
        let length = u64_at(stream, 24).unwrap_or(0);
        let stamp = |at: usize, ten_ms: usize, offset: usize| {
            time::exfat(
                u32_at(file, at).unwrap_or(0),
                u8_at(file, ten_ms).unwrap_or(0),
                u8_at(file, offset).unwrap_or(0),
            )
        };

        // A deleted file's FAT chain often survives (exFAT frees clusters in
        // the bitmap). Trust it if it's the right length and still free.
        let chain = if no_fat_chain {
            false
        } else if !deleted {
            true
        } else {
            self.chain(first).is_ok_and(|chain| {
                chain.len() as u64 == length.div_ceil(self.cluster_size)
                    && chain.iter().all(|&c| !self.allocated(c))
            })
        };
        let note = deleted.then(|| {
            let (used, total) = if chain {
                let clusters = self.chain(first).map(|c| c.to_vec()).unwrap_or_default();
                (
                    clusters.iter().filter(|&&c| self.allocated(c)).count() as u64,
                    clusters.len() as u64,
                )
            } else {
                clusters::reused(self, first, length, |c| self.allocated(c))
            };
            clusters::deleted_note(!no_fat_chain && !chain, used, total)
        });
        let entry = Entry {
            id: EntryId::Clusters {
                cluster: first,
                length,
                chain,
            },
            name,
            is_dir,
            size: if is_dir { 0 } else { length },
            deleted,
            created: stamp(8, 20, 22),
            modified: stamp(12, 21, 23),
            accessed: time::exfat(
                u32_at(file, 16).unwrap_or(0),
                0,
                u8_at(file, 24).unwrap_or(0),
            ),
            attributes,
            streams: Vec::new(),
            note,
        };
        Some((entry, 1 + secondary))
    }
}

impl ClusterMap for Exfat {
    fn volume(&self) -> &Volume {
        &self.volume
    }

    fn cluster_size(&self) -> u64 {
        self.cluster_size
    }

    fn cluster_offset(&self, cluster: u32) -> Option<u64> {
        (cluster >= 2 && cluster - 2 < self.cluster_count)
            .then(|| self.heap_start + u64::from(cluster - 2) * self.cluster_size)
    }

    fn chain(&self, start: u32) -> Result<Arc<Vec<u32>>, FsError> {
        let mut cache = self.chains.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(chain) = cache.get(&start) {
            return Ok(Arc::clone(chain));
        }
        let mut chain = Vec::new();
        let mut seen = HashSet::new();
        let mut cluster = start;
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

impl FileSystem for Exfat {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn root(&self) -> EntryId {
        EntryId::Clusters {
            cluster: self.root_cluster,
            length: 0,
            chain: true,
        }
    }

    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError> {
        Ok(self.parse_dir(&self.dir_bytes(dir)?))
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
                "exFAT files have no named streams; that stream".to_owned(),
            ));
        }
        clusters::read(self, file, offset, buf)
    }

    fn extents(&self, file: &EntryId, _stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        clusters::extents(self, file)
    }
}
