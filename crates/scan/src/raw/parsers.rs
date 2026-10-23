//! Finding where a file found by its signature ends, by reading its format.
//! Each parser also checks the structure holds up, so a few matching bytes
//! inside some other file aren't taken for a file.
//!
//! Everything here reads untrusted data: reads are bounds-checked through
//! [`Source`], loops are capped, and sizes are checked before use.

use super::signature::Parser;

/// A candidate file's bytes, from where its header was found, up to its
/// type's largest size (or the end of what's scanned).
pub(crate) trait Source {
    /// Reads at `offset` from the file's start. Returns how many bytes were
    /// read: fewer at the limit.
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> usize;
    /// How far the file may go.
    fn limit(&self) -> u64;
}

/// Where a file ends (`None`: it looks right, but its end wasn't found), and
/// a better extension when the contents say what it is (docx, wav, …).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Found {
    pub size: Option<u64>,
    pub extension: Option<&'static str>,
}

impl Found {
    fn sized(size: u64) -> Option<Found> {
        Some(Found {
            size: Some(size),
            extension: None,
        })
    }

    fn open_ended() -> Option<Found> {
        Some(Found {
            size: None,
            extension: None,
        })
    }

    fn named(mut self, extension: &'static str) -> Found {
        self.extension = Some(extension);
        self
    }
}

/// Small helpers over a [`Source`].
struct In<'a> {
    src: &'a mut dyn Source,
}

impl In<'_> {
    fn bytes<const N: usize>(&mut self, at: u64) -> Option<[u8; N]> {
        let mut buf = [0; N];
        (self.src.read(at, &mut buf) == N).then_some(buf)
    }

    fn vec(&mut self, at: u64, len: usize) -> Option<Vec<u8>> {
        let mut buf = vec![0; len];
        (self.src.read(at, &mut buf) == len).then_some(buf)
    }

    fn u8(&mut self, at: u64) -> Option<u8> {
        self.bytes::<1>(at).map(|b| b[0])
    }

    fn u16le(&mut self, at: u64) -> Option<u16> {
        self.bytes(at).map(u16::from_le_bytes)
    }

    fn u16be(&mut self, at: u64) -> Option<u16> {
        self.bytes(at).map(u16::from_be_bytes)
    }

    fn u32le(&mut self, at: u64) -> Option<u32> {
        self.bytes(at).map(u32::from_le_bytes)
    }

    fn u32be(&mut self, at: u64) -> Option<u32> {
        self.bytes(at).map(u32::from_be_bytes)
    }

    fn u64le(&mut self, at: u64) -> Option<u64> {
        self.bytes(at).map(u64::from_le_bytes)
    }

    fn u64be(&mut self, at: u64) -> Option<u64> {
        self.bytes(at).map(u64::from_be_bytes)
    }

    fn limit(&self) -> u64 {
        self.src.limit()
    }

    /// Where `pattern` next appears at or after `from`, before `until`.
    fn find(&mut self, from: u64, pattern: &[u8], until: u64) -> Option<u64> {
        const BLOCK: usize = 64 * 1024;
        let until = until.min(self.limit());
        let mut at = from;
        let mut buf = vec![0; BLOCK + pattern.len()];
        while at < until {
            let n = self.src.read(at, &mut buf);
            if n < pattern.len() {
                return None;
            }
            if let Some(i) = memchr::memmem::find(&buf[..n], pattern) {
                let hit = at + i as u64;
                return (hit < until).then_some(hit);
            }
            // Overlap so a pattern across blocks is found.
            at += (n - pattern.len() + 1) as u64;
        }
        None
    }
}

/// Reads the file's format to find its end, or `None` if it isn't one.
pub(crate) fn measure(parser: Parser, src: &mut dyn Source) -> Option<Found> {
    let mut input = In { src };
    let r = &mut input;
    match parser {
        Parser::Jpeg => jpeg(r),
        Parser::Png => png(r),
        Parser::Gif => gif(r),
        Parser::Bmp => bmp(r),
        Parser::Tiff => tiff(r),
        Parser::Riff => riff(r),
        Parser::Zip => zip(r),
        Parser::Pdf => pdf(r),
        Parser::IsoBmff => isobmff(r),
        Parser::Mp3 => mp3(r),
        Parser::Ogg => ogg(r),
        Parser::Ebml => ebml(r),
        Parser::SevenZip => seven_zip(r),
        Parser::Rar => rar(r),
        Parser::Sqlite => sqlite(r),
        Parser::Ole => ole(r),
        Parser::Pe => pe(r),
    }
}

/// The end after `footer`, searched for from `from`.
pub(crate) fn footer(src: &mut dyn Source, from: u64, footer: &[u8]) -> Option<Found> {
    let mut r = In { src };
    let limit = r.limit();
    match r.find(from, footer, limit) {
        Some(at) => Found::sized(at + footer.len() as u64),
        None => Found::open_ended(),
    }
}

/// The size stored in the header.
pub(crate) fn field(
    src: &mut dyn Source,
    offset: usize,
    bytes: usize,
    big_endian: bool,
    add: u64,
) -> Option<Found> {
    let mut r = In { src };
    let raw = r.vec(offset as u64, bytes)?;
    let mut padded = [0u8; 8];
    if big_endian {
        padded[8 - bytes..].copy_from_slice(&raw);
        Found::sized(u64::from_be_bytes(padded).checked_add(add)?)
    } else {
        padded[..bytes].copy_from_slice(&raw);
        Found::sized(u64::from_le_bytes(padded).checked_add(add)?)
    }
}

fn jpeg(r: &mut In) -> Option<Found> {
    // The first segment after the start marker must be a real one.
    let first = r.u8(3)?;
    if !matches!(first, 0xC0..=0xCF | 0xDB | 0xDD | 0xE0..=0xEF | 0xFE) {
        return None;
    }
    let mut pos = 2u64;
    for _ in 0..100_000 {
        let [ff, marker] = r.bytes::<2>(pos)?;
        if ff != 0xFF {
            return None;
        }
        match marker {
            0xFF => pos += 1, // fill byte
            0xD9 => return Found::sized(pos + 2),
            0x01 | 0xD0..=0xD7 => pos += 2,
            0xD8 | 0x00 => return None,
            0xDA => {
                let len = u64::from(r.u16be(pos + 2)?);
                pos = entropy_end(r, pos + 2 + len)?;
            }
            _ => {
                let len = u64::from(r.u16be(pos + 2)?);
                if len < 2 {
                    return None;
                }
                pos += 2 + len;
            }
        }
        if pos >= r.limit() {
            return Found::open_ended();
        }
    }
    Found::open_ended()
}

/// The next marker after compressed image data: an FF not followed by 00
/// (stuffing) or a restart marker.
fn entropy_end(r: &mut In, from: u64) -> Option<u64> {
    let limit = r.limit();
    let mut at = from;
    while at < limit {
        let hit = r.find(at, &[0xFF], limit)?;
        match r.u8(hit + 1)? {
            0x00 | 0xD0..=0xD7 | 0xFF => at = hit + 1,
            _ => return Some(hit),
        }
    }
    None
}

fn png(r: &mut In) -> Option<Found> {
    let mut pos = 8u64;
    for i in 0..1_000_000 {
        let len = u64::from(r.u32be(pos)?);
        let kind = r.bytes::<4>(pos + 4)?;
        if len > 0x7FFF_FFFF || !kind.iter().all(u8::is_ascii_alphabetic) {
            return None;
        }
        if i == 0 && &kind != b"IHDR" {
            return None;
        }
        pos += 12 + len;
        if &kind == b"IEND" {
            return Found::sized(pos);
        }
        if pos >= r.limit() {
            return Found::open_ended();
        }
    }
    Found::open_ended()
}

fn gif(r: &mut In) -> Option<Found> {
    let flags = r.u8(10)?;
    let mut pos = 13 + color_table(flags);
    for _ in 0..1_000_000 {
        match r.u8(pos)? {
            0x3B => return Found::sized(pos + 1),
            0x21 => pos = sub_blocks(r, pos + 2)?,
            0x2C => {
                let flags = r.u8(pos + 9)?;
                pos = sub_blocks(r, pos + 10 + color_table(flags) + 1)?;
            }
            _ => return if pos > 13 { Found::open_ended() } else { None },
        }
    }
    Found::open_ended()
}

fn color_table(flags: u8) -> u64 {
    if flags & 0x80 != 0 {
        3 << ((flags & 7) + 1)
    } else {
        0
    }
}

/// Skips GIF data sub-blocks; returns where the next block starts.
fn sub_blocks(r: &mut In, mut pos: u64) -> Option<u64> {
    loop {
        let size = u64::from(r.u8(pos)?);
        pos += 1 + size;
        if size == 0 {
            return Some(pos);
        }
    }
}

fn bmp(r: &mut In) -> Option<Found> {
    let size = u64::from(r.u32le(2)?);
    let data = u64::from(r.u32le(10)?);
    let header = r.u32le(14)?;
    let plausible = matches!(header, 12 | 40 | 52 | 56 | 108 | 124)
        && data >= 14 + u64::from(header)
        && data < size;
    plausible.then_some(())?;
    Found::sized(size)
}

fn tiff(r: &mut In) -> Option<Found> {
    let little = r.bytes::<2>(0)? == *b"II";
    let u16at = |r: &mut In, at| if little { r.u16le(at) } else { r.u16be(at) };
    let u32at = |r: &mut In, at| if little { r.u32le(at) } else { r.u32be(at) };
    let first = u64::from(u32at(r, 4)?);
    if first < 8 {
        return None;
    }
    let mut end = 8u64;
    let mut queue = vec![first];
    let mut seen = std::collections::HashSet::new();
    let mut ifds = 0;
    while let Some(ifd) = queue.pop() {
        if ifd == 0 || !seen.insert(ifd) || ifds >= 64 {
            continue;
        }
        ifds += 1;
        let count = u64::from(u16at(r, ifd)?);
        if count == 0 || count > 1000 {
            if ifds == 1 {
                return None; // the first IFD must be real
            }
            continue;
        }
        end = end.max(ifd + 2 + 12 * count + 4);
        let (mut strips, mut counts) = (Vec::new(), Vec::new());
        for i in 0..count {
            let at = ifd + 2 + 12 * i;
            let tag = u16at(r, at)?;
            let kind = u16at(r, at + 2)?;
            let n = u64::from(u32at(r, at + 4)?);
            let width: u64 = match kind {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 | 13 => 4,
                5 | 10 | 12 => 8,
                _ => continue,
            };
            let bytes = n.saturating_mul(width);
            let value_at = if bytes > 4 {
                u64::from(u32at(r, at + 8)?)
            } else {
                at + 8
            };
            if bytes > 4 {
                end = end.max(value_at.saturating_add(bytes));
            }
            let values = |r: &mut In| -> Option<Vec<u64>> {
                (0..n.min(100_000))
                    .map(|j| match width {
                        2 => u16at(r, value_at + 2 * j).map(u64::from),
                        _ => u32at(r, value_at + 4 * j).map(u64::from),
                    })
                    .collect()
            };
            match tag {
                273 | 324 => strips = values(r)?,
                279 | 325 => counts = values(r)?,
                513 => strips.extend(values(r)?),
                514 => counts.extend(values(r)?),
                330 | 34665 | 34853 => queue.extend(values(r)?),
                _ => {}
            }
        }
        for (offset, length) in strips.iter().zip(&counts) {
            end = end.max(offset.saturating_add(*length));
        }
        queue.push(u64::from(u32at(r, ifd + 2 + 12 * count)?));
    }
    if end > r.limit() {
        return Found::open_ended();
    }
    let found = Found::sized(end)?;
    // Canon CR2 files say so right after the header.
    Some(if r.bytes::<2>(8)? == *b"CR" {
        found.named("cr2")
    } else {
        found
    })
}

fn riff(r: &mut In) -> Option<Found> {
    let size = u64::from(r.u32le(4)?) + 8;
    let form = r.bytes::<4>(8)?;
    let extension = match &form {
        b"WAVE" => "wav",
        b"AVI " => "avi",
        b"WEBP" => "webp",
        b"RMID" => "rmi",
        b"CDXA" => "dat",
        _ if form.iter().all(|b| b.is_ascii_graphic() || *b == b' ') => "riff",
        _ => return None,
    };
    // Chunks are padded to even lengths; some writers pad the whole file.
    Some(Found::sized(size)?.named(extension))
}

fn zip(r: &mut In) -> Option<Found> {
    // A sane first local header: version, name length.
    let version = r.u16le(4)?;
    let name_len = r.u16le(26)?;
    if version > 100 || name_len == 0 || name_len > 1024 {
        return None;
    }
    let limit = r.limit();
    let mut from = 30;
    while let Some(at) = r.find(from, b"PK\x05\x06", limit) {
        from = at + 1;
        let cd_size = u64::from(r.u32le(at + 12)?);
        let cd_offset = u64::from(r.u32le(at + 16)?);
        let comment = u64::from(r.u16le(at + 20)?);
        let mut central = None;
        if cd_offset.checked_add(cd_size) == Some(at) {
            central = Some((cd_offset, cd_size));
        } else if cd_offset == 0xFFFF_FFFF && at >= 20 && r.bytes::<4>(at - 20)? == *b"PK\x06\x07" {
            // ZIP64: the real numbers are in the ZIP64 end record.
            let record = r.u64le(at - 20 + 8)?;
            if r.bytes::<4>(record)? == *b"PK\x06\x06" {
                let size64 = r.u64le(record.saturating_add(40))?;
                let offset64 = r.u64le(record.saturating_add(48))?;
                if offset64.checked_add(size64) == Some(record) {
                    central = Some((offset64, size64));
                }
            }
        }
        if let Some((offset, size)) = central {
            let found = Found::sized(at + 22 + comment)?;
            return Some(match zip_kind(r, offset, size) {
                Some(extension) => found.named(extension),
                None => found,
            });
        }
    }
    Found::open_ended()
}

/// What a ZIP file is, from its first file and the names it holds.
fn zip_kind(r: &mut In, cd_offset: u64, cd_size: u64) -> Option<&'static str> {
    // OpenDocument and EPUB start with an uncompressed "mimetype" file.
    let name_len = usize::from(r.u16le(26)?);
    let first_name = r.vec(30, name_len)?;
    if first_name == b"mimetype" {
        let extra = u64::from(r.u16le(28)?);
        let mime = r.vec(30 + 8 + extra, 50).unwrap_or_default();
        let kinds: [(&[u8], &str); 4] = [
            (b"application/epub+zip", "epub"),
            (b"application/vnd.oasis.opendocument.text", "odt"),
            (b"application/vnd.oasis.opendocument.spreadsheet", "ods"),
            (b"application/vnd.oasis.opendocument.presentation", "odp"),
        ];
        for (prefix, extension) in kinds {
            if mime.starts_with(prefix) {
                return Some(extension);
            }
        }
    }
    let directory = r.vec(cd_offset, usize::try_from(cd_size.min(1 << 20)).ok()?)?;
    let mut at = 0;
    while let Some(entry) = directory
        .get(at..at + 46)
        .filter(|e| e.starts_with(b"PK\x01\x02"))
    {
        let name_len = usize::from(u16::from_le_bytes([entry[28], entry[29]]));
        let extra = usize::from(u16::from_le_bytes([entry[30], entry[31]]));
        let comment = usize::from(u16::from_le_bytes([entry[32], entry[33]]));
        let name = directory.get(at + 46..at + 46 + name_len)?;
        let kind = if name.starts_with(b"word/") {
            Some("docx")
        } else if name.starts_with(b"xl/") {
            Some("xlsx")
        } else if name.starts_with(b"ppt/") {
            Some("pptx")
        } else if name == b"AndroidManifest.xml" {
            Some("apk")
        } else if name == b"META-INF/MANIFEST.MF" {
            Some("jar")
        } else {
            None
        };
        if kind.is_some() {
            return kind;
        }
        at += 46 + name_len + extra + comment;
    }
    None
}

fn pdf(r: &mut In) -> Option<Found> {
    let limit = r.limit();
    let mut from = 5;
    while let Some(at) = r.find(from, b"%%EOF", limit) {
        let mut end = at + 5;
        // The end of line after it belongs to the file.
        match r.bytes::<2>(end) {
            Some([b'\r', b'\n']) => end += 2,
            Some([b'\r' | b'\n', _]) => end += 1,
            _ => {}
        }
        // A PDF updated in place has more objects after its first %%EOF.
        let next = r.vec(end, 32).unwrap_or_default();
        let text = String::from_utf8_lossy(&next);
        let rest = text.trim_start();
        let more = rest.starts_with("xref")
            || rest
                .split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .as_slice()
                .get(..3)
                .is_some_and(|w| {
                    w[0].bytes().all(|b| b.is_ascii_digit())
                        && w[1].bytes().all(|b| b.is_ascii_digit())
                        && w[2].starts_with("obj")
                });
        if !more {
            return Found::sized(end);
        }
        from = end;
    }
    Found::open_ended()
}

fn isobmff(r: &mut In) -> Option<Found> {
    const TOP_LEVEL: [&[u8; 4]; 18] = [
        b"ftyp", b"moov", b"mdat", b"free", b"skip", b"wide", b"meta", b"uuid", b"pdin", b"moof",
        b"mfra", b"styp", b"sidx", b"ssix", b"prft", b"emsg", b"pnot", b"junk",
    ];
    let brand = r.bytes::<4>(8)?;
    let mut pos = 0u64;
    let mut boxes = 0;
    let mut media = false;
    while let Some(kind) = r.bytes::<4>(pos + 4) {
        if !TOP_LEVEL.contains(&&kind) || boxes >= 100_000 {
            break;
        }
        let mut size = u64::from(r.u32be(pos)?);
        if size == 1 {
            size = r.u64be(pos + 8)?;
        } else if size == 0 {
            return Some(Found::open_ended()?.named(brand_extension(&brand)));
        }
        if size < 8 {
            break;
        }
        media |= matches!(&kind, b"moov" | b"mdat" | b"meta" | b"moof");
        boxes += 1;
        pos = pos.saturating_add(size);
        if pos > r.limit() {
            return Some(Found::open_ended()?.named(brand_extension(&brand)));
        }
    }
    if boxes < 2 || !media {
        return None;
    }
    Some(Found::sized(pos)?.named(brand_extension(&brand)))
}

fn brand_extension(brand: &[u8; 4]) -> &'static str {
    match brand {
        b"qt  " => "mov",
        b"M4A " | b"M4B " => "m4a",
        b"M4V " => "m4v",
        b"heic" | b"heix" | b"mif1" | b"msf1" | b"heim" | b"heis" => "heic",
        b"avif" => "avif",
        b"crx " => "cr3",
        _ if brand.starts_with(b"3g") => "3gp",
        _ => "mp4",
    }
}

fn mp3(r: &mut In) -> Option<Found> {
    let [major, _, flags] = r.bytes::<3>(3)?;
    let size = r.bytes::<4>(6)?;
    if !(2..=4).contains(&major) || size.iter().any(|b| b & 0x80 != 0) {
        return None;
    }
    let tag = size.iter().fold(0u64, |n, &b| (n << 7) | u64::from(b));
    let mut pos = 10 + tag + if flags & 0x10 != 0 { 10 } else { 0 };
    // Some files pad after the tag.
    let mut skipped = 0;
    while r.u8(pos)? == 0 && skipped < 64 * 1024 {
        pos += 1;
        skipped += 1;
    }
    let mut frames = 0;
    while let Some(len) = r.bytes::<4>(pos).and_then(mpeg_frame_len) {
        pos += len;
        frames += 1;
        if frames > 10_000_000 {
            break;
        }
    }
    if frames < 2 {
        return None;
    }
    // An ID3v1 tag can follow the frames.
    if r.bytes::<3>(pos) == Some(*b"TAG") {
        pos += 128;
    }
    Found::sized(pos)
}

/// The length of an MPEG audio frame from its header, if it is one.
fn mpeg_frame_len(header: [u8; 4]) -> Option<u64> {
    if header[0] != 0xFF || header[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (header[1] >> 3) & 3; // 0: 2.5, 2: 2, 3: 1
    let layer = (header[1] >> 1) & 3; // 1: III, 2: II, 3: I
    let bitrate_index = usize::from(header[2] >> 4);
    let rate_index = usize::from((header[2] >> 2) & 3);
    let padding = u64::from((header[2] >> 1) & 1);
    if version == 1 || layer == 0 || bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    const V1: [[u64; 15]; 3] = [
        [
            0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
        ],
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
        ],
        [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ],
    ];
    const V2: [[u64; 15]; 2] = [
        [
            0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
        ],
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ];
    const RATES: [[u64; 3]; 3] = [
        [11_025, 12_000, 8_000],
        [22_050, 24_000, 16_000],
        [44_100, 48_000, 32_000],
    ];
    let mpeg1 = version == 3;
    let bitrate = 1000
        * match (mpeg1, layer) {
            (true, 3) => V1[0][bitrate_index],
            (true, 2) => V1[1][bitrate_index],
            (true, _) => V1[2][bitrate_index],
            (false, 3) => V2[0][bitrate_index],
            (false, _) => V2[1][bitrate_index],
        };
    let rate = RATES[match version {
        0 => 0,
        2 => 1,
        _ => 2,
    }][rate_index];
    let len = match layer {
        3 => (12 * bitrate / rate + padding) * 4,
        1 if !mpeg1 => 72 * bitrate / rate + padding,
        _ => 144 * bitrate / rate + padding,
    };
    (len >= 4).then_some(len)
}

fn ogg(r: &mut In) -> Option<Found> {
    let mut pos = 0u64;
    let mut pages = 0;
    let mut extension = "ogg";
    while r.bytes::<4>(pos) == Some(*b"OggS") && r.u8(pos + 4) == Some(0) {
        let segments = u64::from(r.u8(pos + 26)?);
        let table = r.vec(pos + 27, usize::try_from(segments).ok()?)?;
        let body: u64 = table.iter().map(|&s| u64::from(s)).sum();
        if pages == 0 {
            let start = r.vec(pos + 27 + segments, 8).unwrap_or_default();
            if start.starts_with(b"OpusHead") {
                extension = "opus";
            } else if start.starts_with(b"\x80theora") {
                extension = "ogv";
            }
        }
        pos += 27 + segments + body;
        pages += 1;
        if pages > 50_000_000 {
            break;
        }
    }
    (pages > 0).then_some(())?;
    Some(Found::sized(pos)?.named(extension))
}

/// An EBML variable-length number at `at`: its length and value (`None`
/// value: "unknown", all ones).
fn vint(r: &mut In, at: u64, keep_marker: bool) -> Option<(u64, Option<u64>)> {
    let first = r.u8(at)?;
    let len = u64::from(first.leading_zeros()) + 1;
    if len > 8 {
        return None;
    }
    let raw = r.vec(at, usize::try_from(len).ok()?)?;
    let mut value = 0u64;
    for &b in &raw {
        value = (value << 8) | u64::from(b);
    }
    if keep_marker {
        return Some((len, Some(value)));
    }
    let bits = 7 * len;
    let mask = (1u64 << bits) - 1;
    let value = value & mask;
    Some((len, (value != mask).then_some(value)))
}

fn ebml(r: &mut In) -> Option<Found> {
    let (id_len, _) = vint(r, 0, true)?;
    let (size_len, header_size) = vint(r, id_len, false)?;
    let header_start = id_len + size_len;
    let header_end = header_start + header_size?;
    // The document type, inside the header.
    let mut extension = "mkv";
    let header = r.vec(header_start, usize::try_from(header_size?.min(256)).ok()?)?;
    if memchr::memmem::find(&header, b"webm").is_some() {
        extension = "webm";
    }
    let segment_id = r.bytes::<4>(header_end)?;
    if segment_id != [0x18, 0x53, 0x80, 0x67] {
        return None;
    }
    let (len, size) = vint(r, header_end + 4, false)?;
    match size {
        Some(size) => Some(Found::sized(header_end + 4 + len + size)?.named(extension)),
        None => Some(Found::open_ended()?.named(extension)),
    }
}

fn seven_zip(r: &mut In) -> Option<Found> {
    let major = r.u8(6)?;
    let offset = r.u64le(12)?;
    let size = r.u64le(20)?;
    if major != 0 || size == 0 || size > 1 << 32 {
        return None;
    }
    Found::sized(32u64.checked_add(offset)?.checked_add(size)?)
}

/// A RAR 5 variable-length number: (bytes used, value).
fn rar_vint(r: &mut In, at: u64) -> Option<(u64, u64)> {
    let mut value = 0u64;
    for i in 0..10 {
        let b = r.u8(at + i)?;
        value |= u64::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Some((i + 1, value));
        }
    }
    None
}

fn rar(r: &mut In) -> Option<Found> {
    match r.bytes::<2>(6)? {
        [0x01, 0x00] => {
            let mut pos = 8u64;
            for _ in 0..1_000_000 {
                let (n, header_size) = rar_vint(r, pos + 4)?;
                let header = pos + 4 + n;
                let (n1, kind) = rar_vint(r, header)?;
                let (n2, flags) = rar_vint(r, header + n1)?;
                let mut at = header + n1 + n2;
                if flags & 1 != 0 {
                    at += rar_vint(r, at)?.0;
                }
                let data = if flags & 2 != 0 {
                    rar_vint(r, at)?.1
                } else {
                    0
                };
                if !(1..=5).contains(&kind) || header_size == 0 || header_size > 2 << 20 {
                    return if pos > 8 { Found::open_ended() } else { None };
                }
                let next = header.checked_add(header_size)?;
                if kind == 5 {
                    return Found::sized(next);
                }
                pos = next.checked_add(data)?;
            }
            Found::open_ended()
        }
        [0x00, _] => {
            let mut pos = 7u64;
            for _ in 0..1_000_000 {
                let kind = r.u8(pos + 2)?;
                let flags = r.u16le(pos + 3)?;
                let size = u64::from(r.u16le(pos + 5)?);
                let add = if flags & 0x8000 != 0 {
                    u64::from(r.u32le(pos + 7)?)
                } else {
                    0
                };
                if !(0x72..=0x7B).contains(&kind) || size < 7 {
                    return if pos > 7 { Found::open_ended() } else { None };
                }
                if kind == 0x7B {
                    return Found::sized(pos + size);
                }
                pos += size + add;
            }
            Found::open_ended()
        }
        _ => None,
    }
}

fn sqlite(r: &mut In) -> Option<Found> {
    let page = match r.u16be(16)? {
        1 => 65_536,
        n if n.is_power_of_two() && n >= 512 => u64::from(n),
        _ => return None,
    };
    let changes = r.u32be(24)?;
    let pages = u64::from(r.u32be(28)?);
    let valid_for = r.u32be(92)?;
    // The page count is only kept up to date by SQLite 3.7.0 and later.
    if pages > 0 && valid_for == changes {
        Found::sized(page * pages)
    } else {
        Found::open_ended()
    }
}

fn ole(r: &mut In) -> Option<Found> {
    if r.u16le(28)? != 0xFFFE {
        return None;
    }
    let shift = r.u16le(30)?;
    if shift != 9 && shift != 12 {
        return None;
    }
    let sector = 1u64 << shift;
    let fat_sectors = r.u32le(44)?;
    let dir_start = r.u32le(48)?;
    let per_sector = sector / 4;
    // Where the FAT's sectors are: 109 in the header, then DIFAT sectors.
    let mut fat: Vec<u32> = (0..109u64)
        .filter_map(|i| r.u32le(76 + 4 * i))
        .filter(|&s| s < 0xFFFF_FFFA)
        .collect();
    let mut difat = r.u32le(68)?;
    let mut hops = 0;
    while difat < 0xFFFF_FFFA && fat.len() < fat_sectors as usize && hops < 10_000 {
        let at = (u64::from(difat) + 1) * sector;
        for i in 0..per_sector - 1 {
            if let Some(s) = r.u32le(at + 4 * i).filter(|&s| s < 0xFFFF_FFFA) {
                fat.push(s);
            }
        }
        difat = r.u32le(at + 4 * (per_sector - 1))?;
        hops += 1;
    }
    fat.truncate(fat_sectors as usize);
    if fat.is_empty() {
        return None;
    }
    // The last sector in use is where the file ends.
    let mut last = 0u64;
    for (n, &s) in fat.iter().enumerate() {
        let at = (u64::from(s) + 1) * sector;
        let entries = r.vec(at, usize::try_from(sector).ok()?)?;
        for (i, pair) in entries.as_chunks::<4>().0.iter().enumerate() {
            let value = u32::from_le_bytes(*pair);
            if value != 0xFFFF_FFFF {
                last = last.max(n as u64 * per_sector + i as u64);
            }
        }
    }
    let found = Found::sized((last + 2) * sector)?;
    // What it is, from the stream names in its first directory sector.
    let names = r
        .vec(
            (u64::from(dir_start) + 1) * sector,
            usize::try_from(sector).ok()?,
        )
        .unwrap_or_default();
    let has = |name: &str| {
        let wide: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        memchr::memmem::find(&names, &wide).is_some()
    };
    Some(if has("WordDocument") {
        found.named("doc")
    } else if has("Workbook") || has("Book") {
        found.named("xls")
    } else if has("PowerPoint Document") {
        found.named("ppt")
    } else if has("__substg1.0_") || has("__properties_version1.0") {
        found.named("msg")
    } else {
        found
    })
}

fn pe(r: &mut In) -> Option<Found> {
    let pe = u64::from(r.u32le(60)?);
    if !(64..=64 * 1024).contains(&pe) || r.bytes::<4>(pe)? != *b"PE\0\0" {
        return None;
    }
    let sections = u64::from(r.u16le(pe + 6)?);
    let optional_size = u64::from(r.u16le(pe + 20)?);
    let characteristics = r.u16le(pe + 22)?;
    if sections == 0 || sections > 96 {
        return None;
    }
    let optional = pe + 24;
    let magic = r.u16le(optional)?;
    let directories = match magic {
        0x10B => optional + 96,
        0x20B => optional + 112,
        _ => return None,
    };
    let mut end = u64::from(r.u32le(optional + 60)?); // size of the headers
    let table = optional + optional_size;
    for i in 0..sections {
        let at = table + 40 * i;
        let size = u64::from(r.u32le(at + 16)?);
        let pointer = u64::from(r.u32le(at + 20)?);
        if size > 0 {
            end = end.max(pointer + size);
        }
    }
    // A signature, if any, is appended at a file offset given in the directory.
    let signature_at = u64::from(r.u32le(directories + 4 * 8).unwrap_or(0));
    let signature_size = u64::from(r.u32le(directories + 4 * 8 + 4).unwrap_or(0));
    if signature_at > 0 && signature_size > 0 {
        end = end.max(signature_at + signature_size);
    }
    let found = Found::sized(end)?;
    Some(if characteristics & 0x2000 != 0 {
        found.named("dll")
    } else {
        found
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file in memory, as a [`Source`].
    pub(crate) struct Bytes(pub Vec<u8>);

    impl Source for Bytes {
        fn read(&mut self, offset: u64, buf: &mut [u8]) -> usize {
            let Some(rest) = usize::try_from(offset).ok().and_then(|o| self.0.get(o..)) else {
                return 0;
            };
            let n = rest.len().min(buf.len());
            buf[..n].copy_from_slice(&rest[..n]);
            n
        }

        fn limit(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn size_of(parser: Parser, file: &[u8], trailing: usize) -> Option<Found> {
        let mut data = file.to_vec();
        data.extend(std::iter::repeat_n(0xAB, trailing));
        measure(parser, &mut Bytes(data))
    }

    #[test]
    fn jpeg_ends_after_its_image_data() {
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x4A, 0x46];
        // An APP1 segment holding a thumbnail with its own end marker.
        jpeg.extend([0xFF, 0xE1, 0x00, 0x08, 0xFF, 0xD8, 0xFF, 0xD9, 0x00, 0x00]);
        jpeg.extend([0xFF, 0xDA, 0x00, 0x02]);
        // Image data with a stuffed FF00 and a restart marker.
        jpeg.extend([0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD3, 0x56, 0xFF, 0xD9]);
        assert_eq!(
            size_of(Parser::Jpeg, &jpeg, 100).unwrap().size,
            Some(jpeg.len() as u64)
        );
        assert_eq!(
            size_of(Parser::Jpeg, &[0xFF, 0xD8, 0xFF, 0x12, 0, 0], 0),
            None
        );
    }

    #[test]
    fn png_and_gif_and_bmp() {
        let chunk = |kind: &[u8], data: &[u8]| {
            let len = u32::try_from(data.len()).unwrap();
            [&len.to_be_bytes()[..], kind, data, &[0, 0, 0, 0]].concat()
        };
        let png = [
            &b"\x89PNG\r\n\x1a\n"[..],
            &chunk(b"IHDR", &[0; 13]),
            &chunk(b"IDAT", &[1; 50]),
            &chunk(b"IEND", &[]),
        ]
        .concat();
        assert_eq!(
            size_of(Parser::Png, &png, 77).unwrap().size,
            Some(png.len() as u64)
        );

        let mut gif = b"GIF89a\x02\x00\x02\x00\x80\x00\x00".to_vec();
        gif.extend([0u8; 6]); // global color table of 2
        gif.extend([0x21, 0xF9, 0x04, 1, 2, 3, 4, 0x00]);
        gif.extend([
            0x2C, 0, 0, 0, 0, 2, 0, 2, 0, 0x00, 0x02, 0x02, 0x4C, 0x01, 0x00, 0x3B,
        ]);
        assert_eq!(
            size_of(Parser::Gif, &gif, 30).unwrap().size,
            Some(gif.len() as u64)
        );

        let mut bmp = vec![0u8; 70];
        bmp[..2].copy_from_slice(b"BM");
        bmp[2..6].copy_from_slice(&70u32.to_le_bytes());
        bmp[10..14].copy_from_slice(&54u32.to_le_bytes());
        bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
        assert_eq!(size_of(Parser::Bmp, &bmp, 10).unwrap().size, Some(70));
        bmp[14] = 99;
        assert_eq!(size_of(Parser::Bmp, &bmp, 10), None);
    }

    #[test]
    fn riff_says_what_it_holds() {
        let mut wav = b"RIFF\x24\x00\x00\x00WAVEfmt ".to_vec();
        wav.resize(44, 0);
        let found = size_of(Parser::Riff, &wav, 20).unwrap();
        assert_eq!((found.size, found.extension), (Some(44), Some("wav")));
    }

    /// A ZIP file with one stored member called `name`.
    pub(crate) fn zip_with(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let local = [
            &b"PK\x03\x04\x14\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"[..],
            &u32::try_from(data.len()).unwrap().to_le_bytes(),
            &u32::try_from(data.len()).unwrap().to_le_bytes(),
            &u16::try_from(name.len()).unwrap().to_le_bytes(),
            &[0, 0],
            name,
            data,
        ]
        .concat();
        out.extend(&local);
        let cd_at = u32::try_from(out.len()).unwrap();
        let central = [
            &b"PK\x01\x02\x14\x00\x14\x00"[..],
            &[0; 12],
            &u32::try_from(data.len()).unwrap().to_le_bytes(),
            &u32::try_from(data.len()).unwrap().to_le_bytes(),
            &u16::try_from(name.len()).unwrap().to_le_bytes(),
            &[0; 12],
            &0u32.to_le_bytes(),
            name,
        ]
        .concat();
        out.extend(&central);
        out.extend(b"PK\x05\x06\0\0\0\0\x01\x00\x01\x00");
        out.extend(u32::try_from(central.len()).unwrap().to_le_bytes());
        out.extend(cd_at.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    #[test]
    fn zip_ends_at_its_own_end_record() {
        // A ZIP stored inside another: the inner end record doesn't fool it.
        let inner = zip_with(b"inner.txt", b"hello");
        let outer = zip_with(b"word/document.xml", &inner);
        let found = size_of(Parser::Zip, &outer, 300).unwrap();
        assert_eq!(
            (found.size, found.extension),
            (Some(outer.len() as u64), Some("docx"))
        );
        assert_eq!(size_of(Parser::Zip, &inner, 0).unwrap().extension, None);
    }

    #[test]
    fn pdf_ends_at_its_last_eof() {
        let pdf = b"%PDF-1.4\n1 0 obj<<>>endobj\ntrailer<<>>\n%%EOF\n2 0 obj<<>>endobj\n%%EOF\r\n";
        assert_eq!(
            size_of(Parser::Pdf, pdf, 40).unwrap().size,
            Some(pdf.len() as u64)
        );
        let open = size_of(Parser::Pdf, b"%PDF-1.4\n1 0 obj", 40).unwrap();
        assert_eq!(open.size, None);
    }

    #[test]
    fn mp4_boxes_and_brands() {
        let boxed = |kind: &[u8], body: &[u8]| {
            let len = u32::try_from(body.len() + 8).unwrap();
            [&len.to_be_bytes()[..], kind, body].concat()
        };
        let mov = [
            boxed(b"ftyp", b"qt  \0\0\0\0"),
            boxed(b"moov", &[1; 30]),
            boxed(b"mdat", &[2; 100]),
        ]
        .concat();
        let found = size_of(Parser::IsoBmff, &mov, 50).unwrap();
        assert_eq!(
            (found.size, found.extension),
            (Some(mov.len() as u64), Some("mov"))
        );
        // Just a file type box isn't a video.
        assert_eq!(
            size_of(Parser::IsoBmff, &boxed(b"ftyp", b"isom\0\0\0\0"), 50),
            None
        );
    }

    #[test]
    fn mp3_frames_after_the_tag() {
        let mut mp3 = b"ID3\x03\x00\x00\x00\x00\x00\x0A".to_vec();
        mp3.extend([0u8; 10]);
        // MPEG-1 layer III, 128 kbit/s, 44.1 kHz: 417-byte frames.
        for _ in 0..3 {
            let mut frame = vec![0u8; 417];
            frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            mp3.extend(frame);
        }
        assert_eq!(
            size_of(Parser::Mp3, &mp3, 100).unwrap().size,
            Some(mp3.len() as u64)
        );
    }

    #[test]
    fn archives_and_databases() {
        let mut seven = vec![0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 4];
        seven.resize(32, 0);
        seven[12..20].copy_from_slice(&100u64.to_le_bytes());
        seven[20..28].copy_from_slice(&20u64.to_le_bytes());
        seven.resize(152, 0x11);
        assert_eq!(
            size_of(Parser::SevenZip, &seven, 10).unwrap().size,
            Some(152)
        );

        // RAR 5: main header, then the end-of-archive header.
        let mut rar5 = b"Rar!\x1A\x07\x01\x00".to_vec();
        rar5.extend([0, 0, 0, 0, 3, 1, 0, 0]);
        rar5.extend([0, 0, 0, 0, 3, 5, 0, 0]);
        assert_eq!(
            size_of(Parser::Rar, &rar5, 10).unwrap().size,
            Some(rar5.len() as u64)
        );

        let mut db = b"SQLite format 3\0".to_vec();
        db.resize(100, 0);
        db[16..18].copy_from_slice(&1024u16.to_be_bytes());
        db[24..28].copy_from_slice(&7u32.to_be_bytes());
        db[28..32].copy_from_slice(&3u32.to_be_bytes());
        db[92..96].copy_from_slice(&7u32.to_be_bytes());
        assert_eq!(size_of(Parser::Sqlite, &db, 10).unwrap().size, Some(3072));
    }

    #[test]
    fn garbage_never_panics() {
        let parsers = [
            Parser::Jpeg,
            Parser::Png,
            Parser::Gif,
            Parser::Bmp,
            Parser::Tiff,
            Parser::Riff,
            Parser::Zip,
            Parser::Pdf,
            Parser::IsoBmff,
            Parser::Mp3,
            Parser::Ogg,
            Parser::Ebml,
            Parser::SevenZip,
            Parser::Rar,
            Parser::Sqlite,
            Parser::Ole,
            Parser::Pe,
        ];
        let mut state = 0x1234_5678_9ABC_DEF1u64;
        for round in 0..200 {
            let len = 16 + (round * 37) % 5000;
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state.to_le_bytes()[0]
                })
                .collect();
            for parser in parsers {
                let _ = measure(parser, &mut Bytes(data.clone()));
            }
        }
    }
}
