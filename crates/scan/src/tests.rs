//! Scans of damaged disks built in memory, and recovery from them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datareco_fs::testutil::{
    FatBuilder, NTFS_CLUSTER, NTFS_ROOT, NtfsBuilder, STAMP, disk, noise, text,
};
use datareco_fs::{Entry, FileSystem, FsKind, read_to_end, resolve};
use datareco_io::{DeviceList, DiskExtent, SourceSpec, VolumeInfo};

use crate::recover::{disks_of_path, disks_of_source, safe_name};
use crate::{
    Control, FoundVolume, RecoverOptions, SavedScan, Scan, ScanMode, VolumeState, open_found,
    recover,
};

fn read_all(fs: &dyn FileSystem, entry: &Entry) -> Vec<u8> {
    read_to_end(fs, &entry.id, None, u64::MAX).unwrap()
}

fn find(volumes: &[FoundVolume], kind: FsKind) -> &FoundVolume {
    volumes
        .iter()
        .find(|v| v.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} in {volumes:#?}"))
}

/// An empty folder for one test.
fn temp_folder(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("datareco-scan-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

/// An MBR disk holding the given volumes, each at its byte offset.
fn mbr_disk(volumes: &[(usize, u8, &[u8])]) -> Vec<u8> {
    let end = volumes
        .iter()
        .map(|(at, _, data)| at + data.len())
        .max()
        .unwrap();
    let mut image = vec![0u8; end];
    for (i, (at, kind, data)) in volumes.iter().enumerate() {
        let entry = 446 + 16 * i;
        image[entry + 4] = *kind;
        let start = u32::try_from(at / 512).unwrap();
        let sectors = u32::try_from(data.len() / 512).unwrap();
        image[entry + 8..entry + 12].copy_from_slice(&start.to_le_bytes());
        image[entry + 12..entry + 16].copy_from_slice(&sectors.to_le_bytes());
        image[*at..*at + data.len()].copy_from_slice(data);
    }
    image[510..512].copy_from_slice(&[0x55, 0xAA]);
    image
}

/// An NTFS volume quick-formatted as FAT32: the format overwrote its boot
/// sector and the MFT's first fragment (records 0-31).
fn reformatted_ntfs() -> (Vec<u8>, Vec<u8>) {
    let mut b = NtfsBuilder::new(2048);
    b.label("Old NTFS");
    for i in 0..8 {
        b.file(
            NTFS_ROOT,
            &format!("early-{i}.bin"),
            &noise(i, 6 * NTFS_CLUSTER),
        );
    }
    let kept = noise(50, 3 * NTFS_CLUSTER + 7);
    b.file(NTFS_ROOT, "kept.bin", &kept);
    let docs = b.mkdir(NTFS_ROOT, "docs");
    b.file(docs, "notes.txt", &text("notes", 9000));
    let mut image = b.build();
    let fat = FatBuilder::new(FsKind::Fat32, image.len() / 512).build();
    image[..160 * 1024].copy_from_slice(&fat[..160 * 1024]);
    (image, kept)
}

#[test]
fn a_full_scan_finds_an_ntfs_volume_after_a_quick_format() {
    let (image, kept) = reformatted_ntfs();
    let disk = disk(image);
    // A quick scan looks at the disk's last sectors too: the backup boot
    // sector there is enough to find the old volume.
    let quick = Scan::run(&disk, ScanMode::Quick).unwrap();
    assert_eq!(find(&quick, FsKind::Ntfs).state, VolumeState::FromBackup);
    assert_eq!(find(&quick, FsKind::Ntfs).found, None);

    let found = Scan::run(&disk, ScanMode::Full).unwrap();
    let ntfs = find(&found, FsKind::Ntfs);
    assert_eq!(ntfs.state, VolumeState::FromBackup);
    assert_eq!((ntfs.offset, ntfs.label.as_deref()), (0, Some("Old NTFS")));
    // The mirror's copies of records 0, 1 and 3, and records 32-34; records
    // 2 and 4-31 were in the overwritten part.
    let numbers: Vec<u64> = ntfs.rebuild.mft_records.keys().copied().collect();
    assert_eq!(numbers, [0, 1, 3, 32, 33, 34]);
    assert_eq!(ntfs.found, Some(6));
    // The new, empty volume ranks first: it opens with its own boot sector.
    assert_eq!(found[0].state, VolumeState::Intact);
    assert_ne!(found[0].kind, FsKind::Ntfs);

    let fs = open_found(Arc::clone(&disk), ntfs).unwrap();
    let entry = resolve(fs.as_ref(), "kept.bin", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &entry), kept);
    let notes = resolve(fs.as_ref(), "docs/notes.txt", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &notes), text("notes", 9000));
}

/// A FAT32 volume with its boot sector, backup and both FATs wiped.
fn fat_without_fat() -> (Vec<u8>, usize) {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    b.label(b"NO FAT     ");
    let root = b.root();
    b.file(root, "readme.txt", &text("readme", 700));
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 3000));
    let (deep, _) = b.mkdir(docs, "deeper");
    b.file(deep, "deep.txt", &text("deep", 2000));
    let (photos, _) = b.mkdir(root, "photos");
    b.file(photos, "photo.bin", &noise(3, 6000));
    let data_start = b.data_start();
    let mut image = b.build();
    image[..data_start].fill(0);
    (image, data_start)
}

#[test]
fn a_full_scan_rebuilds_fat32_without_its_boot_sector_or_fat() {
    let (image, data_start) = fat_without_fat();
    let disk = disk(image);
    assert!(Scan::run(&disk, ScanMode::Quick).unwrap().is_empty());

    let found = Scan::run(&disk, ScanMode::Full).unwrap();
    let fat = find(&found, FsKind::Fat32);
    assert_eq!(fat.state, VolumeState::Rebuilt);
    assert_eq!(fat.label.as_deref(), Some("NO FAT"));
    let layout = fat.rebuild.fat.as_ref().unwrap();
    assert_eq!(
        (layout.cluster_size, layout.data_start),
        (512, data_start as u64)
    );
    assert_eq!(layout.fat_start, None);

    let fs = open_found(Arc::clone(&disk), fat).unwrap();
    for (path, contents) in [
        ("readme.txt", text("readme", 700)),
        ("docs/notes.txt", text("notes", 3000)),
        ("docs/deeper/deep.txt", text("deep", 2000)),
        ("photos/photo.bin", noise(3, 6000)),
    ] {
        let entry = resolve(fs.as_ref(), path, false).unwrap();
        assert_eq!(read_all(fs.as_ref(), &entry), contents, "{path}");
    }
}

#[test]
fn a_quick_scan_finds_the_volumes_in_the_partition_table() {
    let mut n = NtfsBuilder::new(2048);
    n.label("Data");
    n.file(NTFS_ROOT, "a.txt", b"a");
    let ntfs = n.build();
    let mut f = FatBuilder::new(FsKind::Fat32, 70_000);
    f.label(b"USB        ");
    let fat = f.build();
    let image = mbr_disk(&[(1 << 20, 0x07, &ntfs), (16 << 20, 0x0C, &fat)]);
    let found = Scan::run(&disk(image), ScanMode::Quick).unwrap();
    assert_eq!(found.len(), 2, "{found:#?}");
    for volume in &found {
        assert_eq!(
            (volume.state, volume.score, volume.in_table),
            (VolumeState::Intact, 100, true)
        );
    }
    assert_eq!(found[0].label.as_deref(), Some("Data"));
    assert_eq!(found[1].label.as_deref(), Some("USB"));
}

#[test]
fn a_scan_can_be_saved_and_carried_on() {
    let (image, _) = fat_without_fat();
    let disk = disk(image);
    let summary = |found: Vec<FoundVolume>| -> Vec<String> {
        found
            .iter()
            .map(|v| {
                format!(
                    "{:?} {} {} {:?} {:?}",
                    v.kind, v.offset, v.score, v.found, v.evidence
                )
            })
            .collect()
    };
    let expected = summary(Scan::run(&disk, ScanMode::Full).unwrap());

    let mut scan = Scan::new(&disk, ScanMode::Full);
    scan.step(&disk, disk.size() / 3).unwrap();
    let path = temp_folder("saved.json");
    scan.save("image:test.img").write_to(&path).unwrap();
    let saved = SavedScan::read_from(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(saved.source, "image:test.img");
    assert!(!saved.done);

    let mut resumed = Scan::restore(&disk, &saved).unwrap();
    assert_eq!(resumed.progress().position, scan.progress().position);
    while !resumed.step(&disk, 1 << 20).unwrap().done {}
    assert_eq!(summary(resumed.results(&disk).unwrap()), expected);

    // A scan of another disk doesn't load.
    let other = datareco_fs::testutil::disk(vec![0; 1 << 20]);
    assert!(Scan::restore(&other, &saved).is_err());
}

#[test]
fn recovery_saves_files_folders_times_a_log_and_checksums() {
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    let root = b.root();
    b.file(root, "readme.txt", &text("readme", 700));
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 3000));
    let old = b.file(docs, "old:report?.txt", &text("old", 900));
    b.delete(&old);
    b.file(root, "con.txt", b"reserved name");
    let fs = datareco_fs::open(disk(b.build()), 0).unwrap();
    let items = fs.list(&fs.root()).unwrap();

    let destination = temp_folder("recover");
    let mut calls = 0;
    let summary = recover(
        fs.as_ref(),
        "test volume",
        &items,
        &destination,
        RecoverOptions::default(),
        &mut |_| {
            calls += 1;
            Control::Continue
        },
    )
    .unwrap();
    assert!(calls > 0);
    assert_eq!(summary.progress.files, 4);
    assert_eq!(summary.progress.folders, 1);
    assert_eq!(summary.progress.failed, 0);

    let read = |path: &str| std::fs::read(destination.join(path)).unwrap();
    assert_eq!(read("readme.txt"), text("readme", 700));
    assert_eq!(read("docs/notes.txt"), text("notes", 3000));
    // Characters Windows doesn't allow, and device names, are replaced.
    assert_eq!(read("docs/old_report_.txt"), text("old", 900));
    assert_eq!(read("_con.txt"), b"reserved name");
    let modified = std::fs::metadata(destination.join("readme.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let seconds = modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(seconds, STAMP.unsigned_abs());

    let log = std::fs::read_to_string(&summary.log).unwrap();
    assert!(
        log.contains("ok          docs/old_report_.txt (deleted), 900 bytes"),
        "{log}"
    );
    assert!(log.contains("Finished: 4 files and 1 folders"), "{log}");
    let sums = std::fs::read_to_string(&summary.checksums).unwrap();
    assert!(
        sums.contains("0bba2a8d7b4c3ae2b48b5bb59dd76fcd1d3dc2c42bbd3d6c4c1b5ac5c3b88ba4  _con.txt")
            || sums.lines().any(|l| l.ends_with("  _con.txt")),
        "{sums}"
    );
    assert_eq!(sums.lines().count(), 4, "{sums}");
    let expected_hash = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(text("notes", 3000));
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert!(
        sums.contains(&format!("{expected_hash}  docs/notes.txt")),
        "{sums}"
    );

    // Again into the same folder, without deleted files: nothing is overwritten.
    let options = RecoverOptions {
        deleted: false,
        ..RecoverOptions::default()
    };
    let second = recover(
        fs.as_ref(),
        "test volume",
        &items,
        &destination,
        options,
        &mut |_| Control::Continue,
    )
    .unwrap();
    assert_eq!(second.progress.files, 3);
    assert_eq!(read("readme (2).txt"), text("readme", 700));
    assert!(!destination.join("docs (2)/old_report_.txt").exists());

    // Stopping ends it with an error, and the log says so.
    let stopped = recover(
        fs.as_ref(),
        "test volume",
        &items,
        &destination,
        options,
        &mut |_| Control::Stop,
    );
    assert!(stopped.is_err());
    std::fs::remove_dir_all(&destination).unwrap();
}

#[test]
fn names_are_made_safe_for_windows() {
    assert_eq!(
        safe_name("a<b>c:d\"e/f\\g|h?i*j.txt"),
        "a_b_c_d_e_f_g_h_i_j.txt"
    );
    assert_eq!(safe_name("trailing. . "), "trailing");
    assert_eq!(safe_name("NUL"), "_NUL");
    assert_eq!(safe_name("com1.log"), "_com1.log");
    assert_eq!(safe_name("computer.txt"), "computer.txt");
    assert_eq!(safe_name(""), "_");
    assert_eq!(safe_name("?EADME.TXT"), "_EADME.TXT");
}

#[test]
fn destinations_on_the_source_disk_are_found() {
    let volume = |path: &str, mounts: &[&str], disk: u32| VolumeInfo {
        path: path.to_owned(),
        mount_points: mounts.iter().map(|m| (*m).to_owned()).collect(),
        label: None,
        file_system: Some("NTFS".to_owned()),
        size: Some(1 << 30),
        extents: vec![DiskExtent {
            disk,
            offset: 1 << 20,
            length: 1 << 30,
        }],
    };
    let devices = DeviceList {
        disks: Vec::new(),
        volumes: vec![
            volume(r"\\?\Volume{c}", &["C:\\"], 0),
            volume(r"\\?\Volume{d}", &["D:\\"], 1),
            volume(r"\\?\Volume{m}", &["C:\\Mounted\\"], 2),
        ],
    };
    assert_eq!(disks_of_path(r"C:\Users\me\Recovered", &devices), [0]);
    assert_eq!(disks_of_path(r"c:\mounted\files", &devices), [2]);
    assert_eq!(disks_of_path(r"D:\", &devices), [1]);
    assert!(disks_of_path(r"\\server\share", &devices).is_empty());
    assert_eq!(disks_of_source(&SourceSpec::PhysicalDisk(1), &devices), [1]);
    assert_eq!(
        disks_of_source(&SourceSpec::Volume("D:".to_owned()), &devices),
        [1]
    );
    assert!(
        disks_of_source(&SourceSpec::Image(Path::new("x.img").to_owned()), &devices).is_empty()
    );
}
