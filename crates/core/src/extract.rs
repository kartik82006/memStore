//! Reading what's inside files, on this PC: document text, photo details (EXIF), a shrunk
//! picture to describe, and a check for private details that must never leave the PC.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;

/// What kind of thing a file is, for describing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    Photo,
    Video,
    Document,
}

impl Media {
    pub fn as_str(self) -> &'static str {
        match self {
            Media::Photo => "photo",
            Media::Video => "video",
            Media::Document => "document",
        }
    }

    pub fn parse(s: &str) -> Option<Media> {
        match s {
            "photo" => Some(Media::Photo),
            "video" => Some(Media::Video),
            "document" => Some(Media::Document),
            _ => None,
        }
    }
}

const PHOTO: &[&str] = &["jpg", "jpeg", "png", "webp", "heic", "heif", "bmp", "gif", "tif", "tiff"];
const VIDEO: &[&str] = &["mp4", "mov", "avi", "mkv", "webm", "wmv", "m4v"];
const DOCUMENT: &[&str] = &["pdf", "docx", "pptx", "xlsx", "txt", "md", "csv", "rtf", "odt"];

pub fn ext(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn media_of(path: &Path) -> Option<Media> {
    let e = ext(path);
    if PHOTO.contains(&e.as_str()) {
        Some(Media::Photo)
    } else if VIDEO.contains(&e.as_str()) {
        Some(Media::Video)
    } else if DOCUMENT.contains(&e.as_str()) {
        Some(Media::Document)
    } else {
        None
    }
}

/// Screenshots are skipped: they're many, short-lived, and often show private screens.
pub fn is_screenshot(path: &Path) -> bool {
    let p = path.to_string_lossy().to_lowercase();
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    name.starts_with("screenshot")
        || name.starts_with("screen shot")
        || name.starts_with("snip")
        || name.contains("screenshot_")
        || p.contains("\\screenshots\\")
        || p.contains("/screenshots/")
}

// ---------- private details ----------

/// File-name words that mean "keep this on the PC".
const PRIVATE_NAMES: &[&str] = &[
    "aadhaar", "aadhar", "pan card", "pancard", "passport", "bank statement", "statement", "salary slip",
    "payslip", "itr", "form 16", "form16", "password", "credentials", "voter id", "driving licence",
    "driving license", "id card", "kyc", "cheque", "medical", "prescription", "lab report",
];

/// Text words that mean "keep this on the PC".
const PRIVATE_WORDS: &[&str] = &[
    "password", "passcode", "cvv", "ifsc", "account no", "account number", "a/c no", "aadhaar", "aadhar",
    "permanent account number", "passport no", "date of birth", "uidai",
];

/// Why a file must stay on the PC, if it must.
pub fn private_reason(path: &Path, text: &str) -> Option<String> {
    let name = path.file_stem().map(|n| n.to_string_lossy().to_lowercase().replace(['_', '-', '.'], " ")).unwrap_or_default();
    if let Some(w) = PRIVATE_NAMES.iter().find(|w| name.contains(*w)) {
        return Some(format!("its name mentions \"{w}\""));
    }
    let lower = text.to_lowercase();
    if let Some(w) = PRIVATE_WORDS.iter().find(|w| lower.contains(*w)) {
        return Some(format!("it mentions \"{w}\""));
    }
    if has_aadhaar(text) {
        return Some("it has a number that looks like an Aadhaar number".into());
    }
    if has_pan(text) {
        return Some("it has a code that looks like a PAN".into());
    }
    if has_card_number(text) {
        return Some("it has a number that looks like a bank card".into());
    }
    None
}

/// Runs of digits, allowing single spaces or dashes inside ("1234 5678 9012").
fn digit_runs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut gap = false;
    for c in text.chars() {
        if c.is_ascii_digit() {
            cur.push(c);
            gap = false;
        } else if (c == ' ' || c == '-') && !cur.is_empty() && !gap {
            gap = true;
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            gap = false;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn has_aadhaar(text: &str) -> bool {
    // 12 digits, first digit 2-9, written as one run or 4-4-4.
    digit_runs(text).iter().any(|r| r.len() == 12 && !r.starts_with(['0', '1']))
}

fn has_pan(text: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| {
        let b = w.as_bytes();
        b.len() == 10
            && b[..5].iter().all(u8::is_ascii_uppercase)
            && b[5..9].iter().all(u8::is_ascii_digit)
            && b[9].is_ascii_uppercase()
    })
}

fn luhn(d: &str) -> bool {
    let mut sum = 0;
    for (i, c) in d.chars().rev().enumerate() {
        let mut n = c.to_digit(10).unwrap_or(0);
        if i % 2 == 1 {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
    }
    sum % 10 == 0
}

fn has_card_number(text: &str) -> bool {
    digit_runs(text).iter().any(|r| (13..=19).contains(&r.len()) && luhn(r) && r.chars().any(|c| c != '0'))
}

// ---------- document text ----------

/// About this many words of a document are read (and at most this much is ever sent).
pub const DOC_WORDS: usize = 3000;

/// The text of a document, cut to about `DOC_WORDS` words.
pub fn document_text(path: &Path) -> Result<String> {
    let raw = match ext(path).as_str() {
        "txt" | "md" | "csv" => {
            let mut s = String::new();
            std::fs::File::open(path)?.take(400_000).read_to_string(&mut s).or_else(|_| {
                // Not UTF-8: keep what we can.
                let bytes = std::fs::read(path)?;
                s = String::from_utf8_lossy(&bytes[..bytes.len().min(400_000)]).into_owned();
                Ok::<usize, std::io::Error>(s.len())
            })?;
            s
        }
        "rtf" => strip_rtf(&std::fs::read_to_string(path).unwrap_or_default()),
        "docx" => office_text(path, |n| n == "word/document.xml")?,
        "pptx" => office_text(path, |n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))?,
        "xlsx" => office_text(path, |n| n == "xl/sharedStrings.xml")?,
        "odt" => office_text(path, |n| n == "content.xml")?,
        "pdf" => {
            let p = path.to_path_buf();
            // The PDF reader can panic on unusual files; treat that as "no text".
            std::panic::catch_unwind(move || pdf_extract::extract_text(&p))
                .map_err(|_| anyhow::anyhow!("could not read this PDF"))?
                .context("could not read this PDF")?
        }
        other => bail!("can't read .{other} files"),
    };
    Ok(first_words(&raw, DOC_WORDS))
}

pub fn first_words(text: &str, n: usize) -> String {
    text.split_whitespace().take(n).collect::<Vec<_>>().join(" ")
}

/// Text inside the matching XML parts of an Office (zip) file, in part order.
fn office_text(path: &Path, want: impl Fn(&str) -> bool) -> Result<String> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path)?).context("not a valid Office file")?;
    let mut names: Vec<String> = zip.file_names().filter(|n| want(n)).map(str::to_string).collect();
    // slide2 before slide10
    names.sort_by_key(|n| (n.len(), n.clone()));
    let mut out = String::new();
    for n in names {
        let mut xml = String::new();
        zip.by_name(&n)?.take(4_000_000).read_to_string(&mut xml)?;
        out.push_str(&xml_text(&xml));
        out.push('\n');
        if out.split_whitespace().count() > DOC_WORDS {
            break;
        }
    }
    Ok(out)
}

/// The text between XML tags, with paragraph and cell ends as spaces.
pub fn xml_text(xml: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    for c in xml.chars() {
        match c {
            '<' => {
                in_tag = true;
                tag.clear();
            }
            '>' if in_tag => {
                in_tag = false;
                if tag.starts_with("/w:p") || tag.starts_with("/a:p") || tag.starts_with("/si") || tag.starts_with("/text:p") || tag.starts_with("w:tab") || tag.starts_with("w:br") {
                    out.push(' ');
                }
            }
            _ if in_tag => tag.push(c),
            _ => out.push(c),
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'")
}

fn strip_rtf(rtf: &str) -> String {
    let mut out = String::new();
    let mut chars = rtf.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // skip control word
                while let Some(&n) = chars.peek() {
                    if n.is_ascii_alphanumeric() || n == '-' {
                        chars.next();
                    } else {
                        break;
                    }
                }
                if chars.peek() == Some(&' ') {
                    chars.next();
                }
            }
            '{' | '}' => {}
            _ => out.push(c),
        }
    }
    out
}

// ---------- photo details ----------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhotoFacts {
    /// "2024:11:01 18:42:10" as written by the camera.
    pub taken: Option<String>,
    pub camera: Option<String>,
    /// Latitude, longitude. Kept on the PC; never sent.
    pub gps: Option<(f64, f64)>,
}

pub fn photo_facts(path: &Path) -> PhotoFacts {
    let Ok(file) = std::fs::File::open(path) else { return PhotoFacts::default() };
    let mut reader = std::io::BufReader::new(file);
    let Ok(ex) = exif::Reader::new().read_from_container(&mut reader) else { return PhotoFacts::default() };
    let text = |tag| ex.get_field(tag, exif::In::PRIMARY).map(|f| f.display_value().to_string().trim_matches('"').trim().to_string());
    let camera = match (text(exif::Tag::Make), text(exif::Tag::Model)) {
        (Some(make), Some(model)) if model.to_lowercase().starts_with(&make.to_lowercase()) => Some(model),
        (Some(make), Some(model)) => Some(format!("{make} {model}")),
        (a, b) => a.or(b),
    };
    let coord = |tag, reftag| -> Option<f64> {
        let f = ex.get_field(tag, exif::In::PRIMARY)?;
        let exif::Value::Rational(ref v) = f.value else { return None };
        if v.len() < 3 {
            return None;
        }
        let deg = v[0].to_f64() + v[1].to_f64() / 60.0 + v[2].to_f64() / 3600.0;
        let r = text(reftag).unwrap_or_default();
        Some(if r.starts_with('S') || r.starts_with('W') { -deg } else { deg })
    };
    let gps = match (coord(exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef), coord(exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef)) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    };
    PhotoFacts {
        taken: text(exif::Tag::DateTimeOriginal).or_else(|| text(exif::Tag::DateTime)),
        camera,
        gps,
    }
}

// ---------- pictures to describe ----------

/// Longest side of the picture sent to be described.
pub const PICTURE_SIDE: u32 = 768;

/// A JPEG of the file, shrunk to fit `PICTURE_SIDE`, or None when it is too small to be worth
/// describing (icons, thumbnails). Photos are read directly; on Windows, videos and formats we
/// can't decode (HEIC) use the picture Windows itself shows in Explorer.
pub fn picture(path: &Path, media: Media) -> Result<Option<Vec<u8>>> {
    let img = match media {
        Media::Photo => match image::ImageReader::open(path)?.with_guessed_format()?.decode() {
            Ok(img) => img,
            Err(e) => shell_picture(path).with_context(|| format!("can't open this picture ({e})"))?,
        },
        Media::Video => shell_picture(path)?,
        Media::Document => bail!("documents are described from their text"),
    };
    if img.width().max(img.height()) < 200 {
        return Ok(None);
    }
    let img = if img.width().max(img.height()) > PICTURE_SIDE { img.thumbnail(PICTURE_SIDE, PICTURE_SIDE) } else { img };
    let mut out = Vec::new();
    let rgb = img.to_rgb8();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&rgb)?;
    Ok(Some(out))
}

/// What a video picture shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoInfo {
    /// How many moments are in the picture (1 = a single frame).
    pub frames: usize,
    pub seconds: Option<u64>,
}

/// Moments taken from a video for describing it.
#[cfg_attr(not(windows), allow(dead_code))]
const VIDEO_MOMENTS: usize = 4;

/// A JPEG grid of four moments from a video (or one frame when that's all Windows can give),
/// and what it shows.
pub fn video_picture(path: &Path) -> Result<Option<(Vec<u8>, Option<VideoInfo>)>> {
    let (frames, seconds) = video_frames(path).unwrap_or_default();
    if frames.len() >= 2 {
        let g = grid(&frames, PICTURE_SIDE);
        if g.width().max(g.height()) < 200 {
            return Ok(None);
        }
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 80).encode_image(&g)?;
        return Ok(Some((out, Some(VideoInfo { frames: frames.len(), seconds }))));
    }
    Ok(picture(path, Media::Video)?.map(|j| (j, Some(VideoInfo { frames: 1, seconds }))))
}

#[cfg(windows)]
fn video_frames(path: &Path) -> Result<(Vec<image::RgbImage>, Option<u64>)> {
    crate::winvideo::frames(path, VIDEO_MOMENTS, PICTURE_SIDE / 2)
}

#[cfg(not(windows))]
fn video_frames(_path: &Path) -> Result<(Vec<image::RgbImage>, Option<u64>)> {
    bail!("video frames are only read on Windows")
}

/// Frames side by side, two per row, on a dark background, fitting `side`.
pub(crate) fn grid(frames: &[image::RgbImage], side: u32) -> image::RgbImage {
    const GAP: u32 = 4;
    let cols = if frames.len() >= 2 { 2 } else { 1 };
    let rows = frames.len().div_ceil(cols) as u32;
    let cell_w = (side - GAP * (cols as u32 - 1)) / cols as u32;
    let cell_h = frames.iter().map(|f| f.height() * cell_w / f.width().max(1)).max().unwrap_or(cell_w).max(1);
    let mut out = image::RgbImage::from_pixel(cell_w * cols as u32 + GAP * (cols as u32 - 1), cell_h * rows + GAP * (rows - 1), image::Rgb([24, 24, 24]));
    for (i, f) in frames.iter().enumerate() {
        let (col, row) = ((i % cols) as u32, (i / cols) as u32);
        let h = (f.height() * cell_w / f.width().max(1)).max(1);
        let small = image::imageops::resize(f, cell_w, h, image::imageops::FilterType::Triangle);
        image::imageops::overlay(&mut out, &small, (col * (cell_w + GAP)) as i64, (row * (cell_h + GAP) + (cell_h - h) / 2) as i64);
    }
    out
}

#[cfg(windows)]
fn shell_picture(path: &Path) -> Result<image::DynamicImage> {
    crate::winthumb::thumbnail(path, PICTURE_SIDE)
}

#[cfg(not(windows))]
fn shell_picture(_path: &Path) -> Result<image::DynamicImage> {
    bail!("video pictures are only available on Windows")
}

/// A fingerprint for spotting copies of the same file: size plus a hash of its first 64 KB.
pub fn fingerprint(path: &Path) -> Result<String> {
    let size = std::fs::metadata(path)?.len();
    let mut buf = vec![0u8; 65_536];
    let n = std::fs::File::open(path)?.read(&mut buf)?;
    // FNV-1a: fast, and good enough to group exact copies.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in &buf[..n] {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    Ok(format!("{size}-{h:016x}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_frames_make_a_two_by_two_grid() {
        let colours = [[200, 0, 0], [0, 200, 0], [0, 0, 200], [200, 200, 0]];
        let frames: Vec<_> = colours.iter().map(|c| image::RgbImage::from_pixel(384, 216, image::Rgb(*c))).collect();
        let g = grid(&frames, 768);
        assert_eq!(g.dimensions(), (382 * 2 + 4, 214 * 2 + 4));
        assert_eq!(*g.get_pixel(10, 10), image::Rgb([200, 0, 0]));
        assert_eq!(*g.get_pixel(700, 10), image::Rgb([0, 200, 0]));
        assert_eq!(*g.get_pixel(10, 300), image::Rgb([0, 0, 200]));
        assert_eq!(*g.get_pixel(700, 300), image::Rgb([200, 200, 0]));
    }

    #[test]
    fn private_details_are_spotted() {
        let p = Path::new("/x/notes.txt");
        assert!(private_reason(p, "my aadhaar is 2345 6789 0123").is_some());
        assert!(private_reason(p, "Number: 2345 6789 0123").is_some());
        assert!(private_reason(p, "PAN ABCDE1234F").is_some());
        assert!(private_reason(p, "card 4111 1111 1111 1111 exp").is_some());
        assert!(private_reason(Path::new("/x/Aadhaar_front.jpg"), "").is_some());
        assert!(private_reason(Path::new("/x/Bank-Statement March.pdf"), "").is_some());
        assert!(private_reason(p, "Trip plan for 12 people, budget 45000, 2024 dates").is_none());
        assert!(private_reason(p, "Invoice INV-2024-000123").is_none());
    }

    #[test]
    fn screenshots_are_spotted() {
        assert!(is_screenshot(Path::new("C:/Users/k/Pictures/Screenshots/a.png")));
        assert!(is_screenshot(Path::new("/x/Screenshot 2026-10-01 101010.png")));
        assert!(!is_screenshot(Path::new("/x/IMG_2041.jpg")));
    }

    #[test]
    fn office_and_text_files_are_read() {
        let dir = crate::test_dir("extract-docx");
        let docx = dir.join("plan.docx");
        {
            let f = std::fs::File::create(&docx).unwrap();
            let mut z = zip::ZipWriter::new(f);
            z.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(
                &mut z,
                br#"<w:document><w:body><w:p><w:r><w:t>Alumni meet</w:t></w:r></w:p><w:p><w:r><w:t>Budget &amp; venue</w:t></w:r></w:p></w:body></w:document>"#,
            )
            .unwrap();
            z.finish().unwrap();
        }
        assert_eq!(document_text(&docx).unwrap(), "Alumni meet Budget & venue");
        let txt = dir.join("a.txt");
        std::fs::write(&txt, "one two  three\nfour").unwrap();
        assert_eq!(document_text(&txt).unwrap(), "one two three four");
    }

    #[test]
    fn photos_are_shrunk_and_icons_skipped() {
        let dir = crate::test_dir("extract-photo");
        let big = dir.join("big.png");
        image::RgbImage::from_pixel(1600, 900, image::Rgb([200, 30, 120])).save(&big).unwrap();
        let jpg = picture(&big, Media::Photo).unwrap().unwrap();
        let back = image::load_from_memory(&jpg).unwrap();
        assert_eq!((back.width(), back.height()), (768, 432));
        let icon = dir.join("icon.png");
        image::RgbImage::new(64, 64).save(&icon).unwrap();
        assert!(picture(&icon, Media::Photo).unwrap().is_none());
    }
}
