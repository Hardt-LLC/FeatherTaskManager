//! Optional, read-only firmware temperatures via Windows' thermal provider.
//!
//! ACPI zones describe firmware-selected sensor locations, not a guaranteed
//! CPU package or motherboard sensor. No driver, WMI, helper process or timer
//! is started. The monitor thread keeps one sampler for its lifetime and hands
//! it between PerfSampler instances, so a pause (menus and dialogs count) does
//! not re-open the query; a reading is never older than [`POLL_INTERVAL`].
//!
//! Microsoft documents the Temperature counter in kelvins:
//! https://learn.microsoft.com/en-us/windows-hardware/design/device-experiences/examples--requirements-and-diagnostics

use crate::performance::counter_array;
use std::collections::BTreeMap;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW, PDH_HCOUNTER,
    PDH_HQUERY,
};

/// Firmware sensors change much more slowly than process utilization.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Missing sensors are normal, especially on desktops. Retry without polling
/// unsupported firmware on every process refresh.
pub const RETRY_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub struct ThermalZone {
    /// Windows thermal-zone instance name, e.g. `\_TZ.TZ00`.
    pub name: String,
    pub celsius: f64,
}

#[derive(Default)]
pub struct ThermalSampler {
    query: Option<ThermalQuery>,
    last_attempt: Option<Instant>,
    zones: Vec<ThermalZone>,
}

impl ThermalSampler {
    pub fn sample(&mut self) -> &[ThermalZone] {
        if self.due(Instant::now()) {
            if self.query.is_none() {
                self.query = ThermalQuery::new();
            }
            let readings = self
                .query
                .as_mut()
                .map(ThermalQuery::read)
                .unwrap_or_default();
            self.update(readings, Instant::now());
        }
        &self.zones
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        let interval = if self.zones.is_empty() {
            RETRY_INTERVAL
        } else {
            POLL_INTERVAL
        };
        self.last_attempt
            .is_none_or(|at| now.saturating_duration_since(at) >= interval)
    }

    fn update(&mut self, readings: Vec<ThermalZone>, now: Instant) {
        // A failed/empty refresh must discard stale temperatures immediately.
        self.zones = readings;
        self.last_attempt = Some(now);
        if self.zones.is_empty() {
            // Release unavailable providers; re-opening on the next retry also
            // discovers sensors that became available after a driver change.
            self.query = None;
        }
    }
}

struct ThermalQuery {
    handle: PDH_HQUERY,
    precise: Option<PDH_HCOUNTER>,
    standard: Option<PDH_HCOUNTER>,
    array_buffer: Vec<usize>,
}

impl ThermalQuery {
    fn new() -> Option<Self> {
        let mut handle = null_mut();
        if unsafe { PdhOpenQueryW(null(), 0, &mut handle) } != 0 {
            return None;
        }
        let mut query = Self {
            handle,
            precise: None,
            standard: None,
            array_buffer: vec![0; 512],
        };
        query.precise = query.add(r"\Thermal Zone Information(*)\High Precision Temperature");
        query.standard = query.add(r"\Thermal Zone Information(*)\Temperature");
        (query.precise.is_some() || query.standard.is_some()).then_some(query)
    }

    fn add(&self, path: &str) -> Option<PDH_HCOUNTER> {
        let path: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let mut counter = null_mut();
        (unsafe { PdhAddEnglishCounterW(self.handle, path.as_ptr(), 0, &mut counter) } == 0)
            .then_some(counter)
    }

    fn read(&mut self) -> Vec<ThermalZone> {
        if unsafe { PdhCollectQueryData(self.handle) } != 0 {
            return Vec::new();
        }
        // These are gauges: unlike CPU/disk rates, they need no second sample.
        // Both counters can register even when one exposes no usable values.
        let precise = self
            .precise
            .and_then(|counter| counter_array(&mut self.array_buffer, counter, None).ok())
            .unwrap_or_default();
        let standard = self
            .standard
            .and_then(|counter| counter_array(&mut self.array_buffer, counter, None).ok())
            .unwrap_or_default();
        combine_zones(&precise, &standard)
    }
}

impl Drop for ThermalQuery {
    fn drop(&mut self) {
        // Closing the query releases both of its counters.
        unsafe { PdhCloseQuery(self.handle) };
    }
}

fn combine_zones(precise: &[(String, f64)], standard: &[(String, f64)]) -> Vec<ThermalZone> {
    let mut zones = BTreeMap::new();
    // The standard counter is whole kelvins; High Precision is decikelvins.
    // Confirmed against the Windows provider's PdhGetCounterInfoW help text.
    // Only a valid precise reading overrides the standard value for that zone.
    for (values, divisor) in [(standard, 1.0), (precise, 10.0)] {
        for (name, value) in values {
            let celsius = value / divisor - 273.15;
            if !name.is_empty()
                && name != "_Total"
                && celsius.is_finite()
                && celsius > 0.0
                && celsius <= 150.0
            {
                zones.insert(name.clone(), celsius);
            }
        }
    }
    zones
        .into_iter()
        .map(|(name, celsius)| ThermalZone { name, celsius })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_precision_overrides_per_zone_and_invalid_precision_falls_back() {
        let precise = vec![
            (r"\_TZ.A".into(), 3131.5),
            (r"\_TZ.B".into(), 0.0),
            (r"\_TZ.HOT".into(), 4500.0),
            (r"\_TZ.ZERO".into(), 2731.5),
            (r"\_TZ.NAN".into(), f64::NAN),
            ("_Total".into(), 3000.0),
        ];
        let standard = vec![(r"\_TZ.A".into(), 310.0), (r"\_TZ.B".into(), 318.15)];
        let zones = combine_zones(&precise, &standard);
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].name, r"\_TZ.A");
        assert!((zones[0].celsius - 40.0).abs() < 1e-9);
        assert_eq!(zones[1].name, r"\_TZ.B");
        assert!((zones[1].celsius - 45.0).abs() < 1e-9);
        assert_eq!(combine_zones(&[], &standard).len(), 2);
        assert!(combine_zones(&[], &[("bad".into(), 0.0)]).is_empty());
    }

    #[test]
    fn refresh_is_throttled_and_failure_discards_stale_values_until_retry() {
        let now = Instant::now();
        let mut sampler = ThermalSampler::default();
        assert!(sampler.due(now));
        sampler.update(
            vec![ThermalZone {
                name: "zone".into(),
                celsius: 40.0,
            }],
            now,
        );
        assert!(!sampler.due(now + POLL_INTERVAL - Duration::from_millis(1)));
        assert!(sampler.due(now + POLL_INTERVAL));
        sampler.update(Vec::new(), now + POLL_INTERVAL);
        assert!(sampler.zones.is_empty());
        assert!(!sampler.due(now + POLL_INTERVAL + RETRY_INTERVAL - Duration::from_millis(1)));
        assert!(sampler.due(now + POLL_INTERVAL + RETRY_INTERVAL));
        assert!(ThermalSampler::default().due(now)); // A new sampler reads immediately.
    }
}
