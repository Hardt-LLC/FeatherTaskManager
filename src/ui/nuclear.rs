//! "Nuclear Zombie": one modal panel that tidies memory on a long-running PC
//! without ending anything the user runs, and reports "zombie" processes
//! (exited processes another program still holds open).
//!
//! The work is `memclean::run` on the job worker, never on the UI thread. It
//! reports progress and its report through the panel's own channel
//! ([`CleanupEvent`], announced to the main window with [`CLEANUP_READY`]),
//! so the panel sees them while its modal loop runs. The panel is a fully
//! painted layered popup (`popup::stage_panel`; DESIGN_SPEC §4 dialog: scrim,
//! surface, radius 8, shadow, header / body / footer) driven by a local loop
//! like the confirm dialog. The header and footer stay put; the body scrolls
//! under the overlay scrollbar (`scroll::Scroller`).
//!
//! Honesty (MASTER.md): every number is measured. Zombies are counted, not
//! sized. Nothing is ended automatically: a holder's End task closes the
//! panel and goes through the app's confirm / end-task flow.

use super::gfx::{Radii, RectF};
use super::popup::{self, PanelContent, Step, TextLine};
use super::scroll::{self, Extent, Scroller};
use super::theme::{solid, Palette};
use super::widgets::{self, ButtonState, ButtonStyle, Painter, PillKind};
use super::*;
use crate::memclean::{
    self, CleanupOptions, CleanupReport, MemoryState, Progress, PurgeError, TrimReport,
    ZombieHolder, ZombieScan,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::TryRecvError;

/// Posted to the main window when the job worker sent a [`CleanupEvent`].
pub(super) const CLEANUP_READY: u32 = WM_APP + 4;

/// What a running cleanup tells the UI.
pub(super) enum CleanupEvent {
    Progress(Progress),
    Done(Box<CleanupReport>),
}

type Key = (usize, u32);

/// Option rows, in panel order.
const TRIM: usize = 0;
const STANDBY: usize = 1;
const MODIFIED: usize = 2;
const ZOMBIES: usize = 3;
/// `.dialog`-like panel width (DIP); `min(560px, 100% - 32px)`.
const WIDTH: f32 = 560.0;
/// Holder rows painted (Copy details lists every holder).
const HOLDERS_SHOWN: usize = 50;
/// Scroll offset, scrollbar opacity and growth of the body.
const SCROLL: Key = (0x400, anim::part::SCROLL);
const BAR: Key = (0x400, anim::part::SCROLLBAR);
const GROW: Key = (0x400, anim::part::SCROLLBAR_WIDTH);
/// One wheel line (CSS px), the tables' row pitch.
const LINE: f32 = 34.0;

thread_local! {
    /// A run whose panel was closed before it finished: its report arrives
    /// later ([`background_event`]).
    static BACKGROUND: RefCell<Option<Receiver<CleanupEvent>>> = const { RefCell::new(None) };
}

// ───────────────────────────── job ─────────────────────────────

/// The job worker's side of a run: every step reports through `events`.
pub(super) fn run_job(hwnd: usize, options: CleanupOptions, events: &Sender<CleanupEvent>) {
    let post = || unsafe {
        PostMessageW(hwnd as HWND, CLEANUP_READY, 0, 0);
    };
    let report = memclean::run(options, |progress| {
        if events.send(CleanupEvent::Progress(progress)).is_ok() {
            post();
        }
    });
    if events.send(CleanupEvent::Done(Box::new(report))).is_ok() {
        post();
    }
}

/// [`CLEANUP_READY`] outside the panel: a run whose panel was closed ended.
/// The action is over (`busy` clears) and a toast gives the measured result.
pub(super) unsafe fn background_event(p: *mut App) {
    // Some(report): finished; Some(None): the worker went away.
    let finished: Option<Option<Box<CleanupReport>>> = BACKGROUND.with(|slot| {
        let mut slot = slot.borrow_mut();
        let rx = slot.as_ref()?;
        let mut result = None;
        loop {
            match rx.try_recv() {
                Ok(CleanupEvent::Done(report)) => result = Some(Some(report)),
                Ok(CleanupEvent::Progress(_)) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    result = result.or(Some(None));
                    break;
                }
            }
        }
        if result.is_some() {
            *slot = None;
        }
        result
    });
    let Some(report) = finished else {
        return;
    };
    (*p).busy = false;
    match report {
        Some(report) => controls::notify_success(p, summary(&report)),
        None => (*p).set_error(ErrorSource::Action, stopped().into()),
    }
    update_buttons(p);
    redraw(p);
}

fn stopped() -> &'static str {
    tr(
        "메모리 정리가 예기치 않게 중단되었습니다.",
        "The memory cleanup stopped unexpectedly.",
    )
}

// ───────────────────────────── text ─────────────────────────────

fn subtitle() -> &'static str {
    tr(
        "메모리를 정리하고 좀비 프로세스를 찾습니다",
        "Clean up memory and find zombie processes",
    )
}

fn note() -> &'static str {
    tr(
        "실행 중인 앱은 그대로 계속 실행됩니다. Windows는 캐시된 메모리를 자동으로 재사용하므로, 이 기능은 주로 메모리 사용량이 크게 치솟은 뒤나 종료된 프로세스를 놓아주지 않는 프로그램을 찾을 때 도움이 됩니다.",
        "Running apps keep running. Windows reuses cached memory automatically, so this mainly helps after a large memory spike, or to find programs that leak exited processes.",
    )
}

fn option_title(index: usize) -> &'static str {
    match index {
        TRIM => tr("작업 집합 정리", "Trim working sets"),
        STANDBY => tr("대기 캐시 비우기", "Clear standby cache"),
        MODIFIED => tr("수정된 페이지 기록", "Flush modified pages"),
        _ => tr("좀비 프로세스 찾기", "Find zombie processes"),
    }
}

fn option_description(index: usize) -> &'static str {
    match index {
        TRIM => tr(
            "실행 중인 앱의 유휴 페이지를 RAM에서 내보냅니다. 다시 사용하면 대기 목록에서 돌아옵니다.",
            "Moves idle pages of running apps out of RAM. They come back from the standby list when used again.",
        ),
        STANDBY => tr(
            "Windows가 여유 메모리에 캐시해 둔 파일 데이터를 버립니다. 캐시가 다시 채워질 때까지 앱이 조금 느리게 열릴 수 있습니다.",
            "Drops file data Windows keeps cached in otherwise unused memory. Apps may open a little slower until it is rebuilt.",
        ),
        MODIFIED => tr(
            "변경된 페이지를 디스크에 기록해 대기 목록으로 옮깁니다.",
            "Writes changed pages to disk so they can join the standby list.",
        ),
        _ => tr(
            "다른 프로그램이 아직 열어 두고 있는 종료된 프로세스를 찾습니다. 읽기 전용이며 아무것도 닫지 않습니다.",
            "Lists exited processes that another program still holds open. Read-only: nothing is closed.",
        ),
    }
}

/// Standby and modified purges need administrator rights (UAC).
fn needs_admin(index: usize) -> bool {
    matches!(index, STANDBY | MODIFIED)
}

fn progress_text(progress: Option<Progress>) -> String {
    match progress {
        None => tr("시작하는 중…", "Starting…").into(),
        Some(Progress::Trimming(count)) => tf!(
            "프로세스 {}개 정리 중…",
            "Trimming {} processes…",
            grouped(count)
        ),
        Some(Progress::WaitingForAdministrator) => tr(
            "관리자 승인을 기다리는 중…",
            "Waiting for administrator approval…",
        )
        .into(),
        Some(Progress::Purging) => tr("메모리 목록을 비우는 중…", "Clearing memory lists…").into(),
        Some(Progress::Scanning) => tr("핸들을 검사하는 중…", "Scanning handles…").into(),
    }
}

fn bytes(value: u64) -> String {
    paint::human_bytes(value as f64)
}

/// A signed change: "+2.6 GB", "−312.0 MB" (U+2212), "±0 B".
fn delta(before: u64, after: u64) -> String {
    match after.cmp(&before) {
        std::cmp::Ordering::Greater => format!("+{}", bytes(after - before)),
        std::cmp::Ordering::Less => format!("\u{2212}{}", bytes(before - after)),
        std::cmp::Ordering::Equal => "\u{00B1}0 B".into(),
    }
}

fn exited(count: u32) -> String {
    if count == 1 {
        tr("종료된 프로세스 1개", "1 exited process").into()
    } else {
        tf!(
            "종료된 프로세스 {}개",
            "{} exited processes",
            grouped(count)
        )
    }
}

fn trim_text(report: &TrimReport) -> String {
    let mut text = tf!(
        "완료 · {}개 정리, {}개 건너뜀(접근 불가)",
        "Done · {} trimmed, {} skipped (no access)",
        grouped(report.trimmed),
        grouped(report.skipped)
    );
    if report.failed > 0 {
        text.push_str(&tf!(", {}개 실패", ", {} failed", grouped(report.failed)));
    }
    text
}

fn scan_summary(scan: &ZombieScan) -> String {
    if scan.total == 0 {
        return tr(
            "좀비 프로세스를 찾지 못했습니다.",
            "No zombie processes found.",
        )
        .into();
    }
    let holders = scan.holders.len();
    let programs = if holders == 1 {
        tr("프로그램 1개", "1 program").to_owned()
    } else {
        tf!("프로그램 {}개", "{} programs", grouped(holders))
    };
    tf!(
        "{}가 {}를 붙잡고 있습니다",
        "{} held open by {}",
        // Korean reads "programs hold exited processes".
        if language() == Language::Korean {
            programs.clone()
        } else {
            exited(scan.total)
        },
        if language() == Language::Korean {
            exited(scan.total)
        } else {
            programs
        }
    )
}

/// "13 exited processes · PowerToys.Settings.exe ×8 · Awake.exe ×3".
fn holder_detail(holder: &ZombieHolder) -> String {
    let mut text = exited(holder.zombies);
    for (name, count) in &holder.examples {
        text.push_str(&format!(" \u{00B7} {name} \u{00D7}{count}"));
    }
    text
}

fn partial_text(scan: &ZombieScan, elevated: bool) -> Option<String> {
    if !scan.partial() {
        return None;
    }
    let mut text = if scan.uninspected > 0 {
        tf!(
            "일부만 검사했습니다: 프로세스 {}개를 검사할 수 없었습니다(다른 사용자, 서비스 또는 보호된 프로세스).",
            "Partial scan: {} processes could not be inspected (other users, services or protected processes).",
            grouped(scan.uninspected)
        )
    } else {
        tf!(
            "일부만 검사했습니다: 프로세스 핸들 {}개를 검사할 수 없었습니다.",
            "Partial scan: {} process handles could not be inspected.",
            grouped(scan.uninspected_handles)
        )
    };
    if !elevated {
        text.push_str(tr(
            " Feather를 관리자 권한으로 실행하면 더 많이 검사할 수 있습니다.",
            " Run Feather as administrator to include more.",
        ));
    }
    Some(text)
}

fn explanation() -> &'static str {
    tr(
        "좀비는 이미 종료되었지만 다른 프로그램이 아직 열어 두고 있어 메모리에 남아 있는 프로세스입니다. 붙잡고 있는 프로그램을 다시 시작하면 해제됩니다. Feather는 핸들을 읽기만 하며 다른 프로그램의 핸들을 닫지 않습니다.",
        "A zombie is a process that has exited but stays in memory because another program still holds it open. Restarting the program that holds it releases it. Feather only reads handles and never closes them inside other programs.",
    )
}

/// How a step ended, for its dot and text color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Done,
    /// Needs administrator, declined, or unavailable: the user can act.
    Attention,
    Failed,
    /// Not attempted (an earlier list failed).
    Skipped,
}

fn purge_outcome(result: &Result<(), PurgeError>) -> (Status, String) {
    match result {
        Ok(()) => (Status::Done, tr("완료", "Done").into()),
        Err(
            error @ (PurgeError::Declined | PurgeError::Unavailable(_) | PurgeError::Privilege(_)),
        ) => (Status::Attention, error.message()),
        Err(error @ PurgeError::NotRun) => (Status::Skipped, error.message()),
        Err(error @ PurgeError::Status(_)) => (Status::Failed, error.message()),
    }
}

/// Every chosen step with its outcome, in panel order.
fn steps(report: &CleanupReport) -> Vec<(usize, Status, String)> {
    let mut rows = Vec::new();
    if let Some(trim) = &report.trim {
        rows.push(match trim {
            Ok(trim) => (TRIM, Status::Done, trim_text(trim)),
            Err(error) => (TRIM, Status::Failed, error.clone()),
        });
    }
    for (index, result) in [(STANDBY, &report.standby), (MODIFIED, &report.modified)] {
        if let Some(result) = result {
            let (status, text) = purge_outcome(result);
            rows.push((index, status, text));
        }
    }
    if let Some(zombies) = &report.zombies {
        rows.push(match zombies {
            Ok(scan) => (
                ZOMBIES,
                Status::Done,
                tf!("완료 · {}", "Done · {}", exited(scan.total)),
            ),
            Err(error) => (ZOMBIES, Status::Failed, error.clone()),
        });
    }
    rows
}

/// The toast after a run whose panel was closed.
fn summary(report: &CleanupReport) -> String {
    match (&report.before, &report.after) {
        (Ok(before), Ok(after)) => tf!(
            "메모리 정리를 마쳤습니다 · 사용 가능 {}",
            "Memory cleanup finished · Available {}",
            delta(before.available, after.available)
        ),
        _ => tr("메모리 정리를 마쳤습니다.", "Memory cleanup finished.").into(),
    }
}

/// The five memory readings: (label, value).
fn readings(state: &MemoryState) -> [(&'static str, Option<u64>); 5] {
    let lists = state.lists;
    [
        (tr("사용 가능", "Available"), Some(state.available)),
        (tr("사용 중", "In use"), Some(state.in_use())),
        (tr("대기", "Standby"), lists.map(|l| l.standby)),
        (tr("수정됨", "Modified"), lists.map(|l| l.modified)),
        (tr("비어 있음", "Free"), lists.map(|l| l.free)),
    ]
}

/// "Copy details": everything the run measured and did, as plain text.
fn details(report: &CleanupReport) -> String {
    let mut out = vec!["Feather Task Manager \u{00B7} Nuclear Zombie".to_owned()];
    out.push(tf!(
        "관리자 권한: {}",
        "Administrator: {}",
        if report.elevated {
            tr("예", "yes")
        } else {
            tr("아니요", "no")
        }
    ));
    match (&report.before, &report.after) {
        (Ok(before), Ok(after)) => {
            out.push(tf!(
                "전체 메모리: {}",
                "Total memory: {}",
                bytes(after.total)
            ));
            for ((label, a), (_, b)) in readings(before).into_iter().zip(readings(after)) {
                out.push(match (a, b) {
                    (Some(a), Some(b)) => {
                        format!("{label}: {} -> {} ({})", bytes(a), bytes(b), delta(a, b))
                    }
                    _ => format!("{label}: \u{2014}"),
                });
            }
        }
        (Err(error), _) | (_, Err(error)) => out.push(error.clone()),
    }
    for (index, _, text) in steps(report) {
        out.push(format!("{}: {text}", option_title(index)));
    }
    if let Some(Ok(scan)) = &report.zombies {
        out.push(scan_summary(scan));
        for holder in &scan.holders {
            let examples: Vec<String> = holder
                .examples
                .iter()
                .map(|(name, count)| format!("{name} ({count})"))
                .collect();
            let mut line = format!(
                "  {} (PID {}) - {}",
                holder.name,
                holder.pid,
                exited(holder.zombies)
            );
            if !examples.is_empty() {
                line.push_str(": ");
                line.push_str(&examples.join(", "));
            }
            out.push(line);
        }
        if let Some(partial) = partial_text(scan, report.elevated) {
            out.push(partial);
        }
    }
    out.join("\r\n")
}

// ───────────────────────────── model ─────────────────────────────

/// Everything focusable or clickable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// An option row (its switch).
    Switch(usize),
    /// A zombie holder's End task.
    End(usize),
    Copy,
    Close,
    Run,
}

impl Target {
    fn id(self) -> usize {
        match self {
            Target::Switch(i) => 0x100 + i,
            Target::End(i) => 0x200 + i,
            Target::Copy => 0x300,
            Target::Close => 0x301,
            Target::Run => 0x302,
        }
    }
    fn hover(self) -> Key {
        (self.id(), anim::part::HOVER)
    }
    fn is_button(self) -> bool {
        !matches!(self, Target::Switch(_))
    }
}

fn switch_key(index: usize) -> Key {
    (Target::Switch(index).id(), anim::part::SWITCH)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Running(Option<Progress>),
    Done,
}

/// How the panel ended.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Outcome {
    Closed,
    /// End task for a holder: (name, PID, creation time, zombies held).
    End(String, u32, u64, u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    Header,
    /// Scrolls: coordinates from the body's top, before the offset.
    Body,
    Footer,
}

/// Colors by token, resolved at paint time (theme changes repaint).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ink {
    Fg,
    Muted,
    Warn,
    Danger,
    Accent,
    Border,
    RowBorder,
    Bg,
    Surface,
}

impl Ink {
    fn color(self, c: &Palette) -> u32 {
        match self {
            Ink::Fg => c.fg,
            Ink::Muted => c.muted,
            Ink::Warn => c.warn_fg,
            Ink::Danger => c.danger,
            Ink::Accent => c.accent,
            Ink::Border => c.border,
            Ink::RowBorder => c.row_border,
            Ink::Bg => c.bg,
            Ink::Surface => c.surface,
        }
    }
}

impl Status {
    fn ink(self) -> Ink {
        match self {
            Status::Done => Ink::Fg,
            Status::Attention => Ink::Warn,
            Status::Failed => Ink::Danger,
            Status::Skipped => Ink::Muted,
        }
    }
    fn dot(self) -> Ink {
        match self {
            Status::Done => Ink::Accent,
            Status::Attention => Ink::Warn,
            Status::Failed => Ink::Danger,
            Status::Skipped => Ink::Muted,
        }
    }
}

/// Font roles with their CSS line boxes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Face {
    /// dialog h2 18 / 600, line 1.3.
    Title,
    Ui,
    UiStrong,
    Small,
    MonoSmall,
    /// 15 / 600 mono, line 1.2 (the header totals' role).
    MonoTotal,
}

impl Face {
    fn font(self, f: &fonts::Fonts) -> HFONT {
        match self {
            Face::Title => f.dialog_title,
            Face::Ui => f.ui,
            Face::UiStrong => f.ui_strong,
            Face::Small => f.small,
            Face::MonoSmall => f.mono_small,
            Face::MonoTotal => f.mono_total,
        }
    }
    /// CSS line height (px).
    fn line(self) -> f32 {
        match self {
            Face::Title => 18.0 * 1.3,
            Face::Ui | Face::UiStrong => 13.0 * 1.45,
            Face::Small | Face::MonoSmall => 12.0 * 1.45,
            Face::MonoTotal => 15.0 * 1.2,
        }
    }
}

/// One painted element (a display list built by [`layout`], so painting a
/// frame never measures or wraps text).
enum Item {
    /// A wrapped paragraph line (fractional advances: it always fits).
    Line {
        face: Face,
        ink: Ink,
        line: TextLine,
        x: i32,
        right: i32,
        top: i32,
    },
    /// Single-line runs on one shared baseline, each at its x (texts are
    /// ellipsized to fit when laid out).
    Spans {
        parts: Vec<(Face, Ink, String, i32)>,
        top: i32,
        right: i32,
    },
    Fill {
        r: RECT,
        ink: Ink,
    },
    /// A bordered rounded box (`.set-row` groups, the memory strip).
    Frame {
        r: RECT,
        fill: Ink,
    },
    /// The memory bar: the track plus proportional segments.
    Bar {
        r: RECT,
        parts: Vec<(f32, Ink)>,
    },
    Swatch {
        r: RECT,
        ink: Ink,
    },
    Dot {
        cx: f32,
        cy: f32,
        ink: Ink,
    },
    Switch {
        r: RECT,
        index: usize,
    },
    Button {
        r: RECT,
        target: Target,
        style: ButtonStyle,
        label: String,
        parent: Ink,
    },
    Pill {
        x: i32,
        cy: i32,
        text: String,
    },
    Trefoil {
        x: f32,
        y: f32,
        size: f32,
    },
    /// `.foot`: bg with a top border, the outer bottom corners rounded.
    Foot {
        top: i32,
    },
}

#[derive(Default)]
struct Doc {
    items: Vec<(Region, Item)>,
    /// Hit / focus targets in visual (Tab) order.
    targets: Vec<(Target, Region, RECT)>,
    /// The panel box (device px).
    size: SIZE,
    /// The body viewport (content px).
    body: RECT,
    /// The body's content height.
    content: i32,
    /// Where the results start in the body.
    results: Option<i32>,
}

struct State {
    dpi: i32,
    width: i32,
    options: [bool; 4],
    elevated: bool,
    /// The strip's reading: at opening, then the run's "after".
    memory: Result<MemoryState, String>,
    phase: Phase,
    report: Option<CleanupReport>,
    /// The run could not be started or stopped without a report.
    failure: Option<String>,
    events: Option<Receiver<CleanupEvent>>,
    focus: Target,
    /// `:focus-visible` (keyboard): the ring shows.
    ring: bool,
    hover: Option<Target>,
    pressed: Option<Target>,
    scroller: Scroller<Key>,
    doc: Doc,
    /// The scroll offset painted last (the cursor's hit test).
    offset: Cell<f32>,
}

impl State {
    unsafe fn new(p: *mut App) -> Self {
        let dpi = (*p).dpi;
        let mut client: RECT = zeroed();
        GetClientRect((*p).hwnd, &mut client);
        let width = gfx::pxi(dpi, WIDTH)
            .min(client.right - client.left - gfx::pxi(dpi, 32.0))
            .max(gfx::pxi(dpi, 320.0));
        Self {
            dpi,
            width,
            options: [true, true, false, true],
            elevated: crate::netetw::is_elevated(),
            memory: memclean::memory_state(),
            phase: Phase::Idle,
            report: None,
            failure: None,
            events: None,
            focus: Target::Run,
            ring: false,
            hover: None,
            pressed: None,
            scroller: Scroller::new(SCROLL, BAR, GROW),
            doc: Doc::default(),
            offset: Cell::new(0.0),
        }
    }
    fn cleanup_options(&self) -> CleanupOptions {
        CleanupOptions {
            trim: self.options[TRIM],
            standby: self.options[STANDBY],
            modified: self.options[MODIFIED],
            zombies: self.options[ZOMBIES],
        }
    }
    fn running(&self) -> bool {
        matches!(self.phase, Phase::Running(_))
    }
    /// The UAC prompt is up: the panel cannot close (the run waits for it).
    fn waiting_for_administrator(&self) -> bool {
        self.phase == Phase::Running(Some(Progress::WaitingForAdministrator))
    }
    fn enabled(&self, target: Target) -> bool {
        match target {
            Target::Switch(_) | Target::End(_) => !self.running(),
            Target::Copy => self.report.is_some() && !self.running(),
            Target::Close => !self.waiting_for_administrator(),
            Target::Run => !self.running() && self.cleanup_options().any(),
        }
    }
    fn target_rect(&self, target: Target) -> Option<(Region, RECT)> {
        self.doc
            .targets
            .iter()
            .find(|(t, _, _)| *t == target)
            .map(|&(_, region, r)| (region, r))
    }
    /// The target under a content point at scroll `offset`.
    fn target_at(&self, at: POINT, offset: f32) -> Option<Target> {
        let body = self.doc.body;
        self.doc.targets.iter().find_map(|&(target, region, r)| {
            let r = if region == Region::Body {
                if !contains(&body, at) {
                    return None;
                }
                shift(r, 0, body.top - offset.round() as i32)
            } else {
                r
            };
            contains(&r, at).then_some(target)
        })
    }
    fn focusable(&self) -> Vec<Target> {
        self.doc
            .targets
            .iter()
            .map(|&(target, _, _)| target)
            .filter(|&target| self.enabled(target))
            .collect()
    }
    fn holder(&self, index: usize) -> Option<&ZombieHolder> {
        match self.report.as_ref()?.zombies.as_ref()? {
            Ok(scan) => scan.holders.get(index),
            Err(_) => None,
        }
    }
}

fn contains(r: &RECT, pt: POINT) -> bool {
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

fn shift(r: RECT, dx: i32, dy: i32) -> RECT {
    RECT {
        left: r.left + dx,
        top: r.top + dy,
        right: r.right + dx,
        bottom: r.bottom + dy,
    }
}

// ───────────────────────────── layout ─────────────────────────────

/// A memory DC for measuring text (no window needed).
struct MeasureDc(HDC);
impl MeasureDc {
    unsafe fn new() -> Self {
        Self(CreateCompatibleDC(null_mut()))
    }
}
impl Drop for MeasureDc {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                DeleteDC(self.0);
            }
        }
    }
}

struct Builder<'a> {
    pt: Painter<'a>,
    doc: Doc,
}

impl Builder<'_> {
    fn px(&self, dip: f32) -> f32 {
        self.pt.px(dip)
    }
    fn pxi(&self, dip: f32) -> i32 {
        self.pt.pxi(dip)
    }
    fn push(&mut self, region: Region, item: Item) {
        self.doc.items.push((region, item));
    }
    fn target(&mut self, target: Target, region: Region, r: RECT) {
        self.doc.targets.push((target, region, r));
    }
    fn width(&self, face: Face, text: &str) -> i32 {
        self.pt.measure(face.font(self.pt.fonts), text).cx
    }
    /// Wrap `text` into `width` from `top` (px); returns the next top.
    #[allow(clippy::too_many_arguments)]
    unsafe fn paragraph(
        &mut self,
        region: Region,
        face: Face,
        ink: Ink,
        text: &str,
        x: i32,
        width: i32,
        top: f32,
    ) -> f32 {
        let font = face.font(self.pt.fonts);
        let mut y = top;
        for line in popup::wrap(self.pt.dc, font, text, width.max(1)) {
            let line = popup::text_line(self.pt.dc, font, line, y.round() as i32);
            self.push(
                region,
                Item::Line {
                    face,
                    ink,
                    line,
                    x,
                    right: x + width,
                    top: y.round() as i32,
                },
            );
            y += self.px(face.line());
        }
        y
    }
    /// One line of runs placed left to right from `x` (8 px apart when
    /// `gap`), the last one ellipsized to `right`.
    fn spans(
        &mut self,
        region: Region,
        runs: &[(Face, Ink, &str)],
        x: i32,
        right: i32,
        top: f32,
        gap: f32,
    ) {
        let mut x = x;
        let mut parts = Vec::with_capacity(runs.len());
        for (i, &(face, ink, text)) in runs.iter().enumerate() {
            if i > 0 {
                x += self.pxi(gap);
            }
            let font = face.font(self.pt.fonts);
            let text = self.pt.ellipsize(font, text, (right - x).max(0));
            let w = self.width(face, &text);
            parts.push((face, ink, text, x));
            x += w;
        }
        self.push(
            region,
            Item::Spans {
                parts,
                top: top.round() as i32,
                right,
            },
        );
    }
    /// A `.btn` sized to its label (never ellipsized).
    fn button_width(&self, style: ButtonStyle, text: &str) -> i32 {
        let face = if style == ButtonStyle::Primary {
            Face::UiStrong
        } else {
            Face::Ui
        };
        self.width(face, text) + 2 * self.pxi(14.0) + 2 * self.pt.hair() as i32
    }
}

/// Build the display list and geometry for `s` at most `max_height` tall.
unsafe fn build(s: &State, fonts: &fonts::Fonts, max_height: i32) -> Doc {
    let dc = MeasureDc::new();
    let mut b = Builder {
        pt: Painter::new(dc.0, s.dpi, fonts),
        doc: Doc::default(),
    };
    let hair = b.pt.hair() as i32;
    let w = s.width;
    let left = hair + b.pxi(24.0);
    let right = w - hair - b.pxi(24.0);
    let text_width = (right - left).max(b.pxi(120.0));

    // Header: trefoil + h2, subtitle.
    let mut y = hair as f32 + b.px(22.0);
    let icon = b.px(20.0).round();
    b.push(
        Region::Header,
        Item::Trefoil {
            x: left as f32,
            y: (y + (b.px(Face::Title.line()) - icon) / 2.0).round(),
            size: icon,
        },
    );
    b.spans(
        Region::Header,
        &[(Face::Title, Ink::Fg, "Nuclear Zombie")],
        left + icon as i32 + b.pxi(10.0),
        right,
        y,
        0.0,
    );
    y += b.px(Face::Title.line()) + b.px(4.0);
    y = b.paragraph(
        Region::Header,
        Face::Ui,
        Ink::Muted,
        subtitle(),
        left,
        text_width,
        y,
    );
    let header = (y + b.px(16.0)).round() as i32;

    // Body: note, memory strip, results, options.
    let mut y = b.paragraph(
        Region::Body,
        Face::Small,
        Ink::Muted,
        note(),
        left,
        text_width,
        0.0,
    );
    y += b.px(16.0);
    y = strip(&mut b, s, left, right, y);
    y += b.px(16.0);
    if s.report.is_some() || s.failure.is_some() {
        b.doc.results = Some(y.round() as i32);
        y = results(&mut b, s, left, right, y);
        y += b.px(20.0);
    }
    y = options(&mut b, s, left, right, y);
    let content = (y + b.px(20.0)).ceil() as i32;

    // Footer: [Copy details]   [Close] [Run].
    let button_h = b.pxi(32.0);
    let foot_h = hair + b.pxi(14.0) + button_h + b.pxi(14.0) + hair;
    let room = (max_height - header - foot_h).max(b.pxi(96.0));
    let view = content.min(room);
    b.doc.body = RECT {
        left: hair,
        top: header,
        right: w - hair,
        bottom: header + view,
    };
    b.doc.content = content;
    let foot = header + view;
    b.push(Region::Footer, Item::Foot { top: foot });
    let top = foot + hair + b.pxi(14.0);
    let run_label = match s.phase {
        Phase::Running(progress) => progress_text(progress),
        Phase::Done => tr("다시 실행", "Run again").into(),
        Phase::Idle => tr("실행", "Run").into(),
    };
    // Right-aligned [Close] [Run], [Copy details] at the left once there is
    // a report; pushed left to right (the Tab order is the visual order).
    let mut buttons = Vec::with_capacity(3);
    let mut x = right;
    for (target, style, label) in [
        (Target::Run, ButtonStyle::Primary, run_label),
        (
            Target::Close,
            ButtonStyle::Default,
            tr("닫기", "Close").to_owned(),
        ),
    ] {
        let width = b.button_width(style, &label);
        let r = RECT {
            left: x - width,
            top,
            right: x,
            bottom: top + button_h,
        };
        x = r.left - b.pxi(8.0);
        buttons.push((target, style, label, r));
    }
    if s.report.is_some() {
        let label = tr("세부 정보 복사", "Copy details").to_owned();
        let width = b.button_width(ButtonStyle::Default, &label);
        let r = RECT {
            left,
            top,
            right: left + width,
            bottom: top + button_h,
        };
        buttons.push((Target::Copy, ButtonStyle::Default, label, r));
    }
    buttons.sort_by_key(|button| button.3.left);
    for (target, style, label, r) in buttons {
        b.target(target, Region::Footer, r);
        b.push(
            Region::Footer,
            Item::Button {
                r,
                target,
                style,
                label,
                parent: Ink::Bg,
            },
        );
    }
    b.doc.size = SIZE {
        cx: w,
        cy: foot + foot_h,
    };
    b.doc
}

/// The memory strip: "Memory · 31.7 GB total", the proportional bar (in use,
/// modified, standby; the rest is free) and the five readings.
unsafe fn strip(b: &mut Builder, s: &State, left: i32, right: i32, top: f32) -> f32 {
    let at = b.doc.items.len();
    let pad_x = b.pxi(16.0);
    let (x, inner_right) = (left + pad_x, right - pad_x);
    let mut y = top + b.px(12.0);
    match &s.memory {
        Ok(memory) => {
            let total = tf!("전체 {}", "{} total", bytes(memory.total));
            let total_x = inner_right - b.width(Face::MonoSmall, &total);
            b.push(
                Region::Body,
                Item::Spans {
                    parts: vec![
                        (Face::UiStrong, Ink::Fg, tr("메모리", "Memory").into(), x),
                        (Face::MonoSmall, Ink::Muted, total, total_x),
                    ],
                    top: y.round() as i32,
                    right: inner_right,
                },
            );
            y += b.px(Face::Ui.line()) + b.px(8.0);
            let scale = memory.total.max(1) as f32;
            let in_use = memory.in_use();
            let mut parts = Vec::new();
            match memory.lists {
                Some(lists) => {
                    let modified = lists.modified.min(in_use);
                    parts.push(((in_use - modified) as f32 / scale, Ink::Fg));
                    parts.push((modified as f32 / scale, Ink::Warn));
                    parts.push((lists.standby as f32 / scale, Ink::Accent));
                }
                None => parts.push((in_use as f32 / scale, Ink::Fg)),
            }
            let bar = RECT {
                left: x,
                top: y.round() as i32,
                right: inner_right,
                bottom: (y + b.px(8.0)).round() as i32,
            };
            b.push(Region::Body, Item::Bar { r: bar, parts });
            y += b.px(8.0) + b.px(12.0);
            // Five equal columns: label (with the bar's swatch) and value.
            let columns = readings(memory);
            let swatches = [
                None,
                Some(Ink::Fg),
                Some(Ink::Accent),
                Some(Ink::Warn),
                Some(Ink::Border),
            ];
            let column = (inner_right - x) / columns.len() as i32;
            let value_top = y + b.px(Face::Small.line()) + b.px(2.0);
            for (i, ((label, value), swatch)) in columns.iter().zip(swatches).enumerate() {
                let cx = x + column * i as i32;
                let cell_right = cx + column - b.pxi(8.0);
                let mut text_x = cx;
                if let Some(ink) = swatch {
                    let size = b.pxi(8.0);
                    let mid = (y + b.px(Face::Small.line()) / 2.0).round() as i32;
                    b.push(
                        Region::Body,
                        Item::Swatch {
                            r: RECT {
                                left: cx,
                                top: mid - size / 2,
                                right: cx + size,
                                bottom: mid - size / 2 + size,
                            },
                            ink,
                        },
                    );
                    text_x += size + b.pxi(6.0);
                }
                b.spans(
                    Region::Body,
                    &[(Face::Small, Ink::Muted, label)],
                    text_x,
                    cell_right,
                    y,
                    0.0,
                );
                let value = value.map_or_else(|| "\u{2014}".to_owned(), bytes);
                b.spans(
                    Region::Body,
                    &[(Face::MonoTotal, Ink::Fg, &value)],
                    cx,
                    cell_right,
                    value_top,
                    0.0,
                );
            }
            y = value_top + b.px(Face::MonoTotal.line());
        }
        Err(error) => {
            y = b.paragraph(
                Region::Body,
                Face::Small,
                Ink::Danger,
                error,
                x,
                inner_right - x,
                y,
            );
        }
    }
    y += b.px(12.0);
    b.doc.items.insert(
        at,
        (
            Region::Body,
            Item::Frame {
                r: RECT {
                    left,
                    top: top.round() as i32,
                    right,
                    bottom: y.round() as i32,
                },
                fill: Ink::Bg,
            },
        ),
    );
    y
}

/// The run's results: available / free before → after, each step's
/// outcome, and the zombie report with a per-holder End task.
unsafe fn results(b: &mut Builder, s: &State, left: i32, right: i32, top: f32) -> f32 {
    let width = right - left;
    let hair = b.pt.hair() as i32;
    let mut y = top;
    if let Some(failure) = &s.failure {
        return b.paragraph(Region::Body, Face::Ui, Ink::Danger, failure, left, width, y);
    }
    let Some(report) = &s.report else {
        return y;
    };
    b.spans(
        Region::Body,
        &[(Face::UiStrong, Ink::Fg, tr("결과", "Results"))],
        left,
        right,
        y,
        0.0,
    );
    y += b.px(Face::Ui.line()) + b.px(8.0);
    match (&report.before, &report.after) {
        (Ok(before), Ok(after)) => {
            let mut rows = vec![(
                tr("사용 가능", "Available"),
                before.available,
                after.available,
            )];
            if let (Some(a), Some(z)) = (before.lists, after.lists) {
                rows.push((tr("비어 있음", "Free"), a.free, z.free));
            }
            let label_w = b.pxi(100.0);
            for (label, a, z) in rows {
                let change = delta(a, z);
                let value = format!("{} \u{2192} {}", bytes(a), bytes(z));
                let ink = if z > a { Ink::Accent } else { Ink::Muted };
                let value_x = left + label_w;
                let change_x = value_x + b.width(Face::MonoTotal, &value) + b.pxi(12.0);
                b.push(
                    Region::Body,
                    Item::Spans {
                        parts: vec![
                            (Face::Ui, Ink::Muted, label.into(), left),
                            (Face::MonoTotal, Ink::Fg, value, value_x),
                            (Face::MonoTotal, ink, change, change_x),
                        ],
                        top: y.round() as i32,
                        right,
                    },
                );
                y += b.px(Face::Ui.line()) + b.px(4.0);
            }
        }
        (Err(error), _) | (_, Err(error)) => {
            y = b.paragraph(
                Region::Body,
                Face::Small,
                Ink::Danger,
                error,
                left,
                width,
                y,
            );
        }
    }
    y += b.px(8.0);
    // Step outcomes: name | ● outcome (wrapped), row borders between.
    let name_w = b.pxi(170.0);
    let text_x = left + name_w + b.pxi(14.0);
    for (index, status, text) in steps(report) {
        let row_top = y;
        b.push(
            Region::Body,
            Item::Fill {
                r: RECT {
                    left,
                    top: row_top.round() as i32,
                    right,
                    bottom: row_top.round() as i32 + hair,
                },
                ink: Ink::RowBorder,
            },
        );
        let line_top = row_top + b.px(7.0);
        b.spans(
            Region::Body,
            &[(Face::Ui, Ink::Fg, option_title(index))],
            left,
            left + name_w - b.pxi(8.0),
            line_top,
            0.0,
        );
        b.push(
            Region::Body,
            Item::Dot {
                cx: (left + name_w) as f32 + b.px(3.0),
                cy: line_top + b.px(Face::Ui.line()) / 2.0,
                ink: status.dot(),
            },
        );
        y = b.paragraph(
            Region::Body,
            Face::Ui,
            status.ink(),
            &text,
            text_x,
            right - text_x,
            line_top,
        );
        y += b.px(7.0);
    }
    b.push(
        Region::Body,
        Item::Fill {
            r: RECT {
                left,
                top: y.round() as i32,
                right,
                bottom: y.round() as i32 + hair,
            },
            ink: Ink::RowBorder,
        },
    );
    let Some(zombies) = &report.zombies else {
        return y;
    };
    y += b.px(20.0);
    b.spans(
        Region::Body,
        &[(
            Face::UiStrong,
            Ink::Fg,
            tr("좀비 프로세스", "Zombie processes"),
        )],
        left,
        right,
        y,
        0.0,
    );
    y += b.px(Face::Ui.line()) + b.px(2.0);
    let scan = match zombies {
        Ok(scan) => scan,
        Err(error) => {
            return b.paragraph(
                Region::Body,
                Face::Small,
                Ink::Danger,
                error,
                left,
                width,
                y,
            );
        }
    };
    y = b.paragraph(
        Region::Body,
        Face::Small,
        Ink::Muted,
        &scan_summary(scan),
        left,
        width,
        y,
    );
    if !scan.holders.is_empty() {
        y += b.px(8.0);
        let end = tr("작업 끝내기", "End task");
        let button_w = b.button_width(ButtonStyle::Default, end);
        let button_h = b.pxi(28.0);
        for (i, holder) in scan.holders.iter().take(HOLDERS_SHOWN).enumerate() {
            let row_top = y;
            b.push(
                Region::Body,
                Item::Fill {
                    r: RECT {
                        left,
                        top: row_top.round() as i32,
                        right,
                        bottom: row_top.round() as i32 + hair,
                    },
                    ink: Ink::RowBorder,
                },
            );
            let row_h = b.px(8.0) + b.px(Face::Ui.line()) + b.px(Face::Small.line()) + b.px(8.0);
            let button_top = (row_top + (row_h - button_h as f32) / 2.0).round() as i32;
            let button = RECT {
                left: right - button_w,
                top: button_top,
                right,
                bottom: button_top + button_h,
            };
            let text_right = button.left - b.pxi(12.0);
            let pid = format!("PID {}", holder.pid);
            b.spans(
                Region::Body,
                &[
                    (Face::UiStrong, Ink::Fg, &holder.name),
                    (Face::MonoSmall, Ink::Muted, &pid),
                ],
                left,
                text_right,
                row_top + b.px(8.0),
                8.0,
            );
            b.spans(
                Region::Body,
                &[(Face::Small, Ink::Muted, &holder_detail(holder))],
                left,
                text_right,
                row_top + b.px(8.0) + b.px(Face::Ui.line()),
                0.0,
            );
            b.target(Target::End(i), Region::Body, button);
            b.push(
                Region::Body,
                Item::Button {
                    r: button,
                    target: Target::End(i),
                    style: ButtonStyle::Default,
                    label: end.into(),
                    parent: Ink::Surface,
                },
            );
            y = row_top + row_h;
        }
        b.push(
            Region::Body,
            Item::Fill {
                r: RECT {
                    left,
                    top: y.round() as i32,
                    right,
                    bottom: y.round() as i32 + hair,
                },
                ink: Ink::RowBorder,
            },
        );
        let hidden = scan.holders.len().saturating_sub(HOLDERS_SHOWN);
        if hidden > 0 {
            y += b.px(6.0);
            y = b.paragraph(
                Region::Body,
                Face::Small,
                Ink::Muted,
                &tf!(
                    "외 프로그램 {}개(세부 정보 복사에 모두 포함)",
                    "{} more programs (Copy details lists all)",
                    grouped(hidden)
                ),
                left,
                width,
                y,
            );
        }
    }
    if let Some(partial) = partial_text(scan, report.elevated) {
        y += b.px(10.0);
        y = b.paragraph(
            Region::Body,
            Face::Small,
            Ink::Warn,
            &partial,
            left,
            width,
            y,
        );
    }
    if scan.total > 0 || scan.partial() {
        y += b.px(8.0);
        y = b.paragraph(
            Region::Body,
            Face::Small,
            Ink::Muted,
            explanation(),
            left,
            width,
            y,
        );
    }
    y
}

/// The option rows: joined `.set-row`s (bg, 1 px border, radius 4) with a
/// 13/600 title, an "administrator" pill where UAC is needed, a 12 px muted
/// description and the switch at the right.
unsafe fn options(b: &mut Builder, s: &State, left: i32, right: i32, top: f32) -> f32 {
    let at = b.doc.items.len();
    let hair = b.pt.hair() as i32;
    let pad_x = b.pxi(16.0);
    let x = left + pad_x;
    let switch_left = right - pad_x - b.pxi(40.0);
    let text_right = switch_left - b.pxi(16.0);
    let mut y = top;
    for index in 0..s.options.len() {
        let row_top = y;
        if index > 0 {
            b.push(
                Region::Body,
                Item::Fill {
                    r: RECT {
                        left: left + hair,
                        top: row_top.round() as i32,
                        right: right - hair,
                        bottom: row_top.round() as i32 + hair,
                    },
                    ink: Ink::Border,
                },
            );
        }
        let title_top = row_top + b.px(12.0);
        let title = option_title(index);
        b.spans(
            Region::Body,
            &[(Face::UiStrong, Ink::Fg, title)],
            x,
            text_right,
            title_top,
            0.0,
        );
        if needs_admin(index) && !s.elevated {
            let pill_x = x + b.width(Face::UiStrong, title) + b.pxi(8.0);
            b.push(
                Region::Body,
                Item::Pill {
                    x: pill_x,
                    cy: (title_top + b.px(Face::Ui.line()) / 2.0).round() as i32,
                    text: tr("관리자 권한 필요", "Needs administrator").into(),
                },
            );
        }
        y = b.paragraph(
            Region::Body,
            Face::Small,
            Ink::Muted,
            option_description(index),
            x,
            text_right - x,
            title_top + b.px(Face::Ui.line()) + b.px(2.0),
        );
        y += b.px(12.0);
        let row = RECT {
            left,
            top: row_top.round() as i32,
            right,
            bottom: y.round() as i32,
        };
        let switch_h = b.pxi(20.0);
        let switch_top = (row.top + row.bottom - switch_h) / 2;
        b.push(
            Region::Body,
            Item::Switch {
                r: RECT {
                    left: switch_left,
                    top: switch_top,
                    right: switch_left + b.pxi(40.0),
                    bottom: switch_top + switch_h,
                },
                index,
            },
        );
        b.target(Target::Switch(index), Region::Body, row);
    }
    b.doc.items.insert(
        at,
        (
            Region::Body,
            Item::Frame {
                r: RECT {
                    left,
                    top: top.round() as i32,
                    right,
                    bottom: y.round() as i32,
                },
                fill: Ink::Bg,
            },
        ),
    );
    y
}

// ───────────────────────────── painting ─────────────────────────────

/// The popup's content: the shared state, painted from its display list.
struct View(Rc<RefCell<State>>);

impl PanelContent for View {
    fn paint(&self, pt: &Painter, content: RECT, anim: &anim::AnimHost<Key>) {
        if let Ok(s) = self.0.try_borrow() {
            unsafe { paint(&s, pt, content, anim) };
        }
    }
    fn pointer(&self, at: POINT) -> bool {
        self.0.try_borrow().is_ok_and(|s| {
            s.target_at(at, s.offset.get())
                .is_some_and(|target| s.enabled(target))
        })
    }
}

unsafe fn paint(s: &State, pt: &Painter, content: RECT, anim: &anim::AnimHost<Key>) {
    let offset = s.scroller.offset(anim).round();
    s.offset.set(offset);
    let body = shift(s.doc.body, content.left, content.top);
    // The body, clipped to its viewport (GDI text and GDI+ shapes).
    pt.canvas.flush();
    let saved = SaveDC(pt.dc);
    IntersectClipRect(pt.dc, body.left, body.top, body.right, body.bottom);
    let clip = pt.canvas.save();
    pt.canvas.set_clip(RectF::from_rect(body));
    let body_dy = body.top - offset as i32;
    for (region, item) in &s.doc.items {
        if *region == Region::Body {
            draw(s, pt, anim, item, content, content.left, body_dy);
        }
    }
    pt.canvas.flush();
    pt.canvas.restore(clip);
    RestoreDC(pt.dc, saved);
    s.scroller.paint(pt, anim, scroll::lane(body, s.dpi));
    // Scrolled content passes under the header: a divider marks the edge.
    if offset > 0.0 {
        pt.fill(
            RECT {
                bottom: body.top + pt.hair() as i32,
                ..body
            },
            pt.c.border,
        );
    }
    for (region, item) in &s.doc.items {
        if *region != Region::Body {
            draw(s, pt, anim, item, content, content.left, content.top);
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw(
    s: &State,
    pt: &Painter,
    anim: &anim::AnimHost<Key>,
    item: &Item,
    content: RECT,
    dx: i32,
    dy: i32,
) {
    let c = &pt.c;
    let f = pt.fonts;
    match item {
        Item::Line {
            face,
            ink,
            line,
            x,
            right,
            top,
        } => {
            let font = face.font(f);
            let band = RECT {
                left: x + dx,
                top: top + dy,
                right: right + dx,
                bottom: top + dy + pt.pxi(face.line()),
            };
            let cell = pt.css_rect(font, band, Some(face.line()));
            popup::draw_line(pt, font, ink.color(c), line, cell);
        }
        Item::Spans { parts, top, right } => {
            let line = parts.iter().map(|p| p.0.line()).fold(0.0, f32::max);
            let band = RECT {
                left: dx,
                top: top + dy,
                right: right + dx,
                bottom: top + dy + pt.pxi(line),
            };
            let group: Vec<HFONT> = parts.iter().map(|p| p.0.font(f)).collect();
            let baseline = pt.css_mixed_baseline(band, &group, line);
            for (face, ink, text, x) in parts {
                pt.text_on_baseline(
                    face.font(f),
                    ink.color(c),
                    text,
                    RECT {
                        left: x + dx,
                        ..band
                    },
                    baseline,
                    DT_LEFT,
                );
            }
        }
        Item::Fill { r, ink } => pt.fill(shift(*r, dx, dy), ink.color(c)),
        Item::Frame { r, fill } => pt.canvas.bordered_round_rect(
            RectF::from_rect(shift(*r, dx, dy)),
            Radii::all(pt.px(theme::RADIUS_SM)),
            pt.hair(),
            solid(fill.color(c)),
            solid(c.border),
        ),
        Item::Bar { r, parts } => {
            let r = RectF::from_rect(shift(*r, dx, dy));
            let radius = r.h / 2.0;
            pt.canvas.fill_round_rect(r, radius, solid(c.border));
            let state = pt.canvas.save();
            pt.canvas.clip_round_rect(r, Radii::all(radius));
            let mut x = r.x;
            for &(fraction, ink) in parts {
                let w = r.w * fraction.clamp(0.0, 1.0);
                if w > 0.0 {
                    pt.canvas.fill_rect(
                        RectF::new(x, r.y, w.min(r.right() - x), r.h),
                        solid(ink.color(c)),
                    );
                }
                x += w;
            }
            pt.canvas.restore(state);
        }
        Item::Swatch { r, ink } => pt.canvas.fill_round_rect(
            RectF::from_rect(shift(*r, dx, dy)),
            pt.px(2.0),
            solid(ink.color(c)),
        ),
        Item::Dot { cx, cy, ink } => {
            widgets::status_dot(pt, cx + dx as f32, cy + dy as f32, ink.color(c))
        }
        Item::Switch { r, index } => {
            let r = shift(*r, dx, dy);
            let on = s.options[*index];
            let target = Target::Switch(*index);
            let t = anim.value_or(switch_key(*index), on as u8 as f32);
            widgets::switch_to(pt, r, t, on, s.enabled(target), false, c.bg);
            if s.ring && s.focus == target {
                widgets::focus_ring(pt, r, pt.px(theme::RADIUS_PILL));
            }
        }
        Item::Button {
            r,
            target,
            style,
            label,
            parent,
        } => {
            let r = shift(*r, dx, dy);
            // A running Run button keeps its face and shows the progress.
            let busy_run = *target == Target::Run && s.running();
            let enabled = s.enabled(*target);
            widgets::button_face(
                pt,
                r,
                *style,
                &ButtonState {
                    hover: if enabled {
                        anim.value(target.hover())
                    } else {
                        0.0
                    },
                    pressed: enabled && s.pressed == Some(*target),
                    disabled: !enabled && !busy_run,
                    ..ButtonState::default()
                },
                label,
                parent.color(c),
            );
            if s.ring && s.focus == *target && enabled {
                widgets::focus_ring(pt, r, pt.px(theme::RADIUS_SM));
            }
        }
        Item::Pill { x, cy, text } => {
            widgets::pill(pt, x + dx, cy + dy, PillKind::Default, text, false);
        }
        Item::Trefoil { x, y, size } => {
            widgets::radiation(pt, x + dx as f32, y + dy as f32, *size, c.fg)
        }
        Item::Foot { top } => {
            let hair = pt.hair();
            let top = top + dy;
            let radius = pt.px(theme::RADIUS);
            pt.canvas.fill_round_rect_corners(
                RectF::ltrb(
                    content.left as f32 + hair,
                    top as f32,
                    content.right as f32 - hair,
                    content.bottom as f32 - hair,
                ),
                Radii::bottom((radius - hair).max(0.0)),
                solid(c.bg),
            );
            pt.fill(
                RECT {
                    left: content.left + hair as i32,
                    top,
                    right: content.right - hair as i32,
                    bottom: top + hair as i32,
                },
                c.border,
            );
        }
    }
}

// ───────────────────────────── the modal panel ─────────────────────────────

/// An open panel: the shared state and its two popups.
struct Session {
    p: *mut App,
    state: Rc<RefCell<State>>,
    scrim: HWND,
    panel: HWND,
}

impl Session {
    unsafe fn host<'a>(&self) -> Option<&'a mut anim::AnimHost<Key>> {
        popup::popup_anim(self.panel)
    }
    unsafe fn refresh(&self) {
        popup::repaint_panel(self.panel);
    }
    unsafe fn dpi(&self) -> i32 {
        self.state.borrow().dpi
    }
    /// Rebuild the display list (content changed or the owner resized),
    /// resize the panel to it and recompose.
    unsafe fn relayout(&self) {
        let p = self.p;
        let doc = build(
            &self.state.borrow(),
            &(*p).fonts,
            popup::panel_max_height(p),
        );
        let extent = Extent::new(doc.content as f32, (doc.body.bottom - doc.body.top) as f32);
        let height = doc.size.cy;
        self.state.borrow_mut().doc = doc;
        if let Some(host) = self.host() {
            self.state.borrow_mut().scroller.set_extent(host, extent);
        }
        popup::resize_panel(p, self.panel, height);
    }
    /// The body lane of the overlay scrollbar (content px).
    unsafe fn lane(&self) -> RECT {
        scroll::lane(self.state.borrow().doc.body, self.dpi())
    }
    /// A screen point relative to the panel's content box.
    unsafe fn local(&self, screen: POINT) -> Option<POINT> {
        let content = popup::content_screen(self.panel)?;
        Some(POINT {
            x: screen.x - content.left,
            y: screen.y - content.top,
        })
    }
    unsafe fn offset(&self) -> f32 {
        match self.host() {
            Some(host) => self.state.borrow().scroller.offset(host),
            None => 0.0,
        }
    }
    unsafe fn target_at(&self, local: POINT) -> Option<Target> {
        let offset = self.offset();
        self.state.borrow().target_at(local, offset)
    }
    unsafe fn can_close(&self) -> bool {
        !self.state.borrow().waiting_for_administrator()
    }
    unsafe fn toggle(&self, index: usize) {
        let on = {
            let mut s = self.state.borrow_mut();
            if s.running() {
                return;
            }
            s.options[index] = !s.options[index];
            s.options[index]
        };
        if let Some(host) = self.host() {
            host.set_target(
                switch_key(index),
                on as u8 as f32,
                anim::motion::SWITCH,
                anim::Easing::Ease,
            );
        }
        self.refresh();
    }
    /// Hover cross-fades on the buttons.
    unsafe fn set_hover(&self, hot: Option<Target>) {
        let old = {
            let mut s = self.state.borrow_mut();
            let hot = hot.filter(|&t| t.is_button() && s.enabled(t));
            if s.hover == hot {
                return;
            }
            let old = std::mem::replace(&mut s.hover, hot);
            (old, hot)
        };
        if let Some(host) = self.host() {
            if let Some(t) = old.0 {
                host.set_target(
                    t.hover(),
                    0.0,
                    anim::motion::HOVER_OUT,
                    anim::Easing::EaseOut,
                );
            }
            if let Some(t) = old.1 {
                host.set_target(
                    t.hover(),
                    1.0,
                    anim::motion::HOVER_IN,
                    anim::Easing::EaseOut,
                );
            }
        }
        self.refresh();
    }
    /// Keep a body target in view (keyboard focus).
    unsafe fn reveal(&self, target: Target) {
        let Some((Region::Body, r)) = self.state.borrow().target_rect(target) else {
            return;
        };
        let pad = gfx::px(self.dpi(), 8.0);
        if let Some(host) = self.host() {
            self.state.borrow_mut().scroller.reveal(
                host,
                r.top as f32 - pad,
                r.bottom as f32 + pad,
                true,
            );
        }
    }
    unsafe fn move_focus(&self, forward: bool) {
        let next = {
            let mut s = self.state.borrow_mut();
            let order = s.focusable();
            if order.is_empty() {
                return;
            }
            let n = order.len();
            let next = match order.iter().position(|&t| t == s.focus) {
                Some(i) if forward => order[(i + 1) % n],
                Some(i) => order[(i + n - 1) % n],
                None if forward => order[0],
                None => order[n - 1],
            };
            s.focus = next;
            s.ring = true;
            next
        };
        self.reveal(next);
        self.refresh();
    }
    unsafe fn scroll_to(&self, offset: f32, animate: bool) {
        if let Some(host) = self.host() {
            let duration = animate.then_some(anim::motion::SCROLL);
            self.state
                .borrow_mut()
                .scroller
                .scroll_to(host, offset, duration);
        }
        self.refresh();
    }
    /// Start a run on the job worker.
    unsafe fn start(&self) {
        let p = self.p;
        let options = {
            let s = self.state.borrow();
            if !s.enabled(Target::Run) {
                return;
            }
            s.cleanup_options()
        };
        let (events, receiver) = mpsc::channel();
        let sent = (*p).jobs.send(Job::Cleanup(options, events)).is_ok();
        {
            let mut s = self.state.borrow_mut();
            s.report = None;
            s.failure = None;
            s.pressed = None;
            if sent {
                s.phase = Phase::Running(None);
                s.events = Some(receiver);
            } else {
                s.phase = Phase::Done;
                s.failure = Some(
                    tr(
                        "관리 작업을 실행할 수 없습니다.",
                        "Unable to run the management action.",
                    )
                    .into(),
                );
            }
        }
        if sent {
            // An action runs: the page's actions wait for it (like others).
            (*p).busy = true;
            (*p).clear_error();
            (*p).notice.clear();
            update_buttons(p);
        }
        self.set_hover(None);
        self.relayout();
        self.scroll_to(0.0, true);
    }
    /// Progress and the report from the worker.
    unsafe fn drain(&self) {
        let mut changed = false;
        let mut finished = false;
        loop {
            let event = match self.state.borrow().events.as_ref() {
                Some(rx) => rx.try_recv(),
                None => break,
            };
            let mut s = self.state.borrow_mut();
            match event {
                Ok(CleanupEvent::Progress(progress)) => {
                    if s.running() {
                        s.phase = Phase::Running(Some(progress));
                        changed = true;
                    }
                }
                Ok(CleanupEvent::Done(report)) => {
                    if let Ok(after) = &report.after {
                        s.memory = Ok(*after);
                    }
                    s.elevated = report.elevated;
                    s.report = Some(*report);
                    s.phase = Phase::Done;
                    s.events = None;
                    (changed, finished) = (true, true);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    s.phase = Phase::Done;
                    s.events = None;
                    s.failure = Some(stopped().into());
                    (changed, finished) = (true, true);
                }
            }
        }
        if finished {
            (*self.p).busy = false;
            update_buttons(self.p);
        }
        if changed {
            self.relayout();
            if finished {
                // The results come into view (the options stay below).
                let top = self.state.borrow().doc.results.unwrap_or(0);
                self.scroll_to(top as f32, true);
            }
            self.refresh();
        }
    }
    /// Run a focused / clicked target. Some(outcome) ends the panel.
    unsafe fn activate(&self, target: Target) -> Option<Outcome> {
        if !self.state.borrow().enabled(target) {
            return None;
        }
        match target {
            Target::Switch(index) => self.toggle(index),
            Target::Run => self.start(),
            Target::Close => return Some(Outcome::Closed),
            Target::Copy => {
                let text = self.state.borrow().report.as_ref().map(details);
                if let Some(text) = text {
                    interactions::report_copy(self.p, &text);
                }
            }
            Target::End(index) => {
                let s = self.state.borrow();
                let holder = s.holder(index)?;
                return Some(Outcome::End(
                    holder.name.clone(),
                    holder.pid,
                    holder.created,
                    holder.zombies,
                ));
            }
        }
        None
    }
}

/// Stage the panel for `state` without running its loop (the loop and the
/// previews use it).
unsafe fn stage(p: *mut App, state: &Rc<RefCell<State>>) -> Option<Session> {
    let doc = build(&state.borrow(), &(*p).fonts, popup::panel_max_height(p));
    let size = doc.size;
    state.borrow_mut().doc = doc;
    let (scrim, panel) = popup::stage_panel(p, size.cx, size.cy, Box::new(View(state.clone())))?;
    let session = Session {
        p,
        state: state.clone(),
        scrim,
        panel,
    };
    if let Some(host) = session.host() {
        let mut s = state.borrow_mut();
        for index in 0..s.options.len() {
            host.set(switch_key(index), s.options[index] as u8 as f32);
        }
        let extent = Extent::new(
            s.doc.content as f32,
            (s.doc.body.bottom - s.doc.body.top) as f32,
        );
        s.scroller.set_extent(host, extent);
    }
    session.refresh();
    Some(session)
}

/// Open the panel (the Processes head button, the palette): modal like the
/// confirm dialog — sampling pauses, queued notifications are re-posted and
/// the keyboard focus comes back afterwards.
pub(super) unsafe fn open(p: *mut App) {
    if (*p).busy || (*p).modal || (*p).hwnd.is_null() {
        return;
    }
    let focus = GetFocus();
    (*p).modal = true;
    update_buttons(p);
    park_focus(p);
    configure(p);
    let outcome = run(p);
    interactions::finish_modal(p);
    restore_focus(p, focus);
    redraw(p);
    if let Outcome::End(name, pid, created, zombies) = outcome {
        // The app's own End task confirmation, never automatic.
        let detail = tf!(
            "이 프로그램이 {}를 붙잡고 있습니다. 프로그램을 종료하면 해제됩니다.",
            "It holds {} open; ending it releases them.",
            exited(zombies)
        );
        end_task_flow(p, &name, pid, created, Some(&detail));
    }
}

/// The panel's modal loop.
unsafe fn run(p: *mut App) -> Outcome {
    let owner = (*p).hwnd;
    let state = Rc::new(RefCell::new(State::new(p)));
    // `:focus-visible` after a keyboard trigger or with keyboard cues shown.
    state.borrow_mut().ring = !controls::cues_hidden(owner)
        || [VK_RETURN, VK_SPACE]
            .iter()
            .any(|&vk| GetKeyState(vk as i32) < 0);
    let Some(session) = stage(p, &state) else {
        return Outcome::Closed;
    };
    let (scrim, panel) = (session.scrim, session.panel);
    let geometry = || unsafe {
        let (mut window, mut client): (RECT, RECT) = (zeroed(), zeroed());
        GetWindowRect(owner, &mut window);
        GetClientRect(owner, &mut client);
        [
            window.left,
            window.top,
            window.right,
            window.bottom,
            client.right,
            client.bottom,
        ]
    };
    let mut placed = geometry();
    let mut outcome = None;
    let mut tracking = false;
    let line = gfx::px(session.dpi(), LINE);
    let min_thumb = gfx::px(session.dpi(), widgets::SCROLL_THUMB_MIN);
    popup::run_modal(
        |msg| {
            // The owner moved or resized: the scrim follows, the panel
            // re-centres and fits its height.
            if IsIconic(owner) == 0 {
                let now = geometry();
                if now != placed {
                    placed = now;
                    popup::refit_confirm(p, scrim, panel);
                    session.relayout();
                }
            }
            if msg.hwnd == owner && msg.message == CLEANUP_READY {
                session.drain();
                return Step::Consumed;
            }
            let point = || unsafe { session.local(popup::message_point(msg)) };
            match msg.message {
                WM_KEYDOWN | WM_SYSKEYDOWN => {
                    let vk = msg.wParam as u16;
                    let shift = GetKeyState(VK_SHIFT as i32) < 0;
                    let ctrl = GetKeyState(VK_CONTROL as i32) < 0;
                    let focus = session.state.borrow().focus;
                    let result = match vk {
                        VK_ESCAPE => session.can_close().then_some(Outcome::Closed),
                        VK_F4 if msg.message == WM_SYSKEYDOWN => {
                            session.can_close().then_some(Outcome::Closed)
                        }
                        VK_TAB => {
                            session.move_focus(!shift);
                            None
                        }
                        VK_DOWN | VK_RIGHT => {
                            session.move_focus(true);
                            None
                        }
                        VK_UP | VK_LEFT => {
                            session.move_focus(false);
                            None
                        }
                        VK_SPACE => session.activate(focus),
                        // Enter runs, unless a button has the focus.
                        VK_RETURN if focus.is_button() => session.activate(focus),
                        VK_RETURN => session.activate(Target::Run),
                        VK_PRIOR | VK_NEXT => {
                            if let Some(host) = session.host() {
                                let direction = if vk == VK_PRIOR { -1.0 } else { 1.0 };
                                session
                                    .state
                                    .borrow_mut()
                                    .scroller
                                    .page(host, direction, line);
                            }
                            session.refresh();
                            None
                        }
                        VK_HOME => {
                            session.scroll_to(0.0, true);
                            None
                        }
                        VK_END => {
                            session.scroll_to(f32::MAX, true);
                            None
                        }
                        0x43 if ctrl => session.activate(Target::Copy),
                        _ => None,
                    };
                    if let Some(result) = result {
                        outcome = Some(result);
                        return Step::Done;
                    }
                    Step::Consumed
                }
                WM_KEYUP | WM_SYSKEYUP | WM_CHAR | WM_SYSCHAR | WM_DEADCHAR | WM_SYSDEADCHAR => {
                    Step::Consumed
                }
                WM_MOUSEMOVE => {
                    if msg.hwnd != panel {
                        session.set_hover(None);
                        return Step::Consumed;
                    }
                    if !tracking {
                        let mut event = TRACKMOUSEEVENT {
                            cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: panel,
                            dwHoverTime: 0,
                        };
                        tracking = TrackMouseEvent(&mut event) != 0;
                    }
                    let Some(at) = point() else {
                        return Step::Consumed;
                    };
                    let lane = session.lane();
                    // One animation-host borrow at a time (each helper
                    // fetches its own).
                    let dragging = session.state.borrow().scroller.is_dragging();
                    if dragging {
                        if let Some(host) = session.host() {
                            let mut s = session.state.borrow_mut();
                            if GetCapture() == panel {
                                s.scroller.drag_to(host, lane, at, min_thumb);
                            } else {
                                s.scroller.release(host);
                            }
                        }
                        session.refresh();
                        return Step::Consumed;
                    }
                    let in_lane = match session.host() {
                        Some(host) => {
                            session
                                .state
                                .borrow_mut()
                                .scroller
                                .pointer(host, lane, Some(at))
                        }
                        None => false,
                    };
                    let hot = if in_lane { None } else { session.target_at(at) };
                    session.set_hover(hot);
                    Step::Consumed
                }
                WM_MOUSELEAVE if msg.hwnd == panel => {
                    tracking = false;
                    let lane = session.lane();
                    if let Some(host) = session.host() {
                        session
                            .state
                            .borrow_mut()
                            .scroller
                            .pointer(host, lane, None);
                    }
                    session.set_hover(None);
                    Step::Consumed
                }
                WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                    if msg.hwnd == scrim {
                        // `.scrim` mousedown outside the panel closes it.
                        if session.can_close() {
                            outcome = Some(Outcome::Closed);
                            return Step::Done;
                        }
                        return Step::Consumed;
                    }
                    if msg.hwnd != panel {
                        return Step::Consumed;
                    }
                    let Some(at) = point() else {
                        return Step::Consumed;
                    };
                    session.state.borrow_mut().ring = false;
                    let lane = session.lane();
                    let on_bar = match session.host() {
                        Some(host) => session
                            .state
                            .borrow_mut()
                            .scroller
                            .press(host, lane, at, min_thumb, line),
                        None => false,
                    };
                    if on_bar {
                        SetCapture(panel);
                        session.refresh();
                        return Step::Consumed;
                    }
                    let hit = session.target_at(at);
                    if let Some(target) = hit.filter(|&t| session.state.borrow().enabled(t)) {
                        {
                            let mut s = session.state.borrow_mut();
                            s.pressed = Some(target);
                            s.focus = target;
                        }
                        SetCapture(panel);
                    }
                    session.refresh();
                    Step::Consumed
                }
                WM_LBUTTONUP => {
                    let released = session.state.borrow_mut().pressed.take();
                    if GetCapture() == panel {
                        ReleaseCapture();
                    }
                    if let Some(host) = session.host() {
                        session.state.borrow_mut().scroller.release(host);
                    }
                    session.refresh();
                    if let (Some(target), Some(at)) = (released, point()) {
                        if session.target_at(at) == Some(target) {
                            if let Some(result) = session.activate(target) {
                                outcome = Some(result);
                                return Step::Done;
                            }
                        }
                    }
                    Step::Consumed
                }
                WM_MOUSEWHEEL => {
                    let delta = ((msg.wParam >> 16) & 0xffff) as u16 as i16 as i32;
                    let over = popup::content_screen(panel)
                        .is_some_and(|r| contains(&r, popup::message_point(msg)));
                    if over {
                        if let Some(host) = session.host() {
                            session.state.borrow_mut().scroller.wheel(host, delta, line);
                        }
                        session.refresh();
                    }
                    Step::Consumed
                }
                WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_XBUTTONDOWN
                | WM_XBUTTONUP | WM_MOUSEHWHEEL | WM_NCLBUTTONDOWN | WM_NCRBUTTONDOWN => {
                    Step::Consumed
                }
                _ => Step::Pass,
            }
        },
        || IsWindow(panel) != 0 && IsWindow(owner) != 0,
    );
    if GetCapture() == panel {
        ReleaseCapture();
    }
    // A run still going: its report comes to the main window later.
    if let Some(events) = state.borrow_mut().events.take() {
        BACKGROUND.with(|slot| *slot.borrow_mut() = Some(events));
    }
    popup::close(panel);
    popup::close(scrim);
    outcome.unwrap_or(Outcome::Closed)
}

// ───────────────────────────── previews ─────────────────────────────

/// A run's report from real, read-only data (dry-run style): memory
/// readings, the processes a trim could open and the zombie scan. Nothing
/// is trimmed or purged; the standby step is staged as declined (a real
/// outcome: the UAC prompt was cancelled).
fn preview_report() -> CleanupReport {
    let before = memclean::memory_state();
    let trim = memclean::process_ids().map(|ids| memclean::count_trimmable(&ids));
    let zombies = memclean::scan_zombies();
    CleanupReport {
        before,
        after: memclean::memory_state(),
        trim: Some(trim),
        modified: None,
        standby: Some(Err(PurgeError::Declined)),
        zombies: Some(zombies),
        elevated: crate::netetw::is_elevated(),
    }
}

/// Capture the panel staged by `setup` over the Processes page.
unsafe fn capture(
    p: *mut App,
    path: &std::path::Path,
    setup: impl FnOnce(&mut State),
    scroll_to_results: bool,
) -> Result<(), String> {
    let state = Rc::new(RefCell::new(State::new(p)));
    setup(&mut state.borrow_mut());
    let session = stage(p, &state).ok_or("Nuclear Zombie preview failed")?;
    if scroll_to_results {
        let top = state.borrow().doc.results.unwrap_or(0);
        session.scroll_to(top as f32, false);
    }
    let saved = controls::save_with_popups(p, path, &[session.scrim, session.panel]);
    popup::destroy(session.panel);
    popup::destroy(session.scrim);
    saved
}

/// `nuclear-{light,dark}` (before a run, keyboard focus on Run),
/// `nuclear-results-{light,dark}` (after a run, scrolled to the results as
/// the panel does) and `nuclear-running-light` (waiting for the UAC prompt).
pub(super) unsafe fn save_previews(p: *mut App, dir: &std::path::Path) -> Result<(), String> {
    let theme = (*p).prefs.theme;
    switch_page(p, Page::Processes);
    rebuild(p, None);
    layout(p);
    update_buttons(p);
    let report = preview_report();
    let result = (|| {
        for (value, suffix) in [(1, "light"), (2, "dark")] {
            (*p).prefs.theme = value;
            interactions::apply_theme(p);
            capture(
                p,
                &dir.join(format!("nuclear-{suffix}.bmp")),
                |s| s.ring = true,
                false,
            )?;
            capture(
                p,
                &dir.join(format!("nuclear-results-{suffix}.bmp")),
                |s| {
                    if let Ok(after) = &report.after {
                        s.memory = Ok(*after);
                    }
                    s.report = Some(report.clone());
                    s.phase = Phase::Done;
                },
                true,
            )?;
        }
        (*p).prefs.theme = 1;
        interactions::apply_theme(p);
        capture(
            p,
            &dir.join("nuclear-running-light.bmp"),
            |s| s.phase = Phase::Running(Some(Progress::WaitingForAdministrator)),
            false,
        )
    })();
    (*p).prefs.theme = theme;
    interactions::apply_theme(p);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memclean::MemoryLists;

    fn memory(available: u64, free: u64) -> MemoryState {
        MemoryState {
            total: 32 << 30,
            available,
            lists: Some(MemoryLists {
                standby: available - free,
                modified: 1 << 28,
                free,
            }),
        }
    }

    fn report() -> CleanupReport {
        CleanupReport {
            before: Ok(memory(10 << 30, 1 << 30)),
            after: Ok(memory(12 << 30, 9 << 30)),
            trim: Some(Ok(TrimReport {
                trimmed: 287,
                skipped: 25,
                failed: 0,
            })),
            modified: None,
            standby: Some(Err(PurgeError::Declined)),
            zombies: Some(Ok(ZombieScan {
                holders: vec![ZombieHolder {
                    pid: 16464,
                    created: 7,
                    name: "PowerToys.exe".into(),
                    zombies: 13,
                    examples: vec![("PowerToys.Settings.exe".into(), 8)],
                }],
                total: 13,
                inspected: 200,
                uninspected: 12,
                uninspected_handles: 40,
            })),
            elevated: false,
        }
    }

    #[test]
    fn outcomes_are_reported_honestly_in_panel_order() {
        crate::i18n::with_language(Language::English, || {
            let rows = steps(&report());
            let order: Vec<usize> = rows.iter().map(|r| r.0).collect();
            assert_eq!(
                order,
                [TRIM, STANDBY, ZOMBIES],
                "unchosen steps are left out"
            );
            assert_eq!(rows[0].1, Status::Done);
            assert_eq!(rows[0].2, "Done · 287 trimmed, 25 skipped (no access)");
            // A declined UAC prompt is not a success.
            assert_eq!(rows[1].1, Status::Attention);
            assert_eq!(rows[2].2, "Done · 13 exited processes");
            let (status, _) = purge_outcome(&Err(PurgeError::Status(0xC000_0061_u32 as i32)));
            assert_eq!(status, Status::Failed);
            let (status, _) = purge_outcome(&Err(PurgeError::Unavailable("x".into())));
            assert_eq!(status, Status::Attention);
            assert_eq!(delta(10 << 30, 12 << 30), "+2.0 GB");
            assert_eq!(delta(12 << 30, 10 << 30), "\u{2212}2.0 GB");
            assert_eq!(delta(5, 5), "\u{00B1}0 B");
            assert_eq!(
                progress_text(Some(Progress::Trimming(1312))),
                "Trimming 1,312 processes…"
            );
        });
    }

    #[test]
    fn copy_details_list_every_holder_and_the_partial_scan() {
        crate::i18n::with_language(Language::English, || {
            let text = details(&report());
            assert!(
                text.contains("Available: 10.0 GB -> 12.0 GB (+2.0 GB)"),
                "{text}"
            );
            assert!(text.contains("Clear standby cache: The administrator request was declined"));
            assert!(text.contains(
                "  PowerToys.exe (PID 16464) - 13 exited processes: PowerToys.Settings.exe (8)"
            ));
            assert!(text.contains("13 exited processes held open by 1 program"));
            assert!(text.contains("Partial scan: 12 processes could not be inspected"));
            assert!(text.contains("Run Feather as administrator"));
            assert!(!text.contains("Flush modified pages"), "not chosen");
            let mut elevated = report();
            elevated.elevated = true;
            assert!(!details(&elevated).contains("Run Feather as administrator"));
        });
    }
}
