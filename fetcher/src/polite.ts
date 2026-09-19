/**
 * Per-host politeness gate: serializes bursts per host with an optional
 * floor, and pushes the host's next-allowed time into the future on
 * 429/backoff signals (Retry-After honored, capped). Default floor is 0:
 * this CLI fetches in short bursts (a handful of requests, then idle for
 * an hour), and the pre-existing behavior was zero inter-request delay -
 * a 1s default made one batchget run measurably SLOWER than the sequential
 * status quo (7.7s vs 3.5s for 8 URLs, 2026-09-19). The reactive half
 * (Retry-After + backoff) is always on; raise the floor explicitly if 429s
 * ever appear. Pure logic except the sleep itself.
 */
export class PoliteLimiter {
  private next = new Map<string, number>();
  constructor(private opts: { minDelayMs?: number; rand?: () => number } = {}) {}

  private delayMs(): number {
    const base = this.opts.minDelayMs ?? 0;
    const r = this.opts.rand ? this.opts.rand() : Math.random();
    return Math.round(base * (0.5 + r));
  }

  /** Sleep until `host` may be hit again, then reserve the next slot. */
  async waitFor(host: string): Promise<void> {
    const h = host.toLowerCase();
    const wait = (this.next.get(h) ?? 0) - Date.now();
    this.next.set(h, Date.now() + Math.max(0, wait) + this.delayMs());
    if (wait > 0) await new Promise((r) => setTimeout(r, wait));
  }

  /** Fold a completed outcome back in: failures (429/5xx) extend the ban. */
  noteResult(host: string, ok: boolean, retryAfterSecs?: number | null): void {
    const h = host.toLowerCase();
    if (ok) return;
    const extra =
      retryAfterSecs && retryAfterSecs > 0 ? Math.min(retryAfterSecs, 120) * 1000 : 5000;
    this.next.set(h, Math.max(this.next.get(h) ?? 0, Date.now() + extra));
  }
}
