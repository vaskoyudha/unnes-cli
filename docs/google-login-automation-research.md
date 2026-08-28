# Google Login Automation — Research Findings

> Searched: 2026-08-26 via StackExchange API, GitHub issues, DuckDuckGo, and
> direct page scraping. The web_search tool was unavailable due to an invalid
> API key, so all sources were fetched with direct HTTP requests.

## Problem

Our `unnes` CLI uses Playwright's bundled Chromium (`launchPersistentContext`)
to drive Google SSO on the UNNES hub. The scripted login works most of the
time, but occasionally Google demands human interaction (password/2FA/
CAPTCHA) and we get `needsInteraction` — the "This browser or app may not be
secure" error.

## Key Finding: Playwright's Bundled Chromium Is Flagged

**Root cause confirmed across multiple sources:** Playwright's bundled
Chromium (and Puppeteer's bundled Chromium) carry automation fingerprints
that Google detects, even with `--disable-blink-features=AutomationControlled`
and `navigator.webdriver` overrides.

- **`navigator.webdriver`** — Playwright's Chromium sets this to `true` by
  default. We override it with `addInitScript`, but Google's detection
  goes beyond this single flag.
- **Browser fingerprint** — the bundled Chromium lacks the full Chrome
  profile (no extensions, no history, no sync data, no default search
  engine), which Google reads as "automated".
- **User-Agent + navigator differences** — the bundled Chromium has subtle
  differences in `navigator.plugins`, `navigator.languages`,
  `chrome.runtime`, etc.

## Solutions Found (ranked by effectiveness)

### 1. Connect to a REAL Chrome via CDP (🎯 Recommended)

The highest-voted answer [SO#79863349](https://stackoverflow.com/questions/79863349)
recommends launching a **real Google Chrome** (not Playwright's bundled
Chromium) via the command line, then connecting Playwright to it via Chrome
DevTools Protocol (CDP):

```js
const { exec } = require("child_process");
const { chromium } = require("playwright");

const cmd = `/usr/bin/google-chrome \
  --remote-debugging-port=9223 \
  --remote-debugging-address=127.0.0.1 \
  --user-data-dir="${userDataDir}" \
  --no-first-run \
  --no-default-browser-check \
  about:blank`;

exec(cmd);
// wait for Chrome to start
const browser = await chromium.connectOverCDP(
  "http://127.0.0.1:9223"
);
const [context] = browser.contexts();
const page = await context.newPage();
```

**Why this works:** Real Chrome has a complete fingerprint — extensions, sync
data, history, default search engine, real `navigator` values. Google trusts
it. The user-data-dir profile persists the session (same as our current
`launchPersistentContext`).

**Trade-off:** Requires `google-chrome` (or `google-chrome-stable`) to be
installed. Not all systems have it (but most Fedora/GNOME desktops do).

### 2. Undetected Chromedriver / Camoufox

The `undetected-chromedriver` Python package
([GitHub](https://github.com/ultrafunkamsterdam/undetected-chromedriver))
patching ChromeDriver to bypass Google's detection. Its approach includes:

- Obfuscating the ChromeDriver interface
- Using a real Chrome installation
- Patching the `navigator.webdriver` flag at the driver level

The same approach exists for Node.js indirectly via `playwright-extra` +
`puppeteer-extra-plugin-stealth`, but these are less maintained (the
stealth plugin was last updated 3+ years ago).

**Camoufox** ([SO#79863349](https://stackoverflow.com/questions/79863349))
is a Firefox-based undetectable browser mentioned as working for the same
use case.

### 3. OAuth Refresh Token / Offline Access

The "correct" API approach: instead of UI automation, obtain an OAuth2
refresh token via Google's consent flow once, then use it to get access
tokens programmatically. However:

- **The UNNES portal uses `gapi.auth2`** (legacy Google Sign-In), which
  does not expose a server-side OAuth token exchange we can use.
- The hub's `/google/auth` endpoint expects an `id_token` from the
  Google Sign-In popup — it's a client-side flow, not a server-side
  OAuth.
- **No offline refresh token** is available because the Google OAuth
  client is configured for the portal's frontend flow, not a CLI client.

### 4. FedCM (Federated Credential Management)

Modern Chromium supports FedCM, which `gapi.auth2` detects and switches to
instead of the popup. However, FedCM in automation **fails with
`NetworkError: Error retrieving a token`** (confirmed in our earlier
research: GitHub issue orca#12854, PR #14023). We already disable it with
`--disable-features=FedCm`.

### 5. What We Already Do Correctly

| Technique | Status |
|---|---|
| `--disable-blink-features=AutomationControlled` | Already applied |
| `navigator.webdriver` override via `addInitScript` | Already applied |
| `--disable-features=FedCm` | Already applied |
| Persistent user-data-dir (browser-profiles) | Already applied |
| Account-chooser click (`div.yavlK`) | Already applied |
| Consent click (`#submit_approve_access`) | Already applied |
| Scripted + headed fallback | Already applied |

## Recommended Next Step

Replace `chromium.launchPersistentContext` with:

1. Launch `google-chrome-stable` (or `google-chrome`) via subprocess with
   `--remote-debugging-port=<port>` and `--user-data-dir=<our browserDir>`.
2. Connect via `chromium.connectOverCDP("http://127.0.0.1:<port>")`.
3. Use the existing context's pages (the persistent profile is already there).

This gives us **a real Chrome browser** with a complete fingerprint —
Google trusts it, and the "This browser or app may not be secure" error
disappears completely (verified by the SO answer: the same user-data-dir that
works from CLI login works when connected via CDP, but fails when launched
by Playwright/Puppeteer).

**Location in code:** `fetcher/src/browser.ts` — `launchContext()` function
and the `browserLogin()`/`scriptedOAuth()` functions that call
`chromium.launchPersistentContext`.

## Sources

- [SO#79863349](https://stackoverflow.com/questions/79863349) — Puppeteer/Playwright can't login; CDP-to-real-Chrome fix (score 4)
- [SO#66209119](https://stackoverflow.com/questions/66209119) — "This browser or app may not be secure" (score 33, 10 answers)
- [SO#77849961](https://stackoverflow.com/questions/77849961) — Selenium undetected-chromedriver no longer works
- [Google-login-bypass (GitHub)](https://github.com/xtekky/google-login-bypass) — undetected-chromedriver approach
- [undetected-chromedriver](https://github.com/ultrafunkamsterdam/undetected-chromedriver) — Python patching driver
- [orca#12854](https://github.com/stablyai/orca/issues/12854) — FedCM bug in automation
