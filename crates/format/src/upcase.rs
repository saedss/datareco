//! exFAT's up-case table: what each UTF-16 character is in capitals, for
//! comparing names. Stored compressed, as the exFAT specification allows:
//! 0xFFFF followed by a count means "the next count characters are their
//! own capitals".

/// A character's capital, when it has a single one in the same plane.
fn upcase(unit: u32) -> u32 {
    let Some(c) = char::from_u32(unit) else {
        return unit; // surrogate halves
    };
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(u), None) if u32::from(u) <= 0xFFFF => u32::from(u),
        _ => unit,
    }
}

/// Runs of characters that are their own capitals shorter than this are
/// listed one by one.
const MIN_RUN: u32 = 3;

/// The compressed table, as little-endian bytes.
pub(crate) fn table() -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    let mut c = 0u32;
    while c <= 0xFFFF {
        let start = c;
        while c <= 0xFFFF && upcase(c) == c {
            c += 1;
        }
        let run = c - start;
        // A lone 0xFFFF would read as the start of a run, so the run that
        // ends the table is always written as one.
        if run >= MIN_RUN || (run > 0 && c > 0xFFFF) {
            let mut left = run;
            while left > 0 {
                let part = left.min(0xFFFF);
                units.push(0xFFFF);
                units.push(u16::try_from(part).unwrap_or(u16::MAX));
                left -= part;
            }
        } else {
            for unit in start..c {
                units.push(u16::try_from(unit).unwrap_or(0));
            }
        }
        if c <= 0xFFFF {
            units.push(u16::try_from(upcase(c)).unwrap_or(0));
            c += 1;
        }
    }
    units.iter().flat_map(|u| u.to_le_bytes()).collect()
}

/// The table's checksum, for its directory entry.
pub(crate) fn checksum(table: &[u8]) -> u32 {
    table.iter().fold(0, |sum, &b| crate::rotate_add(sum, b))
}

/// Expands a compressed table into one capital per character.
#[cfg(test)]
pub(crate) fn expand(table: &[u8]) -> Vec<u16> {
    let units: Vec<u16> = table
        .chunks(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        if units[i] == 0xFFFF && i + 1 < units.len() {
            for _ in 0..units[i + 1] {
                out.push(u16::try_from(out.len()).unwrap_or(0));
            }
            i += 2;
        } else {
            out.push(units[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_maps_letters_to_capitals_and_the_rest_to_themselves() {
        let table = table();
        assert!(table.len() < 8 * 1024, "compressed: {} bytes", table.len());
        let full = expand(&table);
        assert_eq!(full.len(), 0x10000);
        assert_eq!(full[usize::from(b'a')], u16::from(b'A'));
        assert_eq!(full[usize::from(b'A')], u16::from(b'A'));
        assert_eq!(full[usize::from(b'1')], u16::from(b'1'));
        assert_eq!(full[0xE9], 0xC9, "é");
        assert_eq!(full[0x3B1], 0x391, "α");
        assert_eq!(full[0x430], 0x410, "а (Cyrillic)");
        assert_eq!(full[0xFFFF], 0xFFFF);
        assert_eq!(full[0xD800], 0xD800, "surrogate halves are left alone");
    }
}
