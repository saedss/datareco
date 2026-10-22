//! Where sectors come from: image files, OS devices, or memory (for tests).

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Raw access to whole sectors of a disk, volume or image file.
///
/// Implementations only read. [`crate::Disk`] adds caching and bad-sector
/// handling on top.
pub trait SectorSource: Send {
    /// Bytes per sector, e.g. 512 or 4096.
    fn sector_size(&self) -> u32;

    /// Total size in bytes. Image files may end partway through a sector.
    fn size(&self) -> u64;

    /// Fills `buf`, a whole number of sectors long, starting at sector `lba`.
    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()>;
}

/// A disk, volume or image file read through the OS file API.
pub struct FileSource {
    file: File,
    size: u64,
    sector_size: u32,
    /// Image files may end partway through their last sector; the rest of
    /// that sector reads as zeros. Devices always end on a sector boundary.
    zero_fill_past_end: bool,
}

impl FileSource {
    /// Opens a disk image file read-only. Images use 512-byte sectors.
    pub fn open_image(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            file,
            size,
            sector_size: 512,
            zero_fill_past_end: true,
        })
    }

    /// Wraps an already opened (read-only) device handle.
    pub(crate) fn from_device(file: File, size: u64, sector_size: u32) -> Self {
        Self {
            file,
            size,
            sector_size,
            zero_fill_past_end: false,
        }
    }
}

impl SectorSource for FileSource {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        let offset = lba
            .checked_mul(u64::from(self.sector_size))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "sector number too large")
            })?;
        self.file.seek(SeekFrom::Start(offset))?;
        if !self.zero_fill_past_end {
            return self.file.read_exact(buf);
        }
        let available = self.size.saturating_sub(offset);
        let readable = usize::try_from(available).map_or(buf.len(), |n| n.min(buf.len()));
        let (data, past_end) = buf.split_at_mut(readable);
        self.file.read_exact(data)?;
        past_end.fill(0);
        Ok(())
    }
}

/// An in-memory disk for tests. Sectors can be made unreadable, or made to
/// fail a few times before they read.
pub struct MemorySource {
    data: Vec<u8>,
    sector_size: u32,
    bad: BTreeSet<u64>,
    flaky: HashMap<u64, u32>,
    reads: Arc<AtomicUsize>,
}

impl MemorySource {
    /// `data` is the disk's content; its length must be a whole number of sectors.
    pub fn new(data: Vec<u8>, sector_size: u32) -> Self {
        assert!(sector_size > 0, "sector size must not be zero");
        assert_eq!(
            data.len() % sector_size as usize,
            0,
            "data must be a whole number of sectors"
        );
        Self {
            data,
            sector_size,
            bad: BTreeSet::new(),
            flaky: HashMap::new(),
            reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Makes these sectors fail every read.
    pub fn with_bad_sectors(mut self, sectors: impl IntoIterator<Item = u64>) -> Self {
        self.bad.extend(sectors);
        self
    }

    /// Makes a sector fail its first `failures` reads, then succeed.
    pub fn with_flaky_sector(mut self, lba: u64, failures: u32) -> Self {
        self.flaky.insert(lba, failures);
        self
    }

    /// A counter of calls to `read_sectors`, readable after the source is moved
    /// into a [`crate::Disk`].
    pub fn read_counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.reads)
    }
}

impl SectorSource for MemorySource {
    fn sector_size(&self) -> u32 {
        self.sector_size
    }

    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> io::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let sector_size = self.sector_size as usize;
        let count = (buf.len() / sector_size) as u64;
        for sector in lba..lba + count {
            if self.bad.contains(&sector) {
                return Err(io::Error::other(format!("simulated bad sector {sector}")));
            }
            if let Some(remaining) = self.flaky.get_mut(&sector)
                && *remaining > 0
            {
                *remaining -= 1;
                return Err(io::Error::other(format!("simulated flaky sector {sector}")));
            }
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|lba| lba.checked_mul(sector_size))
            .filter(|&start| start + buf.len() <= self.data.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end"))?;
        buf.copy_from_slice(&self.data[start..start + buf.len()]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("datareco-io-{}-{name}", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn image_file_reads_whole_sectors() {
        let content: Vec<u8> = (0..2048u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let path = temp_file("whole.img", &content);
        let mut source = FileSource::open_image(&path).unwrap();
        assert_eq!(source.size(), 2048);
        let mut buf = vec![0; 512];
        source.read_sectors(2, &mut buf).unwrap();
        assert_eq!(buf, content[1024..1536]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn image_file_ending_mid_sector_reads_zeros_past_the_end() {
        let path = temp_file("partial.img", &[0xAB; 700]);
        let mut source = FileSource::open_image(&path).unwrap();
        let mut buf = vec![0xFF; 512];
        source.read_sectors(1, &mut buf).unwrap();
        assert!(buf[..188].iter().all(|&b| b == 0xAB));
        assert!(buf[188..].iter().all(|&b| b == 0));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn memory_source_simulates_bad_and_flaky_sectors() {
        let mut source = MemorySource::new(vec![1; 4096], 512)
            .with_bad_sectors([3])
            .with_flaky_sector(5, 1);
        let mut buf = vec![0; 512];
        assert!(source.read_sectors(3, &mut buf).is_err());
        assert!(source.read_sectors(5, &mut buf).is_err());
        assert!(source.read_sectors(5, &mut buf).is_ok());
        assert_eq!(source.read_counter().load(Ordering::Relaxed), 3);
    }
}
