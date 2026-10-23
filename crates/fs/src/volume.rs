//! A volume: a stretch of a disk that holds one file system.

use std::sync::Arc;

use datareco_io::Disk;

use crate::error::FsError;

/// Single reads larger than this mean the structures are garbage.
const MAX_READ: u64 = 256 << 20;

pub(crate) struct Volume {
    disk: Arc<Disk>,
    start: u64,
    size: u64,
}

impl Volume {
    /// `size` is what the file system claims; it's capped at the disk's end.
    pub fn new(disk: Arc<Disk>, start: u64, size: u64) -> Self {
        let size = size.min(disk.size().saturating_sub(start));
        Self { disk, start, size }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    /// An offset within the volume as an offset on the disk.
    pub fn absolute(&self, offset: u64) -> u64 {
        self.start.saturating_add(offset)
    }

    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|&end| end <= self.size);
        if end.is_none() {
            return Err(FsError::damaged(format!(
                "The file system points past the end of the volume (offset {offset}, {} bytes).",
                buf.len()
            )));
        }
        self.disk.read_at(self.start + offset, buf)?;
        Ok(())
    }

    pub fn read_vec(&self, offset: u64, len: u64) -> Result<Vec<u8>, FsError> {
        if len > MAX_READ {
            return Err(FsError::damaged(format!(
                "A structure claims to be {len} bytes long."
            )));
        }
        let mut data = vec![0; usize::try_from(len).unwrap_or(0)];
        self.read(offset, &mut data)?;
        Ok(data)
    }
}
