import { CookieJar } from "./cookiejar.js";
import type { PoliteLimiter } from "./polite.js";
import type { UrlValidators } from "./validators.js";
import { applyValidators, captureValidators } from "./validators.js";

export interface HttpResult {
  status: number;
  finalUrl: string;
  html: string;
  setCookie: string[];
  /** seconds from Retry-After header, when present */
  retryAfter: number | null;
  challenge: boolean;
  sessionExpired: boolean;
  /** HTTP 304: the caller's cached copy is still current (no body) */
  notModified: boolean;
  fetchError: { code: string; message: string } | null;
}

export interface RequestOptions {
  method: "GET" | "POST";
  url: string;
  body?: URLSearchParams;
  /** form _token; also sent as X-CSRF-TOKEN on POST */
  token?: string;
}

const MAX_REDIRECTS = 10;

export interface DownloadResult {
  status: number;
  finalUrl: string;
  /** raw bytes of the file */
  bytes: Uint8Array;
  contentType: string;
  /** from Content-Disposition, else the URL tail, else a fallback name */
  filename: string;
  sessionExpired: boolean;
  fetchError: { code: string; message: string } | null;
}

/** Refuse absurd downloads before they OOM the one-shot fetcher process. */
const MAX_DOWNLOAD_BYTES = 256 * 1024 * 1024;

export function filenameFrom(res: Response, finalUrl: string): string {
  const cd = res.headers.get("content-disposition") ?? "";
  // RFC 5987/8187: filename*=charset'lang'percent-encoded (charset varies).
  const star = cd.match(/filename\*\s*=\s*([^']*)'[^']*'([^;]+)/i);
  if (star) {
    try {
      const n = decodeURIComponent(star[2].trim().replace(/^"|"$/g, ""));
      if (n) return n;
    } catch { /* fall through */ }
  }
  const plain = cd.match(/filename\s*=\s*"([^"]+)"|filename\s*=\s*([^;]+)/i);
  const hit = (plain?.[1] ?? plain?.[2] ?? "").trim();
  if (hit) return hit;
  try {
    const tail = decodeURIComponent(new URL(finalUrl).pathname.split("/").filter(Boolean).pop() ?? "");
    if (tail && tail !== "view.php") return tail;
  } catch { /* fall through */ }
  return "materi-" + Date.now();
}

export function extFor(contentType: string): string {
  const ct = contentType.split(";")[0].trim().toLowerCase();
  const map: Record<string, string> = {
    "application/pdf": ".pdf",
    "application/zip": ".zip",
    "application/x-zip-compressed": ".zip",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document": ".docx",
    "application/msword": ".doc",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet": ".xlsx",
    "application/vnd.ms-excel": ".xls",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation": ".pptx",
    "image/png": ".png",
    "image/jpeg": ".jpg",
    "text/plain": ".txt",
    "text/csv": ".csv",
    "video/mp4": ".mp4",
    "application/vnd.ms-powerpoint": ".ppt",
    "application/octet-stream": ".bin",
  };
  return map[ct] ?? "";
}

export class HttpFetcher {
  constructor(
    private jar: CookieJar,
    private userAgent: string,
    private timeoutMs = 30000,
    private polite?: PoliteLimiter,
    private validators?: Record<string, UrlValidators>,
  ) {}

  /**
   * Perform a request with manual redirect handling.
   *
   * fetch() follows redirects internally but has no cookie store, so cookies
   * set by a 3xx response (e.g. the session cookie on a Laravel login POST)
   * would never reach the redirected request. Here we walk the chain hop by
   * hop: apply Set-Cookie to the jar after every response, and rebuild the
   * Cookie header before each hop.
   *
   * GET redirects are followed (up to MAX_REDIRECTS) so extraction sees the
   * real final page. POST redirects are NOT followed: the 3xx response is
   * returned as-is so the caller can inspect the Location header and the
   * response body (Laravel puts the error page on the 302).
   */
  async request(opts: RequestOptions): Promise<HttpResult> {
    let url = new URL(opts.url);
    let method = opts.method;
    let body = opts.body;
    let token = opts.token;
    let redirects = 0;
    const requested = url;
    // Rollback point: a redirect chain that ends on the gateway's login page
    // must not clobber the saved session with the anonymous cookies the
    // login page issues (that poisoned whole sessions - see tugas/elena).
    const jarSnap = this.jar.snapshot();

    // Hops already retried once after a 429 (keyed per hop so a redirect
    // chain with two rate-limited hops still retries each once, but a
    // persistently throttled hop never loops forever).
    const retried429 = new Set<string>();

    for (;;) {
      await this.polite?.waitFor(url.hostname);
      const headers: Record<string, string> = {
        "user-agent": this.userAgent,
        accept: "text/html,application/xhtml+xml,*/*;q=0.8",
        "accept-language": "en-US,en;q=0.9",
      };
      if (method === "POST") {
        headers["content-type"] = "application/x-www-form-urlencoded";
        const xsrf = this.jar.cookieValue(["XSRF-TOKEN", "xsrf-token"], url);
        // A corrupt jar value must not brick every POST: fall back to raw.
        if (xsrf) {
          try {
            headers["x-xsrf-token"] = decodeURIComponent(xsrf);
          } catch {
            headers["x-xsrf-token"] = xsrf;
          }
        }
        if (token) headers["x-csrf-token"] = token;
      }
      const jarHeader = this.jar.headerFor(url);
      if (jarHeader) headers["cookie"] = jarHeader;
      if (this.validators) applyValidators(headers, opts.url, this.validators);

      let res: Response;
      try {
        res = await fetch(url, {
          method,
          headers,
          body,
          redirect: "manual",
          signal: AbortSignal.timeout(this.timeoutMs),
        });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        const code = /abort|timeout/i.test(message) ? "timeout" : "network";
        return {
          status: 0,
          finalUrl: opts.url,
          html: "",
          setCookie: [],
          retryAfter: null,
          challenge: false,
          sessionExpired: false,
          notModified: false,
          fetchError: { code, message },
        };
      }

      const setCookie = this.collectSetCookie(res);
      this.jar.addFromSetCookie(setCookie, new URL(res.url));

      // 304: our cached copy is still current. No body, no session signal,
      // no validator recapture - the caller serves its cache instead.
      if (res.status === 304) {
        return {
          status: res.status,
          finalUrl: res.url,
          html: "",
          setCookie: [],
          retryAfter: null,
          challenge: false,
          sessionExpired: false,
          notModified: true,
          fetchError: null,
        };
      }

      // Rate limit: sleep Retry-After (capped) and retry the same hop once.
      // Login-style POSTs tolerate one same-hop retry; the alternative
      // (treating 429 as a hard failure) burns the whole run.
      const hopKey = method + " " + url.href;
      if (res.status === 429 && !retried429.has(hopKey)) {
        retried429.add(hopKey);
        const ra = Number(res.headers.get("retry-after"));
        const secs = Number.isFinite(ra) ? ra : null;
        this.polite?.noteResult(url.hostname, false, secs);
        const waitMs = secs !== null && secs > 0 ? Math.min(secs, 120) * 1000 : 0;
        if (waitMs > 0) await new Promise((r) => setTimeout(r, waitMs));
        continue;
      }
      if (res.status === 429) {
        const ra2 = Number(res.headers.get("retry-after"));
        return {
          status: res.status,
          finalUrl: res.url,
          html: "",
          setCookie: [],
          retryAfter: Number.isFinite(ra2) ? ra2 : null,
          challenge: false,
          sessionExpired: false,
          notModified: false,
          fetchError: { code: "ratelimit", message: "HTTP 429 for " + url.href },
        };
      }
      this.polite?.noteResult(url.hostname, true);

      const loc = res.headers.get("location");
      const isRedirect = res.status >= 300 && res.status < 400 && loc !== null;
      const follow = opts.method === "GET" && isRedirect && redirects < MAX_REDIRECTS;
      if (follow) {
        redirects += 1;
        // Fetch spec: 303 -> GET; 301/302 with POST -> GET; 307/308 keep method/body.
        if (res.status === 303 || (method === "POST" && (res.status === 301 || res.status === 302))) {
          method = "GET";
          body = undefined;
          token = undefined;
        }
        url = new URL(loc!, res.url);
        continue;
      }
      if (this.validators && res.status === 200) {
        captureValidators(opts.url, res, this.validators);
      }

      const html = await res.text();
      const finalUrl = isRedirect ? new URL(loc!, res.url).href : res.url;
      const final = new URL(finalUrl);
      const challenge = res.status === 403 && /cf-chl|just a moment|enable javascript|attention required/i.test(html);
      // Login pages: Laravel /auth/login and the gateway's /login.
      const isLoginPath = (p: string) => p === "/login" || p.startsWith("/login/") || p.startsWith("/auth/login") || p === "/login/index.php";
      const redirectedToLogin = isLoginPath(final.pathname) && !isLoginPath(requested.pathname);
      // Some portals answer expired sessions with HTTP 200 access-denied pages
      // (duanol/Sikadu: "tidak diberi hak untuk mengakses fitur ini [tamu]!").
      const denied = /tidak diberi hak untuk mengakses|\[tamu\]|sesi (anda )?berakhir/i.test(html);
      // A data-portal request that ends up on the GATEWAY hub means the
      // portal bounced us to the SSO login (elena/duanol do this when their
      // own session died). The hub issues anonymous session cookies that
      // would CLOBBER the saved gateway session - roll the jar back and
      // report the (sub-)session as expired so callers can re-prime it.
      const bouncedToGateway =
        final.hostname !== requested.hostname &&
        /(^|\.)apps\.unnes\.ac\.id$/.test(final.hostname);
      if (bouncedToGateway) {
        this.jar.restore(jarSnap);
      }
      const sessionExpired = redirectedToLogin || res.status === 401 || denied || bouncedToGateway;
      let retryAfter: number | null = null;
      const ra = res.headers.get("retry-after");
      if (ra) {
        const secs = Number(ra);
        retryAfter = Number.isFinite(secs) ? secs : null;
      }
      return {
        status: res.status,
        finalUrl,
        html,
        setCookie: bouncedToGateway ? [] : setCookie,
        retryAfter,
        challenge,
        sessionExpired,
        notModified: false,
        fetchError: null,
      };
    }
  }

  private collectSetCookie(res: Response): string[] {
    const setCookie: string[] = [];
    try {
      const h = res.headers as unknown as { getSetCookie?: () => string[] };
      if (typeof h.getSetCookie === "function") setCookie.push(...h.getSetCookie());
    } catch {
      /* headers API without getSetCookie: skip */
    }
    return setCookie;
  }

  async get(url: string): Promise<HttpResult> {
    return this.request({ method: "GET", url });
  }

  async post(url: string, body: URLSearchParams, token?: string): Promise<HttpResult> {
    return this.request({ method: "POST", url, body, token });
  }

  /**
   * Download a (binary) file with the jar session: same redirect + cookie
   * walk as request(), but the body is kept as bytes. Moodle resource links
   * (/mod/resource/view.php?id=..) usually redirect straight to the file;
   * when they land on an HTML confirmation page instead, the first
   * pluginfile link on that page is followed once.
   */
  async download(url: string): Promise<DownloadResult> {
    const fail = (code: string, message: string): DownloadResult => ({
      status: 0, finalUrl: url, bytes: new Uint8Array(), contentType: "",
      filename: "", sessionExpired: false, fetchError: { code, message },
    });
    const jarSnap = this.jar.snapshot();
    let current = url;
    let pluginFollowed = false;
    for (let hop = 0; hop <= MAX_REDIRECTS; hop++) {
      const jarHeader = this.jar.headerFor(new URL(current));
      const headers: Record<string, string> = {
        "user-agent": this.userAgent,
        accept: "*/*",
      };
      if (jarHeader) headers["cookie"] = jarHeader;
      let res: Response;
      try {
        res = await fetch(current, {
          method: "GET",
          headers,
          redirect: "manual",
          signal: AbortSignal.timeout(this.timeoutMs),
        });
      } catch (err) {
        const message = err instanceof Error ? err.message : String(err);
        return fail(/abort|timeout/i.test(message) ? "timeout" : "network", message);
      }
      const setCookie = this.collectSetCookie(res);
      this.jar.addFromSetCookie(setCookie, new URL(res.url));
      const loc = res.headers.get("location");
      if (res.status >= 300 && res.status < 400 && loc !== null) {
        current = new URL(loc, res.url).href;
        continue;
      }
      const contentType = res.headers.get("content-type") ?? "";
      const finalUrl = res.url;
      // Expired sessions surface as login pages (same shapes as request()),
      // including Moodle's own /login/index.php and 200 inline denials.
      const final = new URL(finalUrl);
      const requested = new URL(url);
      const isLoginPath = (p: string) =>
        p === "/login" || p.startsWith("/login/") || p.startsWith("/auth/login") || p === "/login/index.php";
      const bouncedToGateway =
        final.hostname !== requested.hostname &&
        /(^|\.)apps\.unnes\.ac\.id$/.test(final.hostname);
      if (bouncedToGateway) this.jar.restore(jarSnap);
      if (isLoginPath(final.pathname) || res.status === 401 || bouncedToGateway) {
        return {
          status: res.status, finalUrl, bytes: new Uint8Array(), contentType: "",
          filename: "", sessionExpired: true, fetchError: null,
        };
      }
      if (res.status < 200 || res.status >= 300) {
        return fail("network", "download failed with HTTP " + res.status + " for " + finalUrl);
      }
      if (contentType.includes("text/html") && !pluginFollowed) {
        // Confirmation page: follow the actual file link once. A 200 inline
        // access-denied page (expired Moodle session) is a session failure,
        // not a "no file link" usage error, so the caller can self-heal.
        const html = await res.text();
        if (/tidak diberi hak untuk mengakses|\[tamu\]|sesi (anda )?berakhir|you are not logged in|you must be logged in/i.test(html)) {
          return {
            status: res.status, finalUrl, bytes: new Uint8Array(), contentType: "",
            filename: "", sessionExpired: true, fetchError: null,
          };
        }
        const m = html.match(/href="([^"]*pluginfile\.php[^"]*)"/);
        if (m) {
          pluginFollowed = true;
          current = new URL(m[1].replace(/&amp;/g, "&"), finalUrl).href;
          continue;
        }
        return fail("usage", "download landed on an HTML page with no file link: " + finalUrl);
      }
      const len = Number(res.headers.get("content-length") ?? "0");
      if (Number.isFinite(len) && len > MAX_DOWNLOAD_BYTES) {
        return fail("usage", "file too large (" + Math.round(len / 1048576) + " MB, max 256 MB): " + finalUrl);
      }
      const buf = new Uint8Array(await res.arrayBuffer());
      if (buf.length > MAX_DOWNLOAD_BYTES) {
        return fail("usage", "file too large (" + Math.round(buf.length / 1048576) + " MB, max 256 MB): " + finalUrl);
      }
      let filename = filenameFrom(res, finalUrl).split("/").pop()!.split("\\").pop()!.trim();
      // Never allow directory traversal out of the destination (a hostile
      // Content-Disposition/URL tail like ".." would otherwise escape it).
      if (filename === "." || filename === ".." || filename.includes("/") || filename.includes("\\")) {
        return fail("usage", "refusing unsafe server filename for " + finalUrl);
      }
      if (!/\.[A-Za-z0-9]{1,8}$/.test(filename)) filename += extFor(contentType);
      if (!filename) filename = "materi-" + Date.now();
      return { status: res.status, finalUrl, bytes: buf, contentType, filename, sessionExpired: false, fetchError: null };
    }
    return fail("network", "too many redirects downloading " + url);
  }
}
