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
    CreateFileW, FILE_FLAG_NO_BUFFERING, FILE_FLAG_WRITE_THROUGH, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    GetDiskFreeSpaceExW, GetVolumeInformationW, GetVolumePathNamesForVolumeNameW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DISK_EXTENT, DISK_GEOMETRY, DISK_GEOMETRY_EX, FSCTL_ALLOW_EXTENDED_DASD_IO,
    FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_SET_SPARSE, GET_LENGTH_INFORMATION,
    IOCTL_DISK_GET_DRIVE_GEOMETRY, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_DISK_GET_LENGTH_INFO,
    IOCTL_DISK_UPDATE_PROPERTIES, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_DEVICE_DESCRIPTOR, StorageAccessAlignmentProperty,
    StorageDeviceProperty, VOLUME_DISK_EXTENTS,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::devices::{DeviceList, DiskExtent, PhysicalDiskInfo, VolumeInfo};
use crate::error::{Error, Result};
use crate::source::FileSource;
use crate::spec::SourceSpec;
use crate::target::OpenedDisk;

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
    let mut locks = Vec::new();
    for volume in volumes {
        let volume_name = SourceSpec::Volume(volume.clone()).display_name();
        let file = create_file(volume, GENERIC_READ | GENERIC_WRITE, 0)
            .map_err(|e| Error::from_io(&volume_name, e))?;
        if ioctl(&file, FSCTL_LOCK_VOLUME, &[], &mut []).is_err() {
            return Err(Error::Refused {
                reason: format!(
                    "{volume_name} on {name} is in use. Close the programs and windows using \
                     it, then try again."
                ),
            });
        }
        ioctl(&file, FSCTL_DISMOUNT_VOLUME, &[], &mut [])
            .map_err(|e| Error::from_io(&volume_name, e))?;
        locks.push(file);
    }
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
    })
}

struct Descriptor {
    model: String,
    serial: Option<String>,
    bus: String,
    removable: bool,
}

fn device_descriptor(file: &File) -> Descriptor {
    let unknown = Descriptor {
        model: "Unknown disk".to_owned(),
        serial: None,
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
    let size = if extents.is_empty() {
        total_size(&root)
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

fn total_size(root: &[u16]) -> Option<u64> {
    let mut total = 0u64;
    // SAFETY: `root` is NUL-terminated; `total` is a valid output; the other
    // outputs are null, which is allowed.
    let ok =
        unsafe { GetDiskFreeSpaceExW(root.as_ptr(), ptr::null_mut(), &mut total, ptr::null_mut()) };
    (ok != 0).then_some(total)
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
    fn reads_c_strings_from_replies() {
        let reply = b"\0\0\0\0Samsung \0SSD 980\0";
        assert_eq!(c_string(reply, 4).as_deref(), Some("Samsung"));
        assert_eq!(c_string(reply, 13).as_deref(), Some("SSD 980"));
        assert_eq!(c_string(reply, 0), None);
        assert_eq!(c_string(reply, 99), None);
    }
}
