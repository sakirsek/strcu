//! App icons (PNG): for open windows and for apps in the Start menu.
//!
//! The Windows shell provides the icon (IShellItemImageFactory): the exe's icon for a classic program, the
//! package's icon for a Store app (`shell:AppsFolder\<id>`). Shell objects need a single-threaded COM
//! apartment, so every call runs on its own thread.

use std::ffi::c_void;

use anyhow::{Result, anyhow, bail};
use image::{ImageFormat, RgbaImage};
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP,
    ReleaseDC,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize, IBindCtx};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY};
use windows::core::HSTRING;

/// Icon of a shell path (file path or `shell:AppsFolder\<id>`) as a roughly `size` pixel square PNG.
pub fn png(path: &str, size: u32) -> Result<Vec<u8>> {
    let path = path.to_string();
    std::thread::spawn(move || {
        let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        let r = render(&path, size);
        if com {
            unsafe { CoUninitialize() };
        }
        r
    })
    .join()
    .map_err(|_| anyhow!("could not get the icon"))?
}

fn render(path: &str, size: u32) -> Result<Vec<u8>> {
    let bmp = unsafe {
        let f: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None::<&IBindCtx>)?;
        f.GetImage(SIZE { cx: size as i32, cy: size as i32 }, SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK)?
    };
    let img = to_rgba(bmp);
    unsafe {
        let _ = DeleteObject(bmp.into());
    }
    let mut out = Vec::new();
    img?.write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)?;
    Ok(out)
}

fn to_rgba(bmp: HBITMAP) -> Result<RgbaImage> {
    let mut buf;
    let (w, h);
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(bmp.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
            bail!("could not read the icon");
        }
        (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 || w > 1024 || h > 1024 {
            bail!("unexpected icon size {w}x{h}");
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // negative: rows top to bottom
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        buf = vec![0u8; w as usize * h as usize * 4];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, bmp, 0, h as u32, Some(buf.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        if lines != h {
            bail!("icon: read {lines}/{h} rows");
        }
    }
    fix_alpha(&mut buf);
    Ok(RgbaImage::from_raw(w as u32, h as u32, buf).expect("buffer size is consistent"))
}

/// GDI returns BGRA. An icon without transparency has alpha 0 everywhere (made fully opaque); the
/// premultiplied alpha the shell returns is converted to the straight alpha PNG expects.
fn fix_alpha(buf: &mut [u8]) {
    let opaque = buf.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
    let premul = !opaque && buf.as_chunks::<4>().0.iter().all(|p| p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3]);
    for p in buf.as_chunks_mut::<4>().0 {
        p.swap(0, 2);
        if opaque {
            p[3] = 255;
        } else if premul && p[3] > 0 && p[3] < 255 {
            let a = p[3] as u32;
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fix_alpha;

    #[test]
    fn alpha_is_fixed() {
        // Icon without alpha: becomes fully opaque, BGRA -> RGBA
        let mut a = vec![10, 20, 30, 0, 1, 2, 3, 0];
        fix_alpha(&mut a);
        assert_eq!(a, vec![30, 20, 10, 255, 3, 2, 1, 255]);
        // Premultiplied half-transparent white -> straight alpha
        let mut b = vec![128, 128, 128, 128, 0, 0, 0, 0];
        fix_alpha(&mut b);
        assert_eq!(&b[..4], &[255, 255, 255, 128]);
        // Straight alpha (colour > alpha) stays as it is
        let mut c = vec![200, 200, 200, 100];
        fix_alpha(&mut c);
        assert_eq!(c, vec![200, 200, 200, 100]);
    }
}
