use std::path::PathBuf;
use std::process::{Command, Output};

fn datareco(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_datareco"))
        .args(args)
        .output()
        .expect("failed to run datareco")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// A two-sector image whose second sector starts with "HELLO".
fn hello_image(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("datareco-cli-{}-{name}", std::process::id()));
    let mut bytes = vec![0u8; 1024];
    bytes[512..517].copy_from_slice(b"HELLO");
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn version_flag_prints_name_and_version() {
    let out = datareco(&["--version"]);
    assert!(out.status.success());
    assert_eq!(
        stdout(&out).trim(),
        format!("datareco {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_shows_help() {
    let out = datareco(&[]);
    let text = stderr(&out) + &stdout(&out);
    assert!(
        text.contains("Usage: datareco"),
        "unexpected output: {text}"
    );
}

#[test]
fn read_shows_a_sector_of_an_image_as_hex() {
    let image = hello_image("read.img");
    let out = datareco(&["read", image.to_str().unwrap(), "--sector", "1"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("2 sectors of 512 bytes"), "{text}");
    assert!(text.contains("Sector 1"), "{text}");
    assert!(
        text.contains(
            "000000000200  48 45 4c 4c 4f 00 00 00  00 00 00 00 00 00 00 00  |HELLO...........|"
        ),
        "{text}"
    );
    std::fs::remove_file(image).unwrap();
}

#[test]
fn reading_past_the_end_fails_with_a_clear_message() {
    let image = hello_image("past-end.img");
    let out = datareco(&["read", image.to_str().unwrap(), "--sector", "2"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("only 1024 bytes long"),
        "{}",
        stderr(&out)
    );
    std::fs::remove_file(image).unwrap();
}

#[test]
fn reading_a_missing_image_says_so() {
    let out = datareco(&["read", "no-such-image.img"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no-such-image.img was not found"),
        "{}",
        stderr(&out)
    );
}

/// A 4-sector image whose sector 0 is an MBR with one FAT32 partition, and
/// with "hello" in sector 2.
fn mbr_image(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("datareco-cli-{}-{name}", std::process::id()));
    let mut bytes = vec![0u8; 2048];
    bytes[446] = 0x80;
    bytes[450] = 0x0C;
    bytes[454..458].copy_from_slice(&2048u32.to_le_bytes());
    bytes[458..462].copy_from_slice(&98_304u32.to_le_bytes());
    bytes[510] = 0x55;
    bytes[511] = 0xAA;
    bytes[1029..1034].copy_from_slice(b"hello");
    std::fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn templates_lists_the_builtin_templates() {
    let out = datareco(&["templates"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Partition tables"), "{text}");
    assert!(text.contains("ntfs_mft_record"), "{text}");
}

#[test]
fn decode_shows_an_mbr() {
    let image = mbr_image("decode.img");
    for template in ["mbr", "auto"] {
        let out = datareco(&["decode", image.to_str().unwrap(), template]);
        assert!(out.status.success(), "{}", stderr(&out));
        let text = stdout(&out);
        assert!(
            text.starts_with("Master Boot Record (MBR) at offset 0x0 (sector 0), 512 bytes"),
            "{text}"
        );
        assert!(text.contains("[0]: FAT32 (LBA), 2048 + 98304"), "{text}");
        assert!(text.contains("0x1be"), "{text}");
        assert!(
            text.contains("Boot signature: 0xAA55  (valid (55 AA))"),
            "{text}"
        );
    }
    let out = datareco(&["decode", image.to_str().unwrap(), "auto", "--sector", "1"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("no template recognizes"),
        "{}",
        stderr(&out)
    );
    let out = datareco(&["decode", image.to_str().unwrap(), "nonsense"]);
    assert!(
        stderr(&out).contains("no template named \"nonsense\""),
        "{}",
        stderr(&out)
    );
    std::fs::remove_file(image).unwrap();
}

#[test]
fn find_locates_text_and_bytes() {
    let image = mbr_image("find.img");
    let path = image.to_str().unwrap();
    let out = datareco(&["find", path, "--text", "HELLO", "--ignore-case"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "0x405  (byte 1029, sector 2 + 5)");

    let out = datareco(&["find", path, "--hex", "55 AA"]);
    assert!(stdout(&out).starts_with("0x1fe"), "{}", stdout(&out));

    let out = datareco(&["find", path, "--text", "hello", "--from", "0x406"]);
    assert_eq!(stdout(&out).trim(), "Not found.");

    let out = datareco(&["find", path, "--hex", "5"]);
    assert!(!out.status.success());
    std::fs::remove_file(image).unwrap();
}

/// An 8 MiB disk with one 4 MiB NTFS partition at sector 2048 (boot sector,
/// an MFT record where the boot sector says, and the backup boot sector).
/// With `wiped`, sector 0 (the partition table) is zeros.
fn ntfs_disk(name: &str, wiped: bool) -> PathBuf {
    let path = std::env::temp_dir().join(format!("datareco-cli-{}-{name}", std::process::id()));
    let mut disk = vec![0u8; 16_384 * 512];
    let mut boot = vec![0u8; 512];
    boot[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
    boot[3..11].copy_from_slice(b"NTFS    ");
    boot[11..13].copy_from_slice(&512u16.to_le_bytes());
    boot[13] = 8;
    boot[40..48].copy_from_slice(&8191u64.to_le_bytes());
    boot[48..56].copy_from_slice(&4u64.to_le_bytes());
    boot[56..64].copy_from_slice(&8u64.to_le_bytes());
    boot[510..512].copy_from_slice(&[0x55, 0xAA]);
    for sector in [2048, 2048 + 8191] {
        disk[sector * 512..sector * 512 + 512].copy_from_slice(&boot);
    }
    disk[(2048 + 32) * 512..(2048 + 32) * 512 + 4].copy_from_slice(b"FILE");
    if !wiped {
        disk[446 + 4] = 0x07;
        disk[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
        disk[446 + 12..446 + 16].copy_from_slice(&8192u32.to_le_bytes());
        disk[510..512].copy_from_slice(&[0x55, 0xAA]);
    }
    std::fs::write(&path, disk).unwrap();
    path
}

#[test]
fn partitions_shows_the_table() {
    let image = ntfs_disk("table.img", false);
    let out = datareco(&["partitions", image.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("Partition table: MBR"), "{text}");
    assert!(text.contains("primary"), "{text}");
    assert!(text.contains("NTFS, exFAT or HPFS"), "{text}");
    assert!(text.contains("4.0 MiB"), "{text}");
    assert!(!text.contains("Problems"), "{text}");
    std::fs::remove_file(image).unwrap();
}

#[test]
fn find_partitions_finds_a_lost_partition() {
    let image = ntfs_disk("wiped.img", true);
    let out = datareco(&["partitions", image.to_str().unwrap()]);
    assert!(
        stdout(&out).contains("Sector 0 is empty"),
        "{}",
        stdout(&out)
    );

    let out = datareco(&["find-partitions", image.to_str().unwrap(), "--full"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.starts_with("Found 1 partition(s) (full search)"),
        "{text}"
    );
    assert!(
        text.contains("2048") && text.contains("8192") && text.contains("NTFS"),
        "{text}"
    );
    assert!(
        text.contains("high"),
        "boot sector and its backup agree: {text}"
    );
    assert!(
        text.contains("NTFS backup boot sector at LBA 10239"),
        "{text}"
    );
    std::fs::remove_file(image).unwrap();
}

#[test]
fn disks_lists_devices_as_json() {
    let out = datareco(&["disks", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let devices: serde_json::Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(devices["disks"].is_array());
    assert!(devices["volumes"].is_array());
}

fn temp_image(name: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("datareco-cli-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// A FAT32 volume (no partition table) with a folder and a deleted file.
fn fat_volume(name: &str) -> PathBuf {
    use datareco_fs::FsKind;
    use datareco_fs::testutil::{FatBuilder, text};
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    b.label(b"CLI TEST   ");
    let root = b.root();
    b.file(root, "readme.txt", b"hello from a FAT volume\n");
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 3000));
    let old = b.file(root, "old.txt", b"deleted, not gone\n");
    b.delete(&old);
    temp_image(name, &b.build())
}

/// An MBR disk with an NTFS volume at sector 2048; one file has a stream.
fn ntfs_partition(name: &str) -> PathBuf {
    use datareco_fs::testutil::{NTFS_ROOT, NtfsBuilder, noise};
    let mut b = NtfsBuilder::new(2048);
    b.label("CLI NTFS");
    let photo = b.file(NTFS_ROOT, "photo.jpg", &noise(1, 9000));
    b.stream(photo, "Zone.Identifier", b"[ZoneTransfer]\r\nZoneId=3\r\n");
    let volume = b.build();
    let mut disk = vec![0u8; 2048 * 512];
    disk[446 + 4] = 0x07;
    disk[446 + 8..446 + 12].copy_from_slice(&2048u32.to_le_bytes());
    let sectors = u32::try_from(volume.len() / 512).unwrap();
    disk[446 + 12..446 + 16].copy_from_slice(&sectors.to_le_bytes());
    disk[510..512].copy_from_slice(&[0x55, 0xAA]);
    disk.extend_from_slice(&volume);
    temp_image(name, &disk)
}

#[test]
fn ls_lists_files_and_deleted_ones() {
    let image = fat_volume("ls.img");
    let out = datareco(&["ls", image.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("FAT32 \"CLI TEST\""), "{text}");
    assert!(
        text.contains("2024-01-15 10:30    24 bytes           readme.txt"),
        "{text}"
    );
    assert!(text.contains("cluster size 512 bytes"), "{text}");
    assert!(text.contains("docs/"), "{text}");
    assert!(text.contains("deleted  old.txt"), "{text}");
    assert!(
        text.contains("2 file(s), 1 folder(s); 1 deleted."),
        "{text}"
    );

    let out = datareco(&["ls", image.to_str().unwrap(), "--recursive", "--deleted"]);
    let text = stdout(&out);
    assert!(
        text.contains("old.txt") && !text.contains("readme.txt"),
        "{text}"
    );

    let out = datareco(&["ls", image.to_str().unwrap(), "-r"]);
    assert!(stdout(&out).contains("docs/notes.txt"), "{}", stdout(&out));

    let out = datareco(&["ls", image.to_str().unwrap(), "readme.txt"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("is a file"), "{}", stderr(&out));
    std::fs::remove_file(image).unwrap();
}

#[test]
fn cat_prints_live_and_deleted_files() {
    let image = fat_volume("cat.img");
    let out = datareco(&["cat", image.to_str().unwrap(), "readme.txt"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"hello from a FAT volume\n");

    let out = datareco(&["cat", image.to_str().unwrap(), r"DOCS\Notes.txt"]);
    assert_eq!(out.stdout, datareco_fs::testutil::text("notes", 3000));

    let out = datareco(&["cat", image.to_str().unwrap(), "old.txt"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"deleted, not gone\n");
    assert!(
        stderr(&out).starts_with("note: Deleted."),
        "{}",
        stderr(&out)
    );

    let out = datareco(&["cat", image.to_str().unwrap(), "missing.txt"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("missing.txt was not found"),
        "{}",
        stderr(&out)
    );
    std::fs::remove_file(image).unwrap();
}

#[test]
fn ls_and_cat_find_the_volume_in_a_partition() {
    let image = ntfs_partition("ntfs-files.img");
    let path = image.to_str().unwrap();
    let out = datareco(&["ls", path]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("NTFS \"CLI NTFS\""), "{text}");
    assert!(
        text.contains("photo.jpg  [streams: Zone.Identifier]"),
        "{text}"
    );

    let out = datareco(&[
        "cat",
        path,
        "photo.jpg",
        "--partition",
        "1",
        "--stream",
        "Zone.Identifier",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"[ZoneTransfer]\r\nZoneId=3\r\n");

    let out = datareco(&["cat", path, "photo.jpg", "--offset", "0x100000"]);
    assert_eq!(out.stdout, datareco_fs::testutil::noise(1, 9000));

    let out = datareco(&["ls", path, "--partition", "2"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("There's no partition 2"),
        "{}",
        stderr(&out)
    );
    std::fs::remove_file(image).unwrap();
}

/// A FAT32 volume whose boot sector, backup and FATs were wiped.
fn fat_without_fat(name: &str) -> PathBuf {
    use datareco_fs::FsKind;
    use datareco_fs::testutil::{FatBuilder, text};
    let mut b = FatBuilder::new(FsKind::Fat32, 70_000);
    b.label(b"LOST FAT   ");
    let root = b.root();
    b.file(root, "readme.txt", b"still here\n");
    let (docs, _) = b.mkdir(root, "docs");
    b.file(docs, "notes.txt", &text("notes", 3000));
    let data_start = b.data_start();
    let mut image = b.build();
    image[..data_start].fill(0);
    temp_image(name, &image)
}

fn temp_folder(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("datareco-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

#[test]
fn scan_finds_a_volume_without_its_fat_and_recover_saves_it() {
    let image = fat_without_fat("nofat.img");
    let image_path = image.to_str().unwrap();
    let saved = temp_folder("nofat-scan.json");
    let out = datareco(&[
        "scan",
        image_path,
        "--full",
        "--save",
        saved.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Found 1 volume(s) (full scan)"), "{text}");
    assert!(text.contains("FAT32 \"LOST FAT\""), "{text}");
    assert!(text.contains("rebuilt from the folders found"), "{text}");
    assert!(text.contains("its FAT is gone"), "{text}");
    assert!(saved.exists());

    // Its volumes can be used like partitions.
    let scan_args = ["--scan", saved.to_str().unwrap(), "--volume", "1"];
    let out = datareco(&[&["ls", image_path, "-r"][..], &scan_args[..]].concat());
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("docs/notes.txt"), "{}", stdout(&out));

    let destination = temp_folder("nofat-recovered");
    let out = datareco(
        &[
            &["recover", image_path, "--to", destination.to_str().unwrap()][..],
            &scan_args[..],
        ]
        .concat(),
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Saved 2 files and 1 folder ("),
        "{}",
        stdout(&out)
    );
    assert_eq!(
        std::fs::read(destination.join("readme.txt")).unwrap(),
        b"still here\n"
    );
    assert_eq!(
        std::fs::read(destination.join("docs").join("notes.txt")).unwrap(),
        datareco_fs::testutil::text("notes", 3000)
    );
    let logs = std::fs::read_dir(&destination)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".log")
        })
        .count();
    assert_eq!(logs, 1);

    // A volume number the scan doesn't have.
    let out = datareco(&[
        "ls",
        image_path,
        "--scan",
        saved.to_str().unwrap(),
        "--volume",
        "7",
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no volume 7"), "{}", stderr(&out));
    std::fs::remove_dir_all(destination).unwrap();
    std::fs::remove_file(saved).unwrap();
    std::fs::remove_file(image).unwrap();
}

#[test]
fn recover_saves_the_paths_asked_for() {
    let image = fat_volume("recover-paths.img");
    let destination = temp_folder("recover-paths");
    let out = datareco(&[
        "recover",
        image.to_str().unwrap(),
        "docs/notes.txt",
        "old.txt",
        "--to",
        destination.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(destination.join("notes.txt")).unwrap(),
        datareco_fs::testutil::text("notes", 3000)
    );
    assert_eq!(
        std::fs::read(destination.join("old.txt")).unwrap(),
        b"deleted, not gone\n"
    );
    assert!(!destination.join("readme.txt").exists());

    let out = datareco(&[
        "recover",
        image.to_str().unwrap(),
        "nope.txt",
        "--to",
        destination.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("nope.txt was not found"),
        "{}",
        stderr(&out)
    );
    std::fs::remove_dir_all(destination).unwrap();
    std::fs::remove_file(image).unwrap();
}

/// A disk with no file system: a PNG and a BMP, each at a sector's start,
/// with noise around them.
fn files_without_a_file_system(name: &str) -> (PathBuf, Vec<u8>, Vec<u8>) {
    use datareco_fs::testutil::noise;
    let chunk = |kind: &[u8], data: &[u8]| {
        let len = u32::try_from(data.len()).unwrap().to_be_bytes();
        [&len[..], kind, data, &[0, 0, 0, 0]].concat()
    };
    let png = [
        &b"\x89PNG\r\n\x1a\n"[..],
        &chunk(b"IHDR", &[0, 0, 0, 8, 0, 0, 0, 8, 8, 2, 0, 0, 0]),
        &chunk(b"IDAT", &noise(1, 2000)),
        &chunk(b"IEND", &[]),
    ]
    .concat();
    let mut bmp = vec![0u8; 54];
    bmp[..2].copy_from_slice(b"BM");
    bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp.extend(noise(2, 3000));
    let size = u32::try_from(bmp.len()).unwrap();
    bmp[2..6].copy_from_slice(&size.to_le_bytes());

    let mut image = noise(3, 1024);
    for file in [&png, &bmp] {
        image.extend(file);
        image.resize(image.len().next_multiple_of(512), 0);
        image.extend(noise(4, 1024));
    }
    (temp_image(name, &image), png, bmp)
}

#[test]
fn raw_finds_files_by_their_contents_and_saves_them() {
    let (image, png, bmp) = files_without_a_file_system("raw.img");
    let image_path = image.to_str().unwrap();
    let saved = temp_folder("raw-scan.json");
    let destination = temp_folder("raw-saved");
    let out = datareco(&[
        "raw",
        image_path,
        "--save",
        saved.to_str().unwrap(),
        "--to",
        destination.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Found 2 files by their contents"), "{text}");
    assert!(text.contains("Saved 2 files and 2 folders"), "{text}");
    let only = |folder: &str| {
        let entries: Vec<_> = std::fs::read_dir(destination.join(folder))
            .unwrap()
            .collect();
        assert_eq!(entries.len(), 1, "{folder}");
        std::fs::read(entries[0].as_ref().unwrap().path()).unwrap()
    };
    assert_eq!(only("PNG"), png);
    assert_eq!(only("BMP"), bmp);

    // What the saved scan found can be listed like a volume.
    let out = datareco(&["ls", image_path, "--raw", saved.to_str().unwrap(), "-r"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("PNG/f0000000002.png"), "{text}");
    assert!(text.contains("BMP/"), "{text}");
    std::fs::remove_dir_all(destination).unwrap();
    std::fs::remove_file(saved).unwrap();
    std::fs::remove_file(image).unwrap();
}

#[test]
fn signatures_can_be_listed_written_and_used() {
    let out = datareco(&["signatures"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("JPEG image"), "{}", stdout(&out));
    assert!(stdout(&out).contains("FF D8 FF"), "{}", stdout(&out));

    let list = temp_folder("signatures.toml");
    let out = datareco(&["signatures", "--write", list.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let again = datareco(&["signatures", "--write", list.to_str().unwrap()]);
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );

    // Only BMPs now: the PNG isn't looked for.
    std::fs::write(
        &list,
        "[[signature]]\nname = \"BMP\"\nextension = \"bmp\"\nheader = \"42 4D\"\nsize = \"bmp\"\n",
    )
    .unwrap();
    let (image, _, _) = files_without_a_file_system("raw-custom.img");
    let out = datareco(&[
        "raw",
        image.to_str().unwrap(),
        "--signatures",
        list.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("Found 1 file by their contents"),
        "{}",
        stdout(&out)
    );

    std::fs::write(
        &list,
        "[[signature]]\nname = \"Bad\"\nextension = \"x\"\nheader = \"ZZ\"\n",
    )
    .unwrap();
    let out = datareco(&[
        "raw",
        image.to_str().unwrap(),
        "--signatures",
        list.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("signature 1 (\"Bad\")"),
        "{}",
        stderr(&out)
    );
    std::fs::remove_file(list).unwrap();
    std::fs::remove_file(image).unwrap();
}

/// Bytes that differ from sector to sector, ending partway through a sector.
fn copy_source(name: &str) -> (PathBuf, Vec<u8>) {
    let bytes: Vec<u8> = (0..300_100u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    (temp_image(name, &bytes), bytes)
}

fn removed(path: PathBuf) -> PathBuf {
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn image_copies_a_source_and_maps_it() {
    let (source, bytes) = copy_source("image-src.img");
    let image =
        removed(std::env::temp_dir().join(format!("datareco-cli-{}-copy.img", std::process::id())));
    let map = image.with_extension("map");
    let _ = std::fs::remove_file(&map);
    let out = datareco(&[
        "image",
        source.to_str().unwrap(),
        "--to",
        image.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Copied all"), "{}", stdout(&out));
    assert_eq!(std::fs::read(&image).unwrap(), bytes);
    let text = std::fs::read_to_string(&map).unwrap();
    assert!(text.contains("0x00000000  0x00049444  +"), "{text}");

    // Running it again finds the copy finished.
    let out = datareco(&[
        "image",
        source.to_str().unwrap(),
        "--to",
        image.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("already finished"),
        "{}",
        stdout(&out)
    );

    // A file that is there already isn't replaced without its map.
    std::fs::remove_file(&map).unwrap();
    let out = datareco(&[
        "image",
        source.to_str().unwrap(),
        "--to",
        image.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("already exists"), "{}", stderr(&out));
    for path in [source, image] {
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn image_carries_on_a_stopped_copy_from_its_map() {
    let (source, bytes) = copy_source("resume-src.img");
    // A copy stopped after 0x10000 bytes, with a stretch that failed to read.
    let mut partial = bytes.clone();
    partial[0x10000..].fill(0);
    let image = temp_image("resume-copy.img", &partial);
    let map = temp_image(
        "resume-copy.map",
        b"# current_pos  current_status  current_pass\n\
          0x00010000     ?               1\n\
          0x00000000  0x00008000  +\n\
          0x00008000  0x00001000  *\n\
          0x00009000  0x00007000  +\n\
          0x00010000  0x00039444  ?\n",
    );
    let out = datareco(&[
        "image",
        source.to_str().unwrap(),
        "--to",
        image.to_str().unwrap(),
        "--map",
        map.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("Carrying on"), "{}", stdout(&out));
    assert_eq!(std::fs::read(&image).unwrap(), bytes);
    let text = std::fs::read_to_string(&map).unwrap();
    assert!(text.contains("0x00000000  0x00049444  +"), "{text}");
    for path in [source, image, map] {
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn clone_only_writes_to_disks_it_can_check() {
    let (source, _) = copy_source("clone-src.img");
    let out = datareco(&["clone", source.to_str().unwrap(), "--to", "copy.img"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("must be a disk"), "{}", stderr(&out));
    let out = datareco(&[
        "clone",
        source.to_str().unwrap(),
        "--to",
        "disk99",
        "--erase",
        "disk99",
    ]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("Disk 99 was not found"),
        "{}",
        stderr(&out)
    );
    std::fs::remove_file(source).unwrap();
}

/// An ext4 volume with a folder, and a file deleted after the journal
/// recorded it.
fn ext4_volume(name: &str) -> PathBuf {
    use datareco_fs::testutil::{EXT_ROOT, ExtBuilder, ExtOptions, text};
    let mut b = ExtBuilder::new(ExtOptions::ext4());
    b.label("cli_ext4");
    b.file(
        EXT_ROOT,
        "readme.txt",
        b"hello from ext4
",
    );
    let docs = b.mkdir(EXT_ROOT, "docs");
    b.file(docs, "notes.txt", &text("notes", 5000));
    let old = b.file(EXT_ROOT, "old.txt", &text("old", 3000));
    b.commit();
    b.delete(old);
    temp_image(name, &b.build())
}

#[test]
fn ext4_volumes_list_and_recover_deleted_files_too() {
    let image = ext4_volume("ext4.img");
    let out = datareco(&["ls", image.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("ext4 \"cli_ext4\""), "{text}");
    assert!(text.contains("block size 4.0 KiB"), "{text}");
    assert!(text.contains("docs/"), "{text}");
    assert!(text.contains("deleted  old.txt"), "{text}");

    let out = datareco(&["cat", image.to_str().unwrap(), "readme.txt"]);
    assert_eq!(
        out.stdout,
        b"hello from ext4
"
    );

    let destination = temp_folder("ext4-recovered");
    let out = datareco(&[
        "recover",
        image.to_str().unwrap(),
        "--to",
        destination.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(destination.join("docs").join("notes.txt")).unwrap(),
        datareco_fs::testutil::text("notes", 5000)
    );
    assert_eq!(
        std::fs::read(destination.join("old.txt")).unwrap(),
        datareco_fs::testutil::text("old", 3000)
    );
    std::fs::remove_dir_all(destination).unwrap();
    std::fs::remove_file(image).unwrap();
}

/// An HFS+ volume with a resource fork and a deleted file left in the catalog.
fn hfs_volume(name: &str) -> PathBuf {
    use datareco_fs::testutil::{HFS_ROOT, HfsBuilder, HfsOptions, text};
    let mut b = HfsBuilder::new(HfsOptions::default());
    b.label("Mac Disk");
    let readme = b.file(HFS_ROOT, "readme.txt", b"hello from HFS+\n");
    b.resource(readme, b"resource fork bytes");
    let docs = b.mkdir(HFS_ROOT, "docs");
    b.file(docs, "notes.txt", &text("notes", 5000));
    let old = b.file(HFS_ROOT, "old.txt", &text("old", 3000));
    b.delete(old);
    temp_image(name, &b.build())
}

#[test]
fn hfs_plus_volumes_list_read_forks_and_recover() {
    let image = hfs_volume("hfsplus.img");
    let out = datareco(&["ls", image.to_str().unwrap()]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.starts_with("HFS+ \"Mac Disk\""), "{text}");
    assert!(text.contains("block size 4.0 KiB"), "{text}");
    assert!(text.contains("deleted  old.txt"), "{text}");

    let out = datareco(&[
        "cat",
        image.to_str().unwrap(),
        "readme.txt",
        "--stream",
        "rsrc",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(out.stdout, b"resource fork bytes");

    let destination = temp_folder("hfs-recovered");
    let out = datareco(&[
        "recover",
        image.to_str().unwrap(),
        "--to",
        destination.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        std::fs::read(destination.join("docs").join("notes.txt")).unwrap(),
        datareco_fs::testutil::text("notes", 5000)
    );
    assert_eq!(
        std::fs::read(destination.join("old.txt")).unwrap(),
        datareco_fs::testutil::text("old", 3000)
    );
    std::fs::remove_dir_all(destination).unwrap();
    std::fs::remove_file(image).unwrap();
}
