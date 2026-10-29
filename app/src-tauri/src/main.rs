//! MemStor tray app: the assistant window and the tray icon. Recording runs in a separate
//! background process (`memstor-app.exe --recorder`) that this app starts and watches.
//! The window opens on Ctrl+Space (or Ctrl+Shift+Space, or from the tray).

// No console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::{Duration, Local, TimeZone};
use memstor_core::agent::Reply;
use memstor_core::actions::Action;
use memstor_core::context::{self, Chip, ContextCard, Foreground};
use memstor_core::db::Db;
use memstor_core::cloud;
use memstor_core::projects::{DEFAULT_NEARBY_MINUTES, Reason};
use memstor_core::{default_db_path, gather};
use tauri_plugin_dialog::DialogExt;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent, Wry};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_opener::OpenerExt;

const HOTKEY_LABEL: &str = "Ctrl+Space";
const HOTKEY_LABEL_LONG: &str = "Ctrl+Shift+Space";
/// Meta key set once start-with-Windows has been turned on the first time.
const AUTOSTART_SET_KEY: &str = "autostart_set";

struct AppState {
    /// The window's own connection; the recorder process has its own.
    db: Mutex<Db>,
    db_path: PathBuf,
    pause_item: MenuItem<Wry>,
    /// The hotkeys that were registered, for the window's hint.
    hotkeys: Mutex<Vec<String>>,
    /// What was going on when the window was last called.
    context: Mutex<ContextCard>,
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
    let db = state.db.lock();
    let (paused, folders) = db.as_ref().map(|d| (d.recording_paused(), d.watched_folders())).unwrap_or_default();
    Status {
        paused,
        db_path: state.db_path.display().to_string(),
        folders: folders.iter().map(|f| f.display().to_string()).collect(),
        hotkey: state.hotkeys.lock().ok().and_then(|h| h.first().cloned()),
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

/// Ask the recorder to start again on the current folders.
fn restart_collector(app: &AppHandle) -> Result<(), String> {
    app.state::<AppState>().db.lock().map_err(err)?.restart_recorder().map_err(err)
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
    pc_awareness: bool,
}

#[tauri::command]
fn get_settings(app: AppHandle, state: State<AppState>) -> Result<SettingsDto, String> {
    let db = state.db.lock().map_err(err)?;
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let wal = PathBuf::from(format!("{}-wal", state.db_path.display()));
    Ok(SettingsDto {
        watching: db.watched_folders().iter().map(|f| f.display().to_string()).collect(),
        extra: db.extra_folders().map_err(err)?,
        excluded: db.excluded_folders().map_err(err)?,
        keep_days: db.keep_days().map_err(err)?,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        db_bytes: size(&state.db_path) + size(&wal),
        db_path: state.db_path.display().to_string(),
        encrypted: db.is_encrypted(),
        pc_awareness: db.pc_awareness().map_err(err)?,
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
    /// This month's spending per feature: (feature, calls, dollars text).
    by_feature: Vec<(String, i64, String)>,
}

#[tauri::command]
fn cloud_status(state: State<AppState>) -> Result<CloudDto, String> {
    let db = state.db.lock().map_err(err)?;
    let key_end = db.api_key().map_err(err)?.map(|k| k.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect());
    let by_feature = db
        .spend_by_feature()
        .map_err(err)?
        .into_iter()
        .map(|(f, n, m)| (f, n, if m < 10_000 { "less than 1 cent".to_string() } else { memstor_core::claude::dollars(m) }))
        .collect();
    Ok(CloudDto { status: db.cloud_status(&db.worker_state()).map_err(err)?, key_end, local_only: db.local_only_folders().map_err(err)?, by_feature })
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
fn index_now(state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.poke_recorder("describe_now").map_err(err)
}

#[tauri::command]
fn rescan_library(state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.poke_recorder("rescan").map_err(err)
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

fn to_found_dto(f: cloud::Found) -> FoundDto {
    let (name, folder) = f.path.as_deref().map(split).unwrap_or_default();
    let exists = f.path.as_deref().is_some_and(|p| Path::new(p).exists());
    FoundDto { found: f, name, folder, exists }
}

/// Files that look like this one (same event, people, place), found on the PC.
#[tauri::command]
fn similar(path: String, state: State<AppState>) -> Result<Vec<FoundDto>, String> {
    Ok(state.db.lock().map_err(err)?.similar(&path, 12).map_err(err)?.into_iter().map(to_found_dto).collect())
}

#[derive(Serialize)]
struct GroupFileDto {
    #[serde(flatten)]
    file: memstor_core::groups::GroupFile,
    name: String,
    folder: String,
    exists: bool,
}

#[derive(Serialize)]
struct GroupDto {
    name: String,
    kind: String,
    start: i64,
    end: i64,
    photos: usize,
    videos: usize,
    documents: usize,
    files: Vec<GroupFileDto>,
}

/// Described files grouped by event or by who they were made for.
#[tauri::command]
fn list_groups(state: State<AppState>) -> Result<Vec<GroupDto>, String> {
    let groups = state.db.lock().map_err(err)?.groups().map_err(err)?;
    Ok(groups
        .into_iter()
        .map(|g| GroupDto {
            files: g
                .files
                .into_iter()
                .map(|f| {
                    let (name, folder) = split(&f.path);
                    GroupFileDto { exists: Path::new(&f.path).exists(), name, folder, file: f }
                })
                .collect(),
            name: g.name,
            kind: g.kind,
            start: g.start,
            end: g.end,
            photos: g.photos,
            videos: g.videos,
            documents: g.documents,
        })
        .collect())
}

/// Shortcuts to a group's files in one folder named after it.
#[tauri::command]
fn gather_group(name: String, paths: Vec<String>, app: AppHandle) -> Result<GatherDto, String> {
    if paths.is_empty() {
        return Err("There are no files to gather.".into());
    }
    let g = gather::gather(&paths, &gather::default_root(), &name).map_err(err)?;
    let folder = g.folder.display().to_string();
    let _ = app.opener().open_path(&folder, None::<&str>);
    Ok(GatherDto { folder, linked: g.linked, missing: g.missing.len() })
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
            results: a.results.into_iter().map(to_found_dto).collect(),
        })
    })
    .await
    .map_err(err)?
}

fn apply_paused(app: &AppHandle, paused: bool) {
    let state = app.state::<AppState>();
    if let Ok(db) = state.db.lock() {
        let _ = db.set_recording_paused(paused);
    }
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

/// Hide the window if it's in front; otherwise note what's going on and show it.
/// `hwnd` is the window in front before MemStor (0 from the tray).
fn toggle_window(app: &AppHandle, hwnd: isize) {
    if let Some(w) = app.get_webview_window("main") {
        if w.is_visible().unwrap_or(false) && w.is_focused().unwrap_or(false) {
            let _ = w.hide();
        } else {
            refresh_context(app, hwnd);
            show_window(app);
        }
    }
}

// ---------- the recorder process ----------

/// Start the background recorder unless it's already running. It takes a lock, so a
/// second copy started by mistake simply exits.
fn ensure_recorder(db_path: &Path) {
    let alive = Db::open(db_path).map(|db| db.recorder_alive()).unwrap_or(false);
    if alive {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--recorder");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }
    if let Err(e) = cmd.spawn() {
        eprintln!("could not start the recorder: {e}");
    }
}

/// Run as the recorder: no window, no tray. Stops when the app asks it to.
fn run_recorder() {
    let quit = std::sync::atomic::AtomicBool::new(false);
    if let Err(e) = memstor_core::recorder::run(&default_db_path(), &quit) {
        eprintln!("recorder stopped: {e:#}");
    }
}

// ---------- the assistant ----------

#[derive(Serialize)]
struct ContextDto {
    chips: Vec<Chip>,
    /// PC awareness hasn't been asked about yet.
    ask_awareness: bool,
    awareness: bool,
    /// Claude is on with a working key and budget left.
    ai_ready: bool,
    recording: bool,
    paused: bool,
}

/// Read what's going on, then tell the window. `hwnd` is the window that was in front when
/// MemStor was called; 0 when opened from the tray (the last app used stands in).
fn refresh_context(app: &AppHandle, hwnd: isize) {
    let app = app.clone();
    std::thread::spawn(move || {
        let fg: Option<Foreground> = if hwnd != 0 { context::read_window(hwnd) } else { None };
        let state = app.state::<AppState>();
        let card = match Db::open(&state.db_path).and_then(|db| db.context_card(fg, chrono::Local::now().timestamp())) {
            Ok(c) => c,
            Err(e) => return eprintln!("could not read the context: {e}"),
        };
        if let Ok(mut c) = state.context.lock() {
            *c = card;
        }
        let _ = app.emit("context-ready", ());
    });
}

#[tauri::command]
fn context_now(state: State<AppState>) -> Result<ContextDto, String> {
    let db = state.db.lock().map_err(err)?;
    let card = state.context.lock().map_err(err)?.clone();
    Ok(ContextDto {
        chips: card.chips(),
        ask_awareness: db.pc_awareness_unasked().map_err(err)?,
        awareness: db.pc_awareness().map_err(err)?,
        ai_ready: db.assistant_ready().map_err(err)?.is_some(),
        recording: db.recorder_alive(),
        paused: db.recording_paused(),
    })
}

#[tauri::command]
fn set_pc_awareness(on: bool, app: AppHandle, state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.set_pc_awareness(on).map_err(err)?;
    if !on {
        if let Ok(mut c) = state.context.lock() {
            *c = c.without(&["window".into(), "folder".into(), "selected".into()]);
        }
    }
    let _ = app.emit("context-ready", ());
    Ok(())
}

/// Answer an ask. `off` lists the context chips the person switched off.
#[tauri::command]
async fn ask(text: String, off: Vec<String>, app: AppHandle) -> Result<Reply, String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("Type what you're looking for.".into());
    }
    let state = app.state::<AppState>();
    let db_path = state.db_path.clone();
    let card = state.context.lock().map_err(err)?.without(&off);
    tauri::async_runtime::spawn_blocking(move || -> Result<Reply, String> {
        let db = Db::open(&db_path).map_err(err)?;
        db.ask(&text, &card).map_err(|e| format!("{e:#}"))
    })
    .await
    .map_err(err)?
}

#[tauri::command]
fn new_chat(state: State<AppState>) -> Result<(), String> {
    state.db.lock().map_err(err)?.clear_chat().map_err(err)
}

/// Make a change the person approved.
#[tauri::command]
fn apply_action(id: i64, state: State<AppState>) -> Result<Action, String> {
    state.db.lock().map_err(err)?.apply_action(id).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn undo_action(id: i64, state: State<AppState>) -> Result<Action, String> {
    state.db.lock().map_err(err)?.undo_action(id).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn recent_changes(state: State<AppState>) -> Result<Vec<Action>, String> {
    state.db.lock().map_err(err)?.recent_actions(20).map_err(err)
}

/// Shortcuts to any files in one new folder, which is then opened.
#[tauri::command]
fn gather_paths(title: String, paths: Vec<String>, app: AppHandle) -> Result<GatherDto, String> {
    if paths.is_empty() {
        return Err("There are no files to gather.".into());
    }
    let g = gather::gather(&paths, &gather::default_root(), &title).map_err(err)?;
    let folder = g.folder.display().to_string();
    let _ = app.opener().open_path(&folder, None::<&str>);
    Ok(GatherDto { folder, linked: g.linked, missing: g.missing.len() })
}

fn main() {
    if std::env::args().any(|a| a == "--recorder") {
        return run_recorder();
    }
    let short = Shortcut::new(Some(Modifiers::CONTROL), Code::Space);
    let long = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::Space);

    tauri::Builder::default()
        // Must be first: a second launch just shows the existing window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| toggle_window(app, 0)))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, sc, event| {
                    if (sc == &short || sc == &long) && event.state() == ShortcutState::Pressed {
                        // Note the window in front before MemStor takes focus.
                        toggle_window(app, context::foreground_window());
                    }
                })
                .build(),
        )
        .setup(move |app| {
            let db_path = default_db_path();
            let db = Db::open(&db_path)?;
            // Start with Windows by default, once; the user can turn it off in Settings.
            if db.get_meta(AUTOSTART_SET_KEY).ok().flatten().is_none() {
                let _ = app.autolaunch().enable();
                let _ = db.set_meta(AUTOSTART_SET_KEY, 1);
            }
            let paused = db.recording_paused();

            let open_item = MenuItem::with_id(app, "open", "Open MemStor", true, None::<&str>)?;
            let pause_item = MenuItem::with_id(app, "pause", if paused { "Resume recording" } else { "Pause recording" }, true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit MemStor", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&open_item, &pause_item, &sep, &quit_item])?;

            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().cloned().expect("app icon is bundled"))
                .tooltip(if paused { "MemStor (paused)" } else { "MemStor (recording)" })
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "open" => toggle_window(app, 0),
                    "pause" => {
                        let paused = app.state::<AppState>().db.lock().map(|d| d.recording_paused()).unwrap_or(false);
                        apply_paused(app, !paused);
                    }
                    "quit" => {
                        // Quitting stops recording too.
                        if let Ok(db) = app.state::<AppState>().db.lock() {
                            let _ = db.stop_recorder();
                        }
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                        toggle_window(tray.app_handle(), 0);
                    }
                })
                .build(app)?;

            // Ctrl+Space, unless several keyboard languages are installed (it often switches
            // them). Ctrl+Shift+Space always works too. Another app may own either key.
            let mut hotkeys = Vec::new();
            let mut keys = vec![(long, HOTKEY_LABEL_LONG)];
            if !context::several_keyboard_layouts() {
                keys.insert(0, (short, HOTKEY_LABEL));
            }
            for (key, label) in keys {
                match app.global_shortcut().register(key) {
                    Ok(()) => hotkeys.push(label.to_string()),
                    Err(e) => eprintln!("could not register {label}: {e}"),
                }
            }

            // A new start clears an old "stop" so the recorder runs again.
            let _ = db.set_kv("recorder_stop", None);
            ensure_recorder(&db_path);
            // Restart the recorder if it ever stops while the app is running.
            let watch_path = db_path.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(15));
                let stopping = Db::open(&watch_path).ok().and_then(|db| db.kv("recorder_stop").ok().flatten()).is_some();
                if !stopping {
                    ensure_recorder(&watch_path);
                }
            });

            let start_visible = std::env::args().any(|a| a == "--show");
            app.manage(AppState {
                db: Mutex::new(db),
                db_path,
                pause_item,
                hotkeys: Mutex::new(hotkeys),
                context: Mutex::new(ContextCard::default()),
            });
            // Starts hidden in the tray, unless launched with --show (e.g. from the Start menu).
            if start_visible {
                toggle_window(app.handle(), 0);
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
            context_now,
            set_pc_awareness,
            ask,
            new_chat,
            apply_action,
            undo_action,
            recent_changes,
            gather_paths,
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
            similar,
            list_groups,
            gather_group,
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
