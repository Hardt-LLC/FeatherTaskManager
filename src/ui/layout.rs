//! The main window's single geometry model (DESIGN_SPEC §3, §5).
//!
//! Every region is a device-pixel `RECT` computed from the client size and the
//! DPI. `ui::layout` positions the child windows from it (MoveWindow) and
//! `paint` draws the chrome from the same rectangles, so painted frames and
//! native controls always line up. Later tracks (custom frame, table, popups)
//! position their parts from here too.
//!
//! Numbers are the reference's CSS px (= DIP), converted edge by edge with
//! rounding (`gfx::pxi`), so neighbouring regions stay seamless at any DPI.
//! Fractional CSS line boxes use Chromium's 1/64 px layout units, which is how
//! the reference renders snap (e.g. settings rows land on the same pixels).
use super::gfx::{hairline, px};
use super::Page;
use windows_sys::Win32::Foundation::RECT;

/// Title strip height (brand, search, caption buttons share it).
pub(super) const TITLEBAR: f32 = 44.0;
/// Nav rail width (the main panel's left border follows it).
pub(super) const RAIL: f32 = 220.0;
/// Status bar height including its top border.
pub(super) const STATUS: f32 = 30.0;
/// Room reserved at the right of the title strip for three 46 px caption buttons.
pub(super) const CAPTION: f32 = 138.0;
pub(super) const SEARCH_MAX: f32 = 460.0;
pub(super) const SEARCH_TOP: f32 = 6.0;
pub(super) const SEARCH_HEIGHT: f32 = 32.0;
/// Search input padding: icon side / kbd side.
pub(super) const SEARCH_PAD_LEFT: f32 = 34.0;
pub(super) const SEARCH_PAD_RIGHT: f32 = 64.0;
pub(super) const BRAND_X: f32 = 16.0;
pub(super) const BRAND_ICON: f32 = 18.0;
pub(super) const BRAND_GAP: f32 = 10.0;
pub(super) const NAV_ITEM: f32 = 40.0;
pub(super) const NAV_GAP: f32 = 2.0;
pub(super) const NAV_PAD_TOP: f32 = 4.0;
pub(super) const NAV_PAD_X: f32 = 8.0;
pub(super) const NAV_PAD_BOTTOM: f32 = 8.0;
pub(super) const HEAD_PAD_X: f32 = 20.0;
pub(super) const HEAD_PAD_Y: f32 = 14.0;
/// `h1` 20 px / 1.2.
pub(super) const H1_LINE: f32 = 24.0;
/// `.meta` 12 px with the body's inherited line-height 1.45.
pub(super) const META_LINE: f32 = 17.4;
/// The body's `line-height: 1.45`, inherited as a factor by every text that
/// does not reset it (table cells, settings rows, segmented buttons, group
/// rows): the line box is `font-size × 1.45`.
pub(super) const BODY_LINE: f32 = 1.45;
/// Gap between the h1 and the meta text, and between head actions.
pub(super) const HEAD_GAP: f32 = 12.0;
pub(super) const ACTION_GAP: f32 = 8.0;
pub(super) const BUTTON_HEIGHT: f32 = 32.0;
pub(super) const SELECT_HEIGHT: f32 = 28.0;
pub(super) const STATUS_SELECT_HEIGHT: f32 = 22.0;
/// `.btn` horizontal padding (each side).
pub(super) const BUTTON_PAD_X: f32 = 14.0;
/// A `<select>`'s width beyond its widest option: padding 6 + 6 and the arrow.
pub(super) const SELECT_EXTRA: f32 = 34.0;
pub(super) const STATUS_PAD_LEFT: f32 = 16.0;
pub(super) const STATUS_PAD_RIGHT: f32 = 12.0;
pub(super) const STATUS_GAP: f32 = 18.0;
/// Status dot: 6 px + 6 px margin before the state text.
pub(super) const STATUS_DOT: f32 = 6.0;

/// Performance: devices column (incl. its right border), padding and cards.
pub(super) const DEVICES_WIDTH: f32 = 260.0;
pub(super) const DEVICES_PAD: f32 = 8.0;
pub(super) const DEVICE_CARD: f32 = 56.0;
pub(super) const DEVICE_GAP: f32 = 2.0;
pub(super) const PERF_PAD_X: f32 = 24.0;
pub(super) const PERF_PAD_Y: f32 = 20.0;

/// Processes telemetry drawer (bottom of the content rect) and the services
/// details panel (right of it; only in windows at least 1100 px wide). The
/// table keeps at least 60 px / 200 px.
pub(super) const DRAWER: f32 = 180.0;
pub(super) const DRAWER_MIN_TABLE: f32 = 60.0;
pub(super) const DETAILS: f32 = 290.0;
pub(super) const DETAILS_MIN_TABLE: f32 = 200.0;
pub(super) const DETAILS_MIN_CLIENT: f32 = 1100.0;

/// Settings column.
pub(super) const SETTINGS_MAX: f32 = 760.0;
pub(super) const SETTINGS_PAD_X: f32 = 24.0;
pub(super) const SETTINGS_PAD_TOP: f32 = 20.0;
pub(super) const SETTINGS_GROUP_GAP: f32 = 24.0;
/// Group `h2`: 15 px / 1.3, margin-bottom 8.
pub(super) const SETTINGS_TITLE: f32 = 19.5;
pub(super) const SETTINGS_TITLE_GAP: f32 = 8.0;
pub(super) const SETTINGS_ROW_PAD_Y: f32 = 14.0;
pub(super) const SETTINGS_ROW_PAD_X: f32 = 16.0;
/// Row title 13 px / 1.45 and description 12 px / 1.45 in 1/64 px units.
pub(super) const SETTINGS_ROW_TITLE: f32 = 18.843_75;
pub(super) const SETTINGS_ROW_TEXT: f32 = 17.390_625;
/// Rows per settings group: Appearance (theme, language), Updates (refresh
/// rate, start page), Window (always on top, tray, Task Manager replacement).
pub(super) const SETTINGS_GROUPS: [usize; 3] = [2, 2, 3];

/// Height of the page head's content box: the tallest of the h1 line (24) and
/// the page's actions (32 px buttons incl. the more button, 28 px selects),
/// as the reference's flex row computes it. The head adds 14 + 14 padding and
/// its 1 px bottom border: 61 px with buttons, 53 px without (Settings).
pub(super) fn head_content(page: Page) -> f32 {
    match page {
        Page::Processes | Page::Performance | Page::Services => BUTTON_HEIGHT,
        // Only a note, the 28 px filter and a 28 px ⋯: 57 px, closer to the
        // reference's 53 px text-only head than 32 px actions would be.
        Page::Startup => SELECT_HEIGHT,
        Page::Settings => H1_LINE,
    }
}

/// Table header height including its bottom border: two lines on Processes
/// (8 + 15/1.2 + 12/1.45 + 8), one label line elsewhere (8 + 12/1.45 + 8).
pub(super) fn table_header_height(page: Page, dpi: i32) -> i32 {
    let content = if page == Page::Processes {
        8.0 + 18.0 + 17.390_625 + 8.0
    } else {
        8.0 + 17.390_625 + 8.0
    };
    px(dpi, content).round() as i32 + hairline(dpi) as i32
}

/// Performance device card pitch (56 px card + 2 px gap) for the owner-draw
/// list items; the card is painted in the item's top 56 px.
pub(super) fn device_item_height(dpi: i32) -> i32 {
    px(dpi, DEVICE_CARD + DEVICE_GAP).round() as i32
}

/// Table data row pitch: 33 px + 1 px row border.
pub(super) fn table_row_height(dpi: i32) -> i32 {
    px(dpi, 33.0).round() as i32 + hairline(dpi) as i32
}

#[derive(Clone, Copy)]
pub(super) struct Layout {
    pub dpi: i32,
    /// A CSS 1 px border in device px.
    pub hair: i32,
    pub client: RECT,
    /// The whole 44 px title strip (window background).
    pub titlebar: RECT,
    /// 18 × 18 feather at x 16, vertically centred.
    pub brand_icon: RECT,
    /// Brand text box (13 px, vertically centred in the strip).
    pub brand_text: RECT,
    /// 32 px search box, centred in the middle column, width min(460, room).
    pub search: RECT,
    /// Where the search input's text lives (left + 34 .. right − 64).
    pub search_text: RECT,
    /// Reserved for the minimize / maximize / close buttons (3 × 46); the
    /// custom frame's caption buttons and hit testing live here.
    pub caption: RECT,
    /// Nav rail below the title strip (window background).
    pub rail: RECT,
    pub nav: [RECT; 4],
    pub nav_settings: RECT,
    /// Main panel including its top and left 1 px border (radius 8 top-left).
    pub main: RECT,
    /// Page head including its bottom border.
    pub head: RECT,
    /// Page head content box (inside the 14 / 20 padding).
    pub head_inner: RECT,
    /// The view below the page head (tables, performance, settings).
    pub content: RECT,
    /// Status bar including its top border.
    pub status: RECT,
    /// Status bar content box (inside the border and 16 / 12 padding).
    pub status_inner: RECT,
}

impl Layout {
    /// `width` × `height` is the client size in device px.
    pub(super) fn new(width: i32, height: i32, dpi: i32, page: Page) -> Self {
        let dpi = dpi.max(48);
        let d = |v: f32| px(dpi, v).round() as i32;
        let hair = hairline(dpi) as i32;
        let client = rect(0, 0, width.max(0), height.max(0));
        let (w, h) = (client.right, client.bottom);
        let top = d(TITLEBAR);
        let status_top = (h - d(STATUS)).max(top);
        let titlebar = rect(0, 0, w, top);
        let icon = d(BRAND_ICON);
        let brand_icon = RECT {
            left: d(BRAND_X),
            top: (top - icon) / 2,
            right: d(BRAND_X) + icon,
            bottom: (top - icon) / 2 + icon,
        };
        let rail_right = d(RAIL);
        let brand_text = rect(
            d(BRAND_X + BRAND_ICON + BRAND_GAP),
            0,
            rail_right.min(w),
            top,
        );
        let caption = rect((w - d(CAPTION)).max(0), 0, w, top);
        // `.search { justify-self: center; width: min(460px, 100%) }` in the
        // 1fr column between the 220 px brand column and the caption buttons.
        let column = (rail_right, caption.left.max(rail_right));
        let room = column.1 - column.0;
        let search_w = d(SEARCH_MAX).min(room).max(0);
        let search_left = column.0 + (room - search_w) / 2;
        let search = rect(
            search_left,
            d(SEARCH_TOP),
            search_left + search_w,
            d(SEARCH_TOP + SEARCH_HEIGHT),
        );
        // CSS padding sits inside the 1 px border.
        let search_text = RECT {
            left: search.left + hair + d(SEARCH_PAD_LEFT),
            right: (search.right - hair - d(SEARCH_PAD_RIGHT))
                .max(search.left + hair + d(SEARCH_PAD_LEFT)),
            ..search
        };
        let rail = rect(0, top, rail_right.min(w), status_top);
        let mut nav = [RECT::default(); 4];
        for (i, item) in nav.iter_mut().enumerate() {
            let y = TITLEBAR + NAV_PAD_TOP + i as f32 * (NAV_ITEM + NAV_GAP);
            *item = rect(d(NAV_PAD_X), d(y), d(RAIL - NAV_PAD_X), d(y + NAV_ITEM));
        }
        let settings_bottom = status_top - d(NAV_PAD_BOTTOM);
        let nav_settings = rect(
            d(NAV_PAD_X),
            settings_bottom - d(NAV_ITEM),
            d(RAIL - NAV_PAD_X),
            settings_bottom,
        );
        let main = rect(rail_right.min(w), top, w, status_top);
        let head_top = top + hair;
        let head_inner_top = head_top + d(HEAD_PAD_Y);
        let head_inner_bottom = head_top + d(HEAD_PAD_Y + head_content(page));
        let head_bottom = head_top + d(2.0 * HEAD_PAD_Y + head_content(page)) + hair;
        let head = rect(main.left + hair, head_top, w, head_bottom.min(status_top));
        let head_inner = rect(
            head.left + d(HEAD_PAD_X),
            head_inner_top,
            (w - d(HEAD_PAD_X)).max(head.left + d(HEAD_PAD_X)),
            head_inner_bottom,
        );
        let content = rect(head.left, head.bottom, w, status_top.max(head.bottom));
        let status = rect(0, status_top, w, h);
        let status_inner = rect(
            d(STATUS_PAD_LEFT),
            status_top + hair,
            (w - d(STATUS_PAD_RIGHT)).max(d(STATUS_PAD_LEFT)),
            h,
        );
        Self {
            dpi,
            hair,
            client,
            titlebar,
            brand_icon,
            brand_text,
            search,
            search_text,
            caption,
            rail,
            nav,
            nav_settings,
            main,
            head,
            head_inner,
            content,
            status,
            status_inner,
        }
    }

    /// DIP → device px (rounded).
    pub(super) fn px(&self, dip: f32) -> i32 {
        px(self.dpi, dip).round() as i32
    }

    /// A box of `height` px vertically centred in `outer` (rounding like
    /// Chromium's flex centring: an odd remainder goes below).
    pub(super) fn center_v(outer: RECT, height: i32) -> (i32, i32) {
        let top = outer.top + (outer.bottom - outer.top - height + 1) / 2;
        (top, top + height)
    }

    /// Page head actions (right aligned, 8 px apart, vertically centred in
    /// the head's content box). `sizes` are (width, height) in device px in
    /// visual left-to-right order; the rectangles come back in that order.
    pub(super) fn head_actions(&self, sizes: &[(i32, i32)]) -> Vec<RECT> {
        flow_right(
            self.head_inner.right,
            self.head_inner,
            sizes,
            self.px(ACTION_GAP),
        )
    }

    /// The status bar's Refresh select (22 px tall, `width` px wide) at the
    /// right padding edge, vertically centred in the bar.
    pub(super) fn rate(&self, width: i32) -> RECT {
        let height = self.px(STATUS_SELECT_HEIGHT);
        let (top, bottom) = Self::center_v(self.status_inner, height);
        RECT {
            left: self.status_inner.right - width,
            top,
            right: self.status_inner.right,
            bottom,
        }
    }

    /// Search input rectangle for an edit control whose text line is
    /// `line_height` px: vertically centred in the 32 px search box.
    pub(super) fn search_edit(&self, line_height: i32) -> RECT {
        let (top, bottom) = Self::center_v(
            self.search,
            line_height.min(self.search.bottom - self.search.top),
        );
        RECT {
            top,
            bottom,
            ..self.search_text
        }
    }

    /// The processes table and, when `shown`, the telemetry drawer below it:
    /// the drawer is the content rect's bottom 180 px (its 1 px top border
    /// included) and the table ends exactly where it starts. Both span the
    /// content width, so the main panel's left border stays visible.
    pub(super) fn drawer(&self, shown: bool) -> (RECT, Option<RECT>) {
        if !shown {
            return (self.content, None);
        }
        let top = (self.content.bottom - self.px(DRAWER))
            .max(self.content.top + self.px(DRAWER_MIN_TABLE))
            .min(self.content.bottom);
        (
            RECT {
                bottom: top,
                ..self.content
            },
            Some(RECT {
                top,
                ..self.content
            }),
        )
    }

    /// The services table and, when `shown` and the client is at least
    /// 1100 px wide, the details panel at the content rect's right 290 px
    /// (its 1 px left border included).
    pub(super) fn details(&self, shown: bool) -> (RECT, Option<RECT>) {
        if !shown || self.client.right < self.px(DETAILS_MIN_CLIENT) {
            return (self.content, None);
        }
        let left = (self.content.right - self.px(DETAILS))
            .max(self.content.left + self.px(DETAILS_MIN_TABLE))
            .min(self.content.right);
        (
            RECT {
                right: left,
                ..self.content
            },
            Some(RECT {
                left,
                ..self.content
            }),
        )
    }

    /// Performance devices column including its right border.
    pub(super) fn perf_devices(&self) -> RECT {
        RECT {
            right: (self.content.left + self.px(DEVICES_WIDTH)).min(self.content.right),
            ..self.content
        }
    }

    /// The device card list: the whole devices column but its right
    /// border. Its 8 px padding (`DEVICES_PAD`) is the list's content inset
    /// and scrolls with the cards, so the overlay scrollbar sits in the
    /// right gutter and the cards scroll up to the head border.
    pub(super) fn perf_device_list(&self) -> RECT {
        let column = self.perf_devices();
        RECT {
            right: (column.right - self.hair).max(column.left),
            ..column
        }
    }

    /// `.perf-main` content box (padding 20 24) right of the devices column.
    pub(super) fn perf_main(&self) -> RECT {
        let column = self.perf_devices();
        RECT {
            left: column.right + self.px(PERF_PAD_X),
            top: self.content.top + self.px(PERF_PAD_Y),
            right: (self.content.right - self.px(PERF_PAD_X))
                .max(column.right + self.px(PERF_PAD_X)),
            bottom: (self.content.bottom - self.px(PERF_PAD_Y))
                .max(self.content.top + self.px(PERF_PAD_Y)),
        }
    }

    /// Settings column: max 760 wide, padding 20 24 40.
    pub(super) fn settings_column(&self) -> RECT {
        let width = (self.content.right - self.content.left).min(self.px(SETTINGS_MAX));
        RECT {
            left: self.content.left + self.px(SETTINGS_PAD_X),
            top: self.content.top + self.px(SETTINGS_PAD_TOP),
            right: (self.content.left + width - self.px(SETTINGS_PAD_X))
                .max(self.content.left + self.px(SETTINGS_PAD_X)),
            bottom: self.content.bottom,
        }
    }

    /// Settings groups: title line, the joined-rows frame and each row's box
    /// between its borders. Lengths accumulate in (fractional) device px and
    /// every edge rounds; borders are whole device px like Chromium's border
    /// snapping, so separators stay exactly 1 px at 125 / 150 %.
    ///
    /// The reference's view scrolls when the groups are taller than the
    /// window; this page is painted, so near the minimum height the rows'
    /// vertical padding (14 → ≥ 6) and then the group gap (24 → ≥ 12) shrink
    /// until every row, and so every setting, stays on screen.
    pub(super) fn settings(&self) -> Settings {
        let column = self.settings_column();
        let rows_total: usize = SETTINGS_GROUPS.iter().sum();
        let groups_total = SETTINGS_GROUPS.len();
        let hair = self.hair as f32;
        let dip = |v: f32| px(self.dpi, v);
        let height = |pad: f32, gap: f32| {
            groups_total as f32 * dip(SETTINGS_TITLE + SETTINGS_TITLE_GAP)
                + (groups_total - 1) as f32 * dip(gap)
                + rows_total as f32 * dip(2.0 * pad + SETTINGS_ROW_TITLE + SETTINGS_ROW_TEXT)
                + (rows_total + groups_total) as f32 * hair
        };
        let available = (self.content.bottom - column.top - self.px(8.0)) as f32;
        let (mut pad, mut gap) = (SETTINGS_ROW_PAD_Y, SETTINGS_GROUP_GAP);
        let excess = height(pad, gap) - available;
        if excess > 0.0 {
            pad = (pad - excess / dip(1.0) / (2.0 * rows_total as f32)).max(6.0);
            let excess = height(pad, gap) - available;
            if excess > 0.0 {
                gap = (gap - excess / dip(1.0) / (groups_total - 1) as f32).max(12.0);
            }
        }
        let edge = |y: f32| y.round() as i32;
        let mut y = column.top as f32;
        let mut groups = Vec::with_capacity(groups_total);
        for (index, &count) in SETTINGS_GROUPS.iter().enumerate() {
            if index > 0 {
                y += dip(gap);
            }
            let title_y = y;
            let title = RECT {
                left: column.left,
                top: edge(y),
                right: column.right,
                bottom: edge(y + dip(SETTINGS_TITLE)),
            };
            y += dip(SETTINGS_TITLE + SETTINGS_TITLE_GAP);
            let frame_top = edge(y);
            y += hair;
            let mut rows = Vec::with_capacity(count);
            let mut row_y = Vec::with_capacity(count);
            for _ in 0..count {
                row_y.push(y);
                let top = edge(y);
                y += dip(2.0 * pad + SETTINGS_ROW_TITLE + SETTINGS_ROW_TEXT);
                rows.push(RECT {
                    left: column.left + self.hair,
                    top,
                    right: column.right - self.hair,
                    bottom: edge(y),
                });
                y += hair;
            }
            let frame = RECT {
                left: column.left,
                top: frame_top,
                right: column.right,
                bottom: edge(y),
            };
            groups.push(SettingsGroup {
                title,
                title_y,
                frame,
                rows,
                row_y,
                pad_y: pad,
            });
        }
        y += dip(gap);
        let footer = RECT {
            left: column.left,
            top: edge(y),
            right: column.right,
            bottom: edge(y + dip(18.0)),
        };
        Settings { groups, footer }
    }
}

/// One caption button (minimize, maximize / restore, close): 46 × 44.
pub(super) const CAPTION_BUTTON: f32 = 46.0;

impl Layout {
    /// The minimize, maximize / restore and close buttons (left to right),
    /// full title-strip height, tiling `caption` from the client's right
    /// edge (edges rounded from the right, so they stay seamless at any DPI).
    pub(super) fn caption_buttons(&self) -> [RECT; 3] {
        let right = self.caption.right;
        let edge = |n: f32| right - self.px(n * CAPTION_BUTTON);
        let (top, bottom) = (self.titlebar.top, self.titlebar.bottom);
        [
            rect(edge(3.0).max(0), top, edge(2.0).max(0), bottom),
            rect(edge(2.0).max(0), top, edge(1.0).max(0), bottom),
            rect(edge(1.0).max(0), top, right, bottom),
        ]
    }
}

/// Settings page geometry (see [`Layout::settings`]).
#[derive(Clone)]
pub(super) struct Settings {
    pub groups: Vec<SettingsGroup>,
    /// Version line below the last group.
    pub footer: RECT,
}

#[derive(Clone)]
pub(super) struct SettingsGroup {
    /// Group `h2` line box.
    pub title: RECT,
    /// The h2 line box's unrounded top (device px).
    pub title_y: f32,
    /// Outer border box of the joined rows (radius 4 on the outer corners).
    pub frame: RECT,
    /// Each row's box between its borders (the 1 px separator lies between
    /// `rows[i].bottom` and `rows[i + 1].top`).
    pub rows: Vec<RECT>,
    /// Each row's unrounded top (device px) — `rows[i].top` before rounding.
    /// Text and the segmented control are placed from these, like Chromium
    /// lays the rows out in fractional units and snaps only when painting.
    pub row_y: Vec<f32>,
    /// Vertical row padding in CSS px (14, less in the compact fallback).
    pub pad_y: f32,
}

impl SettingsGroup {
    /// A row's unrounded content top and height (device px): the text block
    /// (title + description line boxes) the row's items centre on.
    pub(super) fn content_y(&self, row: usize, dpi: i32) -> (f32, f32) {
        (
            self.row_y[row] + px(dpi, self.pad_y),
            px(dpi, SETTINGS_ROW_TITLE + SETTINGS_ROW_TEXT),
        )
    }
    /// A row's content box (inside padding 14 16).
    pub(super) fn row_content(&self, row: usize, dpi: i32) -> RECT {
        let r = self.rows[row];
        let d = |v: f32| px(dpi, v).round() as i32;
        RECT {
            left: r.left + d(SETTINGS_ROW_PAD_X),
            top: r.top + d(self.pad_y),
            right: r.right - d(SETTINGS_ROW_PAD_X),
            bottom: r.bottom - d(self.pad_y),
        }
    }
}

/// Lay out boxes right to left from `right`, `gap` apart, each vertically
/// centred in `band`. `sizes` are in visual (left-to-right) order and the
/// result keeps that order.
pub(super) fn flow_right(right: i32, band: RECT, sizes: &[(i32, i32)], gap: i32) -> Vec<RECT> {
    let mut rects = vec![RECT::default(); sizes.len()];
    let mut x = right;
    for (i, &(w, h)) in sizes.iter().enumerate().rev() {
        let (top, bottom) = Layout::center_v(band, h);
        rects[i] = RECT {
            left: x - w,
            top,
            right: x,
            bottom,
        };
        x -= w + gap;
    }
    rects
}

const fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RECT has no PartialEq/Debug in windows-sys; compare edge tuples.
    fn r(left: i32, top: i32, right: i32, bottom: i32) -> (i32, i32, i32, i32) {
        (left, top, right, bottom)
    }
    fn t(r: RECT) -> (i32, i32, i32, i32) {
        (r.left, r.top, r.right, r.bottom)
    }

    /// 1200 × 820 client at 96 DPI against the reference's measured pixels
    /// (reference PNG coordinates minus its 1 px window border).
    #[test]
    fn reference_window_geometry_at_1200_by_820() {
        let l = Layout::new(1200, 820, 96, Page::Processes);
        assert_eq!(l.hair, 1);
        assert_eq!(t(l.titlebar), r(0, 0, 1200, 44));
        assert_eq!(t(l.brand_icon), r(16, 13, 34, 31));
        assert_eq!(l.brand_text.left, 44);
        assert_eq!(t(l.caption), r(1062, 0, 1200, 44));
        // 460 wide, centred in 220..1062, 32 tall at y 6.
        assert_eq!(t(l.search), r(411, 6, 871, 38));
        assert_eq!(t(l.search_text), r(446, 6, 806, 38));
        assert_eq!(t(l.nav[0]), r(8, 48, 212, 88));
        assert_eq!(t(l.nav[1]), r(8, 90, 212, 130));
        assert_eq!(t(l.nav[3]), r(8, 174, 212, 214));
        // Settings sits 8 px above the status bar.
        assert_eq!(t(l.nav_settings), r(8, 742, 212, 782));
        assert_eq!(t(l.rail), r(0, 44, 220, 790));
        assert_eq!(t(l.main), r(220, 44, 1200, 790));
        // Head: 1 px main border, 14 + 32 + 14, 1 px bottom border.
        assert_eq!(t(l.head), r(221, 45, 1200, 106));
        assert_eq!(t(l.head_inner), r(241, 59, 1180, 91));
        assert_eq!(t(l.content), r(221, 106, 1200, 790));
        assert_eq!(t(l.status), r(0, 790, 1200, 820));
        assert_eq!(t(l.status_inner), r(16, 791, 1188, 820));
        // Refresh select 77 × 22 at the right padding, centred like the reference.
        assert_eq!(t(l.rate(77)), r(1111, 795, 1188, 817));
        assert_eq!(table_header_height(Page::Processes, 96), 52);
        assert_eq!(table_header_height(Page::Startup, 96), 34);
        assert_eq!(table_row_height(96), 34);
    }

    #[test]
    fn head_height_follows_the_tallest_action_like_the_reference() {
        for (page, bottom) in [
            (Page::Processes, 106),
            (Page::Services, 106),
            (Page::Performance, 106),
            // A note, a 28 px select and a 28 px ⋯: 57 px (the reference's
            // text-only Startup head is 53).
            (Page::Startup, 102),
            // No actions: the h1 line (24) sets the height, as on the
            // reference's Settings / Startup / Performance pages (53 px).
            (Page::Settings, 98),
        ] {
            let l = Layout::new(1200, 820, 96, page);
            assert_eq!(l.head.bottom, bottom, "{page:?}");
            assert_eq!(l.content.top, bottom);
            assert_eq!(l.head_inner.top, 59);
        }
    }

    #[test]
    fn head_actions_flow_right_with_gap_and_centre_by_height() {
        let l = Layout::new(1200, 820, 96, Page::Processes);
        // Reference: Efficiency mode 120 + End task 80, 8 apart, right edge 1180.
        let rects = l.head_actions(&[(120, 32), (80, 32), (100, 28)]);
        assert_eq!(t(rects[2]), r(1080, 61, 1180, 89));
        assert_eq!(t(rects[1]), r(992, 59, 1072, 91));
        assert_eq!(t(rects[0]), r(864, 59, 984, 91));
        let odd = flow_right(20, rect(0, 0, 100, 20), &[(5, 7), (7, 20)], 3);
        assert_eq!(t(odd[0]), r(5, 7, 10, 14));
        assert_eq!(t(odd[1]), r(13, 0, 20, 20));
    }

    #[test]
    fn minimum_window_geometry_at_980_by_660() {
        let l = Layout::new(980, 660, 96, Page::Performance);
        assert_eq!(t(l.caption), r(842, 0, 980, 44));
        // The middle column (220..842) is 622 wide: the search stays 460.
        assert_eq!(t(l.search), r(301, 6, 761, 38));
        assert_eq!(t(l.nav_settings), r(8, 582, 212, 622));
        assert_eq!(t(l.status), r(0, 630, 980, 660));
        assert_eq!(t(l.content), r(221, 106, 980, 630));
        assert_eq!(t(l.perf_devices()), r(221, 106, 481, 630));
        assert_eq!(t(l.perf_device_list()), r(221, 106, 480, 630));
        assert_eq!(t(l.perf_main()), r(505, 126, 956, 610));
        // A narrow window shrinks the search below 460 and keeps it centred.
        let narrow = Layout::new(700, 500, 96, Page::Processes);
        assert_eq!(t(narrow.search), r(220, 6, 562, 38));
    }

    #[test]
    fn geometry_scales_with_dpi_at_150_percent() {
        let l = Layout::new(1800, 1230, 144, Page::Processes);
        assert_eq!(l.hair, 1);
        assert_eq!(t(l.titlebar), r(0, 0, 1800, 66));
        assert_eq!(t(l.brand_icon), r(24, 19, 51, 46));
        assert_eq!(t(l.caption), r(1593, 0, 1800, 66));
        assert_eq!(t(l.search), r(616, 9, 1306, 57));
        assert_eq!(t(l.nav[0]), r(12, 72, 318, 132));
        assert_eq!(t(l.nav[1]), r(12, 135, 318, 195));
        assert_eq!(t(l.main), r(330, 66, 1800, 1185));
        assert_eq!(t(l.head), r(331, 67, 1800, 158));
        assert_eq!(t(l.head_inner), r(361, 88, 1770, 136));
        assert_eq!(l.content.top, 158);
        assert_eq!(t(l.status), r(0, 1185, 1800, 1230));
        assert_eq!(t(l.nav_settings), r(12, 1113, 318, 1173));
        assert_eq!(table_header_height(Page::Processes, 144), 78);
        assert_eq!(table_row_height(144), 51);
        let perf = Layout::new(1800, 1230, 144, Page::Performance);
        assert_eq!(t(perf.perf_devices()), r(331, 158, 721, 1185));
        assert_eq!(device_item_height(144), 87);
        assert_eq!(device_item_height(96), 58);
    }

    /// The reference's settings borders (client y): 146, 211 | 263, 328, 394 | 446, 511.
    #[test]
    fn settings_rows_join_and_snap_like_the_reference() {
        let l = Layout::new(1200, 820, 96, Page::Settings);
        assert_eq!(t(l.settings_column()), r(245, 118, 957, 790));
        let s = l.settings();
        assert_eq!(s.groups.len(), 3);
        let g = &s.groups[0];
        assert_eq!(t(g.title), r(245, 118, 957, 138));
        assert_eq!(g.frame.top, 146);
        assert_eq!(t(g.rows[0]), r(246, 147, 956, 211));
        // Joined rows: one 1 px separator between them, no double border.
        assert_eq!(g.rows[1].top, g.rows[0].bottom + 1);
        assert_eq!(t(g.row_content(0, 96)), r(262, 161, 940, 197));
        // Unrounded edges (Chromium's layout units): the h2 at 118, the
        // frame at 145.5 (painted at 146), the text block from 160.5.
        assert_eq!(g.title_y, 118.0);
        assert_eq!(g.row_y[0], 146.5);
        assert_eq!(g.content_y(0, 96), (160.5, 36.234375));
        let wide = Layout::new(1800, 1230, 144, Page::Settings).settings();
        assert_eq!(wide.groups[0].frame.top, 217);
        assert_eq!(wide.groups[0].content_y(0, 144).0, 239.25);
        // Group 2 starts 24 px below group 1 with the reference's snapping.
        let g2 = &s.groups[1];
        assert_eq!(g2.frame.top, 328);
        let g3 = &s.groups[2];
        assert_eq!(g3.rows.len(), 3);
        assert_eq!(g3.frame.bottom, g3.rows[2].bottom + 1);
        // Reference-shaped window (1198 × 818): its first two frames line up.
        let reference = Layout::new(1198, 818, 96, Page::Settings);
        let s = reference.settings();
        assert_eq!(s.groups[0].frame.top, 146);
        assert_eq!(s.groups[0].rows[0].bottom, 211);
        // Group 1 has two rows here (theme, language) where the reference has
        // one, so group 2 starts one row pitch lower; rows snap the same way.
        assert_eq!(s.groups[1].title.top, 301);
        let pitch = s.groups[1].rows[1].top - s.groups[1].rows[0].top;
        assert!((65..=66).contains(&pitch));
    }

    /// Borders are whole device pixels at any scale, and near the minimum
    /// height every settings row stays on screen (padding tightens instead).
    #[test]
    fn settings_borders_stay_crisp_and_rows_fit_the_minimum_window() {
        for (w, h, dpi) in [(1225, 1025, 120), (1800, 1230, 144), (2400, 1640, 192)] {
            let l = Layout::new(w, h, dpi, Page::Settings);
            for g in l.settings().groups {
                assert_eq!(g.rows[0].top - g.frame.top, l.hair);
                assert_eq!(g.frame.bottom - g.rows.last().unwrap().bottom, l.hair);
                for pair in g.rows.windows(2) {
                    assert_eq!(pair[1].top - pair[0].bottom, l.hair, "{dpi} DPI");
                }
                assert_eq!(g.pad_y, SETTINGS_ROW_PAD_Y);
            }
        }
        for (w, h, dpi) in [(980, 660, 96), (1470, 990, 144)] {
            let l = Layout::new(w, h, dpi, Page::Settings);
            let s = l.settings();
            let last = s.groups.last().unwrap();
            assert!(last.frame.bottom <= l.content.bottom, "{dpi} DPI");
            assert!(last.pad_y < SETTINGS_ROW_PAD_Y && last.pad_y >= 6.0);
            // Text still fits the compact rows.
            let content = last.row_content(0, dpi);
            assert!(
                content.bottom - content.top
                    >= px(dpi, SETTINGS_ROW_TITLE + SETTINGS_ROW_TEXT).round() as i32 - 1
            );
        }
    }

    /// The telemetry drawer and the service details panel split the content
    /// rect with no gap or overlap at any DPI; the list is placed from the
    /// same rectangles the panels are painted in.
    #[test]
    fn drawer_and_details_split_the_content_rect() {
        let l = Layout::new(1200, 820, 96, Page::Processes);
        assert_eq!(t(l.drawer(false).0), t(l.content));
        assert!(l.drawer(false).1.is_none());
        let (table, drawer) = l.drawer(true);
        let drawer = drawer.unwrap();
        assert_eq!(t(table), r(221, 106, 1200, 610));
        assert_eq!(t(drawer), r(221, 610, 1200, 790));
        let s = Layout::new(1200, 820, 96, Page::Services);
        let (table, panel) = s.details(true);
        assert_eq!(t(table), r(221, 106, 910, 790));
        assert_eq!(t(panel.unwrap()), r(910, 106, 1200, 790));
        // Narrow windows keep the whole table (the panel needs 1100 px).
        let narrow = Layout::new(1099, 820, 96, Page::Services);
        assert!(narrow.details(true).1.is_none());
        for (w, h, dpi) in [(1225, 1025, 120), (1800, 1000, 144), (1470, 990, 144)] {
            let l = Layout::new(w, h, dpi, Page::Processes);
            let (table, drawer) = l.drawer(true);
            let drawer = drawer.unwrap();
            assert_eq!(table.bottom, drawer.top, "{dpi}");
            assert_eq!(
                (table.top, drawer.bottom),
                (l.content.top, l.content.bottom)
            );
            assert_eq!(
                drawer.left,
                l.main.left + l.hair,
                "left border stays visible"
            );
            assert!(table.bottom - table.top >= l.px(DRAWER_MIN_TABLE));
            let (table, panel) = l.details(true);
            assert_eq!(
                panel.is_some(),
                w >= l.px(DETAILS_MIN_CLIENT),
                "{w} at {dpi}"
            );
            if let Some(panel) = panel {
                assert_eq!(table.right, panel.left);
                assert_eq!(panel.right, w);
            }
        }
        // A short window keeps 60 px of table above the drawer.
        let short = Layout::new(1200, 300, 96, Page::Processes);
        let (table, _) = short.drawer(true);
        assert_eq!(table.bottom - table.top, 60);
    }

    #[test]
    fn regions_tile_the_client_without_gaps_at_several_dpis() {
        for (w, h, dpi) in [
            (1200, 820, 96),
            (1225, 1025, 120),
            (1800, 1230, 144),
            (2400, 1640, 192),
        ] {
            for page in [Page::Processes, Page::Startup, Page::Settings] {
                let l = Layout::new(w, h, dpi, page);
                assert_eq!(l.titlebar.bottom, l.rail.top);
                assert_eq!(l.rail.bottom, l.status.top);
                assert_eq!(l.rail.right, l.main.left);
                assert_eq!(l.main.left + l.hair, l.head.left);
                assert_eq!(l.main.top + l.hair, l.head.top);
                assert_eq!(l.head.bottom, l.content.top);
                assert_eq!(l.content.bottom, l.status.top);
                assert_eq!(l.content.right, w);
                assert_eq!(l.status.bottom, h);
                assert!(l.search.left >= l.rail.right && l.search.right <= l.caption.left);
                assert!(
                    (l.search.left - l.rail.right - (l.caption.left - l.search.right)).abs() <= 1
                );
                assert!(l.nav[3].bottom < l.nav_settings.top);
            }
        }
    }
}
