//! "A new version is out": once a day MemStor asks GitHub for the newest release and, if it
//! is newer, the window shows a note with a link to download it. Only the version number is
//! asked for; nothing about the PC or its files is sent. It can be turned off in Settings.

use crate::db::Db;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const RELEASES_API: &str = "https://api.github.com/repos/kartik82006/memStore/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/kartik82006/memStore/releases/latest";
const CHECK_EVERY_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Update {
    pub version: String,
    pub url: String,
}

/// "v0.7.1" or "0.7.1" as numbers; None if it isn't a version.
fn parse(v: &str) -> Option<Vec<u64>> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let core = v.split(['-', '+']).next()?;
    core.split('.').map(|p| p.parse().ok()).collect()
}

/// True when `latest` is a newer version than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse(latest), parse(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

impl Db {
    /// Whether MemStor checks for new versions. On unless turned off.
    pub fn update_check_on(&self) -> Result<bool> {
        Ok(self.kv("update_check")?.as_deref() != Some("0"))
    }

    pub fn set_update_check_on(&self, on: bool) -> Result<()> {
        self.set_kv("update_check", Some(if on { "1" } else { "0" }))?;
        if !on {
            self.set_kv("update_available", None)?;
        }
        Ok(())
    }

    /// A newer version that was found, if any.
    pub fn update_available(&self) -> Result<Option<Update>> {
        let Some(v) = self.kv("update_available")? else { return Ok(None) };
        let u: Update = serde_json::from_str(&v)?;
        Ok(is_newer(&u.version, env!("CARGO_PKG_VERSION")).then_some(u))
    }

    /// True when it's time to ask GitHub again.
    pub fn update_check_due(&self, now: i64) -> Result<bool> {
        let last = self.kv("update_checked_at")?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
        Ok(self.update_check_on()? && now - last >= CHECK_EVERY_SECS)
    }

    /// Note what GitHub said.
    pub fn note_latest(&self, latest: Option<Update>, now: i64) -> Result<()> {
        self.set_kv("update_checked_at", Some(&now.to_string()))?;
        match latest.filter(|u| is_newer(&u.version, env!("CARGO_PKG_VERSION"))) {
            Some(u) => self.set_kv("update_available", Some(&serde_json::to_string(&u)?)),
            None => self.set_kv("update_available", None),
        }
    }
}

/// Ask GitHub for the newest release.
pub fn fetch_latest() -> Result<Option<Update>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(15))).build().into();
    let mut resp = match agent.get(RELEASES_API).header("User-Agent", "MemStor-update-check").header("Accept", "application/vnd.github+json").call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => return Ok(None), // no release yet
        Err(e) => return Err(e).context("could not reach GitHub"),
    };
    let v: serde_json::Value = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    let Some(tag) = v["tag_name"].as_str() else { return Ok(None) };
    Ok(Some(Update { version: tag.trim_start_matches('v').to_string(), url: v["html_url"].as_str().unwrap_or(RELEASES_PAGE).to_string() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers() {
        assert!(is_newer("v0.10.0", "0.9.3"));
        assert!(is_newer("0.7.1", "0.7.0"));
        assert!(!is_newer("v0.7.0", "0.7.0"));
        assert!(!is_newer("nightly", "0.7.0"));
    }

    #[test]
    fn remembers_a_newer_version_only() {
        let db = Db::in_memory().unwrap();
        assert!(db.update_check_due(1_000_000).unwrap());
        db.note_latest(Some(Update { version: "99.0.0".into(), url: "u".into() }), 1_000_000).unwrap();
        assert!(!db.update_check_due(1_000_100).unwrap());
        assert_eq!(db.update_available().unwrap().unwrap().version, "99.0.0");
        db.note_latest(Some(Update { version: "0.0.1".into(), url: "u".into() }), 2_000_000).unwrap();
        assert!(db.update_available().unwrap().is_none());
        db.set_update_check_on(false).unwrap();
        assert!(!db.update_check_due(9_000_000).unwrap());
    }
}
