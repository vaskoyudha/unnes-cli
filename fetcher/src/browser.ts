// Browser-assisted login for Google SSO (apps.unnes.ac.id).
//
// Plain HTTP cannot complete Google OAuth (interactive consent/2FA), so
// login runs a real (headed) Chromium window once, lets the user sign in,
// captures every *.unnes.ac.id cookie into the profile jar, and closes.
// Everything after that (fetch/watch) stays plain HTTP and headless.
//
// Playwright is imported dynamically so the module loads without it; the
// browser binary itself is installed separately (npx playwright install
// chromium) and never needed for non-login operations.

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

export async function browserLogin(jarPath: string, hubUrl: string = HUB_URL): Promise<BrowserLoginResult> {
  const fail = (code: string, message: string): BrowserLoginResult => ({
    contract: 1, ok: false, mode: "browser", landingUrl: null, capturedCookies: 0,
    error: { code, message },
  });

  // Test/CI escape hatch: never open a browser.
  if (process.env.UNNES_NO_BROWSER) {
    return fail("usage", "browser login disabled via UNNES_NO_BROWSER");
  }

  // playwright is a CJS package; normalize the interop shape defensively.
  let chromium: { launch: (opts: Record<string, unknown>) => Promise<unknown> } | null = null;
  try {
    const mod = (await import("playwright")) as unknown as {
      chromium?: { launch: (opts: Record<string, unknown>) => Promise<unknown> };
      default?: { chromium?: { launch: (opts: Record<string, unknown>) => Promise<unknown> } };
    };
    chromium = mod.chromium ?? mod.default?.chromium ?? null;
  } catch {
    chromium = null;
  }
  if (!chromium) {
    return fail("usage", "playwright is not installed; run: cd fetcher && npm ci && npx playwright install chromium");
  }

  let browser: unknown;
  try {
    browser = await chromium.launch({ headless: false });
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return fail("usage", "could not launch Chromium: " + message + " (run: npx playwright install chromium)");
  }

  const ctx = browser as {
    newContext(): Promise<{
      newPage(): Promise<{ url(): Promise<string>; goto(u: string, o?: unknown): Promise<unknown> }>;
      cookies(): Promise<PlaywrightCookie[]>;
    }>;
    close(): Promise<void>;
  };

  try {
    const context = await ctx.newContext();
    const page = await context.newPage();
    await page.goto(hubUrl, { waitUntil: "domcontentloaded", timeout: 60000 });

    // Instructions go to stderr: stdout is reserved for the single JSON result.
    console.error("");
    console.error("Opening " + hubUrl + " in your browser...");
    console.error("Sign in with your UNNES Google account. It auto-captures once");
    console.error("the SSO lands you on a UNNES app page, or press Enter here");
    console.error("after you have signed in. Close the window to abort.");
    console.error("");

    const deadline = Date.now() + MAX_WAIT_MS;
    const started = Date.now();
    let done: string | null = null;

    // Enter on stdin = the human says "I am logged in".
    const enter = new Promise<string>((resolve) => {
      process.stdin.setEncoding("utf8");
      process.stdin.once("data", () => resolve("enter"));
    });

    // Auto-detect: the browser left the hub host for another *.unnes.ac.id
    // subdomain (the SSO handoff). Guest cookies alone (laravel_session,
    // XSRF-TOKEN, G_ENABLED_IDPS) are deliberately NOT a trigger - they
    // exist before login, and a name-based check cannot tell guest from
    // authenticated sessions.
    while (Date.now() < deadline && done === null) {
      const poll = (async (): Promise<string | null> => {
        if (Date.now() - started < GRACE_MS) return null;
        let url = "";
        try { url = await page.url(); } catch { return "closed"; } // window closed by the user
        let u: URL | null = null;
        try { u = new URL(url); } catch { return null; }
        if (u.hostname.endsWith("unnes.ac.id") && u.hostname !== "apps.unnes.ac.id") {
          return "handoff";
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
    try { landingUrl = await page.url(); } catch { /* page closed */ }

    const all = await context.cookies();
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

    // When auto-triggered, require some evidence of a real session: a cookie
    // beyond the guest trio, or a landing page outside the hub.
    if (done === "handoff") {
      const hasNonGuest = [...names].some((n) => !GUEST_COOKIES.has(n));
      let host = "";
      try { host = new URL(landingUrl ?? "").hostname; } catch { /* ignore */ }
      const leftHub = host !== "" && host !== "apps.unnes.ac.id";
      if (!hasNonGuest && !leftHub) {
        await ctx.close();
        return fail("usage", "auto-capture fired but only guest cookies present (" + [...names].join(", ") + "). Sign in on the Google page, then press Enter here.");
      }
    }

    console.error("captured " + captured + " unnes.ac.id cookies: " + [...names].sort().join(", "));
    await ctx.close();
    return { contract: 1, ok: true, mode: "browser", landingUrl, capturedCookies: captured };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    try { await ctx.close(); } catch { /* already closed */ }
    return fail("internal", "browser login failed: " + message);
  }
}
