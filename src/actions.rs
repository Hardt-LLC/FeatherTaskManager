//! Explicit user actions; no process handles remain open between operations.

use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr::null};

use crate::{i18n::tr, process_tree::TerminationPlan};

use windows_sys::Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0},
    System::{
        SystemInformation::GetWindowsDirectoryW,
        Threading::{
            GetCurrentProcessId, GetProcessTimes, IsProcessCritical, OpenProcess,
            QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
            PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
            PROCESS_TERMINATE,
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
        return Err(last_error(tr(
            "프로세스의 시작 시간을 확인할 수 없습니다",
            "Cannot verify the process creation time",
        )));
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
    let parameters = format!("--language {}", crate::i18n::language().code());
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
        assert!(executable_path(pid, 0).is_err());
        assert!(executable_path(pid, created + 1).is_err());
        let actual = executable_path(pid, created).unwrap();
        assert_eq!(
            std::path::Path::new(&actual),
            std::env::current_exe().unwrap().as_path()
        );
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
