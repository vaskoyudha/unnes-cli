
import { chromium } from "playwright";
import { join } from "node:path";
import { homedir } from "node:os";
import { mkdirSync } from "node:fs";
const home = process.env.UNNES_HOME || join(homedir(), ".config", "unnes");
const browserDir = join(home, "browser-profiles", "default");
mkdirSync(browserDir, { recursive: true });
const ctx = await chromium.launchPersistentContext(browserDir, { headless: true });
const page = await ctx.newPage();
await page.goto("https://apps.unnes.ac.id/23", { waitUntil: "domcontentloaded", timeout: 60000 });
await page.waitForTimeout(10000);
console.log("prime url:", page.url());
if (page.url().includes("/login")) { console.log("STILL LOGGED OUT - session not ready"); await ctx.close(); process.exit(1); }
const info = await page.evaluate(() => {
  const html = document.documentElement.outerHTML;
  const logout = (html.match(/app_logout_url\s*=\s*'([^']+)'/) || [])[1];
  const tok = (html.match(/sso_token\s*:\s*'([^']+)'/) || [])[1];
  const urls = [...new Set([...html.matchAll(/https:\/\/[a-z0-9.]+\.[a-z0-9.]+\.ac\.id[^"'\s]*/gi)].map(m => m[0]))].slice(0, 20);
  return { title: document.title, logout: logout || "(none)", token: !!tok, body: document.body.innerText.replace(/\s+/g, " ").slice(0, 400), urls };
});
console.log("=== APPS/23 DETAIL ===");
console.log(JSON.stringify(info, null, 1));
await ctx.close();
