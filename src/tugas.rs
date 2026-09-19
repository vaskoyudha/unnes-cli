//! Elena assignment/quiz tracker (tugas): every course's mod_assign / mod_quiz
//! items with due dates and submission status. Shared by the CLI command and
//! the TUI dashboard.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

/// Friendly course names from the stored elena-kursus crawl
/// ("course-2018" -> "Kriptografi"). Shared by tugas + materi.
pub fn course_names(home: &UnnesHome) -> std::collections::HashMap<u32, String> {
    let mut out: std::collections::HashMap<u32, String> = Default::default();
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
                        out.entry(n).or_insert(short);
                    }
                }
            }
        }
    }
    out
}

/// Deadline urgency of one item (pure, testable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    Done,
    Overdue,
    Hours(i64),
    Days(i64),
    Unknown,
}

pub fn urgency_at(due: &str, submitted: bool, now: chrono::NaiveDateTime) -> Urgency {
    if submitted {
        return Urgency::Done;
    }
    let Ok(d) = chrono::NaiveDateTime::parse_from_str(due, "%Y-%m-%d %H:%M") else {
        return Urgency::Unknown;
    };
    let left = d - now;
    if left < chrono::Duration::zero() {
        Urgency::Overdue
    } else if left <= chrono::Duration::hours(48) {
        Urgency::Hours(left.num_hours())
    } else {
        Urgency::Days(left.num_days())
    }
}

pub fn urgency(it: &TugasItem) -> Urgency {
    urgency_at(&it.due, it.status == "Submitted", chrono::Local::now().naive_local())
}

/// Text warning marker for the urgency (CLI tables + popups).
pub fn urgency_mark(u: Urgency) -> &'static str {
    match u {
        Urgency::Overdue => "!!! TERLAMBAT",
        Urgency::Hours(h) if h <= 24 => "!! <24 jam",
        Urgency::Hours(_) => "!! <48 jam",
        Urgency::Days(d) if d <= 7 => "! <7 hari",
        Urgency::Days(_) => "",
        Urgency::Unknown => "? tanpa deadline",
        Urgency::Done => "OK",
    }
}

pub fn is_pending(it: &TugasItem) -> bool {
    it.status.is_empty() || it.status == "Belum dikumpulkan" || it.status == "Draft"
}

fn urgency_rank(it: &TugasItem) -> (u8, i64) {
    match urgency(it) {
        Urgency::Overdue => (0, 0),
        Urgency::Hours(h) => (1, h),
        Urgency::Days(d) => (2, d),
        Urgency::Unknown => (3, 0),
        Urgency::Done => (4, 0),
    }
}

/// Unsubmitted items, nearest deadline first (unknown due dates last).
/// Powers the "HARUS DIKUMPULKAN" popup in the CLI + TUI.
pub fn pending_sorted(items: &[TugasItem]) -> Vec<&TugasItem> {
    let mut v: Vec<&TugasItem> = items.iter().filter(|it| is_pending(it)).collect();
    v.sort_by_key(|it| urgency_rank(it));
    v
}

/// Distinct courses with (total, pending) counts, sorted by name.
/// Powers the "pilih matakuliah dulu" course picker.
pub fn courses(items: &[TugasItem]) -> Vec<(String, usize, usize)> {
    let mut map: std::collections::BTreeMap<String, (usize, usize)> = Default::default();
    for it in items {
        let e = map.entry(it.course.clone()).or_insert((0, 0));
        e.0 += 1;
        if is_pending(it) {
            e.1 += 1;
        }
    }
    map.into_iter().map(|(c, (t, p))| (c, t, p)).collect()
}

/// Case-insensitive substring filter on course name (falls back to item name).
pub fn filter_course<'a>(items: &'a [TugasItem], f: &str) -> Vec<&'a TugasItem> {
    let q = f.to_lowercase();
    items
        .iter()
        .filter(|it| it.course.to_lowercase().contains(&q) || it.nama.to_lowercase().contains(&q))
        .collect()
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

/// Re-prime the independent Elena (Moodle) session once through the browser
/// handshake (sso_app 30). Shared by the tugas + materi fetches: Elena's
/// session dies independently of the gateway session.
pub fn prime_elena(home: &UnnesHome, profile: &str, interactive: bool) {
    // Notice only on interactive terminals: the TUI shares stdout with the
    // dashboard, so background refreshes must stay silent.
    if interactive {
        eprintln!("sesi Elena habis, memulihkan otomatis (jendela browser hanya bila Google minta klik)...");
    }
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
}

/// True when a fetch result means "the Elena session died" (worth one
/// browser prime). Transient transport errors are NOT session deaths:
/// priming a browser for those just hangs the command on a headed window.
pub fn needs_elena_prime(res: &Result<crate::fetcher::JobResult>) -> bool {
    matches!(res, Ok(r) if r.session_expired)
}

/// (kind label, mod path) probed per course, in fixed order.
const KINDS: [(&str, &str); 2] = [("Tugas", "mod/assign"), ("Kuis", "mod/quiz")];

/// One (url, extract-spec) pair per (course, kind) overview page, in order:
/// for each cid, [assign, quiz]. Pure (no I/O) so the wiring is testable.
fn index_pairs(kursus: &[u32]) -> Vec<(String, Value)> {
    let mut out = Vec::with_capacity(kursus.len() * KINDS.len());
    for cid in kursus {
        for (_kind, base) in KINDS {
            out.push((
                format!("https://elena.unnes.ac.id/{base}/index.php?id={cid}"),
                json!({
                    "selector": "tr[data-mdl-overview-cmid]",
                    "fields": {
                        "nama": "a.activityname",
                        "url": "a.activityname@href",
                        "due_ts": "td[data-mdl-overview-item='duedate']@data-mdl-overview-value",
                        "status": "td[data-mdl-overview-item='submissionstatus']",
                    },
                }),
            ));
        }
    }
    out
}

/// Plain activity-link extract for one (course, kind) index page: used when
/// the overview table is absent (e.g. mod/quiz layouts). Pure.
fn fallback_extract(base: &str) -> Value {
    json!({
        "selector": format!("a[href*='{base}/view.php']"),
        // "" = the <a> itself (a descendant "a" selector would match
        // nothing and blank every name).
        "fields": { "nama": "", "url": "@href" },
    })
}

/// Fold one overview/fallback entry's records into items. Returns the
/// (cid, kind, base) triple when the overview table was absent AND another
/// fallback is allowed, so the caller can refetch with `fallback_extract`.
/// Non-view.php links are skipped, exactly like the old per-course loop.
fn merge_index_entry(
    items: &mut Vec<TugasItem>,
    records: &[Value],
    cid: u32,
    course: &str,
    kind: &str,
    base: &str,
    allow_fallback: bool,
) -> Option<(u32, String, String)> {
    if records.is_empty() {
        return allow_fallback.then(|| (cid, kind.to_string(), base.to_string()));
    }
    for rec in records {
        let url = rec.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if !url.contains("view.php") {
            continue;
        }
        let nama = rec.get("nama").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let due_ts = rec.get("due_ts").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let due = due_ts.parse::<i64>().ok().map(fmt_due).unwrap_or_default();
        let status = normalize_status(rec.get("status").and_then(|v| v.as_str()).unwrap_or(""));
        items.push(TugasItem {
            course: course.to_string(),
            course_id: cid,
            nama,
            url,
            due,
            status,
            kategori: kind.to_string(),
        });
    }
    None
}

/// Fill an item's missing due/status from a detail page's normalized text.
/// Never overwrites present values. Pure.
fn apply_detail(item: &mut TugasItem, normalized: &str) {
    let txt = flatten(normalized);
    if item.due.is_empty() {
        item.due = parse_due(&txt);
    }
    if item.status.is_empty() {
        item.status = parse_status(&txt);
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
    let mut session_ok = true;
    // (cid, kind, base) context per pair, in index_pairs order.
    let ctx: Vec<(u32, &str, &str)> = kursus
        .iter()
        .flat_map(|cid| KINDS.iter().map(|(k, b)| (*cid, *k, *b)))
        .collect();
    // The activities index links each task as /mod/<mod>/view.php?id=...
    // One batchget op (one spawn, one jar) instead of one spawn per page.
    let pairs = index_pairs(&kursus);
    let mut batch = fetcher::run_batchget(home, profile, &pairs);
    if batch.as_ref().map(|r| r.session_expired).unwrap_or(false) {
        // elena session dead: gateway refresh + browser handshake once,
        // then rerun ONLY the expired entries (good results stand).
        prime_elena(home, profile, interactive);
        let expired: Vec<usize> = batch
            .as_ref()
            .map(|r| {
                r.results
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.session_expired)
                    .map(|(i, _)| i)
                    .collect()
            })
            .unwrap_or_default();
        if !expired.is_empty() {
            let sub: Vec<(String, Value)> = expired.iter().map(|&i| pairs[i].clone()).collect();
            if let Ok(r2) = fetcher::run_batchget(home, profile, &sub) {
                if let Ok(b) = batch.as_mut() {
                    for (k, &i) in expired.iter().enumerate() {
                        if let Some(rep) = r2.results.get(k) {
                            b.results[i] = rep.clone();
                        }
                    }
                    b.session_expired = b.results.iter().any(|e| e.session_expired);
                }
            }
        }
    }
    // Fallback refetches for overview-less pages (same base URL, plain
    // activity-link extract, no further fallback, no prime - like before).
    let mut fallbacks: Vec<(u32, String, String)> = Vec::new();
    match batch {
        Ok(r) => {
            for (entry, &(cid, kind, base)) in r.results.iter().zip(ctx.iter()) {
                if !(entry.ok && !entry.session_expired) {
                    session_ok = false;
                    continue;
                }
                // Moodle's activity overview table carries everything in
                // one row: name link, due-date timestamp, submission
                // status - one request per course, no detail crawl.
                if let Some(fb) = merge_index_entry(
                    &mut items,
                    &entry.records,
                    cid,
                    &format!("course-{cid}"),
                    kind,
                    base,
                    true,
                ) {
                    fallbacks.push(fb);
                }
            }
        }
        Err(e) => {
            if interactive {
                eprintln!("peringatan: tugas gagal diambil: {e:#}");
            }
            session_ok = false;
        }
    }
    if !fallbacks.is_empty() {
        // Layout without the overview table (e.g. mod/quiz): plain activity
        // links; due/status then come from the per-item detail pass below.
        let sub: Vec<(String, Value)> = fallbacks
            .iter()
            .map(|(cid, _kind, base)| {
                (
                    format!("https://elena.unnes.ac.id/{base}/index.php?id={cid}"),
                    fallback_extract(base),
                )
            })
            .collect();
        if let Ok(r2) = fetcher::run_batchget(home, profile, &sub) {
            for (entry, fb) in r2.results.iter().zip(fallbacks.iter()) {
                if entry.ok && !entry.session_expired {
                    merge_index_entry(&mut items, &entry.records, fb.0, &format!("course-{}", fb.0), &fb.1, &fb.2, false);
                }
            }
        }
    }
    if !session_ok {
        bail!("elena session unavailable; run: unnes login");
    }
    // Fallback only: when the overview row lacked due/status, visit the
    // item page once for the details. One batchget op for all needy items
    // (no prime: failures here just keep the empty fields, like before).
    let needy: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.due.is_empty() || it.status.is_empty())
        .map(|(i, _)| i)
        .collect();
    if !needy.is_empty() {
        let sub: Vec<(String, Value)> = needy.iter().map(|&i| (items[i].url.clone(), Value::Null)).collect();
        if let Ok(r) = fetcher::run_batchget(home, profile, &sub) {
            for (entry, &i) in r.results.iter().zip(needy.iter()) {
                if entry.ok && !entry.session_expired {
                    if let Some(norm) = entry.normalized.as_deref() {
                        apply_detail(&mut items[i], norm);
                    }
                }
            }
        }
    }
    // Friendly course names from the stored elena-kursus crawl
    // ("course-2018" -> "Kriptografi").
    let course_names = course_names(home);
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

    #[test]
    fn index_pairs_cover_every_course_twice_in_order() {
        let p = index_pairs(&[11, 22]);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0].0, "https://elena.unnes.ac.id/mod/assign/index.php?id=11");
        assert_eq!(p[1].0, "https://elena.unnes.ac.id/mod/quiz/index.php?id=11");
        assert_eq!(p[2].0, "https://elena.unnes.ac.id/mod/assign/index.php?id=22");
        assert_eq!(p[3].0, "https://elena.unnes.ac.id/mod/quiz/index.php?id=22");
        assert_eq!(p[0].1["selector"], json!("tr[data-mdl-overview-cmid]"));
        assert_eq!(p[0].1["fields"]["url"], json!("a.activityname@href"));
    }

    #[test]
    fn merge_index_entry_parses_filters_and_falls_back() {
        let mut items = Vec::new();
        let recs = vec![
            json!({"url": "https://elena.unnes.ac.id/mod/assign/view.php?id=5", "nama": "T1", "due_ts": "1767225600", "status": "Submitted"}),
            json!({"url": "https://elena.unnes.ac.id/about", "nama": "X", "due_ts": "", "status": ""}),
        ];
        let fb = merge_index_entry(&mut items, &recs, 11, "course-11", "Tugas", "mod/assign", true);
        assert!(fb.is_none(), "complete overview must not refetch");
        assert_eq!(items.len(), 1, "non-view.php links are skipped");
        assert_eq!(items[0].nama, "T1");
        assert_eq!(items[0].status, "Submitted");
        assert_eq!(items[0].kategori, "Tugas");
        assert!(!items[0].due.is_empty());
        let fb = merge_index_entry(&mut items, &[], 11, "course-11", "Tugas", "mod/assign", true);
        assert_eq!(fb, Some((11, "Tugas".to_string(), "mod/assign".to_string())));
        assert!(merge_index_entry(&mut items, &[], 11, "course-11", "Tugas", "mod/assign", false).is_none());
    }

    #[test]
    fn apply_detail_fills_only_empties() {
        let mut it = titem("c", "n", "", "");
        apply_detail(&mut it, "Batas waktu Jumat, 30 Agustus 2026, 23:00");
        assert_eq!(it.due, "Jumat, 30 Agustus 2026, 23:00");
        let mut it = titem("c", "n", "", "");
        apply_detail(&mut it, "Status pengumpulan Diserahkan untuk dinilai");
        assert_eq!(it.status, "Diserahkan untuk dinilai");
        let mut it = titem("c", "n", "keep-due", "keep-status");
        apply_detail(&mut it, "Batas waktu Jumat, 30 Agustus 2026, 23:00 Status pengumpulan Diserahkan untuk dinilai");
        assert_eq!((it.due.as_str(), it.status.as_str()), ("keep-due", "keep-status"));
    }

    fn titem(course: &str, nama: &str, due: &str, status: &str) -> TugasItem {
        TugasItem {
            course: course.into(),
            course_id: 1,
            nama: nama.into(),
            url: "https://elena.unnes.ac.id/mod/assign/view.php?id=1".into(),
            due: due.into(),
            status: status.into(),
            kategori: "Tugas".into(),
        }
    }

    #[test]
    fn urgency_marks_deadlines() {
        use chrono::NaiveDateTime;
        let now = NaiveDateTime::parse_from_str("2026-09-18 12:00", "%Y-%m-%d %H:%M").unwrap();
        assert_eq!(urgency_at("2026-09-17 12:00", false, now), Urgency::Overdue);
        assert_eq!(urgency_at("2026-09-18 18:00", false, now), Urgency::Hours(6));
        assert_eq!(urgency_at("2026-09-25 12:00", false, now), Urgency::Days(7));
        assert_eq!(urgency_at("", false, now), Urgency::Unknown);
        assert_eq!(urgency_at("2026-09-17 12:00", true, now), Urgency::Done);
        assert_eq!(urgency_mark(Urgency::Overdue), "!!! TERLAMBAT");
        assert_eq!(urgency_mark(Urgency::Hours(6)), "!! <24 jam");
        assert_eq!(urgency_mark(Urgency::Days(30)), "");
    }

    #[test]
    fn prime_only_on_session_expiry_not_on_transient_errors() {
        let expired = Ok(crate::fetcher::JobResult { session_expired: true, ..Default::default() });
        assert!(needs_elena_prime(&expired));
        let failed = Ok(crate::fetcher::JobResult {
            ok: false,
            error: Some(crate::fetcher::JobError { code: "network".into(), message: "boom".into() }),
            ..Default::default()
        });
        assert!(!needs_elena_prime(&failed));
        let ok = Ok(crate::fetcher::JobResult { ok: true, ..Default::default() });
        assert!(!needs_elena_prime(&ok));
        let spawn_fail: Result<crate::fetcher::JobResult> = Err(anyhow::anyhow!("no node"));
        assert!(!needs_elena_prime(&spawn_fail));
    }

    #[test]
    fn pending_sorts_nearest_first_and_courses_count() {
        let now = chrono::Local::now().naive_local();
        let fmt = |d: chrono::NaiveDateTime| d.format("%Y-%m-%d %H:%M").to_string();
        let items = vec![
            titem("Kripto", "t-nodue", "", "Belum dikumpulkan"),
            titem("Kripto", "t-far", &fmt(now + chrono::Duration::days(9)), "Belum dikumpulkan"),
            titem("Jarkom", "t-near", &fmt(now + chrono::Duration::hours(5)), "Draft"),
            titem("Jarkom", "done", &fmt(now - chrono::Duration::days(8)), "Submitted"),
        ];
        let p = pending_sorted(&items);
        assert_eq!(p.len(), 3);
        // nearest deadline first across courses, unknown due dates last
        assert_eq!(p[0].nama, "t-near");
        assert_eq!(p[1].nama, "t-far");
        assert_eq!(p[2].nama, "t-nodue");
        let cs = courses(&items);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0], ("Jarkom".to_string(), 2, 1));
        assert_eq!(cs[1], ("Kripto".to_string(), 2, 2));
        assert_eq!(filter_course(&items, "krip").len(), 2);
        assert_eq!(filter_course(&items, "T-NEAR").len(), 1);
    }
}