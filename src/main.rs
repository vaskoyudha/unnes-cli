//! unnes-cli — UNNES student portal CLI.
//!
//! Milestones: M1 core (this), M2 node fetcher, M3 session, M4 watch engine, M5 release.

// Modules are consumed by later milestones (M2/M3/M4) plus their own tests;
// silence the interim dead-code noise that would otherwise spam every build.
#![allow(dead_code)]

mod changelog;
mod config;
mod diff;
mod output;
mod paths;

use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use crate::config::Config;
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
    /// Save an authenticated session (interactive prompt). [M3]
    Login(LoginArgs),
    /// Forget the saved session. [M3]
    Logout,
    /// Session state, last sync, next poll. [M3]
    Status,
    /// Fetch one configured page and print records. [M3]
    Fetch(FetchArgs),
    /// Fetch the grades page (alias for fetch grades). [M3]
    Grades(FetchArgs),
    /// Fetch the schedule page (alias for fetch schedule). [M3]
    Schedule(ScheduleArgs),
    /// Fetch the announcements page (alias for fetch announcements). [M3]
    Announcements(FetchArgs),
    /// Watch commands: add/rm/list/run/daemon. [M4]
    Watch(WatchArgs),
    /// Print the change log.
    Changelog(ChangelogArgs),
}

#[derive(Args)]
struct LoginArgs {
    /// Account email (prompted when omitted)
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
    match cli.cmd {
        Cmd::Login(_) => Err(not_yet("login", "M3")),
        Cmd::Logout => Err(not_yet("logout", "M3")),
        Cmd::Status => Err(not_yet("status", "M3")),
        Cmd::Fetch(a) => Err(not_yet(&format!("fetch {}", a.page_id), "M3")),
        Cmd::Grades(_) => Err(not_yet("grades", "M3")),
        Cmd::Schedule(_) => Err(not_yet("schedule", "M3")),
        Cmd::Announcements(_) => Err(not_yet("announcements", "M3")),
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
