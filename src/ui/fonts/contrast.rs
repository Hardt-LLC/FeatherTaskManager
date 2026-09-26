//! Light-on-dark text contrast, as the reference's Chromium/Skia renders it.
//!
//! Measured on the reference (ink = Σ clamp((bg − px)/(bg − fg)) over a
//! string): GDI ClearType text that is darker than its background matches
//! Chromium within 1 %, but text that is *lighter* than its background (the
//! whole dark theme, white labels on the primary/danger buttons, the toast)
//! comes out 9–20 % thinner. Skia lifts the coverage of such text with a
//! per-text-color lookup table (`SkTMaskGamma_build_correcting_lut`: the text
//! is blended as if in linear light over the "opposite" background, with a
//! contrast boost), GDI does not.
//!
//! So light text is drawn twice-removed: the same GDI call renders the text
//! black on white into a scratch mask (GDI's dark-on-light coverage, which
//! already matches), each ClearType channel of that coverage is mapped
//! through Skia's table for the real text color, and the text color is
//! blended over the pixels the text lands on (read back from the target, so
//! any background — rows, heat cells, buttons, popups — works). Clip region
//! and viewport origin of the target are honoured (BitBlt in logical units).
//! Dark text never takes this path: it stays plain GDI output.
//!
//! The table's gamma and contrast were fitted to the reference renders
//! (`window-*-dark.png`: the page title, brand, nav labels, muted header,
//! search placeholder and status-bar strings all within ±3 % ink).
use super::super::gfx::Dib;
use std::cell::{Cell, RefCell};
use std::mem::zeroed;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, GetBkMode, GetClipBox, GetCurrentObject, GetTextAlign, GetTextColor, SelectObject,
    SetBkMode, SetTextAlign, SetTextColor, CLR_INVALID, HDC, OBJ_FONT, SRCCOPY, TRANSPARENT,
};

/// Skia's text gamma (linear-light exponent of the correcting table).
pub(super) const TEXT_GAMMA: f32 = 2.3;
/// Skia's text contrast (`apply_contrast`, tapered by the assumed background).
pub(super) const TEXT_CONTRAST: f32 = 1.0;
/// Text darker than this (sRGB luma 0..1) is dark text: plain GDI, no read-back.
const DARK_TEXT: f32 = 0.2;
/// Areas larger than this (device px²) are drawn plainly (never in practice).
const MAX_AREA: i64 = 1 << 21;

/// sRGB luma (0..1) of a COLORREF.
fn luma(color: u32) -> f32 {
    let (r, g, b) = (color & 0xff, (color >> 8) & 0xff, (color >> 16) & 0xff);
    (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32) / 255.0
}

/// Skia's correcting tables for the 8 luminance levels a text color channel
/// is quantized to (3 bits, like Skia's LCD preblend): `table[q][ink]` maps
/// GDI's black-on-white coverage (0..255) to the blend fraction of a text
/// channel of level `q` (value `q·255/7`). Level 0 (black) is the identity.
pub(super) fn tables() -> &'static [[u8; 256]; 8] {
    static TABLES: OnceLock<[[u8; 256]; 8]> = OnceLock::new();
    TABLES.get_or_init(|| build(TEXT_GAMMA, TEXT_CONTRAST))
}

fn build(gamma: f32, contrast: f32) -> [[u8; 256]; 8] {
    let mut tables = [[0u8; 256]; 8];
    for (level, table) in tables.iter_mut().enumerate() {
        let src = level as f32 / 7.0;
        // Skia guesses the background as the perceptual inverse of the text.
        let dst = 1.0 - src;
        let (lin_src, lin_dst) = (src.powf(gamma), dst.powf(gamma));
        let k = contrast * lin_dst;
        for (i, out) in table.iter_mut().enumerate() {
            let m = i as f32 / 255.0;
            // The raw coverage GDI's black-on-white m stands for: Skia's
            // black-text table is srca → 1 − (1 − srca)^(1/γ) with srca the
            // contrasted coverage (contrast × 1 for a white background).
            let srca_black = 1.0 - (1.0 - m).powf(gamma);
            let raw = if contrast > 0.0 {
                let b = 1.0 + contrast;
                (b - (b * b - 4.0 * contrast * srca_black).max(0.0).sqrt()) / (2.0 * contrast)
            } else {
                srca_black
            };
            let srca = (raw + (1.0 - raw) * k * raw).clamp(0.0, 1.0);
            let result = if (src - dst).abs() < 1.0 / 256.0 {
                srca
            } else {
                let lin_out = lin_src * srca + (1.0 - srca) * lin_dst;
                (lin_out.powf(1.0 / gamma) - dst) / (src - dst)
            };
            *out = (result.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    tables
}

/// The background copy and the black-on-white mask (grow-only, per thread).
struct Scratch {
    background: Dib,
    mask: Dib,
}

thread_local! {
    static SCRATCH: RefCell<Option<Scratch>> = const { RefCell::new(None) };
    /// The color whose correcting table shapes the coverage (None: the text
    /// color). See [`with_lift_of`].
    static LIFT: Cell<Option<u32>> = const { Cell::new(None) };
}

/// Draw with the coverage (weight) Skia gives `color` while blending the
/// actual (e.g. `.45`-disabled) text color: Chromium draws a disabled
/// button's label in full fg and composites the whole button at opacity
/// .45, so its light label keeps the full color's lift instead of the much
/// weaker lift of the blended grey.
pub(super) fn with_lift_of<R>(color: u32, draw: impl FnOnce() -> R) -> R {
    let previous = LIFT.with(|lift| lift.replace(Some(color)));
    let result = draw();
    LIFT.with(|lift| lift.set(previous));
    result
}

unsafe fn scratch(slot: &mut Option<Scratch>, w: i32, h: i32) -> Option<&mut Scratch> {
    let fits = slot
        .as_ref()
        .is_some_and(|s| s.mask.width() >= w && s.mask.height() >= h);
    if !fits {
        let (old_w, old_h) = slot
            .as_ref()
            .map_or((0, 0), |s| (s.mask.width(), s.mask.height()));
        // Round up so a few sizes cover every label (no per-frame churn).
        let round = |v: i32| (v + 63) / 64 * 64;
        let (w, h) = (round(w.max(old_w)), round(h.max(old_h)));
        *slot = None;
        *slot = Some(Scratch {
            background: Dib::new(w, h)?,
            mask: Dib::new(w, h)?,
        });
    }
    slot.as_mut()
}

/// Whether text drawn on `dc` now could take the contrast path (a light text
/// color, transparent background): the cheap test before measuring.
///
/// # Safety
/// `dc` must be a valid DC.
pub(super) unsafe fn candidate(dc: HDC) -> bool {
    let fg = GetTextColor(dc);
    fg != CLR_INVALID && GetBkMode(dc) == TRANSPARENT as i32 && luma(fg) >= DARK_TEXT
}

/// Draw GDI text with the reference's light-on-dark contrast. `area` is the
/// logical rectangle the ink can reach; `paint(dc, dx, dy)` issues the GDI
/// text call on `dc` with every coordinate offset by (dx, dy), using the
/// font, color and alignment already set on `dc`. Returns None — without
/// calling `paint` — when the text is not lighter than what it is drawn on
/// (or the DC cannot be read back): the caller then draws plainly.
///
/// # Safety
/// `dc` must be a valid DC with the intended font, text color and
/// transparent background mode selected.
pub(super) unsafe fn draw<R>(
    dc: HDC,
    area: RECT,
    mut paint: impl FnMut(HDC, i32, i32) -> R,
) -> Option<R> {
    if !candidate(dc) {
        return None;
    }
    let fg = GetTextColor(dc);
    let mut clip: RECT = zeroed();
    if GetClipBox(dc, &mut clip) <= 1 {
        // ERROR or NULLREGION: nothing visible, the plain call is free.
        return None;
    }
    let area = RECT {
        left: area.left.max(clip.left),
        top: area.top.max(clip.top),
        right: area.right.min(clip.right),
        bottom: area.bottom.min(clip.bottom),
    };
    let (w, h) = (area.right - area.left, area.bottom - area.top);
    if w <= 0 || h <= 0 || w as i64 * h as i64 > MAX_AREA {
        return None;
    }
    SCRATCH.with(|cell| {
        let mut slot = cell.try_borrow_mut().ok()?;
        let s = scratch(&mut slot, w, h)?;
        let stride = s.mask.width() as usize;
        if BitBlt(
            s.background.dc(),
            0,
            0,
            w,
            h,
            dc,
            area.left,
            area.top,
            SRCCOPY,
        ) == 0
        {
            return None;
        }
        let (w, h) = (w as usize, h as usize);
        // Light *on dark*: the text must be lighter than what it covers.
        let background = s.background.pixels();
        let mut sum = 0.0f32;
        for y in 0..h {
            for &px in &background[y * stride..y * stride + w] {
                sum += luma(((px >> 16) & 0xff) | (px & 0xff00) | ((px & 0xff) << 16));
            }
        }
        if luma(fg) <= sum / (w * h) as f32 {
            return None;
        }
        let mask = s.mask.pixels();
        for y in 0..h {
            mask[y * stride..y * stride + w].fill(0x00ff_ffff);
        }
        let mdc = s.mask.dc();
        let font = SelectObject(mdc, GetCurrentObject(dc, OBJ_FONT as u32));
        SetTextColor(mdc, 0);
        SetBkMode(mdc, TRANSPARENT as i32);
        let align = SetTextAlign(mdc, GetTextAlign(dc));
        let result = paint(mdc, -area.left, -area.top);
        SetTextAlign(mdc, align);
        SelectObject(mdc, font);
        let t = tables();
        let lift = LIFT.with(Cell::get).unwrap_or(fg);
        let channel = |shift: u32| {
            let value = (fg >> shift) & 0xff;
            (value as i32, &t[(((lift >> shift) & 0xff) >> 5) as usize])
        };
        // COLORREF 0x00BBGGRR; DIB pixels 0xAARRGGBB.
        let (red, red_table) = channel(0);
        let (green, green_table) = channel(8);
        let (blue, blue_table) = channel(16);
        let mask = s.mask.pixels();
        let background = s.background.pixels();
        let blend = |bg: u32, ink: u32, fg: i32, table: &[u8; 256]| -> u32 {
            let t = table[(255 - ink) as usize] as i32;
            let bg = bg as i32;
            (bg + ((fg - bg) * t + if fg >= bg { 127 } else { -127 }) / 255) as u32
        };
        for y in 0..h {
            let row = y * stride;
            for x in row..row + w {
                let m = mask[x] & 0x00ff_ffff;
                if m == 0x00ff_ffff {
                    continue;
                }
                let px = background[x];
                let r = blend((px >> 16) & 0xff, (m >> 16) & 0xff, red, red_table);
                let g = blend((px >> 8) & 0xff, (m >> 8) & 0xff, green, green_table);
                let b = blend(px & 0xff, m & 0xff, blue, blue_table);
                background[x] = (px & 0xff00_0000) | (r << 16) | (g << 8) | b;
            }
        }
        BitBlt(
            dc,
            area.left,
            area.top,
            w as i32,
            h as i32,
            s.background.dc(),
            0,
            0,
            SRCCOPY,
        );
        Some(result)
    })
}

#[cfg(test)]
mod tests {
    use super::super::{draw_text, Fonts};
    use super::*;
    use crate::i18n::Language;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{
        DrawTextW, IntersectClipRect, SelectClipRgn, SetViewportOrgEx, DT_LEFT, DT_NOPREFIX,
        DT_SINGLELINE, DT_VCENTER, HFONT,
    };

    fn text() -> Vec<u16> {
        "Performance 13,837".encode_utf16().collect()
    }

    /// Σ ink of `fg` over `bg` (the reference-comparison metric).
    fn ink(pixels: &[u32], fg: u32, bg: u32) -> f32 {
        let sum = |c: u32| ((c >> 16) & 0xff) + ((c >> 8) & 0xff) + (c & 0xff);
        let (f, b) = (sum(fg) as f32, sum(bg) as f32);
        pixels
            .iter()
            .map(|&p| ((sum(p & 0x00ff_ffff) as f32 - b) / (f - b)).clamp(0.0, 1.0))
            .sum()
    }

    unsafe fn paint(dib: &mut Dib, bg: u32, fg: u32, plain: bool, font: HFONT) {
        dib.pixels().fill(bg);
        let dc = dib.dc();
        SelectObject(dc, font);
        // COLORREF from 0xRRGGBB.
        SetTextColor(
            dc,
            ((fg & 0xff) << 16) | (fg & 0xff00) | ((fg >> 16) & 0xff),
        );
        SetBkMode(dc, TRANSPARENT as i32);
        let text = text();
        let mut r = RECT {
            left: 4,
            top: 2,
            right: 196,
            bottom: 30,
        };
        let flags = DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX;
        if plain {
            DrawTextW(dc, text.as_ptr(), text.len() as i32, &mut r, flags);
        } else {
            draw_text(dc, &text, &mut r, flags);
        }
    }

    #[test]
    fn light_text_on_dark_gets_the_reference_weight_and_dark_text_is_untouched() {
        unsafe {
            let fonts = Fonts::new(96, Language::English);
            let mut ours = Dib::new(200, 32).unwrap();
            let mut gdi = Dib::new(200, 32).unwrap();
            for font in [fonts.body, fonts.ui_strong, fonts.mono_small] {
                // Dark on light: plain GDI output, pixel for pixel.
                paint(&mut ours, 0xffff_ffff, 0x121c23, false, font);
                paint(&mut gdi, 0xffff_ffff, 0x121c23, true, font);
                assert_eq!(ours.pixels().to_vec(), gdi.pixels().to_vec());
                // Light on dark: heavier than GDI, and heavier than GDI's
                // dark-on-light weight of the same string, like Chromium.
                let (fg, bg) = (0xe7ecf0, 0xff17_1e23);
                paint(&mut ours, bg, fg, false, font);
                paint(&mut gdi, bg, fg, true, font);
                let lifted = ink(ours.pixels(), fg, bg);
                let native = ink(gdi.pixels(), fg, bg);
                paint(&mut gdi, 0xffff_ffff, 0x000000, true, font);
                let dark = ink(gdi.pixels(), 0, 0xffff_ffff);
                assert!(lifted > native * 1.08, "{lifted} vs GDI {native}");
                assert!(
                    lifted > dark * 1.08 && lifted < dark * 1.6,
                    "{lifted} vs {dark}"
                );
                // Only the text changed: every pixel GDI leaves alone is bg,
                // and alpha bytes survive.
                paint(&mut gdi, bg, fg, true, font);
                for (a, b) in ours.pixels().iter().zip(gdi.pixels().iter()) {
                    if *b == bg {
                        assert_eq!(*a, bg);
                    }
                    assert_eq!(a >> 24, 0xff);
                }
            }
        }
    }

    #[test]
    fn light_text_honours_the_clip_region_and_the_viewport_origin() {
        unsafe {
            let fonts = Fonts::new(96, Language::English);
            let (fg, bg) = (0xffffff, 0xff0f_1519);
            let mut full = Dib::new(200, 32).unwrap();
            paint(&mut full, bg, fg, false, fonts.body);
            // Clipped to x 40..90: the same pixels inside, bg outside.
            let mut clipped = Dib::new(200, 32).unwrap();
            IntersectClipRect(clipped.dc(), 40, 0, 90, 32);
            paint(&mut clipped, bg, fg, false, fonts.body);
            SelectClipRgn(clipped.dc(), std::ptr::null_mut());
            for y in 0..32 {
                for x in 0..200 {
                    let (a, b) = (clipped.pixel(x, y), full.pixel(x, y));
                    if (40..90).contains(&x) {
                        assert_eq!(a, b, "{x},{y}");
                    } else {
                        assert_eq!(a, bg, "{x},{y}");
                    }
                }
            }
            // A viewport origin shifts the text like any GDI call.
            let mut shifted = Dib::new(200, 32).unwrap();
            let mut old = POINT { x: 0, y: 0 };
            SetViewportOrgEx(shifted.dc(), -3, 1, &mut old);
            paint(&mut shifted, bg, fg, false, fonts.body);
            SetViewportOrgEx(shifted.dc(), 0, 0, &mut old);
            for y in 1..31 {
                for x in 0..190 {
                    assert_eq!(shifted.pixel(x, y + 1), full.pixel(x + 3, y), "{x},{y}");
                }
            }
        }
    }

    #[test]
    fn correcting_tables_lift_light_text_and_leave_black_text_alone() {
        let t = build(TEXT_GAMMA, TEXT_CONTRAST);
        for (i, &v) in t[0].iter().enumerate() {
            assert!(
                (v as i32 - i as i32).abs() <= 1,
                "black is the identity: {i} → {v}"
            );
        }
        for table in &t {
            assert_eq!((table[0], table[255]), (0, 255));
            assert!(table.windows(2).all(|w| w[0] <= w[1]), "monotonic");
        }
        // White text: half coverage becomes ~3/4 ink (Skia's gamma hack).
        assert!((180..=205).contains(&t[7][128]), "{}", t[7][128]);
        // The lift shrinks toward mid-grey text (muted labels).
        assert!(t[4][128] < t[7][128] && t[4][128] > 128, "{}", t[4][128]);
        // No gamma, no contrast: identity everywhere.
        let flat = build(1.0, 0.0);
        for table in &flat {
            for (i, &v) in table.iter().enumerate() {
                assert!((v as i32 - i as i32).abs() <= 1);
            }
        }
    }
}
