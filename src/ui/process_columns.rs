//! Stable process column identities and bounded, opt-in layout preferences.
use super::*;
use windows_sys::Win32::System::Registry::*;

const PATH: &str = r"Software\FeatherTask\Preferences";
const VALUE: &str = "ProcessColumnsV1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(super) enum ProcessColumn {
    Name,
    Pid,
    Cpu,
    Memory,
    AllIo,
    Network,
    Gpu,
    Status,
    User,
    Session,
    CommandLine,
    CpuTime,
    Threads,
    Handles,
    PrivateBytes,
    GpuEngine,
    DedicatedGpu,
    SharedGpu,
}
use ProcessColumn::*;
pub(super) const ALL: [ProcessColumn; 18] = [
    Name,
    Pid,
    Cpu,
    Memory,
    AllIo,
    Network,
    Gpu,
    Status,
    User,
    Session,
    CommandLine,
    CpuTime,
    Threads,
    Handles,
    PrivateBytes,
    GpuEngine,
    DedicatedGpu,
    SharedGpu,
];

impl ProcessColumn {
    pub fn from_id(id: usize) -> Option<Self> {
        ALL.get(id).copied()
    }
    pub fn label(self) -> &'static str {
        match self {
            Name => tr("이름", "Name"),
            Pid => "PID",
            Cpu => "CPU",
            Memory => tr("메모리", "Memory"),
            AllIo => tr("전체 I/O", "All I/O"),
            Network => tr("네트워크", "Network"),
            Gpu => "GPU",
            Status => tr("상태", "Status"),
            User => tr("사용자", "User"),
            Session => tr("세션 ID", "Session ID"),
            CommandLine => tr("명령줄", "Command line"),
            CpuTime => tr("CPU 시간", "CPU time"),
            Threads => tr("스레드", "Threads"),
            Handles => tr("핸들", "Handles"),
            PrivateBytes => tr("전용 바이트", "Private bytes"),
            GpuEngine => tr("GPU 엔진", "GPU engine"),
            DedicatedGpu => tr("전용 GPU 메모리", "Dedicated GPU memory"),
            SharedGpu => tr("공유 GPU 메모리", "Shared GPU memory"),
        }
    }
    pub fn numeric(self) -> bool {
        !matches!(self, Name | Status | User | CommandLine | GpuEngine)
    }
    fn width(self) -> u32 {
        match self {
            Name => 0,
            Pid | Gpu => 80,
            Cpu => 92,
            Memory => 112,
            AllIo => 100,
            Network => 120,
            CommandLine => 320,
            User | GpuEngine => 180,
            DedicatedGpu | SharedGpu => 180,
            Status => 150,
            _ => 120,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    id: ProcessColumn,
    width: u32,
    visible: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Config {
    entries: Vec<Entry>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            entries: ALL
                .into_iter()
                .map(|id| Entry {
                    id,
                    width: id.width(),
                    visible: (id as usize) <= Gpu as usize,
                })
                .collect(),
        }
    }
}
impl Config {
    pub fn contains(&self, id: ProcessColumn) -> bool {
        self.entries.iter().any(|e| e.id == id && e.visible)
    }
    pub fn shown(&self) -> impl Iterator<Item = ProcessColumn> + '_ {
        self.entries.iter().filter(|e| e.visible).map(|e| e.id)
    }
    pub fn at(&self, index: usize) -> Option<ProcessColumn> {
        self.shown().nth(index)
    }
    pub fn columns(&self) -> Vec<table::Column> {
        self.entries
            .iter()
            .filter(|e| e.visible)
            .map(|e| table::Column {
                label: e.id.label().into(),
                width: e.width as f32,
                flex: e.width == 0,
                right: e.id.numeric(),
            })
            .collect()
    }
    pub(super) fn toggle(&mut self, id: ProcessColumn) {
        if id == Name {
            return;
        }
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.visible = !entry.visible;
        }
    }
    fn resize(&mut self, id: ProcessColumn, width: f32) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
            entry.width = width.round().clamp(48.0, 800.0) as u32;
        }
    }
    fn reorder(&mut self, from: usize, to: usize) {
        let (Some(source), Some(target)) = (self.at(from), self.at(to)) else {
            return;
        };
        if source == Name || target == Name || from == to {
            return;
        }
        let src = self.entries.iter().position(|e| e.id == source).unwrap();
        let dst = self.entries.iter().position(|e| e.id == target).unwrap();
        let entry = self.entries.remove(src);
        self.entries.insert(dst, entry);
    }
    fn encode(&self) -> Vec<u32> {
        let mut words = vec![1];
        for entry in &self.entries {
            words.extend([entry.id as u32, entry.width, u32::from(entry.visible)]);
        }
        words
    }
    fn decode(words: &[u32]) -> Option<Self> {
        if words.len() != 1 + ALL.len() * 3 || words[0] != 1 {
            return None;
        }
        let mut entries = Vec::with_capacity(ALL.len());
        for chunk in words[1..].as_chunks::<3>().0 {
            let id = ProcessColumn::from_id(chunk[0] as usize)?;
            if entries.iter().any(|e: &Entry| e.id == id)
                || !(48..=800).contains(&chunk[1]) && !(id == Name && chunk[1] == 0)
                || chunk[2] > 1
            {
                return None;
            }
            entries.push(Entry {
                id,
                width: chunk[1],
                visible: chunk[2] == 1,
            });
        }
        if entries[0].id != Name || !entries[0].visible {
            return None;
        }
        Some(Self { entries })
    }
    pub fn load() -> Self {
        let read = || -> Option<Self> {
            let key = crate::registry::open(HKEY_CURRENT_USER, PATH, KEY_QUERY_VALUE).ok()??;
            let mut words = [0u32; 55];
            let mut bytes = size_of::<[u32; 55]>() as u32;
            let code = unsafe {
                RegGetValueW(
                    key.0,
                    null(),
                    wide(VALUE).as_ptr(),
                    RRF_RT_REG_BINARY,
                    null_mut(),
                    words.as_mut_ptr().cast(),
                    &mut bytes,
                )
            };
            if code != 0 || bytes as usize != size_of::<[u32; 55]>() {
                return None;
            }
            Self::decode(&words)
        };
        read().unwrap_or_default()
    }
    fn save(&self) -> Result<(), String> {
        let key = crate::registry::create(HKEY_CURRENT_USER, PATH, KEY_SET_VALUE).map_err(|e| {
            format!(
                "{} (Windows {e})",
                tr("열 설정을 저장할 수 없습니다", "Cannot save column layout")
            )
        })?;
        let words = self.encode();
        let code = unsafe {
            RegSetValueExW(
                key.0,
                wide(VALUE).as_ptr(),
                0,
                REG_BINARY,
                words.as_ptr().cast(),
                (words.len() * 4) as u32,
            )
        };
        if code == 0 {
            Ok(())
        } else {
            Err(format!(
                "{} (Windows {code})",
                tr("열 설정을 저장할 수 없습니다", "Cannot save column layout")
            ))
        }
    }
}

pub(super) fn status(process: &Process) -> String {
    let mut parts = Vec::new();
    if process.responsiveness == Some(false) {
        parts.push(tr("응답 없음", "Not responding"));
    }
    if process.suspended == Some(true) {
        parts.push(tr("일시 중단", "Suspended"));
    }
    if process.efficiency == Some(true) {
        parts.push(tr("효율성 모드", "Efficiency mode"));
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" · ")
    }
}
pub(super) fn text(process: &Process, column: ProcessColumn) -> String {
    let mb = |bytes: u64| format!("{} MB", thousands(bytes as f64 / 1048576.0));
    match column {
        Name => process.name.clone(),
        Pid => process.pid.to_string(),
        Cpu => format!("{:.1}%", process.cpu_percent),
        Memory => mb(process.working_set),
        AllIo => format!("{}/s", rate(process.io_bytes_per_sec)),
        Network => process
            .network_bytes_per_sec
            .map_or_else(|| "—".into(), mbps),
        Gpu => process
            .gpu_percent
            .map_or_else(|| "—".into(), |v| format!("{v:.1}%")),
        Status => status(process),
        User => process.user_name.clone().unwrap_or_else(|| "—".into()),
        Session => process.session_id.to_string(),
        CommandLine => process.command_line.clone().unwrap_or_else(|| "—".into()),
        CpuTime => {
            let seconds = process.cpu_time_100ns / 10_000_000;
            format!(
                "{}:{:02}:{:02}",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )
        }
        Threads => process.threads.to_string(),
        Handles => process.handles.to_string(),
        PrivateBytes => mb(process.private_bytes),
        GpuEngine => process.gpu_engine.clone().unwrap_or_else(|| "—".into()),
        DedicatedGpu => process.gpu_dedicated_bytes.map_or_else(|| "—".into(), mb),
        SharedGpu => process.gpu_shared_bytes.map_or_else(|| "—".into(), mb),
    }
}
pub(super) fn compare(a: &Process, b: &Process, column: ProcessColumn) -> Ordering {
    match column {
        Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        Pid => a.pid.cmp(&b.pid),
        Cpu => a.cpu_percent.total_cmp(&b.cpu_percent),
        Memory => a.working_set.cmp(&b.working_set),
        AllIo => a.io_bytes_per_sec.total_cmp(&b.io_bytes_per_sec),
        Network => option_cmp(a.network_bytes_per_sec, b.network_bytes_per_sec),
        Gpu => option_cmp(a.gpu_percent, b.gpu_percent),
        Status => status(a).cmp(&status(b)),
        User => a.user_name.cmp(&b.user_name),
        Session => a.session_id.cmp(&b.session_id),
        CommandLine => a.command_line.cmp(&b.command_line),
        CpuTime => a.cpu_time_100ns.cmp(&b.cpu_time_100ns),
        Threads => a.threads.cmp(&b.threads),
        Handles => a.handles.cmp(&b.handles),
        PrivateBytes => a.private_bytes.cmp(&b.private_bytes),
        GpuEngine => a.gpu_engine.cmp(&b.gpu_engine),
        DedicatedGpu => a.gpu_dedicated_bytes.cmp(&b.gpu_dedicated_bytes),
        SharedGpu => a.gpu_shared_bytes.cmp(&b.gpu_shared_bytes),
    }
}

unsafe fn changed(p: *mut App) {
    if !(*p)
        .process_columns
        .contains(ProcessColumn::from_id((*p).sort).unwrap_or(Name))
    {
        (*p).sort = Name as usize;
        (*p).descending = false;
    }
    table::set_columns((*p).list, shown_columns(p));
    configure(p);
    if (*p).persist_preferences {
        if let Err(error) = (*p).process_columns.save() {
            (*p).set_error(ErrorSource::Action, error);
        }
    }
    rebuild(p, selected_identity(p));
    redraw(p);
}
pub(super) unsafe fn resize(p: *mut App, index: usize, width: f32) {
    if let Some(id) = (*p).process_columns.at(index) {
        (*p).process_columns.resize(id, width);
        changed(p);
    }
}
pub(super) unsafe fn reorder(p: *mut App, from: usize, to: usize) {
    (*p).process_columns.reorder(from, to);
    changed(p);
}
pub(super) unsafe fn menu(p: *mut App, index: usize, at: POINT) {
    if (*p).modal {
        return;
    }
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let add = |parent: HMENU, id: usize, label: &str, flags: u32| {
        AppendMenuW(parent, MF_STRING | flags, id, wide(label).as_ptr());
    };
    for (label, ids) in [
        (tr("기본 열", "Basic columns"), &ALL[..8]),
        (tr("프로세스 상세", "Process details"), &ALL[8..15]),
        (tr("GPU 상세", "GPU details"), &ALL[15..]),
    ] {
        let child = CreatePopupMenu();
        if child.is_null() {
            continue;
        }
        for id in ids {
            add(
                child,
                *id as usize + 1,
                id.label(),
                if *id == Name {
                    MF_CHECKED | MF_GRAYED
                } else if (*p).process_columns.contains(*id) {
                    MF_CHECKED
                } else {
                    0
                },
            );
        }
        AppendMenuW(menu, MF_POPUP, child as usize, wide(label).as_ptr());
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    if let Some(id) = (*p).process_columns.at(index) {
        add(
            menu,
            101,
            tr("왼쪽으로 이동", "Move left"),
            if index > 1 { 0 } else { MF_GRAYED },
        );
        add(
            menu,
            102,
            tr("오른쪽으로 이동", "Move right"),
            if index > 0 && index + 1 < (*p).process_columns.shown().count() {
                0
            } else {
                MF_GRAYED
            },
        );
        add(menu, 103, tr("열 너비 초기화", "Reset column width"), 0);
        if id != Name {
            add(menu, 104, tr("이 열 숨기기", "Hide this column"), 0);
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
    }
    add(
        menu,
        105,
        tr("모든 열 설정 초기화", "Reset column layout"),
        0,
    );
    let previous = GetFocus();
    (*p).modal = true;
    configure(p);
    let command = popup::track_menu(p, menu, popup::Anchor::Point(at));
    DestroyMenu(menu);
    interactions::finish_modal(p);
    restore_focus(p, previous);
    match command {
        1..=18 => (*p).process_columns.toggle(ALL[command - 1]),
        101 => (*p).process_columns.reorder(index, index.saturating_sub(1)),
        102 => (*p).process_columns.reorder(index, index + 1),
        103 => {
            if let Some(id) = (*p).process_columns.at(index) {
                if let Some(e) = (*p).process_columns.entries.iter_mut().find(|e| e.id == id) {
                    e.width = id.width();
                }
            }
        }
        104 => {
            if let Some(id) = (*p).process_columns.at(index) {
                (*p).process_columns.toggle(id);
            }
        }
        105 => (*p).process_columns = Config::default(),
        _ => return,
    }
    changed(p);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preferences_preserve_identity_order_width_and_reject_invalid_data() {
        let mut config = Config::default();
        config.toggle(User);
        config.toggle(DedicatedGpu);
        config.resize(User, 290.0);
        config.reorder(7, 2);
        assert_eq!(config.at(2), Some(User));
        assert_eq!(Config::decode(&config.encode()), Some(config.clone()));
        config.reorder(2, 0);
        config.toggle(Name);
        assert_eq!(config.at(0), Some(Name));
        let mut invalid = config.encode();
        invalid[1] = 1;
        assert!(Config::decode(&invalid).is_none());
        let mut invalid = config.encode();
        invalid[2] = u32::MAX;
        assert!(Config::decode(&invalid).is_none());
        assert!(Config::decode(&[1, 0]).is_none());
    }
}
