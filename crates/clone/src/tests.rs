//! Copies between disks in memory, read back with DataReco's own readers.

use std::sync::{Arc, Mutex};

use datareco_fs::testutil::{ExfatBuilder, FatBuilder, noise, text};
use datareco_fs::{FileSystem, FsKind, read_to_end, resolve};
use datareco_io::{Disk, MemorySource, MemoryTarget};
use datareco_partitions::edit::{
    DiskCopyOptions, Operation, Style, list_backups, plan, plan_disk_copy, plan_partition_copy,
};
use datareco_partitions::read_table;

use super::*;

const MIB: u64 = 2048;

fn disk_of(data: &[u8]) -> Disk {
    Disk::new("Source", Box::new(MemorySource::new(data.to_vec(), 512)))
}

/// Writes a table made of `ops` onto `data`.
fn with_table(data: &mut [u8], ops: &[Operation]) {
    let planned = plan(&disk_of(data), ops).unwrap();
    assert!(
        planned.is_ready(),
        "{:?} {:?}",
        planned.blocked,
        planned.error
    );
    for w in &planned.writes {
        let at = usize::try_from(w.lba * 512).unwrap();
        data[at..at + w.data.len()].copy_from_slice(&w.data);
    }
}

fn put(data: &mut [u8], lba: u64, bytes: &[u8]) {
    let at = usize::try_from(lba * 512).unwrap();
    data[at..at + bytes.len()].copy_from_slice(bytes);
}

fn create(start: u64, sectors: u64, kind: &str) -> Operation {
    Operation::Create {
        start_lba: start,
        sectors,
        type_code: kind.into(),
        logical: false,
        name: None,
    }
}

/// A FAT32 volume of 34 MiB (enough 512-byte clusters to be FAT32) with a
/// few files, and plenty of free space.
fn fat32() -> Vec<u8> {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    let root = b.root();
    b.file(root, "readme.txt", &text("readme", 3000));
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "photo.bin", &noise(7, 300_000));
    b.build()
}

/// An exFAT volume of 8 MiB with a file.
fn exfat() -> Vec<u8> {
    let mut b = ExfatBuilder::new(16_384);
    let root = b.root();
    b.file(root, "notes.txt", &text("notes", 20_000));
    b.build()
}

/// A 256 MiB GPT disk: FAT32 at 1 MiB (40 MiB), exFAT at 128 MiB (16 MiB)
/// and something DataReco can't read at 200 MiB (8 MiB, all of it data).
fn source_disk() -> Vec<u8> {
    let mut data = vec![0u8; usize::try_from(256 * MIB * 512).unwrap()];
    with_table(
        &mut data,
        &[
            Operation::Initialize { style: Style::Gpt },
            create(MIB, 40 * MIB, "fat32"),
            create(128 * MIB, 16 * MIB, "ntfs"),
            create(200 * MIB, 8 * MIB, "linux"),
        ],
    );
    put(&mut data, MIB, &fat32());
    put(&mut data, 128 * MIB, &exfat());
    put(
        &mut data,
        200 * MIB,
        &noise(9, usize::try_from(8 * MIB * 512).unwrap()),
    );
    data
}

fn run(job: Job, sources: &mut CloneSources, target: &mut dyn SectorTarget) -> CloneProgress {
    let mut cloner = Cloner::new(job);
    for _ in 0..10_000 {
        let progress = cloner
            .step(sources, target, Duration::from_millis(50))
            .unwrap();
        if progress.finished {
            return progress;
        }
    }
    panic!("the copy never finished");
}

fn sources_for(data: &[u8]) -> CloneSources {
    CloneSources {
        disk: Box::new(MemorySource::new(data.to_vec(), 512)),
        snapshots: HashMap::new(),
    }
}

fn file(fs: &dyn FileSystem, path: &str) -> Vec<u8> {
    let entry = resolve(fs, path, false).unwrap();
    read_to_end(fs, &entry.id, None, u64::MAX).unwrap()
}

#[test]
fn ranges_skip_free_space_and_join_short_gaps() {
    let allocation = Allocation {
        used: vec![(0, 4096), (4096 + 100, 8192), (10 << 20, (10 << 20) + 512)],
        covered: 20 << 20,
    };
    assert_eq!(
        ranges_to_copy(Some(&allocation), 21 << 20, 512),
        [
            (0, 8192),
            (10 << 20, (10 << 20) + 512),
            (20 << 20, 21 << 20)
        ],
        "touching ranges join, the area past the clusters is copied"
    );
    assert_eq!(ranges_to_copy(None, 4096, 512), [(0, 4096)]);
}

#[test]
fn copies_a_disk_onto_a_smaller_one_used_space_only() {
    let data = source_disk();
    let source = disk_of(&data);
    let copy = plan_disk_copy(
        &source,
        "Target",
        96 * MIB,
        512,
        &DiskCopyOptions::default(),
    )
    .unwrap();
    assert!(copy.packed);
    let shared = Arc::new(source);
    let allocations: Vec<_> = copy
        .parts
        .iter()
        .map(|p| allocation(Arc::clone(&shared), p.from_lba * 512))
        .collect();
    assert!(allocations[0].is_some() && allocations[1].is_some());
    assert!(allocations[2].is_none(), "unreadable: copied whole");
    let job = Job::disk(&copy, &allocations, 512, "Copy".into()).unwrap();
    let whole: u64 = copy.parts.iter().map(|p| p.sectors * 512).sum();
    assert!(job.total() < whole / 2, "free space isn't copied");

    // The target held something else before.
    let mut target =
        MemoryTarget::with_data(vec![0xEE; usize::try_from(96 * MIB * 512).unwrap()], 512);
    let progress = run(job, &mut sources_for(&data), &mut target);
    assert_eq!(progress.done, progress.total);
    assert_eq!(progress.unreadable, 0);

    let copied = Arc::new(disk_of(target.data()));
    let table = read_table(&copied).unwrap();
    let starts: Vec<_> = table.partitions.iter().map(|p| p.start_lba).collect();
    assert_eq!(starts, [MIB, 41 * MIB, 57 * MIB]);
    let fat = datareco_fs::open(Arc::clone(&copied), MIB * 512).unwrap();
    assert_eq!(fat.info().kind, FsKind::Fat32);
    assert_eq!(file(fat.as_ref(), "readme.txt"), text("readme", 3000));
    assert_eq!(file(fat.as_ref(), "docs/photo.bin"), noise(7, 300_000));
    let exfat = datareco_fs::open(Arc::clone(&copied), 41 * MIB * 512).unwrap();
    assert_eq!(exfat.info().kind, FsKind::Exfat);
    assert_eq!(file(exfat.as_ref(), "notes.txt"), text("notes", 20_000));
    let at = |lba: u64| usize::try_from(lba * 512).unwrap();
    let hidden = u32::from_le_bytes(
        target.data()[at(MIB) + 28..at(MIB) + 32]
            .try_into()
            .unwrap(),
    );
    assert_eq!(u64::from(hidden), MIB, "FAT32 knows where it starts");
    let offset = u64::from_le_bytes(
        target.data()[at(41 * MIB) + 64..at(41 * MIB) + 72]
            .try_into()
            .unwrap(),
    );
    assert_eq!(offset, 41 * MIB, "exFAT knows where it starts now");
    assert_eq!(
        target.data()[at(57 * MIB)..at(65 * MIB)],
        data[at(200 * MIB)..at(208 * MIB)],
        "the unreadable one, whole"
    );
    // Free clusters of the FAT32 copy were skipped: they still hold the
    // target's old bytes.
    assert!(target.data()[at(MIB)..at(41 * MIB)].contains(&0xEE));
}

#[test]
fn copies_one_partition_into_free_space_with_a_backup() {
    let data = source_disk();
    let mut target_data = vec![0u8; usize::try_from(64 * MIB * 512).unwrap()];
    with_table(
        &mut target_data,
        &[
            Operation::Initialize { style: Style::Mbr },
            create(MIB, 8 * MIB, "ntfs"),
        ],
    );
    let source = disk_of(&data);
    let target_disk = disk_of(&target_data);
    let copy = plan_partition_copy(&source, 128 * MIB, &target_disk, 20 * MIB).unwrap();
    let backups = std::env::temp_dir().join(format!("datareco-clone-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&backups);
    let held = allocation(Arc::new(disk_of(&data)), 128 * MIB * 512);
    let job = Job::partition(&copy, &target_disk, "image:target", &backups, held.as_ref()).unwrap();
    assert!(
        job.description.starts_with("Copy partition 2 (exFAT"),
        "{}",
        job.description
    );
    let mut target = MemoryTarget::with_data(target_data, 512);
    let progress = run(job, &mut sources_for(&data), &mut target);

    let backup = progress.backup.expect("the table change is backed up");
    assert!(backup.exists());
    let listed = list_backups(&backups, "image:target");
    assert!(listed[0].undoable, "a copy's table entry can be undone");
    let copied = Arc::new(disk_of(target.data()));
    let table = read_table(&copied).unwrap();
    let added = table
        .partitions
        .iter()
        .find(|p| p.start_lba == 20 * MIB)
        .unwrap();
    assert_eq!(added.type_code, "0x07", "exFAT's MBR type");
    let fs = datareco_fs::open(copied, 20 * MIB * 512).unwrap();
    assert_eq!(file(fs.as_ref(), "notes.txt"), text("notes", 20_000));
    std::fs::remove_dir_all(&backups).unwrap();
}

#[test]
fn reads_from_a_snapshot_where_there_is_one() {
    // The disk says 0x22 all through the partition; the snapshot of its
    // volume, 4 MiB long, says 0x11.
    let mut data = vec![0u8; usize::try_from(64 * MIB * 512).unwrap()];
    with_table(
        &mut data,
        &[
            Operation::Initialize { style: Style::Gpt },
            create(MIB, 8 * MIB, "linux"),
        ],
    );
    put(
        &mut data,
        MIB,
        &vec![0x22; usize::try_from(8 * MIB * 512).unwrap()],
    );
    let copy = plan_disk_copy(
        &disk_of(&data),
        "Target",
        64 * MIB,
        512,
        &DiskCopyOptions::default(),
    )
    .unwrap();
    let job = Job::disk(&copy, &[None], 512, "Copy".into()).unwrap();
    let mut sources = sources_for(&data);
    sources.snapshots.insert(
        0,
        Box::new(MemorySource::new(
            vec![0x11; usize::try_from(4 * MIB * 512).unwrap()],
            512,
        )),
    );
    let mut target = MemoryTarget::with_data(vec![0; data.len()], 512);
    run(job, &mut sources, &mut target);
    let at = |lba: u64| usize::try_from(lba * 512).unwrap();
    assert!(
        target.data()[at(MIB)..at(5 * MIB)]
            .iter()
            .all(|&b| b == 0x11)
    );
    assert!(
        target.data()[at(5 * MIB)..at(9 * MIB)]
            .iter()
            .all(|&b| b == 0x22)
    );
}

#[test]
fn mbr_boot_code_after_sector_0_comes_along() {
    let mut data = vec![0u8; usize::try_from(64 * MIB * 512).unwrap()];
    with_table(
        &mut data,
        &[
            Operation::Initialize { style: Style::Mbr },
            create(MIB, 8 * MIB, "linux"),
        ],
    );
    let loader = noise(3, usize::try_from((MIB - 1) * 512).unwrap());
    put(&mut data, 1, &loader);
    let copy = plan_disk_copy(
        &disk_of(&data),
        "Target",
        32 * MIB,
        512,
        &DiskCopyOptions {
            keep_ids: true,
            ..DiskCopyOptions::default()
        },
    )
    .unwrap();
    let job = Job::disk(&copy, &[None], 512, "Copy".into()).unwrap();
    let mut target =
        MemoryTarget::with_data(vec![0; usize::try_from(32 * MIB * 512).unwrap()], 512);
    run(job, &mut sources_for(&data), &mut target);
    assert_eq!(
        target.data()[512..usize::try_from(MIB * 512).unwrap()],
        loader[..]
    );
    assert_eq!(target.data()[..440], data[..440], "the MBR's own boot code");
    assert_eq!(
        target.data()[440..444],
        data[440..444],
        "kept IDs: the same disk signature"
    );
}

/// A target that fails its writes after a while, like a disk unplugged.
struct Failing {
    inner: MemoryTarget,
    left: Arc<Mutex<usize>>,
}

impl SectorTarget for Failing {
    fn sector_size(&self) -> u32 {
        512
    }
    fn capacity(&self) -> Option<u64> {
        self.inner.capacity()
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut left = self.left.lock().unwrap();
        if *left == 0 {
            return Err(io::Error::other("the disk went away"));
        }
        *left -= 1;
        self.inner.write_at(offset, data)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_target_that_fails_stops_the_copy_before_the_table() {
    let data = source_disk();
    let copy = plan_disk_copy(
        &disk_of(&data),
        "Target",
        96 * MIB,
        512,
        &DiskCopyOptions::default(),
    )
    .unwrap();
    let job = Job::disk(&copy, &[None, None, None], 512, "Copy".into()).unwrap();
    // The target had partitions of its own.
    let mut old = vec![0u8; usize::try_from(96 * MIB * 512).unwrap()];
    with_table(
        &mut old,
        &[
            Operation::Initialize { style: Style::Gpt },
            create(MIB, 80 * MIB, "ntfs"),
        ],
    );
    let mut target = Failing {
        inner: MemoryTarget::with_data(old, 512),
        left: Arc::new(Mutex::new(5)),
    };
    let mut cloner = Cloner::new(job);
    let error = loop {
        match cloner.step(&mut sources_for(&data), &mut target, Duration::from_secs(5)) {
            Ok(progress) => assert!(!progress.finished),
            Err(e) => break e,
        }
    };
    assert!(error.to_string().contains("the disk went away"), "{error}");
    assert!(
        read_table(&disk_of(target.inner.data()))
            .unwrap()
            .partitions
            .is_empty(),
        "no table points at half a copy"
    );
}
