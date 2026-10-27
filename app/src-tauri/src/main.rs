//! MemStor tray app. A background thread records file activity; a hidden window
//! opens on Ctrl+Shift+Space (or from the tray) to search it.

// No console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use chrono::{Duration, Local, TimeZone};
use memstor_core::db::Db;
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
use tauri_plugin_opener::OpenerExt;

const HOTKEY_LABEL: &str = "Ctrl+Shift+Space";

struct AppState {
    /// The window's own connection; the collector thread has a separate one.
    db: Mutex<Db>,
    control: Arc<Control>,
    db_path: PathBuf,
    folders: Vec<PathBuf>,
    pause_item: MenuItem<Wry>,
    hotkey_ok: Mutex<bool>,
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
        paused: state.control.is_paused(),
        db_path: state.db_path.display().to_string(),
        folders: state.folders.iter().map(|f| f.display().to_string()).collect(),
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

fn apply_paused(app: &AppHandle, paused: bool) {
    let state = app.state::<AppState>();
    state.control.set_paused(paused);
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
            let folders = watch::default_folders();
            let control = Arc::new(Control::default());

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
                        let paused = app.state::<AppState>().control.is_paused();
                        apply_paused(app, !paused);
                    }
                    "quit" => {
                        app.state::<AppState>().control.stop.store(true, std::sync::atomic::Ordering::Relaxed);
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
            let start_visible = std::env::args().any(|a| a == "--show");
            app.manage(AppState {
                db: Mutex::new(db),
                control,
                db_path,
                folders,
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
