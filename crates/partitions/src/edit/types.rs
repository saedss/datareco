//! Partition types by name, for both table styles, and the conversions
//! between MBR type codes and GPT type GUIDs.

use serde::Serialize;

use crate::FsKind;
use crate::bytes::guid_text;

use super::Style;

/// A partition type people can pick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeChoice {
    /// What to pass as `type_code`: "ntfs", "linux", …
    pub code: &'static str,
    pub name: &'static str,
}

struct Known {
    code: &'static str,
    name: &'static str,
    mbr: Option<u8>,
    gpt: Option<&'static str>,
}

const BASIC_DATA: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";

const KNOWN: &[Known] = &[
    Known {
        code: "ntfs",
        name: "NTFS or exFAT (Windows data)",
        mbr: Some(0x07),
        gpt: Some(BASIC_DATA),
    },
    Known {
        code: "fat32",
        name: "FAT32",
        mbr: Some(0x0C),
        gpt: Some(BASIC_DATA),
    },
    Known {
        code: "fat16",
        name: "FAT16",
        mbr: Some(0x0E),
        gpt: Some(BASIC_DATA),
    },
    Known {
        code: "linux",
        name: "Linux file system",
        mbr: Some(0x83),
        gpt: Some("0FC63DAF-8483-4772-8E79-3D69D8477DE4"),
    },
    Known {
        code: "linux-swap",
        name: "Linux swap",
        mbr: Some(0x82),
        gpt: Some("0657FD6D-A4AB-43C4-84E5-0933C84B4F4F"),
    },
    Known {
        code: "linux-lvm",
        name: "Linux LVM",
        mbr: Some(0x8E),
        gpt: Some("E6D6D379-F507-44C2-A23C-238F2A3DF928"),
    },
    Known {
        code: "linux-raid",
        name: "Linux RAID",
        mbr: Some(0xFD),
        gpt: Some("A19D880F-05FC-4D3B-A006-743F0F84911E"),
    },
    Known {
        code: "efi",
        name: "EFI system partition",
        mbr: Some(0xEF),
        gpt: Some("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
    },
    Known {
        code: "recovery",
        name: "Windows recovery",
        mbr: Some(0x27),
        gpt: Some("DE94BBA4-06D1-4D40-A16A-BFD50179D6AC"),
    },
    Known {
        code: "msr",
        name: "Microsoft reserved",
        mbr: None,
        gpt: Some("E3C9E316-0B5C-4DB8-817D-F92DF00215AE"),
    },
    Known {
        code: "bios-boot",
        name: "BIOS boot partition",
        mbr: None,
        gpt: Some("21686148-6449-6E6F-744E-656564454649"),
    },
    Known {
        code: "hfsplus",
        name: "Apple HFS+",
        mbr: Some(0xAF),
        gpt: Some("48465300-0000-11AA-AA11-00306543ECAC"),
    },
    Known {
        code: "apfs",
        name: "Apple APFS",
        mbr: None,
        gpt: Some("7C3457EF-0000-11AA-AA11-00306543ECAC"),
    },
];

/// The types offered for a table style, most common first.
pub fn type_choices(style: Style) -> Vec<TypeChoice> {
    KNOWN
        .iter()
        .filter(|k| match style {
            Style::Mbr => k.mbr.is_some(),
            Style::Gpt => k.gpt.is_some(),
        })
        .map(|k| TypeChoice {
            code: k.code,
            name: k.name,
        })
        .collect()
}

/// A type for an MBR entry: a name from [`type_choices`], or a code like
/// "0x07" or "7".
pub(crate) fn mbr_type(code: &str) -> Result<u8, String> {
    let code = code.trim();
    if let Some(known) = KNOWN.iter().find(|k| k.code.eq_ignore_ascii_case(code)) {
        return known
            .mbr
            .ok_or_else(|| format!("{} partitions only exist on GPT disks.", known.name));
    }
    let digits = code.trim_start_matches("0x").trim_start_matches("0X");
    let value = u8::from_str_radix(digits, 16).map_err(|_| {
        format!(
            "\"{code}\" isn't a partition type: use a name such as ntfs, or a code such as 0x07."
        )
    })?;
    match value {
        0 => Err("Type 0x00 marks an empty entry.".to_owned()),
        0x05 | 0x0F | 0x85 => {
            Err("Extended partitions are made by creating logical partitions.".to_owned())
        }
        0xEE => Err("Type 0xEE marks a GPT disk; convert the disk to GPT instead.".to_owned()),
        0x42 => Err("Type 0x42 marks a dynamic disk, which DataReco doesn't change.".to_owned()),
        _ => Ok(value),
    }
}

/// A type GUID for a GPT entry: a name from [`type_choices`], or the GUID.
pub(crate) fn gpt_type(code: &str) -> Result<[u8; 16], String> {
    let code = code.trim();
    if let Some(known) = KNOWN.iter().find(|k| k.code.eq_ignore_ascii_case(code)) {
        let text = known
            .gpt
            .ok_or_else(|| format!("{} partitions only exist on MBR disks.", known.name))?;
        return parse_guid(text).ok_or_else(|| "bad built-in GUID".to_owned());
    }
    let guid = parse_guid(code).ok_or_else(|| {
        format!("\"{code}\" isn't a partition type: use a name such as ntfs, or a type GUID.")
    })?;
    if guid == [0; 16] {
        return Err("An all-zero type GUID marks an empty entry.".to_owned());
    }
    if is_dynamic_gpt(&guid) {
        return Err("Dynamic disk types aren't supported.".to_owned());
    }
    Ok(guid)
}

/// The GPT type for an MBR partition, when converting the disk to GPT.
pub(crate) fn mbr_to_gpt(code: u8) -> Option<[u8; 16]> {
    let unhidden = unhide_mbr(code).unwrap_or(code);
    let name = match unhidden {
        0x01 | 0x04 | 0x06 | 0x07 | 0x0B | 0x0C | 0x0E => "ntfs",
        other => KNOWN.iter().find(|k| k.mbr == Some(other))?.code,
    };
    gpt_type(name).ok()
}

/// The MBR type for a GPT partition, when converting the disk to MBR. Basic
/// data partitions get the code their file system needs.
pub(crate) fn gpt_to_mbr(guid: &[u8; 16], fs: Option<FsKind>) -> Option<u8> {
    let text = guid_text(guid);
    if text == BASIC_DATA {
        return Some(match fs {
            Some(FsKind::Fat32) => 0x0C,
            Some(FsKind::Fat16) => 0x0E,
            Some(FsKind::Fat12) => 0x01,
            _ => 0x07,
        });
    }
    KNOWN.iter().find(|k| k.gpt == Some(text.as_str()))?.mbr
}

pub(crate) fn is_msr(guid: &[u8; 16]) -> bool {
    guid_text(guid) == "E3C9E316-0B5C-4DB8-817D-F92DF00215AE"
}

pub(crate) fn is_basic_data(guid: &[u8; 16]) -> bool {
    guid_text(guid) == BASIC_DATA
}

/// Windows LDM (dynamic disk) and Storage Spaces types.
pub(crate) fn is_dynamic_gpt(guid: &[u8; 16]) -> bool {
    matches!(
        guid_text(guid).as_str(),
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3"
            | "AF9B60A0-1431-4F62-BC68-3311714A69AD"
            | "E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D"
    )
}

/// The hidden variant of a Windows MBR type, which Windows doesn't mount.
pub(crate) fn hide_mbr(code: u8) -> Option<u8> {
    matches!(code, 0x01 | 0x04 | 0x06 | 0x07 | 0x0B | 0x0C | 0x0E).then_some(code | 0x10)
}

pub(crate) fn unhide_mbr(code: u8) -> Option<u8> {
    matches!(code, 0x11 | 0x14 | 0x16 | 0x17 | 0x1B | 0x1C | 0x1E).then_some(code & !0x10)
}

/// The usual MBR type for a file system found on the disk (restoring a
/// lost partition).
pub fn type_for_file_system(fs: FsKind) -> &'static str {
    match fs {
        FsKind::Fat32 => "fat32",
        FsKind::Fat16 | FsKind::Fat12 => "fat16",
        FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4 | FsKind::Btrfs => "linux",
        FsKind::HfsPlus => "hfsplus",
        FsKind::Apfs => "apfs",
        _ => "ntfs",
    }
}

/// Parses "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" into the on-disk byte order
/// (the first three groups little-endian).
pub(crate) fn parse_guid(text: &str) -> Option<[u8; 16]> {
    let text = text.trim().trim_start_matches('{').trim_end_matches('}');
    let groups: Vec<&str> = text.split('-').collect();
    if groups.len() != 5
        || [8, 4, 4, 4, 12] != groups.iter().map(|g| g.len()).collect::<Vec<_>>()[..]
    {
        return None;
    }
    let hex: String = groups.concat();
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_round_trip_in_disk_order() {
        let text = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
        let bytes = parse_guid(text).unwrap();
        assert_eq!(bytes[0], 0x28, "first group little-endian");
        assert_eq!(guid_text(&bytes), text);
        assert!(parse_guid("not-a-guid").is_none());
        assert_eq!(parse_guid(&format!("{{{text}}}")), Some(bytes));
    }

    #[test]
    fn types_by_name_and_code() {
        assert_eq!(mbr_type("ntfs"), Ok(0x07));
        assert_eq!(mbr_type("0x83"), Ok(0x83));
        assert_eq!(mbr_type("c"), Ok(0x0C));
        assert!(mbr_type("msr").unwrap_err().contains("GPT"));
        assert!(mbr_type("0x0F").unwrap_err().contains("logical"));
        assert!(mbr_type("0xEE").is_err());
        assert!(mbr_type("zz").is_err());
        assert_eq!(
            guid_text(&gpt_type("efi").unwrap()),
            "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        );
        assert!(
            gpt_type("5808C8AA-7E8F-42E0-85D2-E1E90434CFB3").is_err(),
            "LDM"
        );
        assert!(type_choices(Style::Gpt).iter().any(|t| t.code == "msr"));
        assert!(!type_choices(Style::Mbr).iter().any(|t| t.code == "msr"));
    }

    #[test]
    fn conversions_between_styles() {
        let basic = gpt_type("ntfs").unwrap();
        assert_eq!(mbr_to_gpt(0x07), Some(basic));
        assert_eq!(mbr_to_gpt(0x1C), Some(basic), "hidden FAT32");
        assert_eq!(mbr_to_gpt(0x83), gpt_type("linux").ok());
        assert_eq!(mbr_to_gpt(0x99), None);
        assert_eq!(gpt_to_mbr(&basic, Some(FsKind::Fat32)), Some(0x0C));
        assert_eq!(gpt_to_mbr(&basic, Some(FsKind::Ntfs)), Some(0x07));
        assert_eq!(gpt_to_mbr(&gpt_type("efi").unwrap(), None), Some(0xEF));
        assert_eq!(gpt_to_mbr(&gpt_type("msr").unwrap(), None), None);
        assert_eq!(
            (hide_mbr(0x07), unhide_mbr(0x17), hide_mbr(0x83)),
            (Some(0x17), Some(0x07), None)
        );
    }
}
