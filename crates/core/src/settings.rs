//! Privacy settings: folders never to record, extra folders to watch, how long to keep
//! history, and deleting everything.

use crate::db::Db;
use crate::projects::is_inside;
use anyhow::{Result, ensure};
use rusqlite::params;
use std::time::{Duration, Instant};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS excluded_folders (path TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS extra_folders (path TEXT PRIMARY KEY);
"#;

/// Meta key: keep history for this many days (absent or 0 = keep everything).
const KEEP_DAYS_KEY: &str = "keep_days";
/// How often a connection re-reads the excluded folders.
const EXCLUDED_REFRESH: Duration = Duration::from_secs(20);

fn same_path(a: &str, b: &str) -> bool {
    if cfg!(windows) { a.replace('/', "\\").eq_ignore_ascii_case(&b.replace('/', "\\")) } else { a == b }
}

impl Db {
    fn list(&self, table: &str) -> Result<Vec<String>> {
        let mut s = self.conn.prepare(&format!("SELECT path FROM {table} ORDER BY path"))?;
        Ok(s.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    pub fn excluded_folders(&self) -> Result<Vec<String>> {
        self.list("excluded_folders")
    }

    pub fn extra_folders(&self) -> Result<Vec<String>> {
        self.list("extra_folders")
    }

    /// True when the path is in (or is) a folder the user excluded.
    pub fn is_excluded(&self, path: &str) -> Result<bool> {
        let stale = self.excluded.borrow().as_ref().is_none_or(|(at, _)| at.elapsed() > EXCLUDED_REFRESH);
        if stale {
            let list = self.excluded_folders()?;
            *self.excluded.borrow_mut() = Some((Instant::now(), list));
        }
        let cache = self.excluded.borrow();
        Ok(cache.as_ref().unwrap().1.iter().any(|f| same_path(path, f) || is_inside(path, f)))
    }

    /// Stop recording a folder and delete everything already recorded inside it.
    /// Returns how many files were forgotten.
    pub fn exclude_folder(&self, folder: &str) -> Result<usize> {
        let folder = folder.trim_end_matches(['\\', '/']);
        ensure!(!folder.is_empty(), "Pick a folder.");
        self.conn.execute("INSERT OR IGNORE INTO excluded_folders (path) VALUES (?1)", [folder])?;
        *self.excluded.borrow_mut() = None;
        let ids: Vec<(i64, String)> = {
            let mut s = self.conn.prepare("SELECT id, path FROM files")?;
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
        };
        let gone: Vec<(i64, String)> = ids.into_iter().filter(|(_, p)| same_path(p, folder) || is_inside(p, folder)).collect();
        self.forget_files(&gone)?;
        self.forget_descriptions_in(folder, "local", "in a folder you never record")?;
        Ok(gone.len())
    }

    /// Record a folder again.
    pub fn include_folder(&self, folder: &str) -> Result<()> {
        self.conn.execute("DELETE FROM excluded_folders WHERE path = ?1", [folder])?;
        *self.excluded.borrow_mut() = None;
        Ok(())
    }

    pub fn add_extra_folder(&self, folder: &str) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO extra_folders (path) VALUES (?1)", [folder.trim_end_matches(['\\', '/'])])?;
        Ok(())
    }

    pub fn remove_extra_folder(&self, folder: &str) -> Result<()> {
        self.conn.execute("DELETE FROM extra_folders WHERE path = ?1", [folder])?;
        Ok(())
    }

    /// Remove files and their events, search entries and memory links; drop memories left empty.
    fn forget_files(&self, files: &[(i64, String)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for (id, path) in files {
            tx.execute("DELETE FROM events WHERE file_id = ?1", [id])?;
            tx.execute("DELETE FROM files_fts WHERE rowid = ?1", [id])?;
            tx.execute("DELETE FROM files WHERE id = ?1", [id])?;
            tx.execute("DELETE FROM memory_files WHERE path = ?1", [path])?;
            tx.execute("DELETE FROM not_related WHERE a = ?1 OR b = ?1", [path])?;
        }
        tx.execute("DELETE FROM memory_days WHERE memory_id NOT IN (SELECT memory_id FROM memory_files)", [])?;
        tx.execute("DELETE FROM memories WHERE id NOT IN (SELECT memory_id FROM memory_files)", [])?;
        tx.commit()?;
        Ok(())
    }

    /// Days of history to keep; 0 means keep everything.
    pub fn keep_days(&self) -> Result<i64> {
        Ok(self.get_meta(KEEP_DAYS_KEY)?.unwrap_or(0))
    }

    pub fn set_keep_days(&self, days: i64) -> Result<()> {
        ensure!(days >= 0, "Pick how long to keep history.");
        self.set_meta(KEEP_DAYS_KEY, days)
    }

    /// Delete history older than the keep setting. Returns how many events were removed.
    pub fn prune(&self, now: i64) -> Result<usize> {
        let days = self.keep_days()?;
        if days == 0 {
            return Ok(0);
        }
        let cutoff = now - days * 86_400;
        let n = self.conn.execute("DELETE FROM events WHERE ts < ?1", [cutoff])?;
        let orphans: Vec<(i64, String)> = {
            let mut s = self.conn.prepare("SELECT id, path FROM files f WHERE NOT EXISTS (SELECT 1 FROM events e WHERE e.file_id = f.id)")?;
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
        };
        self.forget_files(&orphans)?;
        let old: Vec<i64> = {
            let mut s = self.conn.prepare("SELECT id FROM memories WHERE end < ?1")?;
            s.query_map([cutoff], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        for id in old {
            self.forget_memory(id)?;
        }
        Ok(n)
    }

    /// Delete all recorded history and memories. Projects and settings stay.
    /// Recording carries on from now; nothing from before is picked up again.
    pub fn wipe_history(&self, now: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for table in ["events", "files_fts", "files", "memory_files", "memory_days", "memories", "not_related"] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        // Don't let the catch-up scan or the memory saver re-read the past.
        tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('collector_alive_at', ?1)", params![now])?;
        tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('memories_next_day', ?1)", params![now])?;
        tx.commit()?;
        self.forget_cloud_descriptions()?;
        // Shrink the file so deleted data doesn't linger on disk.
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Kind;

    const T: i64 = 1_800_000_000;

    fn rec(db: &Db, path: &str, ts: i64) {
        db.record(path, Kind::Modified, "watcher", ts).unwrap();
    }

    #[test]
    fn excluded_folder_is_forgotten_and_not_recorded() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/home/k/Private/diary.docx", T);
        rec(&db, "/home/k/Private/sub/scan.pdf", T);
        rec(&db, "/home/k/Privateer/plan.docx", T);
        assert_eq!(db.exclude_folder("/home/k/Private/").unwrap(), 2);
        assert_eq!(db.excluded_folders().unwrap(), ["/home/k/Private"]);
        rec(&db, "/home/k/Private/new.docx", T + 10);
        let left: Vec<String> = db.recent(10).unwrap().into_iter().map(|h| h.path).collect();
        assert_eq!(left, ["/home/k/Privateer/plan.docx"]);
        assert!(db.search("diary", 10).unwrap().is_empty());

        db.include_folder("/home/k/Private").unwrap();
        rec(&db, "/home/k/Private/new.docx", T + 20);
        assert_eq!(db.recent(10).unwrap().len(), 2);
    }

    #[test]
    fn prune_keeps_recent_history() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/x/old.docx", T - 100 * 86_400);
        rec(&db, "/x/both.docx", T - 100 * 86_400);
        rec(&db, "/x/both.docx", T - 86_400);
        assert_eq!(db.prune(T).unwrap(), 0, "keeps everything by default");
        db.set_keep_days(90).unwrap();
        assert_eq!(db.prune(T).unwrap(), 2);
        let left: Vec<String> = db.recent(10).unwrap().into_iter().map(|h| h.path).collect();
        assert_eq!(left, ["/x/both.docx"]);
    }

    #[test]
    fn wipe_removes_history_but_keeps_projects() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/x/a.docx", T);
        db.create_project("Thesis").unwrap();
        db.wipe_history(T + 5).unwrap();
        assert!(db.recent(10).unwrap().is_empty());
        assert!(db.search("a", 10).unwrap().is_empty());
        assert_eq!(db.projects().unwrap().len(), 1);
        assert_eq!(db.get_meta("collector_alive_at").unwrap(), Some(T + 5));
    }
}
