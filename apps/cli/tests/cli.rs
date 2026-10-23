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
