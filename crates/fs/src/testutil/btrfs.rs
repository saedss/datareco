//! Small btrfs volumes, laid out the way mkfs.btrfs and the driver write
//! them: superblock (and its copy at 64 MiB on big enough volumes), the
//! system chunk array, chunk tree, root tree and subvolume trees (inode,
//! inode ref, folder item and index, file extent items), with logical
//! addresses that chunks map to the disk.

use std::collections::{BTreeMap, HashMap};

use super::{STAMP, put};
use datareco_partitions::crc32c;

/// The top-level subvolume.
pub const BTRFS_TOP: u64 = 5;
/// Every subvolume's root folder.
pub const BTRFS_ROOT_DIR: u64 = 256;

const NODE: usize = 4096;
const BLOCK: u64 = 4096;
const BLOCK_BYTES: usize = 4096;
const MIB: u64 = 1 << 20;
/// (logical start, physical start, length, type) of the system,
/// metadata and data chunks. Metadata is DUP: a second copy at 60 MiB.
const SYSTEM: (u64, u64, u64) = (MIB, MIB, MIB);
const METADATA: (u64, u64, u64) = (0x1000_0000, 2 * MIB, 4 * MIB);
const DATA_LOGICAL: u64 = 0x2000_0000;
const DATA_PHYSICAL: u64 = 6 * MIB;

#[derive(Clone, Debug)]
enum ExtentSpec {
    Inline {
        data: Vec<u8>,
        compression: u8,
    },
    Regular {
        offset: u64,
        disk: u64,
        length: u64,
        preallocated: bool,
        compression: u8,
    },
}

#[derive(Clone, Debug)]
struct InodeSpec {
    mode: u32,
    size: u64,
    links: u32,
    time: u64,
    extents: Vec<ExtentSpec>,
}

/// Where a folder entry points: an inode, or another subvolume.
#[derive(Clone, Copy, Debug)]
enum Target {
    Inode(u64),
    Subvolume(u64),
}

#[derive(Clone, Debug)]
struct TreeSpec {
    inodes: BTreeMap<u64, InodeSpec>,
    /// (folder, name, target, file type).
    entries: Vec<(u64, String, Target, u8)>,
    next_inode: u64,
}

/// Builds a btrfs volume in memory.
pub struct BtrfsBuilder {
    image: Vec<u8>,
    label: String,
    trees: BTreeMap<u64, TreeSpec>,
    next_data: u64,
    data_end: u64,
    next_meta: u64,
    next_system: u64,
    clock: u64,
}

fn key(objectid: u64, kind: u8, offset: u64) -> Vec<u8> {
    let mut k = objectid.to_le_bytes().to_vec();
    k.push(kind);
    k.extend_from_slice(&offset.to_le_bytes());
    k
}

impl BtrfsBuilder {
    /// A volume of `mib` MiB; 65 or more gets the superblock copy at 64 MiB.
    pub fn new(mib: usize) -> Self {
        assert!(mib >= 16, "btrfs test volumes need 16 MiB");
        let size = mib as u64 * MIB;
        let mut b = Self {
            image: vec![0u8; usize::try_from(size).unwrap()],
            label: String::new(),
            trees: BTreeMap::new(),
            next_data: DATA_LOGICAL,
            data_end: DATA_LOGICAL + (size.min(60 * MIB) - DATA_PHYSICAL),
            next_meta: METADATA.0,
            next_system: SYSTEM.0,
            clock: u64::try_from(STAMP).unwrap(),
        };
        b.trees.insert(BTRFS_TOP, Self::new_tree(b.clock));
        b
    }

    fn new_tree(time: u64) -> TreeSpec {
        let mut inodes = BTreeMap::new();
        inodes.insert(
            BTRFS_ROOT_DIR,
            InodeSpec {
                mode: 0o40755,
                size: 0,
                links: 1,
                time,
                extents: Vec::new(),
            },
        );
        TreeSpec {
            inodes,
            entries: Vec::new(),
            next_inode: 257,
        }
    }

    pub fn label(&mut self, label: &str) {
        self.label = label.to_owned();
    }

    fn tick(&mut self) -> u64 {
        self.clock += 60;
        self.clock
    }

    /// Where a logical data address is on the disk.
    fn data_physical(logical: u64) -> usize {
        usize::try_from(DATA_PHYSICAL + (logical - DATA_LOGICAL)).unwrap()
    }

    /// `count` data blocks in a row, by logical address.
    pub fn alloc(&mut self, count: usize) -> Vec<u64> {
        let start = self.next_data;
        self.next_data += count as u64 * BLOCK;
        assert!(self.next_data <= self.data_end, "test volume is full");
        (0..count as u64).map(|i| start + i * BLOCK).collect()
    }

    fn add(&mut self, tree: u64, parent: u64, name: &str, mode: u32, kind: u8) -> u64 {
        let time = self.tick();
        let spec = self.trees.get_mut(&tree).expect("no such subvolume");
        let id = spec.next_inode;
        spec.next_inode += 1;
        spec.inodes.insert(
            id,
            InodeSpec {
                mode,
                size: 0,
                links: 1,
                time,
                extents: Vec::new(),
            },
        );
        spec.entries
            .push((parent, name.to_owned(), Target::Inode(id), kind));
        id
    }

    fn inode(&mut self, tree: u64, id: u64) -> &mut InodeSpec {
        self.trees
            .get_mut(&tree)
            .unwrap()
            .inodes
            .get_mut(&id)
            .unwrap()
    }

    pub fn mkdir(&mut self, tree: u64, parent: u64, name: &str) -> u64 {
        self.add(tree, parent, name, 0o40755, 2)
    }

    /// A file: up to 2048 bytes inline (as the driver does), else in blocks.
    pub fn file(&mut self, tree: u64, parent: u64, name: &str, data: &[u8]) -> u64 {
        if data.len() <= 2048 && !data.is_empty() {
            let id = self.add(tree, parent, name, 0o100644, 1);
            let inode = self.inode(tree, id);
            inode.size = data.len() as u64;
            inode.extents.push(ExtentSpec::Inline {
                data: data.to_vec(),
                compression: 0,
            });
            return id;
        }
        let blocks = self.alloc(data.len().div_ceil(BLOCK_BYTES));
        self.file_in(tree, parent, name, data, &blocks)
    }

    /// A file in the given data blocks (logical addresses; 0 for holes,
    /// which aren't recorded, as with the no-holes feature).
    pub fn file_in(
        &mut self,
        tree: u64,
        parent: u64,
        name: &str,
        data: &[u8],
        blocks: &[u64],
    ) -> u64 {
        let id = self.add(tree, parent, name, 0o100644, 1);
        let mut extents: Vec<(u64, u64, u64)> = Vec::new();
        for (i, &block) in blocks.iter().enumerate() {
            let offset = i as u64 * BLOCK;
            if block == 0 {
                continue;
            }
            let piece = data
                .get(i * BLOCK_BYTES..data.len().min((i + 1) * BLOCK_BYTES))
                .unwrap_or(&[]);
            put(&mut self.image, Self::data_physical(block), piece);
            match extents.last_mut() {
                Some((o, d, len)) if *o + *len == offset && *d + *len == block => *len += BLOCK,
                _ => extents.push((offset, block, BLOCK)),
            }
        }
        let inode = self.inode(tree, id);
        inode.size = data.len() as u64;
        inode.extents = extents
            .into_iter()
            .map(|(offset, disk, length)| ExtentSpec::Regular {
                offset,
                disk,
                length,
                preallocated: false,
                compression: 0,
            })
            .collect();
        id
    }

    /// A file whose space is allocated but never written: zeros.
    pub fn preallocated(&mut self, tree: u64, parent: u64, name: &str, size: usize) -> u64 {
        let blocks = self.alloc(size.div_ceil(BLOCK_BYTES));
        for &block in &blocks {
            put(
                &mut self.image,
                Self::data_physical(block),
                &[0xEE; BLOCK_BYTES],
            );
        }
        let id = self.add(tree, parent, name, 0o100644, 1);
        let inode = self.inode(tree, id);
        inode.size = size as u64;
        inode.extents.push(ExtentSpec::Regular {
            offset: 0,
            disk: blocks[0],
            length: blocks.len() as u64 * BLOCK,
            preallocated: true,
            compression: 0,
        });
        id
    }

    /// A file stored compressed (zlib): `size` bytes once uncompressed.
    pub fn compressed(&mut self, tree: u64, parent: u64, name: &str, size: u64) -> u64 {
        let block = self.alloc(1)[0];
        let id = self.add(tree, parent, name, 0o100644, 1);
        let inode = self.inode(tree, id);
        inode.size = size;
        inode.extents.push(ExtentSpec::Regular {
            offset: 0,
            disk: block,
            length: size.next_multiple_of(BLOCK),
            preallocated: false,
            compression: 1,
        });
        id
    }

    pub fn symlink(&mut self, tree: u64, parent: u64, name: &str, target: &str) -> u64 {
        let id = self.add(tree, parent, name, 0o120777, 7);
        let inode = self.inode(tree, id);
        inode.size = target.len() as u64;
        inode.extents.push(ExtentSpec::Inline {
            data: target.as_bytes().to_vec(),
            compression: 0,
        });
        id
    }

    pub fn hard_link(&mut self, tree: u64, id: u64, parent: u64, name: &str) {
        let spec = self.trees.get_mut(&tree).unwrap();
        spec.inodes.get_mut(&id).unwrap().links += 1;
        spec.entries
            .push((parent, name.to_owned(), Target::Inode(id), 1));
    }

    /// A new subvolume, as a folder of `parent`; returns its id.
    pub fn subvolume(&mut self, tree: u64, parent: u64, name: &str) -> u64 {
        let id = 256 + self.trees.keys().filter(|&&t| t >= 256).count() as u64;
        self.trees.insert(id, Self::new_tree(self.clock));
        self.trees.get_mut(&tree).unwrap().entries.push((
            parent,
            name.to_owned(),
            Target::Subvolume(id),
            2,
        ));
        id
    }

    fn items(&self, spec: &TreeSpec) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut items = Vec::new();
        for (&id, inode) in &spec.inodes {
            let mut value = vec![0u8; 160];
            value[0..8].copy_from_slice(&10u64.to_le_bytes());
            value[16..24].copy_from_slice(&inode.size.to_le_bytes());
            value[40..44].copy_from_slice(&inode.links.to_le_bytes());
            value[52..56].copy_from_slice(&inode.mode.to_le_bytes());
            for at in [112, 124, 136] {
                value[at..at + 8].copy_from_slice(&inode.time.to_le_bytes());
            }
            // Created a day before it was last changed.
            value[148..156].copy_from_slice(&(inode.time - 86_400).to_le_bytes());
            items.push((key(id, 1, 0), value));
            for extent in &inode.extents {
                let mut value = vec![0u8; 21];
                value[0..8].copy_from_slice(&10u64.to_le_bytes());
                let file_offset = match extent {
                    ExtentSpec::Inline { data, compression } => {
                        value[8..16].copy_from_slice(&(data.len() as u64).to_le_bytes());
                        value[16] = *compression;
                        value.extend_from_slice(data);
                        0
                    }
                    ExtentSpec::Regular {
                        offset: o,
                        disk,
                        length,
                        preallocated,
                        compression,
                    } => {
                        value[8..16].copy_from_slice(&length.to_le_bytes());
                        value[16] = *compression;
                        value[20] = if *preallocated { 2 } else { 1 };
                        value.extend_from_slice(&disk.to_le_bytes());
                        value.extend_from_slice(&length.to_le_bytes());
                        value.extend_from_slice(&0u64.to_le_bytes());
                        value.extend_from_slice(&length.to_le_bytes());
                        *o
                    }
                };
                items.push((key(id, 108, file_offset), value));
            }
        }
        // ".." of the root folder.
        let mut back = 0u64.to_le_bytes().to_vec();
        back.extend_from_slice(&2u16.to_le_bytes());
        back.extend_from_slice(b"..");
        items.push((key(BTRFS_ROOT_DIR, 12, BTRFS_ROOT_DIR), back));
        let mut index: HashMap<u64, u64> = HashMap::new();
        for (dir, name, target, kind) in &spec.entries {
            let location = match target {
                Target::Inode(id) => key(*id, 1, 0),
                Target::Subvolume(id) => key(*id, 132, u64::MAX),
            };
            let mut value = location;
            value.extend_from_slice(&10u64.to_le_bytes());
            value.extend_from_slice(&0u16.to_le_bytes());
            value.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
            value.push(*kind);
            value.extend_from_slice(name.as_bytes());
            let hash = u64::from(crc32c(name.as_bytes()));
            items.push((key(*dir, 84, hash), value.clone()));
            let next = index.entry(*dir).or_insert(2);
            items.push((key(*dir, 96, *next), value));
            if let Target::Inode(id) = target {
                let mut reference = next.to_le_bytes().to_vec();
                reference.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
                reference.extend_from_slice(name.as_bytes());
                items.push((key(*id, 12, *dir), reference));
            }
            *next += 1;
        }
        items.sort_by(|a, b| {
            let order = |k: &[u8]| {
                (
                    u64::from_le_bytes(k[0..8].try_into().unwrap()),
                    k[8],
                    u64::from_le_bytes(k[9..17].try_into().unwrap()),
                )
            };
            order(&a.0).cmp(&order(&b.0))
        });
        items
    }

    /// Writes a node at a logical address (system or metadata chunk).
    fn write_node(&mut self, logical: u64, owner: u64, level: u8, count: usize, body: &[u8]) {
        let mut node = vec![0u8; NODE];
        node[32..48].copy_from_slice(&[0xF5; 16]);
        node[48..56].copy_from_slice(&logical.to_le_bytes());
        node[56..64].copy_from_slice(&(1u64 | (1 << 56)).to_le_bytes());
        node[80..88].copy_from_slice(&10u64.to_le_bytes());
        node[88..96].copy_from_slice(&owner.to_le_bytes());
        node[96..100].copy_from_slice(&u32::try_from(count).unwrap().to_le_bytes());
        node[100] = level;
        node[101..101 + body.len()].copy_from_slice(body);
        let sum = crc32c(&node[0x20..]);
        node[0..4].copy_from_slice(&sum.to_le_bytes());
        let physical = if (SYSTEM.0..SYSTEM.0 + SYSTEM.2).contains(&logical) {
            SYSTEM.1 + (logical - SYSTEM.0)
        } else {
            METADATA.1 + (logical - METADATA.0)
        };
        put(&mut self.image, usize::try_from(physical).unwrap(), &node);
    }

    fn next_node(&mut self, system: bool) -> u64 {
        let counter = if system {
            &mut self.next_system
        } else {
            &mut self.next_meta
        };
        let at = *counter;
        *counter += NODE as u64;
        at
    }

    /// Writes a tree of `items`; returns its root's logical address and level.
    fn write_tree(&mut self, owner: u64, items: &[(Vec<u8>, Vec<u8>)], system: bool) -> (u64, u8) {
        let mut leaves: Vec<Vec<(Vec<u8>, Vec<u8>)>> = vec![Vec::new()];
        let mut used = 0;
        for item in items {
            let size = 25 + item.1.len();
            if used + size > NODE - 101 {
                leaves.push(Vec::new());
                used = 0;
            }
            used += size;
            leaves.last_mut().unwrap().push(item.clone());
        }
        let mut written = Vec::new();
        for leaf in &leaves {
            let logical = self.next_node(system);
            let mut body = vec![0u8; NODE - 101];
            let mut data_end = NODE - 101;
            for (i, (k, value)) in leaf.iter().enumerate() {
                data_end -= value.len();
                body[data_end..data_end + value.len()].copy_from_slice(value);
                let at = i * 25;
                body[at..at + 17].copy_from_slice(k);
                body[at + 17..at + 21]
                    .copy_from_slice(&u32::try_from(data_end).unwrap().to_le_bytes());
                body[at + 21..at + 25]
                    .copy_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
            }
            self.write_node(logical, owner, 0, leaf.len(), &body);
            written.push((leaf[0].0.clone(), logical));
        }
        if written.len() == 1 {
            return (written[0].1, 0);
        }
        let logical = self.next_node(system);
        let mut body = Vec::new();
        for (first, child) in &written {
            body.extend_from_slice(first);
            body.extend_from_slice(&child.to_le_bytes());
            body.extend_from_slice(&10u64.to_le_bytes());
        }
        self.write_node(logical, owner, 1, written.len(), &body);
        (logical, 1)
    }

    fn chunk_item(logical_physical_length: (u64, u64, u64), kind: u64, copies: &[u64]) -> Vec<u8> {
        let (_, _, length) = logical_physical_length;
        let mut chunk = vec![0u8; 48];
        chunk[0..8].copy_from_slice(&length.to_le_bytes());
        chunk[8..16].copy_from_slice(&2u64.to_le_bytes());
        chunk[16..24].copy_from_slice(&0x1_0000u64.to_le_bytes());
        chunk[24..32].copy_from_slice(&kind.to_le_bytes());
        chunk[32..36].copy_from_slice(&4096u32.to_le_bytes());
        chunk[36..40].copy_from_slice(&4096u32.to_le_bytes());
        chunk[40..44].copy_from_slice(&4096u32.to_le_bytes());
        chunk[44..46].copy_from_slice(&u16::try_from(copies.len()).unwrap().to_le_bytes());
        for &physical in copies {
            let mut stripe = vec![0u8; 32];
            stripe[0..8].copy_from_slice(&1u64.to_le_bytes());
            stripe[8..16].copy_from_slice(&physical.to_le_bytes());
            chunk.extend(stripe);
        }
        chunk
    }

    pub fn build(mut self) -> Vec<u8> {
        let size = self.image.len() as u64;
        let data = (DATA_LOGICAL, DATA_PHYSICAL, self.data_end - DATA_LOGICAL);
        // Subvolume trees, then the root tree pointing at them.
        let mut roots = Vec::new();
        let ids: Vec<u64> = self.trees.keys().copied().collect();
        for id in ids {
            let spec = self.trees[&id].clone();
            let items = self.items(&spec);
            let (root, level) = self.write_tree(id, &items, false);
            roots.push((id, root, level));
        }
        let mut root_items = Vec::new();
        for &(id, root, level) in &roots {
            let mut value = vec![0u8; 439];
            value[0..8].copy_from_slice(&1u64.to_le_bytes());
            value[52..56].copy_from_slice(&0o40755u32.to_le_bytes());
            value[160..168].copy_from_slice(&10u64.to_le_bytes());
            value[168..176].copy_from_slice(&BTRFS_ROOT_DIR.to_le_bytes());
            value[176..184].copy_from_slice(&root.to_le_bytes());
            value[216..220].copy_from_slice(&1u32.to_le_bytes());
            value[238] = level;
            // Changed, then created a day before.
            value[327..335].copy_from_slice(&self.clock.to_le_bytes());
            value[339..347].copy_from_slice(&(self.clock - 86_400).to_le_bytes());
            root_items.push((key(id, 132, 0), value));
        }
        let (root_tree, root_level) = self.write_tree(1, &root_items, false);
        // The chunk tree, in the system chunk.
        let metadata_copies = [METADATA.1, 60 * MIB];
        let chunks = vec![
            (
                key(256, 228, SYSTEM.0),
                Self::chunk_item(SYSTEM, 2, &[SYSTEM.1]),
            ),
            (
                key(256, 228, METADATA.0),
                Self::chunk_item(METADATA, 4 | 0x20, &metadata_copies),
            ),
            (key(256, 228, data.0), Self::chunk_item(data, 1, &[data.1])),
        ];
        let (chunk_root, chunk_level) = self.write_tree(3, &chunks, true);
        let mut array = key(256, 228, SYSTEM.0);
        array.extend(Self::chunk_item(SYSTEM, 2, &[SYSTEM.1]));
        let superblock = |bytenr: u64, label: &str| {
            let mut sb = vec![0u8; 4096];
            sb[0x20..0x30].copy_from_slice(&[0xF5; 16]);
            sb[0x30..0x38].copy_from_slice(&bytenr.to_le_bytes());
            sb[0x40..0x48].copy_from_slice(b"_BHRfS_M");
            sb[0x48..0x50].copy_from_slice(&10u64.to_le_bytes());
            sb[0x50..0x58].copy_from_slice(&root_tree.to_le_bytes());
            sb[0x58..0x60].copy_from_slice(&chunk_root.to_le_bytes());
            sb[0x70..0x78].copy_from_slice(&size.to_le_bytes());
            sb[0x80..0x88].copy_from_slice(&6u64.to_le_bytes());
            sb[0x88..0x90].copy_from_slice(&1u64.to_le_bytes());
            for at in [0x90, 0x94, 0x98, 0x9C] {
                sb[at..at + 4].copy_from_slice(&4096u32.to_le_bytes());
            }
            sb[0xA0..0xA4].copy_from_slice(&u32::try_from(array.len()).unwrap().to_le_bytes());
            sb[0xC6] = root_level;
            sb[0xC7] = chunk_level;
            sb[0xC9..0xD1].copy_from_slice(&1u64.to_le_bytes());
            sb[0x12B..0x12B + label.len()].copy_from_slice(label.as_bytes());
            sb[0x32B..0x32B + array.len()].copy_from_slice(&array);
            let sum = crc32c(&sb[0x20..]);
            sb[0..4].copy_from_slice(&sum.to_le_bytes());
            sb
        };
        let label = self.label.clone();
        put(&mut self.image, 0x1_0000, &superblock(0x1_0000, &label));
        if size >= 64 * MIB + 4096 {
            put(&mut self.image, 0x400_0000, &superblock(0x400_0000, &label));
        }
        self.image
    }
}
