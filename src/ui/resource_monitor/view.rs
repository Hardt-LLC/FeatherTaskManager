use super::*;
use gfx::{Radii, RectF};
use theme::solid;

fn rect(x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}
unsafe fn move_to(hwnd: HWND, r: RECT) {
    let mut previous: RECT = zeroed();
    GetWindowRect(hwnd, &mut previous);
    MapWindowPoints(
        null_mut(),
        GetParent(hwnd),
        (&mut previous as *mut RECT).cast(),
        2,
    );
    if previous.left == r.left
        && previous.top == r.top
        && previous.right == r.right
        && previous.bottom == r.bottom
    {
        return;
    }
    SetWindowPos(
        hwnd,
        null_mut(),
        r.left,
        r.top,
        (r.right - r.left).max(0),
        (r.bottom - r.top).max(0),
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
}
unsafe fn show(hwnd: HWND, visible: bool) {
    if (GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_VISIBLE != 0) != visible {
        ShowWindow(hwnd, if visible { SW_SHOWNA } else { SW_HIDE });
    }
}
unsafe fn nav_width(s: *mut State, width: i32) -> i32 {
    gfx::pxi(
        (*s).dpi,
        if width < gfx::pxi((*s).dpi, 960.0) {
            56.0
        } else {
            220.0
        },
    )
}
pub(super) unsafe fn layout(s: *mut State) {
    if (*s).body.is_null() {
        return;
    }
    let mut r: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut r);
    let px = |v| gfx::pxi((*s).dpi, v);
    let nav = nav_width(s, r.right);
    let top = px(44.0);
    let head = px(68.0);
    let foot = px(30.0);
    let search_left = (nav + px(28.0)).max((r.right - px(460.0)) / 2);
    let search_right = (search_left + px(460.0)).min(r.right - px(154.0));
    move_to(
        (*s).search,
        rect(
            search_left + px(30.0),
            px(11.0),
            (search_right - search_left - px(43.0)).max(0),
            px(22.0),
        ),
    );
    for (i, h) in (*s).nav.iter().enumerate() {
        move_to(
            *h,
            rect(
                px(8.0),
                top + px(4.0) + i as i32 * px(42.0),
                nav - px(16.0),
                px(40.0),
            ),
        );
    }
    move_to(
        (*s).back_button,
        rect(
            px(8.0),
            r.bottom - foot - px(48.0),
            nav - px(16.0),
            px(40.0),
        ),
    );
    let right = r.right - px(20.0);
    let end_width = px(132.0);
    let clear_width = px(96.0);
    let trace_width = px(136.0);
    let gap = px(8.0);
    move_to(
        (*s).end,
        rect(right - end_width, top + px(12.0), end_width, px(32.0)),
    );
    move_to(
        (*s).clear,
        rect(
            right - end_width - gap - clear_width,
            top + px(12.0),
            clear_width,
            px(32.0),
        ),
    );
    move_to(
        (*s).trace_button,
        rect(
            right - end_width - clear_width - trace_width - gap * 2,
            top + px(12.0),
            trace_width,
            px(32.0),
        ),
    );
    show(
        (*s).trace_button,
        matches!((*s).tab, Tab::Overview | Tab::Disk | Tab::Network),
    );
    move_to(
        (*s).rate,
        rect(
            r.right - px(92.0),
            r.bottom - foot + px(4.0),
            px(80.0),
            px(22.0),
        ),
    );
    move_to(
        (*s).body,
        rect(
            nav + 1,
            top + head,
            r.right - nav - 1,
            (r.bottom - top - head - foot).max(0),
        ),
    );
    layout_body(s);
}
pub(super) unsafe fn layout_body(s: *mut State) {
    let mut client: RECT = zeroed();
    GetClientRect((*s).body, &mut client);
    if client.right <= 0 || client.bottom <= 0 {
        return;
    }
    let dpi = (*s).dpi;
    let px = |v| gfx::pxi(dpi, v);
    let mut window: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut window);
    let stacked = window.right < px(1100.0);
    let panels_width = if stacked {
        client.right
    } else {
        (client.right - px(320.0)).max(px(380.0))
    };
    (*s).panels_width = panels_width;
    let mut y = px(16.0);
    for panel in &mut (*s).panels {
        let collapsed = (*s).collapsed.contains(&((*s).tab, panel.kind));
        let body = if collapsed {
            0
        } else if panel.kind == Kind::Physical {
            px(196.0)
        } else {
            px((panel.rows.len().min(7) as f32 * 32.0 + 34.0).clamp(120.0, 264.0))
        };
        panel.bounds = rect(
            px(20.0),
            y,
            (panels_width - px(40.0)).max(0),
            px(44.0) + body,
        );
        y += px(44.0) + body + px(12.0);
    }
    (*s).chart_left = if stacked { 0 } else { panels_width };
    (*s).chart_top = if stacked { y + px(8.0) } else { 0 };
    (*s).chart_width = if stacked {
        client.right
    } else {
        client.right - panels_width
    };
    let charts = charts(s);
    let mut chart_height = charts.len() as i32 * px(144.0) + px(16.0);
    if (*s).tab == Tab::Cpu {
        let cores = (*s)
            .performance
            .as_ref()
            .map_or(0, |p| p.logical_processors.len().min(256));
        let columns = if stacked {
            (((*s).chart_width - px(32.0)) / px(100.0)).max(1)
        } else {
            3
        };
        chart_height += px(34.0) + (cores as i32 + columns - 1) / columns * px(68.0);
    }
    (*s).content_height = (y + px(20.0)).max((*s).chart_top + chart_height + px(16.0));
    (*s).scroll = (*s)
        .scroll
        .clamp(0, ((*s).content_height - client.bottom).max(0));
    for panel in &(*s).panels {
        let r = panel.bounds;
        let top = r.top - (*s).scroll;
        move_to(panel.header, rect(r.left, top, r.right - r.left, px(44.0)));
        if !panel.table.is_null() {
            let visible = !(*s).collapsed.contains(&((*s).tab, panel.kind));
            show(panel.table, visible);
            if visible {
                move_to(
                    panel.table,
                    rect(
                        r.left + 1,
                        top + px(44.0),
                        r.right - r.left - 2,
                        r.bottom - r.top - px(44.0) - 1,
                    ),
                );
            }
        }
    }
}
unsafe fn frame(s: *mut State, dc: HDC) {
    let mut r: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut r);
    let pt = Painter::new(dc, (*s).dpi, &(*s).fonts);
    let px = |v| pt.pxi(v);
    let nav = nav_width(s, r.right);
    let title = px(44.0);
    let foot = px(30.0);
    pt.fill(r, pt.c.bg);
    let main = rect(nav, title, r.right - nav, r.bottom - title - foot);
    pt.canvas.bordered_round_rect(
        RectF::from_rect(main),
        Radii::top(pt.px(8.0)),
        pt.hair(),
        solid(pt.c.surface),
        solid(pt.c.border),
    );
    widgets::icon(
        &pt,
        widgets::Icon::Feather,
        pt.px(16.0),
        pt.px(13.0),
        pt.px(18.0),
        pt.c.fg,
    );
    if nav > px(56.0) {
        pt.label(
            pt.fonts.ui,
            pt.c.fg,
            "Feather Resource Monitor",
            rect(px(44.0), 0, nav - px(48.0), title),
            DT_LEFT,
        );
    }
    let mut input: RECT = zeroed();
    GetWindowRect((*s).search, &mut input);
    MapWindowPoints(
        null_mut(),
        (*s).hwnd,
        &mut input as *mut RECT as *mut POINT,
        2,
    );
    let search = RECT {
        left: input.left - px(30.0),
        top: px(6.0),
        right: input.right + px(13.0),
        bottom: px(38.0),
    };
    pt.canvas.bordered_round_rect(
        RectF::from_rect(search),
        Radii::all(pt.px(4.0)),
        pt.hair(),
        solid(pt.c.surface),
        solid(if GetFocus() == (*s).search {
            pt.c.fg
        } else {
            pt.c.border
        }),
    );
    widgets::icon(
        &pt,
        widgets::Icon::Search,
        search.left as f32 + pt.px(10.0),
        pt.px(15.0),
        pt.px(14.0),
        pt.c.muted,
    );
    chrome::paint(s, &pt, r.right);
    pt.label(
        pt.fonts.h1,
        pt.c.fg,
        (*s).tab.title(),
        rect(nav + px(20.0), title + px(8.0), px(185.0), px(32.0)),
        DT_LEFT,
    );
    let meta = meta(s);
    pt.label(
        pt.fonts.small,
        pt.c.muted,
        &meta,
        rect(
            nav + px(20.0),
            title + px(43.0),
            r.right - nav - px(40.0),
            px(20.0),
        ),
        DT_LEFT,
    );
    pt.fill(rect(nav, title + px(67.0), r.right - nav, 1), pt.c.border);
    pt.fill(rect(0, r.bottom - foot, r.right, 1), pt.c.border);
    let running = (*s).interval > 0;
    widgets::status_dot(
        &pt,
        pt.px(18.0),
        (r.bottom - foot / 2) as f32,
        if running { pt.c.fg } else { pt.c.warn_fg },
    );
    let count = (*s).snapshot.as_ref().map_or(0, |p| p.processes.len());
    let mut status = format!(
        "{}  ·  {} {}",
        if running {
            tr("실시간", "Live")
        } else {
            tr("일시 중지", "Paused")
        },
        count,
        tr("프로세스", "processes")
    );
    if !(*s).checked.is_empty() {
        status.push_str(&format!(
            "  ·  {} {}",
            (*s).checked.len(),
            tr("개 필터", "selected")
        ));
    }
    if super::request((*s).owner) != crate::resource::Request::default() {
        if let Some(data) = &(*s).data {
            let age = Instant::now()
                .saturating_duration_since(data.sampled_at)
                .as_secs();
            if age >= 5 {
                status.push_str(&format!(
                    "  ·  {} {age}s",
                    tr("세부 데이터 경과", "Details age")
                ));
            }
        }
    }
    let owner = &*(*s).owner;
    if let Some(error) = owner.error.as_ref() {
        status = error.clone();
    } else if !owner.notice.is_empty() {
        status = owner.notice.clone();
    }
    pt.label(
        pt.fonts.mono_small,
        if owner.error.is_some() {
            pt.c.warn_fg
        } else {
            pt.c.muted
        },
        &status,
        rect(
            px(30.0),
            r.bottom - foot,
            (r.right - px(330.0)).max(0),
            foot,
        ),
        DT_LEFT,
    );
    if let Some(me) = (*s)
        .snapshot
        .as_ref()
        .and_then(|p| p.processes.iter().find(|p| p.pid == std::process::id()))
    {
        let text = format!(
            "Feather {:.1}% · {}",
            me.cpu_percent,
            paint::human_bytes(me.working_set as f64)
        );
        pt.label(
            pt.fonts.mono_tiny,
            pt.c.fg,
            &text,
            rect(r.right - px(390.0), r.bottom - foot, px(250.0), foot),
            DT_RIGHT,
        );
    }
    pt.label(
        pt.fonts.small,
        pt.c.muted,
        tr("갱신", "Refresh"),
        rect(r.right - px(138.0), r.bottom - foot, px(42.0), foot),
        DT_RIGHT,
    );
}
unsafe fn meta(s: *mut State) -> String {
    match (*s).tab {
        Tab::Overview => tr(
            "프로세스별 CPU·I/O·네트워크·메모리 · 상세 추적은 필요할 때만 켜세요",
            "Live CPU, I/O, network and memory by process · tracing is optional",
        )
        .into(),
        Tab::Cpu => (*s).performance.as_ref().map_or_else(String::new, |p| {
            format!(
                "{} · {} {} · {}",
                p.cpu_name,
                p.logical_cpus,
                tr("논리 프로세서", "logical processors"),
                tr("평균 CPU: 창을 연 이후", "Avg CPU: since opening")
            )
        }),
        Tab::Memory => (*s).snapshot.as_ref().map_or_else(String::new, |p| {
            format!(
                "{} {} · {}",
                paint::human_bytes(p.memory_total as f64),
                tr("전체 물리 메모리", "total physical memory"),
                tr(
                    "개인: 상주 작업 집합 · 커밋: 개인 커밋",
                    "Private: resident working set · Commit: private committed bytes"
                )
            )
        }),
        Tab::Disk => tr(
            "물리 디스크 처리량과 파일 I/O 요청을 구분해 표시합니다",
            "Physical disk transfers and file I/O requests are shown separately",
        )
        .into(),
        Tab::Network => tr(
            "실제 TCP·UDP 포트 · 주소별 전송량은 선택적 상세 추적",
            "Live TCP/UDP ports · traffic by endpoint uses optional tracing",
        )
        .into(),
    }
}
pub(super) unsafe fn paint_window(s: *mut State) {
    let mut back = std::mem::take(&mut (*s).back);
    gfx::paint_buffered((*s).hwnd, &mut back, |dc, _| {
        exclude_children((*s).hwnd, dc);
        frame(s, dc);
    });
    (*s).back = back;
}
pub(super) unsafe fn print_window(s: *mut State, dc: HDC) {
    frame(s, dc);
}
pub(super) unsafe fn paint_body(s: *mut State) {
    let mut back = std::mem::take(&mut (*s).body_back);
    gfx::paint_buffered((*s).body, &mut back, |dc, _| {
        exclude_children((*s).body, dc);
        body(s, dc);
    });
    (*s).body_back = back;
}
/// WS_CLIPCHILDREN clips the screen DC, but not the reusable memory DC.
/// Avoid drawing large parent surfaces behind the tables and other children.
unsafe fn exclude_children(hwnd: HWND, dc: HDC) {
    let mut child = GetWindow(hwnd, GW_CHILD);
    while !child.is_null() {
        if GetWindowLongPtrW(child, GWL_STYLE) as u32 & WS_VISIBLE != 0 {
            let mut r: RECT = zeroed();
            GetWindowRect(child, &mut r);
            MapWindowPoints(null_mut(), hwnd, (&mut r as *mut RECT).cast(), 2);
            ExcludeClipRect(dc, r.left, r.top, r.right, r.bottom);
        }
        child = GetWindow(child, GW_HWNDNEXT);
    }
}
pub(super) unsafe fn print_body(s: *mut State, dc: HDC) {
    body(s, dc);
}
unsafe fn body(s: *mut State, dc: HDC) {
    let mut r: RECT = zeroed();
    GetClientRect((*s).body, &mut r);
    let pt = Painter::new(dc, (*s).dpi, &(*s).fonts);
    pt.fill(r, pt.c.surface);
    let chart = rect(
        (*s).chart_left,
        (*s).chart_top - (*s).scroll,
        (*s).chart_width,
        ((*s).content_height - (*s).chart_top).max(r.bottom - ((*s).chart_top - (*s).scroll)),
    );
    pt.fill(chart, pt.c.bg);
    if (*s).chart_left > 0 {
        pt.fill(rect(chart.left, 0, 1, r.bottom), pt.c.border);
    } else {
        pt.fill(rect(0, chart.top, r.right, 1), pt.c.border);
    }
    for panel in &(*s).panels {
        let bounds = RECT {
            top: panel.bounds.top - (*s).scroll,
            bottom: panel.bounds.bottom - (*s).scroll,
            ..panel.bounds
        };
        if bounds.bottom < 0 || bounds.top > r.bottom {
            continue;
        }
        pt.canvas.bordered_round_rect(
            RectF::from_rect(bounds),
            Radii::all(pt.px(8.0)),
            pt.hair(),
            solid(pt.c.surface),
            solid(pt.c.border),
        );
        if panel.kind == Kind::Physical && !(*s).collapsed.contains(&((*s).tab, Kind::Physical)) {
            physical(
                s,
                &pt,
                RECT {
                    top: bounds.top + pt.pxi(44.0),
                    ..bounds
                },
            );
        }
    }
    let charts = charts(s);
    let x = chart.left + pt.pxi(16.0);
    let width = chart.right - chart.left - pt.pxi(32.0);
    let mut y = chart.top + pt.pxi(16.0);
    for (i, c) in charts.iter().enumerate() {
        if y + pt.pxi(144.0) >= 0 && y < r.bottom {
            draw_chart(s, &pt, c, rect(x, y, width, pt.pxi(126.0)), i == 0);
        }
        y += pt.pxi(144.0);
    }
    if (*s).tab == Tab::Cpu {
        pt.label(
            pt.fonts.ui_strong,
            pt.c.fg,
            tr("논리 프로세서", "Logical processors"),
            rect(x, y, width, pt.pxi(24.0)),
            DT_LEFT,
        );
        y += pt.pxi(30.0);
        if let Some(perf) = &(*s).performance {
            let cols = if (*s).chart_left == 0 {
                (width / pt.pxi(100.0)).max(1)
            } else {
                3
            };
            let gap = pt.pxi(10.0);
            let w = (width - gap * (cols - 1)) / cols;
            for (i, cpu) in perf.logical_processors.iter().take(256).enumerate() {
                let left = x + i as i32 % cols * (w + gap);
                let top = y + i as i32 / cols * pt.pxi(68.0);
                if top + pt.pxi(64.0) < 0 || top > r.bottom {
                    continue;
                }
                let key = format!("core:{}", cpu.id);
                let value = latest(s, &key);
                let value = if value.is_finite() {
                    format!("{value:.0}%")
                } else {
                    "—".into()
                };
                pt.label(
                    pt.fonts.mono_tiny,
                    pt.c.muted,
                    &format!("CPU {}", cpu.index),
                    rect(left, top, w, pt.pxi(18.0)),
                    DT_LEFT,
                );
                pt.label(
                    pt.fonts.mono_tiny,
                    pt.c.fg,
                    &value,
                    rect(left, top, w, pt.pxi(18.0)),
                    DT_RIGHT,
                );
                let points = points(s, &key);
                paint::chart_with(
                    (*s).dpi,
                    &(*s).fonts,
                    pt.dc,
                    rect(left, top + pt.pxi(19.0), w, pt.pxi(40.0)),
                    &points,
                    Some(100.0),
                    pt.c.fg,
                    paint::ChartStyle::Core,
                );
            }
        }
    }
    body_scroll::paint(s, &pt);
}
struct Chart {
    key: String,
    title: String,
    percent: bool,
    bytes: bool,
    ceiling: Option<f64>,
}
unsafe fn charts(s: *mut State) -> Vec<Chart> {
    let c = |key: &str, title: &str, percent, bytes| Chart {
        key: key.into(),
        title: title.into(),
        percent,
        bytes,
        ceiling: percent.then_some(100.0),
    };
    let cpu = || c("cpu", "CPU", true, false);
    let mem = || {
        c(
            "memory",
            tr("사용 중인 물리 메모리", "Used physical memory"),
            true,
            false,
        )
    };
    let disk = || {
        c(
            "disk",
            tr("물리 디스크 전송량", "Physical disk transfers"),
            false,
            true,
        )
    };
    let net = || c("network", tr("네트워크", "Network"), false, true);
    match (*s).tab {
        Tab::Overview => vec![cpu(), disk(), net(), mem()],
        Tab::Cpu => vec![
            cpu(),
            c(
                "service_hosts",
                tr("서비스 호스트 CPU", "Service host CPU"),
                true,
                false,
            ),
        ],
        Tab::Memory => vec![
            mem(),
            c("commit", tr("커밋 사용률", "Commit charge"), true, false),
            c(
                "faults",
                tr(
                    "관측된 프로세스 하드 폴트/초",
                    "Observed process hard faults/sec",
                ),
                false,
                false,
            ),
        ],
        Tab::Disk => {
            let mut v = vec![disk()];
            if let Some(perf) = &(*s).performance {
                for d in perf.disks.iter().take(32) {
                    v.push(c(
                        &format!("disk:{}", d.id),
                        &format!(
                            "{} · {}",
                            paint::disk_name(&d.id),
                            tr("활성 시간", "active time")
                        ),
                        true,
                        false,
                    ));
                }
            }
            v
        }
        Tab::Network => {
            let mut v = vec![
                net(),
                c("tcp", tr("TCP 연결", "TCP connections"), false, false),
            ];
            if let Some(perf) = &(*s).performance {
                for n in perf.networks.iter().filter(|n| n.connected).take(32) {
                    v.push(c(
                        &format!("nic:{}", n.id),
                        &format!("{} · {}", n.name, tr("링크 사용률", "link utilization")),
                        true,
                        false,
                    ));
                }
            }
            v
        }
    }
}
unsafe fn latest(s: *mut State, key: &str) -> f64 {
    (*s).traces
        .get(key)
        .and_then(|t| t.points.back())
        .map_or(f64::NAN, |p| p.values[0])
}
unsafe fn points(s: *mut State, key: &str) -> Vec<(Instant, f64)> {
    (*s).traces.get(key).map_or_else(Vec::new, |t| {
        t.points.iter().map(|p| (p.at, p.values[0])).collect()
    })
}
unsafe fn draw_chart(s: *mut State, pt: &Painter, c: &Chart, r: RECT, accent: bool) {
    let value = latest(s, &c.key);
    let text = if !value.is_finite() {
        "—".into()
    } else if c.percent {
        format!("{value:.1}%")
    } else if c.bytes {
        format!("{}/s", paint::human_bytes(value))
    } else {
        format!("{value:.1}")
    };
    let title = rect(r.left, r.top, r.right - r.left, pt.pxi(22.0));
    let value_width = pt.measure(pt.fonts.mono_cell, &text).cx;
    pt.label(
        pt.fonts.ui_strong,
        pt.c.fg,
        &c.title,
        RECT {
            right: (title.right - value_width - pt.pxi(8.0)).max(title.left),
            ..title
        },
        DT_LEFT,
    );
    pt.label(pt.fonts.mono_cell, pt.c.fg, &text, title, DT_RIGHT);
    let graph = rect(r.left, r.top + pt.pxi(24.0), r.right - r.left, pt.pxi(92.0));
    let points = points(s, &c.key);
    paint::chart_with(
        (*s).dpi,
        &(*s).fonts,
        pt.dc,
        graph,
        &points,
        c.ceiling,
        if accent { pt.c.accent } else { pt.c.fg },
        paint::ChartStyle::Main,
    );
    pt.label(
        pt.fonts.mono_tiny,
        pt.c.muted,
        tr("60초", "60 seconds"),
        rect(
            r.left,
            graph.bottom + pt.pxi(3.0),
            r.right - r.left,
            pt.pxi(16.0),
        ),
        DT_LEFT,
    );
    pt.label(
        pt.fonts.mono_tiny,
        pt.c.muted,
        if c.percent {
            "100%"
        } else {
            tr("자동", "Auto")
        },
        rect(
            r.left,
            graph.bottom + pt.pxi(3.0),
            r.right - r.left,
            pt.pxi(16.0),
        ),
        DT_RIGHT,
    );
}
unsafe fn physical(s: *mut State, pt: &Painter, r: RECT) {
    let memory = (*s).data.as_ref().and_then(|d| d.memory);
    let Some(memory) = memory else {
        pt.text(
            pt.fonts.small,
            pt.c.muted,
            tr(
                "물리 메모리 상세 정보를 수집하는 중입니다.",
                "Collecting physical memory details.",
            ),
            RECT {
                left: r.left + pt.pxi(14.0),
                right: r.right - pt.pxi(14.0),
                ..r
            },
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
        return;
    };
    let mut segments = vec![
        (
            tr("사용 중", "In use"),
            memory.total.saturating_sub(memory.available),
            pt.c.fg,
        ),
        (tr("사용 가능", "Available"), memory.available, pt.c.surface),
    ];
    if let Some(lists) = memory.lists {
        segments = vec![
            (
                tr("사용 중", "In use"),
                memory
                    .total
                    .saturating_sub(memory.available)
                    .saturating_sub(lists.modified),
                pt.c.fg,
            ),
            (tr("수정됨", "Modified"), lists.modified, pt.c.heat),
            (
                tr("대기", "Standby"),
                lists.standby,
                theme::mix(pt.c.surface, pt.c.fg, 0.16),
            ),
            (tr("여유", "Free"), lists.free, pt.c.surface),
        ];
    }
    let bar = rect(
        r.left + pt.pxi(14.0),
        r.top + pt.pxi(14.0),
        r.right - r.left - pt.pxi(28.0),
        pt.pxi(28.0),
    );
    let mut left = bar.left;
    for (_, value, color) in &segments {
        let w = ((bar.right - bar.left) as f64 * (*value as f64 / memory.total.max(1) as f64))
            .round() as i32;
        pt.fill(
            rect(
                left,
                bar.top,
                w.min(bar.right - left).max(0),
                bar.bottom - bar.top,
            ),
            *color,
        );
        left += w;
    }
    pt.canvas.stroke_round_rect(
        RectF::from_rect(bar),
        pt.px(4.0),
        pt.hair(),
        solid(pt.c.border),
    );
    let columns = if r.right - r.left < pt.pxi(550.0) {
        2
    } else {
        4
    };
    let width = (r.right - r.left - pt.pxi(28.0)) / columns;
    for (i, (name, value, color)) in segments.iter().enumerate() {
        let x = bar.left + i as i32 % columns * width;
        let y = bar.bottom + pt.pxi(14.0) + i as i32 / columns * pt.pxi(48.0);
        pt.canvas.bordered_round_rect(
            RectF::new(x as f32, y as f32 + pt.px(4.0), pt.px(10.0), pt.px(10.0)),
            Radii::all(pt.px(2.0)),
            pt.hair(),
            solid(*color),
            solid(pt.c.border),
        );
        pt.label(
            pt.fonts.small,
            pt.c.muted,
            name,
            rect(x + pt.pxi(16.0), y, width - pt.pxi(20.0), pt.pxi(18.0)),
            DT_LEFT,
        );
        pt.label(
            pt.fonts.mono_total,
            pt.c.fg,
            &paint::human_bytes(*value as f64),
            rect(x, y + pt.pxi(19.0), width - pt.pxi(8.0), pt.pxi(21.0)),
            DT_LEFT,
        );
    }
    let total = format!(
        "{} {}   ·   {} {}",
        tr("전체", "Total"),
        paint::human_bytes(memory.total as f64),
        tr("사용 가능", "Available"),
        paint::human_bytes(memory.available as f64)
    );
    pt.label(
        pt.fonts.mono_small,
        pt.c.muted,
        &total,
        rect(
            bar.left,
            r.bottom - pt.pxi(30.0),
            bar.right - bar.left,
            pt.pxi(22.0),
        ),
        DT_LEFT,
    );
}
pub(super) unsafe fn draw_button(s: *mut State, item: &DRAWITEMSTRUCT) {
    gfx::buffered_item(item.hDC, &item.rcItem, |dc| draw_button_to(s, item, dc));
}
unsafe fn draw_button_to(s: *mut State, item: &DRAWITEMSTRUCT, dc: HDC) {
    let pt = Painter::new(dc, (*s).dpi, &(*s).fonts);
    let id = item.CtlID as usize;
    let r = item.rcItem;
    let mut st = ButtonState {
        pressed: item.itemState & ODS_SELECTED != 0,
        focused: item.itemState & ODS_FOCUS != 0,
        disabled: item.itemState & ODS_DISABLED != 0,
        ..Default::default()
    };
    if (NAV..NAV + 5).contains(&id) || id == BACK {
        let index = if id == BACK { 5 } else { id - NAV };
        st.selected = index < 5 && Tab::ALL[index] == (*s).tab;
        let label = if r.right - r.left < pt.pxi(80.0) {
            ""
        } else if index < 5 {
            Tab::ALL[index].title()
        } else {
            tr("작업 관리자 열기", "Open Task Manager")
        };
        let count = match index {
            1 => Some(latest(s, "cpu")),
            2 => Some(latest(s, "memory")),
            _ => None,
        }
        .map(|n| {
            if n.is_finite() {
                format!("{n:.0}%")
            } else {
                "—".into()
            }
        });
        widgets::nav_item(
            &pt,
            r,
            &st,
            label,
            if label.is_empty() {
                None
            } else {
                count.as_deref()
            },
            pt.c.bg,
            |pt, x, y, size, color| nav_icon(pt, index, x, y, size, color),
        );
    } else if (HEAD..HEAD + (*s).panels.len()).contains(&id) {
        let panel = &(&(*s).panels)[id - HEAD];
        let collapsed = (*s).collapsed.contains(&((*s).tab, panel.kind));
        pt.fill(r, pt.c.surface);
        pt.canvas.bordered_round_rect(
            RectF::from_rect(r),
            if collapsed {
                Radii::all(pt.px(8.0))
            } else {
                Radii::top(pt.px(8.0))
            },
            pt.hair(),
            solid(pt.c.bg),
            solid(pt.c.border),
        );
        widgets::chevron_button(
            &pt,
            rect(pt.pxi(8.0), pt.pxi(12.0), pt.pxi(20.0), pt.pxi(20.0)),
            if collapsed { 0.0 } else { 90.0 },
            0.0,
        );
        let title = format!(
            "{}{}",
            panel.kind.title(),
            if panel.rows.is_empty() {
                String::new()
            } else {
                format!(" ({})", panel.rows.len())
            }
        );
        let title_width = pt.measure(pt.fonts.ui_strong, &title).cx;
        pt.label(
            pt.fonts.ui_strong,
            pt.c.fg,
            &title,
            rect(
                pt.pxi(38.0),
                0,
                title_width.min(r.right - pt.pxi(50.0)),
                r.bottom,
            ),
            DT_LEFT,
        );
        let left = pt.pxi(50.0) + title_width;
        if r.right - left > pt.pxi(90.0) {
            pt.label(
                pt.fonts.mono_tiny,
                pt.c.muted,
                &panel.summary,
                rect(left, 0, r.right - left - pt.pxi(14.0), r.bottom),
                DT_RIGHT,
            );
        }
        if st.focused {
            widgets::focus_ring_inset(&pt, r, pt.px(4.0));
        }
    } else if id == RATE {
        widgets::select_face(
            &pt,
            r,
            rate_label((*s).interval),
            &widgets::FieldState {
                focused: st.focused,
                disabled: st.disabled,
                ..Default::default()
            },
            Some(pt.fonts.mono_small),
            pt.c.bg,
        );
    } else {
        let label = paint::window_text(item.hwndItem);
        widgets::button_face(
            &pt,
            r,
            if id == END {
                ButtonStyle::Primary
            } else {
                ButtonStyle::Default
            },
            &st,
            &label,
            pt.c.surface,
        );
    }
}
fn nav_icon(pt: &Painter, index: usize, x: f32, y: f32, size: f32, color: u32) {
    let c = &pt.canvas;
    let f = |v: f32| v * size / 16.0;
    let line = |x1: f32, y1: f32, x2: f32, y2: f32| {
        c.line(
            x + f(x1),
            y + f(y1),
            x + f(x2),
            y + f(y2),
            pt.px(1.4),
            solid(color),
        )
    };
    match index {
        0 => {
            for (a, b) in [(2.0, 2.0), (9.0, 2.0), (2.0, 9.0), (9.0, 9.0)] {
                c.stroke_round_rect(
                    RectF::new(x + f(a), y + f(b), f(5.0), f(5.0)),
                    f(1.0),
                    pt.px(1.4),
                    solid(color),
                );
            }
        }
        1 => {
            c.stroke_round_rect(
                RectF::new(x + f(4.0), y + f(4.0), f(8.0), f(8.0)),
                f(1.0),
                pt.px(1.4),
                solid(color),
            );
            for p in [6.5, 9.5] {
                line(p, 1.5, p, 4.0);
                line(p, 12.0, p, 14.5);
                line(1.5, p, 4.0, p);
                line(12.0, p, 14.5, p);
            }
        }
        2 => {
            c.stroke_round_rect(
                RectF::new(x + f(1.5), y + f(4.5), f(13.0), f(7.0)),
                f(1.0),
                pt.px(1.4),
                solid(color),
            );
            for p in [4.5, 7.0, 9.5, 12.0] {
                line(p, 7.0, p, 9.0);
            }
        }
        3 => {
            c.stroke_round_rect(
                RectF::new(x + f(2.5), y + f(2.0), f(11.0), f(12.0)),
                f(2.0),
                pt.px(1.4),
                solid(color),
            );
            line(2.5, 6.0, 13.5, 6.0);
            line(2.5, 10.0, 13.5, 10.0);
        }
        4 => {
            c.stroke_circle(x + f(8.0), y + f(8.0), f(6.0), pt.px(1.4), solid(color));
            line(2.0, 8.0, 14.0, 8.0);
            line(8.0, 2.0, 8.0, 14.0);
        }
        _ => widgets::icon(pt, widgets::Icon::Processes, x, y, size, color),
    }
}
