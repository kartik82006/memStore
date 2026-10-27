//! Decides which paths are noise (temp files, caches, build output) and should not be recorded.

use std::path::Path;

/// Folder names that never hold files a person cares about finding later.
/// Folders whose name starts with "." (.git, .vscode, .cargo…) are skipped too.
const IGNORED_DIRS: &[&str] = &[
    "node_modules", "target", "__pycache__", "venv", "obj", "appdata", "$recycle.bin",
    "system volume information", "$windows.~bt", "$windows.~ws", "windows", "program files",
    "program files (x86)", "programdata", "msocache", "recovery", "intel", "perflogs",
];

/// File types MemStor records for now: everyday documents, sheets, slides, PDFs, images,
/// audio, video and archives. Anything else (system files, program files, code) is skipped.
pub const TRACKED_TYPES: &[(&str, &[&str])] = &[
    ("Documents", &["doc", "docx", "odt", "rtf", "txt", "md", "pdf", "epub", "pages", "tex"]),
    ("Spreadsheets", &["xls", "xlsx", "xlsm", "ods", "csv", "tsv", "numbers"]),
    ("Presentations", &["ppt", "pptx", "odp", "key"]),
    ("Images", &["jpg", "jpeg", "png", "gif", "bmp", "webp", "heic", "heif", "tif", "tiff", "svg", "psd", "ai"]),
    ("Audio", &["mp3", "wav", "m4a", "aac", "flac", "ogg", "wma"]),
    ("Video", &["mp4", "mov", "avi", "mkv", "webm", "wmv", "m4v"]),
    ("Archives", &["zip", "rar", "7z"]),
];

/// True when the file's extension is one of the tracked types.
pub fn is_tracked_type(path: &Path) -> bool {
    let Some(ext) = path.extension() else { return false };
    let ext = ext.to_string_lossy().to_lowercase();
    TRACKED_TYPES.iter().any(|(_, exts)| exts.contains(&ext.as_str()))
}

/// Exact file names that are always system noise.
const IGNORED_NAMES: &[&str] = &["desktop.ini", "thumbs.db", ".ds_store"];

/// File endings used by temp, partial-download and lock files.
const IGNORED_SUFFIXES: &[&str] = &[
    ".tmp", ".temp", ".crdownload", ".part", ".partial", ".download", ".lock", ".swp", "~",
];

/// True for a folder whose contents are never worth recording.
pub fn is_noise_dir_name(name: &str) -> bool {
    let n = name.to_lowercase();
    n.starts_with('.') || IGNORED_DIRS.contains(&n.as_str())
}

/// Returns true when the path should be skipped.
pub fn is_noise(path: &Path) -> bool {
    let mut comps = path.components().peekable();
    while let Some(comp) = comps.next() {
        // Only folders count here; the file name itself is checked below.
        if comps.peek().is_some() {
            if let std::path::Component::Normal(c) = comp {
                if is_noise_dir_name(&c.to_string_lossy()) {
                    return true;
                }
            }
        }
    }
    let Some(name) = path.file_name() else { return true };
    let name = name.to_string_lossy().to_lowercase();
    if IGNORED_NAMES.contains(&name.as_str()) {
        return true;
    }
    // Office owner files (~$report.docx), LibreOffice locks (.~lock.x#), hidden dotfiles.
    // Registry hives in the profile folder (NTUSER.DAT, ntuser.dat.LOG1…) change constantly.
    if name.starts_with("~$") || name.starts_with(".~lock") || name.starts_with('.') || name.starts_with("ntuser.") {
        return true;
    }
    if IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    !is_tracked_type(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_normal_documents() {
        assert!(!is_noise(Path::new("/home/u/Documents/Thesis/chapter1.docx")));
        assert!(!is_noise(Path::new("C:/Users/k/Downloads/invoice.pdf")));
        assert!(!is_noise(Path::new("D:/Uni/Thesis/target-audience.docx")), "a file named like a noise folder is fine");
        assert!(!is_noise(Path::new("C:/Users/k/OneDrive/Work/plan.xlsx")));
        assert!(!is_noise(Path::new("C:/Users/k/Pictures/IMG_0042.JPG")), "extensions match in any case");
    }

    #[test]
    fn drops_noise() {
        for p in [
            "/home/u/Documents/~$chapter1.docx",
            "/home/u/proj/node_modules/x/index.js",
            "/home/u/proj/.git/HEAD",
            "/home/u/Downloads/movie.mp4.crdownload",
            "/home/u/Pictures/Thumbs.db",
            "C:/Users/k/AppData/Local/x.log",
            "/home/u/Documents/.hidden",
            "C:/Users/k/.vscode/extensions/x/package.json",
            "C:/Users/k/NTUSER.DAT",
            "C:/Users/k/ntuser.dat.LOG1",
            "D:/Program Files/App/app.exe",
            "C:/Users/k/Downloads/setup.exe",
            "C:/Users/k/Documents/notes",
            "C:/Users/k/Documents/app.dll",
            "C:/Users/k/Documents/server.log",
        ] {
            assert!(is_noise(Path::new(p)), "{p} should be noise");
        }
    }
}
