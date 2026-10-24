//! FAT32, from Microsoft's FAT specification (fatgen103, "Microsoft
//! Extensible Firmware Initiative FAT32 File System Specification").
//!
//! Layout: the reserved area (boot sector, FSInfo, a third boot sector,
//! their backups from sector 6, padding so the data area starts on a
//! cluster and 1 MiB boundary), two FATs, then the data area, whose first
//! cluster (2) is the root folder.

use super::{FormatOptions, FormatSummary, Region, check_label, time};
use crate::FormatKind;

const RESERVED: u64 = 32;
const FATS: u64 = 2;
const BACKUP_BOOT: u64 = 6;
/// More clusters than this and it's not FAT32 (the spec's limit).
const MAX_CLUSTERS: u64 = 0x0FFF_FFF5;
/// Fewer and every system takes it for FAT16.
const MIN_CLUSTERS: u64 = 65_525;
/// Clusters bigger than this aren't read everywhere.
const MAX_CLUSTER_BYTES: u64 = 32 * 1024;

/// The usual cluster size for a volume, as Windows picks them.
fn default_cluster(volume_bytes: u64) -> u64 {
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    match volume_bytes {
        b if b <= 64 * MIB => 512,
        b if b <= 128 * MIB => 1024,
        b if b <= 256 * MIB => 2048,
        b if b <= 8 * GIB => 4096,
        b if b <= 16 * GIB => 8192,
        b if b <= 32 * GIB => 16 * 1024,
        _ => 32 * 1024,
    }
}

struct Layout {
    sector: u64,
    total: u64,
    per_cluster: u64,
    reserved: u64,
    fat: u64,
    clusters: u64,
}

impl Layout {
    fn data_start(&self) -> u64 {
        self.reserved + FATS * self.fat
    }

    /// The reserved area grows until the data area starts on the boundary;
    /// the FATs shrink a little as it does.
    fn new(total: u64, sector: u64, per_cluster: u64) -> Layout {
        let align = if total * sector >= 64 << 20 {
            ((1 << 20) / sector).max(per_cluster)
        } else {
            per_cluster
        };
        let mut reserved = RESERVED;
        loop {
            let most_clusters = total.saturating_sub(reserved) / per_cluster;
            let fat = ((most_clusters + 2) * 4).div_ceil(sector);
            let start = reserved + FATS * fat;
            let aligned = start.next_multiple_of(align);
            if aligned == start {
                let clusters = total.saturating_sub(start) / per_cluster;
                return Layout {
                    sector,
                    total,
                    per_cluster,
                    reserved,
                    fat,
                    clusters,
                };
            }
            reserved += aligned - start;
        }
    }
}

pub(crate) fn plan(
    volume_bytes: u64,
    sector_size: u32,
    options: &FormatOptions,
    serial: u32,
) -> Result<(FormatSummary, Vec<Region>), String> {
    let sector = u64::from(sector_size);
    let total = volume_bytes / sector;
    if total > u64::from(u32::MAX) {
        return Err(format!(
            "FAT32 volumes can't be over {} sectors (2 TiB with 512-byte sectors). Use exFAT or NTFS.",
            u32::MAX
        ));
    }
    let label = options
        .label
        .as_deref()
        .map(|l| check_label(FormatKind::Fat32, l))
        .transpose()?
        .filter(|l| !l.is_empty());
    let wanted = match options.cluster_size {
        Some(size) => {
            let size = u64::from(size);
            if !size.is_power_of_two() || size < sector || size > MAX_CLUSTER_BYTES {
                return Err(format!(
                    "FAT32 clusters are {sector} bytes to 32 KiB, in powers of two."
                ));
            }
            size
        }
        None => default_cluster(volume_bytes).max(sector),
    };
    // Too many clusters for FAT32: bigger ones. Too few: smaller ones.
    let mut cluster = wanted;
    let mut layout = Layout::new(total, sector, cluster / sector);
    while layout.clusters > MAX_CLUSTERS && cluster < MAX_CLUSTER_BYTES {
        cluster *= 2;
        layout = Layout::new(total, sector, cluster / sector);
    }
    while layout.clusters < MIN_CLUSTERS && cluster > sector && options.cluster_size.is_none() {
        cluster /= 2;
        layout = Layout::new(total, sector, cluster / sector);
    }
    if layout.clusters < MIN_CLUSTERS {
        let smallest = (MIN_CLUSTERS * sector + (RESERVED + 2 * 1024) * sector) >> 20;
        return Err(format!(
            "The volume is too small for FAT32 with these clusters (it needs about {smallest} MiB \
             or more). Use exFAT."
        ));
    }
    if layout.clusters > MAX_CLUSTERS {
        return Err("The volume is too big for FAT32. Use exFAT or NTFS.".into());
    }
    let summary = FormatSummary {
        kind: FormatKind::Fat32,
        label: label.clone(),
        cluster_size: u32::try_from(cluster).unwrap_or(u32::MAX),
        clusters: layout.clusters,
        capacity: (layout.clusters - 1) * cluster,
        serial,
    };
    Ok((summary, regions(&layout, options, serial, label.as_deref())))
}

fn regions(
    layout: &Layout,
    options: &FormatOptions,
    serial: u32,
    label: Option<&str>,
) -> Vec<Region> {
    let sector = layout.sector;
    let ss = usize::try_from(sector).unwrap_or(512);
    let at = |lba: u64| lba * sector;
    let boot = boot_sector(layout, options, serial, label);
    let fsinfo = fsinfo(layout.clusters - 1, 3, ss);
    let mut third = vec![0u8; ss];
    third[510] = 0x55;
    third[511] = 0xAA;

    let mut regions = vec![
        // The whole reserved area, the FATs and the root cluster are
        // cleared first; then the structures go on top.
        Region::Zeros {
            offset: 0,
            len: at(layout.data_start() + layout.per_cluster),
        },
    ];
    for base in [0, BACKUP_BOOT] {
        regions.push(Region::Bytes {
            offset: at(base),
            data: [boot.clone(), fsinfo.clone(), third.clone()].concat(),
        });
    }
    // Entry 0 holds the media byte, 1 the end-of-chain mark, 2 the root
    // folder's one cluster.
    let mut first = vec![0u8; ss];
    first[0..4].copy_from_slice(&0x0FFF_FFF8u32.to_le_bytes());
    first[4..8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
    first[8..12].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
    for copy in 0..FATS {
        regions.push(Region::Bytes {
            offset: at(layout.reserved + copy * layout.fat),
            data: first.clone(),
        });
    }
    if let Some(label) = label {
        let mut entry = vec![0u8; 32];
        entry[..11].copy_from_slice(&padded(label));
        entry[11] = 0x08; // volume label
        let (date, time) = time::dos_now();
        for (offset, word) in [(14, time), (16, date), (18, date), (22, time), (24, date)] {
            entry[offset..offset + 2].copy_from_slice(&word.to_le_bytes());
        }
        regions.push(Region::Bytes {
            offset: at(layout.data_start()),
            data: entry,
        });
    }
    regions
}

/// An 11-byte label, padded with spaces.
fn padded(label: &str) -> [u8; 11] {
    let mut bytes = [b' '; 11];
    for (slot, b) in bytes.iter_mut().zip(label.bytes()) {
        *slot = b;
    }
    bytes
}

fn boot_sector(
    layout: &Layout,
    options: &FormatOptions,
    serial: u32,
    label: Option<&str>,
) -> Vec<u8> {
    let mut s = vec![0u8; usize::try_from(layout.sector).unwrap_or(512)];
    let mut put = |at: usize, bytes: &[u8]| s[at..at + bytes.len()].copy_from_slice(bytes);
    let u16_of = |v: u64| u16::try_from(v).unwrap_or(u16::MAX).to_le_bytes();
    let u32_of = |v: u64| u32::try_from(v).unwrap_or(u32::MAX).to_le_bytes();
    put(0, &[0xEB, 0x58, 0x90]);
    put(3, b"MSWIN4.1");
    put(11, &u16_of(layout.sector));
    put(13, &[u8::try_from(layout.per_cluster).unwrap_or(1)]);
    put(14, &u16_of(layout.reserved));
    put(16, &[2]); // FATs
    put(21, &[0xF8]); // media: fixed disk
    put(24, &63u16.to_le_bytes()); // sectors per track
    put(26, &255u16.to_le_bytes()); // heads
    put(28, &u32_of(options.partition_offset)); // hidden sectors
    put(32, &u32_of(layout.total));
    put(36, &u32_of(layout.fat));
    put(44, &2u32.to_le_bytes()); // root folder cluster
    put(48, &1u16.to_le_bytes()); // FSInfo sector
    put(50, &u16_of(BACKUP_BOOT));
    put(64, &[0x80]); // drive number
    put(66, &[0x29]); // extended boot signature
    put(67, &serial.to_le_bytes());
    put(71, &label.map_or(*b"NO NAME    ", padded));
    put(82, b"FAT32   ");
    // Not bootable: INT 18h asks the BIOS to try the next device, then halt.
    put(90, &[0xCD, 0x18, 0xF4, 0xEB, 0xFD]);
    put(510, &[0x55, 0xAA]);
    s
}

/// The FSInfo sector: free clusters, and where to look for a free one.
fn fsinfo(free: u64, next: u32, sector: usize) -> Vec<u8> {
    let mut s = vec![0u8; sector];
    s[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
    s[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
    s[488..492].copy_from_slice(&u32::try_from(free).unwrap_or(u32::MAX).to_le_bytes());
    s[492..496].copy_from_slice(&next.to_le_bytes());
    s[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    s
}
