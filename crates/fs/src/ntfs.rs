//! NTFS. Layout: the Linux-NTFS project's "NTFS documentation".
//!
//! Folders are listed from one scan of the whole MFT (done the first time
//! anything is listed) rather than from their indexes: deleted files keep
//! their MFT records until they're reused, so they show up too, and so do
//! files whose folder is gone ("Lost files").

mod lznt1;
pub(crate) mod record;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::bytes::{u8_at, u16_at, u64_at, utf16_text};
use crate::error::FsError;
use crate::volume::Volume;
use crate::{Entry, EntryId, Extent, FileSystem, FsKind, StreamInfo, VolumeInfo, attributes};
use record::{FileName, Run, StandardInfo, Value};

/// Well-known records.
const MFT: u64 = 0;
const VOLUME: u64 = 3;
const ROOT: u64 = 5;
const BITMAP: u64 = 6;

/// The scan reads the MFT this many bytes at a time.
const SCAN_CHUNK: u64 = 1 << 20;
/// Attribute lists longer than this are garbage.
const MAX_LIST: u64 = 16 << 20;
const MAX_BITMAP: u64 = 512 << 20;
/// Clusters checked when telling whether a deleted file was overwritten.
const MAX_CHECKED_CLUSTERS: u64 = 1 << 24;
/// Compression units larger than this are garbage.
const MAX_UNIT: u64 = 4 << 20;

/// What a scan of the disk found, for opening a damaged volume.
#[derive(Default)]
pub(crate) struct Options {
    /// A boot sector to use instead of the volume's first sector (its backup).
    pub boot: Option<Vec<u8>>,
    /// MFT records found on the disk: record number → offset in the volume.
    /// Used when the MFT itself doesn't have a readable copy.
    pub found: BTreeMap<u64, u64>,
}

pub(crate) struct Ntfs {
    volume: Volume,
    info: VolumeInfo,
    cluster_size: u64,
    /// Clusters in the volume, from its boot sector. NTFS counts one sector
    /// less than its partition, so the backup boot sector is past them.
    clusters: u64,
    record_size: u64,
    /// The MFT itself (`$MFT`'s data). Empty if it can't be located.
    mft: Stream,
    /// Where the MFT mirror (copies of records 0-3) is, in the volume.
    mirror: Option<u64>,
    found: BTreeMap<u64, u64>,
    index: OnceLock<Result<Index, String>>,
    /// The last decompressed compression unit, keyed by record, stream and unit.
    unit: Mutex<Option<(UnitKey, Arc<Vec<u8>>)>>,
}

type UnitKey = (u64, String, u64);

/// A record's bytes, and where it is on the disk if known.
type Located = (Vec<u8>, Option<u64>);

/// A data stream, joined from the pieces in all of a file's records.
#[derive(Clone, Debug)]
struct Stream {
    name: String,
    size: u64,
    flags: u16,
    data: Data,
}

#[derive(Clone, Debug)]
enum Data {
    /// Stored in the MFT record; `at` is where on the disk, if known.
    Resident { bytes: Vec<u8>, at: Option<u64> },
    Runs {
        runs: Vec<Run>,
        initialized: u64,
        compression_unit: u8,
    },
}

/// A file: its base record joined with its extension records.
struct File {
    number: u64,
    in_use: bool,
    is_dir: bool,
    names: Vec<FileName>,
    standard: Option<StandardInfo>,
    streams: Vec<Stream>,
}

/// What a scan of the whole MFT found.
struct Index {
    children: HashMap<u64, Vec<u64>>,
    /// Records whose folder's record now holds something else.
    orphans: Vec<u64>,
    /// Records whose folder's record is gone, by that folder's record number.
    lost_folders: BTreeMap<u64, Vec<u64>>,
    /// Extension records by base record.
    extensions: HashMap<u64, Vec<u64>>,
    /// `$Bitmap`: one bit per cluster, set if in use.
    bitmap: Option<Vec<u8>>,
}

/// What the scan keeps about each record.
#[derive(Clone, Copy, Default)]
struct Seen {
    sequence: u16,
    valid: bool,
    in_use: bool,
    is_dir: bool,
}

/// `Seen` by record number: a vector for the MFT, a map for records found
/// elsewhere (their numbers can be anything).
#[derive(Default)]
struct SeenMap {
    dense: Vec<Seen>,
    sparse: HashMap<u64, Seen>,
}

impl SeenMap {
    fn get(&self, number: u64) -> Option<&Seen> {
        usize::try_from(number)
            .ok()
            .and_then(|n| self.dense.get(n))
            .filter(|s| s.valid)
            .or_else(|| self.sparse.get(&number))
    }

    fn set(&mut self, number: u64, seen: Seen) {
        match usize::try_from(number) {
            Ok(n) if n < self.dense.len() => self.dense[n] = seen,
            Ok(n) if n == self.dense.len() => self.dense.push(seen),
            _ if seen.valid => {
                self.sparse.insert(number, seen);
            }
            _ => {}
        }
    }
}

/// Where a file's folder reference leads.
enum Parent {
    Found,
    /// The record is there but holds something else now.
    Reused,
    /// No record at all.
    Missing,
}

/// A piece of a `$DATA` attribute, owned.
enum Piece {
    Resident { bytes: Vec<u8>, at: Option<u64> },
    NonResident(record::NonResident),
}

impl Ntfs {
    pub fn open(volume: Volume) -> Result<Ntfs, FsError> {
        Self::open_with(volume, Options::default())
    }

    pub fn open_with(volume: Volume, options: Options) -> Result<Ntfs, FsError> {
        let boot = match options.boot {
            Some(boot) => boot,
            None => volume.read_vec(0, 512)?,
        };
        let damaged = || FsError::damaged("The NTFS boot sector is damaged.");
        let sector = u64::from(u16_at(&boot, 11).ok_or_else(damaged)?);
        let raw = u8_at(&boot, 13).ok_or_else(damaged)?;
        // Up to 128 sectors per cluster directly; above that, 2^(256 - value).
        let per_cluster = if raw <= 128 {
            u64::from(raw)
        } else {
            1u64.checked_shl(256 - u32::from(raw)).ok_or_else(damaged)?
        };
        let cluster_size = sector.checked_mul(per_cluster).ok_or_else(damaged)?;
        if !matches!(sector, 512 | 1024 | 2048 | 4096)
            || !per_cluster.is_power_of_two()
            || cluster_size > 2 << 20
        {
            return Err(damaged());
        }
        let record_size = size_field(u8_at(&boot, 64).ok_or_else(damaged)?, cluster_size)
            .filter(|size| (256..=65_536).contains(size) && size.is_power_of_two())
            .ok_or_else(damaged)?;
        let total_sectors = u64_at(&boot, 40).ok_or_else(damaged)?;
        let mft_lcn = u64_at(&boot, 48).ok_or_else(damaged)?;
        let mirror_lcn = u64_at(&boot, 56).ok_or_else(damaged)?;

        let mut ntfs = Ntfs {
            info: VolumeInfo {
                kind: FsKind::Ntfs,
                name: FsKind::Ntfs.name().to_owned(),
                label: None,
                offset: volume.start(),
                size: total_sectors.saturating_add(1).saturating_mul(sector),
                cluster_size,
            },
            volume,
            cluster_size,
            clusters: total_sectors / per_cluster,
            record_size,
            mft: Stream {
                name: String::new(),
                size: 0,
                flags: 0,
                data: Data::Resident {
                    bytes: Vec::new(),
                    at: None,
                },
            },
            mirror: mirror_lcn.checked_mul(cluster_size),
            found: options.found,
            index: OnceLock::new(),
            unit: Mutex::new(None),
        };
        // If the MFT's first record is damaged, its copy in the MFT mirror says where the MFT is.
        match ntfs
            .locate_mft(mft_lcn)
            .or_else(|_| ntfs.locate_mft(mirror_lcn))
        {
            Ok(mft) => ntfs.mft = mft,
            // Records a scan found elsewhere stand in for a lost MFT.
            Err(_) if !ntfs.found.is_empty() => {}
            Err(error) => return Err(error),
        }
        ntfs.info.label = ntfs.label();
        Ok(ntfs)
    }

    /// `$MFT`'s data, from its record at cluster `lcn`.
    fn locate_mft(&self, lcn: u64) -> Result<Stream, FsError> {
        let damaged = || FsError::damaged("The MFT's first record is damaged.");
        let offset = lcn.checked_mul(self.cluster_size).ok_or_else(damaged)?;
        let mut first = self.volume.read_vec(offset, self.record_size)?;
        if !record::fixup(&mut first) {
            return Err(damaged());
        }
        let mut pieces = data_pieces(None, &first, "");
        // A very fragmented MFT lists the rest of its runs in extension records.
        let list = record::attributes(&first)
            .into_iter()
            .find(|a| a.kind == record::ATTRIBUTE_LIST)
            .map(|a| self.value(&a));
        // An unreadable list leaves the runs record 0 has (usually most of the MFT).
        if let Some(Ok(list)) = list {
            let mut more: Vec<record::ListEntry> = record::attribute_list(&list)
                .into_iter()
                .filter(|e| e.kind == record::DATA && e.name.is_empty() && e.record != MFT)
                .collect();
            more.sort_by_key(|e| e.start_vcn);
            for entry in more {
                let so_far = join(String::new(), &pieces).ok_or_else(damaged)?;
                let Ok(Some(extension)) = self.read_record_in(&so_far, entry.record) else {
                    continue;
                };
                if record::header(&extension).is_some_and(|h| h.base == MFT) {
                    pieces.extend(data_pieces(None, &extension, ""));
                }
            }
        }
        let mft = join(String::new(), &pieces).ok_or_else(damaged)?;
        match &mft.data {
            Data::Runs { runs, .. } if !runs.is_empty() && mft.size >= self.record_size => Ok(mft),
            _ => Err(damaged()),
        }
    }

    fn label(&self) -> Option<String> {
        let (volume, _) = self.read_record(VOLUME).ok()??;
        record::attributes(&volume)
            .iter()
            .find(|a| a.kind == record::VOLUME_NAME)
            .and_then(|a| a.resident())
            .and_then(utf16_text)
    }

    /// An MFT record with its fixups undone, and where it is on the disk;
    /// `None` if there's no readable copy. The MFT's copy comes first, then
    /// one a scan found elsewhere, then (records 0-3) the MFT mirror's.
    fn read_record(&self, number: u64) -> Result<Option<Located>, FsError> {
        let from_mft = self.read_record_in(&self.mft, number);
        if let Ok(Some(record)) = from_mft {
            let at = number
                .checked_mul(self.record_size)
                .and_then(|at| self.mft_offset(at));
            return Ok(Some((record, at)));
        }
        for at in self.other_copies(number) {
            if let Some(mut record) = self.raw_at(at)
                && record::fixup(&mut record)
            {
                return Ok(Some((record, Some(self.volume.absolute(at)))));
            }
        }
        from_mft.map(|_| None)
    }

    /// Other places record `number` may be, in the volume: where a scan found
    /// it, and for records 0-3, the MFT mirror.
    fn other_copies(&self, number: u64) -> Vec<u64> {
        let mut places: Vec<u64> = self.found.get(&number).copied().into_iter().collect();
        if number < 4
            && let Some(mirror) = self.mirror
        {
            places.extend(mirror.checked_add(number * self.record_size));
        }
        places
    }

    /// A record's raw bytes at `offset` in the volume.
    fn raw_at(&self, offset: u64) -> Option<Vec<u8>> {
        self.volume.read_vec(offset, self.record_size).ok()
    }

    fn read_record_in(&self, mft: &Stream, number: u64) -> Result<Option<Vec<u8>>, FsError> {
        let Some(offset) = number
            .checked_mul(self.record_size)
            .filter(|&o| o < mft.size)
        else {
            return Ok(None);
        };
        let mut record = vec![0; usize::try_from(self.record_size).unwrap_or(0)];
        let read = self.read_stream(MFT, mft, offset, &mut record)?;
        Ok((read == record.len() && record::fixup(&mut record)).then_some(record))
    }

    /// An attribute's whole value, resident or not (up to 16 MiB).
    fn value(&self, attribute: &record::Attribute) -> Result<Vec<u8>, FsError> {
        match &attribute.value {
            Value::Resident { data, .. } => Ok(data.to_vec()),
            Value::NonResident(header) => {
                let size = header.size.min(MAX_LIST);
                let stream = Stream {
                    name: String::new(),
                    size,
                    flags: 0,
                    data: Data::Runs {
                        runs: header.runs.clone(),
                        initialized: header.initialized,
                        compression_unit: 0,
                    },
                };
                let mut data = vec![0; usize::try_from(size).unwrap_or(0)];
                let read = self.read_stream(MFT, &stream, 0, &mut data)?;
                data.truncate(read);
                Ok(data)
            }
        }
    }

    fn index(&self) -> Result<&Index, FsError> {
        self.index
            .get_or_init(|| {
                self.scan()
                    .map_err(|e| format!("Reading the MFT failed: {e}"))
            })
            .as_ref()
            .map_err(|message| FsError::damaged(message.clone()))
    }

    /// Reads every MFT record once, noting each file's folders.
    fn scan(&self) -> Result<Index, FsError> {
        let size = self.record_size;
        // Only stored clusters hold records: a damaged size or sparse runs
        // mustn't make the scan read zeros for ever.
        let stored = match &self.mft.data {
            Data::Runs { runs, .. } => runs
                .iter()
                .filter(|r| r.lcn.is_some())
                .fold(0u64, |sum, r| {
                    sum.saturating_add(r.length.saturating_mul(self.cluster_size))
                }),
            Data::Resident { .. } => 0,
        };
        let count = self.mft.size.min(stored).min(self.volume.size()) / size;
        let per_chunk = (SCAN_CHUNK / size).max(1);
        let mut seen = SeenMap::default();
        let mut links: Vec<(u64, u64, u16)> = Vec::new();
        let mut extensions: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut first = 0;
        while first < count {
            let records = per_chunk.min(count - first);
            let mut chunk = vec![0; usize::try_from(records * size).unwrap_or(0)];
            let read = match self.read_stream(MFT, &self.mft, first * size, &mut chunk) {
                Ok(read) => read,
                // Past the end of the volume, or unreadable: the rest of the MFT is lost.
                Err(_) if first > 0 || !self.found.is_empty() => break,
                Err(error) => return Err(error),
            };
            chunk.truncate(read);
            for (i, record) in chunk
                .chunks_exact_mut(usize::try_from(size).unwrap_or(1))
                .enumerate()
            {
                let number = first + i as u64;
                let mut found = scan_record(number, record, &mut links, &mut extensions);
                // A damaged copy in the MFT: try the others.
                for at in self.other_copies(number) {
                    if found.valid {
                        break;
                    }
                    if let Some(mut copy) = self.raw_at(at) {
                        found = scan_record(number, &mut copy, &mut links, &mut extensions);
                    }
                }
                seen.set(number, found);
            }
            if (read as u64) < records * size {
                break;
            }
            first += records;
        }
        // Records found past what's left of the MFT.
        for (&number, &at) in self.found.range(first..) {
            if let Some(mut record) = self.raw_at(at) {
                let found = scan_record(number, &mut record, &mut links, &mut extensions);
                seen.set(number, found);
            }
        }

        links.sort_unstable();
        links.dedup();
        let mut children: HashMap<u64, Vec<u64>> = HashMap::new();
        let mut placed = HashSet::new();
        let mut reused = BTreeSet::new();
        let mut lost_folders: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for (child, parent, sequence) in links {
            if child == parent {
                continue; // the root folder is its own parent
            }
            match parent_state(&seen, parent, sequence) {
                Parent::Found => {
                    children.entry(parent).or_default().push(child);
                    placed.insert(child);
                }
                Parent::Reused => {
                    reused.insert(child);
                }
                Parent::Missing => {
                    lost_folders.entry(parent).or_default().push(child);
                    placed.insert(child);
                }
            }
        }
        for list in children.values_mut().chain(lost_folders.values_mut()) {
            list.dedup();
        }
        Ok(Index {
            children,
            orphans: reused.into_iter().filter(|n| !placed.contains(n)).collect(),
            lost_folders,
            extensions,
            bitmap: self.cluster_bitmap(),
        })
    }

    fn cluster_bitmap(&self) -> Option<Vec<u8>> {
        let (record, at) = self.read_record(BITMAP).ok()??;
        let pieces = data_pieces(at, &record, "");
        let mut stream = join(String::new(), &pieces)?;
        let clusters = self.volume.size() / self.cluster_size;
        stream.size = stream.size.min(MAX_BITMAP).min(clusters.div_ceil(8) + 8);
        let mut bitmap = vec![0; usize::try_from(stream.size).ok()?];
        let read = self.read_stream(BITMAP, &stream, 0, &mut bitmap).ok()?;
        bitmap.truncate(read);
        Some(bitmap)
    }

    /// A file with all its records joined.
    fn load(&self, number: u64) -> Result<File, FsError> {
        let index = self.index()?;
        let missing = || FsError::NotFound(format!("MFT record {number}"));
        let (base, base_at) = self.read_record(number)?.ok_or_else(missing)?;
        let header = record::header(&base).ok_or_else(missing)?;

        let mut others: BTreeSet<u64> = index
            .extensions
            .get(&number)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        if let Some(list) = record::attributes(&base)
            .iter()
            .find(|a| a.kind == record::ATTRIBUTE_LIST)
        {
            let entries = record::attribute_list(&self.value(list).unwrap_or_default());
            others.extend(entries.iter().map(|e| e.record).filter(|&n| n != number));
        }
        let mut records = vec![(base_at, base)];
        for other in others {
            let Ok(Some((extension, at))) = self.read_record(other) else {
                continue;
            };
            let Some(h) = record::header(&extension) else {
                continue;
            };
            // Extension records of a deleted file were freed with it (and
            // their base's sequence number has moved on by one).
            let belongs = h.base == number
                && if header.in_use() {
                    h.in_use() && h.base_sequence == header.sequence
                } else {
                    !h.in_use()
                        && (h.base_sequence == header.sequence
                            || h.base_sequence.wrapping_add(1) == header.sequence)
                };
            if belongs {
                records.push((at, extension));
            }
        }

        let mut names = Vec::new();
        let mut standard = None;
        let mut streams: Vec<(String, Vec<(u16, Piece)>)> = Vec::new();
        for (at, record) in &records {
            for attribute in record::attributes(record) {
                match attribute.kind {
                    record::STANDARD_INFORMATION if standard.is_none() => {
                        standard = attribute.resident().and_then(record::standard_info);
                    }
                    record::FILE_NAME => {
                        names.extend(attribute.resident().and_then(record::file_name))
                    }
                    record::DATA => {
                        let piece = piece(*at, &attribute);
                        match streams.iter_mut().find(|(name, _)| *name == attribute.name) {
                            Some((_, pieces)) => pieces.push((attribute.flags, piece)),
                            None => streams
                                .push((attribute.name.clone(), vec![(attribute.flags, piece)])),
                        }
                    }
                    _ => {}
                }
            }
        }
        let streams = streams
            .into_iter()
            .filter_map(|(name, pieces)| join(name, &pieces))
            .collect();
        let is_dir = header.is_dir()
            || names
                .iter()
                .any(|n| n.flags & record::NAME_IS_DIRECTORY != 0);
        Ok(File {
            number,
            in_use: header.in_use(),
            is_dir,
            names,
            standard,
            streams,
        })
    }

    fn entry(&self, file: &File, parent: Option<u64>, index: &Index) -> Entry {
        // The name in this folder (a file has one per hard link), long names first.
        let pick = |folder: Option<u64>| {
            file.names
                .iter()
                .filter(|n| folder.is_none_or(|p| n.parent == p))
                .min_by_key(|n| n.rank())
                .cloned()
        };
        let name = parent.and_then(|p| pick(Some(p))).or_else(|| pick(None));
        let main = file.streams.iter().find(|s| s.name.is_empty());
        let mut flags = file.standard.map_or(0, |s| s.attributes) & 0xFFFF;
        if file.is_dir {
            flags |= attributes::DIRECTORY;
        }
        let mut notes = Vec::new();
        if !file.in_use {
            let (used, total) = main.map_or((0, 0), |s| self.reused(s, index));
            notes.push(if used > 0 {
                format!(
                    "Deleted. {used} of its {total} clusters are now used by other files, so those parts are probably overwritten."
                )
            } else {
                "Deleted.".to_owned()
            });
        }
        if flags & attributes::ENCRYPTED != 0
            || main.is_some_and(|s| s.flags & record::ENCRYPTED != 0)
        {
            notes.push("Encrypted with EFS: what's read is the encrypted data, which only the owner's key opens.".to_owned());
        }
        let times = file.standard.map(|s| (s.created, s.modified, s.accessed));
        let name_times = name.as_ref().map(|n| (n.created, n.modified, n.accessed));
        let (created, modified, accessed) = times.or(name_times).unwrap_or_default();
        Entry {
            id: EntryId::Record {
                number: file.number,
            },
            name: name.map_or_else(|| format!("(record {})", file.number), |n| n.name),
            is_dir: file.is_dir,
            size: if file.is_dir {
                0
            } else {
                main.map_or(0, |s| s.size)
            },
            deleted: !file.in_use,
            created,
            modified,
            accessed,
            attributes: flags,
            streams: file
                .streams
                .iter()
                .filter(|s| !s.name.is_empty())
                .map(|s| StreamInfo {
                    name: s.name.clone(),
                    size: s.size,
                })
                .collect(),
            note: (!notes.is_empty()).then(|| notes.join(" ")),
        }
    }

    /// How many of a stream's clusters `$Bitmap` says are in use, out of how many.
    fn reused(&self, stream: &Stream, index: &Index) -> (u64, u64) {
        let (Data::Runs { runs, .. }, Some(bitmap)) = (&stream.data, &index.bitmap) else {
            return (0, 0);
        };
        let bits = bitmap.len() as u64 * 8;
        let clusters = stream.size.div_ceil(self.cluster_size);
        let (mut used, mut total, mut checked) = (0u64, 0u64, 0u64);
        for run in runs {
            let Some(lcn) = run.lcn else { continue };
            let length = run.length.min(clusters.saturating_sub(run.vcn));
            total = total.saturating_add(length);
            for cluster in lcn..lcn.saturating_add(length).min(bits) {
                if checked >= MAX_CHECKED_CLUSTERS {
                    break;
                }
                checked += 1;
                let byte = usize::try_from(cluster / 8)
                    .ok()
                    .and_then(|i| bitmap.get(i))
                    .copied()
                    .unwrap_or(0);
                used += u64::from(byte >> (cluster % 8) & 1);
            }
        }
        (used, total)
    }

    fn lost_files_folder() -> Entry {
        virtual_folder(
            EntryId::Orphans,
            crate::LOST_FILES.to_owned(),
            "Files and folders whose folder no longer exists, so where they were is unknown.",
        )
    }

    /// The folders in "Lost files" that stand for a folder whose own record
    /// is gone: its files still name it, so they're kept together.
    fn lost_folder_entries(index: &Index) -> Vec<Entry> {
        index
            .lost_folders
            .keys()
            .map(|&number| {
                virtual_folder(
                    EntryId::LostFolder { number },
                    format!("Folder {number}"),
                    "This folder's own MFT record is gone, so its name is unknown; \
                     what was in it is here.",
                )
            })
            .collect()
    }

    fn entries(&self, numbers: &[u64], parent: Option<u64>) -> Result<Vec<Entry>, FsError> {
        let index = self.index()?;
        let mut entries: Vec<Entry> = numbers
            .iter()
            .filter_map(|&n| self.load(n).ok())
            .map(|file| self.entry(&file, parent, index))
            .collect();
        entries.sort_by_cached_key(|e| (e.name.to_lowercase(), e.deleted));
        Ok(entries)
    }

    fn stream(&self, id: &EntryId, name: Option<&str>) -> Result<(u64, Stream), FsError> {
        let EntryId::Record { number } = id else {
            return Err(FsError::NotFound("That entry's data".to_owned()));
        };
        let file = self.load(*number)?;
        let name = name.unwrap_or("");
        let stream = file
            .streams
            .iter()
            .find(|s| s.name == name)
            .or_else(|| {
                file.streams
                    .iter()
                    .find(|s| s.name.to_lowercase() == name.to_lowercase())
            })
            .cloned()
            .ok_or_else(|| {
                FsError::NotFound(if name.is_empty() {
                    "That entry's data".to_owned()
                } else {
                    format!("The stream \"{name}\"")
                })
            })?;
        Ok((*number, stream))
    }

    /// Reads from a stream of record `owner`. Returns the bytes read.
    fn read_stream(
        &self,
        owner: u64,
        stream: &Stream,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, FsError> {
        if offset >= stream.size || buf.is_empty() {
            return Ok(0);
        }
        let wanted = usize::try_from(stream.size - offset)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let buf = &mut buf[..wanted];
        match &stream.data {
            Data::Resident { bytes, .. } => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX);
                let available = bytes.get(start..).unwrap_or_default();
                let n = available.len().min(wanted);
                buf[..n].copy_from_slice(&available[..n]);
                buf[n..].fill(0);
            }
            Data::Runs {
                runs,
                initialized,
                compression_unit,
            } => {
                if stream.flags & record::COMPRESSED != 0 && *compression_unit > 0 {
                    self.read_compressed(owner, stream, runs, *compression_unit, offset, buf)?;
                } else {
                    self.read_runs(runs, *initialized, offset, buf)?;
                }
            }
        }
        Ok(wanted)
    }

    fn read_runs(
        &self,
        runs: &[Run],
        initialized: u64,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), FsError> {
        let cluster_size = self.cluster_size;
        let mut done = 0;
        while done < buf.len() {
            let position = offset + done as u64;
            let rest = &mut buf[done..];
            if position >= initialized {
                // Written space the file never wrote to reads as zeros.
                rest.fill(0);
                break;
            }
            let vcn = position / cluster_size;
            let run = find_run(runs, vcn).ok_or_else(|| {
                FsError::damaged(
                    "The file's data runs end before the file does (its record is damaged).",
                )
            })?;
            let run_end = run
                .vcn
                .saturating_add(run.length)
                .saturating_mul(cluster_size);
            let n = usize::try_from((run_end - position).min(initialized - position))
                .unwrap_or(usize::MAX)
                .min(rest.len());
            match run.lcn {
                None => rest[..n].fill(0),
                Some(lcn) => {
                    let start = lcn
                        .checked_add(vcn - run.vcn)
                        .and_then(|c| c.checked_mul(cluster_size))
                        .and_then(|b| b.checked_add(position % cluster_size))
                        .ok_or_else(|| {
                            FsError::damaged("A data run points past the end of the volume.")
                        })?;
                    self.volume.read(start, &mut rest[..n])?;
                }
            }
            done += n;
        }
        Ok(())
    }

    fn read_compressed(
        &self,
        owner: u64,
        stream: &Stream,
        runs: &[Run],
        compression_unit: u8,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), FsError> {
        // Windows uses 16-cluster units of at most 64 KiB; allow some slack.
        let unit_bytes = self.cluster_size << compression_unit.min(16);
        if compression_unit > 8 || unit_bytes > MAX_UNIT {
            return Err(FsError::damaged(
                "The file's compression unit makes no sense.",
            ));
        }
        let mut done = 0;
        while done < buf.len() {
            let position = offset + done as u64;
            let unit = position / unit_bytes;
            let within = usize::try_from(position % unit_bytes).unwrap_or(0);
            let data = self.unit(owner, stream, runs, compression_unit, unit)?;
            let n = (data.len() - within).min(buf.len() - done);
            buf[done..done + n].copy_from_slice(&data[within..within + n]);
            done += n;
        }
        Ok(())
    }

    /// One compression unit (16 clusters, usually), decompressed. A unit
    /// whose clusters are all stored isn't compressed; one that's partly
    /// sparse holds LZNT1 data; one that's all sparse is zeros.
    fn unit(
        &self,
        owner: u64,
        stream: &Stream,
        runs: &[Run],
        shift: u8,
        unit: u64,
    ) -> Result<Arc<Vec<u8>>, FsError> {
        let key = (owner, stream.name.clone(), unit);
        {
            let cached = self.unit.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((k, data)) = cached.as_ref()
                && *k == key
            {
                return Ok(Arc::clone(data));
            }
        }
        let clusters = 1u64 << shift;
        let unit_bytes = usize::try_from(self.cluster_size << shift).unwrap_or(0);
        let first = unit.saturating_mul(clusters);
        let lcns: Vec<Option<u64>> = (first..first.saturating_add(clusters))
            .map(|vcn| {
                find_run(runs, vcn).and_then(|r| r.lcn.map(|lcn| lcn.saturating_add(vcn - r.vcn)))
            })
            .collect();
        let size = usize::try_from(self.cluster_size).unwrap_or(0);
        let mut stored = Vec::new();
        for lcn in lcns.iter().flatten() {
            let start = lcn
                .checked_mul(self.cluster_size)
                .ok_or_else(|| FsError::damaged("A data run is damaged."))?;
            let at = stored.len();
            stored.resize(at + size, 0);
            self.volume.read(start, &mut stored[at..])?;
        }
        let data = if stored.is_empty() {
            vec![0; unit_bytes]
        } else if lcns.iter().all(Option::is_some) {
            stored
        } else {
            lznt1::decompress(&stored, unit_bytes)
        };
        let data = Arc::new(data);
        *self.unit.lock().unwrap_or_else(PoisonError::into_inner) = Some((key, Arc::clone(&data)));
        Ok(data)
    }

    /// Where a byte of the MFT is on the disk.
    fn mft_offset(&self, at: u64) -> Option<u64> {
        let Data::Runs { runs, .. } = &self.mft.data else {
            return None;
        };
        let vcn = at / self.cluster_size;
        let run = find_run(runs, vcn)?;
        let cluster = run.lcn?.checked_add(vcn - run.vcn)?;
        let offset = cluster
            .checked_mul(self.cluster_size)?
            .checked_add(at % self.cluster_size)?;
        Some(self.volume.absolute(offset))
    }
}

impl FileSystem for Ntfs {
    fn info(&self) -> &VolumeInfo {
        &self.info
    }

    fn allocation(&self) -> Option<crate::Allocation> {
        let bitmap = self.cluster_bitmap()?;
        let count = self
            .clusters
            .min(self.volume.size() / self.cluster_size)
            .min(bitmap.len() as u64 * 8);
        let mut ranges = crate::Ranges::default();
        // The boot sector, whatever $Bitmap says about cluster 0.
        ranges.add(0, self.cluster_size);
        ranges.add_bits(&bitmap, count, 0, self.cluster_size);
        Some(ranges.finish(count * self.cluster_size))
    }

    fn root(&self) -> EntryId {
        EntryId::Record { number: ROOT }
    }

    fn list(&self, dir: &EntryId) -> Result<Vec<Entry>, FsError> {
        let index = self.index()?;
        match dir {
            EntryId::Record { number } => {
                let children = index.children.get(number).map_or(&[][..], Vec::as_slice);
                let mut entries = self.entries(children, Some(*number))?;
                if *number == ROOT && (!index.orphans.is_empty() || !index.lost_folders.is_empty())
                {
                    entries.push(Self::lost_files_folder());
                }
                Ok(entries)
            }
            EntryId::Orphans => {
                let mut entries = Self::lost_folder_entries(index);
                entries.extend(self.entries(&index.orphans, None)?);
                Ok(entries)
            }
            EntryId::LostFolder { number } => {
                let children = index
                    .lost_folders
                    .get(number)
                    .map_or(&[][..], Vec::as_slice);
                self.entries(children, Some(*number))
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
        let (owner, stream) = self.stream(file, stream)?;
        self.read_stream(owner, &stream, offset, buf)
    }

    fn extents(&self, file: &EntryId, stream: Option<&str>) -> Result<Vec<Extent>, FsError> {
        let (_, stream) = self.stream(file, stream)?;
        let mut extents: Vec<Extent> = Vec::new();
        match &stream.data {
            Data::Resident { bytes, at } => {
                if let Some(offset) = at.filter(|_| !bytes.is_empty()) {
                    extents.push(Extent {
                        offset,
                        length: bytes.len() as u64,
                        sparse: false,
                    });
                }
            }
            Data::Runs { runs, .. } => {
                let mut remaining = stream.size;
                for run in runs {
                    if remaining == 0 {
                        break;
                    }
                    let length = run.length.saturating_mul(self.cluster_size).min(remaining);
                    remaining -= length;
                    let offset = match run.lcn {
                        Some(lcn) => self.volume.absolute(lcn.saturating_mul(self.cluster_size)),
                        None => 0,
                    };
                    match extents.last_mut() {
                        Some(last) if last.sparse && run.lcn.is_none() => last.length += length,
                        Some(last)
                            if !last.sparse
                                && run.lcn.is_some()
                                && last.offset.checked_add(last.length) == Some(offset) =>
                        {
                            last.length += length;
                        }
                        _ => extents.push(Extent {
                            offset,
                            length,
                            sparse: run.lcn.is_none(),
                        }),
                    }
                }
            }
        }
        Ok(extents)
    }
}

/// Notes what one record says about where it belongs.
fn scan_record(
    number: u64,
    record: &mut [u8],
    links: &mut Vec<(u64, u64, u16)>,
    extensions: &mut HashMap<u64, Vec<u64>>,
) -> Seen {
    if !record::fixup(record) {
        return Seen::default();
    }
    let Some(header) = record::header(record) else {
        return Seen::default();
    };
    let seen = Seen {
        sequence: header.sequence,
        valid: true,
        in_use: header.in_use(),
        is_dir: header.is_dir(),
    };
    let owner = if header.base == 0 {
        number
    } else {
        extensions.entry(header.base).or_default().push(number);
        if !header.in_use() {
            return seen;
        }
        header.base
    };
    for attribute in record::attributes(record) {
        if attribute.kind == record::FILE_NAME
            && let Some(name) = attribute.resident().and_then(record::file_name)
        {
            links.push((owner, name.parent, name.parent_sequence));
        }
    }
    seen
}

/// Whether `parent` is still the folder a file name pointed at: a folder
/// whose sequence number matches the reference's, or is one more if the
/// folder was deleted too (deleting a record bumps its sequence number).
/// The root folder is always there, even when its record isn't.
fn parent_state(seen: &SeenMap, parent: u64, sequence: u16) -> Parent {
    if parent == ROOT {
        return Parent::Found;
    }
    let Some(folder) = seen.get(parent).filter(|s| s.valid) else {
        return Parent::Missing;
    };
    let same = folder.is_dir
        && (sequence == 0
            || folder.sequence == sequence
            || (!folder.in_use && folder.sequence == sequence.wrapping_add(1)));
    if same { Parent::Found } else { Parent::Reused }
}

/// A folder that isn't on the disk: "Lost files" and the folders in it.
fn virtual_folder(id: EntryId, name: String, note: &str) -> Entry {
    Entry {
        id,
        name,
        is_dir: true,
        size: 0,
        deleted: false,
        created: None,
        modified: None,
        accessed: None,
        attributes: attributes::DIRECTORY,
        streams: Vec::new(),
        note: Some(note.to_owned()),
    }
}

/// The run holding virtual cluster `vcn`.
fn find_run(runs: &[Run], vcn: u64) -> Option<&Run> {
    let i = runs.partition_point(|r| r.vcn.saturating_add(r.length) <= vcn);
    runs.get(i).filter(|r| r.vcn <= vcn)
}

/// The boot sector's record and index sizes: clusters if positive, else 2^-value bytes.
fn size_field(raw: u8, cluster_size: u64) -> Option<u64> {
    let value = i8::from_le_bytes([raw]);
    if value < 0 {
        1u64.checked_shl(u32::from(value.unsigned_abs()))
    } else {
        cluster_size.checked_mul(u64::from(value.unsigned_abs()))
    }
}

/// An attribute's value as a stream piece. `record_at` is where its record
/// is on the disk, if known.
fn piece(record_at: Option<u64>, attribute: &record::Attribute) -> Piece {
    match &attribute.value {
        Value::Resident { data, offset } => Piece::Resident {
            bytes: data.to_vec(),
            at: record_at.and_then(|at| at.checked_add(*offset as u64)),
        },
        Value::NonResident(header) => Piece::NonResident(header.clone()),
    }
}

/// The pieces of a record's `$DATA` attribute called `name`.
fn data_pieces(record_at: Option<u64>, record: &[u8], name: &str) -> Vec<(u16, Piece)> {
    record::attributes(record)
        .iter()
        .filter(|a| a.kind == record::DATA && a.name == name)
        .map(|a| (a.flags, piece(record_at, a)))
        .collect()
}

/// Joins a stream's pieces: resident data, or runs from all non-resident
/// pieces in VCN order (sizes come from the piece that starts at VCN 0).
fn join(name: String, pieces: &[(u16, Piece)]) -> Option<Stream> {
    if let Some((flags, Piece::Resident { bytes, at })) = pieces
        .iter()
        .find(|(_, p)| matches!(p, Piece::Resident { .. }))
    {
        return Some(Stream {
            name,
            size: bytes.len() as u64,
            flags: *flags,
            data: Data::Resident {
                bytes: bytes.clone(),
                at: *at,
            },
        });
    }
    let mut headers: Vec<(u16, &record::NonResident)> = pieces
        .iter()
        .filter_map(|(flags, p)| match p {
            Piece::NonResident(header) => Some((*flags, header)),
            Piece::Resident { .. } => None,
        })
        .collect();
    headers.sort_by_key(|(_, h)| h.start_vcn);
    headers.dedup_by_key(|(_, h)| h.start_vcn);
    let (flags, first) = *headers.first()?;
    let mut runs: Vec<Run> = headers
        .iter()
        .flat_map(|(_, h)| h.runs.iter().copied())
        .collect();
    runs.sort_by_key(|r| r.vcn);
    Some(Stream {
        name,
        size: first.size,
        flags,
        data: Data::Runs {
            runs,
            initialized: first.initialized.min(first.size),
            compression_unit: first.compression_unit,
        },
    })
}
