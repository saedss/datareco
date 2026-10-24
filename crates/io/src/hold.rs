//! Keeping what's being copied still: closing a disk's volumes so nothing
//! writes to them while they're read, or, for a volume Windows can't close
//! (the one it runs from), reading a snapshot of it instead.

use std::fs::File;

use crate::devices::VolumeInfo;
use crate::error::Result;
use crate::spec::SourceSpec;

/// Volumes closed to Windows and every program (locked and dismounted)
/// until this is dropped; then Windows mounts them again.
pub struct ClosedVolumes {
    _locks: Vec<File>,
    /// The volumes, as people know them ("E: (Photos)").
    pub names: Vec<String>,
}

/// Closes `volumes` (of the disk `disk` names, for messages) so the disk
/// can be read while nothing changes them. Refuses if a program has files
/// open on one. Needs administrator rights. Nothing is written.
pub fn close_volumes(volumes: &[&VolumeInfo], disk: &str) -> Result<ClosedVolumes> {
    let paths: Vec<String> = volumes.iter().map(|v| v.path.clone()).collect();
    let locks = crate::platform::close_volumes(&paths, disk)?;
    Ok(ClosedVolumes {
        _locks: locks,
        names: volumes.iter().map(|v| volume_name(v)).collect(),
    })
}

/// "E: (Photos)", or the volume's path when it has no drive letter.
pub fn volume_name(volume: &VolumeInfo) -> String {
    let place = volume.mount_points.first().map_or_else(
        || SourceSpec::Volume(volume.path.clone()).display_name(),
        |m| m.trim_end_matches('\\').to_owned(),
    );
    match &volume.label {
        Some(label) => format!("{place} ({label})"),
        None => place,
    }
}

/// A snapshot (shadow copy) Windows keeps of a volume as it was when it was
/// taken. Dropping it deletes it.
pub struct Snapshot {
    id: String,
    /// The device to read it from, like a volume:
    /// `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy7`.
    pub device: String,
    /// The volume it's of, as people know it.
    pub volume: String,
}

impl Snapshot {
    /// Asks Windows for a snapshot of a mounted NTFS or ReFS volume. Needs
    /// administrator rights and Windows' Volume Shadow Copy service; takes
    /// a few seconds.
    pub fn take(volume: &VolumeInfo) -> Result<Snapshot> {
        let (id, device) = crate::platform::take_snapshot(&volume.path)?;
        Ok(Snapshot {
            id,
            device,
            volume: volume_name(volume),
        })
    }

    /// What to open to read it.
    pub fn source(&self) -> SourceSpec {
        SourceSpec::Volume(self.device.clone())
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = crate::platform::delete_snapshot(&self.id);
    }
}
