//! Changing volumes through Windows: formatting with Windows' own
//! formatter, volume labels and drive letters. Each needs administrator
//! rights; the caller checks the volume first ([`crate::check_volume_for_changes`]
//! for formatting).

use crate::error::{Error, Result};

/// The file systems formatted through Windows rather than by DataReco.
pub const WINDOWS_FILE_SYSTEMS: [&str; 2] = ["NTFS", "ReFS"];

/// Formats the volume at `path` (`\\?\Volume{…}`) with Windows' own
/// formatter: `file_system` is one of [`WINDOWS_FILE_SYSTEMS`]. Quick
/// formats write only the file system's own structures.
pub fn format_with_windows(
    path: &str,
    file_system: &str,
    label: Option<&str>,
    quick: bool,
) -> Result<()> {
    let Some(file_system) = WINDOWS_FILE_SYSTEMS
        .iter()
        .find(|fs| fs.eq_ignore_ascii_case(file_system))
    else {
        return Err(Error::refused(format!(
            "Windows formats {}, not {file_system}.",
            WINDOWS_FILE_SYSTEMS.join(" and ")
        )));
    };
    if let Some(label) = label
        && label.chars().count() > 32
    {
        return Err(Error::refused("An NTFS label holds at most 32 characters."));
    }
    crate::platform::format_with_windows(
        path,
        file_system,
        label.filter(|l| !l.trim().is_empty()),
        quick,
    )
}

/// How small and how big Windows can make the partition starting `offset`
/// bytes into disk `number`: (smallest, biggest) in bytes. Only NTFS and
/// ReFS volumes can be resized; growing needs free space right after the
/// partition. Can take a while: Windows looks at where files sit.
pub fn partition_size_limits(number: u32, offset: u64) -> Result<(u64, u64)> {
    crate::platform::partition_size_limits(number, offset)
}

/// Shrinks or grows the partition starting `offset` bytes into disk
/// `number` to `size` bytes, with its file system, through Windows (which
/// moves files out of the way when shrinking). Works while the volume is in
/// use, the one Windows runs from included.
pub fn resize_with_windows(number: u32, offset: u64, size: u64) -> Result<()> {
    crate::platform::resize_with_windows(number, offset, size)
}

/// Sets (or, with `None` or an empty label, removes) a mounted volume's
/// label.
pub fn set_volume_label(path: &str, label: Option<&str>) -> Result<()> {
    crate::platform::set_volume_label(path, label.map(str::trim).filter(|l| !l.is_empty()))
}

/// Gives a volume the drive letter `letter` (in place of any it has), or
/// with `None` takes its drive letters away. Refuses letters in use, and
/// the volume Windows runs from.
pub fn set_drive_letter(path: &str, mount_points: &[String], letter: Option<char>) -> Result<()> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
    if mount_points
        .iter()
        .any(|m| m.trim_end_matches('\\').eq_ignore_ascii_case(&drive))
    {
        return Err(Error::refused(format!(
            "{drive} is where Windows runs from; its letter can't change."
        )));
    }
    let letter = match letter {
        Some(l) if l.is_ascii_alphabetic() => Some(l.to_ascii_uppercase()),
        Some(l) => return Err(Error::refused(format!("\"{l}\" isn't a drive letter."))),
        None => None,
    };
    if matches!(letter, Some('A' | 'B')) {
        return Err(Error::refused("A: and B: are kept for floppy drives."));
    }
    crate::platform::set_drive_letter(path, letter)
}
