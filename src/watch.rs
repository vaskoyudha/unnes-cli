//! Watch engine: poll configured pages, diff against snapshots, log changes.
//!
//! One pass (run_pass) does, per page: fetch (same dispatch as unnes fetch,
//! including sso exchange / browser render / crawls) -> compare with the last
//! snapshot -> on change append a changelog entry and fire the notify hook ->
//! persist the fresh snapshot. The daemon loops passes with adaptive intervals,
//! jitter and backoff-friendly sleeping.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rand::Rng;
use serde_json::{json, Value};

use crate::changelog::{self, ChangelogEntry};
use crate::config::{Config, Page};
use crate::diff::{diff_lines, diff_records, DiffResult};
use crate::fetcher::{self, JobResult};
use chrono::{Datelike, Timelike};

use crate::paths::UnnesHome;

/// One page's outcome in a pass.
#[derive(Debug, Clone)]
pub struct WatchOutcome {
    pub page_id: String,
    pub changed: bool,
    pub summary: String,
}

/// Fetch one configured page using the same dispatch as `unnes fetch`
/// (plain HTTP with auto sso bootstrap, browser render, or crawl).
pub fn fetch_page(home: &UnnesHome, profile: &str, page: &Page) -> Result<JobResult> {
    let mut job = if page.link_selector.is_some() {
        fetcher::job("crawl", profile)
    } else if page.render.unwrap_or(false) {
        fetcher::job("page", profile)
    } else {
        fetcher::job("get", profile)
    };
    job["url"] = json!(page.url);
    if let Some(sel) = &page.selector {
        job["extract"] = json!({ "selector": sel });
    }
    if !page.normalize.is_empty() {
        job["extraRegexes"] = json!(page.normalize);
    }
    if let Some(app) = &page.sso_app {
        job["ssoApp"] = json!(app);
    }
    if let Some(sel) = &page.link_selector {
        job["linkSelector"] = json!(sel);
    }
    if let Some(pre) = &page.pre_url {
        job["preUrl"] = json!(pre);
    }
    if let Some(sem) = &page.sso_semester {
        job["semester"] = json!(sem);
    }
    let res = fetcher::run_job(home, profile, job)?;
    if !res.ok {
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        let msg = res.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        bail!("{msg} ({code})");
    }
    Ok(res)
}

fn snapshot_path(home: &UnnesHome, page_id: &str) -> std::path::PathBuf {
    home.snapshots_dir().join(format!("{page_id}.json"))
}

fn load_snapshot(path: &std::path::Path) -> (Vec<Value>, Option<String>) {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(_) => return (Vec::new(), None),
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => (
            v.get("records").and_then(|r| r.as_array()).cloned().unwrap_or_default(),
            v.get("normalized").and_then(|n| n.as_str()).map(String::from),
        ),
        Err(_) => (Vec::new(), None),
    }
}

fn save_snapshot(path: &std::path::Path, records: &[Value], normalized: Option<&str>) -> Result<()> {
    let v = json!({ "records": records, "normalized": normalized });
    fs::write(path, serde_json::to_string_pretty(&v)?).with_context(|| format!("cannot write snapshot {}", path.display()))
}

fn has_changes(d: &DiffResult) -> bool {
    !d.added.is_empty() || !d.removed.is_empty() || !d.changed.is_empty()
        || !d.lines_added.is_empty() || !d.lines_removed.is_empty()
}

/// Run the notify hook (config.notify.command) with the entry JSON on stdin.
fn notify(home: &UnnesHome, entry: &ChangelogEntry) -> Result<()> {
    let cfg = Config::load(home)?;
    let Some(cmd) = cfg.notify.command.clone() else { return Ok(()) };
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("notify command failed to spawn: {cmd}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = serde_json::to_writer(&mut stdin, entry);
        let _ = stdin.flush();
    }
    let _ = child.wait();
    Ok(())
}

/// One watch pass over all (or one) configured pages.
pub fn run_pass(home: &UnnesHome, profile: &str, only: Option<&str>) -> Result<Vec<WatchOutcome>> {
    let cfg = Config::load(home)?;
    let mut out = Vec::new();
    for page in cfg.pages.iter().filter(|p| only.map_or(true, |id| p.id == id)) {
        // 1. fetch
        let res = match fetch_page(home, profile, page) {
            Ok(r) => r,
            Err(e) => {
                out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: format!("ERROR: {e:#}") });
                continue;
            }
        };
        if res.session_expired {
            out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: "session expired; run: unnes login".into() });
            continue;
        }
        if res.challenge {
            out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: "Cloudflare challenge; backing off".into() });
            continue;
        }

        // 2. diff against the snapshot
        let path = snapshot_path(home, &page.id);
        let (old_records, old_norm) = load_snapshot(&path);
        let new_records = res.records.clone();
        let new_norm = res.normalized.clone();

        let diff = if page.selector.is_some() || page.link_selector.is_some() {
            diff_records(&old_records, &new_records, page.key_field.as_deref())
        } else if let Some(norm) = &new_norm {
            let old = old_norm.unwrap_or_default();
            diff_lines(&old, norm)
        } else {
            diff_records(&old_records, &new_records, page.key_field.as_deref())
        };

        let changed = has_changes(&diff);
        if changed {
            let entry = ChangelogEntry {
                at: chrono::Utc::now().to_rfc3339(),
                page_id: page.id.clone(),
                event: "changed".to_string(),
                added: diff.added.clone(),
                removed: diff.removed.clone(),
                changed_records: diff.changed.clone(),
                lines_added: diff.lines_added.clone(),
                lines_removed: diff.lines_removed.clone(),
            };
            changelog::append(home, &entry)?;
            notify(home, &entry)?;
            out.push(WatchOutcome { page_id: page.id.clone(), changed: true, summary: entry.summary() });
        } else {
            out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: "no change".to_string() });
        }

        // 3. persist the fresh snapshot (baseline stays current)
        if let Err(e) = save_snapshot(&path, &new_records, new_norm.as_deref()) {
            out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: format!("snapshot error: {e:#}") });
        }
    }
    Ok(out)
}

/// Adaptive interval: inside a declared window use its interval, else default.
fn next_interval(cfg: &Config) -> u64 {
    let now = chrono::Local::now();
    let now_min = now.month() as u32 * 31 * 24 * 60 + now.day() as u32 * 24 * 60
        + now.hour() as u32 * 60 + now.minute() as u32;
    for w in &cfg.general.adaptive {
        if let (Some(s), Some(e)) = (window_minutes(&w.start), window_minutes(&w.end)) {
            let inside = if s <= e { now_min >= s && now_min <= e } else { now_min >= s || now_min <= e };
            if inside {
                return w.interval.max(cfg.general.min_interval);
            }
        }
    }
    cfg.general.default_interval
}

/// Parse "MM-DD HH:MM" into minutes since Jan 1.
fn window_minutes(s: &str) -> Option<u32> {
    let s = s.trim();
    let (date, time) = s.split_once(' ')?;
    let (m, d) = date.split_once('-')?;
    let (h, mi) = time.split_once(':')?;
    Some(
        m.parse::<u32>().ok()? * 31 * 24 * 60
            + d.parse::<u32>().ok()? * 24 * 60
            + h.parse::<u32>().ok()? * 60
            + mi.parse::<u32>().ok()?,
    )
}

/// Polling daemon: run passes forever with adaptive, jittered intervals.
pub fn daemon(home: &UnnesHome, profile: &str) -> Result<()> {
    let cfg = Config::load(home)?;
    if cfg.pages.is_empty() {
        bail!("no pages configured; add one with: unnes watch add <id> --url=... --selector=...");
    }
    println!("watching {} page(s) every ~{}s (^C to stop)", cfg.pages.len(), cfg.general.default_interval);
    loop {
        let started = Instant::now();
        match run_pass(home, profile, None) {
            Ok(outcomes) => {
                for o in &outcomes {
                    let mark = if o.changed { "CHANGED" } else { "ok" };
                    println!("[{}] {} {}", o.page_id, mark, o.summary);
                }
            }
            Err(e) => println!("pass error: {e:#}"),
        }
        let base = next_interval(&cfg);
        let jitter = rand::thread_rng().gen_range(-1.0..=1.0) * cfg.general.jitter_fraction * base as f64;
        let target = (base as f64 + jitter).max(cfg.general.min_interval as f64) as u64;
        let elapsed = started.elapsed().as_secs();
        let sleep = target.saturating_sub(elapsed);
        std::thread::sleep(Duration::from_secs(sleep));
    }
}

/// watch add: persist a new page into config.toml.
#[allow(clippy::too_many_arguments)]
pub fn add_page(
    home: &UnnesHome,
    id: &str,
    url: &str,
    selector: Option<String>,
    interval: Option<u64>,
    key_field: Option<String>,
    render: bool,
    sso_app: Option<String>,
    pre_url: Option<String>,
    link_selector: Option<String>,
    sso_semester: Option<String>,
) -> Result<()> {
    if id.trim().is_empty() || id.contains(char::is_whitespace) {
        bail!("invalid page id '{id}' (no spaces)");
    }
    let mut cfg = Config::load(home)?;
    if cfg.page(id).is_some() {
        bail!("page '{id}' already configured; remove it first (unnes watch rm {id})");
    }
    cfg.pages.push(Page {
        id: id.to_string(),
        url: url.to_string(),
        interval,
        selector,
        key_field,
        normalize: Vec::new(),
        render: render.then_some(true),
        sso_app,
        pre_url,
        link_selector,
        sso_semester,
    });
    let toml = cfg.to_toml()?;
    fs::write(home.config_file(), toml).with_context(|| format!("cannot write {}", home.config_file().display()))?;
    Ok(())
}

/// watch rm: remove a page from config.toml.
pub fn rm_page(home: &UnnesHome, id: &str) -> Result<()> {
    let mut cfg = Config::load(home)?;
    let before = cfg.pages.len();
    cfg.pages.retain(|p| p.id != id);
    if cfg.pages.len() == before {
        bail!("page '{id}' is not configured");
    }
    let toml = cfg.to_toml()?;
    fs::write(home.config_file(), toml)?;
    let _ = fs::remove_file(snapshot_path(home, id));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_window_math() {
        let cfg = Config::try_from_str(
            r#"
[general]
default_interval = 900
[[general.adaptive]]
label = "release"
start = "01-01 00:00"
end = "12-31 23:59"
interval = 60
"#,
        )
        .unwrap();
        assert_eq!(next_interval(&cfg), 60);
    }

    #[test]
    fn window_minutes_parses() {
        assert_eq!(window_minutes("08-18 10:30"), Some(8 * 31 * 24 * 60 + 18 * 24 * 60 + 10 * 60 + 30));
        assert_eq!(window_minutes("bad"), None);
    }
}
