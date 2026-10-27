//! MemStor M0: a command-line test of the background collector.
//! It records which files you create, edit, rename and open, and lists them by day.

use memstor_core::{db, recent, watch};
use anyhow::{Context, Result, bail};
use chrono::{Local, NaiveDate, TimeZone};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "memstor", about = "Remembers which files you worked on, and when.")]
struct Cli {
    /// Database file. Defaults to %LOCALAPPDATA%\MemStor\memstor.db
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start recording. Watches your user folder (minus AppData) and other fixed drives unless you name folders.
    Watch {
        /// A folder to watch (repeat for more)
        #[arg(long = "folder")]
        folders: Vec<PathBuf>,
        /// Don't read Windows' Recent Items list
        #[arg(long)]
        no_recent: bool,
    },
    /// List files you touched today
    Today,
    /// List files you touched on a given day (YYYY-MM-DD)
    Day { date: String },
    /// Find files by words in their name or folder
    Search { words: Vec<String> },
    /// List projects with their folders
    Projects,
    /// Create a project for a folder
    ProjectAdd {
        name: String,
        #[arg(long)]
        folder: PathBuf,
    },
    /// List a project's files for a day (default today), including files used around the same time
    ProjectFiles {
        name: String,
        /// YYYY-MM-DD
        #[arg(long)]
        date: Option<String>,
    },
    /// Put shortcuts to a project's files for a day into Documents\MemStor Gathered
    Gather {
        name: String,
        /// YYYY-MM-DD
        #[arg(long)]
        date: Option<String>,
    },
    /// Import Windows' Recent Items once
    ScanRecent {
        /// Folder of .lnk shortcuts (defaults to the Windows Recent folder)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

fn day_bounds(date: NaiveDate) -> Result<(i64, i64)> {
    let start = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .context("invalid local date")?;
    Ok((start.timestamp(), (start + chrono::Duration::days(1)).timestamp()))
}

fn parse_date(date: Option<String>) -> Result<NaiveDate> {
    match date {
        Some(d) => NaiveDate::parse_from_str(&d, "%Y-%m-%d").context("use the form YYYY-MM-DD"),
        None => Ok(Local::now().date_naive()),
    }
}

fn find_project(db: &db::Db, name: &str) -> Result<memstor_core::projects::Project> {
    db.projects()?
        .into_iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
        .with_context(|| format!("no project called \"{name}\"; see `memstor projects`"))
}

fn print_day(db: &db::Db, date: NaiveDate) -> Result<()> {
    let (start, end) = day_bounds(date)?;
    let rows = db.touched_between(start, end)?;
    if rows.is_empty() {
        println!("No files recorded on {date}.");
        return Ok(());
    }
    println!("{} file(s) on {date}:\n", rows.len());
    let hm = |ts: i64| Local.timestamp_opt(ts, 0).single().map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
    for r in rows {
        println!("  {}–{}  {:>3}×  {:<24} {}", hm(r.first), hm(r.last), r.events, r.kinds, r.path);
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = cli.db.unwrap_or_else(memstor_core::default_db_path);
    let db = db::Db::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;

    match cli.cmd {
        Cmd::Watch { folders, no_recent } => {
            let folders = if folders.is_empty() { watch::default_folders() } else { folders };
            if folders.is_empty() {
                bail!("no folders to watch; pass --folder <path>");
            }
            let recent_dir = if no_recent { None } else { recent::default_dir().filter(|d| d.is_dir()) };
            println!("database: {}", db_path.display());
            println!("recording… press Ctrl+C to stop");
            watch::run(&db, &folders, recent_dir, &watch::Control::default())
        }
        Cmd::Today => print_day(&db, Local::now().date_naive()),
        Cmd::Day { date } => {
            let d = NaiveDate::parse_from_str(&date, "%Y-%m-%d").context("use the form YYYY-MM-DD")?;
            print_day(&db, d)
        }
        Cmd::Search { words } => {
            let hits = db.search(&words.join(" "), 30)?;
            if hits.is_empty() {
                println!("Nothing found.");
            }
            for h in hits {
                let (path, last) = (h.path, h.last_seen);
                let when = Local.timestamp_opt(last, 0).single().map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
                println!("  {when}  {:<9} {path}", h.last_kind);
            }
            Ok(())
        }
        Cmd::Projects => {
            let list = db.projects()?;
            if list.is_empty() {
                println!("No projects yet. Create one with `memstor project-add <name> --folder <path>`.");
            }
            for p in list {
                println!("{}", p.name);
                for f in p.folders {
                    println!("    {f}");
                }
            }
            Ok(())
        }
        Cmd::ProjectAdd { name, folder } => {
            let id = db.create_project(&name)?;
            db.add_project_folder(id, &folder.to_string_lossy())?;
            println!("Created project \"{}\" for {}", name.trim(), folder.display());
            Ok(())
        }
        Cmd::ProjectFiles { name, date } => {
            let p = find_project(&db, &name)?;
            let date = parse_date(date)?;
            let (start, end) = day_bounds(date)?;
            let files = db.project_files(p.id, start, end, memstor_core::projects::DEFAULT_NEARBY_MINUTES)?;
            println!("{} file(s) for {} on {date}:\n", files.len(), p.name);
            for f in files {
                let tag = if f.reason == memstor_core::projects::Reason::Nearby { "same time" } else { "in folder" };
                println!("  {tag:<10} {}", f.file.path);
            }
            Ok(())
        }
        Cmd::Gather { name, date } => {
            let p = find_project(&db, &name)?;
            let date = parse_date(date)?;
            let (start, end) = day_bounds(date)?;
            let paths: Vec<String> = db
                .project_files(p.id, start, end, memstor_core::projects::DEFAULT_NEARBY_MINUTES)?
                .into_iter()
                .map(|f| f.file.path)
                .collect();
            if paths.is_empty() {
                bail!("no files used for {} on {date}", p.name);
            }
            let g = memstor_core::gather::gather(&paths, &memstor_core::gather::default_root(), &format!("{} {date}", p.name))?;
            println!("Made {} shortcut(s) in {}", g.linked, g.folder.display());
            if !g.missing.is_empty() {
                println!("Skipped {} file(s) that no longer exist.", g.missing.len());
            }
            Ok(())
        }
        Cmd::ScanRecent { dir } => {
            let dir = dir.or_else(recent::default_dir).context("no Recent Items folder on this system; pass --dir")?;
            let n = recent::scan(&dir, &db)?;
            println!("Imported {n} new opened file(s) from {}", dir.display());
            Ok(())
        }
    }
}
