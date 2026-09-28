//! On-demand native modal dialog; no timers, polling, or retained command history.
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

struct State {
    app: *mut App,
    fonts: fonts::Fonts,
    dpi: i32,
    surface: HBRUSH,
    /// The administrator switch (owner-drawn; always on when Feather is elevated).
    admin: bool,
    result: Option<TaskLaunch>,
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.surface);
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
                IDOK => {
                    let task = TaskLaunch {
                        command: read(hwnd, PROGRAM),
                        arguments: read(hwnd, ARGUMENTS),
                        elevated: state.admin,
                    };
                    // Resolve the program here: an unknown name or a
                    // rejected path shows below the fields, not after closing.
                    match crate::actions::check_task(&task) {
                        Ok(_) => {
                            state.result = Some(task);
                            EndDialog(hwnd, IDOK as isize);
                        }
                        Err(error) => {
                            SetDlgItemTextW(hwnd, ERROR, wide(&error).as_ptr());
                            SetFocus(GetDlgItem(hwnd, PROGRAM));
                        }
                    }
                }
                IDCANCEL => {
                    EndDialog(hwnd, IDCANCEL as isize);
                }
                BROWSE => browse(hwnd),
                ADMIN if w >> 16 == BN_CLICKED as usize => {
                    state.admin = !state.admin;
                    InvalidateRect(l as HWND, null(), 0);
                }
                PROGRAM if w >> 16 == EN_CHANGE as usize => {
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
        WM_CLOSE => {
            EndDialog(hwnd, IDCANCEL as isize);
            1
        }
        WM_CTLCOLORDLG => state.surface as isize,
        WM_CTLCOLOREDIT | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            SetBkColor(w as HDC, colors().surface);
            SetTextColor(
                w as HDC,
                if GetDlgCtrlID(l as HWND) == ERROR {
                    colors().danger
                } else {
                    colors().fg
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
