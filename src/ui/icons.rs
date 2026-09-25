//! Square vector artwork rasterized at build time. Cache premultiplied native
//! bitmaps so painting never reparses artwork or allocates a rendering engine.
use super::*;

mod masks {
    include!("../../assets/generated/masks.rs");
}

#[derive(Default)]
pub(super) struct IconCache {
    entries: VecDeque<Bitmap>,
}

impl IconCache {
    pub(super) unsafe fn draw(
        &mut self,
        dc: HDC,
        kind: usize,
        x: i32,
        y: i32,
        size: i32,
        color: u32,
    ) {
        let size = size.clamp(1, 256);
        if !self
            .entries
            .iter()
            .any(|b| b.kind == kind && b.size == size && b.color == color)
        {
            if let Some(bitmap) = Bitmap::new(kind, size, color) {
                if self.entries.len() >= 24 {
                    self.entries.pop_front();
                }
                self.entries.push_back(bitmap);
            }
        }
        if let Some(bitmap) = self
            .entries
            .iter()
            .find(|b| b.kind == kind && b.size == size && b.color == color)
        {
            GdiAlphaBlend(
                dc,
                x,
                y,
                size,
                size,
                bitmap.dc,
                0,
                0,
                size,
                size,
                BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                },
            );
        }
    }
}

struct Bitmap {
    kind: usize,
    size: i32,
    color: u32,
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
}

impl Bitmap {
    unsafe fn new(kind: usize, size: i32, color: u32) -> Option<Self> {
        let sources = match kind {
            0 => masks::PROCESSES,
            1 => masks::PERFORMANCE,
            2 => masks::STARTUP,
            3 => masks::SERVICES,
            _ => masks::FEATHER,
        };
        let &(source_size, encoded) = sources
            .iter()
            .find(|(n, _)| *n >= size)
            .unwrap_or(sources.last()?);
        let mut alpha = Vec::with_capacity((source_size * source_size) as usize);
        for pair in encoded.as_chunks::<2>().0 {
            alpha.extend(std::iter::repeat_n(pair[1], pair[0] as usize));
        }
        if alpha.len() != (source_size * source_size) as usize {
            return None;
        }
        let mut result = Self {
            kind,
            size,
            color,
            dc: CreateCompatibleDC(null_mut()),
            bitmap: null_mut(),
            previous: null_mut(),
        };
        if result.dc.is_null() {
            return None;
        }
        let mut info: BITMAPINFO = zeroed();
        info.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = size;
        info.bmiHeader.biHeight = -size;
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB;
        let mut bits = null_mut();
        result.bitmap =
            CreateDIBSection(result.dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if result.bitmap.is_null() || bits.is_null() {
            return None;
        }
        let old = SelectObject(result.dc, result.bitmap);
        if old.is_null() || old as isize == -1 {
            return None;
        }
        result.previous = old;
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), (size * size * 4) as usize);
        for y in 0..size {
            for x in 0..size {
                let a = if source_size == size {
                    alpha[(y * size + x) as usize]
                } else {
                    // Area sampling retains coverage at nonstandard DPI. It is
                    // performed once per cached size, not on each repaint.
                    coverage(&alpha, source_size, size, x, y)
                };
                let at = ((y * size + x) * 4) as usize;
                pixels[at] = ((((color >> 16) & 255) * a as u32 + 127) / 255) as u8;
                pixels[at + 1] = ((((color >> 8) & 255) * a as u32 + 127) / 255) as u8;
                pixels[at + 2] = (((color & 255) * a as u32 + 127) / 255) as u8;
                pixels[at + 3] = a;
            }
        }
        Some(result)
    }
}

fn coverage(alpha: &[u8], source: i32, target: i32, x: i32, y: i32) -> u8 {
    let ratio = source as f64 / target as f64;
    let x0 = x as f64 * ratio;
    let y0 = y as f64 * ratio;
    let x1 = (x + 1) as f64 * ratio;
    let y1 = (y + 1) as f64 * ratio;
    let mut sum = 0.0;
    for sy in y0.floor() as i32..(y1.ceil() as i32).min(source) {
        for sx in x0.floor() as i32..(x1.ceil() as i32).min(source) {
            let area = (x1.min((sx + 1) as f64) - x0.max(sx as f64))
                * (y1.min((sy + 1) as f64) - y0.max(sy as f64));
            sum += alpha[(sy * source + sx) as usize] as f64 * area;
        }
    }
    (sum / (ratio * ratio)).round().clamp(0.0, 255.0) as u8
}

impl Drop for Bitmap {
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
