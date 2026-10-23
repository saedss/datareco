//! What a raw scan found, as a volume: a folder per kind of file, so it can
//! be browsed, previewed and recovered like any other.

use std::sync::Arc;

use datareco_fs::{Entry, EntryId, Extent, FileSystem, FsError, FsKind, VolumeInfo, attributes};
use datareco_io::Disk;

use super::RawFile;

/// Files found by their contents.
pub struct RawVolume {
    disk: Arc<Disk>,
    info: VolumeInfo,
    /// Files by extension, each sorted by where it is.
    groups: Vec<(String, Vec<RawFile>)>,
}

impl RawVolume {
    pub(crate) fn new(
        disk: Arc<Disk>,
        start: u64,
        end: u64,
        extensions: &[String],
        files: Vec<RawFile>,
    ) -> RawVolume {
        let mut groups: Vec<(String, Vec<RawFile>)> = Vec::new();
        for file in files {
            let extension = extensions
                .get(file.extension())
                .cloned()
                .unwrap_or_default();
            match groups.iter_mut().find(|(e, _)| *e == extension) {
                Some((_, list)) => list.push(file),
                None => groups.push((extension, vec![file])),
            }
        }
        groups.sort_by(|a, b| a.0.cmp(&b.0));
        RawVolume {
            disk,
            info: VolumeInfo {
                kind: FsKind::Raw,
                name: "Files found by their contents".to_owned(),
                label: None,
                offset: start,
                size: end - start,
                cluster_size: 512,
            },
            groups,
        }
    }

    /// How many files were found.
    pub fn file_count(&self) -> usize {
        self.groups.iter().map(|(_, files)| files.len()).sum()
    }

    /// How many files of each kind were found, by extension.
    pub fn counts(&self) -> Vec<(String, usize)> {
        self.groups
            .iter()
            .map(|(e, files)| (e.clone(), files.len()))
            .collect()
    }

    fn file_entry(extension: &str, file: RawFile) -> Entry {
        let sector = file.offset() / 512;
        let note = if file.exact() {
            format!(
                "Found by its contents at sector {sector}. Its name, folder and times are lost, \
                 and if it was fragmented, only its first piece is right."
            )
        } else {
            format!(
                "Found by its contents at sector {sector}. Its end wasn't found, so it was cut \
                 where the next file starts (or at the largest size for its kind)."
            )
        };
        Entry {
            id: EntryId::Span {
                offset: file.offset(),
                length: file.size(),
            },
            name: format!("f{sector:010}.{extension}"),
            is_dir: false,
            size: file.size(),
            deleted: false,
            created: None,
            modified: None,
            accessed: None,
            attributes: 0,
            streams: Vec::new(),
            note: Some(note),
        }
    }
}

impl FileSystem for RawVolume {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn root(&self) -> EntryId {
        EntryId::Group { index: 0 }
    }

    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError> {
        match dir {
            EntryId::Group { index: 0 } => Ok(self
                .groups
                .iter()
                .enumerate()
                .map(|(i, (extension, files))| Entry {
                    id: EntryId::Group {
                        index: u32::try_from(i + 1).unwrap_or(u32::MAX),
                    },
                    name: extension.to_uppercase(),
                    is_dir: true,
                    size: 0,
                    deleted: false,
                    created: None,
                    modified: None,
                    accessed: None,
                    attributes: attributes::DIRECTORY,
                    streams: Vec::new(),
                    note: Some(format!(
                        "{} .{extension} files found by their contents.",
                        files.len()
                    )),
                })
                .collect()),
            EntryId::Group { index } => {
                let (extension, files) = usize::try_from(*index)
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| self.groups.get(i))
                    .ok_or_else(|| FsError::NotFound("That folder".to_owned()))?;
                Ok(files
                    .iter()
                    .map(|&f| Self::file_entry(extension, f))
                    .collect())
            }
            _ => Err(FsError::NotFound("That folder".to_owned())),
        }
    }

    fn read(
        &self,
        file: &EntryId,
        stream: Option<&str>,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        let EntryId::Span {
            offset: start,
            length,
        } = file
        else {
            return Err(FsError::NotFound("That file".to_owned()));
        };
        if stream.is_some() {
            return Err(FsError::NotFound(
                "Found files have no streams; that stream".to_owned(),
            ));
        }
        if offset >= *length || buf.is_empty() {
            return Ok(0);
        }
        let n = usize::try_from(length - offset)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        self.disk.read_at(start + offset, &mut buf[..n])?;
        Ok(n)
    }

    fn extents(&self, file: &EntryId, _stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        match file {
            EntryId::Span { offset, length } => Ok(vec![Extent {
                offset: *offset,
                length: *length,
                sparse: false,
            }]),
            _ => Err(FsError::NotFound("That file".to_owned())),
        }
    }
}
