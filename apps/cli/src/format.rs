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

#[cfg(test)]
mod tests {
    use super::*;

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
