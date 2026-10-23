//! Disk and image-file access: listing disks, reading sectors, caching, and
//! handling bad sectors (retry, skip, log).
//!
//! Everything here is read-only: no function in this crate asks the OS for
//! write access to a disk, volume or image.
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
mod search;
mod source;
mod spec;

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
    DeviceList, DiskExtent, PhysicalDiskInfo, VolumeInfo, is_elevated, list_devices,
    relaunch_elevated,
};
pub use disk::{BLOCK_SIZE, Disk, ReadOutcome, ReadPolicy};
pub use error::{Error, Result};
pub use search::SearchProgress;
pub use source::{FileSource, MemorySource, SectorSource};
pub use spec::SourceSpec;
