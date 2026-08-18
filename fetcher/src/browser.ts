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

// ---------------------------------------------------------------------------
// op: page - render a JS-driven page in the persistent browser session and
// extract records from the live DOM. Used for Livewire pages (akademik KRS,
// schedules) and for portals whose SSO needs the browser iframe protocol
// (Elena). The browser profile already holds the gateway session; the app's
// iframe exchange runs naturally, and afterwards every *.unnes.ac.id cookie
// is copied back into the jar so later plain-HTTP fetches also work.
// ---------------------------------------------------------------------------

export interface RenderPageOpts {
  url: string;
  /** gateway app id to prime first (e.g. "76" for akademik, "30" for elena) */
  ssoApp?: string;
  /** URL to visit before the target (e.g. the akademik semester switcher) */
  preUrl?: string;
  /** elena semester to open after SSO (default 20261, current) */
  semester?: string;
  extract?: { selector: string; fields?: Record<string, string> };
  /** max ms to wait for the extract selector; default 15000 */
  waitMs?: number;
  hubUrl?: string;
}

export interface RenderPageResult {
  contract: number;
  ok: boolean;
  op: "page";
  status?: number;
  finalUrl: string | null;
  sessionExpired: boolean;
  records: Record<string, string>[];
  capturedCookies: number;
  error?: { code: string; message: string };
  [k: string]: unknown;
}

const LOGIN_MARKERS = /login dengan unnes-id|masukan email dan password|username\s+password/i;

async function launchContext(browserDir: string): Promise<unknown> {
  const chromium = await import("playwright").then(
    (m) => (m as unknown as { chromium?: unknown; default?: { chromium?: unknown } }).chromium
      ?? (m as unknown as { default?: { chromium?: unknown } }).default?.chromium,
    () => null,
  );
  if (!chromium) throw new Error("playwright is not installed; run: cd fetcher && npm ci && npx playwright install chromium");
  try {
    mkdirSync(browserDir, { recursive: true });
    chmodSync(browserDir, 0o700);
  } catch { /* best effort */ }
  return (chromium as { launchPersistentContext(d: string, o: Record<string, unknown>): Promise<unknown> })
    .launchPersistentContext(browserDir, { headless: true });
}

/** Copy every *.unnes.ac.id cookie from the browser context into the jar. */
async function syncJarFromContext(ctx: unknown, jar: CookieJar): Promise<number> {
  const cookies = await (ctx as { cookies(): Promise<PlaywrightCookie[]> }).cookies();
  let n = 0;
  for (const c of cookies) {
    if (!c.domain.endsWith("unnes.ac.id")) continue;
    jar.addCookie({
      name: c.name,
      value: c.value,
      domain: c.domain,
      path: c.path,
      secure: c.secure,
      httpOnly: c.httpOnly,
      expires: c.expires === -1 ? null : c.expires * 1000,
    });
    n += 1;
  }
  return n;
}


/**
 * Complete the Elena (app 30) session handshake inside the persistent
 * browser. The gateway iframe exchange alone only primes elena_gateway_session;
 * the final MoodleSession is established by:
 *   1. clicking the semester button (#btnKlik_<semester>) in the login_sso iframe,
 *   2. the parent frame navigating to /portal/apis/login_url/<semester>,
 *   3. clicking #btnTest ("continue") on that page,
 * which lands on elena /my/ with an authenticated Moodle session.
 * Verified against the live portal (2026-08).
 */
async function completeElenaSession(
  page: { url(): Promise<string>; frames(): { url(): Promise<string>; click(s: string, o?: unknown): Promise<void> }[]; click(s: string, o?: unknown): Promise<void> },
  semester: string,
): Promise<void> {
  try {
    for (const f of page.frames()) {
      let u = "";
      try { u = await f.url(); } catch { continue; }
      if (u.includes("login_sso")) {
        try { await f.click("#btnKlik_" + semester, { timeout: 5000 }); } catch { /* semester button missing */ }
        break;
      }
    }
    await new Promise((r) => setTimeout(r, 5000));
    try { await page.click("#btnTest", { timeout: 5000 }); } catch { /* continue button missing */ }
    await new Promise((r) => setTimeout(r, 6000));
  } catch { /* best effort: session may already be established */ }
}

export async function renderPage(jarPath: string, browserDir: string, opts: RenderPageOpts): Promise<RenderPageResult> {
  const base = {
    contract: 1, ok: false as boolean, op: "page" as const,
    finalUrl: null as string | null, sessionExpired: false, records: [] as Record<string, string>[], capturedCookies: 0,
  };
  if (process.env.UNNES_NO_BROWSER) {
    return { ...base, error: { code: "usage", message: "page render disabled via UNNES_NO_BROWSER" } };
  }
  let ctx: unknown;
  try {
    ctx = await launchContext(browserDir);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "usage", message: message } };
  }
  const page = await (ctx as { newPage(): Promise<unknown> }).newPage();
  try {
    const hub = opts.hubUrl ?? "https://apps.unnes.ac.id";
    if (opts.ssoApp) {
      await (page as { goto(u: string, o?: unknown): Promise<unknown> }).goto(hub + "/" + opts.ssoApp, { waitUntil: "domcontentloaded", timeout: 60000 });
      // the app's iframe exchange runs client-side; give it a moment
      await new Promise((r) => setTimeout(r, 6000));
      let loginUrl = "";
      let loginBody = "";
      try {
        loginUrl = await (page as { url(): Promise<string> }).url();
        loginBody = await (page as { content(): Promise<string> }).content();
      } catch { /* closed */ }
      if (/\/(auth\/)?login/i.test(loginUrl) || LOGIN_MARKERS.test(loginBody.replace(/<script[\s\S]*?<\/script>/gi, " "))) {
        return { ...base, sessionExpired: true, error: { code: "session", message: "gateway session expired; run: unnes login" } };
      }
    }
    if (opts.ssoApp === "30") {
      await completeElenaSession(page as never, opts.semester ?? "20261");
    }
    if (opts.preUrl) {
      await (page as { goto(u: string, o?: unknown): Promise<unknown> }).goto(opts.preUrl, { waitUntil: "domcontentloaded", timeout: 60000 });
      await new Promise((r) => setTimeout(r, 2500));
    }
    await (page as { goto(u: string, o?: unknown): Promise<unknown> }).goto(opts.url, { waitUntil: "domcontentloaded", timeout: 60000 });
    if (opts.extract?.selector) {
      try {
        await (page as { waitForSelector(s: string, o?: unknown): Promise<unknown> })
          .waitForSelector(opts.extract.selector, { timeout: opts.waitMs ?? 15000 });
      } catch { /* zero matches is a valid outcome (empty records) */ }
    }
    const html = await (page as { content(): Promise<string> }).content();
    let finalUrl = "";
    try { finalUrl = await (page as { url(): Promise<string> }).url(); } catch { /* closed */ }

    const jar = await CookieJar.load(jarPath);
    const captured = await syncJarFromContext(ctx, jar);
    await jar.save(jarPath);

    // Session health: target redirected back to the gateway login, or the
    // gateway's login page (has the UNNES-ID button) is showing.
    const body = html.replace(/<script[\s\S]*?<\/script>/gi, " ").replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim().slice(0, 500);
    const landedOnGateway = finalUrl.startsWith("https://apps.unnes.ac.id");
    const sessionExpired = (landedOnGateway && LOGIN_MARKERS.test(body)) || /\/auth\/login/i.test(finalUrl);
    if (sessionExpired) {
      return { ...base, finalUrl, sessionExpired: true, capturedCookies: captured, error: { code: "session", message: "session expired; run: unnes login" } };
    }

    let records: Record<string, string>[] = [];
    if (opts.extract) {
      const { extractRecords } = await import("./extract.js");
      records = extractRecords(html, opts.extract);
    }
    return { ...base, ok: true, finalUrl, records, capturedCookies: captured };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "network", message: "page render failed: " + message } };
  } finally {
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  }
}

// ---------------------------------------------------------------------------
// op: crawl - follow links from a listing page and extract records from each
// linked page (bounded). This powers "for every mata kuliah": e.g. Elena
// /my/ -> each course page -> .activityinstance rows (assignments included).
// Records are merged with _source (the page URL) and _title (link text).
// ---------------------------------------------------------------------------

export interface CrawlOpts {
  startUrl: string;
  /** selector yielding the <a> elements to follow (same-origin *.unnes.ac.id) */
  linkSelector: string;
  /** extraction applied to each linked page */
  pageExtract: { selector: string; fields?: Record<string, string> };
  /** gateway app to prime first */
  ssoApp?: string;
  /** URL to visit before the start page (e.g. semester switcher) */
  preUrl?: string;
  /** elena semester to open after SSO (default 20261, current) */
  semester?: string;
  waitMs?: number;
  /** max links to follow; default 50 */
  maxLinks?: number;
  hubUrl?: string;
}

export interface CrawlResult {
  contract: number;
  ok: boolean;
  op: "crawl";
  finalUrl: string | null;
  sessionExpired: boolean;
  followed: number;
  records: Record<string, string>[];
  error?: { code: string; message: string };
  [k: string]: unknown;
}

export async function crawlPage(jarPath: string, browserDir: string, opts: CrawlOpts): Promise<CrawlResult> {
  const base = {
    contract: 1, ok: false as boolean, op: "crawl" as const,
    finalUrl: null as string | null, sessionExpired: false, followed: 0, records: [] as Record<string, string>[],
  };
  if (process.env.UNNES_NO_BROWSER) {
    return { ...base, error: { code: "usage", message: "crawl disabled via UNNES_NO_BROWSER" } };
  }
  let ctx: unknown;
  try {
    ctx = await launchContext(browserDir);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "usage", message } };
  }
  const page = await (ctx as { newPage(): Promise<unknown> }).newPage();
  const P = page as {
    goto(u: string, o?: unknown): Promise<unknown>;
    url(): Promise<string>;
    content(): Promise<string>;
    waitForSelector(s: string, o?: unknown): Promise<unknown>;
    evaluate(fn: string): Promise<unknown>;
  };
  const maxLinks = opts.maxLinks ?? 50;
  try {
    const hub = opts.hubUrl ?? "https://apps.unnes.ac.id";
    if (opts.ssoApp) {
      await P.goto(hub + "/" + opts.ssoApp, { waitUntil: "domcontentloaded", timeout: 60000 });
      await new Promise((r) => setTimeout(r, 6000));
      let u = "";
      let uBody = "";
      try {
        u = await P.url();
        uBody = await P.content();
      } catch { /* closed */ }
      if (/\/(auth\/)?login/i.test(u) || LOGIN_MARKERS.test(uBody.replace(/<script[\s\S]*?<\/script>/gi, " "))) {
        return { ...base, sessionExpired: true, error: { code: "session", message: "gateway session expired; run: unnes login" } };
      }
    }

    if (opts.ssoApp === "30") {
      await completeElenaSession(page as never, opts.semester ?? "20261");
    }
    if (opts.preUrl) {
      await P.goto(opts.preUrl, { waitUntil: "domcontentloaded", timeout: 60000 });
      await new Promise((r) => setTimeout(r, 2500));
    }
    await P.goto(opts.startUrl, { waitUntil: "domcontentloaded", timeout: 60000 });
    {
      const st = await P.content();
      if (LOGIN_MARKERS.test(st.replace(/<script[\s\S]*?<\/script>/gi, " ")) && /apps\.unnes\.ac\.id/.test(await P.url())) {
        return { ...base, sessionExpired: true, error: { code: "session", message: "session expired; run: unnes login" } };
      }
    }
    try {
      await P.waitForSelector(opts.linkSelector, { timeout: opts.waitMs ?? 15000 });
    } catch {
      return { ...base, finalUrl: await safeUrl(P), records: [], error: { code: "usage", message: "no links matched selector '" + opts.linkSelector + "' on " + opts.startUrl } };
    }
    // Collect absolute same-origin links.
    const links = (await P.evaluate(
      "(function(){var out=[];var seen={};document.querySelectorAll(SEL).forEach(function(a){var h=a.href||a.getAttribute('href');if(!h)return;var u=new URL(h,location.href);if(!/unnes\\.ac\\.id$/.test(u.hostname)&&u.hostname!=='elena.unnes.ac.id'&&u.hostname!=='akademik.unnes.ac.id'&&u.hostname!=='student.unnes.ac.id')return;if(seen[u.href])return;seen[u.href]=1;out.push({href:u.href,text:(a.innerText||a.textContent||'').replace(/\\s+/g,' ').trim().slice(0,120)});});return out;})()"
        .replace("SEL", JSON.stringify(opts.linkSelector)),
    )) as { href: string; text: string }[];

    const { extractRecords } = await import("./extract.js");
    const records: Record<string, string>[] = [];
    const waitMs = opts.waitMs ?? 15000;
    for (const link of links.slice(0, maxLinks)) {
      try {
        await P.goto(link.href, { waitUntil: "domcontentloaded", timeout: 60000 });
      } catch { continue; }
      try {
        await P.waitForSelector(opts.pageExtract.selector, { timeout: waitMs });
      } catch { continue; } // page without matches: skip
      const html = await P.content();
      const recs = extractRecords(html, opts.pageExtract);
      for (const r of recs) {
        records.push({ ...r, _source: link.href, _title: link.text });
      }
    }
    const jar = await CookieJar.load(jarPath);
    await syncJarFromContext(ctx, jar);
    await jar.save(jarPath);
    return { ...base, ok: true, finalUrl: await safeUrl(P), followed: Math.min(links.length, maxLinks), records };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "network", message: "crawl failed: " + message } };
  } finally {
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  }
}

async function safeUrl(P: { url(): Promise<string> }): Promise<string | null> {
  try { return await P.url(); } catch { return null; }
}


// ---------------------------------------------------------------------------
// op: batch - render many pages in ONE persistent browser session.
// SSO primes are deduplicated per gateway app (one elena handshake serves all
// its pages), which is what makes watch passes fast. The jar is synced once.
// ---------------------------------------------------------------------------

export interface BatchEntry {
  url: string;
  ssoApp?: string;
  preUrl?: string;
  semester?: string;
  extract?: { selector: string; fields?: Record<string, string> };
  /** crawl mode: follow these links from the page (extract applied per page) */
  linkSelector?: string;
  maxLinks?: number;
  waitMs?: number;
}

export interface BatchPageResult {
  url: string;
  ok: boolean;
  finalUrl: string | null;
  sessionExpired: boolean;
  records: Record<string, string>[];
  error?: { code: string; message: string };
}

export interface BatchResult {
  contract: number;
  ok: boolean;
  op: "batch";
  results: BatchPageResult[];
  capturedCookies: number;
  error?: { code: string; message: string };
  [k: string]: unknown;
}


/** Crawl-mode batch entry: collect links from the current page and extract
 * per linked page, merged with _source/_title (shared session). */
async function batchCrawlLinks(
  P: { goto(u: string, o?: unknown): Promise<unknown>; url(): Promise<string>; content(): Promise<string>; waitForSelector(s: string, o?: unknown): Promise<unknown>; evaluate(fn: string): Promise<unknown> },
  entry: BatchEntry,
  r: BatchPageResult,
): Promise<void> {
  const { extractRecords } = await import("./extract.js");
  try {
    await P.waitForSelector(entry.linkSelector!, { timeout: entry.waitMs ?? 15000 });
  } catch {
    r.error = { code: "usage", message: "no links matched selector '" + entry.linkSelector + "' on " + entry.url };
    return;
  }
  const links = (await P.evaluate(
    "(function(){var out=[];var seen={};document.querySelectorAll(SEL).forEach(function(a){var h=a.href||a.getAttribute('href');if(!h)return;var u=new URL(h,location.href);if(!/unnes\.ac\.id$/.test(u.hostname))return;if(seen[u.href])return;seen[u.href]=1;out.push({href:u.href,text:(a.innerText||a.textContent||'').replace(/\s+/g,' ').trim().slice(0,120)});});return out;})()"
      .replace("SEL", JSON.stringify(entry.linkSelector)),
  )) as { href: string; text: string }[];
  const maxLinks = entry.maxLinks ?? 50;
  const records: Record<string, string>[] = [];
  for (const link of links.slice(0, maxLinks)) {
    try {
      await P.goto(link.href, { waitUntil: "domcontentloaded", timeout: 60000 });
    } catch { continue; }
    if (entry.extract?.selector) {
      try { await P.waitForSelector(entry.extract.selector, { timeout: entry.waitMs ?? 15000 }); } catch { continue; }
    }
    const html = await P.content();
    const recs = extractRecords(html, entry.extract ?? { selector: "body" });
    for (const rec of recs) {
      records.push({ ...rec, _source: link.href, _title: link.text });
    }
  }
  r.finalUrl = await P.url();
  r.records = records;
}
export async function batchPages(
  jarPath: string,
  browserDir: string,
  entries: BatchEntry[],
  hubUrl: string = "https://apps.unnes.ac.id",
): Promise<BatchResult> {
  const fail = (code: string, message: string): BatchResult => ({
    contract: 1, ok: false, op: "batch", results: [], capturedCookies: 0, error: { code, message },
  });
  if (process.env.UNNES_NO_BROWSER) {
    return fail("usage", "batch render disabled via UNNES_NO_BROWSER");
  }
  let ctx: unknown;
  try {
    ctx = await launchContext(browserDir);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return fail("usage", message);
  }
  const page = await (ctx as { newPage(): Promise<unknown> }).newPage();
  const P = page as {
    goto(u: string, o?: unknown): Promise<unknown>;
    url(): Promise<string>;
    content(): Promise<string>;
    waitForSelector(s: string, o?: unknown): Promise<unknown>;
    evaluate(fn: string): Promise<unknown>;
  };
  const primed = new Set<string>();
  const results: BatchPageResult[] = [];
  try {
    for (const entry of entries) {
      const r: BatchPageResult = { url: entry.url, ok: false, finalUrl: null, sessionExpired: false, records: [] };
      try {
        // 1. prime the app session once per ssoApp
        if (entry.ssoApp && !primed.has(entry.ssoApp)) {
          await P.goto(hubUrl + "/" + entry.ssoApp, { waitUntil: "domcontentloaded", timeout: 60000 });
          await new Promise((r) => setTimeout(r, 6000));
          let primeUrl = "";
          try { primeUrl = await P.url(); } catch { /* closed */ }
          if (/\/(auth\/)?login/i.test(primeUrl)) {
            r.error = { code: "session", message: "gateway session expired; run: unnes login" };
            r.sessionExpired = true;
            results.push(r);
            primed.add(entry.ssoApp);
            continue;
          }
          if (entry.ssoApp === "30") {
            await completeElenaSession(page as never, entry.semester ?? "20261");
          }
          primed.add(entry.ssoApp);
        }
        if (entry.preUrl) {
          await P.goto(entry.preUrl, { waitUntil: "domcontentloaded", timeout: 60000 });
          await new Promise((r) => setTimeout(r, 2500));
        }
        // 2. the page itself (crawl mode: follow links, extract per page)
        await P.goto(entry.url, { waitUntil: "domcontentloaded", timeout: 60000 });
        if (entry.linkSelector) {
          await batchCrawlLinks(P, entry, r);
        } else {
          if (entry.extract?.selector) {
            try {
              await P.waitForSelector(entry.extract.selector, { timeout: entry.waitMs ?? 15000 });
            } catch { /* zero matches is valid */ }
          }
          const html = await P.content();
          r.finalUrl = await P.url();
          // 3. session health
          const body = html.replace(/<script[\s\S]*?<\/script>/gi, " ").replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim().slice(0, 500);
          const landedOnGateway = (r.finalUrl ?? "").startsWith("https://apps.unnes.ac.id");
          r.sessionExpired = (landedOnGateway && LOGIN_MARKERS.test(body)) || /\/auth\/login/i.test(r.finalUrl ?? "");
          if (r.sessionExpired) {
            results.push(r);
            continue;
          }
          if (entry.extract) {
            const { extractRecords } = await import("./extract.js");
            r.records = extractRecords(html, entry.extract);
          }
        }
        r.ok = true;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        r.error = { code: "network", message: message.slice(0, 200) };
      }
      results.push(r);
    }
    const jar = await CookieJar.load(jarPath);
    const captured = await syncJarFromContext(ctx, jar);
    await jar.save(jarPath);
    return { contract: 1, ok: true, op: "batch", results, capturedCookies: captured };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return fail("internal", "batch render failed: " + message);
  } finally {
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  }
}
