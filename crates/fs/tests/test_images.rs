//! File-system reading against the generated test disk images
//! (testdata/README.md): every live file reads back with the right hash,
//! NTFS streams too, and deleted files are listed.
//!
//! The images live in `testdata/out` (or `$DATARECO_TESTDATA`). Without them
//! these tests pass with a note, unless `DATARECO_REQUIRE_TESTDATA=1`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use datareco_fs::{Entry, FileSystem, FsKind, open, read_to_end};
use datareco_io::{Disk, SourceSpec};
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

/// Every undamaged volume DataReco can read, opened, with its manifest entry.
fn volumes(dir: &Path, images: &[Value]) -> Vec<(String, Box<dyn FileSystem>, Value)> {
    let mut found = Vec::new();
    for image in images.iter().filter(|i| i["derived_from"].is_null()) {
        let path = dir.join(image["file"].as_str().unwrap());
        let disk = Arc::new(Disk::open(&SourceSpec::Image(path)).unwrap());
        for volume in image["volumes"].as_array().unwrap() {
            let kind = volume["fs"].as_str().unwrap();
            if !matches!(kind, "fat12" | "fat16" | "fat32" | "exfat" | "ntfs") {
                continue;
            }
            let name = format!("{}/{kind}", image["name"].as_str().unwrap());
            let offset = volume["offset"].as_u64().unwrap();
            let fs = open(Arc::clone(&disk), offset).unwrap_or_else(|e| panic!("{name}: {e}"));
            found.push((name, fs, volume.clone()));
        }
    }
    assert!(
        !found.is_empty(),
        "no FAT, exFAT or NTFS volumes in the test images"
    );
    found
}

fn sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether a listed name is the one wanted. A deleted FAT entry without a
/// long name has lost its first character.
fn same_name(found: &Entry, wanted: &str) -> bool {
    let (found_name, wanted) = (found.name.to_lowercase(), wanted.to_lowercase());
    found_name == wanted
        || (found.deleted
            && found_name.chars().count() == wanted.chars().count()
            && found_name.chars().skip(1).eq(wanted.chars().skip(1)))
}

/// Finds an entry by path, preferring deleted entries when `deleted`.
fn find(fs: &dyn FileSystem, path: &str, deleted: bool) -> Option<Entry> {
    let mut dir = fs.root();
    let mut found = None;
    for part in path.split('/') {
        let entry = fs
            .list(&dir)
            .ok()?
            .into_iter()
            .filter(|e| same_name(e, part))
            .min_by_key(|e| e.deleted != deleted)?;
        dir = entry.id.clone();
        found = Some(entry);
    }
    found
}

#[test]
fn live_files_read_back_with_the_right_hash() {
    let Some((dir, images)) = images() else {
        return;
    };
    for (name, fs, volume) in volumes(&dir, &images) {
        if volume["fs"] == "ntfs" || volume["fs"].as_str().unwrap().starts_with("fat") {
            assert_eq!(
                fs.info().label.as_deref(),
                volume["label"].as_str(),
                "{name}"
            );
        }
        assert_eq!(
            fs.info().kind == FsKind::Ntfs,
            volume["fs"] == "ntfs",
            "{name}"
        );
        for file in volume["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["state"] == "live")
        {
            let path = file["path"].as_str().unwrap();
            let entry = find(fs.as_ref(), path, false)
                .unwrap_or_else(|| panic!("{name}: {path} not found"));
            assert!(!entry.deleted, "{name}: {path} is listed as deleted");
            assert_eq!(entry.size, file["size"].as_u64().unwrap(), "{name}: {path}");
            let data = read_to_end(fs.as_ref(), &entry.id, None, u64::MAX)
                .unwrap_or_else(|e| panic!("{name}: reading {path}: {e}"));
            assert_eq!(sha256(&data), file["sha256"], "{name}: {path}");
            if volume["fs"] == "ntfs" {
                assert_eq!(entry.modified, file["mtime"].as_i64(), "{name}: {path}");
            }
        }
    }
}

#[test]
fn ntfs_streams_read_back() {
    let Some((dir, images)) = images() else {
        return;
    };
    for (name, fs, volume) in volumes(&dir, &images) {
        for stream in volume["streams"].as_array().unwrap() {
            let path = stream["file"].as_str().unwrap();
            let stream_name = stream["name"].as_str().unwrap();
            let entry = find(fs.as_ref(), path, false)
                .unwrap_or_else(|| panic!("{name}: {path} not found"));
            assert!(
                entry
                    .streams
                    .iter()
                    .any(|s| s.name == stream_name && s.size == stream["size"].as_u64().unwrap()),
                "{name}: {path} streams {:?}",
                entry.streams
            );
            let data = read_to_end(fs.as_ref(), &entry.id, Some(stream_name), u64::MAX).unwrap();
            assert_eq!(
                sha256(&data),
                stream["sha256"],
                "{name}: {path}:{stream_name}"
            );
        }
    }
}

#[test]
fn deleted_files_are_listed() {
    let Some((dir, images)) = images() else {
        return;
    };
    for (name, fs, volume) in volumes(&dir, &images) {
        for file in volume["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["state"] == "deleted")
        {
            let path = file["path"].as_str().unwrap();
            let entry = find(fs.as_ref(), path, true)
                .unwrap_or_else(|| panic!("{name}: deleted {path} not listed"));
            assert!(entry.deleted, "{name}: {path} isn't marked deleted");
            assert_eq!(entry.size, file["size"].as_u64().unwrap(), "{name}: {path}");
            // Nothing overwrote these, and a file in one piece is found whole.
            if file["extents"].as_u64() == Some(1) {
                let data = read_to_end(fs.as_ref(), &entry.id, None, u64::MAX)
                    .unwrap_or_else(|e| panic!("{name}: reading deleted {path}: {e}"));
                assert_eq!(sha256(&data), file["sha256"], "{name}: deleted {path}");
            }
        }
    }
}
