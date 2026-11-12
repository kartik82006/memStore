//! "Remember when" search over saved memories, all on this computer.
//!
//! Understands plain questions like "the edit I made before Diwali", "that tight deadline
//! project last month" or "the video with alumni pics". The question is split into:
//! - a time: today, yesterday, last week, last month, a month name, a year, or a festival or
//!   holiday with before / after / around;
//! - the kind of work: edit, video, deck, slides, sheet, photos, report…;
//! - how it felt: deadline, rush, all-nighter, late night (busy and late memories rank higher);
//! - any other words, matched against the memory's name, its file names and folders.

use crate::db::Db;
use crate::memory::Memory;
use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone};
use std::collections::HashSet;
use std::path::Path;

/// Festival and holiday dates (main day). Lunar festivals move each year.
const MOVING: &[(&[&str], &[(i32, u32, u32)])] = &[
    (&["diwali", "deepavali"], &[(2024, 10, 31), (2025, 10, 20), (2026, 11, 8), (2027, 10, 29), (2028, 10, 17)]),
    (&["holi"], &[(2024, 3, 25), (2025, 3, 14), (2026, 3, 4), (2027, 3, 22), (2028, 3, 11)]),
    (&["dussehra", "dasara", "vijayadashami"], &[(2024, 10, 12), (2025, 10, 2), (2026, 10, 20), (2027, 10, 9)]),
    (&["navratri", "navaratri"], &[(2024, 10, 3), (2025, 9, 22), (2026, 10, 11), (2027, 9, 30)]),
    (&["ganesh chaturthi", "ganpati", "ganeshotsav"], &[(2024, 9, 7), (2025, 8, 27), (2026, 9, 14), (2027, 9, 4)]),
    (&["raksha bandhan", "rakhi"], &[(2024, 8, 19), (2025, 8, 9), (2026, 8, 28), (2027, 8, 17)]),
    (&["eid"], &[(2024, 4, 10), (2025, 3, 31), (2026, 3, 20), (2027, 3, 10)]),
    (&["easter"], &[(2024, 3, 31), (2025, 4, 20), (2026, 4, 5), (2027, 3, 28)]),
];

/// Holidays on the same date every year.
const FIXED: &[(&[&str], u32, u32)] = &[
    (&["christmas", "xmas"], 12, 25),
    (&["new year", "new years", "new year's"], 1, 1),
    (&["republic day"], 1, 26),
    (&["independence day"], 8, 15),
    (&["valentine", "valentines", "valentine's"], 2, 14),
    (&["halloween"], 10, 31),
    (&["gandhi jayanti"], 10, 2),
];

const MONTHS: &[&str] =
    &["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];

/// Words that mean "a hard push": rank busy and late memories higher.
const RUSH: &[&str] = &["deadline", "deadlines", "tight", "rush", "rushed", "urgent", "crunch", "hurry", "last-minute", "pressure", "busy", "intense"];
const LATE: &[&str] = &["night", "nights", "late", "overnight", "all-nighter", "allnighter", "midnight"];

/// Words naming a kind of work, with the kinds of file that count as a match.
const KINDS: &[(&[&str], &str, &[&str])] = &[
    (&["edit", "edited", "editing", "video", "videos", "vlog", "reel", "reels", "film", "clip", "footage", "montage"], "video", &["Video", "Creative projects"]),
    (&["presentation", "deck", "slides", "slide", "ppt", "powerpoint", "pitch"], "presentation", &["Presentations"]),
    (&["sheet", "sheets", "spreadsheet", "excel", "budget", "accounts", "data"], "spreadsheet", &["Spreadsheets"]),
    (&["photos", "photo", "pics", "pic", "pictures", "picture", "images", "image", "shoot", "album"], "photos", &["Images"]),
    (&["essay", "report", "doc", "document", "writing", "assignment", "thesis", "paper", "notes", "letter", "resume", "cv"], "writing", &["Documents"]),
    (&["music", "song", "songs", "audio", "podcast", "track", "mix", "recording"], "audio", &["Audio"]),
];

/// Filler words dropped from the question.
const FILLER: &[&str] = &[
    "do", "you", "remember", "the", "a", "an", "i", "me", "my", "we", "our", "did", "made", "make", "worked",
    "work", "working", "on", "for", "with", "which", "that", "this", "had", "has", "have", "was", "were", "is",
    "it", "of", "in", "at", "to", "from", "some", "something", "thing", "stuff", "project", "projects", "files",
    "file", "find", "show", "where", "what", "when", "those", "these", "etc", "and", "or", "about", "one",
    "before", "after", "around", "during", "near", "last", "this", "ago", "back", "few", "time", "just", "like",
    "can", "could", "please", "all", "used", "using", "organisation", "organization", "org", "company", "client",
    "for", "some", "really", "very", "big",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Question {
    /// Only memories that overlap [start, end).
    pub range: Option<(i64, i64)>,
    /// e.g. "2 weeks before Diwali 2026"
    pub range_label: Option<String>,
    pub kinds: Vec<&'static str>,
    pub rush: bool,
    pub late: bool,
    pub words: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Recalled {
    pub memory: Memory,
    pub score: f64,
    /// Short reasons it matched, shown to the user.
    pub why: Vec<String>,
}

fn day_start<Tz: TimeZone>(tz: &Tz, d: NaiveDate) -> i64 {
    tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp()).unwrap_or(0)
}

/// The most recent date of a holiday, up to a week from now.
fn holiday_date(name: &str, today: NaiveDate) -> Option<NaiveDate> {
    let limit = today + Duration::days(7);
    for (names, dates) in MOVING {
        if names.contains(&name) {
            return dates
                .iter()
                .filter_map(|&(y, m, d)| NaiveDate::from_ymd_opt(y, m, d))
                .filter(|d| *d <= limit)
                .max();
        }
    }
    for (names, m, d) in FIXED {
        if names.contains(&name) {
            let this = NaiveDate::from_ymd_opt(today.year(), *m, *d)?;
            return Some(if this <= limit { this } else { NaiveDate::from_ymd_opt(today.year() - 1, *m, *d)? });
        }
    }
    None
}

fn holiday_names() -> impl Iterator<Item = &'static str> {
    MOVING.iter().flat_map(|(n, _)| n.iter().copied()).chain(FIXED.iter().flat_map(|(n, _, _)| n.iter().copied()))
}

fn title(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Turn a question into what to look for. `now` is a unix time.
pub fn parse<Tz: TimeZone>(q: &str, now: i64, tz: &Tz) -> Question {
    let today = DateTime::from_timestamp(now, 0).unwrap_or_default().with_timezone(tz).date_naive();
    let lower = q.to_lowercase().replace(['?', '!', ',', '.', '"'], " ");
    let mut text = format!(" {} ", lower.split_whitespace().collect::<Vec<_>>().join(" "));
    let mut out = Question::default();
    let at = |d: NaiveDate| day_start(tz, d);

    // Holidays, longest names first so "new year" wins over "new".
    let mut names: Vec<&str> = holiday_names().collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    for name in names {
        let pat = format!(" {name} ");
        let Some(pos) = text.find(&pat) else { continue };
        let Some(date) = holiday_date(name, today) else { continue };
        let before = &text[..pos];
        let pretty = format!("{} {}", title(name), date.year());
        let (range, label) = if before.ends_with(" before") || before.ends_with(" till") || before.ends_with(" until") {
            ((at(date - Duration::days(30)), at(date + Duration::days(1))), format!("the month before {pretty}"))
        } else if before.ends_with(" after") {
            ((at(date), at(date + Duration::days(30))), format!("the month after {pretty}"))
        } else {
            ((at(date - Duration::days(10)), at(date + Duration::days(10))), format!("around {pretty}"))
        };
        out.range = Some(range);
        out.range_label = Some(label);
        text = text.replacen(&pat, " ", 1);
        break;
    }

    if out.range.is_none() {
        let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
        let first_of = |y: i32, m: u32| NaiveDate::from_ymd_opt(y, m, 1).unwrap();
        let this_month = first_of(today.year(), today.month());
        let last_month = if today.month() == 1 { first_of(today.year() - 1, 12) } else { first_of(today.year(), today.month() - 1) };
        let phrases: Vec<(&str, NaiveDate, NaiveDate, String)> = vec![
            ("today", today, today + Duration::days(1), "today".into()),
            ("yesterday", today - Duration::days(1), today, "yesterday".into()),
            ("last week", monday - Duration::days(7), monday, "last week".into()),
            ("this week", monday, today + Duration::days(1), "this week".into()),
            ("last month", last_month, this_month, "last month".into()),
            ("this month", this_month, today + Duration::days(1), "this month".into()),
            ("last year", first_of(today.year() - 1, 1), first_of(today.year(), 1), format!("{}", today.year() - 1)),
            ("this year", first_of(today.year(), 1), today + Duration::days(1), format!("{}", today.year())),
        ];
        for (p, a, b, label) in phrases {
            let pat = format!(" {p} ");
            if text.contains(&pat) {
                out.range = Some((at(a), at(b)));
                out.range_label = Some(label);
                text = text.replacen(&pat, " ", 1);
                break;
            }
        }
    }
    if out.range.is_none() {
        let owned = text.clone();
        let tokens: Vec<&str> = owned.split_whitespace().collect();
        let year = tokens.iter().filter_map(|t| t.parse::<i32>().ok()).find(|y| (2000..2100).contains(y));
        // A month by full name or three letters ("mar"); "may" only after "in" (it is also a verb).
        let month = tokens.iter().enumerate().find_map(|(i, t)| {
            let mi = MONTHS.iter().position(|m| *m == *t || (t.len() == 3 && m.starts_with(*t)))?;
            (*t != "may" || (i > 0 && tokens[i - 1] == "in")).then_some((mi, t.to_string()))
        });
        if let Some((mi, tok)) = month {
            let m = mi as u32 + 1;
            let y = year.unwrap_or(if m <= today.month() { today.year() } else { today.year() - 1 });
            let a = NaiveDate::from_ymd_opt(y, m, 1).unwrap();
            let b = if m == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap() } else { NaiveDate::from_ymd_opt(y, m + 1, 1).unwrap() };
            out.range = Some((at(a), at(b)));
            out.range_label = Some(format!("{} {y}", title(MONTHS[mi])));
            text = text.replacen(&format!(" {tok} "), " ", 1);
        } else if let Some(y) = year {
            out.range = Some((at(NaiveDate::from_ymd_opt(y, 1, 1).unwrap()), at(NaiveDate::from_ymd_opt(y + 1, 1, 1).unwrap())));
            out.range_label = Some(y.to_string());
        }
        // "3 weeks ago", "2 months ago"
        if out.range.is_none() {
            for w in tokens.windows(3) {
                if w[2] != "ago" {
                    continue;
                }
                let n: i64 = match w[0] { "a" | "one" => 1, "two" | "couple" => 2, "few" | "three" => 3, s => s.parse().unwrap_or(0) };
                let days = match w[1].trim_end_matches('s') { "day" => n, "week" => n * 7, "month" => n * 30, "year" => n * 365, _ => 0 };
                if n > 0 && days > 0 {
                    let mid = today - Duration::days(days);
                    let spread = (days / 3).max(2);
                    out.range = Some((at(mid - Duration::days(spread)), at(mid + Duration::days(spread + 1))));
                    out.range_label = Some(format!("about {} {} ago", w[0], w[1]));
                }
            }
        }
    }

    let skip: HashSet<&str> = FILLER.iter().copied().chain(MONTHS.iter().copied()).collect();
    for t in text.split_whitespace() {
        let t = t.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
        if t.is_empty() || t.parse::<i64>().is_ok() {
            continue;
        }
        if RUSH.contains(&t) {
            out.rush = true;
            continue;
        }
        if LATE.contains(&t) {
            out.late = true;
            continue;
        }
        if let Some((_, kind, _)) = KINDS.iter().find(|(ws, _, _)| ws.contains(&t)) {
            if !out.kinds.contains(kind) {
                out.kinds.push(kind);
            }
            // "vlog", "thesis", "budget" can also be in file names, so keep them as words too.
            if !["edit", "edited", "editing", "video", "videos", "photos", "photo", "pics", "pic", "pictures", "picture", "images", "image", "doc", "document", "sheet", "sheets", "spreadsheet", "slides", "slide", "presentation", "deck"].contains(&t) {
                out.words.push(t.to_string());
            }
            continue;
        }
        if t.len() >= 3 && !skip.contains(t) {
            out.words.push(t.to_string());
        }
    }
    out
}

/// True when a memory word matches a question word: same word, or one starts the other
/// (alumni / alumnus, organisation / org).
fn word_match(q: &str, m: &str) -> bool {
    q == m
        || (q.len() >= 4 && m.len() >= 4 && (q.starts_with(m) || m.starts_with(q)))
        || (q.len() >= 5 && m.len() >= 5 && q[..5] == m[..5])
}

fn file_category(path: &str) -> &'static str {
    let ext = Path::new(path).extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    crate::filter::TRACKED_TYPES.iter().find(|(_, exts)| exts.contains(&ext.as_str())).map(|(c, _)| *c).unwrap_or("")
}

fn hours(mins: i64) -> String {
    if mins < 60 { format!("{mins} min") } else { format!("{:.1} h", mins as f64 / 60.0).replace(".0 h", " h") }
}

impl Db {
    /// Memories matching a plain-language question, best first.
    pub fn recall(&self, q: &str, limit: usize) -> Result<(Question, Vec<Recalled>)> {
        self.recall_in(q, chrono::Utc::now().timestamp(), &chrono::Local, limit)
    }

    pub fn recall_in<Tz: TimeZone>(&self, q: &str, now: i64, tz: &Tz, limit: usize) -> Result<(Question, Vec<Recalled>)> {
        let question = parse(q, now, tz);
        let memories = self.memories()?;
        let max_intensity = memories.iter().map(Memory::intensity).fold(1.0, f64::max);
        let mut out = Vec::new();
        for m in memories {
            let mut score = 0.0;
            let mut why = Vec::new();
            if let Some((a, b)) = question.range {
                if m.end < a || m.start >= b {
                    continue;
                }
                score += 1.0;
                if let Some(l) = &question.range_label {
                    why.push(l.clone());
                }
            }
            let mwords: Vec<&str> = m.words.split_whitespace().collect();
            let matched: Vec<&String> = question.words.iter().filter(|q| mwords.iter().any(|w| word_match(q, w))).collect();
            if !matched.is_empty() {
                score += 3.0 * matched.len() as f64;
                why.push(format!("matches {}", matched.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")));
            }
            for kind in &question.kinds {
                let cats = KINDS.iter().find(|(_, k, _)| k == kind).map(|(_, _, c)| *c).unwrap_or(&[]);
                if m.work == *kind {
                    score += 2.0;
                } else if m.files.iter().any(|f| cats.contains(&file_category(f))) {
                    score += 1.0;
                } else {
                    continue;
                }
                why.push(crate::memory::work_label(kind).to_lowercase().replace("spreadsheet work", "spreadsheets"));
            }
            if question.rush {
                let r = m.intensity() / max_intensity;
                if r < 0.25 {
                    continue; // asked for a hard push; this was a light one
                }
                score += 2.0 * r + if m.late_minutes > 0 { 0.5 } else { 0.0 };
                if r >= 0.5 {
                    why.push(format!("busy: {} over {} day{}", hours(m.active_minutes), m.days, if m.days == 1 { "" } else { "s" }));
                }
            }
            if question.late && m.late_minutes > 0 {
                score += 1.0 + (m.late_minutes as f64 / 60.0).min(3.0) / 3.0;
                why.push(format!("{} late at night", hours(m.late_minutes)));
            }
            let asked_something = !question.words.is_empty() || !question.kinds.is_empty() || question.rush || question.late;
            let relevant = matched.len() + why.len();
            if relevant == 0 || (asked_something && score <= 1.0) {
                continue;
            }
            out.push(Recalled { memory: m, score, why });
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.memory.end.cmp(&a.memory.end)));
        out.truncate(limit);
        Ok((question, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Kind;
    use chrono::FixedOffset;

    fn ist() -> FixedOffset {
        FixedOffset::east_opt(5 * 3600 + 1800).unwrap()
    }
    fn at(y: i32, m: u32, d: u32, h: u32) -> i64 {
        ist().with_ymd_and_hms(y, m, d, h, 0, 0).unwrap().timestamp()
    }
    fn date(ts: i64) -> NaiveDate {
        DateTime::from_timestamp(ts, 0).unwrap().with_timezone(&ist()).date_naive()
    }

    #[test]
    fn parses_holidays_feelings_kinds_and_words() {
        let now = at(2026, 11, 20, 12);
        let q = parse("do you remember the project i did on a tight deadline before Diwali", now, &ist());
        let (a, b) = q.range.unwrap();
        assert_eq!((date(a), date(b)), (NaiveDate::from_ymd_opt(2026, 10, 9).unwrap(), NaiveDate::from_ymd_opt(2026, 11, 9).unwrap()));
        assert!(q.rush);
        assert!(q.words.is_empty(), "{:?}", q.words);

        let q = parse("the edit I made for this organisation which had alumni pics etc", now, &ist());
        assert_eq!(q.range, None);
        assert_eq!(q.kinds, ["video", "photos"]);
        assert_eq!(q.words, ["alumni"]);

        // Before this year's Diwali (Nov 8) has come round, "Diwali" means last year's.
        let q = parse("after diwali", at(2026, 6, 1, 12), &ist());
        assert_eq!(date(q.range.unwrap().0), NaiveDate::from_ymd_opt(2025, 10, 20).unwrap());

        let q = parse("budget sheet in march", now, &ist());
        assert_eq!(q.range_label.as_deref(), Some("March 2026"));
        assert_eq!(q.kinds, ["spreadsheet"]);
        assert_eq!(q.words, ["budget"]);

        let q = parse("late night essay last month", now, &ist());
        assert!(q.late);
        assert_eq!(q.range_label.as_deref(), Some("last month"));
        assert_eq!(q.kinds, ["writing"]);
    }

    #[test]
    fn finds_the_right_memory() {
        let db = Db::in_memory().unwrap();
        let home = dirs::home_dir().unwrap().to_string_lossy().into_owned();
        let rec = |p: &str, ts: i64| db.record(&format!("{home}/{p}"), Kind::Modified, "watcher", ts).unwrap();
        // Alumni video: late nights the week before Diwali 2026.
        for d in 3..=6 {
            for m in 0..36 {
                let ts = at(2026, 11, d, 21) + m * 300;
                rec("Videos/NSS Alumni Meet/alumni meet.prproj", ts);
                rec("Videos/NSS Alumni Meet/alumni pics.zip", ts + 30);
            }
        }
        // A relaxed spreadsheet in the same weeks.
        rec("Documents/Budget/budget 2026.xlsx", at(2026, 10, 25, 11));
        rec("Documents/Budget/budget notes.docx", at(2026, 10, 25, 11) + 120);
        db.remember_finished_days_in(&ist(), at(2026, 11, 20, 9)).unwrap();
        assert_eq!(db.memories().unwrap().len(), 2);

        let now = at(2026, 11, 20, 12);
        let (_, r) = db.recall_in("the project I did on a tight deadline before Diwali", now, &ist(), 5).unwrap();
        assert_eq!(r.len(), 1, "{r:#?}");
        assert_eq!(r[0].memory.topic, "Alumni");
        assert!(r[0].why.iter().any(|w| w.starts_with("busy")), "{:?}", r[0].why);

        let (_, r) = db.recall_in("the edit for that organisation with alumni pics", now, &ist(), 5).unwrap();
        assert_eq!(r[0].memory.topic, "Alumni");

        let (_, r) = db.recall_in("budget", now, &ist(), 5).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].memory.topic, "Budget");

        let (_, r) = db.recall_in("something about whales", now, &ist(), 5).unwrap();
        assert!(r.is_empty());
    }
}
