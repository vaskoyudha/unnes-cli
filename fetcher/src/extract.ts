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
        const text = sel && sel.trim() !== "" ? $(el).find(sel).first().text() : $(el).text();
        rec[k] = text.replace(/\s+/g, " ").trim();
      }
    }
    out.push(rec);
  });
  return out;
}
