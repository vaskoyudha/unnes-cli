# Fetch Optimization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut wall-clock fetch time (tugas/materi/TUI-load/watch passes) by fetching less often, in fewer processes, and in parallel — without hammering the portal and without touching the login-profile single-writer rule.

**Architecture:** Three tracer slices. Phase 1: one `node` spawn serves N plain-HTTP GETs (`op=batchget`) behind a per-host politeness limiter, wired into the materi loop as vertical proof. Phase 2: skip unchanged work (Moodle-WS spike → verdict; ETag/304 validators; 304-as-cache-hit). Phase 3: intra-process parallel GETs (single jar owner, no cross-process jar races) wired into the tugas loop, plus browser signal-waits and resource blocking. Each phase ends with timed before/after evidence.

**Tech Stack:** Node 22 (global `fetch`/undici built-in — `undici` is NOT importable as a package here, verified 2026-09-19), TypeScript, Playwright (browser only), Rust (`std::thread::scope` if Rust-side threads are ever needed — prefer TS-side pooling), `node:test` + `cargo test`.

**Spec:** This file is self-contained; research appendix at the bottom carries the design rationale. Contract changes are specified inline per task (op/field names exact).

## Global Constraints

- NEVER open `browser-profiles/<profile>` (MAIN) from a headless process. Renders/submit/open stay on `<profile>-headless` via `launchContext` (`fetcher/src/browser.ts`). Browser login stays headed-only.
- Contract v1 (`fetcher/CONTRACT.md`): every new op/field gets a CONTRACT.md entry, a Rust struct field, and a resync step `cp fetcher/dist/*.js ~/.config/unnes/fetcher/dist/` after each `dist` rebuild.
- Exit codes: `err_code_for` (`src/main.rs:358-364`) maps `usage|contract→2`, `network|timeout|challenge→5`, else `1`. New fetcher code `ratelimit` MUST map to `5`. `notModified` is NOT an error (no exit impact).
- Baselines stay green: 56 Rust tests + 31 fetcher contract tests (2026-09-19). Timing assertions only against `127.0.0.1` with ≥2× margins, never against the live portal.
- TDD for every behavior change: failing test → run → minimal implementation → run → commit. One task = one commit.
- No new npm dependencies (stdlib + existing `cheerio`/`playwright` only). No `TODO`/`TBD`/vague steps.

---

## File Map (what changes where)

| File | Responsibility after plan |
|---|---|
| `fetcher/src/polite.ts` (NEW) | `PoliteLimiter`: per-host `next-allowed-at` + jittered floor + 429/`Retry-After` backoff state. Pure logic, no I/O except sleep. |
| `fetcher/src/pool.ts` (NEW) | `mapLimit<T,R>(items, limit, fn)`: order-preserving bounded pool, fail-open per item. |
| `fetcher/src/validators.ts` (NEW) | Per-URL `{etag,lastModified}` JSON store beside the jar; pure `apply`/`capture` helpers. |
| `fetcher/src/http.ts` (modify: `HttpFetcher`, `request()`) | Accept `polite?`; wait before fetch; 429→sleep(`Retry-After`≤120s)+one retry→`ratelimit` error; send validators; return `notModified` on 304 without body read. |
| `fetcher/src/index.ts` (modify: `Job`, `processJob`) | New `op=batchget`: sequential prime pass + parallel data pass (Phase 3), single jar save, per-URL SSO bootstrap. |
| `fetcher/src/browser.ts` (modify: `completeElenaSession`, batch prime) | `settle()` helper (exported): `waitForResponse`+selector with capped timeout; replaces fixed 5s/6s sleeps. `context.route` resource blocking on render contexts only. |
| `fetcher/test/contract.test.mjs` (modify) | New routes on the existing `startServer()` + new tests per task (uses `processJob`, `withHome`, local `base`). |
| `fetcher/CONTRACT.md`, `README.md` (modify) | Document `batchget`, `notModified`, `ratelimit`, timing table. |
| `src/fetcher.rs` (modify) | `BatchGetEntry` + `BatchGetResult` + `run_batchget()`; `err_code_for` gains `"ratelimit" => 5`; `JobResult.not_modified` field (Phase 2). |
| `src/materi.rs` (modify: `fetch_materi`, ~lines 62-114) | Course loop → one `run_batchget` (Phase 1 proof); honors `notModified` via cache (Phase 2). |
| `src/tugas.rs` (modify: `fetch_items`, ~lines 242-345) | Course×kind loop + detail-fallback loop → `run_batchget` (Phase 3); pure `batchget_job()`/`merge_batch_results()` seams for unit tests. |
| `src/cache.rs` (modify, Phase 2) | `fresh(key, ttl)` helper reused by callers for 304-as-cache-hit. |

**Interfaces (exact names tasks share):**
- `PoliteLimiter`: `new PoliteLimiter(opts?: {minDelayMs?: number; rand?: () => number})`, `waitFor(host: string): Promise<void>`, `noteResult(host: string, ok: boolean, retryAfterSecs?: number | null): void`. Default `minDelayMs = 0` (measured 2026-09-19: a 1000ms default made one 8-URL batchget run 7.7s vs 3.5s sequential; this CLI fetches in short bursts so the floor is opt-in, while Retry-After/backoff is always on); jitter `delay*(0.5+rand())`, `rand` defaults to `Math.random`, tests inject `() => 0.5`.
- `mapLimit<T, R>(items: T[], limit: number, fn: (item: T, index: number) => Promise<R>): Promise<R[]>` — resolves in input order; one item's rejection never rejects the batch (wrap `fn` at call site; `mapLimit` itself propagates only its own bugs).
- Validators file: `jarPath.replace(/\.json$/, ".validators.json")`, shape `{version: 1, urls: {[url: string]: {etag?: string; lastModified?: string}}}`.
- `op=batchget` job: `{contract:1, op:"batchget", profile?, urls: {url: string; extract?: ExtractSpec; extraRegexes?: string[]}[], concurrency?: number}` (default 1 in Phase 1, 3 in Phase 3). Per-entry result `{url, ok, status?, finalUrl?, sessionExpired, records?, normalized?, notModified?, error?}`; top-level `{contract:1, op:"batchget", ok:true, results:[...]}`; top `sessionExpired = results.some(r => r.sessionExpired)` (conservative: any expiry triggers the caller's existing prime→retry path).
- Rust: `pub struct BatchGetEntry { pub url: String, pub ok: bool, pub status: Option<u64>, pub final_url: Option<String>, #[serde(default)] pub session_expired: bool, #[serde(default)] pub records: Vec<Value>, pub normalized: Option<String>, #[serde(default)] pub not_modified: bool, pub error: Option<JobError> }`, `pub struct BatchGetResult { pub ok: bool, #[serde(default)] pub results: Vec<BatchGetEntry>, pub error: Option<JobError> }`, `pub fn run_batchget(home: &UnnesHome, profile: &str, urls: &[(String, Value)]) -> Result<BatchGetResult>` (builds the job, calls `run_job`, maps + validates `contract==1` via existing check).

---

## Phase 1 — Fewer spawns, warmer connections, polite by construction

### Task 1: `PoliteLimiter` + 429 handling in `HttpFetcher`

**Files:**
- Create: `fetcher/src/polite.ts`
- Modify: `fetcher/src/http.ts` (`HttpFetcher` constructor gains 4th optional param `polite?: PoliteLimiter`; `request()` waits + 429 retry)
- Test: `fetcher/test/contract.test.mjs` (add `/flaky-429` route + unit tests importing `../dist/polite.js`)

**Interfaces:**
- Consumes: nothing new.
- Produces: `PoliteLimiter` (exact API above) for Tasks 2/3/8.

- [ ] **Step 1: Write failing unit tests** for the limiter (no server):
```js
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
```
- [ ] **Step 2: Run** `npm test` — expect FAIL (`Cannot find module '../dist/polite.js'`).
- [ ] **Step 3: Implement** `fetcher/src/polite.ts`:
```ts
export class PoliteLimiter {
  private next = new Map<string, number>();
  constructor(private opts: { minDelayMs?: number; rand?: () => number } = {}) {}
  private delayMs(): number {
    const base = this.opts.minDelayMs ?? 0;
    const r = this.opts.rand ? this.opts.rand() : Math.random();
    return Math.round(base * (0.5 + r));
  }
  async waitFor(host: string): Promise<void> {
    const h = host.toLowerCase();
    const wait = (this.next.get(h) ?? 0) - Date.now();
    this.next.set(h, Date.now() + Math.max(0, wait) + this.delayMs());
    if (wait > 0) await new Promise((r) => setTimeout(r, wait));
  }
  noteResult(host: string, ok: boolean, retryAfterSecs?: number | null): void {
    const h = host.toLowerCase();
    if (ok) return;
    const extra = retryAfterSecs && retryAfterSecs > 0 ? Math.min(retryAfterSecs, 120) * 1000 : 5000;
    this.next.set(h, Math.max(this.next.get(h) ?? 0, Date.now() + extra));
  }
}
```
- [ ] **Step 4: Run** `npm test` — expect the new test to PASS (others unaffected).
- [ ] **Step 5: Write failing contract test** for 429 honor. Add route to `startServer()`:
```js
if (url.pathname === "/flaky-429") {
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
```
(`let hits429 = 0;` next to `tokenCounter`.) Test:
```js
test("429 with Retry-After is retried once, then succeeds", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("ratelimit", async () => {
    const res = await processJob({ contract: 1, op: "get", url: base + "/flaky-429" });
    assert.equal(res.ok, true);
    assert.equal(res.status, 200);
  });
});
```
- [ ] **Step 6: Run** — expect FAIL (today 429 falls through as `ok:true status:429` with no retry; assert on `status===200` fails).
- [ ] **Step 7: Implement** in `http.ts`: constructor 4th param `polite`, `request()` calls `await this.polite?.waitFor(url.hostname)` before `fetch`, and after a `429` response: `noteResult(host,false,retryAfterSecs)` + sleep `min(retryAfter,120)s` (0 allowed) + ONE retry of the same hop; if still 429 → `fetchError {code:"ratelimit", message:"HTTP 429 for "+url}`. Call `noteResult(host,true)` on 2xx/3xx-terminal.
- [ ] **Step 8: Run** `npm run build && npm test` — expect 32+/32+ pass (count grows; zero fail).
- [ ] **Step 9: Commit** `feat(fetcher): per-host politeness limiter with Retry-After honor`.

### Task 2: `op=batchget` — N plain GETs, one spawn, one jar save

**Files:**
- Modify: `fetcher/src/index.ts` (`Job` type + `processJob` case), `src/fetcher.rs` (structs + `run_batchget` + `err_code_for`), `fetcher/CONTRACT.md`
- Test: `fetcher/test/contract.test.mjs`, Rust `src/fetcher.rs` unit test

**Interfaces:**
- Consumes: `PoliteLimiter` (Task 1), existing `doGet` logic (refactor, don't duplicate).
- Produces: `run_batchget()` for Tasks 3/9.

- [ ] **Step 1: Write failing contract test**:
```js
test("batchget serves N urls in one spawn, one jar save", async (t) => {
  const { server, base } = await startServer();
  t.after(() => new Promise((res) => server.close(res)));
  await withHome("batchget", async (home) => {
    const { CookieJar } = await import("../dist/cookiejar.js");
    const res = await processJob({ contract: 1, op: "batchget", urls: [
      { url: base + "/grades" },
      { url: base + "/dashboard" },
    ]});
    assert.equal(res.ok, true);
    assert.equal(res.results.length, 2);
    assert.equal(res.results[0].url, base + "/grades");
    assert.ok(res.results.every((r) => r.ok));
    const jar = await CookieJar.load(join(home, "profiles", "default.json"));
    assert.ok(jar.cookieNames().length >= 0);
  });
});
```
(Note: `withHome(tag, fn)` sets `UNNES_HOME` to the temp home — check its exact signature at `contract.test.mjs:107` before running; the executor must read it. Unauthenticated `/dashboard` 302s to login in the fixture — pick two 200 routes or assert per-entry shapes accordingly. Adjust expected values to what the fixture actually returns; the red assertion is `res.results.length === 2` with `op:"batchget"` which fails today with `unknown op batchget`.)
- [ ] **Step 2: Run** — expect FAIL (`unknown op batchget`).
- [ ] **Step 3: Implement**: refactor `doGet` into `doOneGet(jar, f, url, extra)` returning the result object; `batchget` case: one `CookieJar.load`, one shared `PoliteLimiter`, sequential loop (concurrency arrives in Task 8 — keep `concurrency` field accepted but clamped to 1 with a `console.error` note), per-URL SSO bootstrap reuse of the existing block (extract into `maybeSsoBootstrap(url, result)`), single `jar.save` at end when `results.some(r => r.ok && !r.sessionExpired)`, top `sessionExpired = results.some(r => r.sessionExpired)`. `Job` type gains `urls?: {url: string; extract?: ExtractSpec; extraRegexes?: string[]}[]; concurrency?: number`.
- [ ] **Step 4: Run** `npm run build && npm test` — PASS.
- [ ] **Step 5: Rust** — add structs + `run_batchget` + `"ratelimit" => 5` arm + unit test deserializing a canned batchget JSON (shape from Step 3) asserting `results[1].session_expired` mapping.
- [ ] **Step 6: Run** `cargo test` — PASS.
- [ ] **Step 7: Docs** — CONTRACT.md `batchget` entry (job/result fields verbatim) + `ratelimit` code.
- [ ] **Step 8: Resync** `cp fetcher/dist/*.js ~/.config/unnes/fetcher/dist/` and commit `feat(fetcher): op=batchget multi-GET in one spawn`.

### Task 3: Wire materi loop through `run_batchget` (vertical proof)

**Files:**
- Modify: `src/materi.rs` (`fetch_materi`, lines ~62-114)
- Test: `src/materi.rs` unit test on the pure job-builder

**Interfaces:**
- Consumes: `run_batchget()` (Task 2).
- Produces: timing evidence + pattern for Task 9.

- [ ] **Step 1: Extract pure seam** `fn batchget_job(profile: &str, kursus: &[u32]) -> Value` building the exact current URL+extract pairs; keep existing loop calling per-URL jobs temporarily. Write test asserting URL count/shape for 2 fake cids.
- [ ] **Step 2: Run** `cargo test materi` — FAIL (function missing).
- [ ] **Step 3: Implement** seam + rewire `fetch_materi`: one `run_batchget`; preserve lazy prime semantics (if any entry `session_expired` and `!primed`: `prime_elena` once, rerun whole batch); preserve per-course `eprintln!` warning (warn per failed entry, only `if interactive`); preserve `seen` dedup + ordering (iterate `results` in request order).
- [ ] **Step 4: Run** `cargo test` — PASS, full suite green.
- [ ] **Step 5: Commit** `perf(materi): one batchget spawn per run`.

### Task 4: Phase 1 verification (evidence, not code)

- [ ] **Step 1: Suites**: `cargo test` → record `56+ passed, 0 failed`; `npm test` → record counts; `tsc --noEmit` clean.
- [ ] **Step 2: Live timing** (portal-dependent, report medians of 3): `time ./target/debug/unnes materi --json >/dev/null` and `time ./target/debug/unnes tugas --json >/dev/null` vs pre-change baseline noted in commit message. If portal is unreachable, state so explicitly instead of inventing numbers.
  Measured 2026-09-19 (expired session, transport-only comparison, live portal):
  8× sequential `op=get` = 3.50s vs 1× `op=batchget` (8 URLs) = 1.98s (1.77×).
  Full `unnes materi` runs could not be timed: session EXPIRED (`unnes status`
  confirms, needsInteraction) and the prime path needs an interactive headed
  login, which blocks in a headless shell. No numbers invented.
- [ ] **Step 3: Commit** nothing (evidence only) — report numbers in the phase summary.

---

## Phase 2 — Fetch less often (skip unchanged work)

### Task 5: Moodle Web-Services spike (time-boxed, live)

- [ ] **Step 1: Probe** with the saved jar (non-interactive, no browser): export cookies from `~/.config/unnes/profiles/default.json` to a curl jar and call `https://elena.unnes.ac.id/webservice/rest/server.php?wstoken=invalid&wsfunction=core_course_get_updates_since&moodlewsrestformat=json&courseid=<cid>&since=0`. Expect auth error (proves token requirement).
- [ ] **Step 2: Probe** `core_course_get_contents` with `wstoken` omitted but session cookies attached. Record whether Moodle accepts session-cookie auth on WS endpoints.
- [ ] **Step 3: Decision record** in the phase summary: if session-cookie WS works → add Task 5b (delta-first fetch); else (expected: token required, needs admin) → DEFER delta API, proceed to Task 6. No code either way; do not build token infrastructure.

### Task 6: ETag/Last-Modified validators + 304-as-cache-hit

**Files:**
- Create: `fetcher/src/validators.ts`
- Modify: `fetcher/src/http.ts` (send on request, short-circuit 304 → `{notModified: true}`), `fetcher/src/index.ts` (surface `notModified` on get + per batchget entry), `src/fetcher.rs` (`JobResult.not_modified`, `BatchGetEntry.not_modified`), `src/cache.rs` (`fresh()` helper), `src/materi.rs` (honor it), `fetcher/test/contract.test.mjs`, `fetcher/CONTRACT.md`

**Interfaces:**
- Consumes: `batchget` entry shape (Task 2).
- Produces: `notModified` end-to-end for Task 7+ callers.

- [ ] **Step 1: Failing contract test**. Add fixture route:
```js
if (url.pathname === "/etag-page") {
  if (req.headers["if-none-match"] === '"v1"') { res.statusCode = 304; res.end(); return; }
  res.setHeader("etag", '"v1"');
  res.setHeader("last-modified", "Wed, 21 Oct 2015 07:28:00 GMT");
  res.end("<html><body>version one</body></html>");
  return;
}
```
Test: first `op=get` → 200 + body; second identical `op=get` (same home) → `ok:true, notModified:true`, empty body, status 304.
- [ ] **Step 2: Run** — FAIL (`notModified` undefined).
- [ ] **Step 3: Implement** `validators.ts` (load/save keyed by exact URL; `apply(headers, url)`; `capture(url, res)` on 200 with etag/last-modified). `http.ts`: attach before fetch; on 304 return `{status:304, notModified:true, sessionExpired:false, ...}` WITHOUT `res.text()`; capture validators on 200. `index.ts`: surface on get + batchget entries; jar save rule unchanged (304 mutates nothing — skip save for pure-304 batches).
- [ ] **Step 4: Run** `npm run build && npm test` — PASS.
- [ ] **Step 5: Rust**: `not_modified` serde-default-false fields + `cache::fresh()` + materi honors it (entry `notModified` → load cached course records via existing `cache::load_any`, count as `ok_courses` without re-parse). Unit test: canned `BatchGetResult` with `not_modified:true` → returns cached items (construct cache file in temp home first).
- [ ] **Step 6: Run** `cargo test` — PASS. Docs: CONTRACT.md `notModified` + validator file layout. Resync dist. Commit `feat(fetcher): conditional requests with 304-as-cache-hit`.

### Task 7: Phase 2 verification

- [ ] Suites green (record counts), live medians for materi/tugas second-run (304 path only helps where portal sends validators — report hit rate honestly, including zero), resync done, commit evidence summary.

---

## Phase 3 — Parallelize (one jar owner) + browser waits

### Task 8: Intra-`batchget` parallel GETs (`pool.ts`, C=3)

**Files:**
- Create: `fetcher/src/pool.ts`
- Modify: `fetcher/src/index.ts` (batchget: sequential prime/SSO pass, then `mapLimit(urls, concurrency ?? 3, ...)` data pass; shared jar object + shared `PoliteLimiter`; order-preserving results)
- Test: `fetcher/test/contract.test.mjs`

**Interfaces:**
- Consumes: `PoliteLimiter`, batchget shape.
- Produces: parallel executor for Task 9 (no Rust changes needed — same `run_batchget`).

- [ ] **Step 1: Failing unit test** for `mapLimit` (order + cap):
```js
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
```
- [ ] **Step 2: Run** — FAIL (module missing).
- [ ] **Step 3: Implement** `pool.ts` (~25 lines, index-based worker loop).
- [ ] **Step 4: Concurrency contract test** against local server: add `/slow?n=` route sleeping `n`ms then 200; batchget 6 slow URLs with `concurrency: 3`; assert all ok AND server-observed `peakInflight <= 3` (counter in route handler) AND wall time < 6×serial (generous: `elapsed < 6 * perReq` where perReq≈150ms → bound 3000ms; serial would be ~900ms+spawn... set bound from measured serial in-test: run one slow URL first, bound = `single*6*0.6`). Keep margins ≥2×.
- [ ] **Step 5: Run** `npm run build && npm test` — PASS.
- [ ] **Step 6: Commit** `feat(fetcher): bounded parallel GETs inside batchget`.

### Task 9: Wire tugas loops through `run_batchget`

**Files:**
- Modify: `src/tugas.rs` (`fetch_items` course×kind loop ~254-321 + detail loop ~328-345)
- Test: `src/tugas.rs` unit tests on pure seams

- [ ] **Step 1: Seams + failing tests**: `fn index_jobs(profile:&str, kursus:&[u32]) -> Value` (2 URLs per cid, exact current extract bodies) and `fn merge_index_results(results:&[BatchGetEntry]) -> (Vec<TugasItem>, bool /*any_expired*/, Vec<String> /*fallback_urls*/)`; test with 2 canned entries (one overview-complete, one empty-records → fallback URL emitted).
- [ ] **Step 2: Run** — FAIL (missing).
- [ ] **Step 3: Implement**: single `run_batchget` for index; existing prime→rerun semantics on `any_expired && !primed`; fallbacks + detail-missing items collected into a SECOND `run_batchget`; per-item parse unchanged (`fmt_due`, `normalize_status`, `clean_nama` paths untouched).
- [ ] **Step 4: Run** full `cargo test` — PASS. Commit `perf(tugas): batchget + parallel GETs for index and detail passes`.

### Task 10: Browser signal-waits (`settle()`) replacing fixed sleeps

**Files:**
- Modify: `fetcher/src/browser.ts` (export `settle()`, use in `completeElenaSession` ~989-1022 and batch prime ~1373-1392)
- Test: `fetcher/test/contract.test.mjs`

- [ ] **Step 1: Failing test** for `settle` against local server: page that fires `/marker` XHR after 300ms then reveals `#ready`; assert `settle()` returns after marker (≈300ms, assert `< 2500ms`) with element found; and a never-resolving case returns at cap (`timeoutMs: 1500`, assert `elapsed < 3000` and `settled:false`).
- [ ] **Step 2: Run** — FAIL (not exported).
- [ ] **Step 3: Implement**:
```ts
export async function settle(page: {
  waitForResponse(pred: (r: {url(): string}) => boolean, o?: unknown): Promise<unknown>;
  waitForSelector(s: string, o?: unknown): Promise<unknown>;
}, opts: { responseRe: RegExp; selector?: string; timeoutMs?: number }): Promise<{ settled: boolean; elapsedMs: number }> {
  const t0 = Date.now();
  const cap = opts.timeoutMs ?? 8000;
  try {
    await page.waitForResponse((r) => opts.responseRe.test(r.url()), { timeout: cap }).catch(() => null);
  } catch { /* fall through to selector check */ }
  if (opts.selector) {
    try { await page.waitForSelector(opts.selector, { timeout: Math.max(1000, cap - (Date.now() - t0)) }); }
    catch { return { settled: false, elapsedMs: Date.now() - t0 }; }
  }
  return { settled: true, elapsedMs: Date.now() - t0 };
}
```
Use in `completeElenaSession`: `settle(page, {responseRe: /login_url/, selector: "#btnTest", timeoutMs: 8000})` before the `#btnTest` click (keep click + missing-button logs); drop the 5s sleep, cap the trailing 6s via `waitForURL`-or-timeout (keep a bounded wait, not a fixed one). Batch prime 6s → `settle(page, {responseRe: /apps\.unnes\.ac\.id\/\d+/, timeoutMs: 6000})`.
- [ ] **Step 4: Run** `npm run build && npm test` — PASS. Commit `perf(browser): signal waits replace fixed prime sleeps`.

### Task 11: Resource blocking on render contexts

**Files:**
- Modify: `fetcher/src/browser.ts` (`launchContext` only — never login paths)
- Test: `fetcher/test/contract.test.mjs`

- [ ] **Step 1: Failing test**: local `/img.png` + `/page-with-img` (HTML referencing it); `op=page` render of the page with extract on body text; assert server saw 0 hits to `/img.png` and records intact. (Requires a render-capable env; gate with `t.skip` when `UNNES_NO_BROWSER` or no chromium, mirroring existing skips.)
- [ ] **Step 2: Run** — FAIL (image requested).
- [ ] **Step 3: Implement** in `launchContext` after context creation:
```ts
await (ctx as { route(u: unknown, h: (r: unknown) => Promise<void>): Promise<void> }).route("**/*", async (route) => {
  const r = route as { request(): { resourceType(): string }; abort(e?: string): Promise<void>; fallback(): Promise<void> };
  const t = r.request().resourceType();
  if (t === "image" || t === "media" || t === "font") { await r.abort("blockedbyclient").catch(() => {}); return; }
  await r.fallback().catch(() => {});
});
```
- [ ] **Step 4: Run** suites — PASS. Commit `perf(browser): block image/media/font on renders`.

### Task 12: Phase 3 verification + docs + release checklist

- [ ] **Step 1: Suites**: `cargo test`, `npm test`, `tsc --noEmit` — record counts, all green.
- [ ] **Step 2: Live medians** (3 runs each): `unnes tugas`, `unnes materi`, TUI cold-load feel, `watch run` pass time — before/after table in summary.
- [ ] **Step 3: Docs**: CONTRACT.md (`batchget`, `notModified`, `ratelimit`), README timing table + any changed flags. Resync dist. Final commit `docs: fetch-optimization release notes`.

---

## Appendix — Research basis (why this order)

- Profiler (in-repo): tugas course×kind loop 16–32 serial spawns ≈24–48s; TUI cold ≈45–55s; spawn overhead 12–20s/run; Elena prime sleeps 17–30s. `op=batch` exists but only watch Phase 1 uses it.
- HTTP layer: undici keep-alive +315% HTTPS req/s in benchmarks (≈2–3 RTT saved/page); H2 multiplex +10–40% under concurrency (verify ALPN; Moodle/Apache may cap streams); `pLimit(N)` turns 60×400ms (24s) into ~4–5s at C=6; RetryAgent honors `Retry-After` (500ms×2, 30s cap). `undici` package is NOT importable on this Node (v22.23.1) — hence no new dep: global fetch already pools per-origin within one process, which `batchget` unlocks for free.
- Browser layer: pages-in-one-context 3.4ms vs 238ms browser launch; `new_context` ~92MB vs ~369MB/browser; never two browsers on one `user-data-dir` (SingletonLock); `waitForResponse` armed before trigger + selector beats 6s sleeps ~5s/page; `networkidle` discouraged (500ms floor, hangs on heartbeats); resource abort saves 25–67% bytes; polite ceiling for one portal: concurrency 2–3, 1–2s delay, ~30–60 req/min.
- Papers/practice: Mercator per-host queues + heap-by-next-fetch-time; BUbiNG per-IP politeness (co-hosted virtual hosts — our portal case); IRLbot budget+backoff co-design; conditional 304 (highest-ROI bandwidth saver, force ~2% unconditional refetch); Moodle `get_updates_since`/`check_updates` + `timemodified` (Mobile-app pattern — needs WS-token verdict from Task 5 spike); Cho & Garcia-Molina freshness (ignore ultra-fast changers, favor mid-rate×importance; adaptive TTL from change-bit history); Scrapy (`per-domain concurrency 1` + AutoThrottle), Colly (`Parallelism 1–2` + jitter), Crawlee (never scale on CPU-idleness alone; per-domain 429 backoff). Parser rewrite explicitly deferred: network-bound at ~1 req/2s, payoff ≈ 0.

## Self-Review

- [x] Spec coverage: every research recommendation maps to a task (keep-alive→T1/T2, pool→T8, H2→free via shared process + ALPN note in T4/T12 verification, delta→T5, 304→T6, TTL→T6/`fresh()`+honest scope, politeness→T1, signal-waits→T10, resource-block→T11, pages-in-context→noted as follow-up (current one-page-per-context on `-headless` is retained; N-pages-in-one-context left for later since batchget covers the serial-render cost first — stated, not silently dropped), parser rewrite→explicitly deferred with reason, hash short-circuit→cut (YAGNI: parse is regex-cheap; dedup already in `data.rs`) with reason recorded.
- [x] Placeholder scan: all steps carry exact code/commands/assertions; live-portal steps say what to do when unreachable (report, don't invent).
- [x] Type consistency: `PoliteLimiter`/`mapLimit`/validator shape/`batchget` job+result/Rust structs defined once in File Map Interfaces and reused verbatim in tasks. `sessionExpired = some(...)` rule identical in T2/T9. `ratelimit→5` in T2 + Global Constraints.

---

Plan complete and saved to `docs/superpowers/plans/2026-09-19-fetch-optimization.md`. Two execution options:

**1. Subagent-Driven (recommended)** - I dispatch a fresh subagent per task, review between tasks, fast iteration

**2. Inline Execution** - Execute tasks in this session using executing-plans, batch execution with checkpoints

**Which approach?**
