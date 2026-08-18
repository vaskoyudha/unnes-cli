//! Curriculum view (Sikadu 2.4): all mata kuliah of the study program,
//! grouped by semester and categorized as LULUS / BERJALAN / BELUM DITEMPUH.
//!
//! Source: duanol.unnes.ac.id/v2/prakuliah/kurikulum/get_kurikulum_mhs/<nim>.aspx
//! (server-rendered; status column carries the letter grade for passed
//! courses, "Peringatan" for the running semester, "Belum ditempuh" later).

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Kursus {
    pub no: u32,
    pub semester: u32,
    pub kode: String,
    pub nama: String,
    pub jenis: String,
    pub sks: u32,
    /// "Wajib" / "Pilihan" ("" for Skripsi-style rows)
    pub wajib: String,
    pub status: String,
}

impl Kursus {
    /// LULUS (OK <grade>) / BERJALAN (Peringatan) / BELUM DITEMPUH.
    pub fn kategori(&self) -> &str {
        if self.status.starts_with("OK") {
            "LULUS"
        } else if self.status.starts_with("Peringatan") {
            "BERJALAN"
        } else {
            "BELUM DITEMPUH"
        }
    }

    /// Letter grade for passed courses ("A", "AB", ...) or "".
    pub fn nilai(&self) -> String {
        self.status
            .strip_prefix("OK")
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

/// Parse the page's flattened text into course records. Lines look like:
///   "1 1 20P00797 Algoritma dan Pemrograman T/P 3 Wajib OK A"
pub fn parse_curriculum(text: &str) -> Vec<Kursus> {
    let mut out = Vec::new();
    // The fetcher's normalized output keeps the HTML tags; flatten to plain
    // text first (also handles already-clean input).
    let tags = regex::Regex::new(r"<[^>]*>").unwrap();
    let ws = regex::Regex::new(r"\s+").unwrap();
    let clean = ws.replace_all(&tags.replace_all(text, " "), " ").into_owned();
    let re = regex::Regex::new(
        r"(\d+)\s+(\d+)\s+([A-Z0-9]+)\s+(.+?)\s+(T/P|T|P|L)\s+(\d+)\s+(Wajib|Pilihan)\s+(OK\s+\S+|Belum ditempuh|Peringatan\s+\S+)",
    )
    .unwrap();
    // Skripsi/TA style rows omit jenis and wajib: "62 8 20U00016 Skripsi 6 SKS belum"
    let re_simple = regex::Regex::new(
        r"(\d+)\s+(\d+)\s+([A-Z0-9]+)\s+(.+?)\s+(\d+)\s+SKS\s+(belum|OK\s+\S+|Peringatan\s+\S+)",
    )
    .unwrap();
    // Collect with both patterns, dedup by (no, kode) so Skripsi-style
    // rows don't double-match the main pattern.
    let mut seen = std::collections::HashSet::new();
    let mut push = |cap: regex::Captures<'_>| {
        let no: u32 = cap[1].parse().unwrap_or(0);
        let kode = cap[3].to_string();
        if !seen.insert((no, kode.clone())) {
            return;
        }
        let is_simple = cap.get(8).is_none();
        out.push(Kursus {
            no,
            semester: cap[2].parse().unwrap_or(0),
            kode,
            nama: cap[4].to_string().trim().to_string(),
            jenis: cap[5].to_string(),
            sks: cap[6].parse().unwrap_or(0),
            wajib: if is_simple { String::new() } else { cap[7].to_string() },
            status: cap[if is_simple { 7 } else { 8 }].to_string().trim().to_string(),
        });
    };
    for cap in re.captures_iter(&clean) {
        push(cap);
    }
    for cap in re_simple.captures_iter(&clean) {
        push(cap);
    }
    out.sort_by_key(|k| (k.semester, k.no));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lines_and_categorizes() {
        let text = concat!(
            "Kurikulum Program Studi #465040 Teknik Informatika, S1 Angkatan 2024 ",
            "No Smstr Kode MK Nama MK Jenis SKS Wajib Status ",
            "1 1 20P00797 Algoritma dan Pemrograman T/P 3 Wajib OK A ",
            "2 1 20U00001 Pendidikan Agama Islam T 2 Wajib Belum ditempuh ",
            "3 5 20P00813 Grafika Komputer T/P 3 Wajib Peringatan ? ",
        );
        let ks = parse_curriculum(text);
        assert_eq!(ks.len(), 3);
        assert_eq!(ks[0].kode, "20P00797");
        assert_eq!(ks[0].wajib, "Wajib");
        assert_eq!(ks[0].kategori(), "LULUS");
        assert_eq!(ks[0].nilai(), "A");
        assert_eq!(ks[1].kategori(), "BELUM DITEMPUH");
        assert_eq!(ks[2].kategori(), "BERJALAN");
        assert_eq!(ks[2].semester, 5);
    }
}
