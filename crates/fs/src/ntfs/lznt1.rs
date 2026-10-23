//! LZNT1, the compression of NTFS compressed files. Data is split into
//! 4 KiB chunks, each stored as is or as LZ77 literals and back-references.
//! Layout: Microsoft [MS-XCA] section 2.5.

use crate::bytes::{u8_at, u16_at};

/// Each chunk stands for this many bytes.
const CHUNK: usize = 4096;

/// Decompresses `input` into exactly `len` bytes. Damaged input stops the
/// decompression, and what's missing reads as zeros.
pub(crate) fn decompress(input: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut at = 0;
    while out.len() < len {
        let Some(header) = u16_at(input, at).filter(|&header| header != 0) else {
            break;
        };
        let size = usize::from(header & 0x0FFF) + 1;
        let Some(chunk) = input.get(at + 2..at + 2 + size) else {
            break;
        };
        at += 2 + size;
        let start = out.len();
        let complete = if header & 0x8000 == 0 {
            out.extend_from_slice(&chunk[..size.min(CHUNK)]);
            true
        } else {
            decompress_chunk(chunk, &mut out, start)
        };
        // A chunk that comes out short is padded: the next one starts 4 KiB on.
        out.resize(start + CHUNK, 0);
        if !complete {
            break;
        }
    }
    out.resize(len, 0);
    out
}

/// Decompresses one chunk onto `out`, whose chunk began at `start`.
/// Returns false if the chunk is damaged.
fn decompress_chunk(chunk: &[u8], out: &mut Vec<u8>, start: usize) -> bool {
    let mut at = 0;
    while let Some(flags) = u8_at(chunk, at) {
        at += 1;
        for bit in 0..8 {
            let position = out.len() - start;
            if at >= chunk.len() || position >= CHUNK {
                return true;
            }
            if flags >> bit & 1 == 0 {
                out.push(chunk[at]);
                at += 1;
                continue;
            }
            let Some(token) = u16_at(chunk, at) else {
                return false;
            };
            at += 2;
            // The further into the chunk, the more bits go to the distance.
            let mut shift = 0;
            let mut i = position.saturating_sub(1);
            while i >= 0x10 {
                shift += 1;
                i >>= 1;
            }
            let length = usize::from(token & (0x0FFF >> shift)) + 3;
            let distance = usize::from(token >> (12 - shift)) + 1;
            if distance > position {
                return false;
            }
            for _ in 0..length.min(CHUNK - position) {
                let Some(&byte) = out.get(out.len() - distance) else {
                    return false;
                };
                out.push(byte);
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{lznt1, noise, text};

    #[test]
    fn round_trips() {
        for data in [
            text("lznt1", 10_000),
            noise(1, 9000),
            [text("a", 3000), vec![0; 5000], text("b", 100)].concat(),
            b"abcabcabcabcabcabcabcabcabcabcabcabcabc".to_vec(),
            vec![7; 4096 * 3],
        ] {
            let packed = lznt1(&data);
            assert_eq!(decompress(&packed, data.len()), data);
        }
    }

    #[test]
    fn a_known_stream() {
        // "abcabcabcabc": three literals, then one token copying 9 bytes from 3 back.
        let chunk = [0b0000_1000, b'a', b'b', b'c', 0x06, 0x20];
        let mut input = vec![];
        input
            .extend_from_slice(&(0xB000 | (u16::try_from(chunk.len()).unwrap() - 1)).to_le_bytes());
        input.extend_from_slice(&chunk);
        assert_eq!(decompress(&input, 12), b"abcabcabcabc");
    }

    #[test]
    fn damaged_input_never_panics() {
        for seed in 0..64 {
            let mut input = noise(seed, 3000);
            input[1] = 0xB0 | (input[1] & 0x0F);
            let out = decompress(&input, 16 * 1024);
            assert_eq!(out.len(), 16 * 1024);
        }
        assert_eq!(decompress(&[], 10), [0; 10]);
        // A back-reference before the start of the chunk.
        assert_eq!(decompress(&[0x02, 0xB0, 0x01, 0x00, 0x00], 4), [0; 4]);
    }
}
