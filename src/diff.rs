//! Record-level and line-level diffing for the watcher.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffResult {
    pub added: Vec<Value>,
    pub removed: Vec<Value>,
    pub changed: Vec<ChangedRecord>,
    /// Present when the page was diffed as plain text.
    #[serde(default)]
    pub lines_added: Vec<String>,
    #[serde(default)]
    pub lines_removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedRecord {
    pub key: Value,
    pub before: Value,
    pub after: Value,
}

impl ChangedRecord {
    pub fn describe(&self) -> String {
        format!(
            "{}: {} -> {}",
            fmt_short(&self.key),
            fmt_short(&self.before),
            fmt_short(&self.after)
        )
    }
}

fn fmt_short(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "∅".to_string(),
        other => other.to_string(),
    }
}

/// Stable record key: key_field when given, else the first object key's value.
fn record_key(r: &Value, key_field: Option<&str>) -> Option<String> {
    let map = r.as_object()?;
    let key = key_field
        .filter(|k| map.contains_key(*k))
        .map(|k| k.to_string())
        .or_else(|| map.keys().next().cloned())?;
    map.get(&key).map(|v| match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// Diff old -> new record sets; returns entries in new-order (added first),
/// then removed (old-order) and changed (new-order).
pub fn diff_records(old: &[Value], new: &[Value], key_field: Option<&str>) -> DiffResult {
    let key_of = |r: &Value| record_key(r, key_field);

    let old_keys: std::collections::HashSet<String> =
        old.iter().filter_map(|r| key_of(r)).collect();

    let mut out = DiffResult::default();

    // New records: added or changed (compared by value equality).
    for r in new {
        match key_of(r) {
            None => {
                // No stable key; treat as added unless byte-identical to some old record.
                if !old.contains(r) {
                    out.added.push(r.clone());
                }
            }
            Some(k) => {
                if !old_keys.contains(&k) {
                    out.added.push(r.clone());
                } else if let Some(old_r) = old.iter().find(|o| key_of(o).as_deref() == Some(&k)) {
                    if old_r != r {
                        out.changed.push(ChangedRecord {
                            key: Value::String(k.clone()),
                            before: old_r.clone(),
                            after: r.clone(),
                        });
                    }
                }
            }
        }
    }

    // Old records missing from new: removed.
    let new_keys: std::collections::HashSet<String> =
        new.iter().filter_map(|r| key_of(r)).collect();
    for r in old {
        match key_of(r) {
            None => {
                if !new.contains(r) {
                    out.removed.push(r.clone());
                }
            }
            Some(k) => {
                if !new_keys.contains(&k) {
                    out.removed.push(r.clone());
                }
            }
        }
    }

    out
}

/// Plain-text line diff via LCS; falls back to full-replace beyond a size cap.
pub fn diff_lines(old: &str, new: &str) -> DiffResult {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut out = DiffResult::default();
    if a.len() * b.len() > 4_000_000 {
        out.lines_removed = a.iter().map(|s| s.to_string()).collect();
        out.lines_added = b.iter().map(|s| s.to_string()).collect();
        return out;
    }
    // LCS DP
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.lines_removed.push(a[i].to_string());
            i += 1;
        } else {
            out.lines_added.push(b[j].to_string());
            j += 1;
        }
    }
    while i < n {
        out.lines_removed.push(a[i].to_string());
        i += 1;
    }
    while j < m {
        out.lines_added.push(b[j].to_string());
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn no_change_is_empty() {
        let r = vec![json!({"code": "A", "grade": "B+"})];
        let d = diff_records(&r, &r, Some("code"));
        assert!(d.added.is_empty() && d.removed.is_empty() && d.changed.is_empty());
    }

    #[test]
    fn added_and_removed() {
        let old = vec![json!({"code": "A", "grade": "B+"})];
        let new = vec![
            json!({"code": "A", "grade": "B+"}),
            json!({"code": "B", "grade": "A"}),
        ];
        let d = diff_records(&old, &new, Some("code"));
        assert_eq!(d.added.len(), 1);
        assert!(d.removed.is_empty() && d.changed.is_empty());
        assert_eq!(d.added[0]["code"], "B");

        let d2 = diff_records(&new, &old, Some("code"));
        assert_eq!(d2.removed.len(), 1);
        assert_eq!(d2.removed[0]["code"], "B");
    }

    #[test]
    fn changed_same_key() {
        let old = vec![json!({"code": "C1", "grade": "B+"})];
        let new = vec![json!({"code": "C1", "grade": "A"})];
        let d = diff_records(&old, &new, Some("code"));
        assert!(d.added.is_empty() && d.removed.is_empty());
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].key, "C1");
        assert_eq!(d.changed[0].after["grade"], "A");
        let s = d.changed[0].describe();
        assert!(s.contains("C1") && s.contains("B+") && s.contains("A") && s.contains("->"));
    }

    #[test]
    fn default_key_is_first_sorted_field() {
        // serde_json maps sort keys, so "first field" = first key in sort order.
        let old = vec![json!({"a": "1", "g": "B"})];
        let new = vec![json!({"a": "1", "g": "A"})];
        let d = diff_records(&old, &new, None);
        assert_eq!(d.changed.len(), 1);
        assert_eq!(d.changed[0].key, "1");
    }

    #[test]
    fn line_diff_simple() {
        let old = "line1\nline2\n";
        let new = "line1\nline3\n";
        let d = diff_lines(old, new);
        assert_eq!(d.lines_removed, vec!["line2".to_string()]);
        assert_eq!(d.lines_added, vec!["line3".to_string()]);
    }

    #[test]
    fn line_diff_identical() {
        let d = diff_lines("a\nb\n", "a\nb\n");
        assert!(d.lines_added.is_empty() && d.lines_removed.is_empty());
    }
}
