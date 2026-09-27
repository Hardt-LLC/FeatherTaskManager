#![windows_subsystem = "windows"]
#![cfg_attr(not(target_os = "windows"), allow(unused))]

#[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
compile_error!("Feather Task Manager currently targets 64-bit Windows (x86_64-pc-windows-msvc).");

mod actions;
mod gpu;
mod hardware;
mod i18n;
mod memclean;
mod netetw;
mod performance;
mod process_tree;
mod registry;
mod replacement;
mod sampler;
mod services;
mod smbios;
mod startup;
mod storage;
mod ui;

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

/// Exit code when the DLL search restriction cannot be set (see
/// `restrict_dll_search`); the process then runs no other code.
const DLL_SEARCH_POLICY_FAILED: i32 = 3;

/// The uninstaller's helper that deletes this account's preferences.
const REMOVE_USER_PREFERENCES: &str = "--remove-user-preferences";
/// Everything Feather stores per user, below HKCU: the `Preferences` subkey
/// with the window settings (ui/preferences.rs) and `Language` (i18n.rs).
const USER_PREFERENCES_KEY: &str = r"Software\FeatherTask";

fn main() {
    // First, before any other code can make Windows load a DLL (FTM-2026-05).
    if !restrict_dll_search() {
        std::process::exit(DLL_SEARCH_POLICY_FAILED);
    }
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some(REMOVE_USER_PREFERENCES) {
        // Run by the (elevated) uninstaller before it deletes the files.
        // Nothing else happens first: no language or preference read, no
        // "Always run as administrator" relaunch (that is `ui::run`), no
        // files and no UI. The result is the exit code only.
        std::process::exit(remove_user_preferences(&args, USER_PREFERENCES_KEY));
    }
    i18n::initialize(&args);
    if let Some(result) = match args.get(1).map(String::as_str) {
        Some("--install-task-manager") => Some(replacement::apply(true)),
        Some("--restore-task-manager") => Some(replacement::apply(false)),
        Some("--prepare-install-directory") => Some(replacement::prepare_install_directory()),
        Some("--validate-installation") => Some(replacement::validate_installation()),
        _ => None,
    } {
        // The UI requests these explicit helpers through UAC; ordinary launches
        // never install files or change the Windows Task Manager association.
        if let Err(error) = result {
            use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR};
            let message: Vec<u16> = error.encode_utf16().chain(Some(0)).collect();
            let title: Vec<u16> = "Feather Task Manager"
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                MessageBoxW(
                    std::ptr::null_mut(),
                    message.as_ptr(),
                    title.as_ptr(),
                    MB_ICONERROR,
                );
            }
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--purge-memory-lists") {
        // Elevated helper of the Nuclear Zombie panel: purge the named memory
        // lists and report through the exit code only (no files, no UI).
        std::process::exit(memclean::helper_main(purge_helper_argument(&args)) as i32);
    }
    if args.get(1).map(String::as_str) == Some("--memory-cleanup-dry-run") {
        // Read-only: what a cleanup would find (memory lists, trimmable
        // processes, zombie processes). Trims and purges nothing.
        let Some(output) = output_argument(&args) else {
            std::process::exit(1);
        };
        let (text, passed) = match memclean::dry_run_report() {
            Ok(text) => (text, true),
            Err(error) => (format!("FAIL: {error}\n"), false),
        };
        if write_output_file(Path::new(output), &text).is_err() || !passed {
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--test-child") {
        std::thread::sleep(std::time::Duration::from_secs(30));
        return;
    }
    if args.get(1).map(String::as_str) == Some("--render-previews") {
        let directory = Path::new(output_argument(&args).unwrap_or("previews"));
        // Every image is written through `create_output_file`; this only keeps
        // the folder creation itself from following a planted link.
        if check_output_folder(directory).is_err() {
            std::process::exit(1);
        }
        if let Err(error) = ui::render_previews(directory) {
            let _ = std::fs::create_dir_all(directory);
            let _ = write_output_file(&directory.join("error.txt"), &error);
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--dump-hardware") {
        // Read-only: writes every collected hardware value to a text file.
        let Some(output) = output_argument(&args) else {
            std::process::exit(1);
        };
        let (text, passed) = match hardware::dump_report() {
            Ok(text) => (text, true),
            Err(error) => (format!("FAIL: {error}\n"), false),
        };
        if write_output_file(Path::new(output), &text).is_err() || !passed {
            std::process::exit(1);
        }
        return;
    }
    if args.get(1).map(String::as_str) == Some("--self-test") {
        let result = self_test();
        let output = output_argument(&args).unwrap_or("self-test.txt");
        let passed = result.is_ok();
        let text = match result {
            Ok(s) => s,
            Err(e) => format!("FAIL: {e}\n"),
        };
        if write_output_file(Path::new(output), &text).is_err() || !passed {
            std::process::exit(1);
        }
        return;
    }
    ui::run();
}

/// Restrict every DLL this process loads from now on to System32
/// (FTM-2026-05). `/DEPENDENTLOADFLAG` (build.rs) covers only the static
/// imports resolved before `main`. Windows components still load DLLs by
/// name at run time (powrprof.dll's delay-loaded umpdc.dll, gdi32full.dll's
/// opengl32.dll), and the standard search order starts with the folder of
/// the executable, which for the portable build may be Downloads. The
/// default directories also apply to the dependencies of a DLL loaded by
/// full path. Side-by-side redirection (comctl32 v6, GDI+) and KnownDLLs are
/// resolved before the search path and are unaffected; every other DLL
/// Feather uses (oleacc, uxtheme, dwmapi, pdh, ...) is in System32. Loads
/// made by DLL initializers before `main` cannot be covered here; build.rs
/// delay-loads powrprof.dll so that its umpdc.dll load happens afterwards.
///
/// Returns false when the restriction cannot be set; the caller then exits
/// instead of running with the unsafe search order.
fn restrict_dll_search() -> bool {
    use windows_sys::Win32::System::{
        LibraryLoader::{SetDefaultDllDirectories, LOAD_LIBRARY_SEARCH_SYSTEM32},
        SystemServices::PROCESS_MITIGATION_IMAGE_LOAD_POLICY,
        Threading::{ProcessImageLoadPolicy, SetProcessMitigationPolicy},
    };
    if unsafe { SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32) } == 0 {
        return false;
    }
    // Defense in depth, best effort: also look in System32 before the
    // application folder for loads that pass their own search flags, and
    // refuse images on remote (UNC/WebDAV) paths. When Windows rejects the
    // policy the restriction above still applies, so the result is ignored.
    const NO_REMOTE_IMAGES: u32 = 1 << 0;
    const PREFER_SYSTEM32_IMAGES: u32 = 1 << 2;
    let mut policy = PROCESS_MITIGATION_IMAGE_LOAD_POLICY::default();
    policy.Anonymous.Flags = NO_REMOTE_IMAGES | PREFER_SYSTEM32_IMAGES;
    let _ = unsafe {
        SetProcessMitigationPolicy(
            ProcessImageLoadPolicy,
            (&raw const policy).cast(),
            std::mem::size_of_val(&policy),
        )
    };
    true
}

/// The list argument of `--purge-memory-lists <list> [--language ko|en]`,
/// accepted only in exactly that shape (the UI appends the language); any
/// other argument makes the helper exit with its usage code.
fn purge_helper_argument(args: &[String]) -> Option<&str> {
    match args {
        [_, _, list] => Some(list),
        [_, _, list, flag, language]
            if flag == "--language" && matches!(language.as_str(), "ko" | "en") =>
        {
            Some(list)
        }
        _ => None,
    }
    .map(String::as_str)
}

/// `FeatherTaskManager.exe --remove-user-preferences`, accepted only with no
/// other argument (else exit code 1): delete the HKCU key `path` of the
/// account running it, with every subkey, never following a registry link
/// (`registry::delete_tree`: a link is removed as the link itself and its
/// target is left untouched). Exit code 0 when the key is gone or never
/// existed, 2 when it could not be removed completely.
fn remove_user_preferences(args: &[String], path: &str) -> i32 {
    if !matches!(args, [_, flag] if flag == REMOVE_USER_PREFERENCES) {
        return 1;
    }
    match registry::delete_tree(
        windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
        path,
    ) {
        Ok(()) => 0,
        Err(_) => 2,
    }
}

/// The output path of a diagnostic command (the argument after it), unless
/// it is missing or another option such as `--language`.
fn output_argument(args: &[String]) -> Option<&str> {
    args.get(2)
        .map(String::as_str)
        .filter(|path| !path.is_empty() && !path.starts_with("--"))
}

// ─────────────────────────── diagnostic output files ───────────────────────────
//
// `--self-test`, `--dump-hardware`, `--memory-cleanup-dry-run` and
// `--render-previews` may run elevated in a folder another user can write
// to, where a planted link could redirect the write to any file the
// administrator can change (FTM-2026-08). Output is therefore written only
// when no folder on the path is a junction, symbolic link or mount point,
// the file itself is no reparse point and has no other hard link, and the
// opened file lies in the very folders that were checked (so a folder
// swapped for a link between the checks and the open is detected too).

/// Open `folder` itself (never a link target) and reject a folder that is a
/// name-surrogate reparse point (junction, symbolic link, mount point).
/// Other reparse points, such as cloud-file placeholders, are ordinary
/// folders to the file system and are allowed.
fn open_output_folder(folder: &Path) -> std::io::Result<File> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    const NAME_SURROGATE: u32 = 0x2000_0000;
    let handle = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(folder)
        .map_err(|error| {
            std::io::Error::new(error.kind(), format!("{}: {error}", folder.display()))
        })?;
    let (attributes, tag) = attribute_tag(&handle)?;
    if attributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(std::io::Error::other(format!(
            "{} is not a folder",
            folder.display()
        )));
    }
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && tag & NAME_SURROGATE != 0 {
        return Err(std::io::Error::other(format!(
            "{} is a junction, symbolic link or mount point",
            folder.display()
        )));
    }
    Ok(handle)
}

/// `FileAttributeTagInfo` of an open handle: (attributes, reparse tag).
fn attribute_tag(handle: &File) -> std::io::Result<(u32, u32)> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_TAG_INFO,
    };
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            handle.as_raw_handle(),
            FileAttributeTagInfo,
            (&raw mut info).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok((info.FileAttributes, info.ReparseTag))
}

/// Open and check every folder of the absolute `folder`, from the root down.
fn open_output_folders(folder: &Path) -> std::io::Result<Vec<File>> {
    let mut chain: Vec<&Path> = folder.ancestors().collect();
    chain.reverse();
    chain.into_iter().map(open_output_folder).collect()
}

/// Check the existing part of a folder path an output folder will be
/// created in (`--render-previews`).
fn check_output_folder(folder: &Path) -> std::io::Result<()> {
    let folder = std::path::absolute(folder)?;
    let mut chain: Vec<&Path> = folder.ancestors().collect();
    chain.reverse();
    for folder in chain {
        match open_output_folder(folder) {
            Ok(_) => {}
            // create_dir_all makes the rest; each file is checked again.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Normalized NT path (`\Device\HarddiskVolume3\...`) of an open handle.
fn final_path(handle: &File) -> std::io::Result<Vec<u16>> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_NT,
    };
    let mut buffer = vec![0_u16; 512];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(
                handle.as_raw_handle(),
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_NT,
            )
        } as usize;
        if length == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(buffer);
        }
        if length > 32_768 {
            return Err(std::io::Error::other("output path is too long"));
        }
        // Too small: `length` includes the terminating null.
        buffer.resize(length, 0);
    }
}

/// Whether `child` (an NT path) lies directly in `folder` (an NT path),
/// compared the way the file system does (ordinal, ignoring case).
fn directly_in(child: &[u16], folder: &[u16]) -> bool {
    use windows_sys::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
    let separator = u16::from(b'\\');
    let Some(end) = child.iter().rposition(|&unit| unit == separator) else {
        return false;
    };
    let parent = &child[..end];
    let mut folder = folder;
    while let [rest @ .., last] = folder {
        if *last != separator {
            break;
        }
        folder = rest;
    }
    (unsafe {
        CompareStringOrdinal(
            parent.as_ptr(),
            parent.len() as i32,
            folder.as_ptr(),
            folder.len() as i32,
            1,
        )
    }) == CSTR_EQUAL
}

/// Whether the opened `file` is a plain file with a single name, inside the
/// checked `folders` (root first), each still inside the one before.
fn verify_output_file(file: &File, folders: &[File]) -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let (attributes, _) = attribute_tag(file).map_err(|error| error.to_string())?;
    if attributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        return Err("the path is a link, reparse point or folder".into());
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if info.nNumberOfLinks != 1 {
        return Err("the file has another hard link".into());
    }
    // A folder replaced by a junction after its check, or an object-manager
    // link reached through one, would put the file (or a later folder)
    // somewhere other than inside the folder checked before it.
    let mut child = final_path(file).map_err(|error| error.to_string())?;
    for folder in folders.iter().rev() {
        let path = final_path(folder).map_err(|error| error.to_string())?;
        if !directly_in(&child, &path) {
            return Err("the path was redirected through a link to another folder".into());
        }
        child = path;
    }
    Ok(())
}

/// Create or truncate the diagnostic output file `path` (see above). A file
/// this call created is removed again when the checks fail; an existing
/// file is truncated only after they pass.
fn create_output_file(path: &Path) -> Result<File, String> {
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, DELETE, FILE_DISPOSITION_INFO,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
    };
    let refuse = |error: &dyn std::fmt::Display| format!("{}: {error}", path.display());
    let absolute = std::path::absolute(path).map_err(|error| refuse(&error))?;
    let (Some(folder), Some(_)) = (absolute.parent(), absolute.file_name()) else {
        return Err(refuse(&"not a file path"));
    };
    let folders = open_output_folders(folder).map_err(|error| refuse(&error))?;
    let open = |new: bool| {
        // Only a file this call creates may be deleted again.
        let delete = if new { DELETE } else { 0 };
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(new)
            .access_mode(FILE_GENERIC_WRITE | FILE_READ_ATTRIBUTES | delete)
            .share_mode(FILE_SHARE_READ)
            // A link itself is opened (and then rejected), never its target.
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        options.open(&absolute)
    };
    let (file, created) = match open(true) {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            (open(false).map_err(|error| refuse(&error))?, false)
        }
        Err(error) => return Err(refuse(&error)),
    };
    if let Err(error) = verify_output_file(&file, &folders) {
        if created {
            let dispose = FILE_DISPOSITION_INFO { DeleteFile: true };
            unsafe {
                SetFileInformationByHandle(
                    file.as_raw_handle(),
                    FileDispositionInfo,
                    (&raw const dispose).cast(),
                    std::mem::size_of_val(&dispose) as u32,
                )
            };
        }
        return Err(refuse(&format!("refusing to write: {error}")));
    }
    file.set_len(0).map_err(|error| refuse(&error))?;
    Ok(file)
}

/// Write `text` to a diagnostic output file through `create_output_file`.
fn write_output_file(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write as _;
    let mut file = create_output_file(path)?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn self_test() -> Result<String, String> {
    use std::time::{Duration, Instant};
    let mut sampler = sampler::Sampler::new()?;
    let t = Instant::now();
    let first = sampler.sample()?;
    let first_ms = t.elapsed().as_secs_f64() * 1000.0;
    let own_pid = std::process::id();
    let own = first
        .processes
        .iter()
        .find(|p| p.pid == own_pid)
        .ok_or("own process missing")?;
    if own.working_set == 0 || own.created == 0 {
        return Err("invalid own process counters".into());
    }
    let path = actions::executable_path(own.pid, own.created)?;
    if actions::terminate(own.pid, own.created).is_ok() {
        return Err("self termination was allowed".into());
    }
    if actions::executable_path(own.pid, own.created.wrapping_add(1)).is_ok() {
        return Err("PID reuse guard failed".into());
    }
    if actions::terminate(4, 1).is_ok() {
        return Err("System termination was allowed".into());
    }
    let mut child = std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?)
        .arg("--test-child")
        .spawn()
        .map_err(|e| e.to_string())?;
    let child_result = (|| {
        let snap = sampler.sample()?;
        let proc = snap
            .processes
            .iter()
            .find(|p| p.pid == child.id())
            .ok_or("test child missing")?;
        actions::terminate(proc.pid, proc.created.wrapping_add(1))
            .err()
            .ok_or("stale identity accepted")?;
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            return Err("stale identity killed child".into());
        }
        actions::terminate(proc.pid, proc.created)?;
        child.wait().map_err(|e| e.to_string())?;
        Ok::<_, String>(())
    })();
    if child_result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    child_result?;
    let mut costs = Vec::new();
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(50));
        let snap = sampler.sample()?;
        if !(0.0..=100.0).contains(&snap.cpu_percent)
            || snap.memory_total == 0
            || snap.memory_used > snap.memory_total
        {
            return Err("invalid system counters".into());
        }
        if snap.processes.iter().any(|p| {
            !p.cpu_percent.is_finite()
                || !(0.0..=100.0).contains(&p.cpu_percent)
                || !p.io_bytes_per_sec.is_finite()
        }) {
            return Err("invalid process counters".into());
        }
        costs.push(snap.sample_ms);
    }
    costs.sort_by(f64::total_cmp);
    let service_count = services::list()?.len();
    let startup_count = startup::list()?.len();
    let mut perf_sampler = performance::PerfSampler::new()?;
    let mut tracker = performance::ProcessGpuTracker::default();
    let mut network = netetw::NetworkMonitor::new();
    let before = sampler.sample()?;
    let primed = perf_sampler.sample()?;
    tracker.join(&before.processes, Some(&primed));
    network.sample(&before.processes);
    std::thread::sleep(Duration::from_millis(1100));
    let processes = sampler.sample()?;
    let perf = perf_sampler.sample()?;
    if perf.logical_cpus == 0 || perf.cpu_name.is_empty() {
        return Err("invalid performance metadata".into());
    }
    let joined = tracker.join(&processes.processes, Some(&perf));
    let net = network.sample(&processes.processes);
    let hardware = hardware_checks(&perf, &processes, &joined, &mut tracker)?;
    let network_report = network_checks(&processes, &net, network.event_count())?;
    Ok(format!("PASS\nprocesses={}\nservices={}\nstartup_entries={}\nfirst_snapshot_ms={first_ms:.3}\nsample_median_ms={:.3}\nsample_max_ms={:.3}\nexecutable={}\nperformance_cpu={}\nperformance_disk_ready={}\nperformance_network_ready={}\nperformance_gpu={:?}\n{hardware}{network_report}performance_warnings={:?}\nChecks: live snapshots, own identity, memory bounds, finite CPU/I/O, stale PID rejection, System/self protection, disposable child termination, read-only service/startup enumeration, native performance providers, hardware identity bounds (SMBIOS, storage, D3DKMT, sensors, thermal zones), per-process GPU bounds and PID-reuse attribution, per-process network availability and bounds.\n", first.processes.len(), service_count, startup_count, costs[costs.len()/2], costs[costs.len()-1], path, perf.cpu_name, perf.disk_rates_ready, perf.network_rates_ready, perf.gpu_percent, perf.warnings))
}

/// Sanity checks for the hardware collectors; returns report lines.
fn hardware_checks(
    perf: &performance::PerfSnapshot,
    processes: &sampler::Snapshot,
    joined: &std::collections::HashMap<(u32, u64), performance::ProcessGpu>,
    tracker: &mut performance::ProcessGpuTracker,
) -> Result<String, String> {
    use std::fmt::Write as _;
    let mut out = String::new();
    if perf
        .cpu_base_mhz
        .is_some_and(|mhz| !(100..=20_000).contains(&mhz))
    {
        return Err("implausible CPU base speed".into());
    }
    if let Some(caches) = perf.cpu_caches {
        if [caches.l1_bytes, caches.l2_bytes, caches.l3_bytes]
            .into_iter()
            .flatten()
            .any(|bytes| bytes == 0)
        {
            return Err("zero-sized CPU cache".into());
        }
    }
    if let Some(memory) = &perf.memory_modules {
        if memory
            .slots_total
            .is_some_and(|total| memory.slots_used > total)
        {
            return Err("more memory modules than slots".into());
        }
        if memory.modules.iter().any(|m| m.size_bytes == Some(0)) {
            return Err("empty memory slot listed as a module".into());
        }
        // Informational: firmware reservations usually make visible memory
        // smaller, but VMs with dynamic memory can legitimately differ.
        let installed: Option<u64> = memory.modules.iter().map(|m| m.size_bytes).sum();
        let _ = writeln!(
            out,
            "memory_modules={} of {:?}, {:?}, {:?}, {:?} MT/s, installed {:?} B, visible {:?} B",
            memory.slots_used,
            memory.slots_total,
            memory.memory_type(),
            memory.form_factor(),
            memory.configured_speed_mts(),
            installed,
            perf.memory.as_ref().map(|m| m.physical_total)
        );
    }
    for disk in &perf.disks {
        let device = perf
            .storage_device(&disk.id)
            .ok_or_else(|| format!("no storage identity for disk {}", disk.id))?;
        if device.capacity_bytes == Some(0) {
            return Err("zero disk capacity".into());
        }
    }
    // Only when every disk answered the extent query must one hold Windows.
    if !perf.storage.is_empty()
        && perf.storage.iter().all(|d| d.system_disk.is_some())
        && !perf.storage.iter().any(|d| d.system_disk == Some(true))
    {
        return Err("no disk holds the Windows volume".into());
    }
    let _ = writeln!(
        out,
        "storage={:?}",
        perf.storage
            .iter()
            .map(|d| (d.disk_number, d.model.clone(), d.bus.map(|b| b.label())))
            .collect::<Vec<_>>()
    );
    for gpu in &perf.gpus {
        if let (Some(used), Some(total)) = (
            gpu.dedicated_bytes,
            gpu.adapter.as_ref().and_then(|a| a.dedicated_video_memory),
        ) {
            if total > 0 && used > total {
                return Err("GPU dedicated usage exceeds its memory".into());
            }
        }
        if let Some(sensors) = gpu.sensors {
            if sensors
                .temperature_c
                .is_some_and(|c| !(0.0..=150.0).contains(&c))
                || sensors
                    .power_percent
                    .is_some_and(|p| !(0.0..=1000.0).contains(&p))
            {
                return Err("implausible GPU sensor value".into());
            }
        }
        let _ = writeln!(
            out,
            "gpu={} name={:?} temperature_c={:?}",
            gpu.id,
            gpu.name,
            gpu.sensors.and_then(|s| s.temperature_c)
        );
    }
    if perf
        .thermal_zones
        .iter()
        .any(|zone| !(0.0..=150.0).contains(&zone.celsius))
    {
        return Err("implausible thermal zone".into());
    }
    if joined.values().any(|value| {
        value
            .percent
            .is_some_and(|p| !p.is_finite() || !(0.0..=100.0).contains(&p))
    }) {
        return Err("invalid per-process GPU utilization".into());
    }
    // A changed creation time (PID reuse) must not inherit a utilization.
    let mut reused = processes.processes.clone();
    let own = reused
        .iter_mut()
        .find(|p| p.pid == std::process::id())
        .ok_or("own process missing")?;
    own.created = own.created.wrapping_add(1);
    let key = (own.pid, own.created);
    let rejoined = tracker.join(&reused, Some(perf));
    if rejoined
        .get(&key)
        .is_some_and(|value| value.percent.is_some())
    {
        return Err("reused PID inherited GPU utilization".into());
    }
    let _ = writeln!(
        out,
        "process_gpu_attributed={} thermal_zones={} gpu_adapters={}",
        joined.values().filter(|v| v.percent.is_some()).count(),
        perf.thermal_zones.len(),
        perf.gpu_adapters.len()
    );
    Ok(out)
}

/// Sanity checks for per-process network (ETW). Returns report lines.
fn network_checks(
    processes: &sampler::Snapshot,
    net: &netetw::ProcessNetworkSample,
    events: u64,
) -> Result<String, String> {
    use std::fmt::Write as _;
    let elevated = netetw::is_elevated();
    // Availability must match elevation: no session, and an honest reason, when
    // not elevated; a running session that measured this second interval when
    // elevated, so an elevated verification run fails loudly if the trace
    // could not start, stopped, or dropped events.
    if elevated {
        if let Some(reason) = &net.reason {
            return Err(format!(
                "elevated per-process network unavailable: {reason}"
            ));
        }
        if !net.measured || net.interval_seconds <= 0.0 {
            return Err(format!(
                "elevated per-process network did not measure an interval (measured={}, interval={:.3} s)",
                net.measured, net.interval_seconds
            ));
        }
    } else {
        match &net.reason {
            Some(reason) if reason.to_lowercase().contains("admin") => {}
            other => {
                return Err(format!(
                    "non-elevated network sample must report an admin reason, got {other:?}"
                ));
            }
        }
        if !net.by_id.is_empty() || net.measured {
            return Err("non-elevated network sample must have no measured rates".into());
        }
    }
    // Rates must be finite and non-negative, and keyed only by live identities.
    let live: std::collections::HashSet<(u32, u64)> = processes
        .processes
        .iter()
        .map(|p| (p.pid, p.created))
        .collect();
    for (id, value) in &net.by_id {
        if !live.contains(id) {
            return Err("network rate attributed to an unknown identity".into());
        }
        for rate in [
            value.send_bytes_per_sec,
            value.recv_bytes_per_sec,
            value.total_bytes_per_sec,
        ] {
            if !rate.is_finite() || rate < 0.0 {
                return Err("invalid per-process network rate".into());
            }
        }
        if (value.send_bytes_per_sec + value.recv_bytes_per_sec - value.total_bytes_per_sec).abs()
            > 1.0
        {
            return Err("network total does not match send + recv".into());
        }
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "process_network_elevated={} measured={} reason={:?} interval_s={:.3} rated_processes={} total_send_bytes_per_sec={:.0} total_recv_bytes_per_sec={:.0} etw_events={}",
        elevated,
        net.measured,
        net.reason,
        net.interval_seconds,
        net.by_id.len(),
        net.total_send_bytes_per_sec,
        net.total_recv_bytes_per_sec,
        events
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn cli_purge_helper_accepts_only_the_exact_shape() {
        let flag = "--purge-memory-lists";
        assert_eq!(
            purge_helper_argument(&args(&["app", flag, "all"])),
            Some("all")
        );
        for language in ["ko", "en"] {
            assert_eq!(
                purge_helper_argument(&args(&["app", flag, "standby", "--language", language])),
                Some("standby")
            );
        }
        for rejected in [
            &["app", flag][..],
            &["app", flag, "all", "--language"],
            &["app", flag, "all", "--language", "fr"],
            &["app", flag, "all", "--lang", "en"],
            &["app", flag, "all", "extra"],
            &["app", flag, "all", "--language", "en", "extra"],
            &["app", flag, "all", "--language", "en", "--language", "ko"],
        ] {
            assert_eq!(purge_helper_argument(&args(rejected)), None, "{rejected:?}");
        }
    }

    #[test]
    fn cli_output_argument_is_never_an_option() {
        let flag = "--dump-hardware";
        assert_eq!(
            output_argument(&args(&["app", flag, "out.txt"])),
            Some("out.txt")
        );
        assert_eq!(output_argument(&args(&["app", flag])), None);
        assert_eq!(output_argument(&args(&["app", flag, ""])), None);
        assert_eq!(
            output_argument(&args(&["app", flag, "--language", "en"])),
            None
        );
    }

    /// Always against a temporary test key, never the real preferences.
    #[test]
    fn cli_remove_user_preferences_accepts_only_the_exact_argument() {
        use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, KEY_QUERY_VALUE};
        let exists = |path: &str| {
            registry::open(HKEY_CURRENT_USER, path, KEY_QUERY_VALUE)
                .unwrap()
                .is_some()
        };
        let fixture = registry::test_support::Fixture::new();
        let path = fixture.path("FeatherTask");
        assert_ne!(path, USER_PREFERENCES_KEY);
        drop(fixture.key(r"FeatherTask\Preferences"));
        let flag = REMOVE_USER_PREFERENCES;
        for rejected in [
            &["app"][..],
            &["app", flag, "extra"],
            &["app", flag, ""],
            &["app", flag, "--language", "en"],
            &["app", "--language", "en", flag],
            &["app", "extra", flag],
            &["app", "--remove-user-preferences=1"],
            &["app", "--Remove-User-Preferences"],
            &["app", "--remove-user-preference"],
        ] {
            assert_eq!(
                remove_user_preferences(&args(rejected), &path),
                1,
                "{rejected:?}"
            );
        }
        assert!(exists(&fixture.path(r"FeatherTask\Preferences")));
        assert_eq!(remove_user_preferences(&args(&["app", flag]), &path), 0);
        assert!(!exists(&path));
        // Already gone, or never there: success as well.
        assert_eq!(remove_user_preferences(&args(&["app", flag]), &path), 0);
        // A key that cannot be removed safely (a link among its ancestors).
        let target = fixture.key("UnrelatedTarget");
        drop(fixture.key(r"UnrelatedTarget\FeatherTask"));
        let _link = fixture.link("Link", Some(&target));
        assert_eq!(
            remove_user_preferences(&args(&["app", flag]), &fixture.path(r"Link\FeatherTask")),
            2
        );
        assert!(exists(&fixture.path(r"UnrelatedTarget\FeatherTask")));
    }

    /// A private folder under %TEMP%, removed on drop.
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("feather-output-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn cli_output_file_writes_and_truncates_plain_files() {
        let scratch = Scratch::new("plain");
        let path = scratch.0.join("report.txt");
        write_output_file(&path, "first, longer text").unwrap();
        write_output_file(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        // A relative path resolves against the current folder.
        assert!(std::path::absolute(Path::new("self-test.txt")).is_ok());
        // A folder is never an output file.
        assert!(write_output_file(&scratch.0, "x").is_err());
    }

    #[test]
    fn cli_output_file_refuses_hard_links_and_keeps_their_content() {
        let scratch = Scratch::new("hardlink");
        let target = scratch.0.join("protected.txt");
        std::fs::write(&target, "protected").unwrap();
        let link = scratch.0.join("report.txt");
        std::fs::hard_link(&target, &link).unwrap();
        assert!(write_output_file(&link, "attack").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "protected");
    }

    #[test]
    fn cli_output_file_refuses_junctions_on_the_path() {
        let scratch = Scratch::new("junction");
        let target = scratch.0.join("target");
        std::fs::create_dir_all(target.join("sub")).unwrap();
        let junction = scratch.0.join("junction");
        // Junctions need no privilege; mklink /J is part of cmd.exe.
        let made = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&target)
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        // The junction as the output folder, and as a folder further up.
        assert!(write_output_file(&junction.join("report.txt"), "x").is_err());
        assert!(write_output_file(&junction.join("sub").join("report.txt"), "x").is_err());
        assert!(check_output_folder(&junction.join("previews")).is_err());
        assert!(!target.join("report.txt").exists());
        assert!(!target.join("sub").join("report.txt").exists());
        // The real folder itself is fine.
        write_output_file(&target.join("report.txt"), "ok").unwrap();
        check_output_folder(&target.join("previews")).unwrap();
        std::fs::remove_dir(&junction).unwrap();
    }

    #[test]
    fn cli_output_file_refuses_symbolic_links_when_they_can_be_made() {
        let scratch = Scratch::new("symlink");
        let target = scratch.0.join("protected.txt");
        std::fs::write(&target, "protected").unwrap();
        let link = scratch.0.join("report.txt");
        // Needs Developer Mode or the symbolic-link privilege.
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return;
        }
        assert!(write_output_file(&link, "attack").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "protected");
    }
}
