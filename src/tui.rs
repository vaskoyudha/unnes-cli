//! Interactive dashboard (ratatui): session, kurikulum, jadwal, tugas and
//! changelog in one keyboard-driven screen. Read-only v1: refresh pulls the
//! same shared fetch+parse functions the CLI commands use.

use std::io::{self, IsTerminal, Stdout};
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

use crate::changelog;
use crate::config::Config;
use crate::data;
use crate::jadwal::{self, JadwalInfo, Sesi};
use crate::kurikulum::{self, Kursus};
use crate::paths::UnnesHome;
use crate::tugas::{self, TugasItem};

const PANELS: [&str; 5] = ["Dashboard", "Kurikulum", "Jadwal", "Tugas", "Changelog"];
const AUTO_REFRESH: Duration = Duration::from_secs(300);

/// Everything the dashboard shows; rebuilt on each refresh.
pub struct TuiState {
    pub profile: String,
    pub session_valid: bool,
    pub session_note: String,
    pub nim: String,
    pub identitas: Vec<(String, String)>,
    pub kursus: Vec<Kursus>,
    pub kurikulum_note: String,
    pub sesi: Vec<Sesi>,
    pub jadwal_info: JadwalInfo,
    pub jadwal_note: String,
    pub items: Vec<TugasItem>,
    pub tugas_note: String,
    pub log: Vec<changelog::ChangelogEntry>,
    pub pages: Vec<(String, String, usize)>,
    pub refresh_note: String,
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

impl TuiState {
    pub fn load(home: &UnnesHome, profile: &str) -> Self {
        let (session_valid, session_note) = probe_session(home, profile);
        let nim = kurikulum::resolve_nim(home).unwrap_or_default();
        let identitas = biodata_rows(home);

        let (kursus, kurikulum_note) = match kurikulum::fetch_plain(home, profile, &nim) {
            Ok(k) => (k, String::new()),
            Err(e) => (Vec::new(), format!("{e:#}")),
        };
        let (sesi, jadwal_info, jadwal_note) = match jadwal::fetch_plain(home, profile, &nim) {
            Ok((s, i)) => (s, i, String::new()),
            Err(e) => (Vec::new(), JadwalInfo::default(), format!("{e:#}")),
        };
        let (items, tugas_note) = match tugas::fetch_items(home, profile) {
            Ok(it) => (it, String::new()),
            Err(e) => (Vec::new(), format!("{e:#}")),
        };
        let log = changelog::read(home, None, None).unwrap_or_default();
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
        Self {
            profile: profile.to_string(),
            session_valid, session_note, nim, identitas, kursus, kurikulum_note,
            sesi, jadwal_info, jadwal_note, items, tugas_note, log, pages,
            refresh_note: format!("terakhir: {}", chrono::Local::now().format("%H:%M:%S")),
        }
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
        2 => draw_jadwal(state, frame, body),
        3 => draw_tugas(state, frame, body),
        _ => draw_changelog(state, frame, body),
    }

    // footer
    let foot = format!("[1-5/Tab] panel   [r] refresh   [q/Esc] keluar   {}", state.refresh_note);
    let err = [&state.kurikulum_note, &state.jadwal_note, &state.tugas_note].iter().find(|s| !s.is_empty()).map(|s| s.as_str()).unwrap_or("");
    let line = if err.is_empty() { foot } else { format!("{}   + {}", foot, err) };
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
        frame.render_widget(Paragraph::new(if state.kurikulum_note.is_empty() { "belum ada data" } else { state.kurikulum_note.as_str() }).block(block("Kurikulum")), area);
        return;
    }
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
        frame.render_widget(Paragraph::new(if state.jadwal_note.is_empty() { "belum ada jadwal" } else { state.jadwal_note.as_str() }).block(block("Jadwal")), area);
        return;
    }
    let today = hari_sekarang();
    let mut rows: Vec<Row> = Vec::new();
    for s in &state.sesi {
        let is_today = s.hari == today;
        rows.push(Row::new(vec![
            Cell::from(s.hari.as_str()),
            Cell::from(format!("{} - {}", s.mulai, s.selesai)),
            Cell::from(s.mata_kuliah.as_str()),
            Cell::from(s.ruang.as_str()),
            Cell::from(format!("{} SKS {}", s.sks, s.tipe)),
        ]).style(Style::default().fg(if is_today { Color::Yellow } else { Color::Reset })));
    }
    let widths = [Constraint::Length(8), Constraint::Length(14), Constraint::Min(20), Constraint::Length(20), Constraint::Length(14)];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["hari", "waktu", "mata kuliah", "ruang", "sesi"]).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(block("Jadwal Kuliah")),
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
        frame.render_widget(Paragraph::new(if state.tugas_note.is_empty() { "Belum ada tugas/kuis di Elena" } else { state.tugas_note.as_str() }).block(block("Tugas")), area);
        return;
    }
    let rows: Vec<Row> = state.items.iter().map(|it| {
        let color = match it.flag() { "OK" => Color::Green, "BELUM" => Color::Yellow, _ => Color::White };
        Row::new(vec![
            Cell::from(it.flag()),
            Cell::from(it.kategori.as_str()),
            Cell::from(it.course.as_str()),
            Cell::from(it.nama.as_str()),
            Cell::from(it.due.as_str()),
            Cell::from(it.status.as_str()),
        ]).style(Style::default().fg(color))
    }).collect();
    let widths = [Constraint::Length(6), Constraint::Length(6), Constraint::Length(10), Constraint::Min(20), Constraint::Length(30), Constraint::Min(12)];
    frame.render_widget(
        Table::new(rows, widths)
            .header(Row::new(vec!["flag", "tipe", "course", "nama", "due", "status"]).style(Style::default().add_modifier(Modifier::BOLD)))
            .block(block("Tugas & Kuis Elena")),
        area,
    );
}

fn draw_changelog(state: &TuiState, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = state.log.iter().take(30).map(|e| {
        ListItem::new(Line::from(format!("{}  {:<14} {}", &e.at[..11.min(e.at.len())], e.page_id, e.summary())))
    }).collect();
    frame.render_widget(List::new(items).block(block("Changelog")), area);
}// ---------------------------------------------------------------------------
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

    terminal.draw(|f| {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled(" unnes tui ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD))),
                Line::from(""),
                Line::from("memuat data dari portal ... (tekan r untuk refresh, q untuk keluar)"),
            ]),
            f.area(),
        );
    })?;
    let mut state = TuiState::load(home, profile);
    let mut selected = 0usize;
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|f| draw(&state, selected, f))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('r') => {
                            state = TuiState::load(home, profile);
                            last_refresh = Instant::now();
                        }
                        KeyCode::Tab => selected = (selected + 1) % PANELS.len(),
                        KeyCode::BackTab => selected = (selected + PANELS.len() - 1) % PANELS.len(),
                        KeyCode::Char(c) if ('1'..='5').contains(&c) => {
                            selected = (c as usize) - ('1' as usize);
                        }
                        _ => {}
                    }
                }
            }
        }
        if last_refresh.elapsed() >= AUTO_REFRESH {
            state = TuiState::load(home, profile);
            last_refresh = Instant::now();
        }
    }
    Ok(())
}