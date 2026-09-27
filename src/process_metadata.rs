//! Optional process columns. A lazily started worker owns all per-process calls;
//! neither the UI nor the bulk monitor waits for token/account/command queries.
use crate::sampler::Process;
use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, SyncSender},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Security::*,
    System::{LibraryLoader::*, Threading::*},
    UI::WindowsAndMessaging::*,
};

type Identity = (u32, u64);
type Query = unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    pub user: bool,
    pub command_line: bool,
    pub status: bool,
}

#[derive(Clone, Default)]
struct Entry {
    user_attempted: bool,
    command_attempted: bool,
    user: Option<String>,
    command: Option<String>,
    status_at: Option<Instant>,
    responsive: Option<bool>,
    efficiency: Option<bool>,
}
struct Request {
    ids: Vec<Identity>,
    needs: Needs,
}
struct Reply {
    entries: HashMap<Identity, Entry>,
    needs: Needs,
}
struct Worker {
    tx: SyncSender<Request>,
    rx: Receiver<Reply>,
}

#[derive(Default)]
pub struct Client {
    worker: Option<Worker>,
    failed: bool,
    latest: Option<Reply>,
}
impl Client {
    pub fn decorate(&mut self, processes: &mut [Process], needs: Needs) {
        if needs == Needs::default() {
            // Static replies remain valid by creation identity across page changes.
            return;
        }
        if self.worker.is_none() && !self.failed {
            let (tx, work) = mpsc::sync_channel(1);
            let (done, rx) = mpsc::sync_channel(1);
            match std::thread::Builder::new()
                .name("feather-columns".into())
                .spawn(move || collect(work, done))
            {
                Ok(_) => self.worker = Some(Worker { tx, rx }),
                Err(_) => self.failed = true,
            }
        }
        let Some(worker) = &self.worker else {
            return;
        };
        while let Ok(reply) = worker.rx.try_recv() {
            self.latest = Some(reply);
        }
        if let Some(reply) = self.latest.as_ref().filter(|r| r.needs == needs) {
            for p in processes.iter_mut() {
                if let Some(e) = reply.entries.get(&(p.pid, p.created)) {
                    if needs.user {
                        p.user_name.clone_from(&e.user);
                    }
                    if needs.command_line {
                        p.command_line.clone_from(&e.command);
                    }
                    // Do not display an old status after a lengthy sampling pause.
                    if needs.status
                        && e.status_at
                            .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
                    {
                        p.responsiveness = e.responsive;
                        p.efficiency = e.efficiency;
                    }
                }
            }
        }
        let _ = worker.tx.try_send(Request {
            ids: processes.iter().map(|p| (p.pid, p.created)).collect(),
            needs,
        });
    }
}

fn collect(work: Receiver<Request>, done: SyncSender<Reply>) {
    let query = command_query();
    let mut entries = HashMap::<Identity, Entry>::new();
    let mut accounts = HashMap::<Vec<u8>, Option<String>>::new();
    let mut cursor = 0usize;
    let mut windows = HashMap::new();
    let mut windows_at: Option<Instant> = None;
    let mut last_needs = Needs::default();
    let mut dirty = false;
    while let Ok(mut request) = work.recv() {
        while let Ok(newer) = work.try_recv() {
            request = newer;
        }
        let live: HashSet<_> = request.ids.iter().copied().collect();
        let before = entries.len();
        entries.retain(|id, _| live.contains(id));
        dirty |= entries.len() != before || last_needs != request.needs;
        last_needs = request.needs;
        if request.needs.status
            && windows_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
        {
            windows = window_status();
            windows_at = Some(Instant::now());
        }
        let start = Instant::now();
        for _ in 0..request.ids.len() {
            cursor %= request.ids.len();
            let id = request.ids[cursor];
            cursor += 1;
            let entry = entries.entry(id).or_default();
            let user = request.needs.user && !entry.user_attempted;
            let command = request.needs.command_line && !entry.command_attempted;
            let status = request.needs.status
                && entry
                    .status_at
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(5));
            if user || command || status {
                dirty = true;
                let handle = checked_process(id);
                if user {
                    entry.user = handle.as_ref().and_then(|h| account(h.0, &mut accounts));
                    entry.user_attempted = true;
                }
                if command {
                    entry.command = handle
                        .as_ref()
                        .and_then(|h| query.and_then(|q| command_line(h.0, q)));
                    entry.command_attempted = true;
                }
                if status {
                    entry.responsive = handle
                        .as_ref()
                        .and_then(|_| windows.get(&id.0))
                        .and_then(|windows| responsiveness(id.0, windows));
                    entry.efficiency = handle.as_ref().and_then(|h| efficiency(h.0));
                    entry.status_at = Some(Instant::now());
                }
            }
            // Fair rotation and bounded work per request. An OS call can take
            // longer, but never holds up the monitor, UI, or shutdown.
            if start.elapsed() >= Duration::from_millis(20) {
                break;
            }
        }
        if dirty
            && done
                .try_send(Reply {
                    entries: entries.clone(),
                    needs: request.needs,
                })
                .is_ok()
        {
            dirty = false;
        }
    }
}

fn command_query() -> Option<Query> {
    unsafe {
        GetProcAddress(
            GetModuleHandleW("ntdll.dll\0".encode_utf16().collect::<Vec<_>>().as_ptr()),
            c"NtQueryInformationProcess".as_ptr().cast(),
        )
        .map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, Query>(f))
    }
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}
fn checked_process(id: Identity) -> Option<Handle> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, id.0);
        if handle.is_null() {
            return None;
        }
        let handle = Handle(handle);
        let (mut created, mut exit, mut kernel, mut user) =
            (zeroed(), zeroed(), zeroed(), zeroed());
        if GetProcessTimes(handle.0, &mut created, &mut exit, &mut kernel, &mut user) == 0 {
            return None;
        }
        let stamp = (created.dwHighDateTime as u64) << 32 | created.dwLowDateTime as u64;
        (stamp == id.1).then_some(handle)
    }
}

fn account(process: HANDLE, accounts: &mut HashMap<Vec<u8>, Option<String>>) -> Option<String> {
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let token = Handle(token);
        let mut needed = 0;
        GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut needed);
        if !(size_of::<TOKEN_USER>() as u32..=4096).contains(&needed) {
            return None;
        }
        let mut storage = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            return None;
        }
        let sid = (*(storage.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        if IsValidSid(sid) == 0 {
            return None;
        }
        let key = std::slice::from_raw_parts(sid.cast::<u8>(), GetLengthSid(sid) as usize).to_vec();
        if let Some(name) = accounts.get(&key) {
            return name.clone();
        }
        let (mut name, mut domain) = ([0u16; 512], [0u16; 512]);
        let (mut n, mut d, mut usage) = (512, 512, 0);
        let value = if LookupAccountSidW(
            null(),
            sid,
            name.as_mut_ptr(),
            &mut n,
            domain.as_mut_ptr(),
            &mut d,
            &mut usage,
        ) != 0
            && n <= 512
            && d <= 512
        {
            let name = String::from_utf16_lossy(&name[..n as usize]);
            Some(if d == 0 {
                name
            } else {
                format!(
                    "{}\\{name}",
                    String::from_utf16_lossy(&domain[..d as usize])
                )
            })
        } else {
            None
        };
        if accounts.len() >= 1024 {
            accounts.clear();
        }
        accounts.insert(key, value.clone());
        value
    }
}

fn command_line(handle: HANDLE, query: Query) -> Option<String> {
    let mut required = 0;
    unsafe {
        query(handle, 60, null_mut(), 0, &mut required);
    }
    if !(16..=131_088).contains(&required) {
        return None;
    }
    let mut data = vec![0u64; (required as usize).div_ceil(8)];
    let capacity = (data.len() * 8) as u32;
    if unsafe {
        query(
            handle,
            60,
            data.as_mut_ptr().cast(),
            capacity,
            &mut required,
        )
    } < 0
    {
        return None;
    }
    if required > capacity {
        return None;
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>(), required as usize) };
    decode_command(bytes)
}

/// The native query returns a UNICODE_STRING pointing inside our allocation.
/// Validate the pointer/length before touching it; never read the remote PEB.
fn decode_command(bytes: &[u8]) -> Option<String> {
    let header = bytes.get(..16)?;
    let length = u16::from_le_bytes(header[..2].try_into().ok()?) as usize;
    let maximum = u16::from_le_bytes(header[2..4].try_into().ok()?) as usize;
    if !length.is_multiple_of(2) || length > maximum {
        return None;
    }
    if length == 0 {
        return Some(String::new());
    }
    let pointer = u64::from_le_bytes(header[8..16].try_into().ok()?) as usize;
    let offset = pointer.checked_sub(bytes.as_ptr() as usize)?;
    if offset < 16 {
        return None;
    }
    let text = bytes.get(offset..offset.checked_add(length)?)?;
    Some(
        char::decode_utf16(
            text.as_chunks::<2>()
                .0
                .iter()
                .map(|w| u16::from_le_bytes(*w)),
        )
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect(),
    )
}

fn efficiency(process: HANDLE) -> Option<bool> {
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    (unsafe {
        GetProcessInformation(
            process,
            ProcessPowerThrottling,
            (&mut state as *mut PROCESS_POWER_THROTTLING_STATE).cast(),
            size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    } != 0)
        .then_some(
            state.ControlMask & state.StateMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED != 0,
        )
}

fn window_status() -> HashMap<u32, Vec<usize>> {
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> windows_sys::core::BOOL {
        if IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER).is_null() {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            let map = &mut *(data as *mut HashMap<u32, Vec<usize>>);
            map.entry(pid).or_default().push(hwnd as usize);
        }
        1
    }
    let mut values = HashMap::new();
    unsafe {
        EnumWindows(Some(visit), &mut values as *mut _ as LPARAM);
    }
    values
}

fn responsiveness(pid: u32, windows: &[usize]) -> Option<bool> {
    let mut result = None;
    for &value in windows {
        unsafe {
            let window = value as HWND;
            let mut owner = 0;
            GetWindowThreadProcessId(window, &mut owner);
            // Cached HWNDs may have closed or been reused since enumeration.
            if owner == pid && IsWindowVisible(window) != 0 && GetWindow(window, GW_OWNER).is_null()
            {
                result = Some(result.unwrap_or(true) && IsHungAppWindow(window) == 0);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_buffer_accepts_unicode_and_rejects_foreign_pointer_and_odd_length() {
        let text: Vec<u8> = "테스트 --flag"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut bytes = vec![0u8; 16 + text.len()];
        bytes[..2].copy_from_slice(&(text.len() as u16).to_le_bytes());
        bytes[2..4].copy_from_slice(&(text.len() as u16).to_le_bytes());
        let pointer = bytes.as_ptr() as u64 + 16;
        bytes[8..16].copy_from_slice(&pointer.to_le_bytes());
        bytes[16..].copy_from_slice(&text);
        assert_eq!(decode_command(&bytes).as_deref(), Some("테스트 --flag"));
        bytes[0] |= 1;
        assert_eq!(decode_command(&bytes), None);
        bytes[0] &= !1;
        bytes[8..16].copy_from_slice(&1u64.to_le_bytes());
        assert_eq!(decode_command(&bytes), None);
    }
    #[test]
    fn native_metadata_reads_only_current_process_generation() {
        let mut sampler = crate::sampler::Sampler::new().unwrap();
        let snapshot = sampler.sample().unwrap();
        let p = snapshot
            .processes
            .iter()
            .find(|p| p.pid == std::process::id())
            .unwrap();
        let handle = checked_process((p.pid, p.created)).unwrap();
        let command = command_line(handle.0, command_query().unwrap()).unwrap();
        assert!(command.contains("FeatherTaskManager"));
        let user = account(handle.0, &mut HashMap::new()).unwrap();
        assert!(!user.is_empty());
        assert!(checked_process((p.pid, p.created.wrapping_add(1))).is_none());
    }
}
