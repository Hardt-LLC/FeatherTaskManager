//! Optional, bounded file I/O request telemetry for the resource monitor.
//!
//! No trace, thread, timer, filename lookup or allocation runs while disabled.
//! The Microsoft-Windows-Kernel-File provider reports requested bytes, including
//! cached I/O; these are not physical disk bytes or completed transfer counts.
//! Only requests whose issuing thread matches the event header are attributed.
//! File names come from the same trace; no files are opened or written here.

use crate::sampler::Process;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::null;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA,
    ERROR_SUCCESS, ERROR_WMI_INSTANCE_NOT_FOUND, FILETIME, HANDLE, WAIT_TIMEOUT,
};
use windows_sys::Win32::System::Diagnostics::Etw::*;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

const PROVIDER: GUID = GUID::from_u128(0xedd08927_9cc4_4e65_b970_c2560fb5c289);
// Filename, Read, Write only. FileObject fallback would require Close events;
// without those, a reused object could inherit a stale path. Never guess it.
const KEYWORDS: u64 = 0x10 | 0x100 | 0x200;
const PREFIX: &str = "Feather Task Manager File ";
const MAX_NAMES: usize = 4096;
const MAX_PATH_UNITS: usize = 4096;
const MAX_ROWS: usize = 2048;
const FLUSH_PERIOD: Duration = Duration::from_secs(1);
const INVALID_TRACE: u64 = u64::MAX;
static SEQUENCE: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Debug)]
pub struct Row {
    pub pid: u32,
    pub created: u64,
    pub path: String,
    pub read_bytes_per_sec: f64,
    pub write_bytes_per_sec: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub enabled: bool,
    pub measured: bool,
    /// Partial-attribution notices can accompany measured, attributable rows.
    pub reason: Option<String>,
    /// Actual sampling duration, including priming and unavailable intervals.
    pub interval_seconds: f64,
    pub rows: Vec<Row>,
    pub limited: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum NameKey {
    FileKey(u64),
}

struct Name {
    at: u64,
    serial: u64,
    // Tombstones prevent an out-of-order NameCreate resurrecting a deleted key.
    path: Option<Arc<str>>,
}

#[derive(Clone)]
struct Counts {
    path: Arc<str>,
    read: u64,
    write: u64,
    earliest: u64,
}

impl Counts {
    fn merge(&mut self, other: &Self) {
        self.read = self.read.saturating_add(other.read);
        self.write = self.write.saturating_add(other.write);
        self.earliest = self.earliest.min(other.earliest);
    }
}

#[derive(Default)]
struct Batch {
    // Integer keys avoid hashing or cloning long paths on every I/O request.
    rows: HashMap<(u32, u64), Counts>,
    limited: bool,
    partial: bool,
}

impl Batch {
    fn add(&mut self, key: (u32, u64), counts: Counts) {
        if let Some(existing) = self.rows.get_mut(&key) {
            existing.merge(&counts);
        } else if self.rows.len() < MAX_ROWS {
            self.rows.insert(key, counts);
        } else {
            self.limited = true;
        }
    }
}

struct Shared {
    totals: Mutex<Batch>,
    stop: AtomicBool,
    ended: AtomicBool,
    status: AtomicU32,
    // A loss of name events invalidates cached file-key generations.
    reset: AtomicU64,
    reset_ack: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        Self {
            totals: Mutex::new(Batch::default()),
            stop: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            status: AtomicU32::new(0),
            reset: AtomicU64::new(0),
            reset_ack: AtomicU64::new(0),
        }
    }
    fn drain(&self) -> Batch {
        self.totals
            .lock()
            .map(|mut b| std::mem::take(&mut *b))
            .unwrap_or_else(|_| Batch {
                limited: true,
                ..Batch::default()
            })
    }
}

struct Consumer {
    shared: Arc<Shared>,
    names: HashMap<NameKey, Name>,
    serial: u64,
    generation: u64,
    batch: Batch,
}

impl Consumer {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            names: HashMap::new(),
            serial: 0,
            generation: 0,
            batch: Batch::default(),
        }
    }
    fn name(&mut self, key: NameKey, at: u64, path: Option<String>) {
        if self.names.get(&key).is_some_and(|name| {
            name.at > at || name.at == at && name.path.is_none() && path.is_some()
        }) {
            return;
        }
        if !self.names.contains_key(&key) {
            if path.is_none() {
                // A delete for a name never seen (opened before tracing began)
                // has nothing to shadow; storing it would only fill the cache.
                return;
            }
            if self.names.len() >= MAX_NAMES {
                // Deleted names only guard against late, older events: drop
                // them all before any live name.
                self.names.retain(|_, name| name.path.is_some());
            }
            if self.names.len() >= MAX_NAMES {
                // Bounded cache: an evicted name makes its later requests
                // unattributed (partial), never guessed or the interval lost.
                if let Some(old) = self.names.keys().next().copied() {
                    self.names.remove(&old);
                }
                self.batch.partial = true;
            }
        }
        self.serial = self.serial.wrapping_add(1);
        self.names.insert(
            key,
            Name {
                at,
                serial: self.serial,
                path: path.map(Arc::from),
            },
        );
    }
    fn request(&mut self, header_pid: u32, header_tid: u32, at: u64, io: Request) {
        if header_pid == 0
            || header_pid == u32::MAX
            || header_tid == 0
            || io.thread != u64::from(header_tid)
            || at == 0
        {
            self.batch.partial = true;
            return;
        }
        let name = self
            .names
            .get(&NameKey::FileKey(io.key))
            .filter(|name| name.at <= at && name.path.is_some());
        let Some(name) = name else {
            self.batch.partial = true;
            return;
        };
        let Some(path) = &name.path else {
            return;
        };
        let key = (header_pid, name.serial);
        if let Some(counts) = self.batch.rows.get_mut(&key) {
            if io.write {
                counts.write = counts.write.saturating_add(io.size as u64);
            } else {
                counts.read = counts.read.saturating_add(io.size as u64);
            }
            counts.earliest = counts.earliest.min(at);
        } else {
            self.batch.add(
                key,
                Counts {
                    path: Arc::clone(path),
                    read: if io.write { 0 } else { io.size as u64 },
                    write: if io.write { io.size as u64 } else { 0 },
                    earliest: at,
                },
            );
        }
    }
    fn flush(&mut self) {
        if let Ok(mut totals) = self.shared.totals.lock() {
            let generation = self.shared.reset.load(Ordering::Acquire);
            if generation != self.generation {
                self.generation = generation;
                self.names.clear();
                self.batch = Batch::default();
                *totals = Batch::default();
                // Publish only after both consumer and shared batches are
                // invalidated. Sampling cannot resume on its own cadence.
                self.shared.reset_ack.store(generation, Ordering::Release);
                return;
            }
            for (key, counts) in self.batch.rows.drain() {
                totals.add(key, counts);
            }
            totals.limited |= std::mem::take(&mut self.batch.limited);
            totals.partial |= std::mem::take(&mut self.batch.partial);
        } else {
            self.batch = Batch::default();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Request {
    key: u64,
    thread: u64,
    size: u32,
    write: bool,
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn pointer(data: &[u8], offset: usize, width: usize) -> Option<u64> {
    match width {
        4 => u32_at(data, offset).map(u64::from),
        8 => Some(u64::from_le_bytes(
            data.get(offset..offset.checked_add(8)?)?.try_into().ok()?,
        )),
        _ => None,
    }
}

/// Exact versioned layouts are from the installed Windows provider manifest
/// (`Get-WinEvent -ListProvider Microsoft-Windows-Kernel-File`). IOSize means
/// requested bytes: https://learn.microsoft.com/windows/win32/etw/fileio-readwrite
fn request(id: u16, version: u8, width: usize, data: &[u8]) -> Option<Request> {
    if !matches!(id, 15 | 16) || !matches!(width, 4 | 8) {
        return None;
    }
    let (thread, key, size) = match version {
        0 if data.len() >= 8 + width * 4 + 8 => (
            pointer(data, 8 + width, width)?,
            pointer(data, 8 + width * 3, width)?,
            u32_at(data, 8 + width * 4)?,
        ),
        1 if data.len() >= 8 + width * 3 + 16 => (
            u32_at(data, 8 + width * 3)? as u64,
            pointer(data, 8 + width * 2, width)?,
            u32_at(data, 12 + width * 3)?,
        ),
        _ => return None,
    };
    Some(Request {
        key,
        thread,
        size,
        write: id == 16,
    })
}

fn path_at(data: &[u8], offset: usize) -> Option<String> {
    let bytes = data.get(offset..)?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let end = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .take(MAX_PATH_UNITS + 1)
        .position(|pair| *pair == [0, 0])?;
    if end == 0 || end > MAX_PATH_UNITS {
        return None;
    }
    Some(
        char::decode_utf16(
            bytes[..end * 2]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&pair| u16::from_le_bytes(pair)),
        )
        .map(|ch| ch.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect(),
    )
}

fn same_provider(provider: &GUID) -> bool {
    (
        provider.data1,
        provider.data2,
        provider.data3,
        provider.data4,
    ) == (
        PROVIDER.data1,
        PROVIDER.data2,
        PROVIDER.data3,
        PROVIDER.data4,
    )
}

unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let record = &*record;
    if record.UserContext.is_null()
        || !same_provider(&record.EventHeader.ProviderId)
        || record.UserData.is_null()
    {
        return;
    }
    let id = record.EventHeader.EventDescriptor.Id;
    if !matches!(id, 10 | 11 | 15 | 16) {
        return;
    }
    let consumer = &mut *record.UserContext.cast::<Consumer>();
    let flags = u32::from(record.EventHeader.Flags);
    let width = match flags & (EVENT_HEADER_FLAG_32_BIT_HEADER | EVENT_HEADER_FLAG_64_BIT_HEADER) {
        EVENT_HEADER_FLAG_32_BIT_HEADER => 4,
        EVENT_HEADER_FLAG_64_BIT_HEADER => 8,
        0 => size_of::<usize>(),
        _ => {
            consumer.batch.partial = true;
            return;
        }
    };
    let data =
        std::slice::from_raw_parts(record.UserData.cast::<u8>(), record.UserDataLength as usize);
    let version = record.EventHeader.EventDescriptor.Version;
    let at = u64::try_from(record.EventHeader.TimeStamp).unwrap_or(0);
    if at == 0 {
        consumer.batch.partial = true;
        return;
    }
    match id {
        10 | 11 if version == 0 => {
            if let Some(key) = pointer(data, 0, width).filter(|v| *v != 0) {
                let path = if id == 11 { None } else { path_at(data, width) };
                if id == 10 && path.is_none() {
                    consumer.batch.partial = true;
                }
                consumer.name(NameKey::FileKey(key), at, path);
            } else {
                consumer.names.clear();
                consumer.batch.partial = true;
            }
        }
        15 | 16 => {
            if let Some(io) = request(id, version, width, data) {
                consumer.request(
                    record.EventHeader.ProcessId,
                    record.EventHeader.ThreadId,
                    at,
                    io,
                );
            } else {
                consumer.batch.partial = true;
            }
        }
        _ => {
            consumer.names.clear();
            consumer.batch.partial = true;
        }
    }
}

unsafe extern "system" fn on_buffer(logfile: *mut EVENT_TRACE_LOGFILEW) -> u32 {
    if logfile.is_null() || (*logfile).Context.is_null() {
        return 1;
    }
    let consumer = &mut *(*logfile).Context.cast::<Consumer>();
    consumer.flush();
    u32::from(!consumer.shared.stop.load(Ordering::Acquire))
}

struct ConsumerPtr(*mut Consumer);
// Only the ProcessTrace thread owns or accesses the pointee after the move.
unsafe impl Send for ConsumerPtr {}
impl ConsumerPtr {
    fn take(self) -> *mut Consumer {
        self.0
    }
}

struct Properties {
    words: Vec<u64>,
}
impl Properties {
    const NAME: usize = size_of::<EVENT_TRACE_PROPERTIES>();
    const LOG: usize = Self::NAME + 2048;
    fn new(start: bool) -> Self {
        let mut value = Self {
            words: vec![0; (Self::LOG + 2048).div_ceil(8)],
        };
        value.reset();
        if start {
            let p = unsafe { &mut *value.ptr() };
            p.Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            p.Wnode.ClientContext = 2; // FILETIME system timestamps.
            p.BufferSize = 64;
            p.MinimumBuffers = 4;
            p.MaximumBuffers = 32;
            p.FlushTimer = FLUSH_PERIOD.as_secs() as u32;
            p.LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
            p.LogFileNameOffset = 0; // No log file.
        }
        value
    }
    fn ptr(&mut self) -> *mut EVENT_TRACE_PROPERTIES {
        self.words.as_mut_ptr().cast()
    }
    fn get(&self) -> &EVENT_TRACE_PROPERTIES {
        unsafe { &*self.words.as_ptr().cast() }
    }
    fn reset(&mut self) {
        self.words.fill(0);
        let bytes = self.words.len() * 8;
        let p = unsafe { &mut *self.ptr() };
        p.Wnode.BufferSize = bytes as u32;
        p.LoggerNameOffset = Self::NAME as u32;
        p.LogFileNameOffset = Self::LOG as u32;
    }
    fn name(&self) -> Option<String> {
        let offset = self.get().LoggerNameOffset as usize;
        if offset < Self::NAME || offset >= self.words.len() * 8 {
            return None;
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(self.words.as_ptr().cast::<u8>(), self.words.len() * 8)
        };
        path_at(bytes, offset)
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}
fn control(name: &[u16], properties: &mut Properties, code: u32) -> u32 {
    unsafe {
        ControlTraceW(
            CONTROLTRACE_HANDLE { Value: 0 },
            name.as_ptr(),
            properties.ptr(),
            code,
        )
    }
}
fn stop(name: &[u16]) {
    control(name, &mut Properties::new(false), EVENT_TRACE_CONTROL_STOP);
}
fn created(process: HANDLE) -> Option<u64> {
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    (unsafe { GetProcessTimes(process, &mut c, &mut e, &mut k, &mut u) } != 0)
        .then_some(u64::from(c.dwLowDateTime) | u64::from(c.dwHighDateTime) << 32)
}
fn alive(pid: u32, started: u64) -> bool {
    let process = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if process.is_null() {
        return unsafe { GetLastError() } != ERROR_INVALID_PARAMETER;
    }
    let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
    let identity = created(process);
    unsafe {
        CloseHandle(process);
    }
    running && identity.is_none_or(|value| value == started)
}
/// Stops file sessions left by Feather processes that no longer run (a crash
/// or kill skips Session::drop). Live instances' sessions are kept.
pub fn stop_stale_sessions() {
    if crate::netetw::is_elevated() {
        stale_sessions();
    }
}

fn stale_sessions() {
    let mut buffers: Vec<Properties> = (0..64).map(|_| Properties::new(false)).collect();
    let mut pointers: Vec<_> = buffers.iter_mut().map(Properties::ptr).collect();
    let mut count = 0;
    let status =
        unsafe { QueryAllTracesW(pointers.as_mut_ptr(), pointers.len() as u32, &mut count) };
    if !matches!(status, ERROR_SUCCESS | ERROR_MORE_DATA) {
        return;
    }
    for p in buffers.iter().take((count as usize).min(64)) {
        let Some(name) = p.name() else {
            continue;
        };
        let Some(tail) = name.strip_prefix(PREFIX) else {
            continue;
        };
        let words: Vec<_> = tail.split_whitespace().collect();
        if words.len() != 3 || words[1].len() != 16 || words[2].parse::<u32>().is_err() {
            continue;
        }
        let (Ok(pid), Ok(started)) = (words[0].parse::<u32>(), u64::from_str_radix(words[1], 16))
        else {
            continue;
        };
        if pid != 0 && started != 0 && !alive(pid, started) {
            stop(&wide(&name));
        }
    }
}

struct Session {
    name: Vec<u16>,
    trace: PROCESSTRACE_HANDLE,
    thread: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    query: Properties,
    _logname: Box<[u16]>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        stop(&self.name);
        // CloseTrace also cancels real-time ProcessTrace when stop failed, so
        // disabling never waits for an otherwise unbounded consumer loop.
        unsafe {
            CloseTrace(self.trace);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
impl Session {
    fn start() -> Result<Self, String> {
        if !crate::netetw::is_elevated() {
            return Err("Requires administrator".into());
        }
        let own_created =
            created(unsafe { GetCurrentProcess() }).ok_or("Cannot read process identity")?;
        stale_sessions();
        let name = wide(&format!(
            "{PREFIX}{} {own_created:016X} {}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut properties = Properties::new(true);
        let mut logger = CONTROLTRACE_HANDLE { Value: 0 };
        let status = unsafe { StartTraceW(&mut logger, name.as_ptr(), properties.ptr()) };
        if status != ERROR_SUCCESS {
            return Err(error("Cannot start file I/O trace", status));
        }
        let params = ENABLE_TRACE_PARAMETERS {
            Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
            ..Default::default()
        };
        let status = unsafe {
            EnableTraceEx2(
                logger,
                &PROVIDER,
                1,
                TRACE_LEVEL_VERBOSE as u8,
                KEYWORDS,
                0,
                0,
                &params,
            )
        };
        if status != ERROR_SUCCESS {
            stop(&name);
            return Err(error("Cannot enable file I/O trace", status));
        }
        let shared = Arc::new(Shared::new());
        let context = Box::into_raw(Box::new(Consumer::new(Arc::clone(&shared))));
        let mut logname = name.clone().into_boxed_slice();
        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { zeroed() };
        logfile.LoggerName = logname.as_mut_ptr();
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        logfile.Anonymous2.EventRecordCallback = Some(on_event);
        logfile.BufferCallback = Some(on_buffer);
        logfile.Context = context.cast();
        let trace = unsafe { OpenTraceW(&mut logfile) };
        if trace.Value == INVALID_TRACE {
            let code = unsafe { GetLastError() };
            unsafe {
                drop(Box::from_raw(context));
            }
            stop(&name);
            return Err(error("Cannot open file I/O trace", code));
        }
        let pointer = ConsumerPtr(context);
        let copy = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("feather-fileetw".into())
            .spawn(move || {
                let context = pointer.take();
                let code = unsafe { ProcessTrace(&trace, 1, null(), null()) };
                copy.status.store(code, Ordering::Release);
                copy.ended.store(true, Ordering::Release);
                unsafe {
                    drop(Box::from_raw(context));
                }
            });
        let thread = match thread {
            Ok(thread) => thread,
            Err(e) => {
                unsafe {
                    CloseTrace(trace);
                    drop(Box::from_raw(context));
                }
                stop(&name);
                return Err(format!("Cannot start file I/O consumer: {e}"));
            }
        };
        Ok(Self {
            name,
            trace,
            thread: Some(thread),
            shared,
            query: Properties::new(false),
            _logname: logname,
        })
    }
    fn losses(&mut self) -> Result<u64, u32> {
        self.query.reset();
        let status = control(&self.name, &mut self.query, EVENT_TRACE_CONTROL_QUERY);
        if status != 0 {
            return Err(status);
        }
        let p = self.query.get();
        Ok(
            u64::from(p.EventsLost)
                + u64::from(p.RealTimeBuffersLost)
                + u64::from(p.LogBuffersLost),
        )
    }
}
fn error(context: &str, code: u32) -> String {
    if code == ERROR_ACCESS_DENIED {
        "Requires administrator".into()
    } else {
        format!("{context} (Windows {code})")
    }
}

#[derive(Default)]
struct Recovery {
    epoch: u64,
    acknowledged: bool,
    clean_elapsed: Duration,
}

#[derive(Default)]
pub struct FileMonitor {
    enabled: bool,
    session: Option<Session>,
    reason: Option<String>,
    previous: HashMap<u32, u64>,
    primed: bool,
    lost: Option<u64>,
    recovery: Option<Recovery>,
}
impl FileMonitor {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.session = None;
        self.reason = None;
        self.previous.clear();
        self.primed = false;
        self.lost = None;
        self.recovery = None;
        if enabled {
            match Session::start() {
                Ok(session) => self.session = Some(session),
                Err(reason) => self.reason = Some(reason),
            }
        }
    }
    /// `elapsed` is the actual duration since the preceding sample, not the
    /// configured refresh interval. The first interval after enable is primed.
    pub fn sample(&mut self, processes: &[Process], elapsed: Duration) -> Sample {
        if !self.enabled {
            return Sample::default();
        }
        let Some(session) = self.session.as_mut() else {
            return Sample {
                enabled: true,
                reason: self.reason.clone(),
                interval_seconds: elapsed.as_secs_f64(),
                ..Sample::default()
            };
        };
        let loss = session.losses();
        let raw = session.shared.drain();
        if session.shared.ended.load(Ordering::Acquire) || loss == Err(ERROR_WMI_INSTANCE_NOT_FOUND)
        {
            let status = session.shared.status.load(Ordering::Acquire);
            let reason = if status == ERROR_SUCCESS {
                // Ended normally (or stopped by another tool): not an error code.
                "File I/O trace stopped".to_owned()
            } else {
                error("File I/O trace stopped", status)
            };
            self.session = None;
            self.reason = Some(reason.clone());
            return Sample {
                enabled: true,
                reason: Some(reason),
                interval_seconds: elapsed.as_secs_f64(),
                ..Sample::default()
            };
        }
        let shared = Arc::clone(&session.shared);
        self.account(processes, elapsed, raw, loss, &shared)
    }
    fn invalidate(&mut self, shared: &Shared) {
        self.recovery = Some(Recovery {
            epoch: shared.reset.fetch_add(1, Ordering::AcqRel).wrapping_add(1),
            ..Recovery::default()
        });
    }
    fn account(
        &mut self,
        processes: &[Process],
        elapsed: Duration,
        raw: Batch,
        loss: Result<u64, u32>,
        shared: &Shared,
    ) -> Sample {
        let current: HashMap<_, _> = processes
            .iter()
            .filter(|p| p.created > 0)
            .map(|p| (p.pid, p.created))
            .collect();
        let previous = std::mem::replace(&mut self.previous, current);
        let mut sample = Sample {
            enabled: true,
            limited: raw.limited,
            interval_seconds: elapsed.as_secs_f64(),
            ..Sample::default()
        };
        let priming = !std::mem::replace(&mut self.primed, true);
        match loss {
            Ok(lost) => {
                if self.lost.is_some_and(|old| lost > old) || self.lost.is_none() && lost > 0 {
                    self.invalidate(shared);
                }
                self.lost = Some(lost);
            }
            Err(code) => {
                sample.reason = Some(error("Cannot check file I/O event loss", code));
                self.invalidate(shared);
                return sample;
            }
        }
        if let Some(recovery) = &mut self.recovery {
            if shared.reset_ack.load(Ordering::Acquire) == recovery.epoch {
                if recovery.acknowledged {
                    recovery.clean_elapsed = recovery.clean_elapsed.saturating_add(elapsed);
                } else {
                    // The just-drained batch may predate the acknowledgement.
                    recovery.acknowledged = true;
                }
                if recovery.clean_elapsed >= FLUSH_PERIOD {
                    // Discard this clean drain too. The next interval starts
                    // after an acknowledged reset and a full ETW flush window.
                    self.recovery = None;
                }
            }
            sample.reason = Some("File I/O events dropped".into());
            return sample;
        }
        if priming || elapsed.is_zero() {
            sample.reason = Some("Preparing file I/O requests".into());
            return sample;
        }
        if raw.limited {
            sample.reason = Some("File I/O tracking limit reached".into());
            return sample;
        }
        let seconds = elapsed.as_secs_f64();
        let mut rows: HashMap<(u32, u64, Arc<str>), Counts> = HashMap::new();
        for ((pid, _), counts) in raw.rows {
            let Some(&created) = self.previous.get(&pid) else {
                continue;
            };
            if previous.get(&pid) != Some(&created) || counts.earliest < created {
                continue;
            }
            rows.entry((pid, created, Arc::clone(&counts.path)))
                .and_modify(|entry| entry.merge(&counts))
                .or_insert(counts);
        }
        sample.rows = rows
            .into_iter()
            .map(|((pid, created, path), counts)| Row {
                pid,
                created,
                path: path.to_string(),
                read_bytes_per_sec: counts.read as f64 / seconds,
                write_bytes_per_sec: counts.write as f64 / seconds,
            })
            .collect();
        sample.measured = true;
        if raw.partial {
            sample.reason = Some("Some file I/O requests could not be attributed".into());
        }
        sample
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn io_data(version: u8, width: usize) -> Vec<u8> {
        let mut data = vec![
            0;
            if version == 0 {
                8 + width * 4 + 8
            } else {
                8 + width * 3 + 16
            }
        ];
        let ptr = |data: &mut [u8], offset: usize, value: u64| {
            data[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
        };
        if version == 0 {
            ptr(&mut data, 8 + width, 77);
            ptr(&mut data, 8 + width * 2, 22);
            ptr(&mut data, 8 + width * 3, 33);
            data[8 + width * 4..12 + width * 4].copy_from_slice(&500u32.to_le_bytes());
        } else {
            ptr(&mut data, 8 + width, 22);
            ptr(&mut data, 8 + width * 2, 33);
            data[8 + width * 3..12 + width * 3].copy_from_slice(&77u32.to_le_bytes());
            data[12 + width * 3..16 + width * 3].copy_from_slice(&500u32.to_le_bytes());
        }
        data
    }
    #[test]
    fn versioned_requests_and_paths_are_bounded_and_thread_attribution_is_conservative() {
        for width in [4, 8] {
            for version in [0, 1] {
                let data = io_data(version, width);
                let io = request(15, version, width, &data).unwrap();
                assert_eq!(
                    io,
                    Request {
                        key: 33,
                        thread: 77,
                        size: 500,
                        write: false
                    }
                );
                assert!(request(15, version, width, &data[..data.len() - 1]).is_none());
                assert!(request(15, 2, width, &data).is_none());
            }
        }
        let encoded: Vec<u8> = "\\Device\\HarddiskVolume3\\sample.txt\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert!(path_at(&encoded, 0).unwrap().ends_with("sample.txt"));
        assert!(path_at(&encoded[..encoded.len() - 2], 0).is_none());
        let mut consumer = Consumer::new(Arc::new(Shared::new()));
        consumer.name(NameKey::FileKey(33), 50, Some("sample".into()));
        let io = request(15, 1, 8, &io_data(1, 8)).unwrap();
        consumer.request(7, 78, 100, io);
        assert!(consumer.batch.rows.is_empty() && consumer.batch.partial);
        consumer.request(7, 77, 100, io);
        assert_eq!(consumer.batch.rows.len(), 1);
        consumer.name(NameKey::FileKey(33), 120, None);
        consumer.name(NameKey::FileKey(33), 110, Some("stale".into()));
        assert!(consumer.names[&NameKey::FileKey(33)].path.is_none());
        // Deletes of never-seen names (files opened before tracing) are not kept.
        let mut consumer = Consumer::new(Arc::new(Shared::new()));
        for key in 0..MAX_NAMES as u64 * 2 {
            consumer.name(NameKey::FileKey(1_000_000 + key), 150, None);
        }
        assert!(consumer.names.is_empty() && !consumer.batch.partial);
        // A full cache drops deleted names before evicting any live one.
        for key in 0..MAX_NAMES as u64 {
            consumer.name(NameKey::FileKey(key), 200, Some("file".into()));
            if key % 2 == 0 {
                consumer.name(NameKey::FileKey(key), 210, None);
            }
        }
        consumer.name(NameKey::FileKey(u64::MAX), 220, Some("new".into()));
        assert_eq!(consumer.names.len(), MAX_NAMES / 2 + 1);
        assert!(!consumer.batch.partial && !consumer.batch.limited);
        // Evicting a live name only makes requests unattributed: the interval
        // stays measured instead of "tracking limit reached".
        for key in 0..MAX_NAMES as u64 + 2 {
            consumer.name(NameKey::FileKey(5_000_000 + key), 230, Some("more".into()));
        }
        assert_eq!(consumer.names.len(), MAX_NAMES);
        assert!(consumer.batch.partial && !consumer.batch.limited);
        let path: Arc<str> = Arc::from("bounded");
        let mut batch = Batch::default();
        for serial in 0..=MAX_ROWS {
            batch.add(
                (7, serial as u64),
                Counts {
                    path: Arc::clone(&path),
                    read: 1,
                    write: 0,
                    earliest: 1,
                },
            );
        }
        assert_eq!(batch.rows.len(), MAX_ROWS);
        assert!(batch.limited);
    }
    #[test]
    fn disabled_monitor_is_inert_and_samples_prime_reject_reuse_and_losses() {
        let mut monitor = FileMonitor::new();
        assert!(!monitor.sample(&[], Duration::from_secs(1)).enabled);
        monitor.set_enabled(false);
        assert!(monitor.session.is_none());
        let shared = Arc::new(Shared::new());
        let process = |created| Process {
            pid: 7,
            created,
            ..Process::default()
        };
        let raw = |earliest| Batch {
            rows: HashMap::from([(
                (7, 1),
                Counts {
                    path: Arc::from("sample"),
                    read: 1000,
                    write: 2000,
                    earliest,
                },
            )]),
            ..Batch::default()
        };
        assert!(
            !monitor
                .account(
                    &[process(10)],
                    Duration::from_secs(2),
                    raw(11),
                    Ok(0),
                    &shared
                )
                .measured
        );
        let sample = monitor.account(
            &[process(10)],
            Duration::from_secs(2),
            raw(11),
            Ok(0),
            &shared,
        );
        assert!(sample.measured);
        assert_eq!(sample.interval_seconds, 2.0);
        assert_eq!(
            (
                sample.rows[0].read_bytes_per_sec,
                sample.rows[0].write_bytes_per_sec
            ),
            (500.0, 1000.0)
        );
        assert!(monitor
            .account(
                &[process(20)],
                Duration::from_secs(2),
                raw(11),
                Ok(0),
                &shared
            )
            .rows
            .is_empty());
        assert!(monitor
            .account(
                &[process(20)],
                Duration::from_secs(2),
                raw(11),
                Ok(0),
                &shared
            )
            .rows
            .is_empty());
        // At 250 ms a sample-count hold can expire before ETW even flushes.
        // No amount of sampling can replace the consumer's reset acknowledgement.
        let tick = Duration::from_millis(250);
        let mut consumer = Consumer::new(Arc::clone(&shared));
        consumer.name(NameKey::FileKey(33), 20, Some("stale".into()));
        consumer.batch = raw(21);
        *shared.totals.lock().unwrap() = raw(21);
        for _ in 0..8 {
            let sample = monitor.account(&[process(20)], tick, raw(21), Ok(1), &shared);
            assert!(!sample.measured && sample.rows.is_empty());
            assert_eq!(sample.interval_seconds, 0.25);
        }
        assert_eq!(shared.reset.load(Ordering::Acquire), 1);
        assert_eq!(shared.reset_ack.load(Ordering::Acquire), 0);
        consumer.flush();
        assert_eq!(shared.reset_ack.load(Ordering::Acquire), 1);
        assert!(consumer.names.is_empty() && consumer.batch.rows.is_empty());
        assert!(shared.drain().rows.is_empty());
        // Observe the acknowledgement, wait a full flush period, and discard
        // that drain before accepting the following complete sample interval.
        for _ in 0..5 {
            let sample = monitor.account(&[process(20)], tick, raw(21), Ok(1), &shared);
            assert!(!sample.measured && sample.rows.is_empty());
        }
        assert!(
            monitor
                .account(&[process(20)], tick, raw(21), Ok(1), &shared)
                .measured
        );
        assert!(
            !monitor
                .account(&[process(20)], tick, raw(21), Err(5), &shared)
                .measured
        );
        assert_eq!(shared.reset.load(Ordering::Acquire), 2);
        assert!(
            !monitor
                .account(&[process(20)], tick, raw(21), Ok(1), &shared)
                .measured
        );
        monitor.enabled = true;
        let unavailable = monitor.sample(&[], tick);
        assert!(unavailable.enabled && !unavailable.measured);
        assert_eq!(unavailable.interval_seconds, 0.25);
    }
}
