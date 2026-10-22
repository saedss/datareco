//! Checks against the real disks of the machine running the tests.
#![cfg(windows)]

use datareco_io::{Disk, Error, SourceSpec, is_elevated, list_devices};

#[test]
fn lists_this_computers_disks_and_system_volume() {
    let devices = list_devices().unwrap();
    assert!(!devices.disks.is_empty(), "no physical disks found");
    for disk in &devices.disks {
        assert!(disk.size > 0, "{disk:?}");
        assert!(matches!(disk.sector_size, 512 | 4096), "{disk:?}");
    }
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let system_volume = devices
        .volumes
        .iter()
        .find(|v| {
            v.mount_points
                .iter()
                .any(|m| m.eq_ignore_ascii_case(&format!("{system}\\")))
        })
        .unwrap_or_else(|| panic!("no volume mounted at {system}: {:#?}", devices.volumes));
    assert!(system_volume.size.unwrap_or(0) > 0);
    assert!(!system_volume.extents.is_empty(), "{system_volume:?}");
}

#[test]
fn reading_the_first_disk_needs_administrator_rights() {
    let result = Disk::open(&SourceSpec::PhysicalDisk(0));
    if !is_elevated() {
        assert!(
            matches!(result, Err(Error::AccessDenied { .. })),
            "expected access denied"
        );
        return;
    }
    // Elevated (as on CI): sector 0 is an MBR or GPT protective MBR, which
    // both end with the 55 AA boot signature.
    let disk = result.unwrap();
    let (sector, outcome) = disk.read_sectors(0, 1).unwrap();
    assert!(outcome.unreadable.is_empty());
    assert_eq!(sector[510..512], [0x55, 0xAA]);
}

#[test]
fn reading_the_system_volume_starts_with_its_boot_sector() {
    if !is_elevated() {
        eprintln!("skipped: needs administrator rights");
        return;
    }
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let disk = Disk::open(&SourceSpec::parse(&system)).unwrap();
    let (sector, _) = disk.read_sectors(0, 1).unwrap();
    assert_eq!(
        &sector[3..11],
        b"NTFS    ",
        "the system volume should be NTFS"
    );
    // The last sector is readable too (NTFS keeps a backup boot sector there).
    disk.read_sectors(disk.sector_count() - 1, 1).unwrap();
}
