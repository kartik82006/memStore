//! A small problem log on the PC (%LOCALAPPDATA%\MemStor\logs), so that when something goes
//! wrong there's something to look at. The app has no console window, so anything MemStor
//! would print about a problem is written here instead, with crashes too. Logs never leave
//! the PC unless the person copies them into a bug report.

use std::io::Write;
use std::path::PathBuf;

/// A log bigger than this is moved to `<name>.old.log` when MemStor starts.
const MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn dir() -> PathBuf {
    crate::default_db_path().with_file_name("logs")
}

/// Send this process's problem messages (and crash reports) to `<logs>\<name>.log`.
pub fn init(name: &str) {
    let dir = dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{name}.log"));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(&path, dir.join(format!("{name}.old.log")));
    }
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    let _ = writeln!(file, "\n=== MemStor {} started {} ===", env!("CARGO_PKG_VERSION"), chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    redirect_stderr(file);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("[{}] MemStor crashed: {info}\n{}", chrono::Local::now().format("%H:%M:%S"), std::backtrace::Backtrace::force_capture());
        previous(info);
    }));
}

#[cfg(windows)]
fn redirect_stderr(file: std::fs::File) {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    // Rust looks the handle up on every write, so eprintln! goes to the file from now on.
    // The handle is kept open for the life of the process.
    let h = file.into_raw_handle();
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, h as _);
    }
}

#[cfg(not(windows))]
fn redirect_stderr(file: std::fs::File) {
    // Outside Windows, keep printing to the terminal and note where the log is.
    drop(file);
}

/// The last `lines` lines of a log, for "Copy details for a bug report".
pub fn tail(name: &str, lines: usize) -> String {
    let text = std::fs::read_to_string(dir().join(format!("{name}.log"))).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}
