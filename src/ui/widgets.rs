//! Pure painters for the design's components (DESIGN_SPEC §4). No behaviour
//! and no state: callers pass animation values (`hover: f32`, switch progress,
//! chevron angle) and flags, painters draw with tokens, fonts and DPI.
//!
//! All painters take a [`Painter`] (HDC + antialiased Canvas + DPI + fonts +
//! palette). Geometry arguments are device-pixel `RECT`s of the element box;
//! internal paddings/radii are the spec's CSS px scaled by the DPI.
//! Tracks may append new painters here (in separate functions).
#![allow(dead_code)] // Painter API consumed by the Frame/Controls/Table tracks.

use super::fonts::{self, Fonts};
use super::gfx::{hairline, px, Canvas, Path, Radii, RectF};
use super::theme::{self, argb, colors, disabled, mix, over, solid, Palette};
use std::mem::zeroed;
use windows_sys::Win32::Foundation::{RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetTextMetricsW, SelectObject, SetBkMode,
    SetTextColor, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, HDC, HFONT, TEXTMETRICW, TRANSPARENT,
};

/// Everything a painter needs. Create it *after* setting up the DC's clip and
/// origin (the Canvas captures them). Text goes through [`Painter::text`],
/// which flushes pending GDI+ output first.
pub(super) struct Painter<'a> {
    pub dc: HDC,
    pub canvas: Canvas,
    pub dpi: i32,
    pub fonts: &'a Fonts,
    pub c: Palette,
}

impl<'a> Painter<'a> {
    /// # Safety
    /// `dc` must be valid for the Painter's lifetime.
    pub(super) unsafe fn new(dc: HDC, dpi: i32, fonts: &'a Fonts) -> Self {
        Self {
            dc,
            canvas: Canvas::new(dc),
            dpi: dpi.max(48),
            fonts,
            c: colors(),
        }
    }
    /// Paint with an explicit palette (previews, popups of another theme).
    pub(super) fn with_palette(mut self, palette: Palette) -> Self {
        self.c = palette;
        self
    }
    /// DIP → device px.
    pub(super) fn px(&self, dip: f32) -> f32 {
        px(self.dpi, dip)
    }
    /// DIP → whole device px (rounded).
    pub(super) fn pxi(&self, dip: f32) -> i32 {
        self.px(dip).round() as i32
    }
    /// A crisp 1 CSS px border width.
    pub(super) fn hair(&self) -> f32 {
        hairline(self.dpi)
    }
    /// Opaque GDI fill (crisp; cheapest for axis-aligned rectangles).
    pub(super) fn fill(&self, r: RECT, color: u32) {
        if r.right <= r.left || r.bottom <= r.top {
            return;
        }
        self.canvas.flush();
        unsafe {
            let brush = CreateSolidBrush(color);
            FillRect(self.dc, &r, brush);
            DeleteObject(brush);
        }
    }
    /// GDI ClearType text. `flags` are DrawText flags (DT_NOPREFIX is added).
    /// Hangul in non-Malgun fonts is drawn with the same-size Malgun Gothic
    /// fallback (`fonts::draw_text`), like the reference's font fallback.
    pub(super) fn text(&self, font: HFONT, color: u32, value: &str, r: RECT, flags: u32) {
        if value.is_empty() {
            return;
        }
        self.canvas.flush();
        unsafe {
            let old = SelectObject(self.dc, font);
            SetTextColor(self.dc, color);
            SetBkMode(self.dc, TRANSPARENT as i32);
            let text: Vec<u16> = value.encode_utf16().collect();
            let mut r = r;
            fonts::draw_text(self.dc, &text, &mut r, DT_NOPREFIX | flags);
            SelectObject(self.dc, old);
        }
    }
    /// The DrawText rectangle (DT_TOP, full GDI cell) that puts `font`'s
    /// baseline where the reference's CSS puts it for a `line_height` CSS px
    /// line box (None = `normal`) centred in `band` — see
    /// `fonts::css_line_rect`. Draw into it with [`Painter::text`] and
    /// `DT_SINGLELINE` (no DT_VCENTER): nothing is clipped, and mono text no
    /// longer sits 1–2 px low.
    pub(super) fn css_rect(&self, font: HFONT, band: RECT, line_height: Option<f32>) -> RECT {
        unsafe {
            let old = SelectObject(self.dc, font);
            let r = fonts::css_line_rect(self.dc, band, line_height.map(|v| self.px(v)));
            SelectObject(self.dc, old);
            r
        }
    }
    /// The baseline (device px) of [`Painter::css_rect`].
    pub(super) fn css_baseline(&self, font: HFONT, band: RECT, line_height: Option<f32>) -> i32 {
        unsafe {
            let old = SelectObject(self.dc, font);
            let y = fonts::css_baseline(self.dc, band, line_height.map(|v| self.px(v)));
            SelectObject(self.dc, old);
            y
        }
    }
    /// [`Painter::css_baseline`] of a band at a fractional device-px `top`
    /// and `height` (`fonts::css_baseline_at`).
    pub(super) fn css_baseline_at(
        &self,
        font: HFONT,
        top: f32,
        height: f32,
        line_height: Option<f32>,
    ) -> i32 {
        unsafe {
            let old = SelectObject(self.dc, font);
            let y = fonts::css_baseline_at(self.dc, top, height, line_height.map(|v| self.px(v)));
            SelectObject(self.dc, old);
            y
        }
    }
    /// The baseline of one `line_height` CSS px line box with text in several
    /// `fonts` (`fonts::css_mixed_baseline`), centred in `band`.
    pub(super) fn css_mixed_baseline(&self, band: RECT, fonts: &[HFONT], line_height: f32) -> i32 {
        unsafe { fonts::css_mixed_baseline(self.dc, band, fonts, self.px(line_height)) }
    }
    /// A single-line label whose baseline is `baseline` (device px), e.g. a
    /// note continuing another font's text on the same line.
    pub(super) fn text_on_baseline(
        &self,
        font: HFONT,
        color: u32,
        value: &str,
        r: RECT,
        baseline: i32,
        align: u32,
    ) {
        let (ascent, height) = unsafe {
            let old = SelectObject(self.dc, font);
            let mut metrics: TEXTMETRICW = zeroed();
            GetTextMetricsW(self.dc, &mut metrics);
            SelectObject(self.dc, old);
            (metrics.tmAscent, metrics.tmHeight)
        };
        let top = baseline - ascent;
        self.text(
            font,
            color,
            value,
            RECT {
                top,
                bottom: top + height,
                ..r
            },
            DT_SINGLELINE | DT_END_ELLIPSIS | align,
        );
    }
    /// Single line, vertically centred, ellipsis; `align` = DT_LEFT/CENTER/RIGHT.
    pub(super) fn label(&self, font: HFONT, color: u32, value: &str, r: RECT, align: u32) {
        self.text(
            font,
            color,
            value,
            r,
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | align,
        );
    }
    /// Text extent in device px (Hangul measured in its fallback face, as
    /// [`Painter::text`] draws it).
    pub(super) fn measure(&self, font: HFONT, value: &str) -> SIZE {
        let size: SIZE = unsafe { zeroed() };
        if value.is_empty() {
            return size;
        }
        unsafe {
            let old = SelectObject(self.dc, font);
            let size = fonts::str_extent(self.dc, value);
            SelectObject(self.dc, old);
            size
        }
    }
    /// `text-overflow: ellipsis`: `value` when it fits in `width` px, else its
    /// longest prefix that fits with one "…" (U+2026, one glyph like
    /// Chromium's, not GDI's three periods), trailing spaces trimmed.
    pub(super) fn ellipsize(&self, font: HFONT, value: &str, width: i32) -> String {
        ellipsize_with(value, width, |s| self.measure(font, s).cx)
    }
}

/// `cursor: pointer` (the reference's `button`, `.palette li`) or the default
/// arrow (`tbody tr`, `.btn:disabled`): set it and report WM_SETCURSOR handled.
pub(super) fn set_pointer(hand: bool) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        LoadCursorW, SetCursor, IDC_ARROW, IDC_HAND,
    };
    unsafe {
        SetCursor(LoadCursorW(
            std::ptr::null_mut(),
            if hand { IDC_HAND } else { IDC_ARROW },
        ));
    }
    1
}

/// [`Painter::ellipsize`] with a measuring function (pure; tests).
pub(super) fn ellipsize_with(value: &str, width: i32, measure: impl Fn(&str) -> i32) -> String {
    const ELLIPSIS: &str = "\u{2026}";
    if value.is_empty() || measure(value) <= width {
        return value.to_owned();
    }
    let room = width - measure(ELLIPSIS);
    if room <= 0 {
        return if width >= measure(ELLIPSIS) {
            ELLIPSIS.into()
        } else {
            String::new()
        };
    }
    // The longest char prefix that fits (widths only grow with the prefix).
    let bounds: Vec<usize> = value
        .char_indices()
        .map(|(i, _)| i)
        .skip(1)
        .chain(Some(value.len()))
        .collect();
    let (mut lo, mut hi) = (0usize, bounds.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&value[..bounds[mid - 1]]) <= room {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let prefix = if lo == 0 {
        ""
    } else {
        value[..bounds[lo - 1]].trim_end()
    };
    format!("{prefix}{ELLIPSIS}")
}

fn inset(r: RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: r.left + dx,
        top: r.top + dy,
        right: r.right - dx,
        bottom: r.bottom - dy,
    }
}

// ───────────────────────────── focus ring ─────────────────────────────

/// `:focus-visible`: 2 px solid fg, 2 px outside the element, radius + 2.
/// Needs 4 px of room around `element` (it is drawn outside it).
pub(super) fn focus_ring(pt: &Painter, element: RECT, radius: f32) {
    let gap = pt.px(2.0);
    let width = pt.px(2.0).round().max(1.0);
    let outer = RectF::from_rect(element).inset(-(gap + width));
    pt.canvas
        .stroke_round_rect(outer, radius + gap + width, width, solid(pt.c.fg));
}

/// Focus ring drawn *inside* `element` (for child windows with no margin).
pub(super) fn focus_ring_inset(pt: &Painter, element: RECT, radius: f32) {
    let width = pt.px(2.0).round().max(1.0);
    pt.canvas
        .stroke_round_rect(RectF::from_rect(element), radius, width, solid(pt.c.fg));
}

// ───────────────────────────── buttons ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ButtonStyle {
    /// `.btn`: surface, 1 px border, hover btn_hover_surface.
    Default,
    /// `.btn-primary`: btn_bg / btn_hover, btn_fg 600.
    Primary,
    /// `.btn-danger`: danger / danger_hover, on_danger 600.
    Danger,
    /// Nav rail item on bg: hover fg_soft_bg, current fg_sel_bg (see [`nav_item`]).
    Nav,
    /// Borderless rail action on bg (like Nav, never "current").
    Rail,
    /// Borderless on any surface: hover fg @ 5 % over the parent.
    Ghost,
    /// 32×32 icon-only `.btn` face (draw the icon into the returned rect).
    Icon,
}

/// Visual state. `hover` is the animated 0..=1 hover amount.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct ButtonState {
    pub hover: f32,
    pub pressed: bool,
    /// Keyboard focus (`:focus-visible`; hide it when ODS_NOFOCUSRECT).
    pub focused: bool,
    pub disabled: bool,
    /// Nav current / toggled on / segmented pressed.
    pub selected: bool,
}

/// Face colors (background, border, text) of a button style in a state.
pub(super) fn button_colors(
    c: &Palette,
    style: ButtonStyle,
    st: &ButtonState,
    parent_bg: u32,
) -> (u32, u32, u32) {
    let hover = if st.disabled {
        0.0
    } else {
        st.hover.clamp(0.0, 1.0)
    };
    let (bg, border, fg) = match style {
        ButtonStyle::Default | ButtonStyle::Icon => {
            let bg = if st.selected {
                c.fg_sel
            } else {
                mix(c.surface, c.btn_hover_surface, hover)
            };
            (bg, c.border, c.fg)
        }
        ButtonStyle::Primary => {
            let bg = mix(c.btn_bg, c.btn_hover, hover);
            (bg, bg, c.btn_fg)
        }
        ButtonStyle::Danger => {
            let bg = mix(c.danger, c.danger_hover, hover);
            (bg, bg, c.on_danger)
        }
        ButtonStyle::Nav | ButtonStyle::Rail => {
            let bg = if st.selected && style == ButtonStyle::Nav {
                c.fg_sel_bg
            } else {
                mix(parent_bg, over(c.fg, 0.05, parent_bg), hover)
            };
            (bg, bg, c.fg)
        }
        ButtonStyle::Ghost => {
            let bg = mix(parent_bg, over(c.fg, 0.05, parent_bg), hover);
            (bg, bg, c.fg)
        }
    };
    if st.disabled {
        (
            disabled(bg, parent_bg),
            disabled(border, parent_bg),
            disabled(fg, parent_bg),
        )
    } else {
        (bg, border, fg)
    }
}

/// Paint a button face (+ centred/left label) and return the content rect
/// (inside the 14 px padding, shifted 1 px down while pressed) for icons.
/// `parent_bg` is what shows around the rounded corners.
pub(super) fn button_face(
    pt: &Painter,
    r: RECT,
    style: ButtonStyle,
    st: &ButtonState,
    text: &str,
    parent_bg: u32,
) -> RECT {
    let (bg, border, fg) = button_colors(&pt.c, style, st, parent_bg);
    pt.fill(r, parent_bg);
    let down = if st.pressed && !st.disabled {
        pt.pxi(1.0).max(1)
    } else {
        0
    };
    // "Active: content and face translate down 1 px" — the whole face. In a
    // child window its bottom row falls outside the window: the parent
    // paints it (`controls::paint_pressed_rows`).
    let face = RectF::from_rect(RECT {
        top: r.top + down,
        bottom: r.bottom + down,
        ..r
    });
    let radius = pt.px(theme::RADIUS_SM);
    let bordered = matches!(
        style,
        ButtonStyle::Default | ButtonStyle::Icon | ButtonStyle::Primary | ButtonStyle::Danger
    );
    pt.canvas.bordered_round_rect(
        face,
        Radii::all(radius),
        if bordered { pt.hair() } else { 0.0 },
        solid(bg),
        solid(border),
    );
    let padding = match style {
        ButtonStyle::Nav | ButtonStyle::Rail => pt.pxi(12.0),
        ButtonStyle::Icon => 0,
        _ => pt.pxi(14.0),
    };
    let content = RECT {
        left: r.left + padding,
        top: r.top + down,
        right: r.right - padding,
        bottom: r.bottom + down,
    };
    let (font, align) = match style {
        ButtonStyle::Primary | ButtonStyle::Danger => (pt.fonts.ui_strong, DT_CENTER),
        ButtonStyle::Nav | ButtonStyle::Rail => (
            if st.selected {
                pt.fonts.body_strong
            } else {
                pt.fonts.body
            },
            DT_LEFT,
        ),
        _ => (pt.fonts.ui, DT_CENTER),
    };
    if !text.is_empty() {
        if st.disabled {
            // `.45` of a label drawn in full fg: the full color's weight.
            let (_, _, full) = button_colors(
                &pt.c,
                style,
                &ButtonState {
                    disabled: false,
                    ..*st
                },
                parent_bg,
            );
            fonts::with_lift_of(full, || pt.label(font, fg, text, content, align));
        } else {
            pt.label(font, fg, text, content, align);
        }
    }
    if st.focused && !st.disabled {
        focus_ring_inset(pt, r, radius);
    }
    content
}

/// Nav rail item: 40 px row, 16 px icon at x + 12, label at x + 40 (14 px,
/// 600 when current), optional right-aligned count (11 px mono muted) and the
/// 3×16 current indicator. `icon` draws the glyph at (x, y, size, color).
pub(super) fn nav_item(
    pt: &Painter,
    r: RECT,
    st: &ButtonState,
    label: &str,
    count: Option<&str>,
    parent_bg: u32,
    icon: impl FnOnce(&Painter, f32, f32, f32, u32),
) {
    nav_item_with(
        pt,
        r,
        st,
        if st.selected { 1.0 } else { 0.0 },
        true,
        label,
        count,
        parent_bg,
        icon,
    );
}

/// [`nav_item`] for an animated rail: the background cross-fades from the
/// hover face to fg_sel_bg with `selected_t` (0..=1, already eased) and the
/// built-in indicator is drawn only when `indicator` is true — a sliding
/// indicator is painted separately with [`nav_indicator`] (see
/// `paint::nav_indicator`). The label weight follows `st.selected` at once
/// (CSS does not transition font-weight).
#[allow(clippy::too_many_arguments)]
pub(super) fn nav_item_with(
    pt: &Painter,
    r: RECT,
    st: &ButtonState,
    selected_t: f32,
    indicator: bool,
    label: &str,
    count: Option<&str>,
    parent_bg: u32,
    icon: impl FnOnce(&Painter, f32, f32, f32, u32),
) {
    let (_, _, fg) = button_colors(&pt.c, ButtonStyle::Nav, st, parent_bg);
    let rest = ButtonState {
        selected: false,
        ..*st
    };
    let (hover_bg, _, _) = button_colors(&pt.c, ButtonStyle::Nav, &rest, parent_bg);
    let (current_bg, _, _) = button_colors(
        &pt.c,
        ButtonStyle::Nav,
        &ButtonState {
            selected: true,
            ..*st
        },
        parent_bg,
    );
    let bg = mix(hover_bg, current_bg, selected_t.clamp(0.0, 1.0));
    pt.fill(r, parent_bg);
    // No pressed style: the reference only translates `.btn`s.
    pt.canvas
        .fill_round_rect(RectF::from_rect(r), pt.px(theme::RADIUS_SM), solid(bg));
    if st.focused && !st.disabled {
        focus_ring_inset(pt, r, pt.px(theme::RADIUS_SM));
    }
    if st.selected && indicator {
        nav_indicator(
            pt,
            r.left as f32,
            r.top as f32 + pt.px(12.0),
            (r.bottom - r.top) as f32 - pt.px(24.0),
            1.0,
        );
    }
    let size = pt.px(16.0).round();
    let x = r.left as f32 + pt.px(12.0);
    let y = ((r.top + r.bottom) as f32 - size) / 2.0;
    icon(pt, x, y.round(), size, fg);
    let mut text = RECT {
        left: r.left + pt.pxi(40.0),
        right: r.right - pt.pxi(12.0),
        ..r
    };
    if let Some(count) = count {
        let width = pt.measure(pt.fonts.mono_tiny, count).cx;
        pt.label(pt.fonts.mono_tiny, pt.c.muted, count, text, DT_RIGHT);
        text.right -= width + pt.pxi(8.0);
    }
    pt.label(
        if st.selected {
            pt.fonts.body_strong
        } else {
            pt.fonts.body
        },
        fg,
        label,
        text,
        DT_LEFT,
    );
}

/// The nav "current" bar: 3 px wide, radius 2, fg, at `x` (item left) from
/// `y` for `height` px. `alpha` lets a sliding indicator fade.
pub(super) fn nav_indicator(pt: &Painter, x: f32, y: f32, height: f32, alpha: f32) {
    let width = pt.px(3.0).round().max(2.0);
    pt.canvas.fill_round_rect(
        RectF::new(x, y, width, height),
        pt.px(2.0),
        argb(pt.c.fg, alpha),
    );
}

// ───────────────────────────── switch / segmented / select ─────────────────────────────

/// 40×20 switch in `r` (the switch box). `t` = checked progress 0..=1 (already
/// eased); the knob slides 20 px with it. Without the target state the
/// colors follow the nearer end (see [`switch_to`]).
pub(super) fn switch(pt: &Painter, r: RECT, t: f32, enabled: bool, focused: bool, parent_bg: u32) {
    switch_to(pt, r, t, t >= 0.5, enabled, focused, parent_bg);
}

/// [`switch`] sliding toward `on`: like the reference (`.switch` has no
/// color transition, only `::after { transition: transform .12s ease }`)
/// the fill, border and knob take `on`'s colors at once and only the knob
/// moves — a cross-fade made knob and track meet in luminance mid-slide.
pub(super) fn switch_to(
    pt: &Painter,
    r: RECT,
    t: f32,
    on: bool,
    enabled: bool,
    focused: bool,
    parent_bg: u32,
) {
    let c = &pt.c;
    let t = t.clamp(0.0, 1.0);
    let state = on as u8 as f32;
    let mut border = mix(c.muted, c.fg, state);
    let mut fill = over(c.fg, state, parent_bg);
    let mut knob = mix(c.muted, c.surface, state);
    if !enabled {
        border = disabled(border, parent_bg);
        fill = disabled(fill, parent_bg);
        knob = disabled(knob, parent_bg);
    }
    let bounds = RectF::from_rect(r);
    let radius = bounds.h / 2.0;
    pt.canvas.bordered_round_rect(
        bounds,
        Radii::all(radius),
        pt.hair(),
        solid(fill),
        solid(border),
    );
    let unit = bounds.w / 40.0;
    pt.canvas.fill_circle(
        bounds.x + unit * (10.0 + 20.0 * t),
        bounds.cy(),
        unit * 6.0,
        solid(knob),
    );
    if focused && enabled {
        focus_ring(pt, r, radius);
    }
}

/// The 40×20 switch box for a DPI, right-aligned in `cell` with `right_pad`
/// DIP and vertically centred.
pub(super) fn switch_rect(pt: &Painter, cell: RECT, right_pad: f32) -> RECT {
    let w = pt.pxi(40.0);
    let h = pt.pxi(20.0);
    let right = cell.right - pt.pxi(right_pad);
    let top = (cell.top + cell.bottom - h) / 2;
    RECT {
        left: right - w,
        top,
        right,
        bottom: top + h,
    }
}

/// One segment of a `.seg` group whose segments are separate rectangles laid
/// edge to edge. `first`/`last` pick the rounded outer corners; every segment
/// but the first draws the 1 px separator on its left.
#[allow(clippy::too_many_arguments)]
pub(super) fn segment(
    pt: &Painter,
    r: RECT,
    first: bool,
    last: bool,
    selected: bool,
    hover: f32,
    text: &str,
    focused: bool,
    parent_bg: u32,
) {
    let c = &pt.c;
    let radius = pt.px(theme::RADIUS_SM);
    let radii = Radii::new(
        if first { radius } else { 0.0 },
        if last { radius } else { 0.0 },
        if last { radius } else { 0.0 },
        if first { radius } else { 0.0 },
    );
    pt.fill(r, parent_bg);
    let bounds = RectF::from_rect(r);
    let hair = pt.hair();
    pt.canvas
        .fill_round_rect_corners(bounds, radii, solid(c.border));
    let inner = RectF::ltrb(
        bounds.x + hair,
        bounds.y + hair,
        bounds.right() - if last { hair } else { 0.0 },
        bounds.bottom() - hair,
    );
    let bg = if selected {
        c.fg
    } else {
        mix(c.surface, c.btn_hover_surface, hover.clamp(0.0, 1.0))
    };
    pt.canvas
        .fill_round_rect_corners(inner, radii.shrink(hair), solid(bg));
    // `.seg button { padding: 6px 14px }`: a 13 px × 1.45 line box 6 px
    // below the control's 1 px border.
    let line = 13.0 * 1.45;
    let baseline = pt.css_baseline_at(
        pt.fonts.ui,
        r.top as f32 + hair + pt.px(6.0),
        pt.px(line),
        Some(line),
    );
    pt.text_on_baseline(
        pt.fonts.ui,
        if selected { c.surface } else { c.fg },
        text,
        r,
        baseline,
        DT_CENTER,
    );
    if focused {
        focus_ring_inset(pt, r, if first || last { radius } else { 0.0 });
    }
}

/// A whole segmented control laid out from its labels (padding 6 14). Returns
/// each segment's rectangle for hit testing. `hovers[i]` is segment i's hover.
pub(super) fn segmented(
    pt: &Painter,
    origin: (i32, i32),
    labels: &[&str],
    selected: usize,
    hovers: &[f32],
    focused: Option<usize>,
    parent_bg: u32,
) -> Vec<RECT> {
    let height = segmented_height(pt);
    let mut x = origin.0;
    let mut rects = Vec::with_capacity(labels.len());
    for (i, label) in labels.iter().enumerate() {
        let width = pt.measure(pt.fonts.ui, label).cx + pt.pxi(28.0) + pt.hair() as i32;
        let r = RECT {
            left: x,
            top: origin.1,
            right: x + width,
            bottom: origin.1 + height,
        };
        segment(
            pt,
            r,
            i == 0,
            i + 1 == labels.len(),
            i == selected,
            hovers.get(i).copied().unwrap_or(0.0),
            label,
            focused == Some(i),
            parent_bg,
        );
        rects.push(r);
        x = r.right;
    }
    rects
}

/// Height of a segmented control: 1 + 6 + line + 6 + 1.
pub(super) fn segmented_height(pt: &Painter) -> i32 {
    let line = pt.measure(pt.fonts.ui, "Ag").cy;
    line + pt.pxi(12.0) + 2 * pt.hair() as i32
}

/// Field state for selects, search boxes and other inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct FieldState {
    pub hover: f32,
    pub focused: bool,
    pub disabled: bool,
    /// Dropdown currently open.
    pub open: bool,
}

/// `.select` face: 28 px (22 in the status bar) — pass the box as `r` — 1 px
/// border radius 4, surface, 12 px text with 6 px padding, chevron-down on
/// the right. `font` defaults to the 12 px text role when None (the status bar
/// passes its mono font like the reference's inherited family).
pub(super) fn select_face(
    pt: &Painter,
    r: RECT,
    text: &str,
    st: &FieldState,
    font: Option<HFONT>,
    parent_bg: u32,
) {
    select_face_with(pt, r, text, st, font, None, parent_bg);
}

/// [`select_face`] with an inherited text color (`select { color: inherit }`):
/// the status bar's Refresh select draws its value and arrow in `muted`.
pub(super) fn select_face_with(
    pt: &Painter,
    r: RECT,
    text: &str,
    st: &FieldState,
    font: Option<HFONT>,
    color: Option<u32>,
    parent_bg: u32,
) {
    let c = &pt.c;
    let (mut bg, mut border, mut fg) = (c.surface, c.border, color.unwrap_or(c.fg));
    if st.disabled {
        bg = disabled(bg, parent_bg);
        border = disabled(border, parent_bg);
        fg = disabled(fg, parent_bg);
    }
    pt.fill(r, parent_bg);
    let radius = pt.px(theme::RADIUS_SM);
    pt.canvas.bordered_round_rect(
        RectF::from_rect(r),
        Radii::all(radius),
        pt.hair(),
        solid(bg),
        solid(border),
    );
    // Measured on the reference (Chromium's menulist inside `padding: 0 6px`):
    // text 11 px from the outer left edge, the arrow centred 9 px from the
    // right edge.
    let arrow = pt.px(9.0);
    let text_rect = RECT {
        left: r.left + pt.pxi(11.0),
        right: r.right - pt.pxi(20.0),
        ..r
    };
    let font = font.unwrap_or(pt.fonts.small);
    if font == pt.fonts.small {
        pt.label(font, fg, text, text_rect, DT_LEFT);
    } else {
        // A mono value (the status bar's `select { font: inherit }`, line
        // height normal) centred like Chromium's menulist text: DT_VCENTER
        // put Cascadia Mono's taller GDI cell 1 px low.
        let cell = pt.css_rect(font, text_rect, None);
        pt.text(
            font,
            fg,
            text,
            RECT {
                left: text_rect.left,
                right: text_rect.right,
                ..cell
            },
            DT_SINGLELINE | DT_END_ELLIPSIS | DT_LEFT,
        );
    }
    let cx = r.right as f32 - arrow;
    let cy = (r.top + r.bottom) as f32 / 2.0;
    // Chromium's native select arrow: ~9×4.5 px, ~1.8 px stroke.
    pt.canvas.chevron(
        cx,
        cy,
        pt.px(11.0),
        if st.open { -90.0 } else { 90.0 },
        pt.px(1.75),
        solid(fg),
    );
    if st.focused && !st.disabled {
        focus_ring_inset(pt, r, radius);
    }
}

// ───────────────────────────── small parts ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PillKind {
    /// Muted text, border.
    Default,
    /// fg text (e.g. Running, Efficiency).
    Ok,
    /// warn_fg text and warn_border.
    Warn,
}

/// Width of a pill for `text` (+ optional 11 px leaf icon with 5 px gap).
pub(super) fn pill_width(pt: &Painter, text: &str, leaf: bool) -> i32 {
    let mut width = pt.measure(pt.fonts.pill, text).cx + pt.pxi(16.0) + 2 * pt.hair() as i32;
    if leaf {
        width += pt.pxi(16.0);
    }
    width
}

/// `.pill` at `x`, vertically centred on `cy`. Returns its width.
pub(super) fn pill(pt: &Painter, x: i32, cy: i32, kind: PillKind, text: &str, leaf: bool) -> i32 {
    let c = &pt.c;
    let width = pill_width(pt, text, leaf);
    let height = pt.pxi(20.0);
    let top = cy - height / 2;
    let (fg, border) = match kind {
        PillKind::Default => (c.muted, c.border),
        PillKind::Ok => (c.fg, c.border),
        PillKind::Warn => (c.warn_fg, c.warn_border),
    };
    let r = RectF::new(x as f32, top as f32, width as f32, height as f32);
    pt.canvas
        .stroke_round_rect(r, r.h / 2.0, pt.hair(), solid(border));
    let mut text_left = x + pt.pxi(8.0) + pt.hair() as i32;
    if leaf {
        let size = pt.px(11.0).round();
        icon(
            pt,
            Icon::Leaf,
            text_left as f32,
            (cy as f32 - size / 2.0).round(),
            size,
            fg,
        );
        text_left += pt.pxi(16.0);
    }
    pt.label(
        pt.fonts.pill,
        fg,
        text,
        RECT {
            left: text_left,
            top,
            right: x + width,
            bottom: top + height,
        },
        DT_LEFT,
    );
    width
}

/// Size of a `.kbd` badge: 11 px mono, padding 1 5, 1 px border. Like the
/// reference's inline box: the height is the font's CSS content area
/// (ascent + descent, 13 px at 96 DPI — GDI's cell is 15) and the width uses
/// the font's fractional advances (GDI rounds each Cascadia advance down).
pub(super) fn kbd_size(pt: &Painter, text: &str) -> SIZE {
    let hair = pt.hair() as i32;
    let (width, content) = unsafe {
        let old = SelectObject(pt.dc, pt.fonts.mono_tiny);
        let (ascent, descent) = fonts::css_metrics(pt.dc);
        let width = fonts::ideal_width(pt.dc, text);
        SelectObject(pt.dc, old);
        (width, ascent + descent)
    };
    SIZE {
        cx: width.ceil() as i32 + 2 * pt.pxi(5.0) + 2 * hair,
        cy: content + 2 * pt.pxi(1.0) + 2 * hair,
    }
}

/// `.kbd` badge whose right edge is `right`, vertically centred on `cy`.
/// Returns its rectangle.
pub(super) fn kbd(pt: &Painter, right: i32, cy: i32, text: &str) -> RECT {
    let size = kbd_size(pt, text);
    kbd_at(pt, right, cy - size.cy / 2, text)
}

/// `.kbd` badge with its right edge at `right` and its top at `top` (the
/// search box places it `top: 7px`). Returns its rectangle.
pub(super) fn kbd_at(pt: &Painter, right: i32, top: i32, text: &str) -> RECT {
    let size = kbd_size(pt, text);
    let r = RECT {
        left: right - size.cx,
        top,
        right,
        bottom: top + size.cy,
    };
    pt.canvas.stroke_round_rect(
        RectF::from_rect(r),
        pt.px(theme::RADIUS_XS),
        pt.hair(),
        solid(pt.c.border),
    );
    let ascent = unsafe {
        let old = SelectObject(pt.dc, pt.fonts.mono_tiny);
        let (ascent, _) = fonts::css_metrics(pt.dc);
        SelectObject(pt.dc, old);
        ascent
    };
    let baseline = top + pt.hair() as i32 + pt.pxi(1.0) + ascent;
    pt.text_on_baseline(pt.fonts.mono_tiny, pt.c.muted, text, r, baseline, DT_CENTER);
    r
}

/// Mono-ico initials: first letters of the first two words of the display
/// name after stripping ".exe" and non-alphanumerics (ASCII, like the
/// reference), upper-cased; "·" when nothing remains.
pub(super) fn initials(name: &str) -> String {
    let trimmed = name.trim();
    let cut = trimmed.len().saturating_sub(4);
    let base = match (trimmed.get(cut..), trimmed.get(..cut)) {
        (Some(suffix), Some(stem)) if suffix.eq_ignore_ascii_case(".exe") => stem,
        _ => trimmed,
    };
    let cleaned: String = base
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == ' ')
        .collect();
    let letters: String = cleaned
        .split(' ')
        .filter(|word| !word.is_empty())
        .take(2)
        .filter_map(|word| word.chars().next())
        .collect::<String>()
        .to_ascii_uppercase();
    if letters.is_empty() {
        "·".into()
    } else {
        letters
    }
}

/// `.mono-ico`: 20×20 (pass the box), radius 4, mono_ico_bg, 10 px 600 mono
/// initials; child rows: transparent with a 1 px border.
pub(super) fn mono_badge(pt: &Painter, r: RECT, initials: &str, child: bool) {
    let c = &pt.c;
    let bounds = RectF::from_rect(r);
    let radius = pt.px(theme::RADIUS_SM);
    if child {
        pt.canvas
            .stroke_round_rect(bounds, radius, pt.hair(), solid(c.border));
    } else {
        pt.canvas
            .fill_round_rect(bounds, radius, solid(c.mono_ico_bg));
    }
    // `display: grid; place-items: center` of a 10 px mono line box: the
    // CSS baseline (DT_VCENTER centred Cascadia's taller cell, 1 px low).
    let cell = pt.css_rect(pt.fonts.mono_badge, r, None);
    pt.text(
        pt.fonts.mono_badge,
        c.fg,
        initials,
        RECT {
            left: r.left,
            right: r.right,
            ..cell
        },
        DT_SINGLELINE | DT_CENTER,
    );
}

/// `.chev` toggle: 20×20 box `r`, hover fg_sel (as fg @ 9 % overlay, radius 3),
/// 10 px chevron stroke 1.5 rotated `angle_deg` (0 → right, 90 → down).
pub(super) fn chevron_button(pt: &Painter, r: RECT, angle_deg: f32, hover: f32) {
    let bounds = RectF::from_rect(r);
    if hover > 0.0 {
        pt.canvas.fill_round_rect(
            bounds,
            pt.px(theme::RADIUS_XS),
            argb(pt.c.fg, 0.09 * hover.clamp(0.0, 1.0)),
        );
    }
    pt.canvas.chevron(
        bounds.cx(),
        bounds.cy(),
        pt.px(10.0),
        angle_deg,
        pt.px(1.5),
        solid(pt.c.fg),
    );
}

/// 6 px status dot centred on `(cx, cy)`.
pub(super) fn status_dot(pt: &Painter, cx: f32, cy: f32, color: u32) {
    pt.canvas.fill_circle(cx, cy, pt.px(3.0), solid(color));
}

// ───────────────────────────── caption buttons ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Caption {
    Minimize,
    Maximize,
    Restore,
    Close,
}

/// 10×10 caption glyph centred in `r`, rasterised like the reference's
/// SVGs: 1 CSS px strokes scaled with the DPI (1.5 px at 150 %), so at 100 %
/// the minimize bar straddles two half rows and the close cross is two
/// crisp staircases, exactly like the browser renders them.
pub(super) fn caption_glyph(pt: &Painter, r: RECT, kind: Caption, color: u32) {
    let size = pt.px(10.0).round();
    let stroke = pt.px(1.0);
    let x = ((r.left + r.right) as f32 - size) / 2.0;
    let y = ((r.top + r.bottom) as f32 - size) / 2.0;
    let (x, y) = (x.round(), y.round());
    let ink = solid(color);
    let canvas = &pt.canvas;
    match kind {
        // `M0 5h10`: centred on the box's middle.
        Caption::Minimize => {
            canvas.fill_rect(
                RectF::new(x, y + size / 2.0 - stroke / 2.0, size, stroke),
                ink,
            );
        }
        // `<rect x=.5 y=.5 width=9 height=9 rx=1>`: an inside stroke of the box.
        // At a 1 px stroke: four crisp edges and the same softened corner
        // at all four corners (Chromium's 31 % corner pixel, 77 % beside
        // it) — GDI+'s tiny arcs antialiased each corner differently.
        Caption::Maximize if stroke == 1.0 => {
            let n = size;
            canvas.fill_rect(RectF::new(x + 2.0, y, n - 4.0, 1.0), ink);
            canvas.fill_rect(RectF::new(x + 2.0, y + n - 1.0, n - 4.0, 1.0), ink);
            canvas.fill_rect(RectF::new(x, y + 2.0, 1.0, n - 4.0), ink);
            canvas.fill_rect(RectF::new(x + n - 1.0, y + 2.0, 1.0, n - 4.0), ink);
            let soft = argb(color, 0.77);
            let corner = argb(color, 0.31);
            for (cx, cy, dx, dy) in [
                (x, y, 1.0, 1.0),
                (x + n - 1.0, y, -1.0, 1.0),
                (x, y + n - 1.0, 1.0, -1.0),
                (x + n - 1.0, y + n - 1.0, -1.0, -1.0),
            ] {
                canvas.fill_rect(RectF::new(cx, cy, 1.0, 1.0), corner);
                canvas.fill_rect(RectF::new(cx + dx, cy, 1.0, 1.0), soft);
                canvas.fill_rect(RectF::new(cx, cy + dy, 1.0, 1.0), soft);
            }
        }
        Caption::Maximize => {
            canvas.stroke_round_rect(RectF::new(x, y, size, size), pt.px(1.0), stroke, ink);
        }
        // Not in the reference: the maximize square in front, another behind.
        Caption::Restore => {
            let front = (size * 0.8).round();
            let offset = size - front;
            canvas.stroke_round_rect(
                RectF::new(x, y + offset, front, front),
                pt.px(1.0),
                stroke,
                ink,
            );
            let h = stroke / 2.0;
            let mut back = Path::new();
            back.lines(&[
                (x + offset + h, y + offset),
                (x + offset + h, y + h),
                (x + size - h, y + h),
                (x + size - h, y + front - h),
                (x + front, y + front - h),
            ]);
            canvas.stroke_path(&back, stroke, ink);
        }
        // `M0 0l10 10M10 0 0 10`: at whole-pixel strokes two crisp
        // staircases corner to corner (the reference's pixels at 100 %),
        // otherwise antialiased diagonals of the scaled stroke.
        Caption::Close => {
            if stroke.fract() == 0.0 {
                let steps = (size - stroke).max(0.0) as i32;
                for i in 0..=steps {
                    let d = i as f32;
                    canvas.fill_rect(RectF::new(x + d, y + d, stroke, stroke), ink);
                    canvas.fill_rect(
                        RectF::new(x + size - stroke - d, y + d, stroke, stroke),
                        ink,
                    );
                }
            } else {
                canvas.line(x, y, x + size, y + size, stroke, ink);
                canvas.line(x + size, y, x, y + size, stroke, ink);
            }
        }
    }
}

/// 46×44 caption button: hover fg_sel_bg (close: danger + on_danger glyph),
/// pressed slightly stronger; glyph muted when the window is inactive.
#[allow(clippy::too_many_arguments)]
pub(super) fn caption_button(
    pt: &Painter,
    r: RECT,
    kind: Caption,
    hover: f32,
    pressed: bool,
    active: bool,
    parent_bg: u32,
) {
    let c = &pt.c;
    let hover = hover.clamp(0.0, 1.0);
    let (bg, glyph) = if kind == Caption::Close {
        let hot = if pressed {
            mix(c.danger, c.danger_hover, 0.35)
        } else {
            c.danger
        };
        let rest = if active { c.fg } else { c.muted };
        (
            mix(parent_bg, hot, if pressed { 1.0 } else { hover }),
            mix(rest, c.on_danger, if pressed { 1.0 } else { hover }),
        )
    } else {
        let hot = if pressed {
            over(c.fg, 0.14, parent_bg)
        } else {
            c.fg_sel_bg
        };
        (
            mix(parent_bg, hot, if pressed { 1.0 } else { hover }),
            if active { c.fg } else { c.muted },
        )
    };
    pt.fill(r, bg);
    caption_glyph(pt, r, kind, glyph);
}

// ───────────────────────────── search box ─────────────────────────────

/// Title-bar search face: surface, 1 px border (fg when focused), radius 4,
/// 14 px magnifier at x + 11, optional placeholder at x + 34 (14 px muted) and
/// a `Ctrl K` kbd badge 8 px from the right. Returns the text area (where an
/// edit control belongs): left + 34 .. right − 64 (− 8 without the badge).
pub(super) fn search_box(
    pt: &Painter,
    r: RECT,
    st: &FieldState,
    placeholder: Option<&str>,
    kbd_text: Option<&str>,
    parent_bg: u32,
) -> RECT {
    let c = &pt.c;
    pt.fill(r, parent_bg);
    pt.canvas.bordered_round_rect(
        RectF::from_rect(r),
        Radii::all(pt.px(theme::RADIUS_SM)),
        pt.hair(),
        solid(c.surface),
        solid(if st.focused && !st.disabled {
            c.fg
        } else {
            c.border
        }),
    );
    let size = pt.px(14.0).round();
    let cy = (r.top + r.bottom) as f32 / 2.0;
    icon(
        pt,
        Icon::Search,
        r.left as f32 + pt.px(11.0),
        (cy - size / 2.0).round(),
        size,
        c.muted,
    );
    let hair = pt.hair() as i32;
    let text = RECT {
        left: r.left + hair + pt.pxi(34.0),
        right: r.right
            - hair
            - if kbd_text.is_some() {
                pt.pxi(64.0)
            } else {
                pt.pxi(8.0)
            },
        ..r
    };
    if let Some(placeholder) = placeholder {
        pt.label(pt.fonts.body, c.muted, placeholder, text, DT_LEFT);
    }
    if let Some(value) = kbd_text {
        // `.search .kbd { right: 8px; top: 7px }`
        kbd_at(pt, r.right - pt.pxi(8.0), r.top + pt.pxi(7.0), value);
    }
    text
}

// ───────────────────────────── tables ─────────────────────────────

/// Heat alpha of the reference: `min(1, v / threshold) · 0.70`, dropped below
/// 4 %, rounded to whole percent like `toFixed(0)`.
pub(super) fn heat_alpha(value: f64, threshold: f64) -> f32 {
    if !value.is_finite() || !threshold.is_finite() || threshold <= 0.0 {
        return 0.0;
    }
    let percent = (value / threshold).clamp(0.0, 1.0) * 70.0;
    if percent < 4.0 {
        0.0
    } else {
        (percent.round() / 100.0) as f32
    }
}

/// Design heat thresholds: CPU 20 %, working set 2400 MB, All I/O 3 MB/s.
pub(super) const HEAT_CPU_PERCENT: f64 = 20.0;
pub(super) const HEAT_MEMORY_BYTES: f64 = 2400.0 * 1_048_576.0;
pub(super) const HEAT_IO_BYTES_PER_SEC: f64 = 3.0 * 1_048_576.0;
/// 8 Mbps (decimal megabits, as the Network column shows) in bytes/s.
pub(super) const HEAT_NETWORK_BYTES_PER_SEC: f64 = 8.0 * 1e6 / 8.0;
pub(super) const HEAT_GPU_PERCENT: f64 = 12.0;

/// Fill a cell with the heat color at `alpha` over the row background and
/// return the resulting color (so hover/selection stay visible under heat).
pub(super) fn heat_cell_bg(pt: &Painter, r: RECT, row_bg: u32, alpha: f32) -> u32 {
    let color = if alpha > 0.0 {
        pt.c.heat_over(alpha, row_bg)
    } else {
        row_bg
    };
    pt.fill(r, color);
    color
}

/// Table row background: selected fg_sel, else hover-faded fg_soft over surface.
pub(super) fn row_background(c: &Palette, hover: f32, selected: bool) -> u32 {
    if selected {
        c.fg_sel
    } else {
        mix(c.surface, c.fg_soft, hover.clamp(0.0, 1.0))
    }
}

/// Group row text (Apps / Background processes …): 13/600 fg with padding-top
/// 8 and padding-left 12, then " (n)" muted 13 px mono on the same baseline.
/// `r` is the whole 38 px row.
pub(super) fn group_row(pt: &Painter, r: RECT, title: &str, count: Option<&str>) {
    let text = RECT {
        left: r.left + pt.pxi(12.0),
        top: r.top + pt.pxi(8.0),
        right: r.right - pt.pxi(12.0),
        bottom: r.bottom,
    };
    // One line box (13 px × the body's 1.45) holding the title and the mono
    // count, centred in the cell below its 8 px top padding.
    let f = pt.fonts;
    let both = [f.ui_strong, f.mono_ui];
    let fonts = if count.is_some() {
        &both[..]
    } else {
        &both[..1]
    };
    let baseline = pt.css_mixed_baseline(text, fonts, 13.0 * 1.45);
    pt.text_on_baseline(f.ui_strong, pt.c.fg, title, text, baseline, DT_LEFT);
    if let Some(count) = count {
        let width = pt.measure(pt.fonts.ui_strong, title).cx;
        let left = text.left + width + pt.pxi(4.0);
        if left < text.right {
            // The count shares the title's baseline (one inline line box).
            pt.text_on_baseline(
                pt.fonts.mono_ui,
                pt.c.muted,
                count,
                RECT { left, ..text },
                baseline,
                DT_LEFT,
            );
        }
    }
}

/// Split "Apps (8)" into ("Apps", Some("(8)")) for [`group_row`].
pub(super) fn split_group_title(value: &str) -> (&str, Option<&str>) {
    match value.rfind(" (") {
        Some(at) if value.ends_with(')') => (&value[..at], Some(&value[at + 1..])),
        _ => (value, None),
    }
}

/// Sticky table header cell: padding 8 12, optional `.tot` line (15/600 mono
/// fg) above the `.lab` (12 px muted, " ↓"/" ↑" when sorted), bottom aligned;
/// `two_line` keeps a blank total line for label alignment. Hover fg_soft.
#[allow(clippy::too_many_arguments)]
pub(super) fn header_cell(
    pt: &Painter,
    r: RECT,
    total: Option<&str>,
    label: &str,
    two_line: bool,
    right_align: bool,
    sort_descending: Option<bool>,
    hover: f32,
) {
    let c = &pt.c;
    pt.fill(r, mix(c.surface, c.fg_soft, hover.clamp(0.0, 1.0)));
    let hair = pt.hair() as i32;
    pt.fill(
        RECT {
            top: r.bottom - hair,
            ..r
        },
        c.border,
    );
    let align = if right_align { DT_RIGHT } else { DT_LEFT };
    let inner = RECT {
        left: r.left + pt.pxi(12.0),
        right: r.right - pt.pxi(12.0),
        top: r.top + pt.pxi(8.0),
        bottom: r.bottom - hair - pt.pxi(8.0),
    };
    let label_height = (pt.px(12.0) * 1.45).round() as i32;
    let total_height = (pt.px(15.0) * 1.2).round() as i32;
    let mut text = label.to_owned();
    if let Some(descending) = sort_descending {
        text.push_str(if descending { " ↓" } else { " ↑" });
    }
    if !two_line {
        pt.label(pt.fonts.small, c.muted, &text, inner, align);
        return;
    }
    let label_rect = RECT {
        top: inner.bottom - label_height,
        ..inner
    };
    pt.label(pt.fonts.small, c.muted, &text, label_rect, align);
    if let Some(total) = total {
        let total_rect = RECT {
            top: label_rect.top - total_height,
            bottom: label_rect.top,
            ..inner
        };
        // Fall back to the 13 px strong face when a long total does not fit.
        let font = if pt.measure(pt.fonts.mono_total, total).cx > total_rect.right - total_rect.left
        {
            pt.fonts.ui_strong
        } else {
            pt.fonts.mono_total
        };
        header_total(pt, font, total, total_rect, align);
    }
}

/// A header `.tot` in its line box (`r`: 15 px × 1.2 above the label) on
/// the CSS baseline — DT_VCENTER centred Cascadia Mono's taller GDI cell
/// and put the totals 1–3 px low.
pub(super) fn header_total(pt: &Painter, font: HFONT, total: &str, r: RECT, align: u32) {
    let cell = pt.css_rect(font, r, Some(15.0 * 1.2));
    pt.text(
        font,
        pt.c.fg,
        total,
        RECT {
            left: r.left,
            right: r.right,
            ..cell
        },
        DT_SINGLELINE | DT_END_ELLIPSIS | align,
    );
}

/// Minimum overlay scrollbar thumb length (CSS px).
pub(super) const SCROLL_THUMB_MIN: f32 = 24.0;

/// Overlay scrollbar thumb geometry `(top, length)` relative to the lane's
/// top, for a `view` px viewport over `content` px scrolled by `offset`, in a
/// lane `lane_len` px long: proportional length, at least `min_len` (pass
/// `pt.px(SCROLL_THUMB_MIN)`), positioned over the *remaining* travel so the
/// minimum never pushes it past the lane's end. Use the same function for
/// painting and for hit testing / dragging ([`scroll_offset_for_thumb`]).
/// Without anything to scroll it returns the whole lane (hide the bar).
pub(super) fn scroll_thumb(
    lane_len: f32,
    view: f32,
    content: f32,
    offset: f32,
    min_len: f32,
) -> (f32, f32) {
    let lane_len = lane_len.max(0.0);
    if view <= 0.0 || content <= view {
        return (0.0, lane_len);
    }
    let length = (lane_len * view / content).max(min_len).min(lane_len);
    let progress = (offset / (content - view)).clamp(0.0, 1.0);
    (progress * (lane_len - length), length)
}

/// The scroll offset that puts the thumb's top at `thumb_top` (lane px) —
/// the inverse of [`scroll_thumb`] for dragging.
pub(super) fn scroll_offset_for_thumb(
    lane_len: f32,
    view: f32,
    content: f32,
    thumb_top: f32,
    min_len: f32,
) -> f32 {
    let (_, length) = scroll_thumb(lane_len, view, content, 0.0, min_len);
    let travel = lane_len - length;
    if travel <= 0.0 || content <= view {
        return 0.0;
    }
    (thumb_top / travel).clamp(0.0, 1.0) * (content - view)
}

/// Overlay scrollbar (Fluent style) drawn over content in `lane` (the right
/// 14 px hover zone of the scroll view). `thumb` is the thumb's (top, length)
/// in device px inside the lane (use [`scroll_thumb`]); it is kept at least
/// 24 px long and clamped inside the lane. `grow` animates 0 → 1 from the
/// idle 3 px thumb to the 8 px hover thumb with a faint track; `opacity`
/// fades it all.
pub(super) fn scrollbar(pt: &Painter, lane: RECT, thumb: (f32, f32), grow: f32, opacity: f32) {
    let c = &pt.c;
    let (grow, opacity) = (grow.clamp(0.0, 1.0), opacity.clamp(0.0, 1.0));
    if opacity <= 0.0 {
        return;
    }
    let inset = pt.px(2.0);
    let width = pt.px(3.0) + (pt.px(8.0) - pt.px(3.0)) * grow;
    let right = lane.right as f32 - inset;
    if grow > 0.0 {
        let track = RectF::ltrb(
            right - pt.px(8.0),
            lane.top as f32 + inset,
            right,
            lane.bottom as f32 - inset,
        );
        pt.canvas
            .fill_round_rect(track, track.w / 2.0, argb(c.fg, 0.05 * grow * opacity));
    }
    let lane_len = (lane.bottom - lane.top).max(0) as f32;
    let length = thumb.1.max(pt.px(SCROLL_THUMB_MIN)).min(lane_len);
    let top = thumb.0.clamp(0.0, (lane_len - length).max(0.0));
    let bar = RectF::new(right - width, lane.top as f32 + top, width, length);
    pt.canvas.fill_round_rect(
        bar,
        width / 2.0,
        argb(c.muted, (0.5 + 0.2 * grow) * opacity),
    );
}

// ───────────────────────────── icons ─────────────────────────────

/// The reference's SVG icons, drawn as antialiased vectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Icon {
    Processes,
    Performance,
    Startup,
    Services,
    Settings,
    /// Brand feather (24-unit stroked SVG).
    Feather,
    Search,
    /// Efficiency leaf (12-unit).
    Leaf,
    /// Three horizontal dots (icon-only "more" button).
    More,
    Plus,
}

impl Icon {
    /// The nav icon for a page index (0..=3 pages, 4 settings).
    pub(super) fn nav(index: usize) -> Icon {
        match index {
            0 => Icon::Processes,
            1 => Icon::Performance,
            2 => Icon::Startup,
            3 => Icon::Services,
            _ => Icon::Settings,
        }
    }
}

/// Draw `kind` in the `size`×`size` device-px box at `(x, y)`.
pub(super) fn icon(pt: &Painter, kind: Icon, x: f32, y: f32, size: f32, color: u32) {
    let canvas = &pt.canvas;
    let ink = solid(color);
    // Map SVG user units of a `view`-unit viewBox into the box.
    let (view, stroke) = match kind {
        Icon::Feather => (24.0, 1.6),
        Icon::Leaf => (12.0, 1.3),
        Icon::Search => (16.0, 1.5),
        _ => (16.0, 1.4),
    };
    let k = size / view;
    let p = |ux: f32, uy: f32| (x + ux * k, y + uy * k);
    let width = stroke * k;
    match kind {
        Icon::Processes => {
            for uy in [3.5, 8.0, 12.5] {
                let (a, b) = (p(2.0, uy), p(14.0, uy));
                canvas.line(a.0, a.1, b.0, b.1, width, ink);
            }
        }
        Icon::Performance => {
            let mut path = Path::new();
            path.lines(&[
                p(1.0, 9.0),
                p(4.0, 9.0),
                p(6.0, 4.0),
                p(9.0, 13.0),
                p(11.0, 7.0),
                p(12.5, 9.0),
                p(15.0, 9.0),
            ]);
            canvas.stroke_path_round(&path, width, ink);
        }
        Icon::Startup => {
            let (a, b) = (p(8.0, 1.5), p(8.0, 7.5));
            canvas.line_round(a.0, a.1, b.0, b.1, width, ink);
            // a5 5 0 1 0 7 0 from (4.5, 4): centre (8, 7.571), large arc through the bottom.
            let centre = p(8.0, 4.0 + (25.0f32 - 12.25).sqrt());
            let radius = 5.0 * k;
            let mut path = Path::new();
            path.arc(
                RectF::new(
                    centre.0 - radius,
                    centre.1 - radius,
                    radius * 2.0,
                    radius * 2.0,
                ),
                225.57,
                -271.14,
            );
            canvas.stroke_path_round(&path, width, ink);
        }
        Icon::Services => {
            for (ux, uy) in [(2.0, 2.0), (9.0, 2.0), (2.0, 9.0), (9.0, 9.0)] {
                let (x0, y0) = p(ux, uy);
                canvas.stroke_round_rect(
                    RectF::new(
                        x0 - width / 2.0,
                        y0 - width / 2.0,
                        5.0 * k + width,
                        5.0 * k + width,
                    ),
                    k + width / 2.0,
                    width,
                    ink,
                );
            }
        }
        Icon::Settings => {
            let c = p(8.0, 8.0);
            canvas.stroke_circle(c.0, c.1, 2.2 * k, width, ink);
            for (x1, y1, x2, y2) in [
                (8.0, 1.5, 8.0, 3.5),
                (8.0, 12.5, 8.0, 14.5),
                (1.5, 8.0, 3.5, 8.0),
                (12.5, 8.0, 14.5, 8.0),
                (3.4, 3.4, 4.8, 4.8),
                (11.2, 11.2, 12.6, 12.6),
                (3.4, 12.6, 4.8, 11.2),
                (11.2, 4.8, 12.6, 3.4),
            ] {
                let (a, b) = (p(x1, y1), p(x2, y2));
                canvas.line(a.0, a.1, b.0, b.1, width, ink);
            }
        }
        Icon::Feather => {
            let mut path = Path::new();
            // M20 4C12 4 6 9 5 19
            path.start_figure()
                .bezier(p(20.0, 4.0), p(12.0, 4.0), p(6.0, 9.0), p(5.0, 19.0));
            // M20 4c0 7-4 12-11 12
            path.start_figure()
                .bezier(p(20.0, 4.0), p(20.0, 11.0), p(16.0, 16.0), p(9.0, 16.0));
            // M9 16l-5 4
            path.start_figure();
            let (a, b) = (p(9.0, 16.0), p(4.0, 20.0));
            path.line(a.0, a.1, b.0, b.1);
            // M11 11h5
            path.start_figure();
            let (a, b) = (p(11.0, 11.0), p(16.0, 11.0));
            path.line(a.0, a.1, b.0, b.1);
            canvas.stroke_path_round(&path, width, ink);
        }
        Icon::Search => {
            let c = p(7.0, 7.0);
            canvas.stroke_circle(c.0, c.1, 4.5 * k, width, ink);
            let (a, b) = (p(10.5, 10.5), p(14.0, 14.0));
            canvas.line(a.0, a.1, b.0, b.1, width, ink);
        }
        Icon::Leaf => {
            // M2 10C2 5 5 2 10 2c0 5-3 8-8 8Z  and  M2 10l5-5
            let mut path = Path::new();
            path.start_figure()
                .bezier(p(2.0, 10.0), p(2.0, 5.0), p(5.0, 2.0), p(10.0, 2.0))
                .bezier(p(10.0, 2.0), p(10.0, 7.0), p(7.0, 10.0), p(2.0, 10.0))
                .close_figure();
            path.start_figure();
            let (a, b) = (p(2.0, 10.0), p(7.0, 5.0));
            path.line(a.0, a.1, b.0, b.1);
            canvas.stroke_path_round(&path, width, ink);
        }
        Icon::More => {
            let cy = y + size / 2.0;
            let gap = size * 0.3;
            for i in -1..=1 {
                canvas.fill_circle(x + size / 2.0 + gap * i as f32, cy, size * 0.09, ink);
            }
        }
        Icon::Plus => {
            let (a, b) = (p(8.0, 2.5), p(8.0, 13.5));
            canvas.line(a.0, a.1, b.0, b.1, width, ink);
            let (a, b) = (p(2.5, 8.0), p(13.5, 8.0));
            canvas.line(a.0, a.1, b.0, b.1, width, ink);
        }
    }
}

/// The radiation trefoil of the "Nuclear Zombie" action, filled in `color`
/// inside the `size` square at (x, y): a centre dot and three 60° blades
/// pointing bottom, top-left and top-right (the symbol's orientation). The
/// gaps are widened from the standard 1 : 1.5 : 5 radii so they stay open at
/// 14 px.
pub(super) fn radiation(pt: &Painter, x: f32, y: f32, size: f32, color: u32) {
    let (cx, cy) = (x + size / 2.0, y + size / 2.0);
    let outer = size / 2.0;
    let inner = size * 0.21;
    let ink = solid(color);
    pt.canvas.fill_circle(cx, cy, size * 0.12, ink);
    let square = |r: f32| RectF::new(cx - r, cy - r, 2.0 * r, 2.0 * r);
    // Screen angles, clockwise from +x: 90° points down.
    for centre in [90.0f32, 210.0, 330.0] {
        let mut blade = Path::new();
        blade
            .start_figure()
            .arc(square(outer), centre - 30.0, 60.0)
            .arc(square(inner), centre + 30.0, -60.0)
            .close_figure();
        pt.canvas.fill_path(&blade, ink);
    }
}

// ───────────────────────────── preview gallery ─────────────────────────────

/// Gallery size in DIP (see [`gallery`]).
pub(super) const GALLERY_DIP: (i32, i32) = (1120, 900);

/// Every painter in its states on one canvas (DIP layout scaled to the
/// Painter's DPI) for `render_previews` → `components-*.png`.
pub(super) fn gallery(pt: &Painter) {
    let c = pt.c;
    let d = |v: i32| pt.pxi(v as f32);
    let r = |x: i32, y: i32, w: i32, h: i32| RECT {
        left: d(x),
        top: d(y),
        right: d(x + w),
        bottom: d(y + h),
    };
    pt.fill(r(0, 0, GALLERY_DIP.0, GALLERY_DIP.1), c.bg);
    // Main-panel card behind the surface components.
    pt.canvas.bordered_round_rect(
        RectF::from_rect(r(236, 8, 876, 884)),
        Radii::new(pt.px(theme::RADIUS), 0.0, 0.0, 0.0),
        pt.hair(),
        solid(c.surface),
        solid(c.border),
    );
    let caption = |x: i32, y: i32, text: &str| {
        pt.label(pt.fonts.mono_tiny, c.muted, text, r(x, y, 300, 16), DT_LEFT);
    };
    let rest = ButtonState::default();

    // Rail (bg): nav items, rail action.
    caption(12, 12, "nav · rest / hover .5 / hover 1 / current");
    for (i, (hover, selected)) in [(0.0, false), (0.5, false), (1.0, false), (0.0, true)]
        .into_iter()
        .enumerate()
    {
        let glyph = Icon::nav(i);
        nav_item(
            pt,
            r(8, 32 + i as i32 * 42, 204, 40),
            &ButtonState {
                hover,
                selected,
                ..rest
            },
            ["Processes", "Performance", "Startup apps", "Services"][i],
            (i == 0 || selected).then_some("30"),
            c.bg,
            |pt, x, y, s, color| icon(pt, glyph, x, y, s, color),
        );
    }
    nav_item(
        pt,
        r(8, 204, 204, 40),
        &ButtonState {
            focused: true,
            ..rest
        },
        "Settings (focused)",
        None,
        c.bg,
        |pt, x, y, s, color| icon(pt, Icon::Settings, x, y, s, color),
    );
    focus_ring_inset(pt, r(8, 204, 204, 40), pt.px(theme::RADIUS_SM));
    caption(12, 256, "status dots · live / paused");
    status_dot(pt, pt.px(20.0), pt.px(282.0), c.fg);
    status_dot(pt, pt.px(36.0), pt.px(282.0), c.warn_fg);
    caption(12, 300, "select 22 (status bar, mono)");
    select_face(
        pt,
        r(12, 320, 70, 22),
        "1 s",
        &FieldState::default(),
        Some(pt.fonts.mono_small),
        c.bg,
    );
    caption(12, 356, "caption · rest/hover/pressed/inactive");
    for (row, kind) in [
        Caption::Minimize,
        Caption::Maximize,
        Caption::Restore,
        Caption::Close,
    ]
    .into_iter()
    .enumerate()
    {
        for (col, (hover, pressed, active)) in [
            (0.0, false, true),
            (1.0, false, true),
            (1.0, true, true),
            (0.0, false, false),
        ]
        .into_iter()
        .enumerate()
        {
            caption_button(
                pt,
                r(12 + col as i32 * 50, 376 + row as i32 * 48, 46, 44),
                kind,
                hover,
                pressed,
                active,
                c.bg,
            );
        }
    }
    caption(12, 572, "brand + icons 16 / 24");
    icon(
        pt,
        Icon::Feather,
        pt.px(12.0),
        pt.px(592.0),
        pt.px(18.0),
        c.fg,
    );
    pt.label(
        pt.fonts.ui,
        c.fg,
        "Feather Task Manager",
        r(40, 590, 180, 22),
        DT_LEFT,
    );
    let kinds = [
        Icon::Processes,
        Icon::Performance,
        Icon::Startup,
        Icon::Services,
        Icon::Settings,
        Icon::Search,
        Icon::Leaf,
        Icon::More,
        Icon::Plus,
    ];
    for (i, kind) in kinds.into_iter().enumerate() {
        icon(
            pt,
            kind,
            pt.px(12.0 + i as f32 * 22.0),
            pt.px(624.0),
            pt.px(16.0),
            c.fg,
        );
        icon(
            pt,
            kind,
            pt.px(12.0 + i as f32 * 22.0),
            pt.px(648.0),
            pt.px(20.0),
            c.muted,
        );
    }
    caption(12, 690, "search · empty / focused / disabled");
    search_box(
        pt,
        r(12, 710, 212, 32),
        &FieldState::default(),
        Some("Search by name, file or PID"),
        Some("Ctrl K"),
        c.bg,
    );
    search_box(
        pt,
        r(12, 750, 212, 32),
        &FieldState {
            focused: true,
            ..Default::default()
        },
        None,
        Some("Ctrl K"),
        c.bg,
    );
    search_box(
        pt,
        r(12, 790, 212, 32),
        &FieldState {
            disabled: true,
            ..Default::default()
        },
        Some("Search isn't available here"),
        None,
        c.bg,
    );

    // Main panel (surface): buttons.
    let x0 = 256;
    caption(
        x0,
        20,
        "button · rest / hover / pressed / focused / disabled",
    );
    for (row, style) in [
        ButtonStyle::Default,
        ButtonStyle::Primary,
        ButtonStyle::Danger,
    ]
    .into_iter()
    .enumerate()
    {
        let label = ["Efficiency mode", "End task", "End task"][row];
        for (col, st) in [
            rest,
            ButtonState { hover: 1.0, ..rest },
            ButtonState {
                hover: 1.0,
                pressed: true,
                ..rest
            },
            ButtonState {
                focused: true,
                ..rest
            },
            ButtonState {
                disabled: true,
                ..rest
            },
        ]
        .into_iter()
        .enumerate()
        {
            button_face(
                pt,
                r(x0 + col as i32 * 132, 40 + row as i32 * 42, 124, 32),
                style,
                &st,
                label,
                c.surface,
            );
        }
    }
    caption(x0, 172, "icon / ghost / toggled");
    let more = button_face(
        pt,
        r(x0, 192, 32, 32),
        ButtonStyle::Icon,
        &rest,
        "",
        c.surface,
    );
    icon(
        pt,
        Icon::More,
        ((more.left + more.right) as f32 - pt.px(16.0)) / 2.0,
        ((more.top + more.bottom) as f32 - pt.px(16.0)) / 2.0,
        pt.px(16.0),
        c.fg,
    );
    button_face(
        pt,
        r(x0 + 40, 192, 110, 32),
        ButtonStyle::Ghost,
        &ButtonState { hover: 1.0, ..rest },
        "Ghost hover",
        c.surface,
    );
    button_face(
        pt,
        r(x0 + 158, 192, 110, 32),
        ButtonStyle::Default,
        &ButtonState {
            selected: true,
            ..rest
        },
        "Logical CPUs",
        c.surface,
    );
    caption(x0 + 300, 172, "focus ring (outside, 2 px gap)");
    button_face(
        pt,
        r(x0 + 304, 192, 100, 32),
        ButtonStyle::Default,
        &rest,
        "Cancel",
        c.surface,
    );
    focus_ring(pt, r(x0 + 304, 192, 100, 32), pt.px(theme::RADIUS_SM));

    // Switch frames: the knob and colors at eased progress.
    caption(
        x0,
        244,
        "switch · t = 0 / .25 / .5 / .75 / 1 · disabled off/on · focused",
    );
    for (i, t) in [0.0f32, 0.25, 0.5, 0.75, 1.0].into_iter().enumerate() {
        switch(
            pt,
            r(x0 + i as i32 * 56, 266, 40, 20),
            t,
            true,
            false,
            c.surface,
        );
    }
    switch(pt, r(x0 + 290, 266, 40, 20), 0.0, false, false, c.surface);
    switch(pt, r(x0 + 346, 266, 40, 20), 1.0, false, false, c.surface);
    switch(pt, r(x0 + 406, 266, 40, 20), 1.0, true, true, c.surface);

    caption(x0, 304, "segmented · selected / hover");
    segmented(
        pt,
        (d(x0), d(324)),
        &["Light", "Dark", "System"],
        0,
        &[0.0, 0.0, 1.0],
        None,
        c.surface,
    );
    caption(
        x0 + 260,
        304,
        "select 28 · rest / focused / open / disabled",
    );
    for (i, st) in [
        FieldState::default(),
        FieldState {
            focused: true,
            ..Default::default()
        },
        FieldState {
            open: true,
            ..Default::default()
        },
        FieldState {
            disabled: true,
            ..Default::default()
        },
    ]
    .into_iter()
    .enumerate()
    {
        select_face(
            pt,
            r(x0 + 264 + i as i32 * 112, 324, 104, 28),
            ["Grouped", "Tree", "Flat", "All services"][i],
            &st,
            None,
            c.surface,
        );
    }

    caption(
        x0,
        372,
        "pill · default / ok+leaf / warn · kbd · mono-ico · chevron 0/45/90/hover",
    );
    let mut x = d(x0);
    for (kind, text, leaf) in [
        (PillKind::Default, "Stopped", false),
        (PillKind::Ok, "Efficiency", true),
        (PillKind::Ok, "Running", false),
        (PillKind::Warn, "High", false),
    ] {
        x += pill(pt, x, d(404), kind, text, leaf) + d(8);
    }
    let kbd_rect = kbd(pt, x + d(60), d(404), "Ctrl K");
    x = kbd_rect.right + d(16);
    mono_badge(
        pt,
        RECT {
            left: x,
            top: d(394),
            right: x + d(20),
            bottom: d(414),
        },
        &initials("Google Chrome"),
        false,
    );
    mono_badge(
        pt,
        RECT {
            left: x + d(28),
            top: d(394),
            right: x + d(48),
            bottom: d(414),
        },
        &initials("Network Service"),
        true,
    );
    for (i, (angle, hover)) in [(0.0, 0.0), (45.0, 0.0), (90.0, 0.0), (0.0, 1.0)]
        .into_iter()
        .enumerate()
    {
        let left = x + d(64 + i as i32 * 26);
        chevron_button(
            pt,
            RECT {
                left,
                top: d(394),
                right: left + d(20),
                bottom: d(414),
            },
            angle,
            hover,
        );
    }

    // Table fragments.
    caption(
        x0,
        432,
        "table header (two-line, sorted, hover) · group row · rows with heat",
    );
    let widths = [300, 120, 112, 112, 100];
    let heads = [
        (None, "Name", false, None, 0.0),
        (None, "Status", false, None, 1.0),
        (Some("30.1%"), "CPU", true, Some(true), 0.0),
        (Some("38.0%"), "Memory", true, None, 0.0),
        (Some("42.1%"), "Disk", true, None, 0.0),
    ];
    let mut left = x0;
    for (i, (total, label, right, sort, hover)) in heads.into_iter().enumerate() {
        header_cell(
            pt,
            r(left, 452, widths[i], 51),
            total,
            label,
            true,
            right,
            sort,
            hover,
        );
        left += widths[i];
    }
    group_row(pt, r(x0, 503, 744, 38), "Apps", Some("(8)"));
    let rows = [
        ("Google Chrome", 0.0, false, [6.8, 1410.4, 0.4]),
        ("Visual Studio Code", 1.0, false, [5.0, 1316.2, 0.1]),
        ("Figma", 0.0, true, [3.8, 768.0, 3.1]),
        ("Feather Task Manager", 0.0, false, [0.3, 18.9, 0.0]),
    ];
    for (i, (name, hover, selected, values)) in rows.into_iter().enumerate() {
        let top = 541 + i as i32 * 34;
        let bg = row_background(&c, hover, selected);
        pt.fill(r(x0, top, 744, 33), bg);
        pt.fill(r(x0, top + 33, 744, 1), c.row_border);
        let chevron = i < 2;
        if chevron {
            chevron_button(
                pt,
                r(x0 + 12, top + 7, 20, 20),
                if i == 1 { 90.0 } else { 0.0 },
                0.0,
            );
        }
        mono_badge(pt, r(x0 + 40, top + 7, 20, 20), &initials(name), false);
        pt.label(pt.fonts.body, c.fg, name, r(x0 + 68, top, 220, 33), DT_LEFT);
        let alphas = [
            heat_alpha(values[0], HEAT_CPU_PERCENT),
            heat_alpha(values[1] * 1_048_576.0, HEAT_MEMORY_BYTES),
            heat_alpha(values[2] * 1_048_576.0, HEAT_IO_BYTES_PER_SEC),
        ];
        let texts = [
            format!("{:.1}%", values[0]),
            format!("{:.1} MB", values[1]),
            format!("{:.1} MB/s", values[2]),
        ];
        let mut cell_left = x0 + 420;
        for (k, width) in [112, 112, 100].into_iter().enumerate() {
            let cell = r(cell_left, top, width, 33);
            heat_cell_bg(pt, cell, bg, alphas[k]);
            pt.label(
                pt.fonts.mono_cell,
                c.fg,
                &texts[k],
                RECT {
                    right: cell.right - d(12),
                    ..cell
                },
                DT_RIGHT,
            );
            cell_left += width;
        }
    }

    // Overlay scrollbar: idle, half-grown, hovered.
    for (i, grow) in [0.0f32, 0.5, 1.0].into_iter().enumerate() {
        scrollbar(
            pt,
            r(x0 + 760 + i as i32 * 20, 452, 14, 226),
            (pt.px(60.0), pt.px(80.0)),
            grow,
            1.0,
        );
    }

    // Motion: the two spec easings, sampled like the animator does.
    caption(x0, 690, "easing · ease (fg) / ease-out (accent) over 0..1");
    let plot = RectF::from_rect(r(x0, 710, 300, 160));
    pt.canvas.bordered_round_rect(
        plot,
        Radii::all(pt.px(4.0)),
        pt.hair(),
        solid(c.bg),
        solid(c.border),
    );
    for (easing, color) in [
        (super::anim::Easing::Ease, c.fg),
        (super::anim::Easing::EaseOut, c.accent),
    ] {
        let points: Vec<(f32, f32)> = (0..=60)
            .map(|i| {
                let t = i as f32 / 60.0;
                (
                    plot.x + plot.w * t,
                    plot.bottom() - plot.h * easing.apply(t),
                )
            })
            .collect();
        pt.canvas.polyline(&points, pt.px(1.5), solid(color));
    }
    caption(x0 + 330, 690, "palette · type roles");
    let mut y = 712;
    for (font, sample) in [
        (pt.fonts.h1, "Processes 20/600"),
        (pt.fonts.group_title, "Appearance 15/600"),
        (pt.fonts.body, "Body 14 · Search by name"),
        (pt.fonts.ui, "UI 13 · Changes apply next time"),
        (pt.fonts.small, "Small 12 · explorer.exe"),
        (pt.fonts.mono_cell, "1,410.4 MB  0.4 MB/s"),
        (pt.fonts.mono_total, "30.1%"),
    ] {
        pt.label(font, c.fg, sample, r(x0 + 330, y, 400, 24), DT_LEFT);
        y += 24;
    }
}

#[cfg(test)]
mod tests {
    use super::super::gfx::{live_objects, Dib};
    use super::super::theme::{hex, DARK_PALETTE, LIGHT};
    use super::*;
    use crate::i18n::Language;

    #[test]
    fn ellipsis_is_one_glyph_after_the_longest_fitting_prefix() {
        // Every char 7 px wide, "…" too (a mono cell).
        let measure = |s: &str| 7 * s.chars().count() as i32;
        assert_eq!(ellipsize_with("Defender", 56, measure), "Defender");
        assert_eq!(
            ellipsize_with("AMD Crash Defender", 70, measure),
            "AMD Crash\u{2026}"
        );
        // A trailing space before the cut is trimmed, like Chromium.
        assert_eq!(
            ellipsize_with("AMD Crash Defender", 77, measure),
            "AMD Crash\u{2026}"
        );
        assert_eq!(ellipsize_with("abc", 7, measure), "\u{2026}");
        assert_eq!(ellipsize_with("abc", 3, measure), "");
        assert_eq!(ellipsize_with("", 0, measure), "");
    }

    #[test]
    fn initials_follow_the_reference_rules() {
        for (name, expected) in [
            ("Google Chrome", "GC"),
            ("Visual Studio Code", "VS"),
            ("svchost.exe", "S"),
            ("SVCHOST.EXE", "S"),
            ("Vmmem (WSL)", "VW"),
            ("Logi Options+", "LO"),
            ("Feather Task Manager", "FT"),
            ("7-Zip.exe", "7"),
            ("테스트.exe", "·"),
            ("", "·"),
            ("  .exe", "·"),
            ("테스트", "·"),
            ("Chrome 테스트.exe", "C"),
            ("a b c", "AB"),
        ] {
            assert_eq!(initials(name), expected, "{name:?}");
        }
    }

    /// Thumb geometry keeps the 24 px minimum inside the lane at both ends,
    /// dragging inverts it exactly, and the painter clamps raw input.
    #[test]
    fn scroll_thumb_keeps_the_minimum_inside_the_lane() {
        // 100 px lane, content 100 × the view: the thumb is 24 px, not 1.
        let (top, len) = scroll_thumb(100.0, 10.0, 1000.0, 990.0, 24.0);
        assert_eq!((top, len), (76.0, 24.0));
        assert_eq!(scroll_thumb(100.0, 10.0, 1000.0, 0.0, 24.0), (0.0, 24.0));
        assert_eq!(scroll_thumb(100.0, 10.0, 1000.0, -50.0, 24.0).0, 0.0);
        assert_eq!(scroll_thumb(100.0, 10.0, 1000.0, 5000.0, 24.0).0, 76.0);
        // Proportional when long enough; nothing to scroll = whole lane.
        assert_eq!(scroll_thumb(200.0, 50.0, 100.0, 25.0, 24.0), (50.0, 100.0));
        assert_eq!(scroll_thumb(200.0, 100.0, 80.0, 0.0, 24.0), (0.0, 200.0));
        for offset in [0.0, 123.0, 495.0, 990.0] {
            let (top, _) = scroll_thumb(100.0, 10.0, 1000.0, offset, 24.0);
            let back = scroll_offset_for_thumb(100.0, 10.0, 1000.0, top, 24.0);
            assert!((back - offset).abs() < 1e-3, "{offset} -> {back}");
        }
        assert_eq!(
            scroll_offset_for_thumb(100.0, 10.0, 1000.0, 500.0, 24.0),
            990.0
        );
        assert_eq!(scroll_offset_for_thumb(100.0, 100.0, 80.0, 10.0, 24.0), 0.0);
        unsafe {
            let fonts = Fonts::new(96, Language::English);
            let mut dib = Dib::new(40, 200).unwrap();
            dib.pixels().fill(0xffff_ffff);
            {
                let pt = Painter::new(dib.dc(), 96, &fonts).with_palette(LIGHT);
                let lane = RECT {
                    left: 0,
                    top: 0,
                    right: 40,
                    bottom: 100,
                };
                // Raw (proportional) input near the end: clamped, not 22 px out.
                scrollbar(&pt, lane, (99.0, 1.0), 1.0, 1.0);
            }
            let w = dib.width();
            let pixels = dib.pixels().to_vec();
            let inked =
                |y: i32| (0..w).any(|x| pixels[(y * w + x) as usize] & 0xff_ffff != 0xff_ffff);
            assert!(inked(90), "the thumb sits at the lane end");
            assert!((100..200).all(|y| !inked(y)), "nothing below the lane");
        }
    }

    #[test]
    fn heat_alpha_matches_the_reference_formula() {
        assert_eq!(heat_alpha(0.0, 20.0), 0.0);
        assert_eq!(heat_alpha(1.0, 20.0), 0.0); // 3.5 % < 4 %
        assert_eq!(heat_alpha(2.0, 20.0), 0.07);
        assert_eq!(heat_alpha(10.0, 20.0), 0.35);
        assert_eq!(heat_alpha(20.0, 20.0), 0.70);
        assert_eq!(heat_alpha(500.0, 20.0), 0.70);
        assert_eq!(heat_alpha(f64::NAN, 20.0), 0.0);
        assert_eq!(heat_alpha(5.0, 0.0), 0.0);
        let c = LIGHT;
        assert_eq!(c.heat_over(0.0, c.surface), c.surface);
        assert_eq!(c.heat_over(1.0, c.surface), c.heat);
    }

    #[test]
    fn button_colors_follow_style_state_and_disabled_opacity() {
        let c = LIGHT;
        let rest = ButtonState::default();
        let hot = ButtonState { hover: 1.0, ..rest };
        let off = ButtonState {
            disabled: true,
            hover: 1.0,
            ..rest
        };
        assert_eq!(
            button_colors(&c, ButtonStyle::Default, &rest, c.surface),
            (c.surface, c.border, c.fg)
        );
        assert_eq!(
            button_colors(&c, ButtonStyle::Default, &hot, c.surface).0,
            c.btn_hover_surface
        );
        assert_eq!(
            button_colors(&c, ButtonStyle::Primary, &hot, c.surface),
            (c.btn_hover, c.btn_hover, c.btn_fg)
        );
        assert_eq!(
            button_colors(&c, ButtonStyle::Danger, &rest, c.surface).2,
            c.on_danger
        );
        let nav = ButtonState {
            selected: true,
            hover: 1.0,
            ..rest
        };
        assert_eq!(
            button_colors(&c, ButtonStyle::Nav, &nav, c.bg).0,
            c.fg_sel_bg
        );
        let nav_hot = button_colors(&c, ButtonStyle::Nav, &hot, c.bg).0;
        assert!(nav_hot != c.bg && nav_hot != c.fg_sel_bg);
        // Disabled ignores hover and keeps 45 % over the parent.
        let (bg, _, fg) = button_colors(&c, ButtonStyle::Primary, &off, c.surface);
        assert_eq!(bg, disabled(c.btn_bg, c.surface));
        assert_eq!(fg, disabled(c.btn_fg, c.surface));
        assert_eq!(row_background(&c, 0.0, false), c.surface);
        assert_eq!(row_background(&c, 1.0, false), c.fg_soft);
        assert_eq!(row_background(&c, 1.0, true), c.fg_sel);
        assert_eq!(split_group_title("Apps (8)"), ("Apps", Some("(8)")));
        assert_eq!(split_group_title("앱"), ("앱", None));
    }

    #[test]
    fn painters_render_every_component_without_leaking() {
        unsafe {
            let fonts = Fonts::new(96, Language::English);
            let mut dib = Dib::new(420, 220).unwrap();
            let before = live_objects();
            for palette in [LIGHT, DARK_PALETTE] {
                let pt = Painter::new(dib.dc(), 96, &fonts).with_palette(palette);
                pt.fill(
                    RECT {
                        left: 0,
                        top: 0,
                        right: 420,
                        bottom: 220,
                    },
                    palette.bg,
                );
                let r = |l, t, w, h| RECT {
                    left: l,
                    top: t,
                    right: l + w,
                    bottom: t + h,
                };
                let st = ButtonState::default();
                button_face(
                    &pt,
                    r(4, 4, 90, 32),
                    ButtonStyle::Default,
                    &st,
                    "Default",
                    palette.surface,
                );
                button_face(
                    &pt,
                    r(100, 4, 90, 32),
                    ButtonStyle::Primary,
                    &ButtonState { hover: 0.5, ..st },
                    "End task",
                    palette.surface,
                );
                button_face(
                    &pt,
                    r(196, 4, 90, 32),
                    ButtonStyle::Danger,
                    &ButtonState {
                        pressed: true,
                        focused: true,
                        ..st
                    },
                    "Danger",
                    palette.surface,
                );
                button_face(
                    &pt,
                    r(292, 4, 32, 32),
                    ButtonStyle::Icon,
                    &ButtonState {
                        disabled: true,
                        ..st
                    },
                    "",
                    palette.surface,
                );
                nav_item(
                    &pt,
                    r(4, 40, 204, 40),
                    &ButtonState {
                        selected: true,
                        ..st
                    },
                    "Processes",
                    Some("30"),
                    palette.bg,
                    |pt, x, y, s, color| icon(pt, Icon::Processes, x, y, s, color),
                );
                switch(&pt, r(220, 50, 40, 20), 1.0, true, true, palette.bg);
                switch(&pt, r(270, 50, 40, 20), 0.5, false, false, palette.bg);
                segmented(
                    &pt,
                    (4, 90),
                    &["Light", "Dark", "System"],
                    1,
                    &[0.0, 0.0, 1.0],
                    Some(2),
                    palette.bg,
                );
                select_face(
                    &pt,
                    r(220, 90, 90, 28),
                    "1 s",
                    &FieldState {
                        focused: true,
                        ..Default::default()
                    },
                    None,
                    palette.bg,
                );
                pill(&pt, 4, 140, PillKind::Ok, "Efficiency", true);
                pill(&pt, 110, 140, PillKind::Warn, "High", false);
                kbd(&pt, 250, 140, "Ctrl K");
                mono_badge(&pt, r(260, 130, 20, 20), &initials("Google Chrome"), false);
                mono_badge(&pt, r(284, 130, 20, 20), "·", true);
                chevron_button(&pt, r(308, 130, 20, 20), 45.0, 1.0);
                status_dot(&pt, 340.0, 140.0, palette.warn_fg);
                for (i, kind) in [
                    Caption::Minimize,
                    Caption::Maximize,
                    Caption::Restore,
                    Caption::Close,
                ]
                .into_iter()
                .enumerate()
                {
                    caption_button(
                        &pt,
                        r(4 + i as i32 * 46, 160, 46, 44),
                        kind,
                        1.0,
                        i == 3,
                        i != 1,
                        palette.bg,
                    );
                }
                search_box(
                    &pt,
                    r(200, 170, 210, 32),
                    &FieldState::default(),
                    Some("Search"),
                    Some("Ctrl K"),
                    palette.bg,
                );
                header_cell(
                    &pt,
                    r(4, 4, 120, 51),
                    Some("30.1%"),
                    "CPU",
                    true,
                    true,
                    Some(true),
                    1.0,
                );
                group_row(&pt, r(4, 60, 200, 38), "Apps", Some("(8)"));
                heat_cell_bg(
                    &pt,
                    r(4, 100, 60, 34),
                    palette.surface,
                    heat_alpha(12.0, 20.0),
                );
                for kind in [
                    Icon::Performance,
                    Icon::Startup,
                    Icon::Services,
                    Icon::Settings,
                    Icon::Feather,
                    Icon::Search,
                    Icon::Leaf,
                    Icon::More,
                    Icon::Plus,
                ] {
                    icon(&pt, kind, 380.0, 60.0, 16.0, palette.fg);
                }
            }
            assert_eq!(live_objects(), before, "painters leaked GDI+ objects");
            // The primary face really uses the token color.
            let pt = Painter::new(dib.dc(), 96, &fonts).with_palette(LIGHT);
            button_face(
                &pt,
                RECT {
                    left: 0,
                    top: 0,
                    right: 80,
                    bottom: 32,
                },
                ButtonStyle::Primary,
                &ButtonState::default(),
                "",
                LIGHT.surface,
            );
            drop(pt);
            let pixel = dib.pixel(40, 16) & 0x00ff_ffff;
            assert_eq!(hex(pixel), LIGHT.btn_bg, "primary button background");
        }
    }
}
