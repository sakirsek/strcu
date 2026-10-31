//! The computer's clipboard: text both ways, and a screenshot onto it.

use std::time::Duration;

use anyhow::{Result, bail};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::w;

use crate::i18n::Msg;

/// Text as UTF-16 (CF_UNICODETEXT).
const UNICODE_TEXT: u32 = 13;
/// A device-independent bitmap (CF_DIB).
const BITMAP: u32 = 8;

/// Open while it lives; another program holding the clipboard for a moment is waited for.
struct Open;

impl Open {
    fn new() -> Result<Open> {
        for _ in 0..20 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                return Ok(Open);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        bail!(Msg::new("err.clip_busy"))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = unsafe { CloseClipboard() };
    }
}

/// The text on the clipboard; None when it holds no text (a picture, files, nothing).
pub fn get() -> Result<Option<String>> {
    let _open = Open::new()?;
    unsafe {
        if IsClipboardFormatAvailable(UNICODE_TEXT).is_err() {
            return Ok(None);
        }
        let mem = HGLOBAL(GetClipboardData(UNICODE_TEXT)?.0);
        let p = GlobalLock(mem) as *const u16;
        if p.is_null() {
            return Ok(None);
        }
        let max = GlobalSize(mem) / 2;
        let wide = std::slice::from_raw_parts(p, max);
        let len = wide.iter().position(|&c| c == 0).unwrap_or(max);
        let text = String::from_utf16_lossy(&wide[..len]);
        let _ = GlobalUnlock(mem);
        Ok(Some(text))
    }
}

/// Puts `text` on the clipboard in place of what was there.
pub fn set(text: &str) -> Result<()> {
    let bytes: Vec<u8> = text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect();
    let _open = Open::new()?;
    unsafe { EmptyClipboard()? };
    put(UNICODE_TEXT, &bytes)
}

/// Puts a picture (a PNG) on the clipboard in place of what was there: as a bitmap, which every program takes,
/// and as the PNG itself, which Office, browsers and the like prefer.
pub fn set_image(png: &[u8]) -> Result<()> {
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.to_rgb8();
    let dib = dib(&img);
    let png_format = unsafe { RegisterClipboardFormatW(w!("PNG")) };
    let _open = Open::new()?;
    unsafe { EmptyClipboard()? };
    put(BITMAP, &dib)?;
    if png_format != 0 {
        put(png_format, png)?;
    }
    Ok(())
}

/// Copies `bytes` into memory the clipboard takes over as `format`. The clipboard must be open.
fn put(format: u32, bytes: &[u8]) -> Result<()> {
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let p = GlobalLock(mem) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(mem));
            bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(mem);
        // Once set, the clipboard owns the memory
        if let Err(e) = SetClipboardData(format, Some(HANDLE(mem.0))) {
            let _ = GlobalFree(Some(mem));
            return Err(e.into());
        }
    }
    Ok(())
}

/// A picture as a device-independent bitmap (CF_DIB): the header, then 24-bit rows from the bottom up, each
/// padded to a multiple of 4 bytes.
fn dib(img: &image::RgbImage) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let stride = (w as usize * 3).next_multiple_of(4);
    let size = stride * h as usize;
    let mut out = Vec::with_capacity(40 + size);
    out.extend(40u32.to_le_bytes()); // biSize
    out.extend((w as i32).to_le_bytes());
    out.extend((h as i32).to_le_bytes()); // positive: bottom-up
    out.extend(1u16.to_le_bytes()); // planes
    out.extend(24u16.to_le_bytes()); // bits per pixel
    out.extend(0u32.to_le_bytes()); // BI_RGB
    out.extend((size as u32).to_le_bytes());
    out.extend([0u8; 16]); // resolution and colour table: none
    for row in img.rows().rev() {
        let start = out.len();
        for px in row {
            out.extend([px[2], px[1], px[0]]);
        }
        out.resize(start + stride, 0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_as_a_bitmap() {
        // 2 x 2: red, green / blue, white. Rows of 6 bytes, padded to 8; the bottom row first, in BGR
        let img = image::RgbImage::from_raw(2, 2, vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]).unwrap();
        let d = dib(&img);
        assert_eq!(d.len(), 40 + 2 * 8);
        assert_eq!(&d[..16], &[40, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0, 1, 0, 24, 0]);
        assert_eq!(u32::from_le_bytes(d[20..24].try_into().unwrap()), 16);
        assert_eq!(&d[40..48], &[255, 0, 0, 255, 255, 255, 0, 0]);
        assert_eq!(&d[48..56], &[0, 0, 255, 0, 255, 0, 0, 0]);
    }

    /// Run by hand (`cargo test -- --ignored`): it puts a picture on the real clipboard, then the text that was
    /// there back. With anything but text on it, nothing is touched.
    #[test]
    #[ignore]
    fn windows_takes_the_picture() {
        use windows::Win32::Graphics::Gdi::{BITMAP as BITMAP_INFO, GetObjectW, HBITMAP};
        let Some(before) = get().unwrap() else { return };
        let img = image::RgbImage::from_fn(3, 2, |x, y| image::Rgb([x as u8 * 80, y as u8 * 120, 200]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        set_image(&png).unwrap();
        // Windows makes a bitmap of its own out of ours when one is asked for: it read what we wrote
        let size = {
            let _open = Open::new().unwrap();
            let png_there = unsafe { IsClipboardFormatAvailable(RegisterClipboardFormatW(w!("PNG"))) }.is_ok();
            let bmp = HBITMAP(unsafe { GetClipboardData(2) }.unwrap().0);
            let mut info = BITMAP_INFO::default();
            let n = unsafe { GetObjectW(bmp.into(), size_of::<BITMAP_INFO>() as i32, Some(&mut info as *mut _ as *mut _)) };
            (png_there, n > 0, info.bmWidth, info.bmHeight)
        };
        set(&before).unwrap();
        assert_eq!(size, (true, true, 3, 2));
        assert_eq!(get().unwrap(), Some(before));
    }
}
