import { readFile, writeFile, rename, chmod, mkdir } from "node:fs/promises";
import { dirname } from "node:path";

export interface StoredCookie {
  name: string;
  value: string;
  domain: string;
  path: string;
  secure: boolean;
  httpOnly: boolean;
  /** epoch ms; null = session cookie */
  expires: number | null;
}

interface JarFile {
  version: number;
  cookies: StoredCookie[];
}

const JAR_VERSION = 1;

function parseSetCookie(line: string): StoredCookie | null {
  const parts = line.split(";").map((p) => p.trim());
  const first = parts.shift();
  if (!first) return null;
  const eq = first.indexOf("=");
  if (eq <= 0) return null;
  const cookie: StoredCookie = { name: first.slice(0, eq).trim(), value: first.slice(eq + 1).trim(), domain: "", path: "/", secure: false, httpOnly: false, expires: null };
  for (const attr of parts) {
    const [k, ...rest] = attr.split("=");
    const key = k.trim().toLowerCase();
    const val = rest.join("=").trim();
    switch (key) {
      case "domain": cookie.domain = val.toLowerCase(); break;
      case "path": cookie.path = val || "/"; break;
      case "secure": cookie.secure = true; break;
      case "httponly": cookie.httpOnly = true; break;
      case "expires": { const t = Date.parse(val); if (!Number.isNaN(t)) cookie.expires = t; break; }
      case "max-age": { const n = Number(val); if (Number.isFinite(n)) cookie.expires = Date.now() + n * 1000; break; }
      default: break;
    }
  }
  return cookie;
}

function domainMatches(cookieDomain: string, host: string): boolean {
  if (!cookieDomain) return true;
  return host === cookieDomain || host.endsWith("." + cookieDomain);
}

export class CookieJar {
  private constructor(private cookies: StoredCookie[] = []) {}

  static empty(): CookieJar {
    return new CookieJar();
  }

  static async load(path: string): Promise<CookieJar> {
    try {
      const raw = await readFile(path, "utf8");
      const parsed = JSON.parse(raw) as JarFile;
      if (parsed.version !== JAR_VERSION || !Array.isArray(parsed.cookies)) return CookieJar.empty();
      const now = Date.now();
      return new CookieJar(parsed.cookies.filter((c) => c.expires === null || c.expires > now));
    } catch {
      return CookieJar.empty();
    }
  }

  async save(path: string): Promise<void> {
    await mkdir(dirname(path), { recursive: true });
    const tmp = path + ".tmp";
    const now = Date.now();
    const data: JarFile = {
      version: JAR_VERSION,
      cookies: this.cookies.filter((c) => c.expires === null || c.expires > now),
    };
    await writeFile(tmp, JSON.stringify(data));
    await chmod(tmp, 0o600);
    await rename(tmp, path);
  }

  /** Apply Set-Cookie headers from a response to the request URL. */
  addFromSetCookie(setCookie: string[], requestUrl: URL): void {
    for (const line of setCookie) {
      const c = parseSetCookie(line);
      if (!c) continue;
      if (!c.domain) c.domain = requestUrl.hostname;
      if (!domainMatches(c.domain, requestUrl.hostname)) continue;
      if (c.expires !== null && c.expires <= Date.now()) continue;
      if (c.secure && requestUrl.protocol !== "https:") continue;
      this.cookies = this.cookies.filter((x) => !(x.name === c.name && x.domain === c.domain && x.path === c.path));
      this.cookies.push(c);
    }
  }

  /** Cookie header value for a request URL, or null when empty. */
  headerFor(url: URL): string | null {
    const parts: string[] = [];
    for (const c of this.cookies) {
      if (c.expires !== null && c.expires <= Date.now()) continue;
      if (c.secure && url.protocol !== "https:") continue;
      if (!domainMatches(c.domain, url.hostname)) continue;
      if (!url.pathname.startsWith(c.path)) continue;
      parts.push(c.name + "=" + c.value);
    }
    return parts.length ? parts.join("; ") : null;
  }

  /** First value among the named cookies (used for XSRF-TOKEN lookups). */
  cookieValue(names: string[], url: URL): string | null {
    for (const c of this.cookies) {
      if (names.includes(c.name) && domainMatches(c.domain, url.hostname)) return c.value;
    }
    return null;
  }

  clear(): void {
    this.cookies = [];
  }
}

export { JAR_VERSION };
