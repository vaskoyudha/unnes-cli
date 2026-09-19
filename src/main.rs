//! unnes-cli — UNNES student portal CLI.
//!
//! Milestones: M1 core (this), M2 node fetcher, M3 session, M4 watch engine, M5 release.

// Modules are consumed by later milestones (M2/M3/M4) plus their own tests;
// silence the interim dead-code noise that would otherwise spam every build.
#![allow(dead_code)]

mod cache;
mod changelog;
mod config;
mod data;
mod diff;
mod fetcher;
mod jadwal;
mod kurikulum;
mod materi;
mod output;
mod paths;
mod peserta;
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
    Tugas(TugasArgs),
    /// Materi: Elena course materials (files/resources) - list and download
    Materi(MateriArgs),
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

#[derive(Args)]
struct TugasArgs {
    /// Only this course (substring of the course name, case-insensitive)
    #[arg(long)]
    course: Option<String>,
    /// Only unsubmitted items (nearest deadline first)
    #[arg(long)]
    pending: bool,
    #[command(subcommand)]
    cmd: Option<TugasCmd>,
}

#[derive(Args)]
struct MateriArgs {
    /// Only this course (substring of the course name, case-insensitive)
    #[arg(long)]
    course: Option<String>,
    #[command(subcommand)]
    cmd: Option<MateriCmd>,
}

#[derive(Subcommand)]
enum TugasCmd {
    /// Upload a file to an Elena assignment (mod/assign/view.php?id=<cmid>)
    Submit {
        /// Course module id from the task URL (e.g. 11226)
        cmid: u32,
        /// Path to the local file to upload
        #[arg(long)]
        file: String,
        /// Finalize the submission (Submit assignment) instead of saving a draft
        #[arg(long)]
        submit: bool,
    },
}

#[derive(Subcommand)]
enum MateriCmd {
    /// Download one material (unique substring of its name or URL) into DIR
    Download {
        /// Substring matching the material name or URL (must match exactly one)
        query: String,
        /// Destination directory (created when missing; default: current dir)
        #[arg(long)]
        out: Option<String>,
    },
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
                println!("page '{id}' added; run: unnes watch run --page-id {id}");
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
        Cmd::Tugas(a) => match a.cmd {
            None => cmd_tugas(&home, &profile, a.course.as_deref(), a.pending, cli.json),
            Some(TugasCmd::Submit { cmid, file, submit }) => cmd_tugas_submit(&home, &profile, cmid, &file, submit, cli.json),
        },
        Cmd::Materi(a) => match &a.cmd {
            None => cmd_materi_list(&home, &profile, a.course.as_deref(), cli.json),
            Some(MateriCmd::Download { query, out }) => cmd_materi_download(&home, &profile, query, out.as_deref(), cli.json),
        },
        Cmd::Tui => tui::run(&home, &profile),
        Cmd::Changelog(a) => changelog_list(&home, &a, cli.json),
    }
}

/// Map a fetcher error code to the CLI exit code spec.
fn err_code_for(code: &str) -> u8 {
    match code {
        "usage" | "contract" => 2,
        "network" | "timeout" | "challenge" | "ratelimit" => 5,
        _ => 1,
    }
}

/// Exit code for fetch-family failures from the full message: session lapses
/// -> 4, transport/challenge -> 5, everything else -> 1. Transport errors must
/// never masquerade as "re-login" (exit 4), and session lapses must never
/// hide as generic errors (exit 1) - both mislabelings were systemic.
fn exit_for_fetch_fail(msg: &str) -> u8 {
    let m = msg.to_lowercase();
    if m.contains("session expired")
        || m.contains("session unavailable")
        || m.contains("(session)")
        || m.contains("needsinteraction")
        || m.contains("needs interaction")
    {
        4
    } else if m.contains("timed out")
        || m.contains("timeout")
        || m.contains("network")
        || m.contains("challenge")
        || m.contains("cloudflare")
        || m.contains("connection")
        || m.contains("(timeout)")
        || m.contains("(challenge)")
        || m.contains("http 4")
        || m.contains("http 5")
    {
        5
    } else {
        1
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
    crate::changelog::write_atomic(&home.profile_meta_file(profile), serde_json::to_string_pretty(&meta)?.as_bytes())?;
    println!("logged in (profile {profile}), {} cookies captured", res.captured_cookies.unwrap_or(0));
    if res.google_persistent == Some(0) {
        println!("WARNING: Google sign-in will NOT persist (no persistent SID-family cookie kept).");
        println!("Next login will show an empty account chooser. Fix now: re-run unnes login,");
        println!("sign into the accounts.google.com tab first with 'Stay signed in', then click");
        println!("'Login dengan UNNES-ID' - and let unnes close the window itself.");
    }
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
    println!("local session cleared (profile {profile}); server sessions expire on their own");
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
    // server-side session; jar-file existence alone is not proof. Elena gets
    // its own probe (same as the TUI): its Moodle session outlives the
    // gateway one, so gateway-dead + elena-alive is not "logged out".
    let mut valid = false;
    let mut elena_valid = false;
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
        if !valid {
            let mut ejob = fetcher::job("get", profile);
            ejob["url"] = json!("https://elena.unnes.ac.id/my/");
            if let Ok(res) = fetcher::run_job(home, profile, ejob) {
                elena_valid = res.ok && !res.session_expired;
            }
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
    // Auto re-login for status: scripted attempt only (non-interactive) - a
    // read-only probe must never pop a click-waiting browser window.
    // Skipped when Elena is alive: its session usually survives the gateway
    // lapse and most commands self-heal from it.
    if !valid && !elena_valid && cfg.general.auto_relogin {
        probe_err = match watch::auto_login(home, profile, false) {
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
    if !valid && !elena_valid {
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
            "elena": elena_valid,
        }))?);
    } else {
        println!("profile: {profile}");
        if valid {
            println!("session: VALID (gateway confirmed)");
        } else {
            println!("session: PARTIAL (gateway ended, Elena alive - most commands self-heal)");
        }
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
        m => app_err(exit_for_fetch_fail(&m), format!("kurikulum: {m}")),
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
        m => app_err(exit_for_fetch_fail(&m), format!("jadwal: {m}")),
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

/// unnes tugas: course picker first, then the must-submit popup.
///
/// No flags: print the mata kuliah list (total/belum per course) plus the
/// HARUS DIKUMPULKAN popup - unsubmitted items nearest deadline first, with
/// urgency warnings. --course selects one course, --pending lists only the
/// unsubmitted items. --json keeps the flat item array (filtered).
fn cmd_tugas(home: &UnnesHome, profile: &str, course: Option<&str>, pending_only: bool, json_out: bool) -> Result<()> {
    let items = tugas::fetch_items(home, profile, true).map_err(|e| match format!("{e:#}") {
        m => app_err(exit_for_fetch_fail(&m), format!("tugas: {m}")),
    })?;

    let mut view: Vec<&tugas::TugasItem> = items.iter().collect();
    if let Some(f) = course {
        view = tugas::filter_course(&items, f);
        if view.is_empty() {
            println!("tidak ada matakuliah/tugas yang cocok dengan '{f}'. Pilihan:");
            for (c, t, p) in tugas::courses(&items) {
                println!("  {c} ({t} tugas, {p} belum)");
            }
            return Ok(());
        }
    }
    if pending_only {
        view.retain(|it| tugas::is_pending(it));
    }

    if json_out {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("Belum ada tugas/kuis di Elena - item baru akan muncul di sini begitu dosen menambahkannya.");
        return Ok(());
    }
    if course.is_some() || pending_only {
        print_tugas_list(&view);
        return Ok(());
    }
    // Default: pick a course first, then the must-submit popup.
    println!("=== MATA KULIAH ({} matakuliah, {} tugas) ===", tugas::courses(&items).len(), items.len());
    for (c, t, p) in tugas::courses(&items) {
        let mark = if p > 0 { format!("{p} BELUM") } else { "lengkap".to_string() };
        println!("  {:<34} {:>2} tugas  [{mark}]", c, t);
    }
    println!();
    println!("  lihat satu matakuliah: unnes tugas --course <nama>");
    println!();
    print_tugas_popup(&items);
    Ok(())
}

fn tugas_due(it: &tugas::TugasItem) -> &str {
    if it.due.is_empty() { "-" } else { &it.due }
}

/// The HARUS DIKUMPULKAN popup: unsubmitted items nearest deadline first.
fn print_tugas_popup(items: &[tugas::TugasItem]) {
    let pending = tugas::pending_sorted(items);
    if pending.is_empty() {
        println!("=== HARUS DIKUMPULKAN: tidak ada - semua sudah dikumpulkan. ===");
        return;
    }
    println!("=== HARUS DIKUMPULKAN ({} item, deadline terdekat dulu) ===", pending.len());
    for (i, it) in pending.iter().take(15).enumerate() {
        let u = tugas::urgency(it);
        let warn = tugas::urgency_mark(u);
        let status = if it.status.is_empty() { "-" } else { &it.status };
        println!("{}. [{}] {} ({})", i + 1, if warn.is_empty() { "jadwal" } else { warn }, it.nama, it.course);
        println!("   {} | {} | due: {} | status: {} ({})", it.kategori, it.course, tugas_due(it), status, it.url);
    }
    if pending.len() > 15 {
        println!("   ... dan {} lagi (unnes tugas --pending untuk semua)", pending.len() - 15);
    }
}

fn print_tugas_list(view: &[&tugas::TugasItem]) {
    if view.is_empty() {
        println!("tidak ada item yang cocok.");
        return;
    }
    println!("=== TUGAS & KUIS ({} item) ===", view.len());
    for it in view {
        let u = tugas::urgency(it);
        let warn = tugas::urgency_mark(u);
        let mark = match it.status.as_str() {
            "Submitted" => "OK dikumpulkan",
            "Belum dikumpulkan" | "Draft" => "BELUM",
            _ => if it.status.is_empty() { "?" } else { "?" },
        };
        let status = if it.status.is_empty() { "-" } else { &it.status };
        println!("[{mark}] {}{}\n   course {} | {} | due: {} | status: {} ({})",
            it.nama,
            if warn.is_empty() || u == tugas::Urgency::Done { String::new() } else { format!("  <-- {warn}") },
            it.course, it.kategori, tugas_due(it), status, it.url);
    }
}

/// unnes materi: list course materials (files/resources lecturers uploaded).
fn cmd_materi_list(home: &UnnesHome, profile: &str, course: Option<&str>, json_out: bool) -> Result<()> {
    let items = crate::materi::fetch_materi(home, profile, true).map_err(|e| match format!("{e:#}") {
        m => app_err(exit_for_fetch_fail(&m), format!("materi: {m}")),
    })?;
    let view: Vec<&crate::materi::MateriItem> = match course {
        Some(f) => crate::materi::filter_course(&items, f),
        None => items.iter().collect(),
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&view)?);
        return Ok(());
    }
    if items.is_empty() {
        println!("Belum ada materi di Elena.");
        return Ok(());
    }
    if view.is_empty() {
        println!("tidak ada materi yang cocok. Pilihan matakuliah:");
        for (c, n) in crate::materi::courses(&items) {
            println!("  {c} ({n} materi)");
        }
        return Ok(());
    }
    if course.is_none() {
        println!("=== MATERI ({} file, {} matakuliah) ===", view.len(), crate::materi::courses(&items).len());
        for (c, n) in crate::materi::courses(&items) {
            println!("  {c:<34} {n} materi");
        }
        println!();
        println!("  lihat satu matakuliah: unnes materi --course <nama>");
        println!("  download: unnes materi download <nama-file> --out <dir>");
        return Ok(());
    }
    let mut last_course = String::new();
    for it in &view {
        if it.course != last_course {
            last_course = it.course.clone();
            println!("=== {} ===", it.course);
        }
        println!("  [{:<8}] {}\n             {}", it.kind, it.nama, it.url);
    }
    println!();
    println!("  download: unnes materi download <nama-file> --out <dir>");
    Ok(())
}

/// unnes materi download <query> [--out DIR]: save one material to disk.
fn cmd_materi_download(home: &UnnesHome, profile: &str, query: &str, out: Option<&str>, json_out: bool) -> Result<()> {
    let items = crate::materi::fetch_materi(home, profile, true).map_err(|e| match format!("{e:#}") {
        m => app_err(exit_for_fetch_fail(&m), format!("materi: {m}")),
    })?;
    let q = query.to_lowercase();
    let hits: Vec<&crate::materi::MateriItem> = items
        .iter()
        .filter(|it| it.nama.to_lowercase().contains(&q) || it.url.to_lowercase().contains(&q))
        .collect();
    if hits.is_empty() {
        return Err(app_err(1, format!("tidak ada materi yang cocok dengan '{query}' (coba: unnes materi)")));
    }
    if hits.len() > 1 {
        println!("'{query}' cocok dengan {} materi - persempit lagi:", hits.len());
        for it in hits.iter().take(15) {
            println!("  [{}] {} ({})", it.kind, it.nama, it.course);
        }
        return Err(app_err(2, format!("query ambigu: '{query}' cocok dengan {} materi", hits.len())));
    }
    let item = hits[0];
    let out_dir = out.map(std::path::PathBuf::from).unwrap_or_else(|| std::path::PathBuf::from("."));
    let saved = crate::materi::download_materi(home, profile, item, &out_dir)?;
    let size = std::fs::metadata(&saved).map(|m| m.len()).unwrap_or(0);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "ok": true,
            "nama": item.nama,
            "course": item.course,
            "path": saved.to_string_lossy(),
            "bytes": size,
        }))?);
    } else {
        println!("tersimpan: {} ({})\n  {} [{}]", saved.display(), human_size(size), item.nama, item.course);
    }
    Ok(())
}

/// Human file size (never "0 KB" for a real file).
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{} B", bytes)
    }
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
    let mut had_selector_error = false;
    let mut had_error = false;
    for o in &outcomes {
        if json_out {
            println!("{}", serde_json::to_string(&serde_json::json!({
                "page": o.page_id,
                "changed": o.changed,
                "summary": o.summary,
            }))?);
        } else {
            println!("[{}] {} {}", o.page_id, watch::outcome_mark(o), o.summary);
        }
        if o.summary.contains("session expired") { had_session_error = true; }
        if o.selector_empty { had_selector_error = true; }
        if o.summary.starts_with("ERROR") { had_error = true; }
    }
    if had_session_error {
        return Err(app_err(4, "session expired for one or more pages; run: unnes login"));
    }
    if had_selector_error {
        return Err(app_err(6, "selector matched nothing for one or more pages - the page may have changed"));
    }
    if had_error {
        return Err(app_err(1, "one or more pages errored (see ERROR lines above)"));
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

/// unnes tugas submit <cmid> --file=<path> [--submit]
/// Upload a file to an Elena assignment (mod/assign/view.php?id=<cmid>).
/// Default saves a draft; --submit finalizes the submission.
///
/// Retry policy (bounded, max 3 submit attempts): an expired session triggers
/// one auto re-login + retry (existing self-healing); transient failures
/// (network/timeout, profile lock held by another op) back off 2s/5s and
/// retry. Deterministic errors (bad file, no file input on the page) fail
/// fast with a diagnosed cause instead of a bare fetcher message.
fn cmd_tugas_submit(home: &UnnesHome, profile: &str, cmid: u32, file: &str, finalize: bool, json_out: bool) -> Result<()> {
    if !home.profile_jar_file(profile).is_file() {
        return Err(app_err(3, format!("not logged in (profile {profile}); run: unnes login")));
    }
    let path = std::path::Path::new(file);
    if !path.is_file() {
        return Err(app_err(1, format!("file not found: {file}")));
    }
    let semester = tugas::configured_elena_semester(home).unwrap_or_else(|| "20261".into());
    let max_attempts = 3u32;
    let mut attempt = 0u32;
    let mut did_session_retry = false;
    let res = loop {
        attempt += 1;
        let mut job = fetcher::job("submit", profile);
        job["url"] = json!(format!("https://elena.unnes.ac.id/mod/assign/view.php?id={cmid}"));
        job["file"] = json!(file);
        job["action"] = json!(if finalize { "submit" } else { "draft" });
        job["ssoApp"] = json!("30");
        job["semester"] = json!(semester);
        let res = fetcher::run_job(home, profile, job)?;
        if res.ok {
            break res;
        }
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        let msg = res.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        // Expired session -> auto re-login (scripted with the saved profile;
        // one-click window only if Google insists) -> immediate retry, once.
        if res.session_expired && !did_session_retry {
            if let Ok(cfg) = Config::load(home) {
                if cfg.general.auto_relogin {
                    eprintln!("session expired; auto re-login...");
                    if watch::auto_login(home, profile, true).is_ok() {
                        did_session_retry = true;
                        continue;
                    }
                }
            }
        }
        if attempt < max_attempts && is_retryable_submit(&code, &msg) {
            let wait = if attempt == 1 { 2 } else { 5 };
            eprintln!("submit attempt {attempt} failed ({code}): {msg}; retrying in {wait}s...");
            std::thread::sleep(std::time::Duration::from_secs(wait));
            continue;
        }
        break res;
    };
    if !res.ok {
        let code = res.error.as_ref().map(|e| e.code.clone()).unwrap_or_default();
        let msg = res.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        // Session exhaustion must exit 4 (not generic 1) so scripts/cron can
        // tell "re-login" apart from other failures.
        let exit = if code == "session" || res.session_expired { 4 } else { err_code_for(&code) };
        return Err(app_err(exit, format!("submit: {}", diagnose_submit(&code, &msg))));
    }
    // The upload ran but the page shows no proof: warn instead of claiming
    // success so the user verifies on the web instead of assuming.
    if !res.submitted && !res.draft && !res.has_file {
        eprintln!("WARNING: upload finished but no submission state could be verified on the page - open the assignment to confirm.");
    }
    let message = res.message.clone().unwrap_or_else(|| "ok".into());
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({
            "ok": true,
            "cmid": cmid,
            "file": file,
            "action": if finalize { "submit" } else { "draft" },
            "message": message,
            "submitted": res.submitted,
            "draft": res.draft,
            "has_file": res.has_file,
        }))?);
    } else {
        println!("tugas {cmid}: {message}");
    }
    Ok(())
}

/// Retryable submit failures: transient transport problems and a profile lock
/// held by a concurrent op. Everything else (bad cmid, closed form, missing
/// file input) is deterministic - retrying would just burn minutes.
fn is_retryable_submit(code: &str, msg: &str) -> bool {
    if code == "network" || code == "timeout" || code == "challenge" {
        return true;
    }
    let m = msg.to_lowercase();
    if m.contains("profile in use") || m.contains("profile locked") || m.contains("already in use") {
        return true;
    }
    if code == "internal"
        && (m.contains("timeout")
            || m.contains("timed out")
            || m.contains("navigation")
            || m.contains("net::")
            || m.contains("protocol")
            || m.contains("closed")
            || m.contains("crash"))
    {
        return true;
    }
    false
}

/// Turn a bare fetcher error into a diagnosed cause with a next action.
fn diagnose_submit(code: &str, msg: &str) -> String {
    let base = format!("{msg} ({code})");
    let m = msg.to_lowercase();
    let hint = if code == "session" {
        "the portal session lapsed and auto re-login did not recover it - run: unnes login, then retry"
    } else if m.contains("could not attach") {
        "the assignment shows no file input - it may already be finalized, past due (form closed), or the cmid is wrong - open the assignment URL to check"
    } else if m.contains("profile in use") || m.contains("locked") || m.contains("already in use") {
        "another unnes browser operation is holding the profile - wait ~1 min and retry"
    } else if code == "network" || code == "timeout" || code == "challenge" || m.contains("timeout") {
        "transient network/timeout talking to Elena and retries are exhausted - check your connection and retry"
    } else if m.contains("no_browser") || m.contains("unnes_no_browser") {
        "browser operations are disabled (UNNES_NO_BROWSER is set) - unset it to submit"
    } else {
        ""
    };
    if hint.is_empty() {
        base
    } else {
        format!("{base} - {hint}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_retries_transient_but_not_deterministic_errors() {
        assert!(is_retryable_submit("network", "page render failed: net::ERR_TIMEOUT"));
        assert!(is_retryable_submit("timeout", "waiting timed out"));
        assert!(is_retryable_submit("internal", "submit failed at stage 'open': Navigation timeout exceeded"));
        assert!(is_retryable_submit("usage", "profile in use by another unnes instance"));
        assert!(!is_retryable_submit("usage", "could not attach the file at stage 'attach': no file input"));
        assert!(!is_retryable_submit("usage", "file not found: /tmp/x.pdf"));
        assert!(!is_retryable_submit("session", "gateway session expired at stage 'prime'"));
        assert!(!is_retryable_submit("internal", "submit failed at stage 'verify': unexpected bug"));
    }

    #[test]
    fn submit_errors_carry_a_diagnosed_cause() {
        assert!(diagnose_submit("session", "session expired at stage 'open'").contains("unnes login"));
        assert!(diagnose_submit("usage", "could not attach the file at stage 'attach'").contains("finalized"));
        assert!(diagnose_submit("network", "boom").contains("retries are exhausted"));
        assert!(diagnose_submit("internal", "submit failed at stage 'open': Navigation timeout").contains("retries are exhausted"));
        // Unknown errors pass through undiagnosed rather than misdiagnosed.
        assert_eq!(diagnose_submit("usage", "mystery"), "mystery (usage)");
    }

    #[test]
    fn fetch_failures_map_to_the_right_exit_code() {
        assert_eq!(exit_for_fetch_fail("elena session unavailable; run: unnes login"), 4);
        assert_eq!(exit_for_fetch_fail("gateway session expired at stage 'prime'"), 4);
        assert_eq!(exit_for_fetch_fail("boom (session)"), 4);
        assert_eq!(exit_for_fetch_fail("page render failed: net::ERR_TIMEOUT"), 5);
        assert_eq!(exit_for_fetch_fail("download failed with HTTP 403 for https://x"), 5);
        assert_eq!(exit_for_fetch_fail("Cloudflare challenge; backing off"), 5);
        assert_eq!(exit_for_fetch_fail("boom (network)"), 5);
        assert_eq!(exit_for_fetch_fail("mystery"), 1);
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(1024), "1 KB");
        assert_eq!(human_size(104629), "102 KB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MB");
    }
}
