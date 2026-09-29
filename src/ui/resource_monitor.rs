//! In-process resource monitor. The shared monitor supplies snapshots; this window
//! has no polling timer and paints virtual rows only when data or input changes.
use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use widgets::{ButtonState, ButtonStyle, Painter};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};

mod aggregate;
mod body_scroll;
mod chrome;
mod content;
mod view;

const SEARCH: usize = 801;
const RATE: usize = 802;
const END: usize = 803;
const CLEAR: usize = 804;
const BACK: usize = 805;
const TRACE: usize = 806;
const NAV: usize = 820;
const HEAD: usize = 840;
const TABLE: usize = 860;
const BODY: usize = 880;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Tab {
    Overview,
    Cpu,
    Memory,
    Disk,
    Network,
}
impl Tab {
    const ALL: [Self; 5] = [
        Self::Overview,
        Self::Cpu,
        Self::Memory,
        Self::Disk,
        Self::Network,
    ];
    fn title(self) -> &'static str {
        match self {
            Self::Overview => tr("개요", "Overview"),
            Self::Cpu => "CPU",
            Self::Memory => tr("메모리", "Memory"),
            Self::Disk => tr("디스크", "Disk"),
            Self::Network => tr("네트워크", "Network"),
        }
    }
    fn panels(self) -> &'static [Kind] {
        match self {
            Self::Overview => &[Kind::Cpu, Kind::Files, Kind::Traffic, Kind::Memory],
            Self::Cpu => &[Kind::Cpu, Kind::Services, Kind::Modules],
            Self::Memory => &[Kind::Memory, Kind::Physical],
            Self::Disk => &[Kind::Io, Kind::Files, Kind::Storage],
            Self::Network => &[Kind::Network, Kind::Traffic, Kind::Tcp, Kind::Listening],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Cpu,
    Memory,
    Io,
    Network,
    Services,
    Modules,
    Files,
    Storage,
    Traffic,
    Tcp,
    Listening,
    Physical,
}
impl Kind {
    fn check(self) -> bool {
        matches!(self, Self::Cpu | Self::Memory | Self::Io | Self::Network)
    }
    fn title(self) -> &'static str {
        match self {
            Self::Cpu | Self::Memory => tr("프로세스", "Processes"),
            Self::Io => tr(
                "프로세스 I/O · 파일·네트워크·장치 포함",
                "Process I/O · files, network and devices",
            ),
            Self::Network => tr(
                "네트워크 활동이 있는 프로세스",
                "Processes with network activity",
            ),
            Self::Services => tr("서비스", "Services"),
            Self::Modules => tr("연결된 모듈", "Associated modules"),
            Self::Files => tr(
                "파일 I/O 요청 · 캐시 포함",
                "File I/O requests · includes cache",
            ),
            Self::Storage => tr("저장소", "Storage"),
            Self::Traffic => tr("네트워크 활동", "Network activity"),
            Self::Tcp => tr("TCP 연결", "TCP connections"),
            Self::Listening => tr("수신 대기 포트", "Listening ports"),
            Self::Physical => tr("물리적 메모리", "Physical memory"),
        }
    }
}

#[derive(Clone)]
struct Cell {
    text: String,
    number: Option<f64>,
}
impl Cell {
    fn text(value: impl Into<String>) -> Self {
        Self {
            text: value.into(),
            number: None,
        }
    }
    fn num(text: String, number: f64) -> Self {
        Self {
            text,
            number: number.is_finite().then_some(number),
        }
    }
}
struct Row {
    identity: Option<(u32, u64)>,
    /// Stable across refreshes: the process of a process row, otherwise what
    /// the row is about (service, module, file, endpoint, volume) within
    /// its process. Selection, sort ties and accessible keys follow it.
    key: u64,
    cells: Vec<Cell>,
}
struct Panel {
    kind: Kind,
    header: HWND,
    table: HWND,
    rows: Vec<Row>,
    columns: Vec<table::Column>,
    sort: usize,
    descending: bool,
    summary: String,
    empty: String,
    bounds: RECT,
}
struct State {
    owner: *mut App,
    hwnd: HWND,
    body: HWND,
    search: HWND,
    rate: HWND,
    end: HWND,
    clear: HWND,
    back_button: HWND,
    trace_button: HWND,
    nav: [HWND; 5],
    dpi: i32,
    fonts: fonts::Fonts,
    brush: HBRUSH,
    theme_generation: u32,
    back: gfx::BackBuffer,
    body_back: gfx::BackBuffer,
    body_scroll: body_scroll::ScrollState,
    tab: Tab,
    panels: Vec<Panel>,
    collapsed: HashSet<(Tab, Kind)>,
    checked: HashSet<(u32, u64)>,
    query: String,
    interval: u64,
    detailed: bool,
    last_refresh: Option<Instant>,
    scroll: i32,
    content_height: i32,
    panels_width: i32,
    chart_left: i32,
    chart_top: i32,
    chart_width: i32,
    snapshot: Option<Arc<crate::sampler::Snapshot>>,
    performance: Option<Arc<PerfSnapshot>>,
    data: Option<Arc<crate::resource::Snapshot>>,
    network: Option<Arc<ProcessNetworkSample>>,
    files: Option<Arc<crate::fileetw::Sample>>,
    services: Vec<Service>,
    traces: HashMap<String, telemetry::Trace>,
    averages: HashMap<(u32, u64), (u64, Instant)>,
    notice: String,
    chrome: chrome::Chrome,
    pending: aggregate::Pending,
    accepted_at: Option<Instant>,
    process_index: HashMap<(u32, u64), usize>,
    pid_index: HashMap<u32, usize>,
    preview: bool,
    creating: bool,
    /// The control that had the focus when the window was deactivated.
    focus: HWND,
}
impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.brush);
        }
    }
}

unsafe fn state(hwnd: HWND) -> *mut State {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State
}
unsafe fn owner_state(owner: *mut App) -> *mut State {
    let hwnd = (*owner).resource_window;
    if hwnd.is_null() {
        null_mut()
    } else {
        state(hwnd)
    }
}
fn class(body: bool) -> &'static [u16] {
    static MAIN: OnceLock<Vec<u16>> = OnceLock::new();
    static BODY_CLASS: OnceLock<Vec<u16>> = OnceLock::new();
    let slot = if body { &BODY_CLASS } else { &MAIN };
    slot.get_or_init(|| unsafe {
        let name = wide(if body {
            "FeatherResourceBody"
        } else {
            "FeatherResourceMonitor"
        });
        RegisterClassW(&WNDCLASSW {
            style: CS_DBLCLKS,
            lpfnWndProc: Some(if body { body_proc } else { window_proc }),
            hInstance: GetModuleHandleW(null()),
            hIcon: LoadIconW(GetModuleHandleW(null()), APP_ICON),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: name.as_ptr(),
            ..zeroed()
        });
        name
    })
}

pub(super) unsafe fn open(owner: *mut App) {
    create(owner, true);
}

unsafe fn create(owner: *mut App, visible: bool) {
    if !(*owner).resource_window.is_null() {
        if visible {
            settings_changed(owner);
            super::shell::reveal((*owner).resource_window);
            configure(owner);
        }
        return;
    }
    if (*owner).resource_snapshot.is_none() && (*owner).error_source != Some(ErrorSource::Monitor) {
        (*owner).resource_snapshot = (*owner).snapshot.clone();
        (*owner).resource_performance = (*owner).performance.clone();
    }
    let dpi = (*owner).dpi;
    let raw = Box::into_raw(Box::new(State {
        owner,
        hwnd: null_mut(),
        body: null_mut(),
        search: null_mut(),
        rate: null_mut(),
        end: null_mut(),
        clear: null_mut(),
        back_button: null_mut(),
        trace_button: null_mut(),
        nav: [null_mut(); 5],
        dpi,
        fonts: fonts::Fonts::new(dpi, language()),
        brush: CreateSolidBrush(colors().surface),
        theme_generation: theme::generation(),
        back: gfx::BackBuffer::new(),
        body_back: gfx::BackBuffer::new(),
        body_scroll: Default::default(),
        tab: Tab::Overview,
        panels: Vec::new(),
        collapsed: HashSet::from([(Tab::Cpu, Kind::Modules)]),
        checked: HashSet::new(),
        query: String::new(),
        interval: 1000,
        detailed: false,
        last_refresh: None,
        scroll: 0,
        content_height: 0,
        panels_width: 0,
        chart_left: 0,
        chart_top: 0,
        chart_width: 0,
        snapshot: None,
        performance: None,
        data: None,
        network: None,
        files: None,
        services: Vec::new(),
        traces: HashMap::new(),
        averages: HashMap::new(),
        notice: String::new(),
        chrome: Default::default(),
        pending: Default::default(),
        accepted_at: None,
        process_index: HashMap::new(),
        pid_index: HashMap::new(),
        preview: !visible,
        creating: true,
        focus: null_mut(),
    }));
    let mut rect: RECT = zeroed();
    GetWindowRect((*owner).hwnd, &mut rect);
    let monitor = MonitorFromWindow((*owner).hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..zeroed()
    };
    GetMonitorInfoW(monitor, &mut info);
    let width = gfx::pxi(dpi, 1320.0).min(info.rcWork.right - info.rcWork.left);
    let height = gfx::pxi(dpi, 840.0).min(info.rcWork.bottom - info.rcWork.top);
    let hwnd = CreateWindowExW(
        WS_EX_APPWINDOW | (GetWindowLongW((*owner).hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST),
        class(false).as_ptr(),
        wide("Feather Resource Monitor").as_ptr(),
        WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
        (rect.left + 24).clamp(info.rcWork.left, info.rcWork.right - width),
        (rect.top + 24).clamp(info.rcWork.top, info.rcWork.bottom - height),
        width,
        height,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null()),
        raw.cast(),
    );
    if hwnd.is_null() {
        drop(Box::from_raw(raw));
        return;
    }
    (*raw).creating = false;
    (*owner).resource_window = hwnd;
    if visible {
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
    }
    for point in &(*owner).history {
        for (key, value) in [
            ("cpu", point.cpu),
            ("memory", point.memory),
            ("disk", point.disk),
            ("network", point.network),
        ] {
            (*raw)
                .traces
                .entry(key.into())
                .or_default()
                .record(point.at, [value, f64::NAN, f64::NAN]);
        }
    }
    refresh(owner, Instant::now());
    configure(owner);
}

pub(super) unsafe fn close(owner: *mut App) {
    if !(*owner).resource_window.is_null() {
        DestroyWindow((*owner).resource_window);
    }
}
pub(super) unsafe fn active(hwnd: HWND) -> bool {
    if hwnd.is_null() || IsWindow(hwnd) == 0 {
        return false;
    }
    let s = state(hwnd);
    !s.is_null() && (*s).interval != 0 && IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0
}
pub(super) unsafe fn interval(hwnd: HWND) -> Option<u64> {
    if !active(hwnd) {
        return None;
    }
    let s = state(hwnd);
    if s.is_null() || (*s).interval == 0 {
        None
    } else {
        Some((*s).interval)
    }
}
pub(super) unsafe fn needs_services(hwnd: HWND) -> bool {
    if interval(hwnd).is_none() {
        return false;
    }
    let s = state(hwnd);
    (*s).tab == Tab::Cpu && !(*s).collapsed.contains(&(Tab::Cpu, Kind::Services))
}
pub(super) unsafe fn tracing(hwnd: HWND) -> (bool, bool) {
    if interval(hwnd).is_none() {
        return (false, false);
    }
    let s = state(hwnd);
    if !(*s).detailed {
        return (false, false);
    }
    let shown =
        |kind| (*s).tab.panels().contains(&kind) && !(*s).collapsed.contains(&((*s).tab, kind));
    (shown(Kind::Files), shown(Kind::Traffic))
}
pub(super) unsafe fn request(owner: *mut App) -> crate::resource::Request {
    let s = owner_state(owner);
    if s.is_null() || interval((*s).hwnd).is_none() {
        return Default::default();
    }
    let shown =
        |kind| (*s).tab.panels().contains(&kind) && !(*s).collapsed.contains(&((*s).tab, kind));
    let selected = ((*s).checked.len() == 1).then(|| *(*s).checked.iter().next().unwrap());
    crate::resource::Request {
        memory: shown(Kind::Physical),
        endpoints: shown(Kind::Tcp) || shown(Kind::Listening),
        volumes: shown(Kind::Storage),
        modules: if shown(Kind::Modules) { selected } else { None },
    }
}

pub(super) unsafe fn needs_status(hwnd: HWND) -> bool {
    if !active(hwnd) {
        return false;
    }
    let s = state(hwnd);
    matches!((*s).tab, Tab::Overview | Tab::Cpu) && !(*s).collapsed.contains(&((*s).tab, Kind::Cpu))
}

pub(super) unsafe fn settings_changed(owner: *mut App) {
    let s = owner_state(owner);
    if s.is_null() {
        return;
    }
    // Keep this separate window reachable when the task manager is topmost.
    let topmost = GetWindowLongW((*owner).hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0;
    let current = GetWindowLongW((*s).hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST != 0;
    if topmost != current {
        SetWindowPos(
            (*s).hwnd,
            if topmost {
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
    }
    let fonts_changed_now = !(*s).fonts.matches((*s).dpi, language());
    if (*s).theme_generation == theme::generation() && !fonts_changed_now {
        return;
    }
    (*s).theme_generation = theme::generation();
    if fonts_changed_now {
        let old = std::mem::replace(&mut (*s).fonts, fonts::Fonts::new((*s).dpi, language()));
        fonts_changed(s);
        drop(old);
        for (i, tab) in Tab::ALL.iter().enumerate() {
            SetWindowTextW((*s).nav[i], wide(tab.title()).as_ptr());
        }
        SetWindowTextW(
            (*s).end,
            wide(tr("프로세스 종료", "End processes")).as_ptr(),
        );
        SetWindowTextW((*s).clear, wide(tr("필터 해제", "Clear filter")).as_ptr());
        SetWindowTextW(
            (*s).trace_button,
            wide(if (*s).detailed {
                tr("상세 추적 켜짐", "Tracing on")
            } else {
                tr("상세 추적 꺼짐", "Tracing off")
            })
            .as_ptr(),
        );
        sync_rate(s);
        SetWindowTextW(
            (*s).back_button,
            wide(tr("작업 관리자 열기", "Open Task Manager")).as_ptr(),
        );
        SendMessageW(
            (*s).search,
            EM_SETCUEBANNER,
            0,
            wide(tr(
                "프로세스·파일·주소 검색",
                "Search processes, files or addresses",
            ))
            .as_ptr() as isize,
        );
        build_panels(s);
    }
    DeleteObject((*s).brush);
    (*s).brush = CreateSolidBrush(colors().surface);
    theme_window(s);
    view::layout(s);
    invalidate(s);
}

pub(super) unsafe fn refresh(owner: *mut App, _at: Instant) {
    let s = owner_state(owner);
    if s.is_null() {
        return;
    }
    settings_changed(owner);
    let notice = (*owner).error.as_ref().unwrap_or(&(*owner).notice);
    let enabled = !(*s).checked.is_empty() && !(*owner).busy;
    if (*s).notice != *notice || (IsWindowEnabled((*s).end) != 0) != enabled {
        (*s).notice = notice.clone();
        EnableWindow((*s).end, i32::from(enabled));
        InvalidateRect((*s).hwnd, null(), 0);
    }
    if (*s).interval == 0 || (!(*s).preview && !active((*s).hwnd)) {
        (*s).pending.clear();
        return;
    }
    let Some(at) = (*owner).resource_sample_at.or((*owner).last_sample) else {
        return;
    };
    if let (Some(network), Some(files)) = (&(*owner).resource_network, &(*owner).resource_files) {
        (*s).pending.push(at, network, files);
    }
    if (*s).accepted_at == Some(at) {
        return;
    }
    if (*s).last_refresh.is_some_and(|last| {
        at.saturating_duration_since(last)
            < Duration::from_millis((*s).interval.saturating_sub(100))
    }) {
        return;
    }
    let initial_details = (*s).snapshot.is_none() || (*s).performance.is_none();
    (*s).last_refresh = Some(at);
    (*s).accepted_at = Some(at);
    (*s).snapshot = (*owner).resource_snapshot.clone();
    (*s).performance = (*owner).resource_performance.clone();
    (*s).data = (*owner).resource_data.clone();
    let (network, files) = (*s).pending.take();
    (*s).network = Some(Arc::new(network));
    (*s).files = Some(Arc::new(files));
    if (*s).tab == Tab::Cpu && !(*s).collapsed.contains(&(Tab::Cpu, Kind::Services)) {
        (*s).services = (*owner)
            .resource_services
            .clone()
            .unwrap_or_else(|| (*owner).services.clone());
    }
    (*s).process_index.clear();
    (*s).pid_index.clear();
    if let Some(snapshot) = &(*s).snapshot {
        for (i, p) in snapshot.processes.iter().enumerate() {
            (*s).process_index.insert((p.pid, p.created), i);
            (*s).pid_index.insert(p.pid, i);
        }
        let before = (*s).checked.len();
        (*s).checked
            .retain(|id| (*s).process_index.contains_key(id));
        (*s).averages
            .retain(|id, _| (*s).process_index.contains_key(id));
        for p in &snapshot.processes {
            (*s).averages
                .entry((p.pid, p.created))
                .or_insert((p.cpu_time_100ns, at));
        }
        if before != (*s).checked.len() {
            configure(owner);
        }
    }
    content::record(s, at);
    content::rebuild(s, at);
    view::layout(s);
    if initial_details {
        invalidate(s);
    } else {
        invalidate_data(s);
    }
}
unsafe fn invalidate_data(s: *mut State) {
    let mut footer: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut footer);
    footer.top = (footer.bottom - gfx::pxi((*s).dpi, 30.0)).max(0);
    InvalidateRect((*s).hwnd, &footer, 0);
    for i in [1, 2] {
        InvalidateRect((*s).nav[i], null(), 0);
    }
    // The body clips out child tables; static shell controls need no repaint.
    InvalidateRect((*s).body, null(), 0);
    for panel in &(*s).panels {
        InvalidateRect(panel.header, null(), 0);
        if !panel.table.is_null() {
            InvalidateRect(panel.table, null(), 0);
        }
    }
    for (button, enabled) in [
        ((*s).end, !(*s).checked.is_empty() && !(*(*s).owner).busy),
        ((*s).clear, !(*s).checked.is_empty()),
    ] {
        if (IsWindowEnabled(button) != 0) != enabled {
            EnableWindow(button, i32::from(enabled));
        }
    }
}
unsafe fn invalidate(s: *mut State) {
    InvalidateRect((*s).hwnd, null(), 0);
    InvalidateRect((*s).body, null(), 0);
    for h in (*s).nav {
        InvalidateRect(h, null(), 0);
    }
    for p in &(*s).panels {
        InvalidateRect(p.header, null(), 0);
        if !p.table.is_null() {
            InvalidateRect(p.table, null(), 0);
        }
    }
    EnableWindow(
        (*s).end,
        i32::from(!(*s).checked.is_empty() && !(*(*s).owner).busy),
    );
    EnableWindow((*s).clear, i32::from(!(*s).checked.is_empty()));
}
/// Rebuild after a view change. `new_data`: it may change what the monitor
/// collects (tab, panel, checks, tracing, rate), so the monitor is told and
/// the next sample is taken as soon as it arrives. A search or a sort only
/// filters and reorders the current frame and never resamples.
unsafe fn changed(s: *mut State, new_data: bool) {
    if new_data {
        (*s).last_refresh = None;
        configure((*s).owner);
    }
    content::rebuild(s, (*s).accepted_at.unwrap_or_else(Instant::now));
    view::layout(s);
    invalidate(s);
}
unsafe fn theme_window(s: *mut State) {
    chrome::apply_theme(s);
}
unsafe fn fonts_changed(s: *mut State) {
    for h in [
        (*s).search,
        (*s).rate,
        (*s).end,
        (*s).clear,
        (*s).trace_button,
        (*s).back_button,
    ]
    .into_iter()
    .chain((*s).nav)
    {
        SendMessageW(h, WM_SETFONT, (*s).fonts.body as usize, 1);
    }
    for panel in &(*s).panels {
        if !panel.table.is_null() {
            SendMessageW(panel.table, WM_SETFONT, (*s).fonts.body as usize, 1);
        }
    }
}
unsafe fn control(parent: HWND, id: usize, class_name: &str, label: &str, style: u32) -> HWND {
    CreateWindowExW(
        0,
        wide(class_name).as_ptr(),
        wide(label).as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
        0,
        0,
        0,
        0,
        parent,
        id as HMENU,
        GetModuleHandleW(null()),
        null(),
    )
}
unsafe fn initialize(s: *mut State) {
    let hwnd = (*s).hwnd;
    (*s).search = control(hwnd, SEARCH, "EDIT", "", ES_AUTOHSCROLL as u32);
    SendMessageW((*s).search, EM_SETLIMITTEXT, 512, 0);
    SendMessageW(
        (*s).search,
        EM_SETCUEBANNER,
        0,
        wide(tr(
            "프로세스·파일·주소 검색",
            "Search processes, files or addresses",
        ))
        .as_ptr() as isize,
    );
    (*s).rate = control(hwnd, RATE, "BUTTON", "", BS_OWNERDRAW as u32);
    sync_rate(s);
    (*s).end = control(
        hwnd,
        END,
        "BUTTON",
        tr("프로세스 종료", "End processes"),
        BS_OWNERDRAW as u32,
    );
    (*s).clear = control(
        hwnd,
        CLEAR,
        "BUTTON",
        tr("필터 해제", "Clear filter"),
        BS_OWNERDRAW as u32,
    );
    (*s).back_button = control(
        hwnd,
        BACK,
        "BUTTON",
        tr("작업 관리자 열기", "Open Task Manager"),
        BS_OWNERDRAW as u32,
    );
    (*s).trace_button = control(
        hwnd,
        TRACE,
        "BUTTON",
        tr("상세 추적 꺼짐", "Tracing off"),
        BS_OWNERDRAW as u32,
    );
    for (i, tab) in Tab::ALL.iter().enumerate() {
        (*s).nav[i] = control(hwnd, NAV + i, "BUTTON", tab.title(), BS_OWNERDRAW as u32);
    }
    (*s).body = CreateWindowExW(
        WS_EX_CONTROLPARENT,
        class(true).as_ptr(),
        wide("").as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_TABSTOP,
        0,
        0,
        0,
        0,
        hwnd,
        BODY as HMENU,
        GetModuleHandleW(null()),
        s.cast(),
    );
    build_panels(s);
    fonts_changed(s);
    theme_window(s);
    chrome::attach(s);
    view::layout(s);
}
unsafe fn build_panels(s: *mut State) {
    for panel in &(*s).panels {
        DestroyWindow(panel.header);
        if !panel.table.is_null() {
            DestroyWindow(panel.table);
        }
    }
    (*s).panels.clear();
    for kind in (*s).tab.panels() {
        let columns = content::columns(*kind);
        let index = (*s).panels.len();
        let header = control(
            (*s).body,
            HEAD + index,
            "BUTTON",
            kind.title(),
            BS_OWNERDRAW as u32,
        );
        (*s).panels.push(Panel {
            kind: *kind,
            header,
            table: null_mut(),
            rows: Vec::new(),
            columns: columns.clone(),
            sort: match kind {
                Kind::Cpu | Kind::Memory | Kind::Io | Kind::Network | Kind::Files => 5,
                Kind::Traffic => 6,
                _ => 0,
            },
            descending: kind.check(),
            summary: String::new(),
            empty: String::new(),
            bounds: zeroed(),
        });
        if *kind != Kind::Physical {
            let table = table::create(
                (*s).body,
                TABLE + index,
                kind.title(),
                table::Mode::Table,
                Box::new(content::Model(s, index)),
            );
            (&mut (*s).panels)[index].table = table;
            table::set_columns(table, columns);
            content::install_scroll(table, s);
        }
    }
    content::rebuild(s, Instant::now());
}
unsafe fn set_tab(s: *mut State, tab: Tab) {
    if (*s).tab == tab {
        return;
    }
    (*s).tab = tab;
    (*s).scroll = 0;
    build_panels(s);
    changed(s, true);
}
unsafe fn set_rate(s: *mut State, rate: u64) {
    let paused = (*s).interval == 0;
    (*s).interval = rate;
    (*s).pending.clear();
    sync_rate(s);
    if paused || rate == 0 {
        for t in (*s).traces.values_mut() {
            t.record(Instant::now(), [f64::NAN; 3]);
        }
    }
    changed(s, true);
}
fn rate_label(rate: u64) -> &'static str {
    match rate {
        0 => tr("일시 중지", "Paused"),
        500 => "0.5 s",
        2000 => "2 s",
        _ => "1 s",
    }
}
unsafe fn sync_rate(s: *mut State) {
    SetWindowTextW(
        (*s).rate,
        wide(&format!(
            "{}: {}",
            tr("갱신", "Refresh"),
            rate_label((*s).interval)
        ))
        .as_ptr(),
    );
}
unsafe fn track_menu(s: *mut State, menu: HMENU, flags: u32, at: POINT) -> u32 {
    let owner = (*s).owner;
    if (*owner).modal {
        return 0;
    }
    // Keep both window states alive while the native menu pumps messages.
    (*owner).modal = true;
    configure(owner);
    let chosen = TrackPopupMenu(
        menu,
        flags | TPM_RETURNCMD | TPM_NONOTIFY,
        at.x,
        at.y,
        0,
        (*s).hwnd,
        null(),
    );
    (*owner).modal = false;
    configure(owner);
    PostMessageW((*owner).hwnd, SNAPSHOT_READY, 0, 0);
    PostMessageW((*owner).hwnd, JOB_READY, 0, 0);
    chosen as u32
}
unsafe fn choose_rate(s: *mut State) {
    let menu = CreatePopupMenu();
    for (i, rate) in [0, 500, 1000, 2000].into_iter().enumerate() {
        AppendMenuW(
            menu,
            MF_STRING | if rate == (*s).interval { MF_CHECKED } else { 0 },
            i + 1,
            wide(rate_label(rate)).as_ptr(),
        );
    }
    let mut rect: RECT = zeroed();
    GetWindowRect((*s).rate, &mut rect);
    let chosen = track_menu(
        s,
        menu,
        TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
        POINT {
            x: rect.right,
            y: rect.top,
        },
    );
    DestroyMenu(menu);
    if let Some(rate) = chosen
        .checked_sub(1)
        .and_then(|n| [0, 500, 1000, 2000].get(n as usize))
    {
        set_rate(s, *rate);
    }
}
unsafe fn toggle(s: *mut State, id: (u32, u64)) {
    if !(*s).checked.remove(&id) {
        (*s).checked.insert(id);
    }
    changed(s, true);
}
/// Most processes the End processes confirmation names; the rest are counted.
const LISTED: usize = 12;

/// The End processes confirmation for `names` (`name (PID n)`): heading,
/// text and a warn line when `warnings` (the main window's End task line of
/// each Windows process among them) is not empty.
fn end_prompt(names: &[String], warnings: &[&str]) -> (String, String, Option<String>) {
    let notice = tr(
        "저장하지 않은 작업은 사라질 수 있습니다. 시스템 프로세스 보호는 그대로 적용됩니다.",
        "Unsaved work may be lost. System process protections remain in effect.",
    );
    let (heading, body) = if let [name] = names {
        (
            tf!("{} 프로세스를 종료할까요?", "End {}?", name),
            notice.to_owned(),
        )
    } else {
        let mut list = names[..names.len().min(LISTED)].join("\n");
        if names.len() > LISTED {
            list.push('\n');
            list.push_str(&tf!("외 {}개", "and {} more", names.len() - LISTED));
        }
        (
            tf!(
                "프로세스 {}개를 종료할까요?",
                "End {} processes?",
                names.len()
            ),
            format!("{list}\n\n{notice}"),
        )
    };
    let warn = match warnings {
        [] => None,
        [one] if names.len() == 1 => Some((*one).to_owned()),
        _ => Some(tf!(
            "Windows 프로세스 {}개가 포함되어 있습니다. 종료하면 Windows가 불안정해지거나 로그아웃될 수 있습니다.",
            "Includes {} Windows processes. Ending them can make Windows unstable or sign you out.",
            warnings.len()
        )),
    };
    (heading, body, warn)
}
unsafe fn end_processes(s: *mut State, ids: Vec<(u32, u64)>) {
    if ids.is_empty() || (*(*s).owner).busy || (*(*s).owner).modal {
        return;
    }
    let names: Vec<_> = ids
        .iter()
        .filter_map(|id| {
            (*s).snapshot
                .as_ref()?
                .processes
                .iter()
                .find(|p| (p.pid, p.created) == *id)
                .map(|p| format!("{} (PID {})", p.name, p.pid))
        })
        .collect();
    if names.is_empty() {
        return;
    }
    // Only images that really live in the Windows directory, never a guess
    // from the name (as for the main window's End task).
    let warnings: Vec<_> = ids
        .iter()
        .filter_map(|&(pid, created)| controls::windows_image_warning(pid, created))
        .collect();
    let (heading, body, warn) = end_prompt(&names, &warnings);
    let owner = (*s).owner;
    let focus = GetFocus();
    (*owner).modal = true;
    configure(owner);
    // Feather's confirm dialog over this window (not a MessageBox): Cancel
    // has the focus; the window's input waits until it closes.
    let confirmed = popup::confirm_dialog_on(
        popup::Host {
            hwnd: (*s).hwnd,
            dpi: (*s).dpi,
            fonts: &(*s).fonts,
        },
        &popup::ConfirmSpec {
            title: &heading,
            body: &body,
            warn: warn.as_deref(),
            action: tr("프로세스 종료", "End processes"),
            cancel: tr("취소", "Cancel"),
            danger: true,
        },
    );
    (*owner).modal = false;
    configure(owner);
    PostMessageW((*owner).hwnd, SNAPSHOT_READY, 0, 0);
    PostMessageW((*owner).hwnd, JOB_READY, 0, 0);
    if IsWindow(focus) != 0 && IsChild((*s).hwnd, focus) != 0 {
        SetFocus(focus);
    }
    if confirmed {
        begin_action(owner, Action::EndMany(ids));
        invalidate(s);
    }
}
unsafe fn notify(s: *mut State, l: LPARAM) -> LRESULT {
    let header = &*(l as *const NMHDR);
    let index = header.idFrom.saturating_sub(TABLE);
    if index >= (*s).panels.len() {
        return 0;
    }
    match header.code {
        LVN_COLUMNCLICK => {
            let n = &*(l as *const NMLISTVIEW);
            let col = n.iSubItem.max(0) as usize;
            let check_all = (&(*s).panels)[index].kind.check() && col == 0;
            if check_all {
                let ids: Vec<_> = (&(*s).panels)[index]
                    .rows
                    .iter()
                    .filter_map(|r| r.identity)
                    .collect();
                let all = ids.iter().all(|id| (*s).checked.contains(id));
                for id in ids {
                    if all {
                        (*s).checked.remove(&id);
                    } else {
                        (*s).checked.insert(id);
                    }
                }
            } else {
                let panel = &mut (&mut (*s).panels)[index];
                if panel.sort == col {
                    panel.descending = !panel.descending;
                } else {
                    panel.sort = col;
                    panel.descending = panel.columns.get(col).is_some_and(|c| c.right);
                }
            }
            // Checks can change what is collected (one checked process's
            // modules); a sort only reorders what the window already has.
            changed(s, check_all);
        }
        NM_CLICK => {
            let n = &*(l as *const NMITEMACTIVATE);
            if (&(*s).panels)[index].kind.check() && n.iItem >= 0 {
                if let Some(id) = (&(*s).panels)[index]
                    .rows
                    .get(n.iItem as usize)
                    .and_then(|r| r.identity)
                {
                    toggle(s, id);
                }
            }
        }
        _ => {}
    }
    0
}
unsafe fn context(s: *mut State, hwnd: HWND, l: LPARAM) {
    let Some(index) = (*s).panels.iter().position(|p| p.table == hwnd) else {
        return;
    };
    let anchor = table::keyboard_menu_anchor(hwnd, l);
    let row = if l == -1 {
        SendMessageW(hwnd, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize)
    } else {
        let mut pt = POINT {
            x: (l as u32 & 0xffff) as i16 as i32,
            y: ((l as u32 >> 16) & 0xffff) as i16 as i32,
        };
        ScreenToClient(hwnd, &mut pt);
        table::part_at(hwnd, pt).map_or(-1, |(row, _)| row as isize)
    };
    let Some(id) = (&(*s).panels)[index]
        .rows
        .get(row as usize)
        .and_then(|r| r.identity)
    else {
        return;
    };
    let menu = CreatePopupMenu();
    AppendMenuW(
        menu,
        MF_STRING,
        1,
        wide(tr("프로세스 종료", "End process")).as_ptr(),
    );
    AppendMenuW(
        menu,
        MF_STRING,
        2,
        wide(tr("이 프로세스로 필터", "Filter to this process")).as_ptr(),
    );
    AppendMenuW(
        menu,
        MF_STRING,
        3,
        wide(tr("작업 관리자에서 보기", "Show in Task Manager")).as_ptr(),
    );
    let mut pt = POINT {
        x: (l as u32 & 0xffff) as i16 as i32,
        y: ((l as u32 >> 16) & 0xffff) as i16 as i32,
    };
    if l == -1 {
        if let Some(r) = anchor {
            pt = POINT {
                x: r.left,
                y: r.bottom,
            };
        } else {
            GetCursorPos(&mut pt);
        }
    }
    let command = track_menu(s, menu, TPM_RIGHTBUTTON, pt);
    DestroyMenu(menu);
    match command {
        1 => end_processes(s, vec![id]),
        2 => {
            (*s).checked.clear();
            (*s).checked.insert(id);
            changed(s, true);
        }
        3 => {
            let owner = (*s).owner;
            super::shell::show_main(owner);
            super::switch_page(owner, Page::Processes);
            SendMessageW(
                (*owner).search,
                WM_SETTEXT,
                0,
                wide(&format!("pid:{}", id.0)).as_ptr() as isize,
            );
        }
        _ => {}
    }
}
pub(super) unsafe fn keyboard(owner: *mut App, msg: &MSG) -> bool {
    let s = owner_state(owner);
    if s.is_null() || !(msg.hwnd == (*s).hwnd || IsChild((*s).hwnd, msg.hwnd) != 0) {
        return false;
    }
    if msg.message == WM_KEYDOWN {
        let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
        if ctrl && msg.wParam == b'F' as usize {
            SetFocus((*s).search);
            SendMessageW((*s).search, EM_SETSEL, 0, -1);
            return true;
        }
        if msg.wParam == VK_ESCAPE as usize {
            if !(&(*s).query).is_empty() {
                SetWindowTextW((*s).search, wide("").as_ptr());
            } else if !(*s).checked.is_empty() {
                (*s).checked.clear();
                changed(s, true);
            } else {
                SetFocus((*s).nav[(*s).tab as usize]);
            }
            return true;
        }
        let typing = msg.hwnd == (*s).search || msg.hwnd == (*s).rate;
        if !typing && !ctrl {
            if (b'1' as usize..=b'5' as usize).contains(&msg.wParam) {
                set_tab(s, Tab::ALL[msg.wParam - b'1' as usize]);
                return true;
            }
            if msg.wParam == VK_DELETE as usize {
                end_processes(s, (*s).checked.iter().copied().collect());
                return true;
            }
            if msg.wParam == VK_SPACE as usize {
                if let Some(index) = (*s)
                    .panels
                    .iter()
                    .position(|p| p.table == msg.hwnd && p.kind.check())
                {
                    let row = SendMessageW(
                        msg.hwnd,
                        LVM_GETNEXTITEM,
                        usize::MAX,
                        LVNI_SELECTED as isize,
                    );
                    if let Some(id) = (&(*s).panels)[index]
                        .rows
                        .get(row as usize)
                        .and_then(|r| r.identity)
                    {
                        toggle(s, id);
                        return true;
                    }
                }
                if msg.hwnd == (*s).body || msg.hwnd == (*s).hwnd {
                    set_rate(s, if (*s).interval == 0 { 1000 } else { 0 });
                    return true;
                }
            }
        }
    }
    let before = GetFocus();
    if IsDialogMessageW((*s).hwnd, msg as *const MSG) != 0 {
        // Tab into a panel scrolled out of the list brings it into view.
        let focus = GetFocus();
        if focus != before {
            body_scroll::reveal(s, focus);
        }
        return true;
    }
    false
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let s = (*(l as *const CREATESTRUCTW)).lpCreateParams as *mut State;
        (*s).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, s as isize);
        return DefWindowProcW(hwnd, msg, w, l);
    }
    let s = state(hwnd);
    if s.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    if let Some(result) = chrome::message(s, hwnd, msg, w, l) {
        return result;
    }
    match msg {
        WM_CREATE => {
            initialize(s);
            0
        }
        WM_SIZE => {
            if w == SIZE_MINIMIZED as usize {
                (*s).pending.clear();
                for t in (*s).traces.values_mut() {
                    t.record(Instant::now(), [f64::NAN; 3]);
                }
            }
            view::layout(s);
            configure((*s).owner);
            invalidate(s);
            0
        }
        WM_DPICHANGED => {
            (*s).dpi = (w & 0xffff) as i32;
            let old = std::mem::replace(&mut (*s).fonts, fonts::Fonts::new((*s).dpi, language()));
            fonts_changed(s);
            drop(old);
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
            view::layout(s);
            invalidate(s);
            0
        }
        WM_GETMINMAXINFO => {
            let m = &mut *(l as *mut MINMAXINFO);
            m.ptMinTrackSize = POINT {
                x: gfx::pxi((*s).dpi, 780.0),
                y: gfx::pxi((*s).dpi, 560.0),
            };
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            view::paint_window(s);
            0
        }
        WM_PRINTCLIENT => {
            view::print_window(s, w as HDC);
            0
        }
        WM_CTLCOLORBTN | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            SetTextColor(w as HDC, colors().fg);
            SetBkColor(w as HDC, colors().surface);
            (*s).brush as isize
        }
        WM_DRAWITEM => {
            view::draw_button(s, &*(l as *const DRAWITEMSTRUCT));
            1
        }
        WM_NOTIFY => notify(s, l),
        // A panel's rows; anything else (the caption strip's system menu)
        // keeps the default handling.
        WM_CONTEXTMENU if (*s).panels.iter().any(|p| p.table == w as HWND) => {
            context(s, w as HWND, l);
            0
        }
        WM_ACTIVATE if (w >> 16) & 0xffff == 0 => {
            if (w & 0xffff) as u32 == WA_INACTIVE {
                let focus = GetFocus();
                if !focus.is_null() && IsChild(hwnd, focus) != 0 {
                    (*s).focus = focus;
                }
            } else {
                // Back to the control that had the focus (DefWindowProc
                // would focus the frame), else the current tab.
                let saved = (*s).focus;
                let usable = IsWindow(saved) != 0
                    && IsChild(hwnd, saved) != 0
                    && IsWindowVisible(saved) != 0
                    && IsWindowEnabled(saved) != 0;
                SetFocus(if usable {
                    saved
                } else {
                    (*s).nav[(*s).tab as usize]
                });
            }
            0
        }
        WM_COMMAND => {
            let id = w & 0xffff;
            let code = (w >> 16) as u32;
            if id == SEARCH && code == EN_CHANGE {
                let mut text = vec![0u16; GetWindowTextLengthW((*s).search).max(0) as usize + 1];
                let n = GetWindowTextW((*s).search, text.as_mut_ptr(), text.len() as i32).max(0)
                    as usize;
                (*s).query = String::from_utf16_lossy(&text[..n]).trim().to_lowercase();
                changed(s, false);
            } else if code == BN_CLICKED {
                match id {
                    RATE => choose_rate(s),
                    END => end_processes(s, (*s).checked.iter().copied().collect()),
                    CLEAR => {
                        (*s).checked.clear();
                        changed(s, true);
                    }
                    BACK => {
                        super::shell::show_main((*s).owner);
                    }
                    TRACE => {
                        (*s).detailed = !(*s).detailed;
                        SetWindowTextW(
                            (*s).trace_button,
                            wide(if (*s).detailed {
                                tr("상세 추적 켜짐", "Tracing on")
                            } else {
                                tr("상세 추적 꺼짐", "Tracing off")
                            })
                            .as_ptr(),
                        );
                        changed(s, true);
                    }
                    _ if (NAV..NAV + 5).contains(&id) => set_tab(s, Tab::ALL[id - NAV]),
                    _ if (HEAD..HEAD + (*s).panels.len()).contains(&id) => {
                        let key = ((*s).tab, (&(*s).panels)[id - HEAD].kind);
                        if !(*s).collapsed.remove(&key) {
                            (*s).collapsed.insert(key);
                        }
                        changed(s, true);
                    }
                    _ => {}
                }
            }
            0
        }
        WM_CLOSE => {
            if !(*(*s).owner).modal {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            let owner = (*s).owner;
            (*owner).resource_window = null_mut();
            (*owner).resource_data = None;
            (*owner).resource_files = None;
            (*owner).resource_network = None;
            (*owner).resource_sample_at = None;
            (*owner).resource_snapshot = None;
            (*owner).resource_performance = None;
            configure(owner);
            0
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !(*s).creating {
                drop(Box::from_raw(s));
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}
unsafe extern "system" fn body_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let s = (*(l as *const CREATESTRUCTW)).lpCreateParams as *mut State;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, s as isize);
        return 1;
    }
    let s = state(hwnd);
    if s.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    if let Some(result) = body_scroll::message(s, hwnd, msg, w, l) {
        return result;
    }
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            view::paint_body(s);
            0
        }
        WM_PRINTCLIENT => {
            view::print_body(s, w as HDC);
            0
        }
        WM_COMMAND | WM_NOTIFY | WM_DRAWITEM | WM_CONTEXTMENU | WM_CTLCOLORBTN
        | WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => SendMessageW((*s).hwnd, msg, w, l),
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

/// Diagnostic-only render path: real local data, hidden Feather-owned windows,
/// no process actions and no optional ETW session.
pub(super) unsafe fn render_previews(owner: *mut App, dir: &std::path::Path) -> Result<(), String> {
    create(owner, false);
    let s = owner_state(owner);
    if s.is_null() {
        return Err("Unable to create Resource Monitor preview".into());
    }
    let theme_before = (*owner).prefs.theme;
    let result = (|| {
        let processes = (*owner)
            .resource_snapshot
            .as_ref()
            .map_or(&[][..], |p| &p.processes);
        let identity = processes
            .iter()
            .find(|p| p.pid == std::process::id())
            .map(|p| (p.pid, p.created));
        let request = crate::resource::Request {
            memory: true,
            endpoints: true,
            volumes: true,
            modules: identity,
        };
        let mut client = crate::resource::Client::default();
        let deadline = Instant::now() + Duration::from_secs(3);
        let details = loop {
            if let Some(snapshot) = client.sample(&request, processes) {
                break Some(snapshot);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        (*s).data = details;
        (*s).services = (*owner).services.clone();
        for theme in [2, 1] {
            (*owner).prefs.theme = theme;
            (*owner).prefs.apply_theme();
            settings_changed(owner);
            fit_client((*s).hwnd, 1320, 840);
            for tab in Tab::ALL {
                set_tab(s, tab);
                content::rebuild(s, (*s).accepted_at.unwrap_or_else(Instant::now));
                view::layout(s);
                for panel in &(*s).panels {
                    if !panel.table.is_null() {
                        table::settle(panel.table);
                    }
                }
                let name = match tab {
                    Tab::Overview => "overview",
                    Tab::Cpu => "cpu",
                    Tab::Memory => "memory",
                    Tab::Disk => "disk",
                    Tab::Network => "network",
                };
                capture::save_window(
                    (*s).hwnd,
                    &dir.join(format!(
                        "resource-{name}-{}.bmp",
                        if theme == 2 { "dark" } else { "light" }
                    )),
                )?;
            }
        }
        if let Some(identity) = identity {
            (*s).checked.insert(identity);
            (*s).collapsed.remove(&(Tab::Cpu, Kind::Modules));
            set_tab(s, Tab::Cpu);
            content::rebuild(s, (*s).accepted_at.unwrap_or_else(Instant::now));
            view::layout(s);
            if let Some(panel) = (*s).panels.iter().find(|p| p.kind == Kind::Modules) {
                (*s).scroll = (panel.bounds.top - gfx::pxi((*s).dpi, 16.0)).max(0);
                view::layout_body(s);
            }
            capture::save_window((*s).hwnd, &dir.join("resource-selected-modules.bmp"))?;
            (*s).checked.clear();
        }
        for (dpi, width, height, name) in [(96, 780, 560, "minimum"), (144, 1980, 1260, "dpi150")] {
            let r = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            SendMessageW(
                (*s).hwnd,
                WM_DPICHANGED,
                dpi | (dpi << 16),
                &r as *const RECT as isize,
            );
            fit_client((*s).hwnd, width, height);
            set_tab(s, Tab::Overview);
            view::layout(s);
            capture::save_window((*s).hwnd, &dir.join(format!("resource-{name}.bmp")))?;
        }
        Ok(())
    })();
    close(owner);
    (*owner).prefs.theme = theme_before;
    (*owner).prefs.apply_theme();
    result
}

#[cfg(test)]
pub(super) unsafe fn assert_end_confirmation(owner: *mut App) {
    use crate::i18n::{with_language, Language};
    with_language(Language::English, || {
        let names: Vec<String> = (0..14).map(|i| format!("app{i}.exe (PID {i})")).collect();
        let (heading, body, warn) = end_prompt(&names[..1], &[]);
        assert_eq!(heading, "End app0.exe (PID 0)?");
        assert!(!body.contains("app0") && warn.is_none());
        let windows = "This is a Windows process.";
        let (_, _, warn) = end_prompt(&names[..1], &[windows]);
        assert_eq!(warn.as_deref(), Some(windows));
        let (heading, body, warn) = end_prompt(&names, &[windows, windows]);
        assert_eq!(heading, "End 14 processes?");
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(
            lines[..12],
            names[..12].iter().map(String::as_str).collect::<Vec<_>>()[..]
        );
        assert_eq!(lines[12], "and 2 more");
        assert!(warn.unwrap().starts_with("Includes 2 Windows processes."));
        assert!(!end_prompt(&names[..12], &[]).1.contains("more"));
    });
    // The dialog belongs to this window: its scrim and card are owned by it
    // and centred on its client, not on the main window.
    create(owner, false);
    let s = owner_state(owner);
    assert!(!s.is_null());
    let host = popup::Host {
        hwnd: (*s).hwnd,
        dpi: (*s).dpi,
        fonts: &(*s).fonts,
    };
    let spec = popup::ConfirmSpec {
        title: "End 2 processes?",
        body: "a
b",
        warn: None,
        action: "End processes",
        cancel: "Cancel",
        danger: true,
    };
    let (scrim, dialog) = popup::stage_confirm_on(host, &spec).unwrap();
    for popup in [scrim, dialog] {
        assert_eq!(GetWindow(popup, GW_OWNER), (*s).hwnd);
    }
    let mut client: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut client);
    MapWindowPoints((*s).hwnd, null_mut(), (&mut client as *mut RECT).cast(), 2);
    // (The layered window also holds the card's shadow, deeper below.)
    popup::settle(dialog);
    let mut card: RECT = zeroed();
    GetWindowRect(dialog, &mut card);
    let centre = |a: i32, b: i32| (a + b) / 2;
    assert!((centre(card.left, card.right) - centre(client.left, client.right)).abs() <= 2);
    assert!(card.top > client.top && card.bottom < client.bottom);
    popup::destroy(dialog);
    popup::destroy(scrim);
    close(owner);
}

/// Open the window shown (so it counts as active and its controls take the
/// focus) without appearing on screen: transparent, not in the taskbar.
#[cfg(test)]
unsafe fn show_invisibly(owner: *mut App) -> *mut State {
    create(owner, false);
    let s = owner_state(owner);
    assert!(!s.is_null());
    let hwnd = (*s).hwnd;
    let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & !WS_EX_APPWINDOW;
    SetWindowLongPtrW(
        hwnd,
        GWL_EXSTYLE,
        (style | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) as isize,
    );
    SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA);
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    s
}

/// [`show_invisibly`] with tracing on.
#[cfg(test)]
pub(super) unsafe fn show_traced(owner: *mut App) {
    let s = show_invisibly(owner);
    (*s).detailed = true;
    changed(s, true);
}

#[cfg(test)]
pub(super) unsafe fn assert_keyboard_focus(owner: *mut App) {
    let s = show_invisibly(owner);
    let hwnd = (*s).hwnd;
    // Alt alone is consumed: DefWindowProc would enter an invisible menu
    // mode that eats the next key.
    assert_eq!(
        chrome::message(s, hwnd, WM_SYSCOMMAND, SC_KEYMENU as usize, 0),
        Some(0)
    );
    // A short window: the last panel starts below the visible list.
    super::fit_client(hwnd, 780, 560);
    view::layout(s);
    (*s).scroll = 0;
    view::layout_body(s);
    let last = (*s).panels.last().unwrap();
    let (header, table) = (last.header, last.table);
    let mut body: RECT = zeroed();
    GetClientRect((*s).body, &mut body);
    assert!(
        last.bounds.bottom > body.bottom,
        "precondition: out of view"
    );
    SetFocus(header);
    let tab = MSG {
        hwnd: header,
        message: WM_KEYDOWN,
        wParam: VK_TAB as usize,
        ..zeroed()
    };
    assert!(keyboard(owner, &tab));
    assert_eq!(GetFocus(), table, "Tab moves from the header to its table");
    let mut shown: RECT = zeroed();
    GetWindowRect(table, &mut shown);
    MapWindowPoints(null_mut(), (*s).body, (&mut shown as *mut RECT).cast(), 2);
    assert!(
        shown.top >= 0 && shown.top < body.bottom,
        "the focused table is scrolled into view: {} in 0..{}",
        shown.top,
        body.bottom
    );
    assert!((*s).scroll > 0);
    // Deactivating keeps the focused control; activating returns to it.
    SendMessageW(hwnd, WM_ACTIVATE, WA_INACTIVE as usize, 0);
    SetFocus((*s).search);
    SendMessageW(hwnd, WM_ACTIVATE, WA_ACTIVE as usize, 0);
    assert_eq!(GetFocus(), table);
    // A control that no longer exists falls back to the current tab.
    (*s).focus = null_mut();
    SendMessageW(hwnd, WM_ACTIVATE, WA_ACTIVE as usize, 0);
    assert_eq!(GetFocus(), (*s).nav[(*s).tab as usize]);
    close(owner);
}

/// A kept frame's reasons follow a language change on the next rebuild:
/// the collectors report codes, translated only when shown.
#[cfg(test)]
pub(super) unsafe fn assert_reasons_follow_the_language(owner: *mut App) {
    use crate::i18n::{with_language, Language};
    create(owner, false);
    let s = owner_state(owner);
    assert!(!s.is_null());
    set_tab(s, Tab::Disk);
    (*s).detailed = true;
    (*s).files = Some(Arc::new(crate::fileetw::Sample {
        enabled: true,
        reason: Some("Preparing file I/O requests".into()),
        ..Default::default()
    }));
    let shown = |language| {
        with_language(language, || {
            content::rebuild(s, Instant::now());
            let panel = (*s).panels.iter().find(|p| p.kind == Kind::Files).unwrap();
            (panel.summary.clone(), panel.empty.clone())
        })
    };
    let korean = "파일 I/O 요청을 준비하는 중입니다";
    assert_eq!(shown(Language::Korean), (korean.into(), korean.into()));
    let english = "Preparing file I/O requests";
    assert_eq!(shown(Language::English), (english.into(), english.into()));
    close(owner);
}

#[cfg(test)]
pub(super) unsafe fn assert_rows_keep_identity(owner: *mut App) {
    create(owner, false);
    let s = owner_state(owner);
    assert!(!s.is_null());
    let service = |name: &str| Service {
        name: name.into(),
        display_name: name.into(),
        state: SERVICE_RUNNING,
        pid: 0,
        start_type: None,
    };
    set_tab(s, Tab::Cpu);
    (*s).services = vec![service("Alpha"), service("Bravo")];
    content::rebuild(s, Instant::now());
    let index = (*s)
        .panels
        .iter()
        .position(|p| p.kind == Kind::Services)
        .unwrap();
    let table = (&(*s).panels)[index].table;
    let item = LVITEMW {
        stateMask: LVIS_SELECTED | LVIS_FOCUSED,
        state: LVIS_SELECTED | LVIS_FOCUSED,
        ..zeroed()
    };
    SendMessageW(table, LVM_SETITEMSTATE, 1, &item as *const LVITEMW as isize);
    // A service sorted before the selected one moves its row: the
    // selection stays on Bravo instead of on row 1.
    (*s).services.insert(0, service("Aardvark"));
    content::rebuild(s, Instant::now());
    let selected = SendMessageW(table, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    assert_eq!(selected, 2);
    assert_eq!((&(*s).panels)[index].rows[2].cells[0].text, "Bravo");
    // Rows the collectors deliver in hash-map order, with equal rates, keep
    // one order on every refresh.
    set_tab(s, Tab::Disk);
    (*s).detailed = true;
    let process = (*s).snapshot.as_ref().unwrap().processes[0].clone();
    let file = |path: &&str| crate::fileetw::Row {
        pid: process.pid,
        created: process.created,
        path: (*path).into(),
        read_bytes_per_sec: 10.0,
        write_bytes_per_sec: 0.0,
    };
    for order in [["b.log", "a.log", "c.log"], ["c.log", "a.log", "b.log"]] {
        (*s).files = Some(Arc::new(crate::fileetw::Sample {
            enabled: true,
            measured: true,
            interval_seconds: 1.0,
            rows: order.iter().map(file).collect(),
            ..Default::default()
        }));
        content::rebuild(s, Instant::now());
        let panel = (*s).panels.iter().find(|p| p.kind == Kind::Files).unwrap();
        let paths: Vec<_> = panel
            .rows
            .iter()
            .map(|r| r.cells[2].text.as_str())
            .collect();
        assert_eq!(paths, ["a.log", "b.log", "c.log"]);
        assert!(panel.columns.len() == 6 && panel.rows.iter().all(|r| r.cells.len() == 6));
    }
    close(owner);
}

#[cfg(test)]
pub(super) unsafe fn assert_snapshot_lifecycle(owner: *mut App) {
    create(owner, false);
    let s = owner_state(owner);
    assert!(!s.is_null());
    assert_eq!(paint::window_text((*s).hwnd), "Feather Resource Monitor");
    let first = (*s).snapshot.clone().expect("shared initial frame");
    assert!(Arc::ptr_eq(&first, (*owner).snapshot.as_ref().unwrap()));
    let process = first.processes.first().unwrap();
    let id = (process.pid, process.created);
    (*s).checked.insert(id);
    set_rate(s, 0);
    let mut next = (*first).clone();
    next.processes[0].created += 1;
    let at = (*s).accepted_at.unwrap() + Duration::from_secs(2);
    (*owner).resource_snapshot = Some(Arc::new(next));
    (*owner).resource_sample_at = Some(at);
    refresh(owner, at);
    assert!(
        Arc::ptr_eq(&first, (*s).snapshot.as_ref().unwrap()),
        "paused frame stays frozen"
    );
    assert!((*s).checked.contains(&id));
    set_rate(s, 1000);
    refresh(owner, at);
    assert!(
        !(*s).checked.contains(&id),
        "a reused PID cannot inherit selection"
    );
    assert!(!Arc::ptr_eq(&first, (*s).snapshot.as_ref().unwrap()));
    // Rows of one process (here: two modules) keep distinct accessible keys,
    // so moving between them is announced; process rows stay keyed by process.
    {
        use table::Model as _;
        let panels = &mut (*s).panels;
        let index = panels.iter().position(|p| !p.kind.check()).unwrap();
        let saved = std::mem::take(&mut panels[index].rows);
        panels[index].rows = ["a.dll", "b.dll"]
            .iter()
            .map(|name| Row {
                identity: Some(id),
                key: content::row_key((id, *name)),
                cells: vec![Cell::text(*name)],
            })
            .collect();
        let model = content::Model(s, index);
        assert_ne!(model.key(0), model.key(1));
        (&mut (*s).panels)[index].rows = saved;
    }
    // Moving to a 150 % monitor keeps the client proportional instead of
    // letting Windows add a native caption's height to the captionless window.
    let (base, target) = ((*s).dpi, (*s).dpi * 3 / 2);
    let mut client: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut client);
    let mut size = SIZE { cx: 0, cy: 0 };
    assert_eq!(
        SendMessageW(
            (*s).hwnd,
            WM_GETDPISCALEDSIZE,
            target as usize,
            &mut size as *mut _ as isize
        ),
        1
    );
    let scale = |v: i32| {
        ((i64::from(v) * i64::from(target) + i64::from(base) / 2) / i64::from(base)) as i32
    };
    assert_eq!(
        (size.cx, size.cy),
        frame::outer_size(
            target as u32,
            scale(client.right),
            scale(client.bottom),
            (*s).chrome.top
        )
    );
    close(owner);
    assert!((*owner).resource_window.is_null());
    assert!((*owner).resource_data.is_none());
    assert!((*owner).resource_files.is_none());
    assert!((*owner).resource_network.is_none());
    assert_eq!(
        (*owner).resource_request,
        (crate::resource::Request::default(), false, false)
    );
}
