// Browser-assisted login for Google SSO (apps.unnes.ac.id).
//
// Plain HTTP cannot complete Google OAuth (interactive consent/2FA), so
// login runs a real (headed) Chromium window, lets the user sign in,
// captures every *.unnes.ac.id cookie into the profile jar, and closes.
// Everything after that (fetch/watch) stays plain HTTP and headless.
//
// The Chromium profile is PERSISTENT (launchPersistentContext into
// <home>/browser-profiles/<profile>): Google's sign-in state (account
// choice, 2FA trust) survives between logins, so re-logins after session
// expiry are one click. The portal itself calls auth2.disconnect() after
// every login, so a full re-auth always happens - the profile just makes
// it painless. Security note: this profile stores Google session data on
// disk (0700); it lives under UNNES_HOME and is never pushed to git.
//
// Playwright is imported dynamically so the module loads without it; the
// browser binary itself is installed separately (npx playwright install
// chromium) and never needed for non-login operations.

import { chmodSync, mkdirSync } from "node:fs";
import { CookieJar } from "./cookiejar.js";

export interface BrowserLoginResult {
  contract: number;
  ok: boolean;
  mode: "browser";
  status?: number;
  landingUrl: string | null;
  capturedCookies: number;
  error?: { code: string; message: string };
  [k: string]: unknown;
}

const HUB_URL = "https://apps.unnes.ac.id/";
// Cookies apps.unnes.ac.id sets for every visitor, before any sign-in:
// XSRF-TOKEN + laravel_session (guest session, host-only) and
// G_ENABLED_IDPS (Google platform, domain-wide). They are NOT proof of login.
const GUEST_COOKIES = new Set(["XSRF-TOKEN", "laravel_session", "G_ENABLED_IDPS"]);
const IDLE_MS = 500; // poll interval while waiting for the user
const GRACE_MS = 3000; // ignore auto-triggers during the first seconds
const MAX_WAIT_MS = 10 * 60 * 1000; // give up after 10 minutes

interface PlaywrightCookie {
  name: string;
  value: string;
  domain: string;
  path: string;
  expires: number; // -1 = session cookie
  httpOnly: boolean;
  secure: boolean;
  sameSite: "Strict" | "Lax" | "None";
}

export async function browserLogin(jarPath: string, browserDir: string, hubUrl: string = HUB_URL): Promise<BrowserLoginResult> {
  const fail = (code: string, message: string): BrowserLoginResult => ({
    contract: 1, ok: false, mode: "browser", landingUrl: null, capturedCookies: 0,
    error: { code, message },
  });

  // Test/CI escape hatch: never open a browser.
  if (process.env.UNNES_NO_BROWSER) {
    return fail("usage", "browser login disabled via UNNES_NO_BROWSER");
  }

  // playwright is a CJS package; normalize the interop shape defensively.
  type ChromiumLike = {
    launch: (opts: Record<string, unknown>) => Promise<unknown>;
    launchPersistentContext: (dir: string, opts: Record<string, unknown>) => Promise<unknown>;
  };
  let chromium: ChromiumLike | null = null;
  try {
    const mod = (await import("playwright")) as unknown as {
      chromium?: ChromiumLike;
      default?: { chromium?: ChromiumLike };
    };
    chromium = mod.chromium ?? mod.default?.chromium ?? null;
  } catch {
    chromium = null;
  }
  if (!chromium) {
    return fail("usage", "playwright is not installed; run: cd fetcher && npm ci && npx playwright install chromium");
  }

  interface PageLike {
    url(): Promise<string>;
    goto(u: string, o?: unknown): Promise<unknown>;
    on(ev: string, cb: () => void): void;
    isClosed(): boolean;
  }
  // Ensure the persistent profile dir exists with owner-only permissions.
  try {
    mkdirSync(browserDir, { recursive: true });
    chmodSync(browserDir, 0o700);
  } catch { /* best effort */ }

  let launched: unknown = null;
  try {
    launched = await chromium.launchPersistentContext(browserDir, { headless: false });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return fail("usage", "could not launch Chromium with persistent profile: " + message + " (is another unnes login running? delete " + browserDir + " to force a fresh profile)");
  }
  const ctx = launched as {
    pages(): PageLike[];
    newPage(): Promise<PageLike>;
    cookies(): Promise<PlaywrightCookie[]>;
    on(ev: "page", cb: (p: PageLike) => void): void;
    on(ev: "close", cb: () => void): void;
    close(): Promise<void>;
  };

  try {
    // Reuse an existing page if the persistent profile restored one.
    const initial = ctx.pages();
    const page = initial.length > 0 ? initial[0] : await ctx.newPage();

    // Track EVERY tab/popup: the hub's 'Login dengan UNNES-ID' button calls
    // gAuth2.signIn() which opens the Google account chooser in a POPUP, and
    // the handoff may land in any tab. We watch them all. A popup closing
    // after consent is NORMAL - only abort when every tab is gone (window
    // closed) or the browser process exits.
    const pages: PageLike[] = [...initial];
    let browserGone = false;
    ctx.on("page", (p) => {
      pages.push(p);
      p.on("close", () => {
        const i = pages.indexOf(p);
        if (i >= 0) pages.splice(i, 1);
      });
    });
    ctx.on("close", () => { browserGone = true; });

    if (!page.isClosed()) {
      await page.goto(hubUrl, { waitUntil: "domcontentloaded", timeout: 60000 });
    }

    // Instructions go to stderr: stdout is reserved for the single JSON result.
    console.error("");
    console.error("Opening " + hubUrl + " in your browser...");
    console.error("Sign in with your UNNES Google account (click 'Login dengan");
    console.error("UNNES-ID'). Google opens a separate sign-in window - complete");
    console.error("it there. Capture happens automatically after the SSO redirect,");
    console.error("or press Enter here. Close the window to abort.");
    console.error("");

    const deadline = Date.now() + MAX_WAIT_MS;
    const started = Date.now();
    let done: string | null = null;

    // Enter on stdin = the human says "I am logged in".
    const enter = new Promise<string>((resolve) => {
      process.stdin.setEncoding("utf8");
      process.stdin.once("data", () => resolve("enter"));
    });

    // Auto-detect, across all tabs. Verified against the live hub (2026-08):
    // the 'Login dengan UNNES-ID' button calls gAuth2.signIn() (Google popup ->
    // accounts.google.com, which closes after consent), then onSignIn() POSTs
    // to /google/auth and the main tab navigates to the server-issued route
    // (window.location.href = obj.route). That route navigation is the handoff:
    //  - the login page never navigates on its own, so ANY path/query change
    //    on the hub host means the route redirect happened, and
    //  - a tab on another *.unnes.ac.id subdomain is the route landing too.
    // The popup closing after consent is NORMAL and must NOT abort - only
    // abort when every tab is gone or the browser process exits.
    while (Date.now() < deadline && done === null) {
      const poll = (async (): Promise<string | null> => {
        if (Date.now() - started < GRACE_MS) return null;
        if (browserGone || pages.length === 0) return "closed";
        for (const p of [...pages]) {
          if (p.isClosed()) continue;
          let url = "";
          try { url = await p.url(); } catch { continue; } // closed mid-loop
          let u: URL | null = null;
          try { u = new URL(url); } catch { continue; }
          if (u.hostname.endsWith("unnes.ac.id")) {
            const leftHub = u.hostname !== "apps.unnes.ac.id";
            const changed = u.pathname !== "/" || u.search !== "";
            if (leftHub || changed) return "handoff";
          }
        }
        return null;
      })();
      const winner = await Promise.race([poll, enter]);
      if (winner) done = winner;
      else await new Promise((r) => setTimeout(r, IDLE_MS));
    }

    if (done === null) {
      await ctx.close();
      return fail("usage", "timed out waiting for login; no session captured");
    }
    if (done === "closed") {
      await ctx.close();
      return fail("usage", "browser window was closed; login aborted, no session saved");
    }

    // Give the handoff a moment to finish setting cookies.
    await new Promise((r) => setTimeout(r, 1500));

    let landingUrl: string | null = null;
    for (const p of [...pages]) {
      if (p.isClosed()) continue;
      try {
        const u = new URL(await p.url());
        if (u.hostname.endsWith("unnes.ac.id")) { landingUrl = u.href; break; }
      } catch { /* ignore */ }
    }
    if (landingUrl === null) {
      try { landingUrl = await page.url(); } catch { /* page closed */ }
    }

    const all = await ctx.cookies();
    const jar = CookieJar.empty();
    let captured = 0;
    const names = new Set<string>();
    for (const c of all as PlaywrightCookie[]) {
      if (!c.domain.endsWith("unnes.ac.id")) continue;
      names.add(c.name);
      jar.addCookie({
        name: c.name,
        value: c.value,
        domain: c.domain,
        path: c.path,
        secure: c.secure,
        httpOnly: c.httpOnly,
        expires: c.expires === -1 ? null : c.expires * 1000, // s -> ms
      });
      captured += 1;
    }
    await jar.save(jarPath);

    if (captured === 0) {
      await ctx.close();
      return fail("usage", "captured no unnes.ac.id cookies; did the Google sign-in complete?");
    }

    // Auto-triggers only fire on real handoff shapes (hub redirect after a
    // Google visit, or a tab on another unnes.ac.id subdomain), so no extra
    // session-cookie proof is required - the jar may legitimately contain
    // only the Laravel session cookie (the hub disconnects Google itself via
    // auth2.disconnect(), which is why login always asks again).
    console.error("captured " + captured + " unnes.ac.id cookies: " + [...names].sort().join(", "));
    await ctx.close();
    return { contract: 1, ok: true, mode: "browser", landingUrl, capturedCookies: captured };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    try { await ctx.close(); } catch { /* already closed */ }
    return fail("internal", "browser login failed: " + message);
  }
}
