//! The btrfs reader against volumes built in memory.

use crate::testutil::{BTRFS_ROOT_DIR, BTRFS_TOP, BtrfsBuilder, STAMP, disk, noise, text};
use crate::{
    Entry, EntryId, FileSystem, FsError, FsKind, Rebuild, attributes, open, open_rebuilt,
    read_to_end, resolve,
};
use datareco_partitions::crc32c;

fn read_all(fs: &dyn FileSystem, entry: &Entry) -> Vec<u8> {
    read_to_end(fs, &entry.id, None, u64::MAX).unwrap()
}

fn find<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
    entries.iter().find(|e| e.name == name).unwrap_or_else(|| {
        let names: Vec<_> = entries.iter().map(|e| &e.name).collect();
        panic!("no {name} in {names:?}")
    })
}

fn names(entries: &[Entry]) -> Vec<String> {
    let mut names: Vec<_> = entries.iter().map(|e| e.name.clone()).collect();
    names.sort();
    names
}

#[test]
fn checksums_are_crc32c() {
    assert_eq!(crc32c(b"123456789"), 0xE306_9283);
}

#[test]
fn files_folders_subvolumes_and_links() {
    let mut b = BtrfsBuilder::new(32);
    b.label("dr_btrfs");
    let top = BTRFS_TOP;
    let root = BTRFS_ROOT_DIR;
    b.file(top, root, "readme.txt", &text("readme", 300));
    let docs = b.mkdir(top, root, "docs");
    b.file(top, docs, "notes.txt", &text("notes", 12_000));
    let pieces = b.alloc(6);
    let x: Vec<u64> = pieces.iter().step_by(2).copied().collect();
    let y: Vec<u64> = pieces.iter().skip(1).step_by(2).copied().collect();
    b.file_in(top, docs, "part-a.bin", &noise(1, 3 * 4096), &x);
    b.file_in(top, docs, "part-b.bin", &noise(2, 2 * 4096 + 100), &y);
    let holes = b.alloc(2);
    let mut sparse = noise(3, 4096);
    sparse.extend(vec![0; 4096]);
    sparse.extend(noise(4, 1000));
    b.file_in(top, root, "sparse.bin", &sparse, &[holes[0], 0, holes[1]]);
    b.preallocated(top, root, "prealloc.bin", 5000);
    b.compressed(top, root, "packed.log", 9000);
    b.file(top, root, "Résumé café.txt", &text("resume", 1000));
    b.file(top, root, "日本語.txt", &text("japanese", 600));
    let long = format!("{}.txt", "a".repeat(200));
    b.file(top, root, &long, &text("long", 100));
    b.symlink(top, root, "link-to-readme", "readme.txt");
    let shared = b.file(top, root, "shared.txt", &text("shared", 7000));
    b.hard_link(top, shared, docs, "second-name.txt");
    let home = b.subvolume(top, root, "home");
    b.file(
        home,
        BTRFS_ROOT_DIR,
        "in-subvolume.txt",
        &text("subvolume", 5000),
    );
    // Enough items for several leaves under an internal node.
    let many = b.mkdir(top, root, "many");
    for i in 0..80 {
        b.file(
            top,
            many,
            &format!("file-{i:02}.txt"),
            &text(&format!("file {i}"), 200),
        );
    }

    let fs = open(disk(b.build()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::Btrfs);
    assert_eq!(fs.info().name, "btrfs");
    assert_eq!(fs.info().label.as_deref(), Some("dr_btrfs"));
    assert_eq!(fs.info().cluster_size, 4096);

    let top_entries = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&top_entries),
        [
            "Résumé café.txt",
            &long,
            "docs",
            "home",
            "link-to-readme",
            "many",
            "packed.log",
            "prealloc.bin",
            "readme.txt",
            "shared.txt",
            "sparse.bin",
            "日本語.txt",
        ]
    );
    let readme = find(&top_entries, "readme.txt");
    assert_eq!(read_all(fs.as_ref(), readme), text("readme", 300));
    assert!(readme.modified.is_some_and(|t| t > STAMP));
    assert_eq!(readme.created, readme.modified.map(|t| t - 86_400));
    // Small files live inside the tree, not in extents of their own.
    assert!(fs.extents(&readme.id, None).unwrap().is_empty());
    assert_eq!(find(&top_entries, "docs").attributes, attributes::DIRECTORY);

    let docs_entries = fs.list(&find(&top_entries, "docs").id).unwrap();
    assert_eq!(
        read_all(fs.as_ref(), find(&docs_entries, "notes.txt")),
        text("notes", 12_000)
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&docs_entries, "part-a.bin")),
        noise(1, 3 * 4096)
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&docs_entries, "part-b.bin")),
        noise(2, 2 * 4096 + 100)
    );
    let part_a = fs
        .extents(&find(&docs_entries, "part-a.bin").id, None)
        .unwrap();
    assert_eq!(part_a.len(), 3);
    // Logical address 0x2000_0000 is 6 MiB into the disk.
    assert_eq!(part_a[0].offset, 6 * 1024 * 1024 + (x[0] - 0x2000_0000));
    assert_eq!(
        read_all(fs.as_ref(), find(&docs_entries, "second-name.txt")),
        text("shared", 7000)
    );

    let sparse_entry = find(&top_entries, "sparse.bin");
    assert_eq!(read_all(fs.as_ref(), sparse_entry), sparse);
    let extents = fs.extents(&sparse_entry.id, None).unwrap();
    assert_eq!(
        extents
            .iter()
            .map(|e| (e.length, e.sparse))
            .collect::<Vec<_>>(),
        [(4096, false), (4096, true), (1000, false)]
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&top_entries, "prealloc.bin")),
        vec![0; 5000]
    );

    let packed = find(&top_entries, "packed.log");
    assert!(packed.note.as_deref().unwrap().contains("Compressed"));
    assert_eq!(packed.attributes, attributes::COMPRESSED);
    assert!(matches!(
        fs.read(&packed.id, None, 0, &mut [0; 16]),
        Err(FsError::Unsupported(_))
    ));
    // Where its compressed bytes are, for looking at them in the sectors.
    let packed_extents = fs.extents(&packed.id, None).unwrap();
    assert!(!packed_extents.is_empty() && packed_extents.iter().all(|e| !e.sparse));

    assert_eq!(
        read_all(fs.as_ref(), find(&top_entries, "Résumé café.txt")),
        text("resume", 1000)
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&top_entries, &long)),
        text("long", 100)
    );
    assert_eq!(
        find(&top_entries, "link-to-readme").note.as_deref(),
        Some("A link to readme.txt")
    );

    let home_entry = find(&top_entries, "home");
    assert!(home_entry.is_dir);
    assert_eq!(home_entry.note.as_deref(), Some("A subvolume."));
    assert!(home_entry.modified.is_some_and(|t| t > STAMP));
    assert_eq!(home_entry.created, home_entry.modified.map(|t| t - 86_400));
    assert!(matches!(
        home_entry.id,
        EntryId::Btrfs {
            tree: 256,
            inode: 256
        }
    ));
    let inside = resolve(fs.as_ref(), "home/in-subvolume.txt", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &inside), text("subvolume", 5000));

    let many_entries = fs.list(&find(&top_entries, "many").id).unwrap();
    assert_eq!(many_entries.len(), 80);
    for i in 0..80 {
        let entry = find(&many_entries, &format!("file-{i:02}.txt"));
        assert_eq!(
            read_all(fs.as_ref(), entry),
            text(&format!("file {i}"), 200)
        );
    }
}

#[test]
fn volumes_open_with_the_superblock_copy_at_64_mib() {
    let mut b = BtrfsBuilder::new(65);
    b.file(BTRFS_TOP, BTRFS_ROOT_DIR, "hello.txt", &text("hello", 5000));
    let mut image = b.build();
    image[0x1_0000..0x1_1000].fill(0);
    let size = image.len() as u64;
    let disk = disk(image);
    assert!(open(disk.clone(), 0).is_err());
    let rebuild = Rebuild {
        boot_sector: Some(0x400_0000),
        ..Rebuild::default()
    };
    let fs = open_rebuilt(disk, FsKind::Btrfs, 0, size, &rebuild).unwrap();
    let hello = resolve(fs.as_ref(), "hello.txt", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &hello), text("hello", 5000));
}

/// Walks every folder and reads every file, ignoring errors.
fn walk(fs: &dyn FileSystem, dir: &EntryId, depth: usize) {
    if depth > 6 {
        return;
    }
    let Ok(entries) = fs.list(dir) else {
        return;
    };
    for entry in entries {
        if entry.is_dir {
            walk(fs, &entry.id, depth + 1);
        } else {
            let _ = read_to_end(fs, &entry.id, None, 1 << 20);
            let _ = fs.extents(&entry.id, None);
        }
    }
}

#[test]
fn damaged_volumes_never_panic() {
    let mut b = BtrfsBuilder::new(16);
    let docs = b.mkdir(BTRFS_TOP, BTRFS_ROOT_DIR, "docs");
    for i in 0..60 {
        b.file(
            BTRFS_TOP,
            docs,
            &format!("file-{i}.txt"),
            &text("x", 1000 + i * 100),
        );
    }
    let home = b.subvolume(BTRFS_TOP, BTRFS_ROOT_DIR, "home");
    b.file(home, BTRFS_ROOT_DIR, "a.txt", &text("a", 5000));
    let mut image = b.build();
    let junk = noise(96, 4096);
    // The superblock's checksum would refuse most changes to it, so these
    // go to the chunk tree (1 MiB) and the other trees (2 MiB on).
    let regions = [1 << 20..(1 << 20) + 4096, 2 << 20..(2 << 20) + 64 * 4096];
    for (seed, region) in regions.iter().cycle().take(120).enumerate() {
        let mut damaged = image.clone();
        let span = region.end - region.start;
        for k in 0..10 {
            let at = region.start + (seed * 7919 + k * 104_729) % span;
            damaged[at] = junk[(seed * 31 + k) % junk.len()];
        }
        if let Ok(fs) = open(disk(damaged), 0) {
            walk(fs.as_ref(), &fs.root(), 0);
        }
    }
    // A broken superblock is refused, not trusted.
    image[0x1_0000 + 0x94] = 0x13;
    assert!(open(disk(image), 0).is_err());
}
