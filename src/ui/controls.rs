//! Custom controls (DESIGN_SPEC §4, §6): the `.select` child control that
//! replaces every ComboBox, `:focus-visible` focus rings (keyboard only, 2 px
//! outside the element), the confirm dialog and toast glue, and the Controls
//! track's previews. Popups themselves live in `popup.rs`.

use super::popup::{self, Anchor, ConfirmSpec, MenuItem, MenuStyle};
use super::widgets::Painter;
use super::*;
use std::cell::Cell;

const SELECT_CLASS: &str = "FeatherTaskManager.Select";
/// A native list box's type-ahead window.
const TYPE_AHEAD: Duration = Duration::from_millis(1000);

// ───────────────────────────── focus cues ─────────────────────────────

/// Keyboard focus cues (UISF_HIDEFOCUS) are hidden for `hwnd`'s window tree:
/// the last navigation was the mouse.
pub(super) unsafe fn cues_hidden(hwnd: HWND) -> bool {
    SendMessageW(hwnd, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS != 0
}

/// A pointer press hides the focus cues (`:focus-visible` never matches a
/// clicked control); Tab / arrow navigation (IsDialogMessage) shows them.
pub(super) unsafe fn pointer_pressed(hwnd: HWND) {
    SendMessageW(
        hwnd,
        WM_CHANGEUISTATE,
        (UIS_SET | (UISF_HIDEFOCUS << 16)) as usize,
        0,
    );
}

/// Keyboard use the dialog manager does not see (Apps / Shift+F10) shows the
/// focus cues, like Tab does.
pub(super) unsafe fn keyboard_used(hwnd: HWND) {
    SendMessageW(
        hwnd,
        WM_CHANGEUISTATE,
        (UIS_CLEAR | (UISF_HIDEFOCUS << 16)) as usize,
        0,
    );
}

/// The main message loop's view of every message before dispatch: a mouse
/// press anywhere in the app hides the keyboard focus cues, so no custom
/// control (table, select, button) needs a hook of its own for
/// `:focus-visible` — and menus opened after a click start without a
/// highlighted item.
pub(super) unsafe fn observe_input(msg: &MSG) {
    if !msg.hwnd.is_null()
        && matches!(
            msg.message,
            WM_LBUTTONDOWN
                | WM_LBUTTONDBLCLK
                | WM_RBUTTONDOWN
                | WM_RBUTTONDBLCLK
                | WM_MBUTTONDOWN
                | WM_XBUTTONDOWN
        )
        && !cues_hidden(msg.hwnd)
    {
        pointer_pressed(msg.hwnd);
    }
}

/// Start with the cues the last input implies (mouse: hidden), like dialogs.
pub(super) unsafe fn init_focus_cues(p: *mut App) {
    SendMessageW(
        (*p).hwnd,
        WM_CHANGEUISTATE,
        (UIS_INITIALIZE | ((UISF_HIDEFOCUS | UISF_HIDEACCEL) << 16)) as usize,
        0,
    );
}

/// Controls whose focus ring is drawn 2 px *outside* them, like the
/// reference's `outline-offset: 2px`: page-head and drawer buttons, the ⋯
/// button, every select, the nav items and the theme segments. The main
/// window paints the ring where it has room, and a neighbouring control the
/// ring overlaps (the next nav item 2 px away, the adjacent segment) paints
/// the part over itself ([`paint_neighbour_ring`]). Switches draw their own
/// (their window holds a 4 px margin).
pub(super) unsafe fn ring_outside(p: *mut App, hwnd: HWND) -> bool {
    if hwnd.is_null() || GetParent(hwnd) != (*p).hwnd {
        return false;
    }
    let id = GetDlgCtrlID(hwnd) as usize;
    translates(id)
        || (NAV..NAV + 4).contains(&id)
        || matches!(
            id,
            SETTINGS
                | VIEW_MODE
                | FILTER
                | RATE
                | PREF_LANGUAGE
                | PREF_RATE
                | PREF_START
                | THEME_LIGHT
                | THEME_DARK
                | THEME_SYSTEM
        )
}

/// `.btn`s whose face translates 1 px down while pressed (`.btn:active`);
/// nav items and segments have no pressed style in the reference.
fn translates(id: usize) -> bool {
    matches!(
        id,
        PRIMARY
            | SECONDARY
            | EXTRA
            | NUCLEAR
            | MORE
            | END_TREE
            | COPY
            | CORES
            | RESOURCE_MONITOR
            | EXPAND_ALL
            | REFRESH
            | PAUSE
            | TOP
            | RUN_TASK
    )
}

/// Whether a control draws its own (inset) keyboard focus ring now.
pub(super) unsafe fn inner_focus(p: *mut App, hwnd: HWND) -> bool {
    GetFocus() == hwnd && !cues_hidden(hwnd) && !ring_outside(p, hwnd)
}

thread_local! {
    /// The ring painted last (client px), so moving focus erases it.
    static RING: Cell<Option<RECT>> = const { Cell::new(None) };
    /// Previews: pretend this control has keyboard focus with cues shown.
    static FOCUS_OVERRIDE: Cell<Option<HWND>> = const { Cell::new(None) };
}

unsafe fn child_rect(p: *mut App, h: HWND) -> RECT {
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

/// The area a control's outside ring can cover: 2 px gap + 2 px ring,
/// 1 px press translation, 1 px antialiasing.
fn ring_rect(dpi: i32, r: RECT) -> RECT {
    let d = gfx::pxi(dpi, 5.0) + 1;
    RECT {
        left: r.left - d,
        top: r.top - d,
        right: r.right + d,
        bottom: r.bottom + d,
    }
}

/// Focus, UI state or position of `hwnd` changed: repaint the old ring and
/// the area around `hwnd` where its ring belongs (the main window and the
/// neighbouring controls the ring overlaps).
pub(super) unsafe fn focus_changed(p: *mut App, hwnd: HWND) {
    if (*p).hwnd.is_null() {
        return;
    }
    let area = |r: &RECT| unsafe {
        RedrawWindow((*p).hwnd, r, null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN);
    };
    if let Some(old) = RING.with(Cell::take) {
        area(&old);
    }
    if ring_outside(p, hwnd) {
        area(&ring_rect((*p).dpi, child_rect(p, hwnd)));
    }
}

/// The element (client px) whose outside focus ring shows now, if any.
unsafe fn visible_ring(p: *mut App) -> Option<(HWND, RECT)> {
    let (focus, forced) = match FOCUS_OVERRIDE.with(Cell::get) {
        Some(h) => (h, true),
        None => (GetFocus(), false),
    };
    let visible = !focus.is_null()
        && ring_outside(p, focus)
        && GetWindowLongW(focus, GWL_STYLE) as u32 & WS_VISIBLE != 0
        && IsWindowEnabled(focus) != 0
        && (forced || !cues_hidden(focus));
    if !visible {
        return None;
    }
    let mut r = child_rect(p, focus);
    // The outline moves with `.btn:active`'s translateY(1px).
    let down = press_offset(p, focus);
    r.top += down;
    r.bottom += down;
    Some((focus, r))
}

/// The main window's part of `:focus-visible`: 2 px fg ring, 2 px outside
/// the focused control (radius 4 + 2), keyboard focus only. Called last by
/// `paint::paint_to`.
pub(super) unsafe fn paint_focus_ring(p: *mut App, dc: HDC) {
    let Some((_, r)) = visible_ring(p) else {
        RING.with(|ring| ring.set(None));
        return;
    };
    let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
    widgets::focus_ring(&pt, r, pt.px(theme::RADIUS_SM));
    RING.with(|ring| ring.set(Some(ring_rect((*p).dpi, r))));
}

/// A control's part of a *neighbour's* outside focus ring: drawn over its
/// own face where the ring crosses it (`dc` = the control's, `bounds` its
/// client rect), so the ring is whole even between controls 2 px apart.
pub(super) unsafe fn paint_neighbour_ring(p: *mut App, dc: HDC, control: HWND, bounds: RECT) {
    let Some((focus, r)) = visible_ring(p) else {
        return;
    };
    if focus == control {
        return;
    }
    let origin = child_rect(p, control);
    let local = RECT {
        left: r.left - origin.left,
        top: r.top - origin.top,
        right: r.right - origin.left,
        bottom: r.bottom - origin.top,
    };
    let reach = ring_rect((*p).dpi, local);
    let overlaps = reach.left < bounds.right
        && reach.right > bounds.left
        && reach.top < bounds.bottom
        && reach.bottom > bounds.top;
    if overlaps {
        let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
        widgets::focus_ring(&pt, local, pt.px(theme::RADIUS_SM));
    }
}

// ───────────────────────────── button press ─────────────────────────────

thread_local! {
    /// Page-head buttons last drawn pressed.
    static PRESSED: std::cell::RefCell<Vec<HWND>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// How far a pressed `.btn` face is translated (0 when not pressed):
/// `.btn:active:not(:disabled) { transform: translateY(1px) }`.
unsafe fn press_offset(p: *mut App, h: HWND) -> i32 {
    if ring_outside(p, h)
        && translates(GetDlgCtrlID(h) as usize)
        && IsWindowEnabled(h) != 0
        && SendMessageW(h, BM_GETSTATE, 0, 0) as u32 & BST_PUSHED != 0
    {
        gfx::pxi((*p).dpi, 1.0).max(1)
    } else {
        0
    }
}

/// A page-head button was drawn (WM_DRAWITEM). Its pressed face is
/// translated 1 px down, so the face's bottom edge lands on the main window
/// below the button: repaint that row (and a keyboard ring that moves with
/// it) whenever the pressed state changes.
pub(super) unsafe fn button_drawn(p: *mut App, h: HWND, pressed: bool) {
    if (*p).hwnd.is_null() || !ring_outside(p, h) || !translates(GetDlgCtrlID(h) as usize) {
        return;
    }
    let changed = PRESSED.with(|list| {
        let mut list = list.borrow_mut();
        list.retain(|&other| IsWindow(other) != 0);
        match (list.contains(&h), pressed) {
            (false, true) => list.push(h),
            (true, false) => list.retain(|&other| other != h),
            _ => return false,
        }
        true
    });
    if changed {
        let r = ring_rect((*p).dpi, child_rect(p, h));
        InvalidateRect((*p).hwnd, &r, 0);
    }
}

/// The main window's part of a pressed page-head button: the bottom rows
/// of its translated face (the button itself shows the rest), painted by
/// `paint::paint_to` from the button's real state, before the focus ring.
pub(super) unsafe fn paint_pressed_rows(p: *mut App, dc: HDC) {
    let mut child = GetWindow((*p).hwnd, GW_CHILD);
    while !child.is_null() {
        let h = child;
        child = GetWindow(child, GW_HWNDNEXT);
        let down = press_offset(p, h);
        if down == 0 || GetWindowLongW(h, GWL_STYLE) as u32 & WS_VISIBLE == 0 {
            continue;
        }
        let r = child_rect(p, h);
        let id = GetDlgCtrlID(h) as usize;
        let saved = SaveDC(dc);
        IntersectClipRect(dc, r.left, r.bottom, r.right, r.bottom + down);
        {
            let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
            widgets::button_face(
                &pt,
                r,
                paint::button_style(p, id),
                &widgets::ButtonState {
                    hover: (*p).anim.value((id, anim::part::HOVER)),
                    pressed: true,
                    selected: id == TOP && (*p).topmost || id == CORES && (*p).core_graphs,
                    ..widgets::ButtonState::default()
                },
                "",
                paint::parent_background(p, h),
            );
        }
        RestoreDC(dc, saved);
    }
}

/// Previews: draw `hwnd`'s keyboard focus ring (None = real focus).
pub(super) unsafe fn preview_focus(p: *mut App, hwnd: Option<HWND>) {
    FOCUS_OVERRIDE.with(|focus| focus.set(hwnd));
    if let Some(h) = hwnd {
        InvalidateRect(h, null(), 0);
    }
    redraw(p);
}

// ───────────────────────────── select ─────────────────────────────

struct Select {
    app: *mut App,
    hwnd: HWND,
    /// The accessible label ("Refresh interval"); the window text is
    /// "label: value" (see [`sync_text`]).
    label: String,
    items: Vec<String>,
    data: Vec<isize>,
    current: isize,
    font: HFONT,
    item_height: i32,
    dropdown: HWND,
    /// Opened by a press on the face that is still held (release on an
    /// item chooses it, like a native drop-down list).
    drag: bool,
    /// A press inside the dropdown.
    pressed: bool,
    typed: String,
    typed_at: Option<Instant>,
}

unsafe fn register_select() -> bool {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REGISTERED.get_or_init(|| unsafe {
        let class = wide(SELECT_CLASS);
        let definition = WNDCLASSW {
            lpfnWndProc: Some(select_proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&definition) != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS
    })
}

/// A `.select` child control of the main window. It answers the ComboBox
/// messages the app uses (CB_ADDSTRING, CB_RESETCONTENT, CB_SETCURSEL,
/// CB_GETCURSEL, CB_GETCOUNT, CB_GETLBTEXT(LEN), CB_FINDSTRING(EXACT),
/// CB_(GET|SET)ITEMDATA, CB_(GET|SET)ITEMHEIGHT, CB_GETDROPPEDSTATE,
/// CB_SHOWDROPDOWN, WM_(GET|SET)FONT) and notifies the parent with
/// WM_COMMAND / CBN_SELCHANGE on user changes only (never for CB_SETCURSEL),
/// so every existing handler keeps working. The window text is its
/// accessible label.
pub(super) unsafe fn select(p: *mut App, label: &str, id: usize) -> HWND {
    create_select((*p).hwnd, p, label, id)
}

/// A select under any parent (the dropdown still belongs to `p`'s window).
unsafe fn create_select(parent: HWND, p: *mut App, label: &str, id: usize) -> HWND {
    if !register_select() {
        return null_mut();
    }
    CreateWindowExW(
        0,
        wide(SELECT_CLASS).as_ptr(),
        wide(label).as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        0,
        0,
        0,
        0,
        parent,
        id as HMENU,
        GetModuleHandleW(null()),
        p.cast(),
    )
}

unsafe fn select_state(hwnd: HWND) -> *mut Select {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Select
}

unsafe fn wide_arg(l: LPARAM) -> String {
    if l == 0 {
        return String::new();
    }
    let text = l as *const u16;
    let mut n = 0;
    while *text.add(n) != 0 && n < 4096 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(text, n))
}

/// The window text — the name assistive technology reads through the
/// default window proxy — carries the value: "Refresh interval: 1 s" (a
/// native ComboBox exposes its value separately; this control has no
/// IAccessible of its own). DefWindowProc raises the name-change event.
unsafe fn sync_text(s: *mut Select) {
    let select = &*s;
    let value = usize::try_from(select.current)
        .ok()
        .and_then(|i| select.items.get(i));
    let text = match value {
        Some(value) if !select.label.is_empty() => format!("{}: {value}", select.label),
        Some(value) => value.clone(),
        None => select.label.clone(),
    };
    DefWindowProcW(select.hwnd, WM_SETTEXT, 0, wide(&text).as_ptr() as LPARAM);
}

unsafe fn is_dropped(s: *mut Select) -> bool {
    !(*s).dropdown.is_null()
}

unsafe fn notify(s: *mut Select, code: u32) {
    let hwnd = (*s).hwnd;
    let id = GetDlgCtrlID(hwnd) as usize & 0xffff;
    SendMessageW(
        GetParent(hwnd),
        WM_COMMAND,
        id | (code as usize) << 16,
        hwnd as isize,
    );
}

/// A user choice: select `index` and notify (only when it changed).
unsafe fn choose(s: *mut Select, index: isize) {
    let n = (*s).items.len() as isize;
    if n == 0 {
        return;
    }
    let index = index.clamp(0, n - 1);
    if index == (*s).current {
        return;
    }
    (*s).current = index;
    InvalidateRect((*s).hwnd, null(), 0);
    sync_text(s);
    notify(s, CBN_SELCHANGE);
}

unsafe fn status_select(s: *mut Select) -> bool {
    GetDlgCtrlID((*s).hwnd) as usize == RATE
}

unsafe fn open(s: *mut Select) {
    let hwnd = (*s).hwnd;
    if is_dropped(s) || (*s).items.is_empty() || IsWindowEnabled(hwnd) == 0 {
        return;
    }
    let mut r: RECT = zeroed();
    GetWindowRect(hwnd, &mut r);
    let items: Vec<MenuItem> = (*s)
        .items
        .iter()
        .enumerate()
        .map(|(i, label)| MenuItem {
            checked: i as isize == (*s).current,
            ..MenuItem::item(i, label)
        })
        .collect();
    let font = if status_select(s) {
        popup::Font::MonoSmall
    } else {
        popup::Font::Small
    };
    let hot = usize::try_from((*s).current).ok();
    let dropdown = popup::open_list(
        (*s).app,
        items,
        MenuStyle::list(r.right - r.left, font),
        Anchor::Below { r, right: false },
        hot,
    );
    if dropdown.is_null() {
        return;
    }
    (*s).dropdown = dropdown;
    (*s).drag = false;
    (*s).pressed = false;
    // Outside clicks, focus loss and capture loss close it.
    SetCapture(hwnd);
    InvalidateRect(hwnd, null(), 0);
}

/// Close the dropdown; `commit` chooses its highlighted item.
unsafe fn close(s: *mut Select, commit: bool) {
    let dropdown = std::mem::replace(&mut (*s).dropdown, null_mut());
    if dropdown.is_null() {
        return;
    }
    let hot = popup::menu_hot(dropdown);
    popup::close(dropdown);
    (*s).drag = false;
    (*s).pressed = false;
    if GetCapture() == (*s).hwnd {
        ReleaseCapture();
    }
    InvalidateRect((*s).hwnd, null(), 0);
    if commit {
        if let Some(index) = hot {
            choose(s, index as isize);
        }
    }
}

/// Keyboard (DESIGN_SPEC §4 Select): closed, Up/Down/Left/Right/Home/End/
/// PageUp/PageDown change the value at once; Alt+Down/Alt+Up/F4/Enter/Space
/// open. Open, the arrows move the highlight; Enter/Space/F4/Alt+arrows
/// choose it; Esc closes.
unsafe fn key(s: *mut Select, vk: u16, alt: bool) -> bool {
    let n = (*s).items.len() as isize;
    if is_dropped(s) {
        let dropdown = (*s).dropdown;
        let hot = popup::menu_hot(dropdown).map_or((*s).current, |h| h as isize);
        let target = match vk {
            VK_UP if !alt => Some(hot - 1),
            VK_DOWN if !alt => Some(hot + 1),
            VK_HOME | VK_PRIOR => Some(0),
            VK_END | VK_NEXT => Some(n - 1),
            _ => None,
        };
        if let Some(target) = target {
            if n > 0 {
                popup::menu_set_hot(dropdown, Some(target.clamp(0, n - 1) as usize));
            }
            return true;
        }
        match vk {
            VK_RETURN | VK_SPACE | VK_F4 | VK_UP | VK_DOWN => close(s, true),
            VK_ESCAPE => close(s, false),
            _ => return false,
        }
        return true;
    }
    let current = (*s).current;
    match vk {
        VK_UP | VK_DOWN | VK_F4 if alt => open(s),
        VK_F4 | VK_RETURN | VK_SPACE => open(s),
        VK_UP | VK_LEFT => choose(s, (current - 1).max(0)),
        VK_DOWN | VK_RIGHT => choose(s, current + 1),
        VK_HOME | VK_PRIOR => choose(s, 0),
        VK_END | VK_NEXT => choose(s, n - 1),
        _ => return false,
    }
    true
}

/// A native list box's type-ahead: repeating one letter cycles through the
/// items starting with it; typing quickly matches a prefix.
pub(super) fn type_ahead(items: &[String], from: isize, typed: &str) -> Option<usize> {
    let n = items.len();
    let first = typed.chars().next()?;
    let cycling = typed.chars().all(|c| c == first);
    let prefix: String = if cycling {
        first.to_string()
    } else {
        typed.to_owned()
    };
    let start = if cycling || from < 0 {
        (from + 1).max(0) as usize
    } else {
        from as usize
    };
    (0..n)
        .map(|k| (start + k) % n)
        .find(|&i| items[i].to_lowercase().starts_with(prefix.as_str()))
}

unsafe fn typed(s: *mut Select, ch: char) {
    if ch.is_control() || ch == ' ' {
        return;
    }
    let now = Instant::now();
    if (*s)
        .typed_at
        .is_none_or(|at| now.duration_since(at) > TYPE_AHEAD)
    {
        (*s).typed.clear();
    }
    (*s).typed_at = Some(now);
    (*s).typed.extend(ch.to_lowercase());
    let from = if is_dropped(s) {
        popup::menu_hot((*s).dropdown).map_or((*s).current, |h| h as isize)
    } else {
        (*s).current
    };
    let Some(index) = type_ahead(&(*s).items, from, &(*s).typed) else {
        return;
    };
    if is_dropped(s) {
        popup::menu_set_hot((*s).dropdown, Some(index));
    } else {
        choose(s, index as isize);
    }
}

unsafe fn screen_point(hwnd: HWND, l: LPARAM) -> POINT {
    let mut pt = POINT {
        x: (l & 0xffff) as i16 as i32,
        y: ((l >> 16) & 0xffff) as i16 as i32,
    };
    ClientToScreen(hwnd, &mut pt);
    pt
}

unsafe fn on_face(hwnd: HWND, pt: POINT) -> bool {
    let mut r: RECT = zeroed();
    GetWindowRect(hwnd, &mut r);
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

unsafe extern "system" fn select_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = &*(l as *const CREATESTRUCTW);
        let state = Box::into_raw(Box::new(Select {
            app: create.lpCreateParams as *mut App,
            hwnd,
            label: wide_arg(create.lpszName as LPARAM),
            items: Vec::new(),
            data: Vec::new(),
            current: -1,
            font: null_mut(),
            item_height: 20,
            dropdown: null_mut(),
            drag: false,
            pressed: false,
            typed: String::new(),
            typed_at: None,
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
    }
    let s = select_state(hwnd);
    if s.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    let count = (*s).items.len();
    let index = w as isize;
    let valid = (0..count as isize).contains(&index);
    const ERR: LRESULT = CB_ERR as LRESULT;
    match msg {
        // `.select` is a native `<select>`: pointer while enabled.
        WM_SETCURSOR if w as HWND == hwnd && IsWindowEnabled(hwnd) != 0 => {
            widgets::set_pointer(true)
        }
        CB_ADDSTRING => {
            (*s).items.push(wide_arg(l));
            (*s).data.push(0);
            InvalidateRect(hwnd, null(), 0);
            (count) as LRESULT
        }
        CB_INSERTSTRING => {
            let at = if index < 0 || index as usize > count {
                count
            } else {
                index as usize
            };
            (*s).items.insert(at, wide_arg(l));
            (*s).data.insert(at, 0);
            if (*s).current >= at as isize {
                (*s).current += 1;
            }
            at as LRESULT
        }
        CB_DELETESTRING => {
            if !valid {
                return ERR;
            }
            (*s).items.remove(index as usize);
            (*s).data.remove(index as usize);
            if (*s).current == index {
                (*s).current = -1;
            } else if (*s).current > index {
                (*s).current -= 1;
            }
            close(s, false);
            InvalidateRect(hwnd, null(), 0);
            sync_text(s);
            (count - 1) as LRESULT
        }
        CB_RESETCONTENT => {
            close(s, false);
            (*s).items.clear();
            (*s).data.clear();
            (*s).current = -1;
            InvalidateRect(hwnd, null(), 0);
            sync_text(s);
            0
        }
        CB_SETCURSEL => {
            let next = if valid { index } else { -1 };
            if next != (*s).current {
                (*s).current = next;
                InvalidateRect(hwnd, null(), 0);
                sync_text(s);
            }
            if valid {
                index
            } else {
                ERR
            }
        }
        CB_GETCURSEL => (*s).current,
        CB_GETCOUNT => count as LRESULT,
        CB_GETLBTEXTLEN => {
            if valid {
                (&(*s).items)[index as usize].encode_utf16().count() as LRESULT
            } else {
                ERR
            }
        }
        CB_GETLBTEXT => {
            if !valid || l == 0 {
                return ERR;
            }
            let text: Vec<u16> = (&(*s).items)[index as usize].encode_utf16().collect();
            let out = l as *mut u16;
            std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len());
            *out.add(text.len()) = 0;
            text.len() as LRESULT
        }
        CB_FINDSTRING | CB_FINDSTRINGEXACT | CB_SELECTSTRING => {
            let wanted = wide_arg(l).to_lowercase();
            let start = if valid { index as usize + 1 } else { 0 };
            let found = (0..count).map(|k| (start + k) % count.max(1)).find(|&i| {
                let item = (&(*s).items)[i].to_lowercase();
                if msg == CB_FINDSTRINGEXACT {
                    item == wanted
                } else {
                    item.starts_with(wanted.as_str())
                }
            });
            match found {
                Some(i) => {
                    if msg == CB_SELECTSTRING {
                        (*s).current = i as isize;
                        InvalidateRect(hwnd, null(), 0);
                        sync_text(s);
                    }
                    i as LRESULT
                }
                None => ERR,
            }
        }
        CB_SETITEMDATA => {
            if !valid {
                return ERR;
            }
            (&mut (*s).data)[index as usize] = l;
            0
        }
        CB_GETITEMDATA => {
            if valid {
                (&(*s).data)[index as usize]
            } else {
                ERR
            }
        }
        // Row heights belong to the painted face and dropdown; accept the
        // owner-draw era's calls.
        CB_SETITEMHEIGHT => {
            (*s).item_height = l as i32;
            0
        }
        CB_GETITEMHEIGHT => (*s).item_height as LRESULT,
        CB_GETDROPPEDSTATE => is_dropped(s) as LRESULT,
        CB_SHOWDROPDOWN => {
            if w != 0 {
                open(s);
            } else {
                close(s, false);
            }
            1
        }
        WM_SETFONT => {
            (*s).font = w as HFONT;
            if l != 0 {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_GETFONT => (*s).font as LRESULT,
        // SetWindowText names the control (language changes): keep the value.
        WM_SETTEXT => {
            (*s).label = wide_arg(l);
            sync_text(s);
            1
        }
        WM_GETDLGCODE => {
            let mut code = DLGC_WANTARROWS | DLGC_WANTCHARS;
            let message = l as *const MSG;
            if !message.is_null() && (*message).message == WM_KEYDOWN {
                match (*message).wParam as u16 {
                    VK_RETURN => code |= DLGC_WANTMESSAGE,
                    VK_ESCAPE if is_dropped(s) => code |= DLGC_WANTMESSAGE,
                    // Tab chooses the highlighted item, then moves on.
                    VK_TAB if is_dropped(s) => close(s, true),
                    _ => {}
                }
            }
            code as LRESULT
        }
        WM_KEYDOWN => {
            if key(s, w as u16, false) {
                0
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        WM_SYSKEYDOWN if matches!(w as u16, VK_UP | VK_DOWN) => {
            key(s, w as u16, true);
            0
        }
        WM_CHAR => {
            if let Some(ch) = char::from_u32(w as u32) {
                typed(s, ch);
            }
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            pointer_pressed(hwnd);
            if GetFocus() != hwnd {
                SetFocus(hwnd);
            }
            let pt = screen_point(hwnd, l);
            if is_dropped(s) {
                if on_face(hwnd, pt) {
                    close(s, false);
                } else {
                    match popup::menu_hit((*s).dropdown, pt) {
                        Some(Some(i)) => {
                            popup::menu_set_hot((*s).dropdown, Some(i));
                            (*s).pressed = true;
                        }
                        Some(None) => {}
                        None => close(s, false),
                    }
                }
            } else {
                open(s);
                (*s).drag = is_dropped(s);
            }
            0
        }
        WM_MOUSEMOVE => {
            if is_dropped(s) {
                if let Some(Some(i)) = popup::menu_hit((*s).dropdown, screen_point(hwnd, l)) {
                    popup::menu_set_hot((*s).dropdown, Some(i));
                }
            }
            0
        }
        WM_LBUTTONUP => {
            if is_dropped(s) {
                let pt = screen_point(hwnd, l);
                match popup::menu_hit((*s).dropdown, pt) {
                    Some(Some(i)) if (*s).drag || (*s).pressed => {
                        popup::menu_set_hot((*s).dropdown, Some(i));
                        close(s, true);
                    }
                    _ => {
                        (*s).drag = false;
                        (*s).pressed = false;
                    }
                }
            }
            0
        }
        // The wheel never changes a select's value (DESIGN_SPEC §4).
        WM_MOUSEWHEEL => 0,
        WM_CAPTURECHANGED => {
            if l as HWND != hwnd {
                close(s, false);
            }
            0
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            if msg == WM_KILLFOCUS {
                close(s, false);
            }
            focus_changed((*s).app, hwnd);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_ENABLE => {
            if w == 0 {
                close(s, false);
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_UPDATEUISTATE => {
            let result = DefWindowProcW(hwnd, msg, w, l);
            focus_changed((*s).app, hwnd);
            InvalidateRect(hwnd, null(), 0);
            result
        }
        WM_SHOWWINDOW if w == 0 => {
            close(s, false);
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_WINDOWPOSCHANGED => {
            let pos = &*(l as *const WINDOWPOS);
            if pos.flags & SWP_HIDEWINDOW != 0 {
                close(s, false);
            } else if is_dropped(s) && pos.flags & SWP_NOMOVE == 0 {
                let mut r: RECT = zeroed();
                GetWindowRect(hwnd, &mut r);
                popup::move_list((*s).dropdown, Anchor::Below { r, right: false });
            }
            if GetFocus() == hwnd {
                focus_changed((*s).app, hwnd);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let dc = BeginPaint(hwnd, &mut ps);
            let mut r: RECT = zeroed();
            GetClientRect(hwnd, &mut r);
            let app = (*s).app;
            gfx::buffered_item(dc, &r, |dc| paint::combo_face(app, hwnd, dc));
            EndPaint(hwnd, &ps);
            0
        }
        WM_PRINTCLIENT => {
            paint::combo_face((*s).app, hwnd, w as HDC);
            0
        }
        WM_ERASEBKGND => 1,
        WM_DESTROY => {
            let dropdown = std::mem::replace(&mut (*s).dropdown, null_mut());
            popup::destroy(dropdown);
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(s));
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

/// The open dropdown of a select (null when closed; tests, previews).
pub(super) unsafe fn dropdown(select: HWND) -> HWND {
    let s = select_of(select);
    if s.is_null() {
        null_mut()
    } else {
        (*s).dropdown
    }
}

/// The state of `hwnd` when it is one of our selects (null otherwise).
unsafe fn select_of(hwnd: HWND) -> *mut Select {
    if hwnd.is_null() {
        return null_mut();
    }
    let mut class = [0u16; 64];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    if length <= 0 || String::from_utf16_lossy(&class[..length as usize]) != SELECT_CLASS {
        return null_mut();
    }
    select_state(hwnd)
}

// ───────────────────────────── notices, dialog, menus ─────────────────────────────

/// A transient success notice (DESIGN_SPEC §4 Toast). A window that cannot
/// show one (hidden, minimized) keeps it in the status bar instead.
pub(super) unsafe fn notify_success(p: *mut App, text: String) {
    if popup::toast(p, &text) {
        (*p).notice.clear();
    } else {
        (*p).notice = text;
    }
}

/// The warn line of the end-task dialog: only for processes whose image
/// really lives in the Windows directory (never guessed from the name).
pub(super) unsafe fn windows_process_warning(process: &Process) -> Option<&'static str> {
    windows_image_warning(process.pid, process.created)
}

/// [`windows_process_warning`] for a process identity (a zombie holder the
/// Nuclear Zombie panel found, which need not be a row of the table).
pub(super) unsafe fn windows_image_warning(pid: u32, created: u64) -> Option<&'static str> {
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
    let path = crate::actions::executable_path(pid, created).ok()?;
    let mut buffer = [0u16; 260];
    let length = GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) as usize;
    if length == 0 || length >= buffer.len() {
        return None;
    }
    let windows = String::from_utf16_lossy(&buffer[..length])
        .trim_end_matches('\\')
        .to_lowercase();
    path.to_lowercase()
        .starts_with(&format!("{windows}\\"))
        .then(|| {
            tr(
                "Windows 프로세스입니다. 종료하면 Windows가 불안정해지거나 로그아웃될 수 있습니다.",
                "This is a Windows process. Ending it can make Windows unstable or sign you out.",
            )
        })
}

/// The confirm dialog for a MessageBox-style prompt: the prompt's first
/// question is the heading, the rest the text, `title` names the action.
pub(super) unsafe fn confirm(
    p: *mut App,
    title: &str,
    prompt: &str,
    warn: Option<&str>,
    danger: bool,
) -> bool {
    let (heading, body) = popup::split_prompt(prompt);
    popup::confirm_dialog(
        p,
        &ConfirmSpec {
            title: &heading,
            body: &body,
            warn,
            action: title,
            cancel: tr("취소", "Cancel"),
            danger,
        },
    )
}

/// The screen rectangle of a child control.
pub(super) unsafe fn screen_rect(h: HWND) -> RECT {
    let mut r: RECT = zeroed();
    GetWindowRect(h, &mut r);
    r
}

/// Main window moved: popups that follow it close.
pub(super) unsafe fn owner_moved(p: *mut App) {
    popup::owner_moved((*p).hwnd);
    let mut child = GetWindow((*p).hwnd, GW_CHILD);
    while !child.is_null() {
        let s = select_of(child);
        if !s.is_null() {
            close(s, false);
        }
        child = GetWindow(child, GW_HWNDNEXT);
    }
}

// ───────────────────────────── previews ─────────────────────────────

/// Capture the client with `popups` composited where DWM shows them.
pub(super) unsafe fn save_with_popups(
    p: *mut App,
    path: &std::path::Path,
    popups: &[HWND],
) -> Result<(), String> {
    (*p).anim.finish_all();
    save_window_with_popups((*p).hwnd, path, popups)
}

/// [`save_with_popups`] for another Feather window (the Resource Monitor).
pub(super) unsafe fn save_window_with_popups(
    owner: HWND,
    path: &std::path::Path,
    popups: &[HWND],
) -> Result<(), String> {
    let mut client: RECT = zeroed();
    GetClientRect(owner, &mut client);
    let mut frame =
        gfx::Dib::new(client.right, client.bottom).ok_or("Preview allocation failed")?;
    frame.pixels().fill(0xffff_ffff);
    capture::paint_client_and_children(owner, frame.dc())?;
    // Popups that open past the client (a submenu at the right edge) get
    // the reference's desk color around the window.
    let mut bounds = client;
    for &hwnd in popups {
        if let Some(r) = popup::client_bounds(owner, hwnd) {
            bounds = RECT {
                left: bounds.left.min(r.left),
                top: bounds.top.min(r.top),
                right: bounds.right.max(r.right),
                bottom: bounds.bottom.max(r.bottom),
            };
        }
    }
    let (w, h) = (bounds.right - bounds.left, bounds.bottom - bounds.top);
    let mut dib = gfx::Dib::new(w, h).ok_or("Preview allocation failed")?;
    dib.pixels().fill(theme::solid(colors().desk).0);
    BitBlt(
        dib.dc(),
        -bounds.left,
        -bounds.top,
        client.right,
        client.bottom,
        frame.dc(),
        0,
        0,
        SRCCOPY,
    );
    popup::composite(
        owner,
        &mut dib,
        POINT {
            x: bounds.left,
            y: bounds.top,
        },
        popups,
    );
    capture::save_dib(&mut dib, path)
}

/// Select the first process row (with its real priority / efficiency
/// state, read-only) so the page head and the ⋯ menu are enabled.
unsafe fn preview_selection(p: *mut App) {
    if let Some(row) = (0..(*p).rows.len()).find(|i| !(*p).group_headers.contains_key(i)) {
        let item = LVITEMW {
            stateMask: LVIS_SELECTED | LVIS_FOCUSED,
            state: LVIS_SELECTED | LVIS_FOCUSED,
            ..zeroed()
        };
        SendMessageW((*p).list, LVM_SETITEMSTATE, row, &item as *const _ as isize);
    }
    if let Some(s) = selected_process(p) {
        (*p).process_detail_identity = Some((s.pid, s.created));
        (*p).process_settings = crate::actions::process_settings(s.pid, s.created).ok();
    }
    update_buttons(p);
}

/// The view select open with another option highlighted.
unsafe fn select_preview(p: *mut App, path: &std::path::Path) -> Result<(), String> {
    SendMessageW((*p).view_mode, CB_SHOWDROPDOWN, 1, 0);
    SendMessageW((*p).view_mode, WM_KEYDOWN, VK_UP as usize, 0);
    let list = dropdown((*p).view_mode);
    let saved = save_with_popups(p, path, &[list]);
    SendMessageW((*p).view_mode, CB_SHOWDROPDOWN, 0, 0);
    popup::destroy(list);
    saved
}

/// The ⋯ menu (button hovered and pressed) with the priority submenu open.
unsafe fn menu_preview(p: *mut App, path: &std::path::Path) -> Result<(), String> {
    let menu = interactions::build_extra_menu(p, true, None);
    let items = popup::menu_items(menu);
    DestroyMenu(menu);
    let priority = items.iter().position(MenuItem::has_submenu);
    let anchor = Anchor::Below {
        r: screen_rect((*p).more),
        right: true,
    };
    let Some(mut session) = popup::MenuSession::open(p, items, anchor, None) else {
        return Err("Menu preview failed".into());
    };
    if let Some(index) = priority {
        session.set_hot(0, Some(index));
        session.open_submenu(0, false);
        if session.levels.len() > 1 {
            session.set_hot(1, Some(2));
        }
    }
    (*p).anim.set((MORE, anim::part::HOVER), 1.0);
    SendMessageW((*p).more, BM_SETSTATE, 1, 0);
    let levels = session.levels.clone();
    let saved = save_with_popups(p, path, &levels);
    SendMessageW((*p).more, BM_SETSTATE, 0, 0);
    (*p).anim.set((MORE, anim::part::HOVER), 0.0);
    session.destroy();
    saved
}

/// The end-task dialog of a Windows process, keyboard focus on Cancel.
unsafe fn dialog_preview(p: *mut App, path: &std::path::Path) -> Result<(), String> {
    let spec = ConfirmSpec {
        title: tr(
            "svchost.exe (PID 1480) 프로세스를 종료할까요?",
            "End svchost.exe (PID 1480)?",
        ),
        body: tr(
            "저장하지 않은 작업은 사라질 수 있습니다.",
            "Unsaved work may be lost.",
        ),
        warn: Some(tr(
            "Windows 프로세스입니다. 종료하면 Windows가 불안정해지거나 로그아웃될 수 있습니다.",
            "This is a Windows process. Ending it can make Windows unstable or sign you out.",
        )),
        action: tr("작업 끝내기", "End task"),
        cancel: tr("취소", "Cancel"),
        danger: true,
    };
    let (scrim, dialog) = popup::stage_confirm(p, &spec).ok_or("Dialog preview failed")?;
    popup::dialog_state(dialog, 0, true, None);
    let saved = save_with_popups(p, path, &[scrim, dialog]);
    popup::destroy(dialog);
    popup::destroy(scrim);
    saved
}

/// Previews of the Controls track, light and dark: an open select dropdown,
/// the ⋯ menu with its priority submenu, the confirm dialog (Windows
/// process variant), a toast, button hover / pressed / focus states and the
/// settings controls (switch mid-slide, focused select, hovered segment);
/// the popups again at 150 %.
pub(super) unsafe fn save_previews(p: *mut App, dir: &std::path::Path) -> Result<(), String> {
    let theme = (*p).prefs.theme;
    for (value, suffix) in [(1, "light"), (2, "dark")] {
        (*p).prefs.theme = value;
        interactions::apply_theme(p);
        switch_page(p, Page::Processes);
        rebuild(p, None);
        preview_selection(p);
        layout(p);
        select_preview(p, &dir.join(format!("select-open-{suffix}.bmp")))?;
        menu_preview(p, &dir.join(format!("menu-open-{suffix}.bmp")))?;
        dialog_preview(p, &dir.join(format!("dialog-{suffix}.bmp")))?;
        // A success toast.
        let toast = popup::show_toast(
            p,
            tr(
                "종료 요청을 보냈습니다.",
                "The process termination request was sent.",
            ),
        );
        save_with_popups(p, &dir.join(format!("toast-{suffix}.bmp")), &[toast])?;
        popup::destroy(toast);
        // The command palette over its scrim.
        shell::save_palette_preview(p, &dir.join(format!("palette-{suffix}.bmp")))?;
        // Buttons: hover on Nuclear Zombie, keyboard focus on End task,
        // the ⋯ button pressed.
        (*p).anim.set((NUCLEAR, anim::part::HOVER), 1.0);
        SendMessageW((*p).more, BM_SETSTATE, 1, 0);
        preview_focus(p, Some((*p).primary));
        save_with_popups(p, &dir.join(format!("buttons-{suffix}.bmp")), &[])?;
        preview_focus(p, None);
        SendMessageW((*p).more, BM_SETSTATE, 0, 0);
        (*p).anim.set((NUCLEAR, anim::part::HOVER), 0.0);
        // Settings: a switch mid-slide, the Dark segment hovered and the
        // language select focused from the keyboard.
        switch_page(p, Page::Settings);
        layout(p);
        (*p).anim.finish_all();
        let tray = GetDlgItem((*p).hwnd, PREF_TRAY as i32);
        let language = GetDlgItem((*p).hwnd, PREF_LANGUAGE as i32);
        (*p).anim.set((THEME_DARK, anim::part::HOVER), 1.0);
        preview_focus(p, Some(language));
        let before = (*p).anim.value((PREF_TRAY, anim::part::SWITCH));
        InvalidateRect(tray, null(), 0);
        let mut client: RECT = zeroed();
        GetClientRect((*p).hwnd, &mut client);
        let mut dib =
            gfx::Dib::new(client.right, client.bottom).ok_or("Preview allocation failed")?;
        dib.pixels().fill(0xffff_ffff);
        // Mid-slide: paint with the knob halfway (a settled capture would
        // jump it to the end).
        (*p).anim.set((PREF_TRAY, anim::part::SWITCH), 0.5);
        capture::paint_client_and_children((*p).hwnd, dib.dc())?;
        capture::save_dib(&mut dib, &dir.join(format!("settings-states-{suffix}.bmp")))?;
        (*p).anim.set((PREF_TRAY, anim::part::SWITCH), before);
        preview_focus(p, None);
        (*p).anim.set((THEME_DARK, anim::part::HOVER), 0.0);
        // Outside focus rings where controls sit close together: the
        // selected theme segment (ring over the fg fill's neighbours) and a
        // nav item (2 px from the next one).
        let selected = GetDlgItem(
            (*p).hwnd,
            match (*p).prefs.theme {
                1 => THEME_LIGHT,
                2 => THEME_DARK,
                _ => THEME_SYSTEM,
            } as i32,
        );
        preview_focus(p, Some(selected));
        capture::save_client(p, &dir.join(format!("focus-segment-{suffix}.bmp")))?;
        preview_focus(p, Some((*p).nav[1]));
        capture::save_client(p, &dir.join(format!("focus-nav-{suffix}.bmp")))?;
        preview_focus(p, None);
    }
    (*p).prefs.theme = theme;
    interactions::apply_theme(p);
    // 150 %: the popups scale with the window (controlled WM_DPICHANGED).
    let resize = |dpi: i32, width: i32, height: i32| {
        let r = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        SendMessageW(
            (*p).hwnd,
            WM_DPICHANGED,
            (dpi | (dpi << 16)) as usize,
            &r as *const _ as isize,
        );
        fit_client((*p).hwnd, width, height);
    };
    resize(144, 1800, 1230);
    switch_page(p, Page::Processes);
    rebuild(p, None);
    preview_selection(p);
    layout(p);
    select_preview(p, &dir.join("select-open-dpi150.bmp"))?;
    menu_preview(p, &dir.join("menu-open-dpi150.bmp"))?;
    dialog_preview(p, &dir.join("dialog-dpi150.bmp"))?;
    resize(96, 1200, 820);
    rebuild(p, None);
    layout(p);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::mpsc;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// A hidden main window (like `ui::tests::TestWindow`) for control tests.
    struct Harness {
        p: *mut App,
        class: Vec<u16>,
        _snapshots: mpsc::SyncSender<MonitorSample>,
        _complete: mpsc::Sender<JobResult>,
        commands: mpsc::Receiver<Command>,
        _jobs: mpsc::Receiver<Job>,
    }

    impl Harness {
        fn new() -> Self {
            unsafe {
                init_controls();
                let (snapshots, rx) = mpsc::sync_channel(1);
                let (tx, commands) = mpsc::channel();
                let (jobs_tx, jobs) = mpsc::channel();
                let (complete, results) = mpsc::channel();
                let p = Box::into_raw(Box::new(App::new(rx, tx, jobs_tx, results)));
                let class = wide(&format!(
                    "FeatherControlsTest_{}_{}",
                    std::process::id(),
                    NEXT.fetch_add(1, AtomicOrdering::Relaxed)
                ));
                assert!(!create_window(p, &class, 1200, 820).is_null());
                fit_client((*p).hwnd, 1200, 820);
                // Deterministic cues: the last input was the mouse.
                pointer_pressed((*p).hwnd);
                Self {
                    p,
                    class,
                    _snapshots: snapshots,
                    _complete: complete,
                    commands,
                    _jobs: jobs,
                }
            }
        }
        /// Post keys for a modal loop to find.
        fn post_keys(&self, keys: &[(u32, u16)]) {
            unsafe {
                for &(message, vk) in keys {
                    PostMessageW((*self.p).hwnd, message, vk as usize, 0);
                }
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow((*self.p).hwnd);
                dispose(self.p);
                UnregisterClassW(self.class.as_ptr(), GetModuleHandleW(null()));
                pump();
            }
        }
    }

    /// Dispatch everything queued (popups close themselves with posted WM_CLOSE).
    unsafe fn pump() {
        let mut msg: MSG = zeroed();
        while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_QUIT {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    thread_local! {
        static COMMANDS: std::cell::RefCell<Vec<(usize, u32)>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    unsafe extern "system" fn recorder_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if msg == WM_COMMAND {
            COMMANDS.with(|c| c.borrow_mut().push((w & 0xffff, (w >> 16) as u32)));
            return 0;
        }
        DefWindowProcW(hwnd, msg, w, l)
    }

    /// A select under a parent that records WM_COMMAND notifications.
    unsafe fn recorded_select(p: *mut App, labels: &[&str]) -> (HWND, HWND) {
        let class = wide("FeatherSelectRecorder");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(recorder_proc),
            hInstance: GetModuleHandleW(null()),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        });
        let parent = CreateWindowExW(
            0,
            class.as_ptr(),
            null(),
            WS_POPUP,
            100,
            100,
            400,
            300,
            null_mut(),
            null_mut(),
            GetModuleHandleW(null()),
            null(),
        );
        assert!(!parent.is_null());
        let select = create_select(parent, p, "Refresh", 0x7777);
        assert!(!select.is_null());
        MoveWindow(select, 20, 20, 120, 28, 0);
        for label in labels {
            SendMessageW(select, CB_ADDSTRING, 0, wide(label).as_ptr() as isize);
        }
        COMMANDS.with(|c| c.borrow_mut().clear());
        (parent, select)
    }

    fn changes() -> usize {
        COMMANDS.with(|c| {
            let n = c
                .borrow()
                .iter()
                .filter(|&&(id, code)| id == 0x7777 && code == CBN_SELCHANGE)
                .count();
            c.borrow_mut().clear();
            n
        })
    }

    unsafe fn text(select: HWND, index: usize) -> String {
        paint::combo_text(select, index as isize)
    }

    fn rgb(pixel: u32) -> u32 {
        (pixel >> 16 & 0xff) | (pixel >> 8 & 0xff) << 8 | (pixel & 0xff) << 16
    }

    const RATES: [&str; 6] = ["Paused", "0.25 s", "0.5 s", "1 s", "2 s", "5 s"];

    #[test]
    fn select_emulates_the_combobox_messages_the_app_uses() {
        let test = Harness::new();
        unsafe {
            let (parent, s) = recorded_select(test.p, &RATES);
            let mut class = [0u16; 64];
            let n = GetClassNameW(s, class.as_mut_ptr(), 64);
            assert_eq!(String::from_utf16_lossy(&class[..n as usize]), SELECT_CLASS);
            assert_eq!(SendMessageW(s, CB_GETCOUNT, 0, 0), 6);
            assert_eq!(SendMessageW(s, CB_GETCURSEL, 0, 0), -1);
            assert_eq!(paint::window_text(s), "Refresh");
            assert_eq!(SendMessageW(s, CB_SETCURSEL, 3, 0), 3);
            assert_eq!(SendMessageW(s, CB_GETCURSEL, 0, 0), 3);
            assert_eq!(changes(), 0, "CB_SETCURSEL never notifies");
            // The accessible name carries the value; relabelling keeps it.
            assert_eq!(paint::window_text(s), "Refresh: 1 s");
            SetWindowTextW(s, wide("Rate").as_ptr());
            assert_eq!(paint::window_text(s), "Rate: 1 s");
            SendMessageW(s, WM_KEYDOWN, VK_DOWN as usize, 0);
            assert_eq!(paint::window_text(s), "Rate: 2 s");
            changes();
            SendMessageW(s, CB_SETCURSEL, 3, 0);
            assert_eq!(SendMessageW(s, CB_GETLBTEXTLEN, 1, 0), 6);
            assert_eq!(text(s, 1), "0.25 s");
            assert_eq!(SendMessageW(s, CB_GETLBTEXTLEN, 9, 0), CB_ERR as isize);
            let mut buffer = [0u16; 16];
            assert_eq!(
                SendMessageW(s, CB_GETLBTEXT, 9, buffer.as_mut_ptr() as isize),
                CB_ERR as isize
            );
            let exact = wide("2 s");
            assert_eq!(
                SendMessageW(s, CB_FINDSTRINGEXACT, usize::MAX, exact.as_ptr() as isize),
                4
            );
            let prefix = wide("0.");
            assert_eq!(
                SendMessageW(s, CB_FINDSTRING, usize::MAX, prefix.as_ptr() as isize),
                1
            );
            assert_eq!(SendMessageW(s, CB_SETITEMDATA, 2, 42), 0);
            assert_eq!(SendMessageW(s, CB_GETITEMDATA, 2, 0), 42);
            // Out of range clears the selection and fails, like a ComboBox.
            assert_eq!(SendMessageW(s, CB_SETCURSEL, 99, 0), CB_ERR as isize);
            assert_eq!(SendMessageW(s, CB_GETCURSEL, 0, 0), -1);
            SendMessageW(s, CB_SETCURSEL, 5, 0);
            let inserted = wide("10 s");
            assert_eq!(
                SendMessageW(s, CB_INSERTSTRING, 0, inserted.as_ptr() as isize),
                0
            );
            assert_eq!(SendMessageW(s, CB_GETCURSEL, 0, 0), 6, "selection follows");
            assert_eq!(SendMessageW(s, CB_DELETESTRING, 0, 0), 6);
            assert_eq!(SendMessageW(s, CB_GETCURSEL, 0, 0), 5);
            let font = (*test.p).fonts.small;
            SendMessageW(s, WM_SETFONT, font as usize, 0);
            assert_eq!(SendMessageW(s, WM_GETFONT, 0, 0), font as isize);
            assert_eq!(SendMessageW(s, CB_GETDROPPEDSTATE, 0, 0), 0);
            assert_eq!(SendMessageW(s, CB_RESETCONTENT, 0, 0), 0);
            assert_eq!(SendMessageW(s, CB_GETCOUNT, 0, 0), 0);
            assert_eq!(SendMessageW(s, CB_GETCURSEL, 0, 0), -1);
            assert_eq!(paint::window_text(s), "Rate");
            // Every app select is one of these (no ComboBox left).
            for h in [
                (*test.p).rate,
                (*test.p).view_mode,
                (*test.p).filter_control,
            ]
            .into_iter()
            .chain(
                [PREF_LANGUAGE, PREF_RATE, PREF_START]
                    .map(|id| GetDlgItem((*test.p).hwnd, id as i32)),
            ) {
                let n = GetClassNameW(h, class.as_mut_ptr(), 64);
                assert_eq!(String::from_utf16_lossy(&class[..n as usize]), SELECT_CLASS);
            }
            assert_eq!(SendMessageW((*test.p).rate, CB_GETCOUNT, 0, 0), 6);
            DestroyWindow(parent);
        }
    }

    #[test]
    fn select_keyboard_changes_values_opens_chooses_and_types_ahead() {
        let test = Harness::new();
        unsafe {
            let (parent, s) = recorded_select(test.p, &RATES);
            SendMessageW(s, CB_SETCURSEL, 3, 0);
            let key = |vk: u16| SendMessageW(s, WM_KEYDOWN, vk as usize, 0);
            let cursel = || SendMessageW(s, CB_GETCURSEL, 0, 0);
            let dropped = || SendMessageW(s, CB_GETDROPPEDSTATE, 0, 0) != 0;
            // Closed: arrows change the value at once (one notification each).
            key(VK_DOWN);
            assert_eq!((cursel(), changes()), (4, 1));
            key(VK_UP);
            key(VK_UP);
            assert_eq!((cursel(), changes()), (2, 2));
            key(VK_END);
            key(VK_END);
            assert_eq!((cursel(), changes()), (5, 1), "no change, no notification");
            key(VK_HOME);
            assert_eq!((cursel(), changes()), (0, 1));
            key(VK_UP);
            assert_eq!((cursel(), changes()), (0, 0), "clamped");
            // Dialog codes: arrows and characters, Enter; Esc only while open.
            let code = |vk: u16| {
                let msg = MSG {
                    hwnd: s,
                    message: WM_KEYDOWN,
                    wParam: vk as usize,
                    ..zeroed()
                };
                SendMessageW(s, WM_GETDLGCODE, vk as usize, &msg as *const MSG as isize) as u32
            };
            assert_ne!(code(VK_DOWN) & DLGC_WANTARROWS, 0);
            assert_ne!(code(VK_RETURN) & DLGC_WANTMESSAGE, 0);
            assert_eq!(code(VK_ESCAPE) & DLGC_WANTMESSAGE, 0);
            // Alt+Down opens with the current item highlighted.
            SendMessageW(s, WM_SYSKEYDOWN, VK_DOWN as usize, 1 << 29);
            assert!(dropped());
            let list = dropdown(s);
            assert!(!list.is_null());
            assert_eq!(popup::menu_hot(list), Some(0));
            assert_ne!(code(VK_ESCAPE) & DLGC_WANTMESSAGE, 0);
            // Open: arrows move the highlight only; Esc keeps the value.
            key(VK_DOWN);
            key(VK_DOWN);
            assert_eq!(popup::menu_hot(list), Some(2));
            assert_eq!((cursel(), changes()), (0, 0));
            key(VK_ESCAPE);
            assert!(!dropped());
            assert_eq!((cursel(), changes()), (0, 0));
            // F4 opens, Enter chooses the highlighted item.
            key(VK_F4);
            key(VK_DOWN);
            key(VK_RETURN);
            assert!(!dropped());
            assert_eq!((cursel(), changes()), (1, 1));
            // Space opens too; Tab chooses and lets the dialog manager move on.
            key(VK_SPACE);
            assert!(dropped());
            key(VK_END);
            assert_eq!(code(VK_TAB) & DLGC_WANTMESSAGE, 0);
            assert!(!dropped());
            assert_eq!((cursel(), changes()), (5, 1));
            // Type-ahead selects by first letter (closed and open).
            // (a second letter within a second extends the prefix: "p2")
            let pause = || (*select_state(s)).typed_at = None;
            SendMessageW(s, WM_CHAR, 'p' as usize, 0);
            assert_eq!((cursel(), changes()), (0, 1));
            SendMessageW(s, WM_CHAR, '2' as usize, 0);
            assert_eq!(cursel(), 0, "no item starts with p2");
            pause();
            SendMessageW(s, WM_CHAR, '2' as usize, 0);
            assert_eq!(cursel(), 4);
            pause();
            SendMessageW(s, CB_SHOWDROPDOWN, 1, 0);
            SendMessageW(s, WM_CHAR, '1' as usize, 0);
            assert_eq!(popup::menu_hot(dropdown(s)), Some(3));
            SendMessageW(s, CB_SHOWDROPDOWN, 0, 0);
            assert_eq!(cursel(), 4, "closing programmatically keeps the value");
            changes();
            // The wheel never changes the value; focus loss closes.
            SendMessageW(s, WM_MOUSEWHEEL, 120usize << 16, 0);
            assert_eq!((cursel(), changes()), (4, 0));
            SendMessageW(s, CB_SHOWDROPDOWN, 1, 0);
            SendMessageW(s, WM_KILLFOCUS, 0, 0);
            assert!(!dropped());
            // Disabled selects do not open.
            EnableWindow(s, 0);
            key(VK_F4);
            assert!(!dropped());
            DestroyWindow(parent);
        }
    }

    #[test]
    fn select_type_ahead_cycles_and_matches_prefixes() {
        let items: Vec<String> = ["Paused", "Process tree", "App groups", "Pinned"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(type_ahead(&items, -1, "p"), Some(0));
        assert_eq!(type_ahead(&items, 0, "p"), Some(1));
        assert_eq!(
            type_ahead(&items, 1, "pp"),
            Some(3),
            "repeated letter cycles"
        );
        assert_eq!(type_ahead(&items, 3, "p"), Some(0), "wraps");
        assert_eq!(type_ahead(&items, 0, "pr"), Some(1));
        assert_eq!(type_ahead(&items, 0, "pi"), Some(3));
        assert_eq!(type_ahead(&items, 0, "x"), None);
        assert_eq!(type_ahead(&[], 0, "p"), None);
    }

    #[test]
    fn select_mouse_opens_drags_and_chooses_items() {
        let test = Harness::new();
        unsafe {
            let (parent, s) = recorded_select(test.p, &RATES);
            SendMessageW(s, CB_SETCURSEL, 3, 0);
            let at = |pt: POINT| {
                let mut local = pt;
                ScreenToClient(s, &mut local);
                (local.x as u16 as isize) | ((local.y as u16 as isize) << 16)
            };
            let center = |r: RECT| POINT {
                x: (r.left + r.right) / 2,
                y: (r.top + r.bottom) / 2,
            };
            // Click: opens under the select, left-aligned, at least as wide.
            SendMessageW(s, WM_LBUTTONDOWN, 1usize, 5 | (5 << 16));
            SendMessageW(s, WM_LBUTTONUP, 0, 5 | (5 << 16));
            let list = dropdown(s);
            assert!(!list.is_null(), "a click opens the dropdown");
            let (content, _) = popup::geometry(list).unwrap();
            let face = screen_rect(s);
            assert_eq!(content.left, face.left);
            assert!(content.top >= face.bottom || content.bottom <= face.top);
            assert!(content.right - content.left >= face.right - face.left);
            // Hover highlights, a click on an item chooses it.
            let item = center(popup::row_rect(list, 5).unwrap());
            SendMessageW(s, WM_MOUSEMOVE, 0, at(item));
            assert_eq!(popup::menu_hot(list), Some(5));
            SendMessageW(s, WM_LBUTTONDOWN, 1usize, at(item));
            SendMessageW(s, WM_LBUTTONUP, 0, at(item));
            assert!(dropdown(s).is_null());
            assert_eq!((SendMessageW(s, CB_GETCURSEL, 0, 0), changes()), (5, 1));
            // Press on the face, drag to an item, release: chooses it.
            SendMessageW(s, WM_LBUTTONDOWN, 1usize, 5 | (5 << 16));
            let list = dropdown(s);
            let item = center(popup::row_rect(list, 1).unwrap());
            SendMessageW(s, WM_MOUSEMOVE, 1usize, at(item));
            SendMessageW(s, WM_LBUTTONUP, 0, at(item));
            assert!(dropdown(s).is_null());
            assert_eq!((SendMessageW(s, CB_GETCURSEL, 0, 0), changes()), (1, 1));
            // A click outside closes without a change; a click on the face
            // of an open select closes it.
            SendMessageW(s, WM_LBUTTONDOWN, 1usize, 5 | (5 << 16));
            SendMessageW(s, WM_LBUTTONUP, 0, 5 | (5 << 16));
            let far = POINT {
                x: face.right + 600,
                y: face.bottom + 600,
            };
            SendMessageW(s, WM_LBUTTONDOWN, 1usize, at(far));
            assert!(dropdown(s).is_null());
            SendMessageW(s, WM_LBUTTONDOWN, 1usize, 5 | (5 << 16));
            SendMessageW(s, WM_LBUTTONUP, 0, 5 | (5 << 16));
            SendMessageW(s, WM_LBUTTONDOWN, 1usize, 5 | (5 << 16));
            assert!(dropdown(s).is_null());
            assert_eq!((SendMessageW(s, CB_GETCURSEL, 0, 0), changes()), (1, 0));
            DestroyWindow(parent);
        }
    }

    #[test]
    fn app_selects_notify_the_existing_handlers() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let view = (*p).view_mode;
            assert_eq!(SendMessageW(view, CB_GETCURSEL, 0, 0), 0);
            // Down on the closed view select: Process tree.
            SendMessageW(view, WM_KEYDOWN, VK_DOWN as usize, 0);
            assert!((*p).tree_mode);
            // Open, move to App groups, Enter.
            SendMessageW(view, WM_SYSKEYDOWN, VK_DOWN as usize, 1 << 29);
            SendMessageW(view, WM_KEYDOWN, VK_DOWN as usize, 0);
            SendMessageW(view, WM_KEYDOWN, VK_RETURN as usize, 0);
            assert!((*p).group_mode && !(*p).tree_mode);
            // The status-bar rate select reconfigures the monitor.
            while test.commands.try_recv().is_ok() {}
            SendMessageW((*p).rate, WM_KEYDOWN, VK_END as usize, 0);
            assert_eq!((*p).interval, 5000);
            assert!(test
                .commands
                .try_iter()
                .any(|c| matches!(c, Command::Configure { interval: 5000, .. })));
            SendMessageW((*p).rate, WM_KEYDOWN, VK_HOME as usize, 0);
            assert!((*p).paused, "the Paused option pauses");
        }
    }

    fn menu() -> Vec<MenuItem> {
        vec![
            MenuItem::item(1, "End task"),
            MenuItem {
                enabled: false,
                ..MenuItem::item(2, "Efficiency mode")
            },
            MenuItem::separator(),
            MenuItem {
                submenu: vec![
                    MenuItem::item(11, "Idle"),
                    MenuItem {
                        checked: true,
                        ..MenuItem::item(12, "Normal")
                    },
                    MenuItem::item(13, "Above normal"),
                ],
                ..MenuItem::item(0, "Priority")
            },
            MenuItem::item(4, "Refresh"),
        ]
    }

    #[test]
    fn menu_model_mirrors_the_native_menus() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let native = interactions::build_extra_menu(p, true, None);
            let items = popup::menu_items(native);
            DestroyMenu(native);
            let find = |id: usize| items.iter().find(|i| i.id == id && !i.separator);
            assert_eq!(find(PRIMARY).unwrap().hint.as_deref(), Some("Del"));
            assert_eq!(find(END_TREE).unwrap().hint.as_deref(), Some("Shift+Del"));
            assert_eq!(find(REFRESH).unwrap().hint.as_deref(), Some("F5"));
            assert!(items.iter().any(|i| i.separator));
            let priority = items.iter().find(|i| i.has_submenu()).unwrap();
            assert_eq!(
                priority.submenu.iter().map(|i| i.id).collect::<Vec<_>>(),
                [500, 501, 502, 503]
            );
            assert!(priority.enabled);
            assert_eq!(
                find(EXPAND_ALL).unwrap().enabled,
                (*p).tree_mode,
                "disabled items stay disabled"
            );
            // Without a process the priority submenu is disabled.
            let native = interactions::build_extra_menu(p, false, None);
            let items = popup::menu_items(native);
            DestroyMenu(native);
            assert!(!items.iter().find(|i| i.has_submenu()).unwrap().enabled);
            // The settings menu: a disabled status header and checked language.
            let native = create_settings_menu(&Ok(crate::replacement::Status::Inactive));
            let items = popup::menu_items(native);
            DestroyMenu(native);
            assert!(!items[0].enabled);
            let checked: Vec<usize> = items.iter().filter(|i| i.checked).map(|i| i.id).collect();
            assert_eq!(checked.len(), 1);
            assert!([LANGUAGE_ENGLISH, LANGUAGE_KOREAN].contains(&checked[0]));
        }
    }

    #[test]
    fn menu_keyboard_navigation_returns_the_chosen_command() {
        let test = Harness::new();
        unsafe {
            let anchor = Anchor::Point(POINT { x: 200, y: 200 });
            let mut s = popup::MenuSession::open(test.p, menu(), anchor, None).unwrap();
            assert_eq!(s.key(VK_DOWN), popup::Flow::Continue);
            assert_eq!(s.hot(0), Some(0));
            s.key(VK_DOWN);
            assert_eq!(
                s.hot(0),
                Some(3),
                "skips the disabled item and the separator"
            );
            s.key(VK_RIGHT);
            assert_eq!(s.levels.len(), 2, "Right opens the submenu");
            assert_eq!(s.hot(1), Some(0), "with its first item");
            s.key(VK_DOWN);
            assert_eq!(s.key(VK_RETURN), popup::Flow::Choose(12));
            s.key(VK_LEFT);
            assert_eq!(s.levels.len(), 1, "Left closes it");
            assert_eq!(s.hot(0), Some(3));
            s.key(VK_RETURN);
            assert_eq!(s.levels.len(), 2, "Enter on a submenu item opens it");
            s.key(VK_ESCAPE);
            assert_eq!(s.levels.len(), 1, "Esc closes one level");
            s.letter('r');
            assert_eq!(s.hot(0), Some(4));
            s.letter('e');
            assert_eq!(s.hot(0), Some(0));
            s.key(VK_END);
            assert_eq!(s.key(VK_SPACE), popup::Flow::Choose(4));
            assert_eq!(s.key(VK_ESCAPE), popup::Flow::Cancel);
            // Rows are hit-tested per level; separators are not items.
            let row = popup::row_rect(s.levels[0], 2).unwrap();
            let on_separator = POINT {
                x: (row.left + row.right) / 2,
                y: (row.top + row.bottom) / 2,
            };
            assert_eq!(s.hit(on_separator), Some((0, None)));
            let row = popup::row_rect(s.levels[0], 4).unwrap();
            assert_eq!(
                s.hit(POINT {
                    x: row.left + 4,
                    y: row.top + 4
                }),
                Some((0, Some(4)))
            );
            assert_eq!(s.hit(POINT { x: -5000, y: -5000 }), None);
            // The submenu opens beside its item, first row on the parent row.
            s.set_hot(0, Some(3));
            assert!(s.open_submenu(0, false));
            let parent = popup::row_rect(s.levels[0], 3).unwrap();
            let child = popup::row_rect(s.levels[1], 0).unwrap();
            assert_eq!(child.top, parent.top);
            assert!(child.left > parent.right || child.right < parent.left);
            s.destroy();
        }
    }

    #[test]
    fn track_menu_is_modal_and_returns_the_command_id() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let native = CreatePopupMenu();
            for (id, label) in [(41, "First\tDel"), (42, "Second"), (43, "Third")] {
                AppendMenuW(native, MF_STRING, id, wide(label).as_ptr());
            }
            AppendMenuW(native, MF_STRING | MF_GRAYED, 44, wide("Disabled").as_ptr());
            let anchor = || Anchor::Point(POINT { x: 300, y: 300 });
            // Opened with the mouse nothing is highlighted: Down, Down = Second.
            let expected = if cues_hidden((*p).hwnd) { 42 } else { 43 };
            test.post_keys(&[
                (WM_KEYDOWN, VK_DOWN),
                (WM_KEYDOWN, VK_DOWN),
                (WM_KEYDOWN, VK_RETURN),
            ]);
            assert_eq!(popup::track_menu(p, native, anchor()), expected);
            test.post_keys(&[(WM_KEYDOWN, VK_ESCAPE)]);
            assert_eq!(popup::track_menu(p, native, anchor()), 0);
            // First-letter jump from real key presses (the loop translates
            // no keys, so no WM_CHAR follows them) and from characters.
            test.post_keys(&[(WM_KEYDOWN, 'T' as u16), (WM_KEYDOWN, VK_RETURN)]);
            assert_eq!(popup::track_menu(p, native, anchor()), 43);
            test.post_keys(&[
                (WM_KEYDOWN, 'S' as u16),
                (WM_KEYUP, 'S' as u16),
                (WM_KEYDOWN, VK_RETURN),
            ]);
            assert_eq!(popup::track_menu(p, native, anchor()), 42);
            test.post_keys(&[(WM_CHAR, 't' as u16), (WM_KEYDOWN, VK_RETURN)]);
            assert_eq!(popup::track_menu(p, native, anchor()), 43);
            // Disabled items cannot be chosen (letters skip them): Enter
            // keeps the initial highlight (none after a click), Alt cancels.
            test.post_keys(&[
                (WM_KEYDOWN, 'D' as u16),
                (WM_KEYDOWN, VK_RETURN),
                (WM_SYSKEYDOWN, VK_MENU),
            ]);
            let unchanged = if cues_hidden((*p).hwnd) { 0 } else { 41 };
            assert_eq!(popup::track_menu(p, native, anchor()), unchanged);
            // The window moving under the menu closes it (Win+Arrow).
            SetTimer((*p).hwnd, 0x5150, 1, Some(move_owner));
            assert_eq!(popup::track_menu(p, native, anchor()), 0);
            DestroyMenu(native);
            pump();
        }
    }

    /// A timer callback that moves and resizes the (hidden) test window,
    /// then posts Esc for the modal loop that is running.
    unsafe extern "system" fn move_owner(hwnd: HWND, _: u32, id: usize, _: u32) {
        KillTimer(hwnd, id);
        let mut r: RECT = zeroed();
        GetWindowRect(hwnd, &mut r);
        SetWindowPos(
            hwnd,
            null_mut(),
            r.left + 40,
            r.top + 30,
            r.right - r.left - 100,
            r.bottom - r.top - 60,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        // A key the dialog ignores, then Esc: both after the move.
        PostMessageW(hwnd, WM_KEYDOWN, VK_F2 as usize, 0);
        PostMessageW(hwnd, WM_KEYDOWN, VK_ESCAPE as usize, 0);
    }

    #[test]
    fn confirm_dialog_follows_the_window_while_open() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let spec = ConfirmSpec {
                title: "End test.exe?",
                body: "Unsaved work may be lost.",
                warn: None,
                action: "End task",
                cancel: "Cancel",
                danger: true,
            };
            SetTimer((*p).hwnd, 0x5151, 1, Some(move_owner));
            assert!(!popup::confirm_dialog(p, &spec));
            // The dialog and scrim are fading out: still there, already moved.
            let mut client: RECT = zeroed();
            GetClientRect((*p).hwnd, &mut client);
            assert_eq!(client.right, 1100, "the window was resized");
            let mut origin = POINT { x: 0, y: 0 };
            ClientToScreen((*p).hwnd, &mut origin);
            let mut window: RECT = zeroed();
            GetWindowRect((*p).hwnd, &mut window);
            let boxes: Vec<RECT> = popup::thread_popups()
                .into_iter()
                .filter_map(|h| popup::geometry(h).map(|(r, _)| r))
                .collect();
            let dialog = boxes
                .iter()
                .find(|r| r.right - r.left == 440)
                .expect("dialog");
            assert!(
                (dialog.left - origin.x - (client.right - 440) / 2).abs() <= 1,
                "re-centred horizontally"
            );
            let middle = (dialog.top + dialog.bottom) / 2 - origin.y;
            assert!((middle - client.bottom / 2).abs() <= 1, "and vertically");
            let scrim = boxes
                .iter()
                .find(|r| r.right - r.left > 440)
                .expect("scrim");
            assert!(
                scrim.left <= origin.x && scrim.right >= origin.x + client.right,
                "the scrim covers the moved window"
            );
            assert!(scrim.left >= window.left && scrim.right <= window.right);
            assert!(scrim.bottom >= origin.y + client.bottom);
            popup::finish_closing();
            pump();
        }
    }

    #[test]
    fn confirm_dialog_lines_fit_their_box_without_losing_words() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            // Real prompts whose GDI widths exceed the 390 px text box by
            // fractional-width line breaking (and their Korean texts).
            let cases = [
                (
                    "End svchost.exe (PID 1480)?\n\nUnsaved work may be lost.",
                    Some("This is a Windows process. Ending it can make Windows unstable or sign you out."),
                ),
                (
                    "Restore the default Windows Task Manager?\n\nAdministrator permission is required and this applies to all users on this PC. Feather will remain in its installation folder.",
                    None,
                ),
                (
                    "End chrome.exe (PID 12345) and its descendants, 57 processes in total?\n\nThis includes the entire captured tree, including hidden children. Processes created after this confirmation opened are not included.\n\nUnsaved work may be lost.",
                    None,
                ),
                (
                    "Use installed Feather as the Windows Task Manager? This applies to Ctrl+Shift+Esc and Ctrl+Alt+Delete and requires administrator permission.",
                    None,
                ),
                (
                    "chrome.exe (PID 12345) 및 하위 프로세스, 총 57개를 종료할까요?\n\n현재 목록에서 확인된 트리 전체가 대상이며 숨겨진 하위 항목도 포함됩니다.",
                    Some("Windows 프로세스입니다. 종료하면 Windows가 불안정해지거나 로그아웃될 수 있습니다."),
                ),
            ];
            let words = |s: &str| {
                s.split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<String>>()
            };
            for (prompt, warn) in cases {
                let (title, body) = popup::split_prompt(prompt);
                let spec = ConfirmSpec {
                    title: &title,
                    body: &body,
                    warn,
                    action: "End task",
                    cancel: "Cancel",
                    danger: true,
                };
                let (scrim, dialog) = popup::stage_confirm(p, &spec).unwrap();
                let (lines, width) = popup::dialog_lines(dialog).unwrap();
                assert!(lines.len() > 2);
                for (text, drawn) in &lines {
                    assert!(
                        *drawn <= width,
                        "{text:?} is drawn {drawn} px wide in a {width} px box"
                    );
                }
                let expected: Vec<String> = [title.as_str(), body.as_str(), warn.unwrap_or("")]
                    .iter()
                    .flat_map(|s| words(s))
                    .collect();
                let shown: Vec<String> = lines.iter().flat_map(|(t, _)| words(t)).collect();
                assert_eq!(shown, expected, "every word is shown, in order");
                // No ink right of the text box (the padding stays clear).
                popup::settle(dialog);
                let content = popup::surface_content(dialog).unwrap();
                let c = colors();
                let text_right = content.left + 1 + 24 + width;
                for y in content.top + 2..content.bottom - 64 {
                    for x in text_right + 2..content.right - 10 {
                        let pixel = popup::pixel(dialog, x, y).unwrap();
                        // (x stops short of the rounded top-right corner)
                        assert_eq!(rgb(pixel), c.surface, "ink at {x},{y} past the text box");
                    }
                }
                popup::destroy(dialog);
                popup::destroy(scrim);
            }
        }
    }

    #[test]
    fn pressed_buttons_translate_their_whole_face() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            layout(p);
            let button = (*p).more;
            assert!(IsWindowEnabled(button) != 0);
            let r = child_rect(p, button);
            let capture = || {
                let mut frame = gfx::Dib::new(1200, 820).unwrap();
                frame.pixels().fill(0xffff_ffff);
                capture::paint_client_and_children((*p).hwnd, frame.dc()).unwrap();
                frame
            };
            let c = colors();
            let parent = paint::parent_background(p, button);
            let x = (r.left + r.right) / 2;
            let mut released = capture();
            assert_eq!(rgb(released.pixel(x, r.top)), c.border);
            assert_eq!(rgb(released.pixel(x, r.bottom - 1)), c.border);
            assert_eq!(rgb(released.pixel(x, r.bottom)), parent);
            SendMessageW(button, BM_SETSTATE, 1, 0);
            let mut pressed = capture();
            assert_eq!(rgb(pressed.pixel(x, r.top)), parent, "the face moved down");
            assert_eq!(rgb(pressed.pixel(x, r.top + 1)), c.border);
            assert_eq!(rgb(pressed.pixel(x, r.bottom - 1)), c.surface);
            assert_eq!(
                rgb(pressed.pixel(x, r.bottom)),
                c.border,
                "its bottom edge is painted on the window below the button"
            );
            SendMessageW(button, BM_SETSTATE, 0, 0);
            let mut again = capture();
            assert_eq!(rgb(again.pixel(x, r.bottom)), parent);
            assert_eq!(rgb(again.pixel(x, r.top)), c.border);
        }
    }

    #[test]
    fn confirm_dialog_follows_the_keyboard_and_keeps_modal_semantics() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let ask = || {
                confirm_with(
                    p,
                    "End task",
                    "End test.exe (PID 7)?\n\nUnsaved work may be lost.",
                    None,
                    true,
                )
            };
            // Cancel has the initial focus.
            test.post_keys(&[(WM_KEYDOWN, VK_RETURN)]);
            while test.commands.try_recv().is_ok() {}
            assert!(!ask());
            assert!(!(*p).modal);
            let paused: Vec<bool> = test
                .commands
                .try_iter()
                .filter_map(|c| match c {
                    Command::Configure { paused, .. } => Some(paused),
                    _ => None,
                })
                .collect();
            assert_eq!(paused, [true, false], "sampling pauses while asking");
            let mut msg: MSG = zeroed();
            assert_ne!(
                PeekMessageW(
                    &mut msg,
                    (*p).hwnd,
                    SNAPSHOT_READY,
                    SNAPSHOT_READY,
                    PM_REMOVE
                ),
                0,
                "queued samples are re-posted"
            );
            assert_ne!(
                PeekMessageW(&mut msg, (*p).hwnd, JOB_READY, JOB_READY, PM_REMOVE),
                0
            );
            test.post_keys(&[(WM_KEYDOWN, VK_TAB), (WM_KEYDOWN, VK_RETURN)]);
            assert!(ask(), "Tab moves to the action");
            test.post_keys(&[(WM_KEYDOWN, VK_RIGHT), (WM_KEYDOWN, VK_SPACE)]);
            assert!(ask(), "arrows move too, Space activates");
            test.post_keys(&[
                (WM_KEYDOWN, VK_TAB),
                (WM_KEYDOWN, VK_TAB),
                (WM_KEYDOWN, VK_RETURN),
            ]);
            assert!(!ask(), "Tab cycles back to Cancel");
            test.post_keys(&[(WM_KEYDOWN, VK_TAB), (WM_KEYDOWN, VK_ESCAPE)]);
            assert!(!ask(), "Esc cancels");
            test.post_keys(&[(WM_SYSKEYDOWN, VK_F4)]);
            assert!(!ask(), "Alt+F4 cancels");
            pump();
        }
    }

    #[test]
    fn keyboard_focus_comes_back_after_the_dialog_closes() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let ask = |p: *mut App| {
                confirm_with(
                    p,
                    "End task",
                    "End test.exe (PID 7)?\n\nUnsaved work may be lost.",
                    None,
                    true,
                )
            };
            // The ⋯ button opened it: the dialog disables it (focus is lost
            // with it), Esc closes the dialog, the button gets focus back.
            let more = (*p).more;
            assert_ne!(IsWindowEnabled(more), 0);
            SetFocus(more);
            assert_eq!(GetFocus(), more);
            test.post_keys(&[(WM_KEYDOWN, VK_ESCAPE)]);
            assert!(!ask(p));
            assert_eq!(GetFocus(), more, "focus returns to the opener");
            // A running action no longer dims ⋯ (its command waits): the
            // focus still comes back to it.
            SetFocus(more);
            (*p).busy = true;
            test.post_keys(&[(WM_KEYDOWN, VK_ESCAPE)]);
            assert!(!ask(p));
            assert_ne!(IsWindowEnabled(more), 0);
            assert_eq!(GetFocus(), more);
            (*p).busy = false;
            // An opener that stays disabled (End task while an action is
            // running): the page's table takes the focus instead of nobody.
            let _ = test._snapshots.send(MonitorSample {
                at: Instant::now(),
                manual_refresh: false,
                snapshot: Ok(crate::sampler::Snapshot {
                    processes: vec![Process {
                        pid: 4321,
                        parent_pid: 1,
                        name: "test.exe".into(),
                        created: 7,
                        cpu_percent: 1.0,
                        working_set: 1 << 20,
                        private_bytes: 1 << 20,
                        io_bytes_per_sec: 0.0,
                        gpu_percent: None,
                        network_bytes_per_sec: None,
                        threads: 1,
                        handles: 1,
                        ..Process::default()
                    }],
                    cpu_percent: 1.0,
                    memory_used: 1,
                    memory_total: 2,
                    sample_ms: 0.0,
                }),
                performance: None,
                process_gpu: Default::default(),
                process_network: Default::default(),
                resource_data: None,
                resource_files: Default::default(),
            });
            SendMessageW((*p).hwnd, SNAPSHOT_READY, 0, 0);
            preview_selection(p);
            let end = (*p).primary;
            assert_ne!(IsWindowEnabled(end), 0);
            SetFocus(end);
            (*p).busy = true;
            test.post_keys(&[(WM_KEYDOWN, VK_ESCAPE)]);
            assert!(!ask(p));
            assert_eq!(IsWindowEnabled(end), 0);
            assert_eq!(GetFocus(), (*p).list, "the table, not nobody");
            // update_buttons disabling the focused button hands focus on too.
            (*p).busy = false;
            update_buttons(p);
            SetFocus(end);
            (*p).busy = true;
            update_buttons(p);
            assert_eq!(GetFocus(), (*p).list);
            (*p).busy = false;
            update_buttons(p);
            pump();
        }
    }

    #[test]
    fn confirm_dialog_is_centered_over_a_scrim_with_the_reference_layout() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let spec = ConfirmSpec {
                title: "End svchost.exe (PID 4)?",
                body: "Unsaved work may be lost.",
                warn: Some("This is a Windows process."),
                action: "End task",
                cancel: "Cancel",
                danger: true,
            };
            let (scrim, dialog) = popup::stage_confirm(p, &spec).unwrap();
            popup::settle(scrim);
            popup::settle(dialog);
            let (bounds, opacity) = popup::geometry(dialog).unwrap();
            assert_eq!(opacity, 1.0);
            assert_eq!(bounds.right - bounds.left, 440);
            let mut client: RECT = zeroed();
            GetClientRect((*p).hwnd, &mut client);
            let mut origin = POINT { x: 0, y: 0 };
            ClientToScreen((*p).hwnd, &mut origin);
            assert!(
                (bounds.left - origin.x - (client.right - 440) / 2).abs() <= 1,
                "centred horizontally"
            );
            let (cover, _) = popup::geometry(scrim).unwrap();
            let mut window: RECT = zeroed();
            GetWindowRect((*p).hwnd, &mut window);
            assert!(cover.left >= window.left && cover.right <= window.right);
            assert!(cover.left <= origin.x && cover.bottom >= origin.y + client.bottom);
            // Scrim: fg @ 22 % premultiplied.
            let c = colors();
            let s = popup::pixel(scrim, 200, 200).unwrap();
            assert_eq!(s >> 24, (0.22f32 * 255.0).round() as u32);
            // Dialog: opaque surface body, rounded transparent corner, bg
            // footer, the danger button, the soft shadow below.
            let content = popup::surface_content(dialog).unwrap();
            let body = popup::pixel(dialog, content.left + 6, content.top + 12).unwrap();
            assert_eq!(body >> 24, 255);
            assert_eq!(rgb(body), c.surface);
            let corner = popup::pixel(dialog, content.left, content.top).unwrap();
            assert!(corner >> 24 < 255, "rounded corner");
            let foot = popup::pixel(dialog, content.left + 6, content.bottom - 6).unwrap();
            assert_eq!(rgb(foot), c.bg);
            let action = popup::pixel(dialog, content.right - 30, content.bottom - 40).unwrap();
            assert_eq!(rgb(action), c.danger);
            let shadow = popup::pixel(
                dialog,
                (content.left + content.right) / 2,
                content.bottom + 20,
            )
            .unwrap();
            assert!(
                shadow >> 24 > 0 && shadow >> 24 < 80,
                "soft shadow {shadow:08x}"
            );
            popup::destroy(dialog);
            popup::destroy(scrim);
        }
    }

    #[test]
    fn toasts_fade_in_hold_fade_out_and_close_themselves() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            // A hidden window keeps notices in the status bar.
            notify_success(p, "Copied to clipboard.".into());
            assert_eq!((*p).notice, "Copied to clipboard.");
            assert!(popup::current_toast((*p).hwnd).is_null());
            let toast = popup::show_toast(p, "Copied to clipboard.");
            assert!(!toast.is_null());
            assert_eq!(popup::current_toast((*p).hwnd), toast);
            let (r, _) = popup::geometry(toast).unwrap();
            let mut corner = POINT { x: 1200, y: 820 };
            ClientToScreen((*p).hwnd, &mut corner);
            assert_eq!((corner.x - r.right, corner.y - r.bottom), (16, 44));
            assert_eq!(r.bottom - r.top, 39, "padding 10 + 13 px line + 10");
            let now = Instant::now();
            popup::advance(toast, now + Duration::from_millis(400));
            assert_eq!(popup::geometry(toast).unwrap().1, 1.0, "faded in");
            popup::advance(toast, now + Duration::from_millis(1500));
            assert_eq!(popup::geometry(toast).unwrap().1, 1.0, "holds 2.6 s");
            popup::advance(toast, now + Duration::from_millis(3200));
            popup::advance(toast, now + Duration::from_millis(4000));
            assert_eq!(popup::geometry(toast).unwrap().1, 0.0, "faded out");
            pump();
            assert_eq!(IsWindow(toast), 0, "closed itself");
            // A new toast replaces the current one.
            let first = popup::show_toast(p, "One");
            let second = popup::show_toast(p, "Two");
            assert_eq!(IsWindow(first), 0);
            assert_eq!(popup::current_toast((*p).hwnd), second);
            popup::owner_moved((*p).hwnd);
            assert_eq!(IsWindow(second), 0);
        }
    }

    #[test]
    fn open_popups_repaint_in_the_new_palette() {
        let test = Harness::new();
        unsafe {
            let toast = popup::show_toast(test.p, "Copied to clipboard.");
            popup::settle(toast);
            let r = popup::surface_content(toast).unwrap();
            let (x, y) = (r.left + 2, (r.top + r.bottom) / 2);
            // theme_changed() repaints every popup with theme::colors();
            // explicit palettes keep the process-wide theme untouched.
            for palette in [theme::DARK_PALETTE, theme::LIGHT] {
                popup::repaint_with(toast, palette);
                assert_eq!(rgb(popup::pixel(toast, x, y).unwrap()), palette.fg);
            }
            popup::destroy(toast);
        }
    }

    #[test]
    fn popups_are_rounded_shadowed_and_fade_in() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let list = popup::open_list(
                p,
                menu(),
                MenuStyle::menu(96),
                Anchor::Point(POINT { x: 300, y: 300 }),
                None,
            );
            let (r, opacity) = popup::geometry(list).unwrap();
            assert!(opacity < 1.0 || anim::reduced_motion(), "fades in");
            assert_eq!(r.right - r.left, 200, "min-width 200");
            // 1 + 4 + 4 rows × 32 + separator 9 + 4 + 1.
            assert_eq!(r.bottom - r.top, 1 + 4 + 4 * 32 + 9 + 4 + 1);
            popup::settle(list);
            let content = popup::surface_content(list).unwrap();
            let c = colors();
            let inside = popup::pixel(list, content.left + 2, content.top + 40).unwrap();
            assert_eq!((inside >> 24, rgb(inside)), (255, c.surface));
            let border = popup::pixel(list, content.left, content.top + 40).unwrap();
            assert_eq!(rgb(border), c.border);
            assert!(popup::pixel(list, content.left, content.top).unwrap() >> 24 < 200);
            let below = popup::pixel(list, content.left + 100, content.bottom + 24).unwrap();
            assert!(below >> 24 > 0, "shadow below");
            assert_eq!(popup::pixel(list, 0, 0).unwrap() >> 24, 0);
            // The hot item cross-fades to fg_sel.
            popup::menu_set_hot(list, Some(0));
            popup::settle(list);
            let row = popup::row_rect(list, 0).unwrap();
            let hot = popup::pixel(
                list,
                content.left + (row.left - r.left) + 4,
                content.top + (row.top - r.top) + 16,
            )
            .unwrap();
            assert_eq!(rgb(hot), c.fg_sel);
            popup::close(list);
            popup::advance(list, Instant::now() + Duration::from_secs(1));
            pump();
            assert_eq!(IsWindow(list), 0, "closes itself after the exit fade");
        }
    }

    #[test]
    fn focus_rings_are_keyboard_only_and_drawn_outside() {
        let test = Harness::new();
        unsafe {
            let p = test.p;
            // A click hides the cues, keyboard navigation shows them.
            pointer_pressed((*p).primary);
            assert!(cues_hidden((*p).hwnd));
            SendMessageW(
                (*p).hwnd,
                WM_CHANGEUISTATE,
                (UIS_CLEAR | (UISF_HIDEFOCUS << 16)) as usize,
                0,
            );
            assert!(!cues_hidden((*p).primary), "cleared for the whole window");
            pointer_pressed((*p).primary);
            assert!(cues_hidden((*p).primary));
            // Tab through the dialog manager (the main loop's IsDialogMessage).
            let tab = MSG {
                hwnd: (*p).more,
                message: WM_KEYDOWN,
                wParam: VK_TAB as usize,
                ..zeroed()
            };
            IsDialogMessageW((*p).hwnd, &tab);
            assert!(!cues_hidden((*p).primary), "Tab shows the focus cues");
            // Any press seen by the main loop hides them again (the list,
            // a table, anything without a hook of its own).
            observe_input(&MSG {
                hwnd: (*p).list,
                message: WM_RBUTTONDOWN,
                ..zeroed()
            });
            assert!(cues_hidden((*p).primary), "a press anywhere hides them");
            IsDialogMessageW((*p).hwnd, &tab);
            assert!(!cues_hidden((*p).primary));
            pointer_pressed((*p).primary);
            assert!(ring_outside(p, (*p).primary) && ring_outside(p, (*p).rate));
            // Nav items and theme segments too (outline-offset 2px); only
            // the switches keep a ring inside their own margin.
            assert!(ring_outside(p, (*p).nav[0]) && ring_outside(p, (*p).settings));
            let segment = GetDlgItem((*p).hwnd, THEME_DARK as i32);
            assert!(ring_outside(p, segment));
            assert!(!ring_outside(p, GetDlgItem((*p).hwnd, PREF_TOP as i32)));
            // Nav items and segments have no pressed translation.
            assert!(!translates(NAV) && !translates(THEME_DARK) && translates(PRIMARY));
            // The ring sits 2..4 px outside the control, in the main window.
            let mut frame = gfx::Dib::new(1200, 820).unwrap();
            layout(p);
            let r = child_rect(p, (*p).more);
            preview_focus(p, Some((*p).more));
            paint::paint_to(p, frame.dc());
            let y = (r.top + r.bottom) / 2;
            assert_eq!(rgb(frame.pixel(r.right + 2, y)), colors().fg);
            assert_eq!(rgb(frame.pixel(r.right + 3, y)), colors().fg);
            assert_ne!(rgb(frame.pixel(r.right, y)), colors().fg, "2 px gap");
            preview_focus(p, None);
            paint::paint_to(p, frame.dc());
            assert_ne!(
                rgb(frame.pixel(r.right + 2, y)),
                colors().fg,
                "no focus, no ring"
            );
        }
    }

    /// Opening and closing every popup kind 100 times leaves no GDI, USER
    /// or GDI+ objects and no windows behind (measured alone in a child
    /// test process, like `repainting_does_not_leak_gdi_or_user_objects`).
    #[test]
    fn popups_do_not_leak_gdi_or_user_objects() {
        const CHILD: &str = "FEATHER_POPUP_LEAK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ui::controls::tests::popups_do_not_leak_gdi_or_user_objects",
                    "--test-threads=1",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(output.status.success(), "{stdout}\n{stderr}");
            assert!(
                stdout.contains("popup-leak-check: ok"),
                "{stdout}\n{stderr}"
            );
            return;
        }
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS,
        };
        let test = Harness::new();
        unsafe {
            let p = test.p;
            let native = CreatePopupMenu();
            for (id, label) in [(61, "First\tDel"), (62, "Second")] {
                AppendMenuW(native, MF_STRING, id, wide(label).as_ptr());
            }
            let spec = ConfirmSpec {
                title: "End test.exe?",
                body: "Unsaved work may be lost.",
                warn: Some("This is a Windows process."),
                action: "End task",
                cancel: "Cancel",
                danger: true,
            };
            let cycle = |i: usize| {
                // Select dropdown: open, hover fade, close (fade + WM_CLOSE).
                SendMessageW((*p).view_mode, CB_SHOWDROPDOWN, 1, 0);
                SendMessageW((*p).view_mode, WM_KEYDOWN, VK_DOWN as usize, 0);
                SendMessageW((*p).view_mode, WM_KEYDOWN, VK_ESCAPE as usize, 0);
                // A menu with its submenu, faded out.
                let anchor = Anchor::Point(POINT { x: 300, y: 300 });
                let mut session = popup::MenuSession::open(p, menu(), anchor, None).unwrap();
                session.set_hot(0, Some(3));
                session.open_submenu(0, true);
                session.key(VK_DOWN);
                session.close();
                // The modal menu and the modal dialog.
                test.post_keys(&[(WM_KEYDOWN, VK_DOWN), (WM_KEYDOWN, VK_ESCAPE)]);
                popup::track_menu(p, native, anchor);
                test.post_keys(&[(WM_KEYDOWN, VK_TAB), (WM_KEYDOWN, VK_ESCAPE)]);
                popup::confirm_dialog(p, &spec);
                // A toast (replaced every other cycle, else run to its end).
                popup::show_toast(
                    p,
                    if i.is_multiple_of(2) {
                        "Copied."
                    } else {
                        "Sent."
                    },
                );
                popup::finish_closing();
                pump();
                popup::finish_closing();
                pump();
            };
            for i in 0..4 {
                cycle(i);
            }
            let process = GetCurrentProcess();
            let gdi = GetGuiResources(process, GR_GDIOBJECTS);
            let user = GetGuiResources(process, GR_USEROBJECTS);
            let gdiplus = gfx::live_objects();
            for i in 0..100 {
                cycle(i);
            }
            let gdi_after = GetGuiResources(process, GR_GDIOBJECTS);
            let user_after = GetGuiResources(process, GR_USEROBJECTS);
            assert!(
                gdi_after <= gdi + 2,
                "GDI objects grew from {gdi} to {gdi_after}"
            );
            assert!(
                user_after <= user + 2,
                "USER objects grew from {user} to {user_after}"
            );
            assert_eq!(gfx::live_objects(), gdiplus, "GDI+ objects leaked");
            assert!(popup::thread_popups().is_empty(), "popup windows left over");
            DestroyMenu(native);
            println!("popup-leak-check: ok gdi {gdi}->{gdi_after} user {user}->{user_after} gdiplus {gdiplus}");
        }
    }
}
