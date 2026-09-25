//! Explicit, reversible Task Manager registration. No registry polling or startup hooks.
//!
//! Windows' IFEO Debugger value redirects taskmgr.exe. The command always names a
//! copy under the system's trusted Program Files folder, never a portable path.

use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{Read, Write},
    mem::size_of,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    time::{SystemTime, UNIX_EPOCH},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, GENERIC_ALL, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    },
    Security::{
        AddAccessAllowedAceEx, CreateWellKnownSid, EqualSid, GetAce, GetKernelObjectSecurity,
        GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, InitializeAcl,
        InitializeSecurityDescriptor, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
        SetSecurityDescriptorGroup, SetSecurityDescriptorOwner, WinBuiltinAdministratorsSid,
        WinBuiltinUsersSid, WinLocalSystemSid, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_REVISION,
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, INHERIT_ONLY_ACE, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR,
        SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED, WELL_KNOWN_SID_TYPE,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandle, MoveFileExW,
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, DELETE, FILE_ALL_ACCESS, FILE_APPEND_DATA,
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_EXECUTE,
        FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, READ_CONTROL, WRITE_DAC, WRITE_OWNER,
    },
    System::{
        Com::CoTaskMemFree,
        Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryInfoKeyW,
            RegQueryValueExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE,
            KEY_SET_VALUE, KEY_WOW64_64KEY, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
        },
        Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE},
    },
    UI::{
        Shell::{
            FOLDERID_ProgramFilesX64, SHGetKnownFolderPath, ShellExecuteExW, SEE_MASK_NOASYNC,
            SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
        },
        WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

const IFEO_KEY: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Image File Execution Options\taskmgr.exe";
const APP_FOLDER: &str = "Feather Task Manager";
const APP_EXE: &str = "FeatherTaskManager.exe";
const MAX_REGISTRY_VALUE: u32 = 65_536;

#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Inactive,
    Active,
    Other(String),
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

struct Process(HANDLE);
impl Drop for Process {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Value {
    kind: u32,
    bytes: Vec<u8>,
}

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn error(context: &str) -> String {
    format!("{context}: {}", std::io::Error::last_os_error())
}

fn registry_error(code: u32) -> String {
    format!(
        "작업 관리자 연결 설정에 접근할 수 없습니다: {}",
        std::io::Error::from_raw_os_error(code as i32)
    )
}

fn install_path() -> Result<PathBuf, String> {
    let mut raw = null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFilesX64, 0, null_mut(), &mut raw) };
    if result < 0 || raw.is_null() {
        return Err(format!(
            "Windows Program Files 폴더를 확인할 수 없습니다 (0x{:08X}).",
            result as u32
        ));
    }
    let mut len = 0;
    while unsafe { *raw.add(len) } != 0 {
        len += 1;
    }
    let mut path = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(raw, len)
    }));
    unsafe { CoTaskMemFree(raw.cast()) };
    if !path.is_absolute() {
        return Err("Windows Program Files 폴더가 절대 경로가 아닙니다.".into());
    }
    path.push(APP_FOLDER);
    path.push(APP_EXE);
    Ok(path)
}

fn debugger_command(path: &Path) -> Result<String, String> {
    let text = path
        .to_str()
        .ok_or("설치 경로를 유니코드로 읽을 수 없습니다.")?;
    if !path.is_absolute()
        || text.contains(['\0', '"'])
        || path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("taskmgr.exe"))
    {
        return Err("작업 관리자 연결에 사용할 수 없는 실행 파일 경로입니다.".into());
    }
    Ok(format!("\"{text}\" --task-manager"))
}

fn open_key(root: HKEY, path: &str, writable: bool, create: bool) -> Result<Option<Key>, String> {
    let path = wide(path);
    let rights = KEY_QUERY_VALUE | KEY_WOW64_64KEY | if writable { KEY_SET_VALUE } else { 0 };
    let mut key = null_mut();
    let result = if create {
        unsafe {
            RegCreateKeyExW(
                root,
                path.as_ptr(),
                0,
                null(),
                REG_OPTION_NON_VOLATILE,
                rights,
                null(),
                &mut key,
                null_mut(),
            )
        }
    } else {
        unsafe { RegOpenKeyExW(root, path.as_ptr(), 0, rights, &mut key) }
    };
    match result {
        ERROR_SUCCESS => Ok(Some(Key(key))),
        ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND if !create => Ok(None),
        code => Err(registry_error(code)),
    }
}

fn read_value(key: &Key, name: &str) -> Result<Option<Value>, String> {
    let name = wide(name);
    let mut kind = 0;
    let mut size = 0;
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            null(),
            &mut kind,
            null_mut(),
            &mut size,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if result != ERROR_SUCCESS {
        return Err(registry_error(result));
    }
    if size > MAX_REGISTRY_VALUE {
        return Err("기존 작업 관리자 연결 값이 너무 커서 안전하게 변경할 수 없습니다.".into());
    }
    let mut bytes = vec![0; size as usize];
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            null(),
            &mut kind,
            bytes.as_mut_ptr(),
            &mut size,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(registry_error(result));
    }
    bytes.truncate(size as usize);
    Ok(Some(Value { kind, bytes }))
}

fn string_value(text: &str) -> Value {
    Value {
        kind: REG_SZ,
        bytes: text
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect(),
    }
}

fn decode_string(value: &Value) -> Option<String> {
    if value.kind != REG_SZ || value.bytes.len() < 2 || !value.bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut words: Vec<u16> = value
        .bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if words.pop() != Some(0) || words.contains(&0) {
        return None;
    }
    String::from_utf16(&words).ok()
}

fn classify(value: Option<&Value>, command: &str) -> Status {
    match value {
        None => Status::Inactive,
        Some(value) if value == &string_value(command) => Status::Active,
        Some(value) => Status::Other(
            decode_string(value)
                .filter(|text| !text.is_empty())
                .map(|text| text.chars().take(240).collect())
                .unwrap_or_else(|| "알 수 없는 형식의 Debugger 설정".into()),
        ),
    }
}

fn ensure_no_filter(key: &Key) -> Result<(), String> {
    if let Some(value) = read_value(key, "UseFilter")? {
        if value.kind != REG_DWORD || value.bytes != 0_u32.to_le_bytes() {
            return Err("taskmgr.exe에 다른 프로그램의 경로별 실행 필터가 설정되어 있습니다. 해당 프로그램에서 먼저 연결을 해제하세요.".into());
        }
    }
    // IFEO may contain path-specific FilterFullPath subkeys. Leave all existing
    // child configurations untouched, including incomplete or disabled filters.
    let mut children = 0;
    let result = unsafe {
        RegQueryInfoKeyW(
            key.0,
            null_mut(),
            null_mut(),
            null(),
            &mut children,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
        )
    };
    if result != ERROR_SUCCESS {
        return Err(registry_error(result));
    }
    if children != 0 {
        return Err("taskmgr.exe에 다른 프로그램의 경로별 설정이 있습니다. 해당 프로그램에서 먼저 연결을 해제하세요.".into());
    }
    Ok(())
}

pub fn status() -> Result<Status, String> {
    let command = debugger_command(&install_path()?)?;
    let Some(key) = open_key(HKEY_LOCAL_MACHINE, IFEO_KEY, false, false)? else {
        return Ok(Status::Inactive);
    };
    let state = classify(read_value(&key, "Debugger")?.as_ref(), &command);
    // A path filter may prevent the normal Debugger value from taking effect.
    if state != Status::Active {
        if let Err(detail) = ensure_no_filter(&key) {
            return Ok(Status::Other(detail));
        }
    }
    Ok(state)
}

/// Called only by an explicit UI action, on the action worker rather than the UI thread.
pub fn run_elevated(enable: bool) -> Result<(), String> {
    let path = std::env::current_exe()
        .map_err(|err| format!("현재 실행 파일을 찾을 수 없습니다: {err}"))?;
    let file = wide(path.as_os_str());
    let verb = wide("runas");
    let parameters = wide(if enable {
        "--install-task-manager"
    } else {
        "--restore-task-manager"
    });
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Err("관리자 권한 요청이 취소되어 연결 설정을 변경하지 않았습니다.".into());
        }
        return Err(error("관리자 권한으로 연결 설정을 실행할 수 없습니다"));
    }
    if info.hProcess.is_null() {
        return Err("연결 설정 프로세스를 확인할 수 없습니다.".into());
    }
    let process = Process(info.hProcess);
    if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
        return Err(error("연결 설정의 완료를 확인할 수 없습니다"));
    }
    let mut exit = 0;
    if unsafe { GetExitCodeProcess(process.0, &mut exit) } == 0 {
        return Err(error("연결 설정 결과를 확인할 수 없습니다"));
    }
    if exit != 0 {
        return Err(
            "연결 설정을 완료하지 못했습니다. 관리자 창에 표시된 오류를 확인하세요.".into(),
        );
    }
    let verified = if enable {
        status()? == Status::Active
    } else {
        // Restore removes only our Debugger value; unrelated filters may remain.
        match open_key(HKEY_LOCAL_MACHINE, IFEO_KEY, false, false)? {
            Some(key) => read_value(&key, "Debugger")?.is_none(),
            None => true,
        }
    };
    if verified {
        Ok(())
    } else {
        Err("연결 설정이 다른 프로그램에 의해 변경되었습니다. 현재 상태를 다시 확인하세요.".into())
    }
}

/// Entry point for the UAC helper. Does not launch a UI or run on normal app startup.
pub fn apply(enable: bool) -> Result<(), String> {
    let target = install_path()?;
    let command = debugger_command(&target)?;
    // Refuse a foreign replacement before even copying the application.
    if let Some(key) = open_key(HKEY_LOCAL_MACHINE, IFEO_KEY, false, false)? {
        ensure_owned_or_empty(&key, &command)?;
        if enable {
            ensure_no_filter(&key)?;
        }
    }
    if enable {
        install_executable(&target)?;
    }
    change_registration(HKEY_LOCAL_MACHINE, IFEO_KEY, &command, enable)
}

fn ensure_owned_or_empty(key: &Key, command: &str) -> Result<(), String> {
    if matches!(
        classify(read_value(key, "Debugger")?.as_ref(), command),
        Status::Other(_)
    ) {
        return Err("다른 프로그램의 작업 관리자 연결이 이미 있습니다. 기존 프로그램에서 먼저 기본 작업 관리자로 복원하세요. 기존 설정은 변경하지 않았습니다.".into());
    }
    Ok(())
}

fn change_registration(root: HKEY, path: &str, command: &str, enable: bool) -> Result<(), String> {
    let Some(key) = open_key(root, path, true, enable)? else {
        return Ok(());
    };
    // Recheck with the writable handle after installation: another application may
    // have changed this value while copying. Other values and subkeys are preserved.
    ensure_owned_or_empty(&key, command)?;
    if enable {
        ensure_no_filter(&key)?;
    }
    let name = wide("Debugger");
    let result = if enable {
        let value = string_value(command);
        unsafe {
            RegSetValueExW(
                key.0,
                name.as_ptr(),
                0,
                value.kind,
                value.bytes.as_ptr(),
                value.bytes.len() as u32,
            )
        }
    } else {
        unsafe { RegDeleteValueW(key.0, name.as_ptr()) }
    };
    if result == ERROR_SUCCESS || (!enable && result == ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err(registry_error(result))
    }
}

fn sid(kind: WELL_KNOWN_SID_TYPE) -> Result<Vec<u32>, String> {
    let mut value = vec![0_u32; (SECURITY_MAX_SID_SIZE as usize).div_ceil(4)];
    let mut size = (value.len() * 4) as u32;
    if unsafe { CreateWellKnownSid(kind, null_mut(), value.as_mut_ptr().cast(), &mut size) } == 0 {
        return Err(error("설치 폴더의 보안 식별자를 만들 수 없습니다"));
    }
    Ok(value)
}

/// Protected ACL: administrators and SYSTEM may modify; standard users may run.
/// Keeping these allocations alive keeps every pointer in the descriptor valid.
struct DirectorySecurity {
    descriptor: SECURITY_DESCRIPTOR,
    _acl: Vec<u32>,
    _admins: Vec<u32>,
    _system: Vec<u32>,
    _users: Vec<u32>,
}

impl DirectorySecurity {
    fn new() -> Result<Self, String> {
        let admins = sid(WinBuiltinAdministratorsSid)?;
        let system = sid(WinLocalSystemSid)?;
        let users = sid(WinBuiltinUsersSid)?;
        let mut acl = vec![0_u32; 128];
        let acl_ptr = acl.as_mut_ptr().cast::<ACL>();
        if unsafe { InitializeAcl(acl_ptr, (acl.len() * 4) as u32, ACL_REVISION) } == 0 {
            return Err(error("설치 폴더의 접근 권한을 만들 수 없습니다"));
        }
        for (identity, access) in [
            (&admins, FILE_ALL_ACCESS),
            (&system, FILE_ALL_ACCESS),
            (&users, FILE_GENERIC_READ | FILE_GENERIC_EXECUTE),
        ] {
            if unsafe {
                AddAccessAllowedAceEx(
                    acl_ptr,
                    ACL_REVISION,
                    OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                    access,
                    identity.as_ptr().cast_mut().cast(),
                )
            } == 0
            {
                return Err(error("설치 폴더의 접근 권한을 만들 수 없습니다"));
            }
        }
        let mut descriptor = SECURITY_DESCRIPTOR::default();
        let raw = (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast();
        if unsafe { InitializeSecurityDescriptor(raw, 1) } == 0
            || unsafe { SetSecurityDescriptorOwner(raw, admins.as_ptr().cast_mut().cast(), 0) } == 0
            || unsafe { SetSecurityDescriptorGroup(raw, admins.as_ptr().cast_mut().cast(), 0) } == 0
            || unsafe { SetSecurityDescriptorDacl(raw, 1, acl_ptr, 0) } == 0
            || unsafe { SetSecurityDescriptorControl(raw, SE_DACL_PROTECTED, SE_DACL_PROTECTED) }
                == 0
        {
            return Err(error("설치 폴더의 보안 정보를 만들 수 없습니다"));
        }
        Ok(Self {
            descriptor,
            _acl: acl,
            _admins: admins,
            _system: system,
            _users: users,
        })
    }
}

fn open_directory(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        // Holding this handle prevents a directory rename while paths are used.
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|err| format!("설치 폴더를 열 수 없습니다 ({}): {err}", path.display()))?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(error("설치 폴더를 확인할 수 없습니다"));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        return Err("설치 경로에 폴더 연결 또는 다른 파일이 있어 연결 설정을 중단했습니다.".into());
    }
    Ok(file)
}

fn protected_directory(file: &File, require_admin_owner: bool) -> Result<(), String> {
    let mut size = 0;
    let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    unsafe { GetKernelObjectSecurity(file.as_raw_handle(), information, null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(error("설치 파일의 보안 정보를 읽을 수 없습니다"));
    }
    let mut descriptor = vec![0_u32; (size as usize).div_ceil(4)];
    let raw = descriptor.as_mut_ptr().cast();
    if unsafe { GetKernelObjectSecurity(file.as_raw_handle(), information, raw, size, &mut size) }
        == 0
    {
        return Err(error("설치 파일의 보안 정보를 읽을 수 없습니다"));
    }
    // Windows returned a valid self-relative descriptor in the live allocation.
    unsafe { validate_security_descriptor(raw, require_admin_owner) }
}

unsafe fn validate_security_descriptor(
    raw: *mut std::ffi::c_void,
    require_admin_owner: bool,
) -> Result<(), String> {
    let admins = sid(WinBuiltinAdministratorsSid)?;
    let system = sid(WinLocalSystemSid)?;
    // Windows Modules Installer's well-known service SID (S-1-5-80-...).
    let trusted_installer = [
        0x0000_0601_u32,
        0x0500_0000,
        80,
        956008885,
        3418522649,
        1831038044,
        1853292631,
        2271478464,
    ];
    let trusted = |candidate| unsafe {
        EqualSid(candidate, admins.as_ptr().cast_mut().cast()) != 0
            || EqualSid(candidate, system.as_ptr().cast_mut().cast()) != 0
            || (!require_admin_owner
                && EqualSid(candidate, trusted_installer.as_ptr().cast_mut().cast()) != 0)
    };
    let mut owner = null_mut();
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorOwner(raw, &mut owner, &mut defaulted) } == 0
        || owner.is_null()
        || !trusted(owner)
    {
        return Err("설치 폴더의 소유자가 관리자 또는 Windows 시스템이 아닙니다. 안전한 Program Files 폴더가 필요합니다.".into());
    }
    let mut present = 0;
    let mut acl = null_mut();
    if unsafe { GetSecurityDescriptorDacl(raw, &mut present, &mut acl, &mut defaulted) } == 0
        || present == 0
        || acl.is_null()
    {
        return Err("설치 폴더가 일반 사용자의 쓰기 접근으로부터 보호되어 있지 않습니다.".into());
    }
    const MUTATING: u32 = GENERIC_ALL
        | GENERIC_WRITE
        | DELETE
        | WRITE_DAC
        | WRITE_OWNER
        | FILE_WRITE_DATA
        | FILE_APPEND_DATA
        | FILE_WRITE_EA
        | FILE_WRITE_ATTRIBUTES
        | FILE_DELETE_CHILD;
    for index in 0..unsafe { (*acl).AceCount } {
        let mut raw_ace = null_mut();
        if unsafe { GetAce(acl, index as u32, &mut raw_ace) } == 0 {
            return Err(error("설치 폴더의 접근 권한을 확인할 수 없습니다"));
        }
        let header = unsafe { &*raw_ace.cast::<ACE_HEADER>() };
        // Program Files may inherit Creator Owner rules. The app's own folder and
        // EXE must also reject rules that would grant write access to children.
        if !require_admin_owner && u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match header.AceType {
            0 => {
                let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
                let identity = (&ace.SidStart as *const u32).cast_mut().cast();
                if ace.Mask & MUTATING != 0 && !trusted(identity) {
                    return Err(
                        "설치 폴더에 일반 사용자의 쓰기 권한이 있어 연결 설정을 중단했습니다."
                            .into(),
                    );
                }
            }
            1 => {} // Deny ACEs never grant a write permission.
            _ => return Err("설치 폴더에 확인할 수 없는 접근 권한 규칙이 있습니다.".into()),
        }
    }
    Ok(())
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, String> {
    let mut left =
        File::open(left).map_err(|err| format!("실행 파일을 읽을 수 없습니다: {err}"))?;
    let mut right =
        File::open(right).map_err(|err| format!("설치된 실행 파일을 읽을 수 없습니다: {err}"))?;
    if left.metadata().map_err(|err| err.to_string())?.len()
        != right.metadata().map_err(|err| err.to_string())?.len()
    {
        return Ok(false);
    }
    let mut a = [0_u8; 16_384];
    let mut b = [0_u8; 16_384];
    loop {
        let len = left.read(&mut a).map_err(|err| err.to_string())?;
        if len == 0 {
            return Ok(true);
        }
        right
            .read_exact(&mut b[..len])
            .map_err(|err| err.to_string())?;
        if a[..len] != b[..len] {
            return Ok(false);
        }
    }
}

fn open_protected_executable(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|err| format!("설치된 실행 파일을 확인할 수 없습니다: {err}"))?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(error("설치된 실행 파일을 확인할 수 없습니다"));
    }
    if info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0 {
        return Err("설치할 실행 파일 위치에 링크 또는 폴더가 있어 중단했습니다.".into());
    }
    protected_directory(&file, true)?;
    Ok(file)
}

fn install_executable(target: &Path) -> Result<(), String> {
    let source = std::env::current_exe().map_err(|err| err.to_string())?;
    if source
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("taskmgr.exe"))
    {
        return Err("실행 파일 이름이 taskmgr.exe이면 Windows 실행 연결이 반복될 수 있습니다. FeatherTaskManager.exe로 이름을 복원하세요.".into());
    }
    let folder = target.parent().ok_or("설치 폴더가 없습니다.")?;
    let program_files = folder.parent().ok_or("Program Files 폴더가 없습니다.")?;
    // Lock each existing ancestor and reject junctions/symlinks before creating.
    let mut locks = Vec::new();
    for ancestor in program_files
        .ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        locks.push(open_directory(ancestor)?);
    }
    protected_directory(locks.last().ok_or("Program Files 폴더가 없습니다.")?, false)?;
    let mut security = DirectorySecurity::new()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut security.descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(wide(folder).as_ptr(), &attributes) } == 0
        && std::io::Error::last_os_error().raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32)
    {
        return Err(error("Program Files에 설치하려면 관리자 권한이 필요합니다"));
    }
    let folder_lock = open_directory(folder)?;
    protected_directory(&folder_lock, true)?;
    if let Ok(metadata) = std::fs::symlink_metadata(target) {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY)
            != 0
        {
            return Err("설치할 실행 파일 위치에 링크 또는 폴더가 있어 중단했습니다.".into());
        }
        // Matching bytes do not make a user-writable binary safe for global IFEO.
        let _target_lock = open_protected_executable(target)?;
        if files_equal(&source, target)? {
            return Ok(());
        }
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| err.to_string())?
        .as_nanos();
    let temporary = folder.join(format!(
        "FeatherTaskManager-{}-{stamp}.tmp",
        std::process::id()
    ));
    let mut temporary_created = false;
    let result = (|| {
        let mut input =
            File::open(&source).map_err(|err| format!("실행 파일을 읽을 수 없습니다: {err}"))?;
        // Apply an explicit protected ACL to the file too: existing directory
        // inheritance is never allowed to make the elevated launch target mutable.
        let raw = unsafe {
            CreateFileW(
                wide(&temporary).as_ptr(),
                GENERIC_WRITE | READ_CONTROL,
                FILE_SHARE_READ,
                &attributes,
                CREATE_NEW,
                0,
                null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(error("설치 파일을 만들 수 없습니다"));
        }
        temporary_created = true;
        let mut output = unsafe { File::from_raw_handle(raw) };
        protected_directory(&output, true)?;
        std::io::copy(&mut input, &mut output)
            .map_err(|err| format!("실행 파일을 복사할 수 없습니다: {err}"))?;
        output.flush().map_err(|err| err.to_string())?;
        output.sync_all().map_err(|err| err.to_string())?;
        drop(output);
        if unsafe {
            MoveFileExW(
                wide(&temporary).as_ptr(),
                wide(target).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(format!(
                "{}. 설치된 Feather Task Manager가 실행 중이면 닫고 다시 시도하세요.",
                error("설치 파일을 교체할 수 없습니다")
            ));
        }
        open_protected_executable(target)?;
        Ok(())
    })();
    if result.is_err() && temporary_created {
        // Only this invocation's create_new staging file is ever removed.
        let _ = std::fs::remove_file(&temporary);
    }
    drop(folder_lock);
    drop(locks);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER, REG_BINARY};

    #[test]
    fn commands_quote_paths_and_refuse_recursive_names() {
        assert_eq!(
            debugger_command(Path::new(
                r"C:\Program Files\Feather Task Manager\FeatherTaskManager.exe"
            ))
            .unwrap(),
            r#""C:\Program Files\Feather Task Manager\FeatherTaskManager.exe" --task-manager"#
        );
        assert!(debugger_command(Path::new(r"C:\Windows\TASKMGR.EXE")).is_err());
        assert!(debugger_command(Path::new("relative.exe")).is_err());
        assert!(debugger_command(Path::new("C:\\bad\"name.exe")).is_err());
    }

    #[test]
    fn only_exact_well_formed_owned_values_are_active() {
        let command =
            r#""C:\Program Files\Feather Task Manager\FeatherTaskManager.exe" --task-manager"#;
        assert_eq!(classify(None, command), Status::Inactive);
        assert_eq!(
            classify(Some(&string_value(command)), command),
            Status::Active
        );
        for value in [
            string_value(""),
            string_value(&format!("{command} --extra")),
            Value {
                kind: REG_BINARY,
                bytes: string_value(command).bytes,
            },
            Value {
                kind: REG_SZ,
                bytes: vec![65, 0],
            },
            Value {
                kind: REG_SZ,
                bytes: vec![65, 0, 0],
            },
            Value {
                kind: REG_SZ,
                bytes: vec![0, 216, 0, 0],
            },
            string_value(&format!("{command}\0hidden")),
        ] {
            assert!(matches!(classify(Some(&value), command), Status::Other(_)));
        }
    }

    #[test]
    fn known_install_location_and_security_descriptor_are_valid() {
        let target = install_path().unwrap();
        assert!(target.is_absolute());
        assert_eq!(target.file_name().unwrap(), APP_EXE);
        let mut security = DirectorySecurity::new().unwrap();
        assert_ne!(
            unsafe {
                windows_sys::Win32::Security::IsValidSecurityDescriptor(
                    (&mut security.descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                )
            },
            0
        );
        // Read-only inspection: never creates the production installation folder.
        let folder = open_directory(target.parent().unwrap().parent().unwrap()).unwrap();
        protected_directory(&folder, false).unwrap();
    }

    #[test]
    fn install_security_rejects_user_owned_or_user_writable_files_and_children() {
        let users = sid(WinBuiltinUsersSid).unwrap();
        for flags in [0, OBJECT_INHERIT_ACE | INHERIT_ONLY_ACE] {
            let mut security = DirectorySecurity::new().unwrap();
            let raw = (&mut security.descriptor as *mut SECURITY_DESCRIPTOR).cast();
            assert!(unsafe { validate_security_descriptor(raw, true) }.is_ok());
            assert_ne!(
                unsafe {
                    AddAccessAllowedAceEx(
                        security._acl.as_mut_ptr().cast(),
                        ACL_REVISION,
                        flags,
                        FILE_WRITE_DATA,
                        users.as_ptr().cast_mut().cast(),
                    )
                },
                0
            );
            assert!(unsafe { validate_security_descriptor(raw, true) }.is_err());
        }
        let mut security = DirectorySecurity::new().unwrap();
        let raw = (&mut security.descriptor as *mut SECURITY_DESCRIPTOR).cast();
        assert_ne!(
            unsafe { SetSecurityDescriptorOwner(raw, users.as_ptr().cast_mut().cast(), 0) },
            0
        );
        assert!(unsafe { validate_security_descriptor(raw, true) }.is_err());
    }

    struct TestKey(String);
    impl TestKey {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            Self(format!(
                r"Software\FeatherTask\Tests\Replacement-{}-{stamp}",
                std::process::id()
            ))
        }
    }
    impl Drop for TestKey {
        fn drop(&mut self) {
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.0).as_ptr()) };
        }
    }

    fn set(key: &Key, name: &str, value: &Value) {
        assert_eq!(
            unsafe {
                RegSetValueExW(
                    key.0,
                    wide(name).as_ptr(),
                    0,
                    value.kind,
                    value.bytes.as_ptr(),
                    value.bytes.len() as u32,
                )
            },
            ERROR_SUCCESS
        );
    }

    #[test]
    fn registration_round_trip_preserves_unrelated_values_and_foreign_commands() {
        let test = TestKey::new();
        let command =
            r#""C:\Program Files\Feather Task Manager\FeatherTaskManager.exe" --task-manager"#;
        let key = open_key(HKEY_CURRENT_USER, &test.0, true, true)
            .unwrap()
            .unwrap();
        let unrelated = string_value("Keep this setting");
        set(&key, "Unrelated", &unrelated);
        change_registration(HKEY_CURRENT_USER, &test.0, command, true).unwrap();
        assert_eq!(
            classify(read_value(&key, "Debugger").unwrap().as_ref(), command),
            Status::Active
        );
        change_registration(HKEY_CURRENT_USER, &test.0, command, true).unwrap();
        change_registration(HKEY_CURRENT_USER, &test.0, command, false).unwrap();
        assert_eq!(read_value(&key, "Debugger").unwrap(), None);
        assert_eq!(read_value(&key, "Unrelated").unwrap(), Some(unrelated));
        for foreign in [
            string_value("other-manager.exe"),
            Value {
                kind: REG_BINARY,
                bytes: vec![1, 2, 3],
            },
        ] {
            set(&key, "Debugger", &foreign);
            assert!(change_registration(HKEY_CURRENT_USER, &test.0, command, true).is_err());
            assert!(change_registration(HKEY_CURRENT_USER, &test.0, command, false).is_err());
            assert_eq!(read_value(&key, "Debugger").unwrap(), Some(foreign));
        }
    }

    #[test]
    fn path_filtered_registrations_are_never_overwritten() {
        let test = TestKey::new();
        let key = open_key(HKEY_CURRENT_USER, &test.0, true, true)
            .unwrap()
            .unwrap();
        set(
            &key,
            "UseFilter",
            &Value {
                kind: REG_DWORD,
                bytes: 1_u32.to_le_bytes().to_vec(),
            },
        );
        assert!(change_registration(HKEY_CURRENT_USER, &test.0, "owned", true).is_err());
        assert_eq!(read_value(&key, "Debugger").unwrap(), None);
        // Recovery still removes only our value if filters were added afterward.
        set(&key, "Debugger", &string_value("owned"));
        change_registration(HKEY_CURRENT_USER, &test.0, "owned", false).unwrap();
        assert_eq!(read_value(&key, "Debugger").unwrap(), None);
        assert_eq!(
            read_value(&key, "UseFilter").unwrap().unwrap().bytes,
            1_u32.to_le_bytes()
        );
    }

    #[test]
    fn even_disabled_path_filter_subkeys_are_preserved() {
        let test = TestKey::new();
        let child = open_key(
            HKEY_CURRENT_USER,
            &format!("{}\\Filter", test.0),
            true,
            true,
        )
        .unwrap()
        .unwrap();
        let filter = string_value(r"C:\Windows\System32\taskmgr.exe");
        set(&child, "FilterFullPath", &filter);
        assert!(change_registration(HKEY_CURRENT_USER, &test.0, "owned", true).is_err());
        assert_eq!(read_value(&child, "FilterFullPath").unwrap(), Some(filter));
    }

    /// Run explicitly from an already elevated test shell. This exercises file
    /// installation beneath target/ only; it never touches IFEO or Program Files.
    #[test]
    #[ignore = "requires an already elevated token; creates only a disposable target/test-install-* directory"]
    fn disposable_installation_copies_secures_and_updates_the_binary() {
        use windows_sys::Win32::{
            Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY},
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        };
        let mut raw_token = null_mut();
        assert_ne!(
            unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) },
            0
        );
        let token = Process(raw_token);
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0;
        assert_ne!(
            unsafe {
                GetTokenInformation(
                    token.0,
                    TokenElevation,
                    (&mut elevation as *mut TOKEN_ELEVATION).cast(),
                    size_of::<TOKEN_ELEVATION>() as u32,
                    &mut size,
                )
            },
            0
        );
        assert_ne!(
            elevation.TokenIsElevated, 0,
            "Run this ignored test only from an already elevated shell; it never requests UAC."
        );

        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .canonicalize()
            .unwrap();
        let base = workspace.join("target").canonicalize().unwrap();
        assert!(base.is_absolute() && base.starts_with(&workspace));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = base.join(format!("test-install-{}-{stamp}", std::process::id()));
        assert_eq!(root.parent(), Some(base.as_path()));
        assert!(!root.exists());

        // All cleanup names are fixed, checked children of this test's fresh root.
        // No recursive delete is used, so any unexpected contents are preserved.
        struct Cleanup {
            root: PathBuf,
            base: PathBuf,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                if self.root.is_absolute()
                    && self.root.parent() == Some(self.base.as_path())
                    && self
                        .root
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("test-install-"))
                {
                    let app = self.root.join(APP_FOLDER);
                    let _ = std::fs::remove_file(app.join(APP_EXE));
                    let _ = std::fs::remove_dir(app);
                    let _ = std::fs::remove_dir(&self.root);
                }
            }
        }
        let mut security = DirectorySecurity::new().unwrap();
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: (&mut security.descriptor as *mut SECURITY_DESCRIPTOR).cast(),
            bInheritHandle: 0,
        };
        assert_ne!(
            unsafe { CreateDirectoryW(wide(&root).as_ptr(), &attributes) },
            0,
            "Cannot create the disposable protected test folder: {}",
            std::io::Error::last_os_error()
        );
        let cleanup = Cleanup {
            root: root.clone(),
            base,
        };
        let target = root.join(APP_FOLDER).join(APP_EXE);
        let source = std::env::current_exe().unwrap();
        install_executable(&target).unwrap();
        assert!(files_equal(&source, &target).unwrap());
        drop(open_protected_executable(&target).unwrap());
        install_executable(&target).unwrap(); // Same bytes must be idempotent.
        std::fs::write(&target, b"old disposable build").unwrap();
        install_executable(&target).unwrap(); // Stage and replace a different build.
        assert!(files_equal(&source, &target).unwrap());
        drop(open_protected_executable(&target).unwrap());
        drop(cleanup);
        assert!(
            !root.exists(),
            "The disposable installation did not clean up fully."
        );
    }
}
