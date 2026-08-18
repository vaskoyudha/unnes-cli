//! unnes-cli — UNNES student portal CLI.
//!
//! Milestones: M1 core (this), M2 node fetcher, M3 session, M4 watch engine, M5 release.

// Modules are consumed by later milestones (M2/M3/M4) plus their own tests;
// silence the interim dead-code noise that would otherwise spam every build.
#![allow(dead_code)]

mod changelog;
mod config;
mod diff;
mod fetcher;
mod output;
mod paths;

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
    /// Watch commands: add/rm/list/run/daemon. [M4]
    Watch(WatchArgs),
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
            WatchCmd::Add { .. } => Err(not_yet("watch add", "M4")),
            WatchCmd::Rm { id } => Err(not_yet(&format!("watch rm {id}"), "M4")),
            WatchCmd::Run { .. } => Err(not_yet("watch run", "M4")),
            WatchCmd::Daemon => Err(not_yet("watch daemon", "M4")),
        },
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
    let jar = home.profile_jar_file(profile);
    if !jar.is_file() {
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json!({ "profile": profile, "logged_in": false }))?);
        }
        return Err(app_err(3, format!("not logged in (profile {profile}); run: unnes login")));
    }
    let modified = jar.metadata()?.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let age_secs = SystemTime::now().duration_since(modified).unwrap_or_default().as_secs();
    let meta_path = home.profile_meta_file(profile);
    let landing: Option<String> = if meta_path.is_file() {
        serde_json::from_str::<serde_json::Value>(&fs::read_to_string(&meta_path)?)
            .ok()
            .and_then(|v| v.get("landing_url").and_then(|l| l.as_str().map(String::from)))
    } else {
        None
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({
            "profile": profile,
            "logged_in": true,
            "jar_age_seconds": age_secs,
            "landing_url": landing,
        }))?);
    } else {
        println!("profile: {profile}");
        println!("session: saved (jar updated {age_secs}s ago)");
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
    let mut job = if page.link_selector.is_some() {
        fetcher::job("crawl", profile)
    } else if page.render.unwrap_or(false) {
        fetcher::job("page", profile)
    } else {
        fetcher::job("get", profile)
    };
    job["url"] = json!(page.url);
    if let Some(sel) = &page.selector {
        job["extract"] = json!({ "selector": sel });
    }
    if let Some(sel) = &page.link_selector {
        job["linkSelector"] = json!(sel);
    }
    if let Some(pre) = &page.pre_url {
        job["preUrl"] = json!(pre);
    }
    if let Some(sem) = &page.sso_semester {
        job["semester"] = json!(sem);
    }
    if !page.normalize.is_empty() {
        job["extraRegexes"] = json!(page.normalize);
    }
    if let Some(app) = &page.sso_app {
        job["ssoApp"] = json!(app);
    }
    let res = fetcher::run_job(home, profile, job)?;
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
