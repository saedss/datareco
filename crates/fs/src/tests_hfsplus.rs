//! The HFS+ reader against volumes built in memory.

use crate::testutil::{HFS_ROOT, HfsBuilder, HfsOptions, STAMP, disk, noise, text};
use crate::{
    Entry, EntryId, FileSystem, FsKind, LOST_FILES, Rebuild, attributes, open, open_rebuilt,
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

#[test]
fn files_folders_forks_and_links() {
    let mut b = HfsBuilder::new(HfsOptions::default());
    b.label("dr_hfsplus");
    let root = HFS_ROOT;
    b.file(root, "readme.txt", &text("readme", 300));
    let docs = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 12_000));
    let (x, y) = b.alloc_interleaved(3);
    b.file_in(docs, "part-a.bin", &noise(1, 3 * 4096), &x);
    b.file_in(docs, "part-b.bin", &noise(2, 2 * 4096 + 100), &y);
    // Ten pieces: two of them in the extents overflow file.
    let pieces: Vec<u32> = (0..10)
        .flat_map(|_| {
            let block = b.alloc(1);
            b.alloc(1);
            block
        })
        .collect();
    b.file_in(root, "scattered.bin", &noise(3, 10 * 4096), &pieces);
    let forked = b.file(root, "forked.txt", &text("data fork", 900));
    b.resource(forked, &noise(4, 5000));
    b.file(root, "empty.txt", &[]);
    // HFS+ keeps names decomposed: e + combining acute accent.
    b.file(
        root,
        "Re\u{301}sume\u{301} cafe\u{301}.txt",
        &text("resume", 1000),
    );
    b.file(root, "日本語.txt", &text("japanese", 600));
    let long = format!("{}.txt", "a".repeat(200));
    b.file(root, &long, &text("long", 100));
    b.symlink(root, "link-to-readme", "readme.txt");
    let original = b.file(root, "original.txt", &text("shared", 7000));
    b.hard_link(original, docs, "second-name.txt");
    let secret = b.file(root, "secret.txt", b"shh");
    b.invisible(secret);
    // Enough records for several leaf nodes and an index node.
    let many = b.mkdir(root, "many");
    for i in 0..40 {
        b.file(
            many,
            &format!("file-{i:02}.txt"),
            &text(&format!("file {i}"), 300),
        );
    }

    let fs = open(disk(b.build()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::HfsPlus);
    assert_eq!(fs.info().name, "HFS+");
    assert_eq!(fs.info().label.as_deref(), Some("dr_hfsplus"));
    assert_eq!(fs.info().cluster_size, 4096);

    let top = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&top),
        [
            "Résumé café.txt",
            &long,
            "docs",
            "empty.txt",
            "forked.txt",
            "link-to-readme",
            "many",
            "original.txt",
            "readme.txt",
            "scattered.bin",
            "secret.txt",
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
    assert_eq!(find(&top, "docs").attributes, attributes::DIRECTORY);
    assert_eq!(find(&top, "secret.txt").attributes, attributes::HIDDEN);

    let docs_entries = fs.list(&find(&top, "docs").id).unwrap();
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
    assert_eq!(
        fs.extents(&find(&docs_entries, "part-a.bin").id, None)
            .unwrap()
            .len(),
        3
    );

    let scattered = find(&top, "scattered.bin");
    assert_eq!(read_all(fs.as_ref(), scattered), noise(3, 10 * 4096));
    assert_eq!(fs.extents(&scattered.id, None).unwrap().len(), 10);

    let forked_entry = find(&top, "forked.txt");
    assert_eq!(read_all(fs.as_ref(), forked_entry), text("data fork", 900));
    assert_eq!(forked_entry.streams.len(), 1);
    assert_eq!(forked_entry.streams[0].size, 5000);
    let resource = read_to_end(fs.as_ref(), &forked_entry.id, Some("rsrc"), u64::MAX).unwrap();
    assert_eq!(resource, noise(4, 5000));

    assert!(read_all(fs.as_ref(), find(&top, "empty.txt")).is_empty());
    assert_eq!(
        read_all(fs.as_ref(), find(&top, "Résumé café.txt")),
        text("resume", 1000)
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&top, "日本語.txt")),
        text("japanese", 600)
    );
    assert_eq!(read_all(fs.as_ref(), find(&top, &long)), text("long", 100));

    let link = find(&top, "link-to-readme");
    assert_eq!(link.note.as_deref(), Some("A link to readme.txt"));

    // Both names of a hard link read the same data.
    let first = find(&top, "original.txt");
    let second = find(&docs_entries, "second-name.txt");
    assert_eq!(first.size, 7000);
    assert_eq!(read_all(fs.as_ref(), first), text("shared", 7000));
    assert_eq!(read_all(fs.as_ref(), second), text("shared", 7000));

    let many_entries = fs.list(&find(&top, "many").id).unwrap();
    assert_eq!(many_entries.len(), 40);
    for i in 0..40 {
        let entry = find(&many_entries, &format!("file-{i:02}.txt"));
        assert_eq!(
            read_all(fs.as_ref(), entry),
            text(&format!("file {i}"), 300)
        );
    }
    let deep = resolve(fs.as_ref(), "docs/notes.txt", false).unwrap();
    assert_eq!(deep.size, 12_000);
    assert!(fs.read(&deep.id, Some("other"), 0, &mut [0; 4]).is_err());
}

#[test]
fn deleted_records_left_in_the_catalog_come_back() {
    let mut b = HfsBuilder::new(HfsOptions::default());
    let root = HFS_ROOT;
    b.file(root, "keep.txt", &text("keep", 2000));
    let gone = b.file(root, "gone.txt", &text("gone", 5000));
    let folder = b.mkdir(root, "old-folder");
    let inner = b.file(folder, "inner.txt", &text("inner", 3000));
    // A folder whose own record was overwritten: its file is lost.
    let vanished = b.mkdir(root, "vanished");
    let stray = b.file(vanished, "stray.txt", &text("stray", 800));
    for id in [gone, inner, folder, stray] {
        b.delete(id);
    }
    b.forget(vanished);
    let fs = open(disk(b.build()), 0).unwrap();
    let top = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&top),
        [
            ("Lost files".to_owned(), false),
            ("gone.txt".to_owned(), true),
            ("keep.txt".to_owned(), false),
            ("old-folder".to_owned(), true),
        ]
    );
    let gone_entry = find(&top, "gone.txt");
    assert_eq!(gone_entry.size, 5000);
    assert!(gone_entry.note.as_deref().unwrap().contains("left over"));
    assert_eq!(read_all(fs.as_ref(), gone_entry), text("gone", 5000));
    let inside = fs.list(&find(&top, "old-folder").id).unwrap();
    assert_eq!(names(&inside), [("inner.txt".to_owned(), true)]);
    assert_eq!(read_all(fs.as_ref(), &inside[0]), text("inner", 3000));
    let lost = fs.list(&find(&top, LOST_FILES).id).unwrap();
    assert_eq!(names(&lost), [("stray.txt".to_owned(), true)]);
    assert_eq!(read_all(fs.as_ref(), &lost[0]), text("stray", 800));
}

#[test]
fn deleted_records_in_a_node_no_longer_in_use() {
    let mut b = HfsBuilder::new(HfsOptions::default());
    b.deleted_into_free_node();
    let folder = b.mkdir(HFS_ROOT, "Trip");
    let photo = b.file(folder, "day1.jpg", &noise(9, 20_000));
    b.file(HFS_ROOT, "keep.txt", b"keep");
    b.delete(photo);
    b.delete(folder);
    let fs = open(disk(b.build()), 0).unwrap();
    let photo = resolve(fs.as_ref(), "Trip/day1.jpg", true).unwrap();
    assert!(photo.deleted);
    assert_eq!(read_all(fs.as_ref(), &photo), noise(9, 20_000));
    assert!(matches!(photo.id, EntryId::Catalog { deleted: true, .. }));
}

#[test]
fn hfsx_volumes_and_the_backup_header() {
    let mut b = HfsBuilder::new(HfsOptions {
        hfsx: true,
        ..HfsOptions::default()
    });
    b.label("case sensitive");
    b.file(HFS_ROOT, "Hello.txt", &text("upper", 100));
    b.file(HFS_ROOT, "hello.txt", &text("lower", 100));
    let backup = b.backup_header() as u64;
    let mut image = b.build();
    let fs = open(disk(image.clone()), 0).unwrap();
    assert_eq!(fs.info().name, "HFSX");
    assert_eq!(fs.list(&fs.root()).unwrap().len(), 2);

    image[1024..1536].fill(0);
    let size = image.len() as u64;
    let disk = disk(image);
    assert!(open(disk.clone(), 0).is_err());
    let rebuild = Rebuild {
        boot_sector: Some(backup),
        ..Rebuild::default()
    };
    let fs = open_rebuilt(disk, FsKind::HfsPlus, 0, size, &rebuild).unwrap();
    let top = fs.list(&fs.root()).unwrap();
    assert_eq!(
        read_all(fs.as_ref(), find(&top, "hello.txt")),
        text("lower", 100)
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&top, "Hello.txt")),
        text("upper", 100)
    );
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
    let mut b = HfsBuilder::new(HfsOptions::default());
    let docs = b.mkdir(HFS_ROOT, "docs");
    for i in 0..30 {
        b.file(docs, &format!("file-{i}.txt"), &text("x", 1000 + i));
    }
    let gone = b.file(HFS_ROOT, "gone.txt", &text("gone", 3000));
    b.delete(gone);
    let image = b.build();
    let junk = noise(98, 4096);
    // The volume header, the extents tree and the catalog's nodes.
    let regions = [1024..1536, 4096..12288, 12288..12288 + 32 * 4096];
    for (seed, region) in regions.iter().cycle().take(90).enumerate() {
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
}
