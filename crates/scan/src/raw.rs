//! Raw recovery: finding files by their contents (their signatures) when no
//! file system says where they are, after a format or when the file system
//! is damaged beyond rebuilding.
//!
//! The scan looks at every 512-byte boundary (files start at cluster
//! boundaries). Where a signature's header matches, the format is read to
//! find where the file ends; a file read through to its end is skipped over,
//! so what's inside it (a JPEG in a document) isn't found again. Files are
//! taken to be in one piece: a fragmented file comes back wrong.

mod parsers;
mod signature;
mod volume;

use std::path::Path;
use std::sync::Arc;

use datareco_io::Disk;
use serde::{Deserialize, Serialize};

pub use signature::{
    DEFAULT_SIGNATURES, End, Parser, Signature, default_signatures, load_signatures,
    parse_signatures,
};
pub use volume::RawVolume;

use crate::ScanError;
use parsers::{Found, Source};

/// Files start at multiples of this.
const STEP: u64 = 512;
/// Bytes read at a time.
const READ_CHUNK: u64 = 4 << 20;
/// Files kept at most.
const MAX_FILES: usize = 10_000_000;
/// The saved-scan format this version writes.
const FORMAT: u32 = 1;

/// A file found: byte offset on the disk, size, extension (an index into
/// the scan's extensions), and whether its end was found (rather than cut).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawFile(pub u64, pub u64, pub u16, pub bool);

impl RawFile {
    pub fn offset(self) -> u64 {
        self.0
    }

    pub fn size(self) -> u64 {
        self.1
    }

    pub fn extension(self) -> usize {
        usize::from(self.2)
    }

    /// Whether its end was found by reading its format (or footer).
    pub fn exact(self) -> bool {
        self.3
    }
}

/// How a raw scan is going.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RawProgress {
    /// Byte offsets on the disk: where the scan started, is, and will end.
    pub start: u64,
    pub position: u64,
    pub end: u64,
    pub done: bool,
    /// Files found so far.
    pub files: usize,
}

/// A raw scan in progress. Call [`RawScan::step`] until it's done; the files
/// found so far can be looked at any time with [`RawScan::volume`].
pub struct RawScan {
    signatures: Vec<Signature>,
    /// Signatures by their header's first byte (headers at offset 0).
    by_first: Vec<Vec<usize>>,
    /// Signatures whose header is further in, or starts with ??.
    others: Vec<usize>,
    start: u64,
    end: u64,
    position: u64,
    done: bool,
    extensions: Vec<String>,
    files: Vec<RawFile>,
    /// The last file whose end wasn't found, and its largest size: it's cut
    /// where the next file starts.
    open: Option<(usize, u64)>,
}

/// A raw scan saved to carry on later, or to use what it found.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedRawScan {
    pub format: u32,
    /// The source, as it was opened (`disk:1`, `image:D:\disk.img`, …).
    pub source: String,
    pub disk_size: u64,
    pub sector_size: u32,
    pub start: u64,
    pub end: u64,
    pub position: u64,
    pub done: bool,
    extensions: Vec<String>,
    files: Vec<RawFile>,
    open: Option<(usize, u64)>,
}

impl RawScan {
    /// A scan of `range` (byte offsets; the whole disk if `None`) for `signatures`.
    pub fn new(disk: &Disk, signatures: Vec<Signature>, range: Option<(u64, u64)>) -> RawScan {
        let (start, end) = range.unwrap_or((0, disk.size()));
        let start = start / STEP * STEP;
        let end = end.min(disk.size()).max(start);
        let mut by_first = vec![Vec::new(); 256];
        let mut others = Vec::new();
        for (i, s) in signatures.iter().enumerate() {
            match s.header.first() {
                Some(Some(b)) if s.header_offset == 0 => by_first[usize::from(*b)].push(i),
                _ => others.push(i),
            }
        }
        RawScan {
            signatures,
            by_first,
            others,
            start,
            end,
            position: start,
            done: start >= end,
            extensions: Vec::new(),
            files: Vec::new(),
            open: None,
        }
    }

    pub fn progress(&self) -> RawProgress {
        RawProgress {
            start: self.start,
            position: self.position,
            end: self.end,
            done: self.done,
            files: self.files.len(),
        }
    }

    /// Looks at about `limit` more bytes.
    pub fn step(&mut self, disk: &Disk, limit: u64) -> Result<RawProgress, ScanError> {
        let stop = self.position.saturating_add(limit.max(1)).min(self.end);
        while self.position < stop && !self.done {
            let len = READ_CHUNK.min(self.end - self.position);
            // A little more, for headers that run past the chunk.
            let extra = 64.min(self.end - self.position - len);
            let mut data = vec![0; usize::try_from(len + extra).unwrap_or(0)];
            disk.read_at(self.position, &mut data)?;
            let mut next = self.position + len;
            let step = usize::try_from(STEP).unwrap_or(512);
            'chunk: for i in (0..len).step_by(step) {
                let at = self.position + i;
                let window = &data[usize::try_from(i).unwrap_or(0)..];
                let first = usize::from(window[0]);
                let candidates: Vec<usize> = self.by_first[first]
                    .iter()
                    .chain(&self.others)
                    .copied()
                    .collect();
                for index in candidates {
                    if !header_matches(&self.signatures[index], window) {
                        continue;
                    }
                    if let Some(skip_to) = self.try_file(disk, at, index) {
                        if let Some(skip_to) = skip_to {
                            next = skip_to.max(at + STEP);
                            break 'chunk;
                        }
                        break;
                    }
                }
            }
            self.position = next.min(self.end);
            if self.files.len() >= MAX_FILES {
                self.position = self.end;
            }
        }
        if self.position >= self.end && !self.done {
            self.close_open(self.end);
            self.done = true;
        }
        Ok(self.progress())
    }

    /// Runs the whole scan and returns what it found.
    pub fn run(
        disk: &Arc<Disk>,
        signatures: Vec<Signature>,
        range: Option<(u64, u64)>,
    ) -> Result<RawVolume, ScanError> {
        let mut scan = RawScan::new(disk, signatures, range);
        while !scan.step(disk, 256 << 20)?.done {}
        Ok(scan.volume(Arc::clone(disk)))
    }

    /// Checks a header match at `at`. `None`: not a file after all. Some
    /// file found: `Some(Some(skip))` when its end was found by reading it
    /// through (the scan goes on after it), `Some(None)` otherwise.
    fn try_file(&mut self, disk: &Disk, at: u64, index: usize) -> Option<Option<u64>> {
        let signature = &self.signatures[index];
        let room = self.end - at;
        let limit = signature.max_size.min(room);
        let mut source = DiskSource::new(disk, at, limit);
        let header_end = (signature.header_offset + signature.header.len()) as u64;
        let found = match &signature.end {
            End::Parser(parser) => parsers::measure(*parser, &mut source),
            End::Footer(footer) => parsers::footer(&mut source, header_end, footer),
            End::Field {
                offset,
                bytes,
                big_endian,
                add,
            } => parsers::field(&mut source, *offset, *bytes, *big_endian, *add),
            End::Unknown => Some(Found {
                size: None,
                extension: None,
            }),
        }?;
        let walked = match &signature.end {
            End::Parser(parser) => walks_to_end(*parser),
            End::Footer(_) => true,
            _ => false,
        };
        let (size, exact) = match found.size {
            Some(size) if size < header_end || size < signature.min_size => return None,
            Some(size) if size <= limit => (Some(size), true),
            // Runs past what's scanned: keep what's there.
            Some(_) if signature.max_size > room => (Some(room), false),
            // Bigger than files of its kind get: not a real one.
            Some(_) => return None,
            None => (None, false),
        };
        let extension = found.extension.unwrap_or(&signature.extension).to_owned();
        let max_size = signature.max_size;
        self.close_open(at);
        let extension = self.intern(extension);
        match size {
            Some(size) => {
                self.files.push(RawFile(at, size, extension, exact));
                Some((walked && exact).then(|| (at + size).div_ceil(STEP) * STEP))
            }
            None => {
                self.open = Some((self.files.len(), max_size));
                self.files.push(RawFile(at, 0, extension, false));
                Some(None)
            }
        }
    }

    /// Ends the file whose end wasn't found where the next one starts.
    fn close_open(&mut self, at: u64) {
        if let Some((index, max_size)) = self.open.take()
            && let Some(file) = self.files.get_mut(index)
        {
            file.1 = at.saturating_sub(file.0).min(max_size);
        }
    }

    fn intern(&mut self, extension: String) -> u16 {
        let index = match self.extensions.iter().position(|e| *e == extension) {
            Some(i) => i,
            None => {
                self.extensions.push(extension);
                self.extensions.len() - 1
            }
        };
        u16::try_from(index).unwrap_or(u16::MAX)
    }

    /// What was found so far, as a volume to browse and recover from.
    pub fn volume(&self, disk: Arc<Disk>) -> RawVolume {
        let mut files = self.files.clone();
        // A file still open ends where the scan is now.
        if let Some((index, max_size)) = self.open
            && let Some(file) = files.get_mut(index)
        {
            file.1 = self.position.saturating_sub(file.0).min(max_size);
        }
        files.retain(|f| f.size() > 0);
        RawVolume::new(disk, self.start, self.end, &self.extensions, files)
    }

    pub fn save(&self, source: &str, disk: &Disk) -> SavedRawScan {
        SavedRawScan {
            format: FORMAT,
            source: source.to_owned(),
            disk_size: disk.size(),
            sector_size: disk.sector_size(),
            start: self.start,
            end: self.end,
            position: self.position,
            done: self.done,
            extensions: self.extensions.clone(),
            files: self.files.clone(),
            open: self.open,
        }
    }

    /// Carries on with a saved scan of `disk`, looking for `signatures`.
    pub fn restore(
        disk: &Disk,
        signatures: Vec<Signature>,
        saved: &SavedRawScan,
    ) -> Result<RawScan, ScanError> {
        if saved.format != FORMAT {
            return Err(ScanError::Saved(format!(
                "This scan was saved by another version of DataReco (format {}).",
                saved.format
            )));
        }
        if saved.disk_size != disk.size() || saved.sector_size != disk.sector_size() {
            return Err(ScanError::Saved(format!(
                "This scan is of a different disk ({} bytes; this one has {} bytes).",
                saved.disk_size,
                disk.size()
            )));
        }
        let mut scan = RawScan::new(disk, signatures, Some((saved.start, saved.end)));
        scan.position = saved.position.clamp(scan.start, scan.end);
        scan.done = saved.done;
        scan.extensions.clone_from(&saved.extensions);
        scan.files.clone_from(&saved.files);
        scan.open = saved.open;
        Ok(scan)
    }
}

impl SavedRawScan {
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }

    pub fn read_from(path: &Path) -> Result<SavedRawScan, ScanError> {
        let data = std::fs::read(path).map_err(|e| {
            ScanError::Saved(format!(
                "Couldn't read the saved scan {}: {e}",
                path.display()
            ))
        })?;
        serde_json::from_slice(&data).map_err(|e| {
            ScanError::Saved(format!("{} isn't a DataReco raw scan: {e}", path.display()))
        })
    }
}

/// Whether the parser reads a file through to its end, so the scan can skip
/// over it. Formats whose size is only a number in the header aren't
/// trusted that far.
fn walks_to_end(parser: Parser) -> bool {
    !matches!(
        parser,
        Parser::Bmp | Parser::SevenZip | Parser::Sqlite | Parser::Pe
    )
}

fn header_matches(signature: &Signature, data: &[u8]) -> bool {
    let Some(at) = data.get(signature.header_offset..) else {
        return false;
    };
    signature.header.len() <= at.len()
        && signature
            .header
            .iter()
            .zip(at)
            .all(|(want, have)| want.is_none_or(|w| w == *have))
}

/// A candidate file's bytes, read from the disk in blocks.
struct DiskSource<'a> {
    disk: &'a Disk,
    start: u64,
    limit: u64,
    block: Vec<u8>,
    /// Where `block` is, from the file's start.
    block_at: u64,
}

impl<'a> DiskSource<'a> {
    const BLOCK: u64 = 64 * 1024;

    fn new(disk: &'a Disk, start: u64, limit: u64) -> Self {
        Self {
            disk,
            start,
            limit,
            block: Vec::new(),
            block_at: 0,
        }
    }

    /// The byte at `offset`, loading its block if needed.
    fn load(&mut self, offset: u64) -> bool {
        let inside = offset >= self.block_at && offset < self.block_at + self.block.len() as u64;
        if inside {
            return true;
        }
        if offset >= self.limit {
            return false;
        }
        let at = offset / Self::BLOCK * Self::BLOCK;
        let len = Self::BLOCK.min(self.limit - at);
        let mut block = vec![0; usize::try_from(len).unwrap_or(0)];
        if self.disk.read_at(self.start + at, &mut block).is_err() {
            return false;
        }
        self.block = block;
        self.block_at = at;
        true
    }
}

impl Source for DiskSource<'_> {
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> usize {
        let mut done = 0;
        while done < buf.len() {
            let at = offset.saturating_add(done as u64);
            if !self.load(at) {
                break;
            }
            let within = usize::try_from(at - self.block_at).unwrap_or(0);
            let n = (self.block.len() - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&self.block[within..within + n]);
            done += n;
        }
        done
    }

    fn limit(&self) -> u64 {
        self.limit
    }
}
