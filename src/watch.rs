//! Watches folders for file changes and writes them to the database.

use crate::db::{Db, Kind};
use crate::filter::is_noise;
use crate::recent;
use anyhow::Result;
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

/// How often to re-read Recent Items while watching.
const RECENT_EVERY: Duration = Duration::from_secs(60);
/// Saves often fire several change events; count one per file within this window.
const MODIFY_DEBOUNCE_SECS: i64 = 5;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Turns raw watcher events into database records.
pub struct Recorder<'a> {
    db: &'a Db,
    last_modify: HashMap<PathBuf, i64>,
    /// Windows reports a rename as two events ("old name", then "new name"); hold the first.
    pending_rename_from: Option<PathBuf>,
}

impl<'a> Recorder<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db, last_modify: HashMap::new(), pending_rename_from: None }
    }

    pub fn handle(&mut self, kind: EventKind, paths: &[PathBuf], ts: i64) -> Result<()> {
        // A lone "old name" not followed by "new name" means the file left the watched folder.
        let is_rename_to = matches!(kind, EventKind::Modify(ModifyKind::Name(RenameMode::To)));
        if !is_rename_to {
            if let Some(from) = self.pending_rename_from.take() {
                self.db.record(&s(&from), Kind::Deleted, "watcher", ts)?;
            }
        }

        match kind {
            EventKind::Create(_) => {
                for p in paths.iter().filter(|p| p.is_file() && !is_noise(p)) {
                    self.db.record(&s(p), Kind::Created, "watcher", ts)?;
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
                self.rename(&paths[0], &paths[1], ts)?;
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                self.pending_rename_from = paths.first().cloned();
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                if let Some(to) = paths.first() {
                    match self.pending_rename_from.take() {
                        Some(from) => self.rename(&from, to, ts)?,
                        None if to.is_file() && !is_noise(to) => {
                            self.db.record(&s(to), Kind::Created, "watcher", ts)?;
                        }
                        None => {}
                    }
                }
            }
            // Access-time and attribute changes are not real edits.
            EventKind::Modify(ModifyKind::Metadata(_)) => {}
            EventKind::Modify(_) => {
                for p in paths.iter().filter(|p| p.is_file() && !is_noise(p)) {
                    let recent = self.last_modify.get(p).is_some_and(|&t| ts - t < MODIFY_DEBOUNCE_SECS);
                    if !recent {
                        self.db.record(&s(p), Kind::Modified, "watcher", ts)?;
                    }
                    self.last_modify.insert(p.clone(), ts);
                }
            }
            EventKind::Remove(_) => {
                for p in paths {
                    self.db.record(&s(p), Kind::Deleted, "watcher", ts)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn rename(&mut self, from: &Path, to: &Path, ts: i64) -> Result<()> {
        match (is_noise(from), is_noise(to)) {
            // Apps often save as "x.tmp" then rename it over "x.docx": that is an edit of x.docx.
            (true, false) => {
                if to.is_file() {
                    self.db.record(&s(to), Kind::Modified, "watcher", ts)?;
                }
            }
            (false, false) => self.db.rename(&s(from), &s(to), "watcher", ts)?,
            (false, true) => {
                self.db.record(&s(from), Kind::Deleted, "watcher", ts)?;
            }
            (true, true) => {}
        }
        Ok(())
    }
}

/// Folders watched when the user names none: Desktop, Documents, Downloads.
pub fn default_folders() -> Vec<PathBuf> {
    [dirs::desktop_dir(), dirs::document_dir(), dirs::download_dir()]
        .into_iter()
        .flatten()
        .filter(|p| p.is_dir())
        .collect()
}

/// Runs until the process is stopped.
pub fn run(db: &Db, folders: &[PathBuf], recent_dir: Option<PathBuf>) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    for f in folders {
        watcher.watch(f, RecursiveMode::Recursive)?;
        println!("watching {}", f.display());
    }
    if let Some(dir) = &recent_dir {
        let n = recent::scan(dir, db)?;
        println!("reading Recent Items from {} ({n} new)", dir.display());
    }
    println!("recording… press Ctrl+C to stop");

    let mut rec = Recorder::new(db);
    let mut last_recent = std::time::Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(ev)) => {
                if let Err(e) = rec.handle(ev.kind, &ev.paths, now()) {
                    eprintln!("could not record {:?}: {e}", ev.paths);
                }
            }
            Ok(Err(e)) => eprintln!("watcher error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(dir) = &recent_dir {
            if last_recent.elapsed() >= RECENT_EVERY {
                last_recent = std::time::Instant::now();
                if let Err(e) = recent::scan(dir, db) {
                    eprintln!("could not read Recent Items: {e}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, RemoveKind};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("memstor-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn windows_style_rename_pair_keeps_history() {
        let dir = tmpdir("rename");
        let (a, b) = (dir.join("draft.docx"), dir.join("final.docx"));
        std::fs::write(&a, b"x").unwrap();
        let db = Db::in_memory().unwrap();
        let mut rec = Recorder::new(&db);
        rec.handle(EventKind::Create(CreateKind::File), &[a.clone()], 1).unwrap();
        std::fs::rename(&a, &b).unwrap();
        rec.handle(EventKind::Modify(ModifyKind::Name(RenameMode::From)), &[a.clone()], 2).unwrap();
        rec.handle(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &[b.clone()], 2).unwrap();

        let t = db.touched_between(0, 10).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].path, s(&b));
        assert_eq!(t[0].kinds, "created,renamed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_via_temp_file_counts_as_edit_and_bursts_are_debounced() {
        let dir = tmpdir("save");
        let doc = dir.join("budget.xlsx");
        std::fs::write(&doc, b"x").unwrap();
        let db = Db::in_memory().unwrap();
        let mut rec = Recorder::new(&db);
        let m = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        rec.handle(m, &[doc.clone()], 10).unwrap();
        rec.handle(m, &[doc.clone()], 11).unwrap();
        rec.handle(m, &[doc.clone()], 12).unwrap();
        rec.handle(EventKind::Modify(ModifyKind::Name(RenameMode::Both)), &[dir.join("A1B2.tmp"), doc.clone()], 30).unwrap();
        rec.handle(EventKind::Remove(RemoveKind::File), &[dir.join("ignored-never-seen.txt")], 31).unwrap();

        let t = db.touched_between(0, 100).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].events, 2, "one burst at t=10, one temp-file save at t=30");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn real_watcher_sees_a_new_file() {
        let dir = tmpdir("live");
        let db = Db::in_memory().unwrap();
        let (tx, rx) = mpsc::channel();
        let mut w = notify::recommended_watcher(tx).unwrap();
        w.watch(&dir, RecursiveMode::Recursive).unwrap();
        std::fs::create_dir_all(dir.join("Thesis")).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(dir.join("Thesis").join("notes.md"), b"hello").unwrap();
        std::fs::write(dir.join("~$notes.docx"), b"lock").unwrap();

        let mut rec = Recorder::new(&db);
        while let Ok(Ok(ev)) = rx.recv_timeout(Duration::from_millis(500)) {
            rec.handle(ev.kind, &ev.paths, 5).unwrap();
        }
        let paths: Vec<String> = db.touched_between(0, 10).unwrap().into_iter().map(|t| t.path).collect();
        assert_eq!(paths.len(), 1, "{paths:?}");
        assert!(paths[0].ends_with("notes.md"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
