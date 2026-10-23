//! NTFS volumes for tests: 512-byte sectors, 4 KiB clusters, 1 KiB MFT
//! records, and an MFT of 64 records in two fragments. Like mkntfs, the MFT
//! starts 16 KiB in and its mirror is in the middle of the volume.

use super::{STAMP, put};
use crate::attributes;

pub const CLUSTER: usize = 4096;
const RECORD: usize = 1024;
const MFT_RECORDS: usize = 64;
/// Where the builder's files start (records 16-23 are reserved).
const FIRST_FILE: usize = 24;
pub const ROOT: u64 = 5;
const ROOT_REFERENCE: u64 = ROOT | (5 << 48);
/// A compression unit: 16 clusters.
const UNIT: usize = 16 * CLUSTER;
/// Data up to this size is stored in the MFT record.
const RESIDENT_LIMIT: usize = 600;

const STANDARD_INFORMATION: u32 = 0x10;
const ATTRIBUTE_LIST: u32 = 0x20;
const FILE_NAME: u32 = 0x30;
const VOLUME_NAME: u32 = 0x60;
const DATA: u32 = 0x80;

const COMPRESSED: u16 = 0x0001;
const SPARSE: u16 = 0x8000;

/// File-name namespaces.
const POSIX: u8 = 0;
const WIN32: u8 = 1;
const DOS: u8 = 2;

/// `STAMP` as a Windows FILETIME.
pub const FILETIME: u64 = (STAMP.unsigned_abs() + 11_644_473_600) * 10_000_000;

/// Runs: `count` clusters at `lcn`, or sparse.
pub type Runs = [(Option<usize>, usize)];

#[derive(Clone)]
enum Body {
    Resident(Vec<u8>),
    NonResident {
        start_vcn: usize,
        runs: Vec<(Option<usize>, usize)>,
        allocated: u64,
        size: u64,
        initialized: u64,
        compression_unit: u8,
    },
}

#[derive(Clone)]
struct Attr {
    kind: u32,
    name: String,
    flags: u16,
    body: Body,
}

#[derive(Clone)]
struct Record {
    sequence: u16,
    flags: u16,
    base: u64,
    attrs: Vec<Attr>,
}

pub struct NtfsBuilder {
    image: Vec<u8>,
    clusters: usize,
    next_cluster: usize,
    bitmap: Vec<u8>,
    records: Vec<Option<Record>>,
    next_record: usize,
    mft_runs: Vec<(usize, usize)>,
}

impl NtfsBuilder {
    pub fn new(clusters: usize) -> Self {
        let mft_runs = vec![(4, 8), (clusters - 16, 8)];
        let mut b = NtfsBuilder {
            image: vec![0; clusters * CLUSTER],
            clusters,
            next_cluster: 12,
            bitmap: vec![0; clusters.div_ceil(8)],
            records: vec![None; MFT_RECORDS],
            next_record: FIRST_FILE,
            mft_runs: mft_runs.clone(),
        };
        // Boot sector, $Bitmap's data and the MFT; the mirror; the backup boot sector.
        for c in (0..12)
            .chain([clusters / 2])
            .chain(clusters - 16..clusters - 8)
            .chain([clusters - 1])
        {
            b.mark(c, true);
        }
        let hidden = attributes::HIDDEN | attributes::SYSTEM;
        let mft_size = (MFT_RECORDS * RECORD) as u64;
        let mft_runs: Vec<_> = mft_runs
            .iter()
            .map(|&(lcn, count)| (Some(lcn), count))
            .collect();
        b.system(
            0,
            "$MFT",
            hidden,
            data_runs("", &mft_runs, mft_size, mft_size, 0, 0),
        );
        b.system(
            1,
            "$MFTMirr",
            hidden,
            data_runs("", &[(Some(clusters / 2), 1)], 4096, 4096, 0, 0),
        );
        let bitmap_size = b.bitmap.len() as u64;
        b.system(
            6,
            "$Bitmap",
            hidden,
            data_runs("", &[(Some(3), 1)], bitmap_size, bitmap_size, 0, 0),
        );
        b.records[3] = Some(Record {
            sequence: 3,
            flags: 1,
            base: 0,
            attrs: vec![
                standard(hidden),
                name_attr(ROOT_REFERENCE, "$Volume", 3, false, 0),
            ],
        });
        b.records[5] = Some(Record {
            sequence: 5,
            flags: 3,
            base: 0,
            attrs: vec![standard(hidden), name_attr(ROOT_REFERENCE, ".", 3, true, 0)],
        });
        b
    }

    fn system(&mut self, n: usize, name: &str, flags: u32, data: Attr) {
        self.records[n] = Some(Record {
            sequence: u16::try_from(n.max(1)).unwrap(),
            flags: 1,
            base: 0,
            attrs: vec![
                standard(flags),
                name_attr(ROOT_REFERENCE, name, 3, false, 0),
                data,
            ],
        });
    }

    fn record(&mut self, n: u64) -> &mut Record {
        self.records[usize::try_from(n).unwrap()].as_mut().unwrap()
    }

    /// A reference to record `n` with its current sequence number.
    fn reference(&self, n: u64) -> u64 {
        let record = self.records[usize::try_from(n).unwrap()].as_ref().unwrap();
        n | (u64::from(record.sequence) << 48)
    }

    pub fn label(&mut self, label: &str) {
        let value = label.encode_utf16().flat_map(u16::to_le_bytes).collect();
        self.record(3).attrs.push(resident(VOLUME_NAME, "", value));
    }

    /// Sets or clears a cluster's bit in `$Bitmap`.
    pub fn mark(&mut self, cluster: usize, used: bool) {
        if used {
            self.bitmap[cluster / 8] |= 1 << (cluster % 8);
        } else {
            self.bitmap[cluster / 8] &= !(1 << (cluster % 8));
        }
    }

    /// The next `count` free clusters, in one piece; the first one's number.
    pub fn alloc(&mut self, count: usize) -> usize {
        // Skip the mirror's cluster.
        let mirror = self.clusters / 2;
        if self.next_cluster <= mirror && self.next_cluster + count > mirror {
            self.next_cluster = mirror + 1;
        }
        let first = self.next_cluster;
        self.next_cluster += count;
        assert!(self.next_cluster < self.clusters - 16, "volume full");
        for c in first..first + count {
            self.mark(c, true);
        }
        first
    }

    fn new_record(&mut self) -> u64 {
        let n = self.next_record;
        assert!(n < MFT_RECORDS, "MFT full");
        self.next_record += 1;
        n as u64
    }

    fn write_runs(&mut self, runs: &Runs, data: &[u8]) {
        let mut vcn = 0;
        for &(lcn, count) in runs {
            let start = (vcn * CLUSTER).min(data.len());
            let end = ((vcn + count) * CLUSTER).min(data.len());
            if let Some(lcn) = lcn {
                put(&mut self.image, lcn * CLUSTER, &data[start..end]);
            }
            vcn += count;
        }
    }

    /// Main or named data: in the record if small, else in new clusters.
    fn data(&mut self, name: &str, data: &[u8]) -> Attr {
        if data.len() <= RESIDENT_LIMIT {
            return resident(DATA, name, data.to_vec());
        }
        let count = data.len().div_ceil(CLUSTER);
        let runs = [(Some(self.alloc(count)), count)];
        self.write_runs(&runs, data);
        let size = data.len() as u64;
        data_runs(name, &runs, size, size, 0, 0)
    }

    fn file_record(&mut self, parent: u64, name: &str, data: &[u8]) -> Record {
        let data = self.data("", data);
        Record {
            sequence: 1,
            flags: 1,
            base: 0,
            attrs: vec![
                standard(attributes::ARCHIVE),
                name_attr(self.reference(parent), name, WIN32, false, size_of(&data)),
                data,
            ],
        }
    }

    pub fn file(&mut self, parent: u64, name: &str, data: &[u8]) -> u64 {
        let n = self.new_record();
        let record = self.file_record(parent, name, data);
        self.records[usize::try_from(n).unwrap()] = Some(record);
        n
    }

    /// A file stored in the given runs, with all bytes from `initialized` on
    /// unwritten (they must read as zeros).
    pub fn file_runs(
        &mut self,
        parent: u64,
        name: &str,
        data: &[u8],
        runs: &Runs,
        initialized: u64,
    ) -> u64 {
        self.write_runs(runs, data);
        let sparse = runs.iter().any(|(lcn, _)| lcn.is_none());
        let size = data.len() as u64;
        let n = self.new_record();
        let mut flags = attributes::ARCHIVE;
        if sparse {
            flags |= attributes::SPARSE;
        }
        let parent = self.reference(parent);
        self.records[usize::try_from(n).unwrap()] = Some(Record {
            sequence: 1,
            flags: 1,
            base: 0,
            attrs: vec![
                standard(flags),
                name_attr(parent, name, WIN32, false, size),
                data_runs(
                    "",
                    runs,
                    size,
                    initialized,
                    if sparse { SPARSE } else { 0 },
                    0,
                ),
            ],
        });
        n
    }

    /// A file compressed the way Windows does: each 64 KiB unit LZNT1-packed
    /// into fewer clusters with the rest sparse, stored as is if that saves
    /// nothing, or left all sparse if it's all zeros.
    pub fn compressed_file(&mut self, parent: u64, name: &str, data: &[u8]) -> u64 {
        let mut runs: Vec<(Option<usize>, usize)> = Vec::new();
        for piece in data.chunks(UNIT) {
            if piece.iter().all(|&b| b == 0) {
                runs.push((None, 16));
                continue;
            }
            let packed = super::lznt1(piece);
            let count = packed.len().div_ceil(CLUSTER);
            if count < 16 {
                let lcn = self.alloc(count);
                put(&mut self.image, lcn * CLUSTER, &packed);
                runs.extend([(Some(lcn), count), (None, 16 - count)]);
            } else {
                let lcn = self.alloc(16);
                put(&mut self.image, lcn * CLUSTER, piece);
                runs.push((Some(lcn), 16));
            }
        }
        let size = data.len() as u64;
        let n = self.new_record();
        let parent = self.reference(parent);
        self.records[usize::try_from(n).unwrap()] = Some(Record {
            sequence: 1,
            flags: 1,
            base: 0,
            attrs: vec![
                standard(attributes::ARCHIVE | attributes::COMPRESSED),
                name_attr(parent, name, WIN32, false, size),
                data_runs("", &runs, size, size, COMPRESSED, 4),
            ],
        });
        n
    }

    /// A file in two fragments whose second run list is in an extension
    /// record, found through the base record's attribute list.
    pub fn split_file(&mut self, parent: u64, name: &str, data: &[u8]) -> (u64, u64) {
        let clusters = data.len().div_ceil(CLUSTER);
        let half = clusters / 2;
        let a = self.alloc(half);
        self.alloc(1);
        let c = self.alloc(clusters - half);
        let runs = [(Some(a), half), (Some(c), clusters - half)];
        self.write_runs(&runs, data);
        let base = self.new_record();
        let extension = self.new_record();
        let size = data.len() as u64;
        let mut first = data_runs("", &runs[..1], size, size, 0, 0);
        if let Body::NonResident { allocated, .. } = &mut first.body {
            *allocated = (clusters * CLUSTER) as u64;
        }
        let second = Attr {
            kind: DATA,
            name: String::new(),
            flags: 0,
            body: Body::NonResident {
                start_vcn: half,
                runs: runs[1..].to_vec(),
                allocated: 0,
                size: 0,
                initialized: 0,
                compression_unit: 0,
            },
        };
        let entry = |kind: u32, record: u64, start_vcn: usize| {
            let mut e = vec![0u8; 32];
            e[..4].copy_from_slice(&kind.to_le_bytes());
            e[4..6].copy_from_slice(&32u16.to_le_bytes());
            e[7] = 26;
            e[8..16].copy_from_slice(&(start_vcn as u64).to_le_bytes());
            e[16..24].copy_from_slice(&(record | (1 << 48)).to_le_bytes());
            e
        };
        let list = [
            entry(STANDARD_INFORMATION, base, 0),
            entry(FILE_NAME, base, 0),
            entry(DATA, base, 0),
            entry(DATA, extension, half),
        ]
        .concat();
        let parent = self.reference(parent);
        self.records[usize::try_from(base).unwrap()] = Some(Record {
            sequence: 1,
            flags: 1,
            base: 0,
            attrs: vec![
                standard(attributes::ARCHIVE),
                resident(ATTRIBUTE_LIST, "", list),
                name_attr(parent, name, WIN32, false, size),
                first,
            ],
        });
        self.records[usize::try_from(extension).unwrap()] = Some(Record {
            sequence: 1,
            flags: 1,
            base: base | (1 << 48),
            attrs: vec![second],
        });
        (base, extension)
    }

    pub fn mkdir(&mut self, parent: u64, name: &str) -> u64 {
        let n = self.new_record();
        let parent = self.reference(parent);
        self.records[usize::try_from(n).unwrap()] = Some(Record {
            sequence: 1,
            flags: 3,
            base: 0,
            attrs: vec![standard(0), name_attr(parent, name, WIN32, true, 0)],
        });
        n
    }

    /// An alternate data stream.
    pub fn stream(&mut self, file: u64, name: &str, data: &[u8]) {
        let attr = self.data(name, data);
        self.record(file).attrs.push(attr);
    }

    /// Another name for a file, in another folder (a hard link).
    pub fn hard_link(&mut self, file: u64, parent: u64, name: &str) {
        let attr = name_attr(self.reference(parent), name, POSIX, false, 0);
        self.record(file).attrs.push(attr);
    }

    /// A DOS 8.3 name, placed before the long name in the record.
    pub fn dos_name(&mut self, file: u64, parent: u64, name: &str) {
        let attr = name_attr(self.reference(parent), name, DOS, false, 0);
        self.record(file).attrs.insert(1, attr);
    }

    /// Deletes like Windows does: the record (and its extension records) is
    /// marked free and its sequence number bumped; its clusters are freed.
    pub fn delete(&mut self, n: u64) {
        let mut freed = Vec::new();
        for (number, slot) in self.records.iter_mut().enumerate() {
            let Some(record) = slot else { continue };
            if number as u64 != n && record.base & 0xFFFF_FFFF_FFFF != n {
                continue;
            }
            record.flags &= !1;
            record.sequence += 1;
            for attr in &record.attrs {
                if let Body::NonResident { runs, .. } = &attr.body {
                    freed.extend(
                        runs.iter()
                            .filter_map(|&(lcn, count)| lcn.map(|lcn| lcn..lcn + count)),
                    );
                }
            }
        }
        for c in freed.into_iter().flatten() {
            self.mark(c, false);
        }
    }

    /// Puts a new file in a deleted file's record, as Windows does when it
    /// reuses a free record. The sequence number stays the bumped one.
    pub fn reuse(&mut self, n: u64, parent: u64, name: &str, data: &[u8]) {
        let sequence = self.record(n).sequence;
        let mut record = self.file_record(parent, name, data);
        record.sequence = sequence;
        self.records[usize::try_from(n).unwrap()] = Some(record);
    }

    /// Where record `n` is in the image.
    pub fn record_offset(&self, n: u64) -> usize {
        let byte = usize::try_from(n).unwrap() * RECORD;
        let mut vcn = 0;
        for &(lcn, count) in &self.mft_runs {
            if byte < (vcn + count) * CLUSTER {
                return lcn * CLUSTER + byte - vcn * CLUSTER;
            }
            vcn += count;
        }
        panic!("record {n} is past the MFT");
    }

    pub fn build(mut self) -> Vec<u8> {
        let mut boot = [0u8; 512];
        boot[..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 8;
        boot[21] = 0xF8;
        boot[40..48].copy_from_slice(&(self.clusters as u64 * 8 - 1).to_le_bytes());
        boot[48..56].copy_from_slice(&4u64.to_le_bytes());
        boot[56..64].copy_from_slice(&(self.clusters as u64 / 2).to_le_bytes());
        boot[64] = 0xF6; // 2^10-byte records
        boot[68] = 1;
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);
        put(&mut self.image, 0, &boot);
        let end = self.image.len() - 512;
        put(&mut self.image, end, &boot);
        let bitmap = self.bitmap.clone();
        put(&mut self.image, 3 * CLUSTER, &bitmap);
        for n in 0..MFT_RECORDS {
            if let Some(record) = &self.records[n] {
                let bytes = serialize(n, record);
                let at = self.record_offset(n as u64);
                put(&mut self.image, at, &bytes);
                if n < 4 {
                    put(
                        &mut self.image,
                        self.clusters / 2 * CLUSTER + n * RECORD,
                        &bytes,
                    );
                }
            }
        }
        self.image
    }
}

fn size_of(attr: &Attr) -> u64 {
    match &attr.body {
        Body::Resident(value) => value.len() as u64,
        Body::NonResident { size, .. } => *size,
    }
}

fn resident(kind: u32, name: &str, value: Vec<u8>) -> Attr {
    Attr {
        kind,
        name: name.to_owned(),
        flags: 0,
        body: Body::Resident(value),
    }
}

fn data_runs(
    name: &str,
    runs: &Runs,
    size: u64,
    initialized: u64,
    flags: u16,
    compression_unit: u8,
) -> Attr {
    let clusters: usize = runs.iter().map(|(_, count)| count).sum();
    Attr {
        kind: DATA,
        name: name.to_owned(),
        flags,
        body: Body::NonResident {
            start_vcn: 0,
            runs: runs.to_vec(),
            allocated: (clusters * CLUSTER) as u64,
            size,
            initialized,
            compression_unit,
        },
    }
}

fn standard(flags: u32) -> Attr {
    let mut value = vec![0u8; 72];
    for at in [0, 8, 16, 24] {
        value[at..at + 8].copy_from_slice(&FILETIME.to_le_bytes());
    }
    value[32..36].copy_from_slice(&flags.to_le_bytes());
    resident(STANDARD_INFORMATION, "", value)
}

fn name_attr(parent: u64, name: &str, namespace: u8, is_dir: bool, size: u64) -> Attr {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut value = vec![0u8; 66 + 2 * units.len()];
    value[..8].copy_from_slice(&parent.to_le_bytes());
    for at in [8, 16, 24, 32] {
        value[at..at + 8].copy_from_slice(&FILETIME.to_le_bytes());
    }
    value[40..48].copy_from_slice(&size.to_le_bytes());
    value[48..56].copy_from_slice(&size.to_le_bytes());
    let flags: u32 = if is_dir { 0x1000_0000 } else { 0x20 };
    value[56..60].copy_from_slice(&flags.to_le_bytes());
    value[64] = u8::try_from(units.len()).unwrap();
    value[65] = namespace;
    for (i, unit) in units.iter().enumerate() {
        value[66 + 2 * i..68 + 2 * i].copy_from_slice(&unit.to_le_bytes());
    }
    resident(FILE_NAME, "", value)
}

fn serialize(n: usize, record: &Record) -> Vec<u8> {
    let mut r = vec![0u8; RECORD];
    r[..4].copy_from_slice(b"FILE");
    r[4..6].copy_from_slice(&48u16.to_le_bytes());
    r[6..8].copy_from_slice(&3u16.to_le_bytes());
    r[16..18].copy_from_slice(&record.sequence.to_le_bytes());
    let links = record.attrs.iter().filter(|a| a.kind == FILE_NAME).count();
    r[18..20].copy_from_slice(&u16::try_from(links).unwrap().to_le_bytes());
    r[20..22].copy_from_slice(&56u16.to_le_bytes());
    r[22..24].copy_from_slice(&record.flags.to_le_bytes());
    r[28..32].copy_from_slice(&u32::try_from(RECORD).unwrap().to_le_bytes());
    r[32..40].copy_from_slice(&record.base.to_le_bytes());
    r[44..48].copy_from_slice(&u32::try_from(n).unwrap().to_le_bytes());
    let mut attrs = record.attrs.clone();
    attrs.sort_by_key(|a| a.kind);
    let mut at = 56;
    for (id, attr) in attrs.iter().enumerate() {
        let bytes = encode(attr, u16::try_from(id).unwrap());
        put(&mut r, at, &bytes);
        at += bytes.len();
    }
    assert!(at + 8 <= RECORD, "record {n} is full");
    put(&mut r, at, &0xFFFF_FFFFu32.to_le_bytes());
    at += 8;
    r[24..28].copy_from_slice(&u32::try_from(at).unwrap().to_le_bytes());
    r[40..42].copy_from_slice(&u16::try_from(attrs.len()).unwrap().to_le_bytes());
    // Fixups: the last two bytes of each 512 go to the update sequence array.
    let usn = [0x34, 0x12];
    for i in 1..3 {
        let end = i * 512 - 2;
        let original = [r[end], r[end + 1]];
        r[48 + 2 * i..50 + 2 * i].copy_from_slice(&original);
        r[end..end + 2].copy_from_slice(&usn);
    }
    r[48..50].copy_from_slice(&usn);
    r
}

fn encode(attr: &Attr, id: u16) -> Vec<u8> {
    let units: Vec<u16> = attr.name.encode_utf16().collect();
    let header = match &attr.body {
        Body::Resident(_) => 24,
        Body::NonResident {
            compression_unit: 0,
            ..
        } => 64,
        Body::NonResident { .. } => 72,
    };
    let after_name = (header + 2 * units.len()).next_multiple_of(8);
    let tail = match &attr.body {
        Body::Resident(value) => value.clone(),
        Body::NonResident { runs, .. } => encode_runs(runs),
    };
    let mut out = vec![0u8; (after_name + tail.len()).next_multiple_of(8)];
    out[..4].copy_from_slice(&attr.kind.to_le_bytes());
    let length = u32::try_from(out.len()).unwrap();
    out[4..8].copy_from_slice(&length.to_le_bytes());
    out[9] = u8::try_from(units.len()).unwrap();
    out[10..12].copy_from_slice(&u16::try_from(header).unwrap().to_le_bytes());
    out[12..14].copy_from_slice(&attr.flags.to_le_bytes());
    out[14..16].copy_from_slice(&id.to_le_bytes());
    for (i, unit) in units.iter().enumerate() {
        out[header + 2 * i..header + 2 * i + 2].copy_from_slice(&unit.to_le_bytes());
    }
    let offset = u16::try_from(after_name).unwrap();
    match &attr.body {
        Body::Resident(value) => {
            out[16..20].copy_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
            out[20..22].copy_from_slice(&offset.to_le_bytes());
            out[22] = u8::from(attr.kind == FILE_NAME);
        }
        Body::NonResident {
            start_vcn,
            runs,
            allocated,
            size,
            initialized,
            compression_unit,
        } => {
            let clusters: usize = runs.iter().map(|(_, count)| count).sum();
            out[8] = 1;
            out[16..24].copy_from_slice(&(*start_vcn as u64).to_le_bytes());
            out[24..32].copy_from_slice(&((start_vcn + clusters) as u64 - 1).to_le_bytes());
            out[32..34].copy_from_slice(&offset.to_le_bytes());
            out[34..36].copy_from_slice(&u16::from(*compression_unit).to_le_bytes());
            out[40..48].copy_from_slice(&allocated.to_le_bytes());
            out[48..56].copy_from_slice(&size.to_le_bytes());
            out[56..64].copy_from_slice(&initialized.to_le_bytes());
            if *compression_unit > 0 {
                let stored: usize = runs
                    .iter()
                    .filter(|(lcn, _)| lcn.is_some())
                    .map(|(_, count)| count)
                    .sum();
                out[64..72].copy_from_slice(&((stored * CLUSTER) as u64).to_le_bytes());
            }
        }
    }
    out[after_name..after_name + tail.len()].copy_from_slice(&tail);
    out
}

fn encode_runs(runs: &Runs) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous: i64 = 0;
    for &(lcn, count) in runs {
        let length = minimal(i64::try_from(count).unwrap());
        let offset = match lcn {
            Some(lcn) => {
                let lcn = i64::try_from(lcn).unwrap();
                let delta = minimal(lcn - previous);
                previous = lcn;
                delta
            }
            None => Vec::new(),
        };
        out.push(u8::try_from((offset.len() << 4) | length.len()).unwrap());
        out.extend(length);
        out.extend(offset);
    }
    out.push(0);
    out
}

/// The fewest little-endian bytes that hold `value` as a signed number.
fn minimal(value: i64) -> Vec<u8> {
    let bytes = value.to_le_bytes();
    for n in 1..=8 {
        let fill = if bytes[n - 1] & 0x80 != 0 { 0xFF } else { 0 };
        let mut raw = [fill; 8];
        raw[..n].copy_from_slice(&bytes[..n]);
        if i64::from_le_bytes(raw) == value {
            return bytes[..n].to_vec();
        }
    }
    bytes.to_vec()
}
