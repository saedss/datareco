//! Turning raw field bytes into readable text.

use std::fmt::Write;

use crate::model::IntKind;

/// Bytes shown for `bytes` fields before the rest is summarized.
const MAX_BYTES_SHOWN: usize = 32;

/// Seconds between 1601-01-01 (Windows FILETIME epoch) and 1970-01-01.
const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

/// Reads an integer of `kind.bytes` bytes, sign-extended if signed.
pub(crate) fn read_int(bytes: &[u8], kind: &IntKind) -> i128 {
    let mut value: u128 = 0;
    let ordered: Vec<u8> = if kind.big_endian {
        bytes.to_vec()
    } else {
        bytes.iter().rev().copied().collect()
    };
    for byte in ordered {
        value = (value << 8) | u128::from(byte);
    }
    let bits = u32::try_from(8 * bytes.len()).expect("at most 64 bits");
    let unsigned = i128::try_from(value).expect("at most 64 bits");
    if kind.signed && bits > 0 && unsigned >> (bits - 1) == 1 {
        unsigned - (1i128 << bits)
    } else {
        unsigned
    }
}

pub(crate) fn int_text(value: i128, kind: &IntKind) -> String {
    if kind.hex {
        // Negative values show as their two's-complement bytes.
        let bits = u32::try_from(8 * kind.bytes).expect("at most 64 bits");
        let mask = (1i128 << bits) - 1;
        format!("0x{:0width$X}", value & mask, width = kind.bytes * 2)
    } else {
        value.to_string()
    }
}

/// The name for a value, or the names of its set flags.
pub(crate) fn int_note(value: i128, kind: &IntKind) -> Option<String> {
    if let Some((_, name)) = kind.values.iter().find(|(v, _)| *v == value) {
        return Some(name.clone());
    }
    if kind.flags.is_empty() {
        return None;
    }
    let mut names = Vec::new();
    let mut rest = value;
    for (bit, name) in &kind.flags {
        if *bit != 0 && value & bit == *bit {
            names.push(name.clone());
            rest &= !bit;
        }
    }
    if rest != 0 {
        names.push(format!("other bits 0x{rest:X}"));
    }
    Some(if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    })
}

pub(crate) fn bytes_text(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(MAX_BYTES_SHOWN)];
    let mut text = shown
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ");
    if bytes.len() > MAX_BYTES_SHOWN {
        let _ = write!(text, " … ({} bytes)", bytes.len());
    }
    if bytes.iter().all(|&b| b == 0) && !bytes.is_empty() {
        return format!("all zeros ({} bytes)", bytes.len());
    }
    text
}

/// Text up to the first NUL, with unprintable bytes as dots.
pub(crate) fn ascii_text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .map(|&b| {
            if (0x20..0x7F).contains(&b) {
                b as char
            } else {
                '.'
            }
        })
        .collect()
}

/// UTF-16LE text up to the first NUL. FFFF (padding in FAT long names) also
/// ends the text.
pub(crate) fn utf16_text(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|&unit| unit != 0 && unit != 0xFFFF)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Microsoft GUID layout: first three groups little-endian.
pub(crate) fn guid_text(b: &[u8; 16]) -> String {
    format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

/// RFC 4122 UUID as stored by Linux file systems (bytes in order).
pub(crate) fn uuid_text(b: &[u8; 16]) -> String {
    let hex: Vec<String> = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[..4].concat(),
        hex[4..6].concat(),
        hex[6..8].concat(),
        hex[8..10].concat(),
        hex[10..].concat()
    )
}

/// The 3-byte cylinder/head/sector address of MBR partition entries.
pub(crate) fn chs_text(b: &[u8; 3]) -> String {
    let head = b[0];
    let sector = b[1] & 0x3F;
    let cylinder = (u16::from(b[1] & 0xC0) << 2) | u16::from(b[2]);
    format!("cylinder {cylinder}, head {head}, sector {sector}")
}

pub(crate) fn fat_time_text(value: u16) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        value >> 11,
        (value >> 5) & 0x3F,
        (value & 0x1F) * 2
    )
}

pub(crate) fn fat_date_text(value: u16) -> String {
    if value == 0 {
        return "not set".to_owned();
    }
    format!(
        "{:04}-{:02}-{:02}",
        1980 + (value >> 9),
        (value >> 5) & 0x0F,
        value & 0x1F
    )
}

/// exFAT timestamps: time in the low 16 bits, date in the high 16 bits.
pub(crate) fn fat_datetime_text(value: u32) -> String {
    let [t0, t1, d0, d1] = value.to_le_bytes();
    let date = u16::from_le_bytes([d0, d1]);
    let time = u16::from_le_bytes([t0, t1]);
    if date == 0 {
        return "not set".to_owned();
    }
    format!("{} {}", fat_date_text(date), fat_time_text(time))
}

/// Windows FILETIME: 100-nanosecond intervals since 1601-01-01 UTC.
pub(crate) fn filetime_text(value: u64) -> String {
    if value == 0 {
        return "not set".to_owned();
    }
    let seconds = i64::try_from(value / 10_000_000).unwrap_or(i64::MAX) - FILETIME_UNIX_OFFSET;
    let fraction = value % 10_000_000;
    let mut text = unix_text(seconds);
    if fraction != 0 {
        text = text.replace(" UTC", &format!(".{fraction:07} UTC"));
    }
    text
}

/// Seconds since 1970-01-01 UTC.
pub(crate) fn unix_text(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        rest / 3600,
        (rest / 60) % 60,
        rest % 60
    )
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
/// Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(bytes: usize, signed: bool) -> IntKind {
        IntKind {
            bytes,
            signed,
            big_endian: false,
            hex: false,
            values: vec![],
            flags: vec![(1, "read-only".into()), (0x10, "directory".into())],
        }
    }

    #[test]
    fn integers() {
        assert_eq!(read_int(&[0x55, 0xAA], &int(2, false)), 0xAA55);
        assert_eq!(read_int(&[0xF6], &int(1, true)), -10);
        assert_eq!(
            read_int(&[1, 2, 3, 4, 5, 6], &int(6, false)),
            0x0605_0403_0201
        );
        let big = IntKind {
            big_endian: true,
            ..int(2, false)
        };
        assert_eq!(read_int(&[0x55, 0xAA], &big), 0x55AA);
        let hex = IntKind {
            hex: true,
            ..int(1, true)
        };
        assert_eq!(int_text(-10, &hex), "0xF6");
    }

    #[test]
    fn flags() {
        assert_eq!(
            int_note(0x11, &int(1, false)).as_deref(),
            Some("read-only, directory")
        );
        assert_eq!(int_note(0, &int(1, false)).as_deref(), Some("none"));
        assert_eq!(
            int_note(0x21, &int(1, false)).as_deref(),
            Some("read-only, other bits 0x20")
        );
    }

    #[test]
    fn dates() {
        assert_eq!(unix_text(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(unix_text(1_705_314_600), "2024-01-15 10:30:00 UTC");
        assert_eq!(unix_text(951_782_400), "2000-02-29 00:00:00 UTC");
        // 2024-01-15 10:30:00 UTC as a FILETIME.
        // (1_705_314_600 + 11_644_473_600) seconds × 10^7.
        assert_eq!(
            filetime_text(133_497_882_000_000_000),
            "2024-01-15 10:30:00 UTC"
        );
        assert_eq!(
            filetime_text(133_497_882_000_000_001),
            "2024-01-15 10:30:00.0000001 UTC"
        );
        assert_eq!(filetime_text(0), "not set");
        // FAT: 2024-01-15 = (44 << 9) | (1 << 5) | 15; 10:30:00 = (10 << 11) | (30 << 5).
        let date = (44 << 9) | (1 << 5) | 15;
        let time = (10 << 11) | (30 << 5);
        assert_eq!(fat_date_text(date), "2024-01-15");
        assert_eq!(fat_time_text(time), "10:30:00");
        assert_eq!(
            fat_datetime_text((u32::from(date) << 16) | u32::from(time)),
            "2024-01-15 10:30:00"
        );
    }

    #[test]
    fn ids_and_text() {
        let guid = [
            0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26,
            0x99, 0xC7,
        ];
        assert_eq!(guid_text(&guid), "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7");
        assert_eq!(
            uuid_text(&[0x12; 16]),
            "12121212-1212-1212-1212-121212121212"
        );
        assert_eq!(
            chs_text(&[0x20, 0x21, 0x00]),
            "cylinder 0, head 32, sector 33"
        );
        assert_eq!(ascii_text(b"NTFS    \0junk"), "NTFS    ");
        assert_eq!(utf16_text(&[b'h', 0, b'i', 0, 0, 0, 0xFF, 0xFF]), "hi");
        assert_eq!(bytes_text(&[0; 8]), "all zeros (8 bytes)");
        assert_eq!(bytes_text(&[0xEB, 0x52, 0x90]), "EB 52 90");
    }
}
