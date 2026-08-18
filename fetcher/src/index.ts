import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { CookieJar } from "./cookiejar.js";
import { HttpFetcher } from "./http.js";
import { LoginForm, opLogin, opLogout } from "./login.js";
import { normalizeHtml } from "./normalize.js";
import { ExtractSpec, extractRecords } from "./extract.js";

export interface Job {
  contract: number;
  op: "get" | "login" | "logout";
  profile?: string;
  /** used by login/logout when the job URL family is not explicit */
  baseUrl?: string;
  url?: string;
  /** login: "form" (legacy email/password, default) or "browser" (Google SSO via Playwright) */
  mode?: "form" | "browser";
  form?: LoginForm;
  extract?: ExtractSpec;
  extraRegexes?: string[];
}

export interface JobResult {
  [k: string]: unknown;
}

const CONTRACT = 1;
const DEFAULT_BASE = "https://student.unnes.ac.id";

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
    // full re-auth. Only used by op=login mode=browser.
    browserDir: join(home, "browser-profiles", profile),
  };
}

export async function processJob(job: Job): Promise<JobResult> {
  if (job.contract !== CONTRACT) return fail("contract", "unsupported contract version " + String(job.contract));
  const { profilePath, browserDir } = await envPaths();
  const jar = await CookieJar.load(profilePath);
  const ua = process.env.UNNES_USER_AGENT ?? "unnes-cli/0.1";
  const f = new HttpFetcher(jar, ua);
  const baseUrl = (job.baseUrl ?? DEFAULT_BASE).replace(/\/+$/, "");

  switch (job.op) {
    case "get": {
      if (!job.url) return fail("usage", "op=get requires url");
      const res = await f.request({ method: "GET", url: job.url });
      if (res.fetchError) return fail(res.fetchError.code, res.fetchError.message);
      const records = job.extract ? extractRecords(res.html, job.extract) : [];
      const normalized = normalizeHtml(res.html, job.extraRegexes ?? []);
      return {
        contract: CONTRACT, op: "get", ok: true, status: res.status, finalUrl: res.finalUrl,
        sessionExpired: res.sessionExpired, challenge: res.challenge, retryAfter: res.retryAfter,
        records, normalized,
      };
    }
    case "login": {
      if (job.mode === "browser") {
        // Google SSO lives on the apps.unnes.ac.id hub - never job.baseUrl,
        // which is the DATA portal and has no Google sign-in.
        const { browserLogin } = await import("./browser.js");
        return browserLogin(profilePath, browserDir);
      }
      if (!job.form) return fail("usage", "op=login requires form{email,password}");
      return opLogin(f, baseUrl, profilePath, jar, job.form);
    }
    case "logout":
      return opLogout(f, baseUrl, jar, profilePath);
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
