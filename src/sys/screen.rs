//! Screenshots (GDI BitBlt). Verified to work while the display is off, too.

use anyhow::{Result, bail};
use image::{DynamicImage, RgbaImage, imageops::FilterType};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ROP_CODE, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
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

pub fn capture(area: Rect) -> Result<RgbaImage> {
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
    // GDI returns BGRA; the alpha channel is meaningless.
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    Ok(RgbaImage::from_raw(area.w as u32, area.h as u32, buf).expect("buffer size is consistent"))
}

pub fn capture_all() -> Result<RgbaImage> {
    capture(virtual_screen())
}

/// Scales down proportionally if the long side exceeds `max_side`; 0 leaves it untouched.
pub fn downscale(img: &RgbaImage, max_side: u32) -> RgbaImage {
    let (w, h) = img.dimensions();
    if max_side == 0 || w.max(h) <= max_side {
        return img.clone();
    }
    let s = max_side as f64 / w.max(h) as f64;
    let (nw, nh) = ((w as f64 * s).round() as u32, (h as f64 * s).round() as u32);
    image::imageops::resize(img, nw, nh, FilterType::Triangle)
}

pub fn encode_png(img: &RgbaImage) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    DynamicImage::ImageRgba8(img.clone())
        .to_rgb8()
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)?;
    Ok(out)
}

pub fn encode_jpeg(img: &RgbaImage, quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let rgb = DynamicImage::ImageRgba8(img.clone()).to_rgb8();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality).encode_image(&rgb)?;
    Ok(out)
}
