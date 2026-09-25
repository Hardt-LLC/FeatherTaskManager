//! On-demand native performance collection. No WMI, shell processes or timers.
//! Disk rates cover all physical disks; activity is the busiest physical disk.
//! GPU load is the busiest physical engine, summed across processes on that
//! engine. Network rates cover active hardware interfaces, excluding loopback,
//! VPN and filter interfaces so one packet is not counted at several layers.

use std::collections::HashMap;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::time::Instant;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_TABLE2,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
    PdhGetFormattedCounterValue, PdhOpenQueryW, PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA,
    PDH_FMT_COUNTERVALUE, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY,
    PDH_MORE_DATA,
};
use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::GetActiveProcessorCount;

const MAX_INTERFACES: usize = 16_384;
const MAX_PDH_BYTES: usize = 8 * 1024 * 1024;
// PDH_FMT_NOCAP100 is not exposed by windows-sys 0.61.2.
const FMT_DOUBLE_UNCAPPED: u32 = PDH_FMT_DOUBLE | 0x8000;

#[derive(Clone, Debug)]
pub struct PerfSnapshot {
    pub cpu_name: String,
    pub logical_cpus: u32,
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
    pub warnings: Vec<String>,
}

pub struct PerfSampler {
    cpu_name: String,
    logical_cpus: u32,
    pdh: Option<PdhQuery>,
    setup_warnings: Vec<String>,
    network: HashMap<u64, NetworkCounters>,
    network_at: Option<Instant>,
    generation: u64,
}

#[derive(Clone, Copy)]
struct NetworkCounters {
    rx: u64,
    tx: u64,
    generation: u64,
}

impl PerfSampler {
    pub fn new() -> Result<Self, String> {
        let mut warnings = Vec::new();
        let cpu_name = cpu_name().unwrap_or_else(|error| {
            warnings.push(error);
            "Processor".to_string()
        });
        let pdh = PdhQuery::new(&mut warnings);
        Ok(Self {
            cpu_name,
            logical_cpus: unsafe { GetActiveProcessorCount(0xffff) }.max(1),
            pdh,
            setup_warnings: warnings,
            network: HashMap::new(),
            network_at: None,
            generation: 0,
        })
    }

    pub fn sample(&mut self) -> Result<PerfSnapshot, String> {
        let mut snapshot = PerfSnapshot {
            cpu_name: self.cpu_name.clone(),
            logical_cpus: self.logical_cpus,
            uptime_seconds: unsafe { GetTickCount64() } / 1000,
            network_rx_bytes_per_sec: 0.0,
            network_tx_bytes_per_sec: 0.0,
            network_rates_ready: false,
            disk_read_bytes_per_sec: 0.0,
            disk_write_bytes_per_sec: 0.0,
            disk_rates_ready: false,
            disk_active_percent: None,
            gpu_percent: None,
            warnings: self.setup_warnings.clone(),
        };
        match self.network_rates() {
            Ok((rx, tx, interfaces, ready)) => {
                snapshot.network_rx_bytes_per_sec = rx;
                snapshot.network_tx_bytes_per_sec = tx;
                snapshot.network_rates_ready = ready;
                if interfaces == 0 {
                    snapshot
                        .warnings
                        .push("Network: no active physical adapter".into());
                }
            }
            Err(error) => {
                // Re-baseline after failures; never divide a multi-interval
                // counter delta by just the most recent interval.
                self.network_at = None;
                self.network.clear();
                snapshot.warnings.push(error);
            }
        }
        if let Some(pdh) = &mut self.pdh {
            pdh.sample(&mut snapshot);
        }
        Ok(snapshot)
    }

    fn network_rates(&mut self) -> Result<(f64, f64, usize, bool), String> {
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
        let mut rx = 0.0;
        let mut tx = 0.0;
        let mut active = 0;
        let mut measured = 0;
        for row in rows {
            let flags = row.InterfaceAndOperStatusFlags._bitfield;
            if row.Type == IF_TYPE_SOFTWARE_LOOPBACK
                || row.OperStatus != IfOperStatusUp
                || flags & 1 == 0 // HardwareInterface
                || flags & 2 != 0
            // FilterInterface
            {
                continue;
            }
            active += 1;
            let luid = unsafe { row.InterfaceLuid.Value };
            let previous = self.network.insert(
                luid,
                NetworkCounters {
                    rx: row.InOctets,
                    tx: row.OutOctets,
                    generation: self.generation,
                },
            );
            if let (Some(previous), Some(seconds)) = (previous, seconds) {
                if seconds.is_finite()
                    && seconds > 0.0
                    && row.InOctets >= previous.rx
                    && row.OutOctets >= previous.tx
                {
                    rx += byte_rate(row.InOctets, previous.rx, seconds);
                    tx += byte_rate(row.OutOctets, previous.tx, seconds);
                    measured += 1;
                }
            }
        }
        self.network
            .retain(|_, counters| counters.generation == self.generation);
        self.network_at = Some(now);
        // Treat an incomplete aggregate as a gap, including a new adapter or
        // driver counter reset; the next successful interval will be ready.
        Ok((rx, tx, active, active > 0 && measured == active))
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
    disk_read: Option<PDH_HCOUNTER>,
    disk_write: Option<PDH_HCOUNTER>,
    disk_idle: Option<PDH_HCOUNTER>,
    gpu: Option<PDH_HCOUNTER>,
    // usize provides sufficient alignment for the array's pointers and f64s.
    array_buffer: Vec<usize>,
    primed: bool,
}

impl PdhQuery {
    fn new(warnings: &mut Vec<String>) -> Option<Self> {
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
            disk_read: None,
            disk_write: None,
            disk_idle: None,
            gpu: None,
            array_buffer: vec![0; 2048],
            primed: false,
        };
        query.disk_read = query.add(
            r"\PhysicalDisk(_Total)\Disk Read Bytes/sec",
            "Disk read",
            warnings,
        );
        query.disk_write = query.add(
            r"\PhysicalDisk(_Total)\Disk Write Bytes/sec",
            "Disk write",
            warnings,
        );
        query.disk_idle = query.add(r"\PhysicalDisk(*)\% Idle Time", "Disk activity", warnings);
        query.gpu = query.add(r"\GPU Engine(*)\Utilization Percentage", "GPU", warnings);
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
        let read = read_counter(self.disk_read, "Disk read", &mut snapshot.warnings);
        let write = read_counter(self.disk_write, "Disk write", &mut snapshot.warnings);
        snapshot.disk_rates_ready = read.is_some() && write.is_some();
        snapshot.disk_read_bytes_per_sec = read.unwrap_or(0.0);
        snapshot.disk_write_bytes_per_sec = write.unwrap_or(0.0);
        if let Some(counter) = self.disk_idle {
            match self.array(counter) {
                Ok(values) => snapshot.disk_active_percent = busiest_disk(&values),
                Err(error) => snapshot.warnings.push(format!("Disk activity: {error}")),
            }
        }
        if let Some(counter) = self.gpu {
            match self.array(counter) {
                Ok(values) => snapshot.gpu_percent = busiest_gpu_engine(&values),
                Err(error) => snapshot.warnings.push(format!("GPU: {error}")),
            }
        }
    }

    fn array(&mut self, counter: PDH_HCOUNTER) -> Result<Vec<(String, f64)>, String> {
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
            for item in items {
                if valid_status(item.FmtValue.CStatus) {
                    let value = unsafe { item.FmtValue.Anonymous.doubleValue };
                    if value.is_finite() {
                        if let Some(name) = array_name(item.szName, &self.array_buffer, used) {
                            values.push((name, value));
                        }
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

fn read_counter(
    counter: Option<PDH_HCOUNTER>,
    label: &str,
    warnings: &mut Vec<String>,
) -> Option<f64> {
    let counter = counter?;
    let mut value = PDH_FMT_COUNTERVALUE::default();
    let status = unsafe {
        PdhGetFormattedCounterValue(counter, FMT_DOUBLE_UNCAPPED, null_mut(), &mut value)
    };
    if status == 0 && valid_status(value.CStatus) {
        let value = unsafe { value.Anonymous.doubleValue };
        if value.is_finite() {
            return Some(value.max(0.0));
        }
    }
    warnings.push(format!("{label} temporarily unavailable"));
    None
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

fn busiest_disk(values: &[(String, f64)]) -> Option<f64> {
    values
        .iter()
        .filter(|(name, idle)| name != "_Total" && idle.is_finite())
        .map(|(_, idle)| (100.0 - idle).clamp(0.0, 100.0))
        .reduce(f64::max)
}

fn busiest_gpu_engine(values: &[(String, f64)]) -> Option<f64> {
    let mut engines: HashMap<&str, f64> = HashMap::new();
    for (name, value) in values {
        if !value.is_finite() || *value < 0.0 {
            continue;
        }
        // GPU Engine instances begin pid_<pid>_luid_<adapter>_phys_<n>_eng_<n>.
        // Removing only PID combines processes on one engine, without summing
        // independent engines or adapters (which would overstate utilization).
        let Some((pid, engine)) = name.split_once("_luid_") else {
            continue;
        };
        if !pid
            .strip_prefix("pid_")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            || !engine.contains("_phys_")
            || !engine.contains("_eng_")
        {
            continue;
        }
        let engine = engine.split('#').next().unwrap_or(engine);
        *engines.entry(engine).or_insert(0.0) += value;
    }
    engines
        .values()
        .copied()
        .reduce(f64::max)
        .map(|value| value.clamp(0.0, 100.0))
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
        println!(
            "Initial collection: {first_ms:.2} ms; warm collection: {sample_ms:.2} ms; {second:#?}"
        );
    }
}
