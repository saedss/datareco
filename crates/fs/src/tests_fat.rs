//! The FAT reader against volumes built in memory.

use crate::testutil::{FatBuilder, STAMP, disk, noise, text};
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
fn fat32_folders_files_and_deleted_entries() {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    b.label(b"TESTVOL    ");
    let root = b.root();
    b.file(root, "readme.txt", &text("readme", 300));
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 12_000));
    let (a, c) = b.alloc_interleaved(4);
    b.file_in(docs, "part-a.bin", &noise(1, 4 * 512), a);
    b.file_in(docs, "part-b.bin", &noise(2, 4 * 512), c);
    b.file(root, "empty.txt", &[]);
    b.file(root, "Résumé café.txt", &text("resume", 1000));
    b.file(root, "日本語.txt", &text("japanese", 600));
    let old = b.file(root, "old-photo.png", &noise(3, 5000));
    b.delete(&old);
    let (_, folder) = b.mkdir(root, "deleted-folder");
    let folder_cluster = folder.chain[0];
    let inner = b.file(folder_cluster, "inner.txt", &text("inner", 700));
    b.delete(&inner);
    b.delete(&folder);

    let fs = open(disk(b.build()), 0).unwrap();
    assert_eq!(fs.info().kind, FsKind::Fat32);
    assert_eq!(fs.info().label.as_deref(), Some("TESTVOL"));

    let root_entries = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&root_entries),
        [
            ("readme.txt".to_owned(), false),
            ("docs".to_owned(), false),
            ("empty.txt".to_owned(), false),
            ("Résumé café.txt".to_owned(), false),
            ("日本語.txt".to_owned(), false),
            ("old-photo.png".to_owned(), true),
            ("deleted-folder".to_owned(), true),
        ]
    );

    let readme = find(&root_entries, "readme.txt");
    assert_eq!(read_all(fs.as_ref(), readme), text("readme", 300));
    assert_eq!(readme.size, 300);
    assert_eq!(readme.modified, Some(STAMP));
    assert_eq!(readme.created, Some(STAMP));
    assert_eq!(readme.attributes, attributes::ARCHIVE);
    assert_eq!(read_all(fs.as_ref(), find(&root_entries, "empty.txt")), b"");
    assert_eq!(
        read_all(fs.as_ref(), find(&root_entries, "日本語.txt")),
        text("japanese", 600)
    );

    let docs = find(&root_entries, "docs");
    assert!(docs.is_dir);
    let doc_entries = fs.list(&docs.id).unwrap();
    assert_eq!(
        read_all(fs.as_ref(), find(&doc_entries, "notes.txt")),
        text("notes", 12_000)
    );
    // Fragmented files read through the FAT, piece by piece.
    let part_a = find(&doc_entries, "part-a.bin");
    assert_eq!(read_all(fs.as_ref(), part_a), noise(1, 4 * 512));
    assert_eq!(
        read_all(fs.as_ref(), find(&doc_entries, "part-b.bin")),
        noise(2, 4 * 512)
    );
    assert_eq!(fs.extents(&part_a.id, None).unwrap().len(), 4);

    // Deleted files read from their contiguous clusters; nothing reused them.
    let photo = find(&root_entries, "old-photo.png");
    assert!(photo.deleted);
    assert_eq!(read_all(fs.as_ref(), photo), noise(3, 5000));
    assert!(
        photo.note.as_deref().unwrap().starts_with("Deleted."),
        "{:?}",
        photo.note
    );
    assert!(!photo.note.as_deref().unwrap().contains("overwritten"));

    // A deleted folder still lists its (deleted) contents.
    let folder = find(&root_entries, "deleted-folder");
    let inside = fs.list(&folder.id).unwrap();
    assert_eq!(names(&inside), [("inner.txt".to_owned(), true)]);
    assert_eq!(read_all(fs.as_ref(), &inside[0]), text("inner", 700));

    let found = resolve(fs.as_ref(), "DOCS/Notes.TXT", false).unwrap();
    assert_eq!(found.name, "notes.txt");
}

#[test]
fn a_deleted_files_reused_clusters_are_flagged() {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    let root = b.root();
    let old = b.file(root, "old.bin", &noise(4, 4 * 512));
    b.delete(&old);
    // A new file takes two of the old one's clusters.
    let reused = old.chain[..2].to_vec();
    b.link(&reused);
    b.file_in(root, "new.bin", &noise(5, 2 * 512), reused);
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
}

#[test]
fn fat16_and_fat12_have_a_fixed_root_folder() {
    for (kind, sectors) in [(FsKind::Fat16, 40_000), (FsKind::Fat12, 2880)] {
        let mut b = FatBuilder::new(kind, sectors);
        let root = b.root();
        b.file(root, "readme.txt", &text("readme", 300));
        let (sub, _) = b.mkdir(root, "sub");
        b.file(sub, "deep.txt", &text("deep", 3000));
        let fs = open(disk(b.build()), 0).unwrap();
        assert_eq!(fs.info().kind, kind);
        assert_eq!(fs.root(), EntryId::FixedRoot);
        let readme = resolve(fs.as_ref(), "readme.txt", false).unwrap();
        assert_eq!(
            read_all(fs.as_ref(), &readme),
            text("readme", 300),
            "{kind:?}"
        );
        let deep = resolve(fs.as_ref(), "sub/deep.txt", false).unwrap();
        assert_eq!(read_all(fs.as_ref(), &deep), text("deep", 3000), "{kind:?}");
    }
}

#[test]
fn damaged_chains_end_instead_of_looping() {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    let root = b.root();
    let looped = b.file(root, "loop.bin", &noise(6, 3 * 512));
    b.set_fat(looped.chain[1], looped.chain[0]); // cluster 2 points back to 1
    let fs = open(disk(b.build()), 0).unwrap();
    let entry = resolve(fs.as_ref(), "loop.bin", false).unwrap();
    let error = read_to_end(fs.as_ref(), &entry.id, None, u64::MAX).unwrap_err();
    assert!(
        error.to_string().contains("cluster chain is damaged"),
        "{error}"
    );
}

#[test]
fn garbage_after_the_boot_sector_never_panics() {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    let root = b.root();
    b.file(root, "x.txt", &text("x", 100));
    let mut image = b.build();
    let garbage = noise(7, image.len() - 512);
    image[512..].copy_from_slice(&garbage);
    let fs = open(disk(image), 0).unwrap();
    // Whatever the root holds now, listing and reading must not panic.
    if let Ok(entries) = fs.list(&fs.root()) {
        for entry in entries.iter().take(50) {
            let _ = read_to_end(fs.as_ref(), &entry.id, None, 1 << 20);
            if entry.is_dir {
                let _ = fs.list(&entry.id);
            }
        }
    }
}
