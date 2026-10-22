//! Platforms without device support yet (Linux and macOS come in Phase 10).
//! Image files still work everywhere.

use crate::devices::DeviceList;
use crate::disk::Disk;
use crate::error::{Error, Result};

pub(crate) fn open_physical_disk(_number: u32) -> Result<Disk> {
    Err(unsupported("Opening physical disks"))
}

pub(crate) fn open_volume(_path: &str) -> Result<Disk> {
    Err(unsupported("Opening volumes"))
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
