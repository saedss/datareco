//! ext inodes: what a file or folder is, its times, and where its data is
//! (an extent tree, a classic block map, or inline in the inode).

use std::collections::HashSet;

use crate::bytes::{bytes, u16_at, u32_at};
use crate::error::FsError;

/// The inode flag for files whose data is described by an extent tree.
const EXTENTS_FL: u32 = 0x8_0000;
/// The inode flag for small files and folders stored inside the inode.
const INLINE_DATA_FL: u32 = 0x1000_0000;
/// The inode flag for folders with a hash tree index.
pub(crate) const INDEX_FL: u32 = 0x1000;
/// The inode flag for encrypted files and folders.
pub(crate) const ENCRYPT_FL: u32 = 0x800;

const EXTENT_MAGIC: u16 = 0xF30A;
/// Deeper extent trees than this are damage (the kernel allows 5).
const MAX_DEPTH: u16 = 5;
/// More pieces than this means the tree is garbage.
const MAX_RUNS: usize = 1 << 22;

/// What kind of thing an inode is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    File,
    Dir,
    Symlink,
    /// Devices, pipes and sockets.
    Special,
}

/// A parsed inode.
#[derive(Clone, Debug)]
pub(crate) struct Inode {
    pub mode: u16,
    pub size: u64,
    pub links: u16,
    pub dtime: u32,
    pub flags: u32,
    /// `i_block`: the extent tree root, block map or inline data.
    pub block: [u8; 60],
    pub atime: i64,
    pub mtime: i64,
    pub crtime: Option<i64>,
}

impl Inode {
    pub fn parse(raw: &[u8]) -> Option<Inode> {
        let mut block = [0; 60];
        block.copy_from_slice(bytes(raw, 40, 60)?);
        let extra = if raw.len() > 128 {
            u16_at(raw, 128).map_or(0, usize::from)
        } else {
            0
        };
        // An extra time field is there when the inode's extra space covers it.
        let extra_field = |at: usize| -> Option<u32> {
            (extra >= at + 4 - 128 && raw.len() >= at + 4)
                .then(|| u32_at(raw, at))
                .flatten()
        };
        let time = |at: usize, extra_at: usize| -> Option<i64> {
            let seconds = i64::from(u32_at(raw, at)?.cast_signed());
            let epoch = extra_field(extra_at).map_or(0, |e| i64::from(e & 3));
            Some(seconds + (epoch << 32))
        };
        let crtime = extra_field(144).map(|seconds| {
            let epoch = extra_field(148).map_or(0, |e| i64::from(e & 3));
            i64::from(seconds.cast_signed()) + (epoch << 32)
        });
        Some(Inode {
            mode: u16_at(raw, 0)?,
            size: u64::from(u32_at(raw, 4)?) | (u64::from(u32_at(raw, 108)?) << 32),
            links: u16_at(raw, 26)?,
            dtime: u32_at(raw, 20)?,
            flags: u32_at(raw, 32)?,
            block,
            atime: time(8, 140)?,
            mtime: time(16, 136)?,
            crtime,
        })
    }

    pub fn kind(&self) -> Kind {
        match self.mode & 0xF000 {
            0x4000 => Kind::Dir,
            0x8000 => Kind::File,
            0xA000 => Kind::Symlink,
            _ => Kind::Special,
        }
    }

    /// In use: linked from a folder and not deleted.
    pub fn live(&self) -> bool {
        self.links > 0 && self.dtime == 0 && self.mode != 0
    }

    pub fn uses_extents(&self) -> bool {
        self.flags & EXTENTS_FL != 0
    }

    pub fn inline(&self) -> bool {
        self.flags & INLINE_DATA_FL != 0
    }

    /// A symbolic link whose target is stored in the inode itself.
    pub fn fast_symlink(&self) -> bool {
        self.kind() == Kind::Symlink && !self.inline() && !self.uses_extents() && self.size < 60
    }

    fn block_map_empty(&self) -> bool {
        self.block.iter().all(|&b| b == 0)
    }

    /// Whether the inode still says where its data is (deleting a file on
    /// ext3 and ext4 clears this).
    pub fn has_data_map(&self) -> bool {
        if self.inline() || self.fast_symlink() {
            return true;
        }
        if self.uses_extents() {
            return u16_at(&self.block, 0) == Some(EXTENT_MAGIC)
                && u16_at(&self.block, 2).is_some_and(|entries| entries > 0);
        }
        !self.block_map_empty()
    }
}

/// A stretch of a file's blocks: `length` blocks from logical block
/// `logical` are at `physical` on the volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    /// Allocated but never written: reads as zeros.
    pub unwritten: bool,
}

/// Reads one block of the volume, for walking trees and block maps.
pub(crate) trait BlockReader {
    fn block_size(&self) -> u64;
    fn read_block(&self, block: u64) -> Result<Vec<u8>, FsError>;
}

/// Where an inode's data is, in logical order.
pub(crate) fn runs(inode: &Inode, reader: &dyn BlockReader) -> Result<Vec<Run>, FsError> {
    let mut runs = if inode.uses_extents() {
        let mut runs = Vec::new();
        let mut seen = HashSet::new();
        extent_node(&inode.block, None, reader, &mut runs, &mut seen)?;
        runs
    } else {
        block_map(inode, reader)?
    };
    runs.sort_by_key(|r| r.logical);
    Ok(merge(runs))
}

/// One node of an extent tree: the root in the inode, or a tree block.
fn extent_node(
    node: &[u8],
    expected_depth: Option<u16>,
    reader: &dyn BlockReader,
    runs: &mut Vec<Run>,
    seen: &mut HashSet<u64>,
) -> Result<(), FsError> {
    let damaged = || FsError::damaged("An extent tree is damaged.");
    if u16_at(node, 0) != Some(EXTENT_MAGIC) {
        return Err(damaged());
    }
    let entries = usize::from(u16_at(node, 2).ok_or_else(damaged)?);
    let depth = u16_at(node, 6).ok_or_else(damaged)?;
    if depth > MAX_DEPTH || expected_depth.is_some_and(|d| d != depth) {
        return Err(damaged());
    }
    for i in 0..entries {
        let entry = bytes(node, 12 + i * 12, 12).ok_or_else(damaged)?;
        let logical = u64::from(u32_at(entry, 0).ok_or_else(damaged)?);
        if depth == 0 {
            let raw_length = u16_at(entry, 4).ok_or_else(damaged)?;
            let (length, unwritten) = if raw_length > 32768 {
                (raw_length - 32768, true)
            } else {
                (raw_length, false)
            };
            let physical = u64::from(u32_at(entry, 8).ok_or_else(damaged)?)
                | (u64::from(u16_at(entry, 6).ok_or_else(damaged)?) << 32);
            if length > 0 {
                runs.push(Run {
                    logical,
                    physical,
                    length: u64::from(length),
                    unwritten,
                });
                if runs.len() > MAX_RUNS {
                    return Err(damaged());
                }
            }
        } else {
            let child = u64::from(u32_at(entry, 4).ok_or_else(damaged)?)
                | (u64::from(u16_at(entry, 8).ok_or_else(damaged)?) << 32);
            if !seen.insert(child) {
                return Err(damaged());
            }
            let block = reader.read_block(child)?;
            extent_node(&block, Some(depth - 1), reader, runs, seen)?;
        }
    }
    Ok(())
}

/// The classic ext2/ext3 block map: 12 direct blocks, then single, double
/// and triple indirect blocks. Zero entries are holes.
fn block_map(inode: &Inode, reader: &dyn BlockReader) -> Result<Vec<Run>, FsError> {
    let block_size = reader.block_size();
    let wanted = inode.size.div_ceil(block_size);
    let per_block = block_size / 4;
    let mut runs = Vec::new();
    let mut logical = 0;
    for i in 0..12 {
        if logical >= wanted {
            return Ok(runs);
        }
        let physical = u64::from(u32_at(&inode.block, i * 4).unwrap_or(0));
        push_block(&mut runs, logical, physical);
        logical += 1;
    }
    for (level, at) in [(1u32, 48), (2, 52), (3, 56)] {
        if logical >= wanted {
            break;
        }
        let span = per_block.saturating_pow(level);
        let root = u64::from(u32_at(&inode.block, at).unwrap_or(0));
        indirect(reader, root, level, logical, wanted, &mut runs)?;
        logical = logical.saturating_add(span);
    }
    Ok(runs)
}

/// An indirect block `level` levels above data blocks, covering logical
/// blocks from `first`.
fn indirect(
    reader: &dyn BlockReader,
    block: u64,
    level: u32,
    first: u64,
    wanted: u64,
    runs: &mut Vec<Run>,
) -> Result<(), FsError> {
    if block == 0 || first >= wanted {
        return Ok(());
    }
    let per_block = reader.block_size() / 4;
    let span = per_block.saturating_pow(level - 1);
    let pointers = reader.read_block(block)?;
    for i in 0..per_block {
        let logical = first.saturating_add(i.saturating_mul(span));
        if logical >= wanted {
            break;
        }
        let at = usize::try_from(i * 4).unwrap_or(usize::MAX);
        let pointer = u64::from(u32_at(&pointers, at).unwrap_or(0));
        if level == 1 {
            push_block(runs, logical, pointer);
        } else {
            indirect(reader, pointer, level - 1, logical, wanted, runs)?;
        }
        if runs.len() > MAX_RUNS {
            return Err(FsError::damaged("A block map is damaged."));
        }
    }
    Ok(())
}

fn push_block(runs: &mut Vec<Run>, logical: u64, physical: u64) {
    if physical == 0 {
        return;
    }
    if let Some(last) = runs.last_mut()
        && !last.unwritten
        && last.logical + last.length == logical
        && last.physical + last.length == physical
    {
        last.length += 1;
        return;
    }
    runs.push(Run {
        logical,
        physical,
        length: 1,
        unwritten: false,
    });
}

/// Joins runs that follow each other both logically and on the volume.
fn merge(runs: Vec<Run>) -> Vec<Run> {
    let mut merged: Vec<Run> = Vec::with_capacity(runs.len());
    for run in runs {
        if let Some(last) = merged.last_mut()
            && last.unwritten == run.unwritten
            && last.logical.checked_add(last.length) == Some(run.logical)
            && last.physical.checked_add(last.length) == Some(run.physical)
        {
            last.length += run.length;
            continue;
        }
        merged.push(run);
    }
    merged
}
