//! Disk imaging and cloning: disk to image, disk to disk, image to disk.
//!
//! A [`Copier`] reads a source onto a target in passes made for failing
//! drives (a fast first pass that skips past trouble, then backwards, then
//! the edges and insides of failed blocks, then retries), and records which
//! parts were copied in a [`Map`]. The map is saved in GNU ddrescue's
//! mapfile format, so a copy can stop at any time and carry on later, in
//! DataReco or in ddrescue.
//!
//! Sources and targets come from `datareco_io`: [`datareco_io::FileSource`]
//! reads without caching, [`datareco_io::ImageTarget`] writes image files and
//! [`datareco_io::DiskTarget`] writes whole disks after their safety checks.

mod copy;
mod map;

#[cfg(test)]
mod tests;

pub use copy::{Copier, CopyError, CopyOptions, CopyProgress};
pub use map::{Area, Map, MapError, Phase, Totals};
