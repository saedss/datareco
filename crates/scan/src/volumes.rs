//! Turning a scan's clues into volumes that open, best first.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use datareco_fs::{
    FatLayout, FatRoot, FileSystem, FsError, FsKind, Rebuild, can_read, clues, open_rebuilt,
};
use datareco_io::Disk;
use datareco_partitions::{FoundPartition, PartitionTable, identify};
use serde::Serialize;

use crate::scan::{FolderClue, RecordClue, ScanError, TableClue};

/// Cluster sizes a FAT volume can have.
const FAT_CLUSTERS: std::ops::RangeInclusive<u64> = 512..=256 * 1024;
/// Root folder sizes FAT12/16 volumes are usually made with (entries).
const ROOT_ENTRIES: [u64; 5] = [512, 224, 112, 1024, 256];

/// How a found volume opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum VolumeState {
    /// With its own boot sector.
    Intact,
    /// Its boot sector is gone or was overwritten (by a format, say); a
    /// backup copy opens it.
    FromBackup,
    /// With no boot sector at all: put together from folders found on the disk.
    Rebuilt,
}

/// A volume a scan found.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoundVolume {
    pub kind: FsKind,
    /// `kind` for people, e.g. "NTFS".
    pub name: String,
    pub label: Option<String>,
    /// Byte offset on the disk, and size in bytes.
    pub offset: u64,
    pub size: u64,
    pub state: VolumeState,
    /// 0-100: how much of the volume's own structure is there. Higher first.
    pub score: u32,
    /// The current partition table lists it.
    pub in_table: bool,
    /// MFT records (NTFS) or folder sectors (FAT) the scan found in it.
    pub found: Option<u64>,
    /// Why DataReco thinks the volume is there, one line each.
    pub evidence: Vec<String>,
    /// What opening it needs.
    #[serde(skip)]
    pub rebuild: Rebuild,
}

/// Opens a volume a scan found.
pub fn open_found(disk: Arc<Disk>, volume: &FoundVolume) -> Result<Box<dyn FileSystem>, FsError> {
    open_rebuilt(
        disk,
        volume.kind,
        volume.offset,
        volume.size,
        &volume.rebuild,
    )
}

pub(crate) struct Clues<'a> {
    pub partitions: &'a [FoundPartition],
    pub table: Option<&'a PartitionTable>,
    pub records: &'a [RecordClue],
    pub folders: &'a [FolderClue],
    pub tables: &'a [TableClue],
}

fn is_fat(kind: FsKind) -> bool {
    matches!(kind, FsKind::Fat12 | FsKind::Fat16 | FsKind::Fat32)
}

pub(crate) fn build(disk: &Arc<Disk>, clues: &Clues) -> Result<Vec<FoundVolume>, ScanError> {
    let sector = u64::from(disk.sector_size());
    let mut found: Vec<FoundVolume> = Vec::new();
    for partition in clues.partitions {
        let Some(fs) = &partition.file_system else {
            continue;
        };
        if !can_read(fs.kind) {
            continue;
        }
        let offset = partition.start_lba * sector;
        let size = partition.sectors * sector;
        if let Some(volume) = from_boot_sector(disk, partition, fs.kind, offset, size, clues)? {
            found.push(volume);
        }
    }
    let rebuilt = rebuilt_fats(disk, clues, &found)?;
    found.extend(rebuilt);

    // The same volume can come from several clues: keep the best.
    found.sort_by(|a, b| b.score.cmp(&a.score).then(a.offset.cmp(&b.offset)));
    let mut seen = HashSet::new();
    found.retain(|v| seen.insert((v.kind, v.offset)));
    Ok(found)
}

/// A volume opened with its own boot sector, or else a backup copy.
fn from_boot_sector(
    disk: &Arc<Disk>,
    partition: &FoundPartition,
    kind: FsKind,
    offset: u64,
    size: u64,
    clues: &Clues,
) -> Result<Option<FoundVolume>, ScanError> {
    let sector = u64::from(disk.sector_size());
    let records = if kind == FsKind::Ntfs {
        records_in(clues.records, offset, size)
    } else {
        BTreeMap::new()
    };
    let mut attempts = Vec::new();
    if identify(disk, offset)?.is_some_and(|fs| fs.kind == kind) {
        attempts.push((None, VolumeState::Intact));
    }
    let ext = matches!(kind, FsKind::Ext2 | FsKind::Ext3 | FsKind::Ext4);
    let backups = match kind {
        FsKind::Ntfs => vec![offset + size.saturating_sub(sector)],
        FsKind::Fat32 => vec![offset + 6 * sector],
        FsKind::Exfat => vec![offset + 12 * sector],
        // Block group 1's superblock, for each usual block size (groups of
        // 8 x block size blocks; with 1 KiB blocks, groups start at block 1).
        _ if ext => vec![
            offset + 8193 * 1024,
            offset + 16384 * 2048,
            offset + 32768 * 4096,
        ],
        // The superblock copy 64 MiB in.
        FsKind::Btrfs => vec![offset + 0x400_0000],
        _ => Vec::new(),
    };
    for backup in backups {
        // An ext superblock sits 1024 bytes into what `identify` looks at.
        let looked_at = if ext { backup - 1024 } else { backup };
        let inside = !ext || backup + 1024 <= offset.saturating_add(size);
        // A btrfs copy can't be recognized on its own here; opening it says.
        let recognized =
            kind == FsKind::Btrfs || identify(disk, looked_at)?.is_some_and(|fs| fs.kind == kind);
        // How much of the copy must be on the disk.
        let length = match kind {
            FsKind::Btrfs => 4096,
            _ if ext => 1024,
            _ => 512,
        };
        if inside && backup + length <= disk.size() && recognized {
            attempts.push((Some(backup), VolumeState::FromBackup));
        }
    }
    for (boot_sector, state) in attempts {
        let rebuild = Rebuild {
            boot_sector,
            mft_records: records.clone(),
            fat: None,
        };
        let Ok(fs) = open_rebuilt(Arc::clone(disk), kind, offset, size, &rebuild) else {
            continue;
        };
        let mut evidence = partition.evidence.clone();
        if let Some(at) = boot_sector {
            let what = if ext || kind == FsKind::Btrfs {
                "backup superblock"
            } else {
                "backup boot sector"
            };
            evidence.push(format!(
                "opened with its {what} at byte {at} (its own is gone or overwritten)"
            ));
        }
        if !records.is_empty() {
            evidence.push(format!("{} MFT records found in it", records.len()));
        }
        // A volume filling a disk without a partition table counts as listed.
        let in_table = partition.in_table
            || (offset == 0
                && clues
                    .table
                    .and_then(|t| t.whole_disk.as_ref())
                    .is_some_and(|fs| fs.kind == kind));
        let score = match (state, in_table) {
            (VolumeState::Intact, true) => 100,
            (VolumeState::Intact, false) => 90,
            _ => 70,
        };
        return Ok(Some(FoundVolume {
            kind,
            name: kind.name().to_owned(),
            label: fs.info().label.clone(),
            offset,
            size,
            state,
            score,
            in_table,
            found: (kind == FsKind::Ntfs && !clues.records.is_empty())
                .then_some(records.len() as u64),
            evidence,
            rebuild,
        }));
    }
    Ok(None)
}

/// The MFT records found between `offset` and `offset + size`, by number.
/// When a number turns up more than once (an older MFT, a copy inside a
/// file), the copy that sits where most records put the MFT wins.
fn records_in(records: &[RecordClue], offset: u64, size: u64) -> BTreeMap<u64, u64> {
    let end = offset.saturating_add(size);
    let inside: Vec<RecordClue> = records
        .iter()
        .copied()
        .filter(|r| r.0 >= offset && r.0 < end)
        .collect();
    // Where the MFT would start if each record sat in it, contiguously.
    let start = |r: &RecordClue| r.0.checked_sub(r.1.saturating_mul(u64::from(r.2)));
    let mut votes: HashMap<u64, usize> = HashMap::new();
    for record in &inside {
        if let Some(start) = start(record) {
            *votes.entry(start).or_default() += 1;
        }
    }
    let weight = |r: &RecordClue| start(r).and_then(|s| votes.get(&s)).copied().unwrap_or(0);
    let mut best: BTreeMap<u64, RecordClue> = BTreeMap::new();
    for record in inside {
        match best.get(&record.1) {
            Some(current) if weight(current) >= weight(&record) => {}
            _ => {
                best.insert(record.1, record);
            }
        }
    }
    best.into_iter().map(|(number, r)| (number, r.0)).collect()
}

/// FAT volumes put together from folder sectors where no boot sector was found.
fn rebuilt_fats(
    disk: &Arc<Disk>,
    clues: &Clues,
    existing: &[FoundVolume],
) -> Result<Vec<FoundVolume>, ScanError> {
    let dots: Vec<(u64, u32, u32)> = clues
        .folders
        .iter()
        .filter_map(|f| f.own().map(|own| (f.offset(), own, f.parent())))
        .collect();
    let roots = root_votes(disk, clues, &dots)?;
    let mut volumes = Vec::new();
    for geometry in fat_geometries(&dots, &roots) {
        let (cluster_size, data_start) = (geometry.cluster_size, geometry.data_start);
        // A FAT volume that opened normally already covers these folders.
        let covered = existing.iter().any(|v| {
            is_fat(v.kind) && v.offset <= data_start && data_start < v.offset.saturating_add(v.size)
        });
        if covered {
            continue;
        }
        let (offset, end) = enclosing(disk, clues, data_start);
        let clusters = u32::try_from((end - data_start) / cluster_size).unwrap_or(u32::MAX);
        let big_numbers = dots
            .iter()
            .any(|&(_, own, parent)| own > 0xFFFF || parent > 0xFFFF);
        let kind = match clusters {
            _ if big_numbers => FsKind::Fat32,
            0..4085 => FsKind::Fat12,
            4085..65525 => FsKind::Fat16,
            _ => FsKind::Fat32,
        };
        let root = if kind == FsKind::Fat32 {
            FatRoot::Cluster { cluster: 2 }
        } else {
            fixed_root(disk, offset, data_start)?
        };
        let folders: Vec<u32> = clues
            .folders
            .iter()
            .filter_map(|f| {
                let at = f.offset();
                let within = at.checked_sub(data_start).filter(|_| at < end)?;
                (within % cluster_size == 0)
                    .then(|| u32::try_from(within / cluster_size + 2).ok())
                    .flatten()
            })
            .collect();
        let fat_start = clues
            .tables
            .iter()
            .filter(|t| t.0 >= offset && t.0 < data_start && t.1 == kind)
            .map(|t| t.0 - offset)
            .min();
        let layout = FatLayout {
            kind,
            cluster_size,
            data_start: data_start - offset,
            clusters,
            root,
            fat_start,
            folders: folders.clone(),
        };
        let rebuild = Rebuild {
            fat: Some(layout),
            ..Rebuild::default()
        };
        let size = end - offset;
        let Ok(fs) = open_rebuilt(Arc::clone(disk), kind, offset, size, &rebuild) else {
            continue;
        };
        let mut evidence = vec![
            format!(
                "{} folders that agree on {cluster_size}-byte clusters starting at byte {data_start}",
                geometry.support
            ),
            "no boot sector was found for it".to_owned(),
        ];
        evidence.push(match fat_start {
            Some(at) => format!("its FAT is still there, at byte {}", offset + at),
            None => "its FAT is gone: each file is taken to be in one piece".to_owned(),
        });
        volumes.push(FoundVolume {
            kind,
            name: kind.name().to_owned(),
            label: fs.info().label.clone(),
            offset,
            size,
            state: VolumeState::Rebuilt,
            score: 40 + u32::try_from(geometry.support.min(20)).unwrap_or(20),
            in_table: false,
            found: Some(folders.len() as u64),
            evidence,
            rebuild,
        });
    }
    Ok(volumes)
}

struct Geometry {
    cluster_size: u64,
    /// Where cluster 2 is on the disk.
    data_start: u64,
    /// Folders that agree with it.
    support: usize,
}

/// Cluster sizes and data areas from root folders. A volume's first folder
/// sector without "." is usually its root, in cluster 2: when it lists a
/// folder whose first sector was found, the distance between the two gives
/// the cluster size. This works with a single folder besides the root.
fn root_votes(
    disk: &Disk,
    clues: &Clues,
    dots: &[(u64, u32, u32)],
) -> Result<Vec<(u64, u64)>, ScanError> {
    let mut by_cluster: HashMap<u32, Vec<u64>> = HashMap::new();
    for &(offset, own, _) in dots {
        by_cluster.entry(own).or_default().push(offset);
    }
    let mut votes = Vec::new();
    for root in clues.folders.iter().filter(|f| f.own().is_none()).take(256) {
        let at = root.offset();
        let mut sector = vec![0; 512];
        disk.read_at(at, &mut sector)?;
        let Some(folder) = clues::folder_sector(&sector) else {
            continue;
        };
        for cluster in folder.subfolders.into_iter().filter(|&c| c > 2) {
            for &x in by_cluster.get(&cluster).into_iter().flatten() {
                let steps = u64::from(cluster - 2);
                if let Some(distance) = x.checked_sub(at).filter(|d| d % steps == 0) {
                    let cluster_size = distance / steps;
                    if cluster_size.is_power_of_two() && FAT_CLUSTERS.contains(&cluster_size) {
                        votes.push((cluster_size, at));
                    }
                }
            }
        }
    }
    Ok(votes)
}

/// Cluster sizes and data areas that the folders found agree on. Two
/// folders' first sectors and their own cluster numbers (from ".") give
/// the cluster size, as does a root folder listing one (`roots`); each
/// group of folders that agrees is one volume.
fn fat_geometries(dots: &[(u64, u32, u32)], roots: &[(u64, u64)]) -> Vec<Geometry> {
    let mut votes: HashMap<(u64, u64), usize> = HashMap::new();
    for &root in roots {
        *votes.entry(root).or_default() += 1;
    }
    let from_root: HashSet<(u64, u64)> = roots.iter().copied().collect();
    for (i, &(x1, n1, _)) in dots.iter().enumerate() {
        for &(x2, n2, _) in dots.iter().skip(i + 1).take(8) {
            if n1 < 2 || n2 <= n1 || x2 <= x1 {
                continue;
            }
            let (dx, dn) = (x2 - x1, u64::from(n2 - n1));
            if dx % dn != 0 {
                continue;
            }
            let cluster_size = dx / dn;
            if !cluster_size.is_power_of_two() || !FAT_CLUSTERS.contains(&cluster_size) {
                continue;
            }
            if let Some(data_start) = x1.checked_sub(u64::from(n1 - 2) * cluster_size) {
                *votes.entry((cluster_size, data_start)).or_default() += 1;
            }
        }
    }
    let mut candidates: Vec<((u64, u64), usize)> = votes.into_iter().collect();
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut used = HashSet::new();
    let mut geometries = Vec::new();
    for ((cluster_size, data_start), _) in candidates {
        let members: Vec<usize> = dots
            .iter()
            .enumerate()
            .filter(|&(i, &(x, n, _))| {
                !used.contains(&i)
                    && x >= data_start
                    && (x - data_start) % cluster_size == 0
                    && (x - data_start) / cluster_size + 2 == u64::from(n)
            })
            .map(|(i, _)| i)
            .collect();
        let enough = members.len() >= 2
            || (members.len() == 1 && from_root.contains(&(cluster_size, data_start)));
        if enough {
            geometries.push(Geometry {
                cluster_size,
                data_start,
                support: members.len(),
            });
            used.extend(members);
        }
    }
    geometries
}

/// The partition holding byte `at` (from the table, or found by the scan);
/// the whole disk if none does.
fn enclosing(disk: &Disk, clues: &Clues, at: u64) -> (u64, u64) {
    let sector = u64::from(disk.sector_size());
    let from_table = clues.table.into_iter().flat_map(|t| {
        t.partitions
            .iter()
            .filter(|p| !matches!(p.kind, datareco_partitions::PartitionKind::Extended))
            .map(|p| (p.start_lba * sector, (p.start_lba + p.sectors) * sector))
    });
    let from_scan = clues
        .partitions
        .iter()
        .map(|p| (p.start_lba * sector, (p.start_lba + p.sectors) * sector));
    from_table
        .chain(from_scan)
        .filter(|&(start, end)| start <= at && at < end)
        .max_by_key(|&(start, _)| start)
        .map_or((0, disk.size()), |(start, end)| {
            (start, end.min(disk.size()))
        })
}

/// Where a FAT12/16 volume's fixed root folder is: right before the
/// clusters, in one of the usual sizes.
fn fixed_root(disk: &Disk, offset: u64, data_start: u64) -> Result<FatRoot, ScanError> {
    for entries in ROOT_ENTRIES {
        let bytes = entries * 32;
        let Some(at) = data_start.checked_sub(bytes).filter(|&at| at >= offset) else {
            continue;
        };
        let mut sector = vec![0; 512];
        disk.read_at(at, &mut sector)?;
        if clues::folder_sector(&sector).is_some() {
            return Ok(FatRoot::Fixed {
                offset: at - offset,
                bytes,
            });
        }
    }
    let bytes = (data_start - offset).min(16 * 1024);
    Ok(FatRoot::Fixed {
        offset: data_start - offset - bytes,
        bytes,
    })
}
