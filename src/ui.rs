//! Native virtual tables; collection and management operations stay off the UI thread.
#![allow(clippy::needless_borrow)]
use crate::{
    performance::{PerfSampler, PerfSnapshot},
    sampler::{Process, Sampler, Snapshot},
    services::Service,
    startup::StartupEntry,
};
use std::{
    cmp::Ordering,
    collections::VecDeque,
    mem::{size_of, zeroed},
    ptr::{null, null_mut},
    sync::mpsc::{self, Receiver, Sender, SyncSender},
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
mod capture;
mod icons;
mod paint;
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
const NAV: usize = 300;
const APP_ICON: *const u16 = 101usize as *const u16;
const SIDEBAR: i32 = 196;
const BG: u32 = rgb(245, 247, 250);
const SURFACE: u32 = rgb(255, 255, 255);
const INK: u32 = rgb(23, 35, 58);
const MUTED: u32 = rgb(89, 102, 123);
const BLUE: u32 = rgb(37, 99, 235);
const BORDER: u32 = rgb(220, 227, 237);
const RAIL: u32 = rgb(21, 32, 54);
const DANGER: u32 = rgb(180, 35, 50);
const GREEN: u32 = rgb(21, 122, 82);
const SELECTED: u32 = rgb(234, 241, 255);
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
}
impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Processes => "프로세스",
            Self::Performance => "성능",
            Self::Startup => "시작 앱",
            Self::Services => "서비스",
        }
    }
    fn subtitle(self) -> &'static str {
        match self {
            Self::Processes => "리소스 사용량을 한눈에 확인하고, 필요한 작업에 집중하세요.",
            Self::Performance => "CPU, 메모리, 디스크와 네트워크의 최근 60초를 확인하세요.",
            Self::Startup => "Windows에 로그인할 때 실행되는 데스크톱 앱을 관리하세요.",
            Self::Services => "백그라운드 서비스의 상태를 확인하고 실행을 관리하세요.",
        }
    }
    fn from_index(i: usize) -> Self {
        match i {
            1 => Self::Performance,
            2 => Self::Startup,
            3 => Self::Services,
            _ => Self::Processes,
        }
    }
}
struct HistoryPoint {
    at: Instant,
    cpu: f64,
    memory: f64,
    disk: f64,
    network: f64,
}
enum Command {
    Configure {
        interval: u64,
        paused: bool,
        performance: bool,
    },
    Refresh,
    Stop,
}
struct MonitorSample {
    at: Instant,
    snapshot: Result<Snapshot, String>,
    performance: Option<Result<PerfSnapshot, String>>,
}
enum Action {
    End(u32, u64),
    Reveal(u32, u64),
    Toggle(Box<StartupEntry>, bool),
    Start(String),
    Stop(String),
    Elevate,
    ReplaceTaskManager(bool, Page),
}
enum Job {
    Startup,
    Services,
    Action(Action),
    Stop,
}
enum JobResult {
    Startup(Result<Vec<StartupEntry>, String>),
    Services(Result<Vec<Service>, String>),
    Action {
        result: Result<(), String>,
        page: Page,
        notice: &'static str,
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
    icons: icons::IconCache,
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
    nav: [HWND; 4],
    font: HFONT,
    small: HFONT,
    heading: HFONT,
    metric: HFONT,
    bold: HFONT,
    big_icon: HICON,
    small_icon: HICON,
    row_images: HIMAGELIST,
    bg: HBRUSH,
    surface: HBRUSH,
    dpi: i32,
    page: Page,
    snapshot: Option<Snapshot>,
    performance: Option<PerfSnapshot>,
    performance_error: Option<String>,
    history: VecDeque<HistoryPoint>,
    startup: Vec<StartupEntry>,
    services: Vec<Service>,
    rows: Vec<usize>,
    filter: String,
    sort: usize,
    descending: bool,
    paused: bool,
    minimized: bool,
    topmost: bool,
    updating: bool,
    modal: bool,
    busy: bool,
    interval: u64,
    hover: usize,
    startup_loading: bool,
    services_loading: bool,
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
            icons: icons::IconCache::default(),
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
            nav: [null_mut(); 4],
            font: null_mut(),
            small: null_mut(),
            heading: null_mut(),
            metric: null_mut(),
            bold: null_mut(),
            big_icon: null_mut(),
            small_icon: null_mut(),
            row_images: 0,
            bg: CreateSolidBrush(BG),
            surface: CreateSolidBrush(SURFACE),
            dpi: 96,
            page: Page::Processes,
            snapshot: None,
            performance: None,
            performance_error: None,
            history: VecDeque::with_capacity(120),
            startup: Vec::new(),
            services: Vec::new(),
            rows: Vec::new(),
            filter: String::new(),
            sort: 3,
            descending: true,
            paused: false,
            minimized: false,
            topmost: false,
            updating: false,
            modal: false,
            busy: false,
            interval: 1000,
            hover: 0,
            startup_loading: false,
            services_loading: false,
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
    InitCommonControlsEx(&INITCOMMONCONTROLSEX {
        dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
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
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        init_controls();
        let (snapshots, rx) = mpsc::sync_channel(1);
        let (tx, commands) = mpsc::channel();
        let (jobs, work) = mpsc::channel();
        let (complete, results) = mpsc::channel();
        let p = Box::into_raw(Box::new(App::new(rx, tx, jobs, results)));
        let dpi = GetDpiForSystem().max(96) as i32;
        let class = wide("FeatherTaskManagerWindow");
        let hwnd = create_window(
            p,
            &class,
            (1200 * dpi / 96).min(GetSystemMetrics(SM_CXSCREEN).saturating_sub(32)),
            (820 * dpi / 96).min(GetSystemMetrics(SM_CYSCREEN).saturating_sub(64)),
        );
        if hwnd.is_null() {
            MessageBoxW(
                null_mut(),
                wide("창을 만들지 못했습니다.").as_ptr(),
                wide("Feather Task Manager").as_ptr(),
                MB_ICONERROR,
            );
            dispose(p);
            return;
        }
        let handle = hwnd as usize;
        if let Err(e) = std::thread::Builder::new()
            .name("feather-monitor".into())
            .spawn(move || monitor(handle, commands, snapshots))
        {
            (*p).set_error(
                ErrorSource::General,
                format!("모니터링 작업을 시작할 수 없습니다: {e}"),
            );
        }
        if let Err(e) = std::thread::Builder::new()
            .name("feather-actions".into())
            .spawn(move || job_worker(handle, work, complete))
        {
            (*p).set_error(
                ErrorSource::General,
                format!("관리 작업을 시작할 수 없습니다: {e}"),
            );
        }
        let args: Vec<String> = std::env::args().collect();
        switch_page(p, initial_page(&args));
        ShowWindow(hwnd, SW_SHOW);
        UpdateWindow(hwnd);
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if keyboard(p, &msg) {
                continue;
            }
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        dispose(p);
        UnregisterClassW(class.as_ptr(), GetModuleHandleW(null()));
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
    let mut perf: Option<PerfSampler> = None;
    let mut performance = false;
    let mut interval = 1000;
    let mut paused = false;
    let mut refresh = true;
    loop {
        if refresh {
            if sampler.is_err() {
                sampler = Sampler::new();
            }
            let snapshot = match &mut sampler {
                Ok(s) => s.sample(),
                Err(e) => Err(e.clone()),
            };
            let perf_result = if performance {
                if perf.is_none() {
                    match PerfSampler::new() {
                        Ok(s) => perf = Some(s),
                        Err(e) => {
                            let value = MonitorSample {
                                at: Instant::now(),
                                snapshot,
                                performance: Some(Err(e)),
                            };
                            if snapshots.try_send(value).is_ok() {
                                unsafe {
                                    PostMessageW(hwnd as HWND, SNAPSHOT_READY, 0, 0);
                                }
                            }
                            refresh = false;
                            continue;
                        }
                    }
                }
                perf.as_mut().map(PerfSampler::sample)
            } else {
                None
            };
            if snapshots
                .try_send(MonitorSample {
                    at: Instant::now(),
                    snapshot,
                    performance: perf_result,
                })
                .is_ok()
            {
                unsafe {
                    PostMessageW(hwnd as HWND, SNAPSHOT_READY, 0, 0);
                }
            }
        }
        let result = if paused {
            commands
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            commands.recv_timeout(Duration::from_millis(interval))
        };
        match result {
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Configure {
                interval: i,
                paused: p,
                performance: requested,
            }) => {
                interval = i;
                paused = p;
                if performance != requested || paused {
                    perf = None;
                }
                performance = requested;
                refresh = !paused;
            }
            Ok(Command::Refresh) => refresh = true,
            Err(mpsc::RecvTimeoutError::Timeout) => refresh = true,
        }
    }
}
fn job_worker(hwnd: usize, jobs: Receiver<Job>, complete: Sender<JobResult>) {
    while let Ok(job) = jobs.recv() {
        let result = match job {
            Job::Stop => break,
            Job::Startup => JobResult::Startup(crate::startup::list()),
            Job::Services => JobResult::Services(crate::services::list()),
            Job::Action(action) => {
                let (result, page, notice) = match action {
                    Action::End(pid, created) => (
                        crate::actions::terminate(pid, created),
                        Page::Processes,
                        "종료 요청을 보냈습니다.",
                    ),
                    Action::Reveal(pid, created) => (
                        crate::actions::reveal_executable(pid, created),
                        Page::Processes,
                        "파일 위치를 열었습니다.",
                    ),
                    Action::Toggle(entry, enabled) => (
                        crate::startup::set_enabled(&entry, enabled),
                        Page::Startup,
                        "시작 앱 설정을 변경했습니다.",
                    ),
                    Action::Start(name) => (
                        crate::services::start(&name),
                        Page::Services,
                        "서비스 시작 요청을 보냈습니다.",
                    ),
                    Action::Stop(name) => (
                        crate::services::stop(&name),
                        Page::Services,
                        "서비스 중지 요청을 보냈습니다.",
                    ),
                    Action::Elevate => (
                        crate::actions::relaunch_elevated(),
                        Page::Processes,
                        "관리자 권한 창을 열었습니다.",
                    ),
                    Action::ReplaceTaskManager(enable, page) => (
                        crate::replacement::run_elevated(enable),
                        page,
                        if enable {
                            "Feather를 Windows 작업 관리자로 설정했습니다. 다음 실행부터 적용됩니다."
                        } else {
                            "Windows 기본 작업 관리자로 복원했습니다."
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
    let app = Box::from_raw(p);
    let _ = app.tx.send(Command::Stop);
    let _ = app.jobs.send(Job::Stop);
    for icon in [app.big_icon, app.small_icon] {
        if !icon.is_null() {
            DestroyIcon(icon);
        }
    }
    for object in [
        app.font,
        app.small,
        app.heading,
        app.metric,
        app.bold,
        app.bg,
        app.surface,
    ] {
        if !object.is_null() {
            DeleteObject(object);
        }
    }
    if app.row_images != 0 {
        ImageList_Destroy(app.row_images);
    }
}
unsafe fn keyboard(p: *mut App, msg: &MSG) -> bool {
    if msg.message != WM_KEYDOWN {
        return false;
    }
    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
    let key = msg.wParam as u16;
    let id = if ctrl && (b'1' as u16..=b'4' as u16).contains(&key) {
        NAV + (key - b'1' as u16) as usize
    } else if ctrl && key == b'F' as u16 && (*p).page != Page::Performance {
        SetFocus((*p).search);
        SendMessageW((*p).search, EM_SETSEL, 0, -1);
        return true;
    } else if key == VK_F5 {
        REFRESH
    } else if key == VK_ESCAPE && msg.hwnd == (*p).search {
        SetWindowTextW((*p).search, wide("").as_ptr());
        return true;
    } else if key == VK_DELETE && msg.hwnd == (*p).list && (*p).page == Page::Processes {
        PRIMARY
    } else if key == VK_SPACE && msg.hwnd == (*p).list {
        PAUSE
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
    match msg {
        WM_CREATE => {
            (*p).dpi = GetDpiForWindow(hwnd).max(96) as i32;
            update_window_icons(p);
            create_controls(p);
            0
        }
        WM_SIZE => {
            let minimized = w == SIZE_MINIMIZED as usize;
            if minimized != (*p).minimized {
                (*p).minimized = minimized;
                configure(p);
            }
            if !minimized {
                layout(p);
            }
            0
        }
        WM_GETMINMAXINFO => {
            let m = &mut *(l as *mut MINMAXINFO);
            m.ptMinTrackSize.x = scale(p, 980);
            m.ptMinTrackSize.y = scale(p, 660);
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
            resize_columns(p);
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
        WM_DRAWITEM => {
            paint::draw_button(p, &*(l as *const DRAWITEMSTRUCT));
            1
        }
        WM_PAINT => {
            paint::paint(p);
            0
        }
        WM_PRINTCLIENT => {
            paint::paint_to(p, w as HDC);
            0
        }
        WM_ERASEBKGND => 1,
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            SetBkColor(w as HDC, SURFACE);
            SetTextColor(w as HDC, INK);
            (*p).surface as isize
        }
        WM_CTLCOLORSTATIC => {
            SetBkColor(w as HDC, BG);
            SetTextColor(w as HDC, INK);
            (*p).bg as isize
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
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
            InvalidateRect(hwnd, null(), 0);
        }
        WM_MOUSELEAVE => {
            if (*p).hover == id {
                (*p).hover = 0;
            }
            InvalidateRect(hwnd, null(), 0);
        }
        WM_SETFOCUS | WM_KILLFOCUS | WM_ENABLE => {
            InvalidateRect(hwnd, null(), 0);
        }
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
    (*p).pause = button(p, "일시정지", PAUSE);
    (*p).rate = create_control(
        p,
        "ComboBox",
        "갱신 간격",
        WS_TABSTOP | CBS_DROPDOWNLIST as u32 | WS_VSCROLL,
        RATE,
    );
    for label in ["0.5초", "1초", "2초", "5초"] {
        SendMessageW((*p).rate, CB_ADDSTRING, 0, wide(label).as_ptr() as isize);
    }
    SendMessageW((*p).rate, CB_SETCURSEL, 1, 0);
    (*p).top = button(p, "항상 위에 표시", TOP);
    (*p).settings = button(p, "설정", SETTINGS);
    (*p).primary = button(p, "작업 끝내기", PRIMARY);
    (*p).secondary = button(p, "파일 위치 열기", SECONDARY);
    (*p).refresh = button(p, "새로고침", REFRESH);
    (*p).list = create_control(
        p,
        "SysListView32",
        "프로세스 목록",
        WS_TABSTOP
            | LVS_REPORT
            | LVS_OWNERDATA
            | LVS_SINGLESEL
            | LVS_SHOWSELALWAYS
            | LVS_SHAREIMAGELISTS,
        200,
    );
    SendMessageW(
        (*p).list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        0,
        (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER) as isize,
    );
    SendMessageW((*p).list, LVM_SETBKCOLOR, 0, SURFACE as isize);
    SendMessageW((*p).list, LVM_SETTEXTBKCOLOR, 0, SURFACE as isize);
    SendMessageW((*p).list, LVM_SETTEXTCOLOR, 0, INK as isize);
    SendMessageW((*p).list, CCM_SETUNICODEFORMAT, 1, 0);
    SetWindowTheme((*p).list, wide("Explorer").as_ptr(), null());
    create_fonts(p);
    setup_columns(p);
    update_buttons(p);
    layout(p);
}
unsafe fn create_fonts(p: *mut App) {
    let old = [(*p).font, (*p).small, (*p).heading, (*p).metric, (*p).bold];
    let dpi = (*p).dpi;
    let make = |pixels: i32, weight: i32, family: &str| {
        CreateFontW(
            -pixels * dpi / 96,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            DEFAULT_PITCH as u32,
            wide(family).as_ptr(),
        )
    };
    (*p).font = make(14, 400, "Malgun Gothic");
    (*p).small = make(12, 400, "Malgun Gothic");
    (*p).heading = make(28, 700, "Malgun Gothic");
    (*p).metric = make(28, 600, "Segoe UI");
    (*p).bold = make(14, 700, "Malgun Gothic");
    for h in [
        (*p).list,
        (*p).search,
        (*p).pause,
        (*p).rate,
        (*p).top,
        (*p).settings,
        (*p).primary,
        (*p).secondary,
        (*p).refresh,
    ]
    .into_iter()
    .chain((*p).nav)
    {
        SendMessageW(h, WM_SETFONT, (*p).font as usize, 1);
    }
    let images = ImageList_Create(1, scale(p, 32), ILC_COLOR32, 1, 0);
    SendMessageW((*p).list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, images);
    if (*p).row_images != 0 {
        ImageList_Destroy((*p).row_images);
    }
    (*p).row_images = images;
    for obj in old {
        if !obj.is_null() {
            DeleteObject(obj);
        }
    }
}
fn columns(page: Page) -> &'static [(&'static str, i32, bool)] {
    match page {
        Page::Processes => &[
            ("프로세스", 250, false),
            ("PID", 76, true),
            ("CPU", 80, true),
            ("메모리", 112, true),
            ("전용 메모리", 120, true),
            ("I/O / 초", 110, true),
            ("스레드", 70, true),
            ("핸들", 70, true),
        ],
        Page::Startup => &[
            ("앱 이름", 230, false),
            ("상태", 180, false),
            ("위치", 200, false),
            ("실행 명령", 420, false),
        ],
        Page::Services => &[
            ("서비스 이름", 220, false),
            ("표시 이름", 330, false),
            ("상태", 120, false),
            ("PID", 80, true),
            ("시작 유형", 120, false),
        ],
        Page::Performance => &[],
    }
}
unsafe fn setup_columns(p: *mut App) {
    while SendMessageW((*p).list, LVM_DELETECOLUMN, 0, 0) != 0 {}
    for (i, &(name, width, numeric)) in columns((*p).page).iter().enumerate() {
        let mut text = wide(name);
        let column = LVCOLUMNW {
            mask: LVCF_TEXT | LVCF_WIDTH | LVCF_FMT | LVCF_SUBITEM,
            fmt: if numeric { LVCFMT_RIGHT } else { LVCFMT_LEFT },
            cx: scale(p, width),
            pszText: text.as_mut_ptr(),
            iSubItem: i as i32,
            ..zeroed()
        };
        SendMessageW(
            (*p).list,
            LVM_INSERTCOLUMNW,
            i,
            &column as *const _ as isize,
        );
    }
    let cue = match (*p).page {
        Page::Processes => "이름 또는 PID 검색 · Ctrl+F",
        Page::Startup => "앱 이름 또는 실행 명령 검색",
        Page::Services => "서비스 이름, 표시 이름 또는 PID 검색",
        Page::Performance => "",
    };
    SendMessageW((*p).search, EM_SETCUEBANNER, 1, wide(cue).as_ptr() as isize);
    SetWindowTextW(
        (*p).list,
        wide(&format!("{} 목록", (*p).page.title())).as_ptr(),
    );
    update_sort_header(p);
}
unsafe fn resize_columns(p: *mut App) {
    for (i, &(_, width, _)) in columns((*p).page).iter().enumerate() {
        SendMessageW((*p).list, LVM_SETCOLUMNWIDTH, i, scale(p, width) as isize);
    }
}
unsafe fn layout(p: *mut App) {
    let mut r: RECT = zeroed();
    GetClientRect((*p).hwnd, &mut r);
    let s = |v| scale(p, v);
    let mv = |h, x, y, w: i32, ht: i32| {
        MoveWindow(h, x, y, w.max(1), ht.max(1), 1);
    };
    for i in 0..4 {
        mv((*p).nav[i], s(12), s(126 + 52 * i as i32), s(172), s(44));
    }
    mv((*p).top, s(12), r.bottom - s(134), s(172), s(36));
    mv((*p).settings, s(12), r.bottom - s(90), s(172), s(36));
    let x = s(220);
    mv((*p).search, x + s(42), s(215), r.right - x - s(356), s(26));
    mv((*p).rate, r.right - s(280), s(213), s(68), s(220));
    mv((*p).pause, r.right - s(200), s(209), s(84), s(36));
    mv((*p).refresh, r.right - s(104), s(209), s(80), s(36));
    mv((*p).list, x, s(267), r.right - x - s(24), r.bottom - s(367));
    mv(
        (*p).primary,
        r.right - s(144),
        r.bottom - s(75),
        s(120),
        s(36),
    );
    mv(
        (*p).secondary,
        r.right - s(292),
        r.bottom - s(75),
        s(136),
        s(36),
    );
    let table = (*p).page != Page::Performance;
    for h in [(*p).list, (*p).search, (*p).primary] {
        ShowWindow(h, if table { SW_SHOW } else { SW_HIDE });
    }
    ShowWindow(
        (*p).secondary,
        if matches!((*p).page, Page::Processes | Page::Services) {
            SW_SHOW
        } else {
            SW_HIDE
        },
    );
    redraw(p);
}
unsafe fn configure(p: *mut App) {
    let _ = (*p).tx.send(Command::Configure {
        interval: (*p).interval,
        paused: (*p).paused || (*p).minimized || (*p).modal,
        performance: (*p).page == Page::Performance,
    });
}
unsafe fn request_list(p: *mut App, page: Page) {
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
                "백그라운드 작업을 실행할 수 없습니다.".into(),
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
    (*p).updating = true;
    SendMessageW((*p).list, LVM_SETITEMCOUNT, 0, 0);
    (*p).rows.clear();
    (*p).page = page;
    (*p).filter.clear();
    (*p).clear_error();
    (*p).notice.clear();
    (*p).sort = if page == Page::Processes { 3 } else { 0 };
    (*p).descending = page == Page::Processes;
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
        Page::Performance => {
            (*p).performance = None;
            (*p).performance_error = None;
        }
        _ => {}
    }
    configure(p);
    redraw(p);
}
unsafe fn command(p: *mut App, id: usize, notification: u32) {
    if (NAV..NAV + 4).contains(&id) {
        switch_page(p, Page::from_index(id - NAV));
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
            let i = SendMessageW((*p).rate, CB_GETCURSEL, 0, 0) as usize;
            (*p).interval = [500, 1000, 2000, 5000].get(i).copied().unwrap_or(1000);
            configure(p);
            redraw(p);
        }
        PAUSE => {
            (*p).paused = !(*p).paused;
            SetWindowTextW(
                (*p).pause,
                wide(if (*p).paused {
                    "계속"
                } else {
                    "일시정지"
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
            let _ = (*p).tx.send(Command::Refresh);
            request_list(p, (*p).page);
            redraw(p);
        }
        PRIMARY if IsWindowEnabled((*p).primary) != 0 => primary_action(p),
        SECONDARY if IsWindowEnabled((*p).secondary) != 0 => secondary_action(p),
        SETTINGS if !(*p).busy && !(*p).modal => settings_menu(p),
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
        Ok(Status::Inactive) => ("작업 관리자 · Windows 기본값", true, false),
        Ok(Status::Active) => ("작업 관리자 · Feather 사용 중", false, true),
        Ok(Status::Other(_)) => ("작업 관리자 · 다른 설정 사용 중", false, false),
        Err(_) => ("작업 관리자 · 상태 확인 실패", false, false),
    };
    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(label).as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    for (id, text, enabled) in [
        (
            REPLACE_TASK_MANAGER,
            "Feather를 작업 관리자로 설정…",
            can_replace,
        ),
        (
            RESTORE_TASK_MANAGER,
            "Windows 기본 작업 관리자로 복원…",
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
    AppendMenuW(menu, MF_STRING, ELEVATE, wide("관리자로 실행").as_ptr());
    menu
}
unsafe fn settings_menu(p: *mut App) {
    let status = crate::replacement::status();
    match &status {
        Err(error) => (*p).set_error(ErrorSource::Action, error.clone()),
        Ok(crate::replacement::Status::Other(value)) => {
            (*p).set_error(
                ErrorSource::Action,
                format!("기존 작업 관리자 연결을 먼저 해제하세요. 현재 설정: {value}"),
            );
        }
        _ => {}
    }
    let menu = create_settings_menu(&status);
    if menu.is_null() {
        (*p).set_error(ErrorSource::Action, "설정 메뉴를 열지 못했습니다.".into());
        redraw(p);
        return;
    }
    (*p).modal = true;
    update_buttons(p);
    configure(p);
    let mut bounds: RECT = zeroed();
    GetWindowRect((*p).settings, &mut bounds);
    let command = TrackPopupMenuEx(
        menu,
        TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTALIGN | TPM_BOTTOMALIGN,
        bounds.left,
        bounds.top,
        (*p).hwnd,
        null(),
    ) as usize;
    DestroyMenu(menu);
    (*p).modal = false;
    configure(p);
    PostMessageW((*p).hwnd, SNAPSHOT_READY, 0, 0);
    PostMessageW((*p).hwnd, JOB_READY, 0, 0);
    update_buttons(p);
    redraw(p);
    match command {
        ELEVATE => begin_action(p, Action::Elevate),
        REPLACE_TASK_MANAGER | RESTORE_TASK_MANAGER => {
            let enable = command == REPLACE_TASK_MANAGER;
            let (title, prompt) = if enable {
                ("Windows 작업 관리자 대체", concat!(
                    "Feather를 Windows 작업 관리자로 설정할까요?\n\n",
                    "Program Files의 Feather Task Manager 폴더에 앱을 설치하고, 이 PC의 모든 사용자에게 적용합니다. ",
                    "Ctrl+Alt+Delete → 작업 관리자와 Ctrl+Shift+Esc로 Feather가 열립니다.\n\n",
                    "관리자 권한이 필요합니다. 설치된 파일을 삭제하기 전에 설정에서 Windows 기본 작업 관리자로 복원하세요."
                ))
            } else {
                ("Windows 작업 관리자 복원", concat!(
                    "Windows 기본 작업 관리자로 되돌릴까요?\n\n",
                    "관리자 권한이 필요하며 이 PC의 모든 사용자에게 적용합니다. Feather 앱은 설치된 폴더에 그대로 남습니다."
                ))
            };
            if confirm(p, title, prompt) {
                begin_action(p, Action::ReplaceTaskManager(enable, (*p).page));
            }
        }
        _ => {}
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
        Page::Performance => None,
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
        Page::Performance => 0,
    }
}
unsafe fn selected_detail(p: *mut App) -> String {
    if (*p).busy {
        return "요청을 처리하는 중…".into();
    }
    if let Some(e) = &(*p).error {
        return e.clone();
    }
    if !(&(*p).notice).is_empty() {
        return (*p).notice.clone();
    }
    let Some(row) = selected_row(p) else {
        return match (*p).page {
            Page::Startup => "로그인 시작 앱 · 일부 앱은 Windows 설정에서 관리합니다.".into(),
            Page::Services => "서비스를 선택하면 시작 또는 중지할 수 있습니다.".into(),
            _ => "작업을 선택하면 파일 위치를 열거나 종료할 수 있습니다.".into(),
        };
    };
    match (*p).page {
        Page::Processes => selected_process(p)
            .map(|s| format!("{}  ·  PID {}  ·  부모 PID {}", s.name, s.pid, s.parent_pid))
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
        Page::Performance => String::new(),
    }
}
unsafe fn update_buttons(p: *mut App) {
    let ready = !(*p).busy && !(*p).modal;
    let mut primary = false;
    let mut secondary = false;
    let mut label = "작업 끝내기";
    let mut second = "파일 위치 열기";
    match (*p).page {
        Page::Processes => {
            if let Some(s) = selected_process(p) {
                primary = s.pid > 4 && s.pid != std::process::id() && s.created != 0;
                secondary = s.pid > 4 && s.created != 0;
            }
        }
        Page::Startup => {
            label = "사용 안 함";
            if let Some(s) = selected_row(p).and_then(|r| (&(*p).startup).get(r)) {
                primary = s.manageable && !(*p).startup_loading;
                if !s.enabled {
                    label = "사용";
                }
            }
        }
        Page::Services => {
            label = "시작";
            second = "중지";
            if let Some(s) = selected_row(p).and_then(|r| (&(*p).services).get(r)) {
                primary =
                    s.state == SERVICE_STOPPED && s.start_type != Some(4) && !(*p).services_loading;
                secondary = s.state == SERVICE_RUNNING && !(*p).services_loading;
            }
        }
        Page::Performance => {}
    }
    SetWindowTextW((*p).primary, wide(label).as_ptr());
    SetWindowTextW((*p).secondary, wide(second).as_ptr());
    EnableWindow((*p).primary, (ready && primary) as i32);
    EnableWindow((*p).secondary, (ready && secondary) as i32);
    EnableWindow((*p).settings, ready as i32);
    let loading = ((*p).page == Page::Startup && (*p).startup_loading)
        || ((*p).page == Page::Services && (*p).services_loading);
    EnableWindow((*p).refresh, (!loading && !(*p).modal) as i32);
}
unsafe fn confirm(p: *mut App, title: &str, prompt: &str) -> bool {
    (*p).modal = true;
    update_buttons(p);
    configure(p);
    let answer = MessageBoxW(
        (*p).hwnd,
        wide(prompt).as_ptr(),
        wide(title).as_ptr(),
        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
    );
    (*p).modal = false;
    configure(p);
    PostMessageW((*p).hwnd, SNAPSHOT_READY, 0, 0);
    PostMessageW((*p).hwnd, JOB_READY, 0, 0);
    update_buttons(p);
    answer == IDYES
}
unsafe fn primary_action(p: *mut App) {
    match (*p).page {
        Page::Processes => {
            if let Some(s) = selected_process(p) {
                if confirm(p,"작업 끝내기",&format!("{} (PID {}) 프로세스를 종료할까요?\n\n저장하지 않은 작업은 사라질 수 있습니다.",s.name,s.pid)){begin_action(p,Action::End(s.pid,s.created));}
            }
        }
        Page::Startup => {
            if let Some(s) = selected_row(p)
                .and_then(|r| (&(*p).startup).get(r))
                .cloned()
            {
                let enabled = !s.enabled;
                begin_action(p, Action::Toggle(Box::new(s), enabled));
            }
        }
        Page::Services => {
            if let Some(s) = selected_row(p)
                .and_then(|r| (&(*p).services).get(r))
                .cloned()
            {
                begin_action(p, Action::Start(s.name));
            }
        }
        Page::Performance => {}
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
                if confirm(p,"서비스 중지",&format!("{} 서비스를 중지할까요?\n\n이 서비스를 사용하는 Windows 기능이나 앱에 영향을 줄 수 있습니다.\n서비스 이름: {}",s.display_name,s.name)){begin_action(p,Action::Stop(s.name));}
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
            "관리 작업을 실행할 수 없습니다.".into(),
        );
    }
    update_buttons(p);
    redraw(p);
}
unsafe fn drain_snapshot(p: *mut App) {
    if (*p).modal {
        return;
    }
    let Ok(sample) = (*p).rx.try_recv() else {
        return;
    };
    let identity = selected_identity(p);
    if let Some(performance) = sample.performance {
        match performance {
            Ok(s) => {
                (*p).performance = Some(s);
                (*p).performance_error = None;
            }
            Err(e) => {
                (*p).performance = None;
                (*p).performance_error = Some(e);
            }
        }
    }
    match sample.snapshot {
        Ok(snapshot) => {
            (*p).recover_error(ErrorSource::Monitor);
            let memory = if snapshot.memory_total == 0 {
                f64::NAN
            } else {
                snapshot.memory_used as f64 / snapshot.memory_total as f64 * 100.0
            };
            let (disk, network) = if (*p).page == Page::Performance {
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
            (*p).snapshot = Some(snapshot);
            if (*p).page == Page::Processes {
                rebuild(p, identity);
            }
        }
        Err(e) => (*p).set_error(ErrorSource::Monitor, e),
    }
    if (*p).page == Page::Services
        && !(*p).paused
        && !(*p).minimized
        && !(*p).busy
        && (*p).last_services.elapsed() >= Duration::from_secs(5)
    {
        request_list(p, Page::Services);
    }
    redraw(p);
}
unsafe fn drain_jobs(p: *mut App) {
    if (*p).modal {
        return;
    }
    while let Ok(result) = (*p).results.try_recv() {
        let identity = selected_identity(p);
        match result {
            JobResult::Startup(result) => {
                (*p).startup_loading = false;
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
            JobResult::Services(result) => {
                (*p).services_loading = false;
                match result {
                    Ok(s) => {
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
                        (*p).notice = notice.into();
                        (*p).clear_error();
                        request_list(p, page);
                        let _ = (*p).tx.send(Command::Refresh);
                    }
                    Err(e) => (*p).set_error(ErrorSource::Action, e),
                }
            }
        }
    }
    update_buttons(p);
    InvalidateRect((*p).list, null(), 0);
    redraw(p);
}
fn compare(a: &Process, b: &Process, col: usize) -> Ordering {
    match col {
        0 => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        1 => a.pid.cmp(&b.pid),
        2 => a.cpu_percent.total_cmp(&b.cpu_percent),
        3 => a.working_set.cmp(&b.working_set),
        4 => a.private_bytes.cmp(&b.private_bytes),
        5 => a.io_bytes_per_sec.total_cmp(&b.io_bytes_per_sec),
        6 => a.threads.cmp(&b.threads),
        7 => a.handles.cmp(&b.handles),
        _ => Ordering::Equal,
    }
}
unsafe fn rebuild(p: *mut App, identity: Option<Identity>) {
    (*p).updating = true;
    let filter = &(*p).filter;
    (*p).rows = (0..total_rows(p))
        .filter(|&row| {
            if filter.is_empty() {
                return true;
            }
            match (*p).page {
                Page::Processes => (*p)
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.processes.get(row))
                    .is_some_and(|s| {
                        s.name.to_lowercase().contains(filter) || s.pid.to_string().contains(filter)
                    }),
                Page::Startup => (&(*p).startup).get(row).is_some_and(|s| {
                    s.name.to_lowercase().contains(filter)
                        || s.command.to_lowercase().contains(filter)
                        || s.location.to_lowercase().contains(filter)
                }),
                Page::Services => (&(*p).services).get(row).is_some_and(|s| {
                    s.name.to_lowercase().contains(filter)
                        || s.display_name.to_lowercase().contains(filter)
                        || s.pid.to_string().contains(filter)
                }),
                Page::Performance => false,
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
                    1 => a.status.cmp(&b.status),
                    2 => a.location.cmp(&b.location),
                    3 => a.command.cmp(&b.command),
                    _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                }
            }
            Page::Services => {
                let a = &(&(*p).services)[a];
                let b = &(&(*p).services)[b];
                match col {
                    1 => a
                        .display_name
                        .to_lowercase()
                        .cmp(&b.display_name.to_lowercase()),
                    2 => a.state.cmp(&b.state),
                    3 => a.pid.cmp(&b.pid),
                    4 => a.start_type.cmp(&b.start_type),
                    _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                }
            }
            Page::Performance => Ordering::Equal,
        };
        (if desc { result.reverse() } else { result }).then(a.cmp(&b))
    });
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
    update_buttons(p);
    InvalidateRect((*p).list, null(), 0);
}
unsafe fn update_sort_header(p: *mut App) {
    let header = SendMessageW((*p).list, LVM_GETHEADER, 0, 0) as HWND;
    for i in 0..columns((*p).page).len() {
        let mut h = HDITEMW {
            mask: HDI_FORMAT,
            ..zeroed()
        };
        SendMessageW(header, HDM_GETITEMW, i, &mut h as *mut _ as isize);
        h.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
        if i == (*p).sort {
            h.fmt |= if (*p).descending {
                HDF_SORTDOWN
            } else {
                HDF_SORTUP
            };
        }
        SendMessageW(header, HDM_SETITEMW, i, &h as *const _ as isize);
    }
}
unsafe fn empty_message(p: *mut App) -> String {
    if let Some(e) = &(*p).error {
        return format!("목록을 불러오지 못했습니다.\n{e}\n새로고침으로 다시 시도하세요.");
    }
    if ((*p).page == Page::Startup && (*p).startup_loading)
        || ((*p).page == Page::Services && (*p).services_loading)
        || ((*p).page == Page::Processes && (*p).snapshot.is_none())
    {
        return "목록을 불러오는 중…".into();
    }
    if !(&(*p).filter).is_empty() {
        "검색 결과가 없습니다.\n다른 이름이나 PID로 검색해 보세요.".into()
    } else if (*p).page == Page::Startup {
        "관리할 데스크톱 시작 앱이 없습니다.\n일부 앱은 Windows 설정에서 관리할 수 있습니다.".into()
    } else {
        "표시할 항목이 없습니다.".into()
    }
}
unsafe fn notify(p: *mut App, l: LPARAM) -> LRESULT {
    let hdr = &*(l as *const NMHDR);
    if hdr.hwndFrom != (*p).list {
        return 0;
    }
    match hdr.code {
        LVN_GETDISPINFOW => {
            let info = &mut *(l as *mut NMLVDISPINFOW);
            if info.item.mask & LVIF_TEXT != 0
                && info.item.iItem >= 0
                && !info.item.pszText.is_null()
                && info.item.cchTextMax > 0
            {
                let value = (&(*p).rows)
                    .get(info.item.iItem as usize)
                    .map(|&r| cell_at(p, r, info.item.iSubItem))
                    .unwrap_or_default();
                let mut n = 0;
                for ch in value.encode_utf16().take(info.item.cchTextMax as usize - 1) {
                    *info.item.pszText.add(n) = ch;
                    n += 1;
                }
                *info.item.pszText.add(n) = 0;
            }
            0
        }
        LVN_GETEMPTYMARKUP => {
            let empty = &mut *(l as *mut NMLVEMPTYMARKUP);
            empty.dwFlags = EMF_CENTERED;
            let value = empty_message(p);
            let cap = empty.szMarkup.len() - 1;
            for (i, ch) in value.encode_utf16().take(cap).enumerate() {
                empty.szMarkup[i] = ch;
                empty.szMarkup[i + 1] = 0;
            }
            1
        }
        LVN_COLUMNCLICK => {
            let col = (*(l as *const NMLISTVIEW)).iSubItem;
            if col < 0 || col as usize >= columns((*p).page).len() {
                return 0;
            }
            let identity = selected_identity(p);
            let col = col as usize;
            if col == (*p).sort {
                (*p).descending = !(*p).descending;
            } else {
                (*p).sort = col;
                (*p).descending = (*p).page == Page::Processes && col >= 2;
            }
            rebuild(p, identity);
            update_sort_header(p);
            redraw(p);
            0
        }
        LVN_ITEMCHANGED => {
            if !(*p).updating {
                (*p).notice.clear();
                update_buttons(p);
                redraw(p);
            }
            0
        }
        NM_DBLCLK => {
            if (*p).page == Page::Processes {
                PostMessageW((*p).hwnd, WM_COMMAND, SECONDARY, 0);
            }
            0
        }
        NM_CUSTOMDRAW => {
            let draw = &mut *(l as *mut NMLVCUSTOMDRAW);
            match draw.nmcd.dwDrawStage {
                CDDS_PREPAINT => CDRF_NOTIFYITEMDRAW as isize,
                CDDS_ITEMPREPAINT => {
                    draw.clrText = INK;
                    // Virtual list custom-draw state may report CDIS_SELECTED
                    // for unselected rows; ask the actual ListView selection.
                    draw.clrTextBk = if SendMessageW(
                        (*p).list,
                        LVM_GETITEMSTATE,
                        draw.nmcd.dwItemSpec,
                        LVIS_SELECTED as isize,
                    ) != 0
                    {
                        SELECTED
                    } else if draw.nmcd.dwItemSpec.is_multiple_of(2) {
                        SURFACE
                    } else {
                        rgb(250, 251, 253)
                    };
                    CDRF_DODEFAULT as isize
                }
                _ => CDRF_DODEFAULT as isize,
            }
        }
        _ => 0,
    }
}
unsafe fn cell_at(p: *mut App, row: usize, col: i32) -> String {
    match (*p).page {
        Page::Processes => (*p)
            .snapshot
            .as_ref()
            .and_then(|s| s.processes.get(row))
            .map(|s| cell(s, col))
            .unwrap_or_default(),
        Page::Startup => (&(*p).startup)
            .get(row)
            .map(|s| match col {
                0 => s.name.clone(),
                1 => s.status.clone(),
                2 => s.location.clone(),
                3 => s.command.clone(),
                _ => String::new(),
            })
            .unwrap_or_default(),
        Page::Services => (&(*p).services)
            .get(row)
            .map(|s| match col {
                0 => s.name.clone(),
                1 => s.display_name.clone(),
                2 => crate::services::state_label(s.state).into(),
                3 => {
                    if s.pid == 0 {
                        "—".into()
                    } else {
                        s.pid.to_string()
                    }
                }
                4 => crate::services::start_type_label(s.start_type).into(),
                _ => String::new(),
            })
            .unwrap_or_default(),
        Page::Performance => String::new(),
    }
}
fn cell(p: &Process, col: i32) -> String {
    match col {
        0 => p.name.clone(),
        1 => p.pid.to_string(),
        2 => format!("{:.1}%", p.cpu_percent),
        3 => format!("{:.1} MB", p.working_set as f64 / 1048576.0),
        4 => format!("{:.1} MB", p.private_bytes as f64 / 1048576.0),
        5 => rate(p.io_bytes_per_sec),
        6 => p.threads.to_string(),
        7 => p.handles.to_string(),
        _ => String::new(),
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
        let result = (|| {
            (*p).startup = crate::startup::list()?;
            (*p).startup_loaded = true;
            (*p).services = crate::services::list()?;
            (*p).services_loaded = true;
            let mut sampler = Sampler::new()?;
            let mut perf = PerfSampler::new()?;
            switch_page(p, Page::Performance);
            // Populate a complete one-minute trace with measured values.
            for i in 0..61 {
                if i > 0 {
                    std::thread::sleep(Duration::from_secs(1));
                }
                snapshots
                    .send(MonitorSample {
                        at: Instant::now(),
                        snapshot: sampler.sample(),
                        performance: Some(perf.sample()),
                    })
                    .map_err(|e| e.to_string())?;
                drain_snapshot(p);
            }
            let performance = (*p).performance.clone();
            for (page, name) in [
                (Page::Processes, "processes.bmp"),
                (Page::Performance, "performance.bmp"),
                (Page::Startup, "startup.bmp"),
                (Page::Services, "services.bmp"),
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
            // Exercise the same native layout at its supported minimum size
            // and with the 150% WM_DPICHANGED font/layout path. The latter is
            // a controlled DPI simulation, not a second physical monitor.
            for (dpi, width, height, suffix) in
                [(96, 980, 660, "minimum"), (144, 1800, 1230, "dpi150")]
            {
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
                for (page, name) in [
                    (Page::Processes, "processes"),
                    (Page::Performance, "performance"),
                ] {
                    switch_page(p, page);
                    if page == Page::Performance {
                        (*p).performance = performance.clone();
                    }
                    rebuild(p, None);
                    layout(p);
                    update_buttons(p);
                    capture::save_client(p, &dir.join(format!("{name}-{suffix}.bmp")))?;
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
                    snapshot: Err(error.into()),
                    performance: None,
                })
                .unwrap();
            unsafe {
                SendMessageW((*self.p).hwnd, SNAPSHOT_READY, 0, 0);
            }
        }
        fn sample(&self, processes: Vec<Process>, at: Instant) {
            self.snapshots
                .send(MonitorSample {
                    at,
                    snapshot: Ok(Snapshot {
                        processes,
                        cpu_percent: 12.5,
                        memory_used: 1024,
                        memory_total: 4096,
                        sample_ms: 0.25,
                    }),
                    performance: None,
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
            threads: 3,
            handles: 12,
        }
    }
    fn rows() -> Vec<Process> {
        vec![
            process(101, 1001, "Alpha.exe", 9, 2.0),
            process(202, 1002, "Beta.exe", 100, 9.0),
            process(303, 1003, "테스트.exe", 2, 10.0),
        ]
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
            assert_eq!(
                GetWindowLongW((*test.p).search, GWL_STYLE) as u32 & WS_VISIBLE,
                0
            );
        }
        assert!(test.jobs.try_recv().is_err());
        test.page(Page::Startup);
        assert!(matches!(test.jobs.try_recv(), Ok(Job::Startup)));
        test.result(JobResult::Startup(Ok(Vec::new())));
        unsafe {
            assert_eq!(
                SendMessageW(
                    SendMessageW((*test.p).list, LVM_GETHEADER, 0, 0) as HWND,
                    HDM_GETITEMCOUNT,
                    0,
                    0
                ),
                4
            );
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
        assert_eq!(test.text(0, 2), "실행 중");
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
        test.sort(3);
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
        assert!(test.jobs.try_recv().is_err());
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
            assert_eq!(test.text(0, 1), entries[row].status);
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
            notice: "unused",
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
        unsafe {
            command(test.p, PAUSE, 0);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure {
                    paused: true,
                    interval: 1000,
                    ..
                }
            ));
            SendMessageW((*test.p).rate, CB_SETCURSEL, 3, 0);
            command(test.p, RATE, CBN_SELCHANGE);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure {
                    paused: true,
                    interval: 5000,
                    ..
                }
            ));
            command(test.p, PAUSE, 0);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure { paused: false, .. }
            ));
            SendMessageW((*test.p).hwnd, WM_SIZE, SIZE_MINIMIZED as usize, 0);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure { paused: true, .. }
            ));
            SendMessageW((*test.p).hwnd, WM_SIZE, SIZE_RESTORED as usize, 0);
            assert!(matches!(
                test.commands.recv().unwrap(),
                Command::Configure { paused: false, .. }
            ));
        }
    }
}
