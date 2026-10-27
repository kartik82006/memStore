//! "Gather": put shortcuts to a set of files into one new folder. The original files are
//! never moved or copied, so nothing can be lost.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Gathered {
    pub folder: PathBuf,
    pub linked: usize,
    /// Files that no longer exist, so no shortcut was made.
    pub missing: Vec<String>,
}

/// Where gathered folders go: Documents\MemStor Gathered.
pub fn default_root() -> PathBuf {
    dirs::document_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("MemStor Gathered")
}

/// Removes characters Windows does not allow in folder names.
pub fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control() { '-' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() { "Gathered files".into() } else { trimmed.to_string() }
}

/// `base`, or `base (2)`, `base (3)`… whichever does not exist yet.
fn unused(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut n = 1;
    loop {
        let name = if n == 1 { format!("{base}{ext}") } else { format!("{base} ({n}){ext}") };
        let p = dir.join(name);
        if !p.exists() {
            return p;
        }
        n += 1;
    }
}

/// Makes `root\<title>` (or `<title> (2)` if taken) and fills it with shortcuts to `files`.
pub fn gather(files: &[String], root: &Path, title: &str) -> Result<Gathered> {
    std::fs::create_dir_all(root).with_context(|| format!("could not create {}", root.display()))?;
    let folder = unused(root, &safe_name(title), "");
    std::fs::create_dir(&folder).with_context(|| format!("could not create {}", folder.display()))?;

    let mut linked = 0;
    let mut missing = Vec::new();
    for f in files {
        let target = Path::new(f);
        if !target.is_file() {
            missing.push(f.clone());
            continue;
        }
        let name = target.file_name().unwrap().to_string_lossy();
        make_shortcut(target, &folder, &name).with_context(|| format!("could not make a shortcut to {f}"))?;
        linked += 1;
    }
    Ok(Gathered { folder, linked, missing })
}

/// A Windows shortcut (.lnk) that Explorer opens like the file itself.
#[cfg(windows)]
fn make_shortcut(target: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let lnk = unused(dir, name, ".lnk");
    let link = mslnk::ShellLink::new(target).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    link.create_lnk(&lnk).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok(lnk)
}

/// Elsewhere (used for development and tests), a symbolic link does the same job.
#[cfg(unix)]
fn make_shortcut(target: &Path, dir: &Path, name: &str) -> Result<PathBuf> {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    let link = unused(dir, &stem, &ext);
    std::os::unix::fs::symlink(target, &link)?;
    Ok(link)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_name_strips_forbidden_characters() {
        assert_eq!(safe_name("Thesis: 27/10/2026?"), "Thesis- 27-10-2026-");
        assert_eq!(safe_name("  ..  "), "Gathered files");
    }

    #[test]
    fn gathers_shortcuts_and_reports_missing_files() {
        let dir = crate::test_dir("gather");
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("a")).unwrap();
        std::fs::create_dir_all(src.join("b")).unwrap();
        // Two files with the same name in different folders must both get a shortcut.
        for p in ["a/report.docx", "b/report.docx", "plot.png"] {
            std::fs::write(src.join(p), b"x").unwrap();
        }
        let files: Vec<String> = ["a/report.docx", "b/report.docx", "plot.png", "gone.pdf"]
            .iter()
            .map(|p| src.join(p).to_string_lossy().into_owned())
            .collect();
        let root = dir.join("out");

        let g = gather(&files, &root, "Thesis 2026-10-27").unwrap();
        assert_eq!(g.folder, root.join("Thesis 2026-10-27"));
        assert_eq!(g.linked, 3);
        assert_eq!(g.missing.len(), 1);
        assert_eq!(std::fs::read_dir(&g.folder).unwrap().count(), 3);

        // Gathering again with the same title makes a second folder, never mixes into the first.
        let g2 = gather(&files[..1], &root, "Thesis 2026-10-27").unwrap();
        assert_eq!(g2.folder, root.join("Thesis 2026-10-27 (2)"));

        // The originals are untouched.
        assert!(src.join("a/report.docx").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// On Windows, ask Windows itself where each shortcut points.
    #[cfg(windows)]
    #[test]
    fn windows_resolves_the_shortcuts() {
        let dir = crate::test_dir("lnk");
        let target = dir.join("Budget 2026.xlsx");
        std::fs::write(&target, b"x").unwrap();
        let g = gather(&[target.to_string_lossy().into_owned()], &dir.join("out"), "T").unwrap();
        let lnk = std::fs::read_dir(&g.folder).unwrap().next().unwrap().unwrap().path();
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!(
                "(New-Object -ComObject WScript.Shell).CreateShortcut('{}').TargetPath",
                lnk.display()
            )])
            .output()
            .unwrap();
        let resolved = String::from_utf8_lossy(&out.stdout).trim().to_lowercase();
        assert_eq!(resolved, target.to_string_lossy().to_lowercase());
        std::fs::remove_dir_all(&dir).ok();
    }
}
