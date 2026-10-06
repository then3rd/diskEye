// Screenshots of the web UI for the README. Usage:
//   node web.mjs <diskeye binary> <snapshot> <output dir>
// Expects DISKEYE_NOW, TZ and XDG_STATE_HOME from `just screenshots`.
import { spawn } from "node:child_process";
import { chromium } from "playwright";

const [bin, snapshot, out] = process.argv.slice(2);
const TABS = ["overview", "physical", "files", "workloads", "reclaim", "diff"];

const server = spawn(bin, ["serve", snapshot, "--no-open", "--port", "7979"], { stdio: ["ignore", "ignore", "pipe"] });
const url = await new Promise((resolve, reject) => {
  let buf = "";
  server.stderr.on("data", (d) => {
    buf += d;
    const m = buf.match(/web UI: (\S+)/);
    if (m) resolve(m[1]);
  });
  server.on("exit", (code) => reject(new Error(`diskeye serve exited (${code}):\n${buf}`)));
});

const browser = await chromium.launch();
try {
  const ctx = await browser.newContext({
    viewport: { width: 1400, height: 900 },
    colorScheme: "light",
    locale: "en-US",
    timezoneId: "UTC",
    reducedMotion: "reduce",
  });
  for (const tab of TABS) {
    const page = await ctx.newPage();
    await page.clock.setFixedTime(Number(process.env.DISKEYE_NOW) * 1000);
    await page.goto(`${url}&tab=${tab}`, { waitUntil: "networkidle" });
    await page.waitForTimeout(1000); // let chart transitions settle
    await page.screenshot({ path: `${out}/web-${tab}.png` });
    await page.close();
  }
} finally {
  await browser.close();
  server.kill();
}
