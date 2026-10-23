//! Recognizing file systems from boot sectors and superblocks.
//!
//! Each parser checks that the numbers make sense, not just the magic bytes:
//! the lost-partition search runs them over every sector of a disk.

use datareco_io::{Disk, Result};
use serde::{Deserialize, Serialize};

use crate::bytes::{array16, bytes, has, text, u8_at, u16_at, u32_at, u32_be, u64_at};

/// Kinds of file systems DataReco recognizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsKind {
    Ntfs,
    Fat12,
    Fat16,
    Fat32,
    Exfat,
    Ext2,
    Ext3,
    Ext4,
    Refs,
    HfsPlus,
    Apfs,
    /// Not a file system: files found by their contents (raw recovery).
    Raw,
}

impl FsKind {
    pub fn name(self) -> &'static str {
        match self {
            FsKind::Ntfs => "NTFS",
            FsKind::Fat12 => "FAT12",
            FsKind::Fat16 => "FAT16",
            FsKind::Fat32 => "FAT32",
            FsKind::Exfat => "exFAT",
            FsKind::Ext2 => "ext2",
            FsKind::Ext3 => "ext3",
            FsKind::Ext4 => "ext4",
            FsKind::Refs => "ReFS",
            FsKind::HfsPlus => "HFS+",
            FsKind::Apfs => "APFS",
            FsKind::Raw => "Raw",
        }
    }
}

/// A file system found at the start of a volume.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystem {
    pub kind: FsKind,
    /// `kind` for people, e.g. "NTFS".
    pub name: String,
    /// Volume label, when the boot sector or superblock holds it.
    pub label: Option<String>,
    /// The size the file system says it has, in bytes.
    pub size: Option<u64>,
}

impl FileSystem {
    fn new(kind: FsKind, label: Option<String>, size: Option<u64>) -> Self {
        Self {
            kind,
            name: kind.name().to_owned(),
            label,
            size,
        }
    }
}

/// An NTFS boot sector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ntfs {
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    /// Sectors in the volume, not counting the backup boot sector at its end.
    pub total_sectors: u64,
    pub mft_lcn: u64,
    pub mft_mirror_lcn: u64,
}

impl Ntfs {
    /// The whole volume, backup boot sector included.
    pub fn size(&self) -> u64 {
        self.total_sectors
            .saturating_add(1)
            .saturating_mul(self.bytes_per_sector)
    }
}

/// A FAT12/16/32 boot sector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fat {
    pub kind: FsKind,
    pub bytes_per_sector: u64,
    pub reserved_sectors: u64,
    pub total_sectors: u64,
    pub media: u8,
    /// FAT32 only: where the backup boot sector is, in sectors from the start.
    pub backup_boot: Option<u64>,
    pub label: Option<String>,
}

impl Fat {
    pub fn size(&self) -> u64 {
        self.total_sectors.saturating_mul(self.bytes_per_sector)
    }
}

/// An exFAT boot sector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Exfat {
    pub bytes_per_sector: u64,
    pub volume_length: u64,
    pub fat_offset: u64,
}

impl Exfat {
    pub fn size(&self) -> u64 {
        self.volume_length.saturating_mul(self.bytes_per_sector)
    }
}

/// An ext2/3/4 superblock (the primary or a backup).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ext {
    pub kind: FsKind,
    pub block_size: u64,
    pub blocks: u64,
    pub blocks_per_group: u64,
    /// Which block group's copy this is; 0 for the primary.
    pub group: u64,
    pub uuid: [u8; 16],
    pub label: Option<String>,
}

impl Ext {
    pub fn size(&self) -> u64 {
        self.blocks.saturating_mul(self.block_size)
    }

    /// Where this superblock sits, in bytes from the start of the volume.
    pub fn offset_in_volume(&self) -> Option<u64> {
        if self.group == 0 {
            return Some(1024);
        }
        let block = self.group.checked_mul(self.blocks_per_group)?;
        if self.block_size == 1024 {
            // With 1 KiB blocks, block 0 is the boot block and groups start at block 1.
            block.checked_add(1)?.checked_mul(1024)
        } else {
            block.checked_mul(self.block_size)
        }
    }

    pub fn file_system(&self) -> FileSystem {
        FileSystem::new(self.kind, self.label.clone(), Some(self.size()))
    }
}

/// A boot sector of a file system that starts with one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Boot {
    Ntfs(Ntfs),
    Fat(Fat),
    Exfat(Exfat),
}

impl Boot {
    pub fn file_system(&self) -> FileSystem {
        match self {
            Boot::Ntfs(ntfs) => FileSystem::new(FsKind::Ntfs, None, Some(ntfs.size())),
            Boot::Fat(fat) => FileSystem::new(fat.kind, fat.label.clone(), Some(fat.size())),
            Boot::Exfat(exfat) => FileSystem::new(FsKind::Exfat, None, Some(exfat.size())),
        }
    }
}

pub(crate) fn parse_boot(sector: &[u8]) -> Option<Boot> {
    parse_ntfs(sector)
        .map(Boot::Ntfs)
        .or_else(|| parse_exfat(sector).map(Boot::Exfat))
        .or_else(|| parse_fat(sector).map(Boot::Fat))
}

fn valid_sector_size(bytes: u64) -> bool {
    matches!(bytes, 512 | 1024 | 2048 | 4096)
}

pub(crate) fn parse_ntfs(s: &[u8]) -> Option<Ntfs> {
    if !has(s, 3, b"NTFS    ") || !has(s, 510, &[0x55, 0xAA]) {
        return None;
    }
    let bytes_per_sector = u64::from(u16_at(s, 11)?);
    let raw = u8_at(s, 13)?;
    // Up to 128 sectors per cluster directly; above that, 2^(256 - value).
    let sectors_per_cluster = if raw <= 128 {
        u64::from(raw)
    } else {
        1u64.checked_shl(256 - u32::from(raw))?
    };
    let total_sectors = u64_at(s, 40)?;
    let mft_lcn = u64_at(s, 48)?;
    let mft_mirror_lcn = u64_at(s, 56)?;
    let plausible = valid_sector_size(bytes_per_sector)
        && sectors_per_cluster.is_power_of_two()
        && total_sectors > 0
        && mft_lcn > 0
        && mft_mirror_lcn > 0;
    plausible.then(|| Ntfs {
        bytes_per_sector,
        cluster_size: bytes_per_sector * sectors_per_cluster,
        total_sectors,
        mft_lcn,
        mft_mirror_lcn,
    })
}

pub(crate) fn parse_exfat(s: &[u8]) -> Option<Exfat> {
    if !has(s, 3, b"EXFAT   ") || !has(s, 510, &[0x55, 0xAA]) {
        return None;
    }
    if bytes(s, 11, 53)?.iter().any(|&b| b != 0) {
        return None;
    }
    let shift = u8_at(s, 108)?;
    let cluster_shift = u8_at(s, 109)?;
    let volume_length = u64_at(s, 72)?;
    let fat_offset = u64::from(u32_at(s, 80)?);
    let plausible = (9..=12).contains(&shift)
        && u32::from(shift) + u32::from(cluster_shift) <= 25
        && volume_length > 0
        && fat_offset >= 24;
    plausible.then(|| Exfat {
        bytes_per_sector: 1 << shift,
        volume_length,
        fat_offset,
    })
}

pub(crate) fn parse_fat(s: &[u8]) -> Option<Fat> {
    let jump = matches!(u8_at(s, 0)?, 0xEB | 0xE9);
    if !jump || !has(s, 510, &[0x55, 0xAA]) {
        return None;
    }
    let bytes_per_sector = u64::from(u16_at(s, 11)?);
    let sectors_per_cluster = u64::from(u8_at(s, 13)?);
    let reserved_sectors = u64::from(u16_at(s, 14)?);
    let fats = u64::from(u8_at(s, 16)?);
    let root_entries = u64::from(u16_at(s, 17)?);
    let total16 = u64::from(u16_at(s, 19)?);
    let media = u8_at(s, 21)?;
    let fat16_size = u64::from(u16_at(s, 22)?);
    let total32 = u64::from(u32_at(s, 32)?);
    let fat32_size = u64::from(u32_at(s, 36)?);

    let fat_size = if fat16_size != 0 {
        fat16_size
    } else {
        fat32_size
    };
    let total_sectors = if total16 != 0 { total16 } else { total32 };
    let plausible = valid_sector_size(bytes_per_sector)
        && sectors_per_cluster.is_power_of_two()
        && sectors_per_cluster <= 128
        && reserved_sectors >= 1
        && (1..=2).contains(&fats)
        && (media == 0xF0 || media >= 0xF8)
        && fat_size > 0
        && total_sectors > 0;
    if !plausible {
        return None;
    }

    // The FAT type follows from the cluster count (Microsoft's rule).
    let root_sectors = (root_entries * 32).div_ceil(bytes_per_sector);
    let metadata = reserved_sectors + fats * fat_size + root_sectors;
    let clusters = total_sectors.checked_sub(metadata)? / sectors_per_cluster;
    let kind = match clusters {
        0..4085 => FsKind::Fat12,
        4085..65525 => FsKind::Fat16,
        _ => FsKind::Fat32,
    };
    // FAT32 keeps the FAT size and root directory elsewhere.
    if kind == FsKind::Fat32 && (fat16_size != 0 || root_entries != 0) {
        return None;
    }
    let (signature_at, label_at) = if kind == FsKind::Fat32 {
        (66, 71)
    } else {
        (38, 43)
    };
    let label = if u8_at(s, signature_at) == Some(0x29) {
        bytes(s, label_at, 11)
            .and_then(text)
            .filter(|label| label != "NO NAME")
    } else {
        None
    };
    let backup_boot = if kind == FsKind::Fat32 {
        u16_at(s, 50)
            .map(u64::from)
            .filter(|&sector| sector != 0 && sector != 0xFFFF)
    } else {
        None
    };
    Some(Fat {
        kind,
        bytes_per_sector,
        reserved_sectors,
        total_sectors,
        media,
        backup_boot,
        label,
    })
}

/// Parses an ext2/3/4 superblock starting at `sb[0]`.
pub(crate) fn parse_ext(sb: &[u8]) -> Option<Ext> {
    if u16_at(sb, 56)? != 0xEF53 {
        return None;
    }
    let log_block_size = u32_at(sb, 24)?;
    let blocks_low = u64::from(u32_at(sb, 4)?);
    let blocks_per_group = u64::from(u32_at(sb, 32)?);
    let inodes_per_group = u32_at(sb, 40)?;
    let revision = u32_at(sb, 76)?;
    let compat = u32_at(sb, 92)?;
    let incompat = u32_at(sb, 96)?;
    let plausible = log_block_size <= 6
        && blocks_low > 0
        && blocks_per_group > 0
        && inodes_per_group > 0
        && revision <= 1;
    if !plausible {
        return None;
    }
    let blocks_high = if incompat & 0x80 != 0 {
        u64::from(u32_at(sb, 0x150)?)
    } else {
        0
    };
    let kind = if incompat & (0x40 | 0x80 | 0x200) != 0 {
        FsKind::Ext4
    } else if compat & 0x4 != 0 {
        FsKind::Ext3
    } else {
        FsKind::Ext2
    };
    Some(Ext {
        kind,
        block_size: 1024 << log_block_size,
        blocks: (blocks_high << 32) | blocks_low,
        blocks_per_group,
        group: u64::from(u16_at(sb, 90)?),
        uuid: array16(sb, 104)?,
        label: bytes(sb, 120, 16).and_then(text),
    })
}

/// Recognizes the file system whose first bytes are `data` (8 KiB is enough).
pub(crate) fn identify_bytes(data: &[u8]) -> Option<FileSystem> {
    if let Some(boot) = parse_boot(data) {
        return Some(boot.file_system());
    }
    if has(data, 3, b"ReFS\0\0\0\0") {
        return Some(FileSystem::new(FsKind::Refs, None, None));
    }
    if let Some(ext) = data.get(1024..).and_then(parse_ext) {
        return Some(ext.file_system());
    }
    if has(data, 1024, b"H+") || has(data, 1024, b"HX") {
        let size = u32_be(data, 1024 + 40)
            .zip(u32_be(data, 1024 + 44))
            .map(|(block_size, blocks)| u64::from(block_size) * u64::from(blocks));
        return Some(FileSystem::new(FsKind::HfsPlus, None, size));
    }
    if has(data, 32, b"NXSB") {
        let size = u32_at(data, 36)
            .zip(u64_at(data, 40))
            .map(|(block_size, blocks)| u64::from(block_size).saturating_mul(blocks));
        return Some(FileSystem::new(FsKind::Apfs, None, size));
    }
    None
}

/// Bytes read to recognize a file system.
pub(crate) const IDENTIFY_BYTES: u64 = 8192;

/// Recognizes the file system starting at byte `offset` of a disk, if any.
pub fn identify(disk: &Disk, offset: u64) -> Result<Option<FileSystem>> {
    let len = IDENTIFY_BYTES.min(disk.size().saturating_sub(offset));
    if len == 0 {
        return Ok(None);
    }
    let mut data = vec![0; usize::try_from(len).unwrap_or(0)];
    disk.read_at(offset, &mut data)?;
    Ok(identify_bytes(&data))
}

#[cfg(test)]
pub(crate) mod samples {
    //! Minimal but valid boot sectors and superblocks, for tests.

    fn put(buf: &mut [u8], offset: usize, bytes: &[u8]) {
        buf[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// An NTFS boot sector: 4 KiB clusters, MFT at cluster 4, mirror at 8.
    pub fn ntfs(total_sectors: u64) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        put(&mut s, 0, &[0xEB, 0x52, 0x90]);
        put(&mut s, 3, b"NTFS    ");
        put(&mut s, 11, &512u16.to_le_bytes());
        s[13] = 8;
        s[21] = 0xF8;
        put(&mut s, 40, &total_sectors.to_le_bytes());
        put(&mut s, 48, &4u64.to_le_bytes());
        put(&mut s, 56, &8u64.to_le_bytes());
        s[64] = 0xF6;
        put(&mut s, 510, &[0x55, 0xAA]);
        s
    }

    /// A FAT32 boot sector with 512-byte clusters and a backup at sector 6.
    pub fn fat32(total_sectors: u32, label: &[u8; 11]) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        put(&mut s, 0, &[0xEB, 0x58, 0x90]);
        put(&mut s, 3, b"mkfs.fat");
        put(&mut s, 11, &512u16.to_le_bytes());
        s[13] = 1;
        put(&mut s, 14, &32u16.to_le_bytes());
        s[16] = 2;
        s[21] = 0xF8;
        put(&mut s, 32, &total_sectors.to_le_bytes());
        let fat_sectors = (total_sectors / 128) + 1;
        put(&mut s, 36, &fat_sectors.to_le_bytes());
        put(&mut s, 44, &2u32.to_le_bytes());
        put(&mut s, 48, &1u16.to_le_bytes());
        put(&mut s, 50, &6u16.to_le_bytes());
        s[66] = 0x29;
        put(&mut s, 71, label);
        put(&mut s, 82, b"FAT32   ");
        put(&mut s, 510, &[0x55, 0xAA]);
        s
    }

    /// The first FAT sector matching `fat32` (media F8, end-of-chain markers).
    pub fn fat32_table() -> Vec<u8> {
        let mut s = vec![0u8; 512];
        put(&mut s, 0, &[0xF8, 0xFF, 0xFF, 0x0F, 0xFF, 0xFF, 0xFF, 0x0F]);
        s
    }

    /// An exFAT boot sector: 512-byte sectors, FAT at sector 128.
    pub fn exfat(volume_sectors: u64) -> Vec<u8> {
        let mut s = vec![0u8; 512];
        put(&mut s, 0, &[0xEB, 0x76, 0x90]);
        put(&mut s, 3, b"EXFAT   ");
        put(&mut s, 72, &volume_sectors.to_le_bytes());
        put(&mut s, 80, &128u32.to_le_bytes());
        put(&mut s, 84, &64u32.to_le_bytes());
        put(&mut s, 88, &256u32.to_le_bytes());
        put(&mut s, 96, &4u32.to_le_bytes());
        s[108] = 9;
        s[109] = 3;
        s[110] = 1;
        put(&mut s, 510, &[0x55, 0xAA]);
        s
    }

    pub fn exfat_table() -> Vec<u8> {
        let mut s = vec![0u8; 512];
        put(&mut s, 0, &[0xF8, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        s
    }

    /// An ext4 superblock (1024 bytes) for a volume of `blocks` 4 KiB blocks.
    pub fn ext4(blocks: u32, group: u16, label: &[u8]) -> Vec<u8> {
        let mut s = vec![0u8; 1024];
        put(&mut s, 0, &8192u32.to_le_bytes());
        put(&mut s, 4, &blocks.to_le_bytes());
        put(&mut s, 24, &2u32.to_le_bytes());
        put(&mut s, 32, &32768u32.to_le_bytes());
        put(&mut s, 40, &8192u32.to_le_bytes());
        put(&mut s, 56, &0xEF53u16.to_le_bytes());
        put(&mut s, 76, &1u32.to_le_bytes());
        put(&mut s, 90, &group.to_le_bytes());
        put(&mut s, 92, &0x3Cu32.to_le_bytes());
        put(&mut s, 96, &0x2C2u32.to_le_bytes());
        put(&mut s, 104, &[0xAB; 16]);
        put(&mut s, 120, label);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::samples;
    use super::*;

    #[test]
    fn recognizes_each_file_system() {
        let ntfs = identify_bytes(&samples::ntfs(98_303)).unwrap();
        assert_eq!((ntfs.kind, ntfs.size), (FsKind::Ntfs, Some(98_304 * 512)));

        let fat = identify_bytes(&samples::fat32(98_304, b"DR_FAT32   ")).unwrap();
        assert_eq!(fat.kind, FsKind::Fat32);
        assert_eq!(fat.label.as_deref(), Some("DR_FAT32"));
        assert_eq!(fat.size, Some(98_304 * 512));

        let exfat = identify_bytes(&samples::exfat(65_536)).unwrap();
        assert_eq!(
            (exfat.kind, exfat.size),
            (FsKind::Exfat, Some(65_536 * 512))
        );

        let mut volume = vec![0u8; 2048];
        volume[1024..].copy_from_slice(&samples::ext4(8192, 0, b"dr_ext4"));
        let ext = identify_bytes(&volume).unwrap();
        assert_eq!(ext.kind, FsKind::Ext4);
        assert_eq!(ext.label.as_deref(), Some("dr_ext4"));
        assert_eq!(ext.size, Some(8192 * 4096));
    }

    #[test]
    fn small_fat_volumes_are_fat12_or_fat16() {
        let mut floppy = samples::fat32(2880, b"FLOPPY     ");
        floppy[17..19].copy_from_slice(&224u16.to_le_bytes()); // root entries
        floppy[19..21].copy_from_slice(&2880u16.to_le_bytes());
        floppy[22..24].copy_from_slice(&9u16.to_le_bytes()); // FAT size
        floppy[38] = 0x29;
        floppy[43..54].copy_from_slice(b"FLOPPY     ");
        assert_eq!(parse_fat(&floppy).unwrap().kind, FsKind::Fat12);
    }

    #[test]
    fn rejects_lookalikes() {
        let mut s = samples::ntfs(1000);
        s[510] = 0;
        assert!(parse_boot(&s).is_none(), "no 55 AA");
        let mut s = samples::ntfs(1000);
        s[11] = 7;
        assert!(parse_ntfs(&s).is_none(), "odd sector size");
        // An NTFS boot sector must not pass for FAT (it has 0 reserved sectors).
        assert!(parse_fat(&samples::ntfs(1000)).is_none());
        let mut s = samples::exfat(1000);
        s[20] = 1;
        assert!(parse_exfat(&s).is_none(), "must-be-zero area isn't");
        assert!(identify_bytes(&[0u8; 8192]).is_none());
        assert!(identify_bytes(&[0xFFu8; 8192]).is_none());
        assert!(identify_bytes(&[]).is_none());
    }

    #[test]
    fn ext_backup_superblock_positions() {
        let backup = parse_ext(&samples::ext4(100_000, 1, b"x")).unwrap();
        assert_eq!(backup.offset_in_volume(), Some(32768 * 4096));
        let primary = parse_ext(&samples::ext4(100_000, 0, b"x")).unwrap();
        assert_eq!(primary.offset_in_volume(), Some(1024));
    }
}
