//! Searching a disk for a byte pattern.

use memchr::memmem::Finder;
use serde::Serialize;

use crate::disk::Disk;
use crate::error::Result;

/// Bytes read per step of a search.
const CHUNK: usize = 1 << 20;

/// How far a search got.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum SearchProgress {
    /// The pattern starts at this byte offset.
    Found { offset: u64 },
    /// Not found before `next`; search again from there.
    Continue { next: u64 },
    /// Not found anywhere from the start offset to the end of the disk.
    NotFound,
}

impl Disk {
    /// Looks for `pattern` starting at byte `from`, examining at most `limit`
    /// bytes in this call so callers can show progress and stop early.
    /// Unreadable sectors read as zeros and the search goes on.
    pub fn find(
        &self,
        pattern: &[u8],
        from: u64,
        limit: u64,
        ignore_ascii_case: bool,
    ) -> Result<SearchProgress> {
        if pattern.is_empty() || from >= self.size() {
            return Ok(SearchProgress::NotFound);
        }
        let end = from.saturating_add(limit.max(1)).min(self.size());
        let needle = if ignore_ascii_case {
            pattern.to_ascii_lowercase()
        } else {
            pattern.to_vec()
        };
        let finder = Finder::new(&needle);
        // Each window overlaps the next by pattern length - 1, so matches
        // across window edges are found.
        let overlap = pattern.len() - 1;
        let mut buf = vec![0; CHUNK + overlap];
        let mut pos = from;
        while pos < end {
            let len = usize::try_from((self.size() - pos).min(buf.len() as u64))
                .expect("at most the buffer size");
            let window = &mut buf[..len];
            self.read_at(pos, window)?;
            if ignore_ascii_case {
                window.make_ascii_lowercase();
            }
            if let Some(i) = finder.find(window) {
                let offset = pos + i as u64;
                if offset < end {
                    return Ok(SearchProgress::Found { offset });
                }
            }
            pos += CHUNK as u64;
        }
        Ok(if end >= self.size() {
            SearchProgress::NotFound
        } else {
            SearchProgress::Continue { next: end }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;

    fn disk_with(placements: &[(usize, &[u8])], size: usize) -> Disk {
        let mut data = vec![0u8; size];
        for (offset, bytes) in placements {
            data[*offset..offset + bytes.len()].copy_from_slice(bytes);
        }
        Disk::new("test", Box::new(MemorySource::new(data, 512)))
    }

    #[test]
    fn finds_a_pattern_that_crosses_a_chunk_boundary() {
        let disk = disk_with(&[(CHUNK - 3, b"NEEDLE")], 3 * CHUNK);
        assert_eq!(
            disk.find(b"NEEDLE", 0, u64::MAX, false).unwrap(),
            SearchProgress::Found {
                offset: (CHUNK - 3) as u64
            }
        );
    }

    #[test]
    fn starts_from_the_given_offset() {
        let disk = disk_with(&[(100, b"AB"), (5000, b"AB")], 8192);
        assert_eq!(
            disk.find(b"AB", 0, u64::MAX, false).unwrap(),
            SearchProgress::Found { offset: 100 }
        );
        assert_eq!(
            disk.find(b"AB", 101, u64::MAX, false).unwrap(),
            SearchProgress::Found { offset: 5000 }
        );
        assert_eq!(
            disk.find(b"AB", 5001, u64::MAX, false).unwrap(),
            SearchProgress::NotFound
        );
    }

    #[test]
    fn ignores_ascii_case_when_asked() {
        let disk = disk_with(&[(700, b"ReadMe")], 4096);
        assert_eq!(
            disk.find(b"README", 0, u64::MAX, false).unwrap(),
            SearchProgress::NotFound
        );
        assert_eq!(
            disk.find(b"README", 0, u64::MAX, true).unwrap(),
            SearchProgress::Found { offset: 700 }
        );
    }

    #[test]
    fn stops_at_the_limit_and_says_where_to_continue() {
        let disk = disk_with(&[(3 * CHUNK + 10, b"LATE")], 4 * CHUNK);
        let limit = CHUNK as u64;
        let mut from = 0;
        let mut calls = 0;
        let found = loop {
            calls += 1;
            match disk.find(b"LATE", from, limit, false).unwrap() {
                SearchProgress::Continue { next } => from = next,
                SearchProgress::Found { offset } => break offset,
                SearchProgress::NotFound => panic!("not found"),
            }
        };
        assert_eq!(found, (3 * CHUNK + 10) as u64);
        assert_eq!(calls, 4);
    }

    #[test]
    fn a_match_just_past_the_limit_is_found_by_the_next_call() {
        let disk = disk_with(&[(1000, b"EDGE")], 8192);
        assert_eq!(
            disk.find(b"EDGE", 0, 999, false).unwrap(),
            SearchProgress::Continue { next: 999 }
        );
        assert_eq!(
            disk.find(b"EDGE", 999, 999, false).unwrap(),
            SearchProgress::Found { offset: 1000 }
        );
    }
}
