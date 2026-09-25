//! Shared visual tokens and lightweight GDI rendering. No animation/render loop.
use super::*;

unsafe fn rect(p: *mut App, x: i32, y: i32, w: i32, h: i32) -> RECT {
    RECT {
        left: scale(p, x),
        top: scale(p, y),
        right: scale(p, x + w),
        bottom: scale(p, y + h),
    }
}
unsafe fn fill(dc: HDC, r: RECT, color: u32) {
    let b = CreateSolidBrush(color);
    FillRect(dc, &r, b);
    DeleteObject(b);
}
unsafe fn rounded(dc: HDC, r: RECT, fill_color: u32, border: u32, radius: i32) {
    let b = CreateSolidBrush(fill_color);
    let pen = CreatePen(PS_SOLID, 1, border);
    let ob = SelectObject(dc, b);
    let op = SelectObject(dc, pen);
    RoundRect(dc, r.left, r.top, r.right, r.bottom, radius, radius);
    SelectObject(dc, op);
    SelectObject(dc, ob);
    DeleteObject(pen);
    DeleteObject(b);
}
unsafe fn label(dc: HDC, font: HFONT, color: u32, value: &str, mut r: RECT, flags: u32) {
    let old = SelectObject(dc, font);
    SetTextColor(dc, color);
    SetBkMode(dc, TRANSPARENT as i32);
    DrawTextW(dc, wide(value).as_ptr(), -1, &mut r, DT_NOPREFIX | flags);
    SelectObject(dc, old);
}
unsafe fn line(dc: HDC, points: &[POINT], color: u32, width: i32) {
    if points.len() < 2 {
        return;
    }
    let pen = CreatePen(PS_SOLID, width, color);
    let old = SelectObject(dc, pen);
    Polyline(dc, points.as_ptr(), points.len() as i32);
    SelectObject(dc, old);
    DeleteObject(pen);
}
unsafe fn circle(dc: HDC, x: i32, y: i32, radius: i32, color: u32) {
    let b = CreateSolidBrush(color);
    let old = SelectObject(dc, b);
    let pen = SelectObject(dc, GetStockObject(NULL_PEN));
    Ellipse(dc, x - radius, y - radius, x + radius, y + radius);
    SelectObject(dc, pen);
    SelectObject(dc, old);
    DeleteObject(b);
}
fn human_bytes(value: f64) -> String {
    if !value.is_finite() {
        return "—".into();
    }
    if value >= 1073741824.0 {
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
        format!("{days}일 {hours}시간")
    } else {
        format!("{hours}시간 {mins}분")
    }
}

pub(super) unsafe fn paint(p: *mut App) {
    let mut ps: PAINTSTRUCT = zeroed();
    let dc = BeginPaint((*p).hwnd, &mut ps);
    // Parent background is inexpensive and clipping excludes child controls.
    paint_to(p, dc);
    EndPaint((*p).hwnd, &ps);
}
pub(super) unsafe fn paint_to(p: *mut App, dc: HDC) {
    let mut bounds: RECT = zeroed();
    GetClientRect((*p).hwnd, &mut bounds);
    let s = |v| scale(p, v);
    let width = bounds.right;
    let height = bounds.bottom;
    let left = s(SIDEBAR + 24);
    fill(dc, bounds, BG);
    fill(
        dc,
        RECT {
            left: 0,
            top: 0,
            right: s(SIDEBAR),
            bottom: height,
        },
        RAIL,
    );

    (*p).icons
        .draw(dc, 4, s(20), s(28), s(40), rgb(147, 190, 255));
    label(
        dc,
        (*p).bold,
        SURFACE,
        "Feather",
        rect(p, 70, 30, 114, 26),
        DT_SINGLELINE | DT_VCENTER,
    );
    label(
        dc,
        (*p).small,
        rgb(163, 180, 206),
        "Task Manager",
        rect(p, 70, 56, 114, 18),
        DT_SINGLELINE,
    );
    label(
        dc,
        (*p).small,
        rgb(155, 173, 200),
        "내 컴퓨터",
        rect(p, 24, 100, 145, 18),
        DT_SINGLELINE,
    );
    line(
        dc,
        &[
            POINT {
                x: s(20),
                y: s(350),
            },
            POINT {
                x: s(176),
                y: s(350),
            },
        ],
        rgb(47, 62, 85),
        1,
    );
    label(
        dc,
        (*p).small,
        rgb(176, 191, 213),
        "바로 가기",
        rect(p, 24, 369, 140, 20),
        DT_SINGLELINE,
    );
    for (i, value) in [
        "Ctrl+1…4    화면 이동",
        "Ctrl+F          검색",
        "F5                 새로고침",
    ]
    .iter()
    .enumerate()
    {
        label(
            dc,
            (*p).small,
            rgb(151, 170, 198),
            value,
            rect(p, 24, 399 + i as i32 * 26, 156, 20),
            DT_SINGLELINE,
        );
    }
    circle(dc, s(25), height - s(28), s(3), rgb(80, 203, 154));
    label(
        dc,
        (*p).small,
        rgb(176, 191, 213),
        concat!("로컬 모니터링   ·   v", env!("CARGO_PKG_VERSION")),
        RECT {
            left: s(37),
            top: height - s(37),
            right: s(188),
            bottom: height - s(15),
        },
        DT_SINGLELINE | DT_VCENTER,
    );

    label(
        dc,
        (*p).heading,
        INK,
        (*p).page.title(),
        RECT {
            left,
            top: s(26),
            right: width - s(190),
            bottom: s(66),
        },
        DT_SINGLELINE | DT_VCENTER,
    );
    label(
        dc,
        (*p).font,
        MUTED,
        (*p).page.subtitle(),
        RECT {
            left,
            top: s(73),
            right: width - s(24),
            bottom: s(97),
        },
        DT_SINGLELINE | DT_END_ELLIPSIS,
    );
    let stale = (*p).last_sample.is_some_and(|t| {
        t.elapsed() > Duration::from_millis((*p).interval.saturating_mul(3).max(5000))
    });
    let (status, color, bg) = if (*p).error.is_some() {
        ("확인 필요", DANGER, rgb(255, 236, 238))
    } else if (*p).page == Page::Performance
        && ((*p).performance_error.is_some()
            || (*p)
                .performance
                .as_ref()
                .is_some_and(|v| !v.warnings.is_empty()))
    {
        ("일부 확인 필요", rgb(149, 96, 0), rgb(255, 245, 219))
    } else if (*p).paused {
        ("일시정지", MUTED, rgb(230, 235, 243))
    } else if (*p).busy {
        ("처리 중", BLUE, SELECTED)
    } else if ((*p).page == Page::Startup && (*p).startup_loading)
        || ((*p).page == Page::Services && (*p).services_loading)
    {
        ("불러오는 중", MUTED, rgb(230, 235, 243))
    } else if (*p).page == Page::Startup && (*p).startup_loaded {
        ("수동 갱신", MUTED, rgb(230, 235, 243))
    } else if (*p).page == Page::Performance && (*p).performance.is_none() {
        ("연결 중", BLUE, SELECTED)
    } else if stale {
        ("업데이트 대기", rgb(149, 96, 0), rgb(255, 245, 219))
    } else if (*p).snapshot.is_none() {
        ("불러오는 중", MUTED, rgb(230, 235, 243))
    } else {
        ("실시간", GREEN, rgb(226, 245, 235))
    };
    let badge = RECT {
        left: width - s(142),
        top: s(34),
        right: width - s(24),
        bottom: s(62),
    };
    rounded(dc, badge, bg, bg, s(24));
    circle(dc, badge.left + s(13), badge.top + s(14), s(3), color);
    label(
        dc,
        (*p).small,
        color,
        status,
        RECT {
            left: badge.left + s(23),
            ..badge
        },
        DT_SINGLELINE | DT_VCENTER,
    );
    summary_cards(p, dc, left, width - s(24));
    label(
        dc,
        (*p).small,
        MUTED,
        "갱신 간격",
        RECT {
            left: width - s(280),
            top: s(184),
            right: width - s(204),
            bottom: s(204),
        },
        DT_SINGLELINE,
    );

    if (*p).page == Page::Performance {
        let details = (*p)
            .performance
            .as_ref()
            .map(|perf| {
                format!(
                    "{}  ·  논리 CPU {}개  ·  가동 {}",
                    perf.cpu_name,
                    perf.logical_cpus,
                    uptime(perf.uptime_seconds)
                )
            })
            .unwrap_or_else(|| "성능 카운터를 연결하고 있습니다…".into());
        label(
            dc,
            (*p).small,
            MUTED,
            &details,
            RECT {
                left,
                top: s(208),
                right: width - s(299),
                bottom: s(246),
            },
            DT_WORDBREAK | DT_END_ELLIPSIS,
        );
        performance_panels(p, dc, left, width - s(24), s(267), height - s(52));
    } else {
        label(
            dc,
            (*p).small,
            MUTED,
            "검색",
            RECT {
                left,
                top: s(184),
                right: left + s(140),
                bottom: s(204),
            },
            DT_SINGLELINE,
        );
        let search = RECT {
            left,
            top: s(209),
            right: width - s(304),
            bottom: s(245),
        };
        rounded(
            dc,
            search,
            SURFACE,
            if GetFocus() == (*p).search {
                BLUE
            } else {
                BORDER
            },
            s(10),
        );
        let cx = left + s(17);
        let cy = s(225);
        let pen = CreatePen(PS_SOLID, s(1).max(1), MUTED);
        let op = SelectObject(dc, pen);
        let ob = SelectObject(dc, GetStockObject(NULL_BRUSH));
        Ellipse(dc, cx - s(5), cy - s(5), cx + s(5), cy + s(5));
        SelectObject(dc, ob);
        SelectObject(dc, op);
        DeleteObject(pen);
        line(
            dc,
            &[
                POINT {
                    x: cx + s(4),
                    y: cy + s(4),
                },
                POINT {
                    x: cx + s(8),
                    y: cy + s(8),
                },
            ],
            MUTED,
            s(1).max(1),
        );
        let loading = match (*p).page {
            Page::Startup => (*p).startup_loading,
            Page::Services => (*p).services_loading,
            _ => false,
        };
        let count = if loading {
            "목록을 불러오는 중…".into()
        } else {
            format!("{}개 표시 · 전체 {}개", (*p).rows.len(), total_rows(p))
        };
        label(
            dc,
            (*p).small,
            MUTED,
            &count,
            RECT {
                left,
                top: s(248),
                right: width - s(24),
                bottom: s(266),
            },
            DT_SINGLELINE,
        );
        let detail = selected_detail(p);
        label(
            dc,
            (*p).font,
            INK,
            &detail,
            RECT {
                left,
                top: height - s(80),
                right: width - s(316),
                bottom: height - s(38),
            },
            DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
    let status_text = if let Some(error) = &(*p).error {
        format!("확인 필요: {error}")
    } else if !(&(*p).notice).is_empty() {
        (*p).notice.clone()
    } else if (*p).page == Page::Performance {
        if let Some(error) = &(*p).performance_error {
            format!("성능 정보: {error}")
        } else if let Some(perf) = &(*p).performance {
            if !perf.warnings.is_empty() {
                perf.warnings.join(" · ")
            } else {
                "디스크: 가장 바쁜 장치 · GPU: 가장 바쁜 엔진 · 네트워크: 활성 물리 어댑터 합계"
                    .into()
            }
        } else {
            "성능 정보는 이 화면을 보고 있을 때만 수집합니다.".into()
        }
    } else if (*p).page == Page::Startup {
        "자동 시작 항목: Run 레지스트리 · 시작프로그램 폴더".into()
    } else if (*p).page == Page::Services {
        "시작·중지 요청 후 상태가 바뀌는 데 시간이 걸릴 수 있습니다. 서비스 목록은 5초마다 갱신합니다.".into()
    } else {
        format!(
            "{}  ·  수집 {:.1} ms  ·  최소화하면 자동으로 수집을 멈춥니다.",
            if (*p).paused {
                "일시정지".into()
            } else {
                format!("{:.1}초마다 갱신", (*p).interval as f64 / 1000.0)
            },
            (*p).snapshot.as_ref().map_or(0.0, |s| s.sample_ms)
        )
    };
    line(
        dc,
        &[
            POINT {
                x: left,
                y: height - s(33),
            },
            POINT {
                x: width - s(24),
                y: height - s(33),
            },
        ],
        BORDER,
        1,
    );
    label(
        dc,
        (*p).small,
        if (*p).error.is_some() { DANGER } else { MUTED },
        &status_text,
        RECT {
            left,
            top: height - s(28),
            right: width - s(24),
            bottom: height - s(8),
        },
        DT_SINGLELINE | DT_END_ELLIPSIS,
    );
}

unsafe fn summary_cards(p: *mut App, dc: HDC, left: i32, right: i32) {
    let snap = (*p).snapshot.as_ref();
    let perf = (*p).performance.as_ref();
    let cpu = snap
        .map(|s| format!("{:.1}%", s.cpu_percent))
        .unwrap_or_else(|| "—".into());
    let mem = snap
        .map(|s| human_bytes(s.memory_used as f64))
        .unwrap_or_else(|| "—".into());
    let cards: Vec<(&str, String, String, u32)> = match (*p).page {
        Page::Processes => vec![
            ("CPU 사용량", cpu, "전체 프로세서 기준".into(), BLUE),
            (
                "물리 메모리",
                mem,
                snap.map(|s| format!("전체 {}", human_bytes(s.memory_total as f64)))
                    .unwrap_or_default(),
                rgb(115, 79, 198),
            ),
            (
                "실행 중인 프로세스",
                snap.map(|s| s.processes.len().to_string())
                    .unwrap_or_else(|| "—".into()),
                "앱과 백그라운드 작업".into(),
                rgb(15, 120, 112),
            ),
        ],
        Page::Performance => vec![
            ("CPU", cpu, "전체 프로세서".into(), BLUE),
            (
                "메모리",
                mem,
                snap.map(|s| format!("전체 {}", human_bytes(s.memory_total as f64)))
                    .unwrap_or_default(),
                rgb(115, 79, 198),
            ),
            (
                "디스크 활성 시간",
                perf.and_then(|s| s.disk_active_percent)
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "—".into()),
                "가장 바쁜 장치".into(),
                rgb(15, 120, 112),
            ),
            (
                "GPU 엔진",
                perf.and_then(|s| s.gpu_percent)
                    .map(|v| format!("{v:.1}%"))
                    .unwrap_or_else(|| "—".into()),
                "가장 바쁜 엔진".into(),
                rgb(166, 88, 10),
            ),
        ],
        Page::Startup => {
            let all = (*p).startup.len();
            let on = (*p).startup.iter().filter(|e| e.enabled).count();
            let managed = (*p).startup.iter().filter(|e| e.manageable).count();
            vec![
                (
                    "시작 앱",
                    if (*p).startup_loaded {
                        all.to_string()
                    } else {
                        "—".into()
                    },
                    "등록된 자동 실행 항목".into(),
                    BLUE,
                ),
                (
                    "사용",
                    if (*p).startup_loaded {
                        on.to_string()
                    } else {
                        "—".into()
                    },
                    "로그인 시 자동 실행".into(),
                    GREEN,
                ),
                (
                    "변경 가능한 항목",
                    if (*p).startup_loaded {
                        managed.to_string()
                    } else {
                        "—".into()
                    },
                    "지원되는 상태의 항목".into(),
                    rgb(115, 79, 198),
                ),
            ]
        }
        Page::Services => {
            let all = (*p).services.len();
            let running = (*p).services.iter().filter(|s| s.state == 4).count();
            let stopped = (*p).services.iter().filter(|s| s.state == 1).count();
            vec![
                (
                    "등록된 서비스",
                    if (*p).services_loaded {
                        all.to_string()
                    } else {
                        "—".into()
                    },
                    "Windows 서비스".into(),
                    BLUE,
                ),
                (
                    "실행 중",
                    if (*p).services_loaded {
                        running.to_string()
                    } else {
                        "—".into()
                    },
                    "백그라운드에서 동작".into(),
                    GREEN,
                ),
                (
                    "중지됨",
                    if (*p).services_loaded {
                        stopped.to_string()
                    } else {
                        "—".into()
                    },
                    "현재 실행하지 않음".into(),
                    MUTED,
                ),
            ]
        }
    };
    let gap = scale(p, 12);
    let width = (right - left - gap * (cards.len() as i32 - 1)) / cards.len() as i32;
    for (i, (title, value, hint, color)) in cards.iter().enumerate() {
        let x = left + i as i32 * (width + gap);
        let r = RECT {
            left: x,
            top: scale(p, 110),
            right: x + width,
            bottom: scale(p, 178),
        };
        rounded(dc, r, SURFACE, BORDER, scale(p, 12));
        label(
            dc,
            (*p).small,
            MUTED,
            title,
            RECT {
                left: x + scale(p, 16),
                top: scale(p, 119),
                right: x + width - scale(p, 12),
                bottom: scale(p, 139),
            },
            DT_SINGLELINE,
        );
        label(
            dc,
            (*p).metric,
            *color,
            value,
            RECT {
                left: x + scale(p, 16),
                top: scale(p, 139),
                right: x + width - scale(p, 12),
                bottom: scale(p, 173),
            },
            DT_SINGLELINE | DT_VCENTER,
        );
        // Secondary captions are omitted if the complete label cannot fit.
        let mut value_size: SIZE = zeroed();
        let mut hint_size: SIZE = zeroed();
        let value_text = wide(value);
        let hint_text = wide(hint);
        let old = SelectObject(dc, (*p).metric);
        GetTextExtentPoint32W(
            dc,
            value_text.as_ptr(),
            value_text.len() as i32 - 1,
            &mut value_size,
        );
        SelectObject(dc, (*p).small);
        GetTextExtentPoint32W(
            dc,
            hint_text.as_ptr(),
            hint_text.len() as i32 - 1,
            &mut hint_size,
        );
        SelectObject(dc, old);
        if value_size.cx + hint_size.cx + scale(p, 48) <= width {
            label(
                dc,
                (*p).small,
                MUTED,
                hint,
                RECT {
                    left: x + width - hint_size.cx - scale(p, 16),
                    top: scale(p, 146),
                    right: x + width - scale(p, 14),
                    bottom: scale(p, 166),
                },
                DT_RIGHT | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
    }
}

unsafe fn performance_panels(p: *mut App, dc: HDC, left: i32, right: i32, top: i32, bottom: i32) {
    let gap = scale(p, 16);
    let w = (right - left - gap) / 2;
    let h = (bottom - top - gap) / 2;
    let snap = (*p).snapshot.as_ref();
    let perf = (*p).performance.as_ref();
    let cpu = snap
        .map(|s| format!("{:.1}%", s.cpu_percent))
        .unwrap_or_else(|| "—".into());
    let memory = snap
        .map(|s| {
            format!(
                "{} / {}",
                human_bytes(s.memory_used as f64),
                human_bytes(s.memory_total as f64)
            )
        })
        .unwrap_or_else(|| "—".into());
    let disk = perf
        .filter(|s| s.disk_rates_ready)
        .map(|s| {
            format!(
                "읽기 {}  ·  쓰기 {} /s",
                human_bytes(s.disk_read_bytes_per_sec),
                human_bytes(s.disk_write_bytes_per_sec)
            )
        })
        .unwrap_or_else(|| "측정 대기 · 카운터 상태 확인 중".into());
    let network = perf
        .filter(|s| s.network_rates_ready)
        .map(|s| {
            format!(
                "수신 {}  ·  송신 {} /s",
                human_bytes(s.network_rx_bytes_per_sec),
                human_bytes(s.network_tx_bytes_per_sec)
            )
        })
        .unwrap_or_else(|| "측정 대기 · 어댑터 상태 확인 중".into());
    let specs = [
        ("CPU", cpu, "전체 CPU 사용률", BLUE),
        ("메모리", memory, "물리 메모리 사용률", rgb(115, 79, 198)),
        ("디스크", disk, "읽기 + 쓰기 처리량", rgb(15, 120, 112)),
        ("네트워크", network, "수신 + 송신 처리량", rgb(166, 88, 10)),
    ];
    for (i, (title, value, hint, color)) in specs.iter().enumerate() {
        let x = left + (i % 2) as i32 * (w + gap);
        let y = top + (i / 2) as i32 * (h + gap);
        let r = RECT {
            left: x,
            top: y,
            right: x + w,
            bottom: y + h,
        };
        rounded(dc, r, SURFACE, BORDER, scale(p, 14));
        label(
            dc,
            (*p).bold,
            INK,
            title,
            RECT {
                left: x + scale(p, 18),
                top: y + scale(p, 13),
                right: x + w - scale(p, 18),
                bottom: y + scale(p, 37),
            },
            DT_SINGLELINE,
        );
        label(
            dc,
            (*p).small,
            MUTED,
            "최근 60초",
            RECT {
                left: x + w - scale(p, 100),
                top: y + scale(p, 14),
                right: x + w - scale(p, 18),
                bottom: y + scale(p, 34),
            },
            DT_RIGHT | DT_SINGLELINE,
        );
        let compact = h < scale(p, 180);
        let value_y = if compact { 37 } else { 44 };
        let pair = perf.and_then(|s| match i {
            2 if s.disk_rates_ready => Some((
                format!("읽기 {}/s", human_bytes(s.disk_read_bytes_per_sec)),
                format!("쓰기 {}/s", human_bytes(s.disk_write_bytes_per_sec)),
            )),
            3 if s.network_rates_ready => Some((
                format!("수신 {}/s", human_bytes(s.network_rx_bytes_per_sec)),
                format!("송신 {}/s", human_bytes(s.network_tx_bytes_per_sec)),
            )),
            _ => None,
        });
        if let Some((first, second)) = pair {
            label(
                dc,
                (*p).font,
                *color,
                &first,
                RECT {
                    left: x + scale(p, 18),
                    top: y + scale(p, value_y),
                    right: x + w / 2 - scale(p, 4),
                    bottom: y + scale(p, value_y + 24),
                },
                DT_SINGLELINE,
            );
            label(
                dc,
                (*p).font,
                *color,
                &second,
                RECT {
                    left: x + w / 2 + scale(p, 4),
                    top: y + scale(p, value_y),
                    right: x + w - scale(p, 18),
                    bottom: y + scale(p, value_y + 24),
                },
                DT_SINGLELINE,
            );
        } else {
            label(
                dc,
                if i < 2 { (*p).bold } else { (*p).font },
                *color,
                value,
                RECT {
                    left: x + scale(p, 18),
                    top: y + scale(p, value_y),
                    right: x + w - scale(p, 18),
                    bottom: y + scale(p, value_y + 24),
                },
                DT_SINGLELINE | DT_END_ELLIPSIS,
            );
        }
        let chart = RECT {
            left: x + scale(p, 18),
            top: y + scale(p, if compact { 64 } else { 80 }),
            right: x + w - scale(p, 18),
            bottom: y + h - scale(p, if compact { 27 } else { 34 }),
        };
        graph(p, dc, chart, i, *color);
        label(
            dc,
            (*p).small,
            MUTED,
            hint,
            RECT {
                left: x + scale(p, 18),
                top: y + h - scale(p, 26),
                right: x + w - scale(p, 18),
                bottom: y + h - scale(p, 8),
            },
            DT_SINGLELINE,
        );
    }
}
unsafe fn graph(p: *mut App, dc: HDC, r: RECT, kind: usize, color: u32) {
    if r.bottom <= r.top {
        return;
    }
    for i in 0..=4 {
        let y = r.top + (r.bottom - r.top) * i / 4;
        line(
            dc,
            &[POINT { x: r.left, y }, POINT { x: r.right, y }],
            rgb(232, 237, 244),
            1,
        );
    }
    for i in 0..=6 {
        let x = r.left + (r.right - r.left) * i / 6;
        line(
            dc,
            &[POINT { x, y: r.top }, POINT { x, y: r.bottom }],
            rgb(239, 242, 247),
            1,
        );
    }
    let value = |p: &HistoryPoint| match kind {
        0 => p.cpu,
        1 => p.memory,
        2 => p.disk,
        _ => p.network,
    };
    let max = if kind < 2 {
        100.0
    } else {
        (*p).history
            .iter()
            .map(value)
            .filter(|v| v.is_finite())
            .fold(1024.0, f64::max)
            * 1.15
    };
    let end = (*p)
        .history
        .back()
        .map(|h| h.at)
        .unwrap_or_else(Instant::now);
    let mut points = Vec::new();
    for point in &(*p).history {
        let v = value(point);
        let age = end.saturating_duration_since(point.at).as_secs_f64();
        if age > 60.0 {
            continue;
        }
        if !v.is_finite() {
            line(dc, &points, color, scale(p, 2).max(1));
            points.clear();
            continue;
        }
        points.push(POINT {
            x: r.right - ((r.right - r.left) as f64 * age / 60.0) as i32,
            y: r.bottom - ((r.bottom - r.top) as f64 * (v / max).clamp(0.0, 1.0)) as i32,
        });
    }
    line(dc, &points, color, scale(p, 2).max(1));
    if let Some(last) = points.last() {
        circle(dc, last.x, last.y, scale(p, 3), color);
    }
    if (*p).history.len() < 2 {
        label(
            dc,
            (*p).small,
            MUTED,
            "데이터를 수집하고 있습니다",
            r,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
    let max_label = if kind < 2 {
        "100%".into()
    } else {
        format!("{} /s", human_bytes(max))
    };
    label(
        dc,
        (*p).small,
        MUTED,
        &max_label,
        RECT {
            top: r.top + scale(p, 2),
            bottom: r.top + scale(p, 20),
            right: r.right - scale(p, 3),
            ..r
        },
        DT_RIGHT | DT_SINGLELINE,
    );
}

pub(super) unsafe fn draw_button(p: *mut App, item: &DRAWITEMSTRUCT) {
    if item.CtlType != ODT_BUTTON {
        return;
    }
    let dc = item.hDC;
    let r = item.rcItem;
    let id = item.CtlID as usize;
    let pressed = item.itemState & ODS_SELECTED != 0;
    let disabled = item.itemState & ODS_DISABLED != 0;
    let focus = item.itemState & ODS_FOCUS != 0;
    let hover = (*p).hover == id;
    let nav = (NAV..NAV + 4).contains(&id);
    let rail = nav || id == TOP || id == SETTINGS;
    let selected = nav && id - NAV == (*p).page as usize || id == TOP && (*p).topmost;
    let primary = id == PRIMARY;
    let dangerous = primary && (*p).page == Page::Processes;
    let bgcolor = if rail {
        if selected {
            rgb(38, 61, 99)
        } else if pressed {
            rgb(44, 65, 98)
        } else if hover {
            rgb(31, 47, 72)
        } else {
            RAIL
        }
    } else if disabled {
        rgb(233, 237, 244)
    } else if primary {
        if dangerous {
            if pressed {
                rgb(143, 26, 39)
            } else {
                DANGER
            }
        } else if pressed {
            rgb(27, 76, 188)
        } else {
            BLUE
        }
    } else if pressed {
        rgb(221, 229, 242)
    } else if hover {
        rgb(235, 241, 251)
    } else {
        SURFACE
    };
    let fg = if disabled {
        rgb(127, 140, 160)
    } else if primary || selected {
        SURFACE
    } else if rail {
        rgb(186, 201, 223)
    } else {
        INK
    };
    fill(dc, r, if rail { RAIL } else { BG });
    rounded(
        dc,
        r,
        bgcolor,
        if rail || primary || disabled {
            bgcolor
        } else {
            BORDER
        },
        scale(p, 10),
    );
    if nav && selected {
        fill(
            dc,
            RECT {
                left: r.left + scale(p, 1),
                top: r.top + scale(p, 12),
                right: r.left + scale(p, 4),
                bottom: r.bottom - scale(p, 12),
            },
            rgb(116, 166, 255),
        );
    }
    let mut buf = [0u16; 128];
    let length = GetWindowTextW(item.hwndItem, buf.as_mut_ptr(), 128);
    let name = String::from_utf16_lossy(&buf[..length.max(0) as usize]);
    let text_rect = if nav {
        let icon_size = scale(p, 24);
        (*p).icons.draw(
            dc,
            id - NAV,
            r.left + scale(p, 17),
            r.top + ((r.bottom - r.top) - icon_size) / 2,
            icon_size,
            fg,
        );
        RECT {
            left: r.left + scale(p, 49),
            ..r
        }
    } else {
        r
    };
    label(
        dc,
        if selected || primary {
            (*p).bold
        } else {
            (*p).font
        },
        fg,
        &name,
        text_rect,
        DT_SINGLELINE | DT_VCENTER | if nav { DT_LEFT } else { DT_CENTER },
    );
    if focus {
        let inset = scale(p, 3);
        let fr = RECT {
            left: r.left + inset,
            top: r.top + inset,
            right: r.right - inset,
            bottom: r.bottom - inset,
        };
        let pen = CreatePen(
            PS_SOLID,
            scale(p, 1).max(1),
            if rail || primary { SURFACE } else { BLUE },
        );
        let old = SelectObject(dc, pen);
        let brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
        RoundRect(
            dc,
            fr.left,
            fr.top,
            fr.right,
            fr.bottom,
            scale(p, 7),
            scale(p, 7),
        );
        SelectObject(dc, brush);
        SelectObject(dc, old);
        DeleteObject(pen);
    }
}
