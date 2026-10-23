//! Opening damaged volumes with what a scan would find (simulated here; the
//! scan crate tests the real thing).

use std::collections::BTreeMap;

use crate::clues::{folder_sector, mft_record};
use crate::testutil::{FatBuilder, NTFS_CLUSTER, NTFS_ROOT, NtfsBuilder, disk, noise, text};
use crate::{
    Entry, EntryId, FatLayout, FatRoot, FileSystem, FsKind, Rebuild, open, open_rebuilt,
    read_to_end, resolve,
};

fn names(entries: &[Entry]) -> Vec<String> {
    entries.iter().map(|e| e.name.clone()).collect()
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

/// Every MFT record in an image, as a scan would note them.
fn mft_records(image: &[u8]) -> BTreeMap<u64, u64> {
    image
        .chunks(512)
        .enumerate()
        .filter_map(|(i, sector)| mft_record(sector).map(|r| (r.number, i as u64 * 512)))
        .collect()
}

/// An NTFS volume whose first 160 KiB were overwritten by a FAT32 quick
/// format: the boot sector, `$Bitmap`'s data and the MFT's first fragment
/// (records 0-31) are gone. Records 32 on, the mirror and the backup boot
/// sector are left.
struct Reformatted {
    image: Vec<u8>,
    survivor: Vec<u8>,
    kept: Vec<u8>,
    inside: Vec<u8>,
}

fn reformatted_ntfs() -> Reformatted {
    let mut b = NtfsBuilder::new(2048);
    b.label("Old NTFS");
    let root = NTFS_ROOT;
    // Record 24: a folder whose record the format destroys.
    let early = b.mkdir(root, "early folder");
    // Records 25-31, with data in the overwritten area.
    for i in 0..7 {
        b.file(root, &format!("early-{i}.bin"), &noise(i, 6 * NTFS_CLUSTER));
    }
    let survivor = text("survivor", 5000);
    let kept = noise(50, 3 * NTFS_CLUSTER + 7);
    let inside = text("inside", 9000);
    b.file(early, "survivor.txt", &survivor); // record 32
    b.file(root, "kept.bin", &kept);
    let docs = b.mkdir(root, "docs");
    b.file(docs, "inside.txt", &inside);
    let mut image = b.build();
    let fat = FatBuilder::new(FsKind::Fat32, image.len() / 512).build();
    image[..160 * 1024].copy_from_slice(&fat[..160 * 1024]);
    Reformatted {
        image,
        survivor,
        kept,
        inside,
    }
}

#[test]
fn ntfs_after_a_quick_format_from_its_backup_boot_sector() {
    let r = reformatted_ntfs();
    let size = r.image.len() as u64;
    let disk = disk(r.image);
    // What's at the start now is the new, empty FAT32 volume.
    assert_ne!(open(disk.clone(), 0).unwrap().info().kind, FsKind::Ntfs);

    let rebuild = Rebuild {
        boot_sector: Some(size - 512),
        ..Rebuild::default()
    };
    let fs = open_rebuilt(disk, FsKind::Ntfs, 0, size, &rebuild).unwrap();
    // $Volume (record 3) is read from the MFT mirror.
    assert_eq!(fs.info().label.as_deref(), Some("Old NTFS"));
    let root = fs.list(&fs.root()).unwrap();
    assert_eq!(
        names(&root),
        [
            "$MFT",
            "$MFTMirr",
            "$Volume",
            "docs",
            "kept.bin",
            "Lost files"
        ]
    );
    assert_eq!(read_all(fs.as_ref(), find(&root, "kept.bin")), r.kept);
    let docs = fs.list(&find(&root, "docs").id).unwrap();
    assert_eq!(read_all(fs.as_ref(), find(&docs, "inside.txt")), r.inside);

    // The folder whose record is gone still holds its files together.
    let lost = fs.list(&find(&root, "Lost files").id).unwrap();
    assert_eq!(names(&lost), ["Folder 24"]);
    assert_eq!(lost[0].id, EntryId::LostFolder { number: 24 });
    let inside_lost = fs.list(&lost[0].id).unwrap();
    assert_eq!(names(&inside_lost), ["survivor.txt"]);
    assert_eq!(read_all(fs.as_ref(), &inside_lost[0]), r.survivor);
}

#[test]
fn ntfs_without_mft_or_mirror_from_records_found_on_the_disk() {
    let mut r = reformatted_ntfs();
    // Also lose the mirror: nothing says where the MFT is any more.
    let mirror = 1024 * NTFS_CLUSTER;
    r.image[mirror..mirror + NTFS_CLUSTER].fill(0);
    let size = r.image.len() as u64;
    let found = mft_records(&r.image);
    assert!(
        found.contains_key(&32) && !found.contains_key(&24),
        "{found:?}"
    );
    let disk = disk(r.image);

    let rebuild = Rebuild {
        boot_sector: Some(size - 512),
        mft_records: found,
        ..Rebuild::default()
    };
    let fs = open_rebuilt(disk, FsKind::Ntfs, 0, size, &rebuild).unwrap();
    let kept = resolve(fs.as_ref(), "kept.bin", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &kept), r.kept);
    let inside = resolve(fs.as_ref(), "docs/inside.txt", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &inside), r.inside);
    let survivor = resolve(fs.as_ref(), "Lost files/Folder 24/survivor.txt", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &survivor), r.survivor);
}

#[test]
fn fat32_without_boot_sector_or_fat() {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    b.label(b"NOFAT      ");
    let root = b.root();
    b.file(root, "readme.txt", &text("readme", 700));
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 3000));
    let (a, c) = b.alloc_interleaved(3);
    b.file_in(docs, "part-a.bin", &noise(1, 3 * 512), a);
    b.file_in(docs, "part-b.bin", &noise(2, 3 * 512), c);
    // Enough entries that the root folder takes a second cluster, allocated
    // after all the data above: without the FAT nothing leads to it.
    for i in 0..6 {
        b.file(
            root,
            &format!("file number {i}.txt"),
            &text(&format!("f{i}"), 900),
        );
    }
    let (late, _) = b.mkdir(root, "late folder");
    b.file(late, "late.txt", &text("late", 1500));
    let old = b.file(root, "deleted one.txt", &text("deleted", 800));
    b.delete(&old);
    let data_start = b.data_start();
    let clusters = u32::try_from(b.cluster_count()).unwrap();
    let mut image = b.build();
    // Wipe the boot sector, its backup and both FATs.
    image[..data_start].fill(0);

    let folders: Vec<u32> = image[data_start..]
        .chunks(512)
        .enumerate()
        .filter(|(_, sector)| folder_sector(sector).is_some())
        .map(|(i, _)| u32::try_from(i).unwrap() + 2)
        .collect();
    let layout = FatLayout {
        kind: FsKind::Fat32,
        cluster_size: 512,
        data_start: data_start as u64,
        clusters,
        root: FatRoot::Cluster { cluster: 2 },
        fat_start: None,
        folders,
    };
    let size = image.len() as u64;
    let rebuild = Rebuild {
        fat: Some(layout),
        ..Rebuild::default()
    };
    let fs = open_rebuilt(disk(image), FsKind::Fat32, 0, size, &rebuild).unwrap();
    assert_eq!(fs.info().label.as_deref(), Some("NOFAT"));

    let root_entries = fs.list(&fs.root()).unwrap();
    assert!(root_entries.iter().any(|e| e.name == "readme.txt"));
    let readme = find(&root_entries, "readme.txt");
    assert_eq!(read_all(fs.as_ref(), readme), text("readme", 700));
    assert!(readme.note.as_deref().unwrap().contains("FAT is gone"));

    let docs = fs.list(&find(&root_entries, "docs").id).unwrap();
    assert_eq!(
        read_all(fs.as_ref(), find(&docs, "notes.txt")),
        text("notes", 3000)
    );
    // A fragmented file can't be put back together without the FAT.
    assert_ne!(
        read_all(fs.as_ref(), find(&docs, "part-a.bin")),
        noise(1, 3 * 512)
    );

    // The root's second cluster is found by the scan: what it lists is
    // under "Lost files", names and all.
    let lost = fs.list(&find(&root_entries, "Lost files").id).unwrap();
    let late = find(&lost, "late folder");
    let late_entries = fs.list(&late.id).unwrap();
    assert_eq!(
        read_all(fs.as_ref(), find(&late_entries, "late.txt")),
        text("late", 1500)
    );
    let deleted = find(&lost, "deleted one.txt");
    assert!(deleted.deleted);
    assert_eq!(read_all(fs.as_ref(), deleted), text("deleted", 800));
    // Every root file is somewhere. One whose long name was split across the
    // two clusters keeps only its short name (FILENU~N.TXT).
    let everything: Vec<&Entry> = root_entries.iter().chain(&lost).collect();
    let mut long_names = 0;
    for i in 0..6 {
        let name = format!("file number {i}.txt");
        let contents = text(&format!("f{i}"), 900);
        let entry = everything
            .iter()
            .find(|e| e.size == 900 && read_all(fs.as_ref(), e) == contents)
            .unwrap_or_else(|| panic!("{name} not found"));
        assert!(
            entry.name == name || entry.name.starts_with("FILENU~"),
            "{}",
            entry.name
        );
        long_names += usize::from(entry.name == name);
    }
    assert!(long_names >= 5);
}
