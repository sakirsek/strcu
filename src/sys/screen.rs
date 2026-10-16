//! Screenshots (GDI BitBlt). Verified to work while the display is off, too.

use anyhow::{Result, bail};
use image::{DynamicImage, RgbaImage};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HDC, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, GetSystemMetrics, HICON, ICONINFO,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::Rect;
use crate::i18n::Msg;

/// Virtual desktop rectangle covering all monitors (physical pixels).
pub fn virtual_screen() -> Rect {
    unsafe {
        Rect {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// The pixels of `area` as Windows gives them: BGRA, rows top to bottom, alpha meaningless. With `cursor` the
/// mouse pointer is drawn in where it is on the screen (a screenshot does not include it).
pub fn grab(area: Rect, cursor: bool) -> Result<Vec<u8>> {
    if area.w <= 0 || area.h <= 0 {
        bail!("invalid area: {area:?}");
    }
    let mut buf = vec![0u8; area.w as usize * area.h as usize * 4];
    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            bail!(Msg::new("err.screen_unavailable"));
        }
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let bmp = CreateCompatibleBitmap(screen_dc, area.w, area.h);
        let old = SelectObject(mem_dc, bmp.into());
        let blt = BitBlt(
            mem_dc,
            0,
            0,
            area.w,
            area.h,
            Some(screen_dc),
            area.x,
            area.y,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        if cursor && blt.is_ok() {
            draw_cursor(mem_dc, area);
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: area.w,
                biHeight: -area.h, // negative: rows top to bottom
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = GetDIBits(
            mem_dc,
            bmp,
            0,
            area.h as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        SelectObject(mem_dc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);
        blt?;
        if lines != area.h {
            bail!("GetDIBits read {lines}/{} rows", area.h);
        }
    }
    Ok(buf)
}

/// The mouse pointer, drawn on a copy of `area` at its place on the screen (a hidden pointer is left out).
unsafe fn draw_cursor(dc: HDC, area: Rect) {
    unsafe {
        let mut ci = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut ci).is_err() || ci.flags.0 & CURSOR_SHOWING.0 == 0 || ci.hCursor.is_invalid() {
            return;
        }
        let icon = HICON(ci.hCursor.0);
        let mut ii = ICONINFO::default();
        if GetIconInfo(icon, &mut ii).is_err() {
            return;
        }
        for b in [ii.hbmMask, ii.hbmColor] {
            if !b.is_invalid() {
                let _ = DeleteObject(b.into());
            }
        }
        let (x, y) = (ci.ptScreenPos.x - ii.xHotspot as i32 - area.x, ci.ptScreenPos.y - ii.yHotspot as i32 - area.y);
        let _ = DrawIconEx(dc, x, y, icon, 0, 0, 0, None, DI_NORMAL);
    }
}

pub fn capture(area: Rect) -> Result<RgbaImage> {
    let mut buf = grab(area, false)?;
    // GDI returns BGRA; the alpha channel is meaningless.
    for px in buf.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
        px[3] = 255;
    }
    Ok(RgbaImage::from_raw(area.w as u32, area.h as u32, buf).expect("buffer size is consistent"))
}

pub fn capture_all() -> Result<RgbaImage> {
    capture(virtual_screen())
}

/// The size that fits within `max_side` (0: no limit), keeping the shape; never larger.
pub fn fit(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    if max_side == 0 || w.max(h) <= max_side {
        return (w, h);
    }
    let s = max_side as f64 / w.max(h) as f64;
    (((w as f64 * s).round() as u32).max(1), ((h as f64 * s).round() as u32).max(1))
}

/// Scales down proportionally if the long side exceeds `max_side`; 0 leaves it untouched.
pub fn downscale(img: &RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    let (nw, nh) = fit(w, h, max_side);
    if (nw, nh) == (w, h) {
        return img.clone();
    }
    let (tx, ty) = (Taps::new(w, nw), Taps::new(h, nh));
    let px = scale_part(img.as_raw(), w as usize, &tx, &ty, Rect { x: 0, y: 0, w: nw as i32, h: nh as i32 });
    RgbaImage::from_raw(nw, nh, px).expect("buffer size is consistent")
}

/// How one axis is scaled down: each output pixel is the average of the source pixels it covers (output pixel
/// `i` covers `[i·r, (i+1)·r)`, `r = src / dst`). Fast, and sharp enough for text at the ratios used here.
pub struct Taps {
    /// First source pixel of each output pixel
    first: Vec<u32>,
    /// Where each output pixel's weights start in `w`; one more entry closes the last
    off: Vec<u32>,
    /// Weights in 1/65536; each output pixel's add up to exactly 65536
    w: Vec<u32>,
}

impl Taps {
    pub fn new(src: u32, dst: u32) -> Taps {
        let r = src as f64 / dst as f64;
        let mut t = Taps { first: Vec::with_capacity(dst as usize), off: vec![0], w: Vec::new() };
        for i in 0..dst {
            let (a, b) = (i as f64 * r, ((i + 1) as f64 * r).min(src as f64));
            let j0 = (a.floor() as u32).min(src - 1);
            let j1 = (b.ceil() as u32).clamp(j0 + 1, src);
            let start = t.w.len();
            for j in j0..j1 {
                let overlap = (b.min(j as f64 + 1.0) - a.max(j as f64)).max(0.0);
                t.w.push((overlap / r * 65536.0).round() as u32);
            }
            let ws = &mut t.w[start..];
            let sum: i64 = ws.iter().map(|&v| v as i64).sum();
            let big = (0..ws.len()).max_by_key(|&k| ws[k]).unwrap_or(0);
            ws[big] = (ws[big] as i64 + 65536 - sum) as u32;
            t.first.push(j0);
            t.off.push(t.w.len() as u32);
        }
        t
    }

    pub fn len(&self) -> usize {
        self.first.len()
    }

    fn of(&self, i: usize) -> (usize, &[u32]) {
        (self.first[i] as usize, &self.w[self.off[i] as usize..self.off[i + 1] as usize])
    }

    /// End (exclusive) of the source pixels of output pixel `i`.
    fn end(&self, i: usize) -> usize {
        self.first[i] as usize + (self.off[i + 1] - self.off[i]) as usize
    }

    /// The output pixels that cover any of the source pixels `[a, b)`.
    pub fn covering(&self, a: usize, b: usize) -> (usize, usize) {
        // Both ends only grow with `i`: binary search for the first pixel past each limit
        let first_where = |f: &dyn Fn(usize) -> bool| {
            let (mut lo, mut hi) = (0, self.len());
            while lo < hi {
                let m = (lo + hi) / 2;
                if f(m) { hi = m } else { lo = m + 1 }
            }
            lo
        };
        let lo = first_where(&|i| self.end(i) > a);
        let hi = first_where(&|i| self.first[i] as usize >= b);
        (lo, hi.max(lo))
    }
}

/// The part `out` (in output pixels) of the 4-channel image `src` (`sw` pixels wide) scaled down with `tx`, `ty`.
/// Reads only the source rows that part needs.
pub fn scale_part(src: &[u8], sw: usize, tx: &Taps, ty: &Taps, out: Rect) -> Vec<u8> {
    let (ox, oy, ow, oh) = (out.x as usize, out.y as usize, out.w as usize, out.h as usize);
    let (y0, y1) = (ty.first[oy] as usize, ty.end(oy + oh - 1));
    // Across: each source row needed to `ow` pixels, kept with 8 more bits than a byte
    let mut mid = vec![0u16; (y1 - y0) * ow * 4];
    for y in y0..y1 {
        let row = &src[y * sw * 4..(y + 1) * sw * 4];
        let m = &mut mid[(y - y0) * ow * 4..(y - y0 + 1) * ow * 4];
        for (i, o) in m.chunks_exact_mut(4).enumerate() {
            let (first, w) = tx.of(ox + i);
            let mut acc = [0u32; 4];
            for (k, &wk) in w.iter().enumerate() {
                let p = &row[(first + k) * 4..(first + k) * 4 + 4];
                for c in 0..4 {
                    acc[c] += wk * p[c] as u32;
                }
            }
            for c in 0..4 {
                o[c] = ((acc[c] + 128) >> 8) as u16;
            }
        }
    }
    // Down: the rows of `mid` to `oh` rows (the sums stay below 2^32: 65536 × 65280 + 2^23)
    let mut dst = vec![0u8; ow * oh * 4];
    let mut acc = vec![0u32; ow * 4];
    for (j, d) in dst.chunks_exact_mut(ow * 4).enumerate() {
        let (first, w) = ty.of(oy + j);
        acc.fill(0);
        for (k, &wk) in w.iter().enumerate() {
            let m = &mid[(first + k - y0) * ow * 4..(first + k - y0 + 1) * ow * 4];
            for (a, &v) in acc.iter_mut().zip(m) {
                *a += wk * v as u32;
            }
        }
        for (o, &a) in d.iter_mut().zip(&acc) {
            *o = ((a + (1 << 23)) >> 24) as u8;
        }
    }
    dst
}

pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(img.clone())
        .to_rgb8()
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

pub fn encode_jpeg(img: &RgbaImage, quality: u8) -> Result<Vec<u8>> {
    jpeg(img.as_raw(), img.width(), img.height(), Order::Rgba, quality, true)
}

/// Byte order of 4-channel pixels.
#[derive(Clone, Copy)]
pub enum Order {
    Rgba,
    Bgra,
}

/// JPEG of 4-channel pixels (alpha ignored). `sharp` keeps full color detail (colored text stays crisp);
/// without it color is kept at half resolution, a quarter smaller, for pictures that are about to change again.
pub fn jpeg(px: &[u8], w: u32, h: u32, order: Order, quality: u8, sharp: bool) -> Result<Vec<u8>> {
    use jpeg_encoder::{ColorType, Encoder, SamplingFactor};
    let mut out = Vec::new();
    let mut e = Encoder::new(&mut out, quality);
    e.set_sampling_factor(if sharp { SamplingFactor::F_1_1 } else { SamplingFactor::F_2_2 });
    e.set_optimized_huffman_tables(true);
    let color = match order {
        Order::Rgba => ColorType::Rgba,
        Order::Bgra => ColorType::Bgra,
    };
    e.encode(px, w.try_into()?, h.try_into()?, color)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taps_add_up_and_cover() {
        for (src, dst) in [(2560, 1280), (1920, 1280), (1366, 1280), (1000, 999), (7, 3), (5, 5)] {
            let t = Taps::new(src, dst);
            assert_eq!(t.len(), dst as usize);
            for i in 0..t.len() {
                let (first, w) = t.of(i);
                assert_eq!(w.iter().sum::<u32>(), 65536, "{src}->{dst} pixel {i}");
                assert!(first + w.len() <= src as usize);
            }
            assert_eq!(t.first[0], 0);
            assert_eq!(t.end(t.len() - 1), src as usize);
        }
    }

    #[test]
    fn covering_finds_every_touched_output_pixel() {
        let t = Taps::new(1920, 1280);
        for (a, b) in [(0, 64), (64, 128), (1856, 1920), (100, 101)] {
            let (lo, hi) = t.covering(a, b);
            for i in 0..t.len() {
                let touches = (t.first[i] as usize) < b && t.end(i) > a;
                assert_eq!(touches, i >= lo && i < hi, "{a}..{b} pixel {i}");
            }
        }
    }

    #[test]
    fn scaling_a_flat_color_keeps_it_and_parts_match_the_whole() {
        let (sw, sh) = (300usize, 200usize);
        let mut src = vec![0u8; sw * sh * 4];
        for (n, p) in src.chunks_exact_mut(4).enumerate() {
            p.copy_from_slice(&[(n % 251) as u8, (n % 13 * 19) as u8, 200, 255]);
        }
        let (tx, ty) = (Taps::new(sw as u32, 128), Taps::new(sh as u32, 85));
        let whole = scale_part(&src, sw, &tx, &ty, Rect { x: 0, y: 0, w: 128, h: 85 });
        let part = Rect { x: 40, y: 30, w: 50, h: 20 };
        let piece = scale_part(&src, sw, &tx, &ty, part);
        for y in 0..20 {
            let a = ((30 + y) * 128 + 40) * 4;
            assert_eq!(&piece[y * 50 * 4..(y + 1) * 50 * 4], &whole[a..a + 50 * 4]);
        }
        assert!(whole.chunks_exact(4).all(|p| p[2] == 200 && p[3] == 255));
    }

    #[test]
    fn identity_scaling_is_exact() {
        let src: Vec<u8> = (0..(16 * 9 * 4)).map(|n| (n * 7 % 256) as u8).collect();
        let (tx, ty) = (Taps::new(16, 16), Taps::new(9, 9));
        assert_eq!(scale_part(&src, 16, &tx, &ty, Rect { x: 0, y: 0, w: 16, h: 9 }), src);
    }
}
