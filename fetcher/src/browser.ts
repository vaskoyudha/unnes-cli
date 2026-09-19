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

import { spawn, type ChildProcess } from "node:child_process";
import { chmodSync, mkdirSync, existsSync, lstatSync, readlinkSync, unlinkSync, copyFileSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { CookieJar } from "./cookiejar.js";

/** Best-effort pre-login backup of the Chromium cookie store, so a
 * killed/crashed login run (torn SQLite WAL) can be recovered by hand.
 * Returns the backup path, or null when there was nothing to back up. */
function backupCookies(browserDir: string): string | null {
  try {
    const src = join(browserDir, "Default", "Cookies");
    if (!existsSync(src)) return null;
    const dst = join(browserDir, "Default", "Cookies.unnes-bak");
    copyFileSync(src, dst);
    return dst;
  } catch {
    return null;
  }
}

function cleanStaleSingleton(browserDir: string): void {
  try {
    const lockPath = join(browserDir, "SingletonLock");
    let isDead = false;
    try {
      const stat = lstatSync(lockPath);
      if (stat.isSymbolicLink()) {
        const target = readlinkSync(lockPath);
        const match = target.match(/-(\d+)$/);
        if (match) {
          const pid = parseInt(match[1], 10);
          let denied = false;
          try {
            process.kill(pid, 0);
          } catch (e) {
            // EPERM = a LIVE process we may not signal (different uid /
            // hidepid): hands off entirely, never unlink a live browser's
            // lock. Anything else (ESRCH...) = dead.
            if ((e as NodeJS.ErrnoException)?.code === "EPERM") denied = true;
            else isDead = true;
          }
          if (!denied && !isDead) {
            // Alive own-uid holder. Reap ONLY when it is provably our own
            // orphan AND no sibling unnes/fetcher process exists that could
            // own it: murdering a concurrent run's browser mid-login was a
            // real session killer (never adopt-or-kill a live sibling).
            if (isOwnChromeProcess(pid, browserDir) && !hasLiveUnnesPeer()) {
              reapOwnOrphan(browserDir, pid);
              isDead = true;
            }
          }
        } else {
          isDead = true;
        }
      }
    } catch {
      // lockPath doesn't exist
    }
    if (isDead) {
      try { unlinkSync(lockPath); } catch {}
      try { unlinkSync(join(browserDir, "SingletonCookie")); } catch {}
      try { unlinkSync(join(browserDir, "SingletonSocket")); } catch {}
    }
  } catch { /* best effort */ }
}

/** True when another unnes CLI / fetcher process is alive besides this
 * process tree: a live lock holder may be ITS browser, so reaping is
 * forbidden - wait it out instead. Linux /proc scan, best effort
 * (fail-open towards caution: unreadable => assume a peer exists). */
function hasLiveUnnesPeer(): boolean {
  try {
    const me = new Set<number>([process.pid]);
    try {
      let ppid: number = process.ppid || 1;
      for (let i = 0; i < 16 && ppid > 1; i++) {
        me.add(ppid);
        const parts: string[] = readFileSync(`/proc/${ppid}/stat`, "utf8").split(" ");
        ppid = Number(parts[3]) || 1;
      }
    } catch { /* best effort */ }
    // Match by program identity, not by repo path: an editor with the repo
    // open must NOT count as a peer (that would permanently disable reaping).
    // A peer is a node process running the fetcher, or an `unnes` binary.
    const mine = ["dist/index.js"];
    for (const entry of readdirSync("/proc")) {
      if (!/^\d+$/.test(entry)) continue;
      const pid = Number(entry);
      if (me.has(pid)) continue;
      let cmd: string;
      try {
        cmd = readFileSync(`/proc/${pid}/cmdline`, "utf8").replace(/\0/g, " ");
      } catch {
        continue;
      }
      const argv0 = cmd.split(" ")[0];
      const base = argv0.split("/").pop() ?? "";
      if (base === "unnes") return true;
      if (mine.some((m) => cmd.includes(m))) {
        // Our own fetcher child could theoretically match: exclude direct
        // children of this process (spawned by us, e.g. none - each op is one
        // node process, but be strict anyway via ppid check below).
        try {
          const ppid = Number(readFileSync(`/proc/${pid}/stat`, "utf8").split(" ")[3]);
          if (ppid === process.pid) continue;
        } catch { /* fall through: count it */ }
        return true;
      }
    }
    return false;
  } catch {
    return true;
  }
}

function findChromeBinary(): string | null {
  if (process.env.CHROME_BIN && existsSync(process.env.CHROME_BIN)) return process.env.CHROME_BIN;
  // $HOME-based first (portable across machines/users), then system paths.
  const home = process.env.HOME ?? "";
  const candidates = [
    home ? home + "/.local/bin/google-chrome" : "",
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
  ];
  for (const c of candidates) {
    if (c && existsSync(c)) return c;
  }
  return null;
}

interface CDPChromeInstance {
  browser: { close: () => Promise<void>; contexts: () => unknown[] };
  ctx: unknown;
  proc: ChildProcess;
}

function chromeLabel(): string {
  // One line on stderr per launch so a future "accounts disappeared" report
  // can rule out a binary flip (real Chrome vs bundled Chromium use different
  // keyring keys, so a flip makes the stored Google cookies unreadable).
  return findChromeBinary() ?? "(playwright bundled chromium)";
}

/** Wait for a spawned Chrome to exit after graceful close; SIGTERM it if it
 * lingers (SIGTERM still shuts Chrome down gracefully with cookies intact),
 * SIGKILL only as the last resort. Resolves true when the process is gone. */
async function awaitChromeExit(proc: ChildProcess, gracefulWaitMs = 5000): Promise<boolean> {
  const exited = await new Promise<boolean>((resolve) => {
    if (proc.exitCode !== null) return resolve(true);
    const t = setTimeout(() => resolve(false), gracefulWaitMs);
    proc.once("exit", () => { clearTimeout(t); resolve(true); });
  });
  if (exited) return true;
  try { proc.kill(); } catch { /* already gone */ }
  const dead = await new Promise<boolean>((resolve) => {
    if (proc.exitCode !== null) return resolve(true);
    const t = setTimeout(() => resolve(false), 5000);
    proc.once("exit", () => { clearTimeout(t); resolve(true); });
  });
  if (!dead) {
    try { proc.kill("SIGKILL"); } catch { /* already gone */ }
  }
  return dead;
}

/** Every launcher touching the MAIN login profile must use the SAME cookie
 * encryption backend, or Chrome reads the other launcher's cookies as
 * undecryptable, treats the session as signed out, and checkpoints that
 * amnesia to disk on close ("password every time").
 *
 * History: the early system used Playwright's bundled chromium everywhere
 * (single backend: `--password-store=basic`), and one-click re-login worked.
 * Switching headed login to the real Chrome binary (CDP, OS-keyring attempt)
 * while headless ops kept Playwright's mock keychain on the SAME dir broke
 * the invariant: alternating backends wiped the planted Google session.
 * Stripping the mock flags (OS-keyring direction) was tried and did not hold
 * on live profiles, so the rule is now the other way: pin the deterministic
 * `basic` store EXPLICITLY on every MAIN launcher (CDP spawn args +
 * Playwright `args`, never `ignoreDefaultArgs`). No keyring daemon, no lock
 * state, same bytes whoever opens the profile. The 0700 dir permission stays
 * the confidentiality boundary, as in the early system.
 * Exported for unit tests. */
export const MAIN_STORE_ARGS = ["--password-store=basic"];

/** Legacy name kept for the compiled test import; same value as
 * MAIN_STORE_ARGS. Do not reintroduce mock-keychain stripping on MAIN. */
export const KEYRING_OVERRIDE_ARGS = ["--password-store=basic"];

/** Headless render ops (page/crawl/batch/submit/open) never need the Google
 * web session - they run on jar-injected unnes cookies. They get their OWN
 * profile dir so no background renderer can ever meet (and burn or wipe)
 * the login profile's planted Google session. This also ends headed-login
 * vs background-render SingletonLock contention.
 * Renderers keep Playwright's stock defaults on their own dir; only MAIN
 * launchers must pin MAIN_STORE_ARGS. Exported for unit tests. */
export function renderDir(browserDir: string): string {
  return browserDir + "-headless";
}

/** True when `pid` is a Chrome process holding THIS profile dir (an orphan
 * from a killed run). Linux-only check, best effort. Exported for tests. */
export function isOwnChromeProcess(pid: number, browserDir: string): boolean {
  try {
    const cmd = readFileSync(`/proc/${pid}/cmdline`, "utf8");
    return cmd.includes("chrome") && cmd.includes(browserDir);
  } catch {
    return false;
  }
}

function reapOwnOrphan(browserDir: string, pid: number): void {
  // A previous run died without cleanup (Ctrl+C / closed terminal / kill):
  // its Chrome survived, still holding the profile lock AND unflushed
  // cookies. Reap it gracefully so this run gets a clean profile.
  // Called only after ownership + no-live-peer checks in cleanStaleSingleton,
  // and re-verified before each lethal step (PID reuse must never hit an
  // unrelated process).
  const stillOurs = () => isOwnChromeProcess(pid, browserDir);
  try {
    if (!stillOurs()) return;
    console.error(`[browser] reaping orphaned Chrome (pid ${pid}) from a previous run...`);
    process.kill(pid, "SIGTERM");
    const start = Date.now();
    while (Date.now() - start < 5000) {
      try {
        process.kill(pid, 0);
      } catch {
        break; // dead
      }
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 200);
    }
    try {
      process.kill(pid, 0);
      if (stillOurs()) process.kill(pid, "SIGKILL"); // refused to die gracefully
    } catch { /* dead */ }
  } catch { /* already gone */ }
}

// ---------------------------------------------------------------------------
// Shutdown guard: Ctrl+C / closed terminal / SIGTERM must NEVER nuke the
// browser mid-write (torn cookie store = "Google session disappeared").
// Browser ops arm the guard with their graceful closer; a signal runs it,
// waits for the flush, then exits with the conventional code.
// ---------------------------------------------------------------------------

let activeShutdown: (() => Promise<void>) | null = null;
let handlersInstalled = false;

function ensureSignalHandlers(): void {
  if (handlersInstalled) return;
  handlersInstalled = true;
  const shutdown = (sig: string, code: number) => {
    const closer = activeShutdown;
    activeShutdown = null;
    if (!closer) {
      process.exit(code);
      return;
    }
    console.error(`[browser] ${sig} received - flushing browser profile before exit...`);
    let done = false;
    const finish = () => {
      if (!done) {
        done = true;
        process.exit(code);
      }
    };
    setTimeout(finish, 15000).unref?.();
    Promise.resolve()
      .then(() => closer())
      .catch(() => {})
      .then(finish);
  };
  process.on("SIGINT", () => shutdown("SIGINT", 130));
  process.on("SIGTERM", () => shutdown("SIGTERM", 143));
  process.on("SIGHUP", () => shutdown("SIGHUP", 129));
}

/** Arm the guard around a browser op; call the returned disarm in `finally`.
 * Exported for unit tests (arm/disarm bookkeeping only). */
export function armShutdownGuard(closer: () => Promise<void>): () => void {
  ensureSignalHandlers();
  activeShutdown = closer;
  let cleared = false;
  return () => {
    if (!cleared) {
      cleared = true;
      if (activeShutdown === closer) activeShutdown = null;
    }
  };
}

async function launchCDPChrome(browserDir: string, headless: boolean, port = 0): Promise<CDPChromeInstance | null> {
  // port 0 = the OS picks a free debugging port per spawn (discovered below
  // via DevToolsActivePort). A FIXED port let two concurrent ops attach to
  // each other's browser: the second Chrome delegates through SingletonLock
  // and exits while the op happily drives (and closes!) the sibling's tabs.
  cleanStaleSingleton(browserDir);
  // A stale DevToolsActivePort from a previous run must not be mistaken for
  // ours (that would re-attach to a sibling): only a file written after this
  // spawn counts.
  const t0 = Date.now();
  try { unlinkSync(join(browserDir, "DevToolsActivePort")); } catch { /* absent */ }
  const chromeBin = findChromeBinary();
  console.error("[browser] launching " + (chromeBin ?? "(playwright bundled chromium)") + " on profile " + browserDir);
  if (!chromeBin) return null;
  try {
    mkdirSync(browserDir, { recursive: true });
    chmodSync(browserDir, 0o700);
  } catch { /* best effort */ }

  const proc = spawn(chromeBin, [
    `--remote-debugging-port=${port}`,
    "--remote-debugging-address=127.0.0.1",
    `--user-data-dir=${browserDir}`,
    "--no-first-run",
    "--no-default-browser-check",
    // Pinned cookie store: every MAIN launcher must agree byte-for-byte
    // (see MAIN_STORE_ARGS). Renders pass through here too, on their own
    // dir, where the same flag is simply harmless determinism.
    "--password-store=basic",
    "--disable-features=FedCm,CrossOriginOpenerPolicy",
    "--disable-blink-features=AutomationControlled",
    ...(headless ? ["--headless=new"] : []),
    "about:blank",
  ], { stdio: "ignore" });

  // Discover OUR port: DevToolsActivePort is written by our own Chrome right
  // after bind. A delegated/duplicate spawn exits without touching it.
  let actualPort = port;
  if (port === 0) {
    for (let i = 0; i < 25; i++) {
      if (proc.exitCode !== null) return null; // delegated to a lock holder; caller falls back
      try {
        const st = lstatSync(join(browserDir, "DevToolsActivePort"));
        if (st.mtimeMs >= t0 - 1000) {
          const n = Number(readFileSync(join(browserDir, "DevToolsActivePort"), "utf8").trim().split("\n")[0]);
          if (Number.isInteger(n) && n > 0) {
            actualPort = n;
            break;
          }
        }
      } catch { /* not written yet */ }
      await new Promise((r) => setTimeout(r, 400));
    }
    if (actualPort === 0) {
      try { proc.kill(); } catch { /* already gone */ }
      return null;
    }
  }

  let connected = false;
  for (let i = 0; i < 25; i++) {
    if (proc.exitCode !== null) return null; // lost the race; do not adopt a foreign browser
    await new Promise((r) => setTimeout(r, 400));
    try {
      const res = await fetch(`http://127.0.0.1:${actualPort}/json/version`);
      if (res.ok) { connected = true; break; }
    } catch {}
  }
  if (!connected) {
    try { proc.kill(); } catch { /* already gone */ }
    return null;
  }

  try {
    const mod = (await import("playwright")) as unknown as { chromium?: { connectOverCDP: (u: string) => Promise<unknown> }; default?: { chromium?: { connectOverCDP: (u: string) => Promise<unknown> } } };
    const chromium = mod.chromium ?? mod.default?.chromium;
    if (!chromium) { try { proc.kill(); } catch { /* already gone */ } return null; }
    const browser = (await chromium.connectOverCDP(`http://127.0.0.1:${actualPort}`)) as { close: () => Promise<void>; contexts: () => unknown[] };
    const ctx = browser.contexts()[0];
    return { browser, ctx, proc };
  } catch {
    // Failed to attach: never adopt a foreign debugger. Kill what we spawned
    // (best effort) and let the caller fall back - a leaked proc here becomes
    // the next run's "profile in use" orphan.
    try { proc.kill(); } catch { /* already gone */ }
    return null;
  }
}

export interface BrowserLoginResult {
  contract: number;
  ok: boolean;
  mode: "browser";
  status?: number;
  landingUrl: string | null;
  capturedCookies: number;
  /** persistent Google auth cookies kept (SID/SSID/HSID/SAPISID/APISID with a
   * real expiry). 0 means the account chooser will be EMPTY on the next
   * login - the web session was session-scoped and real Chrome dropped it. */
  googlePersistent?: number;
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

// Google cookies that prove a PERSISTENT web sign-in (they survive a browser
// restart and populate the account chooser): the SID family plus the account
// chooser marker. NID/OTZ/GAPS are prefs/telemetry - they do NOT keep you
// signed in on their own. Playwright reports session cookies with
// expires === -1; real Chrome drops those on close, which is exactly the
// "my Google accounts disappeared" symptom: the hub OAuth succeeded (so unnes
// cookies exist and login looks fine) but nothing persistent was planted.
// Exported for unit tests.
const GOOGLE_AUTH_COOKIES = new Set(["SID", "SSID", "HSID", "SAPISID", "APISID", "ACCOUNT_CHOOSER"]);

export interface GooglePersistence {
  /** persistent auth cookies (chooser will list accounts next login) */
  persistentAuth: string[];
  /** auth cookies that die with the browser (chooser empty next login) */
  sessionAuth: string[];
  /** prefs/telemetry only (NID/OTZ/GAPS/...) - not a sign-in */
  other: string[];
}

export function googlePersistenceStatus(all: PlaywrightCookie[]): GooglePersistence {
  const persistentAuth = new Set<string>();
  const sessionAuth = new Set<string>();
  const other = new Set<string>();
  for (const c of all) {
    if (!c.domain.includes("google")) continue;
    if (!GOOGLE_AUTH_COOKIES.has(c.name)) {
      other.add(c.name);
      continue;
    }
    if (c.expires === -1) sessionAuth.add(c.name);
    else persistentAuth.add(c.name);
  }
  return {
    persistentAuth: [...persistentAuth].sort(),
    sessionAuth: [...sessionAuth].sort(),
    other: [...other].sort(),
  };
}

export interface PersistenceChange {
  /** persistent auth survived this run */
  kept: boolean;
  /** persistent auth existed at open but is gone now (kill/crash/sign-out) */
  lost: boolean;
}

/** Compare the pre-login baseline against the post-login state. Exported
 * for unit tests. */
export function persistenceChange(pre: GooglePersistence, post: GooglePersistence): PersistenceChange {
  return {
    kept: post.persistentAuth.length > 0,
    lost: post.persistentAuth.length === 0 && pre.persistentAuth.length > 0,
  };
}

export async function browserLogin(jarPath: string, browserDir: string, hubUrl: string = HUB_URL): Promise<BrowserLoginResult> {
  cleanStaleSingleton(browserDir);
  let disarmShutdown: () => void = () => {};
  const fail = (code: string, message: string): BrowserLoginResult => {
    disarmShutdown();
    return {
      contract: 1, ok: false, mode: "browser", landingUrl: null, capturedCookies: 0,
      error: { code, message },
    };
  };

  // Snapshot the cookie store BEFORE Chrome opens it: if this run ends with
  // FEWER persistent Google cookies than it started with, the profile was
  // damaged mid-run (killed/crashed window, torn SQLite WAL) or signed out -
  // either way the user gets an exact diagnosis instead of "disappeared".
  const cookiesBackup = backupCookies(browserDir);

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

  let cdpInstance: CDPChromeInstance | null = null;
  let browserToClose: { close: () => Promise<void> } | null = null;
  let ctx: unknown = null;

  const cleanup = async () => {
    // Graceful close FIRST and let it finish: killing Chrome mid-write can
    // tear the cookie store (SQLite journal), which reads back as a
    // "disappeared" Google session. Verified by experiment: CDP close() alone
    // does NOT terminate an externally-spawned Chrome (still alive 30s+), so
    // the wait-then-SIGTERM below is what actually reaps it - SIGTERM shuts
    // Chrome down gracefully (exit code 0) with cookies intact. SIGKILL is
    // the last resort only.
    try {
      if (browserToClose) await browserToClose.close().catch(() => {});
      else if (ctx) await (ctx as { close(): Promise<void> }).close().catch(() => {});
      const proc = cdpInstance?.proc;
      if (proc) await awaitChromeExit(proc);
    } catch { /* best effort */ }
  };
  // From here on a browser may be alive: Ctrl+C / closed terminal / SIGTERM
  // runs cleanup() first (flush profile) instead of nuking mid-write.
  disarmShutdown = armShutdownGuard(cleanup);

  // 1. Try real Chrome via CDP first (Google does not block genuine Chrome binary)
  try {
    cdpInstance = await launchCDPChrome(browserDir, false);
    if (cdpInstance) {
      ctx = cdpInstance.ctx;
      browserToClose = cdpInstance.browser;
    }
  } catch { /* fall back to Playwright persistent context below */ }

  // 2. Fallback to Playwright persistent context if CDP Chrome is not available
  if (!ctx) {
    const chromeBin = findChromeBinary();
    for (let attempt = 0; attempt < 4; attempt++) {
      try {
        ctx = await (chromium as { launchPersistentContext(d: string, o: Record<string, unknown>): Promise<unknown> })
          .launchPersistentContext(browserDir, {
            headless: false,
            executablePath: chromeBin ?? undefined,
            args: ["--disable-blink-features=AutomationControlled", "--disable-features=FedCm,CrossOriginOpenerPolicy", ...MAIN_STORE_ARGS],
          });
        break;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        const busy = /user data directory is already in use|profile in use|singleton|process singleton/i.test(message);
        if (!busy) {
          return fail("usage", "could not launch Chromium with persistent profile: " + message + " (is another unnes login running? delete " + browserDir + " to force a fresh profile)");
        }
        await new Promise((r) => setTimeout(r, 15000));
      }
    }
  }

  if (!ctx) {
    return fail("usage", "could not launch browser: the profile is in use by another unnes instance - close it and retry, or run: unnes login");
  }

  const C = ctx as {
    pages(): PageLike[];
    newPage(): Promise<PageLike>;
    cookies(): Promise<PlaywrightCookie[]>;
    on(ev: "page", cb: (p: PageLike) => void): void;
    on(ev: "close", cb: () => void): void;
    close(): Promise<void>;
  };

  try {
    // Reuse an existing page if the persistent profile restored one.
    const initial = C.pages();
    const page = initial.length > 0 ? initial[0] : await C.newPage();

    // Track EVERY tab/popup: the hub's 'Login dengan UNNES-ID' button calls
    // gAuth2.signIn() which opens the Google account chooser in a POPUP, and
    // the handoff may land in any tab. We watch them all. A popup closing
    // after consent is NORMAL - only abort when every tab is gone (window
    // closed) or the browser process exits.
    const pages: PageLike[] = [...initial];
    let browserGone = false;
    C.on("page", (p) => {
      pages.push(p);
      p.on("close", () => {
        const i = pages.indexOf(p);
        if (i >= 0) pages.splice(i, 1);
      });
    });
    C.on("close", () => { browserGone = true; });
    if (cdpInstance?.proc) {
      cdpInstance.proc.on("exit", () => { browserGone = true; });
    }

    if (!page.isClosed()) {
      await page.goto(hubUrl, { waitUntil: "domcontentloaded", timeout: 60000 });
    }

    // If this profile holds no Google session, the popup forces full
    // credential entry every time ("fresh spawning"). Open accounts.google.com
    // first so the user can plant a persistent session there - next logins
    // degrade to one click on the account chooser.
    // The same read doubles as the pre-login persistence baseline: compared
    // against the post-login state it proves whether THIS run damaged the
    // profile (kill/crash/sign-out) or simply never planted anything.
    let prePersist: GooglePersistence = { persistentAuth: [], sessionAuth: [], other: [] };
    try {
      const pre = await C.cookies().catch(() => [] as PlaywrightCookie[]);
      prePersist = googlePersistenceStatus(pre as PlaywrightCookie[]);
      const hasGoogle = prePersist.persistentAuth.length + prePersist.sessionAuth.length + prePersist.other.length > 0;
      if (!hasGoogle) {
        const gpage = await C.newPage().catch(() => null);
        if (gpage && !(gpage as PageLike).isClosed()) {
          await (gpage as PageLike).goto("https://accounts.google.com", { waitUntil: "domcontentloaded", timeout: 30000 }).catch(() => {});
        }
        console.error("NO Google session in this profile yet: sign into Google in the");
        console.error("accounts.google.com tab first (stay signed in), THEN click");
        console.error("'Login dengan UNNES-ID' on the hub tab.");
      }
    } catch { /* best effort: hub flow still works without it */ }

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
    // (window.location.href = obj.route, e.g. /gate/list). That route navigation is the handoff.
    // Note: apps.unnes.ac.id redirects / to /login when not authenticated, so /login
    // is NOT a handoff - handoff requires leaving /login to /gate/list or another subdomain.
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
            const onLogin = u.pathname === "/" || u.pathname === "/login" || u.pathname.startsWith("/login/");
            const isAuthRoute = u.pathname.startsWith("/gate") || u.pathname.startsWith("/dashboard") || /^\/\d+/.test(u.pathname);
            if (leftHub || (!onLogin && isAuthRoute)) return "handoff";
          }
        }
        return null;
      })();
      const winner = await Promise.race([poll, enter]);
      if (winner) done = winner;
      else await new Promise((r) => setTimeout(r, IDLE_MS));
    }

    if (done === null) {
      await cleanup();
      return fail("usage", "timed out waiting for login; no session captured");
    }
    if (done === "closed") {
      const currentCookies = await C.cookies().catch(() => []);
      const hasSession = (currentCookies as PlaywrightCookie[]).some(c => c.name === "identitas_sso");
      if (!hasSession) {
        await cleanup();
        return fail("usage", "browser window was closed; login aborted, no portal session saved (anything already signed into Google in the profile tabs is kept on disk)");
      }
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

    // Prime Elena session (app 30) automatically right after gateway handoff
    try {
      const activeP = pages.find((p) => !p.isClosed()) || page;
      if (!activeP.isClosed()) {
        console.error("Priming Elena (app 30) session...");
        await (activeP as { goto(u: string, o?: unknown): Promise<unknown> }).goto("https://apps.unnes.ac.id/30", { waitUntil: "domcontentloaded", timeout: 30000 });
        await completeElenaSession(activeP as never, "20261");
      }
    } catch { /* best effort */ }

    const all = await C.cookies();
    // Overlay onto the EXISTING jar (never from empty): sibling-portal
    // cookies the login window never visited (akademik/duanol sessions) stay
    // valid server-side and must survive a hub-only re-login.
    const jar = await CookieJar.load(jarPath);
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
      await cleanup();
      return fail("usage", "captured no unnes.ac.id cookies; did the Google sign-in complete?");
    }

    // Auto-triggers only fire on real handoff shapes (hub redirect after a
    // Google visit, or a tab on another unnes.ac.id subdomain), so no extra
    // session-cookie proof is required - the jar may legitimately contain
    // only the Laravel session cookie (the hub disconnects Google itself via
    // auth2.disconnect(), which is why login always asks again).
    console.error("captured " + captured + " unnes.ac.id cookies: " + [...names].sort().join(", "));
    const persist = googlePersistenceStatus(all as PlaywrightCookie[]);
    const googlePersistent = persist.persistentAuth.length;
    const change = persistenceChange(prePersist, persist);
    if (change.kept) {
      console.error("Google session kept in profile (" + persist.persistentAuth.join(", ") + ") - next login is one click.");
    } else if (change.lost) {
      // The profile ENTERED this run with a persistent session and leaves
      // without one: killed/crashed window (torn cookie WAL) or an explicit
      // Google sign-out mid-run. Say exactly that, plus the recovery.
      console.error("!!! Google session LOST DURING this login: the profile had persistent auth");
      console.error("!!! (" + prePersist.persistentAuth.join(", ") + ") when the window opened, now it has none.");
      console.error("!!! Causes: the window/process was killed or crashed before Chrome flushed");
      console.error("!!! cookies to disk (never Ctrl+C / close the terminal mid-login - let unnes");
      console.error("!!! close the window itself), or you signed out of Google in one of the tabs.");
      if (cookiesBackup) {
        console.error("!!! A pre-login cookie backup exists at:");
        console.error("!!!   " + cookiesBackup);
        console.error("!!! With every browser closed you MAY restore it via:");
        console.error("!!!   cp " + cookiesBackup + " " + join(browserDir, "Default", "Cookies"));
        console.error("!!! then re-run unnes login fully. Skip the restore if you signed out on purpose.");
      }
    } else {
      // Loud on purpose: with zero persistent auth cookies the account
      // chooser is EMPTY on the next login ("accounts disappeared"), even
      // though the portal session above is valid. Session-only auth cookies
      // (expires -1) are dropped by Chrome on close - they never persist.
      console.error("!!! Google sign-in will NOT persist - no persistent SID/SSID/HSID/SAPISID/APISID cookie in this profile.");
      if (persist.sessionAuth.length > 0) {
        console.error("!!! found session-only auth cookies (" + persist.sessionAuth.join(", ") + "): they die when the browser closes.");
      } else if (persist.other.length > 0) {
        console.error("!!! found only prefs/telemetry cookies (" + persist.other.join(", ") + "): those are not a sign-in.");
      } else {
        console.error("!!! found no Google cookies at all in this profile.");
      }
      console.error("!!! To fix: in the opened window, open the accounts.google.com tab FIRST,");
      console.error("!!! sign in there and tick 'Stay signed in' (trust this device / 2FA),");
      console.error("!!! THEN click 'Login dengan UNNES-ID' on the hub tab. Do not close the");
      console.error("!!! window yourself - let unnes close it so cookies flush to disk.");
    }
    console.error("note: the hub severs its own Google grant after every handshake, so the");
    console.error("'Login dengan UNNES-ID' click stays - but with the session above it is");
    console.error("one click, no password retype. Session check any time: unnes status.");
    await cleanup();
    disarmShutdown();
    return { contract: 1, ok: true, mode: "browser", landingUrl, capturedCookies: captured, googlePersistent };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    await cleanup();
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
// Moodle shows an inline login page instead of redirecting.
const MOODLE_LOGIN_MARKERS = /you are not logged in|you must be logged in|log in\s*\|/i;

async function launchContext(browserDir: string, headless = true): Promise<unknown> {
  // Render ops run on the SEPARATE headless profile (see renderDir): no
  // background process may open the login profile headless (a headless
  // visit burns the kept Google session server-side). All callers pass the
  // main profile dir; the mapping happens here, in one place.
  const dir = renderDir(browserDir);
  cleanStaleSingleton(dir);
  const chromium = await import("playwright").then(
    (m) => (m as unknown as { chromium?: unknown; default?: { chromium?: unknown } }).chromium
      ?? (m as unknown as { default?: { chromium?: unknown } }).default?.chromium,
    () => null,
  );
  if (!chromium) throw new Error("playwright is not installed; run: cd fetcher && npm ci && npx playwright install chromium");
  try {
    mkdirSync(dir, { recursive: true });
    chmodSync(dir, 0o700);
  } catch { /* best effort */ }
  // FedCm disabled so gapi falls back to the popup flow (see above).
  // Headed is used by op=open so the user sees the real logged-in page in
  // the profile browser (the system default browser has no session).
  // Retry loop: another operation (submit, open) may hold the profile lock.
  // browserLogin has the same pattern for the same reason.
  console.error("[browser] render context on profile " + dir + " via " + chromeLabel());
  for (let attempt = 0; attempt < 4; attempt++) {
    try {
      const chromeBin = findChromeBinary();
      const ctx = await (chromium as { launchPersistentContext(d: string, o: Record<string, unknown>): Promise<unknown> })
        .launchPersistentContext(dir, {
          headless,
          executablePath: chromeBin ?? undefined,
          args: ["--disable-blink-features=AutomationControlled", "--disable-features=FedCm,CrossOriginOpenerPolicy"],
        });
      // Render-only savings: images, media and fonts never affect text
      // extraction, so abort them at the request edge (bytes + time saved
      // per page). Login-profile contexts never pass through here - Google
      // chooser/popup flows are sensitive to blocked subresources.
      try {
        await (ctx as {
          route(u: string, h: (route: {
            request(): { resourceType(): string };
            abort(e?: string): Promise<void>;
            fallback(): Promise<void>;
          }) => Promise<void>): Promise<void>;
        }).route("**/*", async (route) => {
          try {
            const t = route.request().resourceType();
            if (t === "image" || t === "media" || t === "font") {
              await route.abort("blockedbyclient").catch(() => {});
              return;
            }
            await route.fallback().catch(() => {});
          } catch {
            /* closed context mid-flight */
          }
        });
      } catch {
        /* routing unsupported here: renders still work, just untrimmed */
      }
      return ctx;
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      const busy = /user data directory is already in use|profile in use|singleton|process singleton|Opening in existing browser session/i.test(message);
      if (!busy || attempt >= 3) throw err;
      await new Promise((r) => setTimeout(r, 12000));
    }
  }
  throw new Error("could not launch context after 4 attempts (profile locked)");
}

/** Copy every *.unnes.ac.id cookie from the jar into the browser context.
 * The persistent profile's own gateway cookies are short-lived; the jar holds
 * the freshest session (re-saved after every op). Without this injection,
 * headless renders navigate with a stale profile and bounce to /login even
 * though `unnes status` reports VALID. */
async function loadJarIntoContext(jarPath: string, ctx: unknown): Promise<number> {
  let n = 0;
  let cookies: Array<{ name: string; value: string; domain: string; path: string; secure: boolean; httpOnly: boolean; expires: number | null }> = [];
  try {
    const parsed = JSON.parse(await import("node:fs/promises").then((fs) => fs.readFile(jarPath, "utf8"))) as { cookies?: typeof cookies };
    cookies = parsed.cookies ?? [];
  } catch { /* no jar yet — first login */ }
  // NOTE: use the RAW file, not CookieJar.load — load() drops expired entries,
  // but the server-side session may still be alive (see expiry clamp below).
  for (const c of cookies) {
    if (!c.domain.endsWith("unnes.ac.id")) continue;
    const ck = {
      name: c.name,
      value: c.value,
      domain: c.domain,
      path: c.path || "/",
      secure: c.secure ?? true,
      httpOnly: c.httpOnly ?? false,
    } as Record<string, unknown>;
    // Chrome silently drops cookies with past expiry, but the server-side
    // session often outlives the client-side timestamp (gateway sessions
    // slide). Inject expired-looking ones as session cookies instead —
    // after the op, syncJarFromContext re-captures fresh values + expiries.
    // floored: fractional seconds can make addCookies reject valid cookies.
    if (c.expires && c.expires > Date.now()) ck.expires = Math.floor(c.expires / 1000);
    try {
      await (ctx as { addCookies(cs: unknown[]): Promise<void> }).addCookies([ck]);
      n += 1;
    } catch { /* malformed cookie — skip */ }
  }
  return n;
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
 * Wait for a page state by SIGNAL instead of a fixed sleep: the watched
 * response (armed before its trigger), then the selector that proves the
 * follow-on render, both bounded by `timeoutMs`. Returns early the moment
 * the state holds - the fixed 5-6s prime sleeps this replaces never could.
 * Either signal alone suffices when only one is given; missing signals
 * resolve `settled: false` instead of hanging. Exported for unit tests.
 */
export interface SettleOpts {
  responseRe?: RegExp;
  selector?: string;
  timeoutMs?: number;
}

export async function settle(
  page: {
    waitForResponse(pred: (r: { url(): string }) => boolean, o?: unknown): Promise<unknown>;
    waitForSelector(s: string, o?: unknown): Promise<unknown>;
  },
  opts: SettleOpts,
): Promise<{ settled: boolean; elapsedMs: number }> {
  const t0 = Date.now();
  const cap = opts.timeoutMs ?? 8000;
  if (!opts.responseRe && !opts.selector) return { settled: false, elapsedMs: 0 };
  if (opts.responseRe) {
    try {
      await page.waitForResponse((r) => (opts.responseRe as RegExp).test(r.url()), { timeout: cap });
    } catch {
      /* fall through to the selector check */
    }
  }
  if (opts.selector) {
    const left = Math.max(1000, cap - (Date.now() - t0));
    try {
      await page.waitForSelector(opts.selector, { timeout: left });
    } catch {
      return { settled: false, elapsedMs: Date.now() - t0 };
    }
  }
  return { settled: true, elapsedMs: Date.now() - t0 };
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
  // Failures are logged (stderr is streamed for browser ops): a wrong
  // semester or renamed button must not masquerade as a good prime.
  const log = (m: string) => console.error("[elena-prime] " + m);
  try {
    let foundSso = false;
    for (const f of page.frames()) {
      let u = "";
      try { u = await f.url(); } catch { continue; }
      if (u.includes("login_sso")) {
        foundSso = true;
        try {
          await f.click("#btnKlik_" + semester, { timeout: 5000 });
        } catch {
          log("semester button #btnKlik_" + semester + " missing (wrong semester? renamed?)");
        }
        break;
      }
    }
    if (!foundSso) log("no login_sso iframe found; handshake may already be done or the page changed");
    // Was a fixed 5s sleep for the login_url page to load. Now: wait for
    // its #btnTest continue button (bounded) - proceeds the moment it
    // appears instead of always paying the full sleep.
    const pg = page as unknown as {
      waitForResponse(pred: (r: { url(): string }) => boolean, o?: unknown): Promise<unknown>;
      waitForSelector(s: string, o?: unknown): Promise<unknown>;
      waitForURL?(u: string | RegExp, o?: unknown): Promise<unknown>;
    };
    if (typeof pg.waitForResponse === "function" && typeof pg.waitForSelector === "function") {
      await settle(pg, { selector: "#btnTest", timeoutMs: 8000 });
    } else {
      await new Promise((r) => setTimeout(r, 5000));
    }
    try {
      await page.click("#btnTest", { timeout: 5000 });
    } catch {
      log("#btnTest continue button missing; handshake may already be done or the page changed");
    }
    // Was a fixed 6s sleep for the /my/ landing. Now: wait for the URL
    // itself (bounded) - same ceiling class, usually instant.
    if (typeof pg.waitForURL === "function") {
      try {
        await pg.waitForURL(/elena\.unnes\.ac\.id\/my/, { timeout: 8000 });
      } catch {
        /* fall through: session may already be established */
      }
    } else {
      await new Promise((r) => setTimeout(r, 6000));
    }
  } catch (e) {
    log("handshake error (best effort, session may already be established): " + (e instanceof Error ? e.message : String(e)).slice(0, 160));
  }
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
    await loadJarIntoContext(jarPath, ctx);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "usage", message: message } };
  }
  // Ctrl+C / closed terminal mid-render must flush, not tear, the profile.
  const disarmShutdown = armShutdownGuard(async () => {
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  });
  let page: unknown;
  try {
    // Inside the guarded try: a newPage() throw now funnels through the
    // normal error path (and the guarded close) instead of leaking the
    // browser with no cleanup.
    page = await (ctx as { newPage(): Promise<unknown> }).newPage();
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

    // Session health FIRST: an expired render must never persist the
    // guest/anonymous cookies the login page issued (that would clobber the
    // good jar and fail the next op too - same guard as the http.ts rollback).
    const body = html.replace(/<script[\s\S]*?<\/script>/gi, " ").replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim().slice(0, 500);
    const landedOnGateway = finalUrl.startsWith("https://apps.unnes.ac.id");
    const sessionExpired = (landedOnGateway && LOGIN_MARKERS.test(body)) || /\/auth\/login/i.test(finalUrl) || MOODLE_LOGIN_MARKERS.test(body);
    if (sessionExpired) {
      return { ...base, finalUrl, sessionExpired: true, capturedCookies: 0, error: { code: "session", message: "session expired; run: unnes login" } };
    }

    const jar = await CookieJar.load(jarPath);
    const captured = await syncJarFromContext(ctx, jar);
    await jar.save(jarPath);

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
    disarmShutdown();
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
  /** links skipped (goto/selector failure) - records are a silent subset
   * without this count, so it is reported, not hidden. */
  skipped: number;
  records: Record<string, string>[];
  error?: { code: string; message: string };
  [k: string]: unknown;
}

export async function crawlPage(jarPath: string, browserDir: string, opts: CrawlOpts): Promise<CrawlResult> {
  const base = {
    contract: 1, ok: false as boolean, op: "crawl" as const,
    finalUrl: null as string | null, sessionExpired: false, followed: 0, skipped: 0, records: [] as Record<string, string>[],
  };
  if (process.env.UNNES_NO_BROWSER) {
    return { ...base, error: { code: "usage", message: "crawl disabled via UNNES_NO_BROWSER" } };
  }
  let ctx: unknown;
  try {
    ctx = await launchContext(browserDir);
    await loadJarIntoContext(jarPath, ctx);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "usage", message } };
  }
  // Ctrl+C / closed terminal mid-render must flush, not tear, the profile.
  const disarmShutdown = armShutdownGuard(async () => {
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  });
  let page: unknown;
  const maxLinks = opts.maxLinks ?? 50;
  try {
    page = await (ctx as { newPage(): Promise<unknown> }).newPage();
    const P = page as {
      goto(u: string, o?: unknown): Promise<unknown>;
      url(): Promise<string>;
      content(): Promise<string>;
      waitForSelector(s: string, o?: unknown): Promise<unknown>;
      evaluate(fn: string): Promise<unknown>;
    };
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
      "(function(){var out=[];var seen={};document.querySelectorAll(__UNNES_SEL__).forEach(function(a){var h=a.href||a.getAttribute('href');if(!h)return;var u=new URL(h,location.href);if(!/(^|\\.)unnes\\.ac\\.id$/.test(u.hostname))return;if(seen[u.href])return;seen[u.href]=1;out.push({href:u.href,text:(a.innerText||a.textContent||'').replace(/\\s+/g,' ').trim().slice(0,120)});});return out;})()"
        .split("__UNNES_SEL__").join(JSON.stringify(opts.linkSelector)),
    )) as { href: string; text: string }[];

    const { extractRecords } = await import("./extract.js");
    const records: Record<string, string>[] = [];
    const waitMs = opts.waitMs ?? 15000;
    let skipped = 0;
    for (const link of links.slice(0, maxLinks)) {
      try {
        await P.goto(link.href, { waitUntil: "domcontentloaded", timeout: 60000 });
      } catch { skipped += 1; continue; }
      try {
        await P.waitForSelector(opts.pageExtract.selector, { timeout: waitMs });
      } catch { skipped += 1; continue; } // page without matches: counted, not hidden
      const html = await P.content();
      const recs = extractRecords(html, opts.pageExtract);
      for (const r of recs) {
        records.push({ ...r, _source: link.href, _title: link.text });
      }
    }
    const jar = await CookieJar.load(jarPath);
    await syncJarFromContext(ctx, jar);
    await jar.save(jarPath);
    if (skipped > 0) console.error(`[crawl] ${skipped}/${Math.min(links.length, maxLinks)} links skipped (goto/selector failures)`);
    return { ...base, ok: true, finalUrl: await safeUrl(P), followed: Math.min(links.length, maxLinks), skipped, records };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "network", message: "crawl failed: " + message } };
  } finally {
    disarmShutdown();
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
  /** crawl-mode links skipped (goto/selector failure) */
  skipped?: number;
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
    "(function(){var out=[];var seen={};document.querySelectorAll(__UNNES_SEL__).forEach(function(a){var h=a.href||a.getAttribute('href');if(!h)return;var u=new URL(h,location.href);if(!/(^|\\.)unnes\\.ac\\.id$/.test(u.hostname))return;if(seen[u.href])return;seen[u.href]=1;out.push({href:u.href,text:(a.innerText||a.textContent||'').replace(/\\s+/g,' ').trim().slice(0,120)});});return out;})()"
      .split("__UNNES_SEL__").join(JSON.stringify(entry.linkSelector)),
  )) as { href: string; text: string }[];
  const maxLinks = entry.maxLinks ?? 50;
  const records: Record<string, string>[] = [];
  let skipped = 0;
  for (const link of links.slice(0, maxLinks)) {
    try {
      await P.goto(link.href, { waitUntil: "domcontentloaded", timeout: 60000 });
    } catch { skipped += 1; continue; }
    if (entry.extract?.selector) {
      try { await P.waitForSelector(entry.extract.selector, { timeout: entry.waitMs ?? 15000 }); } catch { skipped += 1; continue; }
    }
    const html = await P.content();
    const recs = extractRecords(html, entry.extract ?? { selector: "body" });
    for (const rec of recs) {
      records.push({ ...rec, _source: link.href, _title: link.text });
    }
  }
  r.finalUrl = await P.url();
  r.records = records;
  r.skipped = skipped;
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
    await loadJarIntoContext(jarPath, ctx);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return fail("usage", message);
  }
  // Ctrl+C / closed terminal mid-render must flush, not tear, the profile.
  const disarmShutdown = armShutdownGuard(async () => {
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  });
  let page: unknown;
  const primed = new Set<string>();
  const results: BatchPageResult[] = [];
  try {
    page = await (ctx as { newPage(): Promise<unknown> }).newPage();
    const P = page as {
      goto(u: string, o?: unknown): Promise<unknown>;
      url(): Promise<string>;
      content(): Promise<string>;
      waitForSelector(s: string, o?: unknown): Promise<unknown>;
      evaluate(fn: string): Promise<unknown>;
    };
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
          r.sessionExpired = (landedOnGateway && LOGIN_MARKERS.test(body)) || /\/auth\/login/i.test(r.finalUrl ?? "") || MOODLE_LOGIN_MARKERS.test(body);
          if (r.sessionExpired) {
            // Honest error (not "unknown batch error"): the Rust side keys
            // its exit-4 session handling off this message.
            r.error = { code: "session", message: "session expired; run: unnes login" };
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
    // Quarantine: one expired entry means the context holds guest cookies -
    // persisting them would clobber the good jar (see renderPage verdict).
    const anyExpired = results.some((r) => r.sessionExpired);
    let captured = 0;
    if (!anyExpired) {
      captured = await syncJarFromContext(ctx, jar);
      await jar.save(jarPath);
    }
    return { contract: 1, ok: true, op: "batch", results, capturedCookies: captured };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return fail("internal", "batch render failed: " + message);
  } finally {
    disarmShutdown();
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  }
}


// ---------------------------------------------------------------------------
// op: login mode=auto - scripted re-login without surprise windows.
//
// Single-writer rule for the MAIN profile: only headed, user-supervised
// flows open it. A headless open burns the planted Google session
// server-side (Google serves a blank identifier instead of the chooser
// afterwards), which is why re-logins used to demand a full password every
// time. So there is deliberately NO headless browser attempt here:
//   - non-interactive callers (status/TUI/watch) get needsInteraction when
//     the gateway lapses and the user does one headed chooser click;
//   - interactive callers go straight to the headed scripted click-through
//     (chooser/consent) and then the manual browser login.
// Plain-HTTP SSO refresh (op=sso) still heals app sessions without any
// browser. If Google demands anything beyond a click (password, 2FA,
// CAPTCHA) we fail with needsInteraction and the caller falls back to the
// headed browser login.
// ---------------------------------------------------------------------------

export async function autoLogin(
  jarPath: string,
  browserDir: string,
  opts: { interactive?: boolean } = {},
): Promise<BrowserLoginResult> {
  const fail = (code: string, message: string): BrowserLoginResult => ({
    contract: 1, ok: false, mode: "browser", landingUrl: null, capturedCookies: 0, error: { code, message },
  });
  if (process.env.UNNES_NO_BROWSER) {
    return fail("usage", "auto login disabled via UNNES_NO_BROWSER");
  }

  // 1. Headless scripted attempt: disabled by design (single-writer rule -
  // scriptedOAuth returns null immediately for headless). Zero windows AND
  // zero profile touches when non-interactive; the kept session survives.
  {
    const res = await scriptedOAuth(jarPath, browserDir, true);
    if (res) return res;
  }

  if (opts.interactive === false) {
    // Callers that must never block on a human (e.g. the TUI dashboard)
    // stop here: NO headed attempt, no window that waits for a click.
    return fail("needsInteraction", "Google asked for human interaction (password/2FA/CAPTCHA); run: unnes login");
  }

  // 2. Headed attempt via the hub's own Google button: gapi's onSignIn keeps
  //    the token in page state (currentUser), so we do not depend on the
  //    popup's opener channel at all. Scripted clicks handle chooser/consent.
  {
    const res = await scriptedOAuth(jarPath, browserDir, false);
    if (res) return res;
  }

  // 3. Google demanded human interaction: keep a headed window open with the
  //    standard flow - the user completes ONE click (saved profile makes it
  //    a single confirmation).
  const { browserLogin } = await import("./browser.js");
  return browserLogin(jarPath, browserDir);
}

// ---------------------------------------------------------------------------
// Drive the Google login scripted: headless or headed. Two token channels:
//   A) postmessage listener on the hub page (works when the popup keeps its
//      opener - i.e. before Google's COOP header kicks in),
//   B) gapi.currentUser on the hub page after the REAL #btn-google handler
//      runs (the channel the human login uses, so it matches user behaviour).
// Returns a BrowserLoginResult when done, or null to try the next mode.
// ---------------------------------------------------------------------------
async function scriptedOAuth(jarPath: string, browserDir: string, headless: boolean): Promise<BrowserLoginResult | null> {
  // MAIN-profile single-writer rule: a HEADLESS open of the login profile
  // burns the planted Google session (observed: 40+ cookies wiped and the
  // popup served a blank identifier instead of the chooser right after a
  // headless visit; every status/TUI/watch auto attempt re-burned it, so the
  // next `unnes login` always asked for a full password again). The old
  // in-flow burn guard ran AFTER the damaging launch + hub navigation, which
  // is too late. So headless scripted never launches at all: return null and
  // let the caller escalate to headed (one chooser click on the kept
  // session) or report needsInteraction. Only headed, user-supervised flows
  // may open MAIN.
  if (headless) {
    console.error("[auto-login] headless scripted disabled on the login profile (would burn the kept Google session); escalating");
    return null;
  }
  cleanStaleSingleton(browserDir);
  let cdpInstance: CDPChromeInstance | null = null;
  let ctx: unknown = null;
  let browserToClose: { close: () => Promise<void> } | null = null;

  // 1. Try real Chrome via CDP first (Google does not block genuine Chrome binary)
  try {
    cdpInstance = await launchCDPChrome(browserDir, headless);
    if (cdpInstance) {
      ctx = cdpInstance.ctx;
      browserToClose = cdpInstance.browser;
    }
  } catch { /* fall back to Playwright persistent context below */ }

  // 2. Fallback to Playwright persistent context if CDP Chrome is not available
  if (!ctx) {
    let chromium: unknown = null;
    try {
      const mod = (await import("playwright")) as unknown as { chromium?: unknown; default?: { chromium?: unknown } };
      chromium = mod.chromium ?? mod.default?.chromium ?? null;
    } catch { /* below */ }
    if (!chromium) {
      console.error("[auto-login] playwright unavailable, trying next mode");
      return null;
    }

    for (let attempt = 0; attempt < 4; attempt++) {
      try {
        const chromeBin = findChromeBinary();
        ctx = await (chromium as { launchPersistentContext(d: string, o: Record<string, unknown>): Promise<unknown> })
          .launchPersistentContext(browserDir, {
            headless,
            executablePath: chromeBin ?? undefined,
            args: ["--disable-blink-features=AutomationControlled", "--disable-features=FedCm,CrossOriginOpenerPolicy", ...MAIN_STORE_ARGS],
          });
        break;
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        if (!/user data directory is already in use|profile in use|singleton|process singleton/i.test(message)) {
          console.error("[auto-login] launch failed, trying next mode: " + message.slice(0, 160));
          return null;
        }
        await new Promise((r) => setTimeout(r, 12000));
      }
    }
  }
  if (!ctx) {
    console.error("[auto-login] could not launch browser (profile locked?), trying next mode");
    return null;
  }
  const C = ctx as {
    pages(): unknown[];
    close(): Promise<void>;
    waitForEvent(e: string, o?: unknown): Promise<unknown>;
    addInitScript(fn: () => void): Promise<void>;
  };
  try {
    await C.addInitScript(() => {
      Object.defineProperty(navigator, "webdriver", { get: () => undefined });
    }).catch(() => {});
  } catch { /* best effort */ }
  // Ctrl+C / closed terminal mid-render must flush, not tear, the profile.
  // The closer also reaps the CDP process: close() alone never terminates an
  // externally-spawned Chrome (verified: still alive 30s+), which would leak
  // an orphan holding the profile lock.
  const disarmShutdown = armShutdownGuard(async () => {
    try {
      if (browserToClose) await browserToClose.close().catch(() => {});
      else await C.close().catch(() => {});
    } catch { /* already closed */ }
    const proc = cdpInstance?.proc;
    if (proc) await awaitChromeExit(proc);
  });
  let page: unknown;
  try {
    page = await (ctx as { newPage(): Promise<unknown> }).newPage();
    const P = page as {
      goto(u: string, o?: unknown): Promise<unknown>;
      url(): Promise<string>;
      content(): Promise<string>;
      evaluate(fn: string | ((...a: any[]) => unknown), arg?: unknown): Promise<unknown>;
      waitForTimeout(ms: number): Promise<void>;
      click(s: string, o?: unknown): Promise<void>;
    };
    await P.goto("https://apps.unnes.ac.id/", { waitUntil: "domcontentloaded", timeout: 60000 });
    await P.waitForTimeout(4000);
    const state0 = (await P.evaluate(() => ({
      isLogin: /Login dengan UNNES-ID|Single Sign On/i.test(document.body.innerText),
    }))) as { isLogin: boolean };
    if (!state0.isLogin) {
      // already logged in: sync cookies and finish
      const jar = await CookieJar.load(jarPath);
      const captured = await syncJarFromContext(C, jar);
      await jar.save(jarPath);
      return { contract: 1, ok: true, mode: "browser", landingUrl: await P.url(), capturedCookies: captured };
    }

    // Session-burn guard: a HEADLESS visit to accounts.google.com while a live
    // planted session exists makes Google distrust + invalidate it (observed:
    // 40+ session cookies wiped right after a headless popup probe; the popup
    // was served a blank identifier instead of the chooser). So when a Google
    // session is present and we are headless, do NOT summon the popup - bail
    // out and let the caller escalate to headed instead of burning the session.
    try {
      const pre = await (C as unknown as { cookies(): Promise<PlaywrightCookie[]> }).cookies().catch(() => [] as PlaywrightCookie[]);
      const hasGoogleSession = (pre as PlaywrightCookie[]).some((c) =>
        c.domain.includes("google") && /^(SID|SSID|HSID|SAPISID|APISID)$/.test(c.name));
      if (hasGoogleSession && headless) {
        console.error("[auto-login] live Google session + headless: escalating to headed (never summon the popup headless)");
        return null;
      }
    } catch { /* best effort: proceed without the guard */ }

    // Channel A: postmessage listener + remember the gapi instance
    await P.evaluate(() => {
      (window as unknown as Record<string, unknown>).__idToken = null;
      window.addEventListener("message", (e) => {
        try {
          const d = JSON.parse(e.data as string);
          if (d && d.id_token) (window as unknown as Record<string, unknown>).__idToken = d.id_token;
        } catch { /* ignore */ }
      });
    });

    // Click the hub's own Google button (its binding drives gapi.signIn and
    // resolves the token into gapi.currentUser in this page).
    try {
      await P.click("#btn-google", { timeout: 8000 });
    } catch {
      console.error("[auto-login] no #btn-google on the hub page (layout changed?), trying next mode");
      return null;
    }

    let token: string | null = null;
    for (let i = 0; i < 30; i++) {
      await P.waitForTimeout(2500);
      const t = (await P.evaluate(() => {
        const w = window as unknown as Record<string, unknown>;
        if (w.__idToken) return w.__idToken as string;
        try {
          const g = (w as { gapi?: { auth2?: { getAuthInstance?: () => { currentUser?: { get?: () => { getAuthResponse?: () => { id_token?: string } } } } } } }).gapi;
          const u = g?.auth2?.getAuthInstance?.()?.currentUser?.get?.();
          const tok = u?.getAuthResponse?.()?.id_token;
          return tok || null;
        } catch { return null; }
      }).catch(() => null)) as string | null;
      if (t) { token = t; break; }
      // scripted clicks on any Google popup (chooser / consent)
      for (const p of C.pages()) {
        if (p === page) continue;
        const PO = p as { evaluate(fn: string | ((...a: any[]) => unknown)): Promise<unknown>; click(s: string, o?: unknown): Promise<void> };
        try {
          const st = (await PO.evaluate(() => ({
            hasAllow: !!document.querySelector("#submit_approve_access"),
            // current account chooser rows are div.yavlK (no data-email)
            hasEmail: !!document.querySelector("[data-email]"),
            hasAccount: !!document.querySelector("div.yavlK"),
            url: location.href.slice(0, 120),
          }))) as { hasAllow: boolean; hasEmail: boolean; hasAccount: boolean; url: string };
          if (st.hasAllow) { await PO.click("#submit_approve_access", { timeout: 3000 }).catch(() => {}); continue; }
          if (st.hasEmail) { await PO.click("[data-email]", { timeout: 2000 }).catch(() => {}); continue; }
          if (st.hasAccount) { await PO.click("div.yavlK", { timeout: 2000 }).catch(() => {}); continue; }
        } catch { /* popup closed */ }
      }
    }
    if (!token) {
      console.error("[auto-login] no id_token after 75s (popup blocked? consent denied? chooser layout changed?), trying next mode");
      return null;
    }

    // POST the id_token to the hub and verify the session really works.
    const verified = await completeHubLogin(P, C, jarPath, token);
    if (!verified) console.error("[auto-login] hub POST/verify failed, trying next mode");
    return verified;
  } catch (err) {
    console.error("[auto-login] scripted attempt failed, trying next mode: " + (err instanceof Error ? err.message : String(err)).slice(0, 160));
    return null;
  } finally {
    disarmShutdown();
    try {
      if (browserToClose) await browserToClose.close().catch(() => {});
      else await C.close().catch(() => {});
      // Same flush-first reasoning as browserLogin cleanup (see
      // awaitChromeExit): close() alone never terminates an
      // externally-spawned Chrome, so verify + SIGTERM + SIGKILL.
      const proc = cdpInstance?.proc;
      if (proc) await awaitChromeExit(proc);
    } catch { /* already closed */ }
  }
}

// POST id_token -> /google/auth, then require the gateway app list to load
// WITHOUT the login page. Returns the result or null when not authenticated.
async function completeHubLogin(
  P: { url(): Promise<string>; goto(u: string, o?: unknown): Promise<unknown>; waitForTimeout(ms: number): Promise<void>; evaluate(fn: string | ((...a: any[]) => unknown), arg?: unknown): Promise<unknown> },
  C: unknown,
  jarPath: string,
  idToken: string,
): Promise<BrowserLoginResult | null> {
  try {
    // The hub POST needs the token owner's email: decode it from the JWT.
    // There is deliberately NO personal fallback - posting someone else's
    // address with your token misattributes the login server-side.
    let email = "";
    try {
      const parts = idToken.split(".");
      if (parts.length >= 2) {
        const payload = JSON.parse(Buffer.from(parts[1], "base64").toString("utf8"));
        if (typeof payload.email === "string") email = payload.email;
      }
    } catch { /* below */ }
    if (!email) {
      console.error("[login] cannot decode email from id_token; aborting hub POST (re-run login)");
      return null;
    }
    const postRaw = String(await P.evaluate(async (a: { csrf: string; email: string; idToken: string }) => {
      const csrfEl = document.querySelector('meta[name="csrf-token"]') as HTMLMetaElement | null;
      const csrf = (csrfEl || { content: "" }).content || "";
      const resp = await fetch("https://apps.unnes.ac.id/google/auth", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({ _token: csrf, email: a.email, id_token: a.idToken }),
      });
      return resp.text();
    }, { csrf: "", email, idToken }));
    let ok = false;
    try {
      const parsed = JSON.parse(postRaw) as { success?: boolean };
      ok = parsed.success === true;
    } catch { /* non-JSON */ }
    if (!ok) return null;
    await P.waitForTimeout(2500);
    await P.goto("https://apps.unnes.ac.id/gate/list", { waitUntil: "domcontentloaded", timeout: 60000 });
    await P.waitForTimeout(2500);
    const body = String(await P.evaluate(() => document.body.innerText.slice(0, 200)));
    if (/Login dengan UNNES-ID|Single Sign On/i.test(body)) return null;

    // Automatically prime Elena (App 30) session
    try {
      await P.goto("https://apps.unnes.ac.id/30", { waitUntil: "domcontentloaded", timeout: 30000 });
      await completeElenaSession(P as never, "20261");
    } catch { /* best effort */ }

    const jar = await CookieJar.load(jarPath);
    const captured = await syncJarFromContext(C, jar);
    await jar.save(jarPath);
    return { contract: 1, ok: true, mode: "browser", landingUrl: await P.url(), capturedCookies: captured };
  } catch {
    return null;
  }
}

async function waitForPopup(ctx: unknown, timeoutMs: number): Promise<unknown | null> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const pages = (ctx as { pages(): unknown[] }).pages();
    const found = pages.find((p) => (p as { url(): string }).url().includes("accounts.google.com"));
    if (found) return found;
    await new Promise((r) => setTimeout(r, 1000));
  }
  return null;
}

// ---------------------------------------------------------------------------
// op: submit - upload a file to an Elena (Moodle) assignment and optionally
// finalize the submission. The submission form is JS-rendered, so this runs in
// the persistent browser session (same SSO priming as op=page) and drives the
// filepicker with Playwright. Steps are logged (not returned) so failures are
// debuggable from stderr; the result reports the final status text.
// ---------------------------------------------------------------------------

export interface SubmitOpts {
  url: string;
  /** absolute path to the local file to upload */
  file: string;
  /** "draft" = Save changes only; "submit" = final Submit assignment */
  action: "draft" | "submit";
  ssoApp?: string;
  semester?: string;
  hubUrl?: string;
  waitMs?: number;
}

export interface SubmitResult {
  contract: number;
  ok: boolean;
  op: "submit";
  finalUrl: string | null;
  sessionExpired: boolean;
  message: string;
  /** verification flags read from the real page state (ok:true but all false
   * means the upload could not be confirmed - the CLI warns instead of
   * claiming success) */
  submitted: boolean;
  draft: boolean;
  hasFile: boolean;
  error?: { code: string; message: string };
  [k: string]: unknown;
}

export interface SubmitFinalState {
  submitted: boolean;
  draft: boolean;
  status: string;
  hasFile: boolean;
  url: string;
}

/** Human message for the verified end state. Exported for unit tests. */
export function describeSubmitState(s: SubmitFinalState): string {
  if (s.submitted) return "submitted for grading: " + s.status;
  if (s.draft || s.hasFile) return "file uploaded and saved as draft";
  return "upload finished but no submission state could be verified on the page"
    + " - open the assignment URL to confirm (it may still need a manual Save/Submit click,"
    + " or the cmid may point at a page without a submission form)";
}

export async function submitAssignment(jarPath: string, browserDir: string, opts: SubmitOpts): Promise<SubmitResult> {
  const base = {
    contract: 1, ok: false as boolean, op: "submit" as const,
    finalUrl: null as string | null, sessionExpired: false, message: "",
    submitted: false, draft: false, hasFile: false,
  };
  const log = (m: string) => console.error("[submit] " + m);
  if (process.env.UNNES_NO_BROWSER) {
    return { ...base, error: { code: "usage", message: "submit disabled via UNNES_NO_BROWSER (unset it to submit)" } };
  }
  // Stage tag: the outer catch reports WHERE it broke, so the CLI can tell
  // the user the cause instead of a bare "submit failed".
  let stage = "launch";
  let ctx: unknown;
  try {
    ctx = await launchContext(browserDir);
    await loadJarIntoContext(jarPath, ctx);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "usage", message: "submit failed at stage 'launch': " + message } };
  }
  // Ctrl+C / closed terminal mid-render must flush, not tear, the profile.
  const disarmShutdown = armShutdownGuard(async () => {
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  });
  let page: unknown;
  try {
    page = await (ctx as { newPage(): Promise<unknown> }).newPage();
    const P = page as {
      goto(u: string, o?: unknown): Promise<unknown>;
      url(): Promise<string>;
      content(): Promise<string>;
      click(s: string, o?: unknown): Promise<void>;
      waitForSelector(s: string, o?: unknown): Promise<unknown>;
      setInputFiles(s: string, f: string, o?: unknown): Promise<void>;
      evaluate(fn: string | ((...a: any[]) => unknown), arg?: unknown): Promise<unknown>;
      waitForTimeout(ms: number): Promise<void>;
    };
    // 1. prime the gateway app session (same as renderPage/crawl)
    stage = "prime";
    const hub = opts.hubUrl ?? "https://apps.unnes.ac.id";
    if (opts.ssoApp) {
      await P.goto(hub + "/" + opts.ssoApp, { waitUntil: "domcontentloaded", timeout: 60000 });
      await new Promise((r) => setTimeout(r, 6000));
      let u = "";
      try { u = await P.url(); } catch { /* closed */ }
      if (/\/(auth\/)?login/i.test(u)) {
        return { ...base, sessionExpired: true, error: { code: "session", message: "gateway session expired at stage 'prime'; run: unnes login" } };
      }
    }
    if (opts.ssoApp === "30") {
      await completeElenaSession(page as never, opts.semester ?? "20261");
    }

    // 2. open the assignment page
    stage = "open";
    await P.goto(opts.url, { waitUntil: "domcontentloaded", timeout: 60000 });
    await P.waitForTimeout(3000);
    const finalUrl = await P.url();
    log("opened " + finalUrl);

    // 3. session health
    const html0 = await P.content();
    const body0 = html0.replace(/<script[\s\S]*?<\/script>/gi, " ").replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim().slice(0, 400);
    const landedOnGateway = finalUrl.startsWith("https://apps.unnes.ac.id");
    if ((landedOnGateway && LOGIN_MARKERS.test(body0)) || /\/auth\/login/i.test(finalUrl) || MOODLE_LOGIN_MARKERS.test(body0)) {
      return { ...base, sessionExpired: true, finalUrl, error: { code: "session", message: "session expired at stage 'open' (assignment page shows a login screen); run: unnes login" } };
    }

    // 4. already-submitted guard: never touch a finalized submission
    const state1 = (await P.evaluate(() => ({
      alreadySubmitted: /submitted for grading|diserahkan untuk dinilai/i.test(document.body.innerText),
      inForm: /action=(addsubmission|editsubmission)/i.test(location.href),
      addSubText: [...document.querySelectorAll("button, input[type=submit], a")].map(e => { const x = e as HTMLElement & HTMLInputElement; return (x.innerText || x.value || "").trim(); }).filter(t => /add submission|tambah pengumpulan/i.test(t)).slice(0, 3),
    }))) as { alreadySubmitted: boolean; inForm: boolean; addSubText: string[] };
    log("state1: " + JSON.stringify(state1));
    if (state1.alreadySubmitted) {
      const msg = "tugas sudah dikumpulkan (file sudah ada di server - buka dengan Enter untuk melihat)";
      log(msg);
      return { ...base, ok: true, finalUrl, message: msg, submitted: true, hasFile: true };
    }

    // 5. open the submission form: click "Add submission" (a submit button
    //    with that label; the current Moodle render is a <button type=submit>).
    if (!state1.inForm) {
      const clicked = await P.evaluate(() => {
        const btn = [...document.querySelectorAll("button, input[type=submit], a")].find(e => { const x = e as HTMLElement & HTMLInputElement; return /add submission|tambah pengumpulan/i.test((x.innerText || x.value || "").trim()); });
        if (!btn) return false;
        (btn as HTMLElement).click();
        return true;
      });
      if (clicked) {
        await P.waitForTimeout(4000);
        log("clicked Add submission");
      } else {
        // form may already be expanded without the URL changing
        log("no Add submission button - form may be open already");
      }
    }

    // 6. open the filepicker dialog via its toolbar Add button, then POLL for
    //    the file input: the dialog renders asynchronously (0.5-3s), so a
    //    single count right after the click races the DOM.
    let fileInputs = (await P.evaluate(() => document.querySelectorAll('input[type="file"]').length)) as number;
    if (fileInputs === 0) {
      const opened = await P.evaluate(() => {
        const btn = document.querySelector(".fp-btn-add a[role=button], .fp-btn-add a, .fp-btn a[role=button], a[title='Add...']");
        if (!btn) return false;
        (btn as HTMLElement).click();
        return true;
      });
      log("filepicker opened: " + opened);
      if (opened) {
        for (let i = 0; i < 30; i++) {
          fileInputs = (await P.evaluate(() => document.querySelectorAll('input[type="file"]').length)) as number;
          if (fileInputs > 0) break;
          await P.waitForTimeout(500);
        }
      }
      log("file inputs after open: " + fileInputs);
    }

    // 7. attach the file
    const fileExists = await import("node:fs").then((m) => m.existsSync(opts.file));
    if (!fileExists) {
      return { ...base, finalUrl, error: { code: "usage", message: "file not found: " + opts.file } };
    }
    let uploaded = false;
    for (const sel of ['input[type="file"]', "#repo_upload_file", "#fileupload_form input[type=file]", "input[name='repo_upload_file']"]) {
      try {
        await P.setInputFiles(sel, opts.file, { timeout: 8000 });
        uploaded = true;
        log("set file on " + sel + " (" + opts.file + ")");
        break;
      } catch { /* selector not present */ }
    }
    if (!uploaded) {
      const msg = "could not attach the file at stage 'attach': the assignment page shows no file input."
        + " Likely causes: the submission is already finalized, it is past due (form closed),"
        + " the cmid points at a non-assignment page, or the form needs its 'Add submission'"
        + " button clicked first. Open " + opts.url + " in a browser to check.";
      log(msg);
      return { ...base, finalUrl, error: { code: "usage", message: msg } };
    }
    stage = "finalize";
    await P.waitForTimeout(2000);

    // 8. confirm the file is staged, then save/submit
    const staged = (await P.evaluate(() => { const m = document.body.innerText.match(/[^\n]*\.pdf|[^\n]*\.docx?|[^\n]*\.xlsx?|[^\n]*\.zip|[^\n]*\.png|[^\n]*\.jpg/g); return m ? m.slice(-1)[0] : ""; })) as string;
    log("staged file text: " + staged);

    // upload button in the picker dialog ("Upload this file")
    try {
      await P.click("#fileuploadbutton, .fp-upload-btn, button[data-action='upload'], input[value='Upload this file'], input[value='Upload']", { timeout: 5000 });
      await P.waitForTimeout(3000);
      log("clicked upload");
    } catch { /* file may attach without a separate upload step */ }

    const finalizeSel = opts.action === "submit"
      ? "input[name='submitbutton'], button[name='submitbutton'], button[data-action='submit'], input[value='Submit assignment']"
      : "input[name='saveandreturn'], button[name='saveandreturn'], input[name='saveandnext'], button[data-action='save-submission'], input[value='Save changes']";
    try {
      await P.click(finalizeSel, { timeout: 8000 });
      await P.waitForTimeout(4000);
      log("clicked " + (opts.action === "submit" ? "Submit assignment" : "Save changes"));
    } catch (err) {
      // the finalize button may be absent (some renders auto-save on the filepicker
      // upload); DON'T assume failure - fall through and read the real server state.
      log("finalize button not found - checking the resulting submission state");
    }

    // 9. report the resulting status from the ACTUAL page state. In this
    //    Moodle render, uploading through the filepicker can already SUBMIT the
    //    assignment (no separate Save/Submit button), and the page then
    //    redirects back to the summary view. So: poll for the summary view
    //    (URL without action=editsubmission AND a status table present), then
    //    read the status cell directly from the DOM instead of regexing the
    //    first 800 chars of a mid-navigation page.
    let finalState = { submitted: false, draft: false, status: "", hasFile: false, url: "" };
    for (let i = 0; i < 20; i++) {
      try {
        finalState = (await P.evaluate(() => {
          const url = location.href || "";
          const inEditForm = /action=(addsubmission|editsubmission)/i.test(url);
          // Moodle status table: first data cell after a "Submission status" row
          let status = "";
          const rows = document.querySelectorAll(".submissionstatustable tr, table.generaltable tr");
          for (const tr of rows) {
            const th = tr.querySelector("th");
            if (th && /submission status|status pengumpulan/i.test(th.textContent || "")) {
              const td = tr.querySelector("td");
              status = (td ? td.textContent || "" : "").trim();
              break;
            }
          }
          const bodyText = document.body ? document.body.innerText : "";
          const submitted = /submitted for grading|diserahkan untuk dinilai/i.test(status || bodyText.slice(0, 600));
          const draft = !submitted && (/submission draft|draft\b|saved as draft|belum dikumpulkan/i.test(status) || /edit submission|remove submission/i.test(bodyText));
          const hasFile = /pluginfile\.php\/.*assignsubmission_file|fileuploadsubmission|draft\sfile/i.test(document.body ? document.body.innerHTML : "") || /Laporan_[^\n]*\.pdf|[^\n]*\.pdf\n/i.test(bodyText);
          return { submitted, draft, status, hasFile, url: url.slice(0, 120) };
        })) as typeof finalState;
        // summary view = not in the edit form anymore; keep polling until we get there
        const notInEdit = !/action=(addsubmission|editsubmission)/i.test(finalState.url);
        if (notInEdit && finalState.status !== "") break;
      } catch { /* navigation in progress */ }
      await P.waitForTimeout(1000);
    }
    log("final state: " + JSON.stringify(finalState));
    stage = "verify";
    const message = describeSubmitState(finalState);
    const jar = await CookieJar.load(jarPath);
    const captured = await syncJarFromContext(ctx, jar);
    await jar.save(jarPath);
    return {
      ...base, ok: true, finalUrl: await P.url(), message, capturedCookies: captured,
      submitted: finalState.submitted, draft: finalState.draft, hasFile: finalState.hasFile,
    };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "internal", message: "submit failed at stage '" + stage + "': " + message } };
  } finally {
    disarmShutdown();
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  }
}

// ---------------------------------------------------------------------------
// op: open - open a URL in the PERSISTENT profile browser (headed) so the
// user sees the real logged-in page (session cookies live in the profile, not
// in the system default browser - which is why xdg-open lands on the SSO gate).
// Primes the gateway/elena session first, then keeps the window open until the
// user closes it or the deadline passes.
// ---------------------------------------------------------------------------

export interface OpenOpts {
  url: string;
  ssoApp?: string;
  semester?: string;
  hubUrl?: string;
  /** max ms to keep the window open; default 10 min */
  maxMs?: number;
}

export interface OpenResult {
  contract: number;
  ok: boolean;
  op: "open";
  finalUrl: string | null;
  sessionExpired: boolean;
  message: string;
  error?: { code: string; message: string };
  [k: string]: unknown;
}

export async function openInProfileBrowser(jarPath: string, browserDir: string, opts: OpenOpts): Promise<OpenResult> {
  const base = {
    contract: 1, ok: false as boolean, op: "open" as const,
    finalUrl: null as string | null, sessionExpired: false, message: ""
  };
  const log = (m: string) => console.error("[open] " + m);
  if (process.env.UNNES_NO_BROWSER) {
    return { ...base, error: { code: "usage", message: "open disabled via UNNES_NO_BROWSER" } };
  }
  let ctx: unknown;
  try {
    ctx = await launchContext(browserDir, false); // HEADED so the user sees it
    await loadJarIntoContext(jarPath, ctx);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "usage", message } };
  }
  // Ctrl+C / closed terminal mid-render must flush, not tear, the profile.
  const disarmShutdown = armShutdownGuard(async () => {
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  });
  let page: unknown;
  // 5 minutes default: short enough that other profile operations (submit,
  // page render) don't starve waiting for the lock, long enough to read a page.
  const deadline = Date.now() + (opts.maxMs ?? 5 * 60 * 1000);
  try {
    page = await (ctx as { newPage(): Promise<unknown> }).newPage();
    const P = page as {
      goto(u: string, o?: unknown): Promise<unknown>;
      url(): Promise<string>;
      waitForTimeout(ms: number): Promise<void>;
      isClosed(): boolean;
    };
    const hub = opts.hubUrl ?? "https://apps.unnes.ac.id";
    if (opts.ssoApp) {
      await P.goto(hub + "/" + opts.ssoApp, { waitUntil: "domcontentloaded", timeout: 60000 });
      await new Promise((r) => setTimeout(r, 6000));
      let u = "";
      try { u = await P.url(); } catch { /* closed */ }
      if (/\/(auth\/)?login/i.test(u)) {
        return { ...base, sessionExpired: true, error: { code: "session", message: "gateway session expired; run: unnes login" } };
      }
    }
    if (opts.ssoApp === "30") {
      await completeElenaSession(page as never, opts.semester ?? "20261");
    }
    await P.goto(opts.url, { waitUntil: "domcontentloaded", timeout: 60000 });
    log("opened " + opts.url + " in the profile browser");
    // keep the window open until the user closes it (or the deadline)
    while (Date.now() < deadline) {
      try {
        if (P.isClosed()) break;
      } catch { break; }
      await new Promise((r) => setTimeout(r, 1000));
    }
    let finalUrl = "";
    try { finalUrl = await P.url(); } catch { /* closed */ }
    const jar = await CookieJar.load(jarPath);
    const captured = await syncJarFromContext(ctx, jar);
    await jar.save(jarPath);
    return { ...base, ok: true, finalUrl: finalUrl || null, message: "opened; window closed by user", capturedCookies: captured };
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    return { ...base, error: { code: "internal", message: "open failed: " + message } };
  } finally {
    disarmShutdown();
    try { await (ctx as { close(): Promise<void> }).close(); } catch { /* already closed */ }
  }
}
