//! Text formatting for the command line.

use std::fmt::Write;

/// `1536` → `1.5 KiB`; `931.5 GiB` style, binary units.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["bytes", "KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    // Precision loss is fine: this is for display only.
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Classic hex dump: offset, 16 bytes in hex, and the printable characters.
///
/// ```text
/// 000000000200  48 45 4c 4c 4f 00 00 00  00 00 00 00 00 00 00 00  |HELLO...........|
/// ```
pub fn hexdump(start_offset: u64, bytes: &[u8]) -> String {
    let mut out = String::new();
    for (row, chunk) in bytes.chunks(16).enumerate() {
        let offset = start_offset + row as u64 * 16;
        let _ = write!(out, "{offset:012x} ");
        for i in 0..16 {
            if i == 8 {
                out.push(' ');
            }
            match chunk.get(i) {
                Some(byte) => {
                    let _ = write!(out, " {byte:02x}");
                }
                None => out.push_str("   "),
            }
        }
        out.push_str("  |");
        out.extend(chunk.iter().map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        }));
        out.push_str("|\n");
    }
    out
}

/// Seconds since 1970 (UTC) as `2024-01-15 10:30`.
pub fn format_time(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let time = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        time / 3600,
        time % 3600 / 60
    )
}

/// Days since 1970-01-01 as (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(format_time(0), "1970-01-01 00:00");
        assert_eq!(format_time(1_705_314_600), "2024-01-15 10:30");
        assert_eq!(format_time(951_782_400), "2000-02-29 00:00");
        assert_eq!(format_time(-86_400), "1969-12-31 00:00");
    }

    #[test]
    fn sizes() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1023), "1023 bytes");
        assert_eq!(format_size(1536), "1.5 KiB");
        assert_eq!(format_size(48 * 1024 * 1024), "48.0 MiB");
        assert_eq!(format_size(1_000_204_886_016), "931.5 GiB");
    }

    #[test]
    fn hexdump_rows() {
        let mut bytes = b"HELLO".to_vec();
        bytes.resize(20, 0);
        assert_eq!(
            hexdump(0x200, &bytes),
            "000000000200  48 45 4c 4c 4f 00 00 00  00 00 00 00 00 00 00 00  |HELLO...........|\n\
             000000000210  00 00 00 00                                       |....|\n"
        );
    }
}
