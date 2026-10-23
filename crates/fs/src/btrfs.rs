//! btrfs: subvolumes, folders and files (inline, regular and preallocated
//! extents, holes, hard links, symbolic links).
//!
//! btrfs addresses are logical: the chunk tree says where on the disk each
//! stretch is. A subvolume's tree is read whole the first time it's opened.
//! Not yet: compressed data, data on other disks of a multi-disk volume,
//! and deleted files.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use datareco_partitions::crc32c;

use crate::bytes::{bytes, text, u8_at, u16_at, u32_at, u64_at};
use crate::error::FsError;
use crate::volume::Volume;
use crate::{Entry, EntryId, Extent, FileSystem, FsKind, VolumeInfo, attributes};

const SUPERBLOCK: u64 = 0x1_0000;
/// The top-level subvolume (the file-system tree).
pub(crate) const TOP_TREE: u64 = 5;
/// The root folder of every subvolume.
pub(crate) const ROOT_DIR: u64 = 256;

const INODE_ITEM: u8 = 1;
const DIR_ITEM: u8 = 84;
const DIR_INDEX: u8 = 96;
const EXTENT_DATA: u8 = 108;
const ROOT_ITEM: u8 = 132;
const CHUNK_ITEM: u8 = 228;

const HEADER: usize = 101;
const ITEM: usize = 25;
const KEY_PTR: usize = 33;
/// Striped profiles: RAID0, RAID10, RAID5, RAID6.
const STRIPED: u64 = 0x8 | 0x40 | 0x80 | 0x100;
/// More nodes than this in one tree means it's damaged.
const MAX_NODES: usize = 1 << 24;

/// A key: object id, item type, offset.
type Key = (u64, u8, u64);

fn key_at(data: &[u8], at: usize) -> Option<Key> {
    Some((
        u64_at(data, at)?,
        u8_at(data, at + 8)?,
        u64_at(data, at + 9)?,
    ))
}

/// A chunk: `length` bytes of logical addresses from `logical`.
#[derive(Clone, Debug)]
struct Chunk {
    logical: u64,
    length: u64,
    kind: u64,
    /// (device id, offset on that device).
    stripes: Vec<(u64, u64)>,
}

fn parse_chunk(logical: u64, data: &[u8]) -> Option<(Chunk, usize)> {
    let count = usize::from(u16_at(data, 44)?);
    let mut stripes = Vec::with_capacity(count);
    for i in 0..count {
        stripes.push((u64_at(data, 48 + i * 32)?, u64_at(data, 56 + i * 32)?));
    }
    Some((
        Chunk {
            logical,
            length: u64_at(data, 0)?,
            kind: u64_at(data, 24)?,
            stripes,
        },
        48 + count * 32,
    ))
}

#[derive(Clone, Debug, Default)]
struct InodeItem {
    size: u64,
    mode: u32,
    accessed: Option<i64>,
    modified: Option<i64>,
    created: Option<i64>,
}

#[derive(Clone, Debug)]
struct DirEntry {
    name: String,
    /// Where it points: an inode in this tree, or another subvolume.
    location: Key,
}

#[derive(Clone, Debug)]
enum FileExtent {
    Inline {
        offset: u64,
        data: Vec<u8>,
        compression: u8,
    },
    Regular {
        offset: u64,
        /// Logical address of the extent on disk, or 0 for a hole.
        disk: u64,
        within: u64,
        length: u64,
        compression: u8,
        preallocated: bool,
    },
}

impl FileExtent {
    fn offset(&self) -> u64 {
        match self {
            FileExtent::Inline { offset, .. } | FileExtent::Regular { offset, .. } => *offset,
        }
    }

    fn length(&self) -> u64 {
        match self {
            FileExtent::Inline { data, .. } => data.len() as u64,
            FileExtent::Regular { length, .. } => *length,
        }
    }

    fn compressed(&self) -> bool {
        match self {
            FileExtent::Inline { compression, .. } | FileExtent::Regular { compression, .. } => {
                *compression != 0
            }
        }
    }
}

/// What a subvolume's tree holds.
#[derive(Default)]
struct Tree {
    inodes: HashMap<u64, InodeItem>,
    dirs: HashMap<u64, Vec<(u64, DirEntry)>>,
    /// Folder entries by name hash, for folders without index items.
    dir_items: HashMap<u64, Vec<DirEntry>>,
    extents: HashMap<u64, Vec<FileExtent>>,
}

/// A subvolume's tree root, and its times from its root item.
#[derive(Clone, Copy, Debug)]
struct SubvolumeRoot {
    node: u64,
    changed: Option<i64>,
    created: Option<i64>,
}

pub(crate) struct Btrfs {
    volume: Volume,
    info: VolumeInfo,
    node_size: u64,
    device: u64,
    chunks: Vec<Chunk>,
    /// Subvolume id → its tree's root node.
    roots: HashMap<u64, SubvolumeRoot>,
    trees: Mutex<HashMap<u64, Arc<Result<Tree, String>>>>,
}

fn timespec(data: &[u8], at: usize) -> Option<i64> {
    let seconds = i64::try_from(u64_at(data, at)?).ok()?;
    (seconds != 0).then_some(seconds)
}

impl Btrfs {
    pub fn open(volume: Volume) -> Result<Btrfs, FsError> {
        Btrfs::open_with(volume, None)
    }

    /// Opens with `superblock` (4096 bytes, a backup copy) instead of the
    /// one 64 KiB in.
    pub fn open_with(volume: Volume, superblock: Option<Vec<u8>>) -> Result<Btrfs, FsError> {
        let sb = match superblock {
            Some(sb) => sb,
            None => volume.read_vec(SUPERBLOCK, 4096)?,
        };
        let damaged =
            |what: &str| FsError::damaged(format!("The btrfs superblock is damaged: {what}."));
        if sb.get(0x40..0x48) != Some(b"_BHRfS_M") {
            return Err(damaged("its signature is missing"));
        }
        if u16_at(&sb, 0xC4) == Some(0)
            && u32_at(&sb, 0) != Some(crc32c(sb.get(0x20..4096).unwrap_or(&[])))
        {
            return Err(damaged("its checksum is wrong"));
        }
        let node_size = u64::from(u32_at(&sb, 0x94).unwrap_or(0));
        let sector_size = u64::from(u32_at(&sb, 0x90).unwrap_or(0));
        if !node_size.is_power_of_two() || !(4096..=65536).contains(&node_size) {
            return Err(damaged("its node size is impossible"));
        }
        if !sector_size.is_power_of_two() || !(512..=65536).contains(&sector_size) {
            return Err(damaged("its sector size is impossible"));
        }
        let total = u64_at(&sb, 0x70).unwrap_or(0);
        let mut btrfs = Btrfs {
            info: VolumeInfo {
                kind: FsKind::Btrfs,
                name: "btrfs".to_owned(),
                label: bytes(&sb, 0x12B, 256).and_then(text),
                offset: volume.start(),
                size: total.min(volume.size()),
                cluster_size: sector_size,
            },
            volume,
            node_size,
            device: u64_at(&sb, 0xC9).unwrap_or(1),
            chunks: Vec::new(),
            roots: HashMap::new(),
            trees: Mutex::new(HashMap::new()),
        };
        // The system chunks in the superblock, enough to read the chunk tree.
        let array_size = usize::try_from(u32_at(&sb, 0xA0).unwrap_or(0).min(2048)).unwrap_or(0);
        let array = sb.get(0x32B..0x32B + array_size).unwrap_or(&[]);
        let mut at = 0;
        while at + 17 <= array.len() {
            let Some((_, kind, logical)) = key_at(array, at) else {
                break;
            };
            if kind != CHUNK_ITEM {
                break;
            }
            let Some((chunk, size)) = array
                .get(at + 17..)
                .and_then(|rest| parse_chunk(logical, rest))
            else {
                break;
            };
            btrfs.chunks.push(chunk);
            at += 17 + size;
        }
        if btrfs.chunks.is_empty() {
            return Err(damaged("it lists no system chunks"));
        }
        let chunk_root = u64_at(&sb, 0x58).unwrap_or(0);
        let mut chunks = Vec::new();
        btrfs.walk(chunk_root, &mut |key, data| {
            if key.1 == CHUNK_ITEM
                && let Some((chunk, _)) = parse_chunk(key.2, data)
            {
                chunks.push(chunk);
            }
        })?;
        for chunk in chunks {
            if !btrfs.chunks.iter().any(|c| c.logical == chunk.logical) {
                btrfs.chunks.push(chunk);
            }
        }
        btrfs.chunks.sort_by_key(|c| c.logical);
        let root = u64_at(&sb, 0x50).unwrap_or(0);
        let mut roots = HashMap::new();
        btrfs.walk(root, &mut |key, data| {
            let is_subvolume = key.0 == TOP_TREE || (256..u64::MAX - 255).contains(&key.0);
            if key.1 == ROOT_ITEM
                && is_subvolume
                && let Some(node) = u64_at(data, 176)
            {
                // Newer root items end with the subvolume's times.
                roots.insert(
                    key.0,
                    SubvolumeRoot {
                        node,
                        changed: timespec(data, 327),
                        created: timespec(data, 339),
                    },
                );
            }
        })?;
        if !roots.contains_key(&TOP_TREE) {
            return Err(FsError::damaged(
                "The btrfs root tree has no file-system tree.",
            ));
        }
        btrfs.roots = roots;
        Ok(btrfs)
    }

    /// Where a logical address is on this disk.
    fn physical(&self, logical: u64) -> Result<u64, FsError> {
        let index = self.chunks.partition_point(|c| c.logical <= logical);
        let chunk = index
            .checked_sub(1)
            .and_then(|i| self.chunks.get(i))
            .filter(|c| logical - c.logical < c.length)
            .ok_or_else(|| {
                FsError::damaged(format!("btrfs address {logical} isn't in any chunk."))
            })?;
        if chunk.kind & STRIPED != 0 && chunk.stripes.len() > 1 {
            return Err(FsError::Unsupported(
                "This btrfs data is striped across several disks (RAID), which isn't \
                 supported yet."
                    .to_owned(),
            ));
        }
        let (_, offset) = chunk
            .stripes
            .iter()
            .find(|(device, _)| *device == self.device)
            .ok_or_else(|| {
                FsError::Unsupported(
                    "This part of the btrfs file system is on another of its disks.".to_owned(),
                )
            })?;
        Ok(offset + (logical - chunk.logical))
    }

    fn read_node(&self, logical: u64) -> Result<Vec<u8>, FsError> {
        let node = self
            .volume
            .read_vec(self.physical(logical)?, self.node_size)?;
        if u64_at(&node, 48) != Some(logical) {
            return Err(FsError::damaged(format!(
                "The btrfs tree node at {logical} isn't there."
            )));
        }
        Ok(node)
    }

    /// Visits every leaf item of a tree: its key and its data.
    fn walk(&self, root: u64, visit: &mut dyn FnMut(Key, &[u8])) -> Result<(), FsError> {
        let mut stack = vec![root];
        let mut seen = HashSet::new();
        let mut first = true;
        while let Some(logical) = stack.pop() {
            if !seen.insert(logical) || seen.len() > MAX_NODES {
                continue;
            }
            let node = match self.read_node(logical) {
                Ok(node) => node,
                // The root must be readable; a lost node elsewhere loses
                // only what it holds.
                Err(error) if first => return Err(error),
                Err(_) => continue,
            };
            first = false;
            let count = usize::try_from(u32_at(&node, 96).unwrap_or(0)).unwrap_or(0);
            let level = u8_at(&node, 100).unwrap_or(0);
            if level == 0 {
                for i in 0..count {
                    let at = HEADER + i * ITEM;
                    let (Some(key), Some(offset), Some(size)) = (
                        key_at(&node, at),
                        u32_at(&node, at + 17),
                        u32_at(&node, at + 21),
                    ) else {
                        break;
                    };
                    let start = HEADER + usize::try_from(offset).unwrap_or(usize::MAX);
                    let data = start
                        .checked_add(usize::try_from(size).unwrap_or(usize::MAX))
                        .and_then(|end| node.get(start..end));
                    if let Some(data) = data {
                        visit(key, data);
                    }
                }
            } else {
                let mut children = Vec::new();
                for i in 0..count {
                    if let Some(child) = u64_at(&node, HEADER + i * KEY_PTR + 17) {
                        children.push(child);
                    }
                }
                stack.extend(children.into_iter().rev());
            }
        }
        Ok(())
    }

    /// A subvolume's tree, read the first time it's needed.
    fn tree(&self, id: u64) -> Result<Arc<Result<Tree, String>>, FsError> {
        if let Some(tree) = self.lock().get(&id) {
            return Ok(Arc::clone(tree));
        }
        let root = self
            .roots
            .get(&id)
            .ok_or_else(|| FsError::NotFound(format!("Subvolume {id}")))?;
        let tree = Arc::new(self.read_tree(root.node).map_err(|e| e.to_string()));
        self.lock().insert(id, Arc::clone(&tree));
        Ok(tree)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Arc<Result<Tree, String>>>> {
        self.trees.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read_tree(&self, root: u64) -> Result<Tree, FsError> {
        let mut tree = Tree::default();
        self.walk(root, &mut |key, data| match key.1 {
            INODE_ITEM => {
                if let (Some(size), Some(mode)) = (u64_at(data, 16), u32_at(data, 52)) {
                    tree.inodes.insert(
                        key.0,
                        InodeItem {
                            size,
                            mode,
                            accessed: timespec(data, 112),
                            modified: timespec(data, 136),
                            created: timespec(data, 148),
                        },
                    );
                }
            }
            DIR_INDEX => {
                if let Some(entry) = dir_entries(data).into_iter().next() {
                    tree.dirs.entry(key.0).or_default().push((key.2, entry));
                }
            }
            DIR_ITEM => {
                tree.dir_items
                    .entry(key.0)
                    .or_default()
                    .extend(dir_entries(data));
            }
            EXTENT_DATA => {
                if let Some(extent) = parse_extent(key.2, data) {
                    tree.extents.entry(key.0).or_default().push(extent);
                }
            }
            _ => {}
        })?;
        for list in tree.dirs.values_mut() {
            list.sort_by_key(|(index, _)| *index);
        }
        for list in tree.extents.values_mut() {
            list.sort_by_key(FileExtent::offset);
        }
        Ok(tree)
    }

    fn entry(&self, tree_id: u64, tree: &Tree, entry: &DirEntry) -> Entry {
        let (target_tree, inode_id) = if entry.location.1 == ROOT_ITEM {
            (entry.location.0, ROOT_DIR)
        } else {
            (tree_id, entry.location.0)
        };
        let subvolume = target_tree != tree_id;
        let inode = if subvolume {
            let root = self.roots.get(&target_tree);
            InodeItem {
                mode: 0o40755,
                modified: root.and_then(|r| r.changed),
                created: root.and_then(|r| r.created),
                ..InodeItem::default()
            }
        } else {
            tree.inodes.get(&inode_id).cloned().unwrap_or_default()
        };
        let is_dir = inode.mode & 0o170000 == 0o40000;
        let extents = tree.extents.get(&inode_id);
        let compressed = extents.is_some_and(|list| list.iter().any(FileExtent::compressed));
        let mut notes = Vec::new();
        if subvolume {
            notes.push("A subvolume.".to_owned());
        }
        if inode.mode & 0o170000 == 0o120000
            && let Some(FileExtent::Inline { data, .. }) = extents.and_then(|l| l.first())
        {
            notes.push(format!("A link to {}", String::from_utf8_lossy(data)));
        }
        if compressed {
            notes
                .push("Compressed: reading compressed btrfs files isn't supported yet.".to_owned());
        }
        if !subvolume && !tree.inodes.contains_key(&inode_id) {
            notes.push("Its details are missing from the volume.".to_owned());
        }
        let mut attributes = 0;
        if is_dir {
            attributes |= attributes::DIRECTORY;
        }
        if compressed {
            attributes |= attributes::COMPRESSED;
        }
        Entry {
            id: EntryId::Btrfs {
                tree: target_tree,
                inode: inode_id,
            },
            name: entry.name.clone(),
            is_dir,
            size: if is_dir { 0 } else { inode.size },
            deleted: false,
            created: inode.created,
            modified: inode.modified,
            accessed: inode.accessed,
            attributes,
            streams: Vec::new(),
            note: (!notes.is_empty()).then(|| notes.join(" ")),
        }
    }

    fn file(&self, file: &EntryId) -> Result<(Arc<Result<Tree, String>>, u64), FsError> {
        let EntryId::Btrfs { tree, inode } = *file else {
            return Err(FsError::NotFound("That file".to_owned()));
        };
        Ok((self.tree(tree)?, inode))
    }
}

fn readable(tree: &Result<Tree, String>) -> Result<&Tree, FsError> {
    tree.as_ref()
        .map_err(|e| FsError::damaged(format!("This btrfs subvolume can't be read: {e}")))
}

/// The entries of a folder item (several share one when names collide).
fn dir_entries(data: &[u8]) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    let mut at = 0;
    while at + 30 <= data.len() {
        let (Some(location), Some(data_len), Some(name_len)) = (
            key_at(data, at),
            u16_at(data, at + 25),
            u16_at(data, at + 27),
        ) else {
            break;
        };
        let name_len = usize::from(name_len);
        let Some(name) = data.get(at + 30..at + 30 + name_len) else {
            break;
        };
        entries.push(DirEntry {
            name: String::from_utf8_lossy(name).into_owned(),
            location,
        });
        at += 30 + name_len + usize::from(data_len);
    }
    entries
}

fn parse_extent(offset: u64, data: &[u8]) -> Option<FileExtent> {
    let compression = u8_at(data, 16)?;
    match u8_at(data, 20)? {
        0 => Some(FileExtent::Inline {
            offset,
            data: data.get(21..)?.to_vec(),
            compression,
        }),
        kind @ (1 | 2) => Some(FileExtent::Regular {
            offset,
            disk: u64_at(data, 21)?,
            within: u64_at(data, 37)?,
            length: u64_at(data, 45)?,
            compression,
            preallocated: kind == 2,
        }),
        _ => None,
    }
}

impl FileSystem for Btrfs {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn root(&self) -> EntryId {
        EntryId::Btrfs {
            tree: TOP_TREE,
            inode: ROOT_DIR,
        }
    }

    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError> {
        let (tree, inode) = self.file(dir)?;
        let EntryId::Btrfs { tree: tree_id, .. } = *dir else {
            unreachable!()
        };
        let tree = readable(&tree)?;
        if tree
            .inodes
            .get(&inode)
            .is_some_and(|i| i.mode & 0o170000 != 0o40000)
        {
            return Err(FsError::NotFound("That folder".to_owned()));
        }
        let entries: Vec<DirEntry> = match tree.dirs.get(&inode) {
            Some(indexed) => indexed.iter().map(|(_, e)| e.clone()).collect(),
            None => {
                let mut items = tree.dir_items.get(&inode).cloned().unwrap_or_default();
                items.sort_by(|a, b| a.name.cmp(&b.name));
                items
            }
        };
        Ok(entries
            .iter()
            .map(|entry| self.entry(tree_id, tree, entry))
            .collect())
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
                "btrfs files have no named streams; that stream".to_owned(),
            ));
        }
        let (tree, inode_id) = self.file(file)?;
        let tree = readable(&tree)?;
        let inode = tree
            .inodes
            .get(&inode_id)
            .ok_or_else(|| FsError::NotFound("That file".to_owned()))?;
        if offset >= inode.size || buf.is_empty() {
            return Ok(0);
        }
        let n = usize::try_from(inode.size - offset)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let buf = &mut buf[..n];
        let extents = tree.extents.get(&inode_id).map_or(&[][..], Vec::as_slice);
        let mut done = 0;
        while done < n {
            let position = offset + done as u64;
            let left = (n - done) as u64;
            let extent = extents
                .iter()
                .find(|e| position >= e.offset() && position - e.offset() < e.length());
            let take = match extent {
                Some(e) => left.min(e.length() - (position - e.offset())),
                None => {
                    let next = extents
                        .iter()
                        .map(FileExtent::offset)
                        .filter(|&o| o > position)
                        .min()
                        .unwrap_or(u64::MAX);
                    left.min(next - position)
                }
            };
            let take = usize::try_from(take).unwrap_or(n - done).max(1);
            let piece = &mut buf[done..done + take];
            match extent {
                Some(e) if e.compressed() => {
                    return Err(FsError::Unsupported(
                        "This file is compressed; reading compressed btrfs files isn't \
                         supported yet."
                            .to_owned(),
                    ));
                }
                Some(FileExtent::Inline { offset, data, .. }) => {
                    let start = usize::try_from(position - offset).unwrap_or(usize::MAX);
                    for (i, byte) in piece.iter_mut().enumerate() {
                        *byte = data.get(start + i).copied().unwrap_or(0);
                    }
                }
                Some(FileExtent::Regular {
                    offset,
                    disk,
                    within,
                    preallocated,
                    ..
                }) if *disk != 0 && !preallocated => {
                    let logical = disk + within + (position - offset);
                    // A piece may cross into the next chunk: read it in
                    // parts no longer than what's left of this one.
                    let mut copied = 0;
                    while copied < piece.len() {
                        let at = self.physical(logical + copied as u64)?;
                        let index = self
                            .chunks
                            .partition_point(|c| c.logical <= logical + copied as u64);
                        let chunk_left = index
                            .checked_sub(1)
                            .and_then(|i| self.chunks.get(i))
                            .map_or(u64::MAX, |c| {
                                c.logical + c.length - (logical + copied as u64)
                            });
                        let part = usize::try_from(chunk_left)
                            .unwrap_or(usize::MAX)
                            .min(piece.len() - copied);
                        self.volume.read(at, &mut piece[copied..copied + part])?;
                        copied += part;
                    }
                }
                _ => piece.fill(0),
            }
            done += take;
        }
        Ok(n)
    }

    fn extents(&self, file: &EntryId, stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        if stream.is_some() {
            return Err(FsError::NotFound("That stream".to_owned()));
        }
        let (tree, inode_id) = self.file(file)?;
        let tree = readable(&tree)?;
        let size = tree.inodes.get(&inode_id).map_or(0, |i| i.size);
        let base = self.volume.start();
        let mut out = Vec::new();
        let mut covered = 0;
        for e in tree.extents.get(&inode_id).into_iter().flatten() {
            let from = e.offset().min(size);
            let to = e.offset().saturating_add(e.length()).min(size);
            // Inline data lives inside a tree node, not in extents of its own.
            if matches!(e, FileExtent::Inline { .. }) {
                covered = covered.max(to);
                continue;
            }
            if from > covered {
                out.push(Extent {
                    offset: 0,
                    length: from - covered,
                    sparse: true,
                });
            }
            if to > from {
                let place = match e {
                    FileExtent::Regular {
                        disk,
                        within,
                        preallocated: false,
                        compression,
                        ..
                    } if *disk != 0 => {
                        // Compressed data starts where its extent does.
                        let skip = if *compression == 0 { *within } else { 0 };
                        self.physical(disk + skip).ok()
                    }
                    _ => None,
                };
                out.push(Extent {
                    offset: place.map_or(0, |p| base.saturating_add(p)),
                    length: to - from,
                    sparse: place.is_none(),
                });
            }
            covered = covered.max(to);
        }
        if covered < size {
            out.push(Extent {
                offset: 0,
                length: size - covered,
                sparse: true,
            });
        }
        Ok(out)
    }
}
