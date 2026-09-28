//! Opt-in horizontal scrolling with the same colors as the vertical overlay.
//! Pointer movement drives updates; there is no animation or polling timer.
use super::*;

#[derive(Default)]
pub(super) struct Bar {
    hover: bool,
    grab: Option<f32>,
}

pub(super) unsafe fn height(s: *mut State) -> i32 {
    if (*s).model.themed_horizontal() && (*s).horizontal_max > 0 {
        gfx::pxi((*s).model.dpi(), 14.0)
    } else {
        0
    }
}

unsafe fn geometry(s: *mut State) -> (RECT, (f32, f32)) {
    let area = client(s);
    let lane = RECT {
        top: (area.bottom - height(s)).max(0),
        ..area
    };
    let thumb = widgets::scroll_thumb(
        area.right.max(0) as f32,
        (*s).horizontal_page as f32,
        ((*s).horizontal_page + (*s).horizontal_max) as f32,
        (*s).horizontal_offset as f32,
        gfx::px((*s).model.dpi(), widgets::SCROLL_THUMB_MIN),
    );
    (lane, thumb)
}

pub(super) unsafe fn paint(s: *mut State, pt: &Painter) {
    if height(s) == 0 {
        return;
    }
    let (lane, thumb) = geometry(s);
    pt.fill(lane, pt.c.surface);
    let hot = (*s).horizontal_bar.hover || (*s).horizontal_bar.grab.is_some();
    let thickness = pt.px(if hot { 8.0 } else { 3.0 });
    let bottom = lane.bottom as f32 - pt.px(2.0);
    if hot {
        pt.canvas.fill_round_rect(
            gfx::RectF::new(
                lane.left as f32,
                bottom - pt.px(8.0),
                (lane.right - lane.left) as f32,
                pt.px(8.0),
            ),
            pt.px(4.0),
            theme::argb(pt.c.fg, 0.05),
        );
    }
    pt.canvas.fill_round_rect(
        gfx::RectF::new(
            lane.left as f32 + thumb.0,
            bottom - thickness,
            thumb.1,
            thickness,
        ),
        thickness / 2.0,
        theme::argb(pt.c.muted, if hot { 0.7 } else { 0.5 }),
    );
}

pub(super) unsafe fn message(s: *mut State, msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    if !(*s).model.themed_horizontal() {
        return None;
    }
    let hwnd = (*s).hwnd;
    let (lane, thumb) = geometry(s);
    let visible = height(s) > 0;
    match msg {
        WM_MOUSEMOVE => {
            let p = point(l);
            if let Some(grab) = (*s).horizontal_bar.grab {
                let offset = widgets::scroll_offset_for_thumb(
                    lane.right.max(0) as f32,
                    (*s).horizontal_page as f32,
                    ((*s).horizontal_page + (*s).horizontal_max) as f32,
                    p.x as f32 - lane.left as f32 - grab,
                    gfx::px((*s).model.dpi(), widgets::SCROLL_THUMB_MIN),
                );
                horizontal_to(s, offset.round() as i32);
                return Some(0);
            }
            let hover = visible && contains(&lane, p);
            if hover != (*s).horizontal_bar.hover {
                (*s).horizontal_bar.hover = hover;
                InvalidateRect(hwnd, &lane, 0);
            }
            if hover {
                set_row_hover(s, None);
                set_header_hover(s, None);
                set_chevron_hover(s, None);
                let mut track = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut track);
                return Some(0);
            }
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK if visible && contains(&lane, point(l)) => {
            SetFocus(hwnd);
            let x = (point(l).x - lane.left) as f32;
            if x >= thumb.0 && x < thumb.0 + thumb.1 {
                (*s).horizontal_bar.grab = Some(x - thumb.0);
                SetCapture(hwnd);
            } else {
                let page = (*s).horizontal_page;
                horizontal_to(
                    s,
                    (*s).horizontal_offset + if x < thumb.0 { -page } else { page },
                );
            }
            InvalidateRect(hwnd, &lane, 0);
            return Some(0);
        }
        WM_LBUTTONUP if (*s).horizontal_bar.grab.take().is_some() => {
            ReleaseCapture();
            InvalidateRect(hwnd, &lane, 0);
            return Some(0);
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            if (*s).horizontal_bar.grab.take().is_some() {
                if GetCapture() == hwnd {
                    ReleaseCapture();
                }
                InvalidateRect(hwnd, &lane, 0);
            }
        }
        WM_MOUSELEAVE => {
            if (*s).horizontal_bar.hover {
                (*s).horizontal_bar.hover = false;
                InvalidateRect(hwnd, &lane, 0);
            }
        }
        WM_KEYDOWN if visible && matches!(w as u16, VK_LEFT | VK_RIGHT) => {
            let direction = if w as u16 == VK_LEFT { -1 } else { 1 };
            horizontal_to(
                s,
                (*s).horizontal_offset + direction * gfx::pxi((*s).model.dpi(), 32.0),
            );
            return Some(0);
        }
        WM_SETCURSOR if visible && (l & 0xffff) as u32 == HTCLIENT => {
            let mut p: POINT = zeroed();
            GetCursorPos(&mut p);
            ScreenToClient(hwnd, &mut p);
            if contains(&lane, p) || (*s).horizontal_bar.grab.is_some() {
                SetCursor(LoadCursorW(null_mut(), IDC_ARROW));
                return Some(1);
            }
        }
        _ => {}
    }
    None
}
