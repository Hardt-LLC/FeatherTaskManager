//! Classic desktop startup entries. Commands and startup files are never executed,
//! rewritten, moved, or deleted: only the matching Explorer approval value changes.
//!
//! Run keys: https://learn.microsoft.com/windows/win32/setupapi/run-and-runonce-registry-keys
//! StartupApproved is NOT a supported public Windows API. The 12-byte layout of
//! DWORD state and FILETIME was independently inspected in Task Manager here:
//! https://frendguo.com/how-to-disable-or-enable-startup-app-in-taskmgr/
//! Only the established states 2/3 are editable; unknown/policy states fail closed.
//! Existing trailing bytes are preserved. Packaged StartupTask apps, RunOnce,
//! scheduled tasks, policies, and startup impact measurements are out of scope.

use crate::i18n::tr;
use crate::registry::Key;

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Storage::FileSystem::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
use windows_sys::Win32::UI::Shell::{
    SHGetFolderPathW, CSIDL_COMMON_STARTUP, CSIDL_FLAG_DONT_VERIFY, CSIDL_STARTUP,
};

const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN32_NATIVE: &str = r"Software\Wow6432Node\Microsoft\Windows\CurrentVersion\Run";
const APPROVED: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved";
const MAX_VALUE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct StartupEntry {
    pub id: String,
    pub name: String,
    pub command: String,
    pub location: String,
    pub enabled: bool,
    pub manageable: bool,
    /// Includes unknown-state information that cannot be represented by `enabled`.
    pub status: String,
    hive: Hive,
    value_name: Vec<u16>,
    approval_path: String,
    approval: Option<Value>,
    source: Source,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hive {
    User,
    Machine,
}

impl Hive {
    fn key(self) -> HKEY {
        match self {
            Self::User => HKEY_CURRENT_USER,
            Self::Machine => HKEY_LOCAL_MACHINE,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::User => tr("현재 사용자", "Current user"),
            Self::Machine => tr("모든 사용자", "All users"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Value {
    kind: u32,
    data: Vec<u8>,
}

#[derive(Clone, Debug)]
enum Source {
    Registry {
        path: String,
        view: u32,
        value: Value,
    },
    Folder {
        path: PathBuf,
        identity: Option<FileIdentity>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileIdentity {
    volume: u32,
    index: u64,
    size: u64,
    created: u64,
    modified: u64,
    attributes: u32,
}

#[cfg(test)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn win_error(context: &str, code: u32) -> String {
    if code == ERROR_ACCESS_DENIED {
        crate::trf!("{context}: 접근이 거부되었습니다. 모든 사용자 항목은 관리자 권한이 필요할 수 있습니다.", "{context}: Access denied. Entries for all users may require administrator privileges.")
    } else {
        crate::trf!(
            "{context}: {} (오류 {code})",
            "{context}: {} (error {code})",
            std::io::Error::from_raw_os_error(code as i32)
        )
    }
}

fn open_key(hive: Hive, path: &str, access: u32) -> Result<Option<Key>, String> {
    // This x64 app supports exactly the classic Run key in both Win32 views.
    // NtOpenKey has no Win32 view mapping; use the known native Run32 location,
    // then walk it without links. HKCU Run is shared between the two views.
    // Do not generalize this to Software\Classes, which has different rules.
    let path = if access & KEY_WOW64_32KEY != 0 && hive == Hive::Machine {
        if path != RUN {
            return Err(win_error(
                tr(
                    "지원하지 않는 레지스트리 보기입니다",
                    "Unsupported registry view",
                ),
                ERROR_INVALID_PARAMETER,
            ));
        }
        RUN32_NATIVE
    } else {
        path
    };
    let access = (access & !KEY_WOW64_32KEY) | KEY_WOW64_64KEY;
    crate::registry::open(hive.key(), path, access).map_err(|code| {
        win_error(
            tr(
                "시작 앱 레지스트리를 열 수 없습니다",
                "Cannot open the startup app registry",
            ),
            code,
        )
    })
}

fn create_key(hive: Hive, path: &str) -> Result<Key, String> {
    crate::registry::create(
        hive.key(),
        path,
        KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
    )
    .map_err(|code| {
        win_error(
            tr(
                "시작 앱 상태를 변경할 수 없습니다",
                "Cannot change the startup app state",
            ),
            code,
        )
    })
}

fn query_value(key: &Key, name: &[u16]) -> Result<Option<Value>, String> {
    let mut kind = 0;
    let mut size = 0;
    let code = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            null(),
            &mut kind,
            null_mut(),
            &mut size,
        )
    };
    if code == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if code != ERROR_SUCCESS {
        return Err(win_error(
            tr(
                "시작 앱 값을 읽을 수 없습니다",
                "Cannot read the startup app value",
            ),
            code,
        ));
    }
    for _ in 0..4 {
        if size as usize > MAX_VALUE_BYTES {
            return Err(tr(
                "시작 앱 레지스트리 값이 너무 큽니다. 변경하지 않았습니다.",
                "The startup app registry value is too large. Nothing was changed.",
            )
            .into());
        }
        let mut data = vec![0; size as usize];
        let code = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                null(),
                &mut kind,
                data.as_mut_ptr(),
                &mut size,
            )
        };
        if code == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if code == ERROR_MORE_DATA {
            continue;
        }
        if code != ERROR_SUCCESS {
            return Err(win_error(
                tr(
                    "시작 앱 값을 읽을 수 없습니다",
                    "Cannot read the startup app value",
                ),
                code,
            ));
        }
        data.truncate(size as usize);
        return Ok(Some(Value { kind, data }));
    }
    Err(tr(
        "시작 앱 값이 계속 변경되고 있습니다. 새로 고침 후 다시 시도하세요.",
        "The startup app value keeps changing. Refresh the list and try again.",
    )
    .into())
}

fn approval_value(hive: Hive, path: &str, name: &[u16]) -> Result<Option<Value>, String> {
    match open_key(hive, path, KEY_QUERY_VALUE | KEY_WOW64_64KEY)? {
        Some(key) => query_value(&key, name),
        None => Ok(None),
    }
}

/// A missing approval means the ordinary Run/startup-folder default is enabled.
/// Unknown values are represented as unknown, not inferred from a parity bit.
fn approved_state(value: Option<&Value>) -> Option<bool> {
    let Some(value) = value else {
        return Some(true);
    };
    if value.kind != REG_BINARY || value.data.len() < 12 {
        return None;
    }
    match u32::from_le_bytes(value.data[..4].try_into().unwrap()) {
        2 => Some(true),
        3 => Some(false),
        _ => None,
    }
}

fn changed_approval(
    previous: Option<&Value>,
    enabled: bool,
    timestamp: u64,
) -> Result<Value, String> {
    if approved_state(previous).is_none() {
        return Err(tr(
            "Windows의 시작 앱 상태 형식을 확인할 수 없어 변경하지 않았습니다.",
            "The Windows startup app state format could not be verified. Nothing was changed.",
        )
        .into());
    }
    let mut data = previous
        .map(|v| v.data.clone())
        .unwrap_or_else(|| vec![0; 12]);
    data[..4].copy_from_slice(&(if enabled { 2u32 } else { 3u32 }).to_le_bytes());
    data[4..12].copy_from_slice(&(if enabled { 0 } else { timestamp }).to_le_bytes());
    Ok(Value {
        kind: REG_BINARY,
        data,
    })
}

fn command_text(value: &Value) -> Option<String> {
    if !matches!(value.kind, REG_SZ | REG_EXPAND_SZ) || !value.data.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = value
        .data
        .as_chunks::<2>()
        .0
        .iter()
        .copied()
        .map(u16::from_le_bytes)
        .collect();
    let len = units.iter().position(|v| *v == 0).unwrap_or(units.len());
    if units[len..].iter().any(|v| *v != 0) {
        return None;
    }
    String::from_utf16(&units[..len])
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn state_text(state: Option<bool>, manageable: bool) -> String {
    match (state, manageable) {
        (None, _) => tr("상태 확인 필요 · 읽기 전용", "State unknown · Read only"),
        (Some(true), false) => tr("사용 · 읽기 전용", "Enabled · Read only"),
        (Some(false), false) => tr("사용 안 함 · 읽기 전용", "Disabled · Read only"),
        (Some(true), true) => tr("사용", "Enabled"),
        (Some(false), true) => tr("사용 안 함", "Disabled"),
    }
    .into()
}

fn registry_entries(hive: Hive, view: u32) -> Result<Vec<StartupEntry>, String> {
    let Some(key) = open_key(hive, RUN, KEY_QUERY_VALUE | view)? else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    let mut index = 0;
    // Registry value names are limited to 16,383 UTF-16 characters.
    let mut name = vec![0u16; 16_384];
    let approval_group = if hive == Hive::Machine && view == KEY_WOW64_32KEY {
        "Run32"
    } else {
        "Run"
    };
    let approval_path = format!(r"{APPROVED}\{approval_group}");
    loop {
        let mut length = name.len() as u32;
        let code = unsafe {
            RegEnumValueW(
                key.0,
                index,
                name.as_mut_ptr(),
                &mut length,
                null(),
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        if code == ERROR_NO_MORE_ITEMS {
            break;
        }
        if code != ERROR_SUCCESS {
            return Err(win_error(
                tr(
                    "시작 앱 목록을 읽을 수 없습니다",
                    "Cannot read the startup app list",
                ),
                code,
            ));
        }
        index += 1;
        let mut value_name = name[..length as usize].to_vec();
        value_name.push(0);
        let Some(value) = query_value(&key, &value_name)? else {
            continue;
        };
        let approval = approval_value(hive, &approval_path, &value_name)?;
        let state = approved_state(approval.as_ref());
        let command = command_text(&value);
        let manageable = state.is_some() && command.is_some();
        let bits = if view == KEY_WOW64_32KEY { "32" } else { "64" };
        let label = String::from_utf16_lossy(&value_name[..value_name.len() - 1]);
        let id_name: String = value_name.iter().map(|v| format!("{v:04x}")).collect();
        result.push(StartupEntry {
            id: format!("run:{hive:?}:{bits}:{id_name}"),
            name: if label.is_empty() {
                tr("(이름 없음)", "(Unnamed)").into()
            } else {
                label
            },
            command: command.unwrap_or_else(|| {
                tr("지원하지 않는 명령 형식", "Unsupported command format").into()
            }),
            location: crate::trf!(
                "{} · 레지스트리 {bits}비트",
                "{} · {bits}-bit registry",
                hive.label()
            ),
            enabled: state.unwrap_or(false),
            manageable,
            status: state_text(state, manageable),
            hive,
            value_name,
            approval_path: approval_path.clone(),
            approval,
            source: Source::Registry {
                path: RUN.into(),
                view,
                value,
            },
        });
    }
    Ok(result)
}

fn file_identity(path: &Path, lock: bool) -> Result<(File, FileIdentity), String> {
    // Deny deletion and modification during an explicit toggle. Listing is fully
    // shareable, so merely opening this tab never blocks another application.
    let share = if lock {
        FILE_SHARE_READ
    } else {
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    };
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(share)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|e| {
            crate::trf!(
                "시작 앱 파일을 확인할 수 없습니다: {e}",
                "Cannot verify the startup app file: {e}"
            )
        })?;
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(win_error(
            tr(
                "시작 앱 파일 정보를 확인할 수 없습니다",
                "Cannot read the startup app file information",
            ),
            unsafe { GetLastError() },
        ));
    }
    let combine = |lo: u32, hi: u32| (hi as u64) << 32 | lo as u64;
    Ok((
        file,
        FileIdentity {
            volume: info.dwVolumeSerialNumber,
            index: combine(info.nFileIndexLow, info.nFileIndexHigh),
            size: combine(info.nFileSizeLow, info.nFileSizeHigh),
            created: combine(
                info.ftCreationTime.dwLowDateTime,
                info.ftCreationTime.dwHighDateTime,
            ),
            modified: combine(
                info.ftLastWriteTime.dwLowDateTime,
                info.ftLastWriteTime.dwHighDateTime,
            ),
            attributes: info.dwFileAttributes,
        },
    ))
}

fn folder_entries(hive: Hive) -> Result<Vec<StartupEntry>, String> {
    let mut path = [0u16; 260];
    let csidl = if hive == Hive::User {
        CSIDL_STARTUP
    } else {
        CSIDL_COMMON_STARTUP
    };
    let hr = unsafe {
        SHGetFolderPathW(
            null_mut(),
            (csidl | CSIDL_FLAG_DONT_VERIFY) as i32,
            null_mut(),
            0,
            path.as_mut_ptr(),
        )
    };
    if hr < 0 {
        return Err(crate::trf!(
            "시작프로그램 폴더 경로를 읽을 수 없습니다 (0x{:08X}).",
            "Cannot read the startup folder path (0x{:08X}).",
            hr as u32
        ));
    }
    let end = path.iter().position(|v| *v == 0).unwrap_or(path.len());
    let folder = PathBuf::from(OsString::from_wide(&path[..end]));
    let files = match fs::read_dir(&folder) {
        Ok(files) => files,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(crate::trf!(
                "시작프로그램 폴더를 읽을 수 없습니다: {e}",
                "Cannot read the startup folder: {e}"
            ))
        }
    };
    let approval_path = format!(r"{APPROVED}\StartupFolder");
    let mut result = Vec::new();
    for file in files {
        let file = file.map_err(|e| {
            crate::trf!(
                "시작프로그램 파일을 읽을 수 없습니다: {e}",
                "Cannot read the startup file: {e}"
            )
        })?;
        let name = file.file_name();
        if name.to_string_lossy().eq_ignore_ascii_case("desktop.ini")
            || file.file_type().map(|t| t.is_dir()).unwrap_or(false)
        {
            continue;
        }
        let path = file.path();
        let value_name: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
        let approval = approval_value(hive, &approval_path, &value_name)?;
        let state = approved_state(approval.as_ref());
        let identity = file_identity(&path, false).ok().map(|(_, info)| info);
        let manageable = state.is_some()
            && identity
                .as_ref()
                .is_some_and(|i| i.attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0);
        let id_name: String = value_name.iter().map(|v| format!("{v:04x}")).collect();
        result.push(StartupEntry {
            id: format!("folder:{hive:?}:{id_name}"),
            name: name.to_string_lossy().into_owned(),
            command: path.to_string_lossy().into_owned(),
            location: crate::trf!(
                "{} · 시작프로그램 폴더",
                "{} · Startup folder",
                hive.label()
            ),
            enabled: state.unwrap_or(false),
            manageable,
            status: state_text(state, manageable),
            hive,
            value_name,
            approval_path: approval_path.clone(),
            approval,
            source: Source::Folder { path, identity },
        });
    }
    Ok(result)
}

pub fn list() -> Result<Vec<StartupEntry>, String> {
    let mut result = Vec::new();
    for hive in [Hive::User, Hive::Machine] {
        let native = registry_entries(hive, KEY_WOW64_64KEY)?;
        let mut redirected = registry_entries(hive, KEY_WOW64_32KEY)?;
        if hive == Hive::User {
            // HKCU Run is shared on supported Windows. Do inspect both views,
            // but do not show the same shared entry twice.
            redirected.retain(|item| {
                !native.iter().any(|other| {
                    other.value_name == item.value_name
                        && match (&other.source, &item.source) {
                            (
                                Source::Registry { value: a, .. },
                                Source::Registry { value: b, .. },
                            ) => a == b,
                            _ => false,
                        }
                })
            });
        }
        result.extend(native);
        result.extend(redirected);
        result.extend(folder_entries(hive)?);
    }
    result.sort_by_cached_key(|entry| (entry.name.to_lowercase(), entry.id.clone()));
    Ok(result)
}

type PublisherCache = HashMap<String, (Instant, Option<String>)>;
static PUBLISHER_CACHE: OnceLock<Mutex<PublisherCache>> = OnceLock::new();
const PUBLISHER_CACHE_TTL: Duration = Duration::from_secs(300);

/// Optional CompanyName from the executable's version resource. This text is
/// supplied by the file author and is NOT a verified signature or identity.
/// Call only on the startup enumeration worker, never from the paint path.
/// No command, shortcut, shell handler, COM object, or DLL entry point is run.
pub fn publisher(entry: &StartupEntry) -> Option<String> {
    if entry.id.len() + entry.command.len() > 8192 {
        return None;
    }
    let mut key = format!("{}\0{}", entry.id, entry.command);
    if let Source::Folder { identity, .. } = &entry.source {
        key.push_str(&format!("\0{identity:?}"));
    }
    let cache = PUBLISHER_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let now = Instant::now();
    if let Some((sampled, result)) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        if now.saturating_duration_since(*sampled) < PUBLISHER_CACHE_TTL {
            return result.clone();
        }
    }
    let result = metadata_executable(entry).and_then(|path| version_company(&path));
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 1024 {
        cache.retain(|_, (sampled, _)| {
            now.saturating_duration_since(*sampled) < PUBLISHER_CACHE_TTL
        });
        if cache.len() >= 1024 {
            cache.clear();
        }
    }
    cache.insert(key, (now, result.clone()));
    result
}

fn command_executable(command: &str) -> Option<PathBuf> {
    if command.len() > 32768 || command.contains('\0') {
        return None;
    }
    let command = command.trim();
    let token = if let Some(rest) = command.strip_prefix('"') {
        let end = rest.find('"')?;
        if rest[end + 1..]
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace())
        {
            return None;
        }
        &rest[..end]
    } else {
        command.split_whitespace().next()?
    };
    // Do not guess how the shell would resolve unquoted paths containing spaces,
    // environment references, bare names, interpreter commands, or arguments.
    if token.contains(['%', '"']) {
        return None;
    }
    let path = PathBuf::from(token);
    if !path.is_absolute() || !metadata_program(&path) {
        return None;
    }
    Some(path)
}

fn metadata_program(path: &Path) -> bool {
    let Some(name) = path
        .file_name()
        .and_then(|v| v.to_str())
        .map(|v| v.to_ascii_lowercase())
    else {
        return false;
    };
    if matches!(
        name.as_str(),
        "cmd.exe"
            | "powershell.exe"
            | "pwsh.exe"
            | "wscript.exe"
            | "cscript.exe"
            | "rundll32.exe"
            | "regsvr32.exe"
            | "mshta.exe"
    ) {
        return false;
    }
    path.extension()
        .is_some_and(|v| v.eq_ignore_ascii_case("exe"))
}

fn local_metadata_path(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    if !path.is_absolute() {
        return None;
    }
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return None;
    };
    // Input paths use ordinary drive syntax; namespaces and UNC are unsupported.
    let Prefix::Disk(drive) = prefix.kind() else {
        return None;
    };
    let root = [u16::from(drive), b':' as u16, b'\\' as u16, 0];
    if unsafe { GetDriveTypeW(root.as_ptr()) } != 3 {
        return None;
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return None;
        }
        if matches!(component, Component::Normal(value) if value.encode_wide().any(|v| v == b':' as u16))
        {
            return None;
        }
        current.push(component);
        // Do not follow junctions/symlinks into network locations while reading
        // metadata supplied by a startup entry. Access failures mean unknown.
        if current.is_absolute()
            && fs::symlink_metadata(&current).ok()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT
                != 0
        {
            return None;
        }
    }
    path.is_file().then(|| path.to_path_buf())
}

fn metadata_executable(entry: &StartupEntry) -> Option<PathBuf> {
    let path = match &entry.source {
        Source::Registry { value, .. } => command_executable(&command_text(value)?)?,
        Source::Folder { path, identity } => {
            local_metadata_path(path)?;
            let expected = identity.as_ref()?;
            let (_guard, actual) = file_identity(path, true).ok()?;
            if &actual != expected || actual.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return None;
            }
            if path.extension()?.eq_ignore_ascii_case("exe") {
                path.clone()
            } else if path.extension()?.eq_ignore_ascii_case("lnk") && actual.size <= 1024 * 1024 {
                let mut bytes = Vec::with_capacity(actual.size as usize);
                File::open(path)
                    .ok()?
                    .take(1024 * 1024 + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                if bytes.len() > 1024 * 1024 {
                    return None;
                }
                shortcut_local_target(&bytes)?
            } else {
                return None;
            }
        }
    };
    if !metadata_program(&path) {
        return None;
    }
    local_metadata_path(&path)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// A small subset of MS-SHLLINK: an explicit local LinkInfo target only. No
/// ShellLink Resolve, network links, ID-list resolution, environment expansion,
/// advertised MSI targets, tracking, or relative-path guessing.
fn shortcut_local_target(bytes: &[u8]) -> Option<PathBuf> {
    const CLSID: [u8; 16] = [1, 20, 2, 0, 0, 0, 0, 0, 192, 0, 0, 0, 0, 0, 0, 70];
    if read_u32(bytes, 0)? != 76 || bytes.get(4..20)? != CLSID {
        return None;
    }
    let flags = read_u32(bytes, 20)?;
    if flags & 2 == 0 || flags & (0x100 | 0x200 | 0x1000) != 0 {
        return None;
    }
    let mut start = 76usize;
    if flags & 1 != 0 {
        let length = u16::from_le_bytes(bytes.get(start..start + 2)?.try_into().ok()?) as usize;
        start = start.checked_add(2 + length)?;
    }
    let length = read_u32(bytes, start)? as usize;
    let info = bytes.get(start..start.checked_add(length)?)?;
    let header = read_u32(info, 4)? as usize;
    if header < 28 || header > info.len() || read_u32(info, 8)? != 1 {
        return None;
    }
    let string = |offset: usize, unicode: bool| -> Option<String> {
        if offset < header || offset >= info.len() {
            return None;
        }
        if unicode {
            let units: Vec<u16> = info[offset..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .take(32768)
                .collect();
            let end = units.iter().position(|v| *v == 0)?;
            String::from_utf16(&units[..end]).ok()
        } else {
            let tail = &info[offset..];
            let end = tail.iter().position(|v| *v == 0)?;
            let value = &tail[..end];
            value
                .is_ascii()
                .then(|| String::from_utf8_lossy(value).into_owned())
        }
    };
    let unicode = header >= 36 && read_u32(info, 28)? != 0 && read_u32(info, 32)? != 0;
    let base = string(
        read_u32(info, if unicode { 28 } else { 16 })? as usize,
        unicode,
    )?;
    let suffix = string(
        read_u32(info, if unicode { 32 } else { 24 })? as usize,
        unicode,
    )?;
    // Common shortcuts contain the complete file path and an empty suffix. The
    // suffix may otherwise be a relative component appended to a local base.
    let mut target = PathBuf::from(base);
    if !suffix.is_empty() {
        let relative = Path::new(&suffix);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }
        target.push(relative);
    }
    Some(target)
}

fn version_query<'a>(buffer: &'a [u32], key: &str, wide_string: bool) -> Option<&'a [u8]> {
    let key: Vec<u16> = key.encode_utf16().chain(Some(0)).collect();
    let mut pointer = null_mut();
    let mut length = 0;
    if unsafe {
        VerQueryValueW(
            buffer.as_ptr().cast(),
            key.as_ptr(),
            &mut pointer,
            &mut length,
        )
    } == 0
    {
        return None;
    }
    let length = (length as usize).checked_mul(if wide_string { 2 } else { 1 })?;
    let offset = (pointer as usize).checked_sub(buffer.as_ptr() as usize)?;
    if offset.checked_add(length)? > std::mem::size_of_val(buffer) {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>().add(offset), length) })
}

fn version_company(path: &Path) -> Option<String> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut ignored = 0;
    // Zero flags read the file resource without opting into MUI sidecar lookup.
    let length = unsafe { GetFileVersionInfoSizeExW(0, path.as_ptr(), &mut ignored) };
    if length == 0 || length > 1024 * 1024 {
        return None;
    }
    let mut buffer = vec![0u32; (length as usize).div_ceil(4)];
    if unsafe { GetFileVersionInfoExW(0, path.as_ptr(), 0, length, buffer.as_mut_ptr().cast()) }
        == 0
    {
        return None;
    }
    let translations = version_query(&buffer, r"\VarFileInfo\Translation", false)?;
    for pair in translations.as_chunks::<4>().0.iter().take(32) {
        let language = u16::from_le_bytes([pair[0], pair[1]]);
        let page = u16::from_le_bytes([pair[2], pair[3]]);
        let key = format!(r"\StringFileInfo\{language:04x}{page:04x}\CompanyName");
        if let Some(bytes) = version_query(&buffer, &key, true) {
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .take(513)
                .collect();
            let Some(end) = units.iter().position(|v| *v == 0) else {
                continue;
            };
            let value = String::from_utf16(&units[..end]).ok()?;
            let value = value.trim();
            if !value.is_empty() && !value.chars().any(char::is_control) {
                return Some(value.to_owned());
            }
        }
    }
    None
}

fn write_value(key: &Key, name: &[u16], value: &Value) -> Result<(), String> {
    let code = unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            value.kind,
            value.data.as_ptr(),
            value.data.len() as u32,
        )
    };
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(win_error(
            tr(
                "시작 앱 상태를 저장할 수 없습니다",
                "Cannot save the startup app state",
            ),
            code,
        ))
    }
}

/// Explicit UI action only. Stale snapshots are refused and must be refreshed.
/// Windows offers no compare-and-swap registry operation: another process can
/// still race the final check/write, so the result is read back and verified.
pub fn set_enabled(entry: &StartupEntry, enabled: bool) -> Result<(), String> {
    if !entry.manageable || approved_state(entry.approval.as_ref()).is_none() {
        return Err(tr(
            "이 시작 앱은 읽기 전용입니다. Windows 설정에서 상태를 확인하세요.",
            "This startup app is read only. Check its state in Windows Settings.",
        )
        .into());
    }
    let stale = || {
        tr(
            "시작 앱이 다른 프로그램에서 변경되었습니다. 새로 고침 후 다시 시도하세요.",
            "Another application changed this startup app. Refresh the list and try again.",
        )
        .to_string()
    };
    let _file_guard = match &entry.source {
        Source::Registry { path, view, value } => {
            let key = open_key(entry.hive, path, KEY_QUERY_VALUE | *view)?.ok_or_else(stale)?;
            if query_value(&key, &entry.value_name)?.as_ref() != Some(value) {
                return Err(stale());
            }
            None
        }
        Source::Folder { path, identity } => {
            let (guard, current) = file_identity(path, true)?;
            if identity.as_ref() != Some(&current)
                || current.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(stale());
            }
            Some(guard)
        }
    };
    if approval_value(entry.hive, &entry.approval_path, &entry.value_name)? != entry.approval {
        return Err(stale());
    }
    if approved_state(entry.approval.as_ref()) == Some(enabled) {
        return Ok(());
    }
    let mut time: FILETIME = unsafe { std::mem::zeroed() };
    unsafe { GetSystemTimeAsFileTime(&mut time) };
    let timestamp = (time.dwHighDateTime as u64) << 32 | time.dwLowDateTime as u64;
    let updated = changed_approval(entry.approval.as_ref(), enabled, timestamp)?;
    let key = create_key(entry.hive, &entry.approval_path)?;
    // Recheck with the exact handle used for the write, including an approval
    // that appeared between the read-only lookup and opening/creating this key.
    if query_value(&key, &entry.value_name)? != entry.approval {
        return Err(stale());
    }
    if let Source::Registry { path, view, value } = &entry.source {
        let source_key = open_key(entry.hive, path, KEY_QUERY_VALUE | *view)?.ok_or_else(stale)?;
        if query_value(&source_key, &entry.value_name)?.as_ref() != Some(value) {
            return Err(stale());
        }
    }
    write_value(&key, &entry.value_name, &updated)?;
    if query_value(&key, &entry.value_name)?.as_ref() != Some(&updated) {
        return Err(tr(
            "시작 앱 상태가 저장 직후 변경되었습니다. 새로 고침하여 확인하세요.",
            "The startup app state changed immediately after saving. Refresh the list to check it.",
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_commands_do_not_guess_execution_rules() {
        assert_eq!(
            command_executable(r#""C:\Program Files\App\app.exe" --hidden"#),
            Some(PathBuf::from(r"C:\Program Files\App\app.exe"))
        );
        assert_eq!(
            command_executable(r"C:\Apps\app.EXE /start"),
            Some(PathBuf::from(r"C:\Apps\app.EXE"))
        );
        for command in [
            "app.exe",
            r"C:app.exe",
            r"C:\Program Files\App\app.exe",
            r"%APPDATA%\App\app.exe",
            r#""C:\Apps\app.exe"suffix"#,
            r"C:\Windows\System32\cmd.exe /c app.exe",
            r"C:\Apps\app.bat",
            "C:\\Apps\\app.exe\0 /bad",
        ] {
            assert!(
                command_executable(command).is_none(),
                "accepted {command:?}"
            );
        }
        for path in [
            r"\\server\share\app.exe",
            r"\\?\C:\Windows\explorer.exe",
            r"C:\Windows\explorer.exe:stream.exe",
            r"C:\Windows\..\Windows\explorer.exe",
        ] {
            assert!(
                local_metadata_path(Path::new(path)).is_none(),
                "accepted {path:?}"
            );
        }
    }

    fn shortcut_fixture(base: &str, suffix: &str) -> Vec<u8> {
        let mut bytes = vec![0; 76 + 28];
        bytes[0..4].copy_from_slice(&76u32.to_le_bytes());
        bytes[4..20].copy_from_slice(&[1, 20, 2, 0, 0, 0, 0, 0, 192, 0, 0, 0, 0, 0, 0, 70]);
        bytes[20..24].copy_from_slice(&2u32.to_le_bytes());
        bytes[80..84].copy_from_slice(&28u32.to_le_bytes());
        bytes[84..88].copy_from_slice(&1u32.to_le_bytes());
        bytes[92..96].copy_from_slice(&28u32.to_le_bytes());
        let suffix_offset = 28 + base.len() as u32 + 1;
        bytes[100..104].copy_from_slice(&suffix_offset.to_le_bytes());
        bytes.extend(base.as_bytes());
        bytes.push(0);
        bytes.extend(suffix.as_bytes());
        bytes.push(0);
        let size = (bytes.len() - 76) as u32;
        bytes[76..80].copy_from_slice(&size.to_le_bytes());
        bytes
    }

    #[test]
    fn publisher_shortcuts_are_bounded_local_linkinfo_only() {
        let valid = shortcut_fixture(r"C:\Apps\app.exe", "");
        assert_eq!(
            shortcut_local_target(&valid),
            Some(PathBuf::from(r"C:\Apps\app.exe"))
        );
        assert_eq!(
            shortcut_local_target(&shortcut_fixture(r"C:\Apps", "app.exe")),
            Some(PathBuf::from(r"C:\Apps\app.exe"))
        );
        assert!(shortcut_local_target(&shortcut_fixture(r"C:\Apps", r"..\evil.exe")).is_none());
        assert!(
            shortcut_local_target(&shortcut_fixture(r"C:\Apps", r"\\server\app.exe")).is_none()
        );
        for length in 0..valid.len() {
            assert!(shortcut_local_target(&valid[..length]).is_none());
        }
        for (offset, value) in [
            (20, 2 | 0x100),
            (20, 2 | 0x200),
            (20, 2 | 0x1000),
            (76, u32::MAX),
            (80, u32::MAX),
            (84, 3),
            (92, u32::MAX),
            (100, u32::MAX),
        ] {
            let mut bad = valid.clone();
            bad[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(
                shortcut_local_target(&bad).is_none(),
                "accepted offset {offset}"
            );
        }
    }

    #[test]
    fn publisher_reads_company_resource_without_launching_target() {
        let mut directory = [0u16; 32768];
        let length = unsafe {
            windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW(
                directory.as_mut_ptr(),
                directory.len() as u32,
            )
        } as usize;
        assert!(length > 0 && length < directory.len());
        let path = PathBuf::from(OsString::from_wide(&directory[..length])).join("cmd.exe");
        let company =
            version_company(&path).expect("Windows command processor CompanyName resource");
        assert!(!company.is_empty());
        assert!(!company.chars().any(char::is_control));
        assert!(
            version_company(Path::new(r"C:\this-feather-fixture-does-not-exist.exe")).is_none()
        );
    }
    use crate::i18n::{with_language, Language};

    #[test]
    fn safe_startup_handles_match_both_win32_registry_views() {
        let text = |key: &Key| {
            let name = crate::registry::test_support::key_name(key);
            String::from_utf16(
                &name
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
            .to_lowercase()
        };
        for hive in [Hive::User, Hive::Machine] {
            for view in [KEY_WOW64_32KEY, KEY_WOW64_64KEY] {
                let mut handle = null_mut();
                let status = unsafe {
                    RegOpenKeyExW(
                        hive.key(),
                        wide(RUN).as_ptr(),
                        0,
                        KEY_QUERY_VALUE | view,
                        &mut handle,
                    )
                };
                let safe = open_key(hive, RUN, KEY_QUERY_VALUE | view).unwrap();
                if matches!(status, ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) {
                    assert!(safe.is_none());
                } else {
                    assert_eq!(status, ERROR_SUCCESS);
                    let ordinary = Key(handle);
                    assert_eq!(text(&safe.unwrap()), text(&ordinary));
                }
            }
        }
    }

    #[test]
    fn startup_toggle_rejects_leaf_and_ancestor_registry_links() {
        let fixture = crate::registry::test_support::Fixture::new();
        let source = fixture.key("RunFixture");
        let target = fixture.key("UnrelatedTarget");
        let nested = fixture.key(r"UnrelatedTarget\Nested");
        let _link = fixture.link("ApprovalLink", Some(&target));
        let _unfinished = fixture.link("UnfinishedLink", None);
        let value_name = wide("HarmlessFixtureValue");
        let command = Value {
            kind: REG_SZ,
            data: wide("not-a-program; fixture only")
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect(),
        };
        write_value(&source, &value_name, &command).unwrap();
        for path in [
            "ApprovalLink",
            r"ApprovalLink\Nested",
            r"ApprovalLink\Missing",
            "UnfinishedLink",
        ] {
            let entry = StartupEntry {
                id: "security-fixture".into(),
                name: "fixture".into(),
                command: "fixture".into(),
                location: "fixture".into(),
                enabled: true,
                manageable: true,
                status: "Enabled".into(),
                hive: Hive::User,
                value_name: value_name.clone(),
                approval_path: fixture.path(path),
                approval: None,
                source: Source::Registry {
                    path: fixture.path("RunFixture"),
                    view: KEY_WOW64_64KEY,
                    value: command.clone(),
                },
            };
            assert!(set_enabled(&entry, false).is_err());
        }
        assert_eq!(query_value(&target, &value_name).unwrap(), None);
        assert_eq!(query_value(&nested, &value_name).unwrap(), None);
        assert_eq!(query_value(&source, &value_name).unwrap(), Some(command));
        assert!(open_key(
            Hive::User,
            &fixture.path(r"UnrelatedTarget\Missing"),
            KEY_QUERY_VALUE
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn english_startup_status_scope_and_errors_are_localized() {
        with_language(Language::English, || {
            assert_eq!(Hive::User.label(), "Current user");
            assert_eq!(Hive::Machine.label(), "All users");
            assert_eq!(state_text(Some(true), true), "Enabled");
            assert_eq!(state_text(Some(false), false), "Disabled · Read only");
            assert_eq!(state_text(None, false), "State unknown · Read only");
            assert_eq!(
                win_error("Save", ERROR_ACCESS_DENIED),
                "Save: Access denied. Entries for all users may require administrator privileges."
            );
        });
    }

    struct Fixture {
        key_path: String,
        folder: Option<PathBuf>,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let key_path = format!(
                r"Software\FeatherTask\Tests\{label}-{}-{}",
                std::process::id(),
                unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() }
            );
            Self {
                key_path,
                folder: None,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            // These exact generated paths are unrelated to live startup locations.
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(&self.key_path).as_ptr()) };
            if let Some(folder) = &self.folder {
                let _ = fs::remove_file(folder.join("fixture.lnk"));
                let _ = fs::remove_dir(folder);
            }
        }
    }

    #[test]
    fn unknown_approval_fails_closed() {
        for state in [0, 1, 4, 6, 7, 8, 9, 255] {
            let mut data = vec![0; 12];
            data[0] = state;
            let value = Value {
                kind: REG_BINARY,
                data,
            };
            assert_eq!(approved_state(Some(&value)), None);
            assert!(changed_approval(Some(&value), true, 42).is_err());
        }
        assert_eq!(
            approved_state(Some(&Value {
                kind: REG_BINARY,
                data: vec![2; 4]
            })),
            None
        );
        assert_eq!(
            approved_state(Some(&Value {
                kind: REG_SZ,
                data: vec![0; 12]
            })),
            None
        );
    }

    #[test]
    fn toggle_preserves_extension_and_never_modifies_input() {
        let mut initial = changed_approval(None, true, 0).unwrap();
        initial.data.extend_from_slice(&[9, 8, 7, 6]);
        let disabled = changed_approval(Some(&initial), false, 0x12345678).unwrap();
        assert_eq!(approved_state(Some(&disabled)), Some(false));
        assert_eq!(&disabled.data[12..], &[9, 8, 7, 6]);
        assert_eq!(&disabled.data[4..12], &0x12345678u64.to_le_bytes());
        assert_eq!(approved_state(Some(&initial)), Some(true));
        assert_eq!(
            changed_approval(Some(&disabled), true, 99).unwrap(),
            initial
        );
    }

    #[test]
    fn registry_strings_are_decoded_without_expansion_or_execution() {
        let data: Vec<u8> = wide(r#""%LOCALAPPDATA%\Some App\app.exe" --background"#)
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        let value = Value {
            kind: REG_EXPAND_SZ,
            data,
        };
        assert_eq!(
            command_text(&value).as_deref(),
            Some(r#""%LOCALAPPDATA%\Some App\app.exe" --background"#)
        );
        assert!(command_text(&Value {
            kind: REG_BINARY,
            data: vec![1, 2]
        })
        .is_none());
        assert!(command_text(&Value {
            kind: REG_SZ,
            data: vec![1]
        })
        .is_none());
        assert!(command_text(&Value {
            kind: REG_SZ,
            data: vec![65, 0, 0, 0, 66, 0]
        })
        .is_none());
    }

    /// Read-only by design. Mutation tests below use a disposable, app-owned key.
    #[test]
    fn live_enumeration_is_read_only() {
        let items = list().expect("live startup enumeration");
        let mut ids: Vec<_> = items.iter().map(|item| &item.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), items.len());
    }

    #[test]
    fn disposable_registry_toggle_preserves_source_and_rejects_stale() {
        // This is deliberately outside every real autorun/StartupApproved key.
        let fixture = Fixture::new("Startup");
        let path = fixture.key_path.clone();
        let key = create_key(Hive::User, &path).unwrap();
        let name = wide("Harmless fixture");
        let command = Value {
            kind: REG_EXPAND_SZ,
            data: wide("not-a-program; fixture only")
                .into_iter()
                .flat_map(u16::to_le_bytes)
                .collect(),
        };
        write_value(&key, &name, &command).unwrap();
        let approval_path = format!(r"{path}\Approval");
        let mut entry = StartupEntry {
            id: "fixture".into(),
            name: "fixture".into(),
            command: "fixture".into(),
            location: "fixture".into(),
            enabled: true,
            manageable: true,
            status: "사용".into(),
            hive: Hive::User,
            value_name: name.clone(),
            approval_path: approval_path.clone(),
            approval: None,
            source: Source::Registry {
                path: path.clone(),
                view: KEY_WOW64_64KEY,
                value: command.clone(),
            },
        };
        let exercise = || -> Result<(), String> {
            set_enabled(&entry, false)?;
            assert_eq!(query_value(&key, &name)?.as_ref(), Some(&command));
            assert!(set_enabled(&entry, true).is_err());
            entry.approval = approval_value(Hive::User, &approval_path, &name)?;
            assert_eq!(approved_state(entry.approval.as_ref()), Some(false));
            entry.enabled = false;
            set_enabled(&entry, true)?;
            assert_eq!(query_value(&key, &name)?.as_ref(), Some(&command));
            entry.approval = approval_value(Hive::User, &approval_path, &name)?;
            write_value(
                &key,
                &name,
                &Value {
                    kind: REG_SZ,
                    data: wide("changed fixture")
                        .into_iter()
                        .flat_map(u16::to_le_bytes)
                        .collect(),
                },
            )?;
            assert!(set_enabled(&entry, false).is_err());
            Ok(())
        };
        let result = {
            let mut exercise = exercise;
            exercise()
        };
        drop(key);
        result.unwrap();
    }

    #[test]
    fn disposable_file_toggle_keeps_file_and_rejects_changed_identity() {
        let mut fixture = Fixture::new("FileStartup");
        let folder = std::env::temp_dir().join(format!(
            "FeatherTask-{}",
            fixture.key_path.rsplit('\\').next().unwrap()
        ));
        fs::create_dir(&folder).unwrap();
        fixture.folder = Some(folder.clone());
        let path = folder.join("fixture.lnk");
        let content = b"Harmless test fixture; not a real shortcut.";
        fs::write(&path, content).unwrap();
        let name = wide("fixture.lnk");
        let (_, identity) = file_identity(&path, false).unwrap();
        let mut entry = StartupEntry {
            id: "fixture".into(),
            name: "fixture".into(),
            command: "fixture".into(),
            location: "fixture".into(),
            enabled: true,
            manageable: true,
            status: "사용".into(),
            hive: Hive::User,
            value_name: name.clone(),
            approval_path: fixture.key_path.clone(),
            approval: None,
            source: Source::Folder {
                path: path.clone(),
                identity: Some(identity),
            },
        };
        set_enabled(&entry, false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);
        entry.approval = approval_value(Hive::User, &fixture.key_path, &name).unwrap();
        assert_eq!(approved_state(entry.approval.as_ref()), Some(false));
        set_enabled(&entry, true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), content);
        entry.approval = approval_value(Hive::User, &fixture.key_path, &name).unwrap();
        fs::write(
            &path,
            b"A different, larger harmless test fixture with changed metadata.",
        )
        .unwrap();
        assert!(set_enabled(&entry, false).is_err());
        assert_eq!(
            approval_value(Hive::User, &fixture.key_path, &name).unwrap(),
            entry.approval
        );
    }
}
