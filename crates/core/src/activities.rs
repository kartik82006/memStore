//! Activities: groups of files that MemStor thinks belong to the same piece of work, found
//! without the user picking folders. For example a video edit: the editor's project file, the
//! footage, the music from Downloads and the exported video.
//!
//! How it works, all from what is already in the local database:
//! 1. Split the events into work sessions (a gap of more than 20 minutes starts a new one).
//! 2. Inside a session, score each pair of files on: same project folder, shared words in the
//!    file names, kinds of file that go together (footage + music + editor project), being used
//!    within a few minutes of each other, and how often the two were used together on other days.
//! 3. Link pairs that score high enough and join linked files into groups. A file used in two
//!    sessions joins those groups, so an edit spread over a morning and an evening stays one.
//! 4. Name each group from its kind of work and the most common word in its file names.
//!
//! When the user drops a file from an activity, the pairs are stored in `not_related` and
//! never linked again.

use crate::db::{Db, Touched};
use crate::filter::TRACKED_TYPES;
use crate::projects::project_folder_for;
use anyhow::Result;
use rusqlite::params;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// A pause longer than this ends a work session.
pub const SESSION_GAP_SECS: i64 = 20 * 60;
/// Files used this close together get a point.
const CLOSE_SECS: i64 = 5 * 60;
/// How far back to look for "used together before".
const HISTORY_DAYS: i64 = 30;
/// A pair needs at least this score to be linked.
const LINK_SCORE: f32 = 2.0;
/// Sessions bigger than this (a bulk copy, a photo import) are only grouped by folder.
const BIG_SESSION: usize = 400;

/// Words in file names that say nothing about the topic.
const STOPWORDS: &[&str] = &[
    "final", "draft", "copy", "new", "untitled", "img", "dsc", "dscn", "dcim", "pxl", "vid", "mvi", "gopr",
    "screenshot", "screen", "shot", "recording", "export", "exported", "render", "rendered", "version",
    "edit", "edited", "old", "backup", "file", "document", "doc", "image", "photo", "video", "audio",
    "the", "and", "for", "with", "from", "of", "to", "in", "on", "my", "rev", "temp", "test", "sample",
    "whatsapp", "signal", "telegram", "download", "downloads", "scan", "page", "part", "jpg", "png",
];

/// What sort of work an activity looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Work {
    Video,
    Audio,
    Photos,
    Presentation,
    Spreadsheet,
    Writing,
    Files,
}

impl Work {
    pub fn as_str(self) -> &'static str {
        match self {
            Work::Video => "video",
            Work::Audio => "audio",
            Work::Photos => "photos",
            Work::Presentation => "presentation",
            Work::Spreadsheet => "spreadsheet",
            Work::Writing => "writing",
            Work::Files => "files",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Work::Video => "Video edit",
            Work::Audio => "Audio",
            Work::Photos => "Photos",
            Work::Presentation => "Presentation",
            Work::Spreadsheet => "Spreadsheet work",
            Work::Writing => "Writing & reading",
            Work::Files => "Files",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Activity {
    /// "Video edit · Trip vlog"
    pub label: String,
    pub work: Work,
    /// The word or folder name the activity is named after.
    pub topic: String,
    /// Most recently used first.
    pub files: Vec<Touched>,
    pub start: i64,
    pub end: i64,
}

/// The file's category from TRACKED_TYPES ("Video", "Documents"…).
fn category(path: &str) -> &'static str {
    let ext = Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    TRACKED_TYPES.iter().find(|(_, exts)| exts.contains(&ext.as_str())).map(|(c, _)| *c).unwrap_or("")
}

/// Kinds of file that are used together for one job.
fn same_kit(a: &str, b: &str) -> bool {
    const MEDIA: &[&str] = &["Video", "Audio", "Images", "Creative projects"];
    const OFFICE: &[&str] = &["Documents", "Spreadsheets", "Presentations"];
    (!a.is_empty() && a == b)
        || (MEDIA.contains(&a) && MEDIA.contains(&b))
        || (OFFICE.contains(&a) && OFFICE.contains(&b))
}

/// The topic words in a file name: "Trip Vlog - final v3.prproj" -> {"trip", "vlog"}.
pub fn name_words(path: &str) -> HashSet<String> {
    let stem = Path::new(path).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    // Also split camelCase: "TripVlog" -> "Trip Vlog".
    let mut spaced = String::new();
    let mut prev_lower = false;
    for c in stem.chars() {
        if c.is_uppercase() && prev_lower {
            spaced.push(' ');
        }
        prev_lower = c.is_lowercase();
        spaced.push(c);
    }
    spaced
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 3 && !w.chars().any(|c| c.is_ascii_digit()) && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Order a pair so it can be looked up either way round.
fn pair<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Smallest gap between two sorted lists of times.
fn closest(a: &[i64], b: &[i64]) -> i64 {
    let (mut i, mut j, mut best) = (0, 0, i64::MAX);
    while i < a.len() && j < b.len() {
        best = best.min((a[i] - b[j]).abs());
        if a[i] < b[j] { i += 1 } else { j += 1 }
    }
    best
}

struct UnionFind(Vec<usize>);

impl UnionFind {
    fn find(&mut self, x: usize) -> usize {
        let p = self.0[x];
        if p == x { return x; }
        let r = self.find(p);
        self.0[x] = r;
        r
    }
    fn join(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb { self.0[ra] = rb; }
    }
}

/// One work session: each file used in it with its event times.
type Session = BTreeMap<String, Vec<i64>>;

fn sessions(events: &[(String, i64)]) -> Vec<Session> {
    let mut out: Vec<Session> = Vec::new();
    let mut last = i64::MIN;
    for (path, ts) in events {
        if out.is_empty() || ts - last > SESSION_GAP_SECS {
            out.push(Session::new());
        }
        out.last_mut().unwrap().entry(path.clone()).or_default().push(*ts);
        last = *ts;
    }
    out
}

impl Db {
    /// Events in [start, end) as (path, time), oldest first, leaving out deletes and files
    /// that have since been deleted.
    fn live_events(&self, start: i64, end: i64) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, e.ts FROM events e JOIN files f ON f.id = e.file_id
             WHERE e.ts >= ?1 AND e.ts < ?2 AND e.kind != 'deleted'
               AND (SELECT kind FROM events l WHERE l.file_id = f.id ORDER BY ts DESC, id DESC LIMIT 1) != 'deleted'
             ORDER BY e.ts",
        )?;
        let rows = stmt.query_map(params![start, end], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn not_related_pairs(&self) -> Result<HashSet<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT a, b FROM not_related")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Remember that `path` does not belong with these other files, so they are never grouped again.
    pub fn mark_not_related(&self, path: &str, others: &[String]) -> Result<()> {
        let mut stmt = self.conn.prepare("INSERT OR IGNORE INTO not_related (a, b) VALUES (?1, ?2)")?;
        for o in others.iter().filter(|o| o.as_str() != path) {
            let (a, b) = pair(path, o);
            stmt.execute(params![a, b])?;
        }
        Ok(())
    }

    /// The activities found in [start, end), most recent first.
    pub fn activities(&self, start: i64, end: i64, home: Option<&Path>) -> Result<Vec<Activity>> {
        let events = self.live_events(start, end)?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let blocked = self.not_related_pairs()?;

        // Sessions from the last month, to count how often two files were used together before.
        let past = self.live_events(start - HISTORY_DAYS * 86_400, start)?;
        let mut seen_in: HashMap<String, HashSet<usize>> = HashMap::new();
        for (i, s) in sessions(&past).iter().enumerate() {
            for p in s.keys() {
                seen_in.entry(p.clone()).or_default().insert(i);
            }
        }
        let used_together_before = |a: &str, b: &str| -> usize {
            match (seen_in.get(a), seen_in.get(b)) {
                (Some(x), Some(y)) => x.intersection(y).count(),
                _ => 0,
            }
        };

        // Facts about each file, computed once.
        let mut index: HashMap<String, usize> = HashMap::new();
        let mut paths: Vec<String> = Vec::new();
        for (p, _) in &events {
            if !index.contains_key(p) {
                index.insert(p.clone(), paths.len());
                paths.push(p.clone());
            }
        }
        let folder: Vec<Option<String>> =
            paths.iter().map(|p| project_folder_for(Path::new(p), home).map(|f| f.to_string_lossy().into_owned())).collect();
        let words: Vec<HashSet<String>> = paths.iter().map(|p| name_words(p)).collect();
        let cat: Vec<&str> = paths.iter().map(|p| category(p)).collect();

        let mut uf = UnionFind((0..paths.len()).collect());
        let mut linked = vec![false; paths.len()];
        let mut link = |uf: &mut UnionFind, a: usize, b: usize| {
            uf.join(a, b);
            linked[a] = true;
            linked[b] = true;
        };

        for s in sessions(&events) {
            let files: Vec<(usize, &Vec<i64>)> = s.iter().map(|(p, t)| (index[p], t)).collect();
            if files.len() > BIG_SESSION {
                let mut by_folder: HashMap<&str, usize> = HashMap::new();
                for (i, _) in &files {
                    if let Some(f) = &folder[*i] {
                        match by_folder.get(f.as_str()) {
                            Some(&first) if !blocked.contains(&pair_owned(&paths[first], &paths[*i])) => link(&mut uf, first, *i),
                            Some(_) => {}
                            None => { by_folder.insert(f, *i); }
                        }
                    }
                }
                continue;
            }
            for x in 0..files.len() {
                for y in x + 1..files.len() {
                    let ((a, ta), (b, tb)) = (files[x], files[y]);
                    if blocked.contains(&pair_owned(&paths[a], &paths[b])) {
                        continue;
                    }
                    let mut score = 0.0f32;
                    if folder[a].is_some() && folder[a] == folder[b] {
                        score += 2.0;
                    }
                    score += (words[a].intersection(&words[b]).count() as f32 * 1.5).min(3.0);
                    if same_kit(cat[a], cat[b]) {
                        score += 1.0;
                    }
                    if closest(ta, tb) <= CLOSE_SECS {
                        score += 1.0;
                    }
                    score += used_together_before(&paths[a], &paths[b]).min(3) as f32;
                    if score >= LINK_SCORE {
                        link(&mut uf, a, b);
                    }
                }
            }
        }

        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..paths.len() {
            if linked[i] {
                let r = uf.find(i);
                groups.entry(r).or_default().push(i);
            }
        }

        let touched: HashMap<String, Touched> =
            self.touched_between(start, end)?.into_iter().map(|t| (t.path.clone(), t)).collect();
        let mut out: Vec<Activity> = groups
            .into_values()
            .filter(|g| g.len() >= 2)
            .map(|g| {
                let mut files: Vec<Touched> = g.iter().filter_map(|&i| touched.get(&paths[i]).cloned()).collect();
                files.sort_by(|a, b| b.last.cmp(&a.last).then(a.path.cmp(&b.path)));
                let work = work_of(&g.iter().map(|&i| cat[i]).collect::<Vec<_>>());
                let topic = topic_of(&g, &paths, &words, &folder, &files);
                Activity {
                    label: format!("{} · {}", work.label(), topic),
                    work,
                    topic,
                    start: files.iter().map(|f| f.first).min().unwrap_or(start),
                    end: files.iter().map(|f| f.last).max().unwrap_or(start),
                    files,
                }
            })
            .collect();
        out.sort_by(|a, b| b.end.cmp(&a.end).then(a.label.cmp(&b.label)));
        Ok(out)
    }
}

fn pair_owned(a: &str, b: &str) -> (String, String) {
    let (x, y) = pair(a, b);
    (x.to_string(), y.to_string())
}

/// The kind of work, from the mix of file kinds.
fn work_of(cats: &[&str]) -> Work {
    let n = |c: &str| cats.iter().filter(|x| **x == c).count();
    let (video, audio, images, creative) = (n("Video"), n("Audio"), n("Images"), n("Creative projects"));
    let (docs, sheets, slides) = (n("Documents"), n("Spreadsheets"), n("Presentations"));
    if video > 0 || (creative > 0 && audio + images > 0) {
        Work::Video
    } else if slides > 0 {
        Work::Presentation
    } else if sheets > 0 && sheets >= docs {
        Work::Spreadsheet
    } else if docs > 0 {
        Work::Writing
    } else if audio > 0 && audio >= images {
        Work::Audio
    } else if images > 0 {
        Work::Photos
    } else {
        Work::Files
    }
}

fn capitalize(w: &str) -> String {
    let mut c = w.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// What to call the activity: a word shared by several file names, else the project folder's
/// name, else the name of its most used file.
fn topic_of(g: &[usize], paths: &[String], words: &[HashSet<String>], folder: &[Option<String>], files: &[Touched]) -> String {
    let mut count: HashMap<&str, usize> = HashMap::new();
    for &i in g {
        for w in &words[i] {
            *count.entry(w.as_str()).or_default() += 1;
        }
    }
    let mut shared: Vec<(&str, usize)> = count.into_iter().filter(|(_, c)| *c >= 2).collect();
    shared.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.len().cmp(&a.0.len())).then(a.0.cmp(b.0)));
    if let Some((w, c)) = shared.first() {
        // Two words that always appear together read better as a phrase: "Trip vlog".
        if let Some((w2, _)) = shared.get(1).filter(|(_, c2)| c2 == c) {
            let first = paths[g[0]].to_lowercase();
            let (a, b) = if first.find(w) <= first.find(w2) { (*w, *w2) } else { (*w2, *w) };
            return format!("{} {}", capitalize(a), b);
        }
        return capitalize(w);
    }
    let mut folders: HashMap<&str, usize> = HashMap::new();
    for &i in g {
        if let Some(f) = &folder[i] {
            *folders.entry(f.as_str()).or_default() += 1;
        }
    }
    if let Some((f, _)) = folders.into_iter().filter(|(_, c)| *c >= 2).max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0))) {
        if let Some(name) = Path::new(f).file_name() {
            return name.to_string_lossy().into_owned();
        }
    }
    files
        .iter()
        .max_by(|a, b| a.events.cmp(&b.events).then(b.path.cmp(&a.path)))
        .and_then(|t| Path::new(&t.path).file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "Untitled".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Kind;

    const H: &str = "/home/k";
    const T: i64 = 1_800_000_000;

    fn rec(db: &Db, path: &str, ts: i64) {
        db.record(path, Kind::Modified, "watcher", ts).unwrap();
    }

    fn names(a: &Activity) -> Vec<String> {
        let mut v: Vec<String> =
            a.files.iter().map(|f| Path::new(&f.path).file_name().unwrap().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    fn acts(db: &Db) -> Vec<Activity> {
        db.activities(T - 3600, T + 86_400, Some(Path::new(H))).unwrap()
    }

    #[test]
    fn words_skip_noise() {
        let w = name_words("/x/TripVlog - FINAL v3 (copy) 2024-05-01.prproj");
        assert_eq!(w, HashSet::from(["trip".to_string(), "vlog".to_string()]));
        assert!(name_words("/x/IMG_2041.jpg").is_empty());
    }

    #[test]
    fn video_edit_spanning_folders_is_one_activity() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/home/k/Videos/Trip Vlog/trip vlog.prproj", T);
        rec(&db, "/home/k/Videos/Trip Vlog/footage/C0012.mp4", T + 60);
        rec(&db, "/home/k/Videos/Trip Vlog/footage/C0013.mp4", T + 90);
        rec(&db, "/home/k/Downloads/upbeat summer.mp3", T + 300); // music from elsewhere, same minutes
        rec(&db, "/home/k/Videos/trip vlog export.mp4", T + 1500);
        rec(&db, "/home/k/Videos/Trip Vlog/trip vlog.prproj", T + 1500);
        // Unrelated work an hour later.
        rec(&db, "/home/k/Documents/Taxes/2025 return.pdf", T + 5400);
        rec(&db, "/home/k/Documents/Taxes/receipts.xlsx", T + 5460);

        let a = acts(&db);
        assert_eq!(a.len(), 2, "{a:#?}");
        let taxes = &a[0];
        assert_eq!(taxes.work, Work::Spreadsheet);
        assert_eq!(taxes.topic, "Taxes");
        let video = &a[1];
        assert_eq!(video.work, Work::Video);
        assert_eq!(video.label, "Video edit · Trip vlog");
        assert_eq!(
            names(video),
            ["C0012.mp4", "C0013.mp4", "trip vlog export.mp4", "trip vlog.prproj", "upbeat summer.mp3"]
        );
    }

    #[test]
    fn lone_and_unrelated_files_are_left_out() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/home/k/Downloads/invoice.pdf", T);
        rec(&db, "/home/k/Pictures/cat.jpg", T + 3000);
        assert!(acts(&db).is_empty());
    }

    #[test]
    fn used_together_before_links_files_apart_in_time() {
        let db = Db::in_memory().unwrap();
        // Last week, three separate days, both files used.
        for d in 1..=3 {
            rec(&db, "/home/k/Downloads/budget.xlsx", T - d * 86_400);
            rec(&db, "/home/k/Desktop/notes.docx", T - d * 86_400 + 600);
        }
        // Today, 15 minutes apart: no shared folder, words or closeness.
        rec(&db, "/home/k/Downloads/budget.xlsx", T);
        rec(&db, "/home/k/Desktop/notes.docx", T + 900);
        let a = acts(&db);
        assert_eq!(a.len(), 1);
        assert_eq!(names(&a[0]), ["budget.xlsx", "notes.docx"]);
    }

    #[test]
    fn dropping_a_file_keeps_it_out() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/home/k/Documents/Club/minutes.docx", T);
        rec(&db, "/home/k/Documents/Club/members.xlsx", T + 60);
        rec(&db, "/home/k/Downloads/club poster.pdf", T + 120);
        let a = acts(&db);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].files.len(), 3);

        let others: Vec<String> = a[0].files.iter().map(|f| f.path.clone()).collect();
        db.mark_not_related("/home/k/Downloads/club poster.pdf", &others).unwrap();
        let a = acts(&db);
        assert_eq!(a.len(), 1);
        assert_eq!(names(&a[0]), ["members.xlsx", "minutes.docx"]);
    }

    #[test]
    fn deleted_files_are_left_out() {
        let db = Db::in_memory().unwrap();
        rec(&db, "/home/k/Documents/Club/minutes.docx", T);
        rec(&db, "/home/k/Documents/Club/old.docx", T + 60);
        db.record("/home/k/Documents/Club/old.docx", Kind::Deleted, "watcher", T + 120).unwrap();
        assert!(acts(&db).is_empty());
    }
}
