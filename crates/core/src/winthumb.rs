//! The picture Windows shows for a file in Explorer (a video frame, a HEIC photo), via the
//! Shell's thumbnail service. Uses the codecs already on the PC; nothing extra is installed.

use anyhow::{Result, bail};
use std::path::Path;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDIBits,
    GetObjectW, HGDIOBJ,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_RESIZETOFIT};
use windows::core::HSTRING;

pub fn thumbnail(path: &Path, side: u32) -> Result<image::DynamicImage> {
    // SAFETY: plain COM and GDI calls; every handle made here is released before returning.
    unsafe {
        // Fine if COM is already set up on this thread.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path.as_os_str()), None)?;
        let hbmp = factory.GetImage(SIZE { cx: side as i32, cy: side as i32 }, SIIGBF_RESIZETOFIT)?;
        let mut bm = BITMAP::default();
        let got = GetObjectW(HGDIOBJ(hbmp.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut _));
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if got == 0 || w <= 0 || h <= 0 {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            bail!("Windows has no picture for this file");
        }
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let dc = CreateCompatibleDC(None);
        let rows = GetDIBits(dc, hbmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut info, DIB_RGB_COLORS);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        if rows == 0 {
            bail!("could not read the picture Windows made");
        }
        // BGRA to RGB
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for px in buf.chunks_exact(4) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        let img = image::RgbImage::from_raw(w as u32, h as u32, rgb).ok_or_else(|| anyhow::anyhow!("bad picture size"))?;
        Ok(image::DynamicImage::ImageRgb8(img))
    }
}
