//! Native service snapshots and explicit user actions. Call these on a worker.

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

struct ServiceHandle(SC_HANDLE);

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        unsafe { CloseServiceHandle(self.0) };
    }
}

fn service_error(context: &str, code: u32) -> String {
    let message = match code {
        ERROR_ACCESS_DENIED => {
            "권한이 없습니다. 필요한 경우 Feather Task를 관리자 권한으로 실행하세요.".to_owned()
        }
        ERROR_SERVICE_ALREADY_RUNNING => "서비스가 이미 실행 중입니다.".to_owned(),
        ERROR_SERVICE_NOT_ACTIVE => "서비스가 이미 중지되어 있습니다.".to_owned(),
        ERROR_SERVICE_DISABLED => "사용 안 함으로 설정된 서비스입니다.".to_owned(),
        ERROR_DEPENDENT_SERVICES_RUNNING => {
            "이 서비스에 의존하는 서비스가 실행 중이므로 중지할 수 없습니다.".to_owned()
        }
        ERROR_SERVICE_CANNOT_ACCEPT_CTRL => {
            "서비스가 현재 요청을 받을 수 없습니다. 상태 변경이 끝난 후 다시 시도하세요.".to_owned()
        }
        ERROR_SERVICE_DOES_NOT_EXIST => {
            "서비스가 더 이상 존재하지 않습니다. 목록을 새로 고치세요.".to_owned()
        }
        _ => std::io::Error::from_raw_os_error(code as i32).to_string(),
    };
    format!("{context}: {message} (Windows {code})")
}

fn open_manager(access: u32) -> Result<ServiceHandle, String> {
    let raw = unsafe { OpenSCManagerW(null(), null(), access) };
    if raw.is_null() {
        return Err(service_error(
            "서비스 관리자에 연결할 수 없습니다",
            unsafe { GetLastError() },
        ));
    }
    Ok(ServiceHandle(raw))
}

fn wide_name(name: &str) -> Result<Vec<u16>, String> {
    let mut wide: Vec<u16> = name.encode_utf16().collect();
    if wide.is_empty() || wide.len() > 256 || wide.contains(&0) || name.contains(['/', '\\']) {
        return Err("올바르지 않은 서비스 이름입니다.".into());
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
        .ok_or("서비스 목록의 문자열 주소가 올바르지 않습니다.")?;
    let len = (bytes - offset) / size_of::<u16>();
    // The pointer is aligned and the complete slice lies within the owned buffer.
    let chars = unsafe { std::slice::from_raw_parts(pointer, len) };
    let end = chars
        .iter()
        .position(|c| *c == 0)
        .ok_or("서비스 목록의 문자열이 올바르게 끝나지 않았습니다.")?;
    Ok(String::from_utf16_lossy(&chars[..end]))
}

fn parse_page(buffer: &[usize], count: u32) -> Result<Vec<Service>, String> {
    let required = (count as usize)
        .checked_mul(size_of::<ENUM_SERVICE_STATUS_PROCESSW>())
        .ok_or("서비스 목록의 크기가 올바르지 않습니다.")?;
    if required > size_of_val(buffer) {
        return Err("서비스 목록이 버퍼 범위를 벗어났습니다.".into());
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
            return Err(service_error("서비스 목록을 읽을 수 없습니다", error));
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
            return Err(
                "서비스 목록 조회가 진행되지 않았습니다. 새로 고친 후 다시 시도하세요.".into(),
            );
        }
    }
    if !complete {
        return Err(
            "서비스 목록 조회의 안전 한도를 초과했습니다. 새로 고친 후 다시 시도하세요.".into(),
        );
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
        return Err(service_error("서비스를 시작할 수 없습니다", unsafe {
            GetLastError()
        }));
    }
    let service = ServiceHandle(raw);
    if unsafe { StartServiceW(service.0, 0, null()) } == 0 {
        return Err(service_error("서비스를 시작할 수 없습니다", unsafe {
            GetLastError()
        }));
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
        return Err(service_error("서비스를 중지할 수 없습니다", unsafe {
            GetLastError()
        }));
    }
    let service = ServiceHandle(raw);
    let mut status = SERVICE_STATUS::default();
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &mut status) } == 0 {
        return Err(service_error("서비스를 중지할 수 없습니다", unsafe {
            GetLastError()
        }));
    }
    Ok(())
}

pub fn state_label(state: u32) -> &'static str {
    match state {
        SERVICE_STOPPED => "중지됨",
        SERVICE_START_PENDING => "시작 중",
        SERVICE_STOP_PENDING => "중지 중",
        SERVICE_RUNNING => "실행 중",
        SERVICE_CONTINUE_PENDING => "다시 시작 중",
        SERVICE_PAUSE_PENDING => "일시 중지 중",
        SERVICE_PAUSED => "일시 중지됨",
        _ => "알 수 없음",
    }
}

pub fn start_type_label(value: Option<u32>) -> &'static str {
    match value {
        Some(SERVICE_BOOT_START) => "부팅",
        Some(SERVICE_SYSTEM_START) => "시스템",
        Some(SERVICE_AUTO_START) => "자동",
        Some(SERVICE_DEMAND_START) => "수동",
        Some(SERVICE_DISABLED) => "사용 안 함",
        _ => "확인 불가",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn labels_distinguish_pending_disabled_and_unknown() {
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
    }

    #[test]
    fn invalid_names_cannot_retarget_native_calls() {
        for name in ["", "service\0different", "service/name", "service\\name"] {
            assert!(wide_name(name).is_err());
            assert!(start(name).is_err());
            assert!(stop(name).is_err());
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
