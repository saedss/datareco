//! Surface test: reading every sector of a disk (or of part of it) in
//! blocks, timing each read, and keeping a map of what was slow or
//! unreadable. It only reads; it never tries to fix or remap anything.
//!
//! On a failing drive every read counts, so the test reads each block once
//! and moves on: no retries, which is the copy's job (Copy).

use std::io;
use std::time::{Duration, Instant};

use datareco_io::{SectorSource, is_device_gone};
use serde::Serialize;

/// Bytes read at a time.
pub const SURFACE_BLOCK_BYTES: u64 = 256 * 1024;

/// Squares in the stored map; pictures are drawn from these.
const CELLS: usize = 1000;

/// Unreadable ranges kept for the list (all are counted).
const MAX_BAD_RANGES: usize = 10_000;

/// How long a block took to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BlockSpeed {
    /// Under 20 ms.
    Fast,
    /// Under 100 ms.
    Normal,
    /// Under 500 ms: the drive had to work at it.
    Slow,
    /// Half a second or more: often a sector about to go bad.
    VerySlow,
    /// The read failed.
    Unreadable,
}

impl BlockSpeed {
    fn symbol(self) -> char {
        match self {
            BlockSpeed::Fast => 'f',
            BlockSpeed::Normal => 'n',
            BlockSpeed::Slow => 's',
            BlockSpeed::VerySlow => 'v',
            BlockSpeed::Unreadable => 'x',
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Which speed a read of one block took.
pub fn classify(elapsed: Duration) -> BlockSpeed {
    match elapsed.as_millis() {
        0..20 => BlockSpeed::Fast,
        20..100 => BlockSpeed::Normal,
        100..500 => BlockSpeed::Slow,
        _ => BlockSpeed::VerySlow,
    }
}

/// How many blocks read at each speed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedCounts {
    pub fast: u64,
    pub normal: u64,
    pub slow: u64,
    pub very_slow: u64,
    pub unreadable: u64,
}

/// Sectors that couldn't be read, next to each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BadRange {
    pub start_lba: u64,
    pub sectors: u64,
}

/// How a surface test is going.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfaceProgress {
    pub start_lba: u64,
    /// Sectors to test, and tested so far.
    pub sectors: u64,
    pub tested: u64,
    pub counts: SpeedCounts,
    /// Sectors in blocks that couldn't be read.
    pub unreadable_sectors: u64,
    /// The first unreadable ranges, in disk order.
    pub bad_ranges: Vec<BadRange>,
    pub bad_range_count: usize,
    /// Time spent reading so far.
    pub elapsed_ms: u64,
    pub bytes_per_second: u64,
    /// The map, one symbol per square: `.` not tested yet, `f` fast,
    /// `n` normal, `s` slow, `v` very slow, `x` unreadable. Each square
    /// shows the slowest block in its part of the range.
    pub picture: String,
    pub done: bool,
    /// Why the test stopped before the end, such as the disk being unplugged.
    pub stopped: Option<String>,
}

/// A surface test of a range of sectors.
pub struct SurfaceTest {
    start: u64,
    end: u64,
    position: u64,
    block_sectors: u64,
    sector_size: u32,
    cells: Vec<Option<BlockSpeed>>,
    counts: [u64; 5],
    unreadable_sectors: u64,
    bad: Vec<BadRange>,
    bad_count: usize,
    elapsed: Duration,
    stopped: Option<String>,
    buffer: Vec<u8>,
}

impl SurfaceTest {
    /// Tests `sectors` sectors from `start_lba`, on a source with
    /// `sector_size`-byte sectors.
    pub fn new(sector_size: u32, start_lba: u64, sectors: u64) -> SurfaceTest {
        let sector_size = sector_size.max(1);
        let block_sectors = (SURFACE_BLOCK_BYTES / u64::from(sector_size)).max(1);
        SurfaceTest {
            start: start_lba,
            end: start_lba.saturating_add(sectors),
            position: start_lba,
            block_sectors,
            sector_size,
            cells: vec![None; CELLS],
            counts: [0; 5],
            unreadable_sectors: 0,
            bad: Vec::new(),
            bad_count: 0,
            elapsed: Duration::ZERO,
            stopped: None,
            buffer: Vec::new(),
        }
    }

    /// Tests the whole source.
    pub fn whole(source: &dyn SectorSource) -> SurfaceTest {
        let sector_size = source.sector_size();
        let sectors = source.size() / u64::from(sector_size.max(1));
        SurfaceTest::new(sector_size, 0, sectors)
    }

    pub fn is_done(&self) -> bool {
        self.position >= self.end || self.stopped.is_some()
    }

    /// Reads blocks for about `budget`, then reports.
    pub fn step(&mut self, source: &mut dyn SectorSource, budget: Duration) -> SurfaceProgress {
        let began = Instant::now();
        while !self.is_done() && began.elapsed() < budget {
            let lba = self.position;
            let count = self.block_sectors.min(self.end - lba);
            let len = usize::try_from(count * u64::from(self.sector_size)).unwrap_or(0);
            self.buffer.resize(len, 0);
            let read = Instant::now();
            let result = source.read_sectors(lba, &mut self.buffer[..len]);
            let took = read.elapsed();
            self.elapsed += took;
            match result {
                Ok(()) => self.record(lba, count, classify(took)),
                Err(e) if is_device_gone(&e) => {
                    self.stopped = Some(gone(&e));
                    break;
                }
                Err(_) => self.record(lba, count, BlockSpeed::Unreadable),
            }
            self.position = lba + count;
        }
        self.progress()
    }

    /// Notes how a block read, and moves past it.
    pub(crate) fn record(&mut self, lba: u64, sectors: u64, speed: BlockSpeed) {
        self.counts[speed.index()] += 1;
        if speed == BlockSpeed::Unreadable {
            self.unreadable_sectors += sectors;
            match self.bad.last_mut() {
                Some(last) if last.start_lba + last.sectors == lba => last.sectors += sectors,
                _ => {
                    self.bad_count += 1;
                    if self.bad.len() < MAX_BAD_RANGES {
                        self.bad.push(BadRange {
                            start_lba: lba,
                            sectors,
                        });
                    }
                }
            }
        }
        let first = self.cell(lba);
        // Squares the block reaches into, rounding up, so short ranges
        // (fewer sectors than squares) leave no gaps.
        let total = u128::from((self.end - self.start).max(1));
        let end = u128::from((lba + sectors).saturating_sub(self.start)) * CELLS as u128;
        let last = usize::try_from(end.div_ceil(total))
            .unwrap_or(CELLS)
            .clamp(first + 1, CELLS);
        for cell in &mut self.cells[first..last] {
            *cell = Some(cell.map_or(speed, |old| old.max(speed)));
        }
        self.position = self.position.max(lba + sectors);
    }

    /// The square sector `lba` falls in.
    fn cell(&self, lba: u64) -> usize {
        let total = u128::from((self.end - self.start).max(1));
        let at = u128::from(lba.saturating_sub(self.start)) * CELLS as u128 / total;
        usize::try_from(at).unwrap_or(CELLS).min(CELLS - 1)
    }

    /// The map in `count` squares (at most the 1000 kept).
    pub fn picture(&self, count: usize) -> String {
        let count = count.clamp(1, CELLS);
        (0..count)
            .map(|i| {
                self.cells[i * CELLS / count..(i + 1) * CELLS / count]
                    .iter()
                    .flatten()
                    .max()
                    .map_or('.', |speed| speed.symbol())
            })
            .collect()
    }

    pub fn progress(&self) -> SurfaceProgress {
        let [fast, normal, slow, very_slow, unreadable] = self.counts;
        let tested = self.position - self.start;
        let seconds = self.elapsed.as_secs_f64();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )] // a rate for people
        let rate = if seconds > 0.0 {
            ((tested * u64::from(self.sector_size)) as f64 / seconds) as u64
        } else {
            0
        };
        SurfaceProgress {
            start_lba: self.start,
            sectors: self.end - self.start,
            tested,
            counts: SpeedCounts {
                fast,
                normal,
                slow,
                very_slow,
                unreadable,
            },
            unreadable_sectors: self.unreadable_sectors,
            bad_ranges: self.bad.iter().take(100).copied().collect(),
            bad_range_count: self.bad_count,
            elapsed_ms: u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX),
            bytes_per_second: rate,
            picture: self.picture(CELLS),
            done: self.is_done(),
            stopped: self.stopped.clone(),
        }
    }
}

fn gone(error: &io::Error) -> String {
    format!("The disk stopped answering ({error}); it may have been unplugged.")
}

#[cfg(test)]
mod tests {
    use datareco_io::MemorySource;

    use super::*;

    #[test]
    fn read_times_are_sorted_into_speeds() {
        assert_eq!(classify(Duration::from_millis(3)), BlockSpeed::Fast);
        assert_eq!(classify(Duration::from_millis(20)), BlockSpeed::Normal);
        assert_eq!(classify(Duration::from_millis(250)), BlockSpeed::Slow);
        assert_eq!(classify(Duration::from_secs(3)), BlockSpeed::VerySlow);
    }

    #[test]
    fn finds_unreadable_blocks_and_merges_them() {
        // 4 MiB of 512-byte sectors: 16 blocks of 512 sectors.
        let sectors = 8192u64;
        let mut source =
            MemorySource::new(vec![7; 4 << 20], 512).with_bad_sectors([1000, 1500, 7000]);
        let mut test = SurfaceTest::whole(&source);
        let progress = test.step(&mut source, Duration::from_secs(60));
        assert!(progress.done);
        assert_eq!(progress.tested, sectors);
        assert_eq!(progress.counts.unreadable, 3, "blocks 1, 2 and 13");
        assert_eq!(
            progress.counts.fast + progress.counts.normal + progress.counts.slow,
            13
        );
        assert_eq!(
            progress.bad_ranges,
            [
                BadRange {
                    start_lba: 512,
                    sectors: 1024
                },
                BadRange {
                    start_lba: 6656,
                    sectors: 512
                },
            ]
        );
        assert_eq!(progress.unreadable_sectors, 1536);
        let picture: Vec<char> = progress.picture.chars().collect();
        for (lba, unreadable) in [
            (0, false),
            (1000, true),
            (1500, true),
            (4000, false),
            (7000, true),
        ] {
            assert_eq!(picture[test.cell(lba)] == 'x', unreadable, "sector {lba}");
        }
    }

    #[test]
    fn tests_part_of_a_disk_and_stops_where_told() {
        let mut source = MemorySource::new(vec![0; 1 << 20], 512);
        let mut test = SurfaceTest::new(512, 100, 600);
        let progress = test.step(&mut source, Duration::from_secs(60));
        assert!(progress.done);
        assert_eq!(
            (progress.start_lba, progress.sectors, progress.tested),
            (100, 600, 600)
        );
        assert_eq!(progress.picture.len(), 1000);
        assert!(!progress.picture.contains('.'));
    }

    #[test]
    fn the_map_shows_the_slowest_block_in_each_square() {
        let mut test = SurfaceTest::new(512, 0, 1000 * 512);
        test.record(0, 512, BlockSpeed::Fast);
        test.record(512, 512, BlockSpeed::VerySlow);
        test.record(1024, 512, BlockSpeed::Normal);
        let picture = test.picture(10);
        assert_eq!(picture.len(), 10);
        assert_eq!(&picture[..2], "v.", "the first square covers 100 blocks");
        assert_eq!(&test.picture(1000)[..4], "fvn.");
        let progress = test.progress();
        assert_eq!(progress.tested, 1536);
        assert!(!progress.done);
        assert_eq!(progress.counts.very_slow, 1);
    }

    #[test]
    fn a_step_stops_after_its_budget() {
        let mut source = MemorySource::new(vec![0; 64 << 20], 512);
        let mut test = SurfaceTest::whole(&source);
        let progress = test.step(&mut source, Duration::ZERO);
        assert_eq!(progress.tested, 0);
        assert!(!progress.done);
        let progress = test.step(&mut source, Duration::from_secs(60));
        assert!(progress.done);
    }
}
