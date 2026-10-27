//! MemStor core: the background collector and the local database it writes to.
//! Shared by the command-line tool and the tray app.

pub mod db;
pub mod filter;
pub mod recent;
pub mod watch;

use std::path::PathBuf;

/// Where the database lives: %LOCALAPPDATA%\MemStor\memstor.db on Windows.
pub fn default_db_path() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("MemStor").join("memstor.db")
}
