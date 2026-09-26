//! Installed memory modules from the firmware SMBIOS table.
//!
//! `GetSystemFirmwareTable('RSMB')` returns the raw SMBIOS structure table
//! without elevation or WMI. Only type 16 (Physical Memory Array) and type 17
//! (Memory Device) are decoded. Every read is bounds-checked against the
//! returned buffer, so a truncated or malformed table yields fewer structures,
//! never a panic or an out-of-range read. Serial numbers and asset tags are
//! never decoded.
//! https://www.dmtf.org/standards/smbios (DSP0134)

use windows_sys::Win32::System::SystemInformation::GetSystemFirmwareTable;

const RSMB: u32 = u32::from_be_bytes(*b"RSMB");
const MAX_TABLE_BYTES: u32 = 4 * 1024 * 1024;
/// Type 16 "Use" value for system memory (as opposed to video, flash, cache).
const USE_SYSTEM_MEMORY: u8 = 3;
/// A type 17 array handle of 0xFFFE means "not provided".
const HANDLE_NOT_PROVIDED: u16 = 0xfffe;

/// One populated memory device (type 17 with a non-zero size).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryModule {
    /// Slot label printed on the board, e.g. "DIMM 1".
    pub device_locator: Option<String>,
    /// Bank/channel label, e.g. "P0 CHANNEL A".
    pub bank_locator: Option<String>,
    pub size_bytes: Option<u64>,
    /// SMBIOS form factor code (see [`form_factor_name`]).
    pub form_factor: u8,
    /// SMBIOS memory type code (see [`memory_type_name`]).
    pub memory_type: u8,
    /// Maximum rated speed in MT/s. `None` when not reported, or when the
    /// table's specification does not define the field in MT/s (see
    /// [`speeds_are_mts`]); the firmware's value is then only in
    /// `speed_reported`.
    pub speed_mts: Option<u32>,
    /// Speed the memory controller configured, in MT/s (SMBIOS 2.7+), under
    /// the same rule as `speed_mts`.
    pub configured_speed_mts: Option<u32>,
    /// The Speed field exactly as the firmware reported it, whatever the
    /// table version. SMBIOS 3.1.1 defines it in MT/s; earlier specifications
    /// say MHz, and firmware of that era reported either the transfer rate or
    /// the clock, so the unit of an older value is unknown.
    pub speed_reported: Option<u32>,
    /// The Configured Memory Speed field exactly as reported (see
    /// `speed_reported`).
    pub configured_speed_reported: Option<u32>,
    pub manufacturer: Option<String>,
    pub part_number: Option<String>,
    /// Configured voltage in millivolts (SMBIOS 2.8+).
    pub configured_voltage_mv: Option<u16>,
}

/// System memory layout decoded from SMBIOS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryInventory {
    pub smbios_version: (u8, u8),
    /// Sum of "Number of Memory Devices" over system-memory arrays (type 16).
    /// `None` when the firmware lists no system-memory array.
    pub slots_total: Option<u32>,
    /// Type 17 devices with a non-zero size that belong to system memory.
    pub slots_used: u32,
    /// Maximum capacity the arrays support, in bytes.
    pub max_capacity_bytes: Option<u64>,
    pub modules: Vec<MemoryModule>,
}

impl MemoryInventory {
    /// The configured speed shared by every populated module; `None` when a
    /// module does not report it or modules disagree.
    pub fn configured_speed_mts(&self) -> Option<u32> {
        common(self.modules.iter().map(|m| m.configured_speed_mts))
    }

    /// The memory type shared by every populated module, e.g. "DDR5".
    pub fn memory_type(&self) -> Option<&'static str> {
        common(self.modules.iter().map(|m| memory_type_name(m.memory_type)))
    }

    /// The form factor shared by every populated module, e.g. "DIMM".
    pub fn form_factor(&self) -> Option<&'static str> {
        common(self.modules.iter().map(|m| form_factor_name(m.form_factor)))
    }
}

/// Whether a table's type 17 speed fields are defined in MT/s. SMBIOS 3.1.1
/// changed them from MHz. A 3.1 table cannot be told apart from 3.1.0 by
/// major.minor (Windows' `RawSMBIOSData` carries no document revision), so
/// only 3.2 and later qualify.
pub fn speeds_are_mts(version: (u8, u8)) -> bool {
    version >= (3, 2)
}

fn common<T: PartialEq + Copy>(mut values: impl Iterator<Item = Option<T>>) -> Option<T> {
    let first = values.next()??;
    values.all(|value| value == Some(first)).then_some(first)
}

/// SMBIOS 3.7 memory type names; unknown/other codes are `None`.
pub fn memory_type_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x03 => "DRAM",
        0x04 => "EDRAM",
        0x05 => "VRAM",
        0x06 => "SRAM",
        0x07 => "RAM",
        0x08 => "ROM",
        0x09 => "Flash",
        0x0A => "EEPROM",
        0x0B => "FEPROM",
        0x0C => "EPROM",
        0x0D => "CDRAM",
        0x0E => "3DRAM",
        0x0F => "SDRAM",
        0x10 => "SGRAM",
        0x11 => "RDRAM",
        0x12 => "DDR",
        0x13 => "DDR2",
        0x14 => "DDR2 FB-DIMM",
        0x18 => "DDR3",
        0x19 => "FBD2",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x1F => "Logical non-volatile device",
        0x20 => "HBM",
        0x21 => "HBM2",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        0x24 => "HBM3",
        _ => return None,
    })
}

/// SMBIOS 3.7 form factor names; unknown/other codes are `None`.
pub fn form_factor_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0x03 => "SIMM",
        0x04 => "SIP",
        0x05 => "Chip",
        0x06 => "DIP",
        0x07 => "ZIP",
        0x08 => "Proprietary card",
        0x09 => "DIMM",
        0x0A => "TSOP",
        0x0B => "Row of chips",
        0x0C => "RIMM",
        0x0D => "SODIMM",
        0x0E => "SRIMM",
        0x0F => "FB-DIMM",
        0x10 => "Die",
        0x11 => "CAMM",
        _ => return None,
    })
}

/// Reads and decodes the live SMBIOS table.
pub fn memory_inventory() -> Result<MemoryInventory, String> {
    let size = unsafe { GetSystemFirmwareTable(RSMB, 0, std::ptr::null_mut(), 0) };
    if size == 0 || size > MAX_TABLE_BYTES {
        return Err(format!(
            "SMBIOS table unavailable: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut buffer = vec![0u8; size as usize];
    let written = unsafe { GetSystemFirmwareTable(RSMB, 0, buffer.as_mut_ptr(), size) };
    if written == 0 || written > size {
        return Err("SMBIOS table changed while reading".into());
    }
    buffer.truncate(written as usize);
    parse_raw(&buffer).ok_or_else(|| "SMBIOS table is malformed".into())
}

/// Parses the `RawSMBIOSData` wrapper returned by `GetSystemFirmwareTable`.
pub fn parse_raw(raw: &[u8]) -> Option<MemoryInventory> {
    let major = *raw.get(1)?;
    let minor = *raw.get(2)?;
    let length = u32::from_le_bytes(raw.get(4..8)?.try_into().ok()?) as usize;
    let table = raw.get(8..)?;
    let table = table.get(..length.min(table.len()))?;
    Some(parse_table(table, (major, minor)))
}

struct Structure<'a> {
    kind: u8,
    handle: u16,
    formatted: &'a [u8],
    strings: Vec<&'a [u8]>,
}

impl Structure<'_> {
    fn byte(&self, offset: usize) -> Option<u8> {
        self.formatted.get(offset).copied()
    }
    fn word(&self, offset: usize) -> Option<u16> {
        Some(u16::from_le_bytes(
            self.formatted.get(offset..offset + 2)?.try_into().ok()?,
        ))
    }
    fn dword(&self, offset: usize) -> Option<u32> {
        Some(u32::from_le_bytes(
            self.formatted.get(offset..offset + 4)?.try_into().ok()?,
        ))
    }
    fn qword(&self, offset: usize) -> Option<u64> {
        Some(u64::from_le_bytes(
            self.formatted.get(offset..offset + 8)?.try_into().ok()?,
        ))
    }
    /// SMBIOS strings are 1-based; 0 means "no string".
    fn string(&self, offset: usize) -> Option<String> {
        let index = self.byte(offset)? as usize;
        let bytes = self.strings.get(index.checked_sub(1)?)?;
        let text: String = bytes
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    ' '
                }
            })
            .collect();
        let text = text.trim();
        (!text.is_empty() && !is_placeholder(text)).then(|| text.to_owned())
    }
}

/// Firmware placeholder text that carries no information.
fn is_placeholder(text: &str) -> bool {
    [
        "unknown",
        "not specified",
        "to be filled by o.e.m.",
        "none",
        "undefined",
    ]
    .iter()
    .any(|placeholder| text.eq_ignore_ascii_case(placeholder))
}

/// Splits the table into structures. Stops at the end-of-table marker (type
/// 127) or at the first structure that does not fit the buffer.
fn structures(mut table: &[u8]) -> Vec<Structure<'_>> {
    let mut result = Vec::new();
    while table.len() >= 4 {
        let kind = table[0];
        let length = table[1] as usize;
        if length < 4 || length > table.len() {
            break;
        }
        let handle = u16::from_le_bytes([table[2], table[3]]);
        let formatted = &table[..length];
        let rest = &table[length..];
        // The string set ends with a double NUL (two NULs even when empty).
        let Some(end) = rest.windows(2).position(|pair| pair == [0, 0]) else {
            break;
        };
        let strings = rest[..end]
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .collect();
        result.push(Structure {
            kind,
            handle,
            formatted,
            strings,
        });
        table = &rest[end + 2..];
        if kind == 127 {
            break;
        }
    }
    result
}

fn parse_table(table: &[u8], version: (u8, u8)) -> MemoryInventory {
    let structures = structures(table);
    let mut inventory = MemoryInventory {
        smbios_version: version,
        ..Default::default()
    };
    let mts = speeds_are_mts(version);
    let mut system_arrays = Vec::new();
    let mut other_arrays = Vec::new();
    for array in structures.iter().filter(|s| s.kind == 16) {
        if array.byte(0x05) != Some(USE_SYSTEM_MEMORY) {
            other_arrays.push(array.handle);
            continue;
        }
        system_arrays.push(array.handle);
        if let Some(devices) = array.word(0x0D) {
            *inventory.slots_total.get_or_insert(0) += u32::from(devices);
        }
        let capacity = match array.dword(0x07) {
            Some(0x8000_0000) => array.qword(0x0F),
            Some(kib) => Some(u64::from(kib) * 1024),
            None => None,
        };
        if let Some(capacity) = capacity.filter(|&bytes| bytes > 0) {
            let total = inventory.max_capacity_bytes.get_or_insert(0);
            *total = total.saturating_add(capacity);
        }
    }
    for device in structures.iter().filter(|s| s.kind == 17) {
        let array = device.word(0x04).unwrap_or(HANDLE_NOT_PROVIDED);
        if other_arrays.contains(&array) && !system_arrays.contains(&array) {
            continue;
        }
        let Some(size) = device_size(device) else {
            continue;
        };
        if size == 0 {
            continue;
        }
        inventory.slots_used += 1;
        let speed_reported = speed(device, 0x15, 0x54);
        let configured_speed_reported = speed(device, 0x20, 0x58);
        inventory.modules.push(MemoryModule {
            device_locator: device.string(0x10),
            bank_locator: device.string(0x11),
            size_bytes: (size != u64::MAX).then_some(size),
            form_factor: device.byte(0x0E).unwrap_or(0),
            memory_type: device.byte(0x12).unwrap_or(0),
            speed_mts: speed_reported.filter(|_| mts),
            configured_speed_mts: configured_speed_reported.filter(|_| mts),
            speed_reported,
            configured_speed_reported,
            manufacturer: device.string(0x17),
            part_number: device.string(0x1A),
            configured_voltage_mv: device.word(0x26).filter(|&mv| mv > 0),
        });
    }
    inventory
}

/// Device size in bytes; `Some(0)` for an empty slot, `Some(u64::MAX)` for a
/// populated slot of unknown size, `None` when the field is missing.
fn device_size(device: &Structure<'_>) -> Option<u64> {
    match device.word(0x0C)? {
        0 => Some(0),
        0xFFFF => Some(u64::MAX),
        0x7FFF => device
            .dword(0x1C)
            .map(|mib| u64::from(mib & 0x7FFF_FFFF) * 1024 * 1024)
            .or(Some(u64::MAX)),
        value if value & 0x8000 != 0 => Some(u64::from(value & 0x7FFF) * 1024),
        value => Some(u64::from(value) * 1024 * 1024),
    }
}

/// A 16-bit speed with its 32-bit extension (used when the word is 0xFFFF).
fn speed(device: &Structure<'_>, word: usize, extended: usize) -> Option<u32> {
    match device.word(word)? {
        0 => None,
        0xFFFF => device
            .dword(extended)
            .map(|value| value & 0x7FFF_FFFF)
            .filter(|&value| value > 0),
        value => Some(u32::from(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Index into a formatted area that excludes the 4-byte header.
    fn field(offset: usize) -> usize {
        offset - 4
    }

    fn structure(kind: u8, handle: u16, formatted: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut bytes = vec![kind, (formatted.len() + 4) as u8];
        bytes.extend_from_slice(&handle.to_le_bytes());
        bytes.extend_from_slice(formatted);
        for text in strings {
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0);
        }
        if strings.is_empty() {
            bytes.push(0);
        }
        bytes.push(0);
        bytes
    }

    fn array(handle: u16, usage: u8, devices: u16) -> Vec<u8> {
        // Offsets are relative to the structure start; the header is 4 bytes.
        let mut formatted = vec![0u8; field(0x17)];
        formatted[field(0x05)] = usage;
        formatted[field(0x07)..field(0x0B)].copy_from_slice(&(128u32 * 1024 * 1024).to_le_bytes());
        formatted[field(0x0D)..field(0x0F)].copy_from_slice(&devices.to_le_bytes());
        structure(16, handle, &formatted, &[])
    }

    fn device(array: u16, size: u16, extended_mib: u32, speed: u16, configured: u16) -> Vec<u8> {
        let mut formatted = vec![0u8; field(0x28)];
        formatted[field(0x04)..field(0x06)].copy_from_slice(&array.to_le_bytes());
        formatted[field(0x0C)..field(0x0E)].copy_from_slice(&size.to_le_bytes());
        formatted[field(0x0E)] = 0x09;
        formatted[field(0x10)] = 1;
        formatted[field(0x11)] = 2;
        formatted[field(0x12)] = 0x22;
        formatted[field(0x15)..field(0x17)].copy_from_slice(&speed.to_le_bytes());
        formatted[field(0x17)] = 3;
        formatted[field(0x18)] = 4; // serial number string: must not be decoded
        formatted[field(0x1A)] = 5;
        formatted[field(0x1C)..field(0x20)].copy_from_slice(&extended_mib.to_le_bytes());
        formatted[field(0x20)..field(0x22)].copy_from_slice(&configured.to_le_bytes());
        formatted[field(0x26)..field(0x28)].copy_from_slice(&1100u16.to_le_bytes());
        structure(
            17,
            0x100u16.wrapping_add(size),
            &formatted,
            &[
                "DIMM 1",
                "P0 CHANNEL A",
                "KLEVV",
                "SERIAL-123",
                "KD5BGUA80-64A320J   ",
            ],
        )
    }

    fn raw(table: &[u8]) -> Vec<u8> {
        let mut raw = vec![0, 3, 7, 0];
        raw.extend_from_slice(&(table.len() as u32).to_le_bytes());
        raw.extend_from_slice(table);
        raw
    }

    fn sample_table() -> Vec<u8> {
        [
            array(0x10, USE_SYSTEM_MEMORY, 4),
            array(0x11, 4, 1), // video memory array: not system slots
            device(0x10, 0x7FFF, 32 * 1024, 6400, 6000),
            device(0x10, 0, 0, 0, 0),
            device(0x10, 0x7FFF, 32 * 1024, 6400, 6000),
            device(0x10, 0, 0, 0, 0),
            device(0x11, 1024, 0, 1000, 1000),
            structure(127, 0xFFFF, &[], &[]),
        ]
        .concat()
    }

    #[test]
    fn decodes_slots_modules_and_shared_summary() {
        let inventory = parse_raw(&raw(&sample_table())).unwrap();
        assert_eq!(inventory.smbios_version, (3, 7));
        assert_eq!(inventory.slots_total, Some(4));
        assert_eq!(inventory.slots_used, 2);
        assert_eq!(inventory.max_capacity_bytes, Some(128 * 1024 * 1024 * 1024));
        assert_eq!(inventory.modules.len(), 2);
        let module = &inventory.modules[0];
        assert_eq!(module.size_bytes, Some(32 * 1024 * 1024 * 1024));
        assert_eq!(module.device_locator.as_deref(), Some("DIMM 1"));
        assert_eq!(module.bank_locator.as_deref(), Some("P0 CHANNEL A"));
        assert_eq!(module.manufacturer.as_deref(), Some("KLEVV"));
        assert_eq!(module.part_number.as_deref(), Some("KD5BGUA80-64A320J"));
        assert_eq!(module.speed_mts, Some(6400));
        assert_eq!(module.configured_speed_mts, Some(6000));
        assert_eq!(module.configured_voltage_mv, Some(1100));
        assert!(!format!("{inventory:?}").contains("SERIAL"));
        assert_eq!(inventory.configured_speed_mts(), Some(6000));
        assert_eq!(inventory.memory_type(), Some("DDR5"));
        assert_eq!(inventory.form_factor(), Some("DIMM"));
    }

    #[test]
    fn size_encodings_and_extended_speeds() {
        let parse = |bytes: Vec<u8>| parse_table(&bytes, (3, 3)).modules;
        // Bit 15 selects KiB granularity.
        assert_eq!(
            parse(device(0xFFFE, 0x8000 | 512, 0, 1, 1))[0].size_bytes,
            Some(512 * 1024)
        );
        assert_eq!(
            parse(device(0xFFFE, 8192, 0, 1, 1))[0].size_bytes,
            Some(8 << 30)
        );
        // 0xFFFF: populated, size unknown.
        assert_eq!(parse(device(0xFFFE, 0xFFFF, 0, 1, 1))[0].size_bytes, None);
        let mut long = device(0xFFFE, 8192, 0, 0xFFFF, 0);
        // Extend the formatted area to hold the 3.3 extended speed fields.
        let strings_at = 0x28;
        let mut formatted = long[4..strings_at].to_vec();
        formatted.resize(field(0x5C), 0);
        formatted[field(0x54)..field(0x58)].copy_from_slice(&70_000u32.to_le_bytes());
        long = structure(17, 1, &formatted, &["A"]);
        let module = &parse(long)[0];
        assert_eq!(module.speed_mts, Some(70_000));
        assert_eq!(module.configured_speed_mts, None);
    }

    #[test]
    fn speeds_are_labeled_mts_only_where_the_specification_says_so() {
        assert!(speeds_are_mts((3, 7)));
        assert!(speeds_are_mts((3, 2)));
        assert!(!speeds_are_mts((3, 1)));
        assert!(!speeds_are_mts((2, 8)));
        let table = [
            array(0x10, USE_SYSTEM_MEMORY, 2),
            device(0x10, 8192, 0, 1600, 800),
        ]
        .concat();
        for version in [(2, 8), (3, 0), (3, 1)] {
            let inventory = parse_table(&table, version);
            let module = &inventory.modules[0];
            assert_eq!(module.speed_mts, None, "{version:?}");
            assert_eq!(module.configured_speed_mts, None, "{version:?}");
            assert_eq!(module.speed_reported, Some(1600));
            assert_eq!(module.configured_speed_reported, Some(800));
            assert_eq!(inventory.configured_speed_mts(), None);
        }
        let module = &parse_table(&table, (3, 2)).modules[0];
        assert_eq!(module.speed_mts, Some(1600));
        assert_eq!(module.configured_speed_mts, Some(800));
    }

    #[test]
    fn disagreeing_modules_have_no_summary() {
        let table = [
            device(0xFFFE, 8192, 0, 3200, 3200),
            device(0xFFFE, 8192, 0, 3600, 3600),
        ]
        .concat();
        let inventory = parse_table(&table, (3, 3));
        assert_eq!(inventory.slots_total, None);
        assert_eq!(inventory.slots_used, 2);
        assert_eq!(inventory.configured_speed_mts(), None);
        assert_eq!(inventory.memory_type(), Some("DDR5"));
        assert_eq!(MemoryInventory::default().memory_type(), None);
    }

    #[test]
    fn truncated_and_malformed_tables_never_panic_or_overread() {
        let table = sample_table();
        for end in 0..table.len() {
            let inventory = parse_table(&table[..end], (3, 0));
            assert!(inventory.modules.len() <= 2);
        }
        for raw_end in 0..12 {
            let _ = parse_raw(&raw(&table)[..raw_end]);
        }
        // Length field larger than the buffer, zero-length and short headers.
        let mut bad = raw(&table);
        bad[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse_raw(&bad).unwrap().slots_used, 2);
        assert!(parse_table(&[17, 2, 0, 0, 0, 0], (3, 0)).modules.is_empty());
        assert!(parse_table(&[17, 200, 0, 0, 0, 0], (3, 0))
            .modules
            .is_empty());
        // Short structures lack fields: out-of-range reads become None.
        let short = structure(17, 1, &[0, 0, 0, 0, 0, 0, 0, 0, 0x00, 0x10], &[]);
        let modules = parse_table(&short, (2, 0)).modules;
        assert_eq!(modules.len(), 1);
        assert_eq!(modules[0].speed_mts, None);
        assert_eq!(modules[0].device_locator, None);
        // String indexes past the string set are ignored.
        let mut invalid = device(0xFFFE, 8192, 0, 1, 1);
        invalid[0x10] = 40;
        assert_eq!(
            parse_table(&invalid, (3, 0)).modules[0].device_locator,
            None
        );
        // Pseudo-random bytes.
        let mut seed = 0x1234_5678u32;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        for start in 0..64 {
            let _ = parse_table(&noise[start..], (3, 0));
            let _ = parse_raw(&noise[start..]);
        }
    }
}
