//! Where a format or wipe writes, and opening it once the person confirms.
//!
//! In an image file, straight into the file. On a disk, a partition that
//! Windows has a volume for is written through that volume alone (only it
//! is closed, so it may share a disk with Windows); anything else (a
//! partition Windows doesn't mount, or the whole disk) through the disk,
//! with all its volumes closed, which is refused for the disk Windows runs
//! from.

use std::path::PathBuf;

use datareco_io::{
    DeviceList, Disk, DiskTarget, DiskTargetCheck, Error, ImageTarget, Result, SectorTarget,
    SourceSpec, VolumeTarget, VolumeTargetCheck, check_disk_for_changes, check_volume_for_changes,
};
use serde::Serialize;

/// What a format or wipe writes to.
#[derive(Clone, Debug)]
pub enum Place {
    Image(PathBuf),
    Volume(VolumeTargetCheck),
    Disk(DiskTargetCheck),
}

/// A stretch of a source to format or wipe, where to write it, and what
/// the person types to confirm.
#[derive(Clone, Debug)]
pub struct Spot {
    pub place: Place,
    /// Where the stretch starts in the target, in bytes (0 for a volume).
    pub offset: u64,
    pub size: u64,
    pub sector_size: u32,
    /// Where it starts on its disk, in sectors (for boot sectors).
    pub start_lba: u64,
    /// What the person types to confirm.
    pub confirmation: String,
    /// The volumes closed while it's written, for people.
    pub closing: Vec<String>,
}

/// What a spot is, for people (sent to the desktop app).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpotInfo {
    pub size: u64,
    pub confirmation: String,
    pub closing: Vec<String>,
    /// The Windows volume it's written through, if any (`\\?\Volume{…}`).
    pub volume: Option<String>,
}

impl Spot {
    pub fn info(&self) -> SpotInfo {
        SpotInfo {
            size: self.size,
            confirmation: self.confirmation.clone(),
            closing: self.closing.clone(),
            volume: match &self.place {
                Place::Volume(check) => Some(check.path.clone()),
                _ => None,
            },
        }
    }

    /// Opens the target once `typed` is the confirmation.
    pub fn open(&self, typed: &str) -> Result<Box<dyn SectorTarget>> {
        match &self.place {
            Place::Image(path) => {
                if !typed.trim().eq_ignore_ascii_case(&self.confirmation) {
                    return Err(Error::Refused {
                        reason: format!(
                            "Type {} to confirm that what's there may be erased.",
                            self.confirmation
                        ),
                    });
                }
                let size = std::fs::metadata(path)
                    .map_err(|e| Error::Io {
                        name: self.confirmation.clone(),
                        source: e,
                    })?
                    .len();
                Ok(Box::new(ImageTarget::resume(path, size)?))
            }
            Place::Volume(check) => Ok(Box::new(VolumeTarget::open(check, typed)?)),
            Place::Disk(check) => Ok(Box::new(DiskTarget::open(check, typed)?)),
        }
    }
}

/// Where to write sectors `start_lba..start_lba + sectors` of `spec` (a
/// partition), or all of it with `None`.
pub fn locate(
    spec: &SourceSpec,
    disk: &Disk,
    range: Option<(u64, u64)>,
    devices: Option<&DeviceList>,
) -> Result<Spot> {
    let sector = u64::from(disk.sector_size());
    let (start_lba, sectors) = range.unwrap_or((0, disk.sector_count()));
    let offset = start_lba.saturating_mul(sector);
    let size = sectors.saturating_mul(sector);
    let spot = |place, offset, confirmation: String, closing| Spot {
        place,
        offset,
        size,
        sector_size: disk.sector_size(),
        start_lba,
        confirmation,
        closing,
    };
    match spec {
        SourceSpec::Image(path) => Ok(spot(
            Place::Image(path.clone()),
            offset,
            spec.display_name(),
            Vec::new(),
        )),
        SourceSpec::Volume(_) => Err(Error::Refused {
            reason: "Pick the disk this volume is on, and the partition on it.".into(),
        }),
        SourceSpec::PhysicalDisk(number) => {
            let devices = devices.ok_or_else(|| Error::Refused {
                reason: "The disks on this computer can't be listed.".into(),
            })?;
            let volume = range.and_then(|_| {
                devices.volumes.iter().find(|v| {
                    v.extents
                        .iter()
                        .any(|e| e.disk == *number && e.offset == offset)
                })
            });
            match volume {
                Some(volume) => {
                    let check = check_volume_for_changes(&volume.path, devices)?;
                    let closing = vec![check.name.clone()];
                    Ok(spot(
                        Place::Volume(check.clone()),
                        0,
                        check.confirmation,
                        closing,
                    ))
                }
                None => {
                    let check = check_disk_for_changes(*number, devices)?;
                    let closing = check.volumes.clone();
                    Ok(spot(
                        Place::Disk(check.clone()),
                        offset,
                        check.confirmation,
                        closing,
                    ))
                }
            }
        }
    }
}
