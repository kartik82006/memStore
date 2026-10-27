//! Local SQLite store: one row per file, one row per thing that happened to it.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub struct Db {
    conn: Connection,
}

/// What happened to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Created,
    Modified,
    Renamed,
    Deleted,
    Opened,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Created => "created",
            Kind::Modified => "modified",
            Kind::Renamed => "renamed",
            Kind::Deleted => "deleted",
            Kind::Opened => "opened",
        }
    }
}

/// One line of the "files I touched" list.
#[derive(Debug, Clone)]
pub struct Touched {
    pub path: String,
    pub first: i64,
    pub last: i64,
    pub events: i64,
    pub kinds: String,
}

/// A file in search results, with what last happened to it.
#[derive(Debug, Clone)]
pub struct Hit {
    pub path: String,
    pub last_seen: i64,
    pub last_kind: String,
}

/// The newest event's kind for a file (ties broken by insert order).
const LAST_KIND: &str =
    "(SELECT kind FROM events e WHERE e.file_id = f.id ORDER BY ts DESC, id DESC LIMIT 1)";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS files (
    id         INTEGER PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    ext        TEXT NOT NULL,
    first_seen INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id      INTEGER PRIMARY KEY,
    file_id INTEGER NOT NULL REFERENCES files(id),
    ts      INTEGER NOT NULL,          -- unix seconds
    kind    TEXT NOT NULL,             -- created | modified | renamed | deleted | opened
    source  TEXT NOT NULL,             -- watcher | recent
    detail  TEXT                       -- e.g. old path for a rename
);
CREATE UNIQUE INDEX IF NOT EXISTS events_dedupe ON events(file_id, ts, kind, source);
CREATE INDEX IF NOT EXISTS events_ts ON events(ts);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS files_fts USING fts5(name, path, tokenize = 'unicode61');
"#;

fn split_name(path: &str) -> (String, String) {
    let p = Path::new(path);
    let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
    (name, ext)
}

/// FTS5 tokenizes on punctuation, so make folder separators and dots searchable words.
fn fts_text(s: &str) -> String {
    s.replace(['\\', '/', '.', '_', '-'], " ")
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // The collector and the search window each hold a connection; wait instead of failing.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    fn file_id(&self, path: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM files WHERE path = ?1", [path], |r| r.get(0))
            .optional()?)
    }

    fn upsert_file(&self, path: &str, ts: i64) -> Result<i64> {
        if let Some(id) = self.file_id(path)? {
            self.conn.execute(
                "UPDATE files SET last_seen = max(last_seen, ?2) WHERE id = ?1",
                params![id, ts],
            )?;
            return Ok(id);
        }
        let (name, ext) = split_name(path);
        self.conn.execute(
            "INSERT INTO files (path, name, ext, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![path, name, ext, ts],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "INSERT INTO files_fts (rowid, name, path) VALUES (?1, ?2, ?3)",
            params![id, fts_text(&name), fts_text(path)],
        )?;
        Ok(id)
    }

    /// Records one event. Returns false if the same event was already stored.
    pub fn record(&self, path: &str, kind: Kind, source: &str, ts: i64) -> Result<bool> {
        // A delete only matters for a file we already know about.
        if kind == Kind::Deleted && self.file_id(path)?.is_none() {
            return Ok(false);
        }
        let id = self.upsert_file(path, ts)?;
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO events (file_id, ts, kind, source) VALUES (?1, ?2, ?3, ?4)",
            params![id, ts, kind.as_str(), source],
        )?;
        Ok(n > 0)
    }

    /// Moves a file's history to its new path, so a rename keeps everything it had.
    pub fn rename(&self, from: &str, to: &str, source: &str, ts: i64) -> Result<()> {
        let id = match (self.file_id(from)?, self.file_id(to)?) {
            (Some(old), None) => {
                let (name, ext) = split_name(to);
                self.conn.execute(
                    "UPDATE files SET path = ?2, name = ?3, ext = ?4, last_seen = ?5 WHERE id = ?1",
                    params![old, to, name, ext, ts],
                )?;
                self.conn.execute(
                    "UPDATE files_fts SET name = ?2, path = ?3 WHERE rowid = ?1",
                    params![old, fts_text(&name), fts_text(to)],
                )?;
                old
            }
            (_, Some(existing)) => existing,
            (None, None) => self.upsert_file(to, ts)?,
        };
        self.conn.execute(
            "INSERT OR IGNORE INTO events (file_id, ts, kind, source, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, ts, Kind::Renamed.as_str(), source, from],
        )?;
        Ok(())
    }

    /// Files with any activity in [start, end), most recent first.
    pub fn touched_between(&self, start: i64, end: i64) -> Result<Vec<Touched>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, min(e.ts), max(e.ts), count(*), group_concat(DISTINCT e.kind)
             FROM events e JOIN files f ON f.id = e.file_id
             WHERE e.ts >= ?1 AND e.ts < ?2
             GROUP BY f.id ORDER BY max(e.ts) DESC",
        )?;
        let rows = stmt.query_map(params![start, end], |r| {
            Ok(Touched { path: r.get(0)?, first: r.get(1)?, last: r.get(2)?, events: r.get(3)?, kinds: r.get(4)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Most recently active files, newest first, skipping files whose last event was a delete.
    pub fn recent(&self, limit: usize) -> Result<Vec<Hit>> {
        let sql = format!(
            "SELECT f.path, f.last_seen, {LAST_KIND} AS k FROM files f
             WHERE k != 'deleted' ORDER BY f.last_seen DESC LIMIT ?1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([limit as i64], |r| Ok(Hit { path: r.get(0)?, last_seen: r.get(1)?, last_kind: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Time of the newest event recorded for a file, if any.
    pub fn last_event_ts(&self, path: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT max(e.ts) FROM events e JOIN files f ON f.id = e.file_id WHERE f.path = ?1",
                [path],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Full-text search over file names and folders. Each word matches as a prefix.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        let terms: Vec<String> = fts_text(query)
            .split_whitespace()
            .map(|w| format!("\"{}\"*", w.replace('"', "")))
            .collect();
        if terms.is_empty() {
            return Ok(vec![]);
        }
        let sql = format!(
            "SELECT f.path, f.last_seen, {LAST_KIND} FROM files_fts JOIN files f ON f.id = files_fts.rowid
             WHERE files_fts MATCH ?1 ORDER BY bm25(files_fts, 10.0, 1.0), f.last_seen DESC LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![terms.join(" "), limit as i64], |r| {
            Ok(Hit { path: r.get(0)?, last_seen: r.get(1)?, last_kind: r.get(2)? })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_lists_a_day() {
        let db = Db::in_memory().unwrap();
        assert!(db.record("C:/Docs/Thesis/ch1.docx", Kind::Created, "watcher", 100).unwrap());
        assert!(db.record("C:/Docs/Thesis/ch1.docx", Kind::Modified, "watcher", 150).unwrap());
        assert!(!db.record("C:/Docs/Thesis/ch1.docx", Kind::Modified, "watcher", 150).unwrap(), "duplicate is ignored");
        db.record("C:/Docs/old.txt", Kind::Opened, "recent", 10).unwrap();

        let t = db.touched_between(50, 200).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].path, "C:/Docs/Thesis/ch1.docx");
        assert_eq!((t[0].first, t[0].last, t[0].events), (100, 150, 2));
    }

    #[test]
    fn delete_of_unknown_file_is_ignored() {
        let db = Db::in_memory().unwrap();
        assert!(!db.record("C:/x.txt", Kind::Deleted, "watcher", 1).unwrap());
        assert!(db.touched_between(0, 10).unwrap().is_empty());
    }

    #[test]
    fn rename_keeps_history_and_updates_search() {
        let db = Db::in_memory().unwrap();
        db.record("C:/Docs/draft.docx", Kind::Created, "watcher", 1).unwrap();
        db.rename("C:/Docs/draft.docx", "C:/Docs/Budget 2026.docx", "watcher", 2).unwrap();

        let t = db.touched_between(0, 10).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].path, "C:/Docs/Budget 2026.docx");
        assert_eq!(t[0].events, 2);
        assert_eq!(db.search("budg", 10).unwrap()[0].path, "C:/Docs/Budget 2026.docx");
        assert!(db.search("draft", 10).unwrap().is_empty());
    }

    #[test]
    fn recent_lists_newest_first_and_hides_deleted() {
        let db = Db::in_memory().unwrap();
        db.record("C:/a.txt", Kind::Created, "watcher", 1).unwrap();
        db.record("C:/b.txt", Kind::Created, "watcher", 2).unwrap();
        db.record("C:/c.txt", Kind::Created, "watcher", 3).unwrap();
        db.record("C:/c.txt", Kind::Deleted, "watcher", 4).unwrap();
        db.record("C:/a.txt", Kind::Modified, "watcher", 5).unwrap();
        let hits = db.recent(10).unwrap();
        let paths: Vec<_> = hits.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["C:/a.txt", "C:/b.txt"]);
        assert_eq!(hits[0].last_kind, "modified");
    }

    #[test]
    fn meta_values_round_trip() {
        let db = Db::in_memory().unwrap();
        assert_eq!(db.get_meta("last_alive").unwrap(), None);
        db.set_meta("last_alive", 5).unwrap();
        db.set_meta("last_alive", 9).unwrap();
        assert_eq!(db.get_meta("last_alive").unwrap(), Some(9));
    }

    #[test]
    fn search_matches_folder_names() {
        let db = Db::in_memory().unwrap();
        db.record("D:/Uni/Thesis/figures/plot.png", Kind::Created, "watcher", 1).unwrap();
        db.record("D:/Work/plot.png", Kind::Created, "watcher", 2).unwrap();
        let hits = db.search("thesis plot", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "D:/Uni/Thesis/figures/plot.png");
        assert_eq!(hits[0].last_kind, "created");
    }
}
