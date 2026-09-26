//! Font roles (DESIGN_SPEC §2), family detection with fallbacks, and the
//! per-window [`Fonts`] set that is recreated on DPI and language changes.
//!
//! GDI exposes the named instances of the variable fonts as separate families
//! ("Segoe UI Variable Text Semibold", "Cascadia Mono SemiBold", truncated to 31
//! characters like "Segoe UI Variable Display Semib"); asking the base family
//! for weight 600 yields *Bold* (700). So every (face, weight) pair resolves to
//! the family that really contains that weight.
//!
//! Hangul (measured on Windows 11, see `hangul_renders_like_malgun_gothic_*`):
//! GDI font linking draws Hangul in Segoe UI Variable and Cascadia Mono text
//! with a *scaled-down* Malgun Gothic at many pixel sizes (17, 18, 20, 21, 22,
//! 26, 28 px …), so at 150 % Hangul came out at ~100 % size. Therefore:
//! * Korean mode: every Text/Display role uses Malgun Gothic (400 / 700; its
//!   Latin glyphs are Segoe-derived), as DESIGN_SPEC §2 allows. Native
//!   controls (edit, list view) then render Hangul correctly as well.
//! * Mono roles keep Cascadia Mono in both languages, and text that holds
//!   Hangul in any non-Malgun font (mono labels in Korean mode, Korean data in
//!   English mode) is drawn and measured with [`draw_text`] / [`text_extent`]:
//!   Hangul runs use a same-size Malgun Gothic on the primary font's baseline,
//!   which is what Chromium's font fallback does for the reference.
//! * The Semibold faces would link to *regular* Malgun Gothic anyway, so strong
//!   roles (600) use Malgun Gothic Bold in Korean mode.
#![allow(dead_code)] // Role API consumed by the Frame/Controls/Table tracks.

mod contrast;

use crate::i18n::Language;
use std::cell::RefCell;
use std::collections::HashMap;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{LPARAM, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, DrawTextW, EnumFontFamiliesExW, ExtTextOutW,
    GetCurrentObject, GetDC, GetObjectW, GetOutlineTextMetricsW, GetTextAlign,
    GetTextExtentExPointW, GetTextExtentPoint32W, GetTextMetricsW, ReleaseDC, SelectObject,
    SetTextAlign, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_BOTTOM, DT_CALCRECT, DT_CENTER,
    DT_EDITCONTROL, DT_END_ELLIPSIS, DT_EXPANDTABS, DT_MODIFYSTRING, DT_NOCLIP, DT_NOPREFIX,
    DT_PATH_ELLIPSIS, DT_RIGHT, DT_RTLREADING, DT_SINGLELINE, DT_TABSTOP, DT_VCENTER, DT_WORDBREAK,
    DT_WORD_ELLIPSIS, ETO_CLIPPED, HDC, HFONT, LOGFONTW, OBJ_FONT, OUTLINETEXTMETRICW, TA_BASELINE,
    TA_LEFT, TEXTMETRICW,
};

/// Typeface class of a role.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Face {
    Text,
    Display,
    Mono,
}

/// Every text style of the design.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Role {
    /// 14/400 Text: tables, nav, search input.
    Body,
    /// 14/600 Text: nav current item.
    BodyStrong,
    /// 13/400 Text: buttons, menus, palette items, dialog text, specs, brand.
    Ui,
    /// 13/600 Text: primary/danger buttons, group rows, set-row titles, device names.
    UiStrong,
    /// 12/400 Text: exe names, meta, set-row descriptions, chart captions, stat keys, selects.
    Small,
    /// 11/400 Text: pills.
    Pill,
    /// 20/600 Display: page title.
    H1,
    /// 28/600 Display: performance device title.
    PerfTitle,
    /// 18/600 Display: dialog title.
    DialogTitle,
    /// 15/600 Display: settings group title.
    GroupTitle,
    /// 15/400 Text: palette input.
    PaletteInput,
    /// 12.5/400 Mono: numeric cells, specs values.
    MonoCell,
    /// 15/600 Mono: table header totals.
    MonoTotal,
    /// 22/600 Mono: performance stat values.
    MonoStat,
    /// 12/400 Mono: status bar, page meta, device sub line.
    MonoSmall,
    /// 11/400 Mono: kbd badges, nav count.
    MonoTiny,
    /// 10/600 Mono: mono-ico initials.
    MonoBadge,
    /// 13/400 Mono: the group-row count "(8)" (`.num` inside a 13 px cell).
    MonoUi,
}

impl Role {
    pub(super) const ALL: [Role; 18] = [
        Role::Body,
        Role::BodyStrong,
        Role::Ui,
        Role::UiStrong,
        Role::Small,
        Role::Pill,
        Role::H1,
        Role::PerfTitle,
        Role::DialogTitle,
        Role::GroupTitle,
        Role::PaletteInput,
        Role::MonoCell,
        Role::MonoTotal,
        Role::MonoStat,
        Role::MonoSmall,
        Role::MonoTiny,
        Role::MonoBadge,
        Role::MonoUi,
    ];
    /// (face, CSS px size, CSS weight).
    pub(super) const fn spec(self) -> (Face, f32, i32) {
        match self {
            Role::Body => (Face::Text, 14.0, 400),
            Role::BodyStrong => (Face::Text, 14.0, 600),
            Role::Ui => (Face::Text, 13.0, 400),
            Role::UiStrong => (Face::Text, 13.0, 600),
            Role::Small => (Face::Text, 12.0, 400),
            Role::Pill => (Face::Text, 11.0, 400),
            Role::H1 => (Face::Display, 20.0, 600),
            Role::PerfTitle => (Face::Display, 28.0, 600),
            Role::DialogTitle => (Face::Display, 18.0, 600),
            Role::GroupTitle => (Face::Display, 15.0, 600),
            Role::PaletteInput => (Face::Text, 15.0, 400),
            Role::MonoCell => (Face::Mono, 12.5, 400),
            Role::MonoTotal => (Face::Mono, 15.0, 600),
            Role::MonoStat => (Face::Mono, 22.0, 600),
            Role::MonoSmall => (Face::Mono, 12.0, 400),
            Role::MonoTiny => (Face::Mono, 11.0, 400),
            Role::MonoBadge => (Face::Mono, 10.0, 600),
            Role::MonoUi => (Face::Mono, 13.0, 400),
        }
    }
}

/// Candidate GDI families for a face at a weight, best first, with the
/// `lfWeight` to request from each. The last entry always exists on Windows.
pub(super) fn candidates(face: Face, weight: i32, korean: bool) -> &'static [(&'static str, i32)] {
    let strong = weight >= 600;
    match (face, strong, korean) {
        (Face::Mono, false, _) => &[("Cascadia Mono", 400), ("Consolas", 400)],
        (Face::Mono, true, _) => &[("Cascadia Mono SemiBold", 600), ("Consolas", 700)],
        (Face::Text | Face::Display, true, true) => &[
            ("Malgun Gothic", 700),
            ("Segoe UI Variable Text Semibold", 600),
            ("Segoe UI Semibold", 600),
            ("Segoe UI", 700),
        ],
        (Face::Text | Face::Display, false, true) => &[
            ("Malgun Gothic", 400),
            ("Segoe UI Variable Text", 400),
            ("Segoe UI", 400),
        ],
        (Face::Text, false, _) => &[("Segoe UI Variable Text", 400), ("Segoe UI", 400)],
        (Face::Text, true, false) => &[
            ("Segoe UI Variable Text Semibold", 600),
            ("Segoe UI Semibold", 600),
            ("Segoe UI", 700),
        ],
        (Face::Display, false, _) => &[("Segoe UI Variable Display", 400), ("Segoe UI", 400)],
        // GDI's family name of this named instance is truncated to 31 chars.
        (Face::Display, true, false) => &[
            ("Segoe UI Variable Display Semib", 600),
            ("Segoe UI Semibold", 600),
            ("Segoe UI", 700),
        ],
    }
}

/// First available candidate (or the last one, which Windows always maps).
pub(super) fn pick(
    options: &'static [(&'static str, i32)],
    available: impl Fn(&str) -> bool,
) -> (&'static str, i32) {
    options
        .iter()
        .copied()
        .find(|(family, _)| available(family))
        .unwrap_or(options[options.len() - 1])
}

/// Font height in device pixels for a CSS px size: nearest pixel, with exact
/// halves rounded down (12.5 px → 12 at 96 DPI, 19 at 144 DPI).
pub(super) fn pixel_size(css_px: f32, dpi: i32) -> i32 {
    let value = css_px * dpi as f32 / 96.0;
    let rounded = if (value.fract() - 0.5).abs() < 1e-4 {
        value.floor()
    } else {
        value.round()
    };
    (rounded as i32).max(1)
}

unsafe extern "system" fn found(
    _font: *const LOGFONTW,
    _metrics: *const TEXTMETRICW,
    _kind: u32,
    hit: LPARAM,
) -> i32 {
    *(hit as *mut bool) = true;
    0 // One match is enough.
}

fn face_name(family: &str) -> [u16; 32] {
    let mut name = [0u16; 32];
    for (slot, unit) in name.iter_mut().zip(family.encode_utf16().take(31)) {
        *slot = unit;
    }
    name
}

/// Whether a family is installed (English or localized name). Cached per family.
pub(super) fn is_installed(family: &str) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = cache.lock().ok().and_then(|map| map.get(family).copied()) {
        return known;
    }
    let installed = unsafe {
        let dc = GetDC(null_mut());
        let mut query: LOGFONTW = zeroed();
        query.lfCharSet = DEFAULT_CHARSET;
        query.lfFaceName = face_name(family);
        let mut hit = false;
        EnumFontFamiliesExW(dc, &query, Some(found), &mut hit as *mut bool as isize, 0);
        ReleaseDC(null_mut(), dc);
        hit
    };
    if let Ok(mut map) = cache.lock() {
        map.insert(family.to_owned(), installed);
    }
    installed
}

/// The family and lfWeight a role resolves to on this machine.
pub(super) fn resolve(role: Role, language: Language) -> (&'static str, i32) {
    let (face, _, weight) = role.spec();
    pick(
        candidates(face, weight, language == Language::Korean),
        is_installed,
    )
}

/// A ClearType GDI font for a role at a DPI. The caller owns the handle.
///
/// # Safety
/// Plain GDI allocation; delete the result with `DeleteObject`.
pub(super) unsafe fn create(role: Role, dpi: i32, language: Language) -> HFONT {
    let (_, size, _) = role.spec();
    create_font(resolve(role, language), size, dpi)
}

/// A ClearType font of a resolved (family, lfWeight) at a CSS px size.
unsafe fn create_font((family, weight): (&str, i32), css_px: f32, dpi: i32) -> HFONT {
    let size = css_px;
    let mut font: LOGFONTW = zeroed();
    font.lfHeight = -pixel_size(size, dpi);
    font.lfWeight = weight;
    font.lfCharSet = DEFAULT_CHARSET;
    font.lfQuality = CLEARTYPE_QUALITY;
    font.lfFaceName = face_name(family);
    CreateFontIndirectW(&font)
}

/// One HFONT per role for one window at one DPI and language. Recreate it
/// (`Fonts::new`) on WM_DPICHANGED and language changes, hand the new
/// handles to child controls (WM_SETFONT), then drop the old set.
pub(super) struct Fonts {
    pub body: HFONT,
    pub body_strong: HFONT,
    pub ui: HFONT,
    pub ui_strong: HFONT,
    pub small: HFONT,
    pub pill: HFONT,
    pub h1: HFONT,
    pub perf_title: HFONT,
    pub dialog_title: HFONT,
    pub group_title: HFONT,
    pub palette_input: HFONT,
    pub mono_cell: HFONT,
    pub mono_total: HFONT,
    pub mono_stat: HFONT,
    pub mono_small: HFONT,
    pub mono_tiny: HFONT,
    pub mono_badge: HFONT,
    pub mono_ui: HFONT,
    /// Sizes outside the role table ([`Fonts::extra`]), owned by the set.
    extras: RefCell<Vec<(Face, u32, i32, HFONT)>>,
    dpi: i32,
    language: Language,
}

impl Fonts {
    /// An empty set (null handles) for windows that have not been created yet.
    pub(super) const fn empty() -> Self {
        let null = null_mut();
        Self {
            body: null,
            body_strong: null,
            ui: null,
            ui_strong: null,
            small: null,
            pill: null,
            h1: null,
            perf_title: null,
            dialog_title: null,
            group_title: null,
            palette_input: null,
            mono_cell: null,
            mono_total: null,
            mono_stat: null,
            mono_small: null,
            mono_tiny: null,
            mono_badge: null,
            mono_ui: null,
            extras: RefCell::new(Vec::new()),
            dpi: 96,
            language: Language::English,
        }
    }
    /// # Safety
    /// Plain GDI allocation; the set owns (and deletes) every handle.
    pub(super) unsafe fn new(dpi: i32, language: Language) -> Self {
        let make = |role| create(role, dpi, language);
        Self {
            body: make(Role::Body),
            body_strong: make(Role::BodyStrong),
            ui: make(Role::Ui),
            ui_strong: make(Role::UiStrong),
            small: make(Role::Small),
            pill: make(Role::Pill),
            h1: make(Role::H1),
            perf_title: make(Role::PerfTitle),
            dialog_title: make(Role::DialogTitle),
            group_title: make(Role::GroupTitle),
            palette_input: make(Role::PaletteInput),
            mono_cell: make(Role::MonoCell),
            mono_total: make(Role::MonoTotal),
            mono_stat: make(Role::MonoStat),
            mono_small: make(Role::MonoSmall),
            mono_tiny: make(Role::MonoTiny),
            mono_badge: make(Role::MonoBadge),
            mono_ui: make(Role::MonoUi),
            extras: RefCell::new(Vec::new()),
            dpi,
            language,
        }
    }
    pub(super) fn get(&self, role: Role) -> HFONT {
        match role {
            Role::Body => self.body,
            Role::BodyStrong => self.body_strong,
            Role::Ui => self.ui,
            Role::UiStrong => self.ui_strong,
            Role::Small => self.small,
            Role::Pill => self.pill,
            Role::H1 => self.h1,
            Role::PerfTitle => self.perf_title,
            Role::DialogTitle => self.dialog_title,
            Role::GroupTitle => self.group_title,
            Role::PaletteInput => self.palette_input,
            Role::MonoCell => self.mono_cell,
            Role::MonoTotal => self.mono_total,
            Role::MonoStat => self.mono_stat,
            Role::MonoSmall => self.mono_small,
            Role::MonoTiny => self.mono_tiny,
            Role::MonoBadge => self.mono_badge,
            Role::MonoUi => self.mono_ui,
        }
    }
    pub(super) fn dpi(&self) -> i32 {
        self.dpi
    }
    pub(super) fn language(&self) -> Language {
        self.language
    }
    /// True when this set matches a DPI and language (skip needless rebuilds).
    pub(super) fn matches(&self, dpi: i32, language: Language) -> bool {
        !self.body.is_null() && self.dpi == dpi && self.language == language
    }
    /// A size/weight the role table does not have (e.g. a 16 px mono label),
    /// at this set's DPI and language with the same family fallbacks. Created
    /// on first use, cached and deleted with the set — tracks need no edit of
    /// the role table for one-off sizes. Null before the set is created.
    pub(super) fn extra(&self, face: Face, css_px: f32, weight: i32) -> HFONT {
        if self.body.is_null() {
            return null_mut();
        }
        let key = (face, css_px.to_bits(), weight);
        let mut extras = self.extras.borrow_mut();
        if let Some(&(_, _, _, font)) = extras.iter().find(|(f, size, w, _)| (*f, *size, *w) == key)
        {
            return font;
        }
        let korean = self.language == Language::Korean;
        let font = unsafe {
            create_font(
                pick(candidates(face, weight, korean), is_installed),
                css_px,
                self.dpi,
            )
        };
        extras.push((face, key.1, weight, font));
        font
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for role in Role::ALL {
            let font = self.get(role);
            if !font.is_null() {
                unsafe { DeleteObject(font) };
            }
        }
        for (_, _, _, font) in self.extras.get_mut().drain(..) {
            if !font.is_null() {
                unsafe { DeleteObject(font) };
            }
        }
    }
}

// ───────────────────────── Hangul fallback (Chromium-style) ─────────────────────────

/// Hangul syllables and jamo, which GDI's font linking renders at the wrong
/// size (see the module docs).
pub(super) fn is_hangul(unit: u16) -> bool {
    matches!(
        unit,
        0x1100..=0x11FF | 0x3130..=0x318F | 0xA960..=0xA97F | 0xAC00..=0xD7AF | 0xD7B0..=0xD7FF
    )
}

const MALGUN: &str = "Malgun Gothic";

/// The em height (px) and weight of the font selected into `dc`.
unsafe fn selected_em(dc: HDC) -> Option<(i32, i32, TEXTMETRICW)> {
    let mut metrics: TEXTMETRICW = zeroed();
    if GetTextMetricsW(dc, &mut metrics) == 0 {
        return None;
    }
    Some((
        metrics.tmHeight - metrics.tmInternalLeading,
        metrics.tmWeight,
        metrics,
    ))
}

/// A same-size Malgun Gothic (400, or 700 for weights ≥ 550) that renders the
/// Hangul runs of text in `font` — None when `font` is Malgun Gothic itself or
/// Malgun Gothic is not installed. `em` is the primary font's em height.
/// Cached for the process lifetime: one handle per (size, weight, italic),
/// a handful per DPI, never per frame.
unsafe fn hangul_partner(font: HFONT, em: i32, weight: i32) -> Option<HFONT> {
    let mut logfont: LOGFONTW = zeroed();
    if font.is_null()
        || GetObjectW(
            font,
            size_of::<LOGFONTW>() as i32,
            (&mut logfont as *mut LOGFONTW).cast(),
        ) == 0
    {
        return None;
    }
    let face_len = logfont
        .lfFaceName
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(32);
    let face = String::from_utf16_lossy(&logfont.lfFaceName[..face_len]);
    if face.starts_with(MALGUN) || !is_installed(MALGUN) || em <= 0 {
        return None;
    }
    let weight = if weight >= 550 { 700 } else { 400 };
    let key = (em, weight, logfont.lfItalic);
    /// (em px, weight, italic) → HFONT (as usize: HFONT is not Send).
    type Partners = Mutex<HashMap<(i32, i32, u8), usize>>;
    static CACHE: OnceLock<Partners> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = cache.lock().ok()?;
    if let Some(&font) = map.get(&key) {
        return Some(font as HFONT);
    }
    let mut partner: LOGFONTW = zeroed();
    partner.lfHeight = -em;
    partner.lfWeight = weight;
    partner.lfItalic = logfont.lfItalic;
    partner.lfCharSet = DEFAULT_CHARSET;
    partner.lfQuality = logfont.lfQuality;
    partner.lfFaceName = face_name(MALGUN);
    let created = CreateFontIndirectW(&partner);
    if created.is_null() {
        return None;
    }
    map.insert(key, created as usize);
    Some(created)
}

/// Flags the run renderer reproduces exactly; anything else (multi-line,
/// tabs, prefix processing, measuring) goes to DrawTextW unchanged.
const RUN_UNSUPPORTED: u32 = DT_CALCRECT
    | DT_WORDBREAK
    | DT_EDITCONTROL
    | DT_EXPANDTABS
    | DT_TABSTOP
    | DT_PATH_ELLIPSIS
    | DT_WORD_ELLIPSIS
    | DT_MODIFYSTRING
    | DT_RTLREADING;

/// DrawTextW with Chromium-style Hangul fallback for the font selected into
/// `dc`: text without Hangul (or in Malgun Gothic) is drawn by DrawTextW
/// itself; single-line DT_NOPREFIX text with Hangul is drawn as runs — Hangul
/// in a same-size Malgun Gothic, everything else in the selected font — on
/// the selected font's baseline, with DrawTextW's alignment, vertical
/// centring, end ellipsis and clipping. Uses the DC's text color/bk mode.
/// Text lighter than what it is drawn on gets the reference's light-on-dark
/// contrast ([`contrast`]); dark text is GDI's output unchanged.
///
/// # Safety
/// `dc` must be a valid DC with the intended font selected.
pub(super) unsafe fn draw_text(dc: HDC, text: &[u16], rect: &mut RECT, flags: u32) -> i32 {
    if flags & DT_CALCRECT == 0 && !text.is_empty() && contrast::candidate(dc) {
        if let Some(area) = ink_area(dc, text, rect, flags) {
            let r = *rect;
            let drawn = contrast::draw(dc, area, |target, dx, dy| unsafe {
                let mut moved = RECT {
                    left: r.left + dx,
                    top: r.top + dy,
                    right: r.right + dx,
                    bottom: r.bottom + dy,
                };
                draw_plain(target, text, &mut moved, flags)
            });
            if let Some(result) = drawn {
                return result;
            }
        }
    }
    draw_plain(dc, text, rect, flags)
}

/// Draw text lifted like `color` (the undisabled color) while blending its
/// actual, disabled color (`contrast::with_lift_of`).
pub(super) fn with_lift_of<R>(color: u32, draw: impl FnOnce() -> R) -> R {
    contrast::with_lift_of(color, draw)
}

/// Run a GDI text call that bypasses [`draw_text`] (ExtTextOutW at explicit
/// advances) with the same light-on-dark contrast: `paint(dc, dx, dy)` draws
/// with every coordinate offset by (dx, dy); `area` bounds its ink (logical
/// units). Dark text, or a DC that cannot be read back, is drawn plainly.
///
/// # Safety
/// `dc` must be a valid DC with the intended font, text color and
/// transparent background mode selected.
pub(super) unsafe fn draw_contrasted<R>(
    dc: HDC,
    area: RECT,
    mut paint: impl FnMut(HDC, i32, i32) -> R,
) -> R {
    if let Some(result) = contrast::draw(dc, area, &mut paint) {
        return result;
    }
    paint(dc, 0, 0)
}

/// [`draw_text`] without the light-on-dark contrast.
unsafe fn draw_plain(dc: HDC, text: &[u16], rect: &mut RECT, flags: u32) -> i32 {
    if text.is_empty() {
        // An empty Vec's pointer is dangling, and DrawTextW dereferences it
        // for some flags (DT_WORDBREAK | DT_END_ELLIPSIS: an access
        // violation). Nothing to draw; a measurement still gets its height.
        if flags & DT_CALCRECT == 0 {
            return 0;
        }
        static EMPTY: [u16; 1] = [0];
        return DrawTextW(dc, EMPTY.as_ptr(), 0, rect, flags);
    }
    if let Some(runs) = hangul_runs(dc, text, flags) {
        return draw_runs(dc, text, &runs, rect, flags);
    }
    DrawTextW(dc, text.as_ptr(), text.len() as i32, rect, flags)
}

/// Where a DrawText call can put ink: its rectangle (DrawText clips to it),
/// or for DT_NOCLIP the rectangle grown by the text's extent in every
/// direction the alignment can overflow. None: multi-line DT_NOCLIP text.
unsafe fn ink_area(dc: HDC, text: &[u16], rect: &RECT, flags: u32) -> Option<RECT> {
    if flags & DT_NOCLIP == 0 {
        return Some(*rect);
    }
    if flags & DT_SINGLELINE == 0 && text.iter().any(|&u| u == 10 || u == 13) {
        return None;
    }
    let extent = text_extent(dc, text);
    let pad = extent.cy / 2 + 2;
    Some(RECT {
        left: rect.left.min(rect.right - extent.cx) - pad,
        top: rect.top.min(rect.bottom - extent.cy) - pad,
        right: rect.right.max(rect.left + extent.cx) + pad,
        bottom: rect.bottom.max(rect.top + extent.cy) + pad,
    })
}

/// [`draw_text`] for a `&str`.
///
/// # Safety
/// As [`draw_text`].
pub(super) unsafe fn draw_str(dc: HDC, text: &str, rect: &mut RECT, flags: u32) -> i32 {
    let units: Vec<u16> = text.encode_utf16().collect();
    draw_text(dc, &units, rect, flags)
}

/// GetTextExtentPoint32W of the selected font, with Hangul measured in its
/// fallback face exactly as [`draw_text`] draws it (height = the selected
/// font's cell height, like GDI).
///
/// # Safety
/// `dc` must be a valid DC with the intended font selected.
pub(super) unsafe fn text_extent(dc: HDC, text: &[u16]) -> SIZE {
    let mut size: SIZE = zeroed();
    if text.is_empty() {
        return size;
    }
    if let Some(runs) = hangul_runs(dc, text, DT_SINGLELINE | DT_NOPREFIX) {
        let primary = GetCurrentObject(dc, OBJ_FONT as u32) as HFONT;
        size.cx = runs
            .fonts
            .iter()
            .zip(runs.spans.iter())
            .map(|(&font, span)| run_width(dc, font, &text[span.0..span.1]))
            .sum();
        SelectObject(dc, primary);
        size.cy = runs.metrics.tmHeight;
        return size;
    }
    GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut size);
    size
}

/// [`text_extent`] for a `&str`.
///
/// # Safety
/// As [`text_extent`].
pub(super) unsafe fn str_extent(dc: HDC, text: &str) -> SIZE {
    let units: Vec<u16> = text.encode_utf16().collect();
    text_extent(dc, &units)
}

struct Runs {
    /// (start, end) of each run in UTF-16 units.
    spans: Vec<(usize, usize)>,
    /// Font of each run (primary or its Hangul partner).
    fonts: Vec<HFONT>,
    /// Metrics of the primary (selected) font.
    metrics: TEXTMETRICW,
}

/// The text's font runs when it needs the fallback path, else None.
unsafe fn hangul_runs(dc: HDC, text: &[u16], flags: u32) -> Option<Runs> {
    if flags & RUN_UNSUPPORTED != 0
        || flags & DT_NOPREFIX == 0
        || !text.iter().copied().any(is_hangul)
        || text
            .iter()
            .any(|&unit| unit == 10 || unit == 13 || unit == 9)
    {
        return None;
    }
    let primary = GetCurrentObject(dc, OBJ_FONT as u32) as HFONT;
    let (em, weight, metrics) = selected_em(dc)?;
    let partner = hangul_partner(primary, em, weight)?;
    split_runs(text, primary, partner, metrics)
}

fn split_runs(text: &[u16], primary: HFONT, partner: HFONT, metrics: TEXTMETRICW) -> Option<Runs> {
    let mut spans = Vec::new();
    let mut fonts = Vec::new();
    let mut start = 0;
    for i in 1..=text.len() {
        if i == text.len() || is_hangul(text[i]) != is_hangul(text[start]) {
            spans.push((start, i));
            fonts.push(if is_hangul(text[start]) {
                partner
            } else {
                primary
            });
            start = i;
        }
    }
    Some(Runs {
        spans,
        fonts,
        metrics,
    })
}

unsafe fn run_width(dc: HDC, font: HFONT, text: &[u16]) -> i32 {
    SelectObject(dc, font);
    let mut size: SIZE = zeroed();
    GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut size);
    size.cx
}

/// Draw `runs` of `text` like DrawTextW(DT_SINGLELINE-style) would place the
/// primary font's line: same horizontal alignment, DT_VCENTER/DT_BOTTOM
/// positions and DT_END_ELLIPSIS ("..." in the primary font).
unsafe fn draw_runs(dc: HDC, text: &[u16], runs: &Runs, rect: &RECT, flags: u32) -> i32 {
    let primary = GetCurrentObject(dc, OBJ_FONT as u32) as HFONT;
    let metrics = runs.metrics;
    let available = rect.right - rect.left;
    // Per-run widths, then the ellipsis cut if the line is too long.
    let mut pieces: Vec<(HFONT, Vec<u16>, i32)> = runs
        .spans
        .iter()
        .zip(runs.fonts.iter())
        .map(|(&(a, b), &font)| (font, text[a..b].to_vec(), run_width(dc, font, &text[a..b])))
        .collect();
    let total: i32 = pieces.iter().map(|piece| piece.2).sum();
    if flags & DT_END_ELLIPSIS != 0 && total > available {
        let dots: Vec<u16> = "...".encode_utf16().collect();
        let dots_width = run_width(dc, primary, &dots);
        let mut room = (available - dots_width).max(0);
        let mut kept = Vec::new();
        for (font, units, width) in pieces {
            if width <= room {
                room -= width;
                kept.push((font, units, width));
                continue;
            }
            // Longest prefix of this run that still fits.
            SelectObject(dc, font);
            let mut fit = 0;
            let mut size: SIZE = zeroed();
            GetTextExtentExPointW(
                dc,
                units.as_ptr(),
                units.len() as i32,
                room,
                &mut fit,
                null_mut(),
                &mut size,
            );
            let fit = (fit.max(0) as usize).min(units.len());
            if fit > 0 {
                let prefix = units[..fit].to_vec();
                let width = run_width(dc, font, &prefix);
                kept.push((font, prefix, width));
            }
            break;
        }
        kept.push((primary, dots, dots_width));
        pieces = kept;
    }
    let width: i32 = pieces.iter().map(|piece| piece.2).sum();
    let x = if flags & DT_CENTER != 0 {
        rect.left + (available - width) / 2
    } else if flags & DT_RIGHT != 0 {
        rect.right - width
    } else {
        rect.left
    };
    let top = if flags & DT_SINGLELINE != 0 && flags & DT_VCENTER != 0 {
        rect.top + (rect.bottom - rect.top - metrics.tmHeight) / 2
    } else if flags & DT_SINGLELINE != 0 && flags & DT_BOTTOM != 0 {
        rect.bottom - metrics.tmHeight
    } else {
        rect.top
    };
    let baseline = top + metrics.tmAscent;
    let align = GetTextAlign(dc);
    SetTextAlign(dc, TA_BASELINE | TA_LEFT);
    let clip = if flags & DT_NOCLIP != 0 {
        0
    } else {
        ETO_CLIPPED
    };
    let mut pen = x;
    for (font, units, width) in &pieces {
        SelectObject(dc, *font);
        ExtTextOutW(
            dc,
            pen,
            baseline,
            clip,
            if clip != 0 { rect } else { null() },
            units.as_ptr(),
            units.len() as u32,
            null(),
        );
        pen += width;
    }
    SetTextAlign(dc, align);
    SelectObject(dc, primary);
    metrics.tmHeight
}

/// The width of `text` in the selected font at its fractional (unhinted)
/// advances, as Chromium lays text out: measured at 16× the size and scaled
/// back (GDI rounds every advance to whole pixels, e.g. Cascadia Mono 11 px
/// is 6 px per glyph in GDI and 6.45 px in the reference).
///
/// # Safety
/// `dc` must be a valid DC with the intended font selected.
pub(super) unsafe fn ideal_width(dc: HDC, text: &str) -> f32 {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.is_empty() {
        return 0.0;
    }
    let font = GetCurrentObject(dc, OBJ_FONT as u32) as HFONT;
    let mut logfont: LOGFONTW = zeroed();
    let Some((em, _, _)) = selected_em(dc) else {
        return 0.0;
    };
    if GetObjectW(
        font,
        size_of::<LOGFONTW>() as i32,
        (&mut logfont as *mut LOGFONTW).cast(),
    ) == 0
        || em <= 0
    {
        return text_extent(dc, &units).cx as f32;
    }
    const SCALE: i32 = 16;
    logfont.lfHeight = -em * SCALE;
    logfont.lfWidth = 0;
    let large = CreateFontIndirectW(&logfont);
    if large.is_null() {
        return text_extent(dc, &units).cx as f32;
    }
    // Plain GDI extent: the Hangul partner cache must not collect 16× sizes
    // (badge texts are Latin; any Hangul is measured by linking here).
    SelectObject(dc, large);
    let mut size: SIZE = zeroed();
    GetTextExtentPoint32W(dc, units.as_ptr(), units.len() as i32, &mut size);
    SelectObject(dc, font);
    DeleteObject(large);
    size.cx as f32 / SCALE as f32
}

// ───────────────────────── CSS line boxes ─────────────────────────

/// The selected font's ascent and descent as Chromium lays text out (the
/// `hhea` metrics GDI reports as otmMacAscent/Descent, rounded like Blink).
/// For Segoe UI they equal tmAscent/tmDescent; Cascadia Mono's are smaller
/// (1900/480 vs 2226/480 units), which is why GDI's DT_VCENTER put mono
/// labels 1–2 px lower than the reference and made `.kbd` 2 px taller.
///
/// # Safety
/// `dc` must be a valid DC with the intended font selected.
pub(super) unsafe fn css_metrics(dc: HDC) -> (i32, i32) {
    let mut outline: OUTLINETEXTMETRICW = zeroed();
    outline.otmSize = size_of::<OUTLINETEXTMETRICW>() as u32;
    if GetOutlineTextMetricsW(dc, size_of::<OUTLINETEXTMETRICW>() as u32, &mut outline) != 0
        && outline.otmMacAscent > 0
    {
        return (outline.otmMacAscent, -outline.otmMacDescent);
    }
    let mut metrics: TEXTMETRICW = zeroed();
    GetTextMetricsW(dc, &mut metrics);
    (metrics.tmAscent, metrics.tmDescent)
}

/// Where Chromium puts the baseline of a `line_height` px line box (CSS
/// `line-height`; `None` = normal) centred in `band` (flex `align-items:
/// center`): the line box is centred in 1/64 px layout units, the ascent side
/// gets `floor(half-leading)`, and the baseline snaps to the nearest pixel.
///
/// # Safety
/// `dc` must be a valid DC with the intended font selected.
pub(super) unsafe fn css_baseline(dc: HDC, band: RECT, line_height: Option<f32>) -> i32 {
    css_baseline_at(
        dc,
        band.top as f32,
        (band.bottom - band.top) as f32,
        line_height,
    )
}

/// [`css_baseline`] for a band at a fractional position (device px), for
/// layouts that keep Chromium's fractional edges (the settings rows: their
/// text sits where the unrounded CSS box puts it, not the snapped one).
///
/// # Safety
/// `dc` must be a valid DC with the intended font selected.
pub(super) unsafe fn css_baseline_at(
    dc: HDC,
    top: f32,
    height: f32,
    line_height: Option<f32>,
) -> i32 {
    let (ascent, descent) = css_metrics(dc);
    let content = (ascent + descent) as f32;
    let line = line_height.unwrap_or(content);
    let snap = |v: f32| (v * 64.0).round() / 64.0;
    let top = top + snap((height - snap(line)) / 2.0);
    let half_leading = ((snap(line) - content) / 2.0).floor();
    (top + half_leading + ascent as f32).round() as i32
}

/// The baseline of one line box holding text in several fonts (`fonts`, the
/// first is the element's own): Blink gives every inline box the line
/// height with `floor(half-leading)` above its ascent, and the line box
/// spans the largest ascent and descent, so a mono `.num` inside 13 px text
/// makes the line taller and moves its baseline. The line box is centred in
/// `band` (a table cell's `vertical-align: middle`).
///
/// # Safety
/// `dc` must be a valid DC; its selected font is restored.
pub(super) unsafe fn css_mixed_baseline(dc: HDC, band: RECT, fonts: &[HFONT], line: f32) -> i32 {
    let snap = |v: f32| (v * 64.0).round() / 64.0;
    let line = snap(line);
    let previous = GetCurrentObject(dc, OBJ_FONT as u32);
    let (mut above, mut below) = (0.0f32, 0.0f32);
    for &font in fonts {
        SelectObject(dc, font);
        let (ascent, descent) = css_metrics(dc);
        let half = ((line - (ascent + descent) as f32) / 2.0).floor();
        above = above.max(ascent as f32 + half);
        below = below.max(line - ascent as f32 - half);
    }
    SelectObject(dc, previous);
    let height = (band.bottom - band.top) as f32;
    let top = band.top as f32 + snap((height - (above + below)) / 2.0);
    (top + above).round() as i32
}

/// The rectangle to DrawText (DT_TOP, single line) the selected font in so its
/// baseline lands on [`css_baseline`]: a full GDI cell (tmHeight tall), so no
/// ascender or descender is ever clipped by the band.
///
/// # Safety
/// `dc` must be a valid DC with the intended font selected.
pub(super) unsafe fn css_line_rect(dc: HDC, band: RECT, line_height: Option<f32>) -> RECT {
    let mut metrics: TEXTMETRICW = zeroed();
    GetTextMetricsW(dc, &mut metrics);
    let top = css_baseline(dc, band, line_height) - metrics.tmAscent;
    RECT {
        left: band.left,
        top,
        right: band.right,
        bottom: top + metrics.tmHeight,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn empty_text_never_reaches_drawtext_with_a_dangling_pointer() {
        use windows_sys::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC};
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            let font = create(Role::Small, 96, Language::English);
            let old = SelectObject(dc, font);
            let mut r = RECT {
                left: 0,
                top: 0,
                right: 200,
                bottom: 60,
            };
            // The Service details description of a service without one.
            let flags = DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX;
            assert_eq!(draw_text(dc, &[], &mut r, flags), 0);
            assert_eq!(draw_str(dc, "", &mut r, flags), 0);
            // A measurement still answers (an empty line box).
            let mut m = r;
            draw_text(dc, &[], &mut m, flags | DT_CALCRECT);
            assert!(m.bottom >= m.top);
            SelectObject(dc, old);
            DeleteObject(font);
            DeleteDC(dc);
        }
    }

    use super::super::gfx::Dib;
    use super::*;
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, DeleteDC, GetTextFaceW, SetBkMode, SetTextColor, DT_LEFT, TRANSPARENT,
    };

    #[test]
    fn fallback_selection_prefers_design_faces_then_windows_faces() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        let classic = |family: &str| {
            family.starts_with("Segoe UI") && !family.contains("Variable") || family == "Consolas"
        };
        let text = candidates(Face::Text, 400, false);
        assert_eq!(pick(text, all), ("Segoe UI Variable Text", 400));
        assert_eq!(pick(text, classic), ("Segoe UI", 400));
        assert_eq!(pick(text, none), ("Segoe UI", 400));
        let strong = candidates(Face::Text, 600, false);
        assert_eq!(pick(strong, all), ("Segoe UI Variable Text Semibold", 600));
        assert_eq!(pick(strong, classic), ("Segoe UI Semibold", 600));
        let display = candidates(Face::Display, 600, false);
        assert_eq!(pick(display, all), ("Segoe UI Variable Display Semib", 600));
        assert!(display
            .iter()
            .all(|(family, _)| family.encode_utf16().count() <= 31));
        let mono = candidates(Face::Mono, 400, false);
        assert_eq!(pick(mono, all), ("Cascadia Mono", 400));
        assert_eq!(pick(mono, classic), ("Consolas", 400));
        assert_eq!(
            pick(candidates(Face::Mono, 600, true), classic),
            ("Consolas", 700)
        );
        // Korean: text roles use Malgun Gothic (linked Hangul is mis-scaled),
        // strong roles and headings real bold Hangul, mono stays mono (its
        // Hangul goes through the draw_text fallback).
        assert_eq!(
            pick(candidates(Face::Text, 400, true), all),
            ("Malgun Gothic", 400)
        );
        assert_eq!(
            pick(candidates(Face::Text, 400, true), classic),
            ("Segoe UI", 400)
        );
        assert_eq!(
            pick(candidates(Face::Display, 600, true), all),
            ("Malgun Gothic", 700)
        );
        assert_eq!(
            pick(candidates(Face::Text, 600, true), all),
            ("Malgun Gothic", 700)
        );
        assert_eq!(
            pick(candidates(Face::Mono, 400, true), all).0,
            "Cascadia Mono"
        );
    }

    #[test]
    fn sizes_round_to_device_pixels() {
        assert_eq!(pixel_size(14.0, 96), 14);
        assert_eq!(pixel_size(12.5, 96), 12);
        assert_eq!(pixel_size(12.5, 144), 19);
        assert_eq!(pixel_size(12.5, 120), 16);
        assert_eq!(pixel_size(20.0, 144), 30);
        assert_eq!(pixel_size(11.0, 120), 14);
        assert_eq!(pixel_size(0.1, 96), 1);
    }

    #[test]
    fn detection_finds_installed_and_rejects_missing_families() {
        assert!(is_installed("Segoe UI"));
        assert!(is_installed("Consolas"));
        assert!(!is_installed("Feather Missing Font 8231"));
        // Cached answers are stable.
        assert!(is_installed("Segoe UI"));
    }

    #[test]
    fn every_role_creates_a_font_with_the_requested_size_and_weight() {
        unsafe {
            let dc = CreateCompatibleDC(null_mut());
            for language in [Language::English, Language::Korean] {
                for dpi in [96, 144] {
                    let fonts = Fonts::new(dpi, language);
                    assert!(fonts.matches(dpi, language));
                    for role in Role::ALL {
                        let font = fonts.get(role);
                        assert!(!font.is_null(), "{role:?}");
                        let old = SelectObject(dc, font);
                        let mut metrics: TEXTMETRICW = zeroed();
                        assert_ne!(GetTextMetricsW(dc, &mut metrics), 0);
                        let (family, weight) = resolve(role, language);
                        let (_, size, css_weight) = role.spec();
                        assert_eq!(
                            metrics.tmHeight - metrics.tmInternalLeading,
                            pixel_size(size, dpi),
                            "{role:?} at {dpi}"
                        );
                        // A face that exists must deliver its real weight
                        // (not GDI's 600 → Bold substitution).
                        if is_installed(family) {
                            assert_eq!(metrics.tmWeight, weight, "{role:?} {family}");
                        }
                        assert_eq!(css_weight >= 600, weight >= 600, "{role:?}");
                        let mut name = [0u16; 64];
                        assert!(GetTextFaceW(dc, 64, name.as_mut_ptr()) > 0);
                        SelectObject(dc, old);
                    }
                }
            }
            DeleteDC(dc);
            let empty = Fonts::empty();
            assert!(!empty.matches(96, Language::English));
        }
    }

    fn wide16(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    /// Ink bounding box (left, top, right, bottom) of dark pixels.
    fn ink(dib: &mut Dib) -> Option<(i32, i32, i32, i32)> {
        let (w, h) = (dib.width(), dib.height());
        let pixels = dib.pixels().to_vec();
        let mut bounds: Option<(i32, i32, i32, i32)> = None;
        for y in 0..h {
            for x in 0..w {
                let p = pixels[(y * w + x) as usize];
                let gray = ((p >> 16 & 0xff) + (p >> 8 & 0xff) + (p & 0xff)) / 3;
                if gray < 160 {
                    bounds = Some(bounds.map_or((x, y, x, y), |(l, t, r, b)| {
                        (l.min(x), t.min(y), r.max(x), b.max(y))
                    }));
                }
            }
        }
        bounds
    }

    unsafe fn white(dib: &mut Dib) {
        dib.pixels().fill(0x00ff_ffff);
        SetBkMode(dib.dc(), TRANSPARENT as i32);
        SetTextColor(dib.dc(), 0);
    }

    /// Every role at 100/125/150/200 % in both languages draws and measures
    /// Hangul exactly like a same-size Malgun Gothic — GDI's linking alone
    /// shrank it at many sizes (e.g. 24 instead of 34 px for "성능" at 17 px).
    #[test]
    fn hangul_renders_like_malgun_gothic_for_every_role_and_dpi() {
        if !is_installed(MALGUN) {
            return;
        }
        unsafe {
            let text = wide16("성능");
            let mut dib = Dib::new(400, 160).unwrap();
            let mut reference = Dib::new(400, 160).unwrap();
            for language in [Language::English, Language::Korean] {
                for dpi in [96, 120, 144, 192] {
                    let fonts = Fonts::new(dpi, language);
                    for role in Role::ALL {
                        let (_, size, weight) = role.spec();
                        let mut logfont: LOGFONTW = zeroed();
                        logfont.lfHeight = -pixel_size(size, dpi);
                        logfont.lfWeight = if weight >= 600 { 700 } else { 400 };
                        logfont.lfCharSet = DEFAULT_CHARSET;
                        logfont.lfQuality = CLEARTYPE_QUALITY;
                        logfont.lfFaceName = face_name(MALGUN);
                        let malgun = CreateFontIndirectW(&logfont);
                        // Extent.
                        let old = SelectObject(dib.dc(), fonts.get(role));
                        let ours = text_extent(dib.dc(), &text);
                        let previous = SelectObject(reference.dc(), malgun);
                        let mut expected: SIZE = zeroed();
                        GetTextExtentPoint32W(reference.dc(), text.as_ptr(), 2, &mut expected);
                        assert_eq!(ours.cx, expected.cx, "{role:?} {dpi} {language:?}");
                        // Ink.
                        white(&mut dib);
                        white(&mut reference);
                        let mut r = RECT {
                            left: 10,
                            top: 10,
                            right: 390,
                            bottom: 150,
                        };
                        draw_text(dib.dc(), &text, &mut r, DT_SINGLELINE | DT_NOPREFIX);
                        let mut r = RECT {
                            left: 10,
                            top: 10,
                            right: 390,
                            bottom: 150,
                        };
                        DrawTextW(
                            reference.dc(),
                            text.as_ptr(),
                            2,
                            &mut r,
                            DT_SINGLELINE | DT_NOPREFIX,
                        );
                        let (a, b) = (ink(&mut dib).unwrap(), ink(&mut reference).unwrap());
                        assert_eq!(a.2 - a.0, b.2 - b.0, "ink width {role:?} {dpi}");
                        assert_eq!(a.3 - a.1, b.3 - b.1, "ink height {role:?} {dpi}");
                        SelectObject(dib.dc(), old);
                        SelectObject(reference.dc(), previous);
                        DeleteObject(malgun);
                    }
                }
            }
        }
    }

    /// Text without Hangul is DrawTextW's own output, and the run renderer
    /// reproduces DrawTextW's alignment, centring and ellipsis exactly (runs
    /// forced onto a Latin string with the primary font as its own partner).
    #[test]
    fn run_renderer_matches_drawtext_placement() {
        unsafe {
            let fonts = Fonts::new(96, Language::English);
            let mut ours = Dib::new(300, 60).unwrap();
            let mut native = Dib::new(300, 60).unwrap();
            let text = wide16("Feather 0.3% CPU · 18.9 MB");
            for font in [fonts.mono_small, fonts.body, fonts.h1] {
                for (flags, right, bottom) in [
                    (DT_LEFT | DT_VCENTER | DT_SINGLELINE, 290, 40),
                    (DT_RIGHT | DT_VCENTER | DT_SINGLELINE, 290, 41),
                    (DT_CENTER | DT_VCENTER | DT_SINGLELINE, 291, 33),
                    (DT_LEFT | DT_BOTTOM | DT_SINGLELINE, 290, 50),
                    (DT_LEFT | DT_SINGLELINE, 290, 50),
                    (
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                        120,
                        40,
                    ),
                    (
                        DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
                        131,
                        40,
                    ),
                    // A band shorter than the cell (text overflows upwards).
                    (DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOCLIP, 290, 22),
                ] {
                    let rect = RECT {
                        left: 6,
                        top: 8,
                        right,
                        bottom,
                    };
                    white(&mut ours);
                    white(&mut native);
                    SelectObject(ours.dc(), font);
                    SelectObject(native.dc(), font);
                    let mut metrics: TEXTMETRICW = zeroed();
                    GetTextMetricsW(ours.dc(), &mut metrics);
                    let runs = split_runs(&text, font, font, metrics).unwrap();
                    draw_runs(ours.dc(), &text, &runs, &rect, flags | DT_NOPREFIX);
                    let mut r = rect;
                    DrawTextW(
                        native.dc(),
                        text.as_ptr(),
                        text.len() as i32,
                        &mut r,
                        flags | DT_NOPREFIX,
                    );
                    assert_eq!(
                        ours.pixels().to_vec(),
                        native.pixels().to_vec(),
                        "flags {flags:#x}"
                    );
                }
            }
            // Plain text takes DrawTextW itself; Hangul in Malgun needs no runs.
            SelectObject(ours.dc(), fonts.body);
            assert!(hangul_runs(ours.dc(), &text, DT_SINGLELINE | DT_NOPREFIX).is_none());
            let korean = Fonts::new(96, Language::Korean);
            SelectObject(ours.dc(), korean.body);
            assert!(hangul_runs(ours.dc(), &wide16("성능"), DT_NOPREFIX).is_none());
            SelectObject(ours.dc(), korean.mono_small);
            let mixed = wide16("실시간 CPU 3%");
            let runs = hangul_runs(ours.dc(), &mixed, DT_SINGLELINE | DT_NOPREFIX).unwrap();
            assert_eq!(runs.spans, vec![(0, 3), (3, mixed.len())]);
            // Multi-line / prefix-processing requests are left to DrawTextW.
            assert!(hangul_runs(ours.dc(), &mixed, DT_WORDBREAK | DT_NOPREFIX).is_none());
            assert!(hangul_runs(ours.dc(), &mixed, DT_SINGLELINE).is_none());
        }
    }

    /// The reference's CSS line boxes (measured on its renders): the `.meta`
    /// mono line sits 2 px above GDI's DT_VCENTER, the 20 px h1 matches it in
    /// a 32 px head row and sits 1 px higher in Settings' 24 px row, and the
    /// `.kbd` content area is 13 px (GDI's cell is 15).
    #[test]
    fn css_line_boxes_follow_the_reference_baselines() {
        unsafe {
            let fonts = Fonts::new(96, Language::English);
            let dc = CreateCompatibleDC(null_mut());
            let band = |h: i32| RECT {
                left: 0,
                top: 59,
                right: 200,
                bottom: 59 + h,
            };
            let vcenter = |dc: HDC, h: i32| {
                let mut metrics: TEXTMETRICW = zeroed();
                GetTextMetricsW(dc, &mut metrics);
                59 + (h - metrics.tmHeight) / 2 + metrics.tmAscent
            };
            SelectObject(dc, fonts.mono_small);
            assert_eq!(css_metrics(dc), (11, 3));
            assert_eq!(css_baseline(dc, band(32), Some(17.4)), vcenter(dc, 32) - 2);
            SelectObject(dc, fonts.h1);
            assert_eq!(css_baseline(dc, band(32), Some(24.0)), vcenter(dc, 32));
            assert_eq!(css_baseline(dc, band(24), Some(24.0)), vcenter(dc, 24) - 1);
            let r = css_line_rect(dc, band(24), Some(24.0));
            let mut metrics: TEXTMETRICW = zeroed();
            GetTextMetricsW(dc, &mut metrics);
            assert_eq!(
                r.bottom - r.top,
                metrics.tmHeight,
                "a full cell: never clipped"
            );
            SelectObject(dc, fonts.mono_tiny);
            let (ascent, descent) = css_metrics(dc);
            assert_eq!(ascent + descent, 13);
            // "Ctrl K" at Cascadia's fractional advances (GDI: 36 px).
            let width = ideal_width(dc, "Ctrl K");
            assert!((38.0..39.5).contains(&width), "{width}");
            // Extra sizes are cached per (face, size, weight) and owned by the set.
            let a = fonts.extra(Face::Mono, 16.0, 400);
            assert!(!a.is_null());
            assert_eq!(fonts.extra(Face::Mono, 16.0, 400), a);
            assert_ne!(fonts.extra(Face::Text, 16.0, 400), a);
            assert!(Fonts::empty().extra(Face::Mono, 16.0, 400).is_null());
            DeleteDC(dc);
        }
    }

    /// Measured on the reference (client y, 100 %): the settings title and
    /// description baselines at 175 / 192 from the unrounded row top 160.5,
    /// the group row "Apps (8)" at 185 (its mono count's line box makes the
    /// line 1 px taller than the title's own).
    #[test]
    fn fractional_and_mixed_line_boxes_land_on_the_reference_baselines() {
        unsafe {
            let fonts = Fonts::new(96, Language::English);
            let dc = CreateCompatibleDC(null_mut());
            SelectObject(dc, fonts.ui_strong);
            let band = RECT {
                left: 0,
                top: 161,
                right: 200,
                bottom: 180,
            };
            assert_eq!(
                css_baseline_at(dc, 161.0, 19.0, Some(18.843_75)),
                css_baseline(dc, band, Some(18.843_75)),
                "integer bands are the same computation"
            );
            assert_eq!(css_baseline_at(dc, 160.5, 18.843_75, Some(18.843_75)), 175);
            SelectObject(dc, fonts.small);
            let desc = 160.5 + 18.843_75;
            assert_eq!(css_baseline_at(dc, desc, 17.390_625, Some(17.390_625)), 192);
            let cell = RECT {
                left: 0,
                top: 166,
                right: 200,
                bottom: 196,
            };
            let title_only = css_mixed_baseline(dc, cell, &[fonts.ui_strong], 18.85);
            SelectObject(dc, fonts.ui_strong);
            assert_eq!(title_only, css_baseline(dc, cell, Some(18.85)));
            SelectObject(dc, fonts.small);
            let mixed = css_mixed_baseline(dc, cell, &[fonts.ui_strong, fonts.mono_ui], 18.85);
            assert_eq!((title_only, mixed), (186, 185));
            assert_eq!(
                GetCurrentObject(dc, OBJ_FONT as u32),
                fonts.small as _,
                "the DC's font is restored"
            );
            DeleteDC(dc);
        }
    }
}
