import * as cheerio from "cheerio";

export interface ExtractSpec {
  /** CSS selector yielding one element per record */
  selector: string;
  /** field name -> CSS selector relative to the record element; value "" = element itself */
  fields?: Record<string, string>;
}

export function extractRecords(html: string, spec: ExtractSpec): Record<string, string>[] {
  const $ = cheerio.load(html);
  const out: Record<string, string>[] = [];
  $(spec.selector).each((_, el) => {
    const rec: Record<string, string> = {};
    const fields = spec.fields ?? {};
    const keys = Object.keys(fields);
    if (keys.length === 0) {
      rec.text = $(el).text().replace(/\s+/g, " ").trim();
    } else {
      for (const k of keys) {
        const sel = fields[k];
        if (!sel || sel.trim() === "") {
          rec[k] = $(el).text().replace(/\s+/g, " ").trim();
          continue;
        }
        // "@attr" extracts an attribute of the record element itself;
        // "sel@attr" of the first matched descendant.
        const at = sel.lastIndexOf("@");
        if (sel.startsWith("@")) {
          const attr = sel.slice(1);
          const v = $(el).attr(attr) ?? "";
          rec[k] = v.trim();
          continue;
        }
        if (at > 0) {
          const css = sel.slice(0, at);
          const attr = sel.slice(at + 1);
          const v = $(el).find(css).first().attr(attr) ?? "";
          rec[k] = v.trim();
          continue;
        }
        rec[k] = $(el).find(sel).first().text().replace(/\s+/g, " ").trim();
      }
    }
    out.push(rec);
  });
  return out;
}
