# unnes-cli

A headless, cron-friendly CLI for the **UNNES student portal** (student.unnes.ac.id):

- **Login** once, persist the authenticated Laravel session (cookie jar), auto re-login on expiry.
- **Fetch** grades / schedule / announcements as human tables, --json, or --csv.
- **Watch** config-driven pages: poll -> normalize -> hash -> diff -> changelog, with adaptive
  intervals, jitter, backoff, and an optional command/webhook hook on change.

Built per HANDOFF.md: Rust CLI shell (clap) + a Node/TS fetch arm (plain HTTP first; browser
automation is a later fallback). The engine is headless; a TUI is explicitly out of v1.

> **Ethics & fairness** - this tool automates *your own student data* only. It polls
> politely (15-30 min default, jittered), backs off on 429/403, and never bypasses
> Cloudflare protections. Do not use for mass scraping or shared credentials.

## Status

| Milestone | State |
|---|---|
| M0 scaffold + git | done (this commit) |
| M1 core Rust (config/paths/output/diff/changelog) | todo |
| M2 Node/TS fetcher arm | todo |
| M3 session (login/status/logout/auto re-login) | todo |
| M4 watch engine + daemon | todo |
| M5 release v0.1.0 + docs | todo |

## Requirements

- Rust 1.97+ (cargo build --release -> target/release/unnes)
- Node.js >= 20 (fetch arm: cd fetcher && npm ci && npm run build)

Docs (install, config schema, command reference, page-discovery runbook) land in M5.
