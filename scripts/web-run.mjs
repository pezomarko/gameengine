#!/usr/bin/env node
// Run the browser client in headless Chromium and print what it says (docs/WEB.md 8).
//   node scripts/web-run.mjs --url URL [--seconds N] [--screenshot FILE [--at S]] [--software]
//                            [--chrome BIN] [--size WxH] [--profile DIR] [--cache NAME]
//                            [--login EMAIL --password PW [--register]] [--click-canvas S]
//                            [--mobile [WxH@DPR]] [--touch S]
// --cache NAME reads every entry of that Cache API cache back at the end and prints
// "GM-CACHE entries=N bytes=M": what the browser holds, whatever the page believes.
// --login fills the page's own form as a person would (docs/CLIENT.md 4.1): a click into the
// field, the text, Tab, the password, Enter, all as input events of the browser itself.
// --click-canvas S clicks the middle of the game's canvas S seconds in, as a person's first
// click would, and prints "web-run: after a click the pointer is held by: ID" (the element
// the browser gave the pointer to, or "nothing"): the game asks for the pointer on a click.
// --mobile makes the page a phone's (docs/WEB.md 3.5): a touch screen of W by H CSS pixels at
// DPR device pixels each (a Galaxy S23 held sideways by default: 892x412@2.625, which is
// 2340 by 1080 device pixels), with the browser's touch emulation. --touch S plays the
// phone's controls S seconds in, as fingers: the stick on the left held forward for a
// second and a half, a swipe across the right, a tap on the right; and prints
// "web-run: touched". The client's report then shows the body moved and the camera turned.
// Console lines go to stdout as they come. Exit 0 when the page reported GM-DONE, 1 on
// GM-ERROR, on a timeout, and on anything that goes wrong on the way (the browser is ended
// and its files removed in every case). Needs Node 22+ (its built-in WebSocket) and a Chromium.
import { spawn } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const args = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 ? args[i + 1] : fallback;
};
const url = opt("--url");
if (!url) {
  console.error("web-run: --url is required");
  process.exit(2);
}
const seconds = Number(opt("--seconds", "30"));
const screenshot = opt("--screenshot");
const shotAt = Number(opt("--at", String(Math.max(1, seconds - 2))));
const size = opt("--size", "1280x720").replace("x", ",");
const chrome = opt("--chrome", process.env.CHROME || "chromium");
const software = args.includes("--software");
const mobile = args.includes("--mobile")
  ? (() => {
      const spec = args[args.indexOf("--mobile") + 1];
      const m = /^(\d+)x(\d+)@([\d.]+)$/.exec(spec || "");
      return m ? { width: +m[1], height: +m[2], dpr: +m[3] } : { width: 892, height: 412, dpr: 2.625 };
    })()
  : null;
const touchAt = opt("--touch");

// --profile DIR keeps the browser's storage between runs (the model cache lives there).
const kept = opt("--profile");
const profile = kept || mkdtempSync(join(tmpdir(), "gm-web-"));
// The browser's own temporary files (a directory per run for its singleton socket) go
// where they are removed with the run.
const scratch = mkdtempSync(join(tmpdir(), "gm-web-tmp-"));
const port = 9300 + (process.pid % 600);
const flags = [
  "--headless=new", "--no-first-run", "--no-default-browser-check", `--user-data-dir=${profile}`,
  `--remote-debugging-port=${port}`, `--window-size=${size}`, "--disable-gpu-sandbox",
  // WebGPU in headless Linux Chromium is behind these; with --use-angle=vulkan it is the
  // machine's GPU, without it SwiftShader (what CI has).
  "--enable-unsafe-webgpu", "--enable-features=Vulkan",
  ...(software ? [] : ["--use-angle=vulkan"]),
  "about:blank",
];
const browser = spawn(chrome, flags, {
  stdio: ["ignore", "ignore", "pipe"],
  env: { ...process.env, TMPDIR: scratch },
});
// The browser's own last words, for a run that never reached the page (a machine without
// the GPU asked for, a browser that is not there, a profile it could not take).
const said = [];
browser.stderr.on("data", (chunk) => {
  for (const line of String(chunk).split("\n")) if (line.trim()) said.push(line);
  while (said.length > 40) said.shift();
});
let finished = false;
let gone = false;
const finish = (code) => {
  if (finished) return;
  finished = true;
  const leave = () => {
    for (const dir of kept ? [scratch] : [profile, scratch]) {
      try { rmSync(dir, { recursive: true, force: true }); } catch { /* still held */ }
    }
    process.exit(code);
  };
  if (gone) return leave();
  // The files go when the browser has: it still writes to them while it shuts down.
  browser.once("exit", leave);
  browser.kill("SIGTERM");
  setTimeout(() => { browser.kill("SIGKILL"); leave(); }, 3000);
};
const fail = (why) => {
  console.error("web-run: " + why);
  if (said.length) console.error("web-run: the browser said:\n" + said.join("\n"));
  finish(1);
};
browser.on("error", (e) => { gone = true; fail(`the browser did not start (${chrome}): ${e.message}`); });
browser.on("exit", () => { gone = true; if (!finished) fail("the browser exited"); });
// Whatever hangs (a renderer that stops answering, a page that never loads) ends here.
setTimeout(() => fail(`nothing came of it in ${seconds + 40} s`), (seconds + 40) * 1000);
process.on("uncaughtException", (e) => fail(String(e && e.stack ? e.stack : e)));
process.on("unhandledRejection", (e) => fail(String(e && e.stack ? e.stack : e)));
for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => fail(`stopped by ${signal}`));

async function target() {
  // Up to 60 s: a first start on a slow machine (a CI runner on its software GPU) can take
  // well over ten.
  for (let i = 0; i < 600 && !finished; i++) {
    try {
      const list = await (await fetch(`http://127.0.0.1:${port}/json/list`)).json();
      const page = list.find((t) => t.type === "page");
      if (page) return page.webSocketDebuggerUrl;
    } catch { /* not up yet */ }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error("no DevTools endpoint");
}

async function main() {
  const ws = new WebSocket(await target());
  let nextId = 1;
  const waiting = new Map();
  // One command of the DevTools protocol; an answer that is an error, or none in 20 s, ends
  // the run.
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const id = nextId++;
    const timer = setTimeout(() => reject(new Error(`${method}: no answer from the browser`)), 20000);
    waiting.set(id, (msg) => {
      clearTimeout(timer);
      if (msg.error) reject(new Error(`${method}: ${msg.error.message}`));
      else resolve(msg.result || {});
    });
    ws.send(JSON.stringify({ id, method, params }));
  });
  let verdict = null;
  ws.addEventListener("message", (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && waiting.has(msg.id)) {
      waiting.get(msg.id)(msg);
      waiting.delete(msg.id);
      return;
    }
    if (msg.method === "Runtime.consoleAPICalled") {
      const line = msg.params.args.map((a) => a.value ?? a.description ?? "").join(" ");
      console.log(line);
      if (line.startsWith("GM-DONE")) verdict ??= 0;
      if (line.startsWith("GM-ERROR")) verdict ??= 1;
    } else if (msg.method === "Runtime.exceptionThrown") {
      const d = msg.params.exceptionDetails;
      console.log("EXCEPTION " + (d.exception?.description || d.text));
    } else if (msg.method === "Log.entryAdded") {
      // What the browser itself says, beside what the page logs: a WebGPU or WebGL
      // validation error is said here and nowhere else, and the page plays on. Those are
      // printed as errors of the client (the gates fail on a line that begins so); the
      // rest (a favicon that is not there) is printed for whoever reads the log.
      const e = msg.params.entry;
      const text = `${e.source}: ${e.text}`.replace(/\s+/g, " ");
      const graphics = e.source === "rendering" || /WebGL|WebGPU|GPUValidationError|GL_INVALID/.test(e.text);
      if (e.level === "error" && graphics) console.log(`[ERROR] browser ${text}`);
      else if (e.level === "error" || e.level === "warning") console.log(`[BROWSER ${e.level}] ${text}`);
    }
  });
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve, { once: true });
    ws.addEventListener("error", () => reject(new Error("the DevTools socket did not open")), { once: true });
  });
  await send("Runtime.enable");
  await send("Log.enable");
  await send("Page.enable");
  if (mobile) {
    await send("Emulation.setDeviceMetricsOverride", {
      width: mobile.width, height: mobile.height, deviceScaleFactor: mobile.dpr, mobile: true,
      screenOrientation: { type: "landscapePrimary", angle: 90 },
    });
    await send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 });
    console.log(`web-run: a phone of ${mobile.width}x${mobile.height} CSS pixels at ${mobile.dpr}`);
  }
  await send("Page.navigate", { url });
  const started = Date.now();

  // The page's form, filled by input the browser cannot tell from a person's.
  const login = opt("--login");
  if (login) {
    const evaluate = async (expression) =>
      (await send("Runtime.evaluate", { expression, returnByValue: true })).result?.value;
    const middle = (id) => evaluate(`(() => {
      const r = document.getElementById(${JSON.stringify(id)}).getBoundingClientRect();
      return [r.left + r.width / 2, r.top + r.height / 2];
    })()`);
    const click = async (id) => {
      const [x, y] = await middle(id);
      for (const type of ["mousePressed", "mouseReleased"]) {
        await send("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1 });
      }
    };
    const key = async (name, code, text) => {
      const base = { key: name, code: name, windowsVirtualKeyCode: code, nativeVirtualKeyCode: code };
      await send("Input.dispatchKeyEvent", { type: text ? "keyDown" : "rawKeyDown", ...base, ...(text ? { text } : {}) });
      await send("Input.dispatchKeyEvent", { type: "keyUp", ...base });
    };
    let shown = false;
    for (let i = 0; i < 300 && !shown && verdict === null; i++) {
      shown = await evaluate(`(() => {
        const panel = document.getElementById("panel"), user = document.getElementById("user");
        return !!panel && !panel.hidden && !!user && !user.disabled;
      })()`);
      if (!shown) await new Promise((r) => setTimeout(r, 100));
    }
    if (!shown) {
      fail("the page never showed its login form");
    } else {
      const password = opt("--password", "");
      const register = args.includes("--register");
      // A new account ticks its box first: the form then asks for the password twice.
      if (register) await click("register");
      await click("user");
      await send("Input.insertText", { text: login });
      await key("Tab", 9);
      await send("Input.insertText", { text: password });
      if (register) {
        await key("Tab", 9);
        await send("Input.insertText", { text: password });
      }
      await key("Enter", 13, "\r");
      const said = await evaluate(`document.getElementById("notice").textContent`);
      console.log(`web-run: the login form was filled and sent (the form says: ${said})`);
    }
  }
  // Fingers, as the browser's touch events: a finger down, moved along a line over a
  // time, lifted.
  const finger = async (id, from, to, ms) => {
    const point = (x, y) => ({ x, y, id, radiusX: 4, radiusY: 4, force: 1 });
    await send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [point(from[0], from[1])] });
    const steps = Math.max(1, Math.round(ms / 25));
    for (let i = 1; i <= steps; i++) {
      await new Promise((r) => setTimeout(r, 25));
      const t = i / steps;
      await send("Input.dispatchTouchEvent", {
        type: "touchMove", touchPoints: [point(from[0] + (to[0] - from[0]) * t, from[1] + (to[1] - from[1]) * t)],
      });
    }
    await send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
  };
  let touched = touchAt === undefined;
  const clickAt = opt("--click-canvas");
  let clicked = clickAt === undefined;
  let shot = !screenshot;
  while (Date.now() - started < (seconds + 8) * 1000 && !finished) {
    await new Promise((r) => setTimeout(r, 100));
    if (!clicked && Date.now() - started >= Number(clickAt) * 1000) {
      clicked = true;
      const value = async (expression) =>
        (await send("Runtime.evaluate", { expression, returnByValue: true })).result?.value;
      const at = await value(`(() => {
        const c = document.getElementById("gm-canvas");
        if (!c) return null;
        const r = c.getBoundingClientRect();
        return [r.left + r.width / 2, r.top + r.height / 2];
      })()`);
      if (!at) {
        console.log("web-run: after a click the pointer is held by: nothing (the page has no canvas)");
      } else {
        for (const type of ["mousePressed", "mouseReleased"]) {
          await send("Input.dispatchMouseEvent", { type, x: at[0], y: at[1], button: "left", clickCount: 1 });
        }
        // The browser answers the asking a moment later.
        await new Promise((r) => setTimeout(r, 700));
        const holder = await value(`document.pointerLockElement ? (document.pointerLockElement.id || "an element without an id") : "nothing"`);
        console.log(`web-run: after a click the pointer is held by: ${holder}`);
      }
    }
    if (!touched && Date.now() - started >= Number(touchAt) * 1000) {
      touched = true;
      const w = mobile ? mobile.width : Number(size.split(",")[0]);
      const h = mobile ? mobile.height : Number(size.split(",")[1]);
      // The stick: landed at a fifth of the width, pushed up, held.
      await finger(1, [w * 0.2, h * 0.7], [w * 0.2, h * 0.45], 1500);
      // The look: a swipe across the right half.
      await finger(2, [w * 0.6, h * 0.5], [w * 0.9, h * 0.5], 400);
      // A tap on the right: the primary.
      await finger(3, [w * 0.75, h * 0.5], [w * 0.75, h * 0.5], 50);
      const seen = (await send("Runtime.evaluate", { expression: `new Promise((resolve) => {
        const c = document.getElementById("gm-canvas");
        if (!c) return resolve("no canvas");
        const said = (box) => resolve(c.width + "x" + c.height + " backing, " + c.clientWidth + "x" + c.clientHeight + " CSS, dpr " + devicePixelRatio + ", device-pixel box " + box);
        try {
          new ResizeObserver((entries) => {
            const b = entries[0].devicePixelContentBoxSize;
            said(b && b[0] ? b[0].inlineSize + "x" + b[0].blockSize : "none");
          }).observe(c, { box: "device-pixel-content-box" });
          setTimeout(() => said("no answer"), 1000);
        } catch (e) { said("error " + e.message); }
      })`, awaitPromise: true, returnByValue: true })).result?.value;
      console.log(`web-run: touched (the canvas is ${seen})`);
    }
    if (!shot && (Date.now() - started >= shotAt * 1000 || verdict !== null)) {
      shot = true;
      const png = await send("Page.captureScreenshot", { format: "png" });
      if (png.data) writeFileSync(screenshot, Buffer.from(png.data, "base64"));
    }
    if (verdict !== null && shot) break;
  }
  const cacheName = opt("--cache");
  if (cacheName) {
    const expr = `(async () => {
      const cache = await caches.open(${JSON.stringify(cacheName)});
      let entries = 0, bytes = 0;
      for (const request of await cache.keys()) {
        const response = await cache.match(request);
        if (response) { entries++; bytes += (await response.arrayBuffer()).byteLength; }
      }
      return "entries=" + entries + " bytes=" + bytes;
    })()`;
    const held = await send("Runtime.evaluate", { expression: expr, awaitPromise: true, returnByValue: true });
    console.log("GM-CACHE " + (held.result?.value ?? "unreadable"));
  }
  if (verdict === null) console.error("web-run: no GM-DONE within the time");
  finish(verdict ?? 1);
}
// Whatever goes wrong on the way ends the browser too.
main().catch((e) => fail(String(e && e.stack ? e.stack : e)));
