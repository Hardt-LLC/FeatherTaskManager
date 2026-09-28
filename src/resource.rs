//! Optional resource-monitor details. No work or worker exists until requested.
//!
//! Endpoint tables are passive IP Helper snapshots, with no name resolution,
//! probes, ESTATS configuration or extra tracing. Memory lists reuse the native
//! read-only collector. Only local fixed-volume capacity and the selected
//! process's module list are queried. Every allocation and retry is bounded.
//! Slow device queries run off the UI/monitor thread, with one outstanding job;
//! disabling requests retires that worker rather than spawning replacements.

use crate::memclean::{self, MemoryState};
use crate::sampler::Process;
use std::collections::HashMap;
use std::mem::{offset_of, size_of};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BAD_LENGTH, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES,
    FILETIME, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::NetworkManagement::IpHelper::*;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE,
};

const REFRESH: Duration = Duration::from_secs(2);
const MODULE_REFRESH: Duration = Duration::from_secs(5);
const MAX_TABLE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENDPOINTS: usize = 16_384;
const MAX_MODULES: usize = 4096;
const MAX_IDENTITIES: usize = 4096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Request {
    pub memory: bool,
    pub endpoints: bool,
    pub volumes: bool,
    pub modules: Option<(u32, u64)>,
    pub handles: Option<(u32, u64)>,
}

impl Request {
    fn enabled(self) -> bool {
        self.memory
            || self.endpoints
            || self.volumes
            || self.modules.is_some()
            || self.handles.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub pid: u32,
    /// Matches both the requesting process snapshot and a fresh process handle.
    /// Unknown/protected owners remain unassociated rather than borrowing a PID.
    pub created: Option<u64>,
    pub protocol: &'static str,
    pub local_address: String,
    pub local_port: u16,
    pub remote_address: Option<String>,
    pub remote_port: Option<u16>,
    pub state: &'static str,
    pub listening: bool,
}

#[derive(Clone, Debug)]
pub struct Module {
    pub name: String,
    pub path: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct Volume {
    pub name: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub sampled_at: Instant,
    pub endpoints: Vec<Endpoint>,
    pub modules: Vec<Module>,
    pub module_identity: Option<(u32, u64)>,
    pub volumes: Vec<Volume>,
    pub memory: Option<MemoryState>,
    pub errors: Vec<String>,
}

impl Snapshot {
    fn empty(request: Request) -> Self {
        Self {
            sampled_at: Instant::now(),
            endpoints: Vec::new(),
            modules: Vec::new(),
            module_identity: request.modules,
            volumes: Vec::new(),
            memory: None,
            errors: Vec::new(),
        }
    }
}

struct Job {
    request: Request,
    identities: HashMap<u32, u64>,
}

struct Reply {
    request: Request,
    snapshot: Arc<Snapshot>,
}

struct Worker {
    tx: Option<SyncSender<Job>>,
    rx: Receiver<Reply>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl Worker {
    fn start() -> Result<Self, String> {
        let (tx, jobs) = mpsc::sync_channel::<Job>(1);
        let (replies, rx) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("feather-resource".into())
            .spawn(move || {
                let mut collector = Collector::default();
                while let Ok(job) = jobs.recv() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let snapshot = Arc::new(collector.collect(&job));
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let _ = replies.try_send(Reply {
                        request: job.request,
                        snapshot,
                    });
                }
            })
            .map_err(|error| format!("Resource details worker unavailable: {error}"))?;
        Ok(Self {
            tx: Some(tx),
            rx,
            stop,
            thread,
        })
    }

    fn retire(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.tx.take();
    }
}

/// Nonblocking bridge used by the existing monitor loop. A disabled request
/// releases cached details and stops its worker after any current OS call.
#[derive(Default)]
pub struct Client {
    worker: Option<Worker>,
    request: Request,
    submitted: Option<Instant>,
    pending: bool,
    cached: Option<Arc<Snapshot>>,
}

impl Client {
    pub fn sample(&mut self, request: &Request, processes: &[Process]) -> Option<Arc<Snapshot>> {
        let mut request = *request;
        let live = |identity| processes.iter().any(|p| (p.pid, p.created) == identity);
        request.modules = request.modules.filter(|&id| live(id));
        request.handles = request.handles.filter(|&id| live(id));
        if request != self.request {
            self.request = request;
            self.cached = None;
            self.submitted = None;
        }
        if !request.enabled() {
            self.cached = None;
            if let Some(worker) = &mut self.worker {
                worker.retire();
            }
        }
        if self.worker.as_ref().is_some_and(|w| w.thread.is_finished()) {
            if let Some(worker) = self.worker.take() {
                let _ = worker.thread.join();
            }
            self.pending = false;
        }
        if !request.enabled() {
            return None;
        }
        if self.worker.is_none() {
            if self.submitted.is_some_and(|at| at.elapsed() < REFRESH) {
                return self.cached.clone();
            }
            match Worker::start() {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    let mut snapshot = Snapshot::empty(request);
                    snapshot.errors.push(error);
                    self.cached = Some(Arc::new(snapshot));
                    self.submitted = Some(Instant::now());
                    return self.cached.clone();
                }
            }
            self.submitted = None;
        }
        let worker = self.worker.as_ref()?;
        if worker.stop.load(Ordering::Acquire) {
            // Never accumulate workers if a local device is slow to respond.
            return None;
        }
        while let Ok(reply) = worker.rx.try_recv() {
            self.pending = false;
            if reply.request == request {
                self.cached = Some(reply.snapshot);
            }
        }
        if !self.pending && self.submitted.is_none_or(|at| at.elapsed() >= REFRESH) {
            let identities = if request.endpoints {
                processes
                    .iter()
                    .take(MAX_IDENTITIES)
                    .map(|p| (p.pid, p.created))
                    .collect()
            } else {
                HashMap::new()
            };
            if worker.tx.as_ref().is_some_and(|tx| {
                tx.try_send(Job {
                    request,
                    identities,
                })
                .is_ok()
            }) {
                self.submitted = Some(Instant::now());
                self.pending = true;
            }
        }
        self.cached.clone()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(worker) = &mut self.worker {
            worker.retire();
        }
    }
}

type ModuleCache = ((u32, u64), Instant, Result<Vec<Module>, String>);

#[derive(Default)]
struct Collector {
    modules: Option<ModuleCache>,
}

impl Collector {
    fn collect(&mut self, job: &Job) -> Snapshot {
        let request = job.request;
        let mut result = Snapshot::empty(request);
        if request.memory {
            match memclean::memory_state() {
                Ok(memory) => result.memory = Some(memory),
                Err(error) => result.errors.push(error),
            }
        }
        if request.endpoints {
            endpoints(&mut result.endpoints, &mut result.errors);
            let mut checked = HashMap::new();
            for endpoint in &mut result.endpoints {
                endpoint.created = *checked.entry(endpoint.pid).or_insert_with(|| {
                    let created = *job.identities.get(&endpoint.pid)?;
                    verified_process((endpoint.pid, created))
                        .ok()
                        .map(|_| created)
                });
            }
        }
        if request.volumes {
            volumes(&mut result.volumes, &mut result.errors);
        }
        if let Some(identity) = request.modules {
            let refresh = self
                .modules
                .as_ref()
                .is_none_or(|(id, at, _)| *id != identity || at.elapsed() >= MODULE_REFRESH);
            if refresh {
                self.modules = Some((identity, Instant::now(), modules(identity)));
            }
            if let Some((_, _, modules)) = &self.modules {
                match modules {
                    Ok(modules) => result.modules.clone_from(modules),
                    Err(error) => result.errors.push(error.clone()),
                }
            }
        } else {
            self.modules = None;
        }
        if request.handles.is_some() {
            result.errors.push("Handle names are unavailable in lightweight monitoring; the process table shows the measured handle count.".into());
        }
        result
    }
}

struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn verified_process((pid, created): (u32, u64)) -> Result<Owned, String> {
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle.is_null() {
        return Err("Process details are unavailable or access was denied.".into());
    }
    let handle = Owned(handle);
    let mut start = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle.0, &mut start, &mut exit, &mut kernel, &mut user) } == 0
        || (u64::from(start.dwHighDateTime) << 32 | u64::from(start.dwLowDateTime)) != created
        || unsafe { WaitForSingleObject(handle.0, 0) } != WAIT_TIMEOUT
    {
        return Err("The selected process has exited or changed.".into());
    }
    Ok(handle)
}

fn modules(identity: (u32, u64)) -> Result<Vec<Module>, String> {
    let process = verified_process(identity)?;
    let mut snapshot = INVALID_HANDLE_VALUE;
    for _ in 0..3 {
        snapshot = unsafe {
            CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, identity.0)
        };
        if snapshot != INVALID_HANDLE_VALUE || unsafe { GetLastError() } != ERROR_BAD_LENGTH {
            break;
        }
    }
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(format!(
            "Module list unavailable: {}",
            std::io::Error::last_os_error()
        ));
    }
    let snapshot = Owned(snapshot);
    let mut row = MODULEENTRY32W {
        dwSize: size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut present = unsafe { Module32FirstW(snapshot.0, &mut row) } != 0;
    let mut result = Vec::new();
    while present {
        if result.len() >= MAX_MODULES {
            return Err("Module list exceeds the 4096-entry limit.".into());
        }
        result.push(Module {
            name: wide_text(&row.szModule),
            path: wide_text(&row.szExePath),
            size_bytes: u64::from(row.modBaseSize),
        });
        row.dwSize = size_of::<MODULEENTRY32W>() as u32;
        present = unsafe { Module32NextW(snapshot.0, &mut row) } != 0;
    }
    let error = unsafe { GetLastError() };
    if error != ERROR_NO_MORE_FILES {
        return Err(format!(
            "Module list unavailable: {}",
            std::io::Error::from_raw_os_error(error as i32)
        ));
    }
    if unsafe { WaitForSingleObject(process.0, 0) } != WAIT_TIMEOUT {
        return Err("The selected process has exited.".into());
    }
    result.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

fn wide_text(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|&v| v == 0).unwrap_or(value.len())])
}

fn volumes(result: &mut Vec<Volume>, errors: &mut Vec<String>) {
    let drives = unsafe { GetLogicalDrives() };
    if drives == 0 {
        errors.push(format!(
            "Volume list unavailable: {}",
            std::io::Error::last_os_error()
        ));
        return;
    }
    for index in 0..26 {
        if drives & (1 << index) == 0 {
            continue;
        }
        let name = format!("{}:\\", char::from(b'A' + index));
        let path: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // DRIVE_FIXED=3. Never query network mounts or wake removable media.
        if unsafe { GetDriveTypeW(path.as_ptr()) } != 3 {
            continue;
        }
        let mut total = 0;
        let mut free = 0;
        if unsafe { GetDiskFreeSpaceExW(path.as_ptr(), null_mut(), &mut total, &mut free) } == 0 {
            errors.push(format!(
                "{name} capacity unavailable: {}",
                std::io::Error::last_os_error()
            ));
        } else {
            result.push(Volume {
                name,
                total_bytes: total,
                free_bytes: free,
            });
        }
    }
}

/// The returned table uses SDK row types and their actual field offset; do not
/// assume tables have no alignment padding between count and first row.
struct NativeTable {
    storage: Vec<u64>,
    bytes: usize,
}

fn endpoint_table(tcp: bool, family: u32) -> Result<NativeTable, String> {
    let mut storage = vec![0u64; 512];
    for _ in 0..4 {
        let mut bytes = (storage.len() * size_of::<u64>()) as u32;
        let status = unsafe {
            if tcp {
                GetExtendedTcpTable(
                    storage.as_mut_ptr().cast(),
                    &mut bytes,
                    0,
                    family,
                    TCP_TABLE_OWNER_PID_ALL,
                    0,
                )
            } else {
                GetExtendedUdpTable(
                    storage.as_mut_ptr().cast(),
                    &mut bytes,
                    0,
                    family,
                    UDP_TABLE_OWNER_PID,
                    0,
                )
            }
        };
        if status == 0 {
            if bytes as usize > storage.len() * size_of::<u64>() {
                return Err("Endpoint table reported an invalid length.".into());
            }
            return Ok(NativeTable {
                storage,
                bytes: bytes as usize,
            });
        }
        if status != ERROR_INSUFFICIENT_BUFFER {
            return Err(format!(
                "Endpoint table unavailable: {}",
                std::io::Error::from_raw_os_error(status as i32)
            ));
        }
        let required = bytes as usize;
        if !(4..=MAX_TABLE_BYTES).contains(&required) {
            return Err("Endpoint table exceeds the 8 MiB limit.".into());
        }
        storage.resize(required.div_ceil(size_of::<u64>()), 0);
    }
    Err("Endpoint table changed too quickly to collect.".into())
}

fn table_rows<T: Copy>(table: &NativeTable, offset: usize) -> Result<Vec<T>, String> {
    if table.bytes > table.storage.len() * size_of::<u64>() {
        return Err("Endpoint table is truncated.".into());
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(table.storage.as_ptr().cast::<u8>(), table.bytes) };
    let count = bytes
        .get(..4)
        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()) as usize)
        .ok_or("Endpoint table is truncated.")?;
    if count > MAX_ENDPOINTS {
        return Err("Endpoint table exceeds the 16384-entry limit.".into());
    }
    let end = count
        .checked_mul(size_of::<T>())
        .and_then(|n| offset.checked_add(n))
        .ok_or("Endpoint table is malformed.")?;
    if end > bytes.len() {
        return Err("Endpoint table is truncated.".into());
    }
    Ok((0..count)
        .map(|i| unsafe {
            std::ptr::read_unaligned(bytes.as_ptr().add(offset + i * size_of::<T>()).cast::<T>())
        })
        .collect())
}

fn endpoints(result: &mut Vec<Endpoint>, errors: &mut Vec<String>) {
    for (tcp, family) in [
        (true, AF_INET),
        (true, AF_INET6),
        (false, AF_INET),
        (false, AF_INET6),
    ] {
        let table = match endpoint_table(tcp, u32::from(family)) {
            Ok(table) => table,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let rows = match (tcp, family) {
            (true, AF_INET) => table_rows::<MIB_TCPROW_OWNER_PID>(
                &table,
                offset_of!(MIB_TCPTABLE_OWNER_PID, table),
            )
            .map(|rows| {
                rows.into_iter()
                    .map(|r| Endpoint {
                        pid: r.dwOwningPid,
                        created: None,
                        protocol: "TCP",
                        local_address: ipv4(r.dwLocalAddr),
                        local_port: port(r.dwLocalPort),
                        remote_address: (r.dwState != 2).then(|| ipv4(r.dwRemoteAddr)),
                        remote_port: (r.dwState != 2).then(|| port(r.dwRemotePort)),
                        state: tcp_state(r.dwState),
                        listening: r.dwState == 2,
                    })
                    .collect::<Vec<_>>()
            }),
            (true, _) => table_rows::<MIB_TCP6ROW_OWNER_PID>(
                &table,
                offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
            )
            .map(|rows| {
                rows.into_iter()
                    .map(|r| Endpoint {
                        pid: r.dwOwningPid,
                        created: None,
                        protocol: "TCP",
                        local_address: ipv6(r.ucLocalAddr, r.dwLocalScopeId),
                        local_port: port(r.dwLocalPort),
                        remote_address: (r.dwState != 2)
                            .then(|| ipv6(r.ucRemoteAddr, r.dwRemoteScopeId)),
                        remote_port: (r.dwState != 2).then(|| port(r.dwRemotePort)),
                        state: tcp_state(r.dwState),
                        listening: r.dwState == 2,
                    })
                    .collect::<Vec<_>>()
            }),
            (false, AF_INET) => table_rows::<MIB_UDPROW_OWNER_PID>(
                &table,
                offset_of!(MIB_UDPTABLE_OWNER_PID, table),
            )
            .map(|rows| {
                rows.into_iter()
                    .map(|r| Endpoint {
                        pid: r.dwOwningPid,
                        created: None,
                        protocol: "UDP",
                        local_address: ipv4(r.dwLocalAddr),
                        local_port: port(r.dwLocalPort),
                        remote_address: None,
                        remote_port: None,
                        state: "Bound",
                        listening: true,
                    })
                    .collect::<Vec<_>>()
            }),
            (false, _) => table_rows::<MIB_UDP6ROW_OWNER_PID>(
                &table,
                offset_of!(MIB_UDP6TABLE_OWNER_PID, table),
            )
            .map(|rows| {
                rows.into_iter()
                    .map(|r| Endpoint {
                        pid: r.dwOwningPid,
                        created: None,
                        protocol: "UDP",
                        local_address: ipv6(r.ucLocalAddr, r.dwLocalScopeId),
                        local_port: port(r.dwLocalPort),
                        remote_address: None,
                        remote_port: None,
                        state: "Bound",
                        listening: true,
                    })
                    .collect::<Vec<_>>()
            }),
        };
        match rows {
            Ok(mut rows) => {
                if rows.len() > MAX_ENDPOINTS.saturating_sub(result.len()) {
                    rows.truncate(MAX_ENDPOINTS.saturating_sub(result.len()));
                    errors.push("Endpoint list limited to 16384 entries.".into());
                }
                result.extend(rows);
            }
            Err(error) => errors.push(error),
        }
    }
}

fn ipv4(value: u32) -> String {
    Ipv4Addr::from(value.to_ne_bytes()).to_string()
}
fn ipv6(value: [u8; 16], scope: u32) -> String {
    let address = Ipv6Addr::from(value);
    if scope == 0 {
        address.to_string()
    } else {
        format!("{address}%{scope}")
    }
}
fn port(value: u32) -> u16 {
    u16::from_be(value as u16)
}
fn tcp_state(state: u32) -> &'static str {
    match state {
        1 => "Closed",
        2 => "Listening",
        3 => "SYN sent",
        4 => "SYN received",
        5 => "Established",
        6 => "FIN wait 1",
        7 => "FIN wait 2",
        8 => "Close wait",
        9 => "Closing",
        10 => "Last ACK",
        11 => "Time wait",
        12 => "Delete TCB",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_tables_validate_count_bounds_and_network_byte_order() {
        assert_eq!(ipv4(u32::from_ne_bytes([127, 0, 0, 1])), "127.0.0.1");
        assert_eq!(port(u16::to_be(443) as u32), 443);
        assert_eq!(ipv6(Ipv6Addr::LOCALHOST.octets(), 3), "::1%3");
        let mut table = NativeTable {
            storage: vec![0u64; 8],
            bytes: 64,
        };
        table.storage[0] = (MAX_ENDPOINTS as u64) + 1;
        assert!(table_rows::<MIB_TCPROW_OWNER_PID>(&table, 4).is_err());
        table.storage[0] = 3;
        assert!(table_rows::<MIB_TCPROW_OWNER_PID>(&table, 4).is_err());
        table.storage[0] = 1;
        assert_eq!(
            table_rows::<MIB_TCPROW_OWNER_PID>(&table, 4).unwrap().len(),
            1
        );
        table.bytes = 4 + size_of::<MIB_TCPROW_OWNER_PID>() - 1;
        assert!(table_rows::<MIB_TCPROW_OWNER_PID>(&table, 4).is_err());
    }

    #[test]
    fn inactive_or_expired_selection_does_not_start_a_worker() {
        let mut client = Client::default();
        assert!(client.sample(&Request::default(), &[]).is_none());
        let request = Request {
            modules: Some((42, 10)),
            ..Request::default()
        };
        assert!(client.sample(&request, &[]).is_none());
        assert!(client.worker.is_none());
    }
}
