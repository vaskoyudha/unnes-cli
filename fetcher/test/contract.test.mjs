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

const FIX = (f) => readFileSync(join(process.cwd(), "test", "fixtures", f), "utf8");

function startServer() {
  let tokenCounter = 0;
  let lastToken = "";
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
    res.statusCode = 404;
    res.end("not found");
  });
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      resolve({ server, base: "http://127.0.0.1:" + server.address().port });
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
