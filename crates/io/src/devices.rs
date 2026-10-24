//! Listing the disks and volumes attached to this computer.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::Result;
use crate::spec::SourceSpec;

/// The physical disks and volumes found on this computer.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceList {
    pub disks: Vec<PhysicalDiskInfo>,
    pub volumes: Vec<VolumeInfo>,
}

/// A physical disk (hard drive, SSD, USB stick, card reader with a card, …).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PhysicalDiskInfo {
    /// Disk number; open it with `SourceSpec::PhysicalDisk(number)`.
    pub number: u32,
    /// The OS device path, e.g. `\\.\PhysicalDrive0`.
    pub path: String,
    /// Vendor and model as the drive reports them.
    pub model: String,
    pub serial: Option<String>,
    /// How the disk is connected: "NVMe", "SATA", "USB", …
    pub bus: String,
    pub removable: bool,
    /// Size in bytes.
    pub size: u64,
    /// Logical sector size: the unit reads use.
    pub sector_size: u32,
    /// Physical sector size, when the drive reports it (e.g. 4096 on "512e" drives).
    pub physical_sector_size: Option<u32>,
    /// Firmware version, when the drive reports it.
    pub firmware: Option<String>,
    /// Whether it's a solid-state drive (no seek penalty), when Windows knows.
    pub solid_state: Option<bool>,
}

/// A volume (partition with a file system the OS knows about).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeInfo {
    /// The OS volume path, e.g. `\\?\Volume{…}`; open it with `SourceSpec::Volume`.
    pub path: String,
    /// Where it is mounted, e.g. `["C:\\"]`. Empty for volumes without a drive letter.
    pub mount_points: Vec<String>,
    pub label: Option<String>,
    /// File system name as the OS reports it ("NTFS", "FAT32", "exFAT", …).
    pub file_system: Option<String>,
    /// Size in bytes.
    pub size: Option<u64>,
    /// Free space in bytes, as the file system reports it (mounted volumes only).
    pub free: Option<u64>,
    /// Where the volume lives on physical disks.
    pub extents: Vec<DiskExtent>,
}

/// A stretch of a physical disk that belongs to a volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskExtent {
    pub disk: u32,
    /// Byte offset from the start of the disk.
    pub offset: u64,
    pub length: u64,
}

impl DeviceList {
    /// The physical disks a source lives on: the disk itself, or the disks
    /// under a volume. Image files live on none (see [`DeviceList::disks_of_path`]).
    pub fn disks_of_source(&self, source: &SourceSpec) -> Vec<u32> {
        match source {
            SourceSpec::PhysicalDisk(n) => vec![*n],
            SourceSpec::Volume(name) => self
                .volume(name)
                .map(|v| v.extents.iter().map(|e| e.disk).collect())
                .unwrap_or_default(),
            SourceSpec::Image(_) => Vec::new(),
        }
    }

    /// The volume a `SourceSpec::Volume` names: by its volume path or a
    /// mount point (`C:`).
    pub fn volume(&self, name: &str) -> Option<&VolumeInfo> {
        self.volumes.iter().find(|v| {
            v.path.eq_ignore_ascii_case(name)
                || v.mount_points
                    .iter()
                    .any(|m| m.trim_end_matches('\\').eq_ignore_ascii_case(name))
        })
    }

    /// The physical disks holding a file or folder, which need not exist yet.
    pub fn disks_of_path(&self, path: &Path) -> Vec<u32> {
        self.disks_of_full_path(&full_path(path).to_string_lossy())
    }

    /// The physical disks under the volume an absolute path is on: the
    /// volume with the longest mount point the path starts with.
    pub fn disks_of_full_path(&self, path: &str) -> Vec<u32> {
        let lower = path.to_lowercase();
        self.volumes
            .iter()
            .flat_map(|v| v.mount_points.iter().map(move |m| (m, v)))
            .filter(|(m, _)| lower.starts_with(&m.to_lowercase()))
            .max_by_key(|(m, _)| m.len())
            .map(|(_, v)| v.extents.iter().map(|e| e.disk).collect())
            .unwrap_or_default()
    }

    /// The physical disks Windows runs from (under the system drive).
    pub fn system_disks(&self) -> Vec<u32> {
        let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        self.disks_of_full_path(&format!("{}\\", drive.trim_end_matches('\\')))
    }
}

/// A path made absolute through its nearest existing folder, so it can be
/// matched against mount points; without Windows' `\\?\` prefix.
pub fn full_path(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (
            existing.file_name().map(ToOwned::to_owned),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                rest.push(name);
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut full = fs::canonicalize(&existing)
        .or_else(|_| std::path::absolute(&existing))
        .unwrap_or(existing);
    for name in rest.into_iter().rev() {
        full.push(name);
    }
    plain_path(full)
}

/// Whether two paths name the same file (which need not exist yet).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let (a, b) = (full_path(a), full_path(b));
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// A Windows path without its `\\?\` prefix (`\\?\UNC\` becomes `\\`).
pub fn plain_path(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => path.clone(),
    }
}

/// Lists physical disks and volumes. Needs no administrator rights; reading
/// them does. Returns empty lists on platforms not supported yet.
pub fn list_devices() -> Result<DeviceList> {
    crate::platform::list_devices()
}

/// Whether this process runs with administrator rights (elevated, on Windows).
pub fn is_elevated() -> bool {
    crate::platform::is_elevated()
}

/// Starts this program again with administrator rights and the given
/// command-line arguments. On Windows this shows the UAC prompt. The caller
/// should exit after it returns `Ok`.
pub fn relaunch_elevated(arguments: &[String]) -> Result<()> {
    crate::platform::relaunch_elevated(arguments)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn volume(path: &str, mounts: &[&str], disk: u32) -> VolumeInfo {
        VolumeInfo {
            path: path.to_owned(),
            mount_points: mounts.iter().map(|m| (*m).to_owned()).collect(),
            label: None,
            file_system: Some("NTFS".to_owned()),
            size: Some(1 << 30),
            free: None,
            extents: vec![DiskExtent {
                disk,
                offset: 1 << 20,
                length: 1 << 30,
            }],
        }
    }

    pub(crate) fn devices() -> DeviceList {
        DeviceList {
            disks: Vec::new(),
            volumes: vec![
                volume(r"\\?\Volume{c}", &["C:\\"], 0),
                volume(r"\\?\Volume{d}", &["D:\\"], 1),
                volume(r"\\?\Volume{m}", &["C:\\Mounted\\"], 2),
            ],
        }
    }

    #[test]
    fn finds_the_disks_of_paths_and_sources() {
        let devices = devices();
        assert_eq!(devices.disks_of_full_path(r"C:\Users\me\Recovered"), [0]);
        assert_eq!(devices.disks_of_full_path(r"c:\mounted\files"), [2]);
        assert_eq!(devices.disks_of_full_path(r"D:\"), [1]);
        assert!(devices.disks_of_full_path(r"\\server\share").is_empty());
        assert_eq!(devices.disks_of_source(&SourceSpec::PhysicalDisk(1)), [1]);
        assert_eq!(
            devices.disks_of_source(&SourceSpec::Volume("D:".to_owned())),
            [1]
        );
        assert!(
            devices
                .disks_of_source(&SourceSpec::Image(PathBuf::from("x.img")))
                .is_empty()
        );
    }

    #[test]
    fn full_paths_are_absolute_even_when_missing() {
        let path = full_path(Path::new("no-such-folder/inside/file.img"));
        assert!(path.is_absolute(), "{}", path.display());
        assert!(path.ends_with("no-such-folder/inside/file.img"));
        assert!(!path.to_string_lossy().starts_with(r"\\?\"));
    }
}
