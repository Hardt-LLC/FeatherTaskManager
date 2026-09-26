//! On-demand native performance collection. No WMI, shell processes or timers.
//! Disk rates cover all physical disks; activity is the busiest physical disk.
//! GPU load is the busiest physical engine, summed across processes on that
//! engine. Network rates cover active hardware interfaces, excluding loopback,
//! VPN and filter interfaces so one packet is not counted at several layers.
//!
//! Static hardware facts (CPU caches and base speed, SMBIOS memory modules,
//! disk models, GPU adapter names and memory sizes) are collected once per
//! sampler and again only when the PDH disk or GPU adapter set changes. Per
//! sample, only GPU sensor values (`gpu::sensors`) are read in addition to
//! the PDH counters and the interface table.

use crate::gpu::{self, GpuAdapter, GpuSensors};
use crate::sampler::Process;
use crate::smbios::{self, MemoryInventory};
use crate::storage::{self, StorageDevice};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::STATUS_BUFFER_TOO_SMALL;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_ETHERNET_CSMACD, IF_TYPE_IEEE80211,
    IF_TYPE_SOFTWARE_LOOPBACK, IF_TYPE_WWANPP, IF_TYPE_WWANPP2, MIB_IF_TABLE2,
};
use windows_sys::Win32::NetworkManagement::Ndis::{IfOperStatusNotPresent, IfOperStatusUp};
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT_COUNTERVALUE_ITEM_W,
    PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
};
use windows_sys::Win32::System::Power::{
    CallNtPowerInformation, ProcessorInformation, PROCESSOR_POWER_INFORMATION,
};
use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows_sys::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetTickCount64, RelationAll,
};
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, IsProcessorFeaturePresent, PF_VIRT_FIRMWARE_ENABLED,
};

const MAX_INTERFACES: usize = 16_384;
const MAX_PDH_BYTES: usize = 8 * 1024 * 1024;
// PDH_FMT_NOCAP100 is not exposed by windows-sys 0.61.2.
const FMT_DOUBLE_UNCAPPED: u32 = PDH_FMT_DOUBLE | 0x8000;
/// Minimum spacing between GPU re-enumerations triggered by unknown adapters.
const GPU_ENUMERATION_INTERVAL: Duration = Duration::from_secs(5);
/// ACPI thermal zone readings outside (0, 150] °C are discarded as implausible.
const THERMAL_MAX_CELSIUS: f64 = 150.0;
const KELVIN_OFFSET: f64 = 273.15;

#[derive(Clone, Debug)]
pub struct LogicalProcessor {
    /// Windows processor group and index, stable across ordering changes.
    pub id: String,
    pub group: u32,
    pub index: u32,
    pub percent: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct MemoryStats {
    pub physical_total: u64,
    pub physical_available: u64,
    pub commit_used: u64,
    pub commit_limit: u64,
    pub commit_peak: u64,
    pub cache: u64,
    pub kernel_paged: u64,
    pub kernel_nonpaged: u64,
    pub processes: u32,
    pub threads: u32,
    pub handles: u32,
}

#[derive(Clone, Debug)]
pub struct DiskStats {
    /// The physical disk counter instance, e.g. `0 C:`. Not a model name.
    pub id: String,
    pub read_bytes_per_sec: Option<f64>,
    pub write_bytes_per_sec: Option<f64>,
    pub active_percent: Option<f64>,
}

/// Connection medium of a network interface, from its NDIS physical medium
/// and, when that is unspecified, its IANA interface type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkKind {
    Ethernet,
    WiFi,
    MobileBroadband,
    Bluetooth,
    Other,
}

impl NetworkKind {
    /// Classifies `MIB_IF_ROW2.PhysicalMediumType` / `Type`. Bluetooth PAN
    /// reports an Ethernet ifType, so the physical medium is checked first.
    pub fn classify(interface_type: u32, physical_medium: i32) -> Self {
        match physical_medium {
            1 | 9 => return Self::WiFi,             // WirelessLan, Native802_11
            8 | 12 => return Self::MobileBroadband, // WirelessWan, WiMax
            10 => return Self::Bluetooth,
            14 => return Self::Ethernet, // 802.3
            _ => {}
        }
        match interface_type {
            IF_TYPE_ETHERNET_CSMACD => Self::Ethernet,
            IF_TYPE_IEEE80211 => Self::WiFi,
            IF_TYPE_WWANPP | IF_TYPE_WWANPP2 => Self::MobileBroadband,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ethernet => "Ethernet",
            Self::WiFi => "Wi-Fi",
            Self::MobileBroadband => "Mobile broadband",
            Self::Bluetooth => "Bluetooth",
            Self::Other => "Other",
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetworkStats {
    pub id: u64,
    /// Interface alias, e.g. "Ethernet" or "Wi-Fi".
    pub name: String,
    /// Adapter hardware description (driver-provided model), e.g.
    /// "Intel(R) Wi-Fi 7 BE200 320MHz". No MAC or IP address is collected.
    pub description: String,
    pub connected: bool,
    /// The interface's hardware is present (`OperStatus` is not
    /// `NotPresent`, e.g. a removed or disabled-by-hardware adapter).
    pub present: bool,
    pub receive_link_bits_per_sec: u64,
    pub transmit_link_bits_per_sec: u64,
    pub rx_bytes_per_sec: Option<f64>,
    pub tx_bytes_per_sec: Option<f64>,
    /// IANA ifType (`MIB_IF_ROW2.Type`).
    pub interface_type: u32,
    /// `NDIS_PHYSICAL_MEDIUM` (`MIB_IF_ROW2.PhysicalMediumType`).
    pub physical_medium: i32,
    pub kind: NetworkKind,
}

#[derive(Clone, Debug)]
pub struct GpuEngine {
    pub id: String,
    pub name: String,
    pub percent: f64,
}

#[derive(Clone, Debug)]
pub struct GpuStats {
    /// Actual Windows adapter LUID and physical index; never an inferred name.
    pub id: String,
    /// Adapter name reported by the display kernel for this LUID (see
    /// [`GpuAdapter::name`]); `None` when the adapter could not be matched.
    pub name: Option<String>,
    /// Busiest engine, not a sum across independent engines.
    pub percent: Option<f64>,
    pub dedicated_bytes: Option<u64>,
    pub shared_bytes: Option<u64>,
    pub engines: Vec<GpuEngine>,
    /// Physical adapter index parsed from `id` (`…_phys_N`).
    pub physical_index: Option<u32>,
    /// Static facts for the matching D3DKMT adapter (memory sizes, driver).
    pub adapter: Option<Arc<GpuAdapter>>,
    /// Live sensors (temperature, fan, power, memory clock) for this physical
    /// adapter; `None` when the driver has no WDDM 2.5 performance data.
    pub sensors: Option<GpuSensors>,
}

impl GpuStats {
    /// A software renderer such as "Microsoft Basic Render Driver" (WARP);
    /// Windows Task Manager does not list these as GPUs.
    pub fn is_software(&self) -> bool {
        self.adapter
            .as_ref()
            .is_some_and(|adapter| adapter.is_software())
    }
}

/// Total cache capacity per level across all processor packages, as listed
/// by `GetLogicalProcessorInformationEx` (each physical cache counted once;
/// level 1 includes instruction and data caches).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuCaches {
    pub l1_bytes: Option<u64>,
    pub l2_bytes: Option<u64>,
    pub l3_bytes: Option<u64>,
}

/// One ACPI thermal zone (`\Thermal Zone Information(*)`). A thermal zone is
/// a firmware-defined sensor location, not necessarily the CPU package; the
/// UI must label it by zone name, never as "CPU temperature".
#[derive(Clone, Debug, PartialEq)]
pub struct ThermalZone {
    /// PDH instance name, e.g. `\_TZ.TZ00`.
    pub name: String,
    pub celsius: f64,
}

/// GPU use of one process, from PDH `GPU Engine` and `GPU Process Memory`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcessGpu {
    /// Utilization of this process's busiest engine, in percent of that
    /// engine (0-100). Per engine, the process's counter instances are
    /// summed; the process value is the maximum over all engines of all
    /// adapters. This is the rule Windows Task Manager's GPU column uses.
    pub percent: Option<f64>,
    /// The busiest engine, when `percent` is above zero.
    pub engine: Option<ProcessGpuEngine>,
    /// Dedicated GPU memory in use by the process: bytes resident in the
    /// adapters' local memory (`GPU Process Memory\Local Usage`), summed
    /// over adapters.
    pub dedicated_bytes: Option<u64>,
    /// Shared GPU memory in use by the process: system memory used by the
    /// GPU for it (`GPU Process Memory\Shared Usage`), summed over adapters.
    pub shared_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessGpuEngine {
    /// PDH adapter id (`luid_…_phys_N`), matching [`GpuStats::id`].
    pub adapter: String,
    /// Engine type from the counter instance, e.g. "3D", "VideoDecode".
    pub engine_type: String,
}

/// Per-process GPU values of one performance sample, keyed by PID only.
/// Use [`ProcessGpuTracker`] to attribute them to process identities.
#[derive(Clone, Debug, Default)]
pub struct ProcessGpuSample {
    /// The engine utilization array was measured for this interval.
    pub utilization_measured: bool,
    /// The per-process GPU memory arrays were measured at this sample.
    pub memory_measured: bool,
    /// Processes that own at least one GPU engine or memory instance.
    pub by_pid: HashMap<u32, ProcessGpu>,
    /// PIDs with engine instances that PDH has not produced a value for yet
    /// (new instances need two collections).
    pub pending: HashSet<u32>,
}

#[derive(Clone, Debug)]
pub struct PerfSnapshot {
    pub cpu_name: String,
    pub logical_cpus: u32,
    pub physical_cores: Option<u32>,
    pub sockets: Option<u32>,
    /// Average of Windows' reported processor frequencies in MHz. This is not
    /// an effective-clock or temperature measurement.
    pub cpu_frequency_mhz: Option<f64>,
    /// Nominal maximum processor frequency in MHz reported by the power
    /// manager (`PROCESSOR_POWER_INFORMATION.MaxMhz`, highest across logical
    /// processors); Task Manager shows the same value as "Base speed".
    pub cpu_base_mhz: Option<u32>,
    pub cpu_caches: Option<CpuCaches>,
    /// Firmware virtualization (VT-x/AMD-V) is enabled and available to
    /// Windows (`PF_VIRT_FIRMWARE_ENABLED`).
    pub virtualization_firmware_enabled: bool,
    pub logical_processors: Vec<LogicalProcessor>,
    pub memory: Option<MemoryStats>,
    /// Installed memory modules from SMBIOS (slots, speed, type, form factor).
    pub memory_modules: Option<Arc<MemoryInventory>>,
    pub disks: Vec<DiskStats>,
    /// Identity of the physical disks in `disks`, by disk number. Use
    /// [`PerfSnapshot::storage_device`] to look one up by PDH instance.
    pub storage: Vec<Arc<StorageDevice>>,
    pub networks: Vec<NetworkStats>,
    pub gpus: Vec<GpuStats>,
    /// Every adapter the display kernel enumerates, including software and
    /// indirect-display adapters that have no PDH engine instances.
    pub gpu_adapters: Vec<Arc<GpuAdapter>>,
    /// Per-process GPU utilization and memory keyed by PID.
    pub process_gpu: ProcessGpuSample,
    /// ACPI thermal zones with plausible readings. Empty on machines whose
    /// firmware exposes none. Not a CPU package temperature.
    pub thermal_zones: Vec<ThermalZone>,
    pub uptime_seconds: u64,
    pub network_rx_bytes_per_sec: f64,
    pub network_tx_bytes_per_sec: f64,
    /// Both rates have a valid previous sample for every active hardware NIC.
    /// False during the first sample, adapter changes, resets, or failures.
    pub network_rates_ready: bool,
    pub disk_read_bytes_per_sec: f64,
    pub disk_write_bytes_per_sec: f64,
    /// Both disk throughput counters were successfully measured this interval.
    pub disk_rates_ready: bool,
    /// Busiest physical disk, not the sum of disk percentages.
    pub disk_active_percent: Option<f64>,
    /// Busiest physical GPU engine, after summing its per-process instances.
    pub gpu_percent: Option<f64>,
    /// Time spent in `PerfSampler::sample`, in milliseconds.
    pub sample_ms: f64,
    /// Part of `sample_ms` spent reading GPU sensors, in milliseconds.
    pub gpu_sensor_ms: f64,
    pub warnings: Vec<String>,
}

impl PerfSnapshot {
    /// Storage identity for a PDH `PhysicalDisk` instance such as `"0 C: D:"`.
    pub fn storage_device(&self, disk_id: &str) -> Option<&StorageDevice> {
        let number = storage::disk_number(disk_id)?;
        self.storage
            .iter()
            .find(|device| device.disk_number == number)
            .map(Arc::as_ref)
    }
}

/// Attributes [`ProcessGpuSample`] values to process identities (PID and
/// creation time) so a reused PID never inherits another process's value.
///
/// Call [`ProcessGpuTracker::join`] once per monitor iteration with the
/// process snapshot and the performance snapshot taken in that iteration.
#[derive(Default)]
pub struct ProcessGpuTracker {
    previous: HashMap<u32, u64>,
}

impl ProcessGpuTracker {
    /// Returns GPU values keyed by `(pid, created)`.
    ///
    /// Utilization is a rate over the interval since the previous sample, so
    /// it is reported only for identities present in both this and the
    /// previous `join`; a process that started or whose PID was reused in
    /// between gets `None`. Memory is a gauge and is reported for every
    /// process in `processes`. When a counter array was measured, a process
    /// without any instance of it reports a measured zero (it owns no GPU
    /// engine context or GPU allocation); when it was not measured, `None`.
    pub fn join(
        &mut self,
        processes: &[Process],
        performance: Option<&PerfSnapshot>,
    ) -> HashMap<(u32, u64), ProcessGpu> {
        let mut result = HashMap::new();
        if let Some(sample) = performance.map(|perf| &perf.process_gpu) {
            result.reserve(processes.len());
            for process in processes {
                let measured = sample.by_pid.get(&process.pid);
                let continuing = self.previous.get(&process.pid) == Some(&process.created);
                let mut value = ProcessGpu::default();
                if sample.utilization_measured
                    && continuing
                    && !sample.pending.contains(&process.pid)
                {
                    value.percent = Some(measured.and_then(|m| m.percent).unwrap_or(0.0));
                    value.engine = measured.and_then(|m| m.engine.clone());
                }
                if sample.memory_measured {
                    value.dedicated_bytes =
                        Some(measured.and_then(|m| m.dedicated_bytes).unwrap_or(0));
                    value.shared_bytes = Some(measured.and_then(|m| m.shared_bytes).unwrap_or(0));
                }
                if value != ProcessGpu::default() {
                    result.insert((process.pid, process.created), value);
                }
            }
        }
        self.previous.clear();
        self.previous.extend(
            processes
                .iter()
                .map(|process| (process.pid, process.created)),
        );
        result
    }
}

pub struct PerfSampler {
    cpu_name: String,
    logical_cpus: u32,
    physical_cores: Option<u32>,
    sockets: Option<u32>,
    cpu_base_mhz: Option<u32>,
    cpu_caches: Option<CpuCaches>,
    virtualization_firmware_enabled: bool,
    memory_modules: Option<Arc<MemoryInventory>>,
    pdh: Option<PdhQuery>,
    setup_warnings: Vec<String>,
    network: HashMap<u64, NetworkCounters>,
    network_at: Option<Instant>,
    generation: u64,
    gpu: GpuInventory,
    storage: StorageInventory,
    /// Collect hardware identity and sensors (false only for `baseline`).
    hardware: bool,
}

/// Physical adapters (with their limits) to read sensors for, per LUID.
type SensorRequests = BTreeMap<(u32, u32), Vec<(u32, Option<gpu::GpuPerfCaps>)>>;

/// Cached D3DKMT adapter facts; refreshed when the PDH adapter set changes.
#[derive(Default)]
struct GpuInventory {
    adapters: Vec<Arc<GpuAdapter>>,
    enumerated_at: Option<Instant>,
    /// PDH adapter LUIDs at the last enumeration; a different set triggers
    /// a new enumeration.
    pdh_luids: BTreeSet<(u32, u32)>,
    error: Option<String>,
}

impl GpuInventory {
    fn due(&self) -> bool {
        self.enumerated_at
            .is_none_or(|at| at.elapsed() >= GPU_ENUMERATION_INTERVAL)
    }
}

/// Cached disk identities for the current set of PDH disk instances.
#[derive(Default)]
struct StorageInventory {
    ids: BTreeSet<String>,
    devices: Vec<Arc<StorageDevice>>,
    warnings: Vec<String>,
}

#[derive(Clone, Copy)]
struct NetworkCounters {
    rx: u64,
    tx: u64,
    generation: u64,
}

impl PerfSampler {
    pub fn new() -> Result<Self, String> {
        Self::build(true)
    }

    /// A sampler without the hardware-identity collectors (SMBIOS, storage,
    /// D3DKMT, per-process GPU, thermal zones). Used only to measure what
    /// those collectors add to each sample.
    pub fn baseline() -> Result<Self, String> {
        Self::build(false)
    }

    fn build(hardware: bool) -> Result<Self, String> {
        let mut warnings = Vec::new();
        let cpu_name = cpu_name().unwrap_or_else(|error| {
            warnings.push(error);
            "Processor".to_string()
        });
        let pdh = PdhQuery::new(&mut warnings, hardware);
        let (physical_cores, sockets, cpu_caches) = match cpu_topology() {
            Ok((cores, sockets, caches)) => (Some(cores), Some(sockets), caches),
            Err(error) => {
                warnings.push(error);
                (None, None, None)
            }
        };
        let logical_cpus = unsafe { GetActiveProcessorCount(0xffff) }.max(1);
        let cpu_base_mhz = cpu_base_mhz(logical_cpus).unwrap_or_else(|error| {
            warnings.push(error);
            None
        });
        let memory_modules = match hardware.then(smbios::memory_inventory) {
            Some(Ok(inventory)) => Some(Arc::new(inventory)),
            Some(Err(error)) => {
                warnings.push(format!("Memory modules: {error}"));
                None
            }
            None => None,
        };
        Ok(Self {
            cpu_name,
            logical_cpus,
            physical_cores,
            sockets,
            cpu_base_mhz,
            cpu_caches,
            virtualization_firmware_enabled: unsafe {
                IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED)
            } != 0,
            memory_modules,
            pdh,
            setup_warnings: warnings,
            network: HashMap::new(),
            network_at: None,
            generation: 0,
            gpu: GpuInventory::default(),
            storage: StorageInventory::default(),
            hardware,
        })
    }

    pub fn sample(&mut self) -> Result<PerfSnapshot, String> {
        let started = Instant::now();
        let mut snapshot = PerfSnapshot {
            cpu_name: self.cpu_name.clone(),
            logical_cpus: self.logical_cpus,
            physical_cores: self.physical_cores,
            sockets: self.sockets,
            cpu_frequency_mhz: None,
            cpu_base_mhz: self.cpu_base_mhz,
            cpu_caches: self.cpu_caches,
            virtualization_firmware_enabled: self.virtualization_firmware_enabled,
            logical_processors: Vec::new(),
            memory: None,
            memory_modules: self.memory_modules.clone(),
            disks: Vec::new(),
            storage: Vec::new(),
            networks: Vec::new(),
            gpus: Vec::new(),
            gpu_adapters: Vec::new(),
            process_gpu: ProcessGpuSample::default(),
            thermal_zones: Vec::new(),
            uptime_seconds: unsafe { GetTickCount64() } / 1000,
            network_rx_bytes_per_sec: 0.0,
            network_tx_bytes_per_sec: 0.0,
            network_rates_ready: false,
            disk_read_bytes_per_sec: 0.0,
            disk_write_bytes_per_sec: 0.0,
            disk_rates_ready: false,
            disk_active_percent: None,
            gpu_percent: None,
            sample_ms: 0.0,
            gpu_sensor_ms: 0.0,
            warnings: self.setup_warnings.clone(),
        };
        match self.network_rates() {
            Ok(interfaces) => {
                let (rx, tx, ready) = network_totals(&interfaces);
                snapshot.network_rx_bytes_per_sec = rx;
                snapshot.network_tx_bytes_per_sec = tx;
                snapshot.network_rates_ready = ready;
                if !interfaces.iter().any(|interface| interface.connected) {
                    snapshot
                        .warnings
                        .push("Network: no active physical adapter".into());
                }
                snapshot.networks = interfaces;
            }
            Err(error) => {
                // Re-baseline after failures; never divide a multi-interval
                // counter delta by just the most recent interval.
                self.network_at = None;
                self.network.clear();
                snapshot.warnings.push(error);
            }
        }
        match memory_stats() {
            Ok(memory) => snapshot.memory = Some(memory),
            Err(error) => snapshot.warnings.push(error),
        }
        if let Some(pdh) = &mut self.pdh {
            pdh.sample(&mut snapshot);
        }
        if self.hardware {
            self.attach_storage(&mut snapshot);
            let sensors_started = Instant::now();
            self.attach_gpus(&mut snapshot);
            snapshot.gpu_sensor_ms = sensors_started.elapsed().as_secs_f64() * 1000.0;
        }
        snapshot.sample_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(snapshot)
    }

    /// Adds disk identities, re-querying the storage stack only when the set
    /// of PDH physical-disk instances changed (arrival, removal, new letters).
    fn attach_storage(&mut self, snapshot: &mut PerfSnapshot) {
        let ids: BTreeSet<String> = snapshot.disks.iter().map(|disk| disk.id.clone()).collect();
        if !ids.is_empty() && ids != self.storage.ids {
            let numbers = ids
                .iter()
                .filter_map(|id| storage::disk_number(id))
                .collect();
            let (devices, warnings) = storage::describe(&numbers);
            self.storage = StorageInventory {
                ids,
                devices: devices.into_iter().map(Arc::new).collect(),
                warnings,
            };
        }
        snapshot.storage = self.storage.devices.clone();
        snapshot
            .warnings
            .extend(self.storage.warnings.iter().cloned());
    }

    /// Matches PDH GPU adapters to D3DKMT adapters by LUID, fills names and
    /// static facts, and reads live sensors once per LUID.
    fn attach_gpus(&mut self, snapshot: &mut PerfSnapshot) {
        let parsed: Vec<_> = snapshot
            .gpus
            .iter()
            .map(|gpu| gpu::parse_pdh_adapter(&gpu.id))
            .collect();
        let luids: BTreeSet<(u32, u32)> = parsed.iter().flatten().map(|(luid, _)| *luid).collect();
        // Enumerate on first use, and again (rate-limited) when the set of
        // PDH adapters changed (arrival, removal, driver reset) or the last
        // enumeration failed.
        if self.gpu.enumerated_at.is_none()
            || (self.gpu.due() && (luids != self.gpu.pdh_luids || self.gpu.error.is_some()))
        {
            self.refresh_gpu_inventory(luids);
        }
        if let Some(error) = &self.gpu.error {
            snapshot.warnings.push(error.clone());
        }
        let mut requests: SensorRequests = BTreeMap::new();
        for (stats, parsed) in snapshot.gpus.iter_mut().zip(&parsed) {
            let Some((luid, physical)) = *parsed else {
                continue;
            };
            stats.physical_index = Some(physical);
            let Some(adapter) = self.gpu.adapters.iter().find(|a| a.luid == luid) else {
                continue;
            };
            stats.name = adapter.name.clone();
            if adapter.perf_data_supported {
                requests
                    .entry(luid)
                    .or_default()
                    .push((physical, adapter.caps(physical).copied()));
            }
            stats.adapter = Some(adapter.clone());
        }
        let mut failed = false;
        for (luid, physical) in requests {
            match gpu::sensors(luid, &physical) {
                Ok(readings) => {
                    for reading in readings {
                        if let Some(stats) =
                            snapshot
                                .gpus
                                .iter_mut()
                                .zip(&parsed)
                                .find_map(|(stats, parsed)| {
                                    (*parsed == Some((luid, reading.physical_index)))
                                        .then_some(stats)
                                })
                        {
                            stats.sensors = Some(reading);
                        }
                    }
                }
                Err(error) => {
                    failed = true;
                    snapshot.warnings.push(format!("GPU sensors: {error}"));
                }
            }
        }
        if failed && self.gpu.due() {
            // The adapter may have been removed or reset.
            let luids = std::mem::take(&mut self.gpu.pdh_luids);
            self.refresh_gpu_inventory(luids);
        }
        snapshot.gpu_adapters = self.gpu.adapters.clone();
    }

    fn refresh_gpu_inventory(&mut self, pdh_luids: BTreeSet<(u32, u32)>) {
        self.gpu.enumerated_at = Some(Instant::now());
        self.gpu.pdh_luids = pdh_luids;
        match gpu::enumerate() {
            Ok(adapters) => {
                self.gpu.adapters = adapters.into_iter().map(Arc::new).collect();
                self.gpu.error = None;
            }
            Err(error) => {
                // Stale facts could describe a removed adapter.
                self.gpu.adapters.clear();
                self.gpu.error = Some(error);
            }
        }
    }

    fn network_rates(&mut self) -> Result<Vec<NetworkStats>, String> {
        let mut raw = null_mut();
        let status = unsafe { GetIfTable2(&mut raw) };
        if status != 0 {
            return Err(format!("Network unavailable (Windows {status})"));
        }
        if raw.is_null() {
            return Err("Network returned an empty interface table".into());
        }
        let table = InterfaceTable(raw);
        let count = unsafe { (*table.0).NumEntries as usize };
        if count > MAX_INTERFACES {
            return Err("Network interface table exceeds the safety limit".into());
        }
        // Table is a Win32 flexible array with NumEntries valid native rows.
        let rows = unsafe { std::slice::from_raw_parts((*table.0).Table.as_ptr(), count) };
        let now = Instant::now();
        let seconds = self
            .network_at
            .map(|before| now.duration_since(before).as_secs_f64());
        self.generation = self.generation.wrapping_add(1);
        let mut interfaces = Vec::new();
        for row in rows {
            let flags = row.InterfaceAndOperStatusFlags._bitfield;
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK
                || flags & 1 == 0 // HardwareInterface
                || flags & 2 != 0
            // FilterInterface
            {
                continue;
            }
            let luid = unsafe { row.InterfaceLuid.Value };
            let connected = row.OperStatus == IfOperStatusUp;
            let previous = connected
                .then(|| {
                    self.network.insert(
                        luid,
                        NetworkCounters {
                            rx: row.InOctets,
                            tx: row.OutOctets,
                            generation: self.generation,
                        },
                    )
                })
                .flatten();
            let rates = network_delta(previous, row.InOctets, row.OutOctets, seconds);
            interfaces.push(NetworkStats {
                id: luid,
                name: utf16_text(&row.Alias),
                description: utf16_text(&row.Description),
                connected,
                present: row.OperStatus != IfOperStatusNotPresent,
                receive_link_bits_per_sec: row.ReceiveLinkSpeed,
                transmit_link_bits_per_sec: row.TransmitLinkSpeed,
                rx_bytes_per_sec: rates.map(|(rx, _)| rx),
                tx_bytes_per_sec: rates.map(|(_, tx)| tx),
                interface_type: row.Type,
                physical_medium: row.PhysicalMediumType,
                kind: NetworkKind::classify(row.Type, row.PhysicalMediumType),
            });
        }
        self.network
            .retain(|_, counters| counters.generation == self.generation);
        self.network_at = Some(now);
        interfaces.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        Ok(interfaces)
    }
}

struct InterfaceTable(*mut MIB_IF_TABLE2);

impl Drop for InterfaceTable {
    fn drop(&mut self) {
        unsafe { FreeMibTable(self.0.cast()) };
    }
}

struct PdhQuery {
    handle: PDH_HQUERY,
    cpu: Option<PDH_HCOUNTER>,
    cpu_frequency: Option<PDH_HCOUNTER>,
    disk_read: Option<PDH_HCOUNTER>,
    disk_write: Option<PDH_HCOUNTER>,
    disk_idle: Option<PDH_HCOUNTER>,
    gpu: Option<PDH_HCOUNTER>,
    gpu_dedicated: Option<PDH_HCOUNTER>,
    gpu_shared: Option<PDH_HCOUNTER>,
    gpu_process_dedicated: Option<PDH_HCOUNTER>,
    gpu_process_shared: Option<PDH_HCOUNTER>,
    /// `(counter, deci_kelvin)`: High Precision Temperature reports tenths of
    /// a kelvin; the older Temperature counter reports whole kelvins.
    thermal: Option<(PDH_HCOUNTER, bool)>,
    /// Aggregate per-process GPU values (false only for the baseline sampler).
    per_process: bool,
    // usize provides sufficient alignment for the array's pointers and f64s.
    array_buffer: Vec<usize>,
    primed: bool,
}

impl PdhQuery {
    fn new(warnings: &mut Vec<String>, hardware: bool) -> Option<Self> {
        let mut handle = null_mut();
        let status = unsafe { PdhOpenQueryW(null(), 0, &mut handle) };
        if status != 0 {
            warnings.push(format!(
                "Disk/GPU counters unavailable (PDH 0x{status:08X})"
            ));
            return None;
        }
        let mut query = Self {
            handle,
            cpu: None,
            cpu_frequency: None,
            disk_read: None,
            disk_write: None,
            disk_idle: None,
            gpu: None,
            gpu_dedicated: None,
            gpu_shared: None,
            gpu_process_dedicated: None,
            gpu_process_shared: None,
            thermal: None,
            per_process: hardware,
            array_buffer: vec![0; 2048],
            primed: false,
        };
        query.cpu = query.add(
            r"\Processor Information(*)\% Processor Time",
            "Logical CPU",
            warnings,
        );
        query.cpu_frequency = query.add(
            r"\Processor Information(*)\Processor Frequency",
            "CPU frequency",
            warnings,
        );
        query.disk_read = query.add(
            r"\PhysicalDisk(*)\Disk Read Bytes/sec",
            "Disk read",
            warnings,
        );
        query.disk_write = query.add(
            r"\PhysicalDisk(*)\Disk Write Bytes/sec",
            "Disk write",
            warnings,
        );
        query.disk_idle = query.add(r"\PhysicalDisk(*)\% Idle Time", "Disk activity", warnings);
        query.gpu = query.add(r"\GPU Engine(*)\Utilization Percentage", "GPU", warnings);
        query.gpu_dedicated = query.add(
            r"\GPU Adapter Memory(*)\Dedicated Usage",
            "GPU dedicated memory",
            warnings,
        );
        query.gpu_shared = query.add(
            r"\GPU Adapter Memory(*)\Shared Usage",
            "GPU shared memory",
            warnings,
        );
        if !hardware {
            return Some(query);
        }
        // "Local Usage" (resident in the adapter's local memory) rather than
        // "Dedicated Usage": the latter reports committed sizes far above the
        // adapter's memory for some processes (294 GiB on a 16 GiB card when
        // measured), while per-process Local/Shared Usage sum to the adapter
        // totals.
        query.gpu_process_dedicated = query.add(
            r"\GPU Process Memory(*)\Local Usage",
            "Process GPU dedicated memory",
            warnings,
        );
        query.gpu_process_shared = query.add(
            r"\GPU Process Memory(*)\Shared Usage",
            "Process GPU shared memory",
            warnings,
        );
        // Thermal zones are optional firmware objects; a missing counter set
        // is normal and not reported as a warning.
        let mut ignored = Vec::new();
        query.thermal = query
            .add(
                r"\Thermal Zone Information(*)\High Precision Temperature",
                "Thermal zones",
                &mut ignored,
            )
            .map(|counter| (counter, true))
            .or_else(|| {
                query
                    .add(
                        r"\Thermal Zone Information(*)\Temperature",
                        "Thermal zones",
                        &mut ignored,
                    )
                    .map(|counter| (counter, false))
            });
        Some(query)
    }

    fn add(&self, path: &str, label: &str, warnings: &mut Vec<String>) -> Option<PDH_HCOUNTER> {
        let mut counter = null_mut();
        let path = wide(path);
        let status = unsafe { PdhAddEnglishCounterW(self.handle, path.as_ptr(), 0, &mut counter) };
        if status == 0 {
            Some(counter)
        } else {
            warnings.push(format!("{label} unavailable (PDH 0x{status:08X})"));
            None
        }
    }

    fn sample(&mut self, snapshot: &mut PerfSnapshot) {
        let status = unsafe { PdhCollectQueryData(self.handle) };
        if status != 0 {
            self.primed = false;
            snapshot
                .warnings
                .push(format!("Disk/GPU sample unavailable (PDH 0x{status:08X})"));
            return;
        }
        if !self.primed {
            self.primed = true;
            // Rate counters require two samples. Leave unavailable gauges as
            // None rather than pretending the first zero is a measured value.
            return;
        }
        let cpus = self.optional_array(self.cpu, "Logical CPU", &mut snapshot.warnings);
        let frequency =
            self.optional_array(self.cpu_frequency, "CPU frequency", &mut snapshot.warnings);
        snapshot.logical_processors = logical_processors(&cpus);
        snapshot.cpu_frequency_mhz = average_cpu_frequency(&frequency);
        let reads = self.optional_array(self.disk_read, "Disk read", &mut snapshot.warnings);
        let writes = self.optional_array(self.disk_write, "Disk write", &mut snapshot.warnings);
        let idle = self.optional_array(self.disk_idle, "Disk activity", &mut snapshot.warnings);
        let read = counter_total(&reads);
        let write = counter_total(&writes);
        snapshot.disk_rates_ready = read.is_some() && write.is_some();
        snapshot.disk_read_bytes_per_sec = read.unwrap_or(0.0);
        snapshot.disk_write_bytes_per_sec = write.unwrap_or(0.0);
        snapshot.disk_active_percent = busiest_disk(&idle);
        snapshot.disks = disk_components(&reads, &writes, &idle);
        let mut pending_engines = Vec::new();
        let engines = self
            .gpu
            .map(|counter| self.array(counter, Some(&mut pending_engines)));
        let engines = match engines {
            Some(Ok(values)) => Some(values),
            Some(Err(error)) => {
                snapshot.warnings.push(format!("GPU engines: {error}"));
                None
            }
            None => None,
        };
        let dedicated = self.optional_array(
            self.gpu_dedicated,
            "GPU dedicated memory",
            &mut snapshot.warnings,
        );
        let shared =
            self.optional_array(self.gpu_shared, "GPU shared memory", &mut snapshot.warnings);
        // Parse each engine instance once for the adapter and process views.
        let parsed = engines.as_deref().map(engine_samples);
        snapshot.gpus = gpu_components(parsed.as_deref().unwrap_or_default(), &dedicated, &shared);
        snapshot.gpu_percent = snapshot
            .gpus
            .iter()
            .filter_map(|gpu| gpu.percent)
            .reduce(f64::max);
        let process_dedicated = self.measured_array(
            self.gpu_process_dedicated,
            "Process GPU dedicated memory",
            &mut snapshot.warnings,
        );
        let process_shared = self.measured_array(
            self.gpu_process_shared,
            "Process GPU shared memory",
            &mut snapshot.warnings,
        );
        if self.per_process {
            snapshot.process_gpu = process_gpu(
                parsed.as_deref(),
                &pending_engines,
                process_dedicated.as_deref(),
                process_shared.as_deref(),
            );
        }
        if let Some((counter, deci_kelvin)) = self.thermal {
            // No instances (common on desktops) is an empty list, not an error.
            if let Ok(values) = self.array(counter, None) {
                snapshot.thermal_zones = thermal_zones(&values, deci_kelvin);
            }
        }
    }

    fn optional_array(
        &mut self,
        counter: Option<PDH_HCOUNTER>,
        label: &str,
        warnings: &mut Vec<String>,
    ) -> Vec<(String, f64)> {
        self.measured_array(counter, label, warnings)
            .unwrap_or_default()
    }

    /// Like `optional_array`, but distinguishes "not measured" (`None`) from
    /// a measured array with no instances.
    fn measured_array(
        &mut self,
        counter: Option<PDH_HCOUNTER>,
        label: &str,
        warnings: &mut Vec<String>,
    ) -> Option<Vec<(String, f64)>> {
        match counter.map(|counter| self.array(counter, None)) {
            Some(Ok(values)) => Some(values),
            Some(Err(error)) => {
                warnings.push(format!("{label}: {error}"));
                None
            }
            None => None,
        }
    }

    /// Reads a formatted counter array. Instances whose value is not valid
    /// yet are skipped; their names are appended to `gaps` when requested.
    fn array(
        &mut self,
        counter: PDH_HCOUNTER,
        mut gaps: Option<&mut Vec<String>>,
    ) -> Result<Vec<(String, f64)>, String> {
        for _ in 0..4 {
            let capacity = self.array_buffer.len() * size_of::<usize>();
            let mut bytes = capacity as u32;
            let mut count = 0;
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    counter,
                    FMT_DOUBLE_UNCAPPED,
                    &mut bytes,
                    &mut count,
                    self.array_buffer.as_mut_ptr().cast(),
                )
            };
            if status == PDH_MORE_DATA {
                let required = bytes as usize;
                if required == 0 || required > MAX_PDH_BYTES {
                    return Err("counter array exceeds the safety limit".into());
                }
                self.array_buffer
                    .resize(required.div_ceil(size_of::<usize>()), 0);
                continue;
            }
            if status != 0 {
                return Err(format!("unavailable (PDH 0x{status:08X})"));
            }
            let used = bytes as usize;
            if used > capacity || count as usize > used / size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() {
                return Err("invalid counter array".into());
            }
            let items = unsafe {
                std::slice::from_raw_parts(
                    self.array_buffer
                        .as_ptr()
                        .cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                    count as usize,
                )
            };
            let mut values = Vec::with_capacity(items.len());
            if let Some(gaps) = gaps.as_deref_mut() {
                gaps.clear();
            }
            for item in items {
                let value = unsafe { item.FmtValue.Anonymous.doubleValue };
                if valid_status(item.FmtValue.CStatus) && value.is_finite() {
                    if let Some(name) = array_name(item.szName, &self.array_buffer, used) {
                        values.push((name, value));
                    }
                } else if let Some(gaps) = gaps.as_deref_mut() {
                    if let Some(name) = array_name(item.szName, &self.array_buffer, used) {
                        gaps.push(name);
                    }
                }
            }
            return Ok(values);
        }
        Err("counter instances changed repeatedly; retrying next refresh".into())
    }
}

impl Drop for PdhQuery {
    fn drop(&mut self) {
        // Closing a PDH query releases all counters belonging to that query.
        unsafe { PdhCloseQuery(self.handle) };
    }
}

fn valid_status(status: u32) -> bool {
    status == PDH_CSTATUS_VALID_DATA || status == PDH_CSTATUS_NEW_DATA
}

fn array_name(pointer: *const u16, buffer: &[usize], used: usize) -> Option<String> {
    let base = buffer.as_ptr() as usize;
    let start = pointer as usize;
    let end = base.checked_add(used.min(std::mem::size_of_val(buffer)))?;
    if start < base || start >= end || !start.is_multiple_of(2) {
        return None;
    }
    let length = ((end - start) / 2).min(1024);
    let text = unsafe { std::slice::from_raw_parts(pointer, length) };
    let nul = text.iter().position(|&c| c == 0)?;
    Some(String::from_utf16_lossy(&text[..nul]))
}

fn byte_rate(current: u64, previous: u64, seconds: f64) -> f64 {
    if !seconds.is_finite() || seconds <= 0.0 {
        return 0.0;
    }
    // Driver resets and interface replacement must not become a huge spike.
    current
        .checked_sub(previous)
        .map(|delta| delta as f64 / seconds)
        .filter(|value| value.is_finite())
        .unwrap_or(0.0)
}

fn network_delta(
    previous: Option<NetworkCounters>,
    rx: u64,
    tx: u64,
    seconds: Option<f64>,
) -> Option<(f64, f64)> {
    let previous = previous?;
    let seconds = seconds.filter(|seconds| seconds.is_finite() && *seconds > 0.0)?;
    if rx < previous.rx || tx < previous.tx {
        return None;
    }
    Some((
        byte_rate(rx, previous.rx, seconds),
        byte_rate(tx, previous.tx, seconds),
    ))
}

fn network_totals(interfaces: &[NetworkStats]) -> (f64, f64, bool) {
    let mut active = 0;
    let mut measured = 0;
    let mut rx = 0.0;
    let mut tx = 0.0;
    for interface in interfaces.iter().filter(|interface| interface.connected) {
        active += 1;
        if let (Some(receive), Some(send)) =
            (interface.rx_bytes_per_sec, interface.tx_bytes_per_sec)
        {
            rx += receive;
            tx += send;
            measured += 1;
        }
    }
    // An adapter arrival/reset is a gap in the aggregate; independently measured
    // adapters retain their valid rates in the per-device list.
    (rx, tx, active > 0 && measured == active)
}

fn processor_id(name: &str) -> Option<(u32, u32)> {
    let (group, index) = name.split_once(',')?;
    Some((group.parse().ok()?, index.parse().ok()?))
}

fn logical_processors(values: &[(String, f64)]) -> Vec<LogicalProcessor> {
    let mut processors = BTreeMap::new();
    for (name, value) in values {
        if let Some((group, index)) = processor_id(name) {
            if value.is_finite() {
                processors.insert(
                    (group, index),
                    LogicalProcessor {
                        id: name.clone(),
                        group,
                        index,
                        percent: Some(value.clamp(0.0, 100.0)),
                    },
                );
            }
        }
    }
    processors.into_values().collect()
}

fn average_cpu_frequency(values: &[(String, f64)]) -> Option<f64> {
    let frequencies: Vec<_> = values
        .iter()
        .filter(|(name, value)| processor_id(name).is_some() && value.is_finite() && *value > 0.0)
        .collect();
    (!frequencies.is_empty()).then(|| {
        frequencies.iter().map(|(_, value)| *value).sum::<f64>() / frequencies.len() as f64
    })
}

fn counter_total(values: &[(String, f64)]) -> Option<f64> {
    values
        .iter()
        .find(|(name, value)| name == "_Total" && value.is_finite() && *value >= 0.0)
        .map(|(_, value)| *value)
}

fn disk_components(
    reads: &[(String, f64)],
    writes: &[(String, f64)],
    idle: &[(String, f64)],
) -> Vec<DiskStats> {
    let mut disks: BTreeMap<String, DiskStats> = BTreeMap::new();
    for (kind, values) in [reads, writes, idle].into_iter().enumerate() {
        for (name, value) in values {
            if name == "_Total"
                || name.is_empty()
                || !value.is_finite()
                || (kind != 2 && *value < 0.0)
            {
                continue;
            }
            let disk = disks.entry(name.clone()).or_insert_with(|| DiskStats {
                id: name.clone(),
                read_bytes_per_sec: None,
                write_bytes_per_sec: None,
                active_percent: None,
            });
            match kind {
                0 => disk.read_bytes_per_sec = Some(*value),
                1 => disk.write_bytes_per_sec = Some(*value),
                _ => disk.active_percent = Some((100.0 - value).clamp(0.0, 100.0)),
            }
        }
    }
    let mut result: Vec<_> = disks.into_values().collect();
    result.sort_by(|a, b| {
        let number = |name: &str| {
            name.split_whitespace()
                .next()
                .and_then(|n| n.parse::<u32>().ok())
        };
        number(&a.id).cmp(&number(&b.id)).then(a.id.cmp(&b.id))
    });
    result
}

fn gpu_adapter_id(name: &str) -> Option<&str> {
    let name = name.split('#').next()?;
    let body = name.strip_prefix("luid_")?;
    let (luid, physical) = body.split_once("_phys_")?;
    let (high, low) = luid.split_once('_')?;
    u32::from_str_radix(high.strip_prefix("0x")?, 16).ok()?;
    u32::from_str_radix(low.strip_prefix("0x")?, 16).ok()?;
    physical.parse::<u32>().ok()?;
    Some(name)
}

/// Parses `pid_P_luid_…_phys_N_eng_E_engtype_T` into `(P, adapter id, E, T)`.
fn gpu_engine_id(name: &str) -> Option<(u32, &str, &str, &str)> {
    let name = name.split('#').next()?;
    let (pid, adapter_engine) = name.split_once("_luid_")?;
    let pid = pid.strip_prefix("pid_")?.parse::<u32>().ok()?;
    let (adapter, engine) = adapter_engine.split_once("_eng_")?;
    // Prefix is already part of the original string, so use a slice to avoid
    // allocating another adapter ID per process/engine instance.
    let adapter_start = name.find("luid_")?;
    let adapter_id = &name[adapter_start..adapter_start + "luid_".len() + adapter.len()];
    gpu_adapter_id(adapter_id)?;
    let (index, kind) = engine.split_once("_engtype_")?;
    index.parse::<u32>().ok()?;
    if kind.is_empty() {
        return None;
    }
    Some((pid, adapter_id, index, kind))
}

/// One valid, parsed `GPU Engine` counter instance.
#[derive(Clone, Copy, Debug)]
struct EngineSample<'a> {
    pid: u32,
    adapter: &'a str,
    index: &'a str,
    kind: &'a str,
    value: f64,
}

/// Parses engine instances, dropping malformed names and invalid values.
fn engine_samples(values: &[(String, f64)]) -> Vec<EngineSample<'_>> {
    values
        .iter()
        .filter(|(_, value)| value.is_finite() && *value >= 0.0)
        .filter_map(|(name, value)| {
            let (pid, adapter, index, kind) = gpu_engine_id(name)?;
            Some(EngineSample {
                pid,
                adapter,
                index,
                kind,
                value: *value,
            })
        })
        .collect()
}

fn gpu_components(
    values: &[EngineSample<'_>],
    dedicated: &[(String, f64)],
    shared: &[(String, f64)],
) -> Vec<GpuStats> {
    let mut adapters: BTreeMap<String, GpuStats> = BTreeMap::new();
    let make_gpu = |id: &str| GpuStats {
        id: id.to_owned(),
        name: None,
        percent: None,
        dedicated_bytes: None,
        shared_bytes: None,
        engines: Vec::new(),
        physical_index: None,
        adapter: None,
        sensors: None,
    };
    // Keys borrow from the counter names; only the per-engine results below
    // are allocated.
    let mut engines: BTreeMap<(&str, &str, &str), f64> = BTreeMap::new();
    for sample in values {
        *engines
            .entry((sample.adapter, sample.index, sample.kind))
            .or_default() += sample.value;
    }
    for ((adapter, index, kind), value) in engines {
        let gpu = adapters
            .entry(adapter.to_owned())
            .or_insert_with(|| make_gpu(adapter));
        let percent = value.clamp(0.0, 100.0);
        gpu.percent = Some(
            gpu.percent
                .map_or(percent, |previous| previous.max(percent)),
        );
        gpu.engines.push(GpuEngine {
            id: index.to_owned(),
            name: kind.to_owned(),
            percent,
        });
    }
    for (kind, values) in [dedicated, shared].into_iter().enumerate() {
        for (name, value) in values {
            if !value.is_finite() || *value < 0.0 || *value >= u64::MAX as f64 {
                continue;
            }
            if let Some(adapter) = gpu_adapter_id(name) {
                let gpu = adapters
                    .entry(adapter.to_owned())
                    .or_insert_with(|| make_gpu(adapter));
                if kind == 0 {
                    gpu.dedicated_bytes = Some(*value as u64);
                } else {
                    gpu.shared_bytes = Some(*value as u64);
                }
            }
        }
    }
    adapters.into_values().collect()
}

/// PID of a PDH GPU instance (`pid_1234_luid_…`).
fn gpu_instance_pid(name: &str) -> Option<u32> {
    let (pid, _) = name.strip_prefix("pid_")?.split_once("_luid_")?;
    pid.parse().ok()
}

/// Aggregates per-process GPU values (see [`ProcessGpu`] for the rule).
/// `engines`/`dedicated`/`shared` are `None` when that array was not measured.
fn process_gpu(
    engines: Option<&[EngineSample<'_>]>,
    pending: &[String],
    dedicated: Option<&[(String, f64)]>,
    shared: Option<&[(String, f64)]>,
) -> ProcessGpuSample {
    let mut sample = ProcessGpuSample {
        utilization_measured: engines.is_some(),
        memory_measured: dedicated.is_some() && shared.is_some(),
        ..Default::default()
    };
    let engines = engines.unwrap_or_default();
    // (pid, adapter, engine index) -> summed instances and engine type.
    let mut per_engine: HashMap<(u32, &str, &str), (f64, &str)> =
        HashMap::with_capacity(engines.len());
    for sample in engines {
        per_engine
            .entry((sample.pid, sample.adapter, sample.index))
            .or_insert((0.0, sample.kind))
            .0 += sample.value;
    }
    for ((pid, adapter, _), (value, kind)) in per_engine {
        let percent = value.clamp(0.0, 100.0);
        let entry = sample.by_pid.entry(pid).or_default();
        let busier = entry.percent.is_none_or(|previous| percent > previous);
        if busier {
            entry.percent = Some(percent);
            entry.engine = (percent > 0.0).then(|| ProcessGpuEngine {
                adapter: adapter.to_owned(),
                engine_type: kind.to_owned(),
            });
        }
    }
    sample.pending = pending
        .iter()
        .filter_map(|name| gpu_instance_pid(name))
        .collect();
    let memory = [(dedicated, true), (shared, false)];
    for (values, is_dedicated) in memory {
        for (name, value) in values.unwrap_or_default() {
            if !value.is_finite() || *value < 0.0 || *value >= u64::MAX as f64 {
                continue;
            }
            let Some(pid) = gpu_instance_pid(name) else {
                continue;
            };
            if name
                .find("luid_")
                .and_then(|start| gpu_adapter_id(&name[start..]))
                .is_none()
            {
                continue;
            }
            let entry = sample.by_pid.entry(pid).or_default();
            let slot = if is_dedicated {
                &mut entry.dedicated_bytes
            } else {
                &mut entry.shared_bytes
            };
            *slot = Some(slot.unwrap_or(0).saturating_add(*value as u64));
        }
    }
    sample
}

/// Converts thermal zone readings to Celsius, dropping implausible values.
fn thermal_zones(values: &[(String, f64)], deci_kelvin: bool) -> Vec<ThermalZone> {
    let mut zones: Vec<_> = values
        .iter()
        .filter(|(name, _)| !name.is_empty() && name != "_Total")
        .filter_map(|(name, value)| {
            let kelvin = if deci_kelvin { value / 10.0 } else { *value };
            let celsius = kelvin - KELVIN_OFFSET;
            (celsius.is_finite() && celsius > 0.0 && celsius <= THERMAL_MAX_CELSIUS).then(|| {
                ThermalZone {
                    name: name.clone(),
                    celsius,
                }
            })
        })
        .collect();
    zones.sort_by(|a, b| a.name.cmp(&b.name));
    zones
}

fn memory_stats() -> Result<MemoryStats, String> {
    let mut info = PERFORMANCE_INFORMATION::default();
    let size = size_of::<PERFORMANCE_INFORMATION>() as u32;
    info.cb = size;
    if unsafe { GetPerformanceInfo(&mut info, size) } == 0 {
        return Err(format!(
            "Memory detail unavailable: {}",
            std::io::Error::last_os_error()
        ));
    }
    let bytes = |pages: usize| {
        (pages as u64)
            .checked_mul(info.PageSize as u64)
            .ok_or_else(|| "Memory size overflow".to_owned())
    };
    if info.PageSize == 0 {
        return Err("Memory page size unavailable".into());
    }
    Ok(MemoryStats {
        physical_total: bytes(info.PhysicalTotal)?,
        physical_available: bytes(info.PhysicalAvailable)?,
        commit_used: bytes(info.CommitTotal)?,
        commit_limit: bytes(info.CommitLimit)?,
        commit_peak: bytes(info.CommitPeak)?,
        cache: bytes(info.SystemCache)?,
        kernel_paged: bytes(info.KernelPaged)?,
        kernel_nonpaged: bytes(info.KernelNonpaged)?,
        processes: info.ProcessCount,
        threads: info.ThreadCount,
        handles: info.HandleCount,
    })
}

fn cpu_topology() -> Result<(u32, u32, Option<CpuCaches>), String> {
    let mut bytes = 0u32;
    unsafe { GetLogicalProcessorInformationEx(RelationAll, null_mut(), &mut bytes) };
    for _ in 0..3 {
        if !(8..=16 * 1024 * 1024).contains(&bytes) {
            return Err("CPU topology size unavailable".into());
        }
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        let mut used = (buffer.len() * size_of::<usize>()) as u32;
        if unsafe {
            GetLogicalProcessorInformationEx(RelationAll, buffer.as_mut_ptr().cast(), &mut used)
        } != 0
        {
            if used as usize > buffer.len() * size_of::<usize>() {
                return Err("Invalid CPU topology length".into());
            }
            let data =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), used as usize) };
            let (cores, sockets) = topology_counts(data)?;
            return Ok((cores, sockets, cache_totals(data)));
        }
        if used <= bytes {
            return Err(format!(
                "CPU topology unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
        bytes = used;
    }
    Err("CPU topology changed repeatedly".into())
}

fn topology_counts(mut data: &[u8]) -> Result<(u32, u32), String> {
    let mut cores = 0;
    let mut sockets = 0;
    while !data.is_empty() {
        let header = data
            .get(..8)
            .ok_or_else(|| "Truncated CPU topology".to_owned())?;
        let relationship = u32::from_le_bytes(header[..4].try_into().unwrap());
        let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        if size < 8 || size > data.len() || !size.is_multiple_of(8) {
            return Err("Invalid CPU topology record".into());
        }
        match relationship {
            0 => cores += 1,   // RelationProcessorCore
            3 => sockets += 1, // RelationProcessorPackage
            _ => {}
        }
        data = &data[size..];
    }
    if cores == 0 || sockets == 0 {
        return Err("CPU topology is incomplete".into());
    }
    Ok((cores, sockets))
}

/// Sums `CACHE_RELATIONSHIP` records (relation 2) per level. Each record is
/// one physical cache; record layout: Level at +8, CacheSize at +12.
fn cache_totals(mut data: &[u8]) -> Option<CpuCaches> {
    let mut caches = CpuCaches::default();
    let mut found = false;
    while data.len() >= 8 {
        let relationship = u32::from_le_bytes(data[..4].try_into().ok()?);
        let size = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
        if size < 8 || size > data.len() {
            return None;
        }
        if relationship == 2 && size >= 16 {
            let level = data[8];
            let bytes = u64::from(u32::from_le_bytes(data[12..16].try_into().ok()?));
            let slot = match level {
                1 => &mut caches.l1_bytes,
                2 => &mut caches.l2_bytes,
                3 => &mut caches.l3_bytes,
                _ => {
                    data = &data[size..];
                    continue;
                }
            };
            *slot = Some(slot.unwrap_or(0).saturating_add(bytes));
            found = true;
        }
        data = &data[size..];
    }
    found.then_some(caches)
}

/// Highest nominal maximum frequency (`PROCESSOR_POWER_INFORMATION.MaxMhz`)
/// across logical processors, in MHz. Task Manager shows this as "Base speed".
fn cpu_base_mhz(logical: u32) -> Result<Option<u32>, String> {
    let mut count = (logical as usize).max(1);
    for _ in 0..4 {
        let mut info = vec![PROCESSOR_POWER_INFORMATION::default(); count];
        let bytes = std::mem::size_of_val(info.as_slice()) as u32;
        let status = unsafe {
            CallNtPowerInformation(
                ProcessorInformation,
                null(),
                0,
                info.as_mut_ptr().cast(),
                bytes,
            )
        };
        if status == STATUS_BUFFER_TOO_SMALL && count < 4096 {
            count *= 2;
            continue;
        }
        if status < 0 {
            return Err(format!(
                "CPU base speed unavailable (NTSTATUS 0x{:08X})",
                status as u32
            ));
        }
        return Ok(info
            .iter()
            .map(|cpu| cpu.MaxMhz)
            .filter(|&mhz| mhz > 0)
            .max());
    }
    Err("CPU base speed unavailable".into())
}

fn utf16_text(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn busiest_disk(values: &[(String, f64)]) -> Option<f64> {
    values
        .iter()
        .filter(|(name, idle)| name != "_Total" && idle.is_finite())
        .map(|(_, idle)| (100.0 - idle).clamp(0.0, 100.0))
        .reduce(f64::max)
}

fn cpu_name() -> Result<String, String> {
    let key = wide(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
    let value = wide("ProcessorNameString");
    let mut buffer = [0u16; 512];
    let mut bytes = std::mem::size_of_val(&buffer) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 || bytes as usize > std::mem::size_of_val(&buffer) || !bytes.is_multiple_of(2) {
        return Err(format!("CPU model unavailable (Windows {status})"));
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    let name = String::from_utf16_lossy(&buffer[..end]).trim().to_owned();
    if name.is_empty() {
        Err("CPU model unavailable".into())
    } else {
        Ok(name)
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_handles_resets_and_invalid_intervals() {
        assert_eq!(byte_rate(4096, 1024, 0.5), 6144.0);
        assert_eq!(byte_rate(5, u64::MAX, 1.0), 0.0);
        assert_eq!(byte_rate(4096, 1024, 0.0), 0.0);
        assert_eq!(byte_rate(4096, 1024, f64::NAN), 0.0);
    }

    #[test]
    fn busiest_disk_excludes_total_and_clamps() {
        let values = vec![
            ("_Total".into(), 0.0),
            ("0 C:".into(), 60.0),
            ("1 D:".into(), 15.0),
        ];
        assert_eq!(busiest_disk(&values), Some(85.0));
        assert_eq!(busiest_disk(&[("0".into(), 101.0)]), Some(0.0));
        assert_eq!(busiest_disk(&[("0".into(), -2.0)]), Some(100.0));
        assert_eq!(busiest_disk(&[]), None);
    }

    #[test]
    fn gpu_sums_processes_but_not_separate_engines_or_adapters() {
        let busiest_gpu_engine = |values: &[(String, f64)]| {
            gpu_components(&engine_samples(values), &[], &[])
                .iter()
                .filter_map(|gpu| gpu.percent)
                .reduce(f64::max)
        };
        let name = |pid, adapter, engine| {
            format!("pid_{pid}_luid_0x00000000_0x{adapter:08x}_phys_0_eng_{engine}_engtype_3D")
        };
        let values = vec![
            (name(10, 1, 0), 25.0),
            (name(20, 1, 0), 35.0),
            (name(10, 1, 1), 50.0),
            (name(10, 2, 0), 40.0),
        ];
        assert_eq!(busiest_gpu_engine(&values), Some(60.0));
        assert_eq!(busiest_gpu_engine(&[(name(10, 1, 0), 120.0)]), Some(100.0));
        assert_eq!(busiest_gpu_engine(&[("invalid".into(), 50.0)]), None);
    }

    #[test]
    fn per_disk_fields_preserve_gaps_and_exclude_aggregate() {
        let disks = disk_components(
            &[
                ("_Total".into(), 250.0),
                ("2 D:".into(), 200.0),
                ("10 E:".into(), 50.0),
            ],
            &[("2 D:".into(), 300.0)],
            &[("2 D:".into(), 20.0), ("10 E:".into(), f64::NAN)],
        );
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].id, "2 D:");
        assert_eq!(disks[0].read_bytes_per_sec, Some(200.0));
        assert_eq!(disks[0].write_bytes_per_sec, Some(300.0));
        assert_eq!(disks[0].active_percent, Some(80.0));
        assert_eq!(disks[1].write_bytes_per_sec, None);
        assert_eq!(disks[1].active_percent, None);
    }

    #[test]
    fn per_cpu_samples_exclude_group_totals_and_sort_numerically() {
        let values = vec![
            ("_Total".into(), 12.0),
            ("0,_Total".into(), 12.0),
            ("1,0".into(), 101.0),
            ("0,10".into(), 10.0),
            ("0,2".into(), 2.0),
        ];
        let processors = logical_processors(&values);
        assert_eq!(
            processors
                .iter()
                .map(|cpu| cpu.id.as_str())
                .collect::<Vec<_>>(),
            ["0,2", "0,10", "1,0"]
        );
        assert_eq!(processors[2].percent, Some(100.0));
        assert_eq!(
            average_cpu_frequency(&[
                ("_Total".into(), 9000.0),
                ("0,0".into(), 2000.0),
                ("0,1".into(), 3000.0)
            ]),
            Some(2500.0)
        );
        assert_eq!(average_cpu_frequency(&[("0,0".into(), 0.0)]), None);
    }

    #[test]
    fn gpu_components_keep_adapter_memory_separate_and_do_not_invent_names() {
        let id = "luid_0x00000000_0x00000001_phys_0";
        let engines = vec![
            (format!("pid_1_{id}_eng_0_engtype_3D"), 10.0),
            (format!("pid_2_{id}_eng_0_engtype_3D"), 20.0),
            (format!("pid_2_{id}_eng_1_engtype_Copy"), 15.0),
        ];
        let gpus = gpu_components(
            &engine_samples(&engines),
            &[(id.into(), 4096.0)],
            &[(id.into(), 2048.0)],
        );
        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].id, id);
        assert!(gpus[0].name.is_none());
        assert_eq!(gpus[0].percent, Some(30.0));
        assert_eq!(gpus[0].dedicated_bytes, Some(4096));
        assert_eq!(gpus[0].shared_bytes, Some(2048));
        assert_eq!(gpus[0].engines.len(), 2);
        let memory_only = gpu_components(&[], &[(id.into(), 512.0)], &[]);
        assert_eq!(memory_only[0].percent, None);
        assert_eq!(memory_only[0].shared_bytes, None);
        assert!(gpu_components(
            &engine_samples(&[("pid_bad_luid_0x0_0x1_phys_0_eng_0_engtype_3D".into(), 20.0)]),
            &[("unknown".into(), 10.0)],
            &[]
        )
        .is_empty());
    }

    #[test]
    fn nic_arrival_reset_and_disconnect_do_not_claim_measured_zero() {
        let before = NetworkCounters {
            rx: 100,
            tx: 200,
            generation: 1,
        };
        assert_eq!(
            network_delta(Some(before), 300, 300, Some(2.0)),
            Some((100.0, 50.0))
        );
        assert_eq!(network_delta(None, 300, 300, Some(2.0)), None);
        assert_eq!(network_delta(Some(before), 99, 300, Some(2.0)), None);
        assert_eq!(network_delta(Some(before), 300, 300, None), None);
        let interface = |id, connected, rx, tx| NetworkStats {
            id,
            name: String::new(),
            description: String::new(),
            connected,
            present: true,
            receive_link_bits_per_sec: 0,
            transmit_link_bits_per_sec: 0,
            rx_bytes_per_sec: rx,
            tx_bytes_per_sec: tx,
            interface_type: IF_TYPE_ETHERNET_CSMACD,
            physical_medium: 14,
            kind: NetworkKind::Ethernet,
        };
        assert_eq!(
            network_totals(&[
                interface(1, true, Some(100.0), Some(50.0)),
                interface(2, false, None, None)
            ]),
            (100.0, 50.0, true)
        );
        assert_eq!(
            network_totals(&[
                interface(1, true, Some(100.0), Some(50.0)),
                interface(2, true, None, None)
            ]),
            (100.0, 50.0, false)
        );
        assert_eq!(network_totals(&[]), (0.0, 0.0, false));
    }

    fn engine(pid: u32, adapter: u32, engine: u32, kind: &str) -> String {
        format!("pid_{pid}_luid_0x00000000_0x{adapter:08X}_phys_0_eng_{engine}_engtype_{kind}")
    }

    #[test]
    fn process_gpu_takes_busiest_engine_after_summing_instances() {
        let engines = vec![
            (engine(10, 1, 0, "3D"), 20.0),
            (format!("{}#1", engine(10, 1, 0, "3D")), 15.0),
            (engine(10, 1, 1, "Copy"), 30.0),
            (engine(10, 2, 0, "3D"), 12.0),
            (engine(20, 1, 0, "3D"), 80.0),
            (engine(20, 1, 2, "VideoDecode"), 90.0),
            (engine(30, 1, 0, "3D"), 0.0),
            (engine(40, 1, 0, "3D"), 250.0),
            ("pid_x_luid_0x0_0x1_phys_0_eng_0_engtype_3D".into(), 50.0),
            (engine(50, 1, 0, "3D"), f64::NAN),
        ];
        let memory = vec![
            ("pid_10_luid_0x00000000_0x00000001_phys_0".into(), 1024.0),
            ("pid_10_luid_0x00000000_0x00000002_phys_0".into(), 512.0),
            ("pid_60_luid_0x00000000_0x00000001_phys_0".into(), 4096.0),
            ("pid_70_unknown".into(), 8.0),
        ];
        let pending = vec![engine(80, 1, 0, "3D")];
        let parsed = engine_samples(&engines);
        let sample = process_gpu(Some(&parsed), &pending, Some(&memory), Some(&[]));
        assert!(sample.utilization_measured && sample.memory_measured);
        let p10 = &sample.by_pid[&10];
        assert_eq!(p10.percent, Some(35.0));
        assert_eq!(
            p10.engine,
            Some(ProcessGpuEngine {
                adapter: "luid_0x00000000_0x00000001_phys_0".into(),
                engine_type: "3D".into(),
            })
        );
        assert_eq!(p10.dedicated_bytes, Some(1536));
        assert_eq!(p10.shared_bytes, None);
        assert_eq!(sample.by_pid[&20].percent, Some(90.0));
        assert_eq!(
            sample.by_pid[&20].engine.as_ref().unwrap().engine_type,
            "VideoDecode"
        );
        assert_eq!(sample.by_pid[&30].percent, Some(0.0));
        assert_eq!(sample.by_pid[&30].engine, None);
        assert_eq!(sample.by_pid[&40].percent, Some(100.0));
        assert!(!sample.by_pid.contains_key(&50));
        assert!(!sample.by_pid.contains_key(&70));
        assert_eq!(sample.by_pid[&60].percent, None);
        assert_eq!(sample.pending, HashSet::from([80]));
        let unmeasured = process_gpu(None, &[], None, Some(&[]));
        assert!(!unmeasured.utilization_measured && !unmeasured.memory_measured);
        assert!(unmeasured.by_pid.is_empty());
    }

    fn process(pid: u32, created: u64) -> Process {
        Process {
            pid,
            parent_pid: 0,
            name: String::new(),
            created,
            cpu_percent: 0.0,
            working_set: 0,
            private_bytes: 0,
            io_bytes_per_sec: 0.0,
            gpu_percent: None,
            network_bytes_per_sec: None,
            threads: 1,
            handles: 0,
        }
    }

    fn perf_with(process_gpu: ProcessGpuSample) -> PerfSnapshot {
        PerfSnapshot {
            cpu_name: String::new(),
            logical_cpus: 1,
            physical_cores: None,
            sockets: None,
            cpu_frequency_mhz: None,
            cpu_base_mhz: None,
            cpu_caches: None,
            virtualization_firmware_enabled: false,
            logical_processors: Vec::new(),
            memory: None,
            memory_modules: None,
            disks: Vec::new(),
            storage: Vec::new(),
            networks: Vec::new(),
            gpus: Vec::new(),
            gpu_adapters: Vec::new(),
            process_gpu,
            thermal_zones: Vec::new(),
            uptime_seconds: 0,
            network_rx_bytes_per_sec: 0.0,
            network_tx_bytes_per_sec: 0.0,
            network_rates_ready: false,
            disk_read_bytes_per_sec: 0.0,
            disk_write_bytes_per_sec: 0.0,
            disk_rates_ready: false,
            disk_active_percent: None,
            gpu_percent: None,
            sample_ms: 0.0,
            gpu_sensor_ms: 0.0,
            warnings: Vec::new(),
        }
    }

    #[test]
    fn tracker_never_attributes_rates_across_pid_reuse_or_new_processes() {
        let busy = ProcessGpu {
            percent: Some(40.0),
            dedicated_bytes: Some(100),
            ..Default::default()
        };
        let sample = ProcessGpuSample {
            utilization_measured: true,
            memory_measured: true,
            by_pid: HashMap::from([(1, busy.clone()), (2, busy.clone()), (4, busy)]),
            pending: HashSet::from([4]),
        };
        let perf = perf_with(sample);
        let mut tracker = ProcessGpuTracker::default();
        let first = [process(1, 100), process(2, 200), process(4, 400)];
        // Nothing continues from before the first join: no utilization yet.
        let joined = tracker.join(&first, Some(&perf));
        assert!(joined.values().all(|value| value.percent.is_none()));
        assert_eq!(joined[&(1, 100)].dedicated_bytes, Some(100));
        // PID 2 is reused (new creation time) and PID 3 is new.
        let second = [
            process(1, 100),
            process(2, 201),
            process(3, 300),
            process(4, 400),
        ];
        let joined = tracker.join(&second, Some(&perf));
        assert_eq!(joined[&(1, 100)].percent, Some(40.0));
        assert_eq!(joined[&(2, 201)].percent, None);
        assert_eq!(joined[&(2, 201)].dedicated_bytes, Some(100));
        // No GPU instances while measured: a measured zero.
        assert_eq!(joined[&(3, 300)].percent, None);
        assert_eq!(joined[&(3, 300)].dedicated_bytes, Some(0));
        // Pending instances have no value yet.
        assert_eq!(joined[&(4, 400)].percent, None);
        let joined = tracker.join(&second, Some(&perf));
        assert_eq!(joined[&(3, 300)].percent, Some(0.0));
        // Unmeasured arrays report nothing, and no snapshot means no values.
        let unmeasured = perf_with(ProcessGpuSample::default());
        assert!(tracker.join(&second, Some(&unmeasured)).is_empty());
        assert!(tracker.join(&second, None).is_empty());
    }

    #[test]
    fn thermal_zones_convert_kelvin_and_drop_implausible_readings() {
        let values = vec![
            (r"\_TZ.TZ01".to_string(), 3131.5),
            (r"\_TZ.TZ00".to_string(), 2731.5),
            (r"\_TZ.HOT".to_string(), 4500.0),
            ("_Total".to_string(), 3000.0),
            (r"\_TZ.NAN".to_string(), f64::NAN),
        ];
        let zones = thermal_zones(&values, true);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].name, r"\_TZ.TZ01");
        assert!((zones[0].celsius - 40.0).abs() < 1e-9);
        let zones = thermal_zones(&[(r"\_TZ.A".into(), 318.15)], false);
        assert!((zones[0].celsius - 45.0).abs() < 1e-9);
        assert!(thermal_zones(&[(r"\_TZ.A".into(), 0.0)], false).is_empty());
    }

    #[test]
    fn cache_records_sum_per_level_and_reject_bad_sizes() {
        let cache = |level: u8, bytes: u32| {
            let mut record = vec![0u8; 48];
            record[..4].copy_from_slice(&2u32.to_le_bytes());
            record[4..8].copy_from_slice(&48u32.to_le_bytes());
            record[8] = level;
            record[12..16].copy_from_slice(&bytes.to_le_bytes());
            record
        };
        let core = [0u32.to_le_bytes(), 8u32.to_le_bytes()].concat();
        let data = [
            cache(1, 32 * 1024),
            cache(1, 48 * 1024),
            core.clone(),
            cache(2, 1024 * 1024),
            cache(3, 32 * 1024 * 1024),
            cache(4, 1),
        ]
        .concat();
        assert_eq!(
            cache_totals(&data),
            Some(CpuCaches {
                l1_bytes: Some(80 * 1024),
                l2_bytes: Some(1024 * 1024),
                l3_bytes: Some(32 * 1024 * 1024),
            })
        );
        assert_eq!(cache_totals(&core), None);
        assert_eq!(cache_totals(&data[..data.len() - 1]), None);
        let mut bad = cache(1, 1);
        bad[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(cache_totals(&bad), None);
    }

    #[test]
    fn network_kind_prefers_physical_medium() {
        assert_eq!(NetworkKind::classify(71, 9), NetworkKind::WiFi);
        assert_eq!(NetworkKind::classify(6, 14), NetworkKind::Ethernet);
        // Bluetooth PAN reports an Ethernet ifType.
        assert_eq!(NetworkKind::classify(6, 10), NetworkKind::Bluetooth);
        assert_eq!(NetworkKind::classify(243, 0), NetworkKind::MobileBroadband);
        assert_eq!(NetworkKind::classify(6, 0), NetworkKind::Ethernet);
        assert_eq!(NetworkKind::classify(131, 0), NetworkKind::Other);
    }

    #[test]
    fn topology_records_are_bounded_and_require_cores_and_packages() {
        let record =
            |relation: u32, size: u32| [relation.to_le_bytes(), size.to_le_bytes()].concat();
        let records = [record(0, 8), record(0, 8), record(3, 8)].concat();
        assert_eq!(topology_counts(&records), Ok((2, 1)));
        assert!(topology_counts(&record(0, 8)).is_err());
        assert!(topology_counts(&record(0, 0)).is_err());
        assert!(topology_counts(&record(0, u32::MAX)).is_err());
        assert!(topology_counts(&records[..records.len() - 1]).is_err());
    }

    #[test]
    fn names_must_point_inside_pdh_buffer_and_be_terminated() {
        let mut buffer = vec![0usize; 4];
        let ptr = buffer.as_mut_ptr().cast::<u16>();
        unsafe { *ptr = 'A' as u16 };
        assert_eq!(array_name(ptr, &buffer, 8).as_deref(), Some("A"));
        assert_eq!(array_name(null(), &buffer, 8), None);
        assert_eq!(array_name(ptr, &buffer, 2), None);
        assert_eq!(
            array_name((ptr as usize + 1) as *const u16, &buffer, 8),
            None
        );
    }

    /// Read-only native smoke: no service, startup or process mutations.
    #[test]
    #[ignore = "reads live Windows performance providers"]
    fn native_performance_smoke() {
        let start = Instant::now();
        let mut sampler = PerfSampler::new().unwrap();
        let first = sampler.sample().unwrap();
        let first_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(!first.cpu_name.is_empty());
        assert!(first.logical_cpus > 0);
        assert_eq!(first.network_rx_bytes_per_sec, 0.0);
        assert_eq!(first.network_tx_bytes_per_sec, 0.0);
        assert!(!first.network_rates_ready);
        assert!(!first.disk_rates_ready);
        assert!(first.physical_cores.is_some());
        assert!(first.sockets.is_some());
        assert!(first
            .memory
            .as_ref()
            .is_some_and(|memory| memory.physical_total > 0));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let start = Instant::now();
        let second = sampler.sample().unwrap();
        let sample_ms = start.elapsed().as_secs_f64() * 1000.0;
        for rate in [
            second.network_rx_bytes_per_sec,
            second.network_tx_bytes_per_sec,
            second.disk_read_bytes_per_sec,
            second.disk_write_bytes_per_sec,
        ] {
            assert!(rate.is_finite() && rate >= 0.0);
        }
        for percent in [second.disk_active_percent, second.gpu_percent]
            .into_iter()
            .flatten()
        {
            assert!((0.0..=100.0).contains(&percent));
        }
        assert!(second.uptime_seconds >= first.uptime_seconds);
        for cpu in &second.logical_processors {
            assert!(cpu
                .percent
                .is_some_and(|value| (0.0..=100.0).contains(&value)));
        }
        for gpu in &second.gpus {
            assert_eq!(
                gpu.percent,
                gpu.engines
                    .iter()
                    .map(|engine| engine.percent)
                    .reduce(f64::max)
            );
        }
        println!(
            "Initial collection: {first_ms:.2} ms; warm collection: {sample_ms:.2} ms; {second:#?}"
        );
    }
}
