//! Listing the disks and volumes attached to this computer.

use serde::Serialize;

use crate::error::Result;

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
