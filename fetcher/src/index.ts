import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { CookieJar } from "./cookiejar.js";
import { HttpFetcher } from "./http.js";
import { LoginForm, opLogin, opLogout } from "./login.js";
import { normalizeHtml } from "./normalize.js";
import { ExtractSpec, extractRecords } from "./extract.js";

export interface Job {
  contract: number;
  op: "get" | "login" | "logout" | "sso" | "page" | "crawl" | "batch";
  profile?: string;
  /** used by login/logout when the job URL family is not explicit */
  baseUrl?: string;
  url?: string;
  /** login: "form" (legacy email/password, default) or "browser" (Google SSO via Playwright) */
  mode?: "form" | "browser" | "auto";
  /** login mode=auto: never open an INTERACTIVE window - scripted attempts
   * only (used by callers that must not block on a human click, e.g. the TUI) */
  interactive?: boolean;
  form?: LoginForm;
  extract?: ExtractSpec;
  extraRegexes?: string[];
  /** op=sso: gateway app id (76 = akademik, 64 = student, 30 = elena) */
  appId?: string;
  /** op=page: gateway app to prime before rendering (e.g. "76", "30") */
  ssoApp?: string;
  /** op=page: max ms to wait for the extract selector */
  waitMs?: number;
  /** op=crawl: selector yielding the <a> links to follow */
  linkSelector?: string;
  /** op=crawl: max links to follow */
  maxLinks?: number;
  /** op=page/crawl: URL to visit before the target (e.g. semester switcher) */
  preUrl?: string;
  /** op=page/crawl (elena): semester to open after SSO, default 20261 */
  semester?: string;
  /** op=batch: entries to render in one shared browser session */
  entries?: {
    url: string;
    ssoApp?: string;
    preUrl?: string;
    semester?: string;
    extract?: ExtractSpec;
    waitMs?: number;
  }[];
}

export interface JobResult {
  [k: string]: unknown;
}

const CONTRACT = 1;
const DEFAULT_BASE = "https://apps.unnes.ac.id";

function fail(code: string, message: string): JobResult {
  return { contract: CONTRACT, ok: false, error: { code, message } };
}

async function envPaths(): Promise<{ profilePath: string; browserDir: string }> {
  const home = process.env.UNNES_HOME ?? join(process.env.HOME ?? ".", ".config", "unnes");
  const profile = process.env.UNNES_PROFILE ?? "default";
  return {
    profilePath: join(home, "profiles", profile + ".json"),
    // Persistent Chromium profile: keeps the Google sign-in state (account
    // choice, 2FA trust) so later browser logins are one click instead of a
    // full re-auth. Used by op=login mode=browser and op=page rendering.
    browserDir: join(home, "browser-profiles", profile),
  };
}

/** Run an sso exchange (gateway token -> app session) for the given app. */
async function refreshAppSession(
  profilePath: string,
  baseUrl: string,
  appId: string,
): Promise<{ ok: boolean; code?: string; message?: string }> {
  const { opSso } = await import("./sso.js");
  const res = await opSso(profilePath, baseUrl, appId);
  return res.ok ? { ok: true } : { ok: false, code: res.error?.code, message: res.error?.message };
}

export async function processJob(job: Job): Promise<JobResult> {
  if (job.contract !== CONTRACT) return fail("contract", "unsupported contract version " + String(job.contract));
  const { profilePath, browserDir } = await envPaths();
  const ua = process.env.UNNES_USER_AGENT ?? "unnes-cli/0.1";
  const baseUrl = (job.baseUrl ?? DEFAULT_BASE).replace(/\/+$/, "");

  switch (job.op) {
    case "get": {
      if (!job.url) return fail("usage", "op=get requires url");
      const doGet = async (): Promise<JobResult> => {
        const jar = await CookieJar.load(profilePath);
        const f = new HttpFetcher(jar, ua);
        const res = await f.request({ method: "GET", url: job.url! });
        if (res.fetchError) return fail(res.fetchError.code, res.fetchError.message);
        const records = job.extract ? extractRecords(res.html, job.extract) : [];
        const normalized = normalizeHtml(res.html, job.extraRegexes ?? []);
        return {
          contract: CONTRACT, op: "get", ok: true, status: res.status, finalUrl: res.finalUrl,
          sessionExpired: res.sessionExpired, challenge: res.challenge, retryAfter: res.retryAfter,
          records, normalized,
        };
      };
      let result = await doGet();
      // Auto SSO bootstrap: the session expired for a known data subdomain -
      // exchange the gateway token once, then retry.
      if (result.ok && result.sessionExpired === true && job.url) {
        const { appForHost } = await import("./sso.js");
        try {
          const cfg = appForHost(new URL(job.url).hostname);
          if (cfg) {
            const refreshed = await refreshAppSession(profilePath, baseUrl, cfg.appId);
            if (refreshed.ok) {
              const retry = await doGet();
              if (retry.ok && retry.sessionExpired !== true) {
                retry.ssoRefreshed = true;
                return retry;
              }
            }
          }
        } catch { /* fall through to the original result */ }
      }
      return result;
    }
    case "sso": {
      if (!job.appId) return fail("usage", "op=sso requires appId");
      const { opSso } = await import("./sso.js");
      return opSso(profilePath, baseUrl, job.appId);
    }
    case "page": {
      if (!job.url) return fail("usage", "op=page requires url");
      const { renderPage } = await import("./browser.js");
      return renderPage(profilePath, browserDir, {
        url: job.url,
        ssoApp: job.ssoApp,
        preUrl: job.preUrl,
        semester: job.semester,
        extract: job.extract,
        waitMs: job.waitMs,
      });
    }
    case "batch": {
      if (!job.entries || job.entries.length === 0) {
        return fail("usage", "op=batch requires entries");
      }
      const { batchPages } = await import("./browser.js");
      const result = await batchPages(profilePath, browserDir, job.entries);
      return result;
    }
    case "crawl": {
      if (!job.url || !job.linkSelector || !job.extract) {
        return fail("usage", "op=crawl requires url, linkSelector and extract");
      }
      const { crawlPage } = await import("./browser.js");
      return crawlPage(profilePath, browserDir, {
        startUrl: job.url,
        linkSelector: job.linkSelector,
        pageExtract: job.extract,
        ssoApp: job.ssoApp,
        preUrl: job.preUrl,
        semester: job.semester,
        waitMs: job.waitMs,
        maxLinks: job.maxLinks,
      });
    }
    case "login": {
      if (job.mode === "browser") {
        // Google SSO lives on the apps.unnes.ac.id hub - never job.baseUrl,
        // which is the data portal and has no Google sign-in.
        const { browserLogin } = await import("./browser.js");
        return browserLogin(profilePath, browserDir);
      }
      if (job.mode === "auto") {
        // Scripted headless re-login with the saved profile; falls back to
        // needsInteraction when Google asks for password/2FA/CAPTCHA -
        // unless interactive:false, in which case it stops there instead of
        // opening a window that waits for a human click.
        const { autoLogin } = await import("./browser.js");
        return autoLogin(profilePath, browserDir, { interactive: job.interactive ?? true });
      }
      if (!job.form) return fail("usage", "op=login requires form{email,password}");
      const jar = await CookieJar.load(profilePath);
      const f = new HttpFetcher(jar, ua);
      return opLogin(f, baseUrl, profilePath, jar, job.form);
    }
    case "logout": {
      const jar = await CookieJar.load(profilePath);
      const f = new HttpFetcher(jar, ua);
      return opLogout(f, baseUrl, jar, profilePath);
    }
    default:
      return fail("usage", "unknown op " + String((job as { op?: string }).op));
  }
}

async function main(): Promise<void> {
  let raw = "";
  process.stdin.setEncoding("utf8");
  for await (const chunk of process.stdin) raw += chunk;
  let job: Job;
  try {
    job = JSON.parse(raw);
  } catch {
    return void console.log(JSON.stringify(fail("usage", "stdin is not valid JSON")));
  }
  try {
    const result = await processJob(job);
    console.log(JSON.stringify(result));
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    console.log(JSON.stringify(fail("internal", message)));
  }
}

const isMain = process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (isMain) {
  void main();
}
