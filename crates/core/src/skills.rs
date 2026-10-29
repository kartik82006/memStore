//! Everything MemStor can do, defined once. Each skill has a name, a description, the inputs
//! it takes and how risky it is. The assistant calls skills as tools, the window shows their
//! results as cards, and the same list can later be offered to other apps.
//!
//! Risk levels:
//! - look: reads MemStor's records. Runs straight away.
//! - safe: opens a file or makes a folder of shortcuts. The user presses one button.
//! - change: renames or moves files. Shown in full first; runs only after "Do it"; undoable.

use crate::actions::{self, Action, Op};
use crate::db::Db;
use anyhow::{Context, Result, bail, ensure};
use chrono::{Duration, Local, NaiveDate, TimeZone};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Look,
    Safe,
    Change,
}

pub struct Skill {
    pub name: &'static str,
    pub description: &'static str,
    pub risk: Risk,
    pub input: fn() -> Value,
}

/// A file as shown in a card.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileItem {
    pub path: String,
    pub name: String,
    pub folder: String,
    /// Last used (unix seconds); 0 when not known.
    pub when: i64,
    /// A few words: "edited", "matches: pink top", "selected".
    pub note: String,
    pub exists: bool,
}

impl FileItem {
    pub fn new(path: &str, when: i64, note: impl Into<String>) -> FileItem {
        let p = Path::new(path);
        FileItem {
            name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string()),
            folder: p.parent().map(|d| d.display().to_string()).unwrap_or_default(),
            exists: p.exists(),
            path: path.to_string(),
            when,
            note: note.into(),
        }
    }
}

/// What the window shows for a skill's result.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Card {
    /// A list of files. `gather` is the folder name to offer for gathering them, if any.
    Files { title: String, files: Vec<FileItem>, gather: Option<String> },
    /// A piece of work, memory or event with its files.
    Work { kind: String, title: String, subtitle: String, start: i64, end: i64, files: Vec<FileItem>, gather: String },
    /// Open a file (or show it in its folder) right away.
    Open { file: FileItem, reveal: bool },
    /// A change waiting for the user's OK.
    Proposal { action: Action },
    /// A short note (budget, privacy).
    Note { text: String },
}

/// A skill's result: text for the assistant and cards for the window.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub text: String,
    pub cards: Vec<Card>,
}

fn obj(props: Value, required: &[&str]) -> Value {
    json!({"type": "object", "additionalProperties": false, "properties": props, "required": required})
}

/// The full list.
pub fn all() -> Vec<Skill> {
    vec![
        Skill {
            name: "find_files",
            description: "Find files by words in their name or folder path. Fast and free; use first when the person names a file or folder.",
            risk: Risk::Look,
            input: || obj(json!({"words": {"type": "string", "description": "Words from the file name or folder, e.g. 'thesis chapter'"}}), &["words"]),
        },
        Skill {
            name: "recent_files",
            description: "The files used most recently, newest first.",
            risk: Risk::Look,
            input: || obj(json!({}), &[]),
        },
        Skill {
            name: "files_on_day",
            description: "Every file created, edited, renamed or opened on one day, with the pieces of work that day.",
            risk: Risk::Look,
            input: || obj(json!({"day": {"type": "string", "description": "'today', 'yesterday' or a date as YYYY-MM-DD"}}), &["day"]),
        },
        Skill {
            name: "activities",
            description: "Pieces of work (groups of files used together, like a video edit with its footage and music) in a period.",
            risk: Risk::Look,
            input: || obj(json!({"range": {"type": "string", "enum": ["today", "yesterday", "week"]}}), &["range"]),
        },
        Skill {
            name: "search_by_memory",
            description: "Search by what the person remembers: what is in a photo or video, an event, who it was for, when, how it felt (\"the edit before Diwali\", \"girl in pink doing yoga\"). Uses the descriptions MemStor keeps and saved memories. Costs a little; use when names alone won't find it.",
            risk: Risk::Look,
            input: || obj(json!({"question": {"type": "string", "description": "The person's description, in their words"}}), &["question"]),
        },
        Skill {
            name: "similar_files",
            description: "Files that look like this one: same event, people, place or things.",
            risk: Risk::Look,
            input: || obj(json!({"path": {"type": "string"}}), &["path"]),
        },
        Skill {
            name: "file_info",
            description: "What MemStor knows about one file: when it was made and used, what's in it (if described), and the piece of work it belongs to.",
            risk: Risk::Look,
            input: || obj(json!({"path": {"type": "string"}}), &["path"]),
        },
        Skill {
            name: "events_and_groups",
            description: "Described files grouped by the event they show or who they were made for. Give a name to get one group's files.",
            risk: Risk::Look,
            input: || obj(json!({"name": {"type": "string", "description": "Part of a group name, or '' for the list"}}), &["name"]),
        },
        Skill {
            name: "projects",
            description: "The person's saved projects (a name and its folders) with today's files.",
            risk: Risk::Look,
            input: || obj(json!({}), &[]),
        },
        Skill {
            name: "open_file",
            description: "Open a file, or show it selected in its folder. Use when the person asks to open or show one specific file.",
            risk: Risk::Safe,
            input: || obj(json!({"path": {"type": "string"}, "show_in_folder": {"type": "boolean"}}), &["path", "show_in_folder"]),
        },
        Skill {
            name: "gather_files",
            description: "Offer to put shortcuts to these files into one new folder (Documents\\MemStor Gathered\\<title>). The originals are not moved. The person presses a button to do it.",
            risk: Risk::Safe,
            input: || obj(json!({"title": {"type": "string"}, "paths": {"type": "array", "items": {"type": "string"}}}), &["title", "paths"]),
        },
        Skill {
            name: "rename_file",
            description: "Propose renaming one file or folder. The person sees it and presses \"Do it\"; it can be undone. Keeps the extension if the new name has none.",
            risk: Risk::Change,
            input: || obj(json!({"path": {"type": "string"}, "new_name": {"type": "string", "description": "Just the new name, not a path"}}), &["path", "new_name"]),
        },
        Skill {
            name: "move_files",
            description: "Propose moving files into a folder, optionally a new one. The person sees every step and presses \"Do it\"; it can be undone. Never overwrites.",
            risk: Risk::Change,
            input: || obj(
                json!({
                    "paths": {"type": "array", "items": {"type": "string"}},
                    "to_folder": {"type": "string", "description": "Full path of the folder"},
                    "make_folder": {"type": "boolean", "description": "true to create to_folder if it doesn't exist"},
                    "title": {"type": "string", "description": "A few words for the change, e.g. 'Put the alumni photos together'"}
                }),
                &["paths", "to_folder", "make_folder", "title"],
            ),
        },
    ]
}

pub fn find(name: &str) -> Option<Skill> {
    all().into_iter().find(|s| s.name == name)
}

/// The skills as Claude tool definitions.
pub fn tool_definitions() -> Vec<Value> {
    all()
        .into_iter()
        .map(|s| json!({"name": s.name, "description": s.description, "input_schema": (s.input)(), "strict": true}))
        .collect()
}

// ---------- helpers ----------

fn local_midnight(d: NaiveDate) -> i64 {
    Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().map(|t| t.timestamp()).unwrap_or(0)
}

/// [start, end) for "today", "yesterday", "week" or a YYYY-MM-DD date, plus a label.
pub fn day_range(day: &str) -> Result<(i64, i64, String)> {
    let today = Local::now().date_naive();
    let (first, days, label) = match day.trim().to_lowercase().as_str() {
        "today" | "" => (today, 1, "today".to_string()),
        "yesterday" => (today - Duration::days(1), 1, "yesterday".to_string()),
        "week" => (today - Duration::days(6), 7, "the last 7 days".to_string()),
        d => {
            let date = NaiveDate::parse_from_str(d, "%Y-%m-%d").with_context(|| format!("\"{day}\" is not a date like 2026-10-28"))?;
            (date, 1, date.format("%-d %b %Y").to_string())
        }
    };
    let start = local_midnight(first);
    Ok((start, local_midnight(first + Duration::days(days)), label))
}

fn date_label(ts: i64) -> String {
    Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default()
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input.get(key).and_then(Value::as_str).with_context(|| format!("missing {key}"))
}

/// Text lines for the assistant: one file per line with when it was used.
fn file_lines(files: &[FileItem], max: usize) -> String {
    let mut out: String = files
        .iter()
        .take(max)
        .map(|f| {
            let mut l = format!("- {}", f.path);
            if f.when > 0 {
                l.push_str(&format!(" (last used {})", date_label(f.when)));
            }
            if !f.note.is_empty() {
                l.push_str(&format!(" [{}]", f.note));
            }
            if !f.exists {
                l.push_str(" [no longer there]");
            }
            l.push('\n');
            l
        })
        .collect();
    if files.len() > max {
        out.push_str(&format!("…and {} more\n", files.len() - max));
    }
    out
}

const KIND_WORDS: &[(&str, &str)] = &[("created", "new"), ("modified", "edited"), ("renamed", "renamed"), ("opened", "opened"), ("deleted", "deleted")];

fn kinds_note(kinds: &str) -> String {
    let mut v: Vec<&str> = kinds.split(',').filter_map(|k| KIND_WORDS.iter().find(|(a, _)| *a == k).map(|(_, b)| *b)).collect();
    v.dedup();
    v.join(", ")
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

/// A path the assistant gave: must be absolute and exist.
fn existing(path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(path.trim());
    ensure!(p.is_absolute(), "{path} is not a full path; use the full paths from earlier results");
    ensure!(p.exists(), "{path} isn't there any more");
    Ok(p)
}

// ---------- running a skill ----------

/// Run a skill with the assistant's input. Errors are returned as text the assistant can read.
pub fn run(db: &Db, name: &str, input: &Value) -> Result<Output> {
    match name {
        "find_files" => {
            let words = str_arg(input, "words")?;
            let hits = db.search(words, 25)?;
            let files: Vec<FileItem> = hits.iter().map(|h| FileItem::new(&h.path, h.last_seen, kinds_note(&h.last_kind))).collect();
            Ok(files_output(format!("Files matching \"{words}\""), files, None))
        }
        "recent_files" => {
            let files: Vec<FileItem> = db.recent(20)?.iter().map(|h| FileItem::new(&h.path, h.last_seen, kinds_note(&h.last_kind))).collect();
            Ok(files_output("Recent files".into(), files, None))
        }
        "files_on_day" => {
            let (start, end, label) = day_range(str_arg(input, "day")?)?;
            let files: Vec<FileItem> = db.touched_between(start, end)?.iter().map(|t| FileItem::new(&t.path, t.last, kinds_note(&t.kinds))).collect();
            let mut out = files_output(format!("Files used {label}"), files, None);
            let acts = activities_output(db, start, end)?;
            out.text.push_str(&acts.text);
            out.cards.extend(acts.cards.into_iter().take(3));
            Ok(out)
        }
        "activities" => {
            let (start, end, _) = day_range(str_arg(input, "range")?)?;
            activities_output(db, start, end)
        }
        "search_by_memory" => {
            let q = str_arg(input, "question")?;
            let a = db.smart_search(q)?;
            let mut out = Output::default();
            if let Some(n) = &a.local_only {
                out.text.push_str(&format!("Note: {n}\n"));
                out.cards.push(Card::Note { text: n.clone() });
            }
            let mut files = Vec::new();
            for f in &a.results {
                match (&f.path, f.memory_id) {
                    (Some(p), _) => files.push(FileItem::new(p, f.date, f.why.clone())),
                    (None, Some(id)) => {
                        if let Some(m) = db.memories()?.into_iter().find(|m| m.id == id) {
                            let mfiles: Vec<FileItem> = m.files.iter().map(|p| FileItem::new(p, 0, "")).collect();
                            out.text.push_str(&format!("Piece of work \"{}\" ({} to {}), {}:\n{}", m.label, date_label(m.start), date_label(m.end), f.why, file_lines(&mfiles, 8)));
                            out.cards.push(Card::Work {
                                kind: "memory".into(),
                                title: m.label.clone(),
                                subtitle: f.summary.clone(),
                                start: m.start,
                                end: m.end,
                                gather: m.topic.clone(),
                                files: mfiles,
                            });
                        }
                    }
                    _ => {}
                }
            }
            if !a.understood.is_empty() {
                out.text.push_str(&format!("Understood as: {}\n", a.understood.join("; ")));
            }
            let f = files_output("Best matches".into(), files, None);
            out.text.push_str(&f.text);
            out.cards.splice(0..0, f.cards);
            Ok(out)
        }
        "similar_files" => {
            let p = str_arg(input, "path")?;
            let found = db.similar(p, 12)?;
            let files: Vec<FileItem> = found.iter().filter_map(|f| f.path.as_ref().map(|p| FileItem::new(p, f.date, f.why.clone()))).collect();
            let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if files.is_empty() && db.description(p)?.is_none() {
                return Ok(Output { text: "That file hasn't been described yet, so similar files can't be found.".into(), cards: vec![] });
            }
            Ok(files_output(format!("Like {name}"), files, None))
        }
        "file_info" => file_info(db, str_arg(input, "path")?),
        "events_and_groups" => {
            let want = input.get("name").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
            let groups = db.groups()?;
            let mut out = Output::default();
            if want.is_empty() {
                for g in groups.iter().take(15) {
                    out.text.push_str(&format!("- {} ({} photos, {} videos, {} documents, {} to {})\n", g.name, g.photos, g.videos, g.documents, date_label(g.start), date_label(g.end)));
                }
                if groups.is_empty() {
                    out.text.push_str("No events or groups yet (files need describing first).\n");
                }
                return Ok(out);
            }
            for g in groups.iter().filter(|g| g.name.to_lowercase().contains(&want)).take(2) {
                let files: Vec<FileItem> = g.files.iter().map(|f| FileItem::new(&f.path, f.date, f.summary.chars().take(80).collect::<String>())).collect();
                out.text.push_str(&format!("Group \"{}\":\n{}", g.name, file_lines(&files, 15)));
                out.cards.push(Card::Work {
                    kind: "group".into(),
                    title: g.name.clone(),
                    subtitle: format!("{} photos, {} videos, {} documents", g.photos, g.videos, g.documents),
                    start: g.start,
                    end: g.end,
                    gather: g.name.clone(),
                    files,
                });
            }
            if out.cards.is_empty() {
                out.text = format!("No group is called anything like \"{want}\".");
            }
            Ok(out)
        }
        "projects" => {
            let (start, end, _) = day_range("today")?;
            let mut out = Output::default();
            for p in db.projects()? {
                let n = db.project_files(p.id, start, end, crate::projects::DEFAULT_NEARBY_MINUTES)?.len();
                out.text.push_str(&format!("- {} (folders: {}; {n} files used today)\n", p.name, p.folders.join(", ")));
            }
            if out.text.is_empty() {
                out.text = "No projects saved.".into();
            }
            Ok(out)
        }
        "open_file" => {
            let p = existing(str_arg(input, "path")?)?;
            let reveal = input.get("show_in_folder").and_then(Value::as_bool).unwrap_or(false);
            ensure!(reveal || p.is_dir() || crate::filter::is_tracked_type(&p), "MemStor only opens everyday files (documents, pictures, videos…).");
            let item = FileItem::new(&p.display().to_string(), 0, "");
            Ok(Output {
                text: format!("{} {} for the person.", if reveal { "Showing" } else { "Opening" }, item.path),
                cards: vec![Card::Open { file: item, reveal }],
            })
        }
        "gather_files" => {
            let title = str_arg(input, "title")?;
            let paths: Vec<String> = input.get("paths").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
            ensure!(!paths.is_empty(), "no files to gather");
            let files: Vec<FileItem> = paths.iter().map(|p| FileItem::new(p, 0, "")).collect();
            let there = files.iter().filter(|f| f.exists).count();
            Ok(Output {
                text: format!("Offered to gather {there} file(s) as \"{title}\"; the person presses the button to do it."),
                cards: vec![Card::Files { title: title.to_string(), files, gather: Some(title.to_string()) }],
            })
        }
        "rename_file" => {
            let from = existing(str_arg(input, "path")?)?;
            let to = actions::renamed_path(&from, str_arg(input, "new_name")?)?;
            let a = db.propose(
                &format!("Rename {}", from.file_name().unwrap_or_default().to_string_lossy()),
                vec![Op::Rename { from: from.display().to_string(), to: to.display().to_string() }],
            )?;
            Ok(proposal_output(a))
        }
        "move_files" => {
            let to_folder = PathBuf::from(str_arg(input, "to_folder")?.trim());
            ensure!(to_folder.is_absolute(), "to_folder must be a full path");
            let make = input.get("make_folder").and_then(Value::as_bool).unwrap_or(false);
            let title = input.get("title").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).unwrap_or("Move files");
            let mut ops = Vec::new();
            if !to_folder.is_dir() {
                ensure!(make, "{} doesn't exist; set make_folder to create it", to_folder.display());
                ops.push(Op::MakeFolder { path: to_folder.display().to_string() });
            }
            let paths = input.get("paths").and_then(Value::as_array).context("missing paths")?;
            for p in paths.iter().filter_map(Value::as_str) {
                let from = existing(p)?;
                let name = from.file_name().context("no file name")?;
                if from.parent().is_some_and(|d| d == to_folder) {
                    continue; // already there
                }
                ops.push(Op::Move { from: from.display().to_string(), to: to_folder.join(name).display().to_string() });
            }
            if ops.iter().all(|o| matches!(o, Op::MakeFolder { .. })) {
                bail!("Those files are already in {}.", to_folder.display());
            }
            Ok(proposal_output(db.propose(title, ops)?))
        }
        other => bail!("there is no skill called {other}"),
    }
}

fn files_output(title: String, files: Vec<FileItem>, gather: Option<String>) -> Output {
    if files.is_empty() {
        return Output { text: format!("{title}: nothing found.\n"), cards: vec![] };
    }
    Output { text: format!("{title}:\n{}", file_lines(&files, 15)), cards: vec![Card::Files { title, files, gather }] }
}

fn proposal_output(a: Action) -> Output {
    Output {
        text: format!("Proposed \"{}\" ({} step(s)). It is shown to the person, who presses \"Do it\" to make the change; nothing has changed yet.", a.title, a.steps.len()),
        cards: vec![Card::Proposal { action: a }],
    }
}

fn activities_output(db: &Db, start: i64, end: i64) -> Result<Output> {
    let mut out = Output::default();
    let acts = db.activities(start, end, home().as_deref())?;
    for a in acts.iter().filter(|a| a.files.len() >= 2).take(6) {
        let files: Vec<FileItem> = a.files.iter().map(|t| FileItem::new(&t.path, t.last, kinds_note(&t.kinds))).collect();
        out.text.push_str(&format!("Piece of work \"{}\" ({} to {}):\n{}", a.label, date_label(a.start), date_label(a.end), file_lines(&files, 8)));
        out.cards.push(Card::Work {
            kind: "activity".into(),
            title: a.label.clone(),
            subtitle: format!("{} files", files.len()),
            start: a.start,
            end: a.end,
            gather: a.topic.clone(),
            files,
        });
    }
    if out.cards.is_empty() {
        out.text.push_str("No pieces of work with several files in that time.\n");
    }
    Ok(out)
}

fn file_info(db: &Db, path: &str) -> Result<Output> {
    let p = Path::new(path);
    let mut text = format!("{path}\n");
    if !p.exists() {
        text.push_str("It is no longer there.\n");
    }
    let (first, last, n): (Option<i64>, Option<i64>, i64) = db.conn.query_row(
        "SELECT min(e.ts), max(e.ts), count(*) FROM events e JOIN files f ON f.id = e.file_id WHERE f.path = ?1",
        [path],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if let (Some(a), Some(b)) = (first, last) {
        text.push_str(&format!("First seen {}, last used {}, {n} recorded events.\n", date_label(a), date_label(b)));
    } else {
        text.push_str("MemStor has no history for it.\n");
    }
    if let Some(d) = db.description(path)? {
        text.push_str(&format!("Description: {}\n", d.search_text().chars().take(600).collect::<String>()));
    }
    if let Some(m) = db.memories()?.into_iter().find(|m| m.files.iter().any(|f| f == path)) {
        text.push_str(&format!("Part of the piece of work \"{}\" ({} to {}).\n", m.label, date_label(m.start), date_label(m.end)));
    }
    Ok(Output { text, cards: vec![Card::Files { title: "File".into(), files: vec![FileItem::new(path, last.unwrap_or(0), "")], gather: None }] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Kind;

    #[test]
    fn every_skill_has_a_strict_schema() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), all().len());
        for d in defs {
            let s = &d["input_schema"];
            assert_eq!(s["type"], "object");
            assert_eq!(s["additionalProperties"], false);
            let props = s["properties"].as_object().unwrap();
            let req: Vec<&str> = s["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
            // Strict tools need every property listed as required.
            assert_eq!(props.len(), req.len(), "{}", d["name"]);
        }
    }

    #[test]
    fn look_skills_read_the_records() {
        let db = Db::in_memory().unwrap();
        let now = Local::now().timestamp();
        db.record("/u/Docs/thesis chapter 3.docx", Kind::Modified, "watcher", now - 60).unwrap();
        db.record("/u/Docs/thesis refs.bib", Kind::Created, "watcher", now - 120).unwrap();
        let out = run(&db, "find_files", &json!({"words": "thesis"})).unwrap();
        assert!(out.text.contains("thesis chapter 3.docx") && out.text.contains("edited"), "{}", out.text);
        assert!(matches!(&out.cards[0], Card::Files { files, .. } if files.len() == 2));
        let out = run(&db, "files_on_day", &json!({"day": "today"})).unwrap();
        assert!(out.text.contains("Files used today"));
        assert!(run(&db, "files_on_day", &json!({"day": "Diwali"})).is_err());
        assert!(run(&db, "no_such_skill", &json!({})).is_err());
        let out = run(&db, "find_files", &json!({"words": "zebra"})).unwrap();
        assert!(out.cards.is_empty() && out.text.contains("nothing found"));
    }

    #[test]
    fn change_skills_only_propose() {
        let dir = crate::test_dir("skills");
        let db = Db::in_memory().unwrap();
        let a = dir.join("IMG_2041.jpg");
        std::fs::write(&a, b"x").unwrap();
        let out = run(&db, "rename_file", &json!({"path": a.display().to_string(), "new_name": "alumni stage"})).unwrap();
        let Card::Proposal { action } = &out.cards[0] else { panic!("expected a proposal") };
        assert_eq!(action.steps, vec!["Rename IMG_2041.jpg to alumni stage.jpg".to_string()]);
        assert!(a.exists(), "nothing moves until the person agrees");
        let to = dir.join("Alumni");
        let out = run(&db, "move_files", &json!({"paths": [a.display().to_string()], "to_folder": to.display().to_string(), "make_folder": true, "title": "Alumni photos"})).unwrap();
        let Card::Proposal { action } = &out.cards[0] else { panic!() };
        assert_eq!(action.steps.len(), 2);
        assert!(run(&db, "move_files", &json!({"paths": [a.display().to_string()], "to_folder": to.display().to_string(), "make_folder": false, "title": ""})).is_err());
        let out = run(&db, "open_file", &json!({"path": a.display().to_string(), "show_in_folder": false})).unwrap();
        assert!(matches!(&out.cards[0], Card::Open { reveal: false, .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
