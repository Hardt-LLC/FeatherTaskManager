//! Page painting on the main window's back buffer: GDI ClearType text over
//! antialiased GDI+ shapes (see `gfx`), design tokens (`theme`), font roles
//! (`fonts`) and component painters (`widgets`). No animation/render loop.
use super::gfx::{Canvas, RectF};
use super::theme::{argb, solid};
use super::widgets::{self, ButtonState, ButtonStyle, FieldState, Icon, Painter};
use super::*;

unsafe fn fill(dc: HDC, r: RECT, color: u32) {
    let b = CreateSolidBrush(color);
    FillRect(dc, &r, b);
    DeleteObject(b);
}
unsafe fn label(dc: HDC, font: HFONT, color: u32, value: &str, mut r: RECT, flags: u32) {
    let old = SelectObject(dc, font);
    SetTextColor(dc, color);
    SetBkMode(dc, TRANSPARENT as i32);
    fonts::draw_str(dc, value, &mut r, DT_NOPREFIX | flags);
    SelectObject(dc, old);
}
unsafe fn text_box(p: *mut App, dc: HDC, value: &str, r: RECT, font: HFONT, color: u32) {
    let _ = p;
    label(
        dc,
        font,
        color,
        value,
        r,
        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
}
pub(super) fn human_bytes(value: f64) -> String {
    if !value.is_finite() {
        return "—".into();
    }
    if value >= 1099511627776.0 {
        format!("{:.1} TB", value / 1099511627776.0)
    } else if value >= 1073741824.0 {
        format!("{:.1} GB", value / 1073741824.0)
    } else if value >= 1048576.0 {
        format!("{:.1} MB", value / 1048576.0)
    } else if value >= 1024.0 {
        format!("{:.1} KB", value / 1024.0)
    } else {
        format!("{value:.0} B")
    }
}
fn uptime(seconds: u64) -> String {
    let days = seconds / 86400;
    let hours = seconds % 86400 / 3600;
    let mins = seconds % 3600 / 60;
    if days > 0 {
        tf!("{days}일 {hours}시간", "{days}d {hours}h")
    } else {
        tf!("{hours}시간 {mins}분", "{hours}h {mins}m")
    }
}
fn percent(value: Option<f64>) -> String {
    value
        .filter(|v| v.is_finite())
        .map_or_else(|| "—".into(), |v| format!("{v:.1}%"))
}
fn bytes(value: Option<u64>) -> String {
    value.map_or_else(|| "—".into(), |v| human_bytes(v as f64))
}
/// A temperature the hardware reported, e.g. "39 °C".
fn celsius(value: f64) -> String {
    format!("{value:.0} \u{00B0}C")
}
/// A cache size like Task Manager's: "640 KB", "8.0 MB".
fn cache_size(bytes: u64) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else {
        format!("{} KB", bytes / 1024)
    }
}
/// A link speed in bits per second: "2.5 Gbps", "866 Mbps".
fn link_speed(bits: u64) -> String {
    if bits >= 1_000_000_000 {
        let gbps = format!("{:.1}", bits as f64 / 1e9);
        format!("{} Gbps", gbps.trim_end_matches(".0"))
    } else {
        format!("{:.0} Mbps", bits as f64 / 1e6)
    }
}
fn yes_no(value: Option<bool>) -> String {
    match value {
        Some(true) => tr("예", "Yes").into(),
        Some(false) => tr("아니요", "No").into(),
        None => "—".into(),
    }
}
fn network_kind(kind: crate::performance::NetworkKind) -> &'static str {
    use crate::performance::NetworkKind::*;
    match kind {
        Ethernet => tr("이더넷", "Ethernet"),
        WiFi => "Wi-Fi",
        MobileBroadband => tr("모바일 광대역", "Mobile broadband"),
        Bluetooth => "Bluetooth",
        Other => tr("기타", "Other"),
    }
}
/// A disk's media and bus: "SSD (NVMe)", "HDD (SATA)", "Removable (USB)"
/// (a disk that reports no seek penalty is neither SSD nor HDD).
fn disk_type(device: &crate::storage::StorageDevice) -> Option<String> {
    let bus = device.bus.map(|b| b.label());
    match (device.media_label(), bus) {
        (Some(media), Some(bus)) => Some(format!("{media} ({bus})")),
        (Some(media), None) => Some(media.to_owned()),
        (None, Some(bus)) if device.removable == Some(true) => {
            Some(tf!("이동식 ({})", "Removable ({})", bus))
        }
        (None, bus) => bus,
    }
}
fn rate(value: Option<f64>) -> String {
    value
        .filter(|v| v.is_finite())
        .map_or_else(|| "—".into(), |v| format!("{}/s", human_bytes(v)))
}

/// WM_PAINT: the whole client is composed on the cached back buffer and
/// copied once, so nothing flickers.
pub(super) unsafe fn paint(p: *mut App) {
    let mut back = std::mem::take(&mut (*p).back);
    gfx::paint_buffered((*p).hwnd, &mut back, |dc, _| paint_to(p, dc));
    (*p).back = back;
}
/// Compose the whole client into the back buffer without presenting it
/// (`ui::present_all`); false when the buffer could not be allocated.
pub(super) unsafe fn render_back(p: *mut App) -> bool {
    let mut client: RECT = zeroed();
    GetClientRect((*p).hwnd, &mut client);
    let mut back = std::mem::take(&mut (*p).back);
    let rendered = match back.prepare(client.right, client.bottom) {
        Some((dc, _)) => {
            let saved = SaveDC(dc);
            paint_to(p, dc);
            RestoreDC(dc, saved);
            true
        }
        None => false,
    };
    (*p).back = back;
    rendered
}
pub(super) unsafe fn paint_to(p: *mut App, dc: HDC) {
    let l = current_layout(p);
    let c = colors();
    // Title strip and nav rail share the window background, which also
    // shows outside the main panel's rounded corner.
    fill(dc, l.titlebar, c.bg);
    fill(dc, l.rail, c.bg);
    // The sliding nav indicator where it crosses the gaps between items
    // (each nav item draws its own part; see `nav_indicator`).
    nav_indicator(
        p,
        &Painter::new(dc, (*p).dpi, &(*p).fonts),
        POINT { x: 0, y: 0 },
    );
    let corner = l.px(theme::RADIUS);
    fill(
        dc,
        RECT {
            left: l.main.left,
            top: l.main.top,
            right: l.main.left + corner,
            bottom: l.main.top + corner,
        },
        c.bg,
    );
    // Main panel: surface with a 1 px border on the top and left only and the
    // antialiased 8 px top-left radius (the bg shows outside the curve). The
    // right/bottom borders of the box fall outside the client / under the
    // status bar's own top border.
    {
        let hair = l.hair as f32;
        Canvas::new(dc).bordered_round_rect(
            RectF::ltrb(
                l.main.left as f32,
                l.main.top as f32,
                l.main.right as f32 + hair,
                l.main.bottom as f32 + hair,
            ),
            gfx::Radii::new(gfx::px((*p).dpi, theme::RADIUS), 0.0, 0.0, 0.0),
            hair,
            solid(c.surface),
            solid(c.border),
        );
    }
    title_strip(p, dc, &l);
    page_head(p, dc, &l);
    // The drawer / details rects are the ones the list was placed around.
    let (_, drawer, details) = view_split(p, &l);
    match (*p).page {
        Page::Performance => performance_panel(p, dc, &l),
        Page::Settings => settings_page(p, dc, &l),
        _ => {}
    }
    if let Some(drawer) = drawer {
        process_telemetry(p, dc, &l, drawer);
    }
    if let Some(panel) = details {
        service_details(p, dc, &l, panel);
    }
    status_bar(p, dc, &l);
    // Pressed page-head buttons' translated bottom rows, then the
    // `:focus-visible` ring 2 px outside the focused control.
    controls::paint_pressed_rows(p, dc);
    controls::paint_focus_ring(p, dc);
}
/// Title strip = the window caption: brand at x 16, the search box and the
/// caption buttons (`frame.rs`); brand and glyphs are muted while inactive.
unsafe fn title_strip(p: *mut App, dc: HDC, l: &layout::Layout) {
    let c = colors();
    let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
    let icon = l.brand_icon;
    let brand = if frame::is_active(p) { c.fg } else { c.muted };
    widgets::icon(
        &pt,
        Icon::Feather,
        icon.left as f32,
        icon.top as f32,
        (icon.right - icon.left) as f32,
        brand,
    );
    pt.label(
        (*p).fonts.ui,
        brand,
        "Feather Task Manager",
        l.brand_text,
        DT_LEFT,
    );
    // Disabled on Performance / Settings: same face, its own placeholder
    // (painted by the input), like the reference's disabled search.
    let enabled = !matches!((*p).page, Page::Performance | Page::Settings);
    widgets::search_box(
        &pt,
        l.search,
        &FieldState {
            focused: enabled && GetFocus() == (*p).search,
            disabled: !enabled,
            ..FieldState::default()
        },
        None,
        Some("Ctrl K"),
        c.bg,
    );
    frame::paint(p, &pt, l);
}
/// A page-meta honesty note: the full sentence and a shorter wording used
/// when the head is too narrow for it.
pub(super) type MetaNote = (&'static str, &'static str);
/// The page's meta text (12 px mono muted) and an optional honesty note
/// (12 px muted) that continues it: what is not measured / where data
/// comes from stays visible next to the title.
pub(super) unsafe fn page_meta(p: *mut App) -> (String, Option<MetaNote>) {
    match (*p).page {
        Page::Processes => (
            (*p).snapshot.as_ref().map_or_else(String::new, |s| {
                tf!("{}개 프로세스", "{} processes", s.processes.len())
            }),
            (*p).network_state
                .as_ref()
                .filter(|state| !state.measured)
                .and_then(|state| state.reason.as_deref())
                .map(network_note),
        ),
        Page::Performance => (tr("최근 60초", "Last 60 seconds").into(), None),
        Page::Startup => (
            if (*p).startup_loaded {
                let enabled = (*p).startup.iter().filter(|s| s.enabled).count();
                tf!(
                    "{1}개 중 {0}개 사용",
                    "{0} of {1} enabled",
                    enabled,
                    (*p).startup.len()
                )
            } else {
                String::new()
            },
            Some((
                tr(
                    "¹ 게시자는 파일 메타데이터이며 확인된 서명자가 아닙니다",
                    "¹ Publisher is file metadata, not a verified signer",
                ),
                tr("¹ 게시자: 파일 메타데이터", "¹ Publisher: file metadata"),
            )),
        ),
        Page::Services => (
            if (*p).services_loaded {
                let running = (*p)
                    .services
                    .iter()
                    .filter(|s| s.state == SERVICE_RUNNING)
                    .count();
                let stopped = (*p)
                    .services
                    .iter()
                    .filter(|s| s.state == SERVICE_STOPPED)
                    .count();
                tf!(
                    "실행 중 {} · 중지됨 {}",
                    "{} running · {} stopped",
                    running,
                    stopped
                )
            } else {
                String::new()
            },
            None,
        ),
        Page::Settings => (String::new(), None),
    }
}
/// Why the Processes page's Network column reads "—": the per-process
/// network trace's reason (`netetw`), as a full and a short note.
pub(super) fn network_note(reason: &str) -> MetaNote {
    if reason == "Requires administrator" {
        (
            tr(
                "프로세스별 네트워크는 관리자 권한이 필요합니다",
                "Network per process requires administrator",
            ),
            tr("네트워크: 관리자 권한 필요", "Network: needs admin"),
        )
    } else if reason.starts_with("Network trace stopped") {
        (
            tr(
                "프로세스별 네트워크 추적이 중지되었습니다",
                "Network per process: the trace stopped",
            ),
            tr("네트워크 추적 중지됨", "Network trace stopped"),
        )
    } else if reason == "Network events dropped" {
        (
            tr(
                "네트워크 이벤트가 누락되어 이 구간은 표시하지 않음",
                "Network events dropped; this interval is not shown",
            ),
            tr("네트워크 이벤트 누락", "Network events dropped"),
        )
    } else if reason.starts_with("Cannot check network event loss") {
        (
            tr(
                "네트워크 이벤트 손실을 확인할 수 없어 표시하지 않음",
                "Network not shown: event loss cannot be checked",
            ),
            tr("네트워크 확인 불가", "Network: cannot check"),
        )
    } else {
        (
            tr(
                "프로세스별 네트워크를 측정할 수 없습니다",
                "Network per process is unavailable",
            ),
            tr("네트워크 측정 불가", "Network unavailable"),
        )
    }
}
/// Where the page meta starts after the h1: Chromium ends the h1 at its
/// fractional advances with `letter-spacing: -0.01em` (GDI's whole-pixel
/// advances are ~3 px wider and pushed the meta right on every page).
unsafe fn title_advance(p: *mut App, dc: HDC) -> i32 {
    let f = &(*p).fonts;
    let title = (*p).page.title();
    let old = SelectObject(dc, f.h1);
    let ideal = fonts::ideal_width(dc, title);
    SelectObject(dc, old);
    let tracking = -0.01 * gfx::px((*p).dpi, 20.0) * title.chars().count() as f32;
    ((ideal + tracking).round() as i32).clamp(0, text_width(dc, f.h1, title).max(1))
}
/// The width the page head's text needs with the short honesty note: h1,
/// gap, meta, " · " and the short note (the head's actions yield
/// decoration before the note is cut, see `ui::head_layout`).
pub(super) unsafe fn head_text_width(p: *mut App, dc: HDC) -> i32 {
    let f = &(*p).fonts;
    let (meta, note) = page_meta(p);
    let mut width = title_advance(p, dc) + gfx::pxi((*p).dpi, layout::HEAD_GAP);
    width += text_width(dc, f.mono_small, &meta);
    if let Some((_, short)) = note {
        if !meta.is_empty() {
            width += text_width(dc, f.mono_small, " · ");
        }
        width += text_width(dc, f.small, short);
    }
    width
}
/// Page head: h1, meta + note, the actions' painted notes, bottom border.
unsafe fn page_head(p: *mut App, dc: HDC, l: &layout::Layout) {
    let c = colors();
    let f = &(*p).fonts;
    fill(
        dc,
        RECT {
            top: l.head.bottom - l.hair,
            ..l.head
        },
        c.border,
    );
    let inner = l.head_inner;
    let actions = head_layout(p, l, dc);
    let gap = l.px(layout::HEAD_GAP);
    let limit = actions
        .first()
        .map_or(inner.right, |(_, r)| r.left - gap)
        .max(inner.left);
    let pt = Painter::new(dc, (*p).dpi, f);
    let title = (*p).page.title();
    let title_width = text_width(dc, f.h1, title);
    let title_advance = title_advance(p, dc);
    // Text sits on the reference's CSS baselines (flex-centred line boxes:
    // h1 20/1.2, meta 12/1.45), drawn in full GDI cells so descenders of
    // "Settings" are never clipped by the 24 px head row.
    let h1 = pt.css_rect(f.h1, inner, Some(layout::H1_LINE));
    pt.text(
        f.h1,
        c.fg,
        title,
        RECT {
            left: inner.left,
            right: (inner.left + title_width).min(limit),
            ..h1
        },
        DT_SINGLELINE | DT_END_ELLIPSIS | DT_LEFT,
    );
    let mut x = inner.left + title_advance + gap;
    let (meta, note) = page_meta(p);
    let meta_baseline = pt.css_baseline(f.mono_small, inner, Some(layout::META_LINE));
    if !meta.is_empty() && x < limit {
        pt.text_on_baseline(
            f.mono_small,
            c.muted,
            &meta,
            RECT {
                left: x,
                right: limit,
                ..inner
            },
            meta_baseline,
            DT_LEFT,
        );
        x += text_width(dc, f.mono_small, &meta);
    }
    if let Some((long, short)) = note.filter(|_| x < limit) {
        // " · " belongs to the mono `.meta` run (two 7 px mono spaces around
        // the dot); only the note itself is 12 px text.
        let separator = if meta.is_empty() { "" } else { " · " };
        let separator_width = text_width(dc, f.mono_small, separator);
        let mut note = long;
        if x + separator_width + text_width(dc, f.small, note) > limit {
            note = short;
        }
        // The note continues the meta on its baseline.
        let baseline = if meta.is_empty() {
            pt.css_baseline(f.small, inner, Some(layout::META_LINE))
        } else {
            meta_baseline
        };
        if !separator.is_empty() {
            pt.text_on_baseline(
                f.mono_small,
                c.muted,
                separator,
                RECT {
                    left: x,
                    right: limit,
                    ..inner
                },
                baseline,
                DT_LEFT,
            );
            x += separator_width;
        }
        if x < limit {
            pt.text_on_baseline(
                f.small,
                c.muted,
                note,
                RECT {
                    left: x,
                    right: limit,
                    ..inner
                },
                baseline,
                DT_LEFT,
            );
        }
    }
    for (item, r) in actions {
        if let HeadItem::Note(text) = item {
            let band = RECT {
                top: inner.top,
                bottom: inner.bottom,
                ..r
            };
            let baseline = pt.css_baseline(f.small, band, Some(layout::META_LINE));
            pt.text_on_baseline(f.small, c.muted, text, r, baseline, DT_LEFT);
        }
    }
}
/// The sliding nav "current" indicator (DESIGN_SPEC §6: slides and
/// stretches from the old item to the new one over 180 ms). Its edges are
/// the main window's animated `(anim::NAV_ID, part::NAV_TOP / NAV_BOTTOM)`
/// in client px (`ui::place_nav_indicator`); the rail paints the part in the
/// gaps and every nav item's owner draw paints its own part, so the bar
/// moves seamlessly over the child windows. `origin` = the client position
/// of `pt`'s DC origin.
pub(super) unsafe fn nav_indicator(p: *mut App, pt: &Painter, origin: POINT) {
    let top = (*p).anim.value((anim::NAV_ID, anim::part::NAV_TOP));
    let bottom = (*p).anim.value((anim::NAV_ID, anim::part::NAV_BOTTOM));
    if bottom - top < 1.0 {
        return;
    }
    let l = current_layout(p);
    widgets::nav_indicator(
        pt,
        (l.nav[0].left - origin.x) as f32,
        top - origin.y as f32,
        bottom - top,
        1.0,
    );
}
/// The window-relative rectangle of a child control.
unsafe fn child_rect(p: *mut App, h: HWND) -> RECT {
    let mut r: RECT = zeroed();
    GetWindowRect(h, &mut r);
    MapWindowPoints(
        null_mut(),
        (*p).hwnd,
        (&mut r as *mut RECT).cast::<POINT>(),
        2,
    );
    r
}
/// Status bar (DESIGN_SPEC §3): `● Live · n processes · CPU · Memory`, grow,
/// Feather's own usage in fg, "Refresh" + the 22 px rate select; 12 px mono,
/// gap 18, padding 0 12 0 16, 1 px top border. Errors replace the system
/// text in the danger color until acted on; notices use the same slot.
unsafe fn status_bar(p: *mut App, dc: HDC, l: &layout::Layout) {
    let c = colors();
    fill(dc, l.status, c.bg);
    fill(
        dc,
        RECT {
            bottom: l.status.top + l.hair,
            ..l.status
        },
        c.border,
    );
    let inner = l.status_inner;
    let stale = (*p).last_sample.is_some_and(|t| {
        t.elapsed() > Duration::from_millis((*p).interval.saturating_mul(3).max(5000))
    });
    let state = if (*p).snapshot.is_none() {
        tr("불러오는 중", "Loading")
    } else if (*p).paused {
        tr("일시정지", "Paused")
    } else if (*p).busy {
        tr("처리 중", "Working")
    } else if stale {
        tr("대기", "Waiting")
    } else {
        tr("실시간", "Live")
    };
    let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
    widgets::status_dot(
        &pt,
        inner.left as f32 + pt.px(layout::STATUS_DOT / 2.0),
        (inner.top + inner.bottom) as f32 / 2.0,
        if (*p).paused {
            c.warn_fg
        } else if stale {
            c.muted
        } else {
            c.fg
        },
    );
    let mono = (*p).fonts.mono_small;
    let gap = l.px(layout::STATUS_GAP);
    let band = |left: i32, right: i32| RECT {
        left,
        right,
        ..inner
    };
    // Right side first: the rate select is a child control; its label and
    // Feather's own usage flow to its left.
    let rate = child_rect(p, (*p).rate);
    let label = tr("새로 고침", "Refresh");
    let label_right = rate.left - text_width(dc, mono, " ");
    let label_left = label_right - text_width(dc, mono, label);
    // `.statusbar { font: 12px mono }` (line-height normal) on its CSS
    // baseline: DT_VCENTER put the Cascadia Mono strings 1 px low at 150 %.
    let base = pt.css_baseline(mono, inner, None);
    let label_at = |text: &str, color: u32, r: RECT| {
        pt.text_on_baseline(mono, color, text, r, base, DT_LEFT);
    };
    label_at(label, c.muted, band(label_left, label_right));
    let snapshot = (*p).snapshot.as_ref();
    let mut right = label_left - gap;
    if let Some(own) =
        snapshot.and_then(|v| v.processes.iter().find(|v| v.pid == std::process::id()))
    {
        let own = format!(
            "Feather {:.1}% CPU · {}",
            own.cpu_percent,
            human_bytes(own.working_set as f64)
        );
        let width = text_width(dc, mono, &own);
        label_at(&own, c.fg, band(right - width, right));
        right -= width + gap;
    }
    let mut x = inner.left + l.px(2.0 * layout::STATUS_DOT);
    for value in [
        state.to_owned(),
        snapshot.map_or_else(
            || "—".into(),
            |v| tf!("{}개 프로세스", "{} processes", v.processes.len()),
        ),
    ] {
        let width = text_width(dc, mono, &value);
        label_at(&value, c.muted, band(x, (x + width).min(right)));
        x += width + gap;
    }
    let message = if let Some(e) = &(*p).error {
        e.clone()
    } else if !(&(*p).notice).is_empty() {
        (*p).notice.clone()
    } else {
        snapshot.map_or_else(String::new, |v| {
            format!(
                "CPU {:.1}% · {} {:.1}/{:.1} GB",
                v.cpu_percent,
                tr("메모리", "Memory"),
                v.memory_used as f64 / 1073741824.0,
                v.memory_total as f64 / 1073741824.0
            )
        })
    };
    if x < right {
        label_at(
            &message,
            if (*p).error.is_some() {
                c.danger
            } else {
                c.muted
            },
            band(x, right),
        );
    }
}
/// Settings page: groups of joined rows (radius 4 on the outer corners,
/// 1 px separators), titles 13/600, descriptions 12 muted; the controls are
/// child windows placed by `interactions::layout_settings`.
unsafe fn settings_page(p: *mut App, dc: HDC, l: &layout::Layout) {
    let c = colors();
    let f = &(*p).fonts;
    let geometry = l.settings();
    let titles = [
        tr("모양", "Appearance"),
        tr("업데이트", "Updates"),
        tr("창", "Window"),
    ];
    let rows: [&[(&str, &str)]; 3] = [
        &[
            (
                tr("테마", "Theme"),
                tr(
                    "Windows 설정을 따르거나 밝게 또는 어둡게 고정합니다",
                    "Follow Windows or pin light or dark",
                ),
            ),
            (
                tr("언어", "Language"),
                tr("화면에 사용할 언어", "Choose your display language"),
            ),
        ],
        &[
            (
                tr("새로 고침 간격", "Refresh rate"),
                tr(
                    "카운터를 갱신하는 주기입니다. 느린 간격은 CPU 사용량을 줄입니다.",
                    "How often the counters update. Slower rates use less CPU.",
                ),
            ),
            (
                tr("기본 시작 화면", "Default start page"),
                tr("Feather가 열릴 때 표시할 화면", "The page Feather opens on"),
            ),
        ],
        &[
            (
                tr("항상 위에 표시", "Always on top"),
                tr(
                    "다른 창보다 위에 표시합니다",
                    "Keep Feather above other windows",
                ),
            ),
            (
                tr("트레이로 최소화", "Minimize to tray"),
                tr(
                    "알림 영역에서 계속 모니터링합니다",
                    "Keep monitoring from the notification area",
                ),
            ),
            // The reference's wording; the replacement is the whole Task
            // Manager (Ctrl+Alt+Delete too), not only the shortcut.
            (
                tr("Ctrl + Shift + Esc로 열기", "Open with Ctrl + Shift + Esc"),
                tr(
                    "Windows 기본 작업 관리자를 대체합니다",
                    "Replace the built-in Task Manager",
                ),
            ),
            (
                tr("항상 관리자 권한으로 실행", "Always run as administrator"),
                tr(
                    "설치된 Feather가 매번 승인(UAC)을 요청합니다. 프로세스별 네트워크와 전체 좀비 검사에 필요합니다.",
                    "The installed Feather asks for approval (UAC) at each start. For per-process network and full zombie scans.",
                ),
            ),
        ],
    ];
    let controls: Vec<RECT> = (*p)
        .preference_controls
        .iter()
        .map(|&h| child_rect(p, h))
        .collect();
    let pt = Painter::new(dc, (*p).dpi, f);
    let hair = l.hair;
    let text_line = l.px(layout::SETTINGS_ROW_TITLE + layout::SETTINGS_ROW_TEXT);
    for (g, group) in geometry.groups.iter().enumerate() {
        // `h2` 15 px / 1.3 from its unrounded line-box top.
        let h2 = pt.css_baseline_at(
            f.group_title,
            group.title_y,
            pt.px(layout::SETTINGS_TITLE),
            Some(layout::SETTINGS_TITLE),
        );
        pt.text_on_baseline(f.group_title, c.fg, titles[g], group.title, h2, DT_LEFT);
        pt.canvas.bordered_round_rect(
            RectF::from_rect(group.frame),
            gfx::Radii::all(gfx::px((*p).dpi, theme::RADIUS_SM)),
            hair as f32,
            solid(c.bg),
            solid(c.border),
        );
        for pair in group.rows.windows(2) {
            pt.fill(
                RECT {
                    left: group.frame.left + hair,
                    top: pair[0].bottom,
                    right: group.frame.right - hair,
                    bottom: pair[1].top,
                },
                c.border,
            );
        }
        for (i, row) in group.rows.iter().enumerate() {
            let Some(&(title, subtitle)) = rows[g].get(i) else {
                continue;
            };
            let content = group.row_content(i, (*p).dpi);
            // Text stops 24 px before the row's control (`.set-row` gap).
            let right = controls
                .iter()
                .filter(|r| r.top >= row.top && r.bottom <= row.bottom + hair)
                .map(|r| r.left - l.px(24.0))
                .min()
                .unwrap_or(content.right)
                .max(content.left);
            // `b` and `span` line boxes (13 and 12 px × 1.45) stacked from
            // the row's unrounded content top, as Chromium places them.
            let (top, _) = group.content_y(i, (*p).dpi);
            let title_px = pt.px(layout::SETTINGS_ROW_TITLE);
            let text_px = pt.px(layout::SETTINGS_ROW_TEXT);
            let title_base =
                pt.css_baseline_at(f.ui_strong, top, title_px, Some(layout::SETTINGS_ROW_TITLE));
            let text_base = pt.css_baseline_at(
                f.small,
                top + title_px,
                text_px,
                Some(layout::SETTINGS_ROW_TEXT),
            );
            let band = RECT {
                left: content.left,
                top: content.top,
                right,
                bottom: content.top + text_line,
            };
            pt.text_on_baseline(f.ui_strong, c.fg, title, band, title_base, DT_LEFT);
            pt.text_on_baseline(f.small, c.muted, subtitle, band, text_base, DT_LEFT);
        }
    }
    if geometry.footer.bottom <= l.content.bottom {
        pt.label(
            f.small,
            c.muted,
            &format!("Feather Task Manager  {}", env!("CARGO_PKG_VERSION")),
            geometry.footer,
            DT_LEFT,
        );
    }
}
/// A device trace as chart points (`kind` = value index; 3 = rx + tx).
pub(super) unsafe fn trace_values(
    trace: Option<&telemetry::Trace>,
    kind: usize,
) -> Vec<(Instant, f64)> {
    trace.map_or_else(Vec::new, |t| {
        t.points
            .iter()
            .map(|v| {
                (
                    v.at,
                    if kind == 3 {
                        v.values[0] + v.values[1]
                    } else {
                        v.values[kind]
                    },
                )
            })
            .collect()
    })
}
/// The reference's chart looks (shared by the performance page, the
/// telemetry drawer and the device cards; public for the Table track).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChartStyle {
    /// Performance chart: bg box, radius 4, 9 × 4 grid, accent @ 22 % area, 1.5 px line.
    Main,
    /// Logical-processor small multiple: like Main without the grid.
    Core,
    /// Device card sparkline: surface box, radius 3, fg @ 14 % area, 1 px fg line.
    Spark,
}
/// A 60-second chart in `r` (device px): frame, grid (Main), area + line.
/// `ceiling` = the value at the top (None = 110 % of the maximum); gaps
/// (non-finite values) break the line. No text except Main's "Collecting
/// data" placeholder.
pub(super) unsafe fn chart(
    p: *mut App,
    dc: HDC,
    r: RECT,
    points: &[(Instant, f64)],
    ceiling: Option<f64>,
    color: u32,
    style: ChartStyle,
) {
    chart_with((*p).dpi, &(*p).fonts, dc, r, points, ceiling, color, style);
}

/// Same chart renderer for independent native windows at their own DPI.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn chart_with(
    dpi: i32,
    fonts: &fonts::Fonts,
    dc: HDC,
    r: RECT,
    points: &[(Instant, f64)],
    ceiling: Option<f64>,
    color: u32,
    style: ChartStyle,
) {
    if r.right <= r.left || r.bottom <= r.top {
        return;
    }
    let c = colors();
    let hair = gfx::hairline(dpi);
    let (background, radius, area, width) = match style {
        ChartStyle::Main => (c.bg, 4.0, argb(color, 0.22), 1.5),
        ChartStyle::Core => (c.bg, 4.0, argb(color, 0.22), 1.0),
        ChartStyle::Spark => (c.surface, 3.0, c.spark_fill(), 1.0),
    };
    let radius = gfx::px(dpi, radius);
    let frame = RectF::from_rect(r);
    let canvas = Canvas::new(dc);
    canvas.bordered_round_rect(
        frame,
        gfx::Radii::all(radius),
        hair,
        solid(background),
        solid(c.border),
    );
    let inner = frame.inset(hair);
    if style == ChartStyle::Main {
        for i in 1..10 {
            let x = (inner.x + inner.w * i as f32 / 10.0).round();
            canvas.fill_rect(RectF::new(x, inner.y, hair, inner.h), solid(c.border));
        }
        for i in 1..5 {
            let y = (inner.y + inner.h * i as f32 / 5.0).round();
            canvas.fill_rect(RectF::new(inner.x, y, inner.w, hair), solid(c.border));
        }
    }
    let maximum = ceiling
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or_else(|| {
            points
                .iter()
                .map(|v| v.1)
                .filter(|v| v.is_finite())
                .fold(1.0, f64::max)
                * 1.1
        });
    canvas.clip_round_rect(inner, gfx::Radii::all((radius - hair).max(0.0)));
    let end = points.last().map_or_else(Instant::now, |v| v.0);
    let mut segment: Vec<(f32, f32)> = Vec::with_capacity(points.len());
    let stroke = gfx::px(dpi, width).max(1.0);
    let line_color = solid(color);
    let at = |age: f64, value: f64| {
        (
            inner.right() - (inner.w as f64 * age / 60.0) as f32,
            inner.bottom() - (inner.h as f64 * (value / maximum).clamp(0.0, 1.0)) as f32,
        )
    };
    // The newest point older than the window: the trace starts exactly at
    // the chart's left edge (interpolated at 60 s), like the reference's
    // area from border to border, instead of a strip that changes width
    // with every sample.
    let mut before: Option<(f64, f64)> = None;
    for &(time, value) in points {
        let age = end.saturating_duration_since(time).as_secs_f64();
        if age > 60.0 {
            before = value.is_finite().then_some((age, value));
            continue;
        }
        if !value.is_finite() {
            chart_segment(&canvas, inner, &segment, area, line_color, stroke);
            segment.clear();
            before = None;
            continue;
        }
        if let Some((old_age, old_value)) = before.take() {
            if segment.is_empty() && old_age > age && old_age - age <= 10.0 {
                let t = (old_age - 60.0) / (old_age - age);
                segment.push(at(60.0, old_value + (value - old_value) * t));
            }
        }
        segment.push(at(age, value));
    }
    chart_segment(&canvas, inner, &segment, area, line_color, stroke);
    canvas.reset_clip();
    drop(canvas);
    if style == ChartStyle::Main && points.iter().filter(|v| v.1.is_finite()).count() < 2 {
        label(
            dc,
            fonts.small,
            c.muted,
            tr("데이터 수집 중", "Collecting data"),
            r,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}
fn chart_segment(
    canvas: &Canvas,
    inner: RectF,
    points: &[(f32, f32)],
    area: theme::Argb,
    line_color: theme::Argb,
    width: f32,
) {
    if points.len() < 2 {
        return;
    }
    let mut polygon = Vec::with_capacity(points.len() + 2);
    polygon.push((points[0].0, inner.bottom()));
    polygon.extend_from_slice(points);
    polygon.push((points[points.len() - 1].0, inner.bottom()));
    canvas.fill_polygon(&polygon, area);
    canvas.polyline(points, width, line_color);
}
struct PerformanceDisplay {
    title: String,
    model: String,
    value: String,
    /// The device card's second line (the reference's `DEV[k].sub()`).
    sub: String,
    caption: &'static str,
    ceiling: Option<f64>,
    index: usize,
    stats: Vec<(&'static str, String)>,
    /// Owned labels: some name their source (an ACPI thermal zone).
    specs: Vec<(String, String)>,
}
unsafe fn performance_display(p: *mut App, target: &PerfTarget) -> PerformanceDisplay {
    let snap = (*p).snapshot.as_ref();
    let perf = (*p).performance.as_ref();
    let unknown = || "—".to_string();
    let mut d = PerformanceDisplay {
        title: String::new(),
        model: String::new(),
        value: unknown(),
        sub: String::new(),
        // Chart header only (the stat keeps "Utilization"), like the reference.
        caption: tr("% 사용률", "% Utilization"),
        ceiling: Some(100.0),
        index: 0,
        stats: Vec::new(),
        specs: Vec::new(),
    };
    let spec = |label: &str, value: String| (label.to_owned(), value);
    match target {
        PerfTarget::Cpu => {
            d.title = "CPU".into();
            d.model = perf.map_or_else(unknown, |v| v.cpu_name.clone());
            d.value = percent(snap.map(|v| v.cpu_percent));
            // "33.2%  3.15 GHz" (the reported speed when there is one).
            d.sub = match perf.and_then(|v| v.cpu_frequency_mhz) {
                Some(mhz) => format!("{}  {:.2} GHz", d.value, mhz / 1000.0),
                None => d.value.clone(),
            };
            d.stats = vec![
                (tr("사용률", "Utilization"), d.value.clone()),
                (
                    tr("보고된 속도", "Reported speed"),
                    perf.and_then(|v| v.cpu_frequency_mhz)
                        .map_or_else(unknown, |v| format!("{:.2} GHz", v / 1000.0)),
                ),
                (
                    tr("프로세스", "Processes"),
                    perf.and_then(|v| v.memory.as_ref())
                        .map(|v| grouped(v.processes))
                        .or_else(|| snap.map(|v| grouped(v.processes.len())))
                        .unwrap_or_else(unknown),
                ),
                (
                    tr("스레드", "Threads"),
                    perf.and_then(|v| v.memory.as_ref())
                        .map_or_else(unknown, |v| grouped(v.threads)),
                ),
            ];
            let caches = perf.and_then(|v| v.cpu_caches);
            let cache = |value: Option<u64>| value.map_or_else(unknown, cache_size);
            d.specs = vec![
                spec(
                    tr("기본 속도", "Base speed"),
                    perf.and_then(|v| v.cpu_base_mhz)
                        .map_or_else(unknown, |v| format!("{:.2} GHz", v as f64 / 1000.0)),
                ),
                spec(
                    tr("소켓", "Sockets"),
                    perf.and_then(|v| v.sockets)
                        .map_or_else(unknown, |v| v.to_string()),
                ),
                spec(
                    tr("코어 / 논리 프로세서", "Cores / logical processors"),
                    perf.map_or_else(unknown, |v| {
                        format!(
                            "{} / {}",
                            v.physical_cores.map_or_else(unknown, |v| v.to_string()),
                            v.logical_cpus
                        )
                    }),
                ),
                spec(
                    tr("가상화", "Virtualization"),
                    perf.map_or_else(unknown, |v| {
                        if v.virtualization_firmware_enabled {
                            tr("사용", "Enabled")
                        } else {
                            tr("사용 안 함", "Disabled")
                        }
                        .into()
                    }),
                ),
                spec(
                    tr("L1 캐시", "L1 cache"),
                    cache(caches.and_then(|c| c.l1_bytes)),
                ),
                spec(
                    tr("L2 캐시", "L2 cache"),
                    cache(caches.and_then(|c| c.l2_bytes)),
                ),
                spec(
                    tr("L3 캐시", "L3 cache"),
                    cache(caches.and_then(|c| c.l3_bytes)),
                ),
                spec(
                    tr("가동 시간", "Uptime"),
                    perf.map_or_else(unknown, |v| uptime(v.uptime_seconds)),
                ),
                spec(
                    tr("핸들", "Handles"),
                    perf.and_then(|v| v.memory.as_ref())
                        .map_or_else(unknown, |v| grouped(v.handles)),
                ),
            ];
            // Firmware-defined ACPI sensor locations, never "CPU temperature".
            // Keep temperatures near the top: short windows trim the final
            // specification rows to preserve the established chart layout.
            let zones = perf.map_or(&[][..], |v| &v.thermal_zones[..]);
            if zones.is_empty() {
                d.specs.insert(
                    1,
                    spec(
                        tr("펌웨어 온도", "Firmware temperature"),
                        tr("사용 불가", "Unavailable").into(),
                    ),
                );
            }
            for (index, zone) in zones.iter().enumerate() {
                d.specs.insert(
                    index + 1,
                    (
                        tf!("ACPI 열 영역 ({})", "ACPI thermal zone ({})", zone.name),
                        celsius(zone.celsius),
                    ),
                );
            }
        }
        PerfTarget::Memory => {
            d.title = tr("메모리", "Memory").into();
            let m = perf.and_then(|v| v.memory.as_ref());
            let modules = perf.and_then(|v| v.memory_modules.as_deref());
            let capacity = m
                .map(|v| v.physical_total)
                .or_else(|| snap.map(|v| v.memory_total));
            // Installed modules and their type ("64.0 GB DDR5", Task
            // Manager's heading) when SMBIOS reports every module's size,
            // else Windows' usable total.
            let installed = modules.and_then(|inv| {
                let sizes: Option<Vec<u64>> = inv.modules.iter().map(|m| m.size_bytes).collect();
                sizes
                    .filter(|s| !s.is_empty())
                    .map(|s| s.iter().sum::<u64>())
            });
            d.model = match (installed, modules.and_then(|inv| inv.memory_type())) {
                (Some(total), Some(kind)) => format!("{} {kind}", human_bytes(total as f64)),
                (Some(total), None) => human_bytes(total as f64),
                _ => capacity.map_or_else(unknown, |total| {
                    tf!(
                        "{} 물리 메모리",
                        "{} physical memory",
                        human_bytes(total as f64)
                    )
                }),
            };
            d.value = snap.map_or_else(unknown, |v| human_bytes(v.memory_used as f64));
            // "12.2/32.0 GB (38.2%)".
            d.sub = snap
                .filter(|v| v.memory_total > 0)
                .map_or_else(unknown, |v| {
                    const GB: f64 = 1073741824.0;
                    format!(
                        "{:.1}/{:.1} GB ({:.1}%)",
                        v.memory_used as f64 / GB,
                        v.memory_total as f64 / GB,
                        v.memory_used as f64 / v.memory_total as f64 * 100.0
                    )
                });
            d.ceiling = capacity.map(|v| v as f64);
            d.caption = tr("물리 메모리 사용량", "Physical memory usage");
            d.stats = vec![
                (tr("사용 중", "In use"), d.value.clone()),
                (
                    tr("사용 가능", "Available"),
                    bytes(m.map(|v| v.physical_available)),
                ),
                (tr("커밋됨", "Committed"), bytes(m.map(|v| v.commit_used))),
                (tr("캐시됨", "Cached"), bytes(m.map(|v| v.cache))),
            ];
            d.specs = Vec::new();
            if let Some(inv) = modules {
                // MT/s only when the SMBIOS table defines the unit (3.2+).
                d.specs.push(spec(
                    tr("속도", "Speed"),
                    inv.configured_speed_mts()
                        .map_or_else(unknown, |v| format!("{v} MT/s")),
                ));
                d.specs.push(spec(
                    tr("사용 중인 슬롯", "Slots used"),
                    match inv.slots_total {
                        Some(total) => tf!("{1}개 중 {0}개", "{0} of {1}", inv.slots_used, total),
                        None => inv.slots_used.to_string(),
                    },
                ));
                d.specs.push(spec(
                    tr("폼 팩터", "Form factor"),
                    inv.form_factor().map_or_else(unknown, str::to_owned),
                ));
            }
            d.specs.extend([
                spec(
                    tr("커밋 한도", "Commit limit"),
                    bytes(m.map(|v| v.commit_limit)),
                ),
                spec(
                    tr("최대 커밋", "Peak commit"),
                    bytes(m.map(|v| v.commit_peak)),
                ),
                spec(
                    tr("페이지 풀", "Paged pool"),
                    bytes(m.map(|v| v.kernel_paged)),
                ),
                spec(
                    tr("비페이지 풀", "Non-paged pool"),
                    bytes(m.map(|v| v.kernel_nonpaged)),
                ),
            ]);
        }
        PerfTarget::Disk(id) => {
            d.title = disk_name(id);
            let device = perf.and_then(|v| v.storage_device(id));
            d.model = device
                .and_then(|v| v.model.clone())
                .unwrap_or_else(|| tr("물리 디스크", "Physical disk").into());
            d.caption = tr("% 활성 시간", "% Active time");
            let disk = perf.and_then(|v| v.disks.iter().find(|v| v.id == *id));
            d.value = percent(disk.and_then(|v| v.active_percent));
            let kind = device.and_then(disk_type);
            // "SSD (NVMe) 0.3%".
            d.sub = match &kind {
                Some(kind) => format!("{kind} {}", d.value),
                None => d.value.clone(),
            };
            d.stats = vec![
                (tr("활성 시간", "Active time"), d.value.clone()),
                (
                    tr("읽기 속도", "Read speed"),
                    rate(disk.and_then(|v| v.read_bytes_per_sec)),
                ),
                (
                    tr("쓰기 속도", "Write speed"),
                    rate(disk.and_then(|v| v.write_bytes_per_sec)),
                ),
            ];
            let (_, letters) = disk_parts(id);
            d.specs = vec![
                spec(
                    tr("용량", "Capacity"),
                    bytes(device.and_then(|v| v.capacity_bytes)),
                ),
                spec(tr("종류", "Type"), kind.unwrap_or_else(unknown)),
                spec(
                    tr("시스템 디스크", "System disk"),
                    yes_no(device.and_then(|v| v.system_disk)),
                ),
                spec(
                    tr("페이지 파일", "Page file"),
                    yes_no(device.and_then(|v| v.page_file)),
                ),
                spec(
                    tr("드라이브", "Drives"),
                    if letters.is_empty() {
                        unknown()
                    } else {
                        letters
                    },
                ),
                spec(
                    tr("측정 범위", "Measurement"),
                    tr("선택한 물리 디스크", "Selected physical disk").into(),
                ),
            ];
        }
        PerfTarget::Network(id) => {
            let nic = perf.and_then(|v| v.networks.iter().find(|v| v.id == *id));
            d.title = nic
                .filter(|v| !v.name.is_empty())
                .map_or_else(|| tr("네트워크", "Network").into(), |v| v.name.clone());
            d.model = nic.map_or_else(unknown, |v| v.description.clone());
            d.value = rate(
                nic.and_then(|v| v.rx_bytes_per_sec.zip(v.tx_bytes_per_sec))
                    .map(|(a, b)| a + b),
            );
            d.caption = tr("처리량 · 수신 + 송신", "Throughput · receive + send");
            d.ceiling = None;
            d.index = 3;
            d.stats = vec![
                (
                    tr("수신", "Receive"),
                    rate(nic.and_then(|v| v.rx_bytes_per_sec)),
                ),
                (
                    tr("송신", "Send"),
                    rate(nic.and_then(|v| v.tx_bytes_per_sec)),
                ),
            ];
            d.specs = vec![
                spec(
                    tr("어댑터", "Adapter"),
                    nic.filter(|v| !v.description.is_empty())
                        .map_or_else(unknown, |v| v.description.clone()),
                ),
                spec(
                    tr("연결 형식", "Connection type"),
                    nic.map_or_else(unknown, |v| network_kind(v.kind).into()),
                ),
                spec(
                    tr("연결 속도", "Link speed"),
                    nic.map_or_else(unknown, |v| {
                        let (rx, tx) = (v.receive_link_bits_per_sec, v.transmit_link_bits_per_sec);
                        if rx == tx {
                            link_speed(rx)
                        } else {
                            tf!(
                                "수신 {} · 송신 {}",
                                "Receive {} · send {}",
                                link_speed(rx),
                                link_speed(tx)
                            )
                        }
                    }),
                ),
                spec(
                    tr("상태", "State"),
                    nic.map_or_else(unknown, |v| {
                        if v.connected {
                            tr("연결됨", "Connected")
                        } else {
                            tr("연결 끊김", "Disconnected")
                        }
                        .into()
                    }),
                ),
            ];
        }
        PerfTarget::Gpu(id) => {
            let gpu = perf.and_then(|v| v.gpus.iter().find(|v| v.id == *id));
            d.title = perf
                .and_then(|v| {
                    v.gpus
                        .iter()
                        .filter(|g| listed_gpu(g))
                        .position(|v| v.id == *id)
                })
                .map_or_else(|| "GPU".into(), |index| format!("GPU {index}"));
            let adapter = gpu.and_then(|v| v.adapter.as_deref());
            d.model = gpu
                .and_then(|v| v.name.clone())
                .unwrap_or_else(|| id.clone());
            d.value = percent(gpu.and_then(|v| v.percent));
            let sensors = gpu.and_then(|v| v.sensors.as_ref());
            let temperature = sensors
                .and_then(|s| s.temperature_c)
                .filter(|t| t.is_finite());
            // "12.0% · 39 °C" (the temperature only when measured).
            d.sub = match temperature {
                Some(t) => format!("{} · {}", d.value, celsius(t)),
                None => d.value.clone(),
            };
            d.caption = tr("가장 바쁜 엔진", "Busiest engine");
            d.stats = vec![
                (tr("사용률", "Utilization"), d.value.clone()),
                (
                    tr("전용 메모리 사용량", "Dedicated memory used"),
                    bytes(gpu.and_then(|v| v.dedicated_bytes)),
                ),
                (
                    tr("공유 메모리 사용량", "Shared memory used"),
                    bytes(gpu.and_then(|v| v.shared_bytes)),
                ),
            ];
            if let Some(t) = temperature {
                d.stats
                    .push((tr("GPU 온도", "GPU temperature"), celsius(t)));
            }
            d.specs = vec![
                spec(
                    tr("전용 GPU 메모리", "Dedicated GPU memory"),
                    bytes(adapter.and_then(|a| a.dedicated_video_memory)),
                ),
                spec(
                    tr("공유 GPU 메모리", "Shared GPU memory"),
                    bytes(adapter.and_then(|a| a.shared_system_memory)),
                ),
                spec(
                    tr("드라이버 버전", "Driver version"),
                    adapter
                        .and_then(|a| a.driver_version.clone())
                        .unwrap_or_else(unknown),
                ),
                spec(
                    tr("드라이버 날짜", "Driver date"),
                    adapter
                        .and_then(|a| a.driver_date.clone())
                        .unwrap_or_else(unknown),
                ),
                spec(
                    tr("WDDM 버전", "WDDM version"),
                    adapter
                        .and_then(|a| a.wddm_version)
                        .map_or_else(unknown, |(major, minor)| format!("{major}.{minor}")),
                ),
                spec(
                    tr("PCI 위치", "PCI location"),
                    adapter.and_then(|a| a.pci_location).map_or_else(
                        unknown,
                        |(bus, device, function)| {
                            tf!(
                                "PCI 버스 {}, 장치 {}, 기능 {}",
                                "PCI bus {}, device {}, function {}",
                                bus,
                                device,
                                function
                            )
                        },
                    ),
                ),
            ];
            if let Some(rpm) = sensors.and_then(|s| s.fan_rpm) {
                d.specs
                    .push(spec(tr("팬 속도", "Fan speed"), format!("{rpm} RPM")));
            }
            if let Some(power) = sensors
                .and_then(|s| s.power_percent)
                .filter(|v| v.is_finite())
            {
                d.specs.push(spec(
                    tr("전력", "Power"),
                    tf!("전력 한도의 {:.0}%", "{:.0}% of power limit", power),
                ));
            }
            if let Some(gpu) = gpu {
                let mut engines: Vec<_> = gpu.engines.iter().collect();
                engines.sort_by(|a, b| b.percent.total_cmp(&a.percent));
                for engine in engines.iter().take(3) {
                    // Two engines of one type (e.g. two VideoEncode) would
                    // read identically; add the Windows engine index then.
                    let repeated = engines
                        .iter()
                        .filter(|other| other.name == engine.name)
                        .count()
                        > 1;
                    let name = if engine.name.is_empty() {
                        engine.id.clone()
                    } else if repeated {
                        format!("{} {}", engine.name, engine.id)
                    } else {
                        engine.name.clone()
                    };
                    d.specs.push(spec(
                        tr("엔진", "Engine"),
                        format!("{name} · {:.1}%", engine.percent),
                    ));
                }
            }
        }
    }
    if d.sub.is_empty() {
        d.sub = d.value.clone();
    }
    d
}
/// A PDH physical-disk instance ("0 D:", "1 C: E:") split into its index
/// and its drive letters ("C: E:").
fn disk_parts(id: &str) -> (String, String) {
    let mut words = id.split_whitespace();
    let first = words.next().unwrap_or("");
    if first.chars().all(|c| c.is_ascii_digit()) && !first.is_empty() {
        (first.to_owned(), words.collect::<Vec<_>>().join(" "))
    } else {
        (String::new(), id.trim().to_owned())
    }
}
/// "Disk 0 (C:)" like the reference (and Task Manager); "Disk 0" without
/// drive letters.
pub(super) fn disk_name(id: &str) -> String {
    let (index, letters) = disk_parts(id);
    let label = tr("디스크", "Disk");
    match (index.is_empty(), letters.is_empty()) {
        (false, false) => format!("{label} {index} ({letters})"),
        (false, true) => format!("{label} {index}"),
        (true, false) => format!("{label} ({letters})"),
        (true, true) => label.to_owned(),
    }
}
/// `.perf-main` vertical flow in CSS px (1/64 px line boxes): the title row,
/// 14 px gaps, 12 px chart captions, the chart, the stats grid and the specs
/// list (`<dl>` keeps its default 1em = 13 px top margin in the reference).
const PERF_TITLE: f32 = 31.6;
const PERF_GAP: f32 = 14.0;
const PERF_CAPTION: f32 = 17.390_625;
const PERF_STAT_KEY: f32 = 17.390_625;
const PERF_STAT_VALUE: f32 = 27.5;
const PERF_STAT_ROW_GAP: f32 = 16.0;
const PERF_STAT_COL_GAP: f32 = 24.0;
const PERF_STAT_MIN: f32 = 140.0;
const PERF_SPEC_ROW: f32 = 18.843_75;
const PERF_SPEC_GAP: f32 = 4.0;
/// Performance page: devices column border and the perf-main content.
unsafe fn performance_panel(p: *mut App, dc: HDC, l: &layout::Layout) {
    let c = colors();
    let f = &(*p).fonts;
    let column = l.perf_devices();
    fill(
        dc,
        RECT {
            left: column.right - l.hair,
            ..column
        },
        c.border,
    );
    let m = l.perf_main();
    let width = m.right - m.left;
    if width <= scale(p, 100) {
        return;
    }
    let dpi = (*p).dpi;
    let display = performance_display(p, &(*p).perf_target);
    // Stats: `repeat(auto-fill, minmax(140px, 1fr))`, gap 16 24.
    let col_gap = gfx::px(dpi, PERF_STAT_COL_GAP);
    let columns = (((width as f32 + col_gap) / (gfx::px(dpi, PERF_STAT_MIN) + col_gap)).floor()
        as usize)
        .max(1);
    let stats = display.stats.len().min(8);
    let stat_rows = stats.div_ceil(columns);
    let specs = display.specs.len();
    let stats_height = stat_rows as f32 * (PERF_STAT_KEY + PERF_STAT_VALUE)
        + stat_rows.saturating_sub(1) as f32 * PERF_STAT_ROW_GAP;
    let specs_height = if specs == 0 {
        0.0
    } else {
        PERF_GAP
            + 13.0
            + 1.0
            + 14.0
            + specs as f32 * PERF_SPEC_ROW
            + (specs - 1) as f32 * PERF_SPEC_GAP
    };
    // `clamp(180px, 34vh, 300px)`: the reference viewport is the window plus
    // its 24 px page margins. Near the minimum size the chart gives up room
    // so the device details stay visible (the reference would scroll).
    let client_h = l.client.bottom as f32 * 96.0 / dpi as f32;
    let fixed =
        PERF_TITLE + PERF_GAP + 2.0 * PERF_CAPTION + PERF_GAP + 6.0 + stats_height + specs_height;
    let available = (m.bottom - m.top) as f32 * 96.0 / dpi as f32;
    // Never below the spec's 180 px floor: at the minimum window size the
    // specs list drops its last rows instead (the loop below stops at the
    // bottom), the chart keeps its shape.
    let chart_h = (0.34 * (client_h + 48.0))
        .clamp(180.0, 300.0)
        .min(available - fixed)
        .max(180.0);
    let y = |offset: f32| m.top + gfx::pxi(dpi, offset);
    let band = |top: f32, height: f32| RECT {
        left: m.left,
        top: y(top),
        right: m.right,
        bottom: y(top + height),
    };
    let pt = Painter::new(dc, dpi, f);
    // Title row: h2 28/600 left, model 14 px muted right, baselines aligned.
    let title_font = f.perf_title;
    let model_font = f.body;
    let (title_height, title_ascent) = font_metrics(dc, title_font);
    let (model_height, model_ascent) = font_metrics(dc, model_font);
    // `h2 { font: 600 28px/1.1 }` on its CSS baseline (the model, 14 px,
    // shares it: `align-items: baseline`).
    let baseline = pt.css_baseline(title_font, band(0.0, 30.8), Some(30.8));
    let title_top = baseline - title_ascent;
    let title_width = text_width(dc, title_font, &display.title);
    let model_width = text_width(dc, model_font, &display.model);
    pt.text(
        title_font,
        c.fg,
        &display.title,
        RECT {
            left: m.left,
            top: title_top,
            right: m.right,
            bottom: title_top + title_height,
        },
        DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    let model_left = (m.right - model_width).max(m.left + title_width + gfx::pxi(dpi, 16.0));
    if model_left < m.right {
        pt.text(
            model_font,
            c.muted,
            &display.model,
            RECT {
                left: model_left,
                top: baseline - model_ascent,
                right: m.right,
                bottom: baseline - model_ascent + model_height,
            },
            DT_SINGLELINE | DT_RIGHT | DT_END_ELLIPSIS,
        );
    }
    let cap_top = PERF_TITLE + PERF_GAP;
    let chart_top = cap_top + PERF_CAPTION;
    let cap2_top = chart_top + chart_h;
    let cap = band(cap_top, PERF_CAPTION);
    cap_text(&pt, f.small, display.caption, cap, PERF_CAPTION, DT_LEFT);
    let chart_rect = band(chart_top, chart_h);
    let trace = (*p).perf_history.traces.get(&(*p).perf_target);
    let points = trace_values(trace, display.index);
    let axis = if display.ceiling == Some(100.0) {
        "100%".into()
    } else if let Some(max) = display.ceiling {
        human_bytes(max)
    } else {
        let max = points
            .iter()
            .map(|v| v.1)
            .filter(|v| v.is_finite())
            .fold(1.0, f64::max)
            * 1.1;
        rate(Some(max))
    };
    cap_text(&pt, f.mono_small, &axis, cap, PERF_CAPTION, DT_RIGHT);
    let cap2 = band(cap2_top, PERF_CAPTION);
    cap_text(
        &pt,
        f.small,
        tr("60초", "60 seconds"),
        cap2,
        PERF_CAPTION,
        DT_LEFT,
    );
    cap_text(&pt, f.mono_small, "0", cap2, PERF_CAPTION, DT_RIGHT);
    drop(pt);
    if (*p).core_graphs && (*p).perf_target == PerfTarget::Cpu {
        core_charts(p, dc, chart_rect);
    } else {
        chart(
            p,
            dc,
            chart_rect,
            &points,
            display.ceiling,
            c.accent,
            ChartStyle::Main,
        );
    }
    let pt = Painter::new(dc, dpi, f);
    let stats_top = cap2_top + PERF_CAPTION + PERF_GAP + 6.0;
    let col_width = (width as f32 - col_gap * (columns - 1) as f32) / columns as f32;
    for (i, (name, value)) in display.stats.iter().take(stats).enumerate() {
        let left = m.left + ((i % columns) as f32 * (col_width + col_gap)).round() as i32;
        let right = left + col_width.floor() as i32;
        let top = stats_top
            + (i / columns) as f32 * (PERF_STAT_KEY + PERF_STAT_VALUE + PERF_STAT_ROW_GAP);
        cap_text(
            &pt,
            f.small,
            name,
            RECT {
                left,
                right,
                ..band(top, PERF_STAT_KEY)
            },
            PERF_STAT_KEY,
            DT_LEFT,
        );
        // Long values (e.g. a missing-counter sentence) fall back to 14/600.
        let font = if text_width(dc, f.mono_stat, value) > right - left {
            f.body_strong
        } else {
            f.mono_stat
        };
        let cell = RECT {
            left,
            right,
            ..band(top + PERF_STAT_KEY, PERF_STAT_VALUE)
        };
        let line = pt.css_rect(font, cell, Some(PERF_STAT_VALUE));
        pt.text(
            font,
            c.fg,
            value,
            RECT {
                left,
                right,
                ..line
            },
            DT_SINGLELINE | DT_END_ELLIPSIS | DT_LEFT,
        );
    }
    if specs == 0 {
        return;
    }
    // Specs: top border, padding-top 14, `max-content 1fr` columns 20 apart.
    let border = stats_top + stats_height + PERF_GAP + 13.0;
    pt.fill(
        RECT {
            left: m.left,
            top: y(border),
            right: m.right,
            bottom: y(border) + l.hair,
        },
        c.border,
    );
    let term_width = display
        .specs
        .iter()
        .map(|(name, _)| text_width(dc, f.ui, name))
        .max()
        .unwrap_or(0)
        .min(width / 2);
    let value_left = m.left + term_width + gfx::pxi(dpi, 20.0);
    for (i, (name, value)) in display.specs.iter().enumerate() {
        let row = band(
            border + 1.0 + 14.0 + i as f32 * (PERF_SPEC_ROW + PERF_SPEC_GAP),
            PERF_SPEC_ROW,
        );
        if row.bottom > m.bottom + gfx::pxi(dpi, PERF_PAD_BOTTOM_SLACK) {
            break;
        }
        pt.label(
            f.ui,
            c.muted,
            name,
            RECT {
                right: value_left - gfx::pxi(dpi, 20.0),
                ..row
            },
            DT_LEFT,
        );
        pt.label(
            f.mono_cell,
            c.fg,
            value,
            RECT {
                left: value_left,
                ..row
            },
            DT_LEFT,
        );
    }
}
/// Muted text on its CSS line box (`line` CSS px) in `band`: the chart
/// captions and stat keys (`.chart-cap`, `.stat .k`; each span of the flex
/// row has its own line box, so the mono number and the text keep their own
/// baselines, like the reference).
fn cap_text(pt: &Painter, font: HFONT, text: &str, band: RECT, line: f32, align: u32) {
    let cell = pt.css_rect(font, band, Some(line));
    pt.text(
        font,
        pt.c.muted,
        text,
        RECT {
            left: band.left,
            right: band.right,
            ..cell
        },
        DT_SINGLELINE | DT_END_ELLIPSIS | align,
    );
}
/// Specs rows may use the perf-main bottom padding before being dropped.
const PERF_PAD_BOTTOM_SLACK: f32 = 16.0;
/// (cell height, ascent) of a font in device px.
unsafe fn font_metrics(dc: HDC, font: HFONT) -> (i32, i32) {
    let old = SelectObject(dc, font);
    let mut metrics: TEXTMETRICW = zeroed();
    GetTextMetricsW(dc, &mut metrics);
    SelectObject(dc, old);
    (metrics.tmHeight, metrics.tmAscent)
}
/// The small-multiples grid for `count` charts in a `width × height` box:
/// (columns, rows) with cells as square as the box allows and as few empty
/// cells as possible (16 CPUs in a wide chart: 8 × 2, not 6 × 3 with two
/// holes).
pub(super) fn core_grid(count: usize, width: f64, height: f64) -> (usize, usize) {
    if count == 0 {
        return (1, 1);
    }
    let aspect = width / height.max(1.0);
    (1..=count)
        .map(|cols| {
            let rows = count.div_ceil(cols);
            let cell = aspect * rows as f64 / cols as f64;
            let empty = cols * rows - count;
            (cols, rows, cell.ln().abs() + 0.25 * empty as f64)
        })
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map_or((1, count), |(cols, rows, _)| (cols, rows))
}
unsafe fn core_charts(p: *mut App, dc: HDC, r: RECT) {
    let Some(perf) = (*p).performance.as_ref() else {
        return;
    };
    let cores = &perf.logical_processors;
    if cores.is_empty() {
        return;
    }
    let count = cores.len().min(512);
    let (cols, rows) = core_grid(count, (r.right - r.left) as f64, (r.bottom - r.top) as f64);
    let (cols, rows) = (cols as i32, rows as i32);
    let dpi = (*p).dpi;
    let gap = gfx::pxi(dpi, 6.0);
    let cw = (r.right - r.left - gap * (cols - 1)) / cols;
    let ch = (r.bottom - r.top - gap * (rows - 1)) / rows;
    let f = &(*p).fonts;
    let c = colors();
    // `.chart-cap` over each small chart (12 px muted, the value in mono):
    // the label never sits on the trace.
    let caption = gfx::pxi(dpi, PERF_CAPTION);
    let captioned = cw > gfx::pxi(dpi, 60.0) && ch > caption + gfx::pxi(dpi, 24.0);
    for (i, core) in cores.iter().take(count).enumerate() {
        let x = r.left + i as i32 % cols * (cw + gap);
        let y = r.top + i as i32 / cols * (ch + gap);
        let cell = RECT {
            left: x,
            top: y,
            right: x + cw,
            bottom: y + ch,
        };
        let chart_rect = if captioned {
            let pt = Painter::new(dc, dpi, f);
            let cap = RECT {
                bottom: y + caption,
                ..cell
            };
            let base = pt.css_baseline(f.small, cap, Some(PERF_CAPTION));
            pt.text_on_baseline(
                f.small,
                c.muted,
                &format!("{}:{}", core.group, core.index),
                cap,
                base,
                DT_LEFT,
            );
            pt.text_on_baseline(
                f.mono_small,
                c.muted,
                &percent(core.percent),
                cap,
                base,
                DT_RIGHT,
            );
            RECT {
                top: y + caption,
                ..cell
            }
        } else {
            cell
        };
        let points = trace_values((*p).perf_history.cores.get(&core.id), 0);
        chart(
            p,
            dc,
            chart_rect,
            &points,
            Some(100.0),
            c.accent,
            ChartStyle::Core,
        );
    }
}
/// One performance device card (`.device`) for `(*p).perf_targets[index]`
/// in the item box `r` (device px): the 56 px card in its top (the rest is
/// the 2 px gap), padding 10, 64 × 36 sparkline, 12 px gap, name 13/600 over
/// a 12 px mono value, the text block centred; `hover` (0..=1, animated)
/// fades in fg_soft, `selected` shows fg_sel, radius 4. The device list
/// (`table::create_devices`) paints its rows with it.
pub(super) unsafe fn device_card(
    p: *mut App,
    dc: HDC,
    r: RECT,
    index: usize,
    hover: f32,
    selected: bool,
) {
    let Some(target) = (&(*p).perf_targets).get(index) else {
        return;
    };
    let c = colors();
    let dpi = (*p).dpi;
    let d = |v: f32| gfx::pxi(dpi, v);
    fill(dc, r, c.surface);
    let card = RECT {
        bottom: (r.top + d(layout::DEVICE_CARD)).min(r.bottom),
        ..r
    };
    let face = if selected {
        Some(c.fg_sel)
    } else if hover > 0.0 {
        Some(theme::mix(c.surface, c.fg_soft, hover.clamp(0.0, 1.0)))
    } else {
        None
    };
    if let Some(face) = face {
        Canvas::new(dc).fill_round_rect(
            RectF::from_rect(card),
            gfx::px(dpi, theme::RADIUS_SM),
            solid(face),
        );
    }
    let display = performance_display(p, target);
    let sx = card.left + d(10.0);
    let sy = card.top + d(10.0);
    let sr = RECT {
        left: sx,
        top: sy,
        right: sx + d(64.0),
        bottom: sy + d(36.0),
    };
    let points = trace_values((*p).perf_history.traces.get(target), display.index);
    chart(p, dc, sr, &points, display.ceiling, c.fg, ChartStyle::Spark);
    let text_left = card.left + d(10.0 + 64.0 + 12.0);
    let block = (layout::DEVICE_CARD - layout::SETTINGS_ROW_TITLE - PERF_CAPTION) / 2.0;
    let name_bottom = card.top + d(block + layout::SETTINGS_ROW_TITLE);
    // `.device b` (13/600) and `.device span` (12 px mono) as block line
    // boxes of 13 and 12 × 1.45 px, on their CSS baselines.
    let pt = Painter::new(dc, dpi, &(*p).fonts);
    let name = RECT {
        left: text_left,
        top: card.top + d(block),
        right: card.right - d(10.0),
        bottom: name_bottom,
    };
    let cell = pt.css_rect(pt.fonts.ui_strong, name, Some(layout::SETTINGS_ROW_TITLE));
    pt.text(
        pt.fonts.ui_strong,
        c.fg,
        &display.title,
        RECT {
            left: name.left,
            right: name.right,
            ..cell
        },
        DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    let sub = RECT {
        top: name_bottom,
        bottom: card.top + d(block + layout::SETTINGS_ROW_TITLE + PERF_CAPTION),
        ..name
    };
    let cell = pt.css_rect(pt.fonts.mono_small, sub, Some(PERF_CAPTION));
    pt.text(
        pt.fonts.mono_small,
        c.muted,
        &display.sub,
        RECT {
            left: sub.left,
            right: sub.right,
            ..cell
        },
        DT_SINGLELINE | DT_END_ELLIPSIS,
    );
}
/// The processes telemetry drawer in `drawer` (layout::Layout::drawer): a
/// 1 px top border, the selection's title and details, three live charts.
unsafe fn process_telemetry(p: *mut App, dc: HDC, l: &layout::Layout, drawer: RECT) {
    let c = colors();
    let d = |v: f32| l.px(v);
    fill(dc, drawer, c.surface);
    fill(
        dc,
        RECT {
            bottom: drawer.top + l.hair,
            ..drawer
        },
        c.border,
    );
    // Content box: 20 px padding; the title row leaves room on the right
    // for the "End process tree" button (placed by `ui::layout`).
    let (left, top) = (drawer.left + d(20.0), drawer.top);
    let inner_w = drawer.right - drawer.left - d(40.0);
    let text_w = (drawer.right - drawer.left - d(220.0)).max(d(120.0));
    let box_at = |y: f32, w: i32, h: f32| RECT {
        left,
        top: top + d(y),
        right: left + w,
        bottom: top + d(y + h),
    };
    let t = &(*p).telemetry;
    let title = if let Some((pid, _)) = t.identity {
        format!(
            "{}  ·  PID {}{}",
            t.name,
            pid,
            if t.ended {
                tr(" · 종료됨", " · Ended")
            } else {
                ""
            }
        )
    } else {
        tr(
            "프로세스를 선택해 사용량 기록을 확인하세요",
            "Select a process to view its resource history",
        )
        .into()
    };
    // 13/600 like the other panel headings (device name, settings title).
    text_box(
        p,
        dc,
        &title,
        box_at(8.0, text_w, 26.0),
        (*p).fonts.ui_strong,
        c.fg,
    );
    if let Some(process) = selected_process(p) {
        let priority = (*p)
            .process_settings
            .as_ref()
            .filter(|settings| settings.priority.is_some())
            .map_or("—", |settings| settings.priority_label);
        let details = tf!(
            "스레드 {} · 핸들 {} · 전용 메모리 {} · 우선 순위 {}",
            "Threads {} · Handles {} · Private memory {} · Priority {}",
            grouped(process.threads),
            grouped(process.handles),
            human_bytes(process.private_bytes as f64),
            priority
        );
        text_box(
            p,
            dc,
            &details,
            box_at(33.0, text_w, 20.0),
            (*p).fonts.small,
            c.muted,
        );
    }
    // Three charts across the content width, 10 px apart.
    let gap = d(10.0);
    let width = (inner_w - 2 * gap) / 3;
    for i in 0..3 {
        let x = left + i as i32 * (width + gap);
        let point = t.trace.points.back();
        let value = point.map_or(f64::NAN, |v| v.values[i]);
        let formatted = match i {
            0 => percent(Some(value)),
            1 => human_bytes(value),
            _ => rate(Some(value)),
        };
        let name = match i {
            0 => "CPU",
            1 => tr("메모리", "Memory"),
            _ => tr("I/O 처리량", "I/O throughput"),
        };
        // `.chart-cap`: the name in 12 px text, the number in mono, one
        // baseline.
        {
            let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
            let cap = RECT {
                left: x,
                right: x + width,
                ..box_at(55.0, 0, 22.0)
            };
            let f = &(*p).fonts;
            let base =
                pt.css_mixed_baseline(cap, &[f.small, f.mono_small], 12.0 * layout::BODY_LINE);
            pt.text_on_baseline(f.small, c.muted, name, cap, base, DT_LEFT);
            let value_left = cap.left + pt.measure(f.small, name).cx + pt.pxi(8.0);
            if value_left < cap.right {
                pt.text_on_baseline(
                    f.mono_small,
                    c.muted,
                    &formatted,
                    RECT {
                        left: value_left,
                        ..cap
                    },
                    base,
                    DT_LEFT,
                );
            }
        }
        let points = trace_values(Some(&t.trace), i);
        chart(
            p,
            dc,
            RECT {
                left: x,
                right: x + width,
                ..box_at(80.0, 0, 78.0)
            },
            &points,
            if i == 0 { Some(100.0) } else { None },
            c.accent,
            ChartStyle::Main,
        );
    }
}
/// Greedy line wrap measured with GDI's own advances (what `Painter::text`
/// draws): spaces break lines; a word wider than `width` breaks after the
/// last of `soft` (path separators) that fits, else between characters.
fn wrap_text(pt: &Painter, font: HFONT, text: &str, width: i32, soft: &[char]) -> Vec<String> {
    let fits = |s: &str| pt.measure(font, s).cx <= width;
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        for word in raw.split(' ').filter(|w| !w.is_empty()) {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if fits(&candidate) {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            let mut rest = word;
            while !rest.is_empty() && !fits(rest) {
                let mut cut = 0;
                let mut soft_cut = 0;
                for (i, ch) in rest.char_indices() {
                    let end = i + ch.len_utf8();
                    if !fits(&rest[..end]) {
                        break;
                    }
                    cut = end;
                    if soft.contains(&ch) {
                        soft_cut = end;
                    }
                }
                // Always make progress, even in a box narrower than a glyph.
                let cut = if soft_cut > 0 {
                    soft_cut
                } else if cut > 0 {
                    cut
                } else {
                    rest.chars().next().map_or(rest.len(), char::len_utf8)
                };
                lines.push(rest[..cut].to_owned());
                rest = &rest[cut..];
            }
            line = rest.to_owned();
        }
        lines.push(line);
    }
    while lines.len() > 1 && lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// Draw `text` wrapped into `left..right` from `top` (line boxes of
/// `line_css` CSS px), at most `max_lines` lines (the last one ellipsized
/// when there is more); returns the bottom of the block.
#[allow(clippy::too_many_arguments)]
unsafe fn text_block(
    pt: &Painter,
    font: HFONT,
    color: u32,
    text: &str,
    left: i32,
    right: i32,
    top: i32,
    line_css: f32,
    max_lines: usize,
    soft: &[char],
) -> i32 {
    let width = (right - left).max(1);
    let mut lines = wrap_text(pt, font, text, width, soft);
    if lines.len() > max_lines {
        let joiner = if soft.is_empty() { " " } else { "" };
        let rest = lines.split_off(max_lines.max(1) - 1).join(joiner);
        lines.push(pt.ellipsize(font, &rest, width));
    }
    let line = pt.px(line_css);
    let mut y = top as f32;
    for text in &lines {
        let band = RECT {
            left,
            top: y.round() as i32,
            right,
            bottom: (y + line).round() as i32,
        };
        let cell = pt.css_rect(font, band, Some(line_css));
        pt.text(
            font,
            color,
            text,
            RECT {
                left,
                right,
                ..cell
            },
            DT_SINGLELINE | DT_LEFT,
        );
        y += line;
    }
    y.round() as i32
}

/// One field of the details panel: (label, value — None while loading,
/// mono, max lines, soft break characters).
type DetailField<'a> = (&'a str, Option<String>, bool, usize, &'a [char]);
/// The services details panel in `panel` (layout::Layout::details): bg with
/// a 1 px left border and a header band like the table's (its bottom border
/// continues the table header's), then the selection stacked top-down with
/// measured heights: the display name (14/600, on the first row's line),
/// the service key (mono 12 muted), the state pill with the PID, the
/// description, and label / value fields 12 px apart. What the list already
/// knows shows at once; the fields only the details query adds say
/// "Loading details…" until it answers. Empty values read "—"; an empty
/// description is left out.
unsafe fn service_details(p: *mut App, dc: HDC, l: &layout::Layout, panel: RECT) {
    let c = colors();
    let f = &(*p).fonts;
    let dpi = (*p).dpi;
    let d = |v: f32| l.px(v);
    fill(dc, panel, c.bg);
    fill(
        dc,
        RECT {
            right: panel.left + l.hair,
            ..panel
        },
        c.border,
    );
    let pt = Painter::new(dc, dpi, f);
    let header = layout::table_header_height(Page::Services, dpi).min(panel.bottom - panel.top);
    widgets::header_cell(
        &pt,
        RECT {
            left: panel.left + l.hair,
            bottom: panel.top + header,
            ..panel
        },
        None,
        tr("세부 정보", "Details"),
        false,
        false,
        None,
        0.0,
    );
    let left = panel.left + l.hair + d(16.0);
    let right = (panel.right - d(16.0)).max(left + 1);
    // The first line sits where the table's first row puts its text.
    let row = layout::table_row_height(dpi) - l.hair;
    let first = panel.top + header;
    let body_line = 14.0 * layout::BODY_LINE;
    let mut y = first + (row - pt.pxi(body_line)) / 2;
    let Some(name) = (*p).service_detail_name.clone() else {
        text_block(
            &pt,
            f.body,
            c.muted,
            tr("서비스를 선택하세요", "Select a service"),
            left,
            right,
            y,
            body_line,
            2,
            &[],
        );
        return;
    };
    let listed = (*p).services.iter().find(|s| s.name == name);
    let detail = (*p)
        .service_details
        .as_ref()
        .filter(|detail| detail.name == name);
    let display = detail
        .map(|d| d.display_name.as_str())
        .or(listed.map(|s| s.display_name.as_str()))
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(&name);
    let small_line = 12.0 * layout::BODY_LINE;
    let bottom_limit = panel.bottom - d(12.0);
    y = text_block(
        &pt,
        f.body_strong,
        c.fg,
        display,
        left,
        right,
        y,
        body_line,
        3,
        &[],
    );
    if display != name {
        y = text_block(
            &pt,
            f.mono_small,
            c.muted,
            &name,
            left,
            right,
            y,
            small_line,
            2,
            &['_', '.', '-'],
        );
    }
    // State pill and PID (`.pill.ok` for Running, like the table).
    let state = detail.map(|d| d.state).or(listed.map(|s| s.state));
    let pid = detail.map(|d| d.pid).or(listed.map(|s| s.pid)).unwrap_or(0);
    if let Some(state) = state {
        y += d(8.0);
        let height = pt.pxi(20.0);
        let kind = if state == SERVICE_RUNNING {
            widgets::PillKind::Ok
        } else {
            widgets::PillKind::Default
        };
        let width = widgets::pill(
            &pt,
            left,
            y + height / 2,
            kind,
            crate::services::state_label(state),
            false,
        );
        if pid != 0 {
            let band = RECT {
                left: left + width + d(8.0),
                top: y,
                right,
                bottom: y + height,
            };
            let base = pt.css_baseline(f.mono_small, band, Some(small_line));
            pt.text_on_baseline(
                f.mono_small,
                c.muted,
                &format!("PID {pid}"),
                band,
                base,
                DT_LEFT,
            );
        }
        y += height;
    }
    let gap = d(12.0);
    let loading = || {
        (*p).service_detail_error
            .clone()
            .unwrap_or_else(|| tr("세부 정보를 불러오는 중…", "Loading details…").into())
    };
    if let Some(detail) = detail {
        if !detail.description.trim().is_empty() {
            y = text_block(
                &pt,
                f.small,
                c.muted,
                detail.description.trim(),
                left,
                right,
                y + gap,
                small_line,
                6,
                &[][..],
            );
        }
    }
    let dash = || "—".to_string();
    let or_dash = |v: &str| {
        if v.trim().is_empty() {
            dash()
        } else {
            v.to_owned()
        }
    };
    let start_type = detail
        .map(|d| Some(d.start_type))
        .or(listed.map(|s| s.start_type))
        .map(|v| crate::services::start_type_label(v).to_owned());
    // (label, value, mono, max lines, soft breaks). None: loading.
    let mut fields: Vec<DetailField> =
        vec![(tr("시작 유형", "Startup type"), start_type, false, 1, &[])];
    match detail {
        Some(detail) => fields.extend([
            (
                tr("계정", "Account"),
                Some(or_dash(&detail.account)),
                false,
                2,
                &[][..],
            ),
            (
                tr("로드 그룹", "Load group"),
                Some(or_dash(&detail.load_order_group)),
                false,
                2,
                &[][..],
            ),
            (
                tr("종속성", "Dependencies"),
                Some(if detail.dependencies.is_empty() {
                    dash()
                } else {
                    detail.dependencies.join(", ")
                }),
                false,
                3,
                &[][..],
            ),
            (
                tr("실행 경로", "Binary path"),
                Some(or_dash(&detail.binary_path)),
                true,
                4,
                &['\\', '/', ' '],
            ),
        ]),
        None => fields.push(("", None, false, 2, &[][..])),
    }
    for (label, value, mono, lines, soft) in fields {
        if y + gap + pt.pxi(small_line * 2.0) > bottom_limit {
            break;
        }
        y = if label.is_empty() {
            y + gap
        } else {
            text_block(
                &pt,
                f.small,
                c.muted,
                label,
                left,
                right,
                y + gap,
                small_line,
                1,
                &[],
            )
        };
        let (font, color, line, text) = match value {
            Some(value) if mono => (f.mono_small, c.fg, small_line, value),
            Some(value) => (f.ui, c.fg, 13.0 * layout::BODY_LINE, value),
            None => (f.small, c.muted, small_line, loading()),
        };
        let room = ((bottom_limit - y) as f32 / pt.px(line)).floor().max(1.0) as usize;
        y = text_block(
            &pt,
            font,
            color,
            &text,
            left,
            right,
            y,
            line,
            lines.min(room),
            soft,
        );
    }
    if let Some(detail) = detail {
        for warning in &detail.warnings {
            if y + gap + pt.pxi(small_line) > bottom_limit {
                break;
            }
            y = text_block(
                &pt,
                f.small,
                c.muted,
                warning,
                left,
                right,
                y + gap,
                small_line,
                3,
                &[][..],
            );
        }
    }
}
// The color behind a child control: settings rows, the rail, the title bar
/// and the status bar are `bg`; the main panel is `surface`.
pub(super) unsafe fn parent_background(p: *mut App, control: HWND) -> u32 {
    let c = colors();
    if (*p).preference_controls.contains(&control) {
        return c.bg;
    }
    let mut r: RECT = zeroed();
    GetWindowRect(control, &mut r);
    MapWindowPoints(
        null_mut(),
        (*p).hwnd,
        (&mut r as *mut RECT).cast::<POINT>(),
        2,
    );
    let l = current_layout(p);
    if r.left < l.main.left || r.top < l.main.top || r.bottom > l.status.top {
        c.bg
    } else {
        c.surface
    }
}
pub(super) unsafe fn window_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub(super) unsafe fn draw_button(p: *mut App, item: &DRAWITEMSTRUCT) {
    if item.CtlType != ODT_BUTTON {
        return;
    }
    // Paint off-screen and copy once: owner-draw buttons never flicker. A
    // neighbour's outside focus ring crossing the button goes on top.
    gfx::buffered_item(item.hDC, &item.rcItem, |dc| {
        draw_button_to(p, item, dc);
        controls::paint_neighbour_ring(p, dc, item.hwndItem, item.rcItem);
    });
}
unsafe fn draw_button_to(p: *mut App, item: &DRAWITEMSTRUCT, dc: HDC) {
    let r = item.rcItem;
    let id = item.CtlID as usize;
    let parent = parent_background(p, item.hwndItem);
    let hover = (*p).anim.value((id, anim::part::HOVER));
    let state = ButtonState {
        hover,
        pressed: item.itemState & ODS_SELECTED != 0,
        focused: item.itemState & ODS_FOCUS != 0
            && item.itemState & ODS_NOFOCUSRECT == 0
            && !controls::ring_outside(p, item.hwndItem),
        disabled: item.itemState & ODS_DISABLED != 0,
        selected: false,
    };
    let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
    // Settings switches keep native button focus/keyboard behavior.
    if (PREF_TOP..=PREF_ADMIN).contains(&id) {
        let checked = match id {
            PREF_TOP => (*p).topmost,
            PREF_TRAY => (*p).prefs.tray,
            PREF_REPLACE => (*p).replacement_active,
            _ => (*p).prefs.always_admin,
        };
        (*p).anim
            .register((id, anim::part::SWITCH), item.hwndItem, None);
        let t = (*p).anim.follow(
            (id, anim::part::SWITCH),
            checked as u8 as f32,
            anim::motion::SWITCH,
            anim::Easing::Ease,
        );
        pt.fill(r, parent);
        // The button is the 40 x 20 switch plus a 4 px margin that holds the
        // focus ring outside the switch (`:focus-visible`, offset 2).
        let x = r.right - pt.pxi(44.0);
        let y = (r.top + r.bottom - pt.pxi(20.0)) / 2;
        let switch = RECT {
            left: x,
            top: y,
            right: x + pt.pxi(40.0),
            bottom: y + pt.pxi(20.0),
        };
        widgets::switch_to(&pt, switch, t, checked, !state.disabled, false, parent);
        if state.focused && !state.disabled {
            widgets::focus_ring(&pt, switch, pt.px(theme::RADIUS_PILL));
        }
        return;
    }
    let text = window_text(item.hwndItem);
    if (THEME_LIGHT..=THEME_SYSTEM).contains(&id) {
        let selected = match id {
            THEME_LIGHT => (*p).prefs.theme == 1,
            THEME_DARK => (*p).prefs.theme == 2,
            _ => (*p).prefs.theme == 0,
        };
        widgets::segment(
            &pt,
            r,
            id == THEME_LIGHT,
            id == THEME_SYSTEM,
            selected,
            hover,
            &text,
            state.focused,
            parent,
        );
        return;
    }
    let nav = (NAV..NAV + 4).contains(&id);
    if nav || id == SETTINGS || id == RUN_TASK {
        let current =
            nav && id - NAV == (*p).page as usize || id == SETTINGS && (*p).page == Page::Settings;
        let count = (id == NAV)
            .then(|| {
                (*p).snapshot
                    .as_ref()
                    .map(|s| s.processes.len().to_string())
            })
            .flatten();
        let glyph = if nav {
            Icon::nav(id - NAV)
        } else if id == SETTINGS {
            Icon::Settings
        } else {
            Icon::Plus
        };
        // The current item's background cross-fades (DESIGN_SPEC §6, 180 ms;
        // targets set by `ui::place_nav_indicator` with the page change); the
        // indicator slides separately across the rail (`nav_indicator`).
        let selected_t = (*p).anim.follow(
            (id, anim::part::SELECTED),
            current as u8 as f32,
            anim::motion::NAV,
            anim::Easing::EaseOut,
        );
        widgets::nav_item_with(
            &pt,
            r,
            &ButtonState {
                selected: current,
                ..state
            },
            selected_t,
            false,
            &text,
            count.as_deref(),
            parent,
            |pt, x, y, size, color| widgets::icon(pt, glyph, x, y, size, color),
        );
        if id != RUN_TASK {
            let origin = child_rect(p, item.hwndItem);
            nav_indicator(
                p,
                &pt,
                POINT {
                    x: origin.left,
                    y: origin.top,
                },
            );
        }
        return;
    }
    let style = button_style(p, id);
    let selected = id == TOP && (*p).topmost || id == CORES && (*p).core_graphs;
    let state = ButtonState { selected, ..state };
    controls::button_drawn(p, item.hwndItem, state.pressed && !state.disabled);
    let iconic = style == ButtonStyle::Icon || id == NUCLEAR;
    let content = widgets::button_face(
        &pt,
        r,
        style,
        &state,
        if iconic { "" } else { &text },
        parent,
    );
    if id == NUCLEAR {
        // `.btn` with a leading icon: trefoil, 6 px gap, label, centred.
        // A narrow head sizes the button without it (`ui::head_layout`).
        let (_, _, fg) = widgets::button_colors(&pt.c, style, &state, parent);
        let size = pt.px(NUCLEAR_ICON).round();
        let label = pt.measure(pt.fonts.ui, &text).cx;
        let gap = pt.pxi(NUCLEAR_GAP);
        let icon = content.right - content.left >= label + size as i32 + gap;
        let (size, gap) = if icon { (size, gap) } else { (0.0, 0) };
        let left = (content.left + content.right - (size as i32 + gap + label)) / 2;
        if icon {
            widgets::radiation(
                &pt,
                left as f32,
                ((content.top + content.bottom) as f32 - size) / 2.0,
                size,
                fg,
            );
        }
        let label_box = RECT {
            left: left + size as i32 + gap,
            right: content.right.max(left + size as i32 + gap + label),
            ..content
        };
        let draw = || pt.label(pt.fonts.ui, fg, &text, label_box, DT_LEFT);
        if state.disabled {
            // `.45` of a label drawn in full fg keeps the full color's weight.
            let (_, _, full) = widgets::button_colors(
                &pt.c,
                style,
                &ButtonState {
                    disabled: false,
                    ..state
                },
                parent,
            );
            fonts::with_lift_of(full, draw);
        } else {
            draw();
        }
        return;
    }
    if style == ButtonStyle::Icon {
        let (_, _, fg) = widgets::button_colors(&pt.c, style, &state, parent);
        let size = pt.px(16.0).round();
        widgets::icon(
            &pt,
            Icon::More,
            ((content.left + content.right) as f32 - size) / 2.0,
            ((content.top + content.bottom) as f32 - size) / 2.0,
            size,
            fg,
        );
    }
}
/// The Nuclear Zombie button's trefoil and the gap before its label (DIP).
pub(super) const NUCLEAR_ICON: f32 = 14.0;
pub(super) const NUCLEAR_GAP: f32 = 6.0;
/// The face style of an owner-draw action button.
pub(super) unsafe fn button_style(p: *mut App, id: usize) -> ButtonStyle {
    match id {
        MORE => ButtonStyle::Icon,
        TOP => ButtonStyle::Ghost,
        PRIMARY if (*p).page == Page::Processes => ButtonStyle::Primary,
        _ => ButtonStyle::Default,
    }
}
pub(super) unsafe fn combo_text(hwnd: HWND, index: isize) -> String {
    if index < 0 {
        return String::new();
    }
    let len = SendMessageW(hwnd, CB_GETLBTEXTLEN, index as usize, 0);
    if !(0..=2048).contains(&len) {
        return String::new();
    }
    let mut value = vec![0_u16; len as usize + 1];
    let copied = SendMessageW(
        hwnd,
        CB_GETLBTEXT,
        index as usize,
        value.as_mut_ptr() as isize,
    );
    if copied < 0 || copied > len {
        return String::new();
    }
    String::from_utf16_lossy(&value[..copied as usize])
}

/// The closed face of a `.select` (controls.rs; see `widgets::select_face`).
pub(super) unsafe fn combo_face(p: *mut App, hwnd: HWND, dc: HDC) {
    let mut bounds: RECT = zeroed();
    GetClientRect(hwnd, &mut bounds);
    let index = SendMessageW(hwnd, CB_GETCURSEL, 0, 0);
    let open = SendMessageW(hwnd, CB_GETDROPPEDSTATE, 0, 0) != 0;
    let pt = Painter::new(dc, (*p).dpi, &(*p).fonts);
    // The status bar's select inherits `.statusbar`'s mono font and muted color.
    let status = hwnd == (*p).rate;
    widgets::select_face_with(
        &pt,
        bounds,
        &combo_text(hwnd, index),
        &FieldState {
            hover: 0.0,
            focused: controls::inner_focus(p, hwnd),
            disabled: IsWindowEnabled(hwnd) == 0,
            open,
        },
        status.then_some((*p).fonts.mono_small),
        status.then_some(pt.c.muted),
        parent_background(p, hwnd),
    );
}

/// The title-strip search placeholder of a page (DESIGN_SPEC §3).
pub(super) fn search_placeholder(page: Page) -> &'static str {
    match page {
        Page::Processes => tr("이름, 파일 또는 PID로 검색", "Search by name, file or PID"),
        Page::Startup => tr("시작 앱 검색", "Search startup apps"),
        Page::Services => tr("서비스 검색", "Search services"),
        Page::Performance | Page::Settings => tr(
            "여기에서는 검색할 수 없습니다",
            "Search isn\u{2019}t available here",
        ),
    }
}
pub(super) unsafe fn search_cue(p: *mut App, dc: HDC, bounds: RECT) {
    if GetWindowTextLengthW((*p).search) != 0 {
        return;
    }
    label(
        dc,
        (*p).fonts.body,
        colors().muted,
        search_placeholder((*p).page),
        bounds,
        DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hardware_values_are_formatted_with_honest_units() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            assert_eq!(celsius(38.9), "39 \u{00B0}C");
            assert_eq!(cache_size(640 * 1024), "640 KB");
            assert_eq!(cache_size(96 * 1_048_576), "96.0 MB");
            assert_eq!(link_speed(2_500_000_000), "2.5 Gbps");
            assert_eq!(link_speed(1_000_000_000), "1 Gbps");
            assert_eq!(link_speed(866_000_000), "866 Mbps");
            assert_eq!(human_bytes(2_000_398_934_016.0), "1.8 TB");
            assert_eq!(yes_no(None), "—");
            let usb = crate::storage::StorageDevice {
                disk_number: 3,
                model: Some("SanDisk Cruzer Blade".into()),
                vendor_id: None,
                product_id: None,
                firmware_revision: None,
                bus: Some(crate::storage::StorageBus::Usb),
                removable: Some(true),
                seek_penalty: None,
                trim_enabled: None,
                capacity_bytes: Some(31_406_948_352),
                system_disk: Some(false),
                page_file: Some(false),
            };
            // No seek-penalty report: never called SSD or HDD.
            assert_eq!(disk_type(&usb).as_deref(), Some("Removable (USB)"));
            let nvme = crate::storage::StorageDevice {
                bus: Some(crate::storage::StorageBus::Nvme),
                removable: Some(false),
                seek_penalty: Some(false),
                ..usb
            };
            assert_eq!(disk_type(&nvme).as_deref(), Some("SSD (NVMe)"));
            assert_eq!(
                network_note("Requires administrator").0,
                "Network per process requires administrator"
            );
            assert_eq!(
                network_note("Network trace stopped (error 5)").1,
                "Network trace stopped"
            );
        });
    }

    #[test]
    fn disks_are_named_like_task_manager() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            assert_eq!(disk_name("0 D:"), "Disk 0 (D:)");
            assert_eq!(disk_name("1 C: E:"), "Disk 1 (C: E:)");
            assert_eq!(disk_name("2"), "Disk 2");
            assert_eq!(disk_name("_Total"), "Disk (_Total)");
        });
    }

    #[test]
    fn core_grids_fill_their_cells() {
        // 16 CPUs in a wide chart: two rows of eight, no holes.
        assert_eq!(core_grid(16, 690.0, 200.0), (8, 2));
        assert_eq!(core_grid(12, 690.0, 200.0), (6, 2));
        assert_eq!(core_grid(4, 300.0, 300.0), (2, 2));
        assert_eq!(core_grid(1, 690.0, 200.0), (1, 1));
        assert_eq!(core_grid(0, 690.0, 200.0), (1, 1));
        let (cols, rows) = core_grid(7, 690.0, 200.0);
        assert!(cols * rows >= 7);
    }
}
