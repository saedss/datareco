//! Builders for small in-memory volumes, laid out the way operating systems
//! write them, for tests. Compiled for this crate's tests, and for other
//! crates' tests through the `testutil` feature.

use std::collections::HashMap;
use std::sync::Arc;

use datareco_io::{Disk, MemorySource};

use crate::FsKind;
use crate::fat::lfn_checksum;

mod ext;
mod ntfs;
pub use ext::{EXT_ROOT, ExtBuilder, ExtOptions};
pub use ntfs::{CLUSTER as NTFS_CLUSTER, NtfsBuilder, ROOT as NTFS_ROOT};

/// 2024-01-15 10:30:00 UTC, as seconds and as FAT date/time.
pub const STAMP: i64 = 1_705_314_600;
pub const DATE: u16 = (44 << 9) | (1 << 5) | 15;
pub const TIME: u16 = (10 << 11) | (30 << 5);

pub fn disk(image: Vec<u8>) -> Arc<Disk> {
    Arc::new(Disk::new("test", Box::new(MemorySource::new(image, 512))))
}

/// Recognizable text: "seed line 00001: …".
pub fn text(seed: &str, len: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut line = 0;
    while out.len() < len {
        out.extend(format!("{seed} line {line:05}: the quick brown fox\n").bytes());
        line += 1;
    }
    out.truncate(len);
    out
}

/// Pseudo-random bytes that are the same on every run.
pub fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn put(image: &mut [u8], at: usize, bytes: &[u8]) {
    image[at..at + bytes.len()].copy_from_slice(bytes);
}

/// LZNT1-compresses data as NTFS stores it (a simple greedy compressor).
pub fn lznt1(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in data.chunks(4096) {
        let packed = lznt1_chunk(chunk);
        if packed.len() < chunk.len() || chunk.len() < 4096 {
            assert!(packed.len() <= 4096, "test data must compress");
            out.extend_from_slice(
                &(0xB000 | u16::try_from(packed.len() - 1).unwrap()).to_le_bytes(),
            );
            out.extend_from_slice(&packed);
        } else {
            // Stored as is.
            out.extend_from_slice(&0x3FFFu16.to_le_bytes());
            out.extend_from_slice(chunk);
        }
    }
    out
}

fn lznt1_chunk(chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < chunk.len() {
        let flags_at = out.len();
        out.push(0);
        for bit in 0..8 {
            if pos >= chunk.len() {
                break;
            }
            let mut shift = 0;
            let mut i = pos.saturating_sub(1);
            while i >= 0x10 {
                shift += 1;
                i >>= 1;
            }
            let longest = ((0x0FFF >> shift) + 3).min(chunk.len() - pos);
            let (mut best, mut best_distance) = (0, 0);
            // Looking back 160 bytes is plenty for test data.
            for distance in 1..=pos.min(1 << (4 + shift)).min(160) {
                let mut len = 0;
                while len < longest && chunk[pos + len - distance] == chunk[pos + len] {
                    len += 1;
                }
                if len > best {
                    (best, best_distance) = (len, distance);
                }
                if best == longest {
                    break;
                }
            }
            if best >= 3 {
                let token = (u16::try_from(best_distance - 1).unwrap() << (12 - shift))
                    | u16::try_from(best - 3).unwrap();
                out.extend_from_slice(&token.to_le_bytes());
                out[flags_at] |= 1 << bit;
                pos += best;
            } else {
                out.push(chunk[pos]);
                pos += 1;
            }
        }
    }
    out
}

/// A file written by a builder: its clusters and its directory entries'
/// places, so a test can delete it the way an OS does.
pub struct Written {
    pub chain: Vec<u32>,
    pub slots: Vec<usize>,
}

/// Writes FAT12, FAT16 or FAT32 volumes.
pub struct FatBuilder {
    kind: FsKind,
    image: Vec<u8>,
    sectors_per_cluster: usize,
    reserved: usize,
    root_entries: usize,
    fat_sectors: usize,
    total: usize,
    fat: Vec<u32>,
    next_free: u32,
    /// Each folder's clusters (key: first cluster; 0 is the FAT12/16 root) and used slots.
    dirs: HashMap<u32, (Vec<u32>, usize)>,
    short_names: u32,
}

impl FatBuilder {
    pub fn new(kind: FsKind, total: usize) -> Self {
        let (sectors_per_cluster, reserved, root_entries) = match kind {
            FsKind::Fat12 => (1, 1, 224),
            FsKind::Fat16 => (4, 1, 512),
            _ => (1, 32, 0),
        };
        let bits = match kind {
            FsKind::Fat12 => 12,
            FsKind::Fat16 => 16,
            _ => 32,
        };
        let root_sectors = root_entries * 32 / 512;
        let mut fat_sectors = 1;
        loop {
            let clusters =
                (total - reserved - 2 * fat_sectors - root_sectors) / sectors_per_cluster;
            let needed = ((clusters + 2) * bits).div_ceil(8 * 512);
            if needed <= fat_sectors {
                break;
            }
            fat_sectors = needed;
        }
        let clusters = (total - reserved - 2 * fat_sectors - root_sectors) / sectors_per_cluster;
        let mut builder = FatBuilder {
            kind,
            image: vec![0; total * 512],
            sectors_per_cluster,
            reserved,
            root_entries,
            fat_sectors,
            total,
            fat: vec![0; clusters + 2],
            next_free: 2,
            dirs: HashMap::new(),
            short_names: 0,
        };
        builder.fat[0] = 0x0FFF_FFF8 & builder.eoc();
        builder.fat[1] = builder.eoc();
        if kind == FsKind::Fat32 {
            let root = builder.alloc(1);
            builder.dirs.insert(root[0], (root, 0));
        } else {
            builder.dirs.insert(0, (Vec::new(), 0));
        }
        builder
    }

    fn eoc(&self) -> u32 {
        match self.kind {
            FsKind::Fat12 => 0xFFF,
            FsKind::Fat16 => 0xFFFF,
            _ => 0x0FFF_FFFF,
        }
    }

    pub fn cluster_size(&self) -> usize {
        self.sectors_per_cluster * 512
    }

    /// Where cluster 2 starts, in bytes.
    pub fn data_start(&self) -> usize {
        self.root_start() + self.root_entries * 32
    }

    /// How many clusters the volume has.
    pub fn cluster_count(&self) -> usize {
        self.fat.len() - 2
    }

    fn root_start(&self) -> usize {
        (self.reserved + 2 * self.fat_sectors) * 512
    }

    fn cluster_at(&self, cluster: u32) -> usize {
        let data = self.root_start() + self.root_entries * 32;
        data + (cluster as usize - 2) * self.cluster_size()
    }

    /// The root folder, for `file`, `mkdir` and friends.
    pub fn root(&self) -> u32 {
        if self.kind == FsKind::Fat32 { 2 } else { 0 }
    }

    /// The next `count` free clusters, linked into a chain.
    pub fn alloc(&mut self, count: usize) -> Vec<u32> {
        let chain: Vec<u32> = (self.next_free..).take(count).collect();
        self.next_free += u32::try_from(count).unwrap();
        self.link(&chain);
        chain
    }

    /// Two chains whose clusters alternate on disk (both fragmented).
    pub fn alloc_interleaved(&mut self, each: usize) -> (Vec<u32>, Vec<u32>) {
        let all = self.alloc(2 * each);
        let a: Vec<u32> = all.iter().step_by(2).copied().collect();
        let b: Vec<u32> = all.iter().skip(1).step_by(2).copied().collect();
        self.link(&a);
        self.link(&b);
        (a, b)
    }

    /// Specific clusters as a chain, e.g. to reuse a deleted file's.
    pub fn link(&mut self, chain: &[u32]) {
        for pair in chain.windows(2) {
            self.fat[pair[0] as usize] = pair[1];
        }
        if let Some(&last) = chain.last() {
            self.fat[last as usize] = self.eoc();
        }
    }

    /// Sets a FAT entry directly (to damage a chain).
    pub fn set_fat(&mut self, cluster: u32, value: u32) {
        self.fat[cluster as usize] = value;
    }

    pub fn write(&mut self, chain: &[u32], data: &[u8]) {
        let size = self.cluster_size();
        for (cluster, piece) in chain.iter().zip(data.chunks(size)) {
            let at = self.cluster_at(*cluster);
            put(&mut self.image, at, piece);
        }
    }

    /// The byte offset of the next free entry slot of a folder, growing it if full.
    fn next_slot(&mut self, dir: u32) -> usize {
        let per_cluster = self.cluster_size() / 32;
        let (chain, used) = self.dirs[&dir].clone();
        let slot = if dir == 0 && self.kind != FsKind::Fat32 {
            assert!(used < self.root_entries, "root folder full");
            self.root_start() + used * 32
        } else {
            let mut chain = chain;
            if used / per_cluster >= chain.len() {
                let more = self.alloc(1);
                chain.push(more[0]);
                self.link(&chain);
                let at = self.cluster_at(more[0]);
                let size = self.cluster_size();
                put(&mut self.image, at, &vec![0; size]);
            }
            let at = self.cluster_at(chain[used / per_cluster]) + (used % per_cluster) * 32;
            self.dirs.get_mut(&dir).unwrap().0 = chain;
            at
        };
        self.dirs.get_mut(&dir).unwrap().1 += 1;
        slot
    }

    fn short_name(&mut self, name: &str) -> [u8; 11] {
        self.short_names += 1;
        let (base, extension) = name.rsplit_once('.').unwrap_or((name, ""));
        let clean = |s: &str, n: usize| -> String {
            s.chars()
                .filter(char::is_ascii_alphanumeric)
                .take(n)
                .collect::<String>()
                .to_ascii_uppercase()
        };
        let mut base = clean(base, 6);
        if base.is_empty() {
            base.push('_');
        }
        let base = format!("{base}~{}", self.short_names);
        let mut short = [b' '; 11];
        short[..base.len().min(8)].copy_from_slice(&base.as_bytes()[..base.len().min(8)]);
        let extension = clean(extension, 3);
        short[8..8 + extension.len()].copy_from_slice(extension.as_bytes());
        short
    }

    /// Writes a folder entry (long-name entries, then the short entry).
    pub fn add(
        &mut self,
        dir: u32,
        name: &str,
        attributes: u8,
        cluster: u32,
        size: u32,
    ) -> Vec<usize> {
        let short = self.short_name(name);
        let checksum = lfn_checksum(&short);
        let mut units: Vec<u16> = name.encode_utf16().collect();
        if !units.len().is_multiple_of(13) {
            units.push(0);
        }
        while !units.len().is_multiple_of(13) {
            units.push(0xFFFF);
        }
        let parts = units.len() / 13;
        let mut slots = Vec::new();
        for part in (1..=parts).rev() {
            let mut slot = [0u8; 32];
            slot[0] = u8::try_from(part).unwrap() | if part == parts { 0x40 } else { 0 };
            slot[11] = 0x0F;
            slot[13] = checksum;
            let chars = &units[(part - 1) * 13..part * 13];
            for (i, unit) in chars.iter().enumerate() {
                let at = match i {
                    0..5 => 1 + 2 * i,
                    5..11 => 14 + 2 * (i - 5),
                    _ => 28 + 2 * (i - 11),
                };
                slot[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            }
            let at = self.next_slot(dir);
            put(&mut self.image, at, &slot);
            slots.push(at);
        }
        let at = self.next_slot(dir);
        let mut slot = [0u8; 32];
        slot[..11].copy_from_slice(&short);
        slot[11] = attributes;
        for (offset, value) in [(14, TIME), (16, DATE), (18, DATE), (22, TIME), (24, DATE)] {
            slot[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        slot[20..22].copy_from_slice(&u16::try_from(cluster >> 16).unwrap().to_le_bytes());
        slot[26..28].copy_from_slice(&u16::try_from(cluster & 0xFFFF).unwrap().to_le_bytes());
        slot[28..32].copy_from_slice(&size.to_le_bytes());
        put(&mut self.image, at, &slot);
        slots.push(at);
        slots
    }

    /// A file stored in the next free clusters.
    pub fn file(&mut self, dir: u32, name: &str, data: &[u8]) -> Written {
        let chain = self.alloc(data.len().div_ceil(self.cluster_size()));
        self.file_in(dir, name, data, chain)
    }

    /// A file stored in the given clusters.
    pub fn file_in(&mut self, dir: u32, name: &str, data: &[u8], chain: Vec<u32>) -> Written {
        self.write(&chain, data);
        let first = chain.first().copied().unwrap_or(0);
        let slots = self.add(dir, name, 0x20, first, u32::try_from(data.len()).unwrap());
        Written { chain, slots }
    }

    pub fn mkdir(&mut self, parent: u32, name: &str) -> (u32, Written) {
        let chain = self.alloc(1);
        let cluster = chain[0];
        let size = self.cluster_size();
        let at = self.cluster_at(cluster);
        put(&mut self.image, at, &vec![0; size]);
        self.dirs.insert(cluster, (chain.clone(), 0));
        for (name, target) in [(*b".          ", cluster), (*b"..         ", parent)] {
            let slot_at = self.next_slot(cluster);
            let mut slot = [0u8; 32];
            slot[..11].copy_from_slice(&name);
            slot[11] = 0x10;
            slot[26..28].copy_from_slice(&u16::try_from(target & 0xFFFF).unwrap().to_le_bytes());
            put(&mut self.image, slot_at, &slot);
        }
        let slots = self.add(parent, name, 0x10, cluster, 0);
        (cluster, Written { chain, slots })
    }

    pub fn label(&mut self, label: &[u8; 11]) {
        let root = self.root();
        let at = self.next_slot(root);
        let mut slot = [0u8; 32];
        slot[..11].copy_from_slice(label);
        slot[11] = 0x08;
        put(&mut self.image, at, &slot);
    }

    /// Deletes like an OS does: E5 over each entry's first byte, clusters freed.
    pub fn delete(&mut self, written: &Written) {
        for &slot in &written.slots {
            self.image[slot] = 0xE5;
        }
        for &cluster in &written.chain {
            self.fat[cluster as usize] = 0;
        }
    }

    pub fn build(mut self) -> Vec<u8> {
        let mut boot = [0u8; 512];
        boot[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        boot[3..11].copy_from_slice(b"TESTFMT ");
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = u8::try_from(self.sectors_per_cluster).unwrap();
        boot[14..16].copy_from_slice(&u16::try_from(self.reserved).unwrap().to_le_bytes());
        boot[16] = 2;
        boot[17..19].copy_from_slice(&u16::try_from(self.root_entries).unwrap().to_le_bytes());
        boot[21] = 0xF8;
        if self.kind == FsKind::Fat32 {
            boot[32..36].copy_from_slice(&u32::try_from(self.total).unwrap().to_le_bytes());
            boot[36..40].copy_from_slice(&u32::try_from(self.fat_sectors).unwrap().to_le_bytes());
            boot[44..48].copy_from_slice(&2u32.to_le_bytes());
            boot[66] = 0x29;
            boot[71..82].copy_from_slice(b"NO NAME    ");
            boot[82..90].copy_from_slice(b"FAT32   ");
        } else {
            match u16::try_from(self.total) {
                Ok(total) => boot[19..21].copy_from_slice(&total.to_le_bytes()),
                Err(_) => {
                    boot[32..36].copy_from_slice(&u32::try_from(self.total).unwrap().to_le_bytes())
                }
            }
            boot[22..24].copy_from_slice(&u16::try_from(self.fat_sectors).unwrap().to_le_bytes());
            boot[38] = 0x29;
            boot[43..54].copy_from_slice(b"NO NAME    ");
            boot[54..62].copy_from_slice(if self.kind == FsKind::Fat12 {
                b"FAT12   "
            } else {
                b"FAT16   "
            });
        }
        boot[510] = 0x55;
        boot[511] = 0xAA;
        put(&mut self.image, 0, &boot);

        let mut table = vec![0u8; self.fat_sectors * 512];
        for (n, &value) in self.fat.iter().enumerate() {
            match self.kind {
                FsKind::Fat12 => {
                    let at = n + n / 2;
                    let value = u16::try_from(value & 0xFFF).unwrap();
                    let byte = |v: u16| u8::try_from(v).unwrap();
                    if n % 2 == 0 {
                        table[at] = byte(value & 0xFF);
                        table[at + 1] = (table[at + 1] & 0xF0) | byte(value >> 8);
                    } else {
                        table[at] = (table[at] & 0x0F) | byte((value & 0x0F) << 4);
                        table[at + 1] = byte(value >> 4);
                    }
                }
                FsKind::Fat16 => table[2 * n..2 * n + 2]
                    .copy_from_slice(&u16::try_from(value).unwrap().to_le_bytes()),
                _ => table[4 * n..4 * n + 4].copy_from_slice(&value.to_le_bytes()),
            }
        }
        for copy in 0..2 {
            let at = (self.reserved + copy * self.fat_sectors) * 512;
            put(&mut self.image, at, &table);
        }
        self.image
    }
}

const EXFAT_CLUSTER: usize = 1024;

/// Writes exFAT volumes with 512-byte sectors and 1 KiB clusters. Files
/// are stored in one piece (no FAT chain) unless a test passes clusters.
pub struct ExfatBuilder {
    image: Vec<u8>,
    total: usize,
    fat_offset: usize,
    fat_length: usize,
    heap_offset: usize,
    cluster_count: usize,
    fat: Vec<u32>,
    bitmap: Vec<u8>,
    bitmap_chain: Vec<u32>,
    next_free: u32,
    /// Each folder's clusters (key: first cluster) and used slots.
    dirs: HashMap<u32, (Vec<u32>, usize)>,
    root: u32,
}

impl ExfatBuilder {
    pub fn new(total: usize) -> Self {
        let fat_offset = 24;
        let rough = (total - fat_offset) / 2;
        let fat_length = ((rough + 2) * 4).div_ceil(512);
        let heap_offset = (fat_offset + fat_length).next_multiple_of(2);
        let cluster_count = (total - heap_offset) / 2;
        let mut builder = ExfatBuilder {
            image: vec![0; total * 512],
            total,
            fat_offset,
            fat_length,
            heap_offset,
            cluster_count,
            fat: vec![0; cluster_count + 2],
            bitmap: vec![0; cluster_count.div_ceil(8)],
            bitmap_chain: Vec::new(),
            next_free: 2,
            dirs: HashMap::new(),
            root: 0,
        };
        builder.fat[0] = 0xFFFF_FFF8;
        builder.fat[1] = 0xFFFF_FFFF;
        let bitmap_clusters = builder.bitmap.len().div_ceil(EXFAT_CLUSTER);
        builder.bitmap_chain = builder.alloc(bitmap_clusters, true);
        let root = builder.alloc(1, true);
        builder.root = root[0];
        builder.dirs.insert(root[0], (root, 0));
        let mut entry = [0u8; 32];
        entry[0] = 0x81;
        entry[20..24].copy_from_slice(&builder.bitmap_chain[0].to_le_bytes());
        entry[24..32].copy_from_slice(&(builder.bitmap.len() as u64).to_le_bytes());
        let at = builder.next_slot(builder.root);
        put(&mut builder.image, at, &entry);
        builder
    }

    pub fn root(&self) -> u32 {
        self.root
    }

    fn cluster_at(&self, cluster: u32) -> usize {
        self.heap_offset * 512 + (cluster as usize - 2) * EXFAT_CLUSTER
    }

    /// Sets or clears a cluster's bit in the allocation bitmap.
    pub fn mark(&mut self, cluster: u32, used: bool) {
        let index = cluster as usize - 2;
        if used {
            self.bitmap[index / 8] |= 1 << (index % 8);
        } else {
            self.bitmap[index / 8] &= !(1 << (index % 8));
        }
    }

    /// The next `count` free clusters; with `chained`, linked in the FAT.
    pub fn alloc(&mut self, count: usize, chained: bool) -> Vec<u32> {
        let clusters: Vec<u32> = (self.next_free..).take(count).collect();
        self.next_free += u32::try_from(count).unwrap();
        for &c in &clusters {
            self.mark(c, true);
        }
        if chained {
            self.link(&clusters);
        }
        clusters
    }

    /// Two FAT chains whose clusters alternate on disk.
    pub fn alloc_interleaved(&mut self, each: usize) -> (Vec<u32>, Vec<u32>) {
        let all = self.alloc(2 * each, false);
        let a: Vec<u32> = all.iter().step_by(2).copied().collect();
        let b: Vec<u32> = all.iter().skip(1).step_by(2).copied().collect();
        self.link(&a);
        self.link(&b);
        (a, b)
    }

    pub fn link(&mut self, chain: &[u32]) {
        for pair in chain.windows(2) {
            self.fat[pair[0] as usize] = pair[1];
        }
        if let Some(&last) = chain.last() {
            self.fat[last as usize] = 0xFFFF_FFFF;
        }
    }

    fn write(&mut self, clusters: &[u32], data: &[u8]) {
        for (cluster, piece) in clusters.iter().zip(data.chunks(EXFAT_CLUSTER)) {
            let at = self.cluster_at(*cluster);
            put(&mut self.image, at, piece);
        }
    }

    fn next_slot(&mut self, dir: u32) -> usize {
        let per_cluster = EXFAT_CLUSTER / 32;
        let (mut chain, used) = self.dirs[&dir].clone();
        if used / per_cluster >= chain.len() {
            assert_eq!(dir, self.root, "only the root folder grows in tests");
            let more = self.alloc(1, false);
            chain.push(more[0]);
            self.link(&chain);
        }
        let at = self.cluster_at(chain[used / per_cluster]) + (used % per_cluster) * 32;
        self.dirs.insert(dir, (chain, used + 1));
        at
    }

    /// Writes an entry set: the file entry, its stream extension and names.
    fn add(
        &mut self,
        dir: u32,
        name: &str,
        attributes: u16,
        first: u32,
        length: u64,
        contiguous: bool,
    ) -> Vec<usize> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let name_entries = units.len().div_ceil(15);
        let mut file = [0u8; 32];
        file[0] = 0x85;
        file[1] = u8::try_from(1 + name_entries).unwrap();
        file[4..6].copy_from_slice(&attributes.to_le_bytes());
        let stamp = (u32::from(DATE) << 16) | u32::from(TIME);
        for at in [8, 12, 16] {
            file[at..at + 4].copy_from_slice(&stamp.to_le_bytes());
        }
        // Written at UTC+2 (8 steps of 15 minutes): 10:30 local is 08:30 UTC.
        file[22..25].copy_from_slice(&[0x88; 3]);
        let mut stream = [0u8; 32];
        stream[0] = 0xC0;
        stream[1] = if contiguous { 0x03 } else { 0x01 };
        stream[3] = u8::try_from(units.len()).unwrap();
        stream[8..16].copy_from_slice(&length.to_le_bytes());
        stream[20..24].copy_from_slice(&first.to_le_bytes());
        stream[24..32].copy_from_slice(&length.to_le_bytes());
        let mut entries = vec![file, stream];
        for part in units.chunks(15) {
            let mut entry = [0u8; 32];
            entry[0] = 0xC1;
            for (i, unit) in part.iter().enumerate() {
                entry[2 + 2 * i..4 + 2 * i].copy_from_slice(&unit.to_le_bytes());
            }
            entries.push(entry);
        }
        let mut slots = Vec::new();
        for entry in entries {
            let at = self.next_slot(dir);
            put(&mut self.image, at, &entry);
            slots.push(at);
        }
        slots
    }

    /// A file stored in one piece in the next free clusters.
    pub fn file(&mut self, dir: u32, name: &str, data: &[u8]) -> Written {
        let clusters = self.alloc(data.len().div_ceil(EXFAT_CLUSTER), false);
        self.file_in(dir, name, data, clusters, true)
    }

    /// A file in the given clusters: in one piece (`contiguous`), or
    /// following a FAT chain the test linked.
    pub fn file_in(
        &mut self,
        dir: u32,
        name: &str,
        data: &[u8],
        clusters: Vec<u32>,
        contiguous: bool,
    ) -> Written {
        for &c in &clusters {
            self.mark(c, true);
        }
        self.write(&clusters, data);
        let first = clusters.first().copied().unwrap_or(0);
        let slots = self.add(dir, name, 0x20, first, data.len() as u64, contiguous);
        Written {
            chain: clusters,
            slots,
        }
    }

    pub fn mkdir(&mut self, parent: u32, name: &str) -> (u32, Written) {
        let clusters = self.alloc(1, false);
        self.dirs.insert(clusters[0], (clusters.clone(), 0));
        let slots = self.add(parent, name, 0x10, clusters[0], EXFAT_CLUSTER as u64, true);
        (
            clusters[0],
            Written {
                chain: clusters,
                slots,
            },
        )
    }

    pub fn label(&mut self, label: &str) {
        let units: Vec<u16> = label.encode_utf16().collect();
        let mut entry = [0u8; 32];
        entry[0] = 0x83;
        entry[1] = u8::try_from(units.len()).unwrap();
        for (i, unit) in units.iter().enumerate() {
            entry[2 + 2 * i..4 + 2 * i].copy_from_slice(&unit.to_le_bytes());
        }
        let root = self.root;
        let at = self.next_slot(root);
        put(&mut self.image, at, &entry);
    }

    /// Deletes like Windows and Linux do: each entry loses its in-use bit
    /// and the clusters are freed in the bitmap; the FAT is left alone.
    pub fn delete(&mut self, written: &Written) {
        for &slot in &written.slots {
            self.image[slot] &= 0x7F;
        }
        for &c in &written.chain {
            self.mark(c, false);
        }
    }

    pub fn build(mut self) -> Vec<u8> {
        let mut boot = [0u8; 512];
        boot[..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
        boot[3..11].copy_from_slice(b"EXFAT   ");
        boot[72..80].copy_from_slice(&(self.total as u64).to_le_bytes());
        boot[80..84].copy_from_slice(&u32::try_from(self.fat_offset).unwrap().to_le_bytes());
        boot[84..88].copy_from_slice(&u32::try_from(self.fat_length).unwrap().to_le_bytes());
        boot[88..92].copy_from_slice(&u32::try_from(self.heap_offset).unwrap().to_le_bytes());
        boot[92..96].copy_from_slice(&u32::try_from(self.cluster_count).unwrap().to_le_bytes());
        boot[96..100].copy_from_slice(&self.root.to_le_bytes());
        boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
        boot[108] = 9;
        boot[109] = 1;
        boot[110] = 1;
        boot[510] = 0x55;
        boot[511] = 0xAA;
        put(&mut self.image, 0, &boot);
        let table: Vec<u8> = self.fat.iter().flat_map(|v| v.to_le_bytes()).collect();
        put(&mut self.image, self.fat_offset * 512, &table);
        let (chain, bitmap) = (self.bitmap_chain.clone(), self.bitmap.clone());
        self.write(&chain, &bitmap);
        self.image
    }
}
