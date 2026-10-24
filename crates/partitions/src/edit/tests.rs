//! Changing tables on disks built in memory, read back with the same
//! reader the rest of DataReco uses.

use std::sync::Arc;

use datareco_io::{Disk, MemorySource, MemoryTarget};

use super::*;
use crate::{PartitionKind, PartitionTable, SegmentKind, Severity, TableKind, layout, read_table};

const MIB: u64 = 2048;

struct Image {
    data: Vec<u8>,
    sector_size: u32,
}

impl Image {
    fn new(sectors: u64) -> Image {
        Image::with_sectors(sectors, 512)
    }

    fn with_sectors(sectors: u64, sector_size: u32) -> Image {
        Image {
            data: vec![0; usize::try_from(sectors * u64::from(sector_size)).unwrap()],
            sector_size,
        }
    }

    fn disk(&self) -> Disk {
        Disk::new(
            "Test disk",
            Box::new(MemorySource::new(self.data.clone(), self.sector_size)),
        )
    }

    fn plan(&self, ops: &[Operation]) -> Plan {
        plan(&self.disk(), ops).unwrap()
    }

    /// Plans `ops`, which must all work, and writes them.
    fn run(&mut self, ops: &[Operation]) -> Plan {
        let plan = self.plan(ops);
        assert_eq!(plan.blocked, None);
        assert_eq!(plan.error, None);
        self.write(&plan.writes);
        plan
    }

    fn write(&mut self, writes: &[SectorWrite]) {
        for w in writes {
            let at = usize::try_from(w.lba * u64::from(self.sector_size)).unwrap();
            self.data[at..at + w.data.len()].copy_from_slice(&w.data);
        }
    }

    /// The table, which must have no errors or warnings.
    fn table(&self) -> PartitionTable {
        let table = read_table(&self.disk()).unwrap();
        let serious: Vec<_> = table
            .problems
            .iter()
            .filter(|p| p.severity >= Severity::Warning)
            .collect();
        assert!(serious.is_empty(), "{serious:#?}");
        table
    }

    fn sector(&self, lba: u64) -> &[u8] {
        let at = usize::try_from(lba * u64::from(self.sector_size)).unwrap();
        &self.data[at..at + self.sector_size as usize]
    }
}

fn create(start: u64, sectors: u64, kind: &str) -> Operation {
    Operation::Create {
        start_lba: start,
        sectors,
        type_code: kind.into(),
        logical: false,
        name: None,
    }
}

fn logical(start: u64, sectors: u64, kind: &str) -> Operation {
    Operation::Create {
        start_lba: start,
        sectors,
        type_code: kind.into(),
        logical: true,
        name: None,
    }
}

/// (number, kind, start, sectors, type code) of each partition.
fn shape(table: &PartitionTable) -> Vec<(u32, PartitionKind, u64, u64, String)> {
    table
        .partitions
        .iter()
        .map(|p| {
            (
                p.number,
                p.kind,
                p.start_lba,
                p.sectors,
                p.type_code.clone(),
            )
        })
        .collect()
}

/// A 64 MiB MBR disk: NTFS primary (1–17 MiB), then an extended partition
/// with Linux (18–34 MiB) and FAT32 (35–43 MiB) logical partitions.
fn mbr_disk() -> Image {
    let mut image = Image::new(64 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Mbr },
        create(MIB, 16 * MIB, "ntfs"),
        logical(18 * MIB, 16 * MIB, "linux"),
        logical(35 * MIB, 8 * MIB, "fat32"),
    ]);
    image
}

use PartitionKind::{Extended as Ext, Gpt as G, Logical as L, Primary as P};

#[test]
fn builds_an_mbr_disk_with_logical_partitions() {
    let image = mbr_disk();
    let table = image.table();
    assert_eq!(table.kind, TableKind::Mbr);
    assert_eq!(
        shape(&table),
        [
            (1, P, MIB, 16 * MIB, "0x07".into()),
            (2, Ext, 17 * MIB, 47 * MIB, "0x0F".into()),
            (5, L, 18 * MIB, 16 * MIB, "0x83".into()),
            (6, L, 35 * MIB, 8 * MIB, "0x0C".into()),
        ]
    );
    // The second logical partition's boot record sits right after the first.
    assert_eq!(&image.sector(34 * MIB)[510..], [0x55, 0xAA]);
    let free: Vec<_> = layout(&table)
        .iter()
        .filter(|s| s.kind == SegmentKind::Free)
        .map(|s| (s.start_lba, s.end_lba(), s.logical))
        .collect();
    assert_eq!(free, [(43 * MIB, 64 * MIB, true)]);
}

#[test]
fn deleting_logical_partitions_keeps_the_chain_whole() {
    let mut image = mbr_disk();
    let plan = image.run(&[Operation::Delete {
        start_lba: 18 * MIB,
    }]);
    assert!(
        plan.steps[0]
            .description
            .starts_with("Delete partition 5 (Linux"),
        "{:?}",
        plan.steps
    );
    assert_eq!(
        shape(&image.table())[2..],
        [(5, L, 35 * MIB, 8 * MIB, "0x0C".into())],
        "the FAT32 one is number 5 now"
    );
    let plan = image.run(&[Operation::Delete {
        start_lba: 35 * MIB,
    }]);
    assert!(
        plan.steps[0]
            .description
            .contains("extended partition that held it")
    );
    assert_eq!(
        shape(&image.table()),
        [(1, P, MIB, 16 * MIB, "0x07".into())]
    );
}

#[test]
fn builds_a_gpt_disk() {
    let mut image = Image::new(64 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Gpt },
        Operation::Create {
            start_lba: MIB,
            sectors: 8 * MIB,
            type_code: "efi".into(),
            logical: false,
            name: Some("EFI system partition".into()),
        },
        create(9 * MIB, 32 * MIB, "ntfs"),
    ]);
    let table = image.table();
    assert_eq!(table.kind, TableKind::Gpt);
    assert_eq!(shape(&table)[0].1, G);
    let names: Vec<_> = table
        .partitions
        .iter()
        .map(|p| p.name.clone().unwrap())
        .collect();
    assert_eq!(names, ["EFI system partition", "Basic data partition"]);
    assert_eq!(table.partitions[1].type_name, "Microsoft basic data");
    assert_ne!(table.partitions[0].guid, table.partitions[1].guid);
    // Both headers are valid: wiping the primary one changes nothing.
    image.data[512..1024].fill(0);
    let table = read_table(&image.disk()).unwrap();
    assert_eq!(table.partitions.len(), 2);
}

#[test]
fn gpt_on_4096_byte_sectors() {
    let mut image = Image::with_sectors(16_384, 4096); // 64 MiB
    image.run(&[
        Operation::Initialize { style: Style::Gpt },
        create(256, 4096, "linux"),
    ]);
    let table = image.table();
    assert_eq!(
        table.usable.unwrap().first,
        6,
        "the 16 KiB array is 4 sectors"
    );
    assert_eq!(
        shape(&table),
        [(
            1,
            G,
            256,
            4096,
            "0FC63DAF-8483-4772-8E79-3D69D8477DE4".into()
        )]
    );
}

#[test]
fn converts_between_mbr_and_gpt_keeping_partitions() {
    let mut image = mbr_disk();
    let plan = image.run(&[Operation::Convert { style: Style::Gpt }]);
    assert!(plan.steps[0].warnings.iter().any(|w| w.contains("BIOS")));
    let table = image.table();
    assert_eq!(table.kind, TableKind::Gpt);
    let places: Vec<_> = table
        .partitions
        .iter()
        .map(|p| (p.start_lba, p.sectors))
        .collect();
    assert_eq!(
        places,
        [(MIB, 16 * MIB), (18 * MIB, 16 * MIB), (35 * MIB, 8 * MIB)]
    );
    assert_eq!(table.partitions[1].type_name, "Linux file system");

    image.run(&[Operation::Convert { style: Style::Mbr }]);
    let table = image.table();
    assert_eq!(table.kind, TableKind::Mbr);
    assert_eq!(
        shape(&table),
        [
            (1, P, MIB, 16 * MIB, "0x07".into()),
            (2, P, 18 * MIB, 16 * MIB, "0x83".into()),
            (3, P, 35 * MIB, 8 * MIB, "0x07".into()),
        ],
        "basic data with no file system found becomes 0x07"
    );
    assert!(
        image.sector(1).iter().all(|&b| b == 0),
        "the old GPT header is cleared"
    );
    assert!(image.sector(64 * MIB - 1).iter().all(|&b| b == 0));
}

#[test]
fn converting_to_gpt_needs_room_at_both_ends() {
    let mut image = Image::new(64 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Mbr },
        create(MIB, 63 * MIB, "ntfs"),
    ]);
    let plan = image.plan(&[Operation::Convert { style: Style::Gpt }]);
    assert!(plan.error.unwrap().message.contains("Shrink it by 1 MiB"));
}

#[test]
fn initializing_never_writes_over_data() {
    let mut image = Image::new(64 * MIB);
    image.data[5 * 512] = 1; // something in the GPT area
    let plan = image.plan(&[Operation::Initialize { style: Style::Gpt }]);
    assert!(
        plan.error
            .unwrap()
            .message
            .contains("Sectors 1–33 hold data")
    );
    image.run(&[Operation::Initialize { style: Style::Mbr }]);
    assert_eq!(
        image.table().kind,
        TableKind::Mbr,
        "an empty MBR reads as one"
    );

    let mut image = Image::new(64 * MIB);
    image.data[446] = 0x80;
    let plan = image.plan(&[Operation::Initialize { style: Style::Mbr }]);
    assert!(plan.error.unwrap().message.contains("Sector 0 hold data"));

    let image = mbr_disk();
    let plan = image.plan(&[Operation::Initialize { style: Style::Gpt }]);
    assert!(plan.error.unwrap().message.contains("already has"));
}

#[test]
fn mistakes_are_explained_and_stop_the_list() {
    let image = mbr_disk();
    let cases: Vec<(Operation, &str)> = vec![
        (create(10 * MIB, MIB, "ntfs"), "overlaps partition 1"),
        (
            create(50 * MIB, MIB, "ntfs"),
            "inside the extended partition",
        ),
        (
            logical(MIB, MIB, "ntfs"),
            "go inside the extended partition",
        ),
        (create(60 * MIB, 8 * MIB, "ntfs"), "past the end"),
        (
            Operation::SetName {
                start_lba: MIB,
                name: "x".into(),
            },
            "Only GPT",
        ),
        (
            Operation::Delete { start_lba: 3 },
            "no partition starting at sector 3",
        ),
        (
            Operation::SetActive {
                start_lba: 18 * MIB,
                active: true,
            },
            "Only primary",
        ),
        (
            Operation::SetHidden {
                start_lba: 18 * MIB,
                hidden: true,
            },
            "Only FAT and NTFS",
        ),
        (
            Operation::SetType {
                start_lba: MIB,
                type_code: "msr".into(),
            },
            "GPT",
        ),
        (Operation::Convert { style: Style::Mbr }, "already MBR"),
    ];
    for (op, expected) in cases {
        let plan = image.plan(&[
            Operation::SetActive {
                start_lba: MIB,
                active: true,
            },
            op.clone(),
        ]);
        let error = plan.error.unwrap_or_else(|| panic!("{op:?} should fail"));
        assert_eq!(error.step, 1);
        assert!(
            error.message.contains(expected),
            "{op:?}: {}",
            error.message
        );
        assert_eq!(plan.steps.len(), 1, "the steps before still count");
    }
    // A fifth primary partition.
    let mut image = Image::new(64 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Mbr },
        create(MIB, MIB, "ntfs"),
        create(3 * MIB, MIB, "ntfs"),
        create(5 * MIB, MIB, "ntfs"),
        create(7 * MIB, MIB, "ntfs"),
    ]);
    let plan = image.plan(&[create(9 * MIB, MIB, "ntfs")]);
    assert!(plan.error.unwrap().message.contains("at most four"));
    let plan = image.plan(&[logical(10 * MIB, MIB, "ntfs")]);
    assert!(
        plan.error
            .unwrap()
            .message
            .contains("no room for an extended")
    );
}

#[test]
fn flags_types_and_names() {
    let mut image = mbr_disk();
    image.run(&[
        Operation::SetActive {
            start_lba: MIB,
            active: true,
        },
        Operation::SetHidden {
            start_lba: 35 * MIB,
            hidden: true,
        },
        Operation::SetType {
            start_lba: 18 * MIB,
            type_code: "linux-swap".into(),
        },
    ]);
    let table = image.table();
    assert!(table.partitions[0].bootable);
    assert_eq!(table.partitions[2].type_code, "0x82");
    assert_eq!(table.partitions[3].type_code, "0x1C", "hidden FAT32");
    image.run(&[Operation::SetHidden {
        start_lba: 35 * MIB,
        hidden: false,
    }]);
    assert_eq!(image.table().partitions[3].type_code, "0x0C");

    let mut image = Image::new(64 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Gpt },
        create(MIB, 8 * MIB, "ntfs"),
    ]);
    image.run(&[
        Operation::SetName {
            start_lba: MIB,
            name: "Photos".into(),
        },
        Operation::SetActive {
            start_lba: MIB,
            active: true,
        },
    ]);
    let table = image.table();
    assert_eq!(table.partitions[0].name.as_deref(), Some("Photos"));
    assert!(table.partitions[0].bootable);
    let long = "x".repeat(37);
    let plan = image.plan(&[Operation::SetName {
        start_lba: MIB,
        name: long,
    }]);
    assert!(plan.error.unwrap().message.contains("36"));
}

#[test]
fn primary_and_logical_swap() {
    let mut image = Image::new(64 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Mbr },
        create(MIB, 8 * MIB, "ntfs"),
        create(10 * MIB, 8 * MIB, "ntfs"),
        create(20 * MIB, 8 * MIB, "linux"),
    ]);
    image.run(&[Operation::MakeLogical {
        start_lba: 20 * MIB,
    }]);
    let table = image.table();
    assert_eq!(table.partitions[2].kind, Ext);
    assert_eq!(
        (table.partitions[3].kind, table.partitions[3].start_lba),
        (L, 20 * MIB)
    );
    // The one before it joins the extended partition too.
    image.run(&[Operation::MakeLogical {
        start_lba: 10 * MIB,
    }]);
    let kinds: Vec<_> = image
        .table()
        .partitions
        .iter()
        .map(|p| (p.kind, p.start_lba))
        .collect();
    assert_eq!(
        kinds,
        [(P, MIB), (Ext, 9 * MIB), (L, 10 * MIB), (L, 20 * MIB)]
    );
    // The first one back out, then the last.
    image.run(&[Operation::MakePrimary {
        start_lba: 10 * MIB,
    }]);
    image.run(&[Operation::MakePrimary {
        start_lba: 20 * MIB,
    }]);
    let kinds: Vec<_> = image
        .table()
        .partitions
        .iter()
        .map(|p| (p.kind, p.start_lba))
        .collect();
    assert_eq!(kinds.iter().filter(|k| k.0 == P).count(), 3);
    assert!(kinds.iter().all(|k| k.0 != Ext && k.0 != L), "{kinds:?}");
}

#[test]
fn preview_shows_the_result_and_writes_nothing() {
    let image = mbr_disk();
    let before = image.data.clone();
    let disk = Arc::new(image.disk());
    let plan = plan(&disk, &[Operation::Delete { start_lba: MIB }]).unwrap();
    assert!(plan.is_ready());
    assert_eq!(plan.sectors_written, 1, "only the MBR changes");
    let after = preview(Arc::clone(&disk), &plan.writes);
    let table = read_table(&after).unwrap();
    assert_eq!(table.partitions.len(), 3);
    assert!(table.partitions.iter().all(|p| p.start_lba != MIB));
    assert_eq!(image.data, before);
    let nothing = plan_for(&image, &[]);
    assert!(!nothing.is_ready(), "nothing to write");
}

fn plan_for(image: &Image, ops: &[Operation]) -> Plan {
    plan(&image.disk(), ops).unwrap()
}

#[test]
fn applying_saves_a_backup_that_undoes_it() {
    let image = mbr_disk();
    let original = image.data.clone();
    let dir = std::env::temp_dir().join(format!("datareco-backups-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let disk = image.disk();
    let plan = plan(&disk, &[Operation::Convert { style: Style::Gpt }]).unwrap();
    let mut target = MemoryTarget::with_data(original.clone(), 512);
    let change = Change {
        source: "image:test.img",
        changes: &plan.descriptions(),
        writes: &plan.writes,
        undoes: None,
        undoable: true,
    };
    let path = apply_writes(&disk, &change, &mut target, &dir).unwrap();
    let changed = Image {
        data: target.data().to_vec(),
        sector_size: 512,
    };
    assert_eq!(changed.table().kind, TableKind::Gpt);
    assert!(
        verify_writes(&changed.disk(), &plan.writes)
            .unwrap()
            .is_empty()
    );

    let listed = list_backups(&dir, "image:test.img");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].changes,
        ["Convert the disk to GPT, keeping every partition"]
    );
    assert!(list_backups(&dir, "disk:9").is_empty());

    let backup = read_backup(&path).unwrap();
    assert_eq!(next_to_undo(&dir, "image:test.img").unwrap().path, path);
    let undo = undo_writes(&changed.disk(), &backup).unwrap().unwrap();
    let mut undone = MemoryTarget::with_data(changed.data.clone(), 512);
    let change = Change {
        source: "image:test.img",
        changes: &["Undo: Convert the disk to GPT".to_owned()],
        writes: &undo,
        undoes: Some(&path),
        undoable: true,
    };
    apply_writes(&changed.disk(), &change, &mut undone, &dir).unwrap();
    assert_eq!(undone.data(), &original[..], "every byte back");
    // The undo is a change of its own, but not one `undo` picks: nothing is
    // left to undo.
    let listed = list_backups(&dir, "image:test.img");
    assert_eq!(listed.len(), 2);
    assert!(listed[0].undoes.is_some() && listed[1].undone);
    assert_eq!(next_to_undo(&dir, "image:test.img"), None);

    // Once the disk has changed again, undoing that backup is refused.
    let mut later = Image {
        data: target.data().to_vec(),
        sector_size: 512,
    };
    later.run(&[Operation::Delete { start_lba: MIB }]);
    let refused = undo_writes(&later.disk(), &backup).unwrap().unwrap_err();
    assert!(refused.contains("has changed since"), "{refused}");
    // And a disk of another size isn't the one backed up.
    let other = Image::new(32 * MIB);
    assert!(
        undo_writes(&other.disk(), &backup)
            .unwrap()
            .unwrap_err()
            .contains("different")
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn damaged_and_dynamic_tables_are_left_alone() {
    let mut image = mbr_disk();
    image.data[446 + 4] = 0x42; // dynamic disk
    assert!(image.plan(&[]).blocked.unwrap().contains("dynamic"));

    let mut image = Image::new(64 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Mbr },
        create(MIB, 8 * MIB, "ntfs"),
        create(20 * MIB, 8 * MIB, "ntfs"),
    ]);
    // Make the second overlap the first.
    image.data[446 + 16 + 8..446 + 16 + 12]
        .copy_from_slice(&u32::try_from(4 * MIB).unwrap().to_le_bytes());
    let blocked = image.plan(&[]).blocked.unwrap();
    assert!(blocked.contains("overlap"), "{blocked}");

    let mut image = Image::new(64 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Gpt },
        create(MIB, 8 * MIB, "ntfs"),
    ]);
    image.data[2 * 512 + 40] ^= 1; // the entries no longer match their checksum
    image.data[usize::try_from(64 * MIB - 33).unwrap() * 512 + 40] ^= 1;
    assert!(image.plan(&[]).blocked.unwrap().contains("checksum"));
}

#[test]
fn whole_disk_volumes_are_not_initialized() {
    let mut image = Image::new(64 * MIB);
    image.data[..512].copy_from_slice(&crate::boot::samples::ntfs(64 * MIB - 1));
    let plan = image.plan(&[Operation::Initialize { style: Style::Mbr }]);
    assert!(plan.error.unwrap().message.contains("hide it"));
}

/// Random operations, many times over: each either fails with a reason, or
/// leaves a table the reader accepts without complaint.
#[test]
fn random_operations_always_leave_valid_tables() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move |n: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state % n.max(1)
    };
    for round in 0..6 {
        let mut image = Image::new(24 * MIB);
        let style = if round % 2 == 0 {
            Style::Mbr
        } else {
            Style::Gpt
        };
        image.run(&[Operation::Initialize { style }]);
        let mut done = 0;
        for _ in 0..80 {
            let table = image.table();
            let segments = layout(&table);
            let starts: Vec<u64> = table
                .partitions
                .iter()
                .filter(|p| p.kind != Ext)
                .map(|p| p.start_lba)
                .collect();
            let pick = |n: u64, list: &[u64]| {
                list.get(usize::try_from(n).unwrap() % list.len().max(1))
                    .copied()
            };
            let op = match next(8) {
                0..=2 => {
                    let free: Vec<_> = segments
                        .iter()
                        .filter(|s| s.kind == SegmentKind::Free)
                        .collect();
                    let Some(segment) = free.get(usize::try_from(next(free.len() as u64)).unwrap())
                    else {
                        continue;
                    };
                    let start = (segment.start_lba + 1).next_multiple_of(MIB);
                    let room = segment.end_lba().saturating_sub(start);
                    if room < MIB {
                        continue;
                    }
                    Operation::Create {
                        start_lba: start,
                        sectors: MIB + next(room - MIB + 1),
                        type_code: ["ntfs", "linux", "fat32"][usize::try_from(next(3)).unwrap()]
                            .into(),
                        logical: segment.logical || next(3) == 0,
                        name: None,
                    }
                }
                3 => match pick(next(64), &starts) {
                    Some(start_lba) => Operation::Delete { start_lba },
                    None => continue,
                },
                4 => match pick(next(64), &starts) {
                    Some(start_lba) => Operation::MakeLogical { start_lba },
                    None => continue,
                },
                5 => match pick(next(64), &starts) {
                    Some(start_lba) => Operation::MakePrimary { start_lba },
                    None => continue,
                },
                6 => match pick(next(64), &starts) {
                    Some(start_lba) => Operation::SetHidden {
                        start_lba,
                        hidden: next(2) == 0,
                    },
                    None => continue,
                },
                _ => Operation::Convert {
                    style: if next(2) == 0 { Style::Mbr } else { Style::Gpt },
                },
            };
            let plan = image.plan(std::slice::from_ref(&op));
            if plan.error.is_some() {
                continue;
            }
            image.write(&plan.writes);
            let after = image.table(); // panics on any warning or error
            let parts = after.partitions.iter().filter(|p| p.kind != Ext).count();
            assert!(parts <= 128, "{op:?}");
            done += 1;
        }
        assert!(done > 20, "round {round}: only {done} operations worked");
    }
}

#[test]
fn converting_warns_about_data_it_writes_over() {
    let mut image = mbr_disk();
    let last = usize::try_from(64 * MIB - 1).unwrap();
    image.data[last * 512 + 7] = 0xA5; // RAID information, say
    image.data[2 * 512] = 0xEB; // a boot loader's first sectors
    let plan = image.plan(&[Operation::Convert { style: Style::Gpt }]);
    let warnings = &plan.steps[0].warnings;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Sectors 1–33 hold data")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("last 33 sectors hold data")),
        "{warnings:?}"
    );
}

#[test]
fn moving_partitions_changes_only_where_the_table_points() {
    let mut image = mbr_disk();
    // The FAT32 logical partition, further into the extended partition.
    let plan = plan_move(&image.disk(), 35 * MIB, 50 * MIB).unwrap();
    assert!(plan.is_ready(), "{plan:#?}");
    assert!(
        plan.steps[0]
            .description
            .contains("15.0 MiB towards the end"),
        "{:?}",
        plan.steps
    );
    image.write(&plan.writes);
    let logical: Vec<_> = image
        .table()
        .partitions
        .iter()
        .filter(|p| p.kind == L)
        .map(|p| (p.number, p.start_lba, p.sectors))
        .collect();
    assert_eq!(logical, [(5, 18 * MIB, 16 * MIB), (6, 50 * MIB, 8 * MIB)]);

    // The first logical partition, over part of its own old place: its boot
    // record stays the extended partition's first sector.
    let plan = plan_move(&image.disk(), 18 * MIB, 20 * MIB).unwrap();
    image.write(&plan.writes);
    assert_eq!(image.table().partitions[2].start_lba, 20 * MIB);

    let refused = |from, to| {
        plan_move(&image.disk(), from, to)
            .unwrap()
            .error
            .unwrap()
            .message
    };
    assert!(refused(MIB, 2 * MIB).contains("inside the extended partition"));
    assert!(refused(20 * MIB, 45 * MIB).contains("overlap partition 6"));
    assert!(refused(50 * MIB, 60 * MIB).contains("past the end"));
    assert!(refused(20 * MIB, 20 * MIB).contains("already starts there"));
    assert!(refused(3, 4).contains("no partition starting at sector 3"));
    // With the primary partition gone, its space is outside the extended one.
    image.run(&[Operation::Delete { start_lba: MIB }]);
    let error = plan_move(&image.disk(), 50 * MIB, 2 * MIB)
        .unwrap()
        .error
        .unwrap();
    assert!(
        error.message.contains("within the extended partition"),
        "{error:?}"
    );

    let mut gpt = Image::new(64 * MIB);
    gpt.run(&[
        Operation::Initialize { style: Style::Gpt },
        create(MIB, 8 * MIB, "ntfs"),
    ]);
    let plan = plan_move(&gpt.disk(), MIB, 30 * MIB).unwrap();
    gpt.write(&plan.writes);
    assert_eq!(gpt.table().partitions[0].start_lba, 30 * MIB);
    let error = plan_move(&gpt.disk(), 30 * MIB, 60 * MIB)
        .unwrap()
        .error
        .unwrap();
    assert!(
        error.message.contains("past the end") || error.message.contains("within sectors"),
        "{error:?}"
    );
}

#[test]
fn a_moves_table_change_is_not_undone_on_its_own() {
    let image = mbr_disk();
    let dir = std::env::temp_dir().join(format!("datareco-move-backups-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let disk = image.disk();
    let plan = plan_move(&disk, 35 * MIB, 50 * MIB).unwrap();
    let mut target = MemoryTarget::with_data(image.data.clone(), 512);
    let change = Change {
        source: "image:move.img",
        changes: &plan.descriptions(),
        writes: &plan.writes,
        undoes: None,
        undoable: false,
    };
    let path = apply_writes(&disk, &change, &mut target, &dir).unwrap();
    let listed = list_backups(&dir, "image:move.img");
    assert!(!listed[0].undoable);
    assert_eq!(next_to_undo(&dir, "image:move.img"), None);
    let moved = Image {
        data: target.data().to_vec(),
        sector_size: 512,
    };
    let refused = undo_writes(&moved.disk(), &read_backup(&path).unwrap())
        .unwrap()
        .unwrap_err();
    assert!(
        refused.contains("Move the partition back instead"),
        "{refused}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------- copying to another disk

/// A 256 MiB GPT disk: EFI at 1 MiB (16 MiB), NTFS at 100 MiB (32 MiB),
/// Linux at 200 MiB (8 MiB).
fn gpt_disk() -> Image {
    let mut image = Image::new(256 * MIB);
    image.run(&[
        Operation::Initialize { style: Style::Gpt },
        Operation::Create {
            start_lba: MIB,
            sectors: 16 * MIB,
            type_code: "efi".into(),
            logical: false,
            name: Some("EFI system partition".into()),
        },
        create(100 * MIB, 32 * MIB, "ntfs"),
        create(200 * MIB, 8 * MIB, "linux"),
    ]);
    image
}

/// Copies `source`'s partitions onto a blank disk of `sectors`, writes the
/// new table and reads it back.
fn copy_disk(source: &Image, sectors: u64, options: &DiskCopyOptions) -> (DiskCopy, Image) {
    let copy = plan_disk_copy(&source.disk(), "Disk 9", sectors, 512, options).unwrap();
    assert_eq!(copy.blocked, None);
    assert_eq!(copy.problem, None);
    let mut target = Image::new(sectors);
    target.write(&copy.writes);
    (copy, target)
}

fn places(copy: &DiskCopy) -> Vec<(u64, u64)> {
    copy.parts.iter().map(|p| (p.from_lba, p.to_lba)).collect()
}

#[test]
fn copying_a_disk_keeps_places_when_they_fit() {
    let source = gpt_disk();
    let options = DiskCopyOptions::default();
    let (copy, target) = copy_disk(&source, 512 * MIB, &options);
    assert!(!copy.packed);
    assert_eq!(
        places(&copy),
        [(MIB, MIB), (100 * MIB, 100 * MIB), (200 * MIB, 200 * MIB)]
    );
    let (before, after) = (source.table(), target.table());
    assert_eq!(
        after.disk_sectors,
        512 * MIB,
        "the backup GPT is at the new end"
    );
    assert_eq!(shape(&after), shape(&before));
    assert_ne!(after.disk_id, before.disk_id, "a new disk GUID");
    assert_ne!(after.partitions[1].guid, before.partitions[1].guid);
    assert_eq!(after.partitions[0].name, before.partitions[0].name);
    assert!(
        copy.warnings[0].contains("left unused, at the end"),
        "{:?}",
        copy.warnings
    );
    assert_eq!(copy.boot_sectors, 0, "nothing to carry over on GPT");

    let keep = DiskCopyOptions {
        keep_ids: true,
        ..DiskCopyOptions::default()
    };
    let (_, target) = copy_disk(&source, 512 * MIB, &keep);
    let after = target.table();
    assert_eq!(after.disk_id, before.disk_id);
    assert_eq!(after.partitions[1].guid, before.partitions[1].guid);
}

#[test]
fn copying_a_disk_onto_a_smaller_one_closes_the_gaps() {
    let source = gpt_disk();
    let (copy, target) = copy_disk(&source, 96 * MIB, &DiskCopyOptions::default());
    assert!(copy.packed);
    assert_eq!(
        places(&copy),
        [(MIB, MIB), (100 * MIB, 17 * MIB), (200 * MIB, 49 * MIB)]
    );
    assert_eq!(copy.needed_sectors, 57 * MIB + 33);
    let after = target.table();
    assert_eq!(
        after
            .partitions
            .iter()
            .map(|p| (p.start_lba, p.sectors))
            .collect::<Vec<_>>(),
        [(MIB, 16 * MIB), (17 * MIB, 32 * MIB), (49 * MIB, 8 * MIB)]
    );

    // Too small even closed up, or asked to keep places: says what's needed.
    let small = plan_disk_copy(
        &source.disk(),
        "Disk 9",
        40 * MIB,
        512,
        &DiskCopyOptions::default(),
    )
    .unwrap();
    let problem = small.problem.unwrap();
    assert!(
        problem.contains("needs to hold at least 57.0 MiB"),
        "{problem}"
    );
    let same = DiskCopyOptions {
        same_places: true,
        ..DiskCopyOptions::default()
    };
    let kept = plan_disk_copy(&source.disk(), "Disk 9", 96 * MIB, 512, &same).unwrap();
    assert!(
        kept.problem
            .unwrap()
            .contains("have to stay where they are")
    );
    assert!(kept.writes.is_empty());
}

#[test]
fn spare_room_can_go_after_a_partition() {
    let source = gpt_disk();
    let options = DiskCopyOptions {
        space_after: Some(100 * MIB),
        ..DiskCopyOptions::default()
    };
    let (copy, target) = copy_disk(&source, 512 * MIB, &options);
    assert!(copy.packed);
    // GPT's backup copy takes the last 33 sectors: the last partition ends
    // on the last 1 MiB boundary before them.
    assert_eq!(
        places(&copy),
        [(MIB, MIB), (100 * MIB, 17 * MIB), (200 * MIB, 503 * MIB)]
    );
    target.table();
    assert!(
        copy.warnings[0].contains("right after partition 2"),
        "{:?}",
        copy.warnings
    );
}

#[test]
fn copying_an_mbr_disk_packs_its_logical_partitions() {
    // Gaps: NTFS at 1 MiB, logical ones at 18 and 35 MiB; moved apart first.
    let mut source = mbr_disk();
    let plan = plan_move(&source.disk(), 35 * MIB, 50 * MIB).unwrap();
    source.write(&plan.writes);
    let (copy, target) = copy_disk(&source, 48 * MIB, &DiskCopyOptions::default());
    assert!(copy.packed);
    assert_eq!(copy.boot_sectors, MIB - 1);
    let after = target.table();
    assert_eq!(
        shape(&after),
        [
            (1, P, MIB, 16 * MIB, "0x07".into()),
            (2, Ext, 17 * MIB, 26 * MIB, "0x0F".into()),
            (5, L, 18 * MIB, 16 * MIB, "0x83".into()),
            (6, L, 35 * MIB, 8 * MIB, "0x0C".into()),
        ]
    );
    let before = source.table();
    assert_ne!(after.disk_id, before.disk_id, "a new disk signature");
}

#[test]
fn copying_a_disk_needs_the_same_sector_size() {
    let source = gpt_disk();
    let copy = plan_disk_copy(
        &source.disk(),
        "Disk 9",
        1 << 20,
        4096,
        &DiskCopyOptions::default(),
    )
    .unwrap();
    assert!(copy.blocked.unwrap().contains("4096-byte sectors"));
    let empty = Image::new(64 * MIB);
    let copy = plan_disk_copy(
        &empty.disk(),
        "Disk 9",
        64 * MIB,
        512,
        &DiskCopyOptions::default(),
    )
    .unwrap();
    assert!(copy.blocked.unwrap().contains("no partitions to copy"));
}

#[test]
fn copying_one_partition_into_free_space() {
    let source = mbr_disk();
    let mut target = gpt_disk();
    let copy = plan_partition_copy(&source.disk(), MIB, &target.disk(), 20 * MIB).unwrap();
    assert_eq!(copy.plan.blocked, None);
    assert_eq!(copy.plan.error, None);
    let part = copy.part.clone().unwrap();
    assert_eq!(
        (part.from_lba, part.to_lba, part.sectors),
        (MIB, 20 * MIB, 16 * MIB)
    );
    assert!(
        copy.plan.steps[0].description.starts_with(
            "Copy partition 1 (NTFS, exFAT or HPFS, 16.0 MiB) from Test disk to sector"
        ),
        "{:?}",
        copy.plan.steps
    );
    target.write(&copy.plan.writes);
    let after = target.table();
    let added = after
        .partitions
        .iter()
        .find(|p| p.start_lba == 20 * MIB)
        .unwrap();
    assert_eq!(
        added.type_name, "Microsoft basic data",
        "MBR 0x07 became basic data"
    );
    assert_eq!(added.sectors, 16 * MIB);

    // Into the extended partition of an MBR disk: a logical partition.
    let mut mbr = mbr_disk();
    let copy = plan_partition_copy(&target.disk(), 200 * MIB, &mbr.disk(), 44 * MIB).unwrap();
    assert!(copy.part.as_ref().unwrap().logical);
    mbr.write(&copy.plan.writes);
    let logical: Vec<_> = mbr
        .table()
        .partitions
        .iter()
        .filter(|p| p.kind == L)
        .map(|p| (p.start_lba, p.type_code.clone()))
        .collect();
    assert_eq!(
        logical,
        [
            (18 * MIB, "0x83".into()),
            (35 * MIB, "0x0C".into()),
            (44 * MIB, "0x83".into())
        ]
    );

    // Over a partition, or past the end: explained.
    let over = plan_partition_copy(&source.disk(), MIB, &target.disk(), 90 * MIB).unwrap();
    assert!(over.plan.error.unwrap().message.contains("overlaps"));
    assert!(over.part.is_none());
    assert!(over.plan.writes.is_empty());
    let past = plan_partition_copy(&source.disk(), MIB, &target.disk(), 250 * MIB).unwrap();
    assert!(past.plan.error.unwrap().message.contains("past the end"));
}
