//! The APFS reader against containers built in memory.

use crate::testutil::{APFS_ROOT, ApfsBuilder, STAMP, disk, noise, text};
use crate::{Entry, EntryId, FileSystem, FsError, FsKind, attributes, open, read_to_end, resolve};

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
fn containers_show_their_volumes_and_files() {
    let mut b = ApfsBuilder::new(4096);
    let data = b.volume("Macintosh HD - Data", 0x40, false);
    b.volume("Macintosh HD", 0x1, false);
    b.volume("Secret", 0, true);
    b.file(data, APFS_ROOT, "readme.txt", &text("readme", 300));
    let docs = b.mkdir(data, APFS_ROOT, "docs");
    b.file(data, docs, "notes.txt", &text("notes", 12_000));
    let pieces = b.alloc(6);
    let (x, y): (Vec<u64>, Vec<u64>) = (
        pieces.iter().step_by(2).copied().collect(),
        pieces.iter().skip(1).step_by(2).copied().collect(),
    );
    b.file_in(data, docs, "part-a.bin", &noise(1, 3 * 4096), &x);
    b.file_in(data, docs, "part-b.bin", &noise(2, 2 * 4096 + 100), &y);
    let holes = b.alloc(2);
    let mut sparse = noise(3, 4096);
    sparse.extend(vec![0; 4096]);
    sparse.extend(noise(4, 1000));
    b.file_in(
        data,
        APFS_ROOT,
        "sparse.bin",
        &sparse,
        &[holes[0], 0, holes[1]],
    );
    // Names as macOS may store them: decomposed.
    b.file(
        data,
        APFS_ROOT,
        "Re\u{301}sume\u{301}.txt",
        &text("resume", 1000),
    );
    b.file(data, APFS_ROOT, "日本語.txt", &text("japanese", 600));
    b.symlink(data, APFS_ROOT, "link-to-readme", "readme.txt");
    let shared = b.file(data, APFS_ROOT, "shared.txt", &text("shared", 7000));
    b.hard_link(data, shared, docs, "second-name.txt");
    let secret = b.file(data, APFS_ROOT, "secret.txt", b"shh");
    b.hidden(data, secret);
    let small_fork = b.file(data, APFS_ROOT, "small-fork.txt", b"data");
    b.resource(data, small_fork, &noise(5, 300));
    let big_fork = b.file(data, APFS_ROOT, "big-fork.txt", b"data");
    b.resource(data, big_fork, &noise(6, 10_000));
    // Enough records for several leaf nodes under an index node.
    let many = b.mkdir(data, APFS_ROOT, "many");
    for i in 0..60 {
        b.file(
            data,
            many,
            &format!("file-{i:02}.txt"),
            &text(&format!("file {i}"), 200),
        );
    }

    let fs = open(disk(b.build()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::Apfs);
    assert_eq!(fs.info().name, "APFS");
    // A container has no name of its own; its volumes do.
    assert_eq!(fs.info().label, None);

    // All three volumes: the newest container superblock lists them (the
    // old copy in block 0 knew only one).
    let volumes = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&volumes),
        ["Macintosh HD", "Macintosh HD - Data", "Secret"]
    );
    assert_eq!(
        find(&volumes, "Macintosh HD - Data").note.as_deref(),
        Some("APFS volume (Data).")
    );
    let secret_volume = find(&volumes, "Secret");
    assert!(secret_volume.note.as_deref().unwrap().contains("Encrypted"));
    assert!(matches!(
        fs.list(&secret_volume.id),
        Err(FsError::Unsupported(_))
    ));
    assert!(
        fs.list(&find(&volumes, "Macintosh HD").id)
            .unwrap()
            .is_empty()
    );

    let top = fs.list(&find(&volumes, "Macintosh HD - Data").id).unwrap();
    assert_eq!(
        names(&top),
        [
            "Résumé.txt",
            "big-fork.txt",
            "docs",
            "link-to-readme",
            "many",
            "readme.txt",
            "secret.txt",
            "shared.txt",
            "small-fork.txt",
            "sparse.bin",
            "日本語.txt",
        ]
    );
    let readme = find(&top, "readme.txt");
    assert_eq!(read_all(fs.as_ref(), readme), text("readme", 300));
    assert!(readme.modified.is_some_and(|t| t > STAMP));
    assert_eq!(readme.created, readme.modified.map(|t| t - 86_400));
    assert_eq!(find(&top, "docs").attributes, attributes::DIRECTORY);
    assert_eq!(find(&top, "secret.txt").attributes, attributes::HIDDEN);
    assert_eq!(
        read_all(fs.as_ref(), find(&top, "Résumé.txt")),
        text("resume", 1000)
    );

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
    assert_eq!(
        read_all(fs.as_ref(), find(&docs_entries, "second-name.txt")),
        text("shared", 7000)
    );

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

    assert_eq!(
        find(&top, "link-to-readme").note.as_deref(),
        Some("A link to readme.txt")
    );
    for (name, fork) in [
        ("small-fork.txt", noise(5, 300)),
        ("big-fork.txt", noise(6, 10_000)),
    ] {
        let entry = find(&top, name);
        assert_eq!(entry.streams.len(), 1, "{name}");
        assert_eq!(entry.streams[0].size, fork.len() as u64);
        assert_eq!(
            read_to_end(fs.as_ref(), &entry.id, Some("rsrc"), u64::MAX).unwrap(),
            fork
        );
        assert_eq!(read_all(fs.as_ref(), entry), b"data");
    }

    let many_entries = fs.list(&find(&top, "many").id).unwrap();
    assert_eq!(many_entries.len(), 60);
    for i in 0..60 {
        let entry = find(&many_entries, &format!("file-{i:02}.txt"));
        assert_eq!(
            read_all(fs.as_ref(), entry),
            text(&format!("file {i}"), 200)
        );
    }
    let deep = resolve(fs.as_ref(), "Macintosh HD - Data/docs/notes.txt", false).unwrap();
    assert!(matches!(deep.id, EntryId::Apfs { volume: 0, .. }));
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
            let _ = read_to_end(fs, &entry.id, Some("rsrc"), 1 << 20);
            let _ = fs.extents(&entry.id, None);
        }
    }
}

#[test]
fn damaged_containers_never_panic() {
    let mut b = ApfsBuilder::new(1024);
    let data = b.volume("Data", 0x40, false);
    let docs = b.mkdir(data, APFS_ROOT, "docs");
    for i in 0..40 {
        b.file(data, docs, &format!("file-{i}.txt"), &text("x", 500 + i));
    }
    let forked = b.file(data, APFS_ROOT, "forked", b"x");
    b.resource(data, forked, &noise(1, 9000));
    let image = b.build();
    let junk = noise(97, 4096);
    // Everything up to the end of the metadata written last.
    let end = image.iter().rposition(|&b| b != 0).unwrap_or(0) + 1;
    for seed in 0..120 {
        let mut damaged = image.clone();
        for k in 0..12 {
            let at = (seed * 7919 + k * 104_729) % end;
            damaged[at] = junk[(seed * 31 + k) % junk.len()];
        }
        if let Ok(fs) = open(disk(damaged), 0) {
            walk(fs.as_ref(), &fs.root(), 0);
        }
    }
}
