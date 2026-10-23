//! Small APFS containers, laid out as Apple's APFS reference describes:
//! container superblock (an old copy in block 0, newer ones in the
//! checkpoint area), container and volume object maps, volume superblocks,
//! and file-system B-trees (inodes with extended fields, folder records,
//! file extents, extended attributes).

use std::collections::BTreeMap;

use super::{STAMP, put};
use crate::apfs::fletcher64;

/// A volume's root folder.
pub const APFS_ROOT: u64 = 2;
const PRIVATE_DIR: u64 = 3;
const BLOCK: usize = 4096;

const OBJ_PHYSICAL: u32 = 0x4000_0000;
const OBJ_EPHEMERAL: u32 = 0x8000_0000;
const TYPE_NX_SUPERBLOCK: u32 = 0x1;
const TYPE_BTREE: u32 = 0x2;
const TYPE_BTREE_NODE: u32 = 0x3;
const TYPE_OMAP: u32 = 0xB;
const TYPE_CHECKPOINT_MAP: u32 = 0xC;
const TYPE_FS: u32 = 0xD;
const TYPE_FSTREE: u32 = 0xE;

#[derive(Clone, Debug)]
enum Rsrc {
    Embedded(Vec<u8>),
    Stream {
        id: u64,
        size: u64,
        extents: Vec<(u64, u64, u64)>,
    },
}

#[derive(Clone, Debug)]
struct InodeSpec {
    parent: u64,
    name: String,
    mode: u16,
    size: u64,
    flags: u32,
    /// (logical byte, length, first block or 0 for a hole).
    extents: Vec<(u64, u64, u64)>,
    time: u64,
    symlink: Option<String>,
    rsrc: Option<Rsrc>,
}

#[derive(Clone, Debug)]
struct VolumeSpec {
    name: String,
    role: u16,
    encrypted: bool,
    inodes: BTreeMap<u64, InodeSpec>,
    /// (parent, name, inode, type).
    entries: Vec<(u64, String, u64, u16)>,
    next_id: u64,
}

/// Builds an APFS container in memory.
pub struct ApfsBuilder {
    image: Vec<u8>,
    next_block: u64,
    volumes: Vec<VolumeSpec>,
    clock: u64,
    xid: u64,
}

fn put_obj(block: &mut [u8], oid: u64, xid: u64, kind: u32, subtype: u32) {
    block[8..16].copy_from_slice(&oid.to_le_bytes());
    block[16..24].copy_from_slice(&xid.to_le_bytes());
    block[24..28].copy_from_slice(&kind.to_le_bytes());
    block[28..32].copy_from_slice(&subtype.to_le_bytes());
}

/// The byte offset of a block.
fn at(block: u64) -> usize {
    usize::try_from(block).unwrap() * BLOCK
}

fn seal(block: &mut [u8]) {
    let sum = fletcher64(block);
    block[0..8].copy_from_slice(&sum.to_le_bytes());
}

impl ApfsBuilder {
    pub fn new(blocks: usize) -> Self {
        Self {
            image: vec![0u8; blocks * BLOCK],
            // Block 0 and the checkpoint area (blocks 1-4).
            next_block: 5,
            volumes: Vec::new(),
            clock: u64::try_from(STAMP).unwrap() * 1_000_000_000,
            xid: 20,
        }
    }

    /// Adds a volume; `role` as APFS numbers them (0x40 Data, 0x1 System, …).
    pub fn volume(&mut self, name: &str, role: u16, encrypted: bool) -> usize {
        let mut inodes = BTreeMap::new();
        for (id, inode_name) in [(APFS_ROOT, "root"), (PRIVATE_DIR, "private-dir")] {
            inodes.insert(
                id,
                InodeSpec {
                    parent: 1,
                    name: inode_name.to_owned(),
                    mode: 0x41ED,
                    size: 0,
                    flags: 0,
                    extents: Vec::new(),
                    time: self.clock,
                    symlink: None,
                    rsrc: None,
                },
            );
        }
        self.volumes.push(VolumeSpec {
            name: name.to_owned(),
            role,
            encrypted,
            inodes,
            entries: Vec::new(),
            next_id: 16,
        });
        self.volumes.len() - 1
    }

    pub fn alloc(&mut self, count: usize) -> Vec<u64> {
        let start = self.next_block;
        self.next_block += count as u64;
        assert!(
            at(self.next_block) < self.image.len(),
            "test container is full"
        );
        (start..self.next_block).collect()
    }

    fn tick(&mut self) -> u64 {
        self.clock += 60_000_000_000;
        self.clock
    }

    fn add(&mut self, v: usize, parent: u64, name: &str, mode: u16, kind: u16) -> u64 {
        let time = self.tick();
        let volume = &mut self.volumes[v];
        let id = volume.next_id;
        volume.next_id += 1;
        volume.inodes.insert(
            id,
            InodeSpec {
                parent,
                name: name.to_owned(),
                mode,
                size: 0,
                flags: 0,
                extents: Vec::new(),
                time,
                symlink: None,
                rsrc: None,
            },
        );
        volume.entries.push((parent, name.to_owned(), id, kind));
        id
    }

    pub fn mkdir(&mut self, v: usize, parent: u64, name: &str) -> u64 {
        self.add(v, parent, name, 0x41ED, 4)
    }

    pub fn file(&mut self, v: usize, parent: u64, name: &str, data: &[u8]) -> u64 {
        let blocks = self.alloc(data.len().div_ceil(BLOCK));
        self.file_in(v, parent, name, data, &blocks)
    }

    fn write_data(&mut self, data: &[u8], blocks: &[u64]) -> Vec<(u64, u64, u64)> {
        let mut extents: Vec<(u64, u64, u64)> = Vec::new();
        for (i, &block) in blocks.iter().enumerate() {
            if block != 0 {
                let piece = data
                    .get(i * BLOCK..data.len().min((i + 1) * BLOCK))
                    .unwrap_or(&[]);
                put(&mut self.image, at(block), piece);
            }
            let logical = (i * BLOCK) as u64;
            match extents.last_mut() {
                Some((l, len, b))
                    if *l + *len == logical
                        && ((*b == 0 && block == 0)
                            || (*b != 0 && *b + *len / BLOCK as u64 == block)) =>
                {
                    *len += BLOCK as u64;
                }
                _ => extents.push((logical, BLOCK as u64, block)),
            }
        }
        extents
    }

    /// A file in the given blocks (0 for a hole, which reads as zeros).
    pub fn file_in(
        &mut self,
        v: usize,
        parent: u64,
        name: &str,
        data: &[u8],
        blocks: &[u64],
    ) -> u64 {
        let id = self.add(v, parent, name, 0x81A4, 8);
        let extents = self.write_data(data, blocks);
        let inode = self.volumes[v].inodes.get_mut(&id).unwrap();
        inode.size = data.len() as u64;
        inode.extents = extents;
        id
    }

    pub fn symlink(&mut self, v: usize, parent: u64, name: &str, target: &str) -> u64 {
        let id = self.add(v, parent, name, 0xA1ED, 10);
        self.volumes[v].inodes.get_mut(&id).unwrap().symlink = Some(target.to_owned());
        id
    }

    /// A resource fork: kept in the record when small, else in its own blocks.
    pub fn resource(&mut self, v: usize, id: u64, data: &[u8]) {
        let rsrc = if data.len() <= 3804 {
            Rsrc::Embedded(data.to_vec())
        } else {
            let blocks = self.alloc(data.len().div_ceil(BLOCK));
            let extents = self.write_data(data, &blocks);
            let stream = self.volumes[v].next_id;
            self.volumes[v].next_id += 1;
            Rsrc::Stream {
                id: stream,
                size: data.len() as u64,
                extents,
            }
        };
        self.volumes[v].inodes.get_mut(&id).unwrap().rsrc = Some(rsrc);
    }

    pub fn hidden(&mut self, v: usize, id: u64) {
        self.volumes[v].inodes.get_mut(&id).unwrap().flags |= 0x8000;
    }

    /// A second name for a file.
    pub fn hard_link(&mut self, v: usize, id: u64, parent: u64, name: &str) {
        self.volumes[v]
            .entries
            .push((parent, name.to_owned(), id, 8));
    }

    /// The file-system tree's records, keys and values.
    fn records(&self, v: &VolumeSpec) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut records = Vec::new();
        let key = |id: u64, kind: u64| ((kind << 60) | id).to_le_bytes().to_vec();
        for (&id, inode) in &v.inodes {
            let mut value = vec![0u8; 92];
            value[0..8].copy_from_slice(&inode.parent.to_le_bytes());
            value[8..16].copy_from_slice(&id.to_le_bytes());
            value[16..24].copy_from_slice(&(inode.time - 86_400_000_000_000).to_le_bytes());
            for at in [24, 32, 40] {
                value[at..at + 8].copy_from_slice(&inode.time.to_le_bytes());
            }
            value[56..60].copy_from_slice(&1u32.to_le_bytes());
            value[68..72].copy_from_slice(&inode.flags.to_le_bytes());
            value[80..82].copy_from_slice(&inode.mode.to_le_bytes());
            // Extended fields: the name, and the data stream for files.
            let mut name = inode.name.as_bytes().to_vec();
            name.push(0);
            let mut fields: Vec<(u8, Vec<u8>)> = vec![(4, name)];
            if inode.mode & 0xF000 == 0x8000 {
                let mut stream = vec![0u8; 40];
                stream[0..8].copy_from_slice(&inode.size.to_le_bytes());
                stream[8..16]
                    .copy_from_slice(&inode.size.next_multiple_of(BLOCK as u64).to_le_bytes());
                fields.push((8, stream));
            }
            let used: usize = fields
                .iter()
                .map(|(_, d)| d.len().next_multiple_of(8))
                .sum();
            value.extend_from_slice(&u16::try_from(fields.len()).unwrap().to_le_bytes());
            value.extend_from_slice(&u16::try_from(used).unwrap().to_le_bytes());
            for (kind, data) in &fields {
                value.extend_from_slice(&[*kind, 0]);
                value.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
            }
            for (_, data) in &fields {
                let mut padded = data.clone();
                padded.resize(data.len().next_multiple_of(8), 0);
                value.extend(padded);
            }
            records.push((key(id, 3), value));
            for &(logical, length, block) in &inode.extents {
                let mut k = key(id, 8);
                k.extend_from_slice(&logical.to_le_bytes());
                let mut value = vec![0u8; 24];
                value[0..8].copy_from_slice(&length.to_le_bytes());
                value[8..16].copy_from_slice(&block.to_le_bytes());
                records.push((k, value));
            }
            let mut xattr = |name: &str, flags: u16, data: Vec<u8>| {
                let mut k = key(id, 4);
                k.extend_from_slice(&u16::try_from(name.len() + 1).unwrap().to_le_bytes());
                k.extend_from_slice(name.as_bytes());
                k.push(0);
                let mut value = flags.to_le_bytes().to_vec();
                value.extend_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
                value.extend(data);
                records.push((k, value));
            };
            if let Some(target) = &inode.symlink {
                let mut data = target.as_bytes().to_vec();
                data.push(0);
                xattr("com.apple.fs.symlink", 2, data);
            }
            match &inode.rsrc {
                Some(Rsrc::Embedded(data)) => xattr("com.apple.ResourceFork", 2, data.clone()),
                Some(Rsrc::Stream {
                    id: stream,
                    size,
                    extents,
                }) => {
                    let mut data = vec![0u8; 48];
                    data[0..8].copy_from_slice(&stream.to_le_bytes());
                    data[8..16].copy_from_slice(&size.to_le_bytes());
                    xattr("com.apple.ResourceFork", 1, data);
                    for &(logical, length, block) in extents {
                        let mut k = key(*stream, 8);
                        k.extend_from_slice(&logical.to_le_bytes());
                        let mut value = vec![0u8; 24];
                        value[0..8].copy_from_slice(&length.to_le_bytes());
                        value[8..16].copy_from_slice(&block.to_le_bytes());
                        records.push((k, value));
                    }
                }
                None => {}
            }
        }
        for (parent, name, id, kind) in &v.entries {
            // Hashed key: the name's length with a hash (not checked by readers here).
            let mut k = key(*parent, 9);
            let length = u32::try_from(name.len() + 1).unwrap();
            k.extend_from_slice(&(length | (0x1234 << 10)).to_le_bytes());
            k.extend_from_slice(name.as_bytes());
            k.push(0);
            let mut value = vec![0u8; 18];
            value[0..8].copy_from_slice(&id.to_le_bytes());
            value[16..18].copy_from_slice(&kind.to_le_bytes());
            records.push((k, value));
        }
        records.sort_by(|a, b| {
            let order = |k: &[u8]| {
                let raw = u64::from_le_bytes(k[0..8].try_into().unwrap());
                (raw & 0x0FFF_FFFF_FFFF_FFFF, raw >> 60, k[8..].to_vec())
            };
            order(&a.0).cmp(&order(&b.0))
        });
        records
    }

    /// A B-tree node with variable-size keys and values.
    #[allow(clippy::too_many_arguments)]
    fn node(
        oid: u64,
        xid: u64,
        storage: u32,
        subtype: u32,
        level: u16,
        root: bool,
        entries: &[(Vec<u8>, Vec<u8>)],
        fixed: Option<(usize, usize)>,
        key_count: u64,
        node_count: u64,
    ) -> Vec<u8> {
        let mut node = vec![0u8; BLOCK];
        let kind = if root { TYPE_BTREE } else { TYPE_BTREE_NODE };
        put_obj(&mut node, oid, xid, kind | storage, subtype);
        let mut flags = if root { 1u16 } else { 0 };
        if level == 0 {
            flags |= 2;
        }
        if fixed.is_some() {
            flags |= 4;
        }
        node[32..34].copy_from_slice(&flags.to_le_bytes());
        node[34..36].copy_from_slice(&level.to_le_bytes());
        node[36..40].copy_from_slice(&u32::try_from(entries.len()).unwrap().to_le_bytes());
        let toc_entry = if fixed.is_some() { 4 } else { 8 };
        let toc_len = entries.len() * toc_entry;
        node[42..44].copy_from_slice(&u16::try_from(toc_len).unwrap().to_le_bytes());
        let keys = 56 + toc_len;
        let values_end = if root { BLOCK - 40 } else { BLOCK };
        let (mut key_at, mut value_back) = (0usize, 0usize);
        for (i, (key, value)) in entries.iter().enumerate() {
            node[keys + key_at..keys + key_at + key.len()].copy_from_slice(key);
            value_back += value.len();
            let value_start = values_end - value_back;
            assert!(keys + key_at + key.len() <= value_start, "node overflow");
            node[value_start..value_start + value.len()].copy_from_slice(value);
            let toc = 56 + i * toc_entry;
            let k = u16::try_from(key_at).unwrap().to_le_bytes();
            let v = u16::try_from(value_back).unwrap().to_le_bytes();
            if fixed.is_some() {
                node[toc..toc + 2].copy_from_slice(&k);
                node[toc + 2..toc + 4].copy_from_slice(&v);
            } else {
                node[toc..toc + 2].copy_from_slice(&k);
                node[toc + 2..toc + 4]
                    .copy_from_slice(&u16::try_from(key.len()).unwrap().to_le_bytes());
                node[toc + 4..toc + 6].copy_from_slice(&v);
                node[toc + 6..toc + 8]
                    .copy_from_slice(&u16::try_from(value.len()).unwrap().to_le_bytes());
            }
            key_at += key.len();
        }
        if root {
            let info = BLOCK - 40;
            let (key_size, value_size) = fixed.unwrap_or((0, 0));
            node[info + 4..info + 8].copy_from_slice(&u32::try_from(BLOCK).unwrap().to_le_bytes());
            node[info + 8..info + 12]
                .copy_from_slice(&u32::try_from(key_size).unwrap().to_le_bytes());
            node[info + 12..info + 16]
                .copy_from_slice(&u32::try_from(value_size).unwrap().to_le_bytes());
            node[info + 24..info + 32].copy_from_slice(&key_count.to_le_bytes());
            node[info + 32..info + 40].copy_from_slice(&node_count.to_le_bytes());
        }
        seal(&mut node);
        node
    }

    /// Writes an object map (header and a one-node tree) for `mappings`
    /// (virtual oid → block); returns the header's block.
    fn object_map(&mut self, mappings: &[(u64, u64)]) -> u64 {
        let [header_block, tree_block] = self.alloc(2)[..] else {
            unreachable!()
        };
        let entries: Vec<(Vec<u8>, Vec<u8>)> = mappings
            .iter()
            .map(|&(oid, block)| {
                let mut key = oid.to_le_bytes().to_vec();
                key.extend_from_slice(&(self.xid - 1).to_le_bytes());
                let mut value = vec![0u8; 16];
                value[4..8].copy_from_slice(&u32::try_from(BLOCK).unwrap().to_le_bytes());
                value[8..16].copy_from_slice(&block.to_le_bytes());
                (key, value)
            })
            .collect();
        let tree = Self::node(
            tree_block,
            self.xid,
            OBJ_PHYSICAL,
            TYPE_OMAP,
            0,
            true,
            &entries,
            Some((16, 16)),
            entries.len() as u64,
            1,
        );
        put(&mut self.image, at(tree_block), &tree);
        let mut header = vec![0u8; BLOCK];
        put_obj(
            &mut header,
            header_block,
            self.xid,
            TYPE_OMAP | OBJ_PHYSICAL,
            0,
        );
        header[40..44].copy_from_slice(&(TYPE_BTREE | OBJ_PHYSICAL).to_le_bytes());
        header[48..56].copy_from_slice(&tree_block.to_le_bytes());
        seal(&mut header);
        put(&mut self.image, at(header_block), &header);
        header_block
    }

    /// Writes a volume's file-system tree, object map and superblock;
    /// returns the superblock's block.
    fn write_volume(&mut self, index: usize, oid: u64) -> u64 {
        let spec = self.volumes[index].clone();
        let records = self.records(&spec);
        // Leaves of up to ~3 KiB of records each, then an index root.
        let mut leaves: Vec<Vec<(Vec<u8>, Vec<u8>)>> = vec![Vec::new()];
        let mut used = 0;
        for record in records.iter().cloned() {
            let size = record.0.len() + record.1.len() + 8;
            if used + size > 3000 {
                leaves.push(Vec::new());
                used = 0;
            }
            used += size;
            leaves.last_mut().unwrap().push(record);
        }
        let first_oid = 2000;
        let mut mappings = Vec::new();
        let node_count = leaves.len() as u64 + u64::from(leaves.len() > 1);
        let root_oid;
        if leaves.len() == 1 {
            root_oid = first_oid;
            let block = self.alloc(1)[0];
            let node = Self::node(
                root_oid,
                self.xid,
                0,
                TYPE_FSTREE,
                0,
                true,
                &leaves[0],
                None,
                records.len() as u64,
                1,
            );
            put(&mut self.image, at(block), &node);
            mappings.push((root_oid, block));
        } else {
            let mut index_entries = Vec::new();
            for (i, leaf) in leaves.iter().enumerate() {
                let leaf_oid = first_oid + 1 + i as u64;
                let block = self.alloc(1)[0];
                let node = Self::node(
                    leaf_oid,
                    self.xid,
                    0,
                    TYPE_FSTREE,
                    0,
                    false,
                    leaf,
                    None,
                    0,
                    0,
                );
                put(&mut self.image, at(block), &node);
                mappings.push((leaf_oid, block));
                index_entries.push((leaf[0].0.clone(), leaf_oid.to_le_bytes().to_vec()));
            }
            root_oid = first_oid;
            let block = self.alloc(1)[0];
            let node = Self::node(
                root_oid,
                self.xid,
                0,
                TYPE_FSTREE,
                1,
                true,
                &index_entries,
                None,
                records.len() as u64,
                node_count,
            );
            put(&mut self.image, at(block), &node);
            mappings.push((root_oid, block));
        }
        let omap = self.object_map(&mappings);
        let block = self.alloc(1)[0];
        let mut sb = vec![0u8; BLOCK];
        put_obj(&mut sb, oid, self.xid, TYPE_FS, 0);
        sb[32..36].copy_from_slice(b"APSB");
        sb[36..40].copy_from_slice(&u32::try_from(index).unwrap().to_le_bytes());
        // Normalization-insensitive: hashed folder records.
        sb[56..64].copy_from_slice(&0x8u64.to_le_bytes());
        sb[116..120].copy_from_slice(&TYPE_BTREE.to_le_bytes());
        sb[128..136].copy_from_slice(&omap.to_le_bytes());
        sb[136..144].copy_from_slice(&root_oid.to_le_bytes());
        let flags: u64 = if spec.encrypted { 0 } else { 1 };
        sb[264..272].copy_from_slice(&flags.to_le_bytes());
        sb[704..704 + spec.name.len()].copy_from_slice(spec.name.as_bytes());
        sb[964..966].copy_from_slice(&spec.role.to_le_bytes());
        seal(&mut sb);
        put(&mut self.image, at(block), &sb);
        block
    }

    fn superblock(&self, xid: u64, omap: u64, volumes: &[u64]) -> Vec<u8> {
        let mut sb = vec![0u8; BLOCK];
        put_obj(&mut sb, 1, xid, TYPE_NX_SUPERBLOCK | OBJ_EPHEMERAL, 0);
        sb[32..36].copy_from_slice(b"NXSB");
        sb[36..40].copy_from_slice(&u32::try_from(BLOCK).unwrap().to_le_bytes());
        sb[40..48].copy_from_slice(&((self.image.len() / BLOCK) as u64).to_le_bytes());
        sb[96..104].copy_from_slice(&(xid + 1).to_le_bytes());
        sb[104..108].copy_from_slice(&4u32.to_le_bytes());
        sb[112..120].copy_from_slice(&1u64.to_le_bytes());
        sb[160..168].copy_from_slice(&omap.to_le_bytes());
        sb[180..184].copy_from_slice(&100u32.to_le_bytes());
        for (i, &oid) in volumes.iter().enumerate() {
            sb[184 + i * 8..192 + i * 8].copy_from_slice(&oid.to_le_bytes());
        }
        seal(&mut sb);
        sb
    }

    pub fn build(mut self) -> Vec<u8> {
        let mut volume_oids = Vec::new();
        let mut mappings = Vec::new();
        for index in 0..self.volumes.len() {
            let oid = 1026 + index as u64;
            let block = self.write_volume(index, oid);
            volume_oids.push(oid);
            mappings.push((oid, block));
        }
        let omap = self.object_map(&mappings);
        // Block 0: an older copy that only knew the first volume.
        let old = self.superblock(self.xid - 5, omap, &volume_oids[..volume_oids.len().min(1)]);
        put(&mut self.image, 0, &old);
        // The checkpoint area: a map, the newest superblock, and a newer one
        // that never finished writing (its checksum is wrong).
        let mut map = vec![0u8; BLOCK];
        put_obj(&mut map, 1, self.xid, TYPE_CHECKPOINT_MAP | OBJ_PHYSICAL, 0);
        seal(&mut map);
        put(&mut self.image, BLOCK, &map);
        let newest = self.superblock(self.xid, omap, &volume_oids);
        put(&mut self.image, 2 * BLOCK, &newest);
        let mut torn = self.superblock(self.xid + 1, omap, &[]);
        torn[0] ^= 0xFF;
        put(&mut self.image, 3 * BLOCK, &torn);
        self.image
    }
}
