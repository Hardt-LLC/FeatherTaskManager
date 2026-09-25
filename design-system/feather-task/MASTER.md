# Feather Task Manager — desktop design system

Source: [UI UX Pro Max skill](https://github.com/nextlevelbuilder/ui-ux-pro-max-skill), applied 2026-09-26. Read its SKILL.md, pro-rules.md and quick-reference.md. Ran `system monitoring dashboard desktop --design-system`, refined with `desktop utility fluent --design-system`, and verified the exact `Fluent 2 desktop --domain style` match.

Source revision: `dcc40ff5133ef78276117db0cc34e7b83cc8aeba`. Skill checkout stays in `vendor/` and is not a runtime dependency.

The generator's marketing-page sections and web fonts are not suitable for this native utility. Apply its verified Fluent 2 style guidance and minimal utility layout principles; preserve Rust/Win32 native controls, system fonts and a small binary. No blur, decorative animation or web runtime.

## Layout
- Calm light workspace with a dark navigation rail, 4 labeled pages: 프로세스, 성능, 시작 앱, 서비스.
- Main content begins at 220 dp; navigation width 196 dp. Spacing follows 4/8 dp increments, gutters 24 dp.
- Page header has one title, one short explanation, live/paused feedback and consistent toolbar.
- Process, startup and service tables use virtual native ListView controls, 32 dp row rhythm, readable headers and numeric alignment. Selection and hover are visible. Actions are tied to the selected item.
- Performance uses four time-series panels: CPU, memory, disk and network; disk/GPU availability is stated explicitly. No data is fabricated.
- Font: Malgun Gothic for Korean labels and Segoe UI for numeric metrics. Title 28 dp, labels/body 14 dp, secondary 12 dp, metrics 28 dp. Preserve DPI scaling and native keyboard focus.

## Tokens
- workspace #F5F7FA; surface #FFFFFF; primary text #17233A; muted text #59667B; border #DCE3ED.
- navigation #152036; navigation text #B8C4D8; navigation active #263D63 and white label.
- brand #2563EB, selection #EAF1FF; success #157A52; warning #956000; destructive #B42332.
- Radius: panels 10 dp; controls 6 dp. Thin dividers, no gradients/shadows/blur.
- Charts: CPU blue, memory violet, disk teal, network orange; always pair colors with labels and current values.

## Interaction and resource budget
- Ctrl+1…4 selects pages; Ctrl+F searches; F5 refreshes; Escape clears search; Delete only ends selected process after confirmation.
- Native buttons expose names, disabled semantics, keyboard focus and pressed states. Search has a persistent visible label, plus contextual placeholder.
- Lazy-load startup/services; performance counters run only on its page. Minimize pauses periodic monitoring. No animation timers.
- Show loading, empty, failure and stale states. Never display unavailable metrics as successfully measured zero.
- Start/stop services and enable/disable startup only on explicit user action; retain selected identity during confirmation and asynchronous work.
- Settings uses an on-demand native popup for administrator launch and Windows Task Manager replacement/restore. Show the current association; disable changes that would overwrite another program. Explain the all-user scope, permanent installation location and restore-before-delete requirement before UAC. No extra background polling or elevation during ordinary startup.

## Verification
- Contrast for normal text >=4.5:1; icons/state boundaries >=3:1 when meaningful.
- Native control integration tests for all page switches, search/sort, selection preservation and action enablement.
- Render actual native client surfaces at normal and high DPI, review clipping, state visibility and table alignment.
- Native desktop rules take precedence over skill examples about phone safe areas, CSS, marketing CTAs or touch-only layout.

Measured contrast: primary text on white 15.70:1; secondary text on workspace 5.42:1; navigation labels 9.23:1; selected navigation 10.88:1; white primary button label 5.17:1.

## Icon refinement — 0.2.1
- Product name: **Feather Task Manager**. Sidebar uses a two-line wordmark; title bar and Windows product metadata use the complete name.
- Feather silhouette has a curved asymmetric vane, visible rachis, open barbs and extended quill. The same geometry is used in the sidebar and app icon.
- Original SVG artwork lives in `assets/source`. Navigation uses a consistent 24×24 viewBox, 1.8-unit round strokes, square aspect ratio and vertically centered placement.
- Tab glyphs: process list, performance pulse, startup power, service gear. Selected and unselected icons change tint without changing shape or stroke weight.
- Build-time SVG rasterization emits coverage masks at 100/125/150/175/200/250/300/400% scale. Runtime GDI uses a bounded cache of premultiplied bitmaps; unusual DPI uses area-resampled coverage. No SVG/image engine is shipped.
- Windows ICO embeds 16/20/24/32/40/48/64/128/256-pixel images. Native big/small window icons reload when DPI changes.
- Regenerate artwork with `node scripts/generate-icons.cjs [absolute-path-to-sharp]`. Checked-in generated assets allow ordinary Rust builds without Node or Sharp.
