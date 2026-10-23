//! Scans of damaged disks built in memory, and recovery from them.

use std::path::PathBuf;
use std::sync::Arc;

use datareco_fs::testutil::{
    EXT_ROOT, ExtBuilder, ExtOptions, FatBuilder, NTFS_CLUSTER, NTFS_ROOT, NtfsBuilder, STAMP,
    disk, noise, text,
};
use datareco_fs::{Entry, FileSystem, FsKind, read_to_end, resolve};

use crate::recover::safe_name;
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
fn scans_find_ext_volumes_and_open_them_from_a_backup_superblock() {
    // A partitioned ext4 volume: a quick scan finds it as it is.
    let mut b = ExtBuilder::new(ExtOptions::ext4());
    b.label("linux");
    b.file(EXT_ROOT, "hello.txt", &text("hello", 500));
    let quick = disk(mbr_disk(&[(1 << 20, 0x83, &b.build())]));
    let found = Scan::run(&quick, ScanMode::Quick).unwrap();
    let volume = find(&found, FsKind::Ext4);
    assert_eq!((volume.state, volume.in_table), (VolumeState::Intact, true));
    assert_eq!(volume.label.as_deref(), Some("linux"));
    let fs = open_found(Arc::clone(&quick), volume).unwrap();
    let hello = resolve(fs.as_ref(), "hello.txt", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &hello), text("hello", 500));

    // ext2 as mke2fs lays it out (8192 blocks per group), its superblock
    // wiped: a full scan finds the copy in block group 1 and opens with it.
    let options = ExtOptions {
        blocks_per_group: 8192,
        ..ExtOptions::ext2()
    };
    let mut b = ExtBuilder::new(options);
    let docs = b.mkdir(EXT_ROOT, "docs");
    b.file(docs, "notes.txt", &text("notes", 4000));
    let mut volume_image = b.build();
    volume_image[1024..2048].fill(0);
    let image = mbr_disk(&[(1 << 20, 0x83, &volume_image)]);
    let disk = disk(image);
    let found = Scan::run(&disk, ScanMode::Full).unwrap();
    let volume = find(&found, FsKind::Ext2);
    assert_eq!(volume.state, VolumeState::FromBackup);
    assert_eq!(volume.offset, 1 << 20);
    assert!(
        volume
            .evidence
            .iter()
            .any(|e| e.contains("backup superblock")),
        "{:?}",
        volume.evidence
    );
    let fs = open_found(Arc::clone(&disk), volume).unwrap();
    let notes = resolve(fs.as_ref(), "docs/notes.txt", false).unwrap();
    assert_eq!(read_all(fs.as_ref(), &notes), text("notes", 4000));
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

mod raw {
    //! Raw recovery: files found by their contents.

    use std::sync::Arc;

    use datareco_fs::testutil::{
        FatBuilder, NTFS_CLUSTER, NTFS_ROOT, NtfsBuilder, disk, noise, text,
    };
    use datareco_fs::{Entry, FileSystem, FsKind};

    use crate::raw::{RawScan, SavedRawScan, default_signatures, parse_signatures};
    use crate::{Control, RecoverOptions, recover};

    pub fn png(seed: u64, len: usize) -> Vec<u8> {
        let chunk = |kind: &[u8], data: &[u8]| {
            let len = u32::try_from(data.len()).unwrap().to_be_bytes();
            [&len[..], kind, data, &[0x12, 0x34, 0x56, 0x78]].concat()
        };
        [
            &b"\x89PNG\r\n\x1a\n"[..],
            &chunk(b"IHDR", &[0, 0, 0, 16, 0, 0, 0, 16, 8, 2, 0, 0, 0]),
            &chunk(b"IDAT", &noise(seed, len)),
            &chunk(b"IEND", &[]),
        ]
        .concat()
    }

    /// A JPEG with a thumbnail (its own start and end markers) in APP1, and
    /// image data with stuffed FF bytes and restart markers.
    pub fn jpeg(seed: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        out.extend(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        out.extend([
            0xFF, 0xE1, 0x00, 0x0A, 0xFF, 0xD8, 0x11, 0x22, 0xFF, 0xD9, 0x00, 0x00,
        ]);
        out.extend([0xFF, 0xDB, 0x00, 0x05, 0x00, 0x01, 0x02]);
        out.extend([0xFF, 0xDA, 0x00, 0x04, 0x01, 0x00]);
        for (i, b) in noise(seed, len).into_iter().enumerate() {
            out.push(b);
            if b == 0xFF {
                out.push(0x00);
            }
            if i % 1000 == 999 {
                out.extend([0xFF, 0xD0 + u8::try_from(i / 1000 % 8).unwrap()]);
            }
        }
        out.extend([0xFF, 0xD9]);
        out
    }

    pub fn wav(seed: u64, len: usize) -> Vec<u8> {
        let mut out =
            b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x40\x1F\0\0\x80\x3E\0\0\x02\0\x10\0data"
                .to_vec();
        out.extend(u32::try_from(len).unwrap().to_le_bytes());
        out.extend(noise(seed, len));
        let riff = u32::try_from(out.len() - 8).unwrap();
        out[4..8].copy_from_slice(&riff.to_le_bytes());
        out
    }

    pub fn pdf(seed: &str, len: usize) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n1 0 obj\n<< /Length 0 >>\nstream\n".to_vec();
        out.extend(text(seed, len));
        out.extend(b"\nendstream\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n");
        out
    }

    /// Files laid one after another on a disk with no file system, each at
    /// the start of a sector, with noise between them.
    fn bare_disk(files: &[Vec<u8>]) -> (Vec<u8>, Vec<u64>) {
        let mut image = noise(77, 3 * 512);
        let mut offsets = Vec::new();
        for (i, file) in files.iter().enumerate() {
            offsets.push(image.len() as u64);
            image.extend(file);
            image.resize(image.len().next_multiple_of(512), 0);
            image.extend(noise(100 + i as u64, 2 * 512));
        }
        (image, offsets)
    }

    fn found(fs: &dyn FileSystem) -> Vec<(Entry, Vec<u8>)> {
        let mut all = Vec::new();
        for group in fs.list(&fs.root()).unwrap() {
            for file in fs.list(&group.id).unwrap() {
                let data = datareco_fs::read_to_end(fs, &file.id, None, u64::MAX).unwrap();
                all.push((file, data));
            }
        }
        all
    }

    #[test]
    fn files_are_found_by_their_contents() {
        let files = vec![
            jpeg(1, 9000),
            png(2, 3000),
            wav(3, 5000),
            pdf("doc", 4000),
            jpeg(4, 300),
        ];
        let (image, offsets) = bare_disk(&files);
        let disk = disk(image);
        let volume = RawScan::run(&disk, default_signatures(), None).unwrap();
        assert_eq!(volume.info().kind, FsKind::Raw);
        let all = found(&volume);
        let names: Vec<&String> = all.iter().map(|(e, _)| &e.name).collect();
        assert_eq!(all.len(), files.len(), "{names:?}");
        let kinds = ["jpg", "png", "wav", "pdf", "jpg"];
        for ((file, offset), extension) in files.iter().zip(&offsets).zip(kinds) {
            let name = format!("f{:010}.{extension}", offset / 512);
            let (entry, data) = all
                .iter()
                .find(|(e, _)| e.name == name)
                .unwrap_or_else(|| panic!("no {name} in {names:?}"));
            assert_eq!(data, file, "{}", entry.name);
            assert!(
                entry
                    .note
                    .as_deref()
                    .unwrap()
                    .contains("Found by its contents")
            );
        }
        let groups: Vec<String> = volume
            .list(&volume.root())
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(groups, ["JPG", "PDF", "PNG", "WAV"]);
    }

    #[test]
    fn a_file_whose_end_is_unknown_is_cut_at_the_next_one() {
        let mut psd = b"8BPS\x00\x01".to_vec();
        psd.extend(noise(9, 2000));
        let (image, offsets) = bare_disk(&[psd, png(5, 1000)]);
        let disk = disk(image);
        let volume = RawScan::run(&disk, default_signatures(), None).unwrap();
        let all = found(&volume);
        let psd = all.iter().find(|(e, _)| e.name.ends_with(".psd")).unwrap();
        assert_eq!(psd.0.size, offsets[1] - offsets[0]);
        assert!(psd.0.note.as_deref().unwrap().contains("end wasn't found"));
    }

    #[test]
    fn custom_signatures_with_a_footer_or_a_size_field() {
        let list = parse_signatures(
            r#"
            [[signature]]
            name = "Footer"
            extension = "ftr"
            header = "53 54 41 52 54"
            footer = "45 4E 44 21"

            [[signature]]
            name = "Field"
            extension = "fld"
            header = "46 4C 44 21"
            size_field = { offset = 4, bytes = 2, endian = "big", add = 6 }
            "#,
        )
        .unwrap();
        let footer = [&b"START"[..], &noise(1, 700), b"END!"].concat();
        let mut field = b"FLD!\x01\x2C".to_vec();
        field.extend(noise(2, 300));
        let (image, _) = bare_disk(&[footer.clone(), field.clone()]);
        let volume = RawScan::run(&disk(image), list, None).unwrap();
        let all = found(&volume);
        let get = |extension: &str| {
            all.iter()
                .find(|(e, _)| e.name.ends_with(extension))
                .unwrap()
                .1
                .clone()
        };
        assert_eq!(get(".ftr"), footer);
        assert_eq!(get(".fld"), field);
    }

    #[test]
    fn files_come_back_after_an_ntfs_volume_was_quick_formatted() {
        // Like the ntfs-reformatted test image: the format destroyed the
        // MFT, but the files' contents are still on the disk.
        let mut b = NtfsBuilder::new(2048);
        // Data written early sits where the new FAT goes; these come after.
        b.alloc(40);
        let photos = b.mkdir(NTFS_ROOT, "Photos");
        let photo = png(11, 3 * NTFS_CLUSTER);
        let song = wav(12, 5 * NTFS_CLUSTER);
        b.file(photos, "beach.png", &photo);
        b.file(NTFS_ROOT, "song.wav", &song);
        let mut image = b.build();
        let fat = FatBuilder::new(FsKind::Fat32, image.len() / 512).build();
        image[..160 * 1024].copy_from_slice(&fat[..160 * 1024]);
        // Lose the MFT's second part too, as on the test image.
        let mft2 = (2048 - 16) * NTFS_CLUSTER;
        image[mft2..mft2 + 8 * NTFS_CLUSTER].fill(0);

        let volume = RawScan::run(&disk(image), default_signatures(), None).unwrap();
        let all = found(&volume);
        assert!(
            all.iter()
                .any(|(e, d)| e.name.ends_with(".png") && *d == photo)
        );
        assert!(
            all.iter()
                .any(|(e, d)| e.name.ends_with(".wav") && *d == song)
        );
    }

    #[test]
    fn a_raw_scan_can_be_saved_and_carried_on_and_recovered_from() {
        let files: Vec<Vec<u8>> = (0..6).map(|i| png(20 + i, 4000)).collect();
        let (image, _) = bare_disk(&files);
        let disk = disk(image);
        let expected: Vec<Vec<u8>> =
            found(&RawScan::run(&disk, default_signatures(), None).unwrap())
                .into_iter()
                .map(|(_, d)| d)
                .collect();
        assert_eq!(expected, files);

        let mut scan = RawScan::new(&disk, default_signatures(), None);
        scan.step(&disk, disk.size() / 2).unwrap();
        let saved = scan.save("image:x.img", &disk);
        let json = serde_json::to_string(&saved).unwrap();
        let saved: SavedRawScan = serde_json::from_str(&json).unwrap();
        let mut resumed = RawScan::restore(&disk, default_signatures(), &saved).unwrap();
        while !resumed.step(&disk, 4096).unwrap().done {}
        let volume = resumed.volume(Arc::clone(&disk));
        let again: Vec<Vec<u8>> = found(&volume).into_iter().map(|(_, d)| d).collect();
        assert_eq!(again, files);

        // Recovering everything puts each kind in its own folder.
        let destination = super::temp_folder("raw-recovered");
        let items = volume.list(&volume.root()).unwrap();
        let summary = recover(
            &volume,
            "raw",
            &items,
            &destination,
            RecoverOptions::default(),
            &mut |_| Control::Continue,
        )
        .unwrap();
        assert_eq!(summary.progress.files, 6);
        let saved = std::fs::read_dir(destination.join("PNG")).unwrap().count();
        assert_eq!(saved, 6);
        std::fs::remove_dir_all(destination).unwrap();
    }
}
