//! Bounded histories from the existing bulk samples; no extra process polling.
use super::*;
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum PerfTarget {
    Cpu,
    Memory,
    Disk(String),
    Network(u64),
    Gpu(String),
}

pub(super) struct TracePoint {
    pub at: Instant,
    pub values: [f64; 3],
}
#[derive(Default)]
pub(super) struct Trace {
    pub points: VecDeque<TracePoint>,
}
impl Trace {
    pub fn record(&mut self, at: Instant, values: [f64; 3]) {
        if self.points.back().is_some_and(|last| at <= last.at) {
            return;
        }
        self.points.push_back(TracePoint { at, values });
        // One point just older than the 60 s window stays (at most one 5 s
        // interval older), so a chart's trace reaches its left edge
        // (`paint::chart` interpolates at 60 s).
        while self.points.len() > 241
            || self.points.get(1).is_some_and(|second| {
                at.saturating_duration_since(second.at) > Duration::from_secs(60)
            })
            || self.points.front().is_some_and(|first| {
                at.saturating_duration_since(first.at) > Duration::from_secs(65)
            })
        {
            self.points.pop_front();
        }
    }
}

#[derive(Default)]
pub(super) struct ProcessTelemetry {
    pub identity: Option<(u32, u64)>,
    pub name: String,
    pub ended: bool,
    pub trace: Trace,
}
impl ProcessTelemetry {
    pub fn select(&mut self, process: Option<&Process>) {
        let identity = process.map(|p| (p.pid, p.created));
        if identity != self.identity {
            self.identity = identity;
            self.name = process.map_or_else(String::new, |p| p.name.clone());
            self.ended = false;
            self.trace.points.clear();
        }
    }
    pub fn record(&mut self, snapshot: &Snapshot, at: Instant) {
        let Some(identity) = self.identity else {
            return;
        };
        if let Some(process) = snapshot
            .processes
            .iter()
            .find(|p| (p.pid, p.created) == identity)
        {
            self.ended = false;
            self.trace.record(
                at,
                [
                    process.cpu_percent,
                    process.working_set as f64,
                    process.io_bytes_per_sec,
                ],
            );
        } else {
            self.ended = true;
            self.trace.record(at, [f64::NAN; 3]);
        }
    }
}

#[derive(Default)]
pub(super) struct PerfHistory {
    pub traces: HashMap<PerfTarget, Trace>,
    pub cores: HashMap<String, Trace>,
}
impl PerfHistory {
    pub fn gap(&mut self, at: Instant) {
        for trace in self.traces.values_mut().chain(self.cores.values_mut()) {
            if trace
                .points
                .back()
                .is_some_and(|p| p.values.iter().any(|v| v.is_finite()))
            {
                trace.record(at, [f64::NAN; 3]);
            }
        }
    }
    pub fn record(&mut self, sample: &PerfSnapshot, snapshot: &Snapshot, at: Instant) {
        self.cores
            .retain(|id, _| sample.logical_processors.iter().any(|cpu| &cpu.id == id));
        for cpu in sample.logical_processors.iter().take(4096) {
            self.cores
                .entry(cpu.id.clone())
                .or_default()
                .record(at, [cpu.percent.unwrap_or(f64::NAN), f64::NAN, f64::NAN]);
        }
        let mut current = HashMap::new();
        current.insert(PerfTarget::Cpu, [snapshot.cpu_percent, f64::NAN, f64::NAN]);
        current.insert(
            PerfTarget::Memory,
            [
                snapshot.memory_used as f64,
                sample
                    .memory
                    .as_ref()
                    .map_or(f64::NAN, |m| m.commit_used as f64),
                f64::NAN,
            ],
        );
        for disk in sample.disks.iter().take(128) {
            current.insert(
                PerfTarget::Disk(disk.id.clone()),
                [
                    disk.active_percent.unwrap_or(f64::NAN),
                    disk.read_bytes_per_sec.unwrap_or(f64::NAN),
                    disk.write_bytes_per_sec.unwrap_or(f64::NAN),
                ],
            );
        }
        for nic in sample.networks.iter().take(128) {
            current.insert(
                PerfTarget::Network(nic.id),
                [
                    nic.rx_bytes_per_sec.unwrap_or(f64::NAN),
                    nic.tx_bytes_per_sec.unwrap_or(f64::NAN),
                    f64::NAN,
                ],
            );
        }
        for gpu in sample.gpus.iter().take(64) {
            current.insert(
                PerfTarget::Gpu(gpu.id.clone()),
                [
                    gpu.percent.unwrap_or(f64::NAN),
                    gpu.dedicated_bytes.map_or(f64::NAN, |b| b as f64),
                    gpu.shared_bytes.map_or(f64::NAN, |b| b as f64),
                ],
            );
        }
        self.traces.retain(|key, trace| {
            current.contains_key(key)
                || trace.points.iter().any(|point| {
                    at.saturating_duration_since(point.at) <= Duration::from_secs(60)
                        && point.values.iter().any(|v| v.is_finite())
                })
        });
        for (key, trace) in &mut self.traces {
            if !current.contains_key(key) {
                trace.record(at, [f64::NAN; 3]);
            }
        }
        for (key, values) in current {
            self.traces.entry(key).or_default().record(at, values);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trace_is_time_and_count_bounded_and_rejects_old_samples() {
        let now = Instant::now();
        let mut trace = Trace::default();
        for i in 0..300 {
            trace.record(now + Duration::from_millis(i * 250), [i as f64; 3]);
        }
        assert_eq!(trace.points.len(), 241);
        trace.record(now, [999.; 3]);
        assert_eq!(trace.points.len(), 241);
        trace.record(now + Duration::from_secs(180), [0.; 3]);
        assert_eq!(trace.points.len(), 1);
    }
    #[test]
    fn reused_pid_does_not_continue_selected_process_history() {
        let mut process = Process {
            pid: 42,
            parent_pid: 1,
            created: 10,
            name: "sample".into(),
            cpu_percent: 3.,
            working_set: 1024,
            private_bytes: 2048,
            io_bytes_per_sec: 7.,
            gpu_percent: None,
            network_bytes_per_sec: None,
            threads: 1,
            handles: 2,
            ..Process::default()
        };
        let mut selected = ProcessTelemetry::default();
        selected.select(Some(&process));
        let now = Instant::now();
        process.created = 11;
        let snapshot = Snapshot {
            processes: vec![process.clone()],
            cpu_percent: 0.,
            memory_used: 0,
            memory_total: 0,
            sample_ms: 0.,
        };
        selected.record(&snapshot, now);
        assert!(selected.ended);
        assert!(selected.trace.points[0].values[0].is_nan());
        selected.select(Some(&process));
        assert!(selected.trace.points.is_empty());
        assert!(!selected.ended);
    }
}
