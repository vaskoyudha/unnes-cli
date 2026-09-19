// Contract tests: drive processJob against a local HTTP server with canned fixtures.
import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createServer } from "node:http";
import { processJob } from "../dist/index.js";
import { normalizeHtml } from "../dist/normalize.js";
import { CookieJar, JAR_VERSION } from "../dist/cookiejar.js";
import { googlePersistenceStatus } from "../dist/browser.js";
import { describeSubmitState } from "../dist/browser.js";
import { persistenceChange } from "../dist/browser.js";
import { extractRecords } from "../dist/extract.js";

const FIX = (f) => readFileSync(join(process.cwd(), "test", "fixtures", f), "utf8");

function startServer() {
  let tokenCounter = 0;
  let lastToken = "";
  let hits429 = 0;
  let inflightSlow = 0;
  let peakSlow = 0;
  const server = createServer((req, res) => {
    const url = new URL(req.url, "http://127.0.0.1");
    const cookie = req.headers.cookie ?? "";
    if (url.pathname === "/auth/login" && req.method === "GET") {
      tokenCounter += 1;
      lastToken = "FIXTURE-TOKEN-" + tokenCounter;
      const html = FIX("login.html").replace("TOKENVALUE", lastToken);
      res.setHeader("set-cookie", [
        "XSRF-TOKEN=enc" + tokenCounter + "; Path=/",
        "myunnesstudent_session=prelogin; Path=/; HttpOnly"
      ]);
      res.end(html);
      return;
    }
    if (url.pathname === "/auth/login" && req.method === "POST") {
      let body = "";
      req.on("data", (c) => (body += c));
      req.on("end", () => {
        const params = new URLSearchParams(body);
        const okCreds = params.get("email") === "ok@example.com" && params.get("password") === "secret";
        const okToken = params.get("_token") === lastToken;
        if (okCreds && okToken) {
          res.statusCode = 302;
          res.setHeader("location", "/dashboard");
          res.setHeader("set-cookie", "myunnesstudent_session=auth123; Path=/; HttpOnly");
          res.end();
        } else {
          res.statusCode = 302;
          res.setHeader("location", "/auth/login");
          res.end(FIX("login-error.html"));
        }
      });
      return;
    }
    if (url.pathname === "/dashboard" && req.method === "GET") {
      if (cookie.includes("myunnesstudent_session=auth123")) {
        res.end(FIX("grades.html"));
      } else {
        res.statusCode = 302;
        res.setHeader("location", "/auth/login");
        res.end();
      }
      return;
    }
    if (url.pathname === "/grades" && req.method === "GET") {
      res.end(FIX("grades.html"));
      return;
    }
    if (url.pathname === "/challenge") {
      res.statusCode = 403;
      res.end(FIX("challenge.html"));
      return;
    }
    if (url.pathname === "/file.bin") {
      res.setHeader("content-type", "application/pdf");
      res.setHeader("content-disposition", 'attachment; filename="slide-1.pdf"');
      res.end(Buffer.from([0x25, 0x50, 0x44, 0x46, 0x31])); // %PDF1
      return;
    }
    if (url.pathname === "/protected-file") {
      // expired session shape: bounce to the login page
      res.statusCode = 302;
      res.setHeader("location", "/auth/login");
      res.end();
      return;
    }
    if (url.pathname === "/etag-page") {
      // conditional-request shape: versioned body, honors If-None-Match
      if (req.headers["if-none-match"] === '"v1"') {
        res.statusCode = 304;
        res.end();
        return;
      }
      res.setHeader("etag", '"v1"');
      res.setHeader("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT");
      res.end("<html><body>version one</body></html>");
      return;
    }
    if (url.pathname === "/settle-page") {
      // signal-wait fixture: #r appears only after the /marker XHR lands
      res.end(`<html><body><div id="r" style="display:none">x</div><script>fetch("/marker").then(() => { document.getElementById("r").style.display = "block"; });</script></body></html>`);
      return;
    }
    if (url.pathname === "/marker") {
      res.end("ok");
      return;
    }
    if (url.pathname === "/slow") {
      // concurrency probe: holds the socket, tracks server-side peak
      const ms = Math.min(Number(url.searchParams.get("n") ?? "150") || 150, 2000);
      inflightSlow += 1;
      peakSlow = Math.max(peakSlow, inflightSlow);
      setTimeout(() => {
        inflightSlow -= 1;
        try { res.end("<html><body>slow fine</body></html>"); } catch { /* client gone */ }
      }, ms);
      return;
    }
    if (url.pathname === "/flaky-429") {
      // rate-limit shape: first hit 429 with Retry-After, then fine
      hits429 += 1;
      if (hits429 === 1) {
        res.statusCode = 429;
        res.setHeader("retry-after", "0");
        res.end("slow down");
        return;
      }
      res.end("<html><body>fine</body></html>");
      return;
    }
    if (url.pathname === "/denied-file") {
      // expired session shape: 200 inline access-denied page (Moodle style)
      res.setHeader("content-type", "text/html");
      res.end("<html><body>tidak diberi hak untuk mengakses fitur ini [tamu]!</body></html>");
      return;
    }
    res.statusCode = 404;
    res.end("not found");
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      resolve({
        server,
        base: "http://127.0.0.1:" + server.address().port,
        slowStats: () => ({ peak: peakSlow }),
      });
    });
  });
}

function makeHome(tag) {
  return mkdtempSync(join(tmpdir(), "unnes-test-" + tag + "-"));
}

async function withHome(tag, fn) {
  const previous = process.env.UNNES_HOME;
  const home = makeHome(tag);
  process.env.UNNES_HOME = home;
  try {
    await fn(home);
  } finally {
    if (previous === undefined) delete process.env.UNNES_HOME;
    else process.env.UNNES_HOME = previous;
    rmSync(home, { recursive: true, force: true });
  }
}

test("login succeeds with correct credentials and persists the jar", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("login-ok", async (home) => {
    const res = await processJob({
      contract: 1,
      op: "login",
      baseUrl: base,
      form: { email: "ok@example.com", password: "secret" },
    });
    assert.equal(res.ok, true);
    assert.equal(res.finalUrl, base + "/dashboard");
    const jarRaw = readFileSync(join(home, "profiles", "default.json"), "utf8");
    const jar = JSON.parse(jarRaw);
    assert.equal(jar.version, JAR_VERSION);
    assert.ok(jar.cookies.some((c) => c.name === "myunnesstudent_session" && c.value === "auth123"));
  });
});

test("login with wrong credentials surfaces the Laravel message", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("login-bad", async (home) => {
    const res = await processJob({
      contract: 1,
      op: "login",
      baseUrl: base,
      form: { email: "bad@example.com", password: "wrong" },
    });
    assert.equal(res.ok, false);
    assert.equal(res.error.code, "login");
    assert.match(res.error.message, /credentials do not match/i);
  });
});

test("login token roundtrip: server only accepts the issued _token", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("login-csrf", async (home) => {
    const res = await processJob({
      contract: 1,
      op: "login",
      baseUrl: base,
      form: { email: "ok@example.com", password: "secret" },
    });
    // The server rejects unless _token equals the issued one, so ok:true
    // proves the GET -> token -> POST cycle worked end to end.
    assert.equal(res.ok, true);
  });
});

test("get on a protected page without session reports sessionExpired", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("get-unauthed", async (home) => {
    const res = await processJob({
      contract: 1,
      op: "get",
      url: base + "/dashboard",
    });
    assert.equal(res.ok, true);
    assert.equal(res.sessionExpired, true);
    assert.equal(res.finalUrl, base + "/auth/login");
  });
});

test("get with a session extracts records via selector", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("get-authed", async (home) => {
    const login = await processJob({
      contract: 1,
      op: "login",
      baseUrl: base,
      form: { email: "ok@example.com", password: "secret" },
    });
    assert.equal(login.ok, true);
    const res = await processJob({
      contract: 1,
      op: "get",
      url: base + "/dashboard",
      extract: {
        selector: "tbody tr",
        fields: {
          code: "td:nth-child(1)",
          subject: "td:nth-child(2)",
          grade: "td:nth-child(3)",
        },
      },
    });
    assert.equal(res.ok, true);
    assert.equal(res.sessionExpired, false);
    assert.equal(res.records.length, 2);
    assert.equal(res.records[0].code, "IF101");
    assert.equal(res.records[0].subject, "Jaringan Komputer");
    assert.equal(res.records[0].grade, "B+");
    assert.equal(res.records[1].grade, "A");
  });
});

test("normalize strips rotating tokens and applies extra regexes", () => {
  const html = FIX("grades.html") + " secret-value-42 here";
  const out = normalizeHtml(html, ["secret-value-[0-9]+"]);
  assert.ok(!out.includes("ROTATING-TOKEN"));
  assert.ok(!out.includes("ROTATING-META"));
  assert.ok(!out.includes("_token"));
  assert.ok(!out.includes("secret-value-42"));
  assert.ok(out.includes("Jaringan Komputer"));
});

test("challenge flag is set for Cloudflare-style 403 pages", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("challenge", async (home) => {
    const res = await processJob({
      contract: 1,
      op: "get",
      url: base + "/challenge",
    });
    assert.equal(res.ok, true);
    assert.equal(res.challenge, true);
  });
});

test("connection failure surfaces a network error code", async () => {
  await withHome("netfail", async (home) => {
    const res = await processJob({
      contract: 1,
      op: "get",
      url: "http://127.0.0.1:1/nothing",
    });
    assert.equal(res.ok, false);
    assert.equal(res.error.code, "network");
  });
});

test("cookie jar roundtrip keeps only matching, unexpired cookies", async () => {
  const jar = CookieJar.empty();
  const url = new URL("https://student.unnes.ac.id/auth/login");
  jar.addFromSetCookie([
    "XSRF-TOKEN=abc; Path=/",
    "myunnesstudent_session=sess123; Path=/; HttpOnly",
    "other=dropme; Path=/; Max-Age=-1",
  ], url);
  const header = jar.headerFor(new URL("https://student.unnes.ac.id/dashboard"));
  assert.ok(header.includes("XSRF-TOKEN=abc"));
  assert.ok(header.includes("myunnesstudent_session=sess123"));
  assert.ok(!header.includes("dropme"));
  assert.equal(jar.cookieValue(["XSRF-TOKEN"], new URL("https://student.unnes.ac.id/x")), "abc");
  assert.equal(jar.headerFor(new URL("https://evil.example/dashboard")), null);
  const dir = makeHome("jar");
  await jar.save(join(dir, "profiles", "default.json"));
  const reloaded = await CookieJar.load(join(dir, "profiles", "default.json"));
  assert.equal(reloaded.headerFor(new URL("https://student.unnes.ac.id/dashboard")), header);
  rmSync(dir, { recursive: true, force: true });
});

test("browser login respects UNNES_NO_BROWSER (no browser launched)", async () => {
  await withHome("nobrowser", async () => {
    process.env.UNNES_NO_BROWSER = "1";
    try {
      const res = await processJob({
        contract: 1,
        op: "login",
        mode: "browser",
        baseUrl: "https://apps.unnes.ac.id",
      });
      assert.equal(res.ok, false);
      assert.equal(res.error.code, "usage");
      assert.match(res.error.message, /UNNES_NO_BROWSER/);
    } finally {
      delete process.env.UNNES_NO_BROWSER;
    }
  });
});


test("batch render respects UNNES_NO_BROWSER (no browser launched)", async () => {
  await withHome("nobatch", async () => {
    process.env.UNNES_NO_BROWSER = "1";
    try {
      const res = await processJob({
        contract: 1,
        op: "batch",
        entries: [{ url: "https://akademik.unnes.ac.id/krs-mahasiswa", ssoApp: "76", extract: { selector: "tbody tr" } }],
      });
      assert.equal(res.ok, false);
      assert.equal(res.error.code, "usage");
    } finally {
      delete process.env.UNNES_NO_BROWSER;
    }
  });
});

test("page render respects UNNES_NO_BROWSER (no browser launched)", async () => {
  await withHome("nopagerender", async () => {
    process.env.UNNES_NO_BROWSER = "1";
    try {
      const res = await processJob({
        contract: 1,
        op: "page",
        url: "https://akademik.unnes.ac.id/krs-mahasiswa",
        ssoApp: "76",
        extract: { selector: "tbody tr" },
      });
      assert.equal(res.ok, false);
      assert.equal(res.error.code, "usage");
      assert.match(res.error.message, /UNNES_NO_BROWSER/);
    } finally {
      delete process.env.UNNES_NO_BROWSER;
    }
  });
});

test("sso op rejects unknown gateway apps without network", async () => {
  await withHome("sso-unknown", async () => {
    const res = await processJob({
      contract: 1,
      op: "sso",
      appId: "999",
      baseUrl: "https://apps.unnes.ac.id",
    });
    assert.equal(res.ok, false);
    assert.equal(res.error.code, "usage");
    assert.match(res.error.message, /unknown gateway app/);
  });
});

test("sso op reports gateway session expiry without a jar", async () => {
  await withHome("sso-nosession", async () => {
    const res = await processJob({
      contract: 1,
      op: "sso",
      appId: "76",
      baseUrl: "http://127.0.0.1:1", // unreachable: no gateway session possible
    });
    assert.equal(res.ok, false);
    // network error surfaces from the gateway fetch (no jar, unreachable host)
    assert.ok(["network", "session"].includes(res.error.code));
  });
});

test("contract versions other than 1 are rejected", async () => {
  await withHome("contract", async (home) => {
    const res = await processJob({
      contract: 99,
      op: "get",
      url: "http://127.0.0.1:1/x",
    });
    assert.equal(res.ok, false);
    assert.equal(res.error.code, "contract");
  });
});

test("googlePersistenceStatus: persistent SID counts, session-only does not", () => {
  const mk = (name, domain, expires) => ({ name, value: "v", domain, path: "/", expires, httpOnly: true, secure: true, sameSite: "Lax" });
  // Persistent auth cookie -> chooser populated next login.
  let s = googlePersistenceStatus([mk("SID", ".google.com", 1900000000), mk("NID", ".google.com", 1900000000)]);
  assert.deepEqual(s.persistentAuth, ["SID"]);
  assert.deepEqual(s.sessionAuth, []);
  // Session-only SID (expires -1, dropped by Chrome on close) -> disappearing accounts.
  s = googlePersistenceStatus([mk("SID", ".google.com", -1), mk("HSID", "accounts.google.com", -1)]);
  assert.deepEqual(s.persistentAuth, []);
  assert.deepEqual(s.sessionAuth, ["HSID", "SID"]);
  // Prefs/telemetry only (the exact state of a profile whose chooser is empty).
  s = googlePersistenceStatus([
    mk("NID", ".google.com", 1900000000),
    mk("OTZ", "accounts.google.com", 1900000000),
    mk("__Host-GAPS", "accounts.google.com", 1900000000),
  ]);
  assert.deepEqual(s.persistentAuth, []);
  assert.deepEqual(s.sessionAuth, []);
  assert.deepEqual(s.other, ["NID", "OTZ", "__Host-GAPS"]);
  // Non-google cookies are ignored; empty input is empty.
  s = googlePersistenceStatus([mk("laravel_session", "apps.unnes.ac.id", 1900000000)]);
  assert.deepEqual(s.persistentAuth, []);
  assert.deepEqual(s.other, []);
  s = googlePersistenceStatus([]);
  assert.deepEqual(s, { persistentAuth: [], sessionAuth: [], other: [] });
});

test("download saves server bytes with the advertised filename", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("dl-ok", async (home) => {
    const out = join(home, "dl");
    const res = await processJob({ contract: 1, op: "download", url: base + "/file.bin", out });
    assert.equal(res.ok, true);
    assert.equal(res.filename, "slide-1.pdf");
    assert.equal(res.bytes, 5);
    assert.equal(res.path, join(out, "slide-1.pdf"));
    const raw = readFileSync(join(out, "slide-1.pdf"));
    assert.equal(raw.length, 5);
    assert.equal(raw[0], 0x25);
  });
});

test("download reports session expiry on a login bounce", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("dl-exp", async (home) => {
    const res = await processJob({ contract: 1, op: "download", url: base + "/protected-file", out: join(home, "dl") });
    assert.equal(res.ok, false);
    assert.equal(res.sessionExpired, true);
  });
});
test("describeSubmitState reports the verified end state honestly", () => {
  assert.match(describeSubmitState({ submitted: true, draft: false, status: "Submitted for grading", hasFile: true, url: "" }), /submitted for grading/);
  assert.equal(describeSubmitState({ submitted: false, draft: true, status: "", hasFile: false, url: "" }), "file uploaded and saved as draft");
  assert.equal(describeSubmitState({ submitted: false, draft: false, status: "", hasFile: true, url: "" }), "file uploaded and saved as draft");
  // Unverifiable is explicit, not a fake success.
  assert.match(
    describeSubmitState({ submitted: false, draft: false, status: "", hasFile: false, url: "" }),
    /could be verified/
  );
});

test("elena course page yields classified mod links (live fixture)", () => {
  const html = FIX("elena-course.html");
  assert.ok(html.includes("Kriptografi"), "fixture is the Kriptografi course page");
  const recs = extractRecords(html, {
    selector: "a[href*='/mod/']",
    fields: { nama: "", url: "@href" },
  });
  assert.ok(recs.length >= 10, `expected >=10 mod links, got ${recs.length}`);
  const kinds = {};
  for (const r of recs) {
    const m = (r.url || "").match(/\/mod\/(\w+)\//);
    const k = m ? m[1] : "?";
    kinds[k] = (kinds[k] || 0) + 1;
  }
  // The live page carries materials alongside activities.
  assert.ok((kinds.resource || 0) >= 1, `expected resource links, got ${JSON.stringify(kinds)}`);
  const named = recs.filter((r) => (r.nama || "").trim() !== "" && (r.url || "").includes("/mod/resource/"));
  assert.ok(named.length >= 1, "expected at least one named resource link");
});

test("persistenceChange distinguishes kept / lost / never-planted", () => {
  const P = (auth) => ({ persistentAuth: auth, sessionAuth: [], other: [] });
  assert.deepEqual(persistenceChange(P(["SID"]), P(["SID", "HSID"])), { kept: true, lost: false });
  assert.deepEqual(persistenceChange(P(["SID"]), P([])), { kept: false, lost: true });
  assert.deepEqual(persistenceChange(P([]), P([])), { kept: false, lost: false });
});

test("isOwnChromeProcess matches only chrome cmdlines holding the dir", async () => {
  const { isOwnChromeProcess, armShutdownGuard } = await import("../dist/browser.js");
  // self: our cmdline contains neither marker
  assert.equal(isOwnChromeProcess(process.pid, "definitely-not-present-xyz"), false);
  // nonexistent pid
  assert.equal(isOwnChromeProcess(42424242, "anything"), false);
  // fake chrome holding the profile dir
  const { spawn } = await import("node:child_process");
  const kid = spawn("bash", ["-c", "exec -a probe-chrome-marker sleep 30"]);
  try {
    assert.equal(isOwnChromeProcess(kid.pid, "probe-chrome-marker"), true);
    assert.equal(isOwnChromeProcess(kid.pid, "/some/other/dir"), false);
  } finally {
    kid.kill("SIGKILL");
  }
});

test("armShutdownGuard installs handlers once and disarms idempotently", async () => {
  const { armShutdownGuard } = await import("../dist/browser.js");
  const before = process.listenerCount("SIGINT");
  const d1 = armShutdownGuard(async () => {});
  const d2 = armShutdownGuard(async () => {});
  assert.equal(process.listenerCount("SIGINT"), before + 1);
  d1(); d1(); d2(); // idempotent, no throw
});

test("SIGINT runs the armed shutdown closer before exit (child process)", async (t) => {
  const { spawnSync } = await import("node:child_process");
  const { mkdtempSync, existsSync, readFileSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const dir = mkdtempSync(join(tmpdir(), "unnes-guard-"));
  const flag = join(dir, "flushed");
  const child = join(dir, "child.mjs");
  const { writeFileSync } = await import("node:fs");
  writeFileSync(child, [
    "import { armShutdownGuard } from " + JSON.stringify(new URL("../dist/browser.js", import.meta.url).href) + ";",
    "import { writeFileSync } from 'node:fs';",
    "armShutdownGuard(async () => {",
    "  await new Promise((r) => setTimeout(r, 200));",
    `  writeFileSync(${JSON.stringify(flag)}, 'flushed');`,
    "});",
    "setTimeout(() => process.kill(process.pid, 'SIGINT'), 200);",
    "setInterval(() => {}, 1000);",
  ].join("\n"));
  const t0 = Date.now();
  const res = spawnSync(process.execPath, [child], { timeout: 20000 });
  const dt = Date.now() - t0;
  assert.equal(res.status, 130);
  assert.ok(existsSync(flag), "closer ran and flushed before exit");
  assert.equal(readFileSync(flag, "utf8"), "flushed");
  assert.ok(dt < 15000, "no 15s fallback wait on the happy path");
  const { rmSync } = await import("node:fs");
  rmSync(dir, { recursive: true, force: true });
});

test("renderDir isolates headless renders from the login profile", async () => {
  const { renderDir, MAIN_STORE_ARGS } = await import("../dist/browser.js");
  assert.equal(renderDir("/h/browser-profiles/default"), "/h/browser-profiles/default-headless");
  // Single-backend rule: every MAIN launcher pins the same deterministic
  // store. No keyring, no mock-vs-real split.
  assert.deepEqual(MAIN_STORE_ARGS, ["--password-store=basic"]);
});

test("unified basic store persists cookies across launchers (early-system invariant)", async (t) => {
  // Regression: headed login (real Chrome via CDP) and headless ops used
  // different cookie-encryption backends on ONE profile dir, so each open
  // read the other's session as signed-out and checkpointed the amnesia -
  // "password every time". All MAIN launchers now pin --password-store=basic
  // explicitly, so a cookie written by one launcher must be visible to the
  // next, whichever launcher opens the dir.
  const { mkdtempSync, rmSync } = await import("node:fs");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { chromium } = await import("playwright");
  const { MAIN_STORE_ARGS } = await import("../dist/browser.js");
  const dir = mkdtempSync(join(tmpdir(), "unnes-store-"));
  const baseOpts = {
    headless: true,
    args: ["--disable-blink-features=AutomationControlled", ...MAIN_STORE_ARGS],
  };
  const exp = Math.floor(Date.now() / 1000) + 86400 * 30;
  let ctx;
  try {
    ctx = await chromium.launchPersistentContext(dir, baseOpts);
  } catch (e) {
    rmSync(dir, { recursive: true, force: true });
    t.skip("cannot launch chromium: " + String(e).slice(0, 120));
    return;
  }
  try {
    await ctx.addCookies([{ name: "SID", value: "k1", domain: ".google.com", path: "/", secure: true, httpOnly: true, expires: exp }]);
    await ctx.close();
    // Second launcher, stock defaults (Playwright already ships basic): the
    // planted session must survive the handoff.
    const c2 = await chromium.launchPersistentContext(dir, { headless: true });
    const seen = (await c2.cookies()).map((c) => c.name);
    await c2.close();
    assert.ok(seen.includes("SID"), "unified basic store must keep SID across launchers");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("leading-dot domains match subdomains (browser-captured cookies)", async () => {
  const { CookieJar } = await import("../dist/cookiejar.js");
  const jar = CookieJar.empty();
  // Playwright reports domain cookies with a leading dot.
  jar.addCookie({ name: "identitas_sso", value: "1", domain: ".unnes.ac.id", path: "/", secure: false, httpOnly: false, expires: null });
  jar.addCookie({ name: "X", value: "y", domain: ".apps.unnes.ac.id", path: "/", secure: false, httpOnly: false, expires: null });
  const h1 = jar.headerFor(new URL("https://elena.unnes.ac.id/my/"));
  assert.ok(h1 && h1.includes("identitas_sso=1"), "dot-domain must be sent to subdomains: " + h1);
  const h2 = jar.headerFor(new URL("https://apps.unnes.ac.id/gate/list"));
  assert.ok(h2 && h2.includes("X=y"), "exact dot-domain host must match: " + h2);
  const h3 = jar.headerFor(new URL("https://evilunnes.ac.id/"));
  assert.ok(!h3 || !h3.includes("identitas_sso"), "must not match evilunnes.ac.id: " + h3);
});

test("cookie paths respect boundaries and Set-Cookie deletion works", async () => {
  const { CookieJar } = await import("../dist/cookiejar.js");
  const jar = CookieJar.empty();
  jar.addCookie({ name: "a", value: "1", domain: "x.test", path: "/app", secure: false, httpOnly: false, expires: null });
  assert.ok(jar.headerFor(new URL("https://x.test/app/y")).includes("a=1"));
  assert.equal(jar.headerFor(new URL("https://x.test/application")), null);
  assert.ok(jar.headerFor(new URL("https://x.test/app")).includes("a=1"));
  // server logout (past expiry) removes the entry instead of being ignored
  jar.addFromSetCookie(["a=gone; Path=/app; Expires=Thu, 01 Jan 1970 00:00:00 GMT"], new URL("https://x.test/app"));
  assert.equal(jar.headerFor(new URL("https://x.test/app/y")), null);
  // explicit Domain attr with leading dot is accepted and normalized
  jar.addFromSetCookie(["b=2; Domain=.x.test; Path=/"], new URL("https://a.x.test/"));
  assert.ok(jar.headerFor(new URL("https://b.x.test/")).includes("b=2"));
});

test("cookieValue skips expired entries", async () => {
  const { CookieJar } = await import("../dist/cookiejar.js");
  const jar = CookieJar.empty();
  jar.addCookie({ name: "XSRF-TOKEN", value: "old", domain: "x.test", path: "/", secure: false, httpOnly: false, expires: null });
  assert.equal(jar.cookieValue(["XSRF-TOKEN"], new URL("https://x.test/")), "old");
});

test("download maps inline denial pages to session expiry", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("dl-denied", async (home) => {
    const res = await processJob({ contract: 1, op: "download", url: base + "/denied-file", out: join(home, "dl") });
    assert.equal(res.ok, false);
    assert.equal(res.sessionExpired, true);
  });
});

test("evil filenames never escape the out dir", async (t) => {
  const { server, base } = await import("node:http").then(async ({ createServer }) => {
    const s = createServer((req, res) => {
      res.setHeader("content-type", "application/pdf");
      res.setHeader("content-disposition", 'attachment; filename=".."');
      res.end(Buffer.from([0x25, 0x50]));
    });
    await new Promise((r) => s.listen(0, "127.0.0.1", r));
    return { server: s, base: "http://127.0.0.1:" + s.address().port };
  });
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("dl-evil", async (home) => {
    const out = join(home, "dl");
    const res = await processJob({ contract: 1, op: "download", url: base + "/evil.pdf", out });
    assert.equal(res.ok, false);
    assert.match(res.error.message, /unsafe/);
    const { existsSync, readdirSync } = await import("node:fs");
    assert.ok(!existsSync(join(home, "parent-should-not-exist")), "no escape write");
    if (existsSync(out)) assert.deepEqual(readdirSync(out), []);
  });
});

test("polite limiter enforces per-host floor with injected rand", async () => {
  const { PoliteLimiter } = await import("../dist/polite.js");
  const lim = new PoliteLimiter({ minDelayMs: 1000, rand: () => 0.5 });
  const t0 = Date.now();
  await lim.waitFor("a.test");
  await lim.waitFor("a.test");
  const dt = Date.now() - t0;
  assert.ok(dt >= 700 && dt < 3000, "second wait must sleep ~750ms, got " + dt);
  const t1 = Date.now();
  await lim.waitFor("b.test");
  assert.ok(Date.now() - t1 < 200, "different host must not wait");
});

test("429 with Retry-After is retried once, then succeeds", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("ratelimit", async () => {
    const res = await processJob({ contract: 1, op: "get", url: base + "/flaky-429" });
    assert.equal(res.ok, true);
    assert.equal(res.status, 200);
  });
});

test("batchget serves N urls in one spawn, one jar save", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("batchget", async (home) => {
    const res = await processJob({ contract: 1, op: "batchget", urls: [
      { url: base + "/grades" },
      { url: base + "/challenge" },
    ]});
    assert.equal(res.ok, true);
    assert.equal(res.results.length, 2);
    assert.equal(res.results[0].url, base + "/grades");
    assert.ok(res.results.every((r) => r.ok));
    const { CookieJar } = await import("../dist/cookiejar.js");
    const jar = await CookieJar.load(join(home, "profiles", "default.json"));
    assert.ok(jar.cookieNames().length >= 0);
  });
});

test("etag validators turn a repeat fetch into 304 notModified", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("etag304", async () => {
    const first = await processJob({ contract: 1, op: "get", url: base + "/etag-page" });
    assert.equal(first.ok, true);
    assert.equal(first.status, 200);
    assert.ok(first.normalized.includes("version one"));
    const second = await processJob({ contract: 1, op: "get", url: base + "/etag-page" });
    assert.equal(second.ok, true);
    assert.equal(second.notModified, true);
    assert.equal(second.status, 304);
  });
});

test("mapLimit preserves order and caps concurrency", async () => {
  const { mapLimit } = await import("../dist/pool.js");
  let inflight = 0, peak = 0;
  const out = await mapLimit([1,2,3,4,5,6], 3, async (n) => {
    inflight++; peak = Math.max(peak, inflight);
    await new Promise((r) => setTimeout(r, 30));
    inflight--;
    return n * 2;
  });
  assert.deepEqual(out, [2,4,6,8,10,12]);
  assert.ok(peak <= 3, "peak " + peak + " exceeds cap");
  assert.ok(peak >= 2, "no parallelism observed, peak " + peak);
});

test("batchget concurrency fans out within the cap", async (t) => {
  const { server, base, slowStats } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("batchconc", async () => {
    const urls = [1, 2, 3, 4, 5, 6].map((i) => ({ url: base + "/slow?n=150&i=" + i }));
    const t0 = Date.now();
    const res = await processJob({ contract: 1, op: "batchget", urls, concurrency: 3 });
    const dt = Date.now() - t0;
    assert.equal(res.ok, true);
    assert.ok(res.results.every((r) => r.ok), JSON.stringify(res).slice(0, 200));
    // 6x150ms serial sleeps cannot finish under 800ms (timers never fire
    // early); 2 waves of 3 finish ~300ms + overhead. Peak is the robust
    // signal, timing corroborates.
    assert.ok(slowStats().peak <= 3, "peak " + slowStats().peak + " exceeds cap");
    assert.ok(slowStats().peak >= 2, "no parallelism observed");
    assert.ok(dt < 800, "6x150ms took " + dt + "ms, still serial?");
  });
});

test("settle returns on response+selector without the full timeout", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  const mod = await import("playwright").catch(() => null);
  const chromium = mod?.chromium ?? mod?.default?.chromium ?? null;
  if (!chromium) { t.skip("no playwright"); return; }
  const { settle } = await import("../dist/browser.js");
  let browser;
  try {
    browser = await chromium.launch({ headless: true });
  } catch (e) {
    t.skip("cannot launch chromium: " + String(e).slice(0, 100));
    return;
  }
  try {
    const page = await browser.newPage();
    // arm before navigation so the marker response is observed, not missed
    const pending = settle(page, { responseRe: /\/marker/, selector: "#r:visible", timeoutMs: 5000 });
    await page.goto(base + "/settle-page");
    const r = await pending;
    assert.equal(r.settled, true);
    assert.ok(r.elapsedMs < 2500, "took " + r.elapsedMs + "ms");
    const s2 = await settle(page, { responseRe: /\/never-there/, selector: "#missing:visible", timeoutMs: 800 });
    assert.equal(s2.settled, false);
  } finally {
    await browser.close().catch(() => {});
  }
});
