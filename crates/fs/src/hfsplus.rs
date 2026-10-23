//! HFS+ and HFSX (Mac OS Extended): folders, files with their data and
//! resource forks, hard links, symbolic links, and deleted records left over
//! in the catalog.
//!
//! The whole catalog B-tree is read when the volume opens, like the MFT on
//! NTFS: every node, including free space inside nodes and nodes no longer
//! in use, where records of deleted files can survive.

use std::collections::{HashMap, HashSet};

use unicode_normalization::UnicodeNormalization;

use crate::error::FsError;
use crate::volume::Volume;
use crate::{
    Entry, EntryId, Extent, FileSystem, FsKind, LOST_FILES, StreamInfo, VolumeInfo, attributes,
};

/// Seconds from 1904-01-01 (HFS+ dates) to 1970-01-01.
const HFS_EPOCH: i64 = 2_082_844_800;
const ROOT: u32 = 2;
const CATALOG_FILE: u32 = 4;
/// The first CNID given to files and folders people make.
const FIRST_USER_ID: u32 = 16;
/// Catalogs bigger than this are damage.
const MAX_CATALOG: u64 = 4 << 30;
/// The resource fork, as a stream name.
pub(crate) const RESOURCE_FORK: &str = "rsrc";

const FOLDER_RECORD: u16 = 1;
const FILE_RECORD: u16 = 2;
const FOLDER_RECORD_SIZE: usize = 88;
const FILE_RECORD_SIZE: usize = 248;

/// Names of HFS+ metadata in the root folder, which aren't shown.
const PRIVATE_FILES: &str = "\0\0\0\0HFS+ Private Data";
const PRIVATE_DIRS: &str = ".HFS+ Private Directory Data\r";
const HIDDEN_IN_ROOT: [&str; 4] = [
    PRIVATE_FILES,
    PRIVATE_DIRS,
    ".journal",
    ".journal_info_block",
];

fn be16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        data.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

fn be64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(
        data.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
}

fn four_cc(text: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*text)
}

/// One fork of a file: its size and where its blocks are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Fork {
    size: u64,
    total_blocks: u64,
    /// (start block, block count), in order.
    extents: Vec<(u64, u64)>,
}

impl Fork {
    /// An HFSPlusForkData (80 bytes).
    fn parse(data: &[u8], at: usize) -> Option<Fork> {
        let mut extents = Vec::new();
        for i in 0..8 {
            let start = be32(data, at + 16 + i * 8)?;
            let count = be32(data, at + 20 + i * 8)?;
            if count > 0 {
                extents.push((u64::from(start), u64::from(count)));
            }
        }
        Some(Fork {
            size: be64(data, at)?,
            total_blocks: u64::from(be32(data, at + 12)?),
            extents,
        })
    }

    fn mapped_blocks(&self) -> u64 {
        self.extents.iter().map(|&(_, count)| count).sum()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Folder,
    File,
}

/// A catalog record for a file or folder.
#[derive(Clone, Debug)]
struct Record {
    parent: u32,
    id: u32,
    name: String,
    kind: Kind,
    created: Option<i64>,
    modified: Option<i64>,
    accessed: Option<i64>,
    locked: bool,
    invisible: bool,
    compressed: bool,
    file_type: u32,
    creator: u32,
    /// The BSD "special" field: the iNode number of a hard link.
    special: u32,
    data: Fork,
    resource: Fork,
}

fn date(seconds: u32) -> Option<i64> {
    (seconds != 0).then(|| i64::from(seconds) - HFS_EPOCH)
}

/// A catalog key: the parent folder and the name, and where the record
/// after it starts.
fn parse_key(node: &[u8], at: usize) -> Option<(u32, Vec<u16>, usize)> {
    let key_length = usize::from(be16(node, at)?);
    let parent = be32(node, at + 2)?;
    let name_length = usize::from(be16(node, at + 6)?);
    if name_length > 255 || key_length != 6 + 2 * name_length {
        return None;
    }
    let name = (0..name_length)
        .map(|i| be16(node, at + 8 + 2 * i))
        .collect::<Option<Vec<u16>>>()?;
    Some((parent, name, at + 2 + key_length))
}

/// A file or folder record at `at` (its key first).
fn parse_record(node: &[u8], at: usize, end: usize) -> Option<(Record, usize)> {
    let (parent, name, data) = parse_key(node, at)?;
    let kind = match be16(node, data)? {
        FOLDER_RECORD => Kind::Folder,
        FILE_RECORD => Kind::File,
        _ => return None,
    };
    let size = if kind == Kind::Folder {
        FOLDER_RECORD_SIZE
    } else {
        FILE_RECORD_SIZE
    };
    let record_end = data.checked_add(size)?;
    if record_end > end.min(node.len()) {
        return None;
    }
    let raw = &node[data..record_end];
    let (data_fork, resource_fork) = if kind == Kind::File {
        (Fork::parse(raw, 88)?, Fork::parse(raw, 168)?)
    } else {
        (Fork::default(), Fork::default())
    };
    let name = String::from_utf16_lossy(&name).nfc().collect();
    let record = Record {
        parent,
        id: be32(raw, 8)?,
        name,
        kind,
        created: date(be32(raw, 12)?),
        modified: date(be32(raw, 16)?),
        accessed: date(be32(raw, 24)?),
        locked: be16(raw, 2)? & 0x1 != 0,
        invisible: be16(raw, 56)? & 0x4000 != 0,
        compressed: raw.get(41).is_some_and(|flags| flags & 0x20 != 0),
        file_type: be32(raw, 48)?,
        creator: be32(raw, 52)?,
        special: be32(raw, 44)?,
        data: data_fork,
        resource: resource_fork,
    };
    Some((record, record_end))
}

/// Extents (start block, block count) continuing a fork from a block.
type Overflow = Vec<(u64, Vec<(u64, u64)>)>;

/// Where a fork's blocks are, as runs of the volume.
#[derive(Clone, Debug, Default)]
struct ForkMap {
    size: u64,
    /// (first logical block, first volume block, block count).
    runs: Vec<(u64, u64, u64)>,
}

pub(crate) struct HfsPlus {
    volume: Volume,
    info: VolumeInfo,
    block_size: u64,
    total_blocks: u64,
    records: HashMap<u32, Record>,
    children: HashMap<u32, Vec<u32>>,
    deleted: HashMap<u32, Record>,
    deleted_children: HashMap<u32, Vec<u32>>,
    /// Deleted records whose folder is gone.
    orphans: Vec<u32>,
    /// (fork type, file id) → (first block of the fork it continues, extents).
    overflow: HashMap<(u8, u32), Overflow>,
    private_files: Option<u32>,
    private_dirs: Option<u32>,
}

/// The parts of the volume header DataReco uses.
struct Header {
    hfsx: bool,
    block_size: u64,
    total_blocks: u64,
    extents_file: Fork,
    catalog_file: Fork,
}

fn parse_header(raw: &[u8]) -> Result<Header, FsError> {
    let damaged =
        |what: &str| FsError::damaged(format!("The HFS+ volume header is damaged: {what}."));
    let signature = be16(raw, 0).ok_or_else(|| damaged("it is cut short"))?;
    let hfsx = match signature {
        0x482B => false,
        0x4858 => true,
        _ => return Err(damaged("its signature is missing")),
    };
    let block_size = u64::from(be32(raw, 40).ok_or_else(|| damaged("it is cut short"))?);
    let total_blocks = u64::from(be32(raw, 44).ok_or_else(|| damaged("it is cut short"))?);
    if !block_size.is_power_of_two() || !(512..=1 << 20).contains(&block_size) {
        return Err(damaged("its block size is impossible"));
    }
    if total_blocks == 0 {
        return Err(damaged("it has no blocks"));
    }
    let fork = |at| Fork::parse(raw, at).ok_or_else(|| damaged("it is cut short"));
    Ok(Header {
        hfsx,
        block_size,
        total_blocks,
        extents_file: fork(192)?,
        catalog_file: fork(272)?,
    })
}

/// Reads a B-tree's nodes: its node size, and each node (`None` for ones
/// that can't be read).
struct Tree {
    node_size: usize,
    first_leaf: u32,
    nodes: Vec<Option<Vec<u8>>>,
}

/// A B-tree node's records: (start of each record), and where free space
/// begins and ends.
struct NodeLayout {
    kind: i8,
    height: u8,
    next: u32,
    records: Vec<usize>,
    free: (usize, usize),
}

fn node_layout(node: &[u8]) -> Option<NodeLayout> {
    let size = node.len();
    let count = usize::from(be16(node, 10)?);
    let table = size.checked_sub(2 * (count + 1))?;
    if table < 14 {
        return None;
    }
    let mut records = Vec::with_capacity(count);
    for i in 0..count {
        let at = usize::from(be16(node, size - 2 * (i + 1))?);
        if at < 14 || at >= table {
            return None;
        }
        records.push(at);
    }
    let free_start = usize::from(be16(node, size - 2 * (count + 1))?).clamp(14, table);
    Some(NodeLayout {
        kind: i8::from_be_bytes([*node.get(8)?]),
        height: *node.get(9)?,
        next: be32(node, 0)?,
        records,
        free: (free_start, table),
    })
}

impl HfsPlus {
    pub fn open(volume: Volume) -> Result<HfsPlus, FsError> {
        HfsPlus::open_with(volume, None)
    }

    /// Opens with `header` (512 bytes, the copy at the volume's end) instead
    /// of the one at byte 1024.
    pub fn open_with(volume: Volume, header: Option<Vec<u8>>) -> Result<HfsPlus, FsError> {
        let raw = match header {
            Some(raw) => raw,
            None => volume.read_vec(1024, 512)?,
        };
        let header = parse_header(&raw)?;
        let mut hfs = HfsPlus {
            info: VolumeInfo {
                kind: FsKind::HfsPlus,
                name: if header.hfsx { "HFSX" } else { "HFS+" }.to_owned(),
                label: None,
                offset: volume.start(),
                size: header
                    .block_size
                    .saturating_mul(header.total_blocks)
                    .min(volume.size()),
                cluster_size: header.block_size,
            },
            volume,
            block_size: header.block_size,
            total_blocks: header.total_blocks,
            records: HashMap::new(),
            children: HashMap::new(),
            deleted: HashMap::new(),
            deleted_children: HashMap::new(),
            orphans: Vec::new(),
            overflow: HashMap::new(),
            private_files: None,
            private_dirs: None,
        };
        // The extents file never overflows; the catalog may, into it.
        let extents_map = hfs.fork_map(&header.extents_file, None);
        if let Some(tree) = hfs.read_tree(&extents_map)? {
            hfs.index_overflow(&tree);
        }
        let catalog_map = hfs.fork_map(&header.catalog_file, Some((0, CATALOG_FILE)));
        let tree = hfs
            .read_tree(&catalog_map)?
            .ok_or_else(|| FsError::damaged("The HFS+ catalog is damaged."))?;
        hfs.index_catalog(&tree);
        let root = hfs
            .records
            .get(&ROOT)
            .ok_or_else(|| FsError::damaged("The HFS+ catalog has no root folder."))?;
        hfs.info.label = Some(root.name.clone()).filter(|n| !n.is_empty());
        Ok(hfs)
    }

    /// A fork's blocks, with its overflow extents when `owner` (fork type,
    /// file id) says whose they are.
    fn fork_map(&self, fork: &Fork, owner: Option<(u8, u32)>) -> ForkMap {
        let mut extents = fork.extents.clone();
        if let Some(owner) = owner
            && let Some(more) = self.overflow.get(&owner)
        {
            let mut mapped = fork.mapped_blocks();
            for (start, pieces) in more {
                if *start != mapped || mapped >= fork.total_blocks {
                    continue;
                }
                for &(block, count) in pieces {
                    extents.push((block, count));
                    mapped += count;
                }
            }
        }
        let mut runs = Vec::new();
        let mut logical = 0u64;
        for (start, count) in extents {
            runs.push((logical, start, count));
            logical = logical.saturating_add(count);
        }
        ForkMap {
            size: fork.size,
            runs,
        }
    }

    fn read_fork(&self, map: &ForkMap, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        if offset >= map.size || buf.is_empty() {
            return Ok(0);
        }
        let n = usize::try_from(map.size - offset)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let block_size = self.block_size;
        let mut done = 0;
        while done < n {
            let position = offset + done as u64;
            let logical = position / block_size;
            let left = (n - done) as u64;
            let run = map
                .runs
                .iter()
                .find(|&&(first, _, count)| logical >= first && logical < first + count);
            let (take, physical) = match run {
                Some(&(first, start, count)) => {
                    let end = (first + count).saturating_mul(block_size);
                    let at = (start + (logical - first)) * block_size + position % block_size;
                    (left.min(end - position), Some(at))
                }
                // Blocks no extent covers (a deleted file's lost overflow
                // extents): zeros.
                None => {
                    let next = map
                        .runs
                        .iter()
                        .map(|&(first, _, _)| first.saturating_mul(block_size))
                        .filter(|&start| start > position)
                        .min()
                        .unwrap_or(u64::MAX);
                    (left.min(next - position), None)
                }
            };
            let take = usize::try_from(take).unwrap_or(n - done).max(1);
            let piece = &mut buf[done..done + take];
            match physical {
                Some(at) => {
                    if at.saturating_add(take as u64) > self.total_blocks * block_size {
                        return Err(FsError::damaged(
                            "A file's extents point past the end of the volume.",
                        ));
                    }
                    self.volume.read(at, piece)?;
                }
                None => piece.fill(0),
            }
            done += take;
        }
        Ok(n)
    }

    /// Reads every node of a B-tree file.
    fn read_tree(&self, map: &ForkMap) -> Result<Option<Tree>, FsError> {
        if map.size == 0 {
            return Ok(None);
        }
        if map.size > MAX_CATALOG {
            return Err(FsError::damaged(
                "An HFS+ B-tree claims to be impossibly big.",
            ));
        }
        let mut header = vec![0u8; 512];
        if self.read_fork(map, 0, &mut header)? < 512 {
            return Ok(None);
        }
        let node_size = usize::from(be16(&header, 32).unwrap_or(0));
        if !node_size.is_power_of_two() || !(512..=32768).contains(&node_size) {
            return Err(FsError::damaged(
                "An HFS+ B-tree's node size is impossible.",
            ));
        }
        let first_leaf = be32(&header, 24).unwrap_or(0);
        let count = map.size / node_size as u64;
        let mut nodes = Vec::new();
        for i in 0..count {
            let mut node = vec![0u8; node_size];
            let read = self.read_fork(map, i * node_size as u64, &mut node);
            nodes.push(matches!(read, Ok(n) if n == node_size).then_some(node));
        }
        Ok(Some(Tree {
            node_size,
            first_leaf,
            nodes,
        }))
    }

    /// The leaf nodes reachable from the first leaf: the ones in use.
    fn leaf_chain(tree: &Tree) -> HashSet<usize> {
        let mut chain = HashSet::new();
        let mut next = tree.first_leaf;
        while next != 0 {
            let index = next as usize;
            if !chain.insert(index) {
                break;
            }
            let Some(layout) = tree
                .nodes
                .get(index)
                .and_then(Option::as_ref)
                .and_then(|n| node_layout(n))
            else {
                break;
            };
            next = layout.next;
        }
        chain
    }

    fn index_overflow(&mut self, tree: &Tree) {
        for index in Self::leaf_chain(tree) {
            let Some(node) = tree.nodes.get(index).and_then(Option::as_ref) else {
                continue;
            };
            let Some(layout) = node_layout(node).filter(|l| l.kind == -1) else {
                continue;
            };
            for &at in &layout.records {
                let parsed = (|| {
                    if be16(node, at)? != 10 {
                        return None;
                    }
                    let fork_type = *node.get(at + 2)?;
                    let file = be32(node, at + 4)?;
                    let start = u64::from(be32(node, at + 8)?);
                    let mut extents = Vec::new();
                    for i in 0..8 {
                        let block = be32(node, at + 12 + i * 8)?;
                        let count = be32(node, at + 16 + i * 8)?;
                        if count > 0 {
                            extents.push((u64::from(block), u64::from(count)));
                        }
                    }
                    Some(((fork_type, file), start, extents))
                })();
                if let Some((owner, start, extents)) = parsed {
                    self.overflow
                        .entry(owner)
                        .or_default()
                        .push((start, extents));
                }
            }
        }
        for list in self.overflow.values_mut() {
            list.sort_by_key(|&(start, _)| start);
        }
    }

    fn index_catalog(&mut self, tree: &Tree) {
        let chain = Self::leaf_chain(tree);
        let mut stale = Vec::new();
        for (index, node) in tree.nodes.iter().enumerate() {
            let Some(node) = node else {
                continue;
            };
            let Some(layout) = node_layout(node) else {
                // A node whose record table is garbage: look at all of it.
                if chain.contains(&index) {
                    continue;
                }
                stale.extend(scan(node, 14, tree.node_size));
                continue;
            };
            if layout.kind != -1 || layout.height != 1 {
                continue;
            }
            if chain.contains(&index) {
                for &at in &layout.records {
                    if let Some((record, _)) = parse_record(node, at, layout.free.0) {
                        self.records.insert(record.id, record);
                    }
                }
                stale.extend(scan(node, layout.free.0, layout.free.1));
            } else {
                // A leaf node no longer in the tree: all of it is left over.
                stale.extend(scan(node, 14, layout.free.1));
            }
        }
        for record in self.records.values() {
            if record.id != ROOT {
                self.children
                    .entry(record.parent)
                    .or_default()
                    .push(record.id);
            }
        }
        for record in stale {
            if self.records.contains_key(&record.id) || self.deleted.contains_key(&record.id) {
                continue;
            }
            self.deleted.insert(record.id, record);
        }
        for record in self.deleted.values() {
            if self.records.contains_key(&record.parent)
                || self.deleted.contains_key(&record.parent)
            {
                self.deleted_children
                    .entry(record.parent)
                    .or_default()
                    .push(record.id);
            } else {
                self.orphans.push(record.id);
            }
        }
        let by_name = |records: &HashMap<u32, Record>, list: &mut Vec<u32>| {
            list.sort_by(|a, b| {
                let name = |id: &u32| records.get(id).map(|r| r.name.to_lowercase());
                name(a).cmp(&name(b))
            });
        };
        for list in self.children.values_mut() {
            by_name(&self.records, list);
        }
        for list in self.deleted_children.values_mut() {
            by_name(&self.deleted, list);
        }
        by_name(&self.deleted, &mut self.orphans);
        let root_child = |name: &str| {
            self.children.get(&ROOT).and_then(|list| {
                list.iter()
                    .copied()
                    .find(|id| self.records.get(id).is_some_and(|r| r.name == name))
            })
        };
        self.private_files = root_child(PRIVATE_FILES);
        self.private_dirs = root_child(PRIVATE_DIRS);
    }

    fn record(&self, id: u32, deleted: bool) -> Option<&Record> {
        if deleted {
            self.deleted.get(&id)
        } else {
            self.records.get(&id)
        }
    }

    /// What a hard link points to: the file or folder holding its data.
    fn link_target(&self, record: &Record) -> Option<&Record> {
        if record.kind != Kind::File {
            return None;
        }
        let (folder, name) =
            if record.file_type == four_cc(b"hlnk") && record.creator == four_cc(b"hfs+") {
                (self.private_files?, format!("iNode{}", record.special))
            } else if record.file_type == four_cc(b"fdrp") && record.creator == four_cc(b"MACS") {
                (self.private_dirs?, format!("dir_{}", record.special))
            } else {
                return None;
            };
        self.children
            .get(&folder)?
            .iter()
            .filter_map(|id| self.records.get(id))
            .find(|r| r.name == name)
    }

    /// The record whose data and children an entry shows.
    fn resolved(&self, id: u32, deleted: bool) -> Option<&Record> {
        let record = self.record(id, deleted)?;
        Some(self.link_target(record).unwrap_or(record))
    }

    fn symlink(record: &Record) -> bool {
        record.file_type == four_cc(b"slnk") && record.creator == four_cc(b"rhap")
    }

    fn entry(&self, record: &Record, deleted: bool) -> Entry {
        let target = self.link_target(record).unwrap_or(record);
        let is_dir = target.kind == Kind::Folder;
        let mut notes = Vec::new();
        if deleted {
            notes.push(
                "Deleted. Its record was found left over in the catalog; its blocks may have \
                 been reused since."
                    .to_owned(),
            );
            if target.data.mapped_blocks() < target.data.total_blocks {
                notes.push(
                    "Only its first eight pieces are known; the rest reads as zeros.".to_owned(),
                );
            }
        }
        if target.compressed {
            notes.push(
                "Compressed by macOS: reading compressed files isn't supported yet.".to_owned(),
            );
        }
        if Self::symlink(target) && !deleted {
            let map = self.fork_map(&target.data, Some((0, target.id)));
            let mut buf = vec![0u8; usize::try_from(map.size.min(4096)).unwrap_or(0)];
            let n = self.read_fork(&map, 0, &mut buf).unwrap_or(0);
            notes.push(format!("A link to {}", String::from_utf8_lossy(&buf[..n])));
        }
        let mut attributes = 0;
        if is_dir {
            attributes |= attributes::DIRECTORY;
        }
        if record.invisible {
            attributes |= attributes::HIDDEN;
        }
        if record.locked {
            attributes |= attributes::READ_ONLY;
        }
        if target.compressed {
            attributes |= attributes::COMPRESSED;
        }
        let streams = if !is_dir && target.resource.size > 0 {
            vec![StreamInfo {
                name: RESOURCE_FORK.to_owned(),
                size: target.resource.size,
            }]
        } else {
            Vec::new()
        };
        Entry {
            id: EntryId::Catalog {
                id: record.id,
                deleted,
            },
            name: record.name.clone(),
            is_dir,
            size: if is_dir { 0 } else { target.data.size },
            deleted,
            created: target.created,
            modified: target.modified,
            accessed: target.accessed,
            attributes,
            streams,
            note: (!notes.is_empty()).then(|| notes.join(" ")),
        }
    }

    fn fork_of(&self, file: &EntryId, stream: Option<&str>) -> Result<ForkMap, FsError> {
        let EntryId::Catalog { id, deleted } = file else {
            return Err(FsError::NotFound("That file".to_owned()));
        };
        let record = self
            .resolved(*id, *deleted)
            .ok_or_else(|| FsError::NotFound("That file".to_owned()))?;
        let (fork, fork_type) = match stream {
            None => (&record.data, 0),
            Some(RESOURCE_FORK) => (&record.resource, 0xFF),
            Some(other) => {
                return Err(FsError::NotFound(format!("The stream {other}")));
            }
        };
        // A deleted file's overflow extents went with it.
        let owner = (!*deleted).then_some((fork_type, record.id));
        Ok(self.fork_map(fork, owner))
    }
}

/// File and folder records left over in `from..to` of a node.
fn scan(node: &[u8], from: usize, to: usize) -> Vec<Record> {
    let mut found = Vec::new();
    let mut at = from.next_multiple_of(2);
    while at + 8 < to {
        match parse_record(node, at, to).filter(plausible) {
            Some((record, end)) => {
                found.push(record);
                at = end;
            }
            None => at += 2,
        }
    }
    found
}

/// Whether a record found in free space looks like a real one.
fn plausible((record, _): &(Record, usize)) -> bool {
    record.id >= FIRST_USER_ID
        && record.parent >= 1
        && !record.name.is_empty()
        && !record.name.chars().any(char::is_control)
        && record.data.size <= record.data.total_blocks.saturating_mul(1 << 20)
}

impl FileSystem for HfsPlus {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn root(&self) -> EntryId {
        EntryId::Catalog {
            id: ROOT,
            deleted: false,
        }
    }

    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError> {
        if *dir == EntryId::Orphans {
            return Ok(self
                .orphans
                .iter()
                .filter_map(|id| self.deleted.get(id))
                .map(|r| self.entry(r, true))
                .collect());
        }
        let EntryId::Catalog { id, deleted } = *dir else {
            return Err(FsError::NotFound("That folder".to_owned()));
        };
        let folder = self
            .resolved(id, deleted)
            .filter(|r| r.kind == Kind::Folder)
            .ok_or_else(|| FsError::NotFound("That folder".to_owned()))?;
        let folder_id = folder.id;
        let mut entries = Vec::new();
        if !deleted {
            for child in self.children.get(&folder_id).into_iter().flatten() {
                let Some(record) = self.records.get(child) else {
                    continue;
                };
                if folder_id == ROOT && HIDDEN_IN_ROOT.contains(&record.name.as_str()) {
                    continue;
                }
                entries.push(self.entry(record, false));
            }
        }
        for child in self.deleted_children.get(&folder_id).into_iter().flatten() {
            if let Some(record) = self.deleted.get(child) {
                entries.push(self.entry(record, true));
            }
        }
        if folder_id == ROOT && !deleted && !self.orphans.is_empty() {
            entries.push(Entry {
                id: EntryId::Orphans,
                name: LOST_FILES.to_owned(),
                is_dir: true,
                size: 0,
                deleted: false,
                created: None,
                modified: None,
                accessed: None,
                attributes: attributes::DIRECTORY,
                streams: Vec::new(),
                note: Some(format!(
                    "{} deleted files and folders found in the catalog whose folder is gone.",
                    self.orphans.len()
                )),
            });
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
        let map = self.fork_of(file, stream)?;
        self.read_fork(&map, offset, buf)
    }

    fn extents(&self, file: &EntryId, stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        let map = self.fork_of(file, stream)?;
        let base = self.volume.start();
        let mut extents = Vec::new();
        let mut covered = 0;
        for &(first, start, count) in &map.runs {
            let from = first.saturating_mul(self.block_size).min(map.size);
            let to = (first + count)
                .saturating_mul(self.block_size)
                .min(map.size);
            if from > covered {
                extents.push(Extent {
                    offset: 0,
                    length: from - covered,
                    sparse: true,
                });
            }
            if to > from {
                extents.push(Extent {
                    offset: base.saturating_add(start.saturating_mul(self.block_size)),
                    length: to - from,
                    sparse: false,
                });
            }
            covered = covered.max(to);
        }
        if covered < map.size {
            extents.push(Extent {
                offset: 0,
                length: map.size - covered,
                sparse: true,
            });
        }
        Ok(extents)
    }
}
