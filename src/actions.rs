//! Explicit user actions; no process handles remain open between operations.

use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr::null};

use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE},
    System::{
        SystemInformation::GetWindowsDirectoryW,
        Threading::{
            GetCurrentProcessId, GetProcessTimes, IsProcessCritical, OpenProcess,
            QueryFullProcessImageNameW, TerminateProcess, PROCESS_ACCESS_RIGHTS,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        },
    },
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
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
        return Err(last_error("프로세스의 시작 시간을 확인할 수 없습니다"));
    }
    Ok(filetime_value(created))
}

fn open_checked(
    pid: u32,
    expected_created: u64,
    access: PROCESS_ACCESS_RIGHTS,
) -> Result<ProcessHandle, String> {
    // A missing sample identity must never act as a wildcard: PIDs are reused.
    if expected_created == 0 {
        return Err(
            "프로세스의 시작 시간이 확인되지 않았습니다. 새로 고친 후 다시 시도하세요.".into(),
        );
    }
    let raw = unsafe { OpenProcess(access | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if raw.is_null() {
        return Err(last_error("프로세스를 열 수 없습니다"));
    }
    let handle = ProcessHandle(raw);
    if creation_time(handle.0)? != expected_created {
        return Err(
            "선택한 프로세스가 이미 종료되고 PID가 재사용되었습니다. 목록을 새로 고치세요.".into(),
        );
    }
    Ok(handle)
}

/// Forcefully end the selected process after the caller has obtained confirmation.
/// All checks and the termination use one handle, so PID reuse cannot change the target.
pub fn terminate(pid: u32, expected_created: u64) -> Result<(), String> {
    if pid == 0 || pid == 4 {
        return Err("Windows 시스템 프로세스는 종료할 수 없습니다.".into());
    }
    if pid == unsafe { GetCurrentProcessId() } {
        return Err("이 작업 관리자는 종료 메뉴 대신 창의 닫기 버튼으로 종료하세요.".into());
    }

    let handle = open_checked(pid, expected_created, PROCESS_TERMINATE)?;
    let mut critical = 0;
    if unsafe { IsProcessCritical(handle.0, &mut critical) } == 0 {
        return Err(last_error(
            "시스템 필수 프로세스 여부를 확인할 수 없어 종료를 취소했습니다",
        ));
    }
    if critical != 0 {
        return Err("Windows 작동에 필수적인 프로세스는 종료할 수 없습니다.".into());
    }
    if unsafe { TerminateProcess(handle.0, 1) } == 0 {
        return Err(last_error("프로세스를 종료할 수 없습니다"));
    }
    // TerminateProcess is asynchronous; refreshing the snapshot shows actual exit.
    Ok(())
}

/// Query only the requested process, with enough space for Windows extended paths.
pub fn executable_path(pid: u32, expected_created: u64) -> Result<String, String> {
    let handle = open_checked(pid, expected_created, PROCESS_QUERY_LIMITED_INFORMATION)?;
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle.0, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
        return Err(last_error("실행 파일의 경로를 확인할 수 없습니다"));
    }
    String::from_utf16(&buffer[..length as usize])
        .map_err(|_| "실행 파일 경로에 표시할 수 없는 유니코드 문자가 있습니다.".into())
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
            0 | 8 => "메모리가 부족합니다",
            2 => "실행 파일을 찾을 수 없습니다",
            3 => "경로를 찾을 수 없습니다",
            5 => "접근이 거부되었거나 관리자 권한 요청이 취소되었습니다",
            26 => "파일이 사용 중입니다",
            27 | 31 => "파일 형식의 연결 프로그램을 찾을 수 없습니다",
            28..=30 => "Windows 셸과 통신할 수 없습니다",
            32 => "필요한 DLL을 찾을 수 없습니다",
            _ => "Windows 셸에서 실행하지 못했습니다",
        };
        Err(format!("{detail} (셸 오류 {result})."))
    }
}

pub fn reveal_executable(pid: u32, expected_created: u64) -> Result<(), String> {
    let path = executable_path(pid, expected_created)?;
    // Never resolve explorer.exe through the current directory or an untrusted PATH.
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    if length == 0 {
        return Err(last_error("Windows 폴더를 찾을 수 없습니다"));
    }
    if length as usize >= buffer.len() {
        return Err("Windows 폴더 경로가 너무 깁니다.".into());
    }
    use std::os::windows::ffi::OsStringExt;
    let mut explorer =
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
    explorer.push("explorer.exe");
    // Quotes are not permitted in Windows filenames; this keeps spaces/commas intact.
    let parameters = format!("/select,\"{path}\"");
    shell_execute("open", explorer.as_os_str(), Some(OsStr::new(&parameters)))
}

/// Request elevation through the standard UAC flow; the caller may close on success.
pub fn relaunch_elevated() -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("현재 실행 파일의 경로를 확인할 수 없습니다: {error}"))?;
    shell_execute("runas", executable.as_os_str(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(executable_path(pid, 0).is_err());
        assert!(executable_path(pid, created + 1).is_err());
        let actual = executable_path(pid, created).unwrap();
        assert_eq!(
            std::path::Path::new(&actual),
            std::env::current_exe().unwrap().as_path()
        );
    }
}
