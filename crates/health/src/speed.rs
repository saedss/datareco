//! Read speed test: how fast a disk reads in big blocks at its start,
//! middle and end (spinning disks slow down towards the end), and how many
//! small 4 KiB reads it serves at random places.
//!
//! It only reads, one request at a time, so SSD numbers come out below the
//! ones their makers quote (measured with many requests at once). Write
//! speed isn't tested: writing a test file could overwrite deleted files
//! someone wants back.

use std::io;
use std::time::{Duration, Instant};

use datareco_io::{SectorSource, is_device_gone};
use serde::Serialize;

const SEQUENTIAL_BLOCK: u64 = 1 << 20;
const RANDOM_BLOCK: u64 = 4096;
/// Most read at each place in the sequential phases.
const SEQUENTIAL_LIMIT: u64 = 1 << 30;
/// How long each phase reads, by default.
const PHASE_TIME: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Big blocks from a place: 0 start, 1 middle, 2 end.
    Sequential(u8),
    Random,
    Done,
}

/// Big-block reading at one place.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SequentialResult {
    /// "start", "middle" or "end".
    pub place: &'static str,
    pub start_lba: u64,
    pub bytes_per_second: u64,
}

/// Small reads at random places.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RandomResult {
    pub reads_per_second: u64,
    pub bytes_per_second: u64,
    /// Average time a read took, in microseconds.
    pub average_micros: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedStatus {
    pub sequential: Vec<SequentialResult>,
    pub random: Option<RandomResult>,
    /// Reads that failed (they count as time spent, not data read).
    pub errors: u64,
    /// How far through the test, in percent.
    pub percent: u8,
    pub done: bool,
    pub stopped: Option<String>,
}

/// A read speed test of a whole disk.
pub struct SpeedTest {
    sector_size: u32,
    sectors: u64,
    phase_time: Duration,
    phase: Phase,
    /// Where the current sequential phase started and reads next.
    from: u64,
    next: u64,
    spent: Duration,
    bytes: u64,
    reads: u64,
    rng: u64,
    status: SpeedStatus,
    buffer: Vec<u8>,
}

impl SpeedTest {
    pub fn new(source: &dyn SectorSource) -> SpeedTest {
        let sector_size = source.sector_size().max(1);
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| u64::from(d.subsec_nanos()) ^ d.as_secs());
        let mut test = SpeedTest {
            sector_size,
            sectors: source.size() / u64::from(sector_size),
            phase_time: PHASE_TIME,
            phase: Phase::Sequential(0),
            from: 0,
            next: 0,
            spent: Duration::ZERO,
            bytes: 0,
            reads: 0,
            rng: seed | 1,
            status: SpeedStatus {
                sequential: Vec::new(),
                random: None,
                errors: 0,
                percent: 0,
                done: false,
                stopped: None,
            },
            buffer: Vec::new(),
        };
        test.begin(Phase::Sequential(0));
        test
    }

    /// Reads for `phase_time` in each phase instead of 3 seconds (for tests).
    pub fn with_phase_time(mut self, phase_time: Duration) -> SpeedTest {
        self.phase_time = phase_time;
        self
    }

    pub fn status(&self) -> SpeedStatus {
        self.status.clone()
    }

    /// Reads for about `budget`, then reports.
    pub fn step(&mut self, source: &mut dyn SectorSource, budget: Duration) -> SpeedStatus {
        let began = Instant::now();
        while self.phase != Phase::Done && began.elapsed() < budget {
            if let Err(e) = self.read_once(source) {
                self.status.stopped = Some(format!(
                    "The disk stopped answering ({e}); it may have been unplugged."
                ));
                self.phase = Phase::Done;
                break;
            }
            if self.spent >= self.phase_time || self.phase_exhausted() {
                self.finish_phase();
            }
        }
        self.status.done = self.phase == Phase::Done;
        self.status.percent = self.percent();
        self.status.clone()
    }

    fn block_sectors(&self, bytes: u64) -> u64 {
        (bytes / u64::from(self.sector_size))
            .max(1)
            .min(self.sectors.max(1))
    }

    fn begin(&mut self, phase: Phase) {
        self.phase = if self.sectors == 0 {
            Phase::Done
        } else {
            phase
        };
        self.spent = Duration::ZERO;
        self.bytes = 0;
        self.reads = 0;
        if let Phase::Sequential(place) = phase {
            let span = self.block_sectors(SEQUENTIAL_LIMIT);
            self.from = match place {
                0 => 0,
                1 => (self.sectors / 2).min(self.sectors - span),
                _ => self.sectors - span,
            };
            self.next = self.from;
        }
    }

    fn phase_exhausted(&self) -> bool {
        match self.phase {
            Phase::Sequential(_) => {
                let span = self.block_sectors(SEQUENTIAL_LIMIT);
                self.next >= self.from + span
            }
            _ => false,
        }
    }

    /// One read; an error only when the disk is gone.
    fn read_once(&mut self, source: &mut dyn SectorSource) -> io::Result<()> {
        let (lba, count) = match self.phase {
            Phase::Sequential(_) => {
                let span_end = self.from + self.block_sectors(SEQUENTIAL_LIMIT);
                let count = self
                    .block_sectors(SEQUENTIAL_BLOCK)
                    .min(span_end - self.next);
                (self.next, count)
            }
            Phase::Random => {
                let count = self.block_sectors(RANDOM_BLOCK);
                let places = (self.sectors - count) / count + 1;
                (self.random() % places * count, count)
            }
            Phase::Done => return Ok(()),
        };
        let len = usize::try_from(count * u64::from(self.sector_size)).unwrap_or(0);
        self.buffer.resize(len, 0);
        let read = Instant::now();
        let result = source.read_sectors(lba, &mut self.buffer[..len]);
        self.spent += read.elapsed();
        self.reads += 1;
        self.next = lba + count;
        match result {
            Ok(()) => self.bytes += len as u64,
            Err(e) if is_device_gone(&e) => return Err(e),
            Err(_) => self.status.errors += 1,
        }
        Ok(())
    }

    fn finish_phase(&mut self) {
        let seconds = self.spent.as_secs_f64().max(1e-9);
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )] // rates for people
        let per_second = |n: u64| (n as f64 / seconds) as u64;
        match self.phase {
            Phase::Sequential(place) => {
                self.status.sequential.push(SequentialResult {
                    place: ["start", "middle", "end"][usize::from(place.min(2))],
                    start_lba: self.from,
                    bytes_per_second: per_second(self.bytes),
                });
                self.begin(if place < 2 {
                    Phase::Sequential(place + 1)
                } else {
                    Phase::Random
                });
            }
            Phase::Random => {
                let micros = u64::try_from(self.spent.as_micros()).unwrap_or(u64::MAX);
                self.status.random = Some(RandomResult {
                    reads_per_second: per_second(self.reads),
                    bytes_per_second: per_second(self.bytes),
                    average_micros: micros / self.reads.max(1),
                });
                self.phase = Phase::Done;
            }
            Phase::Done => {}
        }
    }

    fn percent(&self) -> u8 {
        let done_phases = match self.phase {
            Phase::Sequential(place) => u32::from(place),
            Phase::Random => 3,
            Phase::Done => return 100,
        };
        let within = (self.spent.as_secs_f64() / self.phase_time.as_secs_f64().max(1e-9)).min(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // 0–100
        let percent = ((f64::from(done_phases) + within) * 25.0) as u8;
        percent.min(99)
    }

    /// xorshift64: good enough to spread reads over the disk.
    fn random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }
}

#[cfg(test)]
mod tests {
    use datareco_io::MemorySource;

    use super::*;

    #[test]
    fn measures_each_place_then_random_reads() {
        let mut source = MemorySource::new(vec![0; 8 << 20], 512);
        let mut test = SpeedTest::new(&source).with_phase_time(Duration::from_millis(20));
        let mut status = test.status();
        for _ in 0..1000 {
            status = test.step(&mut source, Duration::from_millis(5));
            if status.done {
                break;
            }
        }
        assert!(status.done);
        assert_eq!(status.percent, 100);
        let places: Vec<_> = status.sequential.iter().map(|s| s.place).collect();
        assert_eq!(places, ["start", "middle", "end"]);
        // The whole 8 MiB disk is the span at every place.
        assert!(status.sequential.iter().all(|s| s.start_lba == 0));
        assert!(status.sequential.iter().all(|s| s.bytes_per_second > 0));
        let random = status.random.unwrap();
        assert!(random.reads_per_second > 0);
        assert_eq!(status.errors, 0);
    }

    #[test]
    fn places_are_spread_over_big_disks() {
        let source = MemorySource::new(vec![0; 512], 512);
        let mut test = SpeedTest::new(&source);
        test.sectors = 1 << 30; // 512 GiB, without the memory
        test.begin(Phase::Sequential(1));
        assert_eq!(test.from, 1 << 29);
        test.begin(Phase::Sequential(2));
        assert_eq!(test.from, (1 << 30) - (1 << 21));
        test.begin(Phase::Random);
        for _ in 0..100 {
            let count = test.block_sectors(RANDOM_BLOCK);
            let places = (test.sectors - count) / count + 1;
            let lba = test.random() % places * count;
            assert!(lba + count <= test.sectors && lba.is_multiple_of(8));
        }
    }

    #[test]
    fn unreadable_sectors_count_as_errors() {
        let mut source = MemorySource::new(vec![0; 1 << 20], 512).with_bad_sectors(0..2048);
        let mut test = SpeedTest::new(&source).with_phase_time(Duration::from_millis(5));
        let mut status = test.status();
        while !status.done {
            status = test.step(&mut source, Duration::from_millis(5));
        }
        assert!(status.errors > 0);
        assert!(status.sequential.iter().all(|s| s.bytes_per_second == 0));
    }

    #[test]
    fn an_empty_source_is_done_at_once() {
        let mut source = MemorySource::new(Vec::new(), 512);
        let mut test = SpeedTest::new(&source);
        assert!(test.step(&mut source, Duration::from_secs(1)).done);
    }
}
