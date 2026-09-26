//! Pixel-smooth scrolling and the Fluent overlay scrollbar (DESIGN_SPEC §4
//! "Scrolling" / "Scrollbar", §6 motion), reusable by any custom list.
//!
//! * [`Extent`] and the wheel helpers are pure math (clamping, revealing a
//!   row below a sticky header, wheel notches → pixels).
//! * [`Scroller`] drives an [`AnimHost`]: the scroll offset eases toward an
//!   accumulated target (`motion::SCROLL`, ease-out), the bar fades in on
//!   activity and out after `motion::SCROLLBAR_IDLE` (a one-shot delay, no
//!   frame timer while idle), grows from the 3 px indicator to the 8 px bar
//!   with a faint track while the pointer is in the right 14 px zone, and
//!   supports thumb dragging and track paging. It never runs a timer unless
//!   something moves.
use super::anim::{motion, AnimHost, Easing};
use super::widgets::{self, Painter};
use std::hash::Hash;
use std::time::Duration;
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETWHEELSCROLLLINES, WHEEL_DELTA,
};

/// Width of the pointer zone at the view's right edge (CSS px).
pub(super) const ZONE_DIP: f32 = 14.0;
/// Precision-touchpad deltas (not whole notches) ease over this short time:
/// proportional scrolling with light smoothing.
pub(super) const TOUCHPAD: Duration = Duration::from_millis(70);
/// `WHEEL_PAGESCROLL`: the user set "one screen at a time".
pub(super) const WHEEL_PAGE: u32 = u32::MAX;

/// A scroll view's content and viewport lengths (device px).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct Extent {
    pub content: f32,
    pub view: f32,
}

impl Extent {
    pub(super) fn new(content: f32, view: f32) -> Self {
        Self {
            content: content.max(0.0),
            view: view.max(0.0),
        }
    }
    /// The largest valid offset.
    pub(super) fn max(&self) -> f32 {
        (self.content - self.view).max(0.0)
    }
    pub(super) fn clamp(&self, offset: f32) -> f32 {
        if offset.is_finite() {
            offset.clamp(0.0, self.max())
        } else {
            0.0
        }
    }
    /// Anything to scroll (and so a scrollbar to show)?
    pub(super) fn scrollable(&self) -> bool {
        self.view > 0.0 && self.content > self.view + 0.5
    }
    /// The offset nearest to `offset` at which content `top..bottom` (e.g. a
    /// row, in content coordinates below the sticky header) is fully
    /// visible; an item taller than the view shows its top.
    pub(super) fn reveal(&self, offset: f32, top: f32, bottom: f32) -> f32 {
        let wanted = if bottom - top >= self.view || top < offset {
            top
        } else if bottom > offset + self.view {
            bottom - self.view
        } else {
            offset
        };
        self.clamp(wanted)
    }
    /// One page (track click / Page Up): the view minus one line of context.
    pub(super) fn page(&self, line: f32) -> f32 {
        (self.view - line).max(line.min(self.view)).max(1.0)
    }
}

/// `SPI_GETWHEELSCROLLLINES` (3 when unavailable; [`WHEEL_PAGE`] = a page).
pub(super) fn wheel_lines() -> u32 {
    let mut lines: u32 = 3;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETWHEELSCROLLLINES,
            0,
            (&mut lines as *mut u32).cast(),
            0,
        )
    } != 0;
    if ok {
        lines
    } else {
        3
    }
}

/// Offset change (device px) for a raw wheel `delta` (positive = away from
/// the user = toward the top): `lines × line_px` per 120, a page per notch
/// with [`WHEEL_PAGE`]; small precision-touchpad deltas scale linearly.
pub(super) fn wheel_pixels(delta: i32, lines: u32, line_px: f32, page_px: f32) -> f32 {
    let notches = delta as f32 / WHEEL_DELTA as f32;
    let per_notch = if lines == WHEEL_PAGE {
        page_px
    } else {
        lines as f32 * line_px
    };
    -notches * per_notch
}

/// Whole notches get the design's 160 ms ease-out; touchpad deltas a short
/// smoothing so the content tracks the fingers.
pub(super) fn wheel_duration(delta: i32) -> Duration {
    if delta != 0 && delta % WHEEL_DELTA as i32 == 0 {
        motion::SCROLL
    } else {
        TOUCHPAD
    }
}

/// Where a pointer is relative to the scrollbar lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BarHit {
    /// Outside the lane (or nothing to scroll).
    None,
    Thumb,
    /// Track above the thumb (page up).
    Before,
    /// Track below the thumb (page down).
    After,
}

/// The pointer zone (`ZONE_DIP` wide) at the right edge of `view`.
pub(super) fn lane(view: RECT, dpi: i32) -> RECT {
    let zone = super::gfx::pxi(dpi, ZONE_DIP);
    RECT {
        left: (view.right - zone).max(view.left),
        ..view
    }
}

fn contains(r: &RECT, pt: POINT) -> bool {
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

/// Scroll position + overlay scrollbar of one view. `K` = the host's key
/// type; the three keys are the animated offset (device px), the bar's
/// opacity and its hover growth (0 = 3 px indicator, 1 = 8 px bar).
pub(super) struct Scroller<K> {
    offset: K,
    opacity: K,
    grow: K,
    extent: Extent,
    /// Pointer inside the lane.
    hover: bool,
    /// Thumb drag in progress: the grab point inside the thumb (px).
    drag: Option<f32>,
}

impl<K: Copy + Eq + Hash> Scroller<K> {
    pub(super) fn new(offset: K, opacity: K, grow: K) -> Self {
        Self {
            offset,
            opacity,
            grow,
            extent: Extent::default(),
            hover: false,
            drag: None,
        }
    }
    pub(super) fn extent(&self) -> Extent {
        self.extent
    }
    /// The offset on screen now (device px).
    pub(super) fn offset(&self, host: &AnimHost<K>) -> f32 {
        host.value(self.offset)
    }
    /// Where the offset is heading (equals [`Scroller::offset`] when idle).
    pub(super) fn target(&self, host: &AnimHost<K>) -> f32 {
        host.anim.target(self.offset).unwrap_or(0.0)
    }
    pub(super) fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }
    /// New content / view lengths. The position is kept (a list rebuild never
    /// jumps), clamped without animation when the content got shorter than
    /// the position; a glide heading past the new end keeps gliding, to the
    /// new end (a row that disappears mid-wheel does not freeze it).
    pub(super) fn set_extent(&mut self, host: &mut AnimHost<K>, extent: Extent) {
        self.extent = extent;
        let max = extent.max();
        let (value, target) = (self.offset(host), self.target(host));
        if !value.is_finite() {
            host.set(self.offset, 0.0);
        } else if value > max {
            host.set(self.offset, max);
        } else if target > max {
            // Retargeting continues from the sampled value (continuous).
            host.set_target(self.offset, max, motion::SCROLL, Easing::EaseOut);
        }
        if !extent.scrollable() {
            self.drag = None;
            host.set(self.opacity, 0.0);
            host.set(self.grow, 0.0);
        }
    }
    /// Rescale the position after a DPI change (device px change with it).
    pub(super) fn rescale(&mut self, host: &mut AnimHost<K>, factor: f32) {
        let value = self.target(host) * factor;
        host.set(self.offset, value.max(0.0));
    }
    /// Scroll to `offset` (clamped): eased over `duration`, or a jump.
    pub(super) fn scroll_to(
        &mut self,
        host: &mut AnimHost<K>,
        offset: f32,
        duration: Option<Duration>,
    ) {
        let target = self.extent.clamp(offset);
        if target == self.target(host) && !host.anim.is_key_animating(self.offset) {
            return;
        }
        match duration {
            // Retargeting keeps the glide's speed (no pulse per notch).
            Some(duration) => {
                host.set_target_smooth(self.offset, target, duration);
            }
            None => host.set(self.offset, target),
        }
        self.activity(host);
    }
    /// A wheel message: the target accumulates while wheeling and the offset
    /// eases toward it from wherever it is (retargeting is continuous).
    pub(super) fn wheel(&mut self, host: &mut AnimHost<K>, delta: i32, line_px: f32) {
        self.wheel_with(host, delta, wheel_lines(), line_px);
    }
    /// [`Scroller::wheel`] with an explicit lines setting (tests).
    pub(super) fn wheel_with(
        &mut self,
        host: &mut AnimHost<K>,
        delta: i32,
        lines: u32,
        line_px: f32,
    ) {
        if !self.extent.scrollable() || delta == 0 {
            return;
        }
        let page = self.extent.page(line_px);
        let pixels = wheel_pixels(delta, lines, line_px, page);
        let target = self.target(host) + pixels;
        self.scroll_to(host, target, Some(wheel_duration(delta)));
    }
    /// Make content `top..bottom` visible. Short distances ease (keyboard
    /// navigation), long ones jump (a revealed search result).
    pub(super) fn reveal(&mut self, host: &mut AnimHost<K>, top: f32, bottom: f32, animate: bool) {
        let target = self.target(host);
        let wanted = self.extent.reveal(target, top, bottom);
        if wanted == target {
            return;
        }
        let far = (wanted - self.offset(host)).abs() > self.extent.view;
        let duration = (animate && !far).then_some(motion::SCROLL);
        self.scroll_to(host, wanted, duration);
    }
    /// Page toward the top (`-1`) or bottom (`1`), eased.
    pub(super) fn page(&mut self, host: &mut AnimHost<K>, direction: f32, line_px: f32) {
        let step = self.extent.page(line_px) * direction.signum();
        let target = self.target(host) + step;
        self.scroll_to(host, target, Some(motion::SCROLL));
    }
    /// Show the bar (100 ms fade-in) and, unless it is held by hover or a
    /// drag, fade it out again after the idle time (200 ms).
    pub(super) fn activity(&mut self, host: &mut AnimHost<K>) {
        if !self.extent.scrollable() {
            return;
        }
        host.set_target(
            self.opacity,
            1.0,
            motion::SCROLLBAR_FADE_IN,
            Easing::EaseOut,
        );
        if self.hover || self.drag.is_some() {
            host.cancel_delayed(self.opacity);
        } else {
            host.set_target_after(
                self.opacity,
                0.0,
                motion::SCROLLBAR_IDLE,
                motion::SCROLLBAR_FADE_OUT,
                Easing::EaseOut,
            );
        }
    }
    fn set_grow(&mut self, host: &mut AnimHost<K>) {
        let target = (self.hover || self.drag.is_some()) as u8 as f32;
        host.set_target(self.grow, target, motion::SCROLLBAR_GROW, Easing::EaseOut);
    }
    /// The thumb `(top, length)` relative to `lane.top` at the current offset.
    pub(super) fn thumb(&self, host: &AnimHost<K>, lane: RECT, min_len: f32) -> (f32, f32) {
        widgets::scroll_thumb(
            (lane.bottom - lane.top) as f32,
            self.extent.view,
            self.extent.content,
            self.offset(host),
            min_len,
        )
    }
    /// What the pointer at `pt` would hit.
    pub(super) fn hit(&self, host: &AnimHost<K>, lane: RECT, pt: POINT, min_len: f32) -> BarHit {
        if !self.extent.scrollable() || !contains(&lane, pt) {
            return BarHit::None;
        }
        let (top, len) = self.thumb(host, lane, min_len);
        let y = (pt.y - lane.top) as f32;
        if y < top {
            BarHit::Before
        } else if y >= top + len {
            BarHit::After
        } else {
            BarHit::Thumb
        }
    }
    /// Pointer moved to `pt` (None: it left the view). The bar appears on
    /// hover of the lane (it grows to 8 px and stays while hovered); leaving
    /// the lane starts the idle fade-out. Returns whether the pointer is in
    /// the lane (the view should not hover its rows there).
    pub(super) fn pointer(
        &mut self,
        host: &mut AnimHost<K>,
        lane: RECT,
        pt: Option<POINT>,
    ) -> bool {
        let inside = self.extent.scrollable() && pt.is_some_and(|pt| contains(&lane, pt));
        if inside != self.hover {
            self.hover = inside;
            self.set_grow(host);
            self.activity(host);
        }
        inside
    }
    /// Button down at `pt`: grab the thumb or page the track. Returns true
    /// when the scrollbar took the click (capture the mouse then).
    pub(super) fn press(
        &mut self,
        host: &mut AnimHost<K>,
        lane: RECT,
        pt: POINT,
        min_len: f32,
        line_px: f32,
    ) -> bool {
        match self.hit(host, lane, pt, min_len) {
            BarHit::None => return false,
            BarHit::Thumb => {
                let (top, _) = self.thumb(host, lane, min_len);
                // Stop any running glide so the thumb stays under the pointer.
                let now = self.offset(host);
                host.set(self.offset, now);
                self.drag = Some((pt.y - lane.top) as f32 - top);
            }
            BarHit::Before => self.page(host, -1.0, line_px),
            BarHit::After => self.page(host, 1.0, line_px),
        }
        self.set_grow(host);
        self.activity(host);
        true
    }
    /// Pointer moved while the button is down; drags the thumb (the offset
    /// follows immediately). Returns true while dragging.
    pub(super) fn drag_to(
        &mut self,
        host: &mut AnimHost<K>,
        lane: RECT,
        pt: POINT,
        min_len: f32,
    ) -> bool {
        let Some(grab) = self.drag else {
            return false;
        };
        let top = (pt.y - lane.top) as f32 - grab;
        let offset = widgets::scroll_offset_for_thumb(
            (lane.bottom - lane.top) as f32,
            self.extent.view,
            self.extent.content,
            top,
            min_len,
        );
        let offset = self.extent.clamp(offset);
        if offset != self.offset(host) {
            host.set(self.offset, offset);
        }
        true
    }
    /// Button released (or capture lost).
    pub(super) fn release(&mut self, host: &mut AnimHost<K>) {
        if self.drag.take().is_some() {
            self.set_grow(host);
            self.activity(host);
        }
    }
    /// Force the bar's look (previews): opacity and growth, no fade pending.
    pub(super) fn stage(&mut self, host: &mut AnimHost<K>, opacity: f32, grow: f32) {
        host.cancel_delayed(self.opacity);
        host.set(self.opacity, opacity);
        host.set(self.grow, grow);
    }
    /// Draw the overlay bar over the content in `lane`.
    pub(super) fn paint(&self, pt: &Painter, host: &AnimHost<K>, lane: RECT) {
        if !self.extent.scrollable() {
            return;
        }
        let opacity = host.value(self.opacity);
        if opacity <= 0.0 {
            return;
        }
        let thumb = self.thumb(host, lane, pt.px(widgets::SCROLL_THUMB_MIN));
        widgets::scrollbar(pt, lane, thumb, host.value(self.grow), opacity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::anim::Animator;
    use std::time::Instant;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    enum K {
        Offset,
        Opacity,
        Grow,
    }

    fn host() -> AnimHost<K> {
        let mut host = AnimHost::new(0x7777);
        host.anim = Animator::new().with_reduced_motion(false);
        host
    }

    fn scroller(content: f32, view: f32, host: &mut AnimHost<K>) -> Scroller<K> {
        let mut s = Scroller::new(K::Offset, K::Opacity, K::Grow);
        s.set_extent(host, Extent::new(content, view));
        s
    }

    #[test]
    fn extent_clamps_and_reveals_rows_below_a_sticky_header() {
        let e = Extent::new(1000.0, 300.0);
        assert_eq!(e.max(), 700.0);
        assert_eq!(e.clamp(-5.0), 0.0);
        assert_eq!(e.clamp(900.0), 700.0);
        assert_eq!(e.clamp(f32::NAN), 0.0);
        assert!(e.scrollable());
        assert!(!Extent::new(300.0, 300.0).scrollable());
        assert_eq!(Extent::new(100.0, 300.0).max(), 0.0);
        // Content coordinates start below the header: a row just under the
        // view's bottom edge scrolls up by exactly its overflow.
        assert_eq!(e.reveal(0.0, 290.0, 324.0), 24.0);
        // Above the view: its top aligns with the top of the view (under the header).
        assert_eq!(e.reveal(400.0, 340.0, 374.0), 340.0);
        // Already visible: unchanged.
        assert_eq!(e.reveal(100.0, 150.0, 184.0), 100.0);
        // Rows near the end never scroll past the last valid offset.
        assert_eq!(e.reveal(0.0, 966.0, 1000.0), 700.0);
        // Taller than the view: its top.
        assert_eq!(Extent::new(1000.0, 30.0).reveal(0.0, 100.0, 134.0), 100.0);
        assert_eq!(e.page(34.0), 266.0);
    }

    #[test]
    fn wheel_notches_map_to_lines_of_rows_and_touchpads_scale() {
        assert_eq!(wheel_pixels(-120, 3, 34.0, 266.0), 102.0);
        assert_eq!(wheel_pixels(240, 3, 34.0, 266.0), -204.0);
        assert_eq!(wheel_pixels(-120, WHEEL_PAGE, 34.0, 266.0), 266.0);
        // A precision touchpad's 12-unit delta is a tenth of a notch.
        assert!((wheel_pixels(-12, 3, 34.0, 266.0) - 10.2).abs() < 1e-4);
        assert_eq!(wheel_duration(-120), motion::SCROLL);
        assert_eq!(wheel_duration(-240), motion::SCROLL);
        assert_eq!(wheel_duration(-12), TOUCHPAD);
    }

    #[test]
    fn wheel_accumulates_eases_and_clamps_at_both_ends() {
        let mut h = host();
        let mut s = scroller(1000.0, 300.0, &mut h);
        let t0 = Instant::now();
        s.wheel_with(&mut h, -120, 3, 34.0);
        assert_eq!(s.target(&h), 102.0);
        assert_eq!(s.offset(&h), 0.0, "eases from where it is");
        // A second notch before the first settles accumulates the target.
        s.wheel_with(&mut h, -120, 3, 34.0);
        assert_eq!(s.target(&h), 204.0);
        h.tick_at(t0 + Duration::from_millis(60));
        let mid = s.offset(&h);
        assert!(mid > 0.0 && mid < 204.0, "{mid}");
        h.tick_at(t0 + Duration::from_secs(1));
        assert_eq!(s.offset(&h), 204.0);
        for _ in 0..20 {
            s.wheel_with(&mut h, -120, 3, 34.0);
        }
        assert_eq!(s.target(&h), 700.0, "clamped at the end");
        for _ in 0..40 {
            s.wheel_with(&mut h, 120, 3, 34.0);
        }
        assert_eq!(s.target(&h), 0.0, "clamped at the top");
        // Shrinking content clamps the position without animation.
        h.finish_all();
        s.scroll_to(&mut h, 600.0, None);
        s.set_extent(&mut h, Extent::new(500.0, 300.0));
        assert_eq!(s.offset(&h), 200.0);
        assert_eq!(s.target(&h), 200.0);
        // Nothing to scroll: wheel does nothing, the bar hides.
        s.set_extent(&mut h, Extent::new(200.0, 300.0));
        assert_eq!(s.offset(&h), 0.0);
        s.wheel_with(&mut h, -120, 3, 34.0);
        assert_eq!(s.offset(&h), 0.0);
        assert_eq!(h.value(K::Opacity), 0.0);
    }

    #[test]
    fn a_glide_toward_the_end_retargets_when_the_content_shrinks() {
        let mut h = host();
        let mut s = scroller(1000.0, 300.0, &mut h);
        // A slow glide (real time: retargeting samples the tween at `now`).
        s.scroll_to(&mut h, 700.0, Some(Duration::from_secs(10)));
        std::thread::sleep(Duration::from_millis(30));
        h.tick_at(Instant::now());
        let mid = s.offset(&h);
        assert!(mid > 0.0 && mid < 666.0, "{mid}");
        // One 34 px row disappears mid-glide (a process exited): the glide
        // continues to the new end instead of freezing where it was.
        s.set_extent(&mut h, Extent::new(966.0, 300.0));
        assert_eq!(s.target(&h), 666.0);
        assert!(h.anim.is_key_animating(K::Offset));
        let now = s.offset(&h);
        assert!(now >= mid && now < 666.0, "continuous: {mid} → {now}");
        h.tick_at(Instant::now() + Duration::from_secs(1));
        assert_eq!(s.offset(&h), 666.0);
        // Content shorter than the current position: an immediate clamp.
        s.set_extent(&mut h, Extent::new(700.0, 300.0));
        assert_eq!(s.offset(&h), 400.0);
        assert!(!h.anim.is_key_animating(K::Offset));
    }

    #[test]
    fn reveal_eases_short_distances_and_jumps_far_ones() {
        let mut h = host();
        let mut s = scroller(10_000.0, 300.0, &mut h);
        s.reveal(&mut h, 290.0, 324.0, true);
        assert_eq!(s.target(&h), 24.0);
        assert!(h.anim.is_key_animating(K::Offset));
        h.finish_all();
        s.reveal(&mut h, 5000.0, 5034.0, true);
        assert_eq!(s.offset(&h), 4734.0, "far reveals jump");
        s.reveal(&mut h, 4800.0, 4834.0, false);
        assert_eq!(s.offset(&h), 4734.0, "visible rows do not scroll");
        assert!(!h.anim.is_key_animating(K::Offset));
    }

    #[test]
    fn thumb_drag_and_track_paging_follow_the_pointer() {
        let mut h = host();
        let mut s = scroller(1000.0, 300.0, &mut h);
        let lane = RECT {
            left: 286,
            top: 34,
            right: 300,
            bottom: 334,
        };
        // Thumb: 300 × 300/1000 = 90 px at the top.
        assert_eq!(s.thumb(&h, lane, 24.0), (0.0, 90.0));
        let at = |x: i32, y: i32| POINT { x, y };
        assert_eq!(s.hit(&h, lane, at(280, 40), 24.0), BarHit::None);
        assert_eq!(s.hit(&h, lane, at(290, 40), 24.0), BarHit::Thumb);
        assert_eq!(s.hit(&h, lane, at(290, 200), 24.0), BarHit::After);
        // Hovering the lane grows the bar and holds it visible.
        assert!(s.pointer(&mut h, lane, Some(at(292, 200))));
        h.finish_all();
        assert_eq!((h.value(K::Opacity), h.value(K::Grow)), (1.0, 1.0));
        assert!(!h.is_delayed(K::Opacity), "no fade-out while hovered");
        // Track click pages down (view − one line), eased.
        assert!(s.press(&mut h, lane, at(292, 200), 24.0, 34.0));
        assert_eq!(s.target(&h), 266.0);
        h.finish_all();
        assert_eq!(s.hit(&h, lane, at(290, 40), 24.0), BarHit::Before);
        // Drag: grab the thumb's top edge and move it 105 px (half the travel).
        let (top, _) = s.thumb(&h, lane, 24.0);
        let grab_y = lane.top + top.round() as i32;
        assert!(s.press(&mut h, lane, at(292, grab_y), 24.0, 34.0));
        assert!(s.is_dragging());
        assert!(s.drag_to(&mut h, lane, at(292, lane.top + 105), 24.0));
        assert!((s.offset(&h) - 350.0).abs() < 3.0, "{}", s.offset(&h));
        assert!(s.drag_to(&mut h, lane, at(292, 5000), 24.0));
        assert_eq!(s.offset(&h), 700.0, "dragging clamps");
        s.release(&mut h);
        assert!(!s.is_dragging());
        // Leaving schedules the idle fade-out (a one-shot delay, no frames).
        assert!(!s.pointer(&mut h, lane, None));
        assert!(h.is_delayed(K::Opacity));
        h.finish_all();
        assert_eq!((h.value(K::Opacity), h.value(K::Grow)), (0.0, 0.0));
    }
}
