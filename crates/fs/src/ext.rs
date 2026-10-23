//! ext2, ext3 and ext4: folders, files (extent trees and classic block
//! maps), symbolic links, and deleted entries.
//!
//! Deleted entries come from the slack space of folder blocks: removing an
//! entry only makes the one before it longer, so the name stays. ext2 keeps
//! a deleted file's size and blocks in its inode. ext3 and ext4 clear them,
//! so DataReco looks for an older copy of the inode in the journal.

mod inode;
mod journal;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::bytes::{bytes, text, u8_at, u16_at, u32_at};
use crate::error::FsError;
use crate::volume::Volume;
use crate::{Entry, EntryId, Extent, FileSystem, FsKind, VolumeInfo, attributes};

use inode::{BlockReader, ENCRYPT_FL, INDEX_FL, Inode, Kind, Run};
use journal::{Journal, Layout};

/// The root folder's inode.
const ROOT: u64 = 2;

const COMPAT_HAS_JOURNAL: u32 = 0x4;
const INCOMPAT_COMPRESSION: u32 = 0x1;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_JOURNAL_DEV: u32 = 0x8;
const INCOMPAT_META_BG: u32 = 0x10;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const RO_COMPAT_SPARSE_SUPER: u32 = 0x1;

/// Folders bigger than this are damage.
const MAX_DIR_SIZE: u64 = 64 << 20;
/// File maps kept for reading, before the cache starts over.
const MAP_CACHE: usize = 256;

/// An ext superblock: the primary copy or a backup.
#[derive(Clone, Debug)]
pub(crate) struct Superblock {
    pub kind: FsKind,
    pub block_size: u64,
    pub blocks: u64,
    pub first_data_block: u64,
    pub blocks_per_group: u64,
    pub inodes_per_group: u64,
    pub inodes: u64,
    pub inode_size: u64,
    pub desc_size: u64,
    /// The block group this copy is in (0 for the primary).
    pub group: u64,
    pub compat: u32,
    pub incompat: u32,
    pub ro_compat: u32,
    pub label: Option<String>,
    pub journal_inode: u64,
    pub first_meta_bg: u64,
}

impl Superblock {
    pub fn groups(&self) -> u64 {
        (self.blocks - self.first_data_block).div_ceil(self.blocks_per_group)
    }

    fn group_start(&self, group: u64) -> u64 {
        self.first_data_block
            .saturating_add(group.saturating_mul(self.blocks_per_group))
    }

    /// Whether a block group starts with a copy of the superblock.
    fn has_superblock(&self, group: u64) -> bool {
        if self.ro_compat & RO_COMPAT_SPARSE_SUPER == 0 || group <= 1 {
            return true;
        }
        [3u64, 5, 7].iter().any(|&base| {
            let mut power = base;
            while power < group {
                power = power.saturating_mul(base);
            }
            power == group
        })
    }

    /// Where group-descriptor block `index` is, reading the copy that goes
    /// with this superblock.
    fn descriptor_block(&self, index: u64) -> u64 {
        let per_block = self.block_size / self.desc_size;
        if self.incompat & INCOMPAT_META_BG != 0 && index >= self.first_meta_bg {
            let group = index.saturating_mul(per_block);
            self.group_start(group) + u64::from(self.has_superblock(group))
        } else {
            self.group_start(self.group) + 1 + index
        }
    }
}

/// Parses a superblock (1024 bytes).
pub(crate) fn parse_superblock(sb: &[u8]) -> Result<Superblock, FsError> {
    let damaged = |what: &str| FsError::damaged(format!("The ext superblock is damaged: {what}."));
    let field = |value: Option<u32>| value.ok_or_else(|| damaged("it is cut short"));
    if u16_at(sb, 56) != Some(0xEF53) {
        return Err(damaged("its signature is missing"));
    }
    let log_block_size = field(u32_at(sb, 24))?;
    if log_block_size > 6 {
        return Err(damaged("its block size is impossible"));
    }
    let block_size = 1024u64 << log_block_size;
    let compat = field(u32_at(sb, 92))?;
    let incompat = field(u32_at(sb, 96))?;
    let ro_compat = field(u32_at(sb, 100))?;
    if incompat & INCOMPAT_JOURNAL_DEV != 0 {
        return Err(FsError::Unsupported(
            "This is an ext journal kept on its own device, not a file system.".to_owned(),
        ));
    }
    if incompat & INCOMPAT_COMPRESSION != 0 {
        return Err(FsError::Unsupported(
            "Compressed ext2 volumes aren't supported.".to_owned(),
        ));
    }
    let blocks_high = if incompat & INCOMPAT_64BIT != 0 {
        u64::from(u32_at(sb, 0x150).unwrap_or(0))
    } else {
        0
    };
    let blocks = (blocks_high << 32) | u64::from(field(u32_at(sb, 4))?);
    let first_data_block = u64::from(field(u32_at(sb, 20))?);
    let blocks_per_group = u64::from(field(u32_at(sb, 32))?);
    let inodes_per_group = u64::from(field(u32_at(sb, 40))?);
    let revision = field(u32_at(sb, 76))?;
    let inode_size = if revision == 0 {
        128
    } else {
        u64::from(u16_at(sb, 88).ok_or_else(|| damaged("it is cut short"))?)
    };
    let desc_size = if incompat & INCOMPAT_64BIT != 0 {
        u64::from(u16_at(sb, 254).unwrap_or(0))
    } else {
        32
    };
    let bits = block_size * 8;
    if first_data_block > 1 || blocks <= first_data_block {
        return Err(damaged("its block count is impossible"));
    }
    if blocks_per_group == 0 || blocks_per_group > bits {
        return Err(damaged("its blocks per group are impossible"));
    }
    if inodes_per_group == 0 || inodes_per_group > bits {
        return Err(damaged("its inodes per group are impossible"));
    }
    if !inode_size.is_power_of_two() || !(128..=block_size).contains(&inode_size) {
        return Err(damaged("its inode size is impossible"));
    }
    if !desc_size.is_power_of_two() || !(32..=block_size).contains(&desc_size) {
        return Err(damaged("its group descriptor size is impossible"));
    }
    let kind = if incompat & (INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG) != 0 {
        FsKind::Ext4
    } else if compat & COMPAT_HAS_JOURNAL != 0 {
        FsKind::Ext3
    } else {
        FsKind::Ext2
    };
    let superblock = Superblock {
        kind,
        block_size,
        blocks,
        first_data_block,
        blocks_per_group,
        inodes_per_group,
        inodes: u64::from(field(u32_at(sb, 0))?),
        inode_size,
        desc_size,
        group: u64::from(u16_at(sb, 90).unwrap_or(0)),
        compat,
        incompat,
        ro_compat,
        label: bytes(sb, 120, 16).and_then(text),
        journal_inode: u64::from(u32_at(sb, 224).unwrap_or(0)),
        first_meta_bg: u64::from(u32_at(sb, 260).unwrap_or(0)),
    };
    if superblock.groups() > 1 << 24 {
        return Err(damaged("it has too many block groups"));
    }
    Ok(superblock)
}

/// Where a file's data is.
#[derive(Debug, Default)]
struct FileMap {
    size: u64,
    runs: Vec<Run>,
    /// Data kept in the inode itself (short symbolic links, inline files).
    inline: Option<Vec<u8>>,
}

impl FileMap {
    /// The run holding logical block `block`, or the first one after it.
    fn run_from(&self, block: u64) -> Option<&Run> {
        let index = self.runs.partition_point(|r| r.logical + r.length <= block);
        self.runs.get(index)
    }
}

/// Where a listed inode's details came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    /// The inode as it is now.
    Live,
    /// A deleted inode that still says where its data was (ext2).
    DeletedOnDisk,
    /// An older copy from the journal.
    Journal,
    /// Deleted, and its data map was cleared.
    Cleared,
    /// Deleted, and its inode is used by another file now.
    Reused,
    /// The folder entry lost its inode number too.
    NoNumber,
}

/// The journal, once read.
struct JournalData {
    journal: Journal,
    map: FileMap,
}

pub(crate) struct Ext {
    volume: Volume,
    info: VolumeInfo,
    sb: Superblock,
    /// Each block group's inode table, as a block number (`None` if its
    /// descriptor makes no sense).
    inode_tables: Vec<Option<u64>>,
    journal: OnceLock<Option<JournalData>>,
    maps: Mutex<HashMap<(u64, bool), Arc<FileMap>>>,
}

/// One entry of a folder block.
#[derive(Clone, Debug)]
struct RawEntry {
    inode: u64,
    name: Vec<u8>,
    file_type: u8,
    deleted: bool,
}

impl Ext {
    pub fn open(volume: Volume) -> Result<Ext, FsError> {
        Ext::open_with(volume, None)
    }

    /// Opens the volume with `superblock` (1024 bytes, a backup copy)
    /// instead of its own, and that copy's group descriptors.
    pub fn open_with(volume: Volume, superblock: Option<Vec<u8>>) -> Result<Ext, FsError> {
        let raw = match superblock {
            Some(raw) => raw,
            None => volume.read_vec(1024, 1024)?,
        };
        let sb = parse_superblock(&raw)?;
        let groups = sb.groups();
        let per_block = sb.block_size / sb.desc_size;
        let mut inode_tables = Vec::new();
        let mut index = 0;
        while (inode_tables.len() as u64) < groups {
            let at = sb.descriptor_block(index);
            let block = volume.read_vec(at.saturating_mul(sb.block_size), sb.block_size)?;
            for i in 0..per_block {
                if inode_tables.len() as u64 >= groups {
                    break;
                }
                let start = usize::try_from(i * sb.desc_size).unwrap_or(usize::MAX);
                let descriptor = bytes(&block, start, usize::try_from(sb.desc_size).unwrap_or(0));
                let table = descriptor.and_then(|d| {
                    let low = u64::from(u32_at(d, 8)?);
                    let high = if sb.desc_size >= 64 {
                        u64::from(u32_at(d, 0x28)?)
                    } else {
                        0
                    };
                    Some((high << 32) | low)
                });
                let fits = |t: u64| {
                    let table_blocks =
                        (sb.inodes_per_group * sb.inode_size).div_ceil(sb.block_size);
                    t > 0
                        && t.checked_add(table_blocks)
                            .is_some_and(|end| end <= sb.blocks)
                };
                inode_tables.push(table.filter(|&t| fits(t)));
            }
            index += 1;
        }
        if inode_tables.first().is_none_or(Option::is_none) {
            return Err(FsError::damaged(
                "The first block group's descriptor is damaged, so the root folder can't be found.",
            ));
        }
        let info = VolumeInfo {
            kind: sb.kind,
            name: sb.kind.name().to_owned(),
            label: sb.label.clone(),
            offset: volume.start(),
            size: sb.blocks.saturating_mul(sb.block_size).min(volume.size()),
            cluster_size: sb.block_size,
        };
        let ext = Ext {
            volume,
            info,
            sb,
            inode_tables,
            journal: OnceLock::new(),
            maps: Mutex::new(HashMap::new()),
        };
        let root = ext.read_inode(ROOT)?;
        if root.kind() != Kind::Dir {
            return Err(FsError::damaged("The root folder's inode is damaged."));
        }
        Ok(ext)
    }

    /// Where inode `number` is: its block, and where in the block.
    fn inode_location(&self, number: u64) -> Result<(u64, usize), FsError> {
        if number == 0 || number > self.sb.inodes {
            return Err(FsError::NotFound(format!("Inode {number}")));
        }
        let group = (number - 1) / self.sb.inodes_per_group;
        let index = (number - 1) % self.sb.inodes_per_group;
        let table = usize::try_from(group)
            .ok()
            .and_then(|g| self.inode_tables.get(g).copied().flatten())
            .ok_or_else(|| {
                FsError::damaged(format!(
                    "Block group {group}'s descriptor is damaged, so inode {number} can't be found."
                ))
            })?;
        let byte = index * self.sb.inode_size;
        let within = usize::try_from(byte % self.sb.block_size).unwrap_or(0);
        Ok((table + byte / self.sb.block_size, within))
    }

    fn read_inode(&self, number: u64) -> Result<Inode, FsError> {
        let (block, within) = self.inode_location(number)?;
        let raw = self.volume.read_vec(
            block * self.sb.block_size + within as u64,
            self.sb.inode_size,
        )?;
        Inode::parse(&raw).ok_or_else(|| FsError::damaged(format!("Inode {number} is damaged.")))
    }

    /// The journal, read the first time it's needed.
    fn journal(&self) -> Option<&JournalData> {
        self.journal.get_or_init(|| self.load_journal()).as_ref()
    }

    fn load_journal(&self) -> Option<JournalData> {
        if self.sb.compat & COMPAT_HAS_JOURNAL == 0 || self.sb.journal_inode == 0 {
            return None;
        }
        let inode = self.read_inode(self.sb.journal_inode).ok()?;
        let map = self.file_map(&inode).ok()?;
        let first = self.journal_block(&map, 0)?;
        let layout = Layout::parse(&first, self.sb.block_size)?;
        let journal = Journal::index(layout, |block| self.journal_block(&map, block));
        Some(JournalData { journal, map })
    }

    /// Block `block` of the journal.
    fn journal_block(&self, map: &FileMap, block: u64) -> Option<Vec<u8>> {
        let run = map
            .run_from(block)
            .filter(|r| r.logical <= block && !r.unwritten)?;
        self.read_block(run.physical + (block - run.logical)).ok()
    }

    /// The newest copy of inode `number` in the journal from while it was
    /// in use.
    fn inode_from_journal(&self, number: u64) -> Option<Inode> {
        let data = self.journal()?;
        let (block, within) = self.inode_location(number).ok()?;
        let size = usize::try_from(self.sb.inode_size).ok()?;
        for copy in data.journal.copies(block) {
            let Some(mut old) = self.journal_block(&data.map, copy.journal_block) else {
                continue;
            };
            Journal::unescape(copy, &mut old);
            let Some(inode) = bytes(&old, within, size).and_then(Inode::parse) else {
                continue;
            };
            if inode.live() {
                return Some(inode);
            }
        }
        None
    }

    /// Inode `number` as a listing should show it.
    fn lookup(&self, number: u64, deleted: bool) -> Result<(Option<Inode>, Origin), FsError> {
        if number == 0 {
            return Ok((None, Origin::NoNumber));
        }
        let now = self.read_inode(number)?;
        if !deleted {
            return Ok((Some(now), Origin::Live));
        }
        if now.live() {
            return Ok((None, Origin::Reused));
        }
        if now.has_data_map() && (now.size > 0 || now.kind() == Kind::Dir) {
            return Ok((Some(now), Origin::DeletedOnDisk));
        }
        if let Some(old) = self.inode_from_journal(number) {
            return Ok((Some(old), Origin::Journal));
        }
        Ok((Some(now), Origin::Cleared))
    }

    fn file_map(&self, inode: &Inode) -> Result<FileMap, FsError> {
        let mut map = FileMap {
            size: inode.size,
            ..FileMap::default()
        };
        if inode.fast_symlink() || inode.inline() {
            let length = usize::try_from(inode.size).unwrap_or(usize::MAX);
            if length > inode.block.len() {
                return Err(FsError::Unsupported(
                    "Files kept inside their inode that are longer than 60 bytes aren't \
                     supported yet."
                        .to_owned(),
                ));
            }
            map.inline = Some(inode.block[..length].to_vec());
            return Ok(map);
        }
        map.runs = inode::runs(inode, self)?;
        Ok(map)
    }

    /// The data map for an entry, cached.
    fn map_for(&self, number: u64, deleted: bool) -> Result<Arc<FileMap>, FsError> {
        let key = (number, deleted);
        if let Some(map) = self.cache().get(&key) {
            return Ok(Arc::clone(map));
        }
        let map = match self.lookup(number, deleted)? {
            (Some(inode), Origin::Live | Origin::DeletedOnDisk | Origin::Journal) => {
                self.file_map(&inode)?
            }
            _ => FileMap::default(),
        };
        let map = Arc::new(map);
        let mut cache = self.cache();
        if cache.len() >= MAP_CACHE {
            cache.clear();
        }
        cache.insert(key, Arc::clone(&map));
        Ok(map)
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<(u64, bool), Arc<FileMap>>> {
        self.maps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn read_map(&self, map: &FileMap, offset: u64, buf: &mut [u8]) -> Result<usize, FsError> {
        if offset >= map.size || buf.is_empty() {
            return Ok(0);
        }
        let n = usize::try_from(map.size - offset)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let buf = &mut buf[..n];
        if let Some(inline) = &map.inline {
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            for (i, byte) in buf.iter_mut().enumerate() {
                *byte = start
                    .checked_add(i)
                    .and_then(|at| inline.get(at))
                    .copied()
                    .unwrap_or(0);
            }
            return Ok(n);
        }
        let block_size = self.sb.block_size;
        let mut done = 0;
        while done < n {
            let position = offset + done as u64;
            let logical = position / block_size;
            let left = (n - done) as u64;
            let (take, physical) = match map.run_from(logical) {
                Some(run) if run.logical <= logical => {
                    let run_end = (run.logical + run.length).saturating_mul(block_size);
                    let take = left.min(run_end - position);
                    let physical = (!run.unwritten).then(|| {
                        (run.physical + (logical - run.logical)) * block_size
                            + position % block_size
                    });
                    (take, physical)
                }
                Some(run) => (left.min(run.logical * block_size - position), None),
                None => (left, None),
            };
            let take = usize::try_from(take).unwrap_or(n - done).max(1);
            let piece = &mut buf[done..done + take];
            match physical {
                Some(at) => {
                    if at.saturating_add(take as u64) > self.sb.blocks * block_size {
                        return Err(FsError::damaged(
                            "A file's data map points past the end of the volume.",
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

    /// The entries of a folder, deleted ones included.
    fn list_dir(&self, number: u64, deleted: bool) -> Result<Vec<Entry>, FsError> {
        let (inode, origin) = self.lookup(number, deleted)?;
        let inode = match (inode, origin) {
            (Some(inode), Origin::Live | Origin::DeletedOnDisk | Origin::Journal)
                if inode.kind() == Kind::Dir =>
            {
                inode
            }
            _ if deleted => return Ok(Vec::new()),
            _ => return Err(FsError::NotFound("That folder".to_owned())),
        };
        if inode.inline() {
            return Err(FsError::Unsupported(
                "Folders kept inside their inode aren't supported yet.".to_owned(),
            ));
        }
        let map = self.file_map(&inode)?;
        let size = map.size.min(MAX_DIR_SIZE);
        let indexed = inode.flags & INDEX_FL != 0;
        let block_size = usize::try_from(self.sb.block_size).unwrap_or(4096);
        let mut raw = Vec::new();
        let mut block = vec![0; block_size];
        for index in 0..size.div_ceil(self.sb.block_size) {
            if self
                .read_map(&map, index * self.sb.block_size, &mut block)
                .is_err()
            {
                continue;
            }
            self.parse_dir_block(&block, indexed && index == 0, &mut raw);
        }
        let live: HashSet<u64> = raw.iter().filter(|e| !e.deleted).map(|e| e.inode).collect();
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for entry in raw {
            if entry.name == b"." || entry.name == b".." {
                continue;
            }
            // An old name of a file still here (renamed, or a second link).
            if entry.deleted && entry.inode != 0 && live.contains(&entry.inode) {
                continue;
            }
            if entry.deleted && !seen.insert((entry.inode, entry.name.clone())) {
                continue;
            }
            entries.push(self.entry(&entry, deleted || entry.deleted));
        }
        Ok(entries)
    }

    /// The entries of one folder block, with deleted ones from its slack.
    fn parse_dir_block(&self, block: &[u8], dx_root: bool, out: &mut Vec<RawEntry>) {
        let filetype = self.sb.incompat & INCOMPAT_FILETYPE != 0;
        let mut at = 0;
        while at + 8 <= block.len() {
            let (Some(inode), Some(rec_len)) = (u32_at(block, at), u16_at(block, at + 4)) else {
                break;
            };
            let rec_len = match usize::from(rec_len) {
                // 64 KiB blocks store a whole-block entry's length this way.
                0 | 65535 => block.len() - at,
                length => length,
            };
            let (name_len, file_type) = name_and_type(block, at, filetype);
            if rec_len < 8 || rec_len % 4 != 0 || at + rec_len > block.len() {
                break;
            }
            // The checksum at the end of a block (metadata_csum).
            if inode == 0 && rec_len == 12 && name_len == 0 && file_type == 0xDE {
                break;
            }
            // A hash-tree index block, which looks like one empty entry.
            if at == 0 && inode == 0 && name_len == 0 && rec_len == block.len() {
                break;
            }
            let needed = round4(8 + name_len);
            if name_len > 0 && 8 + name_len <= rec_len {
                let name = block[at + 8..at + 8 + name_len].to_vec();
                if inode != 0 {
                    out.push(RawEntry {
                        inode: u64::from(inode),
                        name,
                        file_type,
                        deleted: false,
                    });
                } else if plausible_name(&name) {
                    // The first entry of a block, deleted: its number is gone.
                    out.push(RawEntry {
                        inode: 0,
                        name,
                        file_type,
                        deleted: true,
                    });
                }
            }
            // The hash-tree root keeps its index in the slack of "..".
            if !(dx_root && at == 12) {
                self.scan_slack(block, at + needed.max(8), at + rec_len, out);
            }
            at += rec_len;
        }
    }

    /// Deleted entries in the unused end of an entry.
    fn scan_slack(&self, block: &[u8], mut at: usize, end: usize, out: &mut Vec<RawEntry>) {
        let filetype = self.sb.incompat & INCOMPAT_FILETYPE != 0;
        while at + 8 <= end {
            let found = (|| {
                let inode = u64::from(u32_at(block, at)?);
                let rec_len = usize::from(u16_at(block, at + 4)?);
                let (name_len, file_type) = name_and_type(block, at, filetype);
                let needed = round4(8 + name_len);
                let fits = inode > 0
                    && inode <= self.sb.inodes
                    && name_len > 0
                    && at + 8 + name_len <= end
                    && rec_len >= needed
                    && rec_len % 4 == 0
                    && at + rec_len <= block.len()
                    && (!filetype || file_type <= 7);
                if !fits {
                    return None;
                }
                let name = block.get(at + 8..at + 8 + name_len)?.to_vec();
                plausible_name(&name).then_some((
                    RawEntry {
                        inode,
                        name,
                        file_type,
                        deleted: true,
                    },
                    needed,
                ))
            })();
            match found {
                Some((entry, size)) => {
                    out.push(entry);
                    at += size;
                }
                None => at += 4,
            }
        }
    }

    fn entry(&self, raw: &RawEntry, deleted: bool) -> Entry {
        let (inode, origin) = self
            .lookup(raw.inode, deleted)
            .unwrap_or((None, Origin::Cleared));
        let usable = matches!(
            origin,
            Origin::Live | Origin::DeletedOnDisk | Origin::Journal
        );
        let inode = inode.filter(|i| usable || i.mode != 0);
        let kind = match &inode {
            Some(i) if i.mode != 0 => i.kind(),
            _ => match raw.file_type {
                2 => Kind::Dir,
                7 => Kind::Symlink,
                1 | 0 => Kind::File,
                _ => Kind::Special,
            },
        };
        let is_dir = kind == Kind::Dir;
        let size = match &inode {
            Some(i) if usable && !is_dir => i.size,
            _ => 0,
        };
        let times = inode.as_ref().filter(|_| origin != Origin::Reused);
        let mut notes = Vec::new();
        match origin {
            Origin::Live => {}
            Origin::DeletedOnDisk => notes.push(
                "Deleted. Its blocks may have been reused since, so check what it holds."
                    .to_owned(),
            ),
            Origin::Journal => notes.push(
                "Deleted. Its size and where its data was come from an older copy of its \
                 inode in the journal; its blocks may have been reused since."
                    .to_owned(),
            ),
            Origin::Cleared => notes.push(
                "Deleted. Its size and where its data was were cleared, and the journal has \
                 no older copy of them."
                    .to_owned(),
            ),
            Origin::Reused => notes.push(
                "Deleted, and its inode now belongs to another file, so its data can't be found."
                    .to_owned(),
            ),
            Origin::NoNumber => notes.push(
                "Deleted, and its inode number was cleared, so its data can't be found.".to_owned(),
            ),
        }
        if usable && let Some(i) = &inode {
            if i.flags & ENCRYPT_FL != 0 {
                notes.push("Encrypted: it can't be read without its key.".to_owned());
            }
            match kind {
                Kind::Symlink => {
                    let target = self
                        .map_for(raw.inode, deleted)
                        .ok()
                        .and_then(|map| {
                            let mut buf = vec![0; usize::try_from(map.size.min(4096)).ok()?];
                            let n = self.read_map(&map, 0, &mut buf).ok()?;
                            Some(String::from_utf8_lossy(&buf[..n]).into_owned())
                        })
                        .unwrap_or_default();
                    notes.push(format!("A link to {target}"));
                }
                Kind::Special => {
                    notes.push("A device, pipe or socket: it has no data.".to_owned());
                }
                Kind::File | Kind::Dir => {}
            }
        }
        let mut attributes = 0;
        if is_dir {
            attributes |= attributes::DIRECTORY;
        }
        if inode.as_ref().is_some_and(|i| i.flags & ENCRYPT_FL != 0) {
            attributes |= attributes::ENCRYPTED;
        }
        Entry {
            id: EntryId::Inode {
                number: raw.inode,
                deleted,
            },
            name: String::from_utf8_lossy(&raw.name).into_owned(),
            is_dir,
            size,
            deleted,
            created: times.and_then(|i| i.crtime),
            modified: times.map(|i| i.mtime),
            accessed: times.map(|i| i.atime),
            attributes,
            streams: Vec::new(),
            note: (!notes.is_empty()).then(|| notes.join(" ")),
        }
    }
}

impl BlockReader for Ext {
    fn block_size(&self) -> u64 {
        self.sb.block_size
    }

    fn read_block(&self, block: u64) -> Result<Vec<u8>, FsError> {
        if block >= self.sb.blocks {
            return Err(FsError::damaged(format!(
                "Block {block} is past the end of the volume."
            )));
        }
        self.volume
            .read_vec(block * self.sb.block_size, self.sb.block_size)
    }
}

impl FileSystem for Ext {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn root(&self) -> EntryId {
        EntryId::Inode {
            number: ROOT,
            deleted: false,
        }
    }

    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError> {
        match dir {
            EntryId::Inode { number, deleted } => self.list_dir(*number, *deleted),
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
        let EntryId::Inode { number, deleted } = file else {
            return Err(FsError::NotFound("That file".to_owned()));
        };
        if stream.is_some() {
            return Err(FsError::NotFound(
                "ext files have no named streams; that stream".to_owned(),
            ));
        }
        let map = self.map_for(*number, *deleted)?;
        self.read_map(&map, offset, buf)
    }

    fn extents(&self, file: &EntryId, _stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        let EntryId::Inode { number, deleted } = file else {
            return Err(FsError::NotFound("That file".to_owned()));
        };
        let map = self.map_for(*number, *deleted)?;
        let block_size = self.sb.block_size;
        let base = self.volume.start();
        let mut extents = Vec::new();
        let mut covered = 0;
        for run in &map.runs {
            let start = run.logical.saturating_mul(block_size).min(map.size);
            let end = (run.logical + run.length)
                .saturating_mul(block_size)
                .min(map.size);
            if start > covered {
                extents.push(Extent {
                    offset: 0,
                    length: start - covered,
                    sparse: true,
                });
            }
            if end > start {
                extents.push(Extent {
                    offset: base.saturating_add(run.physical.saturating_mul(block_size)),
                    length: end - start,
                    sparse: run.unwritten,
                });
            }
            covered = covered.max(end);
        }
        if map.inline.is_none() && covered < map.size {
            extents.push(Extent {
                offset: 0,
                length: map.size - covered,
                sparse: true,
            });
        }
        Ok(extents)
    }
}

fn name_and_type(block: &[u8], at: usize, filetype: bool) -> (usize, u8) {
    if filetype {
        (
            usize::from(u8_at(block, at + 6).unwrap_or(0)),
            u8_at(block, at + 7).unwrap_or(0),
        )
    } else {
        (usize::from(u16_at(block, at + 6).unwrap_or(0)), 0)
    }
}

fn round4(value: usize) -> usize {
    value.next_multiple_of(4)
}

/// Whether bytes found in slack space look like a real file name.
fn plausible_name(name: &[u8]) -> bool {
    !name.is_empty()
        && !name.contains(&0)
        && !name.contains(&b'/')
        && std::str::from_utf8(name).is_ok_and(|s| !s.chars().any(char::is_control))
}
