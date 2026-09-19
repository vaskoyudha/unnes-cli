//! Elena course materials (materi): files/resources the lecturers upload
//! (mod/resource, mod/folder, mod/url, mod/page, mod/book). Shared by the
//! CLI commands and the TUI dashboard.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::fetcher;
use crate::paths::UnnesHome;
use crate::tugas;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MateriItem {
    pub course: String,
    pub course_id: u32,
    pub nama: String,
    pub url: String,
    /// Moodle module: resource | folder | url | page | book
    pub kind: String,
}

/// Module kinds that are downloadable/readable materials (not assignments,
/// quizzes, forums or other interactive activities).
pub fn is_materi_kind(kind: &str) -> bool {
    matches!(kind, "resource" | "folder" | "url" | "page" | "book" | "imscp")
}

fn kind_of(url: &str) -> String {
    url.split("/mod/")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or("")
        .to_string()
}

/// Strip Moodle's screen-reader type suffix from activity names
/// ("RPS File" -> "RPS"): the <a> text includes the .accesshide span.
pub fn clean_nama(nama: &str) -> String {
    let t = nama.trim();
    for suffix in [" File", " Folder", " URL", " Page", " Book"] {
        if let Some(stripped) = t.strip_suffix(suffix) {
            if !stripped.trim().is_empty() {
                return stripped.trim().to_string();
            }
        }
    }
    t.to_string()
}

/// One (url, extract-spec) pair per course: the same URL and extract spec
/// as the old per-course loop, in kursus order so batch results map back
/// by position. Pure (no I/O) so the wiring is unit-testable.
fn course_urls(kursus: &[u32]) -> Vec<(String, Value)> {
    kursus
        .iter()
        .map(|cid| {
            (
                format!("https://elena.unnes.ac.id/course/view.php?id={cid}"),
                json!({
                    "selector": "a[href*='/mod/']",
                    // NOTE: "nama" must be "" (the record element's own
                    // text): the record IS the <a>, so a descendant
                    // selector like "a" matches nothing and every name
                    // comes back empty.
                    "fields": { "nama": "", "url": "@href" },
                }),
            )
        })
        .collect()
}

/// Collect every material link across every stored course: one batchget op
/// (one node spawn, one jar) instead of one spawn per course page.
pub fn fetch_materi(home: &UnnesHome, profile: &str, interactive: bool) -> Result<Vec<MateriItem>> {
    let kursus = tugas::course_ids(home);
    if kursus.is_empty() {
        bail!("no courses stored yet - run: unnes watch run (elena-kursus) or unnes discover --elena");
    }
    // NOTE: no upfront gateway ensure_session here on purpose. Elena owns an
    // independent session (often alive long after the gateway lapses); probing
    // the gateway first would pop a headed re-login window for a session this
    // fetch does not even need. Each course primes lazily below, like tugas.
    let mut items: Vec<MateriItem> = Vec::new();
    let mut seen: std::collections::HashSet<String> = Default::default();
    let mut ok_courses = 0u32;
    let pairs = course_urls(&kursus);
    let mut batch = fetcher::run_batchget(home, profile, &pairs);
    if batch.as_ref().map(|r| r.session_expired).unwrap_or(false) {
        // elena session dead: gateway refresh + browser handshake once
        tugas::prime_elena(home, profile, interactive);
        batch = fetcher::run_batchget(home, profile, &pairs);
    }
    let results = match batch {
        Ok(r) => r.results,
        Err(e) => {
            if interactive {
                eprintln!("peringatan: materi gagal diambil: {e:#}");
            }
            Vec::new()
        }
    };
    for (entry, cid) in results.iter().zip(kursus.iter()) {
        if !(entry.ok && !entry.session_expired) {
            // One flaky course must not kill the whole list (and must
            // never trigger a browser prime - only session expiry does).
            if interactive {
                let msg = entry.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
                eprintln!("peringatan: materi course-{cid} gagal diambil, dilewati{msg}", msg = if msg.is_empty() { String::new() } else { format!(": {msg}") });
            }
            continue;
        }
        ok_courses += 1;
        for rec in &entry.records {
            let url = rec.get("url").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let kind = kind_of(&url);
            if !is_materi_kind(&kind) || !seen.insert(url.clone()) {
                continue;
            }
            let nama = rec.get("nama").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let nama = clean_nama(&nama);
            if nama.is_empty() {
                continue;
            }
            items.push(MateriItem {
                course: format!("course-{cid}"),
                course_id: *cid,
                nama,
                url,
                kind,
            });
        }
    }
    if ok_courses == 0 {
        bail!("elena session unavailable; run: unnes login");
    }
    let names = tugas::course_names(home);
    for it in items.iter_mut() {
        if let Some(nm) = names.get(&it.course_id) {
            it.course = nm.clone();
        }
    }
    items.sort_by(|a, b| a.course.cmp(&b.course).then_with(|| a.nama.cmp(&b.nama)));
    Ok(items)
}

/// Case-insensitive substring filter on course name (falls back to item name).
pub fn filter_course<'a>(items: &'a [MateriItem], f: &str) -> Vec<&'a MateriItem> {
    let q = f.to_lowercase();
    items
        .iter()
        .filter(|it| it.course.to_lowercase().contains(&q) || it.nama.to_lowercase().contains(&q))
        .collect()
}

/// Distinct courses with material counts, sorted by name.
pub fn courses(items: &[MateriItem]) -> Vec<(String, usize)> {
    let mut map: std::collections::BTreeMap<String, usize> = Default::default();
    for it in items {
        *map.entry(it.course.clone()).or_insert(0) += 1;
    }
    map.into_iter().collect()
}

/// Download one material into `out_dir` (created when missing) via the
/// plain-HTTP fetcher path - no browser profile involved, so this never
/// contends with uploads/renders holding the profile lock.
pub fn download_materi(
    home: &UnnesHome,
    profile: &str,
    item: &MateriItem,
    out_dir: &std::path::Path,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(out_dir)?;
    let mut job = fetcher::job("download", profile);
    job["url"] = json!(item.url);
    job["out"] = json!(out_dir.to_string_lossy());
    let mut res = fetcher::run_job(home, profile, job.clone())?;
    if res.session_expired {
        // One self-heal like the fetch path: re-prime elena, retry once.
        tugas::prime_elena(home, profile, true);
        res = fetcher::run_job(home, profile, job)?;
    }
    if !res.ok {
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        let msg = res.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        if code == "session" || res.session_expired {
            bail!("session expired while downloading '{}'; run: unnes login", item.nama);
        }
        bail!("download '{}': {msg} ({code})", item.nama);
    }
    match res.path {
        Some(p) => Ok(std::path::PathBuf::from(p)),
        None => bail!("download '{}': fetcher returned no path", item.nama),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn course_urls_builds_one_entry_per_course() {
        let pairs = course_urls(&[11, 22]);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "https://elena.unnes.ac.id/course/view.php?id=11");
        assert_eq!(pairs[1].0, "https://elena.unnes.ac.id/course/view.php?id=22");
        assert_eq!(pairs[0].1["selector"], json!("a[href*='/mod/']"));
        assert_eq!(pairs[0].1["fields"]["nama"], json!(""));
        assert_eq!(pairs[0].1["fields"]["url"], json!("@href"));
    }

    #[test]
    fn kind_filtering() {        assert!(is_materi_kind("resource"));
        assert!(is_materi_kind("folder"));
        assert!(is_materi_kind("url"));
        assert!(is_materi_kind("page"));
        assert!(is_materi_kind("book"));
        assert!(!is_materi_kind("assign"));
        assert!(!is_materi_kind("quiz"));
        assert!(!is_materi_kind("forum"));
        assert_eq!(kind_of("https://elena.unnes.ac.id/mod/resource/view.php?id=42"), "resource");
        assert_eq!(kind_of("https://x/mod/folder/view.php?id=1"), "folder");
        assert_eq!(kind_of("https://x/other"), "");
    }

    #[test]
    fn nama_strips_accesshide_suffix() {
        assert_eq!(clean_nama("RPS File"), "RPS");
        assert_eq!(clean_nama("Materi 1 Folder"), "Materi 1");
        assert_eq!(clean_nama("Contoh"), "Contoh");
        assert_eq!(clean_nama("File"), "File"); // never strip to empty
    }
}
