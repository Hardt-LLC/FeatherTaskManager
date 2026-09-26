//! Native controls and explicit actions for the supplied Feather layout.
use super::*;
use windows_sys::Win32::System::{DataExchange::*, Memory::*};
use windows_sys::Win32::UI::Controls::Dialogs::*;

pub(super) unsafe fn populate_filters(p: *mut App) {
    SendMessageW((*p).filter_control, CB_RESETCONTENT, 0, 0);
    let labels = match (*p).page {
        Page::Startup => vec![
            tr("전체", "All apps"),
            tr("사용", "Enabled"),
            tr("사용 안 함", "Disabled"),
        ],
        Page::Services => vec![
            tr("전체", "All services"),
            tr("실행 중", "Running"),
            tr("중지됨", "Stopped"),
            tr("자동", "Automatic"),
            tr("수동", "Manual"),
            tr("사용 안 함", "Disabled"),
        ],
        _ => vec![tr("전체", "All processes")],
    };
    for label in labels {
        SendMessageW(
            (*p).filter_control,
            CB_ADDSTRING,
            0,
            wide(label).as_ptr() as isize,
        );
    }
    SendMessageW((*p).filter_control, CB_SETCURSEL, (*p).category, 0);
}
pub(super) unsafe fn category_matches(p: *mut App, row: usize) -> bool {
    match ((*p).page, (*p).category) {
        (Page::Startup, 1) => (&(*p).startup).get(row).is_some_and(|s| s.enabled),
        (Page::Startup, 2) => (&(*p).startup).get(row).is_some_and(|s| !s.enabled),
        (Page::Services, n @ 1..=5) => (&(*p).services).get(row).is_some_and(|s| match n {
            1 => s.state == SERVICE_RUNNING,
            2 => s.state == SERVICE_STOPPED,
            3 => s.start_type == Some(2),
            4 => s.start_type == Some(3),
            _ => s.start_type == Some(4),
        }),
        _ => true,
    }
}
pub(super) unsafe fn selection_changed(p: *mut App) {
    if (*p).page == Page::Processes {
        let process = selected_process(p);
        // A disappeared selection keeps an ended trace until the user chooses another row.
        if process.is_some() {
            (*p).telemetry.select(process.as_ref());
        }
        if let Some(process) = process {
            let id = (process.pid, process.created);
            if (*p).process_detail_identity != Some(id) {
                (*p).process_detail_identity = Some(id);
                (*p).process_settings = None;
                let _ = (*p).jobs.send(Job::ProcessDetails(id.0, id.1));
            }
        }
    } else if (*p).page == Page::Services {
        let name = selected_row(p)
            .and_then(|r| (&(*p).services).get(r))
            .map(|s| s.name.clone());
        if (*p).service_detail_name != name {
            (*p).service_detail_name = name.clone();
            (*p).service_details = None;
            (*p).service_detail_error = None;
            if let Some(name) = name {
                let _ = (*p).jobs.send(Job::ServiceDetails(name));
            }
        }
    }
}
pub(super) unsafe fn refresh_components(p: *mut App) {
    let mut targets = vec![PerfTarget::Cpu, PerfTarget::Memory];
    if let Some(perf) = &(*p).performance {
        targets.extend(
            perf.disks
                .iter()
                .take(128)
                .map(|s| PerfTarget::Disk(s.id.clone())),
        );
        targets.extend(
            perf.networks
                .iter()
                .filter(|s| listed_network(s))
                .take(128)
                .map(|s| PerfTarget::Network(s.id)),
        );
        targets.extend(
            perf.gpus
                .iter()
                .filter(|s| listed_gpu(s))
                .take(64)
                .map(|s| PerfTarget::Gpu(s.id.clone())),
        );
    }
    if targets != (*p).perf_targets || SendMessageW((*p).perf_list, LB_GETCOUNT, 0, 0) == 0 {
        (*p).perf_targets = targets;
        if !(*p).perf_targets.contains(&(*p).perf_target) {
            let selected = (*p).perf_target.clone();
            (*p).perf_targets.push(selected);
        }
        SendMessageW((*p).perf_list, WM_SETREDRAW, 0, 0);
        SendMessageW((*p).perf_list, LB_RESETCONTENT, 0, 0);
        for target in &(*p).perf_targets {
            let name = component_name(p, target);
            SendMessageW(
                (*p).perf_list,
                LB_ADDSTRING,
                0,
                wide(&name).as_ptr() as isize,
            );
        }
        let index = (*p)
            .perf_targets
            .iter()
            .position(|t| t == &(*p).perf_target)
            .unwrap_or(0);
        SendMessageW((*p).perf_list, LB_SETCURSEL, index, 0);
        SendMessageW((*p).perf_list, WM_SETREDRAW, 1, 0);
    }
    InvalidateRect((*p).perf_list, null(), 0);
}
pub(super) unsafe fn component_name(p: *mut App, target: &PerfTarget) -> String {
    match target {
        PerfTarget::Cpu => "CPU".into(),
        PerfTarget::Memory => tr("메모리", "Memory").into(),
        PerfTarget::Disk(id) => paint::disk_name(id),
        PerfTarget::Network(id) => (*p)
            .performance
            .as_ref()
            .and_then(|s| s.networks.iter().find(|n| n.id == *id))
            .map_or_else(|| "Network".into(), |n| n.name.clone()),
        PerfTarget::Gpu(id) => {
            let index = (*p)
                .performance
                .as_ref()
                .and_then(|s| {
                    s.gpus
                        .iter()
                        .filter(|g| listed_gpu(g))
                        .position(|n| &n.id == id)
                })
                .unwrap_or(0);
            format!("GPU {index}")
        }
    }
}
unsafe fn save(p: *mut App) {
    if !(*p).persist_preferences {
        return;
    }
    if let Err(e) = (*p).prefs.save() {
        (*p).set_error(ErrorSource::Action, e);
    }
}
pub(super) unsafe fn rate_changed(p: *mut App, control: HWND) {
    let i = SendMessageW(control, CB_GETCURSEL, 0, 0) as usize;
    let value = [0, 250, 500, 1000, 2000, 5000]
        .get(i)
        .copied()
        .unwrap_or(1000);
    (*p).paused = value == 0;
    if value > 0 {
        (*p).interval = value;
        (*p).prefs.rate = value;
        save(p);
    }
    sync_settings(p);
    configure(p);
    redraw(p);
}
pub(super) unsafe fn apply_theme(p: *mut App) {
    // Every window switches in the same presented frame (`present_all`).
    let suspended = suspend_painting(p);
    shell::destroy_palette(p);
    (*p).prefs.apply_theme();
    let old_bg = (*p).bg;
    let old_surface = (*p).surface;
    (*p).bg = CreateSolidBrush(colors().bg);
    (*p).surface = CreateSolidBrush(colors().surface);
    DeleteObject(old_bg);
    DeleteObject(old_surface);
    SendMessageW((*p).list, LVM_SETBKCOLOR, 0, colors().surface as isize);
    SendMessageW((*p).list, LVM_SETTEXTBKCOLOR, 0, colors().surface as isize);
    SendMessageW((*p).list, LVM_SETTEXTCOLOR, 0, colors().fg as isize);
    // DWM dark mode, Windows 11 corners and border color (custom frame).
    frame::apply_theme(p);
    present_all(p, suspended);
    // Layered popups keep the palette they were opened with; repaint them.
    popup::theme_changed();
}
unsafe fn combo(p: *mut App, id: usize, label: &str) -> HWND {
    controls::select(p, label, id)
}
pub(super) unsafe fn create_settings_controls(p: *mut App) {
    for (id, label) in [
        (THEME_LIGHT, tr("밝게", "Light")),
        (THEME_DARK, tr("어둡게", "Dark")),
        (THEME_SYSTEM, tr("시스템", "System")),
        (PREF_TOP, tr("항상 위", "Always on top")),
        (PREF_TRAY, tr("트레이로 최소화", "Minimize to tray")),
        (PREF_REPLACE, tr("작업 관리자 대체", "Replace Task Manager")),
    ] {
        let h = button(p, label, id);
        (*p).preference_controls.push(h);
    }
    for (id, label) in [
        (PREF_LANGUAGE, tr("언어", "Language")),
        (PREF_RATE, tr("새로 고침 간격", "Refresh rate")),
        (PREF_START, tr("시작 화면", "Start page")),
    ] {
        let h = combo(p, id, label);
        (*p).preference_controls.push(h);
    }
    populate_settings(p);
}
pub(super) unsafe fn populate_settings(p: *mut App) {
    for id in [RATE, PREF_RATE] {
        let h = GetDlgItem((*p).hwnd, id as i32);
        SendMessageW(h, CB_RESETCONTENT, 0, 0);
        for value in rate_labels() {
            SendMessageW(h, CB_ADDSTRING, 0, wide(value).as_ptr() as isize);
        }
    }
    let start = GetDlgItem((*p).hwnd, PREF_START as i32);
    SendMessageW(start, CB_RESETCONTENT, 0, 0);
    for i in 0..4 {
        SendMessageW(
            start,
            CB_ADDSTRING,
            0,
            wide(Page::from_index(i).title()).as_ptr() as isize,
        );
    }
    let lang = GetDlgItem((*p).hwnd, PREF_LANGUAGE as i32);
    SendMessageW(lang, CB_RESETCONTENT, 0, 0);
    for name in ["한국어", "English"] {
        SendMessageW(lang, CB_ADDSTRING, 0, wide(name).as_ptr() as isize);
    }
    for (h, label) in [
        ((*p).extra, tr("효율 모드", "Efficiency mode")),
        ((*p).copy, tr("정보 복사", "Copy info")),
        ((*p).cores, tr("코어별 보기", "Logical CPUs")),
        (
            (*p).resource_monitor,
            tr("리소스 모니터", "Resource Monitor"),
        ),
        ((*p).run_task, tr("새 작업 실행", "Run new task")),
        ((*p).expand_all, tr("모두 펼치기", "Expand all")),
    ] {
        SetWindowTextW(h, wide(label).as_ptr());
    }
    sync_settings(p);
}
pub(super) unsafe fn sync_settings(p: *mut App) {
    let rate = if (*p).paused {
        0
    } else {
        [0, 250, 500, 1000, 2000, 5000]
            .iter()
            .position(|r| *r == (*p).interval)
            .unwrap_or(3)
    };
    for id in [RATE, PREF_RATE] {
        SendMessageW(GetDlgItem((*p).hwnd, id as i32), CB_SETCURSEL, rate, 0);
    }
    SendMessageW(
        GetDlgItem((*p).hwnd, PREF_START as i32),
        CB_SETCURSEL,
        (*p).prefs.default_page as usize,
        0,
    );
    SendMessageW(
        GetDlgItem((*p).hwnd, PREF_LANGUAGE as i32),
        CB_SETCURSEL,
        (language() == Language::English) as usize,
        0,
    );
    for h in &(*p).preference_controls {
        InvalidateRect(*h, null(), 0);
    }
    SetWindowTextW(
        (*p).pause,
        wide(if (*p).paused {
            tr("계속", "Resume")
        } else {
            tr("일시정지", "Pause")
        })
        .as_ptr(),
    );
}
/// Settings controls, right-aligned in their rows (DESIGN_SPEC §4 Settings):
/// the theme segments edge to edge, selects at their intrinsic width, and the
/// switches as a 40 × 20 box inside a 4 px margin for the focus ring.
pub(super) unsafe fn layout_settings(p: *mut App, l: &layout::Layout) {
    let show = (*p).page == Page::Settings;
    let mut appearing = false;
    for &h in &(*p).preference_controls {
        let visible = GetWindowLongW(h, GWL_STYLE) as u32 & WS_VISIBLE != 0;
        if visible != show {
            appearing |= show;
            ShowWindow(h, if show { SW_SHOW } else { SW_HIDE });
        }
    }
    if !show {
        return;
    }
    if appearing {
        // The switches show their state as the page appears: a change made
        // elsewhere (palette, ⋯ menu) while the page was hidden must not
        // play a stale slide now.
        for (id, on) in [
            (PREF_TOP, (*p).topmost),
            (PREF_TRAY, (*p).prefs.tray),
            (PREF_REPLACE, (*p).replacement_active),
        ] {
            (*p).anim.set((id, anim::part::SWITCH), on as u8 as f32);
        }
    }
    let geometry = l.settings();
    let row = |group: usize, index: usize| {
        let g = &geometry.groups[group];
        (g.rows[index], g.row_content(index, l.dpi))
    };
    let dc = GetDC((*p).hwnd);
    if dc.is_null() {
        return;
    }
    let f = &(*p).fonts;
    let hair = l.hair;
    // Theme: Light | Dark | System, `.seg button { padding: 6px 14px }`.
    // Centred on the row's unrounded text block, snapped per edge (Chromium
    // keeps the fractional box and snaps it when painting).
    let (_, theme_content) = row(0, 0);
    let (block_top, block) = geometry.groups[0].content_y(0, l.dpi);
    let height = gfx::px(l.dpi, 12.0 + layout::SETTINGS_ROW_TITLE) + 2.0 * hair as f32;
    let top_f = block_top + (block - height) / 2.0;
    let (top, bottom) = (top_f.round() as i32, (top_f + height).round() as i32);
    // Segment widths on Chromium's fractional advances, the edges
    // accumulated from the right and each one snapped (an inline-flex box
    // of fractional buttons).
    let old = SelectObject(dc, f.ui);
    let mut right = theme_content.right as f32;
    for (id, last) in [
        (THEME_SYSTEM, true),
        (THEME_DARK, false),
        (THEME_LIGHT, false),
    ] {
        let h = GetDlgItem((*p).hwnd, id as i32);
        let width = fonts::ideal_width(dc, &paint::window_text(h))
            + gfx::px(l.dpi, 28.0)
            + hair as f32
            + if last { hair as f32 } else { 0.0 };
        place(
            p,
            h,
            RECT {
                left: (right - width).round() as i32,
                top,
                right: right.round() as i32,
                bottom,
            },
        );
        right -= width;
    }
    SelectObject(dc, old);
    for (id, group, index) in [(PREF_LANGUAGE, 0, 1), (PREF_RATE, 1, 0), (PREF_START, 1, 1)] {
        let h = GetDlgItem((*p).hwnd, id as i32);
        let (r, content) = row(group, index);
        let width = select_width(p, dc, h, f.small);
        let (top, bottom) = layout::Layout::center_v(r, l.px(layout::SELECT_HEIGHT));
        place(
            p,
            h,
            RECT {
                left: content.right - width,
                top,
                right: content.right,
                bottom,
            },
        );
    }
    ReleaseDC((*p).hwnd, dc);
    for (id, index) in [(PREF_TOP, 0), (PREF_TRAY, 1), (PREF_REPLACE, 2)] {
        let h = GetDlgItem((*p).hwnd, id as i32);
        let (r, content) = row(2, index);
        let margin = l.px(4.0);
        let (top, bottom) = layout::Layout::center_v(r, l.px(20.0) + 2 * margin);
        place(
            p,
            h,
            RECT {
                left: content.right - l.px(40.0) - margin,
                top,
                right: content.right + margin,
                bottom,
            },
        );
    }
}
pub(super) unsafe fn command(p: *mut App, id: usize, notification: u32) -> bool {
    match id {
        MORE if !(*p).busy && !(*p).modal => more_menu(p),
        FILTER if notification == CBN_SELCHANGE => {
            (*p).category = SendMessageW((*p).filter_control, CB_GETCURSEL, 0, 0).max(0) as usize;
            rebuild(p, selected_identity(p));
            redraw(p);
        }
        PERF_COMPONENT if notification == LBN_SELCHANGE => {
            let index = SendMessageW((*p).perf_list, LB_GETCURSEL, 0, 0) as usize;
            if let Some(target) = (&(*p).perf_targets).get(index).cloned() {
                (*p).perf_target = target;
                redraw(p);
            }
        }
        CORES => {
            (*p).core_graphs = !(*p).core_graphs;
            InvalidateRect((*p).cores, null(), 0);
            redraw(p);
        }
        COPY => {
            let text = performance_text(p);
            report_copy(p, &text);
        }
        RESOURCE_MONITOR => begin_action(
            p,
            Action::SystemTool(crate::actions::SystemTool::ResourceMonitor),
        ),
        EXPAND_ALL => {
            (*p).collapsed.clear();
            // The grouped view: open every app that has processes.
            if (*p).group_mode && !(*p).tree_mode {
                if let Some(snapshot) = (*p).snapshot.as_ref() {
                    for row in (&(*p).tree_rows).iter().filter(|r| r.has_children) {
                        if let Some(process) = snapshot.processes.get(row.index) {
                            (*p).expanded_groups.insert(ProcessIdentity::from(process));
                        }
                    }
                }
            }
            rebuild(p, selected_identity(p));
            redraw(p);
        }
        RUN_TASK if !(*p).busy && !(*p).modal => run_task(p),
        EXTRA if !(*p).busy && !(*p).modal => {
            if (*p).page == Page::Services {
                restart_service(p);
            } else {
                efficiency(p);
            }
        }
        THEME_LIGHT | THEME_DARK | THEME_SYSTEM => {
            (*p).prefs.theme = match id {
                THEME_LIGHT => 1,
                THEME_DARK => 2,
                _ => 0,
            };
            save(p);
            apply_theme(p);
            sync_settings(p);
        }
        PREF_LANGUAGE if notification == CBN_SELCHANGE => {
            let choice = if SendMessageW(
                GetDlgItem((*p).hwnd, PREF_LANGUAGE as i32),
                CB_GETCURSEL,
                0,
                0,
            ) == 1
            {
                Language::English
            } else {
                Language::Korean
            };
            match set_language(choice) {
                Ok(()) => {
                    refresh_language(p);
                    populate_settings(p);
                }
                Err(e) => (*p).set_error(ErrorSource::Action, e),
            }
            redraw(p);
        }
        PREF_RATE if notification == CBN_SELCHANGE => {
            rate_changed(p, GetDlgItem((*p).hwnd, PREF_RATE as i32))
        }
        PREF_START if notification == CBN_SELCHANGE => {
            (*p).prefs.default_page =
                SendMessageW(GetDlgItem((*p).hwnd, PREF_START as i32), CB_GETCURSEL, 0, 0)
                    .clamp(0, 3) as u32;
            save(p);
        }
        PREF_TOP => {
            super::command(p, TOP, 0);
            (*p).prefs.topmost = (*p).topmost;
            save(p);
            sync_settings(p);
            redraw(p);
        }
        PREF_TRAY => {
            (*p).prefs.tray = !(*p).prefs.tray;
            save(p);
            sync_settings(p);
            redraw(p);
        }
        PREF_REPLACE if !(*p).busy && !(*p).modal => {
            let active = matches!(
                crate::replacement::status(),
                Ok(crate::replacement::Status::Active)
            );
            let prompt = if active {
                tr(
                    "Windows 기본 작업 관리자로 복원할까요? 관리자 권한이 필요합니다.",
                    "Restore the Windows Task Manager? Administrator permission is required.",
                )
            } else {
                tr("설치된 Feather를 Windows 작업 관리자로 설정할까요? Ctrl+Shift+Esc와 Ctrl+Alt+Delete에 적용되며 관리자 권한이 필요합니다.","Use installed Feather as the Windows Task Manager? This applies to Ctrl+Shift+Esc and Ctrl+Alt+Delete and requires administrator permission.")
            };
            let action = if active {
                tr("Windows 작업 관리자 복원", "Restore Task Manager")
            } else {
                tr("작업 관리자로 설정", "Replace Task Manager")
            };
            if confirm_with(p, action, prompt, None, false) {
                begin_action(p, Action::ReplaceTaskManager(!active, Page::Settings));
            }
        }
        _ => return false,
    }
    true
}
unsafe fn efficiency(p: *mut App) {
    if let (Some(process), Some(enabled)) = (
        selected_process(p),
        (*p).process_settings.as_ref().and_then(|s| s.efficiency),
    ) {
        begin_action(
            p,
            Action::Efficiency(process.pid, process.created, !enabled),
        );
    }
}
unsafe fn restart_service(p: *mut App) {
    if let Some(service) = selected_row(p)
        .and_then(|i| (&(*p).services).get(i))
        .cloned()
    {
        if service.state == SERVICE_RUNNING
            && confirm(
                p,
                tr("서비스 다시 시작", "Restart service"),
                &tf!(
                    "{} 서비스를 다시 시작할까요? 이를 사용하는 기능이 일시적으로 중단됩니다.",
                    "Restart {}? Features using this service will be temporarily interrupted.",
                    service.display_name
                ),
            )
        {
            begin_action(p, Action::Restart(service.name, service.display_name));
        }
    }
}
pub(super) unsafe fn startup_click(p: *mut App, click: &NMITEMACTIVATE) {
    if click.iItem < 0 || (*p).busy || (*p).modal || (*p).startup_loading {
        return;
    }
    // Only the switch hitbox acts. Clicking the row itself just selects it.
    if table::part_at((*p).list, click.ptAction)
        != Some((click.iItem as usize, table::Part::Switch))
    {
        return;
    }
    if let Some(entry) = (&(*p).rows)
        .get(click.iItem as usize)
        .and_then(|row| (&(*p).startup).get(*row))
        .filter(|s| s.manageable)
        .cloned()
    {
        let enabled = !entry.enabled;
        toggle_startup(p, entry, enabled);
    }
}
pub(super) unsafe extern "system" fn search_subclass(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    // The app's own Undo / Cut / Copy / Paste menu, never the stock Edit menu.
    if msg == WM_CONTEXTMENU {
        edit_menu(data as *mut App, hwnd, l);
        return 0;
    }
    let was_empty = edits_text(msg).then(|| GetWindowTextLengthW(hwnd) == 0);
    let result = DefSubclassProc(hwnd, msg, w, l);
    if let Some(was_empty) = was_empty {
        placeholder_changed(hwnd, was_empty);
    }
    // The search box frame (border turns fg while focused) belongs to the
    // parent's title strip painting.
    if matches!(msg, WM_SETFOCUS | WM_KILLFOCUS | WM_ENABLE) {
        let p = data as *mut App;
        let frame = current_layout(p).search;
        InvalidateRect((*p).hwnd, &frame, 0);
    }
    // `.search input::placeholder { color: muted }`, focused or not (the
    // native cue banner is never drawn); the caret is hidden around the
    // extra paint.
    if matches!(msg, WM_PAINT | WM_PRINTCLIENT) && GetWindowTextLengthW(hwnd) == 0 {
        let dc = if msg == WM_PRINTCLIENT {
            w as HDC
        } else {
            GetDC(hwnd)
        };
        if !dc.is_null() {
            let focused = msg == WM_PAINT && GetFocus() == hwnd;
            if focused {
                HideCaret(hwnd);
            }
            let mut r: RECT = zeroed();
            GetClientRect(hwnd, &mut r);
            FillRect(dc, &r, (*(data as *mut App)).surface);
            paint::search_cue(data as *mut App, dc, r);
            if focused {
                ShowCaret(hwnd);
            }
            if msg != WM_PRINTCLIENT {
                ReleaseDC(hwnd, dc);
            }
        }
    }
    if msg == WM_NCDESTROY {
        RemoveWindowSubclass(hwnd, Some(search_subclass), id);
    }
    result
}
/// Messages that can change an Edit's text (never WM_GETTEXTLENGTH itself:
/// measuring inside the subclass would recurse).
pub(super) fn edits_text(msg: u32) -> bool {
    matches!(
        msg,
        WM_CHAR
            | WM_KEYDOWN
            | WM_PASTE
            | WM_CUT
            | WM_CLEAR
            | WM_UNDO
            | EM_UNDO
            | EM_REPLACESEL
            | WM_SETTEXT
            | WM_IME_CHAR
            | WM_IME_COMPOSITION
    )
}
/// The input became empty or stopped being empty: repaint it whole, so the
/// painted placeholder appears / disappears at once (the Edit itself only
/// repaints the characters that changed).
pub(super) unsafe fn placeholder_changed(edit: HWND, was_empty: bool) {
    if IsWindow(edit) != 0 && (GetWindowTextLengthW(edit) == 0) != was_empty {
        InvalidateRect(edit, null(), 0);
    }
}
/// Commands of the text inputs' context menu.
const EDIT_UNDO: usize = 600;
const EDIT_CUT: usize = 601;
const EDIT_COPY: usize = 602;
const EDIT_PASTE: usize = 603;
const EDIT_DELETE: usize = 604;
const EDIT_SELECT_ALL: usize = 605;
/// The search box's / palette input's context menu (WM_CONTEXTMENU, `l` =
/// its lParam): the app's layered menu in the app's language instead of the
/// stock Edit menu — Undo, Cut, Copy, Paste, Delete, Select all with their
/// shortcuts, enabled by what the input can do now. The keyboard (Shift+F10
/// / Apps, lParam −1) opens it at the caret.
pub(super) unsafe fn edit_menu(p: *mut App, edit: HWND, l: LPARAM) {
    if (*p).modal || (*p).hwnd.is_null() {
        return;
    }
    let (mut start, mut end) = (0u32, 0u32);
    SendMessageW(
        edit,
        EM_GETSEL,
        &mut start as *mut u32 as usize,
        &mut end as *mut u32 as isize,
    );
    let length = GetWindowTextLengthW(edit).max(0) as u32;
    let selection = end > start;
    let writable = GetWindowLongW(edit, GWL_STYLE) as u32 & ES_READONLY as u32 == 0;
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    for (id, label, enabled) in [
        (
            EDIT_UNDO,
            tr("실행 취소\tCtrl+Z", "Undo\tCtrl+Z"),
            writable && SendMessageW(edit, EM_CANUNDO, 0, 0) != 0,
        ),
        (0, "", false),
        (
            EDIT_CUT,
            tr("잘라내기\tCtrl+X", "Cut\tCtrl+X"),
            writable && selection,
        ),
        (EDIT_COPY, tr("복사\tCtrl+C", "Copy\tCtrl+C"), selection),
        (
            EDIT_PASTE,
            tr("붙여넣기\tCtrl+V", "Paste\tCtrl+V"),
            writable && IsClipboardFormatAvailable(13) != 0,
        ),
        (
            EDIT_DELETE,
            tr("삭제\tDel", "Delete\tDel"),
            writable && selection,
        ),
        (0, "", false),
        (
            EDIT_SELECT_ALL,
            tr("모두 선택\tCtrl+A", "Select all\tCtrl+A"),
            length > 0 && (start > 0 || end < length),
        ),
    ] {
        if id == 0 {
            AppendMenuW(menu, MF_SEPARATOR, 0, null());
        } else {
            AppendMenuW(
                menu,
                MF_STRING | if enabled { 0 } else { MF_GRAYED },
                id,
                wide(label).as_ptr(),
            );
        }
    }
    let keyboard = (l & 0xffff) as u16 as i16 == -1 && ((l >> 16) & 0xffff) as u16 as i16 == -1;
    let anchor = if keyboard {
        // Below the caret (or the input's left edge without one).
        let mut caret: POINT = zeroed();
        if GetCaretPos(&mut caret) == 0 {
            caret = POINT { x: 0, y: 0 };
        }
        let mut client: RECT = zeroed();
        GetClientRect(edit, &mut client);
        let mut at = POINT {
            x: caret.x,
            y: client.bottom,
        };
        ClientToScreen(edit, &mut at);
        popup::Anchor::Point(at)
    } else {
        popup::Anchor::Point(POINT {
            x: (l & 0xffff) as i16 as i32,
            y: ((l >> 16) & 0xffff) as i16 as i32,
        })
    };
    (*p).modal = true;
    configure(p);
    let id = popup::track_menu(p, menu, anchor);
    DestroyMenu(menu);
    finish_modal(p);
    if IsWindow(edit) == 0 {
        return;
    }
    if GetFocus() != edit {
        SetFocus(edit);
    }
    let was_empty = GetWindowTextLengthW(edit) == 0;
    match id {
        EDIT_UNDO => {
            SendMessageW(edit, EM_UNDO, 0, 0);
        }
        EDIT_CUT => {
            SendMessageW(edit, WM_CUT, 0, 0);
        }
        EDIT_COPY => {
            SendMessageW(edit, WM_COPY, 0, 0);
        }
        EDIT_PASTE => {
            SendMessageW(edit, WM_PASTE, 0, 0);
        }
        EDIT_DELETE => {
            SendMessageW(edit, WM_CLEAR, 0, 0);
        }
        EDIT_SELECT_ALL => {
            SendMessageW(edit, EM_SETSEL, 0, -1);
        }
        _ => {}
    }
    placeholder_changed(edit, was_empty);
}
/// The table's context menu, at the cursor.
pub(super) unsafe fn extra_menu(p: *mut App) {
    let mut point: POINT = zeroed();
    GetCursorPos(&mut point);
    extra_menu_at(p, popup::Anchor::Point(point));
}
/// The table's keyboard context menu (Apps / Shift+F10): left-aligned under
/// the selected row's first cell (`anchor`, screen px), flipping above it
/// near the bottom, like a list view's keyboard menu.
pub(super) unsafe fn extra_menu_below(p: *mut App, anchor: RECT) {
    extra_menu_at(
        p,
        popup::Anchor::Below {
            r: anchor,
            right: false,
        },
    );
}
/// The page head's "⋯" button: the same menu, right-aligned under the button
/// (flipping above it when there is no room below).
pub(super) unsafe fn more_menu(p: *mut App) {
    let mut bounds: RECT = zeroed();
    GetWindowRect((*p).more, &mut bounds);
    extra_menu_at(
        p,
        popup::Anchor::Below {
            r: bounds,
            right: true,
        },
    );
}
/// The extra actions as the layered menu (popup.rs) at `anchor`.
unsafe fn extra_menu_at(p: *mut App, anchor: popup::Anchor) {
    if (*p).busy || (*p).modal {
        return;
    }
    let process = selected_process(p);
    let service = selected_row(p)
        .and_then(|i| (&(*p).services).get(i))
        .cloned();
    let menu = build_extra_menu(p, process.is_some(), service.as_ref());
    if menu.is_null() {
        return;
    }
    // The chosen command may disable the ⋯ button that has the focus
    // (End task, a busy action); the focus then goes back where it belongs.
    let focus = GetFocus();
    (*p).modal = true;
    configure(p);
    let id = popup::track_menu(p, menu, anchor);
    DestroyMenu(menu);
    finish_modal(p);
    dispatch_extra(p, id, process);
    restore_focus(p, focus);
}
/// The page's extra actions menu (the ⋯ button and the list context menu).
/// The caller owns (destroys) the returned menu; null on failure.
pub(super) unsafe fn build_extra_menu(
    p: *mut App,
    has_process: bool,
    service: Option<&crate::services::Service>,
) -> HMENU {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return menu;
    }
    let add = |id, text: &str, enabled: bool| {
        AppendMenuW(
            menu,
            MF_STRING | if enabled { 0 } else { MF_GRAYED },
            id,
            wide(text).as_ptr(),
        );
    };
    if (*p).page == Page::Processes {
        add(
            PRIMARY,
            tr("작업 끝내기\tDel", "End task\tDel"),
            IsWindowEnabled((*p).primary) != 0,
        );
        add(
            END_TREE,
            tr("트리 전체 종료\tShift+Del", "End process tree\tShift+Del"),
            IsWindowEnabled((*p).end_tree) != 0,
        );
        add(
            SECONDARY,
            tr("파일 위치 열기\tCtrl+L", "Open file location\tCtrl+L"),
            has_process,
        );
        add(
            EXTRA,
            tr("효율 모드 전환", "Toggle efficiency mode"),
            (*p).process_settings
                .as_ref()
                .and_then(|s| s.efficiency)
                .is_some(),
        );
        // Priority is a submenu with the current class checked.
        let priorities = CreatePopupMenu();
        if !priorities.is_null() {
            for (i, priority) in [
                crate::actions::Priority::Idle,
                crate::actions::Priority::BelowNormal,
                crate::actions::Priority::Normal,
                crate::actions::Priority::AboveNormal,
            ]
            .iter()
            .enumerate()
            {
                let current = (*p)
                    .process_settings
                    .as_ref()
                    .is_some_and(|s| s.priority == Some(*priority));
                AppendMenuW(
                    priorities,
                    MF_STRING
                        | if has_process { 0 } else { MF_GRAYED }
                        | if current { MF_CHECKED } else { 0 },
                    500 + i,
                    wide(priority.label()).as_ptr(),
                );
            }
            AppendMenuW(
                menu,
                MF_POPUP | if has_process { 0 } else { MF_GRAYED },
                priorities as usize,
                wide(tr("우선순위", "Priority")).as_ptr(),
            );
        }
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
        add(510, tr("이름 복사", "Copy name"), has_process);
        // The PID itself as the item's hint badge, like the reference.
        let copy_pid = match selected_process(p).filter(|_| has_process) {
            Some(s) => format!("{}\t{}", tr("PID 복사", "Copy PID"), s.pid),
            None => tr("PID 복사", "Copy PID").to_owned(),
        };
        add(511, &copy_pid, has_process);
        add(
            512,
            tr("실시간 그래프 표시/숨김", "Show / hide live telemetry"),
            true,
        );
        add(ELEVATE, tr("관리자로 실행", "Run as administrator"), true);
        add(
            EXPAND_ALL,
            tr("모두 펼치기", "Expand all"),
            (*p).tree_mode || (*p).group_mode,
        );
        add(RUN_TASK, tr("새 작업 실행…", "Run new task…"), true);
        add(
            RESOURCE_MONITOR,
            tr("리소스 모니터", "Resource Monitor"),
            true,
        );
    } else if (*p).page == Page::Services {
        add(
            PRIMARY,
            tr("시작", "Start"),
            IsWindowEnabled((*p).primary) != 0,
        );
        add(
            SECONDARY,
            tr("중지", "Stop"),
            IsWindowEnabled((*p).secondary) != 0,
        );
        add(
            EXTRA,
            tr("다시 시작", "Restart"),
            service.is_some_and(|s| s.state == SERVICE_RUNNING),
        );
        // A toggle (checked while the panel shows); the panel needs a
        // window at least `DETAILS_MIN_CLIENT` wide, so narrower windows
        // grey the item out instead of offering a command that does nothing.
        let mut client: RECT = zeroed();
        GetClientRect((*p).hwnd, &mut client);
        let wide_enough = client.right >= gfx::pxi((*p).dpi, layout::DETAILS_MIN_CLIENT);
        let shown = wide_enough && (*p).show_details;
        AppendMenuW(
            menu,
            MF_STRING
                | if wide_enough && (service.is_some() || shown) {
                    0
                } else {
                    MF_GRAYED
                }
                | if shown { MF_CHECKED } else { 0 },
            513,
            wide(tr("서비스 상세 정보", "Service details")).as_ptr(),
        );
        add(514, tr("서비스 관리 열기", "Open Services"), true);
    } else if (*p).page == Page::Startup {
        // Named after what it will do to the selected entry; Space runs it
        // from the table.
        let enabled = selected_row(p)
            .and_then(|r| (&(*p).startup).get(r))
            .is_none_or(|s| s.enabled);
        add(
            PRIMARY,
            if enabled {
                tr("시작 시 사용 안 함\tSpace", "Disable at startup\tSpace")
            } else {
                tr("시작 시 사용\tSpace", "Enable at startup\tSpace")
            },
            IsWindowEnabled((*p).primary) != 0,
        );
        add(
            515,
            tr("실행 명령 복사", "Copy startup command"),
            selected_row(p).is_some(),
        );
    }
    // Refresh left the rail with Pause; F5 and the palette reach it too.
    let loading = ((*p).page == Page::Startup && (*p).startup_loading)
        || ((*p).page == Page::Services && (*p).services_loading);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    add(REFRESH, tr("새로 고침\tF5", "Refresh\tF5"), !loading);
    menu
}
/// Run the extra menu's chosen command (`process` = the selection captured
/// when the menu opened).
unsafe fn dispatch_extra(p: *mut App, id: usize, process: Option<Process>) {
    match id {
        500..=503 => {
            if let Some(s) = process {
                begin_action(
                    p,
                    Action::Priority(
                        s.pid,
                        s.created,
                        [
                            crate::actions::Priority::Idle,
                            crate::actions::Priority::BelowNormal,
                            crate::actions::Priority::Normal,
                            crate::actions::Priority::AboveNormal,
                        ][id - 500],
                    ),
                );
            }
        }
        510 => {
            if let Some(s) = process {
                report_copy(p, &s.name);
            }
        }
        511 => {
            if let Some(s) = process {
                report_copy(p, &s.pid.to_string());
            }
        }
        512 => {
            (*p).show_telemetry = !(*p).show_telemetry;
            layout(p);
        }
        513 => {
            (*p).show_details = !(*p).show_details;
            selection_changed(p);
            layout(p);
        }
        514 => begin_action(p, Action::SystemTool(crate::actions::SystemTool::Services)),
        515 => {
            if let Some(s) = selected_row(p).and_then(|r| (&(*p).startup).get(r)) {
                report_copy(p, &s.command.clone());
            }
        }
        ELEVATE => begin_action(p, Action::Elevate),
        0 => {}
        _ => super::command(p, id, 0),
    }
    update_buttons(p);
    redraw(p);
}
unsafe fn run_task(p: *mut App) {
    let mut path = [0u16; 32768];
    // The system's Open dialog, titled and filtered in the app's language.
    let filter: Vec<u16> = format!(
        "{} (*.exe;*.com)\0*.exe;*.com\0\0",
        tr("프로그램", "Programs")
    )
    .encode_utf16()
    .collect();
    let title = wide(tr("새 작업 실행", "Run new task"));
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: (*p).hwnd,
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        lpstrFilter: filter.as_ptr(),
        lpstrTitle: title.as_ptr(),
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER,
        ..zeroed()
    };
    (*p).modal = true;
    configure(p);
    let accepted = GetOpenFileNameW(&mut dialog) != 0;
    finish_modal(p);
    if accepted {
        let end = path.iter().position(|x| *x == 0).unwrap_or(path.len());
        begin_action(p, Action::RunTask(String::from_utf16_lossy(&path[..end])));
    }
}
pub(super) unsafe fn report_copy(p: *mut App, value: &str) {
    match clipboard((*p).hwnd, value) {
        Ok(()) => controls::notify_success(
            p,
            tr("클립보드에 복사했습니다.", "Copied to clipboard.").into(),
        ),
        Err(e) => (*p).set_error(ErrorSource::Action, e),
    }
    redraw(p);
}
unsafe fn clipboard(hwnd: HWND, value: &str) -> Result<(), String> {
    let data = wide(value);
    let handle = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
    if handle.is_null() {
        return Err("Clipboard allocation failed".into());
    }
    let pointer = GlobalLock(handle);
    if pointer.is_null() {
        GlobalFree(handle);
        return Err("Clipboard allocation failed".into());
    }
    std::ptr::copy_nonoverlapping(data.as_ptr(), pointer.cast(), data.len());
    GlobalUnlock(handle);
    if OpenClipboard(hwnd) == 0 {
        GlobalFree(handle);
        return Err(tr(
            "클립보드를 사용할 수 없습니다.",
            "Clipboard is unavailable.",
        )
        .into());
    }
    let ok = EmptyClipboard() != 0 && !SetClipboardData(13, handle).is_null();
    CloseClipboard();
    if !ok {
        GlobalFree(handle);
        return Err("Clipboard write failed".into());
    }
    Ok(())
}
unsafe fn performance_text(p: *mut App) -> String {
    let Some(perf) = &(*p).performance else {
        return tr("아직 측정값이 없습니다.", "No measurements yet.").into();
    };
    match &(*p).perf_target {
        PerfTarget::Cpu=>format!("{}\r\nCPU: {:.1}%\r\nLogical processors: {}\r\nCores: {:?}\r\nReported MHz: {:?}\r\nUptime: {} s",perf.cpu_name,(*p).snapshot.as_ref().map_or(0.,|s|s.cpu_percent),perf.logical_cpus,perf.physical_cores,perf.cpu_frequency_mhz,perf.uptime_seconds),
        PerfTarget::Memory=>format!("Memory (bytes)\r\n{:#?}",perf.memory),
        PerfTarget::Disk(id)=>format!("{:#?}",perf.disks.iter().find(|s|&s.id==id)),
        PerfTarget::Network(id)=>format!("{:#?}",perf.networks.iter().find(|s|&s.id==id)),
        PerfTarget::Gpu(id)=>format!("{:#?}",perf.gpus.iter().find(|s|&s.id==id)),
    }
}

pub(super) unsafe fn finish_modal(p: *mut App) {
    (*p).modal = false;
    configure(p);
    PostMessageW((*p).hwnd, SNAPSHOT_READY, 0, 0);
    PostMessageW((*p).hwnd, JOB_READY, 0, 0);
    update_buttons(p);
}

/// `path` lies inside the Windows directory `windows` (no trailing
/// backslash), case-insensitively.
pub(super) fn in_windows_dir(path: &str, windows: &str) -> bool {
    let (path, windows) = (path.to_lowercase(), windows.to_lowercase());
    path.strip_prefix(windows.trim_end_matches('\\'))
        .is_some_and(|rest| rest.starts_with('\\'))
}
/// The Windows directory as an NT path (`\Device\HarddiskVolume3\Windows`),
/// read once; empty when it cannot be resolved.
fn nt_windows_dir() -> &'static str {
    static DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DIR.get_or_init(|| unsafe {
        use windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW;
        use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
        let mut buffer = [0u16; 260];
        let length = GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) as usize;
        let dir = String::from_utf16_lossy(&buffer[..length.min(buffer.len())]);
        let Some((drive, rest)) = dir.split_once('\\') else {
            return String::new();
        };
        let mut device = [0u16; 512];
        let drive = wide(drive);
        let n = QueryDosDeviceW(drive.as_ptr(), device.as_mut_ptr(), device.len() as u32) as usize;
        let end = device[..n.min(device.len())]
            .iter()
            .position(|&u| u == 0)
            .unwrap_or(n.min(device.len()));
        if end == 0 {
            return String::new();
        }
        format!("{}\\{rest}", String::from_utf16_lossy(&device[..end]))
    })
}
/// The image path of `pid` as an NT path, read with
/// `NtQuerySystemInformation(SystemProcessIdInformation)`: unlike opening
/// the process it needs no access right, so service hosts, csrss and lsass
/// answer without elevation. None for processes without an image.
unsafe fn nt_image_path(pid: u32) -> Option<String> {
    type Query = unsafe extern "system" fn(u32, *mut std::ffi::c_void, u32, *mut u32) -> i32;
    static QUERY: std::sync::OnceLock<Option<Query>> = std::sync::OnceLock::new();
    let query = (*QUERY.get_or_init(|| unsafe {
        let ntdll = wide("ntdll.dll");
        let module = GetModuleHandleW(ntdll.as_ptr());
        if module.is_null() {
            return None;
        }
        GetProcAddress(module, c"NtQuerySystemInformation".as_ptr().cast())
            .map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, Query>(f))
    }))?;
    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum: u16,
        buffer: *mut u16,
    }
    #[repr(C)]
    struct ProcessIdInformation {
        pid: usize,
        name: UnicodeString,
    }
    const SYSTEM_PROCESS_ID_INFORMATION: u32 = 0x58;
    let mut buffer = vec![0u16; 1024];
    let mut info = ProcessIdInformation {
        pid: pid as usize,
        name: UnicodeString {
            length: 0,
            maximum: (buffer.len() * 2) as u16,
            buffer: buffer.as_mut_ptr(),
        },
    };
    let status = query(
        SYSTEM_PROCESS_ID_INFORMATION,
        (&mut info as *mut ProcessIdInformation).cast(),
        size_of::<ProcessIdInformation>() as u32,
        null_mut(),
    );
    let units = (info.name.length as usize / 2).min(buffer.len());
    (status >= 0 && units > 0).then(|| String::from_utf16_lossy(&buffer[..units]))
}
/// Windows' own processes, the grouped view's "Windows processes": PID 0 / 4,
/// the minimal processes System starts (Registry, Memory Compression, smss:
/// children of PID 4) and every process whose executable lies inside the
/// Windows directory — the end-task warning's test, on the image path the
/// kernel reports for any PID ([`nt_image_path`]: service hosts, csrss and
/// lsass included, without elevation). A process whose path cannot be read
/// follows the process that started it. Nothing is guessed from executable
/// names. Paths are read once per process (`App.windows_images`:
/// Some(inside) or None = unreadable).
pub(super) unsafe fn windows_processes(p: *mut App, processes: &[Process]) -> Vec<bool> {
    let tree = Tree::new(processes);
    let mut windows = vec![false; processes.len()];
    let mut stack: Vec<usize> = (0..processes.len())
        .filter(|&i| tree.parent(i).is_none())
        .collect();
    stack.reverse();
    while let Some(i) = stack.pop() {
        let process = &processes[i];
        windows[i] = if process.pid <= 4 || process.parent_pid == 4 {
            true
        } else {
            let identity = ProcessIdentity::from(process);
            let inside = *(*p).windows_images.entry(identity).or_insert_with(|| {
                let windows = nt_windows_dir();
                nt_image_path(process.pid)
                    .filter(|_| !windows.is_empty())
                    .map(|path| in_windows_dir(&path, windows))
            });
            inside.unwrap_or_else(|| tree.parent(i).is_some_and(|parent| windows[parent]))
        };
        stack.extend(tree.children_of(i).iter().rev());
    }
    windows
}
/// The process that owns the shell's desktop window (explorer.exe).
unsafe fn shell_pid() -> Option<u32> {
    let shell = GetShellWindow();
    if shell.is_null() {
        return None;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(shell, &mut pid);
    (pid != 0).then_some(pid)
}
/// The apps of the grouped view ("App groups"), from the process hierarchy
/// and the processes that own a visible top-level window (`windows`, PIDs).
/// Every such process is an app; a process without a window belongs to the
/// app of its nearest ancestor that has one (Chrome's renderers, a
/// terminal's shells), except under the shell (`shell`: the desktop
/// window's process), whose app keeps only its own image's instances — it
/// starts every sign-in app. A windowed descendant of the same image joins
/// its ancestor's app; one of another image starts its own.
/// `app[i]` = the app root process `i` belongs to (a root maps to itself).
pub(super) fn app_groups(
    processes: &[Process],
    windows: &HashSet<u32>,
    shell: Option<u32>,
) -> Vec<Option<usize>> {
    let tree = Tree::new(processes);
    let name = |i: usize| processes[i].name.to_lowercase();
    let windowed = |i: usize| processes[i].pid > 4 && windows.contains(&processes[i].pid);
    let is_shell = |i: usize| shell == Some(processes[i].pid);
    let mut app: Vec<Option<usize>> = vec![None; processes.len()];
    // Parents before children (iterative: hierarchies can be deep).
    let mut stack: Vec<usize> = (0..processes.len())
        .filter(|&i| tree.parent(i).is_none())
        .collect();
    stack.reverse();
    while let Some(i) = stack.pop() {
        let inherited = tree.parent(i).and_then(|parent| app[parent]);
        app[i] = match inherited {
            Some(root) if windowed(i) => (name(root) == name(i)).then_some(root).or(Some(i)),
            Some(root) if !is_shell(root) || name(i) == name(root) => Some(root),
            _ if windowed(i) => Some(i),
            _ => None,
        };
        stack.extend(tree.children_of(i).iter().rev());
    }
    app
}
/// Grouped view rows (`(*p).rows` holds every process index in the sort
/// order; `matches` = the search's matches, None without a search):
/// "Apps (n)" with one row per app — collapsed unless opened, its label
/// "name (processes)" and its CPU / memory / I/O summed over the app like
/// the reference's parent rows — then "Background processes" and "Windows
/// processes". `(*p).tree_rows` mirrors the rows (depth, chevron state).
pub(super) unsafe fn group_rows(p: *mut App, matches: Option<&HashSet<usize>>) {
    if (*p)
        .last_window_scan
        .is_none_or(|at| at.elapsed() >= Duration::from_secs(5))
    {
        (*p).window_pids.clear();
        EnumWindows(Some(visible_window), p as isize);
        (*p).last_window_scan = Some(Instant::now());
    }
    (*p).group_totals.clear();
    (*p).tree_rows.clear();
    let Some(snapshot) = &(*p).snapshot else {
        (*p).rows.clear();
        return;
    };
    let processes = &snapshot.processes;
    let app = app_groups(processes, &(*p).window_pids, shell_pid());
    let windows = windows_processes(p, processes);
    let order = std::mem::take(&mut (*p).rows);
    let matched = |i: usize| matches.is_none_or(|m| m.contains(&i));
    // Members of each app in the sort order (the root first).
    let mut members: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    let mut roots = Vec::new();
    let mut background = Vec::new();
    let mut system = Vec::new();
    for &index in &order {
        match app.get(index).copied().flatten() {
            Some(root) if root == index => roots.push(index),
            Some(root) => members.entry(root).or_default().push(index),
            None if windows[index] => {
                if matched(index) {
                    system.push(index)
                }
            }
            None => {
                if matched(index) {
                    background.push(index)
                }
            }
        }
    }
    let empty = Vec::new();
    // An app is listed when it or one of its processes matches the search.
    roots.retain(|&root| {
        matched(root)
            || members
                .get(&root)
                .is_some_and(|m| m.iter().any(|&i| matched(i)))
    });
    for &root in &roots {
        let group = members.get(&root).unwrap_or(&empty);
        if group.is_empty() {
            continue;
        }
        let total = GroupTotal::of(std::iter::once(&root).chain(group).map(|&i| &processes[i]));
        (*p).group_totals.insert(root, total);
    }
    // Apps sort by their totals, like the reference's `val(parent)`.
    let (col, desc) = ((*p).sort, (*p).descending);
    let totals = &(*p).group_totals;
    let shown = |i: usize| -> Process {
        let mut process = processes[i].clone();
        if let Some(total) = totals.get(&i) {
            total.apply(&mut process);
        }
        process
    };
    let rank: std::collections::HashMap<usize, usize> =
        order.iter().enumerate().map(|(r, &i)| (i, r)).collect();
    roots.sort_by(|&a, &b| {
        let result = super::compare(&shown(a), &shown(b), col);
        (if desc { result.reverse() } else { result }).then(rank[&a].cmp(&rank[&b]))
    });
    let searching = matches.is_some();
    let mut rows = Vec::with_capacity(order.len() + 3);
    let mut tree_rows = Vec::with_capacity(order.len() + 3);
    let header = |rows: &mut Vec<usize>, tree_rows: &mut Vec<TreeRow>, title: String| {
        (*p).group_headers.insert(rows.len(), title);
        rows.push(usize::MAX);
        tree_rows.push(TreeRow {
            index: usize::MAX,
            depth: 0,
            has_children: false,
            expanded: false,
            children: 0,
        });
    };
    let leaf = |index: usize, depth: usize| TreeRow {
        index,
        depth,
        has_children: false,
        expanded: false,
        children: 0,
    };
    if !roots.is_empty() {
        header(
            &mut rows,
            &mut tree_rows,
            format!("{} ({})", tr("앱", "Apps"), roots.len()),
        );
        for &root in &roots {
            let group = members.get(&root).unwrap_or(&empty);
            let open = !group.is_empty()
                && (searching
                    || (*p)
                        .expanded_groups
                        .contains(&ProcessIdentity::from(&processes[root])));
            rows.push(root);
            tree_rows.push(TreeRow {
                index: root,
                depth: 0,
                has_children: !group.is_empty(),
                expanded: open,
                children: group.len(),
            });
            if open {
                // The whole app when it matched itself, else its matches.
                let all = matched(root);
                for &child in group.iter().filter(|&&i| all || matched(i)) {
                    rows.push(child);
                    tree_rows.push(leaf(child, 1));
                }
            }
        }
    }
    for (title, items) in [
        (
            tr("백그라운드 프로세스", "Background processes"),
            background,
        ),
        (tr("Windows 프로세스", "Windows processes"), system),
    ] {
        if items.is_empty() {
            continue;
        }
        header(
            &mut rows,
            &mut tree_rows,
            format!("{title} ({})", items.len()),
        );
        for index in items {
            rows.push(index);
            tree_rows.push(leaf(index, 0));
        }
    }
    (*p).rows = rows;
    (*p).tree_rows = tree_rows;
    // Groups and paths of processes that exited are forgotten.
    let alive: HashSet<ProcessIdentity> = processes.iter().map(ProcessIdentity::from).collect();
    (*p).expanded_groups
        .retain(|identity| alive.contains(identity));
    (*p).windows_images
        .retain(|identity, _| alive.contains(identity));
}
/// The app root the grouped view lists `identity` under (itself for an
/// app), or None (background / Windows processes, unknown processes).
pub(super) unsafe fn app_of(p: *mut App, identity: ProcessIdentity) -> Option<ProcessIdentity> {
    let snapshot = (*p).snapshot.as_ref()?;
    let processes = &snapshot.processes;
    let index = processes
        .iter()
        .position(|s| ProcessIdentity::from(s) == identity)?;
    let root = app_groups(processes, &(*p).window_pids, shell_pid())[index]?;
    Some(ProcessIdentity::from(&processes[root]))
}
unsafe extern "system" fn visible_window(hwnd: HWND, data: LPARAM) -> i32 {
    if IsWindowVisible(hwnd) == 0
        || !GetWindow(hwnd, GW_OWNER).is_null()
        || GetWindowTextLengthW(hwnd) == 0
    {
        return 1;
    }
    let mut cloaked: u32 = 0;
    windows_sys::Win32::Graphics::Dwm::DwmGetWindowAttribute(
        hwnd,
        14,
        (&mut cloaked as *mut u32).cast(),
        4,
    );
    if cloaked != 0 {
        return 1;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    (*(data as *mut App)).window_pids.insert(pid);
    1
}
