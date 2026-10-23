//! Scanning and recovery.
//!
//! - [`Scan`]: a quick scan (the partition table and where volumes usually
//!   start) or a full scan (every sector) that finds volumes, including
//!   ones whose boot sector, partition entry or MFT start is gone, and
//!   ranks them. Scans go a step at a time, so they can be paused, saved
//!   ([`SavedScan`]) and carried on later.
//! - [`recover`]: copies files and folders out of a volume to a folder on
//!   another disk, with their times, a log and SHA-256 checksums.
//!
//! Raw recovery by file signature comes in Phase 6 (see docs/ROADMAP.md).

mod recover;
mod scan;
mod volumes;

#[cfg(test)]
mod tests;

pub use datareco_partitions::ScanMode;
pub use recover::{
    Control, RecoverError, RecoverOptions, RecoverProgress, RecoverSummary, check_destination,
    is_system_file, recover,
};
pub use scan::{SavedScan, Scan, ScanError, ScanProgress};
pub use volumes::{FoundVolume, VolumeState, open_found};
