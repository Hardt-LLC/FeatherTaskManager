//! Readable report of every hardware value Feather collects, written by
//! `FeatherTaskManager.exe --dump-hardware <file>` so the values can be
//! compared with independent tools. It uses exactly the collectors the UI
//! uses; nothing here is shown in the application.

use crate::netetw::{NetworkMonitor, ProcessNetworkSample};
use crate::performance::{PerfSampler, PerfSnapshot, ProcessGpu, ProcessGpuTracker};
use crate::sampler::{Sampler, Snapshot};
use crate::smbios::{form_factor_name, memory_type_name};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

/// Samples used for the per-sample cost comparison.
const COST_SAMPLES: usize = 20;

pub fn dump_report() -> Result<String, String> {
    let mut out = String::new();
    let _ = writeln!(out, "Feather Task Manager hardware dump");
    let _ = writeln!(out, "version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(out, "local time: {}", local_time());
    let _ = writeln!(out, "elevated: {}", yes_no(crate::netetw::is_elevated()));
    let _ = writeln!(
        out,
        "Values are exactly what the collectors return; '-' means not reported."
    );

    let mut processes = Sampler::new()?;
    let started = Instant::now();
    let mut perf = PerfSampler::new()?;
    let construct_ms = ms(started);
    let mut tracker = ProcessGpuTracker::default();
    let mut network = NetworkMonitor::new();
    let snapshot = processes.sample()?;
    let started = Instant::now();
    let prime = perf.sample()?;
    let prime_ms = ms(started);
    tracker.join(&snapshot.processes, Some(&prime));
    network.sample(&snapshot.processes);

    let mut samples = Vec::new();
    for _ in 0..2 {
        std::thread::sleep(Duration::from_secs(1));
        let snapshot = processes.sample()?;
        let sample = perf.sample()?;
        let joined = tracker.join(&snapshot.processes, Some(&sample));
        let net = network.sample(&snapshot.processes);
        samples.push((snapshot, sample, joined, net));
    }

    section(&mut out, "Collection cost (this run)");
    let _ = writeln!(
        out,
        "sampler construction (CPU, SMBIOS, PDH setup; includes the process's one-time PDH initialization): {construct_ms:.3} ms"
    );
    let started = Instant::now();
    let _ = PerfSampler::new()?;
    let _ = writeln!(
        out,
        "sampler construction again (PDH already initialized): {:.3} ms",
        ms(started)
    );
    let _ = writeln!(
        out,
        "first sample (PDH priming, D3DKMT enumeration): {prime_ms:.3} ms"
    );
    let started = Instant::now();
    let adapters = crate::gpu::enumerate();
    let _ = writeln!(
        out,
        "D3DKMT adapter enumeration alone: {:.3} ms ({} adapters)",
        ms(started),
        adapters.as_ref().map_or(0, Vec::len)
    );
    let disk_numbers = samples[0]
        .1
        .disks
        .iter()
        .filter_map(|disk| crate::storage::disk_number(&disk.id))
        .collect();
    let started = Instant::now();
    let _ = crate::storage::describe(&disk_numbers);
    let _ = writeln!(
        out,
        "storage identity query alone: {:.3} ms ({} disks)",
        ms(started),
        disk_numbers.len()
    );
    let started = Instant::now();
    let _ = crate::smbios::memory_inventory();
    let _ = writeln!(out, "SMBIOS read and parse alone: {:.3} ms", ms(started));
    cost_comparison(&mut out, &mut perf)?;

    let static_snapshot = &samples[0].1;
    static_facts(&mut out, static_snapshot);
    for (index, (snapshot, sample, joined, net)) in samples.iter().enumerate() {
        dynamic_values(&mut out, index + 1, snapshot, sample, joined, net);
    }
    let _ = writeln!(
        out,
        "\nnetwork ETW events processed: {}",
        network.event_count()
    );
    Ok(out)
}

/// Interleaves the full sampler with a baseline sampler that lacks the
/// hardware collectors, and reports both medians.
fn cost_comparison(out: &mut String, full: &mut PerfSampler) -> Result<(), String> {
    let mut baseline = PerfSampler::baseline()?;
    let _ = baseline.sample()?;
    std::thread::sleep(Duration::from_millis(250));
    let mut full_ms = Vec::new();
    let mut sensor_ms = Vec::new();
    let mut base_ms = Vec::new();
    for index in 0..COST_SAMPLES {
        std::thread::sleep(Duration::from_millis(250));
        // Alternate which sampler runs first; the first collection after an
        // idle period is slower.
        if index % 2 == 1 {
            base_ms.push(baseline.sample()?.sample_ms);
        }
        let sample = full.sample()?;
        full_ms.push(sample.sample_ms);
        sensor_ms.push(sample.gpu_sensor_ms);
        if index % 2 == 0 {
            base_ms.push(baseline.sample()?.sample_ms);
        }
    }
    let median = |values: &mut Vec<f64>| {
        values.sort_by(f64::total_cmp);
        values[values.len() / 2]
    };
    let full = median(&mut full_ms);
    let base = median(&mut base_ms);
    let _ = writeln!(
        out,
        "per-sample median over {COST_SAMPLES} interleaved samples: with hardware collectors {full:.3} ms, baseline {base:.3} ms, added {:.3} ms (GPU sensors {:.3} ms)",
        full - base,
        median(&mut sensor_ms)
    );
    Ok(())
}

fn static_facts(out: &mut String, perf: &PerfSnapshot) {
    section(out, "CPU");
    let _ = writeln!(out, "name: {}", perf.cpu_name);
    let _ = writeln!(
        out,
        "sockets / cores / logical: {} / {} / {}",
        opt(perf.sockets),
        opt(perf.physical_cores),
        perf.logical_cpus
    );
    let _ = writeln!(
        out,
        "base speed (MaxMhz): {}",
        opt_unit(perf.cpu_base_mhz, "MHz")
    );
    match perf.cpu_caches {
        Some(caches) => {
            let _ = writeln!(
                out,
                "caches: L1 {} / L2 {} / L3 {}",
                opt_bytes(caches.l1_bytes),
                opt_bytes(caches.l2_bytes),
                opt_bytes(caches.l3_bytes)
            );
        }
        None => {
            let _ = writeln!(out, "caches: -");
        }
    }
    let _ = writeln!(
        out,
        "virtualization enabled in firmware: {}",
        yes_no(perf.virtualization_firmware_enabled)
    );
    let _ = writeln!(
        out,
        "temperature source: Windows Thermal Zone Information (read-only PDH); firmware zone location, not CPU package; refresh {} s, unavailable retry {} s",
        crate::thermal::POLL_INTERVAL.as_secs(),
        crate::thermal::RETRY_INTERVAL.as_secs()
    );

    section(out, "Memory modules (SMBIOS)");
    match &perf.memory_modules {
        Some(memory) => {
            let _ = writeln!(
                out,
                "SMBIOS {}.{}; slots used {} of {}; max capacity {}",
                memory.smbios_version.0,
                memory.smbios_version.1,
                memory.slots_used,
                opt(memory.slots_total),
                opt_bytes(memory.max_capacity_bytes)
            );
            let _ = writeln!(
                out,
                "summary: type {}, form factor {}, configured speed {}",
                opt(memory.memory_type()),
                opt(memory.form_factor()),
                opt_unit(memory.configured_speed_mts(), "MT/s")
            );
            for module in &memory.modules {
                let _ = writeln!(
                    out,
                    "- {} / {}: {} {} {}, speed {}, configured {}, {} mV, {} {}",
                    opt(module.bank_locator.as_deref()),
                    opt(module.device_locator.as_deref()),
                    opt_bytes(module.size_bytes),
                    opt(memory_type_name(module.memory_type)),
                    opt(form_factor_name(module.form_factor)),
                    memory_speed(module.speed_mts, module.speed_reported),
                    memory_speed(
                        module.configured_speed_mts,
                        module.configured_speed_reported
                    ),
                    opt(module.configured_voltage_mv),
                    opt(module.manufacturer.as_deref()),
                    opt(module.part_number.as_deref()),
                );
            }
        }
        None => {
            let _ = writeln!(out, "-");
        }
    }
    if let Some(memory) = &perf.memory {
        let _ = writeln!(
            out,
            "Windows physical memory: {}",
            bytes(memory.physical_total)
        );
    }

    section(out, "Storage");
    for device in &perf.storage {
        let _ = writeln!(
            out,
            "disk {}: model {}; vendor id {}; product id {}; firmware {}; bus {}; media {}; removable {}; TRIM {}; capacity {} ({} bytes); system disk {}; page file {}",
            device.disk_number,
            opt(device.model.as_deref()),
            opt(device.vendor_id.as_deref()),
            opt(device.product_id.as_deref()),
            opt(device.firmware_revision.as_deref()),
            opt(device.bus.map(|bus| bus.label())),
            opt(device.media_label()),
            opt(device.removable.map(yes_no)),
            opt(device.trim_enabled.map(yes_no)),
            opt_bytes(device.capacity_bytes),
            opt(device.capacity_bytes),
            opt(device.system_disk.map(yes_no)),
            opt(device.page_file.map(yes_no)),
        );
    }
    for disk in &perf.disks {
        let _ = writeln!(
            out,
            "PDH instance \"{}\" -> {}",
            disk.id,
            perf.storage_device(&disk.id)
                .and_then(|device| device.model.as_deref())
                .unwrap_or("-")
        );
    }

    section(out, "GPU adapters (D3DKMT)");
    for adapter in &perf.gpu_adapters {
        let flags = adapter.flags.map_or_else(
            || "-".into(),
            |flags| {
                let mut names = Vec::new();
                for (set, name) in [
                    (flags.render_supported, "render"),
                    (flags.display_supported, "display"),
                    (flags.software, "software"),
                    (flags.hybrid_discrete, "hybrid-discrete"),
                    (flags.hybrid_integrated, "hybrid-integrated"),
                    (flags.indirect_display, "indirect-display"),
                    (flags.paravirtualized, "paravirtualized"),
                    (flags.compute_only, "compute-only"),
                ] {
                    if set {
                        names.push(name);
                    }
                }
                names.join(",")
            },
        );
        let _ = writeln!(
            out,
            "luid 0x{:08X}_0x{:08X} ({}): name {}; adapter string {}; chip {}; DAC {}; BIOS {}; flags {}",
            adapter.luid.0,
            adapter.luid.1,
            if adapter.is_software() {
                "software adapter"
            } else if adapter.is_indirect_display() {
                "indirect display, no render engines"
            } else {
                "hardware"
            },
            opt(adapter.name.as_deref()),
            opt(adapter.adapter_string.as_deref()),
            opt(adapter.chip_type.as_deref()),
            opt(adapter.dac_type.as_deref()),
            opt(adapter.bios.as_deref()),
            flags
        );
        let _ = writeln!(
            out,
            "  dedicated video {} ({} bytes); dedicated system {}; shared system {}; WDDM {}; driver {} ({}); PCI {}; location {}; physical adapters {}; perf data {}",
            opt_bytes(adapter.dedicated_video_memory),
            opt(adapter.dedicated_video_memory),
            opt_bytes(adapter.dedicated_system_memory),
            opt_bytes(adapter.shared_system_memory),
            opt(adapter.wddm_version.map(|(major, minor)| format!("{major}.{minor}"))),
            opt(adapter.driver_version.as_deref()),
            opt(adapter.driver_date.as_deref()),
            opt(adapter.pci.map(|pci| format!(
                "VEN_{:04X}&DEV_{:04X}&SUBSYS_{:04X}{:04X}&REV_{:02X}",
                pci.vendor_id,
                pci.device_id,
                pci.subsystem_id,
                pci.subsystem_vendor_id,
                pci.revision_id
            ))),
            opt(adapter.pci_location.map(|(bus, device, function)| format!(
                "bus {bus}, device {device}, function {function}"
            ))),
            adapter.physical_adapters,
            yes_no(adapter.perf_data_supported)
        );
        for caps in &adapter.perf_caps {
            let _ = writeln!(
                out,
                "  phys {} limits: max fan {}; temperature max {}; warning {}; max memory bandwidth {}/s; max PCIe bandwidth {}/s",
                caps.physical_index,
                opt_unit(caps.max_fan_rpm, "RPM"),
                opt_unit(caps.temperature_max_c, "C"),
                opt_unit(caps.temperature_warning_c, "C"),
                opt_bytes(caps.max_memory_bandwidth),
                opt_bytes(caps.max_pcie_bandwidth)
            );
        }
    }
}

fn dynamic_values(
    out: &mut String,
    index: usize,
    snapshot: &Snapshot,
    perf: &PerfSnapshot,
    joined: &HashMap<(u32, u64), ProcessGpu>,
    net: &ProcessNetworkSample,
) {
    section(out, &format!("Sample {index}"));
    let _ = writeln!(
        out,
        "perf sample {:.3} ms (GPU sensors {:.3} ms); process snapshot {:.3} ms; CPU {:.1}%; reported frequency {}",
        perf.sample_ms,
        perf.gpu_sensor_ms,
        snapshot.sample_ms,
        snapshot.cpu_percent,
        opt(perf.cpu_frequency_mhz.map(|mhz| format!("{mhz:.0} MHz")))
    );
    for gpu in &perf.gpus {
        let sensors = gpu.sensors;
        let _ = writeln!(
            out,
            "GPU {} ({}{}): utilization {}; dedicated used {}; shared used {}; temperature {}; fan {}; power {}; memory clock {} (max {}); engines {}",
            gpu.id,
            opt(gpu.name.as_deref()),
            if gpu.is_software() { ", software" } else { "" },
            opt(gpu.percent.map(|p| format!("{p:.1}%"))),
            opt_bytes(gpu.dedicated_bytes),
            opt_bytes(gpu.shared_bytes),
            opt(sensors.and_then(|s| s.temperature_c).map(|c| format!("{c:.1} C"))),
            opt_unit(sensors.and_then(|s| s.fan_rpm), "RPM"),
            opt(sensors.and_then(|s| s.power_percent).map(|p| format!("{p:.1}% of limit"))),
            opt(sensors.and_then(|s| s.memory_clock_hz).map(mhz)),
            opt(sensors.and_then(|s| s.max_memory_clock_hz).map(mhz)),
            gpu.engines.len()
        );
    }
    if perf.thermal_zones.is_empty() {
        let _ = writeln!(
            out,
            "ACPI thermal zones: unavailable (no valid firmware reading)"
        );
    }
    for zone in &perf.thermal_zones {
        let _ = writeln!(
            out,
            "ACPI thermal zone {}: {:.1} C",
            zone.name, zone.celsius
        );
    }
    for nic in &perf.networks {
        let _ = writeln!(
            out,
            "network \"{}\": {}; {} (ifType {}, medium {}); {}{}; link {} / {} Mbps; rx {} tx {}",
            nic.name,
            nic.description,
            nic.kind.label(),
            nic.interface_type,
            nic.physical_medium,
            if nic.connected {
                "connected"
            } else {
                "disconnected"
            },
            if nic.present { "" } else { ", not present" },
            nic.receive_link_bits_per_sec / 1_000_000,
            nic.transmit_link_bits_per_sec / 1_000_000,
            opt(nic.rx_bytes_per_sec.map(rate)),
            opt(nic.tx_bytes_per_sec.map(rate)),
        );
    }
    for disk in &perf.disks {
        let _ = writeln!(
            out,
            "disk \"{}\": active {}; read {}; write {}",
            disk.id,
            opt(disk.active_percent.map(|p| format!("{p:.1}%"))),
            opt(disk.read_bytes_per_sec.map(rate)),
            opt(disk.write_bytes_per_sec.map(rate)),
        );
    }
    let sample = &perf.process_gpu;
    let with_percent = joined.values().filter(|v| v.percent.is_some()).count();
    let _ = writeln!(
        out,
        "process GPU: utilization measured {}, memory measured {}, {} PIDs with instances, {} pending, {} of {} processes attributed a utilization",
        yes_no(sample.utilization_measured),
        yes_no(sample.memory_measured),
        sample.by_pid.len(),
        sample.pending.len(),
        with_percent,
        snapshot.processes.len()
    );
    let names: HashMap<(u32, u64), &str> = snapshot
        .processes
        .iter()
        .map(|p| ((p.pid, p.created), p.name.as_str()))
        .collect();
    let mut rows: Vec<_> = joined.iter().collect();
    rows.sort_by(|a, b| {
        let key = |v: &ProcessGpu| (v.percent.unwrap_or(-1.0), v.dedicated_bytes.unwrap_or(0));
        let (ap, ad) = key(a.1);
        let (bp, bd) = key(b.1);
        bp.total_cmp(&ap).then(bd.cmp(&ad))
    });
    for ((pid, created), value) in rows.into_iter().take(15) {
        let _ = writeln!(
            out,
            "  pid {pid} {}: GPU {}{}; dedicated {}; shared {}",
            names.get(&(*pid, *created)).unwrap_or(&"?"),
            opt(value.percent.map(|p| format!("{p:.1}%"))),
            value
                .engine
                .as_ref()
                .map_or_else(String::new, |engine| format!(
                    " ({} {})",
                    engine.adapter, engine.engine_type
                )),
            opt_bytes(value.dedicated_bytes),
            opt_bytes(value.shared_bytes),
        );
    }
    process_network(out, snapshot, net);
    if !perf.warnings.is_empty() {
        let _ = writeln!(out, "warnings: {:?}", perf.warnings);
    }
}

/// Reports per-process network throughput (ETW) with the top talkers, or the
/// reason it is unavailable.
fn process_network(out: &mut String, snapshot: &Snapshot, net: &ProcessNetworkSample) {
    if let Some(reason) = &net.reason {
        let _ = writeln!(out, "process network: unavailable ({reason})");
        return;
    }
    if !net.measured {
        let _ = writeln!(out, "process network: priming (no interval yet)");
        return;
    }
    let _ = writeln!(
        out,
        "process network: measured over {:.3} s; total send {} recv {}; {} processes with rates",
        net.interval_seconds,
        rate(net.total_send_bytes_per_sec),
        rate(net.total_recv_bytes_per_sec),
        net.by_id.len(),
    );
    let names: HashMap<(u32, u64), &str> = snapshot
        .processes
        .iter()
        .map(|p| ((p.pid, p.created), p.name.as_str()))
        .collect();
    let mut rows: Vec<_> = net
        .by_id
        .iter()
        .filter(|(_, v)| v.total_bytes_per_sec > 0.0)
        .collect();
    rows.sort_by(|a, b| b.1.total_bytes_per_sec.total_cmp(&a.1.total_bytes_per_sec));
    for ((pid, created), value) in rows.into_iter().take(15) {
        let _ = writeln!(
            out,
            "  pid {pid} {}: send {} recv {} total {}",
            names.get(&(*pid, *created)).unwrap_or(&"?"),
            rate(value.send_bytes_per_sec),
            rate(value.recv_bytes_per_sec),
            rate(value.total_bytes_per_sec),
        );
    }
}

/// A memory speed in MT/s, or the firmware's raw value when the table's
/// specification does not define its unit as MT/s.
fn memory_speed(mts: Option<u32>, reported: Option<u32>) -> String {
    match (mts, reported) {
        (Some(mts), _) => format!("{mts} MT/s"),
        (None, Some(raw)) => format!("reported {raw} (unit not MT/s before SMBIOS 3.1.1)"),
        (None, None) => "-".to_string(),
    }
}

fn section(out: &mut String, title: &str) {
    let _ = writeln!(out, "\n== {title} ==");
}

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

fn opt<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map_or_else(|| "-".into(), |value| value.to_string())
}

fn opt_unit<T: std::fmt::Display>(value: Option<T>, unit: &str) -> String {
    value.map_or_else(|| "-".into(), |value| format!("{value} {unit}"))
}

fn opt_bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "-".into(), bytes)
}

fn bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = value as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value} B")
    } else {
        format!("{size:.2} {}", UNITS[unit])
    }
}

fn rate(value: f64) -> String {
    format!("{}/s", bytes(value.max(0.0) as u64))
}

fn mhz(hz: u64) -> String {
    format!("{:.0} MHz", hz as f64 / 1_000_000.0)
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn local_time() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    let mut time = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut time) };
    let t: windows_sys::Win32::Foundation::SYSTEMTIME = time;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}
