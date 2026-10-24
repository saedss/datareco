//! Platforms without device support yet (Linux and macOS come in Phase 11).
//! Image files still work everywhere.

use crate::devices::DeviceList;
use std::fs::File;
use std::io;

use crate::error::{Error, Result};
use crate::health::HealthData;
use crate::source::FileSource;
use crate::target::{OpenedDisk, OpenedVolume};

pub(crate) fn open_physical_disk(_number: u32) -> Result<FileSource> {
    Err(unsupported("Opening physical disks"))
}

pub(crate) fn open_volume(_path: &str) -> Result<FileSource> {
    Err(unsupported("Opening volumes"))
}

pub(crate) fn open_disk_for_writing(_number: u32, _volumes: &[String]) -> Result<OpenedDisk> {
    Err(unsupported("Writing to disks"))
}

pub(crate) fn update_disk_properties(_file: &File) {}

pub(crate) fn open_volume_for_writing(_path: &str) -> Result<OpenedVolume> {
    Err(unsupported("Writing to volumes"))
}

pub(crate) fn set_volume_label(_path: &str, _label: Option<&str>) -> Result<()> {
    Err(unsupported("Changing volume labels"))
}

pub(crate) fn set_drive_letter(_path: &str, _letter: Option<char>) -> Result<()> {
    Err(unsupported("Changing drive letters"))
}

pub(crate) fn partition_size_limits(_number: u32, _offset: u64) -> Result<(u64, u64)> {
    Err(unsupported("Resizing NTFS"))
}

pub(crate) fn resize_with_windows(_number: u32, _offset: u64, _size: u64) -> Result<()> {
    Err(unsupported("Resizing NTFS"))
}

pub(crate) fn format_with_windows(
    _path: &str,
    _file_system: &str,
    _label: Option<&str>,
    _quick: bool,
) -> Result<()> {
    Err(unsupported("Formatting NTFS"))
}

pub(crate) fn read_health_data(_number: u32) -> Result<HealthData> {
    Err(unsupported("Reading drive health"))
}

/// Files on Linux and macOS are sparse without asking.
pub(crate) fn make_sparse(_file: &File) -> io::Result<()> {
    Ok(())
}

pub(crate) fn is_device_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

pub(crate) fn list_devices() -> Result<DeviceList> {
    Ok(DeviceList::default())
}

pub(crate) fn is_elevated() -> bool {
    false
}

pub(crate) fn relaunch_elevated(_arguments: &[String]) -> Result<()> {
    Err(unsupported("Restarting as administrator"))
}

fn unsupported(what: &str) -> Error {
    Error::Unsupported {
        what: what.to_owned(),
    }
}
