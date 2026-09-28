//! Short-lived native command palette and opt-in notification-area window route.
//! Neither feature starts a collector or executes command text from search input.

use super::widgets::Painter;
use super::*;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};

pub(super) const TRAY_MESSAGE: u32 = WM_APP + 3;
const TRAY_ID: u32 = 1;
// Shellapi.h: NIN_KEYSELECT = NIN_SELECT | NINF_KEY (0x1). The SDK alias is
// not emitted by windows-sys 0.61.2.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const PALETTE_EDIT: usize = 1;
const PALETTE_TIMER: usize = 1;
const MAX_RESULTS: usize = 10;
const MAX_QUERY: usize = 256;
const THEME_LIGHT: usize = 130;
const THEME_DARK: usize = 131;

#[derive(Clone, Debug, PartialEq, Eq)]
enum PaletteAction {
    Command(usize),
    Process(ProcessIdentity),
}

#[derive(Clone, Debug)]
struct PaletteItem {
    title: String,
    hint: String,
    keywords: String,
    action: PaletteAction,
}

/// `.palette` geometry (DIP): 560 wide, 90 below the window top, a 48 px
/// input with a bottom border, the list `padding: 6px; max-height: 320px`
/// with 38 px rows (`padding: 0 12px`), `.empty { padding: 14px 12px }`.
const PALETTE_WIDTH: f32 = 560.0;
const PALETTE_TOP: f32 = 90.0;
const PALETTE_INPUT: f32 = 48.0;
const PALETTE_INPUT_PAD: f32 = 18.0;
const PALETTE_LIST_PAD: f32 = 6.0;
const PALETTE_LIST_MAX: f32 = 320.0;
const PALETTE_ITEM: f32 = 38.0;
const PALETTE_ITEM_PAD: f32 = 12.0;
const PALETTE_EMPTY_PAD: f32 = 14.0;
/// The shadow's fade timer (distinct from PALETTE_TIMER).
const PALETTE_ANIM_TIMER: usize = 0xFEB6;
/// The palette's opacity (its layered alpha, its shadow and its entrance
/// slide follow it).
const SHADOW_KEY: (usize, u32) = (0, anim::part::OPACITY);
/// Posted when the exit fade has finished: destroy the palette.
const PALETTE_DONE: u32 = WM_APP + 41;
/// Overlay scrollbar hover growth and the smooth wheel scroll offset.
const BAR_KEY: (usize, u32) = (1, anim::part::SCROLLBAR_WIDTH);
const SCROLL_KEY: (usize, u32) = (2, anim::part::SCROLL);
/// The overlay scrollbar's opacity: shown on open, scroll and hover, faded
/// out after the idle time like the tables' (DESIGN_SPEC §4).
const BAR_OPACITY: (usize, u32) = (3, anim::part::SCROLLBAR);

struct CommandPalette {
    app: *mut App,
    hwnd: HWND,
    edit: HWND,
    previous_focus: HWND,
    items: Vec<PaletteItem>,
    query: String,
    /// The highlighted row (`li[aria-selected]`).
    selected: usize,
    /// List scroll offset (device px).
    scroll: i32,
    /// Pointer over the list's scrollbar zone: the thumb grows.
    bar_hover: bool,
    /// Row pressed by the mouse (runs on release over the same row).
    pressed: Option<usize>,
    back: gfx::BackBuffer,
    /// Soft shadow behind the opaque popup (it hosts the native Edit).
    shadow: Option<gfx::ShadowWindow>,
    scrim: HWND,
    anim: anim::AnimHost<(usize, u32)>,
    /// Screen position when fully shown (the entrance slides into it).
    origin: POINT,
    /// Entrance offset at opacity 0 (device px; negative = from above).
    slide: f32,
    /// Fading out (`close_palette`): input is ignored, then it is destroyed.
    closing: bool,
}

struct CreatePalette {
    state: *mut CommandPalette,
    attached: bool,
}

/// Palette commands. Everything that left the nav rail (Refresh, Pause,
/// Run new task, Resource Monitor, Always on top) stays reachable here.
fn command_items(
    paused: bool,
    dark: bool,
    selected_process: bool,
    topmost: bool,
) -> Vec<PaletteItem> {
    let mut items = Vec::with_capacity(14);
    for (index, title, english) in [
        (0, tr("프로세스로 이동", "Go to Processes"), "processes"),
        (
            1,
            tr("성능으로 이동", "Go to Performance"),
            "performance cpu memory disk network gpu",
        ),
        (
            2,
            tr("시작 앱으로 이동", "Go to Startup apps"),
            "startup apps",
        ),
        (3, tr("서비스로 이동", "Go to Services"), "services"),
        (4, tr("설정으로 이동", "Go to Settings"), "settings"),
    ] {
        items.push(PaletteItem {
            title: title.into(),
            hint: tr("화면", "Page").into(),
            keywords: english.into(),
            action: PaletteAction::Command(NAV + index),
        });
    }
    if selected_process {
        items.push(PaletteItem {
            title: tr("선택한 작업 끝내기", "End selected task").into(),
            hint: "Del".into(),
            keywords: "end terminate selected task".into(),
            action: PaletteAction::Command(PRIMARY),
        });
    }
    items.push(PaletteItem {
        title: if paused {
            tr("업데이트 계속", "Resume updates")
        } else {
            tr("업데이트 일시정지", "Pause updates")
        }
        .into(),
        // Space pauses / resumes from the table, like the reference's hint.
        hint: "Space".into(),
        keywords: "pause resume updates".into(),
        action: PaletteAction::Command(PAUSE),
    });
    for (title, hint, keywords, command) in [
        (
            tr("새로 고침", "Refresh"),
            "F5",
            "refresh reload update",
            REFRESH,
        ),
        (
            tr("새 작업 실행…", "Run new task…"),
            tr("작업", "Task"),
            "run new task program open",
            RUN_TASK,
        ),
        (
            tr("리소스 모니터 열기", "Open Resource Monitor"),
            tr("도구", "Tool"),
            "resource monitor resmon",
            RESOURCE_MONITOR,
        ),
        (
            if topmost {
                tr("항상 위에 표시 끄기", "Turn off always on top")
            } else {
                tr("항상 위에 표시", "Keep on top of other windows")
            },
            tr("창", "Window"),
            "always on top topmost window pin",
            PREF_TOP,
        ),
        (
            // The panel's name in both languages; "memory" / "zombie" and
            // their Korean words find it.
            "Nuclear Zombie",
            tr("메모리 정리", "Memory cleanup"),
            "nuclear zombie memory cleanup clean free ram standby cache working set trim leak handles 메모리 정리 좀비 캐시",
            NUCLEAR,
        ),
    ] {
        items.push(PaletteItem {
            title: title.into(),
            hint: hint.into(),
            keywords: keywords.into(),
            action: PaletteAction::Command(command),
        });
    }
    items.push(PaletteItem {
        title: if dark {
            tr("밝은 테마로 변경", "Switch to light theme")
        } else {
            tr("어두운 테마로 변경", "Switch to dark theme")
        }
        .into(),
        hint: tr("테마", "Theme").into(),
        keywords: "theme light dark".into(),
        action: PaletteAction::Command(if dark { THEME_LIGHT } else { THEME_DARK }),
    });
    items
}

fn palette_items(
    query: &str,
    commands: Vec<PaletteItem>,
    processes: &[Process],
) -> Vec<PaletteItem> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return commands;
    }
    let mut items: Vec<_> = commands
        .into_iter()
        .filter(|item| item.title.to_lowercase().contains(&query) || item.keywords.contains(&query))
        .take(MAX_RESULTS)
        .collect();
    let remaining = MAX_RESULTS.saturating_sub(items.len());
    // Sort matching candidates by name/PID so live resource sorting cannot move
    // a palette entry underneath the keyboard selection on each refresh.
    let mut matching: Vec<_> = processes
        .iter()
        .filter(|process| {
            process.name.to_lowercase().contains(&query) || process.pid.to_string().contains(&query)
        })
        .collect();
    matching.sort_unstable_by(|a, b| a.name.cmp(&b.name).then(a.pid.cmp(&b.pid)));
    items.extend(
        matching
            .into_iter()
            .take(remaining)
            .map(|process| PaletteItem {
                title: process.name.clone(),
                hint: format!("PID {} · {:.1}% CPU", process.pid, process.cpu_percent),
                keywords: String::new(),
                action: PaletteAction::Process(ProcessIdentity {
                    pid: process.pid,
                    created: process.created,
                }),
            }),
    );
    items
}

unsafe fn palette_state(hwnd: HWND) -> *mut CommandPalette {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CommandPalette
}

/// The palette's regions in its client (device px): the input band, the
/// list viewport (with its 6 px padding) and the row height.
struct PaletteGeometry {
    hair: i32,
    input: RECT,
    list: RECT,
    pad: i32,
    row: i32,
}

unsafe fn palette_geometry(state: *mut CommandPalette) -> PaletteGeometry {
    let dpi = (*(*state).app).dpi;
    let mut client: RECT = zeroed();
    GetClientRect((*state).hwnd, &mut client);
    let hair = gfx::hairline(dpi) as i32;
    let input = RECT {
        left: hair,
        top: hair,
        right: client.right - hair,
        bottom: hair + gfx::pxi(dpi, PALETTE_INPUT),
    };
    PaletteGeometry {
        hair,
        input,
        list: RECT {
            left: hair,
            top: input.bottom,
            right: client.right - hair,
            bottom: client.bottom - hair,
        },
        pad: gfx::pxi(dpi, PALETTE_LIST_PAD),
        row: gfx::pxi(dpi, PALETTE_ITEM),
    }
}

/// Window height for `count` rows: border, input, list (max 320), border.
fn palette_height(dpi: i32, count: usize) -> i32 {
    let hair = gfx::hairline(dpi) as i32;
    let pad = gfx::pxi(dpi, PALETTE_LIST_PAD);
    let list = if count == 0 {
        (gfx::px(dpi, PALETTE_LIST_PAD + PALETTE_EMPTY_PAD) * 2.0 + gfx::px(dpi, 13.0 * 1.45))
            .round() as i32
    } else {
        (2 * pad + count as i32 * gfx::pxi(dpi, PALETTE_ITEM)).min(gfx::pxi(dpi, PALETTE_LIST_MAX))
    };
    2 * hair + gfx::pxi(dpi, PALETTE_INPUT) + list
}

/// The list's scroll range: (content, viewport) heights.
unsafe fn palette_extent(state: *mut CommandPalette) -> (i32, i32) {
    let g = palette_geometry(state);
    (
        2 * g.pad + (*state).items.len() as i32 * g.row,
        g.list.bottom - g.list.top,
    )
}

/// Show the list's scrollbar (when it overflows) and fade it out after the
/// idle time unless the pointer is over it.
unsafe fn palette_bar_activity(state: *mut CommandPalette) {
    let (content, view) = palette_extent(state);
    let host = &mut (*state).anim;
    if content <= view {
        host.set(BAR_OPACITY, 0.0);
        return;
    }
    host.set_target(
        BAR_OPACITY,
        1.0,
        anim::motion::SCROLLBAR_FADE_IN,
        anim::Easing::EaseOut,
    );
    if (*state).bar_hover {
        host.cancel_delayed(BAR_OPACITY);
    } else {
        host.set_target_after(
            BAR_OPACITY,
            0.0,
            anim::motion::SCROLLBAR_IDLE,
            anim::motion::SCROLLBAR_FADE_OUT,
            anim::Easing::EaseOut,
        );
    }
}

/// Clamp the scroll offset, optionally bringing the selected row into view.
unsafe fn palette_scroll(state: *mut CommandPalette, reveal: bool) {
    let g = palette_geometry(state);
    let (content, view) = palette_extent(state);
    if reveal && !(*state).items.is_empty() {
        let top = g.pad + (*state).selected as i32 * g.row;
        let bottom = top + g.row;
        if top - g.pad < (*state).scroll {
            (*state).scroll = top - g.pad;
        } else if bottom + g.pad > (*state).scroll + view {
            (*state).scroll = bottom + g.pad - view;
        }
    }
    let before = (*state).anim.value(SCROLL_KEY).round() as i32;
    (*state).scroll = (*state).scroll.clamp(0, (content - view).max(0));
    (*state).anim.set(SCROLL_KEY, (*state).scroll as f32);
    if (*state).scroll != before {
        palette_bar_activity(state);
    }
}

/// The list offset on screen (the wheel eases toward `scroll`).
unsafe fn visual_scroll(state: *mut CommandPalette) -> i32 {
    (*state).anim.value(SCROLL_KEY).round() as i32
}

/// The row under a client point.
unsafe fn palette_row(state: *mut CommandPalette, pt: POINT) -> Option<usize> {
    let g = palette_geometry(state);
    if pt.y < g.list.top || pt.y >= g.list.bottom || pt.x < g.list.left || pt.x >= g.list.right {
        return None;
    }
    let y = pt.y - g.list.top + visual_scroll(state) - g.pad;
    if y < 0 {
        return None;
    }
    let index = (y / g.row) as usize;
    (index < (*state).items.len()).then_some(index)
}

/// One frame of the entrance / exit: the palette's layered alpha (LWA_ALPHA
/// keeps drawing its Edit child), its 4 px slide and its shadow follow the
/// opacity tween.
unsafe fn palette_frame(state: *mut CommandPalette) {
    let hwnd = (*state).hwnd;
    let t = (*state).anim.value(SHADOW_KEY).clamp(0.0, 1.0);
    SetLayeredWindowAttributes(hwnd, 0, (t * 255.0).round() as u8, LWA_ALPHA);
    let dy = ((1.0 - t) * (*state).slide).round() as i32;
    let mut r: RECT = zeroed();
    GetWindowRect(hwnd, &mut r);
    let (x, y) = ((*state).origin.x, (*state).origin.y + dy);
    if r.left != x || r.top != y {
        SetWindowPos(
            hwnd,
            null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOREDRAW,
        );
    }
    palette_shadow(state);
}

/// Show the shadow behind the palette at the shadow's fade value.
unsafe fn palette_shadow(state: *mut CommandPalette) {
    let hwnd = (*state).hwnd;
    let p = (*state).app;
    let alpha = ((*state).anim.value(SHADOW_KEY).clamp(0.0, 1.0) * 255.0).round() as u8;
    if let Some(shadow) = (*state).shadow.as_mut() {
        if IsWindowVisible(hwnd) == 0 {
            shadow.hide();
            return;
        }
        let mut r: RECT = zeroed();
        GetWindowRect(hwnd, &mut r);
        shadow.show(
            hwnd,
            r,
            (*p).dpi,
            gfx::px((*p).dpi, theme::RADIUS),
            colors().shadow(),
            alpha,
        );
    }
}

unsafe fn refresh_palette(state: *mut CommandPalette, reset_selection: bool) {
    if state.is_null() {
        return;
    }
    let p = (*state).app;
    let selected = (!reset_selection)
        .then(|| {
            (&(*state).items)
                .get((*state).selected)
                .map(|item| item.action.clone())
        })
        .flatten();
    let length = GetWindowTextLengthW((*state).edit).max(0) as usize;
    let mut text = vec![0u16; length.min(MAX_QUERY) + 1];
    let length =
        GetWindowTextW((*state).edit, text.as_mut_ptr(), text.len() as i32).max(0) as usize;
    (*state).query = String::from_utf16_lossy(&text[..length]);
    let commands = command_items(
        (*p).paused,
        colors().dark,
        (*p).page == Page::Processes && selected_process(p).is_some() && !(*p).busy,
        (*p).topmost,
    );
    (*state).items = palette_items(
        &(*state).query,
        commands,
        (*p).snapshot
            .as_ref()
            .map_or(&[], |snapshot| snapshot.processes.as_slice()),
    );
    (*state).selected = selected
        .and_then(|selected| {
            (*state)
                .items
                .iter()
                .position(|item| item.action == selected)
        })
        .unwrap_or(0);
    if reset_selection {
        (*state).scroll = 0;
    }
    let height = palette_height((*p).dpi, (*state).items.len());
    let mut bounds = RECT::default();
    GetWindowRect((*state).hwnd, &mut bounds);
    if bounds.bottom - bounds.top != height {
        SetWindowPos(
            (*state).hwnd,
            null_mut(),
            0,
            0,
            bounds.right - bounds.left,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    palette_scroll(state, true);
    InvalidateRect((*state).hwnd, null(), 0);
}

pub(super) unsafe fn open_palette(p: *mut App) {
    create_palette(p, true);
}

/// Create the palette window (`show` = false keeps it hidden: tests).
/// DESIGN_SPEC §4: an opaque popup (the native Edit keeps caret and IME)
/// rounded and bordered by DWM, with a soft shadow window behind it and the
/// scrim over the owner; both fade in.
pub(super) unsafe fn create_palette(p: *mut App, show: bool) {
    if (*p).modal {
        return;
    }
    if !(*p).palette.is_null() && IsWindow((*p).palette) != 0 {
        SetForegroundWindow((*p).palette);
        let state = palette_state((*p).palette);
        if !state.is_null() {
            SetFocus((*state).edit);
        }
        return;
    }
    let class = wide("FeatherTaskManager.CommandPalette");
    let instance = GetModuleHandleW(null());
    let definition = WNDCLASSW {
        lpfnWndProc: Some(palette_proc),
        hInstance: instance,
        hCursor: LoadCursorW(null_mut(), IDC_ARROW),
        lpszClassName: class.as_ptr(),
        ..zeroed()
    };
    if RegisterClassW(&definition) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        return;
    }
    // `margin-top: 90px`, centred, `width: min(560px, 100% - 32px)`.
    let mut client = RECT::default();
    GetClientRect((*p).hwnd, &mut client);
    MapWindowPoints(
        (*p).hwnd,
        null_mut(),
        (&mut client as *mut RECT).cast::<POINT>(),
        2,
    );
    let available = client.right - client.left - gfx::pxi((*p).dpi, 32.0);
    let width = gfx::pxi((*p).dpi, PALETTE_WIDTH)
        .min(available)
        .max(gfx::pxi((*p).dpi, 320.0));
    let height = palette_height((*p).dpi, MAX_RESULTS);
    // The scrim first: the palette stacks above it.
    let scrim = if show {
        popup::open_scrim(p, null_mut())
    } else {
        null_mut()
    };
    let origin = POINT {
        x: client.left + (client.right - client.left - width) / 2,
        y: client.top + gfx::pxi((*p).dpi, PALETTE_TOP),
    };
    let slide = -gfx::px((*p).dpi, anim::motion::POPUP_SLIDE_DIP);
    let state = Box::into_raw(Box::new(CommandPalette {
        app: p,
        hwnd: null_mut(),
        edit: null_mut(),
        previous_focus: GetFocus(),
        items: Vec::new(),
        query: String::new(),
        selected: 0,
        scroll: 0,
        bar_hover: false,
        pressed: None,
        back: gfx::BackBuffer::new(),
        shadow: None,
        scrim,
        anim: anim::AnimHost::new(PALETTE_ANIM_TIMER),
        origin,
        slide: if show { slide } else { 0.0 },
        closing: false,
    }));
    let mut create = CreatePalette {
        state,
        attached: false,
    };
    // Layered with a constant alpha (LWA_ALPHA): unlike UpdateLayeredWindow
    // it still draws the Edit child, and it can fade in and out.
    let hwnd = CreateWindowExW(
        WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT | if show { WS_EX_LAYERED } else { 0 },
        class.as_ptr(),
        wide(tr("명령 팔레트", "Command palette")).as_ptr(),
        WS_POPUP | WS_CLIPCHILDREN,
        origin.x,
        origin.y + (*state).slide.round() as i32,
        width,
        height,
        (*p).hwnd,
        null_mut(),
        instance,
        (&mut create as *mut CreatePalette).cast(),
    );
    if hwnd.is_null() {
        if !create.attached {
            popup::destroy(scrim);
            drop(Box::from_raw(state));
        }
        return;
    }
    (*p).palette = hwnd;
    // Windows 11 rounds the corners (8 px) and draws the 1 px border.
    gfx::round_popup(hwnd, colors().border, false);
    if show {
        popup::set_dismiss(scrim, hwnd);
        (*state).shadow = gfx::ShadowWindow::new((*p).hwnd);
        (*state).anim.set(SHADOW_KEY, 0.0);
        // Painted completely (the Edit included) while still transparent,
        // then the palette, its shadow and the scrim fade in together: no
        // frame shows a hollow outline, a blank input or a popping scrim.
        SetLayeredWindowAttributes(hwnd, 0, 0, LWA_ALPHA);
        ShowWindow(hwnd, SW_SHOW);
        SetForegroundWindow(hwnd);
        SetFocus((*state).edit);
        RedrawWindow(
            hwnd,
            null(),
            null_mut(),
            RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
        );
        popup::restart_fade(scrim);
        (*state).anim.set_target(
            SHADOW_KEY,
            1.0,
            anim::motion::POPUP_IN,
            anim::Easing::EaseOut,
        );
        palette_frame(state);
    }
}

/// Destroy the palette at once (theme / font / DPI changes, shutdown).
pub(super) unsafe fn destroy_palette(p: *mut App) {
    let hwnd = (*p).palette;
    if !hwnd.is_null() && IsWindow(hwnd) != 0 {
        DestroyWindow(hwnd);
    }
    (*p).palette = null_mut();
}

/// Close the palette like the popups: it and its scrim fade out together
/// (motion::POPUP_OUT), then it destroys itself. `reactivate`: give the
/// main window and its previous focus back now (Esc, a chosen command, a
/// click on the scrim) — not when another application took the focus.
pub(super) unsafe fn close_palette(p: *mut App, reactivate: bool) {
    let hwnd = (*p).palette;
    if hwnd.is_null() || IsWindow(hwnd) == 0 {
        (*p).palette = null_mut();
        return;
    }
    let state = palette_state(hwnd);
    if state.is_null() || IsWindowVisible(hwnd) == 0 {
        destroy_palette(p);
        return;
    }
    (*p).palette = null_mut();
    (*state).closing = true;
    (*state).slide = 0.0;
    popup::close((*state).scrim);
    (*state).scrim = null_mut();
    if reactivate && IsWindow((*p).hwnd) != 0 && IsIconic((*p).hwnd) == 0 {
        SetForegroundWindow((*p).hwnd);
        let previous = (*state).previous_focus;
        if IsWindow(previous) != 0 && IsChild((*p).hwnd, previous) != 0 {
            SetFocus(previous);
        }
    }
    if !(*state).anim.set_target(
        SHADOW_KEY,
        0.0,
        anim::motion::POPUP_OUT,
        anim::Easing::EaseOut,
    ) {
        // Reduced motion: gone at once.
        PostMessageW(hwnd, PALETTE_DONE, 0, 0);
    }
    palette_frame(state);
}

unsafe fn execute_palette(state: *mut CommandPalette) {
    let Some(action) = (&(*state).items)
        .get((*state).selected)
        .map(|item| item.action.clone())
    else {
        return;
    };
    let p = (*state).app;
    close_palette(p, true);
    match action {
        PaletteAction::Command(command) => {
            PostMessageW((*p).hwnd, WM_COMMAND, command, 0);
        }
        PaletteAction::Process(identity) => {
            if !(*p).snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.processes.iter().any(|process| {
                    process.pid == identity.pid && process.created == identity.created
                })
            }) {
                controls::notify_success(
                    p,
                    tr(
                        "이 프로세스가 종료되었습니다. 다시 검색하세요.",
                        "This process has exited. Search again.",
                    )
                    .into(),
                );
                redraw(p);
                return;
            }
            switch_page(p, Page::Processes);
            (*p).filter.clear();
            (*p).category = 0;
            (*p).updating = true;
            SendMessageW((*p).filter_control, CB_SETCURSEL, 0, 0);
            SetWindowTextW((*p).search, wide("").as_ptr());
            // Reveal hidden descendants in tree mode and the process's app in
            // the grouped view. No process is modified.
            (*p).collapsed.clear();
            if let Some(app) = interactions::app_of(p, identity) {
                (*p).expanded_groups.insert(app);
            }
            rebuild(p, Some(Identity::Process(identity.pid, identity.created)));
            let row = SendMessageW(
                (*p).list,
                LVM_GETNEXTITEM,
                usize::MAX,
                LVNI_SELECTED as isize,
            );
            if row >= 0 {
                SendMessageW((*p).list, LVM_ENSUREVISIBLE, row as usize, 0);
            }
            SetFocus((*p).list);
            update_buttons(p);
            interactions::selection_changed(p);
            redraw(p);
        }
    }
}

pub(super) unsafe fn palette_key(p: *mut App, message: &MSG) -> bool {
    let hwnd = (*p).palette;
    if hwnd.is_null()
        || IsWindow(hwnd) == 0
        || (message.hwnd != hwnd && IsChild(hwnd, message.hwnd) == 0)
    {
        return false;
    }
    if message.message != WM_KEYDOWN {
        return false;
    }
    let state = palette_state(hwnd);
    if state.is_null() {
        return false;
    }
    match message.wParam as u16 {
        VK_ESCAPE => close_palette(p, true),
        VK_RETURN => execute_palette(state),
        VK_UP | VK_DOWN => {
            let count = (*state).items.len();
            if count > 0 {
                let current = (*state).selected.min(count - 1);
                (*state).selected = if message.wParam as u16 == VK_DOWN {
                    (current + 1) % count
                } else {
                    (current + count - 1) % count
                };
                palette_scroll(state, true);
                InvalidateRect(hwnd, null(), 0);
            }
        }
        // The input keeps the focus (the list is painted, not a control).
        VK_TAB => {}
        _ => {
            // Keep owner shortcuts from acting while typing in the palette.
            return message.wParam as u16 == VK_F5
                || (GetKeyState(VK_CONTROL as i32) < 0
                    && !matches!(
                        message.wParam as u16,
                        0x41 | 0x43 | 0x56 | 0x58 | 0x59 | 0x5a
                    ));
        }
    }
    true
}

/// `.palette`: surface, 1 px border, the 48 px input band with its bottom
/// border, 38 px rows (radius 4, selected fg_sel, 13 px title, 12 px muted
/// hint on the right), the empty state and an overlay scrollbar.
unsafe fn paint_palette(state: *mut CommandPalette, dc: HDC, client: &RECT) {
    let p = (*state).app;
    let f = &(*p).fonts;
    let g = palette_geometry(state);
    {
        let pt = Painter::new(dc, (*p).dpi, f);
        let c = pt.c;
        pt.fill(*client, c.border);
        pt.fill(
            RECT {
                left: g.hair,
                top: g.hair,
                right: client.right - g.hair,
                bottom: client.bottom - g.hair,
            },
            c.surface,
        );
        pt.fill(
            RECT {
                top: g.input.bottom - g.hair,
                ..g.input
            },
            c.border,
        );
    }
    let saved = SaveDC(dc);
    IntersectClipRect(dc, g.list.left, g.list.top, g.list.right, g.list.bottom);
    {
        let pt = Painter::new(dc, (*p).dpi, f);
        let c = pt.c;
        let pad_x = pt.pxi(PALETTE_ITEM_PAD);
        if (*state).items.is_empty() {
            let top = g.list.top + pt.pxi(PALETTE_LIST_PAD + PALETTE_EMPTY_PAD);
            let band = RECT {
                left: g.list.left + g.pad + pad_x,
                top,
                right: g.list.right - g.pad - pad_x,
                bottom: top + pt.pxi(13.0 * 1.45),
            };
            let cell = pt.css_rect(f.ui, band, Some(13.0 * 1.45));
            pt.text(
                f.ui,
                c.muted,
                tr(
                    "일치하는 명령이나 프로세스가 없습니다",
                    "No matching commands or processes",
                ),
                RECT {
                    left: band.left,
                    right: band.right,
                    ..cell
                },
                DT_SINGLELINE | DT_END_ELLIPSIS | DT_LEFT,
            );
        }
        for (i, item) in (*state).items.iter().enumerate() {
            let top = g.list.top + g.pad + i as i32 * g.row - visual_scroll(state);
            if top >= g.list.bottom || top + g.row <= g.list.top {
                continue;
            }
            let row = RECT {
                left: g.list.left + g.pad,
                top,
                right: g.list.right - g.pad,
                bottom: top + g.row,
            };
            if i == (*state).selected {
                pt.canvas.fill_round_rect(
                    gfx::RectF::from_rect(row),
                    pt.px(theme::RADIUS_SM),
                    theme::solid(c.fg_sel),
                );
            }
            let hint_width = pt.measure(f.small, &item.hint).cx;
            let hint_right = row.right - pad_x;
            let hint_cell = pt.css_rect(f.small, row, Some(12.0 * 1.45));
            pt.text(
                f.small,
                c.muted,
                &item.hint,
                RECT {
                    left: hint_right - hint_width,
                    right: hint_right,
                    ..hint_cell
                },
                DT_SINGLELINE | DT_RIGHT,
            );
            let title_cell = pt.css_rect(f.ui, row, Some(13.0 * 1.45));
            pt.text(
                f.ui,
                c.fg,
                &item.title,
                RECT {
                    left: row.left + pad_x,
                    right: (hint_right - hint_width - pt.pxi(16.0)).max(row.left + pad_x),
                    ..title_cell
                },
                DT_SINGLELINE | DT_END_ELLIPSIS | DT_LEFT,
            );
        }
        // Overlay scrollbar over the right 14 px of the list.
        let (content, view) = palette_extent(state);
        if content > view {
            let lane = RECT {
                left: g.list.right - pt.pxi(14.0),
                top: g.list.top + g.pad,
                right: g.list.right,
                bottom: g.list.bottom - g.pad,
            };
            let lane_len = (lane.bottom - lane.top) as f32;
            let thumb = widgets::scroll_thumb(
                lane_len,
                view as f32,
                content as f32,
                visual_scroll(state) as f32,
                pt.px(widgets::SCROLL_THUMB_MIN),
            );
            widgets::scrollbar(
                &pt,
                lane,
                thumb,
                (*state).anim.value(BAR_KEY),
                (*state).anim.value(BAR_OPACITY),
            );
        }
    }
    RestoreDC(dc, saved);
}

/// The palette input's placeholder color: the reference only styles
/// `.search input::placeholder` (muted), so its palette shows Chromium's
/// default placeholder gray in both themes.
const PLACEHOLDER: u32 = theme::hex(0x757575);

fn palette_placeholder() -> &'static str {
    tr(
        "명령이나 프로세스 이름, PID 입력",
        "Type a command, process name or PID",
    )
}

/// The Edit's placeholder ([`PLACEHOLDER`]), also while focused
/// (`::placeholder`); the caret is hidden around the extra paint. The
/// context menu is the app's (`interactions::edit_menu`).
unsafe extern "system" fn palette_edit_proc(
    hwnd: HWND,
    message: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    if message == WM_CONTEXTMENU {
        let state = data as *mut CommandPalette;
        interactions::edit_menu((*state).app, hwnd, l);
        return 0;
    }
    let was_empty = interactions::edits_text(message).then(|| GetWindowTextLengthW(hwnd) == 0);
    let result = DefSubclassProc(hwnd, message, w, l);
    if let Some(was_empty) = was_empty {
        interactions::placeholder_changed(hwnd, was_empty);
    }
    if matches!(message, WM_PAINT | WM_PRINTCLIENT) && GetWindowTextLengthW(hwnd) == 0 {
        let state = data as *mut CommandPalette;
        let p = (*state).app;
        let dc = if message == WM_PRINTCLIENT {
            w as HDC
        } else {
            GetDC(hwnd)
        };
        if !dc.is_null() {
            let focused = GetFocus() == hwnd && message == WM_PAINT;
            if focused {
                HideCaret(hwnd);
            }
            let mut r: RECT = zeroed();
            GetClientRect(hwnd, &mut r);
            let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
            pt.fill(r, pt.c.surface);
            pt.label(
                (*p).fonts.palette_input,
                PLACEHOLDER,
                palette_placeholder(),
                r,
                DT_LEFT,
            );
            drop(pt);
            if focused {
                ShowCaret(hwnd);
            }
            if message != WM_PRINTCLIENT {
                ReleaseDC(hwnd, dc);
            }
        }
    }
    if message == WM_NCDESTROY {
        RemoveWindowSubclass(hwnd, Some(palette_edit_proc), id);
    }
    result
}

unsafe extern "system" fn palette_proc(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if message == WM_NCCREATE {
        let info = &*(l as *const CREATESTRUCTW);
        let create = &mut *(info.lpCreateParams as *mut CreatePalette);
        create.attached = true;
        (*create.state).hwnd = hwnd;
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.state as isize);
    }
    let state = palette_state(hwnd);
    if state.is_null() {
        return DefWindowProcW(hwnd, message, w, l);
    }
    let p = (*state).app;
    let point = || POINT {
        x: (l & 0xffff) as i16 as i32,
        y: ((l >> 16) & 0xffff) as i16 as i32,
    };
    match message {
        WM_CREATE => {
            let instance = GetModuleHandleW(null());
            (*state).edit = CreateWindowExW(
                0,
                wide("Edit").as_ptr(),
                wide("").as_ptr(),
                WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                0,
                0,
                0,
                0,
                hwnd,
                PALETTE_EDIT as HMENU,
                instance,
                null(),
            );
            if (*state).edit.is_null() {
                return -1;
            }
            SendMessageW(
                (*state).edit,
                WM_SETFONT,
                (*p).fonts.palette_input as usize,
                1,
            );
            SendMessageW(
                (*state).edit,
                EM_SETMARGINS,
                (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
                0,
            );
            SendMessageW((*state).edit, EM_SETLIMITTEXT, MAX_QUERY, 0);
            // The placeholder for assistive technology; it is painted by
            // `palette_edit_proc` (muted, also while focused).
            SendMessageW(
                (*state).edit,
                EM_SETCUEBANNER,
                0,
                wide(palette_placeholder()).as_ptr() as isize,
            );
            SetWindowSubclass((*state).edit, Some(palette_edit_proc), 1, state as usize);
            (*state).anim.attach(hwnd);
            refresh_palette(state, true);
            // An overflowing list shows its bar briefly on open.
            palette_bar_activity(state);
            SetTimer(hwnd, PALETTE_TIMER, 1000, None);
            0
        }
        WM_SIZE => {
            (*state).back.release();
            let g = palette_geometry(state);
            let dc = GetDC(hwnd);
            let line = if dc.is_null() {
                gfx::pxi((*p).dpi, 20.0)
            } else {
                let line = line_height(dc, (*p).fonts.palette_input);
                ReleaseDC(hwnd, dc);
                line
            };
            // `input { padding: 0 18px }`, the text centred in the 48 px band
            // above its bottom border.
            let pad = gfx::pxi((*p).dpi, PALETTE_INPUT_PAD);
            let band = g.input.bottom - g.hair - g.input.top;
            MoveWindow(
                (*state).edit,
                g.input.left + pad,
                g.input.top + (band - line) / 2,
                (g.input.right - g.input.left - 2 * pad).max(1),
                line,
                1,
            );
            palette_scroll(state, false);
            0
        }
        WM_WINDOWPOSCHANGED => {
            palette_shadow(state);
            DefWindowProcW(hwnd, message, w, l)
        }
        WM_COMMAND => {
            let id = w & 0xffff;
            let notification = (w >> 16) as u32;
            if id == PALETTE_EDIT && notification == EN_CHANGE {
                refresh_palette(state, true);
            }
            0
        }
        WM_TIMER if w == PALETTE_TIMER => {
            refresh_palette(state, false);
            0
        }
        WM_TIMER => {
            if (*state).anim.on_timer(w) {
                palette_frame(state);
                if (*state).closing && !(*state).anim.anim.is_key_animating(SHADOW_KEY) {
                    PostMessageW(hwnd, PALETTE_DONE, 0, 0);
                }
                0
            } else {
                DefWindowProcW(hwnd, message, w, l)
            }
        }
        PALETTE_DONE => {
            DestroyWindow(hwnd);
            0
        }
        // Fading out: no more input.
        WM_MOUSEWHEEL | WM_MOUSEMOVE | WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_LBUTTONUP
            if (*state).closing =>
        {
            0
        }
        WM_MOUSEWHEEL => {
            let delta = (w >> 16) as u16 as i16 as i32;
            let mut lines: u32 = 3;
            SystemParametersInfoW(
                SPI_GETWHEELSCROLLLINES,
                0,
                (&mut lines as *mut u32).cast(),
                0,
            );
            let row = gfx::pxi((*p).dpi, PALETTE_ITEM);
            let (content, view) = palette_extent(state);
            (*state).scroll = ((*state).scroll - delta * lines.min(20) as i32 * row / 120)
                .clamp(0, (content - view).max(0));
            (*state).anim.set_target_smooth(
                SCROLL_KEY,
                (*state).scroll as f32,
                anim::motion::SCROLL,
            );
            palette_bar_activity(state);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        // `.palette li { cursor: pointer }`.
        WM_SETCURSOR if w as HWND == hwnd && (l & 0xffff) as u32 == HTCLIENT => {
            let mut pt: POINT = zeroed();
            GetCursorPos(&mut pt);
            ScreenToClient(hwnd, &mut pt);
            widgets::set_pointer(!(*state).closing && palette_row(state, pt).is_some())
        }
        WM_MOUSEMOVE => {
            let g = palette_geometry(state);
            let pt = point();
            let (content, view) = palette_extent(state);
            let over = content > view
                && pt.x >= g.list.right - gfx::pxi((*p).dpi, 14.0)
                && pt.y >= g.list.top
                && pt.y < g.list.bottom;
            if over != (*state).bar_hover {
                (*state).bar_hover = over;
                (*state).anim.set_target(
                    BAR_KEY,
                    over as u8 as f32,
                    anim::motion::SCROLLBAR_GROW,
                    anim::Easing::EaseOut,
                );
                palette_bar_activity(state);
            }
            let mut event = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut event);
            0
        }
        WM_MOUSELEAVE => {
            if (*state).bar_hover {
                (*state).bar_hover = false;
                (*state).anim.set_target(
                    BAR_KEY,
                    0.0,
                    anim::motion::SCROLLBAR_GROW,
                    anim::Easing::EaseOut,
                );
                palette_bar_activity(state);
            }
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            (*state).pressed = palette_row(state, point());
            if let Some(index) = (*state).pressed {
                (*state).selected = index;
                InvalidateRect(hwnd, null(), 0);
            }
            SetFocus((*state).edit);
            0
        }
        WM_LBUTTONUP => {
            let pressed = (*state).pressed.take();
            if pressed.is_some() && palette_row(state, point()) == pressed {
                execute_palette(state);
            }
            0
        }
        WM_ACTIVATE if w & 0xffff == WA_INACTIVE as usize => {
            if !(*state).closing && (*p).palette == hwnd {
                // Another window took the focus: fade out, give nothing back.
                let to_owner = l as HWND == (*p).hwnd;
                close_palette(p, false);
                if to_owner {
                    let previous = (*state).previous_focus;
                    if IsWindow(previous) != 0 && IsChild((*p).hwnd, previous) != 0 {
                        SetFocus(previous);
                    }
                }
            }
            0
        }
        // The scrim was pressed (its dismiss target).
        WM_CLOSE => {
            if !(*state).closing && (*p).palette == hwnd {
                close_palette(p, true);
            } else if !(*state).closing {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DPICHANGED => {
            DestroyWindow(hwnd);
            0
        }
        WM_PAINT => {
            let mut back = std::mem::take(&mut (*state).back);
            gfx::paint_buffered(hwnd, &mut back, |dc, client| unsafe {
                paint_palette(state, dc, client)
            });
            (*state).back = back;
            0
        }
        WM_ERASEBKGND => 1,
        WM_CTLCOLOREDIT => {
            SetBkColor(w as HDC, colors().surface);
            SetTextColor(w as HDC, colors().fg);
            (*p).surface as isize
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            KillTimer(hwnd, PALETTE_TIMER);
            (*state).anim.stop();
            if (*p).palette == hwnd {
                (*p).palette = null_mut();
            }
            popup::close((*state).scrim);
            let previous_focus = (*state).previous_focus;
            let faded = (*state).closing;
            drop(Box::from_raw(state));
            if faded {
                // The focus went back when the exit fade started.
                return DefWindowProcW(hwnd, message, w, l);
            }
            let foreground = GetForegroundWindow();
            if (foreground == hwnd || foreground == (*p).hwnd)
                && IsWindow((*p).hwnd) != 0
                && IsWindowVisible((*p).hwnd) != 0
                && IsIconic((*p).hwnd) == 0
                && IsWindow(previous_focus) != 0
                && IsChild((*p).hwnd, previous_focus) != 0
            {
                SetFocus(previous_focus);
            }
            DefWindowProcW(hwnd, message, w, l)
        }
        _ => DefWindowProcW(hwnd, message, w, l),
    }
}

/// Preview: the palette over its scrim with its shadow, composited over the
/// window like DWM shows it (rounded corners), the second command selected.
pub(super) unsafe fn save_palette_preview(
    p: *mut App,
    path: &std::path::Path,
) -> Result<(), String> {
    destroy_palette(p);
    create_palette(p, false);
    let hwnd = (*p).palette;
    let state = palette_state(hwnd);
    if state.is_null() {
        return Err("Palette preview failed".into());
    }
    (*state).selected = 1;
    let dpi = (*p).dpi;
    let radius = gfx::px(dpi, theme::RADIUS);
    let c = colors();
    (*p).anim.finish_all();
    let mut client: RECT = zeroed();
    GetClientRect((*p).hwnd, &mut client);
    let mut canvas =
        gfx::Dib::new(client.right, client.bottom).ok_or("Preview allocation failed")?;
    canvas.pixels().fill(0xffff_ffff);
    capture::paint_client_and_children((*p).hwnd, canvas.dc())?;
    let scrim = popup::open_scrim(p, null_mut());
    popup::composite((*p).hwnd, &mut canvas, POINT { x: 0, y: 0 }, &[scrim]);
    popup::destroy(scrim);
    let mut bounds: RECT = zeroed();
    GetWindowRect(hwnd, &mut bounds);
    let mut origin = POINT { x: 0, y: 0 };
    ClientToScreen((*p).hwnd, &mut origin);
    let (w, h) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
    let (x, y) = (bounds.left - origin.x, bounds.top - origin.y);
    let blend = |target: &gfx::Dib, surface: &gfx::LayeredSurface, x: i32, y: i32| {
        let size = surface.size();
        GdiAlphaBlend(
            target.dc(),
            x,
            y,
            size.cx,
            size.cy,
            surface.dc(),
            0,
            0,
            size.cx,
            size.cy,
            BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            },
        );
    };
    // The shadow window's image.
    let mut shadow = gfx::LayeredSurface::new(w, h, &gfx::popup_shadow(dpi))
        .ok_or("Preview allocation failed")?;
    shadow.compose_shadow(radius, c.shadow());
    let m = shadow.margins();
    blend(&canvas, &shadow, x - m.left, y - m.top);
    // The palette itself (painted + its Edit), rounded like DWM does.
    let mut body = gfx::LayeredSurface::new(w, h, &[]).ok_or("Preview allocation failed")?;
    let area = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    paint_palette(state, body.dc(), &area);
    let mut edit: RECT = zeroed();
    GetWindowRect((*state).edit, &mut edit);
    let saved = SaveDC(body.dc());
    SetViewportOrgEx(
        body.dc(),
        edit.left - bounds.left,
        edit.top - bounds.top,
        null_mut(),
    );
    SendMessageW(
        (*state).edit,
        WM_PRINT,
        body.dc() as WPARAM,
        (PRF_CLIENT | PRF_NONCLIENT | PRF_ERASEBKGND) as LPARAM,
    );
    RestoreDC(body.dc(), saved);
    body.compose(radius, c.shadow());
    blend(&canvas, &body, x, y);
    destroy_palette(p);
    capture::save_dib(&mut canvas, path)
}

unsafe fn tray_data(p: *mut App) -> NOTIFYICONDATAW {
    let mut data: NOTIFYICONDATAW = zeroed();
    data.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    data.hWnd = (*p).hwnd;
    data.uID = TRAY_ID;
    data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    data.uCallbackMessage = TRAY_MESSAGE;
    data.hIcon = if (*p).small_icon.is_null() {
        (*p).big_icon
    } else {
        (*p).small_icon
    };
    let tip = wide("Feather Task Manager");
    let length = tip.len().min(data.szTip.len());
    data.szTip[..length].copy_from_slice(&tip[..length]);
    data
}

unsafe fn add_tray(p: *mut App) -> bool {
    let mut data = tray_data(p);
    if data.hIcon.is_null() || Shell_NotifyIconW(NIM_ADD, &data) == 0 {
        return false;
    }
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    // Older notification-area behavior still supports double-click/right-click.
    Shell_NotifyIconW(NIM_SETVERSION, &data);
    (*p).tray_visible = true;
    true
}

pub(super) unsafe fn minimize_to_tray(p: *mut App) {
    if !(*p).prefs.tray {
        return;
    }
    destroy_palette(p);
    if (*p).tray_visible || add_tray(p) {
        ShowWindow((*p).hwnd, SW_HIDE);
    } else {
        (*p).notice = tr(
            "알림 영역 아이콘을 만들 수 없어 작업 표시줄에 유지합니다.",
            "Could not create a notification icon; keeping the window in the taskbar.",
        )
        .into();
    }
}

pub(super) unsafe fn remove_tray(p: *mut App) {
    if (*p).tray_visible {
        Shell_NotifyIconW(NIM_DELETE, &tray_data(p));
        (*p).tray_visible = false;
    }
}

unsafe fn restore_from_tray(p: *mut App) {
    ShowWindow((*p).hwnd, SW_RESTORE);
    SetForegroundWindow((*p).hwnd);
    remove_tray(p);
}

pub(super) unsafe fn taskbar_created(p: *mut App) {
    if !(*p).tray_visible {
        return;
    }
    (*p).tray_visible = false;
    if !add_tray(p) {
        restore_from_tray(p);
    }
}

pub(super) unsafe fn tray_message(p: *mut App, parameter: LPARAM) {
    let message = parameter as u32 & 0xffff;
    match message {
        NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONDBLCLK => restore_from_tray(p),
        WM_CONTEXTMENU | WM_RBUTTONUP => {
            let menu = CreatePopupMenu();
            if menu.is_null() {
                restore_from_tray(p);
                return;
            }
            AppendMenuW(
                menu,
                MF_STRING,
                1,
                wide(tr("Feather 열기", "Show Feather")).as_ptr(),
            );
            AppendMenuW(menu, MF_STRING, 2, wide(tr("종료", "Exit")).as_ptr());
            SetMenuDefaultItem(menu, 1, 0);
            let mut point = POINT::default();
            GetCursorPos(&mut point);
            SetForegroundWindow((*p).hwnd);
            let selected = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                point.x,
                point.y,
                0,
                (*p).hwnd,
                null(),
            );
            DestroyMenu(menu);
            PostMessageW((*p).hwnd, WM_NULL, 0, 0);
            if selected == 1 {
                restore_from_tray(p);
            } else if selected == 2 {
                remove_tray(p);
                PostMessageW((*p).hwnd, WM_CLOSE, 0, 0);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, created: u64, name: &str) -> Process {
        Process {
            pid,
            parent_pid: 0,
            created,
            name: name.into(),
            cpu_percent: 1.0,
            working_set: 0,
            private_bytes: 0,
            io_bytes_per_sec: 0.0,
            gpu_percent: None,
            network_bytes_per_sec: None,
            threads: 1,
            handles: 1,
            ..Process::default()
        }
    }

    #[test]
    fn palette_search_is_bounded_and_captures_process_generation() {
        let processes: Vec<_> = (1..=20)
            .map(|pid| process(pid, u64::from(pid) + 100, "test.exe"))
            .collect();
        let items = palette_items(
            "test",
            command_items(false, false, false, false),
            &processes,
        );
        assert_eq!(items.len(), MAX_RESULTS);
        assert_eq!(
            items[0].action,
            PaletteAction::Process(ProcessIdentity {
                pid: 1,
                created: 101
            })
        );
        let by_pid = palette_items("20", Vec::new(), &processes);
        assert_eq!(by_pid.len(), 1);
        assert_eq!(
            by_pid[0].action,
            PaletteAction::Process(ProcessIdentity {
                pid: 20,
                created: 120
            })
        );
        assert!(palette_items("unmatched", Vec::new(), &processes).is_empty());
    }

    #[test]
    fn palette_empty_search_contains_commands_only_and_actions_match_state() {
        let commands = command_items(true, true, false, false);
        // 5 pages, pause, refresh, run task, Resource Monitor, always on top,
        // Nuclear Zombie, theme.
        assert_eq!(commands.len(), 12);
        for command in [
            PAUSE,
            REFRESH,
            RUN_TASK,
            RESOURCE_MONITOR,
            PREF_TOP,
            NUCLEAR,
        ] {
            assert!(
                commands
                    .iter()
                    .any(|item| item.action == PaletteAction::Command(command)),
                "command {command} must stay reachable from the palette"
            );
        }
        assert!(commands
            .iter()
            .any(|item| item.action == PaletteAction::Command(THEME_LIGHT)));
        assert!(!commands
            .iter()
            .any(|item| item.action == PaletteAction::Command(PRIMARY)));
        assert_eq!(
            palette_items("  ", commands, &[process(1, 1, "test")]).len(),
            12
        );
        // Searching "memory" or "zombie" (either language) finds the panel.
        for query in ["memory", "Zombie", "메모리", "좀비"] {
            assert!(
                palette_items(query, command_items(false, false, false, false), &[])
                    .iter()
                    .any(|item| item.action == PaletteAction::Command(NUCLEAR)),
                "{query}"
            );
        }
        assert!(
            palette_items("refresh", command_items(false, false, false, true), &[])
                .iter()
                .any(|item| item.action == PaletteAction::Command(REFRESH))
        );
        assert!(command_items(false, false, true, false)
            .iter()
            .any(|item| item.action == PaletteAction::Command(THEME_DARK)));
    }

    /// Only an invisible, disposable owner/palette is created. No notification
    /// icon, persisted preference or real process operation is exercised here.
    #[test]
    fn native_palette_search_navigation_and_close_keep_owner_alive() {
        unsafe {
            init_controls();
            let (_snapshots, rx) = mpsc::sync_channel(1);
            let (tx, _commands) = mpsc::channel();
            let (jobs, _jobs) = mpsc::channel();
            let (_complete, results) = mpsc::channel();
            let p = Box::into_raw(Box::new(App::new(rx, tx, jobs, results)));
            (*p).hwnd = CreateWindowExW(
                0,
                wide("Static").as_ptr(),
                wide("Feather palette test owner").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                1000,
                700,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!(*p).hwnd.is_null());
            create_fonts(p);
            create_palette(p, false);
            assert!(!(*p).palette.is_null());
            assert_eq!(IsWindowVisible((*p).palette), 0);
            let state = palette_state((*p).palette);
            assert_eq!((*state).items.len(), 12);
            let key = |value: u16| MSG {
                hwnd: (*state).edit,
                message: WM_KEYDOWN,
                wParam: value as usize,
                ..zeroed()
            };
            assert!(palette_key(p, &key(VK_DOWN)));
            assert_eq!((*state).selected, 1);
            assert!(palette_key(p, &key(VK_UP)));
            assert_eq!((*state).selected, 0);
            assert!(palette_key(p, &key(VK_UP)), "wraps to the last command");
            assert_eq!((*state).selected, 11);
            // `max-height: 320px`: 12 commands scroll; the last one is in view.
            let (content, view) = palette_extent(state);
            assert!(content > view);
            assert_eq!((*state).scroll, content - view);
            let mut bounds = RECT::default();
            GetWindowRect((*p).palette, &mut bounds);
            assert_eq!(bounds.bottom - bounds.top, 1 + 48 + 320 + 1);
            assert_eq!(bounds.right - bounds.left, 560);
            SetWindowTextW((*state).edit, wide("unmatched-command-985413").as_ptr());
            assert_eq!((*state).items.len(), 0);
            GetWindowRect((*p).palette, &mut bounds);
            assert_eq!(bounds.bottom - bounds.top, palette_height(96, 0));
            assert!(palette_key(p, &key(VK_ESCAPE)));
            assert!((*p).palette.is_null());
            assert_ne!(IsWindow((*p).hwnd), 0);
            DestroyWindow((*p).hwnd);
            dispose(p);
        }
    }
}
