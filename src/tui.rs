//! Interactive dashboard (ratatui): session, kurikulum, jadwal, tugas and
//! changelog in one keyboard-driven screen. Read-only v1: refresh pulls the
//! same shared fetch+parse functions the CLI commands use.

use std::io::{self, IsTerminal, Stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, List, ListItem, Paragraph, Row, Table, Tabs};
use ratatui::{Frame, Terminal};

use crate::cache;
use crate::changelog;
use crate::config::Config;
use crate::data;
use crate::jadwal::{self, JadwalInfo, Sesi};
use crate::kurikulum::{self, Kursus};
use crate::paths::UnnesHome;
use crate::peserta;
use crate::tugas::{self, TugasItem};

const PANELS: [&str; 5] = ["Dashboard", "Kurikulum", "Jadwal", "Tugas", "Changelog"];
const AUTO_REFRESH: Duration = Duration::from_secs(300);

// Local cache TTLs (seconds): static dashboard data is served from disk
// instead of re-fetching on every launch; 'r' always forces a fresh fetch.
const CACHE_TTL_KURIKULUM: u64 = 12 * 3600; // grades change rarely
const CACHE_TTL_JADWAL: u64 = 24 * 3600; // schedule is fixed per semester
const CACHE_TTL_TUGAS: u64 = 3600; // assignments are dynamic
const CACHE_TTL_PESERTA: u64 = 24 * 3600; // rosters are fixed per semester

/// Everything the dashboard shows. Published INCREMENTALLY by the load
/// thread (session -> kurikulum -> jadwal -> tugas -> watch), so panels
/// appear as soon as their source completes and the screen can never stay
/// stuck on the loading splash.
#[derive(Clone)]
pub struct TuiState {
    pub profile: String,
    pub session_valid: bool,
    pub session_note: String,
    pub nim: String,
    pub identitas: Vec<(String, String)>,
    pub kursus: Vec<Kursus>,
    pub kurikulum_note: String,
    pub kurikulum_loaded: bool,
    pub sesi: Vec<Sesi>,
    pub jadwal_info: JadwalInfo,
    pub jadwal_note: String,
    pub jadwal_loaded: bool,
    pub items: Vec<TugasItem>,
    pub tugas_note: String,
    pub tugas_loaded: bool,
    /// selected row in the Tugas panel (Enter opens the task URL)
    pub tugas_sel: usize,
    pub log: Vec<changelog::ChangelogEntry>,
    pub pages: Vec<(String, String, usize)>,
    pub refresh_note: String,
    // class roster overlay (Jadwal tab -> Enter)
    pub peserta_open: bool,
    pub peserta_course: String,
    pub peserta_cid: u32,
    pub peserta: Vec<peserta::Peserta>,
    pub peserta_loaded: bool,
    pub peserta_note: String,
    pub jadwal_sel: usize,
}

fn probe_session(home: &UnnesHome, profile: &str) -> (bool, String) {
    let mut job = crate::fetcher::job("get", profile);
    job["url"] = serde_json::json!("https://apps.unnes.ac.id/gate/list");
    match crate::fetcher::run_job(home, profile, job) {
        Ok(res) if res.ok && !res.session_expired => (true, "VALID".into()),
        Ok(_) => (false, "EXPIRED - run: unnes login".into()),
        Err(e) => (false, format!("probe failed: {e:#}")),
    }
}

fn biodata_rows(home: &UnnesHome) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Ok(Some(entry)) = data::latest(home, "biodata") {
        for r in &entry.records {
            let t = r.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if let Some((k, v)) = t.split_once(' ') {
                if ["NIM", "Nama", "Program", "Angkatan", "Dosen", "Email", "Jalur"].iter().any(|p| k.starts_with(p)) {
                    out.push((k.to_string(), v.to_string()));
                }
            }
        }
    }
    out
}

fn next_class_today(sesi: &[Sesi]) -> Option<&Sesi> {
    use chrono::{Datelike, Timelike};
    let now = chrono::Local::now();
    let hari = match now.weekday().number_from_monday() {
        1 => "Senin", 2 => "Selasa", 3 => "Rabu", 4 => "Kamis", 5 => "Jumat", 6 => "Sabtu", _ => return None,
    };
    let now_min = now.hour() as u32 * 60 + now.minute() as u32;
    sesi.iter()
        .filter(|s| s.hari == hari)
        .filter_map(|s| {
            let (h, m) = s.mulai.split_once(':')?;
            let t: u32 = match (h.parse::<u32>(), m.parse::<u32>()) {
                (Ok(hh), Ok(mm)) => hh * 60 + mm,
                _ => return None,
            };
            (t >= now_min).then_some((t, s))
        })
        .min_by_key(|(t, _)| *t)
        .map(|(_, s)| s)
}

/// Env-gated debug log: UNNES_TUI_DEBUG=1 writes the load timeline to
/// $UNNES_HOME/tui-debug.log so failures can be diagnosed on the user machine.
fn dbg(home: &UnnesHome, msg: &str) {
    if std::env::var("UNNES_TUI_DEBUG").map(|v| v == "1").unwrap_or(false) {
        let line = format!("{} {}
", chrono::Local::now().format("%H:%M:%S"), msg);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(home.root.join("tui-debug.log")) {
            use std::io::Write;
            let _ = f.write_all(line.as_bytes());
        }
    }
}

impl TuiState {
    /// Skeleton for a refresh that KEEPS the previous data: the panels
    /// keep showing the last good snapshot while re-fetching, so a session
    /// lapse can never blank the dashboard "suddenly" - old data stays
    /// visible and the footer reports the fetch progress/errors.
    pub fn refresh_skeleton(prev: Option<Self>, profile: &str) -> Self {
        let mut st = Self::skeleton(profile);
        if let Some(p) = prev {
            st.nim = p.nim;
            st.identitas = p.identitas;
            st.kursus = p.kursus;
            st.sesi = p.sesi;
            st.jadwal_info = p.jadwal_info;
            st.items = p.items;
            st.log = p.log;
            st.pages = p.pages;
        }
        st
    }

    /// Empty shell published on the very first tick: replaces the bare
    /// loading splash with the dashboard skeleton + a "memuat" footer.
    pub fn skeleton(profile: &str) -> Self {
        Self {
            profile: profile.to_string(),
            session_valid: false,
            session_note: "memuat...".into(),
            nim: String::new(),
            identitas: Vec::new(),
            kursus: Vec::new(),
            kurikulum_note: String::new(),
            kurikulum_loaded: false,
            sesi: Vec::new(),
            jadwal_info: JadwalInfo::default(),
            jadwal_note: String::new(),
            jadwal_loaded: false,
            items: Vec::new(),
            tugas_note: String::new(),
            tugas_loaded: false,
            tugas_sel: 0,
            log: Vec::new(),
            pages: Vec::new(),
            refresh_note: String::new(),
            peserta_open: false,
            peserta_course: String::new(),
            peserta_cid: 0,
            peserta: Vec::new(),
            peserta_loaded: false,
            peserta_note: String::new(),
            jadwal_sel: 0,
        }
    }

    /// Step 1: gateway session probe + stored identity (fast).
    pub fn probe(&mut self, home: &UnnesHome, profile: &str) {
        let (session_valid, session_note) = probe_session(home, profile);
        dbg(home, &format!("load: session valid={} note={}", session_valid, session_note));
        self.session_valid = session_valid;
        self.session_note = session_note;
        self.nim = kurikulum::resolve_nim(home).unwrap_or_default();
        self.identitas = biodata_rows(home);
    }

    /// Step 2: kurikulum. Cache-first: a fresh cached curriculum renders
    /// instantly with zero network; force (manual 'r') always refetches.
    pub fn load_kurikulum(&mut self, home: &UnnesHome, profile: &str, force: bool) {
        if !force {
            if let Some(k) = cache::load::<Vec<Kursus>>(home, "kurikulum", CACHE_TTL_KURIKULUM) {
                dbg(home, &format!("load: kurikulum from cache ({} mk)", k.len()));
                self.kursus = k;
                self.kurikulum_loaded = true;
                return;
            }
        }
        match kurikulum::fetch_and_parse(home, profile, &self.nim, false) {
            Ok(k) => {
                dbg(home, &format!("load: kurikulum ok {} mk", k.len()));
                let _ = cache::save(home, "kurikulum", &k);
                self.kursus = k;
            }
            Err(e) => {
                dbg(home, &format!("load: kurikulum FAIL: {e:#}"));
                // stale-but-present data beats a blank panel when the portal is down
                if let Some(k) = cache::load_any::<Vec<Kursus>>(home, "kurikulum") {
                    self.kursus = k;
                    self.kurikulum_note = format!("cache (gagal ambil baru): {e:#}");
                } else {
                    self.kurikulum_note = format!("{e:#}");
                }
            }
        }
        self.kurikulum_loaded = true;
    }

    /// Step 3: jadwal (same cache-first policy).
    pub fn load_jadwal(&mut self, home: &UnnesHome, profile: &str, force: bool) {
        if !force {
            if let Some((s, i)) = cache::load::<(Vec<Sesi>, JadwalInfo)>(home, "jadwal", CACHE_TTL_JADWAL) {
                dbg(home, &format!("load: jadwal from cache ({} sesi)", s.len()));
                self.sesi = s;
                self.jadwal_info = i;
                self.jadwal_loaded = true;
                return;
            }
        }
        match jadwal::fetch_and_parse(home, profile, &self.nim, false) {
            Ok((s, i)) => {
                dbg(home, &format!("load: jadwal ok {} sesi", s.len()));
                let _ = cache::save(home, "jadwal", &(s.clone(), i.clone()));
                self.sesi = s;
                self.jadwal_info = i;
            }
            Err(e) => {
                dbg(home, &format!("load: jadwal FAIL: {e:#}"));
                if let Some((s, i)) = cache::load_any::<(Vec<Sesi>, JadwalInfo)>(home, "jadwal") {
                    self.sesi = s;
                    self.jadwal_info = i;
                    self.jadwal_note = format!("cache (gagal ambil baru): {e:#}");
                } else {
                    self.jadwal_note = format!("{e:#}");
                }
            }
        }
        self.jadwal_loaded = true;
    }

    /// Step 4: Elena task/quiz items (same cache-first policy; assignments
    /// are dynamic so the TTL is short).
    pub fn load_tugas(&mut self, home: &UnnesHome, profile: &str, force: bool) {
        if !force {
            if let Some(it) = cache::load::<Vec<TugasItem>>(home, "tugas", CACHE_TTL_TUGAS) {
                dbg(home, &format!("load: tugas from cache ({} item)", it.len()));
                self.items = it;
                self.tugas_loaded = true;
                return;
            }
        }
        match tugas::fetch_items(home, profile, false) {
            Ok(it) => {
                dbg(home, &format!("load: tugas ok {} item", it.len()));
                let _ = cache::save(home, "tugas", &it);
                self.items = it;
            }
            Err(e) => {
                dbg(home, &format!("load: tugas FAIL: {e:#}"));
                if let Some(it) = cache::load_any::<Vec<TugasItem>>(home, "tugas") {
                    self.items = it;
                    self.tugas_note = format!("cache (gagal ambil baru): {e:#}");
                } else {
                    self.tugas_note = format!("{e:#}");
                }
            }
        }
        self.tugas_loaded = true;
    }

    /// Step 5: changelog + watch table + refresh stamp (local files only).
    /// Also re-probes the gateway: the load may have re-established the
    /// session mid-way (scripted re-login), so the header should show VALID
    /// instead of the expired state probed at load start.
    pub fn finish(&mut self, home: &UnnesHome, profile: &str) {
        let (session_valid, session_note) = probe_session(home, profile);
        dbg(home, &format!("load: re-probe session valid={} note={}", session_valid, session_note));
        self.session_valid = session_valid;
        self.session_note = session_note;
        self.log = changelog::read(home, None, None).unwrap_or_default();
        let mut pages = Vec::new();
        if let Ok(cfg) = Config::load(home) {
            for p in &cfg.pages {
                let (at, n) = match data::latest(home, &p.id) {
                    Ok(Some(e)) => (e.at.clone(), e.records.len()),
                    _ => (String::new(), 0),
                };
                pages.push((p.id.clone(), at, n));
            }
        }
        self.pages = pages;
        self.refresh_note = format!("terakhir: {}", chrono::Local::now().format("%H:%M:%S"));
        dbg(home, "load: selesai");
    }

    pub fn sks_lulus(&self) -> u32 { self.kursus.iter().filter(|k| k.kategori() == "LULUS").map(|k| k.sks).sum() }
    pub fn sks_total(&self) -> u32 { self.kursus.iter().map(|k| k.sks).sum() }
}// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn block<'a>(title: &'a str) -> Block<'a> {
    Block::default().borders(Borders::ALL).title(title)
}

fn day_color(k: &str) -> Color {
    match k {
        "LULUS" => Color::Green,
        "BERJALAN" => Color::Yellow,
        _ => Color::DarkGray,
    }
}

fn draw(state: &TuiState, selected: usize, frame: &mut Frame) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(2), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // header
    let header = Line::from(vec![
        Span::styled(" unnes ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(format!("  {}  ", state.profile)),
        Span::styled(if state.session_valid { "• session VALID" } else { "• session EXPIRED" },
            Style::default().fg(if state.session_valid { Color::Green } else { Color::Red })),
        Span::raw(format!("  NIM {}", state.nim)),
    ]);
    frame.render_widget(Paragraph::new(header).block(Block::default().borders(Borders::ALL)), chunks[0]);

    // tab bar
    let tabs = Tabs::new(PANELS.to_vec())
        .select(selected)
        .block(Block::default().borders(Borders::BOTTOM))
        .highlight_style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD));
    frame.render_widget(tabs, chunks[1]);

    let body = chunks[2];
    match selected {
        0 => draw_dashboard(state, frame, body),
        1 => draw_kurikulum(state, frame, body),
        2 if state.peserta_open => draw_peserta(state, frame, body),
        2 => draw_jadwal(state, frame, body),
        3 => draw_tugas(state, frame, body),
        _ => draw_changelog(state, frame, body),
    }

    // footer: loading progress while sources are still coming in, then
    // the first panel error (if any) once everything settled.
    let mut pending: Vec<&str> = Vec::new();
    if !state.kurikulum_loaded { pending.push("kurikulum"); }
    if !state.jadwal_loaded { pending.push("jadwal"); }
    if !state.tugas_loaded { pending.push("tugas"); }
    let foot = format!("[1-5/Tab] panel   [r] refresh   [q/Esc] keluar   {}", state.refresh_note);
    let line = if pending.is_empty() {
        let err = [&state.kurikulum_note, &state.jadwal_note, &state.tugas_note].iter().find(|s| !s.is_empty()).map(|s| s.as_str()).unwrap_or("");
        if err.is_empty() { foot } else { format!("{}   + {}", foot, err) }
    } else {
        let sesi = if state.session_valid { "sesi OK" } else { "sesi HANGUS - auto-login berjalan" };
        format!("menunggu: {} ...  ({})   [q] keluar", pending.join(", "), sesi)
    };
    frame.render_widget(Paragraph::new(line).style(Style::default().fg(Color::DarkGray)), chunks[3]);
}

fn draw_dashboard(state: &TuiState, frame: &mut Frame, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);
    let lines = rows[0];
    let right = rows[1];

    let mut info = vec![Line::from(Span::styled("IDENTITAS", Style::default().add_modifier(Modifier::BOLD))), Line::from("")];
    for (k, v) in &state.identitas {
        info.push(Line::from(format!("{k}: {v}")));
    }
    info.push(Line::from(""));
    if state.jadwal_info.semester > 0 {
        info.push(Line::from(format!("Semester ke-{}", state.jadwal_info.semester)));
        info.push(Line::from(format!("IPK: {}", state.jadwal_info.ipk)));
        info.push(Line::from(format!("SKS semester ini: {}", state.jadwal_info.sks_plan)));
    }
    // Tugas: overdue + due-within-48h summary (red when something is late)
    let now = chrono::Local::now().naive_local();
    let mut overdue: Vec<&crate::tugas::TugasItem> = Vec::new();
    let mut due_soon: Vec<&crate::tugas::TugasItem> = Vec::new();
    for it in &state.items {
        if it.status == "Submitted" || it.due.is_empty() {
            continue;
        }
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(&it.due, "%Y-%m-%d %H:%M") {
            let left = d - now;
            if left < chrono::Duration::zero() {
                overdue.push(it);
            } else if left <= chrono::Duration::hours(48) {
                due_soon.push(it);
            }
        }
    }
    if !overdue.is_empty() || !due_soon.is_empty() {
        info.push(Line::from(""));
        if let Some(first) = overdue.first() {
            info.push(Line::from(Span::styled(
                format!("TERLAMBAT ({}): {} - {}", overdue.len(), first.nama, first.course),
                Style::default().fg(Color::Red),
            )));
        }
        for it in due_soon.iter().take(2) {
            info.push(Line::from(Span::styled(
                format!("Due <=48 jam: {} ({})", it.nama, it.course),
                Style::default().fg(Color::Yellow),
            )));
        }
    }
    if let Some(next) = next_class_today(&state.sesi) {
        info.push(Line::from(""));
        info.push(Line::from(Span::styled(format!("Berikutnya hari ini: {}", next.mata_kuliah), Style::default().fg(Color::Yellow))));
        info.push(Line::from(format!("  {} - {} @ {}", next.mulai, next.selesai, next.ruang)));
    }
    frame.render_widget(Paragraph::new(info).block(block("Status")), lines);

    let right_lines = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Length(3), Constraint::Min(0)])
        .split(right);
    let total = state.sks_total().max(1);
    let pct = (state.sks_lulus() as f64 / total as f64 * 100.0) as u16;
    frame.render_widget(
        Gauge::default()
            .block(block("PROGRESS SKS")).gauge_style(Style::default().fg(Color::Green))
            .percent(pct)
            .label(format!("{} / {} SKS lulus", state.sks_lulus(), state.sks_total())),
        right_lines[0],
    );
    let lulus = state.kursus.iter().filter(|k| k.kategori() == "LULUS").count();
    let berjalan = state.kursus.iter().filter(|k| k.kategori() == "BERJALAN").count();
    let belum = state.kursus.len().saturating_sub(lulus + berjalan);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("Mata kuliah: {} lulus / {} berjalan / {} belum", lulus, berjalan, belum)),
            Line::from(format!("Tugas: {} item Elena", state.items.len())),
        ]).block(block("Ringkasan")),
        right_lines[1],
    );
    let rows: Vec<Row> = state.pages.iter().map(|(id, at, n)| {
        Row::new(vec![Cell::from(id.as_str()), Cell::from(n.to_string()), Cell::from(if at.is_empty() { "-" } else { &at[..11.min(at.len())] })])
    }).collect();
    let widths = [Constraint::Length(18), Constraint::Length(8), Constraint::Length(20)];
    frame.render_widget(
        Table::new(rows, widths).header(Row::new(vec!["page", "records", "last capture"])).block(block("Watch")),
        right_lines[2],
    );
}

fn draw_kurikulum(state: &TuiState, frame: &mut Frame, area: Rect) {
    if state.kursus.is_empty() {
        if !state.kurikulum_loaded {
            let (title, body) = if state.session_valid {
                ("Kurikulum - MEMUAT", "memuat kurikulum dari portal... (beberapa detik)")
            } else {
                ("Kurikulum - MENUNGGU SESI", "sesi gateway hangus - login ulang otomatis sedang berjalan (hingga ~2 mnt); atau q lalu: unnes login")
            };
            frame.render_widget(Paragraph::new(body).block(block(title)), area);
            return;
        }
        frame.render_widget(Paragraph::new(if state.kurikulum_note.is_empty() { "belum ada data - tekan r untuk memuat" } else { state.kurikulum_note.as_str() }).block(block("Kurikulum - GAGAL AMBIL")), area);
        return;
    }
    // stale-but-present data stays visible during a refresh
    let rows: Vec<Row> = state.kursus.iter().map(|k| {
        Row::new(vec![
            Cell::from(k.semester.to_string()),
            Cell::from(k.kode.as_str()),
            Cell::from(k.nama.as_str()),
            Cell::from(k.sks.to_string()),
            Cell::from(k.kategori()),
            Cell::from(k.nilai()),
        ]).style(Style::default().fg(day_color(k.kategori())))
    }).collect();
    let widths = [Constraint::Length(8), Constraint::Length(10), Constraint::Min(20), Constraint::Length(4), Constraint::Length(14), Constraint::Length(4)];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["smstr", "kode", "mata kuliah", "sks", "kategori", "nilai"]).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(block("Kurikulum")),
        area,
    );
}

fn draw_jadwal(state: &TuiState, frame: &mut Frame, area: Rect) {
    if state.sesi.is_empty() {
        if !state.jadwal_loaded {
            let (title, body) = if state.session_valid {
                ("Jadwal - MEMUAT", "memuat jadwal dari portal... (beberapa detik)")
            } else {
                ("Jadwal - MENUNGGU SESI", "sesi gateway hangus - login ulang otomatis sedang berjalan (hingga ~2 mnt); atau q lalu: unnes login")
            };
            frame.render_widget(Paragraph::new(body).block(block(title)), area);
            return;
        }
        frame.render_widget(Paragraph::new(if state.jadwal_note.is_empty() { "belum ada jadwal - tekan r untuk memuat" } else { state.jadwal_note.as_str() }).block(block("Jadwal - GAGAL AMBIL")), area);
        return;
    }
    // stale-but-present data stays visible during a refresh
    let today = hari_sekarang();
    let mut rows: Vec<Row> = Vec::new();
    for (idx, s) in state.sesi.iter().enumerate() {
        let is_today = s.hari == today;
        let is_sel = idx == state.jadwal_sel;
        let mut style = Style::default().fg(if is_today { Color::Yellow } else { Color::Reset });
        if is_sel {
            style = style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
        }
        rows.push(Row::new(vec![
            Cell::from(s.hari.as_str()),
            Cell::from(format!("{} - {}", s.mulai, s.selesai)),
            Cell::from(s.mata_kuliah.as_str()),
            Cell::from(s.ruang.as_str()),
            Cell::from(format!("{} SKS {}", s.sks, s.tipe)),
        ]).style(style));
    }
    let widths = [Constraint::Length(8), Constraint::Length(14), Constraint::Min(20), Constraint::Length(20), Constraint::Length(14)];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["hari", "waktu", "mata kuliah", "ruang", "sesi"]).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(block("Jadwal Kuliah - ↑↓ pilih · Enter: peserta kelas")),
        area,
    );
}

/// Class roster overlay: the student list of the selected jadwal row's
/// elena course (fetched lazily on Enter).
fn draw_peserta(state: &TuiState, frame: &mut Frame, area: Rect) {
    let title = if state.peserta_cid > 0 {
        format!("Peserta - {} (elena #{})", state.peserta_course, state.peserta_cid)
    } else {
        format!("Peserta - {}", state.peserta_course)
    };
    if !state.peserta_loaded {
        let body = if state.peserta_note.is_empty() {
            "memuat daftar peserta dari elena..."
        } else {
            state.peserta_note.as_str()
        };
        frame.render_widget(Paragraph::new(body).block(block(&title)), area);
        return;
    }
    if state.peserta.is_empty() {
        let msg = if state.peserta_note.is_empty() {
            "tidak ada peserta terdaftar".to_string()
        } else {
            state.peserta_note.clone()
        };
        frame.render_widget(Paragraph::new(msg).block(block(&format!("{title} - GAGAL"))), area);
        return;
    }
    let rows: Vec<Row> = state
        .peserta
        .iter()
        .enumerate()
        .map(|(i, p)| {
            Row::new(vec![
                Cell::from((i + 1).to_string()),
                Cell::from(p.nama.as_str()),
                Cell::from(p.nim.as_str()),
                Cell::from(p.peran.as_str()),
            ])
        })
        .collect();
    let widths = [Constraint::Length(6), Constraint::Min(30), Constraint::Length(16), Constraint::Length(12)];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["no", "nama", "nim", "peran"]).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(block(&format!("{title} · Esc: kembali"))),
        area,
    );
}

fn hari_sekarang() -> String {
    use chrono::Datelike;
    let w = chrono::Local::now().weekday().number_from_monday();
    ["", "Senin", "Selasa", "Rabu", "Kamis", "Jumat", "Sabtu"].get(w as usize).unwrap_or(&"").to_string()
}

fn draw_tugas(state: &TuiState, frame: &mut Frame, area: Rect) {
    if state.items.is_empty() {
        if !state.tugas_loaded {
            let (title, body) = if state.session_valid {
                ("Tugas - MEMUAT", "memuat tugas dari Elena... (beberapa detik)")
            } else {
                ("Tugas - MENUNGGU SESI", "menunggu sesi Elena pulih (login ulang otomatis)...")
            };
            frame.render_widget(Paragraph::new(body).block(block(title)), area);
            return;
        }
        frame.render_widget(Paragraph::new(if state.tugas_note.is_empty() { "Belum ada tugas/kuis di Elena" } else { state.tugas_note.as_str() }).block(block("Tugas")), area);
        return;
    }
    // stale-but-present items stay visible during a refresh
    let now = chrono::Local::now().naive_local();
    let rows: Vec<Row> = state.items.iter().enumerate().map(|(i, it)| {
        let base = match it.flag() { "OK" => Color::Green, "BELUM" => Color::Yellow, _ => Color::White };
        // overdue: due date passed and nothing submitted
        let overdue = it.status != "Submitted"
            && chrono::NaiveDateTime::parse_from_str(&it.due, "%Y-%m-%d %H:%M")
                .map(|d| d < now)
                .unwrap_or(false);
        let is_sel = i == state.tugas_sel;
        let mut style = Style::default().fg(if overdue { Color::Red } else { base });
        if is_sel {
            style = style.bg(Color::DarkGray).add_modifier(Modifier::BOLD);
        }
        Row::new(vec![
            Cell::from(it.flag()),
            Cell::from(it.kategori.as_str()),
            Cell::from(it.course.as_str()),
            Cell::from(it.nama.as_str()),
            Cell::from(if overdue { format!("{} (terlewat)", it.due) } else { it.due.clone() }),
            Cell::from(it.status.as_str()),
        ]).style(style)
    }).collect();
    let widths = [Constraint::Length(6), Constraint::Length(6), Constraint::Length(16), Constraint::Min(20), Constraint::Length(34), Constraint::Min(12)];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["flag", "tipe", "course", "nama", "due", "status"]).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(block("Tugas & Kuis Elena - ↑↓ pilih · Enter: buka di browser")),
        area,
    );
}

fn draw_changelog(state: &TuiState, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = state.log.iter().take(30).map(|e| {
        ListItem::new(Line::from(format!("{}  {:<14} {}", &e.at[..11.min(e.at.len())], e.page_id, e.summary())))
    }).collect();
    frame.render_widget(List::new(items).block(block("Changelog")), area);
}

/// Open a URL in the default browser (xdg-open on Linux, open on macOS,
/// start on Windows). Best-effort: a missing launcher is ignored.
fn open_in_browser(url: &str) {
    let launcher = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "cmd"
    } else {
        "xdg-open"
    };
    match std::process::Command::new(launcher).arg(url).spawn() {
        Ok(mut child) => { let _ = child.wait(); }
        Err(_) => eprintln!("tidak bisa membuka browser: tidak ada {launcher}"),
    }
}
// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run(home: &UnnesHome, profile: &str) -> Result<()> {
    // Interactive terminal required.
    if !io::stdout().is_terminal() {
        eprintln!("unnes tui membutuhkan terminal interaktif (bukan pipe/file)");
        std::process::exit(1);
    }
    // Session/data loading happens on a background thread so the screen and
    // the keyboard stay responsive even while the portal prime takes a while.
    // The state is published AFTER EVERY STEP (session -> kurikulum ->
    // jadwal -> tugas -> watch), so panels appear as soon as their source
    // finishes and a slow source (or a scripted re-login) never freezes the
    // whole dashboard on the loading splash. The state starts as the
    // skeleton (never None), so the splash screen is unreachable: the
    // dashboard is on screen from the very first tick.
    let state: Arc<Mutex<Option<TuiState>>> = Arc::new(Mutex::new(Some(TuiState::skeleton(profile))));
    fn start_load(home: &UnnesHome, profile: &str, target: &Arc<Mutex<Option<TuiState>>>, force: bool) {
        let h = home.clone();
        let p = profile.to_string();
        let t = Arc::clone(target);
        std::thread::spawn(move || {
            dbg(&h, if force { "load: mulai (force)" } else { "load: mulai" });
            let publish = |st: &TuiState| {
                if let Ok(mut g) = t.lock() {
                    *g = Some(st.clone());
                }
            };
            let mut st = TuiState::skeleton(&p);
            publish(&st);
            st.probe(&h, &p);
            publish(&st);
            st.load_kurikulum(&h, &p, force);
            publish(&st);
            st.load_jadwal(&h, &p, force);
            publish(&st);
            st.load_tugas(&h, &p, force);
            publish(&st);
            st.finish(&h, &p);
            publish(&st);
        });
    }

    /// Background fetch of one course's participant list; publishes into the
    /// live state so the overlay flips from "memuat" to the table.
    /// Cache-first: a roster seen within the last 24h renders instantly.
    fn start_peserta_load(home: &UnnesHome, profile: &str, cid: u32, target: &Arc<Mutex<Option<TuiState>>>) {
        let h = home.clone();
        let p = profile.to_string();
        let t = Arc::clone(target);
        std::thread::spawn(move || {
            let key = format!("peserta-{cid}");
            if let Some(list) = cache::load::<Vec<peserta::Peserta>>(&h, &key, CACHE_TTL_PESERTA) {
                if let Ok(mut g) = t.lock() {
                    if let Some(st) = g.as_mut() {
                        st.peserta = list;
                        st.peserta_loaded = true;
                    }
                }
                return;
            }
            match peserta::fetch_peserta(&h, &p, cid, false) {
                Ok(list) => {
                    let _ = cache::save(&h, &key, &list);
                    if let Ok(mut g) = t.lock() {
                        if let Some(st) = g.as_mut() {
                            st.peserta = list;
                            st.peserta_loaded = true;
                        }
                    }
                }
                Err(e) => {
                    if let Ok(mut g) = t.lock() {
                        if let Some(st) = g.as_mut() {
                            st.peserta_note = format!("{e:#}");
                            st.peserta_loaded = true;
                        }
                    }
                }
            }
        });
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let _guard = Guard;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let size = terminal.size()?;
    if size.width < 40 || size.height < 15 {
        eprintln!("terminal terlalu kecil (butuh >= 40x15)");
        return Ok(());
    }

    start_load(home, profile, &state, false);
    let mut selected = 0usize;
    let mut last_refresh = Instant::now();
    loop {
        {
            // state is Some from the first tick (skeleton) - the loading
            // splash is unreachable by construction.
            let guard = state.lock().unwrap();
            let st = guard.as_ref().expect("tui state is published before the loop");
            terminal.draw(|f| draw(st, selected, f))?;
        }
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let peserta_open = { state.lock().ok().and_then(|g| g.as_ref().map(|s| s.peserta_open)).unwrap_or(false) };
                    let tugas_items = { state.lock().ok().and_then(|g| g.as_ref().map(|s| s.items.len())).unwrap_or(0) };
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            if peserta_open {
                                // Esc closes the roster overlay first
                                if let Ok(mut g) = state.lock() {
                                    if let Some(st) = g.as_mut() {
                                        st.peserta_open = false;
                                    }
                                }
                            } else {
                                break;
                            }
                        }
                        KeyCode::Char('r') => {
                            // manual refresh: force a fresh fetch, bypass the cache
                            if let Ok(mut g) = state.lock() {
                                *g = Some(TuiState::refresh_skeleton(g.take(), profile));
                            }
                            start_load(home, profile, &state, true);
                            last_refresh = Instant::now();
                        }
                        KeyCode::Tab => selected = (selected + 1) % PANELS.len(),
                        KeyCode::BackTab => selected = (selected + PANELS.len() - 1) % PANELS.len(),
                        KeyCode::Up if selected == 2 && !peserta_open => {
                            if let Ok(mut g) = state.lock() {
                                if let Some(st) = g.as_mut() {
                                    st.jadwal_sel = st.jadwal_sel.saturating_sub(1);
                                }
                            }
                        }
                        KeyCode::Down if selected == 2 && !peserta_open => {
                            if let Ok(mut g) = state.lock() {
                                if let Some(st) = g.as_mut() {
                                    if st.jadwal_sel + 1 < st.sesi.len() {
                                        st.jadwal_sel += 1;
                                    }
                                }
                            }
                        }
                        KeyCode::Up if selected == 3 && tugas_items > 0 => {
                            if let Ok(mut g) = state.lock() {
                                if let Some(st) = g.as_mut() {
                                    st.tugas_sel = st.tugas_sel.saturating_sub(1);
                                }
                            }
                        }
                        KeyCode::Down if selected == 3 && tugas_items > 0 => {
                            if let Ok(mut g) = state.lock() {
                                if let Some(st) = g.as_mut() {
                                    if st.tugas_sel + 1 < st.items.len() {
                                        st.tugas_sel += 1;
                                    }
                                }
                            }
                        }
                        KeyCode::Enter if selected == 3 && tugas_items > 0 => {
                            // open the selected task's URL in the default browser
                            let url = {
                                let g = state.lock().unwrap();
                                let st = g.as_ref().expect("tui state");
                                st.items
                                    .get(st.tugas_sel.min(st.items.len() - 1))
                                    .map(|it| it.url.clone())
                            };
                            if let Some(url) = url {
                                open_in_browser(&url);
                            }
                        }
                        KeyCode::Enter if selected == 2 && !peserta_open => {
                            // open the class roster of the selected jadwal row
                            let cid = {
                                let mut g = state.lock().unwrap();
                                let st = g.as_mut().expect("tui state");
                                if !st.jadwal_loaded || st.sesi.is_empty() {
                                    None
                                } else {
                                    let sesi = &st.sesi[st.jadwal_sel.min(st.sesi.len() - 1)];
                                    match peserta::find_cid_for_course(home, &sesi.mata_kuliah) {
                                        Some(cid) => {
                                            st.peserta_open = true;
                                            st.peserta_course = sesi.mata_kuliah.clone();
                                            st.peserta_cid = cid;
                                            st.peserta.clear();
                                            st.peserta_loaded = false;
                                            st.peserta_note.clear();
                                            Some(cid)
                                        }
                                        None => {
                                            st.peserta_open = true;
                                            st.peserta_course = sesi.mata_kuliah.clone();
                                            st.peserta_note =
                                                "tidak ada kursus Elena yang cocok untuk mata kuliah ini".into();
                                            st.peserta_loaded = true;
                                            None
                                        }
                                    }
                                }
                            };
                            if let Some(cid) = cid {
                                start_peserta_load(home, profile, cid, &state);
                            }
                        }
                        KeyCode::Char(c) if ('1'..='5').contains(&c) => {
                            selected = (c as usize) - ('1' as usize);
                        }
                        _ => {}
                    }
                }
            }
        }
        if last_refresh.elapsed() >= AUTO_REFRESH {
            if let Ok(mut g) = state.lock() {
                *g = Some(TuiState::refresh_skeleton(g.take(), profile));
            }
            start_load(home, profile, &state, false);
            last_refresh = Instant::now();
        }
    }
    Ok(())
}
