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
  "op": "get" | "login" | "logout",
  "profile": "default",
  "baseUrl": "https://student.unnes.ac.id",
  "url": "https://...",
  "mode": "form" | "browser",
  "form": { "email": "", "password": "" },
  "extract": { "selecto": "tbody tr" },
  "extraRegexes": []
}
```

Result (stdout): ok/status/finalUrl/sessionExpired/challenge/retryAfter,
records (when extract supplied), normalized (html minus rotating bits).
Browser login additionally returns mode/landingUrl/capturedCookies.

Error shape (ok:false): { error: { code, message } } with code in
network | timeout | csrf | login | usage | contract | internal.

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
  Requires npx playwright install chromium once. Never runs headless in cron
  paths - only during interactive unnes login.
- get: sessionExpired when a non-login URL lands on /auth/login or 401;
  the CLI then tells the user to re-run login (SSO cannot auto re-login).
- challenge: heuristic for Cloudflare 403 challenge pages; CLI backs off.
- No retries/caching/JS rendering for plain HTTP ops - the Rust side drives
  all policy.

contract is bumped on breaking changes; CLI refuses unknown versions.
