//! unnes-cli — UNNES student portal CLI.
//!
//! Milestones: M1 core (this), M2 node fetcher, M3 session, M4 watch engine, M5 release.

// Modules are consumed by later milestones (M2/M3/M4) plus their own tests;
// silence the interim dead-code noise that would otherwise spam every build.
#![allow(dead_code)]

mod changelog;
mod config;
mod data;
mod diff;
mod fetcher;
mod jadwal;
mod kurikulum;
mod output;
mod paths;
mod tugas;
mod tui;
mod watch;

use std::fs;
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde_json::json;

use crate::config::Config;
use crate::fetcher::{JobError, JobResult};
use crate::paths::UnnesHome;

/// CLI entry.
#[derive(Parser)]
#[command(
    name = "unnes",
    version,
    about = "UNNES student portal CLI: session, grades/schedule, change watching"
)]
struct Cli {
    /// Machine-readable JSON output where supported
    #[arg(long, global = true)]
    json: bool,

    /// Suppress human progress output (errors still print)
    #[arg(long, global = true)]
    quiet: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Save an authenticated session (Google SSO via a browser window)
    Login(LoginArgs),
    /// Forget the saved session.
    Logout,
    /// Session state, last sync, next poll.
    Status,
    /// Fetch one configured page and print records.
    Fetch(FetchArgs),
    /// Fetch the grades page (alias for fetch grades).
    Grades(PageAliasArgs),
    /// Fetch the schedule page (alias for fetch schedule).
    Schedule(ScheduleArgs),
    /// Fetch the announcements page (alias for fetch announcements).
    Announcements(PageAliasArgs),
    /// Watch commands: add/rm/list/run/daemon.
    Watch(WatchArgs),
    /// Discover gateway apps or elena courses and print watch recipes
    Discover(DiscoverArgs),
    /// Stored data: list/show/history/export captured page states
    Data(DataArgs),
    /// Kurikulum: all mata kuliah by semester (LULUS/BERJALAN/BELUM DITEMPUH)
    Kurikulum,
    /// Jadwal kuliah: weekly class schedule (Senin..Sabtu)
    Jadwal,
    /// Tugas: Elena assignments/quizzes with deadlines and submission status
    Tugas,
    /// TUI: interactive dashboard (ratatui)
    Tui,
    /// Print the change log.
    Changelog(ChangelogArgs),
}

#[derive(Args)]
struct LoginArgs {
    /// Account email (kept for future form-login mode; SSO ignores it)
    #[arg(long)]
    email: Option<String>,
}

#[derive(Args)]
struct FetchArgs {
    /// Page id from config.toml
    page_id: String,
    /// Emit CSV instead of a table (overrides --json)
    #[arg(long)]
    csv: bool,
}

#[derive(Args)]
struct PageAliasArgs {
    /// Emit CSV instead of a table (overrides --json)
    #[arg(long)]
    csv: bool,
}

#[derive(Args)]
struct ScheduleArgs {
    /// Week number (0 = current week)
    #[arg(long)]
    week: Option<u32>,
    /// Emit CSV instead of a table (overrides --json)
    #[arg(long)]
    csv: bool,
}

#[derive(Args)]
struct WatchArgs {
    #[command(subcommand)]
    cmd: WatchCmd,
}

#[derive(Subcommand)]
enum WatchCmd {
    /// Register a page to watch (persists to config.toml)
    Add {
        /// Unique page id (grades/schedule/announcements are conventional)
        id: String,
        /// Absolute URL of the page
        #[arg(long)]
        url: String,
        /// CSS selector producing one element per record
        #[arg(long)]
        selector: Option<String>,
        /// Poll interval in seconds (default: general.default_interval)
        #[arg(long)]
        interval: Option<u64>,
        /// Field used as the record key when diffing
        #[arg(long)]
        key_field: Option<String>,
        /// Render in the persistent browser session (Livewire / iframe-SSO)
        #[arg(long)]
        render: bool,
        /// Gateway app id to prime the session (76 akademik, 30 elena, 64 student)
        #[arg(long)]
        sso_app: Option<String>,
        /// URL visited before the target (e.g. semester switcher)
        #[arg(long)]
        pre_url: Option<String>,
        /// Crawl mode: follow these links from the page
        #[arg(long)]
        link_selector: Option<String>,
        /// Elena semester to open after SSO (default 20261)
        #[arg(long)]
        sso_semester: Option<String>,
    },
    /// Remove a watched page
    Rm { id: String },
    /// List configured pages
    List,
    /// Run one watch pass for all (or one) pages; cron-friendly
    Run {
        /// Only this page id
        #[arg(long)]
        page_id: Option<String>,
    },
    /// Adaptive polling daemon
    Daemon,
}

#[derive(Args)]
struct DataArgs {
    #[command(subcommand)]
    cmd: DataCmd,
}

#[derive(Subcommand)]
enum DataCmd {
    /// List stored datasets (one per configured page) with capture counts
    List,
    /// Show the latest stored records of a page (table/--csv/--json)
    Show {
        /// Page id from config.toml
        page_id: String,
        /// Emit CSV instead of a table (overrides --json)
        #[arg(long)]
        csv: bool,
    },
    /// Show the capture timeline (distinct states stored so far)
    History { page_id: String },
    /// Export the full history (--csv: latest state as CSV lines)
    Export {
        /// Page id from config.toml
        page_id: String,
        /// Emit CSV for the latest state (default: full history JSON)
        #[arg(long)]
        csv: bool,
    },
}

#[derive(Args)]
struct DiscoverArgs {
    /// List elena courses (requires a session) instead of gateway apps
    #[arg(long)]
    elena: bool,
    /// Elena semester to open after SSO (default 20261)
    #[arg(long)]
    semester: Option<String>,
}

#[derive(Args)]
struct ChangelogArgs {
    /// Only entries at or after this RFC3339 timestamp
    #[arg(long)]
    since: Option<String>,
    /// Only entries for this page id
    #[arg(long)]
    page_id: Option<String>,
}

/// Error carrying the CLI exit code (spec: 0 ok, 1 generic, 2 usage,
/// 3 not logged in, 4 session expired, 5 network/429, 6 selector outdated).
#[derive(Debug)]
struct AppError {
    code: u8,
    msg: String,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for AppError {}

fn app_err(code: u8, msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(AppError { code, msg: msg.into() })
}

fn not_yet(what: &str, milestone: &str) -> anyhow::Error {
    app_err(1, format!("{what} is implemented in milestone {milestone}; check the README for the roadmap"))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            let code = e.downcast_ref::<AppError>().map(|a| a.code).unwrap_or(1);
            ExitCode::from(code)
        }
    }
}
fn run(cli: Cli) -> Result<()> {
    let home = UnnesHome::discover();
    home.ensure_dirs()?;
    let profile = fetcher::profile_name();
    match cli.cmd {
        Cmd::Login(_) => cmd_login(&home, &profile),
        Cmd::Logout => cmd_logout(&home, &profile),
        Cmd::Status => cmd_status(&home, &profile, cli.json),
        Cmd::Fetch(a) => cmd_fetch(&home, &profile, &a.page_id, a.csv, cli.json),
        Cmd::Grades(a) => cmd_fetch(&home, &profile, "grades", a.csv, cli.json),
        Cmd::Schedule(a) => cmd_fetch(&home, &profile, "schedule", a.csv, cli.json),
        Cmd::Announcements(a) => cmd_fetch(&home, &profile, "announcements", a.csv, cli.json),
        Cmd::Watch(w) => match w.cmd {
            WatchCmd::List => watch_list(&home, cli.json),
            WatchCmd::Add { id, url, selector, interval, key_field, render, sso_app, pre_url, link_selector, sso_semester } => {
                watch::add_page(&home, &id, &url, selector, interval, key_field, render, sso_app, pre_url, link_selector, sso_semester)?;
                println!("page '{id}' added; run: unnes watch run --page_id {id}");
                Ok(())
            }
            WatchCmd::Rm { id } => {
                watch::rm_page(&home, &id)?;
                println!("page '{id}' removed");
                Ok(())
            }
            WatchCmd::Run { page_id } => cmd_watch_run(&home, &profile, page_id.as_deref(), cli.json),
            WatchCmd::Daemon => watch::daemon(&home, &profile),
        },
        Cmd::Discover(a) => cmd_discover(&home, &profile, &a, cli.json),
        Cmd::Data(d) => cmd_data(&home, &profile, &d.cmd, cli.json),
        Cmd::Kurikulum => cmd_kurikulum(&home, &profile, cli.json),
        Cmd::Jadwal => cmd_jadwal(&home, &profile, cli.json),
        Cmd::Tugas => cmd_tugas(&home, &profile, cli.json),
        Cmd::Tui => tui::run(&home, &profile),
        Cmd::Changelog(a) => changelog_list(&home, &a, cli.json),
    }
}

/// Map a fetcher error code to the CLI exit code spec.
fn err_code_for(code: &str) -> u8 {
    match code {
        "usage" | "contract" => 2,
        "network" | "timeout" | "challenge" => 5,
        _ => 1,
    }
}

fn err_msg(res: &JobResult) -> String {
    match &res.error {
        Some(JobError { code, message }) => format!("{message} ({code})"),
        None => "unknown fetcher error".to_string(),
    }
}

/// unnes login: Google SSO via a headed browser window.
fn cmd_login(home: &UnnesHome, profile: &str) -> Result<()> {
    let cfg = Config::load(home)?;
    let mut job = fetcher::job("login", profile);
    job["mode"] = json!("browser");
    job["baseUrl"] = json!(cfg.general.base_url);
    let res = fetcher::run_job(home, profile, job)?;
    if !res.ok {
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        return Err(app_err(err_code_for(&code), format!("login failed: {}", err_msg(&res))));
    }
    // Persist login metadata for unnes status.
    let meta = json!({
        "profile": profile,
        "landing_url": res.landing_url,
        "logged_in_at": chrono::Utc::now().to_rfc3339(),
    });
    fs::write(home.profile_meta_file(profile), serde_json::to_string_pretty(&meta)?)?;
    println!("logged in (profile {profile}), {} cookies captured", res.captured_cookies.unwrap_or(0));
    if let Some(landing) = &res.landing_url {
        println!("SSO landing page: {landing}");
        println!("point a watch at it: unnes watch add <id> --url={landing} --selector=<css>");
    }
    Ok(())
}

/// unnes logout: drop the saved session.
fn cmd_logout(home: &UnnesHome, profile: &str) -> Result<()> {
    let job = fetcher::job("logout", profile);
    let res = fetcher::run_job(home, profile, job)?;
    if !res.ok {
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        return Err(app_err(err_code_for(&code), format!("logout failed: {}", err_msg(&res))));
    }
    let _ = fs::remove_file(home.profile_meta_file(profile));
    println!("session cleared (profile {profile})");
    Ok(())
}

/// unnes status: session state from the saved jar + login metadata.
fn cmd_status(home: &UnnesHome, profile: &str, json_out: bool) -> Result<()> {
    let cfg = Config::load(home)?;
    let jar = home.profile_jar_file(profile);
    if !jar.is_file() {
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json!({ "profile": profile, "logged_in": false }))?);
        }
        return Err(app_err(3, format!("not logged in (profile {profile}); run: unnes login")));
    }
    let modified = jar.metadata()?.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let age_secs = SystemTime::now().duration_since(modified).unwrap_or_default().as_secs();

    // Live session check: the gateway answers the app list only with a valid
    // server-side session; jar-file existence alone is not proof.
    let mut valid = false;
    let mut probe_err = String::new();
    {
        let mut job = fetcher::job("get", profile);
        job["url"] = json!("https://apps.unnes.ac.id/gate/list");
        match fetcher::run_job(home, profile, job) {
            Ok(res) => {
                valid = res.ok && !res.session_expired;
                if res.session_expired {
                    probe_err = "gateway session ended".into();
                }
            }
            Err(e) => probe_err = format!("{e:#}"),
        }
    }
    let meta_path = home.profile_meta_file(profile);
    let landing: Option<String> = if meta_path.is_file() {
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&meta_path)?)
            .ok()
            .and_then(|v| v.get("landing_url").and_then(|l| l.as_str().map(String::from)))
    } else {
        None
    };
    // Auto re-login for status: try the scripted login before declaring EXPIRED.
    if !valid && cfg.general.auto_relogin {
        probe_err = match watch::auto_login(home, profile, true) {
            Ok(how) => format!("re-login ok ({how})"),
            Err(e) => format!("auto re-login failed: {e:#}"),
        };
        let mut job = fetcher::job("get", profile);
        job["url"] = json!("https://apps.unnes.ac.id/gate/list");
        if let Ok(res) = fetcher::run_job(home, profile, job) {
            valid = res.ok && !res.session_expired;
            if valid {
                probe_err = "re-login ok".into();
            }
        }
    }
    if !valid {
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json!({
                "profile": profile,
                "logged_in": false,
                "jar_age_seconds": age_secs,
                "reason": probe_err,
            }))?);
        } else {
            println!("profile: {profile}");
            println!("session: EXPIRED ({probe_err}); run: unnes login");
        }
        return Err(app_err(4, "session expired; run: unnes login"));
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({
            "profile": profile,
            "logged_in": true,
            "jar_age_seconds": age_secs,
            "landing_url": landing,
        }))?);
    } else {
        println!("profile: {profile}");
        println!("session: VALID (gateway confirmed)");
        match landing {
            Some(l) => println!("SSO landing: {l}"),
            None => println!("SSO landing: unknown (re-run unnes login)"),
        }
    }
    Ok(())
}

/// unnes fetch / grades / schedule / announcements.
fn cmd_fetch(home: &UnnesHome, profile: &str, page_id: &str, csv: bool, json_out: bool) -> Result<()> {
    let cfg = Config::load(home)?;
    let page = cfg.page(page_id).ok_or_else(|| {
        app_err(1, format!("page '{page_id}' is not configured ({}); add it with: unnes watch add {page_id} --url=<page-url> --selector=<css>", home.config_file().display()))
    })?;
    if !home.profile_jar_file(profile).is_file() {
        return Err(app_err(3, format!("not logged in (profile {profile}); run: unnes login")));
    }

    // render=true pages (Livewire / iframe-SSO apps) go through the
    // persistent browser session; everything else is plain HTTP with an
    // automatic sso_token exchange on session expiry. link_selector pages
    // become crawls (follow links, extract rows on each linked page).
    // Shared dispatch lives in watch::fetch_page (get/page/crawl + sso).
    let mut res = watch::fetch_page(home, profile, page)?;
    // Auto re-login: session expired -> scripted Google re-login (saved
    // profile) -> one retry, when enabled.
    if res.session_expired && cfg.general.auto_relogin {
        if watch::auto_login(home, profile, true).is_ok() {
            res = watch::fetch_page(home, profile, page)?;
        }
    }
    if !res.ok {
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        if code == "session" {
            return Err(app_err(4, format!("session expired while fetching '{page_id}'; run: unnes login")));
        }
        return Err(app_err(err_code_for(&code), format!("fetch {page_id}: {}", err_msg(&res))));
    }
    if res.session_expired {
        return Err(app_err(4, format!("session expired while fetching '{page_id}'; run: unnes login")));
    }
    if res.challenge {
        return Err(app_err(5, format!("fetch {page_id}: Cloudflare challenge; back off and retry later")));
    }

    let records = &res.records;
    if csv {
        println!("{}", output::records_csv(records));
    } else if json_out {
        println!("{}", output::records_json(records));
    } else {
        println!("{}", output::records_table(records));
    }

    if let Some(sel) = &page.selector {
        if records.is_empty() {
            return Err(app_err(6, format!("fetch {page_id}: no records matched selector '{sel}' - the page may have changed")));
        }
    }
    Ok(())
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in name.chars() {
        if ch.is_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') { out.pop(); }
    out
}

/// unnes discover: list gateway apps or elena courses with ready-to-run
/// watch add commands.
/// unnes data: List / Show / History / Export of the stored page states.
fn cmd_data(home: &UnnesHome, _profile: &str, cmd: &DataCmd, json_out: bool) -> Result<()> {
    let cfg = Config::load(home)?;
    match cmd {
        DataCmd::List => {
            let mut rows: Vec<serde_json::Value> = Vec::new();
            for page in &cfg.pages {
                let entries = crate::data::read(home, &page.id).unwrap_or_default();
                let last = entries.last().map(|e| e.at.clone()).unwrap_or_default();
                rows.push(serde_json::json!({
                    "page": page.id,
                    "states": entries.len(),
                    "records": entries.last().map(|e| e.records.len()).unwrap_or(0),
                    "last_capture": last,
                }));
            }
            if json_out {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("stored datasets ({}):", rows.len());
                if rows.is_empty() {
                    println!("  none yet - run: unnes watch run");
                }
                for r in &rows {
                    let last = r["last_capture"].as_str().unwrap_or("-");
                    println!(
                        "  {:<16} {} states, {} records, last {}",
                        r["page"].as_str().unwrap_or(""),
                        r["states"].as_u64().unwrap_or(0),
                        r["records"].as_u64().unwrap_or(0),
                        &last[..11.min(last.len())],
                    );
                }
            }
        }
        DataCmd::Show { page_id, csv } => {
            let latest = crate::data::latest(home, page_id)?
                .ok_or_else(|| app_err(1, format!("no stored data for '{page_id}' yet; run: unnes watch run")))?;
            if *csv {
                println!("{}", output::records_csv(&latest.records));
            } else if json_out {
                println!("{}", output::records_json(&latest.records));
            } else {
                let at = &latest.at[..11.min(latest.at.len())];
                println!("{page_id} @ {at}:");
                println!("{}", output::records_table(&latest.records));
            }
        }
        DataCmd::History { page_id } => {
            let entries = crate::data::read(home, page_id)?;
            if entries.is_empty() {
                println!("no stored data for '{page_id}' yet; run: unnes watch run");
                return Ok(());
            }
            let rows: Vec<serde_json::Value> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| serde_json::json!({ "#": i + 1, "at": e.at, "records": e.records.len() }))
                .collect();
            if json_out {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("{page_id} history ({} states):", entries.len());
                println!("{}", output::records_table(&rows));
            }
        }
        DataCmd::Export { page_id, csv } => {
            let entries = crate::data::read(home, page_id)?;
            if entries.is_empty() {
                println!("no stored data for '{page_id}' yet; run: unnes watch run");
                return Ok(());
            }
            if *csv {
                let last = entries.last().unwrap();
                println!("{}", output::records_csv(&last.records));
            } else {
                let full: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| serde_json::json!({ "at": e.at, "records": e.records }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&full)?);
            }
        }
    }
    Ok(())
}

/// unnes kurikulum: full curriculum grouped by semester with status
/// categories (LULUS / BERJALAN / BELUM DITEMPUH) per mata kuliah.
fn cmd_kurikulum(home: &UnnesHome, profile: &str, json_out: bool) -> Result<()> {
    let nim = kurikulum::resolve_nim(home)
        .ok_or_else(|| app_err(1, "cannot determine NIM - set [general] nim in config or run unnes watch run first (biodata)"))?;
    let kursus = kurikulum::fetch_and_parse(home, profile, &nim, true).map_err(|e| match format!("{e:#}") {
        m if m.contains("session unavailable") || m.contains("session expired") => app_err(4, format!("kurikulum: {m}")),
        m => app_err(1, format!("kurikulum: {m}")),
    })?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "nim": nim,
            "total": kursus.len(),
            "semester": kursus,
        }))?);
        return Ok(());
    }

    let mut by_sem: std::collections::BTreeMap<u32, Vec<&kurikulum::Kursus>> = Default::default();
    let mut lulus = 0u32;
    let mut sks_lulus = 0u32;
    let mut sks_total = 0u32;
    for k in &kursus {
        by_sem.entry(k.semester).or_default().push(k);
        sks_total += k.sks;
        if k.kategori() == "LULUS" {
            lulus += 1;
            sks_lulus += k.sks;
        }
    }
    println!("Kurikulum Teknik Informatika, S1 (angkatan 2024) - {} mata kuliah, {} SKS", kursus.len(), sks_total);
    println!("Lulus: {} MK / {} SKS  |  Sisa: {} SKS", lulus, sks_lulus, sks_total - sks_lulus);
    for (sem, list) in &by_sem {
        let done = list.iter().filter(|k| k.kategori() == "LULUS").count();
        let running = list.iter().filter(|k| k.kategori() == "BERJALAN").count();
        println!();
        println!("--- Semester {} ({}) | lulus {} | berjalan {} | belum {} ---", sem, list.len(), done, running, list.len() - done - running);
        for k in list {
            let mark = match k.kategori() {
                "LULUS" => "lulus",
                "BERJALAN" => "sedang",
                _ => "belum",
            };
            let g = if k.nilai().is_empty() { String::new() } else { format!(" [{}]", k.nilai()) };
            println!("   {:3}  {:<7} {:<42} {:>2} SKS  {}{}", k.no % 100, k.kode, k.nama, k.sks, mark, g);
        }
    }
    Ok(())
}

/// unnes jadwal: weekly class schedule from the Sikadu 2.4 KRS form.
fn cmd_jadwal(home: &UnnesHome, profile: &str, json_out: bool) -> Result<()> {
    let nim = kurikulum::resolve_nim(home)
        .ok_or_else(|| app_err(1, "cannot determine NIM - set [general] nim in config or run unnes watch run first (biodata)"))?;
    let (sesi, info) = jadwal::fetch_and_parse(home, profile, &nim, true).map_err(|e| match format!("{e:#}") {
        m if m.contains("session unavailable") || m.contains("session expired") => app_err(4, format!("jadwal: {m}")),
        m => app_err(1, format!("jadwal: {m}")),
    })?;
    if sesi.is_empty() {
        return Err(app_err(1, "jadwal: no sessions parsed - the KRS form may be empty or changed"));
    }

    if json_out {
        println!("{}", serde_json::to_string_pretty(&sesi)?);
        return Ok(());
    }

    let mut hari_sekarang = "";
    for s in &sesi {
        if s.hari != hari_sekarang {
            hari_sekarang = &s.hari;
            println!();
            println!("=== {} ===", s.hari.to_uppercase());
        }
        println!(
            "  {} - {}  {:<32} {:<22} {} SKS {}",
            s.mulai, s.selesai, s.mata_kuliah, s.ruang, s.sks, s.tipe
        );
    }
    println!();
    println!("{} sessions / {} mata kuliah | semester {} | IPK {} | {} SKS", sesi.len(), sesi.iter().map(|s| &s.mata_kuliah).collect::<std::collections::HashSet<_>>().len(), info.semester, info.ipk, info.sks_plan);
    Ok(())
}

/// unnes tugas: Elena assignments + quizzes across all courses, with due
/// dates and submission status. New items appear here the moment the
/// professor adds them (the watch crawl also logs them as changes).
fn cmd_tugas(home: &UnnesHome, profile: &str, json_out: bool) -> Result<()> {
    let items = tugas::fetch_items(home, profile).map_err(|e| match format!("{e:#}") {
        m if m.contains("session unavailable") => app_err(4, format!("tugas: {m}")),
        m => app_err(1, format!("tugas: {m}")),
    })?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("Belum ada tugas/kuis di Elena - item baru akan muncul di sini begitu dosen menambahkannya.");
        return Ok(());
    }
    println!("=== TUGAS & KUIS ELENA ({} item) ===", items.len());
    for it in &items {
        let mark = match it.status.as_str() {
            "Submitted" => "OK dikumpulkan",
            "Belum dikumpulkan" | "Draft" => "!! BELUM",
            _ => if it.status.is_empty() { "?" } else { "?" },
        };
        println!("[{}] {}
   course {} | {} | due: {} | status: {} ({})", mark, it.nama, it.course, it.kategori, if it.due.is_empty() { "-" } else { &it.due }, if it.status.is_empty() { "-" } else { &it.status }, it.url);
    }
    Ok(())
}

fn cmd_discover(home: &UnnesHome, profile: &str, args: &DiscoverArgs, json_out: bool) -> Result<()> {
    if !home.profile_jar_file(profile).is_file() {
        return Err(app_err(3, format!("not logged in (profile {profile}); run: unnes login")));
    }
    let semester = args.semester.as_deref().unwrap_or("20261");
    if args.elena {
        let mut job = fetcher::job("page", profile);
        job["url"] = json!("https://elena.unnes.ac.id/my/courses.php");
        job["ssoApp"] = json!("30");
        job["semester"] = json!(semester);
        job["extract"] = json!({
            "selector": "a[href*='/course/view.php']",
            "fields": { "name": "", "url": "@href" },
        });
        let res = fetcher::run_job(home, profile, job)?;
        if !res.ok {
            let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
            return Err(app_err(err_code_for(&code), format!("discover: {}", err_msg(&res))));
        }
        if res.session_expired {
            return Err(app_err(4, "session expired; run: unnes login"));
        }
        let mut seen = std::collections::HashSet::new();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for r in &res.records {
            let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if name.is_empty() || url.is_empty() || !seen.insert(url.clone()) { continue; }
            rows.push(serde_json::json!({ "name": name, "url": url, "suggested_id": slugify(&name) }));
        }
        if json_out {
            println!("{}", serde_json::to_string_pretty(&rows)?);
            return Ok(());
        }
        println!("Elena courses ({}):", rows.len());
        for r in &rows {
            let name = r["name"].as_str().unwrap_or("");
            let url = r["url"].as_str().unwrap_or("");
            let id = r["suggested_id"].as_str().unwrap_or("");
            println!("  {name}");
            println!("    unnes watch add {id} --url={url} --selector=.activity-item --render --sso-app=30 --sso-semester={semester}");
        }
        return Ok(());
    }

    // Gateway apps (plain HTTP with the jar).
    let mut job = fetcher::job("get", profile);
    job["url"] = json!("https://apps.unnes.ac.id/gate/list");
    job["extract"] = json!({
        "selector": "a[href*='apps.unnes.ac.id/']",
        "fields": { "name": "", "url": "@href" },
    });
    let res = fetcher::run_job(home, profile, job)?;
    if !res.ok {
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        return Err(app_err(err_code_for(&code), format!("discover: {}", err_msg(&res))));
    }
    if res.session_expired {
        return Err(app_err(4, "session expired; run: unnes login"));
    }
    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for r in &res.records {
        let name = r.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let app_id = url.rsplit('/').next().unwrap_or("");
        if name.is_empty() || app_id.is_empty() || !app_id.chars().all(|c| c.is_ascii_digit()) { continue; }
        if !seen.insert(app_id.to_string()) { continue; }
        rows.push(serde_json::json!({ "id": app_id, "name": name }));
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!("UNNES gateway apps ({}):", rows.len());
    for r in &rows {
        println!("  {} = {}", r["id"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or(""));
    }
    println!("prime a session with: unnes watch add <id> --url=<app-url> --render --sso-app=<app-id>");
    Ok(())
}

fn cmd_watch_run(home: &UnnesHome, profile: &str, only: Option<&str>, json_out: bool) -> Result<()> {
    if !home.profile_jar_file(profile).is_file() {
        return Err(app_err(3, format!("not logged in (profile {profile}); run: unnes login")));
    }
    let outcomes = watch::run_pass(home, profile, only)?;
    if outcomes.is_empty() {
        println!("no pages configured ({}); add one with: unnes watch add <id> --url=<page-url> --selector=<css>", home.config_file().display());
        return Ok(());
    }
    let mut had_session_error = false;
    for o in &outcomes {
        if json_out {
            println!("{}", serde_json::to_string(&serde_json::json!({
                "page": o.page_id,
                "changed": o.changed,
                "summary": o.summary,
            }))?);
        } else {
            let mark = if o.changed { "CHANGED" } else { "ok" };
            println!("[{}] {} {}", o.page_id, mark, o.summary);
        }
        if o.summary.contains("session expired") { had_session_error = true; }
    }
    if had_session_error {
        return Err(app_err(4, "session expired for one or more pages; run: unnes login"));
    }
    Ok(())
}

fn watch_list(home: &UnnesHome, json: bool) -> Result<()> {
    let cfg = Config::load(home)?;
    if cfg.pages.is_empty() {
        println!("no pages configured ({});", home.config_file().display());
        println!("add one with: unnes watch add <id> --url=<page-url> --selector=<css>");
        return Ok(());
    }
    let records: Vec<serde_json::Value> = cfg
        .pages
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "url": p.url,
                "interval": p.interval.unwrap_or(cfg.general.default_interval),
                "selector": p.selector.as_deref().unwrap_or("(none)"),
                "key_field": p.key_field.as_deref().unwrap_or("(first field)"),
            })
        })
        .collect();
    if json {
        println!("{}", output::records_json(&records));
    } else {
        println!("{}", output::records_table(&records));
    }
    Ok(())
}

fn changelog_list(home: &UnnesHome, args: &ChangelogArgs, json: bool) -> Result<()> {
    let entries = changelog::read(home, args.since.as_deref(), args.page_id.as_deref())?;
    if entries.is_empty() {
        println!("no changelog entries yet (run unnes watch run once per page to record baselines)");
        return Ok(());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    let records: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "at": e.at,
                "page": e.page_id,
                "event": e.event,
                "summary": e.summary(),
            })
        })
        .collect();
    println!("{}", output::records_table(&records));
    Ok(())
}
