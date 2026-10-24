//! Formatting volumes (FAT32 and exFAT, built here from Microsoft's
//! published specifications) and wiping disks and partitions.
//!
//! Nothing here opens a disk. A format is planned as a list of regions to
//! write ([`Formatting`]), which [`write_format`] writes to a target the
//! caller opened after the person confirmed; a [`Wipe`] writes a range of a
//! target a step at a time. Formatting is quick: only the file system's own
//! structures are written, so the old files' data stays where it was until
//! new files overwrite it.

mod exfat;
mod fat32;
mod place;
mod time;
mod upcase;
mod wipe;

use std::io;

use datareco_io::SectorTarget;
use serde::{Deserialize, Serialize};

pub use place::{Place, Spot, SpotInfo, locate};
pub use wipe::{Wipe, WipePattern, WipeProgress};

/// The file systems DataReco formats itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FormatKind {
    Fat32,
    Exfat,
}

impl FormatKind {
    pub fn name(self) -> &'static str {
        match self {
            FormatKind::Fat32 => "FAT32",
            FormatKind::Exfat => "exFAT",
        }
    }
}

/// How to format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatOptions {
    pub kind: FormatKind,
    /// The volume label: up to 11 characters.
    pub label: Option<String>,
    /// Bytes per cluster; the usual size for the volume's size when `None`.
    pub cluster_size: Option<u32>,
    /// The volume's serial number; made from the time when `None`.
    pub serial: Option<u32>,
    /// Where the volume starts on its disk, in sectors (the boot sector
    /// records it).
    pub partition_offset: u64,
}

/// What a format will make, for people.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatSummary {
    pub kind: FormatKind,
    pub label: Option<String>,
    /// Bytes per cluster.
    pub cluster_size: u32,
    pub clusters: u64,
    /// Bytes files can use.
    pub capacity: u64,
    pub serial: u32,
}

/// Something to write, at a byte offset from the volume's start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Region {
    Bytes { offset: u64, data: Vec<u8> },
    Zeros { offset: u64, len: u64 },
}

/// A planned format: what it makes, and what it writes.
#[derive(Clone, Debug)]
pub struct Formatting {
    pub summary: FormatSummary,
    pub(crate) regions: Vec<Region>,
    sector_size: u32,
}

impl Formatting {
    /// Bytes it writes.
    pub fn bytes_written(&self) -> u64 {
        self.regions
            .iter()
            .map(|r| match r {
                Region::Bytes { data, .. } => data.len() as u64,
                Region::Zeros { len, .. } => *len,
            })
            .sum()
    }
}

/// Works out a format of a `volume_bytes`-byte volume with `sector_size`
/// sectors, or says why it can't be done.
pub fn plan_format(
    volume_bytes: u64,
    sector_size: u32,
    options: &FormatOptions,
) -> Result<Formatting, String> {
    if !matches!(sector_size, 512 | 1024 | 2048 | 4096) {
        return Err(format!("{sector_size}-byte sectors aren't supported."));
    }
    let serial = options.serial.unwrap_or_else(time::serial);
    let (summary, regions) = match options.kind {
        FormatKind::Fat32 => fat32::plan(volume_bytes, sector_size, options, serial)?,
        FormatKind::Exfat => exfat::plan(volume_bytes, sector_size, options, serial)?,
    };
    Ok(Formatting {
        summary,
        regions,
        sector_size,
    })
}

/// Bytes of zeros written at a time.
const ZERO_CHUNK: usize = 1 << 20;

/// Writes a planned format to `target`, whose volume starts at byte
/// `volume_offset`, and flushes it.
pub fn write_format(
    formatting: &Formatting,
    target: &mut dyn SectorTarget,
    volume_offset: u64,
) -> io::Result<()> {
    if target.sector_size() != formatting.sector_size {
        return Err(io::Error::other(
            "the target's sector size differs from the plan's",
        ));
    }
    let zeros = vec![0u8; ZERO_CHUNK];
    for region in &formatting.regions {
        match region {
            Region::Bytes { offset, data } => target.write_at(volume_offset + offset, data)?,
            Region::Zeros { offset, len } => {
                let mut done = 0;
                while done < *len {
                    let n = (len - done).min(ZERO_CHUNK as u64);
                    let n = usize::try_from(n).unwrap_or(ZERO_CHUNK);
                    target.write_at(volume_offset + offset + done, &zeros[..n])?;
                    done += n as u64;
                }
            }
        }
    }
    target.flush()
}

/// Checks a label for FAT32 (11 characters of letters, digits and a few
/// marks, stored in capitals) or exFAT (11 UTF-16 characters).
pub fn check_label(kind: FormatKind, label: &str) -> Result<String, String> {
    let label = label.trim();
    match kind {
        FormatKind::Fat32 => {
            if label.chars().count() > 11 {
                return Err("A FAT32 label holds at most 11 characters.".into());
            }
            if let Some(bad) = label.chars().find(|&c| {
                !c.is_ascii() || c.is_ascii_control() || "*?.,;:/\\|+=<>[]\"".contains(c)
            }) {
                return Err(format!("A FAT32 label can't contain \"{bad}\"."));
            }
            Ok(label.to_ascii_uppercase())
        }
        FormatKind::Exfat => {
            if label.encode_utf16().count() > 11 {
                return Err("An exFAT label holds at most 11 characters.".into());
            }
            if let Some(bad) = label
                .chars()
                .find(|&c| c.is_control() || "*?/\\|<>:\"".contains(c))
            {
                return Err(format!("An exFAT label can't contain \"{bad}\"."));
            }
            Ok(label.to_owned())
        }
    }
}

/// The FAT/exFAT boot sector checksum step, as both specifications define
/// it: rotate right by one, then add the byte.
pub(crate) fn rotate_add(sum: u32, byte: u8) -> u32 {
    sum.rotate_right(1).wrapping_add(u32::from(byte))
}

#[cfg(test)]
mod tests;
