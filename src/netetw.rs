//! Per-process network throughput from a private real-time ETW session.
//!
//! Windows has no non-elevated user-mode API for live per-process network
//! byte counts. Windows Task Manager (which auto-elevates) reads the same data
//! from an Event Tracing for Windows kernel-network trace. This module does the
//! same, but only when the process is already elevated: it starts a private
//! real-time session for the Microsoft-Windows-Kernel-Network provider, consumes
//! it on a dedicated thread, and aggregates TCP/UDP send and receive bytes per
//! process id.
//!
//! Design notes:
//! - Elevation gate: enabling this provider requires administrator rights, so
//!   [`NetworkMonitor::new`] does nothing when the token is not elevated and
//!   reports a reason the UI can show next to "-".
//! - One session per monitor: the name carries this process's PID, creation
//!   time and a sequence number, and every control call addresses the session
//!   by that name (never by a logger handle that ETW may reuse). A second
//!   Feather instance, `--self-test` or `--dump-hardware` therefore never stops
//!   another instance's trace. Before starting, sessions whose owning process
//!   (PID and creation time) no longer runs are stopped as crash leftovers.
//! - Honesty: a sample is `measured` only while the consumer thread runs, the
//!   session answers a query, and ETW reports no lost events or buffers for the
//!   interval. Bytes whose earliest event predates the process's creation time
//!   (a reused PID whose previous owner's events were delivered late) are not
//!   attributed.
//! - The event callback is allocation-free per event (a hash-map update into a
//!   consumer-thread-local batch); the batch is flushed to the shared totals
//!   once per ETW buffer, so the shared mutex is taken per buffer, not per event.
//! - Only measured bytes are exposed. No connection, address, or port fields are
//!   read, so no IP address, MAC address, or remote endpoint ever leaves ETW.
//!
//! Event ids and payload layout were verified on this machine against the
//! provider manifest (`wevtutil gp Microsoft-Windows-Kernel-Network /ge /gm`):
//! every send/recv event begins with `PID: UInt32` then `size: UInt32`.

use crate::sampler::Process;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_INVALID_PARAMETER,
    ERROR_MORE_DATA, ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND, FILETIME, HANDLE, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, QueryAllTracesW,
    StartTraceW, CONTROLTRACE_HANDLE, ENABLE_TRACE_PARAMETERS, ENABLE_TRACE_PARAMETERS_VERSION_2,
    EVENT_RECORD, EVENT_TRACE_CONTROL_QUERY, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE, PROCESSTRACE_HANDLE,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_REAL_TIME, TRACE_LEVEL_VERBOSE,
    WNODE_FLAG_TRACED_GUID,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

/// Microsoft-Windows-Kernel-Network {7DD42A49-5329-4832-8DFD-43D979153A88}.
const KERNEL_NETWORK_PROVIDER: GUID = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a88);
/// IPv4 (0x10) and IPv6 (0x20) keywords, matching Task Manager's capture.
const KEYWORD_IPV4_IPV6: u64 = 0x10 | 0x20;
/// Session names are `"<prefix><pid> <creation time, 16 hex digits> <sequence>"`.
const SESSION_PREFIX: &str = "Feather Task Manager Network ";
/// ETW session and log file names are at most 1024 characters.
const MAX_NAME_CHARS: usize = 1024;
/// `QueryAllTracesW` accepts at most 64 property buffers.
const MAX_QUERY_SESSIONS: usize = 64;
/// Real-time buffers are bounded so a stuck consumer cannot grow without limit.
const BUFFER_SIZE_KB: u32 = 64;
const MIN_BUFFERS: u32 = 4;
const MAX_BUFFERS: u32 = 64;
/// Flush partly-filled buffers every second so light traffic is still delivered.
const FLUSH_TIMER_SECONDS: u32 = 1;
/// `WNODE_HEADER::ClientContext` 2: system-time timestamps, the clock process
/// creation times use, so event times and `Process::created` are comparable.
const CLOCK_SYSTEM_TIME: u32 = 2;
/// 64-bit invalid consumer handle (`(TRACEHANDLE)INVALID_HANDLE_VALUE`).
const INVALID_PROCESSTRACE_HANDLE: u64 = u64::MAX;
/// EVENT_CONTROL_CODE_ENABLE_PROVIDER.
const ENABLE_PROVIDER: u32 = 1;
/// Intervals reported unmeasured after ETW reports lost events: the interval in
/// which the loss was seen, and the next one, whose buffers may also have been
/// written before the loss was counted (flush lag is up to one second).
const LOSS_HOLD_SAMPLES: u8 = 2;

/// Distinguishes several monitors in one process (e.g. `--self-test`).
static SESSION_SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// TCP/UDP send event ids (IPv4 TCP, IPv6 TCP, IPv4 UDP, IPv6 UDP).
const SEND_IDS: [u16; 4] = [10, 26, 42, 58];
/// TCP/UDP receive event ids in the same order.
const RECV_IDS: [u16; 4] = [11, 27, 43, 59];

/// Bytes sent and received by one process id since the last swap, with the
/// system time (100 ns units) of the earliest contributing event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PidBytes {
    send: u64,
    recv: u64,
    earliest: u64,
}

impl Default for PidBytes {
    fn default() -> Self {
        Self {
            send: 0,
            recv: 0,
            earliest: u64::MAX,
        }
    }
}

impl PidBytes {
    fn merge(&mut self, other: PidBytes) {
        self.send = self.send.saturating_add(other.send);
        self.recv = self.recv.saturating_add(other.recv);
        self.earliest = self.earliest.min(other.earliest);
    }
}

/// Per-process network rates for one sample, in bytes per second.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProcessNet {
    pub send_bytes_per_sec: f64,
    pub recv_bytes_per_sec: f64,
    /// `send + recv`, provided so callers need not recompute it.
    pub total_bytes_per_sec: f64,
}

/// The per-process network result of one monitor iteration.
#[derive(Clone, Debug, Default)]
pub struct ProcessNetworkSample {
    /// Rates are valid for the entries present: a real interval elapsed on a
    /// running session and ETW reported no lost events. False while
    /// unavailable, for an interval with lost events, and during the first
    /// (priming) sample.
    pub measured: bool,
    /// Why this sample has no rates, e.g. "Requires administrator" or
    /// "Network events dropped". `None` when measured, or during the first
    /// priming sample of a running session.
    pub reason: Option<String>,
    /// Seconds since the previous swap; 0 on the first sample.
    pub interval_seconds: f64,
    /// Rates keyed by `(pid, created)` so a reused PID never inherits bytes.
    /// A process present in both this and the previous sample gets a measured
    /// value (0 when it transferred nothing); a new or reused identity is
    /// omitted, so the UI shows "-" until its first full interval.
    pub by_id: HashMap<(u32, u64), ProcessNet>,
    /// Sum of all attributable send bytes per second this interval.
    pub total_send_bytes_per_sec: f64,
    /// Sum of all attributable receive bytes per second this interval.
    pub total_recv_bytes_per_sec: f64,
}

impl ProcessNetworkSample {
    fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    /// A running session whose interval cannot be reported honestly.
    fn unmeasured(reason: impl Into<String>, seconds: f64) -> Self {
        Self {
            reason: Some(reason.into()),
            interval_seconds: seconds,
            ..Self::default()
        }
    }
}

/// State shared between the consumer thread and the sampler.
struct Shared {
    /// Bytes accumulated per PID since the last [`Shared::drain`].
    totals: Mutex<HashMap<u32, PidBytes>>,
    /// Set on shutdown; the buffer callback returns false to end `ProcessTrace`.
    stop: AtomicBool,
    /// Total send/recv events processed, for diagnostics.
    events: AtomicU64,
    /// Set by the consumer thread once `ProcessTrace` has returned.
    ended: AtomicBool,
    /// `ProcessTrace`'s return value, valid once `ended` is set.
    status: AtomicU32,
}

impl Shared {
    fn new() -> Self {
        Self {
            totals: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
            events: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            status: AtomicU32::new(ERROR_SUCCESS),
        }
    }

    fn drain(&self) -> HashMap<u32, PidBytes> {
        match self.totals.lock() {
            Ok(mut totals) => std::mem::take(&mut *totals),
            Err(_) => HashMap::new(),
        }
    }
}

/// Owned by the consumer thread; reached from both ETW callbacks by raw pointer.
/// Only the consumer thread touches `batch`, so it needs no lock.
struct Consumer {
    batch: HashMap<u32, PidBytes>,
    shared: Arc<Shared>,
}

/// The consumer's heap address (from `Box::into_raw`), moved into the consumer
/// thread. It is freed exactly once: by that thread after `ProcessTrace`
/// returns, or by `start_session` when the thread could not be spawned.
struct ConsumerPtr(*mut Consumer);

// SAFETY: after the move the pointee is used only on the consumer thread (by
// the ETW callbacks that `ProcessTrace` runs there, then by the final free).
unsafe impl Send for ConsumerPtr {}

impl ConsumerPtr {
    /// Takes the pointer by value, so a closure captures the whole wrapper.
    fn into_raw(self) -> *mut Consumer {
        self.0
    }
}

/// A running real-time session and its consumer thread.
struct Session {
    /// This session's unique, NUL-terminated name. All control calls use it.
    name: Vec<u16>,
    trace: PROCESSTRACE_HANDLE,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    stopped: bool,
    /// Reused buffer for the per-sample `EVENT_TRACE_CONTROL_QUERY`.
    query: TraceProperties,
    /// Backs `EVENT_TRACE_LOGFILEW::LoggerName` for the session's lifetime.
    _logname: Box<[u16]>,
}

impl Session {
    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.shared.stop.store(true, Ordering::Release);
        // Stopping the kernel session flushes remaining buffers and makes
        // `ProcessTrace` return; the buffer callback also returns false. The
        // name is unique to this session, so this never stops another one.
        stop_by_name(&self.name);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // Safe to close only after `ProcessTrace` has returned (thread joined).
        if self.trace.Value != INVALID_PROCESSTRACE_HANDLE {
            unsafe {
                CloseTrace(self.trace);
            }
        }
    }

    /// Cumulative lost events plus lost buffers reported by ETW for this
    /// session, or the Win32 error of the query.
    fn lost(&mut self) -> Result<u64, u32> {
        self.query.reset_for_control();
        let status = unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                self.name.as_ptr(),
                self.query.as_mut_ptr(),
                EVENT_TRACE_CONTROL_QUERY,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(status);
        }
        let p = self.query.properties();
        Ok(
            u64::from(p.EventsLost)
                + u64::from(p.RealTimeBuffersLost)
                + u64::from(p.LogBuffersLost),
        )
    }

    /// `ProcessTrace`'s status once the consumer thread has ended.
    fn ended(&self) -> Option<u32> {
        let finished = self.shared.ended.load(Ordering::Acquire)
            || self.thread.as_ref().is_some_and(JoinHandle::is_finished);
        finished.then(|| self.shared.status.load(Ordering::Acquire))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Collects per-process network throughput across monitor iterations.
///
/// Construct one next to the monitor loop and call [`NetworkMonitor::sample`]
/// each iteration with the current process list. When the process is not
/// elevated, or the session cannot start or stops, every sample reports
/// `measured: false` with a reason and no per-process values, leaving
/// non-elevated behaviour unchanged.
pub struct NetworkMonitor {
    session: Option<Session>,
    reason: Option<String>,
    last_swap: Option<Instant>,
    /// Identities seen at the previous sample, to reject PID reuse.
    previous: HashMap<u32, u64>,
    /// ETW's cumulative lost count at the last successful query.
    lost_baseline: Option<u64>,
    /// Remaining intervals to report unmeasured after lost events.
    loss_hold: u8,
    /// Events processed by a session that has since ended.
    ended_events: u64,
}

impl NetworkMonitor {
    /// Starts the session when elevated; otherwise records why it is unavailable.
    pub fn new() -> Self {
        if !is_elevated() {
            return Self::from_start(Err(requires_admin()));
        }
        Self::from_start(start_session(true))
    }

    fn from_start(result: Result<Session, String>) -> Self {
        let (session, reason) = match result {
            Ok(session) => (Some(session), None),
            Err(reason) => (None, Some(reason)),
        };
        Self {
            session,
            reason,
            last_swap: None,
            previous: HashMap::new(),
            lost_baseline: None,
            loss_hold: 0,
            ended_events: 0,
        }
    }

    /// Total send/recv events the session has processed (diagnostics only).
    pub fn event_count(&self) -> u64 {
        self.session.as_ref().map_or(self.ended_events, |s| {
            s.shared.events.load(Ordering::Relaxed)
        })
    }

    /// Swaps out the accumulated byte counts and converts them to rates using
    /// the real elapsed time since the previous call.
    pub fn sample(&mut self, processes: &[Process]) -> ProcessNetworkSample {
        let Some(session) = self.session.as_mut() else {
            return ProcessNetworkSample::unavailable(
                self.reason.clone().unwrap_or_else(requires_admin),
            );
        };
        // Query first, then drain, then check the thread: a trace that ended
        // before the drain is never reported as a measured interval.
        let lost = session.lost();
        let now = Instant::now();
        let raw = session.shared.drain();
        let ended = session.ended();
        self.account(processes, now, raw, lost, ended)
    }

    /// Converts one interval's drained bytes to a sample. Separate from
    /// [`NetworkMonitor::sample`] so the rules are testable without ETW.
    fn account(
        &mut self,
        processes: &[Process],
        now: Instant,
        raw: HashMap<u32, PidBytes>,
        lost: Result<u64, u32>,
        ended: Option<u32>,
    ) -> ProcessNetworkSample {
        if let Some(status) = ended {
            return self.end_session(stopped_reason(status));
        }
        if lost == Err(ERROR_WMI_INSTANCE_NOT_FOUND) {
            // Someone stopped the session; the consumer is about to return.
            return self.end_session(stopped_reason(ERROR_SUCCESS));
        }
        let priming = self.last_swap.is_none();
        let previous_swap = self.last_swap.replace(now);
        let previous = std::mem::replace(&mut self.previous, identities(processes));
        // ETW's lost counters are cumulative since the session started.
        let query_error = match lost {
            Ok(total) => {
                let increased = match self.lost_baseline {
                    Some(baseline) => total > baseline,
                    None => !priming && total > 0,
                };
                self.lost_baseline = Some(total);
                if increased {
                    self.loss_hold = LOSS_HOLD_SAMPLES;
                }
                None
            }
            Err(status) => Some(status),
        };
        let Some(previous_swap) = previous_swap else {
            // First observation: bytes cover an unknown span since start; prime.
            return ProcessNetworkSample::default();
        };
        let seconds = now.saturating_duration_since(previous_swap).as_secs_f64();
        if seconds <= 0.0 {
            return ProcessNetworkSample::default();
        }
        if let Some(status) = query_error {
            return ProcessNetworkSample::unmeasured(
                format!("Cannot check network event loss (error {status})"),
                seconds,
            );
        }
        if self.loss_hold > 0 {
            self.loss_hold -= 1;
            return ProcessNetworkSample::unmeasured("Network events dropped", seconds);
        }
        let mut total_send = 0.0;
        let mut total_recv = 0.0;
        let mut by_id = HashMap::with_capacity(processes.len());
        for process in processes {
            // Report only identities continuous across the interval; a new or
            // reused (pid, created) cannot be attributed the interval's bytes.
            if previous.get(&process.pid) != Some(&process.created) {
                continue;
            }
            let bytes = raw.get(&process.pid).copied().unwrap_or_default();
            // Events older than this process belong to an earlier owner of the
            // PID whose buffers were delivered late; they cannot be split off.
            if bytes.earliest < process.created {
                continue;
            }
            let send = bytes.send as f64 / seconds;
            let recv = bytes.recv as f64 / seconds;
            total_send += send;
            total_recv += recv;
            by_id.insert(
                (process.pid, process.created),
                ProcessNet {
                    send_bytes_per_sec: send,
                    recv_bytes_per_sec: recv,
                    total_bytes_per_sec: send + recv,
                },
            );
        }
        ProcessNetworkSample {
            measured: true,
            reason: None,
            interval_seconds: seconds,
            by_id,
            total_send_bytes_per_sec: total_send,
            total_recv_bytes_per_sec: total_recv,
        }
    }

    /// Tears down a session that stopped and reports `reason` from now on.
    fn end_session(&mut self, reason: String) -> ProcessNetworkSample {
        if let Some(mut session) = self.session.take() {
            self.ended_events = session.shared.events.load(Ordering::Relaxed);
            session.stop();
        }
        self.reason = Some(reason.clone());
        self.last_swap = None;
        self.previous.clear();
        ProcessNetworkSample::unavailable(reason)
    }
}

impl Default for NetworkMonitor {
    fn default() -> Self {
        Self::new()
    }
}

fn identities(processes: &[Process]) -> HashMap<u32, u64> {
    processes.iter().map(|p| (p.pid, p.created)).collect()
}

fn requires_admin() -> String {
    "Requires administrator".to_string()
}

fn stopped_reason(status: u32) -> String {
    if status == ERROR_SUCCESS {
        "Network trace stopped".to_string()
    } else {
        format!("Network trace stopped (error {status})")
    }
}

/// A UTF-16, NUL-terminated copy of a string for a Win32 wide-string argument.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn session_name(pid: u32, created: u64, sequence: u32) -> String {
    format!("{SESSION_PREFIX}{pid} {created:016X} {sequence}")
}

/// The owning process identity encoded in a Feather session name.
fn parse_session_name(name: &str) -> Option<(u32, u64)> {
    let mut parts = name.strip_prefix(SESSION_PREFIX)?.split(' ');
    let pid = parts.next()?.parse::<u32>().ok()?;
    let created = parts.next().filter(|text| text.len() == 16)?;
    let created = u64::from_str_radix(created, 16).ok()?;
    parts.next()?.parse::<u32>().ok()?;
    parts.next().is_none().then_some((pid, created))
}

/// An `EVENT_TRACE_PROPERTIES` buffer with room for a session name and a log
/// file name after it, aligned for the struct's 8-byte fields by backing it
/// with `u64`s.
struct TraceProperties {
    words: Vec<u64>,
}

impl TraceProperties {
    const LOGGER_NAME_OFFSET: usize = size_of::<EVENT_TRACE_PROPERTIES>();
    const LOG_FILE_NAME_OFFSET: usize = Self::LOGGER_NAME_OFFSET + MAX_NAME_CHARS * 2;
    const BYTES: usize = Self::LOG_FILE_NAME_OFFSET + MAX_NAME_CHARS * 2;

    fn zeroed() -> Self {
        Self {
            words: vec![0u64; Self::BYTES.div_ceil(size_of::<u64>())],
        }
    }

    fn as_mut_ptr(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        self.words.as_mut_ptr().cast()
    }

    fn properties(&self) -> &EVENT_TRACE_PROPERTIES {
        // The allocation is larger than the struct and 8-byte aligned.
        unsafe { &*self.words.as_ptr().cast() }
    }

    fn properties_mut(&mut self) -> &mut EVENT_TRACE_PROPERTIES {
        unsafe { &mut *self.as_mut_ptr() }
    }

    fn byte_size(&self) -> u32 {
        (self.words.len() * size_of::<u64>()) as u32
    }

    /// Properties for `ControlTraceW` and `QueryAllTracesW`, which write the
    /// session and log file names back into the trailing space.
    fn for_control() -> Self {
        let mut props = Self::zeroed();
        props.reset_for_control();
        props
    }

    fn reset_for_control(&mut self) {
        self.words.fill(0);
        let size = self.byte_size();
        let p = self.properties_mut();
        p.Wnode.BufferSize = size;
        p.LoggerNameOffset = Self::LOGGER_NAME_OFFSET as u32;
        p.LogFileNameOffset = Self::LOG_FILE_NAME_OFFSET as u32;
    }

    /// Properties for `StartTraceW`: a private real-time session with bounded
    /// buffers and system-time timestamps. `StartTraceW` copies the session
    /// name into the trailing bytes; there is no log file.
    fn for_start() -> Self {
        let mut props = Self::zeroed();
        let size = props.byte_size();
        let p = props.properties_mut();
        p.Wnode.BufferSize = size;
        p.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        p.Wnode.ClientContext = CLOCK_SYSTEM_TIME;
        p.BufferSize = BUFFER_SIZE_KB;
        p.MinimumBuffers = MIN_BUFFERS;
        p.MaximumBuffers = MAX_BUFFERS;
        p.FlushTimer = FLUSH_TIMER_SECONDS;
        p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
        p.LoggerNameOffset = Self::LOGGER_NAME_OFFSET as u32;
        props
    }

    /// The session name ETW wrote at `LoggerNameOffset`, bounded to the buffer.
    fn logger_name(&self) -> Option<String> {
        let offset = self.properties().LoggerNameOffset as usize;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                self.words.as_ptr().cast::<u8>(),
                self.words.len() * size_of::<u64>(),
            )
        };
        if offset < Self::LOGGER_NAME_OFFSET {
            return None;
        }
        let units: Vec<u16> = bytes
            .get(offset..)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_le_bytes(pair))
            .take_while(|&unit| unit != 0)
            .collect();
        String::from_utf16(&units).ok()
    }
}

/// Stops a session by its unique NUL-terminated name; returns the Win32 status.
fn stop_by_name(name: &[u16]) -> u32 {
    let mut props = TraceProperties::for_control();
    unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            name.as_ptr(),
            props.as_mut_ptr(),
            EVENT_TRACE_CONTROL_STOP,
        )
    }
}

/// Names and owner identities of every running Feather network session.
fn feather_sessions() -> Vec<(String, u32, u64)> {
    let mut buffers: Vec<TraceProperties> = (0..MAX_QUERY_SESSIONS)
        .map(|_| TraceProperties::for_control())
        .collect();
    let mut pointers: Vec<*mut EVENT_TRACE_PROPERTIES> = buffers
        .iter_mut()
        .map(TraceProperties::as_mut_ptr)
        .collect();
    let mut count = 0u32;
    let status =
        unsafe { QueryAllTracesW(pointers.as_mut_ptr(), MAX_QUERY_SESSIONS as u32, &mut count) };
    if status != ERROR_SUCCESS && status != ERROR_MORE_DATA {
        return Vec::new();
    }
    buffers
        .iter()
        .take((count as usize).min(MAX_QUERY_SESSIONS))
        .filter_map(|props| {
            let name = props.logger_name()?;
            let (pid, created) = parse_session_name(&name)?;
            Some((name, pid, created))
        })
        .collect()
}

/// Stops Feather sessions whose owning process (PID and creation time) no
/// longer runs: leftovers of a crash. Live instances' sessions are kept.
fn stop_stale_sessions() {
    for (name, pid, created) in feather_sessions() {
        if !process_alive(pid, created) {
            stop_by_name(&wide(&name));
        }
    }
}

/// True unless the process with this PID and creation time is known to be gone.
fn process_alive(pid: u32, created: u64) -> bool {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        // Only "no such process" proves the owner is gone; access denied (a
        // protected process) keeps the session.
        return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
    }
    let running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
    let started = process_created(handle);
    unsafe { CloseHandle(handle) };
    running && started.is_none_or(|started| started == created)
}

/// A process's creation time in 100 ns units, the value `Process::created`
/// and session names carry.
fn process_created(handle: HANDLE) -> Option<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let ok = unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    (ok != 0).then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

fn own_identity() -> Option<(u32, u64)> {
    Some((
        std::process::id(),
        process_created(unsafe { GetCurrentProcess() })?,
    ))
}

/// Starts a uniquely named session, enables the provider (when asked; the
/// lifecycle test starts one without it), opens the consumer, and spawns the
/// `ProcessTrace` thread. On any failure the partially-started session is
/// stopped before returning.
fn start_session(enable_provider: bool) -> Result<Session, String> {
    let (pid, created) =
        own_identity().ok_or_else(|| "Cannot read this process's creation time".to_string())?;
    stop_stale_sessions();
    let sequence = SESSION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = wide(&session_name(pid, created, sequence));

    let mut props = TraceProperties::for_start();
    let mut control = CONTROLTRACE_HANDLE { Value: 0 };
    let mut status = unsafe { StartTraceW(&mut control, name.as_ptr(), props.as_mut_ptr()) };
    if status == ERROR_ALREADY_EXISTS {
        // Only this process can have used this name; a leftover is ours.
        stop_by_name(&name);
        props = TraceProperties::for_start();
        control = CONTROLTRACE_HANDLE { Value: 0 };
        status = unsafe { StartTraceW(&mut control, name.as_ptr(), props.as_mut_ptr()) };
    }
    if status != ERROR_SUCCESS {
        return Err(start_error(status));
    }

    if enable_provider {
        let params = ENABLE_TRACE_PARAMETERS {
            Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
            ..Default::default()
        };
        let status = unsafe {
            EnableTraceEx2(
                control,
                &KERNEL_NETWORK_PROVIDER,
                ENABLE_PROVIDER,
                TRACE_LEVEL_VERBOSE as u8,
                KEYWORD_IPV4_IPV6,
                0,
                0,
                &params,
            )
        };
        if status != ERROR_SUCCESS {
            stop_by_name(&name);
            // A Performance Log Users member can create the session but still
            // cannot enable this kernel provider; that is an elevation
            // requirement too.
            return Err(if status == ERROR_ACCESS_DENIED {
                requires_admin()
            } else {
                format!("Cannot enable the network provider (error {status})")
            });
        }
    }

    let shared = Arc::new(Shared::new());
    let context = Box::into_raw(Box::new(Consumer {
        batch: HashMap::new(),
        shared: Arc::clone(&shared),
    }));

    let mut logname = name.clone().into_boxed_slice();
    let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
    logfile.LoggerName = logname.as_mut_ptr();
    logfile.Anonymous1.ProcessTraceMode =
        PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
    logfile.Anonymous2.EventRecordCallback = Some(on_event);
    logfile.BufferCallback = Some(on_buffer);
    logfile.Context = context.cast::<c_void>();

    let trace = unsafe { OpenTraceW(&mut logfile) };
    if trace.Value == INVALID_PROCESSTRACE_HANDLE {
        let error = std::io::Error::last_os_error();
        // No callback runs without ProcessTrace, so the consumer is unused.
        drop(unsafe { Box::from_raw(context) });
        stop_by_name(&name);
        return Err(format!("Cannot open the network trace: {error}"));
    }

    let pointer = ConsumerPtr(context);
    let thread_shared = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("feather-netetw".into())
        .spawn(move || {
            let context = pointer.into_raw();
            let handles = [trace];
            // Blocks until the session is stopped or the buffer callback
            // returns false, then returns.
            let status = unsafe { ProcessTrace(handles.as_ptr(), 1, null(), null()) };
            thread_shared.status.store(status, Ordering::Release);
            thread_shared.ended.store(true, Ordering::Release);
            // SAFETY: ProcessTrace has returned, so no callback can still use
            // the consumer, and this thread is its only owner.
            drop(unsafe { Box::from_raw(context) });
        });
    let thread = match spawned {
        Ok(thread) => thread,
        Err(error) => {
            // The closure never ran, so the consumer was not freed; no callback
            // can fire because ProcessTrace never ran.
            unsafe {
                CloseTrace(trace);
            }
            drop(unsafe { Box::from_raw(context) });
            stop_by_name(&name);
            return Err(format!("Cannot start the network consumer thread: {error}"));
        }
    };

    Ok(Session {
        name,
        trace,
        thread: Some(thread),
        shared,
        stopped: false,
        query: TraceProperties::for_control(),
        _logname: logname,
    })
}

fn start_error(status: u32) -> String {
    if status == ERROR_ACCESS_DENIED {
        requires_admin()
    } else {
        format!("Cannot start the network trace (error {status})")
    }
}

/// Whether two GUIDs are equal (`windows_sys::core::GUID` has no `PartialEq`).
fn same_guid(a: &GUID, b: &GUID) -> bool {
    (a.data1, a.data2, a.data3, a.data4) == (b.data1, b.data2, b.data3, b.data4)
}

/// ETW event callback. Runs on the consumer thread only. Accepts only events
/// of the Microsoft-Windows-Kernel-Network provider: another provider enabled
/// on the session (anyone allowed to control it could add one) must not be
/// able to inject byte counts with matching event ids. Reads the process id
/// and byte size (the first two payload fields for every send/recv event) and
/// adds them, with the event's system time, to the consumer-thread-local
/// batch; no address or port is read.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let record = &*record;
    let context = record.UserContext.cast::<Consumer>();
    if context.is_null() {
        return;
    }
    if !same_guid(&record.EventHeader.ProviderId, &KERNEL_NETWORK_PROVIDER) {
        return;
    }
    let id = record.EventHeader.EventDescriptor.Id;
    let send = SEND_IDS.contains(&id);
    let recv = RECV_IDS.contains(&id);
    if !send && !recv {
        return;
    }
    if let Some((pid, size)) = read_pid_size(record.UserData, record.UserDataLength) {
        let consumer = &mut *context;
        let entry = consumer.batch.entry(pid).or_default();
        if send {
            entry.send = entry.send.saturating_add(u64::from(size));
        } else {
            entry.recv = entry.recv.saturating_add(u64::from(size));
        }
        // A negative (invalid) time becomes 0, which predates every process,
        // so such bytes are never attributed.
        let time = u64::try_from(record.EventHeader.TimeStamp).unwrap_or(0);
        entry.earliest = entry.earliest.min(time);
        consumer.shared.events.fetch_add(1, Ordering::Relaxed);
    }
}

/// Reads the leading `PID: u32, size: u32` of an event payload, if present.
///
/// # Safety
/// `data` must be null or point to at least `len` readable bytes (the ETW
/// payload buffer), which the callback guarantees.
unsafe fn read_pid_size(data: *const c_void, len: u16) -> Option<(u32, u32)> {
    if data.is_null() || (len as usize) < 8 {
        return None;
    }
    let bytes = data.cast::<u8>();
    let read_u32 = |offset: usize| {
        u32::from_le_bytes([
            *bytes.add(offset),
            *bytes.add(offset + 1),
            *bytes.add(offset + 2),
            *bytes.add(offset + 3),
        ])
    };
    Some((read_u32(0), read_u32(4)))
}

/// ETW buffer callback. Runs on the consumer thread after each buffer is
/// delivered. Flushes the batch to the shared totals under one lock, and stops
/// `ProcessTrace` promptly once shutdown was requested.
unsafe extern "system" fn on_buffer(logfile: *mut EVENT_TRACE_LOGFILEW) -> u32 {
    if logfile.is_null() {
        return 1;
    }
    let context = (*logfile).Context.cast::<Consumer>();
    if context.is_null() {
        return 1;
    }
    let consumer = &mut *context;
    if !consumer.batch.is_empty() {
        if let Ok(mut totals) = consumer.shared.totals.lock() {
            for (pid, bytes) in consumer.batch.drain() {
                totals.entry(pid).or_default().merge(bytes);
            }
        } else {
            consumer.batch.clear();
        }
    }
    // Returning false (0) ends ProcessTrace; keep consuming otherwise.
    u32::from(!consumer.shared.stop.load(Ordering::Acquire))
}

/// True when the current process token is elevated (`TokenElevation`). A
/// kernel-network ETW session requires it.
pub(crate) fn is_elevated() -> bool {
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::OpenProcessToken;
    let mut token = null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return false;
    }
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    };
    unsafe { CloseHandle(token) };
    ok != 0 && elevation.TokenIsElevated != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Builds a synthetic send/recv event payload: PID then size, little-endian.
    fn payload(pid: u32, size: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&pid.to_le_bytes());
        bytes.extend_from_slice(&size.to_le_bytes());
        // Trailing address/port fields the parser must ignore.
        bytes.extend_from_slice(&[0xFF; 24]);
        bytes
    }

    #[test]
    fn reads_pid_and_size_and_ignores_trailing_fields() {
        let data = payload(0x1234, 1460);
        let parsed = unsafe { read_pid_size(data.as_ptr().cast(), data.len() as u16) };
        assert_eq!(parsed, Some((0x1234, 1460)));
    }

    #[test]
    fn rejects_short_or_null_payloads() {
        assert_eq!(unsafe { read_pid_size(null(), 0) }, None);
        let short = [0u8; 7];
        assert_eq!(
            unsafe { read_pid_size(short.as_ptr().cast(), short.len() as u16) },
            None
        );
        // A length shorter than the buffer must be honoured.
        let data = payload(9, 9);
        assert_eq!(unsafe { read_pid_size(data.as_ptr().cast(), 4) }, None);
    }

    /// Drives the event and buffer callbacks against a synthetic consumer, the
    /// way ETW would, and checks the aggregation without a live session.
    fn feed(consumer: &mut Consumer, id: u16, pid: u32, size: u32, time: i64) {
        feed_from(consumer, KERNEL_NETWORK_PROVIDER, id, pid, size, time);
    }

    fn feed_from(consumer: &mut Consumer, provider: GUID, id: u16, pid: u32, size: u32, time: i64) {
        let data = payload(pid, size);
        let mut record: EVENT_RECORD = unsafe { std::mem::zeroed() };
        record.EventHeader.ProviderId = provider;
        record.EventHeader.EventDescriptor.Id = id;
        record.EventHeader.TimeStamp = time;
        record.UserData = data.as_ptr() as *mut c_void;
        record.UserDataLength = data.len() as u16;
        record.UserContext = (consumer as *mut Consumer).cast();
        unsafe { on_event(&mut record) };
    }

    fn flush(consumer: &mut Consumer) {
        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
        logfile.Context = (consumer as *mut Consumer).cast();
        assert_eq!(unsafe { on_buffer(&mut logfile) }, 1);
    }

    fn consumer() -> Consumer {
        Consumer {
            batch: HashMap::new(),
            shared: Arc::new(Shared::new()),
        }
    }

    #[test]
    fn aggregates_send_and_recv_per_pid_across_protocols() {
        let mut consumer = consumer();
        feed(&mut consumer, 10, 100, 1000, 500); // TCPv4 send
        feed(&mut consumer, 42, 100, 500, 300); // UDPv4 send
        feed(&mut consumer, 11, 100, 2000, 700); // TCPv4 recv
        feed(&mut consumer, 27, 200, 4096, 900); // TCPv6 recv
        feed(&mut consumer, 58, 200, 128, 800); // UDPv6 send
        feed(&mut consumer, 99, 200, 9999, 1); // unknown id: ignored
        flush(&mut consumer);
        // A later buffer merges into the same totals; the earliest time wins.
        feed(&mut consumer, 10, 100, 1, 100);
        flush(&mut consumer);
        let totals = consumer.shared.drain();
        let first = totals.get(&100).copied().unwrap();
        assert_eq!((first.send, first.recv, first.earliest), (1501, 2000, 100));
        let second = totals.get(&200).copied().unwrap();
        assert_eq!(
            (second.send, second.recv, second.earliest),
            (128, 4096, 800)
        );
        assert_eq!(consumer.shared.events.load(Ordering::Relaxed), 6);
        // A second drain is empty; totals were swapped out.
        assert!(consumer.shared.drain().is_empty());
    }

    #[test]
    fn events_of_other_providers_are_ignored() {
        let mut consumer = consumer();
        // Same send/recv ids, but not Microsoft-Windows-Kernel-Network: a
        // provider someone else enabled on the session, or a zeroed header.
        let other = GUID::from_u128(0x7dd42a49_5329_4832_8dfd_43d979153a89);
        feed_from(&mut consumer, other, 10, 100, 1000, 500);
        feed_from(&mut consumer, GUID::default(), 11, 100, 2000, 500);
        feed(&mut consumer, 10, 200, 64, 500);
        flush(&mut consumer);
        let totals = consumer.shared.drain();
        assert!(!totals.contains_key(&100));
        assert_eq!(totals.get(&200).map(|t| t.send), Some(64));
        assert_eq!(consumer.shared.events.load(Ordering::Relaxed), 1);
        assert!(same_guid(
            &KERNEL_NETWORK_PROVIDER,
            &KERNEL_NETWORK_PROVIDER
        ));
        assert!(!same_guid(&KERNEL_NETWORK_PROVIDER, &other));
    }

    #[test]
    fn negative_event_time_is_never_attributable() {
        let mut consumer = consumer();
        feed(&mut consumer, 10, 7, 10, -5);
        flush(&mut consumer);
        assert_eq!(consumer.shared.drain().get(&7).unwrap().earliest, 0);
    }

    #[test]
    fn buffer_callback_stops_when_requested() {
        let mut consumer = consumer();
        consumer.shared.stop.store(true, Ordering::Release);
        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
        logfile.Context = (&mut consumer as *mut Consumer).cast();
        assert_eq!(unsafe { on_buffer(&mut logfile) }, 0);
    }

    #[test]
    fn start_properties_are_bounded_real_time_system_time_with_room_for_the_name() {
        let props = TraceProperties::for_start();
        let p = props.properties();
        assert_eq!(p.LogFileMode, EVENT_TRACE_REAL_TIME_MODE);
        assert_eq!(p.MaximumBuffers, MAX_BUFFERS);
        assert_eq!(p.FlushTimer, FLUSH_TIMER_SECONDS);
        assert_eq!(p.Wnode.ClientContext, CLOCK_SYSTEM_TIME);
        assert_eq!(p.LogFileNameOffset, 0);
        assert_eq!(
            p.LoggerNameOffset,
            size_of::<EVENT_TRACE_PROPERTIES>() as u32
        );
        assert!(p.Wnode.BufferSize as usize >= TraceProperties::BYTES);
        let control = TraceProperties::for_control();
        let c = control.properties();
        assert!(c.LogFileNameOffset as usize + MAX_NAME_CHARS * 2 <= c.Wnode.BufferSize as usize);
    }

    #[test]
    fn logger_name_reads_back_bounded_text() {
        let mut props = TraceProperties::for_control();
        let name = session_name(42, 0x01DC_0000_1234_5678, 3);
        let offset = TraceProperties::LOGGER_NAME_OFFSET;
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(
                props.words.as_mut_ptr().cast::<u8>(),
                props.words.len() * 8,
            )
        };
        for (index, unit) in name.encode_utf16().enumerate() {
            bytes[offset + index * 2..offset + index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(props.logger_name().as_deref(), Some(name.as_str()));
        // An offset outside the buffer yields nothing instead of a wild read.
        props.properties_mut().LoggerNameOffset = u32::MAX;
        assert_eq!(props.logger_name(), None);
        props.properties_mut().LoggerNameOffset = 4;
        assert_eq!(props.logger_name(), None);
    }

    #[test]
    fn session_names_round_trip_and_reject_others() {
        let name = session_name(1234, 0x01DC_2F00_AABB_CCDD, 7);
        assert_eq!(name, "Feather Task Manager Network 1234 01DC2F00AABBCCDD 7");
        assert_eq!(
            parse_session_name(&name),
            Some((1234, 0x01DC_2F00_AABB_CCDD))
        );
        for other in [
            "Feather Task Manager Network",
            "Feather Task Manager Network 1234",
            "Feather Task Manager Network 1234 01DC2F00AABBCCDD",
            "Feather Task Manager Network 1234 1DC2F00AABBCCDD 7",
            "Feather Task Manager Network 1234 01DC2F00AABBCCDD 7 x",
            "Feather Task Manager Network x 01DC2F00AABBCCDD 7",
            "NT Kernel Logger",
            "",
        ] {
            assert_eq!(parse_session_name(other), None, "{other:?}");
        }
    }

    #[test]
    fn own_process_is_alive_and_other_identities_are_not() {
        let (pid, created) = own_identity().expect("own creation time");
        assert!(created > 0);
        assert!(process_alive(pid, created));
        // Same PID, different creation time: a reused PID is not the owner.
        assert!(!process_alive(pid, created ^ 1));
        // PIDs are multiples of 4; an odd PID never names a process.
        assert!(!process_alive(0xFFFF_FFF1, created));
    }

    fn proc(pid: u32, created: u64) -> Process {
        Process {
            pid,
            parent_pid: 0,
            name: format!("p{pid}"),
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

    #[test]
    fn unavailable_when_not_started() {
        let mut monitor = NetworkMonitor::from_start(Err(requires_admin()));
        let sample = monitor.sample(&[proc(1, 1)]);
        assert!(!sample.measured);
        assert_eq!(sample.reason.as_deref(), Some("Requires administrator"));
        assert!(sample.by_id.is_empty());
    }

    /// A monitor with a fake running session, so the accounting rules can be
    /// tested without elevation or a live trace.
    fn fake_running() -> NetworkMonitor {
        NetworkMonitor::from_start(Ok(Session {
            name: vec![0],
            trace: PROCESSTRACE_HANDLE {
                Value: INVALID_PROCESSTRACE_HANDLE,
            },
            thread: None,
            shared: Arc::new(Shared::new()),
            stopped: true, // No real session; stop() is a no-op.
            query: TraceProperties::for_control(),
            _logname: Box::new([0u16]),
        }))
    }

    fn bytes(entries: &[(u32, u64, u64, u64)]) -> HashMap<u32, PidBytes> {
        entries
            .iter()
            .map(|&(pid, send, recv, earliest)| {
                (
                    pid,
                    PidBytes {
                        send,
                        recv,
                        earliest,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn first_sample_primes_then_reports_rates() {
        let mut monitor = fake_running();
        let t0 = Instant::now();
        let processes = [proc(100, 1), proc(200, 1)];
        // First sample discards the bytes accumulated since start.
        let first = monitor.account(
            &processes,
            t0,
            bytes(&[(100, 10_000, 20_000, 5)]),
            Ok(0),
            None,
        );
        assert!(!first.measured);
        assert!(first.reason.is_none());
        assert!(first.by_id.is_empty());
        // The next interval's bytes become a rate over the real elapsed time.
        let second = monitor.account(
            &processes,
            t0 + Duration::from_secs(2),
            bytes(&[(100, 2_000, 4_000, 5)]),
            Ok(0),
            None,
        );
        assert!(second.measured);
        assert_eq!(second.interval_seconds, 2.0);
        let rate = second.by_id.get(&(100, 1)).copied().unwrap();
        assert_eq!(rate.send_bytes_per_sec, 1000.0);
        assert_eq!(rate.recv_bytes_per_sec, 2000.0);
        assert_eq!(rate.total_bytes_per_sec, 3000.0);
        // A continuing process with no traffic gets a measured zero, not "-".
        let idle = second.by_id.get(&(200, 1)).copied().unwrap();
        assert_eq!(idle.total_bytes_per_sec, 0.0);
        assert_eq!(second.total_recv_bytes_per_sec, 2000.0);
    }

    #[test]
    fn reused_pid_does_not_inherit_bytes() {
        let mut monitor = fake_running();
        let t0 = Instant::now();
        let _ = monitor.account(&[proc(100, 1)], t0, HashMap::new(), Ok(0), None);
        // Same PID, new creation time: the identity changed, so it is omitted.
        let sample = monitor.account(
            &[proc(100, 2)],
            t0 + Duration::from_secs(1),
            bytes(&[(100, 5_000, 5_000, 3)]),
            Ok(0),
            None,
        );
        assert!(sample.measured);
        assert!(!sample.by_id.contains_key(&(100, 2)));
        assert!(!sample.by_id.contains_key(&(100, 1)));
    }

    #[test]
    fn late_bytes_of_a_previous_pid_owner_are_not_attributed() {
        let mut monitor = fake_running();
        let t0 = Instant::now();
        let second = Duration::from_secs(1);
        let _ = monitor.account(&[proc(100, 1_000)], t0, HashMap::new(), Ok(0), None);
        // PID 100 is reused (created 5_000); the new owner is omitted once.
        let reused = [proc(100, 5_000)];
        let _ = monitor.account(&reused, t0 + second, HashMap::new(), Ok(0), None);
        // The old owner's buffered bytes (event time 900) arrive late: the new
        // owner is continuing now, but those bytes predate it.
        let late = monitor.account(
            &reused,
            t0 + second * 2,
            bytes(&[(100, 9_000, 9_000, 900)]),
            Ok(0),
            None,
        );
        assert!(late.measured);
        assert!(!late.by_id.contains_key(&(100, 5_000)));
        assert_eq!(late.total_send_bytes_per_sec, 0.0);
        // The new owner's own bytes (event time after creation) are reported.
        let own = monitor.account(
            &reused,
            t0 + second * 3,
            bytes(&[(100, 300, 0, 5_001)]),
            Ok(0),
            None,
        );
        assert_eq!(
            own.by_id.get(&(100, 5_000)).unwrap().send_bytes_per_sec,
            300.0
        );
    }

    #[test]
    fn lost_events_make_two_intervals_unmeasured() {
        let mut monitor = fake_running();
        let t0 = Instant::now();
        let at = |n: u64| t0 + Duration::from_secs(n);
        let processes = [proc(100, 1)];
        let traffic = || bytes(&[(100, 10, 10, 5)]);
        // Losses before the first sample are outside any reported interval.
        assert!(
            !monitor
                .account(&processes, at(0), traffic(), Ok(3), None)
                .measured
        );
        assert!(
            monitor
                .account(&processes, at(1), traffic(), Ok(3), None)
                .measured
        );
        for n in [2, 3] {
            let sample = monitor.account(&processes, at(n), traffic(), Ok(9), None);
            assert!(!sample.measured, "interval {n}");
            assert_eq!(sample.reason.as_deref(), Some("Network events dropped"));
            assert!(sample.by_id.is_empty());
            assert_eq!(sample.interval_seconds, 1.0);
        }
        assert!(
            monitor
                .account(&processes, at(4), traffic(), Ok(9), None)
                .measured
        );
    }

    #[test]
    fn failed_loss_query_is_unmeasured_and_compared_to_the_last_good_value() {
        let mut monitor = fake_running();
        let t0 = Instant::now();
        let at = |n: u64| t0 + Duration::from_secs(n);
        let _ = monitor.account(&[], at(0), HashMap::new(), Ok(0), None);
        let failed = monitor.account(&[], at(1), HashMap::new(), Err(5), None);
        assert!(!failed.measured);
        assert_eq!(
            failed.reason.as_deref(),
            Some("Cannot check network event loss (error 5)")
        );
        // Losses during the failed interval still show up against baseline 0.
        let after = monitor.account(&[], at(2), HashMap::new(), Ok(1), None);
        assert_eq!(after.reason.as_deref(), Some("Network events dropped"));
    }

    #[test]
    fn ended_trace_reports_stopped_instead_of_zeros() {
        let mut monitor = fake_running();
        let t0 = Instant::now();
        let processes = [proc(100, 1)];
        let _ = monitor.account(&processes, t0, HashMap::new(), Ok(0), None);
        let ended = monitor.account(
            &processes,
            t0 + Duration::from_secs(1),
            HashMap::new(),
            Ok(0),
            Some(ERROR_SUCCESS),
        );
        assert!(!ended.measured);
        assert!(ended.by_id.is_empty());
        assert_eq!(ended.reason.as_deref(), Some("Network trace stopped"));
        assert!(monitor.session.is_none());
        // Every later sample keeps reporting the reason.
        let later = monitor.sample(&processes);
        assert!(!later.measured);
        assert_eq!(later.reason.as_deref(), Some("Network trace stopped"));
    }

    #[test]
    fn missing_session_reports_stopped() {
        let mut monitor = fake_running();
        let sample = monitor.account(
            &[],
            Instant::now(),
            HashMap::new(),
            Err(ERROR_WMI_INSTANCE_NOT_FOUND),
            None,
        );
        assert_eq!(sample.reason.as_deref(), Some("Network trace stopped"));
        assert!(monitor.session.is_none());
        let mut failed = fake_running();
        let sample = failed.account(&[], Instant::now(), HashMap::new(), Ok(0), Some(1223));
        assert_eq!(
            sample.reason.as_deref(),
            Some("Network trace stopped (error 1223)")
        );
    }

    fn query_status(name: &[u16]) -> u32 {
        let mut props = TraceProperties::for_control();
        unsafe {
            ControlTraceW(
                CONTROLTRACE_HANDLE { Value: 0 },
                name.as_ptr(),
                props.as_mut_ptr(),
                EVENT_TRACE_CONTROL_QUERY,
            )
        }
    }

    /// Exercises the real session lifecycle without enabling the provider:
    /// unique names, per-sample query, stale cleanup, external stop detection
    /// and teardown. Run explicitly:
    /// `cargo test live_session_lifecycle -- --ignored --nocapture`.
    #[test]
    #[ignore = "starts real ETW sessions; needs elevation or Performance Log Users membership"]
    fn live_session_lifecycle() {
        let (pid, created) = own_identity().unwrap();
        let mine = || {
            feather_sessions()
                .into_iter()
                .filter(|(_, p, c)| (*p, *c) == (pid, created))
                .count()
        };
        if !is_elevated() {
            // Enabling the kernel provider needs administrator; the failure
            // path must leave no session behind.
            assert_eq!(start_session(true).err(), Some(requires_admin()));
            assert_eq!(mine(), 0, "failed enable leaked a session");
        }
        let mut first = start_session(false).expect("start a provider-less session");
        let second = start_session(false).expect("start a second session");
        assert_ne!(first.name, second.name);
        assert_eq!(mine(), 2);
        assert_eq!(first.lost(), Ok(0));
        let started = Instant::now();
        for _ in 0..100 {
            let _ = first.lost();
        }
        println!(
            "loss query: {:.4} ms each",
            started.elapsed().as_secs_f64() * 10.0
        );

        // A session named for a gone identity (same PID, other creation time)
        // is stale; the live ones survive the cleanup.
        let stale = wide(&session_name(pid, created ^ 0x5555, 999));
        let mut props = TraceProperties::for_start();
        let mut control = CONTROLTRACE_HANDLE { Value: 0 };
        let status = unsafe { StartTraceW(&mut control, stale.as_ptr(), props.as_mut_ptr()) };
        assert_eq!(status, ERROR_SUCCESS);
        assert_eq!(query_status(&stale), ERROR_SUCCESS);
        stop_stale_sessions();
        assert_eq!(query_status(&stale), ERROR_WMI_INSTANCE_NOT_FOUND);
        assert_eq!(first.lost(), Ok(0), "a live session was stopped");
        assert_eq!(mine(), 2);

        // A session stopped by someone else ends the consumer thread; the
        // monitor then reports the stop instead of measured zeros.
        let mut monitor = NetworkMonitor::from_start(Ok(second));
        let processes = [proc(pid, created)];
        let primed = monitor.sample(&processes);
        assert!(!primed.measured && primed.reason.is_none());
        let name = monitor.session.as_ref().unwrap().name.clone();
        assert_eq!(stop_by_name(&name), ERROR_SUCCESS);
        let deadline = Instant::now() + Duration::from_secs(5);
        while monitor.session.as_ref().unwrap().ended().is_none() {
            assert!(Instant::now() < deadline, "consumer thread did not end");
            std::thread::sleep(Duration::from_millis(20));
        }
        println!(
            "ProcessTrace status after external stop: {:?}",
            monitor.session.as_ref().unwrap().ended()
        );
        let stopped = monitor.sample(&processes);
        assert!(!stopped.measured);
        assert!(stopped
            .reason
            .as_deref()
            .unwrap()
            .starts_with("Network trace stopped"));
        assert!(monitor.session.is_none());

        // Dropping a session removes it.
        let name = first.name.clone();
        drop(first);
        assert_eq!(query_status(&name), ERROR_WMI_INSTANCE_NOT_FOUND);
        assert_eq!(mine(), 0);
    }
}
