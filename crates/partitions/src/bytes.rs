//! Bounds-checked little-endian reads. Disk data is untrusted, so nothing in
//! this crate indexes slices directly: a short or odd buffer gives `None`.

pub(crate) fn bytes(data: &[u8], offset: usize, len: usize) -> Option<&[u8]> {
    data.get(offset..offset.checked_add(len)?)
}

pub(crate) fn has(data: &[u8], offset: usize, expected: &[u8]) -> bool {
    bytes(data, offset, expected.len()) == Some(expected)
}

pub(crate) fn u8_at(data: &[u8], offset: usize) -> Option<u8> {
    data.get(offset).copied()
}

pub(crate) fn u16_at(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes(data, offset, 2)?.try_into().ok()?))
}

pub(crate) fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes(data, offset, 4)?.try_into().ok()?))
}

pub(crate) fn u64_at(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes(data, offset, 8)?.try_into().ok()?))
}

pub(crate) fn u32_be(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes(data, offset, 4)?.try_into().ok()?))
}

pub(crate) fn array16(data: &[u8], offset: usize) -> Option<[u8; 16]> {
    bytes(data, offset, 16)?.try_into().ok()
}

/// Text up to the first NUL, trimmed; `None` if empty.
pub(crate) fn text(data: &[u8]) -> Option<String> {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let text = String::from_utf8_lossy(data.get(..end)?).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// UTF-16LE text up to the first NUL; `None` if empty.
pub(crate) fn utf16_text(data: &[u8]) -> Option<String> {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .take_while(|&unit| unit != 0)
        .collect();
    let text = String::from_utf16_lossy(&units).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Microsoft GUID text: the first three groups are little-endian.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_never_panic() {
        let data = [1, 2, 3];
        assert_eq!(u16_at(&data, 1), Some(0x0302));
        assert_eq!(u16_at(&data, 2), None);
        assert_eq!(u64_at(&data, usize::MAX), None);
        assert!(!has(&data, 2, b"\x03\x04"));
        assert_eq!(text(b"  NO\0junk"), Some("NO".to_owned()));
        assert_eq!(utf16_text(&[b'h', 0, b'i', 0, 0, 0]), Some("hi".to_owned()));
    }
}
