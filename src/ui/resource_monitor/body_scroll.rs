//! A themed overlay scrollbar. Input-driven only: no animation or idle timer.
use super::*;

#[derive(Default)]
pub(super) struct ScrollState {
    hover: bool,
    grab: Option<f32>,
}

unsafe fn geometry(s: *mut State) -> (RECT, RECT, (f32, f32)) {
    let mut area: RECT = zeroed();
    GetClientRect((*s).body, &mut area);
    let lane = scroll::lane(area, (*s).dpi);
    let thumb = widgets::scroll_thumb(
        (lane.bottom - lane.top) as f32,
        area.bottom as f32,
        (*s).content_height as f32,
        (*s).scroll as f32,
        gfx::px((*s).dpi, 24.0),
    );
    (area, lane, thumb)
}
fn point(l: LPARAM) -> POINT {
    POINT {
        x: (l & 0xffff) as u16 as i16 as i32,
        y: ((l >> 16) & 0xffff) as u16 as i16 as i32,
    }
}
fn contains(r: RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}
unsafe fn update(s: *mut State, offset: i32) {
    (*s).scroll = offset;
    view::layout_body(s);
    InvalidateRect((*s).body, null(), 0);
}
/// Scroll the panel list so the panel holding `focus` (its header, or its
/// table from the header down) is in view, with the list's 16 px margin.
pub(super) unsafe fn reveal(s: *mut State, focus: HWND) {
    let Some(panel) = (*s).panels.iter().find(|p| {
        p.header == focus
            || (!p.table.is_null() && (p.table == focus || IsChild(p.table, focus) != 0))
    }) else {
        return;
    };
    let px = |v| gfx::pxi((*s).dpi, v);
    let (top, bottom) = if panel.header == focus {
        (panel.bounds.top, panel.bounds.top + px(44.0))
    } else {
        (panel.bounds.top, panel.bounds.bottom)
    };
    let mut area: RECT = zeroed();
    GetClientRect((*s).body, &mut area);
    let mut offset = (*s).scroll;
    if bottom + px(16.0) > offset + area.bottom {
        offset = bottom + px(16.0) - area.bottom;
    }
    // A panel taller than the view shows from its header.
    if top - px(16.0) < offset {
        offset = top - px(16.0);
    }
    if offset != (*s).scroll {
        update(s, offset.max(0));
    }
}
pub(super) unsafe fn paint(s: *mut State, pt: &Painter) {
    let (area, lane, thumb) = geometry(s);
    if (*s).content_height <= area.bottom {
        return;
    }
    widgets::scrollbar(
        pt,
        lane,
        thumb,
        if (*s).body_scroll.hover || (*s).body_scroll.grab.is_some() {
            1.0
        } else {
            0.0
        },
        1.0,
    );
}
pub(super) unsafe fn message(
    s: *mut State,
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_GETDLGCODE => Some(DLGC_WANTARROWS as isize),
        WM_MOUSEWHEEL => {
            let delta = ((w >> 16) & 0xffff) as i16 as i32;
            let (area, _, _) = geometry(s);
            let distance = scroll::wheel_pixels(
                delta,
                scroll::wheel_lines(),
                gfx::px((*s).dpi, 32.0),
                area.bottom as f32,
            );
            update(s, (*s).scroll - distance.round() as i32);
            Some(0)
        }
        WM_MOUSEMOVE => {
            let (area, lane, _) = geometry(s);
            let p = point(l);
            if let Some(grab) = (*s).body_scroll.grab {
                let offset = widgets::scroll_offset_for_thumb(
                    (lane.bottom - lane.top) as f32,
                    area.bottom as f32,
                    (*s).content_height as f32,
                    p.y as f32 - lane.top as f32 - grab,
                    gfx::px((*s).dpi, 24.0),
                );
                update(s, offset.round() as i32);
            } else {
                let hover = (*s).content_height > area.bottom && contains(lane, p);
                if hover != (*s).body_scroll.hover {
                    (*s).body_scroll.hover = hover;
                    InvalidateRect(hwnd, &lane, 0);
                }
            }
            let mut track = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut track);
            Some(0)
        }
        WM_MOUSELEAVE => {
            if (*s).body_scroll.hover {
                (*s).body_scroll.hover = false;
                InvalidateRect(hwnd, null(), 0);
            }
            Some(0)
        }
        WM_LBUTTONDOWN => {
            let (area, lane, thumb) = geometry(s);
            let p = point(l);
            SetFocus(hwnd);
            if (*s).content_height > area.bottom && contains(lane, p) {
                let y = p.y as f32 - lane.top as f32;
                if y >= thumb.0 && y < thumb.0 + thumb.1 {
                    (*s).body_scroll.grab = Some(y - thumb.0);
                    SetCapture(hwnd);
                } else {
                    update(
                        s,
                        (*s).scroll
                            + if y < thumb.0 {
                                -area.bottom
                            } else {
                                area.bottom
                            },
                    );
                }
                InvalidateRect(hwnd, &lane, 0);
                Some(0)
            } else {
                None
            }
        }
        WM_LBUTTONUP => {
            if (*s).body_scroll.grab.take().is_some() {
                ReleaseCapture();
                InvalidateRect(hwnd, null(), 0);
                Some(0)
            } else {
                None
            }
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            if (*s).body_scroll.grab.take().is_some() {
                if GetCapture() == hwnd {
                    ReleaseCapture();
                }
                InvalidateRect(hwnd, null(), 0);
            }
            Some(0)
        }
        WM_KEYDOWN => {
            let (area, _, _) = geometry(s);
            let line = gfx::pxi((*s).dpi, 32.0);
            let next = match w as u16 {
                VK_UP => (*s).scroll - line,
                VK_DOWN => (*s).scroll + line,
                VK_PRIOR => (*s).scroll - area.bottom,
                VK_NEXT => (*s).scroll + area.bottom,
                VK_HOME => 0,
                VK_END => (*s).content_height,
                _ => return None,
            };
            update(s, next);
            Some(0)
        }
        WM_VSCROLL => {
            let (area, _, _) = geometry(s);
            let line = gfx::pxi((*s).dpi, 32.0);
            let next = match (w & 0xffff) as i32 {
                SB_LINEUP => (*s).scroll - line,
                SB_LINEDOWN => (*s).scroll + line,
                SB_PAGEUP => (*s).scroll - area.bottom,
                SB_PAGEDOWN => (*s).scroll + area.bottom,
                SB_TOP => 0,
                SB_BOTTOM => (*s).content_height,
                _ => return Some(0),
            };
            update(s, next);
            Some(0)
        }
        _ => None,
    }
}
