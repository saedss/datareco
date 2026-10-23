//! The exFAT reader against volumes built in memory.

use crate::testutil::{ExfatBuilder, STAMP, disk, noise, text};
use crate::{Entry, EntryId, FileSystem, FsKind, attributes, open, read_to_end, resolve};

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

#[test]
fn files_folders_and_deleted_entries() {
    let mut b = ExfatBuilder::new(8192);
    b.label("Test exFAT");
    let root = b.root();
    b.file(root, "readme.txt", &text("readme", 300));
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 12_000));
    let (a, c) = b.alloc_interleaved(4);
    b.file_in(docs, "part-a.bin", &noise(1, 4 * 1024), a, false);
    b.file_in(
        docs,
        "part-b.bin",
        &noise(2, 3 * 1024 + 10),
        c[..4].to_vec(),
        false,
    );
    b.file(root, "empty.txt", &[]);
    b.file(
        root,
        "A file name longer than fifteen characters.txt",
        &text("long", 2000),
    );
    b.file(root, "日本語.txt", &text("japanese", 600));
    let old = b.file(root, "old-photo.png", &noise(3, 5000));
    b.delete(&old);
    // A fragmented file: deleting it keeps its FAT chain.
    let (x, y) = b.alloc_interleaved(3);
    let gone = b.file_in(root, "fragmented.bin", &noise(4, 3 * 1024), x, false);
    b.file_in(root, "keeper.bin", &noise(5, 3 * 1024), y, false);
    b.delete(&gone);
    let (folder_cluster, folder) = b.mkdir(root, "deleted-folder");
    let inner = b.file(folder_cluster, "inner.txt", &text("inner", 700));
    b.delete(&inner);
    b.delete(&folder);

    let fs = open(disk(b.build()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::Exfat);
    assert_eq!(fs.info().label.as_deref(), Some("Test exFAT"));
    assert_eq!(fs.info().cluster_size, 1024);

    let root_entries = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&root_entries),
        [
            ("readme.txt".to_owned(), false),
            ("docs".to_owned(), false),
            ("empty.txt".to_owned(), false),
            (
                "A file name longer than fifteen characters.txt".to_owned(),
                false
            ),
            ("日本語.txt".to_owned(), false),
            ("old-photo.png".to_owned(), true),
            ("fragmented.bin".to_owned(), true),
            ("keeper.bin".to_owned(), false),
            ("deleted-folder".to_owned(), true),
        ]
    );

    let readme = find(&root_entries, "readme.txt");
    assert_eq!(read_all(fs.as_ref(), readme), text("readme", 300));
    assert_eq!(readme.size, 300);
    // Stored as 10:30 at UTC+2.
    assert_eq!(readme.modified, Some(STAMP - 2 * 3600));
    assert_eq!(readme.created, Some(STAMP - 2 * 3600));
    assert_eq!(readme.attributes, attributes::ARCHIVE);
    assert_eq!(read_all(fs.as_ref(), find(&root_entries, "empty.txt")), b"");
    let long = find(
        &root_entries,
        "A file name longer than fifteen characters.txt",
    );
    assert_eq!(read_all(fs.as_ref(), long), text("long", 2000));
    assert_eq!(
        read_all(fs.as_ref(), find(&root_entries, "日本語.txt")),
        text("japanese", 600)
    );
    assert_eq!(fs.extents(&readme.id, None).unwrap().len(), 1);

    let docs = find(&root_entries, "docs");
    assert!(docs.is_dir);
    assert_eq!(docs.size, 0);
    let doc_entries = fs.list(&docs.id).unwrap();
    assert_eq!(
        read_all(fs.as_ref(), find(&doc_entries, "notes.txt")),
        text("notes", 12_000)
    );
    let part_a = find(&doc_entries, "part-a.bin");
    assert_eq!(read_all(fs.as_ref(), part_a), noise(1, 4 * 1024));
    assert_eq!(
        read_all(fs.as_ref(), find(&doc_entries, "part-b.bin")),
        noise(2, 3 * 1024 + 10)
    );
    assert_eq!(fs.extents(&part_a.id, None).unwrap().len(), 4);

    // A deleted file stored in one piece reads back whole.
    let photo = find(&root_entries, "old-photo.png");
    assert_eq!(read_all(fs.as_ref(), photo), noise(3, 5000));
    let note = photo.note.as_deref().unwrap();
    assert!(note.starts_with("Deleted."), "{note}");
    assert!(!note.contains("overwritten"), "{note}");
    assert!(!note.contains("follow one another"), "{note}");

    // A deleted fragmented file follows its surviving FAT chain.
    let fragmented = find(&root_entries, "fragmented.bin");
    assert!(
        matches!(fragmented.id, EntryId::Clusters { chain: true, .. }),
        "{:?}",
        fragmented.id
    );
    assert_eq!(fs.extents(&fragmented.id, None).unwrap().len(), 3);
    assert_eq!(read_all(fs.as_ref(), fragmented), noise(4, 3 * 1024));
    assert_eq!(
        read_all(fs.as_ref(), find(&root_entries, "keeper.bin")),
        noise(5, 3 * 1024)
    );

    let folder = find(&root_entries, "deleted-folder");
    let inside = fs.list(&folder.id).unwrap();
    assert_eq!(names(&inside), [("inner.txt".to_owned(), true)]);
    assert_eq!(read_all(fs.as_ref(), &inside[0]), text("inner", 700));

    let found = resolve(fs.as_ref(), "DOCS/Notes.TXT", false).unwrap();
    assert_eq!(found.name, "notes.txt");
}

#[test]
fn reused_clusters_are_flagged() {
    let mut b = ExfatBuilder::new(8192);
    let root = b.root();
    let old = b.file(root, "old.bin", &noise(6, 4 * 1024));
    b.delete(&old);
    b.file_in(
        root,
        "new.bin",
        &noise(7, 2 * 1024),
        old.chain[..2].to_vec(),
        true,
    );
    // A fragmented file whose clusters were reused: its chain can't be
    // trusted, so the clusters are a guess.
    let (x, _) = b.alloc_interleaved(2);
    let frag = b.file_in(root, "frag.bin", &noise(8, 2 * 1024), x, false);
    b.delete(&frag);
    b.mark(frag.chain[1], true);

    let fs = open(disk(b.build()), 0).unwrap();
    let entries = fs.list(&fs.root()).unwrap();
    let old = find(&entries, "old.bin");
    assert!(
        old.note
            .as_deref()
            .unwrap()
            .contains("2 of its 4 clusters are now used"),
        "{:?}",
        old.note
    );
    let frag = find(&entries, "frag.bin");
    assert!(
        matches!(frag.id, EntryId::Clusters { chain: false, .. }),
        "{:?}",
        frag.id
    );
    assert!(
        frag.note.as_deref().unwrap().contains("follow one another"),
        "{:?}",
        frag.note
    );
}

#[test]
fn a_root_folder_over_several_clusters() {
    let mut b = ExfatBuilder::new(8192);
    let root = b.root();
    for i in 0..40 {
        b.file(
            root,
            &format!("file number {i:03}.txt"),
            &text(&format!("f{i}"), 100 + i),
        );
    }
    let fs = open(disk(b.build()), 0).unwrap();
    let entries = fs.list(&fs.root()).unwrap();
    assert_eq!(entries.len(), 40);
    let last = find(&entries, "file number 039.txt");
    assert_eq!(read_all(fs.as_ref(), last), text("f39", 139));
}

#[test]
fn garbage_after_the_boot_sector_never_panics() {
    let mut b = ExfatBuilder::new(8192);
    let root = b.root();
    b.file(root, "x.txt", &text("x", 100));
    let mut image = b.build();
    for seed in 0..4 {
        let garbage = noise(10 + seed, image.len() - 512);
        image[512..].copy_from_slice(&garbage);
        let Ok(fs) = open(disk(image.clone()), 0) else {
            continue;
        };
        if let Ok(entries) = fs.list(&fs.root()) {
            for entry in entries.iter().take(50) {
                let _ = read_to_end(fs.as_ref(), &entry.id, None, 1 << 20);
                if entry.is_dir {
                    let _ = fs.list(&entry.id);
                }
            }
        }
    }
}
