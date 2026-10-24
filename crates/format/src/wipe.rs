//! Wiping: writing over a range of a disk, a step at a time, so it can be
//! shown, stopped and carried on.
//!
//! One pass is enough for hard drives. SSDs and USB sticks keep spare
//! blocks the computer can't reach, which may still hold old data after
//! any number of passes; only the drive's own erase command reaches those.

use std::io;
use std::time::{Duration, Instant};

use datareco_io::SectorTarget;
use serde::{Deserialize, Serialize};

/// What to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WipePattern {
    /// Zeros: quick, and the disk then reads as empty.
    Zeros,
    /// Random-looking bytes.
    Random,
}

/// Bytes written at a time.
const CHUNK: usize = 4 << 20;

/// How a wipe is going.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WipeProgress {
    /// Bytes to write, and written so far.
    pub total: u64,
    pub done: u64,
    pub bytes_per_second: u64,
    pub finished: bool,
}

/// Wiping bytes `start..end` of a target.
pub struct Wipe {
    start: u64,
    end: u64,
    position: u64,
    pattern: WipePattern,
    state: u64,
    buffer: Vec<u8>,
    elapsed: Duration,
}

impl Wipe {
    /// `start` and `end` must be on sector boundaries of the target.
    pub fn new(start: u64, end: u64, pattern: WipePattern) -> Wipe {
        Wipe {
            start,
            end: end.max(start),
            position: start,
            pattern,
            state: 0x9E37_79B9_7F4A_7C15 ^ start,
            buffer: vec![0; CHUNK],
            elapsed: Duration::ZERO,
        }
    }

    pub fn progress(&self) -> WipeProgress {
        let done = self.position - self.start;
        let seconds = self.elapsed.as_secs_f64();
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss
        )] // a rate for people
        let rate = if seconds > 0.0 {
            (done as f64 / seconds) as u64
        } else {
            0
        };
        WipeProgress {
            total: self.end - self.start,
            done,
            bytes_per_second: rate,
            finished: self.position >= self.end,
        }
    }

    /// Writes for about `budget`, then flushes and reports.
    pub fn step(
        &mut self,
        target: &mut dyn SectorTarget,
        budget: Duration,
    ) -> io::Result<WipeProgress> {
        let began = Instant::now();
        while self.position < self.end && began.elapsed() < budget {
            let n = usize::try_from((self.end - self.position).min(CHUNK as u64)).unwrap_or(CHUNK);
            if self.pattern == WipePattern::Random {
                self.fill(n);
            }
            let started = Instant::now();
            target.write_at(self.position, &self.buffer[..n])?;
            self.elapsed += started.elapsed();
            self.position += n as u64;
        }
        target.flush()?;
        Ok(self.progress())
    }

    /// Fills the buffer with xorshift output: fast, and nothing like what
    /// was there.
    fn fill(&mut self, n: usize) {
        for word in self.buffer[..n].chunks_mut(8) {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            let bytes = self.state.to_le_bytes();
            word.copy_from_slice(&bytes[..word.len()]);
        }
    }
}
