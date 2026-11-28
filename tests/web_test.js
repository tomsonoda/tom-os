#!/usr/bin/env node
// Tests the browser version (web/): loads the page in headless Chromium
// through Playwright and drives it with the keyboard, the pointer and
// touch. The demo "model" built into the page answers, so no key is needed.
//
//   make web && node tests/web_test.js
const http = require("http");
const fs = require("fs");
const path = require("path");
const { chromium, devices } = require("playwright");

const ROOT = path.join(__dirname, "..", "web");
const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".png": "image/png",
  ".webmanifest": "application/manifest+json",
};

function serve() {
  const server = http.createServer((request, response) => {
    const file = path.join(ROOT, request.url === "/" ? "index.html" : path.normalize(request.url.split("?")[0]));
    if (!file.startsWith(ROOT) || !fs.existsSync(file)) return response.writeHead(404).end();
    response.writeHead(200, { "Content-Type": TYPES[path.extname(file)] || "application/octet-stream" });
    fs.createReadStream(file).pipe(response);
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

let count = 0, failed = 0;
async function step(name, run) {
  count++;
  try {
    await run();
    console.log(`ok ${String(count).padStart(2)} - ${name}`);
  } catch (error) {
    failed++;
    console.log(`not ok ${String(count).padStart(2)} - ${name}\n    ${String(error.message || error).split("\n").join("\n    ")}`);
  }
}

// One page with tom-os in it, and what its serial console has printed.
async function open(browser, url, options) {
  const page = await (await browser.newContext(options)).newPage();
  const session = { page, log: "", errors: [] };
  page.on("console", (m) => { session.log += m.text() + "\n"; });
  page.on("pageerror", (e) => session.errors.push(String(e)));
  session.expect = async (text, timeout = 20000) => {
    const end = Date.now() + timeout;
    while (!session.log.includes(text)) {
      if (session.errors.length) throw new Error("page error: " + session.errors[0]);
      if (Date.now() > end) throw new Error(`did not see "${text}"; console:\n${session.log.slice(-800)}`);
      await page.waitForTimeout(50);
    }
    session.log = session.log.slice(session.log.indexOf(text) + text.length);
  };
  // the colour of the canvas at a point given in CSS pixels
  session.pixel = (x, y) => page.evaluate(([x, y]) => {
    const c = document.getElementById("screen"), box = c.getBoundingClientRect();
    const d = c.getContext("2d").getImageData(Math.floor(x * c.width / box.width), Math.floor(y * c.height / box.height), 1, 1).data;
    return (d[0] << 16) | (d[1] << 8) | d[2];
  }, [x, y]);
  session.until = async (what, check, timeout = 15000) => {
    const end = Date.now() + timeout;
    while (!(await check())) {
      if (Date.now() > end) throw new Error("timed out waiting for " + what);
      await page.waitForTimeout(100);
    }
  };
  await page.goto(url);
  return session;
}

(async () => {
  const server = await serve();
  const url = `http://127.0.0.1:${server.address().port}/`;
  const browser = await chromium.launch();
  const HOME = 0xF2F6F5, BALLS = 0x10202A, WHITE = 0xFFFFFF;

  const desk = await open(browser, url, { viewport: { width: 1024, height: 768 } });
  await step("the first visit shows only the choice of model; tom-os starts after it", async () => {
    await desk.page.waitForSelector("dialog[open]");
    await desk.page.waitForTimeout(500);
    if (desk.log.includes("tom-os")) throw new Error("tom-os started before the choice was made");
    if (await desk.page.isVisible("#gear")) throw new Error("the settings button is shown behind the dialog");
    await desk.page.click("#save");
    await desk.expect("tom-os 0.1");
    await desk.expect("through the browser");
    await desk.until("the home screen", async () => (await desk.pixel(20, 300)) === HOME);
  });
  await step("keys reach the shell; commands work", async () => {
    await desk.page.keyboard.type("/lua print(6*7, #'tom-os')", { delay: 10 });
    await desk.page.keyboard.press("Enter");
    await desk.expect("42\t6");
  });
  await step("the demo starts nothing that was not asked for", async () => {
    await desk.page.keyboard.type("hello", { delay: 10 });
    await desk.page.keyboard.press("Enter");
    await desk.expect("choose Claude or another server");
    await desk.page.waitForTimeout(800);
    if (desk.log.includes("launch_app")) throw new Error("an app was started");
    if ((await desk.pixel(20, 300)) !== HOME) throw new Error("the home screen is not shown");
  });
  await step("asked for it, the agent builds an app and runs it on its own thread", async () => {
    await desk.page.keyboard.type("balls", { delay: 10 });
    await desk.page.keyboard.press("Enter");
    await desk.expect("[write_file] apps/balls.lua");
    await desk.expect("[launch_app] apps/balls.lua");
    await desk.expect("It is running.");
    await desk.until("the app's picture", async () => (await desk.pixel(20, 300)) === BALLS);
  });
  await step("the shell stays usable while the app runs", async () => {
    await desk.page.evaluate(() => { for (const c of "/ps\n") window.tomos.serial.push(c.charCodeAt(0)); });
    await desk.expect("apps/balls.lua");
  });
  await step("keys reach the app: Esc ends it and the home screen is back", async () => {
    await desk.page.keyboard.down("Escape"); // held for a moment, as a finger does
    await desk.page.waitForTimeout(200);
    await desk.page.keyboard.up("Escape");
    await desk.until("the home screen", async () => (await desk.pixel(20, 300)) === HOME);
  });

  await step("can be installed and starts without a connection", async () => {
    const manifest = await desk.page.evaluate(async () => (await fetch(document.querySelector("link[rel=manifest]").href)).json());
    if (manifest.display !== "standalone" || manifest.icons.length < 2) throw new Error("manifest: " + JSON.stringify(manifest));
    await desk.page.evaluate(() => navigator.serviceWorker.ready);
    await desk.page.reload(); // now everything has passed through the service worker
    await desk.expect("tom-os 0.1");
    await desk.page.context().setOffline(true);
    await desk.page.reload();
    await desk.expect("tom-os 0.1");
    await desk.page.context().setOffline(false);
  });

  const phone = await open(browser, url, devices["Pixel 7"]);
  await step("starts at phone size, drawn at the display's density", async () => {
    await phone.page.click("#save");
    await phone.expect("tom-os 0.1");
    const size = await phone.page.evaluate(() => [document.getElementById("screen").width, window.innerWidth, window.devicePixelRatio]);
    if (size[0] !== size[1] * Math.round(size[2])) throw new Error("canvas is " + size[0] + " wide for " + size[1] + " x " + size[2]);
  });
  await step("text from an on-screen keyboard or input method is typed", async () => {
    await phone.page.locator("#typing").focus();
    await phone.page.keyboard.insertText("paint");
    await phone.page.keyboard.insertText("\n");
    await phone.expect("[launch_app] apps/paint.lua");
    await phone.until("the drawing pad", async () => (await phone.pixel(200, 400)) === WHITE);
  });
  await step("touch draws in the app", async () => {
    await phone.page.touchscreen.tap(200, 400);
    await phone.until("a dot where the finger was", async () => (await phone.pixel(200, 400)) !== WHITE);
  });

  await browser.close();
  server.close();
  console.log(failed ? `${failed} of ${count} steps failed` : `all ${count} steps passed`);
  process.exit(failed ? 1 : 0);
})();
