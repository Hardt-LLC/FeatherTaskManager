//! Custom virtual table / list control (DESIGN_SPEC §4 "Tables", the
//! performance device list). It replaces SysListView32 (processes, startup,
//! services) and the owner-draw device ListBox with one window class:
//!
//! * a sticky header (`widgets::header_cell`: totals, sort arrows, hover),
//! * virtual rows of variable height (group rows 38 px, data rows 33 + 1 px
//!   row border) painted by a [`Model`] — the app's pages and device cards
//!   are models ([`create_table`], [`create_devices`]),
//! * single selection + keyboard navigation that skips group rows (Up / Down
//!   / Page Up / Page Down / Home / End, typeahead, Enter = `NM_RETURN`),
//! * pixel-smooth scrolling and the Fluent overlay scrollbar (`scroll.rs`),
//! * per-row hover cross-fades, chevron rotation (120 ms `ease`) and switch
//!   slides (120 ms `ease`) on the window's own [`AnimHost`]: a timer runs
//!   only while something moves.
//!
//! Compatibility: in [`Mode::Table`] the window answers the subset of the
//! `LVM_*` messages the app uses (item count, selection state, next item,
//! ensure visible, item / sub-item rectangles, item text) and sends the same
//! notifications as a report-view ListView (`LVN_ITEMCHANGED`,
//! `LVN_COLUMNCLICK`, `NM_CLICK` / `NM_DBLCLK` with `NMITEMACTIVATE`,
//! `NM_RETURN`); in [`Mode::List`] it answers the `LB_*` messages of the
//! device list and sends `LBN_SELCHANGE` / `LBN_DBLCLK`. Existing selection
//! logic, identity-stable rebuilds and the command palette keep working.
//! `WM_CONTEXTMENU` reaches the parent through `DefWindowProc` (mouse: at the
//! cursor; Apps / Shift+F10: lParam −1, see [`keyboard_menu_anchor`]).
use super::anim::{motion, AnimHost, Easing};
use super::fonts::Fonts;
use super::gfx::{self, BackBuffer};
use super::scroll::{self, Extent, Scroller};
use super::widgets::{self, Painter, PillKind};
use super::*;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

mod access;
mod horizontal;

const CLASS: &str = "FeatherTable";
/// Frame timer id of the table's animation host (its delay timer is
/// `anim::delay_timer_id(TIMER)`).
const TIMER: usize = 0xFEA8;
/// A flexible column never gets narrower than this (CSS px).
const FLEX_MIN: f32 = 120.0;
/// Empty-state band below the header (CSS px).
const EMPTY_HEIGHT: f32 = 120.0;
/// Group row height (CSS px, no border).
pub(super) const GROUP_ROW: f32 = 38.0;
/// Deeper tree levels stop indenting here (28 px per level).
const TREE_DEPTH_CAP: usize = 6;
/// Typeahead: keys closer than this extend the search text.
const TYPEAHEAD: Duration = Duration::from_millis(1000);

/// Which message family the control speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    /// ListView-compatible table (`LVM_*`, `WM_NOTIFY`).
    Table,
    /// ListBox-compatible list without header (`LB_*`, `WM_COMMAND`).
    List,
}

/// One column. Widths are CSS px; `flex` columns share the remaining width
/// (never below 120 px); `right` aligns the header label (and, by the
/// model's convention, the cells) to the right.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Column {
    pub label: String,
    pub width: f32,
    pub flex: bool,
    pub right: bool,
}

/// Content of one header cell.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct HeaderCell {
    /// `.tot` line (15/600 mono) above the label.
    pub total: Option<String>,
    /// Sorted column: Some(descending).
    pub sort: Option<bool>,
    /// Keep the total line (blank when None) so labels bottom-align.
    pub two_line: bool,
}

/// Interactive parts of a row, client coordinates.
#[derive(Clone, Copy, Default)]
pub(super) struct Parts {
    pub chevron: Option<RECT>,
    pub switch: Option<RECT>,
}

/// What a point hits inside a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Part {
    Row,
    Chevron,
    Switch,
}

/// Animated state a model paints a row with.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct RowPaint {
    /// Hover amount 0..=1 (fg_soft cross-fade).
    pub hover: f32,
    pub selected: bool,
    /// The control has keyboard focus.
    pub focused: bool,
    /// Chevron angle in degrees (0 → right, 90 → down).
    pub chevron: f32,
    /// Chevron hover amount 0..=1.
    pub chevron_hover: f32,
    /// Switch checked progress 0..=1.
    pub switch: f32,
}

/// The data behind a control. Row geometry is device px; `row_height`
/// includes whatever border the painter draws (tables: 1 px row border).
pub(super) trait Model {
    /// DPI and fonts to paint with (the app's: child windows share the main
    /// window's monitor).
    unsafe fn dpi(&self) -> i32;
    unsafe fn fonts(&self) -> &Fonts;
    /// Header height including its bottom border (0 = no header).
    unsafe fn header_height(&self) -> i32;
    unsafe fn row_height(&self, row: usize) -> i32;
    /// One wheel "line" (device px).
    unsafe fn line(&self) -> i32;
    /// Group rows are never hovered, selected or reached by the keyboard.
    unsafe fn is_group(&self, _row: usize) -> bool {
        false
    }
    /// Stable identity for per-item animations (chevron, switch) that must
    /// survive rebuilds and re-sorts.
    unsafe fn key(&self, row: usize) -> u64 {
        row as u64
    }
    /// Expanded state of a tree parent (None: the row has no chevron).
    unsafe fn expanded(&self, _row: usize) -> Option<bool> {
        None
    }
    /// Checked state of the row's interactive switch (None: none).
    unsafe fn switch_on(&self, _row: usize) -> Option<bool> {
        None
    }
    unsafe fn header(&self, _col: usize) -> HeaderCell {
        HeaderCell::default()
    }
    unsafe fn editable_columns(&self) -> bool {
        false
    }
    /// Preserve column widths and scroll horizontally without requiring editing.
    unsafe fn horizontal_columns(&self) -> bool {
        self.editable_columns()
    }
    unsafe fn resize_column(&self, _column: usize, _width: f32) {}
    unsafe fn reorder_column(&self, _from: usize, _to: usize) {}
    /// The column menu at the screen point `point`: for the shown column
    /// `column`, or (None, opened from the keyboard) for any of them.
    unsafe fn column_menu(&self, _column: Option<usize>, _point: POINT) {}
    /// Clickable parts of `row` laid out in the row rectangle `r` with the
    /// column cells `cells` (same geometry the painter uses).
    unsafe fn parts(&self, _row: usize, _r: RECT, _cells: &[RECT]) -> Parts {
        Parts::default()
    }
    /// Paint row `row` in `r`; `cells` are its column cells, empty (zero
    /// width) where a partial repaint does not reach them.
    unsafe fn paint_row(&self, pt: &Painter, row: usize, r: RECT, cells: &[RECT], st: &RowPaint);
    /// Where the keyboard focus ring goes in row `r` and its corner radius
    /// (device px): the row without its 1 px border by default.
    unsafe fn focus_box(&self, _row: usize, r: RECT) -> (RECT, f32) {
        (content_of(gfx::hairline(self.dpi()) as i32, r), 0.0)
    }
    /// Cell text (typeahead, `LVM_GETITEMW`, accessibility names).
    unsafe fn text(&self, row: usize, col: usize) -> String;
    /// Centred muted text when there are no rows.
    unsafe fn empty_text(&self) -> String {
        String::new()
    }
    /// Content padding (device px, horizontal and vertical) that scrolls
    /// with the rows: `padding: 8px` of the devices column.
    unsafe fn padding(&self) -> (i32, i32) {
        (0, 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Key {
    Scroll,
    Bar,
    Grow,
    Row(usize),
    Header(usize),
    ChevronHover(usize),
    Chevron(u64),
    Switch(u64),
}

/// Keyboard navigation steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Nav {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
}

/// The row `nav` moves the selection to from `from` among `count` rows,
/// never landing on a group row; `page` = rows per page. None when there is
/// no selectable row.
pub(super) fn step(
    count: usize,
    group: impl Fn(usize) -> bool,
    from: Option<usize>,
    nav: Nav,
    page: usize,
) -> Option<usize> {
    let data = |i: &usize| !group(*i);
    let first = (0..count).find(data)?;
    let last = (0..count).rev().find(data)?;
    let down = |start: usize| (start.min(count)..count).find(data);
    let up = |start: usize| (0..=start.min(count - 1)).rev().find(data);
    let Some(from) = from.filter(|&i| i < count) else {
        return Some(if nav == Nav::End { last } else { first });
    };
    let page = page.max(1);
    Some(match nav {
        Nav::Up if from == 0 => from,
        Nav::Up => up(from - 1).unwrap_or(from),
        Nav::Down => down(from + 1).unwrap_or(from),
        Nav::PageUp => {
            let at = from.saturating_sub(page);
            up(at).or_else(|| down(at)).unwrap_or(from)
        }
        Nav::PageDown => {
            let at = (from + page).min(count - 1);
            down(at).or_else(|| up(at)).unwrap_or(from)
        }
        Nav::Home => first,
        Nav::End => last,
    })
}

/// Column spans `(left, right)` across `width` px: fixed widths at the DPI,
/// flexible columns share the rest (at least 120 px each). When even that
/// does not fit (a narrow window with the services details panel), the
/// fixed columns shrink proportionally so every column stays on screen
/// (there is no horizontal scrolling).
pub(super) fn spans(columns: &[Column], width: i32, dpi: i32) -> Vec<(i32, i32)> {
    if columns.is_empty() {
        return vec![(0, width.max(0))];
    }
    let widths: Vec<i32> = columns
        .iter()
        .map(|c| if c.flex { 0 } else { gfx::pxi(dpi, c.width) })
        .collect();
    let fixed: i32 = widths.iter().sum();
    let flexible = columns.iter().filter(|c| c.flex).count() as i32;
    let min_flex = gfx::pxi(dpi, FLEX_MIN);
    let room = width - flexible * min_flex;
    let scale = if fixed > room && fixed > 0 {
        room.max(0) as f32 / fixed as f32
    } else {
        1.0
    };
    let flex = if flexible > 0 {
        ((width - (fixed as f32 * scale).round() as i32) / flexible).max(min_flex)
    } else {
        0
    };
    let mut x = 0;
    columns
        .iter()
        .zip(widths)
        .map(|(c, w)| {
            let w = if c.flex {
                flex
            } else {
                (w as f32 * scale).floor() as i32
            };
            let span = (x, x + w);
            x += w;
            span
        })
        .collect()
}

/// The row containing content y (tops = prefix offsets, `count + 1` long;
/// `tops[0]` is the content's top padding).
fn row_at(tops: &[i32], y: i32) -> Option<usize> {
    if tops.len() < 2 || y < tops[0].max(0) || y >= *tops.last()? {
        return None;
    }
    Some(tops.partition_point(|&top| top <= y) - 1)
}

fn contains(r: &RECT, pt: POINT) -> bool {
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

fn point(l: LPARAM) -> POINT {
    POINT {
        x: (l & 0xffff) as u16 as i16 as i32,
        y: ((l >> 16) & 0xffff) as u16 as i16 as i32,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    Nothing,
    Header(usize),
    Row(usize),
    Group(usize),
}

struct Init {
    mode: Mode,
    model: Option<Box<dyn Model>>,
}

struct State {
    hwnd: HWND,
    mode: Mode,
    model: Box<dyn Model>,
    columns: Vec<Column>,
    count: usize,
    /// List mode: the LB_ADDSTRING items.
    items: Vec<String>,
    /// Prefix row offsets (content px), `count + 1` long.
    tops: Vec<i32>,
    /// DPI the geometry was computed at.
    dpi: i32,
    selected: Option<usize>,
    hover: Option<usize>,
    hover_header: Option<usize>,
    hover_chevron: Option<usize>,
    tracking: bool,
    pressed: Option<usize>,
    pressed_header: Option<usize>,
    header_origin: i32,
    header_dragged: bool,
    resizing: Option<(usize, i32, f32)>,
    horizontal_offset: i32,
    horizontal_max: i32,
    horizontal_page: i32,
    horizontal_bar: horizontal::Bar,
    redraw: bool,
    typed: String,
    typed_at: Option<Instant>,
    /// Accessibility: a selection announcement is posted, and the identity
    /// (`Model::key`) of the selection announced last.
    announce_pending: bool,
    announced: Option<u64>,
    /// Previews: draw the keyboard focus ring without keyboard focus.
    staged_focus: bool,
    /// The scroll offset (device px) of the rows on screen; None until a
    /// paint covered every row (see [`sync_scroll`]).
    painted_offset: Option<i32>,
    scroll: Scroller<Key>,
    anim: AnimHost<Key>,
    back: BackBuffer,
}

fn class() -> &'static [u16] {
    static NAME: OnceLock<Vec<u16>> = OnceLock::new();
    NAME.get_or_init(|| unsafe {
        let name = wide(CLASS);
        RegisterClassExW(&WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS | CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: GetModuleHandleW(null()),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: name.as_ptr(),
            ..zeroed()
        });
        name
    })
}

/// A table / list child window of `parent` showing `model`.
pub(super) unsafe fn create(
    parent: HWND,
    id: usize,
    label: &str,
    mode: Mode,
    model: Box<dyn Model>,
) -> HWND {
    let class = class();
    let mut init = Init {
        mode,
        model: Some(model),
    };
    CreateWindowExW(
        0,
        class.as_ptr(),
        wide(label).as_ptr(),
        WS_CHILD | WS_VISIBLE | WS_TABSTOP,
        0,
        0,
        0,
        0,
        parent,
        id as HMENU,
        GetModuleHandleW(null()),
        (&mut init as *mut Init).cast(),
    )
}

/// The processes / startup / services table of the main window.
pub(super) unsafe fn create_table(p: *mut App, id: usize, label: &str) -> HWND {
    create((*p).hwnd, id, label, Mode::Table, Box::new(AppRows(p)))
}

/// The performance device card list of the main window.
pub(super) unsafe fn create_devices(p: *mut App, id: usize, label: &str) -> HWND {
    create((*p).hwnd, id, label, Mode::List, Box::new(Devices(p)))
}

unsafe fn state(hwnd: HWND) -> *mut State {
    if hwnd.is_null() || GetClassLongPtrW(hwnd, GCLP_WNDPROC) != proc as *const () as usize {
        return null_mut();
    }
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State
}

impl State {
    fn new(hwnd: HWND, mode: Mode, model: Box<dyn Model>) -> Self {
        Self {
            hwnd,
            mode,
            model,
            columns: Vec::new(),
            count: 0,
            items: Vec::new(),
            tops: vec![0],
            dpi: 0,
            selected: None,
            hover: None,
            hover_header: None,
            hover_chevron: None,
            tracking: false,
            pressed: None,
            pressed_header: None,
            header_origin: 0,
            header_dragged: false,
            resizing: None,
            horizontal_offset: 0,
            horizontal_max: 0,
            horizontal_page: 0,
            horizontal_bar: horizontal::Bar::default(),
            redraw: true,
            typed: String::new(),
            typed_at: None,
            announce_pending: false,
            announced: None,
            staged_focus: false,
            painted_offset: None,
            scroll: Scroller::new(Key::Scroll, Key::Bar, Key::Grow),
            anim: AnimHost::new(TIMER),
            back: BackBuffer::new(),
        }
    }
}

// ───────────────────────────── public API ─────────────────────────────

/// Replace the columns (a page switch); header hover is reset.
pub(super) unsafe fn set_columns(hwnd: HWND, columns: Vec<Column>) {
    let s = state(hwnd);
    if s.is_null() {
        return;
    }
    if (*s).columns != columns {
        (*s).columns = columns;
        set_header_hover(s, None);
        // A drag started on the previous columns must not be applied to these.
        (*s).resizing = None;
        (*s).pressed_header = None;
        (*s).header_dragged = false;
    }
    relayout(s);
    InvalidateRect(hwnd, null(), 0);
}

/// How many columns the table has (`ui::sync_columns`, tests).
pub(super) unsafe fn column_count(hwnd: HWND) -> usize {
    let s = state(hwnd);
    if s.is_null() {
        0
    } else {
        (*s).columns.len()
    }
}

/// The sticky header's height (device px, incl. its bottom border).
#[cfg(test)]
pub(super) unsafe fn header_height(hwnd: HWND) -> i32 {
    let s = state(hwnd);
    if s.is_null() {
        0
    } else {
        (*s).model.header_height()
    }
}

/// Repaint the header (sort arrows, totals).
pub(super) unsafe fn invalidate_header(hwnd: HWND) {
    let s = state(hwnd);
    if s.is_null() {
        return;
    }
    let mut client: RECT = zeroed();
    GetClientRect(hwnd, &mut client);
    client.bottom = client.top + (*s).model.header_height();
    InvalidateRect(hwnd, &client, 0);
}

/// The row and part (chevron, switch, or the row itself) under `pt`
/// (client coordinates); None on the header, group rows or empty space.
pub(super) unsafe fn part_at(hwnd: HWND, pt: POINT) -> Option<(usize, Part)> {
    let s = state(hwnd);
    if s.is_null() {
        return None;
    }
    let Hit::Row(row) = hit(s, pt) else {
        return None;
    };
    let parts = row_parts(s, row)?;
    if parts.chevron.is_some_and(|r| contains(&r, pt)) {
        Some((row, Part::Chevron))
    } else if parts.switch.is_some_and(|r| contains(&r, pt)) {
        Some((row, Part::Switch))
    } else {
        Some((row, Part::Row))
    }
}

/// The client rectangle of a row's part (device drag target or tests).
pub(super) unsafe fn part_rect(hwnd: HWND, row: usize, part: Part) -> Option<RECT> {
    let s = state(hwnd);
    if s.is_null() {
        return None;
    }
    match part {
        Part::Row => row_rect(s, row),
        Part::Chevron => row_parts(s, row)?.chevron,
        Part::Switch => row_parts(s, row)?.switch,
    }
}

/// For a keyboard context menu (`WM_CONTEXTMENU` with lParam −1 from Apps /
/// Shift+F10): the selected row's first cell in screen coordinates (the row
/// is revealed first), so the menu opens at the row, not at the cursor.
/// Without a selection it opens at the row that shows the focus ring (the
/// first data row) when that row is in view, else at the top of the rows
/// below the sticky header — never at the mouse pointer. None only for a
/// mouse invocation (real coordinates) or a window that is not a table.
pub(super) unsafe fn keyboard_menu_anchor(hwnd: HWND, l: LPARAM) -> Option<RECT> {
    let s = state(hwnd);
    let keyboard = (l & 0xffff) as u16 as i16 == -1 && ((l >> 16) & 0xffff) as u16 as i16 == -1;
    if s.is_null() || !keyboard {
        return None;
    }
    let mut client: RECT = zeroed();
    GetClientRect(hwnd, &mut client);
    let header = (*s).model.header_height().min(client.bottom);
    let rows = match (*s).selected {
        Some(row) => {
            reveal(s, row, false);
            row_rect(s, row)
        }
        None => focus_row(s)
            .and_then(|row| row_rect(s, row))
            .filter(|r| r.top >= header && r.bottom <= client.bottom),
    }
    .unwrap_or(RECT {
        top: header,
        bottom: header,
        ..client
    });
    let first = spans(&(*s).columns, client.right, (*s).model.dpi())[0];
    let mut anchor = RECT {
        left: first.0,
        right: first.1.min(client.right),
        top: rows.top.max(header),
        bottom: rows.bottom.min(client.bottom).max(header),
    };
    MapWindowPoints(
        hwnd,
        null_mut(),
        (&mut anchor as *mut RECT).cast::<POINT>(),
        2,
    );
    Some(anchor)
}

/// Compose the whole table into its back buffer without presenting it
/// (`ui::present_all`); false when there is nothing to present.
pub(super) unsafe fn render_back(hwnd: HWND) -> bool {
    let s = state(hwnd);
    if s.is_null() || !(*s).redraw || IsWindowVisible(hwnd) == 0 {
        return false;
    }
    let area = client(s);
    let mut back = std::mem::take(&mut (*s).back);
    let rendered = match back.prepare(area.right, area.bottom) {
        Some((dc, _)) => {
            let saved = SaveDC(dc);
            let (offset, _) = paint_to(s, dc);
            RestoreDC(dc, saved);
            (*s).painted_offset = Some(offset);
            true
        }
        None => false,
    };
    (*s).back = back;
    rendered
}

/// Put the frame [`render_back`] composed on screen and validate the window.
pub(super) unsafe fn present_back(hwnd: HWND) {
    let s = state(hwnd);
    if s.is_null() {
        return;
    }
    let area = client(s);
    let dc = GetDC(hwnd);
    if !dc.is_null() {
        (*s).back.present(dc, &area);
        ReleaseDC(hwnd, dc);
        ValidateRect(hwnd, null());
    } else {
        InvalidateRect(hwnd, null(), 0);
    }
}

/// Jump every running animation to its end (preview captures).
pub(super) unsafe fn settle(hwnd: HWND) {
    let s = state(hwnd);
    if !s.is_null() {
        (*s).anim.finish_all();
    }
}

/// The displayed scroll offset (device px).
pub(super) unsafe fn scroll_offset(hwnd: HWND) -> f32 {
    let s = state(hwnd);
    if s.is_null() {
        0.0
    } else {
        (*s).scroll.offset(&(*s).anim)
    }
}

/// Horizontal overflow (tests): offset, maximum offset and the height of
/// the themed bar (0 while the columns fit), in device px.
#[cfg(test)]
pub(super) unsafe fn horizontal_state(hwnd: HWND) -> (i32, i32, i32) {
    let s = state(hwnd);
    if s.is_null() {
        (0, 0, 0)
    } else {
        (
            (*s).horizontal_offset,
            (*s).horizontal_max,
            horizontal::height(s),
        )
    }
}

/// The scrollable extent (content, view) in device px.
pub(super) unsafe fn extent(hwnd: HWND) -> Extent {
    let s = state(hwnd);
    if s.is_null() {
        Extent::default()
    } else {
        (*s).scroll.extent()
    }
}

/// Jump to `offset` (clamped).
pub(super) unsafe fn scroll_to(hwnd: HWND, offset: f32) {
    let s = state(hwnd);
    if !s.is_null() {
        (*s).scroll.scroll_to(&mut (*s).anim, offset, None);
        InvalidateRect(hwnd, null(), 0);
    }
}

/// Show `row` hovered (settled), e.g. for previews.
pub(super) unsafe fn stage_hover(hwnd: HWND, row: Option<usize>) {
    let s = state(hwnd);
    if s.is_null() {
        return;
    }
    if let Some(old) = (*s).hover.take() {
        (*s).anim.set(Key::Row(old), 0.0);
    }
    if let Some(row) = row.filter(|&r| r < (*s).count && !(*s).model.is_group(r)) {
        (*s).hover = Some(row);
        (*s).anim.set(Key::Row(row), 1.0);
    }
}

/// Show the overlay scrollbar at `opacity` / `grow` without a pending fade
/// (previews: the idle 3 px indicator, the hovered 8 px bar).
pub(super) unsafe fn stage_scrollbar(hwnd: HWND, opacity: f32, grow: f32) {
    let s = state(hwnd);
    if !s.is_null() {
        (*s).scroll.stage(&mut (*s).anim, opacity, grow);
    }
}

/// Show the keyboard focus ring as if focused from the keyboard (previews).
pub(super) unsafe fn stage_focus(hwnd: HWND, on: bool) {
    let s = state(hwnd);
    if !s.is_null() {
        (*s).staged_focus = on;
        InvalidateRect(hwnd, null(), 0);
    }
}

/// Timer / animation state for tests.
#[cfg(test)]
pub(super) unsafe fn is_animating(hwnd: HWND) -> bool {
    let s = state(hwnd);
    !s.is_null() && ((*s).anim.is_running() || (*s).anim.is_waiting())
}

// ───────────────────────────── geometry ─────────────────────────────

unsafe fn client(s: *mut State) -> RECT {
    let mut r: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut r);
    r
}

unsafe fn content_client(s: *mut State) -> RECT {
    let mut r = client(s);
    r.bottom = (r.bottom - horizontal::height(s)).max(r.top);
    r
}

/// Process layouts keep explicit widths; overflow uses a horizontal bar.
/// Other pages retain their original compact, proportionally fitted columns.
unsafe fn column_spans(s: *mut State) -> Vec<(i32, i32)> {
    let area = client(s);
    let dpi = (*s).model.dpi();
    let width = if (*s).model.horizontal_columns() {
        area.right.max(
            (*s).columns
                .iter()
                .map(|c| gfx::pxi(dpi, if c.flex { FLEX_MIN } else { c.width }))
                .sum(),
        )
    } else {
        area.right
    };
    spans(&(*s).columns, width, dpi)
        .into_iter()
        .map(|(left, right)| {
            (
                left - (*s).horizontal_offset,
                right - (*s).horizontal_offset,
            )
        })
        .collect()
}

unsafe fn sync_horizontal(s: *mut State) {
    let area = client(s);
    let maximum = if (*s).model.horizontal_columns() {
        (column_spans(s)
            .last()
            .map_or(0, |span| span.1 + (*s).horizontal_offset)
            - area.right)
            .max(0)
    } else {
        0
    };
    if maximum == (*s).horizontal_max && area.right == (*s).horizontal_page {
        return;
    }
    (*s).horizontal_max = maximum;
    (*s).horizontal_page = area.right;
    (*s).horizontal_offset = (*s).horizontal_offset.clamp(0, maximum);
}

unsafe fn horizontal_to(s: *mut State, position: i32) {
    let next = position.clamp(0, (*s).horizontal_max);
    if next == (*s).horizontal_offset {
        return;
    }
    (*s).horizontal_offset = next;
    (*s).painted_offset = None;
    InvalidateRect((*s).hwnd, null(), 0);
}

/// Recompute row offsets, the scroll extent and the scrollbar lane (after a
/// row count, size, DPI or header change). A DPI change rescales the offset.
unsafe fn relayout(s: *mut State) {
    let dpi = (*s).model.dpi();
    if (*s).dpi != dpi {
        if (*s).dpi > 0 {
            let factor = dpi as f32 / (*s).dpi as f32;
            (*s).scroll.rescale(&mut (*s).anim, factor);
        }
        (*s).dpi = dpi;
    }
    sync_horizontal(s);
    let count = (*s).count;
    let (_, pad) = (*s).model.padding();
    let tops = &mut (*s).tops;
    tops.clear();
    tops.reserve(count + 1);
    let mut y = pad;
    tops.push(pad);
    for row in 0..count {
        y += (*s).model.row_height(row).max(1);
        tops.push(y);
    }
    let area = content_client(s);
    let header = (*s).model.header_height().clamp(0, area.bottom.max(0));
    let content = if count > 0 { y + pad } else { 0 };
    let extent = Extent::new(content as f32, (area.bottom - header).max(0) as f32);
    (*s).scroll.set_extent(&mut (*s).anim, extent);
    let lane = lane(s, client(s));
    (*s).anim.register(Key::Bar, (*s).hwnd, Some(lane));
    (*s).anim.register(Key::Grow, (*s).hwnd, Some(lane));
    // The glide repaints incrementally (`sync_scroll`), not the whole
    // window per frame.
    (*s).anim
        .register(Key::Scroll, (*s).hwnd, Some(RECT::default()));
}

/// The rows' area below the sticky header (client px).
unsafe fn rows_area(s: *mut State) -> RECT {
    let area = content_client(s);
    RECT {
        top: (*s).model.header_height().clamp(0, area.bottom.max(0)),
        ..area
    }
}

/// A GDI region owned by the table (deleted on drop).
struct Region(HRGN);

impl Region {
    unsafe fn rect(r: &RECT) -> Self {
        Region(CreateRectRgnIndirect(r))
    }
    unsafe fn add_rect(&mut self, r: &RECT) {
        let other = Region::rect(r);
        CombineRgn(self.0, self.0, other.0, RGN_OR);
    }
    /// Add `other` moved down by `dy`.
    unsafe fn add(&mut self, other: &Region, dy: i32) {
        let moved = Region::rect(&RECT::default());
        CombineRgn(moved.0, other.0, null_mut(), RGN_COPY);
        OffsetRgn(moved.0, 0, dy);
        CombineRgn(self.0, self.0, moved.0, RGN_OR);
    }
    unsafe fn clip_to(&mut self, r: &RECT) {
        let bounds = Region::rect(r);
        CombineRgn(self.0, self.0, bounds.0, RGN_AND);
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
    }
}

/// Move the rows' pixels of the back buffer `dy` px down (up when negative).
unsafe fn shift_rows(dc: HDC, rows: RECT, dy: i32) {
    let width = rows.right - rows.left;
    let height = rows.bottom - rows.top - dy.abs();
    if width <= 0 || height <= 0 {
        return;
    }
    let (to, from) = if dy < 0 {
        (rows.top, rows.top - dy)
    } else {
        (rows.top + dy, rows.top)
    };
    BitBlt(
        dc, rows.left, to, width, height, dc, rows.left, from, SRCCOPY,
    );
}

/// Bring the screen up to the current scroll offset incrementally (glide
/// frames, thumb drags): shift the rows already painted in the back buffer,
/// repaint only the strip that scrolled into view, the overlay scrollbar's
/// lane (it moved with the pixels) and whatever was already due inside the
/// rows (a fading hover row, where it was and where it moved), then
/// present the rows area with one blit — a glide frame costs a strip, not
/// the whole table, and the screen never shows a half-updated frame. The
/// back buffer mirrors the screen (every paint presents what it drew), so
/// its rows are the ones at `painted_offset`. Anything else falls back to
/// a full repaint.
unsafe fn sync_scroll(s: *mut State) {
    let now = offset_px(s);
    let hwnd = (*s).hwnd;
    let Some(last) = (*s).painted_offset else {
        // Nothing reliable on screen to shift: repaint everything.
        InvalidateRect(hwnd, null(), 0);
        return;
    };
    let area = client(s);
    if now == last {
        // A sub-pixel step: the rows stay, the thumb (fractional) moves.
        InvalidateRect(hwnd, &lane(s, area), 0);
        return;
    }
    let rows = rows_area(s);
    let dy = last - now;
    let fits = (*s).back.size() == Some((area.right, area.bottom));
    let memory = (*s).back.dc().filter(|_| {
        fits && (*s).redraw && dy.abs() < rows.bottom - rows.top && IsWindowVisible(hwnd) != 0
    });
    let Some(memory) = memory else {
        (*s).painted_offset = None;
        InvalidateRect(hwnd, null(), 0);
        return;
    };
    (*s).painted_offset = Some(now);
    let strip = if dy < 0 {
        RECT {
            top: rows.bottom + dy,
            ..rows
        }
    } else {
        RECT {
            bottom: rows.top + dy,
            ..rows
        }
    };
    let mut repaint = Region::rect(&strip);
    repaint.add_rect(&lane(s, area));
    let due = Region::rect(&RECT::default());
    if GetUpdateRgn(hwnd, due.0, 0) > NULLREGION {
        repaint.add(&due, 0);
        repaint.add(&due, dy);
    }
    repaint.clip_to(&rows);
    shift_rows(memory, rows, dy);
    let saved = SaveDC(memory);
    SelectClipRgn(memory, repaint.0);
    paint_to(s, memory);
    RestoreDC(memory, saved);
    let dc = GetDC(hwnd);
    if !dc.is_null() {
        (*s).back.present(dc, &rows);
        ReleaseDC(hwnd, dc);
    }
    // Everything due inside the rows was repainted; the rest (the header)
    // stays due for WM_PAINT.
    ValidateRect(hwnd, &rows);
}

unsafe fn lane(s: *mut State, area: RECT) -> RECT {
    let area = RECT {
        bottom: (area.bottom - horizontal::height(s)).max(area.top),
        ..area
    };
    let header = (*s).model.header_height().clamp(0, area.bottom.max(0));
    scroll::lane(
        RECT {
            top: header,
            ..area
        },
        (*s).model.dpi(),
    )
}

unsafe fn offset_px(s: *mut State) -> i32 {
    (*s).scroll.offset(&(*s).anim).round() as i32
}

unsafe fn row_rect(s: *mut State, row: usize) -> Option<RECT> {
    if row >= (*s).count || row + 1 >= (*s).tops.len() {
        return None;
    }
    let area = client(s);
    let tops = &(*s).tops;
    let top = (*s).model.header_height() + tops[row] - offset_px(s);
    let (pad, _) = (*s).model.padding();
    Some(RECT {
        left: pad,
        top,
        right: (area.right - pad).max(pad),
        bottom: top + tops[row + 1] - tops[row],
    })
}

unsafe fn cells(s: *mut State, r: RECT) -> Vec<RECT> {
    column_spans(s)
        .into_iter()
        .map(|(left, right)| RECT { left, right, ..r })
        .collect()
}

unsafe fn row_parts(s: *mut State, row: usize) -> Option<Parts> {
    let r = row_rect(s, row)?;
    let cells = cells(s, r);
    Some((*s).model.parts(row, r, &cells))
}

unsafe fn hit(s: *mut State, pt: POINT) -> Hit {
    let area = content_client(s);
    if !contains(&area, pt) {
        return Hit::Nothing;
    }
    let header = (*s).model.header_height();
    if pt.y < header {
        return column_spans(s)
            .iter()
            .position(|&(l, r)| pt.x >= l && pt.x < r)
            .filter(|&col| col < (*s).columns.len())
            .map_or(Hit::Nothing, Hit::Header);
    }
    let (pad, _) = (*s).model.padding();
    if pt.x < pad || pt.x >= area.right - pad {
        return Hit::Nothing;
    }
    match row_at(&(*s).tops, pt.y - header + offset_px(s)) {
        Some(row) if row < (*s).count => {
            if (*s).model.is_group(row) {
                Hit::Group(row)
            } else {
                Hit::Row(row)
            }
        }
        _ => Hit::Nothing,
    }
}

/// `pt` is on a row's interactive part (chevron, switch). The startup switch
/// ends 12 px from the right edge, inside the 14 px scrollbar lane: clicks
/// and hover there belong to the switch, not to the scrollbar.
unsafe fn on_part(s: *mut State, pt: POINT) -> bool {
    let Hit::Row(row) = hit(s, pt) else {
        return false;
    };
    row_parts(s, row).is_some_and(|parts| {
        [parts.chevron, parts.switch]
            .iter()
            .flatten()
            .any(|r| contains(r, pt))
    })
}

unsafe fn column_at(s: *mut State, x: i32) -> i32 {
    column_spans(s)
        .iter()
        .position(|&(l, r)| x >= l && x < r)
        .map_or(-1, |c| c as i32)
}

// ───────────────────────────── selection & notifications ─────────────────────────────

unsafe fn notify(
    s: *mut State,
    code: u32,
    row: Option<usize>,
    col: i32,
    pt: POINT,
    new: u32,
    old: u32,
) {
    let hwnd = (*s).hwnd;
    let parent = GetParent(hwnd);
    if parent.is_null() {
        return;
    }
    let id = GetDlgCtrlID(hwnd) as usize;
    let hdr = NMHDR {
        hwndFrom: hwnd,
        idFrom: id,
        code,
    };
    let item = row.map_or(-1, |r| r as i32);
    match code {
        NM_CLICK | NM_DBLCLK | NM_RCLICK => {
            let nm = NMITEMACTIVATE {
                hdr,
                iItem: item,
                iSubItem: col,
                ptAction: pt,
                ..zeroed()
            };
            SendMessageW(parent, WM_NOTIFY, id, &nm as *const _ as isize);
        }
        NM_RETURN => {
            SendMessageW(parent, WM_NOTIFY, id, &hdr as *const _ as isize);
        }
        _ => {
            let nm = NMLISTVIEW {
                hdr,
                iItem: item,
                iSubItem: col,
                uNewState: new,
                uOldState: old,
                uChanged: if code == LVN_ITEMCHANGED {
                    LVIF_STATE
                } else {
                    0
                },
                ptAction: pt,
                lParam: 0,
            };
            SendMessageW(parent, WM_NOTIFY, id, &nm as *const _ as isize);
        }
    }
}

unsafe fn command(s: *mut State, code: u32) {
    let hwnd = (*s).hwnd;
    let parent = GetParent(hwnd);
    if !parent.is_null() {
        let id = GetDlgCtrlID(hwnd) as usize & 0xffff;
        SendMessageW(
            parent,
            WM_COMMAND,
            id | (code as usize) << 16,
            hwnd as isize,
        );
    }
}

/// Change the selection. Table mode notifies like a ListView (always, per
/// changed item); list mode sends LBN_SELCHANGE for user changes only.
unsafe fn select(s: *mut State, row: Option<usize>, user: bool) -> bool {
    let row = row.filter(|&r| r < (*s).count && !(*s).model.is_group(r));
    let old = (*s).selected;
    if old == row {
        return false;
    }
    (*s).selected = row;
    InvalidateRect((*s).hwnd, null(), 0);
    let state = LVIS_SELECTED | LVIS_FOCUSED;
    match (*s).mode {
        Mode::Table => {
            let none = POINT { x: 0, y: 0 };
            if let Some(old) = old {
                notify(s, LVN_ITEMCHANGED, Some(old), 0, none, 0, state);
            }
            if let Some(new) = (*s).selected {
                notify(s, LVN_ITEMCHANGED, Some(new), 0, none, state, 0);
            }
        }
        Mode::List if user => command(s, LBN_SELCHANGE),
        Mode::List => {}
    }
    access::selection_changed(s);
    true
}

/// Scroll `row` into view below the sticky header, together with the group
/// rows directly above it (a group's first row brings its title along).
unsafe fn reveal(s: *mut State, row: usize, animate: bool) {
    if row + 1 >= (*s).tops.len() {
        return;
    }
    let mut first = row;
    while first > 0 && (*s).model.is_group(first - 1) {
        first -= 1;
    }
    let tops = &(*s).tops;
    // The first and last rows bring the content padding along.
    let (_, pad) = (*s).model.padding();
    let top = if first == 0 { 0 } else { tops[first] };
    let bottom = tops[row + 1] + if row + 1 == (*s).count { pad } else { 0 };
    let (top, bottom) = (top as f32, bottom as f32);
    (*s).scroll.reveal(&mut (*s).anim, top, bottom, animate);
    InvalidateRect((*s).hwnd, null(), 0);
}

unsafe fn set_count(s: *mut State, count: usize) {
    (*s).count = count;
    if (*s).selected.is_some_and(|r| r >= count) {
        (*s).selected = None;
    }
    if (*s).hover.is_some_and(|r| r >= count) {
        set_row_hover(s, None);
    }
    if (*s).hover_chevron.is_some_and(|r| r >= count) {
        set_chevron_hover(s, None);
    }
    relayout(s);
    InvalidateRect((*s).hwnd, null(), 0);
}

// ───────────────────────────── hover ─────────────────────────────

/// Hover backgrounds cross-fade (100 ms in, 150 ms out, ease-out); while
/// the content glides under a resting pointer they `snap` instead, so a
/// scroll leaves no trail of fading rows.
unsafe fn fade(s: *mut State, old: Option<Key>, new: Option<Key>, snap: bool) {
    // Only the hovered row / header cell repaints while it fades.
    for key in [old, new].into_iter().flatten() {
        if let Some(r) = key_region(s, key) {
            (*s).anim.register(key, (*s).hwnd, Some(r));
        }
    }
    if let Some(key) = old {
        if snap {
            (*s).anim.set(key, 0.0);
        } else {
            (*s).anim
                .set_target(key, 0.0, motion::HOVER_OUT, Easing::EaseOut);
        }
    }
    if let Some(key) = new {
        if snap {
            (*s).anim.set(key, 1.0);
        } else {
            (*s).anim
                .set_target(key, 1.0, motion::HOVER_IN, Easing::EaseOut);
        }
    }
}

/// What a hover key repaints: its row (rows move when scrolling; `paint_to`
/// re-registers the rectangles it drew) or its header cell.
unsafe fn key_region(s: *mut State, key: Key) -> Option<RECT> {
    match key {
        Key::Row(row) | Key::ChevronHover(row) => row_rect(s, row),
        Key::Header(col) => {
            let bottom = (*s).model.header_height();
            column_spans(s)
                .get(col)
                .filter(|_| col < (*s).columns.len())
                .map(|&(left, right)| RECT {
                    left,
                    top: 0,
                    right,
                    bottom,
                })
        }
        _ => None,
    }
}

unsafe fn set_row_hover(s: *mut State, row: Option<usize>) {
    set_row_hover_with(s, row, false);
}

unsafe fn set_row_hover_with(s: *mut State, row: Option<usize>, snap: bool) {
    if (*s).hover != row {
        let old = (*s).hover.map(Key::Row);
        (*s).hover = row;
        fade(s, old, row.map(Key::Row), snap);
    }
}

unsafe fn set_header_hover(s: *mut State, col: Option<usize>) {
    if (*s).hover_header != col {
        let old = (*s).hover_header.map(Key::Header);
        (*s).hover_header = col;
        fade(s, old, col.map(Key::Header), false);
    }
}

unsafe fn set_chevron_hover(s: *mut State, row: Option<usize>) {
    set_chevron_hover_with(s, row, false);
}

unsafe fn set_chevron_hover_with(s: *mut State, row: Option<usize>, snap: bool) {
    if (*s).hover_chevron != row {
        let old = (*s).hover_chevron.map(Key::ChevronHover);
        (*s).hover_chevron = row;
        fade(s, old, row.map(Key::ChevronHover), snap);
    }
}

unsafe fn mouse_move(s: *mut State, pt: POINT) {
    if let Some((column, origin, original)) = (*s).resizing {
        let width =
            (original + (pt.x - origin) as f32 * 96.0 / (*s).model.dpi() as f32).clamp(48.0, 800.0);
        if let Some(entry) = (&mut (*s).columns).get_mut(column) {
            entry.width = width;
            entry.flex = false;
        }
        sync_horizontal(s);
        (*s).painted_offset = None;
        InvalidateRect((*s).hwnd, null(), 0);
        return;
    }
    if (*s).pressed_header.is_some()
        && (*s).model.editable_columns()
        && (pt.x - (*s).header_origin).abs() > gfx::pxi((*s).model.dpi(), 5.0)
    {
        (*s).header_dragged = true;
        invalidate_header((*s).hwnd);
        // Reveal adjacent columns without a timer; movement drives scrolling.
        let width = client(s).right;
        if pt.x < 20 {
            horizontal_to(s, (*s).horizontal_offset - 18);
        } else if pt.x > width - 20 {
            horizontal_to(s, (*s).horizontal_offset + 18);
        }
    }
    hover_at(s, pt, false);
}

unsafe fn resize_edge(s: *mut State, pt: POINT) -> Option<usize> {
    if !(*s).model.editable_columns() || pt.y < 0 || pt.y >= (*s).model.header_height() {
        return None;
    }
    let radius = gfx::pxi((*s).model.dpi(), 5.0);
    column_spans(s)
        .iter()
        .position(|&(_, right)| (pt.x - right).abs() <= radius)
}

/// Update hover (rows, header, chevron, scrollbar lane) for the pointer at
/// `pt`; `snap` = no cross-fade (the content moved, not the pointer).
unsafe fn hover_at(s: *mut State, pt: POINT, snap: bool) {
    let hwnd = (*s).hwnd;
    if !(*s).tracking {
        let mut e = TRACKMOUSEEVENT {
            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        (*s).tracking = TrackMouseEvent(&mut e) != 0;
    }
    let area = client(s);
    let lane = lane(s, area);
    if (*s).scroll.is_dragging() {
        let min = gfx::px((*s).model.dpi(), widgets::SCROLL_THUMB_MIN);
        (*s).scroll.drag_to(&mut (*s).anim, lane, pt, min);
        sync_scroll(s);
        return;
    }
    // A row's switch / chevron wins over the scrollbar lane it overlaps.
    let lane_pt = (!on_part(s, pt)).then_some(pt);
    let in_lane = (*s).scroll.pointer(&mut (*s).anim, lane, lane_pt);
    let target = if in_lane { Hit::Nothing } else { hit(s, pt) };
    set_header_hover(
        s,
        match target {
            Hit::Header(col) => Some(col),
            _ => None,
        },
    );
    let row = match target {
        Hit::Row(row) => Some(row),
        _ => None,
    };
    set_row_hover_with(s, row, snap);
    let chevron = row.filter(|&row| {
        row_parts(s, row)
            .and_then(|parts| parts.chevron)
            .is_some_and(|r| contains(&r, pt))
    });
    set_chevron_hover_with(s, chevron, snap);
}

unsafe fn mouse_leave(s: *mut State) {
    (*s).tracking = false;
    let lane = lane(s, client(s));
    (*s).scroll.pointer(&mut (*s).anim, lane, None);
    set_row_hover(s, None);
    set_header_hover(s, None);
    set_chevron_hover(s, None);
}

/// While the content glides under a resting pointer, the hovered row
/// follows the pointer (snapping, see [`fade`]).
unsafe fn refresh_hover(s: *mut State) {
    let mut pt: POINT = zeroed();
    if GetCursorPos(&mut pt) == 0 || WindowFromPoint(pt) != (*s).hwnd {
        return;
    }
    ScreenToClient((*s).hwnd, &mut pt);
    if !(*s).scroll.is_dragging() {
        hover_at(s, pt, true);
    }
}

// ───────────────────────────── input ─────────────────────────────

unsafe fn line_px(s: *mut State) -> f32 {
    (*s).model.line().max(1) as f32
}

unsafe fn button_down(s: *mut State, pt: POINT, double: bool) {
    let hwnd = (*s).hwnd;
    if GetFocus() != hwnd {
        SetFocus(hwnd);
    }
    if let Some(column) = resize_edge(s, pt) {
        let span = column_spans(s)[column];
        (*s).resizing = Some((
            column,
            pt.x,
            (span.1 - span.0) as f32 * 96.0 / (*s).model.dpi() as f32,
        ));
        SetCapture(hwnd);
        return;
    }
    let area = client(s);
    let lane = lane(s, area);
    let min = gfx::px((*s).model.dpi(), widgets::SCROLL_THUMB_MIN);
    if !on_part(s, pt) && (*s).scroll.press(&mut (*s).anim, lane, pt, min, line_px(s)) {
        SetCapture(hwnd);
        return;
    }
    match hit(s, pt) {
        Hit::Header(col) if (*s).mode == Mode::Table => {
            (*s).pressed_header = Some(col);
            (*s).header_origin = pt.x;
            (*s).header_dragged = false;
            SetCapture(hwnd);
        }
        Hit::Row(row) => {
            // No scrolling on a click: the row stays under the pointer so
            // the release lands on the same row (NM_CLICK, switch, chevron).
            select(s, Some(row), true);
            if double {
                (*s).pressed = None;
                match (*s).mode {
                    Mode::Table => notify(s, NM_DBLCLK, Some(row), column_at(s, pt.x), pt, 0, 0),
                    Mode::List => command(s, LBN_DBLCLK),
                }
            } else {
                (*s).pressed = Some(row);
                SetCapture(hwnd);
            }
        }
        _ => {}
    }
}

/// Saves a finished divider drag. A click or double-click without movement
/// changes nothing: a flexible column has no width of its own until it moves.
unsafe fn commit_resize(s: *mut State, column: usize) {
    if let Some(entry) = (&(*s).columns).get(column).filter(|entry| !entry.flex) {
        (*s).model.resize_column(column, entry.width);
    }
}

unsafe fn button_up(s: *mut State, pt: POINT) {
    let resizing = (*s).resizing.take();
    let header = (*s).pressed_header.take();
    let header_dragged = std::mem::take(&mut (*s).header_dragged);
    let pressed = (*s).pressed.take();
    let dragging = (*s).scroll.is_dragging();
    if GetCapture() == (*s).hwnd {
        ReleaseCapture();
    }
    if let Some((column, _, _)) = resizing {
        commit_resize(s, column);
        return;
    }
    if dragging {
        (*s).scroll.release(&mut (*s).anim);
        return;
    }
    if let Some(col) = header {
        if header_dragged {
            if let Hit::Header(target) = hit(s, pt) {
                (*s).model.reorder_column(col, target);
            }
            invalidate_header((*s).hwnd);
        } else if hit(s, pt) == Hit::Header(col) {
            notify(s, LVN_COLUMNCLICK, None, col as i32, pt, 0, 0);
        }
        return;
    }
    if let Some(row) = pressed {
        if (*s).mode == Mode::Table && hit(s, pt) == Hit::Row(row) {
            notify(s, NM_CLICK, Some(row), column_at(s, pt.x), pt, 0, 0);
        }
    }
}

/// The row's default action (accessibility `accDoDefaultAction`): select
/// it and send the double-click notification a pointer would.
unsafe fn activate(s: *mut State, row: usize) {
    if row >= (*s).count || (*s).model.is_group(row) {
        return;
    }
    select(s, Some(row), true);
    reveal(s, row, false);
    match (*s).mode {
        Mode::Table => {
            // A point inside the row, left of the name cell's chevron.
            let pt = row_rect(s, row).map_or(zeroed(), |r| POINT {
                x: r.left + 1,
                y: (r.top + r.bottom) / 2,
            });
            notify(s, NM_DBLCLK, Some(row), 0, pt, 0, 0);
        }
        Mode::List => command(s, LBN_DBLCLK),
    }
}

unsafe fn page_rows(s: *mut State) -> usize {
    let view = (*s).scroll.extent().view;
    ((view / line_px(s)).floor() as usize)
        .saturating_sub(1)
        .max(1)
}

unsafe fn navigate(s: *mut State, nav: Nav) {
    let model = &*(*s).model;
    let next = step(
        (*s).count,
        |row| model.is_group(row),
        (*s).selected,
        nav,
        page_rows(s),
    );
    if let Some(row) = next {
        select(s, Some(row), true);
        reveal(s, row, true);
    }
}

/// Keyboard focus is visible (`:focus-visible`): the control has focus and
/// the window's UI state shows focus cues. Windows hides them until the
/// keyboard is used (IsDialogMessage clears `UISF_HIDEFOCUS` on Tab), like
/// the app's buttons (`ODS_NOFOCUSRECT`).
unsafe fn focus_visible(s: *mut State) -> bool {
    let hwnd = (*s).hwnd;
    (*s).staged_focus
        || GetFocus() == hwnd
            && SendMessageW(hwnd, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS == 0
}

/// The row the focus ring marks: the selection, else the first data row
/// (where Down / Home would put it).
unsafe fn focus_row(s: *mut State) -> Option<usize> {
    let model = &*(*s).model;
    (*s).selected
        .or_else(|| step((*s).count, |row| model.is_group(row), None, Nav::Home, 1))
}

/// Keyboard use in the control shows the focus cues of the whole window
/// (like a ListView): the top-level window broadcasts WM_UPDATEUISTATE.
unsafe fn show_focus_cues(s: *mut State) {
    let hwnd = (*s).hwnd;
    let hidden = || SendMessageW(hwnd, WM_QUERYUISTATE, 0, 0) as u32 & UISF_HIDEFOCUS != 0;
    let clear = (UISF_HIDEFOCUS << 16 | UIS_CLEAR) as usize;
    if hidden() {
        SendMessageW(hwnd, WM_CHANGEUISTATE, clear, 0);
    }
    if hidden() {
        // The top-level window already shows them (an inconsistent tree).
        SendMessageW(hwnd, WM_UPDATEUISTATE, clear, 0);
    }
}

unsafe fn key_down(s: *mut State, key: u16) -> bool {
    let nav = match key {
        VK_UP => Nav::Up,
        VK_DOWN => Nav::Down,
        VK_PRIOR => Nav::PageUp,
        VK_NEXT => Nav::PageDown,
        VK_HOME => Nav::Home,
        VK_END => Nav::End,
        VK_RETURN if (*s).mode == Mode::Table => {
            if let Some(row) = (*s).selected {
                notify(s, NM_RETURN, Some(row), 0, zeroed(), 0, 0);
            }
            return true;
        }
        _ => return false,
    };
    show_focus_cues(s);
    navigate(s, nav);
    true
}

unsafe fn item_text(s: *mut State, row: usize) -> String {
    match (*s).mode {
        Mode::List => (&(*s).items).get(row).cloned().unwrap_or_default(),
        Mode::Table => (*s).model.text(row, 0),
    }
}

/// Type-to-select: the next row whose first cell starts with the typed text
/// (repeating one letter cycles through the rows starting with it).
unsafe fn typeahead(s: *mut State, unit: u32) {
    let Some(ch) = char::from_u32(unit).filter(|c| !c.is_control()) else {
        return;
    };
    show_focus_cues(s);
    let now = Instant::now();
    if (*s)
        .typed_at
        .is_none_or(|at| now.saturating_duration_since(at) > TYPEAHEAD)
    {
        (*s).typed.clear();
    }
    if ch == ' ' && (&(*s).typed).is_empty() {
        return;
    }
    (*s).typed_at = Some(now);
    (*s).typed.extend(ch.to_lowercase());
    let typed = (*s).typed.clone();
    let mut letters = typed.chars();
    let first = letters.next().unwrap_or(ch);
    let repeat = letters.all(|c| c == first);
    let needle = if repeat { first.to_string() } else { typed };
    let count = (*s).count;
    if count == 0 {
        return;
    }
    let start = match (*s).selected {
        Some(row) if repeat => row + 1,
        Some(row) => row,
        None => 0,
    };
    for k in 0..count {
        let row = (start + k) % count;
        if (*s).model.is_group(row) {
            continue;
        }
        if item_text(s, row).to_lowercase().starts_with(&needle) {
            select(s, Some(row), true);
            reveal(s, row, true);
            return;
        }
    }
}

unsafe fn wheel(s: *mut State, delta: i32) -> bool {
    if !(*s).scroll.extent().scrollable() {
        return false;
    }
    // The frames repaint incrementally (`sync_scroll`), the bar its lane:
    // no full repaint per wheel message (touchpads send dozens a second).
    (*s).scroll.wheel(&mut (*s).anim, delta, line_px(s));
    true
}

// ───────────────────────────── painting ─────────────────────────────

unsafe fn row_state(s: *mut State, row: usize, visible: &mut Vec<u64>) -> RowPaint {
    let model = &*(*s).model;
    let key = model.key(row);
    visible.push(key);
    let chevron = model.expanded(row).map_or(0.0, |open| {
        (*s).anim.follow(
            Key::Chevron(key),
            if open { 90.0 } else { 0.0 },
            motion::CHEVRON,
            Easing::Ease,
        )
    });
    let switch = model.switch_on(row).map_or(0.0, |on| {
        (*s).anim.follow(
            Key::Switch(key),
            on as u8 as f32,
            motion::SWITCH,
            Easing::Ease,
        )
    });
    RowPaint {
        hover: (*s).anim.value(Key::Row(row)),
        selected: (*s).selected == Some(row),
        focused: GetFocus() == (*s).hwnd,
        chevron,
        chevron_hover: (*s).anim.value(Key::ChevronHover(row)),
        switch,
    }
}

/// Paint the table into `dc` (the back buffer or a WM_PRINTCLIENT target);
/// returns the scroll offset painted and the clip box it covered.
unsafe fn paint_to(s: *mut State, dc: HDC) -> (i32, RECT) {
    if (*s).model.dpi() != (*s).dpi {
        relayout(s);
    }
    let area = client(s);
    let model: &dyn Model = &*(*s).model;
    let dpi = model.dpi();
    let pt = Painter::new(dc, dpi, model.fonts());
    pt.fill(area, pt.c.surface);
    let header = model.header_height().clamp(0, area.bottom.max(0));
    let spans = column_spans(s);
    let offset = offset_px(s);
    // Rows outside the update region are not drawn (a hover fade repaints
    // one row); their animation state is still tracked below.
    let mut clip: RECT = zeroed();
    if GetClipBox(dc, &mut clip) == 0 {
        clip = area;
    }
    // Against the clip *region*: a glide frame repaints a strip plus the
    // scrollbar lane, whose bounding box is everything.
    let drawn = |r: &RECT| RectVisible(dc, r) != 0;
    let hwnd = (*s).hwnd;
    let ring = if focus_visible(s) { focus_row(s) } else { None };
    let mut visible = Vec::new();
    if (*s).count == 0 {
        let band = RECT {
            top: header,
            bottom: (header + pt.pxi(EMPTY_HEIGHT)).min(area.bottom),
            ..area
        };
        paint_empty(&pt, band, &model.empty_text(), (*s).mode == Mode::Table);
    } else {
        let (pad, _) = model.padding();
        let first = offset.max((&(*s).tops)[0]);
        let mut row = row_at(&(*s).tops, first).unwrap_or((*s).count);
        while row < (*s).count && row + 1 < (*s).tops.len() {
            let tops = &(*s).tops;
            let top = header + tops[row] - offset;
            if top >= area.bottom {
                break;
            }
            let r = RECT {
                left: pad,
                top,
                right: (area.right - pad).max(pad),
                bottom: header + tops[row + 1] - offset,
            };
            if r.bottom > header {
                let st = row_state(s, row, &mut visible);
                for key in [Key::Row(row), Key::ChevronHover(row)] {
                    if (*s).anim.anim.target(key).is_some() {
                        (*s).anim.register(key, hwnd, Some(r));
                    }
                }
                if drawn(&r) {
                    // Cells outside the clip are empty (nothing to paint:
                    // under the lane only the last cell is due).
                    let cells: Vec<RECT> = spans
                        .iter()
                        .map(|&(left, right)| {
                            let cell = RECT { left, right, ..r };
                            if drawn(&cell) {
                                cell
                            } else {
                                RECT {
                                    right: left,
                                    ..cell
                                }
                            }
                        })
                        .collect();
                    model.paint_row(&pt, row, r, &cells, &st);
                    if ring == Some(row) {
                        let (element, radius) = model.focus_box(row, r);
                        widgets::focus_ring_inset(&pt, element, radius);
                    }
                }
            }
            row += 1;
        }
    }
    let band = RECT {
        bottom: header,
        ..area
    };
    if header > 0 && drawn(&band) {
        let mut right = band.left;
        let mut overflow = Vec::new();
        for (i, column) in (*s).columns.iter().enumerate() {
            let (left, r) = spans[i];
            let info = model.header(i);
            let cell = RECT {
                left,
                right: r,
                ..band
            };
            let hover = (*s).anim.value(Key::Header(i));
            overflow.extend(paint_header_cell(&pt, cell, column, &info, hover));
            right = r;
        }
        // After every cell: an overflowing total reaches into a neighbour.
        for (total, r, align) in overflow {
            widgets::header_total(&pt, pt.fonts.mono_total, &total, r, align);
        }
        if (*s).header_dragged {
            if let (Some(source), Some(target)) = ((*s).pressed_header, (*s).hover_header) {
                if source > 0 && target > 0 && source != target {
                    let edge = if target > source {
                        spans[target].1
                    } else {
                        spans[target].0
                    };
                    pt.fill(
                        RECT {
                            left: edge - pt.pxi(1.0),
                            right: edge + pt.pxi(1.0),
                            ..band
                        },
                        pt.c.accent,
                    );
                }
            }
        }
        if right < band.right {
            widgets::header_cell(
                &pt,
                RECT {
                    left: right,
                    ..band
                },
                None,
                "",
                false,
                false,
                None,
                0.0,
            );
        }
    }
    (*s).scroll.paint(&pt, &(*s).anim, lane(s, area));
    horizontal::paint(s, &pt);
    // Per-row keys stay bounded: hover keys settle back to 0 and go; chevron
    // and switch keys live while their rows are on screen.
    (*s).anim.retain(|key, value, animating| {
        animating
            || match *key {
                Key::Chevron(id) | Key::Switch(id) => visible.contains(&id),
                Key::Row(_) | Key::Header(_) | Key::ChevronHover(_) => value != 0.0,
                Key::Scroll | Key::Bar | Key::Grow => true,
            }
    });
    (offset, clip)
}

/// One sticky header cell (`widgets::header_cell`). A `.tot` line wider
/// than the cell's padding box overflows like the reference's visible
/// overflow ("123.4 MB/s" in the 100 px All I/O column): through its own
/// padding into the neighbour's 12 px padding, 2 px clear of the
/// neighbour's text, instead of falling back to a smaller face. Returns that
/// overflowing total and where it goes; the caller draws it after every
/// cell so the neighbour's background never covers it.
fn paint_header_cell(
    pt: &Painter,
    cell: RECT,
    column: &Column,
    info: &HeaderCell,
    hover: f32,
) -> Option<(String, RECT, u32)> {
    let pad = pt.pxi(12.0);
    let total = info.total.as_deref();
    let fits = total
        .is_none_or(|t| pt.measure(pt.fonts.mono_total, t).cx <= cell.right - cell.left - 2 * pad);
    widgets::header_cell(
        pt,
        cell,
        if fits { total } else { None },
        &column.label,
        info.two_line,
        column.right,
        info.sort,
        hover,
    );
    let total = total.filter(|_| !fits)?;
    // The widget's line boxes: 8 px padding, `.lab` 12/1.45, `.tot` 15/1.2 above it.
    let bottom = cell.bottom - pt.hair() as i32 - pt.pxi(8.0) - (pt.px(12.0) * 1.45).round() as i32;
    let top = bottom - (pt.px(15.0) * 1.2).round() as i32;
    let reach = pad - pt.pxi(2.0);
    let (r, align) = if column.right {
        (
            RECT {
                left: cell.left - reach,
                top,
                right: cell.right - pad,
                bottom,
            },
            DT_RIGHT,
        )
    } else {
        (
            RECT {
                left: cell.left + pad,
                top,
                right: cell.right + reach,
                bottom,
            },
            DT_LEFT,
        )
    };
    Some((total.to_owned(), r, align))
}

/// The All I/O column total: whole MB/s from 100 MB/s ("136 MB/s") and
/// GB/s from 1000 MB/s ("1.2 GB/s"), so a busy disk's total stays inside
/// the 100 px column (at most its own padding over the edge) and never
/// reads as one string with the Memory total.
fn total_rate(bytes_per_sec: f64) -> String {
    const MB: f64 = 1048576.0;
    if bytes_per_sec.is_finite() && bytes_per_sec >= 999.5 * MB {
        format!("{:.1} GB/s", bytes_per_sec / (1024.0 * MB))
    } else if bytes_per_sec.is_finite() && bytes_per_sec >= 99.95 * MB {
        format!("{:.0} MB/s", bytes_per_sec / MB)
    } else {
        format!("{}/s", super::rate(bytes_per_sec))
    }
}

/// `<td colspan style="height:120px; text-align:center; color:muted">`.
unsafe fn paint_empty(pt: &Painter, band: RECT, text: &str, border: bool) {
    if band.bottom <= band.top {
        return;
    }
    let font = pt.fonts.body;
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    let height = pt.measure(font, "Ag").cy.max(1);
    let total = height * lines.len() as i32;
    let mut y = band.top + (band.bottom - band.top - total) / 2;
    for line in lines {
        pt.label(
            font,
            pt.c.muted,
            line,
            RECT {
                left: band.left + pt.pxi(12.0),
                right: band.right - pt.pxi(12.0),
                top: y,
                bottom: y + height,
            },
            DT_CENTER,
        );
        y += height;
    }
    if border {
        pt.fill(
            RECT {
                top: band.bottom - pt.hair() as i32,
                ..band
            },
            pt.c.row_border,
        );
    }
}

// ───────────────────────────── message emulation ─────────────────────────────

unsafe fn copy_text(text: &str, out: *mut u16, capacity: i32) {
    if out.is_null() || capacity <= 0 {
        return;
    }
    let mut n = 0;
    for unit in text.encode_utf16().take(capacity as usize - 1) {
        *out.add(n) = unit;
        n += 1;
    }
    *out.add(n) = 0;
}

unsafe fn wide_arg(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *ptr.add(len) != 0 && len < 32_768 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}

/// The ListView messages the app sends (LVM_*); None = not handled.
unsafe fn list_view_message(s: *mut State, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    let index = w as isize;
    Some(match msg {
        LVM_GETITEMCOUNT => (*s).count as isize,
        LVM_SETITEMCOUNT => {
            set_count(s, w);
            1
        }
        LVM_GETNEXTITEM => {
            let flags = l as u32;
            if flags & (LVNI_SELECTED | LVNI_FOCUSED) != 0 {
                match (*s).selected {
                    Some(row) if index < 0 || row as isize > index => row as isize,
                    _ => -1,
                }
            } else {
                let next = if index < 0 { 0 } else { index + 1 };
                if (next as usize) < (*s).count {
                    next
                } else {
                    -1
                }
            }
        }
        LVM_SETITEMSTATE => {
            if l == 0 {
                return Some(0);
            }
            let item = &*(l as *const LVITEMW);
            if item.stateMask & LVIS_SELECTED == 0 {
                return Some(1);
            }
            let on = item.state & LVIS_SELECTED != 0;
            if index < 0 {
                if !on {
                    select(s, None, false);
                }
                return Some(1);
            }
            let row = index as usize;
            if row >= (*s).count {
                return Some(0);
            }
            if on {
                // Group rows are never selected: selecting one clears the selection.
                let target = (!(*s).model.is_group(row)).then_some(row);
                select(s, target, false);
            } else if (*s).selected == Some(row) {
                select(s, None, false);
            }
            1
        }
        LVM_GETITEMSTATE => {
            if (*s).selected == Some(w) {
                ((LVIS_SELECTED | LVIS_FOCUSED) & l as u32) as isize
            } else {
                0
            }
        }
        LVM_ENSUREVISIBLE => {
            if w < (*s).count {
                reveal(s, w, true);
                1
            } else {
                0
            }
        }
        LVM_GETITEMRECT => {
            let Some(r) = row_rect(s, w) else {
                return Some(0);
            };
            if l != 0 {
                *(l as *mut RECT) = r;
            }
            1
        }
        LVM_GETSUBITEMRECT => {
            if l == 0 {
                return Some(0);
            }
            let out = &mut *(l as *mut RECT);
            let col = out.top.max(0) as usize;
            let Some(r) = row_rect(s, w) else {
                return Some(0);
            };
            let Some(cell) = cells(s, r).get(col).copied() else {
                return Some(0);
            };
            *out = cell;
            1
        }
        LVM_GETITEMW => {
            if l == 0 {
                return Some(0);
            }
            let item = &mut *(l as *mut LVITEMW);
            let row = item.iItem;
            if row < 0 || row as usize >= (*s).count {
                return Some(0);
            }
            if item.mask & LVIF_TEXT != 0 {
                let text = (*s).model.text(row as usize, item.iSubItem.max(0) as usize);
                copy_text(&text, item.pszText, item.cchTextMax);
            }
            if item.mask & LVIF_STATE != 0 {
                item.state = if (*s).selected == Some(row as usize) {
                    (LVIS_SELECTED | LVIS_FOCUSED) & item.stateMask
                } else {
                    0
                };
            }
            1
        }
        LVM_GETCOLUMNWIDTH => column_spans(s)
            .get(w)
            .filter(|_| w < (*s).columns.len())
            .map_or(0, |&(l, r)| (r - l) as isize),
        LVM_GETHEADER => 0,
        LVM_GETTOPINDEX => row_at(&(*s).tops, offset_px(s).max(0)).map_or(0, |r| r as isize),
        LVM_GETCOUNTPERPAGE => page_rows(s) as isize + 1,
        LVM_SETBKCOLOR | LVM_SETTEXTBKCOLOR | LVM_SETTEXTCOLOR => 1,
        _ => return None,
    })
}

/// The ListBox messages the device list receives (LB_*); None = not handled.
unsafe fn list_box_message(s: *mut State, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    Some(match msg {
        LB_GETCOUNT => (*s).count as isize,
        LB_RESETCONTENT => {
            (*s).items.clear();
            set_count(s, 0);
            0
        }
        LB_ADDSTRING => {
            (*s).items.push(wide_arg(l as *const u16));
            set_count(s, (*s).items.len());
            (*s).count as isize - 1
        }
        LB_SETCURSEL => {
            let index = w as isize;
            let row = (index >= 0 && (index as usize) < (*s).count).then_some(index as usize);
            select(s, row, false);
            if let Some(row) = row {
                reveal(s, row, false);
                row as isize
            } else {
                LB_ERR as isize
            }
        }
        LB_GETCURSEL => (*s).selected.map_or(LB_ERR as isize, |r| r as isize),
        LB_GETITEMHEIGHT => (*s).model.row_height(0) as isize,
        LB_SETITEMHEIGHT => 0,
        LB_GETTEXTLEN => (&(*s).items)
            .get(w)
            .map_or(LB_ERR as isize, |t| t.encode_utf16().count() as isize),
        LB_GETTEXT => match (&(*s).items).get(w).filter(|_| l != 0) {
            Some(text) => {
                let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
                std::ptr::copy_nonoverlapping(units.as_ptr(), l as *mut u16, units.len());
                units.len() as isize - 1
            }
            None => LB_ERR as isize,
        },
        _ => return None,
    })
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(l as *const CREATESTRUCTW);
        let init = cs.lpCreateParams as *mut Init;
        let Some(model) = init.as_mut().and_then(|init| init.model.take()) else {
            return 0;
        };
        let s = Box::into_raw(Box::new(State::new(hwnd, (*init).mode, model)));
        (*s).anim.attach(hwnd);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, s as isize);
        return DefWindowProcW(hwnd, msg, w, l);
    }
    let s = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if s.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    if let Some(result) = horizontal::message(s, msg, w, l) {
        return result;
    }
    match msg {
        WM_NCDESTROY => {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(s));
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_DESTROY => {
            (*s).anim.stop();
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            if !(*s).redraw {
                ValidateRect(hwnd, null());
                return 0;
            }
            let mut back = std::mem::take(&mut (*s).back);
            gfx::paint_buffered(hwnd, &mut back, |dc, _| {
                let (offset, clip) = paint_to(s, dc);
                // Every row repainted: the screen shows this offset now.
                let rows = rows_area(s);
                if clip.left <= rows.left
                    && clip.top <= rows.top
                    && clip.right >= rows.right
                    && clip.bottom >= rows.bottom
                {
                    (*s).painted_offset = Some(offset);
                }
            });
            (*s).back = back;
            0
        }
        WM_PRINTCLIENT => {
            paint_to(s, w as HDC);
            0
        }
        WM_SIZE => {
            relayout(s);
            0
        }
        WM_SHOWWINDOW => {
            if w == 0 {
                // Hidden (page switch): no stale hover or bar, no cached frame.
                (*s).back.release();
                set_row_hover(s, None);
                set_header_hover(s, None);
                set_chevron_hover(s, None);
                (*s).scroll.stage(&mut (*s).anim, 0.0, 0.0);
                (*s).anim.finish_all();
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        // Not DefWindowProc: it would toggle WS_VISIBLE of a hidden list.
        WM_SETREDRAW => {
            (*s).redraw = w != 0;
            if w != 0 {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_SETFONT | WM_GETFONT => 0,
        WM_GETDLGCODE => {
            let mut code = DLGC_WANTARROWS | DLGC_WANTCHARS;
            let message = l as *const MSG;
            if (*s).mode == Mode::Table
                && !message.is_null()
                && (*message).message == WM_KEYDOWN
                && (*message).wParam as u16 == VK_RETURN
            {
                code |= DLGC_WANTMESSAGE;
            }
            code as isize
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            InvalidateRect(hwnd, null(), 0);
            if msg == WM_SETFOCUS {
                access::focus_gained(s);
            }
            0
        }
        // Focus cues shown / hidden for the window (the focus ring).
        WM_UPDATEUISTATE => {
            let result = DefWindowProcW(hwnd, msg, w, l);
            InvalidateRect(hwnd, null(), 0);
            result
        }
        WM_GETOBJECT => {
            access::get_object(hwnd, w, l).unwrap_or_else(|| DefWindowProcW(hwnd, msg, w, l))
        }
        access::WM_ANNOUNCE => {
            access::announce(s);
            0
        }
        access::WM_ACTIVATE_ROW => {
            activate(s, w);
            0
        }
        WM_TIMER => {
            let scrolling = (*s).anim.anim.is_key_animating(Key::Scroll);
            if (*s).anim.on_timer(w) {
                if scrolling {
                    // The hover moves first, so the shifted rows and the new
                    // highlight are presented together.
                    refresh_hover(s);
                    sync_scroll(s);
                }
                0
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        // Header buttons, chevrons, switches and device cards are the
        // reference's `<button>`s (pointer); rows keep `cursor: default`.
        WM_SETCURSOR if w as HWND == hwnd && (l & 0xffff) as u32 == HTCLIENT => {
            let mut pt: POINT = zeroed();
            GetCursorPos(&mut pt);
            ScreenToClient(hwnd, &mut pt);
            if (*s).resizing.is_some() || resize_edge(s, pt).is_some() {
                SetCursor(LoadCursorW(null_mut(), IDC_SIZEWE));
                return 1;
            }
            if (*s).header_dragged {
                SetCursor(LoadCursorW(null_mut(), IDC_SIZEALL));
                return 1;
            }
            let lane = lane(s, client(s));
            let hand = match hit(s, pt) {
                _ if contains(&lane, pt)
                    && (*s).scroll.extent().scrollable()
                    && !on_part(s, pt) =>
                {
                    false
                }
                Hit::Header(_) => (*s).mode == Mode::Table,
                Hit::Row(_) => (*s).mode == Mode::List || on_part(s, pt),
                _ => false,
            };
            widgets::set_pointer(hand)
        }
        WM_MOUSEMOVE => {
            mouse_move(s, point(l));
            0
        }
        WM_MOUSELEAVE => {
            mouse_leave(s);
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            button_down(s, point(l), msg == WM_LBUTTONDBLCLK);
            0
        }
        WM_LBUTTONUP => {
            button_up(s, point(l));
            0
        }
        WM_RBUTTONDOWN => {
            if GetFocus() != hwnd {
                SetFocus(hwnd);
            }
            // Like a ListView: right-click selects the row the menu is for.
            if let Hit::Row(row) = hit(s, point(l)) {
                select(s, Some(row), true);
            }
            0
        }
        WM_CONTEXTMENU if (*s).model.editable_columns() => {
            let keyboard =
                (l & 0xffff) as u16 as i16 == -1 && ((l >> 16) & 0xffff) as u16 as i16 == -1;
            let target = if keyboard {
                // Shift+F10 / the menu key while pointing at a header cell:
                // that column's menu below the cell (rows keep their menu).
                (*s).hover_header.and_then(|column| {
                    let (left, _) = *column_spans(s).get(column)?;
                    let mut at = POINT {
                        x: left.max(0),
                        y: (*s).model.header_height(),
                    };
                    ClientToScreen(hwnd, &mut at);
                    Some((column, at))
                })
            } else {
                let mut pt = point(l);
                ScreenToClient(hwnd, &mut pt);
                match hit(s, pt) {
                    Hit::Header(column) => Some((column, point(l))),
                    _ => None,
                }
            };
            if let Some((column, at)) = target {
                (*s).model.column_menu(Some(column), at);
                return 0;
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_CAPTURECHANGED => {
            (*s).scroll.release(&mut (*s).anim);
            (*s).pressed = None;
            (*s).pressed_header = None;
            (*s).header_dragged = false;
            if let Some((column, _, _)) = (*s).resizing.take() {
                commit_resize(s, column);
            }
            0
        }
        // The themed bar replaces Windows' native one (which ignores the
        // dark theme); scroll requests from other sources still work.
        WM_HSCROLL => {
            let now = (*s).horizontal_offset;
            let page = client(s).right.max(1);
            let next = match (w & 0xffff) as i32 {
                SB_LINELEFT => now - gfx::pxi((*s).model.dpi(), 32.0),
                SB_LINERIGHT => now + gfx::pxi((*s).model.dpi(), 32.0),
                SB_PAGELEFT => now - page,
                SB_PAGERIGHT => now + page,
                SB_LEFT => 0,
                SB_RIGHT => (*s).horizontal_max,
                _ => now,
            };
            horizontal_to(s, next);
            0
        }
        WM_MOUSEHWHEEL => {
            let delta = ((w >> 16) & 0xffff) as u16 as i16 as i32;
            horizontal_to(s, (*s).horizontal_offset + delta);
            0
        }
        WM_MOUSEWHEEL => {
            let delta = ((w >> 16) & 0xffff) as u16 as i16 as i32;
            if w & 0x0004 != 0 && (*s).horizontal_max > 0 {
                horizontal_to(s, (*s).horizontal_offset - delta);
                return 0;
            }
            if wheel(s, delta) {
                0
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        WM_KEYDOWN => {
            if (*s).model.editable_columns()
                && w as u16 == b'C' as u16
                && GetKeyState(VK_CONTROL as i32) < 0
                && GetKeyState(VK_SHIFT as i32) < 0
            {
                let mut at = POINT {
                    x: 16,
                    y: (*s).model.header_height(),
                };
                ClientToScreen(hwnd, &mut at);
                (*s).model.column_menu(None, at);
                return 0;
            }
            if key_down(s, w as u16) {
                0
            } else {
                DefWindowProcW(hwnd, msg, w, l)
            }
        }
        WM_CHAR => {
            typeahead(s, w as u32);
            0
        }
        _ => match (*s).mode {
            Mode::Table => list_view_message(s, msg, w, l),
            Mode::List => list_box_message(s, msg, w, l),
        }
        .unwrap_or_else(|| DefWindowProcW(hwnd, msg, w, l)),
    }
}

// ───────────────────────────── the app's models ─────────────────────────────

fn hash_of(value: impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

/// Where the name cell's parts go (`.pname`: [chevron | pad] 8 [mono-ico] 8
/// [label]) inside the row's content box (the row minus its border).
struct NameBoxes {
    chevron: RECT,
    icon: RECT,
    text_left: i32,
    right: i32,
}

/// `icon_x` = the mono-ico's offset from the cell's 12 px padding (CSS px).
/// A deep tree level in a narrow column never pushes the icon out of its
/// cell.
fn name_boxes(dpi: i32, cell: RECT, icon_x: f32) -> NameBoxes {
    let d = |v: f32| gfx::pxi(dpi, v);
    let left = cell.left + d(12.0);
    let size = d(20.0);
    let top = cell.top + (cell.bottom - cell.top - size + 1) / 2;
    let icon_left = (left + d(icon_x))
        .min(cell.right - d(12.0) - size)
        .max(left);
    let chevron_left = icon_left - d(28.0);
    NameBoxes {
        chevron: RECT {
            left: chevron_left,
            top,
            right: chevron_left + size,
            bottom: top + size,
        },
        icon: RECT {
            left: icon_left,
            top,
            right: icon_left + size,
            bottom: top + size,
        },
        text_left: icon_left + size + d(8.0),
        right: cell.right - d(12.0),
    }
}

/// Mono-ico offset (CSS px after the cell padding) of a tree row. The
/// reference's `.pname` is [chevron | chevron pad] 8 [icon] at the top level
/// and `tr.child .pname { padding-left: 28px }` + [icon] (no pad) for
/// children, so a child's icon sits under its parent's. Deeper levels add
/// 20 px each (DESIGN_SPEC §4, capped). Every row of a depth — leaf or
/// parent — has its icon in the same column; a nested parent hangs its
/// chevron in the 28 px before its icon (the twistie slot every row
/// reserves), so siblings never form a staircase.
fn tree_icon_x(depth: usize, _has_children: bool) -> f32 {
    28.0 + 20.0 * depth.clamp(1, TREE_DEPTH_CAP).saturating_sub(1) as f32
}

/// The row's content box (without the 1 px row border).
fn content_of(pt_hair: i32, r: RECT) -> RECT {
    RECT {
        bottom: (r.bottom - pt_hair).max(r.top),
        ..r
    }
}

/// The line box to centre cell text in, measured on the reference's rows
/// (every data row after a group's first): 12 px muted cells sit on the
/// body's `line-height: 1.45` (17.4 px) baseline; 14 px text and the mono
/// numbers (12.5 px, 12 px in GDI) land on it with the font's own line box.
fn cell_line(pt: &Painter, font: HFONT) -> Option<f32> {
    (font == pt.fonts.small).then_some(12.0 * layout::BODY_LINE)
}

/// Single-line cell text inside the 12 px cell padding on the reference's
/// CSS baseline (the cell's line box centred in the row).
fn cell_text(pt: &Painter, font: HFONT, color: u32, text: &str, cell: RECT, align: u32) {
    if text.is_empty() {
        return;
    }
    let inner = RECT {
        left: cell.left + pt.pxi(12.0),
        right: cell.right - pt.pxi(12.0),
        ..cell
    };
    if inner.right <= inner.left {
        return;
    }
    let r = pt.css_rect(font, inner, cell_line(pt, font));
    // `text-overflow: ellipsis` with Chromium's single "…" glyph.
    let shown = pt.ellipsize(font, text, inner.right - inner.left);
    pt.text(
        font,
        color,
        &shown,
        RECT {
            left: inner.left,
            right: inner.right,
            ..r
        },
        DT_SINGLELINE | align,
    );
}

/// `.pname`: chevron (or nothing), mono-ico, label with an optional muted
/// 12 px suffix (`.exe` style: a parent's " (n)").
#[allow(clippy::too_many_arguments)]
fn name_cell(
    pt: &Painter,
    cell: RECT,
    icon_x: f32,
    chevron: Option<(f32, f32)>,
    initials: &str,
    child: bool,
    label: &str,
    suffix: Option<&str>,
) {
    if cell.right <= cell.left {
        return;
    }
    let boxes = name_boxes(pt.dpi, cell, icon_x);
    if let Some((angle, hover)) = chevron {
        widgets::chevron_button(pt, boxes.chevron, angle, hover);
    }
    widgets::mono_badge(pt, boxes.icon, initials, child);
    if boxes.right <= boxes.text_left {
        return;
    }
    let f = pt.fonts;
    let band = RECT {
        left: boxes.text_left,
        right: boxes.right,
        ..cell
    };
    let baseline = pt.css_baseline(f.body, band, None);
    let shown = pt.ellipsize(f.body, label, boxes.right - boxes.text_left);
    pt.text_on_baseline(f.body, pt.c.fg, &shown, band, baseline, DT_LEFT);
    if let Some(suffix) = suffix.filter(|_| shown == label) {
        let x = boxes.text_left + pt.measure(f.body, label).cx + pt.measure(f.body, " ").cx;
        if x < boxes.right {
            let suffix = pt.ellipsize(f.small, suffix, boxes.right - x);
            pt.text_on_baseline(
                f.small,
                pt.c.muted,
                &suffix,
                RECT { left: x, ..band },
                baseline,
                DT_LEFT,
            );
        }
    }
}

/// The table row background (hover / selection) and its 1 px row border;
/// returns (content box, background color).
fn row_frame(pt: &Painter, r: RECT, st: &RowPaint) -> (RECT, u32) {
    let content = content_of(pt.hair() as i32, r);
    let bg = widgets::row_background(&pt.c, st.hover, st.selected);
    pt.fill(content, bg);
    pt.fill(
        RECT {
            top: content.bottom,
            ..r
        },
        pt.c.row_border,
    );
    (content, bg)
}

/// The startup Enabled switch: 40 × 20, right padding 12, top 8 (the
/// reference's inline-block `vertical-align: middle` in a 33 px cell).
fn switch_box(dpi: i32, cell: RECT) -> RECT {
    let d = |v: f32| gfx::pxi(dpi, v);
    let right = cell.right - d(12.0);
    let top = cell.top + d(8.0);
    RECT {
        left: right - d(40.0),
        top,
        right,
        bottom: top + d(20.0),
    }
}

/// A read-only startup entry whose approval state could not be read.
fn state_unknown(entry: &crate::startup::StartupEntry) -> bool {
    !entry.manageable
        && (entry.status.starts_with("State unknown") || entry.status.starts_with("상태 확인 필요"))
}

/// The processes / startup / services pages of the main window.
struct AppRows(*mut App);

impl AppRows {
    unsafe fn index(&self, row: usize) -> Option<usize> {
        (&(*self.0).rows)
            .get(row)
            .copied()
            .filter(|&i| i != usize::MAX)
    }
    unsafe fn tree_row(&self, row: usize) -> Option<TreeRow> {
        let p = self.0;
        if (*p).page == Page::Processes && ((*p).tree_mode || (*p).group_mode) {
            (&(*p).tree_rows).get(row).copied()
        } else {
            None
        }
    }
    unsafe fn process_row(&self, pt: &Painter, row: usize, r: RECT, cells: &[RECT], st: &RowPaint) {
        let p = self.0;
        let Some(index) = self.index(row) else {
            return;
        };
        // A grouped app row shows its app's totals (`ui::shown_process`).
        let Some(process) = super::shown_process(p, index) else {
            return;
        };
        let process = &process;
        let (content, bg) = row_frame(pt, r, st);
        if cells.is_empty() {
            return;
        }
        let c = pt.c;
        let f = pt.fonts;
        let at = |col: usize| RECT {
            top: content.top,
            bottom: content.bottom,
            ..cells[col]
        };
        let (indent, chevron, child, suffix) = match self.tree_row(row) {
            Some(tree) => (
                tree_icon_x(tree.depth, tree.has_children),
                tree.has_children.then_some((st.chevron, st.chevron_hover)),
                tree.depth > 0,
                tree.has_children.then(|| format!("({})", tree.children)),
            ),
            None => (28.0, None, false, None),
        };
        name_cell(
            pt,
            at(0),
            indent,
            chevron,
            &widgets::initials(&process.name),
            child,
            &process.name,
            suffix.as_deref(),
        );
        // Design heat thresholds: CPU 20 %, memory 2400 MB, All I/O 3 MB/s,
        // network 8 Mbps, GPU 12 %, composited over the row background
        // (hover/selection stay visible). Unmeasured network / GPU cells read
        // "—" in the muted color, without heat.
        let optional = |value: Option<f64>, threshold: f64| {
            value.map_or(0.0, |v| widgets::heat_alpha(v, threshold))
        };
        use process_columns::ProcessColumn::*;
        for (col, id) in (*p).process_columns.shown().enumerate().skip(1) {
            if cells[col].right <= cells[col].left {
                continue;
            }
            let alpha = match id {
                Cpu => widgets::heat_alpha(process.cpu_percent, widgets::HEAT_CPU_PERCENT),
                Memory => {
                    widgets::heat_alpha(process.working_set as f64, widgets::HEAT_MEMORY_BYTES)
                }
                AllIo => {
                    widgets::heat_alpha(process.io_bytes_per_sec, widgets::HEAT_IO_BYTES_PER_SEC)
                }
                Network => optional(
                    process.network_bytes_per_sec,
                    widgets::HEAT_NETWORK_BYTES_PER_SEC,
                ),
                Gpu => optional(process.gpu_percent, widgets::HEAT_GPU_PERCENT),
                _ => 0.0,
            };
            if alpha > 0.0 {
                widgets::heat_cell_bg(pt, at(col), bg, alpha);
            }
            let text = super::cell(process, id as i32);
            let unmeasured = text == "—";
            cell_text(
                pt,
                if id.numeric() { f.mono_cell } else { f.body },
                if unmeasured { c.muted } else { c.fg },
                &text,
                at(col),
                if id.numeric() { DT_RIGHT } else { DT_LEFT },
            );
        }
    }
    unsafe fn startup_row(&self, pt: &Painter, row: usize, r: RECT, cells: &[RECT], st: &RowPaint) {
        let p = self.0;
        let Some(entry) = self.index(row).and_then(|i| (&(*p).startup).get(i)) else {
            return;
        };
        let (content, bg) = row_frame(pt, r, st);
        if cells.len() < 4 {
            return;
        }
        let c = pt.c;
        let f = pt.fonts;
        let at = |col: usize| RECT {
            top: content.top,
            bottom: content.bottom,
            ..cells[col]
        };
        name_cell(
            pt,
            at(0),
            0.0,
            None,
            &widgets::initials(&entry.name),
            false,
            &entry.name,
            None,
        );
        let publisher = (*p).startup_publishers.get(&entry.id);
        cell_text(
            pt,
            f.body,
            c.muted,
            publisher.map_or("—", String::as_str),
            at(1),
            DT_LEFT,
        );
        // No startup-impact measurement exists: say so (no invented pill).
        cell_text(
            pt,
            f.body,
            c.muted,
            tr("측정 안 됨", "Not measured"),
            at(2),
            DT_LEFT,
        );
        let cell = at(3);
        if state_unknown(entry) {
            cell_text(
                pt,
                f.small,
                c.muted,
                tr("알 수 없음", "Unknown"),
                cell,
                DT_RIGHT,
            );
        } else if entry.manageable {
            // Slides as soon as it is clicked (`App::startup_pending`); a
            // running action or a reload only ignores clicks, the switches
            // keep their look (only unmanageable entries are drawn at .45).
            let on = self.switch_on(row).unwrap_or(entry.enabled);
            widgets::switch_to(pt, switch_box(pt.dpi, cell), st.switch, on, true, false, bg);
        } else {
            widgets::switch(
                pt,
                switch_box(pt.dpi, cell),
                entry.enabled as u8 as f32,
                false,
                false,
                bg,
            );
        }
    }
    unsafe fn service_row(&self, pt: &Painter, row: usize, r: RECT, cells: &[RECT], st: &RowPaint) {
        let p = self.0;
        let Some(service) = self.index(row).and_then(|i| (&(*p).services).get(i)) else {
            return;
        };
        let (content, _) = row_frame(pt, r, st);
        // Startup type (column 4) is hidden while the details panel shows.
        if cells.len() < 4 {
            return;
        }
        let c = pt.c;
        let f = pt.fonts;
        let at = |col: usize| RECT {
            top: content.top,
            bottom: content.bottom,
            ..cells[col]
        };
        name_cell(
            pt,
            at(0),
            0.0,
            None,
            &widgets::initials(&service.name),
            false,
            &service.name,
            None,
        );
        if service.pid != 0 {
            cell_text(
                pt,
                f.mono_cell,
                c.fg,
                &service.pid.to_string(),
                at(1),
                DT_RIGHT,
            );
        }
        cell_text(pt, f.body, c.fg, &service.display_name, at(2), DT_LEFT);
        let status = at(3);
        let height = pt.pxi(20.0);
        let top = status.top + (status.bottom - status.top - height + 1) / 2;
        let kind = if service.state == SERVICE_RUNNING {
            PillKind::Ok
        } else {
            PillKind::Default
        };
        widgets::pill(
            pt,
            status.left + pt.pxi(12.0),
            top + height / 2,
            kind,
            crate::services::state_label(service.state),
            false,
        );
        if cells.len() > 4 {
            cell_text(
                pt,
                f.body,
                c.muted,
                crate::services::start_type_label(service.start_type),
                at(4),
                DT_LEFT,
            );
        }
    }
}

impl Model for AppRows {
    unsafe fn dpi(&self) -> i32 {
        (*self.0).dpi
    }
    unsafe fn fonts(&self) -> &Fonts {
        &(*self.0).fonts
    }
    unsafe fn header_height(&self) -> i32 {
        layout::table_header_height((*self.0).page, (*self.0).dpi)
    }
    unsafe fn row_height(&self, row: usize) -> i32 {
        if self.is_group(row) {
            gfx::pxi((*self.0).dpi, GROUP_ROW)
        } else {
            layout::table_row_height((*self.0).dpi)
        }
    }
    unsafe fn line(&self) -> i32 {
        layout::table_row_height((*self.0).dpi)
    }
    unsafe fn is_group(&self, row: usize) -> bool {
        let p = self.0;
        (*p).page == Page::Processes && (*p).group_headers.contains_key(&row)
    }
    unsafe fn key(&self, row: usize) -> u64 {
        let p = self.0;
        let Some(index) = self.index(row) else {
            return hash_of(("row", row));
        };
        match (*p).page {
            Page::Processes => (*p)
                .snapshot
                .as_ref()
                .and_then(|s| s.processes.get(index))
                .map_or(0, |s| hash_of((s.pid, s.created))),
            Page::Startup => (&(*p).startup)
                .get(index)
                .map_or(0, |s| hash_of(("startup", &s.id))),
            Page::Services => (&(*p).services)
                .get(index)
                .map_or(0, |s| hash_of(("service", &s.name))),
            Page::Performance | Page::Settings => 0,
        }
    }
    unsafe fn expanded(&self, row: usize) -> Option<bool> {
        self.tree_row(row)
            .filter(|tree| tree.has_children)
            .map(|tree| tree.expanded)
    }
    unsafe fn switch_on(&self, row: usize) -> Option<bool> {
        let p = self.0;
        if (*p).page != Page::Startup {
            return None;
        }
        self.index(row)
            .and_then(|i| (&(*p).startup).get(i))
            .filter(|entry| entry.manageable)
            .map(|entry| match &(*p).startup_pending {
                // Flipped by the user: the requested state until the
                // reloaded list confirms (or a failure clears) it.
                Some((id, on)) if *id == entry.id => *on,
                _ => entry.enabled,
            })
    }
    unsafe fn header(&self, col: usize) -> HeaderCell {
        let p = self.0;
        let processes = (*p).page == Page::Processes;
        let col = if processes {
            (*p).process_columns
                .at(col)
                .map_or(usize::MAX, |id| id as usize)
        } else {
            col
        };
        let total = if processes {
            (*p).snapshot.as_ref().and_then(|s| match col {
                2 => Some(format!("{:.1}%", s.cpu_percent)),
                3 => (s.memory_total > 0).then(|| {
                    format!(
                        "{:.1}%",
                        s.memory_used as f64 / s.memory_total as f64 * 100.0
                    )
                }),
                // The column total of the per-process I/O rates.
                4 => {
                    let rates = s
                        .processes
                        .iter()
                        .map(|p| p.io_bytes_per_sec)
                        .filter(|v| v.is_finite());
                    let (sum, any) = rates.fold((0.0, false), |(sum, _), v| (sum + v, true));
                    any.then(|| total_rate(sum))
                }
                // System-wide: physical adapters' throughput and the busiest
                // real GPU (the performance sample; per-process values may
                // be unavailable).
                5 => (*p)
                    .performance
                    .as_ref()
                    .filter(|perf| perf.network_rates_ready)
                    .map(|perf| {
                        super::mbps(perf.network_rx_bytes_per_sec + perf.network_tx_bytes_per_sec)
                    }),
                6 => (*p)
                    .performance
                    .as_ref()
                    .and_then(|perf| super::system_gpu_percent(perf))
                    .map(|v| format!("{v:.1}%")),
                _ => None,
            })
        } else {
            None
        };
        HeaderCell {
            total,
            sort: (col == (*p).sort && (processes || (*p).sort_chosen)).then_some((*p).descending),
            two_line: processes,
        }
    }
    unsafe fn editable_columns(&self) -> bool {
        (*self.0).page == Page::Processes
    }
    unsafe fn resize_column(&self, column: usize, width: f32) {
        process_columns::resize(self.0, column, width);
    }
    unsafe fn reorder_column(&self, from: usize, to: usize) {
        process_columns::reorder(self.0, from, to);
    }
    unsafe fn column_menu(&self, column: Option<usize>, point: POINT) {
        process_columns::menu(self.0, column, point);
    }
    unsafe fn parts(&self, row: usize, r: RECT, cells: &[RECT]) -> Parts {
        let p = self.0;
        let dpi = (*p).dpi;
        let content = content_of(gfx::hairline(dpi) as i32, r);
        let mut parts = Parts::default();
        match (*p).page {
            Page::Processes => {
                if let (Some(tree), Some(cell)) = (self.tree_row(row), cells.first()) {
                    if tree.has_children {
                        let cell = RECT {
                            top: content.top,
                            bottom: content.bottom,
                            ..*cell
                        };
                        let icon_x = tree_icon_x(tree.depth, tree.has_children);
                        parts.chevron = Some(name_boxes(dpi, cell, icon_x).chevron);
                    }
                }
            }
            Page::Startup => {
                if let (Some(_), Some(cell)) = (self.switch_on(row), cells.get(3)) {
                    let cell = RECT {
                        top: content.top,
                        bottom: content.bottom,
                        ..*cell
                    };
                    parts.switch = Some(switch_box(dpi, cell));
                }
            }
            _ => {}
        }
        parts
    }
    unsafe fn paint_row(&self, pt: &Painter, row: usize, r: RECT, cells: &[RECT], st: &RowPaint) {
        let p = self.0;
        if self.is_group(row) {
            // "Apps (8)": 38 px, no border, never hovered or selected.
            pt.fill(r, pt.c.surface);
            let title = (&(*p).group_headers).get(&row).map_or("", String::as_str);
            let (title, count) = widgets::split_group_title(title);
            widgets::group_row(pt, r, title, count);
            return;
        }
        match (*p).page {
            Page::Processes => self.process_row(pt, row, r, cells, st),
            Page::Startup => self.startup_row(pt, row, r, cells, st),
            Page::Services => self.service_row(pt, row, r, cells, st),
            Page::Performance | Page::Settings => {}
        }
    }
    unsafe fn text(&self, row: usize, col: usize) -> String {
        let p = self.0;
        if self.is_group(row) {
            return if col == 0 {
                (&(*p).group_headers).get(&row).cloned().unwrap_or_default()
            } else {
                String::new()
            };
        }
        self.index(row)
            .map(|index| cell_at(p, index, col as i32))
            .unwrap_or_default()
    }
    unsafe fn empty_text(&self) -> String {
        empty_message(self.0)
    }
}

/// The performance page's device cards (`paint::device_card`).
struct Devices(*mut App);

impl Model for Devices {
    unsafe fn dpi(&self) -> i32 {
        (*self.0).dpi
    }
    unsafe fn fonts(&self) -> &Fonts {
        &(*self.0).fonts
    }
    unsafe fn header_height(&self) -> i32 {
        0
    }
    unsafe fn row_height(&self, _row: usize) -> i32 {
        layout::device_item_height((*self.0).dpi)
    }
    unsafe fn line(&self) -> i32 {
        layout::device_item_height((*self.0).dpi)
    }
    unsafe fn padding(&self) -> (i32, i32) {
        let pad = gfx::pxi((*self.0).dpi, layout::DEVICES_PAD);
        (pad, pad)
    }
    unsafe fn paint_row(&self, pt: &Painter, row: usize, r: RECT, _cells: &[RECT], st: &RowPaint) {
        // The card paints with its own canvas: keep the drawing order.
        pt.canvas.flush();
        paint::device_card(self.0, pt.dc, r, row, st.hover, st.selected);
    }
    unsafe fn focus_box(&self, _row: usize, r: RECT) -> (RECT, f32) {
        // The 56 px card (radius 4) in the item's top.
        let dpi = (*self.0).dpi;
        let card = RECT {
            bottom: (r.top + gfx::pxi(dpi, layout::DEVICE_CARD)).min(r.bottom),
            ..r
        };
        (card, gfx::px(dpi, theme::RADIUS_SM))
    }
    unsafe fn text(&self, row: usize, _col: usize) -> String {
        let p = self.0;
        (&(*p).perf_targets)
            .get(row)
            .map(|target| interactions::component_name(p, target))
            .unwrap_or_default()
    }
}

/// Preview captures of the table's interactive states (light and dark):
/// a table scrolled to the middle with the idle and the hovered scrollbar, a
/// hovered and a selected row, tree mode with an expanded parent, startup
/// with a hovered row, and the device list with its scrollbar.
pub(super) unsafe fn save_previews(p: *mut App, dir: &std::path::Path) -> Result<(), String> {
    let list = (*p).list;
    let settle_page = |p: *mut App, page: Page| {
        switch_page(p, page);
        (*p).startup_loading = false;
        (*p).services_loading = false;
        rebuild(p, None);
        layout(p);
        update_buttons(p);
    };
    let first_data = |p: *mut App, skip: usize| {
        (0..(*p).rows.len())
            .filter(|i| !(*p).group_headers.contains_key(i))
            .nth(skip)
    };
    let select_row = |p: *mut App, row: Option<usize>| {
        let item = LVITEMW {
            stateMask: LVIS_SELECTED | LVIS_FOCUSED,
            state: if row.is_some() {
                LVIS_SELECTED | LVIS_FOCUSED
            } else {
                0
            },
            ..zeroed()
        };
        SendMessageW(
            (*p).list,
            LVM_SETITEMSTATE,
            row.unwrap_or(usize::MAX),
            &item as *const _ as isize,
        );
        update_buttons(p);
    };
    let performance = (*p).performance.clone();
    for (theme, name) in [(1, "light"), (2, "dark")] {
        (*p).prefs.theme = theme;
        interactions::apply_theme(p);
        settle_page(p, Page::Processes);
        let e = extent(list);
        scroll_to(list, e.max() / 2.0);
        stage_scrollbar(list, 1.0, 0.0);
        capture::save_client(p, &dir.join(format!("table-scrolled-{name}.bmp")))?;
        stage_scrollbar(list, 1.0, 1.0);
        capture::save_client(p, &dir.join(format!("table-scrollbar-hover-{name}.bmp")))?;
        stage_scrollbar(list, 0.0, 0.0);
        // The grouped view's last section ("Windows processes") at the top,
        // the background section's end above it.
        if let Some(&row) = (*p).group_headers.keys().max() {
            let s = state(list);
            if !s.is_null() {
                let top = (&(*s).tops).get(row).copied().unwrap_or(0);
                scroll_to(
                    list,
                    (top - 3 * layout::table_row_height((*p).dpi)).max(0) as f32,
                );
                capture::save_client(p, &dir.join(format!("table-groups-{name}.bmp")))?;
            }
        }
        scroll_to(list, 0.0);
        select_row(p, first_data(p, 1));
        stage_hover(list, first_data(p, 3));
        capture::save_client(p, &dir.join(format!("table-rows-{name}.bmp")))?;
        stage_hover(list, None);
        // Keyboard focus: the 2 px ring on the selected row.
        stage_focus(list, true);
        capture::save_client(p, &dir.join(format!("table-focus-{name}.bmp")))?;
        stage_focus(list, false);
        select_row(p, None);
        // Tree mode with the first parent that has children, expanded, at
        // the top; its chevron hovered.
        set_tree_mode(p, true);
        (*p).collapsed.clear();
        rebuild(p, None);
        let parent = (&(*p).tree_rows)
            .iter()
            .position(|row| row.has_children && row.depth == 0);
        let s = state(list);
        if let (Some(row), false) = (parent, s.is_null()) {
            scroll_to(list, (&(*s).tops).get(row).copied().unwrap_or(0) as f32);
            select_row(p, Some(row + 1));
            (*s).hover_chevron = Some(row);
            (*s).anim.set(Key::ChevronHover(row), 1.0);
        }
        capture::save_client(p, &dir.join(format!("table-tree-{name}.bmp")))?;
        if !s.is_null() {
            set_chevron_hover(s, None);
            (*s).anim.finish_all();
        }
        // A nested parent (depth 1 with children) under its top-level
        // parent: chevron one step right, its children's icons under its own.
        let rows = &(*p).tree_rows;
        let nested = rows
            .iter()
            .position(|row| row.has_children && row.depth == 1);
        if let (Some(row), false) = (nested, s.is_null()) {
            let top = (0..row).rev().find(|&i| rows[i].depth == 0).unwrap_or(row);
            scroll_to(list, (&(*s).tops).get(top).copied().unwrap_or(0) as f32);
            select_row(p, Some(row));
            capture::save_client(p, &dir.join(format!("table-tree-nested-{name}.bmp")))?;
        }
        select_row(p, None);
        set_tree_mode(p, false);
        scroll_to(list, 0.0);
        settle_page(p, Page::Startup);
        stage_hover(list, first_data(p, 1));
        capture::save_client(p, &dir.join(format!("table-startup-{name}.bmp")))?;
        stage_hover(list, None);
        settle_page(p, Page::Services);
        select_row(p, first_data(p, 2));
        stage_hover(list, first_data(p, 4));
        capture::save_client(p, &dir.join(format!("table-services-{name}.bmp")))?;
        stage_hover(list, None);
        select_row(p, None);
        // Empty state: a search without matches.
        SetWindowTextW((*p).search, wide("no-such-service-0000").as_ptr());
        capture::save_client(p, &dir.join(format!("table-empty-{name}.bmp")))?;
        SetWindowTextW((*p).search, wide("").as_ptr());
        switch_page(p, Page::Performance);
        (*p).performance = performance.clone();
        interactions::refresh_components(p);
        layout(p);
        update_buttons(p);
        stage_hover((*p).perf_list, Some(2));
        stage_scrollbar((*p).perf_list, 1.0, 0.0);
        capture::save_client(p, &dir.join(format!("devices-{name}.bmp")))?;
        stage_focus((*p).perf_list, true);
        capture::save_client(p, &dir.join(format!("devices-focus-{name}.bmp")))?;
        stage_focus((*p).perf_list, false);
        // Scroll first: scrolling schedules the idle fade-out, staging drops it.
        let e = extent((*p).perf_list);
        scroll_to((*p).perf_list, e.max());
        stage_scrollbar((*p).perf_list, 1.0, 1.0);
        capture::save_client(p, &dir.join(format!("devices-scrolled-{name}.bmp")))?;
        stage_scrollbar((*p).perf_list, 0.0, 0.0);
        stage_hover((*p).perf_list, None);
        scroll_to((*p).perf_list, 0.0);
        (*p).group_mode = true;
    }
    (*p).prefs.theme = 1;
    interactions::apply_theme(p);
    settle_page(p, Page::Processes);
    Ok(())
}

/// A processes table scrolled to its middle with the idle scrollbar (the
/// minimum / 150 % previews).
pub(super) unsafe fn save_scrolled(p: *mut App, path: &std::path::Path) -> Result<(), String> {
    let list = (*p).list;
    let e = extent(list);
    scroll_to(list, e.max() / 2.0);
    stage_scrollbar(list, 1.0, 0.0);
    let result = capture::save_client(p, path);
    stage_scrollbar(list, 0.0, 0.0);
    scroll_to(list, 0.0);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::ffi::c_void;
    use std::rc::Rc;

    thread_local! {
        static EVENTS: RefCell<Vec<(u32, i32)>> = const { RefCell::new(Vec::new()) };
    }

    unsafe extern "system" fn parent_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_NOTIFY => {
                let hdr = &*(l as *const NMHDR);
                let item = if matches!(hdr.code, NM_CLICK | NM_DBLCLK) {
                    (*(l as *const NMITEMACTIVATE)).iItem
                } else if hdr.code == NM_RETURN {
                    -1
                } else {
                    (*(l as *const NMLISTVIEW)).iItem
                };
                EVENTS.with(|e| e.borrow_mut().push((hdr.code, item)));
                0
            }
            WM_COMMAND => {
                EVENTS.with(|e| e.borrow_mut().push(((w >> 16) as u32 | 0x8000_0000, 0)));
                0
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    /// The column menus a test table asked for: (column, screen x, y).
    type Menus = Rc<RefCell<Vec<(Option<usize>, i32, i32)>>>;

    fn events() -> Vec<(u32, i32)> {
        EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut()))
    }

    /// 96 DPI rows: groups at 0 and 10 (38 px), data rows 34 px, row 1 is
    /// a collapsed tree parent with a chevron, row 3 has a switch.
    struct Fake {
        fonts: Fonts,
        groups: Vec<usize>,
        /// Row 1's expanded state and row 3's switch, shared with the test.
        open: Rc<Cell<bool>>,
        on: Rc<Cell<bool>>,
        /// Editable columns: the column menus asked for.
        menus: Option<Menus>,
    }

    impl Model for Fake {
        unsafe fn editable_columns(&self) -> bool {
            self.menus.is_some()
        }
        unsafe fn column_menu(&self, column: Option<usize>, point: POINT) {
            if let Some(menus) = &self.menus {
                menus.borrow_mut().push((column, point.x, point.y));
            }
        }
        unsafe fn dpi(&self) -> i32 {
            96
        }
        unsafe fn fonts(&self) -> &Fonts {
            &self.fonts
        }
        unsafe fn header_height(&self) -> i32 {
            34
        }
        unsafe fn row_height(&self, row: usize) -> i32 {
            if self.is_group(row) {
                38
            } else {
                34
            }
        }
        unsafe fn line(&self) -> i32 {
            34
        }
        unsafe fn is_group(&self, row: usize) -> bool {
            self.groups.contains(&row)
        }
        unsafe fn expanded(&self, row: usize) -> Option<bool> {
            (row == 1).then(|| self.open.get())
        }
        unsafe fn switch_on(&self, row: usize) -> Option<bool> {
            (row == 3).then(|| self.on.get())
        }
        unsafe fn parts(&self, row: usize, r: RECT, cells: &[RECT]) -> Parts {
            let content = content_of(1, r);
            let cell = |i: usize| RECT {
                top: content.top,
                bottom: content.bottom,
                ..cells[i]
            };
            Parts {
                chevron: (row == 1).then(|| name_boxes(96, cell(0), 28.0).chevron),
                switch: (row == 3).then(|| switch_box(96, cell(1))),
            }
        }
        unsafe fn paint_row(
            &self,
            pt: &Painter,
            row: usize,
            r: RECT,
            cells: &[RECT],
            st: &RowPaint,
        ) {
            let (content, bg) = row_frame(pt, r, st);
            let cell = RECT {
                top: content.top,
                bottom: content.bottom,
                ..cells[0]
            };
            name_cell(pt, cell, 28.0, None, "R", false, &self.text(row, 0), None);
            // Numeric cells with heat, like the processes table.
            for (col, c) in cells.iter().enumerate().skip(1) {
                let at = RECT {
                    top: content.top,
                    bottom: content.bottom,
                    ..*c
                };
                widgets::heat_cell_bg(pt, at, bg, 0.1 * col as f32);
                cell_text(pt, pt.fonts.mono_cell, pt.c.fg, "1,234.5 MB", at, DT_RIGHT);
            }
        }
        unsafe fn text(&self, row: usize, col: usize) -> String {
            match (col, row) {
                (0, 5) => "Beta".into(),
                (0, 6) => "Bravo".into(),
                (0, 7) => "Charlie".into(),
                _ => format!("Row {row}"),
            }
        }
    }

    struct Harness {
        parent: HWND,
        table: HWND,
        open: Rc<Cell<bool>>,
        on: Rc<Cell<bool>>,
        menus: Menus,
    }

    impl Harness {
        fn new(mode: Mode, rows: usize) -> Self {
            Self::build(mode, rows, false)
        }
        fn build(mode: Mode, rows: usize, editable: bool) -> Self {
            unsafe {
                gfx::startup();
                static PARENT: OnceLock<Vec<u16>> = OnceLock::new();
                let class = PARENT.get_or_init(|| {
                    let name = wide("FeatherTableTestParent");
                    RegisterClassExW(&WNDCLASSEXW {
                        cbSize: size_of::<WNDCLASSEXW>() as u32,
                        lpfnWndProc: Some(parent_proc),
                        hInstance: GetModuleHandleW(null()),
                        lpszClassName: name.as_ptr(),
                        ..zeroed()
                    });
                    name
                });
                let parent = CreateWindowExW(
                    0,
                    class.as_ptr(),
                    wide("parent").as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    420,
                    300,
                    null_mut(),
                    null_mut(),
                    GetModuleHandleW(null()),
                    null(),
                );
                assert!(!parent.is_null());
                let open = Rc::new(Cell::new(false));
                let on = Rc::new(Cell::new(true));
                let menus = Rc::new(RefCell::new(Vec::new()));
                let model = Box::new(Fake {
                    fonts: Fonts::new(96, Language::English),
                    groups: vec![0, 10],
                    open: open.clone(),
                    on: on.clone(),
                    menus: editable.then(|| menus.clone()),
                });
                let table = create(parent, 7, "fake", mode, model);
                assert!(!table.is_null());
                MoveWindow(table, 0, 0, 400, 240, 0);
                if mode == Mode::Table {
                    SendMessageW(table, LVM_SETITEMCOUNT, rows, 0);
                } else {
                    for i in 0..rows {
                        SendMessageW(
                            table,
                            LB_ADDSTRING,
                            0,
                            wide(&format!("Item {i}")).as_ptr() as isize,
                        );
                    }
                }
                set_columns(
                    table,
                    vec![
                        Column {
                            label: "Name".into(),
                            width: 0.0,
                            flex: true,
                            right: false,
                        },
                        Column {
                            label: "Value".into(),
                            width: 100.0,
                            flex: false,
                            right: true,
                        },
                    ],
                );
                let s = state(table);
                (*s).anim.anim = anim::Animator::new().with_reduced_motion(false);
                events();
                Self {
                    parent,
                    table,
                    open,
                    on,
                    menus,
                }
            }
        }
        fn selected(&self) -> isize {
            unsafe {
                SendMessageW(
                    self.table,
                    LVM_GETNEXTITEM,
                    usize::MAX,
                    LVNI_SELECTED as isize,
                )
            }
        }
        fn key(&self, key: u16) {
            unsafe {
                SendMessageW(self.table, WM_KEYDOWN, key as usize, 0);
            }
        }
        fn at(x: i32, y: i32) -> LPARAM {
            ((y as u16 as u32) << 16 | x as u16 as u32) as isize
        }
        /// Show the parent without it appearing on screen: a fully
        /// transparent, non-activating tool window still gets WM_PAINT
        /// (an off-screen one has no update region) and events.
        fn show_invisibly(&self) {
            unsafe {
                SetWindowLongPtrW(
                    self.parent,
                    GWL_EXSTYLE,
                    (WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT)
                        as isize,
                );
                SetLayeredWindowAttributes(self.parent, 0, 0, LWA_ALPHA);
                ShowWindow(self.parent, SW_SHOWNOACTIVATE);
            }
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.parent);
            }
        }
    }

    #[test]
    fn keyboard_steps_skip_group_rows_and_stop_at_the_ends() {
        let groups = [0usize, 3, 4, 9];
        let g = |i: usize| groups.contains(&i);
        assert_eq!(step(10, g, None, Nav::Down, 3), Some(1));
        assert_eq!(step(10, g, None, Nav::End, 3), Some(8));
        assert_eq!(step(10, g, Some(2), Nav::Down, 3), Some(5), "skips 3 and 4");
        assert_eq!(step(10, g, Some(5), Nav::Up, 3), Some(2));
        assert_eq!(step(10, g, Some(1), Nav::Up, 3), Some(1), "top stays");
        assert_eq!(step(10, g, Some(8), Nav::Down, 3), Some(8), "bottom stays");
        assert_eq!(step(10, g, Some(1), Nav::PageDown, 3), Some(5));
        assert_eq!(step(10, g, Some(8), Nav::PageUp, 3), Some(5));
        assert_eq!(step(10, g, Some(2), Nav::PageUp, 30), Some(1));
        assert_eq!(step(10, g, Some(2), Nav::PageDown, 30), Some(8));
        assert_eq!(step(10, g, Some(6), Nav::Home, 3), Some(1));
        assert_eq!(step(10, g, Some(6), Nav::End, 3), Some(8));
        assert_eq!(step(3, |_| true, None, Nav::Down, 3), None, "only groups");
        assert_eq!(step(0, g, None, Nav::Down, 3), None);
    }

    #[test]
    fn column_menus_target_the_pointed_header_or_every_column_from_the_keyboard() {
        let h = Harness::build(Mode::Table, 3, true);
        unsafe {
            let screen = |x: i32, y: i32| {
                let mut at = POINT { x, y };
                ClientToScreen(h.table, &mut at);
                (at.x, at.y)
            };
            let keyboard = 0xFFFF_FFFF_isize;
            let value = column_spans(state(h.table))[1].0;
            // Shift+F10 over the rows keeps the row menu (the parent's).
            SendMessageW(h.table, WM_CONTEXTMENU, h.table as usize, keyboard);
            assert!(h.menus.borrow().is_empty());
            // Right-click on a header cell: that column, at the pointer.
            let (x, y) = screen(20, 10);
            SendMessageW(h.table, WM_CONTEXTMENU, h.table as usize, Harness::at(x, y));
            assert_eq!(h.menus.borrow().last(), Some(&(Some(0), x, y)));
            // Shift+F10 while pointing at the Value header: below its cell.
            SendMessageW(h.table, WM_MOUSEMOVE, 0, Harness::at(value + 20, 10));
            SendMessageW(h.table, WM_CONTEXTMENU, h.table as usize, keyboard);
            let (x, y) = screen(value, 34);
            assert_eq!(h.menus.borrow().last(), Some(&(Some(1), x, y)));
            // Ctrl+Shift+C: no header is pointed at, every column is offered.
            let mut keys = [0u8; 256];
            GetKeyboardState(keys.as_mut_ptr());
            let saved = keys;
            keys[VK_CONTROL as usize] = 0x80;
            keys[VK_SHIFT as usize] = 0x80;
            SetKeyboardState(keys.as_ptr());
            h.key(b'C' as u16);
            SetKeyboardState(saved.as_ptr());
            assert_eq!(h.menus.borrow().last().map(|m| m.0), Some(None));
            assert_eq!(h.menus.borrow().len(), 3);
        }
    }

    #[test]
    fn columns_flex_to_the_width_and_scale_with_dpi() {
        let cols = |flex: bool, width: f32| Column {
            label: String::new(),
            width,
            flex,
            right: false,
        };
        let c = [cols(true, 0.0), cols(false, 112.0), cols(false, 92.0)];
        assert_eq!(spans(&c, 979, 96), vec![(0, 775), (775, 887), (887, 979)]);
        assert_eq!(spans(&c, 1468, 144)[1], (1162, 1330));
        // Too narrow: the flexible column keeps its minimum and the fixed
        // columns shrink proportionally, so every column stays visible.
        let narrow = spans(&c, 300, 96);
        assert_eq!(narrow[0], (0, 120));
        assert!(narrow[2].1 <= 300 && narrow[2].1 >= 298, "{narrow:?}");
        assert!(narrow[1].1 - narrow[1].0 > narrow[2].1 - narrow[2].0);
        assert_eq!(spans(&[], 300, 96), vec![(0, 300)]);
        assert_eq!(row_at(&[0, 38, 72, 106], 37), Some(0));
        assert_eq!(row_at(&[0, 38, 72, 106], 38), Some(1));
        assert_eq!(row_at(&[0, 38, 72, 106], 106), None);
    }

    #[test]
    fn hit_testing_finds_the_header_rows_parts_and_the_scrollbar() {
        let h = Harness::new(Mode::Table, 40);
        unsafe {
            let t = h.table;
            assert_eq!(header_height(t), 34);
            assert_eq!(column_count(t), 2);
            // Rows: group 0 (34..72), row 1 (72..106), row 2 (106..140) …
            let s = state(t);
            assert_eq!(hit(s, POINT { x: 20, y: 10 }), Hit::Header(0));
            assert_eq!(hit(s, POINT { x: 350, y: 10 }), Hit::Header(1));
            assert_eq!(hit(s, POINT { x: 20, y: 40 }), Hit::Group(0));
            assert_eq!(hit(s, POINT { x: 20, y: 80 }), Hit::Row(1));
            assert_eq!(hit(s, POINT { x: 20, y: 120 }), Hit::Row(2));
            let mut r = RECT {
                left: LVIR_BOUNDS as i32,
                ..zeroed()
            };
            assert_ne!(
                SendMessageW(t, LVM_GETITEMRECT, 1, &mut r as *mut _ as isize),
                0
            );
            assert_eq!((r.top, r.bottom, r.right), (72, 106, 400));
            let mut cell = RECT {
                top: 1,
                left: LVIR_BOUNDS as i32,
                ..zeroed()
            };
            assert_ne!(
                SendMessageW(t, LVM_GETSUBITEMRECT, 3, &mut cell as *mut _ as isize),
                0
            );
            assert_eq!((cell.left, cell.right), (300, 400));
            // Chevron 20×20 at the name cell's padding, switch right-aligned.
            let chevron = part_rect(t, 1, Part::Chevron).unwrap();
            assert_eq!((chevron.left, chevron.right), (12, 32));
            assert_eq!((chevron.top, chevron.bottom), (79, 99));
            let center = |r: RECT| POINT {
                x: (r.left + r.right) / 2,
                y: (r.top + r.bottom) / 2,
            };
            assert_eq!(part_at(t, center(chevron)), Some((1, Part::Chevron)));
            assert_eq!(part_at(t, POINT { x: 100, y: 80 }), Some((1, Part::Row)));
            let switch = part_rect(t, 3, Part::Switch).unwrap();
            assert_eq!(
                (switch.left, switch.right, switch.bottom - switch.top),
                (348, 388, 20)
            );
            assert_eq!(part_at(t, center(switch)), Some((3, Part::Switch)));
            assert_eq!(part_at(t, POINT { x: 20, y: 40 }), None, "group rows");
            assert_eq!(part_at(t, POINT { x: 20, y: 10 }), None, "header");
            // A click in the scrollbar lane pages instead of selecting.
            SendMessageW(t, WM_LBUTTONDOWN, 0, Harness::at(395, 200));
            SendMessageW(t, WM_LBUTTONUP, 0, Harness::at(395, 200));
            assert_eq!(h.selected(), -1);
            settle(t);
            assert!(scroll_offset(t) > 0.0, "the track click paged down");
            // A click on a row selects it (LVN_ITEMCHANGED) then NM_CLICK.
            scroll_to(t, 0.0);
            events();
            SendMessageW(t, WM_LBUTTONDOWN, 0, Harness::at(100, 120));
            SendMessageW(t, WM_LBUTTONUP, 0, Harness::at(100, 120));
            assert_eq!(h.selected(), 2);
            assert_eq!(events(), vec![(LVN_ITEMCHANGED, 2), (NM_CLICK, 2)]);
            // Group rows cannot be clicked into the selection.
            SendMessageW(t, WM_LBUTTONDOWN, 0, Harness::at(100, 40));
            SendMessageW(t, WM_LBUTTONUP, 0, Harness::at(100, 40));
            assert_eq!(h.selected(), 2);
            // Header click: LVN_COLUMNCLICK for the column under the pointer.
            events();
            SendMessageW(t, WM_LBUTTONDOWN, 0, Harness::at(350, 10));
            SendMessageW(t, WM_LBUTTONUP, 0, Harness::at(350, 10));
            assert_eq!(events(), vec![(LVN_COLUMNCLICK, -1)]);
            // Double click: NM_DBLCLK on the row.
            SendMessageW(t, WM_LBUTTONDBLCLK, 0, Harness::at(100, 80));
            assert!(events().contains(&(NM_DBLCLK, 1)));
        }
    }

    #[test]
    fn keyboard_moves_the_selection_below_the_sticky_header() {
        let h = Harness::new(Mode::Table, 40);
        unsafe {
            let t = h.table;
            h.key(VK_DOWN);
            assert_eq!(h.selected(), 1, "first data row, not the group");
            assert_eq!(events(), vec![(LVN_ITEMCHANGED, 1)]);
            for _ in 0..8 {
                h.key(VK_DOWN);
            }
            assert_eq!(h.selected(), 9);
            h.key(VK_DOWN);
            assert_eq!(h.selected(), 11, "skips the group row at 10");
            h.key(VK_UP);
            assert_eq!(h.selected(), 9);
            h.key(VK_END);
            assert_eq!(h.selected(), 39);
            settle(t);
            let row = part_rect(t, 39, Part::Row).unwrap();
            assert!(row.top >= header_height(t), "{} under the header", row.top);
            assert_eq!(row.bottom, 240, "the last row ends at the bottom");
            h.key(VK_HOME);
            settle(t);
            assert_eq!(h.selected(), 1);
            let row = part_rect(t, 1, Part::Row).unwrap();
            assert_eq!(row.top, 72, "the group row above it shows too");
            assert_eq!(scroll_offset(t), 0.0);
            h.key(VK_NEXT);
            assert!(h.selected() > 1);
            // Enter: NM_RETURN; typeahead: the next row starting with the text.
            events();
            h.key(VK_RETURN);
            assert_eq!(events(), vec![(NM_RETURN, -1)]);
            SendMessageW(t, WM_CHAR, 'b' as usize, 0);
            assert_eq!(h.selected(), 5);
            SendMessageW(t, WM_CHAR, 'r' as usize, 0);
            assert_eq!(h.selected(), 6, "\"br\" → Bravo");
            // Programmatic: selecting a group row clears the selection;
            // ensure-visible reveals rows under the sticky header.
            let item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: LVIS_SELECTED | LVIS_FOCUSED,
                ..zeroed()
            };
            SendMessageW(t, LVM_SETITEMSTATE, 10, &item as *const _ as isize);
            assert_eq!(h.selected(), -1);
            SendMessageW(t, LVM_SETITEMSTATE, 30, &item as *const _ as isize);
            assert_eq!(h.selected(), 30);
            SendMessageW(t, LVM_ENSUREVISIBLE, 30, 0);
            settle(t);
            let row = part_rect(t, 30, Part::Row).unwrap();
            assert!(
                row.top >= 34 && row.bottom <= 240,
                "{}..{}",
                row.top,
                row.bottom
            );
            // Shrinking the list clamps the scroll position and drops a
            // selection past the end.
            SendMessageW(t, LVM_SETITEMCOUNT, 12, 0);
            assert_eq!(h.selected(), -1);
            assert!(scroll_offset(t) <= extent(t).max());
        }
    }

    #[test]
    fn keyboard_context_menu_opens_at_the_focus_row_never_at_the_pointer() {
        let h = Harness::new(Mode::Table, 40);
        unsafe {
            let t = h.table;
            let keyboard = -1isize;
            let screen = |mut r: RECT| {
                MapWindowPoints(t, null_mut(), (&mut r as *mut RECT).cast::<POINT>(), 2);
                r
            };
            assert!(
                keyboard_menu_anchor(t, Harness::at(30, 60)).is_none(),
                "mouse"
            );
            // No selection: the ring row (the first data row, under the group).
            assert_eq!(h.selected(), -1);
            let ring = screen(part_rect(t, 1, Part::Row).unwrap());
            let anchor = keyboard_menu_anchor(t, keyboard).unwrap();
            assert_eq!((anchor.top, anchor.bottom), (ring.top, ring.bottom));
            assert_eq!(anchor.left, ring.left, "left-aligned in the first column");
            // Scrolled away from it: the top of the rows, below the header,
            // and the view does not jump back.
            for _ in 0..20 {
                SendMessageW(t, WM_MOUSEWHEEL, (-120i32 as u16 as usize) << 16, 0);
            }
            settle(t);
            let offset = scroll_offset(t);
            assert!(offset > 0.0);
            let anchor = keyboard_menu_anchor(t, keyboard).unwrap();
            let top = screen(RECT {
                left: 0,
                top: header_height(t),
                right: 0,
                bottom: header_height(t),
            });
            assert_eq!((anchor.top, anchor.bottom), (top.top, top.top));
            assert_eq!(scroll_offset(t), offset);
            // A selection: that row, revealed first.
            h.key(VK_HOME);
            settle(t);
            let row = screen(part_rect(t, 1, Part::Row).unwrap());
            let anchor = keyboard_menu_anchor(t, keyboard).unwrap();
            assert_eq!((anchor.top, anchor.bottom), (row.top, row.bottom));
        }
    }

    #[test]
    fn wheel_scrolls_smoothly_and_the_bar_fades_without_idle_timers() {
        let h = Harness::new(Mode::Table, 40);
        unsafe {
            let t = h.table;
            let wheel = |delta: i32| {
                SendMessageW(t, WM_MOUSEWHEEL, (delta as u16 as usize) << 16, 0);
            };
            wheel(-120);
            assert!(is_animating(t), "the wheel glide runs on the frame timer");
            wheel(-120);
            let s = state(t);
            let lines = scroll::wheel_lines();
            if lines != scroll::WHEEL_PAGE {
                let expected = (2 * lines * 34) as f32;
                assert_eq!(
                    (*s).scroll.target(&(*s).anim),
                    expected.min(extent(t).max())
                );
            }
            settle(t);
            assert!(scroll_offset(t) > 0.0);
            assert!(!is_animating(t), "idle after the fade-out");
            for _ in 0..200 {
                wheel(-120);
            }
            settle(t);
            assert_eq!(scroll_offset(t), extent(t).max());
            // Nothing to scroll: the wheel goes to the parent, the bar hides.
            SendMessageW(t, LVM_SETITEMCOUNT, 2, 0);
            assert_eq!(scroll_offset(t), 0.0);
            wheel(-120);
            assert!(!is_animating(t));
        }
    }

    #[test]
    fn chevron_rotates_and_switch_slides_when_their_state_changes() {
        let h = Harness::new(Mode::Table, 20);
        unsafe {
            let t = h.table;
            let s = state(t);
            let dib = gfx::Dib::new(400, 240).unwrap();
            let paint = || SendMessageW(t, WM_PRINTCLIENT, dib.dc() as usize, 0);
            paint();
            let chevron = Key::Chevron((*s).model.key(1));
            let switch = Key::Switch((*s).model.key(3));
            // First sight jumps: nothing animates on the first paint.
            assert_eq!((*s).anim.value(chevron), 0.0);
            assert_eq!((*s).anim.value(switch), 1.0);
            assert!(!(*s).anim.anim.is_animating());
            // Expanding the branch rotates the chevron 0 → 90° over 120 ms
            // (`ease`); the confirmed switch state slides the knob.
            h.open.set(true);
            h.on.set(false);
            paint();
            assert!((*s).anim.anim.is_key_animating(chevron));
            assert!((*s).anim.anim.is_key_animating(switch));
            assert!(is_animating(t), "the frame timer runs");
            let t0 = Instant::now();
            (*s).anim.tick_at(t0 + Duration::from_millis(40));
            let mid = (*s).anim.value(chevron);
            assert!(mid > 0.0 && mid < 90.0, "{mid}");
            let knob = (*s).anim.value(switch);
            assert!(knob > 0.0 && knob < 1.0, "{knob}");
            (*s).anim.tick_at(t0 + Duration::from_millis(500));
            assert_eq!((*s).anim.value(chevron), 90.0);
            assert_eq!((*s).anim.value(switch), 0.0);
            // Keys of rows scrolled out of view are dropped; they jump on
            // their next sight.
            scroll_to(t, extent(t).max());
            paint();
            assert!((*s).anim.anim.target(chevron).is_none());
        }
    }

    #[test]
    fn list_mode_speaks_list_box_messages() {
        let h = Harness::new(Mode::List, 6);
        unsafe {
            let t = h.table;
            assert_eq!(SendMessageW(t, LB_GETCOUNT, 0, 0), 6);
            assert_eq!(SendMessageW(t, LB_GETCURSEL, 0, 0), LB_ERR as isize);
            assert_eq!(SendMessageW(t, LB_SETCURSEL, 2, 0), 2);
            assert_eq!(SendMessageW(t, LB_GETCURSEL, 0, 0), 2);
            assert!(events().is_empty(), "programmatic changes do not notify");
            h.key(VK_DOWN);
            assert_eq!(SendMessageW(t, LB_GETCURSEL, 0, 0), 3);
            assert_eq!(events(), vec![(LBN_SELCHANGE | 0x8000_0000, 0)]);
            let mut text = [0u16; 32];
            let len = SendMessageW(t, LB_GETTEXT, 3, text.as_mut_ptr() as isize);
            assert_eq!(String::from_utf16_lossy(&text[..len as usize]), "Item 3");
            SendMessageW(t, LB_RESETCONTENT, 0, 0);
            assert_eq!(SendMessageW(t, LB_GETCOUNT, 0, 0), 0);
            assert_eq!(SendMessageW(t, LB_GETCURSEL, 0, 0), LB_ERR as isize);
            // WM_SETREDRAW must not make a hidden list visible.
            ShowWindow(t, SW_HIDE);
            SendMessageW(t, WM_SETREDRAW, 0, 0);
            SendMessageW(t, WM_SETREDRAW, 1, 0);
            assert_eq!(GetWindowLongW(t, GWL_STYLE) as u32 & WS_VISIBLE, 0);
        }
    }

    #[test]
    fn hover_and_chevron_animate_and_settle_to_idle() {
        let h = Harness::new(Mode::Table, 20);
        unsafe {
            let t = h.table;
            let s = state(t);
            SendMessageW(t, WM_MOUSEMOVE, 0, Harness::at(100, 80));
            assert_eq!((*s).hover, Some(1));
            // The hover background cross-fades in (100 ms ease-out).
            let t0 = Instant::now();
            (*s).anim.tick_at(t0 + Duration::from_millis(30));
            let fade = (*s).anim.value(Key::Row(1));
            assert!(fade > 0.0 && fade < 1.0, "{fade}");
            SendMessageW(t, WM_MOUSEMOVE, 0, Harness::at(22, 89));
            assert_eq!((*s).hover_chevron, Some(1), "chevron hover");
            SendMessageW(t, WM_MOUSEMOVE, 0, Harness::at(100, 40));
            assert_eq!((*s).hover, None, "group rows never hover");
            settle(t);
            // A paint keeps keys bounded: settled hover keys at 0 are pruned.
            let dib = gfx::Dib::new(400, 240).unwrap();
            SendMessageW(t, WM_PRINTCLIENT, dib.dc() as usize, 0);
            assert!((*s).anim.anim.len() <= 6, "{}", (*s).anim.anim.len());
            SendMessageW(t, WM_MOUSELEAVE, 0, 0);
            settle(t);
            assert!(!is_animating(t));
        }
    }

    #[test]
    fn tree_rows_of_one_depth_share_their_icon_column() {
        // The reference: top level [chevron | pad] 8 [icon] → icon at +28;
        // children padded 28 with no pad → their icon under the parent's.
        assert_eq!(tree_icon_x(0, true), 28.0);
        assert_eq!(tree_icon_x(0, false), 28.0);
        assert_eq!(tree_icon_x(1, false), 28.0);
        // Every row of a depth has its icon in one column, parent or leaf
        // (no staircase); deeper levels add 20 px (DESIGN_SPEC §4) and a
        // nested parent hangs its chevron in the 28 px before its icon.
        assert_eq!(tree_icon_x(1, true), 28.0);
        assert_eq!(tree_icon_x(2, false), 48.0);
        assert_eq!(tree_icon_x(2, true), 48.0);
        assert_eq!(tree_icon_x(3, false), 68.0);
        assert_eq!(tree_icon_x(40, false), tree_icon_x(TREE_DEPTH_CAP, false));
        let cell = RECT {
            left: 0,
            top: 0,
            right: 400,
            bottom: 33,
        };
        let parent = name_boxes(96, cell, tree_icon_x(0, true));
        let nested = name_boxes(96, cell, tree_icon_x(2, true));
        let leaf = name_boxes(96, cell, tree_icon_x(2, false));
        assert_eq!((parent.chevron.left, parent.icon.left), (12, 40));
        assert_eq!(nested.icon.left, leaf.icon.left, "siblings line up");
        assert_eq!((nested.chevron.left, nested.icon.left), (32, 60));
        assert_eq!(name_boxes(144, cell, 48.0).icon.left, 18 + 72);
        // A deep level in a narrow column keeps the icon inside the cell.
        let narrow = RECT { right: 120, ..cell };
        let deep = name_boxes(96, narrow, tree_icon_x(6, false));
        assert_eq!(deep.icon.right, 120 - 12);
    }

    #[test]
    fn the_switch_wins_over_the_scrollbar_lane_it_overlaps() {
        let h = Harness::new(Mode::Table, 40);
        unsafe {
            let t = h.table;
            // Row 3's switch: 348..388 (right padding 12); lane 386..400.
            let switch = part_rect(t, 3, Part::Switch).unwrap();
            let y = (switch.top + switch.bottom) / 2;
            let x = switch.right - 1;
            assert!(x >= 400 - 14, "inside the lane");
            assert_eq!(part_at(t, POINT { x, y }), Some((3, Part::Switch)));
            SendMessageW(t, WM_MOUSEMOVE, 0, Harness::at(x, y));
            let s = state(t);
            assert_eq!((*s).hover, Some(3), "the row hovers, not the lane");
            SendMessageW(t, WM_LBUTTONDOWN, 0, Harness::at(x, y));
            SendMessageW(t, WM_LBUTTONUP, 0, Harness::at(x, y));
            settle(t);
            assert_eq!(scroll_offset(t), 0.0, "no page");
            assert_eq!(events(), vec![(LVN_ITEMCHANGED, 3), (NM_CLICK, 3)]);
            // Beside the switch the lane still pages.
            SendMessageW(t, WM_LBUTTONDOWN, 0, Harness::at(398, y));
            SendMessageW(t, WM_LBUTTONUP, 0, Harness::at(398, y));
            settle(t);
            assert!(scroll_offset(t) > 0.0);
        }
    }

    #[test]
    fn header_totals_overflow_into_the_neighbours_padding() {
        const MB: f64 = 1048576.0;
        assert_eq!(total_rate(16.2 * MB), "16.2 MB/s");
        assert_eq!(total_rate(99.9 * MB), "99.9 MB/s");
        assert_eq!(total_rate(135.6 * MB), "136 MB/s");
        assert_eq!(total_rate(999.4 * MB), "999 MB/s");
        assert_eq!(total_rate(999.9 * MB), "1.0 GB/s");
        assert_eq!(total_rate(1100.0 * MB), "1.1 GB/s");
        assert_eq!(total_rate(f64::NAN), "—/s");
        unsafe {
            gfx::startup();
            let fonts = Fonts::new(96, Language::English);
            let dib = gfx::Dib::new(400, 60).unwrap();
            let pt = Painter::new(dib.dc(), 96, &fonts);
            let column = Column {
                label: "All I/O".into(),
                width: 100.0,
                flex: false,
                right: true,
            };
            let cell = RECT {
                left: 200,
                top: 0,
                right: 300,
                bottom: 52,
            };
            let info = |total: &str| HeaderCell {
                total: Some(total.into()),
                sort: None,
                two_line: true,
            };
            assert!(paint_header_cell(&pt, cell, &column, &info("0 B/s"), 0.0).is_none());
            // Wider than the 76 px padding box: through the cell's own
            // padding into the Memory column's 12 px right padding, 2 px
            // clear of its text — even the widest total below 1 GB/s.
            for total in ["16.2 MB/s", "99.9 MB/s"] {
                let (text, r, align) =
                    paint_header_cell(&pt, cell, &column, &info(total), 0.0).unwrap();
                assert_eq!(text, total);
                assert_eq!((r.left, r.right, align), (190, 288, DT_RIGHT));
                assert!(pt.measure(fonts.mono_total, &text).cx <= r.right - r.left);
            }
            // From 100 MB/s the total drops its decimal and fits its own
            // padding box: never closer than two paddings to the Memory total.
            for total in [total_rate(135.6 * MB), total_rate(999.4 * MB)] {
                assert!(paint_header_cell(&pt, cell, &column, &info(&total), 0.0).is_none());
                assert!(pt.measure(fonts.mono_total, &total).cx <= 76);
            }
        }
    }

    #[test]
    fn keyboard_use_shows_a_focus_ring_on_the_focused_row() {
        let h = Harness::new(Mode::Table, 20);
        unsafe {
            let t = h.table;
            let s = state(t);
            SetFocus(t);
            if GetFocus() != t {
                eprintln!("focus unavailable in this session; skipped");
                return;
            }
            // Mouse-initiated: focus cues hidden (UISF_HIDEFOCUS) in the
            // whole window tree.
            SendMessageW(
                t,
                WM_CHANGEUISTATE,
                (UISF_HIDEFOCUS << 16 | UIS_SET) as usize,
                0,
            );
            assert!(!focus_visible(s));
            let mut dib = gfx::Dib::new(400, 240).unwrap();
            let dc = dib.dc();
            let paint = || SendMessageW(t, WM_PRINTCLIENT, dc as usize, 0);
            paint();
            let row1 = part_rect(t, 1, Part::Row).unwrap();
            let y = row1.top + 10;
            assert_eq!(dib.pixel(0, y), dib.pixel(8, y), "no ring");
            // The keyboard shows the cues; the ring marks the selected row.
            h.key(VK_DOWN);
            assert_eq!(h.selected(), 1);
            assert!(focus_visible(s), "keyboard use clears UISF_HIDEFOCUS");
            assert_eq!(focus_row(s), Some(1));
            paint();
            assert_ne!(dib.pixel(0, y), dib.pixel(8, y), "2 px ring inside the row");
            assert_eq!(dib.pixel(0, y), dib.pixel(1, y));
            // No selection: the ring waits on the first data row.
            let item = LVITEMW {
                stateMask: LVIS_SELECTED,
                ..zeroed()
            };
            SendMessageW(t, LVM_SETITEMSTATE, usize::MAX, &item as *const _ as isize);
            assert_eq!(h.selected(), -1);
            assert_eq!(focus_row(s), Some(1), "the group row is skipped");
            SetFocus(null_mut());
            assert!(!focus_visible(s));
        }
    }

    unsafe fn vtbl(object: *mut c_void) -> &'static access::Vtbl {
        &**(object as *const *const access::Vtbl)
    }

    unsafe fn bstr_text(value: windows_sys::core::BSTR) -> String {
        if value.is_null() {
            return String::new();
        }
        let mut len = 0;
        while *value.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(value, len));
        SysFreeString(value);
        text
    }

    #[test]
    fn screen_readers_see_the_rows_through_msaa() {
        use access::*;
        use windows_sys::Win32::UI::Accessibility::AccessibleObjectFromWindow;
        let h = Harness::new(Mode::Table, 20);
        unsafe {
            let t = h.table;
            SetWindowTextW(t, wide("Process list").as_ptr());
            let mut object: *mut c_void = null_mut();
            let hr =
                AccessibleObjectFromWindow(t, OBJID_CLIENT as u32, &IID_IACCESSIBLE, &mut object);
            assert_eq!(hr, 0, "{hr:#x}");
            assert!(!object.is_null());
            let v = vtbl(object);
            let this = object.cast::<access::Object>();
            let id = Variant::i4;
            let name = |child: i32| {
                let mut out = null();
                (v.acc_name)(this, id(child), &mut out);
                bstr_text(out)
            };
            let role = |child: i32| {
                let mut out = Variant::empty();
                assert_eq!((v.acc_role)(this, id(child), &mut out), 0);
                out.as_i4().unwrap()
            };
            let flags = |child: i32| {
                let mut out = Variant::empty();
                assert_eq!((v.acc_state)(this, id(child), &mut out), 0);
                out.as_i4().unwrap()
            };
            let mut count = 0;
            assert_eq!((v.acc_child_count)(this, &mut count), 0);
            assert_eq!(count, 20);
            assert_eq!(name(0), "Process list");
            assert_eq!(role(0), ROLE_LIST);
            // Child id = row + 1: row 0 is a group title, row 2 a data row.
            assert_eq!(role(1), ROLE_GROUPING);
            assert_eq!(role(3), ROLE_LISTITEM);
            assert_eq!(name(3), "Row 2");
            let mut out = null();
            (v.acc_description)(this, id(3), &mut out);
            assert_eq!(bstr_text(out), "Value: Row 2");
            assert_eq!(flags(3) & STATE_SELECTABLE, STATE_SELECTABLE);
            assert_eq!(flags(3) & STATE_SELECTED, 0);
            assert_eq!(
                flags(2) & STATE_COLLAPSED,
                STATE_COLLAPSED,
                "row 1: a tree parent"
            );
            assert_eq!(
                flags(20) & STATE_OFFSCREEN,
                STATE_OFFSCREEN,
                "row 19: below the view"
            );
            let mut child = null_mut();
            assert_eq!(
                (v.acc_child)(this, id(3), &mut child),
                1,
                "a simple element"
            );
            assert!(child.is_null());
            let mut bad = Variant::empty();
            assert_ne!(
                (v.acc_role)(this, id(99), &mut bad),
                0,
                "an invalid child id"
            );
            // Selection and location follow the table.
            h.key(VK_DOWN);
            h.key(VK_DOWN);
            assert_eq!(h.selected(), 2);
            assert_eq!(flags(3) & STATE_SELECTED, STATE_SELECTED);
            let mut selection = Variant::empty();
            assert_eq!((v.acc_selection)(this, &mut selection), 0);
            assert_eq!(selection.as_i4(), Some(3));
            let (mut x, mut y, mut w, mut height) = (0, 0, 0, 0);
            assert_eq!(
                (v.acc_location)(this, &mut x, &mut y, &mut w, &mut height, id(3)),
                0
            );
            let mut r = part_rect(t, 2, Part::Row).unwrap();
            MapWindowPoints(t, null_mut(), (&mut r as *mut RECT).cast::<POINT>(), 2);
            assert_eq!(
                (x, y, w, height),
                (r.left, r.top, r.right - r.left, r.bottom - r.top)
            );
            let mut at = Variant::empty();
            assert_eq!((v.acc_hit_test)(this, x + 20, y + 5, &mut at), 0);
            assert_eq!(at.as_i4(), Some(3));
            let mut next = Variant::empty();
            assert_eq!((v.acc_navigate)(this, NAVDIR_NEXT, id(3), &mut next), 0);
            assert_eq!(next.as_i4(), Some(4));
            // accSelect moves the selection (with the ListView notification).
            events();
            assert_eq!((v.acc_select)(this, SELFLAG_TAKESELECTION, id(6)), 0);
            assert_eq!(h.selected(), 5);
            assert!(events().contains(&(LVN_ITEMCHANGED, 5)));
            let mut out = null();
            (v.acc_default_action)(this, id(6), &mut out);
            assert_eq!(bstr_text(out), tr("두 번 클릭", "Double click"));
            // Once the window is gone the object answers "disconnected".
            DestroyWindow(t);
            let mut out = null();
            assert_eq!((v.acc_name)(this, id(0), &mut out), HR_DISCONNECTED);
            (v.release)(this);
        }
    }

    /// The back buffer (what the last paints presented) against a fresh
    /// full paint of the same state; None when another test flipped the
    /// process-wide theme in between (nothing to compare).
    /// `None` when the process-wide palette changed since `generation`
    /// (parallel tests flip it): the two paints are then not comparable.
    unsafe fn frame_matches_full_paint(t: HWND, generation: u32) -> Option<usize> {
        let s = state(t);
        let mut client: RECT = zeroed();
        GetClientRect(t, &mut client);
        let (w, h) = (client.right, client.bottom);
        let mut full = gfx::Dib::new(w, h).unwrap();
        let mut shown = gfx::Dib::new(w, h).unwrap();
        SendMessageW(t, WM_PRINTCLIENT, full.dc() as usize, 0);
        BitBlt(shown.dc(), 0, 0, w, h, (*s).back.dc()?, 0, 0, SRCCOPY);
        if theme::generation() != generation {
            return None;
        }
        let a = full.pixels().to_vec();
        Some(
            a.iter()
                .zip(shown.pixels().iter())
                .filter(|(x, y)| x != y)
                .count(),
        )
    }

    #[test]
    fn glide_frames_shift_the_back_buffer_and_match_a_full_paint() {
        let h = Harness::new(Mode::Table, 40);
        unsafe {
            let t = h.table;
            let s = state(t);
            // Seven columns (fixed ones shrink to fit): every row has cells
            // under the scrollbar lane and outside a repainted strip.
            let column = |flex: bool| Column {
                label: "C".into(),
                width: if flex { 0.0 } else { 100.0 },
                flex,
                right: !flex,
            };
            set_columns(t, (0..7).map(|i| column(i == 0)).collect());
            // Shown (fully transparent), so WM_PAINT really paints.
            h.show_invisibly();
            UpdateWindow(t);
            assert_eq!(
                (*s).painted_offset,
                Some(0),
                "the first paint covered every row"
            );
            let wheel = |delta: i32| {
                SendMessageW(t, WM_MOUSEWHEEL, (delta as u16 as usize) << 16, 0);
            };
            let frames = Cell::new(0);
            // The palette the back buffer was fully painted in.
            let clean = Cell::new(theme::generation());
            let glide = |delta: i32, hover: Option<POINT>| {
                wheel(delta);
                if let Some(pt) = hover {
                    // A row hover fading in while the rows glide under it.
                    SendMessageW(t, WM_MOUSEMOVE, 0, Harness::at(pt.x, pt.y));
                }
                UpdateWindow(t);
                for _ in 0..200 {
                    if !(*s).anim.anim.is_key_animating(Key::Scroll) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(8));
                    let generation = theme::generation();
                    SendMessageW(t, WM_TIMER, TIMER, 0);
                    // Incremental: nothing inside the rows is left to repaint
                    // but, after a sub-pixel step, the scrollbar lane.
                    let mut due: RECT = zeroed();
                    if GetUpdateRect(t, &mut due, 0) != 0 {
                        let lane = lane(s, client(s));
                        assert!(
                            due.bottom <= header_height(t) || due.left >= lane.left,
                            "{},{}..{},{} left for WM_PAINT",
                            due.left,
                            due.top,
                            due.right,
                            due.bottom
                        );
                    }
                    assert_eq!((*s).painted_offset, Some(offset_px(s)));
                    UpdateWindow(t);
                    if generation != clean.get() {
                        // A parallel test flipped the palette: rows painted
                        // in the other theme may have been shifted into
                        // this frame. Repaint everything and go on.
                        clean.set(theme::generation());
                        InvalidateRect(t, null(), 0);
                        UpdateWindow(t);
                    } else if let Some(diff) = frame_matches_full_paint(t, generation) {
                        assert_eq!(diff, 0, "frame {} at {}", frames.get(), offset_px(s));
                    }
                    frames.set(frames.get() + 1);
                }
            };
            glide(-240, None);
            let down = offset_px(s);
            assert!(down > 0);
            glide(120, Some(POINT { x: 100, y: 150 }));
            assert!(offset_px(s) < down, "scrolled back up");
            assert!(frames.get() >= 4, "{} frames", frames.get());
            // A jump falls back to a full repaint, then glides go on.
            scroll_to(t, extent(t).max());
            UpdateWindow(t);
            assert_eq!((*s).painted_offset, Some(offset_px(s)));
            glide(-120, None);
            let generation = theme::generation();
            if generation == clean.get() {
                if let Some(diff) = frame_matches_full_paint(t, generation) {
                    assert_eq!(diff, 0, "after the jump");
                }
            }
            ShowWindow(h.parent, SW_HIDE);
        }
    }

    /// Glide frames create clip regions and borrow the window DC: 300 of
    /// them (both directions, with a fading hover row, sub-pixel steps and
    /// jumps) must leave the GDI and USER object counts flat. Measured in a
    /// child test process (the counts are per process; other tests run
    /// concurrently).
    #[test]
    fn glide_frames_do_not_leak_gdi_objects() {
        const CHILD: &str = "FEATHER_TABLE_LEAK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ui::table::tests::glide_frames_do_not_leak_gdi_objects",
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
                stdout.contains("table-leak-check: ok"),
                "{stdout}\n{stderr}"
            );
            return;
        }
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS, GR_USEROBJECTS,
        };
        let h = Harness::new(Mode::Table, 60);
        unsafe {
            let t = h.table;
            let s = state(t);
            h.show_invisibly();
            UpdateWindow(t);
            let process = GetCurrentProcess();
            let frames = |n: usize| {
                for i in 0..n {
                    let max = extent(t).max();
                    let step = if (i / 40) % 2 == 0 { 7.4 } else { -7.4 };
                    let offset = ((*s).scroll.offset(&(*s).anim) + step).clamp(0.0, max);
                    (*s).anim.set(Key::Scroll, offset);
                    let y = 40 + (i as i32 * 13) % 190;
                    SendMessageW(t, WM_MOUSEMOVE, 0, Harness::at(100, y));
                    sync_scroll(s);
                    if i % 50 == 25 {
                        scroll_to(t, max / 2.0);
                    }
                    UpdateWindow(t);
                }
            };
            frames(20);
            let gdi = GetGuiResources(process, GR_GDIOBJECTS);
            let user = GetGuiResources(process, GR_USEROBJECTS);
            frames(300);
            SendMessageW(t, WM_MOUSELEAVE, 0, 0);
            settle(t);
            UpdateWindow(t);
            let gdi_after = GetGuiResources(process, GR_GDIOBJECTS);
            let user_after = GetGuiResources(process, GR_USEROBJECTS);
            assert_eq!(gdi_after, gdi, "GDI objects leaked");
            // USER counts may drop by one (lazily freed input state), never grow.
            assert!(
                user_after <= user,
                "USER objects leaked: {user} -> {user_after}"
            );
            println!("table-leak-check: ok gdi {gdi}->{gdi_after} user {user}->{user_after}");
        }
    }

    thread_local! {
        static WIN_EVENTS: RefCell<Vec<(u32, i32)>> = const { RefCell::new(Vec::new()) };
        static WATCHED: Cell<HWND> = const { Cell::new(null_mut()) };
    }

    unsafe extern "system" fn on_win_event(
        _hook: windows_sys::Win32::UI::Accessibility::HWINEVENTHOOK,
        event: u32,
        hwnd: HWND,
        object: i32,
        child: i32,
        _thread: u32,
        _time: u32,
    ) {
        if hwnd == WATCHED.with(Cell::get) && object == OBJID_CLIENT {
            WIN_EVENTS.with(|e| e.borrow_mut().push((event, child)));
        }
    }

    fn pump() -> Vec<(u32, i32)> {
        unsafe {
            let mut msg: MSG = zeroed();
            for _ in 0..5 {
                while PeekMessageW(&mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        WIN_EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut()))
    }

    #[test]
    fn selection_changes_are_announced_once_by_identity() {
        use access::{EVENT_FOCUS, EVENT_SELECTION, EVENT_SELECTIONWITHIN};
        use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
        use windows_sys::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
        let h = Harness::new(Mode::Table, 20);
        unsafe {
            let t = h.table;
            h.show_invisibly();
            WATCHED.with(|w| w.set(t));
            let hook = SetWinEventHook(
                EVENT_FOCUS,
                EVENT_SELECTIONWITHIN,
                null_mut(),
                Some(on_win_event),
                GetCurrentProcessId(),
                GetCurrentThreadId(),
                WINEVENT_OUTOFCONTEXT,
            );
            assert!(!hook.is_null());
            pump();
            let item = |on: bool| LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: if on { LVIS_SELECTED | LVIS_FOCUSED } else { 0 },
                ..zeroed()
            };
            let set = |row: usize, on: bool| {
                SendMessageW(t, LVM_SETITEMSTATE, row, &item(on) as *const _ as isize);
            };
            set(4, true);
            assert!(pump().contains(&(EVENT_SELECTION, 5)));
            // A rebuild clears and restores the same row: nothing to say.
            set(usize::MAX, false);
            SendMessageW(t, LVM_SETITEMCOUNT, 20, 0);
            set(4, true);
            assert_eq!(pump(), vec![]);
            // Another row: one selection event.
            set(6, true);
            let events = pump();
            assert_eq!(events.iter().filter(|e| e.0 == EVENT_SELECTION).count(), 1);
            assert!(events.contains(&(EVENT_SELECTION, 7)));
            UnhookWinEvent(hook);
            WATCHED.with(|w| w.set(null_mut()));
        }
    }
}
