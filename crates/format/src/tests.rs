//! Formats written to disks in memory, then opened with DataReco's own
//! readers and checked field by field against the specifications.

use std::sync::Arc;
use std::time::Duration;

use datareco_io::{Disk, MemorySource, MemoryTarget};

use super::*;

const MIB: u64 = 1 << 20;

fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
}

/// Formats a volume of `size` bytes, `offset` bytes into a disk of junk,
/// and returns the disk's bytes.
fn formatted(
    size: u64,
    offset: u64,
    sector: u32,
    options: &FormatOptions,
) -> (Formatting, Vec<u8>) {
    let total = usize::try_from(offset + size).unwrap();
    let mut target = MemoryTarget::with_data(vec![0xE5; total], sector);
    let plan = plan_format(size, sector, options).unwrap();
    write_format(&plan, &mut target, offset).unwrap();
    (plan, target.data().to_vec())
}

fn options(kind: FormatKind, label: Option<&str>) -> FormatOptions {
    FormatOptions {
        kind,
        label: label.map(str::to_owned),
        cluster_size: None,
        serial: Some(0x1234_ABCD),
        partition_offset: 2048,
    }
}

fn open(data: Vec<u8>, sector: u32, offset: u64) -> Box<dyn datareco_fs::FileSystem> {
    let disk = Arc::new(Disk::new("test", Box::new(MemorySource::new(data, sector))));
    datareco_fs::open(disk, offset).unwrap()
}

#[test]
fn fat32_volumes_follow_the_specification() {
    let size = 256 * MIB;
    let offset = MIB;
    let (plan, data) = formatted(
        size,
        offset,
        512,
        &options(FormatKind::Fat32, Some("Photos 2026")),
    );
    let vol = &data[usize::try_from(offset).unwrap()..];
    let boot = &vol[..512];
    assert_eq!(&boot[0..3], [0xEB, 0x58, 0x90]);
    assert_eq!(u16_at(boot, 11), 512);
    let per_cluster = u64::from(boot[13]);
    assert_eq!(per_cluster * 512, u64::from(plan.summary.cluster_size));
    assert_eq!(
        plan.summary.cluster_size, 2048,
        "the usual size for 256 MiB"
    );
    let reserved = u64::from(u16_at(boot, 14));
    assert!(reserved >= 32);
    assert_eq!(boot[16], 2);
    assert_eq!(u16_at(boot, 17), 0, "no fixed root folder");
    assert_eq!(u16_at(boot, 19), 0);
    assert_eq!(u16_at(boot, 22), 0);
    assert_eq!(
        u32_at(boot, 28),
        2048,
        "hidden sectors: where the partition starts"
    );
    assert_eq!(u64::from(u32_at(boot, 32)), size / 512);
    let fat = u64::from(u32_at(boot, 36));
    assert_eq!(u32_at(boot, 44), 2, "root folder cluster");
    assert_eq!(u16_at(boot, 48), 1);
    assert_eq!(u16_at(boot, 50), 6);
    assert_eq!(boot[66], 0x29);
    assert_eq!(u32_at(boot, 67), 0x1234_ABCD);
    assert_eq!(&boot[71..82], b"PHOTOS 2026");
    assert_eq!(&boot[82..90], b"FAT32   ");
    assert_eq!(&boot[510..512], [0x55, 0xAA]);
    assert_eq!(&vol[6 * 512..7 * 512], boot, "the backup boot sector");

    // The data area starts on a 1 MiB boundary, and the FATs fit every cluster.
    let data_start = reserved + 2 * fat;
    assert_eq!((data_start * 512) % MIB, 0);
    let clusters = (size / 512 - data_start) / per_cluster;
    assert_eq!(clusters, plan.summary.clusters);
    assert!(clusters >= 65_525, "FAT32, not FAT16");
    assert!(fat * 512 / 4 >= clusters + 2);

    // FSInfo: free clusters (all but the root folder's), and its signatures.
    let info = &vol[512..1024];
    assert_eq!(u32_at(info, 0), 0x4161_5252);
    assert_eq!(u32_at(info, 484), 0x6141_7272);
    assert_eq!(u64::from(u32_at(info, 488)), clusters - 1);
    assert_eq!(u32_at(info, 508), 0xAA55_0000);

    // Both FATs: media, end of chain, root folder; the rest free.
    for copy in 0..2 {
        let at = usize::try_from((reserved + copy * fat) * 512).unwrap();
        assert_eq!(u32_at(vol, at), 0x0FFF_FFF8);
        assert_eq!(u32_at(vol, at + 4), 0x0FFF_FFFF);
        assert_eq!(u32_at(vol, at + 8), 0x0FFF_FFFF);
        let rest = &vol[at + 12..at + usize::try_from(fat * 512).unwrap()];
        assert!(rest.iter().all(|&b| b == 0), "the old FAT is gone");
    }
    // The old junk past the root folder is left (a quick format).
    let after_root = usize::try_from((data_start + per_cluster) * 512).unwrap();
    assert_eq!(vol[after_root], 0xE5);

    let fs = open(data, 512, offset);
    assert_eq!(fs.info().kind, datareco_partitions::FsKind::Fat32);
    assert_eq!(fs.info().label.as_deref(), Some("PHOTOS 2026"));
    let root = fs.list(&fs.root()).unwrap();
    assert!(
        root.iter().all(|e| e.deleted),
        "an empty root folder: {root:#?}"
    );
}

#[test]
fn fat32_over_32_gib_and_on_4k_sectors() {
    // Only the plan: a 64 GiB volume in memory is too much.
    let plan = plan_format(64 << 30, 512, &options(FormatKind::Fat32, None)).unwrap();
    assert_eq!(plan.summary.cluster_size, 32 * 1024);
    // 2,097,152 clusters of 32 KiB, less the 34,816 sectors (17 MiB) the
    // reserved area and two 8 MiB FATs take, rounded up to 1 MiB.
    assert_eq!(plan.summary.clusters, 2_097_152 - 34_816 / 64);
    assert!(
        plan.bytes_written() < 20 << 20,
        "writes only the FATs and the start"
    );

    let (plan, data) = formatted(512 * MIB, 0, 4096, &options(FormatKind::Fat32, None));
    assert_eq!(u16_at(&data, 11), 4096);
    assert_eq!(&data[71..82], b"NO NAME    ");
    let fs = open(data, 4096, 0);
    assert_eq!(fs.info().cluster_size, u64::from(plan.summary.cluster_size));
}

#[test]
fn fat32_refuses_what_it_cant_hold() {
    assert!(
        plan_format(16 * MIB, 512, &options(FormatKind::Fat32, None))
            .unwrap_err()
            .contains("too small")
    );
    assert!(
        plan_format(3 << 40, 512, &options(FormatKind::Fat32, None))
            .unwrap_err()
            .contains("2 TiB")
    );
    let mut o = options(FormatKind::Fat32, Some("TOO LONG A LABEL"));
    assert!(plan_format(256 * MIB, 512, &o).unwrap_err().contains("11"));
    o.label = Some("a:b".into());
    assert!(plan_format(256 * MIB, 512, &o).unwrap_err().contains(":"));
    o.label = None;
    o.cluster_size = Some(3000);
    assert!(
        plan_format(256 * MIB, 512, &o)
            .unwrap_err()
            .contains("powers of two")
    );
}

#[test]
fn exfat_volumes_follow_the_specification() {
    let size = 512 * MIB;
    let offset = MIB;
    let (plan, data) = formatted(
        size,
        offset,
        512,
        &options(FormatKind::Exfat, Some("Fotos é")),
    );
    let vol = &data[usize::try_from(offset).unwrap()..];
    let boot = &vol[..512];
    assert_eq!(&boot[0..3], [0xEB, 0x76, 0x90]);
    assert_eq!(&boot[3..11], b"EXFAT   ");
    assert!(boot[11..64].iter().all(|&b| b == 0), "MustBeZero");
    assert_eq!(u64_at(boot, 64), 2048);
    assert_eq!(u64_at(boot, 72), size / 512);
    let fat_offset = u64::from(u32_at(boot, 80));
    let fat_length = u64::from(u32_at(boot, 84));
    let heap = u64::from(u32_at(boot, 88));
    let clusters = u64::from(u32_at(boot, 92));
    assert!(fat_offset >= 24);
    assert!(heap >= fat_offset + fat_length);
    assert_eq!((heap * 512) % MIB, 0, "the heap is aligned");
    assert!(fat_length * 512 / 4 >= clusters + 2);
    assert_eq!(clusters, plan.summary.clusters);
    assert_eq!(u16_at(boot, 104), 0x0100);
    assert_eq!(boot[108], 9);
    assert_eq!(
        1u64 << boot[109],
        u64::from(plan.summary.cluster_size) / 512
    );
    assert_eq!(plan.summary.cluster_size, 32 * 1024);
    assert_eq!(boot[110], 1);
    assert_eq!(&boot[510..512], [0x55, 0xAA]);

    // Extended boot sectors' signatures, and the checksum sector.
    for n in 1..=8 {
        assert_eq!(u32_at(vol, n * 512 + 508), 0xAA55_0000);
    }
    let sum = vol[..11 * 512]
        .iter()
        .enumerate()
        .filter(|(i, _)| !matches!(i, 106 | 107 | 112))
        .fold(0u32, |s, (_, &b)| rotate_add(s, b));
    for word in vol[11 * 512..12 * 512].chunks(4) {
        assert_eq!(u32::from_le_bytes(word.try_into().unwrap()), sum);
    }
    assert_eq!(
        &vol[12 * 512..24 * 512],
        &vol[..12 * 512],
        "the backup boot region"
    );

    // The root folder: label, bitmap and up-case table entries, all valid.
    let cluster_bytes = u64::from(plan.summary.cluster_size);
    let at = |cluster: u64| usize::try_from(heap * 512 + (cluster - 2) * cluster_bytes).unwrap();
    let root_cluster = u64::from(u32_at(boot, 96));
    let root = &vol[at(root_cluster)..at(root_cluster) + 96];
    assert_eq!(root[0], 0x83);
    assert_eq!(root[1], 7);
    assert_eq!(root[32], 0x81);
    let bitmap_first = u64::from(u32_at(root, 52));
    assert_eq!(bitmap_first, 2);
    assert_eq!(u64_at(root, 56), clusters.div_ceil(8));
    assert_eq!(root[64], 0x82);
    let upcase_first = u64::from(u32_at(root, 84));
    let upcase_len = usize::try_from(u64_at(root, 88)).unwrap();
    let table = &vol[at(upcase_first)..at(upcase_first) + upcase_len];
    assert_eq!(u32_at(root, 68), upcase::checksum(table));
    let full = upcase::expand(table);
    assert_eq!(full[usize::from(b'z')], u16::from(b'Z'));
    // The bitmap marks exactly the three structures' clusters as used.
    let used = root_cluster - 1;
    let bitmap = &vol[at(2)..at(2) + usize::try_from(clusters.div_ceil(8)).unwrap()];
    let set: u64 = bitmap.iter().map(|b| u64::from(b.count_ones())).sum();
    assert_eq!(set, used);
    // The FAT chains them, each ending with an end-of-chain mark.
    let fat = usize::try_from(fat_offset * 512).unwrap();
    assert_eq!(u32_at(vol, fat), 0xFFFF_FFF8);
    assert_eq!(
        u32_at(vol, fat + 4 * usize::try_from(root_cluster).unwrap()),
        0xFFFF_FFFF
    );
    assert_eq!(boot[112], u8::try_from(used * 100 / clusters).unwrap());

    let fs = open(data, 512, offset);
    assert_eq!(fs.info().kind, datareco_partitions::FsKind::Exfat);
    assert_eq!(fs.info().label.as_deref(), Some("Fotos é"));
    assert!(fs.list(&fs.root()).unwrap().iter().all(|e| e.deleted));
}

#[test]
fn exfat_on_small_volumes_and_4k_sectors() {
    let (plan, data) = formatted(8 * MIB, 0, 512, &options(FormatKind::Exfat, None));
    assert_eq!(plan.summary.cluster_size, 4096);
    let fs = open(data, 512, 0);
    assert_eq!(fs.info().label, None);

    let (_, data) = formatted(256 * MIB, 0, 4096, &options(FormatKind::Exfat, Some("Big")));
    assert_eq!(data[108], 12);
    let fs = open(data, 4096, 0);
    assert_eq!(fs.info().label.as_deref(), Some("Big"));
    assert!(plan_format(MIB / 2, 512, &options(FormatKind::Exfat, None)).is_err());
    let plan = plan_format(4 << 40, 512, &options(FormatKind::Exfat, None)).unwrap();
    assert_eq!(plan.summary.cluster_size, 128 * 1024, "4 TiB");
}

#[test]
fn wiping_writes_the_range_and_nothing_else() {
    let mut target = MemoryTarget::with_data(vec![0xAB; 16 << 20], 512);
    let mut wipe = Wipe::new(MIB, 9 * MIB, WipePattern::Zeros);
    let mut progress = wipe.step(&mut target, Duration::ZERO).unwrap();
    assert!(!progress.finished);
    while !progress.finished {
        progress = wipe.step(&mut target, Duration::from_secs(5)).unwrap();
    }
    assert_eq!(progress.done, 8 * MIB);
    let data = target.data();
    let (before, rest) = data.split_at(usize::try_from(MIB).unwrap());
    let (wiped, after) = rest.split_at(usize::try_from(8 * MIB).unwrap());
    assert!(before.iter().all(|&b| b == 0xAB));
    assert!(wiped.iter().all(|&b| b == 0));
    assert!(after.iter().all(|&b| b == 0xAB));

    let mut target = MemoryTarget::with_data(vec![0; 8 << 20], 512);
    let mut wipe = Wipe::new(0, 8 * MIB, WipePattern::Random);
    while !wipe
        .step(&mut target, Duration::from_secs(5))
        .unwrap()
        .finished
    {}
    let zeros = target.data().iter().filter(|&&b| b == 0).count();
    assert!(zeros < 64 * 1024, "random bytes, not zeros ({zeros})");
}
