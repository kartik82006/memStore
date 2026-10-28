//! Cloud descriptions and smart search.
//!
//! Off until the user turns it on and pastes their own Claude API key. Then, while the PC is
//! idle, a background worker goes through the user's photos, videos and documents:
//! 1. On the PC: skip screenshots, copies and tiny images; keep excluded and "local only"
//!    folders, and files with private details (ID, bank, passwords), on the PC.
//! 2. Send a shrunk picture (photos, one frame of each video) or a text excerpt (documents)
//!    to Claude Haiku 5.5, which returns a structured description.
//! 3. Store the description only here, in the encrypted database, searchable with FTS5.
//!
//! Spending is capped: $5 for the first full index, then $1 a month, and the worker stops and
//! asks before going over. Smart search turns a question into a plan, searches descriptions,
//! file names and memories on the PC, and asks Claude to rerank the best few.

use crate::claude::{CallError, Claude, Description, Spend};
use crate::db::Db;
use crate::extract::{self, Media};
use crate::filter::is_noise_dir_name;
use crate::projects::is_inside;
use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use rusqlite::{OptionalExtension, params};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS local_only_folders (path TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS describe_queue (
    path        TEXT PRIMARY KEY,
    media       TEXT NOT NULL,
    size        INTEGER NOT NULL DEFAULT 0,
    mtime       INTEGER NOT NULL DEFAULT 0,
    fingerprint TEXT,
    status      TEXT NOT NULL DEFAULT 'pending',  -- pending | done | local | skipped | error
    reason      TEXT,
    tries       INTEGER NOT NULL DEFAULT 0,
    added       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS describe_queue_status ON describe_queue(status);
CREATE INDEX IF NOT EXISTS describe_queue_fp ON describe_queue(fingerprint);
CREATE TABLE IF NOT EXISTS descriptions (
    id        INTEGER PRIMARY KEY,
    path      TEXT NOT NULL UNIQUE,
    media     TEXT NOT NULL,
    summary   TEXT NOT NULL,
    details   TEXT NOT NULL,            -- the full description as JSON
    taken     TEXT,                     -- photo date from the camera
    camera    TEXT,
    lat       REAL,                     -- GPS stays on the PC; never sent
    lon       REAL,
    file_time INTEGER NOT NULL,         -- when it was taken or last changed
    described_at INTEGER NOT NULL,
    micro_usd INTEGER NOT NULL DEFAULT 0
);
CREATE VIRTUAL TABLE IF NOT EXISTS descriptions_fts USING fts5(name, text, tokenize = 'porter unicode61');
CREATE TABLE IF NOT EXISTS spend (month TEXT PRIMARY KEY, micro_usd INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS memory_stories (
    memory_id INTEGER PRIMARY KEY,
    story     TEXT NOT NULL,
    files     INTEGER NOT NULL          -- how many files the memory had when written
);
"#;

/// Spending caps, in millionths of a dollar.
pub const FIRST_INDEX_CAP: i64 = 5_000_000;
pub const MONTHLY_CAP: i64 = 1_000_000;
/// "Allow $1 more" raises the current cap by this much.
pub const ALLOW_STEP: i64 = 1_000_000;
/// Don't start a call with less than this left (a large document costs about this much).
const MIN_LEFT: i64 = 3_000;
/// Wait for this much keyboard and mouse quiet before describing (unless "Index now").
const IDLE_SECS: u64 = 120;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn month(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m").to_string()).unwrap_or_default()
}

fn same_or_inside(path: &str, folder: &str) -> bool {
    path.eq_ignore_ascii_case(folder) || is_inside(path, folder)
}

/// Where the cloud work stands, for the Settings screen.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CloudStatus {
    pub on: bool,
    pub has_key: bool,
    pub key_problem: Option<String>,
    /// What the worker is doing: off, no_key, scanning, waiting_idle, describing, cap, done, key_problem
    pub state: String,
    pub pending: i64,
    pub described: i64,
    pub kept_local: i64,
    pub skipped: i64,
    pub errors: i64,
    pub first_index_done: bool,
    /// Spent on the first index, or this month after it.
    pub spent: i64,
    pub cap: i64,
    pub spent_total: i64,
}

/// A smart search result: a file, or a memory (a piece of work).
#[derive(Debug, Clone, serde::Serialize)]
pub struct Found {
    pub path: Option<String>,
    pub memory_id: Option<i64>,
    pub title: String,
    pub date: i64,
    pub summary: String,
    pub why: String,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SmartAnswer {
    /// What was understood, as short phrases.
    pub understood: Vec<String>,
    pub results: Vec<Found>,
    /// Set when the cloud wasn't used (off, no key, over the cap) and why.
    pub local_only: Option<String>,
    pub micro_usd: i64,
}

impl Db {
    // ---------- settings ----------

    pub fn kv(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_kv(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            Some(v) => self.conn.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", params![key, v])?,
            None => self.conn.execute("DELETE FROM kv WHERE key = ?1", [key])?,
        };
        Ok(())
    }

    fn kv_i64(&self, key: &str) -> Result<i64> {
        Ok(self.kv(key)?.and_then(|v| v.parse().ok()).unwrap_or(0))
    }

    pub fn cloud_on(&self) -> Result<bool> {
        Ok(self.kv("cloud_on")?.as_deref() == Some("1"))
    }

    pub fn set_cloud_on(&self, on: bool) -> Result<()> {
        self.set_kv("cloud_on", on.then_some("1"))
    }

    /// The user's API key. The database is encrypted, so it is kept there.
    pub fn api_key(&self) -> Result<Option<String>> {
        Ok(self.kv("api_key")?.filter(|k| !k.trim().is_empty()))
    }

    pub fn set_api_key(&self, key: Option<&str>) -> Result<()> {
        self.set_kv("key_problem", None)?;
        self.set_kv("api_key", key.map(str::trim).filter(|k| !k.is_empty()))
    }

    pub fn local_only_folders(&self) -> Result<Vec<String>> {
        let mut s = self.conn.prepare("SELECT path FROM local_only_folders ORDER BY path")?;
        Ok(s.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    /// Keep a folder's files on the PC: never described in the cloud, and forget any
    /// descriptions already made for them.
    pub fn add_local_only_folder(&self, folder: &str) -> Result<usize> {
        let folder = folder.trim_end_matches(['\\', '/']);
        self.conn.execute("INSERT OR IGNORE INTO local_only_folders (path) VALUES (?1)", [folder])?;
        self.forget_descriptions_in(folder, "local", "in a folder you keep on this PC")
    }

    pub fn remove_local_only_folder(&self, folder: &str) -> Result<()> {
        self.conn.execute("DELETE FROM local_only_folders WHERE path = ?1", [folder])?;
        // Let those files be described now.
        let paths: Vec<String> = self.queue_paths_with_status("local")?;
        for p in paths.iter().filter(|p| same_or_inside(p, folder)) {
            self.conn.execute("UPDATE describe_queue SET status = 'pending', reason = NULL WHERE path = ?1", [p])?;
        }
        Ok(())
    }

    fn queue_paths_with_status(&self, status: &str) -> Result<Vec<String>> {
        let mut s = self.conn.prepare("SELECT path FROM describe_queue WHERE status = ?1")?;
        Ok(s.query_map([status], |r| r.get(0))?.collect::<Result<_, _>>()?)
    }

    /// Delete descriptions under a folder and mark those files with `status`.
    pub(crate) fn forget_descriptions_in(&self, folder: &str, status: &str, reason: &str) -> Result<usize> {
        let rows: Vec<(i64, String)> = {
            let mut s = self.conn.prepare("SELECT id, path FROM descriptions")?;
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?
        };
        let mut n = 0;
        for (id, _) in rows.into_iter().filter(|(_, p)| same_or_inside(p, folder)) {
            self.conn.execute("DELETE FROM descriptions_fts WHERE rowid = ?1", [id])?;
            self.conn.execute("DELETE FROM descriptions WHERE id = ?1", [id])?;
            n += 1;
        }
        let queued: Vec<String> = {
            let mut s = self.conn.prepare("SELECT path FROM describe_queue")?;
            s.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        for p in queued.iter().filter(|p| same_or_inside(p, folder)) {
            self.conn.execute("UPDATE describe_queue SET status = ?2, reason = ?3 WHERE path = ?1", params![p, status, reason])?;
        }
        Ok(n)
    }

    /// Delete every cloud description and memory story. The files are not touched, and
    /// the queue starts over if the cloud is still on.
    pub fn forget_cloud_descriptions(&self) -> Result<()> {
        for t in ["descriptions_fts", "descriptions", "describe_queue", "memory_stories"] {
            self.conn.execute(&format!("DELETE FROM {t}"), [])?;
        }
        for k in ["library_scanned", "first_index_done"] {
            self.set_kv(k, None)?;
        }
        Ok(())
    }

    // ---------- spending ----------

    pub fn first_index_done(&self) -> Result<bool> {
        Ok(self.kv("first_index_done")?.is_some())
    }

    fn record_spend(&self, s: Spend) -> Result<()> {
        if s.micro_usd == 0 {
            return Ok(());
        }
        if self.first_index_done()? {
            self.conn.execute(
                "INSERT INTO spend (month, micro_usd) VALUES (?1, ?2) ON CONFLICT(month) DO UPDATE SET micro_usd = micro_usd + ?2",
                params![month(now()), s.micro_usd],
            )?;
        } else {
            let v = self.kv_i64("first_index_micro")? + s.micro_usd;
            self.set_kv("first_index_micro", Some(&v.to_string()))?;
        }
        let total = self.kv_i64("total_micro")? + s.micro_usd;
        self.set_kv("total_micro", Some(&total.to_string()))
    }

    /// (spent, cap) for the current period: the first index, or this month after it.
    pub fn spend_and_cap(&self) -> Result<(i64, i64)> {
        if self.first_index_done()? {
            let m = month(now());
            let spent: i64 = self.conn.query_row("SELECT micro_usd FROM spend WHERE month = ?1", [&m], |r| r.get(0)).optional()?.unwrap_or(0);
            Ok((spent, MONTHLY_CAP + self.kv_i64(&format!("allow_{m}"))?))
        } else {
            Ok((self.kv_i64("first_index_micro")?, FIRST_INDEX_CAP + self.kv_i64("allow_first")?))
        }
    }

    /// The user agreed to spend `ALLOW_STEP` more in the current period.
    pub fn allow_more(&self) -> Result<()> {
        let key = if self.first_index_done()? { format!("allow_{}", month(now())) } else { "allow_first".into() };
        let v = self.kv_i64(&key)? + ALLOW_STEP;
        self.set_kv(&key, Some(&v.to_string()))
    }

    fn money_left(&self) -> Result<i64> {
        let (spent, cap) = self.spend_and_cap()?;
        Ok(cap - spent)
    }

    // ---------- queue ----------

    /// Why a file must stay on the PC because of where it is, if it must.
    fn kept_local_by_folder(&self, path: &str) -> Result<Option<String>> {
        if self.excluded_folders()?.iter().any(|f| same_or_inside(path, f)) {
            return Ok(Some("in a folder MemStor never records".into()));
        }
        if self.local_only_folders()?.iter().any(|f| same_or_inside(path, f)) {
            return Ok(Some("in a folder you keep on this PC".into()));
        }
        Ok(None)
    }

    /// Add one file to the queue (or re-queue it if it changed). Returns true if it will be described.
    pub fn queue_file(&self, path: &Path) -> Result<bool> {
        let Some(media) = extract::media_of(path) else { return Ok(false) };
        let Ok(meta) = std::fs::metadata(path) else { return Ok(false) };
        if !meta.is_file() {
            return Ok(false);
        }
        let p = path.to_string_lossy().into_owned();
        let mtime = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
        let size = meta.len() as i64;
        let known: Option<(i64, i64, String)> = self
            .conn
            .query_row("SELECT size, mtime, status FROM describe_queue WHERE path = ?1", [&p], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        if let Some((s, m, _)) = &known {
            if *s == size && *m == mtime {
                return Ok(false); // unchanged
            }
        }
        let (status, reason, fp): (&str, Option<String>, Option<String>) = if extract::is_screenshot(path) {
            ("skipped", Some("screenshot".into()), None)
        } else if let Some(r) = self.kept_local_by_folder(&p)? {
            ("local", Some(r), None)
        } else if media == Media::Photo && size < 20_000 {
            ("skipped", Some("too small to be a photo".into()), None)
        } else {
            let fp = extract::fingerprint(path).ok();
            let copy_of: Option<String> = match &fp {
                Some(f) => self
                    .conn
                    .query_row("SELECT path FROM describe_queue WHERE fingerprint = ?1 AND path != ?2 LIMIT 1", params![f, p], |r| r.get(0))
                    .optional()?,
                None => None,
            };
            match copy_of {
                Some(other) => ("skipped", Some(format!("copy of {other}")), fp),
                None => ("pending", None, fp),
            }
        };
        self.conn.execute(
            "INSERT INTO describe_queue (path, media, size, mtime, fingerprint, status, reason, tries, added)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)
             ON CONFLICT(path) DO UPDATE SET media = ?2, size = ?3, mtime = ?4, fingerprint = ?5, status = ?6, reason = ?7, tries = 0",
            params![p, media.as_str(), size, mtime, fp, status, reason, now()],
        )?;
        Ok(status == "pending")
    }

    /// Walk the folders once and queue every photo, video and document. Returns how many were queued.
    pub fn scan_library(&self, folders: &[PathBuf], stop: &AtomicBool) -> Result<usize> {
        let mut n = 0;
        let mut stack: Vec<PathBuf> = folders.to_vec();
        let excluded = self.excluded_folders()?;
        while let Some(dir) = stack.pop() {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let ds = dir.to_string_lossy();
            if excluded.iter().any(|e| same_or_inside(&ds, e)) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                let path = entry.path();
                if ft.is_dir() {
                    if !is_noise_dir_name(&entry.file_name().to_string_lossy()) {
                        stack.push(path);
                    }
                } else if ft.is_file() && !crate::filter::is_noise(&path) && self.queue_file(&path)? {
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// Queue files MemStor saw used since `since` (new and edited files).
    pub fn queue_recent(&self, since: i64) -> Result<usize> {
        let paths: Vec<String> = {
            let mut s = self.conn.prepare(
                "SELECT DISTINCT f.path FROM events e JOIN files f ON f.id = e.file_id WHERE e.ts >= ?1 AND e.kind != 'deleted'",
            )?;
            s.query_map([since], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        let mut n = 0;
        for p in paths {
            if self.queue_file(Path::new(&p))? {
                n += 1;
            }
        }
        Ok(n)
    }

    fn next_pending(&self) -> Result<Option<(String, Media)>> {
        // Newest files first: what you worked on lately is most likely to be searched for.
        Ok(self
            .conn
            .query_row(
                "SELECT path, media FROM describe_queue WHERE status = 'pending' ORDER BY mtime DESC LIMIT 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?
            .and_then(|(p, m)| Media::parse(&m).map(|m| (p, m))))
    }

    fn set_status(&self, path: &str, status: &str, reason: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE describe_queue SET status = ?2, reason = ?3 WHERE path = ?1", params![path, status, reason])?;
        Ok(())
    }

    fn store_description(&self, path: &str, media: Media, d: &Description, facts: &extract::PhotoFacts, file_time: i64, spend: Spend) -> Result<()> {
        let details = serde_json::to_string(d)?;
        let old: Option<i64> = self.conn.query_row("SELECT id FROM descriptions WHERE path = ?1", [path], |r| r.get(0)).optional()?;
        if let Some(id) = old {
            self.conn.execute("DELETE FROM descriptions_fts WHERE rowid = ?1", [id])?;
            self.conn.execute("DELETE FROM descriptions WHERE id = ?1", [id])?;
        }
        self.conn.execute(
            "INSERT INTO descriptions (path, media, summary, details, taken, camera, lat, lon, file_time, described_at, micro_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![path, media.as_str(), d.summary, details, facts.taken, facts.camera, facts.gps.map(|g| g.0), facts.gps.map(|g| g.1), file_time, now(), spend.micro_usd],
        )?;
        let id = self.conn.last_insert_rowid();
        let name = Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let folder = Path::new(path).parent().and_then(|p| p.file_name()).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        self.conn.execute(
            "INSERT INTO descriptions_fts (rowid, name, text) VALUES (?1, ?2, ?3)",
            params![id, format!("{name} {folder}"), d.search_text()],
        )?;
        Ok(())
    }

    /// Counts and spending for the Settings screen. `state` comes from the worker.
    pub fn cloud_status(&self, state: &str) -> Result<CloudStatus> {
        let count = |st: &str| -> Result<i64> {
            Ok(self.conn.query_row("SELECT count(*) FROM describe_queue WHERE status = ?1", [st], |r| r.get(0))?)
        };
        let (spent, cap) = self.spend_and_cap()?;
        Ok(CloudStatus {
            on: self.cloud_on()?,
            has_key: self.api_key()?.is_some(),
            key_problem: self.kv("key_problem")?,
            state: state.to_string(),
            pending: count("pending")?,
            described: self.conn.query_row("SELECT count(*) FROM descriptions", [], |r| r.get(0))?,
            kept_local: count("local")?,
            skipped: count("skipped")?,
            errors: count("error")?,
            first_index_done: self.first_index_done()?,
            spent,
            cap,
            spent_total: self.kv_i64("total_micro")?,
        })
    }

    // ---------- describing ----------

    /// Describe one file. Returns Err only for problems that should stop the worker for a while.
    fn describe_one(&self, claude: &Claude, path: &str, media: Media) -> Result<(), CallError> {
        let p = Path::new(path);
        let fail = |e: anyhow::Error| CallError::Retry(e.to_string());
        if !p.exists() {
            self.set_status(path, "skipped", Some("no longer there")).map_err(fail)?;
            return Ok(());
        }
        if let Some(r) = self.kept_local_by_folder(path).map_err(fail)? {
            self.set_status(path, "local", Some(&r)).map_err(fail)?;
            return Ok(());
        }
        let mtime = std::fs::metadata(p).ok().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0);
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let folder = p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut facts = extract::PhotoFacts::default();
        let result = match media {
            Media::Document => {
                let text = match extract::document_text(p) {
                    Ok(t) => t,
                    Err(e) => {
                        self.set_status(path, "skipped", Some(&e.to_string())).map_err(fail)?;
                        return Ok(());
                    }
                };
                if let Some(r) = extract::private_reason(p, &text) {
                    self.set_status(path, "local", Some(&format!("kept on this PC: {r}"))).map_err(fail)?;
                    return Ok(());
                }
                if text.split_whitespace().count() < 5 {
                    self.set_status(path, "skipped", Some("no readable text")).map_err(fail)?;
                    return Ok(());
                }
                let context = format!("name \"{name}\", folder \"{folder}\", last changed {}", date_text(mtime));
                claude.describe_document(&text, &context)
            }
            Media::Photo | Media::Video => {
                if let Some(r) = extract::private_reason(p, "") {
                    self.set_status(path, "local", Some(&format!("kept on this PC: {r}"))).map_err(fail)?;
                    return Ok(());
                }
                if media == Media::Photo {
                    facts = extract::photo_facts(p);
                }
                let jpeg = match extract::picture(p, media) {
                    Ok(Some(j)) => j,
                    Ok(None) => {
                        self.set_status(path, "skipped", Some("too small to describe")).map_err(fail)?;
                        return Ok(());
                    }
                    Err(e) => {
                        self.set_status(path, "skipped", Some(&e.to_string())).map_err(fail)?;
                        return Ok(());
                    }
                };
                let when = facts.taken.clone().unwrap_or_else(|| date_text(mtime));
                let context = format!("{} named \"{name}\" in folder \"{folder}\", from {when}", if media == Media::Video { "a frame from a video" } else { "a photo" });
                claude.describe_picture(&jpeg, &context)
            }
        };
        match result {
            Ok((d, spend)) => {
                let file_time = facts.taken.as_deref().and_then(exif_time).unwrap_or(mtime);
                self.record_spend(spend).map_err(fail)?;
                self.store_description(path, media, &d, &facts, file_time, spend).map_err(fail)?;
                self.set_status(path, "done", None).map_err(fail)?;
                Ok(())
            }
            Err(CallError::Skip(why)) => {
                self.set_status(path, "skipped", Some(&why)).map_err(fail)?;
                Ok(())
            }
            Err(CallError::Retry(why)) => {
                let tries: i64 = self.conn.query_row("UPDATE describe_queue SET tries = tries + 1 WHERE path = ?1 RETURNING tries", [path], |r| r.get(0)).unwrap_or(3);
                if tries >= 3 {
                    self.set_status(path, "error", Some(&why)).map_err(fail)?;
                }
                Err(CallError::Retry(why))
            }
            Err(e) => Err(e),
        }
    }

    /// Write stories for memories that have none, or that grew since. Returns how many were written.
    fn write_stories(&self, claude: &Claude, limit: usize) -> Result<usize, CallError> {
        let fail = |e: anyhow::Error| CallError::Retry(e.to_string());
        let mems = self.memories().map_err(fail)?;
        let mut done = 0;
        for m in mems {
            if done >= limit || self.money_left().map_err(fail)? < MIN_LEFT {
                break;
            }
            let have: Option<i64> = self.conn.query_row("SELECT files FROM memory_stories WHERE memory_id = ?1", [m.id], |r| r.get(0)).optional().map_err(|e| fail(e.into()))?;
            if have.is_some_and(|n| n as usize == m.files.len()) {
                continue;
            }
            let mut facts = format!(
                "Name: {}\nDates: {} to {}\nTime spent: {} minutes over {} day(s), {} minutes late at night\nFiles:\n",
                m.label, date_text(m.start), date_text(m.end), m.active_minutes, m.days, m.late_minutes
            );
            for f in m.files.iter().take(25) {
                let name = Path::new(f).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let summary: Option<String> = self.conn.query_row("SELECT summary FROM descriptions WHERE path = ?1", [f], |r| r.get(0)).optional().map_err(|e| fail(e.into()))?;
                facts.push_str(&format!("- {name}{}\n", summary.map(|s| format!(": {s}")).unwrap_or_default()));
            }
            let (story, spend) = claude.memory_story(&facts)?;
            self.record_spend(spend).map_err(fail)?;
            self.conn
                .execute("INSERT OR REPLACE INTO memory_stories (memory_id, story, files) VALUES (?1, ?2, ?3)", params![m.id, story, m.files.len() as i64])
                .map_err(|e| fail(e.into()))?;
            // Make the story's words findable by the local "remember when" search too.
            let extra: String = story
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() >= 4)
                .map(str::to_lowercase)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(" ");
            self.conn.execute("UPDATE memories SET words = words || ' ' || ?2 WHERE id = ?1", params![m.id, extra]).map_err(|e| fail(e.into()))?;
            done += 1;
        }
        Ok(done)
    }

    pub fn memory_story(&self, id: i64) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT story FROM memory_stories WHERE memory_id = ?1", [id], |r| r.get(0)).optional()?)
    }

    // ---------- smart search ----------

    /// Search by memory. With the cloud on, Claude plans the search and reranks the best
    /// candidates; otherwise the local search runs alone.
    pub fn smart_search(&self, question: &str) -> Result<SmartAnswer> {
        let tz = Local;
        let now_ts = now();
        let local = crate::recall::parse(question, now_ts, &tz);
        let mut answer = SmartAnswer::default();
        let claude = match (self.cloud_on()?, self.api_key()?) {
            (true, Some(k)) if self.money_left()? >= MIN_LEFT && self.kv("key_problem")?.is_none() => Some(Claude::new(&k)),
            (true, Some(_)) if self.kv("key_problem")?.is_some() => {
                answer.local_only = Some("The API key needs attention in Settings, so this search ran on your PC only.".into());
                None
            }
            (true, Some(_)) => {
                answer.local_only = Some("This month's AI budget is used up, so this search ran on your PC only. You can allow more in Settings.".into());
                None
            }
            _ => None,
        };

        // 1. Understand the question.
        let today = Local::now().format("%Y-%m-%d (%A)").to_string();
        let mut range = local.range;
        let mut kinds: Vec<String> = Vec::new();
        let mut words: Vec<String> = local.words.clone();
        if let Some(l) = &local.range_label {
            answer.understood.push(l.clone());
        }
        if let Some(c) = &claude {
            let hint = match (&local.range_label, local.range) {
                (Some(l), Some((a, b))) => format!("A date parser read the time in the question as \"{l}\": {} to {}.", date_text(a), date_text(b - 1)),
                _ => String::new(),
            };
            match c.plan(question, &today, &hint) {
                Ok((plan, spend)) => {
                    answer.micro_usd += spend.micro_usd;
                    self.record_spend(spend)?;
                    if range.is_none() {
                        if let (Some(a), Some(b)) = (parse_day(&plan.date_from), parse_day(&plan.date_to)) {
                            range = Some((a, b + 86_400));
                            answer.understood.push(format!("{} to {}", plan.date_from, plan.date_to));
                        }
                    }
                    kinds = plan.kinds.into_iter().filter(|k| k != "any").collect();
                    for w in plan.keywords.iter().chain(plan.details.iter()) {
                        for t in w.split_whitespace() {
                            let t = t.to_lowercase();
                            if t.len() >= 3 && !words.contains(&t) {
                                words.push(t);
                            }
                        }
                    }
                    if !plan.about.is_empty() {
                        answer.understood.insert(0, plan.about);
                    }
                }
                Err(CallError::BadKey(m)) => {
                    self.set_kv("key_problem", Some(&m))?;
                }
                Err(_) => {}
            }
        }
        if kinds.is_empty() {
            kinds = local.kinds.iter().filter_map(|k| match *k {
                "video" => Some("video".to_string()),
                "photos" => Some("photo".to_string()),
                "presentation" | "spreadsheet" | "writing" => Some("document".to_string()),
                _ => None,
            }).collect();
        }
        if !kinds.is_empty() {
            answer.understood.push(kinds.join(" or "));
        }
        if local.rush {
            answer.understood.push("busy, on a deadline".into());
        }

        // 2. Candidates from this PC: descriptions, memories, file names.
        let mut cands: Vec<(Found, f64)> = Vec::new();
        if !words.is_empty() {
            let q = words.iter().map(|w| format!("\"{}\"*", w.replace('"', ""))).collect::<Vec<_>>().join(" OR ");
            let mut s = self.conn.prepare(
                "SELECT d.path, d.media, d.summary, d.file_time, bm25(descriptions_fts) AS rank FROM descriptions_fts
                 JOIN descriptions d ON d.id = descriptions_fts.rowid WHERE descriptions_fts MATCH ?1 ORDER BY rank LIMIT 200",
            )?;
            let rows: Vec<(String, String, String, i64, f64)> =
                s.query_map([&q], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?.collect::<Result<_, _>>()?;
            for (path, media, summary, t, rank) in rows {
                if !kinds.is_empty() && !kinds.contains(&media) {
                    continue;
                }
                if let Some((a, b)) = range {
                    if !(t >= a && t < b) && !self.used_between(&path, a, b)? {
                        continue;
                    }
                }
                cands.push((file_found(&path, t, summary, String::new()), -rank));
            }
        }
        let (_, mems) = self.recall_in(question, now_ts, &tz, 10)?;
        for r in mems {
            let story = self.memory_story(r.memory.id)?.unwrap_or_default();
            let summary = if story.is_empty() { format!("{} files", r.memory.files.len()) } else { story };
            cands.push((
                Found { path: None, memory_id: Some(r.memory.id), title: r.memory.label.clone(), date: r.memory.end, summary, why: r.why.join(" · ") },
                r.score * 2.0,
            ));
        }
        if !local.words.is_empty() {
            for h in self.search(&local.words.join(" "), 20)? {
                if cands.iter().any(|(c, _)| c.path.as_deref() == Some(h.path.as_str())) {
                    continue;
                }
                if let Some((a, b)) = range {
                    if !self.used_between(&h.path, a, b)? {
                        continue;
                    }
                }
                cands.push((file_found(&h.path, h.last_seen, String::new(), "name matches".into()), 1.0));
            }
        }
        cands.sort_by(|a, b| b.1.total_cmp(&a.1));
        cands.truncate(40);

        // 3. Rerank with Claude.
        if let (Some(c), false) = (&claude, cands.is_empty()) {
            let list: Vec<(usize, String)> = cands
                .iter()
                .enumerate()
                .map(|(i, (f, _))| {
                    let summary: String = f.summary.chars().take(300).collect();
                    let kind = if f.memory_id.is_some() { "[piece of work] " } else { "" };
                    (i, format!("{kind}{} · {} · {}", f.title, date_text(f.date), summary))
                })
                .collect();
            match c.rerank(question, &list) {
                Ok((order, spend)) => {
                    answer.micro_usd += spend.micro_usd;
                    self.record_spend(spend)?;
                    answer.results = order
                        .into_iter()
                        .map(|(i, why)| {
                            let mut f = cands[i].0.clone();
                            f.why = if f.why.is_empty() { why } else { format!("{why} · {}", f.why) };
                            f
                        })
                        .collect();
                    return Ok(answer);
                }
                Err(CallError::BadKey(m)) => self.set_kv("key_problem", Some(&m))?,
                Err(_) => {}
            }
        }
        answer.results = cands.into_iter().take(10).map(|(mut f, _)| {
            if f.why.is_empty() {
                f.why = "description matches".into();
            }
            f
        }).collect();
        Ok(answer)
    }

    fn used_between(&self, path: &str, a: i64, b: i64) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM events e JOIN files f ON f.id = e.file_id WHERE f.path = ?1 AND e.ts >= ?2 AND e.ts < ?3 LIMIT 1",
                params![path, a, b],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// The description of one file, if it has one.
    pub fn description(&self, path: &str) -> Result<Option<Description>> {
        let d: Option<String> = self.conn.query_row("SELECT details FROM descriptions WHERE path = ?1", [path], |r| r.get(0)).optional()?;
        Ok(d.and_then(|j| serde_json::from_str(&j).ok()))
    }
}

fn file_found(path: &str, t: i64, summary: String, why: String) -> Found {
    let title = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Found { path: Some(path.to_string()), memory_id: None, title, date: t, summary, why }
}

fn date_text(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

fn parse_day(s: &str) -> Option<i64> {
    let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()?;
    Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest().map(|t| t.timestamp())
}

/// "2024:11:01 18:42:10" from a camera, as unix time.
fn exif_time(s: &str) -> Option<i64> {
    let t = chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y:%m:%d %H:%M:%S"))
        .ok()?;
    Local.from_local_datetime(&t).earliest().map(|d: DateTime<Local>| d.timestamp())
}

// ---------- the background worker ----------

/// Shared between the app and the worker thread.
#[derive(Default)]
pub struct Worker {
    pub stop: AtomicBool,
    /// "Index now": describe even while the PC is in use.
    pub force: AtomicBool,
    /// Rescan the folders on the next round.
    pub rescan: AtomicBool,
    pub state: Mutex<String>,
}

impl Worker {
    fn set(&self, s: &str) {
        if let Ok(mut st) = self.state.lock() {
            if *st != s {
                *st = s.to_string();
            }
        }
    }

    pub fn state(&self) -> String {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

/// Seconds since the last keyboard or mouse input.
#[cfg(windows)]
pub fn idle_secs() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: info is a valid LASTINPUTINFO with cbSize set.
    if unsafe { GetLastInputInfo(&mut info) } == 0 {
        return u64::MAX;
    }
    let now = unsafe { GetTickCount() };
    (now.wrapping_sub(info.dwTime) / 1000) as u64
}

#[cfg(not(windows))]
pub fn idle_secs() -> u64 {
    u64::MAX
}

/// Runs until `worker.stop`: queues files and describes them while the PC is idle, within the caps.
pub fn run_worker(db: &Db, folders: &[PathBuf], worker: &Worker, paused: &AtomicBool) {
    let mut last_recent = now() - 3600;
    let pause = |secs: u64| {
        for _ in 0..secs {
            if worker.stop.load(Ordering::Relaxed) || worker.force.load(Ordering::Relaxed) && secs > 5 {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    };
    while !worker.stop.load(Ordering::Relaxed) {
        let step = (|| -> Result<u64> {
            if !db.cloud_on()? {
                worker.set("off");
                return Ok(10);
            }
            let Some(key) = db.api_key()? else {
                worker.set("no_key");
                return Ok(10);
            };
            if db.kv("key_problem")?.is_some() {
                worker.set("key_problem");
                return Ok(30);
            }
            if paused.load(Ordering::Relaxed) {
                worker.set("paused");
                return Ok(10);
            }
            if db.kv("library_scanned")?.is_none() || worker.rescan.swap(false, Ordering::Relaxed) {
                worker.set("scanning");
                db.scan_library(folders, &worker.stop)?;
                if worker.stop.load(Ordering::Relaxed) {
                    return Ok(0);
                }
                db.set_kv("library_scanned", Some(&now().to_string()))?;
            }
            if now() - last_recent > 300 {
                db.queue_recent(last_recent - 60)?;
                last_recent = now();
            }
            let force = worker.force.load(Ordering::Relaxed);
            if !force && idle_secs() < IDLE_SECS {
                worker.set("waiting_idle");
                return Ok(20);
            }
            if db.money_left()? < MIN_LEFT {
                worker.set("cap");
                worker.force.store(false, Ordering::Relaxed);
                return Ok(30);
            }
            let claude = Claude::new(&key);
            let Some((path, media)) = db.next_pending()? else {
                if !db.first_index_done()? {
                    db.set_kv("first_index_done", Some(&now().to_string()))?;
                }
                match db.write_stories(&claude, 5) {
                    Ok(0) => {
                        worker.set("done");
                        worker.force.store(false, Ordering::Relaxed);
                        return Ok(60);
                    }
                    Ok(_) => return Ok(1),
                    Err(CallError::BadKey(m)) => {
                        db.set_kv("key_problem", Some(&m))?;
                        return Ok(30);
                    }
                    Err(_) => return Ok(60),
                }
            };
            worker.set("describing");
            match db.describe_one(&claude, &path, media) {
                Ok(()) => Ok(1),
                Err(CallError::BadKey(m)) => {
                    db.set_kv("key_problem", Some(&m))?;
                    Ok(30)
                }
                Err(e) => {
                    eprintln!("describing {path}: {e}");
                    Ok(60)
                }
            }
        })();
        match step {
            Ok(secs) => pause(secs),
            Err(e) => {
                eprintln!("cloud worker: {e:#}");
                pause(60);
            }
        }
    }
}

/// Check a key with a tiny call; on success clear any key problem.
pub fn check_key(db: &Db, key: &str) -> Result<()> {
    let c = Claude::new(key);
    match c.check_key() {
        Ok(spend) => {
            db.record_spend(spend)?;
            db.set_kv("key_problem", None)?;
            Ok(())
        }
        Err(e) => anyhow::bail!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::Person;

    #[test]
    fn caps_switch_from_first_index_to_monthly() {
        let db = Db::in_memory().unwrap();
        assert_eq!(db.spend_and_cap().unwrap(), (0, FIRST_INDEX_CAP));
        db.record_spend(Spend { micro_usd: 4_999_000, ..Default::default() }).unwrap();
        assert!(db.money_left().unwrap() < MIN_LEFT);
        db.allow_more().unwrap();
        assert_eq!(db.money_left().unwrap(), 1_001_000);
        db.set_kv("first_index_done", Some("1")).unwrap();
        assert_eq!(db.spend_and_cap().unwrap(), (0, MONTHLY_CAP));
        db.record_spend(Spend { micro_usd: 300, ..Default::default() }).unwrap();
        assert_eq!(db.spend_and_cap().unwrap(), (300, MONTHLY_CAP));
        assert_eq!(db.kv_i64("total_micro").unwrap(), 4_999_300);
    }

    #[test]
    fn queue_skips_screenshots_copies_and_private_folders() {
        let dir = crate::test_dir("cloud-queue");
        let photos = dir.join("Trip");
        let private = dir.join("Bank");
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::create_dir_all(&private).unwrap();
        let px = image::RgbImage::from_fn(600, 400, |x, y| image::Rgb([((x * 7919 + y * 104729) % 251) as u8, ((x * y + 13) % 241) as u8, ((x ^ y) % 233) as u8]));
        px.save(photos.join("beach.png")).unwrap();
        std::fs::copy(photos.join("beach.png"), photos.join("beach copy.png")).unwrap();
        px.save(photos.join("Screenshot 2026-10-01.png")).unwrap();
        px.save(private.join("cheque.png")).unwrap();
        std::fs::write(photos.join("notes.txt"), "packing list for the trip").unwrap();

        let db = Db::in_memory().unwrap();
        db.add_local_only_folder(&private.to_string_lossy()).unwrap();
        let stop = AtomicBool::new(false);
        assert_eq!(db.scan_library(&[dir.clone()], &stop).unwrap(), 2, "one photo and the notes");
        let status = |name: &str| -> (String, Option<String>) {
            let p = dir.join(name).to_string_lossy().into_owned();
            db.conn.query_row("SELECT status, reason FROM describe_queue WHERE path = ?1", [p], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        assert_eq!(status("Trip/Screenshot 2026-10-01.png").0, "skipped");
        assert_eq!(status("Bank/cheque.png").0, "local");
        let (a, b) = (status("Trip/beach.png"), status("Trip/beach copy.png"));
        assert!([a.0.as_str(), b.0.as_str()].contains(&"pending") && [a.0.as_str(), b.0.as_str()].contains(&"skipped"), "{a:?} {b:?}");
        // Unchanged files are not queued again.
        assert_eq!(db.scan_library(&[dir.clone()], &stop).unwrap(), 0);
    }

    #[test]
    fn local_search_finds_descriptions_by_details() {
        let db = Db::in_memory().unwrap();
        let d = Description {
            summary: "A young woman in a pink top holds a yoga pose on a rooftop at sunrise".into(),
            people: vec![Person { looks: "young woman".into(), wearing: vec!["pink top".into()], doing: "yoga".into() }],
            colours: vec!["pink".into()],
            ..Default::default()
        };
        db.store_description("/v/morning_03.mp4", Media::Video, &d, &Default::default(), 1_700_000_000, Spend::default()).unwrap();
        let other = Description { summary: "A group photo of students at a convocation".into(), ..Default::default() };
        db.store_description("/p/convocation.jpg", Media::Photo, &other, &Default::default(), 1_700_000_000, Spend::default()).unwrap();

        let a = db.smart_search("video of a girl doing yoga in pink").unwrap();
        assert_eq!(a.results.first().and_then(|f| f.path.as_deref()), Some("/v/morning_03.mp4"), "{a:?}");
        assert!(a.results.iter().all(|f| f.path.as_deref() != Some("/p/convocation.jpg")));
        assert!(a.local_only.is_none());

        db.forget_cloud_descriptions().unwrap();
        assert!(db.smart_search("yoga in pink").unwrap().results.is_empty());
    }

    #[test]
    fn camera_dates_are_read() {
        assert!(exif_time("2024:11:01 18:42:10").is_some());
        assert!(exif_time("2024-11-01 18:42:10").is_some());
        assert!(exif_time("garbage").is_none());
    }
}
