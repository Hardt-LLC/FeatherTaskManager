//! On-demand native modal dialog; no polling or retained command history. The
//! program is resolved on the app's one check thread (a one-shot timer only
//! labels a slow check), so a slow drive never blocks the dialog or the app's
//! windows, and a stalled one never accumulates threads.
use super::*;
use crate::actions::TaskLaunch;
use windows_sys::Win32::{
    Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE},
    UI::Controls::Dialogs::*,
};

const PROGRAM: i32 = 701;
const ARGUMENTS: i32 = 702;
const BROWSE: i32 = 703;
const ADMIN: i32 = 704;
const ERROR: i32 = 705;
/// A program check finished (its result waits in `State::checked`).
const CHECKED: u32 = WM_APP + 0x70;
/// Shows [`checking_text`] when a check takes longer than `CHECK_LABEL_MS`.
const CHECK_TIMER: usize = 0x7A5;
const CHECK_LABEL_MS: u32 = 250;

type Checked = (u64, Result<(), String>);

/// Set when a request is withdrawn (its dialog was edited or closed).
type Cancelled = Arc<std::sync::atomic::AtomicBool>;

/// One program check and what to do with its result.
struct Check {
    task: TaskLaunch,
    cancelled: Cancelled,
    done: Box<dyn FnOnce(Result<(), String>) + Send>,
}

#[derive(Default)]
struct Queue {
    /// The check thread exists (it exits when nothing waits).
    running: bool,
    /// The newest request behind the one being checked. A newer request
    /// replaces it: it came from an edited, re-run or closed dialog.
    waiting: Option<Check>,
}

/// Program checks, app-wide: one thread at most, however slow a drive is
/// and however often the dialog is re-run or reopened meanwhile.
#[derive(Default)]
struct Checks {
    queue: std::sync::Mutex<Queue>,
    /// Tests stand in for a slow drive by delaying each check.
    #[cfg(test)]
    delay_ms: std::sync::atomic::AtomicU64,
    /// Threads started (tests).
    #[cfg(test)]
    started: std::sync::atomic::AtomicUsize,
}

impl Checks {
    fn queue(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn submit(self: &Arc<Self>, check: Check) -> Result<(), String> {
        let mut queue = self.queue();
        queue.waiting = Some(check);
        if queue.running {
            return Ok(());
        }
        queue.running = true;
        let checks = Arc::clone(self);
        match std::thread::Builder::new()
            .name("feather-run-task-check".into())
            .spawn(move || checks.work())
        {
            Ok(_) => {
                #[cfg(test)]
                self.started
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Err(error) => {
                queue.running = false;
                queue.waiting = None;
                Err(error.to_string())
            }
        }
    }
    /// Withdraw a request: one that has not started leaves the queue now, and
    /// the thread skips it if it was just taken. A running check cannot be
    /// interrupted; its result is ignored.
    fn withdraw(&self, cancelled: &Cancelled) {
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        let mut queue = self.queue();
        if queue
            .waiting
            .as_ref()
            .is_some_and(|check| Arc::ptr_eq(&check.cancelled, cancelled))
        {
            queue.waiting = None;
        }
    }
    fn work(&self) {
        loop {
            let Some(check) = self.queue().waiting.take() else {
                // Checked under the lock: a request submitted now starts a
                // new thread instead of waiting for this one.
                let mut queue = self.queue();
                if queue.waiting.is_none() {
                    queue.running = false;
                    return;
                }
                continue;
            };
            if check.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            #[cfg(test)]
            std::thread::sleep(Duration::from_millis(
                self.delay_ms.load(std::sync::atomic::Ordering::Relaxed),
            ));
            (check.done)(crate::actions::check_task(&check.task));
        }
    }
}

fn checks() -> &'static Arc<Checks> {
    static CHECKS: std::sync::OnceLock<Arc<Checks>> = std::sync::OnceLock::new();
    CHECKS.get_or_init(Default::default)
}

/// No request waits for the check thread (tests).
#[cfg(test)]
pub(super) fn nothing_waits() -> bool {
    checks().queue().waiting.is_none()
}

#[cfg(test)]
pub(super) fn set_check_delay(delay: Duration) {
    checks().delay_ms.store(
        delay.as_millis() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
}

struct State {
    app: *mut App,
    fonts: fonts::Fonts,
    dpi: i32,
    surface: HBRUSH,
    /// The administrator switch (owner-drawn; always on when Feather is elevated).
    admin: bool,
    result: Option<TaskLaunch>,
    /// The Run request whose program is being resolved. An edit or closing
    /// the dialog withdraws it (a late result for it is then ignored).
    checking: Option<Pending>,
    requests: u64,
    checked: (mpsc::Sender<Checked>, mpsc::Receiver<Checked>),
}

struct Pending {
    request: u64,
    task: TaskLaunch,
    cancelled: Cancelled,
}

impl Drop for State {
    fn drop(&mut self) {
        // Closed (Cancel, Esc, the close button) while a check waits: it
        // must not hold the check thread for a dialog that is gone.
        if let Some(pending) = self.checking.take() {
            checks().withdraw(&pending.cancelled);
        }
        unsafe {
            DeleteObject(self.surface);
        }
    }
}

/// The error line while a program check is slow (muted, not an error).
pub(super) fn checking_text() -> &'static str {
    tr("프로그램을 확인하는 중…", "Checking the program…")
}

/// Forget the check in flight: the fields it was made for changed.
unsafe fn discard_check(hwnd: HWND, state: &mut State) {
    if let Some(pending) = state.checking.take() {
        checks().withdraw(&pending.cancelled);
        KillTimer(hwnd, CHECK_TIMER);
        SetDlgItemTextW(hwnd, ERROR, wide("").as_ptr());
    }
}

/// Run: the command's syntax is checked at once; resolving the program
/// searches folders and reads the file system, so it runs on the check
/// thread and its result arrives as [`CHECKED`]. Cancel works meanwhile.
unsafe fn start_check(hwnd: HWND, state: &mut State) {
    if state.checking.is_some() {
        return;
    }
    let task = TaskLaunch {
        command: read(hwnd, PROGRAM),
        arguments: read(hwnd, ARGUMENTS),
        elevated: state.admin,
    };
    if let Err(error) = crate::actions::task_command(&task) {
        SetDlgItemTextW(hwnd, ERROR, wide(&error).as_ptr());
        SetFocus(GetDlgItem(hwnd, PROGRAM));
        return;
    }
    state.requests += 1;
    let request = state.requests;
    let sender = state.checked.0.clone();
    let dialog = hwnd as usize;
    let cancelled = Cancelled::default();
    let started = checks().submit(Check {
        task: task.clone(),
        cancelled: Arc::clone(&cancelled),
        done: Box::new(move |result| {
            // A closed dialog dropped the receiver: nothing is posted.
            if sender.send((request, result)).is_ok() {
                unsafe { PostMessageW(dialog as HWND, CHECKED, 0, 0) };
            }
        }),
    });
    match started {
        Ok(_) => {
            state.checking = Some(Pending {
                request,
                task,
                cancelled,
            });
            SetDlgItemTextW(hwnd, ERROR, wide("").as_ptr());
            SetTimer(hwnd, CHECK_TIMER, CHECK_LABEL_MS, None);
        }
        Err(error) => {
            SetDlgItemTextW(
                hwnd,
                ERROR,
                wide(&tf!(
                    "프로그램을 확인할 수 없습니다: {}",
                    "Cannot check the program: {}",
                    error
                ))
                .as_ptr(),
            );
        }
    }
}

/// Results of finished checks: the current one closes the dialog with its
/// task or shows why the program was rejected; discarded ones are dropped.
unsafe fn finish_checks(hwnd: HWND, state: &mut State) {
    while let Ok((request, result)) = state.checked.1.try_recv() {
        if state
            .checking
            .as_ref()
            .is_none_or(|pending| pending.request != request)
        {
            continue;
        }
        let Some(Pending { task, .. }) = state.checking.take() else {
            continue;
        };
        KillTimer(hwnd, CHECK_TIMER);
        match result {
            Ok(()) => {
                state.result = Some(task);
                EndDialog(hwnd, IDOK as isize);
            }
            Err(error) => {
                SetDlgItemTextW(hwnd, ERROR, wide(&error).as_ptr());
                SetFocus(GetDlgItem(hwnd, PROGRAM));
            }
        }
    }
}

unsafe fn read(hwnd: HWND, id: i32) -> String {
    let control = GetDlgItem(hwnd, id);
    let mut text = vec![0u16; GetWindowTextLengthW(control).max(0) as usize + 1];
    let length = GetWindowTextW(control, text.as_mut_ptr(), text.len() as i32).max(0) as usize;
    String::from_utf16_lossy(&text[..length])
}

unsafe fn browse(hwnd: HWND) {
    let mut path = [0u16; 32768];
    let filter: Vec<u16> = format!(
        "{} (*.exe;*.com)\0*.exe;*.com\0\0",
        tr("프로그램", "Programs")
    )
    .encode_utf16()
    .collect();
    let title = wide(tr("프로그램 선택", "Choose a program"));
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        lpstrFilter: filter.as_ptr(),
        lpstrTitle: title.as_ptr(),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER,
        ..zeroed()
    };
    if GetOpenFileNameW(&mut dialog) != 0 {
        let end = path
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(path.len());
        SetDlgItemTextW(
            hwnd,
            PROGRAM,
            wide(&format!("\"{}\"", String::from_utf16_lossy(&path[..end]))).as_ptr(),
        );
        SetFocus(GetDlgItem(hwnd, PROGRAM));
    } else {
        let error = CommDlgExtendedError();
        if error != 0 {
            SetDlgItemTextW(
                hwnd,
                ERROR,
                wide(&format!(
                    "{} ({error})",
                    tr(
                        "프로그램 선택 창을 열 수 없습니다.",
                        "Cannot open the program picker."
                    )
                ))
                .as_ptr(),
            );
        }
    }
}

unsafe fn initialize(hwnd: HWND, state: &State) {
    let dpi = state.dpi;
    let px = |dip: f32| gfx::pxi(dpi, dip);
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: px(560.0),
        bottom: px(344.0),
    };
    AdjustWindowRectExForDpi(
        &mut rect,
        GetWindowLongW(hwnd, GWL_STYLE) as u32,
        0,
        GetWindowLongW(hwnd, GWL_EXSTYLE) as u32,
        dpi as u32,
    );
    let mut owner: RECT = zeroed();
    GetWindowRect((*state.app).hwnd, &mut owner);
    let monitor = MonitorFromWindow((*state.app).hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..zeroed()
    };
    GetMonitorInfoW(monitor, &mut info);
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    let x = (owner.left + (owner.right - owner.left - width) / 2).clamp(
        info.rcWork.left,
        (info.rcWork.right - width).max(info.rcWork.left),
    );
    let y = (owner.top + (owner.bottom - owner.top - height) / 2).clamp(
        info.rcWork.top,
        (info.rcWork.bottom - height).max(info.rcWork.top),
    );
    SetWindowPos(
        hwnd,
        null_mut(),
        x,
        y,
        width,
        height,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    SetWindowTextW(hwnd, wide(tr("새 작업 실행", "Run new task")).as_ptr());
    let dark = i32::from(colors().dark);
    DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
        (&dark as *const i32).cast(),
        size_of::<i32>() as u32,
    );

    let control = |class: &str, text: &str, id: i32, style: u32, bounds: [f32; 4]| {
        let child = CreateWindowExW(
            0,
            wide(class).as_ptr(),
            wide(text).as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            px(bounds[0]),
            px(bounds[1]),
            px(bounds[2]),
            px(bounds[3]),
            hwnd,
            id as usize as HMENU,
            GetModuleHandleW(null()),
            null(),
        );
        SendMessageW(
            child,
            WM_SETFONT,
            state.fonts.get(fonts::Role::Ui) as usize,
            0,
        );
        child
    };
    control(
        "STATIC",
        tr("프로그램 또는 명령", "Program or command"),
        -1,
        0,
        [24.0, 20.0, 500.0, 20.0],
    );
    let program = control(
        "EDIT",
        "",
        PROGRAM,
        WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
        [24.0, 45.0, 408.0, 30.0],
    );
    SendMessageW(program, EM_SETLIMITTEXT, 32760, 0);
    SendMessageW(
        program,
        EM_SETCUEBANNER,
        0,
        wide("notepad.exe").as_ptr() as isize,
    );
    control(
        "BUTTON",
        tr("찾아보기…", "Browse…"),
        BROWSE,
        WS_TABSTOP | BS_OWNERDRAW as u32,
        [440.0, 45.0, 96.0, 30.0],
    );
    control(
        "STATIC",
        tr("인수 (선택 사항)", "Arguments (optional)"),
        -1,
        0,
        [24.0, 93.0, 512.0, 20.0],
    );
    let arguments = control(
        "EDIT",
        "",
        ARGUMENTS,
        WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
        [24.0, 118.0, 512.0, 30.0],
    );
    SendMessageW(arguments, EM_SETLIMITTEXT, 32760, 0);
    let admin = control(
        "BUTTON",
        tr(
            "관리자 권한으로 이 작업 실행",
            "Run this task as administrator",
        ),
        ADMIN,
        WS_TABSTOP | BS_OWNERDRAW as u32,
        [24.0, 166.0, 512.0, 26.0],
    );
    if crate::netetw::is_elevated() {
        // Every launch from an elevated Feather inherits its token.
        EnableWindow(admin, 0);
    }
    let hint = if crate::netetw::is_elevated() {
        tr(
            "Feather가 관리자 권한으로 실행 중이므로 새 작업도 권한을 상속합니다.",
            "Feather is elevated; new tasks inherit its administrator permissions.",
        )
    } else {
        tr(
            "공백이 있는 경로는 큰따옴표로 묶으세요. 관리자 실행은 UAC를 요청합니다.",
            "Quote paths containing spaces. Administrator mode requests UAC consent.",
        )
    };
    control("STATIC", hint, -1, 0, [24.0, 198.0, 512.0, 40.0]);
    control("STATIC", "", ERROR, 0, [24.0, 241.0, 512.0, 42.0]);
    control(
        "BUTTON",
        tr("취소", "Cancel"),
        IDCANCEL,
        WS_TABSTOP | BS_OWNERDRAW as u32,
        [328.0, 294.0, 100.0, 32.0],
    );
    control(
        "BUTTON",
        tr("실행", "Run"),
        IDOK,
        WS_TABSTOP | BS_OWNERDRAW as u32,
        [436.0, 294.0, 100.0, 32.0],
    );
    SendMessageW(hwnd, DM_SETDEFID, IDOK as usize, 0);
    EnableWindow(GetDlgItem(hwnd, IDOK), 0);
    SetFocus(program);
}

/// The administrator option as the app's own switch followed by its label:
/// a themed native checkbox ignores the dark theme's text color.
unsafe fn draw_admin_switch(pt: &widgets::Painter, item: &DRAWITEMSTRUCT, state: &State) {
    let r = item.rcItem;
    let enabled = item.itemState & ODS_DISABLED == 0;
    let focused = item.itemState & ODS_FOCUS != 0 && item.itemState & ODS_NOFOCUSRECT == 0;
    pt.fill(r, colors().surface);
    let height = pt.pxi(20.0);
    let top = (r.top + r.bottom - height) / 2;
    let track = RECT {
        left: r.left + pt.pxi(2.0),
        top,
        right: r.left + pt.pxi(42.0),
        bottom: top + height,
    };
    let on = if state.admin { 1.0 } else { 0.0 };
    widgets::switch(pt, track, on, enabled, focused, colors().surface);
    let mut value = [0u16; 80];
    let length =
        GetWindowTextW(item.hwndItem, value.as_mut_ptr(), value.len() as i32).max(0) as usize;
    let label = RECT {
        left: track.right + pt.pxi(10.0),
        ..r
    };
    pt.label(
        state.fonts.get(fonts::Role::Ui),
        if enabled { colors().fg } else { colors().muted },
        &String::from_utf16_lossy(&value[..length]),
        label,
        DT_LEFT,
    );
}

unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> isize {
    if message == WM_INITDIALOG {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, l);
        initialize(hwnd, &*(l as *const State));
        return 0; // We assigned initial keyboard focus ourselves.
    }
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if pointer.is_null() {
        return 0;
    }
    let state = &mut *pointer;
    match message {
        WM_COMMAND => {
            let id = (w & 0xffff) as i32;
            match id {
                // An unknown name or a rejected path shows below the fields,
                // not after closing.
                IDOK => start_check(hwnd, state),
                IDCANCEL => {
                    EndDialog(hwnd, IDCANCEL as isize);
                }
                BROWSE => {
                    discard_check(hwnd, state);
                    browse(hwnd);
                }
                ADMIN if w >> 16 == BN_CLICKED as usize => {
                    discard_check(hwnd, state);
                    state.admin = !state.admin;
                    InvalidateRect(l as HWND, null(), 0);
                }
                ARGUMENTS if w >> 16 == EN_CHANGE as usize => discard_check(hwnd, state),
                PROGRAM if w >> 16 == EN_CHANGE as usize => {
                    discard_check(hwnd, state);
                    EnableWindow(
                        GetDlgItem(hwnd, IDOK),
                        i32::from(!read(hwnd, PROGRAM).trim().is_empty()),
                    );
                    SetDlgItemTextW(hwnd, ERROR, wide("").as_ptr());
                }
                _ => return 0,
            }
            1
        }
        CHECKED => {
            finish_checks(hwnd, state);
            1
        }
        WM_TIMER if w == CHECK_TIMER => {
            KillTimer(hwnd, CHECK_TIMER);
            if state.checking.is_some() {
                SetDlgItemTextW(hwnd, ERROR, wide(checking_text()).as_ptr());
            }
            1
        }
        WM_CLOSE => {
            EndDialog(hwnd, IDCANCEL as isize);
            1
        }
        WM_CTLCOLORDLG => state.surface as isize,
        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            SetBkColor(w as HDC, colors().surface);
            SetTextColor(
                w as HDC,
                match GetDlgCtrlID(l as HWND) {
                    ERROR if state.checking.is_some() => colors().muted,
                    ERROR => colors().danger,
                    _ => colors().fg,
                },
            );
            state.surface as isize
        }
        WM_DRAWITEM => {
            let item = &*(l as *const DRAWITEMSTRUCT);
            let pt = widgets::Painter::new(item.hDC, state.dpi, &state.fonts);
            if item.CtlID == ADMIN as u32 {
                draw_admin_switch(&pt, item, state);
                return 1;
            }
            let mut value = [0u16; 80];
            let length = GetWindowTextW(item.hwndItem, value.as_mut_ptr(), value.len() as i32)
                .max(0) as usize;
            let button = widgets::ButtonState {
                pressed: item.itemState & ODS_SELECTED != 0,
                focused: item.itemState & ODS_FOCUS != 0 && item.itemState & ODS_NOFOCUSRECT == 0,
                disabled: item.itemState & ODS_DISABLED != 0,
                ..Default::default()
            };
            widgets::button_face(
                &pt,
                item.rcItem,
                if item.CtlID == IDOK as u32 {
                    widgets::ButtonStyle::Primary
                } else {
                    widgets::ButtonStyle::Default
                },
                &button,
                &String::from_utf16_lossy(&value[..length]),
                colors().surface,
            );
            1
        }
        _ => 0,
    }
}

pub(super) unsafe fn show(app: *mut App) -> Option<TaskLaunch> {
    // A standard dialog manager supplies Tab/Shift+Tab, Enter, Escape, owner
    // disabling, screen reader semantics, and cancellation of its modal loop.
    #[repr(C, align(4))]
    struct Template {
        header: DLGTEMPLATE,
        menu: u16,
        class: u16,
        title: u16,
    }
    let template = Template {
        header: DLGTEMPLATE {
            style: WS_POPUP | WS_CAPTION | WS_SYSMENU | DS_MODALFRAME as u32,
            dwExtendedStyle: 0,
            cdit: 0,
            x: 0,
            y: 0,
            cx: 360,
            cy: 230,
        },
        menu: 0,
        class: 0,
        title: 0,
    };
    let mut state = State {
        app,
        fonts: fonts::Fonts::new((*app).dpi, language()),
        dpi: (*app).dpi,
        surface: CreateSolidBrush(colors().surface),
        admin: crate::netetw::is_elevated(),
        result: None,
        checking: None,
        requests: 0,
        checked: mpsc::channel(),
    };
    let result = DialogBoxIndirectParamW(
        GetModuleHandleW(null()),
        &template.header,
        (*app).hwnd,
        Some(procedure),
        (&mut state as *mut State) as isize,
    );
    if result == -1 {
        (*app).set_error(
            ErrorSource::Action,
            tf!(
                "새 작업 창을 열 수 없습니다: {}",
                "Cannot open the new task dialog: {}",
                std::io::Error::last_os_error()
            ),
        );
    }
    state.result.take()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn submit_to(
        checks: &Arc<Checks>,
        sender: &mpsc::Sender<(&'static str, bool)>,
        name: &'static str,
    ) -> Cancelled {
        let sender = sender.clone();
        let cancelled = Cancelled::default();
        checks
            .submit(Check {
                task: TaskLaunch {
                    command: "cmd".into(),
                    arguments: String::new(),
                    elevated: false,
                },
                cancelled: Arc::clone(&cancelled),
                done: Box::new(move |result| {
                    let _ = sender.send((name, result.is_ok()));
                }),
            })
            .unwrap();
        cancelled
    }

    /// A request withdrawn before it starts (its dialog was edited or
    /// closed) never reaches the drive, whether it still waited or the
    /// thread had just taken it.
    #[test]
    fn withdrawn_requests_are_never_checked() {
        let checks = Arc::new(Checks::default());
        checks
            .delay_ms
            .store(300, std::sync::atomic::Ordering::Relaxed);
        let (sender, results) = mpsc::channel();
        let running = submit_to(&checks, &sender, "running");
        while checks.queue().waiting.is_some() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let waiting = submit_to(&checks, &sender, "withdrawn");
        // Withdrawing the running check only marks it: the waiting one stays.
        checks.withdraw(&running);
        assert!(checks.queue().waiting.is_some());
        checks.withdraw(&waiting);
        assert!(
            checks.queue().waiting.is_none(),
            "it left the queue at once"
        );
        // The same request already taken by the thread: only its token says
        // it was withdrawn (as when the thread takes it just before).
        let taken = submit_to(&checks, &sender, "cancelled");
        taken.store(true, std::sync::atomic::Ordering::Release);
        let wait = || results.recv_timeout(Duration::from_secs(10));
        assert_eq!(wait().unwrap(), ("running", true));
        // Neither withdrawn request is checked; the thread goes idle.
        assert!(results.recv_timeout(Duration::from_millis(800)).is_err());
        while checks.queue().running {
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A stalled drive: re-running and reopening the dialog meanwhile never
    /// adds threads, and only the newest waiting request is checked.
    #[test]
    fn checks_share_one_thread_and_keep_only_the_newest_request() {
        let checks = Arc::new(Checks::default());
        checks
            .delay_ms
            .store(300, std::sync::atomic::Ordering::Relaxed);
        let (sender, results) = mpsc::channel();
        let submit = |name: &'static str| {
            submit_to(&checks, &sender, name);
        };
        submit("first");
        // Once the thread has taken it, later requests wait behind it.
        while checks.queue().waiting.is_some() {
            std::thread::sleep(Duration::from_millis(5));
        }
        for name in ["second", "third", "newest"] {
            submit(name);
        }
        let wait = || results.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(wait(), ("first", true), "the running check finishes");
        assert_eq!(wait(), ("newest", true), "then only the newest waiting one");
        assert!(results.recv_timeout(Duration::from_millis(500)).is_err());
        assert_eq!(checks.started.load(std::sync::atomic::Ordering::Relaxed), 1);
        // The idle thread has exited; the next request starts one again.
        while checks.queue().running {
            std::thread::sleep(Duration::from_millis(10));
        }
        checks
            .delay_ms
            .store(0, std::sync::atomic::Ordering::Relaxed);
        submit("later");
        assert_eq!(wait(), ("later", true));
        assert_eq!(checks.started.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}
