# unnes-fetcher contract (v1)

The Rust CLI spawns node fetcher/dist/index.js per operation: ONE JSON job on stdin,
ONE JSON result on stdout (single line). Anything else on stdout is a bug.

## Environment

- UNNES_HOME - state root (cookie jars under profiles/, persistent Chromium
  profiles under browser-profiles/); required by the CLI.
- UNNES_USER_AGENT - UA string for requests (optional, polite default).
- UNNES_PROFILE - profile name; default 'default'.
- UNNES_NO_BROWSER - set to 1 to make op=login mode=browser fail fast
  (tests/CI; never opens a browser).

## Job (stdin)

```json
{
  "contract": 1,
  "op": "get" | "login" | "logout" | "sso" | "page" | "crawl" | "batch" | "submit" | "open" | "download" | "batchget",
  "profile": "default",
  "baseUrl": "https://student.unnes.ac.id",
  "url": "https://...",
  "mode": "form" | "browser",
  "form": { "email": "", "password": "" },
  "extract": { "selecto": "tbody tr" },
  "extraRegexes": [],
  "out": "<dir for op=download>"
}
```

Result (stdout): ok/status/finalUrl/sessionExpired/challenge/retryAfter,
records (when extract supplied), normalized (html minus rotating bits).
Browser login additionally returns mode/landingUrl/capturedCookies/
googlePersistent (persistent SID-family cookies kept; 0 = the account
chooser will be empty on the next login - sign into the accounts.google.com
tab with 'Stay signed in' before clicking Login dengan UNNES-ID).
op=submit additionally returns submitted/draft/hasFile verification flags
read from the real page state (all false = upload unverified, not a proven
success) and stage-tagged errors (submit failed at stage '<prime|open|
attach|finalize|verify>'). The CLI retries transient failures (network/
timeout, profile lock) with backoff up to 3 attempts and diagnoses
deterministic ones (closed form, bad cmid) without retrying.

Error shape (ok:false): { error: { code, message } } with code in
network | timeout | ratelimit | csrf | login | usage | contract | internal.
`ratelimit` = HTTP 429 survived one Retry-After-honoring retry; CLI exits 5.

## Semantics

- Cookies persist to <home>/profiles/<profile>.json (0600, atomic write).
- login mode=form (default): GET login page, harvest _token, POST form with
  the cookie jar, detect success by final URL/status, save jar. The UNNES
  student portal keeps this legacy path, but the canonical login is SSO.
- login mode=browser: Google SSO via a headed Chromium window (Playwright).
  The browser ALWAYS opens the SSO hub https://apps.unnes.ac.id/ - job.baseUrl
  is the data portal and is deliberately ignored for browser login (it has no
  Google sign-in). The user signs in interactively; all *.unnes.ac.id cookies
  are captured into the jar and the SSO landing URL is reported. Auto-capture
  fires on the SSO route navigation (any path/query change on the hub host, or
  a tab on another *.unnes.ac.id subdomain) or on an explicit Enter.
- The Chromium profile is PERSISTENT in <home>/browser-profiles/<profile>
  (0700): Google's sign-in state (account choice, 2FA trust) survives between
  logins, so re-logins after session expiry are one click. The hub calls
  auth2.disconnect() after every login, so a full Google re-auth always
  happens - the profile just makes it painless. This profile stores Google
  session data on disk; it is never uploaded or committed to git.
  Ctrl+C / closed terminal / SIGTERM mid-login runs the graceful shutdown
  first (flush profile, reap Chrome) instead of tearing the cookie store;
  a pre-login Cookies backup (Cookies.unnes-bak) plus pre/post persistence
  comparison diagnoses lost-vs-never-planted sessions in the login output.
  Requires npx playwright install chromium once. Never runs headless in cron
  paths - only during interactive unnes login.
- get: sessionExpired when a non-login URL lands on /auth/login or 401;
  the CLI then tells the user to re-run login (SSO cannot auto re-login).
- challenge: heuristic for Cloudflare 403 challenge pages; CLI backs off.
- op=sso: exchange the gateway sso_token for an app session (app 76/64:
  GET+POST auth/sso_login; app 30: gateway iframe exchange + semester choice).
  op=get auto-runs it once on session expiry for known app hosts.
- op=batchget: N plain-HTTP GETs in ONE spawn sharing one jar and one
  per-host politeness limiter (job: `urls: [{url, extract?, extraRegexes?}]`,
  `concurrency` accepted but sequential until the pool lands). One jar save
  at the end (skipped only when every entry expired); per-entry SSO
  bootstrap with jar reload before retry. Top-level `ok` means the op ran;
  per-URL success lives on `results[]`; top `sessionExpired` is true when
  ANY entry expired (callers reuse the prime-then-retry path).
- op=page: render a JS-driven page (Livewire) in the persistent browser session
  and extract records; op=crawl: follow link_selector from a start page and
  extract pageExtract rows per linked page (adds _source/_title). Both sync
  every *.unnes.ac.id cookie back into the jar.
- No retries/caching/JS rendering for plain HTTP ops - the Rust side drives
  all policy.
- op=download: binary GET with the jar session (manual redirects, one
  pluginfile follow for confirmation pages) saved into `out/` with the
  server filename; returns path/bytes/filename. Powers `unnes materi`.
  Inline access-denied pages map to sessionExpired (like login bounces).
- op=crawl returns `skipped` (links dropped on goto/selector failure) so a
  partial crawl never looks complete; failed SSO refreshes attach
  `ssoRefreshError` instead of failing silently.

contract is bumped on breaking changes; CLI refuses unknown versions.
