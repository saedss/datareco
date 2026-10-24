//! Windows: listing and opening physical disks and volumes, and elevation.
//!
//! Devices are opened with no access rights to query them (no administrator
//! rights needed), and with read access only to read them. Write access is
//! only requested by [`open_disk_for_writing`], for a disk that passed
//! `check_disk_target` and was confirmed by name.

use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io;
use std::iter;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_DEV_NOT_EXIST, ERROR_DEVICE_NOT_CONNECTED, ERROR_DEVICE_REMOVED,
    ERROR_NO_SUCH_DEVICE, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DeleteVolumeMountPointW, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH,
    FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FindFirstVolumeW,
    FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumePathNamesForVolumeNameW, OPEN_EXISTING, SetVolumeLabelW, SetVolumeMountPointW,
};
use windows_sys::Win32::Storage::IscsiDisc::{
    IOCTL_SCSI_PASS_THROUGH, SCSI_IOCTL_DATA_IN, SCSI_PASS_THROUGH,
};
use windows_sys::Win32::Storage::Nvme::NVME_LOG_PAGE_HEALTH_INFO;
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_EXTENT, DISK_GEOMETRY, DISK_GEOMETRY_EX, FSCTL_ALLOW_EXTENDED_DASD_IO,
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_SET_SPARSE, GET_LENGTH_INFORMATION, IDEREGS,
    IOCTL_DISK_GET_DRIVE_GEOMETRY, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_DISK_UPDATE_PROPERTIES, IOCTL_STORAGE_PREDICT_FAILURE, IOCTL_STORAGE_QUERY_PROPERTY,
    NVMeDataTypeLogPage, PropertyStandardQuery, ProtocolTypeNvme, SENDCMDINPARAMS,
    SENDCMDOUTPARAMS, SMART_RCV_DRIVE_DATA, STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR,
    STORAGE_DEVICE_DESCRIPTOR, STORAGE_PREDICT_FAILURE, STORAGE_PROTOCOL_DATA_DESCRIPTOR,
    STORAGE_PROTOCOL_SPECIFIC_DATA, StorageAccessAlignmentProperty,
    StorageAdapterProtocolSpecificProperty, StorageDeviceProperty,
    StorageDeviceProtocolSpecificProperty, StorageDeviceSeekPenaltyProperty, VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::devices::{DeviceList, DiskExtent, PhysicalDiskInfo, VolumeInfo};
use crate::error::{Error, Result};
use crate::health::{HEALTH_PAGE, HealthData, HealthPath};
use crate::source::FileSource;
use crate::spec::SourceSpec;
use crate::target::{OpenedDisk, OpenedVolume};

/// Physical disk numbers to probe. Windows numbers disks from 0 and rarely
/// leaves gaps.
const MAX_DISKS: u32 = 64;

/// `CTL_CODE(IOCTL_VOLUME_BASE ('V'), 0, METHOD_BUFFERED, FILE_ANY_ACCESS)`,
/// from winioctl.h. windows-sys doesn't include it.
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

// ---------------------------------------------------------------- opening

pub(crate) fn open_physical_disk(number: u32) -> Result<FileSource> {
    let name = SourceSpec::PhysicalDisk(number).display_name();
    let file = open_device(&disk_path(number), true).map_err(|e| Error::from_io(&name, e))?;
    let (geometry, size) = disk_geometry(&file).map_err(|e| Error::from_io(&name, e))?;
    Ok(FileSource::from_device(file, size, geometry.BytesPerSector))
}

pub(crate) fn open_volume(path: &str) -> Result<FileSource> {
    let name = SourceSpec::Volume(path.to_owned()).display_name();
    let file =
        open_device(&volume_device_path(path), true).map_err(|e| Error::from_io(&name, e))?;
    // Let reads reach the very end of the volume, past where the file system
    // says it ends (NTFS keeps its backup boot sector there). Not all volumes
    // support this, and that's fine.
    let _ = ioctl(&file, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], &mut []);
    let size = volume_length(&file).map_err(|e| Error::from_io(&name, e))?;
    let sector_size = volume_sector_size(&file).unwrap_or(512);
    Ok(FileSource::from_device(file, size, sector_size))
}

fn disk_path(number: u32) -> String {
    format!(r"\\.\PhysicalDrive{number}")
}

/// `C:` becomes `\\.\C:`; `\\?\Volume{…}` is used as is, without a trailing backslash.
fn volume_device_path(path: &str) -> String {
    let path = path.trim_end_matches('\\');
    if path.len() == 2 && path.ends_with(':') {
        format!(r"\\.\{path}")
    } else {
        path.to_owned()
    }
}

/// Opens a device. `read: false` asks for no access at all, which is enough
/// for queries and needs no administrator rights.
fn open_device(path: &str, read: bool) -> io::Result<File> {
    let access = if read { GENERIC_READ } else { 0 };
    create_file(path, access, 0)
}

fn create_file(path: &str, access: u32, flags: FILE_FLAGS_AND_ATTRIBUTES) -> io::Result<File> {
    let wide = to_wide(path);
    // SAFETY: `wide` is NUL-terminated and outlives the call; null security
    // attributes and template handle are allowed.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            flags,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `handle` is a valid handle that we own; the File closes it.
    Ok(unsafe { File::from_raw_handle(handle) })
}

// ---------------------------------------------------------------- writing

/// Locks and dismounts each volume, so neither Windows nor any program
/// uses them while the disk is written, then opens the disk for writing
/// straight to the drive. The locks last until the returned handles close.
pub(crate) fn open_disk_for_writing(number: u32, volumes: &[String]) -> Result<OpenedDisk> {
    let name = SourceSpec::PhysicalDisk(number).display_name();
    let locks = close_volumes(volumes, &name)?;
    let file = create_file(
        &disk_path(number),
        GENERIC_READ | GENERIC_WRITE,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
    )
    .map_err(|e| Error::from_io(&name, e))?;
    let (geometry, size) = disk_geometry(&file).map_err(|e| Error::from_io(&name, e))?;
    Ok(OpenedDisk {
        file,
        locks,
        size,
        sector_size: geometry.BytesPerSector,
    })
}

/// Locks and dismounts each volume (of the disk `disk` names), so neither
/// Windows nor any program uses them; the locks last until the returned
/// handles close, and then Windows mounts them again. Locking needs the
/// volume opened for writing, though nothing is written through it.
pub(crate) fn close_volumes(volumes: &[String], disk: &str) -> Result<Vec<File>> {
    let mut locks = Vec::new();
    for volume in volumes {
        let volume_name = SourceSpec::Volume(volume.clone()).display_name();
        let file = create_file(volume, GENERIC_READ | GENERIC_WRITE, 0)
            .map_err(|e| Error::from_io(&volume_name, e))?;
        if ioctl(&file, FSCTL_LOCK_VOLUME, &[], &mut []).is_err() {
            return Err(Error::Refused {
                reason: format!(
                    "{volume_name} on {disk} is in use. Close the programs and windows using \
                     it, then try again."
                ),
            });
        }
        ioctl(&file, FSCTL_DISMOUNT_VOLUME, &[], &mut [])
            .map_err(|e| Error::from_io(&volume_name, e))?;
        locks.push(file);
    }
    Ok(locks)
}

/// Asks Windows for a snapshot (a shadow copy) of a mounted NTFS or ReFS
/// volume: it stays as the volume was at this moment while the volume goes
/// on changing. Returns its ID and the device to read it from.
pub(crate) fn take_snapshot(volume: &str) -> Result<(String, String)> {
    let name = SourceSpec::Volume(volume.to_owned()).display_name();
    let root = format!("{}\\", volume.trim_end_matches('\\')).replace('\'', "''");
    let script = format!(
        "$r = Invoke-CimMethod -ClassName Win32_ShadowCopy -MethodName Create -Arguments \
         @{{ Volume = '{root}'; Context = 'ClientAccessible' }} -ErrorAction Stop; \
         if ($r.ReturnValue -ne 0) {{ throw \"Windows' snapshot service answered with error $($r.ReturnValue).\" }}; \
         $s = Get-CimInstance -ClassName Win32_ShadowCopy -Filter \"ID='$($r.ShadowID)'\" -ErrorAction Stop; \
         \"$($s.ID)|$($s.DeviceObject)\""
    );
    let out = powershell(&script, &name, "Windows couldn't take a snapshot of it")?;
    let line = out.lines().map(str::trim).find(|l| l.contains('|'));
    match line.and_then(|l| l.split_once('|')) {
        Some((id, device)) if !id.is_empty() && device.starts_with(r"\\?\GLOBALROOT\") => {
            Ok((id.to_owned(), device.to_owned()))
        }
        _ => Err(Error::Io {
            name,
            source: io::Error::other(format!("unexpected answer from Windows: {}", out.trim())),
        }),
    }
}

/// Deletes a snapshot [`take_snapshot`] took.
pub(crate) fn delete_snapshot(id: &str) -> Result<()> {
    // IDs look like {GUID}; nothing else goes into the script.
    if !id
        .chars()
        .all(|c| c.is_ascii_hexdigit() || matches!(c, '{' | '}' | '-'))
    {
        return Err(Error::refused("That isn't a snapshot ID."));
    }
    let script = format!(
        "Get-CimInstance -ClassName Win32_ShadowCopy -Filter \"ID='{id}'\" -ErrorAction Stop | \
         Remove-CimInstance -ErrorAction Stop"
    );
    powershell(&script, "a snapshot", "Windows couldn't delete it").map(|_| ())
}

/// Locks and dismounts one volume, so neither Windows nor any program uses
/// it while it's written, and opens it for writing straight to the drive.
/// The lock lasts until the handle closes; then Windows mounts it again.
pub(crate) fn open_volume_for_writing(path: &str) -> Result<OpenedVolume> {
    let name = SourceSpec::Volume(path.to_owned()).display_name();
    let file = create_file(
        &volume_device_path(path),
        GENERIC_READ | GENERIC_WRITE,
        FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH,
    )
    .map_err(|e| Error::from_io(&name, e))?;
    if ioctl(&file, FSCTL_LOCK_VOLUME, &[], &mut []).is_err() {
        return Err(Error::Refused {
            reason: format!(
                "{name} is in use. Close the programs and windows using it, then try again."
            ),
        });
    }
    ioctl(&file, FSCTL_DISMOUNT_VOLUME, &[], &mut []).map_err(|e| Error::from_io(&name, e))?;
    // Writes may reach the volume's very end, past any file system's idea of it.
    let _ = ioctl(&file, FSCTL_ALLOW_EXTENDED_DASD_IO, &[], &mut []);
    let size = volume_length(&file).map_err(|e| Error::from_io(&name, e))?;
    let sector_size = volume_sector_size(&file).unwrap_or(512);
    Ok(OpenedVolume {
        file,
        size,
        sector_size,
    })
}

/// `\\?\Volume{…}\`, the root Windows' volume functions take.
fn volume_root(path: &str) -> Vec<u16> {
    to_wide(&format!("{}\\", path.trim_end_matches('\\')))
}

pub(crate) fn set_volume_label(path: &str, label: Option<&str>) -> Result<()> {
    let name = SourceSpec::Volume(path.to_owned()).display_name();
    let root = volume_root(path);
    let label = label.map(to_wide);
    // SAFETY: both strings are NUL-terminated and outlive the call; a null
    // label removes it, which is allowed.
    let ok = unsafe {
        SetVolumeLabelW(
            root.as_ptr(),
            label.as_ref().map_or(ptr::null(), |l| l.as_ptr()),
        )
    };
    if ok == 0 {
        return Err(Error::from_io(&name, io::Error::last_os_error()));
    }
    Ok(())
}

pub(crate) fn set_drive_letter(path: &str, letter: Option<char>) -> Result<()> {
    let name = SourceSpec::Volume(path.to_owned()).display_name();
    let root = volume_root(path);
    let current: Vec<String> = mount_points(&root)
        .into_iter()
        .filter(|m| m.len() == 3 && m.ends_with(":\\"))
        .collect();
    if let Some(letter) = letter {
        let wanted = format!("{letter}:\\");
        if current.iter().any(|m| m.eq_ignore_ascii_case(&wanted)) {
            return Ok(());
        }
        // SAFETY: no arguments; returns a bitmask of drive letters in use.
        let in_use = unsafe { GetLogicalDrives() };
        let bit = u32::from(letter) - u32::from('A');
        if in_use & (1 << bit) != 0 {
            return Err(Error::refused(format!("{letter}: is already in use.")));
        }
    }
    for mount in &current {
        let mount = to_wide(mount);
        // SAFETY: `mount` is NUL-terminated and outlives the call.
        if unsafe { DeleteVolumeMountPointW(mount.as_ptr()) } == 0 {
            return Err(Error::from_io(&name, io::Error::last_os_error()));
        }
    }
    if let Some(letter) = letter {
        let mount = to_wide(&format!("{letter}:\\"));
        // SAFETY: both strings are NUL-terminated and outlive the call.
        if unsafe { SetVolumeMountPointW(mount.as_ptr(), root.as_ptr()) } == 0 {
            return Err(Error::from_io(&name, io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Formats a volume with Windows' own formatter (`Format-Volume`), for the
/// file systems DataReco doesn't format itself.
pub(crate) fn format_with_windows(
    path: &str,
    file_system: &str,
    label: Option<&str>,
    quick: bool,
) -> Result<()> {
    let name = SourceSpec::Volume(path.to_owned()).display_name();
    let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
    let mut script = format!(
        "Format-Volume -Path {} -FileSystem {} -Force -Confirm:$false -ErrorAction Stop",
        quote(&format!("{}\\", path.trim_end_matches('\\'))),
        file_system
    );
    if let Some(label) = label {
        script.push_str(&format!(" -NewFileSystemLabel {}", quote(label)));
    }
    if !quick {
        script.push_str(" -Full");
    }
    script.push_str(" | Out-Null");
    powershell(&script, &name, "Windows couldn't format it").map(|_| ())
}

/// Runs a PowerShell script for the Windows storage commands, and returns
/// what it printed. On failure, the error says `failed` and the first line
/// PowerShell gave as the reason.
fn powershell(script: &str, name: &str, failed: &str) -> Result<String> {
    use std::os::windows::process::CommandExt;
    // No console window flashing up behind the desktop app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| Error::from_io("PowerShell", e))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let message = String::from_utf8_lossy(&output.stderr);
    // The storage commands need administrator rights, like reading disks.
    if message.contains("PermissionDenied") || message.contains("Access denied") {
        return Err(Error::AccessDenied {
            name: name.to_owned(),
        });
    }
    let first = message
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("it gave no reason");
    Err(Error::Io {
        name: name.to_owned(),
        source: io::Error::other(format!("{failed}: {first}")),
    })
}

/// The partition starting `offset` bytes into disk `number`, for the
/// storage commands.
fn partition_query(number: u32, offset: u64) -> String {
    format!(
        "$p = Get-Partition -DiskNumber {number} -ErrorAction Stop | Where-Object {{ $_.Offset -eq {offset} }}; \
         if (-not $p) {{ throw 'Windows has no partition starting there.' }}; "
    )
}

pub(crate) fn partition_size_limits(number: u32, offset: u64) -> Result<(u64, u64)> {
    let name = SourceSpec::PhysicalDisk(number).display_name();
    let script = format!(
        "{}$s = $p | Get-PartitionSupportedSize -ErrorAction Stop; \"$($s.SizeMin) $($s.SizeMax)\"",
        partition_query(number, offset)
    );
    let out = powershell(
        &script,
        &name,
        "Windows couldn't work out how far it can resize it",
    )?;
    let mut numbers = out.split_whitespace().map(str::parse::<u64>);
    match (numbers.next(), numbers.next()) {
        (Some(Ok(min)), Some(Ok(max))) => Ok((min, max)),
        _ => Err(Error::Io {
            name,
            source: io::Error::other(format!("unexpected answer from Windows: {}", out.trim())),
        }),
    }
}

pub(crate) fn resize_with_windows(number: u32, offset: u64, size: u64) -> Result<()> {
    let name = SourceSpec::PhysicalDisk(number).display_name();
    let script = format!(
        "{}$p | Resize-Partition -Size {size} -ErrorAction Stop",
        partition_query(number, offset)
    );
    powershell(&script, &name, "Windows couldn't resize it").map(|_| ())
}

/// Asks Windows to read the disk's partition table again.
pub(crate) fn update_disk_properties(file: &File) {
    let _ = ioctl(file, IOCTL_DISK_UPDATE_PROPERTIES, &[], &mut []);
}

/// Marks a new file sparse, so parts never written take no space.
pub(crate) fn make_sparse(file: &File) -> io::Result<()> {
    ioctl(file, FSCTL_SET_SPARSE, &[], &mut []).map(|_| ())
}

/// Whether a read failed because the device is gone (unplugged), rather
/// than because of the sectors read.
pub(crate) fn is_device_gone(error: &io::Error) -> bool {
    error.raw_os_error().is_some_and(|code| {
        u32::try_from(code).is_ok_and(|code| {
            [
                ERROR_DEV_NOT_EXIST,
                ERROR_DEVICE_NOT_CONNECTED,
                ERROR_DEVICE_REMOVED,
                ERROR_NO_SUCH_DEVICE,
            ]
            .contains(&code)
        })
    })
}

// ---------------------------------------------------------------- listing

pub(crate) fn list_devices() -> Result<DeviceList> {
    Ok(DeviceList {
        disks: (0..MAX_DISKS).filter_map(describe_disk).collect(),
        volumes: list_volumes(),
    })
}

fn describe_disk(number: u32) -> Option<PhysicalDiskInfo> {
    let path = disk_path(number);
    let file = open_device(&path, false).ok()?;
    // Card readers without a card have no geometry; skip them.
    let (geometry, size) = disk_geometry(&file).ok()?;
    let descriptor = device_descriptor(&file);
    let physical_sector_size = storage_query(&file, StorageAccessAlignmentProperty)
        .ok()
        .and_then(|reply| read_struct::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>(&reply, 0))
        .map(|alignment| alignment.BytesPerPhysicalSector)
        .filter(|&bytes| bytes > 0);
    // DEVICE_SEEK_PENALTY_DESCRIPTOR: Version, Size, then a one-byte
    // IncursSeekPenalty, read as a byte (a Rust bool can't hold any value).
    let solid_state = storage_query(&file, StorageDeviceSeekPenaltyProperty)
        .ok()
        .and_then(|reply| reply.get(8).map(|&penalty| penalty == 0));
    Some(PhysicalDiskInfo {
        number,
        path,
        model: descriptor.model,
        serial: descriptor.serial,
        bus: descriptor.bus,
        removable: descriptor.removable,
        size,
        sector_size: geometry.BytesPerSector,
        physical_sector_size,
        firmware: descriptor.firmware,
        solid_state,
    })
}

struct Descriptor {
    model: String,
    serial: Option<String>,
    firmware: Option<String>,
    bus: String,
    removable: bool,
}

fn device_descriptor(file: &File) -> Descriptor {
    let unknown = Descriptor {
        model: "Unknown disk".to_owned(),
        serial: None,
        firmware: None,
        bus: "Unknown".to_owned(),
        removable: false,
    };
    let Ok(reply) = storage_query(file, StorageDeviceProperty) else {
        return unknown;
    };
    let Some(descriptor) = read_struct::<STORAGE_DEVICE_DESCRIPTOR>(&reply, 0) else {
        return unknown;
    };
    let text = |offset: u32| c_string(&reply, offset as usize);
    let model = [
        text(descriptor.VendorIdOffset),
        text(descriptor.ProductIdOffset),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    Descriptor {
        model: if model.is_empty() {
            unknown.model
        } else {
            model
        },
        serial: text(descriptor.SerialNumberOffset),
        firmware: text(descriptor.ProductRevisionOffset),
        bus: bus_name(descriptor.BusType).to_owned(),
        removable: descriptor.RemovableMedia,
    }
}

/// Names for `STORAGE_BUS_TYPE` values.
fn bus_name(bus: i32) -> &'static str {
    match bus {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        4 => "IEEE 1394",
        5 => "SSA",
        6 => "Fibre Channel",
        7 => "USB",
        8 => "RAID",
        9 => "iSCSI",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        14 => "Virtual",
        15 => "Virtual (file)",
        16 => "Storage Spaces",
        17 => "NVMe",
        18 => "SCM",
        19 => "UFS",
        _ => "Unknown",
    }
}

fn list_volumes() -> Vec<VolumeInfo> {
    let mut name = [0u16; 512];
    // SAFETY: `name` is writable for the length passed.
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), len32(name.len())) };
    if find == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut volumes = Vec::new();
    loop {
        if let Some(volume) = describe_volume(&from_wide(&name)) {
            volumes.push(volume);
        }
        // SAFETY: `find` is the open search handle; `name` as above.
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), len32(name.len())) } == 0 {
            break;
        }
    }
    // SAFETY: `find` came from FindFirstVolumeW and is closed exactly once.
    unsafe { FindVolumeClose(find) };
    // Volumes with drive letters first, in letter order.
    volumes.sort_by(|a, b| {
        (a.mount_points.is_empty(), &a.mount_points)
            .cmp(&(b.mount_points.is_empty(), &b.mount_points))
    });
    volumes
}

/// `path` is a volume GUID path with its trailing backslash, as
/// FindFirstVolumeW returns it.
fn describe_volume(path: &str) -> Option<VolumeInfo> {
    let root = to_wide(path);
    let device = path.trim_end_matches('\\');
    let extents = open_device(device, false)
        .and_then(|file| disk_extents(&file))
        .unwrap_or_default();
    let space = space(&root);
    let size = if extents.is_empty() {
        space.map(|(total, _)| total)
    } else {
        Some(extents.iter().map(|extent| extent.length).sum())
    };
    // No size means no media (an empty card reader or DVD drive).
    size?;
    let (label, file_system) = volume_information(&root);
    Some(VolumeInfo {
        path: device.to_owned(),
        mount_points: mount_points(&root),
        label,
        file_system,
        size,
        free: space.map(|(_, free)| free),
        extents,
    })
}

fn mount_points(root: &[u16]) -> Vec<String> {
    let mut buf = vec![0u16; 1024];
    let mut needed = 0u32;
    // SAFETY: `root` is NUL-terminated; `buf` is writable for the length passed.
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(
            root.as_ptr(),
            buf.as_mut_ptr(),
            len32(buf.len()),
            &mut needed,
        )
    };
    if ok == 0 {
        return Vec::new();
    }
    // A list of NUL-terminated strings, ended by an empty one.
    buf.split(|&c| c == 0)
        .take_while(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

fn volume_information(root: &[u16]) -> (Option<String>, Option<String>) {
    let mut label = [0u16; 261];
    let mut file_system = [0u16; 261];
    // SAFETY: `root` is NUL-terminated; output buffers are writable for the
    // lengths passed; the outputs we don't need are null, which is allowed.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            len32(label.len()),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            file_system.as_mut_ptr(),
            len32(file_system.len()),
        )
    };
    if ok == 0 {
        return (None, None);
    }
    (
        non_empty(from_wide(&label)),
        non_empty(from_wide(&file_system)),
    )
}

/// A mounted volume's size and free space, in bytes, as its file system
/// reports them.
fn space(root: &[u16]) -> Option<(u64, u64)> {
    let mut total = 0u64;
    let mut free = 0u64;
    // SAFETY: `root` is NUL-terminated; `total` and `free` are valid outputs;
    // the space free to this user isn't asked for (null), which is allowed.
    let ok = unsafe { GetDiskFreeSpaceExW(root.as_ptr(), ptr::null_mut(), &mut total, &mut free) };
    (ok != 0).then_some((total, free))
}

// ---------------------------------------------------------------- device queries

/// The disk's geometry and its size in bytes.
fn disk_geometry(file: &File) -> io::Result<(DISK_GEOMETRY, u64)> {
    let mut reply = [0u8; 256];
    let n = ioctl(file, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], &mut reply)?;
    let reply = &reply[..n];
    let geometry = read_struct::<DISK_GEOMETRY>(reply, offset_of!(DISK_GEOMETRY_EX, Geometry))
        .ok_or_else(short_reply)?;
    let size = read_struct::<i64>(reply, offset_of!(DISK_GEOMETRY_EX, DiskSize))
        .ok_or_else(short_reply)?;
    Ok((geometry, u64::try_from(size).unwrap_or(0)))
}

fn volume_length(file: &File) -> io::Result<u64> {
    let mut reply = [0u8; size_of::<GET_LENGTH_INFORMATION>()];
    let n = ioctl(file, IOCTL_DISK_GET_LENGTH_INFO, &[], &mut reply)?;
    let length = read_struct::<GET_LENGTH_INFORMATION>(&reply[..n], 0).ok_or_else(short_reply)?;
    Ok(u64::try_from(length.Length).unwrap_or(0))
}

fn volume_sector_size(file: &File) -> Option<u32> {
    let mut reply = [0u8; size_of::<DISK_GEOMETRY>()];
    let n = ioctl(file, IOCTL_DISK_GET_DRIVE_GEOMETRY, &[], &mut reply).ok()?;
    read_struct::<DISK_GEOMETRY>(&reply[..n], 0)
        .map(|geometry| geometry.BytesPerSector)
        .filter(|&bytes| bytes > 0)
}

fn disk_extents(file: &File) -> io::Result<Vec<DiskExtent>> {
    let mut reply = vec![0u8; 4096];
    let n = ioctl(file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, &[], &mut reply)?;
    let reply = &reply[..n];
    let count = read_struct::<u32>(reply, offset_of!(VOLUME_DISK_EXTENTS, NumberOfDiskExtents))
        .ok_or_else(short_reply)?;
    let first = offset_of!(VOLUME_DISK_EXTENTS, Extents);
    (0..count as usize)
        .map(|i| {
            let extent = read_struct::<DISK_EXTENT>(reply, first + i * size_of::<DISK_EXTENT>())
                .ok_or_else(short_reply)?;
            Ok(DiskExtent {
                disk: extent.DiskNumber,
                offset: u64::try_from(extent.StartingOffset).unwrap_or(0),
                length: u64::try_from(extent.ExtentLength).unwrap_or(0),
            })
        })
        // Virtual drives (e.g. Google Drive) report a zero-length extent on
        // disk 0 that doesn't exist.
        .filter(|extent| !matches!(extent, Ok(DiskExtent { length: 0, .. })))
        .collect()
}

/// Runs IOCTL_STORAGE_QUERY_PROPERTY for a standard query of `property`.
fn storage_query(file: &File, property: i32) -> io::Result<Vec<u8>> {
    // STORAGE_PROPERTY_QUERY: PropertyId (i32), QueryType (i32), then one
    // byte of AdditionalParameters padded to 4. Built by hand so no padding
    // bytes are ever read.
    let mut query = [0u8; 12];
    query[..4].copy_from_slice(&property.to_le_bytes());
    query[4..8].copy_from_slice(&PropertyStandardQuery.to_le_bytes());
    let mut reply = vec![0u8; 1024];
    let n = ioctl(file, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut reply)?;
    reply.truncate(n);
    Ok(reply)
}

fn ioctl(file: &File, code: u32, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let input_ptr = if input.is_empty() {
        ptr::null()
    } else {
        input.as_ptr().cast::<c_void>()
    };
    let output_ptr = if output.is_empty() {
        ptr::null_mut()
    } else {
        output.as_mut_ptr().cast::<c_void>()
    };
    let mut returned = 0u32;
    // SAFETY: the buffers are null or valid for the lengths passed, for the
    // whole call; the call is synchronous (no OVERLAPPED).
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input_ptr,
            len32(input.len()),
            output_ptr,
            len32(output.len()),
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

/// Reads a plain-data Windows struct from `bytes` at `offset`.
fn read_struct<T: Copy>(bytes: &[u8], offset: usize) -> Option<T> {
    let slice = bytes.get(offset..offset.checked_add(size_of::<T>())?)?;
    // SAFETY: `slice` holds exactly size_of::<T>() bytes. T is only ever a
    // plain-data Windows struct or integer, valid for any bit pattern, and
    // `read_unaligned` has no alignment requirement.
    Some(unsafe { ptr::read_unaligned(slice.as_ptr().cast::<T>()) })
}

fn short_reply() -> io::Error {
    io::Error::other("the device returned less data than expected")
}

/// Writes `bytes` into `buffer` at `offset`, for requests built by hand
/// (so no struct padding bytes are ever sent).
fn put(buffer: &mut [u8], offset: usize, bytes: &[u8]) {
    buffer[offset..offset + bytes.len()].copy_from_slice(bytes);
}

// ---------------------------------------------------------------- drive health

/// The only ATA commands DataReco sends. All of them read; keeping them in
/// one closed list means nothing else can be sent by mistake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AtaRead {
    /// SMART READ DATA: the attribute page.
    SmartData,
    /// SMART READ ATTRIBUTE THRESHOLDS.
    SmartThresholds,
    /// IDENTIFY DEVICE.
    Identify,
}

/// ATA command codes and the registers S.M.A.R.T. commands need (ACS-3).
const ATA_SMART: u8 = 0xB0;
const ATA_IDENTIFY_DEVICE: u8 = 0xEC;
const SMART_READ_DATA: u8 = 0xD0;
const SMART_READ_THRESHOLDS: u8 = 0xD1;
const SMART_LBA_MID: u8 = 0x4F;
const SMART_LBA_HIGH: u8 = 0xC2;
/// The device register: LBA mode, device 0.
const ATA_DEVICE: u8 = 0xA0;

impl AtaRead {
    /// (features, LBA mid, LBA high, command).
    fn registers(self) -> (u8, u8, u8, u8) {
        match self {
            AtaRead::SmartData => (SMART_READ_DATA, SMART_LBA_MID, SMART_LBA_HIGH, ATA_SMART),
            AtaRead::SmartThresholds => (
                SMART_READ_THRESHOLDS,
                SMART_LBA_MID,
                SMART_LBA_HIGH,
                ATA_SMART,
            ),
            AtaRead::Identify => (0, 0, 0, ATA_IDENTIFY_DEVICE),
        }
    }
}

pub(crate) fn read_health_data(number: u32) -> Result<HealthData> {
    let name = SourceSpec::PhysicalDisk(number).display_name();
    let path = disk_path(number);
    let query = open_device(&path, false).map_err(|e| Error::from_io(&name, e))?;
    let bus = device_descriptor(&query).bus;
    if bus == "NVMe" {
        let file = open_device(&path, true).map_err(|e| Error::from_io(&name, e))?;
        return Ok(match nvme_health_log(&file) {
            Ok(log) => HealthData::Nvme { log },
            Err(e) => HealthData::Unavailable {
                reason: format!("{name} didn't return its NVMe health log ({e})."),
            },
        });
    }

    // Windows passes ATA commands on only through a handle opened for
    // writing. Only the read-only commands of `AtaRead` go through it.
    let file = create_file(&path, GENERIC_READ | GENERIC_WRITE, 0)
        .map_err(|e| Error::from_io(&name, e))?;
    let prediction = predict_failure(&query).ok();
    let failure_predicted = prediction.as_ref().map(|(predicted, _)| *predicted);

    let driver = |command| ata_through_driver(&file, command);
    if let Some(attributes) = driver(AtaRead::SmartData)
        .ok()
        .filter(|page| has_attributes(page))
    {
        return Ok(HealthData::Ata {
            attributes,
            thresholds: driver(AtaRead::SmartThresholds).ok(),
            identify: driver(AtaRead::Identify).ok(),
            failure_predicted,
            via: HealthPath::Driver,
        });
    }
    // USB adapters that follow SCSI/ATA Translation pass ATA commands on.
    // Only tried over USB: elsewhere the driver path above is the one that works.
    let adapter = |command| ata_through_usb_adapter(&file, command);
    if bus == "USB"
        && let Some(attributes) = adapter(AtaRead::SmartData)
            .ok()
            .filter(|page| has_attributes(page))
    {
        return Ok(HealthData::Ata {
            attributes,
            thresholds: adapter(AtaRead::SmartThresholds).ok(),
            identify: adapter(AtaRead::Identify).ok(),
            failure_predicted,
            via: HealthPath::UsbAdapter,
        });
    }
    if let Some((predicted, page)) = prediction
        && (predicted || has_attributes(&page))
    {
        return Ok(HealthData::Ata {
            attributes: page,
            thresholds: None,
            identify: None,
            failure_predicted: Some(predicted),
            via: HealthPath::FailurePrediction,
        });
    }

    let identify = driver(AtaRead::Identify).ok();
    let reason = if identify.as_deref().is_some_and(smart_turned_off) {
        format!(
            "S.M.A.R.T. is turned off on {name}. DataReco doesn't change drive settings; it \
             can usually be turned on in the computer's firmware (BIOS or UEFI) setup."
        )
    } else if bus == "USB" {
        format!(
            "{name} is connected through a USB adapter that doesn't pass on the drive's health \
             data. Connected directly (SATA or NVMe), it may."
        )
    } else {
        format!(
            "{name} didn't return any health data. Memory cards, USB sticks, RAID volumes and \
             virtual disks usually have none."
        )
    };
    Ok(HealthData::Unavailable { reason })
}

/// Whether an ATA S.M.A.R.T. attribute page lists any attribute: some
/// adapters answer with an empty page instead of an error.
fn has_attributes(page: &[u8]) -> bool {
    page.len() == HEALTH_PAGE && (0..30).any(|i| page[2 + 12 * i] != 0)
}

/// Whether IDENTIFY DEVICE says S.M.A.R.T. is supported (word 82, bit 0)
/// but turned off (word 85, bit 0).
fn smart_turned_off(identify: &[u8]) -> bool {
    let bit0 = |word: usize| identify.get(2 * word).is_some_and(|&low| low & 1 != 0);
    identify.len() == HEALTH_PAGE && bit0(82) && !bit0(85)
}

/// SENDCMDINPARAMS (packed) for a read command returning one 512-byte page.
fn smart_request(command: AtaRead) -> Vec<u8> {
    let (features, mid, high, code) = command.registers();
    let mut input = vec![0u8; size_of::<SENDCMDINPARAMS>()];
    put(
        &mut input,
        offset_of!(SENDCMDINPARAMS, cBufferSize),
        &len32(HEALTH_PAGE).to_le_bytes(),
    );
    let registers = offset_of!(SENDCMDINPARAMS, irDriveRegs);
    let mut set = |field: usize, value: u8| input[registers + field] = value;
    set(offset_of!(IDEREGS, bFeaturesReg), features);
    set(offset_of!(IDEREGS, bSectorCountReg), 1);
    set(offset_of!(IDEREGS, bSectorNumberReg), 1);
    set(offset_of!(IDEREGS, bCylLowReg), mid);
    set(offset_of!(IDEREGS, bCylHighReg), high);
    set(offset_of!(IDEREGS, bDriveHeadReg), ATA_DEVICE);
    set(offset_of!(IDEREGS, bCommandReg), code);
    // bDriveNumber stays 0: the handle already names the drive.
    input
}

/// Sends a read command through the disk driver's S.M.A.R.T. interface.
fn ata_through_driver(file: &File, command: AtaRead) -> io::Result<Vec<u8>> {
    let data = offset_of!(SENDCMDOUTPARAMS, bBuffer);
    let mut reply = vec![0u8; data + HEALTH_PAGE];
    let n = ioctl(
        file,
        SMART_RCV_DRIVE_DATA,
        &smart_request(command),
        &mut reply,
    )?;
    let error = reply[offset_of!(SENDCMDOUTPARAMS, DriverStatus)];
    if error != 0 {
        return Err(io::Error::other(format!(
            "the drive refused the command (error {error})"
        )));
    }
    if n < data + HEALTH_PAGE {
        return Err(short_reply());
    }
    Ok(reply[data..data + HEALTH_PAGE].to_vec())
}

/// ATA PASS-THROUGH (16) from SCSI/ATA Translation (SAT-3): a PIO data-in
/// command reading one 512-byte block.
fn sat_cdb(command: AtaRead) -> [u8; 16] {
    let (features, mid, high, code) = command.registers();
    let mut cdb = [0u8; 16];
    cdb[0] = 0x85; // ATA PASS-THROUGH (16)
    cdb[1] = 4 << 1; // protocol 4, PIO data-in; 28-bit command
    cdb[2] = 0x0E; // data from the device, counted in blocks, length in the sector count
    cdb[4] = features;
    cdb[6] = 1; // sector count
    cdb[8] = 1; // LBA low (sector number)
    cdb[10] = mid;
    cdb[12] = high;
    cdb[13] = ATA_DEVICE;
    cdb[14] = code;
    cdb
}

/// Sense data bytes asked for with pass-through commands.
const SENSE_BYTES: usize = 32;

/// SCSI_PASS_THROUGH followed by its sense and data buffers.
fn sat_request(command: AtaRead) -> (Vec<u8>, usize) {
    let header = size_of::<SCSI_PASS_THROUGH>();
    let sense = header;
    let data = (header + SENSE_BYTES).next_multiple_of(8);
    let mut buffer = vec![0u8; data + HEALTH_PAGE];
    put(
        &mut buffer,
        offset_of!(SCSI_PASS_THROUGH, Length),
        &u16::try_from(header).unwrap_or(u16::MAX).to_le_bytes(),
    );
    buffer[offset_of!(SCSI_PASS_THROUGH, CdbLength)] = 16;
    buffer[offset_of!(SCSI_PASS_THROUGH, SenseInfoLength)] =
        u8::try_from(SENSE_BYTES).unwrap_or(u8::MAX);
    buffer[offset_of!(SCSI_PASS_THROUGH, DataIn)] = u8::try_from(SCSI_IOCTL_DATA_IN).unwrap_or(1);
    put(
        &mut buffer,
        offset_of!(SCSI_PASS_THROUGH, DataTransferLength),
        &len32(HEALTH_PAGE).to_le_bytes(),
    );
    put(
        &mut buffer,
        offset_of!(SCSI_PASS_THROUGH, TimeOutValue),
        &10u32.to_le_bytes(),
    );
    put(
        &mut buffer,
        offset_of!(SCSI_PASS_THROUGH, DataBufferOffset),
        &data.to_le_bytes(),
    );
    put(
        &mut buffer,
        offset_of!(SCSI_PASS_THROUGH, SenseInfoOffset),
        &len32(sense).to_le_bytes(),
    );
    put(
        &mut buffer,
        offset_of!(SCSI_PASS_THROUGH, Cdb),
        &sat_cdb(command),
    );
    (buffer, data)
}

/// Sends a read command through a USB adapter's SCSI/ATA Translation.
fn ata_through_usb_adapter(file: &File, command: AtaRead) -> io::Result<Vec<u8>> {
    let (request, data) = sat_request(command);
    let mut reply = vec![0u8; request.len()];
    ioctl(file, IOCTL_SCSI_PASS_THROUGH, &request, &mut reply)?;
    let status = reply[offset_of!(SCSI_PASS_THROUGH, ScsiStatus)];
    if status != 0 {
        return Err(io::Error::other(format!(
            "the adapter refused the command (SCSI status {status:#04x})"
        )));
    }
    Ok(reply[data..data + HEALTH_PAGE].to_vec())
}

/// Windows' failure prediction: whether the drive says it will fail, and
/// (for ATA drives) its S.M.A.R.T. attribute page. Needs no write access.
fn predict_failure(file: &File) -> io::Result<(bool, Vec<u8>)> {
    let mut reply = vec![0u8; size_of::<STORAGE_PREDICT_FAILURE>()];
    let n = ioctl(file, IOCTL_STORAGE_PREDICT_FAILURE, &[], &mut reply)?;
    let page = offset_of!(STORAGE_PREDICT_FAILURE, VendorSpecific);
    if n < page + HEALTH_PAGE {
        return Err(short_reply());
    }
    let predicted = read_struct::<u32>(&reply, offset_of!(STORAGE_PREDICT_FAILURE, PredictFailure))
        .ok_or_else(short_reply)?;
    Ok((predicted != 0, reply[page..page + HEALTH_PAGE].to_vec()))
}

/// The NVMe SMART / Health Information log, asked of the device and then
/// of its adapter (drivers answer one or the other).
fn nvme_health_log(file: &File) -> io::Result<Vec<u8>> {
    match nvme_query(file, StorageDeviceProtocolSpecificProperty) {
        Ok(log) => Ok(log),
        Err(_) => nvme_query(file, StorageAdapterProtocolSpecificProperty),
    }
}

/// STORAGE_PROPERTY_QUERY with STORAGE_PROTOCOL_SPECIFIC_DATA in its
/// AdditionalParameters, asking for a log page, and room for the page.
fn nvme_request(property: i32) -> Vec<u8> {
    let parameters = 8; // after PropertyId and QueryType
    let specific = size_of::<STORAGE_PROTOCOL_SPECIFIC_DATA>();
    let mut query = vec![0u8; parameters + specific + HEALTH_PAGE];
    put(&mut query, 0, &property.to_le_bytes());
    put(&mut query, 4, &PropertyStandardQuery.to_le_bytes());
    let mut field = |offset: usize, value: u32| {
        put(&mut query, parameters + offset, &value.to_le_bytes());
    };
    let code = |value: i32| u32::try_from(value).unwrap_or(0);
    field(
        offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolType),
        code(ProtocolTypeNvme),
    );
    field(
        offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, DataType),
        code(NVMeDataTypeLogPage),
    );
    field(
        offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataRequestValue),
        code(NVME_LOG_PAGE_HEALTH_INFO),
    );
    field(
        offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataOffset),
        len32(specific),
    );
    field(
        offset_of!(STORAGE_PROTOCOL_SPECIFIC_DATA, ProtocolDataLength),
        len32(HEALTH_PAGE),
    );
    query
}

fn nvme_query(file: &File, property: i32) -> io::Result<Vec<u8>> {
    let request = nvme_request(property);
    let mut reply = vec![0u8; request.len()];
    let n = ioctl(file, IOCTL_STORAGE_QUERY_PROPERTY, &request, &mut reply)?;
    let reply = &reply[..n];
    let specific = offset_of!(STORAGE_PROTOCOL_DATA_DESCRIPTOR, ProtocolSpecificData);
    let value = |field: usize| read_struct::<u32>(reply, specific + field).ok_or_else(short_reply);
    let offset = value(offset_of!(
        STORAGE_PROTOCOL_SPECIFIC_DATA,
        ProtocolDataOffset
    ))? as usize;
    let length = value(offset_of!(
        STORAGE_PROTOCOL_SPECIFIC_DATA,
        ProtocolDataLength
    ))? as usize;
    if length < HEALTH_PAGE {
        return Err(short_reply());
    }
    let start = specific.checked_add(offset).ok_or_else(short_reply)?;
    reply
        .get(start..start + HEALTH_PAGE)
        .map(<[u8]>::to_vec)
        .ok_or_else(short_reply)
}

// ---------------------------------------------------------------- elevation

pub(crate) fn is_elevated() -> bool {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo-handle that needs no closing;
    // `token` receives a new handle, closed below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut returned = 0u32;
    // SAFETY: `elevation` is a valid output buffer of the size passed.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast::<c_void>(),
            len32(size_of::<TOKEN_ELEVATION>()),
            &mut returned,
        )
    };
    // SAFETY: `token` was opened above and is closed exactly once.
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

pub(crate) fn relaunch_elevated(arguments: &[String]) -> Result<()> {
    let name = "Restarting as administrator";
    let exe = std::env::current_exe().map_err(|e| Error::from_io(name, e))?;
    let arguments: Vec<String> = arguments.iter().map(|arg| quote_argument(arg)).collect();
    let verb = to_wide("runas");
    let file: Vec<u16> = exe.as_os_str().encode_wide().chain(iter::once(0)).collect();
    let parameters = to_wide(&arguments.join(" "));
    // SAFETY: every string is NUL-terminated and outlives the call; a null
    // parent window and working directory are allowed.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ptr(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW signals success with a value above 32.
    if result as isize > 32 {
        Ok(())
    } else {
        Err(Error::Io {
            name: name.to_owned(),
            source: io::Error::last_os_error(),
        })
    }
}

fn quote_argument(argument: &str) -> String {
    if argument.is_empty() || argument.contains([' ', '\t', '"']) {
        format!("\"{}\"", argument.replace('"', "\\\""))
    } else {
        argument.to_owned()
    }
}

// ---------------------------------------------------------------- strings

fn to_wide(text: &str) -> Vec<u16> {
    OsStr::new(text)
        .encode_wide()
        .chain(iter::once(0))
        .collect()
}

/// The text before the first NUL.
fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

/// A NUL-terminated ASCII string at `offset` in an ioctl reply; offset 0 means none.
fn c_string(bytes: &[u8], offset: usize) -> Option<String> {
    if offset == 0 {
        return None;
    }
    let rest = bytes.get(offset..)?;
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    non_empty(String::from_utf8_lossy(&rest[..end]).trim().to_owned())
}

fn non_empty(text: String) -> Option<String> {
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn len32(len: usize) -> u32 {
    u32::try_from(len).expect("buffers passed to Windows are small")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_device_paths() {
        assert_eq!(volume_device_path("C:"), r"\\.\C:");
        let guid = r"\\?\Volume{12345678-1234-1234-1234-123456789abc}";
        assert_eq!(volume_device_path(&format!("{guid}\\")), guid);
    }

    #[test]
    fn arguments_with_spaces_are_quoted() {
        assert_eq!(quote_argument("disk0"), "disk0");
        assert_eq!(
            quote_argument(r"C:\My Images\a.img"),
            r#""C:\My Images\a.img""#
        );
        assert_eq!(quote_argument(""), r#""""#);
    }

    #[test]
    fn smart_requests_carry_read_commands_only() {
        let request = smart_request(AtaRead::SmartData);
        assert_eq!(request.len(), 33, "SENDCMDINPARAMS is packed");
        assert_eq!(request[..4], 512u32.to_le_bytes());
        // Features, count, number, LBA mid, LBA high, device, command.
        assert_eq!(request[4..11], [0xD0, 1, 1, 0x4F, 0xC2, 0xA0, 0xB0]);
        assert_eq!(smart_request(AtaRead::SmartThresholds)[4], 0xD1);
        assert_eq!(
            smart_request(AtaRead::Identify)[4..11],
            [0, 1, 1, 0, 0, 0xA0, 0xEC]
        );
    }

    #[test]
    fn usb_adapter_requests_are_ata_pass_through_reads() {
        let cdb = sat_cdb(AtaRead::SmartData);
        assert_eq!(
            cdb,
            [
                0x85, 0x08, 0x0E, 0, 0xD0, 0, 1, 0, 1, 0, 0x4F, 0, 0xC2, 0xA0, 0xB0, 0
            ]
        );
        let (request, data) = sat_request(AtaRead::Identify);
        assert_eq!(request.len(), data + 512);
        let cdb_at = offset_of!(SCSI_PASS_THROUGH, Cdb);
        assert_eq!(request[cdb_at + 14], 0xEC);
        assert_eq!(request[offset_of!(SCSI_PASS_THROUGH, DataIn)], 1, "data in");
        let offset_at = offset_of!(SCSI_PASS_THROUGH, DataBufferOffset);
        assert_eq!(
            request[offset_at..offset_at + 8],
            (data as u64).to_le_bytes()
        );
    }

    #[test]
    fn nvme_requests_ask_for_the_health_log() {
        let request = nvme_request(StorageDeviceProtocolSpecificProperty);
        assert_eq!(request.len(), 8 + 40 + 512);
        let u32_at = |at: usize| u32::from_le_bytes(request[at..at + 4].try_into().unwrap());
        assert_eq!(u32_at(0), 50, "StorageDeviceProtocolSpecificProperty");
        assert_eq!(u32_at(8), 3, "NVMe");
        assert_eq!(u32_at(12), 2, "log page");
        assert_eq!(u32_at(16), 2, "health log");
        assert_eq!(u32_at(24), 40, "data after the request");
        assert_eq!(u32_at(28), 512);
    }

    #[test]
    fn smart_pages_and_identify_flags() {
        let mut page = vec![0u8; 512];
        assert!(!has_attributes(&page));
        page[2 + 12 * 3] = 5;
        assert!(has_attributes(&page));
        assert!(!has_attributes(&page[..100]));
        let mut identify = vec![0u8; 512];
        identify[164] = 1; // word 82: supported
        assert!(smart_turned_off(&identify));
        identify[170] = 1; // word 85: turned on
        assert!(!smart_turned_off(&identify));
    }

    #[test]
    fn reads_c_strings_from_replies() {
        let reply = b"\0\0\0\0Samsung \0SSD 980\0";
        assert_eq!(c_string(reply, 4).as_deref(), Some("Samsung"));
        assert_eq!(c_string(reply, 13).as_deref(), Some("SSD 980"));
        assert_eq!(c_string(reply, 0), None);
        assert_eq!(c_string(reply, 99), None);
    }
}
