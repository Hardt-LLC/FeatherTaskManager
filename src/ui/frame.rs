//! Custom window frame (DESIGN_SPEC §3): the 44 px title strip *is* the
//! window caption, one bar with the brand, the search box and the caption
//! buttons, on Windows 10 and 11.
//!
//! * `WM_NCCALCSIZE` removes the native caption and top frame and keeps the
//!   invisible left / right / bottom resize borders (SM_CXFRAME +
//!   SM_CXPADDEDBORDER at the window's DPI). On Windows 11 the top keeps only
//!   DWM's visible 1 px border row, so the whole 44 px strip shows below the
//!   window border like the reference's `.window { border: 1px }`.
//!   Maximized, all four sides are inset so nothing lies off-monitor, and an
//!   auto-hide taskbar keeps a 2 px strip to be revealed through.
//! * `WM_NCHITTEST` ([`hit_test`], pure): top resize band + corners (not
//!   when maximized), the caption buttons as HTMINBUTTON / HTMAXBUTTON
//!   (Windows 11 Snap Layouts) / HTCLOSE, the search box as HTCLIENT and
//!   every other title-strip pixel (brand included) as HTCAPTION, so drag,
//!   double-click maximize, drag-to-restore and Aero Snap stay native.
//! * The caption buttons (46 × 44, `widgets::caption_button`) are painted in
//!   the title strip; hover cross-fades run on the main window's AnimHost
//!   (`(anim::CAPTION_ID + n, part::CAPTION)`). Non-client button clicks are
//!   intercepted, so DefWindowProc never paints classic buttons; a click acts
//!   on button-up over the same button.
//! * DWM: immersive dark mode follows the app theme, Windows 11 round corners
//!   and the border token as border color; on Windows 10 the frame is
//!   extended by the 1 px top border (painted black = transparent there) so
//!   the native top edge stays.
//! * The search Edit stays native (caret, IME): typed Hangul is drawn with a
//!   same-size Malgun Gothic in English mode (GDI font linking would shrink
//!   it), see [`update_search_font`].
use super::widgets::{self, Caption, Painter};
use super::*;
use windows_sys::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_BORDER_COLOR,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_VISIBLE_FRAME_BORDER_THICKNESS,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows_sys::Win32::UI::Shell::{
    SHAppBarMessage, ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETAUTOHIDEBAREX, ABM_GETSTATE,
    ABS_AUTOHIDE, APPBARDATA,
};

/// Undocumented uxtheme messages that paint the classic caption / frame
/// when the window text or icon changes; the frame is custom, so swallow them.
const WM_NCUAHDRAWCAPTION: u32 = 0x00AE;
const WM_NCUAHDRAWFRAME: u32 = 0x00AF;
/// DWMWA_USE_IMMERSIVE_DARK_MODE before Windows 10 20H1.
const DWMWA_USE_IMMERSIVE_DARK_MODE_OLD: u32 = 19;
/// Subclass id of the search Edit's Hangul font hook.
const SEARCH_FONT_SUBCLASS: usize = 0xF4A3;
/// PRIMARYLANGID of a Korean keyboard layout.
const LANG_KOREAN: usize = 0x12;
/// MSAA states reported in WM_GETTITLEBARINFOEX.
const STATE_PRESSED: u32 = 0x8;
const STATE_HOTTRACKED: u32 = 0x80;
const STATE_INVISIBLE: u32 = 0x8000;
const STATE_FOCUSABLE: u32 = 0x10_0000;
/// Resize corners reach this far along the edges (DIP).
pub(super) const RESIZE_CORNER: f32 = 16.0;
/// Maximized next to an auto-hide taskbar: leave this many device px free
/// so the mouse can reveal it (a window covering the whole monitor edge is
/// treated as full screen and the taskbar would stay hidden).
pub(super) const AUTOHIDE_STRIP: i32 = 2;
/// Minimum usable client size (DIP): the layout's supported minimum.
pub(super) const MIN_CLIENT: (f32, f32) = (980.0, 660.0);

/// The three caption buttons, left to right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Button {
    Minimize,
    Maximize,
    Close,
}

impl Button {
    pub(super) const ALL: [Button; 3] = [Button::Minimize, Button::Maximize, Button::Close];
    pub(super) fn index(self) -> usize {
        self as usize
    }
    /// The WM_NCHITTEST code of the button.
    pub(super) fn hit(self) -> u32 {
        match self {
            Button::Minimize => HTMINBUTTON,
            Button::Maximize => HTMAXBUTTON,
            Button::Close => HTCLOSE,
        }
    }
    pub(super) fn from_hit(code: u32) -> Option<Button> {
        match code {
            HTMINBUTTON => Some(Button::Minimize),
            HTMAXBUTTON => Some(Button::Maximize),
            HTCLOSE => Some(Button::Close),
            _ => None,
        }
    }
    /// Hover key on the main window's AnimHost.
    pub(super) fn key(self) -> (usize, u32) {
        (anim::CAPTION_ID + self.index(), anim::part::CAPTION)
    }
}

/// Frame state of the main window (`App.frame`).
pub(super) struct Frame {
    /// Caption button under the mouse (while pressed: only when over it).
    hot: Option<Button>,
    /// Caption button held down (mouse captured until button-up).
    pressed: Option<Button>,
    /// TME_LEAVE | TME_NONCLIENT is armed.
    tracking: bool,
    /// TME_LEAVE (client) is armed for the minimize / close buttons.
    client_tracking: bool,
    /// The app is active (WM_ACTIVATEAPP / WM_NCACTIVATE): otherwise the
    /// caption glyphs and brand are muted. The app's own popups (palette,
    /// dialogs) keep it active, like the reference's in-window overlays.
    active: bool,
    /// Last maximized state seen (WM_SIZE), to drop stale hover on changes.
    maximized: bool,
    /// Windows 11 DWM (round corners + border color available).
    win11: bool,
    /// Windows 10: rows of the 1 px DWM top border extended into the client
    /// (painted black, i.e. transparent to DWM); 0 on Windows 11 / maximized.
    top_border: i32,
    /// Windows 11: non-client rows kept at the top for DWM's visible border
    /// (DWMWA_VISIBLE_FRAME_BORDER_THICKNESS, 1 px); 0 elsewhere / maximized.
    top_frame: i32,
    margins: Option<i32>,
    /// Previews: paint as maximized (restore glyph) without showing the window.
    preview_maximized: Option<bool>,
    /// Malgun Gothic body font for Hangul in the English-mode search Edit.
    hangul_font: HFONT,
    hangul_dpi: i32,
    /// The search Edit currently uses `hangul_font`.
    search_hangul: bool,
    /// A Korean IME composition is running in the search Edit.
    composing: bool,
}

impl Frame {
    pub(super) const fn new() -> Self {
        Self {
            hot: None,
            pressed: None,
            tracking: false,
            client_tracking: false,
            active: true,
            maximized: false,
            win11: false,
            top_border: 0,
            top_frame: 0,
            margins: None,
            preview_maximized: None,
            hangul_font: null_mut(),
            hangul_dpi: 0,
            search_hangul: false,
            composing: false,
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        if !self.hangul_font.is_null() {
            unsafe {
                DeleteObject(self.hangul_font);
            }
        }
    }
}

// ───────────────────────────── geometry (pure) ─────────────────────────────

/// The invisible resize border of a WS_THICKFRAME window at `dpi`:
/// (left/right, bottom) = SM_CXFRAME / SM_CYFRAME + SM_CXPADDEDBORDER.
pub(super) fn resize_border(dpi: u32) -> (i32, i32) {
    unsafe {
        let dpi = dpi.max(96);
        let pad = GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        (
            GetSystemMetricsForDpi(SM_CXFRAME, dpi) + pad,
            GetSystemMetricsForDpi(SM_CYFRAME, dpi) + pad,
        )
    }
}

/// Window size for a `width` × `height` client at `dpi` (the frame has no
/// caption: only the left / right / bottom resize borders and `top` rows of
/// DWM border are outside it).
pub(super) fn outer_size(dpi: u32, width: i32, height: i32, top: i32) -> (i32, i32) {
    let (fx, fy) = resize_border(dpi);
    (width + 2 * fx, height + fy + top)
}

/// WM_NCCALCSIZE: the client rect for a proposed `window` rect. `border` is
/// the resize border ([`resize_border`]) and `top` the non-client rows kept
/// for DWM's top border (restored windows). Maximized windows overhang the
/// monitor by the border on every side, so the top is inset by it too and
/// the result is clamped to the monitor's `work` area; `autohide` (left,
/// top, right, bottom) keeps [`AUTOHIDE_STRIP`] px free on edges with an
/// auto-hide taskbar.
pub(super) fn client_area(
    window: RECT,
    border: (i32, i32),
    top: i32,
    maximized: bool,
    work: Option<RECT>,
    autohide: [bool; 4],
) -> RECT {
    let (fx, fy) = border;
    let mut r = RECT {
        left: window.left + fx,
        top: window.top + if maximized { 0 } else { top },
        right: window.right - fx,
        bottom: window.bottom - fy,
    };
    if maximized {
        r.top += fy;
        if let Some(work) = work {
            r = RECT {
                left: r.left.max(work.left),
                top: r.top.max(work.top),
                right: r.right.min(work.right),
                bottom: r.bottom.min(work.bottom),
            };
        }
        let strip = AUTOHIDE_STRIP;
        if autohide[0] {
            r.left += strip;
        }
        if autohide[1] {
            r.top += strip;
        }
        if autohide[2] {
            r.right -= strip;
        }
        if autohide[3] {
            r.bottom -= strip;
        }
    }
    r.right = r.right.max(r.left);
    r.bottom = r.bottom.max(r.top);
    r
}

/// Everything WM_NCHITTEST needs, in client coordinates (device px).
#[derive(Clone, Copy)]
pub(super) struct HitMap {
    pub width: i32,
    pub height: i32,
    /// Top resize band inside the client (the frame has no top border).
    pub top_band: i32,
    /// Corner length along the edges.
    pub corner: i32,
    /// False when maximized: no resize bands, nothing outside the client.
    pub resizable: bool,
    pub titlebar_bottom: i32,
    pub buttons: [RECT; 3],
    pub search: RECT,
}

impl HitMap {
    /// From the window layout; `top_band` is the resize border height.
    pub(super) fn new(l: &layout::Layout, top_band: i32, maximized: bool) -> Self {
        Self {
            width: l.client.right,
            height: l.client.bottom,
            top_band: if maximized { 0 } else { top_band },
            corner: l.px(RESIZE_CORNER),
            resizable: !maximized,
            titlebar_bottom: l.titlebar.bottom,
            buttons: l.caption_buttons(),
            search: l.search,
        }
    }
}

fn contains(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

/// WM_NCHITTEST for a point in client coordinates (outside the client =
/// the invisible resize borders).
pub(super) fn hit_test(m: &HitMap, x: i32, y: i32) -> u32 {
    if m.resizable {
        let (left, right, top, bottom) = (x < 0, x >= m.width, y < m.top_band, y >= m.height);
        let near_left = x < m.corner;
        let near_right = x >= m.width - m.corner;
        let near_top = y < m.corner;
        let near_bottom = y >= m.height - m.corner;
        if (top && near_left) || (left && near_top) {
            return HTTOPLEFT;
        }
        if (top && near_right) || (right && near_top) {
            return HTTOPRIGHT;
        }
        if (bottom && near_left) || (left && near_bottom) {
            return HTBOTTOMLEFT;
        }
        if (bottom && near_right) || (right && near_bottom) {
            return HTBOTTOMRIGHT;
        }
        if left {
            return HTLEFT;
        }
        if right {
            return HTRIGHT;
        }
        if bottom {
            return HTBOTTOM;
        }
        if top {
            return HTTOP;
        }
    } else if x < 0 || y < 0 || x >= m.width || y >= m.height {
        return HTNOWHERE;
    }
    if y < m.titlebar_bottom {
        for (button, r) in Button::ALL.iter().zip(m.buttons.iter()) {
            if contains(r, x, y) {
                return button.hit();
            }
        }
        if contains(&m.search, x, y) {
            return HTCLIENT;
        }
        return HTCAPTION;
    }
    HTCLIENT
}

// ───────────────────────────── window glue ─────────────────────────────

unsafe fn window_dpi(hwnd: HWND) -> u32 {
    GetDpiForWindow(hwnd).max(96)
}

unsafe fn hit_map(p: *mut App) -> HitMap {
    let l = current_layout(p);
    let (_, fy) = resize_border(window_dpi((*p).hwnd));
    // The top resize zone is as tall as the other borders, counting DWM's
    // non-client border row above the client.
    let band = (fy - (*p).frame.top_frame).max(1);
    HitMap::new(&l, band, IsZoomed((*p).hwnd) != 0)
}

fn point_from(l: LPARAM) -> (i32, i32) {
    (
        (l & 0xffff) as u16 as i16 as i32,
        ((l >> 16) & 0xffff) as u16 as i16 as i32,
    )
}

/// The monitor work area and auto-hide taskbar edges for a maximized window.
pub(super) unsafe fn maximized_bounds(window: RECT) -> (Option<RECT>, [bool; 4]) {
    let monitor = MonitorFromRect(&window, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..zeroed()
    };
    if monitor.is_null() || GetMonitorInfoW(monitor, &mut info) == 0 {
        return (None, [false; 4]);
    }
    let mut autohide = [false; 4];
    let mut state = APPBARDATA {
        cbSize: size_of::<APPBARDATA>() as u32,
        ..zeroed()
    };
    if SHAppBarMessage(ABM_GETSTATE, &mut state) as u32 & ABS_AUTOHIDE != 0 {
        for (slot, edge) in autohide
            .iter_mut()
            .zip([ABE_LEFT, ABE_TOP, ABE_RIGHT, ABE_BOTTOM])
        {
            let mut bar = APPBARDATA {
                cbSize: size_of::<APPBARDATA>() as u32,
                uEdge: edge,
                rc: info.rcMonitor,
                ..zeroed()
            };
            *slot = SHAppBarMessage(ABM_GETAUTOHIDEBAREX, &mut bar) != 0;
        }
    }
    (Some(info.rcWork), autohide)
}

/// Windows 11 DWM draws a visible border of this many px (1) around the
/// window; 0 where the attribute does not exist (Windows 10).
pub(super) unsafe fn visible_border(hwnd: HWND) -> i32 {
    let mut thickness: u32 = 0;
    let ok = DwmGetWindowAttribute(
        hwnd,
        DWMWA_VISIBLE_FRAME_BORDER_THICKNESS as u32,
        (&mut thickness as *mut u32).cast(),
        4,
    ) == 0;
    if ok {
        thickness.min(4) as i32
    } else {
        0
    }
}

unsafe fn nc_calc_size(p: *mut App, hwnd: HWND, w: WPARAM, l: LPARAM) -> LRESULT {
    if IsIconic(hwnd) != 0 || l == 0 {
        return DefWindowProcW(hwnd, WM_NCCALCSIZE, w, l);
    }
    let r: &mut RECT = if w != 0 {
        &mut (*(l as *mut NCCALCSIZE_PARAMS)).rgrc[0]
    } else {
        &mut *(l as *mut RECT)
    };
    let maximized = IsZoomed(hwnd) != 0;
    let (work, autohide) = if maximized {
        maximized_bounds(*r)
    } else {
        (None, [false; 4])
    };
    // Only with Windows 11's DWM border (probed in `apply_theme`); Windows 10
    // extends the frame into the client instead (`apply_margins`).
    let top = if (*p).frame.win11 {
        visible_border(hwnd)
    } else {
        0
    };
    (*p).frame.top_frame = if maximized { 0 } else { top };
    *r = client_area(
        *r,
        resize_border(window_dpi(hwnd)),
        top,
        maximized,
        work,
        autohide,
    );
    0
}

/// Frame messages of the main window. `Some(result)` = handled; `None` =
/// continue with the window procedure (some messages are only observed).
pub(super) unsafe fn handle(
    p: *mut App,
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_NCCALCSIZE => Some(nc_calc_size(p, hwnd, w, l)),
        WM_NCHITTEST => {
            let (x, y) = point_from(l);
            let mut point = POINT { x, y };
            ScreenToClient(hwnd, &mut point);
            // Minimize and close are client area to the system: over
            // HTMINBUTTON / HTCLOSE Windows shows its classic caption
            // tooltips (pale yellow, in the OS language) whatever the window
            // does with the messages. Only maximize stays HTMAXBUTTON, for
            // Windows 11's Snap Layouts flyout. The client handlers below
            // track, press and click them like the non-client path.
            let hit = hit_test(&hit_map(p), point.x, point.y);
            Some(if matches!(hit, HTMINBUTTON | HTCLOSE) {
                HTCLIENT
            } else {
                hit
            } as LRESULT)
        }
        WM_NCMOUSEMOVE => {
            if (*p).frame.pressed.is_none() {
                set_hot(p, Button::from_hit(w as u32));
            }
            if !(*p).frame.tracking {
                let mut event = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE | TME_NONCLIENT,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                (*p).frame.tracking = TrackMouseEvent(&mut event) != 0;
            }
            // DefWindowProc keeps Windows 11's Snap Layouts flyout over
            // HTMAXBUTTON. Over minimize and close it would only add the
            // classic caption tooltips (pale yellow, in the OS language) the
            // reference does not have.
            if matches!(w as u32, HTMINBUTTON | HTCLOSE) {
                return Some(0);
            }
            None
        }
        WM_NCMOUSEHOVER if matches!(w as u32, HTMINBUTTON | HTCLOSE) => Some(0),
        WM_NCMOUSELEAVE => {
            (*p).frame.tracking = false;
            // From maximize onto close / minimize (client area now): that
            // button keeps the hover it just got.
            let mut at: POINT = zeroed();
            GetCursorPos(&mut at);
            ScreenToClient(hwnd, &mut at);
            let onto = client_button(p, at.x, at.y);
            if (*p).frame.pressed.is_none() && (onto.is_none() || onto != (*p).frame.hot) {
                set_hot(p, None);
            }
            None
        }
        WM_MOUSEMOVE => {
            let (x, y) = point_from(l);
            if let Some(button) = (*p).frame.pressed {
                // Captured: the button shows pressed only while the mouse is over it.
                let over = Button::from_hit(hit_test(&hit_map(p), x, y)) == Some(button);
                set_hot(p, over.then_some(button));
                return Some(0);
            }
            // Minimize / close hover (client area, see WM_NCHITTEST).
            let over = client_button(p, x, y);
            if over.is_some() && !(*p).frame.client_tracking {
                let mut event = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                (*p).frame.client_tracking = TrackMouseEvent(&mut event) != 0;
            }
            if (*p).frame.hot != over {
                set_hot(p, over);
            }
            None
        }
        WM_MOUSELEAVE => {
            (*p).frame.client_tracking = false;
            if (*p).frame.pressed.is_none() && client_hot(p) {
                set_hot(p, None);
            }
            None
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK
            if client_button(p, point_from(l).0, point_from(l).1).is_some() =>
        {
            let (x, y) = point_from(l);
            let button = client_button(p, x, y)?;
            (*p).frame.pressed = Some(button);
            set_hot(p, Some(button));
            invalidate_button(p, button);
            SetCapture(hwnd);
            Some(0)
        }
        WM_RBUTTONDOWN if client_button(p, point_from(l).0, point_from(l).1).is_some() => Some(0),
        WM_RBUTTONUP if client_button(p, point_from(l).0, point_from(l).1).is_some() => {
            let (x, y) = point_from(l);
            let mut at = POINT { x, y };
            ClientToScreen(hwnd, &mut at);
            system_menu(hwnd, at.x, at.y, false);
            Some(0)
        }
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => {
            let button = Button::from_hit(w as u32)?;
            (*p).frame.pressed = Some(button);
            set_hot(p, Some(button));
            invalidate_button(p, button);
            SetCapture(hwnd);
            Some(0)
        }
        WM_NCLBUTTONUP => {
            let button = Button::from_hit(w as u32)?;
            // Only reached without capture (e.g. it was taken away).
            if (*p).frame.pressed == Some(button) {
                release(p, Some(button));
            }
            Some(0)
        }
        WM_LBUTTONUP if (*p).frame.pressed.is_some() => {
            let (x, y) = point_from(l);
            let over = Button::from_hit(hit_test(&hit_map(p), x, y));
            release(p, over);
            Some(0)
        }
        WM_CAPTURECHANGED => {
            if let Some(button) = (*p).frame.pressed.take() {
                invalidate_button(p, button);
                set_hot(p, None);
            }
            None
        }
        WM_NCRBUTTONDOWN if w as u32 == HTCAPTION || Button::from_hit(w as u32).is_some() => {
            Some(0)
        }
        WM_NCRBUTTONUP if w as u32 == HTCAPTION || Button::from_hit(w as u32).is_some() => {
            let (x, y) = point_from(l);
            system_menu(hwnd, x, y, false);
            Some(0)
        }
        WM_SYSCOMMAND if (w & 0xfff0) as u32 == SC_KEYMENU && l == VK_SPACE as isize => {
            // Alt+Space: DefWindowProc would place the menu below a native
            // caption (SM_CYCAPTION), over the strip; open it below the strip.
            let mut at = keyboard_menu_point(&current_layout(p));
            ClientToScreen(hwnd, &mut at);
            system_menu(hwnd, at.x, at.y, true);
            Some(0)
        }
        WM_SYSCOMMAND if (w & 0xfff0) as u32 == SC_KEYMENU && l == 0 => {
            // Alt / F10 alone: without a menu bar DefWindowProc would enter
            // an invisible system-menu mode that swallows the next key (and
            // Down would drop the menu over the strip). Like Chromium's
            // frame, ignore it; Alt+Space opens the menu.
            Some(0)
        }
        // The caption buttons are the reference's `<button>`s: pointer.
        WM_SETCURSOR
            if w as HWND == hwnd
                && matches!((l & 0xffff) as u32, HTMINBUTTON | HTMAXBUTTON | HTCLOSE) =>
        {
            Some(widgets::set_pointer(true))
        }
        WM_SETCURSOR
            if w as HWND == hwnd && (l & 0xffff) as u32 == HTCLIENT && {
                let mut at: POINT = zeroed();
                GetCursorPos(&mut at);
                ScreenToClient(hwnd, &mut at);
                client_button(p, at.x, at.y).is_some()
            } =>
        {
            Some(widgets::set_pointer(true))
        }
        WM_SETCURSOR
            if w as HWND == hwnd
                && (l & 0xffff) as u32 == HTCLIENT
                && IsWindowEnabled((*p).search) != 0 =>
        {
            // The whole search box is the input (its padding focuses it too).
            let mut at: POINT = zeroed();
            GetCursorPos(&mut at);
            ScreenToClient(hwnd, &mut at);
            if !contains(&current_layout(p).search, at.x, at.y) {
                return None;
            }
            SetCursor(LoadCursorW(null_mut(), IDC_IBEAM));
            Some(1)
        }
        WM_LBUTTONDOWN => {
            // The search box padding (icon, `Ctrl K`) focuses the input.
            let (x, y) = point_from(l);
            let l = current_layout(p);
            if contains(&l.search, x, y) && IsWindowEnabled((*p).search) != 0 {
                SetFocus((*p).search);
                let end = GetWindowTextLengthW((*p).search).max(0) as usize;
                SendMessageW((*p).search, EM_SETSEL, end, end as isize);
                return Some(0);
            }
            None
        }
        WM_NCACTIVATE => {
            // Deactivation in favour of one of the app's own windows (the
            // palette) keeps the caption active; WM_ACTIVATEAPP mutes it
            // when another application takes over.
            if w != 0 {
                set_active(p, true);
            }
            // -1: DefWindowProc must not repaint a (classic) non-client caption.
            Some(DefWindowProcW(hwnd, msg, w, -1))
        }
        WM_ACTIVATEAPP => {
            set_active(p, w != 0);
            None
        }
        WM_ACTIVATE | WM_DWMCOMPOSITIONCHANGED => {
            if msg == WM_DWMCOMPOSITIONCHANGED {
                (*p).frame.margins = None;
                apply_theme(p);
            } else {
                apply_margins(p);
            }
            None
        }
        WM_SIZE => {
            let maximized = w == SIZE_MAXIMIZED as usize;
            if w != SIZE_MINIMIZED as usize {
                if maximized != (*p).frame.maximized {
                    (*p).frame.maximized = maximized;
                    // The buttons moved and the max glyph changed: no stale
                    // hover, and fades still running end at once.
                    (*p).frame.hot = None;
                    settle_buttons(p);
                }
                // Fades that keep running repaint the buttons where they are now.
                register_buttons(p);
            }
            apply_margins(p);
            None
        }
        WM_DPICHANGED => {
            // The buttons are resized and moved: settle their fades.
            settle_buttons(p);
            None
        }
        WM_SETTINGCHANGE if w as u32 == SPI_SETWORKAREA && IsZoomed(hwnd) != 0 => {
            // The taskbar moved or its auto-hide changed: recompute the insets.
            SetWindowPos(
                hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            None
        }
        WM_GETDPISCALEDSIZE if IsZoomed(hwnd) == 0 && IsIconic(hwnd) == 0 && l != 0 => {
            // Scale the client, not the (captionless) window: the default
            // would add a native caption's height on every monitor change.
            let (old, new) = (window_dpi(hwnd) as i64, (w as u32).max(96) as i64);
            let mut client: RECT = zeroed();
            GetClientRect(hwnd, &mut client);
            let scale = |v: i32| ((v as i64 * new + old / 2) / old) as i32;
            let (width, height) = outer_size(
                new as u32,
                scale(client.right),
                scale(client.bottom),
                (*p).frame.top_frame,
            );
            let size = &mut *(l as *mut SIZE);
            size.cx = width;
            size.cy = height;
            Some(1)
        }
        WM_GETTITLEBARINFOEX if l != 0 => {
            // Accessibility and the shell (Snap Layouts flyout placement)
            // ask where the caption buttons are: the painted ones.
            title_bar_info(p, &mut *(l as *mut TITLEBARINFOEX));
            Some(0)
        }
        WM_NCUAHDRAWCAPTION | WM_NCUAHDRAWFRAME => Some(0),
        _ => None,
    }
}

/// WM_GETTITLEBARINFOEX: the strip and the caption buttons in screen
/// coordinates with their hover / pressed states (help and the reserved
/// element are invisible).
unsafe fn title_bar_info(p: *mut App, info: &mut TITLEBARINFOEX) {
    let hwnd = (*p).hwnd;
    let l = current_layout(p);
    let screen = |r: RECT| {
        let mut a = POINT {
            x: r.left,
            y: r.top,
        };
        let mut b = POINT {
            x: r.right,
            y: r.bottom,
        };
        ClientToScreen(hwnd, &mut a);
        ClientToScreen(hwnd, &mut b);
        RECT {
            left: a.x,
            top: a.y,
            right: b.x,
            bottom: b.y,
        }
    };
    info.rcTitleBar = screen(l.titlebar);
    info.rgstate = [STATE_FOCUSABLE, STATE_INVISIBLE, 0, 0, STATE_INVISIBLE, 0];
    info.rgrect = [RECT::default(); 6];
    let f = &(*p).frame;
    for (button, r) in Button::ALL.iter().zip(l.caption_buttons()) {
        let slot = match button {
            Button::Minimize => 2,
            Button::Maximize => 3,
            Button::Close => 5,
        };
        let mut state = 0;
        if f.hot == Some(*button) {
            state |= STATE_HOTTRACKED;
            if f.pressed == Some(*button) {
                state |= STATE_PRESSED;
            }
        }
        info.rgstate[slot] = state;
        info.rgrect[slot] = screen(r);
    }
}

/// WM_CREATE (after the child controls exist): DWM attributes, the search
/// Edit's font hook, and a frame recalculation at the real DPI.
pub(super) unsafe fn attach(p: *mut App) {
    let hwnd = (*p).hwnd;
    apply_theme(p);
    if !(*p).search.is_null() {
        SetWindowSubclass(
            (*p).search,
            Some(search_font_subclass),
            SEARCH_FONT_SUBCLASS,
            p as usize,
        );
    }
    SetWindowPos(
        hwnd,
        null_mut(),
        0,
        0,
        0,
        0,
        SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
    // Windows are created at `outer_size(.., top = 0)`: DWM's border row
    // above the strip (Windows 11) is added here, so the client keeps the
    // height it was created for.
    let (_, fy) = resize_border(window_dpi(hwnd));
    let mut window: RECT = zeroed();
    let mut client: RECT = zeroed();
    GetWindowRect(hwnd, &mut window);
    GetClientRect(hwnd, &mut client);
    let grow = window.bottom - window.top - fy - client.bottom;
    if grow > 0 && IsZoomed(hwnd) == 0 && IsIconic(hwnd) == 0 {
        SetWindowPos(
            hwnd,
            null_mut(),
            0,
            0,
            window.right - window.left,
            window.bottom - window.top + grow,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// Theme switch (and creation): DWM dark mode follows the app theme,
/// Windows 11 corners are round with the border token as border color.
pub(super) unsafe fn apply_theme(p: *mut App) {
    let hwnd = (*p).hwnd;
    if hwnd.is_null() {
        return;
    }
    let c = colors();
    let dark = c.dark as i32;
    let set = |attribute: u32, value: *const core::ffi::c_void| {
        DwmSetWindowAttribute(hwnd, attribute, value, 4)
    };
    if set(
        DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
        (&dark as *const i32).cast(),
    ) != 0
    {
        set(
            DWMWA_USE_IMMERSIVE_DARK_MODE_OLD,
            (&dark as *const i32).cast(),
        );
    }
    let corner = DWMWCP_ROUND;
    // Only Windows 11 knows the corner preference: it doubles as the probe.
    (*p).frame.win11 = set(
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        (&corner as *const i32).cast(),
    ) == 0;
    if (*p).frame.win11 {
        let border: u32 = c.border;
        set(DWMWA_BORDER_COLOR as u32, (&border as *const u32).cast());
    }
    apply_margins(p);
}

/// Windows 10 keeps its native 1 px top border by extending the frame 1 px
/// into the client (restored windows only); Windows 11 draws the border itself.
unsafe fn apply_margins(p: *mut App) {
    let hwnd = (*p).hwnd;
    let top = i32::from(!(*p).frame.win11 && IsZoomed(hwnd) == 0 && IsIconic(hwnd) == 0);
    if (*p).frame.margins == Some(top) {
        return;
    }
    let margins = MARGINS {
        cxLeftWidth: 0,
        cxRightWidth: 0,
        cyTopHeight: top,
        cyBottomHeight: 0,
    };
    // Without DWM the black row would stay black: paint it only when the
    // frame really extends under it.
    let top = if DwmExtendFrameIntoClientArea(hwnd, &margins) >= 0 {
        top
    } else {
        0
    };
    (*p).frame.margins = Some(top);
    if (*p).frame.top_border != top {
        (*p).frame.top_border = top;
        let strip = current_layout(p).titlebar;
        InvalidateRect(hwnd, &strip, 0);
    }
}

unsafe fn set_active(p: *mut App, active: bool) {
    if (*p).frame.active != active {
        (*p).frame.active = active;
        let strip = current_layout(p).titlebar;
        InvalidateRect((*p).hwnd, &strip, 0);
    }
}

/// Move hover to `hot` (cross-fade: in 100 ms, out 150 ms, ease-out).
/// The minimize or close button at a client point (the caption buttons the
/// system sees as client area, see WM_NCHITTEST).
unsafe fn client_button(p: *mut App, x: i32, y: i32) -> Option<Button> {
    Button::from_hit(hit_test(&hit_map(p), x, y)).filter(|b| *b != Button::Maximize)
}

/// The hot button is one of the client-area buttons (its leave comes as
/// WM_MOUSELEAVE, not WM_NCMOUSELEAVE).
unsafe fn client_hot(p: *mut App) -> bool {
    matches!((*p).frame.hot, Some(Button::Minimize | Button::Close))
}

unsafe fn set_hot(p: *mut App, hot: Option<Button>) {
    if (*p).frame.hot == hot {
        return;
    }
    let old = std::mem::replace(&mut (*p).frame.hot, hot);
    register_buttons(p);
    if let Some(button) = old {
        (*p).anim.set_target(
            button.key(),
            0.0,
            anim::motion::HOVER_OUT,
            anim::Easing::EaseOut,
        );
        invalidate_button(p, button);
    }
    if let Some(button) = hot {
        (*p).anim.set_target(
            button.key(),
            1.0,
            anim::motion::HOVER_IN,
            anim::Easing::EaseOut,
        );
        invalidate_button(p, button);
    }
}

/// The hover keys repaint the buttons' current rects (they move with the
/// window width, maximize / restore and DPI).
unsafe fn register_buttons(p: *mut App) {
    let rects = current_layout(p).caption_buttons();
    let hwnd = (*p).hwnd;
    for button in Button::ALL {
        (*p).anim
            .register(button.key(), hwnd, Some(rects[button.index()]));
    }
}

/// Jump every hover fade to where it is heading (the hot button lit).
unsafe fn settle_buttons(p: *mut App) {
    let hot = (*p).frame.hot;
    for button in Button::ALL {
        let target = if hot == Some(button) { 1.0 } else { 0.0 };
        if (*p).anim.anim.is_key_animating(button.key()) || (*p).anim.value(button.key()) != target
        {
            (*p).anim.set(button.key(), target);
        }
    }
}

unsafe fn invalidate_button(p: *mut App, button: Button) {
    let r = current_layout(p).caption_buttons()[button.index()];
    InvalidateRect((*p).hwnd, &r, 0);
}

/// Button-up: release the capture and act when released over the pressed button.
unsafe fn release(p: *mut App, over: Option<Button>) {
    let pressed = (*p).frame.pressed.take();
    if GetCapture() == (*p).hwnd {
        ReleaseCapture();
    }
    let Some(button) = pressed else {
        return;
    };
    invalidate_button(p, button);
    if over != Some(button) {
        set_hot(p, None);
        return;
    }
    let hwnd = (*p).hwnd;
    let command = match button {
        Button::Minimize => SC_MINIMIZE,
        Button::Maximize if IsZoomed(hwnd) != 0 => SC_RESTORE,
        Button::Maximize => SC_MAXIMIZE,
        Button::Close => SC_CLOSE,
    };
    if button != Button::Close {
        // The window changes state (and the buttons move): no stale hover.
        (*p).frame.hot = None;
        (*p).anim.set(button.key(), 0.0);
    }
    // Posted, like the native caption: the command runs after this message.
    PostMessageW(hwnd, WM_SYSCOMMAND, command as usize, 0);
}

/// Alt+Space: where the system menu drops down from the keyboard, the
/// strip's bottom-left corner (a native caption's menu opens right below it).
pub(super) fn keyboard_menu_point(l: &layout::Layout) -> POINT {
    POINT {
        x: l.client.left,
        y: l.titlebar.bottom,
    }
}

/// The window's system menu with its top-left corner at a screen point
/// (right-click on the strip: the cursor; Alt+Space: [`keyboard_menu_point`]).
/// From the keyboard the first item is highlighted, like DefWindowProc's.
unsafe fn system_menu(hwnd: HWND, x: i32, y: i32, keyboard: bool) {
    let menu = GetSystemMenu(hwnd, 0);
    if menu.is_null() {
        return;
    }
    let maximized = IsZoomed(hwnd) != 0;
    for (command, enabled) in [
        (SC_RESTORE, maximized),
        (SC_MOVE, !maximized),
        (SC_SIZE, !maximized),
        (SC_MINIMIZE, true),
        (SC_MAXIMIZE, !maximized),
        (SC_CLOSE, true),
    ] {
        EnableMenuItem(
            menu,
            command,
            MF_BYCOMMAND | if enabled { MF_ENABLED } else { MF_GRAYED },
        );
    }
    SetMenuDefaultItem(menu, SC_CLOSE, 0);
    if keyboard {
        // Picked up by the menu's modal loop: selects the first item.
        PostMessageW(hwnd, WM_KEYDOWN, VK_DOWN as usize, 0);
    }
    let command = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_LEFTALIGN | TPM_TOPALIGN,
        x,
        y,
        0,
        hwnd,
        null(),
    );
    if command != 0 {
        PostMessageW(hwnd, WM_SYSCOMMAND, command as usize, 0);
    }
}

// ───────────────────────────── painting ─────────────────────────────

/// Paint as maximized when previews ask for it (the preview window is never shown).
unsafe fn is_maximized(p: *mut App) -> bool {
    (*p).frame
        .preview_maximized
        .unwrap_or_else(|| IsZoomed((*p).hwnd) != 0)
}

/// Inactive windows show muted caption glyphs and brand (DESIGN_SPEC §3).
pub(super) unsafe fn is_active(p: *mut App) -> bool {
    (*p).frame.active
}

/// The caption buttons in the title strip (called by `paint::title_strip`)
/// and, on Windows 10, the transparent row that shows DWM's top border.
pub(super) unsafe fn paint(p: *mut App, pt: &Painter, l: &layout::Layout) {
    let f = &(*p).frame;
    let maximized = is_maximized(p);
    let rects = l.caption_buttons();
    for button in Button::ALL {
        let kind = match button {
            Button::Minimize => Caption::Minimize,
            Button::Maximize if maximized => Caption::Restore,
            Button::Maximize => Caption::Maximize,
            Button::Close => Caption::Close,
        };
        widgets::caption_button(
            pt,
            rects[button.index()],
            kind,
            (*p).anim.value(button.key()),
            f.pressed == Some(button) && f.hot == Some(button),
            f.active,
            pt.c.bg,
        );
    }
    if f.top_border > 0 && !maximized {
        // Black (alpha 0) inside the extended frame = DWM's own top border.
        pt.fill(
            RECT {
                left: 0,
                top: 0,
                right: l.client.right,
                bottom: f.top_border,
            },
            0,
        );
    }
}

/// WM_GETMINMAXINFO: the supported minimum client plus the resize borders.
pub(super) unsafe fn min_track_size(p: *mut App) -> POINT {
    let (fx, fy) = resize_border(window_dpi((*p).hwnd));
    POINT {
        x: gfx::pxi((*p).dpi, MIN_CLIENT.0) + 2 * fx,
        y: gfx::pxi((*p).dpi, MIN_CLIENT.1) + fy + (*p).frame.top_frame,
    }
}

// ───────────────────────────── search input font ─────────────────────────────

/// The font the search Edit uses now (layout sizes the Edit from it).
pub(super) unsafe fn search_font(p: *mut App) -> HFONT {
    if (*p).frame.search_hangul && !(*p).frame.hangul_font.is_null() {
        (*p).frame.hangul_font
    } else {
        (*p).fonts.body
    }
}

/// After `create_fonts` handed the Edit the new body font (DPI / language).
pub(super) unsafe fn refresh_search_font(p: *mut App) {
    (*p).frame.search_hangul = false;
    update_search_font(p);
}

unsafe fn has_hangul(hwnd: HWND) -> bool {
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 {
        return false;
    }
    let mut text = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, text.as_mut_ptr(), text.len() as i32).max(0) as usize;
    text[..copied.min(text.len())]
        .iter()
        .any(|&unit| fonts::is_hangul(unit))
}

/// The keyboard layout of this thread is Korean (Hangul IME).
unsafe fn korean_input() -> bool {
    GetKeyboardLayout(0) as usize & 0x3ff == LANG_KOREAN
}

/// The search Edit is a native EDIT (caret, IME, accessibility). In English
/// mode its body font is Segoe UI Variable, and GDI font linking draws Hangul
/// in it with a scaled-down Malgun Gothic (FOUNDATION_API §2); so while the
/// input holds Hangul, or a Korean IME composition runs, it switches to the
/// same-size Malgun Gothic body font (Korean mode already uses it). Latin
/// text keeps the spec font whenever there is no Hangul.
pub(super) unsafe fn update_search_font(p: *mut App) {
    let search = (*p).search;
    if search.is_null() || (*p).fonts.body.is_null() {
        return;
    }
    let want = language() != Language::Korean && ((*p).frame.composing || has_hangul(search));
    if want == (*p).frame.search_hangul {
        return;
    }
    let font = if want {
        let f = &mut (*p).frame;
        if f.hangul_font.is_null() || f.hangul_dpi != (*p).dpi {
            let old = std::mem::replace(
                &mut f.hangul_font,
                fonts::create(fonts::Role::Body, (*p).dpi, Language::Korean),
            );
            f.hangul_dpi = (*p).dpi;
            if !old.is_null() {
                DeleteObject(old);
            }
        }
        f.hangul_font
    } else {
        (*p).fonts.body
    };
    (*p).frame.search_hangul = want;
    SendMessageW(search, WM_SETFONT, font as usize, 1);
    // WM_SETFONT resets the margins; the text starts at the 34 px inset.
    SendMessageW(
        search,
        EM_SETMARGINS,
        (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize,
        0,
    );
    let dc = GetDC((*p).hwnd);
    if !dc.is_null() {
        let edit = current_layout(p).search_edit(line_height(dc, font));
        ReleaseDC((*p).hwnd, dc);
        place(p, search, edit);
    }
}

unsafe extern "system" fn search_font_subclass(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    let p = data as *mut App;
    if msg == WM_IME_STARTCOMPOSITION && korean_input() {
        // Switch before the first composed syllable is drawn.
        (*p).frame.composing = true;
        update_search_font(p);
    }
    let result = DefSubclassProc(hwnd, msg, w, l);
    match msg {
        WM_IME_ENDCOMPOSITION => {
            (*p).frame.composing = false;
            update_search_font(p);
        }
        WM_SETTEXT | WM_CHAR | WM_IME_CHAR | WM_IME_COMPOSITION | WM_PASTE | WM_CUT | WM_CLEAR
        | WM_UNDO | EM_UNDO | EM_REPLACESEL | WM_KEYDOWN => update_search_font(p),
        WM_NCDESTROY => {
            RemoveWindowSubclass(hwnd, Some(search_font_subclass), id);
        }
        _ => {}
    }
    result
}

// ───────────────────────────── previews ─────────────────────────────

/// Preview captures of the caption states (DESIGN_SPEC §8.3): hover on
/// minimize / maximize / close, pressed close, maximized (restore glyph) and
/// inactive, in light and dark (`caption-<state>-<theme>.bmp`, full client)
/// plus one sheet of the title strips per theme and at 150 %
/// (`titlebar-states-<theme>.bmp`, rows in the order of [`PREVIEW_STATES`]).
pub(super) unsafe fn save_previews(p: *mut App, dir: &std::path::Path) -> Result<(), String> {
    let theme = (*p).prefs.theme;
    let result = (|| {
        for (value, name) in [(1, "light"), (2, "dark")] {
            (*p).prefs.theme = value;
            interactions::apply_theme(p);
            let mut strips = Vec::new();
            for &(state, hot, pressed, maximized, active) in PREVIEW_STATES {
                stage(p, hot, pressed, maximized, active);
                if state != "rest" {
                    capture::save_client(p, &dir.join(format!("caption-{state}-{name}.bmp")))?;
                }
                strips.push((state, title_strip(p)?));
            }
            for (state, focused) in [("search-focused", true), ("search-hangul", false)] {
                strips.push((state, search_state(p, focused)?));
                if !focused {
                    capture::save_client(p, &dir.join(format!("{state}-{name}.bmp")))?;
                }
                end_search_state(p);
            }
            save_sheet(
                &mut strips,
                &dir.join(format!("titlebar-states-{name}.bmp")),
            )?;
        }
        // 150 %: the same states from the WM_DPICHANGED path.
        (*p).prefs.theme = 1;
        interactions::apply_theme(p);
        preview_dpi((*p).hwnd, 144, 1800, 1230);
        let mut strips = Vec::new();
        for &(state, hot, pressed, maximized, active) in PREVIEW_STATES {
            stage(p, hot, pressed, maximized, active);
            strips.push((state, title_strip(p)?));
        }
        strips.push(("search-hangul", search_state(p, false)?));
        capture::save_client(p, &dir.join("search-hangul-dpi150.bmp"))?;
        end_search_state(p);
        save_sheet(&mut strips, &dir.join("titlebar-states-dpi150.bmp"))?;
        preview_dpi((*p).hwnd, 96, 1200, 820);
        Ok(())
    })();
    stage(p, None, None, None, true);
    (*p).prefs.theme = theme;
    interactions::apply_theme(p);
    layout(p);
    result
}

/// (name, hovered, pressed, maximized, active)
type PreviewState = (
    &'static str,
    Option<Button>,
    Option<Button>,
    Option<bool>,
    bool,
);
const PREVIEW_STATES: &[PreviewState] = &[
    ("rest", None, None, None, true),
    ("hover-min", Some(Button::Minimize), None, None, true),
    ("hover-max", Some(Button::Maximize), None, None, true),
    ("hover-close", Some(Button::Close), None, None, true),
    (
        "pressed-close",
        Some(Button::Close),
        Some(Button::Close),
        None,
        true,
    ),
    ("maximized", None, None, Some(true), true),
    (
        "maximized-hover",
        Some(Button::Maximize),
        None,
        Some(true),
        true,
    ),
    ("inactive", None, None, None, false),
    (
        "inactive-hover-close",
        Some(Button::Close),
        None,
        None,
        false,
    ),
];

/// Stage a settled caption state for a capture (no mouse, no timer).
unsafe fn stage(
    p: *mut App,
    hot: Option<Button>,
    pressed: Option<Button>,
    maximized: Option<bool>,
    active: bool,
) {
    let f = &mut (*p).frame;
    f.hot = hot;
    f.pressed = pressed;
    f.preview_maximized = maximized;
    f.active = active;
    for button in Button::ALL {
        (*p).anim
            .set(button.key(), if hot == Some(button) { 1.0 } else { 0.0 });
    }
    redraw(p);
}

/// Typed text in the search box: focused (border fg, caret) or Hangul
/// mixed with Latin, which switches the input to its Hangul font.
unsafe fn search_state(p: *mut App, focused: bool) -> Result<gfx::Dib, String> {
    stage(p, None, None, None, true);
    let text = if focused { "svchost" } else { "크롬 chrome" };
    SetWindowTextW((*p).search, wide(text).as_ptr());
    if focused {
        SetFocus((*p).search);
    }
    redraw(p);
    title_strip(p)
}

unsafe fn end_search_state(p: *mut App) {
    SetWindowTextW((*p).search, wide("").as_ptr());
    if GetFocus() == (*p).search {
        SetFocus((*p).hwnd);
    }
    redraw(p);
}

/// The client's title strip (children included), settled.
unsafe fn title_strip(p: *mut App) -> Result<gfx::Dib, String> {
    (*p).anim.finish_all();
    let l = current_layout(p);
    let mut full = gfx::Dib::new(l.client.right, l.client.bottom).ok_or("Strip surface failed")?;
    capture::paint_client_and_children((*p).hwnd, full.dc())?;
    let (width, height) = (l.client.right, l.titlebar.bottom);
    let mut strip = gfx::Dib::new(width, height).ok_or("Strip surface failed")?;
    let source = full.pixels().to_vec();
    let target = strip.pixels();
    for y in 0..height as usize {
        let row = y * width as usize;
        target[row..row + width as usize].copy_from_slice(&source[row..row + width as usize]);
    }
    Ok(strip)
}

/// Title strips stacked with 6 px gaps and a label column on the left.
unsafe fn save_sheet(
    strips: &mut [(&str, gfx::Dib)],
    path: &std::path::Path,
) -> Result<(), String> {
    let label = 150;
    let gap = 6;
    let width = strips.iter().map(|(_, s)| s.width()).max().unwrap_or(0) + label;
    let height = strips.iter().map(|(_, s)| s.height() + gap).sum::<i32>() + gap;
    let mut sheet = gfx::Dib::new(width, height).ok_or("Sheet surface failed")?;
    let desk = colors().desk;
    let fonts = fonts::Fonts::new(96, language());
    {
        let pt = Painter::new(sheet.dc(), 96, &fonts);
        pt.fill(
            RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            },
            desk,
        );
        let mut y = gap;
        for (name, strip) in strips.iter() {
            pt.label(
                fonts.small,
                pt.c.fg,
                name,
                RECT {
                    left: 8,
                    top: y,
                    right: label - 8,
                    bottom: y + strip.height(),
                },
                DT_LEFT,
            );
            y += strip.height() + gap;
        }
    }
    let mut y = gap as usize;
    let stride = width as usize;
    for (_, strip) in strips.iter_mut() {
        let (w, h) = (strip.width() as usize, strip.height() as usize);
        let source = strip.pixels().to_vec();
        let target = sheet.pixels();
        for row in 0..h {
            let at = (y + row) * stride + label as usize;
            target[at..at + w].copy_from_slice(&source[row * w..row * w + w]);
        }
        y += h + gap as usize;
    }
    capture::save_dib(&mut sheet, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    fn t(r: RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right, r.bottom)
    }
    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn client_area_keeps_side_and_bottom_borders_and_drops_the_caption() {
        let window = rect(100, 50, 1316, 878);
        assert_eq!(
            t(client_area(window, (8, 8), 0, false, None, [false; 4])),
            (108, 50, 1308, 870)
        );
        // Windows 11 keeps DWM's 1 px border row above the strip.
        assert_eq!(
            t(client_area(window, (8, 8), 1, false, None, [false; 4])),
            (108, 51, 1308, 870)
        );
        // 150 %: 11 + 4 padded (frame metrics scale per DPI).
        assert_eq!(
            t(client_area(window, (15, 15), 0, false, None, [false; 4])),
            (115, 50, 1301, 863)
        );
    }

    #[test]
    fn maximized_client_is_inset_on_all_sides_and_clamped_to_the_work_area() {
        // Maximized windows overhang the monitor by the resize border.
        let window = rect(-8, -8, 1928, 1040 + 8);
        let work = rect(0, 0, 1920, 1040);
        assert_eq!(
            t(client_area(window, (8, 8), 1, true, Some(work), [false; 4])),
            (0, 0, 1920, 1040)
        );
        // A second monitor to the right: nothing reaches past its edges.
        let window = rect(1912, -8, 3848, 1088);
        let work = rect(1920, 0, 3840, 1080);
        assert_eq!(
            t(client_area(window, (8, 8), 1, true, Some(work), [false; 4])),
            (1920, 0, 3840, 1080)
        );
        // Auto-hide taskbar at the bottom: a 2 px strip reveals it.
        let window = rect(-8, -8, 1928, 1088);
        let work = rect(0, 0, 1920, 1080);
        assert_eq!(
            t(client_area(
                window,
                (8, 8),
                1,
                true,
                Some(work),
                [false, false, false, true]
            )),
            (0, 0, 1920, 1078)
        );
        assert_eq!(
            t(client_area(
                window,
                (8, 8),
                1,
                true,
                Some(work),
                [true, true, false, false]
            )),
            (2, 2, 1920, 1080)
        );
    }

    fn map(width: i32, height: i32, dpi: i32, maximized: bool) -> HitMap {
        let l = layout::Layout::new(width, height, dpi, Page::Processes);
        let band = resize_border(dpi as u32).1;
        HitMap::new(&l, band, maximized)
    }

    #[test]
    fn caption_buttons_tile_the_reserved_caption_area() {
        for (w, h, dpi) in [
            (1200, 820, 96),
            (1225, 1025, 120),
            (1800, 1230, 144),
            (2400, 1640, 192),
        ] {
            let l = layout::Layout::new(w, h, dpi, Page::Processes);
            let b = l.caption_buttons();
            assert_eq!(b[0].left, l.caption.left, "{dpi}");
            assert_eq!(b[0].right, b[1].left);
            assert_eq!(b[1].right, b[2].left);
            assert_eq!(b[2].right, w);
            for r in b {
                assert_eq!((r.top, r.bottom), (0, l.titlebar.bottom));
                assert!((r.right - r.left - l.px(46.0)).abs() <= 1);
            }
        }
        let l = layout::Layout::new(1200, 820, 96, Page::Processes);
        let b = l.caption_buttons();
        assert_eq!(t(b[0]), (1062, 0, 1108, 44));
        assert_eq!(t(b[1]), (1108, 0, 1154, 44));
        assert_eq!(t(b[2]), (1154, 0, 1200, 44));
    }

    #[test]
    fn hit_test_maps_the_title_strip_and_resize_borders_at_100_percent() {
        let m = map(1200, 820, 96, false);
        let band = m.top_band;
        assert_eq!(band, resize_border(96).1);
        // Caption buttons (below the top resize band).
        assert_eq!(hit_test(&m, 1080, 22), HTMINBUTTON);
        assert_eq!(hit_test(&m, 1130, 30), HTMAXBUTTON);
        assert_eq!(hit_test(&m, 1177, 22), HTCLOSE);
        assert_eq!(hit_test(&m, 1199, 43), HTCLOSE);
        // Brand, empty strip on both sides of the search box -> caption.
        assert_eq!(hit_test(&m, 20, 22), HTCAPTION);
        assert_eq!(hit_test(&m, 120, 22), HTCAPTION);
        assert_eq!(hit_test(&m, 300, 22), HTCAPTION);
        assert_eq!(hit_test(&m, 950, 40), HTCAPTION);
        // The search box (frame padding too) is client.
        assert_eq!(hit_test(&m, 415, 22), HTCLIENT);
        assert_eq!(hit_test(&m, 860, 22), HTCLIENT);
        assert_eq!(hit_test(&m, 411, 6.max(band)), HTCLIENT);
        // Below the strip: client (rail, main panel, status bar).
        assert_eq!(hit_test(&m, 100, 44), HTCLIENT);
        assert_eq!(hit_test(&m, 600, 400), HTCLIENT);
        assert_eq!(hit_test(&m, 1199, 819), HTCLIENT);
        // Top resize band inside the strip, corners along the edges.
        assert_eq!(hit_test(&m, 600, 0), HTTOP);
        assert_eq!(hit_test(&m, 600, band - 1), HTTOP);
        assert_eq!(hit_test(&m, 300, band), HTCAPTION);
        assert_eq!(hit_test(&m, 5, 2), HTTOPLEFT);
        assert_eq!(hit_test(&m, 1190, 2), HTTOPRIGHT);
        assert_eq!(hit_test(&m, 1177, 2), HTTOP);
        // The invisible borders outside the client.
        assert_eq!(hit_test(&m, -3, 400), HTLEFT);
        assert_eq!(hit_test(&m, 1203, 400), HTRIGHT);
        assert_eq!(hit_test(&m, 600, 824), HTBOTTOM);
        assert_eq!(hit_test(&m, -3, 10), HTTOPLEFT);
        assert_eq!(hit_test(&m, 1203, 10), HTTOPRIGHT);
        assert_eq!(hit_test(&m, -3, 815), HTBOTTOMLEFT);
        assert_eq!(hit_test(&m, 5, 824), HTBOTTOMLEFT);
        assert_eq!(hit_test(&m, 1203, 815), HTBOTTOMRIGHT);
        assert_eq!(hit_test(&m, 1195, 824), HTBOTTOMRIGHT);
    }

    #[test]
    fn maximized_hit_test_has_no_resize_bands() {
        let m = map(1920, 1040, 96, true);
        assert_eq!(m.top_band, 0);
        // Fitts: the very corner closes, the very top is caption.
        assert_eq!(hit_test(&m, 1919, 0), HTCLOSE);
        assert_eq!(hit_test(&m, 1919 - 46, 0), HTMAXBUTTON);
        assert_eq!(hit_test(&m, 600, 0), HTCAPTION);
        assert_eq!(hit_test(&m, 0, 0), HTCAPTION);
        assert_eq!(hit_test(&m, 0, 500), HTCLIENT);
        assert_eq!(hit_test(&m, -1, 500), HTNOWHERE);
        assert_eq!(hit_test(&m, 500, 1040), HTNOWHERE);
    }

    #[test]
    fn hit_test_scales_at_150_percent() {
        let m = map(1800, 1230, 144, false);
        let band = resize_border(144).1;
        assert_eq!(m.top_band, band);
        let l = layout::Layout::new(1800, 1230, 144, Page::Processes);
        assert_eq!(l.titlebar.bottom, 66);
        let b = l.caption_buttons();
        assert_eq!(t(b[2]), (1731, 0, 1800, 66));
        assert_eq!(hit_test(&m, 1760, 40), HTCLOSE);
        assert_eq!(hit_test(&m, 1700, 40), HTMAXBUTTON);
        assert_eq!(hit_test(&m, 1600, 40), HTMINBUTTON);
        assert_eq!(hit_test(&m, 1590, 40), HTCAPTION);
        assert_eq!(hit_test(&m, 620, 30), HTCLIENT);
        assert_eq!(hit_test(&m, 100, 65), HTCAPTION);
        assert_eq!(hit_test(&m, 100, 66), HTCLIENT);
        assert_eq!(hit_test(&m, 20, band - 1), HTTOPLEFT);
        assert_eq!(hit_test(&m, 30, band - 1), HTTOP);
    }

    /// Alt+Space drops the system menu down from the strip's bottom-left
    /// corner, never over the 44 px strip.
    #[test]
    fn keyboard_system_menu_opens_below_the_title_strip() {
        for (w, h, dpi, bottom) in [(1200, 820, 96, 44), (1800, 1230, 144, 66)] {
            let l = layout::Layout::new(w, h, dpi, Page::Processes);
            let at = keyboard_menu_point(&l);
            assert_eq!((at.x, at.y), (0, bottom), "{dpi}");
            assert_eq!(at.y, l.titlebar.bottom);
        }
    }

    /// The caption glyphs match the reference render pixel for pixel at
    /// 100 % (window-processes-light.png, caption at x 1060 of its content).
    #[test]
    fn caption_glyphs_rasterise_like_the_reference() {
        unsafe {
            gfx::startup();
            let fonts = fonts::Fonts::new(96, Language::English);
            let c = theme::LIGHT;
            let mut dib = gfx::Dib::new(46 * 3, 44).unwrap();
            {
                let pt = Painter::new(dib.dc(), 96, &fonts).with_palette(c);
                for (i, kind) in [Caption::Minimize, Caption::Maximize, Caption::Close]
                    .into_iter()
                    .enumerate()
                {
                    let r = rect(46 * i as i32, 0, 46 * (i as i32 + 1), 44);
                    widgets::caption_button(&pt, r, kind, 0.0, false, true, c.bg);
                }
            }
            let rgb = |dib: &mut gfx::Dib, x: i32, y: i32| dib.pixel(x, y) & 0x00ff_ffff;
            let hex = |c: u32| (c & 0xff) << 16 | (c & 0xff00) | (c >> 16 & 0xff);
            let (bg, fg) = (hex(c.bg), hex(c.fg));
            let near = |a: u32, b: u32| {
                (0..3).all(|k| {
                    ((a >> (8 * k) & 0xff) as i32 - (b >> (8 * k) & 0xff) as i32).abs() <= 2
                })
            };
            // Minimize: `M0 5h10` covers rows 21 and 22 at 50 % (#848B8F).
            for x in 18..28 {
                assert!(near(rgb(&mut dib, x, 21), 0x848B8F), "min {x}");
                assert!(near(rgb(&mut dib, x, 22), 0x848B8F), "min {x}");
                assert_eq!(rgb(&mut dib, x, 20), bg);
                assert_eq!(rgb(&mut dib, x, 23), bg);
            }
            assert_eq!(rgb(&mut dib, 17, 21), bg);
            assert_eq!(rgb(&mut dib, 28, 21), bg);
            // Maximize: a crisp 10 × 10 outline with the same softened
            // corner at all four corners (31 % corner pixel, 77 % beside it).
            for i in 2..8 {
                assert_eq!(rgb(&mut dib, 46 + 18 + i, 17), fg);
                assert_eq!(rgb(&mut dib, 46 + 18 + i, 26), fg);
                assert_eq!(rgb(&mut dib, 46 + 18, 17 + i), fg);
                assert_eq!(rgb(&mut dib, 46 + 27, 17 + i), fg);
            }
            for i in 1..9 {
                assert_eq!(rgb(&mut dib, 46 + 22, 17 + i), bg);
            }
            let soft = hex(theme::mix(c.bg, c.fg, 0.77));
            let corner = hex(theme::mix(c.bg, c.fg, 0.31));
            for (x, y, dx, dy) in [
                (18, 17, 1, 1),
                (27, 17, -1, 1),
                (18, 26, 1, -1),
                (27, 26, -1, -1),
            ] {
                assert!(near(rgb(&mut dib, 46 + x, y), corner), "corner {x},{y}");
                assert!(near(rgb(&mut dib, 46 + x + dx, y), soft), "soft {x},{y}");
                assert!(near(rgb(&mut dib, 46 + x, y + dy), soft), "soft {x},{y}");
            }
            // Close: two one-pixel staircases, no antialiasing fringe.
            for i in 0..10 {
                assert_eq!(rgb(&mut dib, 92 + 18 + i, 17 + i), fg);
                assert_eq!(rgb(&mut dib, 92 + 27 - i, 17 + i), fg);
                if i != 4 && i != 5 {
                    assert_eq!(rgb(&mut dib, 92 + 19 + i, 17 + i), bg, "close {i}");
                }
            }
            assert_eq!(rgb(&mut dib, 92 + 17, 16), bg);
            assert_eq!(rgb(&mut dib, 92 + 28, 27), bg);
        }
    }

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    /// A hidden main window (never shown) with its App.
    struct Harness {
        p: *mut App,
        class: Vec<u16>,
        _channels: (
            SyncSender<MonitorSample>,
            Receiver<Command>,
            Receiver<Job>,
            Sender<JobResult>,
        ),
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
                    "FeatherFrameTest_{}_{}",
                    std::process::id(),
                    NEXT.fetch_add(1, AtomicOrdering::Relaxed)
                ));
                assert!(!create_window(p, &class, 1200, 820).is_null());
                assert_eq!(IsWindowVisible((*p).hwnd), 0);
                Self {
                    p,
                    class,
                    _channels: (snapshots, commands, jobs, complete),
                }
            }
        }
        unsafe fn hit(&self, x: i32, y: i32) -> u32 {
            let mut point = POINT { x, y };
            ClientToScreen((*self.p).hwnd, &mut point);
            let packed = (point.x as u16 as u32 | (point.y as u16 as u32) << 16) as isize;
            SendMessageW((*self.p).hwnd, WM_NCHITTEST, 0, packed) as u32
        }
        unsafe fn posted_command(&self) -> Option<u32> {
            let mut msg: MSG = zeroed();
            let mut found = None;
            while PeekMessageW(
                &mut msg,
                (*self.p).hwnd,
                WM_SYSCOMMAND,
                WM_SYSCOMMAND,
                PM_REMOVE,
            ) != 0
            {
                found = Some(msg.wParam as u32 & 0xfff0);
            }
            found
        }
    }

    impl Drop for Harness {
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

    fn client_pack(x: i32, y: i32) -> isize {
        (x as u16 as u32 | (y as u16 as u32) << 16) as isize
    }

    #[test]
    fn the_window_has_no_native_caption_only_resize_borders() {
        let h = Harness::new();
        unsafe {
            let hwnd = (*h.p).hwnd;
            let (fx, fy) = resize_border(GetDpiForWindow(hwnd));
            let mut window: RECT = zeroed();
            let mut client: RECT = zeroed();
            GetWindowRect(hwnd, &mut window);
            GetClientRect(hwnd, &mut client);
            let mut origin = POINT { x: 0, y: 0 };
            ClientToScreen(hwnd, &mut origin);
            // The client starts at the window's top edge (below DWM's 1 px
            // border row on Windows 11): no caption, no top frame.
            let top = (*h.p).frame.top_frame;
            assert!((0..=1).contains(&top));
            assert_eq!(origin.y, window.top + top);
            assert_eq!(origin.x, window.left + fx);
            assert_eq!(window.right - window.left - client.right, 2 * fx);
            assert_eq!(window.bottom - window.top - client.bottom, fy + top);
            // Created 1200 × 820: attach kept that window's client height
            // although DWM's border row became known late.
            assert_eq!(client.bottom, 820 - fy);
            // fit_client lands exactly (previews compare 1:1 with the reference).
            fit_client(hwnd, 1200, 820);
            GetClientRect(hwnd, &mut client);
            assert_eq!(t(client), (0, 0, 1200, 820));
            GetWindowRect(hwnd, &mut window);
            assert_eq!(
                (window.right - window.left, window.bottom - window.top),
                outer_size(GetDpiForWindow(hwnd), 1200, 820, top)
            );
            // WM_NCHITTEST through the real window procedure.
            layout(h.p);
            // Button centres from the layout (the test machine may not be at 100 %).
            let centre = |r: RECT| ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            let b = current_layout(h.p).caption_buttons();
            // Minimize and close answer HTCLIENT (no system caption
            // tooltips); maximize stays HTMAXBUTTON for Snap Layouts.
            for (r, code) in b.into_iter().zip([HTCLIENT, HTMAXBUTTON, HTCLIENT]) {
                let (x, y) = centre(r);
                assert_eq!(h.hit(x, y), code);
            }
            // The client-area buttons hover like the non-client path.
            let at = |(x, y): (i32, i32)| (x & 0xffff | (y & 0xffff) << 16) as isize;
            SendMessageW(hwnd, WM_MOUSEMOVE, 0, at(centre(b[2])));
            assert_eq!((*h.p).frame.hot, Some(Button::Close));
            SendMessageW(hwnd, WM_MOUSEMOVE, 0, at(centre(b[0])));
            assert_eq!((*h.p).frame.hot, Some(Button::Minimize));
            SendMessageW(hwnd, WM_MOUSELEAVE, 0, 0);
            assert_eq!((*h.p).frame.hot, None);
            assert_eq!(h.hit(100, 22), HTCAPTION);
            assert_eq!(h.hit(600, 2), HTTOP);
            assert_eq!(h.hit(600, -top), HTTOP);
            assert_eq!(h.hit(600, 300), HTCLIENT);
            assert_eq!(h.hit(-2, 300), HTLEFT);
            assert_eq!(h.hit(600, 822), HTBOTTOM);
            // The minimum track size is the supported minimum client + borders.
            let mut info: MINMAXINFO = zeroed();
            SendMessageW(hwnd, WM_GETMINMAXINFO, 0, &mut info as *mut _ as isize);
            assert_eq!(
                (info.ptMinTrackSize.x, info.ptMinTrackSize.y),
                (980 + 2 * fx, 660 + fy + top)
            );
            // Scaling to 150 % keeps the client proportional (no caption added).
            let mut size = SIZE { cx: 0, cy: 0 };
            assert_eq!(
                SendMessageW(hwnd, WM_GETDPISCALEDSIZE, 144, &mut size as *mut _ as isize),
                1
            );
            assert_eq!((size.cx, size.cy), outer_size(144, 1800, 1230, top));
        }
    }

    #[test]
    fn caption_buttons_hover_press_and_act_on_release_over_the_same_button() {
        let h = Harness::new();
        unsafe {
            let p = h.p;
            let hwnd = (*p).hwnd;
            fit_client(hwnd, 1200, 820);
            layout(p);
            // Hover: non-client moves over the close button fade it in.
            SendMessageW(hwnd, WM_NCMOUSEMOVE, HTCLOSE as usize, 0);
            assert_eq!((*p).frame.hot, Some(Button::Close));
            assert_eq!((*p).anim.anim.target(Button::Close.key()), Some(1.0));
            SendMessageW(hwnd, WM_NCMOUSEMOVE, HTMINBUTTON as usize, 0);
            assert_eq!((*p).frame.hot, Some(Button::Minimize));
            assert_eq!((*p).anim.anim.target(Button::Close.key()), Some(0.0));
            assert_eq!((*p).anim.anim.target(Button::Minimize.key()), Some(1.0));
            // Leaving the non-client area (e.g. into the client) clears it.
            SendMessageW(hwnd, WM_NCMOUSELEAVE, 0, 0);
            assert_eq!((*p).frame.hot, None);
            assert_eq!((*p).anim.anim.target(Button::Minimize.key()), Some(0.0));
            SendMessageW(hwnd, WM_NCMOUSEMOVE, HTMAXBUTTON as usize, 0);
            SendMessageW(hwnd, WM_MOUSEMOVE, 0, client_pack(600, 400));
            assert_eq!((*p).frame.hot, None);
            // Press minimize, release elsewhere: nothing happens.
            h.posted_command();
            SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTMINBUTTON as usize, 0);
            assert_eq!((*p).frame.pressed, Some(Button::Minimize));
            SendMessageW(hwnd, WM_MOUSEMOVE, 1, client_pack(600, 22));
            assert_eq!((*p).frame.hot, None, "dragged off the pressed button");
            SendMessageW(hwnd, WM_LBUTTONUP, 0, client_pack(600, 22));
            assert_eq!((*p).frame.pressed, None);
            assert_eq!(h.posted_command(), None);
            // Press and release over the same button: the command is posted.
            let b = current_layout(p).caption_buttons();
            let mid = |r: RECT| (r.left + r.right) / 2;
            for (code, x, command) in [
                (HTMINBUTTON, mid(b[0]), SC_MINIMIZE),
                (HTMAXBUTTON, mid(b[1]), SC_MAXIMIZE),
                (HTCLOSE, mid(b[2]), SC_CLOSE),
            ] {
                SendMessageW(hwnd, WM_NCLBUTTONDOWN, code as usize, 0);
                SendMessageW(hwnd, WM_MOUSEMOVE, 1, client_pack(x, 30));
                assert_eq!((*p).frame.hot.map(Button::hit), Some(code));
                SendMessageW(hwnd, WM_LBUTTONUP, 0, client_pack(x, 30));
                assert_eq!(h.posted_command(), Some(command), "{code}");
                assert_eq!((*p).frame.pressed, None);
            }
            // Pressing another button and releasing over it after moving:
            // press max, move over close, release -> nothing.
            SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTMAXBUTTON as usize, 0);
            SendMessageW(hwnd, WM_LBUTTONUP, 0, client_pack(mid(b[2]), 30));
            assert_eq!(h.posted_command(), None);
            // Losing the capture cancels a press.
            SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCLOSE as usize, 0);
            SendMessageW(hwnd, WM_CAPTURECHANGED, 0, 0);
            assert_eq!((*p).frame.pressed, None);
            SendMessageW(hwnd, WM_LBUTTONUP, 0, client_pack(mid(b[2]), 30));
            assert_eq!(h.posted_command(), None);
            // Clicking the search box padding (icon / `Ctrl K`) focuses the input.
            let l = current_layout(p);
            let cy = (l.search.top + l.search.bottom) / 2;
            SetFocus(hwnd);
            SetWindowTextW((*p).search, wide("svc").as_ptr());
            SendMessageW(hwnd, WM_LBUTTONDOWN, 1, client_pack(l.search.left + 12, cy));
            assert_eq!(GetFocus(), (*p).search);
            let mut selection = (0u32, 0u32);
            SendMessageW(
                (*p).search,
                EM_GETSEL,
                &mut selection.0 as *mut u32 as usize,
                &mut selection.1 as *mut u32 as isize,
            );
            assert_eq!(selection, (3, 3), "caret at the end");
            SetFocus(hwnd);
            SendMessageW(
                hwnd,
                WM_LBUTTONDOWN,
                1,
                client_pack(l.search.right - 20, cy),
            );
            assert_eq!(GetFocus(), (*p).search);
            // Not while the search is disabled (Performance / Settings).
            SetFocus(hwnd);
            switch_page(p, Page::Performance);
            SendMessageW(hwnd, WM_LBUTTONDOWN, 1, client_pack(l.search.left + 12, cy));
            assert_ne!(GetFocus(), (*p).search);
            switch_page(p, Page::Processes);
            // The shell / accessibility see the painted caption buttons.
            let mut info: TITLEBARINFOEX = zeroed();
            info.cbSize = size_of::<TITLEBARINFOEX>() as u32;
            SendMessageW(hwnd, WM_GETTITLEBARINFOEX, 0, &mut info as *mut _ as isize);
            let mut origin = POINT { x: 0, y: 0 };
            ClientToScreen(hwnd, &mut origin);
            let buttons = current_layout(p).caption_buttons();
            for (slot, r) in [(2, buttons[0]), (3, buttons[1]), (5, buttons[2])] {
                assert_eq!(info.rgrect[slot].left, origin.x + r.left);
                assert_eq!(info.rgrect[slot].bottom, origin.y + r.bottom);
                assert_eq!(info.rgstate[slot] & STATE_INVISIBLE, 0);
            }
            assert_ne!(info.rgstate[4] & STATE_INVISIBLE, 0, "no help button");
            // Activation: inactive windows mute the caption.
            // Another app takes over: muted. The app's own popup: still active.
            SendMessageW(hwnd, WM_NCACTIVATE, 0, 0);
            assert!(is_active(p), "owned popup activation keeps the caption");
            SendMessageW(hwnd, WM_ACTIVATEAPP, 0, 0);
            assert!(!is_active(p));
            SendMessageW(hwnd, WM_NCACTIVATE, 1, 0);
            assert!(is_active(p));
            SendMessageW(hwnd, WM_ACTIVATEAPP, 0, 0);
            SendMessageW(hwnd, WM_ACTIVATEAPP, 1, 0);
            assert!(is_active(p));
            (*p).anim.stop();
        }
    }

    /// Hover fades that are still running when the buttons move (resize,
    /// maximize / restore, DPI) never stay behind as a partial tint: a
    /// plain resize keeps them running (their repaint region follows the
    /// buttons), a maximize / restore or DPI change settles them at once.
    #[test]
    fn caption_fades_follow_the_buttons_when_the_window_changes() {
        let h = Harness::new();
        unsafe {
            let p = h.p;
            let hwnd = (*p).hwnd;
            (*p).anim.anim = anim::Animator::new().with_reduced_motion(false);
            fit_client(hwnd, 1200, 820);
            layout(p);
            let key = Button::Minimize.key();
            // Hovered minimize, then the mouse leaves: a 150 ms fade-out runs.
            SendMessageW(hwnd, WM_NCMOUSEMOVE, HTMINBUTTON as usize, 0);
            (*p).anim.set(key, 1.0);
            SendMessageW(hwnd, WM_NCMOUSELEAVE, 0, 0);
            assert!((*p).anim.anim.is_key_animating(key));
            let before = current_layout(p).caption_buttons()[0];
            // Wider (e.g. snapped): the fade continues where the button is now.
            fit_client(hwnd, 1400, 820);
            let after = current_layout(p).caption_buttons()[0];
            assert_eq!(after.left - before.left, 200);
            assert!((*p).anim.anim.is_key_animating(key));
            assert_eq!((*p).anim.anim.target(key), Some(0.0));
            // Maximize (a window-state change): every fade settles now.
            SendMessageW(
                hwnd,
                WM_SIZE,
                SIZE_MAXIMIZED as usize,
                client_pack(1400, 820),
            );
            assert!((*p).frame.maximized);
            assert_eq!((*p).frame.hot, None);
            for button in Button::ALL {
                assert!(!(*p).anim.anim.is_key_animating(button.key()));
                assert_eq!((*p).anim.value(button.key()), 0.0);
            }
            SendMessageW(
                hwnd,
                WM_SIZE,
                SIZE_RESTORED as usize,
                client_pack(1400, 820),
            );
            assert!(!(*p).frame.maximized);
            // Restore while a fade runs: settled as well.
            SendMessageW(hwnd, WM_NCMOUSEMOVE, HTCLOSE as usize, 0);
            (*p).anim.set(Button::Close.key(), 1.0);
            SendMessageW(hwnd, WM_NCMOUSELEAVE, 0, 0);
            SendMessageW(
                hwnd,
                WM_SIZE,
                SIZE_MAXIMIZED as usize,
                client_pack(1400, 820),
            );
            assert!(!(*p).anim.anim.is_key_animating(Button::Close.key()));
            SendMessageW(
                hwnd,
                WM_SIZE,
                SIZE_RESTORED as usize,
                client_pack(1400, 820),
            );
            // DPI change: fading keys jump to their end, the hot one stays lit.
            SendMessageW(hwnd, WM_NCMOUSEMOVE, HTMAXBUTTON as usize, 0);
            assert!((*p).anim.anim.is_key_animating(Button::Maximize.key()));
            let r = RECT {
                left: 0,
                top: 0,
                right: 1800,
                bottom: 1230,
            };
            SendMessageW(
                hwnd,
                WM_DPICHANGED,
                144 | (144 << 16),
                &r as *const _ as isize,
            );
            assert_eq!((*p).frame.hot, Some(Button::Maximize));
            assert!(!(*p).anim.anim.is_key_animating(Button::Maximize.key()));
            assert_eq!((*p).anim.value(Button::Maximize.key()), 1.0);
            let r = RECT {
                left: 0,
                top: 0,
                right: 1200,
                bottom: 820,
            };
            SendMessageW(
                hwnd,
                WM_DPICHANGED,
                96 | (96 << 16),
                &r as *const _ as isize,
            );
            (*p).anim.stop();
        }
    }

    /// The Windows 10 path on any machine: no DWM border row, the frame
    /// extended 1 px into the client and that row painted black (= DWM's
    /// own top border shows through); maximized windows drop both.
    #[test]
    fn windows_10_path_extends_the_frame_under_a_black_top_row() {
        let h = Harness::new();
        unsafe {
            let p = h.p;
            let hwnd = (*p).hwnd;
            (*p).frame.win11 = false;
            (*p).frame.margins = None;
            apply_margins(p);
            assert_eq!((*p).frame.top_border, 1);
            SetWindowPos(
                hwnd,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
            assert_eq!((*p).frame.top_frame, 0);
            let mut window: RECT = zeroed();
            GetWindowRect(hwnd, &mut window);
            let mut origin = POINT { x: 0, y: 0 };
            ClientToScreen(hwnd, &mut origin);
            assert_eq!(origin.y, window.top);
            fit_client(hwnd, 1200, 820);
            layout(p);
            let mut dib = gfx::Dib::new(1200, 820).unwrap();
            capture::paint_client_and_children(hwnd, dib.dc()).unwrap();
            let bg = colors().bg;
            let rgb = |c: u32| (c & 0xff) << 16 | (c & 0xff00) | (c >> 16 & 0xff);
            for x in [0, 300, 1100, 1199] {
                assert_eq!(dib.pixel(x, 0) & 0xff_ffff, 0, "row 0 at {x}");
            }
            assert_eq!(dib.pixel(300, 1) & 0xff_ffff, rgb(bg));
            // Previews / painting as maximized: no border row.
            (*p).frame.preview_maximized = Some(true);
            let mut dib = gfx::Dib::new(1200, 820).unwrap();
            capture::paint_client_and_children(hwnd, dib.dc()).unwrap();
            assert_eq!(dib.pixel(300, 0) & 0xff_ffff, rgb(bg));
            (*p).frame.preview_maximized = None;
            // Back to this machine's real DWM.
            apply_theme(p);
        }
    }

    unsafe fn face(font: HFONT) -> String {
        let mut info: LOGFONTW = zeroed();
        GetObjectW(
            font,
            size_of::<LOGFONTW>() as i32,
            (&mut info as *mut LOGFONTW).cast(),
        );
        let end = info.lfFaceName.iter().position(|&c| c == 0).unwrap_or(32);
        String::from_utf16_lossy(&info.lfFaceName[..end])
    }

    #[test]
    fn typed_hangul_switches_the_search_input_to_a_full_size_hangul_font() {
        crate::i18n::with_language(Language::English, || unsafe {
            let h = Harness::new();
            let p = h.p;
            let search = (*p).search;
            let current = || SendMessageW(search, WM_GETFONT, 0, 0) as HFONT;
            assert_eq!(current(), (*p).fonts.body);
            SetWindowTextW(search, wide("svc").as_ptr());
            assert_eq!(current(), (*p).fonts.body);
            SetWindowTextW(search, wide("svc 서비스").as_ptr());
            assert_ne!(current(), (*p).fonts.body);
            assert_eq!(face(current()), "Malgun Gothic");
            assert_eq!(search_font(p), current());
            // Same pixel size as the body role (14 px at 96 DPI).
            let mut info: LOGFONTW = zeroed();
            GetObjectW(
                current(),
                size_of::<LOGFONTW>() as i32,
                (&mut info as *mut LOGFONTW).cast(),
            );
            assert_eq!(info.lfHeight, -fonts::pixel_size(14.0, (*p).dpi));
            // The margins stay at 0 (the text starts at the 34 px inset).
            assert_eq!(SendMessageW(search, EM_GETMARGINS, 0, 0), 0);
            // Clearing the Hangul returns to the spec font.
            SetWindowTextW(search, wide("svc").as_ptr());
            assert_eq!(current(), (*p).fonts.body);
            // A DPI change keeps the switch at the new size.
            SetWindowTextW(search, wide("크롬").as_ptr());
            let r = RECT {
                left: 0,
                top: 0,
                right: 1800,
                bottom: 1230,
            };
            SendMessageW(
                (*p).hwnd,
                WM_DPICHANGED,
                144 | (144 << 16),
                &r as *const _ as isize,
            );
            GetObjectW(
                current(),
                size_of::<LOGFONTW>() as i32,
                (&mut info as *mut LOGFONTW).cast(),
            );
            assert_eq!(face(current()), "Malgun Gothic");
            assert_eq!(info.lfHeight, -fonts::pixel_size(14.0, 144));
            SetWindowTextW(search, wide("").as_ptr());
            assert_eq!(current(), (*p).fonts.body);
        });
    }
}
