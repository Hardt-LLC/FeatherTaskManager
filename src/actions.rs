//! Explicit user actions; no process handles remain open between operations.

use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr::null};

use crate::{i18n::tr, process_tree::TerminationPlan};

use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE},
        SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW},
        Threading::{
            GetCurrentProcessId, GetPriorityClass, GetProcessInformation, GetProcessTimes,
            IsProcessCritical, OpenProcess, ProcessPowerThrottling, QueryFullProcessImageNameW,
            SetPriorityClass, SetProcessInformation, TerminateProcess, WaitForSingleObject,
            ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
            IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_ACCESS_RIGHTS,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_STATE, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SET_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            REALTIME_PRIORITY_CLASS,
        },
    },
    UI::{
        Shell::{
            ShellExecuteExW, ShellExecuteW, SEE_MASK_FLAG_NO_UI, SEE_MASK_INVOKEIDLIST,
            SEE_MASK_NOASYNC, SHELLEXECUTEINFOW,
        },
        WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

struct ProcessHandle(HANDLE);

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // Only successfully opened, owned handles enter this wrapper.
        unsafe { CloseHandle(self.0) };
    }
}

fn last_error(context: &str) -> String {
    format!("{context}: {}", std::io::Error::last_os_error())
}

fn filetime_value(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

fn creation_time(handle: HANDLE) -> Result<u64, String> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(last_error(tr(
            "프로세스의 시작 시간을 확인할 수 없습니다",
            "Cannot verify the process creation time",
        )));
    }
    Ok(filetime_value(created))
}

/// Resolve a live process identity for navigation. The liveness check and
/// creation time use one retained handle, and never wait for the process.
pub fn running_process_created(pid: u32) -> Result<u64, String> {
    let exited = || {
        tr(
            "프로세스가 실행 중이 아닙니다. 목록을 새로 고치세요.",
            "The process is not running. Refresh the list.",
        )
        .to_owned()
    };
    if pid == 0 {
        return Err(exited());
    }
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if raw.is_null() {
        return Err(last_error(tr(
            "프로세스를 열 수 없습니다",
            "Cannot open the process",
        )));
    }
    let handle = ProcessHandle(raw);
    match unsafe { WaitForSingleObject(handle.0, 0) } {
        WAIT_TIMEOUT => creation_time(handle.0),
        WAIT_OBJECT_0 => Err(exited()),
        _ => Err(last_error(tr(
            "프로세스 실행 상태를 확인할 수 없습니다",
            "Cannot verify whether the process is running",
        ))),
    }
}

fn open_checked(
    pid: u32,
    expected_created: u64,
    access: PROCESS_ACCESS_RIGHTS,
) -> Result<ProcessHandle, String> {
    // A missing sample identity must never act as a wildcard: PIDs are reused.
    if expected_created == 0 {
        return Err(tr(
            "프로세스의 시작 시간이 확인되지 않았습니다. 새로 고친 후 다시 시도하세요.",
            "The process creation time is unknown. Refresh the list and try again.",
        )
        .into());
    }
    let raw = unsafe { OpenProcess(access | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(last_error(tr(
            "프로세스를 열 수 없습니다",
            "Cannot open the process",
        )));
    }
    let handle = ProcessHandle(raw);
    if creation_time(handle.0)? != expected_created {
        return Err(tr(
            "선택한 프로세스가 이미 종료되고 PID가 재사용되었습니다. 목록을 새로 고치세요.",
            "The selected process exited and its PID was reused. Refresh the list.",
        )
        .into());
    }
    Ok(handle)
}

fn check_target_pid(pid: u32) -> Result<(), String> {
    if pid == 0 || pid == 4 {
        return Err(tr(
            "Windows 시스템 프로세스는 종료할 수 없습니다.",
            "Windows system processes cannot be terminated.",
        )
        .into());
    }
    if pid == unsafe { GetCurrentProcessId() } {
        return Err(tr(
            "이 작업 관리자는 종료 메뉴 대신 창의 닫기 버튼으로 종료하세요.",
            "Close this task manager using its window close button.",
        )
        .into());
    }
    Ok(())
}

fn check_noncritical(handle: &ProcessHandle) -> Result<(), String> {
    let mut critical = 0;
    if unsafe { IsProcessCritical(handle.0, &mut critical) } == 0 {
        return Err(last_error(tr(
            "시스템 필수 프로세스 여부를 확인할 수 없어 종료를 취소했습니다",
            "Termination was cancelled because the process critical status could not be verified",
        )));
    }
    if critical != 0 {
        return Err(tr(
            "Windows 작동에 필수적인 프로세스는 종료할 수 없습니다.",
            "Processes critical to Windows cannot be terminated.",
        )
        .into());
    }
    Ok(())
}

/// Deliberately excludes High and Realtime, which can starve the desktop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Priority {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
}

impl Priority {
    fn native(self) -> u32 {
        match self {
            Self::Idle => IDLE_PRIORITY_CLASS,
            Self::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
            Self::Normal => NORMAL_PRIORITY_CLASS,
            Self::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        }
    }

    fn from_native(value: u32) -> Option<Self> {
        match value {
            IDLE_PRIORITY_CLASS => Some(Self::Idle),
            BELOW_NORMAL_PRIORITY_CLASS => Some(Self::BelowNormal),
            NORMAL_PRIORITY_CLASS => Some(Self::Normal),
            ABOVE_NORMAL_PRIORITY_CLASS => Some(Self::AboveNormal),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        priority_label(self.native())
    }
}

fn priority_label(value: u32) -> &'static str {
    match value {
        IDLE_PRIORITY_CLASS => tr("낮음", "Idle"),
        BELOW_NORMAL_PRIORITY_CLASS => tr("낮은 우선순위", "Below normal"),
        NORMAL_PRIORITY_CLASS => tr("보통", "Normal"),
        ABOVE_NORMAL_PRIORITY_CLASS => tr("높은 우선순위", "Above normal"),
        HIGH_PRIORITY_CLASS => tr("높음", "High"),
        REALTIME_PRIORITY_CLASS => tr("실시간", "Realtime"),
        _ => tr("확인 불가", "Unavailable"),
    }
}

#[derive(Clone, Debug)]
pub struct ProcessSettings {
    pub priority: Option<Priority>,
    pub priority_label: &'static str,
    /// None means the OS or process does not expose power-throttling information.
    pub efficiency: Option<bool>,
}

fn power_state(handle: HANDLE) -> Result<PROCESS_POWER_THROTTLING_STATE, String> {
    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    if unsafe {
        GetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (&mut state as *mut PROCESS_POWER_THROTTLING_STATE).cast(),
            std::mem::size_of_val(&state) as u32,
        )
    } == 0
    {
        return Err(last_error(tr(
            "이 Windows 버전 또는 프로세스에서 효율성 모드를 확인할 수 없습니다",
            "Efficiency mode is unavailable for this Windows version or process",
        )));
    }
    Ok(state)
}

fn write_power_state(handle: HANDLE, state: &PROCESS_POWER_THROTTLING_STATE) -> bool {
    unsafe {
        SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            (state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            std::mem::size_of_val(state) as u32,
        ) != 0
    }
}

/// Queries only the selected process; callers should not run this per table row.
pub fn process_settings(pid: u32, expected_created: u64) -> Result<ProcessSettings, String> {
    let handle = open_checked(pid, expected_created, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let priority = unsafe { GetPriorityClass(handle.0) };
    if priority == 0 {
        return Err(last_error(tr(
            "프로세스 우선순위를 읽을 수 없습니다",
            "Cannot read the process priority",
        )));
    }
    Ok(ProcessSettings {
        priority: Priority::from_native(priority),
        priority_label: priority_label(priority),
        efficiency: power_state(handle.0).ok().map(|state| {
            state.ControlMask & state.StateMask & PROCESS_POWER_THROTTLING_EXECUTION_SPEED != 0
        }),
    })
}

fn open_mutable(pid: u32, expected_created: u64) -> Result<ProcessHandle, String> {
    if pid == 0 || pid == 4 || pid == unsafe { GetCurrentProcessId() } {
        return Err(tr(
            "Windows 시스템 프로세스와 Feather Task Manager의 실행 설정은 변경할 수 없습니다.",
            "Execution settings of Windows system processes and Feather Task Manager cannot be changed.",
        ).into());
    }
    let handle = open_checked(pid, expected_created, PROCESS_SET_INFORMATION)?;
    let mut critical = 0;
    if unsafe { IsProcessCritical(handle.0, &mut critical) } == 0 {
        return Err(last_error(tr(
            "시스템 필수 프로세스 여부를 확인할 수 없어 변경을 취소했습니다",
            "The change was cancelled because the process critical status could not be verified",
        )));
    }
    if critical != 0 {
        return Err(tr(
            "Windows 작동에 필수적인 프로세스의 실행 설정은 변경할 수 없습니다.",
            "Execution settings of processes critical to Windows cannot be changed.",
        )
        .into());
    }
    Ok(handle)
}

pub fn set_priority(pid: u32, expected_created: u64, priority: Priority) -> Result<(), String> {
    let handle = open_mutable(pid, expected_created)?;
    if unsafe { SetPriorityClass(handle.0, priority.native()) } == 0 {
        return Err(last_error(tr(
            "프로세스 우선순위를 변경할 수 없습니다",
            "Cannot change the process priority",
        )));
    }
    Ok(())
}

fn efficiency_power_state(
    original: &PROCESS_POWER_THROTTLING_STATE,
    enabled: bool,
) -> PROCESS_POWER_THROTTLING_STATE {
    let flag = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
    PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: if enabled {
            original.ControlMask | flag
        } else {
            original.ControlMask & !flag
        },
        StateMask: if enabled {
            original.StateMask | flag
        } else {
            original.StateMask & !flag
        },
    }
}

fn efficiency_priority(original: u32, enabled: bool) -> u32 {
    if enabled && original != IDLE_PRIORITY_CLASS {
        BELOW_NORMAL_PRIORITY_CLASS
    } else if !enabled && original == BELOW_NORMAL_PRIORITY_CLASS {
        NORMAL_PRIORITY_CLASS
    } else {
        original
    }
}

/// EcoQoS plus BelowNormal priority (Idle stays Idle). Disabling returns a
/// BelowNormal process to Normal and returns execution-speed QoS to Windows
/// management; other priority classes remain unchanged. No old process
/// identities or handles are cached. The UI should explain this before enabling.
pub fn set_efficiency(pid: u32, expected_created: u64, enabled: bool) -> Result<(), String> {
    let handle = open_mutable(pid, expected_created)?;
    let original = power_state(handle.0)?;
    let old_priority = unsafe { GetPriorityClass(handle.0) };
    if old_priority == 0 {
        return Err(last_error(tr(
            "우선순위를 확인할 수 없습니다",
            "Cannot verify the priority",
        )));
    }
    let state = efficiency_power_state(&original, enabled);
    if !write_power_state(handle.0, &state) {
        return Err(last_error(tr(
            "효율성 모드를 변경할 수 없습니다",
            "Cannot change efficiency mode",
        )));
    }
    let priority = efficiency_priority(old_priority, enabled);
    if priority != old_priority && unsafe { SetPriorityClass(handle.0, priority) } == 0 {
        let failure = last_error(tr(
            "우선순위를 변경할 수 없습니다",
            "Cannot change the priority",
        ));
        if !write_power_state(handle.0, &original) {
            return Err(format!("{failure}\n{}", tr(
                "전원 설정을 되돌리지 못했습니다. 효율성 설정이 일부만 변경되었을 수 있습니다. 새로 고쳐 확인하세요.",
                "Power settings could not be restored. Efficiency settings may be partially changed. Refresh to verify."
            )));
        }
        return Err(failure);
    }
    Ok(())
}

/// Forcefully end the selected process after the caller has obtained confirmation.
/// All checks and the termination use one handle, so PID reuse cannot change the target.
pub fn terminate(pid: u32, expected_created: u64) -> Result<(), String> {
    check_target_pid(pid)?;
    let handle = open_checked(pid, expected_created, PROCESS_TERMINATE)?;
    check_noncritical(&handle)?;
    if unsafe { TerminateProcess(handle.0, 1) } == 0 {
        return Err(last_error(tr(
            "프로세스를 종료할 수 없습니다",
            "Cannot terminate the process",
        )));
    }
    // TerminateProcess is asynchronous; refreshing the snapshot shows actual exit.
    Ok(())
}

/// Terminate only the identities in the confirmed snapshot, children first.
/// Preflight the entire plan before changing anything and retain every checked
/// handle: PID reuse can never redirect an already-confirmed termination.
/// A process can still exit or change critical status after preflight, so each
/// termination is checked again and partial outcomes are reported explicitly.
pub fn terminate_tree(plan: &TerminationPlan) -> Result<String, String> {
    if plan.is_empty() || plan.targets().last() != Some(&plan.root) {
        return Err(tr(
            "프로세스 트리 대상이 올바르지 않습니다.",
            "The process tree target is invalid.",
        )
        .into());
    }
    let cancelled = |pid: u32, error: String| {
        format!(
            "{} (PID {pid}): {error}",
            tr(
                "트리 종료 사전 확인에 실패했습니다. 어떤 프로세스에도 종료를 요청하지 않았습니다",
                "Tree preflight failed. No process was asked to terminate"
            )
        )
    };
    // Refuse protected identifiers anywhere in the tree before opening handles.
    for target in plan.targets() {
        check_target_pid(target.pid).map_err(|error| cancelled(target.pid, error))?;
    }
    let mut handles = Vec::with_capacity(plan.len());
    for target in plan.targets() {
        let handle = open_checked(
            target.pid,
            target.created,
            PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
        )
        .map_err(|error| cancelled(target.pid, error))?;
        check_noncritical(&handle).map_err(|error| cancelled(target.pid, error))?;
        handles.push(handle);
    }

    let mut requested = 0;
    let mut exited = 0;
    let mut failures = Vec::new();
    for (target, handle) in plan.targets().iter().zip(&handles) {
        if unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_OBJECT_0 {
            exited += 1;
            continue;
        }
        if let Err(error) = check_noncritical(handle) {
            failures.push(format!("PID {}: {error}", target.pid));
            continue;
        }
        if unsafe { TerminateProcess(handle.0, 1) } != 0 {
            requested += 1;
        } else {
            let error = last_error(tr(
                "프로세스를 종료할 수 없습니다",
                "Cannot terminate the process",
            ));
            // Another actor may have ended the process between the check and
            // TerminateProcess. Its held handle still identifies that process.
            if unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_OBJECT_0 {
                exited += 1;
            } else {
                failures.push(format!("PID {}: {error}", target.pid));
            }
        }
    }
    let summary = format!(
        "{}: {requested}/{} · {}: {exited} · {}: {}",
        tr("종료 요청", "Termination requested"),
        plan.len(),
        tr("이미 종료됨", "Already exited"),
        tr("실패", "Failed"),
        failures.len()
    );
    if failures.is_empty() {
        Ok(summary)
    } else {
        let details = failures
            .iter()
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        Err(format!("{summary}\n\n{details}"))
    }
}

/// Query only the requested process, with enough space for Windows extended paths.
pub fn executable_path(pid: u32, expected_created: u64) -> Result<String, String> {
    let handle = open_checked(pid, expected_created, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle.0, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
        return Err(last_error(tr(
            "실행 파일의 경로를 확인할 수 없습니다",
            "Cannot read the executable path",
        )));
    }
    String::from_utf16(&buffer[..length as usize]).map_err(|_| {
        tr(
            "실행 파일 경로에 표시할 수 없는 유니코드 문자가 있습니다.",
            "The executable path contains invalid Unicode.",
        )
        .into()
    })
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn shell_execute(verb: &str, file: &OsStr, parameters: Option<&OsStr>) -> Result<(), String> {
    let verb = wide(OsStr::new(verb));
    let file = wide(file);
    let parameters = parameters.map(wide);
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            parameters.as_ref().map_or(null(), |value| value.as_ptr()),
            null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result > 32 {
        Ok(())
    } else {
        // ShellExecute returns its own error code; GetLastError is not guaranteed.
        let detail = match result {
            0 | 8 => tr("메모리가 부족합니다", "Not enough memory"),
            2 => tr("실행 파일을 찾을 수 없습니다", "Executable not found"),
            3 => tr("경로를 찾을 수 없습니다", "Path not found"),
            5 => tr(
                "접근이 거부되었거나 관리자 권한 요청이 취소되었습니다",
                "Access was denied or the administrator request was cancelled",
            ),
            26 => tr("파일이 사용 중입니다", "The file is in use"),
            27 | 31 => tr(
                "파일 형식의 연결 프로그램을 찾을 수 없습니다",
                "No application is associated with this file type",
            ),
            28..=30 => tr(
                "Windows 셸과 통신할 수 없습니다",
                "Cannot communicate with the Windows shell",
            ),
            32 => tr(
                "필요한 DLL을 찾을 수 없습니다",
                "A required DLL was not found",
            ),
            _ => tr(
                "Windows 셸에서 실행하지 못했습니다",
                "The Windows shell could not launch the application",
            ),
        };
        Err(format!(
            "{detail} ({} {result}).",
            tr("셸 오류", "shell error")
        ))
    }
}

pub fn reveal_executable(pid: u32, expected_created: u64) -> Result<(), String> {
    let path = executable_path(pid, expected_created)?;
    // Never resolve explorer.exe through the current directory or an untrusted PATH.
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 {
        return Err(last_error(tr(
            "Windows 폴더를 찾을 수 없습니다",
            "Cannot locate the Windows folder",
        )));
    }
    if length as usize >= buffer.len() {
        return Err(tr(
            "Windows 폴더 경로가 너무 깁니다.",
            "The Windows folder path is too long.",
        )
        .into());
    }
    use std::os::windows::ffi::OsStringExt;
    let mut explorer =
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
    explorer.push("explorer.exe");
    // Quotes are not permitted in Windows filenames; this keeps spaces/commas intact.
    let parameters = format!("/select,\"{path}\"");
    shell_execute("open", explorer.as_os_str(), Some(OsStr::new(&parameters)))
}

/// Show the checked executable's native file properties on the action worker.
/// The verb is fixed and no executable arguments or process waits are used.
pub fn show_properties(pid: u32, expected_created: u64) -> Result<(), String> {
    let path = executable_path(pid, expected_created)?;
    let verb = wide(OsStr::new("properties"));
    let file = wide(OsStr::new(&path));

    // Shell extensions may require an STA. Balance S_OK and S_FALSE alike;
    // a failed initialization (including an incompatible apartment) owns none.
    let initialized = unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if initialized < 0 {
        return Err(format!(
            "{} (HRESULT 0x{:08X})",
            tr(
                "파일 속성을 위한 Windows 셸을 초기화할 수 없습니다",
                "Cannot initialize the Windows shell for file properties"
            ),
            initialized as u32
        ));
    }
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _apartment = Apartment;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // INVOKEIDLIST enables the properties context-menu verb; NOASYNC is
        // required because the job worker has no Windows message loop.
        fMask: SEE_MASK_INVOKEIDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(last_error(tr(
            "실행 파일의 속성을 열 수 없습니다",
            "Cannot open the executable's properties",
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SystemTool {
    ResourceMonitor,
    Services,
}

fn system_tool_paths(tool: SystemTool) -> Result<(std::path::PathBuf, Option<String>), String> {
    use std::os::windows::ffi::OsStringExt;
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 || length as usize >= buffer.len() {
        return Err(last_error(tr(
            "Windows 시스템 폴더를 찾을 수 없습니다",
            "Cannot locate the Windows system directory",
        )));
    }
    let directory =
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
    match tool {
        SystemTool::ResourceMonitor => Ok((directory.join("resmon.exe"), None)),
        // Resolve both MMC and its snap-in absolutely; never dispatch an .msc
        // association or search for either executable through the working folder.
        SystemTool::Services => Ok((
            directory.join("mmc.exe"),
            Some(format!("\"{}\"", directory.join("services.msc").display())),
        )),
    }
}

pub fn launch_system_tool(tool: SystemTool) -> Result<(), String> {
    let (path, parameters) = system_tool_paths(tool)?;
    shell_execute(
        "open",
        path.as_os_str(),
        parameters.as_deref().map(OsStr::new),
    )
}

fn task_executable(path: &str) -> Result<std::path::PathBuf, String> {
    use std::path::{Component, Path, Prefix};
    let invalid = || {
        tr(
            "로컬 드라이브의 실행 파일(.exe 또는 .com)을 선택하세요.",
            "Select an executable (.exe or .com) on a local drive.",
        )
        .to_owned()
    };
    if path.contains('\0') {
        return Err(invalid());
    }
    let candidate = Path::new(path);
    let local_path = |value: &Path| {
        value.is_absolute()
            && matches!(
                value.components().next(),
                Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
            )
    };
    if !local_path(candidate) {
        return Err(invalid());
    }
    let resolved = candidate.canonicalize().map_err(|_| invalid())?;
    // Canonicalization prevents a local-looking junction from selecting a UNC
    // executable; arguments and elevation are handled only after this check.
    if !local_path(&resolved) || !resolved.is_file() || resolved.components().any(|component| {
        matches!(component, Component::Normal(name) if name.encode_wide().any(|unit| unit == b':' as u16))
    }) {
        return Err(invalid());
    }
    let extension = resolved
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or_default();
    if !extension.eq_ignore_ascii_case("exe") && !extension.eq_ignore_ascii_case("com") {
        return Err(invalid());
    }
    let Some(Component::Prefix(prefix)) = resolved.components().next() else {
        return Err(invalid());
    };
    let drive = match prefix.kind() {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
        _ => return Err(invalid()),
    };
    // A mapped network drive also has a drive letter; reject it explicitly.
    let root = [u16::from(drive), b':' as u16, b'\\' as u16, 0];
    let drive_type =
        unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) };
    if !matches!(drive_type, 2 | 3 | 5 | 6) {
        return Err(invalid());
    }
    Ok(resolved)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskLaunch {
    pub command: String,
    pub arguments: String,
    pub elevated: bool,
}

/// Separate the program from its arguments without invoking a command shell.
/// A path containing spaces must be quoted (Browse supplies those quotes).
pub fn task_command(task: &TaskLaunch) -> Result<(String, String), String> {
    let invalid = || {
        tr(
            "프로그램 이름을 입력하고 공백이 있는 경로는 큰따옴표로 묶으세요.",
            "Enter a program name; enclose paths containing spaces in double quotes.",
        )
        .to_owned()
    };
    if task
        .command
        .chars()
        .chain(task.arguments.chars())
        .any(|c| matches!(c, '\0' | '\r' | '\n'))
        || task.command.encode_utf16().count() + task.arguments.encode_utf16().count() > 32760
    {
        return Err(invalid());
    }
    let command = task.command.trim();
    let (file, tail) = if let Some(quoted) = command.strip_prefix('"') {
        let end = quoted.find('"').ok_or_else(invalid)?;
        let tail = &quoted[end + 1..];
        if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
            return Err(invalid());
        }
        (&quoted[..end], tail.trim_start())
    } else {
        let end = command.find(char::is_whitespace).unwrap_or(command.len());
        (&command[..end], command[end..].trim_start())
    };
    if file.is_empty() || file.contains('"') {
        return Err(invalid());
    }
    let arguments = match (tail.is_empty(), task.arguments.is_empty()) {
        (true, _) => task.arguments.clone(),
        (_, true) => tail.to_owned(),
        _ => format!("{tail} {}", task.arguments),
    };
    Ok((file.into(), arguments))
}

fn resolve_task_executable(file: &str) -> Result<std::path::PathBuf, String> {
    use std::path::Path;
    if file.contains(['\\', '/', ':']) {
        return task_executable(file);
    }
    let extension = Path::new(file).extension().and_then(OsStr::to_str);
    if extension
        .is_some_and(|ext| !ext.eq_ignore_ascii_case("exe") && !ext.eq_ignore_ascii_case("com"))
    {
        return Err(tr(
            "실행 파일(.exe 또는 .com)을 입력하세요.",
            "Enter an executable (.exe or .com).",
        )
        .into());
    }
    let name = if extension.is_none() {
        format!("{file}.exe")
    } else {
        file.to_owned()
    };
    // Explicit search order: Windows tools first, then absolute local PATH entries.
    // Never include the current directory, an implicit interpreter, or file associations.
    let (system_tool, _) = system_tool_paths(SystemTool::ResourceMonitor)?;
    let mut directories = vec![
        system_tool.parent().unwrap().to_path_buf(),
        windows_explorer()?.parent().unwrap().to_path_buf(),
    ];
    if let Some(path) = std::env::var_os("PATH") {
        directories.extend(std::env::split_paths(&path).filter(|path| path.is_absolute()));
    }
    for directory in directories {
        if let Some(path) = directory.join(&name).to_str() {
            if let Ok(executable) = task_executable(path) {
                return Ok(executable);
            }
        }
    }
    Err(tr(
        "프로그램을 찾을 수 없습니다. 찾아보기에서 실행 파일을 선택하세요.",
        "Program not found. Use Browse to choose its executable.",
    )
    .into())
}

/// Programs and arguments remain separate; only an explicit admin choice uses runas.
pub fn launch_task(task: &TaskLaunch, owner: usize) -> Result<(), String> {
    let (file, arguments) = task_command(task)?;
    let executable = resolve_task_executable(&file)?;
    let initialized = unsafe {
        CoInitializeEx(
            null(),
            (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
        )
    };
    if initialized < 0 {
        return Err(tr(
            "Windows 셸을 초기화할 수 없습니다.",
            "Cannot initialize the Windows shell.",
        )
        .into());
    }
    let file = wide(executable.as_os_str());
    let parameters = wide(OsStr::new(&arguments));
    let verb = wide(OsStr::new(if task.elevated { "runas" } else { "open" }));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        hwnd: owner as _,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    let result = if unsafe { ShellExecuteExW(&mut info) } == 0 {
        Err(last_error(tr(
            "새 작업을 실행할 수 없습니다",
            "Cannot start the new task",
        )))
    } else {
        Ok(())
    };
    unsafe { CoUninitialize() };
    result
}

fn windows_explorer() -> Result<std::path::PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(last_error(tr(
            "Windows 폴더를 찾을 수 없습니다",
            "Cannot locate the Windows directory",
        )));
    }
    Ok(
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]))
            .join("explorer.exe"),
    )
}

/// Read-only identity check, also used to decide whether to offer Restart Explorer.
pub fn is_shell_process(pid: u32, expected_created: u64) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
    let shell = unsafe { GetShellWindow() };
    let mut shell_pid = 0;
    if shell.is_null() {
        return false;
    }
    unsafe { GetWindowThreadProcessId(shell, &mut shell_pid) };
    shell_pid == pid && running_process_created(pid).ok() == Some(expected_created)
}

fn checked_shell(pid: u32, expected_created: u64) -> Result<ProcessHandle, String> {
    use windows_sys::Win32::{
        Security::{
            EqualSid, GetTokenInformation, TokenElevation, TokenUser, TOKEN_ELEVATION, TOKEN_QUERY,
            TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let invalid = || {
        tr(
            "현재 사용자의 Windows 탐색기를 안전하게 확인할 수 없습니다. 목록을 새로 고치세요.",
            "Cannot safely verify the current user's Windows Explorer. Refresh the list.",
        )
        .to_owned()
    };
    if !is_shell_process(pid, expected_created) {
        return Err(invalid());
    }
    let process = open_checked(pid, expected_created, PROCESS_SYNCHRONIZE)?;
    let actual = std::path::PathBuf::from(executable_path(pid, expected_created)?)
        .canonicalize()
        .map_err(|_| invalid())?;
    let expected = windows_explorer()?.canonicalize().map_err(|_| invalid())?;
    if !actual
        .as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy())
    {
        return Err(invalid());
    }
    let token = |process| -> Result<ProcessHandle, String> {
        let mut handle = std::ptr::null_mut();
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut handle) } == 0 {
            return Err(invalid());
        }
        Ok(ProcessHandle(handle))
    };
    let shell_token = token(process.0)?;
    let our_token = token(unsafe { GetCurrentProcess() })?;
    // Aligned token buffers retain their SIDs until comparison completes.
    let user = |token: HANDLE| -> Result<Vec<usize>, String> {
        let mut needed = 0;
        unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
        if needed == 0 || needed > 65536 {
            return Err(invalid());
        }
        let mut data = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                data.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        } == 0
        {
            return Err(invalid());
        }
        Ok(data)
    };
    let shell_user = user(shell_token.0)?;
    let our_user = user(our_token.0)?;
    let same_user = unsafe {
        EqualSid(
            (*(shell_user.as_ptr().cast::<TOKEN_USER>())).User.Sid,
            (*(our_user.as_ptr().cast::<TOKEN_USER>())).User.Sid,
        )
    } != 0;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut needed = 0;
    if !same_user
        || unsafe {
            GetTokenInformation(
                shell_token.0,
                TokenElevation,
                (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut needed,
            )
        } == 0
        || elevation.TokenIsElevated != 0
    {
        return Err(invalid());
    }
    Ok(process)
}

/// Restart Manager preserves Explorer's restart registration and normal user token.
/// No forced termination or fallback to launching an elevated desktop shell.
pub fn restart_explorer(pid: u32, expected_created: u64) -> Result<(), String> {
    use windows_sys::Win32::System::RestartManager::*;
    let _process = checked_shell(pid, expected_created)?;
    let error = |code| {
        format!(
            "{}: {}",
            tr(
                "Windows 탐색기를 다시 시작할 수 없습니다",
                "Cannot restart Windows Explorer"
            ),
            std::io::Error::from_raw_os_error(code as i32)
        )
    };
    let mut session = 0;
    let mut key = [0u16; CCH_RM_SESSION_KEY as usize + 1];
    let status = unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) };
    if status != 0 {
        return Err(error(status));
    }
    struct Session(u32);
    impl Drop for Session {
        fn drop(&mut self) {
            unsafe { RmEndSession(self.0) };
        }
    }
    let session = Session(session);
    let process = RM_UNIQUE_PROCESS {
        dwProcessId: pid,
        ProcessStartTime: FILETIME {
            dwLowDateTime: expected_created as u32,
            dwHighDateTime: (expected_created >> 32) as u32,
        },
    };
    let status = unsafe { RmRegisterResources(session.0, 0, null(), 1, &process, 0, null()) };
    if status != 0 {
        return Err(error(status));
    }
    let mut item = RM_PROCESS_INFO::default();
    let (mut needed, mut count, mut reasons) = (0, 1, 0);
    let status = unsafe { RmGetList(session.0, &mut needed, &mut count, &mut item, &mut reasons) };
    if status != 0 {
        return Err(error(status));
    }
    if count != 1
        || reasons != 0
        || item.bRestartable == 0
        || item.Process.dwProcessId != pid
        || filetime_value(item.Process.ProcessStartTime) != expected_created
        || item.ApplicationType != RmExplorer
        || !is_shell_process(pid, expected_created)
    {
        return Err(tr(
            "Windows에서 탐색기의 안전한 재시작을 지원하지 않아 취소했습니다.",
            "Cancelled because Windows cannot safely restart this Explorer process.",
        )
        .into());
    }
    let shutdown = unsafe { RmShutdown(session.0, RmShutdownOnlyRegistered as u32, None) };
    // Required even after a partial shutdown failure; restore anything RM stopped.
    let restart = unsafe { RmRestart(session.0, 0, None) };
    if restart != 0 {
        return Err(error(restart));
    }
    if shutdown != 0 {
        return Err(error(shutdown));
    }
    Ok(())
}

fn app_window(pid: u32) -> Option<windows_sys::Win32::Foundation::HWND> {
    use windows_sys::{
        core::BOOL,
        Win32::{
            Foundation::{HWND, LPARAM},
            UI::WindowsAndMessaging::*,
        },
    };
    struct Search {
        pid: u32,
        window: HWND,
    }
    unsafe extern "system" fn visit(hwnd: HWND, data: LPARAM) -> BOOL {
        let search = &mut *(data as *mut Search);
        let mut pid = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == search.pid
            && IsWindowVisible(hwnd) != 0
            && GetWindow(hwnd, GW_OWNER).is_null()
            && GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW == 0
            && hwnd != GetShellWindow()
        {
            let mut cloaked = 0u32;
            windows_sys::Win32::Graphics::Dwm::DwmGetWindowAttribute(
                hwnd,
                windows_sys::Win32::Graphics::Dwm::DWMWA_CLOAKED as u32,
                (&mut cloaked as *mut u32).cast(),
                std::mem::size_of::<u32>() as u32,
            );
            if cloaked != 0 {
                return 1;
            }
            search.window = hwnd;
            return 0;
        }
        1
    }
    let mut search = Search {
        pid,
        window: std::ptr::null_mut(),
    };
    unsafe { EnumWindows(Some(visit), (&mut search as *mut Search) as isize) };
    (!search.window.is_null()).then_some(search.window)
}

pub fn has_app_window(pid: u32) -> bool {
    app_window(pid).is_some()
}

/// Call on the UI thread in direct response to user input. Windows remains in
/// control of foreground eligibility; never attach input queues or bypass locks.
pub fn switch_to_window(pid: u32, expected_created: u64) -> Result<(), String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    let _process = open_checked(pid, expected_created, PROCESS_SYNCHRONIZE)?;
    let window = app_window(pid).ok_or_else(|| {
        tr(
            "전환할 앱 창이 없습니다.",
            "There is no application window to switch to.",
        )
        .to_owned()
    })?;
    let mut actual_pid = 0;
    unsafe { GetWindowThreadProcessId(window, &mut actual_pid) };
    if actual_pid != pid || running_process_created(pid)? != expected_created {
        return Err(tr(
            "앱 창이 변경되었습니다. 다시 시도하세요.",
            "The application window changed. Try again.",
        )
        .into());
    }
    unsafe {
        if IsIconic(window) != 0 {
            ShowWindowAsync(window, SW_RESTORE);
        }
        if SetForegroundWindow(window) == 0 {
            return Err(tr("Windows가 창의 포커스 전환을 허용하지 않았습니다. 작업 표시줄에서 앱을 선택하세요.", "Windows did not allow the focus change. Select the app on the taskbar.").into());
        }
    }
    Ok(())
}

/// Private marker on every elevated relaunch of the UI (this one-shot
/// command and "Always run as administrator"): the instance it starts never
/// relaunches itself again, and says so when Windows started it unelevated
/// (a standard user with UAC turned off), instead of repeating the launch.
pub const ELEVATED_RELAUNCH: &str = "--elevated-relaunch";

/// Request elevation through the standard UAC flow; the caller may close on success.
pub fn relaunch_elevated() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| {
        format!(
            "{}: {error}",
            tr(
                "현재 실행 파일의 경로를 확인할 수 없습니다",
                "Cannot locate this executable"
            )
        )
    })?;
    let parameters = format!(
        "--language {} {ELEVATED_RELAUNCH}",
        crate::i18n::language().code()
    );
    shell_execute(
        "runas",
        executable.as_os_str(),
        Some(OsStr::new(&parameters)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{process_tree::Tree, sampler::Sampler};
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    #[test]
    fn system_and_self_termination_are_refused() {
        assert!(terminate(0, 1).is_err());
        assert!(terminate(4, 1).is_err());
        assert!(terminate(unsafe { GetCurrentProcessId() }, 1).is_err());
    }

    #[test]
    fn current_process_path_requires_the_sampled_identity() {
        let pid = unsafe { GetCurrentProcessId() };
        let created = creation_time(unsafe { GetCurrentProcess() }).unwrap();
        assert_ne!(created, 0);
        assert_eq!(running_process_created(pid).unwrap(), created);
        assert!(running_process_created(0).is_err());
        assert!(executable_path(pid, 0).is_err());
        assert!(executable_path(pid, created + 1).is_err());
        // Invalid identities fail before COM or a native properties UI opens.
        assert!(show_properties(pid, 0).is_err());
        assert!(show_properties(pid, created + 1).is_err());
        let actual = executable_path(pid, created).unwrap();
        assert_eq!(
            std::path::Path::new(&actual),
            std::env::current_exe().unwrap().as_path()
        );
    }

    #[test]
    fn settings_queries_are_read_only_and_require_identity() {
        let pid = unsafe { GetCurrentProcessId() };
        let created = creation_time(unsafe { GetCurrentProcess() }).unwrap();
        assert!(process_settings(pid, 0).is_err());
        assert!(process_settings(pid, created + 1).is_err());
        let settings = process_settings(pid, created).unwrap();
        assert!(!settings.priority_label.is_empty());
        for protected in [0, 4, pid] {
            assert!(set_priority(protected, created, Priority::Normal).is_err());
            assert!(set_efficiency(protected, created, true).is_err());
            assert!(set_efficiency(protected, created, false).is_err());
        }
    }

    #[test]
    fn stale_identity_cannot_change_disposable_child_settings() {
        let child = ChildCleanup(fixture_command("leaf").spawn().unwrap());
        let handle = ProcessHandle(unsafe {
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, child.0.id())
        });
        assert!(!handle.0.is_null());
        let created = creation_time(handle.0).unwrap();
        let original = process_settings(child.0.id(), created).unwrap();
        assert!(set_priority(child.0.id(), created + 1, Priority::BelowNormal).is_err());
        assert!(set_efficiency(child.0.id(), created + 1, true).is_err());
        let current = process_settings(child.0.id(), created).unwrap();
        assert_eq!(current.priority, original.priority);
        assert_eq!(current.efficiency, original.efficiency);
    }

    #[test]
    fn efficiency_only_changes_execution_speed_flag_and_safe_priorities() {
        let original = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: 4,
            StateMask: 4,
        };
        let enabled = efficiency_power_state(&original, true);
        assert_eq!(enabled.ControlMask, 5);
        assert_eq!(enabled.StateMask, 5);
        let disabled = efficiency_power_state(&enabled, false);
        assert_eq!(disabled.ControlMask, 4);
        assert_eq!(disabled.StateMask, 4);
        assert_eq!(
            efficiency_priority(IDLE_PRIORITY_CLASS, true),
            IDLE_PRIORITY_CLASS
        );
        assert_eq!(
            efficiency_priority(NORMAL_PRIORITY_CLASS, true),
            BELOW_NORMAL_PRIORITY_CLASS
        );
        assert_eq!(
            efficiency_priority(BELOW_NORMAL_PRIORITY_CLASS, false),
            NORMAL_PRIORITY_CLASS
        );
        assert_eq!(
            efficiency_priority(ABOVE_NORMAL_PRIORITY_CLASS, false),
            ABOVE_NORMAL_PRIORITY_CLASS
        );
        assert_eq!(Priority::from_native(HIGH_PRIORITY_CLASS), None);
        assert_eq!(Priority::from_native(REALTIME_PRIORITY_CLASS), None);
    }

    #[test]
    fn system_tool_launches_use_absolute_system_paths() {
        let (resource, parameters) = system_tool_paths(SystemTool::ResourceMonitor).unwrap();
        assert!(resource.is_absolute());
        assert_eq!(resource.file_name().unwrap(), "resmon.exe");
        assert!(parameters.is_none());
        let (mmc, parameters) = system_tool_paths(SystemTool::Services).unwrap();
        assert!(mmc.is_absolute());
        assert_eq!(mmc.file_name().unwrap(), "mmc.exe");
        assert_eq!(
            parameters.unwrap(),
            format!(
                "\"{}\"",
                mmc.parent().unwrap().join("services.msc").display()
            )
        );
    }

    #[test]
    fn new_task_validation_never_dispatches_strings_as_commands() {
        for input in [
            "cmd.exe",
            "C:cmd.exe",
            "\\\\server\\share\\app.exe",
            "https://example.com/app.exe",
            "C:\\Windows\\notepad.exe\0x",
            "\\\\.\\PIPE\\app.exe",
        ] {
            assert!(task_executable(input).is_err(), "accepted {input:?}");
        }
        let current = std::env::current_exe().unwrap();
        assert_eq!(
            task_executable(current.to_str().unwrap()).unwrap(),
            current.canonicalize().unwrap()
        );
        let (resource, _) = system_tool_paths(SystemTool::ResourceMonitor).unwrap();
        assert!(
            task_executable(resource.with_file_name("services.msc").to_str().unwrap()).is_err()
        );
    }

    #[test]
    fn task_commands_preserve_arguments_and_require_explicit_programs() {
        let request = |command: &str, arguments: &str| TaskLaunch {
            command: command.into(),
            arguments: arguments.into(),
            elevated: false,
        };
        assert_eq!(
            task_command(&request(
                r#""C:\Program Files\App\app.exe" --one"#,
                r#""two words" &literal"#
            ))
            .unwrap(),
            (
                r#"C:\Program Files\App\app.exe"#.into(),
                r#"--one "two words" &literal"#.into()
            )
        );
        assert_eq!(
            task_command(&request("cmd.exe /c echo hello", "")).unwrap(),
            ("cmd.exe".into(), "/c echo hello".into())
        );
        for invalid in [
            "",
            "\"\"",
            "\"unterminated",
            "\"cmd.exe\"suffix",
            "cmd.exe\n/c whoami",
            "cmd.exe\0",
        ] {
            assert!(
                task_command(&request(invalid, "")).is_err(),
                "accepted {invalid:?}"
            );
        }
        for invalid in [
            "script.bat",
            "script.ps1",
            "services.msc",
            "https://example.com",
            "C:cmd.exe",
            "\\\\server\\app.exe",
        ] {
            assert!(
                resolve_task_executable(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert_eq!(
            resolve_task_executable("cmd").unwrap().file_name().unwrap(),
            "cmd.exe"
        );
        assert!(task_command(&request("cmd.exe", "\0")).is_err());
    }

    #[test]
    fn shell_targeting_and_window_switch_reject_stale_identity() {
        // Read-only targeting: never call restart_explorer on the real shell.
        let pid = unsafe { GetCurrentProcessId() };
        assert!(!is_shell_process(pid, 0));
        assert!(checked_shell(pid, 0).is_err());
        assert!(switch_to_window(pid, 0).is_err());
        let shell = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetShellWindow() };
        if !shell.is_null() {
            let mut shell_pid = 0;
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                    shell,
                    &mut shell_pid,
                )
            };
            if let Ok(created) = running_process_created(shell_pid) {
                assert!(is_shell_process(shell_pid, created));
                assert!(!is_shell_process(shell_pid, created.saturating_add(1)));
                assert!(checked_shell(shell_pid, created.saturating_add(1)).is_err());
            }
        }
    }

    /// This ignored fixture runs only in a disposable child test-harness process.
    /// No ordinary test invocation sleeps or creates grandchildren here.
    #[test]
    #[ignore = "Subprocess fixture for disposable_tree_preflight_and_termination"]
    fn disposable_tree_fixture() {
        match std::env::var("FEATHER_TEST_TREE_ROLE").as_deref() {
            Ok("parent") => {
                let mut leaf = fixture_command("leaf").spawn().unwrap();
                let _ = leaf.wait();
                std::thread::sleep(Duration::from_secs(15));
            }
            Ok("leaf") => std::thread::sleep(Duration::from_secs(15)),
            _ => {}
        }
    }

    fn fixture_command(role: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "actions::tests::disposable_tree_fixture",
                "--ignored",
            ])
            .env("FEATHER_TEST_TREE_ROLE", role)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    struct ChildCleanup(std::process::Child);
    impl Drop for ChildCleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    struct LeafCleanup(ProcessHandle);
    impl Drop for LeafCleanup {
        fn drop(&mut self) {
            // This handle was opened only for the known disposable fixture leaf.
            unsafe { TerminateProcess(self.0 .0, 1) };
        }
    }

    #[test]
    fn disposable_tree_preflight_and_termination() {
        let mut parent = ChildCleanup(fixture_command("parent").spawn().unwrap());
        let parent_pid = parent.0.id();
        let mut sampler = Sampler::new().unwrap();
        let started = Instant::now();
        let processes = loop {
            let snapshot = sampler.sample().unwrap();
            if let (Some(root), Some(leaf)) = (
                snapshot.processes.iter().find(|p| p.pid == parent_pid),
                snapshot
                    .processes
                    .iter()
                    .find(|p| p.parent_pid == parent_pid),
            ) {
                break vec![root.clone(), leaf.clone()];
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "fixture leaf did not start"
            );
            assert!(
                parent.0.try_wait().unwrap().is_none(),
                "fixture exited early"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let leaf = LeafCleanup(
            open_checked(
                processes[1].pid,
                processes[1].created,
                PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
            )
            .unwrap(),
        );

        // A stale root identity fails only after a valid descendant was opened.
        // Preflight must still leave that descendant and the parent alive.
        let mut stale = processes.clone();
        stale[0].created += 1;
        assert!(terminate_tree(&Tree::new(&stale).plan(0).unwrap()).is_err());
        assert!(parent.0.try_wait().unwrap().is_none());
        assert_ne!(unsafe { WaitForSingleObject(leaf.0 .0, 0) }, WAIT_OBJECT_0);

        // A protected process anywhere in a supplied snapshot aborts the entire
        // action. The synthetic System row is never opened or acted upon.
        let mut protected = processes.clone();
        protected[1].pid = 4;
        assert!(terminate_tree(&Tree::new(&protected).plan(0).unwrap()).is_err());
        assert!(parent.0.try_wait().unwrap().is_none());
        assert_ne!(unsafe { WaitForSingleObject(leaf.0 .0, 0) }, WAIT_OBJECT_0);

        let plan = Tree::new(&processes).plan(0).unwrap();
        assert_eq!(plan.len(), 2);
        terminate_tree(&plan).unwrap();
        assert_eq!(
            unsafe { WaitForSingleObject(leaf.0 .0, 5_000) },
            WAIT_OBJECT_0
        );
        let stopped = Instant::now();
        while parent.0.try_wait().unwrap().is_none() {
            assert!(stopped.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
