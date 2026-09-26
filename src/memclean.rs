//! Memory cleanup for the "Nuclear Zombie" panel (no UI code): working-set
//! trimming, administrator-only memory-list purges and a read-only scan for
//! zombie processes (exited processes that another process still holds open).
//!
//! Honesty rules (design-system MASTER.md): every number is measured.
//! Nothing here estimates how much memory a zombie keeps (Windows exposes no
//! supported per-zombie figure), so zombies are counted, not sized. Handles
//! inside other processes are never closed: `DUPLICATE_CLOSE_SOURCE` would
//! pull a handle out from under its owner and can crash it. Only our own
//! duplicates are closed. The elevated helper (`--purge-memory-lists`)
//! reports through its exit code only and writes no files.

use std::{
    collections::{HashMap, HashSet},
    ffi::c_void,
    mem::{size_of, size_of_val},
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, DuplicateHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, FILETIME, HANDLE, LUID,
        WAIT_OBJECT_0,
    },
    Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        SE_PROF_SINGLE_PROCESS_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    },
    System::{
        ProcessStatus::{K32EmptyWorkingSet, K32EnumProcesses},
        SystemInformation::{
            GetSystemInfo, GetSystemTimeAsFileTime, GlobalMemoryStatusEx, MEMORYSTATUSEX,
            SYSTEM_INFO,
        },
        Threading::{
            GetCurrentProcess, GetCurrentProcessId, GetProcessId, GetProcessTimes, OpenProcess,
            OpenProcessToken, QueryFullProcessImageNameW, WaitForSingleObject, PROCESS_DUP_HANDLE,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA, PROCESS_SYNCHRONIZE,
        },
    },
};

use crate::i18n::tr;
use crate::trf as tf;

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        information: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
    fn NtSetSystemInformation(class: u32, information: *const c_void, length: u32) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        information: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

/// `SYSTEM_INFORMATION_CLASS` values (stable since Windows XP / Vista).
const SYSTEM_EXTENDED_HANDLE_INFORMATION: u32 = 64;
const SYSTEM_MEMORY_LIST_INFORMATION: u32 = 80;
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
const STATUS_BUFFER_TOO_SMALL: i32 = 0xC000_0023_u32 as i32;
/// The handle table query never allocates more than this.
const MAX_HANDLE_BUFFER: usize = 256 << 20;
/// `SYSTEM_HANDLE_INFORMATION_EX` header and `_TABLE_ENTRY_INFO_EX` (x64).
const HANDLE_HEADER: usize = 16;
const HANDLE_ENTRY: usize = 40;
/// `SYSTEM_MEMORY_LIST_INFORMATION`: 22 pointer-sized counters (x64).
const MEMORY_LIST_WORDS: usize = 22;

struct Owned(HANDLE);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn filetime(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

fn creation_time(handle: HANDLE) -> Option<u64> {
    let (mut created, mut exited, mut kernel, mut user) = Default::default();
    (unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } != 0)
        .then(|| filetime(created))
        .filter(|&value| value != 0)
}

fn file_name(path: &str) -> Option<String> {
    path.rsplit(['\\', '/'])
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// The executable file name (no directory) of a process handle. An exited
/// process no longer answers `QueryFullProcessImageNameW`; the image name
/// the kernel keeps for it (`ProcessImageFileName`, class 27) still does.
fn image_name(handle: HANDLE) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) } != 0 {
        return file_name(&String::from_utf16_lossy(
            &buffer[..(length as usize).min(buffer.len())],
        ));
    }
    // A UNICODE_STRING { u16 length, u16 maximum, u32 pad, *u16 } whose
    // text follows it in the same buffer.
    let mut raw = [0u64; 260];
    let mut returned = 0u32;
    let status = unsafe {
        NtQueryInformationProcess(
            handle,
            27,
            raw.as_mut_ptr().cast(),
            size_of_val(&raw) as u32,
            &mut returned,
        )
    };
    if status < 0 {
        return None;
    }
    let bytes = (raw[0] & 0xffff) as usize;
    let text = raw[1] as usize;
    let start = raw.as_ptr() as usize;
    let end = start + size_of_val(&raw);
    if bytes == 0 || text < start + 16 || text + bytes > end || !text.is_multiple_of(2) {
        return None;
    }
    let text = unsafe { std::slice::from_raw_parts(text as *const u16, bytes / 2) };
    file_name(&String::from_utf16_lossy(text))
}

/// An NTSTATUS as text: the Windows message for it plus the code.
pub fn status_text(status: i32) -> String {
    let code = unsafe { RtlNtStatusToDosError(status) };
    // ERROR_MR_MID_NOT_FOUND (317): no Win32 equivalent.
    if code == 0 || code == 317 {
        return format!("NTSTATUS 0x{:08X}", status as u32);
    }
    let message = std::io::Error::from_raw_os_error(code as i32).to_string();
    let message = message
        .split(" (os error")
        .next()
        .unwrap_or(&message)
        .trim()
        .trim_end_matches('.')
        .to_owned();
    format!("{message} (0x{:08X})", status as u32)
}

// ───────────────────────────── memory state ─────────────────────────────

/// The standby / modified / free page lists in bytes
/// (`SystemMemoryListInformation`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryLists {
    /// Standby list, all eight priorities (cached, reusable at once).
    pub standby: u64,
    /// Modified list: changed pages not yet written to disk.
    pub modified: u64,
    /// Free and zeroed pages.
    pub free: u64,
}

/// One measurement of physical memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryState {
    pub total: u64,
    /// `GlobalMemoryStatusEx` available bytes (standby + free + zeroed).
    pub available: u64,
    /// None when Windows does not report the lists to this process.
    pub lists: Option<MemoryLists>,
}

impl MemoryState {
    /// Total minus available, as the app's memory meter reports it.
    pub fn in_use(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

fn page_size() -> u64 {
    let mut info = SYSTEM_INFO::default();
    unsafe { GetSystemInfo(&mut info) };
    u64::from(info.dwPageSize.max(1))
}

/// Parse `SYSTEM_MEMORY_LIST_INFORMATION` counters (pages) into bytes:
/// zero, free, modified, modified-no-write, bad, standby[8], repurposed[8],
/// modified-pagefile. None when the buffer is short or a sum overflows.
pub(crate) fn parse_memory_lists(words: &[usize], page: u64) -> Option<MemoryLists> {
    if words.len() < 13 || page == 0 {
        return None;
    }
    let bytes = |pages: u64| pages.checked_mul(page);
    let standby = words[5..13]
        .iter()
        .try_fold(0u64, |sum, &pages| sum.checked_add(pages as u64))?;
    Some(MemoryLists {
        standby: bytes(standby)?,
        modified: bytes(words[2] as u64)?,
        free: bytes((words[0] as u64).checked_add(words[1] as u64)?)?,
    })
}

/// The page lists, or the NTSTATUS the query failed with.
pub fn memory_lists() -> Result<MemoryLists, i32> {
    let mut words = [0usize; MEMORY_LIST_WORDS];
    let mut returned = 0u32;
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_MEMORY_LIST_INFORMATION,
            words.as_mut_ptr().cast(),
            size_of_val(&words) as u32,
            &mut returned,
        )
    };
    if status < 0 {
        return Err(status);
    }
    let count = (returned as usize / size_of::<usize>()).min(words.len());
    parse_memory_lists(&words[..count], page_size()).ok_or(STATUS_BUFFER_TOO_SMALL)
}

/// Measure physical memory now (cheap: two system calls).
pub fn memory_state() -> Result<MemoryState, String> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(tf!(
            "메모리 상태를 읽을 수 없습니다: {}",
            "Cannot read the memory status: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(MemoryState {
        total: status.ullTotalPhys,
        available: status.ullAvailPhys.min(status.ullTotalPhys),
        lists: memory_lists().ok(),
    })
}

// ───────────────────────────── working sets ─────────────────────────────

/// Outcome of trimming the working sets of every process we may open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrimReport {
    pub trimmed: u32,
    /// Could not be opened with the rights trimming needs (usually
    /// processes of other users, services or protected processes).
    pub skipped: u32,
    /// Opened, but Windows refused to trim.
    pub failed: u32,
}

/// Every process ID (PIDs 0 and 4, Idle and System, excluded).
pub fn process_ids() -> Result<Vec<u32>, String> {
    let mut ids = vec![0u32; 1024];
    for _ in 0..8 {
        let bytes = (ids.len() * size_of::<u32>()) as u32;
        let mut used = 0u32;
        if unsafe { K32EnumProcesses(ids.as_mut_ptr(), bytes, &mut used) } == 0 {
            return Err(tf!(
                "프로세스 목록을 읽을 수 없습니다: {}",
                "Cannot list processes: {}",
                std::io::Error::last_os_error()
            ));
        }
        if used < bytes {
            ids.truncate(used as usize / size_of::<u32>());
            ids.retain(|&pid| pid != 0 && pid != 4);
            return Ok(ids);
        }
        ids.resize(ids.len() * 2, 0);
    }
    Err(tr(
        "프로세스 목록이 너무 큽니다.",
        "The process list is too large.",
    )
    .into())
}

fn open_for_trim(pid: u32) -> Option<Owned> {
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SET_QUOTA,
            0,
            pid,
        )
    };
    (!raw.is_null()).then_some(Owned(raw))
}

/// How many of `ids` could be trimmed (opened with the required rights),
/// without trimming anything (the dry run).
pub fn count_trimmable(ids: &[u32]) -> TrimReport {
    let mut report = TrimReport::default();
    for &pid in ids {
        match open_for_trim(pid) {
            Some(_) => report.trimmed += 1,
            None => report.skipped += 1,
        }
    }
    report
}

/// `EmptyWorkingSet` on every process of `ids` we can open. Running apps
/// keep running: their idle pages move to the standby / modified lists and
/// fault back in when used.
pub fn trim_working_sets(ids: &[u32]) -> TrimReport {
    let mut report = TrimReport::default();
    for &pid in ids {
        let Some(process) = open_for_trim(pid) else {
            report.skipped += 1;
            continue;
        };
        if unsafe { K32EmptyWorkingSet(process.0) } != 0 {
            report.trimmed += 1;
        } else {
            report.failed += 1;
        }
    }
    report
}

// ───────────────────────────── memory lists (administrator) ─────────────────────────────

/// A memory list the elevated purge can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryList {
    /// `MemoryPurgeStandbyList`: drops cached file data.
    Standby,
    /// `MemoryPurgeLowPriorityStandbyList`.
    LowPriorityStandby,
    /// `MemoryFlushModifiedList`: writes changed pages to disk.
    Modified,
}

impl MemoryList {
    /// `SYSTEM_MEMORY_LIST_COMMAND`.
    fn command(self) -> u32 {
        match self {
            Self::Modified => 3,
            Self::Standby => 4,
            Self::LowPriorityStandby => 5,
        }
    }
}

/// The lists a `--purge-memory-lists` argument names, in the order they
/// run (modified first: flushed pages then join the standby list).
pub fn parse_lists(argument: &str) -> Option<Vec<MemoryList>> {
    Some(match argument {
        "standby" => vec![MemoryList::Standby],
        "lowstandby" => vec![MemoryList::LowPriorityStandby],
        "modified" => vec![MemoryList::Modified],
        "all" => vec![MemoryList::Modified, MemoryList::Standby],
        _ => return None,
    })
}

/// The helper argument for the UI's choice (None: nothing to purge).
pub fn lists_argument(modified: bool, standby: bool) -> Option<&'static str> {
    match (modified, standby) {
        (true, true) => Some("all"),
        (true, false) => Some("modified"),
        (false, true) => Some("standby"),
        (false, false) => None,
    }
}

/// Why a list was not purged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PurgeError {
    /// The administrator (UAC) request was declined.
    Declined,
    /// No administrator helper could run (the text says why).
    Unavailable(String),
    /// An earlier list failed, so this one was not attempted.
    NotRun,
    /// `SeProfileSingleProcessPrivilege` could not be enabled.
    Privilege(String),
    /// `NtSetSystemInformation` failed with this NTSTATUS.
    Status(i32),
}

impl PurgeError {
    pub fn message(&self) -> String {
        match self {
            Self::Declined => tr(
                "관리자 권한 요청이 취소되었습니다",
                "The administrator request was declined",
            )
            .into(),
            Self::Unavailable(detail) => detail.clone(),
            Self::NotRun => tr(
                "앞 단계가 실패해 실행하지 않았습니다",
                "Not run because the previous step failed",
            )
            .into(),
            Self::Privilege(detail) => tf!(
                "메모리 목록 권한을 사용할 수 없습니다: {}",
                "The memory list privilege is unavailable: {}",
                detail
            ),
            Self::Status(status) => status_text(*status),
        }
    }
}

pub type PurgeResults = Vec<(MemoryList, Result<(), PurgeError>)>;

/// A privilege enabled in this process's token for as long as the value
/// lives; dropping it restores the previous state, so an elevated UI does
/// not keep the privilege enabled after a purge (FTM-2026-06).
struct EnabledPrivilege {
    token: Owned,
    /// What `AdjustTokenPrivileges` changed (count 0: it was already on).
    previous: TOKEN_PRIVILEGES,
}

impl EnabledPrivilege {
    /// Enable the privilege `name` (a token that does not hold it fails).
    fn enable(name: windows_sys::core::PCWSTR) -> Result<Self, String> {
        let mut token = null_mut();
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let token = Owned(token);
        let mut luid = LUID::default();
        if unsafe { LookupPrivilegeValueW(null(), name, &mut luid) } == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let privileges = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let mut previous = TOKEN_PRIVILEGES::default();
        let mut length = 0;
        if unsafe {
            AdjustTokenPrivileges(
                token.0,
                0,
                &privileges,
                size_of::<TOKEN_PRIVILEGES>() as u32,
                &mut previous,
                &mut length,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
        // From here on, dropping restores whatever was changed.
        let enabled = Self { token, previous };
        // Success with ERROR_NOT_ALL_ASSIGNED: the token does not hold it.
        if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            return Err(tr(
                "이 계정에 권한이 없습니다(관리자 권한 필요)",
                "this account does not hold it (administrator required)",
            )
            .into());
        }
        Ok(enabled)
    }
}

impl Drop for EnabledPrivilege {
    fn drop(&mut self) {
        if self.previous.PrivilegeCount != 0 {
            unsafe {
                AdjustTokenPrivileges(self.token.0, 0, &self.previous, 0, null_mut(), null_mut())
            };
        }
    }
}

/// Purge `lists` in order from this (elevated) process. After a failure
/// the remaining lists are not attempted. `SeProfileSingleProcessPrivilege`
/// (administrators hold it, disabled by default) is enabled only for the
/// duration of the purge.
pub fn purge_in_process(lists: &[MemoryList]) -> PurgeResults {
    let privilege = match EnabledPrivilege::enable(SE_PROF_SINGLE_PROCESS_NAME) {
        Ok(privilege) => privilege,
        Err(detail) => {
            return lists
                .iter()
                .map(|&list| (list, Err(PurgeError::Privilege(detail.clone()))))
                .collect();
        }
    };
    let mut failed = false;
    let results = lists
        .iter()
        .map(|&list| {
            if failed {
                return (list, Err(PurgeError::NotRun));
            }
            let command = list.command();
            let status = unsafe {
                NtSetSystemInformation(
                    SYSTEM_MEMORY_LIST_INFORMATION,
                    (&command as *const u32).cast(),
                    size_of::<u32>() as u32,
                )
            };
            if status < 0 {
                failed = true;
                (list, Err(PurgeError::Status(status)))
            } else {
                (list, Ok(()))
            }
        })
        .collect();
    // Disable it again right away (restores the previous state).
    drop(privilege);
    results
}

/// Helper exit codes: 0 success; 1 usage; 2 privilege not enabled; an
/// NTSTATUS error of the failed list otherwise, with the NTSTATUS
/// "customer" bit (never set by Windows) marking a failure of the second
/// list of "all" (the first then succeeded).
pub const HELPER_USAGE: u32 = 1;
pub const HELPER_PRIVILEGE: u32 = 2;
const SECOND_LIST: u32 = 0x2000_0000;

/// The exit code for a purge's results.
pub fn helper_exit_code(results: &PurgeResults) -> u32 {
    for (index, (_, result)) in results.iter().enumerate() {
        match result {
            Ok(()) | Err(PurgeError::NotRun) => {}
            Err(PurgeError::Privilege(_)) => return HELPER_PRIVILEGE,
            Err(PurgeError::Status(status)) => {
                let code = *status as u32 & !SECOND_LIST;
                return if index == 0 { code } else { code | SECOND_LIST };
            }
            Err(_) => return HELPER_USAGE,
        }
    }
    0
}

/// The per-list results an elevated helper's exit code stands for.
pub fn decode_helper_exit(lists: &[MemoryList], code: u32) -> PurgeResults {
    let all = |error: PurgeError| {
        lists
            .iter()
            .map(|&list| (list, Err(error.clone())))
            .collect::<PurgeResults>()
    };
    match code {
        0 => lists.iter().map(|&list| (list, Ok(()))).collect(),
        HELPER_PRIVILEGE => all(PurgeError::Privilege(
            tr(
                "관리자 도우미가 권한을 얻지 못했습니다",
                "the administrator helper could not enable it",
            )
            .into(),
        )),
        code if code & 0x8000_0000 != 0 => {
            let failed = usize::from(code & SECOND_LIST != 0);
            let status = (code & !SECOND_LIST) as i32;
            lists
                .iter()
                .enumerate()
                .map(|(index, &list)| {
                    let result = match index.cmp(&failed) {
                        std::cmp::Ordering::Less => Ok(()),
                        std::cmp::Ordering::Equal => Err(PurgeError::Status(status)),
                        std::cmp::Ordering::Greater => Err(PurgeError::NotRun),
                    };
                    (list, result)
                })
                .collect()
        }
        code => all(PurgeError::Unavailable(tf!(
            "관리자 도우미가 종료 코드 {}로 끝났습니다",
            "The administrator helper ended with exit code {}",
            code
        ))),
    }
}

/// `FeatherTaskManager.exe --purge-memory-lists <standby|lowstandby|modified|all>`
/// (launched elevated by the UI): purge and return the exit code. Writes
/// nothing; the UI measures memory before and after itself.
pub fn helper_main(argument: Option<&str>) -> u32 {
    match argument.and_then(parse_lists) {
        Some(lists) => helper_exit_code(&purge_in_process(&lists)),
        None => HELPER_USAGE,
    }
}

/// Purge `lists`: in this process when it is elevated, else through the
/// installed image's elevated helper (UAC prompt), waiting for it. Worker
/// thread only.
pub fn purge(lists: &[MemoryList], argument: &str, elevated: bool) -> PurgeResults {
    if elevated {
        return purge_in_process(lists);
    }
    let unavailable = |error: PurgeError| {
        lists
            .iter()
            .map(|&list| (list, Err(error.clone())))
            .collect::<PurgeResults>()
    };
    match crate::replacement::run_installed_helper(&format!("--purge-memory-lists {argument}")) {
        Ok(crate::replacement::HelperLaunch::Exited(code)) => decode_helper_exit(lists, code),
        Ok(crate::replacement::HelperLaunch::Cancelled) => unavailable(PurgeError::Declined),
        Err(detail) => unavailable(PurgeError::Unavailable(detail)),
    }
}

// ───────────────────────────── zombie processes ─────────────────────────────

/// One `SYSTEM_HANDLE_TABLE_ENTRY_INFO_EX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HandleEntry {
    /// Kernel object address (zero when Windows hides it from this caller).
    pub object: usize,
    pub pid: usize,
    pub handle: usize,
    pub access: u32,
    pub type_index: u16,
}

/// Parse a `SYSTEM_HANDLE_INFORMATION_EX` buffer (x64 layout: 16-byte
/// header, 40-byte entries). The count must fit the buffer.
pub(crate) fn parse_handle_table(buffer: &[u8]) -> Result<Vec<HandleEntry>, String> {
    let word = |bytes: &[u8], at: usize| -> usize {
        let mut value = [0u8; 8];
        value.copy_from_slice(&bytes[at..at + 8]);
        u64::from_le_bytes(value) as usize
    };
    if buffer.len() < HANDLE_HEADER {
        return Err("Truncated handle table".into());
    }
    let count = word(buffer, 0);
    let needed = count
        .checked_mul(HANDLE_ENTRY)
        .and_then(|bytes| bytes.checked_add(HANDLE_HEADER))
        .ok_or("Invalid handle count")?;
    if needed > buffer.len() {
        return Err("Handle count exceeds the returned table".into());
    }
    Ok(buffer[HANDLE_HEADER..needed]
        .as_chunks::<HANDLE_ENTRY>()
        .0
        .iter()
        .map(|entry| HandleEntry {
            object: word(&entry[..], 0),
            pid: word(&entry[..], 8),
            handle: word(&entry[..], 16),
            access: u32::from_le_bytes([entry[24], entry[25], entry[26], entry[27]]),
            type_index: u16::from_le_bytes([entry[30], entry[31]]),
        })
        .collect())
}

/// Query the system handle table with a growing, bounded buffer.
fn query_handle_table() -> Result<Vec<u8>, String> {
    let mut size = 4usize << 20;
    loop {
        // u64 elements keep the buffer 8-byte aligned.
        let mut buffer = vec![0u64; size / 8];
        let mut returned = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                SYSTEM_EXTENDED_HANDLE_INFORMATION,
                buffer.as_mut_ptr().cast(),
                size as u32,
                &mut returned,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH || status == STATUS_BUFFER_TOO_SMALL {
            // The table grows between calls: leave headroom.
            let wanted = (returned as usize).max(size) + (size / 2);
            if wanted > MAX_HANDLE_BUFFER || size >= MAX_HANDLE_BUFFER {
                return Err(
                    tr("핸들 목록이 너무 큽니다.", "The handle table is too large.").into(),
                );
            }
            size = wanted.next_multiple_of(8).min(MAX_HANDLE_BUFFER);
            continue;
        }
        if status < 0 {
            return Err(tf!(
                "핸들 목록을 읽을 수 없습니다: {}",
                "Cannot read the handle table: {}",
                status_text(status)
            ));
        }
        let bytes = (returned as usize).min(size);
        let mut out = Vec::with_capacity(bytes);
        for value in &buffer[..bytes.div_ceil(8)] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.truncate(bytes);
        return Ok(out);
    }
}

/// A process that holds handles to exited processes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZombieHolder {
    pub pid: u32,
    /// Creation time (identity for the guarded End task).
    pub created: u64,
    pub name: String,
    /// Distinct exited processes it holds open.
    pub zombies: u32,
    /// The most common executable names among them, with counts (up to 3;
    /// names Windows would not report are left out).
    pub examples: Vec<(String, u32)>,
}

/// A zombie scan. Counts only: the memory a zombie keeps is not measurable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZombieScan {
    /// Busiest first.
    pub holders: Vec<ZombieHolder>,
    /// Distinct exited processes held open by the inspected processes.
    pub total: u32,
    /// Processes whose handles were inspected.
    pub inspected: u32,
    /// Processes holding process handles that could not be inspected
    /// (other users, services, protected processes; more need
    /// administrator rights).
    pub uninspected: u32,
    /// Process handles that could not be duplicated for inspection.
    pub uninspected_handles: u32,
}

impl ZombieScan {
    pub fn partial(&self) -> bool {
        self.uninspected > 0 || self.uninspected_handles > 0
    }
}

/// Group the process-type handles of `entries` by holder (skipping the
/// Idle, System and `own` processes), holder PIDs ascending.
pub(crate) fn process_handles_by_holder(
    entries: &[HandleEntry],
    process_type: u16,
    own: usize,
) -> Vec<(u32, Vec<usize>)> {
    let mut holders: HashMap<u32, Vec<usize>> = HashMap::new();
    for entry in entries {
        if entry.type_index != process_type
            || entry.pid == own
            || entry.pid == 0
            || entry.pid == 4
            || entry.pid > u32::MAX as usize
        {
            continue;
        }
        holders
            .entry(entry.pid as u32)
            .or_default()
            .push(entry.handle);
    }
    let mut holders: Vec<_> = holders.into_iter().collect();
    holders.sort_by_key(|(pid, _)| *pid);
    holders
}

/// Busiest holder first, then by name and PID; examples most common first.
pub(crate) fn sort_holders(holders: &mut [ZombieHolder]) {
    holders.sort_by(|a, b| {
        b.zombies
            .cmp(&a.zombies)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then(a.pid.cmp(&b.pid))
    });
}

fn top_examples(names: HashMap<String, u32>) -> Vec<(String, u32)> {
    let mut names: Vec<_> = names.into_iter().collect();
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    names.truncate(3);
    names
}

/// Find exited processes that other processes still hold open. Read-only:
/// every duplicated handle is our own and is closed; nothing in another
/// process is changed.
pub fn scan_zombies() -> Result<ZombieScan, String> {
    let own_pid = unsafe { GetCurrentProcessId() };
    let mut now = FILETIME::default();
    unsafe { GetSystemTimeAsFileTime(&mut now) };
    let snapshot_time = filetime(now);
    // A real handle to ourselves identifies the Process object type index.
    let own = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, own_pid) };
    if own.is_null() {
        return Err(tf!(
            "프로세스 핸들 형식을 확인할 수 없습니다: {}",
            "Cannot identify the process handle type: {}",
            std::io::Error::last_os_error()
        ));
    }
    let own = Owned(own);
    let buffer = query_handle_table()?;
    let entries = parse_handle_table(&buffer)?;
    drop(buffer);
    let process_type = entries
        .iter()
        .find(|e| e.pid == own_pid as usize && e.handle == own.0 as usize)
        .map(|e| e.type_index)
        .ok_or_else(|| {
            tr(
                "핸들 목록에서 프로세스 형식을 찾지 못했습니다.",
                "The process handle type was not found in the handle table.",
            )
            .to_owned()
        })?;
    drop(own);
    let holders = process_handles_by_holder(&entries, process_type, own_pid as usize);
    drop(entries);
    let mut scan = ZombieScan::default();
    let mut all: HashSet<(u32, u64)> = HashSet::new();
    let current = unsafe { GetCurrentProcess() };
    for (pid, handles) in holders {
        let raw = unsafe {
            OpenProcess(
                PROCESS_DUP_HANDLE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        if raw.is_null() {
            scan.uninspected += 1;
            scan.uninspected_handles += handles.len() as u32;
            continue;
        }
        let holder = Owned(raw);
        // A holder created after the table was read reused a PID: the
        // handle values belong to another process.
        let Some(created) = creation_time(holder.0).filter(|&c| c <= snapshot_time) else {
            scan.uninspected += 1;
            scan.uninspected_handles += handles.len() as u32;
            continue;
        };
        scan.inspected += 1;
        let mut zombies: HashSet<(u32, u64)> = HashSet::new();
        let mut names: HashMap<String, u32> = HashMap::new();
        for value in handles {
            let mut duplicate: HANDLE = null_mut();
            // Our own limited-access copy; the holder's handle is untouched
            // (options 0: never DUPLICATE_CLOSE_SOURCE).
            let ok = unsafe {
                DuplicateHandle(
                    holder.0,
                    value as HANDLE,
                    current,
                    &mut duplicate,
                    PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                    0,
                    0,
                )
            };
            if ok == 0 || duplicate.is_null() {
                scan.uninspected_handles += 1;
                continue;
            }
            let duplicate = Owned(duplicate);
            // GetProcessId fails for anything that is not a process.
            let target = unsafe { GetProcessId(duplicate.0) };
            if target == 0 || target == pid {
                continue;
            }
            if unsafe { WaitForSingleObject(duplicate.0, 0) } != WAIT_OBJECT_0 {
                continue;
            }
            let Some(target_created) = creation_time(duplicate.0) else {
                continue;
            };
            let identity = (target, target_created);
            if zombies.insert(identity) {
                if let Some(name) = image_name(duplicate.0) {
                    *names.entry(name).or_default() += 1;
                }
            }
            all.insert(identity);
        }
        if !zombies.is_empty() {
            scan.holders.push(ZombieHolder {
                pid,
                created,
                name: image_name(holder.0).unwrap_or_else(|| format!("PID {pid}")),
                zombies: zombies.len() as u32,
                examples: top_examples(names),
            });
        }
    }
    scan.total = all.len() as u32;
    sort_holders(&mut scan.holders);
    Ok(scan)
}

// ───────────────────────────── one run ─────────────────────────────

/// What the user chose in the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanupOptions {
    pub trim: bool,
    pub standby: bool,
    pub modified: bool,
    pub zombies: bool,
}

impl CleanupOptions {
    pub fn any(&self) -> bool {
        self.trim || self.standby || self.modified || self.zombies
    }
}

/// Progress of a run, reported before each step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    Trimming(usize),
    /// UAC prompt (the helper waits for the user).
    WaitingForAdministrator,
    Purging,
    Scanning,
}

/// Everything one run measured and did. `None` = the step was not chosen.
#[derive(Clone, Debug)]
pub struct CleanupReport {
    pub before: Result<MemoryState, String>,
    pub after: Result<MemoryState, String>,
    pub trim: Option<Result<TrimReport, String>>,
    pub modified: Option<Result<(), PurgeError>>,
    pub standby: Option<Result<(), PurgeError>>,
    pub zombies: Option<Result<ZombieScan, String>>,
    pub elevated: bool,
}

/// Run the chosen steps in order (trim, flush modified, purge standby,
/// zombie scan), measuring memory right before and after. Worker thread
/// only: the administrator step waits for the UAC prompt.
pub fn run(options: CleanupOptions, mut progress: impl FnMut(Progress)) -> CleanupReport {
    let elevated = crate::netetw::is_elevated();
    let before = memory_state();
    let trim = options.trim.then(|| {
        process_ids().map(|ids| {
            progress(Progress::Trimming(ids.len()));
            trim_working_sets(&ids)
        })
    });
    let (mut modified, mut standby) = (None, None);
    if let Some(argument) = lists_argument(options.modified, options.standby) {
        let lists = parse_lists(argument).unwrap_or_default();
        progress(if elevated {
            Progress::Purging
        } else {
            Progress::WaitingForAdministrator
        });
        for (list, result) in purge(&lists, argument, elevated) {
            match list {
                MemoryList::Modified => modified = Some(result),
                _ => standby = Some(result),
            }
        }
    }
    let zombies = options.zombies.then(|| {
        progress(Progress::Scanning);
        scan_zombies()
    });
    CleanupReport {
        before,
        after: memory_state(),
        trim,
        modified,
        standby,
        zombies,
        elevated,
    }
}

/// `--memory-cleanup-dry-run <file>`: what a run would find, without
/// trimming or purging anything (memory, trimmable processes, zombies).
pub fn dry_run_report() -> Result<String, String> {
    use std::fmt::Write as _;
    let mut out = String::new();
    let gb = |bytes: u64| bytes as f64 / 1_073_741_824.0;
    let state = memory_state()?;
    let _ = writeln!(
        out,
        "elevated={}\nmemory_total_gb={:.2}\nmemory_available_gb={:.2}\nmemory_in_use_gb={:.2}",
        crate::netetw::is_elevated(),
        gb(state.total),
        gb(state.available),
        gb(state.in_use())
    );
    match memory_lists() {
        Ok(lists) => {
            let _ = writeln!(
                out,
                "memory_standby_gb={:.2}\nmemory_modified_gb={:.2}\nmemory_free_gb={:.2}",
                gb(lists.standby),
                gb(lists.modified),
                gb(lists.free)
            );
        }
        Err(status) => {
            let _ = writeln!(out, "memory_lists=unavailable ({})", status_text(status));
        }
    }
    let ids = process_ids()?;
    let trimmable = count_trimmable(&ids);
    let _ = writeln!(
        out,
        "processes={}\ntrimmable={}\ntrim_skipped_access={}",
        ids.len(),
        trimmable.trimmed,
        trimmable.skipped
    );
    let started = std::time::Instant::now();
    let scan = scan_zombies()?;
    let _ = writeln!(
        out,
        "zombie_scan_ms={:.1}\nzombies_total={}\nholders={}\ninspected_processes={}\nuninspected_processes={}\nuninspected_handles={}",
        started.elapsed().as_secs_f64() * 1000.0,
        scan.total,
        scan.holders.len(),
        scan.inspected,
        scan.uninspected,
        scan.uninspected_handles
    );
    for holder in &scan.holders {
        let _ = writeln!(
            out,
            "holder {} (PID {}) zombies={} examples={:?}",
            holder.name, holder.pid, holder.zombies, holder.examples
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(object: usize, pid: usize, handle: usize, access: u32, kind: u16) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HANDLE_ENTRY);
        bytes.extend_from_slice(&(object as u64).to_le_bytes());
        bytes.extend_from_slice(&(pid as u64).to_le_bytes());
        bytes.extend_from_slice(&(handle as u64).to_le_bytes());
        bytes.extend_from_slice(&access.to_le_bytes());
        bytes.extend_from_slice(&7u16.to_le_bytes()); // creator back trace
        bytes.extend_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes()); // attributes
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(bytes.len(), HANDLE_ENTRY);
        bytes
    }

    fn table(count: u64, entries: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = count.to_le_bytes().to_vec();
        bytes.extend_from_slice(&0u64.to_le_bytes());
        for e in entries {
            bytes.extend_from_slice(e);
        }
        bytes
    }

    #[test]
    fn handle_table_parses_x64_entries_and_rejects_bad_counts() {
        let entries = [
            entry(0xFFFF_8000_0000_1000, 1234, 0x4c, 0x1fffff, 7),
            entry(0, 88, 0x10, 0x101000, 8),
        ];
        let parsed = parse_handle_table(&table(2, &entries)).unwrap();
        assert_eq!(
            parsed,
            vec![
                HandleEntry {
                    object: 0xFFFF_8000_0000_1000,
                    pid: 1234,
                    handle: 0x4c,
                    access: 0x1fffff,
                    type_index: 7
                },
                HandleEntry {
                    object: 0,
                    pid: 88,
                    handle: 0x10,
                    access: 0x101000,
                    type_index: 8
                }
            ]
        );
        // Extra trailing bytes (the buffer is larger than the table) are fine.
        let mut padded = table(1, &entries[..1]);
        padded.extend_from_slice(&[0u8; 17]);
        assert_eq!(parse_handle_table(&padded).unwrap().len(), 1);
        assert!(parse_handle_table(&table(0, &[])).unwrap().is_empty());
        // A count past the buffer, a truncated header, an overflowing count.
        assert!(parse_handle_table(&table(3, &entries)).is_err());
        assert!(parse_handle_table(&[0u8; 8]).is_err());
        assert!(parse_handle_table(&table(u64::MAX, &entries)).is_err());
        let mut short = table(2, &entries);
        short.truncate(short.len() - 1);
        assert!(parse_handle_table(&short).is_err());
    }

    #[test]
    fn process_handles_group_by_holder_without_idle_system_or_self() {
        let e = |pid, handle, kind| HandleEntry {
            object: 0,
            pid,
            handle,
            access: 0,
            type_index: kind,
        };
        let entries = [
            e(900, 0x10, 7),
            e(900, 0x14, 7),
            e(900, 0x18, 3), // not a process handle
            e(4, 0x10, 7),   // System
            e(0, 0x10, 7),   // Idle
            e(555, 0x20, 7), // ourselves
            e(120, 0x30, 7),
        ];
        assert_eq!(
            process_handles_by_holder(&entries, 7, 555),
            vec![(120, vec![0x30]), (900, vec![0x10, 0x14])]
        );
        assert!(process_handles_by_holder(&entries, 9, 555).is_empty());
    }

    #[test]
    fn holders_sort_busiest_first_and_examples_keep_the_top_three() {
        let holder = |pid, name: &str, zombies| ZombieHolder {
            pid,
            created: 1,
            name: name.into(),
            zombies,
            examples: Vec::new(),
        };
        let mut holders = vec![
            holder(3, "b.exe", 2),
            holder(1, "svchost.exe", 30),
            holder(2, "A.exe", 2),
        ];
        sort_holders(&mut holders);
        let order: Vec<u32> = holders.iter().map(|h| h.pid).collect();
        assert_eq!(order, vec![1, 2, 3]);
        let names = HashMap::from([
            ("a.exe".to_owned(), 1),
            ("b.exe".to_owned(), 5),
            ("c.exe".to_owned(), 5),
            ("d.exe".to_owned(), 2),
        ]);
        assert_eq!(
            top_examples(names),
            vec![
                ("b.exe".to_owned(), 5),
                ("c.exe".to_owned(), 5),
                ("d.exe".to_owned(), 2)
            ]
        );
    }

    #[test]
    fn memory_lists_parse_pages_into_bytes() {
        let mut words = [0usize; MEMORY_LIST_WORDS];
        words[0] = 10; // zeroed
        words[1] = 5; // free
        words[2] = 7; // modified
        words[3] = 99; // modified no-write (not flushable, not counted)
        for (i, w) in words[5..13].iter_mut().enumerate() {
            *w = i + 1; // standby priorities 0..7: 36 pages
        }
        let lists = parse_memory_lists(&words, 4096).unwrap();
        assert_eq!(lists.free, 15 * 4096);
        assert_eq!(lists.modified, 7 * 4096);
        assert_eq!(lists.standby, 36 * 4096);
        assert!(parse_memory_lists(&words[..12], 4096).is_none());
        assert!(parse_memory_lists(&words, 0).is_none());
        let mut huge = words;
        huge[5] = usize::MAX;
        assert!(parse_memory_lists(&huge, 4096).is_none());
    }

    #[test]
    fn list_arguments_round_trip_in_run_order() {
        use MemoryList::*;
        assert_eq!(parse_lists("all"), Some(vec![Modified, Standby]));
        assert_eq!(parse_lists("standby"), Some(vec![Standby]));
        assert_eq!(parse_lists("lowstandby"), Some(vec![LowPriorityStandby]));
        assert_eq!(parse_lists("modified"), Some(vec![Modified]));
        assert_eq!(parse_lists("everything"), None);
        assert_eq!(parse_lists(""), None);
        assert_eq!(lists_argument(true, true), Some("all"));
        assert_eq!(lists_argument(false, true), Some("standby"));
        assert_eq!(lists_argument(true, false), Some("modified"));
        assert_eq!(lists_argument(false, false), None);
        assert_eq!(helper_main(Some("bogus")), HELPER_USAGE);
        assert_eq!(helper_main(None), HELPER_USAGE);
    }

    #[test]
    fn helper_exit_codes_encode_which_list_failed() {
        use MemoryList::*;
        let lists = [Modified, Standby];
        let denied = 0xC000_0061_u32 as i32; // STATUS_PRIVILEGE_NOT_HELD
                                             // Success.
        let ok: PurgeResults = vec![(Modified, Ok(())), (Standby, Ok(()))];
        assert_eq!(helper_exit_code(&ok), 0);
        assert_eq!(decode_helper_exit(&lists, 0), ok);
        // The first list failed: the second never ran.
        let first: PurgeResults = vec![
            (Modified, Err(PurgeError::Status(denied))),
            (Standby, Err(PurgeError::NotRun)),
        ];
        let code = helper_exit_code(&first);
        assert_eq!(code, 0xC000_0061);
        assert_eq!(decode_helper_exit(&lists, code), first);
        // The second list failed after the first succeeded.
        let second: PurgeResults = vec![
            (Modified, Ok(())),
            (Standby, Err(PurgeError::Status(denied))),
        ];
        let code = helper_exit_code(&second);
        assert_eq!(code, 0xE000_0061);
        assert_eq!(decode_helper_exit(&lists, code), second);
        // The privilege could not be enabled.
        let privilege: PurgeResults = vec![(Standby, Err(PurgeError::Privilege("x".into())))];
        assert_eq!(helper_exit_code(&privilege), HELPER_PRIVILEGE);
        assert!(matches!(
            decode_helper_exit(&[Standby], HELPER_PRIVILEGE)[0].1,
            Err(PurgeError::Privilege(_))
        ));
        // Anything else is reported as-is, never as success.
        assert!(matches!(
            decode_helper_exit(&[Standby], 7)[0].1,
            Err(PurgeError::Unavailable(_))
        ));
        assert!(matches!(
            decode_helper_exit(&[Standby], HELPER_USAGE)[0].1,
            Err(PurgeError::Unavailable(_))
        ));
    }

    #[test]
    fn purge_without_privilege_fails_honestly() {
        // Not elevated (the usual test run): the privilege is not held, so
        // nothing is purged and every list says why. Elevated runs skip
        // this check rather than purging the machine's cache in a test.
        if crate::netetw::is_elevated() {
            return;
        }
        let results = purge_in_process(&[MemoryList::Modified, MemoryList::Standby]);
        assert_eq!(results.len(), 2);
        for (_, result) in &results {
            assert!(
                matches!(result, Err(PurgeError::Privilege(_))),
                "{result:?}"
            );
        }
        assert_eq!(helper_exit_code(&results), HELPER_PRIVILEGE);
        assert!(status_text(0xC000_0061_u32 as i32).contains("0xC0000061"));
    }

    /// Whether this process's token holds `name` enabled (None: not held).
    fn privilege_enabled(name: windows_sys::core::PCWSTR) -> Option<bool> {
        use windows_sys::Win32::Security::{GetTokenInformation, TokenPrivileges};
        let mut token = null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) },
            0
        );
        let token = Owned(token);
        let mut luid = LUID::default();
        assert_ne!(unsafe { LookupPrivilegeValueW(null(), name, &mut luid) }, 0);
        let mut length = 0;
        unsafe { GetTokenInformation(token.0, TokenPrivileges, null_mut(), 0, &mut length) };
        let mut buffer = vec![0_u64; (length as usize).div_ceil(8)];
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.0,
                    TokenPrivileges,
                    buffer.as_mut_ptr().cast(),
                    length,
                    &mut length,
                )
            },
            0
        );
        let header = buffer.as_ptr().cast::<TOKEN_PRIVILEGES>();
        let count = unsafe { (*header).PrivilegeCount } as usize;
        let entries = unsafe {
            std::slice::from_raw_parts(
                std::ptr::addr_of!((*header).Privileges).cast::<LUID_AND_ATTRIBUTES>(),
                count,
            )
        };
        entries
            .iter()
            .find(|entry| {
                entry.Luid.LowPart == luid.LowPart && entry.Luid.HighPart == luid.HighPart
            })
            .map(|entry| entry.Attributes & SE_PRIVILEGE_ENABLED != 0)
    }

    #[test]
    fn enabled_privilege_is_restored_when_dropped() {
        // A harmless privilege every interactive token holds disabled, so the
        // purge privilege is never touched by a test.
        use windows_sys::Win32::Security::SE_TIME_ZONE_NAME;
        let Some(before) = privilege_enabled(SE_TIME_ZONE_NAME) else {
            return;
        };
        let enabled = EnabledPrivilege::enable(SE_TIME_ZONE_NAME).unwrap();
        assert_eq!(privilege_enabled(SE_TIME_ZONE_NAME), Some(true));
        drop(enabled);
        assert_eq!(privilege_enabled(SE_TIME_ZONE_NAME), Some(before));
        // Enabling what is already on changes nothing and restores nothing.
        let outer = EnabledPrivilege::enable(SE_TIME_ZONE_NAME).unwrap();
        drop(EnabledPrivilege::enable(SE_TIME_ZONE_NAME).unwrap());
        assert_eq!(privilege_enabled(SE_TIME_ZONE_NAME), Some(true));
        drop(outer);
        assert_eq!(privilege_enabled(SE_TIME_ZONE_NAME), Some(before));
    }

    #[test]
    fn live_memory_state_and_zombie_scan_are_bounded() {
        let state = memory_state().unwrap();
        assert!(state.total > 0 && state.available <= state.total);
        let scan = scan_zombies().unwrap();
        assert!(scan.holders.iter().map(|h| h.zombies).max().unwrap_or(0) <= scan.total);
        assert!(scan.holders.iter().all(|h| h.zombies > 0 && h.created != 0));
        assert!(scan
            .holders
            .iter()
            .all(|h| h.pid != std::process::id() && h.pid > 4));
    }
}
