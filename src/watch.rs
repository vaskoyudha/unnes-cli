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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rand::Rng;
use serde_json::{json, Value};

use crate::changelog::{self, ChangelogEntry};
use crate::config::{Config, Page};
use crate::data;
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

/// Build the per-page job/entry fields shared by get/page/crawl/batch.
fn page_fields(job: &mut Value, page: &Page) {
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
    page_fields(&mut job, page);
    let res = fetcher::run_job(home, profile, job)?;
    if !res.ok {
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        let msg = res.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        bail!("{msg} ({code})");
    }
    Ok(res)
}

/// When the last login attempt happened (unix ms). Non-interactive callers
/// (the TUI dashboard) skip a second attempt within 4 minutes: a failed
/// scripted login is expensive (~80s) and repeating it per source would
/// stall every remaining panel.
static LAST_LOGIN_ATTEMPT_MS: AtomicU64 = AtomicU64::new(0);

/// Session refresh using the SAVED profile:
/// 1. scripted attempts (zero interaction when Google cooperates),
/// 2. when `interactive`, fall back to the headed window automatically if
///    Google demands interaction (password/2FA/CAPTCHA) - the user clicks
///    once. When not interactive (e.g. the TUI dashboard), the retry fails
///    with needsInteraction instead of blocking on a human click.
pub fn auto_login(home: &UnnesHome, profile: &str, interactive: bool) -> Result<String> {
    if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        LAST_LOGIN_ATTEMPT_MS.store(now.as_millis() as u64, Ordering::Relaxed);
    }
    let mut job = fetcher::job("login", profile);
    job["mode"] = json!("auto");
    job["interactive"] = json!(interactive);
    match fetcher::run_job(home, profile, job) {
        Ok(res) if res.ok => Ok("re-login ok (scripted)".to_string()),
        Ok(res) => {
            let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
            let msg = res.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
            if code == "needsInteraction" && interactive {
                // Google wants a real click: open the headed window automatically.
                let mut hjob = fetcher::job("login", profile);
                hjob["mode"] = json!("browser");
                let res = fetcher::run_job(home, profile, hjob)?;
                if res.ok {
                    return Ok("re-login ok (you clicked once in the window)".to_string());
                }
                let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
                let msg = res.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                bail!("{msg} ({code})");
            }
            bail!("{msg} ({code})");
        }
        Err(e) => bail!("{e:#}"),
    }
}

/// Best-effort: if the gateway session is expired and auto_relogin is enabled,
/// run the scripted re-login once. Errors are swallowed here - the per-page
/// outcomes still report session problems honestly. `interactive` controls
/// whether the re-login may open a window that waits for a human click.
pub fn ensure_session(home: &UnnesHome, profile: &str, interactive: bool) {
    let Ok(cfg) = Config::load(home) else { return };
    if !cfg.general.auto_relogin {
        return;
    }
    let mut job = fetcher::job("get", profile);
    job["url"] = json!("https://apps.unnes.ac.id/gate/list");
    let expired = match fetcher::run_job(home, profile, job) {
        Ok(res) => res.session_expired || !res.ok,
        Err(_) => false,
    };
    if expired {
        if !interactive {
            // A scripted login takes up to ~80s; without this guard every
            // remaining source (jadwal, tugas) would repeat a failed attempt.
            let last = LAST_LOGIN_ATTEMPT_MS.load(Ordering::Relaxed);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if last != 0 && now.saturating_sub(last) < 240_000 {
                return;
            }
        }
        let _ = auto_login(home, profile, interactive);
    }
}

/// One batch job entry for a render/crawl page.
fn batch_entry(page: &Page) -> Value {
    let mut e = json!({ "url": page.url });
    if let Some(sel) = &page.selector {
        e["extract"] = json!({ "selector": sel });
    }
    if let Some(app) = &page.sso_app {
        e["ssoApp"] = json!(app);
    }
    if let Some(sel) = &page.link_selector {
        e["linkSelector"] = json!(sel);
    }
    if let Some(pre) = &page.pre_url {
        e["preUrl"] = json!(pre);
    }
    if let Some(sem) = &page.sso_semester {
        e["semester"] = json!(sem);
    }
    e
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

/// Diff + snapshot + changelog + notify for one fetched page.
fn handle_result(
    home: &UnnesHome,
    page: &Page,
    records: Vec<Value>,
    normalized: Option<String>,
    session_expired: bool,
    challenge: bool,
) -> WatchOutcome {
    if session_expired {
        return WatchOutcome { page_id: page.id.clone(), changed: false, summary: "session expired; run: unnes login".into() };
    }
    if challenge {
        return WatchOutcome { page_id: page.id.clone(), changed: false, summary: "Cloudflare challenge; backing off".into() };
    }

    let path = snapshot_path(home, &page.id);
    let (old_records, old_norm) = load_snapshot(&path);

    let diff = if page.selector.is_some() || page.link_selector.is_some() {
        diff_records(&old_records, &records, page.key_field.as_deref())
    } else if let Some(norm) = &normalized {
        let old = old_norm.unwrap_or_default();
        diff_lines(&old, norm)
    } else {
        diff_records(&old_records, &records, page.key_field.as_deref())
    };

    let changed = has_changes(&diff);
    let summary;
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
        let _ = changelog::append(home, &entry);
        let _ = notify(home, &entry);
        summary = entry.summary();
    } else {
        summary = "no change".to_string();
    }

    // Durable full-state history (deduped): keep every distinct state.
    let _ = data::append(home, &page.id, &records);

    if let Err(e) = save_snapshot(&path, &records, normalized.as_deref()) {
        return WatchOutcome { page_id: page.id.clone(), changed, summary: format!("snapshot error: {e:#}") };
    }
    WatchOutcome { page_id: page.id.clone(), changed, summary }
}

/// One watch pass over all (or one) configured pages.
///
/// Render/crawl pages share ONE persistent browser session (op=batch, SSO
/// primes deduplicated); plain pages are fetched over plain HTTP individually.
pub fn run_pass(home: &UnnesHome, profile: &str, only: Option<&str>) -> Result<Vec<WatchOutcome>> {
    let cfg = Config::load(home)?;
    ensure_session(home, profile, true);
    let pages: Vec<&Page> = cfg.pages.iter().filter(|p| only.map_or(true, |id| p.id == id)).collect();
    let mut out = Vec::new();

    // Phase 1: render/crawl pages in a single browser session.
    let render_pages: Vec<&Page> = pages.iter().filter(|p| p.render.unwrap_or(false) || p.link_selector.is_some()).copied().collect();
    if !render_pages.is_empty() {
        let mut job = fetcher::job("batch", profile);
        job["entries"] = json!(render_pages.iter().map(|p| batch_entry(p)).collect::<Vec<_>>());
        match fetcher::run_job(home, profile, job) {
            Ok(res) if res.ok => {
                let by_url: std::collections::HashMap<&str, &fetcher::BatchPageResult> =
                    res.results.iter().map(|r| (r.url.as_str(), r)).collect();
                for page in &render_pages {
                    match by_url.get(page.url.as_str()) {
                        Some(r) if r.ok => out.push(handle_result(home, page, r.records.clone(), None, r.session_expired, false)),
                        Some(r) => {
                            let msg = r.error.as_ref().map(|e| format!("{} ({})", e.message, e.code)).unwrap_or_else(|| "unknown batch error".into());
                            out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: format!("ERROR: {msg}") });
                        }
                        None => out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: "ERROR: batch returned no result for this page".into() }),
                    }
                }
            }
            Ok(res) => {
                let msg = res.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                for page in &render_pages {
                    out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: format!("ERROR: {msg}") });
                }
            }
            Err(e) => {
                for page in &render_pages {
                    out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: format!("ERROR: {e:#}") });
                }
            }
        }
    }

    // Phase 2: plain pages one by one.
    for page in pages.iter().filter(|p| !(p.render.unwrap_or(false) || p.link_selector.is_some())) {
        let res = match fetch_page(home, profile, page) {
            Ok(r) => r,
            Err(e) => {
                out.push(WatchOutcome { page_id: page.id.clone(), changed: false, summary: format!("ERROR: {e:#}") });
                continue;
            }
        };
        out.push(handle_result(home, page, res.records.clone(), res.normalized.clone(), res.session_expired, res.challenge));
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
