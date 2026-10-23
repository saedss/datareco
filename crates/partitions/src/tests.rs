//! Whole-disk scenarios built in memory, mirroring the generated test images.

use datareco_io::{Disk, MemorySource};

use crate::boot::samples as fs;
use crate::gpt::samples as gpt;
use crate::mbr::samples::table;
use crate::{Confidence, FsKind, PartitionKind, Scan, ScanMode, Severity, TableKind, read_table};

const MIB: u64 = 2048; // sectors

struct Image(Vec<u8>);

impl Image {
    fn new(sectors: u64) -> Self {
        Image(vec![0; usize::try_from(sectors * 512).unwrap()])
    }

    fn put(&mut self, lba: u64, bytes: &[u8]) -> &mut Self {
        let at = usize::try_from(lba * 512).unwrap();
        self.0[at..at + bytes.len()].copy_from_slice(bytes);
        self
    }

    fn wipe(&mut self, lba: u64) -> &mut Self {
        self.put(lba, &[0; 512])
    }

    fn disk(&self) -> Disk {
        Disk::new("test", Box::new(MemorySource::new(self.0.clone(), 512)))
    }
}

// The MBR disk: FAT32 primary, then NTFS and ext4 as logical partitions.
const FAT_START: u64 = MIB;
const FAT_SECTORS: u64 = 33 * MIB;
const EXT_START: u64 = 34 * MIB;
const EBR2: u64 = 67 * MIB;
const NTFS_START: u64 = 35 * MIB;
const NTFS_SECTORS: u64 = 32 * MIB;
const LINUX_START: u64 = 68 * MIB;
const LINUX_SECTORS: u64 = 16 * MIB;
const MBR_DISK: u64 = 96 * MIB;

fn mbr_disk() -> Image {
    let mut image = Image::new(MBR_DISK);
    let ext_sectors = MBR_DISK - EXT_START;
    image
        .put(
            0,
            &table(&[
                (0x80, 0x0C, sector(FAT_START), sector(FAT_SECTORS)),
                (0, 0x0F, sector(EXT_START), sector(ext_sectors)),
            ]),
        )
        // FAT32: boot sector, its backup at +6, and the first FAT after 32 reserved sectors.
        .put(FAT_START, &fs::fat32(sector(FAT_SECTORS), b"DR_P1_FAT32"))
        .put(
            FAT_START + 6,
            &fs::fat32(sector(FAT_SECTORS), b"DR_P1_FAT32"),
        )
        .put(FAT_START + 32, &fs::fat32_table())
        // First EBR: the NTFS partition, then a link to the second EBR.
        .put(
            EXT_START,
            &table(&[
                (
                    0,
                    0x07,
                    sector(NTFS_START - EXT_START),
                    sector(NTFS_SECTORS),
                ),
                (
                    0,
                    0x05,
                    sector(EBR2 - EXT_START),
                    sector(MIB + LINUX_SECTORS),
                ),
            ]),
        )
        .put(NTFS_START, &fs::ntfs(NTFS_SECTORS - 1))
        .put(NTFS_START + 32, b"FILE")
        .put(NTFS_START + NTFS_SECTORS - 1, &fs::ntfs(NTFS_SECTORS - 1))
        // Second EBR: the ext4 partition.
        .put(
            EBR2,
            &table(&[(0, 0x83, sector(LINUX_START - EBR2), sector(LINUX_SECTORS))]),
        )
        .put(
            LINUX_START + 2,
            &fs::ext4(sector(LINUX_SECTORS / 8), 0, b"dr_p6_ext4"),
        );
    image
}

fn sector(n: u64) -> u32 {
    u32::try_from(n).unwrap()
}

#[test]
fn reads_an_mbr_with_logical_partitions() {
    let table = read_table(&mbr_disk().disk()).unwrap();
    assert_eq!(table.kind, TableKind::Mbr);
    assert_eq!(table.disk_id.as_deref(), Some("0DA7A8EC"));
    assert!(table.problems.is_empty(), "{:#?}", table.problems);
    let summary: Vec<_> = table
        .partitions
        .iter()
        .map(|p| {
            (
                p.number,
                p.kind,
                p.start_lba,
                p.sectors,
                p.file_system.as_ref().map(|f| f.kind),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                1,
                PartitionKind::Primary,
                FAT_START,
                FAT_SECTORS,
                Some(FsKind::Fat32)
            ),
            (
                2,
                PartitionKind::Extended,
                EXT_START,
                MBR_DISK - EXT_START,
                None
            ),
            (
                5,
                PartitionKind::Logical,
                NTFS_START,
                NTFS_SECTORS,
                Some(FsKind::Ntfs)
            ),
            (
                6,
                PartitionKind::Logical,
                LINUX_START,
                LINUX_SECTORS,
                Some(FsKind::Ext4)
            ),
        ]
    );
    assert_eq!(table.partitions[0].type_name, "FAT32 (LBA)");
    assert!(table.partitions[0].bootable);
    assert_eq!(
        table.partitions[0]
            .file_system
            .as_ref()
            .unwrap()
            .label
            .as_deref(),
        Some("DR_P1_FAT32")
    );
}

fn assert_found_the_three_volumes(found: &[crate::FoundPartition]) {
    let summary: Vec<_> = found
        .iter()
        .filter(|f| f.confidence >= Confidence::Medium)
        .map(|f| {
            (
                f.start_lba,
                f.sectors,
                f.file_system.as_ref().map(|fs| fs.kind),
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (FAT_START, FAT_SECTORS, Some(FsKind::Fat32)),
            (NTFS_START, NTFS_SECTORS, Some(FsKind::Ntfs)),
            (LINUX_START, LINUX_SECTORS, Some(FsKind::Ext4)),
        ],
        "{found:#?}"
    );
}

#[test]
fn finds_partitions_after_the_mbr_is_wiped() {
    let mut image = mbr_disk();
    image.wipe(0);
    let disk = image.disk();
    let table = read_table(&disk).unwrap();
    assert_eq!(table.kind, TableKind::None);
    assert!(table.partitions.is_empty());

    for mode in [ScanMode::Quick, ScanMode::Full] {
        let found = Scan::run(&disk, mode, Some(&table)).unwrap();
        assert_found_the_three_volumes(&found);
        assert!(found.iter().all(|f| !f.in_table));
        // The logical partitions are backed by their EBRs as well as their boot sectors.
        let ntfs = found.iter().find(|f| f.start_lba == NTFS_START).unwrap();
        assert_eq!(ntfs.confidence, Confidence::High, "{ntfs:#?}");
        assert!(
            ntfs.evidence
                .iter()
                .any(|e| e.contains("extended boot record")),
            "{ntfs:#?}"
        );
        assert_eq!(ntfs.type_name.as_deref(), Some("NTFS, exFAT or HPFS"));
    }

    // Only a full search reaches the NTFS backup boot sector at the volume's end.
    let full = Scan::run(&disk, ScanMode::Full, None).unwrap();
    let ntfs = full.iter().find(|f| f.start_lba == NTFS_START).unwrap();
    assert!(
        ntfs.evidence
            .iter()
            .any(|e| e.contains("backup boot sector")),
        "{ntfs:#?}"
    );
    let fat = full.iter().find(|f| f.start_lba == FAT_START).unwrap();
    assert!(
        fat.evidence
            .iter()
            .any(|e| e.contains("FAT32 backup boot sector")),
        "{fat:#?}"
    );
}

#[test]
fn finds_a_volume_whose_boot_sector_is_gone_from_its_backup() {
    let mut image = mbr_disk();
    image.wipe(0).wipe(EXT_START).wipe(NTFS_START);
    let found = Scan::run(&image.disk(), ScanMode::Full, None).unwrap();
    let ntfs = found
        .iter()
        .find(|f| f.start_lba == NTFS_START)
        .expect("found from the backup");
    assert_eq!(ntfs.sectors, NTFS_SECTORS);
    assert_eq!(ntfs.file_system.as_ref().unwrap().kind, FsKind::Ntfs);
    assert_eq!(ntfs.confidence, Confidence::Medium);
}

#[test]
fn a_reformatted_volume_shows_both_file_systems() {
    let sectors = 64 * MIB;
    // NTFS with its MFT mirror in the middle of the volume, as mkntfs does.
    let mut ntfs = fs::ntfs(sectors - 1);
    ntfs[56..64].copy_from_slice(&4096u64.to_le_bytes());
    let mut image = Image::new(sectors);
    image
        .put(0, &ntfs)
        .put(32, b"FILE")
        .put(4096 * 8, b"FILE")
        .put(sectors - 1, &ntfs);
    // Quick-formatted as FAT32: a new boot sector and FAT over the start.
    image
        .put(0, &fs::fat32(sector(sectors), b"NEW_VOLUME "))
        .put(32, &fs::fat32_table());

    let found = Scan::run(&image.disk(), ScanMode::Full, None).unwrap();
    let at_zero: Vec<FsKind> = found
        .iter()
        .filter(|f| f.start_lba == 0 && f.confidence >= Confidence::Medium)
        .map(|f| f.file_system.as_ref().unwrap().kind)
        .collect();
    assert!(at_zero.contains(&FsKind::Fat32), "{found:#?}");
    assert!(
        at_zero.contains(&FsKind::Ntfs),
        "the old NTFS, from its backup boot sector: {found:#?}"
    );
}

#[test]
fn table_partitions_are_marked_as_already_known() {
    let disk = mbr_disk().disk();
    let table = read_table(&disk).unwrap();
    let found = Scan::run(&disk, ScanMode::Quick, Some(&table)).unwrap();
    assert!(
        found
            .iter()
            .filter(|f| f.confidence >= Confidence::Medium)
            .all(|f| f.in_table),
        "{found:#?}"
    );
}

// The GPT disk: exFAT and NTFS.
const GPT_DISK: u64 = 64 * MIB;
const EXFAT_START: u64 = MIB;
const EXFAT_SECTORS: u64 = 16 * MIB;
const GPT_NTFS_START: u64 = 17 * MIB;
const GPT_NTFS_SECTORS: u64 = 32 * MIB;

fn gpt_disk() -> Image {
    let last = GPT_DISK - 1;
    let entries = gpt::entries(&[
        (
            EXFAT_START,
            EXFAT_START + EXFAT_SECTORS - 1,
            "DataReco exFAT",
        ),
        (
            GPT_NTFS_START,
            GPT_NTFS_START + GPT_NTFS_SECTORS - 1,
            "DataReco NTFS",
        ),
    ]);
    let mut image = Image::new(GPT_DISK);
    image
        .put(0, &table(&[(0, 0xEE, 1, sector(last))]))
        .put(1, &gpt::header(1, GPT_DISK, 2, &entries))
        .put(2, &entries)
        .put(last - 32, &entries)
        .put(last, &gpt::header(last, GPT_DISK, last - 32, &entries))
        .put(EXFAT_START, &fs::exfat(EXFAT_SECTORS))
        .put(EXFAT_START + 12, &fs::exfat(EXFAT_SECTORS))
        .put(EXFAT_START + 128, &fs::exfat_table())
        .put(GPT_NTFS_START, &fs::ntfs(GPT_NTFS_SECTORS - 1))
        .put(GPT_NTFS_START + 32, b"FILE");
    image
}

#[test]
fn reads_a_gpt_disk() {
    let table = read_table(&gpt_disk().disk()).unwrap();
    assert_eq!(table.kind, TableKind::Gpt);
    assert!(table.problems.is_empty(), "{:#?}", table.problems);
    assert_eq!(
        table.disk_id.as_deref(),
        Some("11111111-1111-1111-1111-111111111111")
    );
    let names: Vec<_> = table
        .partitions
        .iter()
        .map(|p| p.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["DataReco exFAT", "DataReco NTFS"]);
    assert_eq!(table.partitions[0].type_name, "Microsoft basic data");
    assert_eq!(
        table.partitions[0].file_system.as_ref().unwrap().kind,
        FsKind::Exfat
    );
    assert_eq!(table.partitions[1].sectors, GPT_NTFS_SECTORS);
}

#[test]
fn falls_back_to_the_backup_gpt_header() {
    let mut image = gpt_disk();
    image.wipe(1);
    let disk = image.disk();
    let table = read_table(&disk).unwrap();
    assert_eq!(table.kind, TableKind::Gpt);
    assert_eq!(table.partitions.len(), 2);
    assert_eq!(table.problems.len(), 1, "{:#?}", table.problems);
    assert_eq!(table.problems[0].severity, Severity::Error);
    assert!(table.problems[0].message.contains("primary GPT header"));

    let found = Scan::run(&disk, ScanMode::Quick, Some(&table)).unwrap();
    let exfat = found.iter().find(|f| f.start_lba == EXFAT_START).unwrap();
    assert_eq!(exfat.confidence, Confidence::High);
    assert_eq!(exfat.name.as_deref(), Some("DataReco exFAT"));
    assert!(
        exfat
            .evidence
            .iter()
            .any(|e| e.contains("GPT backup header")),
        "{exfat:#?}"
    );
    assert!(exfat.in_table);
}

#[test]
fn a_damaged_backup_gpt_header_is_a_warning() {
    let mut image = gpt_disk();
    image.wipe(GPT_DISK - 1);
    let table = read_table(&image.disk()).unwrap();
    assert_eq!(table.kind, TableKind::Gpt);
    assert_eq!(table.problems.len(), 1);
    assert_eq!(table.problems[0].severity, Severity::Warning);
}

#[test]
fn protective_mbr_without_any_gpt_header_is_an_error() {
    let mut image = gpt_disk();
    image.wipe(1).wipe(GPT_DISK - 1);
    let table = read_table(&image.disk()).unwrap();
    assert!(
        table
            .problems
            .iter()
            .any(|p| p.severity == Severity::Error && p.message.contains("GPT disk"))
    );
}

#[test]
fn flags_overlaps_and_partitions_past_the_end() {
    let mut image = Image::new(8 * MIB);
    image.put(
        0,
        &table(&[
            (0x80, 0x07, 2048, 4096),
            (0x80, 0x07, 4000, 4096),
            (0, 0x83, 10_000, 100_000),
        ]),
    );
    let table = read_table(&image.disk()).unwrap();
    let messages: Vec<&str> = table.problems.iter().map(|p| p.message.as_str()).collect();
    assert!(
        messages
            .iter()
            .any(|m| m.contains("Partitions 1 and 2 overlap")),
        "{messages:#?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("Partition 3 ends at sector") && m.contains("past the end")),
        "{messages:#?}"
    );
    assert!(
        messages
            .iter()
            .any(|m| m.contains("More than one partition is marked active")),
        "{messages:#?}"
    );
}

#[test]
fn a_looping_ebr_chain_stops_with_an_error() {
    let mut image = Image::new(8 * MIB);
    image
        .put(0, &table(&[(0, 0x0F, 2048, 12_288)]))
        // The first EBR links to a second one 4096 sectors into the extended
        // partition, whose link points back at itself.
        .put(
            2048,
            &table(&[(0, 0x83, 2048, 1024), (0, 0x05, 4096, 4096)]),
        )
        .put(
            6144,
            &table(&[(0, 0x83, 2048, 1024), (0, 0x05, 4096, 4096)]),
        );
    let table = read_table(&image.disk()).unwrap();
    assert!(
        table
            .problems
            .iter()
            .any(|p| p.message.contains("loops back")),
        "{:#?}",
        table.problems
    );
    assert_eq!(
        table
            .partitions
            .iter()
            .filter(|p| p.kind == PartitionKind::Logical)
            .count(),
        2
    );
}

#[test]
fn a_whole_disk_volume_has_no_partition_table() {
    let mut image = Image::new(40 * MIB);
    image
        .put(0, &fs::fat32(sector(40 * MIB), b"STICK      "))
        .put(32, &fs::fat32_table());
    let table = read_table(&image.disk()).unwrap();
    assert_eq!(table.kind, TableKind::None);
    assert_eq!(table.whole_disk.as_ref().unwrap().kind, FsKind::Fat32);
    assert_eq!(table.problems[0].severity, Severity::Info);
}

#[test]
fn random_bytes_never_panic() {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let data: Vec<u8> = (0..4 * MIB * 512)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect();
    let disk = Disk::new("random", Box::new(MemorySource::new(data, 512)));
    read_table(&disk).unwrap();
    Scan::run(&disk, ScanMode::Full, None).unwrap();
}

#[test]
fn a_fed_search_can_be_saved_and_restored() {
    let mut image = mbr_disk();
    image.wipe(0);
    let disk = image.disk();
    let summary = |found: &[crate::FoundPartition]| -> Vec<(u64, u64, String)> {
        found
            .iter()
            .map(|f| {
                (
                    f.start_lba,
                    f.sectors,
                    format!("{:?} {:?}", f.confidence, f.evidence),
                )
            })
            .collect()
    };
    let expected = summary(&Scan::run(&disk, ScanMode::Full, None).unwrap());

    // Fed in uneven chunks, saved halfway, restored and finished.
    let chunk = 3000 * 512;
    let mut scan = Scan::new(&disk, ScanMode::Full);
    let mut offset = 0u64;
    let mut saved = None;
    while offset < disk.size() {
        let len = chunk.min(disk.size() - offset);
        let mut data = vec![0; usize::try_from(len).unwrap()];
        disk.read_at(offset, &mut data).unwrap();
        scan.feed(offset / 512, &data);
        offset += len;
        if saved.is_none() && offset > disk.size() / 2 {
            saved = Some(scan.state());
            let restored = Scan::restore(&disk, saved.as_ref().unwrap()).unwrap();
            assert_eq!(restored.state(), scan.state());
            scan = restored;
        }
    }
    assert!(scan.finish(&disk).unwrap().done);
    assert_eq!(summary(&scan.results(&disk, None).unwrap()), expected);
}
