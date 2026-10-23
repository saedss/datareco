//! The copy: reading a source onto a target in passes that get the most
//! data off a failing drive first, recording everything in a [`Map`] so the
//! copy can stop and carry on at any point.
//!
//! The passes, as in GNU ddrescue:
//!
//! 1. Copying, forwards, in big blocks. After a block fails, skip ahead (a
//!    little more after each failure in a row), because failing areas are
//!    slow to read and tend to come in clusters.
//! 2. Copying, backwards over what was skipped, skipping the same way.
//! 3. Copying whatever is left, without skipping.
//! 4. Trimming: reading failed blocks sector by sector from both edges
//!    inwards, until a sector fails at each edge.
//! 5. Scraping: reading the rest of failed blocks sector by sector.
//! 6. Retrying the bad sectors, forwards then backwards, as many passes as asked.
//! 7. Filling (only when asked, for disks): writing zeros over the bad
//!    sectors, so the target doesn't keep its old contents there.

use std::io;
use std::time::{Duration, Instant};

use datareco_io::{SectorSource, SectorTarget, is_device_gone};
use serde::Serialize;

use crate::map::{Area, Map, Phase, Totals};

/// How to copy.
#[derive(Clone, Debug)]
pub struct CopyOptions {
    /// Skip ahead after read errors in the first passes, and come back to
    /// the skipped areas later: gets the most data off a failing drive first.
    pub skip: bool,
    /// Passes over the bad sectors at the end.
    pub retries: u32,
    /// Write zeros over sectors that couldn't be read, when done. For disks:
    /// otherwise those sectors keep what the disk held before.
    pub fill_bad: bool,
    /// Bytes read at a time while copying (rounded up to whole sectors).
    pub block_size: usize,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            skip: true,
            retries: 1,
            fill_bad: false,
            block_size: 1024 * 1024,
        }
    }
}

/// Why a copy stopped.
#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error(
        "The source stopped answering ({0}). Was it unplugged? Connect it again and carry on \
         the copy with the same map file."
    )]
    SourceGone(#[source] io::Error),
    #[error("Writing to the target failed at byte {offset}: {source}")]
    Write {
        offset: u64,
        #[source]
        source: io::Error,
    },
    #[error("{0}")]
    Mismatch(String),
}

/// Where a copy is, for people.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyProgress {
    pub phase: Phase,
    pub pass: u32,
    /// What the copy is doing, in words: "Copying, pass 1 (forwards)".
    pub description: String,
    /// Where in the source it is.
    pub position: u64,
    pub size: u64,
    pub totals: Totals,
    /// Read errors since this copy was started or carried on.
    pub read_errors: u64,
    pub finished: bool,
}

/// A copy in progress. Call [`Copier::step`] until it is finished, saving
/// [`Copier::map`] now and then; a copy made from a saved map carries on
/// where that one was.
pub struct Copier {
    map: Map,
    options: CopyOptions,
    sector: u64,
    block: u64,
    /// How far the copying passes skip after the next error.
    skip: u64,
    /// Trimming is working on the end of a failed block (its start failed).
    trim_back: bool,
    buffer: Vec<u8>,
    read_errors: u64,
}

impl Copier {
    /// Starts a copy, or carries one on, from `map`. `sector_size` is the
    /// source's. A finished map with bad sectors goes back to retrying them
    /// when `options.retries` asks for retries.
    pub fn new(map: Map, sector_size: u32, options: CopyOptions) -> Result<Copier, CopyError> {
        if sector_size == 0 || !sector_size.is_power_of_two() {
            return Err(CopyError::Mismatch(format!(
                "The source has {sector_size}-byte sectors, which DataReco can't copy."
            )));
        }
        let sector = u64::from(sector_size);
        let block = (options.block_size.max(1) as u64).next_multiple_of(sector);
        let buffer = vec![0; usize::try_from(block).unwrap_or(usize::MAX)];
        let mut copy = Copier {
            map,
            options,
            sector,
            block,
            skip: 0,
            trim_back: false,
            buffer,
            read_errors: 0,
        };
        // Carrying on a finished copy means trying its bad sectors again.
        if copy.map.phase == Phase::Finished
            && copy.options.retries > 0
            && copy.map.totals().bad > 0
        {
            copy.map.phase = Phase::Retrying;
            copy.map.pass = 1;
            copy.map.position = 0;
        }
        copy.settle();
        Ok(copy)
    }

    pub fn map(&self) -> &Map {
        &self.map
    }

    pub fn is_finished(&self) -> bool {
        self.map.phase == Phase::Finished
    }

    pub fn progress(&self) -> CopyProgress {
        CopyProgress {
            phase: self.map.phase,
            pass: self.map.pass,
            description: self.describe(),
            position: self.map.position,
            size: self.map.size(),
            totals: self.map.totals(),
            read_errors: self.read_errors,
            finished: self.is_finished(),
        }
    }

    fn describe(&self) -> String {
        let pass = self.map.pass;
        match self.map.phase {
            Phase::Copying => match pass {
                1 if self.options.skip => "Copying, pass 1 (forwards, skipping past errors)".into(),
                1 => "Copying".into(),
                2 => "Copying, pass 2 (backwards over what was skipped)".into(),
                _ => "Copying, pass 3 (what's left)".into(),
            },
            Phase::Trimming => "Trimming: reading the edges of failed blocks".into(),
            Phase::Scraping => "Scraping: reading failed blocks sector by sector".into(),
            Phase::Retrying => format!(
                "Retrying bad sectors, pass {pass} of {}",
                self.options.retries.max(pass)
            ),
            Phase::Filling => "Writing zeros over bad sectors".into(),
            Phase::Finished => "Finished".into(),
        }
    }

    /// Copies for about `budget` (at least one read), then reports where
    /// the copy is. Read errors are recorded in the map; write errors and a
    /// source that has gone away stop the copy.
    pub fn step(
        &mut self,
        source: &mut dyn SectorSource,
        target: &mut dyn SectorTarget,
        budget: Duration,
    ) -> Result<CopyProgress, CopyError> {
        self.check(source, target)?;
        let started = Instant::now();
        while !self.is_finished() {
            self.one(source, target)?;
            if started.elapsed() >= budget {
                break;
            }
        }
        Ok(self.progress())
    }

    fn check(&self, source: &dyn SectorSource, target: &dyn SectorTarget) -> Result<(), CopyError> {
        if source.size() != self.map.size() {
            return Err(CopyError::Mismatch(format!(
                "The map is for a source of {} bytes, but this one has {} bytes. Is it the \
                 right map?",
                self.map.size(),
                source.size()
            )));
        }
        if u64::from(source.sector_size()) != self.sector {
            return Err(CopyError::Mismatch(
                "The source's sector size changed during the copy.".to_owned(),
            ));
        }
        let target_sector = u64::from(target.sector_size().max(1));
        if !self.sector.is_multiple_of(target_sector) {
            return Err(CopyError::Mismatch(format!(
                "The target has {target_sector}-byte sectors and the source has {}-byte \
                 sectors, so the copy can't line up with them.",
                self.sector
            )));
        }
        if let Some(capacity) = target.capacity()
            && capacity < self.map.size()
        {
            return Err(CopyError::Mismatch(format!(
                "The target holds {capacity} bytes, less than the {} bytes being copied.",
                self.map.size()
            )));
        }
        Ok(())
    }

    /// One read (or one fill write), or moving on to the next pass.
    fn one(
        &mut self,
        source: &mut dyn SectorSource,
        target: &mut dyn SectorTarget,
    ) -> Result<(), CopyError> {
        let position = self.map.position;
        let size = self.map.size();
        match (self.map.phase, self.map.pass) {
            (Phase::Copying, 2) => {
                let Some((start, end)) = self.map.previous(position, &[Area::Untried]) else {
                    return self.advance();
                };
                let start = start.max(align_down(end - 1, self.block));
                if self.read(source, target, start, end, Area::Untrimmed)? {
                    self.skip = 0;
                    self.map.position = start;
                } else {
                    self.grow_skip();
                    self.map.position = start.saturating_sub(self.skip);
                }
            }
            (Phase::Copying, pass) => {
                let Some((start, end)) = self.map.next(position, &[Area::Untried]) else {
                    return self.advance();
                };
                let end = end.min(align_down(start, self.block).saturating_add(self.block));
                if self.read(source, target, start, end, Area::Untrimmed)? {
                    self.skip = 0;
                    self.map.position = end;
                } else if pass == 1 {
                    self.grow_skip();
                    self.map.position = end.saturating_add(self.skip).min(size);
                } else {
                    self.map.position = end;
                }
            }
            (Phase::Trimming, _) => {
                let Some((start, end)) = self.map.next(position, &[Area::Untrimmed]) else {
                    return self.advance();
                };
                if self.trim_back {
                    let last = start.max(align_down(end - 1, self.sector));
                    let read = self.read(source, target, last, end, Area::Bad)?;
                    if !read || last == start {
                        self.map.set(start, last, Area::Unscraped);
                        self.trim_back = false;
                        self.map.position = end;
                    }
                } else {
                    let first = self.sector_end(start, end);
                    if !self.read(source, target, start, first, Area::Bad)? && first < end {
                        self.trim_back = true;
                    }
                    self.map.position = first;
                }
            }
            (Phase::Scraping, _) => {
                let Some((start, end)) = self.map.next(position, &[Area::Unscraped]) else {
                    return self.advance();
                };
                let first = self.sector_end(start, end);
                self.read(source, target, start, first, Area::Bad)?;
                self.map.position = first;
            }
            (Phase::Retrying, pass) if pass % 2 == 0 => {
                let Some((start, end)) = self.map.previous(position, &[Area::Bad]) else {
                    return self.advance();
                };
                let last = start.max(align_down(end - 1, self.sector));
                self.read(source, target, last, end, Area::Bad)?;
                self.map.position = last;
            }
            (Phase::Retrying, _) => {
                let Some((start, end)) = self.map.next(position, &[Area::Bad]) else {
                    return self.advance();
                };
                let first = self.sector_end(start, end);
                self.read(source, target, start, first, Area::Bad)?;
                self.map.position = first;
            }
            (Phase::Filling, _) => {
                let Some((start, end)) = self.map.next(position, &[Area::Bad]) else {
                    return self.advance();
                };
                // Only whole sectors, in case a map made elsewhere doesn't
                // follow this source's sectors.
                let from = start.next_multiple_of(self.sector);
                let to = if end == size {
                    end
                } else {
                    align_down(end, self.sector)
                };
                let to = to.min(from.saturating_add(self.block));
                if from < to {
                    let length = usize::try_from(to - from).unwrap_or(self.buffer.len());
                    self.buffer[..length].fill(0);
                    target
                        .write_at(from, &self.buffer[..length])
                        .map_err(|source| CopyError::Write {
                            offset: from,
                            source,
                        })?;
                }
                self.map.position = if from < to { to } else { end };
            }
            (Phase::Finished, _) => {}
        }
        Ok(())
    }

    /// Reads `start..end` and writes it to the target. On success the area
    /// becomes copied; on failure it becomes `failed`. Returns whether it read.
    fn read(
        &mut self,
        source: &mut dyn SectorSource,
        target: &mut dyn SectorTarget,
        start: u64,
        end: u64,
        failed: Area,
    ) -> Result<bool, CopyError> {
        let size = self.map.size();
        // Maps made elsewhere may not follow this source's sectors.
        let from = align_down(start, self.sector);
        let to = end.min(size).next_multiple_of(self.sector);
        // Every caller asks for at most a block.
        let length = usize::try_from(to.saturating_sub(from))
            .ok()
            .filter(|&length| length > 0 && length <= self.buffer.len())
            .ok_or_else(|| {
                CopyError::Mismatch(format!("Can't read bytes {start} to {end} in one go."))
            })?;
        let buffer = &mut self.buffer[..length];
        match source.read_sectors(from / self.sector, buffer) {
            Ok(()) => {
                let stored = to.min(size);
                let stored_length = usize::try_from(stored - from).unwrap_or(length);
                target
                    .write_at(from, &buffer[..stored_length])
                    .map_err(|source| CopyError::Write {
                        offset: from,
                        source,
                    })?;
                self.map.set(from, stored, Area::Copied);
                Ok(true)
            }
            Err(error) if is_device_gone(&error) => Err(CopyError::SourceGone(error)),
            Err(_) => {
                self.read_errors += 1;
                self.map.set(start, end, failed);
                Ok(false)
            }
        }
    }

    /// The end of the sector starting at `start`, within `..end`.
    fn sector_end(&self, start: u64, end: u64) -> u64 {
        align_down(start, self.sector)
            .saturating_add(self.sector)
            .min(end)
    }

    fn grow_skip(&mut self) {
        if !self.options.skip {
            self.skip = 0;
            return;
        }
        // Up to 1% of the source (at most 1 GiB), in whole blocks so the
        // map stays in whole sectors.
        let largest = align_down((self.map.size() / 100).min(1 << 30), self.block).max(self.block);
        self.skip = if self.skip == 0 {
            self.block
        } else {
            self.skip.saturating_mul(2).min(largest)
        };
    }

    /// Moves on to the next pass or phase.
    fn advance(&mut self) -> Result<(), CopyError> {
        let size = self.map.size();
        let (phase, pass, position) = match (self.map.phase, self.map.pass) {
            (Phase::Copying, 1) if self.options.skip => (Phase::Copying, 2, size),
            (Phase::Copying, 2) => (Phase::Copying, 3, 0),
            (Phase::Copying, _) => (Phase::Trimming, 1, 0),
            (Phase::Trimming, _) => (Phase::Scraping, 1, 0),
            (Phase::Scraping, _) => (Phase::Retrying, 1, 0),
            (Phase::Retrying, pass) => {
                let next = pass.saturating_add(1);
                (Phase::Retrying, next, if next % 2 == 0 { size } else { 0 })
            }
            (Phase::Filling | Phase::Finished, _) => (Phase::Finished, 1, 0),
        };
        self.map.phase = phase;
        self.map.pass = pass;
        self.map.position = position;
        self.skip = 0;
        self.trim_back = false;
        self.settle();
        Ok(())
    }

    /// Skips phases with nothing to do, and goes back to earlier phases if
    /// the map still has areas only they handle (maps made elsewhere, or
    /// a finished copy asked to retry its bad sectors).
    fn settle(&mut self) {
        let size = self.map.size();
        self.map.position = self.map.position.min(size);
        self.map.pass = self.map.pass.max(1);
        if self.map.phase == Phase::Copying && self.map.pass > 3 {
            self.map.pass = 3;
        }
        if self.map.phase == Phase::Retrying && self.map.pass > self.options.retries {
            self.map.phase = Phase::Filling;
            self.map.pass = 1;
            self.map.position = 0;
        }
        if self.map.phase == Phase::Filling && !self.options.fill_bad {
            self.map.phase = Phase::Finished;
        }
        if self.map.phase == Phase::Finished {
            let totals = self.map.totals();
            let restart = if totals.untried > 0 {
                Some((Phase::Copying, 3))
            } else if totals.untrimmed > 0 {
                Some((Phase::Trimming, 1))
            } else if totals.unscraped > 0 {
                Some((Phase::Scraping, 1))
            } else {
                None
            };
            let (phase, pass) = restart.unwrap_or((Phase::Finished, 1));
            self.map.phase = phase;
            self.map.pass = pass;
            self.map.position = 0;
        }
    }
}

fn align_down(value: u64, unit: u64) -> u64 {
    value - value % unit
}
