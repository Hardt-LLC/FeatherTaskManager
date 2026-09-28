//! Resource-window caption: the existing frame geometry and drawing primitives,
//! with independent state and no animation/polling timer.
use super::*;
use windows_sys::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};

#[derive(Default)]
pub(super) struct Chrome {
    hot: Option<frame::Button>,
    pressed: Option<frame::Button>,
    active: bool,
    pub(super) top: i32,
    extend: i32,
}

fn buttons(dpi: i32, width: i32) -> [RECT; 3] {
    std::array::from_fn(|i| RECT {
        left: width - gfx::pxi(dpi, (3 - i) as f32 * 46.0),
        top: 0,
        right: width - gfx::pxi(dpi, (2 - i) as f32 * 46.0),
        bottom: gfx::pxi(dpi, 44.0),
    })
}

pub(super) unsafe fn apply_theme(s: *mut State) {
    let dark = i32::from(colors().dark);
    DwmSetWindowAttribute(
        (*s).hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
        (&dark as *const i32).cast(),
        4,
    );
    let color = colors().border;
    DwmSetWindowAttribute(
        (*s).hwnd,
        DWMWA_BORDER_COLOR as u32,
        (&color as *const u32).cast(),
        4,
    );
    let corner = DWMWCP_ROUND;
    DwmSetWindowAttribute(
        (*s).hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE as u32,
        (&corner as *const i32).cast(),
        4,
    );
}

pub(super) unsafe fn attach(s: *mut State) {
    apply_theme(s);
    (*s).chrome.active = true;
    SetWindowPos(
        (*s).hwnd,
        null_mut(),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
    );
}

unsafe fn map(s: *mut State) -> frame::HitMap {
    let mut client: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut client);
    let mut search: RECT = zeroed();
    if !(*s).search.is_null() {
        GetWindowRect((*s).search, &mut search);
        MapWindowPoints(null_mut(), (*s).hwnd, (&mut search as *mut RECT).cast(), 2);
        // The whole drawn search face is interactive, including its margins.
        search.left -= gfx::pxi((*s).dpi, 30.0);
        search.right += gfx::pxi((*s).dpi, 13.0);
        search.top = gfx::pxi((*s).dpi, 6.0);
        search.bottom = gfx::pxi((*s).dpi, 38.0);
    }
    let zoomed = IsZoomed((*s).hwnd) != 0;
    frame::HitMap {
        width: client.right,
        height: client.bottom,
        top_band: if zoomed {
            0
        } else {
            (frame::resize_border((*s).dpi as u32).1 - (*s).chrome.top).max(1)
        },
        corner: gfx::pxi((*s).dpi, frame::RESIZE_CORNER),
        resizable: !zoomed,
        titlebar_bottom: gfx::pxi((*s).dpi, 44.0),
        buttons: buttons((*s).dpi, client.right),
        search,
    }
}
fn point(l: LPARAM) -> POINT {
    POINT {
        x: (l as u32 & 0xffff) as i16 as i32,
        y: ((l as u32 >> 16) & 0xffff) as i16 as i32,
    }
}
unsafe fn button_at(s: *mut State, pt: POINT) -> Option<frame::Button> {
    frame::Button::from_hit(frame::hit_test(&map(s), pt.x, pt.y))
}
unsafe fn repaint(s: *mut State) {
    let mut r: RECT = zeroed();
    GetClientRect((*s).hwnd, &mut r);
    r.top = 0;
    r.bottom = gfx::pxi((*s).dpi, 44.0);
    InvalidateRect((*s).hwnd, &r, 0);
}
unsafe fn hot(s: *mut State, button: Option<frame::Button>) {
    if (*s).chrome.hot != button {
        (*s).chrome.hot = button;
        repaint(s);
    }
}
unsafe fn press(s: *mut State, button: frame::Button) {
    (*s).chrome.pressed = Some(button);
    hot(s, Some(button));
    repaint(s);
    SetCapture((*s).hwnd);
}
unsafe fn release(s: *mut State, over: Option<frame::Button>) {
    let pressed = (*s).chrome.pressed.take();
    ReleaseCapture();
    hot(s, over);
    repaint(s);
    if let Some(button) = pressed.filter(|b| Some(*b) == over) {
        let command = match button {
            frame::Button::Minimize => SC_MINIMIZE,
            frame::Button::Maximize => {
                if IsZoomed((*s).hwnd) != 0 {
                    SC_RESTORE
                } else {
                    SC_MAXIMIZE
                }
            }
            frame::Button::Close => SC_CLOSE,
        };
        PostMessageW((*s).hwnd, WM_SYSCOMMAND, command as usize, 0);
    }
}

pub(super) unsafe fn message(
    s: *mut State,
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_NCCALCSIZE if l != 0 && IsIconic(hwnd) == 0 => {
            let r = if w != 0 {
                &mut (*(l as *mut NCCALCSIZE_PARAMS)).rgrc[0]
            } else {
                &mut *(l as *mut RECT)
            };
            let maximized = IsZoomed(hwnd) != 0;
            let (work, autohide) = if maximized {
                frame::maximized_bounds(*r)
            } else {
                (None, [false; 4])
            };
            let top = frame::visible_border(hwnd);
            (*s).chrome.top = if maximized { 0 } else { top };
            (*s).chrome.extend = if !maximized && top == 0 { 1 } else { 0 };
            *r = frame::client_area(
                *r,
                frame::resize_border((*s).dpi as u32),
                top,
                maximized,
                work,
                autohide,
            );
            Some(0)
        }
        WM_GETDPISCALEDSIZE if IsZoomed(hwnd) == 0 && IsIconic(hwnd) == 0 && l != 0 => {
            // As for the main window: scale the client, not the captionless
            // window, or each monitor change adds a native caption's height.
            let (old, new) = (i64::from((*s).dpi.max(96)), i64::from((w as u32).max(96)));
            let mut client: RECT = zeroed();
            GetClientRect(hwnd, &mut client);
            let scale = |v: i32| ((i64::from(v) * new + old / 2) / old) as i32;
            let (width, height) = frame::outer_size(
                new as u32,
                scale(client.right),
                scale(client.bottom),
                (*s).chrome.top,
            );
            let size = &mut *(l as *mut SIZE);
            size.cx = width;
            size.cy = height;
            Some(1)
        }
        WM_SIZE => {
            let margin = MARGINS {
                cxLeftWidth: 0,
                cxRightWidth: 0,
                cyTopHeight: (*s).chrome.extend,
                cyBottomHeight: 0,
            };
            DwmExtendFrameIntoClientArea(hwnd, &margin);
            hot(s, None);
            None
        }
        WM_NCHITTEST => {
            let mut p = point(l);
            ScreenToClient(hwnd, &mut p);
            let hit = frame::hit_test(&map(s), p.x, p.y);
            Some(if matches!(hit, HTMINBUTTON | HTCLOSE) {
                HTCLIENT
            } else {
                hit
            } as isize)
        }
        WM_NCMOUSEMOVE => {
            if (*s).chrome.pressed.is_none() {
                hot(s, frame::Button::from_hit(w as u32));
            }
            let mut event = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE | TME_NONCLIENT,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut event);
            // Native handling of HTMAXBUTTON preserves Windows Snap Layouts.
            None
        }
        WM_MOUSEMOVE => {
            let over = button_at(s, point(l));
            hot(
                s,
                if let Some(held) = (*s).chrome.pressed {
                    over.filter(|b| *b == held)
                } else {
                    over
                },
            );
            if over.is_some() {
                let mut event = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut event);
            }
            None
        }
        WM_NCMOUSELEAVE => {
            let mut at: POINT = zeroed();
            GetCursorPos(&mut at);
            ScreenToClient(hwnd, &mut at);
            let onto = button_at(s, at);
            if (*s).chrome.pressed.is_none() && (onto.is_none() || onto != (*s).chrome.hot) {
                hot(s, None);
            }
            None
        }
        WM_MOUSELEAVE => {
            if (*s).chrome.pressed.is_none() {
                hot(s, None);
            }
            None
        }
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK => {
            let button = frame::Button::from_hit(w as u32)?;
            press(s, button);
            Some(0)
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
            let button = button_at(s, point(l))?;
            press(s, button);
            Some(0)
        }
        WM_LBUTTONUP if (*s).chrome.pressed.is_some() => {
            release(s, button_at(s, point(l)));
            Some(0)
        }
        WM_NCLBUTTONUP if (*s).chrome.pressed.is_some() => {
            release(s, frame::Button::from_hit(w as u32));
            Some(0)
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => {
            if (*s).chrome.pressed.take().is_some() {
                hot(s, None);
                repaint(s);
            }
            None
        }
        WM_NCACTIVATE => {
            (*s).chrome.active = w != 0;
            repaint(s);
            Some(DefWindowProcW(hwnd, msg, w, -1))
        }
        // Prevent uxtheme from drawing a second, classic caption.
        0x00AE | 0x00AF => Some(0),
        WM_GETTITLEBARINFOEX if l != 0 => {
            let info = &mut *(l as *mut TITLEBARINFOEX);
            let m = map(s);
            info.rcTitleBar = RECT {
                left: 0,
                top: 0,
                right: m.width,
                bottom: m.titlebar_bottom,
            };
            MapWindowPoints(
                hwnd,
                null_mut(),
                (&mut info.rcTitleBar as *mut RECT).cast(),
                2,
            );
            info.rgstate.fill(0);
            info.rgrect.fill(RECT::default());
            info.rgstate[1] = 0x8000;
            info.rgstate[4] = 0x8000;
            for (slot, (button, mut rect)) in [2, 3, 5]
                .into_iter()
                .zip(frame::Button::ALL.into_iter().zip(m.buttons))
            {
                MapWindowPoints(hwnd, null_mut(), (&mut rect as *mut RECT).cast(), 2);
                info.rgrect[slot] = rect;
                info.rgstate[slot] = 0x100000
                    | if (*s).chrome.hot == Some(button) {
                        0x80
                    } else {
                        0
                    }
                    | if (*s).chrome.pressed == Some(button) {
                        8
                    } else {
                        0
                    };
            }
            Some(0)
        }
        _ => None,
    }
}

pub(super) unsafe fn paint(s: *mut State, pt: &Painter, width: i32) {
    for (button, r) in frame::Button::ALL.into_iter().zip(buttons((*s).dpi, width)) {
        let kind = match button {
            frame::Button::Minimize => widgets::Caption::Minimize,
            frame::Button::Maximize => {
                if IsZoomed((*s).hwnd) != 0 {
                    widgets::Caption::Restore
                } else {
                    widgets::Caption::Maximize
                }
            }
            frame::Button::Close => widgets::Caption::Close,
        };
        widgets::caption_button(
            pt,
            r,
            kind,
            if (*s).chrome.hot == Some(button) {
                1.0
            } else {
                0.0
            },
            (*s).chrome.pressed == Some(button) && (*s).chrome.hot == Some(button),
            (*s).chrome.active,
            pt.c.bg,
        );
    }
    if (*s).chrome.extend > 0 {
        pt.fill(
            RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: (*s).chrome.extend,
            },
            0,
        );
    }
}
