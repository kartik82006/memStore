//! Memories: activities saved for good, so they can be found later by what they were like
//! ("the edit I did before Diwali", "that tight-deadline project").
//!
//! Once a day has ended, its activities are saved. An activity that shares files with a memory
//! from the last two weeks continues that memory, so a project worked on over several days
//! becomes one memory with its full date span. For each memory we keep how much work went into
//! it (active minutes, days, the busiest day) and how much of it happened late at night.

use crate::activities::{Work, name_words};
use crate::db::Db;
use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate, TimeZone, Timelike};
use rusqlite::{OptionalExtension, params};
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS memories (
    id        INTEGER PRIMARY KEY,
    label     TEXT NOT NULL,
    work      TEXT NOT NULL,
    topic     TEXT NOT NULL,
    start     INTEGER NOT NULL,
    end       INTEGER NOT NULL,
    active_minutes INTEGER NOT NULL DEFAULT 0,  -- 5-minute slots with any use, in minutes
    late_minutes   INTEGER NOT NULL DEFAULT 0,  -- of those, between 22:00 and 05:00
    days           INTEGER NOT NULL DEFAULT 0,
    busiest_day_minutes INTEGER NOT NULL DEFAULT 0,
    words     TEXT NOT NULL DEFAULT '',         -- topic, file-name and folder words, for search
    renamed   INTEGER NOT NULL DEFAULT 0          -- 1 once the user named it
);
CREATE INDEX IF NOT EXISTS memories_end ON memories(end);
CREATE TABLE IF NOT EXISTS memory_files (
    memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    path      TEXT NOT NULL,
    PRIMARY KEY (memory_id, path)
);
CREATE TABLE IF NOT EXISTS memory_days (
    memory_id INTEGER NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    day_start INTEGER NOT NULL,
    PRIMARY KEY (memory_id, day_start)
);
"#;

/// Meta key: start of the first day not yet saved as memories.
const NEXT_DAY_KEY: &str = "memories_next_day";
/// On the first run, look back this far (the collector's own first-run lookback is 14 days).
const FIRST_RUN_DAYS: i64 = 30;
/// An activity continues a memory whose last day is at most this long ago.
const CONTINUE_DAYS: i64 = 14;
const SLOT_SECS: i64 = 5 * 60;

#[derive(Debug, Clone)]
pub struct Memory {
    pub id: i64,
    pub label: String,
    pub work: String,
    pub topic: String,
    pub start: i64,
    pub end: i64,
    pub active_minutes: i64,
    pub late_minutes: i64,
    pub days: i64,
    pub busiest_day_minutes: i64,
    pub words: String,
    pub files: Vec<String>,
}

impl Memory {
    /// Minutes of work per active day.
    pub fn intensity(&self) -> f64 {
        self.active_minutes as f64 / self.days.max(1) as f64
    }
}

/// Local midnight at the start of `date`.
fn midnight<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> i64 {
    tz.from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .map(|d| d.timestamp())
        .unwrap_or_else(|| date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp())
}

fn local_date<Tz: TimeZone>(tz: &Tz, ts: i64) -> NaiveDate {
    DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(tz).date_naive()
}

fn is_late<Tz: TimeZone>(tz: &Tz, ts: i64) -> bool {
    let h = DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(tz).hour();
    h >= 22 || h < 5
}

/// Search words for a memory: its topic, its file names and the folders they are in.
fn words_for(topic: &str, files: &[String]) -> String {
    let mut w: BTreeSet<String> = topic.split_whitespace().map(str::to_lowercase).collect();
    for f in files {
        w.extend(name_words(f));
        if let Some(dir) = Path::new(f).parent().and_then(|p| p.file_name()) {
            w.extend(name_words(&format!("{}.x", dir.to_string_lossy())));
        }
    }
    w.into_iter().collect::<Vec<_>>().join(" ")
}

impl Db {
    /// Save every finished day's activities that is not saved yet. Call it now and then;
    /// it only does work after midnight.
    pub fn remember_finished_days(&self) -> Result<usize> {
        self.remember_finished_days_in(&Local, chrono::Utc::now().timestamp())
    }

    pub fn remember_finished_days_in<Tz: TimeZone>(&self, tz: &Tz, now: i64) -> Result<usize> {
        let today = local_date(tz, now);
        let mut day = match self.get_meta(NEXT_DAY_KEY)? {
            Some(ts) => local_date(tz, ts),
            None => today - chrono::Duration::days(FIRST_RUN_DAYS),
        };
        let mut saved = 0;
        while day < today {
            let next = day.succ_opt().unwrap();
            saved += self.remember_day(tz, midnight(tz, day), midnight(tz, next))?;
            self.set_meta(NEXT_DAY_KEY, midnight(tz, next))?;
            day = next;
        }
        Ok(saved)
    }

    /// Save one day's activities as memories, extending earlier memories where they continue.
    fn remember_day<Tz: TimeZone>(&self, tz: &Tz, start: i64, end: i64) -> Result<usize> {
        let acts = self.activities(start, end, dirs::home_dir().as_deref())?;
        for a in &acts {
            let paths: Vec<String> = a.files.iter().map(|f| f.path.clone()).collect();
            let id = match self.continuing_memory(&paths, start)? {
                Some(id) => id,
                None => {
                    self.conn.execute(
                        "INSERT INTO memories (label, work, topic, start, end) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![a.label, a.work.as_str(), a.topic, a.start, a.end],
                    )?;
                    self.conn.last_insert_rowid()
                }
            };
            for p in &paths {
                self.conn.execute("INSERT OR IGNORE INTO memory_files (memory_id, path) VALUES (?1, ?2)", params![id, p])?;
            }
            self.conn.execute("INSERT OR IGNORE INTO memory_days (memory_id, day_start) VALUES (?1, ?2)", params![id, start])?;
            self.refresh_memory(tz, id)?;
        }
        Ok(acts.len())
    }

    /// A memory from the last two weeks that shares at least half of the smaller file set.
    fn continuing_memory(&self, paths: &[String], day_start: i64) -> Result<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT m.id, count(*) FROM memories m JOIN memory_files mf ON mf.memory_id = m.id
             WHERE m.end >= ?1 AND mf.path = ?2 GROUP BY m.id",
        )?;
        let since = day_start - CONTINUE_DAYS * 86_400;
        let mut shared: std::collections::HashMap<i64, usize> = Default::default();
        for p in paths {
            for row in stmt.query_map(params![since, p], |r| r.get::<_, i64>(0))? {
                *shared.entry(row?).or_default() += 1;
            }
        }
        let mut best: Option<(i64, f64)> = None;
        for (id, n) in shared {
            let size: i64 = self.conn.query_row("SELECT count(*) FROM memory_files WHERE memory_id = ?1", [id], |r| r.get(0))?;
            let overlap = n as f64 / (paths.len().min(size as usize).max(1)) as f64;
            if overlap >= 0.5 && best.is_none_or(|(_, o)| overlap > o) {
                best = Some((id, overlap));
            }
        }
        Ok(best.map(|(id, _)| id))
    }

    /// Recompute a memory's span, effort, name and search words from its files and days.
    fn refresh_memory<Tz: TimeZone>(&self, tz: &Tz, id: i64) -> Result<()> {
        let files: Vec<String> = {
            let mut s = self.conn.prepare("SELECT path FROM memory_files WHERE memory_id = ?1 ORDER BY path")?;
            s.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        let days: Vec<i64> = {
            let mut s = self.conn.prepare("SELECT day_start FROM memory_days WHERE memory_id = ?1 ORDER BY day_start")?;
            s.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        let mut slots: HashSet<i64> = HashSet::new();
        let mut ts_all: Vec<i64> = Vec::new();
        {
            let mut s = self.conn.prepare(
                "SELECT e.ts FROM events e JOIN files f ON f.id = e.file_id
                 WHERE f.path = ?1 AND e.ts >= ?2 AND e.ts < ?3 AND e.kind != 'deleted'",
            )?;
            for p in &files {
                for &d in &days {
                    for ts in s.query_map(params![p, d, d + 86_400 + 3_600], |r| r.get::<_, i64>(0))? {
                        let ts = ts?;
                        slots.insert(ts / SLOT_SECS);
                        ts_all.push(ts);
                    }
                }
            }
        }
        let active = slots.len() as i64 * 5;
        let late = slots.iter().filter(|s| is_late(tz, *s * SLOT_SECS)).count() as i64 * 5;
        let mut per_day: std::collections::HashMap<NaiveDate, i64> = Default::default();
        for s in &slots {
            *per_day.entry(local_date(tz, s * SLOT_SECS)).or_default() += 5;
        }
        let busiest = per_day.values().copied().max().unwrap_or(0);
        let (start, end) = (ts_all.iter().min().copied(), ts_all.iter().max().copied());

        // Rename from all its files as the memory grows, unless the user named it.
        let (mut topic, work, renamed): (String, String, bool) = self.conn.query_row(
            "SELECT topic, work, renamed FROM memories WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        if !renamed {
            if let Some(t) = crate::activities::shared_topic(&files, dirs::home_dir().as_deref()) {
                topic = t;
                let label = format!("{} · {}", work_label(&work), topic);
                self.conn.execute("UPDATE memories SET topic = ?2, label = ?3 WHERE id = ?1", params![id, topic, label])?;
            }
        }
        let words = words_for(&topic, &files);
        self.conn.execute(
            "UPDATE memories SET start = coalesce(?2, start), end = coalesce(?3, end), active_minutes = ?4,
                late_minutes = ?5, days = ?6, busiest_day_minutes = ?7, words = ?8 WHERE id = ?1",
            params![id, start, end, active, late, per_day.len() as i64, busiest, words],
        )?;
        Ok(())
    }

    /// All memories, newest first.
    pub fn memories(&self) -> Result<Vec<Memory>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, label, work, topic, start, end, active_minutes, late_minutes, days, busiest_day_minutes, words
             FROM memories ORDER BY end DESC",
        )?;
        let mut out: Vec<Memory> = stmt
            .query_map([], |r| {
                Ok(Memory {
                    id: r.get(0)?,
                    label: r.get(1)?,
                    work: r.get(2)?,
                    topic: r.get(3)?,
                    start: r.get(4)?,
                    end: r.get(5)?,
                    active_minutes: r.get(6)?,
                    late_minutes: r.get(7)?,
                    days: r.get(8)?,
                    busiest_day_minutes: r.get(9)?,
                    words: r.get(10)?,
                    files: Vec::new(),
                })
            })?
            .collect::<Result<_, _>>()?;
        let mut files = self.conn.prepare("SELECT path FROM memory_files WHERE memory_id = ?1 ORDER BY path")?;
        for m in &mut out {
            m.files = files.query_map([m.id], |r| r.get(0))?.collect::<Result<_, _>>()?;
        }
        Ok(out)
    }

    /// Rename a memory (the user knows better).
    pub fn rename_memory(&self, id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "Give it a name.");
        let work: Option<String> =
            self.conn.query_row("SELECT work FROM memories WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        let work = work.ok_or_else(|| anyhow::anyhow!("That memory no longer exists."))?;
        let label = format!("{} · {}", work_label(&work), name);
        self.conn.execute("UPDATE memories SET topic = ?2, label = ?3, renamed = 1 WHERE id = ?1", params![id, name, label])?;
        let files: Vec<String> = {
            let mut s = self.conn.prepare("SELECT path FROM memory_files WHERE memory_id = ?1")?;
            s.query_map([id], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        self.conn.execute("UPDATE memories SET words = ?2 WHERE id = ?1", params![id, words_for(name, &files)])?;
        Ok(())
    }

    pub fn forget_memory(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM memory_files WHERE memory_id = ?1", [id])?;
        self.conn.execute("DELETE FROM memory_days WHERE memory_id = ?1", [id])?;
        self.conn.execute("DELETE FROM memories WHERE id = ?1", [id])?;
        Ok(())
    }
}

pub fn work_label(work: &str) -> &'static str {
    [Work::Video, Work::Audio, Work::Photos, Work::Presentation, Work::Spreadsheet, Work::Writing, Work::Files]
        .into_iter()
        .find(|w| w.as_str() == work)
        .unwrap_or(Work::Files)
        .label()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Kind;
    use chrono::FixedOffset;

    fn ist() -> FixedOffset {
        FixedOffset::east_opt(5 * 3600 + 1800).unwrap()
    }

    /// Unix time for a local IST date and hour.
    pub(crate) fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        ist().with_ymd_and_hms(y, m, d, h, min, 0).unwrap().timestamp()
    }

    fn rec(db: &Db, path: &str, ts: i64) {
        db.record(path, Kind::Modified, "watcher", ts).unwrap();
    }

    #[test]
    fn days_are_saved_once_and_multi_day_work_is_one_memory() {
        let db = Db::in_memory().unwrap();
        let home = dirs::home_dir().unwrap().to_string_lossy().into_owned();
        let p = |s: &str| format!("{home}/Videos/Alumni Meet/{s}");
        // Two evenings of editing, the second running late before a deadline.
        for (d, h) in [(1, 19), (2, 22)] {
            for i in 0..12 {
                rec(&db, &p("alumni meet.prproj"), at(2026, 10, d, h, i * 5));
                rec(&db, &p("alumni pics.zip"), at(2026, 10, d, h, i * 5 + 1));
            }
        }
        rec(&db, &p("alumni meet final.mp4"), at(2026, 10, 2, 23, 30));

        let saved = db.remember_finished_days_in(&ist(), at(2026, 10, 3, 9, 0)).unwrap();
        assert_eq!(saved, 2);
        let m = db.memories().unwrap();
        assert_eq!(m.len(), 1, "{m:#?}");
        let m = &m[0];
        assert_eq!(m.days, 2);
        assert_eq!(m.files.len(), 3);
        assert_eq!(m.topic, "Alumni meet");
        assert!(m.late_minutes >= 60, "{m:#?}");
        assert!(m.active_minutes >= 120, "{m:#?}");
        assert!(m.words.contains("alumni") && m.words.contains("pics"), "{}", m.words);
        assert_eq!(m.start, at(2026, 10, 1, 19, 0));

        // Running again saves nothing new.
        assert_eq!(db.remember_finished_days_in(&ist(), at(2026, 10, 3, 18, 0)).unwrap(), 0);
        assert_eq!(db.memories().unwrap().len(), 1);
    }

    #[test]
    fn rename_and_forget() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/x/Club/minutes.docx", at(2026, 10, 1, 10, 0));
        rec(&db, "/x/Club/members.xlsx", at(2026, 10, 1, 10, 2));
        db.remember_finished_days_in(&ist(), at(2026, 10, 2, 9, 0)).unwrap();
        let id = db.memories().unwrap()[0].id;
        db.rename_memory(id, "Rotaract handover").unwrap();
        let m = &db.memories().unwrap()[0];
        assert_eq!(m.label, "Spreadsheet work · Rotaract handover");
        assert!(m.words.contains("rotaract"));
        db.forget_memory(id).unwrap();
        assert!(db.memories().unwrap().is_empty());
    }
}
