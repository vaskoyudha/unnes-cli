//! TTL-based local cache for parsed dashboard data (kurikulum, jadwal,
//! tugas, peserta).
//!
//! The TUI's load pipeline used to hit the portals on every launch and on
//! every auto-refresh; static data (curriculum, schedule, rosters) rarely
//! changes, so this cache serves it from disk within a TTL window and the
//! panels render instantly. The manual 'r' refresh bypasses the cache.
//!
//! Layout: <home>/cache/<key>.json holding {"at": <unix seconds>, "data": ...}.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::paths::UnnesHome;

fn cache_file(home: &UnnesHome, key: &str) -> std::path::PathBuf {
    home.cache_dir().join(format!("{key}.json"))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persist a value under a cache key, stamped with the current time.
pub fn save<T: Serialize>(home: &UnnesHome, key: &str, value: &T) -> Result<()> {
    let path = cache_file(home, key);
    fs::create_dir_all(path.parent().unwrap_or(home.cache_dir().as_path()))?;
    let body = serde_json::to_string(value)?;
    let data: serde_json::Value = serde_json::from_str(&body)?;
    let wrapped = serde_json::json!({ "at": now_secs(), "data": data });
    fs::write(&path, serde_json::to_string_pretty(&wrapped)?)
        .with_context(|| format!("cannot write cache {}", path.display()))
}

/// Read a cached value when present and younger than `max_age_secs`.
/// Returns None for a missing file, corrupt content, or an expired entry.
pub fn load<T: DeserializeOwned>(home: &UnnesHome, key: &str, max_age_secs: u64) -> Option<T> {
    let raw = fs::read_to_string(cache_file(home, key)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let at = v.get("at")?.as_u64()?;
    if now_secs().saturating_sub(at) > max_age_secs {
        return None;
    }
    let data = v.get("data")?;
    serde_json::from_value(data.clone()).ok()
}

/// Read a cached value REGARDLESS of age. Used as a fallback when a fresh
/// fetch fails: stale-but-present data beats a blank panel.
pub fn load_any<T: DeserializeOwned>(home: &UnnesHome, key: &str) -> Option<T> {
    let raw = fs::read_to_string(cache_file(home, key)).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let data = v.get("data")?;
    serde_json::from_value(data.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // One unique temp dir per test: the 4 cache tests run in parallel
    // inside one binary and would clobber each other if they shared a path.
    fn home() -> UnnesHome {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "unnes-cache-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        UnnesHome { root: dir }
    }

    #[test]
    fn roundtrip_within_ttl() {
        let h = home();
        assert!(save(&h, "kurikulum", &vec![1u32, 2, 3]).is_ok());
        let got: Option<Vec<u32>> = load(&h, "kurikulum", 3600);
        assert_eq!(got, Some(vec![1, 2, 3]));
        let _ = fs::remove_dir_all(&h.root);
    }

    #[test]
    fn missing_key_returns_none() {
        let h = home();
        let got: Option<Vec<u32>> = load(&h, "nope", 3600);
        assert_eq!(got, None);
        let _ = fs::remove_dir_all(&h.root);
    }

    #[test]
    fn load_any_ignores_age() {
        let h = home();
        assert!(save(&h, "kurikulum", &vec![7u32]).is_ok());
        let path = cache_file(&h, "kurikulum");
        let v = serde_json::json!({ "at": now_secs() - 10_000, "data": [7] });
        fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        // TTL load refuses, load_any still returns
        let fresh: Option<Vec<u32>> = load(&h, "kurikulum", 60);
        assert_eq!(fresh, None);
        let any: Option<Vec<u32>> = load_any(&h, "kurikulum");
        assert_eq!(any, Some(vec![7]));
        let _ = fs::remove_dir_all(&h.root);
    }

    #[test]
    fn expired_entry_returns_none() {
        let h = home();
        assert!(save(&h, "jadwal", &"x".to_string()).is_ok());
        // age the entry: rewrite with a timestamp in the past
        let path = cache_file(&h, "jadwal");
        let v = serde_json::json!({ "at": now_secs() - 10_000, "data": "x" });
        fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();
        let got: Option<String> = load(&h, "jadwal", 60);
        assert_eq!(got, None);
        let _ = fs::remove_dir_all(&h.root);
    }
}
