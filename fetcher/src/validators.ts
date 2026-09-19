import { readFile, writeFile, chmod, mkdir, rename } from "node:fs/promises";
import { dirname } from "node:path";

export interface UrlValidators {
  etag?: string;
  lastModified?: string;
}

interface ValidatorFile {
  version: number;
  urls: Record<string, UrlValidators>;
}

/** Validator store path derived from the jar path:
 * profiles/default.json -> profiles/default.validators.json */
export function validatorsPath(jarPath: string): string {
  return jarPath.replace(/\.json$/, ".validators.json");
}

export async function loadValidators(jarPath: string): Promise<Record<string, UrlValidators>> {
  try {
    const raw = await readFile(validatorsPath(jarPath), "utf8");
    const parsed = JSON.parse(raw) as ValidatorFile;
    if (parsed.version !== 1 || typeof parsed.urls !== "object" || !parsed.urls) return {};
    return parsed.urls;
  } catch {
    return {};
  }
}

export async function saveValidators(
  jarPath: string,
  urls: Record<string, UrlValidators>,
): Promise<void> {
  const path = validatorsPath(jarPath);
  await mkdir(dirname(path), { recursive: true });
  const tmp = `${path}.tmp.${process.pid}`;
  await writeFile(tmp, JSON.stringify({ version: 1, urls }));
  await chmod(tmp, 0o600);
  await rename(tmp, path);
}

/** Attach If-None-Match / If-Modified-Since for a URL seen before (both
 * when we have both — RFC 9110: the server ignores Last-Modified when
 * If-None-Match is present). */
export function applyValidators(
  headers: Record<string, string>,
  url: string,
  store: Record<string, UrlValidators>,
): void {
  const v = store[url];
  if (!v) return;
  if (v.etag) headers["if-none-match"] = v.etag;
  if (v.lastModified) headers["if-modified-since"] = v.lastModified;
}

/** Capture validators from a 200 response. Returns true when the store
 * changed (caller saves only then). Keyed by requested URL to match the
 * next run's apply key; servers that send neither validator drop any
 * stale entry so a changed page is never skipped on old data. */
export function captureValidators(
  url: string,
  res: Response,
  store: Record<string, UrlValidators>,
): boolean {
  const etag = res.headers.get("etag");
  const lm = res.headers.get("last-modified");
  if (!etag && !lm) {
    if (store[url]) {
      delete store[url];
      return true;
    }
    return false;
  }
  const next: UrlValidators = { ...(etag ? { etag } : {}), ...(lm ? { lastModified: lm } : {}) };
  const prev = store[url];
  if (prev?.etag === next.etag && prev?.lastModified === next.lastModified) return false;
  store[url] = next;
  return true;
}
