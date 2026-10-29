//! The background recorder: a small process with no window that records file activity,
//! describes files with the cloud (when on) and notes which app is in front. It starts with
//! Windows and keeps going when the assistant window is closed, crashes or updates.
//!
//! The window talks to it through the database: a few settings rows it reads every couple of
//! seconds (pause, restart with new folders, describe now, stop), and a heartbeat it writes.

use crate::cloud::{self, Worker};
use crate::context;
use crate::db::Db;
use crate::recent;
use crate::watch::{self, Control};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The recorder counts as running if its heartbeat is this recent.
pub const ALIVE_SECS: i64 = 10;
const TICK: Duration = Duration::from_secs(2);
/// How often the app in front is noted.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);

fn now() -> i64 {
    chrono::Local::now().timestamp()
}

impl Db {
    pub fn recorder_alive(&self) -> bool {
        self.kv("recorder_alive").ok().flatten().and_then(|v| v.parse::<i64>().ok()).is_some_and(|t| now() - t <= ALIVE_SECS)
    }

    pub fn recording_paused(&self) -> bool {
        self.kv("paused").ok().flatten().as_deref() == Some("1")
    }

    pub fn set_recording_paused(&self, paused: bool) -> Result<()> {
        self.set_kv("paused", paused.then_some("1"))
    }

    /// Ask the recorder to start again on the current folders.
    pub fn restart_recorder(&self) -> Result<()> {
        let g = self.kv("recorder_generation")?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0) + 1;
        self.set_kv("recorder_generation", Some(&g.to_string()))
    }

    /// Ask the recorder to stop (when the person quits MemStor).
    pub fn stop_recorder(&self) -> Result<()> {
        self.set_kv("recorder_stop", Some("1"))
    }

    /// "Describe now" and "Look for files again" from Settings.
    pub fn poke_recorder(&self, what: &str) -> Result<()> {
        self.set_kv(&format!("poke_{what}"), Some("1"))
    }

    /// What the cloud worker is doing, as the recorder last reported it.
    pub fn worker_state(&self) -> String {
        if !self.recorder_alive() {
            return "stopped".into();
        }
        self.kv("worker_state").ok().flatten().unwrap_or_default()
    }

    fn take_poke(&self, what: &str) -> bool {
        let key = format!("poke_{what}");
        let on = self.kv(&key).ok().flatten().is_some();
        if on {
            let _ = self.set_kv(&key, None);
        }
        on
    }
}

/// Holds the lock that makes sure only one recorder runs.
pub struct Lock(#[allow(dead_code)] std::fs::File);

/// Take the recorder lock next to the database, or None if another recorder holds it.
pub fn lock(db_path: &Path) -> Option<Lock> {
    let path = db_path.with_file_name("recorder.lock");
    let f = std::fs::File::options().create(true).truncate(false).write(true).open(path).ok()?;
    f.try_lock().ok()?;
    Some(Lock(f))
}

struct Running {
    control: Arc<Control>,
    worker: Arc<Worker>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

fn start(db_path: &Path, folders: Vec<PathBuf>, paused: bool) -> Running {
    let control = Arc::new(Control::default());
    control.set_paused(paused);
    let worker = Arc::new(Worker::default());
    let mut threads = Vec::new();
    {
        let (path, folders, control) = (db_path.to_path_buf(), folders.clone(), control.clone());
        threads.push(std::thread::spawn(move || {
            let db = match Db::open(&path) {
                Ok(db) => db,
                Err(e) => return eprintln!("collector could not open the database: {e}"),
            };
            let recent_dir = recent::default_dir().filter(|d| d.is_dir());
            if let Err(e) = watch::run(&db, &folders, recent_dir, &control) {
                eprintln!("collector stopped: {e}");
            }
        }));
    }
    {
        let (path, control, worker) = (db_path.to_path_buf(), control.clone(), worker.clone());
        threads.push(std::thread::spawn(move || {
            let db = match Db::open(&path) {
                Ok(db) => db,
                Err(e) => return eprintln!("cloud worker could not open the database: {e}"),
            };
            cloud::run_worker(&db, &folders, &worker, &control.paused);
        }));
    }
    Running { control, worker, threads }
}

fn stop(r: Running) {
    r.control.stop.store(true, Ordering::Relaxed);
    r.worker.stop.store(true, Ordering::Relaxed);
    for t in r.threads {
        let _ = t.join();
    }
}

/// Run until `quit` is set or the window asks the recorder to stop. Returns at once if
/// another recorder is already running.
pub fn run(db_path: &Path, quit: &AtomicBool) -> Result<()> {
    run_on(db_path, quit, None)
}

/// `run`, watching `folders` instead of the usual ones (for tests).
pub fn run_on(db_path: &Path, quit: &AtomicBool, folders: Option<Vec<PathBuf>>) -> Result<()> {
    let Some(_lock) = lock(db_path) else {
        eprintln!("another recorder is already running");
        return Ok(());
    };
    let db = Db::open(db_path)?;
    db.set_kv("recorder_stop", None)?;
    db.set_kv("recorder_alive", Some(&now().to_string()))?;
    let generation = |db: &Db| db.kv("recorder_generation").ok().flatten().unwrap_or_default();
    'outer: loop {
        let gen_started = generation(&db);
        let running = start(db_path, folders.clone().unwrap_or_else(|| db.watched_folders()), db.recording_paused());
        let mut last_sample = Instant::now() - SAMPLE_EVERY;
        let mut last_state = String::new();
        loop {
            std::thread::sleep(TICK);
            let stop_asked = quit.load(Ordering::Relaxed) || db.kv("recorder_stop").ok().flatten().is_some();
            if stop_asked {
                stop(running);
                let _ = db.set_kv("recorder_alive", None);
                let _ = db.set_kv("worker_state", None);
                break 'outer;
            }
            let _ = db.set_kv("recorder_alive", Some(&now().to_string()));
            let paused = db.recording_paused();
            running.control.set_paused(paused);
            if db.take_poke("describe_now") {
                running.worker.force.store(true, Ordering::Relaxed);
            }
            if db.take_poke("rescan") {
                running.worker.rescan.store(true, Ordering::Relaxed);
            }
            let state = running.worker.state();
            if state != last_state {
                let _ = db.set_kv("worker_state", Some(&state));
                last_state = state;
            }
            if !paused && last_sample.elapsed() >= SAMPLE_EVERY {
                last_sample = Instant::now();
                if db.pc_awareness().unwrap_or(false) {
                    // Explorer's folder isn't read here; only when the assistant is called.
                    if let Some(fg) = context::read_window_basic(context::foreground_window()) {
                        if let Err(e) = db.log_app(&fg, now()) {
                            eprintln!("could not note the app in front: {e}");
                        }
                    }
                }
            }
            if generation(&db) != gen_started {
                stop(running);
                continue 'outer;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_through_the_database() {
        let dir = crate::test_dir("recorder");
        let path = dir.join("memstor.db");
        let db = Db::open(&path).unwrap();
        assert!(!db.recorder_alive());
        db.set_recording_paused(true).unwrap();
        assert!(db.recording_paused());
        db.poke_recorder("describe_now").unwrap();
        assert!(db.take_poke("describe_now") && !db.take_poke("describe_now"));

        // Only one recorder at a time.
        let first = lock(&path).expect("first lock");
        assert!(lock(&path).is_none());
        drop(first);
        assert!(lock(&path).is_some());

        // A real run: starts, beats, and stops when asked.
        let quit = Arc::new(AtomicBool::new(false));
        let (p, q) = (path.clone(), quit.clone());
        let watched = vec![dir.clone()];
        let t = std::thread::spawn(move || run_on(&p, &q, Some(watched)).unwrap());
        let started = Instant::now();
        while !db.recorder_alive() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(db.recorder_alive());
        db.stop_recorder().unwrap();
        t.join().unwrap();
        assert!(!db.recorder_alive());
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
