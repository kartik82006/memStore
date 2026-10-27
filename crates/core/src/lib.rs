//! MemStor core: the background collector and the local database it writes to.
//! Shared by the command-line tool and the tray app.

pub mod activities;
pub mod db;
pub mod filter;
pub mod gather;
pub mod memory;
pub mod projects;
pub mod recall;
pub mod recent;
pub mod watch;

use std::path::PathBuf;

/// Where the database lives: %LOCALAPPDATA%\MemStor\memstor.db on Windows.
pub fn default_db_path() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("MemStor").join("memstor.db")
}

/// A fresh folder for tests. Not the system temp folder: on Windows that sits under
/// AppData, which the noise filter skips on purpose.
#[cfg(test)]
pub(crate) fn test_dir(tag: &str) -> PathBuf {
    let d = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("test-scratch")
        .join(format!("{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}
