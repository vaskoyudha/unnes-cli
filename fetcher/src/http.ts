import { CookieJar } from "./cookiejar.js";

export interface HttpResult {
  status: number;
  finalUrl: string;
  html: string;
  setCookie: string[];
  /** seconds from Retry-After header, when present */
  retryAfter: number | null;
  challenge: boolean;
  sessionExpired: boolean;
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

export class HttpFetcher {
  constructor(
    private jar: CookieJar,
    private userAgent: string,
    private timeoutMs = 30000,
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

    for (;;) {
      const headers: Record<string, string> = {
        "user-agent": this.userAgent,
        accept: "text/html,application/xhtml+xml,*/*;q=0.8",
        "accept-language": "en-US,en;q=0.9",
      };
      if (method === "POST") {
        headers["content-type"] = "application/x-www-form-urlencoded";
        const xsrf = this.jar.cookieValue(["XSRF-TOKEN", "xsrf-token"], url);
        if (xsrf) headers["x-xsrf-token"] = decodeURIComponent(xsrf);
        if (token) headers["x-csrf-token"] = token;
      }
      const jarHeader = this.jar.headerFor(url);
      if (jarHeader) headers["cookie"] = jarHeader;

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
          fetchError: { code, message },
        };
      }

      const setCookie = this.collectSetCookie(res);
      this.jar.addFromSetCookie(setCookie, new URL(res.url));

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

      const html = await res.text();
      const finalUrl = isRedirect ? new URL(loc!, res.url).href : res.url;
      const final = new URL(finalUrl);
      const challenge = res.status === 403 && /cf-chl|just a moment|enable javascript|attention required/i.test(html);
      // Login pages: Laravel /auth/login and the gateway's /login.
      const isLoginPath = (p: string) => p === "/login" || p.startsWith("/login/") || p.startsWith("/auth/login");
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
}
