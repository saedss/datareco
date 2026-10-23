//! The ext2/3/4 reader against volumes built in memory.

use crate::testutil::{EXT_ROOT, ExtBuilder, ExtOptions, STAMP, disk, noise, text};
use crate::{
    Entry, EntryId, Extent, FileSystem, FsKind, Rebuild, attributes, open, open_rebuilt,
    read_to_end, resolve,
};

fn read_all(fs: &dyn FileSystem, entry: &Entry) -> Vec<u8> {
    read_to_end(fs, &entry.id, None, u64::MAX).unwrap()
}

fn find<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
    entries.iter().find(|e| e.name == name).unwrap_or_else(|| {
        let names: Vec<_> = entries.iter().map(|e| (&e.name, e.deleted)).collect();
        panic!("no {name} in {names:?}")
    })
}

fn names(entries: &[Entry]) -> Vec<(String, bool)> {
    let mut names: Vec<_> = entries
        .iter()
        .map(|e| (e.name.clone(), e.deleted))
        .collect();
    names.sort();
    names
}

/// A hole of `len` zeros between two pieces of data.
fn with_hole(first: &[u8], len: usize, last: &[u8]) -> Vec<u8> {
    let mut data = first.to_vec();
    data.extend(std::iter::repeat_n(0, len));
    data.extend_from_slice(last);
    data
}

#[test]
fn ext4_files_folders_links_and_holes() {
    let mut b = ExtBuilder::new(ExtOptions::ext4());
    b.label("dr_ext4");
    let root = EXT_ROOT;
    b.file(root, "readme.txt", &text("readme", 300));
    let docs = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 12_000));
    let (x, y) = b.alloc_interleaved(3);
    b.file_in(docs, "part-a.bin", &noise(1, 3 * 4096), &x);
    b.file_in(docs, "part-b.bin", &noise(2, 2 * 4096 + 100), &y);
    // Six pieces: more than the inode holds, so the tree gets a leaf block.
    let scattered: Vec<u32> = (0..6)
        .flat_map(|_| {
            let block = b.alloc(1);
            b.alloc(1);
            block
        })
        .collect();
    b.file_in(root, "scattered.bin", &noise(3, 6 * 4096), &scattered);
    let pieces = b.alloc(2);
    let sparse = with_hole(&noise(4, 4096), 4096, &noise(5, 1000));
    b.file_in(root, "sparse.bin", &sparse, &[pieces[0], 0, pieces[1]]);
    b.preallocated(root, "prealloc.bin", 5000);
    b.file(root, "empty.txt", &[]);
    b.file(root, "Résumé café.txt", &text("resume", 1000));
    b.file(root, "日本語.txt", &text("japanese", 600));
    let long = format!("{}.txt", "a".repeat(250));
    b.file(root, &long, &text("long", 100));
    b.symlink(root, "link-to-readme", "readme.txt");

    let fs = open(disk(b.build()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::Ext4);
    assert_eq!(fs.info().name, "ext4");
    assert_eq!(fs.info().label.as_deref(), Some("dr_ext4"));
    assert_eq!(fs.info().cluster_size, 4096);

    let top = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&top),
        [
            "Résumé café.txt",
            &long,
            "docs",
            "empty.txt",
            "link-to-readme",
            "lost+found",
            "prealloc.bin",
            "readme.txt",
            "scattered.bin",
            "sparse.bin",
            "日本語.txt",
        ]
        .iter()
        .map(|n| ((*n).to_owned(), false))
        .collect::<Vec<_>>()
    );
    let readme = find(&top, "readme.txt");
    assert_eq!(read_all(fs.as_ref(), readme), text("readme", 300));
    assert!(readme.modified.is_some_and(|t| t > STAMP));
    assert_eq!(readme.created, readme.modified.map(|t| t - 86_400));
    assert!(find(&top, "docs").is_dir);
    assert_eq!(find(&top, "docs").attributes, attributes::DIRECTORY);

    let docs = fs.list(&find(&top, "docs").id).unwrap();
    assert_eq!(
        read_all(fs.as_ref(), find(&docs, "notes.txt")),
        text("notes", 12_000)
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&docs, "part-a.bin")),
        noise(1, 3 * 4096)
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&docs, "part-b.bin")),
        noise(2, 2 * 4096 + 100)
    );
    let part_a = fs.extents(&find(&docs, "part-a.bin").id, None).unwrap();
    assert_eq!(part_a.len(), 3, "{part_a:?}");

    let scattered_entry = find(&top, "scattered.bin");
    assert_eq!(read_all(fs.as_ref(), scattered_entry), noise(3, 6 * 4096));
    assert_eq!(fs.extents(&scattered_entry.id, None).unwrap().len(), 6);

    let sparse_entry = find(&top, "sparse.bin");
    assert_eq!(read_all(fs.as_ref(), sparse_entry), sparse);
    let extents = fs.extents(&sparse_entry.id, None).unwrap();
    assert_eq!(
        extents
            .iter()
            .map(|e| (e.length, e.sparse))
            .collect::<Vec<_>>(),
        [(4096, false), (4096, true), (1000, false)]
    );
    assert_eq!(extents[0].offset, u64::from(pieces[0]) * 4096);

    // Allocated but never written: zeros, whatever the blocks hold.
    let prealloc = find(&top, "prealloc.bin");
    assert_eq!(read_all(fs.as_ref(), prealloc), vec![0; 5000]);
    assert!(
        fs.extents(&prealloc.id, None)
            .unwrap()
            .iter()
            .all(|e| e.sparse)
    );

    assert!(read_all(fs.as_ref(), find(&top, "empty.txt")).is_empty());
    assert_eq!(
        read_all(fs.as_ref(), find(&top, "Résumé café.txt")),
        text("resume", 1000)
    );
    assert_eq!(read_all(fs.as_ref(), find(&top, &long)), text("long", 100));

    let link = find(&top, "link-to-readme");
    assert!(!link.is_dir);
    assert_eq!(link.note.as_deref(), Some("A link to readme.txt"));
    assert_eq!(read_all(fs.as_ref(), link), b"readme.txt");

    let found = resolve(fs.as_ref(), "docs/notes.txt", false).unwrap();
    assert_eq!(found.size, 12_000);
    assert!(fs.read(&found.id, Some("stream"), 0, &mut [0; 4]).is_err());
}

#[test]
fn ext4_deleted_files_come_back_from_the_journal() {
    let mut b = ExtBuilder::new(ExtOptions::ext4());
    let root = EXT_ROOT;
    b.file(root, "keep.txt", &text("keep", 2000));
    let gone = b.file(root, "gone.txt", &text("gone", 5000));
    let folder = b.mkdir(root, "deleted-folder");
    let inner = b.file(folder, "inner.txt", &text("inner", 3000));
    let sub = b.mkdir(folder, "sub");
    let deep = b.file(sub, "deep.txt", &text("deep", 9000));
    // The driver commits these to the journal...
    b.commit();
    // ...but not this one before it's deleted.
    let late = b.file(root, "late.txt", &text("late", 100));
    for n in [gone, deep, sub, inner, folder, late] {
        b.delete(n);
    }
    // The deletions are in the journal too, as its newest copies.
    b.commit();

    let fs = open(disk(b.build()), 0).unwrap();
    let top = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&top),
        [
            ("deleted-folder".to_owned(), true),
            ("gone.txt".to_owned(), true),
            ("keep.txt".to_owned(), false),
            ("late.txt".to_owned(), true),
            ("lost+found".to_owned(), false),
        ]
    );
    let gone_entry = find(&top, "gone.txt");
    assert_eq!(gone_entry.size, 5000);
    assert!(gone_entry.note.as_deref().unwrap().contains("journal"));
    assert_eq!(read_all(fs.as_ref(), gone_entry), text("gone", 5000));

    let late_entry = find(&top, "late.txt");
    assert_eq!(late_entry.size, 0);
    assert!(
        late_entry
            .note
            .as_deref()
            .unwrap()
            .contains("no older copy")
    );
    assert!(read_all(fs.as_ref(), late_entry).is_empty());

    let folder_entry = find(&top, "deleted-folder");
    assert!(folder_entry.is_dir && folder_entry.deleted);
    let inside = fs.list(&folder_entry.id).unwrap();
    assert_eq!(
        names(&inside),
        [("inner.txt".to_owned(), true), ("sub".to_owned(), true)]
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&inside, "inner.txt")),
        text("inner", 3000)
    );
    let deep_entry = resolve(fs.as_ref(), "deleted-folder/sub/deep.txt", true).unwrap();
    assert!(deep_entry.deleted);
    assert_eq!(read_all(fs.as_ref(), &deep_entry), text("deep", 9000));
    assert_eq!(
        read_all(fs.as_ref(), find(&top, "keep.txt")),
        text("keep", 2000)
    );
}

#[test]
fn ext4_entry_deleted_first_in_its_block_has_no_inode_number() {
    let mut b = ExtBuilder::new(ExtOptions::ext4());
    let dir = b.mkdir_with(EXT_ROOT, "two-blocks", 2, false);
    b.file(dir, "a.txt", &text("a", 10));
    let first = b.file_in_new_block(dir, "starts-a-block.txt", &text("first", 10));
    b.file(dir, "after.txt", &text("after", 10));
    b.commit();
    b.delete(first);

    let fs = open(disk(b.build()), 0).unwrap();
    let entries = fs
        .list(&resolve(fs.as_ref(), "two-blocks", false).unwrap().id)
        .unwrap();
    assert_eq!(
        names(&entries),
        [
            ("a.txt".to_owned(), false),
            ("after.txt".to_owned(), false),
            ("starts-a-block.txt".to_owned(), true),
        ]
    );
    let lost = find(&entries, "starts-a-block.txt");
    assert_eq!(
        lost.id,
        EntryId::Inode {
            number: 0,
            deleted: true
        }
    );
    assert!(
        lost.note
            .as_deref()
            .unwrap()
            .contains("inode number was cleared")
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&entries, "after.txt")),
        text("after", 10)
    );
}

#[test]
fn ext2_block_maps_second_group_and_deleted_files() {
    let mut b = ExtBuilder::new(ExtOptions::ext2());
    b.label("dr_ext2");
    let root = EXT_ROOT;
    // 300 KiB in 1 KiB blocks: direct, single and double indirect blocks.
    let big = noise(7, 300 * 1024);
    b.file(root, "big.bin", &big);
    let pieces = b.alloc(2);
    let sparse = with_hole(&noise(8, 1024), 1024, &noise(9, 500));
    b.file_in(root, "sparse.bin", &sparse, &[pieces[0], 0, pieces[1]]);
    let folder = b.mkdir_with(root, "many", 2, false);
    // Enough files that their inodes spill into the second group's table.
    for i in 0..25 {
        b.file(
            folder,
            &format!("file-{i:02}.txt"),
            &text(&format!("file {i}"), 700),
        );
    }
    let gone = b.file(root, "gone.bin", &noise(10, 20 * 1024));
    b.delete(gone);

    let fs = open(disk(b.build()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::Ext2);
    assert_eq!(fs.info().label.as_deref(), Some("dr_ext2"));
    let top = fs.list(&fs.root()).unwrap();
    assert_eq!(read_all(fs.as_ref(), find(&top, "big.bin")), big);
    assert_eq!(read_all(fs.as_ref(), find(&top, "sparse.bin")), sparse);
    let many = fs.list(&find(&top, "many").id).unwrap();
    assert_eq!(many.len(), 25);
    for i in 0..25 {
        let entry = find(&many, &format!("file-{i:02}.txt"));
        assert_eq!(
            read_all(fs.as_ref(), entry),
            text(&format!("file {i}"), 700)
        );
    }
    assert!(
        many.iter()
            .any(|e| matches!(e.id, EntryId::Inode { number, .. } if number > 32))
    );
    // ext2 keeps a deleted file's size and blocks.
    let gone_entry = find(&top, "gone.bin");
    assert!(gone_entry.deleted);
    assert_eq!(gone_entry.size, 20 * 1024);
    assert_eq!(read_all(fs.as_ref(), gone_entry), noise(10, 20 * 1024));
}

#[test]
fn ext3_deleted_files_come_back_from_its_journal() {
    let mut b = ExtBuilder::new(ExtOptions::ext3());
    let gone = b.file(EXT_ROOT, "gone.txt", &text("gone", 30 * 1024));
    b.commit();
    b.delete(gone);
    let fs = open(disk(b.build()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::Ext3);
    let top = fs.list(&fs.root()).unwrap();
    let entry = find(&top, "gone.txt");
    assert!(entry.deleted);
    assert_eq!(entry.size, 30 * 1024);
    assert_eq!(read_all(fs.as_ref(), entry), text("gone", 30 * 1024));
}

#[test]
fn hash_tree_folders_list_only_their_entries() {
    let mut b = ExtBuilder::new(ExtOptions::ext4());
    let dir = b.mkdir_with(EXT_ROOT, "indexed", 3, true);
    for i in 0..5 {
        b.file(
            dir,
            &format!("entry-{i}.txt"),
            &text(&format!("entry {i}"), 50),
        );
    }
    let fs = open(disk(b.build()), 0).unwrap();
    let entries = fs
        .list(&resolve(fs.as_ref(), "indexed", false).unwrap().id)
        .unwrap();
    assert_eq!(
        names(&entries),
        (0..5)
            .map(|i| (format!("entry-{i}.txt"), false))
            .collect::<Vec<_>>()
    );
}

#[test]
fn volumes_open_with_a_backup_superblock() {
    for options in [ExtOptions::ext2(), ExtOptions::ext4()] {
        let mut b = ExtBuilder::new(options);
        b.file(EXT_ROOT, "hello.txt", &text("hello", 100));
        let backup = b.backup_superblock() as u64;
        let mut image = b.build();
        let kind = open(disk(image.clone()), 0).unwrap().info().kind;
        image[1024..2048].fill(0);
        let size = image.len() as u64;
        let disk = disk(image);
        assert!(open(disk.clone(), 0).is_err());
        let rebuild = Rebuild {
            boot_sector: Some(backup),
            ..Rebuild::default()
        };
        let fs = open_rebuilt(disk, kind, 0, size, &rebuild).unwrap();
        let hello = resolve(fs.as_ref(), "hello.txt", false).unwrap();
        assert_eq!(read_all(fs.as_ref(), &hello), text("hello", 100));
    }
}

#[test]
fn volumes_inside_a_disk_report_where_their_data_is() {
    let mut b = ExtBuilder::new(ExtOptions::ext4());
    let blocks = b.alloc(2);
    b.file_in(EXT_ROOT, "data.bin", &noise(11, 8192), &blocks);
    let mut image = vec![0u8; 1 << 20];
    image.extend(b.build());
    let fs = open(disk(image), 1 << 20).unwrap();
    let entry = resolve(fs.as_ref(), "data.bin", false).unwrap();
    assert_eq!(
        fs.extents(&entry.id, None).unwrap(),
        [Extent {
            offset: (1 << 20) + u64::from(blocks[0]) * 4096,
            length: 8192,
            sparse: false
        }]
    );
    assert_eq!(read_all(fs.as_ref(), &entry), noise(11, 8192));
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
    for options in [ExtOptions::ext4(), ExtOptions::ext2()] {
        let mut b = ExtBuilder::new(options.clone());
        let docs = b.mkdir(EXT_ROOT, "docs");
        b.file(docs, "notes.txt", &text("notes", 9000));
        let scattered: Vec<u32> = (0..6)
            .flat_map(|_| {
                let block = b.alloc(1);
                b.alloc(1);
                block
            })
            .collect();
        b.file_in(
            EXT_ROOT,
            "scattered.bin",
            &noise(3, 6 * options.block_size),
            &scattered,
        );
        b.file(EXT_ROOT, "big.bin", &noise(4, 100 * 1024));
        let gone = b.file(EXT_ROOT, "gone.txt", &text("gone", 3000));
        if options.journal_blocks > 0 {
            b.commit();
        }
        b.delete(gone);
        let table = b.inode_table(0) * options.block_size;
        let image = b.build();
        // Scribble over the superblock, descriptors, inode table and the
        // first data blocks.
        let regions = [1024..2048, 2048..table + 8192, table..table + 16384];
        let junk = noise(99, 4096);
        for (seed, region) in regions.iter().cycle().take(60).enumerate() {
            let mut damaged = image.clone();
            let span = region.end - region.start;
            for k in 0..8 {
                let at = region.start + (seed * 7919 + k * 104_729) % span;
                damaged[at] = junk[(seed * 31 + k) % junk.len()];
            }
            if let Ok(fs) = open(disk(damaged), 0) {
                walk(fs.as_ref(), &fs.root(), 0);
            }
        }
    }
}
