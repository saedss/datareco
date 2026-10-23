//! [`Disk`]: cached, bad-sector-tolerant reads from any [`SectorSource`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::sync::{Mutex, PoisonError};

use serde::Serialize;

use crate::aligned::AlignedBuf;
use crate::error::{Error, Result};
use crate::source::{FileSource, SectorSource};
use crate::spec::SourceSpec;

/// Reads go to the source in blocks of this many bytes, which are cached.
pub const BLOCK_SIZE: usize = 64 * 1024;

/// Blocks kept in the cache by default (16 MiB).
const DEFAULT_CACHE_BLOCKS: usize = 256;

/// How reads deal with sectors that fail.
#[derive(Clone, Debug)]
pub struct ReadPolicy {
    /// Extra attempts for a sector that fails to read.
    pub retries: u32,
    /// Byte written in place of data that couldn't be read.
    pub fill_byte: u8,
}

impl Default for ReadPolicy {
    fn default() -> Self {
        Self {
            retries: 2,
            fill_byte: 0,
        }
    }
}

/// What happened during a read that succeeded overall.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadOutcome {
    /// Sectors in the requested range that couldn't be read, in order. Their
    /// bytes hold [`ReadPolicy::fill_byte`].
    pub unreadable: Vec<u64>,
}

/// An open disk, volume or image file.
///
/// Reads work at any byte offset and length. They are served from a cache of
/// [`BLOCK_SIZE`] blocks. When a block can't be read, each of its sectors is
/// retried on its own; sectors that still fail are filled with
/// [`ReadPolicy::fill_byte`], reported in [`ReadOutcome`], and remembered in
/// [`Disk::bad_sectors`]. Nothing is ever written.
pub struct Disk {
    name: String,
    size: u64,
    sector_size: u32,
    policy: ReadPolicy,
    state: Mutex<State>,
}

struct State {
    source: Box<dyn SectorSource>,
    cache: BlockCache,
    bad_sectors: BTreeSet<u64>,
}

struct Block {
    data: AlignedBuf,
    /// Sectors in this block that couldn't be read.
    unreadable: Vec<u64>,
}

impl Disk {
    /// Opens a physical disk, volume or image file, read-only.
    pub fn open(spec: &SourceSpec) -> Result<Disk> {
        let source = FileSource::open(spec)?;
        Ok(Disk::new(spec.display_name(), Box::new(source)))
    }

    /// Wraps any sector source. `name` is used in errors and the UI.
    pub fn new(name: impl Into<String>, source: Box<dyn SectorSource>) -> Disk {
        let sector_size = source.sector_size();
        assert!(
            sector_size > 0 && BLOCK_SIZE.is_multiple_of(sector_size as usize),
            "unsupported sector size {sector_size}"
        );
        Disk {
            name: name.into(),
            size: source.size(),
            sector_size,
            policy: ReadPolicy::default(),
            state: Mutex::new(State {
                source,
                cache: BlockCache::new(DEFAULT_CACHE_BLOCKS),
                bad_sectors: BTreeSet::new(),
            }),
        }
    }

    pub fn with_read_policy(mut self, policy: ReadPolicy) -> Disk {
        self.policy = policy;
        self
    }

    /// Changes how many blocks the cache keeps (at least one).
    pub fn with_cache_blocks(self, blocks: usize) -> Disk {
        self.lock().cache = BlockCache::new(blocks.max(1));
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    /// Number of sectors, counting a partial last sector of an image file.
    pub fn sector_count(&self) -> u64 {
        self.size.div_ceil(u64::from(self.sector_size))
    }

    /// Every sector found unreadable so far, in order.
    pub fn bad_sectors(&self) -> Vec<u64> {
        self.lock().bad_sectors.iter().copied().collect()
    }

    /// Reads `count` sectors starting at sector `lba`. A partial last sector of
    /// an image file is padded with zeros.
    pub fn read_sectors(&self, lba: u64, count: u32) -> Result<(Vec<u8>, ReadOutcome)> {
        let sector_size = u64::from(self.sector_size);
        let len = u64::from(count) * sector_size;
        let offset = lba
            .checked_mul(sector_size)
            .ok_or_else(|| self.out_of_range(u64::MAX, len))?;
        let available = self.size.saturating_sub(offset).min(len);
        if offset >= self.size || offset + len > self.sector_count() * sector_size {
            return Err(self.out_of_range(offset, len));
        }
        let mut data = vec![0; to_usize(len)];
        let outcome = self.read_at(offset, &mut data[..to_usize(available)])?;
        Ok((data, outcome))
    }

    /// Fills `buf` with the bytes starting at `offset`.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<ReadOutcome> {
        let len = buf.len() as u64;
        let end = offset
            .checked_add(len)
            .filter(|&end| end <= self.size)
            .ok_or_else(|| self.out_of_range(offset, len))?;
        let block_size = BLOCK_SIZE as u64;
        let sector_size = u64::from(self.sector_size);

        let mut state = self.lock();
        let mut unreadable = BTreeSet::new();
        let mut pos = offset;
        while pos < end {
            let index = pos / block_size;
            let block = state.block(index, self)?;
            let in_block = to_usize(pos - index * block_size);
            let n = (block.data.len() - in_block).min(to_usize(end - pos));
            let written = to_usize(pos - offset);
            buf[written..written + n].copy_from_slice(&block.data[in_block..in_block + n]);
            for &lba in &block.unreadable {
                let start = lba * sector_size;
                if start < pos + n as u64 && start + sector_size > pos {
                    unreadable.insert(lba);
                }
            }
            pos += n as u64;
        }
        Ok(ReadOutcome {
            unreadable: unreadable.into_iter().collect(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the lock can't leave the cache inconsistent
        // (blocks are inserted whole), so a poisoned lock is safe to reuse.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn out_of_range(&self, offset: u64, len: u64) -> Error {
        Error::OutOfRange {
            name: self.name.clone(),
            offset,
            len,
            size: self.size,
        }
    }
}

impl State {
    /// The block at `index`, from the cache or freshly read.
    fn block(&mut self, index: u64, disk: &Disk) -> Result<&Block> {
        if !self.cache.contains(index) {
            let block = self.load_block(index, disk)?;
            self.bad_sectors.extend(&block.unreadable);
            self.cache.insert(index, block);
        }
        Ok(self.cache.get(index).expect("block was just cached"))
    }

    fn load_block(&mut self, index: u64, disk: &Disk) -> Result<Block> {
        let sector_size = u64::from(disk.sector_size);
        let per_block = BLOCK_SIZE as u64 / sector_size;
        let first = index * per_block;
        let count = per_block.min(disk.sector_count() - first);
        let mut data = AlignedBuf::zeroed(to_usize(count * sector_size));

        match self.source.read_sectors(first, &mut data) {
            Ok(()) => {
                return Ok(Block {
                    data,
                    unreadable: Vec::new(),
                });
            }
            Err(e) if is_fatal(&e) => return Err(Error::from_io(&disk.name, e)),
            Err(_) => {}
        }

        // The block failed as a whole: go sector by sector to save what we can.
        let mut unreadable = Vec::new();
        let mut sector = AlignedBuf::zeroed(disk.sector_size as usize);
        for i in 0..count {
            let lba = first + i;
            let range = to_usize(i * sector_size)..to_usize((i + 1) * sector_size);
            let mut read = false;
            for _attempt in 0..=disk.policy.retries {
                match self.source.read_sectors(lba, &mut sector) {
                    Ok(()) => {
                        read = true;
                        break;
                    }
                    Err(e) if is_fatal(&e) => return Err(Error::from_io(&disk.name, e)),
                    Err(_) => {}
                }
            }
            if read {
                data[range].copy_from_slice(&sector);
            } else {
                data[range].fill(disk.policy.fill_byte);
                unreadable.push(lba);
            }
        }
        Ok(Block { data, unreadable })
    }
}

/// Errors that retrying sector by sector can't fix.
fn is_fatal(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::PermissionDenied | io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
    )
}

/// Converts a byte count or offset that fits in memory (it indexes a buffer).
fn to_usize(value: u64) -> usize {
    usize::try_from(value).expect("value indexes an in-memory buffer")
}

/// A least-recently-used cache of blocks.
struct BlockCache {
    capacity: usize,
    blocks: HashMap<u64, (u64, Block)>,
    by_age: BTreeMap<u64, u64>,
    clock: u64,
}

impl BlockCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            blocks: HashMap::new(),
            by_age: BTreeMap::new(),
            clock: 0,
        }
    }

    fn contains(&self, index: u64) -> bool {
        self.blocks.contains_key(&index)
    }

    fn get(&mut self, index: u64) -> Option<&Block> {
        self.clock += 1;
        let (last_used, block) = self.blocks.get_mut(&index)?;
        self.by_age.remove(last_used);
        *last_used = self.clock;
        self.by_age.insert(self.clock, index);
        Some(block)
    }

    fn insert(&mut self, index: u64, block: Block) {
        while self.blocks.len() >= self.capacity {
            let Some((_, oldest)) = self.by_age.pop_first() else {
                break;
            };
            self.blocks.remove(&oldest);
        }
        self.clock += 1;
        self.by_age.insert(self.clock, index);
        self.blocks.insert(index, (self.clock, block));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;
    use crate::source::MemorySource;

    /// A disk whose byte at offset `i` is `i % 251`.
    fn pattern(size: usize) -> Vec<u8> {
        (0..size).map(|i| u8::try_from(i % 251).unwrap()).collect()
    }

    fn disk(source: MemorySource) -> Disk {
        Disk::new("test disk", Box::new(source))
    }

    #[test]
    fn reads_any_range_including_across_blocks() {
        let data = pattern(3 * BLOCK_SIZE);
        let disk = disk(MemorySource::new(data.clone(), 512));
        let mut buf = vec![0; 1000];
        let offset = BLOCK_SIZE as u64 - 300;
        let outcome = disk.read_at(offset, &mut buf).unwrap();
        assert!(outcome.unreadable.is_empty());
        assert_eq!(buf, data[BLOCK_SIZE - 300..BLOCK_SIZE + 700]);
    }

    #[test]
    fn reads_whole_sectors() {
        let data = pattern(BLOCK_SIZE);
        let disk = disk(MemorySource::new(data.clone(), 512));
        let (bytes, outcome) = disk.read_sectors(3, 2).unwrap();
        assert_eq!(bytes, data[1536..2560]);
        assert_eq!(outcome, ReadOutcome::default());
        assert_eq!(disk.sector_count(), 128);
    }

    #[test]
    fn reading_past_the_end_is_an_error() {
        let disk = disk(MemorySource::new(vec![0; 4096], 512));
        let mut buf = vec![0; 10];
        assert!(matches!(
            disk.read_at(4090, &mut buf),
            Err(Error::OutOfRange { .. })
        ));
        assert!(matches!(
            disk.read_sectors(8, 1),
            Err(Error::OutOfRange { .. })
        ));
        assert!(matches!(
            disk.read_sectors(7, 2),
            Err(Error::OutOfRange { .. })
        ));
        assert!(disk.read_sectors(7, 1).is_ok());
    }

    #[test]
    fn bad_sectors_are_filled_reported_and_remembered() {
        let data = pattern(2 * BLOCK_SIZE);
        let disk = disk(MemorySource::new(data.clone(), 512).with_bad_sectors([130]))
            .with_read_policy(ReadPolicy {
                retries: 1,
                fill_byte: 0xEE,
            });

        // A read that doesn't touch sector 130 still succeeds cleanly,
        // even though it shares a block with it.
        let (bytes, outcome) = disk.read_sectors(128, 2).unwrap();
        assert_eq!(bytes, data[128 * 512..130 * 512]);
        assert!(outcome.unreadable.is_empty());

        let (bytes, outcome) = disk.read_sectors(129, 3).unwrap();
        assert_eq!(outcome.unreadable, vec![130]);
        assert_eq!(bytes[..512], data[129 * 512..130 * 512]);
        assert!(bytes[512..1024].iter().all(|&b| b == 0xEE));
        assert_eq!(bytes[1024..], data[131 * 512..132 * 512]);
        assert_eq!(disk.bad_sectors(), vec![130]);
    }

    #[test]
    fn a_sector_that_fails_once_is_recovered_by_retrying() {
        let data = pattern(BLOCK_SIZE);
        let disk = disk(MemorySource::new(data.clone(), 512).with_flaky_sector(5, 1));
        let (bytes, outcome) = disk.read_sectors(0, 8).unwrap();
        assert!(outcome.unreadable.is_empty());
        assert_eq!(bytes, data[..8 * 512]);
        assert!(disk.bad_sectors().is_empty());
    }

    #[test]
    fn repeated_reads_come_from_the_cache() {
        let source = MemorySource::new(pattern(4 * BLOCK_SIZE), 512);
        let reads = source.read_counter();
        let disk = disk(source);
        for _ in 0..5 {
            disk.read_sectors(0, 1).unwrap();
            disk.read_sectors(100, 4).unwrap();
        }
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_least_recently_used_block_is_evicted() {
        let source = MemorySource::new(pattern(4 * BLOCK_SIZE), 512);
        let reads = source.read_counter();
        let disk = disk(source).with_cache_blocks(2);
        disk.read_sectors(0, 1).unwrap(); // block 0
        disk.read_sectors(128, 1).unwrap(); // block 1
        disk.read_sectors(0, 1).unwrap(); // block 0 again: cached
        disk.read_sectors(256, 1).unwrap(); // block 2 evicts block 1
        assert_eq!(reads.load(Ordering::Relaxed), 3);
        disk.read_sectors(0, 1).unwrap(); // still cached
        assert_eq!(reads.load(Ordering::Relaxed), 3);
        disk.read_sectors(128, 1).unwrap(); // block 1 was evicted
        assert_eq!(reads.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn works_with_4096_byte_sectors() {
        let data = pattern(2 * BLOCK_SIZE);
        let disk = disk(MemorySource::new(data.clone(), 4096).with_bad_sectors([17]));
        assert_eq!(disk.sector_count(), 32);
        let (bytes, outcome) = disk.read_sectors(16, 2).unwrap();
        assert_eq!(bytes[..4096], data[16 * 4096..17 * 4096]);
        assert_eq!(outcome.unreadable, vec![17]);
    }
}
