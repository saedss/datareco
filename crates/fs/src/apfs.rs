//! APFS: a container's volumes, and their folders and files (data, resource
//! forks, hard links, symbolic links, sparse files).
//!
//! The container is shown as a folder per volume. A volume's file-system
//! tree is read whole the first time it's opened, like the MFT on NTFS.
//! Deleted files aren't found yet, and encrypted volumes can't be read.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use unicode_normalization::UnicodeNormalization;

use crate::bytes::{u16_at, u32_at, u64_at};
use crate::error::FsError;
use crate::volume::Volume;
use crate::{Entry, EntryId, Extent, FileSystem, FsKind, StreamInfo, VolumeInfo, attributes};

const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 0x1;
const OBJ_PHYSICAL: u32 = 0x4000_0000;
const BTNODE_ROOT: u16 = 0x1;
const BTNODE_LEAF: u16 = 0x2;
const BTNODE_FIXED_KV_SIZE: u16 = 0x4;
/// Size of the B-tree info at the end of a root node.
const BTREE_INFO: usize = 40;
const OMAP_VAL_DELETED: u32 = 0x1;

const TYPE_INODE: u64 = 3;
const TYPE_XATTR: u64 = 4;
const TYPE_FILE_EXTENT: u64 = 8;
const TYPE_DIR_REC: u64 = 9;
const OBJ_ID_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;

/// A volume's root folder.
const ROOT_DIR: u64 = 2;
const FS_UNENCRYPTED: u64 = 0x1;
const XATTR_DATA_STREAM: u16 = 0x1;
const XATTR_DATA_EMBEDDED: u16 = 0x2;
const INO_EXT_TYPE_DSTREAM: u8 = 8;
const UF_COMPRESSED: u32 = 0x20;
const UF_HIDDEN: u32 = 0x8000;

const RESOURCE_FORK_XATTR: &str = "com.apple.ResourceFork";
const SYMLINK_XATTR: &str = "com.apple.fs.symlink";
/// The resource fork, as a stream name.
const RESOURCE_FORK: &str = "rsrc";

/// More B-tree nodes than this in one tree means it's damaged.
const MAX_NODES: usize = 1 << 24;

/// APFS's Fletcher-64 checksum of an object (stored in its first 8 bytes).
pub(crate) fn fletcher64(block: &[u8]) -> u64 {
    let modulus = u64::from(u32::MAX);
    let (mut sum1, mut sum2) = (0u64, 0u64);
    for word in block.get(8..).unwrap_or(&[]).as_chunks::<4>().0 {
        sum1 = (sum1 + u64::from(u32::from_le_bytes(*word))) % modulus;
        sum2 = (sum2 + sum1) % modulus;
    }
    let check1 = modulus - ((sum1 + sum2) % modulus);
    let check2 = modulus - ((sum1 + check1) % modulus);
    (check2 << 32) | check1
}

fn checksum_ok(block: &[u8]) -> bool {
    u64_at(block, 0).is_some_and(|stored| stored == fletcher64(block))
}

/// One node of a B-tree, and how to find its keys and values.
struct Node<'a> {
    data: &'a [u8],
    flags: u16,
    count: usize,
    toc: usize,
    keys: usize,
    values_end: usize,
}

impl<'a> Node<'a> {
    fn parse(data: &'a [u8]) -> Option<Node<'a>> {
        let flags = u16_at(data, 32)?;
        let count = usize::try_from(u32_at(data, 36)?).ok()?;
        let toc_offset = usize::from(u16_at(data, 40)?);
        let toc_length = usize::from(u16_at(data, 42)?);
        let toc = 56usize.checked_add(toc_offset)?;
        let keys = toc.checked_add(toc_length)?;
        let values_end = if flags & BTNODE_ROOT != 0 {
            data.len().checked_sub(BTREE_INFO)?
        } else {
            data.len()
        };
        (keys <= values_end && count <= data.len()).then_some(Node {
            data,
            flags,
            count,
            toc,
            keys,
            values_end,
        })
    }

    fn leaf(&self) -> bool {
        self.flags & BTNODE_LEAF != 0
    }

    /// The fixed key and value sizes, from a root node's B-tree info.
    fn fixed_sizes(&self) -> Option<(usize, usize)> {
        let info = self.data.len().checked_sub(BTREE_INFO)?;
        let key = usize::try_from(u32_at(self.data, info + 8)?).ok()?;
        let value = usize::try_from(u32_at(self.data, info + 12)?).ok()?;
        Some((key, value))
    }

    /// Entry `i`: its key, and its value (`None` if it has none).
    fn entry(
        &self,
        i: usize,
        fixed: Option<(usize, usize)>,
    ) -> Option<(&'a [u8], Option<&'a [u8]>)> {
        let (key_at, key_len, value_at, value_len) = if self.flags & BTNODE_FIXED_KV_SIZE != 0 {
            let (key_size, value_size) = fixed?;
            let at = self.toc + i * 4;
            let value_size = if self.leaf() { value_size } else { 8 };
            (
                usize::from(u16_at(self.data, at)?),
                key_size,
                usize::from(u16_at(self.data, at + 2)?),
                value_size,
            )
        } else {
            let at = self.toc + i * 8;
            (
                usize::from(u16_at(self.data, at)?),
                usize::from(u16_at(self.data, at + 2)?),
                usize::from(u16_at(self.data, at + 4)?),
                usize::from(u16_at(self.data, at + 6)?),
            )
        };
        let key_start = self.keys.checked_add(key_at)?;
        let key = self.data.get(key_start..key_start.checked_add(key_len)?)?;
        if value_at == 0xFFFF {
            return Some((key, None));
        }
        let value_start = self.values_end.checked_sub(value_at)?;
        let value = self
            .data
            .get(value_start..value_start.checked_add(value_len)?)?;
        Some((key, Some(value)))
    }
}

/// An APFS volume in the container, as its superblock describes it.
struct VolumeHeader {
    name: String,
    role: u16,
    encrypted: bool,
    omap: u64,
    root_tree: u64,
    root_tree_physical: bool,
}

/// What a volume's file-system tree holds.
#[derive(Default)]
struct Tree {
    inodes: HashMap<u64, InodeRec>,
    children: HashMap<u64, Vec<DirRec>>,
    extents: HashMap<u64, Vec<FileExtent>>,
    xattrs: HashMap<u64, Vec<Xattr>>,
}

#[derive(Clone, Debug, Default)]
struct InodeRec {
    stream: u64,
    created: Option<i64>,
    modified: Option<i64>,
    accessed: Option<i64>,
    mode: u16,
    flags: u32,
    size: u64,
}

#[derive(Clone, Debug)]
struct DirRec {
    name: String,
    id: u64,
}

#[derive(Clone, Copy, Debug)]
struct FileExtent {
    logical: u64,
    length: u64,
    /// The first block, or 0 for a hole.
    block: u64,
}

#[derive(Clone, Debug)]
enum XattrData {
    Embedded(Vec<u8>),
    Stream { id: u64, size: u64 },
}

#[derive(Clone, Debug)]
struct Xattr {
    name: String,
    data: XattrData,
}

/// A stream's size, and its bytes when kept in a record, else its extents.
type StreamData = (u64, Option<Vec<u8>>, Vec<FileExtent>);

struct VolumeSlot {
    header: Result<VolumeHeader, String>,
    tree: OnceLock<Result<Tree, String>>,
}

pub(crate) struct Apfs {
    volume: Volume,
    info: VolumeInfo,
    block_size: u64,
    blocks: u64,
    xid: u64,
    volumes: Vec<VolumeSlot>,
}

fn nanoseconds(ns: u64) -> Option<i64> {
    (ns != 0).then(|| i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX))
}

/// A null-terminated UTF-8 name, in composed form.
fn name(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).nfc().collect()
}

impl Apfs {
    pub fn open(volume: Volume) -> Result<Apfs, FsError> {
        let first = volume.read_vec(0, 4096)?;
        if first.get(32..36) != Some(b"NXSB") {
            return Err(FsError::damaged(
                "The APFS container superblock is missing.",
            ));
        }
        let block_size = u64::from(u32_at(&first, 36).unwrap_or(0));
        if !block_size.is_power_of_two() || !(4096..=65536).contains(&block_size) {
            return Err(FsError::damaged("The APFS block size is impossible."));
        }
        let blocks = u64_at(&first, 40).unwrap_or(0);
        let mut apfs = Apfs {
            info: VolumeInfo {
                kind: FsKind::Apfs,
                name: "APFS".to_owned(),
                label: None,
                offset: volume.start(),
                size: block_size.saturating_mul(blocks).min(volume.size()),
                cluster_size: block_size,
            },
            volume,
            block_size,
            blocks,
            xid: 0,
            volumes: Vec::new(),
        };
        let superblock = apfs.latest_superblock()?;
        apfs.xid = u64_at(&superblock, 16).unwrap_or(0);
        let omap = apfs.object_map(u64_at(&superblock, 160).unwrap_or(0))?;
        let max = usize::try_from(u32_at(&superblock, 180).unwrap_or(0).min(100)).unwrap_or(0);
        for i in 0..max {
            let oid = u64_at(&superblock, 184 + i * 8).unwrap_or(0);
            if oid == 0 {
                continue;
            }
            let header = omap
                .get(&oid)
                .ok_or_else(|| "its superblock isn't in the container's object map".to_owned())
                .and_then(|&block| apfs.volume_header(block));
            apfs.volumes.push(VolumeSlot {
                header,
                tree: OnceLock::new(),
            });
        }
        if apfs.volumes.is_empty() {
            return Err(FsError::damaged("The APFS container has no volumes."));
        }
        Ok(apfs)
    }

    fn read_block(&self, block: u64) -> Result<Vec<u8>, FsError> {
        if block == 0 || block >= self.blocks {
            return Err(FsError::damaged(format!(
                "APFS block {block} is outside the container."
            )));
        }
        self.volume
            .read_vec(block * self.block_size, self.block_size)
    }

    /// The newest valid container superblock: block 0, or one of the copies
    /// in the checkpoint area.
    fn latest_superblock(&self) -> Result<Vec<u8>, FsError> {
        let first = self.volume.read_vec(0, self.block_size)?;
        let mut best = checksum_ok(&first).then(|| first.clone());
        let base = u64_at(&first, 112).unwrap_or(0);
        let count = u64::from(u32_at(&first, 104).unwrap_or(0) & 0x7FFF_FFFF);
        // A set high bit means the area is described by a tree; not handled.
        if base >> 63 == 0 {
            for i in 0..count.min(1 << 16) {
                let Ok(block) = self.read_block(base + i) else {
                    continue;
                };
                let is_superblock = u32_at(&block, 24)
                    .is_some_and(|t| t & 0xFFFF == OBJECT_TYPE_NX_SUPERBLOCK)
                    && block.get(32..36) == Some(b"NXSB");
                let newer = best
                    .as_ref()
                    .is_none_or(|b| u64_at(&block, 16) > u64_at(b, 16));
                if is_superblock && checksum_ok(&block) && newer {
                    best = Some(block);
                }
            }
        }
        Ok(best.unwrap_or(first))
    }

    /// An object map: virtual object id → block, as of the container's
    /// newest transaction.
    fn object_map(&self, omap: u64) -> Result<HashMap<u64, u64>, FsError> {
        let header = self.read_block(omap)?;
        let tree = u64_at(&header, 48).unwrap_or(0);
        let mut newest: HashMap<u64, (u64, u64)> = HashMap::new();
        self.walk(tree, None, &mut |key, value| {
            let (Some(oid), Some(xid)) = (u64_at(key, 0), u64_at(key, 8)) else {
                return;
            };
            let (Some(flags), Some(block)) = (u32_at(value, 0), u64_at(value, 8)) else {
                return;
            };
            if xid > self.xid || flags & OMAP_VAL_DELETED != 0 {
                return;
            }
            let slot = newest.entry(oid).or_insert((xid, block));
            if xid >= slot.0 {
                *slot = (xid, block);
            }
        })?;
        Ok(newest
            .into_iter()
            .map(|(oid, (_, block))| (oid, block))
            .collect())
    }

    /// Visits every leaf entry of a B-tree. `omap` turns child ids into
    /// blocks for virtual trees; `None` for physical ones.
    fn walk(
        &self,
        root: u64,
        omap: Option<&HashMap<u64, u64>>,
        visit: &mut dyn FnMut(&[u8], &[u8]),
    ) -> Result<(), FsError> {
        let resolve = |oid: u64| match omap {
            Some(map) => map.get(&oid).copied(),
            None => Some(oid),
        };
        let root_block =
            resolve(root).ok_or_else(|| FsError::damaged("A B-tree's root is missing."))?;
        let root_node = self.read_block(root_block)?;
        let fixed = Node::parse(&root_node).and_then(|n| n.fixed_sizes());
        let mut stack = vec![root_block];
        let mut seen = HashSet::new();
        while let Some(block) = stack.pop() {
            if !seen.insert(block) || seen.len() > MAX_NODES {
                continue;
            }
            let Ok(data) = self.read_block(block) else {
                continue;
            };
            let Some(node) = Node::parse(&data) else {
                continue;
            };
            let mut children = Vec::new();
            for i in 0..node.count {
                let Some((key, Some(value))) = node.entry(i, fixed) else {
                    continue;
                };
                if node.leaf() {
                    visit(key, value);
                } else if let Some(child) = u64_at(value, 0).and_then(resolve) {
                    children.push(child);
                }
            }
            // Visit children in order.
            stack.extend(children.into_iter().rev());
        }
        Ok(())
    }

    fn volume_header(&self, block: u64) -> Result<VolumeHeader, String> {
        let data = self.read_block(block).map_err(|e| e.to_string())?;
        if data.get(32..36) != Some(b"APSB") {
            return Err("its superblock is damaged".to_owned());
        }
        let field = |at| u64_at(&data, at).unwrap_or(0);
        Ok(VolumeHeader {
            name: name(data.get(704..960).unwrap_or(&[])),
            role: u16_at(&data, 964).unwrap_or(0),
            encrypted: field(264) & FS_UNENCRYPTED == 0,
            omap: field(128),
            root_tree: field(136),
            root_tree_physical: u32_at(&data, 116).is_some_and(|t| t & OBJ_PHYSICAL != 0),
        })
    }

    /// A volume's file-system tree, read the first time it's needed.
    fn tree(&self, index: u32) -> Result<&Tree, FsError> {
        let slot = usize::try_from(index)
            .ok()
            .and_then(|i| self.volumes.get(i))
            .ok_or_else(|| FsError::NotFound("That volume".to_owned()))?;
        let header = slot
            .header
            .as_ref()
            .map_err(|e| FsError::damaged(format!("This APFS volume can't be read: {e}.")))?;
        if header.encrypted {
            return Err(FsError::Unsupported(
                "This APFS volume is encrypted; reading encrypted volumes isn't supported yet."
                    .to_owned(),
            ));
        }
        slot.tree
            .get_or_init(|| self.read_tree(header).map_err(|e| e.to_string()))
            .as_ref()
            .map_err(|e| FsError::damaged(format!("This APFS volume can't be read: {e}")))
    }

    fn read_tree(&self, header: &VolumeHeader) -> Result<Tree, FsError> {
        let omap = self.object_map(header.omap)?;
        let mut tree = Tree::default();
        let omap = (!header.root_tree_physical).then_some(&omap);
        self.walk(header.root_tree, omap, &mut |key, value| {
            let Some(id_and_type) = u64_at(key, 0) else {
                return;
            };
            let id = id_and_type & OBJ_ID_MASK;
            match id_and_type >> 60 {
                TYPE_INODE => {
                    if let Some(inode) = parse_inode(value) {
                        tree.inodes.insert(id, inode);
                    }
                }
                TYPE_DIR_REC => {
                    if let Some(record) = parse_dir_rec(key, value) {
                        tree.children.entry(id).or_default().push(record);
                    }
                }
                TYPE_FILE_EXTENT => {
                    let (Some(logical), Some(length), Some(block)) =
                        (u64_at(key, 8), u64_at(value, 0), u64_at(value, 8))
                    else {
                        return;
                    };
                    tree.extents.entry(id).or_default().push(FileExtent {
                        logical,
                        length: length & 0x00FF_FFFF_FFFF_FFFF,
                        block,
                    });
                }
                TYPE_XATTR => {
                    if let Some(xattr) = parse_xattr(key, value) {
                        tree.xattrs.entry(id).or_default().push(xattr);
                    }
                }
                _ => {}
            }
        })?;
        for list in tree.extents.values_mut() {
            list.sort_by_key(|e| e.logical);
        }
        for list in tree.children.values_mut() {
            list.sort_by_key(|record| record.name.to_lowercase());
        }
        Ok(tree)
    }

    fn xattr<'t>(tree: &'t Tree, id: u64, wanted: &str) -> Option<&'t Xattr> {
        tree.xattrs.get(&id)?.iter().find(|x| x.name == wanted)
    }

    fn entry(&self, volume: u32, tree: &Tree, record: &DirRec) -> Entry {
        let inode = tree.inodes.get(&record.id).cloned().unwrap_or_default();
        let is_dir = inode.mode & 0xF000 == 0x4000;
        let mut notes = Vec::new();
        if inode.mode & 0xF000 == 0xA000
            && let Some(Xattr {
                data: XattrData::Embedded(target),
                ..
            }) = Self::xattr(tree, record.id, SYMLINK_XATTR)
        {
            notes.push(format!("A link to {}", name(target)));
        }
        if inode.flags & UF_COMPRESSED != 0 {
            notes.push(
                "Compressed by macOS: reading compressed files isn't supported yet.".to_owned(),
            );
        }
        if !tree.inodes.contains_key(&record.id) {
            notes.push("Its details are missing from the volume.".to_owned());
        }
        let mut attributes = 0;
        if is_dir {
            attributes |= attributes::DIRECTORY;
        }
        if inode.flags & UF_HIDDEN != 0 {
            attributes |= attributes::HIDDEN;
        }
        if inode.flags & UF_COMPRESSED != 0 {
            attributes |= attributes::COMPRESSED;
        }
        let streams = match Self::xattr(tree, record.id, RESOURCE_FORK_XATTR) {
            Some(Xattr { data, .. }) if !is_dir => vec![StreamInfo {
                name: RESOURCE_FORK.to_owned(),
                size: match data {
                    XattrData::Embedded(bytes) => bytes.len() as u64,
                    XattrData::Stream { size, .. } => *size,
                },
            }],
            _ => Vec::new(),
        };
        Entry {
            id: EntryId::Apfs {
                volume,
                id: record.id,
            },
            name: record.name.clone(),
            is_dir,
            size: if is_dir { 0 } else { inode.size },
            deleted: false,
            created: inode.created,
            modified: inode.modified,
            accessed: inode.accessed,
            attributes,
            streams,
            note: (!notes.is_empty()).then(|| notes.join(" ")),
        }
    }

    /// The data a read covers: embedded bytes, or (size, extents).
    fn data_of(&self, file: &EntryId, stream: Option<&str>) -> Result<StreamData, FsError> {
        let EntryId::Apfs { volume, id } = *file else {
            return Err(FsError::NotFound("That file".to_owned()));
        };
        let tree = self.tree(volume)?;
        let inode = tree
            .inodes
            .get(&id)
            .ok_or_else(|| FsError::NotFound("That file".to_owned()))?;
        match stream {
            None => Ok((
                inode.size,
                None,
                tree.extents.get(&inode.stream).cloned().unwrap_or_default(),
            )),
            Some(RESOURCE_FORK) => {
                match Self::xattr(tree, id, RESOURCE_FORK_XATTR).map(|x| &x.data) {
                    Some(XattrData::Embedded(bytes)) => {
                        Ok((bytes.len() as u64, Some(bytes.clone()), Vec::new()))
                    }
                    Some(XattrData::Stream { id, size }) => Ok((
                        *size,
                        None,
                        tree.extents.get(id).cloned().unwrap_or_default(),
                    )),
                    None => Err(FsError::NotFound("The resource fork".to_owned())),
                }
            }
            Some(other) => Err(FsError::NotFound(format!("The stream {other}"))),
        }
    }
}

fn parse_inode(value: &[u8]) -> Option<InodeRec> {
    let mut inode = InodeRec {
        stream: u64_at(value, 8)?,
        created: nanoseconds(u64_at(value, 16)?),
        modified: nanoseconds(u64_at(value, 24)?),
        accessed: nanoseconds(u64_at(value, 40)?),
        flags: u32_at(value, 68)?,
        mode: u16_at(value, 80)?,
        size: 0,
    };
    // Extended fields: a count, then a 4-byte header for each, then their
    // data, each padded to 8 bytes.
    let count = usize::from(u16_at(value, 92).unwrap_or(0));
    let mut data = 92 + 4 + count * 4;
    for i in 0..count {
        let header = 96 + i * 4;
        let (Some(&kind), Some(size)) = (value.get(header), u16_at(value, header + 2)) else {
            break;
        };
        if kind == INO_EXT_TYPE_DSTREAM {
            inode.size = u64_at(value, data).unwrap_or(0);
        }
        data += usize::from(size).next_multiple_of(8);
    }
    Some(inode)
}

fn parse_dir_rec(key: &[u8], value: &[u8]) -> Option<DirRec> {
    // Hashed keys (case- or normalization-insensitive volumes) store the
    // name's length with a hash; plain keys store just the length.
    let hashed = u32_at(key, 8).map(|v| usize::try_from(v & 0x3FF).unwrap_or(0));
    let raw = match hashed {
        Some(length) if key.len() == 12 + length => key.get(12..)?,
        _ => {
            let length = usize::from(u16_at(key, 8)?);
            (key.len() == 10 + length).then(|| key.get(10..))??
        }
    };
    Some(DirRec {
        name: name(raw),
        id: u64_at(value, 0)?,
    })
}

fn parse_xattr(key: &[u8], value: &[u8]) -> Option<Xattr> {
    let length = usize::from(u16_at(key, 8)?);
    let xattr_name = name(key.get(10..10 + length)?);
    let flags = u16_at(value, 0)?;
    let data_len = usize::from(u16_at(value, 2)?);
    let data = value.get(4..4 + data_len)?;
    let data = if flags & XATTR_DATA_EMBEDDED != 0 {
        XattrData::Embedded(data.to_vec())
    } else if flags & XATTR_DATA_STREAM != 0 {
        XattrData::Stream {
            id: u64_at(data, 0)?,
            size: u64_at(data, 8)?,
        }
    } else {
        return None;
    };
    Some(Xattr {
        name: xattr_name,
        data,
    })
}

fn role_name(role: u16) -> Option<&'static str> {
    Some(match role {
        0x1 => "System",
        0x2 => "User",
        0x4 => "Recovery",
        0x8 => "VM",
        0x10 => "Preboot",
        0x20 => "Installer",
        0x40 => "Data",
        _ => return None,
    })
}

impl FileSystem for Apfs {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    /// The container: a folder per volume.
    fn root(&self) -> EntryId {
        EntryId::Group { index: 0 }
    }

    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError> {
        match *dir {
            EntryId::Group { index: 0 } => Ok(self
                .volumes
                .iter()
                .enumerate()
                .map(|(i, slot)| {
                    let index = u32::try_from(i).unwrap_or(u32::MAX);
                    let (name, note) = match &slot.header {
                        Ok(header) => {
                            let mut note = role_name(header.role)
                                .map(|role| format!("APFS volume ({role})."))
                                .unwrap_or_else(|| "APFS volume.".to_owned());
                            if header.encrypted {
                                note.push_str(" Encrypted: it can't be read without its password.");
                            }
                            let name = if header.name.is_empty() {
                                format!("Volume {}", i + 1)
                            } else {
                                header.name.clone()
                            };
                            (name, note)
                        }
                        Err(problem) => (
                            format!("Volume {}", i + 1),
                            format!("This APFS volume can't be read: {problem}."),
                        ),
                    };
                    Entry {
                        id: EntryId::Apfs {
                            volume: index,
                            id: ROOT_DIR,
                        },
                        name,
                        is_dir: true,
                        size: 0,
                        deleted: false,
                        created: None,
                        modified: None,
                        accessed: None,
                        attributes: attributes::DIRECTORY,
                        streams: Vec::new(),
                        note: Some(note),
                    }
                })
                .collect()),
            EntryId::Apfs { volume, id } => {
                let tree = self.tree(volume)?;
                if tree
                    .inodes
                    .get(&id)
                    .is_some_and(|i| i.mode & 0xF000 != 0x4000)
                {
                    return Err(FsError::NotFound("That folder".to_owned()));
                }
                Ok(tree
                    .children
                    .get(&id)
                    .into_iter()
                    .flatten()
                    .map(|record| self.entry(volume, tree, record))
                    .collect())
            }
            _ => Err(FsError::NotFound("That folder".to_owned())),
        }
    }

    fn read(
        &self,
        file: &EntryId,
        stream: Option<&str>,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let (size, embedded, extents) = self.data_of(file, stream)?;
        if offset >= size || buf.is_empty() {
            return Ok(0);
        }
        let n = usize::try_from(size - offset)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let buf = &mut buf[..n];
        if let Some(bytes) = embedded {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            for (i, byte) in buf.iter_mut().enumerate() {
                *byte = bytes.get(start + i).copied().unwrap_or(0);
            }
            return Ok(n);
        }
        let mut done = 0;
        while done < n {
            let position = offset + done as u64;
            let left = (n - done) as u64;
            let extent = extents
                .iter()
                .find(|e| position >= e.logical && position - e.logical < e.length);
            let (take, at) = match extent {
                Some(e) => {
                    let take = left.min(e.length - (position - e.logical));
                    let at = (e.block != 0).then(|| {
                        e.block
                            .saturating_mul(self.block_size)
                            .saturating_add(position - e.logical)
                    });
                    (take, at)
                }
                None => {
                    let next = extents
                        .iter()
                        .map(|e| e.logical)
                        .filter(|&l| l > position)
                        .min()
                        .unwrap_or(u64::MAX);
                    (left.min(next - position), None)
                }
            };
            let take = usize::try_from(take).unwrap_or(n - done).max(1);
            let piece = &mut buf[done..done + take];
            match at {
                Some(at) => {
                    if at.saturating_add(take as u64) > self.blocks * self.block_size {
                        return Err(FsError::damaged(
                            "A file's extents point past the end of the container.",
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

    fn extents(&self, file: &EntryId, stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        let (size, embedded, extents) = self.data_of(file, stream)?;
        if embedded.is_some() {
            return Ok(Vec::new());
        }
        let base = self.volume.start();
        let mut out = Vec::new();
        let mut covered = 0;
        for e in &extents {
            let from = e.logical.min(size);
            let to = e.logical.saturating_add(e.length).min(size);
            if from > covered {
                out.push(Extent {
                    offset: 0,
                    length: from - covered,
                    sparse: true,
                });
            }
            if to > from {
                out.push(Extent {
                    offset: if e.block == 0 {
                        0
                    } else {
                        base.saturating_add(e.block.saturating_mul(self.block_size))
                    },
                    length: to - from,
                    sparse: e.block == 0,
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
