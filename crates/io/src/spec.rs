//! [`SourceSpec`]: what to open.

use std::fmt;
use std::path::PathBuf;

/// What to open: a physical disk, a volume, or a disk image file.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceSpec {
    /// A physical disk by number (`\\.\PhysicalDrive0` on Windows).
    PhysicalDisk(u32),
    /// A volume: a drive letter such as `C:`, or a volume path such as
    /// `\\?\Volume{…}` (no trailing backslash).
    Volume(String),
    /// A disk image file (.img, .dd, .raw, …).
    Image(PathBuf),
}

impl SourceSpec {
    /// Understands the ways people write a source:
    ///
    /// - disks: `disk0`, `disk:0`, `PhysicalDrive0`, `\\.\PhysicalDrive0`
    /// - volumes: `C:`, `C:\`, `volume:C:`, `\\?\Volume{…}`
    /// - anything else is an image file path (`image:` prefix optional)
    ///
    /// [`SourceSpec::id`] produces text that parses back to the same spec.
    pub fn parse(text: &str) -> SourceSpec {
        let text = text.trim();
        if let Some(path) = strip_prefix_ignore_case(text, "image:") {
            return SourceSpec::Image(PathBuf::from(path));
        }
        if let Some(volume) = strip_prefix_ignore_case(text, "volume:") {
            return SourceSpec::Volume(normalize_volume(volume));
        }
        for prefix in ["disk:", "disk", r"\\.\PhysicalDrive", "PhysicalDrive"] {
            if let Some(number) =
                strip_prefix_ignore_case(text, prefix).and_then(|n| n.parse().ok())
            {
                return SourceSpec::PhysicalDisk(number);
            }
        }
        if is_drive_letter(text) || strip_prefix_ignore_case(text, r"\\?\Volume{").is_some() {
            return SourceSpec::Volume(normalize_volume(text));
        }
        SourceSpec::Image(PathBuf::from(text))
    }

    /// A stable identifier that [`SourceSpec::parse`] turns back into this spec,
    /// e.g. `disk:0`, `volume:C:`, `image:D:\disk.img`.
    pub fn id(&self) -> String {
        self.to_string()
    }

    /// A short name for people: `Disk 0`, `Volume C:`, or the image's file name.
    pub fn display_name(&self) -> String {
        match self {
            SourceSpec::PhysicalDisk(number) => format!("Disk {number}"),
            SourceSpec::Volume(path) if is_drive_letter(path) => format!("Volume {path}"),
            SourceSpec::Volume(path) => {
                let guid = path.trim_start_matches(r"\\?\Volume");
                format!("Volume {guid}")
            }
            SourceSpec::Image(path) => path.file_name().map_or_else(
                || path.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
        }
    }
}

impl fmt::Display for SourceSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceSpec::PhysicalDisk(number) => write!(f, "disk:{number}"),
            SourceSpec::Volume(path) => write!(f, "volume:{path}"),
            SourceSpec::Image(path) => write!(f, "image:{}", path.display()),
        }
    }
}

fn strip_prefix_ignore_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &text[prefix.len()..])
}

/// `C:` or `C:\`.
fn is_drive_letter(text: &str) -> bool {
    let bytes = text.as_bytes();
    matches!(bytes, [letter, b':'] | [letter, b':', b'\\'] if letter.is_ascii_alphabetic())
}

/// `c:\` becomes `C:`; volume paths lose their trailing backslash.
fn normalize_volume(text: &str) -> String {
    let text = text.trim_end_matches('\\');
    if is_drive_letter(text) {
        text.to_ascii_uppercase()
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_ways_people_write_disks() {
        for text in [
            "disk0",
            "disk:0",
            "Disk:0",
            "PhysicalDrive0",
            r"\\.\PhysicalDrive0",
            r"\\.\physicaldrive0",
        ] {
            assert_eq!(
                SourceSpec::parse(text),
                SourceSpec::PhysicalDisk(0),
                "{text}"
            );
        }
        assert_eq!(SourceSpec::parse("disk12"), SourceSpec::PhysicalDisk(12));
    }

    #[test]
    fn parses_volumes() {
        for text in ["C:", "c:", r"C:\", "volume:C:"] {
            assert_eq!(
                SourceSpec::parse(text),
                SourceSpec::Volume("C:".into()),
                "{text}"
            );
        }
        let guid = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}";
        assert_eq!(
            SourceSpec::parse(&format!("{guid}\\")),
            SourceSpec::Volume(guid.into())
        );
    }

    #[test]
    fn anything_else_is_an_image_file() {
        assert_eq!(
            SourceSpec::parse("disk.img"),
            SourceSpec::Image("disk.img".into())
        );
        assert_eq!(
            SourceSpec::parse(r"D:\images\disk.img"),
            SourceSpec::Image(r"D:\images\disk.img".into())
        );
        assert_eq!(
            SourceSpec::parse("diskette.img"),
            SourceSpec::Image("diskette.img".into())
        );
        assert_eq!(
            SourceSpec::parse("image:disk0"),
            SourceSpec::Image("disk0".into())
        );
    }

    #[test]
    fn ids_parse_back_to_the_same_spec() {
        let specs = [
            SourceSpec::PhysicalDisk(3),
            SourceSpec::Volume("E:".into()),
            SourceSpec::Volume(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}".into()),
            SourceSpec::Image(r"D:\images\disk0".into()),
        ];
        for spec in specs {
            assert_eq!(SourceSpec::parse(&spec.id()), spec);
        }
    }

    #[test]
    fn display_names() {
        assert_eq!(SourceSpec::PhysicalDisk(1).display_name(), "Disk 1");
        assert_eq!(SourceSpec::Volume("C:".into()).display_name(), "Volume C:");
        assert_eq!(
            SourceSpec::Image("dir/fat32.img".into()).display_name(),
            "fat32.img"
        );
    }
}
