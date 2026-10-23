//! FAT12, FAT16 and FAT32. Layout: Microsoft "FAT: General Overview of On-Disk Format".

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use crate::bytes::{bytes, text, u8_at, u16_at, u32_at};
use crate::clusters::{self, ClusterMap};
use crate::error::FsError;
use crate::time;
use crate::volume::Volume;
use crate::{Entry, EntryId, Extent, FileSystem, FsKind, VolumeInfo};

/// The largest folder FAT allows: 65,536 entries of 32 bytes.
const MAX_DIR_BYTES: usize = 65_536 * 32;
/// Cluster chains kept in memory.
const CHAIN_CACHE: usize = 256;

pub(crate) struct Fat {
    volume: Volume,
    info: VolumeInfo,
    kind: FsKind,
    cluster_size: u64,
    fat_start: u64,
    root_start: u64,
    root_bytes: u64,
    data_start: u64,
    cluster_count: u32,
    root_cluster: u32,
    chains: Mutex<HashMap<u32, Arc<Vec<u32>>>>,
}

impl Fat {
    pub fn open(volume: Volume) -> Result<Fat, FsError> {
        let boot = volume.read_vec(0, 512)?;
        let damaged = || FsError::damaged("The FAT boot sector is damaged.");
        let field16 = |at| u16_at(&boot, at).map(u64::from).ok_or_else(damaged);
        let field32 = |at| u32_at(&boot, at).map(u64::from).ok_or_else(damaged);
        let bytes_per_sector = field16(11)?;
        let sectors_per_cluster = u64::from(u8_at(&boot, 13).ok_or_else(damaged)?);
        let reserved = field16(14)?;
        let fats = u64::from(u8_at(&boot, 16).ok_or_else(damaged)?);
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
        let boot_label = bytes(&boot, label_at, 11)
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
            root_start: (reserved + fats * fat_sectors) * bytes_per_sector,
            root_bytes,
            data_start: data_sector * bytes_per_sector,
            cluster_count: u32::try_from(clusters).map_err(|_| damaged())?,
            root_cluster: if kind == FsKind::Fat32 {
                u32_at(&boot, 44).ok_or_else(damaged)?
            } else {
                0
            },
            chains: Mutex::new(HashMap::new()),
            volume,
        };
        // The label in the root folder is the one Windows shows.
        if let Ok(root) = fat.dir_bytes(&fat.root())
            && let Some(label) = volume_label(&root)
        {
            fat.info.label = Some(label);
        }
        Ok(fat)
    }

    /// The FAT entry for a cluster: the next cluster, end-of-chain, bad or free (0).
    fn entry(&self, cluster: u32) -> Result<u32, FsError> {
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
        self.entry(cluster).is_ok_and(|value| value != 0)
    }

    /// A folder's raw entries.
    fn dir_bytes(&self, dir: &EntryId) -> Result<Vec<u8>, FsError> {
        match dir {
            EntryId::FixedRoot if self.kind != FsKind::Fat32 => {
                self.volume.read_vec(self.root_start, self.root_bytes)
            }
            EntryId::Clusters {
                cluster,
                chain: true,
                ..
            } => {
                let mut data = Vec::new();
                for &c in self.chain(*cluster)?.iter() {
                    if data.len() >= MAX_DIR_BYTES {
                        break;
                    }
                    let offset = self
                        .cluster_offset(c)
                        .ok_or_else(|| FsError::damaged("A folder's cluster chain is damaged."))?;
                    data.extend(self.volume.read_vec(offset, self.cluster_size)?);
                }
                Ok(data)
            }
            EntryId::Clusters {
                cluster,
                chain: false,
                ..
            } => {
                // A deleted folder: its clusters are taken to follow one
                // another, up to the end-of-folder marker.
                let mut data = Vec::new();
                let mut c = *cluster;
                while let Some(offset) = self.cluster_offset(c)
                    && data.len() < MAX_DIR_BYTES
                {
                    let chunk = self.volume.read_vec(offset, self.cluster_size)?;
                    let ended = chunk.as_chunks::<32>().0.iter().any(|slot| slot[0] == 0);
                    data.extend(chunk);
                    if ended {
                        break;
                    }
                    c += 1;
                }
                Ok(data)
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
        let note = deleted.then(|| {
            let (used, total) = clusters::reused(self, cluster, size, |c| self.in_use(c));
            clusters::deleted_note(!is_dir || size > 0, used, total)
        });
        Entry {
            id: EntryId::Clusters {
                cluster,
                length: size,
                chain: !deleted,
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

    fn root(&self) -> EntryId {
        if self.kind == FsKind::Fat32 {
            EntryId::Clusters {
                cluster: self.root_cluster,
                length: 0,
                chain: true,
            }
        } else {
            EntryId::FixedRoot
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
                "FAT files have no named streams; that stream".to_owned(),
            ));
        }
        clusters::read(self, file, offset, buf)
    }

    fn extents(&self, file: &EntryId, _stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        clusters::extents(self, file)
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
}
