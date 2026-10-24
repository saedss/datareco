//! Disk and image-file access: listing disks, reading sectors, caching, and
//! handling bad sectors (retry, skip, log).
//!
//! Reading never writes: opening a disk, volume or image to read it never
//! asks the OS for write access. Writing happens only through [`target`]:
//! new image files, image files a copy carries on into, whole disks that
//! passed [`check_disk_target`] or [`check_disk_for_changes`], and single
//! volumes that passed [`check_volume_for_changes`], each confirmed by
//! typing the disk's name; and through [`manage`], which asks Windows to
//! format a volume or change its label or drive letter. Closing volumes so
//! a copy reads them unchanged ([`close_volumes`]) opens them for writing
//! only to lock them, and [`Snapshot`] asks Windows for shadow copies.
//! The one exception is [`read_health_data`]: Windows only passes ATA
//! S.M.A.R.T. commands on through a handle opened for writing, so it opens
//! one, and sends only the read-only commands listed in its module.
//!
//! Start with [`SourceSpec`] to say what to open and [`Disk::open`] to open it.
//! [`Disk`] serves reads at any byte offset from a block cache, and records
//! sectors that could not be read instead of failing the whole read.
//!
//! This is the only crate allowed to use `unsafe`, for the OS calls that open
//! raw disks (`windows` module) and for aligned I/O buffers (`aligned` module).

#[allow(unsafe_code)]
mod aligned;
mod devices;
mod disk;
mod error;
mod health;
mod hold;
pub mod manage;
mod search;
mod source;
mod spec;
pub mod target;

#[cfg(not(windows))]
mod unsupported;
#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;

#[cfg(not(windows))]
use unsupported as platform;
#[cfg(windows)]
use windows as platform;

pub use devices::{
    DeviceList, DiskExtent, PhysicalDiskInfo, VolumeInfo, full_path, is_elevated, list_devices,
    plain_path, relaunch_elevated, same_path,
};
pub use disk::{BLOCK_SIZE, Disk, ReadOutcome, ReadPolicy};
pub use error::{Error, Result};
pub use health::{HEALTH_PAGE, HealthData, HealthPath, read_health_data};
pub use hold::{ClosedVolumes, Snapshot, close_volumes, volume_name};
pub use search::SearchProgress;
pub use source::{FileSource, MemorySource, SectorSource, is_device_gone};
pub use spec::SourceSpec;
pub use target::{
    CopySource, DiskTarget, DiskTargetCheck, ImageTarget, MemoryTarget, SectorTarget, VolumeTarget,
    VolumeTargetCheck, check_disk_for_changes, check_disk_target, check_outside_source,
    check_volume_for_changes,
};
