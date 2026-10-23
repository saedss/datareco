//! MFT records: fixups, attributes and data runs. Layout: the Linux-NTFS
//! project's "NTFS documentation" (layout.h).

use crate::bytes::{bytes, has, u8_at, u16_at, u32_at, u64_at};
use crate::time;

pub(crate) const STANDARD_INFORMATION: u32 = 0x10;
pub(crate) const ATTRIBUTE_LIST: u32 = 0x20;
pub(crate) const FILE_NAME: u32 = 0x30;
pub(crate) const VOLUME_NAME: u32 = 0x60;
pub(crate) const DATA: u32 = 0x80;
const END: u32 = 0xFFFF_FFFF;

/// Attribute flags.
pub(crate) const COMPRESSED: u16 = 0x0001;
pub(crate) const ENCRYPTED: u16 = 0x4000;

/// Record header flags.
const IN_USE: u16 = 0x0001;
const DIRECTORY: u16 = 0x0002;

/// `FileName::flags`: the file is a folder (it has a file-name index).
pub(crate) const NAME_IS_DIRECTORY: u32 = 0x1000_0000;

/// Fixups protect every 512 bytes of a record, whatever the sector size.
const STRIDE: usize = 512;

/// A file reference: the record number (low 48 bits) and the record's
/// sequence number when the reference was made (high 16 bits).
pub(crate) fn reference(value: u64) -> (u64, u16) {
    (
        value & 0xFFFF_FFFF_FFFF,
        u16::try_from(value >> 48).unwrap_or(0),
    )
}

/// Undoes the fixups that guard a record against half-finished writes.
/// Returns false if this isn't an MFT record. A fixup that doesn't match (a
/// write that was cut off) is undone anyway: most of the record is right.
pub(crate) fn fixup(record: &mut [u8]) -> bool {
    if !has(record, 0, b"FILE") {
        return false;
    }
    let (Some(offset), Some(count)) = (u16_at(record, 4), u16_at(record, 6)) else {
        return false;
    };
    let (offset, count) = (usize::from(offset), usize::from(count));
    if count == 0 || (count - 1) * STRIDE > record.len() {
        return false;
    }
    for i in 1..count {
        let Some(original) = bytes(record, offset + 2 * i, 2).map(|b| [b[0], b[1]]) else {
            return false;
        };
        let at = i * STRIDE - 2;
        let Some(slot) = record.get_mut(at..at + 2) else {
            return false;
        };
        slot.copy_from_slice(&original);
    }
    true
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Header {
    pub sequence: u16,
    flags: u16,
    /// For extension records: the base record and its sequence number. 0 for base records.
    pub base: u64,
    pub base_sequence: u16,
    first_attribute: usize,
    used: usize,
}

impl Header {
    pub fn in_use(&self) -> bool {
        self.flags & IN_USE != 0
    }

    pub fn is_dir(&self) -> bool {
        self.flags & DIRECTORY != 0
    }
}

pub(crate) fn header(record: &[u8]) -> Option<Header> {
    let (base, base_sequence) = reference(u64_at(record, 32)?);
    Some(Header {
        sequence: u16_at(record, 16)?,
        flags: u16_at(record, 22)?,
        base,
        base_sequence,
        first_attribute: usize::from(u16_at(record, 20)?),
        used: usize::try_from(u32_at(record, 24)?).ok()?.min(record.len()),
    })
}

/// A stretch of clusters: `length` clusters from virtual cluster `vcn` of
/// the stream are at logical cluster `lcn` of the volume, or nowhere
/// (sparse, reads as zeros) when `lcn` is `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub vcn: u64,
    pub length: u64,
    pub lcn: Option<u64>,
}

/// Decodes a run list. A run that makes no sense ends it; the runs before it
/// are still returned.
pub(crate) fn decode_runs(data: &[u8], start_vcn: u64) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut at = 0;
    let mut vcn = start_vcn;
    let mut lcn: i64 = 0;
    while let Some(&head) = data.get(at) {
        let length_size = usize::from(head & 0x0F);
        let offset_size = usize::from(head >> 4);
        if head == 0 || length_size == 0 || length_size > 8 || offset_size > 8 {
            break;
        }
        let Some(length) = bytes(data, at + 1, length_size).map(unsigned) else {
            break;
        };
        let Some(offset) = bytes(data, at + 1 + length_size, offset_size).map(signed) else {
            break;
        };
        at += 1 + length_size + offset_size;
        let start = if offset_size == 0 {
            None
        } else {
            match lcn
                .checked_add(offset)
                .and_then(|value| u64::try_from(value).ok())
            {
                Some(value) => {
                    lcn += offset;
                    Some(value)
                }
                None => break,
            }
        };
        let Some(next) = vcn.checked_add(length).filter(|_| length > 0) else {
            break;
        };
        runs.push(Run {
            vcn,
            length,
            lcn: start,
        });
        vcn = next;
    }
    runs
}

fn unsigned(data: &[u8]) -> u64 {
    let mut raw = [0; 8];
    raw[..data.len()].copy_from_slice(data);
    u64::from_le_bytes(raw)
}

fn signed(data: &[u8]) -> i64 {
    let fill = if data.last().is_some_and(|&b| b & 0x80 != 0) {
        0xFF
    } else {
        0
    };
    let mut raw = [fill; 8];
    raw[..data.len()].copy_from_slice(data);
    i64::from_le_bytes(raw)
}

/// A non-resident attribute's header. Sizes are only meaningful in the
/// piece that starts at VCN 0.
#[derive(Clone, Debug)]
pub(crate) struct NonResident {
    pub start_vcn: u64,
    pub compression_unit: u8,
    pub size: u64,
    pub initialized: u64,
    pub runs: Vec<Run>,
}

pub(crate) enum Value<'a> {
    /// The value itself, and where it starts in the record.
    Resident {
        data: &'a [u8],
        offset: usize,
    },
    NonResident(NonResident),
}

pub(crate) struct Attribute<'a> {
    pub kind: u32,
    pub name: String,
    pub flags: u16,
    pub value: Value<'a>,
}

impl Attribute<'_> {
    pub fn resident(&self) -> Option<&[u8]> {
        match self.value {
            Value::Resident { data, .. } => Some(data),
            Value::NonResident(_) => None,
        }
    }
}

/// A record's attributes, up to the end marker or the first damaged one.
pub(crate) fn attributes(record: &[u8]) -> Vec<Attribute<'_>> {
    let mut found = Vec::new();
    let Some(header) = header(record) else {
        return found;
    };
    let mut at = header.first_attribute;
    while let Some(kind) = u32_at(record, at).filter(|&kind| kind != END) {
        let Some(length) = u32_at(record, at + 4).and_then(|l| usize::try_from(l).ok()) else {
            break;
        };
        let Some(attribute) = record
            .get(at..at + length)
            .filter(|_| length >= 16 && at + length <= header.used)
        else {
            break;
        };
        if let Some(parsed) = attribute_at(attribute, kind, at) {
            found.push(parsed);
        }
        at += length;
    }
    found
}

fn attribute_at(attribute: &[u8], kind: u32, at: usize) -> Option<Attribute<'_>> {
    let non_resident = u8_at(attribute, 8)? != 0;
    let name_length = usize::from(u8_at(attribute, 9)?);
    let name_offset = usize::from(u16_at(attribute, 10)?);
    let flags = u16_at(attribute, 12)?;
    let name = utf16(bytes(attribute, name_offset, 2 * name_length)?);
    let value = if non_resident {
        let start_vcn = u64_at(attribute, 16)?;
        let runs_offset = usize::from(u16_at(attribute, 32)?);
        Value::NonResident(NonResident {
            start_vcn,
            compression_unit: u8_at(attribute, 34)?,
            size: u64_at(attribute, 48)?,
            initialized: u64_at(attribute, 56)?,
            runs: decode_runs(attribute.get(runs_offset..)?, start_vcn),
        })
    } else {
        let length = usize::try_from(u32_at(attribute, 16)?).ok()?;
        let offset = usize::from(u16_at(attribute, 20)?);
        Value::Resident {
            data: bytes(attribute, offset, length)?,
            offset: at + offset,
        }
    };
    Some(Attribute {
        kind,
        name,
        flags,
        value,
    })
}

/// UTF-16LE text, all of it (NTFS names aren't NUL-terminated).
fn utf16(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
    String::from_utf16_lossy(&units)
}

/// A `$FILE_NAME` attribute: one of a file's names, in one folder.
#[derive(Clone, Debug)]
pub(crate) struct FileName {
    pub parent: u64,
    pub parent_sequence: u16,
    pub name: String,
    pub namespace: u8,
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    pub flags: u32,
}

impl FileName {
    /// Long (Win32) names first, then POSIX names, then DOS 8.3 names.
    pub fn rank(&self) -> u8 {
        match self.namespace {
            1 | 3 => 0,
            0 => 1,
            _ => 2,
        }
    }
}

pub(crate) fn file_name(value: &[u8]) -> Option<FileName> {
    let (parent, parent_sequence) = reference(u64_at(value, 0)?);
    let length = usize::from(u8_at(value, 64)?);
    let name = utf16(bytes(value, 66, 2 * length)?);
    (!name.is_empty()).then_some(())?;
    Some(FileName {
        parent,
        parent_sequence,
        name,
        namespace: u8_at(value, 65)?,
        created: time::filetime(u64_at(value, 8)?),
        modified: time::filetime(u64_at(value, 16)?),
        accessed: time::filetime(u64_at(value, 32)?),
        flags: u32_at(value, 56)?,
    })
}

/// `$STANDARD_INFORMATION`: times and Windows attributes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StandardInfo {
    pub created: Option<i64>,
    pub modified: Option<i64>,
    pub accessed: Option<i64>,
    pub attributes: u32,
}

pub(crate) fn standard_info(value: &[u8]) -> Option<StandardInfo> {
    Some(StandardInfo {
        created: time::filetime(u64_at(value, 0)?),
        modified: time::filetime(u64_at(value, 8)?),
        accessed: time::filetime(u64_at(value, 24)?),
        attributes: u32_at(value, 32)?,
    })
}

/// An `$ATTRIBUTE_LIST` entry: which record holds (part of) an attribute.
#[derive(Clone, Debug)]
pub(crate) struct ListEntry {
    pub kind: u32,
    pub start_vcn: u64,
    pub record: u64,
    pub name: String,
}

pub(crate) fn attribute_list(value: &[u8]) -> Vec<ListEntry> {
    let mut entries = Vec::new();
    let mut at = 0;
    while let (Some(kind), Some(length)) = (u32_at(value, at), u16_at(value, at + 4)) {
        let length = usize::from(length);
        let Some(entry) = value.get(at..at + length).filter(|_| length >= 26) else {
            break;
        };
        let name_length = usize::from(u8_at(entry, 6).unwrap_or(0));
        let name_offset = usize::from(u8_at(entry, 7).unwrap_or(0));
        let (Some(start_vcn), Some(reference_value)) = (u64_at(entry, 8), u64_at(entry, 16)) else {
            break;
        };
        entries.push(ListEntry {
            kind,
            start_vcn,
            record: reference(reference_value).0,
            name: bytes(entry, name_offset, 2 * name_length)
                .map(utf16)
                .unwrap_or_default(),
        });
        at += length;
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_lists() {
        // 0x18 clusters at 0x5634, 0x10 sparse, then 0x20 at 0x5634 - 0x100.
        let data = [
            0x21, 0x18, 0x34, 0x56, 0x01, 0x10, 0x21, 0x20, 0x00, 0xFF, 0x00,
        ];
        assert_eq!(
            decode_runs(&data, 0),
            [
                Run {
                    vcn: 0,
                    length: 0x18,
                    lcn: Some(0x5634)
                },
                Run {
                    vcn: 0x18,
                    length: 0x10,
                    lcn: None
                },
                Run {
                    vcn: 0x28,
                    length: 0x20,
                    lcn: Some(0x5534)
                },
            ]
        );
        // Cut short, negative, or zero-length: decoding stops.
        assert_eq!(decode_runs(&data[..3], 0), []);
        assert_eq!(decode_runs(&[0x11, 0x05, 0x80, 0x00], 0), []);
        assert_eq!(decode_runs(&[0x11, 0x00, 0x05, 0x00], 0), []);
        assert_eq!(decode_runs(&[0x91, 0x05], 7), []);
    }

    #[test]
    fn fixups_restore_each_sectors_last_bytes() {
        let mut record = vec![0u8; 1024];
        record[..4].copy_from_slice(b"FILE");
        record[4..6].copy_from_slice(&48u16.to_le_bytes());
        record[6..8].copy_from_slice(&3u16.to_le_bytes());
        record[48..54].copy_from_slice(&[0x07, 0x00, 0xAA, 0xBB, 0xCC, 0xDD]);
        record[510..512].copy_from_slice(&[0x07, 0x00]);
        record[1022..1024].copy_from_slice(&[0x07, 0x00]);
        assert!(fixup(&mut record));
        assert_eq!(record[510..512], [0xAA, 0xBB]);
        assert_eq!(record[1022..1024], [0xCC, 0xDD]);
        // Too many fixups for the record, or not a record at all.
        record[6..8].copy_from_slice(&9u16.to_le_bytes());
        assert!(!fixup(&mut record));
        assert!(!fixup(&mut [0u8; 1024]));
    }
}
