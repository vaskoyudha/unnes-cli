/**
 * Per-host politeness gate: at most one request burst per host per floor
 * interval, with jitter so concurrent workers don't march in lockstep.
 * A 429/backoff signal pushes the host's next-allowed time into the future
 * (Retry-After honored, capped). Pure logic except the sleep itself.
 */
export class PoliteLimiter {
  private next = new Map<string, number>();
  constructor(private opts: { minDelayMs?: number; rand?: () => number } = {}) {}

  private delayMs(): number {
    const base = this.opts.minDelayMs ?? 1000;
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
