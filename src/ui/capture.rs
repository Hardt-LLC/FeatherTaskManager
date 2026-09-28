//! Internal render verification for Feather Task's own client area. This paints
//! the app and its native child controls into a memory bitmap; it never reads
//! pixels from the desktop or another application's window.

use super::*;
// Preview files are created like every diagnostic output: never through a
// link planted in the preview folder (`--render-previews` may run elevated).
use crate::create_output_file;
use std::io::{BufWriter, Write};
use std::path::Path;

const MAX_DIMENSION: i32 = 16_384;
const MAX_BITMAP_BYTES: usize = 128 * 1024 * 1024;
const BMP_HEADER_BYTES: u32 = 14 + 40;

/// Must run on the window's owning UI thread with a valid, fully laid-out App.
/// Root WM_PRINTCLIENT calls paint::paint_to. Direct children are printed at
/// client-relative positions explicitly: DefWindowProc's recursive WM_PRINT
/// path otherwise includes the hidden root's non-client origin in this DIB.
pub(super) unsafe fn save_client(p: *mut App, path: &Path) -> Result<(), String> {
    if p.is_null() || (*p).hwnd.is_null() {
        return Err("Preview requires an initialized application window".into());
    }
    // Settle the task manager's animated controls before rendering.
    (*p).anim.finish_all();
    table::settle((*p).list);
    table::settle((*p).perf_list);
    save_window((*p).hwnd, path)
}

/// Render only a Feather-owned window and its own child controls.
pub(super) unsafe fn save_window(hwnd: HWND, path: &Path) -> Result<(), String> {
    let mut rect: RECT = zeroed();
    if GetClientRect(hwnd, &mut rect) == 0 {
        return Err(win32_error("GetClientRect"));
    }
    let width = rect
        .right
        .checked_sub(rect.left)
        .ok_or("Invalid preview width")?;
    let height = rect
        .bottom
        .checked_sub(rect.top)
        .ok_or("Invalid preview height")?;
    // A preview is a settled state: running tweens (a page switch's nav
    // slide, hover fades) jump to their end — no message loop ticks them
    // here. Stage a mid-animation state with `anim.set` instead.
    let surface = Surface::new(width, height)?;
    // Initialize every pixel, including pixels outside any native child's paint
    // region. Root WM_PRINTCLIENT paints the real app background over this.
    std::ptr::write_bytes(surface.bits, 255, surface.bytes);
    paint_client_and_children(hwnd, surface.dc)?;
    // GDI may batch writes; complete them before CPU access to the DIB memory.
    if GdiFlush() == 0 {
        return Err(win32_error("GdiFlush"));
    }
    let pixels = std::slice::from_raw_parts_mut(surface.bits, surface.bytes);
    // BI_RGB ignores alpha, but some image viewers honor it. GDI's standard
    // controls may clear the reserved byte, so make the preview fully opaque.
    for pixel in pixels.as_chunks_mut::<4>().0.iter_mut() {
        pixel[3] = 255;
    }
    let file = create_output_file(path).map_err(|error| format!("Create preview: {error}"))?;
    let mut output = BufWriter::new(file);
    write_bmp(&mut output, width, height, pixels)
        .and_then(|()| output.flush())
        .map_err(|error| format!("Write preview: {error}"))
}

pub(super) unsafe fn paint_client_and_children(hwnd: HWND, dc: HDC) -> Result<(), String> {
    let saved = SaveDC(dc);
    if saved == 0 {
        return Err(win32_error("SaveDC root"));
    }
    SendMessageW(hwnd, WM_PRINTCLIENT, dc as WPARAM, PRF_CLIENT as LPARAM);
    RestoreDC(dc, saved);

    // Enumerate only direct children. Their own WM_PRINT implementation paints
    // nested parts such as a ListView's header or a ComboBox's selected label.
    // WS_VISIBLE is intentional: IsWindowVisible also tests the hidden root.
    let mut children = Vec::new();
    let mut child = GetWindow(hwnd, GW_CHILD);
    while !child.is_null() {
        if GetWindowLongW(child, GWL_STYLE) as u32 & WS_VISIBLE != 0 {
            children.push(child);
        }
        child = GetWindow(child, GW_HWNDNEXT);
    }
    // Paint lower siblings before higher siblings, matching native z-order.
    for child in children.into_iter().rev() {
        let mut bounds: RECT = zeroed();
        if GetWindowRect(child, &mut bounds) == 0 {
            return Err(win32_error("GetWindowRect child"));
        }
        let mut origin = POINT {
            x: bounds.left,
            y: bounds.top,
        };
        if ScreenToClient(hwnd, &mut origin) == 0 {
            return Err(win32_error("ScreenToClient child"));
        }
        let width = bounds.right - bounds.left;
        let height = bounds.bottom - bounds.top;
        if width <= 0 || height <= 0 {
            continue;
        }
        let saved = SaveDC(dc);
        if saved == 0 {
            return Err(win32_error("SaveDC child"));
        }
        SetWindowOrgEx(dc, 0, 0, null_mut());
        SetViewportOrgEx(dc, origin.x, origin.y, null_mut());
        IntersectClipRect(dc, 0, 0, width, height);
        let print_state = SaveDC(dc);
        SendMessageW(
            child,
            WM_PRINT,
            dc as WPARAM,
            (PRF_CLIENT | PRF_NONCLIENT | PRF_CHILDREN | PRF_ERASEBKGND) as LPARAM,
        );
        if print_state != 0 {
            RestoreDC(dc, print_state);
        }
        RestoreDC(dc, saved);
    }
    Ok(())
}

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    bytes: usize,
}

impl Surface {
    unsafe fn new(width: i32, height: i32) -> Result<Self, String> {
        let bytes = bitmap_bytes(width, height)?;
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return Err(win32_error("CreateCompatibleDC"));
        }
        // Construct the guard before subsequent fallible calls so both the DC
        // and any successfully-created bitmap are always released.
        let mut surface = Self {
            dc,
            bitmap: null_mut(),
            previous: null_mut(),
            bits: null_mut(),
            bytes,
        };
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = width;
        info.bmiHeader.biHeight = -height; // Top-down DIB.
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        info.bmiHeader.biSizeImage = bytes as u32;
        let mut bits = null_mut();
        surface.bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if surface.bitmap.is_null() || bits.is_null() {
            return Err(win32_error("CreateDIBSection"));
        }
        surface.bits = bits.cast();
        let previous = SelectObject(dc, surface.bitmap);
        if previous.is_null() || previous as isize == -1 {
            return Err(win32_error("SelectObject"));
        }
        surface.previous = previous;
        Ok(surface)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_null() {
                SelectObject(self.dc, self.previous);
            }
            if !self.bitmap.is_null() {
                DeleteObject(self.bitmap);
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}

fn bitmap_bytes(width: i32, height: i32) -> Result<usize, String> {
    if !(1..=MAX_DIMENSION).contains(&width) || !(1..=MAX_DIMENSION).contains(&height) {
        return Err("Preview dimensions exceed the safety limit".into());
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|&bytes| bytes <= MAX_BITMAP_BYTES)
        .ok_or("Preview bitmap exceeds the memory limit")?;
    Ok(bytes)
}

fn write_bmp(
    output: &mut impl Write,
    width: i32,
    height: i32,
    pixels: &[u8],
) -> std::io::Result<()> {
    let expected = bitmap_bytes(width, height)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if pixels.len() != expected {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Preview pixel size does not match its dimensions",
        ));
    }
    // Explicit fields avoid Rust struct padding in the 14-byte BITMAPFILEHEADER.
    output.write_all(b"BM")?;
    output.write_all(&(BMP_HEADER_BYTES + expected as u32).to_le_bytes())?;
    output.write_all(&0u32.to_le_bytes())?; // Both reserved WORDs.
    output.write_all(&BMP_HEADER_BYTES.to_le_bytes())?;
    output.write_all(&40u32.to_le_bytes())?; // BITMAPINFOHEADER.
    output.write_all(&width.to_le_bytes())?;
    output.write_all(&(-height).to_le_bytes())?;
    output.write_all(&1u16.to_le_bytes())?; // Planes.
    output.write_all(&32u16.to_le_bytes())?;
    output.write_all(&BI_RGB.to_le_bytes())?;
    output.write_all(&(expected as u32).to_le_bytes())?;
    output.write_all(&0i32.to_le_bytes())?; // Horizontal pixels/meter.
    output.write_all(&0i32.to_le_bytes())?; // Vertical pixels/meter.
    output.write_all(&0u32.to_le_bytes())?; // Color table entries.
    output.write_all(&0u32.to_le_bytes())?; // Important colors.
    output.write_all(pixels)
}

/// Foundation previews for review: the component gallery (light, dark,
/// 150 %), font specimens for both languages and a layered popup with its
/// soft shadow composited over the window background.
pub(super) unsafe fn save_foundation_previews(dir: &Path) -> Result<(), String> {
    use super::theme::{DARK_PALETTE, LIGHT};
    use super::widgets::{gallery, Painter, GALLERY_DIP};
    for (dpi, palette, name) in [
        (96, LIGHT, "components-light.bmp"),
        (96, DARK_PALETTE, "components-dark.bmp"),
        (144, LIGHT, "components-dpi150.bmp"),
    ] {
        let fonts = fonts::Fonts::new(dpi, language());
        let mut dib = gfx::Dib::new(
            gfx::pxi(dpi, GALLERY_DIP.0 as f32),
            gfx::pxi(dpi, GALLERY_DIP.1 as f32),
        )
        .ok_or("Gallery surface allocation failed")?;
        {
            let pt = Painter::new(dib.dc(), dpi, &fonts).with_palette(palette);
            gallery(&pt);
        }
        save_dib(&mut dib, &dir.join(name))?;
    }
    for (lang, name) in [
        (Language::English, "fonts-en.bmp"),
        (Language::Korean, "fonts-ko.bmp"),
    ] {
        let mut dib = gfx::Dib::new(900, 560).ok_or("Specimen allocation failed")?;
        font_specimen(&mut dib, lang);
        save_dib(&mut dib, &dir.join(name))?;
    }
    for (palette, name) in [(LIGHT, "popup-light.bmp"), (DARK_PALETTE, "popup-dark.bmp")] {
        let mut dib = popup_sample(palette, 96).ok_or("Popup sample failed")?;
        save_dib(&mut dib, &dir.join(name))?;
    }
    Ok(())
}

/// Every font role with its resolved family, in one language.
unsafe fn font_specimen(dib: &mut gfx::Dib, lang: Language) {
    use super::fonts::{resolve, Fonts, Role};
    use super::theme::LIGHT;
    use super::widgets::Painter;
    let fonts = Fonts::new(96, lang);
    let pt = Painter::new(dib.dc(), 96, &fonts).with_palette(LIGHT);
    pt.fill(
        RECT {
            left: 0,
            top: 0,
            right: dib.width(),
            bottom: dib.height(),
        },
        LIGHT.surface,
    );
    let korean = lang == Language::Korean;
    let mut y = 8;
    for role in Role::ALL {
        let (family, weight) = resolve(role, lang);
        let (_, size, _) = role.spec();
        let sample = match (role, korean) {
            (Role::MonoCell | Role::MonoTotal | Role::MonoStat, _) => "1,410.4 MB  30.1%  0.4 MB/s",
            (Role::MonoSmall, true) => "568개 프로세스 · CPU 26.7%",
            (Role::MonoSmall, false) => "568 processes · CPU 26.7%",
            (Role::MonoTiny, _) => "Ctrl K  564",
            (Role::MonoBadge, _) => "GC VS FT",
            (_, true) => "프로세스 성능 시작 앱 서비스 설정 · Processes",
            (_, false) => "Processes Performance Startup apps · 0123",
        };
        pt.label(
            fonts.small,
            LIGHT.muted,
            &format!("{role:?} {size}px → {family} {weight}"),
            RECT {
                left: 8,
                top: y,
                right: 330,
                bottom: y + 30,
            },
            DT_LEFT,
        );
        pt.label(
            fonts.get(role),
            LIGHT.fg,
            sample,
            RECT {
                left: 336,
                top: y,
                right: 892,
                bottom: y + 30,
            },
            DT_LEFT,
        );
        y += 32;
    }
}

/// A context-menu-like layered popup composed with `gfx::LayeredSurface`
/// and alpha-blended over the window background (as DWM would show it).
unsafe fn popup_sample(palette: theme::Palette, dpi: i32) -> Option<gfx::Dib> {
    use super::widgets::{kbd, Painter};
    let fonts = fonts::Fonts::new(dpi, language());
    let s = |v: i32| gfx::pxi(dpi, v as f32);
    let layers = gfx::popup_shadow(dpi);
    let mut surface = gfx::LayeredSurface::new(s(208), s(4 + 32 * 4 + 9 + 4), &layers)?;
    let content = surface.content();
    surface.fill_frame(
        palette.surface,
        palette.border,
        gfx::px(dpi, theme::RADIUS),
        gfx::hairline(dpi),
    );
    {
        let pt = Painter::new(surface.dc(), dpi, &fonts).with_palette(palette);
        let items = [
            ("End task", Some("Del"), true, false),
            ("Efficiency mode", None, false, false),
            ("Open file location", None, false, true),
        ];
        let mut y = content.top + s(4);
        for (i, (text, hint, hot, disabled)) in items.into_iter().enumerate() {
            let item = RECT {
                left: content.left + s(4),
                top: y,
                right: content.right - s(4),
                bottom: y + s(32),
            };
            if hot {
                pt.canvas.fill_round_rect(
                    gfx::RectF::from_rect(item),
                    pt.px(theme::RADIUS_SM),
                    theme::solid(palette.fg_sel),
                );
            }
            pt.label(
                fonts.ui,
                if disabled { palette.muted } else { palette.fg },
                text,
                RECT {
                    left: item.left + s(10),
                    ..item
                },
                DT_LEFT,
            );
            if let Some(hint) = hint {
                kbd(&pt, item.right - s(10), (item.top + item.bottom) / 2, hint);
            }
            y += s(32);
            if i == 1 {
                pt.fill(
                    RECT {
                        left: content.left + s(4),
                        top: y + s(4),
                        right: content.right - s(4),
                        bottom: y + s(4) + gfx::hairline(dpi) as i32,
                    },
                    palette.border,
                );
                y += s(9);
            }
        }
        pt.label(
            fonts.ui,
            palette.fg,
            "Copy PID",
            RECT {
                left: content.left + s(14),
                top: y,
                right: content.right - s(14),
                bottom: y + s(32),
            },
            DT_LEFT,
        );
        kbd(&pt, content.right - s(14), y + s(16), "4812");
    }
    surface.compose(gfx::px(dpi, theme::RADIUS), palette.shadow());
    let size = surface.size();
    let mut output = gfx::Dib::new(size.cx + s(40), size.cy + s(40))?;
    output
        .pixels()
        .fill(0xff00_0000 | theme::solid(palette.bg).0);
    GdiAlphaBlend(
        output.dc(),
        s(20),
        s(20),
        size.cx,
        size.cy,
        surface.dc(),
        0,
        0,
        size.cx,
        size.cy,
        BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        },
    );
    Some(output)
}

/// Save a foundation DIB (gallery, specimen, popup previews) as an opaque BMP.
pub(super) fn save_dib(dib: &mut gfx::Dib, path: &Path) -> Result<(), String> {
    let (width, height) = (dib.width(), dib.height());
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for pixel in dib.pixels().iter() {
        pixels.extend_from_slice(&(pixel | 0xff00_0000).to_le_bytes());
    }
    let file = create_output_file(path).map_err(|error| format!("Create preview: {error}"))?;
    let mut output = BufWriter::new(file);
    write_bmp(&mut output, width, height, &pixels)
        .and_then(|()| output.flush())
        .map_err(|error| format!("Write preview: {error}"))
}

fn win32_error(operation: &str) -> String {
    format!("{operation}: {}", std::io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bmp_has_packed_header_and_top_down_pixels() {
        let pixels = [3, 2, 1, 255, 30, 20, 10, 255];
        let mut output = Vec::new();
        write_bmp(&mut output, 1, 2, &pixels).unwrap();
        assert_eq!(&output[..2], b"BM");
        assert_eq!(output.len(), 54 + pixels.len());
        assert_eq!(u32::from_le_bytes(output[2..6].try_into().unwrap()), 62);
        assert_eq!(u32::from_le_bytes(output[10..14].try_into().unwrap()), 54);
        assert_eq!(i32::from_le_bytes(output[18..22].try_into().unwrap()), 1);
        assert_eq!(i32::from_le_bytes(output[22..26].try_into().unwrap()), -2);
        assert_eq!(&output[54..], &pixels);
    }

    #[test]
    fn preview_rejects_empty_oversized_or_inconsistent_buffers() {
        assert!(bitmap_bytes(0, 100).is_err());
        assert!(bitmap_bytes(-1, 100).is_err());
        assert!(bitmap_bytes(16_384, 16_384).is_err());
        assert!(bitmap_bytes(i32::MAX, 1).is_err());
        assert!(write_bmp(&mut Vec::new(), 2, 2, &[0; 4]).is_err());
    }

    #[test]
    fn hidden_client_children_use_client_coordinates_and_isolated_dc_state() {
        unsafe extern "system" fn fixture_proc(
            hwnd: HWND,
            msg: u32,
            w: WPARAM,
            l: LPARAM,
        ) -> LRESULT {
            if msg == WM_PRINTCLIENT || msg == WM_PRINT {
                let color = match unsafe { GetDlgCtrlID(hwnd) } {
                    1 => rgb(10, 190, 30),
                    2 => rgb(200, 20, 70),
                    _ => rgb(20, 40, 160),
                };
                unsafe {
                    let mut bounds: RECT = zeroed();
                    GetClientRect(hwnd, &mut bounds);
                    let brush = CreateSolidBrush(color);
                    FillRect(w as HDC, &bounds, brush);
                    DeleteObject(brush);
                    // Each control is allowed to alter its supplied DC. The
                    // capture caller must restore it before the next control.
                    SetViewportOrgEx(w as HDC, 400, 500, null_mut());
                }
                0
            } else {
                unsafe { DefWindowProcW(hwnd, msg, w, l) }
            }
        }
        unsafe {
            let instance = GetModuleHandleW(null());
            let class = wide("FeatherCaptureCoordinatesTest");
            assert_ne!(
                RegisterClassExW(&WNDCLASSEXW {
                    cbSize: size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(fixture_proc),
                    hInstance: instance,
                    lpszClassName: class.as_ptr(),
                    ..zeroed()
                }),
                0
            );
            let root = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("Hidden own-app capture fixture").as_ptr(),
                WS_OVERLAPPEDWINDOW,
                50,
                50,
                240,
                200,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            assert!(!root.is_null());
            let first = CreateWindowExW(
                0,
                class.as_ptr(),
                null(),
                WS_CHILD | WS_VISIBLE,
                20,
                30,
                40,
                40,
                root,
                1 as HMENU,
                instance,
                null(),
            );
            let second = CreateWindowExW(
                0,
                class.as_ptr(),
                null(),
                WS_CHILD | WS_VISIBLE,
                80,
                30,
                40,
                40,
                root,
                2 as HMENU,
                instance,
                null(),
            );
            let hidden = CreateWindowExW(
                0,
                class.as_ptr(),
                null(),
                WS_CHILD,
                20,
                80,
                40,
                40,
                root,
                2 as HMENU,
                instance,
                null(),
            );
            assert!(!first.is_null() && !second.is_null() && !hidden.is_null());
            assert_eq!(IsWindowVisible(root), 0);
            let mut bounds: RECT = zeroed();
            GetClientRect(root, &mut bounds);
            let surface = Surface::new(bounds.right, bounds.bottom).unwrap();
            paint_client_and_children(root, surface.dc).unwrap();
            GdiFlush();
            let pixel = |x: usize, y: usize| {
                let offset = (y * bounds.right as usize + x) * 4;
                rgb(
                    *surface.bits.add(offset + 2) as u32,
                    *surface.bits.add(offset + 1) as u32,
                    *surface.bits.add(offset) as u32,
                )
            };
            assert_eq!(pixel(21, 31), rgb(10, 190, 30));
            assert_eq!(pixel(81, 31), rgb(200, 20, 70));
            assert_eq!(pixel(21, 81), rgb(20, 40, 160));
            assert_eq!(pixel(61, 31), rgb(20, 40, 160));
            DestroyWindow(root);
            UnregisterClassW(class.as_ptr(), instance);
        }
    }
}
