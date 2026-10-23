//! The built-in templates against hand-built structures.

use datareco_templates::{Node, TemplateSet};

fn templates() -> TemplateSet {
    let set = TemplateSet::builtin();
    assert!(set.problems().is_empty(), "{:#?}", set.problems());
    set
}

/// Follows child names, e.g. `["Partitions", "[0]", "Type"]`.
fn at<'a>(node: &'a Node, path: &[&str]) -> &'a Node {
    path.iter().fold(node, |node, name| {
        node.children
            .iter()
            .find(|child| child.name == *name)
            .unwrap_or_else(|| panic!("no \"{name}\" in {}: {:#?}", node.name, names(node)))
    })
}

fn names(node: &Node) -> Vec<&str> {
    node.children.iter().map(|c| c.name.as_str()).collect()
}

fn put(buf: &mut [u8], offset: usize, bytes: &[u8]) {
    buf[offset..offset + bytes.len()].copy_from_slice(bytes);
}

fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// 2024-01-15 10:30:00 UTC.
const FILETIME: u64 = 133_497_882_000_000_000;
const MS_BASIC_DATA: [u8; 16] = [
    0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7,
];

#[test]
fn builtin_templates_load_and_are_listed() {
    let set = templates();
    let listed: Vec<String> = set.list().into_iter().map(|t| t.id).collect();
    for id in [
        "mbr",
        "ebr",
        "gpt_header",
        "gpt_entries",
        "fat_boot_sector",
        "fat32_boot_sector",
        "fat_dir_entries",
        "exfat_boot_sector",
        "exfat_dir_entries",
        "ntfs_boot_sector",
        "ntfs_mft_record",
        "ext4_superblock",
    ] {
        assert!(listed.contains(&id.to_owned()), "{id} is not listed");
    }
    assert!(
        !listed.contains(&"fat_short_entry".to_owned()),
        "helpers stay hidden"
    );
}

#[test]
fn mbr_partition_table() {
    let mut sector = vec![0u8; 512];
    put(
        &mut sector,
        446,
        &[0x80, 0x20, 0x21, 0x00, 0x0C, 0xFE, 0xFF, 0xFF],
    );
    put(&mut sector, 454, &2048u32.to_le_bytes());
    put(&mut sector, 458, &98_304u32.to_le_bytes());
    put(&mut sector, 510, &[0x55, 0xAA]);
    let mbr = templates().decode("mbr", &sector, 0).unwrap();

    assert_eq!(at(&mbr, &["Partitions"]).children.len(), 4);
    let first = at(&mbr, &["Partitions", "[0]"]);
    assert_eq!(first.value, "FAT32 (LBA), 2048 + 98304");
    assert_eq!(first.offset, 446);
    assert_eq!(
        at(first, &["Status"]).note.as_deref(),
        Some("active (bootable)")
    );
    assert_eq!(at(first, &["Type"]).value, "0x0C");
    assert_eq!(
        at(first, &["First sector (CHS)"]).value,
        "cylinder 0, head 32, sector 33"
    );
    assert_eq!(at(first, &["First sector (LBA)"]).value, "2048");
    assert_eq!(
        at(&mbr, &["Partitions", "[1]", "Type"]).note.as_deref(),
        Some("empty")
    );
    assert_eq!(
        at(&mbr, &["Boot signature"]).note.as_deref(),
        Some("valid (55 AA)")
    );
}

#[test]
fn detection_prefers_the_most_specific_template() {
    let mut sector = vec![0u8; 512];
    put(&mut sector, 3, b"NTFS    ");
    put(&mut sector, 510, &[0x55, 0xAA]);
    let found: Vec<String> = templates()
        .detect(&sector)
        .into_iter()
        .map(|t| t.id)
        .collect();
    assert_eq!(found.first().map(String::as_str), Some("ntfs_boot_sector"));
    assert!(found.contains(&"mbr".to_owned()));
    assert!(templates().detect(&[0u8; 512]).is_empty());
}

#[test]
fn gpt_header_and_entry() {
    let mut header = vec![0u8; 512];
    put(&mut header, 0, b"EFI PART");
    put(&mut header, 8, &0x0001_0000u32.to_le_bytes());
    put(&mut header, 12, &92u32.to_le_bytes());
    put(&mut header, 24, &1u64.to_le_bytes());
    put(&mut header, 32, &196_607u64.to_le_bytes());
    put(&mut header, 56, &MS_BASIC_DATA);
    put(&mut header, 72, &2u64.to_le_bytes());
    put(&mut header, 80, &128u32.to_le_bytes());
    put(&mut header, 84, &128u32.to_le_bytes());
    let decoded = templates().decode("gpt_header", &header, 512).unwrap();
    assert_eq!(at(&decoded, &["Signature"]).value, "\"EFI PART\"");
    assert_eq!(at(&decoded, &["Revision"]).note.as_deref(), Some("1.0"));
    assert_eq!(at(&decoded, &["Other header's LBA"]).value, "196607");
    assert_eq!(at(&decoded, &["Partition entries LBA"]).value, "2");
    assert_eq!(
        at(&decoded, &["Disk GUID"]).offset,
        512 + 56,
        "offsets are absolute"
    );

    let mut entries = vec![0u8; 256];
    put(&mut entries, 0, &MS_BASIC_DATA);
    put(&mut entries, 32, &2048u64.to_le_bytes());
    put(&mut entries, 40, &67_583u64.to_le_bytes());
    put(&mut entries, 48, &(1u64 << 63).to_le_bytes());
    put(&mut entries, 56, &utf16("DataReco exFAT"));
    let decoded = templates().decode("gpt_entries", &entries, 1024).unwrap();
    let first = at(&decoded, &["Entries", "[0]"]);
    assert_eq!(first.value, "DataReco exFAT");
    assert_eq!(
        at(first, &["Type"]).value,
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7"
    );
    assert_eq!(
        at(first, &["Type"]).note.as_deref(),
        Some("Microsoft basic data")
    );
    assert_eq!(
        at(first, &["Attributes"]).note.as_deref(),
        Some("no drive letter (Microsoft)")
    );
    assert_eq!(
        at(&decoded, &["Entries", "[1]", "Type"]).note.as_deref(),
        Some("unused entry")
    );
}

#[test]
fn fat_directory_with_long_name_and_deleted_entry() {
    let mut dir = vec![0u8; 96];
    // Long-name entry for "readme.txt": last (and only) part.
    put(&mut dir, 0, &[0x41]);
    put(&mut dir, 1, &utf16("readm"));
    put(&mut dir, 11, &[0x0F, 0x00, 0x5A]);
    put(&mut dir, 14, &utf16("e.txt\0"));
    put(&mut dir, 28, &[0xFF, 0xFF, 0xFF, 0xFF]);
    // Its short-name entry.
    put(&mut dir, 32, b"README  TXT");
    put(&mut dir, 32 + 11, &[0x20, 0x18]);
    put(
        &mut dir,
        32 + 24,
        &((44u16 << 9) | (1 << 5) | 15).to_le_bytes(),
    );
    put(&mut dir, 32 + 26, &3u16.to_le_bytes());
    put(&mut dir, 32 + 28, &300u32.to_le_bytes());
    // A deleted entry.
    put(&mut dir, 64, b"\xE5LD     TXT");
    put(&mut dir, 64 + 11, &[0x20]);

    let decoded = templates().decode("fat_dir_entries", &dir, 0).unwrap();
    assert_eq!(at(&decoded, &["Entries", "[0]"]).value, "readme.txt");
    assert_eq!(
        at(&decoded, &["Entries", "[0]", "Attributes"])
            .note
            .as_deref(),
        Some("long-name entry")
    );
    let short = at(&decoded, &["Entries", "[1]"]);
    assert_eq!(short.value, "README.TXT");
    assert_eq!(at(short, &["File size"]).value, "300");
    assert_eq!(at(short, &["Attributes"]).note.as_deref(), Some("archive"));
    assert_eq!(at(short, &["Modified date"]).value, "2024-01-15");
    assert_eq!(
        at(&decoded, &["Entries", "[2]", "Status"]).note.as_deref(),
        Some("deleted")
    );
}

#[test]
fn exfat_file_entry_set() {
    let mut dir = vec![0u8; 128];
    put(&mut dir, 0, &[0x85, 0x02]);
    put(&mut dir, 4, &0x20u16.to_le_bytes());
    let modified =
        (u32::from((44u16 << 9) | (1 << 5) | 15) << 16) | u32::from((10u16 << 11) | (30 << 5));
    put(&mut dir, 12, &modified.to_le_bytes());
    put(&mut dir, 32, &[0xC0, 0x03, 0x00, 10]);
    put(&mut dir, 32 + 8, &300u64.to_le_bytes());
    put(&mut dir, 32 + 20, &5u32.to_le_bytes());
    put(&mut dir, 32 + 24, &300u64.to_le_bytes());
    put(&mut dir, 64, &[0xC1, 0x00]);
    put(&mut dir, 66, &utf16("readme.txt"));
    put(&mut dir, 96, &[0x05]); // a deleted file entry

    let decoded = templates().decode("exfat_dir_entries", &dir, 0).unwrap();
    let file = at(&decoded, &["Entries", "[0]"]);
    assert_eq!(file.value, "File (2 more entries)");
    assert_eq!(at(file, &["Modified"]).value, "2024-01-15 10:30:00");
    assert_eq!(at(file, &["Attributes"]).note.as_deref(), Some("archive"));
    assert_eq!(
        at(&decoded, &["Entries", "[1]"]).value,
        "Stream: 300 bytes from cluster 5"
    );
    assert_eq!(
        at(&decoded, &["Entries", "[1]", "Flags"]).note.as_deref(),
        Some("allocation possible, contiguous (no FAT chain)")
    );
    assert_eq!(at(&decoded, &["Entries", "[2]"]).value, "Name: readme.txt");
    assert_eq!(
        at(&decoded, &["Entries", "[3]", "Entry type"])
            .note
            .as_deref(),
        Some("file (deleted)")
    );
}

/// An MFT record for $MFT: standard information, file name, and a
/// non-resident $DATA attribute, then the end marker.
fn mft_record() -> Vec<u8> {
    let mut rec = vec![0u8; 1024];
    put(&mut rec, 0, b"FILE");
    put(&mut rec, 4, &[0x30, 0x00, 0x03, 0x00]);
    put(
        &mut rec,
        16,
        &[0x01, 0x00, 0x01, 0x00, 0x38, 0x00, 0x01, 0x00],
    );
    put(&mut rec, 28, &1024u32.to_le_bytes());

    // $STANDARD_INFORMATION, resident, 0x48-byte value at +0x18.
    let a = 0x38;
    put(&mut rec, a, &0x10u32.to_le_bytes());
    put(&mut rec, a + 4, &0x60u32.to_le_bytes());
    put(&mut rec, a + 10, &0x18u16.to_le_bytes());
    put(&mut rec, a + 16, &0x48u32.to_le_bytes());
    put(&mut rec, a + 20, &0x18u16.to_le_bytes());
    for i in 0..4 {
        put(&mut rec, a + 0x18 + 8 * i, &FILETIME.to_le_bytes());
    }
    put(&mut rec, a + 0x18 + 32, &0x06u32.to_le_bytes());

    // $FILE_NAME "$MFT", resident.
    let b = a + 0x60;
    put(&mut rec, b, &0x30u32.to_le_bytes());
    put(&mut rec, b + 4, &0x68u32.to_le_bytes());
    put(&mut rec, b + 10, &0x18u16.to_le_bytes());
    put(&mut rec, b + 14, &3u16.to_le_bytes());
    put(&mut rec, b + 16, &0x4Au32.to_le_bytes());
    put(&mut rec, b + 20, &0x18u16.to_le_bytes());
    let v = b + 0x18;
    put(&mut rec, v, &[5, 0, 0, 0, 0, 0, 5, 0]);
    put(&mut rec, v + 8, &FILETIME.to_le_bytes());
    put(&mut rec, v + 48, &16_384u64.to_le_bytes());
    put(&mut rec, v + 56, &0x06u32.to_le_bytes());
    put(&mut rec, v + 64, &[4, 3]);
    put(&mut rec, v + 66, &utf16("$MFT"));

    // $DATA, non-resident: 16 clusters at LCN 4.
    let c = b + 0x68;
    put(&mut rec, c, &0x80u32.to_le_bytes());
    put(&mut rec, c + 4, &0x48u32.to_le_bytes());
    put(&mut rec, c + 8, &[1, 0]);
    put(&mut rec, c + 10, &0x40u16.to_le_bytes());
    put(&mut rec, c + 24, &15u64.to_le_bytes());
    put(&mut rec, c + 32, &0x40u16.to_le_bytes());
    put(&mut rec, c + 48, &65_536u64.to_le_bytes());
    put(&mut rec, c + 0x40, &[0x11, 0x10, 0x04]);

    put(&mut rec, c + 0x48, &0xFFFF_FFFFu32.to_le_bytes());
    rec
}

#[test]
fn ntfs_mft_record_attributes() {
    let decoded = templates()
        .decode("ntfs_mft_record", &mft_record(), 0x4000)
        .unwrap();
    assert_eq!(at(&decoded, &["Flags"]).note.as_deref(), Some("in use"));
    let attributes = at(&decoded, &["Attributes"]);
    assert_eq!(attributes.value, "3 items", "{:#?}", names(attributes));

    let info = at(attributes, &["[0]"]);
    assert_eq!(info.value, "$STANDARD_INFORMATION");
    assert_eq!(info.offset, 0x4000 + 0x38);
    assert_eq!(
        at(info, &["Value", "Created"]).value,
        "2024-01-15 10:30:00 UTC"
    );
    assert_eq!(
        at(info, &["Value", "File attributes"]).note.as_deref(),
        Some("hidden, system")
    );

    let name = at(attributes, &["[1]"]);
    assert_eq!(name.value, "$FILE_NAME");
    assert_eq!(at(name, &["Value"]).value, "$MFT");
    assert_eq!(at(name, &["Value", "Parent directory record"]).value, "5");
    assert_eq!(
        at(name, &["Value", "Namespace"]).note.as_deref(),
        Some("Win32 and DOS")
    );
    assert_eq!(at(name, &["Name"]).value, "\"\"", "unnamed attribute");

    let data = at(attributes, &["[2]"]);
    assert_eq!(data.value, "$DATA");
    assert_eq!(at(data, &["Real size"]).value, "65536");
    assert_eq!(at(data, &["Data runs"]).value, "11 10 04 00 00 00 00 00");
}

#[test]
fn ext4_superblock() {
    let mut sb = vec![0u8; 1024];
    put(&mut sb, 0, &8192u32.to_le_bytes());
    put(&mut sb, 24, &2u32.to_le_bytes());
    put(&mut sb, 56, &[0x53, 0xEF, 0x01, 0x00]);
    put(&mut sb, 88, &256u16.to_le_bytes());
    put(&mut sb, 96, &0x2C2u32.to_le_bytes());
    put(&mut sb, 104, &[0x12; 16]);
    put(&mut sb, 120, b"dr_ext4");
    put(&mut sb, 264, &1_705_314_600u32.to_le_bytes());
    let set = templates();
    assert_eq!(set.detect(&sb)[0].id, "ext4_superblock");
    let decoded = set.decode("ext4_superblock", &sb, 1024).unwrap();
    assert_eq!(
        at(&decoded, &["Block size"]).note.as_deref(),
        Some("4096 bytes")
    );
    assert_eq!(at(&decoded, &["Magic"]).note.as_deref(), Some("ext2/3/4"));
    assert_eq!(
        at(&decoded, &["State"]).note.as_deref(),
        Some("cleanly unmounted")
    );
    assert_eq!(
        at(&decoded, &["Incompatible features"]).note.as_deref(),
        Some("filetype, extents, 64bit, flex_bg")
    );
    assert_eq!(at(&decoded, &["Volume name"]).value, "\"dr_ext4\"");
    assert_eq!(
        at(&decoded, &["UUID"]).value,
        "12121212-1212-1212-1212-121212121212"
    );
    assert_eq!(at(&decoded, &["Created"]).value, "2024-01-15 10:30:00 UTC");
    assert_eq!(at(&decoded, &["Last mounted"]).value, "not set");
}

#[test]
fn truncated_data_is_reported_not_fatal() {
    let decoded = templates().decode("mbr", &[0u8; 100], 0).unwrap();
    let signature = at(&decoded, &["Boot signature"]);
    assert!(signature.problem);
    assert_eq!(
        signature.note.as_deref(),
        Some("past the end of the bytes read")
    );
}

#[test]
fn malformed_attribute_chains_stop_safely() {
    let mut rec = mft_record();
    put(&mut rec, 0x38 + 4, &0u32.to_le_bytes()); // first attribute length 0
    let decoded = templates().decode("ntfs_mft_record", &rec, 0).unwrap();
    let attributes = at(&decoded, &["Attributes"]);
    assert!(attributes.children.last().unwrap().problem);

    let mut rec = mft_record();
    put(&mut rec, 20, &0xFFF0u16.to_le_bytes()); // first attribute far past the record
    let decoded = templates().decode("ntfs_mft_record", &rec, 0).unwrap();
    assert_eq!(at(&decoded, &["Attributes"]).value, "0 items");
}

/// Every template, over data that's empty, short, all zeros, all FF and
/// random: decoding must finish without panicking.
#[test]
fn no_template_panics_on_any_bytes() {
    let set = templates();
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut random = |len: usize| -> Vec<u8> {
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    };
    let mut inputs = vec![Vec::new(), vec![0x41; 7], vec![0; 512], vec![0xFF; 4096]];
    for len in [16, 33, 512, 1024, 4096, 16384] {
        inputs.push(random(len));
    }
    for template in set.templates() {
        for data in &inputs {
            let node = set.decode(&template.id, data, u64::MAX - 10).unwrap();
            assert_eq!(node.name, template.name);
        }
    }
}

#[test]
fn user_templates_add_to_and_override_the_builtin_ones() {
    let mut set = TemplateSet::builtin();
    let added = set.add_file(
        r#"
        [[template]]
        id = "my_header"
        name = "My header"
        group = "Mine"
        size = 4
        signature = [{ offset = 0, ascii = "MINE" }]

        [[template.field]]
        name = "Magic"
        offset = 0
        type = "ascii"
        length = 4
        "#,
        "mine.toml",
    );
    assert_eq!(added, 1);
    set.add_file("this is not toml [", "broken.toml");
    set.check();
    assert_eq!(set.problems().len(), 1);
    assert!(set.problems()[0].starts_with("broken.toml"));
    assert_eq!(set.detect(b"MINE")[0].id, "my_header");
    assert!(set.get("mbr").is_some(), "built-in templates stay");
}
