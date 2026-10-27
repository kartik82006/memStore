//! Projects: a name plus the folders that belong to it. A day's files for a project are the
//! files in those folders, plus files elsewhere (Downloads, Desktop…) used around the same time.

use crate::db::{Db, Touched};
use anyhow::{Result, bail};
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS not_related (
    a TEXT NOT NULL,
    b TEXT NOT NULL,
    PRIMARY KEY (a, b)
);
CREATE TABLE IF NOT EXISTS projects (
    id      INTEGER PRIMARY KEY,
    name    TEXT NOT NULL UNIQUE COLLATE NOCASE,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS project_folders (
    project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    path       TEXT NOT NULL,
    PRIMARY KEY (project_id, path)
);
"#;

/// Files used within this many minutes of a project file count as "used around the same time".
pub const DEFAULT_NEARBY_MINUTES: i64 = 30;

#[derive(Debug, Clone)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub folders: Vec<String>,
}

/// Why a file is listed under a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The file is inside one of the project's folders.
    InFolder,
    /// The file is elsewhere but was used around the same time as the project's files.
    Nearby,
}

#[derive(Debug, Clone)]
pub struct ProjectFile {
    pub file: Touched,
    pub reason: Reason,
}

/// A folder that looks like a project, with how many files were used in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub folder: String,
    pub name: String,
    pub files: i64,
}

/// Compares paths the way the OS does: case-insensitive with either slash on Windows.
fn norm(p: &str) -> String {
    if cfg!(windows) { p.replace('/', "\\").to_lowercase() } else { p.to_string() }
}

/// True when `path` is inside `folder` (at any depth).
pub fn is_inside(path: &str, folder: &str) -> bool {
    let (p, f) = (norm(path), norm(folder));
    let sep = if cfg!(windows) { '\\' } else { '/' };
    let f = f.trim_end_matches(sep);
    p.len() > f.len() && p.starts_with(f) && p[f.len()..].starts_with(sep)
}

/// Folder names that hold many unrelated things, so they are never a project themselves.
const CONTAINER_NAMES: &[&str] = &[
    "desktop", "documents", "downloads", "pictures", "videos", "music", "screenshots",
    "camera roll", "saved pictures", "onedrive", "dropbox", "google drive", "icloud drive",
];

impl Db {
    pub fn create_project(&self, name: &str) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            bail!("Give the project a name.");
        }
        let now = chrono::Utc::now().timestamp();
        match self.conn.execute("INSERT INTO projects (name, created) VALUES (?1, ?2)", params![name, now]) {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                bail!("A project called \"{name}\" already exists.")
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn delete_project(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM project_folders WHERE project_id = ?1", [id])?;
        self.conn.execute("DELETE FROM projects WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn add_project_folder(&self, id: i64, folder: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO project_folders (project_id, path) VALUES (?1, ?2)",
            params![id, folder.trim_end_matches(['/', '\\'])],
        )?;
        Ok(())
    }

    pub fn remove_project_folder(&self, id: i64, folder: &str) -> Result<()> {
        self.conn.execute("DELETE FROM project_folders WHERE project_id = ?1 AND path = ?2", params![id, folder])?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare("SELECT id, name FROM projects ORDER BY name COLLATE NOCASE")?;
        let mut list: Vec<Project> = stmt
            .query_map([], |r| Ok(Project { id: r.get(0)?, name: r.get(1)?, folders: vec![] }))?
            .collect::<Result<_, _>>()?;
        let mut fstmt = self.conn.prepare("SELECT path FROM project_folders WHERE project_id = ?1 ORDER BY path")?;
        for p in &mut list {
            p.folders = fstmt.query_map([p.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
        }
        Ok(list)
    }

    /// The files used for a project in [start, end): files in its folders, plus files elsewhere
    /// used within `nearby_minutes` of one of them. Files that belong to another project's
    /// folders are never counted as nearby.
    pub fn project_files(&self, id: i64, start: i64, end: i64, nearby_minutes: i64) -> Result<Vec<ProjectFile>> {
        let all = self.projects()?;
        let Some(project) = all.iter().find(|p| p.id == id) else { bail!("That project no longer exists.") };
        let other_folders: Vec<&String> = all.iter().filter(|p| p.id != id).flat_map(|p| &p.folders).collect();
        let in_project = |path: &str| project.folders.iter().any(|f| is_inside(path, f));
        let in_other = |path: &str| other_folders.iter().any(|f| is_inside(path, f));

        let touched = self.touched_between(start, end)?;
        let events = self.events_between(start, end)?;
        let project_times: Vec<i64> = events.iter().filter(|(p, _)| in_project(p)).map(|(_, t)| *t).collect();
        let window = nearby_minutes * 60;
        let mut nearby: HashSet<&str> = HashSet::new();
        if !project_times.is_empty() {
            for (path, ts) in &events {
                if in_project(path) || in_other(path) {
                    continue;
                }
                // project_times is sorted because events are; find the closest one.
                let i = project_times.partition_point(|t| t < ts);
                let close = |j: usize| project_times.get(j).is_some_and(|t| (t - ts).abs() <= window);
                if close(i) || (i > 0 && close(i - 1)) {
                    nearby.insert(path.as_str());
                }
            }
        }

        Ok(touched
            .into_iter()
            .filter_map(|t| {
                let reason = if in_project(&t.path) {
                    Reason::InFolder
                } else if nearby.contains(t.path.as_str()) {
                    Reason::Nearby
                } else {
                    return None;
                };
                Some(ProjectFile { file: t, reason })
            })
            .collect())
    }

    /// Folders that look like projects: the first specific folder under a general one
    /// (Documents\Thesis, Desktop\Whatsapp Inventory, D:\Uni) with at least two files used
    /// in [start, end). Folders already in a project are left out.
    pub fn suggest_projects(&self, start: i64, end: i64, home: Option<&Path>, limit: usize) -> Result<Vec<Suggestion>> {
        let taken: Vec<String> = self.projects()?.into_iter().flat_map(|p| p.folders).collect();
        let mut counts: HashMap<PathBuf, i64> = HashMap::new();
        for t in self.touched_between(start, end)? {
            let path = Path::new(&t.path);
            if taken.iter().any(|f| is_inside(&t.path, f)) {
                continue;
            }
            if let Some(folder) = project_folder_for(path, home) {
                *counts.entry(folder).or_default() += 1;
            }
        }
        let mut out: Vec<Suggestion> = counts
            .into_iter()
            .filter(|(_, n)| *n >= 2)
            .map(|(folder, files)| Suggestion {
                name: folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                folder: folder.to_string_lossy().into_owned(),
                files,
            })
            .collect();
        out.sort_by(|a, b| b.files.cmp(&a.files).then(a.name.cmp(&b.name)));
        out.truncate(limit);
        Ok(out)
    }
}

/// The folder that would be this file's project: walking down from the drive, skip general
/// folders (the drive, the user folder, its direct subfolders, Documents, Downloads…) and
/// return the first specific one. None when the file sits directly in a general folder.
pub fn project_folder_for(file: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let parent = file.parent()?;
    let home_norm = home.map(|h| norm(&h.to_string_lossy()));
    let is_container = |dir: &Path| -> bool {
        if dir.parent().is_none() || dir.file_name().is_none() {
            return true; // drive root
        }
        let d = norm(&dir.to_string_lossy());
        if let Some(h) = &home_norm {
            if &d == h || dir.parent().is_some_and(|p| &norm(&p.to_string_lossy()) == h) || is_inside(h, &d) {
                return true; // the user folder, its direct subfolders, or C:\Users
            }
        }
        let name = dir.file_name().unwrap().to_string_lossy().to_lowercase();
        CONTAINER_NAMES.contains(&name.as_str())
    };
    let ancestors: Vec<&Path> = parent.ancestors().collect();
    ancestors.into_iter().rev().find(|d| !is_container(d)).map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Kind;

    const H: &str = "/home/k";

    fn rec(db: &Db, path: &str, ts: i64) {
        db.record(path, Kind::Modified, "watcher", ts).unwrap();
    }

    #[test]
    fn inside_checks_whole_folder_names() {
        assert!(is_inside("/home/k/Documents/Thesis/ch1.docx", "/home/k/Documents/Thesis"));
        assert!(is_inside("/home/k/Documents/Thesis/a/b.png", "/home/k/Documents/Thesis/"));
        assert!(!is_inside("/home/k/Documents/Thesis2/ch1.docx", "/home/k/Documents/Thesis"));
        assert!(!is_inside("/home/k/Documents/Thesis", "/home/k/Documents/Thesis"));
    }

    #[test]
    fn project_folder_skips_general_folders() {
        let h = Some(Path::new(H));
        let f = |p: &str| project_folder_for(Path::new(p), h).map(|p| p.to_string_lossy().into_owned());
        assert_eq!(f("/home/k/Documents/Thesis/figures/plot.png").as_deref(), Some("/home/k/Documents/Thesis"));
        assert_eq!(f("/home/k/Desktop/Whatsapp Inventory/plan.pdf").as_deref(), Some("/home/k/Desktop/Whatsapp Inventory"));
        assert_eq!(f("/home/k/OneDrive/Documents/Club/list.xlsx").as_deref(), Some("/home/k/OneDrive/Documents/Club"));
        assert_eq!(f("/data/Uni/Sem5/notes.docx").as_deref(), Some("/data"));
        assert_eq!(f("/home/k/Downloads/invoice.pdf"), None);
        assert_eq!(f("/home/k/Pictures/Screenshots/shot.png"), None);
    }

    #[test]
    fn names_are_unique_and_required() {
        let db = Db::in_memory().unwrap();
        db.create_project("Thesis").unwrap();
        assert!(db.create_project("thesis").is_err());
        assert!(db.create_project("  ").is_err());
    }

    #[test]
    fn project_files_include_nearby_files_but_not_other_projects() {
        let db = Db::in_memory().unwrap();
        let thesis = db.create_project("Thesis").unwrap();
        db.add_project_folder(thesis, "/home/k/Documents/Thesis").unwrap();
        let club = db.create_project("Club").unwrap();
        db.add_project_folder(club, "/home/k/Documents/Club").unwrap();

        let base = 1_000_000;
        rec(&db, "/home/k/Documents/Thesis/ch1.docx", base);
        rec(&db, "/home/k/Downloads/paper.pdf", base + 10 * 60); // 10 min later: nearby
        rec(&db, "/home/k/Documents/Club/list.xlsx", base + 5 * 60); // another project's file
        rec(&db, "/home/k/Downloads/movie.mp4", base + 3 * 3600); // hours later: unrelated

        let files = db.project_files(thesis, 0, i64::MAX, 30).unwrap();
        let got: Vec<(&str, Reason)> = files.iter().map(|f| (f.file.path.as_str(), f.reason)).collect();
        assert_eq!(got.len(), 2, "{got:?}");
        assert!(got.contains(&("/home/k/Documents/Thesis/ch1.docx", Reason::InFolder)));
        assert!(got.contains(&("/home/k/Downloads/paper.pdf", Reason::Nearby)));

        // With no project files that day, nothing is "nearby".
        assert!(db.project_files(thesis, base + 3600, i64::MAX, 30).unwrap().is_empty());
    }

    #[test]
    fn suggestions_group_by_project_folder_and_skip_existing_projects() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/home/k/Documents/Thesis/ch1.docx", 1);
        rec(&db, "/home/k/Documents/Thesis/figures/plot.png", 2);
        rec(&db, "/home/k/Desktop/Shop/stock.xlsx", 3);
        rec(&db, "/home/k/Desktop/Shop/prices.xlsx", 4);
        rec(&db, "/home/k/Desktop/Shop/logo.png", 5);
        rec(&db, "/home/k/Downloads/a.pdf", 6);
        rec(&db, "/home/k/Downloads/b.pdf", 7);
        rec(&db, "/home/k/Documents/Lonely/one.docx", 8);

        let s = db.suggest_projects(0, 100, Some(Path::new(H)), 10).unwrap();
        let names: Vec<(&str, i64)> = s.iter().map(|s| (s.name.as_str(), s.files)).collect();
        assert_eq!(names, [("Shop", 3), ("Thesis", 2)]);

        let id = db.create_project("Thesis").unwrap();
        db.add_project_folder(id, "/home/k/Documents/Thesis").unwrap();
        let s = db.suggest_projects(0, 100, Some(Path::new(H)), 10).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(db.projects().unwrap()[0].folders, ["/home/k/Documents/Thesis"]);
    }

    #[test]
    fn deleting_a_project_removes_its_folders() {
        let db = Db::in_memory().unwrap();
        let id = db.create_project("X").unwrap();
        db.add_project_folder(id, "/a/b").unwrap();
        db.delete_project(id).unwrap();
        assert!(db.projects().unwrap().is_empty());
        let n: i64 = db.conn.query_row("SELECT count(*) FROM project_folders", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 0);
    }
}
