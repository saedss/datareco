//! Copies of disks with simulated bad sectors, interrupted and carried on.

use std::io;
use std::sync::atomic::Ordering;
use std::time::Duration;

use datareco_io::{MemorySource, MemoryTarget, SectorSource, SectorTarget};

use crate::{Area, Copier, CopyError, CopyOptions, CopyProgress, Map, Phase};

const SECTOR: usize = 512;

/// Bytes that differ from sector to sector.
fn pattern(size: usize) -> Vec<u8> {
    let mut state = 0x2545_F491_u32;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn options(block_size: usize) -> CopyOptions {
    CopyOptions {
        block_size,
        ..CopyOptions::default()
    }
}

fn finish(
    copy: &mut Copier,
    source: &mut dyn SectorSource,
    target: &mut dyn SectorTarget,
) -> CopyProgress {
    loop {
        let progress = copy.step(source, target, Duration::from_secs(60)).unwrap();
        if progress.finished {
            return progress;
        }
    }
}

fn sector(data: &[u8], number: usize) -> &[u8] {
    &data[number * SECTOR..(number + 1) * SECTOR]
}

/// A 2 MiB disk with bad sectors in small clusters, one bad stretch, one
/// at the very end, and one that fails once and then reads.
const SIZE: usize = 2 * 1024 * 1024;
const SECTORS: usize = SIZE / SECTOR;

fn bad_sectors() -> Vec<u64> {
    let mut bad = vec![100, 101, 102, 2000];
    bad.extend(3000..3100);
    bad.push(SECTORS as u64 - 1);
    bad
}

fn failing_disk(data: &[u8]) -> MemorySource {
    MemorySource::new(data.to_vec(), 512)
        .with_bad_sectors(bad_sectors())
        .with_flaky_sector(500, 1)
}

#[test]
fn a_healthy_disk_copies_in_one_pass() {
    let data = pattern(1024 * 1024);
    let mut source = MemorySource::new(data.clone(), 512);
    let reads = source.read_counter();
    let mut target = MemoryTarget::growing(512);
    let mut copy = Copier::new(Map::new(data.len() as u64), 512, options(64 * 1024)).unwrap();
    let progress = finish(&mut copy, &mut source, &mut target);
    assert_eq!(target.data(), data);
    assert_eq!(progress.totals.copied, data.len() as u64);
    assert_eq!(progress.read_errors, 0);
    assert_eq!(reads.load(Ordering::Relaxed), 16);
    assert_eq!(
        copy.map().areas().collect::<Vec<_>>(),
        [(0, data.len() as u64, Area::Copied)]
    );
}

#[test]
fn bad_sectors_are_mapped_and_everything_else_is_copied() {
    let data = pattern(SIZE);
    let mut source = failing_disk(&data);
    let mut target = MemoryTarget::disk(SIZE, 512, 0xEE);
    let mut copy = Copier::new(Map::new(SIZE as u64), 512, options(64 * 1024)).unwrap();
    let progress = finish(&mut copy, &mut source, &mut target);

    let bad = bad_sectors();
    assert_eq!(progress.totals.bad, (bad.len() * SECTOR) as u64);
    assert_eq!(progress.totals.bad_areas, 4);
    assert_eq!(progress.totals.copied, SIZE as u64 - progress.totals.bad);
    for number in 0..SECTORS {
        let state = copy.map().area_at((number * SECTOR) as u64).unwrap().2;
        if bad.contains(&(number as u64)) {
            assert_eq!(state, Area::Bad, "sector {number}");
            // Without filling, a disk keeps what it held there.
            assert!(sector(target.data(), number).iter().all(|&b| b == 0xEE));
        } else {
            assert_eq!(state, Area::Copied, "sector {number}");
            assert_eq!(sector(target.data(), number), sector(&data, number));
        }
    }
}

#[test]
fn an_interrupted_copy_carries_on_to_the_same_result() {
    let data = pattern(SIZE);
    let mut source = failing_disk(&data);
    let mut target = MemoryTarget::disk(SIZE, 512, 0xEE);
    let mut copy = Copier::new(Map::new(SIZE as u64), 512, options(64 * 1024)).unwrap();
    finish(&mut copy, &mut source, &mut target);
    let expected_map = copy.map().to_text();

    // The same copy, stopped after every few reads and carried on from its
    // saved map, through every phase.
    let mut source = failing_disk(&data);
    let mut interrupted = MemoryTarget::disk(SIZE, 512, 0xEE);
    let mut copy = Copier::new(Map::new(SIZE as u64), 512, options(64 * 1024)).unwrap();
    let mut phases = Vec::new();
    let mut steps = 0;
    while !copy.is_finished() {
        let progress = copy
            .step(&mut source, &mut interrupted, Duration::ZERO)
            .unwrap();
        if phases.last() != Some(&(progress.phase, progress.pass)) {
            phases.push((progress.phase, progress.pass));
        }
        steps += 1;
        if steps % 7 == 0 {
            let saved = Map::parse(&copy.map().to_text()).unwrap();
            copy = Copier::new(saved, 512, options(64 * 1024)).unwrap();
        }
    }
    assert_eq!(copy.map().to_text(), expected_map);
    assert_eq!(interrupted.data(), target.data());
    for phase in [
        (Phase::Copying, 2),
        (Phase::Copying, 3),
        (Phase::Trimming, 1),
        (Phase::Scraping, 1),
        (Phase::Retrying, 1),
    ] {
        assert!(phases.contains(&phase), "{phase:?} not in {phases:?}");
    }
}

#[test]
fn skipping_gets_the_far_side_of_a_bad_stretch_first() {
    let data = pattern(1024 * 1024);
    let bad_stretch = 512..1024u64;
    let first_pass = |skip: bool| {
        let mut source = MemorySource::new(data.clone(), 512).with_bad_sectors(bad_stretch.clone());
        let mut target = MemoryTarget::growing(512);
        let options = CopyOptions {
            skip,
            ..options(4096)
        };
        let mut copy = Copier::new(Map::new(data.len() as u64), 512, options).unwrap();
        let mut errors = 0;
        while copy.progress().phase == Phase::Copying && copy.progress().pass == 1 {
            errors = copy
                .step(&mut source, &mut target, Duration::ZERO)
                .unwrap()
                .read_errors;
        }
        (copy, errors)
    };
    let (copy, errors) = first_pass(true);
    let totals = copy.map().totals();
    assert!(totals.untried > 0, "some of the stretch was skipped");
    assert!(errors < 64, "{errors} read errors in the first pass");
    assert_eq!(
        copy.map().area_at(data.len() as u64 - 1).unwrap().2,
        Area::Copied
    );
    assert_eq!(copy.progress().pass, 2);
    let (copy, errors) = first_pass(false);
    assert_eq!(copy.map().totals().untried, 0);
    assert_eq!(errors, 64);
}

#[test]
fn disks_can_have_bad_sectors_filled_with_zeros() {
    let data = pattern(SIZE);
    let mut source = failing_disk(&data);
    let mut target = MemoryTarget::disk(SIZE, 512, 0xEE);
    let options = CopyOptions {
        fill_bad: true,
        ..options(64 * 1024)
    };
    let mut copy = Copier::new(Map::new(SIZE as u64), 512, options).unwrap();
    finish(&mut copy, &mut source, &mut target);
    for number in 0..SECTORS {
        if bad_sectors().contains(&(number as u64)) {
            assert!(sector(target.data(), number).iter().all(|&b| b == 0));
        } else {
            assert_eq!(sector(target.data(), number), sector(&data, number));
        }
    }
}

#[test]
fn carrying_on_a_finished_copy_retries_its_bad_sectors() {
    let data = pattern(64 * 1024);
    // Fails when copied, trimmed and retried, then reads.
    let mut source = MemorySource::new(data.clone(), 512).with_flaky_sector(40, 3);
    let reads = source.read_counter();
    let mut target = MemoryTarget::growing(512);
    let mut copy = Copier::new(Map::new(data.len() as u64), 512, options(4096)).unwrap();
    let progress = finish(&mut copy, &mut source, &mut target);
    assert_eq!(progress.totals.bad, 512);

    let finished = copy.map().clone();
    let no_retries = CopyOptions {
        retries: 0,
        ..options(4096)
    };
    let before = reads.load(Ordering::Relaxed);
    let mut copy = Copier::new(finished.clone(), 512, no_retries).unwrap();
    assert!(copy.is_finished());
    finish(&mut copy, &mut source, &mut target);
    assert_eq!(reads.load(Ordering::Relaxed), before);

    let mut copy = Copier::new(finished, 512, options(4096)).unwrap();
    assert_eq!(copy.progress().phase, Phase::Retrying);
    let progress = finish(&mut copy, &mut source, &mut target);
    assert_eq!(progress.totals.bad, 0);
    assert_eq!(target.data(), data);
}

/// A source whose device goes away after some reads.
struct Unplugged {
    inner: MemorySource,
    reads_left: u32,
}

impl SectorSource for Unplugged {
    fn sector_size(&self) -> u32 {
        self.inner.sector_size()
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        if self.reads_left == 0 {
            #[cfg(windows)]
            return Err(io::Error::from_raw_os_error(1167));
            #[cfg(not(windows))]
            return Err(io::ErrorKind::NotFound.into());
        }
        self.reads_left -= 1;
        self.inner.read_sectors(lba, buf)
    }
}

#[test]
fn a_source_that_goes_away_stops_the_copy() {
    let data = pattern(64 * 1024);
    let mut source = Unplugged {
        inner: MemorySource::new(data, 512),
        reads_left: 3,
    };
    let mut target = MemoryTarget::growing(512);
    let mut copy = Copier::new(Map::new(64 * 1024), 512, options(4096)).unwrap();
    let error = copy
        .step(&mut source, &mut target, Duration::from_secs(60))
        .unwrap_err();
    assert!(matches!(error, CopyError::SourceGone(_)), "{error}");
    let totals = copy.map().totals();
    assert_eq!(
        (totals.copied, totals.bad, totals.untrimmed),
        (3 * 4096, 0, 0)
    );
}

/// An image file that ends partway through its last sector.
struct Partial(Vec<u8>);

impl SectorSource for Partial {
    fn sector_size(&self) -> u32 {
        512
    }

    fn size(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        let start = usize::try_from(lba).unwrap() * 512;
        buf.fill(0);
        let available = self.0.len().saturating_sub(start).min(buf.len());
        buf[..available].copy_from_slice(&self.0[start..start + available]);
        Ok(())
    }
}

#[test]
fn images_ending_mid_sector_copy_exactly() {
    let data = pattern(1000);
    let mut source = Partial(data.clone());
    let mut target = MemoryTarget::growing(512);
    let mut copy = Copier::new(Map::new(1000), 512, CopyOptions::default()).unwrap();
    finish(&mut copy, &mut source, &mut target);
    assert_eq!(target.data(), data);
}

#[test]
fn wrong_maps_and_targets_are_refused() {
    let mut source = MemorySource::new(vec![0; 8192], 512);
    let mut copy = Copier::new(Map::new(4096), 512, CopyOptions::default()).unwrap();
    let error = copy
        .step(&mut source, &mut MemoryTarget::growing(512), Duration::ZERO)
        .unwrap_err();
    assert!(error.to_string().contains("right map"), "{error}");

    let mut copy = Copier::new(Map::new(8192), 512, CopyOptions::default()).unwrap();
    let error = copy
        .step(
            &mut source,
            &mut MemoryTarget::disk(4096, 512, 0),
            Duration::ZERO,
        )
        .unwrap_err();
    assert!(error.to_string().contains("less than"), "{error}");
    let error = copy
        .step(
            &mut source,
            &mut MemoryTarget::disk(8192, 4096, 0),
            Duration::ZERO,
        )
        .unwrap_err();
    assert!(error.to_string().contains("line up"), "{error}");
    assert!(Copier::new(Map::new(8192), 0, CopyOptions::default()).is_err());
}

#[test]
fn copies_started_by_ddrescue_carry_on() {
    let data = pattern(64 * 1024);
    let map = Map::parse(
        "# Mapfile. Created by GNU ddrescue version 1.27\n\
         0x00004000     *               1\n\
         0x00000000  0x00004000  +\n\
         0x00004000  0x00001000  *\n\
         0x00005000  0x0000B000  ?\n",
    )
    .unwrap();
    let mut source = MemorySource::new(data.clone(), 512);
    let mut target = MemoryTarget::growing(512);
    target.write_at(0, &data[..0x4000]).unwrap();
    let mut copy = Copier::new(map, 512, options(4096)).unwrap();
    let progress = finish(&mut copy, &mut source, &mut target);
    assert_eq!(progress.totals.copied, data.len() as u64);
    assert_eq!(target.data(), data);
}
