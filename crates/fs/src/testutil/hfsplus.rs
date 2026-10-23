//! Small HFS+ volumes, laid out the way newfs_hfs and the drivers write
//! them: volume header and its copy at the end, a catalog B-tree (header,
//! leaf and index nodes, file, folder and thread records), the extents
//! overflow B-tree, resource forks, hard links through the private data
//! folder, and deleted records left over in free space.

use std::collections::BTreeMap;

use super::{STAMP, put};

pub const HFS_ROOT: u32 = 2;
const HFS_EPOCH: i64 = 2_082_844_800;
const PRIVATE_FILES: &str = "\0\0\0\0HFS+ Private Data";

/// How to lay a volume out.
#[derive(Clone, Debug)]
pub struct HfsOptions {
    pub block_size: usize,
    pub blocks: usize,
    pub node_size: usize,
    /// HFSX (case-sensitive) instead of HFS+.
    pub hfsx: bool,
    /// Catalog nodes set aside.
    pub catalog_nodes: usize,
}

impl Default for HfsOptions {
    fn default() -> Self {
        Self {
            block_size: 4096,
            blocks: 2048,
            node_size: 4096,
            hfsx: false,
            catalog_nodes: 32,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ForkSpec {
    size: u64,
    total_blocks: u32,
    extents: Vec<(u32, u32)>,
}

#[derive(Clone, Debug)]
struct Rec {
    parent: u32,
    id: u32,
    name: Vec<u16>,
    folder: bool,
    data: ForkSpec,
    resource: ForkSpec,
    file_type: [u8; 4],
    creator: [u8; 4],
    special: u32,
    date: u32,
    invisible: bool,
}

/// An extents overflow record: fork type, file id, first block, extents.
type OverflowRecord = (u8, u32, u32, Vec<(u32, u32)>);

/// Builds an HFS+ volume in memory.
pub struct HfsBuilder {
    o: HfsOptions,
    image: Vec<u8>,
    next_block: usize,
    next_id: u32,
    records: BTreeMap<u32, Rec>,
    deleted: Vec<u32>,
    /// Put deleted records in a leaf node no longer in the tree, rather
    /// than in the free space of a node in use.
    stale_node: bool,
    /// (fork type, file id, first block, extents).
    overflow: Vec<OverflowRecord>,
    label: String,
    catalog_start: usize,
    extents_start: usize,
    clock: u32,
}

fn utf16(name: &str) -> Vec<u16> {
    name.encode_utf16().collect()
}

impl HfsBuilder {
    pub fn new(o: HfsOptions) -> Self {
        let image = vec![0u8; o.blocks * o.block_size];
        let mut b = Self {
            image,
            next_block: 1,
            next_id: 16,
            records: BTreeMap::new(),
            deleted: Vec::new(),
            stale_node: false,
            overflow: Vec::new(),
            label: "Untitled".to_owned(),
            catalog_start: 0,
            extents_start: 0,
            clock: u32::try_from(STAMP + HFS_EPOCH).unwrap(),
            o,
        };
        // The allocation bitmap, then both B-trees.
        b.alloc((b.o.blocks / 8).div_ceil(b.o.block_size).max(1));
        b.extents_start = b.alloc(2 * b.o.node_size / b.o.block_size)[0] as usize;
        let catalog_blocks = b.o.catalog_nodes * b.o.node_size / b.o.block_size;
        b.catalog_start = b.alloc(catalog_blocks)[0] as usize;
        let date = b.tick();
        b.records.insert(
            HFS_ROOT,
            Rec {
                parent: 1,
                id: HFS_ROOT,
                name: Vec::new(),
                folder: true,
                data: ForkSpec::default(),
                resource: ForkSpec::default(),
                file_type: [0; 4],
                creator: [0; 4],
                special: 0,
                date,
                invisible: false,
            },
        );
        b
    }

    pub fn label(&mut self, label: &str) {
        self.label = label.to_owned();
    }

    pub fn block_size(&self) -> usize {
        self.o.block_size
    }

    /// Where the copy of the volume header is, from the volume's start.
    pub fn backup_header(&self) -> usize {
        self.image.len() - 1024
    }

    /// Deleted records go to a leaf node no longer in the tree.
    pub fn deleted_into_free_node(&mut self) {
        self.stale_node = true;
    }

    fn tick(&mut self) -> u32 {
        self.clock += 60;
        self.clock
    }

    /// `count` blocks in a row.
    pub fn alloc(&mut self, count: usize) -> Vec<u32> {
        let start = self.next_block;
        self.next_block += count;
        assert!(self.next_block < self.o.blocks - 1, "test volume is full");
        (start..start + count)
            .map(|b| u32::try_from(b).unwrap())
            .collect()
    }

    /// Two files' worth of blocks, alternating.
    pub fn alloc_interleaved(&mut self, each: usize) -> (Vec<u32>, Vec<u32>) {
        let all = self.alloc(each * 2);
        (
            all.iter().step_by(2).copied().collect(),
            all.iter().skip(1).step_by(2).copied().collect(),
        )
    }

    fn write_fork(&mut self, data: &[u8], blocks: &[u32]) -> ForkSpec {
        let bs = self.o.block_size;
        for (i, &block) in blocks.iter().enumerate() {
            let piece = data
                .get(i * bs..data.len().min((i + 1) * bs))
                .unwrap_or(&[]);
            put(&mut self.image, block as usize * bs, piece);
        }
        let mut extents: Vec<(u32, u32)> = Vec::new();
        for &block in blocks {
            match extents.last_mut() {
                Some((start, count)) if *start + *count == block => *count += 1,
                _ => extents.push((block, 1)),
            }
        }
        ForkSpec {
            size: data.len() as u64,
            total_blocks: u32::try_from(blocks.len()).unwrap(),
            extents,
        }
    }

    fn add(&mut self, parent: u32, name: &str, folder: bool) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let date = self.tick();
        self.records.insert(
            id,
            Rec {
                parent,
                id,
                name: utf16(name),
                folder,
                data: ForkSpec::default(),
                resource: ForkSpec::default(),
                file_type: [0; 4],
                creator: [0; 4],
                special: 0,
                date,
                invisible: false,
            },
        );
        id
    }

    pub fn mkdir(&mut self, parent: u32, name: &str) -> u32 {
        self.add(parent, name, true)
    }

    pub fn file(&mut self, parent: u32, name: &str, data: &[u8]) -> u32 {
        let blocks = self.alloc(data.len().div_ceil(self.o.block_size));
        self.file_in(parent, name, data, &blocks)
    }

    /// A file stored in the given blocks. More than 8 pieces go to the
    /// extents overflow file.
    pub fn file_in(&mut self, parent: u32, name: &str, data: &[u8], blocks: &[u32]) -> u32 {
        let id = self.add(parent, name, false);
        let mut fork = self.write_fork(data, blocks);
        if fork.extents.len() > 8 {
            let rest = fork.extents.split_off(8);
            let mut start: u32 = fork.extents.iter().map(|&(_, c)| c).sum();
            for chunk in rest.chunks(8) {
                self.overflow.push((0, id, start, chunk.to_vec()));
                start += chunk.iter().map(|&(_, c)| c).sum::<u32>();
            }
        }
        self.records.get_mut(&id).unwrap().data = fork;
        id
    }

    /// Gives a file a resource fork.
    pub fn resource(&mut self, file: u32, data: &[u8]) {
        let blocks = self.alloc(data.len().div_ceil(self.o.block_size));
        let fork = self.write_fork(data, &blocks);
        self.records.get_mut(&file).unwrap().resource = fork;
    }

    pub fn invisible(&mut self, id: u32) {
        self.records.get_mut(&id).unwrap().invisible = true;
    }

    pub fn symlink(&mut self, parent: u32, name: &str, target: &str) -> u32 {
        let id = self.file(parent, name, target.as_bytes());
        let record = self.records.get_mut(&id).unwrap();
        record.file_type = *b"slnk";
        record.creator = *b"rhap";
        id
    }

    /// Makes a second name for `file` the way HFS+ does: the data moves to
    /// "iNode<file>" in the private folder, and both names become links.
    pub fn hard_link(&mut self, file: u32, parent: u32, name: &str) -> u32 {
        let private = match self
            .records
            .values()
            .find(|r| r.name == utf16(PRIVATE_FILES))
        {
            Some(r) => r.id,
            None => {
                let id = self.mkdir(HFS_ROOT, PRIVATE_FILES);
                self.invisible(id);
                id
            }
        };
        let original = self.records[&file].clone();
        let inode = self.add(private, &format!("iNode{file}"), false);
        {
            let target = self.records.get_mut(&inode).unwrap();
            target.data = original.data;
            target.resource = original.resource;
        }
        let link = self.add(parent, name, false);
        for id in [file, link] {
            let record = self.records.get_mut(&id).unwrap();
            record.data = ForkSpec::default();
            record.resource = ForkSpec::default();
            record.file_type = *b"hlnk";
            record.creator = *b"hfs+";
            record.special = file;
        }
        link
    }

    /// Removes a record without a trace (as if overwritten).
    pub fn forget(&mut self, id: u32) {
        self.records.remove(&id);
    }

    /// Deletes a file or folder: its records leave the tree, but their
    /// bytes stay where they were.
    pub fn delete(&mut self, id: u32) {
        self.deleted.push(id);
    }

    fn key(parent: u32, name: &[u16]) -> Vec<u8> {
        let mut key = Vec::new();
        key.extend_from_slice(&u16::try_from(6 + 2 * name.len()).unwrap().to_be_bytes());
        key.extend_from_slice(&parent.to_be_bytes());
        key.extend_from_slice(&u16::try_from(name.len()).unwrap().to_be_bytes());
        for unit in name {
            key.extend_from_slice(&unit.to_be_bytes());
        }
        key
    }

    fn fork_bytes(fork: &ForkSpec, out: &mut [u8]) {
        out[0..8].copy_from_slice(&fork.size.to_be_bytes());
        out[12..16].copy_from_slice(&fork.total_blocks.to_be_bytes());
        for (i, &(start, count)) in fork.extents.iter().take(8).enumerate() {
            out[16 + i * 8..20 + i * 8].copy_from_slice(&start.to_be_bytes());
            out[20 + i * 8..24 + i * 8].copy_from_slice(&count.to_be_bytes());
        }
    }

    /// The record's catalog entry (key and data), and its thread record.
    fn record_bytes(&self, r: &Rec) -> (Vec<u8>, Vec<u8>) {
        let name = if r.id == HFS_ROOT {
            utf16(&self.label)
        } else {
            r.name.clone()
        };
        let mut record = Self::key(r.parent, &name);
        let mut data = vec![0u8; if r.folder { 88 } else { 248 }];
        data[0..2].copy_from_slice(&(if r.folder { 1u16 } else { 2 }).to_be_bytes());
        data[2..4].copy_from_slice(&2u16.to_be_bytes());
        data[8..12].copy_from_slice(&r.id.to_be_bytes());
        for at in [12, 16, 20, 24] {
            data[at..at + 4].copy_from_slice(&r.date.to_be_bytes());
        }
        // Created a day before it was last changed.
        data[12..16].copy_from_slice(&(r.date - 86_400).to_be_bytes());
        data[44..48].copy_from_slice(&r.special.to_be_bytes());
        if !r.folder {
            data[48..52].copy_from_slice(&r.file_type);
            data[52..56].copy_from_slice(&r.creator);
            Self::fork_bytes(&r.data, &mut data[88..168]);
            Self::fork_bytes(&r.resource, &mut data[168..248]);
        }
        if r.invisible {
            data[56..58].copy_from_slice(&0x4000u16.to_be_bytes());
        }
        record.extend(data);
        let mut thread = Self::key(r.id, &[]);
        thread.extend_from_slice(&(if r.folder { 3u16 } else { 4 }).to_be_bytes());
        thread.extend_from_slice(&[0, 0]);
        thread.extend_from_slice(&r.parent.to_be_bytes());
        thread.extend_from_slice(&u16::try_from(name.len()).unwrap().to_be_bytes());
        for unit in &name {
            thread.extend_from_slice(&unit.to_be_bytes());
        }
        (record, thread)
    }

    /// A node: descriptor, records, then the offsets table. `extra` goes in
    /// its free space, after the records.
    fn node(&self, kind: i8, height: u8, next: u32, records: &[Vec<u8>], extra: &[u8]) -> Vec<u8> {
        let size = self.o.node_size;
        let mut node = vec![0u8; size];
        node[0..4].copy_from_slice(&next.to_be_bytes());
        node[8] = kind.to_be_bytes()[0];
        node[9] = height;
        node[10..12].copy_from_slice(&u16::try_from(records.len()).unwrap().to_be_bytes());
        let mut at = 14;
        for (i, record) in records.iter().enumerate() {
            node[at..at + record.len()].copy_from_slice(record);
            let slot = size - 2 * (i + 1);
            node[slot..slot + 2].copy_from_slice(&u16::try_from(at).unwrap().to_be_bytes());
            at += record.len();
        }
        let slot = size - 2 * (records.len() + 1);
        node[slot..slot + 2].copy_from_slice(&u16::try_from(at).unwrap().to_be_bytes());
        assert!(
            at + extra.len() <= slot,
            "no room in the node for left-over records"
        );
        node[at..at + extra.len()].copy_from_slice(extra);
        node
    }

    fn fits(&self, records: &[Vec<u8>], more: usize) -> bool {
        let used: usize = records.iter().map(Vec::len).sum();
        14 + used + more + 2 * (records.len() + 2) <= self.o.node_size
    }

    /// The B-tree header node.
    fn header_node(
        &self,
        depth: u16,
        root: u32,
        leaf_records: u32,
        first: u32,
        last: u32,
        total: u32,
    ) -> Vec<u8> {
        let mut header = vec![0u8; 106];
        header[0..2].copy_from_slice(&depth.to_be_bytes());
        header[2..6].copy_from_slice(&root.to_be_bytes());
        header[6..10].copy_from_slice(&leaf_records.to_be_bytes());
        header[10..14].copy_from_slice(&first.to_be_bytes());
        header[14..18].copy_from_slice(&last.to_be_bytes());
        header[18..20].copy_from_slice(&u16::try_from(self.o.node_size).unwrap().to_be_bytes());
        header[20..22].copy_from_slice(&516u16.to_be_bytes());
        header[22..26].copy_from_slice(&total.to_be_bytes());
        header[37] = if self.o.hfsx { 0xBC } else { 0xCF };
        header[38..42].copy_from_slice(&6u32.to_be_bytes());
        self.node(1, 0, 0, &[header, vec![0u8; 128], vec![0xFFu8; 8]], &[])
    }

    fn catalog(&self) -> Vec<Vec<u8>> {
        let mut live: Vec<(u32, Vec<u16>, Vec<u8>)> = Vec::new();
        let mut stale = Vec::new();
        for r in self.records.values() {
            let (record, thread) = self.record_bytes(r);
            if self.deleted.contains(&r.id) {
                stale.extend(record);
                continue;
            }
            let lower: Vec<u16> = String::from_utf16_lossy(&r.name)
                .to_lowercase()
                .encode_utf16()
                .collect();
            live.push((r.parent, lower, record));
            live.push((r.id, Vec::new(), thread));
        }
        live.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        // Pack the records into leaf nodes 1, 2, ...
        let mut leaves: Vec<Vec<Vec<u8>>> = vec![Vec::new()];
        let mut first_keys: Vec<Vec<u8>> = Vec::new();
        for (_, _, record) in &live {
            if !self.fits(leaves.last().unwrap(), record.len() + 64) {
                leaves.push(Vec::new());
            }
            if leaves.last().unwrap().is_empty() {
                let key_length = usize::from(u16::from_be_bytes([record[0], record[1]]));
                first_keys.push(record[..2 + key_length].to_vec());
            }
            leaves.last_mut().unwrap().push(record.clone());
        }
        let leaf_count = leaves.len();
        let index_node = u32::try_from(leaf_count + 1).unwrap();
        let mut nodes = Vec::new();
        let root = if leaf_count > 1 { index_node } else { 1 };
        let depth = if leaf_count > 1 { 2 } else { 1 };
        nodes.push(self.header_node(
            depth,
            root,
            u32::try_from(live.len()).unwrap(),
            1,
            u32::try_from(leaf_count).unwrap(),
            u32::try_from(self.o.catalog_nodes).unwrap(),
        ));
        for (i, records) in leaves.iter().enumerate() {
            let next = if i + 1 < leaf_count {
                u32::try_from(i + 2).unwrap()
            } else {
                0
            };
            let last = i + 1 == leaf_count;
            let extra = if last && !self.stale_node {
                stale.as_slice()
            } else {
                &[]
            };
            nodes.push(self.node(-1, 1, next, records, extra));
        }
        if leaf_count > 1 {
            let entries: Vec<Vec<u8>> = first_keys
                .iter()
                .enumerate()
                .map(|(i, key)| {
                    let mut entry = key.clone();
                    entry.extend_from_slice(&u32::try_from(i + 1).unwrap().to_be_bytes());
                    entry
                })
                .collect();
            nodes.push(self.node(0, 2, 0, &entries, &[]));
        }
        if self.stale_node {
            // A leaf node freed after its records were deleted: its table
            // still lists them.
            let mut records = Vec::new();
            let mut at = 0;
            while at < stale.len() {
                let key_length = usize::from(u16::from_be_bytes([stale[at], stale[at + 1]]));
                let kind =
                    u16::from_be_bytes([stale[at + 2 + key_length], stale[at + 3 + key_length]]);
                let size = 2 + key_length + if kind == 1 { 88 } else { 248 };
                records.push(stale[at..at + size].to_vec());
                at += size;
            }
            nodes.push(self.node(-1, 1, 0, &records, &[]));
        }
        assert!(nodes.len() <= self.o.catalog_nodes, "catalog too small");
        nodes
    }

    fn extents_tree(&self) -> Vec<Vec<u8>> {
        let mut records: Vec<Vec<u8>> = self
            .overflow
            .iter()
            .map(|(fork, file, start, extents)| {
                let mut record = vec![0u8; 12 + 64];
                record[0..2].copy_from_slice(&10u16.to_be_bytes());
                record[2] = *fork;
                record[4..8].copy_from_slice(&file.to_be_bytes());
                record[8..12].copy_from_slice(&start.to_be_bytes());
                for (i, &(block, count)) in extents.iter().enumerate() {
                    record[12 + i * 8..16 + i * 8].copy_from_slice(&block.to_be_bytes());
                    record[16 + i * 8..20 + i * 8].copy_from_slice(&count.to_be_bytes());
                }
                record
            })
            .collect();
        records.sort_by(|a, b| a[2..12].cmp(&b[2..12]));
        let (depth, root, first) = if records.is_empty() {
            (0, 0, 0)
        } else {
            (1, 1, 1)
        };
        vec![
            self.header_node(
                depth,
                root,
                u32::try_from(records.len()).unwrap(),
                first,
                first,
                2,
            ),
            self.node(-1, 1, 0, &records, &[]),
        ]
    }

    pub fn build(mut self) -> Vec<u8> {
        let bs = self.o.block_size;
        let node_size = self.o.node_size;
        for (i, node) in self.catalog().into_iter().enumerate() {
            put(
                &mut self.image,
                self.catalog_start * bs + i * node_size,
                &node,
            );
        }
        for (i, node) in self.extents_tree().into_iter().enumerate() {
            put(
                &mut self.image,
                self.extents_start * bs + i * node_size,
                &node,
            );
        }
        let mut header = vec![0u8; 512];
        let signature: u16 = if self.o.hfsx { 0x4858 } else { 0x482B };
        header[0..2].copy_from_slice(&signature.to_be_bytes());
        header[2..4].copy_from_slice(&(if self.o.hfsx { 5u16 } else { 4 }).to_be_bytes());
        header[4..8].copy_from_slice(&0x100u32.to_be_bytes());
        header[40..44].copy_from_slice(&u32::try_from(bs).unwrap().to_be_bytes());
        header[44..48].copy_from_slice(&u32::try_from(self.o.blocks).unwrap().to_be_bytes());
        header[64..68].copy_from_slice(&self.next_id.to_be_bytes());
        let fork = |start: usize, bytes: usize| {
            let blocks = u32::try_from(bytes / bs).unwrap();
            ForkSpec {
                size: bytes as u64,
                total_blocks: blocks,
                extents: vec![(u32::try_from(start).unwrap(), blocks)],
            }
        };
        Self::fork_bytes(
            &fork(self.extents_start, 2 * node_size),
            &mut header[192..272],
        );
        Self::fork_bytes(
            &fork(self.catalog_start, self.o.catalog_nodes * node_size),
            &mut header[272..352],
        );
        put(&mut self.image, 1024, &header);
        let backup = self.image.len() - 1024;
        put(&mut self.image, backup, &header);
        self.image
    }
}
