//! Elena assignment/quiz tracker (tugas): every course's mod_assign / mod_quiz
//! items with due dates and submission status. Shared by the CLI command and
//! the TUI dashboard.

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::json;

use crate::data;
use crate::fetcher;
use crate::paths::UnnesHome;
use crate::watch;

#[derive(Debug, Clone, Serialize)]
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
                "selector": format!("a[href*='{base}/view.php']"),
                "fields": { "nama": "a", "url": "@href" },
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
                    sso_semester: Some("20261".into()),
                    ..Default::default()
                };
                let _ = watch::fetch_page(home, profile, &page);
                res = fetcher::run_job(home, profile, job);
            }
            match res {
                Ok(r) if r.ok && !r.session_expired => {
                    for r in &r.records {
                        let nama = r.get("nama").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
                        let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if nama.is_empty() || url.is_empty() || nama == "Course activities" {
                            continue;
                        }
                        if !url.contains("view.php") {
                            continue;
                        }
                        items.push(TugasItem {
                            course: name.clone(),
                            course_id: *cid,
                            nama,
                            url,
                            due: String::new(),
                            status: String::new(),
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
    for it in items.iter_mut() {
        let mut job = fetcher::job("get", profile);
        job["url"] = json!(it.url);
        let res = match fetcher::run_job(home, profile, job) {
            Ok(r) if r.ok && !r.session_expired => r,
            _ => continue,
        };
        let txt = flatten(&res.normalized.unwrap_or_default());
        it.due = parse_due(&txt);
        it.status = parse_status(&txt);
    }
    items.sort_by(|a, b| {
        let pa = if a.status.is_empty() || a.status == "Belum dikumpulkan" || a.status == "Draft" { 0 } else { 1 };
        let pb = if b.status.is_empty() || b.status == "Belum dikumpulkan" || b.status == "Draft" { 0 } else { 1 };
        pa.cmp(&pb).then_with(|| a.due.cmp(&b.due))
    });
    Ok(items)
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