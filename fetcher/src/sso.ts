// SSO exchange: turn the gateway session into an app session.
//
// The gateway (apps.unnes.ac.id) embeds each app behind a short-lived JWT
// (sso_token, ~10 min) rendered into the app page (sikaduV3Beta.render etc).
// Each target app exposes an exchange endpoint that accepts the token and
// sets its own session cookie. We replicate the exchange over plain HTTP.
//
// Verified against the live portal (2026-08):
//   app 76 (Sikadu V3-Beta) -> akademik.unnes.ac.id/auth/sso_login
//       GET (primes session) then POST (validates token, 302 /dashboard)
//   app 64 (MyUNNES-Student) -> student.unnes.ac.id/auth/sso_login
//   app 30 (Elena) -> elena.unnes.ac.id/portal/apis/sso
//       GET only; partial (browser iframe flow still needed for full auth)

import { CookieJar } from "./cookiejar.js";
import { HttpFetcher } from "./http.js";

export interface SsoConfig {
  appId: string;
  /** exchange endpoint on the target subdomain */
  loginUrl: string;
  /** POST the token, or pass it as a query param (GET) */
  method: "get" | "get-post";
}

const APP_TABLE: Record<string, SsoConfig> = {
  "76": { appId: "76", loginUrl: "https://akademik.unnes.ac.id/auth/sso_login", method: "get-post" },
  "64": { appId: "64", loginUrl: "https://student.unnes.ac.id/auth/sso_login", method: "get-post" },
  "30": { appId: "30", loginUrl: "https://elena.unnes.ac.id/portal/apis/sso", method: "get" },
};

/** Map a data-subdomain host to the gateway app that gates it. */
export function appForHost(host: string): SsoConfig | null {
  if (host === "akademik.unnes.ac.id") return APP_TABLE["76"];
  if (host === "student.unnes.ac.id") return APP_TABLE["64"];
  if (host === "elena.unnes.ac.id") return APP_TABLE["30"];
  return null;
}

/** Extract the sso_token from a gateway app page (the render call). */
export function extractSsoToken(html: string): string | null {
  const m = html.match(/sso_token\s*:\s*'([^']+)'/) || html.match(/sso_token\s*:\s*"([^"]+)"/);
  return m ? m[1] : null;
}

export interface SsoResult {
  contract: number;
  ok: boolean;
  op: "sso";
  appId: string;
  target: string;
  cookiesSet: string[];
  error?: { code: string; message: string };
  [k: string]: unknown;
}

/**
 * Exchange the gateway token for an app session. Returns ok:false with code
 * "session" when the gateway session itself is expired (needs unnes login).
 */
export async function opSso(
  profilePath: string,
  gatewayUrl: string,
  appId: string,
): Promise<SsoResult> {
  const base = { contract: 1, ok: false as boolean, op: "sso" as const, appId, target: "", cookiesSet: [] as string[] };
  const cfg = APP_TABLE[appId];
  if (!cfg) return { ...base, error: { code: "usage", message: "unknown gateway app id: " + appId } };
  base.target = cfg.loginUrl;

  const jar = await CookieJar.load(profilePath);
  const f = new HttpFetcher(jar, "unnes-cli/0.1");

  // 1. Fetch the gateway app page for a fresh token.
  const page = await f.get(gatewayUrl + "/" + appId);
  if (page.fetchError) return { ...base, error: { code: page.fetchError.code, message: page.fetchError.message } };
  if (page.sessionExpired || /auth\/login/i.test(page.finalUrl)) {
    return { ...base, error: { code: "session", message: "gateway session expired; run: unnes login" } };
  }
  const token = extractSsoToken(page.html);
  if (!token) return { ...base, error: { code: "csrf", message: "no sso_token found on gateway app page " + appId } };

  // 2. Exchange at the target.
  if (cfg.method === "get") {
    const r = await f.get(cfg.loginUrl + "?sso_token=" + encodeURIComponent(token));
    if (r.fetchError) return { ...base, error: { code: r.fetchError.code, message: r.fetchError.message } };
  } else {
    // akademik/student: GET primes the session, POST validates the token.
    const g = await f.get(cfg.loginUrl + "?sso_token=" + encodeURIComponent(token));
    if (g.fetchError) return { ...base, error: { code: g.fetchError.code, message: g.fetchError.message } };
    const p = await f.post(cfg.loginUrl, new URLSearchParams({ sso_token: token }));
    if (p.fetchError) return { ...base, error: { code: p.fetchError.code, message: p.fetchError.message } };
  }

  await jar.save(profilePath);
  const names = jar.cookieNames ? jar.cookieNames() : [];
  return { ...base, ok: true, cookiesSet: names };
}
