# unnes-fetcher contract (v1)

The Rust CLI spawns node fetcher/dist/index.js per operation: ONE JSON job on stdin,
ONE JSON result on stdout (single line). Anything else on stdout is a bug.

## Environment

- UNNES_HOME - state root (cookie jars under profiles/); required by the CLI.
- UNNES_USER_AGENT - UA string for requests (optional, polite default).
- UNNES_PROFILE - profile name; default 'default'.

## Job (stdin)

```json
{
  "contract": 1,
  "op": "get" | "login" | "logout",
  "profile": "default",
  "baseUrl": "https://student.unnes.ac.id",
  "url": "https://...",
  "form": { "email": "", "password": "" },
  "extract": { "selecto": "tbody tr" },
  "extraRegexes": []
}
```

Result (stdout): ok/status/finalUrl/sessionExpired/challenge/retryAfter,
records (when extract supplied), normalized (html minus rotating bits).

Error shape (ok:false): { error: { code, message } } with code in
network | timeout | csrf | login | usage | contract | internal.

## Semantics

- Cookies persist to <home>/profiles/<profile>.json (0600, atomic write).
- login: GET login page, harvest _token, POST form with the cookie jar,
  detect success by final URL/status, save jar.
- get: sessionExpired when a non-login URL lands on /auth/login or 401;
  the CLI then auto re-logins once.
- challenge: heuristic for Cloudflare 403 challenge pages; CLI backs off.
- No retries/caching/JS rendering in v1 - the Rust side drives all policy.

contract is bumped on breaking changes; CLI refuses unknown versions.
