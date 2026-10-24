//! Which parts of a volume are in use, for copying only those: every live
//! file's data must be inside, free space mostly outside.

use crate::testutil::{ExfatBuilder, FatBuilder, NTFS_ROOT, NtfsBuilder, disk, noise, text};
use crate::{Allocation, FileSystem, FsKind, open, resolve};

/// Every extent of each named file (from the root) lies in a used range.
fn check_files_inside(fs: &dyn FileSystem, allocation: &Allocation, files: &[&str]) {
    for name in files {
        let entry = resolve(fs, name, false).unwrap();
        for extent in fs.extents(&entry.id, None).unwrap() {
            let (start, end) = (extent.offset, extent.offset + extent.length);
            assert!(
                allocation
                    .used
                    .iter()
                    .any(|&(from, to)| from <= start && end <= to),
                "{name}'s data at {start}..{end} isn't in {:?}",
                allocation.used
            );
        }
    }
}

fn check_shape(allocation: &Allocation, size: u64) {
    assert!(allocation.covered <= size);
    assert!(
        allocation.used.windows(2).all(|pair| pair[0].1 < pair[1].0),
        "in order and apart: {:?}",
        allocation.used
    );
    assert!(allocation.used.iter().all(|(start, end)| start < end));
    assert!(
        allocation.used_bytes() < allocation.covered / 2,
        "mostly free"
    );
}

#[test]
fn fat32_says_which_clusters_hold_files() {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    let root = b.root();
    b.file(root, "readme.txt", &text("readme", 300));
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 12_000));
    let (a, c) = b.alloc_interleaved(4);
    b.file_in(docs, "part-a.bin", &noise(1, 4 * 512), a);
    b.file_in(docs, "part-b.bin", &noise(2, 4 * 512), c);
    let old = b.file(root, "old.bin", &noise(3, 5000));
    let old_cluster = old.chain[0];
    b.delete(&old);
    let data_start = b.data_start() as u64;
    let cluster_size = b.cluster_size() as u64;
    let image = b.build();
    let size = image.len() as u64;

    let fs = open(disk(image), 0).unwrap();
    let allocation = fs.allocation().expect("FAT keeps track");
    check_shape(&allocation, size);
    assert_eq!(allocation.used[0].0, 0, "the boot sector and FATs");
    assert!(allocation.used[0].1 >= data_start);
    check_files_inside(
        fs.as_ref(),
        &allocation,
        &[
            "readme.txt",
            "docs/notes.txt",
            "docs/part-a.bin",
            "docs/part-b.bin",
        ],
    );
    let old_at = data_start + u64::from(old_cluster - 2) * cluster_size;
    assert!(
        !allocation
            .used
            .iter()
            .any(|&(from, to)| from <= old_at && old_at < to),
        "a deleted file's clusters are free"
    );
}

#[test]
fn fat16_and_fat12_too() {
    for (kind, total) in [(FsKind::Fat16, 20_000), (FsKind::Fat12, 4_000)] {
        let mut b = FatBuilder::new(kind, total);
        let root = b.root();
        b.file(root, "one.txt", &text("one", 3000));
        b.file(root, "two.txt", &text("two", 700));
        let image = b.build();
        let size = image.len() as u64;
        let fs = open(disk(image), 0).unwrap();
        assert_eq!(fs.info().kind, kind);
        let allocation = fs.allocation().unwrap();
        check_shape(&allocation, size);
        check_files_inside(fs.as_ref(), &allocation, &["one.txt", "two.txt"]);
    }
}

#[test]
fn exfat_says_which_clusters_hold_files() {
    let mut b = ExfatBuilder::new(8192);
    let root = b.root();
    b.file(root, "readme.txt", &text("readme", 300));
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 12_000));
    let (x, y) = b.alloc_interleaved(3);
    b.file_in(docs, "x.bin", &noise(4, 3 * 1024), x, false);
    b.file_in(docs, "y.bin", &noise(5, 3 * 1024), y, false);
    let image = b.build();
    let size = image.len() as u64;

    let fs = open(disk(image), 0).unwrap();
    let allocation = fs.allocation().expect("exFAT keeps a bitmap");
    check_shape(&allocation, size);
    assert_eq!(allocation.used[0].0, 0, "the boot regions and the FAT");
    check_files_inside(
        fs.as_ref(),
        &allocation,
        &["readme.txt", "docs/notes.txt", "docs/x.bin", "docs/y.bin"],
    );
}

#[test]
fn ntfs_says_which_clusters_hold_files() {
    let mut b = NtfsBuilder::new(2048);
    b.file(NTFS_ROOT, "readme.txt", &text("readme", 300));
    b.file(NTFS_ROOT, "big.txt", &text("big", 20_000));
    let docs = b.mkdir(NTFS_ROOT, "docs");
    b.file(docs, "notes.txt", &text("notes", 9000));
    let image = b.build();
    let size = image.len() as u64;

    let fs = open(disk(image), 0).unwrap();
    let allocation = fs.allocation().expect("NTFS keeps $Bitmap");
    check_shape(&allocation, size);
    assert_eq!(allocation.used[0].0, 0, "the boot sector");
    check_files_inside(
        fs.as_ref(),
        &allocation,
        &["readme.txt", "big.txt", "docs/notes.txt"],
    );
    assert!(
        allocation.covered < size,
        "the backup boot sector is past the clusters"
    );
}
