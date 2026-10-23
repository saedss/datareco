//! Scanning and recovery against the generated test disk images
//! (testdata/README.md).
//!
//! The images live in `testdata/out` (or `$DATARECO_TESTDATA`). Without them
//! these tests pass with a note, unless `DATARECO_REQUIRE_TESTDATA=1`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use datareco_fs::{FileSystem, FsKind, read_to_end};
use datareco_io::{Disk, SourceSpec};
use datareco_scan::raw::{RawScan, default_signatures};
use datareco_scan::{Control, RecoverOptions, Scan, ScanMode, VolumeState, open_found, recover};
use serde_json::Value;
use sha2::{Digest, Sha256};

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

fn open(dir: &Path, images: &[Value], name: &str) -> (Arc<Disk>, Value) {
    let image = images.iter().find(|i| i["name"] == name).unwrap().clone();
    let path = dir.join(image["file"].as_str().unwrap());
    (
        Arc::new(Disk::open(&SourceSpec::Image(path)).unwrap()),
        image,
    )
}

fn sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Every file under `folder`, as (size, SHA-256).
fn saved_files(folder: &Path) -> HashSet<(u64, String)> {
    let mut files = HashSet::new();
    for entry in std::fs::read_dir(folder).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(saved_files(&path));
        } else {
            let data = std::fs::read(&path).unwrap();
            files.insert((data.len() as u64, sha256(&data)));
        }
    }
    files
}

#[test]
fn files_from_fat32_without_its_fat_are_recovered() {
    let Some((dir, images)) = images() else {
        return;
    };
    let (disk, image) = open(&dir, &images, "fat32-no-fat");
    let found = Scan::run(&disk, ScanMode::Full).unwrap();
    let volume = found
        .iter()
        .find(|v| v.kind == FsKind::Fat32)
        .unwrap_or_else(|| panic!("no FAT32 volume found: {found:#?}"));
    assert_eq!(volume.state, VolumeState::Rebuilt);
    assert_eq!(volume.offset, 0);
    assert_eq!(volume.label.as_deref(), Some("DR_FAT32"));

    let fs = open_found(Arc::clone(&disk), volume).unwrap();
    let items = fs.list(&fs.root()).unwrap();
    let destination =
        std::env::temp_dir().join(format!("datareco-fat32-no-fat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&destination);
    let summary = recover(
        fs.as_ref(),
        "fat32-no-fat",
        &items,
        &destination,
        RecoverOptions::default(),
        &mut |_| Control::Continue,
    )
    .unwrap();
    assert!(summary.progress.files > 0);
    let saved = saved_files(&destination);
    std::fs::remove_dir_all(&destination).unwrap();

    // Files in one piece come back whole (fragmented ones need the FAT).
    let volume = &image["volumes"][0];
    for file in volume["files"].as_array().unwrap() {
        if file["state"] != "live" || file["extents"].as_u64() != Some(1) {
            continue;
        }
        let expected = (
            file["size"].as_u64().unwrap(),
            file["sha256"].as_str().unwrap().to_owned(),
        );
        assert!(
            saved.contains(&expected),
            "{} wasn't recovered",
            file["path"]
        );
    }
}

#[test]
fn the_old_volume_is_found_after_a_quick_format() {
    let Some((dir, images)) = images() else {
        return;
    };
    let (disk, _) = open(&dir, &images, "ntfs-reformatted");
    let found = Scan::run(&disk, ScanMode::Full).unwrap();
    // The new, empty FAT32 volume, and the old NTFS one from its backup boot sector.
    let new = found.iter().find(|v| v.kind == FsKind::Fat32).unwrap();
    assert_eq!((new.offset, new.state), (0, VolumeState::Intact));
    let old = found
        .iter()
        .find(|v| v.kind == FsKind::Ntfs)
        .unwrap_or_else(|| panic!("the old NTFS volume wasn't found: {found:#?}"));
    assert_eq!((old.offset, old.state), (0, VolumeState::FromBackup));
    assert!(open_found(Arc::clone(&disk), old).is_ok());
}

/// Every file a raw scan of the image found, as (size, SHA-256).
fn found_by_contents(disk: &Arc<Disk>) -> HashSet<(u64, String)> {
    let volume = RawScan::run(disk, default_signatures(), None).unwrap();
    let mut files = HashSet::new();
    for group in volume.list(&volume.root()).unwrap() {
        for file in volume.list(&group.id).unwrap() {
            let data = read_to_end(&volume, &file.id, None, u64::MAX).unwrap();
            files.insert((data.len() as u64, sha256(&data)));
        }
    }
    files
}

/// The image's files a raw scan should find: the kinds with signatures,
/// stored in one piece. Deleted ones too: their contents are still there.
fn findable(image: &Value) -> Vec<(String, u64, String)> {
    image["volumes"][0]["files"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| {
            matches!(
                f["kind"].as_str(),
                Some("png" | "bmp" | "wav" | "zip" | "pdf")
            )
        })
        .filter(|f| f["extents"].as_u64() == Some(1))
        .map(|f| {
            (
                f["path"].as_str().unwrap().to_owned(),
                f["size"].as_u64().unwrap(),
                f["sha256"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn raw_recovery_finds_the_files_in_fat32_without_its_fat() {
    let Some((dir, images)) = images() else {
        return;
    };
    let (disk, image) = open(&dir, &images, "fat32-no-fat");
    let found = found_by_contents(&disk);
    let wanted = findable(&image);
    assert!(!wanted.is_empty());
    for (path, size, hash) in wanted {
        assert!(
            found.contains(&(size, hash)),
            "{path} wasn't found by its contents"
        );
    }
}

#[test]
fn raw_recovery_brings_back_files_after_the_ntfs_quick_format() {
    // The format overwrote the old volume's MFT, but not the files' contents.
    let Some((dir, images)) = images() else {
        return;
    };
    let (disk, image) = open(&dir, &images, "ntfs-reformatted");
    let found = found_by_contents(&disk);
    let wanted = findable(&image);
    assert!(!wanted.is_empty());
    for (path, size, hash) in wanted {
        assert!(
            found.contains(&(size, hash)),
            "{path} wasn't found by its contents"
        );
    }
}
