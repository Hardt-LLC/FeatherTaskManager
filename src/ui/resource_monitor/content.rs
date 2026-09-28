use super::*;

fn col(label: &str, width: f32, right: bool) -> table::Column {
    table::Column {
        label: label.into(),
        width,
        flex: width == 0.0,
        right,
    }
}
pub(super) fn columns(kind: Kind) -> Vec<table::Column> {
    let image = || col(tr("이미지", "Image"), 150.0, false);
    let pid = || col("PID", 64.0, true);
    let mut cols = match kind {
        Kind::Cpu => vec![
            image(),
            pid(),
            col(tr("상태", "Status"), 110.0, false),
            col(tr("스레드", "Threads"), 76.0, true),
            col("CPU", 78.0, true),
            col(tr("평균 CPU", "Avg CPU"), 96.0, true),
        ],
        Kind::Memory => vec![
            image(),
            pid(),
            col(tr("하드 폴트/초", "Hard faults/sec"), 110.0, true),
            col(tr("커밋", "Commit"), 100.0, true),
            col(tr("작업 집합", "Working set"), 100.0, true),
            col(tr("공유 가능", "Shareable"), 100.0, true),
            col(tr("개인", "Private"), 100.0, true),
        ],
        Kind::Io => vec![
            image(),
            pid(),
            col(tr("읽기", "Read"), 110.0, true),
            col(tr("쓰기", "Write"), 110.0, true),
            col(tr("합계", "Total"), 110.0, true),
        ],
        Kind::Network => vec![
            image(),
            pid(),
            col(tr("보내기", "Send"), 110.0, true),
            col(tr("받기", "Receive"), 110.0, true),
            col(tr("합계", "Total"), 110.0, true),
        ],
        Kind::Services => vec![
            col(tr("이름", "Name"), 140.0, false),
            pid(),
            col(tr("표시 이름", "Display name"), 0.0, false),
            col(tr("상태", "Status"), 96.0, false),
            col(tr("호스트 CPU", "Host CPU"), 100.0, true),
        ],
        Kind::Handles => vec![
            image(),
            pid(),
            col(tr("형식", "Type"), 90.0, false),
            col(tr("핸들 이름", "Handle name"), 0.0, false),
        ],
        Kind::Modules => vec![
            image(),
            pid(),
            col(tr("모듈 이름", "Module name"), 170.0, false),
            col(tr("전체 경로", "Full path"), 0.0, false),
            col(tr("이미지 크기", "Image size"), 100.0, true),
        ],
        Kind::Files => vec![
            image(),
            pid(),
            col(tr("파일", "File"), 0.0, false),
            col(tr("읽기", "Read"), 96.0, true),
            col(tr("쓰기", "Write"), 96.0, true),
            col(tr("합계", "Total"), 96.0, true),
            col(tr("응답", "Response"), 86.0, true),
        ],
        Kind::Storage => vec![
            col(tr("볼륨", "Volume"), 0.0, false),
            col(tr("사용 가능", "Available"), 130.0, true),
            col(tr("전체", "Total"), 130.0, true),
        ],
        Kind::Traffic => vec![
            image(),
            pid(),
            col(tr("원격 주소", "Remote address"), 0.0, false),
            col(tr("프로토콜", "Protocol"), 80.0, false),
            col(tr("보내기", "Send"), 96.0, true),
            col(tr("받기", "Receive"), 96.0, true),
            col(tr("합계", "Total"), 96.0, true),
        ],
        Kind::Tcp => vec![
            image(),
            pid(),
            col(tr("로컬 주소", "Local address"), 160.0, false),
            col(tr("포트", "Port"), 65.0, true),
            col(tr("원격 주소", "Remote address"), 160.0, false),
            col(tr("포트", "Port"), 65.0, true),
            col(tr("상태", "State"), 120.0, false),
            col(tr("지연 시간", "Latency"), 90.0, true),
        ],
        Kind::Listening => vec![
            image(),
            pid(),
            col(tr("주소", "Address"), 0.0, false),
            col(tr("포트", "Port"), 75.0, true),
            col(tr("프로토콜", "Protocol"), 90.0, false),
            col(tr("방화벽 상태", "Firewall status"), 170.0, false),
        ],
        Kind::Physical => Vec::new(),
    };
    if kind.check() {
        cols.insert(0, col("✓", 40.0, false));
    }
    cols
}
fn integer(value: u64) -> Cell {
    Cell::num(value.to_string(), value as f64)
}
fn bytes(value: u64) -> Cell {
    Cell::num(paint::human_bytes(value as f64), value as f64)
}
fn number(value: Option<f64>, format: impl Fn(f64) -> String) -> Cell {
    value
        .filter(|n| n.is_finite())
        .map_or_else(|| Cell::text("—"), |n| Cell::num(format(n), n))
}
fn percent(value: f64) -> Cell {
    number(Some(value), |v| format!("{v:.1}%"))
}
fn rate(value: Option<f64>) -> Cell {
    number(value, |v| format!("{}/s", paint::human_bytes(v)))
}
fn status(p: &Process) -> String {
    if p.suspended == Some(true) {
        tr("일시 중단", "Suspended")
    } else if p.responsiveness == Some(false) {
        tr("응답 없음", "Not responding")
    } else if p.efficiency == Some(true) {
        tr("효율 모드", "Efficiency mode")
    } else {
        tr("실행 중", "Running")
    }
    .into()
}
fn current(s: &State, pid: u32, created: Option<u64>) -> Option<&Process> {
    let index = s.process_index.get(&(pid, created?))?;
    s.snapshot.as_ref()?.processes.get(*index)
}
fn identity_columns(s: &State, pid: u32, created: Option<u64>) -> (Option<(u32, u64)>, Vec<Cell>) {
    let process = current(s, pid, created);
    (
        process.map(|p| (p.pid, p.created)),
        vec![
            Cell::text(process.map_or("—", |p| p.name.as_str())),
            integer(pid as u64),
        ],
    )
}
fn error_text(s: &State, fallback: &str) -> String {
    s.data
        .as_ref()
        .filter(|d| !d.errors.is_empty())
        .map_or_else(|| fallback.into(), |d| d.errors.join(" · "))
}
fn process_rows(s: &State, kind: Kind, at: Instant) -> Vec<Row> {
    let at = s.accepted_at.unwrap_or(at);
    let Some(snapshot) = &s.snapshot else {
        return Vec::new();
    };
    snapshot
        .processes
        .iter()
        .filter(|p| match kind {
            Kind::Io => {
                p.io_read_bytes_per_sec > 0.0
                    || p.io_write_bytes_per_sec > 0.0
                    || !p.io_bytes_per_sec.is_finite()
            }
            Kind::Network => s
                .network
                .as_ref()
                .and_then(|n| {
                    n.measured
                        .then(|| n.by_id.get(&(p.pid, p.created)))
                        .flatten()
                })
                .is_none_or(|v| v.total_bytes_per_sec > 0.0),
            _ => true,
        })
        .map(|p| {
            let mut cells = vec![Cell::text(""), Cell::text(&p.name), integer(p.pid as u64)];
            match kind {
                Kind::Cpu => {
                    let average = s
                        .averages
                        .get(&(p.pid, p.created))
                        .and_then(|(start, when)| {
                            let elapsed = at.saturating_duration_since(*when).as_secs_f64();
                            let count =
                                s.performance.as_ref().map_or(1, |p| p.logical_cpus.max(1)) as f64;
                            (elapsed > 0.1).then(|| {
                                p.cpu_time_100ns.saturating_sub(*start) as f64
                                    / 10_000_000.0
                                    / elapsed
                                    / count
                                    * 100.0
                            })
                        });
                    cells.extend([
                        Cell::text(status(p)),
                        integer(p.threads as u64),
                        percent(p.cpu_percent),
                        number(average, |n| format!("{n:.1}%")),
                    ]);
                }
                Kind::Memory => cells.extend([
                    number(p.hard_faults_per_sec, |n| format!("{n:.1}")),
                    bytes(p.private_bytes),
                    bytes(p.working_set),
                    bytes(p.working_set.saturating_sub(p.private_working_set)),
                    bytes(p.private_working_set),
                ]),
                Kind::Io => cells.extend([
                    rate(Some(p.io_read_bytes_per_sec)),
                    rate(Some(p.io_write_bytes_per_sec)),
                    rate(Some(p.io_read_bytes_per_sec + p.io_write_bytes_per_sec)),
                ]),
                Kind::Network => {
                    let net = s
                        .network
                        .as_ref()
                        .filter(|n| n.measured)
                        .and_then(|n| n.by_id.get(&(p.pid, p.created)));
                    cells.extend([
                        rate(net.map(|n| n.send_bytes_per_sec)),
                        rate(net.map(|n| n.recv_bytes_per_sec)),
                        rate(net.map(|n| n.total_bytes_per_sec)),
                    ]);
                }
                _ => {}
            }
            Row {
                identity: Some((p.pid, p.created)),
                cells,
            }
        })
        .collect()
}
unsafe fn rows(s: *mut State, kind: Kind, at: Instant) -> (Vec<Row>, String, String) {
    let st = &*s;
    let no_rows = tr("현재 활동이 없습니다.", "No activity right now");
    let mut empty = no_rows.to_owned();
    let mut summary = String::new();
    let rows = match kind {
        Kind::Cpu | Kind::Memory | Kind::Io | Kind::Network => {
            summary = match kind {
                Kind::Cpu => st
                    .snapshot
                    .as_ref()
                    .map_or_else(String::new, |p| format!("{:.1}% CPU", p.cpu_percent)),
                Kind::Memory => st.snapshot.as_ref().map_or_else(String::new, |p| {
                    format!(
                        "{} / {}",
                        paint::human_bytes(p.memory_used as f64),
                        paint::human_bytes(p.memory_total as f64)
                    )
                }),
                Kind::Io => tr(
                    "모든 읽기·쓰기 I/O · 물리 디스크 처리량과 다릅니다",
                    "All read/write I/O · not physical disk throughput",
                )
                .into(),
                _ => {
                    if st.network.as_ref().is_none_or(|n| !n.measured) {
                        st.network
                            .as_ref()
                            .and_then(|n| n.reason.clone())
                            .unwrap_or_else(|| {
                                tr(
                                    "프로세스별 측정에는 관리자 권한이 필요합니다",
                                    "Per-process measurement requires administrator",
                                )
                                .into()
                            })
                    } else {
                        String::new()
                    }
                }
            };
            process_rows(st, kind, at)
        }
        Kind::Services => {
            summary = tr(
                "CPU는 공유 호스트 전체 사용량입니다",
                "CPU is the entire shared host's usage",
            )
            .into();
            st.services
                .iter()
                .map(|service| {
                    let process = st
                        .pid_index
                        .get(&service.pid)
                        .and_then(|index| st.snapshot.as_ref()?.processes.get(*index));
                    Row {
                        identity: process.map(|p| (p.pid, p.created)),
                        cells: vec![
                            Cell::text(&service.name),
                            if service.pid == 0 {
                                Cell::text("—")
                            } else {
                                integer(service.pid as u64)
                            },
                            Cell::text(&service.display_name),
                            Cell::text(if service.state == SERVICE_RUNNING {
                                tr("실행 중", "Running")
                            } else if service.state == SERVICE_STOPPED {
                                tr("중지됨", "Stopped")
                            } else {
                                tr("전환 중", "Changing")
                            }),
                            process.map_or_else(|| Cell::text("—"), |p| percent(p.cpu_percent)),
                        ],
                    }
                })
                .collect()
        }
        Kind::Handles => {
            empty = tr(
                "핸들 이름 조회 미지원 · 전체 핸들 수는 작업 관리자 상세 열에서 확인하세요.",
                "Handle names unavailable · see handle counts in Task Manager's detail columns.",
            )
            .into();
            Vec::new()
        }
        Kind::Modules => {
            empty = if st.checked.len() != 1 {
                tr("프로세스 하나를 선택하면 모듈을 조회합니다. 전체 프로세스를 주기적으로 검색하지 않습니다.","Select one process to inspect its modules. Other processes are not periodically scanned.").into()
            } else {
                error_text(
                    st,
                    tr(
                        "모듈을 조회하는 중입니다. 보호된 프로세스는 조회할 수 없습니다.",
                        "Loading modules. Protected processes may be unavailable.",
                    ),
                )
            };
            let requested = (st.checked.len() == 1).then(|| *st.checked.iter().next().unwrap());
            st.data
                .as_ref()
                .filter(|d| d.module_identity == requested && requested.is_some())
                .map_or_else(Vec::new, |data| {
                    let id = requested.unwrap();
                    data.modules
                        .iter()
                        .map(|module| {
                            let (identity, mut cells) = identity_columns(st, id.0, Some(id.1));
                            cells.extend([
                                Cell::text(&module.name),
                                Cell::text(&module.path),
                                bytes(module.size_bytes),
                            ]);
                            Row { identity, cells }
                        })
                        .collect()
                })
        }
        Kind::Files => {
            if !st.detailed {
                empty=tr("상세 추적을 켜면 파일별 I/O 요청을 수집합니다. 캐시를 포함하며 물리 디스크 전송량과 다릅니다.","Turn on tracing to collect file I/O requests. These include cache activity and differ from physical disk transfers.").into();
                Vec::new()
            } else if let Some(files) = &st.files {
                summary = files.reason.clone().unwrap_or_default();
                empty = files.reason.clone().unwrap_or_else(|| {
                    tr(
                        "이 구간에 프로세스와 파일을 확인할 수 있는 요청이 없습니다.",
                        "No attributable process/file requests in this interval.",
                    )
                    .into()
                });
                if files.measured {
                    files
                        .rows
                        .iter()
                        .map(|row| {
                            let (identity, mut cells) =
                                identity_columns(st, row.pid, Some(row.created));
                            cells.extend([
                                Cell::text(&row.path),
                                rate(Some(row.read_bytes_per_sec)),
                                rate(Some(row.write_bytes_per_sec)),
                                rate(Some(row.read_bytes_per_sec + row.write_bytes_per_sec)),
                                number(row.response_ms, |v| format!("{v:.1} ms")),
                            ]);
                            Row { identity, cells }
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                empty = tr("상세 추적을 준비하는 중입니다.", "Preparing tracing.").into();
                Vec::new()
            }
        }
        Kind::Traffic => {
            if !st.detailed {
                empty=tr("상세 추적을 켜면 원격 주소별 전송량을 수집합니다. DNS 조회나 패킷 본문 수집은 하지 않습니다.","Turn on tracing to measure traffic by remote address. No DNS lookup or packet content collection.").into();
                Vec::new()
            } else if let Some(network) = &st.network {
                let endpoints = &network.endpoints;
                summary = endpoints.reason.clone().unwrap_or_default();
                empty = endpoints.reason.clone().unwrap_or_else(|| {
                    tr(
                        "이 구간에 확인된 전송이 없습니다.",
                        "No attributable transfers in this interval.",
                    )
                    .into()
                });
                if endpoints.measured {
                    endpoints
                        .rows
                        .iter()
                        .map(|row| {
                            let (identity, mut cells) =
                                identity_columns(st, row.pid, Some(row.created));
                            cells.extend([
                                Cell::text(
                                    std::net::SocketAddr::new(row.remote_addr, row.remote_port)
                                        .to_string(),
                                ),
                                Cell::text(match row.protocol {
                                    crate::netetw::Transport::Tcp => "TCP",
                                    crate::netetw::Transport::Udp => "UDP",
                                }),
                                rate(Some(row.send_bytes_per_sec)),
                                rate(Some(row.recv_bytes_per_sec)),
                                rate(Some(row.send_bytes_per_sec + row.recv_bytes_per_sec)),
                            ]);
                            Row { identity, cells }
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            } else {
                empty = tr("상세 추적을 준비하는 중입니다.", "Preparing tracing.").into();
                Vec::new()
            }
        }
        Kind::Tcp | Kind::Listening => {
            empty = error_text(st, no_rows);
            summary = if kind == Kind::Tcp {
                tr("지연 시간은 측정하지 않습니다", "Latency is not measured")
            } else {
                tr(
                    "UDP는 바인딩된 포트 · 방화벽 정책은 평가하지 않습니다",
                    "UDP shows bound ports · firewall policy is not evaluated",
                )
            }
            .into();
            st.data.as_ref().map_or_else(Vec::new, |data| {
                data.endpoints
                    .iter()
                    .filter(|e| {
                        if kind == Kind::Tcp {
                            e.protocol == "TCP" && !e.listening
                        } else {
                            e.listening
                        }
                    })
                    .map(|e| {
                        let (identity, mut cells) = identity_columns(st, e.pid, e.created);
                        cells.extend([Cell::text(&e.local_address), integer(e.local_port as u64)]);
                        if kind == Kind::Tcp {
                            cells.extend([
                                Cell::text(e.remote_address.as_deref().unwrap_or("—")),
                                e.remote_port
                                    .map_or_else(|| Cell::text("—"), |v| integer(v as u64)),
                                Cell::text(e.state),
                                Cell::text("—"),
                            ]);
                        } else {
                            cells.extend([
                                Cell::text(e.protocol),
                                Cell::text(tr("평가하지 않음", "Not evaluated")),
                            ]);
                        }
                        Row { identity, cells }
                    })
                    .collect()
            })
        }
        Kind::Storage => {
            empty = error_text(st, no_rows);
            summary = tr(
                "로컬 고정 볼륨의 사용 가능 공간",
                "Available space on local fixed volumes",
            )
            .into();
            st.data.as_ref().map_or_else(Vec::new, |data| {
                data.volumes
                    .iter()
                    .map(|v| Row {
                        identity: None,
                        cells: vec![
                            Cell::text(&v.name),
                            bytes(v.free_bytes),
                            bytes(v.total_bytes),
                        ],
                    })
                    .collect()
            })
        }
        Kind::Physical => Vec::new(),
    };
    (rows, summary, empty)
}
pub(super) unsafe fn rebuild(s: *mut State, at: Instant) {
    for index in 0..(*s).panels.len() {
        let kind = (&(*s).panels)[index].kind;
        if (*s).collapsed.contains(&((*s).tab, kind)) {
            let had_rows = !(&(*s).panels)[index].rows.is_empty();
            (&mut (*s).panels)[index].rows.clear();
            (&mut (*s).panels)[index].summary.clear();
            let table = (&(*s).panels)[index].table;
            if !table.is_null() && had_rows {
                SendMessageW(table, LVM_SETITEMCOUNT, 0, 0);
            }
            continue;
        }
        let (mut rows, summary, mut empty) = rows(s, kind, at);
        if !kind.check() && kind != Kind::Storage && !(*s).checked.is_empty() {
            rows.retain(|r| r.identity.is_some_and(|id| (*s).checked.contains(&id)));
        }
        if !(&(*s).query).is_empty() {
            rows.retain(|r| {
                r.cells
                    .iter()
                    .any(|c| c.text.to_lowercase().contains(&(*s).query))
            });
        }
        let sort = (&(*s).panels)[index].sort;
        let descending = (&(*s).panels)[index].descending;
        rows.sort_by(|a, b| {
            if kind.check() {
                let checked = |r: &Row| r.identity.is_some_and(|id| (*s).checked.contains(&id));
                let order = checked(b).cmp(&checked(a));
                if order != Ordering::Equal {
                    return order;
                }
            }
            let order = match (a.cells.get(sort), b.cells.get(sort)) {
                (Some(a), Some(b)) => match (a.number, b.number) {
                    (Some(a), Some(b)) => a.total_cmp(&b),
                    (Some(_), None) => Ordering::Greater,
                    (None, Some(_)) => Ordering::Less,
                    _ => a.text.cmp(&b.text),
                },
                _ => Ordering::Equal,
            };
            (if descending { order.reverse() } else { order })
                .then_with(|| a.identity.cmp(&b.identity))
        });
        if rows.is_empty() && !(&(*s).query).is_empty() {
            empty = tr(
                "검색과 일치하는 항목이 없습니다.",
                "Nothing matches the search.",
            )
            .into();
        }
        let count = rows.len();
        let table = (&(*s).panels)[index].table;
        // Native focus is an index, while CPU/IO sorting changes indices.
        // Keep keyboard actions attached to the same process generation.
        let selected = if kind.check() && !table.is_null() {
            let row = SendMessageW(table, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
            (row >= 0).then_some(row as usize)
        } else {
            None
        };
        let selected_identity = selected
            .and_then(|row| (&(*s).panels)[index].rows.get(row))
            .and_then(|row| row.identity);
        let next_selected =
            selected_identity.and_then(|id| rows.iter().position(|row| row.identity == Some(id)));
        let count_changed = (&(*s).panels)[index].rows.len() != count;
        (&mut (*s).panels)[index].rows = rows;
        (&mut (*s).panels)[index].summary = summary;
        (&mut (*s).panels)[index].empty = empty;
        if !table.is_null() && count_changed {
            SendMessageW(table, LVM_SETITEMCOUNT, count, LVSICF_NOSCROLL as isize);
        }
        if selected != next_selected {
            let item = LVITEMW {
                stateMask: LVIS_SELECTED | LVIS_FOCUSED,
                state: if next_selected.is_some() {
                    LVIS_SELECTED | LVIS_FOCUSED
                } else {
                    0
                },
                ..zeroed()
            };
            SendMessageW(
                table,
                LVM_SETITEMSTATE,
                next_selected.unwrap_or(usize::MAX),
                &item as *const LVITEMW as isize,
            );
        }
    }
}
pub(super) unsafe fn record(s: *mut State, at: Instant) {
    let Some(snapshot) = &(*s).snapshot else {
        return;
    };
    let memory = if snapshot.memory_total > 0 {
        snapshot.memory_used as f64 / snapshot.memory_total as f64 * 100.0
    } else {
        f64::NAN
    };
    let mut values = vec![
        ("cpu".into(), snapshot.cpu_percent),
        ("memory".into(), memory),
    ];
    if (*s).tab == Tab::Memory {
        let faults = snapshot
            .processes
            .iter()
            .filter_map(|p| p.hard_faults_per_sec)
            .reduce(|a, b| a + b);
        values.push(("faults".into(), faults.unwrap_or(f64::NAN)));
    }
    if (*s).tab == Tab::Cpu && !(*s).services.is_empty() {
        let hosts: HashSet<_> = (*s)
            .services
            .iter()
            .filter(|service| service.state == SERVICE_RUNNING && service.pid != 0)
            .map(|service| service.pid)
            .collect();
        let cpu = hosts
            .iter()
            .filter_map(|pid| {
                (*s).pid_index
                    .get(pid)
                    .and_then(|index| snapshot.processes.get(*index))
            })
            .map(|p| p.cpu_percent)
            .reduce(|a, b| a + b);
        values.push(("service_hosts".into(), cpu.unwrap_or(f64::NAN)));
    }
    if let Some(perf) = &(*s).performance {
        let commit = perf
            .memory
            .as_ref()
            .filter(|m| m.commit_limit > 0)
            .map_or(f64::NAN, |m| {
                m.commit_used as f64 / m.commit_limit as f64 * 100.0
            });
        if (*s).tab == Tab::Memory {
            values.push(("commit".into(), commit));
        }
        values.extend([
            (
                "disk".into(),
                if perf.disk_rates_ready {
                    perf.disk_read_bytes_per_sec + perf.disk_write_bytes_per_sec
                } else {
                    f64::NAN
                },
            ),
            (
                "network".into(),
                if perf.network_rates_ready {
                    perf.network_rx_bytes_per_sec + perf.network_tx_bytes_per_sec
                } else {
                    f64::NAN
                },
            ),
        ]);
        for cpu in perf
            .logical_processors
            .iter()
            .filter(|_| (*s).tab == Tab::Cpu)
            .take(256)
        {
            values.push((format!("core:{}", cpu.id), cpu.percent.unwrap_or(f64::NAN)));
        }
        for disk in perf.disks.iter().filter(|_| (*s).tab == Tab::Disk).take(32) {
            values.push((
                format!("disk:{}", disk.id),
                disk.active_percent.unwrap_or(f64::NAN),
            ));
        }
        for nic in perf
            .networks
            .iter()
            .filter(|n| (*s).tab == Tab::Network && n.connected)
            .take(32)
        {
            let utilization = nic
                .rx_bytes_per_sec
                .zip(nic.tx_bytes_per_sec)
                .filter(|_| nic.receive_link_bits_per_sec > 0 && nic.transmit_link_bits_per_sec > 0)
                .map_or(f64::NAN, |(rx, tx)| {
                    (rx * 8.0 / nic.receive_link_bits_per_sec as f64)
                        .max(tx * 8.0 / nic.transmit_link_bits_per_sec as f64)
                        * 100.0
                });
            values.push((format!("nic:{}", nic.id), utilization));
        }
    }
    if let Some(data) = (*s).data.as_ref().filter(|_| (*s).tab == Tab::Network) {
        values.push((
            "tcp".into(),
            data.endpoints
                .iter()
                .filter(|e| e.protocol == "TCP" && !e.listening)
                .count() as f64,
        ));
    }
    let live: HashSet<_> = values.iter().map(|(key, _)| key.clone()).collect();
    (*s).traces.retain(|key, _| live.contains(key));
    for (key, value) in values {
        (*s).traces
            .entry(key)
            .or_default()
            .record(at, [value, f64::NAN, f64::NAN]);
    }
}

pub(super) struct Model(pub(super) *mut State, pub(super) usize);

/// At a nested table's scroll boundary, wheel input continues through the
/// surrounding panel list, as it does in the reference's nested scroll areas.
pub(super) unsafe fn install_scroll(table: HWND, s: *mut State) {
    SetWindowSubclass(table, Some(scroll_chain), 0xF452, s as usize);
}
unsafe extern "system" fn scroll_chain(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    if msg == WM_NCDESTROY {
        RemoveWindowSubclass(hwnd, Some(scroll_chain), id);
    }
    if msg == WM_MOUSEWHEEL && w & 0x0004 == 0 {
        let delta = ((w >> 16) & 0xffff) as i16 as i32;
        let extent = table::extent(hwnd);
        let offset = table::scroll_offset(hwnd);
        if (delta > 0 && offset <= 0.5) || (delta < 0 && offset >= extent.max() - 0.5) {
            let s = data as *mut State;
            return SendMessageW((*s).body, msg, w, l);
        }
    }
    DefSubclassProc(hwnd, msg, w, l)
}
impl table::Model for Model {
    unsafe fn horizontal_columns(&self) -> bool {
        true
    }
    unsafe fn themed_horizontal(&self) -> bool {
        true
    }
    unsafe fn dpi(&self) -> i32 {
        (*self.0).dpi
    }
    unsafe fn fonts(&self) -> &fonts::Fonts {
        &(*self.0).fonts
    }
    unsafe fn header_height(&self) -> i32 {
        gfx::pxi((*self.0).dpi, 34.0)
    }
    unsafe fn row_height(&self, _: usize) -> i32 {
        gfx::pxi((*self.0).dpi, 32.0)
    }
    unsafe fn line(&self) -> i32 {
        self.row_height(0)
    }
    unsafe fn key(&self, row: usize) -> u64 {
        use std::hash::{Hash, Hasher};
        let state = &*self.0;
        let panel = &state.panels[self.1];
        let Some(entry) = panel.rows.get(row) else {
            return row as u64;
        };
        let Some((pid, created)) = entry.identity else {
            return row as u64;
        };
        let process = created.rotate_left(13) ^ pid as u64;
        if panel.kind.check() {
            return process;
        }
        // One process owns several rows here (modules, endpoints, files,
        // services); their text cells tell them apart, so a screen reader
        // announces each row instead of only the first of a process.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        process.hash(&mut hasher);
        for cell in entry.cells.iter().filter(|cell| cell.number.is_none()) {
            cell.text.hash(&mut hasher);
        }
        hasher.finish()
    }
    unsafe fn header(&self, col: usize) -> table::HeaderCell {
        let state = &*self.0;
        let panel = &state.panels[self.1];
        table::HeaderCell {
            sort: (panel.sort == col).then_some(panel.descending),
            ..Default::default()
        }
    }
    unsafe fn text(&self, row: usize, col: usize) -> String {
        let state = &*self.0;
        let panel = &state.panels[self.1];
        if panel.kind.check() && col == 0 {
            return panel.rows.get(row).map_or_else(String::new, |row| {
                let name = row.cells.get(1).map_or("", |c| c.text.as_str());
                let checked = row
                    .identity
                    .is_some_and(|id| (*self.0).checked.contains(&id));
                format!(
                    "{} · {}",
                    name,
                    if checked {
                        tr("선택됨", "Checked")
                    } else {
                        tr("선택 안 됨", "Not checked")
                    }
                )
            });
        }
        panel
            .rows
            .get(row)
            .and_then(|r| r.cells.get(col))
            .map_or_else(String::new, |c| c.text.clone())
    }
    unsafe fn empty_text(&self) -> String {
        let state = &*self.0;
        state.panels[self.1].empty.clone()
    }
    unsafe fn paint_row(
        &self,
        pt: &Painter,
        row: usize,
        r: RECT,
        cells: &[RECT],
        st: &table::RowPaint,
    ) {
        let state = &*self.0;
        let panel = &state.panels[self.1];
        let Some(row) = panel.rows.get(row) else {
            return;
        };
        let checked = row.identity.is_some_and(|id| state.checked.contains(&id));
        let bg = widgets::row_background(&pt.c, st.hover, st.selected || checked);
        pt.fill(r, bg);
        for (i, (cell, value)) in cells.iter().zip(&row.cells).enumerate() {
            if cell.right <= cell.left {
                continue;
            }
            if panel.kind.check() && i == 0 {
                let size = pt.px(14.0);
                let x = (cell.left + cell.right) as f32 / 2.0 - size / 2.0;
                let y = (r.top + r.bottom) as f32 / 2.0 - size / 2.0;
                pt.canvas.bordered_round_rect(
                    gfx::RectF::new(x, y, size, size),
                    gfx::Radii::all(pt.px(2.0)),
                    pt.hair(),
                    theme::solid(if checked { pt.c.fg } else { bg }),
                    theme::solid(if checked { pt.c.fg } else { pt.c.muted }),
                );
                if checked {
                    pt.canvas.polyline(
                        &[
                            (x + size * 0.2, y + size * 0.52),
                            (x + size * 0.43, y + size * 0.75),
                            (x + size * 0.82, y + size * 0.26),
                        ],
                        pt.px(1.5),
                        theme::solid(pt.c.surface),
                    );
                }
                continue;
            }
            let column = &panel.columns[i];
            let mut text_box = RECT {
                left: cell.left + pt.pxi(12.0),
                right: cell.right - pt.pxi(12.0),
                ..*cell
            };
            let first_text = if panel.kind.check() { 1 } else { 0 };
            if i == first_text && panel.kind != Kind::Storage {
                let badge = RECT {
                    left: text_box.left,
                    top: (r.top + r.bottom - pt.pxi(20.0)) / 2,
                    right: text_box.left + pt.pxi(20.0),
                    bottom: (r.top + r.bottom + pt.pxi(20.0)) / 2,
                };
                widgets::mono_badge(pt, badge, &widgets::initials(&value.text), false);
                text_box.left += pt.pxi(28.0);
            }
            if column.right {
                if let Some(value) = value.number {
                    let threshold = match panel.kind {
                        Kind::Cpu if i >= 5 => Some(12.0),
                        Kind::Memory if i >= 5 => Some(2_400_000_000.0),
                        Kind::Io | Kind::Network if i >= 3 => Some(1_000_000.0),
                        Kind::Files if (3..6).contains(&i) => Some(1_000_000.0),
                        Kind::Traffic if i >= 4 => Some(1_000_000.0),
                        _ => None,
                    };
                    if let Some(threshold) = threshold {
                        widgets::heat_cell_bg(pt, *cell, bg, widgets::heat_alpha(value, threshold));
                    }
                }
            }
            let color = if value.text == "—" {
                pt.c.muted
            } else {
                pt.c.fg
            };
            pt.label(
                if column.right {
                    pt.fonts.mono_cell
                } else {
                    pt.fonts.body
                },
                color,
                &value.text,
                text_box,
                if column.right { DT_RIGHT } else { DT_LEFT },
            );
        }
        pt.fill(
            RECT {
                top: r.bottom - pt.hair().ceil() as i32,
                ..r
            },
            pt.c.row_border,
        );
    }
}
