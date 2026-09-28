//! Stable process column identities and bounded, opt-in layout preferences.
use super::*;
use windows_sys::Win32::System::Registry::*;

const PATH: &str = r"Software\FeatherTask\Preferences";
const VALUE: &str = "ProcessColumnsV1";
/// Menu command of a shown column's action: `ACTIONS + index * 8 + action`.
const ACTIONS: usize = 200;
/// One Wider / Narrower step (DIP).
const WIDTH_STEP: f32 = 16.0;

/// What the column menu does to one shown column: the keyboard's way to
/// drag a header or its edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    MoveLeft,
    MoveRight,
    Wider,
    Narrower,
    ResetWidth,
    Hide,
}
impl Action {
    const ALL: [Self; 6] = [
        Self::MoveLeft,
        Self::MoveRight,
        Self::Wider,
        Self::Narrower,
        Self::ResetWidth,
        Self::Hide,
    ];
    fn label(self) -> &'static str {
        match self {
            Self::MoveLeft => tr("왼쪽으로 이동", "Move left"),
            Self::MoveRight => tr("오른쪽으로 이동", "Move right"),
            Self::Wider => tr("너비 늘리기", "Wider"),
            Self::Narrower => tr("너비 줄이기", "Narrower"),
            Self::ResetWidth => tr("열 너비 초기화", "Reset column width"),
            Self::Hide => tr("이 열 숨기기", "Hide this column"),
        }
    }
    fn command(self, index: usize) -> usize {
        ACTIONS + index * 8 + self as usize
    }
    fn from_command(command: usize) -> Option<(usize, Self)> {
        let offset = command.checked_sub(ACTIONS)?;
        Some((offset / 8, *Self::ALL.get(offset % 8)?))
    }
}

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
    /// Whether `action` changes the shown column `index` (`width`: its drawn
    /// width in DIP, since a flexible column stores none).
    fn allows(&self, index: usize, action: Action, width: f32) -> bool {
        let Some(id) = self.at(index) else {
            return false;
        };
        match action {
            Action::MoveLeft => index > 1,
            Action::MoveRight => index > 0 && index + 1 < self.shown().count(),
            Action::Wider => width < 800.0,
            Action::Narrower => width > 48.0,
            Action::ResetWidth => true,
            Action::Hide => id != Name,
        }
    }
    /// Apply `action` to the shown column `index`; false when it does nothing.
    fn apply(&mut self, index: usize, action: Action, width: f32) -> bool {
        if !self.allows(index, action, width) {
            return false;
        }
        let Some(id) = self.at(index) else {
            return false;
        };
        match action {
            Action::MoveLeft => self.reorder(index, index - 1),
            Action::MoveRight => self.reorder(index, index + 1),
            Action::Wider => self.resize(id, width + WIDTH_STEP),
            Action::Narrower => self.resize(id, width - WIDTH_STEP),
            Action::ResetWidth => {
                if let Some(entry) = self.entries.iter_mut().find(|e| e.id == id) {
                    entry.width = id.width();
                }
            }
            Action::Hide => self.toggle(id),
        }
        true
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
/// The drawn width (DIP) of the shown column `index` of the process table.
unsafe fn drawn_width(p: *mut App, index: usize) -> f32 {
    let px = SendMessageW((*p).list, LVM_GETCOLUMNWIDTH, index, 0) as f32;
    px * 96.0 / (*p).dpi.max(1) as f32
}
/// Append the shown column `index`'s actions to `menu`.
unsafe fn add_actions(p: *mut App, menu: HMENU, index: usize) {
    let width = drawn_width(p, index);
    for action in Action::ALL {
        if action == Action::Hide && (*p).process_columns.at(index) == Some(Name) {
            continue;
        }
        let enabled = (*p).process_columns.allows(index, action, width);
        AppendMenuW(
            menu,
            MF_STRING | if enabled { 0 } else { MF_GRAYED },
            action.command(index),
            wide(action.label()).as_ptr(),
        );
    }
}
/// The column menu: the column toggles, then the actions of `target` (the
/// header that was pointed at) or, opened from the keyboard (Ctrl+Shift+C,
/// ⋯ → Choose columns), an Edit column submenu with every shown column.
pub(super) unsafe fn menu(p: *mut App, target: Option<usize>, at: POINT) {
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
    match target {
        Some(index) => {
            if (*p).process_columns.at(index).is_some() {
                add_actions(p, menu, index);
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
            }
        }
        None => {
            let edit = CreatePopupMenu();
            if !edit.is_null() {
                let shown: Vec<_> = (*p).process_columns.shown().collect();
                for (index, id) in shown.into_iter().enumerate() {
                    let actions = CreatePopupMenu();
                    if !actions.is_null() {
                        add_actions(p, actions, index);
                        AppendMenuW(edit, MF_POPUP, actions as usize, wide(id.label()).as_ptr());
                    }
                }
                AppendMenuW(
                    menu,
                    MF_POPUP,
                    edit as usize,
                    wide(tr("열 편집", "Edit column")).as_ptr(),
                );
                AppendMenuW(menu, MF_SEPARATOR, 0, null());
            }
        }
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
    // Destroys the submenus with it.
    DestroyMenu(menu);
    interactions::finish_modal(p);
    restore_focus(p, previous);
    match command {
        1..=18 => (*p).process_columns.toggle(ALL[command - 1]),
        105 => (*p).process_columns = Config::default(),
        _ => {
            let Some((index, action)) = Action::from_command(command) else {
                return;
            };
            let width = drawn_width(p, index);
            if !(*p).process_columns.apply(index, action, width) {
                return;
            }
        }
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
    #[test]
    fn keyboard_actions_reorder_resize_and_hide_the_targeted_column() {
        let mut config = Config::default();
        assert_eq!(
            Action::from_command(Action::Narrower.command(3)),
            Some((3, Action::Narrower))
        );
        assert_eq!(Action::from_command(ACTIONS + 6), None);
        assert_eq!(Action::from_command(105), None);
        // Memory (index 3) moves both ways; Name stays first.
        assert!(config.apply(3, Action::MoveLeft, 112.0));
        assert_eq!(config.at(2), Some(Memory));
        assert!(config.apply(2, Action::MoveRight, 112.0));
        assert_eq!(config.at(3), Some(Memory));
        assert!(!config.apply(1, Action::MoveLeft, 80.0));
        assert!(!config.apply(0, Action::MoveRight, 300.0));
        let last = config.shown().count() - 1;
        assert!(!config.apply(last, Action::MoveRight, 80.0));
        // Width steps start from the drawn width, also for the flexible Name.
        assert!(config.apply(3, Action::Wider, 112.0));
        assert_eq!(config.columns()[3].width, 128.0);
        assert!(config.apply(0, Action::Narrower, 300.0));
        assert_eq!(config.columns()[0].width, 284.0);
        assert!(!config.columns()[0].flex);
        assert!(!config.apply(3, Action::Wider, 800.0));
        assert!(!config.apply(3, Action::Narrower, 48.0));
        assert!(config.apply(0, Action::ResetWidth, 284.0));
        assert!(config.columns()[0].flex);
        // Hide removes only the targeted column; Name cannot be hidden.
        assert!(!config.apply(0, Action::Hide, 300.0));
        assert!(config.apply(3, Action::Hide, 128.0));
        assert!(!config.contains(Memory));
        assert!(!config.apply(ALL.len(), Action::Wider, 100.0));
    }
}
