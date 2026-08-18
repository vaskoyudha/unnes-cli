import { HttpFetcher } from "./http.js";
import { CookieJar } from "./cookiejar.js";

export interface LoginForm {
  email: string;
  password: string;
}

function extractToken(html: string): string | null {
  const m = html.match(/<input[^>]*name=["']?_token["']?[^>]*value=["']([^"']+)["']/i);
  return m ? m[1] : null;
}

function isLoginPage(url: string): boolean {
  try {
    return new URL(url).pathname.startsWith("/auth/login");
  } catch {
    return url.includes("/auth/login");
  }
}

function extractLoginError(html: string): string | null {
  const m = html.match(/<div[^>]*class=["'][^"']*alert-danger[^"']*["'][^>]*>([\s\S]{0,300}?)<\/div>/i);
  return m ? m[1].replace(/\s+/g, " ").trim() : null;
}

export async function opLogin(
  f: HttpFetcher,
  baseUrl: string,
  jarPath: string,
  jar: CookieJar,
  form: LoginForm,
) {
  const loginUrl = baseUrl.replace(/\/+$/, "") + "/auth/login";
  const page = await f.get(loginUrl);
  if (page.fetchError) {
    return { contract: 1, ok: false, status: page.status, error: { code: page.fetchError.code, message: page.fetchError.message } };
  }
  const token = extractToken(page.html);
  if (!token) return { contract: 1, ok: false, status: page.status, error: { code: "csrf", message: "no _token hidden input found on login page" } };
  // The form action may be root-relative (/auth/login) or absolute; resolve it
  // against the login URL so HttpFetcher always receives an absolute URL.
  const rawAction = (page.html.match(/<form[^>]*action=["']([^"']+)["']/i) || [])[1] || loginUrl;
  let action: string;
  try {
    action = new URL(rawAction, loginUrl).href;
  } catch {
    action = loginUrl;
  }
  const body = new URLSearchParams({ _token: token, email: form.email, password: form.password });
  const post = await f.post(action, body, token);
  if (post.fetchError) {
    return { contract: 1, ok: false, status: post.status, error: { code: post.fetchError.code, message: post.fetchError.message } };
  }
  await jar.save(jarPath);
  if (!isLoginPage(post.finalUrl) && post.status !== 401 && post.status < 400) {
    return { contract: 1, ok: true, status: post.status, finalUrl: post.finalUrl, challenge: post.challenge };
  }
  const msg = extractLoginError(post.html);
  return { contract: 1, ok: false, status: post.status, finalUrl: post.finalUrl, error: { code: "login", message: msg ?? "credentials rejected" } };
}

export async function opLogout(f: HttpFetcher, baseUrl: string, jar: CookieJar, jarPath: string) {
  const loginUrl = baseUrl.replace(/\/+$/, "") + "/auth/login";
  const page = await f.get(loginUrl);
  const token = page.html ? extractToken(page.html) : null;
  if (token) {
    const body = new URLSearchParams({ _token: token });
    const post = await f.post(baseUrl.replace(/\/+$/, "") + "/auth/logout", body, token);
    if (post.fetchError || isLoginPage(post.finalUrl)) { /* site has no logout route; dropping the jar is enough */ }
  }
  jar.clear();
  await jar.save(jarPath);
  return { contract: 1, ok: true };
}
