//! Moves on disks in memory, stopped by simulated power cuts at every
//! stage and carried on, then checked byte for byte.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datareco_format::{FormatKind, FormatOptions, plan_format, write_format};
use datareco_io::{Disk, MemoryTarget, SectorSource, SectorTarget};
use datareco_partitions::edit::{Operation, Style, plan};
use datareco_partitions::read_table;

use super::*;

const MIB: u64 = 2048;

/// A disk both read and written, which can lose power after a number of
/// writes: the write in progress then lands only half done.
#[derive(Clone)]
struct Shared {
    data: Arc<Mutex<Vec<u8>>>,
    writes_left: Arc<Mutex<Option<usize>>>,
}

impl Shared {
    fn new(data: Vec<u8>) -> Shared {
        Shared {
            data: Arc::new(Mutex::new(data)),
            writes_left: Arc::new(Mutex::new(None)),
        }
    }

    fn cut_power_after(&self, writes: usize) {
        *self.writes_left.lock().unwrap() = Some(writes);
    }

    fn bytes(&self) -> Vec<u8> {
        self.data.lock().unwrap().clone()
    }

    fn disk(&self) -> Disk {
        Disk::new(
            "Test disk",
            Box::new(datareco_io::MemorySource::new(self.bytes(), 512)),
        )
    }
}

impl SectorSource for Shared {
    fn sector_size(&self) -> u32 {
        512
    }
    fn size(&self) -> u64 {
        self.data.lock().unwrap().len() as u64
    }
    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        let data = self.data.lock().unwrap();
        let at = usize::try_from(lba * 512).unwrap();
        buf.copy_from_slice(&data[at..at + buf.len()]);
        Ok(())
    }
}

impl SectorTarget for Shared {
    fn sector_size(&self) -> u32 {
        512
    }
    fn capacity(&self) -> Option<u64> {
        Some(self.size())
    }
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let mut left = self.writes_left.lock().unwrap();
        let mut len = bytes.len();
        if let Some(n) = left.as_mut() {
            if *n == 0 {
                len /= 2; // a torn write
            } else {
                *n -= 1;
            }
        }
        let mut data = self.data.lock().unwrap();
        let at = usize::try_from(offset).unwrap();
        data[at..at + len].copy_from_slice(&bytes[..len]);
        if *left == Some(0) && len < bytes.len() {
            *left = None;
            return Err(io::Error::other("the power went"));
        }
        Ok(())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A 128 MiB MBR disk with one `kind` partition of 40 MiB at `start`,
/// filled with a pattern past its file system's start.
fn disk_with(kind: FormatKind, start: u64) -> Vec<u8> {
    disk_styled(kind, start, Style::Mbr)
}

fn disk_styled(kind: FormatKind, start: u64, style: Style) -> Vec<u8> {
    let mut target = MemoryTarget::with_data(vec![0; 128 << 20], 512);
    let image = Disk::new(
        "t",
        Box::new(datareco_io::MemorySource::new(target.data().to_vec(), 512)),
    );
    let table = plan(
        &image,
        &[
            Operation::Initialize { style },
            Operation::Create {
                start_lba: start,
                sectors: 40 * MIB,
                type_code: "ntfs".into(),
                logical: false,
                name: None,
            },
        ],
    )
    .unwrap();
    for w in &table.writes {
        target.write_at(w.lba * 512, &w.data).unwrap();
    }
    let options = FormatOptions {
        kind,
        label: Some("MOVE ME".into()),
        cluster_size: None,
        serial: Some(7),
        partition_offset: start,
    };
    let format = plan_format(40 << 20, 512, &options).unwrap();
    write_format(&format, &mut target, start * 512).unwrap();
    let mut data = target.data().to_vec();
    // Something recognizable at the partition's far end.
    let end = usize::try_from((start + 40 * MIB) * 512).unwrap();
    for (i, b) in data[end - (8 << 20)..end].iter_mut().enumerate() {
        *b = u8::try_from(i % 251).unwrap();
    }
    data
}

fn dirs(name: &str) -> (PathBuf, PathBuf) {
    let base =
        std::env::temp_dir().join(format!("datareco-relocate-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    (base.join("moves"), base.join("backups"))
}

/// Runs a move to the end, cutting the power after `cut` writes (if any)
/// and carrying on from the journal each time it does.
fn run_move(shared: &Shared, from: u64, to: u64, cut: Option<usize>, name: &str) -> MoveProgress {
    let (journal_dir, backup_dir) = dirs(name);
    let mut mover = Mover::begin(
        &shared.disk(),
        "image:test",
        from,
        to,
        &journal_dir,
        &backup_dir,
    )
    .unwrap();
    if let Some(cut) = cut {
        shared.cut_power_after(cut);
    }
    let mut cuts = 0;
    loop {
        let (mut source, mut target) = (shared.clone(), shared.clone());
        match mover.step(&mut source, &mut target, Duration::from_secs(60)) {
            Ok(progress) if progress.finished => {
                assert!(unfinished(&journal_dir, "image:test").is_none());
                let _ = fs::remove_dir_all(journal_dir.parent().unwrap());
                return progress;
            }
            Ok(_) => {}
            Err(e) => {
                // The power went: start again from the journal.
                assert!(e.to_string().contains("the power went"), "{e}");
                cuts += 1;
                assert!(cuts < 3, "it keeps failing");
                let journal = unfinished(&journal_dir, "image:test").expect("a journal is left");
                mover = Mover::resume(&journal, &shared.disk()).unwrap();
            }
        }
    }
}

/// The partition's bytes at `start`, with the file system's note of where
/// it starts put back as it was at `original`, to compare moves.
fn partition_at(data: &[u8], start: u64, kind: FormatKind) -> Vec<u8> {
    let at = usize::try_from(start * 512).unwrap();
    let mut bytes = data[at..at + usize::try_from(40 * MIB * 512).unwrap()].to_vec();
    match kind {
        FormatKind::Fat32 => {
            for sector in [0, 6] {
                bytes[sector * 512 + 28..sector * 512 + 32].fill(0);
            }
        }
        FormatKind::Exfat => {
            for region in [0, 12] {
                bytes[region * 512 + 64..region * 512 + 72].fill(0);
                bytes[(region + 11) * 512..(region + 12) * 512].fill(0);
            }
        }
    }
    bytes
}

fn check_moved(shared: &Shared, original: &[u8], from: u64, to: u64, kind: FormatKind) {
    let data = shared.bytes();
    assert_eq!(
        partition_at(&data, to, kind),
        partition_at(original, from, kind),
        "the data moved whole"
    );
    let disk = Arc::new(shared.disk());
    let table = read_table(&disk).unwrap();
    assert_eq!(table.partitions[0].start_lba, to);
    assert!(table.problems.is_empty(), "{:?}", table.problems);
    let fs = datareco_fs::open(Arc::clone(&disk), to * 512).unwrap();
    assert_eq!(fs.info().label.as_deref(), Some("MOVE ME"));
    let at = usize::try_from(to * 512).unwrap();
    match kind {
        FormatKind::Fat32 => {
            let hidden = u32::from_le_bytes(data[at + 28..at + 32].try_into().unwrap());
            assert_eq!(u64::from(hidden), to, "hidden sectors");
        }
        FormatKind::Exfat => {
            let offset = u64::from_le_bytes(data[at + 64..at + 72].try_into().unwrap());
            assert_eq!(offset, to, "PartitionOffset");
        }
    }
}

#[test]
fn moves_far_in_both_directions() {
    for (from, to) in [(MIB, 70 * MIB), (70 * MIB, MIB)] {
        let original = disk_with(FormatKind::Fat32, from);
        let shared = Shared::new(original.clone());
        let progress = run_move(&shared, from, to, None, &format!("far-{from}"));
        assert_eq!(progress.done, 40 << 20);
        assert!(progress.backup.is_some());
        check_moved(&shared, &original, from, to, FormatKind::Fat32);
    }
}

#[test]
fn moves_a_little_over_its_own_place() {
    // 1 MiB, less than a batch: each batch's data goes to the journal first.
    for (from, to) in [(10 * MIB, 11 * MIB), (11 * MIB, 10 * MIB)] {
        let original = disk_with(FormatKind::Exfat, from);
        let shared = Shared::new(original.clone());
        run_move(&shared, from, to, None, &format!("near-{from}"));
        check_moved(&shared, &original, from, to, FormatKind::Exfat);
    }
}

#[test]
fn carries_on_after_a_power_cut_at_every_stage() {
    // 3 batches, a boot fix and a table write: cut after each write in turn.
    for (from, to) in [(10 * MIB, 11 * MIB), (11 * MIB, 10 * MIB), (MIB, 70 * MIB)] {
        for cut in 0..9 {
            let original = disk_with(FormatKind::Fat32, from);
            let shared = Shared::new(original.clone());
            run_move(
                &shared,
                from,
                to,
                Some(cut),
                &format!("cut-{from}-{to}-{cut}"),
            );
            check_moved(&shared, &original, from, to, FormatKind::Fat32);
        }
    }
}

#[test]
fn carries_on_after_a_power_cut_on_a_gpt_disk() {
    // GPT writes its table in several pieces, any of which may be cut.
    for cut in 0..10 {
        let original = disk_styled(FormatKind::Exfat, MIB, Style::Gpt);
        let shared = Shared::new(original.clone());
        run_move(&shared, MIB, 70 * MIB, Some(cut), &format!("gpt-{cut}"));
        check_moved(&shared, &original, MIB, 70 * MIB, FormatKind::Exfat);
    }
}

#[test]
fn refuses_a_second_move_and_a_changed_table() {
    let original = disk_with(FormatKind::Fat32, MIB);
    let shared = Shared::new(original);
    let (journal_dir, backup_dir) = dirs("refuse");
    let mover = Mover::begin(
        &shared.disk(),
        "image:test",
        MIB,
        70 * MIB,
        &journal_dir,
        &backup_dir,
    )
    .unwrap();
    let again = Mover::begin(
        &shared.disk(),
        "image:test",
        MIB,
        80 * MIB,
        &journal_dir,
        &backup_dir,
    );
    assert!(again.err().unwrap().to_string().contains("hasn't finished"));
    // Something else changes the table meanwhile.
    shared.data.lock().unwrap()[446 + 4] = 0x83;
    let journal = mover.progress().journal;
    let resumed = Mover::resume(&journal, &shared.disk());
    assert!(
        resumed
            .err()
            .unwrap()
            .to_string()
            .contains("changed since the move started")
    );
    let _ = fs::remove_dir_all(journal_dir.parent().unwrap());
}
