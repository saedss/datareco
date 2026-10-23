//! The ext3/ext4 journal (jbd2), read for one thing: older copies of
//! file-system blocks. Deleting a file on ext3 and ext4 clears where its
//! data is from its inode, but the journal often still holds the inode as
//! it was before.
//!
//! The journal is scanned whole, for every descriptor block in it, not just
//! the transactions still to replay: after a clean unmount the log is
//! marked empty, yet its old transactions are all still there.

use std::collections::HashMap;

use crate::bytes::bytes;

const MAGIC: u32 = 0xC03B_3998;
const DESCRIPTOR: u32 = 1;
const SUPERBLOCK_V1: u32 = 3;
const SUPERBLOCK_V2: u32 = 4;

const INCOMPAT_64BIT: u32 = 0x2;
const INCOMPAT_CSUM_V2: u32 = 0x8;
const INCOMPAT_CSUM_V3: u32 = 0x10;

const TAG_ESCAPED: u32 = 0x1;
const TAG_SAME_UUID: u32 = 0x2;
const TAG_LAST: u32 = 0x8;

/// A copy of a file-system block kept in the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BlockCopy {
    /// The transaction it belongs to; higher is newer.
    pub sequence: u32,
    /// Where the copy is, as a block of the journal.
    pub journal_block: u64,
    /// Its first four bytes were the journal's magic number, stored as zeros.
    pub escaped: bool,
}

/// Every copy of every block the journal holds.
#[derive(Debug, Default)]
pub(crate) struct Journal {
    /// File-system block → its copies, newest first.
    copies: HashMap<u64, Vec<BlockCopy>>,
}

fn be32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes(data, at, 4)?.try_into().ok()?))
}

fn be16(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes(data, at, 2)?.try_into().ok()?))
}

/// The journal's layout, from its superblock.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    /// Journal blocks that hold the log: `first..length`.
    pub first: u64,
    pub length: u64,
    tag_size: usize,
    sixty_four_bit: bool,
    csum_v3: bool,
    tail: usize,
}

impl Layout {
    /// Reads the journal superblock (journal block 0).
    pub fn parse(block: &[u8], block_size: u64) -> Option<Layout> {
        if be32(block, 0)? != MAGIC {
            return None;
        }
        let kind = be32(block, 4)?;
        if kind != SUPERBLOCK_V1 && kind != SUPERBLOCK_V2 {
            return None;
        }
        if u64::from(be32(block, 12)?) != block_size {
            return None;
        }
        let length = u64::from(be32(block, 16)?);
        let first = u64::from(be32(block, 20)?);
        if first == 0 || first >= length {
            return None;
        }
        let incompat = if kind == SUPERBLOCK_V2 {
            be32(block, 40)?
        } else {
            0
        };
        let sixty_four_bit = incompat & INCOMPAT_64BIT != 0;
        let csum_v3 = incompat & INCOMPAT_CSUM_V3 != 0;
        let csum_v2 = incompat & INCOMPAT_CSUM_V2 != 0;
        let tag_size = if csum_v3 {
            16
        } else {
            let size = if csum_v2 { 14 } else { 12 };
            if sixty_four_bit { size } else { size - 4 }
        };
        Some(Layout {
            first,
            length,
            tag_size,
            sixty_four_bit,
            csum_v3,
            tail: if csum_v2 || csum_v3 { 4 } else { 0 },
        })
    }

    /// The journal block after `block`, wrapping around the log.
    fn next(&self, block: u64) -> u64 {
        if block + 1 >= self.length {
            self.first
        } else {
            block + 1
        }
    }

    /// The tags of a descriptor block: which file-system blocks the journal
    /// blocks after it are copies of.
    fn tags(&self, block: &[u8]) -> Vec<(u64, bool)> {
        let mut tags = Vec::new();
        let end = block.len().saturating_sub(self.tail);
        let mut at = 12;
        while at + self.tag_size <= end {
            let Some(low) = be32(block, at) else {
                break;
            };
            let flags = if self.csum_v3 {
                be32(block, at + 4)
            } else {
                be16(block, at + 6).map(u32::from)
            };
            let Some(flags) = flags else {
                break;
            };
            let high = if self.sixty_four_bit {
                be32(block, at + 8).unwrap_or(0)
            } else {
                0
            };
            tags.push((
                (u64::from(high) << 32) | u64::from(low),
                flags & TAG_ESCAPED != 0,
            ));
            at += self.tag_size;
            if flags & TAG_SAME_UUID == 0 {
                at += 16;
            }
            if flags & TAG_LAST != 0 {
                break;
            }
        }
        tags
    }
}

impl Journal {
    /// Indexes the journal: `read` gives a journal block (by its number in
    /// the journal), or `None` if it can't be read.
    pub fn index(layout: Layout, mut read: impl FnMut(u64) -> Option<Vec<u8>>) -> Journal {
        let mut journal = Journal::default();
        for block in layout.first..layout.length {
            let Some(data) = read(block) else {
                continue;
            };
            if be32(&data, 0) != Some(MAGIC) || be32(&data, 4) != Some(DESCRIPTOR) {
                continue;
            }
            let Some(sequence) = be32(&data, 8) else {
                continue;
            };
            let mut at = block;
            for (target, escaped) in layout.tags(&data) {
                at = layout.next(at);
                journal.copies.entry(target).or_default().push(BlockCopy {
                    sequence,
                    journal_block: at,
                    escaped,
                });
            }
        }
        for copies in journal.copies.values_mut() {
            copies.sort_by_key(|c| std::cmp::Reverse(c.sequence));
        }
        journal
    }

    /// The copies of a file-system block, newest first.
    pub fn copies(&self, block: u64) -> &[BlockCopy] {
        self.copies.get(&block).map_or(&[], Vec::as_slice)
    }

    /// Puts back the magic number an escaped copy had zeroed.
    pub fn unescape(copy: &BlockCopy, data: &mut [u8]) {
        if copy.escaped
            && let Some(first) = data.get_mut(..4)
        {
            first.copy_from_slice(&MAGIC.to_be_bytes());
        }
    }
}
