// The machine tom-os runs on in a browser: this page. It loads the kernel
// (tomos.wasm), switches its threads, and provides the screen (a canvas),
// keyboard and pointer events, clocks, and the way to the LLM.
//
// The other side of this interface is rust/src/arch/wasm32.

import { chat } from "./providers.js";

const canvas = document.getElementById("screen");
const typing = document.getElementById("typing");
const context = canvas.getContext("2d");

let wasm; // the module's exports
const u8 = () => new Uint8Array(wasm.memory.buffer);
const u32 = () => new Uint32Array(wasm.memory.buffer);
const i32 = () => new Int32Array(wasm.memory.buffer);
const word = (addr) => u32()[addr >> 2];
const setWord = (addr, v) => { u32()[addr >> 2] = v; };

// ------------------------------------------------------------- threads
// A thread's control block (see arch/wasm32/thread.rs): word 0 started,
// 1 stack pointer, 2 boot thread, 3 and 4 asyncify's record, then the
// buffer for the saved call stack.
const RECORD = 12, BUFFER = 20;
let pending = null; // why the kernel returned to us: {to} or {wait}

function leave(block, why) {
  // called from inside the kernel: unwind its call stack into the block
  setWord(block, 1);
  setWord(block + 4, wasm.__stack_pointer.value);
  setWord(block + RECORD, block + BUFFER);
  pending = why;
  wasm.asyncify_start_unwind(block + RECORD);
}

function arriving(block) {
  // true when a call is being replayed to get back to where `leave` was
  if (wasm.asyncify_get_state() !== 2) return false;
  wasm.asyncify_stop_rewind();
  wasm.__stack_pointer.value = word(block + 4);
  return true;
}

function enter(block) {
  // runs a thread until it leaves again
  if (block === 0) {
    wasm.kmain(); // the very first start
  } else if (!word(block)) {
    setWord(block, 1);
    wasm.__stack_pointer.value = word(block + 4);
    wasm.fiber_start();
  } else {
    wasm.asyncify_start_rewind(block + RECORD);
    if (word(block + 8)) wasm.kmain(); else wasm.fiber_start();
  }
  if (wasm.asyncify_get_state() !== 1) throw new Error("the kernel returned");
  wasm.asyncify_stop_unwind();
}

let wake = null; // ends the current wait early
function sleep(ms) {
  return new Promise((resolve) => {
    const timer = ms > 0 ? setTimeout(done, ms) : null;
    const channel = ms > 0 ? null : new MessageChannel();
    function done() { clearTimeout(timer); wake = null; resolve(); }
    wake = done;
    if (channel) { channel.port1.onmessage = done; channel.port2.postMessage(0); }
  });
}

async function run() {
  let current = 0;
  for (;;) {
    enter(current);
    const why = pending;
    pending = null;
    if (why.to !== undefined) {
      current = why.to;
    } else {
      current = why.block;
      await sleep(events.length ? 0 : why.wait);
    }
  }
}

// -------------------------------------------------------------- screen
let picture = null; // {pixels, w, h}: where the kernel keeps the picture

function present(x0, y0, x1, y1) {
  if (!picture) return;
  x1 = Math.min(x1, picture.w); y1 = Math.min(y1, picture.h);
  const w = x1 - x0, h = y1 - y0;
  if (w <= 0 || h <= 0) return;
  const source = u32(), image = context.createImageData(w, h), out = new Uint32Array(image.data.buffer);
  for (let y = 0; y < h; y++) {
    let from = (picture.pixels >> 2) + (y0 + y) * picture.w + x0;
    for (let x = 0, to = y * w; x < w; x++, to++, from++) {
      const p = source[from]; // 0x00RRGGBB -> the canvas's R, G, B, A bytes
      out[to] = 0xFF000000 | (p & 0xFF) << 16 | (p & 0xFF00) | (p >> 16 & 0xFF);
    }
  }
  context.putImageData(image, x0, y0);
}

// --------------------------------------------------------------- input
const events = [];
function post(event) {
  events.push(event);
  if (wake) wake();
}

// KeyboardEvent.code -> PC scancode (set 1); the second list is the keys
// with an E0 prefix
const KEYS = {
  Escape: 1, Digit1: 2, Digit2: 3, Digit3: 4, Digit4: 5, Digit5: 6, Digit6: 7, Digit7: 8, Digit8: 9, Digit9: 10, Digit0: 11,
  Minus: 12, Equal: 13, Backspace: 14, Tab: 15, KeyQ: 16, KeyW: 17, KeyE: 18, KeyR: 19, KeyT: 20, KeyY: 21, KeyU: 22,
  KeyI: 23, KeyO: 24, KeyP: 25, BracketLeft: 26, BracketRight: 27, Enter: 28, ControlLeft: 29, KeyA: 30, KeyS: 31,
  KeyD: 32, KeyF: 33, KeyG: 34, KeyH: 35, KeyJ: 36, KeyK: 37, KeyL: 38, Semicolon: 39, Quote: 40, Backquote: 41,
  ShiftLeft: 42, Backslash: 43, KeyZ: 44, KeyX: 45, KeyC: 46, KeyV: 47, KeyB: 48, KeyN: 49, KeyM: 50, Comma: 51,
  Period: 52, Slash: 53, ShiftRight: 54, AltLeft: 56, Space: 57,
};
const EXTENDED_KEYS = {
  NumpadEnter: 0x1C, ControlRight: 0x1D, AltRight: 0x38, Home: 0x47, ArrowUp: 0x48, PageUp: 0x49, ArrowLeft: 0x4B,
  ArrowRight: 0x4D, End: 0x4F, ArrowDown: 0x50, PageDown: 0x51, Delete: 0x53,
};

function onKey(e, down) {
  if (document.querySelector("dialog[open]")) return;
  // text being composed by the host's input method arrives as text instead
  if (e.isComposing || e.key === "Process" || e.key === "Unidentified") return;
  const extended = e.code in EXTENDED_KEYS;
  // (on-screen keyboards often give no code: go by the key's name then)
  const code = extended ? EXTENDED_KEYS[e.code] : KEYS[e.code] ?? { Enter: 28, Backspace: 14 }[e.key];
  if (code === undefined) return;
  if (e.metaKey || (e.ctrlKey && "rRtTwWnN".includes(e.key))) return; // leave browser shortcuts alone
  e.preventDefault();
  if (e.key.length === 1 && !e.ctrlKey && !e.altKey) {
    // A key that types a character: the browser knows the keyboard layout,
    // so take the character from it, and report the key itself only as
    // held or not (for games).
    post({ kind: 4, a: code, b: 0, c: down ? 1 : 0, d: 0 });
    if (down) post({ kind: 3, text: e.key });
  } else {
    post({ kind: 1, a: code, b: extended ? 1 : 0, c: down ? 1 : 0, d: 0 });
  }
}
window.addEventListener("keydown", (e) => onKey(e, true));
window.addEventListener("keyup", (e) => onKey(e, false));

// on-screen keyboards and input methods type into the hidden field
// (Enter and Backspace change nothing in an empty one-line field, so they
// are taken before the field handles them)
const tap = (code) => { post({ kind: 1, a: code, b: 0, c: 1, d: 0 }); post({ kind: 1, a: code, b: 0, c: 0, d: 0 }); };
typing.addEventListener("beforeinput", (e) => {
  if (e.isComposing) return;
  if (e.inputType === "insertLineBreak" || e.data === "\n") { e.preventDefault(); tap(28); }
  else if (e.inputType === "deleteContentBackward") { e.preventDefault(); tap(14); }
});
typing.addEventListener("input", (e) => {
  if (e.isComposing) return;
  if (typing.value) post({ kind: 3, text: typing.value });
  typing.value = "";
});
typing.addEventListener("compositionend", (e) => {
  if (e.data) post({ kind: 3, text: e.data });
  typing.value = "";
});

let buttons = 0, wheel = 0, lastX = 0, lastY = 0;
function onPointer(e) {
  const box = canvas.getBoundingClientRect();
  lastX = Math.max(0, Math.min(65535, Math.round((e.clientX - box.left) * 65536 / box.width)));
  lastY = Math.max(0, Math.min(65535, Math.round((e.clientY - box.top) * 65536 / box.height)));
  if (e.type === "pointerdown") buttons |= e.button === 2 ? 2 : 1;
  if (e.type === "pointerup" || e.type === "pointercancel") buttons &= ~(e.button === 2 ? 2 : 1);
  post({ kind: 2, a: lastX, b: lastY, c: buttons, d: 0 });
}
for (const type of ["pointerdown", "pointermove", "pointerup", "pointercancel"]) {
  canvas.addEventListener(type, (e) => {
    e.preventDefault();
    if (type === "pointerdown") { canvas.setPointerCapture(e.pointerId); typing.focus({ preventScroll: true }); }
    onPointer(e);
  });
}
canvas.addEventListener("contextmenu", (e) => e.preventDefault());
canvas.addEventListener("wheel", (e) => {
  e.preventDefault();
  wheel += e.deltaY;
  const steps = Math.trunc(wheel / 40);
  if (steps) { wheel -= steps * 40; post({ kind: 2, a: lastX, b: lastY, c: buttons, d: steps }); }
}, { passive: false });

// ------------------------------------------------------------- the LLM
let request = null; // {done, status, body, abort}
function startChat(body) {
  const abort = new AbortController();
  const r = { done: false, status: 0, body: new Uint8Array(0), abort };
  request = r;
  chat(JSON.parse(body), abort.signal)
    .then((answer) => { r.status = answer.status; r.body = new TextEncoder().encode(JSON.stringify(answer.body)); })
    .catch((error) => { r.status = 0; r.body = new TextEncoder().encode(String(error.message || error)); })
    .finally(() => { r.done = true; if (wake) wake(); });
}

// ------------------------------------------------------------ the rest
let line = "";
const serialInput = [];
const copyOut = (text, out, max) => {
  const bytes = new TextEncoder().encode(text).subarray(0, max);
  u8().set(bytes, out);
  return bytes.length;
};

const env = {
  js_now: () => performance.now(),
  js_unix_time: () => Date.now() / 1000 - new Date().getTimezoneOffset() * 60, // local time
  js_random: () => Math.random(),
  js_switch(from, to) { if (!arriving(from)) leave(from, { to }); },
  js_wait(block, ms) { if (!arriving(block)) leave(block, { block, wait: ms }); },
  js_power(kind) {
    if (kind === 1) location.reload();
    else document.getElementById("stopped").style.display = "flex";
  },
  js_serial_put(c) {
    // the serial console goes to the browser's console, a line at a time
    if (c === 10) { console.log(line.replace(/\x1b\[[?0-9;]*[a-zA-Z]/g, "")); line = ""; } else line += String.fromCharCode(c);
  },
  js_serial_get: () => (serialInput.length ? serialInput.shift() : -1),
  js_cmdline: (out, max) => copyOut(commandLine(), out, max),
  js_event(out) {
    const e = events.shift();
    if (!e) return 0;
    if (e.kind === 3) { env.text = e.text; return 3; }
    i32().set([e.a, e.b, e.c, e.d], out >> 2);
    return e.kind;
  },
  js_event_text: (out, max) => copyOut(env.text || "", out, max),
  js_mode(pixels, w, h) {
    picture = { pixels, w, h };
    canvas.width = w;
    canvas.height = h;
  },
  js_present: present,
  js_llm_start(body, len) { startChat(new TextDecoder().decode(u8().slice(body, body + len))); },
  js_llm_poll: () => (request && request.done ? request.body.length : -1),
  js_llm_status: () => (request ? request.status : 0),
  js_llm_read(out) { u8().set(request.body, out); },
  js_llm_abort() { if (request) request.abort.abort(); },
};

// The screen is as large as the window; on dense displays everything is
// drawn at twice (or more) the size, as on the other machines.
function commandLine() {
  const scale = Math.max(1, Math.min(4, Math.round(window.devicePixelRatio || 1)));
  const w = Math.max(360, Math.floor(window.innerWidth)) * scale;
  const h = Math.max(360, Math.floor(window.innerHeight)) * scale;
  return `res=${w}x${h} scale=${scale}`;
}

// ------------------------------------------------------------ settings
const dialog = document.getElementById("settings");
const field = (id) => document.getElementById(id);
function showFields() {
  field("for-claude").style.display = field("provider").value === "claude" ? "" : "none";
  field("for-openai").style.display = field("provider").value === "openai" ? "" : "none";
}
const SAVED = ["provider", "claude-key", "claude-model", "openai-url", "openai-key", "openai-model"];
for (const id of SAVED) {
  const v = localStorage.getItem("tomos." + id);
  if (v !== null) field(id).value = v;
}
field("provider").addEventListener("change", showFields);
field("gear").addEventListener("click", () => { showFields(); dialog.showModal(); });
function saveSettings() {
  for (const id of SAVED) localStorage.setItem("tomos." + id, field(id).value);
  typing.focus({ preventScroll: true });
}
field("save").addEventListener("click", () => dialog.close());
dialog.addEventListener("close", saveSettings); // also when closed with Esc

// On the first visit the choice of model comes first, and nothing else is
// shown until it has been made.
if (localStorage.getItem("tomos.provider") === null) {
  field("gear").style.display = "none";
  showFields();
  dialog.showModal();
  await new Promise((resolve) => dialog.addEventListener("close", resolve, { once: true }));
  field("gear").style.display = "";
}

// ---------------------------------------------------------------- start
const { instance } = await WebAssembly.instantiateStreaming(fetch("tomos.wasm"), { env });
wasm = instance.exports;
window.tomos = { post, serial: serialInput }; // for tests
typing.focus({ preventScroll: true });
run().catch((error) => {
  console.error(error);
  document.getElementById("stopped").style.display = "flex";
});
