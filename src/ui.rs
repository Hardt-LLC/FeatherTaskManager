//! Native virtual tables; collection and management operations stay off the UI thread.
#![allow(clippy::needless_borrow)]
use crate::trf as tf;
use crate::{
    i18n::{language, set_language, tr, Language},
    netetw::{NetworkMonitor, ProcessNetworkSample},
    performance::{PerfSampler, PerfSnapshot, ProcessGpu, ProcessGpuTracker},
    process_tree::{Identity as ProcessIdentity, Row as TreeRow, TerminationPlan, Tree},
    sampler::{Process, Sampler, Snapshot},
    services::Service,
    startup::StartupEntry,
};
use std::{
    cmp::Ordering,
    collections::{HashSet, VecDeque},
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::{
        mpsc::{self, Receiver, Sender, SyncSender},
        Arc,
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    System::{
        LibraryLoader::*,
        Services::{SERVICE_RUNNING, SERVICE_STOPPED},
    },
    UI::{
        Controls::*,
        HiDpi::*,
        Input::KeyboardAndMouse::*,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::*,
    },
};
mod anim;
mod capture;
mod controls;
mod elevation;
mod fonts;
mod frame;
mod gfx;
// Raster nav/logo masks from before the vector icons (`widgets::icon`)
// matched the reference; unused, kept until the artwork pipeline is retired.
#[allow(dead_code)]
mod icons;
mod interactions;
mod layout;
mod navigation;
mod nuclear;
mod paint;
mod perf_order;
mod popup;
mod preferences;
mod process_columns;
mod resource_monitor;
mod run_task;
mod scroll;
mod shell;
mod table;
mod telemetry;
mod theme;
mod widgets;
use preferences::Preferences;
use telemetry::{PerfHistory, PerfTarget, ProcessTelemetry};
use theme::colors;
const SNAPSHOT_READY: u32 = WM_APP + 1;
const JOB_READY: u32 = WM_APP + 2;
const SEARCH: usize = 101;
const PAUSE: usize = 102;
const RATE: usize = 103;
const TOP: usize = 104;
const SETTINGS: usize = 105;
const PRIMARY: usize = 106;
const SECONDARY: usize = 107;
const REFRESH: usize = 108;
const ELEVATE: usize = 109;
const REPLACE_TASK_MANAGER: usize = 110;
const RESTORE_TASK_MANAGER: usize = 111;
const VIEW_MODE: usize = 112;
const END_TREE: usize = 113;
const LANGUAGE_KOREAN: usize = 114;
const LANGUAGE_ENGLISH: usize = 115;
const RUN_TASK: usize = 116;
const EXTRA: usize = 117;
const FILTER: usize = 118;
const COPY: usize = 119;
const CORES: usize = 120;
const RESOURCE_MONITOR: usize = 121;
const EXPAND_ALL: usize = 122;
const PERF_COMPONENT: usize = 123;
/// Icon-only "⋯" page-head button that opens the page's extra actions menu.
const MORE: usize = 124;
/// "Nuclear Zombie": the memory cleanup and zombie-process panel (`nuclear.rs`).
const NUCLEAR: usize = 125;
const THEME_LIGHT: usize = 130;
const THEME_DARK: usize = 131;
const THEME_SYSTEM: usize = 132;
const PREF_LANGUAGE: usize = 133;
const PREF_RATE: usize = 134;
const PREF_START: usize = 135;
const PREF_TOP: usize = 136;
const PREF_TRAY: usize = 137;
const PREF_REPLACE: usize = 138;
/// Settings → Window: "Always run as administrator" (`elevation.rs`).
const PREF_ADMIN: usize = 139;
const NAV: usize = 300;
/// One-shot timer of the main window: prefetch the Services / Startup lists.
const PREFETCH_TIMER: usize = 0xFE01;
const APP_ICON: *const u16 = 101usize as *const u16;
#[cfg(test)]
const fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | g << 8 | b << 16
}
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Processes = 0,
    Performance = 1,
    Startup = 2,
    Services = 3,
    Settings = 4,
}
impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Processes => tr("프로세스", "Processes"),
            Self::Performance => tr("성능", "Performance"),
            Self::Startup => tr("시작 앱", "Startup apps"),
            Self::Services => tr("서비스", "Services"),
            Self::Settings => tr("설정", "Settings"),
        }
    }
    #[allow(dead_code)]
    fn subtitle(self) -> &'static str {
        match self {
            Self::Settings => tr(
                "화면, 갱신 및 창 동작을 설정합니다.",
                "Customize appearance, updates and window behavior.",
            ),
            Self::Processes => tr(
                "리소스 사용량을 한눈에 확인하고, 필요한 작업에 집중하세요.",
                "See resource usage at a glance and manage running tasks.",
            ),
            Self::Performance => tr(
                "CPU, 메모리, 디스크와 네트워크의 최근 60초를 확인하세요.",
                "Explore the last 60 seconds of CPU, memory, disk and network activity.",
            ),
            Self::Startup => tr(
                "Windows에 로그인할 때 실행되는 데스크톱 앱을 관리하세요.",
                "Manage desktop apps that run when you sign in to Windows.",
            ),
            Self::Services => tr(
                "백그라운드 서비스의 상태를 확인하고 실행을 관리하세요.",
                "Check background services and manage their running state.",
            ),
        }
    }
    fn from_index(i: usize) -> Self {
        match i {
            1 => Self::Performance,
            2 => Self::Startup,
            3 => Self::Services,
            4 => Self::Settings,
            _ => Self::Processes,
        }
    }
}
#[allow(dead_code)] // Aggregate readiness regression fixture; device graphs use PerfHistory.
struct HistoryPoint {
    at: Instant,
    cpu: f64,
    memory: f64,
    disk: f64,
    network: f64,
}
enum Command {
    Metadata(crate::process_metadata::Needs),
    Resource(crate::resource::Request, bool, bool),
    /// Sampling interval and pause state. Every page samples the performance
    /// counters too (see `monitor`), so there is no per-page switch.
    Configure {
        interval: u64,
        paused: bool,
    },
    Refresh,
    Stop,
}
struct MonitorSample {
    at: Instant,
    /// An explicit one-shot refresh may update the main window while paused.
    manual_refresh: bool,
    snapshot: Result<Snapshot, String>,
    performance: Option<Result<PerfSnapshot, String>>,
    /// GPU use per `(pid, created)` of `snapshot`'s processes, joined with
    /// this iteration's performance sample (`ProcessGpuTracker`); empty when
    /// no performance sample was taken.
    process_gpu: std::collections::HashMap<(u32, u64), ProcessGpu>,
    /// Network rates per `(pid, created)` and their availability
    /// (`NetworkMonitor`, elevated only).
    process_network: ProcessNetworkSample,
    resource_data: Option<Arc<crate::resource::Snapshot>>,
    resource_files: crate::fileetw::Sample,
}
/// A grouped app row's values summed over its processes: CPU %, working
/// set, I/O rate (NaN when none measured), network bytes/s and GPU % (None
/// when no process of the app has a measured value; GPU capped at 100 %).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct GroupTotal {
    cpu: f64,
    memory: u64,
    io: f64,
    network: Option<f64>,
    gpu: Option<f64>,
    gpu_dedicated: Option<u64>,
    gpu_shared: Option<u64>,
    cpu_time: u64,
    threads: u32,
    handles: u32,
    private_bytes: u64,
}
impl GroupTotal {
    /// Sum `processes` (an app's root and members).
    fn of<'a>(processes: impl IntoIterator<Item = &'a Process>) -> Self {
        let mut total = Self {
            io: f64::NAN,
            gpu_dedicated: Some(0),
            gpu_shared: Some(0),
            ..Self::default()
        };
        let add = |sum: Option<f64>, value: Option<f64>| match (sum, value) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        for process in processes {
            total.cpu += process.cpu_percent;
            total.memory += process.working_set;
            if process.io_bytes_per_sec.is_finite() {
                total.io =
                    if total.io.is_finite() { total.io } else { 0.0 } + process.io_bytes_per_sec;
            }
            total.network = add(total.network, process.network_bytes_per_sec);
            total.gpu = add(total.gpu, process.gpu_percent);
            total.gpu_dedicated = total
                .gpu_dedicated
                .zip(process.gpu_dedicated_bytes)
                .map(|(a, b)| a.saturating_add(b));
            total.gpu_shared = total
                .gpu_shared
                .zip(process.gpu_shared_bytes)
                .map(|(a, b)| a.saturating_add(b));
            total.cpu_time = total.cpu_time.saturating_add(process.cpu_time_100ns);
            total.threads = total.threads.saturating_add(process.threads);
            total.handles = total.handles.saturating_add(process.handles);
            total.private_bytes = total.private_bytes.saturating_add(process.private_bytes);
        }
        total.gpu = total.gpu.map(|v| v.min(100.0));
        total
    }
    fn apply(&self, process: &mut Process) {
        process.cpu_percent = self.cpu;
        process.working_set = self.memory;
        process.io_bytes_per_sec = self.io;
        process.network_bytes_per_sec = self.network;
        process.gpu_percent = self.gpu;
        process.gpu_dedicated_bytes = self.gpu_dedicated;
        process.gpu_shared_bytes = self.gpu_shared;
        process.cpu_time_100ns = self.cpu_time;
        process.threads = self.threads;
        process.handles = self.handles;
        process.private_bytes = self.private_bytes;
    }
}
/// The per-process network availability of the latest sample.
#[derive(Clone, Debug, Default, PartialEq)]
struct NetworkState {
    measured: bool,
    /// Why per-process network is unavailable (from `netetw`), if known.
    reason: Option<String>,
}
enum Action {
    End(u32, u64),
    EndMany(Vec<(u32, u64)>),
    Priority(u32, u64, crate::actions::Priority),
    Efficiency(u32, u64, bool),
    /// Service key and display name (the toast names the service).
    Restart(String, String),
    SystemTool(crate::actions::SystemTool),
    RunTask(crate::actions::TaskLaunch),
    RestartExplorer(u32, u64),
    EndTree(TerminationPlan),
    Reveal(u32, u64),
    Properties(u32, u64),
    Toggle(Box<StartupEntry>, bool),
    Start(String, String),
    Stop(String, String),
    Elevate,
    ReplaceTaskManager(bool, Page),
}
enum Job {
    Navigate(navigation::Request),
    ProcessDetails(u32, u64),
    ServiceDetails(String),
    Startup,
    Services,
    Action(Action),
    /// A Nuclear Zombie run (`memclean::run`): progress and the report go
    /// to the panel's own channel, announced with `nuclear::CLEANUP_READY`.
    Cleanup(
        crate::memclean::CleanupOptions,
        Sender<nuclear::CleanupEvent>,
    ),
    Stop,
}
enum JobResult {
    Navigate(navigation::Request, Result<navigation::Target, String>),
    ProcessDetails(u32, u64, Result<crate::actions::ProcessSettings, String>),
    ServiceDetails(String, Result<crate::services::ServiceDetails, String>),
    Startup(Result<Vec<StartupEntry>, String>),
    Publishers(std::collections::HashMap<String, String>),
    Services(Result<Vec<Service>, String>),
    Action {
        result: Result<(), String>,
        page: Page,
        notice: String,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ErrorSource {
    General,
    Monitor,
    Startup,
    Services,
    Action,
}
struct App {
    metadata_needs: crate::process_metadata::Needs,
    persist_preferences: bool,
    palette: HWND,
    prefs: Preferences,
    /// The preferences as this window last read or saved them: a save
    /// writes only the values changed since (`Preferences::save`).
    stored_prefs: Preferences,
    process_columns: process_columns::Config,
    preference_controls: Vec<HWND>,
    tray_visible: bool,
    replacement_active: bool,
    show_telemetry: bool,
    show_details: bool,
    hwnd: HWND,
    list: HWND,
    search: HWND,
    pause: HWND,
    rate: HWND,
    top: HWND,
    settings: HWND,
    primary: HWND,
    secondary: HWND,
    refresh: HWND,
    view_mode: HWND,
    end_tree: HWND,
    run_task: HWND,
    extra: HWND,
    filter_control: HWND,
    copy: HWND,
    cores: HWND,
    resource_monitor: HWND,
    expand_all: HWND,
    more: HWND,
    /// Processes head: opens the Nuclear Zombie panel (it took the place of
    /// Efficiency mode, which stays in the ⋯ and context menus).
    nuclear: HWND,
    perf_list: HWND,
    nav: [HWND; 4],
    /// Every font role at the window's DPI and language (`fonts.rs`).
    fonts: fonts::Fonts,
    /// Cached frame for flicker-free WM_PAINT (`gfx.rs`).
    back: gfx::BackBuffer,
    /// Main-window animation driver; keys are `(control id, anim::part::*)`.
    anim: anim::AnimHost<(usize, u32)>,
    /// Custom frame: caption buttons, hit testing, DWM (`frame.rs`).
    frame: frame::Frame,
    big_icon: HICON,
    small_icon: HICON,
    bg: HBRUSH,
    surface: HBRUSH,
    dpi: i32,
    page: Page,
    snapshot: Option<Arc<Snapshot>>,
    performance: Option<Arc<PerfSnapshot>>,
    resource_window: HWND,
    resource_snapshot: Option<Arc<Snapshot>>,
    resource_performance: Option<Arc<PerfSnapshot>>,
    /// A services list only the Resource Monitor asked for while the
    /// Services page was paused (whose rows index `services`).
    resource_services: Option<Vec<Service>>,
    resource_network: Option<Arc<ProcessNetworkSample>>,
    resource_data: Option<Arc<crate::resource::Snapshot>>,
    resource_files: Option<Arc<crate::fileetw::Sample>>,
    resource_sample_at: Option<Instant>,
    resource_request: (crate::resource::Request, bool, bool),
    performance_error: Option<String>,
    /// Per-process network availability of the latest sample (None before
    /// the first one).
    network_state: Option<NetworkState>,
    history: VecDeque<HistoryPoint>,
    telemetry: ProcessTelemetry,
    perf_history: PerfHistory,
    perf_target: PerfTarget,
    perf_targets: Vec<PerfTarget>,
    perf_order: perf_order::Order,
    core_graphs: bool,
    category: usize,
    process_settings: Option<crate::actions::ProcessSettings>,
    process_detail_identity: Option<(u32, u64)>,
    service_details: Option<crate::services::ServiceDetails>,
    service_detail_name: Option<String>,
    service_detail_error: Option<String>,
    startup: Vec<StartupEntry>,
    startup_publishers: std::collections::HashMap<String, String>,
    services: Vec<Service>,
    rows: Vec<usize>,
    group_mode: bool,
    group_headers: std::collections::HashMap<usize, String>,
    window_pids: HashSet<u32>,
    last_window_scan: Option<Instant>,
    tree_mode: bool,
    tree_rows: Vec<TreeRow>,
    collapsed: HashSet<ProcessIdentity>,
    /// App groups the user opened (the grouped view starts collapsed).
    expanded_groups: HashSet<ProcessIdentity>,
    /// Grouped view: whether a process's executable is inside the Windows
    /// directory ("Windows processes"; None = unreadable), read once per
    /// process.
    windows_images: std::collections::HashMap<ProcessIdentity, Option<bool>>,
    /// Grouped view: an app row's summed values over the app's processes,
    /// keyed by the app root's snapshot index.
    group_totals: std::collections::HashMap<usize, GroupTotal>,
    filter: String,
    sort: usize,
    descending: bool,
    /// The user picked the sort column. Startup and Services show a sort
    /// arrow only then (the reference's headers there show none).
    sort_chosen: bool,
    paused: bool,
    minimized: bool,
    topmost: bool,
    updating: bool,
    modal: bool,
    busy: bool,
    interval: u64,
    hover: usize,
    startup_loading: bool,
    startup_refresh_pending: bool,
    /// A startup switch the user just flipped: (entry id, requested state).
    /// The switch shows the requested state at once (it slides on click, like
    /// the reference) until the reloaded list confirms it; a failed change
    /// clears it and the switch slides back.
    startup_pending: Option<(String, bool)>,
    services_loading: bool,
    /// The services job in flight is the Resource Monitor's alone.
    services_for_monitor: bool,
    startup_loaded: bool,
    services_loaded: bool,
    error: Option<String>,
    error_source: Option<ErrorSource>,
    notice: String,
    last_services: Instant,
    last_sample: Option<Instant>,
    rx: Receiver<MonitorSample>,
    tx: Sender<Command>,
    jobs: Sender<Job>,
    results: Receiver<JobResult>,
}
impl App {
    unsafe fn new(
        rx: Receiver<MonitorSample>,
        tx: Sender<Command>,
        jobs: Sender<Job>,
        results: Receiver<JobResult>,
    ) -> Self {
        Self {
            metadata_needs: Default::default(),
            persist_preferences: false,
            palette: null_mut(),
            prefs: Preferences::default(),
            stored_prefs: Preferences::default(),
            process_columns: process_columns::Config::default(),
            preference_controls: Vec::new(),
            tray_visible: false,
            replacement_active: false,
            show_telemetry: false,
            show_details: false,
            hwnd: null_mut(),
            list: null_mut(),
            search: null_mut(),
            pause: null_mut(),
            rate: null_mut(),
            top: null_mut(),
            settings: null_mut(),
            primary: null_mut(),
            secondary: null_mut(),
            refresh: null_mut(),
            view_mode: null_mut(),
            end_tree: null_mut(),
            run_task: null_mut(),
            extra: null_mut(),
            filter_control: null_mut(),
            copy: null_mut(),
            cores: null_mut(),
            resource_monitor: null_mut(),
            expand_all: null_mut(),
            more: null_mut(),
            nuclear: null_mut(),
            perf_list: null_mut(),
            nav: [null_mut(); 4],
            fonts: fonts::Fonts::empty(),
            back: gfx::BackBuffer::new(),
            anim: anim::AnimHost::new(anim::ANIM_TIMER_ID),
            frame: frame::Frame::new(),
            big_icon: null_mut(),
            small_icon: null_mut(),
            bg: CreateSolidBrush(colors().bg),
            surface: CreateSolidBrush(colors().surface),
            dpi: 96,
            page: Page::Processes,
            snapshot: None,
            performance: None,
            resource_window: null_mut(),
            resource_snapshot: None,
            resource_performance: None,
            resource_services: None,
            resource_network: None,
            resource_data: None,
            resource_files: None,
            resource_sample_at: None,
            resource_request: Default::default(),
            performance_error: None,
            network_state: None,
            history: VecDeque::with_capacity(120),
            telemetry: ProcessTelemetry::default(),
            perf_history: PerfHistory::default(),
            perf_target: PerfTarget::Cpu,
            perf_targets: vec![PerfTarget::Cpu, PerfTarget::Memory],
            perf_order: perf_order::Order::default(),
            core_graphs: false,
            category: 0,
            process_settings: None,
            process_detail_identity: None,
            service_details: None,
            service_detail_name: None,
            service_detail_error: None,
            startup: Vec::new(),
            startup_publishers: std::collections::HashMap::new(),
            services: Vec::new(),
            rows: Vec::new(),
            group_mode: false,
            group_headers: std::collections::HashMap::new(),
            window_pids: HashSet::new(),
            last_window_scan: None,
            tree_mode: false,
            tree_rows: Vec::new(),
            collapsed: HashSet::new(),
            expanded_groups: HashSet::new(),
            windows_images: std::collections::HashMap::new(),
            group_totals: std::collections::HashMap::new(),
            filter: String::new(),
            sort: 3,
            descending: true,
            sort_chosen: false,
            paused: false,
            minimized: false,
            topmost: false,
            updating: false,
            modal: false,
            busy: false,
            interval: 1000,
            hover: 0,
            startup_loading: false,
            startup_refresh_pending: false,
            startup_pending: None,
            services_loading: false,
            services_for_monitor: false,
            startup_loaded: false,
            services_loaded: false,
            error: None,
            error_source: None,
            notice: String::new(),
            last_services: Instant::now(),
            last_sample: None,
            rx,
            tx,
            jobs,
            results,
        }
    }
    fn clear_error(&mut self) {
        self.error = None;
        self.error_source = None;
    }
    fn set_error(&mut self, source: ErrorSource, error: String) {
        // Keep management failures visible until an explicit user action.
        // Background refreshes must neither replace nor dismiss that result.
        if self.error_source == Some(ErrorSource::Action) && source != ErrorSource::Action {
            return;
        }
        self.error = Some(error);
        self.error_source = Some(source);
    }
    fn recover_error(&mut self, source: ErrorSource) {
        if self.error_source == Some(source) {
            self.clear_error();
        }
    }
}
unsafe fn init_controls() {
    // GDI+ must be running before any window paints (idempotent).
    gfx::startup();
    InitCommonControlsEx(&INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_STANDARD_CLASSES,
    });
}
unsafe fn create_window(p: *mut App, class: &[u16], width: i32, height: i32) -> HWND {
    let instance = GetModuleHandleW(null());
    RegisterClassExW(&WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        // Shared class fallback; each window gets owned icons at its actual DPI.
        hIcon: LoadIconW(instance, APP_ICON),
        hIconSm: LoadIconW(instance, APP_ICON),
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: class.as_ptr(),
        ..zeroed()
    });
    CreateWindowExW(
        WS_EX_APPWINDOW,
        class.as_ptr(),
        wide("Feather Task Manager").as_ptr(),
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        width,
        height,
        null_mut(),
        null_mut(),
        instance,
        p.cast(),
    )
}
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    let prefs = Preferences::load();
    // "Always run as administrator": before any window, so an approved
    // prompt never shows a second, unelevated window first.
    let elevation_notice = match elevation::relaunch(&args, &prefs) {
        elevation::Startup::Relaunched => return,
        elevation::Startup::Unelevated(notice) => Some(notice),
        elevation::Startup::Continue => None,
    };
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        init_controls();
        let (snapshots, rx) = mpsc::sync_channel(1);
        let (tx, commands) = mpsc::channel();
        let (jobs, work) = mpsc::channel();
        let (complete, results) = mpsc::channel();
        let p = Box::into_raw(Box::new(App::new(rx, tx, jobs, results)));
        (*p).persist_preferences = true;
        (*p).perf_order = perf_order::Order::load();
        (*p).group_mode = true;
        (*p).stored_prefs = prefs.clone();
        (*p).prefs = prefs;
        (*p).process_columns = process_columns::Config::load();
        (*p).interval = (*p).prefs.rate;
        (*p).topmost = (*p).prefs.topmost;
        (*p).prefs.apply_theme();
        let dpi = GetDpiForSystem().max(96) as i32;
        let class = wide("FeatherTaskManagerWindow");
        // The window is its client plus the invisible resize borders (the
        // custom frame has no caption): the reference's 1200 × 820 window.
        let (width, height) = frame::outer_size(dpi as u32, 1200 * dpi / 96, 820 * dpi / 96, 0);
        let hwnd = create_window(
            p,
            &class,
            width.min(GetSystemMetrics(SM_CXSCREEN).saturating_sub(32)),
            height.min(GetSystemMetrics(SM_CYSCREEN).saturating_sub(64)),
        );
        if hwnd.is_null() {
            MessageBoxW(
                null_mut(),
                wide(tr(
                    "창을 만들지 못했습니다.",
                    "Unable to create the window.",
                ))
                .as_ptr(),
                wide("Feather Task Manager").as_ptr(),
                MB_ICONERROR,
            );
            dispose(p);
            return;
        }
        let handle = hwnd as usize;
        let monitor_thread = std::thread::Builder::new()
            .name("feather-monitor".into())
            .spawn(move || monitor(handle, commands, snapshots));
        if let Err(e) = &monitor_thread {
            (*p).set_error(
                ErrorSource::General,
                tf!(
                    "모니터링 작업을 시작할 수 없습니다: {e}",
                    "Unable to start monitoring: {e}"
                ),
            );
        }
        if let Err(e) = std::thread::Builder::new()
            .name("feather-actions".into())
            .spawn(move || job_worker(handle, work, complete))
        {
            (*p).set_error(
                ErrorSource::General,
                tf!(
                    "관리 작업을 시작할 수 없습니다: {e}",
                    "Unable to start management tasks: {e}"
                ),
            );
        }
        let requested = if args.iter().any(|a| a == "--page" || a == "--task-manager") {
            initial_page(&args)
        } else {
            Page::from_index((*p).prefs.default_page as usize)
        };
        switch_page(p, requested);
        if let Some(notice) = elevation_notice {
            // After switch_page, which clears the notice.
            (*p).notice = notice;
        }
        interactions::apply_theme(p);
        interactions::sync_settings(p);
        if (*p).topmost {
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        configure(p);
        // Load the Services and Startup lists in the background shortly
        // after launch, so their first visit shows rows at once instead of
        // "Loading the list…".
        SetTimer(hwnd, PREFETCH_TIMER, 1500, None);
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        if args.iter().any(|arg| arg == "--resource-monitor") {
            resource_monitor::open(p);
        }
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            controls::observe_input(&msg);
            if keyboard(p, &msg) {
                continue;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        dispose(p);
        if let Ok(thread) = monitor_thread {
            // The monitor owns the ETW sessions; let it stop them on Stop rather
            // than ending the process under it. Bounded, so exit never hangs.
            let deadline = Instant::now() + Duration::from_secs(2);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
        UnregisterClassW(class.as_ptr(), GetModuleHandleW(null()));
        gfx::buffered_paint_shutdown();
        gfx::shutdown();
    }
}
fn initial_page(args: &[String]) -> Page {
    // IFEO appends taskmgr.exe and its arguments. They belong to Windows,
    // not to Feather's own command-line interface.
    if args.get(1).map(String::as_str) == Some("--task-manager") {
        return Page::Processes;
    }
    match args
        .windows(2)
        .find(|pair| pair[0] == "--page")
        .map(|pair| pair[1].as_str())
    {
        Some("performance") => Page::Performance,
        Some("startup") => Page::Startup,
        Some("services") => Page::Services,
        _ => Page::Processes,
    }
}
fn monitor(hwnd: usize, commands: Receiver<Command>, snapshots: SyncSender<MonitorSample>) {
    let mut sampler = Sampler::new();
    // The performance counters are sampled on every page, like Windows Task
    // Manager keeps its history while another tab is open: the Performance
    // charts (and the Processes GPU column) continue without a gap when the
    // user comes back. The sampler stays warm across page switches; only a
    // pause (also minimized or modal) drops it, and that gap is recorded.
    let mut perf: Option<PerfSampler> = None;
    // Firmware temperatures keep their cache and retry schedule across those
    // pauses: the sampler moves out of a dropped PerfSampler into the next.
    let mut thermal = crate::thermal::ThermalSampler::default();
    let resume_perf = |thermal: &mut crate::thermal::ThermalSampler| {
        PerfSampler::new().map(|sampler| sampler.with_thermal(std::mem::take(thermal)))
    };
    // One GPU attribution tracker and one network trace for the monitor's
    // lifetime; the trace (an ETW session, elevated only) ends when this
    // thread returns on Stop.
    let mut gpu_tracker = ProcessGpuTracker::default();
    let mut network = NetworkMonitor::new();
    let mut resource = crate::resource::Client::default();
    let mut resource_request = crate::resource::Request::default();
    let mut files = crate::fileetw::FileMonitor::new();
    // A crash or kill skips the sessions' Drop; clear such leftovers now,
    // not only when file tracing is next turned on.
    crate::fileetw::stop_stale_sessions();
    let mut metadata = crate::process_metadata::Client::default();
    let mut metadata_needs = crate::process_metadata::Needs::default();
    let mut interval = 1000;
    let mut paused = false;
    let mut refresh = true;
    let mut manual_refresh = false;
    // What a sample collects changed since the last one (a panel opened, a
    // column or trace turned on): the next Configure samples at once.
    let mut new_work = false;
    let mut traces = (false, false);
    // When the last sample was taken: the next one is due one interval
    // later, whatever commands arrive in between.
    let mut last: Option<Instant> = None;
    loop {
        if refresh {
            new_work = false;
            let elapsed = last.map_or(Duration::from_millis(interval), |at| at.elapsed());
            last = Some(Instant::now());
            if sampler.is_err() {
                sampler = Sampler::new();
            }
            let mut snapshot = match &mut sampler {
                Ok(s) => s.sample(),
                Err(e) => Err(e.clone()),
            };
            if let Ok(snapshot) = &mut snapshot {
                metadata.decorate(&mut snapshot.processes, metadata_needs);
            }
            let processes = snapshot.as_ref().map_or(&[][..], |s| &s.processes);
            let resource_data = resource.sample(&resource_request, processes);
            let resource_files = files.sample(processes, elapsed);
            if perf.is_none() {
                match resume_perf(&mut thermal) {
                    Ok(s) => perf = Some(s),
                    Err(e) => {
                        let value = MonitorSample {
                            at: Instant::now(),
                            manual_refresh,
                            snapshot,
                            performance: Some(Err(e)),
                            process_gpu: Default::default(),
                            process_network: Default::default(),
                            resource_data,
                            resource_files,
                        };
                        if snapshots.try_send(value).is_ok() {
                            manual_refresh = false;
                            unsafe {
                                PostMessageW(hwnd as HWND, SNAPSHOT_READY, 0, 0);
                            }
                        }
                        refresh = false;
                        continue;
                    }
                }
            }
            let perf_result = perf.as_mut().map(PerfSampler::sample);
            // Join both per-process maps with this iteration's process list
            // (keyed by pid and creation time, so a reused PID never
            // inherits a value).
            let (process_gpu, process_network) = match &snapshot {
                Ok(s) => (
                    gpu_tracker.join(
                        &s.processes,
                        perf_result.as_ref().and_then(|r| r.as_ref().ok()),
                    ),
                    network.sample(&s.processes),
                ),
                Err(_) => Default::default(),
            };
            if snapshots
                .try_send(MonitorSample {
                    at: Instant::now(),
                    manual_refresh,
                    snapshot,
                    performance: perf_result,
                    process_gpu,
                    process_network,
                    resource_data,
                    resource_files,
                })
                .is_ok()
            {
                manual_refresh = false;
                unsafe {
                    PostMessageW(hwnd as HWND, SNAPSHOT_READY, 0, 0);
                }
            }
        }
        // A full delivery slot must not lose F5 while paused. Retry on the
        // normal interval until that one requested frame has been delivered.
        let result = if paused && !manual_refresh {
            commands
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            let period = Duration::from_millis(interval);
            commands.recv_timeout(last.map_or(period, |at| period.saturating_sub(at.elapsed())))
        };
        match result {
            Ok(Command::Resource(request, file_trace, endpoint_trace)) => {
                new_work |= request != resource_request || (file_trace, endpoint_trace) != traces;
                resource_request = request;
                traces = (file_trace, endpoint_trace);
                // Stop optional work even when both windows are paused and
                // no further sampling iteration is scheduled.
                if request == crate::resource::Request::default() {
                    let _ = resource.sample(&request, &[]);
                }
                files.set_enabled(file_trace);
                network.set_endpoint_capture(endpoint_trace);
                refresh = false;
            }
            Ok(Command::Metadata(needs)) => {
                new_work |= needs != metadata_needs;
                metadata_needs = needs;
                refresh = false;
            }
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Configure {
                interval: i,
                paused: p,
            }) => {
                // Resizes, keystrokes and page switches re-send the same
                // configuration: only a change samples ahead of schedule.
                let changed = i != interval || p != paused || std::mem::take(&mut new_work);
                interval = i;
                if p && !paused {
                    // Bytes counted while paused are not shown: the first
                    // sample after resuming starts a fresh interval (every
                    // process reads "—" once, like GPU after a pause).
                    let _ = network.sample(&[]);
                }
                paused = p;
                if paused {
                    // Rates after a pause start over (the UI records the gap).
                    if let Some(sampler) = perf.take() {
                        thermal = sampler.into_thermal();
                    }
                }
                // A sample only milliseconds after the previous one measures
                // CPU over a sliver of time: a needle at the start of every
                // chart (the app configures itself twice while starting). A
                // recent sample stands; the next comes on schedule, and a
                // performance sampler dropped by a pause is primed now (its
                // first collection has no rates anyway).
                let recent = last.is_some_and(|at| {
                    at.elapsed() < Duration::from_millis((interval / 2).max(100))
                });
                refresh = manual_refresh || (changed && !paused && !recent);
                if !paused && recent && perf.is_none() {
                    if let Ok(mut sampler) = resume_perf(&mut thermal) {
                        let _ = sampler.sample();
                        perf = Some(sampler);
                    }
                }
            }
            Ok(Command::Refresh) => {
                manual_refresh = true;
                refresh = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => refresh = true,
        }
    }
}
/// A service's name for a notice: its display name, else its key.
fn service_label<'a>(name: &'a str, display: &'a str) -> &'a str {
    if display.trim().is_empty() {
        name
    } else {
        display
    }
}
/// The toast after a startup app was switched on or off.
fn startup_notice(name: &str, enabled: bool) -> String {
    if enabled {
        tf!(
            "{}: 다음 로그인부터 실행됩니다",
            "{} will launch at startup",
            name
        )
    } else {
        tf!(
            "{}: 다음 로그인부터 실행되지 않습니다",
            "{} won\u{2019}t launch at startup",
            name
        )
    }
}
fn job_worker(hwnd: usize, jobs: Receiver<Job>, complete: Sender<JobResult>) {
    while let Ok(job) = jobs.recv() {
        let job = match job {
            Job::Cleanup(options, events) => {
                nuclear::run_job(hwnd, options, &events);
                continue;
            }
            job => job,
        };
        let result = match job {
            Job::Stop => break,
            Job::Navigate(request) => {
                let result = navigation::resolve(&request);
                JobResult::Navigate(request, result)
            }
            Job::Cleanup(..) => unreachable!(),
            Job::ProcessDetails(pid, created) => JobResult::ProcessDetails(
                pid,
                created,
                crate::actions::process_settings(pid, created),
            ),
            Job::ServiceDetails(name) => {
                let result = crate::services::details(&name);
                JobResult::ServiceDetails(name, result)
            }
            Job::Startup => {
                let result = crate::startup::list();
                if let Ok(entries) = &result {
                    let publishers = entries
                        .iter()
                        .filter_map(|e| crate::startup::publisher(e).map(|v| (e.id.clone(), v)))
                        .collect();
                    let _ = complete.send(JobResult::Publishers(publishers));
                }
                JobResult::Startup(result)
            }
            Job::Services => JobResult::Services(crate::services::list()),
            Job::Action(action) => {
                if let Action::EndTree(plan) = action {
                    let result = crate::actions::terminate_tree(&plan);
                    let (result, notice) = match result {
                        Ok(notice) => (Ok(()), notice),
                        Err(error) => (Err(error), String::new()),
                    };
                    if complete
                        .send(JobResult::Action {
                            result,
                            page: Page::Processes,
                            notice,
                        })
                        .is_err()
                    {
                        break;
                    }
                    unsafe {
                        PostMessageW(hwnd as HWND, JOB_READY, 0, 0);
                    }
                    continue;
                }
                let (result, page, notice) = match action {
                    Action::EndTree(_) => unreachable!(),
                    Action::Priority(pid, created, priority) => (
                        crate::actions::set_priority(pid, created, priority),
                        Page::Processes,
                        tr("우선순위를 변경했습니다.", "Updated process priority.").into(),
                    ),
                    Action::Efficiency(pid, created, enabled) => (
                        crate::actions::set_efficiency(pid, created, enabled),
                        Page::Processes,
                        tr("효율 모드를 변경했습니다.", "Updated efficiency mode.").into(),
                    ),
                    Action::Restart(name, display) => (
                        crate::services::restart(&name),
                        Page::Services,
                        tf!(
                            "{} 서비스를 다시 시작했습니다.",
                            "Restarted {}",
                            service_label(&name, &display)
                        ),
                    ),
                    Action::SystemTool(tool) => (
                        crate::actions::launch_system_tool(tool),
                        Page::Performance,
                        tr("Windows 도구를 열었습니다.", "Opened the Windows tool.").into(),
                    ),
                    Action::RunTask(task) => {
                        let result = crate::actions::launch_task(&task, hwnd);
                        let notice = if matches!(result, Ok(false)) {
                            tr(
                                "관리자 권한 실행을 취소했습니다.",
                                "Administrator launch cancelled.",
                            )
                        } else {
                            tr("새 작업을 실행했습니다.", "Started the new task.")
                        };
                        (result.map(|_| ()), Page::Processes, notice.into())
                    }
                    Action::RestartExplorer(pid, created) => (
                        crate::actions::restart_explorer(pid, created),
                        Page::Processes,
                        tr(
                            "Windows 탐색기를 다시 시작했습니다.",
                            "Restarted Windows Explorer.",
                        )
                        .into(),
                    ),
                    Action::End(pid, created) => (
                        crate::actions::terminate(pid, created),
                        Page::Processes,
                        tr(
                            "종료 요청을 보냈습니다.",
                            "The process termination request was sent.",
                        )
                        .into(),
                    ),
                    Action::EndMany(identities) => {
                        let mut errors = Vec::new();
                        let mut ended = 0;
                        for (pid, created) in identities {
                            match crate::actions::terminate(pid, created) {
                                Ok(()) => ended += 1,
                                Err(error) => errors.push(format!("{pid}: {error}")),
                            }
                        }
                        let notice = tf!(
                            "{}개 프로세스에 종료 요청을 보냈습니다.",
                            "Sent termination requests to {} processes.",
                            ended
                        );
                        let result = if errors.is_empty() {
                            Ok(())
                        } else {
                            Err(format!("{notice}\n{}", errors.join("\n")))
                        };
                        (result, Page::Processes, notice)
                    }
                    Action::Reveal(pid, created) => (
                        crate::actions::reveal_executable(pid, created),
                        Page::Processes,
                        tr("파일 위치를 열었습니다.", "Opened the file location.").into(),
                    ),
                    Action::Properties(pid, created) => (
                        crate::actions::show_properties(pid, created),
                        Page::Processes,
                        tr("파일 속성을 열었습니다.", "Opened file properties.").into(),
                    ),
                    Action::Toggle(entry, enabled) => (
                        crate::startup::set_enabled(&entry, enabled),
                        Page::Startup,
                        // Sent only after the change succeeded, naming the app
                        // and the outcome like the reference's toast.
                        startup_notice(&entry.name, enabled),
                    ),
                    Action::Start(name, display) => (
                        crate::services::start(&name),
                        Page::Services,
                        tf!(
                            "{} 서비스 시작 요청을 보냈습니다.",
                            "Start request sent: {}",
                            service_label(&name, &display)
                        ),
                    ),
                    Action::Stop(name, display) => (
                        crate::services::stop(&name),
                        Page::Services,
                        tf!(
                            "{} 서비스 중지 요청을 보냈습니다.",
                            "Stop request sent: {}",
                            service_label(&name, &display)
                        ),
                    ),
                    Action::Elevate => (
                        crate::actions::relaunch_elevated(),
                        Page::Processes,
                        tr(
                            "관리자 권한 창을 열었습니다.",
                            "Opened an administrator window.",
                        )
                        .into(),
                    ),
                    Action::ReplaceTaskManager(enable, page) => (
                        crate::replacement::run_elevated(enable),
                        page,
                        if enable {
                            tr("Feather를 Windows 작업 관리자로 설정했습니다. 다음 실행부터 적용됩니다.", "Feather is now the Windows Task Manager. The change applies next time you open it.").into()
                        } else {
                            tr(
                                "Windows 기본 작업 관리자로 복원했습니다.",
                                "Restored the default Windows Task Manager.",
                            )
                            .into()
                        },
                    ),
                };
                JobResult::Action {
                    result,
                    page,
                    notice,
                }
            }
        };
        if complete.send(result).is_err() {
            break;
        }
        unsafe {
            PostMessageW(hwnd as HWND, JOB_READY, 0, 0);
        }
    }
}
unsafe fn dispose(p: *mut App) {
    resource_monitor::close(p);
    shell::destroy_palette(p);
    shell::remove_tray(p);
    let app = Box::from_raw(p);
    let _ = app.tx.send(Command::Stop);
    let _ = app.jobs.send(Job::Stop);
    for icon in [app.big_icon, app.small_icon] {
        if !icon.is_null() {
            DestroyIcon(icon);
        }
    }
    // Fonts, the back buffer and the animation timer are released by their owners.
    for object in [app.bg, app.surface] {
        if !object.is_null() {
            DeleteObject(object);
        }
    }
}
unsafe fn keyboard(p: *mut App, msg: &MSG) -> bool {
    if resource_monitor::keyboard(p, msg) {
        return true;
    }
    if !(*p).resource_window.is_null()
        && (msg.hwnd == (*p).resource_window || IsChild((*p).resource_window, msg.hwnd) != 0)
    {
        return false;
    }
    if shell::palette_key(p, msg) {
        return true;
    }
    if msg.message != WM_KEYDOWN {
        return false;
    }
    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
    let key = msg.wParam as u16;
    if ctrl && key == b'K' as u16 {
        shell::open_palette(p);
        return true;
    }
    let id = if ctrl && (b'1' as u16..=b'4' as u16).contains(&key) {
        NAV + (key - b'1' as u16) as usize
    } else if ctrl
        && key == b'F' as u16
        && (*p).page != Page::Performance
        && (*p).page != Page::Settings
    {
        SetFocus((*p).search);
        SendMessageW((*p).search, EM_SETSEL, 0, -1);
        return true;
    } else if key == VK_F5 {
        REFRESH
    } else if key == VK_ESCAPE && msg.hwnd == (*p).search {
        SetWindowTextW((*p).search, wide("").as_ptr());
        return true;
    } else if key == VK_DELETE && msg.hwnd == (*p).list && (*p).page == Page::Processes {
        if GetKeyState(VK_SHIFT as i32) < 0 {
            END_TREE
        } else {
            PRIMARY
        }
    } else if matches!(key, VK_LEFT | VK_RIGHT)
        && msg.hwnd == (*p).list
        && ((*p).tree_mode || grouped_view(p))
        && (*p).page == Page::Processes
    {
        toggle_selected_branch(p, Some(key == VK_RIGHT));
        return true;
    } else if key == VK_SPACE && msg.hwnd == (*p).list {
        if (*p).page == Page::Startup {
            PRIMARY
        } else {
            PAUSE
        }
    } else if ctrl && key == b'L' as u16 && (*p).page == Page::Processes {
        SECONDARY
    } else {
        return false;
    };
    SendMessageW((*p).hwnd, WM_COMMAND, id, 0);
    true
}
unsafe fn state(hwnd: HWND) -> *mut App {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App
}
/// Explorer's "TaskbarCreated" broadcast (it restarted): re-add the tray icon.
fn taskbar_created_message() -> u32 {
    static TASKBAR_CREATED: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *TASKBAR_CREATED
        .get_or_init(|| unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) })
}
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(l as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        (*(cs.lpCreateParams as *mut App)).hwnd = hwnd;
    }
    let p = state(hwnd);
    if p.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    if msg != 0 && msg == taskbar_created_message() {
        shell::taskbar_created(p);
        return 0;
    }
    if let Some(result) = frame::handle(p, hwnd, msg, w, l) {
        return result;
    }
    match msg {
        WM_CREATE => {
            (*p).dpi = GetDpiForWindow(hwnd).max(96) as i32;
            (*p).anim.attach(hwnd);
            // UIPI drops Explorer's (medium integrity) broadcast to an
            // elevated window, so a restarted Explorer would never get the
            // hidden tray icon back. Only this message is let through; its
            // handler just re-adds the icon.
            let taskbar_created = taskbar_created_message();
            if taskbar_created != 0 {
                ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, null_mut());
            }
            update_window_icons(p);
            create_controls(p);
            frame::attach(p);
            0
        }
        WM_SETTINGCHANGE => {
            anim::refresh_reduced_motion();
            if (*p).prefs.theme == 0 {
                interactions::apply_theme(p);
            }
            0
        }
        shell::TRAY_MESSAGE => {
            shell::tray_message(p, l);
            0
        }
        WM_SIZE => {
            let minimized = w == SIZE_MINIMIZED as usize;
            if minimized != (*p).minimized {
                (*p).minimized = minimized;
                configure(p);
                if minimized && (*p).prefs.tray {
                    shell::minimize_to_tray(p);
                }
            }
            if !minimized {
                layout(p);
            }
            0
        }
        WM_GETMINMAXINFO => {
            let m = &mut *(l as *mut MINMAXINFO);
            m.ptMinTrackSize = frame::min_track_size(p);
            0
        }
        WM_DPICHANGED => {
            (*p).dpi = (w & 0xffff) as i32;
            update_window_icons(p);
            create_fonts(p);
            let r = &*(l as *const RECT);
            SetWindowPos(
                hwnd,
                null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            layout(p);
            0
        }
        WM_COMMAND => {
            command(p, w & 0xffff, (w >> 16) as u32);
            0
        }
        WM_NOTIFY => notify(p, l),
        SNAPSHOT_READY => {
            drain_snapshot(p);
            0
        }
        JOB_READY => {
            drain_jobs(p);
            0
        }
        // A cleanup run finished after its panel was closed.
        nuclear::CLEANUP_READY => {
            nuclear::background_event(p);
            0
        }
        WM_CONTEXTMENU if w as HWND == (*p).list => {
            // Apps / Shift+F10 open the menu at the selected row.
            match table::keyboard_menu_anchor((*p).list, l) {
                Some(anchor) => interactions::extra_menu_below(p, anchor),
                None => interactions::extra_menu(p),
            }
            0
        }
        WM_CONTEXTMENU if w as HWND == (*p).settings => {
            settings_menu(p);
            0
        }
        WM_DRAWITEM => {
            paint::draw_button(p, &*(l as *const DRAWITEMSTRUCT));
            1
        }
        WM_PAINT => {
            paint::paint(p);
            0
        }
        WM_TIMER if w == PREFETCH_TIMER => {
            KillTimer(hwnd, PREFETCH_TIMER);
            for (page, loaded) in [
                (Page::Services, (*p).services_loaded),
                (Page::Startup, (*p).startup_loaded),
            ] {
                if !loaded && !(*p).modal {
                    request_list(p, page);
                }
            }
            0
        }
        WM_TIMER => {
            if (*p).anim.on_timer(w) {
                0
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        WM_PRINTCLIENT => {
            paint::paint_to(p, w as HDC);
            0
        }
        WM_ERASEBKGND => 1,
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            SetBkColor(w as HDC, colors().surface);
            SetTextColor(w as HDC, colors().fg);
            (*p).surface as isize
        }
        // A disabled edit asks for static colors; the search input keeps the
        // search box's surface so it never shows as a plain rectangle.
        WM_CTLCOLORSTATIC if l as HWND == (*p).search => {
            SetBkColor(w as HDC, colors().surface);
            SetTextColor(w as HDC, colors().muted);
            (*p).surface as isize
        }
        WM_CTLCOLORSTATIC => {
            SetBkColor(w as HDC, colors().bg);
            SetTextColor(w as HDC, colors().fg);
            (*p).bg as isize
        }
        // Popups anchored to the window (dropdowns, toast) do not follow it.
        WM_MOVE => {
            controls::owner_moved(p);
            0
        }
        WM_CLOSE => {
            if !(*p).modal {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            resource_monitor::close(p);
            (*p).anim.stop();
            let _ = (*p).tx.send(Command::Stop);
            let _ = (*p).jobs.send(Job::Stop);
            PostQuitMessage(0);
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}
unsafe fn scale(p: *mut App, v: i32) -> i32 {
    v * (*p).dpi / 96
}
unsafe fn update_window_icons(p: *mut App) {
    let dpi = (*p).dpi.max(96) as u32;
    for (kind, horizontal, vertical, slot) in [
        (ICON_BIG, SM_CXICON, SM_CYICON, &mut (*p).big_icon),
        (ICON_SMALL, SM_CXSMICON, SM_CYSMICON, &mut (*p).small_icon),
    ] {
        // No LR_SHARED: its resource-name cache may return the wrong size.
        let icon = LoadImageW(
            GetModuleHandleW(null()),
            APP_ICON,
            IMAGE_ICON,
            GetSystemMetricsForDpi(horizontal, dpi),
            GetSystemMetricsForDpi(vertical, dpi),
            LR_DEFAULTCOLOR,
        ) as HICON;
        if !icon.is_null() {
            SendMessageW((*p).hwnd, WM_SETICON, kind as usize, icon as isize);
            let old = std::mem::replace(slot, icon);
            if !old.is_null() {
                DestroyIcon(old);
            }
        }
    }
}
unsafe fn redraw(p: *mut App) {
    InvalidateRect((*p).hwnd, null(), 0);
}
/// Stop the main window and every child from painting (`WM_SETREDRAW`)
/// while a page switch or a theme change rebuilds them; [`present_all`]
/// puts the result on screen. Hidden windows (tests, previews) are left
/// alone: re-enabling redraw would give them WS_VISIBLE. Returns whether
/// painting was suspended.
unsafe fn suspend_painting(p: *mut App) -> bool {
    let hwnd = (*p).hwnd;
    if hwnd.is_null() || IsWindowVisible(hwnd) == 0 {
        return false;
    }
    SendMessageW(hwnd, WM_SETREDRAW, 0, 0);
    true
}
/// Resume painting after [`suspend_painting`] and present the whole new
/// frame at once: the main window and the table compose into their back
/// buffers first, then — right after a DWM composition, so the copies land
/// in one frame — both are copied to the screen back to back and the small
/// child controls paint synchronously. No presented frame mixes the old
/// page (or theme) with the new one.
unsafe fn present_all(p: *mut App, suspended: bool) {
    let hwnd = (*p).hwnd;
    if !suspended {
        RedrawWindow(
            hwnd,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_FRAME,
        );
        return;
    }
    SendMessageW(hwnd, WM_SETREDRAW, 1, 0);
    let main = paint::render_back(p);
    let table = table::render_back((*p).list);
    windows_sys::Win32::Graphics::Dwm::DwmFlush();
    let mut client: RECT = zeroed();
    GetClientRect(hwnd, &mut client);
    let dc = GetDC(hwnd);
    if main && !dc.is_null() {
        (*p).back.present(dc, &client);
        ValidateRect(hwnd, null());
    } else {
        InvalidateRect(hwnd, null(), 0);
    }
    if !dc.is_null() {
        ReleaseDC(hwnd, dc);
    }
    if table {
        table::present_back((*p).list);
    }
    let mut child = GetWindow(hwnd, GW_CHILD);
    while !child.is_null() {
        if (child != (*p).list || !table) && IsWindowVisible(child) != 0 {
            RedrawWindow(
                child,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
            );
        }
        child = GetWindow(child, GW_HWNDNEXT);
    }
    UpdateWindow(hwnd);
}
unsafe fn create_control(p: *mut App, class: &str, label: &str, style: u32, id: usize) -> HWND {
    CreateWindowExW(
        0,
        wide(class).as_ptr(),
        wide(label).as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        0,
        0,
        0,
        0,
        (*p).hwnd,
        id as HMENU,
        GetModuleHandleW(null()),
        null(),
    )
}
unsafe fn button(p: *mut App, label: &str, id: usize) -> HWND {
    let hwnd = create_control(p, "Button", label, WS_TABSTOP | BS_OWNERDRAW as u32, id);
    SetWindowSubclass(hwnd, Some(button_subclass), id, p as usize);
    hwnd
}
unsafe extern "system" fn button_subclass(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let p = data as *mut App;
    match msg {
        WM_MOUSEMOVE if (*p).hover != id => {
            (*p).hover = id;
            let mut e = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut e);
            // Hover backgrounds cross-fade in (spec section 6); the driver
            // repaints only this button while the fade runs.
            (*p).anim.register((id, anim::part::HOVER), hwnd, None);
            (*p).anim.set_target(
                (id, anim::part::HOVER),
                1.0,
                anim::motion::HOVER_IN,
                anim::Easing::EaseOut,
            );
            InvalidateRect(hwnd, null(), 0);
        }
        WM_MOUSELEAVE => {
            if (*p).hover == id {
                (*p).hover = 0;
            }
            (*p).anim.set_target(
                (id, anim::part::HOVER),
                0.0,
                anim::motion::HOVER_OUT,
                anim::Easing::EaseOut,
            );
            InvalidateRect(hwnd, null(), 0);
        }
        // The face is painted completely (buffered) in WM_DRAWITEM.
        WM_ERASEBKGND => return 1,
        // `button { cursor: pointer }` (a disabled button never gets here:
        // its parent shows the arrow).
        WM_SETCURSOR if w as HWND == hwnd && IsWindowEnabled(hwnd) != 0 => {
            return widgets::set_pointer(true);
        }
        // `:focus-visible`: a click never shows the keyboard focus ring.
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => controls::pointer_pressed(hwnd),
        WM_SETFOCUS | WM_KILLFOCUS | WM_ENABLE | WM_UPDATEUISTATE => {
            controls::focus_changed(p, hwnd);
            InvalidateRect(hwnd, null(), 0);
        }
        WM_WINDOWPOSCHANGED if GetFocus() == hwnd => controls::focus_changed(p, hwnd),
        WM_NCDESTROY => {
            RemoveWindowSubclass(hwnd, Some(button_subclass), id);
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, w, l)
}
unsafe fn create_controls(p: *mut App) {
    for i in 0..4 {
        (*p).nav[i] = button(p, Page::from_index(i).title(), NAV + i);
    }
    (*p).search = create_control(p, "Edit", "", WS_TABSTOP | ES_AUTOHSCROLL as u32, SEARCH);
    (*p).pause = button(p, tr("일시정지", "Pause"), PAUSE);
    (*p).rate = controls::select(p, tr("새로 고침 간격", "Refresh interval"), RATE);
    for label in rate_labels() {
        SendMessageW((*p).rate, CB_ADDSTRING, 0, wide(label).as_ptr() as isize);
    }
    SendMessageW((*p).rate, CB_SETCURSEL, 3, 0);
    (*p).top = button(p, tr("항상 위에 표시", "Always on top"), TOP);
    (*p).settings = button(p, tr("설정", "Settings"), SETTINGS);
    (*p).primary = button(p, tr("작업 끝내기", "End task"), PRIMARY);
    (*p).secondary = button(p, tr("파일 위치 열기", "Open file location"), SECONDARY);
    (*p).refresh = button(p, tr("새로고침", "Refresh"), REFRESH);
    (*p).view_mode = controls::select(p, tr("프로세스 표시 방식", "Process view"), VIEW_MODE);
    (*p).end_tree = button(p, tr("트리 전체 종료", "End process tree"), END_TREE);
    populate_view_modes(p);
    (*p).run_task = button(p, tr("새 작업 실행", "Run new task"), RUN_TASK);
    (*p).extra = button(p, tr("더 보기", "More actions"), EXTRA);
    (*p).copy = button(p, tr("정보 복사", "Copy info"), COPY);
    (*p).cores = button(p, tr("코어별 보기", "Logical CPUs"), CORES);
    (*p).resource_monitor = button(p, tr("리소스 모니터", "Resource Monitor"), RESOURCE_MONITOR);
    (*p).expand_all = button(p, tr("모두 펼치기", "Expand all"), EXPAND_ALL);
    // Painted as the 32 × 32 "⋯" icon button; the text names it for
    // accessibility tools.
    (*p).more = button(p, tr("추가 작업", "More actions"), MORE);
    // The same name in both languages (the user's choice); the panel's
    // subtitle says what it does.
    (*p).nuclear = button(p, "Nuclear Zombie", NUCLEAR);
    (*p).filter_control = controls::select(p, tr("상태 필터", "Status filter"), FILTER);
    // Custom virtual list / table controls (table.rs): device cards and the
    // processes / startup / services table, with overlay scrollbars.
    (*p).perf_list = table::create_devices(
        p,
        PERF_COMPONENT,
        tr("하드웨어 구성 요소", "Hardware components"),
    );
    perf_order::install((*p).perf_list, p);
    (*p).list = table::create_table(p, 200, tr("프로세스 목록", "Process list"));
    SetWindowSubclass(
        (*p).search,
        Some(interactions::search_subclass),
        1,
        p as usize,
    );
    interactions::create_settings_controls(p);
    create_fonts(p);
    setup_columns(p);
    update_buttons(p);
    layout(p);
    controls::init_focus_cues(p);
}
/// The refresh-rate choices (status bar and Settings), localized.
fn rate_labels() -> [&'static str; 6] {
    [
        tr("일시정지", "Paused"),
        tr("0.25초", "0.25 s"),
        tr("0.5초", "0.5 s"),
        tr("1초", "1 s"),
        tr("2초", "2 s"),
        tr("5초", "5 s"),
    ]
}
unsafe fn create_fonts(p: *mut App) {
    shell::destroy_palette(p);
    let fonts = fonts::Fonts::new((*p).dpi, language());
    // Body text (14 px) for the table, the search input and the nav rail;
    // selects use the 12 px role; every other control the 13 px UI role.
    let (body, ui, small) = (fonts.body, fonts.ui, fonts.small);
    let selects = [(*p).rate, (*p).view_mode, (*p).filter_control];
    for h in [(*p).list, (*p).search].into_iter().chain((*p).nav) {
        SendMessageW(h, WM_SETFONT, body as usize, 1);
    }
    // The search text starts exactly at the search box's 34 px text inset
    // (where the placeholder is painted); WM_SETFONT resets the margins.
    SendMessageW(
        (*p).search,
        EM_SETMARGINS,
        (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
        0,
    );
    for h in [
        (*p).pause,
        (*p).top,
        (*p).settings,
        (*p).primary,
        (*p).secondary,
        (*p).refresh,
        (*p).end_tree,
        (*p).run_task,
        (*p).extra,
        (*p).copy,
        (*p).cores,
        (*p).resource_monitor,
        (*p).expand_all,
        (*p).more,
        (*p).nuclear,
        (*p).perf_list,
    ]
    .into_iter()
    .chain(selects)
    .chain((*p).preference_controls.iter().copied())
    {
        let select = selects.contains(&h)
            || matches!(
                GetDlgCtrlID(h) as usize,
                PREF_LANGUAGE | PREF_RATE | PREF_START
            );
        SendMessageW(h, WM_SETFONT, if select { small } else { ui } as usize, 1);
    }
    // Controls now reference the new handles; the old set is deleted here.
    drop(std::mem::replace(&mut (*p).fonts, fonts));
    frame::refresh_search_font(p);
}
/// Table columns per page: (label, CSS px width — 0 = flexible, numeric).
/// Widths follow the reference's `<colgroup>`s: Processes PID 80 (where the
/// reference has a Status column) / 92 / 112 / 100 / 120 / 80 — the metric
/// columns sit exactly where the reference puts them — Startup 200 / 120 /
/// 120, Services 180 / 80 / flexible description / 110 / 170.
fn columns(page: Page) -> Vec<(&'static str, i32, bool)> {
    match page {
        Page::Processes => vec![
            (tr("이름", "Name"), 0, false),
            ("PID", 80, true),
            ("CPU", 92, true),
            (tr("메모리", "Memory"), 112, true),
            (tr("전체 I/O", "All I/O"), 100, true),
            (tr("네트워크", "Network"), 120, true),
            ("GPU", 80, true),
        ],
        Page::Startup => vec![
            (tr("이름", "Name"), 0, false),
            (tr("게시자¹", "Publisher¹"), 200, false),
            (tr("시작 영향", "Startup impact"), 120, false),
            (tr("사용", "Enabled"), 120, false),
        ],
        Page::Services => vec![
            (tr("이름", "Name"), 180, false),
            ("PID", 80, true),
            (tr("표시 이름", "Display name"), 0, false),
            (tr("상태", "Status"), 110, false),
            (tr("시작 유형", "Startup type"), 170, false),
        ],
        Page::Performance | Page::Settings => vec![],
    }
}
/// The columns the table shows now: the page's, minus Services' Startup type
/// while the details panel is open (the panel repeats it, and the Display
/// name column gets the room — the reference drops `.col-group` the same way
/// when space runs out).
unsafe fn shown_columns(p: *mut App) -> Vec<table::Column> {
    let page = (*p).page;
    if page == Page::Processes {
        return (*p).process_columns.columns();
    }
    let mut all = columns(page);
    if page == Page::Services && view_split(p, &current_layout(p)).2.is_some() {
        all.truncate(4);
    }
    all.iter()
        .enumerate()
        .map(|(i, &(name, width, numeric))| table::Column {
            label: name.into(),
            width: width as f32,
            flex: width == 0,
            // Numbers and the startup switch (`th.r`) are right-aligned.
            right: numeric || (page == Page::Startup && i == 3),
        })
        .collect()
}
/// Re-apply [`shown_columns`] when they changed (the details panel opened
/// or closed, or the window crossed its minimum width).
unsafe fn sync_columns(p: *mut App) {
    let columns = shown_columns(p);
    if table::column_count((*p).list) != columns.len() {
        table::set_columns((*p).list, columns);
    }
}
unsafe fn setup_columns(p: *mut App) {
    // Loaded layouts and page changes must never sort by a hidden column.
    if (*p).page == Page::Processes
        && !process_columns::ProcessColumn::from_id((*p).sort)
            .is_some_and(|column| (*p).process_columns.contains(column))
    {
        (*p).sort = process_columns::ProcessColumn::Name as usize;
        (*p).descending = false;
    }
    table::set_columns((*p).list, shown_columns(p));
    // The placeholder is painted by the search subclass in the design's
    // muted color, focused or not; the cue banner (never drawn: wParam 0
    // and the subclass paints over the empty input) names it for
    // assistive technology.
    let cue = paint::search_placeholder((*p).page);
    SendMessageW((*p).search, EM_SETCUEBANNER, 0, wide(cue).as_ptr() as isize);
    SetWindowTextW(
        (*p).list,
        wide(&tf!("{} 목록", "{} list", (*p).page.title())).as_ptr(),
    );
    interactions::populate_filters(p);
    update_sort_header(p);
}
/// The geometry of the current client area and page (see `layout.rs`).
unsafe fn current_layout(p: *mut App) -> layout::Layout {
    let mut r: RECT = zeroed();
    GetClientRect((*p).hwnd, &mut r);
    layout::Layout::new(r.right, r.bottom, (*p).dpi, (*p).page)
}
/// One entry of the page head's right-aligned action row (DESIGN_SPEC §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeadItem {
    /// A child control (button, select or the "⋯" more button).
    Control(HWND),
    /// Muted 12 px text painted by the page head (Startup's sign-in note).
    Note(&'static str),
}
fn startup_note() -> &'static str {
    tr(
        "다음 로그인부터 적용됩니다",
        "Changes apply next time you sign in",
    )
}
/// The page head's actions in visual order. Everything else that used to sit
/// in the rail is reachable through ⋯ menus, the palette, shortcuts, the
/// status bar or Settings.
unsafe fn head_items(p: *mut App) -> Vec<HeadItem> {
    let c = HeadItem::Control;
    match (*p).page {
        Page::Processes => {
            let mut items = vec![c((*p).view_mode)];
            if (*p).tree_mode {
                items.push(c((*p).expand_all));
            }
            items.extend([c((*p).nuclear), c((*p).primary), c((*p).more)]);
            items
        }
        Page::Performance => vec![c((*p).cores), c((*p).copy), c((*p).resource_monitor)],
        Page::Startup => vec![
            HeadItem::Note(startup_note()),
            c((*p).filter_control),
            c((*p).more),
        ],
        Page::Services => vec![
            c((*p).filter_control),
            c((*p).primary),
            c((*p).secondary),
            c((*p).extra),
            c((*p).more),
        ],
        Page::Settings => Vec::new(),
    }
}
unsafe fn text_width(dc: HDC, font: HFONT, text: &str) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let old = SelectObject(dc, font);
    // Hangul is measured in its fallback face, as `fonts::draw_text` draws it.
    let size = fonts::str_extent(dc, text);
    SelectObject(dc, old);
    size.cx
}
/// The font's line (cell) height in device px.
unsafe fn line_height(dc: HDC, font: HFONT) -> i32 {
    let old = SelectObject(dc, font);
    let mut metrics: TEXTMETRICW = zeroed();
    GetTextMetricsW(dc, &mut metrics);
    SelectObject(dc, old);
    metrics.tmHeight
}
/// Owner-draw selects (combo boxes painted as `.select`).
unsafe fn is_select(p: *mut App, h: HWND) -> bool {
    h == (*p).rate
        || h == (*p).view_mode
        || h == (*p).filter_control
        || matches!(
            GetDlgCtrlID(h) as usize,
            PREF_LANGUAGE | PREF_RATE | PREF_START
        )
}
/// A `<select>`'s intrinsic width: its widest option plus padding and arrow.
unsafe fn select_width(p: *mut App, dc: HDC, h: HWND, font: HFONT) -> i32 {
    let count = SendMessageW(h, CB_GETCOUNT, 0, 0).clamp(0, 64);
    let widest = (0..count)
        .map(|i| text_width(dc, font, &paint::combo_text(h, i)))
        .max()
        .unwrap_or(0);
    widest + gfx::pxi((*p).dpi, layout::SELECT_EXTRA)
}
/// A `.btn`'s intrinsic width: label + 14 px padding each side + borders.
unsafe fn button_width(p: *mut App, dc: HDC, h: HWND, font: HFONT) -> i32 {
    let hair = gfx::hairline((*p).dpi) as i32;
    text_width(dc, font, &paint::window_text(h))
        + 2 * gfx::pxi((*p).dpi, layout::BUTTON_PAD_X)
        + 2 * hair
}
unsafe fn head_item_size(p: *mut App, dc: HDC, item: HeadItem) -> (i32, i32) {
    let f = &(*p).fonts;
    let d = |v: f32| gfx::pxi((*p).dpi, v);
    match item {
        HeadItem::Note(text) => (text_width(dc, f.small, text), line_height(dc, f.small)),
        // The icon button is as tall as the head's controls (28 × 28 next
        // to Startup's filter, 32 × 32 elsewhere).
        HeadItem::Control(h) if h == (*p).more => {
            let size = d(layout::head_content((*p).page).min(layout::BUTTON_HEIGHT));
            (size, size)
        }
        HeadItem::Control(h) if is_select(p, h) => {
            (select_width(p, dc, h, f.small), d(layout::SELECT_HEIGHT))
        }
        // Label plus the 14 px trefoil and its 6 px gap.
        HeadItem::Control(h) if h == (*p).nuclear => (
            button_width(p, dc, h, f.ui) + d(paint::NUCLEAR_ICON + paint::NUCLEAR_GAP),
            d(layout::BUTTON_HEIGHT),
        ),
        HeadItem::Control(h) => {
            let font = if paint::button_style(p, GetDlgCtrlID(h) as usize)
                == widgets::ButtonStyle::Primary
            {
                f.ui_strong
            } else {
                f.ui
            };
            (button_width(p, dc, h, font), d(layout::BUTTON_HEIGHT))
        }
    }
}
/// The page head's actions with their rectangles (client px), measured with
/// the window's fonts on `dc`. Painting and control placement share this.
unsafe fn head_layout(p: *mut App, l: &layout::Layout, dc: HDC) -> Vec<(HeadItem, RECT)> {
    let items = head_items(p);
    let mut sizes: Vec<_> = items
        .iter()
        .map(|&item| head_item_size(p, dc, item))
        .collect();
    // The Nuclear Zombie trefoil is decoration: where the head has no room
    // for the page's (short) honesty note, the button drops it and keeps
    // its label (the 980 px minimum window).
    let nuclear = HeadItem::Control((*p).nuclear);
    if let Some(i) = items.iter().position(|&item| item == nuclear) {
        let first = l
            .head_actions(&sizes)
            .first()
            .map_or(l.head_inner.right, |r| r.left);
        if l.head_inner.left + paint::head_text_width(p, dc) > first - l.px(layout::HEAD_GAP) {
            sizes[i].0 -= l.px(paint::NUCLEAR_ICON + paint::NUCLEAR_GAP);
        }
    }
    items.into_iter().zip(l.head_actions(&sizes)).collect()
}
/// Move `h` to `r` (client px) only when it is elsewhere, so frequent
/// relayouts (label changes on every sample) cost nothing when stable.
unsafe fn place(p: *mut App, h: HWND, r: RECT) {
    let (w, height) = (r.right - r.left, r.bottom - r.top);
    let mut current: RECT = zeroed();
    GetWindowRect(h, &mut current);
    MapWindowPoints(
        null_mut(),
        (*p).hwnd,
        (&mut current as *mut RECT).cast::<POINT>(),
        2,
    );
    let same = current.left == r.left
        && current.top == r.top
        && current.right - current.left == w
        && current.bottom - current.top == height;
    if !same {
        MoveWindow(h, r.left, r.top, w.max(1), height.max(1), 1);
    }
}
/// Position the page head's action controls (labels change with the
/// selection, e.g. "Efficiency mode" ↔ "Exit efficiency mode").
unsafe fn place_head(p: *mut App, l: &layout::Layout) {
    let dc = GetDC((*p).hwnd);
    if dc.is_null() {
        return;
    }
    let items = head_layout(p, l, dc);
    ReleaseDC((*p).hwnd, dc);
    for (item, r) in items {
        if let HeadItem::Control(h) = item {
            place(p, h, r);
        }
    }
}
/// Controls shown on each page. The rail holds only the four pages and
/// Settings; Refresh, Pause, Run new task, Always on top and the old
/// "More actions" button stay as (hidden) command targets reachable from
/// F5 / Space / the status-bar rate select / ⋯ menus / the palette / Settings.
unsafe fn visible_controls(p: *mut App) -> Vec<(HWND, bool)> {
    let page = (*p).page;
    let list_page = matches!(page, Page::Processes | Page::Startup | Page::Services);
    let drawer = page == Page::Processes && (*p).show_telemetry;
    vec![
        ((*p).view_mode, page == Page::Processes),
        ((*p).expand_all, page == Page::Processes && (*p).tree_mode),
        ((*p).end_tree, drawer),
        (
            (*p).filter_control,
            matches!(page, Page::Startup | Page::Services),
        ),
        ((*p).list, list_page),
        (
            (*p).primary,
            matches!(page, Page::Processes | Page::Services),
        ),
        ((*p).secondary, page == Page::Services),
        // Processes: Efficiency mode lives in the ⋯ / context menus (its
        // button is only the Services page's Restart).
        ((*p).extra, page == Page::Services),
        ((*p).nuclear, page == Page::Processes),
        ((*p).more, list_page),
        ((*p).perf_list, page == Page::Performance),
        ((*p).copy, page == Page::Performance),
        ((*p).cores, page == Page::Performance),
        ((*p).resource_monitor, page == Page::Performance),
        ((*p).top, false),
        ((*p).run_task, false),
        ((*p).refresh, false),
        ((*p).pause, false),
    ]
}
unsafe fn layout(p: *mut App) {
    SendMessageW(
        (*p).view_mode,
        CB_SETCURSEL,
        if (*p).group_mode {
            2
        } else {
            (*p).tree_mode as usize
        },
        0,
    );
    let l = current_layout(p);
    for i in 0..4 {
        place(p, (*p).nav[i], l.nav[i]);
    }
    place(p, (*p).settings, l.nav_settings);
    place_nav_indicator(p, &l, false);
    // The search input sits in the title strip's search box text area.
    let dc = GetDC((*p).hwnd);
    if !dc.is_null() {
        let edit = l.search_edit(line_height(dc, frame::search_font(p)));
        let rate = l.rate(select_width(p, dc, (*p).rate, (*p).fonts.mono_small));
        ReleaseDC((*p).hwnd, dc);
        place(p, (*p).search, edit);
        place(p, (*p).rate, rate);
    }
    place_head(p, &l);
    place(p, (*p).perf_list, l.perf_device_list());
    // The table fills the content rect exactly; the optional telemetry drawer
    // and service details panel take its bottom 180 / right 290 px. Painting
    // uses the same rectangles (`view_split`).
    let (table, drawer, _) = view_split(p, &l);
    place(p, (*p).list, table);
    sync_columns(p);
    // "End process tree" sits in the telemetry drawer's title row, clear of
    // its three charts.
    let drawer = drawer.unwrap_or(RECT {
        top: l.content.bottom,
        ..l.content
    });
    place(
        p,
        (*p).end_tree,
        RECT {
            left: drawer.right - l.px(180.0),
            top: drawer.top + l.px(8.0),
            right: drawer.right - l.px(24.0),
            bottom: drawer.top + l.px(40.0),
        },
    );
    for (h, show) in visible_controls(p) {
        let visible = GetWindowLongW(h, GWL_STYLE) as u32 & WS_VISIBLE != 0;
        if visible != show {
            ShowWindow(h, if show { SW_SHOW } else { SW_HIDE });
        }
    }
    EnableWindow(
        (*p).search,
        (!matches!((*p).page, Page::Performance | Page::Settings)) as i32,
    );
    interactions::layout_settings(p, &l);
    order_tabs(p);
    redraw(p);
}
/// The nav indicator's edges for `page` (client px): the item rect inset
/// 12 px top and bottom, i.e. the reference's 3 × 16 bar.
fn nav_indicator_target(l: &layout::Layout, page: Page) -> (f32, f32) {
    let item = if page == Page::Settings {
        l.nav_settings
    } else {
        l.nav[(page as usize).min(3)]
    };
    (
        (item.top + l.px(12.0)) as f32,
        (item.bottom - l.px(12.0)) as f32,
    )
}
/// Move the nav indicator to the current page. `slide`: it slides and
/// stretches (motion::NAV, the leading edge eases out and the trailing edge
/// follows); otherwise it jumps (first layout, resize, DPI) unless a slide
/// is running, which then continues toward the (re-laid-out) target. Every
/// frame repaints the rail and the nav buttons under it.
unsafe fn place_nav_indicator(p: *mut App, l: &layout::Layout, slide: bool) {
    place_nav_indicator_at(p, l, slide, Instant::now());
}
/// [`place_nav_indicator`] with an explicit clock (deterministic tests).
unsafe fn place_nav_indicator_at(p: *mut App, l: &layout::Layout, slide: bool, now: Instant) {
    let top_key = (anim::NAV_ID, anim::part::NAV_TOP);
    let bottom_key = (anim::NAV_ID, anim::part::NAV_BOTTOM);
    let (top, bottom) = nav_indicator_target(l, (*p).page);
    let host = &mut (*p).anim;
    // Each frame paints the rail and the nav buttons in one synchronous
    // pass, so the bar never tears across them.
    for key in [top_key, bottom_key] {
        host.register_children_sync(key, (*p).hwnd, Some(l.rail));
    }
    let known = host.anim.target(top_key).is_some();
    let moving = host.anim.is_key_animating(top_key) || host.anim.is_key_animating(bottom_key);
    // Each item's background cross-fades with the slide. Targets are set
    // here, with the page change, so paint code only reads them.
    let items = [
        (NAV, (*p).nav[0]),
        (NAV + 1, (*p).nav[1]),
        (NAV + 2, (*p).nav[2]),
        (NAV + 3, (*p).nav[3]),
        (SETTINGS, (*p).settings),
    ];
    for (id, hwnd) in items {
        let key = (id, anim::part::SELECTED);
        let current = if id == SETTINGS {
            (*p).page == Page::Settings
        } else {
            id - NAV == (*p).page as usize
        };
        let target = current as u8 as f32;
        host.register(key, hwnd, None);
        if slide && known {
            host.set_target_at(key, target, anim::motion::NAV, anim::Easing::EaseOut, now);
        } else if host.anim.target(key) != Some(target) {
            host.set(key, target);
        }
    }
    if !known || !(slide || moving) {
        host.set(top_key, top);
        host.set(bottom_key, bottom);
        return;
    }
    let down = top > host.value(top_key);
    let lead = anim::Easing::EaseOut;
    let trail = anim::Easing::Bezier(0.6, 0.0, 0.2, 1.0);
    let (top_easing, bottom_easing) = if down { (trail, lead) } else { (lead, trail) };
    host.set_target_at(top_key, top, anim::motion::NAV, top_easing, now);
    host.set_target_at(bottom_key, bottom, anim::motion::NAV, bottom_easing, now);
}
/// The page's table rect and its optional companions: the processes
/// telemetry drawer (bottom) and the services details panel (right), from
/// `layout::Layout::{drawer, details}`. Control placement and painting share it.
unsafe fn view_split(p: *mut App, l: &layout::Layout) -> (RECT, Option<RECT>, Option<RECT>) {
    match (*p).page {
        Page::Processes => {
            let (table, drawer) = l.drawer((*p).show_telemetry);
            (table, drawer, None)
        }
        Page::Services => {
            let (table, details) = l.details((*p).show_details);
            (table, None, details)
        }
        _ => (l.content, None, None),
    }
}
/// Keyboard (Tab) order follows the visual order: the rail, the search box,
/// the page head's actions left to right, the page's view, the settings
/// controls, then the status bar's rate select. IsDialogMessage walks the
/// children in z-order, so restack them (no visual effect: none overlap).
unsafe fn tab_order(p: *mut App) -> Vec<HWND> {
    let mut order: Vec<HWND> = (*p).nav.to_vec();
    order.extend([(*p).settings, (*p).search]);
    order.extend(head_items(p).into_iter().filter_map(|item| match item {
        HeadItem::Control(h) => Some(h),
        HeadItem::Note(_) => None,
    }));
    order.extend([(*p).list, (*p).perf_list, (*p).end_tree]);
    for id in [
        THEME_LIGHT,
        THEME_DARK,
        THEME_SYSTEM,
        PREF_LANGUAGE,
        PREF_RATE,
        PREF_START,
        PREF_TOP,
        PREF_TRAY,
        PREF_REPLACE,
        PREF_ADMIN,
    ] {
        order.push(GetDlgItem((*p).hwnd, id as i32));
    }
    order.push((*p).rate);
    order.retain(|h| !h.is_null());
    order
}
unsafe fn order_tabs(p: *mut App) {
    let mut previous = HWND_TOP;
    for h in tab_order(p) {
        SetWindowPos(
            h,
            previous,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOREDRAW | SWP_NOOWNERZORDER,
        );
        previous = h;
    }
}
unsafe fn configure(p: *mut App) {
    use process_columns::ProcessColumn;
    let mut needs =
        if (*p).page == Page::Processes && !((*p).paused || (*p).minimized || (*p).modal) {
            crate::process_metadata::Needs {
                user: (*p).process_columns.contains(ProcessColumn::User),
                command_line: (*p).process_columns.contains(ProcessColumn::CommandLine),
                status: (*p).process_columns.contains(ProcessColumn::Status),
            }
        } else {
            Default::default()
        };
    needs.status |= !(*p).modal && resource_monitor::needs_status((*p).resource_window);
    if needs != (*p).metadata_needs {
        (*p).metadata_needs = needs;
        let _ = (*p).tx.send(Command::Metadata(needs));
    }
    let main_paused = (*p).paused || (*p).minimized || (*p).modal;
    let resource_shown = resource_monitor::interval((*p).resource_window).is_some();
    let resource_active = !(*p).modal && resource_shown;
    let request = if resource_active {
        let (files, endpoints) = resource_monitor::tracing((*p).resource_window);
        (resource_monitor::request(p), files, endpoints)
    } else if resource_shown {
        // A menu or dialog pauses sampling but keeps the request: stopping
        // the file and endpoint traces for it would restart (and re-prime)
        // their ETW sessions after every menu.
        (*p).resource_request
    } else {
        Default::default()
    };
    if request != (*p).resource_request {
        (*p).resource_request = request;
        let _ = (*p)
            .tx
            .send(Command::Resource(request.0, request.1, request.2));
    }
    if main_paused {
        (*p).perf_history.gap(Instant::now());
    }
    let resource_interval = resource_active
        .then(|| resource_monitor::interval((*p).resource_window))
        .flatten();
    let interval = resource_interval.map_or((*p).interval, |rate| {
        if main_paused {
            rate
        } else {
            rate.min((*p).interval)
        }
    });
    // Not the page: history keeps recording while another page is open.
    let _ = (*p).tx.send(Command::Configure {
        interval,
        paused: main_paused && !resource_active,
    });
}
unsafe fn request_list(p: *mut App, page: Page) {
    if page == Page::Services {
        // The main window asked: it gets the next list, also one that is
        // already in flight for the Resource Monitor.
        (*p).services_for_monitor = false;
    }
    let job = match page {
        Page::Startup if !(*p).startup_loading => {
            (*p).startup_loading = true;
            Some(Job::Startup)
        }
        Page::Services if !(*p).services_loading => {
            (*p).services_loading = true;
            (*p).last_services = Instant::now();
            Some(Job::Services)
        }
        _ => None,
    };
    if let Some(job) = job {
        if (*p).jobs.send(job).is_err() {
            (*p).startup_loading = false;
            (*p).services_loading = false;
            (*p).set_error(
                ErrorSource::General,
                tr(
                    "백그라운드 작업을 실행할 수 없습니다.",
                    "Unable to run the background job.",
                )
                .into(),
            );
        }
        update_buttons(p);
        InvalidateRect((*p).list, null(), 0);
        redraw(p);
    }
}
unsafe fn switch_page(p: *mut App, page: Page) {
    if (*p).page == page || (*p).modal {
        return;
    }
    // Atomic: the head, its relabelled buttons, the search placeholder and
    // the table appear together (`present_all`).
    let suspended = suspend_painting(p);
    (*p).updating = true;
    SendMessageW((*p).list, LVM_SETITEMCOUNT, 0, 0);
    (*p).rows.clear();
    (*p).page = page;
    place_nav_indicator(p, &current_layout(p), true);
    if page == Page::Settings {
        (*p).replacement_active = matches!(
            crate::replacement::status(),
            Ok(crate::replacement::Status::Active)
        );
        interactions::sync_settings(p);
    }
    (*p).filter.clear();
    (*p).clear_error();
    (*p).notice.clear();
    (*p).category = 0;
    (*p).sort = if page == Page::Processes { 3 } else { 0 };
    (*p).descending = page == Page::Processes;
    (*p).sort_chosen = false;
    SetWindowTextW((*p).search, wide("").as_ptr());
    setup_columns(p);
    rebuild(p, None);
    layout(p);
    for h in (*p).nav {
        InvalidateRect(h, null(), 0);
    }
    match page {
        Page::Startup if !(*p).startup_loaded => request_list(p, page),
        Page::Services => request_list(p, page),
        // The performance sample is current on every page (the monitor
        // samples it throughout), so Performance shows it at once.
        _ => {}
    }
    configure(p);
    present_all(p, suspended);
    // The nav slide starts with the first frame of the new page, not before
    // the work above (which would eat most of its 180 ms).
    let now = Instant::now();
    for key in [
        (anim::NAV_ID, anim::part::NAV_TOP),
        (anim::NAV_ID, anim::part::NAV_BOTTOM),
        (NAV, anim::part::SELECTED),
        (NAV + 1, anim::part::SELECTED),
        (NAV + 2, anim::part::SELECTED),
        (NAV + 3, anim::part::SELECTED),
        (SETTINGS, anim::part::SELECTED),
    ] {
        (*p).anim.rebase(key, now);
    }
}
unsafe fn command(p: *mut App, id: usize, notification: u32) {
    if (NAV..NAV + 5).contains(&id) {
        switch_page(p, Page::from_index(id - NAV));
        return;
    }
    if interactions::command(p, id, notification) {
        return;
    }
    match id {
        SEARCH if notification == EN_CHANGE && !(*p).updating => {
            let identity = selected_identity(p);
            let n = GetWindowTextLengthW((*p).search).max(0) as usize;
            let mut s = vec![0u16; n + 1];
            GetWindowTextW((*p).search, s.as_mut_ptr(), s.len() as i32);
            (*p).filter = String::from_utf16_lossy(&s[..n]).trim().to_lowercase();
            rebuild(p, identity);
            redraw(p);
        }
        RATE if notification == CBN_SELCHANGE => {
            interactions::rate_changed(p, (*p).rate);
        }
        VIEW_MODE if notification == CBN_SELCHANGE => {
            let mode = SendMessageW((*p).view_mode, CB_GETCURSEL, 0, 0);
            (*p).group_mode = mode == 2;
            set_tree_mode(p, mode == 1);
        }
        PAUSE => {
            (*p).paused = !(*p).paused;
            interactions::sync_settings(p);
            SetWindowTextW(
                (*p).pause,
                wide(if (*p).paused {
                    tr("계속", "Resume")
                } else {
                    tr("일시정지", "Pause")
                })
                .as_ptr(),
            );
            configure(p);
            redraw(p);
        }
        TOP => {
            (*p).topmost = !(*p).topmost;
            SetWindowPos(
                (*p).hwnd,
                if (*p).topmost {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                },
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            InvalidateRect((*p).top, null(), 0);
        }
        REFRESH if !(*p).modal => {
            (*p).clear_error();
            (*p).notice.clear();
            (*p).process_detail_identity = None;
            (*p).service_detail_name = None;
            interactions::selection_changed(p);
            let _ = (*p).tx.send(Command::Refresh);
            request_list(p, (*p).page);
            redraw(p);
        }
        PRIMARY if IsWindowEnabled((*p).primary) != 0 && !(*p).modal => primary_action(p),
        END_TREE if IsWindowEnabled((*p).end_tree) != 0 && !(*p).modal => end_tree_action(p),
        SECONDARY if IsWindowEnabled((*p).secondary) != 0 && !(*p).modal => secondary_action(p),
        SETTINGS if !(*p).modal => switch_page(p, Page::Settings),
        NUCLEAR if !(*p).busy && !(*p).modal => nuclear::open(p),
        _ => {}
    }
}
unsafe fn create_settings_menu(status: &Result<crate::replacement::Status, String>) -> HMENU {
    use crate::replacement::Status;
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return menu;
    }
    let (label, can_replace, can_restore) = match status {
        Ok(Status::Inactive) => (
            tr(
                "작업 관리자 · Windows 기본값",
                "Task Manager · Windows default",
            ),
            true,
            false,
        ),
        Ok(Status::Active) => (
            tr(
                "작업 관리자 · Feather 사용 중",
                "Task Manager · Feather active",
            ),
            false,
            true,
        ),
        Ok(Status::Other(_)) => (
            tr(
                "작업 관리자 · 다른 설정 사용 중",
                "Task Manager · Another app active",
            ),
            false,
            false,
        ),
        Err(_) => (
            tr(
                "작업 관리자 · 상태 확인 실패",
                "Task Manager · Status unavailable",
            ),
            false,
            false,
        ),
    };
    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(label).as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    for (id, text, enabled) in [
        (
            REPLACE_TASK_MANAGER,
            tr(
                "Feather를 작업 관리자로 설정…",
                "Use Feather as Task Manager…",
            ),
            can_replace,
        ),
        (
            RESTORE_TASK_MANAGER,
            tr(
                "Windows 기본 작업 관리자로 복원…",
                "Restore Windows Task Manager…",
            ),
            can_restore,
        ),
    ] {
        AppendMenuW(
            menu,
            MF_STRING | if enabled { MF_ENABLED } else { MF_GRAYED },
            id,
            wide(text).as_ptr(),
        );
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(
        menu,
        MF_STRING,
        ELEVATE,
        wide(tr("관리자로 실행", "Run as administrator")).as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    for (id, name, selected) in [
        (LANGUAGE_KOREAN, "한국어", language() == Language::Korean),
        (LANGUAGE_ENGLISH, "English", language() == Language::English),
    ] {
        AppendMenuW(
            menu,
            MF_STRING | if selected { MF_CHECKED } else { MF_UNCHECKED },
            id,
            wide(name).as_ptr(),
        );
    }
    menu
}
unsafe fn settings_menu(p: *mut App) {
    let status = crate::replacement::status();
    match &status {
        Err(error) => (*p).set_error(ErrorSource::Action, error.clone()),
        Ok(crate::replacement::Status::Other(value)) => {
            (*p).set_error(
                ErrorSource::Action,
                tf!(
                    "기존 작업 관리자 연결을 먼저 해제하세요. 현재 설정: {value}",
                    "Remove the existing Task Manager replacement first. Current setting: {value}"
                ),
            );
        }
        _ => {}
    }
    let menu = create_settings_menu(&status);
    if menu.is_null() {
        (*p).set_error(
            ErrorSource::Action,
            tr(
                "설정 메뉴를 열지 못했습니다.",
                "Unable to open the settings menu.",
            )
            .into(),
        );
        redraw(p);
        return;
    }
    (*p).modal = true;
    update_buttons(p);
    configure(p);
    let mut bounds: RECT = zeroed();
    GetWindowRect((*p).settings, &mut bounds);
    // The layered menu above the Settings item (below it without room).
    let command = popup::track_menu(p, menu, popup::Anchor::Above { r: bounds });
    DestroyMenu(menu);
    (*p).modal = false;
    configure(p);
    PostMessageW((*p).hwnd, SNAPSHOT_READY, 0, 0);
    PostMessageW((*p).hwnd, JOB_READY, 0, 0);
    update_buttons(p);
    redraw(p);
    match command {
        LANGUAGE_KOREAN | LANGUAGE_ENGLISH => {
            let chosen = if command == LANGUAGE_KOREAN {
                Language::Korean
            } else {
                Language::English
            };
            match set_language(chosen) {
                Ok(()) => refresh_language(p),
                Err(error) => (*p).set_error(ErrorSource::Action, error),
            }
            redraw(p);
        }
        ELEVATE => begin_action(p, Action::Elevate),
        REPLACE_TASK_MANAGER | RESTORE_TASK_MANAGER => {
            let enable = command == REPLACE_TASK_MANAGER;
            let (title, prompt) = if enable {
                (tr("Windows 작업 관리자 대체", "Replace Windows Task Manager"), tr(concat!(
                    "Feather를 Windows 작업 관리자로 설정할까요?\n\n",
                    "설치 프로그램으로 먼저 설치한 Feather를 이 PC의 모든 사용자에게 연결합니다. ",
                    "Ctrl+Alt+Delete → 작업 관리자와 Ctrl+Shift+Esc로 Feather가 열립니다.\n\n",
                    "관리자 권한이 필요합니다. 설치된 파일을 삭제하기 전에 설정에서 Windows 기본 작업 관리자로 복원하세요."
                ), "Use Feather as the Windows Task Manager?\n\nInstall Feather with Setup first. This setting connects the installed app for all users on this PC. Ctrl+Alt+Delete → Task Manager and Ctrl+Shift+Esc will open Feather.\n\nAdministrator permission is required. Restore the default Windows Task Manager in Settings before deleting the installed files."))
            } else {
                (tr("Windows 작업 관리자 복원", "Restore Windows Task Manager"), tr(concat!(
                    "Windows 기본 작업 관리자로 되돌릴까요?\n\n",
                    "관리자 권한이 필요하며 이 PC의 모든 사용자에게 적용합니다. Feather 앱은 설치된 폴더에 그대로 남습니다."
                ), "Restore the default Windows Task Manager?\n\nAdministrator permission is required and this applies to all users on this PC. Feather will remain in its installation folder."))
            };
            if confirm_with(p, title, prompt, None, false) {
                begin_action(p, Action::ReplaceTaskManager(enable, (*p).page));
            }
        }
        _ => {}
    }
}
unsafe fn populate_view_modes(p: *mut App) {
    SendMessageW((*p).view_mode, CB_RESETCONTENT, 0, 0);
    for value in [
        tr("목록 보기", "List view"),
        tr("프로세스 트리", "Process tree"),
        tr("앱별 그룹", "App groups"),
    ] {
        SendMessageW(
            (*p).view_mode,
            CB_ADDSTRING,
            0,
            wide(value).as_ptr() as isize,
        );
    }
    SendMessageW(
        (*p).view_mode,
        CB_SETCURSEL,
        if (*p).group_mode {
            2
        } else {
            (*p).tree_mode as usize
        },
        0,
    );
}
unsafe fn refresh_language(p: *mut App) {
    let identity = selected_identity(p);
    // Startup rows contain localized status/location strings. An already
    // queued result can contain the previous language, or a mixture if the
    // language changed during collection. Discard it and collect once more.
    (*p).startup_loaded = false;
    (*p).startup_refresh_pending = (*p).startup_loading;
    (*p).updating = true;
    for (i, handle) in (*p).nav.iter().enumerate() {
        SetWindowTextW(*handle, wide(Page::from_index(i).title()).as_ptr());
    }
    for (handle, value) in [
        (
            (*p).pause,
            if (*p).paused {
                tr("계속", "Resume")
            } else {
                tr("일시정지", "Pause")
            },
        ),
        ((*p).top, tr("항상 위에 표시", "Always on top")),
        ((*p).settings, tr("설정", "Settings")),
        ((*p).refresh, tr("새로고침", "Refresh")),
        ((*p).end_tree, tr("트리 전체 종료", "End process tree")),
        ((*p).view_mode, tr("프로세스 표시 방식", "Process view")),
    ] {
        SetWindowTextW(handle, wide(value).as_ptr());
    }
    interactions::populate_settings(p);
    populate_view_modes(p);
    (*p).notice.clear();
    create_fonts(p);
    setup_columns(p);
    rebuild(p, identity);
    layout(p);
    update_buttons(p);
    if (*p).page == Page::Startup {
        request_list(p, Page::Startup);
    }
    if (*p).page == Page::Services {
        request_list(p, Page::Services);
    }
    InvalidateRect((*p).hwnd, null(), 1);
    resource_monitor::settings_changed(p);
}
unsafe fn set_tree_mode(p: *mut App, enabled: bool) {
    let identity = selected_identity(p);
    (*p).tree_mode = enabled;
    if enabled {
        (*p).group_mode = false;
    }
    SendMessageW(
        (*p).view_mode,
        CB_SETCURSEL,
        if (*p).group_mode { 2 } else { enabled as usize },
        0,
    );
    rebuild(p, identity);
    // The page head changes with the mode (Expand all shows only in tree
    // mode): re-run the layout so visibility, placement and tab order agree.
    layout(p);
}
/// The grouped view ("App groups") is showing (it has app rows).
unsafe fn grouped_view(p: *mut App) -> bool {
    (*p).page == Page::Processes && (*p).group_mode && !(*p).tree_mode
}
/// Expand / collapse (None: toggle) the selected tree parent or app row.
unsafe fn toggle_selected_branch(p: *mut App, expand: Option<bool>) {
    let grouped = grouped_view(p);
    if (*p).page != Page::Processes || !((*p).tree_mode || grouped) || !(&(*p).filter).is_empty() {
        return;
    }
    let selected = SendMessageW(
        (*p).list,
        LVM_GETNEXTITEM,
        usize::MAX,
        LVNI_SELECTED as isize,
    );
    if selected < 0 {
        return;
    }
    let Some(row) = (&(*p).tree_rows).get(selected as usize).copied() else {
        return;
    };
    // Arrow keys follow the native tree convention: move to a parent or first
    // child when the requested branch is already collapsed or expanded.
    if let Some(expand) = expand {
        let destination = if expand && row.expanded {
            (&(*p).tree_rows)
                .get(selected as usize + 1)
                .filter(|child| child.depth > row.depth)
                .map(|_| selected as usize + 1)
        } else if !expand && (!row.has_children || !row.expanded) {
            (0..selected as usize)
                .rev()
                .find(|&index| (&(*p).tree_rows)[index].depth < row.depth)
        } else {
            None
        };
        if let Some(destination) = destination {
            let clear = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                ..zeroed()
            };
            SendMessageW(
                (*p).list,
                LVM_SETITEMSTATE,
                usize::MAX,
                &clear as *const _ as isize,
            );
            let selected = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: LVIS_SELECTED | LVIS_FOCUSED,
                ..zeroed()
            };
            SendMessageW(
                (*p).list,
                LVM_SETITEMSTATE,
                destination,
                &selected as *const _ as isize,
            );
            SendMessageW((*p).list, LVM_ENSUREVISIBLE, destination, 0);
            return;
        }
    }
    if !row.has_children {
        return;
    }
    let Some(process) = (*p)
        .snapshot
        .as_ref()
        .and_then(|s| s.processes.get(row.index))
    else {
        return;
    };
    let id = ProcessIdentity::from(process);
    let expand = expand.unwrap_or(!row.expanded);
    // Tree rows start expanded (the collapsed set), app groups collapsed.
    match (grouped, expand) {
        (true, true) => {
            (*p).expanded_groups.insert(id);
        }
        (true, false) => {
            (*p).expanded_groups.remove(&id);
        }
        (false, true) => {
            (*p).collapsed.remove(&id);
        }
        (false, false) => {
            (*p).collapsed.insert(id);
        }
    }
    rebuild(p, Some(Identity::Process(id.pid, id.created)));
    redraw(p);
}
unsafe fn captured_tree_plan(p: *mut App) -> Result<TerminationPlan, String> {
    let index = selected_row(p)
        .ok_or_else(|| tr("프로세스를 선택하세요.", "Select a process.").to_owned())?;
    let snapshot = (*p).snapshot.as_ref().ok_or_else(|| {
        tr("프로세스 정보가 없습니다.", "Process data is unavailable.").to_owned()
    })?;
    Tree::new(&snapshot.processes).plan(index)
}
unsafe fn end_tree_action(p: *mut App) {
    let plan = match captured_tree_plan(p) {
        Ok(plan) => plan,
        Err(error) => {
            (*p).set_error(ErrorSource::Action, error);
            redraw(p);
            return;
        }
    };
    let prompt = tf!(
        "{} (PID {}) 및 하위 프로세스, 총 {}개를 종료할까요?\n\n현재 목록에서 확인된 트리 전체가 대상이며 숨겨진 하위 항목도 포함됩니다. 이 확인창을 연 뒤 새로 생성된 프로세스는 포함하지 않습니다.\n\n저장하지 않은 작업이 사라질 수 있습니다.",
        "End {} (PID {}) and its descendants, {} processes in total?\n\nThis includes the entire captured tree, including hidden children. Processes created after this confirmation opened are not included.\n\nUnsaved work may be lost.",
        plan.root_name, plan.root.pid, plan.len());
    let warn = selected_process(p).and_then(|s| controls::windows_process_warning(&s));
    if confirm_with(
        p,
        tr("트리 전체 종료", "End process tree"),
        &prompt,
        warn,
        true,
    ) {
        begin_action(p, Action::EndTree(plan));
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Identity {
    Process(u32, u64),
    Startup(String),
    Service(String),
}
unsafe fn selected_row(p: *mut App) -> Option<usize> {
    let i = SendMessageW(
        (*p).list,
        LVM_GETNEXTITEM,
        usize::MAX,
        LVNI_SELECTED as isize,
    );
    if i < 0 {
        None
    } else {
        (&(*p).rows).get(i as usize).copied()
    }
}
unsafe fn identity_at(p: *mut App, row: usize) -> Option<Identity> {
    match (*p).page {
        Page::Processes => (*p)
            .snapshot
            .as_ref()?
            .processes
            .get(row)
            .map(|s| Identity::Process(s.pid, s.created)),
        Page::Startup => (&(*p).startup)
            .get(row)
            .map(|s| Identity::Startup(s.id.clone())),
        Page::Services => (&(*p).services)
            .get(row)
            .map(|s| Identity::Service(s.name.clone())),
        Page::Performance | Page::Settings => None,
    }
}
unsafe fn selected_identity(p: *mut App) -> Option<Identity> {
    identity_at(p, selected_row(p)?)
}
unsafe fn selected_process(p: *mut App) -> Option<Process> {
    if (*p).page != Page::Processes {
        return None;
    }
    (*p).snapshot
        .as_ref()?
        .processes
        .get(selected_row(p)?)
        .cloned()
}
unsafe fn total_rows(p: *mut App) -> usize {
    match (*p).page {
        Page::Processes => (*p).snapshot.as_ref().map_or(0, |s| s.processes.len()),
        Page::Startup => (*p).startup.len(),
        Page::Services => (*p).services.len(),
        Page::Performance | Page::Settings => 0,
    }
}
#[allow(dead_code)]
unsafe fn selected_detail(p: *mut App) -> String {
    if (*p).busy {
        return tr("요청을 처리하는 중…", "Processing your request…").into();
    }
    if let Some(e) = &(*p).error {
        return e.clone();
    }
    if !(&(*p).notice).is_empty() {
        return (*p).notice.clone();
    }
    let Some(row) = selected_row(p) else {
        return match (*p).page {
            Page::Startup => tr(
                "로그인 시작 앱 · 일부 앱은 Windows 설정에서 관리합니다.",
                "Startup apps · Some apps are managed in Windows Settings.",
            )
            .into(),
            Page::Services => tr(
                "서비스를 선택하면 시작 또는 중지할 수 있습니다.",
                "Select a service to start or stop it.",
            )
            .into(),
            _ => tr(
                "작업을 선택하면 파일 위치를 열거나 종료할 수 있습니다.",
                "Select a process to end it or open its file location.",
            )
            .into(),
        };
    };
    match (*p).page {
        Page::Processes => selected_process(p)
            .map(|s| {
                tf!(
                    "{}  ·  PID {}  ·  부모 PID {}",
                    "{}  ·  PID {}  ·  Parent PID {}",
                    s.name,
                    s.pid,
                    s.parent_pid
                )
            })
            .unwrap_or_default(),
        Page::Startup => (&(*p).startup)
            .get(row)
            .map(|s| format!("{}  ·  {}", s.name, s.status))
            .unwrap_or_default(),
        Page::Services => (&(*p).services)
            .get(row)
            .map(|s| {
                format!(
                    "{}  ·  {}",
                    s.display_name,
                    crate::services::state_label(s.state)
                )
            })
            .unwrap_or_default(),
        Page::Performance | Page::Settings => String::new(),
    }
}
unsafe fn update_buttons(p: *mut App) {
    let focus = GetFocus();
    update_button_states(p);
    // Disabling the focused button (a row deselected, an action running)
    // leaves the thread without keyboard focus; hand it on. During a modal
    // popup the popup owns the keyboard and the caller restores focus.
    if !(*p).modal && !focus.is_null() {
        restore_focus(p, focus);
    }
}
/// Give keyboard focus back after a modal popup or a disabled control:
/// `previous` when it can still take focus, else the page's table, else the
/// main window, so Tab and the arrow keys keep working. The layered popups
/// never deactivate the window (unlike MessageBox), so nothing else would
/// restore it. Does nothing while a control has focus (the main window
/// itself only holds it for a modal popup, see [`park_focus`]) or when the
/// main window is not the thread's active window (the user switched away).
unsafe fn restore_focus(p: *mut App, previous: HWND) {
    let hwnd = (*p).hwnd;
    let focus = GetFocus();
    if hwnd.is_null()
        || GetActiveWindow() != hwnd
        || !(focus.is_null() || focus == hwnd && previous != hwnd)
    {
        return;
    }
    let usable = |h: HWND| {
        !h.is_null()
            && IsWindow(h) != 0
            && (h == hwnd || IsChild(hwnd, h) != 0)
            && (h == hwnd || GetWindowLongW(h, GWL_STYLE) as u32 & WS_VISIBLE != 0)
            && IsWindowEnabled(h) != 0
    };
    let target = if usable(previous) {
        previous
    } else if usable((*p).list) {
        (*p).list
    } else {
        hwnd
    };
    if target != focus {
        SetFocus(target);
    }
}
/// While a layered modal popup runs, keep the keyboard focus on the main
/// window when disabling its opener dropped it: keys then arrive as plain
/// WM_KEYDOWN (with no focus at all every key is a WM_SYSKEYDOWN, so F4
/// alone would act like Alt+F4). [`restore_focus`] hands it back after.
unsafe fn park_focus(p: *mut App) {
    let hwnd = (*p).hwnd;
    if !hwnd.is_null() && GetFocus().is_null() && GetActiveWindow() == hwnd {
        SetFocus(hwnd);
    }
}
unsafe fn update_button_states(p: *mut App) {
    // Actions are unavailable while one runs (busy) or a modal popup is
    // open, but only `busy` shows it: under the dialog's scrim or a menu the
    // page keeps its look, like the reference's page, and never restyles
    // on the way in or out. The modal loops block every input, and the
    // commands check `modal` as well.
    let ready = !(*p).busy;
    let mut primary = false;
    let mut secondary = false;
    let mut label = tr("작업 끝내기", "End task");
    let mut second = tr("파일 위치 열기", "Open file location");
    match (*p).page {
        Page::Processes => {
            if let Some(s) = selected_process(p) {
                primary = s.pid > 4 && s.pid != std::process::id() && s.created != 0;
                secondary = s.pid > 4 && s.created != 0;
            }
        }
        Page::Startup => {
            label = tr("사용 안 함", "Disable");
            if let Some(s) = selected_row(p).and_then(|r| (&(*p).startup).get(r)) {
                primary = s.manageable && !(*p).startup_loading;
                if !s.enabled {
                    label = tr("사용", "Enable");
                }
            }
        }
        Page::Services => {
            label = tr("시작", "Start");
            second = tr("중지", "Stop");
            if let Some(s) = selected_row(p).and_then(|r| (&(*p).services).get(r)) {
                primary =
                    s.state == SERVICE_STOPPED && s.start_type != Some(4) && !(*p).services_loading;
                secondary = s.state == SERVICE_RUNNING && !(*p).services_loading;
            }
        }
        Page::Performance | Page::Settings => {}
    }
    SetWindowTextW((*p).primary, wide(label).as_ptr());
    SetWindowTextW((*p).secondary, wide(second).as_ptr());
    EnableWindow((*p).primary, (ready && primary) as i32);
    let tree_allowed = (*p).page == Page::Processes && primary && captured_tree_plan(p).is_ok();
    EnableWindow((*p).end_tree, (ready && tree_allowed) as i32);
    EnableWindow((*p).view_mode, 1);
    EnableWindow((*p).secondary, (ready && secondary) as i32);
    // Settings is a nav item like the others (never dimmed; the command
    // waits for a modal popup to close).
    EnableWindow((*p).settings, 1);
    let extra_ready = if (*p).page == Page::Processes {
        primary
            && (*p)
                .process_settings
                .as_ref()
                .and_then(|s| s.efficiency)
                .is_some()
    } else {
        secondary
    };
    EnableWindow((*p).extra, (ready && extra_ready) as i32);
    SetWindowTextW(
        (*p).extra,
        wide(if (*p).page == Page::Services {
            tr("다시 시작", "Restart")
        } else if (*p)
            .process_settings
            .as_ref()
            .is_some_and(|s| s.efficiency == Some(true))
        {
            tr("효율 모드 해제", "Exit efficiency mode")
        } else {
            tr("효율 모드", "Efficiency mode")
        })
        .as_ptr(),
    );
    EnableWindow((*p).run_task, ready as i32);
    EnableWindow((*p).nuclear, ready as i32);
    let loading = ((*p).page == Page::Startup && (*p).startup_loading)
        || ((*p).page == Page::Services && (*p).services_loading);
    EnableWindow((*p).refresh, (!loading) as i32);
    // The ⋯ menu waits for a running action (its command checks), without
    // dimming the button for the moment a toggle or a request takes.
    EnableWindow((*p).more, 1);
    // Labels above may have changed width; the head row re-flows like CSS.
    if !(*p).hwnd.is_null() {
        place_head(p, &current_layout(p));
    }
}
/// Ask before a destructive action (the layered confirm dialog; `title`
/// names the danger button, the prompt's first question is the heading).
unsafe fn confirm(p: *mut App, title: &str, prompt: &str) -> bool {
    confirm_with(p, title, prompt, None, true)
}
/// [`confirm`] with an optional warn line and the action's style (danger
/// for destructive actions, primary otherwise). Modal like MessageBox:
/// sampling pauses and queued notifications are re-posted afterwards.
unsafe fn confirm_with(
    p: *mut App,
    title: &str,
    prompt: &str,
    warn: Option<&str>,
    danger: bool,
) -> bool {
    // update_buttons disables the button that opened the dialog (End task,
    // Stop, ⋯, Settings), which drops the keyboard focus; restore it after.
    let focus = GetFocus();
    (*p).modal = true;
    update_buttons(p);
    park_focus(p);
    configure(p);
    let answer = controls::confirm(p, title, prompt, warn, danger);
    (*p).modal = false;
    configure(p);
    PostMessageW((*p).hwnd, SNAPSHOT_READY, 0, 0);
    PostMessageW((*p).hwnd, JOB_READY, 0, 0);
    update_buttons(p);
    restore_focus(p, focus);
    answer
}
/// Confirm ending one process identity, then run the termination job: the
/// selected row's End task, and the Nuclear Zombie panel's per-holder End
/// task (`detail` = an extra paragraph, e.g. how many exited processes it
/// holds open). The warn line appears for Windows processes.
unsafe fn end_task_flow(p: *mut App, name: &str, pid: u32, created: u64, detail: Option<&str>) {
    let warn = controls::windows_image_warning(pid, created);
    let mut prompt = tf!(
        "{} (PID {}) 프로세스를 종료할까요?\n\n저장하지 않은 작업은 사라질 수 있습니다.",
        "End {} (PID {})?\n\nUnsaved work may be lost.",
        name,
        pid
    );
    if let Some(detail) = detail.filter(|d| !d.trim().is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(detail);
    }
    if confirm_with(p, tr("작업 끝내기", "End task"), &prompt, warn, true) {
        begin_action(p, Action::End(pid, created));
    }
}
unsafe fn primary_action(p: *mut App) {
    match (*p).page {
        Page::Processes => {
            if let Some(s) = selected_process(p) {
                end_task_flow(p, &s.name, s.pid, s.created, None);
            }
        }
        Page::Startup => {
            if let Some(s) = selected_row(p)
                .and_then(|r| (&(*p).startup).get(r))
                .cloned()
            {
                let enabled = !s.enabled;
                toggle_startup(p, s, enabled);
            }
        }
        Page::Services => {
            if let Some(s) = selected_row(p)
                .and_then(|r| (&(*p).services).get(r))
                .cloned()
            {
                begin_action(p, Action::Start(s.name, s.display_name));
            }
        }
        Page::Performance | Page::Settings => {}
    }
}
/// Switch a startup entry on or off: the switch slides at once (optimistic,
/// [`App::startup_pending`]) while the registry change runs.
unsafe fn toggle_startup(p: *mut App, entry: StartupEntry, enabled: bool) {
    let id = entry.id.clone();
    begin_action(p, Action::Toggle(Box::new(entry), enabled));
    if (*p).busy {
        (*p).startup_pending = Some((id, enabled));
        InvalidateRect((*p).list, null(), 0);
    }
}
unsafe fn secondary_action(p: *mut App) {
    match (*p).page {
        Page::Processes => {
            if let Some(s) = selected_process(p) {
                begin_action(p, Action::Reveal(s.pid, s.created));
            }
        }
        Page::Services => {
            if let Some(s) = selected_row(p)
                .and_then(|r| (&(*p).services).get(r))
                .cloned()
            {
                if confirm(p,tr("서비스 중지", "Stop service"),&tf!("{} 서비스를 중지할까요?\n\n이 서비스를 사용하는 Windows 기능이나 앱에 영향을 줄 수 있습니다.\n서비스 이름: {}", "Stop {}?\n\nThis may affect Windows features or apps using this service.\nService name: {}",s.display_name,s.name)){begin_action(p,Action::Stop(s.name,s.display_name));}
            }
        }
        _ => {}
    }
}
unsafe fn begin_action(p: *mut App, action: Action) {
    if (*p).busy || (*p).modal {
        return;
    }
    (*p).busy = true;
    (*p).clear_error();
    (*p).notice.clear();
    if (*p).jobs.send(Job::Action(action)).is_err() {
        (*p).busy = false;
        (*p).set_error(
            ErrorSource::Action,
            tr(
                "관리 작업을 실행할 수 없습니다.",
                "Unable to run the management action.",
            )
            .into(),
        );
    }
    update_buttons(p);
    redraw(p);
}
/// Copy the monitor's per-`(pid, created)` GPU and network values onto the
/// snapshot's processes; a process without a measured value keeps `None`.
fn join_process_samples(
    snapshot: &mut Snapshot,
    gpu: &std::collections::HashMap<(u32, u64), ProcessGpu>,
    network: &ProcessNetworkSample,
    performance: Option<&PerfSnapshot>,
) {
    for process in &mut snapshot.processes {
        let id = (process.pid, process.created);
        process.gpu_percent = gpu.get(&id).and_then(|g| g.percent);
        process.gpu_dedicated_bytes = gpu.get(&id).and_then(|g| g.dedicated_bytes);
        process.gpu_shared_bytes = gpu.get(&id).and_then(|g| g.shared_bytes);
        process.gpu_engine = gpu.get(&id).and_then(|g| g.engine.as_ref()).map(|engine| {
            let index = performance.and_then(|s| {
                s.gpus
                    .iter()
                    .filter(|g| listed_gpu(g))
                    .position(|g| g.id == engine.adapter)
            });
            index.map_or_else(
                || engine.engine_type.clone(),
                |i| format!("GPU {i} - {}", engine.engine_type),
            )
        });
        process.network_bytes_per_sec = network
            .measured
            .then(|| network.by_id.get(&id).map(|n| n.total_bytes_per_sec))
            .flatten();
        process.network_send_bytes_per_sec = network
            .measured
            .then(|| network.by_id.get(&id).map(|n| n.send_bytes_per_sec))
            .flatten();
        process.network_recv_bytes_per_sec = network
            .measured
            .then(|| network.by_id.get(&id).map(|n| n.recv_bytes_per_sec))
            .flatten();
    }
}
unsafe fn drain_snapshot(p: *mut App) {
    if (*p).modal {
        return;
    }
    let Ok(sample) = (*p).rx.try_recv() else {
        return;
    };
    let identity = selected_identity(p);
    let fresh_performance = matches!(&sample.performance, Some(Ok(_)));
    let resource_active = resource_monitor::active((*p).resource_window);
    // The shared worker can run faster for Resource Monitor. Keep the main
    // window's independent pause/rate; tolerate normal collection-time jitter.
    let due = (*p).last_sample.is_none_or(|at| {
        sample.at.saturating_duration_since(at)
            >= Duration::from_millis((*p).interval.saturating_sub(100))
    });
    let main_updates =
        sample.manual_refresh || (!(*p).paused && !(*p).minimized && (!resource_active || due));
    if let Some(performance) = sample.performance {
        match performance {
            Ok(s) => {
                (*p).resource_performance = Some(Arc::new(s));
                if main_updates {
                    (*p).performance = (*p).resource_performance.clone();
                    (*p).performance_error = None;
                }
            }
            Err(e) => {
                (*p).resource_performance = None;
                if main_updates {
                    (*p).performance = None;
                    (*p).performance_error = Some(e);
                }
            }
        }
    }
    match sample.snapshot {
        Ok(mut snapshot) => {
            (*p).recover_error(ErrorSource::Monitor);
            join_process_samples(
                &mut snapshot,
                &sample.process_gpu,
                &sample.process_network,
                (*p).resource_performance.as_deref(),
            );
            let snapshot = Arc::new(snapshot);
            (*p).resource_snapshot = Some(Arc::clone(&snapshot));
            if main_updates {
                (*p).network_state = Some(NetworkState {
                    measured: sample.process_network.measured,
                    reason: sample.process_network.reason.clone(),
                });
                let memory = if snapshot.memory_total == 0 {
                    f64::NAN
                } else {
                    snapshot.memory_used as f64 / snapshot.memory_total as f64 * 100.0
                };
                let (disk, network) = if fresh_performance {
                    (*p).performance
                        .as_ref()
                        .map(|s| {
                            (
                                if s.disk_rates_ready {
                                    s.disk_read_bytes_per_sec + s.disk_write_bytes_per_sec
                                } else {
                                    f64::NAN
                                },
                                if s.network_rates_ready {
                                    s.network_rx_bytes_per_sec + s.network_tx_bytes_per_sec
                                } else {
                                    f64::NAN
                                },
                            )
                        })
                        .unwrap_or((f64::NAN, f64::NAN))
                } else {
                    (f64::NAN, f64::NAN)
                };
                if (*p).last_sample.is_none_or(|at| sample.at > at) {
                    (*p).history.push_back(HistoryPoint {
                        at: sample.at,
                        cpu: snapshot.cpu_percent,
                        memory,
                        disk,
                        network,
                    });
                    while (*p).history.len() > 120
                        || (*p).history.front().is_some_and(|h| {
                            sample.at.saturating_duration_since(h.at) > Duration::from_secs(60)
                        })
                    {
                        (*p).history.pop_front();
                    }
                    (*p).last_sample = Some(sample.at);
                }
                (*p).telemetry.record(&snapshot, sample.at);
                if fresh_performance {
                    if let Some(perf) = &(*p).performance {
                        (*p).perf_history.record(perf, &snapshot, sample.at);
                        interactions::refresh_components(p);
                    }
                } else {
                    (*p).perf_history.gap(sample.at);
                }
                let count = snapshot.processes.len();
                let recount = (*p).snapshot.as_ref().map(|s| s.processes.len()) != Some(count);
                (*p).snapshot = Some(snapshot);
                if recount {
                    // The Processes nav item (an owner-draw child that `redraw`
                    // does not reach) shows the process count.
                    InvalidateRect((*p).nav[0], null(), 0);
                }
                if (*p).page == Page::Processes {
                    rebuild(p, identity);
                }
            }
        }
        Err(e) => {
            (*p).resource_snapshot = None;
            (*p).set_error(ErrorSource::Monitor, e);
        }
    }
    if !(*p).resource_window.is_null() {
        (*p).resource_sample_at = Some(sample.at);
        (*p).resource_data = sample.resource_data;
        (*p).resource_network = Some(Arc::new(sample.process_network));
        (*p).resource_files = Some(Arc::new(sample.resource_files));
        resource_monitor::refresh(p, sample.at);
    }
    refresh_services(p);
    if main_updates {
        redraw(p);
    }
}
/// Reload services every 5 s while a live Services page or the Resource
/// Monitor's Services panel shows them.
unsafe fn refresh_services(p: *mut App) {
    let main = (*p).page == Page::Services && !(*p).paused && !(*p).minimized;
    if !(main || resource_monitor::needs_services((*p).resource_window))
        || (*p).busy
        || (*p).last_services.elapsed() < Duration::from_secs(5)
    {
        return;
    }
    if main {
        request_list(p, Page::Services);
    } else if !(*p).services_loading {
        request_list(p, Page::Services);
        (*p).services_for_monitor = (*p).services_loading;
    }
}
unsafe fn drain_jobs(p: *mut App) {
    if (*p).modal {
        return;
    }
    while let Ok(result) = (*p).results.try_recv() {
        let identity = selected_identity(p);
        match result {
            JobResult::Navigate(request, result) => navigation::finish(p, request, result),
            JobResult::ProcessDetails(pid, created, result) => {
                if (*p).process_detail_identity == Some((pid, created)) {
                    (*p).process_settings = result.ok();
                }
            }
            JobResult::ServiceDetails(name, result) => {
                if (*p).service_detail_name.as_ref() == Some(&name) {
                    match result {
                        Ok(details) => {
                            (*p).service_details = Some(details);
                            (*p).service_detail_error = None;
                        }
                        Err(e) => (*p).service_detail_error = Some(e),
                    }
                }
            }
            JobResult::Publishers(values) => {
                (*p).startup_publishers = values;
            }
            JobResult::Startup(result) => {
                (*p).startup_loading = false;
                if (*p).startup_refresh_pending {
                    (*p).startup_refresh_pending = false;
                    if (*p).page == Page::Startup {
                        request_list(p, Page::Startup);
                    }
                    continue;
                }
                // The reloaded list is the truth now (the switch follows it).
                (*p).startup_pending = None;
                match result {
                    Ok(s) => {
                        (*p).startup = s;
                        (*p).startup_loaded = true;
                        (*p).recover_error(ErrorSource::Startup);
                    }
                    Err(e) => {
                        if (*p).page == Page::Startup {
                            (*p).set_error(ErrorSource::Startup, e);
                        }
                    }
                }
                if (*p).page == Page::Startup {
                    rebuild(p, identity);
                }
            }
            JobResult::Services(result)
                if std::mem::take(&mut (*p).services_for_monitor)
                    && (*p).page == Page::Services
                    && ((*p).paused || (*p).minimized) =>
            {
                // A paused Services page keeps its frame; only the Resource
                // Monitor takes this list.
                (*p).services_loading = false;
                if let Ok(services) = result {
                    (*p).resource_services = Some(services);
                }
            }
            JobResult::Services(result) => {
                (*p).services_loading = false;
                if result.is_ok() {
                    (*p).resource_services = None;
                }
                match result {
                    Ok(s) => {
                        if let Some(detail) = &mut (*p).service_details {
                            if let Some(service) = s.iter().find(|s| s.name == detail.name) {
                                detail.state = service.state;
                                detail.pid = service.pid;
                            }
                        }
                        (*p).services = s;
                        (*p).services_loaded = true;
                        (*p).recover_error(ErrorSource::Services);
                    }
                    Err(e) => {
                        if (*p).page == Page::Services {
                            (*p).set_error(ErrorSource::Services, e);
                        }
                    }
                }
                if (*p).page == Page::Services {
                    rebuild(p, identity);
                }
            }
            JobResult::Action {
                result,
                page,
                notice,
            } => {
                (*p).busy = false;
                match result {
                    Ok(()) => {
                        (*p).replacement_active = matches!(
                            crate::replacement::status(),
                            Ok(crate::replacement::Status::Active)
                        );
                        (*p).process_detail_identity = None;
                        (*p).service_detail_name = None;
                        interactions::selection_changed(p);
                        controls::notify_success(p, notice);
                        (*p).clear_error();
                        request_list(p, page);
                        let _ = (*p).tx.send(Command::Refresh);
                    }
                    Err(e) => {
                        // A switch flipped optimistically slides back.
                        (*p).startup_pending = None;
                        (*p).set_error(ErrorSource::Action, e)
                    }
                }
            }
        }
    }
    update_buttons(p);
    InvalidateRect((*p).list, null(), 0);
    redraw(p);
    resource_monitor::refresh(p, Instant::now());
}
fn option_cmp(a: Option<f64>, b: Option<f64>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.total_cmp(&b),
        (a, b) => a.is_some().cmp(&b.is_some()),
    }
}
fn compare(a: &Process, b: &Process, col: usize) -> Ordering {
    process_columns::ProcessColumn::from_id(col).map_or(Ordering::Equal, |column| {
        process_columns::compare(a, b, column)
    })
}
unsafe fn rebuild(p: *mut App, identity: Option<Identity>) {
    (*p).updating = true;
    (*p).group_headers.clear();
    let filter = &(*p).filter;
    (*p).rows = (0..total_rows(p))
        .filter(|&row| {
            if !interactions::category_matches(p, row) {
                return false;
            }
            // Tree and grouped views filter while building their rows
            // (a match keeps its ancestors / its app).
            if filter.is_empty()
                || ((*p).page == Page::Processes && ((*p).tree_mode || (*p).group_mode))
            {
                return true;
            }
            match (*p).page {
                Page::Processes => (*p)
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.processes.get(row))
                    .is_some_and(|s| navigation::matches_search(filter, s.pid, &[&s.name])),
                // Name, publisher (the visible column), command, location.
                Page::Startup => (&(*p).startup).get(row).is_some_and(|s| {
                    s.name.to_lowercase().contains(filter)
                        || (*p)
                            .startup_publishers
                            .get(&s.id)
                            .is_some_and(|v| v.to_lowercase().contains(filter))
                        || s.command.to_lowercase().contains(filter)
                        || s.location.to_lowercase().contains(filter)
                }),
                Page::Services => (&(*p).services).get(row).is_some_and(|s| {
                    navigation::matches_search(filter, s.pid, &[&s.name, &s.display_name])
                }),
                Page::Performance | Page::Settings => false,
            }
        })
        .collect();
    let col = (*p).sort;
    let desc = (*p).descending;
    (*p).rows.sort_unstable_by(|&a, &b| {
        let result = match (*p).page {
            Page::Processes => (*p)
                .snapshot
                .as_ref()
                .map(|s| compare(&s.processes[a], &s.processes[b], col))
                .unwrap_or(Ordering::Equal),
            Page::Startup => {
                let a = &(&(*p).startup)[a];
                let b = &(&(*p).startup)[b];
                match col {
                    // Case-insensitive; entries without a publisher ("—")
                    // stay last in both directions.
                    1 => {
                        let publisher = |e: &StartupEntry| {
                            (*p).startup_publishers.get(&e.id).map(|v| v.to_lowercase())
                        };
                        match (publisher(a), publisher(b)) {
                            (Some(x), Some(y)) => x.cmp(&y),
                            (Some(_), None) => return Ordering::Less,
                            (None, Some(_)) => return Ordering::Greater,
                            (None, None) => Ordering::Equal,
                        }
                    }
                    3 => a.enabled.cmp(&b.enabled),
                    _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                }
            }
            Page::Services => {
                let a = &(&(*p).services)[a];
                let b = &(&(*p).services)[b];
                match col {
                    1 => a.pid.cmp(&b.pid),
                    2 => a
                        .display_name
                        .to_lowercase()
                        .cmp(&b.display_name.to_lowercase()),
                    3 => a.state.cmp(&b.state),
                    4 => a.start_type.cmp(&b.start_type),
                    _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                }
            }
            Page::Performance | Page::Settings => Ordering::Equal,
        };
        (if desc { result.reverse() } else { result }).then(a.cmp(&b))
    });
    (*p).tree_rows.clear();
    (*p).group_totals.clear();
    let matches = (*p)
        .snapshot
        .as_ref()
        .filter(|_| !filter.is_empty() && (*p).page == Page::Processes)
        .map(|snapshot| {
            snapshot
                .processes
                .iter()
                .enumerate()
                .filter(|(_, process)| {
                    navigation::matches_search(filter, process.pid, &[&process.name])
                })
                .map(|(index, _)| index)
                .collect::<HashSet<_>>()
        });
    if (*p).page == Page::Processes && (*p).tree_mode {
        if let Some(snapshot) = (*p).snapshot.as_ref() {
            let identities = snapshot
                .processes
                .iter()
                .map(ProcessIdentity::from)
                .collect::<HashSet<_>>();
            (*p).collapsed
                .retain(|identity| identities.contains(identity));
            (*p).tree_rows =
                Tree::new(&snapshot.processes).rows(&(*p).rows, &(*p).collapsed, matches.as_ref());
            (*p).rows = (*p).tree_rows.iter().map(|row| row.index).collect();
        }
    } else if (*p).page == Page::Processes && (*p).group_mode {
        interactions::group_rows(p, matches.as_ref());
    }
    let clear = LVITEMW {
        stateMask: LVIS_SELECTED | LVIS_FOCUSED,
        ..zeroed()
    };
    SendMessageW(
        (*p).list,
        LVM_SETITEMSTATE,
        usize::MAX,
        &clear as *const _ as isize,
    );
    SendMessageW(
        (*p).list,
        LVM_SETITEMCOUNT,
        (*p).rows.len(),
        (LVSICF_NOINVALIDATEALL | LVSICF_NOSCROLL) as isize,
    );
    if let Some(identity) = identity {
        if let Some(index) = (*p)
            .rows
            .iter()
            .position(|&row| identity_at(p, row).as_ref() == Some(&identity))
        {
            let item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: LVIS_SELECTED | LVIS_FOCUSED,
                ..zeroed()
            };
            SendMessageW(
                (*p).list,
                LVM_SETITEMSTATE,
                index,
                &item as *const _ as isize,
            );
        }
    }
    (*p).updating = false;
    interactions::selection_changed(p);
    update_buttons(p);
    InvalidateRect((*p).list, null(), 0);
}
/// The table header paints the sort arrow from `(*p).sort / descending`.
unsafe fn update_sort_header(p: *mut App) {
    table::invalidate_header((*p).list);
}
unsafe fn empty_message(p: *mut App) -> String {
    if let Some(e) = &(*p).error {
        return tf!(
            "목록을 불러오지 못했습니다.\n{e}\n새로고침으로 다시 시도하세요.",
            "Unable to load the list.\n{e}\nSelect Refresh to try again."
        );
    }
    if ((*p).page == Page::Startup && (*p).startup_loading)
        || ((*p).page == Page::Services && (*p).services_loading)
        || ((*p).page == Page::Processes && (*p).snapshot.is_none())
    {
        return tr("목록을 불러오는 중…", "Loading the list…").into();
    }
    if !(&(*p).filter).is_empty() && (*p).page == Page::Startup {
        // Startup entries have no PID; the search matches these fields.
        tr(
            "검색 결과가 없습니다.\n다른 이름, 게시자 또는 명령으로 검색해 보세요.",
            "No matching results.\nTry a different name, publisher or command.",
        )
        .into()
    } else if !(&(*p).filter).is_empty() {
        tr(
            "검색 결과가 없습니다.\n다른 이름이나 PID로 검색해 보세요.",
            "No matching results.\nTry a different name or PID.",
        )
        .into()
    } else if (*p).page == Page::Startup {
        tr(
            "관리할 데스크톱 시작 앱이 없습니다.\n일부 앱은 Windows 설정에서 관리할 수 있습니다.",
            "No desktop startup apps to manage.\nSome apps are managed in Windows Settings.",
        )
        .into()
    } else {
        tr("표시할 항목이 없습니다.", "No items to display.").into()
    }
}
unsafe fn notify(p: *mut App, l: LPARAM) -> LRESULT {
    let hdr = &*(l as *const NMHDR);
    if hdr.hwndFrom != (*p).list {
        return 0;
    }
    match hdr.code {
        LVN_COLUMNCLICK => {
            let col = (*(l as *const NMLISTVIEW)).iSubItem;
            if col < 0 || col as usize >= shown_columns(p).len() {
                return 0;
            }
            // No startup-impact measurement exists: nothing to sort by.
            if (*p).page == Page::Startup && col == 2 {
                return 0;
            }
            let identity = selected_identity(p);
            let col = if (*p).page == Page::Processes {
                (*p).process_columns
                    .at(col as usize)
                    .unwrap_or(process_columns::ProcessColumn::Name) as usize
            } else {
                col as usize
            };
            (*p).sort_chosen = true;
            if col == (*p).sort {
                (*p).descending = !(*p).descending;
            } else {
                (*p).sort = col;
                (*p).descending = (*p).page == Page::Processes
                    && process_columns::ProcessColumn::from_id(col).is_some_and(|column| {
                        column.numeric() && column != process_columns::ProcessColumn::Pid
                    });
            }
            rebuild(p, identity);
            update_sort_header(p);
            redraw(p);
            0
        }
        LVN_ITEMCHANGED => {
            if !(*p).updating {
                (*p).notice.clear();
                interactions::selection_changed(p);
                update_buttons(p);
                redraw(p);
            }
            0
        }
        // Double click and Enter run the row's default action.
        NM_DBLCLK | NM_RETURN => {
            if (*p).page == Page::Processes {
                // An app row of the grouped view opens and closes like a
                // tree parent; other rows open the file location.
                let parent = selected_row(p).is_some()
                    && SendMessageW(
                        (*p).list,
                        LVM_GETNEXTITEM,
                        usize::MAX,
                        LVNI_SELECTED as isize,
                    )
                    .try_into()
                    .ok()
                    .and_then(|row: usize| (&(*p).tree_rows).get(row).copied())
                    .is_some_and(|row| row.has_children);
                if (*p).tree_mode || (grouped_view(p) && parent) {
                    // A first click on the chevron already toggled the branch.
                    if hdr.code == NM_RETURN || !branch_glyph_hit(p, &*(l as *const NMITEMACTIVATE))
                    {
                        toggle_selected_branch(p, None);
                    }
                } else {
                    PostMessageW((*p).hwnd, WM_COMMAND, SECONDARY, 0);
                }
            }
            0
        }
        NM_CLICK if (*p).page == Page::Startup => {
            interactions::startup_click(p, &*(l as *const NMITEMACTIVATE));
            0
        }
        NM_CLICK if (*p).page == Page::Processes && ((*p).tree_mode || grouped_view(p)) => {
            let click = &*(l as *const NMITEMACTIVATE);
            if branch_glyph_hit(p, click) {
                toggle_selected_branch(p, None);
            }
            0
        }
        _ => 0,
    }
}
/// The click landed on the row's tree chevron (the table's own geometry).
unsafe fn branch_glyph_hit(p: *mut App, click: &NMITEMACTIVATE) -> bool {
    click.iItem >= 0
        && table::part_at((*p).list, click.ptAction)
            == Some((click.iItem as usize, table::Part::Chevron))
}
/// The process at snapshot index `index` as the table shows it: a grouped
/// app row carries its app's summed CPU, memory and I/O
/// (`(*p).group_totals`), every other row the process's own values.
unsafe fn shown_process(p: *mut App, index: usize) -> Option<Process> {
    let mut process = (*p).snapshot.as_ref()?.processes.get(index)?.clone();
    if let Some(total) = (*p).group_totals.get(&index) {
        total.apply(&mut process);
    }
    Some(process)
}
unsafe fn cell_at(p: *mut App, row: usize, col: i32) -> String {
    match (*p).page {
        Page::Processes => shown_process(p, row)
            .and_then(|s| {
                (*p).process_columns
                    .at(col as usize)
                    .map(|column| process_columns::text(&s, column))
            })
            .unwrap_or_default(),
        Page::Startup => (&(*p).startup)
            .get(row)
            .map(|s| match col {
                0 => s.name.clone(),
                1 => (*p)
                    .startup_publishers
                    .get(&s.id)
                    .cloned()
                    .unwrap_or_else(|| "—".into()),
                2 => tr("측정 안 됨", "Not measured").into(),
                3 => s.status.clone(),
                _ => String::new(),
            })
            .unwrap_or_default(),
        Page::Services => (&(*p).services)
            .get(row)
            .map(|s| match col {
                0 => s.name.clone(),
                1 => {
                    if s.pid == 0 {
                        "—".into()
                    } else {
                        s.pid.to_string()
                    }
                }
                2 => s.display_name.clone(),
                3 => crate::services::state_label(s.state).into(),
                4 => crate::services::start_type_label(s.start_type).into(),
                _ => String::new(),
            })
            .unwrap_or_default(),
        Page::Performance | Page::Settings => String::new(),
    }
}
fn cell(p: &Process, col: i32) -> String {
    process_columns::ProcessColumn::from_id(col as usize)
        .map_or_else(String::new, |column| process_columns::text(p, column))
}
/// One decimal with thousands separators, like the reference's
/// `toLocaleString('en-US', { minimumFractionDigits: 1 })` ("1,410.4").
fn thousands(value: f64) -> String {
    let text = format!("{value:.1}");
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, "0"));
    let (sign, digits) = match whole.strip_prefix('-') {
        Some(digits) => ("-", digits),
        None => ("", whole),
    };
    format!("{sign}{}.{fraction}", group_digits(digits))
}
/// A count with thousands separators, like the reference's
/// `toLocaleString('en-US')` ("13,837").
fn grouped(value: impl std::fmt::Display) -> String {
    group_digits(&value.to_string())
}
fn group_digits(digits: &str) -> String {
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}
/// A GPU the Performance page lists: not a software renderer (Microsoft
/// Basic Render Driver) or an indirect (virtual) display adapter, which
/// Windows Task Manager does not list either.
fn listed_gpu(gpu: &crate::performance::GpuStats) -> bool {
    !gpu.is_software()
        && !gpu
            .adapter
            .as_ref()
            .is_some_and(|adapter| adapter.is_indirect_display())
}
/// A network adapter the Performance page lists: present and connected
/// (Task Manager hides the others).
fn listed_network(nic: &crate::performance::NetworkStats) -> bool {
    nic.present && nic.connected
}
/// System GPU utilization: the busiest listed adapter.
fn system_gpu_percent(perf: &PerfSnapshot) -> Option<f64> {
    perf.gpus
        .iter()
        .filter(|gpu| listed_gpu(gpu))
        .filter_map(|gpu| gpu.percent)
        .fold(None, |max: Option<f64>, v| {
            Some(max.map_or(v, |m| m.max(v)))
        })
}
/// Bytes per second as megabits per second, one decimal ("6.8 Mbps").
fn mbps(bytes_per_sec: f64) -> String {
    if bytes_per_sec.is_finite() {
        format!("{:.1} Mbps", bytes_per_sec * 8.0 / 1e6)
    } else {
        "—".into()
    }
}
fn rate(bytes: f64) -> String {
    if !bytes.is_finite() {
        "—".into()
    } else if bytes >= 1048576.0 {
        format!("{:.1} MB", bytes / 1048576.0)
    } else if bytes >= 1024.0 {
        format!("{:.1} KB", bytes / 1024.0)
    } else {
        format!("{bytes:.0} B")
    }
}

/// Resize `hwnd` so its client area is exactly `width` × `height` device px,
/// whatever its frame (native caption now, custom frame later): the frame's
/// actual size is measured instead of assumed.
unsafe fn fit_client(hwnd: HWND, width: i32, height: i32) {
    for _ in 0..3 {
        let mut client: RECT = zeroed();
        GetClientRect(hwnd, &mut client);
        if client.right == width && client.bottom == height {
            return;
        }
        let mut window: RECT = zeroed();
        GetWindowRect(hwnd, &mut window);
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            window.right - window.left + width - client.right,
            window.bottom - window.top + height - client.bottom,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Preview captures of layout states that need interaction: a selected
/// process (enabled head actions) with the ⋯ button hovered, the process
/// telemetry drawer and the service details panel inside the content rect.
unsafe fn save_layout_states(p: *mut App, dir: &std::path::Path) -> Result<(), String> {
    let select_first = |p: *mut App| {
        if let Some(row) = (0..(*p).rows.len()).find(|i| !(*p).group_headers.contains_key(i)) {
            let item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: LVIS_SELECTED | LVIS_FOCUSED,
                ..zeroed()
            };
            SendMessageW((*p).list, LVM_SETITEMSTATE, row, &item as *const _ as isize);
        }
        update_buttons(p);
    };
    // The nav indicator a third of the way from Processes to Services
    // (motion::NAV): stretched across the rail gaps and the nav buttons,
    // the old item's background fading out and the new one's in.
    switch_page(p, Page::Processes);
    switch_page(p, Page::Services);
    (*p).services_loading = false;
    rebuild(p, None);
    {
        let l = current_layout(p);
        let from = nav_indicator_target(&l, Page::Processes);
        let to = nav_indicator_target(&l, Page::Services);
        let lead = anim::Easing::EaseOut.apply(0.33);
        let trail = anim::Easing::Bezier(0.6, 0.0, 0.2, 1.0).apply(0.33);
        (*p).anim.set(
            (anim::NAV_ID, anim::part::NAV_TOP),
            from.0 + (to.0 - from.0) * trail,
        );
        (*p).anim.set(
            (anim::NAV_ID, anim::part::NAV_BOTTOM),
            from.1 + (to.1 - from.1) * lead,
        );
        (*p).anim.set((NAV, anim::part::SELECTED), 1.0 - lead);
        (*p).anim.set((NAV + 3, anim::part::SELECTED), lead);
        capture::save_client(p, &dir.join("nav-slide.bmp"))?;
    }
    switch_page(p, Page::Processes);
    rebuild(p, None);
    select_first(p);
    (*p).anim.set((MORE, anim::part::HOVER), 1.0);
    layout(p);
    capture::save_client(p, &dir.join("processes-selected.bmp"))?;
    (*p).anim.set((MORE, anim::part::HOVER), 0.0);
    (*p).show_telemetry = true;
    layout(p);
    capture::save_client(p, &dir.join("process-telemetry.bmp"))?;
    (*p).show_telemetry = false;
    layout(p);
    // Typed search text inside the title-strip search box.
    SetWindowTextW((*p).search, wide("svc").as_ptr());
    capture::save_client(p, &dir.join("processes-search.bmp"))?;
    SetWindowTextW((*p).search, wide("").as_ptr());
    switch_page(p, Page::Services);
    (*p).services_loading = false;
    rebuild(p, None);
    select_first(p);
    (*p).show_details = true;
    layout(p);
    // What the list already knows while the details query runs ...
    capture::save_client(p, &dir.join("service-details.bmp"))?;
    // ... and the answer: the first running service's real details.
    let running = (*p).rows.iter().position(|&i| {
        (&(*p).services)
            .get(i)
            .is_some_and(|s| s.state == SERVICE_RUNNING)
    });
    if let Some(row) = running {
        let item = LVITEMW {
            stateMask: LVIS_SELECTED | LVIS_FOCUSED,
            state: LVIS_SELECTED | LVIS_FOCUSED,
            ..zeroed()
        };
        SendMessageW((*p).list, LVM_SETITEMSTATE, row, &item as *const _ as isize);
        update_buttons(p);
        if let Some(name) = (*p).service_detail_name.clone() {
            (*p).service_details = crate::services::details(&name).ok();
        }
        for (theme, name) in [(1, "light"), (2, "dark")] {
            (*p).prefs.theme = theme;
            interactions::apply_theme(p);
            capture::save_client(p, &dir.join(format!("service-details-loaded-{name}.bmp")))?;
        }
    }
    (*p).show_details = false;
    layout(p);
    // The Logical CPUs view (captions above the small multiples).
    let performance = (*p).performance.clone();
    switch_page(p, Page::Performance);
    (*p).performance = performance;
    for (theme, name) in [(1, "light"), (2, "dark")] {
        (*p).prefs.theme = theme;
        interactions::apply_theme(p);
        (*p).core_graphs = true;
        redraw(p);
        capture::save_client(p, &dir.join(format!("cores-{name}.bmp")))?;
        (*p).core_graphs = false;
    }
    (*p).prefs.theme = 1;
    interactions::apply_theme(p);
    // One capture per device kind (its first listed device): the model
    // line, card subs and hardware specs (`perf-<kind>.bmp`).
    interactions::refresh_components(p);
    let first = |kind: fn(&PerfTarget) -> bool| (&(*p).perf_targets).iter().position(kind);
    for (index, name) in [
        (first(|t| matches!(t, PerfTarget::Cpu)), "cpu"),
        (first(|t| matches!(t, PerfTarget::Memory)), "memory"),
        (first(|t| matches!(t, PerfTarget::Disk(_))), "disk"),
        (first(|t| matches!(t, PerfTarget::Network(_))), "network"),
        (first(|t| matches!(t, PerfTarget::Gpu(_))), "gpu"),
    ] {
        let Some(index) = index else {
            continue;
        };
        (*p).perf_target = (&(*p).perf_targets)[index].clone();
        SendMessageW((*p).perf_list, LB_SETCURSEL, index, 0);
        redraw(p);
        capture::save_client(p, &dir.join(format!("perf-{name}.bmp")))?;
    }
    (*p).perf_target = PerfTarget::Cpu;
    SendMessageW((*p).perf_list, LB_SETCURSEL, 0, 0);
    // Processes sorted by GPU, busiest first (the flat list).
    switch_page(p, Page::Processes);
    let (group_mode, sort, descending) = ((*p).group_mode, (*p).sort, (*p).descending);
    (*p).group_mode = false;
    (*p).sort = 6;
    (*p).descending = true;
    rebuild(p, None);
    layout(p);
    update_buttons(p);
    capture::save_client(p, &dir.join("processes-gpu.bmp"))?;
    ((*p).group_mode, (*p).sort, (*p).descending) = (group_mode, sort, descending);
    rebuild(p, None);
    redraw(p);
    Ok(())
}

/// Render this application's own hidden native client, using read-only live data.
pub fn render_previews(dir: &std::path::Path) -> Result<(), String> {
    unsafe {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        init_controls();
        let (snapshots, rx) = mpsc::sync_channel(1);
        let (tx, _commands) = mpsc::channel();
        let (jobs, _work) = mpsc::channel();
        let (_complete, results) = mpsc::channel();
        let p = Box::into_raw(Box::new(App::new(rx, tx, jobs, results)));
        let class = wide("FeatherTaskRenderV2");
        let hwnd = create_window(p, &class, 1200, 820);
        if hwnd.is_null() {
            dispose(p);
            return Err("Preview window creation failed".into());
        }
        // Client 1200 × 820 compares 1:1 with the reference window renders.
        fit_client(hwnd, 1200, 820);
        let result = (|| {
            (*p).startup = crate::startup::list()?;
            (*p).startup_loaded = true;
            (*p).services = crate::services::list()?;
            (*p).services_loaded = true;
            let mut sampler = Sampler::new()?;
            let mut perf = PerfSampler::new()?;
            switch_page(p, Page::Performance);
            // Populate a complete one-minute trace with measured values.
            // Development iterations may shorten it; release previews keep 61 x 1 s.
            let env_u64 = |name: &str, default: u64| {
                std::env::var(name)
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(default)
            };
            let samples = env_u64("FEATHER_PREVIEW_SAMPLES", 61).clamp(2, 61);
            let interval = env_u64("FEATHER_PREVIEW_INTERVAL_MS", 1000).clamp(50, 1000);
            // The monitor's per-process joins, as the running app does.
            let mut gpu_tracker = ProcessGpuTracker::default();
            let mut network = NetworkMonitor::new();
            for i in 0..samples {
                if i > 0 {
                    std::thread::sleep(Duration::from_millis(interval));
                }
                let snapshot = sampler.sample();
                let performance = perf.sample();
                let (process_gpu, process_network) = match &snapshot {
                    Ok(s) => (
                        gpu_tracker.join(&s.processes, performance.as_ref().ok()),
                        network.sample(&s.processes),
                    ),
                    Err(_) => Default::default(),
                };
                snapshots
                    .send(MonitorSample {
                        at: Instant::now(),
                        manual_refresh: false,
                        snapshot,
                        performance: Some(performance),
                        process_gpu,
                        process_network,
                        resource_data: None,
                        resource_files: Default::default(),
                    })
                    .map_err(|e| e.to_string())?;
                drain_snapshot(p);
            }
            (*p).group_mode = true;
            let performance = (*p).performance.clone();
            (*p).startup_publishers = (*p)
                .startup
                .iter()
                .filter_map(|e| crate::startup::publisher(e).map(|v| (e.id.clone(), v)))
                .collect();
            for (page, name) in [
                (Page::Processes, "processes.bmp"),
                (Page::Performance, "performance.bmp"),
                (Page::Startup, "startup.bmp"),
                (Page::Services, "services.bmp"),
                (Page::Settings, "settings.bmp"),
            ] {
                switch_page(p, page);
                if page == Page::Performance {
                    (*p).performance = performance.clone();
                }
                (*p).startup_loading = false;
                (*p).services_loading = false;
                rebuild(p, None);
                layout(p);
                update_buttons(p);
                capture::save_client(p, &dir.join(name))?;
            }
            for theme in [2, 1] {
                (*p).prefs.theme = theme;
                interactions::apply_theme(p);
                for (page, name) in [
                    (Page::Processes, "processes"),
                    (Page::Performance, "performance"),
                    (Page::Startup, "startup"),
                    (Page::Services, "services"),
                    (Page::Settings, "settings"),
                ] {
                    switch_page(p, page);
                    if page == Page::Performance {
                        (*p).performance = performance.clone();
                    }
                    (*p).startup_loading = false;
                    (*p).services_loading = false;
                    rebuild(p, None);
                    layout(p);
                    update_buttons(p);
                    capture::save_client(
                        p,
                        &dir.join(format!(
                            "{name}-{}.bmp",
                            if theme == 2 { "dark" } else { "light" }
                        )),
                    )?;
                }
            }
            capture::save_foundation_previews(dir)?;
            save_layout_states(p, dir)?;
            table::save_previews(p, dir)?;
            controls::save_previews(p, dir)?;
            // The Nuclear Zombie panel before / during / after a run, from
            // read-only data (nothing is trimmed or purged).
            nuclear::save_previews(p, dir)?;
            resource_monitor::render_previews(p, dir)?;
            frame::save_previews(p, dir)?;
            switch_page(p, Page::Processes);
            set_tree_mode(p, true);
            capture::save_client(p, &dir.join("process-tree.bmp"))?;
            set_tree_mode(p, false);
            // Exercise the same native layout at its supported minimum size
            // and with the 150% WM_DPICHANGED font/layout path. The latter is
            // a controlled DPI simulation, not a second physical monitor.
            for (dpi, width, height, suffix) in
                [(96, 980, 660, "minimum"), (144, 1800, 1230, "dpi150")]
            {
                // These captures come long after the last preview sample: the
                // status bar would read "Waiting" instead of the live state.
                (*p).last_sample = Some(Instant::now());
                let r = RECT {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                };
                SendMessageW(
                    hwnd,
                    WM_DPICHANGED,
                    dpi | (dpi << 16),
                    &r as *const _ as isize,
                );
                fit_client(hwnd, width, height);
                for (page, name) in [
                    (Page::Processes, "processes"),
                    (Page::Performance, "performance"),
                    (Page::Settings, "settings"),
                ] {
                    switch_page(p, page);
                    if page == Page::Performance {
                        (*p).performance = performance.clone();
                    }
                    rebuild(p, None);
                    layout(p);
                    update_buttons(p);
                    capture::save_client(p, &dir.join(format!("{name}-{suffix}.bmp")))?;
                    if page == Page::Processes {
                        table::save_scrolled(p, &dir.join(format!("table-scrolled-{suffix}.bmp")))?;
                        set_tree_mode(p, true);
                        capture::save_client(p, &dir.join(format!("process-tree-{suffix}.bmp")))?;
                        set_tree_mode(p, false);
                    }
                }
            }
            Ok(())
        })();
        DestroyWindow(hwnd);
        dispose(p);
        UnregisterClassW(class.as_ptr(), GetModuleHandleW(null()));
        let mut msg: MSG = zeroed();
        while PeekMessageW(&mut msg, null_mut(), WM_QUIT, WM_QUIT, PM_REMOVE) != 0 {}
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    #[test]
    fn resource_monitor_shares_frames_freezes_and_clears_reused_pid_selection() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            resource_monitor::assert_snapshot_lifecycle(test.p);
        }
    }
    #[test]
    fn menus_and_dialogs_pause_sampling_but_keep_resource_monitor_tracing() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            resource_monitor::show_traced(test.p);
            let traced = (*test.p).resource_request;
            assert!(traced.1 && traced.2, "file and endpoint tracing are on");
            while test.commands.try_recv().is_ok() {}
            // A menu or confirm dialog: modal while it runs.
            (*test.p).modal = true;
            configure(test.p);
            (*test.p).modal = false;
            configure(test.p);
            let sent: Vec<_> = test.commands.try_iter().collect();
            assert!(
                !sent.iter().any(|c| matches!(c, Command::Resource(..))),
                "the traces are neither stopped nor restarted"
            );
            assert!(sent
                .iter()
                .any(|c| matches!(c, Command::Configure { paused: true, .. })));
            assert!(matches!(
                sent.last(),
                Some(Command::Configure { paused: false, .. })
            ));
            assert_eq!((*test.p).resource_request, traced);
            // Closing the window still stops them.
            resource_monitor::close(test.p);
            assert!(test.commands.try_iter().any(|c| matches!(
                c,
                Command::Resource(request, false, false) if request == Default::default()
            )));
        }
    }
    #[test]
    fn resource_monitor_confirms_ending_processes_over_its_own_window() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            resource_monitor::assert_end_confirmation(test.p);
        }
    }
    #[test]
    fn resource_monitor_rows_keep_their_identity_across_refreshes() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            resource_monitor::assert_rows_keep_identity(test.p);
        }
    }
    #[test]
    fn counts_and_memory_use_thousands_separators() {
        assert_eq!(grouped(7u32), "7");
        assert_eq!(grouped(470u32), "470");
        assert_eq!(grouped(2470u32), "2,470");
        assert_eq!(grouped(13837u64), "13,837");
        assert_eq!(grouped(789440usize), "789,440");
        assert_eq!(grouped(1234567u64), "1,234,567");
        assert_eq!(thousands(1499.84), "1,499.8");
        assert_eq!(thousands(12.26), "12.3");
        assert_eq!(thousands(-2048.0), "-2,048.0");
    }

    #[test]
    fn new_task_dialog_cancels_without_a_job_and_submits_exact_arguments() {
        use std::cell::{Cell, RefCell};
        use windows_sys::Win32::System::Threading::GetCurrentThreadId;
        /// What the hook does with the dialog: Cancel, Run, or Run a missing
        /// program (the dialog stays open with its error) and then Cancel.
        #[derive(Clone, Copy, PartialEq)]
        enum Plan {
            Cancel,
            Run,
            RunMissing,
        }
        thread_local! {
            static PLAN: Cell<(usize, Plan)> = const { Cell::new((0, Plan::Cancel)) };
            static PROGRAM: RefCell<String> = const { RefCell::new(String::new()) };
            static INSPECTED: Cell<bool> = const { Cell::new(false) };
            static ERROR_LINE: RefCell<String> = const { RefCell::new(String::new()) };
        }
        unsafe extern "system" fn dialog_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
            let dialog = w as HWND;
            let (owner, plan) = PLAN.get();
            let ours = |dialog: HWND| unsafe {
                GetWindow(dialog, GW_OWNER) as usize == owner && !GetDlgItem(dialog, 701).is_null()
            };
            if code == HCBT_DESTROYWND as i32 && ours(dialog) {
                let mut text = [0u16; 512];
                let length = GetDlgItemTextW(dialog, 705, text.as_mut_ptr(), text.len() as i32);
                ERROR_LINE.with(|line| {
                    *line.borrow_mut() = String::from_utf16_lossy(&text[..length as usize])
                });
            }
            if code == HCBT_ACTIVATE as i32 && ours(dialog) {
                INSPECTED.set(IsWindowEnabled(GetDlgItem(dialog, IDOK)) == 0);
                let program = PROGRAM.with(|program| wide(&program.borrow()));
                SetDlgItemTextW(dialog, 701, program.as_ptr());
                SetDlgItemTextW(dialog, 702, wide(r#""two words" &literal"#).as_ptr());
                // One click turns the owner-drawn administrator switch on
                // (BM_CLICK would re-activate the dialog inside this hook).
                SendMessageW(
                    dialog,
                    WM_COMMAND,
                    (BN_CLICKED as usize) << 16 | 704,
                    GetDlgItem(dialog, 704) as isize,
                );
                if plan != Plan::Cancel {
                    PostMessageW(dialog, WM_COMMAND, IDOK as usize, 0);
                }
                if plan != Plan::Run {
                    PostMessageW(dialog, WM_COMMAND, IDCANCEL as usize, 0);
                }
            }
            CallNextHookEx(null_mut(), code, w, l)
        }
        struct Hook(HHOOK);
        impl Drop for Hook {
            fn drop(&mut self) {
                unsafe {
                    UnhookWindowsHookEx(self.0);
                }
            }
        }
        let test = TestWindow::new();
        unsafe {
            while test.jobs.try_recv().is_ok() {}
            let hook = Hook(SetWindowsHookExW(
                WH_CBT,
                Some(dialog_hook),
                null_mut(),
                GetCurrentThreadId(),
            ));
            assert!(!hook.0.is_null());
            let set_program =
                |text: &str| PROGRAM.with(|program| *program.borrow_mut() = text.into());
            set_program(r#""C:\Program Files\Example\app.exe" --inline"#);
            PLAN.set(((*test.p).hwnd as usize, Plan::Cancel));
            SendMessageW((*test.p).hwnd, WM_COMMAND, RUN_TASK, 0);
            assert!(
                INSPECTED.get(),
                "Run starts disabled until a program is entered"
            );
            assert!(
                test.jobs.try_recv().is_err(),
                "Cancel must never enqueue a task"
            );
            assert!(!(*test.p).modal && !(*test.p).busy);
            // A program that does not resolve keeps the dialog open with the
            // reason in its error line, before anything is enqueued.
            PLAN.set(((*test.p).hwnd as usize, Plan::RunMissing));
            SendMessageW((*test.p).hwnd, WM_COMMAND, RUN_TASK, 0);
            assert!(test.jobs.try_recv().is_err(), "nothing runs");
            assert_eq!(
                ERROR_LINE.with(|line| line.borrow().clone()),
                tr(
                    "로컬 드라이브의 실행 파일(.exe 또는 .com)을 선택하세요.",
                    "Select an executable (.exe or .com) on a local drive.",
                )
            );
            assert!(!(*test.p).modal && !(*test.p).busy);
            let program = format!(
                "\"{}\" --inline",
                std::env::current_exe().unwrap().display()
            );
            set_program(&program);
            PLAN.set(((*test.p).hwnd as usize, Plan::Run));
            SendMessageW((*test.p).hwnd, WM_COMMAND, RUN_TASK, 0);
            let Job::Action(Action::RunTask(task)) = test.jobs.try_recv().expect("submitted task")
            else {
                panic!("unexpected job")
            };
            assert_eq!(task.command, program);
            assert_eq!(task.arguments, r#""two words" &literal"#);
            assert!(task.elevated);
            assert!(!(*test.p).modal);
            assert!(
                test.jobs.try_recv().is_err(),
                "submit enqueues exactly one task"
            );
        }
    }
    struct TestWindow {
        p: *mut App,
        class: Vec<u16>,
        snapshots: SyncSender<MonitorSample>,
        complete: Sender<JobResult>,
        commands: Receiver<Command>,
        jobs: Receiver<Job>,
    }
    impl TestWindow {
        fn new() -> Self {
            unsafe {
                init_controls();
                let (snapshots, rx) = mpsc::sync_channel(1);
                let (tx, commands) = mpsc::channel();
                let (jobs_tx, jobs) = mpsc::channel();
                let (complete, results) = mpsc::channel();
                let p = Box::into_raw(Box::new(App::new(rx, tx, jobs_tx, results)));
                let class = wide(&format!(
                    "FeatherV2Test_{}_{}",
                    std::process::id(),
                    NEXT.fetch_add(1, AtomicOrdering::Relaxed)
                ));
                assert!(!create_window(p, &class, 1200, 820).is_null());
                assert_eq!(IsWindowVisible((*p).hwnd), 0);
                Self {
                    p,
                    class,
                    snapshots,
                    complete,
                    commands,
                    jobs,
                }
            }
        }
        fn snapshot(&self, processes: Vec<Process>) {
            self.sample(processes, Instant::now());
        }
        fn failed_snapshot(&self, error: &str) {
            self.snapshots
                .send(MonitorSample {
                    at: Instant::now(),
                    manual_refresh: false,
                    snapshot: Err(error.into()),
                    performance: None,
                    process_gpu: Default::default(),
                    process_network: Default::default(),
                    resource_data: None,
                    resource_files: Default::default(),
                })
                .unwrap();
            unsafe {
                SendMessageW((*self.p).hwnd, SNAPSHOT_READY, 0, 0);
            }
        }
        fn sample(&self, processes: Vec<Process>, at: Instant) {
            self.sample_refresh(processes, at, false);
        }
        fn sample_refresh(&self, processes: Vec<Process>, at: Instant, manual_refresh: bool) {
            self.snapshots
                .send(MonitorSample {
                    at,
                    manual_refresh,
                    snapshot: Ok(Snapshot {
                        processes,
                        cpu_percent: 12.5,
                        memory_used: 1024,
                        memory_total: 4096,
                        sample_ms: 0.25,
                    }),
                    performance: None,
                    process_gpu: Default::default(),
                    process_network: Default::default(),
                    resource_data: None,
                    resource_files: Default::default(),
                })
                .unwrap();
            unsafe {
                SendMessageW((*self.p).hwnd, SNAPSHOT_READY, 0, 0);
            }
        }
        fn count(&self) -> usize {
            unsafe { SendMessageW((*self.p).list, LVM_GETITEMCOUNT, 0, 0) as usize }
        }
        fn text(&self, row: usize, column: i32) -> String {
            unsafe {
                let mut buffer = [0u16; 512];
                let mut item = LVITEMW {
                    mask: LVIF_TEXT,
                    iItem: row as i32,
                    iSubItem: column,
                    pszText: buffer.as_mut_ptr(),
                    cchTextMax: buffer.len() as i32,
                    ..zeroed()
                };
                assert_ne!(
                    SendMessageW(
                        (*self.p).list,
                        LVM_GETITEMW,
                        0,
                        &mut item as *mut _ as isize
                    ),
                    0
                );
                String::from_utf16_lossy(&buffer[..buffer.iter().position(|&s| s == 0).unwrap()])
            }
        }
        fn search(&self, value: &str) {
            unsafe {
                SetWindowTextW((*self.p).search, wide(value).as_ptr());
            }
        }
        fn select(&self, row: usize) {
            unsafe {
                let item = LVITEMW {
                    stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                    state: LVIS_SELECTED | LVIS_FOCUSED,
                    ..zeroed()
                };
                assert_ne!(
                    SendMessageW(
                        (*self.p).list,
                        LVM_SETITEMSTATE,
                        row,
                        &item as *const _ as isize
                    ),
                    0
                );
            }
        }
        fn page(&self, page: Page) {
            unsafe {
                SendMessageW((*self.p).hwnd, WM_COMMAND, NAV + page as usize, 0);
            }
        }
        fn sort(&self, col: i32) {
            unsafe {
                let event = NMLISTVIEW {
                    hdr: NMHDR {
                        hwndFrom: (*self.p).list,
                        idFrom: 200,
                        code: LVN_COLUMNCLICK,
                    },
                    iSubItem: col,
                    ..zeroed()
                };
                SendMessageW((*self.p).hwnd, WM_NOTIFY, 200, &event as *const _ as isize);
            }
        }
        fn result(&self, result: JobResult) {
            self.complete.send(result).unwrap();
            unsafe {
                SendMessageW((*self.p).hwnd, JOB_READY, 0, 0);
            }
        }
        fn identity(&self) -> Option<Identity> {
            unsafe { selected_identity(self.p) }
        }
    }
    impl Drop for TestWindow {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow((*self.p).hwnd);
                dispose(self.p);
                UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null()));
                let mut msg: MSG = zeroed();
                while PeekMessageW(&mut msg, null_mut(), WM_QUIT, WM_QUIT, PM_REMOVE) != 0 {}
            }
        }
    }
    fn process(pid: u32, created: u64, name: &str, memory: u64, cpu: f64) -> Process {
        Process {
            pid,
            parent_pid: 4,
            name: name.into(),
            created,
            cpu_percent: cpu,
            working_set: memory * 1048576,
            private_bytes: memory * 524288,
            io_bytes_per_sec: 2048.0,
            gpu_percent: None,
            network_bytes_per_sec: None,
            threads: 3,
            handles: 12,
            ..Process::default()
        }
    }
    fn rows() -> Vec<Process> {
        vec![
            process(101, 1001, "Alpha.exe", 9, 2.0),
            process(202, 1002, "Beta.exe", 100, 9.0),
            process(303, 1003, "테스트.exe", 2, 10.0),
        ]
    }
    #[test]
    fn process_header_drag_resize_preserve_semantic_sort_and_scroll_to_overflow() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            let list = (*p).list;
            let widths = || {
                (0..7)
                    .map(|i| SendMessageW(list, LVM_GETCOLUMNWIDTH, i, 0) as i32)
                    .collect::<Vec<_>>()
            };
            let width = widths();
            let middle = |column: usize| width[..column].iter().sum::<i32>() + width[column] / 2;
            let at = |x: i32| ((12i32 << 16) | (x & 0xffff)) as isize;
            SendMessageW(list, WM_LBUTTONDOWN, 0, at(middle(2)));
            SendMessageW(list, WM_MOUSEMOVE, 0, at(middle(4)));
            SendMessageW(list, WM_LBUTTONUP, 0, at(middle(4)));
            assert_eq!(
                (*p).process_columns.at(4),
                Some(process_columns::ProcessColumn::Cpu)
            );
            test.sort(4);
            assert_eq!((*p).sort, process_columns::ProcessColumn::Cpu as usize);
            assert_eq!(test.text(0, 0), "테스트.exe");
            assert_eq!(test.text(0, 4), "10.0%");
            let width = widths();
            let edge = width[..=4].iter().sum::<i32>();
            SendMessageW(list, WM_LBUTTONDOWN, 0, at(edge));
            SendMessageW(list, WM_MOUSEMOVE, 0, at(edge + 600));
            SendMessageW(list, WM_LBUTTONUP, 0, at(edge + 600));
            assert!((*p).process_columns.columns()[4].width > 400.0);
            // The overflow bar is the themed one: Windows' native bar (which
            // ignores the dark theme) never appears.
            let (offset, maximum, bar) = table::horizontal_state(list);
            assert_eq!(offset, 0);
            assert!(maximum > 0, "extra width must remain reachable");
            assert!(bar > 0);
            assert_eq!(GetWindowLongW(list, GWL_STYLE) as u32 & WS_HSCROLL, 0);
            let mut scroll = SCROLLINFO {
                cbSize: size_of::<SCROLLINFO>() as u32,
                fMask: SIF_ALL,
                ..zeroed()
            };
            assert_eq!(GetScrollInfo(list, SB_HORZ, &mut scroll), 0);
            SendMessageW(list, WM_HSCROLL, SB_RIGHT as usize, 0);
            assert_eq!(table::horizontal_state(list).0, maximum);
            // Resizing and scrolling never change the semantic sort column.
            assert_eq!((*p).sort, process_columns::ProcessColumn::Cpu as usize);
            test.page(Page::Services);
            assert_eq!(table::horizontal_state(list), (0, 0, 0));
            (*p).process_columns
                .toggle(process_columns::ProcessColumn::Memory);
            test.page(Page::Processes);
            assert_eq!((*p).sort, process_columns::ProcessColumn::Name as usize);
            assert!(!(*p).descending);
            assert_eq!(test.text(0, 0), "Alpha.exe");
            // Startup setup uses the same normalization for a restored layout.
            (*p).sort = process_columns::ProcessColumn::Memory as usize;
            (*p).descending = true;
            setup_columns(p);
            assert_eq!((*p).sort, process_columns::ProcessColumn::Name as usize);
            assert!(!(*p).descending);
        }
    }
    #[test]
    fn divider_clicks_keep_widths_and_page_switches_cancel_resizing() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            let list = (*p).list;
            let at = |x: i32| ((12i32 << 16) | (x & 0xffff)) as isize;
            let edge = |columns: usize| {
                (0..columns)
                    .map(|i| SendMessageW(list, LVM_GETCOLUMNWIDTH, i, 0) as i32)
                    .sum::<i32>()
            };
            let before = (*p).process_columns.columns().to_vec();
            // A click or double-click on the flexible Name column's divider
            // (the auto-fit habit) must not collapse it to the minimum.
            SendMessageW(list, WM_LBUTTONDOWN, 0, at(edge(1)));
            SendMessageW(list, WM_LBUTTONUP, 0, at(edge(1)));
            SendMessageW(list, WM_LBUTTONDBLCLK, 0, at(edge(1)));
            SendMessageW(list, WM_LBUTTONUP, 0, at(edge(1)));
            assert_eq!((*p).process_columns.columns(), &before[..]);
            // Changing page mid-drag drops the drag instead of applying the
            // process column index to the new page's columns.
            SendMessageW(list, WM_LBUTTONDOWN, 0, at(edge(7)));
            SendMessageW(list, WM_MOUSEMOVE, 0, at(edge(7) + 40));
            test.page(Page::Startup);
            SendMessageW(list, WM_LBUTTONUP, 0, at(edge(4) + 40));
            test.page(Page::Processes);
            assert_eq!((*p).process_columns.columns(), &before[..]);
        }
    }
    #[test]
    fn per_process_gpu_and_network_join_by_identity_format_sort_and_sum() {
        let mut snapshot = Snapshot {
            processes: rows(),
            cpu_percent: 0.,
            memory_used: 0,
            memory_total: 0,
            sample_ms: 0.,
        };
        let mut gpu = std::collections::HashMap::new();
        gpu.insert(
            (101, 1001),
            ProcessGpu {
                percent: Some(12.34),
                ..ProcessGpu::default()
            },
        );
        // A reused PID (other creation time) never inherits a value.
        gpu.insert(
            (202, 9999),
            ProcessGpu {
                percent: Some(50.),
                ..ProcessGpu::default()
            },
        );
        let mut network = ProcessNetworkSample {
            measured: true,
            ..ProcessNetworkSample::default()
        };
        network.by_id.insert(
            (202, 1002),
            crate::netetw::ProcessNet {
                send_bytes_per_sec: 50_000.,
                recv_bytes_per_sec: 800_000.,
                total_bytes_per_sec: 850_000.,
            },
        );
        join_process_samples(&mut snapshot, &gpu, &network, None);
        let [a, b, c] = &snapshot.processes[..] else {
            panic!("three rows");
        };
        assert_eq!(
            (a.gpu_percent, b.gpu_percent, c.gpu_percent),
            (Some(12.34), None, None)
        );
        assert_eq!(b.network_bytes_per_sec, Some(850_000.));
        assert_eq!(cell(a, 6), "12.3%");
        assert_eq!(cell(b, 6), "—");
        assert_eq!(cell(b, 5), "6.8 Mbps");
        assert_eq!(cell(a, 5), "—");
        // Unmeasured ranks below measured values.
        assert_eq!(compare(a, c, 6), Ordering::Greater);
        assert_eq!(compare(c, b, 5), Ordering::Less);
        // Unmeasured network samples show no values at all.
        network.measured = false;
        join_process_samples(&mut snapshot, &gpu, &network, None);
        assert!(snapshot
            .processes
            .iter()
            .all(|p| p.network_bytes_per_sec.is_none()));
        // App rows sum what was measured; GPU is capped at 100 %.
        let mut busy = snapshot.processes.clone();
        busy[0].gpu_percent = Some(70.);
        busy[1].gpu_percent = Some(45.);
        busy[1].network_bytes_per_sec = Some(1000.);
        let total = GroupTotal::of(&busy);
        assert_eq!(total.gpu, Some(100.));
        assert_eq!(total.network, Some(1000.));
        assert_eq!(GroupTotal::of(&snapshot.processes[2..]).gpu, None);
    }
    #[test]
    fn a_services_list_for_the_resource_monitor_leaves_a_paused_page_alone() {
        let test = TestWindow::new();
        test.page(Page::Services);
        assert!(matches!(test.jobs.try_recv(), Ok(Job::Services)));
        test.result(JobResult::Services(Ok(vec![service(
            "Alpha",
            SERVICE_RUNNING,
            120,
            Some(2),
        )])));
        assert_eq!(test.count(), 1);
        let two = || {
            vec![
                service("Alpha", SERVICE_STOPPED, 0, Some(2)),
                service("Beta", SERVICE_RUNNING, 7, Some(3)),
            ]
        };
        unsafe {
            (*test.p).paused = true;
            // The Resource Monitor's periodic request while the page is paused.
            (*test.p).services_loading = true;
            (*test.p).services_for_monitor = true;
            test.result(JobResult::Services(Ok(two())));
            assert_eq!(test.count(), 1, "the paused page keeps its frame");
            assert_eq!(test.text(0, 3), "실행 중");
            assert_eq!((*test.p).services.len(), 1);
            assert_eq!((*test.p).resource_services.as_ref().map(Vec::len), Some(2));
            assert!(!(*test.p).services_loading);
            // F5 is the main window's own request, even while one for the
            // Resource Monitor is still in flight: that list is shown.
            (*test.p).services_loading = true;
            (*test.p).services_for_monitor = true;
            command(test.p, REFRESH, 0);
            test.result(JobResult::Services(Ok(two())));
            assert_eq!(test.count(), 2);
            assert!((*test.p).resource_services.is_none());
        }
    }

    fn service(name: &str, state: u32, pid: u32, start_type: Option<u32>) -> Service {
        Service {
            name: name.into(),
            display_name: format!("{name} 표시 이름"),
            state,
            pid,
            start_type,
        }
    }

    #[test]
    fn embedded_window_icons_follow_dpi_and_product_name() {
        let test = TestWindow::new();
        unsafe {
            let mut title = [0u16; 128];
            let len = GetWindowTextW((*test.p).hwnd, title.as_mut_ptr(), title.len() as i32);
            assert_eq!(
                String::from_utf16_lossy(&title[..len as usize]),
                "Feather Task Manager"
            );
            for dpi in [96, 120, 144, 192] {
                (*test.p).dpi = dpi;
                update_window_icons(test.p);
                for (kind, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
                    let icon = SendMessageW((*test.p).hwnd, WM_GETICON, kind as usize, 0) as HICON;
                    assert!(!icon.is_null(), "Missing icon at {dpi} DPI");
                    let mut info: ICONINFO = zeroed();
                    assert_ne!(GetIconInfo(icon, &mut info), 0);
                    let mut bitmap: BITMAP = zeroed();
                    let got_bitmap = GetObjectW(
                        info.hbmColor,
                        size_of::<BITMAP>() as i32,
                        (&mut bitmap as *mut BITMAP).cast(),
                    );
                    if !info.hbmColor.is_null() {
                        DeleteObject(info.hbmColor);
                    }
                    if !info.hbmMask.is_null() {
                        DeleteObject(info.hbmMask);
                    }
                    assert_ne!(got_bitmap, 0);
                    let expected = GetSystemMetricsForDpi(metric, dpi as u32);
                    assert_eq!((bitmap.bmWidth, bitmap.bmHeight), (expected, expected));
                }
            }
        }
    }
    #[test]
    fn a_new_process_count_repaints_the_processes_nav_item() {
        let test = TestWindow::new();
        unsafe {
            let p = test.p;
            let hwnd = (*p).hwnd;
            // Shown fully transparent and click-through: hidden windows keep
            // no update region.
            SetWindowLongPtrW(
                hwnd,
                GWL_EXSTYLE,
                GetWindowLongPtrW(hwnd, GWL_EXSTYLE)
                    | (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT)
                        as isize,
            );
            SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA);
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let nav = (*p).nav[0];
            let pending = || {
                let mut r: RECT = zeroed();
                GetUpdateRect(nav, &mut r, 0) != 0
            };
            test.snapshot(rows());
            ValidateRect(nav, null());
            test.snapshot(rows());
            assert!(!pending(), "the same count leaves the nav item alone");
            test.snapshot(rows()[..2].to_vec());
            assert!(pending(), "the count in the nav item changed");
            ShowWindow(hwnd, SW_HIDE);
        }
    }
    #[test]
    fn native_virtual_table_search_sort_and_display_are_consistent() {
        let test = TestWindow::new();
        test.snapshot(rows());
        assert_eq!(test.count(), 3);
        assert_eq!(test.text(0, 0), "Beta.exe");
        assert_eq!(test.text(0, 3), "100.0 MB");
        test.sort(2);
        assert_eq!(test.text(0, 0), "테스트.exe");
        test.sort(2);
        assert_eq!(test.text(0, 0), "Alpha.exe");
        test.search("  aLPHa  ");
        assert_eq!(test.count(), 1);
        assert_eq!(test.text(0, 1), "101");
        test.search("202");
        assert_eq!(test.text(0, 0), "Beta.exe");
        test.search("테스트");
        assert_eq!(test.text(0, 0), "테스트.exe");
        test.search("absent");
        assert_eq!(test.count(), 0);
        unsafe {
            assert!(empty_message(test.p).contains("검색 결과"));
        }
        test.search("");
        assert_eq!(test.count(), 3);
    }
    #[test]
    fn tree_view_keeps_selection_and_numeric_cells_aligned_through_collapse_and_search() {
        let test = TestWindow::new();
        let mut root = process(201, 100, "Parent.exe", 30, 1.0);
        root.parent_pid = 4;
        let mut child = process(202, 200, "Child.exe", 20, 2.0);
        child.parent_pid = 201;
        let mut leaf = process(203, 300, "Leaf.exe", 10, 3.0);
        leaf.parent_pid = 202;
        test.snapshot(vec![root, child, leaf]);
        test.select(0);
        unsafe {
            set_tree_mode(test.p, true);
        }
        assert_eq!(test.identity(), Some(Identity::Process(201, 100)));
        assert_eq!(test.text(0, 0), "Parent.exe");
        assert_eq!(test.text(1, 1), "202");
        assert_eq!(test.text(2, 3), "10.0 MB");
        unsafe {
            assert_eq!(
                (*test.p)
                    .tree_rows
                    .iter()
                    .map(|row| row.depth)
                    .collect::<Vec<_>>(),
                [0, 1, 2]
            );
            let mut bounds = RECT {
                left: LVIR_BOUNDS as i32,
                ..zeroed()
            };
            SendMessageW(
                (*test.p).list,
                LVM_GETITEMRECT,
                0,
                &mut bounds as *mut _ as isize,
            );
            let click = NMITEMACTIVATE {
                hdr: NMHDR {
                    hwndFrom: (*test.p).list,
                    idFrom: 200,
                    code: NM_CLICK,
                },
                iItem: 0,
                iSubItem: 0,
                // The chevron: 20 × 20 inside the name cell's 12 px padding.
                ptAction: POINT {
                    x: bounds.left + scale(test.p, 22),
                    y: (bounds.top + bounds.bottom) / 2,
                },
                ..zeroed()
            };
            SendMessageW((*test.p).hwnd, WM_NOTIFY, 200, &click as *const _ as isize);
        }
        assert_eq!(test.count(), 1);
        assert_eq!(test.identity(), Some(Identity::Process(201, 100)));
        let plan = unsafe { captured_tree_plan(test.p).unwrap() };
        assert_eq!(
            plan.len(),
            3,
            "A collapsed branch must still target its full captured tree"
        );
        assert_eq!(plan.targets()[0].pid, 203);
        test.search("leaf");
        assert_eq!(
            test.count(),
            3,
            "Filtering reveals matches and their ancestry"
        );
        test.search("");
        assert_eq!(test.count(), 1, "Filtering must preserve collapsed state");
        unsafe {
            toggle_selected_branch(test.p, Some(true));
            set_tree_mode(test.p, false);
        }
        assert_eq!(test.count(), 3);
        assert_eq!(test.identity(), Some(Identity::Process(201, 100)));
        test.snapshot(vec![process(901, 901, "New.exe", 2, 1.0)]);
        assert_eq!(
            plan.len(),
            3,
            "A confirmation plan cannot be replaced by later samples"
        );
        assert_eq!(plan.root.pid, 201);
    }
    #[test]
    fn language_switch_updates_native_controls_and_retains_selection_and_filter() {
        let test = TestWindow::new();
        test.snapshot(rows());
        test.search("Beta");
        test.select(0);
        let identity = test.identity();
        crate::i18n::with_language(Language::English, || unsafe {
            refresh_language(test.p);
            assert_eq!(Page::Processes.title(), "Processes");
            assert_eq!(columns(Page::Processes)[3].0, "Memory");
            assert_eq!(test.identity(), identity);
            assert_eq!(test.count(), 1);
            let mut text = [0u16; 80];
            let length = GetWindowTextW((*test.p).end_tree, text.as_mut_ptr(), 80);
            assert_eq!(
                String::from_utf16_lossy(&text[..length as usize]),
                "End process tree"
            );
            let length = GetWindowTextW((*test.p).settings, text.as_mut_ptr(), 80);
            assert_eq!(
                String::from_utf16_lossy(&text[..length as usize]),
                "Settings"
            );
            assert!(empty_message(test.p).contains("No matching results"));
            let menu = create_settings_menu(&Ok(crate::replacement::Status::Inactive));
            assert_ne!(
                GetMenuState(menu, LANGUAGE_ENGLISH as u32, MF_BYCOMMAND) & MF_CHECKED,
                0
            );
            assert_eq!(
                GetMenuState(menu, LANGUAGE_KOREAN as u32, MF_BYCOMMAND) & MF_CHECKED,
                0
            );
            DestroyMenu(menu);
        });
        unsafe {
            refresh_language(test.p);
        }
        assert_eq!(test.identity(), identity);
    }
    #[test]
    fn language_switch_retries_in_flight_startup_once_without_losing_selection() {
        let test = TestWindow::new();
        let entries = crate::startup::list().unwrap();
        test.page(Page::Startup);
        assert!(matches!(test.jobs.try_recv(), Ok(Job::Startup)));
        test.result(JobResult::Startup(Ok(entries.clone())));
        if !entries.is_empty() {
            test.select(0);
        }
        let identity = test.identity();
        unsafe {
            request_list(test.p, Page::Startup);
        }
        assert!(matches!(test.jobs.try_recv(), Ok(Job::Startup)));
        crate::i18n::with_language(Language::English, || unsafe {
            refresh_language(test.p);
            assert!((*test.p).startup_refresh_pending);
            assert!(!(*test.p).startup_loaded);
            assert!(
                test.jobs.try_recv().is_err(),
                "Do not duplicate the in-flight request"
            );
            assert_eq!(test.identity(), identity);
            // This old result would erase the selected row if accepted.
            test.result(JobResult::Startup(Ok(Vec::new())));
            assert_eq!(test.count(), entries.len());
            assert_eq!(test.identity(), identity);
            assert!(matches!(test.jobs.try_recv(), Ok(Job::Startup)));
            assert!(
                test.jobs.try_recv().is_err(),
                "Schedule only one replacement request"
            );
            assert!(!(*test.p).startup_refresh_pending);
            assert!((*test.p).startup_loading);
            let mut localized = entries.clone();
            for entry in &mut localized {
                entry.status = "Fresh English status".into();
                entry.location = "Fresh English location".into();
            }
            test.result(JobResult::Startup(Ok(localized)));
            assert_eq!(test.identity(), identity);
            if !entries.is_empty() {
                assert_eq!(test.text(0, 3), "Fresh English status");
                let row = selected_row(test.p).unwrap();
                assert_eq!((&(*test.p).startup)[row].location, "Fresh English location");
            }
            assert!((*test.p).startup_loaded);
            assert!(!(*test.p).startup_loading);
            drain_jobs(test.p);
            assert!(
                test.jobs.try_recv().is_err(),
                "No continuing refresh after recovery"
            );
        });
    }
    #[test]
    fn language_switch_invalidates_inactive_startup_cache_and_discards_old_errors() {
        let test = TestWindow::new();
        test.page(Page::Startup);
        assert!(matches!(test.jobs.try_recv(), Ok(Job::Startup)));
        test.page(Page::Processes);
        test.snapshot(rows());
        test.select(0);
        let identity = test.identity();
        crate::i18n::with_language(Language::English, || unsafe {
            refresh_language(test.p);
            test.result(JobResult::Startup(Err("Old-language failure".into())));
            assert!((*test.p).error.is_none());
            assert!(!(*test.p).startup_loaded);
            assert!(
                test.jobs
                    .try_iter()
                    .all(|job| matches!(job, Job::ProcessDetails(..))),
                "Keep inactive startup pages lazy; selected process metadata is read-only"
            );
            assert_eq!(test.identity(), identity);
            test.page(Page::Startup);
            assert!(matches!(test.jobs.try_recv(), Ok(Job::Startup)));
            test.result(JobResult::Startup(Ok(Vec::new())));
            test.page(Page::Processes);
            refresh_language(test.p);
            assert!(
                !(*test.p).startup_loaded,
                "Invalidate an already-loaded inactive cache too"
            );
            test.page(Page::Startup);
            assert!(matches!(test.jobs.try_recv(), Ok(Job::Startup)));
            assert!(test.jobs.try_recv().is_err());
        });
    }
    #[test]
    fn native_process_selection_survives_reordering_but_not_pid_reuse() {
        let test = TestWindow::new();
        test.snapshot(rows());
        test.select(1);
        assert_eq!(test.identity(), Some(Identity::Process(101, 1001)));
        let mut changed = rows();
        changed[0].working_set = 500 * 1048576;
        test.snapshot(changed);
        assert_eq!(test.text(0, 1), "101");
        assert_eq!(test.identity(), Some(Identity::Process(101, 1001)));
        let mut changed = rows();
        changed[0].created = 2001;
        test.snapshot(changed);
        assert_eq!(test.identity(), None);
        unsafe {
            assert_eq!(IsWindowEnabled((*test.p).primary), 0);
            assert_eq!(IsWindowEnabled((*test.p).secondary), 0);
        }
        test.snapshot(vec![process(std::process::id(), 5, "Self", 10, 0.0)]);
        test.select(0);
        unsafe {
            assert_eq!(IsWindowEnabled((*test.p).primary), 0);
        }
    }
    #[test]
    fn navigation_roundtrip_preserves_exact_identity_and_ignores_stale_source() {
        for (group_mode, tree_mode) in [(false, false), (true, false), (false, true)] {
            let test = TestWindow::new();
            let parent = process(10, 100, "Parent.exe", 30, 0.0);
            let mut host = process(12, 200, "Host.exe", 20, 0.0);
            host.parent_pid = 10;
            let id = ProcessIdentity::from(&host);
            unsafe {
                (*test.p).group_mode = group_mode;
                (*test.p).tree_mode = tree_mode;
                (*test.p).window_pids = HashSet::from([10]);
                (*test.p).last_window_scan = Some(Instant::now());
                (*test.p).collapsed.insert(ProcessIdentity::from(&parent));
            }
            test.snapshot(vec![parent, host, process(120, 300, "Other.exe", 10, 0.0)]);
            test.search("pid:12");
            test.select(
                (0..test.count())
                    .find(|&row| test.text(row, 1) == "12")
                    .unwrap(),
            );
            let request = navigation::Request::Services(id);
            unsafe {
                navigation::begin(test.p, request.clone());
            }
            assert!(test
                .jobs
                .try_iter()
                .any(|job| matches!(job, Job::Navigate(_))));
            test.result(JobResult::Navigate(
                request,
                Ok(navigation::Target::Services(
                    vec![
                        service("HostService", SERVICE_RUNNING, 12, Some(2)),
                        service("OtherService", SERVICE_RUNNING, 120, Some(2)),
                    ],
                    12,
                )),
            ));
            assert_eq!(test.count(), 1);
            assert_eq!(
                test.identity(),
                Some(Identity::Service("HostService".into()))
            );

            let request = navigation::Request::Process("HostService".into());
            unsafe {
                navigation::begin(test.p, request.clone());
            }
            test.result(JobResult::Navigate(
                request,
                Ok(navigation::Target::Process(id)),
            ));
            assert_eq!(test.identity(), Some(Identity::Process(12, 200)));
            assert!((0..test.count()).all(|row| test.text(row, 1) != "120"));
            unsafe {
                assert_eq!(
                    ((*test.p).group_mode, (*test.p).tree_mode),
                    (group_mode, tree_mode)
                );
                assert!((*test.p).collapsed.contains(&ProcessIdentity {
                    pid: 10,
                    created: 100
                }));
            }

            // A different page must discard both delayed success and its target.
            let request = navigation::Request::Services(id);
            unsafe {
                navigation::begin(test.p, request.clone());
            }
            test.page(Page::Performance);
            test.result(JobResult::Navigate(
                request,
                Ok(navigation::Target::Services(
                    vec![service("StaleService", SERVICE_RUNNING, 12, Some(2))],
                    12,
                )),
            ));
            unsafe {
                assert_eq!((*test.p).page, Page::Performance);
            }

            // A different selected process must also discard a delayed error.
            test.page(Page::Processes);
            test.search("pid:12");
            test.select(
                (0..test.count())
                    .find(|&row| test.text(row, 1) == "12")
                    .unwrap(),
            );
            let request = navigation::Request::Services(id);
            unsafe {
                navigation::begin(test.p, request.clone());
            }
            test.search("pid:120");
            test.select(
                (0..test.count())
                    .find(|&row| test.text(row, 1) == "120")
                    .unwrap(),
            );
            test.result(JobResult::Navigate(
                request,
                Err("stale navigation failure".into()),
            ));
            assert_eq!(test.identity(), Some(Identity::Process(120, 300)));
            unsafe {
                assert!((*test.p).error.is_none());
                assert!(!(*test.p).busy);
            }
        }
    }
    #[test]
    fn four_pages_switch_native_controls_and_lazily_request_lists() {
        let test = TestWindow::new();
        test.snapshot(rows());
        test.page(Page::Performance);
        unsafe {
            assert_eq!((*test.p).page, Page::Performance);
            assert_eq!(
                GetWindowLongW((*test.p).list, GWL_STYLE) as u32 & WS_VISIBLE,
                0
            );
            assert_eq!(IsWindowEnabled((*test.p).search), 0);
        }
        assert!(test.jobs.try_recv().is_err());
        test.page(Page::Startup);
        assert!(matches!(test.jobs.try_recv(), Ok(Job::Startup)));
        test.result(JobResult::Startup(Ok(Vec::new())));
        unsafe {
            assert_eq!(table::column_count((*test.p).list), 4);
        }
        test.page(Page::Processes);
        test.page(Page::Startup);
        assert!(test.jobs.try_recv().is_err());
        test.page(Page::Services);
        assert!(matches!(test.jobs.try_recv(), Ok(Job::Services)));
        test.result(JobResult::Services(Ok(vec![service(
            "Running",
            SERVICE_RUNNING,
            42,
            Some(2),
        )])));
        assert_eq!(test.count(), 1);
        assert_eq!(test.text(0, 3), "실행 중");
        test.page(Page::Processes);
        assert_eq!(test.count(), 3);
        assert_eq!(test.text(0, 0), "Beta.exe");
    }
    #[test]
    fn services_selection_actions_and_numeric_sort_respect_state_and_busy() {
        let test = TestWindow::new();
        test.page(Page::Services);
        let _ = test.jobs.recv().unwrap();
        test.result(JobResult::Services(Ok(vec![
            service("Alpha", SERVICE_RUNNING, 120, Some(2)),
            service("Beta", SERVICE_STOPPED, 0, Some(3)),
            service("Disabled", SERVICE_STOPPED, 0, Some(4)),
        ])));
        test.select(0);
        unsafe {
            assert_eq!(IsWindowEnabled((*test.p).primary), 0);
            assert_ne!(IsWindowEnabled((*test.p).secondary), 0);
        }
        test.sort(1);
        assert_eq!(test.identity(), Some(Identity::Service("Alpha".into())));
        test.search("Beta");
        test.select(0);
        unsafe {
            assert_ne!(IsWindowEnabled((*test.p).primary), 0);
            assert_eq!(IsWindowEnabled((*test.p).secondary), 0);
            (*test.p).busy = true;
            update_buttons(test.p);
            assert_eq!(IsWindowEnabled((*test.p).primary), 0);
            command(test.p, PRIMARY, 0);
        }
        assert!(
            test.jobs
                .try_iter()
                .all(|job| matches!(job, Job::ServiceDetails(_))),
            "Busy state cannot enqueue management actions"
        );
        unsafe {
            (*test.p).busy = false;
            update_buttons(test.p);
        }
        test.search("Disabled");
        test.select(0);
        unsafe {
            assert_eq!(IsWindowEnabled((*test.p).primary), 0);
        }
    }
    #[test]
    fn startup_uses_status_and_stable_identity_without_mutating_registry() {
        let entries = crate::startup::list().unwrap();
        let test = TestWindow::new();
        test.page(Page::Startup);
        let _ = test.jobs.recv().unwrap();
        test.result(JobResult::Startup(Ok(entries.clone())));
        assert_eq!(test.count(), entries.len());
        if !entries.is_empty() {
            test.select(0);
            let identity = test.identity();
            let row = unsafe { selected_row(test.p).unwrap() };
            assert_eq!(test.text(0, 3), entries[row].status);
            test.sort(2);
            assert_eq!(test.identity(), identity);
            unsafe {
                assert_eq!(
                    IsWindowEnabled((*test.p).primary) != 0,
                    entries[row].manageable
                );
                (*test.p).busy = true;
                update_buttons(test.p);
                command(test.p, PRIMARY, 0);
            }
            assert!(test.jobs.try_recv().is_err());
        }
        test.search("impossible-startup-query-987654");
        assert_eq!(test.count(), 0);
    }
    #[test]
    fn modal_freezes_sample_and_job_delivery_then_resumes_once() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            (*test.p).modal = true;
        }
        test.snapshot(vec![process(999, 77, "Replacement", 1, 2.0)]);
        assert_eq!(test.count(), 3);
        test.result(JobResult::Services(Ok(vec![service(
            "Saved",
            SERVICE_RUNNING,
            1,
            Some(2),
        )])));
        unsafe {
            assert!((*test.p).services.is_empty());
            (*test.p).modal = false;
            drain_snapshot(test.p);
            drain_jobs(test.p);
            assert_eq!((*test.p).history.len(), 2);
            drain_snapshot(test.p);
            assert_eq!((*test.p).history.len(), 2);
            assert_eq!((*test.p).services.len(), 1);
        }
        assert_eq!(test.text(0, 0), "Replacement");
    }
    #[test]
    fn successful_monitor_refresh_clears_its_transient_error() {
        let test = TestWindow::new();
        test.failed_snapshot("Temporary process collection failure");
        unsafe {
            assert_eq!((*test.p).error_source, Some(ErrorSource::Monitor));
            assert!(selected_detail(test.p).contains("Temporary process"));
        }
        test.snapshot(rows());
        unsafe {
            assert!((*test.p).error.is_none());
            assert!((*test.p).error_source.is_none());
        }
        assert_eq!(test.count(), 3);
    }
    #[test]
    fn only_matching_list_success_clears_its_load_error() {
        let test = TestWindow::new();
        test.page(Page::Services);
        test.result(JobResult::Services(Err("Temporary service failure".into())));
        test.snapshot(rows());
        unsafe {
            assert_eq!((*test.p).error_source, Some(ErrorSource::Services));
        }
        test.result(JobResult::Services(Ok(vec![service(
            "Recovered",
            SERVICE_RUNNING,
            42,
            Some(2),
        )])));
        unsafe {
            assert!((*test.p).error.is_none());
        }
        test.page(Page::Startup);
        test.result(JobResult::Startup(Err("Temporary startup failure".into())));
        test.result(JobResult::Services(Ok(Vec::new())));
        test.snapshot(rows());
        unsafe {
            assert_eq!((*test.p).error_source, Some(ErrorSource::Startup));
        }
        test.result(JobResult::Startup(Ok(Vec::new())));
        unsafe {
            assert!((*test.p).error.is_none());
        }
    }
    #[test]
    fn action_failures_survive_background_failures_and_recovery() {
        let test = TestWindow::new();
        test.page(Page::Services);
        test.result(JobResult::Action {
            result: Err("Service action denied".into()),
            page: Page::Services,
            notice: "unused".into(),
        });
        test.failed_snapshot("Unrelated monitor failure");
        test.snapshot(rows());
        test.result(JobResult::Services(Err("Unrelated list failure".into())));
        test.result(JobResult::Services(Ok(Vec::new())));
        unsafe {
            assert_eq!((*test.p).error.as_deref(), Some("Service action denied"));
            assert_eq!((*test.p).error_source, Some(ErrorSource::Action));
            command(test.p, REFRESH, 0);
            assert!((*test.p).error.is_none());
            assert!((*test.p).error_source.is_none());
        }
    }
    #[test]
    fn chart_history_is_bounded_and_missing_rates_are_gaps() {
        let test = TestWindow::new();
        let now = Instant::now();
        for i in 0..140 {
            test.sample(Vec::new(), now + Duration::from_millis(i * 500));
        }
        unsafe {
            assert_eq!((*test.p).history.len(), 120);
            assert!((*test.p)
                .history
                .iter()
                .all(|h| h.disk.is_nan() && h.network.is_nan()));
            assert!((*test.p).history.iter().all(|h| h.memory == 25.0));
        }
        test.sample(Vec::new(), now + Duration::from_secs(200));
        unsafe {
            assert_eq!((*test.p).history.len(), 1);
        }
    }
    #[test]
    fn ifeo_target_arguments_do_not_select_a_feather_page() {
        let args = |values: &[&str]| values.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            initial_page(&args(&["Feather.exe", "--page", "performance"])),
            Page::Performance
        );
        assert_eq!(
            initial_page(&args(&[
                "Feather.exe",
                "--task-manager",
                "C:\\Windows\\System32\\taskmgr.exe",
                "--page",
                "performance"
            ])),
            Page::Processes
        );
    }
    #[test]
    fn replacement_menu_only_offers_valid_transitions() {
        use crate::replacement::Status;
        for (status, replace, restore) in [
            (Ok(Status::Inactive), true, false),
            (Ok(Status::Active), false, true),
            (Ok(Status::Other("another debugger".into())), false, false),
            (Err("access denied".into()), false, false),
        ] {
            unsafe {
                let menu = create_settings_menu(&status);
                assert!(!menu.is_null());
                for (id, expected) in [
                    (REPLACE_TASK_MANAGER, replace),
                    (RESTORE_TASK_MANAGER, restore),
                    (ELEVATE, true),
                ] {
                    let state = GetMenuState(menu, id as u32, MF_BYCOMMAND);
                    assert_ne!(state, u32::MAX);
                    assert_eq!(state & (MF_GRAYED | MF_DISABLED) == 0, expected);
                }
                assert_ne!(DestroyMenu(menu), 0);
            }
        }
    }
    #[test]
    fn pause_minimize_and_interval_are_forwarded_to_monitor() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let first = (*test.p).snapshot.clone().unwrap();
            command(test.p, PAUSE, 0);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure {
                    paused: true,
                    interval: 1000,
                    ..
                }
            ));
            // A queued automatic frame (including one from a just-closed
            // Resource Monitor) cannot change the paused main window.
            test.snapshot(vec![process(909, 99, "Queued.exe", 2, 1.)]);
            assert!(Arc::ptr_eq(&first, (*test.p).snapshot.as_ref().unwrap()));
            command(test.p, REFRESH, 0);
            assert!(matches!(test.commands.recv().unwrap(), Command::Refresh));
            test.sample_refresh(
                vec![process(909, 99, "Manual.exe", 2, 1.)],
                Instant::now(),
                true,
            );
            assert_eq!(test.text(0, 0), "Manual.exe");
            assert!(
                (*test.p).paused,
                "a one-shot refresh does not resume sampling"
            );
            SendMessageW((*test.p).rate, CB_SETCURSEL, 5, 0);
            command(test.p, RATE, CBN_SELCHANGE);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure {
                    paused: false,
                    interval: 5000,
                    ..
                }
            ));
            SendMessageW((*test.p).hwnd, WM_SIZE, SIZE_MINIMIZED as usize, 0);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure { paused: true, .. }
            ));
            let minimized = (*test.p).snapshot.clone().unwrap();
            test.snapshot(rows());
            assert!(Arc::ptr_eq(
                &minimized,
                (*test.p).snapshot.as_ref().unwrap()
            ));
            SendMessageW((*test.p).hwnd, WM_SIZE, SIZE_RESTORED as usize, 0);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure { paused: false, .. }
            ));
        }
    }
    #[test]
    fn new_modal_exit_reposts_consumed_notifications_and_unblocks_sampling() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            (*test.p).modal = true;
        }
        test.snapshot(vec![process(909, 99, "Queued.exe", 2, 1.)]);
        test.result(JobResult::Services(Ok(vec![service(
            "Queued",
            SERVICE_RUNNING,
            1,
            Some(2),
        )])));
        unsafe {
            interactions::finish_modal(test.p);
            let mut msg: MSG = zeroed();
            let mut delivered = 0;
            while PeekMessageW(
                &mut msg,
                (*test.p).hwnd,
                SNAPSHOT_READY,
                JOB_READY,
                PM_REMOVE,
            ) != 0
            {
                DispatchMessageW(&msg);
                delivered += 1;
            }
            assert!(delivered >= 2);
            assert_eq!((*test.p).services.len(), 1);
        }
        assert_eq!(test.text(0, 0), "Queued.exe");
        test.snapshot(rows());
        assert_eq!(test.count(), 3);
    }
    #[test]
    fn grouped_headers_are_never_process_targets_and_real_selection_survives() {
        let test = TestWindow::new();
        test.snapshot(rows());
        test.select(0);
        let identity = test.identity();
        unsafe {
            (*test.p).group_mode = true;
            (*test.p).window_pids.insert(202);
            (*test.p).last_window_scan = Some(Instant::now());
            rebuild(test.p, identity.clone());
            assert_eq!(test.identity(), identity);
            assert_eq!((*test.p).group_headers.len(), 2);
            test.select(0);
            assert!(test.identity().is_none());
            assert!(selected_process(test.p).is_none());
            assert_eq!(IsWindowEnabled((*test.p).primary), 0);
            set_tree_mode(test.p, true);
            assert!(!(*test.p).group_mode);
            assert!((*test.p).group_headers.is_empty());
        }
    }
    /// Service details: nothing selected, the list row's facts while the
    /// query runs, then the answer — with an empty description, path and
    /// dependencies (third-party services have none; an empty string used
    /// to reach DrawTextW with a dangling pointer and crash); the Startup
    /// type column steps aside while the panel shows.
    #[test]
    fn service_details_panel_flows_and_survives_empty_fields() {
        let test = TestWindow::new();
        test.page(Page::Services);
        let _ = test.jobs.recv().unwrap();
        test.result(JobResult::Services(Ok(vec![
            service("NoDescription", SERVICE_RUNNING, 4242, Some(2)),
            service("Other", SERVICE_STOPPED, 0, Some(3)),
        ])));
        unsafe {
            let p = test.p;
            fit_client((*p).hwnd, 1200, 820);
            assert_eq!(table::column_count((*p).list), 5);
            (*p).show_details = true;
            layout(p);
            assert_eq!(table::column_count((*p).list), 4, "Startup type hides");
            let l = current_layout(p);
            let panel = l.details(true).1.unwrap();
            let mut frame = gfx::Dib::new(1200, 820).unwrap();
            // Ink (anything but the panel background) inside a band.
            let ink = |frame: &mut gfx::Dib, top: i32, bottom: i32| {
                let bg = colors().bg;
                let bg = (bg & 0xff) << 16 | (bg & 0xff00) | (bg >> 16 & 0xff);
                (top..bottom)
                    .flat_map(|y| (panel.left + 16..panel.right - 16).map(move |x| (x, y)))
                    .filter(|&(x, y)| frame.pixel(x, y) & 0x00ff_ffff != bg)
                    .count()
            };
            let header = layout::table_header_height(Page::Services, 96);
            let first_row = panel.top + header..panel.top + header + 34;
            paint::paint_to(p, frame.dc());
            assert!(
                ink(&mut frame, first_row.start, first_row.end) > 0,
                "Select a service"
            );
            test.select(0);
            assert!(matches!(
                test.jobs.try_recv(),
                Ok(Job::ServiceDetails(name)) if name == "NoDescription"
            ));
            // Loading: the name on the first row's line, the state pill below.
            paint::paint_to(p, frame.dc());
            assert!(ink(&mut frame, first_row.start, first_row.end) > 0);
            assert!(ink(&mut frame, first_row.end, first_row.end + 40) > 0);
            let details = crate::services::ServiceDetails {
                name: "NoDescription".into(),
                display_name: "NoDescription".into(),
                description: String::new(),
                account: String::new(),
                binary_path: String::new(),
                load_order_group: String::new(),
                dependencies: Vec::new(),
                state: SERVICE_RUNNING,
                pid: 4242,
                start_type: 2,
                warnings: Vec::new(),
            };
            test.result(JobResult::ServiceDetails(
                "NoDescription".into(),
                Ok(details.clone()),
            ));
            assert!((*p).service_details.is_some());
            // (The dark theme is covered by the previews: flipping the
            // process-wide palette here would race the parallel tests.)
            paint::paint_to(p, frame.dc());
            assert!(ink(&mut frame, first_row.start, first_row.end) > 0);
            // A long unbroken path wraps inside the panel, never past it.
            let path = format!("C:\\{}\\service.exe", "VeryLongDirectoryName".repeat(6));
            (*p).service_details = Some(crate::services::ServiceDetails {
                binary_path: path,
                description: "A description long enough to wrap onto several lines of \
                              the details panel without leaving holes between fields."
                    .into(),
                ..details
            });
            paint::paint_to(p, frame.dc());
            (*p).show_details = false;
            layout(p);
            assert_eq!(table::column_count((*p).list), 5);
        }
    }
    /// The grouped view ("App groups"): every process with a window is an
    /// app, its windowless descendants belong to it (not under the shell,
    /// which starts every sign-in app), an app of the same image nests, and
    /// the rest splits into background and Windows processes.
    #[test]
    fn app_groups_follow_windows_and_the_process_tree() {
        let tree = |pid: u32, parent: u32, name: &str| Process {
            parent_pid: parent,
            ..process(pid, u64::from(pid), name, 10, 1.0)
        };
        let processes = vec![
            tree(10, 0, "explorer.exe"),
            tree(20, 10, "chrome.exe"),
            tree(21, 20, "chrome.exe"),
            tree(22, 20, "chrome.exe"),
            tree(23, 21, "crashpad_handler.exe"),
            tree(24, 20, "chrome.exe"),
            tree(30, 10, "OneDrive.exe"),
            tree(31, 10, "explorer.exe"),
            tree(40, 0, "svchost.exe"),
            tree(50, 0, "Code.exe"),
            tree(51, 50, "node.exe"),
        ];
        // Windows: explorer (10, the shell), chrome (20 and its second
        // window 24), Code.
        let windows = HashSet::from([10, 20, 24, 50]);
        let app = interactions::app_groups(&processes, &windows, Some(10));
        assert_eq!(app[0], Some(0), "explorer is an app");
        assert_eq!(app[1], Some(1), "chrome under explorer is its own app");
        assert_eq!((app[2], app[3], app[4]), (Some(1), Some(1), Some(1)));
        assert_eq!(app[5], Some(1), "a second chrome window joins chrome");
        assert_eq!(app[6], None, "the shell keeps no sign-in app");
        assert_eq!(app[7], Some(0), "but its own instances");
        assert_eq!(app[8], None);
        assert_eq!((app[9], app[10]), (Some(9), Some(9)));
        // Not the shell: a windowless child joins its parent's app.
        let app = interactions::app_groups(&processes, &windows, None);
        assert_eq!(app[6], Some(0));
        // "Windows processes" come from the executable's directory.
        let windows_dir = "C:\\Windows";
        assert!(interactions::in_windows_dir(
            "C:\\WINDOWS\\System32\\svchost.exe",
            windows_dir
        ));
        assert!(!interactions::in_windows_dir(
            "C:\\WindowsApps\\x.exe",
            windows_dir
        ));
        assert!(!interactions::in_windows_dir(
            "D:\\Tools\\svchost.exe",
            windows_dir
        ));
    }
    #[test]
    fn grouped_view_lists_collapsible_apps_with_their_totals() {
        let test = TestWindow::new();
        let tree = |pid: u32, parent: u32, name: &str, memory: u64, cpu: f64| Process {
            parent_pid: parent,
            ..process(pid, u64::from(pid), name, memory, cpu)
        };
        test.snapshot(vec![
            tree(20, 0, "chrome.exe", 100, 1.0),
            tree(21, 20, "chrome.exe", 50, 2.0),
            tree(22, 20, "chrome.exe", 25, 0.5),
            tree(30, 0, "idle.exe", 5, 0.0),
            tree(40, 0, "svchost.exe", 7, 0.1),
            // Its path unreadable: started by a Windows process.
            tree(41, 40, "host.exe", 3, 0.0),
        ]);
        unsafe {
            let p = test.p;
            (*p).group_mode = true;
            (*p).window_pids = HashSet::from([20]);
            (*p).last_window_scan = Some(Instant::now());
            // The path test of the classification, known for this fixture.
            for (pid, inside) in [(30u32, Some(false)), (40, Some(true)), (41, None)] {
                (*p).windows_images.insert(
                    ProcessIdentity {
                        pid,
                        created: u64::from(pid),
                    },
                    inside,
                );
            }
            rebuild(p, None);
            let headers: Vec<String> = (0..(*p).rows.len())
                .filter_map(|row| (*p).group_headers.get(&row).cloned())
                .collect();
            assert_eq!(
                headers,
                ["앱 (1)", "백그라운드 프로세스 (1)", "Windows 프로세스 (2)"]
            );
            // Collapsed by default: one app row with its three processes'
            // totals and the child count; no child rows.
            assert_eq!(test.count(), 7);
            assert_eq!(test.text(1, 0), "chrome.exe");
            assert_eq!(test.text(1, 2), "3.5%");
            assert_eq!(test.text(1, 3), "175.0 MB");
            assert_eq!((&(*p).tree_rows)[1].children, 2);
            assert!((&(*p).tree_rows)[1].has_children && !(&(*p).tree_rows)[1].expanded);
            // Right opens it (like a tree parent), children at depth 1.
            test.select(1);
            toggle_selected_branch(p, Some(true));
            assert_eq!(test.count(), 9);
            assert_eq!((&(*p).tree_rows)[2].depth, 1);
            assert_eq!(test.text(2, 3), "50.0 MB", "children show their own values");
            assert_eq!(test.identity(), Some(Identity::Process(20, 20)));
            toggle_selected_branch(p, Some(false));
            assert_eq!(test.count(), 7);
            // A search opens the apps whose processes match.
            test.search("22");
            assert_eq!(test.count(), 3, "Apps, chrome, its matching child");
            assert_eq!(test.text(2, 1), "22");
        }
    }
    /// A flipped startup switch shows the requested state at once and
    /// slides back when the change fails.
    #[test]
    fn startup_switch_is_optimistic_until_the_list_confirms() {
        let Some(entry) = crate::startup::list()
            .unwrap()
            .into_iter()
            .find(|entry| entry.manageable)
        else {
            return;
        };
        let test = TestWindow::new();
        test.page(Page::Startup);
        let _ = test.jobs.recv().unwrap();
        test.result(JobResult::Startup(Ok(vec![entry.clone()])));
        unsafe {
            let p = test.p;
            let switch = table::part_rect((*p).list, 0, table::Part::Switch).unwrap();
            let click = NMITEMACTIVATE {
                iItem: 0,
                iSubItem: 3,
                ptAction: POINT {
                    x: (switch.left + switch.right) / 2,
                    y: (switch.top + switch.bottom) / 2,
                },
                ..zeroed()
            };
            interactions::startup_click(p, &click);
            assert!(matches!(test.jobs.try_recv(), Ok(Job::Action(_))));
            assert_eq!(
                (*p).startup_pending,
                Some((entry.id.clone(), !entry.enabled))
            );
            // The page keeps its look: ⋯ and Settings stay enabled.
            assert_ne!(IsWindowEnabled((*p).more), 0);
            assert_ne!(IsWindowEnabled((*p).settings), 0);
            test.result(JobResult::Action {
                result: Err("Access is denied.".into()),
                page: Page::Startup,
                notice: String::new(),
            });
            assert_eq!((*p).startup_pending, None, "a failure slides it back");
            // A success waits for the reloaded list.
            interactions::startup_click(p, &click);
            let _ = test.jobs.try_recv();
            test.result(JobResult::Action {
                result: Ok(()),
                page: Page::Startup,
                notice: startup_notice(&entry.name, !entry.enabled),
            });
            assert!((*p).startup_pending.is_some());
            let _ = test.jobs.try_recv();
            test.result(JobResult::Startup(Ok(vec![entry.clone()])));
            assert_eq!((*p).startup_pending, None);
        }
        assert!(startup_notice("Spotify", true).contains("Spotify"));
    }
    #[test]
    fn startup_publisher_sort_is_case_insensitive_with_missing_last() {
        let test = TestWindow::new();
        test.page(Page::Startup);
        let _ = test.jobs.recv().unwrap();
        let Some(template) = crate::startup::list().unwrap().into_iter().next() else {
            return;
        };
        let entries: Vec<StartupEntry> = ["a", "b", "c", "d"]
            .iter()
            .map(|id| {
                let mut entry = template.clone();
                entry.id = (*id).into();
                entry.name = format!("App {id}");
                entry
            })
            .collect();
        test.result(JobResult::Startup(Ok(entries)));
        test.result(JobResult::Publishers(std::collections::HashMap::from([
            ("a".to_string(), "wizvera".to_string()),
            ("b".to_string(), "Now.gg".to_string()),
            ("c".to_string(), "WIZVERA inc".to_string()),
        ])));
        test.sort(1);
        let order = |test: &TestWindow| (0..4).map(|r| test.text(r, 1)).collect::<Vec<_>>();
        assert_eq!(order(&test), ["Now.gg", "wizvera", "WIZVERA inc", "—"]);
        test.sort(1);
        assert_eq!(order(&test), ["WIZVERA inc", "wizvera", "Now.gg", "—"]);
        // Startup impact is not measured: its header does not sort.
        let before = order(&test);
        test.sort(2);
        assert_eq!(order(&test), before);
        unsafe {
            assert_eq!((*test.p).sort, 1);
        }
        // The search finds publishers too.
        test.search("now.gg");
        assert_eq!(test.count(), 1);
    }
    #[test]
    fn inline_startup_switch_queues_exact_entry_only_inside_its_hitbox() {
        let Some(entry) = crate::startup::list()
            .unwrap()
            .into_iter()
            .find(|entry| entry.manageable)
        else {
            return;
        };
        let test = TestWindow::new();
        test.page(Page::Startup);
        let _ = test.jobs.recv().unwrap();
        test.result(JobResult::Startup(Ok(vec![entry.clone()])));
        test.select(0);
        unsafe {
            // The 40 × 20 switch, right-aligned in the Enabled cell.
            let mut cell = RECT {
                top: 3,
                left: LVIR_BOUNDS as i32,
                ..zeroed()
            };
            SendMessageW(
                (*test.p).list,
                LVM_GETSUBITEMRECT,
                0,
                &mut cell as *mut _ as isize,
            );
            let switch = table::part_rect((*test.p).list, 0, table::Part::Switch)
                .expect("a manageable entry shows a switch");
            assert_eq!(switch.right, cell.right - scale(test.p, 12));
            assert_eq!(switch.right - switch.left, scale(test.p, 40));
            let mut click = NMITEMACTIVATE {
                iItem: 0,
                iSubItem: 3,
                ptAction: POINT {
                    x: switch.left - scale(test.p, 6),
                    y: (switch.top + switch.bottom) / 2,
                },
                ..zeroed()
            };
            interactions::startup_click(test.p, &click);
            assert!(test.jobs.try_recv().is_err(), "beside the switch");
            click.ptAction.x = (switch.left + switch.right) / 2;
            interactions::startup_click(test.p, &click);
            match test.jobs.try_recv().unwrap() {
                Job::Action(Action::Toggle(captured, enabled)) => {
                    assert_eq!(captured.id, entry.id);
                    assert_eq!(enabled, !entry.enabled);
                }
                _ => panic!("Expected exact startup switch action"),
            }
            interactions::startup_click(test.p, &click);
            assert!(
                test.jobs.try_recv().is_err(),
                "Busy switch cannot duplicate mutation"
            );
        }
    }
    #[test]
    fn settings_page_controls_and_rate_are_connected_without_saving_user_preferences() {
        let test = TestWindow::new();
        test.page(Page::Settings);
        unsafe {
            assert!(!(*test.p).persist_preferences);
            assert_eq!(IsWindowEnabled((*test.p).search), 0);
            assert_ne!(
                GetWindowLongW(GetDlgItem((*test.p).hwnd, PREF_TRAY as i32), GWL_STYLE) as u32
                    & WS_VISIBLE,
                0
            );
            command(test.p, PREF_TRAY, 0);
            assert!((*test.p).prefs.tray);
            command(test.p, PREF_TOP, 0);
            assert!((*test.p).topmost);
            // Always run as administrator: saved at once (not here, where
            // preferences never persist), applied from the next launch; the
            // hidden test window keeps the toast text in the status bar. The
            // test harness is not the installed copy, so the toast says the
            // setting applies to that only.
            assert!(!(*test.p).prefs.always_admin);
            command(test.p, PREF_ADMIN, 0);
            assert!((*test.p).prefs.always_admin);
            if crate::netetw::is_elevated() {
                assert!((&(*test.p).notice).is_empty());
            } else {
                assert_eq!((*test.p).notice, interactions::always_admin_notice());
                assert_eq!(
                    (*test.p).notice,
                    tr(
                        "이 설정은 이 버전과 같은 설치된 Feather에만 적용됩니다",
                        "This applies only to an installed Feather of this same version",
                    )
                );
            }
            (*test.p).notice.clear();
            command(test.p, PREF_ADMIN, 0);
            assert!(!(*test.p).prefs.always_admin);
            assert!((&(*test.p).notice).is_empty(), "no toast when turned off");
            command(test.p, THEME_DARK, 0);
            assert!(colors().dark);
            command(test.p, THEME_LIGHT, 0);
            assert!(!colors().dark);
            SendMessageW(
                GetDlgItem((*test.p).hwnd, PREF_RATE as i32),
                CB_SETCURSEL,
                1,
                0,
            );
            command(test.p, PREF_RATE, CBN_SELCHANGE);
            assert_eq!((*test.p).interval, 250);
            assert_eq!(SendMessageW((*test.p).rate, CB_GETCURSEL, 0, 0), 1);
        }
    }
    #[test]
    fn performance_drag_cancels_and_reorders_without_changing_the_chart() {
        use windows_sys::Win32::System::SystemServices::MK_LBUTTON;
        let test = TestWindow::new();
        test.page(Page::Performance);
        unsafe {
            let p = test.p;
            interactions::refresh_components(p);
            let list = (*p).perf_list;
            assert!(!(*p).persist_preferences);
            (*p).perf_history
                .traces
                .entry(PerfTarget::Cpu)
                .or_default()
                .record(Instant::now(), [12.0, 0.0, 0.0]);
            let cpu = table::part_rect(list, 0, table::Part::Row).unwrap();
            let memory = table::part_rect(list, 1, table::Part::Row).unwrap();
            let at = |x: i32, y: i32| ((x as u16 as u32) | ((y as u16 as u32) << 16)) as isize;
            let start = at(memory.left + 30, (memory.top + memory.bottom) / 2);
            let end = at(cpu.left + 30, cpu.top + 1);
            let original = (*p).perf_targets.clone();
            // An ordinary click does not reorder the list.
            SendMessageW(list, WM_LBUTTONDOWN, MK_LBUTTON as usize, end);
            SendMessageW(list, WM_LBUTTONUP, 0, end);
            assert_eq!((*p).perf_targets, original);
            for cancel in [WM_KEYDOWN, WM_CAPTURECHANGED] {
                SendMessageW(list, WM_LBUTTONDOWN, MK_LBUTTON as usize, start);
                SendMessageW(list, WM_MOUSEMOVE, MK_LBUTTON as usize, end);
                SendMessageW(list, cancel, VK_ESCAPE as usize, 0);
                SendMessageW(list, WM_LBUTTONUP, 0, end);
                assert_ne!(GetCapture(), list);
                assert_eq!((*p).perf_targets, original);
                assert_eq!((*p).perf_target, PerfTarget::Cpu);
            }
            SendMessageW(list, WM_LBUTTONDOWN, MK_LBUTTON as usize, start);
            SendMessageW(list, WM_MOUSEMOVE, MK_LBUTTON as usize, end);
            SendMessageW(list, WM_LBUTTONUP, 0, end);
            assert_eq!((*p).perf_targets, vec![PerfTarget::Memory, PerfTarget::Cpu]);
            assert_eq!((*p).perf_target, PerfTarget::Cpu);
            assert_eq!(SendMessageW(list, LB_GETCURSEL, 0, 0), 1);
            assert_eq!(
                (&(*p).perf_history.traces)[&PerfTarget::Cpu].points.len(),
                1
            );
            interactions::refresh_components(p);
            assert_eq!((*p).perf_targets, vec![PerfTarget::Memory, PerfTarget::Cpu]);
        }
    }
    #[test]
    fn missing_performance_is_a_gap_and_warmup_preserves_device_selection() {
        let test = TestWindow::new();
        test.page(Page::Performance);
        unsafe {
            let mut perf = PerfSampler::new().unwrap().sample().unwrap();
            perf.disks = vec![crate::performance::DiskStats {
                id: "fixture disk".into(),
                read_bytes_per_sec: Some(10.),
                write_bytes_per_sec: Some(20.),
                active_percent: Some(30.),
            }];
            let target = PerfTarget::Disk("fixture disk".into());
            (*test.p).perf_target = target.clone();
            test.snapshots
                .send(MonitorSample {
                    at: Instant::now(),
                    manual_refresh: false,
                    snapshot: Ok(Snapshot {
                        processes: rows(),
                        cpu_percent: 12.,
                        memory_used: 100,
                        memory_total: 400,
                        sample_ms: 1.,
                    }),
                    performance: Some(Ok(perf.clone())),
                    process_gpu: Default::default(),
                    process_network: Default::default(),
                    resource_data: None,
                    resource_files: Default::default(),
                })
                .unwrap();
            drain_snapshot(test.p);
            assert_eq!(
                (&(*test.p).perf_history.traces)[&target]
                    .points
                    .back()
                    .unwrap()
                    .values[0],
                30.
            );
            test.sample(rows(), Instant::now() + Duration::from_millis(250));
            assert!((&(*test.p).perf_history.traces)[&target]
                .points
                .back()
                .unwrap()
                .values[0]
                .is_nan());
            perf.disks.clear();
            (*test.p).performance = Some(Arc::new(perf));
            interactions::refresh_components(test.p);
            assert_eq!((*test.p).perf_target, target);
            assert!((*test.p).perf_targets.contains(&target));
        }
    }
    /// The reported broken chart segments: after Startup apps, Services or
    /// Settings, the Performance charts had a gap, because only Performance
    /// and Processes sampled the counters. Every page records the history
    /// now and Performance shows the current sample at once; a pause (here
    /// minimizing) still marks a gap.
    #[test]
    fn performance_history_continues_on_every_page_and_minimize_still_gaps() {
        let test = TestWindow::new();
        let mut perf = PerfSampler::new().unwrap().sample().unwrap();
        perf.logical_processors = vec![crate::performance::LogicalProcessor {
            id: "fixture cpu".into(),
            group: 0,
            index: 0,
            percent: Some(40.),
        }];
        perf.disks = vec![crate::performance::DiskStats {
            id: "fixture disk".into(),
            read_bytes_per_sec: Some(10.),
            write_bytes_per_sec: Some(20.),
            active_percent: Some(30.),
        }];
        perf.networks.clear();
        perf.gpus.clear();
        // Warm rates, as every sample after the first has (the overview's
        // disk and network history).
        perf.disk_rates_ready = true;
        perf.disk_read_bytes_per_sec = 10.;
        perf.disk_write_bytes_per_sec = 20.;
        perf.network_rates_ready = true;
        perf.network_rx_bytes_per_sec = 1.;
        perf.network_tx_bytes_per_sec = 2.;
        let feed = |at: Instant| {
            test.snapshots
                .send(MonitorSample {
                    at,
                    manual_refresh: false,
                    snapshot: Ok(Snapshot {
                        processes: rows(),
                        cpu_percent: 12.,
                        memory_used: 100,
                        memory_total: 400,
                        sample_ms: 1.,
                    }),
                    performance: Some(Ok(perf.clone())),
                    process_gpu: Default::default(),
                    process_network: Default::default(),
                    resource_data: None,
                    resource_files: Default::default(),
                })
                .unwrap();
            unsafe {
                SendMessageW((*test.p).hwnd, SNAPSHOT_READY, 0, 0);
            }
        };
        // Past timestamps, so the gap recorded at "now" comes after them.
        let start = Instant::now().checked_sub(Duration::from_secs(30)).unwrap();
        let pages = [
            Page::Performance,
            Page::Startup,
            Page::Performance,
            Page::Services,
            Page::Settings,
            Page::Performance,
            Page::Processes,
        ];
        unsafe {
            for (i, page) in pages.into_iter().enumerate() {
                test.page(page);
                assert_eq!((*test.p).page, page);
                // Switching pages never pauses sampling.
                while let Ok(command) = test.commands.try_recv() {
                    assert!(
                        matches!(command, Command::Configure { paused: false, .. }),
                        "{page:?}"
                    );
                }
                if i > 0 {
                    assert!((*test.p).performance.is_some(), "{page:?} kept the sample");
                }
                feed(start + Duration::from_secs(i as u64));
            }
            let history = &(*test.p).perf_history;
            for target in [
                PerfTarget::Cpu,
                PerfTarget::Memory,
                PerfTarget::Disk("fixture disk".into()),
            ] {
                let trace = &history.traces[&target];
                assert_eq!(trace.points.len(), pages.len(), "{target:?}");
                assert!(
                    trace.points.iter().all(|p| p.values[0].is_finite()),
                    "{target:?} has no gap"
                );
            }
            let core = &history.cores["fixture cpu"];
            assert_eq!(core.points.len(), pages.len());
            assert!(core.points.iter().all(|p| p.values[0] == 40.));
            // The overview history kept its disk and network values on the
            // other pages too (they used to be gaps there).
            let overview = &(*test.p).history;
            assert_eq!(overview.len(), pages.len());
            for point in overview {
                assert_eq!((point.disk, point.network), (30., 3.));
            }
            SendMessageW((*test.p).hwnd, WM_SIZE, SIZE_MINIMIZED as usize, 0);
            assert!(matches!(
                test.commands.try_recv().unwrap(),
                Command::Configure { paused: true, .. }
            ));
            let last = |target: &PerfTarget| {
                (&(*test.p).perf_history.traces)[target]
                    .points
                    .back()
                    .unwrap()
                    .values[0]
            };
            assert!(last(&PerfTarget::Cpu).is_nan(), "minimized is a gap");
            assert!((&(*test.p).perf_history.cores)["fixture cpu"]
                .points
                .back()
                .unwrap()
                .values[0]
                .is_nan());
            SendMessageW((*test.p).hwnd, WM_SIZE, SIZE_RESTORED as usize, 0);
            feed(Instant::now());
            assert_eq!(last(&PerfTarget::Cpu), 12.);
        }
    }
    /// The monitor samples the performance counters every interval whatever
    /// the page (the command no longer names one), also after a pause, and a
    /// page switch keeps the sampler warm (its next sample has rates).
    #[test]
    fn monitor_samples_performance_every_interval_and_after_a_pause() {
        // Elevated, the monitor would start the per-process network ETW
        // session; the default test run never starts one.
        if crate::netetw::is_elevated() {
            return;
        }
        let (tx, commands) = mpsc::channel();
        let (snapshots, rx) = mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || monitor(0, commands, snapshots));
        let configure = |paused| {
            tx.send(Command::Configure {
                interval: 250,
                paused,
            })
            .unwrap()
        };
        let next = |label: &str| {
            let sample = rx.recv_timeout(Duration::from_secs(20)).unwrap();
            assert!(sample.snapshot.is_ok(), "{label}");
            assert!(matches!(sample.performance, Some(Ok(_))), "{label}");
            sample
        };
        // A warm sampler reports per-CPU usage; a new one's first sample
        // has no rates at all.
        let warm = |sample: &MonitorSample| matches!(&sample.performance, Some(Ok(s)) if !s.logical_processors.is_empty());
        configure(false);
        let first = next("first sample");
        assert!(!warm(&first), "a new sampler has no rates yet");
        next("second sample");
        let third = next("third sample");
        // Where the per-CPU counters work (not every machine exposes them),
        // a page switch (Configure without a pause) must not restart the
        // sampler: its next sample still has rates.
        let counters_work = warm(&third);
        std::thread::sleep(Duration::from_millis(150));
        configure(false);
        let after_switch = next("after a page switch");
        if counters_work {
            assert!(warm(&after_switch), "the page switch kept the sampler");
        }
        // Page switches, resizes and keystrokes re-send the same
        // configuration: past the "recent sample" window (125 ms here) each
        // one used to sample at once. The schedule stands instead: about 6
        // samples in 1.5 s, not one per Configure (10).
        let mut samples = 0;
        for _ in 0..10 {
            configure(false);
            let until = Instant::now() + Duration::from_millis(150);
            while let Ok(sample) = rx.recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                assert!(sample.snapshot.is_ok());
                samples += 1;
            }
        }
        assert!(
            (4..=7).contains(&samples),
            "{samples} samples for 10 unchanged configurations"
        );
        // A pause drops it (the UI records the gap); sampling resumes with
        // performance data every interval.
        configure(true);
        configure(false);
        for i in 0..2 {
            next(&format!("after the pause {i}"));
        }
        configure(true);
        tx.send(Command::Refresh).unwrap();
        let queued = next("manual refresh while paused");
        let manual = if queued.manual_refresh {
            queued
        } else {
            // An automatic frame can already occupy the bounded slot.
            next("manual refresh after a queued frame")
        };
        assert!(manual.manual_refresh);
        assert!(rx.recv_timeout(Duration::from_millis(350)).is_err());
        tx.send(Command::Stop).unwrap();
        thread.join().unwrap();
    }
    /// Client-relative rectangle of a child control.
    unsafe fn child(p: *mut App, h: HWND) -> RECT {
        let mut r: RECT = zeroed();
        GetWindowRect(h, &mut r);
        MapWindowPoints(
            null_mut(),
            (*p).hwnd,
            (&mut r as *mut RECT).cast::<POINT>(),
            2,
        );
        r
    }
    fn edges(r: RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right, r.bottom)
    }
    unsafe fn shown(h: HWND) -> bool {
        GetWindowLongW(h, GWL_STYLE) as u32 & WS_VISIBLE != 0
    }
    /// Every page against the geometry model (itself pinned to the reference
    /// in `layout::tests`): the rail keeps only navigation, the head's actions
    /// are right-aligned 8 px apart at their heights, the view fills the
    /// content rect, the status bar select sits at the right padding.
    #[test]
    fn controls_follow_the_layout_model_on_every_page() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            fit_client((*p).hwnd, 1200, 820);
            let mut client: RECT = zeroed();
            GetClientRect((*p).hwnd, &mut client);
            assert_eq!((client.right, client.bottom), (1200, 820));
            for page in [
                Page::Processes,
                Page::Performance,
                Page::Startup,
                Page::Services,
                Page::Settings,
            ] {
                test.page(page);
                let l = current_layout(p);
                for i in 0..4 {
                    assert_eq!(edges(child(p, (*p).nav[i])), edges(l.nav[i]));
                }
                assert_eq!(edges(child(p, (*p).settings)), edges(l.nav_settings));
                // Nothing but navigation lives in the rail any more.
                let mut h = GetWindow((*p).hwnd, GW_CHILD);
                while !h.is_null() {
                    let r = child(p, h);
                    if shown(h) && r.right <= l.rail.right && r.top >= l.rail.top {
                        assert!(
                            (*p).nav.contains(&h) || h == (*p).settings,
                            "{page:?}: {} is in the rail",
                            paint::window_text(h)
                        );
                    }
                    h = GetWindow(h, GW_HWNDNEXT);
                }
                for hidden in [(*p).pause, (*p).refresh, (*p).run_task, (*p).top] {
                    assert!(!shown(hidden), "{page:?}: rail command shown");
                }
                // Search input inside the title strip's search box.
                let search = child(p, (*p).search);
                assert_eq!(
                    (search.left, search.right),
                    (l.search_text.left, l.search_text.right)
                );
                assert!(search.top > l.search.top && search.bottom < l.search.bottom);
                assert_eq!(
                    IsWindowEnabled((*p).search) != 0,
                    !matches!(page, Page::Performance | Page::Settings)
                );
                // Status bar Refresh select: 22 px at the right padding.
                let rate = child(p, (*p).rate);
                assert_eq!(rate.right, l.status_inner.right);
                assert_eq!(rate.bottom - rate.top, 22);
                assert_eq!(rate.top, 795);
                // Page head: right-aligned, 8 px apart, 32 px buttons / 28 px
                // selects, vertically centred in the head's content box.
                let controls: Vec<HWND> = head_items(p)
                    .into_iter()
                    .filter_map(|item| match item {
                        HeadItem::Control(h) => Some(h),
                        HeadItem::Note(_) => None,
                    })
                    .collect();
                assert_eq!(controls.is_empty(), page == Page::Settings);
                let mut right = l.head_inner.right;
                for &h in controls.iter().rev() {
                    let r = child(p, h);
                    assert!(shown(h), "{page:?}: head action hidden");
                    assert_eq!(r.right, right, "{page:?}: {}", paint::window_text(h));
                    // 28 px selects; 32 px buttons, but ⋯ matches Startup's
                    // 28 px head.
                    let height = if is_select(p, h) || page == Page::Startup {
                        28
                    } else {
                        32
                    };
                    assert_eq!(r.bottom - r.top, height);
                    assert!(
                        (r.top - l.head_inner.top - (l.head_inner.bottom - r.bottom)).abs() <= 1
                    );
                    right = r.left - 8;
                }
                if page == Page::Processes {
                    assert_eq!(controls.last(), Some(&(*p).more));
                    assert_eq!(child(p, (*p).more).right - child(p, (*p).more).left, 32);
                }
                // The view fills the content rect exactly.
                let list_page = matches!(page, Page::Processes | Page::Startup | Page::Services);
                assert_eq!(shown((*p).list), list_page);
                if list_page {
                    assert_eq!(edges(child(p, (*p).list)), edges(l.content));
                    assert_eq!(
                        table::header_height((*p).list),
                        layout::table_header_height(page, 96)
                    );
                }
                assert_eq!(shown((*p).perf_list), page == Page::Performance);
                if page == Page::Performance {
                    assert_eq!(edges(child(p, (*p).perf_list)), edges(l.perf_device_list()));
                    assert_eq!(SendMessageW((*p).perf_list, LB_GETITEMHEIGHT, 0, 0), 58);
                }
                // Tab order follows the visual order.
                let order = tab_order(p);
                let mut next = GetWindow((*p).hwnd, GW_CHILD);
                for &h in &order {
                    assert_eq!(next, h, "{page:?}: tab order");
                    next = GetWindow(h, GW_HWNDNEXT);
                }
            }
            // Settings: joined rows, controls right-aligned in their rows.
            test.page(Page::Settings);
            let l = current_layout(p);
            let s = l.settings();
            for (id, group, row, inset) in [
                (THEME_SYSTEM, 0, 0, 0),
                (PREF_LANGUAGE, 0, 1, 0),
                (PREF_RATE, 1, 0, 0),
                (PREF_START, 1, 1, 0),
                (PREF_TOP, 2, 0, 4),
                (PREF_TRAY, 2, 1, 4),
                (PREF_REPLACE, 2, 2, 4),
                (PREF_ADMIN, 2, 3, 4),
            ] {
                let h = GetDlgItem((*p).hwnd, id as i32);
                let r = child(p, h);
                let g = &s.groups[group];
                assert!(shown(h));
                assert_eq!(r.right, g.row_content(row, 96).right + inset, "{id}");
                let (row_top, row_bottom) = (g.rows[row].top, g.rows[row].bottom);
                assert!(
                    (r.top - row_top - (row_bottom - r.bottom)).abs() <= 1,
                    "{id}"
                );
            }
            let light = child(p, GetDlgItem((*p).hwnd, THEME_LIGHT as i32));
            let dark = child(p, GetDlgItem((*p).hwnd, THEME_DARK as i32));
            let system = child(p, GetDlgItem((*p).hwnd, THEME_SYSTEM as i32));
            assert_eq!((light.right, dark.right), (dark.left, system.left));
        }
    }
    /// Choosing a view in the page head's select re-lays the head: Expand all
    /// appears 8 px after the select in tree mode (no hole) and disappears
    /// again (no stale button under the select) for the flat/grouped views,
    /// also after a relayout while in tree mode.
    #[test]
    fn view_mode_select_shows_expand_all_only_in_tree_mode() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            fit_client((*p).hwnd, 1200, 820);
            test.page(Page::Processes);
            let choose = |mode: usize| {
                SendMessageW((*p).view_mode, CB_SETCURSEL, mode, 0);
                SendMessageW(
                    (*p).hwnd,
                    WM_COMMAND,
                    VIEW_MODE | (CBN_SELCHANGE as usize) << 16,
                    (*p).view_mode as isize,
                );
            };
            let intersects = |a: RECT, b: RECT| {
                a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
            };
            choose(1);
            assert!((*p).tree_mode);
            assert!(shown((*p).expand_all), "Expand all must show in tree mode");
            let (view, expand, nuclear) = (
                child(p, (*p).view_mode),
                child(p, (*p).expand_all),
                child(p, (*p).nuclear),
            );
            assert_eq!(view.right + 8, expand.left, "no gap before Expand all");
            assert_eq!(expand.right + 8, nuclear.left);
            assert!(tab_order(p).contains(&(*p).expand_all));
            // A relayout in tree mode (resize, page return), then flat again.
            layout(p);
            for mode in [0, 2] {
                choose(mode);
                assert!(!(*p).tree_mode);
                assert!(!shown((*p).expand_all), "mode {mode}: stale Expand all");
                let view = child(p, (*p).view_mode);
                assert_eq!(view.right + 8, child(p, (*p).nuclear).left);
                assert!(!intersects(view, child(p, (*p).expand_all)) || !shown((*p).expand_all));
                assert!(!tab_order(p).contains(&(*p).expand_all));
                choose(1);
                assert!(shown((*p).expand_all));
            }
        }
    }
    /// The nav "current" indicator slides and stretches from the old item to
    /// the new one (180 ms, frame timer only while moving), is painted across
    /// the rail gaps and the nav buttons from the same edges, and jumps on a
    /// relayout when idle.
    #[test]
    fn nav_indicator_slides_between_items_across_the_rail() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            fit_client((*p).hwnd, 1200, 820);
            (*p).anim.anim = anim::Animator::new().with_reduced_motion(false);
            layout(p);
            let top_key = (anim::NAV_ID, anim::part::NAV_TOP);
            let bottom_key = (anim::NAV_ID, anim::part::NAV_BOTTOM);
            let l = current_layout(p);
            let edges_of = |r: RECT| ((r.top + 12) as f32, (r.bottom - 12) as f32);
            let value = move |key| (*p).anim.value(key);
            assert_eq!((value(top_key), value(bottom_key)), edges_of(l.nav[0]));
            assert!(!(*p).anim.is_running(), "a relayout jumps");
            test.page(Page::Services);
            assert!((*p).anim.is_running(), "the slide runs on the frame timer");
            // Replay the slide on a known clock (the page switch took time).
            test.page(Page::Processes);
            (*p).anim.finish_all();
            let t0 = Instant::now();
            (*p).page = Page::Services;
            place_nav_indicator_at(p, &l, true, t0);
            (*p).anim.tick_at(t0 + Duration::from_millis(60));
            let (top, bottom) = (value(top_key), value(bottom_key));
            let (target_top, target_bottom) = edges_of(l.nav[3]);
            assert!(top > edges_of(l.nav[0]).0 && top < target_top, "{top}");
            assert!(
                bottom > top + 16.0,
                "stretches while moving: {top}..{bottom}"
            );
            // Mid-slide paint: the bar shows in the rail gap between items 1
            // and 2 (parent paint) and inside item 2 (its owner draw) at the
            // same edges.
            (*p).anim.tick_at(t0 + Duration::from_millis(40));
            let (top, bottom) = (value(top_key), value(bottom_key));
            let mut frame = gfx::Dib::new(1200, 820).unwrap();
            capture::paint_client_and_children((*p).hwnd, frame.dc()).unwrap();
            let c = colors();
            let rgb = |pixel: u32| {
                let (r, g, b) = (pixel >> 16 & 0xff, pixel >> 8 & 0xff, pixel & 0xff);
                r | g << 8 | b << 16
            };
            let x = l.nav[0].left + 1;
            let gap = l.nav[1].bottom;
            assert!((top as i32) < gap && (bottom as i32) > gap + 2);
            assert_eq!(rgb(frame.pixel(x, gap)), c.fg, "indicator in the rail gap");
            let inside = (top as i32 + 2).max(l.nav[1].top + 2);
            assert_eq!(
                rgb(frame.pixel(x, inside)),
                c.fg,
                "indicator over a nav item"
            );
            assert_ne!(rgb(frame.pixel(x, bottom as i32 + 3)), c.fg);
            // Settles on the new item and stops the timer.
            (*p).anim.tick_at(t0 + Duration::from_secs(1));
            assert_eq!(
                (value(top_key), value(bottom_key)),
                (target_top, target_bottom)
            );
            assert!(!(*p).anim.is_running());
            assert_eq!(
                (*p).anim.value((NAV + 3, anim::part::SELECTED)),
                1.0,
                "the new item's background faded in"
            );
            // Settings sits at the bottom of the rail.
            test.page(Page::Settings);
            (*p).anim.tick_at(Instant::now() + Duration::from_secs(1));
            assert_eq!(
                (value(top_key), value(bottom_key)),
                edges_of(current_layout(p).nav_settings)
            );
            // A resize while idle jumps to the new geometry.
            fit_client((*p).hwnd, 1100, 700);
            assert!(!(*p).anim.is_running());
            assert_eq!(
                (value(top_key), value(bottom_key)),
                edges_of(current_layout(p).nav_settings)
            );
        }
    }
    /// The device-card painter is usable by any list: hover fades fg_soft in,
    /// selection shows fg_sel, the 2 px gap below the card stays surface.
    #[test]
    fn device_cards_paint_hover_and_selection_from_any_list() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            let mut dib = gfx::Dib::new(260, 58).unwrap();
            let c = colors();
            let rgb = |pixel: u32| {
                let (r, g, b) = (pixel >> 16 & 0xff, pixel >> 8 & 0xff, pixel & 0xff);
                r | g << 8 | b << 16
            };
            let item = RECT {
                left: 0,
                top: 0,
                right: 260,
                bottom: 58,
            };
            for (hover, selected, face) in [
                (0.0, false, c.surface),
                (1.0, false, c.fg_soft),
                (0.0, true, c.fg_sel),
                (1.0, true, c.fg_sel),
            ] {
                paint::device_card(p, dib.dc(), item, 1, hover, selected);
                assert_eq!(
                    rgb(dib.pixel(4, 28)),
                    face,
                    "hover {hover} selected {selected}"
                );
                assert_eq!(rgb(dib.pixel(4, 57)), c.surface, "gap below the card");
            }
            // Out-of-range indices paint nothing (no panic).
            paint::device_card(p, dib.dc(), item, 99, 1.0, true);
        }
    }
    /// The telemetry drawer and the service details panel are placed and
    /// painted from `Layout::{drawer, details}`: the list ends where the
    /// drawer's top border starts, the main panel's left border stays visible
    /// along the drawer, and Show / hide leaves no floating drawer button.
    #[test]
    fn drawer_and_details_follow_the_layout_model() {
        let test = TestWindow::new();
        test.snapshot(rows());
        test.select(0);
        unsafe {
            let p = test.p;
            fit_client((*p).hwnd, 1200, 820);
            (*p).show_telemetry = true;
            layout(p);
            let l = current_layout(p);
            let (table, drawer) = l.drawer(true);
            let drawer = drawer.unwrap();
            assert_eq!(edges(child(p, (*p).list)), edges(table));
            let end_tree = child(p, (*p).end_tree);
            assert!(shown((*p).end_tree));
            assert!(end_tree.top > drawer.top && end_tree.bottom < drawer.bottom);
            let mut frame = gfx::Dib::new(1200, 820).unwrap();
            paint::paint_to(p, frame.dc());
            let c = colors();
            let rgb = |pixel: u32| {
                let (r, g, b) = (pixel >> 16 & 0xff, pixel >> 8 & 0xff, pixel & 0xff);
                r | g << 8 | b << 16
            };
            assert_eq!(rgb(frame.pixel(l.main.left, drawer.top + 60)), c.border);
            assert_eq!(rgb(frame.pixel(drawer.left + 4, drawer.top)), c.border);
            assert_eq!(rgb(frame.pixel(drawer.left + 4, drawer.top + 1)), c.surface);
            (*p).show_telemetry = false;
            layout(p);
            assert!(!shown((*p).end_tree));
            assert_eq!(edges(child(p, (*p).list)), edges(l.content));
            test.page(Page::Services);
            (*p).show_details = true;
            layout(p);
            let l = current_layout(p);
            let (table, panel) = l.details(true);
            assert_eq!(edges(child(p, (*p).list)), edges(table));
            paint::paint_to(p, frame.dc());
            let panel = panel.unwrap();
            assert_eq!(rgb(frame.pixel(panel.left, panel.top + 40)), c.border);
            assert_eq!(rgb(frame.pixel(panel.left + 2, panel.bottom - 4)), c.bg);
        }
    }
    /// Commands that left the rail stay reachable: Always on top is a plain
    /// toggle again on every page (it used to double as the rail's "More
    /// actions" button), and each page's ⋯ menu carries its commands.
    #[test]
    fn rail_commands_stay_reachable_from_menus_and_shortcuts() {
        let test = TestWindow::new();
        test.snapshot(rows());
        test.select(0);
        unsafe {
            let p = test.p;
            let before = (*p).topmost;
            command(p, TOP, 0);
            assert_eq!((*p).topmost, !before, "TOP must not open a menu");
            command(p, TOP, 0);
            assert_eq!((*p).topmost, before);
            let has =
                |menu: HMENU, id: usize| GetMenuState(menu, id as u32, MF_BYCOMMAND) != u32::MAX;
            let menu = interactions::build_extra_menu(p, true, None);
            for id in [
                PRIMARY,
                END_TREE,
                SECONDARY,
                EXTRA,
                ELEVATE,
                EXPAND_ALL,
                RUN_TASK,
                RESOURCE_MONITOR,
                REFRESH,
                510,
                511,
                512,
            ] {
                assert!(has(menu, id), "processes menu lacks {id}");
            }
            DestroyMenu(menu);
            test.page(Page::Services);
            let running = service("Alpha", SERVICE_RUNNING, 120, Some(2));
            let menu = interactions::build_extra_menu(p, false, Some(&running));
            for id in [PRIMARY, SECONDARY, EXTRA, 513, 514, REFRESH] {
                assert!(has(menu, id), "services menu lacks {id}");
            }
            assert_eq!(
                GetMenuState(menu, EXTRA as u32, MF_BYCOMMAND) & MF_GRAYED,
                0
            );
            DestroyMenu(menu);
            test.page(Page::Startup);
            let menu = interactions::build_extra_menu(p, false, None);
            for id in [PRIMARY, 515, REFRESH] {
                assert!(has(menu, id), "startup menu lacks {id}");
            }
            // Loading lists grey out Refresh.
            assert_ne!(
                GetMenuState(menu, REFRESH as u32, MF_BYCOMMAND) & MF_GRAYED,
                0
            );
            DestroyMenu(menu);
            assert!(shown((*p).more));
            test.page(Page::Performance);
            assert!(!shown((*p).more));
            for h in [(*p).cores, (*p).copy, (*p).resource_monitor] {
                assert!(shown(h));
            }
        }
    }
    /// Nuclear Zombie took Efficiency mode's place in the Processes head
    /// (Efficiency mode stays in the ⋯ / context menus). Its panel is a
    /// modal loop: Tab / Space toggle an option and Esc closes without
    /// running anything; Enter queues a run on the job worker (here the
    /// test's queue: nothing is trimmed or purged) as a busy action, and a
    /// run whose panel was closed still ends that action when it reports.
    #[test]
    fn nuclear_zombie_replaces_efficiency_mode_and_runs_on_the_worker() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            fit_client((*p).hwnd, 1200, 820);
            test.page(Page::Processes);
            assert!(shown((*p).nuclear));
            assert!(!shown((*p).extra), "Efficiency mode left the head");
            assert!(head_items(p).contains(&HeadItem::Control((*p).nuclear)));
            assert!(tab_order(p).contains(&(*p).nuclear));
            let menu = interactions::build_extra_menu(p, true, None);
            assert_ne!(
                GetMenuState(menu, EXTRA as u32, MF_BYCOMMAND),
                u32::MAX,
                "Toggle efficiency mode stays in the menu"
            );
            DestroyMenu(menu);
            test.page(Page::Services);
            assert!(!shown((*p).nuclear));
            assert!(shown((*p).extra), "Services keeps Restart");
            test.page(Page::Processes);
            let keys = |keys: &[u16]| {
                for &vk in keys {
                    PostMessageW((*p).hwnd, WM_KEYDOWN, vk as usize, 0);
                }
            };
            // Page switches queued list jobs; only cleanup jobs matter here.
            while test.jobs.try_recv().is_ok() {}
            keys(&[VK_TAB, VK_SPACE, VK_ESCAPE]);
            command(p, NUCLEAR, 0);
            assert!(!(*p).modal && !(*p).busy);
            assert!(
                !test
                    .jobs
                    .try_iter()
                    .any(|job| matches!(job, Job::Cleanup(..))),
                "closing runs nothing"
            );
            keys(&[VK_RETURN, VK_ESCAPE]);
            command(p, NUCLEAR, 0);
            let Some(Job::Cleanup(options, events)) = test
                .jobs
                .try_iter()
                .find(|job| matches!(job, Job::Cleanup(..)))
            else {
                panic!("Enter queues a cleanup job");
            };
            assert_eq!(
                options,
                crate::memclean::CleanupOptions {
                    trim: true,
                    standby: true,
                    modified: false,
                    zombies: true
                }
            );
            assert!((*p).busy && !(*p).modal, "the run is an action");
            assert!(IsWindowEnabled((*p).nuclear) == 0);
            events
                .send(nuclear::CleanupEvent::Done(Box::new(
                    crate::memclean::CleanupReport {
                        before: Err("test".into()),
                        after: Err("test".into()),
                        trim: None,
                        modified: None,
                        standby: None,
                        zombies: None,
                        elevated: false,
                    },
                )))
                .unwrap();
            SendMessageW((*p).hwnd, nuclear::CLEANUP_READY, 0, 0);
            assert!(!(*p).busy, "the late report ends the action");
            assert!(IsWindowEnabled((*p).nuclear) != 0);
            // With the panel open, progress and the report arrive through
            // its loop: the results are laid out in the same panel, then Esc
            // closes it (a timer plays the worker inside the loop).
            while test.jobs.try_recv().is_ok() {}
            NUCLEAR_JOBS.with(|slot| {
                *slot.borrow_mut() = Some(std::ptr::addr_of!(test.jobs) as usize);
            });
            keys(&[VK_RETURN]);
            SetTimer((*p).hwnd, 0x5152, 30, Some(feed_nuclear_report));
            command(p, NUCLEAR, 0);
            NUCLEAR_JOBS.with(|slot| slot.borrow_mut().take());
            assert!(
                !(*p).modal && !(*p).busy,
                "the report ended the run in the panel"
            );
            popup::finish_closing();
        }
    }
    thread_local! {
        /// The test window's job queue for [`feed_nuclear_report`].
        static NUCLEAR_JOBS: std::cell::RefCell<Option<usize>> = const { std::cell::RefCell::new(None) };
    }
    /// Inside the panel's loop: take its cleanup job, report progress and a
    /// finished run (real memory readings, one holder), then press Esc.
    unsafe extern "system" fn feed_nuclear_report(hwnd: HWND, _: u32, id: usize, _: u32) {
        KillTimer(hwnd, id);
        let Some(jobs) = NUCLEAR_JOBS.with(|slot| *slot.borrow()) else {
            return;
        };
        let jobs = &*(jobs as *const Receiver<Job>);
        let Some(Job::Cleanup(_, events)) =
            jobs.try_iter().find(|job| matches!(job, Job::Cleanup(..)))
        else {
            panic!("the panel queued no cleanup job");
        };
        let memory = crate::memclean::memory_state();
        let report = crate::memclean::CleanupReport {
            before: memory.clone(),
            after: memory,
            trim: Some(Ok(crate::memclean::TrimReport {
                trimmed: 3,
                skipped: 1,
                failed: 0,
            })),
            modified: None,
            standby: Some(Err(crate::memclean::PurgeError::Declined)),
            zombies: Some(Ok(crate::memclean::ZombieScan {
                holders: vec![crate::memclean::ZombieHolder {
                    pid: 4242,
                    created: 1,
                    name: "holder.exe".into(),
                    zombies: 2,
                    examples: vec![("child.exe".into(), 2)],
                }],
                total: 2,
                inspected: 10,
                uninspected: 1,
                uninspected_handles: 0,
            })),
            elevated: false,
        };
        let progress = crate::memclean::Progress::Scanning;
        events
            .send(nuclear::CleanupEvent::Progress(progress))
            .unwrap();
        events
            .send(nuclear::CleanupEvent::Done(Box::new(report)))
            .unwrap();
        PostMessageW(hwnd, nuclear::CLEANUP_READY, 0, 0);
        PostMessageW(hwnd, WM_KEYDOWN, VK_ESCAPE as usize, 0);
    }
    #[test]
    fn wm_paint_composes_on_the_cached_back_buffer_and_timers_stay_idle() {
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            let mut client: RECT = zeroed();
            GetClientRect((*p).hwnd, &mut client);
            RedrawWindow(
                (*p).hwnd,
                null(),
                null_mut(),
                RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
            );
            // Drive one WM_PAINT explicitly as well (hidden windows may skip it).
            SendMessageW((*p).hwnd, WM_PAINT, 0, 0);
            assert_eq!((*p).back.size(), Some((client.right, client.bottom)));
            let first = (*p).back.dc();
            SendMessageW((*p).hwnd, WM_PAINT, 0, 0);
            assert_eq!((*p).back.dc(), first, "the frame must be reused per size");
            // Hovering a button starts the frame timer; it stops once settled.
            SendMessageW((*p).primary, WM_MOUSEMOVE, 0, 0);
            assert!((*p).anim.is_running() || anim::reduced_motion());
            SendMessageW((*p).primary, WM_MOUSELEAVE, 0, 0);
            (*p).anim.tick_at(Instant::now() + Duration::from_secs(1));
            assert!(!(*p).anim.is_running());
            assert_eq!((*p).anim.value((PRIMARY, anim::part::HOVER)), 0.0);
            assert_eq!(KillTimer((*p).hwnd, anim::ANIM_TIMER_ID), 0);
        }
    }

    /// GetGuiResources counts the whole process and the other tests create
    /// windows, fonts and bitmaps concurrently, so the measurement runs alone
    /// in a child test process (same binary, this test only, one thread).
    #[test]
    fn repainting_does_not_leak_gdi_or_user_objects() {
        const CHILD: &str = "FEATHER_GDI_LEAK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ui::tests::repainting_does_not_leak_gdi_or_user_objects",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{stdout}\n{stderr}");
            assert!(stdout.contains("gdi-leak-check: ok"), "{stdout}\n{stderr}");
            return;
        }
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS,
        };
        let test = TestWindow::new();
        test.snapshot(rows());
        unsafe {
            let p = test.p;
            let mut client: RECT = zeroed();
            GetClientRect((*p).hwnd, &mut client);
            let mut frame = gfx::Dib::new(client.right, client.bottom).unwrap();
            // Every painter path: back-buffered root paint, buffered owner-draw
            // buttons/items, list custom draw, header, select faces.
            let repaint = |frame: &mut gfx::Dib| {
                let mut back = std::mem::take(&mut (*p).back);
                if let Some((dc, _)) = back.prepare(client.right, client.bottom) {
                    paint::paint_to(p, dc);
                }
                (*p).back = back;
                capture::paint_client_and_children((*p).hwnd, frame.dc()).unwrap();
            };
            let pages = [
                Page::Processes,
                Page::Performance,
                Page::Startup,
                Page::Services,
                Page::Settings,
            ];
            for page in pages {
                test.page(page);
                repaint(&mut frame);
            }
            set_tree_mode(p, true);
            repaint(&mut frame);
            set_tree_mode(p, false);
            test.page(Page::Processes);
            repaint(&mut frame);
            // The paths that rebuild GDI resources: theme brushes and DWM
            // frame, the DPI font set / image list / back buffer, the
            // language (labels, fonts, columns; in memory only, never the
            // registry) and the palette window.
            let recreate = |cycle: usize| {
                let even = cycle.is_multiple_of(2);
                (*p).prefs.theme = if even { 2 } else { 1 };
                interactions::apply_theme(p);
                let dpi = if even { 144 } else { 96 };
                let r = RECT {
                    left: 0,
                    top: 0,
                    right: 1200 * dpi / 96,
                    bottom: 820 * dpi / 96,
                };
                SendMessageW(
                    (*p).hwnd,
                    WM_DPICHANGED,
                    (dpi | dpi << 16) as usize,
                    &r as *const _ as isize,
                );
                let language = if even {
                    Language::English
                } else {
                    Language::Korean
                };
                crate::i18n::with_language(language, || refresh_language(p));
                shell::create_palette(p, false);
                assert!(!(*p).palette.is_null());
                shell::destroy_palette(p);
            };
            // Warm the per-size caches (Hangul fallback faces) once per combination.
            for cycle in 0..2 {
                recreate(cycle);
                for page in pages {
                    test.page(page);
                    repaint(&mut frame);
                }
            }
            test.page(Page::Processes);
            repaint(&mut frame);
            let process = GetCurrentProcess();
            let gdi = GetGuiResources(process, GR_GDIOBJECTS);
            let user = GetGuiResources(process, GR_USEROBJECTS);
            let gdiplus = gfx::live_objects();
            for i in 0..200 {
                if i % 40 == 20 {
                    test.page(pages[(i / 40) % pages.len()]);
                }
                if i % 50 == 49 {
                    test.page(Page::Processes);
                }
                if i % 25 == 10 {
                    recreate(i / 25);
                }
                // Hover fades and switch slides exercise the animation driver.
                (*p).anim.set_target(
                    (PRIMARY, anim::part::HOVER),
                    (i % 2) as f32,
                    anim::motion::HOVER_IN,
                    anim::Easing::EaseOut,
                );
                (*p).anim
                    .tick_at(Instant::now() + Duration::from_millis(500));
                // The custom table: wheel glides, row / header hover, the
                // overlay scrollbar and its fades on the table's own driver.
                let list = (*p).list;
                let delta = if i % 3 == 2 { 120i32 } else { -120 };
                SendMessageW(list, WM_MOUSEWHEEL, (delta as u16 as usize) << 16, 0);
                let (x, y) = (40 + (i as isize * 7) % 900, 20 + (i as isize * 13) % 600);
                SendMessageW(list, WM_MOUSEMOVE, 0, y << 16 | x);
                if i % 10 == 5 {
                    SendMessageW(list, WM_KEYDOWN, VK_NEXT as usize, 0);
                }
                if i % 2 == 0 {
                    table::settle(list);
                }
                repaint(&mut frame);
            }
            test.page(Page::Processes);
            repaint(&mut frame);
            // The page switch starts the nav slide; once it settles the
            // window must be idle again.
            (*p).anim.tick_at(Instant::now() + Duration::from_secs(1));
            SendMessageW((*p).list, WM_MOUSELEAVE, 0, 0);
            table::settle((*p).list);
            repaint(&mut frame);
            (*p).anim.tick_at(Instant::now() + Duration::from_secs(2));
            let gdi_after = GetGuiResources(process, GR_GDIOBJECTS);
            let user_after = GetGuiResources(process, GR_USEROBJECTS);
            assert!(
                gdi_after <= gdi + 2,
                "GDI objects grew from {gdi} to {gdi_after} over 200 repaints"
            );
            assert!(
                user_after <= user + 2,
                "USER objects grew from {user} to {user_after} over 200 repaints"
            );
            assert_eq!(gfx::live_objects(), gdiplus, "GDI+ objects leaked");
            assert!(
                !(*p).anim.is_running(),
                "an idle window must not keep a timer"
            );
            assert!(
                !table::is_animating((*p).list),
                "an idle table must not keep a timer"
            );
            println!(
                "gdi-leak-check: ok gdi {gdi}->{gdi_after} user {user}->{user_after} gdiplus {gdiplus}"
            );
        }
    }
}
