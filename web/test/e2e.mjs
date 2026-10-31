// End-to-end check of the built page in headless Chrome:
//
//   node web/test/e2e.mjs OLD.docx NEW.docx
//   SCREENSHOT=page.png node web/test/e2e.mjs OLD.docx NEW.docx   (also saves a screenshot)
//
// Opens web/dist/xxf.html, picks both files, compares, downloads the redline,
// and checks that: the page reports no errors or CSP violations, it made no
// network requests (and blocks one when tried), its change counts match the
// xxf CLI, and the downloaded redline accepts back to NEW (via
// `xxf NEW REDLINE`, which must find no differences). Uses Chrome's DevTools
// protocol directly: no npm packages.

import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const [oldPath, newPath] = process.argv.slice(2).map((p) => resolve(p));
if (!oldPath || !newPath) {
  console.error("usage: node web/test/e2e.mjs OLD.docx NEW.docx");
  process.exit(2);
}
const root = resolve(import.meta.dirname, "../..");
const page = join(root, "web/dist/xxf.html");
const chromePath = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const work = mkdtempSync(join(tmpdir(), "xxf-e2e-"));
const downloads = join(work, "downloads");
const port = 9300 + Math.floor(Math.random() * 500);

const chrome = spawn(chromePath, [
  "--headless=new", `--remote-debugging-port=${port}`, `--user-data-dir=${join(work, "profile")}`,
  "--no-first-run", "--no-default-browser-check", "about:blank",
], { stdio: "ignore" });

const failures = [];
const fail = (msg) => failures.push(msg);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

try {
  // Wait for the DevTools endpoint, then attach to the page.
  let targets;
  for (let i = 0; i < 100 && !targets; i++) {
    try { targets = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json(); } catch { await sleep(100); }
  }
  const target = targets.find((t) => t.type === "page");
  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener("open", r, { once: true }));
  let nextId = 0;
  const pending = new Map();
  const events = [];
  ws.addEventListener("message", ({ data }) => {
    const msg = JSON.parse(data);
    if (msg.id && pending.has(msg.id)) {
      const { ok, err } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? err(new Error(msg.error.message)) : ok(msg.result);
    } else if (msg.method) {
      events.push(msg);
    }
  });
  const send = (method, params = {}) => new Promise((ok, err) => {
    const id = ++nextId;
    pending.set(id, { ok, err });
    ws.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression) =>
    (await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true })).result.value;

  for (const d of ["Page", "Runtime", "Log", "Network", "DOM"]) await send(`${d}.enable`);
  await send("Browser.setDownloadBehavior", { behavior: "allow", downloadPath: downloads });
  await send("Page.navigate", { url: `file://${page}` });
  for (let i = 0; i < 100 && !(await evaluate("document.readyState === 'complete'").catch(() => false)); i++) {
    await sleep(100);
  }

  const { root: doc } = await send("DOM.getDocument");
  for (const [sel, file] of [["#file-old", oldPath], ["#file-new", newPath]]) {
    const { nodeId } = await send("DOM.querySelector", { nodeId: doc.nodeId, selector: sel });
    await send("DOM.setFileInputFiles", { nodeId, files: [file] });
  }
  await evaluate("document.getElementById('compare').click()");
  let state;
  for (let i = 0; i < 6000; i++) {
    state = await evaluate(`({
      done: !document.getElementById('result').hidden,
      error: document.getElementById('status').classList.contains('error'),
      status: document.getElementById('status').textContent })`);
    if (state.done || state.error) break;
    await sleep(100);
  }
  if (!state.done) fail(`comparison did not finish: ${state.status}`);
  console.log(`page: ${state.status}`);

  const pageCounts = await evaluate(
    "[...document.querySelectorAll('#summary > span')].map(s => s.textContent).join(', ')");
  console.log(`page: ${pageCounts}`);

  // The CLI's summary line should report the same counts.
  const xxf = join(root, "target/release/xxf");
  spawnSync("cargo", ["build", "--release", "--quiet"], { cwd: root, stdio: "inherit" });
  const cli = spawnSync(xxf, ["--no-color", oldPath, newPath], { encoding: "utf8" }).stdout.trim().split("\n").pop();
  for (const [n, k] of [...pageCounts.matchAll(/(\d+)([a-z ]+)/g)].map((m) => [m[1], m[2].trim()])) {
    if (k !== "empty paragraphs" && !cli.includes(`${n} ${k}`)) fail(`page says ${n} ${k}, CLI says: ${cli}`);
  }

  if (state.done) {
    await evaluate("document.getElementById('download').click()");
    let file;
    for (let i = 0; i < 100 && !file; i++) {
      file = (() => { try { return readdirSync(downloads).find((f) => f.endsWith(".docx")); } catch {} })();
      if (!file) await sleep(100);
    }
    if (!file) fail("redline was not downloaded");
    else {
      const check = spawnSync(xxf, ["--no-color", newPath, join(downloads, file)], { encoding: "utf8" });
      if (check.status !== 0) fail(`redline does not accept back to NEW:\n${check.stdout}`);
      else console.log(`redline: ${file} accepts back to the new document`);
    }
  }

  // Errors, CSP violations, and any network traffic beyond the page itself.
  for (const e of events) {
    if (e.method === "Runtime.exceptionThrown") fail(`exception: ${e.params.exceptionDetails.text}`);
    if (e.method === "Log.entryAdded" && ["error", "warning"].includes(e.params.entry.level)) {
      fail(`console ${e.params.entry.level}: ${e.params.entry.text}`);
    }
    if (e.method === "Network.requestWillBeSent") {
      const url = e.params.request.url;
      if (!/^(file|data|blob):/.test(url)) fail(`network request: ${url}`);
    }
  }
  if (process.env.SCREENSHOT) {
    await send("Emulation.setDeviceMetricsOverride", { width: 1100, height: 1400, deviceScaleFactor: 1, mobile: false });
    const { data } = await send("Page.captureScreenshot", { format: "png" });
    (await import("node:fs")).writeFileSync(process.env.SCREENSHOT, Buffer.from(data, "base64"));
  }

  // Finally, prove the policy holds: an attempted upload must be blocked.
  const probe = await evaluate(
    "fetch('https://example.com/', { method: 'POST', body: 'x' }).then(() => 'sent', () => 'blocked')");
  if (probe !== "blocked") fail(`a network request from the page was not blocked (${probe})`);
  ws.close();
} catch (e) {
  fail(e.stack);
} finally {
  chrome.kill();
  await sleep(200);
  rmSync(work, { recursive: true, force: true });
}

if (failures.length) {
  console.error(`\nFAILED:\n- ${failures.join("\n- ")}`);
  process.exit(1);
}
console.log("ok: no errors, no CSP violations, no network requests; an attempted upload was blocked");
