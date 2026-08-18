//! Append-only changelog (JSONL) with a lockfile guarding concurrent writers.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::diff::ChangedRecord;

pub const EVENT_BASELINE: &str = "baseline";
pub const EVENT_CHANGE: &str = "change";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangelogEntry {
    /// RFC3339 timestamp of the observation.
    pub at: String,
    pub page_id: String,
    /// "baseline" (first snapshot) or "change".
    pub event: String,
    #[serde(default)]
    pub added: Vec<Value>,
    #[serde(default)]
    pub removed: Vec<Value>,
    #[serde(default)]
    pub changed_records: Vec<ChangedRecord>,
    #[serde(default)]
    pub lines_added: Vec<String>,
    #[serde(default)]
    pub lines_removed: Vec<String>,
}

impl ChangelogEntry {
    /// Human summary used in table output.
    pub fn summary(&self) -> String {
        if self.event == EVENT_BASELINE {
            "baseline recorded".to_string()
        } else {
            let parts: Vec<String> = [
                (self.added.len() > 0).then(|| format!("+{} added", self.added.len())),
                (self.removed.len() > 0).then(|| format!("-{} removed", self.removed.len())),
                (self.changed_records.len() > 0)
                    .then(|| format!("~{} changed", self.changed_records.len())),
                (self.lines_added.len() > 0)
                    .then(|| format!("+{} lines", self.lines_added.len())),
                (self.lines_removed.len() > 0)
                    .then(|| format!("-{} lines", self.lines_removed.len())),
            ]
            .into_iter()
            .flatten()
            .collect();
            if parts.is_empty() {
                "no visible diff".to_string()
            } else {
                parts.join(", ")
            }
        }
    }
}

/// RAII lock on a lockfile path.
pub struct LockGuard {
    path: PathBuf,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Acquire an exclusive lock (create_new semantics) with a timeout.
pub fn acquire_lock(lock_path: &Path, timeout: Duration) -> Result<LockGuard> {
    let start = Instant::now();
    loop {
        match OpenOptions::new().write(true).create_new(true).open(lock_path) {
            Ok(_) => return Ok(LockGuard { path: lock_path.to_path_buf() }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if start.elapsed() > timeout {
                    bail!("lock {} held for >{timeout:?}; another unnes process may be running", lock_path.display());
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Append one JSONL entry under the lock.
pub fn append(home: &crate::paths::UnnesHome, entry: &ChangelogEntry) -> Result<()> {
    let file = home.changelog_file();
    let _guard = acquire_lock(&home.lock_file(), Duration::from_secs(10))?;
    let mut f = OpenOptions::new().create(true).append(true).open(&file)?;
    serde_json::to_writer(&mut f, entry)?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Read entries, newest first; optional timestamp + page filters.
pub fn read(
    home: &crate::paths::UnnesHome,
    since: Option<&str>,
    page_id: Option<&str>,
) -> Result<Vec<ChangelogEntry>> {
    let file = home.changelog_file();
    if !file.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&file)?;
    let since_dt = since.map(chrono::DateTime::parse_from_rfc3339).transpose()?;
    let mut out = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<ChangelogEntry>(line) {
            Ok(e) => {
                if let Some(pid) = page_id {
                    if e.page_id != pid {
                        continue;
                    }
                }
                if let Some(sd) = &since_dt {
                    match chrono::DateTime::parse_from_rfc3339(&e.at) {
                        Ok(at) => {
                            if at < *sd {
                                continue;
                            }
                        }
                        Err(_) => continue,
                    }
                }
                out.push(e);
            }
            Err(err) => {
                // Tolerate corrupt lines but surface a warning via stderr? Keep silent
                // for v1: skip unparsable lines rather than failing the whole log.
                let _ = err;
            }
        }
    }
    out.reverse(); // newest first
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::UnnesHome;
    use std::path::PathBuf;
    use std::time::Duration;

    fn tmp_home(tag: &str) -> UnnesHome {
        let dir = std::env::temp_dir().join(format!("unnes-changelog-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = UnnesHome { root: PathBuf::from(&dir) };
        let _ = home.ensure_dirs();
        home
    }

    fn entry(page: &str, event: &str) -> ChangelogEntry {
        ChangelogEntry {
            at: "2026-08-18T00:00:00Z".to_string(),
            page_id: page.to_string(),
            event: event.to_string(),
            added: vec![],
            removed: vec![],
            changed_records: vec![],
            lines_added: vec![],
            lines_removed: vec![],
        }
    }

    #[test]
    fn append_and_read_roundtrip() {
        let home = tmp_home("roundtrip");
        append(&home, &entry("grades", EVENT_CHANGE)).unwrap();
        append(&home, &entry("schedule", EVENT_BASELINE)).unwrap();
        let all = read(&home, None, None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].page_id, "schedule"); // newest first
        let filtered = read(&home, None, Some("grades")).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].page_id, "grades");
    }

    #[test]
    fn since_filter() {
        let home = tmp_home("since");
        append(&home, &entry("grades", EVENT_CHANGE)).unwrap();
        let later = read(&home, Some("2026-08-18T01:00:00Z"), None).unwrap();
        assert!(later.is_empty());
        let earlier = read(&home, Some("2026-08-17T00:00:00Z"), None).unwrap();
        assert_eq!(earlier.len(), 1);
    }

    #[test]
    fn missing_file_reads_empty() {
        let home = tmp_home("missing");
        assert!(read(&home, None, None).unwrap().is_empty());
    }

    #[test]
    fn lock_times_out_then_releases() {
        let home = tmp_home("lock");
        let g1 = acquire_lock(&home.lock_file(), Duration::from_secs(10)).unwrap();
        let g2 = acquire_lock(&home.lock_file(), Duration::from_millis(300));
        assert!(g2.is_err());
        drop(g1);
        let g3 = acquire_lock(&home.lock_file(), Duration::from_secs(10));
        assert!(g3.is_ok());
    }

    #[test]
    fn summary_text() {
        let mut e = entry("grades", EVENT_CHANGE);
        e.added = vec![serde_json::json!({"code": "X"})];
        assert!(e.summary().contains("1 added"));
        let b = entry("grades", EVENT_BASELINE);
        assert_eq!(b.summary(), "baseline recorded");
        let e2 = ChangelogEntry { event: EVENT_CHANGE.to_string(), ..entry("g", EVENT_CHANGE) };
        assert_eq!(e2.summary(), "no visible diff");
    }
}
