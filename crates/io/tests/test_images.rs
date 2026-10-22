//! Checks reads against the generated test disk images (testdata/README.md).
//!
//! The images live in `testdata/out` (or `$DATARECO_TESTDATA`). Without them
//! these tests pass with a note, unless `DATARECO_REQUIRE_TESTDATA=1` (set on
//! CI), which makes missing images a failure.

use std::path::PathBuf;

use datareco_io::{Disk, Error, SourceSpec};
use serde_json::Value;

fn testdata_dir() -> PathBuf {
    std::env::var_os("DATARECO_TESTDATA").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/out"),
        PathBuf::from,
    )
}

/// The manifest's image list, or `None` if the images haven't been built.
fn images() -> Option<(PathBuf, Vec<Value>)> {
    let dir = testdata_dir();
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
    assert_eq!(manifest["schema"], 1);
    Some((dir, manifest["images"].as_array().unwrap().clone()))
}

fn open(dir: &std::path::Path, image: &Value) -> Disk {
    let path = dir.join(image["file"].as_str().unwrap());
    Disk::open(&SourceSpec::Image(path)).unwrap()
}

fn u64_of(value: &Value) -> u64 {
    value.as_u64().unwrap()
}

#[test]
fn every_image_opens_with_the_right_size_and_reads_to_its_end() {
    let Some((dir, images)) = images() else {
        return;
    };
    assert_eq!(images.len(), 12);
    for image in &images {
        let disk = open(&dir, image);
        assert_eq!(disk.size(), u64_of(&image["size"]), "{}", image["name"]);
        assert_eq!(disk.sector_size(), 512);
        let last = disk.sector_count() - 1;
        let (_, outcome) = disk.read_sectors(last, 1).unwrap();
        assert!(outcome.unreadable.is_empty());
        assert!(matches!(
            disk.read_sectors(last + 1, 1),
            Err(Error::OutOfRange { .. })
        ));
    }
}

#[test]
fn partitioned_disks_start_with_a_boot_signature() {
    let Some((dir, images)) = images() else {
        return;
    };
    for image in &images {
        let name = image["name"].as_str().unwrap();
        let (sector0, _) = open(&dir, image).read_sectors(0, 1).unwrap();
        match name {
            "mbr" | "gpt" | "gpt-header-damaged" => {
                assert_eq!(sector0[510..], [0x55, 0xAA], "{name}")
            }
            "mbr-wiped" => assert!(
                sector0.iter().all(|&b| b == 0),
                "{name}: sector 0 should be wiped"
            ),
            _ => {}
        }
        if name == "gpt" {
            let (header, _) = open(&dir, image).read_sectors(1, 1).unwrap();
            assert_eq!(&header[..8], b"EFI PART");
        }
    }
}

/// Every volume's boot sector or superblock is where the manifest says.
#[test]
fn volumes_are_at_their_manifest_offsets() {
    let Some((dir, images)) = images() else {
        return;
    };
    let mut checked = 0;
    for image in images
        .iter()
        .filter(|image| image["damage"].as_array().unwrap().is_empty())
    {
        let disk = open(&dir, image);
        for volume in image["volumes"].as_array().unwrap() {
            let offset = u64_of(&volume["offset"]);
            let fs = volume["fs"].as_str().unwrap();
            let mut boot = vec![0; 2048];
            disk.read_at(offset, &mut boot).unwrap();
            let context = format!("{} {fs} at {offset}", image["name"]);
            match fs {
                "fat12" | "fat16" => {
                    assert_eq!(boot[510..512], [0x55, 0xAA], "{context}");
                    assert_eq!(&boot[54..59], fs.to_uppercase().as_bytes(), "{context}");
                }
                "fat32" => assert_eq!(&boot[82..87], b"FAT32", "{context}"),
                "exfat" => assert_eq!(&boot[3..11], b"EXFAT   ", "{context}"),
                "ntfs" => assert_eq!(&boot[3..11], b"NTFS    ", "{context}"),
                // The ext superblock starts 1024 bytes in; its magic is at byte 56.
                "ext4" => assert_eq!(boot[1024 + 56..1024 + 58], [0x53, 0xEF], "{context}"),
                other => panic!("unexpected file system {other}"),
            }
            checked += 1;
        }
    }
    assert_eq!(checked, 11, "volumes checked");
}
