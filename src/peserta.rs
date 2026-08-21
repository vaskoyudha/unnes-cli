//! Elena course participants: the student list of one course
//! (Moodle /user/index.php?id=<courseid>).
//!
//! A dead elena session is re-primed once through the browser handshake
//! (sso_app 30) exactly like tugas - never opening a click-waiting window.

use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::json;

use crate::data;
use crate::fetcher;
use crate::paths::UnnesHome;
use crate::watch;

#[derive(Debug, Clone, Serialize)]
pub struct Peserta {
    pub nama: String,
    pub nim: String,
    pub peran: String,
}

/// Normalized form used for fuzzy course-name matching
/// (lowercase alphanumerics only).
pub fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Longest common subsequence length (OCR/typo tolerant similarity).
fn lcs_len(a: &[char], b: &[char]) -> usize {
    let (m, n) = (a.len(), b.len());
    if m == 0 || n == 0 {
        return 0;
    }
    let mut prev = vec![0usize; n + 1];
    let mut cur = vec![0usize; n + 1];
    for i in 1..=m {
        for j in 1..=n {
            cur[j] = if a[i - 1] == b[j - 1] { prev[j - 1] + 1 } else { prev[j].max(cur[j - 1]) };
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

/// Dice-style similarity of two normalized strings (0.0..=1.0).
fn similarity(a: &str, b: &str) -> f64 {
    let (x, y): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if x.is_empty() || y.is_empty() {
        return 0.0;
    }
    let l = lcs_len(&x, &y);
    (2.0 * l as f64) / ((x.len() + y.len()) as f64)
}

/// Find the elena course id whose stored title best matches a jadwal
/// course name. The stored titles carry OCR artifacts ("Si tem Informa i"),
/// so this uses LCS-based fuzzy similarity on normalized text - best match
/// wins with a conservative threshold.
pub fn find_cid_for_course(home: &UnnesHome, course_name: &str) -> Option<u32> {
    let entry = data::latest(home, "elena-kursus").ok()??;
    let target = norm(course_name);
    let mut best: Option<(f64, u32)> = None;
    for rec in &entry.records {
        let src = rec.get("_source").and_then(|v| v.as_str()).unwrap_or("");
        let Some(id) = src.split("id=").nth(1).and_then(|x| x.split(['.', '&', '#']).next()) else { continue };
        let Ok(cid) = id.parse::<u32>() else { continue };
        let title = rec
            .get("_title")
            .or_else(|| rec.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let sim = similarity(&target, &norm(title));
        if sim >= 0.5 && best.map_or(true, |(bs, _)| sim > bs) {
            best = Some((sim, cid));
        }
    }
    best.map(|(_, cid)| cid)
}

fn participant_job(_home: &UnnesHome, profile: &str, cid: u32) -> serde_json::Value {
    let mut job = fetcher::job("get", profile);
    job["url"] = json!(format!("https://elena.unnes.ac.id/user/index.php?id={cid}"));
    job["extract"] = json!({
        "selector": "#participants tbody tr",
        "fields": {
            // span.userinitials carries "Full Name NIM" in its title attr
            "nama": "a.aabtn span.userinitials@title",
            "peran": "td.c2",
            "url": "a.aabtn@href",
        },
    });
    job
}

/// Fetch the participant list of one elena course.
pub fn fetch_peserta(home: &UnnesHome, profile: &str, cid: u32, interactive: bool) -> Result<Vec<Peserta>> {
    let mut res = fetcher::run_job(home, profile, participant_job(home, profile, cid));
    if res.as_ref().map(|r| r.session_expired || !r.ok).unwrap_or(true) {
        // elena session dead: gateway refresh + browser handshake, retry once
        watch::ensure_session(home, profile, interactive);
        let page = crate::config::Page {
            id: "elena-prime".into(),
            url: "https://elena.unnes.ac.id/my/".into(),
            render: Some(true),
            sso_app: Some("30".into()),
            ..Default::default()
        };
        let _ = watch::fetch_page(home, profile, &page);
        res = fetcher::run_job(home, profile, participant_job(home, profile, cid));
    }
    let r = res?;
    if r.session_expired || !r.ok {
        bail!("elena session unavailable; run: unnes login");
    }
    let mut out: Vec<Peserta> = Vec::new();
    for rec in &r.records {
        let raw = rec.get("nama").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        if raw.is_empty() {
            continue;
        }
        // "Alamsyah 132320168" -> nama + trailing NIM when the last token is numeric
        let (nama, nim) = match raw.rsplit_once(' ') {
            Some((n, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => {
                (n.trim().to_string(), tail.to_string())
            }
            _ => (raw.clone(), String::new()),
        };
        let peran = match rec.get("peran").and_then(|v| v.as_str()).unwrap_or("").trim() {
            "Teacher" => "Dosen".to_string(),
            "Student" => "Mahasiswa".to_string(),
            other => other.to_string(),
        };
        out.push(Peserta { nama, nim, peran });
    }
    out.sort_by(|a, b| b.peran.cmp(&a.peran).then(a.nama.to_lowercase().cmp(&b.nama.to_lowercase())));
    Ok(out)
}
