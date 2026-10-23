//! GUID Partition Tables: headers (primary at LBA 1, backup in the last
//! sector) and their partition entry arrays.

use datareco_io::{Disk, Result};

use crate::bytes::{array16, bytes, guid_text, has, u32_at, u64_at, utf16_text};
use crate::crc32::crc32;

/// Entry arrays larger than this aren't real (the norm is 16 KiB).
const MAX_ENTRY_BYTES: u64 = 4 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub my_lba: u64,
    pub alternate_lba: u64,
    pub first_usable: u64,
    pub last_usable: u64,
    pub disk_guid: [u8; 16],
    pub entries_lba: u64,
    pub entry_count: u64,
    pub entry_size: u64,
    pub entries_crc: u32,
    /// Whether the header's own CRC32 matches.
    pub crc_ok: bool,
}

/// Parses a GPT header sector. Returns it even when its CRC is wrong; check
/// `crc_ok`.
pub(crate) fn parse_header(sector: &[u8]) -> Option<Header> {
    if !has(sector, 0, b"EFI PART") {
        return None;
    }
    let header_size = usize::try_from(u32_at(sector, 12)?).ok()?;
    let entry_count = u64::from(u32_at(sector, 80)?);
    let entry_size = u64::from(u32_at(sector, 84)?);
    let plausible = (92..=sector.len()).contains(&header_size)
        && entry_size >= 128
        && entry_size.is_power_of_two()
        && entry_count > 0
        && entry_count * entry_size <= MAX_ENTRY_BYTES;
    if !plausible {
        return None;
    }
    let mut copy = bytes(sector, 0, header_size)?.to_vec();
    let stored_crc = u32_at(&copy, 16)?;
    copy.get_mut(16..20)?.fill(0);
    Some(Header {
        my_lba: u64_at(sector, 24)?,
        alternate_lba: u64_at(sector, 32)?,
        first_usable: u64_at(sector, 40)?,
        last_usable: u64_at(sector, 48)?,
        disk_guid: array16(sector, 56)?,
        entries_lba: u64_at(sector, 72)?,
        entry_count,
        entry_size,
        entries_crc: u32_at(sector, 88)?,
        crc_ok: crc32(&copy) == stored_crc,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub attributes: u64,
    pub name: Option<String>,
    /// Position in the array, from 0.
    pub index: u64,
}

/// Reads a header's entry array: the used entries, and whether the array's
/// CRC32 matches the header.
pub(crate) fn read_entries(disk: &Disk, header: &Header) -> Result<(Vec<Entry>, bool)> {
    let sector_size = u64::from(disk.sector_size());
    let len = header.entry_count * header.entry_size;
    let Some(offset) = header
        .entries_lba
        .checked_mul(sector_size)
        .filter(|&o| o.saturating_add(len) <= disk.size())
    else {
        return Ok((Vec::new(), false));
    };
    let mut data = vec![0; usize::try_from(len).unwrap_or(0)];
    disk.read_at(offset, &mut data)?;
    let crc_ok = crc32(&data) == header.entries_crc;
    let entry_size = usize::try_from(header.entry_size).unwrap_or(usize::MAX);
    let entries = data
        .chunks(entry_size)
        .zip(0..)
        .filter_map(|(raw, index)| {
            let type_guid = array16(raw, 0)?;
            if type_guid == [0; 16] {
                return None;
            }
            Some(Entry {
                type_guid,
                unique_guid: array16(raw, 16)?,
                first_lba: u64_at(raw, 32)?,
                last_lba: u64_at(raw, 40)?,
                attributes: u64_at(raw, 48)?,
                name: bytes(raw, 56, 72).and_then(utf16_text),
                index,
            })
        })
        .collect();
    Ok((entries, crc_ok))
}

/// Names for common GPT partition type GUIDs.
pub(crate) fn type_name(guid: &str) -> &'static str {
    match guid {
        "C12A7328-F81F-11D2-BA4B-00A0C93EC93B" => "EFI system partition",
        "21686148-6449-6E6F-744E-656564454649" => "BIOS boot partition",
        "E3C9E316-0B5C-4DB8-817D-F92DF00215AE" => "Microsoft reserved",
        "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7" => "Microsoft basic data",
        "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC" => "Windows recovery",
        "5808C8AA-7E8F-42E0-85D2-E1E90434CFB3" => "Windows LDM metadata",
        "AF9B60A0-1431-4F62-BC68-3311714A69AD" => "Windows LDM data",
        "E75CAF8F-F680-4CEE-AFA3-B001E56EFC2D" => "Windows Storage Spaces",
        "0FC63DAF-8483-4772-8E79-3D69D8477DE4" => "Linux file system",
        "0657FD6D-A4AB-43C4-84E5-0933C84B4F4F" => "Linux swap",
        "E6D6D379-F507-44C2-A23C-238F2A3DF928" => "Linux LVM",
        "A19D880F-05FC-4D3B-A006-743F0F84911E" => "Linux RAID",
        "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709" => "Linux root (x86-64)",
        "933AC7E1-2EB4-4F13-B844-0E14E2AEF915" => "Linux /home",
        "48465300-0000-11AA-AA11-00306543ECAC" => "Apple HFS+",
        "7C3457EF-0000-11AA-AA11-00306543ECAC" => "Apple APFS",
        "516E7CB4-6ECF-11D6-8FF8-00022D09712B" => "FreeBSD data",
        "AA31E02A-400F-11DB-9590-000C2911D1B8" => "VMware VMFS",
        _ => "unknown",
    }
}

pub(crate) fn guid(bytes: &[u8; 16]) -> String {
    guid_text(bytes)
}

#[cfg(test)]
pub(crate) mod samples {
    use crate::crc32::crc32;

    pub const BASIC_DATA: [u8; 16] = [
        0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99,
        0xC7,
    ];

    /// A 128-entry array (32 sectors) with the given (first, last, name) partitions.
    pub fn entries(partitions: &[(u64, u64, &str)]) -> Vec<u8> {
        let mut data = vec![0u8; 128 * 128];
        for (i, (first, last, name)) in partitions.iter().enumerate() {
            let at = i * 128;
            data[at..at + 16].copy_from_slice(&BASIC_DATA);
            data[at + 16] = u8::try_from(i + 1).unwrap();
            data[at + 32..at + 40].copy_from_slice(&first.to_le_bytes());
            data[at + 40..at + 48].copy_from_slice(&last.to_le_bytes());
            for (j, unit) in name.encode_utf16().enumerate() {
                data[at + 56 + 2 * j..at + 58 + 2 * j].copy_from_slice(&unit.to_le_bytes());
            }
        }
        data
    }

    /// A header at `my_lba` for a disk of `sectors` sectors, with valid CRCs.
    pub fn header(my_lba: u64, sectors: u64, entries_lba: u64, entries: &[u8]) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[0..8].copy_from_slice(b"EFI PART");
        s[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        s[12..16].copy_from_slice(&92u32.to_le_bytes());
        let alternate = if my_lba == 1 { sectors - 1 } else { 1 };
        s[24..32].copy_from_slice(&my_lba.to_le_bytes());
        s[32..40].copy_from_slice(&alternate.to_le_bytes());
        s[40..48].copy_from_slice(&34u64.to_le_bytes());
        s[48..56].copy_from_slice(&(sectors - 34).to_le_bytes());
        s[56..72].copy_from_slice(&[0x11; 16]);
        s[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        s[80..84].copy_from_slice(&128u32.to_le_bytes());
        s[84..88].copy_from_slice(&128u32.to_le_bytes());
        s[88..92].copy_from_slice(&crc32(entries).to_le_bytes());
        let crc = crc32(&s[..92]);
        s[16..20].copy_from_slice(&crc.to_le_bytes());
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_checksums() {
        let entries = samples::entries(&[(2048, 4095, "one")]);
        let header = parse_header(&samples::header(1, 10_000, 2, &entries)).unwrap();
        assert!(header.crc_ok);
        assert_eq!(header.alternate_lba, 9999);
        assert_eq!(header.entries_crc, crc32(&entries));

        let mut damaged = samples::header(1, 10_000, 2, &entries);
        damaged[40] ^= 1;
        assert!(!parse_header(&damaged).unwrap().crc_ok);
        assert!(parse_header(&[0u8; 512]).is_none());
    }

    #[test]
    fn type_names() {
        assert_eq!(
            type_name(&guid(&samples::BASIC_DATA)),
            "Microsoft basic data"
        );
    }
}
