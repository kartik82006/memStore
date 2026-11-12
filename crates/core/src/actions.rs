//! File changes the assistant can make: rename, move, and new folders. Every change is
//! first stored as a proposal the user sees in full; it runs only when they press the
//! button, and it can be undone for 30 days. Nothing is ever deleted or overwritten, and
//! Windows, program and app-data folders are off limits.

use crate::db::Db;
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS actions (
    id       INTEGER PRIMARY KEY,
    created  INTEGER NOT NULL,
    title    TEXT NOT NULL,
    ops      TEXT NOT NULL,                 -- JSON list of Op
    status   TEXT NOT NULL,                 -- proposed | done | undone | failed
    done_at  INTEGER,
    note     TEXT
);
"#;

/// How long a change can be undone.
pub const UNDO_DAYS: i64 = 30;
/// Most files one proposal may touch.
pub const MAX_OPS: usize = 200;

/// One step of a change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Make a new, empty folder.
    MakeFolder { path: String },
    /// Rename a file or folder in place.
    Rename { from: String, to: String },
    /// Move a file into another folder, keeping its name.
    Move { from: String, to: String },
}

impl Op {
    /// The step in plain words, for the preview.
    pub fn describe(&self) -> String {
        let name = |p: &str| Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string());
        let folder = |p: &str| Path::new(p).parent().map(|d| d.display().to_string()).unwrap_or_default();
        match self {
            Op::MakeFolder { path } => format!("New folder {path}"),
            Op::Rename { from, to } => format!("Rename {} to {}", name(from), name(to)),
            Op::Move { from, to } => format!("Move {} to {}", name(from), folder(to)),
        }
    }
}

/// A change waiting for the user, or already made.
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub id: i64,
    pub title: String,
    pub steps: Vec<String>,
    pub status: String,
    pub created: i64,
    pub can_undo: bool,
}

fn now() -> i64 {
    chrono::Local::now().timestamp()
}

/// True for places MemStor must never change: Windows itself, installed programs, app data,
/// the recycle bin, and MemStor's own folder.
pub fn is_protected(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        return true; // a drive or the root itself
    }
    const TOP: &[&str] = &["windows", "program files", "program files (x86)", "programdata", "$recycle.bin", "system volume information", "recovery", "boot"];
    // Windows paths start with a drive, so the first normal part is the top folder.
    if cfg!(windows) && TOP.contains(&parts[0].as_str()) {
        return true;
    }
    if parts.iter().any(|p| p == "appdata" || p == "$recycle.bin") {
        return true;
    }
    if cfg!(not(windows)) && ["usr", "bin", "sbin", "etc", "lib", "var", "proc", "sys", "boot", "opt"].contains(&parts[0].as_str()) {
        return true;
    }
    let own = crate::default_db_path();
    own.parent().is_some_and(|d| path.starts_with(d))
}

fn check_path(p: &str) -> Result<PathBuf> {
    let path = PathBuf::from(p);
    ensure!(path.is_absolute(), "{p} is not a full path");
    ensure!(!path.components().any(|c| matches!(c, Component::ParentDir)), "{p} has .. in it");
    ensure!(!is_protected(&path), "MemStor doesn't change files in {p}: it's a Windows, program or app folder.");
    Ok(path)
}

/// Characters Windows doesn't allow in names, and names it reserves.
fn check_name(name: &str) -> Result<()> {
    ensure!(!name.trim().is_empty(), "The new name is empty.");
    ensure!(
        !name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || c.is_control()),
        "\"{name}\" has a character Windows doesn't allow in names."
    );
    ensure!(!name.ends_with('.') && !name.ends_with(' '), "Names can't end with a dot or a space.");
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    const RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "LPT1", "LPT2", "LPT3"];
    ensure!(!RESERVED.contains(&stem.as_str()), "\"{name}\" is a name Windows reserves.");
    Ok(())
}

/// The new name for a rename: keeps the old extension if the new name has none.
pub fn renamed_path(from: &Path, new_name: &str) -> Result<PathBuf> {
    let new_name = new_name.trim();
    check_name(new_name)?;
    let mut name = new_name.to_string();
    if let Some(ext) = from.extension().filter(|_| from.is_file()) {
        if Path::new(new_name).extension().is_none() {
            name = format!("{new_name}.{}", ext.to_string_lossy());
        }
    }
    Ok(from.with_file_name(name))
}

/// Check every step against the disk as it is now, without changing anything.
pub fn validate(ops: &[Op]) -> Result<()> {
    ensure!(!ops.is_empty(), "There is nothing to change.");
    ensure!(ops.len() <= MAX_OPS, "That's {} changes; MemStor does at most {MAX_OPS} at a time.", ops.len());
    let mut new_folders: Vec<PathBuf> = Vec::new();
    let mut taken: Vec<PathBuf> = Vec::new();
    let mut sources: Vec<PathBuf> = Vec::new();
    let free = |p: &Path, taken: &[PathBuf]| !p.exists() && !taken.iter().any(|t| same(t, p));
    for op in ops {
        match op {
            Op::MakeFolder { path } => {
                let p = check_path(path)?;
                if p.is_dir() {
                    continue;
                }
                ensure!(free(&p, &taken), "{path} already exists.");
                let parent = p.parent().context("no parent folder")?;
                ensure!(parent.is_dir() || new_folders.iter().any(|f| same(f, parent)), "The folder {} doesn't exist.", parent.display());
                new_folders.push(p.clone());
                taken.push(p);
            }
            Op::Rename { from, to } | Op::Move { from, to } => {
                let f = check_path(from)?;
                let t = check_path(to)?;
                ensure!(f.exists(), "{from} isn't there any more.");
                ensure!(!sources.iter().any(|s| same(s, &f)), "{from} is changed twice.");
                if let Op::Move { .. } = op {
                    ensure!(f.is_file(), "Only files can be moved, and {from} is a folder.");
                }
                if same(&f, &t) {
                    bail!("{from} is already there.");
                }
                ensure!(free(&t, &taken), "Something called {} is already in {}. MemStor never overwrites.", t.file_name().unwrap_or_default().to_string_lossy(), t.parent().map(|p| p.display().to_string()).unwrap_or_default());
                let parent = t.parent().context("no parent folder")?;
                ensure!(parent.is_dir() || new_folders.iter().any(|n| same(n, parent)), "The folder {} doesn't exist.", parent.display());
                sources.push(f);
                taken.push(t);
            }
        }
    }
    Ok(())
}

fn same(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// Move one file or folder. Across drives, a file is copied (keeping its date) and the
/// original removed only once the copy is complete.
fn move_path(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    ensure!(from.is_file(), "could not move {}", from.display());
    let modified = std::fs::metadata(from)?.modified().ok();
    std::fs::copy(from, to).with_context(|| format!("could not copy {} to {}", from.display(), to.display()))?;
    if let Some(m) = modified {
        let _ = std::fs::File::options().write(true).open(to).and_then(|f| f.set_modified(m));
    }
    ensure!(std::fs::metadata(to)?.len() == std::fs::metadata(from)?.len(), "the copy of {} is incomplete", from.display());
    std::fs::remove_file(from).with_context(|| format!("copied, but could not remove {}", from.display()))?;
    Ok(())
}

/// Do one step. Returns the step that reverses it.
fn apply_op(op: &Op) -> Result<Op> {
    Ok(match op {
        Op::MakeFolder { path } => {
            std::fs::create_dir(path).with_context(|| format!("could not make {path}"))?;
            op.clone()
        }
        Op::Rename { from, to } => {
            move_path(Path::new(from), Path::new(to))?;
            Op::Rename { from: to.clone(), to: from.clone() }
        }
        Op::Move { from, to } => {
            move_path(Path::new(from), Path::new(to))?;
            Op::Move { from: to.clone(), to: from.clone() }
        }
    })
}

/// Run a reversing step (from `apply_op`). A new folder is removed only when it's empty again.
fn reverse_op(done: &Op) -> Result<()> {
    match done {
        Op::MakeFolder { path } => {
            let p = Path::new(path);
            if p.is_dir() && std::fs::read_dir(p)?.next().is_none() {
                std::fs::remove_dir(p)?;
            }
            Ok(())
        }
        Op::Rename { from, to } | Op::Move { from, to } => {
            // A reversing step: the file is now at `from` and goes back to `to`.
            ensure!(Path::new(from).exists(), "{from} isn't there any more, so it can't be put back.");
            ensure!(!Path::new(to).exists(), "Something new is at {to}, so MemStor won't overwrite it.");
            move_path(Path::new(from), Path::new(to))
        }
    }
}

impl Db {
    /// Store a change for the user to look at. Nothing on disk changes yet.
    pub fn propose(&self, title: &str, ops: Vec<Op>) -> Result<Action> {
        validate(&ops)?;
        self.conn.execute(
            "INSERT INTO actions (created, title, ops, status) VALUES (?1, ?2, ?3, 'proposed')",
            params![now(), title, serde_json::to_string(&ops)?],
        )?;
        self.action(self.conn.last_insert_rowid())
    }

    pub fn action(&self, id: i64) -> Result<Action> {
        let (title, ops, status, created, done_at): (String, String, String, i64, Option<i64>) = self
            .conn
            .query_row("SELECT title, ops, status, created, done_at FROM actions WHERE id = ?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?
            .context("That change is no longer available.")?;
        let ops: Vec<Op> = serde_json::from_str(&ops)?;
        let can_undo = status == "done" && done_at.is_some_and(|t| now() - t < UNDO_DAYS * 86_400);
        Ok(Action { id, title, steps: ops.iter().map(Op::describe).collect(), status, created, can_undo })
    }

    fn action_ops(&self, id: i64) -> Result<(String, Vec<Op>)> {
        let (status, ops): (String, String) =
            self.conn.query_row("SELECT status, ops FROM actions WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?.context("That change is no longer available.")?;
        Ok((status, serde_json::from_str(&ops)?))
    }

    /// Make a proposed change. Checks again first; if a step fails, the steps already done
    /// are put back. File history follows renamed and moved files.
    pub fn apply_action(&self, id: i64) -> Result<Action> {
        let (status, ops) = self.action_ops(id)?;
        ensure!(status == "proposed", "That change was already {}.", if status == "done" { "made" } else { status.as_str() });
        validate(&ops)?;
        let mut undo: Vec<Op> = Vec::new();
        for op in &ops {
            match apply_op(op) {
                Ok(rev) => undo.push(rev),
                Err(e) => {
                    for r in undo.iter().rev() {
                        let _ = reverse_op(r);
                    }
                    self.conn.execute("UPDATE actions SET status = 'failed', note = ?2 WHERE id = ?1", params![id, e.to_string()])?;
                    return Err(e.context("Nothing was changed"));
                }
            }
        }
        let ts = now();
        for op in &ops {
            if let Op::Rename { from, to } | Op::Move { from, to } = op {
                self.rename(from, to, "memstor", ts)?;
            }
        }
        // Store the reversing steps, newest first, so undo is just "run these".
        let reverse: Vec<Op> = undo.into_iter().rev().collect();
        self.conn.execute(
            "UPDATE actions SET status = 'done', done_at = ?2, note = ?3 WHERE id = ?1",
            params![id, ts, serde_json::to_string(&reverse)?],
        )?;
        self.action(id)
    }

    /// Put things back the way they were before a change.
    pub fn undo_action(&self, id: i64) -> Result<Action> {
        let a = self.action(id)?;
        ensure!(a.status == "done", "Only a change that was made can be undone.");
        ensure!(a.can_undo, "Changes can be undone for {UNDO_DAYS} days, and this one is older.");
        let note: String = self.conn.query_row("SELECT note FROM actions WHERE id = ?1", [id], |r| r.get(0))?;
        let reverse: Vec<Op> = serde_json::from_str(&note)?;
        let mut problems = Vec::new();
        let ts = now();
        for r in &reverse {
            match reverse_op(r) {
                Ok(()) => {
                    if let Op::Rename { from, to } | Op::Move { from, to } = r {
                        self.rename(from, to, "memstor", ts)?;
                    }
                }
                Err(e) => problems.push(e.to_string()),
            }
        }
        self.conn.execute("UPDATE actions SET status = 'undone' WHERE id = ?1", [id])?;
        ensure!(problems.is_empty(), "Undone, except: {}", problems.join("; "));
        self.action(id)
    }

    /// Recent changes, newest first, for the "Changes" list.
    pub fn recent_actions(&self, limit: usize) -> Result<Vec<Action>> {
        let ids: Vec<i64> = {
            let mut s = self.conn.prepare("SELECT id FROM actions WHERE status IN ('done', 'undone') ORDER BY id DESC LIMIT ?1")?;
            s.query_map([limit as i64], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        ids.into_iter().map(|id| self.action(id)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(p: &Path) -> String {
        p.display().to_string()
    }

    #[test]
    fn rename_move_and_undo() {
        let dir = crate::test_dir("actions");
        let db = Db::in_memory().unwrap();
        let a = dir.join("draft.docx");
        std::fs::write(&a, b"hello").unwrap();
        db.record(&s(&a), crate::db::Kind::Created, "watcher", 100).unwrap();

        // Rename keeps the extension when the new name has none.
        let to = renamed_path(&a, "Final report").unwrap();
        assert_eq!(to.file_name().unwrap(), "Final report.docx");
        let new_folder = dir.join("Reports");
        let moved = new_folder.join("Final report.docx");
        let p = db
            .propose("Tidy the report", vec![
                Op::Rename { from: s(&a), to: s(&to) },
                Op::MakeFolder { path: s(&new_folder) },
                Op::Move { from: s(&to), to: s(&moved) },
            ])
            .unwrap_err();
        // The same file can't be changed twice in one go.
        assert!(p.to_string().contains("changed twice") || p.to_string().contains("isn't there"), "{p}");

        let p = db.propose("Rename", vec![Op::Rename { from: s(&a), to: s(&to) }]).unwrap();
        assert_eq!(p.status, "proposed");
        assert!(a.exists(), "nothing changes before the user agrees");
        let done = db.apply_action(p.id).unwrap();
        assert!(done.can_undo && to.exists() && !a.exists());
        // History follows the file.
        assert!(db.search("Final report", 5).unwrap().iter().any(|h| h.path == s(&to)));
        assert!(db.apply_action(p.id).is_err(), "a change runs once");

        let m = db.propose("Move", vec![Op::MakeFolder { path: s(&new_folder) }, Op::Move { from: s(&to), to: s(&moved) }]).unwrap();
        db.apply_action(m.id).unwrap();
        assert!(moved.exists() && !to.exists());

        db.undo_action(m.id).unwrap();
        assert!(to.exists() && !moved.exists() && !new_folder.exists(), "undo puts it back and removes the empty folder");
        db.undo_action(p.id).unwrap();
        assert!(a.exists() && !to.exists());
        assert_eq!(std::fs::read(&a).unwrap(), b"hello");
        assert_eq!(db.recent_actions(5).unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn never_overwrites_or_touches_system_folders() {
        let dir = crate::test_dir("actions-safe");
        let db = Db::in_memory().unwrap();
        let a = dir.join("a.txt");
        let b = dir.join("b.txt");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let e = db.propose("x", vec![Op::Rename { from: s(&a), to: s(&b) }]).unwrap_err();
        assert!(e.to_string().contains("never overwrites"), "{e}");
        assert!(check_name("a:b").is_err() && check_name("CON.txt").is_err() && check_name("ok name").is_ok());
        let sys = if cfg!(windows) { "C:\\Windows\\notepad.exe" } else { "/usr/bin/env" };
        assert!(is_protected(Path::new(sys)));
        assert!(is_protected(Path::new(if cfg!(windows) { "C:\\Users\\me\\AppData\\Local\\x.txt" } else { "/home/me/AppData/x.txt" })));
        assert!(!is_protected(&a));
        assert!(db.propose("x", vec![Op::Move { from: sys.into(), to: s(&dir.join("env")) }]).is_err());
        // A step that fails puts the earlier ones back.
        let c = dir.join("c.txt");
        let p = db.propose("two", vec![Op::Rename { from: s(&a), to: s(&c) }, Op::Rename { from: s(&b), to: s(&dir.join("d.txt")) }]).unwrap();
        std::fs::remove_file(&b).unwrap(); // gone before the user pressed the button
        assert!(db.apply_action(p.id).is_err());
        assert!(a.exists() && !c.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
