//! Weekly class schedule (jadwal kuliah) derived from the Sikadu 2.4 KRS form.
//!
//! Each course row's "Jadwal Kuliah" cell contains one or more sessions like
//!   "Digital Center 2A - Rabu, pk. 13:00 WIB, 2 SKS Teori"
//! concatenated; each session is parsed and laid out in a weekly view.

use serde::Serialize;
use anyhow::{bail, Result};
use serde_json::json;

use crate::config::Config;
use crate::fetcher;
use crate::paths::UnnesHome;
use crate::watch;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sesi {
    pub mata_kuliah: String,
    pub kode: String,
    pub hari: String,
    pub mulai: String, // "13:00"
    pub selesai: String,
    pub ruang: String,
    pub sks: u32,
    pub tipe: String,
}

const HARI_URUT: [&str; 6] = ["Senin", "Selasa", "Rabu", "Kamis", "Jumat", "Sabtu"];

/// KRS header summary used by the dashboard: current semester, IPK, SKS plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct JadwalInfo {
    pub semester: u32,
    pub ipk: f64,
    pub sks_plan: u32,
}

/// Fetch the KRS form (plain HTTP; browser prime + retry on session miss)
/// and return the weekly sessions plus the header info.
pub fn fetch_and_parse(home: &UnnesHome, profile: &str, nim: &str) -> Result<(Vec<Sesi>, JadwalInfo)> {
    let url = format!("https://duanol.unnes.ac.id/v2/prakuliah/krs/form_isi_krs/{nim}.aspx");
    let fetch = |u: &str| -> Result<Option<(Vec<serde_json::Value>, String)>> {
        let mut job = fetcher::job("get", profile);
        job["url"] = json!(u);
        job["extract"] = json!({
            "selector": "table tbody tr",
            "fields": { "kode": "td:nth-child(3)", "nama": "td:nth-child(4)", "sks": "td:nth-child(5)", "jadwal": "td:nth-child(7)" },
        });
        let res = fetcher::run_job(home, profile, job)?;
        if !res.ok || res.session_expired {
            return Ok(None);
        }
        Ok(Some((res.records, res.normalized.unwrap_or_default())))
    };
    let mut data = fetch(&url)?;
    if data.is_none() {
        let _ = watch::ensure_session(home, profile);
        let cfg = Config::load(home)?;
        let page = cfg.pages.iter().find(|p| p.id == "sikadu-krs").cloned().unwrap_or_default();
        let _ = watch::fetch_page(home, profile, &page);
        data = fetch(&url)?;
        if data.is_none() {
            bail!("duanol session unavailable; run: unnes login");
        }
    }
    let (records, normalized) = data.unwrap();

    let mut sesi: Vec<Sesi> = Vec::new();
    for r in &records {
        let nama = r.get("nama").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let kode = r.get("kode").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let cell = r.get("jadwal").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if nama.is_empty() || cell.is_empty() {
            continue;
        }
        sesi.extend(parse_jadwal_cell(&cell, &nama, &kode));
    }
    sesi.sort_by(|a, b| (hari_urutan(&a.hari), &a.mulai, &a.mata_kuliah).cmp(&(hari_urutan(&b.hari), &b.mulai, &b.mata_kuliah)));

    // Header info: "Semester ke-5 3.58 24 SKS"
    let mut info = JadwalInfo::default();
    let tags = regex::Regex::new(r"<[^>]*>").unwrap();
    let flat = regex::Regex::new(r"\s+").unwrap().replace_all(&tags.replace_all(&normalized, " "), " ").to_string();
    if let Some(c) = regex::Regex::new(r"Semester ke-(\d+)[^0-9]+([0-9]+\.[0-9]+)[^0-9]+(\d+) SKS").unwrap().captures(&flat) {
        info.semester = c[1].parse().unwrap_or(0);
        info.ipk = c[2].parse().unwrap_or(0.0);
        info.sks_plan = c[3].parse().unwrap_or(0);
    }
    Ok((sesi, info))
}
/// Parse one course's jadwal cell into sessions.
/// Example cell: "D.1-502 - Selasa, pk. 13:00 WIB, 2 SKS TeoriD.1-502 - Selasa, pk. 15:00 WIB, 1 SKS Praktik"
pub fn parse_jadwal_cell(cell: &str, mata_kuliah: &str, kode: &str) -> Vec<Sesi> {
    // Segments are concatenated with no separator ("TeoriD.1-502 - ..."),
    // so walk them one at a time: find the next " - Day, pk. HH:MM WIB, N SKS "
    // from the current position, derive the type from the tail (a known type
    // keyword terminates the segment), and continue after it.
    let re = regex::Regex::new(
        r"(.+?) - (Senin|Selasa|Rabu|Kamis|Jumat|Sabtu), pk\. (\d{2}:\d{2}) WIB, (\d+) SKS ",
    )
    .unwrap();
    const TIPE_KW: [&str; 8] = ["Teori", "Praktik", "Praktikum", "Tutorial", "Responsi", "Seminar", "Lainnya", "Kerja"];
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(m) = re.find(&cell[pos..]) {
        let seg = &cell[pos..];
        let caps = re.captures(seg).unwrap();
        let sks: u32 = caps[4].parse().unwrap_or(0);
        let mulai = caps[3].to_string();
        let tail = &cell[pos + m.end()..];
        let found = TIPE_KW
            .iter()
            .filter_map(|k| tail.find(k).map(|p| (p, k.len())))
            .min_by_key(|(p, _)| *p);
        let tipe = match found {
            Some((p, len)) => tail[..p + len].trim().to_string(),
            None => tail.trim().to_string(),
        };
        // the next segment starts right after the type keyword
        let tipe_len = match found {
            Some((p, len)) => p + len,
            None => tail.len(),
        };
        let selesai = plus_minutes(&mulai, sks * 50);
        out.push(Sesi {
            mata_kuliah: mata_kuliah.to_string(),
            kode: kode.to_string(),
            hari: caps[2].to_string(),
            mulai,
            selesai,
            ruang: caps[1].trim().to_string(),
            sks,
            tipe,
        });
        pos += m.end() + tipe_len;
    }
    out
}

/// "HH:MM" + minutes -> "HH:MM" (wraps the hour, not the day)
fn plus_minutes(hhmm: &str, minutes: u32) -> String {
    let (h, m) = hhmm.split_once(':').unwrap_or(("00", "00"));
    let h: u32 = h.parse().unwrap_or(0);
    let m: u32 = m.parse().unwrap_or(0);
    let total = h * 60 + m + minutes;
    format!("{:02}:{:02}", (total / 60) % 24, total % 60)
}

/// Group a day's sessions and print (or sort) helpers used by the report.
pub fn hari_urutan(hari: &str) -> usize {
    HARI_URUT.iter().position(|h| *h == hari).unwrap_or(99)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_sessions_and_computes_end() {
        let cell = "D.1-502 - Selasa, pk. 13:00 WIB, 2 SKS TeoriD.1-502 - Selasa, pk. 15:00 WIB, 1 SKS Praktik";
        let s = parse_jadwal_cell(cell, "Teknik Multimedia", "20P00812");
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].ruang, "D.1-502");
        assert_eq!(s[0].mulai, "13:00");
        assert_eq!(s[0].selesai, "14:40"); // 2 SKS * 50m
        assert_eq!(s[0].tipe, "Teori");
        assert_eq!(s[1].mulai, "15:00");
        assert_eq!(s[1].selesai, "15:50");
        assert_eq!(s[1].tipe, "Praktik");
    }

    #[test]
    fn handles_dashed_rooms() {
        let s = parse_jadwal_cell("D.1-802 - Kamis, pk. 07:00 WIB, 3 SKS Teori", "K&M", "X");
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].ruang, "D.1-802");
        assert_eq!(s[0].selesai, "09:30");
    }
}
