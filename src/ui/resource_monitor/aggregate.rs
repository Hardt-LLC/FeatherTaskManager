//! Time-weighted transport samples between resource-monitor repaints.
//! The shared monitor can run faster than this window. Discarding those
//! samples would discard ETW byte batches, especially with one-second flushing.

use crate::{fileetw, netetw};
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

const MAX_PROCESSES: usize = 4096;
const MAX_DETAILS: usize = 2048;
type Identity = (u32, u64);
type EndpointKey = (u32, u64, netetw::Transport, IpAddr, u16, IpAddr, u16);

#[derive(Default)]
pub(super) struct Pending {
    last_at: Option<Instant>,
    network: Network,
    files: Files,
}

impl Pending {
    pub(super) fn push(
        &mut self,
        at: Instant,
        network: &netetw::ProcessNetworkSample,
        files: &fileetw::Sample,
    ) {
        if self.last_at.is_some_and(|last| at <= last) {
            return;
        }
        let skipped = self.last_at.is_some_and(|last| {
            let elapsed = at.saturating_duration_since(last).as_secs_f64();
            let seconds = [network.interval_seconds, files.interval_seconds]
                .into_iter()
                .filter(|&seconds| duration(seconds))
                // Providers drain at different points in the same iteration;
                // allow the longer observed interval to cover collector work.
                .fold(0.0, f64::max);
            duration(seconds) && elapsed > seconds + (seconds * 0.25).max(0.1)
        });
        self.last_at = Some(at);
        self.network.push(network);
        self.files.push(files);
        if skipped {
            // The monitor's bounded delivery channel can drop a sample while
            // the UI is busy. Its bytes have already been drained from ETW;
            // treating the remaining intervals as complete would under-report.
            let reason = "Some resource samples were skipped; waiting for a complete interval";
            self.network.invalid = true;
            self.network.reason = Some(reason.into());
            self.network.by_id.clear();
            if self.network.endpoints.enabled {
                self.network.endpoints.invalid = true;
                self.network.endpoints.reason = Some(reason.into());
                self.network.endpoints.rows.clear();
            }
            if self.files.enabled {
                self.files.invalid = true;
                self.files.reason = Some(reason.into());
                self.files.rows.clear();
            }
        }
    }

    pub(super) fn take(&mut self) -> (netetw::ProcessNetworkSample, fileetw::Sample) {
        (
            std::mem::take(&mut self.network).finish(),
            std::mem::take(&mut self.files).finish(),
        )
    }

    pub(super) fn clear(&mut self) {
        // Retain last_at: a repaint or pause/resume must not count the same
        // cached monitor sample for a second time.
        self.network = Network::default();
        self.files = Files::default();
    }
}

fn duration(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

#[derive(Clone, Copy, Default)]
struct Bytes {
    send: f64,
    recv: f64,
}

impl Bytes {
    fn add(&mut self, send: f64, recv: f64, seconds: f64) -> bool {
        if !send.is_finite() || send < 0.0 || !recv.is_finite() || recv < 0.0 {
            return false;
        }
        self.send += send * seconds;
        self.recv += recv * seconds;
        self.send.is_finite() && self.recv.is_finite() && (self.send + self.recv).is_finite()
    }

    fn rates(self, seconds: f64) -> netetw::ProcessNet {
        netetw::ProcessNet {
            send_bytes_per_sec: self.send / seconds,
            recv_bytes_per_sec: self.recv / seconds,
            total_bytes_per_sec: (self.send + self.recv) / seconds,
        }
    }
}

#[derive(Default)]
struct Network {
    seen: bool,
    invalid: bool,
    seconds: f64,
    reason: Option<String>,
    by_id: HashMap<Identity, Bytes>,
    totals: Bytes,
    endpoints: Endpoints,
}

impl Network {
    fn push(&mut self, sample: &netetw::ProcessNetworkSample) {
        self.endpoints.push(&sample.endpoints, sample.measured);
        let first = !self.seen;
        self.seen = true;
        if duration(sample.interval_seconds) {
            self.seconds += sample.interval_seconds;
        }
        if !sample.measured || !duration(sample.interval_seconds) || !self.seconds.is_finite() {
            self.invalid = true;
            self.reason = sample
                .reason
                .clone()
                .or_else(|| Some("Network sample unavailable".into()));
            return;
        }
        if self.invalid {
            return;
        }
        if sample.by_id.len() > MAX_PROCESSES {
            self.invalid = true;
            self.reason = Some("Process network tracking limit reached".into());
            self.by_id.clear();
            return;
        }
        if first {
            self.by_id.extend(
                sample
                    .by_id
                    .keys()
                    .copied()
                    .map(|id| (id, Bytes::default())),
            );
        } else {
            // A process needs every interval in the admitted frame. In
            // particular, new/reused PIDs must not acquire fabricated zeros.
            self.by_id.retain(|id, _| sample.by_id.contains_key(id));
        }
        for (id, counts) in &mut self.by_id {
            let value = sample.by_id[id];
            if !counts.add(
                value.send_bytes_per_sec,
                value.recv_bytes_per_sec,
                sample.interval_seconds,
            ) {
                self.invalid = true;
            }
        }
        if !self.totals.add(
            sample.total_send_bytes_per_sec,
            sample.total_recv_bytes_per_sec,
            sample.interval_seconds,
        ) {
            self.invalid = true;
        }
        if self.invalid {
            self.reason = Some("Network sample unavailable".into());
        }
    }

    fn finish(self) -> netetw::ProcessNetworkSample {
        let measured = self.seen && !self.invalid && duration(self.seconds);
        let totals = if measured {
            self.totals.rates(self.seconds)
        } else {
            netetw::ProcessNet::default()
        };
        netetw::ProcessNetworkSample {
            measured,
            reason: self.reason,
            interval_seconds: if self.seconds.is_finite() {
                self.seconds
            } else {
                0.0
            },
            by_id: if measured {
                self.by_id
                    .into_iter()
                    .map(|(id, bytes)| (id, bytes.rates(self.seconds)))
                    .collect()
            } else {
                HashMap::new()
            },
            total_send_bytes_per_sec: totals.send_bytes_per_sec,
            total_recv_bytes_per_sec: totals.recv_bytes_per_sec,
            endpoints: self.endpoints.finish(),
        }
    }
}

#[derive(Default)]
struct Endpoints {
    enabled: bool,
    invalid: bool,
    seconds: f64,
    reason: Option<String>,
    limited: bool,
    rows: HashMap<EndpointKey, Bytes>,
}

impl Endpoints {
    fn push(&mut self, sample: &netetw::EndpointSample, parent_measured: bool) {
        if !sample.enabled {
            *self = Self::default();
            return;
        }
        self.enabled = true;
        self.limited |= sample.limited;
        if duration(sample.interval_seconds) {
            self.seconds += sample.interval_seconds;
        }
        if !parent_measured
            || !sample.measured
            || !duration(sample.interval_seconds)
            || !self.seconds.is_finite()
            || sample.limited
        {
            self.invalid = true;
            self.reason = sample
                .reason
                .clone()
                .or_else(|| Some("Endpoint traffic unavailable".into()));
            return;
        }
        if self.invalid {
            return;
        }
        for row in &sample.rows {
            let key = (
                row.pid,
                row.created,
                row.protocol,
                row.local_addr,
                row.local_port,
                row.remote_addr,
                row.remote_port,
            );
            if !self.rows.contains_key(&key) && self.rows.len() >= MAX_DETAILS {
                self.invalid = true;
                self.limited = true;
                self.reason = Some("Endpoint tracking limit reached".into());
                self.rows.clear();
                return;
            }
            if !self.rows.entry(key).or_default().add(
                row.send_bytes_per_sec,
                row.recv_bytes_per_sec,
                sample.interval_seconds,
            ) {
                self.invalid = true;
                self.reason = Some("Endpoint traffic unavailable".into());
                return;
            }
        }
    }

    fn finish(self) -> netetw::EndpointSample {
        let measured = self.enabled && !self.invalid && duration(self.seconds);
        netetw::EndpointSample {
            enabled: self.enabled,
            measured,
            reason: self.reason,
            interval_seconds: if self.seconds.is_finite() {
                self.seconds
            } else {
                0.0
            },
            rows: if measured {
                self.rows
                    .into_iter()
                    .map(
                        |(
                            (
                                pid,
                                created,
                                protocol,
                                local_addr,
                                local_port,
                                remote_addr,
                                remote_port,
                            ),
                            bytes,
                        )| {
                            let rates = bytes.rates(self.seconds);
                            netetw::EndpointTraffic {
                                pid,
                                created,
                                protocol,
                                local_addr,
                                local_port,
                                remote_addr,
                                remote_port,
                                send_bytes_per_sec: rates.send_bytes_per_sec,
                                recv_bytes_per_sec: rates.recv_bytes_per_sec,
                            }
                        },
                    )
                    .collect()
            } else {
                Vec::new()
            },
            limited: self.limited,
        }
    }
}

#[derive(Default)]
struct Files {
    enabled: bool,
    invalid: bool,
    seconds: f64,
    reason: Option<String>,
    notice: Option<String>,
    limited: bool,
    row_count: usize,
    // Borrowed lookup avoids copying each path again on every fast sample.
    rows: HashMap<Identity, HashMap<String, Bytes>>,
}

impl Files {
    fn push(&mut self, sample: &fileetw::Sample) {
        if !sample.enabled {
            *self = Self::default();
            return;
        }
        self.enabled = true;
        self.limited |= sample.limited;
        if duration(sample.interval_seconds) {
            self.seconds += sample.interval_seconds;
        }
        if !sample.measured
            || !duration(sample.interval_seconds)
            || !self.seconds.is_finite()
            || sample.limited
        {
            self.invalid = true;
            self.reason = sample
                .reason
                .clone()
                .or_else(|| Some("File I/O requests unavailable".into()));
            return;
        }
        if self.invalid {
            return;
        }
        if sample.reason.is_some() {
            self.notice.clone_from(&sample.reason);
        }
        for row in &sample.rows {
            let key = (row.pid, row.created);
            let existing = self
                .rows
                .get_mut(&key)
                .and_then(|paths| paths.get_mut(&row.path));
            if let Some(bytes) = existing {
                if !bytes.add(
                    row.read_bytes_per_sec,
                    row.write_bytes_per_sec,
                    sample.interval_seconds,
                ) {
                    self.invalid = true;
                    self.reason = Some("File I/O requests unavailable".into());
                    return;
                }
                continue;
            }
            if self.row_count >= MAX_DETAILS {
                self.invalid = true;
                self.limited = true;
                self.reason = Some("File I/O tracking limit reached".into());
                self.rows.clear();
                return;
            }
            let mut bytes = Bytes::default();
            if !bytes.add(
                row.read_bytes_per_sec,
                row.write_bytes_per_sec,
                sample.interval_seconds,
            ) {
                self.invalid = true;
                self.reason = Some("File I/O requests unavailable".into());
                return;
            }
            self.rows
                .entry(key)
                .or_default()
                .insert(row.path.clone(), bytes);
            self.row_count += 1;
        }
    }

    fn finish(self) -> fileetw::Sample {
        let measured = self.enabled && !self.invalid && duration(self.seconds);
        fileetw::Sample {
            enabled: self.enabled,
            measured,
            interval_seconds: if self.seconds.is_finite() {
                self.seconds
            } else {
                0.0
            },
            reason: self.reason.or(self.notice),
            rows: if measured {
                self.rows
                    .into_iter()
                    .flat_map(|((pid, created), paths)| {
                        paths.into_iter().map(move |(path, bytes)| fileetw::Row {
                            pid,
                            created,
                            path,
                            read_bytes_per_sec: bytes.send / self.seconds,
                            write_bytes_per_sec: bytes.recv / self.seconds,
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            },
            limited: self.limited,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample(seconds: f64, rate: f64) -> (netetw::ProcessNetworkSample, fileetw::Sample) {
        (
            netetw::ProcessNetworkSample {
                measured: true,
                interval_seconds: seconds,
                by_id: HashMap::from([(
                    (7, 11),
                    netetw::ProcessNet {
                        send_bytes_per_sec: rate,
                        recv_bytes_per_sec: 0.0,
                        total_bytes_per_sec: rate,
                    },
                )]),
                total_send_bytes_per_sec: rate,
                endpoints: netetw::EndpointSample {
                    enabled: true,
                    measured: true,
                    interval_seconds: seconds,
                    rows: vec![netetw::EndpointTraffic {
                        pid: 7,
                        created: 11,
                        protocol: netetw::Transport::Tcp,
                        local_addr: "127.0.0.1".parse().unwrap(),
                        local_port: 1000,
                        remote_addr: "127.0.0.1".parse().unwrap(),
                        remote_port: 2000,
                        send_bytes_per_sec: rate,
                        recv_bytes_per_sec: 0.0,
                    }],
                    ..Default::default()
                },
                ..Default::default()
            },
            fileetw::Sample {
                enabled: true,
                measured: true,
                interval_seconds: seconds,
                rows: vec![fileetw::Row {
                    pid: 7,
                    created: 11,
                    path: "sample".into(),
                    read_bytes_per_sec: rate,
                    write_bytes_per_sec: 0.0,
                }],
                ..Default::default()
            },
        )
    }

    #[test]
    fn skipped_frames_are_time_weighted_and_cached_timestamps_are_not_recounted() {
        let at = Instant::now();
        let mut pending = Pending::default();
        let (network, files) = sample(0.25, 4000.0);
        pending.push(at, &network, &files);
        pending.push(at, &network, &files);
        let (network, files) = sample(0.75, 0.0);
        pending.push(at + Duration::from_millis(750), &network, &files);
        let (network, files) = pending.take();
        assert_eq!(network.interval_seconds, 1.0);
        assert_eq!(network.by_id[&(7, 11)].send_bytes_per_sec, 1000.0);
        assert_eq!(network.endpoints.rows[0].send_bytes_per_sec, 1000.0);
        assert_eq!(files.rows[0].read_bytes_per_sec, 1000.0);
        pending.push(at + Duration::from_millis(750), &network, &files);
        assert!(!pending.take().0.measured);
    }

    #[test]
    fn unmeasured_frames_and_new_processes_do_not_receive_fabricated_rates() {
        let at = Instant::now();
        let mut pending = Pending::default();
        let (network, files) = sample(0.5, 100.0);
        pending.push(at, &network, &files);
        let mut changed = network.clone();
        changed.by_id.remove(&(7, 11));
        changed.by_id.insert((7, 12), netetw::ProcessNet::default());
        pending.push(at + Duration::from_millis(550), &changed, &files);
        let (network, _) = pending.take();
        assert!(network.measured && network.by_id.is_empty());
        let (mut network, files) = sample(0.5, 100.0);
        network.measured = false;
        pending.push(at + Duration::from_millis(1050), &network, &files);
        network.measured = true;
        pending.push(at + Duration::from_millis(1550), &network, &files);
        let (network, _) = pending.take();
        assert!(!network.measured && network.by_id.is_empty());
        let (network, files) = sample(0.5, 100.0);
        pending.push(at + Duration::from_millis(3050), &network, &files);
        let (missing_network, missing_files) = pending.take();
        assert!(!missing_network.measured && !missing_network.endpoints.measured);
        assert!(missing_network.by_id.is_empty() && missing_network.endpoints.rows.is_empty());
        assert!(!missing_files.measured && missing_files.rows.is_empty());
        pending.push(at + Duration::from_millis(3550), &network, &files);
        assert!(pending.take().0.measured);
        pending.clear();
    }

    /// Reasons are stable codes, translated where they are shown: a cached
    /// frame must follow a language change.
    #[test]
    fn reasons_are_codes_whatever_the_ui_language() {
        use crate::i18n::{with_language, Language};
        let skipped = with_language(Language::Korean, || {
            let at = Instant::now();
            let mut pending = Pending::default();
            let (network, files) = sample(0.5, 100.0);
            pending.push(at, &network, &files);
            pending.push(at + Duration::from_secs(3), &network, &files);
            pending.take()
        });
        let code = "Some resource samples were skipped; waiting for a complete interval";
        assert_eq!(skipped.0.reason.as_deref(), Some(code));
        assert_eq!(skipped.1.reason.as_deref(), Some(code));
    }
}
