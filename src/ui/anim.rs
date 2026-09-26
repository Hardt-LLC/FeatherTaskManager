//! On-demand tweens (DESIGN_SPEC §6). Nothing here runs a render loop:
//! [`AnimHost`] starts a window timer only while some key is animating and
//! kills it as soon as every tween has settled. Data updates never tween.
//!
//! Usage from any window procedure (main window, custom table, popup):
//! ```ignore
//! // WM_CREATE:          host.attach(hwnd);
//! // on hover enter:     host.set_target(key, 1.0, motion::HOVER_IN, Easing::EaseOut);
//! // WM_TIMER:           if host.on_timer(wparam) { return 0; }
//! // WM_PAINT:           let hover = host.value(key);
//! // WM_SETTINGCHANGE:   anim::refresh_reduced_motion();
//! // WM_DESTROY:         host.stop();
//! ```
#![allow(dead_code)] // Driver API consumed by the Frame/Controls/Table tracks.

use std::collections::HashMap;
use std::hash::Hash;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    InvalidateRect, RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    KillTimer, SetTimer, SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION,
};

/// Motion tokens (DESIGN_SPEC §6).
pub(super) mod motion {
    use std::time::Duration;
    const fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }
    /// Hover backgrounds fade in (90–120 ms ease-out) ...
    pub const HOVER_IN: Duration = ms(100);
    /// ... and out (150 ms).
    pub const HOVER_OUT: Duration = ms(150);
    /// Switch knob slide + color cross-fade (`ease`).
    pub const SWITCH: Duration = ms(120);
    /// Tree chevron rotation 0° → 90° (`ease`).
    pub const CHEVRON: Duration = ms(120);
    /// Nav current indicator slide/stretch (ease-out).
    pub const NAV: Duration = ms(180);
    /// Wheel scroll toward the accumulated target (ease-out).
    pub const SCROLL: Duration = ms(160);
    pub const SCROLLBAR_FADE_IN: Duration = ms(100);
    pub const SCROLLBAR_GROW: Duration = ms(120);
    pub const SCROLLBAR_FADE_OUT: Duration = ms(200);
    /// Idle time before the overlay scrollbar fades out.
    pub const SCROLLBAR_IDLE: Duration = ms(1000);
    /// Popups, menus, dropdowns, dialog, palette: fade + 4 px slide.
    pub const POPUP_IN: Duration = ms(120);
    pub const POPUP_OUT: Duration = ms(80);
    /// Popup entrance slide distance in DIP.
    pub const POPUP_SLIDE_DIP: f32 = 4.0;
    pub const TOAST_IN: Duration = ms(150);
    pub const TOAST_OUT: Duration = ms(200);
    pub const TOAST_HOLD: Duration = ms(2600);
}

/// Shared `part` numbers for `(control_id, part)` keys of the main window's
/// [`AnimHost`], so tracks never collide. Append new parts at the end.
pub(super) mod part {
    /// 0..=1 hover amount of a control.
    pub const HOVER: u32 = 0;
    /// 0..=1 checked progress of a switch.
    pub const SWITCH: u32 = 1;
    /// Chevron angle (degrees) of a tree row toggle.
    pub const CHEVRON: u32 = 2;
    /// Reserved (superseded by [`NAV_TOP`] / [`NAV_BOTTOM`]: a stretching
    /// indicator animates its two edges, not a position and a height).
    pub const NAV_Y: u32 = 3;
    /// Reserved, see [`NAV_Y`].
    pub const NAV_H: u32 = 4;
    /// Caption button hover, key `(super::CAPTION_ID + n, CAPTION)` with
    /// n = 0 minimize, 1 maximize/restore, 2 close.
    pub const CAPTION: u32 = 5;
    /// Scroll offset (device px).
    pub const SCROLL: u32 = 6;
    /// Scrollbar opacity / width.
    pub const SCROLLBAR: u32 = 7;
    pub const SCROLLBAR_WIDTH: u32 = 8;
    /// Popup / dialog / toast opacity.
    pub const OPACITY: u32 = 9;
    /// Nav current indicator top edge (client device px), key `(super::NAV_ID, NAV_TOP)`.
    pub const NAV_TOP: u32 = 10;
    /// Nav current indicator bottom edge (client device px), key `(super::NAV_ID, NAV_BOTTOM)`.
    pub const NAV_BOTTOM: u32 = 11;
    /// 0..=1 "current / selected" amount of a control (nav bg cross-fade).
    pub const SELECTED: u32 = 12;
}

/// Key ids of main-window animations that belong to no child control (keep
/// them clear of control ids, which are < 0x1000).
pub(super) const CAPTION_ID: usize = 0x1000;
/// The sliding nav indicator (one per rail).
pub(super) const NAV_ID: usize = 0x1100;

/// Timing functions. `Ease` = CSS `ease` = cubic-bezier(.25,.1,.25,1);
/// `EaseOut` = cubic-bezier(0,0,.2,1) as pinned by the spec.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Easing {
    Linear,
    Ease,
    EaseOut,
    Bezier(f32, f32, f32, f32),
}

impl Easing {
    /// Map linear progress `t` (clamped to 0..=1) to eased progress.
    pub(super) fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Easing::Linear => t,
            Easing::Ease => cubic_bezier(0.25, 0.1, 0.25, 1.0, t),
            Easing::EaseOut => cubic_bezier(0.0, 0.0, 0.2, 1.0, t),
            Easing::Bezier(x1, y1, x2, y2) => cubic_bezier(x1, y1, x2, y2, t),
        }
    }
}

/// CSS cubic-bezier timing function (WebKit UnitBezier: Newton, then bisection).
pub(super) fn cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let (x1, y1, x2, y2, x) = (x1 as f64, y1 as f64, x2 as f64, y2 as f64, x as f64);
    let cx = 3.0 * x1;
    let bx = 3.0 * (x2 - x1) - cx;
    let ax = 1.0 - cx - bx;
    let cy = 3.0 * y1;
    let by = 3.0 * (y2 - y1) - cy;
    let ay = 1.0 - cy - by;
    let sample_x = |t: f64| ((ax * t + bx) * t + cx) * t;
    let sample_dx = |t: f64| (3.0 * ax * t + 2.0 * bx) * t + cx;
    let sample_y = |t: f64| ((ay * t + by) * t + cy) * t;
    let mut t = x;
    for _ in 0..8 {
        let error = sample_x(t) - x;
        if error.abs() < 1e-7 {
            return sample_y(t) as f32;
        }
        let slope = sample_dx(t);
        if slope.abs() < 1e-6 {
            break;
        }
        t -= error / slope;
    }
    let (mut lo, mut hi) = (0.0, 1.0);
    t = x;
    for _ in 0..60 {
        let value = sample_x(t);
        if (value - x).abs() < 1e-7 {
            break;
        }
        if value < x {
            lo = t;
        } else {
            hi = t;
        }
        t = (lo + hi) / 2.0;
    }
    sample_y(t) as f32
}

// 0 = not queried yet, 1 = animations on, 2 = reduced motion.
static MOTION: AtomicU8 = AtomicU8::new(0);

/// True when Windows "Animation effects" is off (SPI_GETCLIENTAREAANIMATION
/// = FALSE): every animation then jumps to its end state.
pub(super) fn reduced_motion() -> bool {
    match MOTION.load(Ordering::Relaxed) {
        0 => refresh_reduced_motion(),
        value => value == 2,
    }
}

/// Re-read the system setting (call on WM_SETTINGCHANGE). Returns the new state.
pub(super) fn refresh_reduced_motion() -> bool {
    let mut enabled: i32 = 1;
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&mut enabled as *mut i32).cast(),
            0,
        )
    } != 0;
    let reduced = ok && enabled == 0;
    MOTION.store(if reduced { 2 } else { 1 }, Ordering::Relaxed);
    reduced
}

#[derive(Clone, Copy, Debug)]
struct Tween {
    from: f32,
    to: f32,
    start: Instant,
    duration: Duration,
    easing: Easing,
    /// A retargeted glide keeps its momentum: the curve is then the cubic
    /// Hermite from `from` leaving at this velocity (units per second) to
    /// `to` at rest ([`Animator::set_target_smooth_at`]).
    velocity: Option<f32>,
    /// The frame timer has advanced it (see [`Animator::catch_up`]).
    ticked: bool,
}

impl Tween {
    fn new(from: f32, to: f32, start: Instant, duration: Duration, easing: Easing) -> Self {
        Self {
            from,
            to,
            start,
            duration,
            easing,
            velocity: None,
            ticked: false,
        }
    }
    fn progress(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(self.start).as_secs_f32() / self.duration.as_secs_f32())
            .clamp(0.0, 1.0)
    }
    fn sample(&self, now: Instant) -> f32 {
        let u = self.progress(now);
        match self.velocity {
            Some(v) => {
                let (u2, u3) = (u * u, u * u * u);
                let t = self.duration.as_secs_f32();
                (2.0 * u3 - 3.0 * u2 + 1.0) * self.from
                    + (u3 - 2.0 * u2 + u) * t * v
                    + (3.0 * u2 - 2.0 * u3) * self.to
            }
            None => self.from + (self.to - self.from) * self.easing.apply(u),
        }
    }
    /// Units per second at `now` (a 4 ms difference inside the tween).
    fn velocity_at(&self, now: Instant) -> f32 {
        let h = Duration::from_millis(4);
        let end = self.start + self.duration;
        let t1 = if now < self.start + h {
            self.start + h
        } else if now > end {
            end.max(self.start + h)
        } else {
            now
        };
        (self.sample(t1) - self.sample(t1 - h)) / h.as_secs_f32()
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    value: f32,
    tween: Option<Tween>,
}

/// Keyed tweens. Keys are small `Copy` values, e.g. `(control_id, part)`.
/// Unknown keys read as 0.0 (use [`Animator::set`] for another start value).
#[derive(Debug)]
pub(super) struct Animator<K> {
    entries: HashMap<K, Entry>,
    reduced: Option<bool>,
}

impl<K: Copy + Eq + Hash> Default for Animator<K> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy + Eq + Hash> Animator<K> {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            reduced: None,
        }
    }
    /// Force reduced motion on/off regardless of the system setting (tests).
    pub(super) fn with_reduced_motion(mut self, reduced: bool) -> Self {
        self.reduced = Some(reduced);
        self
    }
    fn reduced(&self) -> bool {
        self.reduced.unwrap_or_else(reduced_motion)
    }
    /// Jump to `value` (no animation), cancelling any tween of `key`.
    pub(super) fn set(&mut self, key: K, value: f32) {
        self.entries.insert(key, Entry { value, tween: None });
    }
    /// Animate `key` toward `target` starting from its current (possibly
    /// mid-flight) value. Returns true when a tween is now running.
    pub(super) fn set_target(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        easing: Easing,
    ) -> bool {
        self.set_target_at(key, target, duration, easing, Instant::now())
    }
    /// [`Animator::set_target`] with an explicit clock (deterministic tests).
    pub(super) fn set_target_at(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        easing: Easing,
        now: Instant,
    ) -> bool {
        let reduced = self.reduced();
        let entry = self.entries.entry(key).or_insert(Entry {
            value: 0.0,
            tween: None,
        });
        if let Some(tween) = entry.tween {
            if tween.to == target && !reduced {
                return true; // Same destination: keep the running timeline.
            }
            entry.value = tween.sample(now); // Retarget from where it is now.
        }
        if reduced || duration.is_zero() || entry.value == target {
            entry.value = target;
            entry.tween = None;
            return false;
        }
        entry.tween = Some(Tween::new(entry.value, target, now, duration, easing));
        true
    }
    /// [`Animator::set_target_at`] for glides (scrolling): a running tween is
    /// retargeted *without a velocity jump* — the new curve leaves the current
    /// position at the current speed and comes to rest on `target` (like
    /// Chromium's `ScrollOffsetAnimationCurve::UpdateTarget`), so wheel
    /// notches in quick succession accelerate one continuous glide instead
    /// of restarting an ease-out each time. A glide reversing direction
    /// starts from rest; the speed is capped so the glide never overshoots.
    /// A key at rest starts with `EaseOut`.
    pub(super) fn set_target_smooth_at(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        now: Instant,
    ) -> bool {
        let reduced = self.reduced();
        let entry = self.entries.entry(key).or_insert(Entry {
            value: 0.0,
            tween: None,
        });
        let mut velocity = None;
        if let Some(tween) = entry.tween {
            if tween.to == target && !reduced {
                return true;
            }
            let speed = tween.velocity_at(now);
            entry.value = tween.sample(now);
            let span = target - entry.value;
            let seconds = duration.as_secs_f32();
            if span != 0.0 && seconds > 0.0 && speed * span > 0.0 {
                // The Hermite with a resting end is monotonic up to 3·span/T.
                let limit = 3.0 * span.abs() / seconds;
                velocity = Some(speed.signum() * speed.abs().min(limit));
            }
        }
        if reduced || duration.is_zero() || entry.value == target {
            entry.value = target;
            entry.tween = None;
            return false;
        }
        let mut tween = Tween::new(entry.value, target, now, duration, Easing::EaseOut);
        tween.velocity = velocity;
        entry.tween = Some(tween);
        true
    }
    /// Restart a running tween's timeline at `now` (from the value it had at
    /// its start): work that ran between setting a target and the first frame
    /// (a page switch) does not eat into the motion.
    pub(super) fn rebase(&mut self, key: K, now: Instant) {
        if let Some(tween) = self.entries.get_mut(&key).and_then(|e| e.tween.as_mut()) {
            tween.start = now;
            tween.ticked = false;
        }
    }
    /// Before the first frame of a new tween: when the UI thread was busy
    /// longer than `lag` since it started, start it `lag` ago instead, so the
    /// first presented frame is near its beginning, not 20-30 % into it.
    pub(super) fn catch_up(&mut self, now: Instant, lag: Duration) {
        for tween in self.entries.values_mut().filter_map(|e| e.tween.as_mut()) {
            if !tween.ticked {
                tween.ticked = true;
                if now.saturating_duration_since(tween.start) > lag {
                    tween.start = now - lag;
                }
            }
        }
    }
    /// State-driven animation: the first call for a key jumps to `target`
    /// (no animation on first paint); later calls animate toward a changed
    /// target. Returns the current value. Ideal inside paint/draw handlers.
    pub(super) fn follow(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        easing: Easing,
    ) -> f32 {
        match self.target(key) {
            None => self.set(key, target),
            Some(current) if current != target => {
                self.set_target(key, target, duration, easing);
            }
            Some(_) => {}
        }
        self.value(key)
    }
    /// The value as of the last `tick` (or `set_target`), 0.0 for unknown keys.
    pub(super) fn value(&self, key: K) -> f32 {
        self.value_or(key, 0.0)
    }
    pub(super) fn value_or(&self, key: K, default: f32) -> f32 {
        self.entries.get(&key).map_or(default, |entry| entry.value)
    }
    /// Sample a key at an arbitrary instant without mutating state.
    pub(super) fn value_at(&self, key: K, now: Instant) -> f32 {
        self.entries.get(&key).map_or(0.0, |entry| {
            entry.tween.map_or(entry.value, |tween| tween.sample(now))
        })
    }
    /// The destination of a key (its value when it is not animating).
    pub(super) fn target(&self, key: K) -> Option<f32> {
        self.entries
            .get(&key)
            .map(|entry| entry.tween.map_or(entry.value, |tween| tween.to))
    }
    pub(super) fn is_animating(&self) -> bool {
        self.entries.values().any(|entry| entry.tween.is_some())
    }
    pub(super) fn is_key_animating(&self, key: K) -> bool {
        self.entries
            .get(&key)
            .is_some_and(|entry| entry.tween.is_some())
    }
    /// Keys that currently have a running tween.
    pub(super) fn active_keys(&self) -> Vec<K> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.tween.is_some())
            .map(|(key, _)| *key)
            .collect()
    }
    /// Advance every tween to `now`; settled tweens store their final value.
    /// Returns true while anything is still animating.
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        let mut running = false;
        for entry in self.entries.values_mut() {
            if let Some(tween) = entry.tween {
                entry.value = tween.sample(now);
                if tween.progress(now) >= 1.0 {
                    entry.value = tween.to;
                    entry.tween = None;
                } else {
                    running = true;
                }
            }
        }
        running
    }
    /// Jump every running tween to its end state.
    pub(super) fn finish_all(&mut self) {
        for entry in self.entries.values_mut() {
            if let Some(tween) = entry.tween.take() {
                entry.value = tween.to;
            }
        }
    }
    pub(super) fn remove(&mut self, key: K) {
        self.entries.remove(&key);
    }
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
    /// Keep only the keys for which `keep(key, value, animating)` is true.
    /// Returns the removed keys (e.g. to drop their regions).
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&K, f32, bool) -> bool) -> Vec<K> {
        let mut removed = Vec::new();
        self.entries.retain(|key, entry| {
            let kept = keep(key, entry.value, entry.tween.is_some());
            if !kept {
                removed.push(*key);
            }
            kept
        });
        removed
    }
    /// Drop every settled key whose value is `default` (a key that is not
    /// stored reads as its default again: hover 0, chevron 0°). Per-row keys
    /// of a long-running table stay bounded this way. A `follow` key that is
    /// pruned jumps on its next sight, so prune only keys whose state is at
    /// the default too. Returns how many keys were removed.
    pub(super) fn prune_settled(&mut self, default: f32) -> usize {
        self.retain(|_, value, animating| animating || value != default)
            .len()
    }
    /// Number of stored keys (tests, diagnostics).
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

/// What to invalidate when a key animates: a window, or a rect inside it.
/// `children` also invalidates the child windows under the rect
/// (`RedrawWindow … RDW_ALLCHILDREN`): a parent with WS_CLIPCHILDREN never
/// repaints its children by itself, so motion that crosses child windows
/// (the nav indicator over the rail's buttons) needs it.
#[derive(Clone, Copy)]
pub(super) struct Region {
    pub hwnd: HWND,
    pub rect: Option<RECT>,
    pub children: bool,
    /// Every frame paints the window and its children synchronously (one
    /// `RDW_UPDATENOW` pass after all of the frame's invalidations), so
    /// motion drawn across several windows is presented in one piece.
    pub sync: bool,
}

impl Region {
    /// `rect` of `hwnd` (None = the whole window), children not included.
    pub(super) fn window(hwnd: HWND, rect: Option<RECT>) -> Self {
        Self {
            hwnd,
            rect,
            children: false,
            sync: false,
        }
    }
    /// `rect` of `parent` and every child window it overlaps.
    pub(super) fn with_children(parent: HWND, rect: Option<RECT>) -> Self {
        Self {
            hwnd: parent,
            rect,
            children: true,
            sync: false,
        }
    }
}

/// Hosts with a running frame timer: while any runs, the system timer
/// resolution is 1 ms so `SetTimer(FRAME_MS)` really fires every ~10 ms
/// (at the default ~15.6 ms tick it alternated 7 / 14 / 21 ms frames).
static FRAME_TIMERS: AtomicUsize = AtomicUsize::new(0);
fn frame_timer_started() {
    if FRAME_TIMERS.fetch_add(1, Ordering::SeqCst) == 0 {
        unsafe {
            timeBeginPeriod(1);
        }
    }
}
fn frame_timer_stopped() {
    if FRAME_TIMERS.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)) == Ok(1)
    {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

/// A tween that starts later ([`AnimHost::set_target_after`]).
#[derive(Clone, Copy)]
struct Delayed {
    target: f32,
    duration: Duration,
    easing: Easing,
    due: Instant,
}

/// Per-window animation driver: owns an [`Animator`], runs `SetTimer` on its
/// window only while something animates, and invalidates the regions that were
/// registered for the animating keys (the whole host window when a key has no
/// region). One host per top-level/custom window class. Delayed starts use a
/// second, one-shot timer ([`delay_timer_id`]) so nothing repaints while
/// waiting (scrollbar idle fade-out, toast hold).
pub(super) struct AnimHost<K> {
    pub anim: Animator<K>,
    hwnd: HWND,
    timer_id: usize,
    interval_ms: u32,
    running: bool,
    regions: HashMap<K, Vec<Region>>,
    delayed: HashMap<K, Delayed>,
    delay_running: bool,
}

/// Default timer id; pick another one if the window already uses it.
pub(super) const ANIM_TIMER_ID: usize = 0xFEA7;
/// SetTimer period. Windows clamps to USER_TIMER_MINIMUM and the ~15.6 ms tick.
pub(super) const FRAME_MS: u32 = 10;

/// The one-shot timer id a host with frame timer `timer_id` uses for delayed
/// starts (keep both ids free on the host window).
pub(super) const fn delay_timer_id(timer_id: usize) -> usize {
    timer_id.wrapping_add(0x1_0000)
}

impl<K: Copy + Eq + Hash> AnimHost<K> {
    pub(super) fn new(timer_id: usize) -> Self {
        Self {
            anim: Animator::new(),
            hwnd: std::ptr::null_mut(),
            timer_id,
            interval_ms: FRAME_MS,
            running: false,
            regions: HashMap::new(),
            delayed: HashMap::new(),
            delay_running: false,
        }
    }
    /// Bind to the window whose WM_TIMER will drive the tweens.
    pub(super) fn attach(&mut self, hwnd: HWND) {
        if self.hwnd != hwnd {
            self.stop();
            self.hwnd = hwnd;
        }
    }
    pub(super) fn hwnd(&self) -> HWND {
        self.hwnd
    }
    pub(super) fn timer_id(&self) -> usize {
        self.timer_id
    }
    /// True while the frame timer is alive.
    pub(super) fn is_running(&self) -> bool {
        self.running
    }
    /// Invalidate `rect` of `hwnd` (None = the whole window) whenever `key` animates.
    pub(super) fn register(&mut self, key: K, hwnd: HWND, rect: Option<RECT>) {
        self.regions.insert(key, vec![Region::window(hwnd, rect)]);
    }
    /// Invalidate several windows / rects whenever `key` animates (e.g. a
    /// highlight that spans a popup and its owner).
    pub(super) fn register_many(&mut self, key: K, regions: &[Region]) {
        self.regions.insert(key, regions.to_vec());
    }
    /// Invalidate `rect` of `parent` *and the child windows under it*
    /// whenever `key` animates (motion drawn across child windows of a
    /// WS_CLIPCHILDREN parent, such as the nav indicator).
    pub(super) fn register_children(&mut self, key: K, parent: HWND, rect: Option<RECT>) {
        self.regions
            .insert(key, vec![Region::with_children(parent, rect)]);
    }
    /// [`AnimHost::register_children`] painted synchronously every frame
    /// (`Region::sync`): the parent and its children present together.
    pub(super) fn register_children_sync(&mut self, key: K, parent: HWND, rect: Option<RECT>) {
        let mut region = Region::with_children(parent, rect);
        region.sync = true;
        self.regions.insert(key, vec![region]);
    }
    pub(super) fn unregister(&mut self, key: K) {
        self.regions.remove(&key);
    }
    pub(super) fn value(&self, key: K) -> f32 {
        self.anim.value(key)
    }
    pub(super) fn value_or(&self, key: K, default: f32) -> f32 {
        self.anim.value_or(key, default)
    }
    /// Jump without animation (and repaint the key's region). Cancels a
    /// pending delayed start of the key.
    pub(super) fn set(&mut self, key: K, value: f32) {
        self.cancel_delayed(key);
        let changed = self.anim.value(key) != value || self.anim.is_key_animating(key);
        self.anim.set(key, value);
        if changed {
            self.invalidate(key);
        }
    }
    /// Start a tween toward `target` after `delay`, without running (or
    /// repainting) anything while waiting: a one-shot timer
    /// ([`delay_timer_id`]) starts it. Used for the scrollbar's fade-out
    /// after `motion::SCROLLBAR_IDLE` and the toast's `motion::TOAST_HOLD`.
    /// Re-issuing replaces the pending start (activity restarts the idle
    /// wait); `set`/`set_target` on the key cancel it. Reduced motion still
    /// waits, then jumps.
    pub(super) fn set_target_after(
        &mut self,
        key: K,
        target: f32,
        delay: Duration,
        duration: Duration,
        easing: Easing,
    ) {
        self.set_target_after_at(key, target, delay, duration, easing, Instant::now());
    }
    /// [`AnimHost::set_target_after`] with an explicit clock.
    pub(super) fn set_target_after_at(
        &mut self,
        key: K,
        target: f32,
        delay: Duration,
        duration: Duration,
        easing: Easing,
        now: Instant,
    ) {
        if delay.is_zero() {
            self.set_target_at(key, target, duration, easing, now);
            return;
        }
        self.delayed.insert(
            key,
            Delayed {
                target,
                duration,
                easing,
                due: now + delay,
            },
        );
        self.schedule_delay(now);
    }
    /// Forget a pending delayed start of `key` (no-op when none).
    pub(super) fn cancel_delayed(&mut self, key: K) {
        if self.delayed.remove(&key).is_some() {
            self.schedule_delay(Instant::now());
        }
    }
    /// True while `key` has a delayed start pending.
    pub(super) fn is_delayed(&self, key: K) -> bool {
        self.delayed.contains_key(&key)
    }
    /// True while the one-shot delay timer is alive.
    pub(super) fn is_waiting(&self) -> bool {
        self.delay_running
    }
    /// Start every delayed tween that is due at `now` (the delay timer calls
    /// this; tests call it with their own clock).
    pub(super) fn start_due_at(&mut self, now: Instant) {
        let due: Vec<(K, Delayed)> = self
            .delayed
            .iter()
            .filter(|(_, pending)| pending.due <= now)
            .map(|(key, pending)| (*key, *pending))
            .collect();
        for (key, pending) in due {
            self.delayed.remove(&key);
            self.set_target_at(key, pending.target, pending.duration, pending.easing, now);
        }
        self.schedule_delay(now);
    }
    /// Keep only the keys for which `keep(key, value, animating)` is true;
    /// removed keys lose their regions and pending starts too.
    pub(super) fn retain(&mut self, keep: impl FnMut(&K, f32, bool) -> bool) -> usize {
        let removed = self.anim.retain(keep);
        for key in &removed {
            self.regions.remove(key);
            self.delayed.remove(key);
        }
        removed.len()
    }
    /// [`Animator::prune_settled`] plus the keys' regions (keys with a
    /// pending delayed start are kept).
    pub(super) fn prune_settled(&mut self, default: f32) -> usize {
        let delayed: Vec<K> = self.delayed.keys().copied().collect();
        self.retain(|key, value, animating| animating || value != default || delayed.contains(key))
    }
    /// Start/retarget a tween and make sure the frame timer runs.
    pub(super) fn set_target(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        easing: Easing,
    ) -> bool {
        self.set_target_at(key, target, duration, easing, Instant::now())
    }
    pub(super) fn set_target_at(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        easing: Easing,
        now: Instant,
    ) -> bool {
        if self.delayed.remove(&key).is_some() {
            self.schedule_delay(now);
        }
        let before = self.anim.value_at(key, now);
        let animating = self.anim.set_target_at(key, target, duration, easing, now);
        if animating {
            self.ensure_timer();
        }
        if animating || before != self.anim.value(key) {
            self.invalidate(key);
        }
        animating
    }
    /// [`Animator::set_target_smooth_at`] (glides keep their momentum) plus
    /// timer management.
    pub(super) fn set_target_smooth(&mut self, key: K, target: f32, duration: Duration) -> bool {
        let now = Instant::now();
        if self.delayed.remove(&key).is_some() {
            self.schedule_delay(now);
        }
        let before = self.anim.value_at(key, now);
        let animating = self.anim.set_target_smooth_at(key, target, duration, now);
        if animating {
            self.ensure_timer();
        }
        if animating || before != self.anim.value(key) {
            self.invalidate(key);
        }
        animating
    }
    /// [`Animator::rebase`].
    pub(super) fn rebase(&mut self, key: K, now: Instant) {
        self.anim.rebase(key, now);
    }
    /// [`Animator::follow`] plus timer management: call it from paint code
    /// with the state's target value; returns the value to draw now.
    pub(super) fn follow(
        &mut self,
        key: K,
        target: f32,
        duration: Duration,
        easing: Easing,
    ) -> f32 {
        match self.anim.target(key) {
            None => self.anim.set(key, target),
            Some(current) if current != target => {
                self.set_target(key, target, duration, easing);
            }
            Some(_) => {}
        }
        self.anim.value(key)
    }
    /// Handle WM_TIMER (frame and delay timers). Returns false when
    /// `timer_id` is not one of this host's timers.
    pub(super) fn on_timer(&mut self, timer_id: usize) -> bool {
        if timer_id == delay_timer_id(self.timer_id) {
            // One-shot: stop it first; `start_due_at` re-arms it if needed.
            self.kill_delay_timer();
            self.start_due_at(Instant::now());
            return true;
        }
        if timer_id != self.timer_id {
            return false;
        }
        let now = Instant::now();
        self.anim
            .catch_up(now, Duration::from_millis(FRAME_MS as u64));
        self.tick_at(now);
        true
    }
    /// One frame: advance, repaint what moved, stop the timer when settled.
    /// Windows of `sync` regions are painted before it returns (once each,
    /// after every invalidation of the frame).
    pub(super) fn tick_at(&mut self, now: Instant) -> bool {
        let moving = self.anim.active_keys();
        let running = self.anim.tick(now);
        let mut sync: Vec<HWND> = Vec::new();
        for key in moving {
            self.invalidate(key);
            for region in self.regions.get(&key).into_iter().flatten() {
                if region.sync && !region.hwnd.is_null() && !sync.contains(&region.hwnd) {
                    sync.push(region.hwnd);
                }
            }
        }
        for hwnd in sync {
            unsafe {
                RedrawWindow(hwnd, null(), null_mut(), RDW_UPDATENOW | RDW_ALLCHILDREN);
            }
        }
        if !running {
            self.kill_timer();
        }
        running
    }
    /// Jump everything to its end state (pending delayed starts included)
    /// and stop the timers.
    pub(super) fn finish_all(&mut self) {
        let delayed: Vec<(K, Delayed)> = self.delayed.drain().collect();
        for (key, pending) in delayed {
            self.anim.set(key, pending.target);
            self.invalidate(key);
        }
        self.kill_delay_timer();
        let moving = self.anim.active_keys();
        self.anim.finish_all();
        for key in moving {
            self.invalidate(key);
        }
        self.kill_timer();
    }
    /// Stop the timers (e.g. WM_DESTROY). Values are kept; pending delayed
    /// starts are dropped.
    pub(super) fn stop(&mut self) {
        self.delayed.clear();
        self.kill_delay_timer();
        self.kill_timer();
    }
    fn ensure_timer(&mut self) {
        if self.running || self.hwnd.is_null() {
            return;
        }
        self.running = unsafe { SetTimer(self.hwnd, self.timer_id, self.interval_ms, None) } != 0;
        if self.running {
            frame_timer_started();
        }
    }
    fn kill_timer(&mut self) {
        if self.running {
            unsafe {
                KillTimer(self.hwnd, self.timer_id);
            }
            self.running = false;
            frame_timer_stopped();
        }
    }
    /// Arm the one-shot delay timer for the earliest pending start (SetTimer
    /// on an existing id replaces it), or kill it when nothing is pending.
    fn schedule_delay(&mut self, now: Instant) {
        let Some(due) = self.delayed.values().map(|pending| pending.due).min() else {
            self.kill_delay_timer();
            return;
        };
        if self.hwnd.is_null() {
            return;
        }
        let wait = due
            .saturating_duration_since(now)
            .as_millis()
            .clamp(1, u32::MAX as u128) as u32;
        self.delay_running =
            unsafe { SetTimer(self.hwnd, delay_timer_id(self.timer_id), wait, None) } != 0;
    }
    fn kill_delay_timer(&mut self) {
        if self.delay_running {
            unsafe {
                KillTimer(self.hwnd, delay_timer_id(self.timer_id));
            }
            self.delay_running = false;
        }
    }
    fn invalidate(&self, key: K) {
        let fallback = [Region::window(self.hwnd, None)];
        let regions = self
            .regions
            .get(&key)
            .map_or(&fallback[..], |regions| &regions[..]);
        for region in regions {
            if region.hwnd.is_null() {
                continue;
            }
            let rect = region
                .rect
                .as_ref()
                .map_or(null(), |rect| rect as *const RECT);
            unsafe {
                if region.children {
                    RedrawWindow(
                        region.hwnd,
                        rect,
                        null_mut(),
                        RDW_INVALIDATE | RDW_ALLCHILDREN,
                    );
                } else {
                    InvalidateRect(region.hwnd, rect, 0);
                }
            }
        }
    }
}

impl<K> Drop for AnimHost<K> {
    fn drop(&mut self) {
        if self.running {
            frame_timer_stopped();
        }
        if !self.hwnd.is_null() {
            unsafe {
                if self.running {
                    KillTimer(self.hwnd, self.timer_id);
                }
                if self.delay_running {
                    KillTimer(self.hwnd, delay_timer_id(self.timer_id));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::zeroed;
    use windows_sys::Win32::Graphics::Gdi::{GetUpdateRect, ValidateRect};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, PeekMessageW, MSG, PM_REMOVE, WM_TIMER, WS_CHILD,
        WS_CLIPCHILDREN, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW, WS_POPUP,
        WS_VISIBLE,
    };

    const MS: fn(u64) -> Duration = Duration::from_millis;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn easing_samples_match_css_cubic_bezier() {
        // Reference values computed by high-precision bisection of the CSS curves.
        for (x, ease, out) in [
            (0.1, 0.094796, 0.303848),
            (0.25, 0.408511, 0.577573),
            (0.5, 0.802403, 0.839245),
            (0.75, 0.960459, 0.964216),
            (0.9, 0.994316, 0.994601),
        ] {
            assert!(close(Easing::Ease.apply(x), ease), "ease({x})");
            assert!(close(Easing::EaseOut.apply(x), out), "ease-out({x})");
        }
        for easing in [
            Easing::Linear,
            Easing::Ease,
            Easing::EaseOut,
            Easing::Bezier(0.4, 0.0, 0.2, 1.0),
        ] {
            assert_eq!(easing.apply(0.0), 0.0);
            assert_eq!(easing.apply(1.0), 1.0);
            assert_eq!(easing.apply(-3.0), 0.0);
            assert_eq!(easing.apply(7.0), 1.0);
            let mut last = 0.0;
            for i in 0..=100 {
                let value = easing.apply(i as f32 / 100.0);
                assert!(value + 1e-5 >= last, "{easing:?} not monotonic");
                last = value;
            }
        }
        assert!(close(Easing::Linear.apply(0.3), 0.3));
    }

    #[test]
    fn tween_runs_from_current_value_and_settles_exactly() {
        let t0 = Instant::now();
        let mut a = Animator::new().with_reduced_motion(false);
        assert_eq!(a.value(1u8), 0.0);
        assert!(a.set_target_at(1u8, 1.0, MS(100), Easing::Linear, t0));
        assert!(a.is_animating());
        assert_eq!(a.value(1), 0.0);
        assert!(a.tick(t0 + MS(50)));
        assert!(close(a.value(1), 0.5));
        assert!(close(a.value_at(1, t0 + MS(75)), 0.75));
        assert!(!a.tick(t0 + MS(100)));
        assert_eq!(a.value(1), 1.0);
        assert!(!a.is_animating());
        assert_eq!(a.target(1), Some(1.0));
        // Setting the value it already has does not start anything.
        assert!(!a.set_target_at(1, 1.0, MS(100), Easing::Linear, t0 + MS(200)));
        assert!(!a.is_animating());
    }

    #[test]
    fn retargeting_mid_flight_is_continuous() {
        let t0 = Instant::now();
        let mut a = Animator::new().with_reduced_motion(false);
        a.set_target_at("hover", 1.0, MS(100), Easing::EaseOut, t0);
        a.tick(t0 + MS(40));
        let before = a.value_at("hover", t0 + MS(40));
        // Mouse leaves at 40 ms: fade back out from exactly where it is.
        assert!(a.set_target_at("hover", 0.0, MS(150), Easing::EaseOut, t0 + MS(40)));
        assert!(close(a.value("hover"), before));
        assert!(close(a.value_at("hover", t0 + MS(40)), before));
        assert!(a.value_at("hover", t0 + MS(41)) <= before);
        // Re-issuing the same target keeps the running timeline.
        a.set_target_at("hover", 0.0, MS(150), Easing::EaseOut, t0 + MS(100));
        assert!(!a.tick(t0 + MS(190)));
        assert_eq!(a.value("hover"), 0.0);
        // Explicit start values and removal.
        a.set("knob", 1.0);
        assert_eq!(a.value("knob"), 1.0);
        a.remove("knob");
        assert_eq!(a.value_or("knob", 0.25), 0.25);
    }

    #[test]
    fn follow_jumps_on_first_sight_then_animates_changes() {
        let mut a = Animator::new().with_reduced_motion(false);
        assert_eq!(a.follow(1u8, 1.0, MS(120), Easing::Ease), 1.0);
        assert!(!a.is_animating(), "first paint must not animate");
        assert_eq!(a.follow(1, 1.0, MS(120), Easing::Ease), 1.0);
        assert!(!a.is_animating());
        let value = a.follow(1, 0.0, MS(120), Easing::Ease);
        assert!(a.is_animating());
        assert!(value > 0.9, "animation starts from the old value");
        assert_eq!(a.target(1), Some(0.0));
    }

    #[test]
    fn reduced_motion_and_zero_duration_jump_to_the_end() {
        let t0 = Instant::now();
        let mut a = Animator::new().with_reduced_motion(true);
        assert!(!a.set_target_at(7u32, 1.0, MS(120), Easing::Ease, t0));
        assert_eq!(a.value(7), 1.0);
        assert!(!a.is_animating());
        let mut b = Animator::new().with_reduced_motion(false);
        assert!(!b.set_target_at(7u32, 0.5, Duration::ZERO, Easing::Ease, t0));
        assert_eq!(b.value(7), 0.5);
        b.set_target_at(8, 1.0, MS(100), Easing::Ease, t0);
        b.finish_all();
        assert_eq!(b.value(8), 1.0);
        assert!(!b.is_animating());
        // The system query itself must not fail.
        let _ = refresh_reduced_motion();
        let _ = reduced_motion();
    }

    #[test]
    fn host_runs_a_timer_only_while_animating() {
        unsafe {
            let hwnd = CreateWindowExW(
                0,
                windows_sys::w!("Static"),
                windows_sys::w!("anim host test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                100,
                100,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            assert!(!hwnd.is_null());
            let mut host: AnimHost<(usize, u32)> = AnimHost::new(ANIM_TIMER_ID);
            host.anim = Animator::new().with_reduced_motion(false);
            host.attach(hwnd);
            let rect: RECT = zeroed();
            host.register((1, 0), hwnd, Some(rect));
            assert!(!host.is_running(), "idle host must not own a timer");
            let t0 = Instant::now();
            assert!(host.set_target_at((1, 0), 1.0, MS(100), Easing::EaseOut, t0));
            assert!(host.is_running());
            // Setting another key does not create a second timer.
            host.set_target_at((2, 0), 1.0, MS(50), Easing::EaseOut, t0);
            assert!(host.tick_at(t0 + MS(60)));
            assert!(host.is_running());
            assert!(
                !host.on_timer(ANIM_TIMER_ID + 1),
                "foreign timer ids are ignored"
            );
            assert!(!host.tick_at(t0 + MS(100)));
            assert!(!host.is_running(), "timer must be killed once settled");
            assert_eq!(KillTimer(hwnd, ANIM_TIMER_ID), 0, "no timer may remain");
            assert_eq!(host.value((1, 0)), 1.0);
            // Jumps never start a timer.
            host.set((1, 0), 0.0);
            assert!(!host.is_running());
            // Reduced motion never starts a timer either.
            host.anim = Animator::new().with_reduced_motion(true);
            assert!(!host.set_target((3, 1), 1.0, MS(100), Easing::Ease));
            assert!(!host.is_running());
            assert_eq!(host.value((3, 1)), 1.0);
            // A running host stops cleanly.
            host.anim = Animator::new().with_reduced_motion(false);
            host.set_target((4, 0), 1.0, MS(1000), Easing::Ease);
            assert!(host.is_running());
            host.stop();
            assert!(!host.is_running());
            assert_eq!(KillTimer(hwnd, ANIM_TIMER_ID), 0);
            DestroyWindow(hwnd);
        }
    }

    unsafe fn window(parent: HWND, class: *const u16, style: u32, rect: RECT) -> HWND {
        CreateWindowExW(
            if parent.is_null() {
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE
            } else {
                0
            },
            class,
            windows_sys::w!(""),
            style,
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            parent,
            std::ptr::null_mut(),
            GetModuleHandleW(null()),
            null(),
        )
    }

    unsafe fn has_update(hwnd: HWND) -> bool {
        let mut r: RECT = zeroed();
        GetUpdateRect(hwnd, &mut r, 0) != 0
    }

    /// A key registered with `register_children` repaints the child windows
    /// under its rect even when the parent clips its children (a plain parent
    /// region does not), and `register_many` repaints every listed window.
    #[test]
    fn retargeted_glides_keep_their_speed_and_never_overshoot() {
        let mut a: Animator<u8> = Animator::new().with_reduced_motion(false);
        let t0 = Instant::now();
        // A first notch: ease-out from rest.
        assert!(a.set_target_smooth_at(1, 100.0, MS(160), t0));
        let t1 = t0 + MS(40);
        let before = a.entries[&1].tween.unwrap().velocity_at(t1);
        let at = a.value_at(1, t1);
        // A second notch 40 ms later: the new curve leaves at the old speed.
        assert!(a.set_target_smooth_at(1, 200.0, MS(160), t1));
        let tween = a.entries[&1].tween.unwrap();
        assert!(close(tween.from, at));
        let after = tween.velocity_at(t1 + MS(1));
        assert!(
            (after - before).abs() / before < 0.1,
            "speed {before} → {after}"
        );
        // Monotonic, no overshoot, ends on the target.
        let mut last = at;
        for ms in (0..=170).step_by(5) {
            let v = a.value_at(1, t1 + MS(ms));
            assert!(v >= last - 1e-3 && v <= 200.0 + 1e-3, "{ms}: {v}");
            last = v;
        }
        a.tick(t1 + MS(200));
        assert_eq!(a.value(1), 200.0);
        // A reversal starts from rest (no dip past the old position).
        a.set_target_smooth_at(1, 300.0, MS(160), t1 + MS(200));
        let t2 = t1 + MS(240);
        let at = a.value_at(1, t2);
        a.set_target_smooth_at(1, 0.0, MS(160), t2);
        assert!(a.entries[&1].tween.unwrap().velocity.is_none());
        assert!(a.value_at(1, t2 + MS(10)) <= at);
    }

    #[test]
    fn rebase_restarts_a_tween_and_catch_up_skips_a_busy_start() {
        let mut a: Animator<u8> = Animator::new().with_reduced_motion(false);
        let t0 = Instant::now();
        a.set_target_at(1, 100.0, MS(100), Easing::Linear, t0);
        a.rebase(1, t0 + MS(50));
        assert!(close(a.value_at(1, t0 + MS(50)), 0.0));
        assert!(close(a.value_at(1, t0 + MS(100)), 50.0));
        // The first frame 60 ms after the start: at most one frame in.
        a.set_target_at(2, 100.0, MS(100), Easing::Linear, t0);
        a.catch_up(t0 + MS(60), MS(10));
        assert!(close(a.value_at(2, t0 + MS(60)), 10.0));
        // Only the first frame is adjusted.
        a.catch_up(t0 + MS(90), MS(10));
        assert!(close(a.value_at(2, t0 + MS(90)), 40.0));
    }

    #[test]
    fn children_regions_repaint_the_child_windows_of_a_clipping_parent() {
        unsafe {
            let at = |l, t, r, b| RECT {
                left: l,
                top: t,
                right: r,
                bottom: b,
            };
            // Off screen but visible: hidden windows keep no update region.
            let parent = window(
                std::ptr::null_mut(),
                windows_sys::w!("Static"),
                WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN,
                at(-12000, -12000, -11700, -11700),
            );
            let style = WS_CHILD | WS_VISIBLE;
            let a = window(parent, windows_sys::w!("Static"), style, at(8, 8, 208, 48));
            let b = window(parent, windows_sys::w!("Static"), style, at(8, 50, 208, 90));
            let outside = window(
                parent,
                windows_sys::w!("Static"),
                style,
                at(8, 200, 208, 240),
            );
            let validate = || {
                for h in [parent, a, b, outside] {
                    ValidateRect(h, null());
                }
            };
            let mut host: AnimHost<(usize, u32)> = AnimHost::new(ANIM_TIMER_ID);
            host.anim = Animator::new().with_reduced_motion(false);
            host.attach(parent);
            let rail = Some(at(0, 0, 220, 100));
            let t0 = Instant::now();
            // Plain parent region: clipped children stay valid.
            validate();
            host.register((1, part::NAV_TOP), parent, rail);
            host.set_target_at((1, part::NAV_TOP), 50.0, MS(180), Easing::EaseOut, t0);
            assert!(has_update(parent));
            assert!(!has_update(a) && !has_update(b));
            // With children: both nav children repaint every frame, the child
            // outside the rect does not.
            validate();
            host.register_children((1, part::NAV_TOP), parent, rail);
            host.tick_at(t0 + MS(40));
            assert!(has_update(parent) && has_update(a) && has_update(b));
            assert!(!has_update(outside));
            // Several windows for one key.
            validate();
            host.register_many(
                (1, part::NAV_TOP),
                &[Region::window(a, None), Region::window(outside, None)],
            );
            host.tick_at(t0 + MS(80));
            assert!(has_update(a) && has_update(outside) && !has_update(b));
            host.stop();
            DestroyWindow(parent);
        }
    }

    /// Delayed starts (scrollbar idle fade, toast hold) wait on a one-shot
    /// timer: no frame timer and no repaint while waiting, then a normal
    /// tween; activity cancels or restarts the wait.
    #[test]
    fn delayed_starts_wait_without_a_frame_timer() {
        unsafe {
            let hwnd = CreateWindowExW(
                0,
                windows_sys::w!("Static"),
                windows_sys::w!("anim delay test"),
                WS_OVERLAPPEDWINDOW,
                0,
                0,
                100,
                100,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                GetModuleHandleW(null()),
                null(),
            );
            let mut host: AnimHost<(usize, u32)> = AnimHost::new(ANIM_TIMER_ID);
            host.anim = Animator::new().with_reduced_motion(false);
            host.attach(hwnd);
            let key = (7, part::OPACITY);
            let t0 = Instant::now();
            host.set(key, 1.0);
            host.set_target_after_at(key, 0.0, MS(1000), MS(200), Easing::EaseOut, t0);
            assert!(host.is_delayed(key) && host.is_waiting());
            assert!(!host.is_running(), "no frame timer during the hold");
            host.start_due_at(t0 + MS(500));
            assert!(host.is_delayed(key) && !host.is_running());
            assert_eq!(host.value(key), 1.0);
            host.start_due_at(t0 + MS(1000));
            assert!(!host.is_delayed(key) && !host.is_waiting());
            assert!(host.is_running(), "the fade runs once due");
            assert_eq!(
                KillTimer(hwnd, delay_timer_id(ANIM_TIMER_ID)),
                0,
                "the one-shot timer is gone"
            );
            assert!(!host.tick_at(t0 + MS(1200)));
            assert_eq!(host.value(key), 0.0);
            // Activity: `set` / `set_target` cancel a pending start.
            host.set_target_after_at(key, 1.0, MS(300), MS(100), Easing::EaseOut, t0);
            host.set(key, 0.5);
            assert!(!host.is_delayed(key) && !host.is_waiting());
            host.set_target_after_at(key, 1.0, MS(300), MS(100), Easing::EaseOut, t0);
            host.set_target_at(key, 0.25, MS(0), Easing::Linear, t0);
            assert!(!host.is_delayed(key) && host.value(key) == 0.25);
            // finish_all resolves pending starts to their end state.
            host.set_target_after_at(key, 1.0, MS(300), MS(100), Easing::EaseOut, t0);
            host.finish_all();
            assert_eq!(host.value(key), 1.0);
            assert!(!host.is_waiting() && !host.is_running());
            // The real one-shot timer fires through on_timer.
            host.set_target_after(key, 0.0, MS(20), MS(0), Easing::Linear);
            let start = Instant::now();
            let mut message: MSG = zeroed();
            while host.is_delayed(key) && start.elapsed() < Duration::from_secs(2) {
                if PeekMessageW(&mut message, hwnd, WM_TIMER, WM_TIMER, PM_REMOVE) != 0 {
                    host.on_timer(message.wParam);
                }
            }
            assert!(!host.is_delayed(key));
            assert_eq!(host.value(key), 0.0);
            host.stop();
            DestroyWindow(hwnd);
        }
    }

    /// Per-row keys settle back at their default and are pruned with their
    /// regions, so a long-running table does not accumulate them.
    #[test]
    fn settled_default_keys_are_pruned_with_their_regions() {
        let t0 = Instant::now();
        let mut host: AnimHost<(usize, u32)> = AnimHost::new(ANIM_TIMER_ID);
        host.anim = Animator::new().with_reduced_motion(false);
        for row in 0..100 {
            host.register((row, part::HOVER), std::ptr::null_mut(), None);
            host.anim
                .set_target_at((row, part::HOVER), 1.0, MS(100), Easing::EaseOut, t0);
            host.anim.set_target_at(
                (row, part::HOVER),
                0.0,
                MS(150),
                Easing::EaseOut,
                t0 + MS(50),
            );
        }
        host.anim.set((500, part::HOVER), 1.0); // still hovered
        host.anim.set_target_at(
            (501, part::HOVER),
            1.0,
            MS(100),
            Easing::EaseOut,
            t0 + MS(250),
        );
        host.anim.tick(t0 + MS(260));
        assert_eq!(host.anim.len(), 102);
        assert_eq!(host.prune_settled(0.0), 100);
        assert_eq!(host.anim.len(), 2, "hovered and animating keys stay");
        assert!(host.regions.is_empty());
        assert_eq!(host.anim.value((3, part::HOVER)), 0.0);
        assert_eq!(host.retain(|key, _, _| key.0 != 500), 1);
        assert_eq!(host.anim.len(), 1);
    }
}
