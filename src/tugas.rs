//! Elena assignment/quiz tracker (tugas): every course's mod_assign / mod_quiz
//! items with due dates and submission status. Shared by the CLI command and
//! the TUI dashboard.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::data;
use crate::fetcher;
use crate::paths::UnnesHome;
use crate::watch;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TugasItem {
    pub course: String,
    pub course_id: u32,
    pub nama: String,
    pub url: String,
    pub due: String,
    pub status: String,
    pub kategori: String,
}

impl TugasItem {
    /// Short flag used by the UIs: OK / BELUM / ?
    pub fn flag(&self) -> &'static str {
        match self.status.as_str() {
            "Submitted" => "OK",
            "Belum dikumpulkan" | "Draft" => "BELUM",
            _ => "?",
        }
    }
}

/// Course ids harvested from the stored elena-kursus crawl (_source urls).
pub fn course_ids(home: &UnnesHome) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    if let Ok(Some(entry)) = data::latest(home, "elena-kursus") {
        for r in &entry.records {
            let src = r.get("_source").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(id) = src.split("id=").nth(1).and_then(|x| x.split(['&', '#']).next()) {
                if let Ok(n) = id.parse::<u32>() {
                    if !out.contains(&n) {
                        out.push(n);
                    }
                }
            }
        }
    }
    out
}

fn flatten(html: &str) -> String {
    let tags = regex::Regex::new(r"<[^>]*>").unwrap();
    regex::Regex::new(r"\s+").unwrap()
        .replace_all(&tags.replace_all(&html.replace('<', " <"), " "), " ")
        .to_string()
}

/// Due date from the item view page text (EN + ID markers).
pub fn parse_due(txt: &str) -> String {
    let re = regex::Regex::new(r"(?i)(due date|batas waktu|jatuh tempo)\s*[:]?\s*([A-Za-z0-9:, ]{8,40})").unwrap();
    re.captures(txt)
        .map(|c| c[2].trim().to_string())
        .unwrap_or_default()
}

/// Submission status from the item view page text (EN + ID markers).
pub fn parse_status(txt: &str) -> String {
    let re = regex::Regex::new(r"(?i)(submission status|status pengumpulan)\s*[:]?\s*([A-Za-z ()]{3,40})").unwrap();
    if let Some(c) = re.captures(txt) {
        let s = c[2].trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let l = txt.to_lowercase();
    if l.contains("submitted for grading") || l.contains("diserahkan untuk dinilai") {
        "Submitted".into()
    } else if l.contains("no attempt") || l.contains("not submitted") || l.contains("belum dikumpulkan") || l.contains("belum ada") {
        "Belum dikumpulkan".into()
    } else if l.contains("draft") {
        "Draft".into()
    } else {
        String::new()
    }
}

/// Collect all Elena assignment/quiz items with due dates and submission
/// status, across every stored course. Unsubmitted items sort first.
///
/// Elena's Moodle session dies independently of the gateway session; when a
/// course page bounces to the SSO login (reported as sessionExpired), the
/// elena session is re-primed ONCE through the browser handshake
/// (sso_app 30) and the fetch retried - exactly like kurikulum/jadwal do
/// for duanol. `interactive` controls whether the gateway re-login may open
/// a click-waiting window (false in the TUI).
pub fn fetch_items(home: &UnnesHome, profile: &str, interactive: bool) -> Result<Vec<TugasItem>> {
    if interactive {
        refresh_stale_courses(home, profile);
    }
    let kursus = course_ids(home);
    if kursus.is_empty() {
        bail!("no courses stored yet - run: unnes watch run (elena-kursus) or unnes discover --elena");
    }
    let mut items: Vec<TugasItem> = Vec::new();
    let mut primed = false;
    let mut session_ok = true;
    // The activities index links each task as /mod/<mod>/view.php?id=...
    for cid in &kursus {
        let name = format!("course-{cid}");
        for (kind, base) in [("Tugas", "mod/assign"), ("Kuis", "mod/quiz")] {
            let mut job = fetcher::job("get", profile);
            job["url"] = json!(format!("https://elena.unnes.ac.id/{base}/index.php?id={cid}"));
            job["extract"] = json!({
                "selector": "tr[data-mdl-overview-cmid]",
                "fields": {
                    "nama": "a.activityname",
                    "url": "a.activityname@href",
                    "due_ts": "td[data-mdl-overview-item='duedate']@data-mdl-overview-value",
                    "status": "td[data-mdl-overview-item='submissionstatus']",
                },
            });
            let mut res = fetcher::run_job(home, profile, job.clone());
            if res.as_ref().map(|r| r.session_expired || !r.ok).unwrap_or(true) && !primed {
                // elena session dead: gateway refresh + browser handshake once
                primed = true;
                watch::ensure_session(home, profile, interactive);
                let page = crate::config::Page {
                    id: "elena-prime".into(),
                    url: "https://elena.unnes.ac.id/my/".into(),
                    render: Some(true),
                    sso_app: Some("30".into()),
                    // semester follows the configured elena pages so a new
                    // term only needs a config bump, not a code change
                    sso_semester: Some(configured_elena_semester(home).unwrap_or_else(|| "20261".into())),
                    ..Default::default()
                };
                let _ = watch::fetch_page(home, profile, &page);
                res = fetcher::run_job(home, profile, job);
            }
            match res {
                Ok(r) if r.ok && !r.session_expired => {
                    // Moodle's activity overview table carries everything in
                    // one row: name link, due-date timestamp, submission
                    // status - one request per course, no detail crawl.
                    let mut recs = r.records.clone();
                    if recs.is_empty() {
                        // Layout without the overview table (e.g. mod/quiz):
                        // fall back to plain activity links; due/status then
                        // come from the per-item detail pass below.
                        let mut fb = fetcher::job("get", profile);
                        fb["url"] = json!(format!("https://elena.unnes.ac.id/{base}/index.php?id={cid}"));
                        fb["extract"] = json!({
                            "selector": format!("a[href*='{base}/view.php']"),
                            "fields": { "nama": "a", "url": "@href" },
                        });
                        if let Ok(r2) = fetcher::run_job(home, profile, fb) {
                            if r2.ok && !r2.session_expired {
                                recs = r2.records;
                            }
                        }
                    }
                    for rec in &recs {
                        let url = rec.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                        if !url.contains("view.php") {
                            continue;
                        }
                        let nama = rec.get("nama").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                        let due_ts = rec.get("due_ts").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                        let due = due_ts.parse::<i64>().ok().map(fmt_due).unwrap_or_default();
                        let status = normalize_status(rec.get("status").and_then(|v| v.as_str()).unwrap_or(""));
                        items.push(TugasItem {
                            course: name.clone(),
                            course_id: *cid,
                            nama,
                            url,
                            due,
                            status,
                            kategori: kind.to_string(),
                        });
                    }
                }
                _ => { session_ok = false; }
            }
        }
    }
    if !session_ok {
        bail!("elena session unavailable; run: unnes login");
    }
    // Fallback only: when the overview row lacked due/status, visit the
    // item page once for the details.
    for it in items.iter_mut() {
        if !it.due.is_empty() && !it.status.is_empty() {
            continue;
        }
        let mut job = fetcher::job("get", profile);
        job["url"] = json!(it.url);
        let res = match fetcher::run_job(home, profile, job) {
            Ok(r) if r.ok && !r.session_expired => r,
            _ => continue,
        };
        let txt = flatten(&res.normalized.unwrap_or_default());
        if it.due.is_empty() {
            it.due = parse_due(&txt);
        }
        if it.status.is_empty() {
            it.status = parse_status(&txt);
        }
    }
    // Friendly course names from the stored elena-kursus crawl
    // ("course-2018" -> "Kriptografi").
    let mut course_names: std::collections::HashMap<u32, String> = Default::default();
    if let Ok(Some(entry)) = data::latest(home, "elena-kursus") {
        for r in &entry.records {
            let src = r.get("_source").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(id) = src.split("id=").nth(1).and_then(|x| x.split(['&', '#']).next()) {
                if let Ok(n) = id.parse::<u32>() {
                    let title = r
                        .get("_title")
                        .or_else(|| r.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let short = title.split(" (").next().unwrap_or(title).trim().to_string();
                    if !short.is_empty() {
                        course_names.entry(n).or_insert(short);
                    }
                }
            }
        }
    }
    for it in items.iter_mut() {
        if let Some(nm) = course_names.get(&it.course_id) {
            it.course = nm.clone();
        }
    }
    items.sort_by(|a, b| {
        let pa = if a.status.is_empty() || a.status == "Belum dikumpulkan" || a.status == "Draft" { 0 } else { 1 };
        let pb = if b.status.is_empty() || b.status == "Belum dikumpulkan" || b.status == "Draft" { 0 } else { 1 };
        pa.cmp(&pb).then_with(|| a.due.cmp(&b.due))
    });
    Ok(items)
}

/// Unix seconds -> "YYYY-MM-DD HH:MM" in local time.
fn fmt_due(ts: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(ts, 0).single() {
        Some(t) => t.format("%Y-%m-%d %H:%M").to_string(),
        None => String::new(),
    }
}

/// The elena semester configured on any sso_app=30 page (e.g. "20261").
pub fn configured_elena_semester(home: &UnnesHome) -> Option<String> {
    let cfg = crate::config::Config::load(home).ok()?;
    cfg.pages
        .iter()
        .find(|p| p.sso_app.as_deref() == Some("30"))
        .and_then(|p| p.sso_semester.clone())
}

/// Re-crawl the elena-kursus course list when the stored one is older than
/// seven days, so newly opened courses show up in tugas without manual
/// `unnes watch run`. Only makes sense where a browser wait is acceptable
/// (the CLI); the TUI keeps using the stored list to stay fast.
fn refresh_stale_courses(home: &UnnesHome, profile: &str) {
    const STALE_AFTER_SECS: u64 = 7 * 24 * 3600;
    let path = data::data_file(home, "elena-kursus");
    let age_ok = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().unwrap_or_default().as_secs() < STALE_AFTER_SECS)
        .unwrap_or(true); // no stored crawl yet: nothing to refresh
    if age_ok {
        return;
    }
    let Ok(cfg) = crate::config::Config::load(home) else { return };
    let Some(page) = cfg.pages.iter().find(|p| p.id == "elena-kursus") else { return };
    watch::ensure_session(home, profile, false);
    let _ = watch::fetch_page(home, profile, page);
}

/// Map the overview table's submission-status wording to the short flags.
/// Handles both the English and Indonesian Moodle UI wordings.
fn normalize_status(s: &str) -> String {
    let l = s.trim().to_lowercase();
    if l.is_empty() {
        return String::new();
    }
    if l.contains("draft") || l.contains("draf") {
        return "Draft".into();
    }
    if l.contains("submitted for grading")
        || l.contains("diserahkan untuk dinilai")
        || l.contains("submitted")
    {
        return "Submitted".into();
    }
    if l.contains("no attempt")
        || l.contains("no submission")
        || l.contains("nothing has been submitted")
        || l.contains("not submitted")
        || l.contains("belum ada submission")
        || l.contains("belum mengumpulkan")
        || l.contains("belum dikumpulkan")
    {
        return "Belum dikumpulkan".into();
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_due_dates_en_and_id() {
        assert_eq!(parse_due("Due date Friday, 30 August 2026, 11:00 PM"), "Friday, 30 August 2026, 11:00 PM");
        assert_eq!(parse_due("Batas waktu Jumat, 30 Agustus 2026, 23:00"), "Jumat, 30 Agustus 2026, 23:00");
        assert_eq!(parse_due("nothing here"), "");
    }

    #[test]
    fn parses_statuses() {
        assert_eq!(parse_status("Submission status Not submitted"), "Not submitted");
        assert_eq!(parse_status("Status pengumpulan Diserahkan untuk dinilai"), "Diserahkan untuk dinilai");
        assert_eq!(parse_status("This submission was submitted for grading"), "Submitted");
        assert_eq!(parse_status("You have not submitted yet"), "Belum dikumpulkan");
    }
}