//! Durable full-state history per page (data/<page-id>.jsonl).
//!
//! Every successful fetch/watch pass appends the page's records as a
//! timestamped entry - but only when the state differs from the last stored
//! one, so the history stays a compact version chain of distinct states.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::paths::UnnesHome;

/// data/<page-id>.jsonl
pub fn data_file(home: &UnnesHome, page_id: &str) -> PathBuf {
    home.data_dir().join(format!("{page_id}.jsonl"))
}

/// One stored state.
#[derive(Debug, Clone)]
pub struct DataEntry {
    pub at: String,
    pub records: Vec<Value>,
}

/// Read all stored states, oldest first.
pub fn read(home: &UnnesHome, page_id: &str) -> Result<Vec<DataEntry>> {
    let path = data_file(home, page_id);
    let mut out = Vec::new();
    if !path.exists() {
        return Ok(out);
    }
    let raw = fs::read_to_string(&path)?;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            out.push(DataEntry {
                at: v.get("at").and_then(|a| a.as_str()).unwrap_or("").to_string(),
                records: v.get("records").and_then(|r| r.as_array()).cloned().unwrap_or_default(),
            });
        }
    }
    Ok(out)
}

/// Latest stored state, if any.
pub fn latest(home: &UnnesHome, page_id: &str) -> Result<Option<DataEntry>> {
    Ok(read(home, page_id)?.pop())
}

/// Append a state unless it is identical to the last stored one.
/// Returns true when a new entry was written.
pub fn append(home: &UnnesHome, page_id: &str, records: &[Value]) -> Result<bool> {
    let path = data_file(home, page_id);
    let current = latest(home, page_id)?;
    if let Some(last) = &current {
        if last.records == records {
            return Ok(false);
        }
    }
    fs::create_dir_all(path.parent().unwrap_or(home.data_dir().as_path()))
        .with_context(|| format!("cannot create {}", home.data_dir().display()))?;
    let entry = json!({ "at": chrono::Utc::now().to_rfc3339(), "records": records });
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    serde_json::to_writer(&mut f, &entry)?;
    f.write_all(b"
")?;
    Ok(true)
}

/// Number of distinct stored states for a page.
pub fn count(home: &UnnesHome, page_id: &str) -> usize {
    read(home, page_id).map(|e| e.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn home() -> UnnesHome {
        let dir = std::env::temp_dir().join(format!("unnes-data-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        UnnesHome { root: dir }
    }

    #[test]
    fn append_dedupes_and_reads_back() {
        let h = home();
        let r1 = vec![json!({"a": 1})];
        let r2 = vec![json!({"a": 2})];
        assert!(append(&h, "krs", &r1).unwrap());
        assert!(!append(&h, "krs", &r1).unwrap()); // identical -> skipped
        assert!(append(&h, "krs", &r2).unwrap());
        let entries = read(&h, "krs").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].records, r1);
        assert_eq!(entries[1].records, r2);
        assert_eq!(latest(&h, "krs").unwrap().unwrap().records, r2);
        assert_eq!(count(&h, "krs"), 2);
        assert_eq!(count(&h, "nope"), 0);
        let _ = fs::remove_dir_all(&h.root);
    }
}
