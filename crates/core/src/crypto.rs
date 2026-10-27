//! The database is encrypted with SQLCipher. Its key is 32 random bytes kept next to the
//! database in `memstor.key`. On Windows the key file is sealed with DPAPI, so only the same
//! Windows user on the same PC can unseal it; copying the database and key elsewhere is useless.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const KEY_LEN: usize = 32;
const PLAIN_HEADER: &[u8] = b"SQLite format 3\0";

pub(crate) fn key_path(db: &Path) -> PathBuf {
    db.with_extension("key")
}

/// The key as SQLCipher wants it in a PRAGMA: "x'hex…'".
pub(crate) fn key_literal(key: &[u8]) -> String {
    let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
    format!("\"x'{hex}'\"")
}

/// Load the key for this database, creating one if there is none yet.
pub(crate) fn load_or_create_key(db: &Path) -> Result<Vec<u8>> {
    let path = key_path(db);
    if path.exists() {
        let sealed = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let key = unseal(&sealed).context("could not unlock the database key (was it copied from another PC or user?)")?;
        if key.len() != KEY_LEN {
            bail!("the database key file {} is damaged", path.display());
        }
        return Ok(key);
    }
    let mut key = vec![0u8; KEY_LEN];
    getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("no random numbers: {e}"))?;
    let sealed = seal(&key)?;
    write_private(&path, &sealed)?;
    Ok(key)
}

/// True when the file is an unencrypted SQLite database (made before encryption was added).
pub(crate) fn is_plain_sqlite(db: &Path) -> bool {
    let mut head = [0u8; 16];
    std::fs::File::open(db)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .map(|_| head == PLAIN_HEADER)
        .unwrap_or(false)
}

/// Turn an unencrypted database into an encrypted one in place.
pub(crate) fn encrypt_existing(db: &Path, key: &[u8]) -> Result<()> {
    let tmp = db.with_extension("db.encrypting");
    let _ = std::fs::remove_file(&tmp);
    {
        let plain = Connection::open(db)?;
        plain.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        let target = tmp.to_string_lossy().replace('\'', "''");
        plain.execute_batch(&format!(
            "ATTACH DATABASE '{target}' AS enc KEY {}; SELECT sqlcipher_export('enc'); DETACH DATABASE enc;",
            key_literal(key)
        ))?;
    }
    for ext in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{ext}", db.display()));
    }
    std::fs::rename(&tmp, db).context("replacing the database with its encrypted copy")?;
    Ok(())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
        f.write_all(bytes)?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

#[cfg(windows)]
fn seal(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, true)
}

#[cfg(windows)]
fn unseal(data: &[u8]) -> Result<Vec<u8>> {
    dpapi(data, false)
}

#[cfg(windows)]
fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: input points at `data` for the call; output is allocated by Windows and freed below.
    let ok = unsafe {
        if protect {
            CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        } else {
            CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        }
    };
    if ok == 0 {
        bail!("Windows could not {} the key: {}", if protect { "protect" } else { "unlock" }, std::io::Error::last_os_error());
    }
    // SAFETY: Windows returned cbData bytes at pbData.
    let out = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as _) };
    Ok(out)
}

// Elsewhere (development and tests) the key file is only protected by its file permissions.
#[cfg(not(windows))]
fn seal(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(not(windows))]
fn unseal(data: &[u8]) -> Result<Vec<u8>> {
    Ok(data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, Kind};

    #[test]
    fn new_database_is_encrypted_and_reopens() {
        let dir = crate::test_dir("crypto-new");
        let path = dir.join("memstor.db");
        {
            let db = Db::open(&path).unwrap();
            assert!(db.is_encrypted());
            db.record("/x/secret plans.docx", Kind::Modified, "watcher", 1).unwrap();
        }
        assert!(!is_plain_sqlite(&path));
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"secret"), "file names must not be readable on disk");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.search("secret", 5).unwrap().len(), 1);

        // Without the key the data can't be read.
        let plain = Connection::open(&path).unwrap();
        assert!(plain.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0)).is_err());
    }

    #[test]
    fn old_unencrypted_database_is_converted() {
        let dir = crate::test_dir("crypto-migrate");
        let path = dir.join("memstor.db");
        {
            // A database from before encryption, with search and projects.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, name TEXT NOT NULL, ext TEXT NOT NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL);
                 CREATE TABLE events (id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL, ts INTEGER NOT NULL, kind TEXT NOT NULL, source TEXT NOT NULL, detail TEXT);
                 INSERT INTO events (file_id, ts, kind, source) VALUES (1, 5, 'modified', 'watcher');
                 CREATE VIRTUAL TABLE files_fts USING fts5(name, path, tokenize = 'unicode61');
                 INSERT INTO files VALUES (1, '/x/thesis draft.docx', 'thesis draft', 'docx', 5, 5);
                 INSERT INTO files_fts (rowid, name, path) VALUES (1, 'thesis draft', '/x/thesis draft.docx');",
            )
            .unwrap();
        }
        assert!(is_plain_sqlite(&path));
        let db = Db::open(&path).unwrap();
        assert!(db.is_encrypted());
        assert!(!is_plain_sqlite(&path));
        assert_eq!(db.search("thesis", 5).unwrap().len(), 1);
    }
}
