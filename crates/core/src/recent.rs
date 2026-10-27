//! Reads Windows' Recent Items folder. Each .lnk shortcut there points at a file the user
//! opened, and the shortcut's modified time is when they last opened it.

use crate::db::{Db, Kind};
use crate::filter::is_noise;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// %APPDATA%\Microsoft\Windows\Recent on Windows; nothing elsewhere.
pub fn default_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        dirs::config_dir().map(|d| d.join("Microsoft").join("Windows").join("Recent"))
    } else {
        None
    }
}

/// Imports every shortcut in `dir` as an "opened" event. Returns how many were new.
pub fn scan(dir: &Path, db: &Db) -> Result<usize> {
    let mut added = 0;
    for entry in std::fs::read_dir(dir)? {
        let lnk_path = entry?.path();
        let is_lnk = lnk_path.extension().is_some_and(|e| e.eq_ignore_ascii_case("lnk"));
        if !is_lnk {
            continue;
        }
        let Some(target) = read_target(&lnk_path) else { continue };
        let target_path = Path::new(&target);
        // Recent Items also lists folders; we only want files.
        if target_path.is_dir() || is_noise(target_path) {
            continue;
        }
        let ts = std::fs::metadata(&lnk_path)?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if db.record(&target, Kind::Opened, "recent", ts)? {
            added += 1;
        }
    }
    Ok(added)
}

/// Returns the full path a shortcut points to, if the shortcut has one.
pub fn read_target(lnk_path: &Path) -> Option<String> {
    let lnk_path = lnk_path.to_path_buf();
    // The lnk parser panics on some malformed shortcuts; one bad file must not stop the scan.
    std::panic::catch_unwind(move || {
        let link = lnk::ShellLink::open(&lnk_path, lnk::encoding::WINDOWS_1252).ok()?;
        if let Some(t) = link.link_target().filter(|t| !t.is_empty()) {
            return Some(t);
        }
        // Some shortcuts have no LinkInfo block; fall back to working folder + relative path.
        let sd = link.string_data();
        let dir = sd.working_dir().as_ref()?;
        let rel = sd.relative_path().as_ref()?;
        let rel = rel.trim_start_matches(".\\").trim_start_matches("./");
        Some(Path::new(dir).join(rel).to_string_lossy().into_owned())
    })
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_shortcuts_as_opened_files() {
        let tmp = std::env::temp_dir().join(format!("memstor-recent-{}", std::process::id()));
        let docs = tmp.join("docs");
        let recent = tmp.join("recent");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::create_dir_all(&recent).unwrap();
        let report = docs.join("report.pdf");
        std::fs::write(&report, b"x").unwrap();

        lnk::ShellLink::new_simple(&report).unwrap().save(recent.join("report.pdf.lnk")).unwrap();
        lnk::ShellLink::new_simple(&docs).unwrap().save(recent.join("docs.lnk")).unwrap();
        std::fs::write(recent.join("notes.txt"), b"not a shortcut").unwrap();

        let db = Db::in_memory().unwrap();
        assert_eq!(scan(&recent, &db).unwrap(), 1, "only the file shortcut counts");
        assert_eq!(scan(&recent, &db).unwrap(), 0, "a second scan adds nothing new");

        let t = db.touched_between(0, i64::MAX).unwrap();
        assert_eq!(t.len(), 1);
        assert!(t[0].path.ends_with("report.pdf"), "{}", t[0].path);
        assert_eq!(t[0].kinds, "opened");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn reads_a_shortcut_made_by_windows() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/windows-sample.lnk");
        assert_eq!(read_target(&fixture).as_deref(), Some("C:\\test\\a.txt"));
    }

    #[test]
    fn garbage_file_is_skipped_not_fatal() {
        let tmp = std::env::temp_dir().join(format!("memstor-bad-{}.lnk", std::process::id()));
        std::fs::write(&tmp, b"L\0\0\0 definitely not a shortcut").unwrap();
        assert_eq!(read_target(&tmp), None);
        std::fs::remove_file(&tmp).ok();
    }
}
