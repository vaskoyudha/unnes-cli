# unnes-cli

A headless, cron-friendly CLI for the **UNNES student portal** (student.unnes.ac.id,
authenticated via the Google SSO hub at apps.unnes.ac.id):

- **Login** once via Google SSO (a Chromium window opens, you sign in, the session
  cookie jar is captured). Everything after that is plain HTTP and headless.
- **Fetch** configured pages (grades / schedule / announcements) as human tables,
  --json, or --csv.
- **Watch** config-driven pages: poll -> normalize -> hash -> diff -> changelog, with
  adaptive intervals, jitter, backoff, and an optional command/webhook hook on change.

Architecture: a Rust CLI shell (clap) drives a Node/TS fetch arm
(fetcher/, per fetcher/CONTRACT.md v1). One JSON job in, one JSON result out.
Login uses Playwright for the Google SSO step only; polling is plain HTTP.
A TUI is explicitly out of v1.

> **Ethics & fairness** - this tool automates *your own student data* only. It polls
> politely (15-30 min default, jittered), backs off on 429/403, and never bypasses
> Cloudflare protections. Do not use for mass scraping or shared credentials.

## Status

| Milestone | State |
|---|---|
| M0 scaffold + git | done |
| M1 core Rust (config/paths/output/diff/changelog) | done (22 tests) |
| M2 Node/TS fetcher arm | done (11 contract tests) |
| M3 session (login/status/logout/fetch) | done - Google SSO login, exit codes 0-6 |
| M4 watch engine + daemon | todo |
| M5 release v0.1.0 + docs | todo |

## Requirements

- Rust 1.97+ (cargo build --release -> target/release/unnes)
- Node.js >= 20 (fetch arm: cd fetcher && npm ci && npm run build)
- Chromium for the SSO login step (one time: cd fetcher && npx playwright install chromium)

## Login & the browser profile

unnes login opens apps.unnes.ac.id in a headed Chromium window backed by a
PERSISTENT profile at $UNNES_HOME/browser-profiles/<profile> (0700). Your
Google account choice and 2FA trust survive between logins, so re-logins after
the portal's session expiry are one click instead of a full Google re-auth.
The portal itself calls auth2.disconnect() after every login, so a Google
sign-in always happens - the stored profile only makes it easy. Note: this
profile stores Google session data on disk under UNNES_HOME; it is never
uploaded or committed (gitignored).

## Quick start

1. Build: cargo build --release
2. Log in: unnes login  (a browser window opens; sign in with your UNNES Google
   account; the session jar is saved when the SSO handoff lands or you press Enter)
3. Point a watch at a page (after your first login the CLI prints the SSO landing
   URL - that is where the app lives):
   unnes watch add grades --url=<page-url> --selector=<table-rows-css>
4. Fetch it: unnes fetch grades  (or: unnes grades / schedule / announcements)
5. Session state: unnes status / unnes logout

Exit codes: 0 ok, 1 generic, 2 usage, 3 not logged in, 4 session expired,
5 network/429/challenge, 6 selector matched nothing (page may have changed).

## Page discovery (runbook)

The post-SSO app layout is not hardcoded: after unnes login, open the printed
landing URL in a browser and inspect which page holds grades/schedule/announcements.
Then register each page (see Quick start) - the CLI never guesses URLs. Full
discovery docs land in M5.

## Notes

- The legacy email+password form on student.unnes.ac.id still exists, but the
  canonical login is Google SSO; form login (mode=form) is kept and tested in the
  fetcher as a fallback.
- Session expiry surfaces as exit code 4; re-run unnes login (SSO cannot be
  automated headlessly - that is intentional).
- The fetcher is located via $UNNES_FETCHER, then ./fetcher/dist/index.js, then
  <exe>/../fetcher/dist/index.js.
