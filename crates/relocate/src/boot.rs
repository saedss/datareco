//! After a partition's data has moved, its file system's own note of where
//! it starts on the disk: FAT and NTFS keep it in their boot sectors
//! ("hidden sectors"), exFAT as its PartitionOffset, covered by the boot
//! region's checksum. Other file systems don't record it.

use std::io;

/// Rewrites the note at the partition now starting at `start_lba` (and in
/// the boot sector's backup copies). Returns which file system it was, if
/// one that keeps such a note. Doing it twice changes nothing.
///
/// `read` and `write` take sector numbers on the disk; `sectors` is the
/// partition's length.
pub fn fix(
    read: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
    write: &mut dyn FnMut(u64, &[u8]) -> io::Result<()>,
    start_lba: u64,
    sectors: u64,
    sector_size: u32,
) -> io::Result<Option<&'static str>> {
    let ss = sector_size as usize;
    let mut boot = vec![0u8; ss];
    read(start_lba, &mut boot)?;
    if boot.get(510..512) != Some(&[0x55, 0xAA]) {
        return Ok(None);
    }
    let hidden = u32::try_from(start_lba).unwrap_or(u32::MAX).to_le_bytes();
    let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]);
    if &boot[3..11] == b"NTFS    " {
        // The backup boot sector is the partition's last sector.
        let total = u64::from_le_bytes(boot[40..48].try_into().unwrap_or_default());
        patch_hidden(read, write, start_lba, &hidden, ss)?;
        if total > 0 && total < sectors {
            let backup = start_lba + total;
            let mut copy = vec![0u8; ss];
            read(backup, &mut copy)?;
            if &copy[3..11] == b"NTFS    " {
                patch_hidden(read, write, backup, &hidden, ss)?;
            }
        }
        return Ok(Some("NTFS"));
    }
    if &boot[3..11] == b"EXFAT   " {
        fix_exfat(read, write, start_lba, start_lba, ss)?;
        fix_exfat(read, write, start_lba, start_lba + 12, ss)?;
        return Ok(Some("exFAT"));
    }
    let fat32 = &boot[82..90] == b"FAT32   ";
    let fat16 = &boot[54..59] == b"FAT12" || &boot[54..59] == b"FAT16";
    if (fat32 || fat16) && usize::from(bytes_per_sector) == ss {
        patch_hidden(read, write, start_lba, &hidden, ss)?;
        if fat32 {
            let backup = u64::from(u16::from_le_bytes([boot[50], boot[51]]));
            if backup > 0 && backup < 32 {
                patch_hidden(read, write, start_lba + backup, &hidden, ss)?;
            }
        }
        return Ok(Some(if fat32 { "FAT32" } else { "FAT" }));
    }
    Ok(None)
}

fn patch_hidden(
    read: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
    write: &mut dyn FnMut(u64, &[u8]) -> io::Result<()>,
    lba: u64,
    hidden: &[u8; 4],
    ss: usize,
) -> io::Result<()> {
    let mut sector = vec![0u8; ss];
    read(lba, &mut sector)?;
    if sector[28..32] != hidden[..] {
        sector[28..32].copy_from_slice(hidden);
        write(lba, &sector)?;
    }
    Ok(())
}

/// One exFAT boot region (12 sectors from `region`, the main one or its
/// backup) of the partition starting at `partition`: its PartitionOffset,
/// then the checksum sector that covers it.
fn fix_exfat(
    read: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
    write: &mut dyn FnMut(u64, &[u8]) -> io::Result<()>,
    partition: u64,
    region: u64,
    ss: usize,
) -> io::Result<()> {
    let mut sectors = vec![0u8; 12 * ss];
    read(region, &mut sectors)?;
    if &sectors[3..11] != b"EXFAT   " {
        return Ok(());
    }
    let offset = partition.to_le_bytes();
    if sectors[64..72] == offset {
        return Ok(());
    }
    sectors[64..72].copy_from_slice(&offset);
    let sum = sectors[..11 * ss]
        .iter()
        .enumerate()
        .filter(|(i, _)| !matches!(i, 106 | 107 | 112))
        .fold(0u32, |s, (_, &b)| {
            s.rotate_right(1).wrapping_add(u32::from(b))
        });
    for word in sectors[11 * ss..].chunks_mut(4) {
        word.copy_from_slice(&sum.to_le_bytes());
    }
    write(region, &sectors)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn at(lba: u64) -> usize {
        usize::try_from(lba).unwrap() * 512
    }

    /// Runs `fix` on a disk of 512-byte sectors in memory.
    fn run(disk: &mut Vec<u8>, start: u64, sectors: u64) -> Option<&'static str> {
        let cell = RefCell::new(std::mem::take(disk));
        let result = fix(
            &mut |lba, buf| {
                buf.copy_from_slice(&cell.borrow()[at(lba)..at(lba) + buf.len()]);
                Ok(())
            },
            &mut |lba, buf| {
                cell.borrow_mut()[at(lba)..at(lba) + buf.len()].copy_from_slice(buf);
                Ok(())
            },
            start,
            sectors,
            512,
        )
        .unwrap();
        *disk = cell.into_inner();
        result
    }

    fn ntfs_boot(total: u64, hidden: u32) -> [u8; 512] {
        let mut boot = [0u8; 512];
        boot[0..3].copy_from_slice(&[0xEB, 0x52, 0x90]);
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[28..32].copy_from_slice(&hidden.to_le_bytes());
        boot[40..48].copy_from_slice(&total.to_le_bytes());
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);
        boot
    }

    #[test]
    fn ntfs_and_its_backup_boot_sector_learn_the_new_start() {
        // Moved from sector 63 to 2048; NTFS counts one sector less than
        // the partition and keeps its backup boot sector in the last one.
        let (start, sectors) = (2048, 100);
        let mut disk = vec![0u8; at(start + sectors)];
        let boot = ntfs_boot(sectors - 1, 63);
        let backup = start + sectors - 1;
        disk[at(start)..at(start + 1)].copy_from_slice(&boot);
        disk[at(backup)..at(backup + 1)].copy_from_slice(&boot);

        assert_eq!(run(&mut disk, start, sectors), Some("NTFS"));
        for lba in [start, backup] {
            assert_eq!(disk[at(lba) + 28..at(lba) + 32], 2048u32.to_le_bytes());
        }
        let done = disk.clone();
        assert_eq!(run(&mut disk, start, sectors), Some("NTFS"));
        assert_eq!(disk, done, "doing it twice changes nothing");
    }

    #[test]
    fn leaves_other_file_systems_alone() {
        let mut disk = vec![0u8; at(2148)];
        disk[at(2048) + 510..at(2048) + 512].copy_from_slice(&[0x55, 0xAA]);
        disk[at(2048) + 3..at(2048) + 11].copy_from_slice(b"MSWIN4.1");
        let before = disk.clone();
        assert_eq!(run(&mut disk, 2048, 100), None);
        assert_eq!(disk, before);
    }
}
