//! The sectors a table is written as, in the order that leaves the disk
//! readable if writing stops partway: for GPT the backup copy, then the
//! primary copy, then the protective MBR; for MBR the logical partitions'
//! boot records, then the MBR, then any old GPT headers being cleared.

use super::backup::SectorWrite;
use super::{EditableTable, Part, Style};
use crate::crc32::crc32;

/// The heads and sectors per track every tool assumes for CHS values.
const HEADS: u64 = 255;
const SECTORS_PER_TRACK: u64 = 63;

impl EditableTable {
    /// Every sector this table occupies (or clears), in writing order.
    pub(crate) fn encode(&self) -> Vec<SectorWrite> {
        match self.style {
            None => Vec::new(),
            Some(Style::Mbr) => self.encode_mbr(),
            Some(Style::Gpt) => self.encode_gpt(),
        }
    }

    fn sector(&self) -> Vec<u8> {
        vec![0; self.sector_size as usize]
    }

    /// Sector 0 with its boot code (bytes 0–439) kept.
    fn boot_sector(&self) -> Vec<u8> {
        let mut sector = self.sector();
        let keep = self.sector0.len().min(440);
        sector[..keep].copy_from_slice(&self.sector0[..keep]);
        sector[510] = 0x55;
        sector[511] = 0xAA;
        sector
    }

    fn encode_mbr(&self) -> Vec<SectorWrite> {
        let mut writes = Vec::new();
        let logicals = self.logicals();
        if let Some(e) = &self.extended {
            // The chain of extended boot records, last first.
            let mut ebrs = Vec::new();
            if logicals.is_empty() {
                let mut sector = self.sector();
                sector[510] = 0x55;
                sector[511] = 0xAA;
                ebrs.push(SectorWrite {
                    lba: e.start,
                    data: sector,
                });
            }
            for (i, l) in logicals.iter().enumerate() {
                let mut sector = self.sector();
                put_entry(
                    &mut sector,
                    446,
                    l.active,
                    l.mbr_type,
                    l.start,
                    l.start - l.ebr,
                    l.sectors,
                );
                if let Some(next) = logicals.get(i + 1) {
                    put_entry(
                        &mut sector,
                        462,
                        false,
                        0x05,
                        next.ebr,
                        next.ebr - e.start,
                        next.end() - next.ebr,
                    );
                }
                sector[510] = 0x55;
                sector[511] = 0xAA;
                ebrs.push(SectorWrite {
                    lba: l.ebr,
                    data: sector,
                });
            }
            ebrs.reverse();
            writes.extend(ebrs);
        }
        let mut mbr = self.boot_sector();
        mbr[440..444].copy_from_slice(&self.mbr_signature.to_le_bytes());
        mbr[444..446].fill(0);
        mbr[446..510].fill(0);
        for p in self.parts.iter().filter(|p| !p.logical) {
            put_entry(
                &mut mbr,
                446 + 16 * p.slot,
                p.active,
                p.mbr_type,
                p.start,
                p.start,
                p.sectors,
            );
        }
        if let Some(e) = &self.extended {
            put_entry(
                &mut mbr,
                446 + 16 * e.slot,
                false,
                e.mbr_type,
                e.start,
                e.start,
                e.sectors,
            );
        }
        writes.push(SectorWrite { lba: 0, data: mbr });
        for &lba in &self.clear {
            writes.push(SectorWrite {
                lba,
                data: self.sector(),
            });
        }
        writes
    }

    fn encode_gpt(&self) -> Vec<SectorWrite> {
        let sector_size = self.sector_size as usize;
        let array_sectors = self.array_sectors();
        let mut array = vec![0u8; usize::try_from(array_sectors).unwrap_or(0) * sector_size];
        let entry_size = self.entry_size as usize;
        for p in &self.parts {
            let at = p.slot * entry_size;
            let entry = &mut array[at..at + entry_size];
            if p.raw.len() == entry_size {
                entry.copy_from_slice(&p.raw);
            }
            write_gpt_entry(entry, p);
        }
        let entries_len = self.entry_count as usize * entry_size;
        let entries_crc = crc32(&array[..entries_len]);
        let last = self.disk_sectors - 1;
        let backup_array = last - array_sectors;
        let header = |my_lba: u64, alternate: u64, entries_lba: u64| {
            let (first, last_usable) = self.usable();
            let mut sector = self.sector();
            sector[0..8].copy_from_slice(b"EFI PART");
            sector[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
            sector[12..16].copy_from_slice(&92u32.to_le_bytes());
            sector[24..32].copy_from_slice(&my_lba.to_le_bytes());
            sector[32..40].copy_from_slice(&alternate.to_le_bytes());
            sector[40..48].copy_from_slice(&first.to_le_bytes());
            sector[48..56].copy_from_slice(&last_usable.to_le_bytes());
            sector[56..72].copy_from_slice(&self.disk_guid);
            sector[72..80].copy_from_slice(&entries_lba.to_le_bytes());
            sector[80..84].copy_from_slice(&self.entry_count.to_le_bytes());
            sector[84..88].copy_from_slice(&self.entry_size.to_le_bytes());
            sector[88..92].copy_from_slice(&entries_crc.to_le_bytes());
            let crc = crc32(&sector[..92]);
            sector[16..20].copy_from_slice(&crc.to_le_bytes());
            sector
        };
        // The protective MBR: one entry of type 0xEE covering the disk.
        let mut mbr = self.boot_sector();
        mbr[440..510].fill(0);
        let covered = (self.disk_sectors - 1).min(u64::from(u32::MAX));
        put_entry(&mut mbr, 446, false, 0xEE, 1, 1, covered);
        vec![
            SectorWrite {
                lba: backup_array,
                data: array.clone(),
            },
            SectorWrite {
                lba: last,
                data: header(last, 1, backup_array),
            },
            SectorWrite {
                lba: 2,
                data: array,
            },
            SectorWrite {
                lba: 1,
                data: header(1, last, 2),
            },
            SectorWrite { lba: 0, data: mbr },
        ]
    }
}

fn write_gpt_entry(entry: &mut [u8], p: &Part) {
    entry[0..16].copy_from_slice(&p.gpt_type);
    entry[16..32].copy_from_slice(&p.guid);
    entry[32..40].copy_from_slice(&p.start.to_le_bytes());
    entry[40..48].copy_from_slice(&(p.end() - 1).to_le_bytes());
    entry[48..56].copy_from_slice(&p.attributes.to_le_bytes());
    entry[56..128].fill(0);
    for (i, unit) in p.name.encode_utf16().take(36).enumerate() {
        entry[56 + 2 * i..58 + 2 * i].copy_from_slice(&unit.to_le_bytes());
    }
}

/// An MBR or EBR entry. `lba` is where it really is on the disk (for the
/// CHS values); `start` is what the entry stores (relative in EBRs).
fn put_entry(
    sector: &mut [u8],
    at: usize,
    active: bool,
    kind: u8,
    lba: u64,
    start: u64,
    sectors: u64,
) {
    let entry = &mut sector[at..at + 16];
    entry[0] = if active { 0x80 } else { 0 };
    entry[1..4].copy_from_slice(&chs(lba));
    entry[4] = kind;
    entry[5..8].copy_from_slice(&chs(lba + sectors.max(1) - 1));
    entry[8..12].copy_from_slice(&u32::try_from(start).unwrap_or(u32::MAX).to_le_bytes());
    entry[12..16].copy_from_slice(&u32::try_from(sectors).unwrap_or(u32::MAX).to_le_bytes());
}

/// Cylinder/head/sector bytes for an LBA, or the usual "too far" value.
fn chs(lba: u64) -> [u8; 3] {
    let cylinder = lba / (HEADS * SECTORS_PER_TRACK);
    if cylinder > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    let head = (lba / SECTORS_PER_TRACK) % HEADS;
    let sector = lba % SECTORS_PER_TRACK + 1;
    let byte = |v: u64| u8::try_from(v & 0xFF).unwrap_or(0);
    [
        byte(head),
        byte(((cylinder >> 8) << 6) | sector),
        byte(cylinder),
    ]
}

#[cfg(test)]
mod tests {
    use super::chs;

    #[test]
    fn chs_values() {
        assert_eq!(chs(0), [0, 1, 0]);
        assert_eq!(chs(2048), [32, 33, 0]);
        assert_eq!(chs(16_450_559), [254, 255, 255], "cylinder 1023, the last");
        assert_eq!(chs(1 << 30), [0xFE, 0xFF, 0xFF]);
    }
}
