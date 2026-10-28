//! MemStor tray app. A background thread records file activity; a hidden window
//! opens on Ctrl+Shift+Space (or from the tray) to search it.

// No console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::{Duration, Local, TimeZone};
use memstor_core::db::Db;
use memstor_core::cloud::{self, Worker};
use memstor_core::watch::{self, Control};
use memstor_core::projects::{DEFAULT_NEARBY_MINUTES, Reason};
use memstor_core::{default_db_path, gather, recent};
use tauri_plugin_dialog::DialogExt;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent, Wry};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt;

const HOTKEY_LABEL: &str = "Ctrl+Shift+Space";
/// Meta key set once start-with-Windows has been turned on the first time.
const AUTOSTART_SET_KEY: &str = "autostart_set";

struct AppState {
    /// The window's own connection; the collector thread has a separate one.
    db: Mutex<Db>,
    /// The running collector; replaced when the watched folders change.
    collector: Mutex<(Arc<Control>, Vec<PathBuf>)>,
    /// The cloud describer; restarted with the collector.
    cloud: Mutex<Arc<Worker>>,
    db_path: PathBuf,
    pause_item: MenuItem<Wry>,
    hotkey_ok: Mutex<bool>,
}

impl AppState {
    fn control(&self) -> Arc<Control> {
        self.collector.lock().map(|c| c.0.clone()).unwrap_or_default()
    }
    fn folders(&self) -> Vec<PathBuf> {
        self.collector.lock().map(|c| c.1.clone()).unwrap_or_default()
    }
    fn worker(&self) -> Arc<Worker> {
        self.cloud.lock().map(|w| w.clone()).unwrap_or_default()
    }
}

#[derive(Serialize)]
struct FileHit {
    path: String,
    name: String,
    folder: String,
    when: i64,
    /// What last happened: created, modified, renamed or opened.
    kind: String,
}

#[derive(Serialize)]
struct DayRow {
    path: String,
    name: String,
    folder: String,
    first: i64,
    last: i64,
    events: i64,
    kinds: Vec<String>,
}

#[derive(Serialize)]
struct Status {
    paused: bool,
    db_path: String,
    folders: Vec<String>,
    hotkey: Option<String>,
}

fn split(path: &str) -> (String, String) {
    let p = Path::new(path);
    let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string());
    let folder = p.parent().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    (name, folder)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Empty query lists recent files; otherwise full-text search on names and folders.
#[tauri::command]
fn search(query: String, state: State<AppState>) -> Result<Vec<FileHit>, String> {
    let db = state.db.lock().map_err(err)?;
    let rows = if query.trim().is_empty() { db.recent(60) } else { db.search(&query, 60) }.map_err(err)?;
    Ok(rows
        .into_iter()
        .map(|h| {
            let (name, folder) = split(&h.path);
            FileHit { name, folder, when: h.last_seen, kind: h.last_kind, path: h.path }
        })
        .collect())
}

/// Files touched on a day: 0 = today, 1 = yesterday, and so on.
#[tauri::command]
fn day(days_ago: i64, state: State<AppState>) -> Result<Vec<DayRow>, String> {
    let date = Local::now().date_naive() - Duration::days(days_ago);
    let start = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .ok_or("invalid local date")?;
    let end = start + Duration::days(1);
    let db = state.db.lock().map_err(err)?;
    let rows = db.touched_between(start.timestamp(), end.timestamp()).map_err(err)?;
    Ok(rows
        .into_iter()
        .map(to_day_row)
        .collect())
}

#[tauri::command]
fn open_file(path: String, app: AppHandle) -> Result<(), String> {
    if !Path::new(&path).exists() {
        return Err(format!("{path} is no longer there. It may have been moved or deleted."));
    }
    app.opener().open_path(&path, None::<&str>).map_err(err)
}

#[tauri::command]
fn show_in_folder(path: String, app: AppHandle) -> Result<(), String> {
    let p = Path::new(&path);
    if p.exists() {
        app.opener().reveal_item_in_dir(p).map_err(err)
    } else if let Some(parent) = p.parent().filter(|d| d.is_dir()) {
        app.opener().open_path(parent.to_string_lossy(), None::<&str>).map_err(err)
    } else {
        Err(format!("The folder for {path} no longer exists."))
    }
}

#[tauri::command]
fn status(state: State<AppState>) -> Status {
    Status {
        paused: state.control().is_paused(),
        db_path: state.db_path.display().to_string(),
        folders: state.folders().iter().map(|f| f.display().to_string()).collect(),
        hotkey: state.hotkey_ok.lock().ok().filter(|ok| **ok).map(|_| HOTKEY_LABEL.to_string()),
    }
}

#[tauri::command]
fn set_paused(paused: bool, app: AppHandle) {
    apply_paused(&app, paused);
}

#[tauri::command]
fn hide_window(app: AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}


// ---------- projects ----------

#[derive(Serialize)]
struct ProjectDto {
    id: i64,
    name: String,
    folders: Vec<String>,
    /// Files used today (in its folders or around the same time).
    today: usize,
}

#[derive(Serialize)]
struct ProjectFileDto {
    #[serde(flatten)]
    row: DayRow,
    /// "folder" when inside the project's folders, "nearby" when used around the same time.
    reason: &'static str,
}

#[derive(Serialize)]
struct SuggestionDto {
    folder: String,
    name: String,
    files: i64,
}

#[derive(Serialize)]
struct GatherDto {
    folder: String,
    linked: usize,
    missing: usize,
}

/// [start, end) in unix seconds for "today", "yesterday" or "week" (last 7 days incl. today).
fn range_bounds(range: &str) -> Result<(i64, i64, String), String> {
    let today = Local::now().date_naive();
    let (first, days, label) = match range {
        "today" => (today, 1, today.format("%Y-%m-%d").to_string()),
        "yesterday" => {
            let y = today - Duration::days(1);
            (y, 1, y.format("%Y-%m-%d").to_string())
        }
        "week" => (today - Duration::days(6), 7, format!("week to {}", today.format("%Y-%m-%d"))),
        other => return Err(format!("unknown range {other}")),
    };
    let start = Local
        .from_local_datetime(&first.and_hms_opt(0, 0, 0).unwrap())
        .earliest()
        .ok_or("invalid local date")?;
    Ok((start.timestamp(), (start + Duration::days(days)).timestamp(), label))
}

fn to_day_row(t: memstor_core::db::Touched) -> DayRow {
    let (name, folder) = split(&t.path);
    DayRow {
        name,
        folder,
        first: t.first,
        last: t.last,
        events: t.events,
        kinds: t.kinds.split(',').map(str::to_string).collect(),
        path: t.path,
    }
}

#[tauri::command]
fn list_projects(state: State<AppState>) -> Result<Vec<ProjectDto>, String> {
    let db = state.db.lock().map_err(err)?;
    let (start, end, _) = range_bounds("today")?;
    db.projects()
        .map_err(err)?
        .into_iter()
        .map(|p| {
            let today = db.project_files(p.id, start, end, DEFAULT_NEARBY_MINUTES).map_err(err)?.len();
            Ok(ProjectDto { id: p.id, name: p.name, folders: p.folders, today })
        })
        .collect()
}

#[tauri::command]
fn create_project(name: String, folder: Option<String>, state: State<AppState>) -> Result<i64, String> {
    let db = state.db.lock().map_err(err)?;
    let id = db.create_project(&name).map_err(err)?;
    if let Some(f) = folder {
        db.add_project_folder(id, &f).map_err(err)?;
    }
    Ok(id)
}

#[tauri::command]
fn delete_project(id: i64, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.delete_project(id).map_err(err)
}

#[tauri::command]
fn add_project_folder(id: i64, folder: String, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.add_project_folder(id, &folder).map_err(err)
}

#[tauri::command]
fn remove_project_folder(id: i64, folder: String, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.remove_project_folder(id, &folder).map_err(err)
}

/// Opens Windows' folder picker. Async so it runs off the main thread.
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    app.dialog().file().set_title("Choose a project folder").blocking_pick_folder().map(|p| p.to_string())
}

#[tauri::command]
fn project_files(id: i64, range: String, state: State<AppState>) -> Result<Vec<ProjectFileDto>, String> {
    let (start, end, _) = range_bounds(&range)?;
    let db = state.db.lock().map_err(err)?;
    Ok(db
        .project_files(id, start, end, DEFAULT_NEARBY_MINUTES)
        .map_err(err)?
        .into_iter()
        .map(|f| ProjectFileDto {
            reason: if f.reason == Reason::InFolder { "folder" } else { "nearby" },
            row: to_day_row(f.file),
        })
        .collect())
}

/// Folders you worked in during the last 7 days that look like projects.
#[tauri::command]
fn suggest_projects(state: State<AppState>) -> Result<Vec<SuggestionDto>, String> {
    let (start, end, _) = range_bounds("week")?;
    let home = dirs_home();
    let db = state.db.lock().map_err(err)?;
    Ok(db
        .suggest_projects(start, end, home.as_deref(), 6)
        .map_err(err)?
        .into_iter()
        .map(|s| SuggestionDto { folder: s.folder, name: s.name, files: s.files })
        .collect())
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from)
}

/// Makes Documents\MemStor Gathered\<project> <date> with shortcuts to `paths`, then opens it.
#[tauri::command]
fn gather_files(id: i64, range: String, paths: Vec<String>, app: AppHandle, state: State<AppState>) -> Result<GatherDto, String> {
    if paths.is_empty() {
        return Err("Pick at least one file to gather.".into());
    }
    let (_, _, label) = range_bounds(&range)?;
    let name = {
        let db = state.db.lock().map_err(err)?;
        db.projects().map_err(err)?.into_iter().find(|p| p.id == id).map(|p| p.name).ok_or("That project no longer exists.")?
    };
    let g = gather::gather(&paths, &gather::default_root(), &format!("{name} {label}")).map_err(err)?;
    let folder = g.folder.display().to_string();
    let _ = app.opener().open_path(&folder, None::<&str>);
    Ok(GatherDto { folder, linked: g.linked, missing: g.missing.len() })
}

// ---------- activities ----------

#[derive(Serialize)]
struct ActivityDto {
    label: String,
    work: &'static str,
    topic: String,
    start: i64,
    end: i64,
    files: Vec<DayRow>,
}

#[tauri::command]
fn list_activities(range: String, state: State<AppState>) -> Result<Vec<ActivityDto>, String> {
    let (start, end, _) = range_bounds(&range)?;
    let db = state.db.lock().map_err(err)?;
    Ok(db
        .activities(start, end, dirs_home().as_deref())
        .map_err(err)?
        .into_iter()
        .map(|a| ActivityDto {
            label: a.label,
            work: a.work.as_str(),
            topic: a.topic,
            start: a.start,
            end: a.end,
            files: a.files.into_iter().map(to_day_row).collect(),
        })
        .collect())
}

/// The user says `path` does not belong with `others`; remember it.
#[tauri::command]
fn drop_from_activity(path: String, others: Vec<String>, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.mark_not_related(&path, &others).map_err(err)
}

#[tauri::command]
fn gather_activity(title: String, range: String, paths: Vec<String>, app: AppHandle) -> Result<GatherDto, String> {
    if paths.is_empty() {
        return Err("Pick at least one file to gather.".into());
    }
    let (_, _, label) = range_bounds(&range)?;
    let g = gather::gather(&paths, &gather::default_root(), &format!("{title} {label}")).map_err(err)?;
    let folder = g.folder.display().to_string();
    let _ = app.opener().open_path(&folder, None::<&str>);
    Ok(GatherDto { folder, linked: g.linked, missing: g.missing.len() })
}

/// Make a project from an activity: its name plus the specific folders its files are in.
#[tauri::command]
fn save_activity_as_project(name: String, paths: Vec<String>, state: State<AppState>) -> Result<i64, String> {
    let home = dirs_home();
    let mut folders: Vec<String> = paths
        .iter()
        .filter_map(|p| memstor_core::projects::project_folder_for(std::path::Path::new(p), home.as_deref()))
        .map(|f| f.display().to_string())
        .collect();
    folders.sort();
    folders.dedup();
    if folders.is_empty() {
        return Err("These files sit in general folders like Downloads, so there is no project folder to save. Use Gather instead.".into());
    }
    let db = state.db.lock().map_err(err)?;
    let id = db.create_project(&name).map_err(err)?;
    for f in &folders {
        db.add_project_folder(id, f).map_err(err)?;
    }
    Ok(id)
}

// ---------- memories ----------

#[derive(Serialize)]
struct MemoryFileDto {
    path: String,
    name: String,
    folder: String,
    exists: bool,
}

#[derive(Serialize)]
struct MemoryDto {
    id: i64,
    label: String,
    work: String,
    topic: String,
    start: i64,
    end: i64,
    active_minutes: i64,
    late_minutes: i64,
    days: i64,
    why: Vec<String>,
    /// A one-line story written by the cloud, if any.
    story: Option<String>,
    files: Vec<MemoryFileDto>,
}

fn to_memory_dto(db: &Db, m: memstor_core::memory::Memory, why: Vec<String>) -> MemoryDto {
    MemoryDto {
        story: db.memory_story(m.id).ok().flatten(),
        files: m
            .files
            .iter()
            .map(|p| {
                let (name, folder) = split(p);
                MemoryFileDto { exists: Path::new(p).exists(), path: p.clone(), name, folder }
            })
            .collect(),
        id: m.id,
        label: m.label,
        work: m.work,
        topic: m.topic,
        start: m.start,
        end: m.end,
        active_minutes: m.active_minutes,
        late_minutes: m.late_minutes,
        days: m.days,
        why,
    }
}

#[derive(Serialize)]
struct RecallDto {
    /// What MemStor understood from the question, as short phrases.
    understood: Vec<String>,
    results: Vec<MemoryDto>,
}

#[tauri::command]
fn recall(query: String, state: State<AppState>) -> Result<RecallDto, String> {
    let db = state.db.lock().map_err(err)?;
    let (q, found) = db.recall(&query, 8).map_err(err)?;
    let mut understood = Vec::new();
    if let Some(l) = q.range_label {
        understood.push(l);
    }
    understood.extend(q.kinds.iter().map(|k| memstor_core::memory::work_label(k).to_string()));
    if q.rush {
        understood.push("busy, on a deadline".into());
    }
    if q.late {
        understood.push("late at night".into());
    }
    understood.extend(q.words.iter().map(|w| format!("\"{w}\"")));
    Ok(RecallDto { understood, results: found.into_iter().map(|r| to_memory_dto(&db, r.memory, r.why)).collect() })
}

#[tauri::command]
fn list_memories(state: State<AppState>) -> Result<Vec<MemoryDto>, String> {
    let db = state.db.lock().map_err(err)?;
    Ok(db.memories().map_err(err)?.into_iter().map(|m| to_memory_dto(&db, m, Vec::new())).collect())
}

#[tauri::command]
fn rename_memory(id: i64, name: String, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.rename_memory(id, &name).map_err(err)
}

#[tauri::command]
fn forget_memory(id: i64, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.forget_memory(id).map_err(err)
}

#[tauri::command]
fn gather_memory(id: i64, app: AppHandle, state: State<AppState>) -> Result<GatherDto, String> {
    let m = {
        let db = state.db.lock().map_err(err)?;
        db.memories().map_err(err)?.into_iter().find(|m| m.id == id).ok_or("That memory no longer exists.")?
    };
    let day = |ts: i64| Local.timestamp_opt(ts, 0).single().map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
    let title = if day(m.start) == day(m.end) { format!("{} {}", m.topic, day(m.start)) } else { format!("{} {} to {}", m.topic, day(m.start), day(m.end)) };
    let g = gather::gather(&m.files, &gather::default_root(), &title).map_err(err)?;
    let folder = g.folder.display().to_string();
    let _ = app.opener().open_path(&folder, None::<&str>);
    Ok(GatherDto { folder, linked: g.linked, missing: g.missing.len() })
}

// ---------- settings ----------

/// Folders to watch: the defaults plus the user's extra folders, minus excluded ones.
fn watched_folders(db: &Db) -> Vec<PathBuf> {
    let excluded = db.excluded_folders().unwrap_or_default();
    let mut folders = watch::default_folders();
    folders.extend(db.extra_folders().unwrap_or_default().into_iter().map(PathBuf::from));
    folders.retain(|f| {
        let f = f.display().to_string();
        !excluded.iter().any(|e| e.eq_ignore_ascii_case(&f) || memstor_core::projects::is_inside(&f, e))
    });
    folders.dedup();
    folders
}

/// Stop the collector and start a new one on the current folders (keeps the paused state).
fn restart_collector(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let folders = watched_folders(&*state.db.lock().map_err(err)?);
    let mut c = state.collector.lock().map_err(err)?;
    c.0.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let control = Arc::new(Control::default());
    control.set_paused(c.0.is_paused());
    start_collector(state.db_path.clone(), folders.clone(), control.clone());
    let mut w = state.cloud.lock().map_err(err)?;
    w.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    *w = start_cloud(state.db_path.clone(), folders.clone(), control.clone());
    *c = (control, folders);
    Ok(())
}

#[derive(Serialize)]
struct SettingsDto {
    watching: Vec<String>,
    extra: Vec<String>,
    excluded: Vec<String>,
    keep_days: i64,
    autostart: bool,
    db_path: String,
    db_bytes: u64,
    encrypted: bool,
}

#[tauri::command]
fn get_settings(app: AppHandle, state: State<AppState>) -> Result<SettingsDto, String> {
    let db = state.db.lock().map_err(err)?;
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let wal = PathBuf::from(format!("{}-wal", state.db_path.display()));
    Ok(SettingsDto {
        watching: state.folders().iter().map(|f| f.display().to_string()).collect(),
        extra: db.extra_folders().map_err(err)?,
        excluded: db.excluded_folders().map_err(err)?,
        keep_days: db.keep_days().map_err(err)?,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        db_bytes: size(&state.db_path) + size(&wal),
        db_path: state.db_path.display().to_string(),
        encrypted: db.is_encrypted(),
    })
}

#[tauri::command]
fn add_extra_folder(folder: String, app: AppHandle, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.add_extra_folder(&folder).map_err(err)?;
    restart_collector(&app)
}

#[tauri::command]
fn remove_extra_folder(folder: String, app: AppHandle, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.remove_extra_folder(&folder).map_err(err)?;
    restart_collector(&app)
}

/// Never record this folder again and forget what was recorded there. Returns files forgotten.
#[tauri::command]
fn exclude_folder(folder: String, app: AppHandle, state: State<AppState>) -> Result<usize, String> {
    let n = state.db.lock().map_err(err)?.exclude_folder(&folder).map_err(err)?;
    restart_collector(&app)?;
    Ok(n)
}

#[tauri::command]
fn include_folder(folder: String, app: AppHandle, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.include_folder(&folder).map_err(err)?;
    restart_collector(&app)
}

#[tauri::command]
fn set_keep_days(days: i64, state: State<AppState>) -> Result<usize, String> {
    let db = state.db.lock().map_err(err)?;
    db.set_keep_days(days).map_err(err)?;
    db.prune(Local::now().timestamp()).map_err(err)
}

#[tauri::command]
fn set_autostart(on: bool, app: AppHandle) -> Result<bool, String> {
    let al = app.autolaunch();
    if on { al.enable() } else { al.disable() }.map_err(err)?;
    Ok(al.is_enabled().unwrap_or(on))
}

#[tauri::command]
fn wipe_history(state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.wipe_history(Local::now().timestamp()).map_err(err)
}

// ---------- cloud descriptions ----------

#[derive(Serialize)]
struct CloudDto {
    #[serde(flatten)]
    status: cloud::CloudStatus,
    /// The last four characters of the saved key, to show which one it is.
    key_end: Option<String>,
    local_only: Vec<String>,
}

#[tauri::command]
fn cloud_status(state: State<AppState>) -> Result<CloudDto, String> {
    let db = state.db.lock().map_err(err)?;
    let key_end = db.api_key().map_err(err)?.map(|k| k.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect());
    Ok(CloudDto { status: db.cloud_status(&state.worker().state()).map_err(err)?, key_end, local_only: db.local_only_folders().map_err(err)? })
}

#[tauri::command]
fn set_cloud_on(on: bool, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.set_cloud_on(on).map_err(err)
}

/// Check the key with a tiny call, then save it. A key that doesn't work is not saved.
#[tauri::command]
async fn set_api_key(key: String, app: AppHandle) -> Result<(), String> {
    let key = key.trim().to_string();
    if key.len() < 20 {
        return Err("That doesn't look like a Claude API key. It starts with sk-ant-.".into());
    }
    let db_path = app.state::<AppState>().db_path.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let db = Db::open(&db_path).map_err(err)?;
        cloud::check_key(&db, &key).map_err(err)?;
        db.set_api_key(Some(&key)).map_err(err)
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn clear_api_key(state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.set_api_key(None).map_err(err)
}

/// Raise this period's cap by $1, once the user agrees.
#[tauri::command]
fn allow_more(state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.allow_more().map_err(err)
}

/// Describe now, even while the PC is in use (still within the cap).
#[tauri::command]
fn index_now(state: State<AppState>) {
    state.worker().force.store(true, std::sync::atomic::Ordering::Relaxed);
}

#[tauri::command]
fn rescan_library(state: State<AppState>) {
    state.worker().rescan.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Files in this folder are never sent to the cloud. Returns how many descriptions were deleted.
#[tauri::command]
fn add_local_only_folder(folder: String, state: State<AppState>) -> Result<usize, String> {
    state.db.lock().map_err(err)?.add_local_only_folder(&folder).map_err(err)
}

#[tauri::command]
fn remove_local_only_folder(folder: String, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.remove_local_only_folder(&folder).map_err(err)
}

#[tauri::command]
fn forget_cloud_descriptions(state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.forget_cloud_descriptions().map_err(err)
}

#[derive(Serialize)]
struct FoundDto {
    #[serde(flatten)]
    found: cloud::Found,
    name: String,
    folder: String,
    exists: bool,
}

#[derive(Serialize)]
struct SmartDto {
    understood: Vec<String>,
    results: Vec<FoundDto>,
    local_only: Option<String>,
    cost: String,
}

/// Search by memory. Runs on its own connection so the window stays responsive while
/// Claude answers.
#[tauri::command]
async fn smart_search(question: String, app: AppHandle) -> Result<SmartDto, String> {
    let db_path = app.state::<AppState>().db_path.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<SmartDto, String> {
        let db = Db::open(&db_path).map_err(err)?;
        let a = db.smart_search(&question).map_err(err)?;
        Ok(SmartDto {
            understood: a.understood,
            local_only: a.local_only,
            cost: match a.micro_usd {
                0 => String::new(),
                m if m < 10_000 => "less than 1 cent".into(),
                m => memstor_core::claude::dollars(m),
            },
            results: a
                .results
                .into_iter()
                .map(|f| {
                    let (name, folder) = f.path.as_deref().map(split).unwrap_or_default();
                    let exists = f.path.as_deref().is_some_and(|p| Path::new(p).exists());
                    FoundDto { found: f, name, folder, exists }
                })
                .collect(),
        })
    })
    .await
    .map_err(err)?
}

fn apply_paused(app: &AppHandle, paused: bool) {
    let state = app.state::<AppState>();
    state.control().set_paused(paused);
    let _ = state.pause_item.set_text(if paused { "Resume recording" } else { "Pause recording" });
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(if paused { "MemStor (paused)" } else { "MemStor (recording)" }));
    }
    let _ = app.emit("paused-changed", paused);
}

fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        let _ = app.emit("window-shown", ());
    }
}

fn toggle_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) {
            let _ = w.hide();
        } else {
            show_window(app);
        }
    }
}

/// Starts the cloud describer on its own thread with its own connection. It does nothing
/// until cloud descriptions are turned on and a key is saved.
fn start_cloud(db_path: PathBuf, folders: Vec<PathBuf>, control: Arc<Control>) -> Arc<Worker> {
    let worker = Arc::new(Worker::default());
    let w = worker.clone();
    std::thread::spawn(move || {
        let db = match Db::open(&db_path) {
            Ok(db) => db,
            Err(e) => return eprintln!("cloud worker could not open the database: {e}"),
        };
        cloud::run_worker(&db, &folders, &w, &control.paused);
    });
    worker
}

/// Starts the collector on its own thread with its own database connection.
fn start_collector(db_path: PathBuf, folders: Vec<PathBuf>, control: Arc<Control>) {
    std::thread::spawn(move || {
        let db = match Db::open(&db_path) {
            Ok(db) => db,
            Err(e) => return eprintln!("collector could not open the database: {e}"),
        };
        let recent_dir = recent::default_dir().filter(|d| d.is_dir());
        if let Err(e) = watch::run(&db, &folders, recent_dir, &control) {
            eprintln!("collector stopped: {e}");
        }
    });
}

fn main() {
    let hotkey = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space);

    tauri::Builder::default()
        // Must be first: a second launch just shows the existing window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_window(app)))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, sc, event| {
                    if sc == &hotkey && event.state() == ShortcutState::Pressed {
                        toggle_window(app);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let db_path = default_db_path();
            let db = Db::open(&db_path)?;
            let folders = watched_folders(&db);
            let control = Arc::new(Control::default());
            // Start with Windows by default, once; the user can turn it off in Settings.
            if db.get_meta(AUTOSTART_SET_KEY).ok().flatten().is_none() {
                let _ = app.autolaunch().enable();
                let _ = db.set_meta(AUTOSTART_SET_KEY, 1);
            }

            let open_item = MenuItem::with_id(app, "open", format!("Search files\t{HOTKEY_LABEL}"), true, None::<&str>)?;
            let pause_item = MenuItem::with_id(app, "pause", "Pause recording", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit MemStor", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open_item, &pause_item, &sep, &quit_item])?;

            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().expect("app icon is bundled"))
                .tooltip("MemStor (recording)")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => show_window(app),
                    "pause" => {
                        let paused = app.state::<AppState>().control().is_paused();
                        apply_paused(app, !paused);
                    }
                    "quit" => {
                        let state = app.state::<AppState>();
                        state.control().stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        state.worker().stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        toggle_window(tray.app_handle());
                    }
                })
                .build(app)?;

            // Another app may already own the hotkey; the tray still works without it.
            let hotkey_ok = match app.global_shortcut().register(hotkey) {
                Ok(()) => true,
                Err(e) => {
                    eprintln!("could not register {HOTKEY_LABEL}: {e}");
                    false
                }
            };

            start_collector(db_path.clone(), folders.clone(), control.clone());
            let worker = start_cloud(db_path.clone(), folders.clone(), control.clone());
            let start_visible = std::env::args().any(|a| a == "--show");
            app.manage(AppState {
                db: Mutex::new(db),
                collector: Mutex::new((control, folders)),
                cloud: Mutex::new(worker),
                db_path,
                pause_item,
                hotkey_ok: Mutex::new(hotkey_ok),
            });
            // Starts hidden in the tray, unless launched with --show (e.g. from the Start menu).
            if start_visible {
                show_window(app.handle());
            }
            Ok(())
        })
        // Closing the window hides it; the app keeps recording in the tray.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            cloud_status,
            set_cloud_on,
            set_api_key,
            clear_api_key,
            allow_more,
            index_now,
            rescan_library,
            add_local_only_folder,
            remove_local_only_folder,
            forget_cloud_descriptions,
            smart_search,
            get_settings,
            add_extra_folder,
            remove_extra_folder,
            exclude_folder,
            include_folder,
            set_keep_days,
            set_autostart,
            wipe_history,
            recall,
            list_memories,
            rename_memory,
            forget_memory,
            gather_memory,
            list_activities,
            drop_from_activity,
            gather_activity,
            save_activity_as_project,
            search,
            day,
            open_file,
            show_in_folder,
            status,
            set_paused,
            hide_window,
            list_projects,
            create_project,
            delete_project,
            add_project_folder,
            remove_project_folder,
            pick_folder,
            project_files,
            suggest_projects,
            gather_files
        ])
        .run(tauri::generate_context!())
        .expect("error while running MemStor");
}
