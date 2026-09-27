//! Identity-stable device ordering. No polling or background work: registry I/O
//! occurs at launch and explicit edits; the autoscroll timer lives only in a drag.
use super::*;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemServices::MK_LBUTTON;

const SUBCLASS: usize = 0xFE31;
const SCROLL_TIMER: usize = 0xFE32;
const PATH: &str = r"Software\FeatherTask\Preferences";
const VALUE: &str = "PerformanceOrder";
const MAX_KEYS: usize = 512;
const MAX_ID_BYTES: usize = 1024;
const MAX_BYTES: usize = 4 + MAX_KEYS * (3 + MAX_ID_BYTES);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Order {
    keys: Vec<PerfTarget>,
    /// Runtime GPU LUID aliases retain the hardware identity across a sample
    /// failure/disconnection. They are bounded and never written to disk.
    gpu_aliases: std::collections::HashMap<String, String>,
}

impl Order {
    pub fn load() -> Self {
        let Some(key) = crate::registry::open(HKEY_CURRENT_USER, PATH, KEY_QUERY_VALUE)
            .ok()
            .flatten()
        else {
            return Self::default();
        };
        let mut bytes = 0;
        let status = unsafe {
            RegGetValueW(
                key.0,
                null(),
                wide(VALUE).as_ptr(),
                RRF_RT_REG_BINARY,
                null_mut(),
                null_mut(),
                &mut bytes,
            )
        };
        if status != 0 || !(4..=MAX_BYTES as u32).contains(&bytes) {
            return Self::default();
        }
        let mut buffer = vec![0u8; bytes as usize];
        let status = unsafe {
            RegGetValueW(
                key.0,
                null(),
                wide(VALUE).as_ptr(),
                RRF_RT_REG_BINARY,
                null_mut(),
                buffer.as_mut_ptr().cast(),
                &mut bytes,
            )
        };
        if status != 0 || bytes as usize > buffer.len() {
            return Self::default();
        }
        Self::decode(&buffer[..bytes as usize]).unwrap_or_default()
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_BYTES || bytes.get(..4)? != b"FPO1" {
            return None;
        }
        let mut rest = &bytes[4..];
        let mut keys = Vec::new();
        let mut seen = HashSet::new();
        while !rest.is_empty() {
            if keys.len() == MAX_KEYS || rest.len() < 3 {
                return None;
            }
            let len = u16::from_le_bytes([rest[1], rest[2]]) as usize;
            if len > MAX_ID_BYTES {
                return None;
            }
            let payload = rest.get(3..3 + len)?;
            let text = || {
                let s = std::str::from_utf8(payload).ok()?;
                (!s.is_empty() && !s.chars().any(char::is_control)).then(|| s.to_owned())
            };
            let key = match rest[0] {
                0 if len == 0 => PerfTarget::Cpu,
                1 if len == 0 => PerfTarget::Memory,
                2 => PerfTarget::Disk(text()?),
                3 if len == 8 => PerfTarget::Network(u64::from_le_bytes(payload.try_into().ok()?)),
                4 => PerfTarget::Gpu(text()?),
                _ => return None,
            };
            if !seen.insert(key.clone()) {
                return None;
            }
            keys.push(key);
            rest = &rest[3 + len..];
        }
        Some(Self {
            keys,
            ..Self::default()
        })
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = b"FPO1".to_vec();
        for key in &self.keys {
            let (tag, data) = match key {
                PerfTarget::Cpu => (0, Vec::new()),
                PerfTarget::Memory => (1, Vec::new()),
                PerfTarget::Disk(id) => (2, id.as_bytes().to_vec()),
                PerfTarget::Network(id) => (3, id.to_le_bytes().to_vec()),
                PerfTarget::Gpu(id) => (4, id.as_bytes().to_vec()),
            };
            if data.len() > MAX_ID_BYTES {
                continue;
            }
            bytes.push(tag);
            bytes.extend((data.len() as u16).to_le_bytes());
            bytes.extend(data);
        }
        bytes
    }

    fn save(&self) -> Result<(), String> {
        let save = || -> Result<(), u32> {
            let key = crate::registry::create(HKEY_CURRENT_USER, PATH, KEY_SET_VALUE)?;
            let bytes = self.encode();
            let status = unsafe {
                RegSetValueExW(
                    key.0,
                    wide(VALUE).as_ptr(),
                    0,
                    REG_BINARY,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                )
            };
            if status == 0 {
                Ok(())
            } else {
                Err(status)
            }
        };
        save().map_err(|code| {
            format!(
                "{} (Windows {code})",
                tr(
                    "구성 요소 순서를 저장할 수 없습니다",
                    "Cannot save component order"
                )
            )
        })
    }

    /// Missing devices retain their slots. Previously unseen devices append,
    /// including after reconnecting devices or changing the sampling order.
    fn arrange(&mut self, targets: &mut [PerfTarget]) {
        // Without a user edit, retain the sampler's canonical category order.
        // Disk counters may appear later than network counters during warmup.
        if self.keys.is_empty() {
            return;
        }
        let present: HashSet<_> = targets.iter().cloned().collect();
        for target in targets.iter() {
            if !self.keys.contains(target) {
                if self.keys.len() == MAX_KEYS {
                    if let Some(index) = self.keys.iter().position(|id| !present.contains(id)) {
                        self.keys.remove(index);
                    } else {
                        break;
                    }
                }
                self.keys.push(target.clone());
            }
        }
        let positions: std::collections::HashMap<_, _> = self
            .keys
            .iter()
            .enumerate()
            .map(|(i, key)| (key, i))
            .collect();
        targets.sort_by_key(|key| positions.get(key).copied().unwrap_or(usize::MAX));
    }

    fn remember(&mut self, targets: &[PerfTarget]) {
        let present: HashSet<_> = targets.iter().collect();
        let mut reordered = targets.iter();
        for key in &mut self.keys {
            if present.contains(key) {
                if let Some(next) = reordered.next() {
                    *key = next.clone();
                }
            }
        }
        self.keys
            .extend(reordered.take(MAX_KEYS - self.keys.len()).cloned());
    }
}

pub(super) unsafe fn reconcile(p: *mut App, targets: &mut Vec<PerfTarget>) {
    // Keep the selected disconnected device visible with its existing history.
    if !targets.contains(&(*p).perf_target) {
        targets.push((*p).perf_target.clone());
    }
    let mut keys = persistent_targets(p, targets);
    let live: std::collections::HashMap<_, _> =
        keys.iter().cloned().zip(targets.drain(..)).collect();
    (*p).perf_order.arrange(&mut keys);
    targets.extend(keys.iter().filter_map(|key| live.get(key).cloned()));
}

/// Display-kernel LUIDs change at reboot. Prefer the adapter's existing PCI
/// identity and location; physical index distinguishes linked adapters. Never
/// merge two devices when a driver reports ambiguous hardware information.
fn gpu_hardware_key(gpu: &crate::performance::GpuStats) -> Option<String> {
    let adapter = gpu.adapter.as_ref()?;
    let pci = adapter.pci.as_ref()?;
    let (bus, device, function) = adapter.pci_location?;
    let physical = gpu.physical_index?;
    Some(format!(
        "pci:{:x}:{:x}:{:x}:{:x}:{:x}:{bus}:{device}:{function}:{physical}",
        pci.vendor_id, pci.device_id, pci.subsystem_id, pci.subsystem_vendor_id, pci.revision_id
    ))
}

unsafe fn persistent_targets(p: *mut App, targets: &[PerfTarget]) -> Vec<PerfTarget> {
    let mut keys: Vec<_> = targets
        .iter()
        .map(|target| {
            if let PerfTarget::Gpu(id) = target {
                if let Some(key) = (*p)
                    .performance
                    .as_ref()
                    .and_then(|perf| perf.gpus.iter().find(|gpu| &gpu.id == id))
                    .and_then(gpu_hardware_key)
                    .or_else(|| (*p).perf_order.gpu_aliases.get(id).cloned())
                {
                    return PerfTarget::Gpu(key);
                }
            }
            target.clone()
        })
        .collect();
    let mut counts = std::collections::HashMap::new();
    for key in &keys {
        *counts.entry(key.clone()).or_insert(0usize) += 1;
    }
    for (key, target) in keys.iter_mut().zip(targets) {
        if counts.get(key).is_some_and(|count| *count > 1) {
            *key = target.clone();
        }
        if let (PerfTarget::Gpu(runtime), PerfTarget::Gpu(stable)) = (target, key) {
            if runtime != stable {
                let aliases = &mut (*p).perf_order.gpu_aliases;
                if aliases.len() >= MAX_KEYS {
                    aliases.retain(|id, _| {
                        targets
                            .iter()
                            .any(|target| matches!(target, PerfTarget::Gpu(live) if live == id))
                    });
                }
                if aliases.get(runtime) != Some(stable) {
                    aliases.insert(runtime.clone(), stable.clone());
                }
            }
        }
    }
    keys
}

/// Move `source` before an insertion slot in the original list. Ordinary clicks
/// and cancelled drags never call this; no-op drops never write preferences.
fn move_before(targets: &mut Vec<PerfTarget>, source: &PerfTarget, slot: usize) -> bool {
    let Some(from) = targets.iter().position(|key| key == source) else {
        return false;
    };
    let to = slot
        .min(targets.len())
        .saturating_sub(usize::from(slot > from));
    if from == to {
        return false;
    }
    let target = targets.remove(from);
    targets.insert(to, target);
    true
}

struct Drag {
    source: PerfTarget,
    origin: POINT,
    active: bool,
    slot: Option<usize>,
}

struct State {
    app: *mut App,
    drag: Option<Drag>,
    timer: bool,
}

pub(super) unsafe fn install(hwnd: HWND, app: *mut App) {
    let state = Box::into_raw(Box::new(State {
        app,
        drag: None,
        timer: false,
    }));
    if SetWindowSubclass(hwnd, Some(subclass), SUBCLASS, state as usize) == 0 {
        drop(Box::from_raw(state));
    }
}

fn point(l: LPARAM) -> POINT {
    POINT {
        x: l as u16 as i16 as i32,
        y: (l >> 16) as u16 as i16 as i32,
    }
}

unsafe fn client(hwnd: HWND) -> RECT {
    let mut area = zeroed();
    GetClientRect(hwnd, &mut area);
    area
}

unsafe fn stop(hwnd: HWND, s: *mut State) -> Option<Drag> {
    let drag = (*s).drag.take();
    if (*s).timer {
        KillTimer(hwnd, SCROLL_TIMER);
        (*s).timer = false;
    }
    if GetCapture() == hwnd {
        ReleaseCapture();
    }
    if drag.as_ref().is_some_and(|drag| drag.active) {
        InvalidateRect(hwnd, null(), 0);
    }
    drag
}

unsafe fn slot_at(hwnd: HWND, count: usize, pt: POINT) -> Option<usize> {
    let area = client(hwnd);
    if pt.x < 0 || pt.x >= area.right || pt.y < 0 || pt.y >= area.bottom {
        return None;
    }
    for row in 0..count {
        let r = table::part_rect(hwnd, row, table::Part::Row)?;
        if pt.y < r.top + (r.bottom - r.top) / 2 {
            return Some(row);
        }
    }
    Some(count)
}

unsafe fn scroll_direction(hwnd: HWND, dpi: i32, pt: POINT) -> f32 {
    let area = client(hwnd);
    if pt.x < 0 || pt.x >= area.right {
        return 0.0;
    }
    let edge = gfx::pxi(dpi, 28.0).min(area.bottom / 3);
    let offset = table::scroll_offset(hwnd);
    if pt.y < edge && offset > 0.0 {
        -1.0
    } else if pt.y >= area.bottom - edge && offset < table::extent(hwnd).max() {
        1.0
    } else {
        0.0
    }
}

unsafe fn update_drag(hwnd: HWND, s: *mut State, pt: POINT) {
    let p = (*s).app;
    if !(*s).drag.as_ref().is_some_and(|drag| drag.active) {
        return;
    }
    let slot = slot_at(hwnd, (*p).perf_targets.len(), pt);
    if let Some(drag) = (*s).drag.as_mut() {
        if drag.slot != slot {
            drag.slot = slot;
            InvalidateRect(hwnd, null(), 0);
        }
    }
    let scrolling = scroll_direction(hwnd, (*p).dpi, pt) != 0.0;
    if scrolling && !(*s).timer {
        (*s).timer = SetTimer(hwnd, SCROLL_TIMER, 50, None) != 0;
    } else if !scrolling && (*s).timer {
        KillTimer(hwnd, SCROLL_TIMER);
        (*s).timer = false;
    }
    SetCursor(LoadCursorW(null_mut(), IDC_SIZEALL));
}

unsafe fn indicator(hwnd: HWND, s: *mut State, dc: HDC) {
    let Some(slot) = (*s)
        .drag
        .as_ref()
        .filter(|drag| drag.active)
        .and_then(|d| d.slot)
    else {
        return;
    };
    let p = (*s).app;
    let count = (*p).perf_targets.len();
    let Some(r) = table::part_rect(hwnd, slot.min(count.saturating_sub(1)), table::Part::Row)
    else {
        return;
    };
    let area = client(hwnd);
    let thickness = gfx::pxi((*p).dpi, 2.0).max(2);
    let y = if slot == count { r.bottom } else { r.top };
    if y < 0 || y > area.bottom {
        return;
    }
    let bar = RECT {
        left: r.left,
        top: y.clamp(0, (area.bottom - thickness).max(0)),
        right: r.right,
        bottom: (y + thickness).min(area.bottom),
    };
    let brush = CreateSolidBrush(colors().accent);
    FillRect(dc, &bar, brush);
    DeleteObject(brush);
}

unsafe fn save_and_refresh(p: *mut App) {
    let keys = persistent_targets(p, &(*p).perf_targets);
    (*p).perf_order.remember(&keys);
    persist_and_refresh(p);
}

unsafe fn persist_and_refresh(p: *mut App) {
    if (*p).persist_preferences {
        if let Err(error) = (*p).perf_order.save() {
            (*p).set_error(ErrorSource::Action, error);
        }
    }
    // Force labels and row identity animations to rebuild even though the
    // already-reordered vector equals the next reconciled sample.
    SendMessageW((*p).perf_list, LB_RESETCONTENT, 0, 0);
    interactions::refresh_components(p);
    redraw(p);
}

unsafe fn move_selected(p: *mut App, down: bool) {
    let source = (*p).perf_target.clone();
    let Some(row) = (*p).perf_targets.iter().position(|key| key == &source) else {
        return;
    };
    let slot = if down { row + 2 } else { row.saturating_sub(1) };
    if move_before(&mut (*p).perf_targets, &source, slot) {
        save_and_refresh(p);
    }
}

unsafe fn context_menu(hwnd: HWND, s: *mut State, l: LPARAM) {
    let p = (*s).app;
    if (*p).modal {
        return;
    }
    stop(hwnd, s);
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let row = (*p)
        .perf_targets
        .iter()
        .position(|key| key == &(*p).perf_target)
        .unwrap_or(0);
    for (id, enabled, label) in [
        (1, row > 0, tr("위로 이동\tAlt+↑", "Move up\tAlt+↑")),
        (
            2,
            row + 1 < (*p).perf_targets.len(),
            tr("아래로 이동\tAlt+↓", "Move down\tAlt+↓"),
        ),
        (3, true, tr("기본 순서로 되돌리기", "Reset component order")),
    ] {
        AppendMenuW(
            menu,
            MF_STRING | if enabled { 0 } else { MF_GRAYED },
            id,
            wide(label).as_ptr(),
        );
    }
    let anchor = table::keyboard_menu_anchor(hwnd, l)
        .map(|r| popup::Anchor::Below { r, right: false })
        .unwrap_or_else(|| popup::Anchor::Point(point(l)));
    (*p).modal = true;
    configure(p);
    let id = popup::track_menu(p, menu, anchor);
    DestroyMenu(menu);
    interactions::finish_modal(p);
    match id {
        1 | 2 => move_selected(p, id == 2),
        3 => {
            (*p).perf_order = Order::default();
            // An empty saved order restores the canonical behavior, including
            // hardware whose first measurement arrives after this reset.
            persist_and_refresh(p);
        }
        _ => {}
    }
}

unsafe extern "system" fn subclass(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    let s = data as *mut State;
    let p = (*s).app;
    match msg {
        WM_NCDESTROY => {
            stop(hwnd, s);
            RemoveWindowSubclass(hwnd, Some(subclass), SUBCLASS);
            drop(Box::from_raw(s));
            return DefSubclassProc(hwnd, msg, w, l);
        }
        WM_LBUTTONDOWN => {
            let pt = point(l);
            let in_lane = table::extent(hwnd).scrollable()
                && pt.x >= client(hwnd).right - gfx::pxi((*p).dpi, scroll::ZONE_DIP);
            if !in_lane {
                if let Some((row, _)) = table::part_at(hwnd, pt) {
                    if let Some(source) = (&(*p).perf_targets).get(row).cloned() {
                        stop(hwnd, s);
                        (*s).drag = Some(Drag {
                            source,
                            origin: pt,
                            active: false,
                            slot: None,
                        });
                        SetFocus(hwnd);
                        SetCapture(hwnd);
                        return 0;
                    }
                }
            }
        }
        WM_MOUSEMOVE if (*s).drag.is_some() => {
            let pt = point(l);
            if w & MK_LBUTTON as usize == 0 {
                stop(hwnd, s);
                return 0;
            }
            if let Some(drag) = (*s).drag.as_mut() {
                if !drag.active
                    && ((pt.x - drag.origin.x).abs()
                        >= GetSystemMetricsForDpi(SM_CXDRAG, (*p).dpi as u32).max(4)
                        || (pt.y - drag.origin.y).abs()
                            >= GetSystemMetricsForDpi(SM_CYDRAG, (*p).dpi as u32).max(4))
                {
                    drag.active = true;
                    table::settle(hwnd);
                }
            }
            update_drag(hwnd, s, pt);
            return 0;
        }
        WM_LBUTTONUP if (*s).drag.is_some() => {
            let pt = point(l);
            let slot = slot_at(hwnd, (*p).perf_targets.len(), pt);
            let Some(drag) = stop(hwnd, s) else { return 0 };
            if drag.active {
                if let Some(slot) = slot {
                    if move_before(&mut (*p).perf_targets, &drag.source, slot) {
                        save_and_refresh(p);
                    }
                }
            } else if table::part_at(hwnd, pt)
                .is_some_and(|(row, _)| (&(*p).perf_targets).get(row) == Some(&drag.source))
            {
                // Ordinary clicks retain native selection/notification behavior.
                DefSubclassProc(hwnd, WM_LBUTTONDOWN, w | MK_LBUTTON as usize, l);
                DefSubclassProc(hwnd, WM_LBUTTONUP, w, l);
            }
            return 0;
        }
        WM_TIMER if w == SCROLL_TIMER => {
            if (*s).drag.as_ref().is_some_and(|d| d.active) {
                let mut pt = zeroed();
                GetCursorPos(&mut pt);
                ScreenToClient(hwnd, &mut pt);
                let direction = scroll_direction(hwnd, (*p).dpi, pt);
                table::scroll_to(
                    hwnd,
                    table::scroll_offset(hwnd) + direction * gfx::px((*p).dpi, 18.0),
                );
                update_drag(hwnd, s, pt);
            } else {
                KillTimer(hwnd, SCROLL_TIMER);
                (*s).timer = false;
            }
            return 0;
        }
        WM_KEYDOWN | WM_SYSKEYDOWN if w as u16 == VK_ESCAPE && (*s).drag.is_some() => {
            stop(hwnd, s);
            return 0;
        }
        WM_KEYDOWN | WM_SYSKEYDOWN
            if [VK_UP, VK_DOWN].contains(&(w as u16)) && GetKeyState(VK_MENU as i32) < 0 =>
        {
            stop(hwnd, s);
            move_selected(p, w as u16 == VK_DOWN);
            return 0;
        }
        WM_GETDLGCODE => {
            let mut result = DefSubclassProc(hwnd, msg, w, l);
            let message = l as *const MSG;
            if !message.is_null()
                && (((*message).wParam as u16 == VK_ESCAPE && (*s).drag.is_some())
                    || ([VK_UP, VK_DOWN].contains(&((*message).wParam as u16))
                        && GetKeyState(VK_MENU as i32) < 0))
            {
                result |= DLGC_WANTMESSAGE as isize;
            }
            return result;
        }
        WM_CAPTURECHANGED | WM_CANCELMODE | WM_KILLFOCUS => {
            stop(hwnd, s);
        }
        WM_SHOWWINDOW if w == 0 => {
            stop(hwnd, s);
        }
        WM_CONTEXTMENU => {
            context_menu(hwnd, s, l);
            return 0;
        }
        WM_PAINT | WM_PRINTCLIENT => {
            let result = DefSubclassProc(hwnd, msg, w, l);
            if (*s).drag.as_ref().is_some_and(|d| d.active) {
                let dc = if msg == WM_PRINTCLIENT {
                    w as HDC
                } else {
                    GetDC(hwnd)
                };
                if !dc.is_null() {
                    indicator(hwnd, s, dc);
                }
                if msg == WM_PAINT {
                    ReleaseDC(hwnd, dc);
                }
            }
            return result;
        }
        _ => {}
    }
    DefSubclassProc(hwnd, msg, w, l)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moving_and_reconnecting_keep_device_identities() {
        let a = PerfTarget::Disk("0 C:".into());
        let b = PerfTarget::Network(42);
        let mut order = Order::default();
        let mut warming = vec![PerfTarget::Cpu, PerfTarget::Memory, b.clone()];
        order.arrange(&mut warming);
        let mut live = vec![PerfTarget::Cpu, PerfTarget::Memory, a.clone(), b.clone()];
        order.arrange(&mut live);
        assert_eq!(
            live,
            vec![PerfTarget::Cpu, PerfTarget::Memory, a.clone(), b.clone()]
        );
        assert!(
            order.keys.is_empty(),
            "sampling alone must not customize order"
        );
        assert!(move_before(&mut live, &b, 0));
        order.remember(&live);
        let mut reconnected = vec![PerfTarget::Cpu, a.clone(), PerfTarget::Memory];
        order.arrange(&mut reconnected);
        assert_eq!(
            reconnected,
            vec![PerfTarget::Cpu, PerfTarget::Memory, a.clone()]
        );
        reconnected.insert(0, PerfTarget::Gpu("new-adapter".into()));
        reconnected.push(b.clone());
        order.arrange(&mut reconnected);
        assert_eq!(
            reconnected,
            vec![
                b,
                PerfTarget::Cpu,
                PerfTarget::Memory,
                a,
                PerfTarget::Gpu("new-adapter".into())
            ]
        );
    }

    #[test]
    fn persistence_is_bounded_and_rejects_malformed_or_duplicate_ids() {
        let order = Order {
            keys: vec![
                PerfTarget::Gpu("gpu:λ".into()),
                PerfTarget::Cpu,
                PerfTarget::Network(u64::MAX),
            ],
            ..Order::default()
        };
        assert_eq!(Order::decode(&order.encode()), Some(order));
        for bytes in [
            b"".as_slice(),
            b"FPO1\x09\x00\x00",
            b"FPO1\x02\x01\x00",
            b"FPO1\x00\x00\x00\x00\x00\x00",
            b"FPO1\x02\x01\x00\x00",
        ] {
            assert!(Order::decode(bytes).is_none());
        }
        assert!(Order::decode(&vec![0; MAX_BYTES + 1]).is_none());
        let too_many = Order {
            keys: (0..=MAX_KEYS as u64).map(PerfTarget::Network).collect(),
            ..Order::default()
        };
        assert!(Order::decode(&too_many.encode()).is_none());
    }

    #[test]
    fn no_op_and_missing_source_never_change_order() {
        let expected = vec![PerfTarget::Cpu, PerfTarget::Memory];
        let mut live = expected.clone();
        assert!(!move_before(&mut live, &PerfTarget::Cpu, 0));
        assert!(!move_before(&mut live, &PerfTarget::Cpu, 1));
        assert!(!move_before(&mut live, &PerfTarget::Gpu("gone".into()), 0));
        assert_eq!(live, expected);
        assert!(move_before(&mut live, &PerfTarget::Cpu, usize::MAX));
        assert_eq!(live, vec![PerfTarget::Memory, PerfTarget::Cpu]);
    }
}
