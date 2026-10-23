//! The built-in templates against the generated test disk images
//! (testdata/README.md): every structure must decode to what the manifest
//! says was written.
//!
//! The images live in `testdata/out` (or `$DATARECO_TESTDATA`). Without them
//! these tests pass with a note, unless `DATARECO_REQUIRE_TESTDATA=1`.

use std::path::{Path, PathBuf};

use datareco_io::{Disk, SourceSpec};
use datareco_templates::{Node, TemplateSet};
use serde_json::Value;

fn images() -> Option<(PathBuf, Vec<Value>)> {
    let dir = std::env::var_os("DATARECO_TESTDATA").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/out"),
        PathBuf::from,
    );
    let Ok(text) = std::fs::read_to_string(dir.join("manifest.json")) else {
        assert!(
            std::env::var("DATARECO_REQUIRE_TESTDATA").as_deref() != Ok("1"),
            "test images are required but missing from {}",
            dir.display()
        );
        eprintln!(
            "skipped: no test images in {} (see testdata/README.md)",
            dir.display()
        );
        return None;
    };
    let manifest: Value = serde_json::from_str(&text).unwrap();
    Some((dir, manifest["images"].as_array().unwrap().clone()))
}

fn open(dir: &Path, image: &Value) -> Disk {
    Disk::open(&SourceSpec::Image(
        dir.join(image["file"].as_str().unwrap()),
    ))
    .unwrap()
}

/// Decodes `template` with the bytes at `offset`.
fn decode(disk: &Disk, template: &str, offset: u64) -> Node {
    let set = TemplateSet::builtin();
    let extent = set.get(template).unwrap().extent;
    let len = extent.min(disk.size() - offset);
    let mut data = vec![0; usize::try_from(len).unwrap()];
    disk.read_at(offset, &mut data).unwrap();
    set.decode(template, &data, offset).unwrap()
}

fn detect(disk: &Disk, offset: u64) -> Vec<String> {
    let mut data = vec![0; 4096];
    disk.read_at(offset, &mut data).unwrap();
    TemplateSet::builtin()
        .detect(&data)
        .into_iter()
        .map(|t| t.id)
        .collect()
}

fn at<'a>(node: &'a Node, path: &[&str]) -> &'a Node {
    path.iter().fold(node, |node, name| {
        node.children
            .iter()
            .find(|child| child.name == *name)
            .unwrap_or_else(|| panic!("no \"{name}\" in {}", node.name))
    })
}

/// A decimal field as a number.
fn num(node: &Node, path: &[&str]) -> u64 {
    let field = at(node, path);
    field
        .value
        .parse()
        .unwrap_or_else(|_| panic!("{} = {} is not a number", field.name, field.value))
}

/// A text field without its quotes and padding.
fn text(node: &Node, path: &[&str]) -> String {
    at(node, path).value.trim_matches('"').trim_end().to_owned()
}

fn u64_of(value: &Value) -> u64 {
    value.as_u64().unwrap()
}

fn undamaged(images: &[Value]) -> impl Iterator<Item = &Value> {
    images
        .iter()
        .filter(|image| image["damage"].as_array().unwrap().is_empty())
}

fn image<'a>(images: &'a [Value], name: &str) -> &'a Value {
    images.iter().find(|image| image["name"] == name).unwrap()
}

#[test]
fn mbr_partition_table_matches_the_manifest() {
    let Some((dir, images)) = images() else {
        return;
    };
    let image = image(&images, "mbr");
    let mbr = decode(&open(&dir, image), "mbr", 0);
    for partition in image["partitions"].as_array().unwrap() {
        let number = u64_of(&partition["number"]);
        if number > 4 {
            continue; // logical partitions live in EBRs (Phase 3)
        }
        let entry = at(&mbr, &["Partitions", &format!("[{}]", number - 1)]);
        let expected_type = u8::from_str_radix(partition["type"].as_str().unwrap(), 16).unwrap();
        assert_eq!(at(entry, &["Type"]).value, format!("0x{expected_type:02X}"));
        assert_eq!(
            num(entry, &["First sector (LBA)"]),
            u64_of(&partition["start_lba"])
        );
        assert_eq!(num(entry, &["Sector count"]), u64_of(&partition["sectors"]));
    }
}

#[test]
fn gpt_headers_and_entries_match_the_manifest() {
    let Some((dir, images)) = images() else {
        return;
    };
    let image = image(&images, "gpt");
    let disk = open(&dir, image);
    let header = decode(&disk, "gpt_header", 512);
    assert_eq!(text(&header, &["Signature"]), "EFI PART");
    let entries_lba = num(&header, &["Partition entries LBA"]);
    let entries = decode(&disk, "gpt_entries", entries_lba * 512);
    for (i, partition) in image["partitions"].as_array().unwrap().iter().enumerate() {
        let entry = at(&entries, &["Entries", &format!("[{i}]")]);
        let start = u64_of(&partition["start_lba"]);
        assert_eq!(num(entry, &["First LBA"]), start);
        assert_eq!(
            num(entry, &["Last LBA"]),
            start + u64_of(&partition["sectors"]) - 1
        );
        assert_eq!(text(entry, &["Name"]), partition["name"].as_str().unwrap());
        assert_eq!(
            at(entry, &["Type"]).value,
            partition["type"].as_str().unwrap().to_uppercase()
        );
    }

    // With the primary header destroyed, the backup in the last sector is intact.
    let damaged = open(&dir, self::image(&images, "gpt-header-damaged"));
    assert!(!detect(&damaged, 512).contains(&"gpt_header".to_owned()));
    let last = damaged.sector_count() - 1;
    let backup = decode(&damaged, "gpt_header", last * 512);
    assert_eq!(text(&backup, &["Signature"]), "EFI PART");
    assert_eq!(num(&backup, &["This header's LBA"]), last);
    assert_eq!(num(&backup, &["Other header's LBA"]), 1);
}

#[test]
fn every_volume_boot_sector_is_detected_and_matches_the_manifest() {
    let Some((dir, images)) = images() else {
        return;
    };
    let mut checked = 0;
    for image in undamaged(&images) {
        let disk = open(&dir, image);
        for volume in image["volumes"].as_array().unwrap() {
            let fs = volume["fs"].as_str().unwrap();
            let offset = u64_of(&volume["offset"]);
            let size = u64_of(&volume["size"]);
            let label = volume["label"].as_str().unwrap();
            let context = format!("{} {fs}", image["name"]);
            let (template, at_offset) = match fs {
                "fat12" | "fat16" => ("fat_boot_sector", offset),
                "fat32" => ("fat32_boot_sector", offset),
                "exfat" => ("exfat_boot_sector", offset),
                "ntfs" => ("ntfs_boot_sector", offset),
                "ext4" => ("ext4_superblock", offset + 1024),
                other => panic!("unexpected file system {other}"),
            };
            assert_eq!(
                detect(&disk, at_offset).first().map(String::as_str),
                Some(template),
                "{context}"
            );
            let boot = decode(&disk, template, at_offset);
            match fs {
                "fat12" | "fat16" | "fat32" => {
                    assert_eq!(text(&boot, &["Volume label"]), label, "{context}");
                    assert_eq!(num(&boot, &["Hidden sectors"]), offset / 512, "{context}");
                    let total = if fs == "fat32" {
                        num(&boot, &["Total sectors"])
                    } else {
                        num(&boot, &["Total sectors (16-bit)"])
                            .max(num(&boot, &["Total sectors (32-bit)"]))
                    };
                    assert_eq!(total, size / 512, "{context}");
                }
                "exfat" => assert_eq!(
                    num(&boot, &["Volume length (sectors)"]),
                    size / 512,
                    "{context}"
                ),
                "ntfs" => {
                    // The backup boot sector in the last sector isn't counted.
                    assert_eq!(num(&boot, &["Total sectors"]), size / 512 - 1, "{context}");
                    assert_eq!(num(&boot, &["Hidden sectors"]), offset / 512, "{context}");
                }
                "ext4" => {
                    assert_eq!(text(&boot, &["Volume name"]), label, "{context}");
                    assert_eq!(
                        at(&boot, &["Block size"]).note.as_deref(),
                        Some("4096 bytes")
                    );
                    assert_eq!(
                        num(&boot, &["Block count (low 32 bits)"]),
                        size / 4096,
                        "{context}"
                    );
                }
                _ => unreachable!(),
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 11, "volumes checked");
}

#[test]
fn ntfs_mft_record_zero_describes_the_mft() {
    let Some((dir, images)) = images() else {
        return;
    };
    for image in undamaged(&images) {
        let disk = open(&dir, image);
        for volume in image["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["fs"] == "ntfs")
        {
            let offset = u64_of(&volume["offset"]);
            let boot = decode(&disk, "ntfs_boot_sector", offset);
            let cluster = num(&boot, &["Bytes per sector"]) * num(&boot, &["Sectors per cluster"]);
            let mft = offset + num(&boot, &["MFT cluster"]) * cluster;
            let record = decode(&disk, "ntfs_mft_record", mft);
            assert_eq!(text(&record, &["Signature"]), "FILE");
            assert!(
                at(&record, &["Flags"])
                    .note
                    .as_deref()
                    .unwrap()
                    .contains("in use")
            );
            let file_name = at(&record, &["Attributes"])
                .children
                .iter()
                .find(|attribute| attribute.value == "$FILE_NAME")
                .expect("record 0 has a $FILE_NAME attribute");
            assert_eq!(at(file_name, &["Value"]).value, "$MFT");
        }
    }
}

/// Finds readme.txt's entries in a decoded directory and checks its size.
fn assert_readme_listed(entries: &Node, expected_size: u64, context: &str) {
    let summaries: Vec<&str> = entries.children.iter().map(|e| e.value.as_str()).collect();
    assert!(
        summaries.contains(&"readme.txt"),
        "{context}: {summaries:?}"
    );
    let short = entries
        .children
        .iter()
        .find(|e| e.value == "README.TXT")
        .unwrap_or_else(|| panic!("{context}: no README.TXT in {summaries:?}"));
    assert_eq!(num(short, &["File size"]), expected_size, "{context}");
    // The generator sets every file's time; readme.txt is the first file:
    // 2024-01-15 10:30:00 UTC.
    assert_eq!(
        at(short, &["Modified date"]).value,
        "2024-01-15",
        "{context}"
    );
    assert_eq!(at(short, &["Modified time"]).value, "10:30:00", "{context}");
}

fn readme_size(volume: &Value) -> u64 {
    let readme = volume["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["path"] == "readme.txt")
        .unwrap();
    u64_of(&readme["size"])
}

#[test]
fn fat_root_directories_list_readme() {
    let Some((dir, images)) = images() else {
        return;
    };
    let mut checked = 0;
    for image in undamaged(&images) {
        let disk = open(&dir, image);
        for volume in image["volumes"].as_array().unwrap() {
            let fs = volume["fs"].as_str().unwrap();
            let offset = u64_of(&volume["offset"]);
            let root_sector = match fs {
                "fat12" | "fat16" => {
                    let boot = decode(&disk, "fat_boot_sector", offset);
                    num(&boot, &["Reserved sectors"])
                        + num(&boot, &["Number of FATs"]) * num(&boot, &["Sectors per FAT"])
                }
                "fat32" => {
                    let boot = decode(&disk, "fat32_boot_sector", offset);
                    let data_start = num(&boot, &["Reserved sectors"])
                        + num(&boot, &["Number of FATs"]) * num(&boot, &["Sectors per FAT"]);
                    data_start
                        + (num(&boot, &["Root directory cluster"]) - 2)
                            * num(&boot, &["Sectors per cluster"])
                }
                _ => continue,
            };
            let entries = decode(&disk, "fat_dir_entries", offset + root_sector * 512);
            let context = format!("{} {fs}", image["name"]);
            assert_readme_listed(at(&entries, &["Entries"]), readme_size(volume), &context);
            checked += 1;
        }
    }
    assert_eq!(checked, 4, "FAT volumes checked");
}

#[test]
fn exfat_root_directories_list_readme() {
    let Some((dir, images)) = images() else {
        return;
    };
    let mut checked = 0;
    for image in undamaged(&images) {
        let disk = open(&dir, image);
        for volume in image["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["fs"] == "exfat")
        {
            let offset = u64_of(&volume["offset"]);
            let boot = decode(&disk, "exfat_boot_sector", offset);
            let sectors_per_cluster = 1u64 << num(&boot, &["Sectors per cluster (power of 2)"]);
            let root_sector = num(&boot, &["Cluster heap offset (sectors)"])
                + (num(&boot, &["Root directory cluster"]) - 2) * sectors_per_cluster;
            let entries = decode(&disk, "exfat_dir_entries", offset + root_sector * 512);
            let summaries: Vec<&str> = at(&entries, &["Entries"])
                .children
                .iter()
                .map(|e| e.value.as_str())
                .collect();
            let name_at = summaries
                .iter()
                .position(|s| *s == "Name: readme.txt")
                .unwrap_or_else(|| panic!("{}: {summaries:?}", image["name"]));
            // The stream entry, just before the name, holds the size.
            let size = readme_size(volume);
            assert!(
                summaries[name_at - 1].starts_with(&format!("Stream: {size} bytes")),
                "{summaries:?}"
            );
            assert!(
                summaries[name_at - 2].starts_with("File ("),
                "{summaries:?}"
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 2, "exFAT volumes checked");
}
