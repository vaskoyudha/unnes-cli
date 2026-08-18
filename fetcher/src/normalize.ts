// Content normalization before hashing: strip everything that rotates per request
// (CSRF tokens, session-specific bits) so watches only fire on real changes.

export function normalizeHtml(html: string, extraRegexes: string[] = []): string {
  let s = html;
  s = s.replace(/<input[^>]*name=["']?_token["']?[^>]*>/gi, "");
  s = s.replace(/<meta[^>]*name=["']csrf-token["'][^>]*>/gi, "");
  s = s.replace(/<meta[^>]*name=["']x-csrf-token["'][^>]*>/gi, "");
  s = s.replace(/[?&]_token=[^&#\s]+/g, "");
  for (const re of extraRegexes) {
    try {
      s = s.replace(new RegExp(re, "g"), "");
    } catch {
      /* invalid user-supplied regex: ignore silently */
    }
  }
  s = s.replace(/[ \t]+/g, " ");
  return s.split("\n").map((l) => l.trim()).join("\n").trim();
}
