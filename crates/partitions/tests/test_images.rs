//! Partition tables and the lost-partition search against the generated test
//! disk images (testdata/README.md).
//!
//! The images live in `testdata/out` (or `$DATARECO_TESTDATA`). Without them
//! these tests pass with a note, unless `DATARECO_REQUIRE_TESTDATA=1`.

use std::path::{Path, PathBuf};

use datareco_io::{Disk, SourceSpec};
use datareco_partitions::{
    Confidence, FsKind, PartitionKind, Scan, ScanMode, Severity, TableKind, read_table,
};
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

fn image<'a>(images: &'a [Value], name: &str) -> &'a Value {
    images.iter().find(|image| image["name"] == name).unwrap()
}

fn open(dir: &Path, image: &Value) -> Disk {
    Disk::open(&SourceSpec::Image(
        dir.join(image["file"].as_str().unwrap()),
    ))
    .unwrap()
}

fn u64_of(value: &Value) -> u64 {
    value.as_u64().unwrap()
}

fn kind_of(fs: &str) -> FsKind {
    match fs {
        "fat12" => FsKind::Fat12,
        "fat16" => FsKind::Fat16,
        "fat32" => FsKind::Fat32,
        "exfat" => FsKind::Exfat,
        "ntfs" => FsKind::Ntfs,
        "ext4" => FsKind::Ext4,
        "hfsplus" => FsKind::HfsPlus,
        "btrfs" => FsKind::Btrfs,
        other => panic!("unexpected file system {other}"),
    }
}

/// Each volume in the manifest as (start LBA, sectors, file system).
fn volumes(image: &Value) -> Vec<(u64, u64, FsKind)> {
    image["volumes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                u64_of(&v["offset"]) / 512,
                u64_of(&v["size"]) / 512,
                kind_of(v["fs"].as_str().unwrap()),
            )
        })
        .collect()
}

#[test]
fn whole_disk_volumes_have_no_partition_table() {
    let Some((dir, images)) = images() else {
        return;
    };
    for name in [
        "fat12", "fat16", "fat32", "exfat", "ntfs", "ext4", "hfsplus", "btrfs",
    ] {
        // HFS+ and btrfs images are skipped where Linux can't make them.
        let Some(image) = images.iter().find(|i| i["name"] == name) else {
            continue;
        };
        let table = read_table(&open(&dir, image)).unwrap();
        assert_eq!(table.kind, TableKind::None, "{name}");
        let fs = table
            .whole_disk
            .unwrap_or_else(|| panic!("{name}: no file system recognized"));
        let volume = &image["volumes"][0];
        assert_eq!(fs.kind, kind_of(volume["fs"].as_str().unwrap()), "{name}");
        assert_eq!(fs.size, Some(u64_of(&volume["size"])), "{name}");
        if name.starts_with("fat") || name == "ext4" || name == "btrfs" {
            assert_eq!(fs.label.as_deref(), volume["label"].as_str(), "{name}");
        }
    }
}

#[test]
fn mbr_disk_matches_the_manifest() {
    let Some((dir, images)) = images() else {
        return;
    };
    let image = image(&images, "mbr");
    let table = read_table(&open(&dir, image)).unwrap();
    assert_eq!(table.kind, TableKind::Mbr);
    assert!(table.problems.is_empty(), "{:#?}", table.problems);
    let expected: Vec<(u64, u64, u64)> = image["partitions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                u64_of(&p["number"]),
                u64_of(&p["start_lba"]),
                u64_of(&p["sectors"]),
            )
        })
        .collect();
    let actual: Vec<(u64, u64, u64)> = table
        .partitions
        .iter()
        .map(|p| (u64::from(p.number), p.start_lba, p.sectors))
        .collect();
    assert_eq!(actual, expected);
    let found: Vec<(u64, u64, FsKind)> = table
        .partitions
        .iter()
        .filter(|p| p.kind != PartitionKind::Extended)
        .map(|p| (p.start_lba, p.sectors, p.file_system.as_ref().unwrap().kind))
        .collect();
    assert_eq!(found, volumes(image));
}

#[test]
fn gpt_disk_matches_the_manifest() {
    let Some((dir, images)) = images() else {
        return;
    };
    let image = image(&images, "gpt");
    let table = read_table(&open(&dir, image)).unwrap();
    assert_eq!(table.kind, TableKind::Gpt);
    assert!(table.problems.is_empty(), "{:#?}", table.problems);
    for (partition, expected) in table
        .partitions
        .iter()
        .zip(image["partitions"].as_array().unwrap())
    {
        assert_eq!(partition.start_lba, u64_of(&expected["start_lba"]));
        assert_eq!(partition.sectors, u64_of(&expected["sectors"]));
        assert_eq!(partition.name.as_deref(), expected["name"].as_str());
        assert_eq!(
            partition.type_code,
            expected["type"].as_str().unwrap().to_uppercase()
        );
    }
    assert_eq!(table.partitions.len(), 2);
}

/// Done-when for Phase 3: the partitions of `mbr-wiped` are found.
#[test]
fn lost_partitions_of_the_wiped_mbr_disk_are_found() {
    let Some((dir, images)) = images() else {
        return;
    };
    let image = image(&images, "mbr-wiped");
    let disk = open(&dir, image);
    let table = read_table(&disk).unwrap();
    assert_eq!(table.kind, TableKind::None);
    for mode in [ScanMode::Quick, ScanMode::Full] {
        let found: Vec<(u64, u64, FsKind)> = Scan::run(&disk, mode, Some(&table))
            .unwrap()
            .into_iter()
            .filter(|f| f.confidence >= Confidence::Medium)
            .map(|f| (f.start_lba, f.sectors, f.file_system.unwrap().kind))
            .collect();
        assert_eq!(found, volumes(image), "{mode:?} search");
    }
}

/// Done-when for Phase 3: `gpt-header-damaged` is read from its backup header.
#[test]
fn damaged_gpt_header_falls_back_to_the_backup() {
    let Some((dir, images)) = images() else {
        return;
    };
    let image = image(&images, "gpt-header-damaged");
    let table = read_table(&open(&dir, image)).unwrap();
    assert_eq!(table.kind, TableKind::Gpt);
    assert!(
        table
            .problems
            .iter()
            .any(|p| p.severity == Severity::Error && p.message.contains("primary GPT header")),
        "{:#?}",
        table.problems
    );
    let found: Vec<(u64, u64, FsKind)> = table
        .partitions
        .iter()
        .map(|p| (p.start_lba, p.sectors, p.file_system.as_ref().unwrap().kind))
        .collect();
    assert_eq!(found, volumes(image));
}

#[test]
fn reformatted_ntfs_volume_is_found_under_the_new_fat32() {
    let Some((dir, images)) = images() else {
        return;
    };
    let image = image(&images, "ntfs-reformatted");
    let disk = open(&dir, image);
    let table = read_table(&disk).unwrap();
    assert_eq!(
        table.whole_disk.as_ref().unwrap().kind,
        FsKind::Fat32,
        "the new file system"
    );
    let found = Scan::run(&disk, ScanMode::Full, Some(&table)).unwrap();
    assert!(
        found.iter().any(|f| f.start_lba == 0
            && f.file_system
                .as_ref()
                .is_some_and(|fs| fs.kind == FsKind::Ntfs)
            && f.evidence.iter().any(|e| e.contains("backup boot sector"))),
        "{found:#?}"
    );
}
