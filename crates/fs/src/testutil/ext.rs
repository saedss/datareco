//! Small ext2/3/4 volumes, laid out the way mke2fs and the Linux driver
//! write them: superblock and a backup in group 1, group descriptors,
//! inode tables, extent trees or block maps, folders whose deleted entries
//! stay in the slack of the entry before, and a journal holding older
//! copies of inode-table blocks.

use std::collections::{BTreeMap, HashSet};

use super::{STAMP, put};

/// The root folder's inode.
pub const EXT_ROOT: u32 = 2;
const JOURNAL_INODE: u32 = 8;
const LOST_FOUND: u32 = 11;
const JOURNAL_MAGIC: u32 = 0xC03B_3998;

/// How to lay a volume out.
#[derive(Clone, Debug)]
pub struct ExtOptions {
    pub block_size: usize,
    pub groups: usize,
    pub blocks_per_group: usize,
    pub inodes_per_group: usize,
    pub inode_size: usize,
    /// Extent trees (ext4); otherwise classic block maps.
    pub extents: bool,
    /// The 64bit feature, with 64-byte group descriptors.
    pub desc64: bool,
    /// Journal blocks (inode 8); 0 for none.
    pub journal_blocks: usize,
    /// metadata_csum: folder blocks end with a 12-byte checksum entry.
    pub csum_tails: bool,
    /// Deleting clears an inode's size and data map (ext3, ext4); ext2
    /// keeps them.
    pub clear_on_delete: bool,
}

impl ExtOptions {
    /// ext4 as mkfs.ext4 makes it (4 KiB blocks, 256-byte inodes).
    pub fn ext4() -> Self {
        Self {
            block_size: 4096,
            groups: 2,
            blocks_per_group: 1024,
            inodes_per_group: 32,
            inode_size: 256,
            extents: true,
            desc64: true,
            journal_blocks: 64,
            csum_tails: true,
            clear_on_delete: true,
        }
    }

    /// ext2 with 1 KiB blocks and 128-byte inodes.
    pub fn ext2() -> Self {
        Self {
            block_size: 1024,
            groups: 2,
            blocks_per_group: 2048,
            inodes_per_group: 32,
            inode_size: 128,
            extents: false,
            desc64: false,
            journal_blocks: 0,
            csum_tails: false,
            clear_on_delete: false,
        }
    }

    /// ext3: ext2 plus a journal, and deleting clears block maps.
    pub fn ext3() -> Self {
        Self {
            journal_blocks: 128,
            clear_on_delete: true,
            ..Self::ext2()
        }
    }
}

#[derive(Clone, Debug)]
struct InodeData {
    mode: u16,
    size: u64,
    links: u16,
    dtime: u32,
    flags: u32,
    block: [u8; 60],
    mtime: u32,
    /// Blocks it uses, for its block count.
    used: u64,
}

impl Default for InodeData {
    fn default() -> Self {
        Self {
            mode: 0,
            size: 0,
            links: 0,
            dtime: 0,
            flags: 0,
            block: [0; 60],
            mtime: 0,
            used: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct DirEntry {
    name: Vec<u8>,
    inode: u32,
    file_type: u8,
    deleted: bool,
    /// Start a new folder block with this entry.
    new_block: bool,
}

#[derive(Clone, Debug)]
struct DirData {
    parent: u32,
    blocks: Vec<u32>,
    entries: Vec<DirEntry>,
    indexed: bool,
}

/// Builds an ext volume in memory.
pub struct ExtBuilder {
    o: ExtOptions,
    image: Vec<u8>,
    first_data_block: usize,
    reserved: HashSet<usize>,
    next_block: usize,
    next_inode: u32,
    inodes: BTreeMap<u32, InodeData>,
    dirs: BTreeMap<u32, DirData>,
    label: String,
    /// Journal transactions: copies of inode-table blocks.
    transactions: Vec<Vec<(usize, Vec<u8>)>>,
    clock: u32,
}

impl ExtBuilder {
    pub fn new(o: ExtOptions) -> Self {
        let first_data_block = usize::from(o.block_size == 1024);
        let blocks = first_data_block + o.groups * o.blocks_per_group;
        let mut b = Self {
            image: vec![0; blocks * o.block_size],
            first_data_block,
            reserved: HashSet::new(),
            next_block: 0,
            next_inode: 12,
            inodes: BTreeMap::new(),
            dirs: BTreeMap::new(),
            label: String::new(),
            transactions: Vec::new(),
            clock: u32::try_from(STAMP).unwrap(),
            o,
        };
        // Superblock and descriptors in groups 0 and 1, then the inode tables.
        for group in 0..b.o.groups.min(2) {
            let start = b.group_start(group);
            b.reserved.insert(start);
            b.reserved.insert(start + 1);
        }
        let mut next = b.first_data_block + 2;
        if b.o.block_size == 4096 {
            b.reserved.insert(0);
        }
        for _ in 0..b.o.groups {
            for block in next..next + b.table_blocks() {
                b.reserved.insert(block);
            }
            next += b.table_blocks();
        }
        b.next_block = next;
        let root = InodeData {
            mode: 0x41ED,
            links: 2,
            ..InodeData::default()
        };
        b.inodes.insert(EXT_ROOT, root);
        let blocks = b.alloc(1);
        b.dirs.insert(
            EXT_ROOT,
            DirData {
                parent: EXT_ROOT,
                blocks: blocks.clone(),
                entries: Vec::new(),
                indexed: false,
            },
        );
        b.map_dir(EXT_ROOT);
        b.mkdir_numbered(EXT_ROOT, "lost+found", LOST_FOUND, 1, false);
        if b.o.journal_blocks > 0 {
            let blocks = b.alloc(b.o.journal_blocks);
            let size = (blocks.len() * b.o.block_size) as u64;
            let mut inode = InodeData {
                mode: 0x8180,
                links: 1,
                size,
                ..InodeData::default()
            };
            b.map(&mut inode, &blocks, false);
            b.inodes.insert(JOURNAL_INODE, inode);
        }
        b
    }

    fn table_blocks(&self) -> usize {
        (self.o.inodes_per_group * self.o.inode_size).div_ceil(self.o.block_size)
    }

    fn group_start(&self, group: usize) -> usize {
        self.first_data_block + group * self.o.blocks_per_group
    }

    /// The first block of group `group`'s inode table.
    pub fn inode_table(&self, group: usize) -> usize {
        self.first_data_block + 2 + group * self.table_blocks()
    }

    /// The byte offset of group 1's backup superblock.
    pub fn backup_superblock(&self) -> usize {
        self.group_start(1) * self.o.block_size
    }

    pub fn block_size(&self) -> usize {
        self.o.block_size
    }

    pub fn label(&mut self, label: &str) {
        self.label = label.to_owned();
    }

    /// `count` free blocks in a row (skipping metadata).
    pub fn alloc(&mut self, count: usize) -> Vec<u32> {
        let mut blocks = Vec::new();
        while blocks.len() < count {
            let block = self.next_block;
            self.next_block += 1;
            assert!(
                block < self.image.len() / self.o.block_size,
                "test volume is full"
            );
            if !self.reserved.contains(&block) {
                blocks.push(u32::try_from(block).unwrap());
            }
        }
        blocks
    }

    /// Two files' worth of blocks, alternating, `each` apiece.
    pub fn alloc_interleaved(&mut self, each: usize) -> (Vec<u32>, Vec<u32>) {
        let all = self.alloc(each * 2);
        let first = all.iter().step_by(2).copied().collect();
        let second = all.iter().skip(1).step_by(2).copied().collect();
        (first, second)
    }

    fn write_block(&mut self, block: u32, data: &[u8]) {
        assert!(data.len() <= self.o.block_size);
        put(&mut self.image, block as usize * self.o.block_size, data);
    }

    fn new_inode(&mut self) -> u32 {
        let n = self.next_inode;
        self.next_inode += 1;
        assert!(
            (n as usize) <= self.o.groups * self.o.inodes_per_group,
            "out of inodes"
        );
        n
    }

    fn tick(&mut self) -> u32 {
        self.clock += 60;
        self.clock
    }

    /// Records where an inode's data is: an extent tree (with a leaf block
    /// when it has more than 4 extents) or a block map with indirect blocks.
    /// `blocks[i]` is logical block i; 0 is a hole.
    fn map(&mut self, inode: &mut InodeData, blocks: &[u32], unwritten: bool) {
        inode.used = blocks.iter().filter(|&&b| b != 0).count() as u64;
        if self.o.extents {
            inode.flags |= 0x8_0000;
            let mut extents: Vec<(u32, u32, u16)> = Vec::new();
            for (logical, &physical) in blocks.iter().enumerate() {
                if physical == 0 {
                    continue;
                }
                let logical = u32::try_from(logical).unwrap();
                match extents.last_mut() {
                    Some((l, p, len))
                        if *l + u32::from(*len) == logical && *p + u32::from(*len) == physical =>
                    {
                        *len += 1;
                    }
                    _ => extents.push((logical, physical, 1)),
                }
            }
            let entry = |(logical, physical, length): (u32, u32, u16)| {
                let mut e = [0u8; 12];
                e[0..4].copy_from_slice(&logical.to_le_bytes());
                let length = if unwritten { length + 32768 } else { length };
                e[4..6].copy_from_slice(&length.to_le_bytes());
                e[8..12].copy_from_slice(&physical.to_le_bytes());
                e
            };
            let header = |entries: usize, max: u16, depth: u16| {
                let mut h = [0u8; 12];
                h[0..2].copy_from_slice(&0xF30Au16.to_le_bytes());
                h[2..4].copy_from_slice(&u16::try_from(entries).unwrap().to_le_bytes());
                h[4..6].copy_from_slice(&max.to_le_bytes());
                h[6..8].copy_from_slice(&depth.to_le_bytes());
                h
            };
            let mut root = [0u8; 60];
            if extents.len() <= 4 {
                root[..12].copy_from_slice(&header(extents.len(), 4, 0));
                for (i, &e) in extents.iter().enumerate() {
                    root[12 + i * 12..24 + i * 12].copy_from_slice(&entry(e));
                }
            } else {
                let leaf = self.alloc(1)[0];
                inode.used += 1;
                let max = u16::try_from((self.o.block_size - 12) / 12).unwrap();
                let mut data = vec![0u8; self.o.block_size];
                data[..12].copy_from_slice(&header(extents.len(), max, 0));
                for (i, &e) in extents.iter().enumerate() {
                    data[12 + i * 12..24 + i * 12].copy_from_slice(&entry(e));
                }
                self.write_block(leaf, &data);
                root[..12].copy_from_slice(&header(1, 4, 1));
                // Index entry: first logical block, then the leaf block.
                root[16..20].copy_from_slice(&leaf.to_le_bytes());
            }
            inode.block = root;
        } else {
            let per_block = self.o.block_size / 4;
            let mut root = [0u8; 60];
            for (i, &b) in blocks.iter().take(12).enumerate() {
                root[i * 4..i * 4 + 4].copy_from_slice(&b.to_le_bytes());
            }
            let rest = blocks.get(12..).unwrap_or(&[]);
            if !rest.is_empty() {
                let single = &rest[..rest.len().min(per_block)];
                let block = self.pointer_block(single);
                inode.used += 1;
                root[48..52].copy_from_slice(&block.to_le_bytes());
            }
            let rest = rest.get(per_block..).unwrap_or(&[]);
            if !rest.is_empty() {
                assert!(rest.len() <= per_block * per_block, "file too big");
                let mut lower = Vec::new();
                for chunk in rest.chunks(per_block) {
                    lower.push(self.pointer_block(chunk));
                    inode.used += 1;
                }
                let block = self.pointer_block(&lower);
                inode.used += 1;
                root[52..56].copy_from_slice(&block.to_le_bytes());
            }
            inode.block = root;
        }
    }

    fn pointer_block(&mut self, pointers: &[u32]) -> u32 {
        let block = self.alloc(1)[0];
        let data: Vec<u8> = pointers.iter().flat_map(|p| p.to_le_bytes()).collect();
        self.write_block(block, &data);
        block
    }

    fn add_entry(&mut self, dir: u32, name: &str, inode: u32, file_type: u8, new_block: bool) {
        self.dirs
            .get_mut(&dir)
            .expect("not a folder")
            .entries
            .push(DirEntry {
                name: name.as_bytes().to_vec(),
                inode,
                file_type,
                deleted: false,
                new_block,
            });
    }

    /// A file stored in the given blocks (`0` for holes); `data` is its
    /// whole content, zeros where the holes are.
    pub fn file_in(&mut self, dir: u32, name: &str, data: &[u8], blocks: &[u32]) -> u32 {
        self.file_with(dir, name, data, blocks, false, false)
    }

    pub fn file(&mut self, dir: u32, name: &str, data: &[u8]) -> u32 {
        let blocks = self.alloc(data.len().div_ceil(self.o.block_size));
        self.file_in(dir, name, data, &blocks)
    }

    /// A file whose entry starts a new folder block.
    pub fn file_in_new_block(&mut self, dir: u32, name: &str, data: &[u8]) -> u32 {
        let blocks = self.alloc(data.len().div_ceil(self.o.block_size));
        self.file_with(dir, name, data, &blocks, false, true)
    }

    /// A file of `size` bytes whose blocks are allocated but never written
    /// (an unwritten extent): it reads as zeros.
    pub fn preallocated(&mut self, dir: u32, name: &str, size: usize) -> u32 {
        let blocks = self.alloc(size.div_ceil(self.o.block_size));
        for &b in &blocks {
            self.write_block(b, &vec![0xEE; self.o.block_size]);
        }
        let n = self.new_inode();
        let mut inode = InodeData {
            mode: 0x81A4,
            links: 1,
            size: size as u64,
            mtime: self.tick(),
            ..InodeData::default()
        };
        self.map(&mut inode, &blocks, true);
        self.inodes.insert(n, inode);
        self.add_entry(dir, name, n, 1, false);
        n
    }

    fn file_with(
        &mut self,
        dir: u32,
        name: &str,
        data: &[u8],
        blocks: &[u32],
        unwritten: bool,
        new_block: bool,
    ) -> u32 {
        let bs = self.o.block_size;
        for (i, &b) in blocks.iter().enumerate() {
            if b != 0 {
                let piece = data
                    .get(i * bs..data.len().min((i + 1) * bs))
                    .unwrap_or(&[]);
                self.write_block(b, piece);
            }
        }
        let n = self.new_inode();
        let mut inode = InodeData {
            mode: 0x81A4,
            links: 1,
            size: data.len() as u64,
            mtime: self.tick(),
            ..InodeData::default()
        };
        self.map(&mut inode, blocks, unwritten);
        self.inodes.insert(n, inode);
        self.add_entry(dir, name, n, 1, new_block);
        n
    }

    /// A symbolic link to `target` (kept in the inode).
    pub fn symlink(&mut self, dir: u32, name: &str, target: &str) -> u32 {
        assert!(target.len() < 60);
        let n = self.new_inode();
        let mut block = [0u8; 60];
        block[..target.len()].copy_from_slice(target.as_bytes());
        let inode = InodeData {
            mode: 0xA1FF,
            links: 1,
            size: target.len() as u64,
            mtime: self.tick(),
            block,
            ..InodeData::default()
        };
        self.inodes.insert(n, inode);
        self.add_entry(dir, name, n, 7, false);
        n
    }

    pub fn mkdir(&mut self, parent: u32, name: &str) -> u32 {
        let n = self.new_inode();
        self.mkdir_numbered(parent, name, n, 1, false);
        n
    }

    /// A folder with `blocks` blocks; `indexed` makes block 0 a hash-tree
    /// root, with the entries in the blocks after it.
    pub fn mkdir_with(&mut self, parent: u32, name: &str, blocks: usize, indexed: bool) -> u32 {
        let n = self.new_inode();
        self.mkdir_numbered(parent, name, n, blocks, indexed);
        n
    }

    fn mkdir_numbered(&mut self, parent: u32, name: &str, n: u32, blocks: usize, indexed: bool) {
        let data_blocks = self.alloc(blocks);
        let inode = InodeData {
            mode: 0x41ED,
            links: 2,
            mtime: self.tick(),
            flags: if indexed { 0x1000 } else { 0 },
            ..InodeData::default()
        };
        self.inodes.insert(n, inode);
        self.dirs.insert(
            n,
            DirData {
                parent,
                blocks: data_blocks,
                entries: Vec::new(),
                indexed,
            },
        );
        self.map_dir(n);
        self.add_entry(parent, name, n, 2, false);
        self.inodes.get_mut(&parent).unwrap().links += 1;
    }

    fn map_dir(&mut self, n: u32) {
        let blocks = self.dirs[&n].blocks.clone();
        let mut inode = self.inodes[&n].clone();
        inode.size = (blocks.len() * self.o.block_size) as u64;
        self.map(&mut inode, &blocks, false);
        self.inodes.insert(n, inode);
    }

    /// Writes the inode-table blocks as they are now into the journal, as
    /// one transaction (as the driver does when it commits).
    pub fn commit(&mut self) {
        assert!(self.o.journal_blocks > 0, "no journal");
        let mut blocks = Vec::new();
        for group in 0..self.o.groups {
            for i in 0..self.table_blocks() {
                let block = self.inode_table(group) + i;
                let data = self.table_block(group, i);
                if data.iter().any(|&b| b != 0) {
                    blocks.push((block, data));
                }
            }
        }
        self.transactions.push(blocks);
    }

    /// Deletes a file or (empty) folder the way the driver does.
    pub fn delete(&mut self, n: u32) {
        for dir in self.dirs.values_mut() {
            for entry in &mut dir.entries {
                if entry.inode == n && !entry.deleted {
                    entry.deleted = true;
                }
            }
        }
        let clear = self.o.clear_on_delete;
        let extents = self.o.extents;
        let dtime = self.tick();
        let inode = self.inodes.get_mut(&n).expect("no such inode");
        inode.links = 0;
        inode.dtime = dtime;
        if clear {
            inode.size = 0;
            inode.used = 0;
            inode.block = [0; 60];
            if extents {
                inode.block[0..2].copy_from_slice(&0xF30Au16.to_le_bytes());
                inode.block[4..6].copy_from_slice(&4u16.to_le_bytes());
            }
        }
    }

    /// Serializes one inode.
    fn inode_bytes(&self, n: u32) -> Vec<u8> {
        let mut raw = vec![0u8; self.o.inode_size];
        let Some(i) = self.inodes.get(&n) else {
            return raw;
        };
        raw[0..2].copy_from_slice(&i.mode.to_le_bytes());
        let low = u32::try_from(i.size & 0xFFFF_FFFF).unwrap();
        raw[4..8].copy_from_slice(&low.to_le_bytes());
        for at in [8, 12, 16] {
            raw[at..at + 4].copy_from_slice(&i.mtime.to_le_bytes());
        }
        raw[20..24].copy_from_slice(&i.dtime.to_le_bytes());
        raw[26..28].copy_from_slice(&i.links.to_le_bytes());
        let sectors = u32::try_from(i.used * (self.o.block_size as u64 / 512)).unwrap();
        raw[28..32].copy_from_slice(&sectors.to_le_bytes());
        raw[32..36].copy_from_slice(&i.flags.to_le_bytes());
        raw[40..100].copy_from_slice(&i.block);
        raw[108..112].copy_from_slice(&u32::try_from(i.size >> 32).unwrap().to_le_bytes());
        if self.o.inode_size >= 256 {
            raw[128..130].copy_from_slice(&32u16.to_le_bytes());
            // Created a day before it was last changed.
            raw[144..148].copy_from_slice(&i.mtime.saturating_sub(86_400).to_le_bytes());
        }
        raw
    }

    /// Block `index` of group `group`'s inode table, as it is now.
    fn table_block(&self, group: usize, index: usize) -> Vec<u8> {
        let per_block = self.o.block_size / self.o.inode_size;
        let mut data = vec![0u8; self.o.block_size];
        for slot in 0..per_block {
            let number = group * self.o.inodes_per_group + index * per_block + slot + 1;
            if slot + index * per_block >= self.o.inodes_per_group {
                break;
            }
            let raw = self.inode_bytes(u32::try_from(number).unwrap());
            data[slot * self.o.inode_size..(slot + 1) * self.o.inode_size].copy_from_slice(&raw);
        }
        data
    }

    fn write_dir(&mut self, n: u32) {
        let dir = self.dirs[&n].clone();
        let bs = self.o.block_size;
        let usable = bs - if self.o.csum_tails { 12 } else { 0 };
        let mut all = vec![
            DirEntry {
                name: b".".to_vec(),
                inode: n,
                file_type: 2,
                deleted: false,
                new_block: false,
            },
            DirEntry {
                name: b"..".to_vec(),
                inode: dir.parent,
                file_type: 2,
                deleted: false,
                new_block: false,
            },
        ];
        let mut first_block = 0;
        if dir.indexed {
            let mut root = vec![0u8; bs];
            dirent(&mut root, 0, n, 12, b".", 2);
            dirent(
                &mut root,
                12,
                dir.parent,
                u16::try_from(bs - 12).unwrap(),
                b"..",
                2,
            );
            // dx_root_info, then one index entry pointing at block 1.
            root[24 + 4] = 1;
            root[24 + 5] = 8;
            let limit = u16::try_from((bs - 32) / 8).unwrap();
            root[32..34].copy_from_slice(&limit.to_le_bytes());
            root[34..36].copy_from_slice(&1u16.to_le_bytes());
            root[36..40].copy_from_slice(&1u32.to_le_bytes());
            self.write_block(dir.blocks[0], &root);
            all.clear();
            first_block = 1;
        }
        all.extend(dir.entries.iter().cloned());
        // Place entries, then work out lengths and deletions per block.
        let mut placed: Vec<Vec<(DirEntry, usize)>> = vec![Vec::new(); dir.blocks.len()];
        let mut block = first_block;
        let mut at = 0;
        for entry in all {
            let needed = (8 + entry.name.len()).next_multiple_of(4);
            if (entry.new_block && at > 0) || at + needed > usable {
                block += 1;
                at = 0;
            }
            assert!(block < dir.blocks.len(), "folder {n} is full");
            placed[block].push((entry, at));
            at += needed;
        }
        for (index, entries) in placed.iter().enumerate().skip(first_block) {
            let mut data = vec![0u8; bs];
            let mut lengths: Vec<usize> = entries
                .iter()
                .enumerate()
                .map(|(i, (_, start))| entries.get(i + 1).map_or(usable, |(_, next)| *next) - start)
                .collect();
            let originals = lengths.clone();
            let mut owner = 0;
            for (i, (entry, _)) in entries.iter().enumerate() {
                if entry.deleted && i > 0 {
                    lengths[owner] += originals[i];
                } else {
                    owner = i;
                }
            }
            for (i, (entry, start)) in entries.iter().enumerate() {
                let inode = if entry.deleted && i == 0 {
                    0
                } else {
                    entry.inode
                };
                // A deleted entry keeps its own old length; the one before
                // it now covers it.
                let length = if entry.deleted && i > 0 {
                    originals[i]
                } else {
                    lengths[i]
                };
                dirent(
                    &mut data,
                    *start,
                    inode,
                    u16::try_from(length).unwrap(),
                    &entry.name,
                    entry.file_type,
                );
            }
            if entries.is_empty() {
                dirent(&mut data, 0, 0, u16::try_from(usable).unwrap(), b"", 0);
            }
            if self.o.csum_tails {
                data[usable + 4..usable + 6].copy_from_slice(&12u16.to_le_bytes());
                data[usable + 7] = 0xDE;
            }
            self.write_block(dir.blocks[index], &data);
        }
    }

    fn superblock(&self, group: usize) -> Vec<u8> {
        let o = &self.o;
        let mut sb = vec![0u8; 1024];
        let blocks = self.image.len() / o.block_size;
        let w32 =
            |sb: &mut Vec<u8>, at: usize, v: u32| sb[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let w16 =
            |sb: &mut Vec<u8>, at: usize, v: u16| sb[at..at + 2].copy_from_slice(&v.to_le_bytes());
        w32(
            &mut sb,
            0,
            u32::try_from(o.groups * o.inodes_per_group).unwrap(),
        );
        w32(&mut sb, 4, u32::try_from(blocks).unwrap());
        w32(&mut sb, 20, u32::try_from(self.first_data_block).unwrap());
        w32(&mut sb, 24, o.block_size.trailing_zeros() - 10);
        w32(&mut sb, 28, o.block_size.trailing_zeros() - 10);
        w32(&mut sb, 32, u32::try_from(o.blocks_per_group).unwrap());
        w32(&mut sb, 36, u32::try_from(o.blocks_per_group).unwrap());
        w32(&mut sb, 40, u32::try_from(o.inodes_per_group).unwrap());
        w16(&mut sb, 56, 0xEF53);
        w16(&mut sb, 58, 1);
        w32(&mut sb, 76, 1);
        w32(&mut sb, 84, 11);
        w16(&mut sb, 88, u16::try_from(o.inode_size).unwrap());
        w16(&mut sb, 90, u16::try_from(group).unwrap());
        let mut compat = 0x8 | 0x20;
        if o.journal_blocks > 0 {
            compat |= 0x4;
        }
        let mut incompat = 0x2;
        if o.extents {
            incompat |= 0x40 | 0x200;
        }
        if o.desc64 {
            incompat |= 0x80;
        }
        let mut ro_compat = 0x1 | 0x2;
        if o.inode_size > 128 {
            ro_compat |= 0x40;
        }
        if o.csum_tails {
            ro_compat |= 0x400;
        }
        w32(&mut sb, 92, compat);
        w32(&mut sb, 96, incompat);
        w32(&mut sb, 100, ro_compat);
        sb[104..120].copy_from_slice(&[0x5A; 16]);
        let label = self.label.as_bytes();
        sb[120..120 + label.len().min(16)].copy_from_slice(&label[..label.len().min(16)]);
        if o.journal_blocks > 0 {
            w32(&mut sb, 224, JOURNAL_INODE);
        }
        if o.desc64 {
            w16(&mut sb, 254, 64);
        }
        sb
    }

    fn descriptors(&self) -> Vec<u8> {
        let size = if self.o.desc64 { 64 } else { 32 };
        let mut data = vec![0u8; self.o.block_size];
        for group in 0..self.o.groups {
            let table = u32::try_from(self.inode_table(group)).unwrap();
            data[group * size + 8..group * size + 12].copy_from_slice(&table.to_le_bytes());
        }
        data
    }

    fn write_journal(&mut self) {
        let Some(journal) = self.inodes.get(&JOURNAL_INODE).cloned() else {
            return;
        };
        let bs = self.o.block_size;
        // The journal's blocks were allocated in a row: find the first.
        let first = if self.o.extents {
            u32::from_le_bytes(journal.block[20..24].try_into().unwrap())
        } else {
            u32::from_le_bytes(journal.block[0..4].try_into().unwrap())
        } as usize;
        let csum3 = self.o.desc64;
        let mut log: Vec<Vec<u8>> = Vec::new();
        let mut jsb = vec![0u8; bs];
        let be =
            |d: &mut Vec<u8>, at: usize, v: u32| d[at..at + 4].copy_from_slice(&v.to_be_bytes());
        be(&mut jsb, 0, JOURNAL_MAGIC);
        be(&mut jsb, 4, 4);
        be(&mut jsb, 12, u32::try_from(bs).unwrap());
        be(&mut jsb, 16, u32::try_from(self.o.journal_blocks).unwrap());
        be(&mut jsb, 20, 1);
        be(&mut jsb, 24, 1);
        // s_start 0: the log is clean (as after unmounting).
        if csum3 {
            be(&mut jsb, 40, 0x2 | 0x10);
        }
        log.push(jsb);
        for (index, transaction) in self.transactions.iter().enumerate() {
            let sequence = u32::try_from(index + 1).unwrap();
            let mut descriptor = vec![0u8; bs];
            be(&mut descriptor, 0, JOURNAL_MAGIC);
            be(&mut descriptor, 4, 1);
            be(&mut descriptor, 8, sequence);
            let mut at = 12;
            for (i, (block, _)) in transaction.iter().enumerate() {
                let mut flags = if i == 0 { 0 } else { 0x2 };
                if i + 1 == transaction.len() {
                    flags |= 0x8;
                }
                be(&mut descriptor, at, u32::try_from(*block).unwrap());
                if csum3 {
                    be(&mut descriptor, at + 4, flags);
                    at += 16;
                } else {
                    let flags = u16::try_from(flags).unwrap();
                    descriptor[at + 6..at + 8].copy_from_slice(&flags.to_be_bytes());
                    at += 8;
                }
                if i == 0 {
                    at += 16;
                }
            }
            log.push(descriptor);
            for (_, data) in transaction {
                log.push(data.clone());
            }
            let mut commit = vec![0u8; bs];
            be(&mut commit, 0, JOURNAL_MAGIC);
            be(&mut commit, 4, 2);
            be(&mut commit, 8, sequence);
            log.push(commit);
        }
        assert!(log.len() <= self.o.journal_blocks, "journal too small");
        for (i, block) in log.iter().enumerate() {
            put(&mut self.image, (first + i) * bs, block);
        }
    }

    pub fn build(mut self) -> Vec<u8> {
        let dirs: Vec<u32> = self.dirs.keys().copied().collect();
        for n in dirs {
            self.write_dir(n);
        }
        self.write_journal();
        let bs = self.o.block_size;
        for group in 0..self.o.groups {
            for index in 0..self.table_blocks() {
                let data = self.table_block(group, index);
                let at = (self.inode_table(group) + index) * bs;
                put(&mut self.image, at, &data);
            }
        }
        let descriptors = self.descriptors();
        for group in 0..self.o.groups.min(2) {
            let sb = self.superblock(group);
            let start = self.group_start(group);
            let at = if group == 0 { 1024 } else { start * bs };
            put(&mut self.image, at, &sb);
            put(&mut self.image, (start + 1) * bs, &descriptors);
        }
        self.image
    }
}

fn dirent(data: &mut [u8], at: usize, inode: u32, rec_len: u16, name: &[u8], file_type: u8) {
    data[at..at + 4].copy_from_slice(&inode.to_le_bytes());
    data[at + 4..at + 6].copy_from_slice(&rec_len.to_le_bytes());
    data[at + 6] = u8::try_from(name.len()).unwrap();
    data[at + 7] = file_type;
    data[at + 8..at + 8 + name.len()].copy_from_slice(name);
}
