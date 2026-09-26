//! Physical disk identity from the storage stack, without elevation.
//!
//! `\\.\PhysicalDriveN` and `\\.\C:` are opened with zero access rights, which
//! permits the `FILE_ANY_ACCESS` queries used here and nothing else: device
//! descriptor (vendor/product/revision, bus, removable), seek penalty (SSD vs
//! rotational), TRIM, capacity and volume extents. The device serial number is
//! never read out of the descriptor. Results are cached by the caller and only
//! re-queried when the set of physical disks changes.
//! https://learn.microsoft.com/windows/win32/api/winioctl/ni-winioctl-ioctl_storage_query_property

use std::collections::BTreeSet;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
    StorageDeviceTrimProperty, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_QUERY_PROPERTY,
    STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_MULTI_SZ};
use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows_sys::Win32::System::IO::DeviceIoControl;

const DESCRIPTOR_BYTES: usize = 4096;

/// `STORAGE_BUS_TYPE` as reported by the port driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageBus {
    Scsi,
    Atapi,
    Ata,
    Ieee1394,
    Ssa,
    Fibre,
    Usb,
    Raid,
    Iscsi,
    Sas,
    Sata,
    Sd,
    Mmc,
    Virtual,
    FileBackedVirtual,
    Spaces,
    Nvme,
    Scm,
    Ufs,
    NvmeOverFabrics,
    /// A value this build does not name (includes 0, `BusTypeUnknown`).
    Other(u32),
}

impl StorageBus {
    pub fn from_raw(value: u32) -> Self {
        match value {
            1 => Self::Scsi,
            2 => Self::Atapi,
            3 => Self::Ata,
            4 => Self::Ieee1394,
            5 => Self::Ssa,
            6 => Self::Fibre,
            7 => Self::Usb,
            8 => Self::Raid,
            9 => Self::Iscsi,
            10 => Self::Sas,
            11 => Self::Sata,
            12 => Self::Sd,
            13 => Self::Mmc,
            14 => Self::Virtual,
            15 => Self::FileBackedVirtual,
            16 => Self::Spaces,
            17 => Self::Nvme,
            18 => Self::Scm,
            19 => Self::Ufs,
            20 => Self::NvmeOverFabrics,
            other => Self::Other(other),
        }
    }

    /// Short display label, e.g. "NVMe", "SATA", "USB".
    pub fn label(self) -> String {
        match self {
            Self::Scsi => "SCSI".into(),
            Self::Atapi => "ATAPI".into(),
            Self::Ata => "ATA".into(),
            Self::Ieee1394 => "IEEE 1394".into(),
            Self::Ssa => "SSA".into(),
            Self::Fibre => "Fibre Channel".into(),
            Self::Usb => "USB".into(),
            Self::Raid => "RAID".into(),
            Self::Iscsi => "iSCSI".into(),
            Self::Sas => "SAS".into(),
            Self::Sata => "SATA".into(),
            Self::Sd => "SD".into(),
            Self::Mmc => "MMC".into(),
            Self::Virtual => "Virtual".into(),
            Self::FileBackedVirtual => "File-backed virtual".into(),
            Self::Spaces => "Storage Spaces".into(),
            Self::Nvme => "NVMe".into(),
            Self::Scm => "SCM".into(),
            Self::Ufs => "UFS".into(),
            Self::NvmeOverFabrics => "NVMe-oF".into(),
            Self::Other(value) => format!("Bus type {value}"),
        }
    }
}

/// Static facts about one physical disk. `None` means the device declined or
/// does not implement that query.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StorageDevice {
    /// Windows disk number: the `N` in `\\.\PhysicalDriveN` and in the PDH
    /// `PhysicalDisk` instance `N C: D:`.
    pub disk_number: u32,
    /// Display model built from the descriptor's vendor and product IDs (see
    /// [`model_name`]), e.g. "Samsung SSD 970 EVO Plus 2TB".
    pub model: Option<String>,
    pub vendor_id: Option<String>,
    pub product_id: Option<String>,
    /// Firmware revision string from the descriptor.
    pub firmware_revision: Option<String>,
    pub bus: Option<StorageBus>,
    /// The device reports removable media (e.g. a USB flash drive).
    pub removable: Option<bool>,
    /// `true` for rotational media. `Some(false)` ("no seek penalty") is how
    /// Windows classifies SSDs for Optimize Drives and Task Manager.
    pub seek_penalty: Option<bool>,
    pub trim_enabled: Option<bool>,
    /// Capacity in bytes (`IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` `DiskSize`).
    pub capacity_bytes: Option<u64>,
    /// Holds (part of) the volume Windows was booted from.
    pub system_disk: Option<bool>,
    /// Holds (part of) a volume with an active page file.
    pub page_file: Option<bool>,
}

impl StorageDevice {
    /// "SSD"/"HDD" from the seek-penalty flag, when the device reports it.
    pub fn media_label(&self) -> Option<&'static str> {
        self.seek_penalty
            .map(|penalty| if penalty { "HDD" } else { "SSD" })
    }
}

/// Parses the leading disk number of a PDH `PhysicalDisk` instance (`"2 D: E:"`).
pub fn disk_number(pdh_instance: &str) -> Option<u32> {
    pdh_instance
        .split_whitespace()
        .next()
        .and_then(|number| number.parse().ok())
}

/// Queries the given disks and marks the system and page-file disks.
/// Individual query failures are recorded as `None`; a disk that cannot be
/// opened at all is still returned so the caller does not retry every sample.
pub fn describe(disks: &BTreeSet<u32>) -> (Vec<StorageDevice>, Vec<String>) {
    let mut warnings = Vec::new();
    let system = system_disks();
    let paging = page_file_disks();
    if let Err(error) = &system {
        warnings.push(format!("System disk: {error}"));
    }
    if let Err(error) = &paging {
        warnings.push(format!("Page file disks: {error}"));
    }
    let devices = disks
        .iter()
        .map(|&number| {
            let mut device = query_disk(number).unwrap_or_else(|error| {
                warnings.push(format!("Disk {number}: {error}"));
                StorageDevice {
                    disk_number: number,
                    ..Default::default()
                }
            });
            device.system_disk = system.as_ref().ok().map(|set| set.contains(&number));
            device.page_file = paging.as_ref().ok().map(|set| set.contains(&number));
            device
        })
        .collect();
    (devices, warnings)
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Opens a device path with no access rights (metadata queries only).
fn open_device(path: &str) -> Result<Handle, String> {
    let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(Handle(handle))
}

/// Runs an IOCTL into an 8-byte aligned buffer and returns the valid bytes.
fn ioctl(handle: &Handle, code: u32, input: &[u8], capacity: usize) -> Result<Vec<u8>, String> {
    let mut buffer = vec![0u64; capacity.div_ceil(8)];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle.0,
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 8) as u32,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let bytes: Vec<u8> = buffer.iter().flat_map(|word| word.to_le_bytes()).collect();
    Ok(bytes[..(returned as usize).min(bytes.len())].to_vec())
}

fn property_query(property: i32) -> Vec<u8> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: property,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut bytes = Vec::with_capacity(12);
    bytes.extend_from_slice(&query.PropertyId.to_le_bytes());
    bytes.extend_from_slice(&query.QueryType.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes
}

fn query_disk(number: u32) -> Result<StorageDevice, String> {
    let handle = open_device(&format!(r"\\.\PhysicalDrive{number}"))?;
    let mut device = StorageDevice {
        disk_number: number,
        ..Default::default()
    };
    if let Ok(bytes) = ioctl(
        &handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &property_query(StorageDeviceProperty),
        DESCRIPTOR_BYTES,
    ) {
        if let Some(descriptor) = parse_device_descriptor(&bytes) {
            device.model = model_name(
                descriptor.vendor_id.as_deref(),
                descriptor.product_id.as_deref(),
            );
            device.vendor_id = descriptor.vendor_id;
            device.product_id = descriptor.product_id;
            device.firmware_revision = descriptor.revision;
            device.bus = Some(StorageBus::from_raw(descriptor.bus_type));
            device.removable = Some(descriptor.removable);
        }
    }
    if let Ok(bytes) = ioctl(
        &handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &property_query(StorageDeviceSeekPenaltyProperty),
        64,
    ) {
        device.seek_penalty = parse_boolean_descriptor(&bytes);
    }
    if let Ok(bytes) = ioctl(
        &handle,
        IOCTL_STORAGE_QUERY_PROPERTY,
        &property_query(StorageDeviceTrimProperty),
        64,
    ) {
        device.trim_enabled = parse_boolean_descriptor(&bytes);
    }
    if let Ok(bytes) = ioctl(&handle, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, &[], 256) {
        device.capacity_bytes = parse_geometry_size(&bytes);
    }
    Ok(device)
}

#[derive(Debug, Default, PartialEq)]
struct DeviceDescriptor {
    removable: bool,
    bus_type: u32,
    vendor_id: Option<String>,
    product_id: Option<String>,
    revision: Option<String>,
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Reads a NUL-terminated ASCII string at `offset` inside the valid bytes.
/// Offset zero means "not present". Serial numbers are never requested.
fn descriptor_string(bytes: &[u8], offset: u32) -> Option<String> {
    let offset = offset as usize;
    if offset == 0 {
        return None;
    }
    let tail = bytes.get(offset..)?;
    let end = tail.iter().position(|&byte| byte == 0)?;
    let text: String = tail[..end]
        .iter()
        .map(|&byte| {
            if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                ' '
            }
        })
        .collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// Parses a `STORAGE_DEVICE_DESCRIPTOR` without trusting any offset.
fn parse_device_descriptor(bytes: &[u8]) -> Option<DeviceDescriptor> {
    let size = u32_at(bytes, 4)? as usize;
    // Offsets are relative to the structure start and must stay within the
    // bytes both claimed by the structure and actually returned.
    let bytes = bytes.get(..size.min(bytes.len()))?;
    if bytes.len() < 36 {
        return None;
    }
    Some(DeviceDescriptor {
        removable: bytes[10] != 0,
        bus_type: u32_at(bytes, 28)?,
        vendor_id: descriptor_string(bytes, u32_at(bytes, 12)?),
        product_id: descriptor_string(bytes, u32_at(bytes, 16)?),
        revision: descriptor_string(bytes, u32_at(bytes, 20)?),
    })
}

/// `DEVICE_SEEK_PENALTY_DESCRIPTOR` / `DEVICE_TRIM_DESCRIPTOR`: two DWORDs and
/// one BOOLEAN.
fn parse_boolean_descriptor(bytes: &[u8]) -> Option<bool> {
    let size = u32_at(bytes, 4)? as usize;
    if size < 9 || bytes.len() < 9 {
        return None;
    }
    Some(bytes[8] != 0)
}

/// `DISK_GEOMETRY_EX.DiskSize` follows the 24-byte `DISK_GEOMETRY`.
fn parse_geometry_size(bytes: &[u8]) -> Option<u64> {
    let size = i64::from_le_bytes(bytes.get(24..32)?.try_into().ok()?);
    u64::try_from(size).ok().filter(|&size| size > 0)
}

/// Disk numbers from `VOLUME_DISK_EXTENTS`.
fn parse_disk_extents(bytes: &[u8]) -> Option<BTreeSet<u32>> {
    let count = u32_at(bytes, 0)? as usize;
    let mut disks = BTreeSet::new();
    for index in 0..count.min(1024) {
        let offset = 8usize.checked_add(index.checked_mul(24)?)?;
        disks.insert(u32_at(bytes, offset)?);
    }
    Some(disks)
}

/// Combines descriptor vendor and product IDs into a model name.
///
/// SCSI/ATA translation layers fill the 8-character vendor field with the
/// placeholders "ATA" or "NVMe" rather than a manufacturer; those are dropped.
/// The vendor is also dropped when the product already starts with it.
pub fn model_name(vendor: Option<&str>, product: Option<&str>) -> Option<String> {
    let vendor = vendor
        .map(str::trim)
        .filter(|vendor| !vendor.is_empty())
        .filter(|vendor| {
            !vendor.eq_ignore_ascii_case("ATA") && !vendor.eq_ignore_ascii_case("NVMe")
        });
    let product = product.map(str::trim).filter(|product| !product.is_empty());
    match (vendor, product) {
        (Some(vendor), Some(product))
            if product
                .to_ascii_lowercase()
                .starts_with(&vendor.to_ascii_lowercase()) =>
        {
            Some(product.to_owned())
        }
        (Some(vendor), Some(product)) => Some(format!("{vendor} {product}")),
        (None, Some(product)) => Some(product.to_owned()),
        (Some(vendor), None) => Some(vendor.to_owned()),
        (None, None) => None,
    }
}

fn volume_disks(volume: &str) -> Result<BTreeSet<u32>, String> {
    let handle = open_device(&format!(r"\\.\{volume}"))?;
    let bytes = ioctl(
        &handle,
        IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
        &[],
        8 + 24 * 32,
    )?;
    parse_disk_extents(&bytes).ok_or_else(|| "invalid volume extents".into())
}

fn system_disks() -> Result<BTreeSet<u32>, String> {
    let mut buffer = [0u16; 260];
    let length = unsafe { GetSystemWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 || length as usize >= buffer.len() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let directory = String::from_utf16_lossy(&buffer[..length as usize]);
    let volume = drive_volume(&directory).ok_or("Windows directory has no drive letter")?;
    volume_disks(&volume)
}

/// `X:` for a Win32 or NT (`\??\X:\…`) path on a drive letter.
fn drive_volume(path: &str) -> Option<String> {
    let path = path.strip_prefix(r"\??\").unwrap_or(path);
    let mut chars = path.chars();
    let letter = chars.next()?;
    (letter.is_ascii_alphabetic() && chars.next() == Some(':')).then(|| format!("{letter}:"))
}

/// Active page files from `ExistingPageFiles` (NT paths like `\??\C:\pagefile.sys`).
fn page_file_disks() -> Result<BTreeSet<u32>, String> {
    let key: Vec<u16> = r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let value: Vec<u16> = "ExistingPageFiles".encode_utf16().chain(Some(0)).collect();
    let mut buffer = vec![0u16; 2048];
    let mut bytes = (buffer.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_MULTI_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status == 2 {
        // No page file is configured.
        return Ok(BTreeSet::new());
    }
    if status != 0 || bytes as usize > buffer.len() * 2 {
        return Err(format!("unavailable (Windows {status})"));
    }
    let units = &buffer[..bytes as usize / 2];
    let mut disks = BTreeSet::new();
    for path in units
        .split(|&unit| unit == 0)
        .filter(|path| !path.is_empty())
    {
        let path = String::from_utf16_lossy(path);
        if let Some(volume) = drive_volume(&path) {
            disks.extend(volume_disks(&volume)?);
        }
    }
    Ok(disks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(vendor: &str, product: &str, revision: &str, bus: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 40];
        let push = |bytes: &mut Vec<u8>, text: &str| {
            if text.is_empty() {
                return 0u32;
            }
            let offset = bytes.len() as u32;
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0);
            offset
        };
        let vendor = push(&mut bytes, vendor);
        let product = push(&mut bytes, product);
        let revision = push(&mut bytes, revision);
        // A serial number is present in real descriptors; it must be ignored.
        let serial = push(&mut bytes, "SERIAL123");
        let size = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        bytes[10] = 1;
        bytes[12..16].copy_from_slice(&vendor.to_le_bytes());
        bytes[16..20].copy_from_slice(&product.to_le_bytes());
        bytes[20..24].copy_from_slice(&revision.to_le_bytes());
        bytes[24..28].copy_from_slice(&serial.to_le_bytes());
        bytes[28..32].copy_from_slice(&bus.to_le_bytes());
        bytes
    }

    #[test]
    fn device_descriptor_reads_ids_and_never_the_serial() {
        let bytes = descriptor("NVMe    ", "Samsung SSD 970 EVO Plus 2TB", "2B2QEXM7", 17);
        let parsed = parse_device_descriptor(&bytes).unwrap();
        assert_eq!(parsed.vendor_id.as_deref(), Some("NVMe"));
        assert_eq!(
            parsed.product_id.as_deref(),
            Some("Samsung SSD 970 EVO Plus 2TB")
        );
        assert_eq!(parsed.revision.as_deref(), Some("2B2QEXM7"));
        assert_eq!(StorageBus::from_raw(parsed.bus_type), StorageBus::Nvme);
        assert!(parsed.removable);
        assert!(!format!("{parsed:?}").contains("SERIAL"));
    }

    #[test]
    fn device_descriptor_rejects_truncation_and_out_of_range_offsets() {
        let bytes = descriptor("SanDisk", "Cruzer Blade", "1.00", 7);
        assert!(parse_device_descriptor(&bytes[..20]).is_none());
        assert!(parse_device_descriptor(&[]).is_none());
        // Offsets past the reported size and strings without a terminator.
        let mut bad = bytes.clone();
        bad[16..20].copy_from_slice(&10_000u32.to_le_bytes());
        assert_eq!(parse_device_descriptor(&bad).unwrap().product_id, None);
        let mut unterminated = bytes.clone();
        let end = unterminated.len();
        unterminated[end - 1] = b'X';
        let size = (end - 5) as u32;
        unterminated[4..8].copy_from_slice(&size.to_le_bytes());
        let parsed = parse_device_descriptor(&unterminated).unwrap();
        assert_eq!(parsed.vendor_id.as_deref(), Some("SanDisk"));
        // A claimed size larger than the returned bytes is clamped.
        let mut large = bytes;
        large[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_device_descriptor(&large).is_some());
    }

    #[test]
    fn models_drop_translation_placeholders_and_duplicate_vendors() {
        let model = |v: Option<&str>, p: Option<&str>| model_name(v, p);
        assert_eq!(
            model(Some("NVMe"), Some("Samsung SSD 990 PRO 2TB")).as_deref(),
            Some("Samsung SSD 990 PRO 2TB")
        );
        assert_eq!(
            model(Some("ATA"), Some("ST4000DM004-2CV104")).as_deref(),
            Some("ST4000DM004-2CV104")
        );
        assert_eq!(
            model(Some("SanDisk"), Some("Cruzer Blade")).as_deref(),
            Some("SanDisk Cruzer Blade")
        );
        assert_eq!(
            model(Some("Samsung"), Some("SAMSUNG SSD 860")).as_deref(),
            Some("SAMSUNG SSD 860")
        );
        assert_eq!(
            model(None, Some("SHPP51-2000GM")).as_deref(),
            Some("SHPP51-2000GM")
        );
        assert_eq!(model(Some("WD"), None).as_deref(), Some("WD"));
        assert_eq!(model(Some(" "), Some("")), None);
    }

    #[test]
    fn small_descriptors_are_bounds_checked() {
        let mut penalty = vec![0u8; 12];
        penalty[4..8].copy_from_slice(&12u32.to_le_bytes());
        penalty[8] = 1;
        assert_eq!(parse_boolean_descriptor(&penalty), Some(true));
        assert_eq!(parse_boolean_descriptor(&penalty[..8]), None);
        let mut geometry = vec![0u8; 32];
        geometry[24..32].copy_from_slice(&2_000_398_934_016i64.to_le_bytes());
        assert_eq!(parse_geometry_size(&geometry), Some(2_000_398_934_016));
        assert_eq!(parse_geometry_size(&geometry[..31]), None);
        geometry[24..32].copy_from_slice(&(-1i64).to_le_bytes());
        assert_eq!(parse_geometry_size(&geometry), None);
        let mut extents = vec![0u8; 8 + 48];
        extents[0..4].copy_from_slice(&2u32.to_le_bytes());
        extents[8..12].copy_from_slice(&1u32.to_le_bytes());
        extents[32..36].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(parse_disk_extents(&extents), Some(BTreeSet::from([1, 3])));
        extents[0..4].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(parse_disk_extents(&extents), None);
    }

    #[test]
    fn disk_numbers_and_volumes_parse_only_valid_forms() {
        assert_eq!(disk_number("0 C: D:"), Some(0));
        assert_eq!(disk_number("12"), Some(12));
        assert_eq!(disk_number("_Total"), None);
        assert_eq!(disk_number(""), None);
        assert_eq!(drive_volume(r"\??\C:\pagefile.sys").as_deref(), Some("C:"));
        assert_eq!(drive_volume(r"D:\Windows").as_deref(), Some("D:"));
        assert_eq!(drive_volume(r"\??\Volume{1}\pagefile.sys"), None);
        assert_eq!(drive_volume(""), None);
    }
}
