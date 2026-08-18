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
const SESSION_HINT_NAMES = ["laravel_session", "session", "sso_session", "unnes_session"];
const IDLE_MS = 500; // poll interval while waiting for the user
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
    console.error("Browser opened: sign in with your UNNES Google account.");
    console.error("This window auto-captures once the SSO handoff lands;");
    console.error("or press Enter here after you have signed in.");
    console.error("");

    const deadline = Date.now() + MAX_WAIT_MS;
    let done: string | null = null;

    // Wait for the handoff (URL left the hub login page, or a domain-wide
    // cookie appeared) or an Enter keypress on stdin.
    const enter = new Promise<string>((resolve) => {
      process.stdin.setEncoding("utf8");
      process.stdin.once("data", () => resolve("enter"));
    });
    while (Date.now() < deadline && done === null) {
      const poll = (async (): Promise<string | null> => {
        let url = "";
        try { url = await page.url(); } catch { return null; }
        let u: URL | null = null;
        try { u = new URL(url); } catch { return null; }
        const onUnnes = u.hostname.endsWith("unnes.ac.id");
        const leftHubRoot = onUnnes && (u.hostname !== "apps.unnes.ac.id" || u.pathname !== "/" || u.search !== "");
        const cookies = await context.cookies();
        const domainCookie = cookies.some(
          (c) => c.domain.startsWith(".") && c.domain.endsWith("unnes.ac.id"),
        );
        const sessionHint = cookies.some(
          (c) => c.domain.endsWith("unnes.ac.id") && SESSION_HINT_NAMES.includes(c.name),
        );
        if (leftHubRoot || domainCookie) return "handoff";
        if (sessionHint && !url.includes("accounts.google.com")) return "cookies";
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

    // Give the handoff a moment to finish setting cookies.
    await new Promise((r) => setTimeout(r, 1500));

    let landingUrl: string | null = null;
    try { landingUrl = await page.url(); } catch { /* page closed */ }

    const all = await context.cookies();
    const jar = CookieJar.empty();
    let captured = 0;
    for (const c of all as PlaywrightCookie[]) {
      if (!c.domain.endsWith("unnes.ac.id")) continue;
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

    const authCookies = all.filter(
      (c) => c.domain.endsWith("unnes.ac.id") && SESSION_HINT_NAMES.includes(c.name),
    );
    if (authCookies.length === 0) {
      await ctx.close();
      return fail("usage", "captured cookies but no session cookie found; did the Google sign-in complete?");
    }

    await ctx.close();
    return { contract: 1, ok: true, mode: "browser", landingUrl, capturedCookies: captured };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    try { await ctx.close(); } catch { /* already closed */ }
    return fail("internal", "browser login failed: " + message);
  }
}
