//! exFAT, from Microsoft's "exFAT file system specification".
//!
//! Layout: the main boot region (sectors 0–11: boot sector, eight
//! extended boot sectors, OEM parameters, a reserved sector, and the boot
//! checksum), its backup (12–23), padding, one FAT, padding, then the
//! cluster heap: the allocation bitmap, the up-case table and the root
//! folder, in that order from cluster 2.

use super::{FormatOptions, FormatSummary, Region, check_label, rotate_add, upcase};
use crate::FormatKind;

const BOOT_REGION: u64 = 12;
/// The most clusters exFAT allows.
const MAX_CLUSTERS: u64 = 0xFFFF_FFF5;
/// Clusters are at most 32 MiB.
const MAX_CLUSTER_BYTES: u64 = 32 << 20;

const ENTRY_BITMAP: u8 = 0x81;
const ENTRY_UPCASE: u8 = 0x82;
const ENTRY_LABEL: u8 = 0x83;

/// The usual cluster size for a volume, as Windows picks them.
fn default_cluster(volume_bytes: u64) -> u64 {
    const MIB: u64 = 1 << 20;
    match volume_bytes {
        b if b <= 256 * MIB => 4096,
        b if b <= 32 * 1024 * MIB => 32 * 1024,
        _ => 128 * 1024,
    }
}

struct Layout {
    sector: u64,
    total: u64,
    per_cluster: u64,
    fat_offset: u64,
    fat_length: u64,
    heap_offset: u64,
    clusters: u64,
}

impl Layout {
    fn new(total: u64, sector: u64, per_cluster: u64) -> Layout {
        let align = if total * sector >= 64 << 20 {
            (1 << 20) / sector
        } else {
            (32 * 1024 / sector).max(1)
        };
        let fat_offset = (2 * BOOT_REGION).next_multiple_of(align);
        let mut fat_length = 1;
        loop {
            let heap_offset = (fat_offset + fat_length).next_multiple_of(align.max(per_cluster));
            let clusters = total.saturating_sub(heap_offset) / per_cluster;
            let needed = ((clusters + 2) * 4).div_ceil(sector);
            if needed <= fat_length {
                return Layout {
                    sector,
                    total,
                    per_cluster,
                    fat_offset,
                    fat_length,
                    heap_offset,
                    clusters,
                };
            }
            fat_length = needed;
        }
    }

    fn cluster_bytes(&self) -> u64 {
        self.per_cluster * self.sector
    }

    /// Byte offset of a cluster, from the volume's start.
    fn cluster_at(&self, cluster: u64) -> u64 {
        (self.heap_offset + (cluster - 2) * self.per_cluster) * self.sector
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
    if volume_bytes < 1 << 20 {
        return Err("exFAT volumes are at least 1 MiB.".into());
    }
    let label = options
        .label
        .as_deref()
        .map(|l| check_label(FormatKind::Exfat, l))
        .transpose()?
        .filter(|l| !l.is_empty());
    let mut cluster = match options.cluster_size {
        Some(size) => {
            let size = u64::from(size);
            if !size.is_power_of_two() || size < sector || size > MAX_CLUSTER_BYTES {
                return Err(format!(
                    "exFAT clusters are {sector} bytes to 32 MiB, in powers of two."
                ));
            }
            size
        }
        None => default_cluster(volume_bytes).max(sector),
    };
    let mut layout = Layout::new(total, sector, cluster / sector);
    while layout.clusters > MAX_CLUSTERS && cluster < MAX_CLUSTER_BYTES {
        cluster *= 2;
        layout = Layout::new(total, sector, cluster / sector);
    }
    let upcase = upcase::table();
    let bitmap_bytes = layout.clusters.div_ceil(8);
    let clusters_for = |bytes: u64| bytes.div_ceil(layout.cluster_bytes()).max(1);
    let bitmap_clusters = clusters_for(bitmap_bytes);
    let upcase_clusters = clusters_for(upcase.len() as u64);
    let used = bitmap_clusters + upcase_clusters + 1;
    if layout.clusters <= used + 1 {
        return Err("The volume is too small for exFAT with these clusters.".into());
    }
    let summary = FormatSummary {
        kind: FormatKind::Exfat,
        label: label.clone(),
        cluster_size: u32::try_from(cluster).unwrap_or(u32::MAX),
        clusters: layout.clusters,
        capacity: (layout.clusters - used) * cluster,
        serial,
    };

    let bitmap_first = 2;
    let upcase_first = bitmap_first + bitmap_clusters;
    let root = upcase_first + upcase_clusters;
    let percent = u8::try_from(used * 100 / layout.clusters).unwrap_or(100);
    let boot = boot_region(&layout, options.partition_offset, serial, root, percent);

    let mut regions = vec![
        // Everything before the heap, and the heap's first clusters, cleared.
        Region::Zeros {
            offset: 0,
            len: layout.cluster_at(root + 1),
        },
        Region::Bytes {
            offset: 0,
            data: boot.clone(),
        },
        Region::Bytes {
            offset: BOOT_REGION * layout.sector,
            data: boot,
        },
    ];

    // The FAT: the media mark, then a chain for each of the three.
    let mut fat = vec![0u32; usize::try_from(root + 1).unwrap_or(0)];
    fat[0] = 0xFFFF_FFF8;
    fat[1] = 0xFFFF_FFFF;
    for (first, count) in [
        (bitmap_first, bitmap_clusters),
        (upcase_first, upcase_clusters),
        (root, 1),
    ] {
        for c in first..first + count {
            let next = if c + 1 == first + count {
                0xFFFF_FFFF
            } else {
                u32::try_from(c + 1).unwrap_or(0)
            };
            fat[usize::try_from(c).unwrap_or(0)] = next;
        }
    }
    regions.push(Region::Bytes {
        offset: layout.fat_offset * layout.sector,
        data: fat.iter().flat_map(|e| e.to_le_bytes()).collect(),
    });

    // The bitmap: the clusters those three use.
    let mut bitmap = vec![0u8; usize::try_from(bitmap_bytes).unwrap_or(0)];
    for index in 0..used {
        bitmap[usize::try_from(index / 8).unwrap_or(0)] |= 1 << (index % 8);
    }
    regions.push(Region::Bytes {
        offset: layout.cluster_at(bitmap_first),
        data: bitmap,
    });
    let checksum = upcase::checksum(&upcase);
    let upcase_len = upcase.len() as u64;
    regions.push(Region::Bytes {
        offset: layout.cluster_at(upcase_first),
        data: upcase,
    });

    // The root folder: the label, the bitmap and the up-case table.
    let mut entries = Vec::new();
    if let Some(label) = &label {
        let mut entry = [0u8; 32];
        entry[0] = ENTRY_LABEL;
        let units: Vec<u16> = label.encode_utf16().collect();
        entry[1] = u8::try_from(units.len()).unwrap_or(11);
        for (i, unit) in units.iter().enumerate() {
            entry[2 + 2 * i..4 + 2 * i].copy_from_slice(&unit.to_le_bytes());
        }
        entries.extend_from_slice(&entry);
    }
    let mut entry = [0u8; 32];
    entry[0] = ENTRY_BITMAP;
    entry[20..24].copy_from_slice(&u32::try_from(bitmap_first).unwrap_or(2).to_le_bytes());
    entry[24..32].copy_from_slice(&bitmap_bytes.to_le_bytes());
    entries.extend_from_slice(&entry);
    let mut entry = [0u8; 32];
    entry[0] = ENTRY_UPCASE;
    entry[4..8].copy_from_slice(&checksum.to_le_bytes());
    entry[20..24].copy_from_slice(&u32::try_from(upcase_first).unwrap_or(0).to_le_bytes());
    entry[24..32].copy_from_slice(&upcase_len.to_le_bytes());
    entries.extend_from_slice(&entry);
    regions.push(Region::Bytes {
        offset: layout.cluster_at(root),
        data: entries,
    });
    Ok((summary, regions))
}

/// The 12 sectors of a boot region: boot sector, 8 extended boot sectors,
/// OEM parameters, a reserved sector, and the checksum sector.
fn boot_region(
    layout: &Layout,
    partition_offset: u64,
    serial: u32,
    root: u64,
    percent: u8,
) -> Vec<u8> {
    let ss = usize::try_from(layout.sector).unwrap_or(512);
    let mut region = vec![0u8; 12 * ss];
    {
        let s = &mut region[..ss];
        let mut put = |at: usize, bytes: &[u8]| s[at..at + bytes.len()].copy_from_slice(bytes);
        let u32_of = |v: u64| u32::try_from(v).unwrap_or(u32::MAX).to_le_bytes();
        put(0, &[0xEB, 0x76, 0x90]);
        put(3, b"EXFAT   ");
        put(64, &partition_offset.to_le_bytes());
        put(72, &layout.total.to_le_bytes());
        put(80, &u32_of(layout.fat_offset));
        put(84, &u32_of(layout.fat_length));
        put(88, &u32_of(layout.heap_offset));
        put(92, &u32_of(layout.clusters));
        put(96, &u32_of(root));
        put(100, &serial.to_le_bytes());
        put(104, &0x0100u16.to_le_bytes()); // revision 1.00
        put(
            108,
            &[u8::try_from(layout.sector.trailing_zeros()).unwrap_or(9)],
        );
        put(
            109,
            &[u8::try_from(layout.per_cluster.trailing_zeros()).unwrap_or(0)],
        );
        put(110, &[1]); // FATs
        put(111, &[0x80]); // drive select
        put(112, &[percent]);
        // Not bootable: INT 18h asks the BIOS to try the next device, then halt.
        put(120, &[0xCD, 0x18, 0xF4, 0xEB, 0xFD]);
        put(510, &[0x55, 0xAA]);
    }
    // Each extended boot sector ends with its signature.
    for n in 1..=8 {
        let end = (n + 1) * ss;
        region[end - 4..end].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
    }
    // The checksum of sectors 0–10, leaving out VolumeFlags and
    // PercentInUse, which change while the volume is used.
    let sum = region[..11 * ss]
        .iter()
        .enumerate()
        .filter(|(i, _)| !matches!(i, 106 | 107 | 112))
        .fold(0u32, |sum, (_, &b)| rotate_add(sum, b));
    for word in region[11 * ss..].chunks_mut(4) {
        word.copy_from_slice(&sum.to_le_bytes());
    }
    region
}
