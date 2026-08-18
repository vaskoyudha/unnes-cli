//! Rendering: human tables (comfy-table), JSON, and a small dependency-free CSV writer.

use comfy_table::presets::UTF8_FULL;
use comfy_table::Table;
use serde_json::Value;

/// Record column headers in order of first appearance.
pub fn record_headers(records: &[Value]) -> Vec<String> {
    let mut headers: Vec<String> = Vec::new();
    for r in records {
        if let Value::Object(map) = r {
            for k in map.keys() {
                if !headers.iter().any(|h| h == k) {
                    headers.push(k.clone());
                }
            }
        }
    }
    headers
}

fn fmt_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Render records as a UTF-8 table.
pub fn records_table(records: &[Value]) -> String {
    if records.is_empty() {
        return "(no records)".to_string();
    }
    let headers = record_headers(records);
    let mut t = Table::new();
    t.load_preset(UTF8_FULL);
    t.set_header(headers.iter().map(String::as_str).collect::<Vec<_>>());
    for r in records {
        let row: Vec<String> = headers
            .iter()
            .map(|h| r.get(h).map(fmt_value).unwrap_or_default())
            .collect();
        t.add_row(row);
    }
    t.to_string()
}

/// Minimal RFC-4180-ish CSV (quotes when needed).
pub fn records_csv(records: &[Value]) -> String {
    let headers = record_headers(records);
    let mut out = String::new();
    out.push_str(&headers.join(","));
    out.push('\n');
    for r in records {
        let row: Vec<String> = headers
            .iter()
            .map(|h| csv_escape(&r.get(h).map(fmt_value).unwrap_or_default()))
            .collect();
        out.push_str(&row.join(","));
        out.push('\n');
    }
    out
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Pretty-printed JSON array of records.
pub fn records_json(records: &[Value]) -> String {
    serde_json::to_string_pretty(records).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn table_contains_headers_and_rows() {
        let records = vec![
            json!({"name": "Jaringan Komputer", "grade": "B+"}),
            json!({"name": "Basis Data", "grade": "A"}),
        ];
        let s = records_table(&records);
        assert!(s.contains("name"));
        assert!(s.contains("grade"));
        assert!(s.contains("Jaringan Komputer"));
        assert!(s.contains("Basis Data"));
        assert!(s.contains("B+"));
        assert!(s.contains("A"));
    }

    #[test]
    fn empty_records_short_circuit() {
        assert_eq!(records_table(&[]), "(no records)");
    }

    #[test]
    fn csv_quotes_tricky_fields() {
        let records = vec![json!({"a": "x,y", "b": "plain", "c": 3})];
        let csv = records_csv(&records);
        assert!(csv.lines().next().unwrap().starts_with("a,b,c"));
        assert!(csv.contains("\"x,y\""));
        assert!(csv.contains("plain"));
    }

    #[test]
    fn json_roundtrip() {
        let records = vec![json!({"k": "v"})];
        let s = records_json(&records);
        let back: Vec<Value> = serde_json::from_str(&s).unwrap();
        assert_eq!(back, records);
    }
}
