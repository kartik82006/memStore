//! Decides which paths are noise (temp files, caches, build output) and should not be recorded.

use std::path::Path;

/// Folder names that never hold files a person cares about finding later.
const IGNORED_DIRS: &[&str] = &[
    "node_modules", ".git", ".svn", ".hg", "target", "__pycache__", ".venv", "venv",
    ".cache", ".vs", ".idea", "$recycle.bin", "system volume information", "appdata",
];

/// Exact file names that are always system noise.
const IGNORED_NAMES: &[&str] = &["desktop.ini", "thumbs.db", ".ds_store", "ntuser.dat"];

/// File endings used by temp, partial-download and lock files.
const IGNORED_SUFFIXES: &[&str] = &[
    ".tmp", ".temp", ".crdownload", ".part", ".partial", ".download", ".lock", ".swp", "~",
];

/// Returns true when the path should be skipped.
pub fn is_noise(path: &Path) -> bool {
    for comp in path.components() {
        let s = comp.as_os_str().to_string_lossy().to_lowercase();
        if IGNORED_DIRS.contains(&s.as_str()) {
            return true;
        }
    }
    let Some(name) = path.file_name() else { return true };
    let name = name.to_string_lossy().to_lowercase();
    if IGNORED_NAMES.contains(&name.as_str()) {
        return true;
    }
    // Office owner files (~$report.docx), LibreOffice locks (.~lock.x#), hidden dotfiles.
    if name.starts_with("~$") || name.starts_with(".~lock") || name.starts_with('.') {
        return true;
    }
    IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_normal_documents() {
        assert!(!is_noise(Path::new("/home/u/Documents/Thesis/chapter1.docx")));
        assert!(!is_noise(Path::new("C:/Users/k/Downloads/invoice.pdf")));
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
        ] {
            assert!(is_noise(Path::new(p)), "{p} should be noise");
        }
    }
}
