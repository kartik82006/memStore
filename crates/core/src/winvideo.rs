//! Frames from a video at several moments, with Windows Media Foundation (the codecs already
//! on the PC). Media Foundation is loaded at run time: Windows "N" editions don't have it, and
//! MemStor must still start there (videos then fall back to Explorer's thumbnail).

use anyhow::{Context, Result, bail};
use image::RgbImage;
use std::ffi::c_void;
use std::path::Path;
use std::sync::OnceLock;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer, IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_PD_DURATION, MF_SOURCE_READER_ALL_STREAMS, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READER_MEDIASOURCE, MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFMediaType_Video, MFSTARTUP_LITE, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Variant::VT_I8;
use windows::core::{GUID, HRESULT, HSTRING, Interface, PCSTR, PCWSTR};

type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type CreateAttributesFn = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type CreateMediaTypeFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type CreateReaderFn = unsafe extern "system" fn(PCWSTR, *mut c_void, *mut *mut c_void) -> HRESULT;

/// The Media Foundation functions MemStor uses, found at run time.
pub(crate) struct Mf {
    pub(crate) create_attributes: CreateAttributesFn,
    pub(crate) create_media_type: CreateMediaTypeFn,
    create_reader: CreateReaderFn,
    // Kept for the test, which encodes a clip with more Media Foundation functions.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) mfplat: windows::Win32::Foundation::HMODULE,
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) mfreadwrite: windows::Win32::Foundation::HMODULE,
}

// SAFETY: plain function pointers and module handles that stay loaded for the whole run.
unsafe impl Send for Mf {}
unsafe impl Sync for Mf {}

pub(crate) unsafe fn proc<T>(module: windows::Win32::Foundation::HMODULE, name: &[u8]) -> Result<T> {
    // SAFETY: the caller names a function whose signature matches T.
    let f = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) }.with_context(|| format!("{} is missing", String::from_utf8_lossy(name)))?;
    Ok(unsafe { std::mem::transmute_copy(&f) })
}

pub(crate) fn mf() -> Result<&'static Mf> {
    static MF: OnceLock<Option<Mf>> = OnceLock::new();
    MF.get_or_init(|| {
        // SAFETY: loading system DLLs and looking up documented exports with matching signatures.
        unsafe {
            let mfplat = LoadLibraryW(&HSTRING::from("mfplat.dll")).ok()?;
            let mfreadwrite = LoadLibraryW(&HSTRING::from("mfreadwrite.dll")).ok()?;
            let startup: StartupFn = proc(mfplat, b"MFStartup\0").ok()?;
            if startup(MF_VERSION, MFSTARTUP_LITE).is_err() {
                return None;
            }
            Some(Mf {
                create_attributes: proc(mfplat, b"MFCreateAttributes\0").ok()?,
                create_media_type: proc(mfplat, b"MFCreateMediaType\0").ok()?,
                create_reader: proc(mfreadwrite, b"MFCreateSourceReaderFromURL\0").ok()?,
                mfplat,
                mfreadwrite,
            })
        }
    })
    .as_ref()
    .context("this PC has no Windows Media Foundation")
}

/// Wrap a COM pointer returned through an out-parameter.
pub(crate) unsafe fn take<T: Interface>(hr: HRESULT, raw: *mut c_void) -> Result<T> {
    hr.ok()?;
    if raw.is_null() {
        bail!("Media Foundation returned nothing");
    }
    // SAFETY: the call succeeded and handed us one reference to a T.
    Ok(unsafe { T::from_raw(raw) })
}

/// Up to `count` frames spread through the video (at 10%, 35%, 60%, 85% for four), each
/// shrunk to fit `side`, and the video's length in seconds.
pub fn frames(path: &Path, count: usize, side: u32) -> Result<(Vec<RgbImage>, Option<u64>)> {
    let mf = mf()?;
    // SAFETY: COM calls on interfaces we own; buffers are unlocked before they are released.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let mut raw = std::ptr::null_mut();
        let attrs: IMFAttributes = take((mf.create_attributes)(&mut raw, 1), raw)?;
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        let url = HSTRING::from(path.as_os_str());
        let mut raw = std::ptr::null_mut();
        let reader: IMFSourceReader = take((mf.create_reader)(PCWSTR(url.as_ptr()), attrs.as_raw(), &mut raw), raw)?;
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(video, true)?;
        let mut raw = std::ptr::null_mut();
        let want: IMFMediaType = take((mf.create_media_type)(&mut raw), raw)?;
        want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(video, None, &want)?;
        let got = reader.GetCurrentMediaType(video)?;
        let size = got.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (w, h) = ((size >> 32) as u32, (size & 0xffff_ffff) as u32);
        if w == 0 || h == 0 {
            bail!("the video has no picture size");
        }
        // Length in 100-nanosecond units.
        let duration = reader
            .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
            .ok()
            .map(|v| v.Anonymous.Anonymous.Anonymous.uhVal)
            .filter(|d| *d > 0);
        let spots: Vec<f64> = match (count, duration) {
            (_, None) | (0 | 1, _) => vec![0.0],
            (n, Some(_)) => (0..n).map(|i| 0.1 + 0.75 * i as f64 / (n - 1) as f64).collect(),
        };
        let mut out = Vec::new();
        for spot in spots {
            if let Some(d) = duration {
                let mut pos = PROPVARIANT::default();
                (*pos.Anonymous.Anonymous).vt = VT_I8;
                (*pos.Anonymous.Anonymous).Anonymous.hVal = (d as f64 * spot) as i64;
                if reader.SetCurrentPosition(&GUID::zeroed(), &pos).is_err() {
                    continue;
                }
            }
            if let Some(img) = read_frame(&reader, video, w, h)? {
                out.push(fit(&img, side));
            }
        }
        if out.is_empty() {
            bail!("no frames could be read from the video");
        }
        Ok((out, duration.map(|d| d / 10_000_000)))
    }
}

fn fit(img: &RgbImage, side: u32) -> RgbImage {
    let (w, h) = img.dimensions();
    if w.max(h) <= side {
        return img.clone();
    }
    let (nw, nh) = if w >= h { (side, (h * side / w).max(1)) } else { ((w * side / h).max(1), side) };
    image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle)
}

/// The next decoded frame after a seek, as RGB.
unsafe fn read_frame(reader: &IMFSourceReader, stream: u32, w: u32, h: u32) -> Result<Option<RgbImage>> {
    // A few tries: right after a seek the decoder may hand back an empty read.
    for _ in 0..30 {
        let mut flags = 0u32;
        let mut sample: Option<IMFSample> = None;
        // SAFETY: out-parameters point at locals.
        unsafe { reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))? };
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            return Ok(None);
        }
        let Some(sample) = sample else { continue };
        // SAFETY: the buffer is locked while its bytes are copied, then unlocked.
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut img = RgbImage::new(w, h);
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                // Lock2D gives the top row and the pitch, negative for bottom-up pictures.
                let mut row0 = std::ptr::null_mut();
                let mut pitch = 0i32;
                b2.Lock2D(&mut row0, &mut pitch)?;
                copy_bgra(row0, pitch as isize, &mut img);
                b2.Unlock2D()?;
            } else {
                let mut data = std::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut data, None, Some(&mut len))?;
                if len as usize >= (w * h * 4) as usize {
                    copy_bgra(data, (w * 4) as isize, &mut img);
                }
                buffer.Unlock()?;
            }
            return Ok(Some(img));
        }
    }
    Ok(None)
}

/// Copy 32-bit BGRX rows into an RGB image.
unsafe fn copy_bgra(row0: *const u8, pitch: isize, img: &mut RgbImage) {
    let (w, h) = img.dimensions();
    for y in 0..h {
        // SAFETY: the caller locked a buffer of h rows of at least w*4 bytes, `pitch` apart.
        let row = unsafe { std::slice::from_raw_parts(row0.offset(pitch * y as isize), (w * 4) as usize) };
        for x in 0..w {
            let p = &row[(x * 4) as usize..(x * 4 + 3) as usize];
            img.put_pixel(x, y, image::Rgb([p[2], p[1], p[0]]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Media::MediaFoundation::{
        IMFMediaBuffer, IMFSinkWriter, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_INTERLACE_MODE,
        MF_MT_PIXEL_ASPECT_RATIO, MFVideoFormat_H264, MFVideoInterlace_Progressive,
    };

    type CreateWriterFn = unsafe extern "system" fn(PCWSTR, *mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;
    type CreateBufferFn = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
    type CreateSampleFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;

    /// Encode a 4-second test video: red over blue for the first half, then all green.
    unsafe fn make_video(mf: &Mf, path: &Path) -> Result<()> {
        let (w, h, fps) = (320u32, 240u32, 10u32);
        unsafe {
            let create_writer: CreateWriterFn = proc(mf.mfreadwrite, b"MFCreateSinkWriterFromURL\0")?;
            let create_buffer: CreateBufferFn = proc(mf.mfplat, b"MFCreateMemoryBuffer\0")?;
            let create_sample: CreateSampleFn = proc(mf.mfplat, b"MFCreateSample\0")?;
            let url = HSTRING::from(path.as_os_str());
            let mut raw = std::ptr::null_mut();
            let writer: IMFSinkWriter =
                take(create_writer(PCWSTR(url.as_ptr()), std::ptr::null_mut(), std::ptr::null_mut(), &mut raw), raw)?;
            let size = ((w as u64) << 32) | h as u64;
            let rate = ((fps as u64) << 32) | 1;
            let mut raw = std::ptr::null_mut();
            let out: IMFMediaType = take((mf.create_media_type)(&mut raw), raw)?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, 800_000)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            let stream = writer.AddStream(&out)?;
            let mut raw = std::ptr::null_mut();
            let input: IMFMediaType = take((mf.create_media_type)(&mut raw), raw)?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            // Positive stride: the first row in memory is the top of the picture.
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
            writer.SetInputMediaType(stream, &input, None)?;
            writer.BeginWriting()?;
            let frames = fps * 4;
            let frame_len = 10_000_000 / fps as i64;
            for i in 0..frames {
                let mut raw = std::ptr::null_mut();
                let buffer: IMFMediaBuffer = take(create_buffer(w * h * 4, &mut raw), raw)?;
                let mut data = std::ptr::null_mut();
                buffer.Lock(&mut data, None, None)?;
                let bytes = std::slice::from_raw_parts_mut(data, (w * h * 4) as usize);
                for y in 0..h {
                    let bgr = if i >= frames / 2 { [0u8, 200, 0] } else if y < h / 2 { [0, 0, 220] } else { [220, 0, 0] };
                    for x in 0..w {
                        let o = ((y * w + x) * 4) as usize;
                        bytes[o..o + 4].copy_from_slice(&[bgr[0], bgr[1], bgr[2], 255]);
                    }
                }
                buffer.Unlock()?;
                buffer.SetCurrentLength(w * h * 4)?;
                let mut raw = std::ptr::null_mut();
                let sample: IMFSample = take(create_sample(&mut raw), raw)?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(i as i64 * frame_len)?;
                sample.SetSampleDuration(frame_len)?;
                writer.WriteSample(stream, &sample)?;
            }
            writer.Finalize()?;
        }
        Ok(())
    }

    #[test]
    fn frames_come_from_several_moments_the_right_way_up() {
        let Ok(mf) = mf() else {
            // Windows N editions and some servers ship without Media Foundation.
            eprintln!("no Media Foundation on this PC; videos use Explorer's thumbnail here");
            return;
        };
        let dir = crate::test_dir("winvideo");
        let path = dir.join("test clip.mp4");
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if let Err(e) = unsafe { make_video(mf, &path) } {
            eprintln!("this PC can't encode H.264, so there is no test video: {e:#}");
            return;
        }
        let (frames, secs) = frames(&path, 4, 160).unwrap();
        assert_eq!(frames.len(), 4);
        assert_eq!(secs, Some(4));
        assert_eq!(frames[0].dimensions(), (160, 120));
        let px = |img: &RgbImage, y: u32| *img.get_pixel(80, y);
        let (top, bottom) = (px(&frames[0], 20), px(&frames[0], 100));
        assert!(top[0] > 150 && top[2] < 80, "first frame: red on top, got {top:?}");
        assert!(bottom[2] > 150 && bottom[0] < 80, "first frame: blue at the bottom, got {bottom:?}");
        let late = px(&frames[3], 60);
        assert!(late[1] > 120 && late[0] < 80, "last frame: green, got {late:?}");
    }
}
