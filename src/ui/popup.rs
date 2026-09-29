//! Layered popups (DESIGN_SPEC §4 "Popup surfaces", §6): menus, select
//! dropdowns, the confirm dialog with its scrim and the toast.
//!
//! Every popup is a fully painted `WS_EX_LAYERED` window presented with
//! `UpdateLayeredWindow` (`gfx::LayeredSurface`: opaque GDI/GDI+ content, the
//! antialiased rounded outline and the reference's two-layer soft shadow).
//! Popups never activate and never host child windows. Each one owns an
//! `AnimHost` on its own window (the frame timer runs only while something
//! moves): fade + 4 px slide in, fade out, item hover cross-fades. The window
//! owns its state (freed in WM_NCDESTROY), so destroying the owner frees
//! every popup and its surface.
//!
//! Menus ([`track_menu`]) and the confirm dialog ([`confirm_dialog`]) are
//! synchronous like TrackPopupMenu / MessageBox: a local message loop
//! dispatches everything except input, which it handles itself, so timers,
//! painting and the popups' own animations keep running.

use super::gfx::{self, RectF};
use super::theme::{mix, solid, Palette};
use super::widgets::{self, ButtonState, ButtonStyle, Painter};
use super::*;
use std::cell::RefCell;
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};

#[link(name = "user32")]
extern "system" {
    /// user32's NotifyWinEvent (windows-sys lists it under the
    /// `Win32_UI_Accessibility` feature, which this build does not enable):
    /// announces menu popups and their current item to assistive technology.
    fn NotifyWinEvent(event: u32, hwnd: HWND, idobject: i32, idchild: i32);
}

const CLASS: &str = "FeatherTaskManager.Layer";
/// Frame timer of every popup's `AnimHost` (and its delay timer).
const TIMER: usize = 0xFEB0;
/// Hover delay before a submenu opens / closes (on the menu's capture window).
const SUBMENU_TIMER: usize = 0xFEB2;
const OPACITY: (usize, u32) = (usize::MAX, anim::part::OPACITY);
const fn hover(index: usize) -> (usize, u32) {
    (index, anim::part::HOVER)
}
/// Check-mark column of menus with checkable items (DIP).
const GUTTER: f32 = 20.0;
/// Distance between an anchor and its popup (DIP).
const GAP: f32 = 4.0;
/// `.ctx`: min-width 200, padding 4 (DIP).
const MENU_MIN_WIDTH: f32 = 200.0;
const MENU_PAD: f32 = 4.0;
/// Widest a menu grows before its labels ellipsize (DIP).
const MENU_MAX_WIDTH: f32 = 480.0;
/// `.dialog { width: min(440px, 100% - 32px) }`.
const DIALOG_WIDTH: f32 = 440.0;

// ───────────────────────────── menu model ─────────────────────────────

/// One entry of a menu or dropdown. `submenu` non-empty = a submenu item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct MenuItem {
    pub id: usize,
    pub label: String,
    /// Right-aligned shortcut shown as a `.kbd` badge ("Del", "F5").
    pub hint: Option<String>,
    pub enabled: bool,
    pub checked: bool,
    pub separator: bool,
    pub submenu: Vec<MenuItem>,
}

impl MenuItem {
    pub(super) fn item(id: usize, label: &str) -> Self {
        Self {
            id,
            label: label.into(),
            enabled: true,
            ..Self::default()
        }
    }
    pub(super) fn separator() -> Self {
        Self {
            separator: true,
            ..Self::default()
        }
    }
    pub(super) fn has_submenu(&self) -> bool {
        !self.submenu.is_empty()
    }
    fn selectable(&self) -> bool {
        !self.separator && self.enabled
    }
}

/// "&Open" → "Open", "&&" → "&" (menus here are mnemonic-free).
fn strip_mnemonics(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            if chars.peek() == Some(&'&') {
                out.push('&');
                chars.next();
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// A native menu label "End task\tDel" → ("End task", Some("Del")).
pub(super) fn split_hint(text: &str) -> (String, Option<String>) {
    match text.split_once('\t') {
        Some((label, hint)) => (
            strip_mnemonics(label),
            Some(hint.trim().to_owned()).filter(|hint| !hint.is_empty()),
        ),
        None => (strip_mnemonics(text), None),
    }
}

/// The model of a native menu (the app keeps building HMENUs, so the
/// existing builders and their tests stay the single source of truth):
/// labels with their tab-separated hints, enabled / checked state,
/// separators and submenus.
pub(super) unsafe fn menu_items(menu: HMENU) -> Vec<MenuItem> {
    menu_items_at(menu, 0)
}

unsafe fn menu_items_at(menu: HMENU, depth: usize) -> Vec<MenuItem> {
    let count = GetMenuItemCount(menu);
    if menu.is_null() || count <= 0 || depth > 4 {
        return Vec::new();
    }
    let mut items = Vec::with_capacity(count as usize);
    for index in 0..count as u32 {
        let mut text = [0u16; 512];
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_STRING,
            dwTypeData: text.as_mut_ptr(),
            cch: text.len() as u32 - 1,
            ..zeroed()
        };
        if GetMenuItemInfoW(menu, index, 1, &mut info) == 0 {
            continue;
        }
        if info.fType & MFT_SEPARATOR != 0 {
            items.push(MenuItem::separator());
            continue;
        }
        let length = (info.cch as usize).min(text.len() - 1);
        let (label, hint) = split_hint(&String::from_utf16_lossy(&text[..length]));
        items.push(MenuItem {
            id: info.wID as usize,
            label,
            hint,
            enabled: info.fState & MFS_DISABLED == 0,
            checked: info.fState & MFS_CHECKED != 0,
            separator: false,
            submenu: if info.hSubMenu.is_null() {
                Vec::new()
            } else {
                menu_items_at(info.hSubMenu, depth + 1)
            },
        });
    }
    items
}

/// The next selectable item after `from` (wrapping; `None` = from the
/// start or the end), skipping separators and disabled items.
pub(super) fn step_item(items: &[MenuItem], from: Option<usize>, forward: bool) -> Option<usize> {
    let n = items.len();
    let mut at = from.filter(|&i| i < n);
    for _ in 0..n {
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
        };
        if items[next].selectable() {
            return Some(next);
        }
        at = Some(next);
    }
    None
}

/// Mnemonic-free first-letter jump: the next selectable item after `from`
/// whose label starts with `letter` (case-insensitive, cycling).
pub(super) fn letter_item(items: &[MenuItem], from: Option<usize>, letter: char) -> Option<usize> {
    let wanted: String = letter.to_lowercase().collect();
    if wanted.trim().is_empty() {
        return None;
    }
    let n = items.len();
    let start = from.map_or(0, |i| i + 1);
    (0..n).map(|k| (start + k) % n).find(|&i| {
        items[i].selectable()
            && items[i]
                .label
                .trim_start()
                .to_lowercase()
                .starts_with(wanted.as_str())
    })
}

// ───────────────────────────── placement ─────────────────────────────

/// Where a popup opens (screen px).
#[derive(Clone, Copy)]
pub(super) enum Anchor {
    /// Below `r` (left edges aligned, or right edges when `right`), flipping
    /// above it when the work area has no room below.
    Below { r: RECT, right: bool },
    /// Above `r` (left edges aligned, or right edges when `right`), flipping
    /// below it when the work area has no room above.
    Above { r: RECT, right: bool },
    /// A context menu at a point: below-right, flipping left / up.
    Point(POINT),
    /// A submenu beside a parent menu's `item` row (`parent` = its box):
    /// right of it with the first item aligned, flipping to the left.
    Beside { item: RECT, parent: RECT },
    /// Centred on `r` (the dialog over the window).
    Center(RECT),
}

/// The top-left of a `w × h` popup box for `anchor` inside `work` and the
/// direction of its entrance slide (-1 = moves down into place, 1 = up,
/// 0 = fade only). `gap` separates it from the anchor; `inset` is the
/// distance from a submenu's top edge to its first item.
pub(super) fn place(
    anchor: Anchor,
    w: i32,
    h: i32,
    work: RECT,
    gap: i32,
    inset: i32,
) -> (POINT, i32) {
    let clamp_x = |x: i32| x.min(work.right - w).max(work.left);
    let clamp_y = |y: i32| y.min(work.bottom - h).max(work.top);
    // Prefer one side; take the other when only it fits, or the roomier
    // side when neither does.
    let vertical = |top: i32, bottom: i32, below_first: bool| {
        let below = bottom + gap;
        let above = top - gap - h;
        let fits_below = below + h <= work.bottom;
        let fits_above = above >= work.top;
        let roomier_below = work.bottom - bottom >= top - work.top;
        let use_below = if below_first {
            fits_below || !fits_above && roomier_below
        } else {
            !fits_above && (fits_below || roomier_below)
        };
        if use_below {
            (clamp_y(below), -1)
        } else {
            (clamp_y(above), 1)
        }
    };
    match anchor {
        Anchor::Below { r, right } => {
            let x = clamp_x(if right { r.right - w } else { r.left });
            let (y, slide) = vertical(r.top, r.bottom, true);
            (POINT { x, y }, slide)
        }
        Anchor::Above { r, right } => {
            let x = clamp_x(if right { r.right - w } else { r.left });
            let (y, slide) = vertical(r.top, r.bottom, false);
            (POINT { x, y }, slide)
        }
        Anchor::Point(pt) => {
            let x = if pt.x + w <= work.right {
                pt.x
            } else {
                pt.x - w
            };
            let below = pt.y + h <= work.bottom;
            let y = if below { pt.y } else { pt.y - h };
            (
                POINT {
                    x: clamp_x(x),
                    y: clamp_y(y),
                },
                if below { -1 } else { 1 },
            )
        }
        Anchor::Beside { item, parent } => {
            let right = parent.right + gap / 2;
            let x = if right + w <= work.right {
                right
            } else {
                parent.left - gap / 2 - w
            };
            (
                POINT {
                    x: clamp_x(x),
                    y: clamp_y(item.top - inset),
                },
                0,
            )
        }
        Anchor::Center(r) => (
            POINT {
                x: clamp_x(r.left + (r.right - r.left - w) / 2),
                y: clamp_y(r.top + (r.bottom - r.top - h) / 2),
            },
            1,
        ),
    }
}

/// The work area of the monitor nearest to `r`.
pub(super) unsafe fn work_area(r: RECT) -> RECT {
    let monitor = MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..zeroed()
    };
    if !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0 {
        info.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        }
    }
}

/// The visible bounds of a top-level window (DWM's frame bounds exclude the
/// invisible resize borders), screen px.
unsafe fn visible_bounds(hwnd: HWND) -> RECT {
    let mut r: RECT = zeroed();
    if DwmGetWindowAttribute(
        hwnd,
        DWMWA_EXTENDED_FRAME_BOUNDS as u32,
        (&mut r as *mut RECT).cast(),
        size_of::<RECT>() as u32,
    ) < 0
        || r.right <= r.left
    {
        GetWindowRect(hwnd, &mut r);
    }
    r
}

unsafe fn client_screen(hwnd: HWND) -> RECT {
    let mut r: RECT = zeroed();
    GetClientRect(hwnd, &mut r);
    MapWindowPoints(hwnd, null_mut(), (&mut r as *mut RECT).cast::<POINT>(), 2);
    r
}

/// Comparable window + client geometry.
fn rect_key(window: RECT, client: RECT) -> [i32; 8] {
    [
        window.left,
        window.top,
        window.right,
        window.bottom,
        client.left,
        client.top,
        client.right,
        client.bottom,
    ]
}

fn contains(r: &RECT, pt: POINT) -> bool {
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

fn offset(r: RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: r.left + dx,
        top: r.top + dy,
        right: r.right + dx,
        bottom: r.bottom + dy,
    }
}

/// A memory DC for measuring text before a popup's surface exists.
struct MeasureDc(HDC);
impl MeasureDc {
    unsafe fn new() -> Self {
        Self(CreateCompatibleDC(null_mut()))
    }
}
impl Drop for MeasureDc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                DeleteDC(self.0);
            }
        }
    }
}

// ───────────────────────────── popup window ─────────────────────────────

/// Menu / dropdown fonts, resolved from the app's current font set at paint
/// time (the set is recreated on DPI and language changes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Font {
    Ui,
    Small,
    MonoSmall,
}

impl Font {
    fn get(self, fonts: &fonts::Fonts) -> HFONT {
        match self {
            Font::Ui => fonts.ui,
            Font::Small => fonts.small,
            Font::MonoSmall => fonts.mono_small,
        }
    }
    /// The CSS line box of the role (`line-height: 1.45`).
    fn line(self) -> f32 {
        match self {
            Font::Ui => 13.0 * 1.45,
            Font::Small | Font::MonoSmall => 12.0 * 1.45,
        }
    }
}

/// Geometry of a menu-like popup.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MenuStyle {
    /// Row height (DIP): menus 32, dropdowns 28.
    pub item: f32,
    /// Row text padding (DIP): menus 10, dropdowns 8.
    pub pad_x: f32,
    pub font: Font,
    /// Minimum box width (device px).
    pub min_width: i32,
    /// Corner radius (DIP).
    pub radius: f32,
    /// Check-mark column: None = only when an item is checked.
    pub gutter: Option<bool>,
}

impl MenuStyle {
    /// `.ctx`: 200 px minimum, 32 px items, 13 px text, radius 8.
    pub(super) fn menu(dpi: i32) -> Self {
        Self {
            item: 32.0,
            pad_x: 10.0,
            font: Font::Ui,
            min_width: gfx::pxi(dpi, MENU_MIN_WIDTH),
            radius: theme::RADIUS,
            gutter: None,
        }
    }
    /// A select's dropdown: at least the select's width, 28 px items, the
    /// select's 12 px font, the current item checked.
    pub(super) fn list(min_width: i32, font: Font) -> Self {
        Self {
            item: 28.0,
            pad_x: 8.0,
            font,
            min_width,
            radius: theme::RADIUS,
            gutter: Some(true),
        }
    }
}

struct MenuView {
    items: Vec<MenuItem>,
    style: MenuStyle,
    gutter: bool,
    /// Item rows, relative to the content box.
    rows: Vec<RECT>,
    hot: Option<usize>,
    /// The item whose submenu is open (it stays highlighted).
    open: Option<usize>,
}

/// One wrapped line of dialog text.
pub(super) struct TextLine {
    text: String,
    /// Top of its CSS line box (content px).
    top: i32,
    /// Per-UTF-16-unit advances at Chromium's fractional widths (text
    /// without Hangul; see [`ideal_advances`]), else drawn by the fallback
    /// renderer it was measured with.
    advances: Option<Vec<i32>>,
}

struct DialogView {
    title: Vec<TextLine>,
    body: Vec<TextLine>,
    warn: Vec<TextLine>,
    /// Cancel, action.
    labels: [String; 2],
    action: ButtonStyle,
    /// Relative to the content box.
    buttons: [RECT; 2],
    foot: i32,
    text_left: i32,
    text_right: i32,
    focus: usize,
    ring: bool,
    pressed: Option<usize>,
}

/// The content of a fully painted modal panel (`stage_panel`): the owner
/// module keeps its state and paints it (e.g. the Nuclear Zombie panel).
pub(super) trait PanelContent {
    /// Paint into `content` (the box inside the frame, surface-filled).
    fn paint(&self, pt: &Painter, content: RECT, anim: &anim::AnimHost<(usize, u32)>);
    /// A pointer cursor at `at` (relative to the content box).
    fn pointer(&self, at: POINT) -> bool;
}

enum Kind {
    Menu(MenuView),
    Dialog(DialogView),
    Panel(Box<dyn PanelContent>),
    Scrim,
    Toast(String),
}

pub(super) struct Popup {
    hwnd: HWND,
    dpi: i32,
    /// The host window's fonts (the field, so a replaced set is followed).
    fonts: *const fonts::Fonts,
    palette: Palette,
    kind: Kind,
    surface: gfx::LayeredSurface,
    anim: anim::AnimHost<(usize, u32)>,
    /// Screen position of the content box when fully shown.
    origin: POINT,
    size: SIZE,
    /// Entrance offset at opacity 0 (device px; negative = from above).
    slide: f32,
    radius: f32,
    compose: bool,
    /// The premultiplied shadow of the box (content transparent), computed
    /// once: recomposing after a hover change only merges the content.
    shadow: Option<Vec<u32>>,
    /// A scrim's popup: closed (WM_CLOSE) when the scrim is pressed.
    dismiss: HWND,
    closing: bool,
    closed: bool,
    /// A shown menu or dropdown announced to assistive technology
    /// (EVENT_SYSTEM_MENUPOPUPSTART; the END event is still due).
    announced: bool,
}

/// The window a popup belongs to, with the DPI and fonts it is drawn in: the
/// main window, or another Feather window (the Resource Monitor) that shows
/// the same menus and confirm dialog over itself.
#[derive(Clone, Copy)]
pub(super) struct Host {
    pub hwnd: HWND,
    pub dpi: i32,
    pub fonts: *const fonts::Fonts,
}
impl Host {
    pub(super) unsafe fn main(app: *mut App) -> Self {
        Self {
            hwnd: (*app).hwnd,
            dpi: (*app).dpi,
            fonts: &(*app).fonts,
        }
    }
}

unsafe fn register() -> bool {
    static REGISTERED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REGISTERED.get_or_init(|| unsafe {
        let class = wide(CLASS);
        let definition = WNDCLASSW {
            lpfnWndProc: Some(popup_proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        RegisterClassW(&definition) != 0 || GetLastError() == ERROR_CLASS_ALREADY_EXISTS
    })
}

/// The state of one of our popup windows (null for anything else).
unsafe fn state(hwnd: HWND) -> *mut Popup {
    if hwnd.is_null() || IsWindow(hwnd) == 0 {
        return null_mut();
    }
    let mut class = [0u16; 64];
    let length = GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32);
    if length <= 0 || String::from_utf16_lossy(&class[..length as usize]) != CLASS {
        return null_mut();
    }
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Popup
}

#[allow(clippy::too_many_arguments)]
unsafe fn create(
    host: Host,
    kind: Kind,
    size: SIZE,
    origin: POINT,
    slide: f32,
    shadow: bool,
    radius: f32,
    extra: u32,
    fade: Duration,
) -> HWND {
    let owner = host.hwnd;
    if owner.is_null() || !register() {
        return null_mut();
    }
    let dpi = host.dpi;
    let layers = if shadow {
        gfx::popup_shadow(dpi).to_vec()
    } else {
        Vec::new()
    };
    let Some(surface) = gfx::LayeredSurface::new(size.cx.max(1), size.cy.max(1), &layers) else {
        return null_mut();
    };
    let margins = surface.margins();
    let total = surface.size();
    let root = GetAncestor(owner, GA_ROOT);
    let topmost = GetWindowLongW(root, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST;
    let class = wide(CLASS);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | topmost | extra,
        class.as_ptr(),
        null(),
        WS_POPUP,
        origin.x - margins.left,
        origin.y - margins.top,
        total.cx,
        total.cy,
        root,
        null_mut(),
        GetModuleHandleW(null()),
        null(),
    );
    if hwnd.is_null() {
        return hwnd;
    }
    let popup = Box::into_raw(Box::new(Popup {
        hwnd,
        dpi,
        fonts: host.fonts,
        palette: theme::colors(),
        kind,
        surface,
        anim: anim::AnimHost::new(TIMER),
        origin,
        size,
        slide,
        radius,
        compose: true,
        shadow: None,
        dismiss: null_mut(),
        closing: false,
        closed: false,
        announced: false,
    }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, popup as isize);
    (*popup).anim.attach(hwnd);
    (*popup).anim.set(OPACITY, 0.0);
    (*popup).render();
    // Hidden owners (tests, previews) get composed but invisible popups.
    if IsWindowVisible(root) != 0 {
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        if matches!((*popup).kind, Kind::Menu(_)) {
            (*popup).announced = true;
            NotifyWinEvent(
                EVENT_SYSTEM_MENUPOPUPSTART,
                hwnd,
                OBJID_CLIENT,
                CHILDID_SELF as i32,
            );
        }
    }
    (*popup)
        .anim
        .set_target(OPACITY, 1.0, fade, anim::Easing::EaseOut);
    (*popup).render();
    (*popup).after_frame();
    hwnd
}

impl Popup {
    /// Recompose if the content changed, then present at the current
    /// opacity / slide offset (moving and fading only re-present).
    unsafe fn render(&mut self) {
        if self.compose {
            self.compose = false;
            self.paint();
        }
        let t = self.anim.value(OPACITY).clamp(0.0, 1.0);
        let dy = ((1.0 - t) * self.slide).round() as i32;
        self.surface.present(
            self.hwnd,
            POINT {
                x: self.origin.x,
                y: self.origin.y + dy,
            },
            (t * 255.0).round() as u8,
        );
    }
    /// After each frame: a shown toast starts its hold; a popup that faded
    /// out closes itself (posted, never inside its own handlers).
    unsafe fn after_frame(&mut self) {
        if self.closed {
            return;
        }
        let settled = !self.anim.anim.is_key_animating(OPACITY) && !self.anim.is_delayed(OPACITY);
        if matches!(self.kind, Kind::Toast(_)) && !self.closing && settled {
            self.closing = true;
            self.anim.set_target_after(
                OPACITY,
                0.0,
                anim::motion::TOAST_HOLD,
                anim::motion::TOAST_OUT,
                anim::Easing::EaseOut,
            );
            return;
        }
        if self.closing && settled && self.anim.value(OPACITY) <= 0.0 {
            self.closed = true;
            PostMessageW(self.hwnd, WM_CLOSE, 0, 0);
        }
    }
    unsafe fn paint(&mut self) {
        let c = self.palette;
        let dpi = self.dpi;
        let hair = gfx::hairline(dpi);
        let radius = self.radius;
        let fonts = &*self.fonts;
        let Popup {
            kind,
            surface,
            anim,
            shadow,
            ..
        } = self;
        let content = surface.content();
        match kind {
            Kind::Scrim => {
                let color = c.scrim();
                scrim_pixels(
                    surface.dib(),
                    color.colorref(),
                    color.alpha() as f32 / 255.0,
                    radius,
                );
                return;
            }
            Kind::Toast(text) => {
                surface.fill_frame(c.fg, c.fg, radius, 0.0);
                let pt = Painter::new(surface.dc(), dpi, fonts).with_palette(c);
                let pad = pt.pxi(14.0);
                let cell = pt.css_rect(fonts.ui, content, Some(Font::Ui.line()));
                pt.text(
                    fonts.ui,
                    c.surface,
                    text,
                    RECT {
                        left: content.left + pad,
                        right: content.right - pad,
                        ..cell
                    },
                    DT_SINGLELINE | DT_END_ELLIPSIS | DT_LEFT,
                );
            }
            Kind::Menu(view) => {
                surface.fill_frame(c.surface, c.border, radius, hair);
                let pt = Painter::new(surface.dc(), dpi, fonts).with_palette(c);
                paint_menu(&pt, content, view, anim);
            }
            Kind::Dialog(view) => {
                surface.fill_frame(c.surface, c.border, radius, hair);
                let pt = Painter::new(surface.dc(), dpi, fonts).with_palette(c);
                paint_dialog(&pt, content, view, anim, radius);
            }
            Kind::Panel(panel) => {
                surface.fill_frame(c.surface, c.border, radius, hair);
                let pt = Painter::new(surface.dc(), dpi, fonts).with_palette(c);
                panel.paint(&pt, content, anim);
            }
        }
        compose_cached(surface, shadow, radius, c.shadow());
    }
    fn content_screen(&self) -> RECT {
        RECT {
            left: self.origin.x,
            top: self.origin.y,
            right: self.origin.x + self.size.cx,
            bottom: self.origin.y + self.size.cy,
        }
    }
    /// Assistive technology: the popup's window text (its accessible name)
    /// is the highlighted item, announced as the focus.
    unsafe fn announce_hot(&mut self) {
        let Kind::Menu(view) = &self.kind else {
            return;
        };
        if !self.announced {
            return;
        }
        let item = view.hot.and_then(|i| view.items.get(i));
        let label = item.map_or(String::new(), |item| item.label.clone());
        SetWindowTextW(self.hwnd, wide(&label).as_ptr());
        if item.is_some() {
            NotifyWinEvent(
                EVENT_OBJECT_FOCUS,
                self.hwnd,
                OBJID_CLIENT,
                CHILDID_SELF as i32,
            );
        }
    }
    /// The menu popup ended (closing or destroyed).
    unsafe fn announce_end(&mut self) {
        if std::mem::take(&mut self.announced) {
            NotifyWinEvent(
                EVENT_SYSTEM_MENUPOPUPEND,
                self.hwnd,
                OBJID_CLIENT,
                CHILDID_SELF as i32,
            );
        }
    }
    /// Start or retarget the hover cross-fade of `index`.
    fn fade_hover(&mut self, index: usize, on: bool) {
        let (target, duration) = if on {
            (1.0, anim::motion::HOVER_IN)
        } else {
            (0.0, anim::motion::HOVER_OUT)
        };
        self.anim
            .set_target(hover(index), target, duration, anim::Easing::EaseOut);
    }
}

/// `LayeredSurface::compose` with the shadow computed once per popup: the
/// margins take the cached shadow, the content box its painted pixels made
/// opaque, and only the rounded corners blend content and shadow.
fn compose_cached(
    surface: &mut gfx::LayeredSurface,
    cache: &mut Option<Vec<u32>>,
    radius: f32,
    shadow_color: u32,
) {
    let content = surface.content();
    let size = surface.size();
    let (w, h) = (size.cx as usize, size.cy as usize);
    if cache.as_ref().is_none_or(|c| c.len() != w * h) {
        // Shadow only (the content box transparent), on a scratch copy.
        let painted = surface.dib().pixels().to_vec();
        surface.compose_shadow(radius, shadow_color);
        *cache = Some(surface.dib().pixels().to_vec());
        surface.dib().pixels().copy_from_slice(&painted);
    }
    let Some(shadow) = cache.as_ref() else {
        return;
    };
    let (left, top) = (content.left as usize, content.top as usize);
    let (right, bottom) = (content.right as usize, content.bottom as usize);
    let pixels = surface.dib().pixels();
    for y in 0..h {
        let row = y * w;
        if y < top || y >= bottom {
            pixels[row..row + w].copy_from_slice(&shadow[row..row + w]);
            continue;
        }
        pixels[row..row + left].copy_from_slice(&shadow[row..row + left]);
        pixels[row + right..row + w].copy_from_slice(&shadow[row + right..row + w]);
        for pixel in &mut pixels[row + left..row + right] {
            *pixel |= 0xff00_0000;
        }
    }
    // Antialiased rounded corners: content × coverage + the shadow beneath.
    let bounds = RectF::from_rect(content);
    let reach = radius.ceil() as usize + 1;
    let corners = [
        (left, top),
        (right.saturating_sub(reach), top),
        (left, bottom.saturating_sub(reach)),
        (right.saturating_sub(reach), bottom.saturating_sub(reach)),
    ];
    for (x0, y0) in corners {
        for y in y0..(y0 + reach).min(bottom) {
            for x in x0..(x0 + reach).min(right) {
                let cover =
                    gfx::rounded_rect_coverage(x as f32 + 0.5, y as f32 + 0.5, bounds, radius);
                if cover >= 1.0 {
                    continue;
                }
                let index = y * w + x;
                let (c, s) = (pixels[index], shadow[index]);
                let channel = |shift: u32| {
                    let content = ((c >> shift) & 0xff) as f32 * cover;
                    ((content + ((s >> shift) & 0xff) as f32).round() as u32).min(255)
                };
                let alpha = (((cover * 255.0) + (s >> 24) as f32).round() as u32).min(255);
                pixels[index] = (alpha << 24)
                    | (channel(16).min(alpha) << 16)
                    | (channel(8).min(alpha) << 8)
                    | channel(0).min(alpha);
            }
        }
    }
}

/// Premultiplied `color @ alpha` over the whole surface with antialiased
/// `radius` corners (the window's own rounded corners on Windows 11).
fn scrim_pixels(dib: &mut gfx::Dib, color: u32, alpha: f32, radius: f32) {
    let (w, h) = (dib.width(), dib.height());
    let bounds = RectF::new(0.0, 0.0, w as f32, h as f32);
    let (r, g, b) = (color & 0xff, (color >> 8) & 0xff, (color >> 16) & 0xff);
    let pixel = |cover: f32| {
        let a = (alpha * cover).clamp(0.0, 1.0);
        let pa = (a * 255.0).round() as u32;
        let ch = |v: u32| ((v as f32 * a).round() as u32).min(pa);
        (pa << 24) | (ch(r) << 16) | (ch(g) << 8) | ch(b)
    };
    let full = pixel(1.0);
    let corner = radius.ceil() as i32 + 1;
    let pixels = dib.pixels();
    for y in 0..h {
        let edge_y = y < corner || y >= h - corner;
        for x in 0..w {
            let index = (y * w + x) as usize;
            pixels[index] = if edge_y && (x < corner || x >= w - corner) && radius > 0.0 {
                pixel(gfx::rounded_rect_coverage(
                    x as f32 + 0.5,
                    y as f32 + 0.5,
                    bounds,
                    radius,
                ))
            } else {
                full
            };
        }
    }
}

unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let s = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Popup;
    match msg {
        WM_TIMER if !s.is_null() => {
            let content = (*s)
                .anim
                .anim
                .active_keys()
                .iter()
                .any(|key| *key != OPACITY);
            if !(*s).anim.on_timer(w) {
                return DefWindowProcW(hwnd, msg, w, l);
            }
            if content {
                (*s).compose = true;
            }
            (*s).render();
            (*s).after_frame();
            0
        }
        // Content is presented with UpdateLayeredWindow; invalidations
        // (the AnimHost's default region) only need validating.
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            BeginPaint(hwnd, &mut ps);
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        // Menu items and dialog buttons are `<button>`s: pointer.
        WM_SETCURSOR if !s.is_null() => {
            let mut pt: POINT = zeroed();
            GetCursorPos(&mut pt);
            widgets::set_pointer(pointer_at(hwnd, pt))
        }
        WM_MOUSEACTIVATE => MA_NOACTIVATE as LRESULT,
        WM_NCHITTEST => {
            if s.is_null() || matches!((*s).kind, Kind::Toast(_)) {
                return HTTRANSPARENT as LRESULT;
            }
            let pt = POINT {
                x: (l & 0xffff) as i16 as i32,
                y: ((l >> 16) & 0xffff) as i16 as i32,
            };
            // The shadow margin is click-through (clicks there land on the
            // scrim or the owner: "outside").
            if contains(&(*s).content_screen(), pt) {
                HTCLIENT as LRESULT
            } else {
                HTTRANSPARENT as LRESULT
            }
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN
            if !s.is_null() && !(*s).dismiss.is_null() =>
        {
            PostMessageW((*s).dismiss, WM_CLOSE, 0, 0);
            0
        }
        // Wake a modal loop so it notices the lost capture.
        WM_CAPTURECHANGED => {
            PostMessageW(null_mut(), WM_NULL, 0, 0);
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_NCDESTROY => {
            if !s.is_null() {
                (*s).announce_end();
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(s));
                PostMessageW(null_mut(), WM_NULL, 0, 0);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

/// A pointer cursor over an enabled menu item or a dialog button.
unsafe fn pointer_at(hwnd: HWND, pt: POINT) -> bool {
    let s = state(hwnd);
    if s.is_null() {
        return false;
    }
    match &(*s).kind {
        Kind::Dialog(_) => dialog_button(hwnd, pt).is_some(),
        Kind::Panel(panel) => {
            let content = (*s).content_screen();
            panel.pointer(POINT {
                x: pt.x - content.left,
                y: pt.y - content.top,
            })
        }
        Kind::Menu(view) => matches!(
            menu_hit(hwnd, pt),
            Some(Some(i)) if view.items.get(i).is_some_and(MenuItem::selectable)
        ),
        _ => false,
    }
}

/// Fade a popup out (80 ms) and let it destroy itself.
pub(super) unsafe fn close(hwnd: HWND) {
    let s = state(hwnd);
    if s.is_null() || (*s).closed {
        return;
    }
    (*s).announce_end();
    (*s).closing = true;
    (*s).slide = 0.0;
    (*s).anim.cancel_delayed(OPACITY);
    (*s).anim
        .set_target(OPACITY, 0.0, anim::motion::POPUP_OUT, anim::Easing::EaseOut);
    (*s).render();
    (*s).after_frame();
}

/// Restart a popup's entrance fade now (from transparent): the scrim is
/// created before the popup above it and starts fading when that popup is
/// ready, so both fade in together.
pub(super) unsafe fn restart_fade(hwnd: HWND) {
    let s = state(hwnd);
    if s.is_null() || (*s).closing {
        return;
    }
    (*s).anim.rebase(OPACITY, Instant::now());
    (*s).render();
}

/// Destroy a popup at once (no exit fade).
pub(super) unsafe fn destroy(hwnd: HWND) {
    if !state(hwnd).is_null() {
        DestroyWindow(hwnd);
    }
}

/// Jump a popup's animations to their end (previews).
pub(super) unsafe fn settle(hwnd: HWND) {
    let s = state(hwnd);
    if s.is_null() {
        return;
    }
    (*s).anim.finish_all();
    (*s).compose = true;
    (*s).render();
}

/// The popup's content box and opacity (tests).
#[cfg(test)]
pub(super) unsafe fn geometry(hwnd: HWND) -> Option<(RECT, f32)> {
    let s = state(hwnd);
    (!s.is_null()).then(|| ((*s).content_screen(), (*s).anim.value(OPACITY)))
}

/// Composite popups over a capture of the owner's client area (`dib`, whose
/// pixel 0,0 is the client point `at`), as DWM shows them (previews).
/// Popups are settled first.
pub(super) unsafe fn composite(owner: HWND, dib: &mut gfx::Dib, at: POINT, popups: &[HWND]) {
    let mut origin = POINT { x: 0, y: 0 };
    ClientToScreen(owner, &mut origin);
    origin.x += at.x;
    origin.y += at.y;
    for &hwnd in popups {
        let s = state(hwnd);
        if s.is_null() {
            continue;
        }
        settle(hwnd);
        let margins = (*s).surface.margins();
        let size = (*s).surface.size();
        let alpha = ((*s).anim.value(OPACITY).clamp(0.0, 1.0) * 255.0).round() as u8;
        GdiAlphaBlend(
            dib.dc(),
            (*s).origin.x - margins.left - origin.x,
            (*s).origin.y - margins.top - origin.y,
            size.cx,
            size.cy,
            (*s).surface.dc(),
            0,
            0,
            size.cx,
            size.cy,
            BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
            },
        );
    }
}

/// The owner-client box a popup shows in, with the visible part of its
/// shadow (previews extend their canvas to it).
pub(super) unsafe fn client_bounds(owner: HWND, hwnd: HWND) -> Option<RECT> {
    let s = state(hwnd);
    // The scrim covers the window itself, never more.
    if s.is_null() || matches!((*s).kind, Kind::Scrim) {
        return None;
    }
    let mut origin = POINT { x: 0, y: 0 };
    ClientToScreen(owner, &mut origin);
    let r = (*s).content_screen();
    let d = gfx::pxi((*s).dpi, 24.0);
    Some(RECT {
        left: r.left - origin.x - d,
        top: r.top - origin.y - d,
        right: r.right - origin.x + d,
        bottom: r.bottom - origin.y + 2 * d,
    })
}

/// Every popup window of this thread (theme changes; tests: nothing may be
/// left over).
pub(super) unsafe fn thread_popups() -> Vec<HWND> {
    unsafe extern "system" fn collect(hwnd: HWND, data: LPARAM) -> i32 {
        if !state(hwnd).is_null() {
            (*(data as *mut Vec<HWND>)).push(hwnd);
        }
        1
    }
    let mut found: Vec<HWND> = Vec::new();
    EnumThreadWindows(
        windows_sys::Win32::System::Threading::GetCurrentThreadId(),
        Some(collect),
        &mut found as *mut Vec<HWND> as LPARAM,
    );
    found
}

/// Run every popup of this thread to the end of its fades and holds, like
/// its timers would over the next seconds (tests).
#[cfg(test)]
pub(super) unsafe fn finish_closing() {
    let now = Instant::now();
    for step in 1..=3 {
        for hwnd in thread_popups() {
            advance(hwnd, now + Duration::from_secs(5 * step));
        }
    }
}

// The composed premultiplied image of a popup (tests).
#[cfg(test)]
pub(super) unsafe fn pixel(hwnd: HWND, x: i32, y: i32) -> Option<u32> {
    let s = state(hwnd);
    (!s.is_null()).then(|| (*s).surface.dib().pixel(x, y))
}

/// The content box inside the popup's surface (tests).
#[cfg(test)]
pub(super) unsafe fn surface_content(hwnd: HWND) -> Option<RECT> {
    let s = state(hwnd);
    (!s.is_null()).then(|| (*s).surface.content())
}

// ───────────────────────────── menus & dropdowns ─────────────────────────────

/// Measure a menu: its box size and the item rows (content-relative).
fn layout_menu(pt: &Painter, items: &[MenuItem], style: &MenuStyle) -> (SIZE, Vec<RECT>, bool) {
    let hair = pt.hair() as i32;
    let pad = pt.pxi(MENU_PAD);
    let gutter = style
        .gutter
        .unwrap_or_else(|| items.iter().any(|item| item.checked));
    let font = style.font.get(pt.fonts);
    let row = pt.pxi(style.item);
    let separator = pt.pxi(9.0);
    let mut width = style.min_width;
    let mut y = hair + pad;
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let height = if item.separator { separator } else { row };
        rows.push(RECT {
            left: hair + pad,
            top: y,
            right: 0,
            bottom: y + height,
        });
        y += height;
        if item.separator {
            continue;
        }
        let mut need = 2 * pt.pxi(style.pad_x) + pt.measure(font, &item.label).cx;
        if gutter {
            need += pt.pxi(GUTTER);
        }
        if let Some(hint) = &item.hint {
            need += pt.pxi(24.0) + widgets::kbd_size(pt, hint).cx;
        }
        if item.has_submenu() {
            need += pt.pxi(24.0);
        }
        width = width.max(need + 2 * (hair + pad));
    }
    let width = width.min(pt.pxi(MENU_MAX_WIDTH).max(style.min_width));
    for r in &mut rows {
        r.right = width - hair - pad;
    }
    (
        SIZE {
            cx: width,
            cy: y + pad + hair,
        },
        rows,
        gutter,
    )
}

/// `.ctx button`: 32 px rows, hover / keyboard-current fg_sel (radius 4,
/// cross-faded), separators 1 px border with 4 px margins, `.kbd` hints on
/// the right, submenu chevrons, check marks, disabled items muted.
unsafe fn paint_menu(
    pt: &Painter,
    content: RECT,
    view: &MenuView,
    anim: &anim::AnimHost<(usize, u32)>,
) {
    let c = &pt.c;
    let font = view.style.font.get(pt.fonts);
    let hair = pt.hair() as i32;
    let pad_x = pt.pxi(view.style.pad_x);
    for (i, (item, row)) in view.items.iter().zip(&view.rows).enumerate() {
        let r = offset(*row, content.left, content.top);
        if item.separator {
            let top = r.top + pt.pxi(4.0);
            pt.fill(
                RECT {
                    top,
                    bottom: top + hair,
                    ..r
                },
                c.border,
            );
            continue;
        }
        let t = if view.open == Some(i) {
            1.0
        } else {
            anim.value(hover(i)).clamp(0.0, 1.0)
        };
        if t > 0.0 && item.enabled {
            pt.canvas.fill_round_rect(
                RectF::from_rect(r),
                pt.px(theme::RADIUS_SM),
                solid(mix(c.surface, c.fg_sel, t)),
            );
        }
        let fg = if item.enabled { c.fg } else { c.muted };
        let cy = (r.top + r.bottom) as f32 / 2.0;
        let mut left = r.left + pad_x;
        if view.gutter {
            if item.checked {
                check_mark(pt, left as f32, cy, fg);
            }
            left += pt.pxi(GUTTER);
        }
        let mut right = r.right - pad_x;
        if item.has_submenu() {
            let size = pt.px(10.0);
            pt.canvas.chevron(
                right as f32 - size / 2.0,
                cy,
                size,
                0.0,
                pt.px(1.5),
                solid(if item.enabled {
                    c.muted
                } else {
                    theme::disabled(c.muted, c.surface)
                }),
            );
            right -= pt.pxi(24.0);
        }
        if let Some(hint) = &item.hint {
            let badge = widgets::kbd(pt, right, (r.top + r.bottom) / 2, hint);
            right = badge.left - pt.pxi(12.0);
        }
        let cell = pt.css_rect(font, r, Some(view.style.font.line()));
        pt.text(
            font,
            fg,
            &item.label,
            RECT {
                left,
                right: right.max(left),
                ..cell
            },
            DT_SINGLELINE | DT_END_ELLIPSIS | DT_LEFT,
        );
    }
}

/// A 16 px check mark (stroke 1.5) with its left edge at `x`.
fn check_mark(pt: &Painter, x: f32, cy: f32, color: u32) {
    let u = pt.px(1.0);
    pt.canvas.polyline(
        &[
            (x + 3.0 * u, cy + 0.5 * u),
            (x + 6.5 * u, cy + 4.0 * u),
            (x + 13.0 * u, cy - 3.5 * u),
        ],
        pt.px(1.5),
        solid(color),
    );
}

/// Open a menu-like popup (one menu level or a select's dropdown) at
/// `anchor` with `hot` highlighted (no fade for the initial highlight).
pub(super) unsafe fn open_list(
    app: *mut App,
    items: Vec<MenuItem>,
    style: MenuStyle,
    anchor: Anchor,
    hot: Option<usize>,
) -> HWND {
    if app.is_null() {
        return null_mut();
    }
    open_list_on(Host::main(app), items, style, anchor, hot)
}

/// [`open_list`] for `host`.
unsafe fn open_list_on(
    host: Host,
    items: Vec<MenuItem>,
    style: MenuStyle,
    anchor: Anchor,
    hot: Option<usize>,
) -> HWND {
    if host.hwnd.is_null() || items.is_empty() {
        return null_mut();
    }
    let dpi = host.dpi;
    let (size, rows, gutter) = {
        let dc = MeasureDc::new();
        if dc.0.is_null() {
            return null_mut();
        }
        let pt = Painter::new(dc.0, dpi, &*host.fonts);
        layout_menu(&pt, &items, &style)
    };
    let hot = hot.filter(|&i| items.get(i).is_some_and(MenuItem::selectable));
    let reference = match anchor {
        Anchor::Below { r, .. } | Anchor::Above { r, .. } | Anchor::Center(r) => r,
        Anchor::Beside { item, .. } => item,
        Anchor::Point(pt) => RECT {
            left: pt.x,
            top: pt.y,
            right: pt.x + 1,
            bottom: pt.y + 1,
        },
    };
    let inset = gfx::hairline(dpi) as i32 + gfx::pxi(dpi, MENU_PAD);
    let (origin, slide) = place(
        anchor,
        size.cx,
        size.cy,
        work_area(reference),
        gfx::pxi(dpi, GAP),
        inset,
    );
    let view = MenuView {
        items,
        style,
        gutter,
        rows,
        hot,
        open: None,
    };
    let hwnd = create(
        host,
        Kind::Menu(view),
        size,
        origin,
        slide as f32 * gfx::px(dpi, anim::motion::POPUP_SLIDE_DIP),
        true,
        gfx::px(dpi, style.radius),
        0,
        anim::motion::POPUP_IN,
    );
    let s = state(hwnd);
    if !s.is_null() {
        if let Some(i) = hot {
            (*s).anim.set(hover(i), 1.0);
            (*s).compose = true;
            (*s).render();
        }
        (*s).announce_hot();
    }
    hwnd
}

unsafe fn menu_view(hwnd: HWND) -> Option<(*mut Popup, *mut MenuView)> {
    let s = state(hwnd);
    if s.is_null() {
        return None;
    }
    match &mut (*s).kind {
        Kind::Menu(view) => Some((s, view as *mut MenuView)),
        _ => None,
    }
}

/// The highlighted item of a menu / dropdown popup.
pub(super) unsafe fn menu_hot(hwnd: HWND) -> Option<usize> {
    menu_view(hwnd).and_then(|(_, view)| (*view).hot)
}

/// Highlight `hot` (cross-fading from the previous item).
pub(super) unsafe fn menu_set_hot(hwnd: HWND, hot: Option<usize>) {
    let Some((popup, view)) = menu_view(hwnd) else {
        return;
    };
    let hot = hot.filter(|&i| (&(*view).items).get(i).is_some_and(MenuItem::selectable));
    if (*view).hot == hot {
        return;
    }
    let old = std::mem::replace(&mut (*view).hot, hot);
    if let Some(old) = old {
        if (*view).open != Some(old) {
            (*popup).fade_hover(old, false);
        }
    }
    if let Some(new) = hot {
        (*popup).fade_hover(new, true);
    }
    (*popup).compose = true;
    (*popup).render();
    (*popup).announce_hot();
}

/// Hit test at a screen point: None = outside the popup's box,
/// Some(None) = inside but not on a selectable row.
pub(super) unsafe fn menu_hit(hwnd: HWND, pt: POINT) -> Option<Option<usize>> {
    let (popup, view) = menu_view(hwnd)?;
    let content = (*popup).content_screen();
    if !contains(&content, pt) {
        return None;
    }
    let local = POINT {
        x: pt.x - content.left,
        y: pt.y - content.top,
    };
    Some(
        (*view)
            .rows
            .iter()
            .position(|row| contains(row, local))
            .filter(|&i| !(&(*view).items)[i].separator),
    )
}

/// The screen rectangle of a menu row (tests).
#[cfg(test)]
pub(super) unsafe fn row_rect(hwnd: HWND, index: usize) -> Option<RECT> {
    let (popup, view) = menu_view(hwnd)?;
    let content = (*popup).content_screen();
    (&(*view).rows)
        .get(index)
        .map(|r| offset(*r, content.left, content.top))
}

/// Advance a popup's animations to `now` like its timers would (tests).
#[cfg(test)]
pub(super) unsafe fn advance(hwnd: HWND, now: Instant) {
    let s = state(hwnd);
    if s.is_null() {
        return;
    }
    (*s).anim.start_due_at(now);
    (*s).anim.tick_at(now);
    (*s).compose = true;
    (*s).render();
    (*s).after_frame();
}

/// Move a dropdown with its select (the page head re-flows while it is open).
pub(super) unsafe fn move_list(hwnd: HWND, anchor: Anchor) {
    let s = state(hwnd);
    if s.is_null() || (*s).closing {
        return;
    }
    let dpi = (*s).dpi;
    let reference = match anchor {
        Anchor::Below { r, .. } | Anchor::Above { r, .. } | Anchor::Center(r) => r,
        Anchor::Beside { item, .. } => item,
        Anchor::Point(pt) => RECT {
            left: pt.x,
            top: pt.y,
            right: pt.x + 1,
            bottom: pt.y + 1,
        },
    };
    let (origin, _) = place(
        anchor,
        (*s).size.cx,
        (*s).size.cy,
        work_area(reference),
        gfx::pxi(dpi, GAP),
        0,
    );
    if origin.x != (*s).origin.x || origin.y != (*s).origin.y {
        (*s).origin = origin;
        (*s).render();
    }
}

/// Outcome of a key in a menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Flow {
    Continue,
    Choose(usize),
    Cancel,
}

/// An open menu: level 0 plus the open submenus (each its own popup).
pub(super) struct MenuSession {
    host: Host,
    pub(super) levels: Vec<HWND>,
}

impl MenuSession {
    pub(super) unsafe fn open(
        app: *mut App,
        items: Vec<MenuItem>,
        anchor: Anchor,
        hot: Option<usize>,
    ) -> Option<Self> {
        Self::open_on(Host::main(app), items, anchor, hot)
    }
    /// [`MenuSession::open`] for `host`.
    pub(super) unsafe fn open_on(
        host: Host,
        items: Vec<MenuItem>,
        anchor: Anchor,
        hot: Option<usize>,
    ) -> Option<Self> {
        let style = MenuStyle::menu(host.dpi);
        let hwnd = open_list_on(host, items, style, anchor, hot);
        (!hwnd.is_null()).then(|| Self {
            host,
            levels: vec![hwnd],
        })
    }
    fn deepest(&self) -> usize {
        self.levels.len() - 1
    }
    pub(super) unsafe fn hot(&self, level: usize) -> Option<usize> {
        menu_hot(self.levels[level])
    }
    unsafe fn open_item(&self, level: usize) -> Option<usize> {
        menu_view(self.levels[level]).and_then(|(_, view)| (*view).open)
    }
    unsafe fn items(&self, level: usize) -> Vec<MenuItem> {
        menu_view(self.levels[level]).map_or_else(Vec::new, |(_, view)| (*view).items.clone())
    }
    pub(super) unsafe fn set_hot(&mut self, level: usize, hot: Option<usize>) {
        menu_set_hot(self.levels[level], hot);
    }
    /// Open the submenu of `level`'s hot item (closing deeper levels).
    pub(super) unsafe fn open_submenu(&mut self, level: usize, select_first: bool) -> bool {
        let Some((popup, view)) = menu_view(self.levels[level]) else {
            return false;
        };
        let Some(index) = (*view).hot else {
            return false;
        };
        let item = (&(*view).items)[index].clone();
        if !item.has_submenu() || !item.enabled {
            return false;
        }
        if self.levels.len() > level + 1 {
            self.close_from(level + 1);
        }
        let content = (*popup).content_screen();
        let row = offset((&(*view).rows)[index], content.left, content.top);
        let hot = if select_first {
            step_item(&item.submenu, None, true)
        } else {
            None
        };
        let hwnd = open_list_on(
            self.host,
            item.submenu,
            MenuStyle::menu(self.host.dpi),
            Anchor::Beside {
                item: row,
                parent: content,
            },
            hot,
        );
        if hwnd.is_null() {
            return false;
        }
        (*view).open = Some(index);
        (*popup).compose = true;
        (*popup).render();
        self.levels.push(hwnd);
        true
    }
    /// Close `levels[level..]` (fade out); the parent keeps its highlight.
    pub(super) unsafe fn close_from(&mut self, level: usize) {
        if level == 0 || level >= self.levels.len() {
            return;
        }
        for hwnd in self.levels.drain(level..).rev() {
            close(hwnd);
        }
        if let Some((popup, view)) = menu_view(self.levels[level - 1]) {
            if let Some(open) = (*view).open.take() {
                if (*view).hot != Some(open) {
                    (*popup).fade_hover(open, false);
                }
            }
            (*popup).compose = true;
            (*popup).render();
        }
    }
    /// Which level (deepest first) and item a screen point is on.
    pub(super) unsafe fn hit(&self, pt: POINT) -> Option<(usize, Option<usize>)> {
        (0..self.levels.len())
            .rev()
            .find_map(|level| menu_hit(self.levels[level], pt).map(|item| (level, item)))
    }
    /// Keyboard navigation (Up/Down/Home/End, Right/Enter open submenus,
    /// Left/Esc close them, Enter/Space choose, Esc/Alt/F10 cancel).
    pub(super) unsafe fn key(&mut self, vk: u16) -> Flow {
        let level = self.deepest();
        let items = self.items(level);
        let hot = self.hot(level);
        match vk {
            VK_DOWN => self.set_hot(level, step_item(&items, hot, true)),
            VK_UP => self.set_hot(level, step_item(&items, hot, false)),
            VK_HOME | VK_PRIOR => self.set_hot(level, step_item(&items, None, true)),
            VK_END | VK_NEXT => self.set_hot(level, step_item(&items, None, false)),
            VK_RIGHT => {
                self.open_submenu(level, true);
            }
            VK_LEFT => self.close_from(level),
            VK_ESCAPE if level > 0 => self.close_from(level),
            VK_ESCAPE | VK_MENU | VK_F10 => return Flow::Cancel,
            VK_RETURN | VK_SPACE => {
                if let Some(item) = hot.and_then(|i| items.get(i)) {
                    if item.has_submenu() {
                        self.open_submenu(level, true);
                    } else if item.enabled {
                        return Flow::Choose(item.id);
                    }
                }
            }
            _ => {}
        }
        Flow::Continue
    }
    /// First-letter jump in the deepest level.
    pub(super) unsafe fn letter(&mut self, letter: char) {
        let level = self.deepest();
        let items = self.items(level);
        if let Some(i) = letter_item(&items, self.hot(level), letter) {
            self.set_hot(level, Some(i));
        }
    }
    /// After the hover delay: close submenus the pointer left, open the
    /// hovered item's submenu.
    pub(super) unsafe fn sync(&mut self, level: usize) {
        if level >= self.levels.len() {
            return;
        }
        let hot = self.hot(level);
        if self.levels.len() > level + 1 && self.open_item(level) != hot {
            self.close_from(level + 1);
        }
        let submenu = hot
            .and_then(|i| self.items(level).get(i).cloned())
            .is_some_and(|item| item.has_submenu() && item.enabled);
        if submenu && self.levels.len() == level + 1 {
            self.open_submenu(level, false);
        }
    }
    /// Fade every level out.
    pub(super) unsafe fn close(self) {
        for hwnd in self.levels.into_iter().rev() {
            close(hwnd);
        }
    }
    /// Destroy every level at once (previews).
    pub(super) unsafe fn destroy(self) {
        for hwnd in self.levels.into_iter().rev() {
            destroy(hwnd);
        }
    }
}

pub(super) enum Step {
    Pass,
    Consumed,
    Done,
}

/// A local message loop: `step` sees every message first (Pass = dispatch
/// normally). Ends when `step` says Done, `alive` fails or on WM_QUIT (which
/// is re-posted for the outer loop).
pub(super) unsafe fn run_modal(
    mut step: impl FnMut(&MSG) -> Step,
    mut alive: impl FnMut() -> bool,
) {
    // Tests drive the loops with posted keys; never hang a test run.
    #[cfg(test)]
    let watchdog = SetTimer(null_mut(), 0, 10_000, None);
    let mut msg: MSG = zeroed();
    while alive() {
        let got = GetMessageW(&mut msg, null_mut(), 0, 0);
        if got == 0 {
            PostQuitMessage(msg.wParam as i32);
            break;
        }
        if got < 0 {
            break;
        }
        #[cfg(test)]
        if msg.message == WM_TIMER && msg.hwnd.is_null() && msg.wParam == watchdog {
            break;
        }
        match step(&msg) {
            Step::Pass => {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            Step::Consumed => {}
            Step::Done => break,
        }
    }
    #[cfg(test)]
    KillTimer(null_mut(), watchdog);
}

/// The screen position of a mouse message.
pub(super) unsafe fn message_point(msg: &MSG) -> POINT {
    let mut pt = POINT {
        x: (msg.lParam & 0xffff) as i16 as i32,
        y: ((msg.lParam >> 16) & 0xffff) as i16 as i32,
    };
    if !matches!(msg.message, WM_MOUSEWHEEL | WM_MOUSEHWHEEL) && !msg.hwnd.is_null() {
        ClientToScreen(msg.hwnd, &mut pt);
    }
    pt
}

/// The character a key press types in the thread's keyboard layout, for
/// the first-letter jump. The menu loop consumes its keys without
/// TranslateMessage (arrows, Enter and Esc must not reach the owner), so no
/// WM_CHAR would follow a real WM_KEYDOWN; the key is mapped here instead
/// (ToUnicode flag 4: dead-key state untouched). Control characters
/// (Enter, Esc, Tab, Ctrl+letter) are not letters.
unsafe fn key_letter(msg: &MSG) -> Option<char> {
    let vk = msg.wParam as u32;
    let mut scan = ((msg.lParam >> 16) & 0xff) as u32;
    if scan == 0 {
        scan = MapVirtualKeyW(vk, MAPVK_VK_TO_VSC);
    }
    let mut keys = [0u8; 256];
    if GetKeyboardState(keys.as_mut_ptr()) == 0 {
        return None;
    }
    let mut typed = [0u16; 8];
    let count = ToUnicode(
        vk,
        scan,
        keys.as_ptr(),
        typed.as_mut_ptr(),
        typed.len() as i32,
        4,
    );
    if count != 1 {
        return None;
    }
    char::from_u32(typed[0] as u32).filter(|ch| !ch.is_control() && !ch.is_whitespace())
}

fn menu_show_delay() -> u32 {
    let mut delay: u32 = 400;
    unsafe {
        SystemParametersInfoW(SPI_GETMENUSHOWDELAY, 0, (&mut delay as *mut u32).cast(), 0);
    }
    delay.clamp(50, 1000)
}

/// Show `menu` (a native HMENU model, see [`menu_items`]) as a layered
/// popup menu at `anchor` and return the chosen command id (0 = cancelled),
/// like `TrackPopupMenu(TPM_RETURNCMD)`. Opened from the keyboard (focus
/// cues shown), the first item is highlighted. The caller owns the HMENU.
pub(super) unsafe fn track_menu(p: *mut App, menu: HMENU, anchor: Anchor) -> usize {
    track_menu_on(Host::main(p), menu, anchor)
}

/// [`track_menu`] over `host` (another Feather window): the menu is owned by
/// it, drawn in its DPI and fonts, and closes when it moves.
pub(super) unsafe fn track_menu_on(host: Host, menu: HMENU, anchor: Anchor) -> usize {
    let owner = host.hwnd;
    let items = menu_items(menu);
    if items.is_empty() || owner.is_null() {
        return 0;
    }
    let keyboard = !super::controls::cues_hidden(owner);
    let hot = if keyboard {
        step_item(&items, None, true)
    } else {
        None
    };
    let Some(mut session) = MenuSession::open_on(host, items, anchor, hot) else {
        return 0;
    };
    let capture = session.levels[0];
    SetCapture(capture);
    let captured = GetCapture() == capture;
    let root = GetAncestor(owner, GA_ROOT);
    let foreground = GetForegroundWindow() == root;
    let delay = menu_show_delay();
    let mut chosen = 0;
    let mut pressed = false;
    let mut pending: Option<usize> = None;
    let geometry = || unsafe { rect_key(visible_bounds(owner), client_screen(owner)) };
    let placed = geometry();
    let session_ptr = &mut session as *mut MenuSession;
    run_modal(
        |msg| {
            // The window moved or resized under the menu (Win+Arrow): the
            // menu closes, like a native one.
            if geometry() != placed {
                return Step::Done;
            }
            let session = &mut *session_ptr;
            let schedule = |pending: &mut Option<usize>, level: Option<usize>| {
                if *pending == level {
                    return;
                }
                *pending = level;
                if level.is_some() {
                    SetTimer(capture, SUBMENU_TIMER, delay, None);
                } else {
                    KillTimer(capture, SUBMENU_TIMER);
                }
            };
            match msg.message {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    let vk = msg.wParam as u16;
                    if msg.message == WM_SYSKEYDOWN && vk == VK_F4 {
                        return Step::Done;
                    }
                    match session.key(vk) {
                        Flow::Choose(id) => {
                            chosen = id;
                            Step::Done
                        }
                        Flow::Cancel => Step::Done,
                        Flow::Continue => {
                            schedule(&mut pending, None);
                            if let Some(letter) = key_letter(msg) {
                                session.letter(letter);
                            }
                            Step::Consumed
                        }
                    }
                }
                // Characters that arrive as such (IME commits, posted).
                WM_CHAR | WM_SYSCHAR => {
                    if let Some(letter) = char::from_u32(msg.wParam as u32) {
                        if !letter.is_control() {
                            session.letter(letter);
                        }
                    }
                    Step::Consumed
                }
                WM_KEYUP | WM_SYSKEYUP | WM_DEADCHAR | WM_SYSDEADCHAR => Step::Consumed,
                WM_MOUSEMOVE => {
                    let pt = message_point(msg);
                    // Captured: no WM_SETCURSOR arrives; set the pointer here.
                    let over = session
                        .hit(pt)
                        .is_some_and(|(level, _)| pointer_at(session.levels[level], pt));
                    widgets::set_pointer(over);
                    match session.hit(pt) {
                        Some((level, item)) => {
                            if level > 0 {
                                // Inside a submenu: its parent keeps the open item.
                                let parent = level - 1;
                                let open = session.open_item(parent);
                                if session.hot(parent) != open {
                                    session.set_hot(parent, open);
                                }
                                if pending == Some(parent) {
                                    schedule(&mut pending, None);
                                }
                            }
                            if let Some(i) = item {
                                if session.hot(level) != Some(i) {
                                    session.set_hot(level, Some(i));
                                }
                            }
                            let hot = session.hot(level);
                            let deeper_stale =
                                session.levels.len() > level + 1 && session.open_item(level) != hot;
                            let opens = hot
                                .and_then(|i| session.items(level).get(i).cloned())
                                .is_some_and(|item| item.has_submenu() && item.enabled)
                                && session.levels.len() == level + 1;
                            if deeper_stale || opens {
                                schedule(&mut pending, Some(level));
                            } else if pending == Some(level) {
                                schedule(&mut pending, None);
                            }
                        }
                        None => {
                            // Outside: a leaf level loses its highlight.
                            let deepest = session.deepest();
                            if session.open_item(deepest).is_none() {
                                session.set_hot(deepest, None);
                            }
                        }
                    }
                    Step::Consumed
                }
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                | WM_LBUTTONDBLCLK | WM_RBUTTONDBLCLK | WM_MBUTTONDBLCLK | WM_NCLBUTTONDOWN
                | WM_NCRBUTTONDOWN | WM_NCMBUTTONDOWN => {
                    let pt = message_point(msg);
                    match session.hit(pt) {
                        None => Step::Done,
                        Some((level, item)) => {
                            pressed = true;
                            if let Some(i) = item {
                                session.set_hot(level, Some(i));
                                if session
                                    .items(level)
                                    .get(i)
                                    .is_some_and(MenuItem::has_submenu)
                                {
                                    schedule(&mut pending, None);
                                    session.open_submenu(level, false);
                                }
                            }
                            Step::Consumed
                        }
                    }
                }
                WM_LBUTTONUP | WM_RBUTTONUP => {
                    let pt = message_point(msg);
                    if let Some((level, Some(i))) = session.hit(pt) {
                        if let Some(item) = session.items(level).get(i) {
                            if pressed && item.enabled && !item.has_submenu() {
                                chosen = item.id;
                                return Step::Done;
                            }
                        }
                    }
                    pressed = false;
                    Step::Consumed
                }
                WM_MBUTTONUP | WM_XBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_NCMOUSEMOVE
                | WM_NCLBUTTONUP | WM_NCRBUTTONUP => Step::Consumed,
                WM_TIMER if msg.hwnd == capture && msg.wParam == SUBMENU_TIMER => {
                    KillTimer(capture, SUBMENU_TIMER);
                    if let Some(level) = pending.take() {
                        session.sync(level);
                    }
                    Step::Consumed
                }
                _ => Step::Pass,
            }
        },
        || {
            IsWindow(capture) != 0
                && (!captured || GetCapture() == capture)
                && (!foreground || GetForegroundWindow() == root)
        },
    );
    KillTimer(capture, SUBMENU_TIMER);
    if GetCapture() == capture {
        ReleaseCapture();
    }
    session.close();
    chosen
}

// ───────────────────────────── confirm dialog ─────────────────────────────

/// The content of a confirm dialog (`.dialog`).
pub(super) struct ConfirmSpec<'a> {
    /// `h2` (18 / 600).
    pub title: &'a str,
    /// `p` (13 muted); blank-line separated paragraphs, `\n` line breaks.
    pub body: &'a str,
    /// Optional `.warn-line` (warn_fg), e.g. for Windows processes.
    pub warn: Option<&'a str>,
    /// The action button (danger or primary) and the cancel button.
    pub action: &'a str,
    pub cancel: &'a str,
    pub danger: bool,
}

/// A MessageBox-style prompt → (h2, body): the first sentence up to its
/// question mark is the heading, the rest the text.
pub(super) fn split_prompt(prompt: &str) -> (String, String) {
    let prompt = prompt.trim();
    let (first, rest) = match prompt.split_once("\n\n") {
        Some((first, rest)) => (first, Some(rest)),
        None => (prompt, None),
    };
    let (title, lead) = match first.find('?') {
        Some(i) => (&first[..=i], first[i + 1..].trim()),
        None => (first, ""),
    };
    let mut body = lead.to_owned();
    if let Some(rest) = rest {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(rest.trim());
    }
    (title.trim().to_owned(), body)
}

/// Per-UTF-16-unit advances of `text` in `font` at its fractional
/// (unhinted) widths — measured at 16× the size like `fonts::ideal_width`
/// — with every glyph position rounded, so the drawn line is
/// `round(ideal width)` wide: never wider than the `ceil(ideal width)` that
/// [`wrap`] broke the lines with. GDI's own hinted advances are ~2–3 %
/// wider (Segoe UI Variable Text 13 px: 398 px for a line that is 389 px in
/// the reference) and would push the last word into an ellipsis. None for
/// text with Hangul, which is measured and drawn by the fallback renderer.
unsafe fn ideal_advances(dc: HDC, font: HFONT, text: &str) -> Option<Vec<i32>> {
    const SCALE: i32 = 16;
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.is_empty() || units.iter().copied().any(fonts::is_hangul) {
        return None;
    }
    let old = SelectObject(dc, font);
    let mut metrics: TEXTMETRICW = zeroed();
    let mut logfont: LOGFONTW = zeroed();
    let large = if GetTextMetricsW(dc, &mut metrics) != 0
        && GetObjectW(
            font,
            size_of::<LOGFONTW>() as i32,
            (&mut logfont as *mut LOGFONTW).cast(),
        ) != 0
        && metrics.tmHeight > metrics.tmInternalLeading
    {
        logfont.lfHeight = -(metrics.tmHeight - metrics.tmInternalLeading) * SCALE;
        logfont.lfWidth = 0;
        CreateFontIndirectW(&logfont)
    } else {
        null_mut()
    };
    let mut advances = None;
    if !large.is_null() {
        SelectObject(dc, large);
        let mut extents = vec![0i32; units.len()];
        let mut size: SIZE = zeroed();
        if GetTextExtentExPointW(
            dc,
            units.as_ptr(),
            units.len() as i32,
            0,
            null_mut(),
            extents.as_mut_ptr(),
            &mut size,
        ) != 0
        {
            let mut pen = 0;
            advances = Some(
                extents
                    .iter()
                    .map(|&extent| {
                        let x = (extent as f32 / SCALE as f32).round() as i32;
                        let advance = x - pen;
                        pen = x;
                        advance
                    })
                    .collect(),
            );
        }
        SelectObject(dc, font);
        DeleteObject(large);
    }
    SelectObject(dc, old);
    advances
}

/// A wrapped line with its advances (see [`TextLine`]).
pub(super) unsafe fn text_line(dc: HDC, font: HFONT, text: String, top: i32) -> TextLine {
    let advances = ideal_advances(dc, font, &text);
    TextLine {
        text,
        top,
        advances,
    }
}

/// The drawn width of a wrapped line (tests: it must fit its box).
#[cfg(test)]
unsafe fn line_width(dc: HDC, font: HFONT, line: &TextLine) -> i32 {
    match &line.advances {
        Some(advances) => advances.iter().sum(),
        None => {
            let old = SelectObject(dc, font);
            let width = fonts::str_extent(dc, &line.text).cx;
            SelectObject(dc, old);
            width
        }
    }
}

/// Draw a wrapped line at the top-left of `cell` (its CSS line rect):
/// Latin text with its glyphs at the fractional advances `wrap` measured
/// (so it always fits and nothing is ellipsized), Hangul lines through the
/// fallback renderer they were measured with.
pub(super) unsafe fn draw_line(pt: &Painter, font: HFONT, color: u32, line: &TextLine, cell: RECT) {
    let Some(advances) = line.advances.as_ref().filter(|a| !a.is_empty()) else {
        pt.text(
            font,
            color,
            &line.text,
            cell,
            DT_SINGLELINE | DT_LEFT | DT_NOCLIP,
        );
        return;
    };
    pt.canvas.flush();
    let units: Vec<u16> = line.text.encode_utf16().collect();
    let old = SelectObject(pt.dc, font);
    SetTextColor(pt.dc, color);
    SetBkMode(pt.dc, TRANSPARENT as i32);
    let align = SetTextAlign(pt.dc, TA_LEFT | TA_TOP);
    // Unclipped glyphs at the given advances: the ink stays within the
    // advances plus a small overhang, inside the line's GDI cell.
    let pad = pt.pxi(3.0);
    let area = RECT {
        left: cell.left - pad,
        top: cell.top - pad,
        right: cell.left + advances.iter().sum::<i32>() + pad,
        bottom: cell.bottom + pad,
    };
    fonts::draw_contrasted(pt.dc, area, |dc, dx, dy| {
        ExtTextOutW(
            dc,
            cell.left + dx,
            cell.top + dy,
            0,
            null(),
            units.as_ptr(),
            units.len() as u32,
            advances.as_ptr(),
        )
    });
    SetTextAlign(pt.dc, align);
    SelectObject(pt.dc, old);
}

/// Greedy word wrap in `font` (Hangul measured in its fallback face):
/// `\n` breaks lines; words wider than `width` break between characters.
pub(super) unsafe fn wrap(dc: HDC, font: HFONT, text: &str, width: i32) -> Vec<String> {
    let old = SelectObject(dc, font);
    // Chromium breaks lines on fractional advances; Hangul (drawn with its
    // same-size fallback face) is measured the way it is drawn.
    let measure = |s: &str| unsafe {
        if s.encode_utf16().any(fonts::is_hangul) {
            fonts::str_extent(dc, s).cx
        } else {
            fonts::ideal_width(dc, s).ceil() as i32
        }
    };
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        for word in raw.split(' ').filter(|word| !word.is_empty()) {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if measure(&candidate) <= width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            if measure(word) <= width {
                line = word.to_owned();
                continue;
            }
            for ch in word.chars() {
                let mut next = line.clone();
                next.push(ch);
                if !line.is_empty() && measure(&next) > width {
                    lines.push(std::mem::take(&mut line));
                    next = ch.to_string();
                }
                line = next;
            }
        }
        lines.push(line);
    }
    SelectObject(dc, old);
    lines
}

/// Lay out `.dialog` (`.body` padding 22 24 18, h2 18/1.3 + 8, p 13/1.45,
/// `.warn-line` margin-top 12, `.foot` padding 14 24 with a top border).
unsafe fn layout_dialog(host: Host, spec: &ConfirmSpec, width: i32) -> (DialogView, SIZE) {
    let dpi = host.dpi;
    let f = &*host.fonts;
    let px = |v: f32| gfx::px(dpi, v);
    let hair = gfx::hairline(dpi) as i32;
    let dc = MeasureDc::new();
    let text_left = hair + gfx::pxi(dpi, 24.0);
    let text_right = width - hair - gfx::pxi(dpi, 24.0);
    let text_width = (text_right - text_left).max(gfx::pxi(dpi, 80.0));
    let title_line = px(18.0 * 1.3);
    let body_line = px(13.0 * 1.45);
    let mut y = hair as f32 + px(22.0);
    let mut title = Vec::new();
    for line in wrap(dc.0, f.dialog_title, spec.title, text_width) {
        title.push(text_line(dc.0, f.dialog_title, line, y.round() as i32));
        y += title_line;
    }
    let mut body = Vec::new();
    let paragraphs: Vec<&str> = spec
        .body
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    if !paragraphs.is_empty() {
        y += px(8.0);
    }
    for (i, paragraph) in paragraphs.iter().enumerate() {
        if i > 0 {
            y += px(8.0);
        }
        for line in wrap(dc.0, f.ui, paragraph, text_width) {
            body.push(text_line(dc.0, f.ui, line, y.round() as i32));
            y += body_line;
        }
    }
    let mut warn = Vec::new();
    if let Some(text) = spec.warn.filter(|w| !w.trim().is_empty()) {
        y += px(12.0);
        for line in wrap(dc.0, f.ui, text, text_width) {
            warn.push(text_line(dc.0, f.ui, line, y.round() as i32));
            y += body_line;
        }
    }
    y += px(18.0);
    let foot = y.round() as i32;
    let button_h = gfx::pxi(dpi, 32.0);
    let button_top = foot + hair + gfx::pxi(dpi, 14.0);
    // Chromium sizes the buttons by the labels' fractional widths (GDI's
    // hinted widths made them 1–2 px wider than the reference's).
    let label_width = |font: HFONT, text: &str| unsafe {
        let old = SelectObject(dc.0, font);
        let w = if text.encode_utf16().any(fonts::is_hangul) {
            fonts::str_extent(dc.0, text).cx
        } else {
            fonts::ideal_width(dc.0, text).round() as i32
        };
        SelectObject(dc.0, old);
        w + 2 * gfx::pxi(dpi, 14.0) + 2 * hair
    };
    let action_w = label_width(f.ui_strong, spec.action);
    let cancel_w = label_width(f.ui, spec.cancel);
    let right = width - hair - gfx::pxi(dpi, 24.0);
    let action = RECT {
        left: right - action_w,
        top: button_top,
        right,
        bottom: button_top + button_h,
    };
    let cancel_right = action.left - gfx::pxi(dpi, 8.0);
    let cancel = RECT {
        left: cancel_right - cancel_w,
        top: button_top,
        right: cancel_right,
        bottom: button_top + button_h,
    };
    let height = button_top + button_h + gfx::pxi(dpi, 14.0) + hair;
    (
        DialogView {
            title,
            body,
            warn,
            labels: [spec.cancel.to_owned(), spec.action.to_owned()],
            action: if spec.danger {
                ButtonStyle::Danger
            } else {
                ButtonStyle::Primary
            },
            buttons: [cancel, action],
            foot,
            text_left,
            text_right,
            focus: 0,
            ring: false,
            pressed: None,
        },
        SIZE {
            cx: width,
            cy: height,
        },
    )
}

unsafe fn paint_dialog(
    pt: &Painter,
    content: RECT,
    view: &DialogView,
    anim: &anim::AnimHost<(usize, u32)>,
    radius: f32,
) {
    let c = &pt.c;
    let f = pt.fonts;
    let hair = pt.hair();
    // `.foot`: bg with a top border, the outer bottom corners rounded.
    let foot_top = content.top + view.foot;
    pt.canvas.fill_round_rect_corners(
        RectF::ltrb(
            content.left as f32 + hair,
            foot_top as f32,
            content.right as f32 - hair,
            content.bottom as f32 - hair,
        ),
        gfx::Radii::bottom((radius - hair).max(0.0)),
        solid(c.bg),
    );
    pt.fill(
        RECT {
            left: content.left + hair as i32,
            top: foot_top,
            right: content.right - hair as i32,
            bottom: foot_top + hair as i32,
        },
        c.border,
    );
    let band = |top: i32, line: f32| RECT {
        left: content.left + view.text_left,
        top: content.top + top,
        right: content.left + view.text_right,
        bottom: content.top + top + pt.pxi(line),
    };
    // Wrapped lines always fit their box (see `draw_line`): no ellipsis.
    for line in &view.title {
        let cell = pt.css_rect(f.dialog_title, band(line.top, 18.0 * 1.3), Some(18.0 * 1.3));
        draw_line(pt, f.dialog_title, c.fg, line, cell);
    }
    for (lines, color) in [(&view.body, c.muted), (&view.warn, c.warn_fg)] {
        for line in lines {
            let cell = pt.css_rect(f.ui, band(line.top, 13.0 * 1.45), Some(13.0 * 1.45));
            draw_line(pt, f.ui, color, line, cell);
        }
    }
    for (i, r) in view.buttons.iter().enumerate() {
        let r = offset(*r, content.left, content.top);
        let style = if i == 0 {
            ButtonStyle::Default
        } else {
            view.action
        };
        widgets::button_face(
            pt,
            r,
            style,
            &ButtonState {
                hover: anim.value(hover(i)),
                pressed: view.pressed == Some(i),
                ..ButtonState::default()
            },
            &view.labels[i],
            c.bg,
        );
        if view.ring && view.focus == i {
            widgets::focus_ring(pt, r, pt.px(theme::RADIUS_SM));
        }
    }
}

/// Open the scrim (fg @ 22 % over the owner window) and the centred dialog
/// without running the modal loop (previews; [`confirm_dialog`] runs it).
pub(super) unsafe fn stage_confirm(p: *mut App, spec: &ConfirmSpec) -> Option<(HWND, HWND)> {
    stage_confirm_on(Host::main(p), spec)
}

/// [`stage_confirm`] over `host`.
pub(super) unsafe fn stage_confirm_on(host: Host, spec: &ConfirmSpec) -> Option<(HWND, HWND)> {
    let owner = host.hwnd;
    if owner.is_null() {
        return None;
    }
    let dpi = host.dpi;
    let scrim = open_scrim_on(host, null_mut());
    let client = client_screen(owner);
    let width = gfx::pxi(dpi, DIALOG_WIDTH)
        .min(client.right - client.left - gfx::pxi(dpi, 32.0))
        .max(gfx::pxi(dpi, 280.0));
    let (mut view, size) = layout_dialog(host, spec, width);
    // `:focus-visible` after a keyboard trigger (Del, Enter, Space) or
    // while keyboard cues are shown; hidden after a mouse click.
    view.ring = !super::controls::cues_hidden(owner)
        || [VK_DELETE, VK_RETURN, VK_SPACE]
            .iter()
            .any(|&vk| GetKeyState(vk as i32) < 0);
    let (origin, _) = place(
        Anchor::Center(client),
        size.cx,
        size.cy,
        work_area(client),
        0,
        0,
    );
    let dialog = create(
        host,
        Kind::Dialog(view),
        size,
        origin,
        gfx::px(dpi, anim::motion::POPUP_SLIDE_DIP),
        true,
        gfx::px(dpi, theme::RADIUS),
        0,
        anim::motion::POPUP_IN,
    );
    if dialog.is_null() {
        destroy(scrim);
        return None;
    }
    // The dialog was composed (shadow included) after the scrim appeared:
    // start both fades in the same frame.
    restart_fade(scrim);
    Some((scrim, dialog))
}

/// `.scrim`: fg @ 22 % over the whole owner window (its rounded corners
/// kept), fading in with the popup above it. A press on it closes
/// `dismiss` (WM_CLOSE) when given; modal loops handle it themselves.
pub(super) unsafe fn open_scrim(p: *mut App, dismiss: HWND) -> HWND {
    open_scrim_on(Host::main(p), dismiss)
}

unsafe fn open_scrim_on(host: Host, dismiss: HWND) -> HWND {
    let owner = host.hwnd;
    let bounds = visible_bounds(owner);
    let corner = if IsZoomed(owner) != 0 {
        0.0
    } else {
        gfx::px(host.dpi, theme::RADIUS)
    };
    let scrim = create(
        host,
        Kind::Scrim,
        SIZE {
            cx: bounds.right - bounds.left,
            cy: bounds.bottom - bounds.top,
        },
        POINT {
            x: bounds.left,
            y: bounds.top,
        },
        0.0,
        false,
        corner,
        0,
        anim::motion::POPUP_IN,
    );
    let s = state(scrim);
    if !s.is_null() {
        (*s).dismiss = dismiss;
    }
    scrim
}

/// Fit an open scrim and dialog to the owner's current geometry: the scrim
/// over the whole window (square corners when maximized), the dialog
/// centred on the client. The dialog is modal but the owner stays enabled
/// like the reference's page, so shell shortcuts (Win+Arrow snap, maximize,
/// restore) still move and resize it. A minimized owner hides its owned
/// popups by itself and is skipped.
pub(super) unsafe fn refit_confirm(p: *mut App, scrim: HWND, dialog: HWND) {
    refit_confirm_on(Host::main(p), scrim, dialog);
}

unsafe fn refit_confirm_on(host: Host, scrim: HWND, dialog: HWND) {
    let owner = host.hwnd;
    if owner.is_null() || IsIconic(owner) != 0 {
        return;
    }
    let s = state(scrim);
    if !s.is_null() && !(*s).closing {
        let bounds = visible_bounds(owner);
        let size = SIZE {
            cx: (bounds.right - bounds.left).max(1),
            cy: (bounds.bottom - bounds.top).max(1),
        };
        let resized = size.cx != (*s).size.cx || size.cy != (*s).size.cy;
        if resized {
            if let Some(surface) = gfx::LayeredSurface::new(size.cx, size.cy, &[]) {
                (*s).surface = surface;
                (*s).size = size;
            }
        }
        (*s).radius = if IsZoomed(owner) != 0 {
            0.0
        } else {
            gfx::px((*s).dpi, theme::RADIUS)
        };
        (*s).origin = POINT {
            x: bounds.left,
            y: bounds.top,
        };
        (*s).compose = true;
        (*s).render();
    }
    let d = state(dialog);
    if !d.is_null() && !(*d).closing {
        let client = client_screen(owner);
        let (origin, _) = place(
            Anchor::Center(client),
            (*d).size.cx,
            (*d).size.cy,
            work_area(client),
            0,
            0,
        );
        if origin.x != (*d).origin.x || origin.y != (*d).origin.y {
            (*d).origin = origin;
            (*d).render();
        }
    }
}

/// Close `dismiss` when the scrim is pressed.
pub(super) unsafe fn set_dismiss(scrim: HWND, dismiss: HWND) {
    let s = state(scrim);
    if !s.is_null() {
        (*s).dismiss = dismiss;
    }
}

unsafe fn dialog_view(hwnd: HWND) -> Option<(*mut Popup, *mut DialogView)> {
    let s = state(hwnd);
    if s.is_null() {
        return None;
    }
    match &mut (*s).kind {
        Kind::Dialog(view) => Some((s, view as *mut DialogView)),
        _ => None,
    }
}

/// The dialog button under a screen point.
unsafe fn dialog_button(hwnd: HWND, pt: POINT) -> Option<usize> {
    let (popup, view) = dialog_view(hwnd)?;
    let content = (*popup).content_screen();
    (*view)
        .buttons
        .iter()
        .position(|r| contains(&offset(*r, content.left, content.top), pt))
}

/// Keyboard focus / ring / pressed state of the staged dialog (previews).
pub(super) unsafe fn dialog_state(hwnd: HWND, focus: usize, ring: bool, pressed: Option<usize>) {
    if let Some((popup, view)) = dialog_view(hwnd) {
        (*view).focus = focus.min(1);
        (*view).ring = ring;
        (*view).pressed = pressed;
        (*popup).compose = true;
        (*popup).render();
    }
}

/// The dialog's wrapped lines (title, body, warn) with their drawn widths,
/// and the width of the text box (tests).
#[cfg(test)]
pub(super) unsafe fn dialog_lines(hwnd: HWND) -> Option<(Vec<(String, i32)>, i32)> {
    let (popup, view) = dialog_view(hwnd)?;
    let fonts = &*(*popup).fonts;
    let dc = MeasureDc::new();
    let mut lines = Vec::new();
    for line in &(*view).title {
        lines.push((
            line.text.clone(),
            line_width(dc.0, fonts.dialog_title, line),
        ));
    }
    for line in (*view).body.iter().chain(&(*view).warn) {
        lines.push((line.text.clone(), line_width(dc.0, fonts.ui, line)));
    }
    Some((lines, (*view).text_right - (*view).text_left))
}

/// The confirm dialog (DESIGN_SPEC §4): scrim over the window, 440 px
/// dialog with h2 / text / optional warn line, footer [Cancel] [action].
/// Synchronous: returns true only when the action is chosen. Cancel has the
/// initial focus; Enter / Space activate the focused button, Esc / Alt+F4 /
/// a click on the scrim cancel, Tab / arrows move the focus.
pub(super) unsafe fn confirm_dialog(p: *mut App, spec: &ConfirmSpec) -> bool {
    confirm_dialog_on(Host::main(p), spec)
}

/// [`confirm_dialog`] over `host` (another Feather window).
pub(super) unsafe fn confirm_dialog_on(host: Host, spec: &ConfirmSpec) -> bool {
    let owner = host.hwnd;
    let Some((scrim, dialog)) = stage_confirm_on(host, spec) else {
        return false;
    };
    let mut result = false;
    let mut hover_on: Option<usize> = None;
    let mut tracking = false;
    let geometry = || unsafe { rect_key(visible_bounds(owner), client_screen(owner)) };
    let mut placed = geometry();
    run_modal(
        |msg| {
            // The owner moved or resized (snap, maximize, restore): the
            // scrim and the dialog follow before the message is handled.
            if IsIconic(owner) == 0 {
                let now = geometry();
                if now != placed {
                    placed = now;
                    refit_confirm_on(host, scrim, dialog);
                }
            }
            let Some((popup, view)) = dialog_view(dialog) else {
                return Step::Done;
            };
            let refresh = || unsafe {
                (*popup).compose = true;
                (*popup).render();
            };
            let mut set_hover = |hot: Option<usize>| unsafe {
                if hover_on == hot {
                    return;
                }
                if let Some(old) = hover_on {
                    (*popup).fade_hover(old, false);
                }
                if let Some(new) = hot {
                    (*popup).fade_hover(new, true);
                }
                hover_on = hot;
                refresh();
            };
            match msg.message {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    match msg.wParam as u16 {
                        VK_ESCAPE => return Step::Done,
                        VK_F4 if msg.message == WM_SYSKEYDOWN => return Step::Done,
                        VK_RETURN | VK_SPACE => {
                            result = (*view).focus == 1;
                            return Step::Done;
                        }
                        VK_TAB | VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN => {
                            (*view).focus = 1 - (*view).focus;
                            (*view).ring = true;
                            refresh();
                        }
                        _ => {}
                    }
                    Step::Consumed
                }
                WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR => {
                    Step::Consumed
                }
                WM_MOUSEMOVE => {
                    if msg.hwnd == dialog {
                        if !tracking {
                            let mut event = TRACKMOUSEEVENT {
                                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TME_LEAVE,
                                hwndTrack: dialog,
                                dwHoverTime: 0,
                            };
                            tracking = TrackMouseEvent(&mut event) != 0;
                        }
                        set_hover(dialog_button(dialog, message_point(msg)));
                    } else {
                        set_hover(None);
                    }
                    Step::Consumed
                }
                WM_MOUSELEAVE if msg.hwnd == dialog => {
                    tracking = false;
                    set_hover(None);
                    Step::Consumed
                }
                WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                    if msg.hwnd == dialog {
                        if let Some(i) = dialog_button(dialog, message_point(msg)) {
                            (*view).pressed = Some(i);
                            (*view).focus = i;
                            (*view).ring = false;
                            refresh();
                            SetCapture(dialog);
                        }
                        Step::Consumed
                    } else if msg.hwnd == scrim {
                        // `.scrim` mousedown outside the dialog cancels.
                        Step::Done
                    } else {
                        Step::Consumed
                    }
                }
                WM_LBUTTONUP => {
                    let released = (*view).pressed.take();
                    if GetCapture() == dialog {
                        ReleaseCapture();
                    }
                    refresh();
                    if let Some(i) = released {
                        if dialog_button(dialog, message_point(msg)) == Some(i) {
                            result = i == 1;
                            return Step::Done;
                        }
                    }
                    Step::Consumed
                }
                WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN
                | WM_XBUTTONUP | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_NCLBUTTONDOWN
                | WM_NCRBUTTONDOWN => Step::Consumed,
                _ => Step::Pass,
            }
        },
        || IsWindow(dialog) != 0 && IsWindow(owner) != 0,
    );
    if GetCapture() == dialog {
        ReleaseCapture();
    }
    close(dialog);
    close(scrim);
    result
}

// ───────────────────────────── modal panel ─────────────────────────────

/// Open the scrim and a centred, fully painted panel of `width` × `height`
/// device px (capped to the owner's client less 16 px each side) whose
/// content `panel` paints. No loop runs here: the caller drives it with
/// [`run_modal`] (or captures it for previews).
pub(super) unsafe fn stage_panel(
    p: *mut App,
    width: i32,
    height: i32,
    panel: Box<dyn PanelContent>,
) -> Option<(HWND, HWND)> {
    let owner = (*p).hwnd;
    if owner.is_null() {
        return None;
    }
    let dpi = (*p).dpi;
    let scrim = open_scrim(p, null_mut());
    let client = client_screen(owner);
    let size = SIZE {
        cx: width
            .min(client.right - client.left - gfx::pxi(dpi, 32.0))
            .max(1),
        cy: height
            .min(client.bottom - client.top - gfx::pxi(dpi, 32.0))
            .max(1),
    };
    let (origin, _) = place(
        Anchor::Center(client),
        size.cx,
        size.cy,
        work_area(client),
        0,
        0,
    );
    let hwnd = create(
        Host::main(p),
        Kind::Panel(panel),
        size,
        origin,
        gfx::px(dpi, anim::motion::POPUP_SLIDE_DIP),
        true,
        gfx::px(dpi, theme::RADIUS),
        0,
        anim::motion::POPUP_IN,
    );
    if hwnd.is_null() {
        destroy(scrim);
        return None;
    }
    restart_fade(scrim);
    Some((scrim, hwnd))
}

/// The largest panel height that fits the owner's client (device px).
pub(super) unsafe fn panel_max_height(p: *mut App) -> i32 {
    let client = client_screen((*p).hwnd);
    (client.bottom - client.top - gfx::pxi((*p).dpi, 32.0)).max(1)
}

/// Give an open panel a new height (capped to the owner) and centre it.
pub(super) unsafe fn resize_panel(p: *mut App, hwnd: HWND, height: i32) {
    let s = state(hwnd);
    if s.is_null() || (*s).closing || !matches!((*s).kind, Kind::Panel(_)) {
        return;
    }
    let height = height.min(panel_max_height(p)).max(1);
    if height != (*s).size.cy {
        let layers = gfx::popup_shadow((*s).dpi);
        let Some(surface) = gfx::LayeredSurface::new((*s).size.cx, height, &layers) else {
            return;
        };
        // UpdateLayeredWindow (render) takes the new window size with it.
        (*s).surface = surface;
        (*s).size.cy = height;
        (*s).shadow = None;
    }
    let client = client_screen((*p).hwnd);
    let (origin, _) = place(
        Anchor::Center(client),
        (*s).size.cx,
        (*s).size.cy,
        work_area(client),
        0,
        0,
    );
    (*s).origin = origin;
    (*s).compose = true;
    (*s).render();
}

/// Recompose an open panel (its state changed).
pub(super) unsafe fn repaint_panel(hwnd: HWND) {
    let s = state(hwnd);
    if !s.is_null() && !(*s).closed {
        (*s).compose = true;
        (*s).render();
    }
}

/// A popup's animation host (panel hover fades, switch slides).
pub(super) unsafe fn popup_anim<'a>(hwnd: HWND) -> Option<&'a mut anim::AnimHost<(usize, u32)>> {
    let s = state(hwnd);
    (!s.is_null()).then(|| &mut (*s).anim)
}

/// The screen rectangle of a popup's content box when fully shown.
pub(super) unsafe fn content_screen(hwnd: HWND) -> Option<RECT> {
    let s = state(hwnd);
    (!s.is_null()).then(|| (*s).content_screen())
}

// ───────────────────────────── toast ─────────────────────────────

thread_local! {
    /// The visible toast of each owner window on this thread.
    static TOASTS: RefCell<Vec<(HWND, HWND)>> = const { RefCell::new(Vec::new()) };
}

/// `.toast`: bottom-right (right 16, bottom 44) of the owner's client, fg
/// background, surface text 13 px, padding 10 14, radius 4, shadow; fades /
/// slides in (150 ms), holds 2.6 s, fades out (200 ms). Replaces the owner's
/// previous toast. Created even for a hidden owner (previews); use [`toast`].
pub(super) unsafe fn show_toast(p: *mut App, text: &str) -> HWND {
    let owner = (*p).hwnd;
    if owner.is_null() || text.trim().is_empty() {
        return null_mut();
    }
    let previous = TOASTS.with(|toasts| {
        let mut toasts = toasts.borrow_mut();
        toasts.retain(|&(_, toast)| IsWindow(toast) != 0);
        toasts
            .iter()
            .position(|&(o, _)| o == owner)
            .map(|i| toasts.remove(i).1)
    });
    if let Some(previous) = previous {
        destroy(previous);
    }
    let dpi = (*p).dpi;
    let client = client_screen(owner);
    let width = {
        let dc = MeasureDc::new();
        let old = SelectObject(dc.0, (*p).fonts.ui);
        let w = fonts::str_extent(dc.0, text).cx;
        SelectObject(dc.0, old);
        (w + 2 * gfx::pxi(dpi, 14.0)).min(client.right - client.left - gfx::pxi(dpi, 32.0))
    };
    let height = (gfx::px(dpi, 10.0) * 2.0 + gfx::px(dpi, Font::Ui.line())).round() as i32;
    let origin = POINT {
        x: client.right - gfx::pxi(dpi, 16.0) - width,
        y: client.bottom - gfx::pxi(dpi, 44.0) - height,
    };
    let hwnd = create(
        Host::main(p),
        Kind::Toast(text.to_owned()),
        SIZE {
            cx: width.max(1),
            cy: height,
        },
        origin,
        gfx::px(dpi, anim::motion::POPUP_SLIDE_DIP),
        true,
        gfx::px(dpi, theme::RADIUS_SM),
        WS_EX_TRANSPARENT,
        anim::motion::TOAST_IN,
    );
    if !hwnd.is_null() {
        TOASTS.with(|toasts| toasts.borrow_mut().push((owner, hwnd)));
    }
    hwnd
}

/// Show a toast when the owner is on screen (false: hidden or minimized,
/// the caller keeps the notice elsewhere).
pub(super) unsafe fn toast(p: *mut App, text: &str) -> bool {
    let owner = (*p).hwnd;
    if owner.is_null() || IsWindowVisible(owner) == 0 || IsIconic(owner) != 0 {
        return false;
    }
    !show_toast(p, text).is_null()
}

/// The owner's current toast window (tests, previews).
pub(super) unsafe fn current_toast(owner: HWND) -> HWND {
    TOASTS.with(|toasts| {
        toasts
            .borrow()
            .iter()
            .find(|&&(o, t)| o == owner && IsWindow(t) != 0)
            .map_or(null_mut(), |&(_, t)| t)
    })
}

/// Close popups that follow the owner's position when it moves (toast).
pub(super) unsafe fn owner_moved(owner: HWND) {
    let toast = current_toast(owner);
    if !toast.is_null() {
        destroy(toast);
    }
}

/// The theme changed (`interactions::apply_theme`): every open popup of this
/// thread (menus, dropdowns, the dialog and its scrim, a toast) repaints in
/// the new palette, shadow included.
pub(super) unsafe fn theme_changed() {
    let palette = theme::colors();
    for hwnd in thread_popups() {
        repaint_with(hwnd, palette);
    }
}

/// Recompose one of our popups in `palette` (its shadow color too).
pub(super) unsafe fn repaint_with(hwnd: HWND, palette: Palette) {
    let s = state(hwnd);
    if s.is_null() || (*s).closed {
        return;
    }
    (*s).palette = palette;
    (*s).shadow = None;
    (*s).compose = true;
    (*s).render();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(labels: &[(&str, bool)]) -> Vec<MenuItem> {
        labels
            .iter()
            .enumerate()
            .map(|(i, &(label, enabled))| {
                if label == "-" {
                    MenuItem::separator()
                } else {
                    MenuItem {
                        enabled,
                        ..MenuItem::item(i + 1, label)
                    }
                }
            })
            .collect()
    }

    #[test]
    fn menu_navigation_skips_separators_and_disabled_items() {
        let menu = items(&[
            ("End task", true),
            ("Disabled", false),
            ("-", true),
            ("Efficiency", true),
            ("Refresh", true),
        ]);
        assert_eq!(step_item(&menu, None, true), Some(0));
        assert_eq!(step_item(&menu, Some(0), true), Some(3));
        assert_eq!(step_item(&menu, Some(4), true), Some(0), "wraps");
        assert_eq!(step_item(&menu, None, false), Some(4));
        assert_eq!(step_item(&menu, Some(0), false), Some(4));
        assert_eq!(step_item(&menu, Some(3), false), Some(0));
        assert_eq!(
            step_item(&items(&[("-", true), ("x", false)]), None, true),
            None
        );
        // First letters cycle through matches, never landing on disabled items.
        assert_eq!(letter_item(&menu, None, 'e'), Some(0));
        assert_eq!(letter_item(&menu, Some(0), 'E'), Some(3));
        assert_eq!(letter_item(&menu, Some(3), 'e'), Some(0));
        assert_eq!(letter_item(&menu, None, 'd'), None);
        assert_eq!(letter_item(&menu, None, 'r'), Some(4));
        assert_eq!(letter_item(&menu, None, ' '), None);
    }

    #[test]
    fn native_labels_become_labels_and_kbd_hints() {
        assert_eq!(
            split_hint("End task\tDel"),
            ("End task".into(), Some("Del".into()))
        );
        assert_eq!(split_hint("&Open && run"), ("Open & run".into(), None));
        assert_eq!(split_hint("Refresh\t"), ("Refresh".into(), None));
    }

    #[test]
    fn popups_open_where_there_is_room_and_flip_at_work_area_edges() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 800,
        };
        let button = RECT {
            left: 900,
            top: 100,
            right: 932,
            bottom: 132,
        };
        // Right-aligned under the ⋯ button, sliding down into place.
        let (pt, slide) = place(
            Anchor::Below {
                r: button,
                right: true,
            },
            220,
            300,
            work,
            4,
            0,
        );
        assert_eq!((pt.x, pt.y, slide), (932 - 220, 136, -1));
        // No room below: above the anchor, sliding up.
        let low = RECT {
            top: 700,
            bottom: 732,
            ..button
        };
        let (pt, slide) = place(
            Anchor::Below {
                r: low,
                right: true,
            },
            220,
            300,
            work,
            4,
            0,
        );
        assert_eq!((pt.y, slide), (700 - 4 - 300, 1));
        // Left-aligned selects clamp to the work area.
        let (pt, _) = place(
            Anchor::Below {
                r: button,
                right: false,
            },
            220,
            100,
            work,
            4,
            0,
        );
        assert_eq!(pt.x, 1000 - 220);
        // Above the Settings button; below when the top has no room.
        let above = |r: RECT, right: bool| place(Anchor::Above { r, right }, 200, 150, work, 4, 0);
        let (pt, slide) = above(low, false);
        assert_eq!((pt.x, pt.y, slide), (1000 - 200, 700 - 4 - 150, 1));
        let (pt, slide) = above(button, false);
        assert_eq!((pt.y, slide), (136, -1));
        // Right-aligned above the Resource Monitor's Refresh button.
        let (pt, slide) = above(low, true);
        assert_eq!((pt.x, pt.y, slide), (932 - 200, 700 - 4 - 150, 1));
        // Context menus flip left / up at the edges.
        let (pt, slide) = place(
            Anchor::Point(POINT { x: 950, y: 750 }),
            200,
            150,
            work,
            4,
            0,
        );
        assert_eq!((pt.x, pt.y, slide), (750, 600, 1));
        let (pt, _) = place(Anchor::Point(POINT { x: 10, y: 10 }), 200, 150, work, 4, 0);
        assert_eq!((pt.x, pt.y), (10, 10));
        // Submenus sit beside the parent with their first item on the row,
        // flipping to the left edge of the parent.
        let parent = RECT {
            left: 500,
            top: 100,
            right: 720,
            bottom: 400,
        };
        let item = RECT {
            left: 505,
            top: 200,
            right: 715,
            bottom: 232,
        };
        let (pt, slide) = place(Anchor::Beside { item, parent }, 200, 150, work, 4, 5);
        assert_eq!((pt.x, pt.y, slide), (722, 195, 0));
        let far = RECT {
            left: 700,
            right: 920,
            ..parent
        };
        let (pt, _) = place(Anchor::Beside { item, parent: far }, 200, 150, work, 4, 5);
        assert_eq!(pt.x, 700 - 2 - 200);
        // The dialog centres on the window.
        let (pt, slide) = place(Anchor::Center(work), 440, 200, work, 0, 0);
        assert_eq!((pt.x, pt.y, slide), (280, 300, 1));
    }

    #[test]
    fn prompts_split_into_heading_and_text() {
        assert_eq!(
            split_prompt("End chrome.exe (PID 42)?\n\nUnsaved work may be lost."),
            (
                "End chrome.exe (PID 42)?".into(),
                "Unsaved work may be lost.".into()
            )
        );
        assert_eq!(
            split_prompt("Restart Audio? Features using it will pause."),
            (
                "Restart Audio?".into(),
                "Features using it will pause.".into()
            )
        );
        assert_eq!(
            split_prompt("Stop X?\n\nThis may affect apps.\nService name: X"),
            (
                "Stop X?".into(),
                "This may affect apps.\nService name: X".into()
            )
        );
        assert_eq!(
            split_prompt("No question here"),
            ("No question here".into(), String::new())
        );
    }
}
