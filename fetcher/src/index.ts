import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { CookieJar } from "./cookiejar.js";
import { HttpFetcher } from "./http.js";
import { PoliteLimiter } from "./polite.js";
import { mapLimit } from "./pool.js";
import { loadValidators, saveValidators } from "./validators.js";
import { LoginForm, opLogin, opLogout } from "./login.js";
import { normalizeHtml } from "./normalize.js";
import { ExtractSpec, extractRecords } from "./extract.js";

// UNNES servers (specifically elena.unnes.ac.id) carry an incomplete intermediate
// SSL certificate chain, which throws UNABLE_TO_VERIFY_LEAF_SIGNATURE in Node fetch.
process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
process.env.NODE_NO_WARNINGS = "1";

export interface Job {
  contract: number;
  op: "get" | "login" | "logout" | "sso" | "page" | "crawl" | "batch" | "submit" | "open" | "download" | "batchget";
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
  /** op=submit: file to upload to an Elena assignment (absolute path) */
  file?: string;
  /** op=download: directory the downloaded file is saved into */
  out?: string;
  /** op=submit: "draft" (default) or "submit" (finalize) */
  action?: "draft" | "submit";
  /** op=open: max ms to keep the profile browser window open (default 10 min) */
  maxMs?: number;
  /** op=batch: entries to render in one shared browser session */
  entries?: {
    url: string;
    ssoApp?: string;
    preUrl?: string;
    semester?: string;
    extract?: ExtractSpec;
    waitMs?: number;
  }[];
  /** op=batchget: plain-HTTP GETs served in one spawn sharing one jar.
   * Per entry: url + optional extract/extraRegexes. `concurrency` caps
   * in-flight data GETs (default 1 until the pool lands; prime/SSO passes
   * always run sequentially). */
  urls?: {
    url: string;
    extract?: ExtractSpec;
    extraRegexes?: string[];
  }[];
  concurrency?: number;
}

export interface JobResult {
  [k: string]: unknown;
}

const CONTRACT = 1;
const DEFAULT_BASE = "https://apps.unnes.ac.id";

function fail(code: string, message: string): JobResult {
  return { contract: CONTRACT, ok: false, error: { code, message } };
}

async function envPaths(jobProfile?: string): Promise<{ profilePath: string; browserDir: string }> {
  // Resolution mirrors the Rust side (UNNES_HOME > XDG_CONFIG_HOME/unnes >
  // ~/.config/unnes). The job envelope ALSO carries `profile`: env wins when
  // set (the CLI always injects it), the job field is the fallback so direct
  // node callers address the intended profile instead of "default".
  const home = process.env.UNNES_HOME
    ?? (process.env.XDG_CONFIG_HOME ? join(process.env.XDG_CONFIG_HOME, "unnes") : null)
    ?? join(process.env.HOME ?? ".", ".config", "unnes");
  const profile = process.env.UNNES_PROFILE ?? jobProfile ?? "default";
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
  const { profilePath, browserDir } = await envPaths(job.profile);
  const ua = process.env.UNNES_USER_AGENT ?? "unnes-cli/0.1";
  const baseUrl = (job.baseUrl ?? DEFAULT_BASE).replace(/\/+$/, "");

  switch (job.op) {
    case "get": {
      if (!job.url) return fail("usage", "op=get requires url");
      const doGet = async (): Promise<JobResult> => {
        const jar = await CookieJar.load(profilePath);
        const store = await loadValidators(profilePath);
        const f = new HttpFetcher(jar, ua, undefined, undefined, store);
        const res = await f.request({ method: "GET", url: job.url! });
        if (res.fetchError) return fail(res.fetchError.code, res.fetchError.message);
        // Persist slides/rotations from plain HTTP too (previously only
        // browser/sso paths saved, so sliding sessions looked expired early).
        // Skipped on expiry: the jar was rolled back to pre-request state and
        // saving would only rewrite it. Validators ride along (a bounced
        // login page carries no usable validators for the requested URL).
        if (!res.sessionExpired) {
          await jar.save(profilePath);
          await saveValidators(profilePath, store);
        }
        const records = job.extract ? extractRecords(res.html, job.extract) : [];
        const normalized = normalizeHtml(res.html, job.extraRegexes ?? []);
        return {
          contract: CONTRACT, op: "get", ok: true, status: res.status, finalUrl: res.finalUrl,
          sessionExpired: res.sessionExpired, challenge: res.challenge, retryAfter: res.retryAfter,
          notModified: res.notModified, records, normalized,
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
            } else {
              // Don't swallow the refresh cause: without it every downstream
              // failure looks like a plain expiry with no remediation trail.
              (result as Record<string, unknown>).ssoRefreshError =
                refreshed.code + ": " + refreshed.message;
            }
          }
        } catch (e) {
          (result as Record<string, unknown>).ssoRefreshError =
            "refresh attempt failed: " + (e instanceof Error ? e.message : String(e)).slice(0, 200);
        }
      }
      return result;
    }
    case "batchget": {
      if (!job.urls || job.urls.length === 0) return fail("usage", "op=batchget requires urls[]");
      // Polite ceiling for one portal: 3 in flight by default (research:
      // 2-3 concurrent + 1-2s floor for a university host). Single jar
      // object shared by all workers - safe on one thread (all jar
      // mutations are synchronous), one limiter shared for the floor.
      const conc = Math.min(6, Math.max(1, Math.floor(job.concurrency ?? 3)));
      // One jar, one limiter, one save for the whole batch: N URLs for the
      // price of one node spawn, with keep-alive reuse inside the process.
      const polite = new PoliteLimiter();
      let jar = await CookieJar.load(profilePath);
      const store = await loadValidators(profilePath);
      const newFetcher = () => new HttpFetcher(jar, ua, undefined, polite, store);
      let f = newFetcher();
      const runGet = async (entry: { url: string; extract?: ExtractSpec; extraRegexes?: string[] }): Promise<JobResult> => {
        const res = await f.request({ method: "GET", url: entry.url });
        if (res.fetchError) return { url: entry.url, ok: false, error: { code: res.fetchError.code, message: res.fetchError.message } };
        const records = entry.extract ? extractRecords(res.html, entry.extract) : [];
        const normalized = normalizeHtml(res.html, entry.extraRegexes ?? []);
        return {
          url: entry.url, ok: true, status: res.status, finalUrl: res.finalUrl,
          sessionExpired: res.sessionExpired, challenge: res.challenge, retryAfter: res.retryAfter,
          notModified: res.notModified, records, normalized,
        };
      };
      const { appForHost } = await import("./sso.js");
      // Pass A: data fetch in parallel. Pass B (below) stays sequential:
      // an SSO refresh rewrites the jar on disk and our in-memory view must
      // reload before retrying - concurrent reloads would race.
      let results = await mapLimit(job.urls, conc, (entry) => runGet(entry));
      for (let i = 0; i < job.urls.length; i++) {
        const entry = job.urls[i];
        let r = results[i];
        // Auto SSO bootstrap per expired entry (same rule as op=get): the
        // refresh rewrites the jar on disk, so reload our in-memory view
        // before retrying - saving the stale view later would clobber it.
        if (r.ok && r.sessionExpired === true) {
          try {
            const cfg = appForHost(new URL(entry.url).hostname);
            if (cfg) {
              const refreshed = await refreshAppSession(profilePath, baseUrl, cfg.appId);
              if (refreshed.ok) {
                jar = await CookieJar.load(profilePath);
                f = newFetcher();
                r = await runGet(entry);
                if (r.ok && r.sessionExpired !== true) {
                  (r as Record<string, unknown>).ssoRefreshed = true;
                }
              } else {
                (r as Record<string, unknown>).ssoRefreshError =
                  refreshed.code + ": " + refreshed.message;
              }
            }
          } catch (e) {
            (r as Record<string, unknown>).ssoRefreshError =
              "refresh attempt failed: " + (e instanceof Error ? e.message : String(e)).slice(0, 200);
          }
        }
        results[i] = r;
      }
      // Same save rule as op=get: skipped only when every entry expired
      // (the jar was rolled back to pre-request state; saving rewrites it).
      if (results.some((r) => r.ok === true && (r as Record<string, unknown>).sessionExpired !== true)) {
        await jar.save(profilePath);
        await saveValidators(profilePath, store);
      }
      return {
        contract: CONTRACT, op: "batchget", ok: true,
        sessionExpired: results.some((r) => (r as Record<string, unknown>).sessionExpired === true),
        results,
      };
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
    case "open": {
      if (!job.url) return fail("usage", "op=open requires url");
      const { openInProfileBrowser } = await import("./browser.js");
      return openInProfileBrowser(profilePath, browserDir, {
        url: job.url,
        ssoApp: job.ssoApp,
        semester: job.semester,
        maxMs: job.maxMs,
      });
    }
    case "submit": {
      if (!job.url || !job.file) {
        return fail("usage", "op=submit requires url and file");
      }
      const { submitAssignment } = await import("./browser.js");
      return submitAssignment(profilePath, browserDir, {
        url: job.url,
        file: job.file,
        action: job.action ?? "draft",
        ssoApp: job.ssoApp,
        semester: job.semester,
        waitMs: job.waitMs,
      });
    }
    case "download": {
      if (!job.url || !job.out) {
        return fail("usage", "op=download requires url and out (directory)");
      }
      const { opDownload } = await import("./download.js");
      return opDownload(profilePath, ua, job.url, job.out);
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
