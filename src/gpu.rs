//! GPU adapter facts and sensors from the Windows display kernel (D3DKMT
//! thunks exported by gdi32.dll). No WMI, vendor SDK, driver or elevation.
//!
//! Static facts (names, memory segment sizes, driver version, PCI identity,
//! capability limits) are collected once per adapter set with
//! `D3DKMTEnumAdapters2` and cached. Dynamic sensor values come from
//! `KMTQAITYPE_ADAPTERPERFDATA`, the WDDM 2.5+ query that Windows Task Manager
//! uses for "GPU temperature". Drivers that do not implement it, or report a
//! zero reading, produce `None`; nothing here estimates a missing value.
//!
//! Adapter handles are always closed before a function returns.
//! https://learn.microsoft.com/windows-hardware/drivers/ddi/d3dkmthk/ne-d3dkmthk-_kmtqueryadapterinfotype

use std::mem::{offset_of, size_of};
use std::ptr::null_mut;
use windows_sys::Wdk::Graphics::Direct3D::{
    D3DKMTCloseAdapter, D3DKMTEnumAdapters2, D3DKMTOpenAdapterFromLuid, D3DKMTQueryAdapterInfo,
    D3DDDI_QUERYREGISTRY_ADAPTERKEY, D3DDDI_QUERYREGISTRY_INFO,
    D3DDDI_QUERYREGISTRY_STATUS_BUFFER_OVERFLOW, D3DDDI_QUERYREGISTRY_STATUS_SUCCESS,
    D3DKMT_ADAPTERADDRESS, D3DKMT_ADAPTERINFO, D3DKMT_ADAPTERREGISTRYINFO, D3DKMT_ADAPTERTYPE,
    D3DKMT_ADAPTER_PERFDATA, D3DKMT_ADAPTER_PERFDATACAPS, D3DKMT_CLOSEADAPTER,
    D3DKMT_ENUMADAPTERS2, D3DKMT_OPENADAPTERFROMLUID, D3DKMT_PHYSICAL_ADAPTER_COUNT,
    D3DKMT_QUERYADAPTERINFO, D3DKMT_QUERY_DEVICE_IDS, D3DKMT_SEGMENTSIZEINFO,
    D3DKMT_UMD_DRIVER_VERSION, KMTQAITYPE_ADAPTERADDRESS, KMTQAITYPE_ADAPTERPERFDATA,
    KMTQAITYPE_ADAPTERPERFDATA_CAPS, KMTQAITYPE_ADAPTERREGISTRYINFO, KMTQAITYPE_ADAPTERTYPE,
    KMTQAITYPE_DRIVERVERSION, KMTQAITYPE_DRIVER_DESCRIPTION, KMTQAITYPE_GETSEGMENTSIZE,
    KMTQAITYPE_PHYSICALADAPTERCOUNT, KMTQAITYPE_PHYSICALADAPTERDEVICEIDS, KMTQAITYPE_QUERYREGISTRY,
    KMTQAITYPE_UMD_DRIVER_VERSION, KMTQUERYADAPTERINFOTYPE,
};
use windows_sys::Win32::Foundation::{LUID, STATUS_BUFFER_TOO_SMALL};
use windows_sys::Win32::System::Registry::REG_SZ;

/// Upper bound on adapters accepted from one enumeration (defensive only).
const MAX_ADAPTERS: u32 = 64;
/// Upper bound on physical adapters in one linked-display-adapter chain.
const MAX_PHYSICAL: u32 = 16;
/// `D3DKMT_DRIVER_DESCRIPTION` holds 4096 UTF-16 units.
const DRIVER_DESCRIPTION_UNITS: usize = 4096;
/// Registry string values larger than this are not display text.
const MAX_REGISTRY_BYTES: usize = 16 * 1024;

/// Adapter capability bits from `KMTQAITYPE_ADAPTERTYPE` (WDDM 1.2+).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuAdapterFlags {
    pub render_supported: bool,
    pub display_supported: bool,
    /// Software rasterizer such as "Microsoft Basic Render Driver" (WARP).
    pub software: bool,
    pub hybrid_discrete: bool,
    pub hybrid_integrated: bool,
    /// Indirect display (IddCx) adapter, e.g. a virtual or USB display.
    pub indirect_display: bool,
    pub paravirtualized: bool,
    pub compute_only: bool,
}

impl GpuAdapterFlags {
    fn from_bits(bits: u32) -> Self {
        let bit = |n: u32| bits & (1 << n) != 0;
        Self {
            render_supported: bit(0),
            display_supported: bit(1),
            software: bit(2),
            hybrid_discrete: bit(4),
            hybrid_integrated: bit(5),
            indirect_display: bit(6),
            paravirtualized: bit(7),
            compute_only: bit(11),
        }
    }
}

/// Manufacturer limits from `KMTQAITYPE_ADAPTERPERFDATA_CAPS`, per physical
/// adapter. Zero fields are reported as `None` (not provided by the driver).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpuPerfCaps {
    pub physical_index: u32,
    pub max_fan_rpm: Option<u32>,
    /// Temperature at which damage may occur, in degrees Celsius.
    pub temperature_max_c: Option<f64>,
    /// Temperature at which the driver starts throttling, in degrees Celsius.
    pub temperature_warning_c: Option<f64>,
    /// Maximum memory bandwidth in bytes per second.
    pub max_memory_bandwidth: Option<u64>,
    /// Maximum PCIe bandwidth in bytes per second.
    pub max_pcie_bandwidth: Option<u64>,
}

/// PCI identity of the first physical adapter (`KMTQAITYPE_PHYSICALADAPTERDEVICEIDS`).
/// Hardware IDs only: no instance path or serial number is read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuPciIds {
    pub vendor_id: u32,
    pub device_id: u32,
    pub subsystem_id: u32,
    pub subsystem_vendor_id: u32,
    pub revision_id: u32,
}

/// Static facts about one display adapter, as reported by the kernel and its
/// driver. Every field is `None` when the driver declines the query.
///
/// For an indirect display adapter without render engines (e.g. a virtual
/// display driver) the kernel answers memory, driver, PCI and limit queries
/// with the facts of the render adapter that draws for it, so only `luid`,
/// `name` and `flags` are collected; see [`GpuAdapter::is_indirect_display`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuAdapter {
    /// Adapter LUID as `(HighPart, LowPart)`, the same numbers PDH prints as
    /// `luid_0xHIGH_0xLOW`. Unique only until the next reboot.
    pub luid: (u32, u32),
    /// The adapter's device description (`KMTQAITYPE_DRIVER_DESCRIPTION`, the
    /// name Device Manager and DXGI show, e.g. "NVIDIA GeForce RTX 5080"),
    /// falling back to the registry adapter string when unavailable.
    pub name: Option<String>,
    /// `HardwareInformation.AdapterString` from the adapter's registry key.
    pub adapter_string: Option<String>,
    /// `HardwareInformation.ChipType`.
    pub chip_type: Option<String>,
    /// `HardwareInformation.DacType`.
    pub dac_type: Option<String>,
    /// `HardwareInformation.BiosString` (video BIOS version text).
    pub bios: Option<String>,
    pub flags: Option<GpuAdapterFlags>,
    pub pci: Option<GpuPciIds>,
    /// PCI bus, device and function numbers.
    pub pci_location: Option<(u32, u32, u32)>,
    /// Dedicated video memory (VRAM) reported by the driver's segments, in bytes.
    pub dedicated_video_memory: Option<u64>,
    /// System memory reserved exclusively for this adapter, in bytes.
    pub dedicated_system_memory: Option<u64>,
    /// System memory the adapter may share with the CPU, in bytes.
    pub shared_system_memory: Option<u64>,
    /// WDDM version implemented by the kernel-mode driver, e.g. `(3, 2)`.
    pub wddm_version: Option<(u32, u32)>,
    /// Driver package version (`DriverVersion` in the adapter key), or the
    /// user-mode driver file version when the key value is unreadable.
    pub driver_version: Option<String>,
    /// Driver date from the adapter key, as ISO `YYYY-MM-DD`.
    pub driver_date: Option<String>,
    /// Physical adapters in this (possibly linked) logical adapter; at least 1.
    pub physical_adapters: u32,
    /// Sensor limits per physical adapter; empty when the driver has no
    /// WDDM 2.5 performance data.
    pub perf_caps: Vec<GpuPerfCaps>,
    /// The driver answered `KMTQAITYPE_ADAPTERPERFDATA` during enumeration.
    pub perf_data_supported: bool,
}

impl GpuAdapter {
    /// Software renderer (e.g. Microsoft Basic Render Driver), not hardware.
    pub fn is_software(&self) -> bool {
        self.flags.is_some_and(|flags| flags.software)
    }

    /// Indirect display adapter without its own render engines.
    pub fn is_indirect_display(&self) -> bool {
        self.flags
            .is_some_and(|flags| flags.indirect_display && !flags.render_supported)
    }

    pub fn caps(&self, physical_index: u32) -> Option<&GpuPerfCaps> {
        self.perf_caps
            .iter()
            .find(|caps| caps.physical_index == physical_index)
    }
}

/// Live sensor readings for one physical adapter (`KMTQAITYPE_ADAPTERPERFDATA`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GpuSensors {
    pub physical_index: u32,
    /// GPU temperature in degrees Celsius (driver reports deci-degrees).
    pub temperature_c: Option<f64>,
    /// Fan speed in RPM. `Some(0)` only when the driver also reports a
    /// maximum fan speed (a stopped fan); otherwise a zero is `None`.
    pub fan_rpm: Option<u32>,
    /// Power draw as a percentage of the adapter's power limit (the driver
    /// reports tenths of a percent). This is not watts.
    pub power_percent: Option<f64>,
    /// Current memory clock in hertz.
    pub memory_clock_hz: Option<u64>,
    /// Maximum (non-overclocked) memory clock in hertz.
    pub max_memory_clock_hz: Option<u64>,
}

/// Enumerates every display adapter known to the kernel, including software and
/// indirect-display adapters. Handles opened by the enumeration are closed.
pub fn enumerate() -> Result<Vec<GpuAdapter>, String> {
    let handles = AdapterList::open()?;
    Ok(handles
        .adapters
        .iter()
        .map(|info| describe(info.hAdapter, info.AdapterLuid))
        .collect())
}

/// Reads live sensor values for the given physical adapters of one LUID.
/// Opens the adapter by LUID and closes it before returning.
pub fn sensors(
    luid: (u32, u32),
    physical: &[(u32, Option<GpuPerfCaps>)],
) -> Result<Vec<GpuSensors>, String> {
    let mut open = D3DKMT_OPENADAPTERFROMLUID {
        AdapterLuid: LUID {
            HighPart: luid.0 as i32,
            LowPart: luid.1,
        },
        hAdapter: 0,
    };
    let status = unsafe { D3DKMTOpenAdapterFromLuid(&mut open) };
    if status < 0 {
        return Err(format!(
            "GPU adapter unavailable (NTSTATUS 0x{:08X})",
            status as u32
        ));
    }
    let adapter = Adapter(open.hAdapter);
    let mut result = Vec::with_capacity(physical.len());
    for &(index, caps) in physical {
        let mut data = D3DKMT_ADAPTER_PERFDATA {
            PhysicalAdapterIndex: index,
            ..Default::default()
        };
        if unsafe { query(adapter.0, KMTQAITYPE_ADAPTERPERFDATA, &mut data) } {
            result.push(sensor_values(index, &data, caps.as_ref()));
        }
    }
    Ok(result)
}

/// Converts raw performance data using the driver's documented units.
pub(crate) fn sensor_values(
    index: u32,
    data: &D3DKMT_ADAPTER_PERFDATA,
    caps: Option<&GpuPerfCaps>,
) -> GpuSensors {
    let fan_reported = caps.is_some_and(|caps| caps.max_fan_rpm.is_some());
    GpuSensors {
        physical_index: index,
        temperature_c: deci_celsius(data.Temperature),
        fan_rpm: (data.FanRPM > 0 || fan_reported).then_some(data.FanRPM),
        // More than 100 % is possible when a card exceeds its limit briefly;
        // anything above 10x the limit is not a plausible reading.
        power_percent: (data.Power > 0 && data.Power <= 10_000).then(|| data.Power as f64 / 10.0),
        memory_clock_hz: (data.MemoryFrequency > 0).then_some(data.MemoryFrequency),
        max_memory_clock_hz: (data.MaxMemoryFrequency > 0).then_some(data.MaxMemoryFrequency),
    }
}

/// Deci-degrees Celsius to degrees; zero and implausible values are `None`.
fn deci_celsius(value: u32) -> Option<f64> {
    let celsius = value as f64 / 10.0;
    (value > 0 && celsius <= 150.0).then_some(celsius)
}

/// Converts a `D3DKMT_DRIVERVERSION` (e.g. 3200) into `(3, 2)`.
fn wddm_version(value: i32) -> Option<(u32, u32)> {
    let value = u32::try_from(value)
        .ok()
        .filter(|v| (1000..10_000).contains(v))?;
    Some((value / 1000, value % 1000 / 100))
}

/// Formats a packed `a.b.c.d` driver file version (four 16-bit words).
fn packed_version(value: i64) -> Option<String> {
    let value = value as u64;
    (value != 0).then(|| {
        format!(
            "{}.{}.{}.{}",
            value >> 48,
            (value >> 32) & 0xffff,
            (value >> 16) & 0xffff,
            value & 0xffff
        )
    })
}

/// Converts the INF `DriverDate` text `M-D-YYYY` to ISO `YYYY-MM-DD`.
fn iso_driver_date(text: &str) -> Option<String> {
    let mut parts = text.trim().split(['-', '/']);
    let month: u32 = parts.next()?.trim().parse().ok()?;
    let day: u32 = parts.next()?.trim().parse().ok()?;
    let year: u32 = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some()
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(1980..=9999).contains(&year)
    {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

fn text(units: &[u16]) -> Option<String> {
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    let value = String::from_utf16_lossy(&units[..end]);
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

/// Parses a PDH GPU adapter instance `luid_0xHHHHHHHH_0xLLLLLLLL_phys_N` into
/// `((high, low), physical_index)`. Hex digits may be in either case.
pub fn parse_pdh_adapter(id: &str) -> Option<((u32, u32), u32)> {
    let body = id.strip_prefix("luid_")?;
    let (luid, physical) = body.split_once("_phys_")?;
    let (high, low) = luid.split_once('_')?;
    let hex = |part: &str| {
        let digits = part
            .strip_prefix("0x")
            .or_else(|| part.strip_prefix("0X"))?;
        (!digits.is_empty() && digits.len() <= 8)
            .then(|| u32::from_str_radix(digits, 16).ok())
            .flatten()
    };
    let physical = physical.parse::<u32>().ok()?;
    Some(((hex(high)?, hex(low)?), physical))
}

struct Adapter(u32);

impl Drop for Adapter {
    fn drop(&mut self) {
        let close = D3DKMT_CLOSEADAPTER { hAdapter: self.0 };
        unsafe { D3DKMTCloseAdapter(&close) };
    }
}

/// Handles returned by `D3DKMTEnumAdapters2`; each one is closed on drop.
struct AdapterList {
    adapters: Vec<D3DKMT_ADAPTERINFO>,
}

impl AdapterList {
    fn open() -> Result<Self, String> {
        let mut request = D3DKMT_ENUMADAPTERS2 {
            NumAdapters: 0,
            pAdapters: null_mut(),
        };
        let status = unsafe { D3DKMTEnumAdapters2(&mut request) };
        if status < 0 {
            return Err(format!(
                "GPU adapters unavailable (NTSTATUS 0x{:08X})",
                status as u32
            ));
        }
        for _ in 0..4 {
            let capacity = request.NumAdapters.clamp(1, MAX_ADAPTERS);
            let mut adapters = vec![D3DKMT_ADAPTERINFO::default(); capacity as usize];
            request.NumAdapters = capacity;
            request.pAdapters = adapters.as_mut_ptr();
            let status = unsafe { D3DKMTEnumAdapters2(&mut request) };
            if status == STATUS_BUFFER_TOO_SMALL && capacity < MAX_ADAPTERS {
                request.NumAdapters = request.NumAdapters.max(capacity + 1);
                continue;
            }
            if status < 0 {
                return Err(format!(
                    "GPU adapters unavailable (NTSTATUS 0x{:08X})",
                    status as u32
                ));
            }
            adapters.truncate(request.NumAdapters.min(capacity) as usize);
            return Ok(Self { adapters });
        }
        Err("GPU adapter list changed repeatedly".into())
    }
}

impl Drop for AdapterList {
    fn drop(&mut self) {
        for adapter in &self.adapters {
            let close = D3DKMT_CLOSEADAPTER {
                hAdapter: adapter.hAdapter,
            };
            unsafe { D3DKMTCloseAdapter(&close) };
        }
    }
}

/// Issues one fixed-size adapter query. `T` must be the exact structure the
/// query type documents, because the kernel validates the size.
unsafe fn query<T>(handle: u32, kind: KMTQUERYADAPTERINFOTYPE, value: &mut T) -> bool {
    query_raw(handle, kind, (value as *mut T).cast(), size_of::<T>())
}

unsafe fn query_raw(
    handle: u32,
    kind: KMTQUERYADAPTERINFOTYPE,
    data: *mut core::ffi::c_void,
    bytes: usize,
) -> bool {
    let Ok(bytes) = u32::try_from(bytes) else {
        return false;
    };
    let mut request = D3DKMT_QUERYADAPTERINFO {
        hAdapter: handle,
        Type: kind,
        pPrivateDriverData: data,
        PrivateDriverDataSize: bytes,
    };
    D3DKMTQueryAdapterInfo(&mut request) >= 0
}

fn describe(handle: u32, luid: LUID) -> GpuAdapter {
    let mut adapter = GpuAdapter {
        luid: (luid.HighPart as u32, luid.LowPart),
        physical_adapters: 1,
        ..Default::default()
    };
    unsafe {
        let mut registry: Box<D3DKMT_ADAPTERREGISTRYINFO> = Box::new(std::mem::zeroed());
        if query(handle, KMTQAITYPE_ADAPTERREGISTRYINFO, &mut *registry) {
            adapter.adapter_string = text(&registry.AdapterString);
            adapter.chip_type = text(&registry.ChipType);
            adapter.dac_type = text(&registry.DacType);
            adapter.bios = text(&registry.BiosString);
        }
        let mut description = vec![0u16; DRIVER_DESCRIPTION_UNITS];
        if query_raw(
            handle,
            KMTQAITYPE_DRIVER_DESCRIPTION,
            description.as_mut_ptr().cast(),
            description.len() * size_of::<u16>(),
        ) {
            adapter.name = text(&description);
        }
        if adapter.name.is_none() {
            adapter.name = adapter.adapter_string.clone();
        }
        let mut kind = D3DKMT_ADAPTERTYPE::default();
        if query(handle, KMTQAITYPE_ADAPTERTYPE, &mut kind) {
            adapter.flags = Some(GpuAdapterFlags::from_bits(kind.Anonymous.Value));
        }
        if adapter.is_indirect_display() {
            // Memory, driver, PCI and sensor queries on an indirect display
            // adapter are answered with the render adapter's facts (e.g. the
            // NVIDIA driver version for a virtual display). Keep only what
            // describes this adapter itself.
            return adapter;
        }
        let mut segments = D3DKMT_SEGMENTSIZEINFO::default();
        if query(handle, KMTQAITYPE_GETSEGMENTSIZE, &mut segments) {
            adapter.dedicated_video_memory = Some(segments.DedicatedVideoMemorySize);
            adapter.dedicated_system_memory = Some(segments.DedicatedSystemMemorySize);
            adapter.shared_system_memory = Some(segments.SharedSystemMemorySize);
        }
        let mut version = 0i32;
        if query(handle, KMTQAITYPE_DRIVERVERSION, &mut version) {
            adapter.wddm_version = wddm_version(version);
        }
        adapter.driver_version = registry_string(handle, "DriverVersion");
        if adapter.driver_version.is_none() {
            let mut umd = D3DKMT_UMD_DRIVER_VERSION::default();
            if query(handle, KMTQAITYPE_UMD_DRIVER_VERSION, &mut umd) {
                adapter.driver_version = packed_version(umd.DriverVersion);
            }
        }
        adapter.driver_date =
            registry_string(handle, "DriverDate").and_then(|date| iso_driver_date(&date));
        let mut address = D3DKMT_ADAPTERADDRESS::default();
        // Adapters without a PCI location (software, indirect display)
        // report all-ones numbers.
        if query(handle, KMTQAITYPE_ADAPTERADDRESS, &mut address)
            && address.BusNumber != u32::MAX
            && address.DeviceNumber != 0xFFFF
        {
            adapter.pci_location = Some((
                address.BusNumber,
                address.DeviceNumber,
                address.FunctionNumber,
            ));
        }
        let mut ids = D3DKMT_QUERY_DEVICE_IDS::default();
        if query(handle, KMTQAITYPE_PHYSICALADAPTERDEVICEIDS, &mut ids)
            && ids.DeviceIds.VendorID != 0
        {
            adapter.pci = Some(GpuPciIds {
                vendor_id: ids.DeviceIds.VendorID,
                device_id: ids.DeviceIds.DeviceID,
                subsystem_id: ids.DeviceIds.SubSystemID,
                subsystem_vendor_id: ids.DeviceIds.SubVendorID,
                revision_id: ids.DeviceIds.RevisionID,
            });
        }
        let mut count = D3DKMT_PHYSICAL_ADAPTER_COUNT::default();
        if query(handle, KMTQAITYPE_PHYSICALADAPTERCOUNT, &mut count) {
            adapter.physical_adapters = count.Count.clamp(1, MAX_PHYSICAL);
        }
        for index in 0..adapter.physical_adapters {
            let mut caps = D3DKMT_ADAPTER_PERFDATACAPS {
                PhysicalAdapterIndex: index,
                ..Default::default()
            };
            if query(handle, KMTQAITYPE_ADAPTERPERFDATA_CAPS, &mut caps) {
                let caps = GpuPerfCaps {
                    physical_index: index,
                    max_fan_rpm: (caps.MaxFanRPM > 0).then_some(caps.MaxFanRPM),
                    temperature_max_c: deci_celsius(caps.TemperatureMax),
                    temperature_warning_c: deci_celsius(caps.TemperatureWarning),
                    max_memory_bandwidth: (caps.MaxMemoryBandwidth > 0)
                        .then_some(caps.MaxMemoryBandwidth),
                    max_pcie_bandwidth: (caps.MaxPCIEBandwidth > 0)
                        .then_some(caps.MaxPCIEBandwidth),
                };
                // All-zero answers carry no limit.
                if caps
                    != (GpuPerfCaps {
                        physical_index: index,
                        ..Default::default()
                    })
                {
                    adapter.perf_caps.push(caps);
                }
            }
            let mut data = D3DKMT_ADAPTER_PERFDATA {
                PhysicalAdapterIndex: index,
                ..Default::default()
            };
            if query(handle, KMTQAITYPE_ADAPTERPERFDATA, &mut data) {
                adapter.perf_data_supported = true;
            }
        }
    }
    adapter
}

/// Reads a `REG_SZ` value from the adapter's own driver key through the
/// display kernel (`KMTQAITYPE_QUERYREGISTRY`, the path user-mode drivers use).
fn registry_string(handle: u32, name: &str) -> Option<String> {
    let header = size_of::<D3DDDI_QUERYREGISTRY_INFO>();
    let output = offset_of!(D3DDDI_QUERYREGISTRY_INFO, Anonymous);
    let mut request: D3DDDI_QUERYREGISTRY_INFO = unsafe { std::mem::zeroed() };
    request.QueryType = D3DDDI_QUERYREGISTRY_ADAPTERKEY;
    request.ValueType = REG_SZ;
    request.PhysicalAdapterIndex = 0;
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.len() >= request.ValueName.len() {
        return None;
    }
    request.ValueName[..units.len()].copy_from_slice(&units);
    let mut extra = 512usize;
    for _ in 0..3 {
        let bytes = header + extra;
        // u64 storage satisfies the structure's alignment.
        let mut buffer = vec![0u64; bytes.div_ceil(size_of::<u64>())];
        let info = buffer.as_mut_ptr().cast::<D3DDDI_QUERYREGISTRY_INFO>();
        unsafe {
            info.write(request);
            if !query_raw(handle, KMTQAITYPE_QUERYREGISTRY, info.cast(), bytes) {
                return None;
            }
            let size = (*info).OutputValueSize as usize;
            match (*info).Status {
                D3DDDI_QUERYREGISTRY_STATUS_SUCCESS => {
                    let available = buffer.len() * size_of::<u64>() - output;
                    if size > available || !size.is_multiple_of(2) {
                        return None;
                    }
                    let start = buffer.as_ptr().cast::<u8>().add(output).cast::<u16>();
                    let units = std::slice::from_raw_parts(start, size / 2);
                    return text(units);
                }
                D3DDDI_QUERYREGISTRY_STATUS_BUFFER_OVERFLOW
                    if size > extra && size <= MAX_REGISTRY_BYTES =>
                {
                    extra = size + 16;
                }
                _ => return None,
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdh_adapter_ids_parse_both_hex_cases_and_reject_malformed() {
        assert_eq!(
            parse_pdh_adapter("luid_0x00000000_0x0001A2bC_phys_1"),
            Some(((0, 0x1a2bc), 1))
        );
        assert_eq!(
            parse_pdh_adapter("luid_0xFFFFFFFF_0x00000001_phys_0"),
            Some(((u32::MAX, 1), 0))
        );
        for bad in [
            "",
            "luid_0x0_phys_0",
            "luid_00000000_0x00000001_phys_0",
            "luid_0x_0x1_phys_0",
            "luid_0x000000001_0x1_phys_0",
            "luid_0x0_0x1_phys_x",
            "pid_1_luid_0x0_0x1_phys_0",
        ] {
            assert_eq!(parse_pdh_adapter(bad), None, "{bad}");
        }
    }

    #[test]
    fn wddm_and_driver_versions_decode_documented_encodings() {
        assert_eq!(wddm_version(3200), Some((3, 2)));
        assert_eq!(wddm_version(1105), Some((1, 1)));
        assert_eq!(wddm_version(2900), Some((2, 9)));
        assert_eq!(wddm_version(0), None);
        assert_eq!(wddm_version(-1), None);
        let packed = (32i64 << 48) | (16 << 16) | 1692;
        assert_eq!(packed_version(packed).as_deref(), Some("32.0.16.1692"));
        assert_eq!(packed_version(0), None);
    }

    #[test]
    fn driver_dates_become_iso_only_when_valid() {
        assert_eq!(iso_driver_date("9-4-2026").as_deref(), Some("2026-09-04"));
        assert_eq!(iso_driver_date("12/31/2024").as_deref(), Some("2024-12-31"));
        for bad in [
            "",
            "2026-09-04",
            "13-1-2026",
            "1-32-2026",
            "1-1",
            "1-1-2026-1",
            "a-b-c",
        ] {
            assert_eq!(iso_driver_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn sensors_treat_zero_as_unreported_except_a_stopped_fan_with_caps() {
        let data = D3DKMT_ADAPTER_PERFDATA {
            Temperature: 453,
            FanRPM: 0,
            Power: 125,
            MemoryFrequency: 15_001_000_000,
            ..Default::default()
        };
        let sensors = sensor_values(0, &data, None);
        assert_eq!(sensors.temperature_c, Some(45.3));
        assert_eq!(sensors.fan_rpm, None);
        assert_eq!(sensors.power_percent, Some(12.5));
        assert_eq!(sensors.memory_clock_hz, Some(15_001_000_000));
        assert_eq!(sensors.max_memory_clock_hz, None);
        let caps = GpuPerfCaps {
            max_fan_rpm: Some(3000),
            ..Default::default()
        };
        assert_eq!(sensor_values(0, &data, Some(&caps)).fan_rpm, Some(0));
        let empty = sensor_values(1, &D3DKMT_ADAPTER_PERFDATA::default(), None);
        assert_eq!(empty.temperature_c, None);
        assert_eq!(empty.power_percent, None);
        assert_eq!(empty.memory_clock_hz, None);
        let hot = D3DKMT_ADAPTER_PERFDATA {
            Temperature: 60_000,
            Power: 20_000,
            ..Default::default()
        };
        assert_eq!(sensor_values(0, &hot, None).temperature_c, None);
        assert_eq!(sensor_values(0, &hot, None).power_percent, None);
    }

    #[test]
    fn adapter_flags_decode_documented_bits() {
        let flags = GpuAdapterFlags::from_bits(0b100_0100);
        assert!(flags.software && flags.indirect_display);
        assert!(!flags.render_supported && !flags.display_supported);
        let flags = GpuAdapterFlags::from_bits(0b11 | (1 << 11));
        assert!(flags.render_supported && flags.display_supported && flags.compute_only);
    }

    #[test]
    fn utf16_text_stops_at_nul_and_trims() {
        let mut units = [0u16; 8];
        for (slot, unit) in units.iter_mut().zip(" GPU \0x".encode_utf16()) {
            *slot = unit;
        }
        assert_eq!(text(&units).as_deref(), Some("GPU"));
        assert_eq!(text(&[0; 4]), None);
        assert_eq!(text(&[32, 32]), None);
    }
}
