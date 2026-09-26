//! Native service snapshots and explicit user actions. Call these on a worker.

use crate::i18n::tr;

use std::{
    collections::HashMap,
    mem::{size_of, size_of_val},
    ptr::null,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        GetLastError, ERROR_ACCESS_DENIED, ERROR_DEPENDENT_SERVICES_RUNNING, ERROR_MORE_DATA,
        ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ERROR_SERVICE_DISABLED,
        ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_NOT_ACTIVE,
    },
    System::Services::*,
};

#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    pub display_name: String,
    pub state: u32,
    pub pid: u32,
    pub start_type: Option<u32>,
}

/// Configuration is queried for a selected service only, never once per row.
#[derive(Clone, Debug)]
pub struct ServiceDetails {
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub account: String,
    pub binary_path: String,
    pub load_order_group: String,
    pub dependencies: Vec<String>,
    pub state: u32,
    pub pid: u32,
    pub start_type: u32,
    pub warnings: Vec<String>,
}

struct ServiceHandle(SC_HANDLE);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn service_error(context: &str, code: u32) -> String {
    let message = match code {
        ERROR_ACCESS_DENIED => {
            tr("권한이 없습니다. 필요한 경우 Feather Task를 관리자 권한으로 실행하세요.", "Access denied. Run Feather Task Manager as administrator if needed.").to_owned()
        }
        ERROR_SERVICE_ALREADY_RUNNING => tr("서비스가 이미 실행 중입니다.", "The service is already running.").to_owned(),
        ERROR_SERVICE_NOT_ACTIVE => tr("서비스가 이미 중지되어 있습니다.", "The service is already stopped.").to_owned(),
        ERROR_SERVICE_DISABLED => tr("사용 안 함으로 설정된 서비스입니다.", "This service is disabled.").to_owned(),
        ERROR_DEPENDENT_SERVICES_RUNNING => {
            tr("이 서비스에 의존하는 서비스가 실행 중이므로 중지할 수 없습니다.", "This service cannot be stopped while dependent services are running.").to_owned()
        }
        ERROR_SERVICE_CANNOT_ACCEPT_CTRL => {
            tr("서비스가 현재 요청을 받을 수 없습니다. 상태 변경이 끝난 후 다시 시도하세요.", "The service cannot accept requests right now. Try again after its state transition completes.").to_owned()
        }
        ERROR_SERVICE_DOES_NOT_EXIST => {
            tr("서비스가 더 이상 존재하지 않습니다. 목록을 새로 고치세요.", "The service no longer exists. Refresh the list.").to_owned()
        }
        _ => std::io::Error::from_raw_os_error(code as i32).to_string(),
    };
    format!("{context}: {message} (Windows {code})")
}

fn open_manager(access: u32) -> Result<ServiceHandle, String> {
    let raw = unsafe { OpenSCManagerW(null(), null(), access) };
    if raw.is_null() {
        return Err(service_error(
            tr(
                "서비스 관리자에 연결할 수 없습니다",
                "Cannot connect to the service manager",
            ),
            unsafe { GetLastError() },
        ));
    }
    Ok(ServiceHandle(raw))
}

fn wide_name(name: &str) -> Result<Vec<u16>, String> {
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    if wide.is_empty() || wide.len() > 256 || wide.contains(&0) || name.contains(['/', '\\']) {
        return Err(tr("올바르지 않은 서비스 이름입니다.", "Invalid service name.").into());
    }
    wide.push(0);
    Ok(wide)
}

/// Reads an in-buffer UTF-16 string without following an unchecked native pointer.
fn buffer_string(buffer: &[usize], pointer: *const u16) -> Result<String, String> {
    let base = buffer.as_ptr() as usize;
    let bytes = size_of_val(buffer);
    let offset = (pointer as usize)
        .checked_sub(base)
        .filter(|offset| *offset < bytes && offset % size_of::<u16>() == 0)
        .ok_or(tr(
            "서비스 목록의 문자열 주소가 올바르지 않습니다.",
            "Invalid string address in the service list.",
        ))?;
    let len = (bytes - offset) / size_of::<u16>();
    // The pointer is aligned and the complete slice lies within the owned buffer.
    let chars = unsafe { std::slice::from_raw_parts(pointer, len) };
    let end = chars.iter().position(|c| *c == 0).ok_or(tr(
        "서비스 목록의 문자열이 올바르게 끝나지 않았습니다.",
        "An unterminated string was found in the service list.",
    ))?;
    Ok(String::from_utf16_lossy(&chars[..end]))
}

fn optional_buffer_string(buffer: &[usize], pointer: *const u16) -> Result<String, String> {
    if pointer.is_null() {
        Ok(String::new())
    } else {
        buffer_string(buffer, pointer)
    }
}

fn buffer_dependencies(buffer: &[usize], pointer: *const u16) -> Result<Vec<String>, String> {
    if pointer.is_null() {
        return Ok(Vec::new());
    }
    let base = buffer.as_ptr() as usize;
    let bytes = size_of_val(buffer);
    let offset = (pointer as usize)
        .checked_sub(base)
        .filter(|offset| *offset < bytes && offset % size_of::<u16>() == 0)
        .ok_or(tr(
            "서비스 종속성 주소가 올바르지 않습니다.",
            "Invalid service dependency address.",
        ))?;
    let chars = unsafe { std::slice::from_raw_parts(pointer, (bytes - offset) / size_of::<u16>()) };
    let mut result = Vec::new();
    let mut position = 0;
    loop {
        let tail = chars.get(position..).ok_or(tr(
            "서비스 종속성 목록이 올바르게 끝나지 않았습니다.",
            "Unterminated service dependency list.",
        ))?;
        let end = tail.iter().position(|value| *value == 0).ok_or(tr(
            "서비스 종속성 목록이 올바르게 끝나지 않았습니다.",
            "Unterminated service dependency list.",
        ))?;
        if end == 0 {
            return Ok(result);
        }
        result.push(String::from_utf16_lossy(&tail[..end]));
        position += end + 1;
    }
}

fn query_status(service: &ServiceHandle) -> Result<SERVICE_STATUS_PROCESS, String> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0;
    if unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            (&mut status as *mut SERVICE_STATUS_PROCESS).cast(),
            size_of_val(&status) as u32,
            &mut needed,
        )
    } == 0
    {
        return Err(service_error(
            tr(
                "서비스 상태를 확인할 수 없습니다",
                "Cannot read the service status",
            ),
            unsafe { GetLastError() },
        ));
    }
    Ok(status)
}

pub fn details(name: &str) -> Result<ServiceDetails, String> {
    let wide = wide_name(name)?;
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let raw = unsafe {
        OpenServiceW(
            manager.0,
            wide.as_ptr(),
            SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
        )
    };
    if raw.is_null() {
        return Err(service_error(
            tr(
                "서비스 세부 정보를 읽을 수 없습니다",
                "Cannot read service details",
            ),
            unsafe { GetLastError() },
        ));
    }
    // Holding this handle also prevents deletion/recreation under the same name
    // while the status and configuration are queried.
    let service = ServiceHandle(raw);
    let status = query_status(&service)?;
    let mut buffer = [0usize; 8192 / size_of::<usize>()];
    let mut needed = 0;
    let config = buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    if unsafe { QueryServiceConfigW(service.0, config, size_of_val(&buffer) as u32, &mut needed) }
        == 0
    {
        return Err(service_error(
            tr(
                "서비스 구성을 읽을 수 없습니다",
                "Cannot read the service configuration",
            ),
            unsafe { GetLastError() },
        ));
    }
    let config = unsafe { config.read() };
    let mut result = ServiceDetails {
        name: name.to_owned(),
        display_name: optional_buffer_string(&buffer, config.lpDisplayName)?,
        description: String::new(),
        account: optional_buffer_string(&buffer, config.lpServiceStartName)?,
        binary_path: optional_buffer_string(&buffer, config.lpBinaryPathName)?,
        load_order_group: optional_buffer_string(&buffer, config.lpLoadOrderGroup)?,
        dependencies: buffer_dependencies(&buffer, config.lpDependencies)?,
        state: status.dwCurrentState,
        pid: status.dwProcessId,
        start_type: config.dwStartType,
        warnings: Vec::new(),
    };
    buffer.fill(0);
    if unsafe {
        QueryServiceConfig2W(
            service.0,
            SERVICE_CONFIG_DESCRIPTION,
            buffer.as_mut_ptr().cast(),
            size_of_val(&buffer) as u32,
            &mut needed,
        )
    } != 0
    {
        let description = unsafe { buffer.as_ptr().cast::<SERVICE_DESCRIPTIONW>().read() };
        result.description = optional_buffer_string(&buffer, description.lpDescription)?;
    } else {
        result.warnings.push(service_error(
            tr(
                "서비스 설명을 읽을 수 없습니다",
                "Cannot read the service description",
            ),
            unsafe { GetLastError() },
        ));
    }
    Ok(result)
}

fn parse_page(buffer: &[usize], count: u32) -> Result<Vec<Service>, String> {
    let required = (count as usize)
        .checked_mul(size_of::<ENUM_SERVICE_STATUS_PROCESSW>())
        .ok_or(tr(
            "서비스 목록의 크기가 올바르지 않습니다.",
            "Invalid service list size.",
        ))?;
    if required > size_of_val(buffer) {
        return Err(tr(
            "서비스 목록이 버퍼 범위를 벗어났습니다.",
            "The service list exceeds the buffer bounds.",
        )
        .into());
    }
    let mut result = Vec::with_capacity(count as usize);
    let entries = buffer.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>();
    for index in 0..count as usize {
        // The array fits in the allocation and usize storage provides pointer alignment.
        let entry = unsafe { entries.add(index).read() };
        result.push(Service {
            name: buffer_string(buffer, entry.lpServiceName)?,
            display_name: buffer_string(buffer, entry.lpDisplayName)?,
            state: entry.ServiceStatusProcess.dwCurrentState,
            pid: entry.ServiceStatusProcess.dwProcessId,
            start_type: None,
        });
    }
    Ok(result)
}

type ConfigCache = HashMap<String, (Instant, Option<u32>)>;
static CONFIG_CACHE: OnceLock<Mutex<ConfigCache>> = OnceLock::new();

fn query_start_type(manager: &ServiceHandle, name: &str) -> Option<u32> {
    let wide = wide_name(name).ok()?;
    let raw = unsafe { OpenServiceW(manager.0, wide.as_ptr(), SERVICE_QUERY_CONFIG) };
    if raw.is_null() {
        return None;
    }
    let service = ServiceHandle(raw);
    // Microsoft documents an 8KiB maximum for QueryServiceConfigW. A single
    // bounded call avoids a separate size query and allocation for every service.
    let mut buffer = [0usize; 8192 / size_of::<usize>()];
    let config = buffer.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>();
    let mut needed = 0;
    if unsafe { QueryServiceConfigW(service.0, config, size_of_val(&buffer) as u32, &mut needed) }
        == 0
    {
        return None;
    }
    Some(unsafe { (*config).dwStartType })
}

/// Enumerates Win32 services, including stopped services. Configuration is cached
/// for 60 seconds; status and PIDs are fresh each call. Denied config reads are
/// shown as unknown without hiding the service or attempting elevation.
pub fn list() -> Result<Vec<Service>, String> {
    let manager = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
    const MAX_BYTES: usize = 256 * 1024;
    const MAX_PAGES: usize = 64;
    let mut buffer = vec![0usize; 64 * 1024 / size_of::<usize>()];
    let mut resume = 0;
    let mut result = Vec::new();
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        let previous_resume = resume;
        let mut needed = 0;
        let mut returned = 0;
        let success = unsafe {
            EnumServicesStatusExW(
                manager.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                buffer.as_mut_ptr().cast(),
                size_of_val(buffer.as_slice()) as u32,
                &mut needed,
                &mut returned,
                &mut resume,
                null(),
            )
        };
        let error = if success == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        if success == 0 && error != ERROR_MORE_DATA {
            return Err(service_error(
                tr(
                    "서비스 목록을 읽을 수 없습니다",
                    "Cannot read the service list",
                ),
                error,
            ));
        }
        result.extend(parse_page(&buffer, returned)?);
        if success != 0 {
            complete = true;
            break;
        }
        let current_bytes = size_of_val(buffer.as_slice());
        let next_bytes = (needed as usize)
            .max(current_bytes.saturating_mul(2))
            .min(MAX_BYTES);
        if next_bytes > current_bytes {
            buffer.resize(next_bytes.div_ceil(size_of::<usize>()), 0);
        } else if returned == 0 || resume == previous_resume {
            return Err(tr(
                "서비스 목록 조회가 진행되지 않았습니다. 새로 고친 후 다시 시도하세요.",
                "Service enumeration made no progress. Refresh the list and try again.",
            )
            .into());
        }
    }
    if !complete {
        return Err(tr(
            "서비스 목록 조회의 안전 한도를 초과했습니다. 새로 고친 후 다시 시도하세요.",
            "Service enumeration exceeded its safety limit. Refresh the list and try again.",
        )
        .into());
    }
    result.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    result.dedup_by(|a, b| a.name == b.name);

    let cache = CONFIG_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    let cached = cache.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut updated = HashMap::with_capacity(result.len());
    for service in &mut result {
        let entry = cached
            .get(&service.name)
            .copied()
            .filter(|(sampled, _)| {
                now.saturating_duration_since(*sampled) < Duration::from_secs(60)
            })
            .unwrap_or_else(|| (now, query_start_type(&manager, &service.name)));
        service.start_type = entry.1;
        updated.insert(service.name.clone(), entry);
    }
    // Replace the cache to discard services removed since the previous snapshot.
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = updated;
    Ok(result)
}

/// Submits a start request; the next snapshot reflects pending/running state.
pub fn start(name: &str) -> Result<(), String> {
    let name = wide_name(name)?;
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let raw = unsafe { OpenServiceW(manager.0, name.as_ptr(), SERVICE_START) };
    if raw.is_null() {
        return Err(service_error(
            tr("서비스를 시작할 수 없습니다", "Cannot start the service"),
            unsafe { GetLastError() },
        ));
    }
    let service = ServiceHandle(raw);
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        return Err(service_error(
            tr("서비스를 시작할 수 없습니다", "Cannot start the service"),
            unsafe { GetLastError() },
        ));
    }
    Ok(())
}

/// Submits a stop request for this service only. Windows enforces its access,
/// accepted-control, and dependent-service checks. The UI confirms before calling.
pub fn stop(name: &str) -> Result<(), String> {
    let name = wide_name(name)?;
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let raw = unsafe { OpenServiceW(manager.0, name.as_ptr(), SERVICE_STOP) };
    if raw.is_null() {
        return Err(service_error(
            tr("서비스를 중지할 수 없습니다", "Cannot stop the service"),
            unsafe { GetLastError() },
        ));
    }
    let service = ServiceHandle(raw);
    let mut status = SERVICE_STATUS::default();
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        return Err(service_error(
            tr("서비스를 중지할 수 없습니다", "Cannot stop the service"),
            unsafe { GetLastError() },
        ));
    }
    Ok(())
}

fn restartable(state: u32, controls: u32) -> bool {
    matches!(state, SERVICE_RUNNING | SERVICE_PAUSED) && controls & SERVICE_ACCEPT_STOP != 0
}

fn wait_for_state(service: &ServiceHandle, target: u32, deadline: Instant) -> Result<(), String> {
    loop {
        let status = query_status(service)?;
        if status.dwCurrentState == target {
            return Ok(());
        }
        if target == SERVICE_RUNNING && status.dwCurrentState == SERVICE_STOPPED {
            return Err(service_error(
                tr(
                    "서비스가 시작되는 동안 중지되었습니다",
                    "The service stopped while starting",
                ),
                status.dwWin32ExitCode,
            ));
        }
        if Instant::now() >= deadline {
            return Err(tr(
                "서비스 상태 변경 대기 시간이 초과되었습니다. 요청은 취소되지 않았으며 Windows에서 계속 처리될 수 있습니다. 새로 고쳐 확인하세요.",
                "Timed out waiting for the service. The request was not cancelled and Windows may still be processing it. Refresh to verify."
            ).into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Restart this service only, without stopping dependent services. All checks
/// and operations use one pinned SCM handle. Run on a worker: our polling is
/// bounded to 15 seconds, while Windows may itself block a control RPC longer.
/// The UI must confirm the service name before calling.
pub fn restart(name: &str) -> Result<(), String> {
    let wide = wide_name(name)?;
    let manager = open_manager(SC_MANAGER_CONNECT)?;
    let raw = unsafe {
        OpenServiceW(
            manager.0,
            wide.as_ptr(),
            SERVICE_QUERY_STATUS | SERVICE_START | SERVICE_STOP,
        )
    };
    if raw.is_null() {
        return Err(service_error(
            tr(
                "서비스를 다시 시작할 수 없습니다",
                "Cannot restart the service",
            ),
            unsafe { GetLastError() },
        ));
    }
    let service = ServiceHandle(raw);
    let status = query_status(&service)?;
    if !restartable(status.dwCurrentState, status.dwControlsAccepted) {
        return Err(tr(
            "실행 중이거나 일시 중지된 서비스 중 중지를 허용하는 서비스만 다시 시작할 수 있습니다. 상태를 새로 고치세요.",
            "Only a running or paused service that accepts stop can be restarted. Refresh its status."
        ).into());
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut stopped = SERVICE_STATUS::default();
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut stopped) } == 0 {
        return Err(service_error(
            tr(
                "다시 시작하기 위해 서비스를 중지할 수 없습니다",
                "Cannot stop the service for restart",
            ),
            unsafe { GetLastError() },
        ));
    }
    wait_for_state(&service, SERVICE_STOPPED, deadline)?;
    // Never start while a stop is pending, including after a wait timeout.
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        return Err(service_error(
            tr(
                "서비스를 중지했지만 다시 시작할 수 없습니다",
                "The service was stopped but could not be started again",
            ),
            unsafe { GetLastError() },
        ));
    }
    wait_for_state(&service, SERVICE_RUNNING, deadline)
}

pub fn state_label(state: u32) -> &'static str {
    match state {
        SERVICE_STOPPED => tr("중지됨", "Stopped"),
        SERVICE_START_PENDING => tr("시작 중", "Starting"),
        SERVICE_STOP_PENDING => tr("중지 중", "Stopping"),
        SERVICE_RUNNING => tr("실행 중", "Running"),
        SERVICE_CONTINUE_PENDING => tr("다시 시작 중", "Resuming"),
        SERVICE_PAUSE_PENDING => tr("일시 중지 중", "Pausing"),
        SERVICE_PAUSED => tr("일시 중지됨", "Paused"),
        _ => tr("알 수 없음", "Unknown"),
    }
}

pub fn start_type_label(value: Option<u32>) -> &'static str {
    match value {
        Some(SERVICE_BOOT_START) => tr("부팅", "Boot"),
        Some(SERVICE_SYSTEM_START) => tr("시스템", "System"),
        Some(SERVICE_AUTO_START) => tr("자동", "Automatic"),
        Some(SERVICE_DEMAND_START) => tr("수동", "Manual"),
        Some(SERVICE_DISABLED) => tr("사용 안 함", "Disabled"),
        _ => tr("확인 불가", "Unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{with_language, Language};
    use std::collections::HashSet;

    #[test]
    fn labels_distinguish_pending_disabled_and_unknown() {
        with_language(Language::Korean, || {
            assert_eq!(state_label(SERVICE_RUNNING), "실행 중");
            assert_ne!(
                state_label(SERVICE_START_PENDING),
                state_label(SERVICE_RUNNING)
            );
            assert_ne!(
                state_label(SERVICE_STOP_PENDING),
                state_label(SERVICE_STOPPED)
            );
            assert_eq!(state_label(u32::MAX), "알 수 없음");
            assert_eq!(start_type_label(Some(SERVICE_DISABLED)), "사용 안 함");
            assert_eq!(start_type_label(None), "확인 불가");
        });
    }

    #[test]
    fn english_service_labels_and_errors_do_not_change_service_names() {
        with_language(Language::English, || {
            assert_eq!(state_label(SERVICE_START_PENDING), "Starting");
            assert_eq!(state_label(SERVICE_RUNNING), "Running");
            assert_eq!(state_label(SERVICE_PAUSED), "Paused");
            assert_eq!(start_type_label(Some(SERVICE_AUTO_START)), "Automatic");
            assert_eq!(start_type_label(None), "Unavailable");
            assert_eq!(wide_name("").unwrap_err(), "Invalid service name.");
            assert!(service_error("Start", ERROR_SERVICE_DISABLED)
                .contains("This service is disabled."));
            assert_eq!(
                wide_name("서비스_123").unwrap(),
                "서비스_123\0".encode_utf16().collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn invalid_names_cannot_retarget_native_calls() {
        for name in ["", "service\0different", "service/name", "service\\name"] {
            assert!(wide_name(name).is_err());
            assert!(start(name).is_err());
            assert!(stop(name).is_err());
            assert!(details(name).is_err());
            assert!(restart(name).is_err());
        }
        assert!(wide_name(&"a".repeat(257)).is_err());
        assert_eq!(wide_name("서비스_123").unwrap().last(), Some(&0));
    }

    #[test]
    fn native_page_parser_validates_counts_and_pointer_bounds() {
        let mut buffer = vec![0usize; 32];
        let entry = buffer.as_mut_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>();
        let string = unsafe {
            buffer
                .as_mut_ptr()
                .cast::<u8>()
                .add(size_of::<ENUM_SERVICE_STATUS_PROCESSW>())
                .cast::<u16>()
        };
        let text: Vec<u16> = "Example\0".encode_utf16().collect();
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), string, text.len());
            (*entry).lpServiceName = string;
            (*entry).lpDisplayName = string;
            (*entry).ServiceStatusProcess.dwCurrentState = SERVICE_RUNNING;
            (*entry).ServiceStatusProcess.dwProcessId = 42;
        }
        let parsed = parse_page(&buffer, 1).unwrap();
        assert_eq!(parsed[0].name, "Example");
        assert_eq!(parsed[0].pid, 42);
        assert!(parse_page(&buffer, u32::MAX).is_err());
        unsafe {
            (*entry).lpServiceName = std::ptr::null_mut();
        }
        assert!(parse_page(&buffer, 1).is_err());
        unsafe {
            (*entry).lpServiceName = buffer.as_mut_ptr().cast::<u8>().add(1).cast();
        }
        assert!(parse_page(&buffer, 1).is_err());
        buffer.fill(usize::MAX);
        assert!(buffer_string(&buffer, buffer.as_ptr().cast()).is_err());
    }

    #[test]
    fn dependencies_validate_native_multi_string_bounds() {
        let mut buffer = vec![0usize; 32];
        let text: Vec<u16> = "RpcSs\0+NetworkProvider\0\0".encode_utf16().collect();
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), buffer.as_mut_ptr().cast(), text.len());
        }
        assert_eq!(
            buffer_dependencies(&buffer, buffer.as_ptr().cast()).unwrap(),
            ["RpcSs", "+NetworkProvider"]
        );
        assert!(buffer_dependencies(&buffer, std::ptr::null())
            .unwrap()
            .is_empty());
        assert!(buffer_dependencies(&buffer, unsafe {
            buffer.as_ptr().cast::<u8>().add(1).cast()
        })
        .is_err());
        buffer.fill(usize::MAX);
        assert!(buffer_dependencies(&buffer, buffer.as_ptr().cast()).is_err());
        // A valid first string without the terminating list NUL is still invalid.
        let end = size_of_val(buffer.as_slice()) / 2;
        unsafe {
            buffer.as_mut_ptr().cast::<u16>().add(end - 1).write(0);
        }
        assert!(buffer_dependencies(&buffer, buffer.as_ptr().cast()).is_err());
    }

    #[test]
    fn restart_preflight_requires_stoppable_stable_state() {
        assert!(restartable(SERVICE_RUNNING, SERVICE_ACCEPT_STOP));
        assert!(restartable(SERVICE_PAUSED, SERVICE_ACCEPT_STOP));
        assert!(!restartable(SERVICE_RUNNING, 0));
        for state in [
            SERVICE_STOPPED,
            SERVICE_START_PENDING,
            SERVICE_STOP_PENDING,
            SERVICE_PAUSE_PENDING,
            SERVICE_CONTINUE_PENDING,
            u32::MAX,
        ] {
            assert!(!restartable(state, SERVICE_ACCEPT_STOP));
        }
    }

    #[test]
    fn selected_service_configuration_is_read_only() {
        let services = list().unwrap();
        let detail = services
            .iter()
            .find_map(|service| details(&service.name).ok())
            .expect("at least one readable service configuration");
        assert!(!detail.name.is_empty());
        assert!(!detail.display_name.is_empty());
        assert!(!detail.binary_path.is_empty());
        assert!((SERVICE_STOPPED..=SERVICE_PAUSED).contains(&detail.state));
    }

    #[test]
    fn live_service_snapshot_is_read_only_and_consistent() {
        let services = list().expect("read-only SCM enumeration");
        assert!(!services.is_empty());
        let mut names = HashSet::new();
        for service in &services {
            assert!(!service.name.is_empty());
            assert!(names.insert(service.name.to_lowercase()));
            assert!((SERVICE_STOPPED..=SERVICE_PAUSED).contains(&service.state));
            if service.state == SERVICE_STOPPED {
                assert_eq!(service.pid, 0);
            }
        }
        assert!(services
            .iter()
            .any(|service| service.state == SERVICE_RUNNING));
        // The second snapshot exercises the configuration cache without mutations.
        assert!(!list().unwrap().is_empty());
    }
}
