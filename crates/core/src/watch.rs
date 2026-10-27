//! Watches folders for file changes and writes them to the database.

use crate::db::{Db, Kind};
use crate::filter::{is_noise, is_noise_dir_name};
use crate::recent;
use anyhow::Result;
use notify::event::{EventKind, ModifyKind, RenameMode};
use notify::{RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// How often to re-read Recent Items while watching.
const RECENT_EVERY: Duration = Duration::from_secs(60);
/// Saves often fire several change events; count one per file within this window.
const MODIFY_DEBOUNCE_SECS: i64 = 5;
/// On the very first run, pick up files edited in this many past days.
const FIRST_RUN_LOOKBACK_DAYS: i64 = 14;
/// Database key holding the last time the collector was known to be running.
const ALIVE_KEY: &str = "collector_alive_at";

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
            // Word and Excel rename the original to a temp name while saving, then put the new
            // version back under the real name. Not a delete; the rename back records the edit.
            (false, true) => {}
            (true, true) => {}
        }
        Ok(())
    }
}

/// Folders watched when the user names none: every folder inside the user folder (Desktop,
/// Documents, Downloads, Pictures, OneDrive…) except AppData and hidden tool folders, plus,
/// on Windows, every other fixed drive (D:, E:…). AppData is left out of the watch itself,
/// not just filtered, because browsers write there constantly and could flood the watcher.
pub fn default_folders() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = dirs::home_dir().map(|h| user_subfolders(&h)).unwrap_or_default();
    out.extend(other_fixed_drives());
    out.retain(|p| p.is_dir());
    out
}

fn user_subfolders(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| !is_noise_dir_name(&e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

#[cfg(windows)]
fn other_fixed_drives() -> Vec<PathBuf> {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    const DRIVE_FIXED: u32 = 3;
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()).to_uppercase();
    (b'A'..=b'Z')
        .map(|l| format!("{}:\\", l as char))
        .filter(|root| !root.starts_with(&system))
        .filter(|root| {
            let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
            // SAFETY: `wide` is a valid NUL-terminated UTF-16 string that outlives the call.
            unsafe { GetDriveTypeW(wide.as_ptr()) == DRIVE_FIXED }
        })
        .map(PathBuf::from)
        .collect()
}

#[cfg(not(windows))]
fn other_fixed_drives() -> Vec<PathBuf> {
    Vec::new()
}

/// Records files in `folders` that were changed after `since` (e.g. while MemStor was closed).
/// Skips noise folders without entering them. Returns how many edits were added.
pub fn catch_up(db: &Db, folders: &[PathBuf], since: i64, control: &Control) -> Result<usize> {
    let mut added = 0;
    let mut stack: Vec<PathBuf> = folders.to_vec();
    while let Some(dir) = stack.pop() {
        if control.stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                if !is_noise_dir_name(&entry.file_name().to_string_lossy()) {
                    stack.push(path);
                }
                continue;
            }
            if !ft.is_file() || is_noise(&path) {
                continue;
            }
            let Some(mtime) = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
            else {
                continue;
            };
            if mtime <= since {
                continue;
            }
            let p = s(&path);
            // Already recorded by the watcher (or a previous catch-up) around that time.
            if db.last_event_ts(&p)?.is_some_and(|t| t >= mtime - MODIFY_DEBOUNCE_SECS) {
                continue;
            }
            if db.record(&p, Kind::Modified, "scan", mtime)? {
                added += 1;
            }
        }
    }
    Ok(added)
}

/// Lets another thread pause recording or stop the collector.
#[derive(Default)]
pub struct Control {
    pub paused: AtomicBool,
    pub stop: AtomicBool,
}

impl Control {
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
    pub fn set_paused(&self, v: bool) {
        self.paused.store(v, Ordering::Relaxed)
    }
}

/// Watches `folders` and re-reads Recent Items every minute until `control.stop` is set.
/// While paused, events are dropped and nothing is written.
pub fn run(db: &Db, folders: &[PathBuf], recent_dir: Option<PathBuf>, control: &Control) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx)?;
    for f in folders {
        match watcher.watch(f, RecursiveMode::Recursive) {
            Ok(()) => println!("watching {}", f.display()),
            Err(e) => eprintln!("cannot watch {}: {e}", f.display()),
        }
    }
    if let Some(dir) = &recent_dir {
        if !control.is_paused() {
            let n = recent::scan(dir, db)?;
            println!("reading Recent Items from {} ({n} new)", dir.display());
        }
    }
    // Pick up edits made while MemStor was not running. The watcher is already on,
    // so nothing saved during the scan is missed.
    let since = db.get_meta(ALIVE_KEY)?.unwrap_or_else(|| now() - FIRST_RUN_LOOKBACK_DAYS * 86_400);
    db.set_meta(ALIVE_KEY, now())?;
    if !control.is_paused() {
        match catch_up(db, folders, since, control) {
            Ok(n) => println!("found {n} file(s) edited while MemStor was closed"),
            Err(e) => eprintln!("could not check for edits made while closed: {e}"),
        }
    }

    let mut rec = Recorder::new(db);
    let mut last_recent = std::time::Instant::now();
    while !control.stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Ok(ev)) if !control.is_paused() => {
                if let Err(e) = rec.handle(ev.kind, &ev.paths, now()) {
                    eprintln!("could not record {:?}: {e}", ev.paths);
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("watcher error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if last_recent.elapsed() >= RECENT_EVERY {
            last_recent = std::time::Instant::now();
            // Kept up to date even while paused, so edits made during a pause aren't
            // picked up later by the catch-up scan.
            if let Err(e) = db.set_meta(ALIVE_KEY, now()) {
                eprintln!("could not save heartbeat: {e}");
            }
            if let (Some(dir), false) = (&recent_dir, control.is_paused()) {
                if let Err(e) = recent::scan(dir, db) {
                    eprintln!("could not read Recent Items: {e}");
                }
            }
        }
    }
    let _ = db.set_meta(ALIVE_KEY, now());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, DataChange, RemoveKind};

    fn tmpdir(tag: &str) -> PathBuf {
        crate::test_dir(tag)
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
    fn pause_drops_events_and_stop_ends_the_loop() {
        let dir = tmpdir("pause");
        let db_path = dir.join("m.db");
        let control = std::sync::Arc::new(Control::default());
        control.set_paused(true);
        let (c2, d2, p2) = (control.clone(), dir.clone(), db_path.clone());
        let h = std::thread::spawn(move || {
            let db = Db::open(&p2).unwrap();
            run(&db, &[d2], None, &c2).unwrap();
        });
        std::thread::sleep(Duration::from_millis(300));
        std::fs::write(dir.join("while-paused.txt"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        control.set_paused(false);
        std::thread::sleep(Duration::from_millis(1200));
        std::fs::write(dir.join("after-resume.txt"), b"x").unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        control.stop.store(true, Ordering::Relaxed);
        h.join().unwrap();

        let db = Db::open(&db_path).unwrap();
        let paths: Vec<String> = db.touched_between(0, i64::MAX).unwrap().into_iter().map(|t| t.path).collect();
        assert!(paths.iter().any(|p| p.ends_with("after-resume.txt")), "{paths:?}");
        assert!(!paths.iter().any(|p| p.ends_with("while-paused.txt")), "{paths:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn word_style_save_is_an_edit_not_a_delete() {
        let dir = tmpdir("wordsave");
        let doc = dir.join("thesis.docx");
        std::fs::write(&doc, b"v1").unwrap();
        let db = Db::in_memory().unwrap();
        let mut rec = Recorder::new(&db);
        let both = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        rec.handle(EventKind::Create(CreateKind::File), &[doc.clone()], 1).unwrap();
        // Word: original -> backup temp, new temp -> original name.
        rec.handle(both, &[doc.clone(), dir.join("~WRL0001.tmp")], 20).unwrap();
        rec.handle(both, &[dir.join("~WRD0000.tmp"), doc.clone()], 20).unwrap();

        let t = db.touched_between(0, 100).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].kinds, "created,modified");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn user_subfolders_skip_appdata_and_hidden() {
        let home = tmpdir("home");
        for d in ["Documents", "AppData", ".vscode", "OneDrive"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::fs::write(home.join("NTUSER.DAT"), b"x").unwrap();
        let names: Vec<String> = user_subfolders(&home)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Documents", "OneDrive"]);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn catch_up_finds_edits_made_while_closed() {
        let dir = tmpdir("catchup");
        std::fs::create_dir_all(dir.join("Thesis")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("Thesis").join("ch1.docx"), b"x").unwrap();
        std::fs::write(dir.join("node_modules").join("readme.md"), b"x").unwrap();
        std::fs::write(dir.join("setup.exe"), b"x").unwrap();
        let db = Db::in_memory().unwrap();
        let control = Control::default();

        assert_eq!(catch_up(&db, &[dir.clone()], 0, &control).unwrap(), 1);
        assert_eq!(catch_up(&db, &[dir.clone()], 0, &control).unwrap(), 0, "no duplicates on a second scan");
        assert_eq!(catch_up(&db, &[dir.clone()], now() + 60, &control).unwrap(), 0, "older than `since` is skipped");
        let t = db.touched_between(0, i64::MAX).unwrap();
        assert!(t[0].path.ends_with("ch1.docx"));
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
