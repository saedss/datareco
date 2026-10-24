//! The map of a disk: its partitions and the free space between them, in
//! disk order, with the volumes Windows has mounted on them.

use datareco_io::{DeviceList, Disk, Result, SourceSpec, VolumeInfo};
use serde::Serialize;

use crate::table::{Partition, PartitionKind, PartitionTable, TableKind, read_table};

/// New partitions start on 1 MiB boundaries, as Windows and Linux tools
/// make them. Free space with less than this left after aligning isn't shown.
pub const ALIGNMENT_BYTES: u64 = 1 << 20;

/// MBR entries hold 32-bit sector numbers, so an MBR reaches 2^32 sectors
/// (2 TiB with 512-byte sectors).
pub const MBR_SECTOR_LIMIT: u64 = 1 << 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SegmentKind {
    /// A partition in the table. The extended partition itself isn't one:
    /// its logical partitions and free space are.
    Partition,
    /// Space no partition uses, big enough for a new one.
    Free,
    /// Space no partition uses that the partition table can't reach.
    Unusable,
    /// No partition table: one volume fills the disk.
    WholeDisk,
}

/// A stretch of the disk on its map.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub kind: SegmentKind,
    pub start_lba: u64,
    pub sectors: u64,
    /// The partition's number in the table (`Partition` segments).
    pub partition: Option<u32>,
    /// Inside the extended partition: a logical partition, or room for one.
    pub logical: bool,
    /// Why the space can't be used (`Unusable` segments).
    pub note: Option<String>,
    /// The volume the OS has mounted here, if any.
    pub volume: Option<VolumeInfo>,
}

impl Segment {
    pub fn end_lba(&self) -> u64 {
        self.start_lba.saturating_add(self.sectors)
    }

    fn new(kind: SegmentKind, start_lba: u64, end_lba: u64, logical: bool) -> Segment {
        Segment {
            kind,
            start_lba,
            sectors: end_lba.saturating_sub(start_lba),
            partition: None,
            logical,
            note: None,
            volume: None,
        }
    }
}

/// A disk's partition table and map.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskMap {
    pub table: PartitionTable,
    pub segments: Vec<Segment>,
}

/// Reads the partition table and maps the disk. With `devices`, each
/// partition gets the volume the OS has mounted on it (drive letter, label,
/// free space). Never writes.
pub fn disk_map(disk: &Disk, spec: &SourceSpec, devices: Option<&DeviceList>) -> Result<DiskMap> {
    let table = read_table(disk)?;
    let mut segments = layout(&table);
    if let Some(devices) = devices {
        attach_volumes(&mut segments, table.sector_size, spec, devices);
    }
    Ok(DiskMap { table, segments })
}

/// The disk's partitions and free space, in disk order. Overlapping
/// partitions (a damaged table, flagged by its problems) are listed as
/// they are; free space is only what no partition covers.
pub fn layout(table: &PartitionTable) -> Vec<Segment> {
    let disk = table.disk_sectors;
    let align = (ALIGNMENT_BYTES / u64::from(table.sector_size.max(1))).max(1);
    let mut map = Mapper {
        align,
        mbr: table.kind == TableKind::Mbr,
        sector_size: table.sector_size,
        segments: Vec::new(),
    };
    let (first, end) = match table.kind {
        TableKind::None => {
            let kind = if table.whole_disk.is_some() {
                SegmentKind::WholeDisk
            } else {
                SegmentKind::Free
            };
            if disk > 0 {
                map.segments.push(Segment::new(kind, 0, disk, false));
            }
            return map.segments;
        }
        TableKind::Gpt => table
            .usable
            .map_or((1, disk), |u| (u.first, u.last.saturating_add(1).min(disk))),
        TableKind::Mbr => (1, disk),
    };

    let extended: Vec<&Partition> = table
        .partitions
        .iter()
        .filter(|p| p.kind == PartitionKind::Extended && p.sectors > 0)
        .collect();
    let inside = |p: &Partition| {
        extended
            .iter()
            .any(|e| p.start_lba >= e.start_lba && p.start_lba < e.end_lba())
    };
    // Logical partitions outside every extended partition (a damaged
    // table) are drawn where they are, among the primary ones.
    let mut top: Vec<&Partition> = table
        .partitions
        .iter()
        .filter(|p| p.sectors > 0 && (p.kind != PartitionKind::Logical || !inside(p)))
        .collect();
    top.sort_by_key(|p| (p.start_lba, p.number));

    let mut cursor = first;
    for p in top {
        map.free(cursor, p.start_lba.min(end), false);
        if p.kind == PartitionKind::Extended {
            let mut logical: Vec<&Partition> = table
                .partitions
                .iter()
                .filter(|l| {
                    l.kind == PartitionKind::Logical
                        && l.sectors > 0
                        && l.start_lba >= p.start_lba
                        && l.start_lba < p.end_lba()
                })
                .collect();
            logical.sort_by_key(|l| (l.start_lba, l.number));
            let mut inner = p.start_lba;
            for l in logical {
                map.free(inner, l.start_lba, true);
                map.partition(l, true);
                inner = inner.max(l.end_lba());
            }
            map.free(inner, p.end_lba().min(end), true);
        } else {
            map.partition(p, p.kind == PartitionKind::Logical);
        }
        cursor = cursor.max(p.end_lba());
    }
    map.free(cursor, end, false);
    map.segments
}

struct Mapper {
    align: u64,
    mbr: bool,
    sector_size: u32,
    segments: Vec<Segment>,
}

impl Mapper {
    fn partition(&mut self, p: &Partition, logical: bool) {
        let mut segment = Segment::new(SegmentKind::Partition, p.start_lba, p.end_lba(), logical);
        segment.partition = Some(p.number);
        self.segments.push(segment);
    }

    /// The gap from `start` to `end`, if a partition fits in it once its
    /// start is aligned (inside the extended partition, after the extended
    /// boot record each logical partition needs before it).
    fn free(&mut self, start: u64, end: u64, logical: bool) {
        if end <= start {
            return;
        }
        if self.mbr && end > MBR_SECTOR_LIMIT {
            let limit = MBR_SECTOR_LIMIT.max(start);
            self.free(start, limit, logical);
            if end - limit >= self.align {
                let mut segment = Segment::new(SegmentKind::Unusable, limit, end, logical);
                segment.note = Some(format!(
                    "An MBR partition table can't reach past {}; converting the disk to GPT \
                     would let this space be used.",
                    crate::table::human_size(MBR_SECTOR_LIMIT * u64::from(self.sector_size))
                ));
                self.segments.push(segment);
            }
            return;
        }
        let first = if logical { start + 1 } else { start };
        let aligned = first.next_multiple_of(self.align);
        if end > aligned && end - aligned >= self.align {
            self.segments
                .push(Segment::new(SegmentKind::Free, start, end, logical));
        }
    }
}

/// Gives each partition the volume mounted on it: on a physical disk, the
/// volume whose extent starts where the partition does; on a volume, the
/// volume itself.
fn attach_volumes(
    segments: &mut [Segment],
    sector_size: u32,
    spec: &SourceSpec,
    devices: &DeviceList,
) {
    match spec {
        SourceSpec::PhysicalDisk(number) => {
            for segment in segments
                .iter_mut()
                .filter(|s| s.kind == SegmentKind::Partition)
            {
                let offset = segment.start_lba.saturating_mul(u64::from(sector_size));
                segment.volume = devices
                    .volumes
                    .iter()
                    .find(|v| {
                        v.extents
                            .iter()
                            .any(|e| e.disk == *number && e.offset == offset)
                    })
                    .cloned();
            }
        }
        SourceSpec::Volume(name) => {
            if let [segment] = segments {
                segment.volume = devices.volume(name).cloned();
            }
        }
        SourceSpec::Image(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use datareco_io::DiskExtent;

    use super::*;
    use crate::table::UsableSectors;

    const MIB: u64 = 2048;

    fn partition(number: u32, kind: PartitionKind, start: u64, sectors: u64) -> Partition {
        Partition {
            number,
            kind,
            start_lba: start,
            sectors,
            type_code: String::new(),
            type_name: String::new(),
            name: None,
            guid: None,
            bootable: false,
            hidden: false,
            file_system: None,
        }
    }

    fn table(kind: TableKind, disk_sectors: u64, partitions: Vec<Partition>) -> PartitionTable {
        PartitionTable {
            kind,
            sector_size: 512,
            disk_sectors,
            disk_id: None,
            partitions,
            whole_disk: None,
            usable: (kind == TableKind::Gpt).then_some(UsableSectors {
                first: 34,
                last: disk_sectors - 34,
            }),
            problems: Vec::new(),
        }
    }

    /// (kind, start, end, partition, logical) for each segment.
    fn shape(segments: &[Segment]) -> Vec<(SegmentKind, u64, u64, Option<u32>, bool)> {
        segments
            .iter()
            .map(|s| (s.kind, s.start_lba, s.end_lba(), s.partition, s.logical))
            .collect()
    }

    use PartitionKind::{Extended, Gpt, Logical, Primary};
    use SegmentKind::{Free, Partition as Part, Unusable, WholeDisk};

    #[test]
    fn gpt_gaps_between_and_after_partitions_are_free() {
        let t = table(
            TableKind::Gpt,
            1000 * MIB,
            vec![
                partition(1, Gpt, MIB, 100 * MIB),
                partition(2, Gpt, 300 * MIB, 200 * MIB),
            ],
        );
        assert_eq!(
            shape(&layout(&t)),
            [
                (Part, MIB, 101 * MIB, Some(1), false),
                (Free, 101 * MIB, 300 * MIB, None, false),
                (Part, 300 * MIB, 500 * MIB, Some(2), false),
                (Free, 500 * MIB, 1000 * MIB - 33, None, false),
            ]
        );
    }

    #[test]
    fn slivers_too_small_for_a_partition_are_left_out() {
        // The usual 1 MiB before the first partition, and half a MiB
        // between two partitions and at the end.
        let t = table(
            TableKind::Gpt,
            201 * MIB,
            vec![
                partition(1, Gpt, MIB, 100 * MIB - 1024),
                partition(2, Gpt, 101 * MIB, 99 * MIB + 1024),
            ],
        );
        assert_eq!(
            shape(&layout(&t)),
            [
                (Part, MIB, 101 * MIB - 1024, Some(1), false),
                (Part, 101 * MIB, 200 * MIB + 1024, Some(2), false),
            ]
        );
    }

    #[test]
    fn mbr_logical_partitions_and_free_space_inside_the_extended_one() {
        let t = table(
            TableKind::Mbr,
            1000 * MIB,
            vec![
                partition(1, Primary, MIB, 99 * MIB),
                partition(2, Extended, 100 * MIB, 500 * MIB),
                partition(5, Logical, 101 * MIB, 99 * MIB),
                partition(6, Logical, 301 * MIB, 99 * MIB),
            ],
        );
        assert_eq!(
            shape(&layout(&t)),
            [
                (Part, MIB, 100 * MIB, Some(1), false),
                (Part, 101 * MIB, 200 * MIB, Some(5), true),
                (Free, 200 * MIB, 301 * MIB, None, true),
                (Part, 301 * MIB, 400 * MIB, Some(6), true),
                (Free, 400 * MIB, 600 * MIB, None, true),
                (Free, 600 * MIB, 1000 * MIB, None, false),
            ]
        );
    }

    #[test]
    fn mbr_space_past_2_tib_is_unusable() {
        let disk = MBR_SECTOR_LIMIT + 1000 * MIB;
        let t = table(
            TableKind::Mbr,
            disk,
            vec![partition(1, Primary, MIB, 1000 * MIB)],
        );
        let segments = layout(&t);
        assert_eq!(
            shape(&segments),
            [
                (Part, MIB, 1001 * MIB, Some(1), false),
                (Free, 1001 * MIB, MBR_SECTOR_LIMIT, None, false),
                (Unusable, MBR_SECTOR_LIMIT, disk, None, false),
            ]
        );
        assert!(segments[2].note.as_deref().unwrap().contains("2.0 TiB"));
    }

    #[test]
    fn overlapping_partitions_are_listed_as_they_are() {
        let t = table(
            TableKind::Mbr,
            1000 * MIB,
            vec![
                partition(1, Primary, MIB, 500 * MIB),
                partition(2, Primary, 400 * MIB, 200 * MIB),
            ],
        );
        assert_eq!(
            shape(&layout(&t)),
            [
                (Part, MIB, 501 * MIB, Some(1), false),
                (Part, 400 * MIB, 600 * MIB, Some(2), false),
                (Free, 600 * MIB, 1000 * MIB, None, false),
            ]
        );
    }

    #[test]
    fn disks_without_a_table_are_one_volume_or_empty() {
        let mut t = table(TableKind::None, 100 * MIB, Vec::new());
        assert_eq!(shape(&layout(&t)), [(Free, 0, 100 * MIB, None, false)]);
        t.whole_disk = Some(crate::FileSystem {
            kind: crate::FsKind::Ntfs,
            name: "NTFS".to_owned(),
            label: None,
            size: None,
        });
        assert_eq!(shape(&layout(&t)), [(WholeDisk, 0, 100 * MIB, None, false)]);
    }

    #[test]
    fn volumes_are_matched_to_partitions_by_where_they_start() {
        let t = table(
            TableKind::Gpt,
            1000 * MIB,
            vec![
                partition(1, Gpt, MIB, 100 * MIB),
                partition(2, Gpt, 300 * MIB, 200 * MIB),
            ],
        );
        let volume = |path: &str, mount: &str, disk: u32, offset: u64| VolumeInfo {
            path: path.to_owned(),
            mount_points: vec![mount.to_owned()],
            label: None,
            file_system: Some("NTFS".to_owned()),
            size: Some(200 * MIB * 512),
            free: Some(MIB * 512),
            extents: vec![DiskExtent {
                disk,
                offset,
                length: 200 * MIB * 512,
            }],
        };
        let devices = DeviceList {
            disks: Vec::new(),
            volumes: vec![
                volume(r"\\?\Volume{d}", "D:\\", 1, 300 * MIB * 512),
                volume(r"\\?\Volume{e}", "E:\\", 2, MIB * 512),
            ],
        };
        let mut segments = layout(&t);
        attach_volumes(&mut segments, 512, &SourceSpec::PhysicalDisk(1), &devices);
        let mounts: Vec<_> = segments
            .iter()
            .map(|s| s.volume.as_ref().map(|v| v.mount_points[0].as_str()))
            .collect();
        assert_eq!(mounts, [None, None, Some("D:\\"), None]);

        let mut whole = layout(&table(TableKind::None, 100 * MIB, Vec::new()));
        attach_volumes(
            &mut whole,
            512,
            &SourceSpec::Volume("E:".to_owned()),
            &devices,
        );
        assert_eq!(whole[0].volume.as_ref().unwrap().path, r"\\?\Volume{e}");
    }
}
