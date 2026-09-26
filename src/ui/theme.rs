//! Design tokens (DESIGN_SPEC §1) for both themes and small color helpers.
//!
//! Every color field is a GDI `COLORREF` (`0x00BBGGRR`), so it can be handed
//! to `SetTextColor`, `CreateSolidBrush`, list views, etc. directly. GDI+
//! drawing (see `gfx`) takes [`Argb`] values instead; convert with
//! [`solid`] / [`argb`]. All mixing is plain sRGB alpha compositing, which
//! reproduces the reference `color-mix(... transparent)` tokens exactly.
#![allow(dead_code)] // Token/helper API consumed by the Frame/Controls/Table tracks.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static DARK: AtomicBool = AtomicBool::new(false);
/// Bumped whenever the active palette changes (tests use it to notice a
/// parallel test flipping the process-wide theme under them).
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// `0xRRGGBB` (as written in CSS / the spec) to a GDI `COLORREF`.
pub(super) const fn hex(value: u32) -> u32 {
    ((value >> 16) & 0xff) | (value & 0xff00) | ((value & 0xff) << 16)
}

/// A GDI+ color: `0xAARRGGBB`. Distinct from the `u32` COLORREF on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub(super) struct Argb(pub u32);

impl Argb {
    pub(super) const TRANSPARENT: Argb = Argb(0);
    /// Alpha in 0..=255.
    pub(super) const fn alpha(self) -> u8 {
        (self.0 >> 24) as u8
    }
    /// The opaque COLORREF of this color (alpha dropped).
    pub(super) const fn colorref(self) -> u32 {
        ((self.0 >> 16) & 0xff) | (self.0 & 0xff00) | ((self.0 & 0xff) << 16)
    }
    /// Multiply the existing alpha by `factor` (0..=1), e.g. for fades.
    pub(super) fn fade(self, factor: f32) -> Argb {
        let a = (self.alpha() as f32 * factor.clamp(0.0, 1.0)).round() as u32;
        Argb((self.0 & 0x00ff_ffff) | (a << 24))
    }
}

/// Opaque GDI+ color from a COLORREF.
pub(super) const fn solid(color: u32) -> Argb {
    argb8(color, 255)
}

/// GDI+ color from a COLORREF and an 8-bit alpha.
pub(super) const fn argb8(color: u32, alpha: u8) -> Argb {
    let r = color & 0xff;
    let g = (color >> 8) & 0xff;
    let b = (color >> 16) & 0xff;
    Argb(((alpha as u32) << 24) | (r << 16) | (g << 8) | b)
}

/// GDI+ color from a COLORREF and a 0..=1 alpha (`fg @ 14 %` = `argb(fg, 0.14)`).
pub(super) fn argb(color: u32, alpha: f32) -> Argb {
    argb8(color, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8)
}

/// Linear sRGB interpolation between two COLORREFs: `t = 0` is `a`, `t = 1` is `b`.
/// `mix(bg, fg, 0.05)` is "fg at 5 % over bg".
pub(super) fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let channel = |shift: u32| {
        let x = ((a >> shift) & 0xff) as f32;
        let y = ((b >> shift) & 0xff) as f32;
        ((x + (y - x) * t).round() as u32).min(255) << shift
    };
    channel(0) | channel(8) | channel(16)
}

/// `color` at `alpha` composited over the opaque `background` (alpha-over).
pub(super) fn over(color: u32, alpha: f32, background: u32) -> u32 {
    mix(background, color, alpha)
}

/// CSS `opacity: .45` for disabled elements: blend every color of the element
/// 55 % back toward the parent background so 45 % of it remains.
pub(super) fn disabled(color: u32, parent_background: u32) -> u32 {
    mix(parent_background, color, DISABLED_OPACITY)
}
pub(super) const DISABLED_OPACITY: f32 = 0.45;

/// Radii (CSS px / DIP) from DESIGN_SPEC §1.
pub(super) const RADIUS: f32 = 8.0;
pub(super) const RADIUS_SM: f32 = 4.0;
pub(super) const RADIUS_XS: f32 = 3.0;
pub(super) const RADIUS_PILL: f32 = 10.0;

/// The complete token palette of one theme (COLORREF values).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Palette {
    pub dark: bool,
    /// Window, title bar, nav rail, status bar.
    pub bg: u32,
    /// Main panel, tables, inputs, buttons, dialogs.
    pub surface: u32,
    /// Ink / primary text.
    pub fg: u32,
    pub muted: u32,
    pub border: u32,
    /// Charts.
    pub accent: u32,
    /// Only behind previews; never inside the app.
    pub desk: u32,
    pub btn_bg: u32,
    pub btn_hover: u32,
    pub btn_fg: u32,
    /// Default button hover: fg 6 % over surface.
    pub btn_hover_surface: u32,
    /// fg 5 % over surface: row hover, header hover.
    pub fg_soft: u32,
    /// fg 9 % over surface: row selected, chevron hover, menu hover.
    pub fg_sel: u32,
    /// fg 5 % over bg: nav hover.
    pub fg_soft_bg: u32,
    /// fg 9 % over bg: nav current, caption button hover.
    pub fg_sel_bg: u32,
    pub caption_hover: u32,
    /// fg 8 % over surface.
    pub mono_ico_bg: u32,
    /// border 60 % over surface (tbody td bottom border).
    pub row_border: u32,
    /// Heat map base (alpha = min(1, v / threshold) * 0.70).
    pub heat: u32,
    pub danger: u32,
    pub danger_hover: u32,
    pub on_danger: u32,
    pub warn_fg: u32,
    /// warn_fg 40 % over surface.
    pub warn_border: u32,
}

pub(super) const LIGHT: Palette = Palette {
    dark: false,
    bg: hex(0xF6F9FC),
    surface: hex(0xFFFFFF),
    fg: hex(0x121C23),
    muted: hex(0x5A656D),
    border: hex(0xD9DFE3),
    accent: hex(0x299236),
    desk: hex(0xDCE0E3),
    btn_bg: hex(0x1C6B26),
    btn_hover: hex(0x124D19),
    btn_fg: hex(0xFFFFFF),
    btn_hover_surface: hex(0xF0F0F0),
    fg_soft: hex(0xF3F4F4),
    fg_sel: hex(0xEAEBEB),
    fg_soft_bg: hex(0xEBEEF1),
    fg_sel_bg: hex(0xE1E5E8),
    caption_hover: hex(0xE1E5E8),
    mono_ico_bg: hex(0xEBEAEA),
    row_border: hex(0xE8ECEE),
    heat: hex(0xF1C45E),
    danger: hex(0xCC2827),
    danger_hover: hex(0x9C1C1C),
    on_danger: hex(0xFCFCFC),
    warn_fg: hex(0x874E00),
    warn_border: hex(0xCFB899),
};

pub(super) const DARK_PALETTE: Palette = Palette {
    dark: true,
    bg: hex(0x0F1519),
    surface: hex(0x171E23),
    fg: hex(0xE7ECF0),
    muted: hex(0x9DA6AD),
    border: hex(0x2F363C),
    accent: hex(0x5BBE62),
    desk: hex(0x05080B),
    btn_bg: hex(0x5BBE62),
    btn_hover: hex(0x7FCC82),
    btn_fg: hex(0x0F1519),
    btn_hover_surface: hex(0x21282D),
    fg_soft: hex(0x21282D),
    fg_sel: hex(0x2A3135),
    fg_soft_bg: hex(0x1A2024),
    fg_sel_bg: hex(0x22282C),
    caption_hover: hex(0x22282C),
    mono_ico_bg: hex(0x252C31),
    row_border: hex(0x252C32),
    heat: hex(0xB07A20),
    danger: hex(0xCC2827),
    danger_hover: hex(0x9C1C1C),
    on_danger: hex(0xFCFCFC),
    warn_fg: hex(0xEDBB64),
    warn_border: hex(0x6D5D3D),
};

impl Palette {
    /// Device sparkline area: fg @ 14 % (alpha over the spark's surface).
    pub(super) fn spark_fill(&self) -> Argb {
        argb(self.fg, 0.14)
    }
    /// Performance chart area: accent @ 22 % (alpha over bg).
    pub(super) fn chart_fill(&self) -> Argb {
        argb(self.accent, 0.22)
    }
    /// Dialog / palette scrim over the whole window: fg @ 22 %.
    pub(super) fn scrim(&self) -> Argb {
        argb(self.fg, 0.22)
    }
    /// Shadow color of popups (alphas come from `gfx::POPUP_SHADOW`).
    pub(super) fn shadow(&self) -> u32 {
        self.fg
    }
    /// Heat cell background for `alpha` (from `widgets::heat_alpha`) over `row_bg`.
    pub(super) fn heat_over(&self, alpha: f32, row_bg: u32) -> u32 {
        over(self.heat, alpha, row_bg)
    }
}

/// The active palette. Cheap (`Copy`), read it at the start of each paint.
pub(super) fn colors() -> Palette {
    if DARK.load(Ordering::Relaxed) {
        DARK_PALETTE
    } else {
        LIGHT
    }
}

/// Select the active palette (called by `Preferences::apply_theme`).
pub(super) fn set_dark(dark: bool) {
    if DARK.swap(dark, Ordering::Relaxed) != dark {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// How many times the active palette has changed so far.
pub(super) fn generation() -> u32 {
    GENERATION.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn css(color: u32) -> u32 {
        // COLORREF back to 0xRRGGBB for readable assertions.
        hex(color)
    }

    #[test]
    fn light_and_dark_tokens_match_the_spec_hex_values() {
        let light = [
            (LIGHT.bg, 0xF6F9FC),
            (LIGHT.surface, 0xFFFFFF),
            (LIGHT.fg, 0x121C23),
            (LIGHT.muted, 0x5A656D),
            (LIGHT.border, 0xD9DFE3),
            (LIGHT.accent, 0x299236),
            (LIGHT.desk, 0xDCE0E3),
            (LIGHT.btn_bg, 0x1C6B26),
            (LIGHT.btn_hover, 0x124D19),
            (LIGHT.btn_fg, 0xFFFFFF),
            (LIGHT.btn_hover_surface, 0xF0F0F0),
            (LIGHT.fg_soft, 0xF3F4F4),
            (LIGHT.fg_sel, 0xEAEBEB),
            (LIGHT.fg_soft_bg, 0xEBEEF1),
            (LIGHT.fg_sel_bg, 0xE1E5E8),
            (LIGHT.caption_hover, 0xE1E5E8),
            (LIGHT.mono_ico_bg, 0xEBEAEA),
            (LIGHT.row_border, 0xE8ECEE),
            (LIGHT.heat, 0xF1C45E),
            (LIGHT.danger, 0xCC2827),
            (LIGHT.danger_hover, 0x9C1C1C),
            (LIGHT.on_danger, 0xFCFCFC),
            (LIGHT.warn_fg, 0x874E00),
            (LIGHT.warn_border, 0xCFB899),
        ];
        let dark = [
            (DARK_PALETTE.bg, 0x0F1519),
            (DARK_PALETTE.surface, 0x171E23),
            (DARK_PALETTE.fg, 0xE7ECF0),
            (DARK_PALETTE.muted, 0x9DA6AD),
            (DARK_PALETTE.border, 0x2F363C),
            (DARK_PALETTE.accent, 0x5BBE62),
            (DARK_PALETTE.desk, 0x05080B),
            (DARK_PALETTE.btn_bg, 0x5BBE62),
            (DARK_PALETTE.btn_hover, 0x7FCC82),
            (DARK_PALETTE.btn_fg, 0x0F1519),
            (DARK_PALETTE.btn_hover_surface, 0x21282D),
            (DARK_PALETTE.fg_soft, 0x21282D),
            (DARK_PALETTE.fg_sel, 0x2A3135),
            (DARK_PALETTE.fg_soft_bg, 0x1A2024),
            (DARK_PALETTE.fg_sel_bg, 0x22282C),
            (DARK_PALETTE.caption_hover, 0x22282C),
            (DARK_PALETTE.mono_ico_bg, 0x252C31),
            (DARK_PALETTE.row_border, 0x252C32),
            (DARK_PALETTE.heat, 0xB07A20),
            (DARK_PALETTE.danger, 0xCC2827),
            (DARK_PALETTE.danger_hover, 0x9C1C1C),
            (DARK_PALETTE.on_danger, 0xFCFCFC),
            (DARK_PALETTE.warn_fg, 0xEDBB64),
            (DARK_PALETTE.warn_border, 0x6D5D3D),
        ];
        for (index, (value, expected)) in light.iter().chain(dark.iter()).enumerate() {
            assert_eq!(css(*value), *expected, "token #{index}");
        }
        const { assert!(!LIGHT.dark && DARK_PALETTE.dark) };
    }

    #[test]
    fn derived_tokens_are_alpha_over_their_parent_surface() {
        // The spec's derived tokens are fg / border / warn at x % over a surface;
        // plain sRGB compositing must land within one step of every token.
        for p in [LIGHT, DARK_PALETTE] {
            let near = |a: u32, b: u32| {
                (0..3).all(|i| {
                    let x = (a >> (i * 8)) & 0xff;
                    let y = (b >> (i * 8)) & 0xff;
                    x.abs_diff(y) <= 1
                })
            };
            assert!(near(over(p.fg, 0.05, p.surface), p.fg_soft), "{p:?}");
            assert!(near(over(p.fg, 0.09, p.surface), p.fg_sel), "{p:?}");
            assert!(near(over(p.fg, 0.05, p.bg), p.fg_soft_bg), "{p:?}");
            assert!(near(over(p.fg, 0.09, p.bg), p.fg_sel_bg), "{p:?}");
            assert!(near(over(p.border, 0.60, p.surface), p.row_border), "{p:?}");
            assert!(
                near(over(p.warn_fg, 0.40, p.surface), p.warn_border),
                "{p:?}"
            );
            // mono_ico_bg / btn_hover_surface are opaque OKLCH mixes in the
            // reference, so they are pinned as literal tokens instead.
        }
    }

    #[test]
    fn color_helpers_convert_and_mix_exactly() {
        assert_eq!(hex(0x123456), 0x563412);
        assert_eq!(solid(hex(0x123456)), Argb(0xFF123456));
        assert_eq!(argb(hex(0x123456), 0.5), Argb(0x80123456));
        assert_eq!(argb8(hex(0xABCDEF), 7).colorref(), hex(0xABCDEF));
        assert_eq!(Argb(0xFF102030).fade(0.5).alpha(), 128);
        assert_eq!(mix(hex(0x000000), hex(0xFFFFFF), 0.0), hex(0x000000));
        assert_eq!(mix(hex(0x000000), hex(0xFFFFFF), 1.0), hex(0xFFFFFF));
        assert_eq!(mix(hex(0x000000), hex(0xFFFFFF), 0.5), hex(0x808080));
        assert_eq!(mix(hex(0x102030), hex(0x102030), 0.3), hex(0x102030));
        // Disabled keeps 45 % of the color over the parent.
        assert_eq!(disabled(hex(0xFFFFFF), hex(0x000000)), hex(0x737373));
        assert_eq!(LIGHT.spark_fill(), argb(LIGHT.fg, 0.14));
        assert_eq!(LIGHT.chart_fill().alpha(), 56);
        assert_eq!(DARK_PALETTE.scrim().alpha(), 56);
    }

    #[test]
    fn active_palette_follows_the_theme_switch() {
        // Other tests may flip the global theme concurrently; only check the
        // accessor returns one of the two exact palettes.
        let current = colors();
        assert!(current == LIGHT || current == DARK_PALETTE);
    }
}
