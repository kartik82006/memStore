//! Events and groups: described photos, videos and documents grouped by the event they show
//! ("NSS alumni meet") or who they were made for ("Rotaract club"). Built on the PC from the
//! descriptions; one small Claude call merges different spellings of the same name.

use crate::claude::{CallError, Claude};
use crate::db::Db;
use crate::extract::Media;
use anyhow::Result;
use rusqlite::params;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

pub(crate) const SCHEMA: &str = r#"
-- A name seen in descriptions (normalised) and the group it belongs to, as Claude merged them.
CREATE TABLE IF NOT EXISTS label_groups (label TEXT PRIMARY KEY, grp TEXT NOT NULL);
"#;

/// Names sent to Claude per call.
const NAMES_PER_CALL: usize = 150;

/// Words that say nothing about an event or who a file was for.
const VAGUE: &[&str] = &[
    "", "none", "unknown", "n a", "na", "not applicable", "not specified", "unclear", "personal", "self", "myself", "me", "general",
    "everyday", "everyday life", "daily life", "casual", "candid", "family", "friends", "work", "school", "home", "travel",
    "social media", "personal use", "the photographer", "the owner", "the user", "no event", "no specific event",
];

#[derive(Debug, Clone, Serialize)]
pub struct GroupFile {
    pub path: String,
    pub media: String,
    pub date: i64,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub name: String,
    /// "event" when files show an event, "for" when they were made for someone.
    pub kind: String,
    pub start: i64,
    pub end: i64,
    pub photos: usize,
    pub videos: usize,
    pub documents: usize,
    pub files: Vec<GroupFile>,
}

/// Lowercase, without punctuation, a leading "the" or extra spaces.
pub(crate) fn normalise(label: &str) -> String {
    let s: String = label.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.strip_prefix("the ").unwrap_or(&s).to_string();
    if VAGUE.contains(&s.as_str()) || s.len() < 3 { String::new() } else { s }
}

impl Db {
    /// (event, for_whom) for every description, with its file.
    fn labelled(&self) -> Result<Vec<(GroupFile, String, String)>> {
        let mut s = self.conn.prepare("SELECT path, media, file_time, summary, details FROM descriptions")?;
        let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (path, media, date, summary, details) = row?;
            let d: serde_json::Value = serde_json::from_str(&details).unwrap_or_default();
            let event = d["event"].as_str().unwrap_or("").trim().to_string();
            let for_whom = d["for_whom"].as_str().unwrap_or("").trim().to_string();
            out.push((GroupFile { path, media, date, summary }, event, for_whom));
        }
        Ok(out)
    }

    fn group_names(&self) -> Result<HashMap<String, String>> {
        let mut s = self.conn.prepare("SELECT label, grp FROM label_groups")?;
        Ok(s.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?)
    }

    /// Events and groups with at least two files, most recent first.
    pub fn groups(&self) -> Result<Vec<Group>> {
        let names = self.group_names()?;
        // key -> (kind, files by path, spellings seen)
        let mut by: BTreeMap<String, (String, BTreeMap<String, GroupFile>, HashMap<String, usize>)> = BTreeMap::new();
        for (file, event, for_whom) in self.labelled()? {
            for (label, kind) in [(event, "event"), (for_whom, "for")] {
                let key = normalise(&label);
                if key.is_empty() {
                    continue;
                }
                let (shown, key) = match names.get(&key) {
                    Some(g) => (g.clone(), normalise(g)),
                    None => (label.clone(), key),
                };
                if key.is_empty() {
                    continue;
                }
                let e = by.entry(key).or_insert_with(|| (kind.to_string(), BTreeMap::new(), HashMap::new()));
                if kind == "event" {
                    e.0 = "event".into();
                }
                e.1.insert(file.path.clone(), file.clone());
                *e.2.entry(shown).or_default() += 1;
            }
        }
        let mut out: Vec<Group> = by
            .into_values()
            .filter(|(_, files, _)| files.len() >= 2)
            .map(|(kind, files, spellings)| {
                let name = spellings.into_iter().max_by(|a, b| a.1.cmp(&b.1).then(b.0.len().cmp(&a.0.len()))).map(|s| s.0).unwrap_or_default();
                let mut files: Vec<GroupFile> = files.into_values().collect();
                files.sort_by_key(|f| f.date);
                let count = |m: Media| files.iter().filter(|f| f.media == m.as_str()).count();
                Group {
                    name,
                    kind,
                    start: files.first().map(|f| f.date).unwrap_or(0),
                    end: files.last().map(|f| f.date).unwrap_or(0),
                    photos: count(Media::Photo),
                    videos: count(Media::Video),
                    documents: count(Media::Document),
                    files,
                }
            })
            .collect();
        out.sort_by(|a, b| b.end.cmp(&a.end));
        Ok(out)
    }

    /// Ask Claude to merge names not seen before into groups. Returns true if it made a call.
    pub(crate) fn name_groups(&self, claude: &Claude) -> Result<bool, CallError> {
        let fail = |e: anyhow::Error| CallError::Retry(e.to_string());
        let known = self.group_names().map_err(fail)?;
        let mut new: BTreeMap<String, String> = BTreeMap::new();
        for (_, event, for_whom) in self.labelled().map_err(fail)? {
            for label in [event, for_whom] {
                let key = normalise(&label);
                if !key.is_empty() && !known.contains_key(&key) {
                    new.entry(key).or_insert(label);
                }
            }
        }
        if new.is_empty() || self.money_left().map_err(fail)? < crate::cloud::MIN_LEFT {
            return Ok(false);
        }
        let batch: Vec<(String, String)> = new.into_iter().take(NAMES_PER_CALL).collect();
        let mut groups: Vec<String> = known.values().cloned().collect();
        groups.sort();
        groups.dedup();
        let labels: Vec<String> = batch.iter().map(|(_, l)| l.clone()).collect();
        let (pairs, spend) = claude.group_names(&groups, &labels)?;
        self.record_spend("groups", spend).map_err(fail)?;
        let answered: HashMap<String, String> = pairs.into_iter().map(|(l, g)| (normalise(&l), g)).collect();
        for (key, label) in batch {
            // A name Claude skipped stays its own group, so it isn't asked about again.
            let grp = answered.get(&key).filter(|g| !normalise(g).is_empty()).cloned().unwrap_or(label);
            self.conn.execute("INSERT OR REPLACE INTO label_groups (label, grp) VALUES (?1, ?2)", params![key, grp]).map_err(|e| fail(e.into()))?;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::{Description, Spend};

    fn add(db: &Db, path: &str, media: Media, event: &str, for_whom: &str, t: i64) {
        let d = Description { summary: format!("{path} summary"), event: event.into(), for_whom: for_whom.into(), ..Default::default() };
        db.store_description(path, media, &d, &Default::default(), t, Spend::default()).unwrap();
    }

    #[test]
    fn files_group_by_event_and_merged_names() {
        let db = Db::in_memory().unwrap();
        add(&db, "/p/a.jpg", Media::Photo, "NSS Alumni Meet 2026", "", 100);
        add(&db, "/p/b.jpg", Media::Photo, "the NSS alumni meet 2026!", "", 200);
        add(&db, "/v/edit.mp4", Media::Video, "Alumni meet", "NSS", 300);
        add(&db, "/d/invite.docx", Media::Document, "", "NSS", 50);
        add(&db, "/p/cat.jpg", Media::Photo, "none", "personal", 400);
        add(&db, "/p/lone.jpg", Media::Photo, "Birthday party", "", 500);

        let g = db.groups().unwrap();
        let names: Vec<&str> = g.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["NSS", "NSS Alumni Meet 2026"], "newest first; spellings merge; vague and lone names don't make groups");
        assert_eq!((g[1].photos, g[1].videos), (2, 0));
        assert_eq!((g[0].kind.as_str(), g[0].videos, g[0].documents), ("for", 1, 1));

        // Once Claude says "Alumni meet" is the same event, the video joins it.
        db.conn.execute("INSERT INTO label_groups (label, grp) VALUES ('alumni meet', 'NSS Alumni Meet 2026')", []).unwrap();
        let g = db.groups().unwrap();
        let meet = g.iter().find(|g| g.name == "NSS Alumni Meet 2026").unwrap();
        assert_eq!((meet.kind.as_str(), meet.photos, meet.videos, meet.start, meet.end), ("event", 2, 1, 100, 300));
    }

    #[test]
    fn vague_names_are_ignored() {
        assert_eq!(normalise("  The Rotaract Club, Delhi "), "rotaract club delhi");
        assert_eq!(normalise("Personal"), "");
        assert_eq!(normalise("N/A"), "");
    }
}
