//! What's inside files, read and kept on this PC with no AI: the words in documents (PDF,
//! Word, PowerPoint, Excel, text), the title and author saved inside Office files, and when
//! and with which camera a photo was taken. The recorder fills this in quietly in the
//! background, and the everyday search uses it, so "the doc that mentions alumni" works
//! without an API key.
//!
//! The everyday search (`Db::find`) also forgives typos, knows a few everyday words
//! ("pic", "ppt", "cv", Hinglish too), understands kinds of file ("photo", "sheet") and app
//! names ("photoshop file") from the app log.

use crate::db::{Db, Hit};
use crate::extract::{self, Media};
use anyhow::Result;
use chrono::{Local, NaiveDateTime, TimeZone};
use rusqlite::{OptionalExtension, params};
use std::collections::HashSet;
use std::path::Path;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS file_content (
    file_id    INTEGER PRIMARY KEY,
    mtime      INTEGER NOT NULL,      -- stamp of the file's modified time and size when it was read
    title      TEXT,
    author     TEXT,
    taken_ts   INTEGER,               -- photos: when it was taken
    camera     TEXT,
    words      INTEGER NOT NULL DEFAULT 0,
    private    INTEGER NOT NULL DEFAULT 0, -- looked like an ID or bank detail: words not kept
    indexed_at INTEGER NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS content_fts USING fts5(meta, body, tokenize = 'porter unicode61 remove_diacritics 2');
CREATE VIRTUAL TABLE IF NOT EXISTS files_vocab USING fts5vocab(files_fts, 'row');
"#;

/// Kinds of file whose insides are read: documents (words, title, author) and photos (when
/// taken, camera).
const INDEXED_EXTS: &[&str] = &[
    "pdf", "docx", "pptx", "xlsx", "txt", "md", "csv", "rtf", "odt", "jpg", "jpeg", "png", "webp", "heic", "heif", "bmp", "gif", "tif", "tiff",
];

/// Files bigger than this aren't opened to read their words.
const MAX_READ_BYTES: u64 = 60 * 1024 * 1024;

/// A file found by the everyday search, with why it matched when it isn't the name.
#[derive(Debug, Clone, PartialEq)]
pub struct Match {
    pub path: String,
    pub last_seen: i64,
    pub last_kind: String,
    /// "mentions “alumni meet”", "made in Photoshop", "close to “budjet”"; None for a name match.
    pub why: Option<String>,
}

impl From<Hit> for Match {
    fn from(h: Hit) -> Match {
        Match { path: h.path, last_seen: h.last_seen, last_kind: h.last_kind, why: None }
    }
}

/// Words people use for the same thing.
const SAME_WORDS: &[&[&str]] = &[
    &["resume", "cv", "biodata"],
    &["invoice", "bill", "receipt"],
    &["assignment", "homework", "hw"],
    &["certificate", "cert", "certi"],
    &["presentation", "ppt", "slides", "deck"],
    &["notes", "note", "notebook"],
    &["report", "rpt"],
    &["project", "proj"],
    &["final", "finl", "fnl"],
    &["photo", "pic", "pics", "picture", "image", "img", "foto"],
    &["video", "vid", "clip", "footage"],
    &["wedding", "shaadi", "shadi", "vivah"],
    &["birthday", "bday", "janamdin"],
    &["college", "clg", "university", "uni"],
    &["school", "skool"],
    &["exam", "test", "paper", "pariksha"],
    &["form", "application"],
    &["marksheet", "result", "grades", "transcript"],
    &["aadhaar", "aadhar", "adhar"],
];

/// Kinds of file, by the words people use for them.
const KIND_WORDS: &[(&[&str], &[&str])] = &[
    (&["photo", "photos", "pic", "pics", "picture", "pictures", "image", "images", "img", "foto", "tasveer"], &["jpg", "jpeg", "png", "webp", "heic", "heif", "bmp", "gif", "tif", "tiff", "raw", "cr2", "nef", "arw", "dng"]),
    (&["video", "videos", "vid", "clip", "clips", "footage"], &["mp4", "mov", "avi", "mkv", "webm", "wmv", "m4v"]),
    (&["pdf", "pdfs"], &["pdf"]),
    (&["ppt", "pptx", "presentation", "slides", "deck"], &["ppt", "pptx", "key", "odp"]),
    (&["sheet", "sheets", "spreadsheet", "excel", "xlsx", "xls"], &["xls", "xlsx", "csv", "ods"]),
    (&["doc", "docs", "docx", "word"], &["doc", "docx", "odt", "rtf"]),
    (&["song", "songs", "music", "audio", "mp3", "track"], &["mp3", "wav", "m4a", "aac", "flac", "ogg", "wma"]),
    (&["zip", "zips", "archive", "rar"], &["zip", "rar", "7z"]),
];

/// Words that say nothing about which file.
const FILLER: &[&str] = &[
    "the", "a", "an", "my", "me", "i", "of", "for", "with", "and", "in", "on", "to", "from", "that", "this", "file", "files", "find", "where", "is",
    "was", "which", "wala", "wali", "vala", "vali", "ka", "ki", "ke", "mera", "meri", "mere", "made", "edited", "saved", "show", "some", "any",
];

/// Lower-cased words of a query, without punctuation.
fn words(query: &str) -> Vec<String> {
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn same_words(w: &str) -> Vec<&'static str> {
    SAME_WORDS.iter().find(|g| g.contains(&w)).map(|g| g.to_vec()).unwrap_or_default()
}

/// An FTS5 query: every word must match (as a prefix), each word or one of its other names.
fn fts_query(terms: &[String]) -> String {
    terms
        .iter()
        .map(|w| {
            let mut alts = vec![w.clone()];
            alts.extend(same_words(w).into_iter().filter(|s| *s != w).map(str::to_string));
            let parts: Vec<String> = alts.iter().map(|a| format!("\"{}\"*", a.replace('"', ""))).collect();
            if parts.len() == 1 { parts[0].clone() } else { format!("({})", parts.join(" OR ")) }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Edit distance (a swap of two letters counts as one), or `max + 1` when it's clearly more.
fn edits(a: &str, b: &str, max: usize) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return max + 1;
    }
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for j in 0..=b.len() {
        d[0][j] = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()].min(max + 1)
}

/// "Saturday 1 November 2024, evening" for a photo's meta words.
fn when_words(ts: i64) -> String {
    let Some(d) = Local.timestamp_opt(ts, 0).single() else { return String::new() };
    let part = match d.format("%H").to_string().parse::<u32>().unwrap_or(12) {
        5..=11 => "morning",
        12..=16 => "afternoon",
        17..=20 => "evening",
        _ => "night",
    };
    format!("{} {part}", d.format("%A %-d %B %Y"))
}

impl Db {
    /// Read what's inside up to `limit` files that are new or changed since they were last
    /// read, newest first. Returns how many were looked at (0 when there's nothing to do).
    pub fn index_content(&self, limit: usize, now: i64) -> Result<usize> {
        let exts: Vec<String> = INDEXED_EXTS.iter().map(|e| format!("'{e}'")).collect();
        let sql = format!(
            "SELECT f.id, f.path FROM files f LEFT JOIN file_content c ON c.file_id = f.id
             WHERE f.ext IN ({}) AND (c.file_id IS NULL OR f.last_seen > c.indexed_at)
             ORDER BY f.last_seen DESC LIMIT ?1",
            exts.join(",")
        );
        let todo: Vec<(i64, String)> = {
            let mut s = self.conn.prepare(&sql)?;
            s.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
        };
        for (id, path) in &todo {
            if let Err(e) = self.index_one(*id, Path::new(path), now) {
                eprintln!("could not read inside {path}: {e:#}");
                // Note it as read so it isn't tried again until it changes.
                self.save_content(*id, 0, &Default::default(), "", "", now)?;
            }
        }
        Ok(todo.len())
    }

    fn index_one(&self, id: i64, path: &Path, now: i64) -> Result<()> {
        let Ok(md) = std::fs::metadata(path) else {
            return self.save_content(id, 0, &Default::default(), "", "", now);
        };
        // Modified time and size together, so a quick second save is noticed too.
        let mtime = md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64).wrapping_mul(31).wrapping_add(md.len() as i64);
        let known: Option<i64> = self.conn.query_row("SELECT mtime FROM file_content WHERE file_id = ?1", [id], |r| r.get(0)).optional()?;
        if known == Some(mtime) && mtime != 0 {
            // Seen again but not changed: just note the time.
            self.conn.execute("UPDATE file_content SET indexed_at = ?2 WHERE file_id = ?1", params![id, now])?;
            return Ok(());
        }
        let mut info = Inside::default();
        let mut meta = Vec::new();
        let mut body = String::new();
        match extract::media_of(path) {
            Some(Media::Photo) => {
                let f = extract::photo_facts(path);
                info.taken_ts = f.taken.as_deref().and_then(|t| NaiveDateTime::parse_from_str(t, "%Y:%m:%d %H:%M:%S").ok()).and_then(|d| Local.from_local_datetime(&d).earliest()).map(|d| d.timestamp());
                info.camera = f.camera;
                meta.push("photo".to_string());
                if let Some(ts) = info.taken_ts {
                    meta.push(format!("taken {}", when_words(ts)));
                }
                if let Some(c) = &info.camera {
                    meta.push(c.clone());
                }
                if extract::is_screenshot(path) {
                    meta.push("screenshot".into());
                }
            }
            Some(Media::Document) if md.len() <= MAX_READ_BYTES => {
                let (title, author) = extract::office_props(path);
                info.title = title;
                info.author = author;
                if let Some(t) = &info.title {
                    meta.push(t.clone());
                }
                if let Some(a) = &info.author {
                    meta.push(format!("by {a}"));
                }
                let text = extract::document_text(path).unwrap_or_default();
                if extract::private_reason(path, &text).is_some() {
                    info.private = true;
                } else {
                    info.words = text.split_whitespace().count() as i64;
                    body = text;
                }
            }
            _ => {}
        }
        self.save_content(id, mtime, &info, &meta.join(" · "), &body, now)
    }

    fn save_content(&self, id: i64, mtime: i64, info: &Inside, meta: &str, body: &str, now: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO file_content (file_id, mtime, title, author, taken_ts, camera, words, private, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![id, mtime, info.title, info.author, info.taken_ts, info.camera, info.words, info.private, now],
        )?;
        tx.execute("DELETE FROM content_fts WHERE rowid = ?1", [id])?;
        if !meta.is_empty() || !body.is_empty() {
            tx.execute("INSERT INTO content_fts (rowid, meta, body) VALUES (?1, ?2, ?3)", params![id, meta, body])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// How far the reading of files has got: (files read, files to read).
    pub fn content_progress(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            &format!(
                "SELECT (SELECT count(*) FROM file_content), (SELECT count(*) FROM files WHERE ext IN ({}))",
                INDEXED_EXTS.iter().map(|e| format!("'{e}'")).collect::<Vec<_>>().join(",")
            ),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// How many files MemStor knows about.
    pub fn count_files(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?)
    }

    /// Forget everything read from inside files (wipe history, or a file forgotten).
    pub fn forget_content(&self, file_id: Option<i64>) -> Result<()> {
        match file_id {
            Some(id) => {
                self.conn.execute("DELETE FROM file_content WHERE file_id = ?1", [id])?;
                self.conn.execute("DELETE FROM content_fts WHERE rowid = ?1", [id])?;
            }
            None => {
                self.conn.execute("DELETE FROM file_content", [])?;
                self.conn.execute("DELETE FROM content_fts", [])?;
            }
        }
        Ok(())
    }

    /// The title, author and photo details read from inside a file, as a line of text.
    pub fn inside_summary(&self, path: &str) -> Result<Option<String>> {
        let row: Option<(Option<String>, Option<String>, Option<i64>, Option<String>, i64, bool)> = self
            .conn
            .query_row(
                "SELECT c.title, c.author, c.taken_ts, c.camera, c.words, c.private FROM file_content c JOIN files f ON f.id = c.file_id WHERE f.path = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?;
        let Some((title, author, taken, camera, n, private)) = row else { return Ok(None) };
        let mut parts = Vec::new();
        if let Some(t) = title {
            parts.push(format!("Title inside: {t}"));
        }
        if let Some(a) = author {
            parts.push(format!("Author: {a}"));
        }
        if let Some(t) = taken {
            parts.push(format!("Taken {}", when_words(t)));
        }
        if let Some(c) = camera {
            parts.push(format!("Camera: {c}"));
        }
        if n > 0 {
            parts.push(format!("{n} words read"));
        }
        if private {
            parts.push("Looks private (ID or bank details), so its words aren't kept".into());
        }
        Ok((!parts.is_empty()).then(|| parts.join(". ")))
    }

    /// Apps that were in front while a file was created or changed, most used first.
    pub fn apps_for(&self, path: &str) -> Result<Vec<String>> {
        let mut s = self.conn.prepare(
            "SELECT a.app, count(*) n FROM events e JOIN files f ON f.id = e.file_id
             JOIN app_usage a ON e.ts BETWEEN a.start_ts - 5 AND a.end_ts + 10
             WHERE f.path = ?1 AND e.kind IN ('created', 'modified', 'renamed')
             GROUP BY a.app ORDER BY n DESC LIMIT 3",
        )?;
        Ok(s.query_map([path], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    /// App names MemStor has seen, lower case, with how they're shown.
    fn known_apps(&self) -> Result<Vec<(String, String)>> {
        let mut s = self.conn.prepare("SELECT DISTINCT app FROM app_usage")?;
        let apps: Vec<String> = s.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(apps.into_iter().map(|a| (a.to_lowercase(), a)).collect())
    }

    /// The everyday search: file and folder names first, then what's inside files, then names
    /// close to a mistyped word. Understands kinds of file and app names.
    pub fn find(&self, query: &str, limit: usize) -> Result<Vec<Match>> {
        let all = words(query);
        if all.is_empty() {
            return Ok(vec![]);
        }
        // Kinds of file ("photo", "sheet") filter by extension instead of matching names.
        let mut kinds: Vec<&str> = Vec::new();
        // App names ("photoshop") filter to files changed while that app was in front.
        let apps = self.known_apps()?;
        let mut app: Option<String> = None;
        let mut terms: Vec<String> = Vec::new();
        for w in &all {
            if let Some((_, exts)) = KIND_WORDS.iter().find(|(ws, _)| ws.contains(&w.as_str())) {
                kinds.extend(exts.iter());
                continue;
            }
            if w.len() >= 4 {
                if let Some((_, shown)) = apps.iter().find(|(l, _)| l.split_whitespace().any(|p| p == w)) {
                    app = Some(shown.clone());
                    continue;
                }
            }
            if !FILLER.contains(&w.as_str()) {
                terms.push(w.clone());
            }
        }
        let kind_ok = |p: &str| kinds.is_empty() || kinds.contains(&extract::ext(Path::new(p)).as_str());
        let mut out: Vec<Match> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut push = |m: Match, out: &mut Vec<Match>| {
            if seen.insert(m.path.clone()) {
                out.push(m);
            }
        };

        // 1. Names, exactly as typed (keeps the old behaviour for plain file names).
        for h in self.search(query, limit * 2)? {
            if kind_ok(&h.path) {
                push(h.into(), &mut out);
            }
        }
        if !terms.is_empty() {
            // 2. Names, with other words for the same thing and without filler words.
            for m in self.name_matches(&terms, limit * 2, None)? {
                if kind_ok(&m.path) {
                    push(m, &mut out);
                }
            }
            // 3. Inside files.
            for m in self.content_matches(&terms, limit * 2)? {
                if kind_ok(&m.path) {
                    push(m, &mut out);
                }
            }
            // 4. Typos, when little was found.
            if out.len() < 3 {
                if let Some((fixed, note)) = self.fix_typos(&terms)? {
                    for m in self.name_matches(&fixed, limit, Some(note.clone()))? {
                        if kind_ok(&m.path) {
                            push(m, &mut out);
                        }
                    }
                    for mut m in self.content_matches(&fixed, limit)? {
                        if kind_ok(&m.path) {
                            m.why = Some(note.clone());
                            push(m, &mut out);
                        }
                    }
                }
            }
        } else if !kinds.is_empty() || app.is_some() {
            // Only a kind or an app: the most recent files of that kind.
            let mut s = self.conn.prepare(&format!(
                "SELECT f.path, f.last_seen, {} FROM files f WHERE NOT EXISTS (SELECT 1 FROM events d WHERE d.file_id = f.id AND d.kind = 'deleted' AND d.ts >= f.last_seen) ORDER BY f.last_seen DESC LIMIT 2000",
                crate::db::LAST_KIND
            ))?;
            let rows: Vec<Hit> = s.query_map([], |r| Ok(Hit { path: r.get(0)?, last_seen: r.get(1)?, last_kind: r.get(2)? }))?.collect::<Result<_, _>>()?;
            for h in rows.into_iter().filter(|h| kind_ok(&h.path)) {
                push(h.into(), &mut out);
                if out.len() >= limit * 4 {
                    break;
                }
            }
        }
        if let Some(app) = app {
            out.retain(|m| self.apps_for(&m.path).map(|a| a.contains(&app)).unwrap_or(false));
            for m in &mut out {
                if m.why.is_none() {
                    m.why = Some(format!("made in {app}"));
                }
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    fn name_matches(&self, terms: &[String], limit: usize, why: Option<String>) -> Result<Vec<Match>> {
        let sql = format!(
            "SELECT f.path, f.last_seen, {} FROM files_fts JOIN files f ON f.id = files_fts.rowid
             WHERE files_fts MATCH ?1 ORDER BY bm25(files_fts, 10.0, 1.0), f.last_seen DESC LIMIT ?2",
            crate::db::LAST_KIND
        );
        let mut s = self.conn.prepare(&sql)?;
        let rows = s.query_map(params![fts_query(terms), limit as i64], |r| {
            Ok(Match { path: r.get(0)?, last_seen: r.get(1)?, last_kind: r.get(2)?, why: why.clone() })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn content_matches(&self, terms: &[String], limit: usize) -> Result<Vec<Match>> {
        let sql = format!(
            "SELECT f.path, f.last_seen, {}, snippet(content_fts, -1, '“', '”', '…', 8) FROM content_fts JOIN files f ON f.id = content_fts.rowid
             WHERE content_fts MATCH ?1 ORDER BY bm25(content_fts, 5.0, 1.0), f.last_seen DESC LIMIT ?2",
            crate::db::LAST_KIND
        );
        let mut s = self.conn.prepare(&sql)?;
        let rows = s.query_map(params![fts_query(terms), limit as i64], |r| {
            let snip: String = r.get(3)?;
            Ok(Match { path: r.get(0)?, last_seen: r.get(1)?, last_kind: r.get(2)?, why: Some(format!("has {}", snip.split_whitespace().collect::<Vec<_>>().join(" "))) })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Swap words that match no file name for the closest word that does.
    fn fix_typos(&self, terms: &[String]) -> Result<Option<(Vec<String>, String)>> {
        let mut fixed = Vec::new();
        let mut changes = Vec::new();
        for w in terms {
            let known: bool = self.conn.query_row("SELECT EXISTS (SELECT 1 FROM files_vocab WHERE term >= ?1 AND term < ?1 || char(1114111))", [w], |r| r.get(0))?;
            let n = w.chars().count();
            if known || n < 4 || w.chars().all(|c| c.is_ascii_digit()) {
                fixed.push(w.clone());
                continue;
            }
            let max = if n >= 7 { 2 } else { 1 };
            let first: String = w.chars().next().map(String::from).unwrap_or_default();
            let mut s = self.conn.prepare("SELECT term, doc FROM files_vocab WHERE term >= ?1 AND term < ?1 || char(1114111)")?;
            let cands: Vec<(String, i64)> = s.query_map([&first], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
            let best = cands
                .into_iter()
                .filter(|(t, _)| t.chars().count().abs_diff(n) <= max)
                .map(|(t, docs)| (edits(w, &t, max), -docs, t))
                .filter(|(d, _, _)| *d <= max)
                .min();
            match best {
                Some((_, _, t)) => {
                    changes.push(format!("“{t}”"));
                    fixed.push(t);
                }
                None => fixed.push(w.clone()),
            }
        }
        Ok((!changes.is_empty()).then(|| (fixed, format!("close to {}", changes.join(", ")))))
    }
}

#[derive(Default)]
struct Inside {
    title: Option<String>,
    author: Option<String>,
    taken_ts: Option<i64>,
    camera: Option<String>,
    words: i64,
    private: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Kind;

    fn setup(tag: &str) -> (Db, std::path::PathBuf) {
        let dir = crate::test_dir(tag);
        (Db::in_memory().unwrap(), dir)
    }

    #[test]
    fn typo_distance() {
        assert_eq!(edits("budget", "budjet", 2), 1);
        assert_eq!(edits("recieve", "receive", 2), 1);
        assert!(edits("thesis", "banana", 2) > 2);
    }

    #[test]
    fn finds_words_inside_documents() {
        let (db, dir) = setup("content-inside");
        let doc = dir.join("notes 3.txt");
        std::fs::write(&doc, "Plan for the alumni meet: photos, banner and the welcome speech.").unwrap();
        let other = dir.join("shopping.txt");
        std::fs::write(&other, "milk eggs bread").unwrap();
        for p in [&doc, &other] {
            db.record(&p.display().to_string(), Kind::Created, "watcher", 1_000).unwrap();
        }
        assert_eq!(db.index_content(10, 2_000).unwrap(), 2);
        assert_eq!(db.index_content(10, 2_000).unwrap(), 0, "nothing left to read");
        let hits = db.find("alumni meet", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("notes 3.txt"));
        assert!(hits[0].why.as_deref().unwrap().contains("alumni"), "{:?}", hits[0].why);
        // A changed file is read again.
        std::fs::write(&other, "alumni list").unwrap();
        db.record(&other.display().to_string(), Kind::Modified, "watcher", 3_000).unwrap();
        db.index_content(10, 3_500).unwrap();
        assert_eq!(db.find("alumni", 10).unwrap().len(), 2);
        db.forget_content(None).unwrap();
        assert!(db.find("milk", 10).unwrap().is_empty());
    }

    #[test]
    fn private_documents_keep_no_words() {
        let (db, dir) = setup("content-private");
        let doc = dir.join("kyc.txt");
        std::fs::write(&doc, "Name Ravi Aadhaar 2345 6789 0123 address").unwrap();
        db.record(&doc.display().to_string(), Kind::Created, "watcher", 1_000).unwrap();
        db.index_content(10, 2_000).unwrap();
        assert!(db.find("ravi", 10).unwrap().is_empty());
        assert!(db.inside_summary(&doc.display().to_string()).unwrap().unwrap().contains("private"));
    }

    #[test]
    fn typos_other_words_kinds_and_apps() {
        let db = Db::in_memory().unwrap();
        for (p, ts) in [
            (r"C:\Users\a\Documents\Budget 2025.xlsx", 100),
            (r"C:\Users\a\Documents\Budget notes.docx", 1_000),
            (r"C:\Users\a\Pictures\poster final.png", 2_000),
            (r"C:\Users\a\Documents\Resume Kartik.pdf", 3_000),
            (r"C:\Users\a\Documents\poster brief.docx", 4_000),
        ] {
            db.record(p, Kind::Modified, "watcher", ts).unwrap();
        }
        // typo
        let hits = db.find("budjet", 10).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits[0].why.as_deref().unwrap().contains("budget"));
        // another word for the same thing
        assert!(db.find("my cv", 10).unwrap()[0].path.ends_with("Resume Kartik.pdf"));
        // a kind of file
        let posters = db.find("poster photo", 10).unwrap();
        assert_eq!(posters.len(), 1);
        assert!(posters[0].path.ends_with(".png"));
        assert_eq!(db.find("sheet", 10).unwrap().len(), 1);
        // an app
        let fg = crate::context::Foreground { app: "Photoshop".into(), exe: "photoshop.exe".into(), title: "poster".into(), ..Default::default() };
        db.log_app(&fg, 1_990).unwrap();
        db.log_app(&fg, 2_005).unwrap();
        let ps = db.find("photoshop file", 10).unwrap();
        assert_eq!(ps.len(), 1, "{ps:?}");
        assert_eq!(ps[0].why.as_deref(), Some("made in Photoshop"));
        assert_eq!(db.apps_for(r"C:\Users\a\Pictures\poster final.png").unwrap(), vec!["Photoshop".to_string()]);
    }
}
