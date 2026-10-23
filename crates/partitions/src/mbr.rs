//! MBR partition tables and the extended boot records (EBRs) of logical partitions.

use crate::bytes::{has, u8_at, u32_at};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub status: u8,
    pub type_code: u8,
    /// First sector: from the disk start (MBR), or relative (EBR).
    pub start: u64,
    pub sectors: u64,
}

impl Entry {
    pub fn is_empty(&self) -> bool {
        self.type_code == 0 || self.sectors == 0
    }

    pub fn is_extended(&self) -> bool {
        matches!(self.type_code, 0x05 | 0x0F | 0x85)
    }
}

/// An MBR or EBR: four entries and the 55 AA signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Mbr {
    pub disk_signature: u32,
    pub entries: [Entry; 4],
}

/// Parses a sector as a partition table. Only accepts it if it looks like
/// one: the signature, status bytes of 00 or 80, and at least one entry.
pub(crate) fn parse(sector: &[u8]) -> Option<Mbr> {
    if !has(sector, 510, &[0x55, 0xAA]) {
        return None;
    }
    let entry = |i: usize| -> Option<Entry> {
        let at = 446 + 16 * i;
        Some(Entry {
            status: u8_at(sector, at)?,
            type_code: u8_at(sector, at + 4)?,
            start: u64::from(u32_at(sector, at + 8)?),
            sectors: u64::from(u32_at(sector, at + 12)?),
        })
    };
    let entries = [entry(0)?, entry(1)?, entry(2)?, entry(3)?];
    let plausible = entries.iter().all(|e| matches!(e.status, 0x00 | 0x80))
        && entries.iter().any(|e| !e.is_empty())
        && entries
            .iter()
            .filter(|e| !e.is_empty())
            .all(|e| e.start > 0);
    plausible.then(|| Mbr {
        disk_signature: u32_at(sector, 440).unwrap_or(0),
        entries,
    })
}

/// Names for MBR partition type codes.
pub(crate) fn type_name(code: u8) -> &'static str {
    match code {
        0x01 => "FAT12",
        0x04 => "FAT16 (under 32 MB)",
        0x05 => "extended (CHS)",
        0x06 => "FAT16",
        0x07 => "NTFS, exFAT or HPFS",
        0x0B => "FAT32 (CHS)",
        0x0C => "FAT32 (LBA)",
        0x0E => "FAT16 (LBA)",
        0x0F => "extended (LBA)",
        0x11 => "hidden FAT12",
        0x12 => "OEM diagnostics",
        0x14 => "hidden FAT16 (under 32 MB)",
        0x16 => "hidden FAT16",
        0x17 => "hidden NTFS or exFAT",
        0x1B => "hidden FAT32 (CHS)",
        0x1C => "hidden FAT32 (LBA)",
        0x1E => "hidden FAT16 (LBA)",
        0x27 => "Windows recovery",
        0x42 => "Windows dynamic disk (LDM)",
        0x82 => "Linux swap",
        0x83 => "Linux",
        0x85 => "Linux extended",
        0x8E => "Linux LVM",
        0xA5 => "FreeBSD",
        0xA6 => "OpenBSD",
        0xA8 => "Apple UFS",
        0xAB => "Apple boot",
        0xAF => "Apple HFS/HFS+",
        0xEE => "GPT protective",
        0xEF => "EFI system",
        0xFB => "VMware VMFS",
        0xFD => "Linux RAID",
        _ => "unknown",
    }
}

#[cfg(test)]
pub(crate) mod samples {
    /// A partition table sector with the given (status, type, start, sectors) entries.
    pub fn table(entries: &[(u8, u8, u32, u32)]) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        s[440..444].copy_from_slice(&0x0DA7_A8ECu32.to_le_bytes());
        for (i, &(status, type_code, start, sectors)) in entries.iter().enumerate() {
            let at = 446 + 16 * i;
            s[at] = status;
            s[at + 4] = type_code;
            s[at + 8..at + 12].copy_from_slice(&start.to_le_bytes());
            s[at + 12..at + 16].copy_from_slice(&sectors.to_le_bytes());
        }
        s[510] = 0x55;
        s[511] = 0xAA;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_table() {
        let mbr = parse(&samples::table(&[
            (0x80, 0x0C, 2048, 98_304),
            (0, 0x0F, 100_352, 200_000),
        ]))
        .unwrap();
        assert_eq!(mbr.disk_signature, 0x0DA7_A8EC);
        assert_eq!(mbr.entries[0].start, 2048);
        assert!(mbr.entries[1].is_extended());
        assert!(mbr.entries[2].is_empty());
    }

    #[test]
    fn rejects_boot_code_and_empty_tables() {
        assert!(parse(&samples::table(&[])).is_none(), "no entries");
        let mut boot_code = samples::table(&[(0x80, 0x0C, 2048, 100)]);
        boot_code[446] = 0x33; // a status byte that's really code
        assert!(parse(&boot_code).is_none());
        assert!(parse(&[0u8; 100]).is_none());
    }
}
