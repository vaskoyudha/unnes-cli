/**
 * Bounded parallel map: at most `limit` invocations of `fn` in flight,
 * results in input order. One item's slowness never blocks others beyond
 * the cap; `fn` must never reject the batch (wrap fallible work at the
 * call site) - a rejection here aborts remaining items, which is reserved
 * for programmer bugs, not fetch outcomes.
 */
export async function mapLimit<T, R>(
  items: T[],
  limit: number,
  fn: (item: T, index: number) => Promise<R>,
): Promise<R[]> {
  const n = Math.max(1, Math.floor(limit));
  const out = new Array<R>(items.length);
  let next = 0;
  const workers: Promise<void>[] = [];
  for (let w = 0; w < Math.min(n, items.length); w++) {
    workers.push(
      (async () => {
        for (;;) {
          const i = next++;
          if (i >= items.length) return;
          out[i] = await fn(items[i], i);
        }
      })(),
    );
  }
  await Promise.all(workers);
  return out;
}
