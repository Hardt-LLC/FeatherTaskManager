//! Screen-reader access for the custom table / list (SysListView32 and the
//! ListBox it replaced exposed their rows; so does this): a minimal
//! hand-rolled MSAA `IAccessible`, served from `WM_GETOBJECT(OBJID_CLIENT)`.
//! UI Automation clients (Narrator) reach it through the system's MSAA
//! proxy.
//!
//! * The control is a `ROLE_SYSTEM_LIST` named after its window text
//!   ("Process list"); every row is a simple child element (child id =
//!   row + 1): `ROLE_SYSTEM_LISTITEM` for data rows, `ROLE_SYSTEM_GROUPING`
//!   for group titles ("Apps (8)").
//! * A row's name is its first cell, its description "Column: value" for
//!   the others (like a report-view ListView), its state selectable /
//!   selected / focused / off-screen (+ expanded or collapsed for tree
//!   parents), its location the row rectangle on screen.
//! * Hit testing, focus, selection, navigation, `accSelect` and the default
//!   action ("Double click", posted: it may open a confirmation dialog, which
//!   must not run inside the client's call) work like a ListView's.
//! * Events: gaining focus raises `EVENT_OBJECT_FOCUS` on the selected row;
//!   a selection change raises `EVENT_OBJECT_SELECTION` (+ focus while
//!   focused). Changes are announced after the message that caused them and
//!   compared by row identity (`Model::key`), so a rebuild that clears and
//!   restores the same selection every sample stays silent.
//!
//! The COM object only holds the HWND; once the window is gone every call
//! returns `RPC_E_DISCONNECTED`.
use super::*;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use windows_sys::core::{BSTR, GUID, HRESULT};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::UI::Accessibility::{
    AccessibleObjectFromWindow, LresultFromObject, NotifyWinEvent,
};

/// Posted to the control: announce the selection (see the module docs).
pub(super) const WM_ANNOUNCE: u32 = WM_APP + 0x11;
/// Posted to the control: run row `wParam`'s default action.
pub(super) const WM_ACTIVATE_ROW: u32 = WM_APP + 0x12;

pub(super) const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
pub(super) const IID_IDISPATCH: GUID = GUID::from_u128(0x00020400_0000_0000_c000_000000000046);
pub(super) const IID_IACCESSIBLE: GUID = GUID::from_u128(0x618736e0_3c3d_11cf_810c_00aa00389b71);

const HR_OK: HRESULT = 0;
const HR_FALSE: HRESULT = 1;
const HR_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
const HR_NOINTERFACE: HRESULT = 0x8000_4002_u32 as i32;
const HR_POINTER: HRESULT = 0x8000_4003_u32 as i32;
const HR_OUTOFMEMORY: HRESULT = 0x8007_000e_u32 as i32;
const HR_INVALIDARG: HRESULT = 0x8007_0057_u32 as i32;
const HR_MEMBERNOTFOUND: HRESULT = 0x8002_0003_u32 as i32;
pub(super) const HR_DISCONNECTED: HRESULT = 0x8001_0108_u32 as i32;
const HR_COM_NOT_INITIALIZED: HRESULT = 0x8004_01f0_u32 as i32;

const VT_EMPTY: u16 = 0;
pub(super) const VT_I4: u16 = 3;

const SELF: i32 = 0; // CHILDID_SELF
const OBJID_CLIENT_ID: i32 = -4;
const OBJID_WINDOW_ID: u32 = 0;

pub(super) const ROLE_LIST: i32 = 33;
pub(super) const ROLE_LISTITEM: i32 = 34;
pub(super) const ROLE_GROUPING: i32 = 20;

pub(super) const STATE_UNAVAILABLE: i32 = 0x1;
pub(super) const STATE_SELECTED: i32 = 0x2;
pub(super) const STATE_FOCUSED: i32 = 0x4;
pub(super) const STATE_READONLY: i32 = 0x40;
pub(super) const STATE_EXPANDED: i32 = 0x200;
pub(super) const STATE_COLLAPSED: i32 = 0x400;
pub(super) const STATE_INVISIBLE: i32 = 0x8000;
pub(super) const STATE_OFFSCREEN: i32 = 0x1_0000;
pub(super) const STATE_FOCUSABLE: i32 = 0x10_0000;
pub(super) const STATE_SELECTABLE: i32 = 0x20_0000;

const NAVDIR_UP: i32 = 1;
const NAVDIR_DOWN: i32 = 2;
pub(super) const NAVDIR_NEXT: i32 = 5;
const NAVDIR_PREVIOUS: i32 = 6;
const NAVDIR_FIRSTCHILD: i32 = 7;
const NAVDIR_LASTCHILD: i32 = 8;

const SELFLAG_TAKEFOCUS: i32 = 0x1;
pub(super) const SELFLAG_TAKESELECTION: i32 = 0x2;
const SELFLAG_EXTENDSELECTION: i32 = 0x4;
const SELFLAG_ADDSELECTION: i32 = 0x8;
const SELFLAG_REMOVESELECTION: i32 = 0x10;

pub(super) const EVENT_FOCUS: u32 = 0x8005;
pub(super) const EVENT_SELECTION: u32 = 0x8006;
pub(super) const EVENT_SELECTIONWITHIN: u32 = 0x8009;

/// `VARIANT` (x64: 24 bytes, x86: 16): the type tag, three reserved words
/// and a two-pointer payload whose first four bytes are `lVal`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Variant {
    pub vt: u16,
    reserved: [u16; 3],
    data: [usize; 2],
}

impl Variant {
    pub(super) const fn empty() -> Self {
        Self {
            vt: VT_EMPTY,
            reserved: [0; 3],
            data: [0; 2],
        }
    }
    pub(super) const fn i4(value: i32) -> Self {
        Self {
            vt: VT_I4,
            reserved: [0; 3],
            data: [value as u32 as usize, 0],
        }
    }
    pub(super) fn as_i4(&self) -> Option<i32> {
        (self.vt == VT_I4).then_some(self.data[0] as u32 as i32)
    }
}

type This = *mut Object;
type Getter = unsafe extern "system" fn(This, Variant, *mut BSTR) -> HRESULT;

/// IUnknown + IDispatch + IAccessible, in vtable order.
#[repr(C)]
pub(super) struct Vtbl {
    pub query_interface: unsafe extern "system" fn(This, *const GUID, *mut *mut c_void) -> HRESULT,
    pub add_ref: unsafe extern "system" fn(This) -> u32,
    pub release: unsafe extern "system" fn(This) -> u32,
    get_type_info_count: unsafe extern "system" fn(This, *mut u32) -> HRESULT,
    get_type_info: unsafe extern "system" fn(This, u32, u32, *mut *mut c_void) -> HRESULT,
    get_ids_of_names: unsafe extern "system" fn(
        This,
        *const GUID,
        *const *const u16,
        u32,
        u32,
        *mut i32,
    ) -> HRESULT,
    #[allow(clippy::type_complexity)]
    invoke: unsafe extern "system" fn(
        This,
        i32,
        *const GUID,
        u32,
        u16,
        *mut c_void,
        *mut Variant,
        *mut c_void,
        *mut u32,
    ) -> HRESULT,
    pub acc_parent: unsafe extern "system" fn(This, *mut *mut c_void) -> HRESULT,
    pub acc_child_count: unsafe extern "system" fn(This, *mut i32) -> HRESULT,
    pub acc_child: unsafe extern "system" fn(This, Variant, *mut *mut c_void) -> HRESULT,
    pub acc_name: Getter,
    pub acc_value: Getter,
    pub acc_description: Getter,
    pub acc_role: unsafe extern "system" fn(This, Variant, *mut Variant) -> HRESULT,
    pub acc_state: unsafe extern "system" fn(This, Variant, *mut Variant) -> HRESULT,
    acc_help: Getter,
    acc_help_topic: unsafe extern "system" fn(This, *mut BSTR, Variant, *mut i32) -> HRESULT,
    acc_keyboard_shortcut: Getter,
    pub acc_focus: unsafe extern "system" fn(This, *mut Variant) -> HRESULT,
    pub acc_selection: unsafe extern "system" fn(This, *mut Variant) -> HRESULT,
    pub acc_default_action: Getter,
    pub acc_select: unsafe extern "system" fn(This, i32, Variant) -> HRESULT,
    pub acc_location:
        unsafe extern "system" fn(This, *mut i32, *mut i32, *mut i32, *mut i32, Variant) -> HRESULT,
    pub acc_navigate: unsafe extern "system" fn(This, i32, Variant, *mut Variant) -> HRESULT,
    pub acc_hit_test: unsafe extern "system" fn(This, i32, i32, *mut Variant) -> HRESULT,
    pub acc_do_default_action: unsafe extern "system" fn(This, Variant) -> HRESULT,
    put_acc_name: unsafe extern "system" fn(This, Variant, BSTR) -> HRESULT,
    put_acc_value: unsafe extern "system" fn(This, Variant, BSTR) -> HRESULT,
}

#[repr(C)]
pub(super) struct Object {
    pub vtbl: *const Vtbl,
    refs: AtomicU32,
    hwnd: HWND,
}

static VTBL: Vtbl = Vtbl {
    query_interface,
    add_ref,
    release,
    get_type_info_count,
    get_type_info,
    get_ids_of_names,
    invoke,
    acc_parent,
    acc_child_count,
    acc_child,
    acc_name,
    acc_value,
    acc_description,
    acc_role,
    acc_state,
    acc_help,
    acc_help_topic,
    acc_keyboard_shortcut,
    acc_focus,
    acc_selection,
    acc_default_action,
    acc_select,
    acc_location,
    acc_navigate,
    acc_hit_test,
    acc_do_default_action,
    put_acc_name,
    put_acc_value,
};

fn same(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

// ───────────────────────────── serving and events ─────────────────────────────

/// `WM_GETOBJECT`: our IAccessible for `OBJID_CLIENT` (None: let
/// DefWindowProc answer, e.g. UI Automation's root object request, which
/// then falls back to this object through the MSAA proxy).
pub(super) unsafe fn get_object(hwnd: HWND, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    if l as i32 != OBJID_CLIENT_ID || state(hwnd).is_null() {
        return None;
    }
    let object = Box::into_raw(Box::new(Object {
        vtbl: &VTBL,
        refs: AtomicU32::new(1),
        hwnd,
    }));
    let mut result = LresultFromObject(&IID_IACCESSIBLE, w, object.cast());
    if result as i32 == HR_COM_NOT_INITIALIZED {
        // Marshalling needs a COM apartment; the UI thread (which pumps
        // messages) becomes a single-threaded one for the process lifetime.
        CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32);
        result = LresultFromObject(&IID_IACCESSIBLE, w, object.cast());
    }
    release(object);
    Some(result)
}

/// The selection changed: announce it once the current message is done.
pub(super) unsafe fn selection_changed(s: *mut State) {
    if !(*s).announce_pending && PostMessageW((*s).hwnd, WM_ANNOUNCE, 0, 0) != 0 {
        (*s).announce_pending = true;
    }
}

fn child_id(row: usize) -> i32 {
    row as i32 + 1
}

/// `WM_ANNOUNCE`: raise the selection (and focus) events when the selected
/// row's identity differs from the one announced last.
pub(super) unsafe fn announce(s: *mut State) {
    (*s).announce_pending = false;
    let current = (*s).selected.map(|row| (*s).model.key(row));
    if current == (*s).announced {
        return;
    }
    (*s).announced = current;
    let hwnd = (*s).hwnd;
    if IsWindowVisible(hwnd) == 0 {
        return;
    }
    let focused = GetFocus() == hwnd;
    match (*s).selected {
        Some(row) => {
            NotifyWinEvent(EVENT_SELECTION, hwnd, OBJID_CLIENT_ID, child_id(row));
            if focused {
                NotifyWinEvent(EVENT_FOCUS, hwnd, OBJID_CLIENT_ID, child_id(row));
            }
        }
        None => {
            NotifyWinEvent(EVENT_SELECTIONWITHIN, hwnd, OBJID_CLIENT_ID, SELF);
            if focused {
                NotifyWinEvent(EVENT_FOCUS, hwnd, OBJID_CLIENT_ID, SELF);
            }
        }
    }
}

/// `WM_SETFOCUS`: the system announced the control; announce its row too.
pub(super) unsafe fn focus_gained(s: *mut State) {
    if let Some(row) = (*s).selected {
        (*s).announced = Some((*s).model.key(row));
        NotifyWinEvent(EVENT_FOCUS, (*s).hwnd, OBJID_CLIENT_ID, child_id(row));
    }
}

// ───────────────────────────── helpers ─────────────────────────────

/// The live control behind the object.
unsafe fn live(this: This) -> Option<*mut State> {
    let s = state((*this).hwnd);
    (!s.is_null()).then_some(s)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Child {
    Me,
    Row(usize),
}

unsafe fn child(s: *mut State, v: &Variant) -> Option<Child> {
    match v.as_i4()? {
        SELF => Some(Child::Me),
        id if id >= 1 && (id as usize) <= (*s).count => Some(Child::Row(id as usize - 1)),
        _ => None,
    }
}

unsafe fn put_bstr(out: *mut BSTR, text: &str) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = null();
    if text.is_empty() {
        return HR_FALSE;
    }
    let units = wide(text);
    *out = SysAllocString(units.as_ptr());
    if (*out).is_null() {
        HR_OUTOFMEMORY
    } else {
        HR_OK
    }
}

unsafe fn put_variant(out: *mut Variant, value: Variant) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = value;
    if value.vt == VT_EMPTY {
        HR_FALSE
    } else {
        HR_OK
    }
}

/// Answer a string property of the control or one of its rows.
unsafe fn text_property(
    this: This,
    v: Variant,
    out: *mut BSTR,
    text: impl FnOnce(*mut State, Child) -> String,
) -> HRESULT {
    if !out.is_null() {
        *out = null();
    }
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    let Some(which) = child(s, &v) else {
        return HR_INVALIDARG;
    };
    put_bstr(out, &text(s, which))
}

/// A row's accessible name: its first cell (a group row's title).
unsafe fn row_name(s: *mut State, row: usize) -> String {
    item_text(s, row)
}

/// A table row's description, like a report-view ListView's:
/// "PID: 1234, CPU: 0.4%, Memory: 12.0 MB, …" (the header labels without
/// footnote marks; empty and unavailable "—" cells are left out rather
/// than read as a dash).
unsafe fn row_description(s: *mut State, row: usize) -> String {
    if (*s).mode != Mode::Table || (*s).model.is_group(row) {
        return String::new();
    }
    let mut parts = Vec::new();
    for (col, column) in (*s).columns.iter().enumerate().skip(1) {
        let text = (*s).model.text(row, col);
        if text.is_empty() || text == "—" {
            continue;
        }
        let label = column.label.trim_end_matches(['¹', '²', '³']);
        parts.push(if label.is_empty() {
            text
        } else {
            format!("{label}: {text}")
        });
    }
    parts.join(", ")
}

unsafe fn row_flags(s: *mut State, row: usize) -> i32 {
    let hwnd = (*s).hwnd;
    let mut state = 0;
    if (*s).model.is_group(row) {
        state |= STATE_READONLY;
    } else {
        state |= STATE_SELECTABLE | STATE_FOCUSABLE;
        if (*s).selected == Some(row) {
            state |= STATE_SELECTED;
            if GetFocus() == hwnd {
                state |= STATE_FOCUSED;
            }
        }
        match (*s).model.expanded(row) {
            Some(true) => state |= STATE_EXPANDED,
            Some(false) => state |= STATE_COLLAPSED,
            None => {}
        }
    }
    // Off-screen: scrolled out of the view below the sticky header.
    let area = client(s);
    let header = (*s).model.header_height().clamp(0, area.bottom.max(0));
    let on_screen = row_rect(s, row).is_some_and(|r| r.bottom > header && r.top < area.bottom);
    if !on_screen || IsWindowVisible(hwnd) == 0 {
        state |= STATE_OFFSCREEN;
    }
    state
}

unsafe fn own_flags(s: *mut State) -> i32 {
    let hwnd = (*s).hwnd;
    let mut state = STATE_FOCUSABLE;
    if GetFocus() == hwnd {
        state |= STATE_FOCUSED;
    }
    if IsWindowVisible(hwnd) == 0 {
        state |= STATE_INVISIBLE;
    }
    if IsWindowEnabled(hwnd) == 0 {
        state |= STATE_UNAVAILABLE;
    }
    state
}

// ───────────────────────────── IUnknown / IDispatch ─────────────────────────────

unsafe extern "system" fn query_interface(
    this: This,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return HR_POINTER;
    }
    let iid = &*iid;
    if same(iid, &IID_IUNKNOWN) || same(iid, &IID_IDISPATCH) || same(iid, &IID_IACCESSIBLE) {
        add_ref(this);
        *out = this.cast();
        HR_OK
    } else {
        *out = null_mut();
        HR_NOINTERFACE
    }
}

unsafe extern "system" fn add_ref(this: This) -> u32 {
    (*this).refs.fetch_add(1, AtomicOrdering::Relaxed) + 1
}

unsafe extern "system" fn release(this: This) -> u32 {
    let left = (*this).refs.fetch_sub(1, AtomicOrdering::AcqRel) - 1;
    if left == 0 {
        drop(Box::from_raw(this));
    }
    left
}

unsafe extern "system" fn get_type_info_count(_this: This, count: *mut u32) -> HRESULT {
    if count.is_null() {
        return HR_POINTER;
    }
    *count = 0;
    HR_OK
}

unsafe extern "system" fn get_type_info(
    _this: This,
    _index: u32,
    _lcid: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    if !out.is_null() {
        *out = null_mut();
    }
    HR_NOTIMPL
}

unsafe extern "system" fn get_ids_of_names(
    _this: This,
    _iid: *const GUID,
    _names: *const *const u16,
    _count: u32,
    _lcid: u32,
    _ids: *mut i32,
) -> HRESULT {
    HR_NOTIMPL
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn invoke(
    _this: This,
    _id: i32,
    _iid: *const GUID,
    _lcid: u32,
    _flags: u16,
    _params: *mut c_void,
    _result: *mut Variant,
    _exception: *mut c_void,
    _arg: *mut u32,
) -> HRESULT {
    HR_NOTIMPL
}

// ───────────────────────────── IAccessible ─────────────────────────────

unsafe extern "system" fn acc_parent(this: This, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = null_mut();
    if live(this).is_none() {
        return HR_DISCONNECTED;
    }
    // The client area's parent is the window object.
    AccessibleObjectFromWindow((*this).hwnd, OBJID_WINDOW_ID, &IID_IACCESSIBLE, out)
}

unsafe extern "system" fn acc_child_count(this: This, out: *mut i32) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    let Some(s) = live(this) else {
        *out = 0;
        return HR_DISCONNECTED;
    };
    *out = (*s).count.min(i32::MAX as usize) as i32;
    HR_OK
}

unsafe extern "system" fn acc_child(this: This, v: Variant, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = null_mut();
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    match child(s, &v) {
        // Rows are simple elements: the client answers for them.
        Some(Child::Row(_)) => HR_FALSE,
        _ => HR_INVALIDARG,
    }
}

unsafe extern "system" fn acc_name(this: This, v: Variant, out: *mut BSTR) -> HRESULT {
    text_property(this, v, out, |s, which| match which {
        Child::Me => paint::window_text((*s).hwnd),
        Child::Row(row) => row_name(s, row),
    })
}

unsafe extern "system" fn acc_value(this: This, v: Variant, out: *mut BSTR) -> HRESULT {
    text_property(this, v, out, |_, _| String::new())
}

unsafe extern "system" fn acc_description(this: This, v: Variant, out: *mut BSTR) -> HRESULT {
    text_property(this, v, out, |s, which| match which {
        Child::Me => String::new(),
        Child::Row(row) => row_description(s, row),
    })
}

unsafe extern "system" fn acc_role(this: This, v: Variant, out: *mut Variant) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = Variant::empty();
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    let role = match child(s, &v) {
        Some(Child::Me) => ROLE_LIST,
        Some(Child::Row(row)) if (*s).model.is_group(row) => ROLE_GROUPING,
        Some(Child::Row(_)) => ROLE_LISTITEM,
        None => return HR_INVALIDARG,
    };
    put_variant(out, Variant::i4(role))
}

unsafe extern "system" fn acc_state(this: This, v: Variant, out: *mut Variant) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = Variant::empty();
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    let state = match child(s, &v) {
        Some(Child::Me) => own_flags(s),
        Some(Child::Row(row)) => row_flags(s, row),
        None => return HR_INVALIDARG,
    };
    put_variant(out, Variant::i4(state))
}

unsafe extern "system" fn acc_help(this: This, v: Variant, out: *mut BSTR) -> HRESULT {
    text_property(this, v, out, |_, _| String::new())
}

unsafe extern "system" fn acc_help_topic(
    this: This,
    file: *mut BSTR,
    _v: Variant,
    topic: *mut i32,
) -> HRESULT {
    if !file.is_null() {
        *file = null();
    }
    if !topic.is_null() {
        *topic = -1;
    }
    if live(this).is_none() {
        return HR_DISCONNECTED;
    }
    HR_FALSE
}

unsafe extern "system" fn acc_keyboard_shortcut(this: This, v: Variant, out: *mut BSTR) -> HRESULT {
    text_property(this, v, out, |_, _| String::new())
}

unsafe extern "system" fn acc_focus(this: This, out: *mut Variant) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = Variant::empty();
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    if GetFocus() != (*s).hwnd {
        return HR_FALSE;
    }
    put_variant(out, Variant::i4((*s).selected.map_or(SELF, child_id)))
}

unsafe extern "system" fn acc_selection(this: This, out: *mut Variant) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = Variant::empty();
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    match (*s).selected {
        Some(row) => put_variant(out, Variant::i4(child_id(row))),
        None => HR_FALSE,
    }
}

unsafe extern "system" fn acc_default_action(this: This, v: Variant, out: *mut BSTR) -> HRESULT {
    text_property(this, v, out, |s, which| match which {
        Child::Row(row) if !(*s).model.is_group(row) => tr("두 번 클릭", "Double click").into(),
        _ => String::new(),
    })
}

unsafe extern "system" fn acc_select(this: This, flags: i32, v: Variant) -> HRESULT {
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    let hwnd = (*s).hwnd;
    match child(s, &v) {
        Some(Child::Me) => {
            if flags & SELFLAG_TAKEFOCUS != 0 {
                SetFocus(hwnd);
            }
            HR_OK
        }
        Some(Child::Row(row)) if !(*s).model.is_group(row) => {
            if flags & SELFLAG_TAKEFOCUS != 0 {
                SetFocus(hwnd);
            }
            let take = SELFLAG_TAKESELECTION | SELFLAG_ADDSELECTION | SELFLAG_EXTENDSELECTION;
            if flags & take != 0 {
                select(s, Some(row), true);
                reveal(s, row, false);
            } else if flags & SELFLAG_REMOVESELECTION != 0 && (*s).selected == Some(row) {
                select(s, None, true);
            }
            HR_OK
        }
        Some(Child::Row(_)) => HR_MEMBERNOTFOUND,
        None => HR_INVALIDARG,
    }
}

unsafe extern "system" fn acc_location(
    this: This,
    left: *mut i32,
    top: *mut i32,
    width: *mut i32,
    height: *mut i32,
    v: Variant,
) -> HRESULT {
    if left.is_null() || top.is_null() || width.is_null() || height.is_null() {
        return HR_POINTER;
    }
    (*left, *top, *width, *height) = (0, 0, 0, 0);
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    let hwnd = (*s).hwnd;
    let mut r = match child(s, &v) {
        Some(Child::Me) => {
            let mut r: RECT = zeroed();
            GetWindowRect(hwnd, &mut r);
            r
        }
        Some(Child::Row(row)) => {
            let Some(mut r) = row_rect(s, row) else {
                return HR_INVALIDARG;
            };
            MapWindowPoints(hwnd, null_mut(), (&mut r as *mut RECT).cast::<POINT>(), 2);
            r
        }
        None => return HR_INVALIDARG,
    };
    if r.right < r.left {
        std::mem::swap(&mut r.left, &mut r.right);
    }
    (*left, *top, *width, *height) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
    HR_OK
}

unsafe extern "system" fn acc_navigate(
    this: This,
    direction: i32,
    start: Variant,
    out: *mut Variant,
) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = Variant::empty();
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    let count = (*s).count;
    let target = match (child(s, &start), direction) {
        (Some(Child::Me), NAVDIR_FIRSTCHILD) => (count > 0).then_some(0),
        (Some(Child::Me), NAVDIR_LASTCHILD) => count.checked_sub(1),
        // Siblings of the client belong to the window object.
        (Some(Child::Me), _) => None,
        (Some(Child::Row(_)), NAVDIR_FIRSTCHILD | NAVDIR_LASTCHILD) => return HR_INVALIDARG,
        (Some(Child::Row(row)), NAVDIR_NEXT | NAVDIR_DOWN) => (row + 1 < count).then_some(row + 1),
        (Some(Child::Row(row)), NAVDIR_PREVIOUS | NAVDIR_UP) => row.checked_sub(1),
        (Some(Child::Row(_)), _) => None,
        (None, _) => return HR_INVALIDARG,
    };
    match target {
        Some(row) => put_variant(out, Variant::i4(child_id(row))),
        None => HR_FALSE,
    }
}

unsafe extern "system" fn acc_hit_test(this: This, x: i32, y: i32, out: *mut Variant) -> HRESULT {
    if out.is_null() {
        return HR_POINTER;
    }
    *out = Variant::empty();
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    let mut pt = POINT { x, y };
    ScreenToClient((*s).hwnd, &mut pt);
    let area = client(s);
    if !contains(&area, pt) {
        return HR_FALSE;
    }
    let id = match hit(s, pt) {
        Hit::Row(row) | Hit::Group(row) => child_id(row),
        Hit::Header(_) | Hit::Nothing => SELF,
    };
    put_variant(out, Variant::i4(id))
}

unsafe extern "system" fn acc_do_default_action(this: This, v: Variant) -> HRESULT {
    let Some(s) = live(this) else {
        return HR_DISCONNECTED;
    };
    match child(s, &v) {
        Some(Child::Row(row)) if !(*s).model.is_group(row) => {
            // Posted: the action may open a dialog, which must not run
            // inside the client's (cross-process) call.
            PostMessageW((*s).hwnd, WM_ACTIVATE_ROW, row, 0);
            HR_OK
        }
        Some(_) => HR_MEMBERNOTFOUND,
        None => HR_INVALIDARG,
    }
}

unsafe extern "system" fn put_acc_name(_this: This, _v: Variant, _value: BSTR) -> HRESULT {
    HR_MEMBERNOTFOUND
}

unsafe extern "system" fn put_acc_value(_this: This, _v: Variant, _value: BSTR) -> HRESULT {
    HR_MEMBERNOTFOUND
}
