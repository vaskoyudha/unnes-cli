# unnes-cli

A headless, cron-friendly CLI for the **UNNES ecosystem**: Google SSO hub
(apps.unnes.ac.id) with app sessions for Sikadu/Akademik (akademik.unnes.ac.id)
and the Elena e-learning portal (elena.unnes.ac.id):

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
| M3b portal scraping (sso exchange, render, crawl) | done - akademik + elena sessions, per-mata-kuliah crawl |
| M4 watch engine (run/daemon, diff, changelog, notify) | done - watch run/daemon, snapshots, notify hook |
| M5 release v0.1.0 + docs | todo |

## Requirements

- Rust 1.97+ (cargo build --release -> target/release/unnes)
- Node.js >= 20 (fetch arm: cd fetcher && npm ci && npm run build)
- Chromium for the SSO login step (one time: make chromium)

## Install

    make build fetcher      # release binary + fetcher dist
    make install            # -> ~/.cargo/bin/unnes
    make test               # full Rust + fetcher suites

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
5. Watch: unnes watch run  (one pass) or unnes watch daemon (adaptive polling)
6. Session state: unnes status / unnes logout

Exit codes: 0 ok, 1 generic, 2 usage, 3 not logged in, 4 session expired,
5 network/429/challenge, 6 selector matched nothing (page may have changed).

## Command reference

    unnes login                 Google SSO login (headed browser, persistent profile)
    unnes logout                clear the saved session
    unnes status [--json]       session state + SSO landing URL
    unnes fetch <page-id>       fetch one configured page (table/--csv/--json)
    unnes grades|schedule|announcements   aliases for fetch <id>
    unnes watch add <id> --url=... --selector=... [--render --sso-app --sso-semester
                                --pre-url --link-selector --interval --key-field]
    unnes watch rm <id>
    unnes watch list
    unnes watch run [--page-id] one pass: fetch -> diff -> changelog -> notify
    unnes watch daemon          adaptive polling loop (jitter + adaptive windows)
    unnes discover [--elena]    list gateway apps / elena courses + watch recipes

A complete starting config with grades/schedule/biodata/course recipes lives in
examples/config.example.toml (copy to ~/.config/unnes/config.toml and edit).
    unnes data list             stored datasets (one per page) + capture counts
    unnes data show <page>      latest stored records (table/--csv/--json)
    unnes data history <page>   capture timeline of distinct states
    unnes data export <page>    full history JSON (--csv: latest state)
    unnes changelog [--since=... --page-id=...]

Render/crawl pages in one watch pass share a single browser session (op=batch),
so the Elena SSO handshake runs once, not per page.

## Stored data (unnes data)

Every successful fetch/watch pass stores the page's records as a timestamped
state in data/<page-id>.jsonl (deduped: only distinct states are kept).
The biodata page captures your identity (NIM, angkatan, prodi, dosen wali,
kontak); hasil-studi captures grades per mata kuliah once your study plan
exists; elena-kursus captures each course's activities. Export with:
unnes data export <page> --csv   (or --json for the full history)

## Scraping mechanism (verified against the live portal)

The gateway embeds every app behind a short-lived JWT (sso_token) and each app
has its own session exchange:

- Akademik (app 76, akademik.unnes.ac.id): GET+POST /auth/sso_login with the
  token (plain HTTP, op=sso; auto-run on session expiry). Data pages (KRS,
  Hasil Studi) are Livewire -> render=true uses the persistent browser session.
- Elena (app 30, elena.unnes.ac.id): the gateway iframe exchange primes the
  portal session; the semester button (#btnKlik_<sem>) in the login_sso iframe
  plus #btnTest on /portal/apis/login_url/<sem> establish the Moodle session
  (automatic in render/crawl with sso_app = "30").
- Page config: render=true (browser) vs plain HTTP; sso_app = gateway app id;
  sso_semester for Elena; pre_url for semester switches; link_selector turns a
  page into a crawl (follow links, extract rows per linked page, with
  _source/_title columns). Example config lives at ~/.config/unnes/config.toml.

## Page discovery (runbook)

After unnes login: unnes status shows the SSO landing (gateway app list).
Fetch the akademik pages (krs, hasil-studi) and the Elena crawl (elena-kursus)
to learn the live URLs; add per-course assignment crawls (mod/assign links)
with link_selector once you see the activity rows. The CLI never hardcodes
URLs - every target is a config page.

## Notes

- The legacy email+password form on student.unnes.ac.id still exists, but the
  canonical login is Google SSO; form login (mode=form) is kept and tested in the
  fetcher as a fallback.
- Session expiry surfaces as exit code 4; re-run unnes login (SSO cannot be
  automated headlessly - that is intentional).
- The fetcher is located via $UNNES_FETCHER, then ./fetcher/dist/index.js, then
  <exe>/../fetcher/dist/index.js.
