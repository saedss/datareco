//! The NTFS reader against volumes built in memory.

use crate::testutil::{NTFS_CLUSTER, NTFS_ROOT, NtfsBuilder, STAMP, disk, noise, text};
use crate::{
    Entry, EntryId, Extent, FileSystem, FsKind, StreamInfo, attributes, open, read_to_end, resolve,
};

const C: usize = NTFS_CLUSTER;

fn names(entries: &[Entry]) -> Vec<(String, bool)> {
    entries
        .iter()
        .map(|e| (e.name.clone(), e.deleted))
        .collect()
}

fn read_all(fs: &dyn FileSystem, entry: &Entry) -> Vec<u8> {
    read_to_end(fs, &entry.id, None, u64::MAX).unwrap()
}

fn find<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
    entries
        .iter()
        .find(|e| e.name == name)
        .unwrap_or_else(|| panic!("no {name} in {:?}", names(entries)))
}

fn live(names: &[&str]) -> Vec<(String, bool)> {
    names.iter().map(|n| ((*n).to_owned(), false)).collect()
}

#[test]
fn files_folders_and_streams() {
    let mut b = NtfsBuilder::new(2048);
    b.label("Test NTFS");
    let root = NTFS_ROOT;
    let readme = b.file(root, "readme.txt", &text("readme", 300));
    let big = b.file(root, "big.txt", &text("big", 20_000));
    b.file(root, "empty.txt", &[]);
    b.file(root, "Résumé café.txt", &text("resume", 1000));
    b.file(root, "日本語.txt", &text("japanese", 600));
    let long = b.file(root, "A long file name.txt", &text("long", 700));
    b.dos_name(long, root, "ALONGF~1.TXT");
    let docs = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 9000));
    let a = b.alloc(2);
    b.alloc(1);
    let c = b.alloc(3);
    let fragmented = noise(1, 5 * C - 100);
    b.file_runs(
        docs,
        "fragmented.bin",
        &fragmented,
        &[(Some(a), 2), (Some(c), 3)],
        fragmented.len() as u64,
    );
    b.stream(big, "Zone.Identifier", b"[ZoneTransfer]\r\nZoneId=3\r\n");
    b.stream(big, "big stream", &noise(2, 10_000));
    b.hard_link(readme, docs, "readme-link.txt");
    let readme_record = b.record_offset(readme);
    let image = b.build();

    let fs = open(disk(image.clone()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::Ntfs);
    assert_eq!(fs.info().label.as_deref(), Some("Test NTFS"));
    assert_eq!(fs.info().cluster_size, 4096);

    let root_entries = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&root_entries),
        live(&[
            "$Bitmap",
            "$MFT",
            "$MFTMirr",
            "$Volume",
            "A long file name.txt",
            "big.txt",
            "docs",
            "empty.txt",
            "readme.txt",
            "Résumé café.txt",
            "日本語.txt",
        ])
    );

    // A small file lives in its MFT record.
    let readme = find(&root_entries, "readme.txt");
    assert_eq!(read_all(fs.as_ref(), readme), text("readme", 300));
    assert_eq!(readme.size, 300);
    assert_eq!(
        (readme.created, readme.modified, readme.accessed),
        (Some(STAMP), Some(STAMP), Some(STAMP))
    );
    assert_eq!(readme.attributes, attributes::ARCHIVE);
    let extents = fs.extents(&readme.id, None).unwrap();
    assert_eq!(extents.len(), 1);
    // On disk, the record's sector ends hold fixup values instead, so only
    // the start is compared.
    let at = usize::try_from(extents[0].offset).unwrap();
    assert!(at > readme_record && at + 300 <= readme_record + 1024);
    let sector_end = (at / 512 + 1) * 512 - 2;
    assert_eq!(
        image[at..sector_end],
        text("readme", 300)[..sector_end - at]
    );

    let big = find(&root_entries, "big.txt");
    assert_eq!(read_all(fs.as_ref(), big), text("big", 20_000));
    assert_eq!(fs.extents(&big.id, None).unwrap().len(), 1);
    assert_eq!(
        big.streams,
        [
            StreamInfo {
                name: "Zone.Identifier".to_owned(),
                size: 26
            },
            StreamInfo {
                name: "big stream".to_owned(),
                size: 10_000
            },
        ]
    );
    let zone = read_to_end(fs.as_ref(), &big.id, Some("Zone.Identifier"), u64::MAX).unwrap();
    assert_eq!(zone, b"[ZoneTransfer]\r\nZoneId=3\r\n");
    assert_eq!(
        read_to_end(fs.as_ref(), &big.id, Some("big stream"), u64::MAX).unwrap(),
        noise(2, 10_000)
    );
    let missing = read_to_end(fs.as_ref(), &big.id, Some("nope"), u64::MAX).unwrap_err();
    assert_eq!(missing.kind(), "not_found");

    assert_eq!(read_all(fs.as_ref(), find(&root_entries, "empty.txt")), b"");
    assert_eq!(
        read_all(fs.as_ref(), find(&root_entries, "日本語.txt")),
        text("japanese", 600)
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&root_entries, "A long file name.txt")),
        text("long", 700)
    );

    let docs = find(&root_entries, "docs");
    assert!(docs.is_dir);
    assert_eq!(docs.size, 0);
    assert_ne!(docs.attributes & attributes::DIRECTORY, 0);
    let doc_entries = fs.list(&docs.id).unwrap();
    assert_eq!(
        names(&doc_entries),
        live(&["fragmented.bin", "notes.txt", "readme-link.txt"])
    );
    let frag = find(&doc_entries, "fragmented.bin");
    assert_eq!(read_all(fs.as_ref(), frag), fragmented);
    assert_eq!(
        fs.extents(&frag.id, None).unwrap(),
        [
            Extent {
                offset: (a * C) as u64,
                length: 2 * C as u64,
                sparse: false
            },
            Extent {
                offset: (c * C) as u64,
                length: 3 * C as u64 - 100,
                sparse: false
            },
        ]
    );
    // A hard link is the same file under another name.
    assert_eq!(
        read_all(fs.as_ref(), find(&doc_entries, "readme-link.txt")),
        text("readme", 300)
    );

    let found = resolve(fs.as_ref(), "DOCS/Notes.TXT", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &found), text("notes", 9000));
}

#[test]
fn sparse_compressed_and_partly_written_files() {
    let mut b = NtfsBuilder::new(2048);
    let root = NTFS_ROOT;
    let sparse = [noise(3, 2 * C), vec![0; 3 * C], noise(4, C + 500)].concat();
    let a = b.alloc(2);
    let c = b.alloc(2);
    b.file_runs(
        root,
        "sparse.bin",
        &sparse,
        &[(Some(a), 2), (None, 3), (Some(c), 2)],
        sparse.len() as u64,
    );
    // Units: compressible text, all zeros, noise (stored as is), a short tail.
    let unit = 16 * C;
    let compressed = [
        text("unit0", unit),
        vec![0; unit],
        noise(5, unit),
        text("tail", 10_000),
    ]
    .concat();
    b.compressed_file(root, "compressed.txt", &compressed);
    let written = noise(6, 3 * C);
    let at = b.alloc(3);
    b.file_runs(root, "growing.bin", &written, &[(Some(at), 3)], 5000);

    let fs = open(disk(b.build()), 0).unwrap();
    let entries = fs.list(&fs.root()).unwrap();

    let entry = find(&entries, "sparse.bin");
    assert_ne!(entry.attributes & attributes::SPARSE, 0);
    assert_eq!(read_all(fs.as_ref(), entry), sparse);
    assert_eq!(
        fs.extents(&entry.id, None).unwrap(),
        [
            Extent {
                offset: (a * C) as u64,
                length: 2 * C as u64,
                sparse: false
            },
            Extent {
                offset: 0,
                length: 3 * C as u64,
                sparse: true
            },
            Extent {
                offset: (c * C) as u64,
                length: C as u64 + 500,
                sparse: false
            },
        ]
    );

    let entry = find(&entries, "compressed.txt");
    assert_ne!(entry.attributes & attributes::COMPRESSED, 0);
    assert_eq!(entry.size, compressed.len() as u64);
    assert_eq!(read_all(fs.as_ref(), entry), compressed);
    // Reads that start mid-unit and cross into the next one.
    for offset in [100, unit - 50, 2 * unit + 7, 3 * unit + 4000] {
        let mut buf = vec![0; 300];
        let n = fs.read(&entry.id, None, offset as u64, &mut buf).unwrap();
        assert_eq!(
            buf[..n],
            compressed[offset..(offset + 300).min(compressed.len())],
            "at {offset}"
        );
    }

    // Past the initialized size, a file reads as zeros whatever is on disk.
    let entry = find(&entries, "growing.bin");
    let expected = [&written[..5000], &vec![0; 3 * C - 5000][..]].concat();
    assert_eq!(read_all(fs.as_ref(), entry), expected);
}

#[test]
fn attribute_lists_join_data_from_extension_records() {
    let mut b = NtfsBuilder::new(2048);
    let data = noise(7, 6 * C + 123);
    b.split_file(NTFS_ROOT, "split.bin", &data);
    let fs = open(disk(b.build()), 0).unwrap();
    let entry = resolve(fs.as_ref(), "split.bin", false).unwrap();
    assert_eq!(entry.size, data.len() as u64);
    assert_eq!(read_all(fs.as_ref(), &entry), data);
    assert_eq!(fs.extents(&entry.id, None).unwrap().len(), 2);
}

#[test]
fn deleted_files_folders_and_lost_files() {
    let mut b = NtfsBuilder::new(2048);
    let root = NTFS_ROOT;
    let small = b.file(root, "small-deleted.txt", &text("small", 200));
    b.delete(small);
    let photo = b.file(root, "photo.jpg", &noise(8, 30_000));
    b.delete(photo);
    // Two of this file's four clusters were taken by another file since.
    let lcn = b.alloc(4);
    let old = b.file_runs(
        root,
        "old.bin",
        &noise(9, 4 * C),
        &[(Some(lcn), 4)],
        4 * C as u64,
    );
    b.delete(old);
    b.mark(lcn, true);
    b.mark(lcn + 1, true);
    let folder = b.mkdir(root, "old-folder");
    let inside = b.file(folder, "inside.txt", &text("inside", 5000));
    b.delete(inside);
    b.delete(folder);
    // A deleted folder whose record now holds a new file: what it held is lost.
    let gone = b.mkdir(root, "gone");
    let lost = b.file(gone, "lost.txt", &text("lost", 3000));
    b.delete(lost);
    b.delete(gone);
    b.reuse(gone, root, "new-file.txt", &text("new", 100));

    let fs = open(disk(b.build()), 0).unwrap();
    let entries = fs.list(&fs.root()).unwrap();
    let user: Vec<_> = names(&entries)
        .into_iter()
        .filter(|(n, _)| !n.starts_with('$'))
        .collect();
    assert_eq!(
        user,
        [
            ("new-file.txt".to_owned(), false),
            ("old-folder".to_owned(), true),
            ("old.bin".to_owned(), true),
            ("photo.jpg".to_owned(), true),
            ("small-deleted.txt".to_owned(), true),
            ("Lost files".to_owned(), false),
        ]
    );

    let small = find(&entries, "small-deleted.txt");
    assert_eq!(read_all(fs.as_ref(), small), text("small", 200));
    assert_eq!(small.note.as_deref(), Some("Deleted."));
    let photo = find(&entries, "photo.jpg");
    assert_eq!(read_all(fs.as_ref(), photo), noise(8, 30_000));
    assert_eq!(photo.note.as_deref(), Some("Deleted."));
    let old = find(&entries, "old.bin");
    assert!(
        old.note
            .as_deref()
            .unwrap()
            .contains("2 of its 4 clusters are now used"),
        "{:?}",
        old.note
    );
    assert_eq!(
        read_all(fs.as_ref(), find(&entries, "new-file.txt")),
        text("new", 100)
    );

    let folder = find(&entries, "old-folder");
    assert!(folder.is_dir && folder.deleted);
    let inside = fs.list(&folder.id).unwrap();
    assert_eq!(names(&inside), [("inside.txt".to_owned(), true)]);
    assert_eq!(read_all(fs.as_ref(), &inside[0]), text("inside", 5000));

    let lost_files = find(&entries, "Lost files");
    assert_eq!(lost_files.id, EntryId::Orphans);
    let lost = fs.list(&lost_files.id).unwrap();
    assert_eq!(names(&lost), [("lost.txt".to_owned(), true)]);
    assert_eq!(read_all(fs.as_ref(), &lost[0]), text("lost", 3000));
    assert_eq!(
        resolve(fs.as_ref(), "lost files/lost.txt", false)
            .unwrap()
            .name,
        "lost.txt"
    );
}

#[test]
fn a_damaged_mft_start_falls_back_to_the_mirror() {
    let mut b = NtfsBuilder::new(2048);
    b.label("Mirror");
    b.file(NTFS_ROOT, "kept.txt", &text("kept", 5000));
    let at = b.record_offset(0);
    let mut image = b.build();
    image[at..at + 1024].fill(0);
    let fs = open(disk(image), 0).unwrap();
    assert_eq!(fs.info().label.as_deref(), Some("Mirror"));
    let kept = resolve(fs.as_ref(), "kept.txt", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &kept), text("kept", 5000));
}

/// Lists and reads everything it can reach, ignoring errors.
fn walk(fs: &dyn FileSystem) {
    let mut folders = vec![fs.root()];
    let mut visited = 0;
    while let Some(folder) = folders.pop() {
        visited += 1;
        if visited > 200 {
            break;
        }
        let Ok(entries) = fs.list(&folder) else {
            continue;
        };
        for entry in entries.iter().take(100) {
            let _ = read_to_end(fs, &entry.id, None, 1 << 20);
            let _ = fs.extents(&entry.id, None);
            for stream in &entry.streams {
                let _ = read_to_end(fs, &entry.id, Some(&stream.name), 1 << 20);
            }
            if entry.is_dir && entry.id != folder {
                folders.push(entry.id.clone());
            }
        }
    }
}

#[test]
fn damaged_records_never_panic() {
    let mut b = NtfsBuilder::new(2048);
    let root = NTFS_ROOT;
    let docs = b.mkdir(root, "docs");
    let big = b.file(docs, "big.txt", &text("big", 20_000));
    b.stream(big, "ads", &noise(1, 3000));
    b.compressed_file(root, "packed.txt", &text("packed", 100_000));
    b.split_file(root, "split.bin", &noise(2, 6 * C));
    let gone = b.file(root, "gone.bin", &noise(3, 9000));
    b.delete(gone);
    let offsets: Vec<usize> = (0..64).map(|n| b.record_offset(n)).collect();
    let image = b.build();

    for seed in 0..12 {
        // Flip a few bytes in each record, past its header.
        let mut damaged = image.clone();
        for (n, &at) in offsets.iter().enumerate() {
            let picks = noise(seed * 1000 + n as u64 + 1, 24);
            for pair in picks.chunks(2) {
                let position =
                    56 + usize::from(u16::from_le_bytes([pair[0], pair[1]])) % (1024 - 56);
                damaged[at + position] = pair[1].wrapping_mul(31);
            }
        }
        if let Ok(fs) = open(disk(damaged), 0) {
            walk(fs.as_ref());
        }
    }
    // Everything after the boot sector replaced.
    let mut damaged = image.clone();
    let junk = noise(99, damaged.len() - 512);
    damaged[512..].copy_from_slice(&junk);
    if let Ok(fs) = open(disk(damaged), 0) {
        walk(fs.as_ref());
    }
}
