//! Antialiased drawing (GDI+ flat API) and flicker-free surfaces.
//!
//! * [`Canvas`] wraps a GDI+ `Graphics` over any HDC with AntiAlias and
//!   `PixelOffsetModeHalf`, so a rectangle `(0, 0, 10, 10)` covers exactly the
//!   10×10 device pixels it names and a 1 px stroke centred on `y + 0.5` is crisp.
//!   Coordinates are **device pixels** (`f32`); convert DIP with [`px`].
//! * Text stays GDI (`DrawTextW`, ClearType) on the same opaque surface. Create
//!   the `Canvas` *after* setting the DC's clip/origin; flush it (drop it or
//!   call [`Canvas::flush`]) before drawing GDI text over GDI+ output.
//! * [`BackBuffer`] caches a 32-bit DIB per window size for WM_PAINT;
//!   [`paint_buffered`] is the whole BeginPaint → draw → BitBlt → EndPaint cycle.
//! * [`ItemBuffer`] / [`buffered_item`] (BeginBufferedPaint) for owner-draw items.
//! * [`LayeredSurface`] renders a popup opaque, then turns it into a
//!   premultiplied per-pixel-alpha image with an antialiased rounded outline and
//!   the spec's soft drop shadow, and presents it with `UpdateLayeredWindow`.
//!
//! Every GDI and GDI+ object created here is owned by an RAII value.
#![allow(dead_code)] // Primitive API consumed by the Frame/Controls/Table tracks.

use super::theme::Argb;
use std::cell::Cell;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint,
    GdiFlush, IntersectClipRect, RestoreDC, SaveDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    PAINTSTRUCT, SRCCOPY,
};
use windows_sys::Win32::Graphics::GdiPlus as gp;
use windows_sys::Win32::UI::Controls::{
    BeginBufferedPaint, BufferedPaintInit, BufferedPaintUnInit, EndBufferedPaint, BPBF_TOPDOWNDIB,
    BP_PAINTPARAMS,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetClientRect, UpdateLayeredWindow, ULW_ALPHA};

// ───────────────────────────── GDI+ lifetime ─────────────────────────────

static TOKEN: OnceLock<Option<usize>> = OnceLock::new();
static STOPPED: AtomicBool = AtomicBool::new(false);

/// Start GDI+ once per process (idempotent, thread-safe). `run()`,
/// `render_previews()` and the test windows call it before creating windows;
/// every [`Canvas`] also calls it lazily. Returns false if GDI+ is unavailable.
pub(super) fn startup() -> bool {
    let started = TOKEN.get_or_init(|| unsafe {
        let input = gp::GdiplusStartupInput {
            GdiplusVersion: 1,
            ..Default::default()
        };
        let mut token = 0usize;
        (gp::GdiplusStartup(&mut token, &input, null_mut()) == gp::Ok).then_some(token)
    });
    started.is_some() && !STOPPED.load(Ordering::Acquire)
}

/// Shut GDI+ down at process exit, after every window and Canvas is gone.
/// Later canvases become no-ops instead of touching a stopped GDI+.
pub(super) fn shutdown() {
    if let Some(Some(token)) = TOKEN.get() {
        if !STOPPED.swap(true, Ordering::AcqRel) {
            unsafe { gp::GdiplusShutdown(*token) };
        }
    }
}

thread_local! {
    // Live GDI+ objects created on this thread (graphics, brushes, pens, paths).
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static BUFFERED_PAINT: Cell<bool> = const { Cell::new(false) };
}
fn track(delta: isize) {
    LIVE.with(|live| live.set(live.get() + delta));
}
/// Number of GDI+ objects currently alive on this thread (leak checks).
pub(super) fn live_objects() -> isize {
    LIVE.with(Cell::get)
}

// ───────────────────────────── geometry ─────────────────────────────

/// DIP → device pixels.
pub(super) fn px(dpi: i32, dip: f32) -> f32 {
    dip * dpi as f32 / 96.0
}
/// DIP → whole device pixels, rounded (use for sizes/positions of shapes).
pub(super) fn pxi(dpi: i32, dip: f32) -> i32 {
    px(dpi, dip).round() as i32
}
/// Width of a CSS 1 px border at this DPI: whole device pixels so it stays crisp
/// (1 at 100–175 %, 2 at 200 %), like Chromium's border snapping.
pub(super) fn hairline(dpi: i32) -> f32 {
    (dpi / 96).max(1) as f32
}

/// Rectangle in device pixels (f32).
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(super) struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub(super) const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub(super) fn ltrb(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self::new(left, top, right - left, bottom - top)
    }
    pub(super) fn from_rect(r: RECT) -> Self {
        Self::ltrb(r.left as f32, r.top as f32, r.right as f32, r.bottom as f32)
    }
    pub(super) fn right(&self) -> f32 {
        self.x + self.w
    }
    pub(super) fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub(super) fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    pub(super) fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    pub(super) fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }
    /// Shrink by `d` on every side (negative grows).
    pub(super) fn inset(self, d: f32) -> Self {
        self.inset_xy(d, d)
    }
    pub(super) fn inset_xy(self, dx: f32, dy: f32) -> Self {
        Self::new(
            self.x + dx,
            self.y + dy,
            self.w - 2.0 * dx,
            self.h - 2.0 * dy,
        )
    }
    pub(super) fn offset(self, dx: f32, dy: f32) -> Self {
        Self::new(self.x + dx, self.y + dy, self.w, self.h)
    }
    /// Integer rectangle (rounded edges).
    pub(super) fn to_rect(self) -> RECT {
        RECT {
            left: self.x.round() as i32,
            top: self.y.round() as i32,
            right: self.right().round() as i32,
            bottom: self.bottom().round() as i32,
        }
    }
}

/// Per-corner radii (device px): top-left, top-right, bottom-right, bottom-left.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub(super) struct Radii {
    pub tl: f32,
    pub tr: f32,
    pub br: f32,
    pub bl: f32,
}

impl Radii {
    pub(super) const fn all(r: f32) -> Self {
        Self {
            tl: r,
            tr: r,
            br: r,
            bl: r,
        }
    }
    pub(super) const fn new(tl: f32, tr: f32, br: f32, bl: f32) -> Self {
        Self { tl, tr, br, bl }
    }
    pub(super) const fn top(r: f32) -> Self {
        Self::new(r, r, 0.0, 0.0)
    }
    pub(super) const fn bottom(r: f32) -> Self {
        Self::new(0.0, 0.0, r, r)
    }
    pub(super) const fn left(r: f32) -> Self {
        Self::new(r, 0.0, 0.0, r)
    }
    pub(super) const fn right(r: f32) -> Self {
        Self::new(0.0, r, r, 0.0)
    }
    pub(super) fn is_zero(&self) -> bool {
        self.tl <= 0.0 && self.tr <= 0.0 && self.br <= 0.0 && self.bl <= 0.0
    }
    /// Shrink every radius by `d` (for strokes/fills inset inside a border).
    pub(super) fn shrink(self, d: f32) -> Self {
        let f = |r: f32| if r > 0.0 { (r - d).max(0.0) } else { 0.0 };
        Self::new(f(self.tl), f(self.tr), f(self.br), f(self.bl))
    }
    fn clamped(self, w: f32, h: f32) -> Self {
        let limit = (w.min(h) / 2.0).max(0.0);
        let f = |r: f32| r.clamp(0.0, limit);
        Self::new(f(self.tl), f(self.tr), f(self.br), f(self.bl))
    }
}

// ───────────────────────────── GDI+ objects ─────────────────────────────

struct Brush(*mut gp::GpSolidFill);
impl Brush {
    fn new(color: Argb) -> Option<Self> {
        let mut brush = null_mut();
        let ok = unsafe { gp::GdipCreateSolidFill(color.0, &mut brush) } == gp::Ok;
        (ok && !brush.is_null()).then(|| {
            track(1);
            Self(brush)
        })
    }
    fn raw(&self) -> *mut gp::GpBrush {
        self.0.cast()
    }
}
impl Drop for Brush {
    fn drop(&mut self) {
        unsafe { gp::GdipDeleteBrush(self.0.cast()) };
        track(-1);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Join {
    Miter,
    Round,
}

struct Pen(*mut gp::GpPen);
impl Pen {
    fn new(color: Argb, width: f32, join: Join) -> Option<Self> {
        let mut pen = null_mut();
        let ok = unsafe { gp::GdipCreatePen1(color.0, width, gp::UnitPixel, &mut pen) } == gp::Ok;
        if !ok || pen.is_null() {
            return None;
        }
        track(1);
        unsafe { gp::GdipSetPenMiterLimit(pen, 4.0) };
        if join == Join::Round {
            unsafe {
                gp::GdipSetPenLineJoin(pen, gp::LineJoinRound);
                gp::GdipSetPenLineCap197819(
                    pen,
                    gp::LineCapRound,
                    gp::LineCapRound,
                    gp::DashCapRound,
                );
            }
        }
        Some(Self(pen))
    }
}
impl Drop for Pen {
    fn drop(&mut self) {
        unsafe { gp::GdipDeletePen(self.0) };
        track(-1);
    }
}

fn points(values: &[(f32, f32)]) -> Vec<gp::PointF> {
    values
        .iter()
        .map(|&(x, y)| gp::PointF { X: x, Y: y })
        .collect()
}

/// A GDI+ path (winding fill). Build with the methods, then fill/stroke it
/// through a [`Canvas`]. Consecutive segments of a figure connect implicitly.
pub(super) struct Path(*mut gp::GpPath);

impl Path {
    pub(super) fn new() -> Self {
        let mut path = null_mut();
        let ok =
            startup() && unsafe { gp::GdipCreatePath(gp::FillModeWinding, &mut path) } == gp::Ok;
        if ok && !path.is_null() {
            track(1);
            Self(path)
        } else {
            Self(null_mut())
        }
    }
    /// Rounded rectangle with per-corner radii (0 = square corner).
    pub(super) fn rounded_rect(r: RectF, radii: Radii) -> Self {
        let mut path = Self::new();
        path.add_rounded_rect(r, radii);
        path
    }
    pub(super) fn is_valid(&self) -> bool {
        !self.0.is_null()
    }
    pub(super) fn add_rounded_rect(&mut self, r: RectF, radii: Radii) -> &mut Self {
        if !self.is_valid() || r.is_empty() {
            return self;
        }
        let radii = radii.clamped(r.w, r.h);
        unsafe {
            gp::GdipStartPathFigure(self.0);
            let corner = |path: *mut gp::GpPath, x: f32, y: f32, radius: f32, start: f32| {
                if radius > 0.0 {
                    gp::GdipAddPathArc(path, x, y, radius * 2.0, radius * 2.0, start, 90.0);
                }
            };
            // Top-left.
            if radii.tl > 0.0 {
                corner(self.0, r.x, r.y, radii.tl, 180.0);
            } else {
                gp::GdipAddPathLine(self.0, r.x, r.y, r.x, r.y);
            }
            // Top-right.
            if radii.tr > 0.0 {
                corner(self.0, r.right() - radii.tr * 2.0, r.y, radii.tr, 270.0);
            } else {
                gp::GdipAddPathLine(self.0, r.right(), r.y, r.right(), r.y);
            }
            // Bottom-right.
            if radii.br > 0.0 {
                corner(
                    self.0,
                    r.right() - radii.br * 2.0,
                    r.bottom() - radii.br * 2.0,
                    radii.br,
                    0.0,
                );
            } else {
                gp::GdipAddPathLine(self.0, r.right(), r.bottom(), r.right(), r.bottom());
            }
            // Bottom-left.
            if radii.bl > 0.0 {
                corner(self.0, r.x, r.bottom() - radii.bl * 2.0, radii.bl, 90.0);
            } else {
                gp::GdipAddPathLine(self.0, r.x, r.bottom(), r.x, r.bottom());
            }
            gp::GdipClosePathFigure(self.0);
        }
        self
    }
    pub(super) fn start_figure(&mut self) -> &mut Self {
        if self.is_valid() {
            unsafe { gp::GdipStartPathFigure(self.0) };
        }
        self
    }
    pub(super) fn close_figure(&mut self) -> &mut Self {
        if self.is_valid() {
            unsafe { gp::GdipClosePathFigure(self.0) };
        }
        self
    }
    pub(super) fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) -> &mut Self {
        if self.is_valid() {
            unsafe { gp::GdipAddPathLine(self.0, x1, y1, x2, y2) };
        }
        self
    }
    pub(super) fn lines(&mut self, values: &[(f32, f32)]) -> &mut Self {
        if self.is_valid() && values.len() >= 2 {
            let pts = points(values);
            unsafe { gp::GdipAddPathLine2(self.0, pts.as_ptr(), pts.len() as i32) };
        }
        self
    }
    /// Cubic Bézier from `p0` through controls `c1`, `c2` to `p1`.
    pub(super) fn bezier(
        &mut self,
        p0: (f32, f32),
        c1: (f32, f32),
        c2: (f32, f32),
        p1: (f32, f32),
    ) -> &mut Self {
        if self.is_valid() {
            unsafe {
                gp::GdipAddPathBezier(self.0, p0.0, p0.1, c1.0, c1.1, c2.0, c2.1, p1.0, p1.1)
            };
        }
        self
    }
    /// Elliptical arc inside `bounds`, angles in degrees clockwise from +x.
    pub(super) fn arc(&mut self, bounds: RectF, start: f32, sweep: f32) -> &mut Self {
        if self.is_valid() {
            unsafe {
                gp::GdipAddPathArc(self.0, bounds.x, bounds.y, bounds.w, bounds.h, start, sweep)
            };
        }
        self
    }
    pub(super) fn ellipse(&mut self, bounds: RectF) -> &mut Self {
        if self.is_valid() {
            unsafe { gp::GdipAddPathEllipse(self.0, bounds.x, bounds.y, bounds.w, bounds.h) };
        }
        self
    }
    pub(super) fn rect(&mut self, r: RectF) -> &mut Self {
        if self.is_valid() {
            unsafe { gp::GdipAddPathRectangle(self.0, r.x, r.y, r.w, r.h) };
        }
        self
    }
}

impl Default for Path {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Path {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { gp::GdipDeletePath(self.0) };
            track(-1);
        }
    }
}

/// Antialiased GDI+ drawing over an HDC (device-pixel f32 coordinates, ARGB).
/// A Canvas whose creation failed is inert: every call is a no-op.
pub(super) struct Canvas {
    g: *mut gp::GpGraphics,
}

impl Canvas {
    /// # Safety
    /// `dc` must be a valid device context that outlives the Canvas.
    pub(super) unsafe fn new(dc: HDC) -> Self {
        let mut g = null_mut();
        if dc.is_null() || !startup() || gp::GdipCreateFromHDC(dc, &mut g) != gp::Ok || g.is_null()
        {
            return Self { g: null_mut() };
        }
        track(1);
        if gp::GdipSetSmoothingMode(g, gp::SmoothingModeAntiAlias8x8) != gp::Ok {
            gp::GdipSetSmoothingMode(g, gp::SmoothingModeAntiAlias);
        }
        gp::GdipSetPixelOffsetMode(g, gp::PixelOffsetModeHalf);
        Self { g }
    }
    pub(super) fn is_valid(&self) -> bool {
        !self.g.is_null()
    }
    /// Complete pending GDI+ output before GDI draws over it.
    pub(super) fn flush(&self) {
        if self.is_valid() {
            unsafe { gp::GdipFlush(self.g, gp::FlushIntentionSync) };
        }
    }
    /// Turn antialiasing off (e.g. for pixel-exact fills) or back on.
    pub(super) fn set_antialias(&self, on: bool) {
        if self.is_valid() {
            unsafe {
                gp::GdipSetSmoothingMode(
                    self.g,
                    if on {
                        gp::SmoothingModeAntiAlias8x8
                    } else {
                        gp::SmoothingModeNone
                    },
                )
            };
        }
    }
    pub(super) fn set_clip(&self, r: RectF) {
        if self.is_valid() {
            unsafe { gp::GdipSetClipRect(self.g, r.x, r.y, r.w, r.h, gp::CombineModeReplace) };
        }
    }
    pub(super) fn intersect_clip(&self, r: RectF) {
        if self.is_valid() {
            unsafe { gp::GdipSetClipRect(self.g, r.x, r.y, r.w, r.h, gp::CombineModeIntersect) };
        }
    }
    pub(super) fn reset_clip(&self) {
        if self.is_valid() {
            unsafe { gp::GdipResetClip(self.g) };
        }
    }
    /// Intersect the clip with a rounded rectangle (e.g. chart areas inside
    /// a rounded frame). Pair with [`Canvas::save`] / [`Canvas::restore`].
    pub(super) fn clip_round_rect(&self, r: RectF, radii: Radii) {
        if !self.is_valid() {
            return;
        }
        if radii.is_zero() {
            return self.intersect_clip(r);
        }
        let path = Path::rounded_rect(r, radii);
        if path.is_valid() {
            unsafe { gp::GdipSetClipPath(self.g, path.0, gp::CombineModeIntersect) };
        }
    }
    /// Save clip/transform state; returns a token for [`Canvas::restore`].
    pub(super) fn save(&self) -> u32 {
        let mut state = 0;
        if self.is_valid() {
            unsafe { gp::GdipSaveGraphics(self.g, &mut state) };
        }
        state
    }
    pub(super) fn restore(&self, state: u32) {
        if self.is_valid() {
            unsafe { gp::GdipRestoreGraphics(self.g, state) };
        }
    }
    fn with_brush(&self, color: Argb, draw: impl FnOnce(*mut gp::GpBrush)) {
        if self.is_valid() && color.alpha() != 0 {
            if let Some(brush) = Brush::new(color) {
                draw(brush.raw());
            }
        }
    }
    /// Stroke `path` by widening its geometry and filling the outline.
    /// GDI+ draws every antialiased pen thinner than 2 device px as exactly
    /// 1 px, which would turn the design's 1.4/1.5 px strokes into hairlines;
    /// the widened outline keeps the exact width (and crisp 1 px lines on
    /// pixel centres).
    fn stroke_widened(&self, path: &Path, width: f32, color: Argb, join: Join) {
        if !self.is_valid() || !path.is_valid() || color.alpha() == 0 || width <= 0.0 {
            return;
        }
        let Some(pen) = Pen::new(color, width, join) else {
            return;
        };
        let mut clone = null_mut();
        if unsafe { gp::GdipClonePath(path.0, &mut clone) } != gp::Ok || clone.is_null() {
            return;
        }
        track(1);
        let outline = Path(clone);
        if unsafe { gp::GdipWidenPath(outline.0, pen.0, null_mut(), 0.1) } == gp::Ok {
            self.fill_path(&outline, color);
        }
    }
    /// Fill a rectangle (translucent colors blend over what is there).
    pub(super) fn fill_rect(&self, r: RectF, color: Argb) {
        if r.is_empty() {
            return;
        }
        self.with_brush(color, |brush| unsafe {
            gp::GdipFillRectangle(self.g, brush, r.x, r.y, r.w, r.h);
        });
    }
    /// Alias of [`Canvas::fill_rect`] for alpha overlays (heat cells, scrims).
    pub(super) fn fill_rect_alpha(&self, r: RectF, color: Argb) {
        self.fill_rect(r, color);
    }
    pub(super) fn fill_round_rect(&self, r: RectF, radius: f32, color: Argb) {
        self.fill_round_rect_corners(r, Radii::all(radius), color);
    }
    pub(super) fn fill_round_rect_corners(&self, r: RectF, radii: Radii, color: Argb) {
        if radii.is_zero() {
            return self.fill_rect(r, color);
        }
        self.fill_path(&Path::rounded_rect(r, radii), color);
    }
    /// Stroke *inside* `r` like a CSS border: the outer edge of the stroke is `r`.
    pub(super) fn stroke_round_rect(&self, r: RectF, radius: f32, width: f32, color: Argb) {
        self.stroke_round_rect_corners(r, Radii::all(radius), width, color);
    }
    pub(super) fn stroke_round_rect_corners(
        &self,
        r: RectF,
        radii: Radii,
        width: f32,
        color: Argb,
    ) {
        let half = width / 2.0;
        let inner = r.inset(half);
        if inner.w < 0.0 || inner.h < 0.0 {
            return;
        }
        self.stroke_path(&Path::rounded_rect(inner, radii.shrink(half)), width, color);
    }
    /// CSS border box: fill `r` with `border`, then the inside with `fill`.
    /// Crisp on straight edges, antialiased on the corners.
    pub(super) fn bordered_round_rect(
        &self,
        r: RectF,
        radii: Radii,
        width: f32,
        fill: Argb,
        border: Argb,
    ) {
        if width > 0.0 && border.alpha() != 0 {
            self.fill_round_rect_corners(r, radii, border);
            self.fill_round_rect_corners(r.inset(width), radii.shrink(width), fill);
        } else {
            self.fill_round_rect_corners(r, radii, fill);
        }
    }
    pub(super) fn fill_circle(&self, cx: f32, cy: f32, radius: f32, color: Argb) {
        self.with_brush(color, |brush| unsafe {
            gp::GdipFillEllipse(
                self.g,
                brush,
                cx - radius,
                cy - radius,
                radius * 2.0,
                radius * 2.0,
            );
        });
    }
    /// Stroke a circle; `radius` is the centre line of the stroke.
    pub(super) fn stroke_circle(&self, cx: f32, cy: f32, radius: f32, width: f32, color: Argb) {
        let mut path = Path::new();
        path.ellipse(RectF::new(
            cx - radius,
            cy - radius,
            radius * 2.0,
            radius * 2.0,
        ));
        self.stroke_widened(&path, width, color, Join::Miter);
    }
    /// A straight line with flat caps.
    pub(super) fn line(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Argb) {
        let mut path = Path::new();
        path.line(x1, y1, x2, y2);
        self.stroke_widened(&path, width, color, Join::Miter);
    }
    /// A straight line with round caps.
    pub(super) fn line_round(&self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Argb) {
        let mut path = Path::new();
        path.line(x1, y1, x2, y2);
        self.stroke_widened(&path, width, color, Join::Round);
    }
    /// Connected line (round joins/caps), e.g. chart traces.
    pub(super) fn polyline(&self, values: &[(f32, f32)], width: f32, color: Argb) {
        if values.len() < 2 {
            return;
        }
        let mut path = Path::new();
        path.lines(values);
        self.stroke_widened(&path, width, color, Join::Round);
    }
    /// Polyline with sharp (miter) joins and flat caps, like SVG defaults.
    pub(super) fn polyline_sharp(&self, values: &[(f32, f32)], width: f32, color: Argb) {
        if values.len() < 2 {
            return;
        }
        let mut path = Path::new();
        path.lines(values);
        self.stroke_widened(&path, width, color, Join::Miter);
    }
    pub(super) fn fill_polygon(&self, values: &[(f32, f32)], color: Argb) {
        if values.len() < 3 {
            return;
        }
        let pts = points(values);
        self.with_brush(color, |brush| unsafe {
            gp::GdipFillPolygon(
                self.g,
                brush,
                pts.as_ptr(),
                pts.len() as i32,
                gp::FillModeWinding,
            );
        });
    }
    /// The reference chevron (`M3 1 l4 4 -4 4` in a 10×10 box) centred on
    /// `(cx, cy)`, `size` device px wide, rotated clockwise by `angle_deg`
    /// (0 = pointing right, 90 = pointing down).
    pub(super) fn chevron(
        &self,
        cx: f32,
        cy: f32,
        size: f32,
        angle_deg: f32,
        width: f32,
        color: Argb,
    ) {
        let unit = size / 10.0;
        let (sin, cos) = angle_deg.to_radians().sin_cos();
        let rotate = |x: f32, y: f32| {
            let (x, y) = (x * unit, y * unit);
            (cx + x * cos - y * sin, cy + x * sin + y * cos)
        };
        let pts = [rotate(-2.0, -4.0), rotate(2.0, 0.0), rotate(-2.0, 4.0)];
        self.polyline_sharp(&pts, width, color);
    }
    pub(super) fn fill_path(&self, path: &Path, color: Argb) {
        if path.is_valid() {
            self.with_brush(color, |brush| unsafe {
                gp::GdipFillPath(self.g, brush, path.0);
            });
        }
    }
    pub(super) fn stroke_path(&self, path: &Path, width: f32, color: Argb) {
        self.stroke_widened(path, width, color, Join::Miter);
    }
    /// Stroke with round joins and caps (SVG `stroke-linecap/linejoin: round`).
    pub(super) fn stroke_path_round(&self, path: &Path, width: f32, color: Argb) {
        self.stroke_widened(path, width, color, Join::Round);
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        if !self.g.is_null() {
            unsafe { gp::GdipDeleteGraphics(self.g) };
            track(-1);
        }
    }
}

// ───────────────────────────── DIB surfaces ─────────────────────────────

/// A 32-bit top-down DIB selected into its own memory DC (BGRA, `0xAARRGGBB`).
pub(super) struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u32,
    width: i32,
    height: i32,
}

impl Dib {
    /// # Safety
    /// Plain GDI allocation; the result owns its DC and bitmap.
    pub(super) unsafe fn new(width: i32, height: i32) -> Option<Self> {
        if !(1..=16_384).contains(&width) || !(1..=16_384).contains(&height) {
            return None;
        }
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return None;
        }
        let mut dib = Self {
            dc,
            bitmap: null_mut(),
            previous: null_mut(),
            bits: null_mut(),
            width,
            height,
        };
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        dib.bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if dib.bitmap.is_null() || bits.is_null() {
            return None;
        }
        dib.bits = bits.cast();
        let previous = SelectObject(dc, dib.bitmap);
        if previous.is_null() || previous as isize == -1 {
            return None;
        }
        dib.previous = previous;
        Some(dib)
    }
    pub(super) fn dc(&self) -> HDC {
        self.dc
    }
    pub(super) fn width(&self) -> i32 {
        self.width
    }
    pub(super) fn height(&self) -> i32 {
        self.height
    }
    /// Pixel access (BGRA little-endian = `0xAARRGGBB`). Flushes GDI first.
    pub(super) fn pixels(&mut self) -> &mut [u32] {
        unsafe {
            GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, (self.width * self.height) as usize)
        }
    }
    /// Fill every pixel with `color` premultiplied (e.g. a scrim: fg @ 22 %
    /// presented with [`present_layered`]).
    pub(super) fn fill_premultiplied(&mut self, color: Argb) {
        let a = color.alpha() as u32;
        let channel = |shift: u32| (((color.0 >> shift) & 0xff) * a + 127) / 255;
        let value = (a << 24) | (channel(16) << 16) | (channel(8) << 8) | channel(0);
        self.pixels().fill(value);
    }
    pub(super) fn pixel(&mut self, x: i32, y: i32) -> u32 {
        let width = self.width;
        self.pixels()[(y * width + x) as usize]
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            DeleteDC(self.dc);
        }
    }
}

/// Off-screen frame for WM_PAINT, cached per window size (reused while the
/// size is unchanged; reallocated on resize).
#[derive(Default)]
pub(super) struct BackBuffer {
    dib: Option<Dib>,
}

impl BackBuffer {
    pub(super) const fn new() -> Self {
        Self { dib: None }
    }
    /// A memory DC of exactly `width × height`. `fresh` is true when the
    /// surface was (re)allocated and therefore holds no previous frame.
    ///
    /// # Safety
    /// Plain GDI allocation.
    pub(super) unsafe fn prepare(&mut self, width: i32, height: i32) -> Option<(HDC, bool)> {
        let width = width.max(1);
        let height = height.max(1);
        let reuse = self
            .dib
            .as_ref()
            .is_some_and(|dib| dib.width == width && dib.height == height);
        if reuse {
            return self.dib.as_ref().map(|dib| (dib.dc, false));
        }
        self.dib = None;
        self.dib = Dib::new(width, height);
        self.dib.as_ref().map(|dib| (dib.dc, true))
    }
    pub(super) fn dc(&self) -> Option<HDC> {
        self.dib.as_ref().map(|dib| dib.dc)
    }
    pub(super) fn size(&self) -> Option<(i32, i32)> {
        self.dib.as_ref().map(|dib| (dib.width, dib.height))
    }
    /// Drop the cached surface (e.g. after WM_DPICHANGED or when hidden).
    pub(super) fn release(&mut self) {
        self.dib = None;
    }
    /// Copy `r` (client coordinates) of the frame to `target`.
    ///
    /// # Safety
    /// `target` must be a valid DC.
    pub(super) unsafe fn present(&self, target: HDC, r: &RECT) {
        if let Some(dib) = &self.dib {
            BitBlt(
                target,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                dib.dc,
                r.left,
                r.top,
                SRCCOPY,
            );
        }
    }
}

/// The complete flicker-free WM_PAINT cycle for a window:
/// BeginPaint → `draw(memory_dc, client_rect)` into the cached back buffer
/// (clipped to the update region unless the buffer is new) → BitBlt → EndPaint.
/// Falls back to painting the window DC directly if allocation fails.
/// Return 1 (non-zero: "erased") from WM_ERASEBKGND so the background is
/// never erased separately.
///
/// # Safety
/// Must be called from WM_PAINT of `hwnd` on its thread.
pub(super) unsafe fn paint_buffered(
    hwnd: HWND,
    back: &mut BackBuffer,
    draw: impl FnOnce(HDC, &RECT),
) {
    let mut ps: PAINTSTRUCT = zeroed();
    let dc = BeginPaint(hwnd, &mut ps);
    if dc.is_null() {
        return;
    }
    let mut client: RECT = zeroed();
    GetClientRect(hwnd, &mut client);
    match back.prepare(client.right - client.left, client.bottom - client.top) {
        Some((memory, fresh)) => {
            let saved = SaveDC(memory);
            if !fresh {
                IntersectClipRect(
                    memory,
                    ps.rcPaint.left,
                    ps.rcPaint.top,
                    ps.rcPaint.right,
                    ps.rcPaint.bottom,
                );
            }
            draw(memory, &client);
            RestoreDC(memory, saved);
            let area = if fresh { client } else { ps.rcPaint };
            back.present(dc, &area);
        }
        None => draw(dc, &client),
    }
    EndPaint(hwnd, &ps);
}

/// Buffered painting for owner-draw items (`BeginBufferedPaint`). Draw into
/// [`ItemBuffer::dc`] with the *same* coordinates as the target rectangle; the
/// result is copied to the target when the value drops.
pub(super) struct ItemBuffer {
    handle: isize,
    dc: HDC,
}

impl ItemBuffer {
    /// # Safety
    /// `target` must be valid for the lifetime of the buffer.
    pub(super) unsafe fn begin(target: HDC, r: &RECT) -> Option<Self> {
        if target.is_null() || r.right <= r.left || r.bottom <= r.top {
            return None;
        }
        BUFFERED_PAINT.with(|initialised| {
            if !initialised.get() && BufferedPaintInit() >= 0 {
                initialised.set(true);
            }
        });
        let params = BP_PAINTPARAMS {
            cbSize: size_of::<BP_PAINTPARAMS>() as u32,
            dwFlags: 0,
            prcExclude: null(),
            pBlendFunction: null(),
        };
        let mut dc = null_mut();
        let handle = BeginBufferedPaint(target, r, BPBF_TOPDOWNDIB, &params, &mut dc);
        (handle != 0 && !dc.is_null()).then_some(Self { handle, dc })
    }
    pub(super) fn dc(&self) -> HDC {
        self.dc
    }
}

impl Drop for ItemBuffer {
    fn drop(&mut self) {
        unsafe { EndBufferedPaint(self.handle, 1) };
    }
}

/// Paint `r` of `target` through a buffered-paint surface (or directly if
/// buffering is unavailable). `draw` receives the DC to draw on.
///
/// # Safety
/// `target` must be a valid DC.
pub(super) unsafe fn buffered_item(target: HDC, r: &RECT, draw: impl FnOnce(HDC)) {
    match ItemBuffer::begin(target, r) {
        Some(buffer) => draw(buffer.dc()),
        None => draw(target),
    }
}

/// Release this thread's buffered-paint cache (call once on the UI thread
/// after its windows are destroyed; later [`ItemBuffer`]s re-initialise).
pub(super) fn buffered_paint_shutdown() {
    BUFFERED_PAINT.with(|initialised| {
        if initialised.replace(false) {
            unsafe { BufferedPaintUnInit() };
        }
    });
}

// ───────────────────────────── layered popups ─────────────────────────────

/// One CSS box-shadow layer (`offset-x offset-y blur color@alpha`), device px.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Shadow {
    pub dx: f32,
    pub dy: f32,
    /// CSS blur radius; the Gaussian's standard deviation is blur / 2.
    pub blur: f32,
    pub alpha: f32,
}

impl Shadow {
    /// Scale a DIP shadow to `dpi`.
    pub(super) fn scaled(self, dpi: i32) -> Self {
        Self {
            dx: px(dpi, self.dx),
            dy: px(dpi, self.dy),
            blur: px(dpi, self.blur),
            alpha: self.alpha,
        }
    }
}

/// `--shadow: 0 24px 64px fg@18%, 0 2px 8px fg@8%` (DIP; scale with [`Shadow::scaled`]).
pub(super) const POPUP_SHADOW: [Shadow; 2] = [
    Shadow {
        dx: 0.0,
        dy: 24.0,
        blur: 64.0,
        alpha: 0.18,
    },
    Shadow {
        dx: 0.0,
        dy: 2.0,
        blur: 8.0,
        alpha: 0.08,
    },
];

/// Scaled popup shadow for a DPI.
pub(super) fn popup_shadow(dpi: i32) -> [Shadow; 2] {
    POPUP_SHADOW.map(|layer| layer.scaled(dpi))
}

/// Extra pixels around a popup's content that its shadow needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Margins {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub(super) fn shadow_margins(layers: &[Shadow]) -> Margins {
    let mut m = Margins::default();
    for layer in layers {
        // The Gaussian tail is invisible beyond ~2σ = one blur radius.
        let reach = layer.blur.max(0.0);
        m.left = m.left.max((reach - layer.dx).ceil().max(0.0) as i32);
        m.right = m.right.max((reach + layer.dx).ceil().max(0.0) as i32);
        m.top = m.top.max((reach - layer.dy).ceil().max(0.0) as i32);
        m.bottom = m.bottom.max((reach + layer.dy).ceil().max(0.0) as i32);
    }
    m
}

/// Abramowitz–Stegun 7.1.26 (|error| < 1.5e-7).
fn erf(x: f32) -> f32 {
    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let y = 1.0
        - (((((1.061_405_4 * t - 1.453_152_1) * t) + 1.421_413_8) * t - 0.284_496_74) * t
            + 0.254_829_6)
            * t
            * (-x * x).exp();
    sign * y
}

/// Coverage of a pixel centred at `c` by the 1-D interval `[lo, hi]` blurred
/// with a Gaussian of standard deviation `sigma`.
fn blurred_cover(c: f32, lo: f32, hi: f32, sigma: f32) -> f32 {
    if sigma <= 0.01 {
        return if c >= lo && c < hi { 1.0 } else { 0.0 };
    }
    let k = 1.0 / (sigma * std::f32::consts::SQRT_2);
    (0.5 * (erf((c - lo) * k) - erf((c - hi) * k))).clamp(0.0, 1.0)
}

/// Antialiased coverage of the pixel centred at `(x, y)` by a rounded rect.
pub(super) fn rounded_rect_coverage(x: f32, y: f32, r: RectF, radius: f32) -> f32 {
    let radius = radius.clamp(0.0, r.w.min(r.h) / 2.0);
    let (hx, hy) = (r.w / 2.0 - radius, r.h / 2.0 - radius);
    let qx = (x - r.cx()).abs() - hx;
    let qy = (y - r.cy()).abs() - hy;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let distance = outside + qx.max(qy).min(0.0) - radius;
    (0.5 - distance).clamp(0.0, 1.0)
}

/// A per-pixel-alpha popup image: `content()` is painted opaque (GDI text +
/// Canvas), then [`LayeredSurface::compose`] computes the rounded-rect coverage
/// alpha and the soft shadow around it, premultiplied, ready for
/// [`LayeredSurface::present`].
pub(super) struct LayeredSurface {
    dib: Dib,
    margins: Margins,
    layers: Vec<Shadow>,
    content_w: i32,
    content_h: i32,
}

impl LayeredSurface {
    /// Surface for `content_w × content_h` device px plus room for `layers`
    /// (already DPI-scaled, e.g. [`popup_shadow`]; `&[]` for no shadow).
    ///
    /// # Safety
    /// Plain GDI allocation.
    pub(super) unsafe fn new(content_w: i32, content_h: i32, layers: &[Shadow]) -> Option<Self> {
        let margins = shadow_margins(layers);
        let dib = Dib::new(
            content_w + margins.left + margins.right,
            content_h + margins.top + margins.bottom,
        )?;
        Some(Self {
            dib,
            margins,
            layers: layers.to_vec(),
            content_w,
            content_h,
        })
    }
    pub(super) fn dc(&self) -> HDC {
        self.dib.dc
    }
    pub(super) fn margins(&self) -> Margins {
        self.margins
    }
    /// Whole surface size (content + shadow margins).
    pub(super) fn size(&self) -> SIZE {
        SIZE {
            cx: self.dib.width,
            cy: self.dib.height,
        }
    }
    /// Where the popup's own box lies inside the surface.
    pub(super) fn content(&self) -> RECT {
        RECT {
            left: self.margins.left,
            top: self.margins.top,
            right: self.margins.left + self.content_w,
            bottom: self.margins.top + self.content_h,
        }
    }
    pub(super) fn dib(&mut self) -> &mut Dib {
        &mut self.dib
    }
    /// Paint the standard popup box: `border` everywhere in the content, then
    /// `fill` inside a `border_width` inset, both with `radius` corners.
    ///
    /// # Safety
    /// GDI/GDI+ drawing on the surface's own DC.
    pub(super) unsafe fn fill_frame(&self, fill: u32, border: u32, radius: f32, border_width: f32) {
        let content = self.content();
        let brush = windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(border);
        windows_sys::Win32::Graphics::Gdi::FillRect(self.dib.dc, &content, brush);
        DeleteObject(brush);
        let canvas = Canvas::new(self.dib.dc);
        canvas.fill_round_rect(
            RectF::from_rect(content).inset(border_width),
            (radius - border_width).max(0.0),
            super::theme::solid(fill),
        );
    }
    /// Turn the opaque content into premultiplied BGRA: coverage alpha of the
    /// rounded content box, composited over the blurred `shadow_color` layers.
    pub(super) fn compose(&mut self, radius: f32, shadow_color: u32) {
        self.compose_with(radius, shadow_color, true);
    }
    /// Only the shadow: fully transparent inside the rounded content box (for
    /// a [`ShadowWindow`] behind an opaque popup), whatever was painted there.
    pub(super) fn compose_shadow(&mut self, radius: f32, shadow_color: u32) {
        self.compose_with(radius, shadow_color, false);
    }
    fn compose_with(&mut self, radius: f32, shadow_color: u32, with_content: bool) {
        let (w, h) = (self.dib.width, self.dib.height);
        let content = RectF::from_rect(self.content());
        let layers = self.layers.clone();
        let profiles: Vec<(Vec<f32>, Vec<f32>, f32)> = layers
            .iter()
            .map(|layer| {
                let sigma = layer.blur / 2.0;
                let xs = (0..w)
                    .map(|x| {
                        blurred_cover(
                            x as f32 + 0.5,
                            content.x + layer.dx,
                            content.right() + layer.dx,
                            sigma,
                        )
                    })
                    .collect();
                let ys = (0..h)
                    .map(|y| {
                        blurred_cover(
                            y as f32 + 0.5,
                            content.y + layer.dy,
                            content.bottom() + layer.dy,
                            sigma,
                        )
                    })
                    .collect();
                (xs, ys, layer.alpha.clamp(0.0, 1.0))
            })
            .collect();
        let sr = (shadow_color & 0xff) as f32;
        let sg = ((shadow_color >> 8) & 0xff) as f32;
        let sb = ((shadow_color >> 16) & 0xff) as f32;
        let pixels = self.dib.pixels();
        for y in 0..h {
            let cy = y as f32 + 0.5;
            let inside_y = cy > content.y - 1.0 && cy < content.bottom() + 1.0;
            for x in 0..w {
                let cx = x as f32 + 0.5;
                let cover = if inside_y && cx > content.x - 1.0 && cx < content.right() + 1.0 {
                    rounded_rect_coverage(cx, cy, content, radius)
                } else {
                    0.0
                };
                let index = (y * w + x) as usize;
                if cover >= 1.0 {
                    if with_content {
                        pixels[index] |= 0xff00_0000;
                    } else {
                        pixels[index] = 0;
                    }
                    continue;
                }
                let mut keep = 1.0;
                for (xs, ys, alpha) in &profiles {
                    keep *= 1.0 - alpha * xs[x as usize] * ys[y as usize];
                }
                let shadow = (1.0 - keep) * (1.0 - cover);
                let (c, cover) = if with_content {
                    (pixels[index], cover)
                } else {
                    (0, 0.0)
                };
                let channel = |content: f32, shadow_channel: f32| {
                    (content * cover + shadow_channel * shadow)
                        .round()
                        .clamp(0.0, 255.0) as u32
                };
                let r = channel(((c >> 16) & 0xff) as f32, sr);
                let g = channel(((c >> 8) & 0xff) as f32, sg);
                let b = channel((c & 0xff) as f32, sb);
                let a = ((cover + shadow) * 255.0).round().clamp(0.0, 255.0) as u32;
                pixels[index] = (a << 24) | (r.min(a) << 16) | (g.min(a) << 8) | b.min(a);
            }
        }
    }
    /// `UpdateLayeredWindow` with this image. `content_origin` is the screen
    /// position of the popup's box (the window itself starts `margins` earlier);
    /// `alpha` is the global opacity for fades (255 = opaque).
    ///
    /// # Safety
    /// `hwnd` must be a `WS_EX_LAYERED` window of this thread.
    pub(super) unsafe fn present(&self, hwnd: HWND, content_origin: POINT, alpha: u8) -> bool {
        let destination = POINT {
            x: content_origin.x - self.margins.left,
            y: content_origin.y - self.margins.top,
        };
        present_layered(hwnd, &self.dib, destination, alpha)
    }
}

/// UpdateLayeredWindow windows never draw child windows, so a popup that
/// needs native children (the palette's Edit with caret and IME, a list)
/// stays an ordinary opaque `WS_POPUP` — rounded and bordered by DWM with
/// [`round_popup`] — and gets this shadow-only layered companion right
/// behind it. Fully painted popups (menus, dropdowns, the dialog, the toast)
/// use a [`LayeredSurface`] instead. The companion is click-through
/// (`WS_EX_TRANSPARENT`, `HTTRANSPARENT`), never activates, is owned by the
/// popup's owner, and is destroyed with the struct.
pub(super) struct ShadowWindow {
    hwnd: HWND,
    surface: Option<LayeredSurface>,
    /// (content w, h, dpi, color, radius bits) the surface was composed for.
    composed: Option<(i32, i32, i32, u32, u32)>,
}

const SHADOW_CLASS: &str = "FeatherTaskManager.PopupShadow";

unsafe extern "system" fn shadow_proc(hwnd: HWND, msg: u32, w: usize, l: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, HTTRANSPARENT, MA_NOACTIVATE, WM_MOUSEACTIVATE, WM_NCHITTEST,
    };
    match msg {
        WM_NCHITTEST => HTTRANSPARENT as isize,
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

impl ShadowWindow {
    /// A hidden shadow window owned by `owner` (the popup's owner window).
    ///
    /// # Safety
    /// Call on the UI thread that owns `owner`.
    pub(super) unsafe fn new(owner: HWND) -> Option<Self> {
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, RegisterClassW, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
        };
        const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;
        let class: Vec<u16> = SHADOW_CLASS.encode_utf16().chain(Some(0)).collect();
        let instance = GetModuleHandleW(null());
        let definition = WNDCLASSW {
            lpfnWndProc: Some(shadow_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..zeroed()
        };
        if RegisterClassW(&definition) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
            return None;
        }
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class.as_ptr(),
            null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            owner,
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            return None;
        }
        Some(Self {
            hwnd,
            surface: None,
            composed: None,
        })
    }
    pub(super) fn hwnd(&self) -> HWND {
        self.hwnd
    }
    /// Show the shadow of the popup box `content` (screen px) directly below
    /// `popup` in z-order at opacity `alpha` (fade it with the popup). The
    /// shadow image is recomposed only when the size, DPI, color or radius
    /// change; moving and fading only re-present it.
    ///
    /// # Safety
    /// Call on the UI thread; `popup` must be a window of this thread.
    pub(super) unsafe fn show(
        &mut self,
        popup: HWND,
        content: RECT,
        dpi: i32,
        radius: f32,
        shadow_color: u32,
        alpha: u8,
    ) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
        };
        let (w, h) = (content.right - content.left, content.bottom - content.top);
        if w <= 0 || h <= 0 {
            return false;
        }
        let key = (w, h, dpi, shadow_color, radius.to_bits());
        if self.composed != Some(key) || self.surface.is_none() {
            let Some(mut surface) = LayeredSurface::new(w, h, &popup_shadow(dpi)) else {
                return false;
            };
            surface.compose_shadow(radius, shadow_color);
            self.surface = Some(surface);
            self.composed = Some(key);
        }
        let Some(surface) = self.surface.as_ref() else {
            return false;
        };
        let shown = surface.present(
            self.hwnd,
            POINT {
                x: content.left,
                y: content.top,
            },
            alpha,
        );
        SetWindowPos(
            self.hwnd,
            popup,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        shown
    }
    /// Hide it (the image is kept for the next `show`).
    ///
    /// # Safety
    /// Call on the UI thread.
    pub(super) unsafe fn hide(&self) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
        ShowWindow(self.hwnd, SW_HIDE);
    }
    /// The composed shadow image (tests, previews).
    pub(super) fn surface(&mut self) -> Option<&mut LayeredSurface> {
        self.surface.as_mut()
    }
}

impl Drop for ShadowWindow {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
        }
    }
}

/// Windows 11 frame for an opaque (non-layered) popup such as the palette:
/// DWM rounds its corners (DWMWCP_ROUND = the design's 8 px, or ROUNDSMALL =
/// 4 px when `small`) and draws its 1 px border in `border` (COLORREF, e.g.
/// the border token). Returns false where DWM does not support it (Windows
/// 10: corners stay square, so paint a 1 px border yourself there).
///
/// # Safety
/// `hwnd` must be a valid top-level window.
pub(super) unsafe fn round_popup(hwnd: HWND, border: u32, small: bool) -> bool {
    use windows_sys::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
        DWMWCP_ROUNDSMALL,
    };
    let corner = if small {
        DWMWCP_ROUNDSMALL
    } else {
        DWMWCP_ROUND
    };
    let rounded = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        (&corner as *const i32).cast(),
        size_of::<i32>() as u32,
    ) >= 0;
    let bordered = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR as u32,
        (&border as *const u32).cast(),
        size_of::<u32>() as u32,
    ) >= 0;
    rounded && bordered
}

/// Present a premultiplied 32-bit DIB on a layered window at `top_left` (screen).
///
/// # Safety
/// `hwnd` must be a `WS_EX_LAYERED` window.
pub(super) unsafe fn present_layered(hwnd: HWND, dib: &Dib, top_left: POINT, alpha: u8) -> bool {
    GdiFlush();
    let size = SIZE {
        cx: dib.width,
        cy: dib.height,
    };
    let source = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: alpha,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    UpdateLayeredWindow(
        hwnd,
        null_mut(),
        &top_left,
        &size,
        dib.dc,
        &source,
        0,
        &blend,
        ULW_ALPHA,
    ) != 0
}

#[cfg(test)]
mod tests {
    use super::super::theme::{argb, hex, solid};
    use super::*;
    use windows_sys::Win32::Graphics::Gdi::{
        CreateSolidBrush, FillRect, SetViewportOrgEx, TextOutW,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_POPUP,
    };

    const WHITE: u32 = 0x00ff_ffff;

    unsafe fn white(dib: &mut Dib) {
        dib.pixels().fill(0xffff_ffff);
    }
    fn rgb(pixel: u32) -> u32 {
        pixel & 0x00ff_ffff
    }
    fn gray(pixel: u32) -> u32 {
        pixel & 0xff
    }

    #[test]
    fn canvas_fills_are_pixel_exact_and_curves_are_antialiased() {
        assert!(startup());
        unsafe {
            let mut dib = Dib::new(64, 64).unwrap();
            white(&mut dib);
            {
                let canvas = Canvas::new(dib.dc());
                assert!(canvas.is_valid());
                canvas.fill_rect(RectF::new(10.0, 10.0, 10.0, 10.0), solid(0));
                canvas.fill_circle(45.0, 45.0, 8.0, solid(0));
                canvas.line(0.0, 30.5, 30.0, 30.5, 1.0, solid(0));
            }
            // PixelOffsetModeHalf: integer rectangles have no fuzzy edges.
            assert_eq!(rgb(dib.pixel(10, 10)), 0);
            assert_eq!(rgb(dib.pixel(19, 19)), 0);
            assert_eq!(rgb(dib.pixel(9, 10)), WHITE);
            assert_eq!(rgb(dib.pixel(20, 19)), WHITE);
            // A 1 px line centred on y + .5 fills exactly one row.
            assert_eq!(rgb(dib.pixel(15, 30)), 0);
            assert_eq!(rgb(dib.pixel(15, 29)), WHITE);
            assert_eq!(rgb(dib.pixel(15, 31)), WHITE);
            // The circle is solid inside and has partially covered edge pixels.
            assert_eq!(rgb(dib.pixel(45, 45)), 0);
            let edge: Vec<u32> = (37..54).map(|x| gray(dib.pixel(x, 40))).collect();
            assert!(edge.iter().any(|&v| v > 10 && v < 245), "no AA: {edge:?}");
        }
        assert_eq!(live_objects(), 0);
    }

    #[test]
    fn canvas_honours_the_dc_origin_and_clip_region() {
        unsafe {
            let mut dib = Dib::new(40, 40).unwrap();
            white(&mut dib);
            let saved = SaveDC(dib.dc());
            SetViewportOrgEx(dib.dc(), 5, 7, null_mut());
            IntersectClipRect(dib.dc(), 0, 0, 10, 10);
            {
                let canvas = Canvas::new(dib.dc());
                canvas.fill_rect(RectF::new(0.0, 0.0, 30.0, 30.0), solid(0));
            }
            RestoreDC(dib.dc(), saved);
            assert_eq!(rgb(dib.pixel(5, 7)), 0, "viewport origin ignored");
            assert_eq!(rgb(dib.pixel(14, 16)), 0);
            assert_eq!(rgb(dib.pixel(4, 7)), WHITE);
            assert_eq!(rgb(dib.pixel(15, 16)), WHITE, "clip region ignored");
            assert_eq!(rgb(dib.pixel(14, 17)), WHITE, "clip region ignored");
        }
    }

    #[test]
    fn fractional_strokes_keep_their_width() {
        unsafe {
            for (width, expected) in [(1.0f32, 1.0f32), (1.4, 1.4), (1.5, 1.5), (2.0, 2.0)] {
                let mut dib = Dib::new(40, 20).unwrap();
                white(&mut dib);
                {
                    let canvas = Canvas::new(dib.dc());
                    canvas.line(5.0, 10.3, 35.0, 10.3, width, solid(0));
                }
                let coverage: f32 = (0..20)
                    .map(|y| (255 - gray(dib.pixel(20, y))) as f32 / 255.0)
                    .sum();
                assert!(
                    (coverage - expected).abs() < 0.06,
                    "{width} px stroke covered {coverage}"
                );
            }
        }
        assert_eq!(live_objects(), 0);
    }

    #[test]
    fn canvas_clips_combine_with_the_dc_clip() {
        unsafe {
            let mut dib = Dib::new(40, 40).unwrap();
            white(&mut dib);
            let saved = SaveDC(dib.dc());
            IntersectClipRect(dib.dc(), 0, 0, 20, 40);
            {
                let canvas = Canvas::new(dib.dc());
                // Replacing the canvas clip never escapes the DC clip.
                canvas.set_clip(RectF::new(0.0, 0.0, 40.0, 40.0));
                canvas.fill_rect(RectF::new(0.0, 0.0, 40.0, 10.0), solid(0));
                let state = canvas.save();
                canvas.clip_round_rect(RectF::new(0.0, 20.0, 40.0, 20.0), Radii::all(8.0));
                canvas.fill_rect(RectF::new(0.0, 12.0, 40.0, 28.0), solid(0));
                canvas.restore(state);
                canvas.reset_clip();
                canvas.fill_rect(RectF::new(0.0, 10.0, 40.0, 2.0), solid(0));
            }
            RestoreDC(dib.dc(), saved);
            assert_eq!(rgb(dib.pixel(5, 5)), 0);
            assert_eq!(
                rgb(dib.pixel(25, 5)),
                WHITE,
                "canvas clip escaped the DC clip"
            );
            assert_eq!(
                rgb(dib.pixel(25, 11)),
                WHITE,
                "reset_clip escaped the DC clip"
            );
            assert_eq!(rgb(dib.pixel(10, 15)), WHITE, "rounded clip ignored");
            assert_eq!(rgb(dib.pixel(10, 30)), 0);
            assert_eq!(rgb(dib.pixel(0, 20)), WHITE, "rounded clip corner");
        }
    }

    #[test]
    fn per_corner_radii_round_only_the_requested_corners() {
        unsafe {
            let mut dib = Dib::new(40, 40).unwrap();
            white(&mut dib);
            {
                let canvas = Canvas::new(dib.dc());
                canvas.fill_round_rect_corners(
                    RectF::new(0.0, 0.0, 40.0, 40.0),
                    Radii::new(12.0, 0.0, 0.0, 0.0),
                    solid(0),
                );
            }
            assert_eq!(rgb(dib.pixel(0, 0)), WHITE, "top-left must be rounded");
            assert_eq!(rgb(dib.pixel(39, 0)), 0, "top-right square");
            assert_eq!(rgb(dib.pixel(39, 39)), 0, "bottom-right square");
            assert_eq!(rgb(dib.pixel(0, 39)), 0, "bottom-left square");
            assert_eq!(rgb(dib.pixel(12, 12)), 0);
            // Inside stroke: the outer pixel row belongs to the border.
            white(&mut dib);
            {
                let canvas = Canvas::new(dib.dc());
                canvas.stroke_round_rect(RectF::new(4.0, 4.0, 20.0, 20.0), 4.0, 1.0, solid(0));
                canvas.bordered_round_rect(
                    RectF::new(26.0, 4.0, 12.0, 12.0),
                    Radii::all(3.0),
                    1.0,
                    solid(WHITE),
                    solid(0),
                );
            }
            assert_eq!(rgb(dib.pixel(14, 4)), 0);
            assert_eq!(rgb(dib.pixel(14, 3)), WHITE);
            assert_eq!(rgb(dib.pixel(14, 5)), WHITE);
            assert_eq!(rgb(dib.pixel(32, 4)), 0);
            assert_eq!(rgb(dib.pixel(32, 5)), WHITE);
        }
    }

    #[test]
    fn chevron_polyline_and_paths_draw_within_their_bounds() {
        unsafe {
            let mut dib = Dib::new(30, 30).unwrap();
            white(&mut dib);
            {
                let canvas = Canvas::new(dib.dc());
                // Pointing down (90°): the tip is below the centre.
                canvas.chevron(15.0, 15.0, 20.0, 90.0, 2.0, solid(0));
            }
            let dark_rows: Vec<i32> = (0..30)
                .filter(|&y| (0..30).any(|x| gray(dib.pixel(x, y)) < 128))
                .collect();
            assert!(dark_rows.first().is_some_and(|&y| y >= 9));
            assert!(dark_rows.last().is_some_and(|&y| y <= 20));
            white(&mut dib);
            {
                let canvas = Canvas::new(dib.dc());
                canvas.polyline(&[(2.0, 2.0), (27.0, 27.0)], 1.5, solid(0));
                canvas.fill_polygon(&[(0.0, 29.0), (6.0, 20.0), (12.0, 29.0)], argb(0, 0.5));
                let mut path = Path::new();
                path.start_figure()
                    .bezier((0.0, 0.0), (10.0, 0.0), (20.0, 10.0), (29.0, 0.0));
                canvas.stroke_path_round(&path, 1.0, solid(0));
            }
            assert!(gray(dib.pixel(15, 15)) < 200);
            let half = gray(dib.pixel(6, 27));
            assert!((100..160).contains(&half), "50 % alpha fill: {half}");
        }
        assert_eq!(live_objects(), 0);
    }

    #[test]
    fn back_buffer_is_cached_per_size() {
        unsafe {
            let mut back = BackBuffer::new();
            let (first, fresh) = back.prepare(120, 80).unwrap();
            assert!(fresh);
            let (again, fresh) = back.prepare(120, 80).unwrap();
            assert!(!fresh);
            assert_eq!(first, again);
            let (_, fresh) = back.prepare(121, 80).unwrap();
            assert!(fresh);
            assert_eq!(back.size(), Some((121, 80)));
            back.release();
            assert_eq!(back.size(), None);
        }
    }

    #[test]
    fn buffered_items_copy_their_drawing_to_the_target() {
        unsafe {
            let mut dib = Dib::new(40, 40).unwrap();
            white(&mut dib);
            let r = RECT {
                left: 10,
                top: 10,
                right: 30,
                bottom: 30,
            };
            buffered_item(dib.dc(), &r, |dc| {
                let brush = CreateSolidBrush(hex(0x102030));
                FillRect(dc, &r, brush);
                DeleteObject(brush);
                let canvas = Canvas::new(dc);
                canvas.fill_circle(20.0, 20.0, 4.0, solid(hex(0xFFFFFF)));
            });
            assert_eq!(rgb(dib.pixel(11, 11)), 0x102030);
            assert_eq!(rgb(dib.pixel(20, 20)), 0xFFFFFF);
            assert_eq!(rgb(dib.pixel(9, 9)), WHITE);
            assert_eq!(rgb(dib.pixel(30, 30)), WHITE);
        }
    }

    #[test]
    fn premultiplied_fill_scales_channels_by_alpha() {
        unsafe {
            let mut dib = Dib::new(4, 4).unwrap();
            dib.fill_premultiplied(argb(hex(0xFF8000), 0.5));
            let pixel = dib.pixel(1, 1);
            assert_eq!(pixel >> 24, 128);
            assert_eq!((pixel >> 16) & 0xff, 128);
            assert_eq!((pixel >> 8) & 0xff, 64);
            assert_eq!(pixel & 0xff, 0);
        }
    }

    #[test]
    fn shadow_math_and_rounded_coverage() {
        assert!(erf(0.0).abs() < 1e-6);
        assert!((erf(1.0) - 0.842_700_8).abs() < 1e-5);
        assert!((erf(-2.0) + 0.995_322_3).abs() < 1e-5);
        assert!((blurred_cover(50.0, 0.0, 100.0, 5.0) - 1.0).abs() < 1e-4);
        assert!((blurred_cover(0.0, 0.0, 100.0, 5.0) - 0.5).abs() < 1e-3);
        assert!(blurred_cover(-20.0, 0.0, 100.0, 5.0) < 1e-4);
        let r = RectF::new(0.0, 0.0, 40.0, 20.0);
        assert_eq!(rounded_rect_coverage(20.5, 10.5, r, 8.0), 1.0);
        assert_eq!(rounded_rect_coverage(0.5, 10.5, r, 8.0), 1.0);
        assert_eq!(rounded_rect_coverage(0.5, 0.5, r, 8.0), 0.0);
        let arc = rounded_rect_coverage(2.5, 2.5, r, 8.0);
        assert!(arc > 0.0 && arc < 1.0, "{arc}");
        let m = shadow_margins(&POPUP_SHADOW);
        assert_eq!(
            m,
            Margins {
                left: 64,
                top: 40,
                right: 64,
                bottom: 88
            }
        );
        assert_eq!(shadow_margins(&popup_shadow(144)).bottom, 132);
        assert_eq!(shadow_margins(&[]), Margins::default());
    }

    #[test]
    fn layered_surface_is_premultiplied_with_a_soft_shadow() {
        unsafe {
            let layers = popup_shadow(96);
            let mut surface = LayeredSurface::new(200, 120, &layers).unwrap();
            assert_eq!(surface.size().cx, 200 + 128);
            assert_eq!(surface.size().cy, 120 + 128);
            let content = surface.content();
            surface.fill_frame(hex(0xFFFFFF), hex(0xD9DFE3), 8.0, 1.0);
            let text = [b'H' as u16, b'i' as u16];
            TextOutW(
                surface.dc(),
                content.left + 20,
                content.top + 20,
                text.as_ptr(),
                2,
            );
            surface.compose(8.0, hex(0x121C23));
            let (w, _) = (surface.size().cx, surface.size().cy);
            let pixels = surface.dib().pixels().to_vec();
            let at = |x: i32, y: i32| pixels[(y * w + x) as usize];
            for &p in &pixels {
                let a = p >> 24;
                assert!((p >> 16) & 0xff <= a && (p >> 8) & 0xff <= a && p & 0xff <= a);
            }
            // Opaque interior (even under GDI text), surface color inside.
            let center = at(content.left + 100, content.top + 60);
            assert_eq!(center, 0xffff_ffff);
            assert_eq!(at(content.left + 21, content.top + 21) >> 24, 255);
            // Straight border edge is fully opaque border color.
            assert_eq!(at(content.left + 100, content.top), 0xffd9_dfe3);
            // The outer corner pixel is cut by the radius.
            assert!(at(content.left, content.top) >> 24 < 128);
            // Shadow: faint, larger below than above, and gone far away.
            let below = at(content.left + 100, content.bottom + 20) >> 24;
            let above = at(content.left + 100, content.top - 20) >> 24;
            assert!(below > above, "below {below} above {above}");
            assert!(below > 0 && below < 60, "{below}");
            assert_eq!(at(0, 0) >> 24, 0);
            // It presents on a (hidden) layered window.
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOOLWINDOW,
                windows_sys::w!("Static"),
                null(),
                WS_POPUP,
                0,
                0,
                10,
                10,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!hwnd.is_null());
            assert!(surface.present(hwnd, POINT { x: 100, y: 100 }, 200));
            DestroyWindow(hwnd);
        }
        assert_eq!(live_objects(), 0);
    }

    /// Popups with native children (palette) keep an opaque window and get a
    /// click-through, non-activating shadow companion right behind them:
    /// transparent inside the box, the soft shadow outside it.
    #[test]
    fn shadow_window_sits_behind_an_opaque_popup() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindow, GetWindowLongW, SendMessageW, ShowWindow, GWL_EXSTYLE, GW_HWNDNEXT,
            HTTRANSPARENT, SW_SHOWNOACTIVATE, WM_NCHITTEST, WS_EX_NOACTIVATE, WS_EX_TRANSPARENT,
        };
        unsafe {
            let popup = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                windows_sys::w!("Static"),
                null(),
                WS_POPUP,
                -12000,
                -12000,
                300,
                200,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!popup.is_null());
            ShowWindow(popup, SW_SHOWNOACTIVATE);
            let _ = round_popup(popup, hex(0xD9DFE3), false);
            let mut shadow = ShadowWindow::new(null_mut()).unwrap();
            let style = GetWindowLongW(shadow.hwnd(), GWL_EXSTYLE) as u32;
            assert_ne!(style & WS_EX_LAYERED, 0);
            assert_ne!(style & WS_EX_TRANSPARENT, 0);
            assert_ne!(style & WS_EX_NOACTIVATE, 0);
            assert_eq!(
                SendMessageW(shadow.hwnd(), WM_NCHITTEST, 0, 0),
                HTTRANSPARENT as isize
            );
            let content = RECT {
                left: -12000,
                top: -12000,
                right: -11700,
                bottom: -11800,
            };
            assert!(shadow.show(popup, content, 96, 8.0, hex(0x121C23), 255));
            assert_eq!(
                GetWindow(popup, GW_HWNDNEXT),
                shadow.hwnd(),
                "directly below the popup"
            );
            let surface = shadow.surface().unwrap();
            let inner = surface.content();
            let w = surface.size().cx;
            let pixels = surface.dib().pixels().to_vec();
            let at = |x: i32, y: i32| pixels[(y * w + x) as usize];
            assert_eq!(at(inner.left + 150, inner.top + 100), 0, "clear inside");
            assert_eq!(at(inner.left + 150, inner.top) >> 24, 0);
            let below = at(inner.left + 150, inner.bottom + 20) >> 24;
            assert!(below > 0 && below < 60, "{below}");
            // Outside the rounded corner the shadow shows through.
            assert!(at(inner.left, inner.top) >> 24 > 0);
            shadow.hide();
            drop(shadow);
            DestroyWindow(popup);
        }
        assert_eq!(live_objects(), 0);
    }

    #[test]
    fn repeated_drawing_releases_every_gdiplus_object() {
        unsafe {
            let before = live_objects();
            let mut back = BackBuffer::new();
            for i in 0..200 {
                let (dc, _) = back.prepare(64, 64).unwrap();
                let canvas = Canvas::new(dc);
                canvas.fill_round_rect(RectF::new(2.0, 2.0, 40.0, 30.0), 4.0, solid(i));
                canvas.stroke_circle(30.0, 30.0, 10.0, 1.5, argb(0, 0.4));
                canvas.polyline(&[(0.0, 0.0), (10.0, 5.0), (20.0, 1.0)], 1.5, solid(0));
                let path = Path::rounded_rect(RectF::new(0.0, 0.0, 20.0, 20.0), Radii::top(8.0));
                canvas.fill_path(&path, solid(0));
                assert!(live_objects() > before);
            }
            assert_eq!(live_objects(), before);
        }
    }
}
