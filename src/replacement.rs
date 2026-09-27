//! Explicit, reversible Task Manager registration. No registry polling or startup hooks.
//!
//! Windows' IFEO Debugger value redirects taskmgr.exe. The command always names a
//! installed image under the system's trusted Program Files folder. Registration
//! never copies or elevates an executable from a portable, user-writable path.

use crate::i18n::tr;

use std::{
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    mem::size_of,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CANCELLED, ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, GENERIC_ALL, GENERIC_WRITE, HANDLE, WAIT_OBJECT_0,
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
        CreateDirectoryW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, DELETE,
        FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_DELETE_CHILD, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, READ_CONTROL,
        WRITE_DAC, WRITE_OWNER,
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
    crate::trf!(
        "작업 관리자 연결 설정에 접근할 수 없습니다: {}",
        "Cannot access the Task Manager association settings: {}",
        std::io::Error::from_raw_os_error(code as i32)
    )
}

fn install_path() -> Result<PathBuf, String> {
    let mut raw = null_mut();
    let result =
        unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFilesX64, 0, null_mut(), &mut raw) };
    if result < 0 || raw.is_null() {
        return Err(crate::trf!(
            "Windows Program Files 폴더를 확인할 수 없습니다 (0x{:08X}).",
            "Cannot locate the Windows Program Files folder (0x{:08X}).",
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
        return Err(tr(
            "Windows Program Files 폴더가 절대 경로가 아닙니다.",
            "The Windows Program Files folder is not an absolute path.",
        )
        .into());
    }
    path.push(APP_FOLDER);
    path.push(APP_EXE);
    Ok(path)
}

fn debugger_command(path: &Path) -> Result<String, String> {
    let text = path.to_str().ok_or(tr(
        "설치 경로를 유니코드로 읽을 수 없습니다.",
        "The installation path contains invalid Unicode.",
    ))?;
    if !path.is_absolute()
        || text.contains(['\0', '"'])
        || path
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("taskmgr.exe"))
    {
        return Err(tr(
            "작업 관리자 연결에 사용할 수 없는 실행 파일 경로입니다.",
            "This executable path cannot be used for the Task Manager association.",
        )
        .into());
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
        return Err(tr(
            "기존 작업 관리자 연결 값이 너무 커서 안전하게 변경할 수 없습니다.",
            "The existing Task Manager association value is too large to change safely.",
        )
        .into());
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
                .unwrap_or_else(|| {
                    tr(
                        "알 수 없는 형식의 Debugger 설정",
                        "Unrecognized Debugger setting format",
                    )
                    .into()
                }),
        ),
    }
}

fn ensure_no_filter(key: &Key) -> Result<(), String> {
    if let Some(value) = read_value(key, "UseFilter")? {
        if value.kind != REG_DWORD || value.bytes != 0_u32.to_le_bytes() {
            return Err(tr("taskmgr.exe에 다른 프로그램의 경로별 실행 필터가 설정되어 있습니다. 해당 프로그램에서 먼저 연결을 해제하세요.", "Another application has configured a path-specific execution filter for taskmgr.exe. Remove its association in that application first.").into());
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
        return Err(tr("taskmgr.exe에 다른 프로그램의 경로별 설정이 있습니다. 해당 프로그램에서 먼저 연결을 해제하세요.", "Another application has configured path-specific settings for taskmgr.exe. Remove its association in that application first.").into());
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

/// How [`runas_and_wait`] ended.
enum Launch {
    Exited(u32),
    Cancelled,
    StartFailed(std::io::Error),
    NoProcess,
    WaitFailed(std::io::Error),
    ExitUnknown(std::io::Error),
}

/// Why [`runas`] started no process.
enum StartFailure {
    Cancelled,
    Failed(std::io::Error),
    NoProcess,
}

/// Start `file` elevated (UAC "runas") with `parameters`. `SEE_MASK_NOASYNC`
/// returns only once the start is complete, so a caller may exit right away,
/// and the process handle proves that a process really started.
fn runas(file: &Path, parameters: &str) -> Result<Process, StartFailure> {
    let file = wide(file.as_os_str());
    let verb = wide("runas");
    let parameters = wide(parameters);
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
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED as i32) {
            return Err(StartFailure::Cancelled);
        }
        return Err(StartFailure::Failed(error));
    }
    if info.hProcess.is_null() {
        return Err(StartFailure::NoProcess);
    }
    Ok(Process(info.hProcess))
}

/// Start `file` elevated (UAC "runas") with `parameters` and wait for it.
fn runas_and_wait(file: &Path, parameters: &str) -> Launch {
    let process = match runas(file, parameters) {
        Ok(process) => process,
        Err(StartFailure::Cancelled) => return Launch::Cancelled,
        Err(StartFailure::Failed(error)) => return Launch::StartFailed(error),
        Err(StartFailure::NoProcess) => return Launch::NoProcess,
    };
    if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
        return Launch::WaitFailed(std::io::Error::last_os_error());
    }
    let mut exit = 0;
    if unsafe { GetExitCodeProcess(process.0, &mut exit) } == 0 {
        return Launch::ExitUnknown(std::io::Error::last_os_error());
    }
    Launch::Exited(exit)
}

/// Called only by an explicit UI action, on the action worker rather than the UI thread.
pub fn run_elevated(enable: bool) -> Result<(), String> {
    let path = install_path()?;
    // Pin every path component and the protected installed image until the
    // elevated helper finishes. A portable EXE can be renamed while running;
    // its current_exe() pathname is not a trusted source for an elevated launch.
    let _installation = lock_existing_installation(&path).map_err(installation_required)?;
    let parameters = format!(
        "{} --language {}",
        if enable {
            "--install-task-manager"
        } else {
            "--restore-task-manager"
        },
        crate::i18n::language().code()
    );
    let context = |message: &str, error: std::io::Error| format!("{message}: {error}");
    let exit = match runas_and_wait(&path, &parameters) {
        Launch::Exited(exit) => exit,
        Launch::Cancelled => {
            return Err(tr(
                "관리자 권한 요청이 취소되어 연결 설정을 변경하지 않았습니다.",
                "The administrator request was cancelled. The association was not changed.",
            )
            .into())
        }
        Launch::StartFailed(error) => {
            return Err(context(
                tr(
                    "관리자 권한으로 연결 설정을 실행할 수 없습니다",
                    "Cannot run the association setup as administrator",
                ),
                error,
            ))
        }
        Launch::NoProcess => {
            return Err(tr(
                "연결 설정 프로세스를 확인할 수 없습니다.",
                "Cannot verify the association setup process.",
            )
            .into())
        }
        Launch::WaitFailed(error) => {
            return Err(context(
                tr(
                    "연결 설정의 완료를 확인할 수 없습니다",
                    "Cannot verify that the association setup completed",
                ),
                error,
            ))
        }
        Launch::ExitUnknown(error) => {
            return Err(context(
                tr(
                    "연결 설정 결과를 확인할 수 없습니다",
                    "Cannot read the association setup result",
                ),
                error,
            ))
        }
    };
    if exit != 0 {
        return Err(
            tr("연결 설정을 완료하지 못했습니다. 관리자 창에 표시된 오류를 확인하세요.", "The association setup did not complete. Check the error shown in the administrator window.").into(),
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
        Err(tr(
            "연결 설정이 다른 프로그램에 의해 변경되었습니다. 현재 상태를 다시 확인하세요.",
            "Another application changed the association. Check its current state again.",
        )
        .into())
    }
}

/// How an elevated helper run from the installed image ended.
#[derive(Debug, PartialEq, Eq)]
pub enum HelperLaunch {
    Exited(u32),
    /// The UAC prompt was declined.
    Cancelled,
}

/// Run the protected installed image elevated with `arguments` (plus the UI
/// language) and wait for its exit code, like the association helpers: the
/// same pinned Program Files image, never a portable current_exe(). It must
/// be this very build, byte for byte, so an older installation (which would
/// open its UI for an argument it does not know) is never started. Worker
/// thread only.
pub fn run_installed_helper(arguments: &str) -> Result<HelperLaunch, String> {
    let path = install_path()?;
    let _installation = lock_existing_installation(&path).map_err(helper_installation_required)?;
    if !same_build(&path)? {
        return Err(tr(
            "설치된 Feather Task Manager가 이 실행 파일과 다른 버전입니다. 같은 버전을 설치하거나 Feather를 관리자 권한으로 실행하세요.",
            "The installed Feather Task Manager is a different build from this one. Install this version, or run Feather as administrator.",
        )
        .into());
    }
    let parameters = format!("{arguments} --language {}", crate::i18n::language().code());
    match runas_and_wait(&path, &parameters) {
        Launch::Exited(code) => Ok(HelperLaunch::Exited(code)),
        Launch::Cancelled => Ok(HelperLaunch::Cancelled),
        Launch::StartFailed(error) | Launch::WaitFailed(error) | Launch::ExitUnknown(error) => {
            Err(crate::trf!(
                "관리자 도우미를 실행하지 못했습니다: {}",
                "Cannot run the administrator helper: {}",
                error
            ))
        }
        Launch::NoProcess => Err(tr(
            "관리자 도우미 프로세스를 확인할 수 없습니다.",
            "Cannot verify the administrator helper process.",
        )
        .into()),
    }
}

/// How [`relaunch_installed_elevated`] ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Relaunch {
    /// The elevated instance started.
    Started,
    /// The UAC prompt was declined.
    Cancelled,
}

/// The installed image that "Always run as administrator" may start
/// elevated, with its path and image pinned: the protected Program Files
/// copy of this very build, like the helpers. A portable current_exe() is
/// never elevated automatically (it can be renamed and replaced while it
/// runs, and the setting would repeat that at every start).
fn installed_relaunch_target() -> Result<(PathBuf, InstallationLocks), String> {
    let path = install_path().map_err(always_admin_needs_installation)?;
    let locks = lock_existing_installation(&path).map_err(always_admin_needs_installation)?;
    if !same_build(&path).map_err(always_admin_needs_installation)? {
        return Err(tr(
            "항상 관리자 권한으로 실행은 이 버전과 같은 설치된 Feather에만 적용됩니다. 이 버전을 설치하세요.",
            "Always run as administrator applies only to an installed Feather of this same version. Install this version.",
        )
        .into());
    }
    Ok((path, locks))
}

/// Whether "Always run as administrator" can start the installed image
/// (Settings, when the switch is turned on); Err says why not. Read-only.
pub fn check_installed_relaunch() -> Result<(), String> {
    installed_relaunch_target().map(|_| ())
}

/// "Always run as administrator": start the pinned installed image of this
/// build through the UAC consent prompt with the fixed `arguments`, and
/// return once it has started (no wait; the caller exits). The locks are
/// held until the elevated process exists.
pub fn relaunch_installed_elevated(arguments: &[&'static str]) -> Result<Relaunch, String> {
    let (path, _installation) = installed_relaunch_target()?;
    match runas(&path, &arguments.join(" ")) {
        Ok(_process) => Ok(Relaunch::Started),
        Err(StartFailure::Cancelled) => Ok(Relaunch::Cancelled),
        Err(StartFailure::Failed(error)) => Err(crate::trf!(
            "관리자 권한으로 시작할 수 없습니다: {}",
            "Cannot start as administrator: {}",
            error
        )),
        Err(StartFailure::NoProcess) => Err(tr(
            "관리자 권한 프로세스를 확인할 수 없습니다.",
            "Cannot verify the administrator process.",
        )
        .into()),
    }
}

fn always_admin_needs_installation(detail: String) -> String {
    crate::trf!(
        "항상 관리자 권한으로 실행은 설치된 Feather에만 적용됩니다. 보호된 설치 파일을 확인할 수 없습니다: {detail}",
        "Always run as administrator applies only to the installed Feather. The protected installed executable could not be verified: {detail}"
    )
}

fn helper_installation_required(detail: String) -> String {
    crate::trf!(
        "관리자 작업에는 설치된 Feather Task Manager가 필요합니다(또는 Feather를 관리자 권한으로 실행하세요). 보호된 설치 파일을 확인할 수 없습니다: {detail}",
        "Administrator steps need the installed Feather Task Manager (or run Feather as administrator). The protected installed executable could not be verified: {detail}"
    )
}

/// Whether the installed image is the running build: the same file, or a
/// byte-identical copy of it.
fn same_build(installed: &Path) -> Result<bool, String> {
    use std::io::Read;
    let unreadable = |error: std::io::Error| {
        crate::trf!(
            "실행 파일을 비교할 수 없습니다: {}",
            "Cannot compare the executables: {}",
            error
        )
    };
    let current = std::env::current_exe().map_err(unreadable)?;
    let mut ours = File::open(&current).map_err(unreadable)?;
    let mut theirs = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(installed)
        .map_err(unreadable)?;
    let identity = |file: &File| {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        (unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0).then_some((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
            (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        ))
    };
    let (Some(a), Some(b)) = (identity(&ours), identity(&theirs)) else {
        return Err(unreadable(std::io::Error::last_os_error()));
    };
    if a == b {
        return Ok(true);
    }
    if a.3 != b.3 {
        return Ok(false);
    }
    let (mut left, mut right) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let n = ours.read(&mut left).map_err(unreadable)?;
        if n == 0 {
            // Equal sizes: the other file must end here too.
            return Ok(theirs.read(&mut right[..1]).map_err(unreadable)? == 0);
        }
        theirs.read_exact(&mut right[..n]).map_err(unreadable)?;
        if left[..n] != right[..n] {
            return Ok(false);
        }
    }
}

/// Entry point for the UAC helper. Does not launch a UI or run on normal app startup.
pub fn apply(enable: bool) -> Result<(), String> {
    let target = install_path()?;
    apply_at(HKEY_LOCAL_MACHINE, IFEO_KEY, &target, enable)
}

fn apply_at(root: HKEY, registry_path: &str, target: &Path, enable: bool) -> Result<(), String> {
    let command = debugger_command(target)?;
    if let Some(key) = open_key(root, registry_path, false, false)? {
        ensure_owned_or_empty(&key, &command)?;
        if enable {
            ensure_no_filter(&key)?;
        }
    }
    // Do not create a folder or copy current_exe(): the installer is the only
    // mechanism that deploys the elevated launch target. Keep these locks through
    // the registry write, including when this helper is invoked directly.
    let _installation = enable
        .then(|| lock_existing_installation(target).map_err(installation_required))
        .transpose()?;
    change_registration(root, registry_path, &command, enable)
}

fn installation_required(detail: String) -> String {
    crate::trf!(
        "작업 관리자 연결을 변경하려면 먼저 설치 파일로 Feather Task Manager를 설치하거나 복구하세요. 보호된 설치 파일을 확인할 수 없습니다: {detail}",
        "Install or repair Feather Task Manager using the installer before changing the Task Manager association. The protected installed executable could not be verified: {detail}"
    )
}

fn ensure_owned_or_empty(key: &Key, command: &str) -> Result<(), String> {
    if matches!(
        classify(read_value(key, "Debugger")?.as_ref(), command),
        Status::Other(_)
    ) {
        return Err(tr("다른 프로그램의 작업 관리자 연결이 이미 있습니다. 기존 프로그램에서 먼저 기본 작업 관리자로 복원하세요. 기존 설정은 변경하지 않았습니다.", "Another application is already associated with Task Manager. Restore the Windows default in that application first. The existing settings were not changed.").into());
    }
    Ok(())
}

fn change_registration(root: HKEY, path: &str, command: &str, enable: bool) -> Result<(), String> {
    let Some(key) = open_key(root, path, true, enable)? else {
        return Ok(());
    };
    // Recheck with the writable handle after validating the installed image:
    // another application may have changed the value. Preserve unrelated settings.
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
        return Err(error(tr(
            "설치 폴더의 보안 식별자를 만들 수 없습니다",
            "Cannot create security identifiers for the installation folder",
        )));
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
            return Err(error(tr(
                "설치 폴더의 접근 권한을 만들 수 없습니다",
                "Cannot create access permissions for the installation folder",
            )));
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
                return Err(error(tr(
                    "설치 폴더의 접근 권한을 만들 수 없습니다",
                    "Cannot create access permissions for the installation folder",
                )));
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
            return Err(error(tr(
                "설치 폴더의 보안 정보를 만들 수 없습니다",
                "Cannot create security information for the installation folder",
            )));
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
        .map_err(|err| {
            crate::trf!(
                "설치 폴더를 열 수 없습니다 ({}): {err}",
                "Cannot open the installation folder ({}): {err}",
                path.display()
            )
        })?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(error(tr(
            "설치 폴더를 확인할 수 없습니다",
            "Cannot verify the installation folder",
        )));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        return Err(tr("설치 경로에 폴더 연결 또는 다른 파일이 있어 연결 설정을 중단했습니다.", "Association setup stopped because the installation path contains a directory link or another file.").into());
    }
    Ok(file)
}

fn protected_directory(file: &File, require_admin_owner: bool) -> Result<(), String> {
    let mut size = 0;
    let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
    unsafe { GetKernelObjectSecurity(file.as_raw_handle(), information, null_mut(), 0, &mut size) };
    if size == 0 {
        return Err(error(tr(
            "설치 파일의 보안 정보를 읽을 수 없습니다",
            "Cannot read security information for the installation file",
        )));
    }
    let mut descriptor = vec![0_u32; (size as usize).div_ceil(4)];
    let raw = descriptor.as_mut_ptr().cast();
    if unsafe { GetKernelObjectSecurity(file.as_raw_handle(), information, raw, size, &mut size) }
        == 0
    {
        return Err(error(tr(
            "설치 파일의 보안 정보를 읽을 수 없습니다",
            "Cannot read security information for the installation file",
        )));
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
        return Err(tr("설치 폴더의 소유자가 관리자 또는 Windows 시스템이 아닙니다. 안전한 Program Files 폴더가 필요합니다.", "The installation folder is not owned by administrators or Windows System. A secure Program Files folder is required.").into());
    }
    let mut present = 0;
    let mut acl = null_mut();
    if unsafe { GetSecurityDescriptorDacl(raw, &mut present, &mut acl, &mut defaulted) } == 0
        || present == 0
        || acl.is_null()
    {
        return Err(tr(
            "설치 폴더가 일반 사용자의 쓰기 접근으로부터 보호되어 있지 않습니다.",
            "The installation folder is not protected against write access by standard users.",
        )
        .into());
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
            return Err(error(tr(
                "설치 폴더의 접근 권한을 확인할 수 없습니다",
                "Cannot verify installation folder permissions",
            )));
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
                        tr("설치 폴더에 일반 사용자의 쓰기 권한이 있어 연결 설정을 중단했습니다.", "Association setup stopped because standard users have write access to the installation folder.")
                            .into(),
                    );
                }
            }
            1 => {} // Deny ACEs never grant a write permission.
            _ => {
                return Err(tr(
                    "설치 폴더에 확인할 수 없는 접근 권한 규칙이 있습니다.",
                    "The installation folder contains an access rule that cannot be verified.",
                )
                .into())
            }
        }
    }
    Ok(())
}

fn open_protected_executable(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|err| {
            crate::trf!(
                "설치된 실행 파일을 확인할 수 없습니다: {err}",
                "Cannot verify the installed executable: {err}"
            )
        })?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(error(tr(
            "설치된 실행 파일을 확인할 수 없습니다",
            "Cannot verify the installed executable",
        )));
    }
    if info.dwFileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0 {
        return Err(tr(
            "설치할 실행 파일 위치에 링크 또는 폴더가 있어 중단했습니다.",
            "Installation stopped because the executable destination is a link or a folder.",
        )
        .into());
    }
    protected_directory(&file, true)?;
    Ok(file)
}

fn lock_install_directory(target: &Path) -> Result<(Vec<File>, File), String> {
    let folder = target.parent().ok_or(tr(
        "설치 폴더가 없습니다.",
        "The installation folder is missing.",
    ))?;
    let program_files = folder.parent().ok_or(tr(
        "Program Files 폴더가 없습니다.",
        "The Program Files folder is missing.",
    ))?;
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
    protected_directory(
        locks.last().ok_or(tr(
            "Program Files 폴더가 없습니다.",
            "The Program Files folder is missing.",
        ))?,
        false,
    )?;
    let mut security = DirectorySecurity::new()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: (&mut security.descriptor as *mut SECURITY_DESCRIPTOR).cast(),
        bInheritHandle: 0,
    };
    if unsafe { CreateDirectoryW(wide(folder).as_ptr(), &attributes) } == 0
        && std::io::Error::last_os_error().raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32)
    {
        return Err(error(tr(
            "Program Files에 설치하려면 관리자 권한이 필요합니다",
            "Administrator privileges are required to install in Program Files",
        )));
    }
    let folder_lock = open_directory(folder)?;
    protected_directory(&folder_lock, true)?;
    Ok((locks, folder_lock))
}

/// Installer-only helper. It prepares the fixed protected directory, without
/// copying the application or changing the Task Manager association.
pub fn prepare_install_directory() -> Result<(), String> {
    lock_install_directory(&install_path()?).map(|_| ())
}

/// Verify the installer's extracted executable and its parents before success.
pub fn validate_installation() -> Result<(), String> {
    lock_existing_installation(&install_path()?).map(|_| ())
}

/// All handles are retained together: no directory component or executable may
/// be renamed while an elevated launch or IFEO registration uses its pathname.
struct InstallationLocks {
    _ancestors: Vec<File>,
    _executable: File,
}

fn lock_existing_installation(target: &Path) -> Result<InstallationLocks, String> {
    let folder = target.parent().ok_or(tr(
        "설치 폴더가 없습니다.",
        "The installation folder is missing.",
    ))?;
    let program_files = folder.parent().ok_or(tr(
        "Program Files 폴더가 없습니다.",
        "The Program Files folder is missing.",
    ))?;
    let mut locks = Vec::new();
    for ancestor in program_files
        .ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        locks.push(open_directory(ancestor)?);
    }
    protected_directory(locks.last().unwrap(), false)?;
    let folder_lock = open_directory(folder)?;
    protected_directory(&folder_lock, true)?;
    locks.push(folder_lock);
    let executable = open_protected_executable(target)?;
    Ok(InstallationLocks {
        _ancestors: locks,
        _executable: executable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{with_language, Language};
    use std::time::{SystemTime, UNIX_EPOCH};
    use windows_sys::Win32::System::Registry::{RegDeleteTreeW, HKEY_CURRENT_USER, REG_BINARY};

    #[test]
    fn english_association_errors_and_unknown_status_are_localized() {
        with_language(Language::English, || {
            assert_eq!(
                debugger_command(Path::new("relative.exe")).unwrap_err(),
                "This executable path cannot be used for the Task Manager association."
            );
            assert!(registry_error(ERROR_FILE_NOT_FOUND)
                .starts_with("Cannot access the Task Manager association settings:"));
            let invalid = Value {
                kind: REG_BINARY,
                bytes: vec![1],
            };
            assert_eq!(
                classify(Some(&invalid), "expected"),
                Status::Other("Unrecognized Debugger setting format".into())
            );
        });
    }

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

    #[test]
    fn registration_rejects_portable_image_without_copying_or_changing_registry() {
        let test = TestKey::new();
        let key = open_key(HKEY_CURRENT_USER, &test.0, true, true)
            .unwrap()
            .unwrap();
        let unrelated = string_value("Keep this setting");
        set(&key, "Unrelated", &unrelated);
        // The test harness is deliberately outside a protected installation.
        // Calling the same enable path as the elevated helper must not deploy it.
        let portable = std::env::current_exe().unwrap();
        assert!(with_language(Language::English, || {
            apply_at(HKEY_CURRENT_USER, &test.0, &portable, true)
        })
        .unwrap_err()
        .starts_with("Install or repair Feather Task Manager"));
        assert!(read_value(&key, "Debugger").unwrap().is_none());
        assert_eq!(read_value(&key, "Unrelated").unwrap(), Some(unrelated));
    }

    #[test]
    fn always_run_as_administrator_never_targets_a_portable_copy() {
        // The test harness is neither the installed image nor a copy of it,
        // so the (read-only) check refuses it; no UAC prompt is involved.
        let refused = with_language(Language::English, check_installed_relaunch).unwrap_err();
        assert!(
            refused.starts_with("Always run as administrator applies only to"),
            "{refused}"
        );
    }

    #[test]
    fn registration_with_missing_installation_does_not_create_directory_or_key() {
        let test = TestKey::new();
        let missing_folder = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("missing-installation-{}", std::process::id()));
        assert!(!missing_folder.exists());
        let missing = missing_folder.join(APP_EXE);
        assert!(apply_at(HKEY_CURRENT_USER, &test.0, &missing, true).is_err());
        assert!(!missing_folder.exists());
        assert!(open_key(HKEY_CURRENT_USER, &test.0, false, false)
            .unwrap()
            .is_none());
    }

    #[test]
    fn recovery_removes_only_owned_registration_even_when_executable_is_missing() {
        let test = TestKey::new();
        let target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("missing-recovery-{}", std::process::id()))
            .join(APP_EXE);
        assert!(!target.exists());
        let key = open_key(HKEY_CURRENT_USER, &test.0, true, true)
            .unwrap()
            .unwrap();
        let command = debugger_command(&target).unwrap();
        set(&key, "Debugger", &string_value(&command));
        apply_at(HKEY_CURRENT_USER, &test.0, &target, false).unwrap();
        assert!(read_value(&key, "Debugger").unwrap().is_none());
        let foreign = string_value("other-manager.exe");
        set(&key, "Debugger", &foreign);
        assert!(apply_at(HKEY_CURRENT_USER, &test.0, &target, false).is_err());
        assert_eq!(read_value(&key, "Debugger").unwrap(), Some(foreign));
    }
}
