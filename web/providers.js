// The way to the LLM. The kernel sends the same request to every backend
// (the "chat completions" wire format: messages, tools) and expects that
// format back; this file passes it to the service chosen in the settings.

const setting = (id) => (localStorage.getItem("tomos." + id) ?? document.getElementById(id).value).trim();

/// Sends one chat request; resolves to {status, body}.
export async function chat(request, signal) {
  switch (setting("provider")) {
    case "claude": return claude(request, signal);
    case "openai": return openai(request, signal);
    default: return demo(request);
  }
}

const reply = (message) => ({ status: 200, body: { choices: [{ index: 0, message, finish_reason: "stop" }] } });
const failure = (status, message) => ({ status, body: { error: { message } } });

// ----------------------------------------------------- another server
async function openai(request, signal) {
  const url = setting("openai-url");
  if (!url) return failure(400, "No server is set. Open the settings (the gear at the top right).");
  const headers = { "Content-Type": "application/json" };
  if (setting("openai-key")) headers.Authorization = "Bearer " + setting("openai-key");
  const response = await fetch(url, {
    method: "POST", headers, signal,
    body: JSON.stringify({ ...request, model: setting("openai-model") || request.model }),
  });
  const text = await response.text();
  try {
    return { status: response.status, body: JSON.parse(text) };
  } catch {
    return failure(response.status, text.slice(0, 300));
  }
}

// --------------------------------------------------------------- Claude
// Through the official SDK, loaded when first needed. The key the user
// entered stays in this browser and goes only to the Anthropic API.
let sdk = null;
// Claude's replies carry thinking blocks that must be sent back unchanged
// on later turns; the kernel keeps only text and tool calls, so the full
// content of each reply is remembered here.
const replies = new Map();
const replyKey = (message) => (message.tool_calls?.length ? "call:" + message.tool_calls[0].id : "text:" + (message.content || ""));

function toClaude(messages) {
  const system = [], out = [];
  for (const m of messages) {
    if (m.role === "system") {
      system.push(m.content);
    } else if (m.role === "tool") {
      // all results of one turn go into a single user message
      const result = { type: "tool_result", tool_use_id: m.tool_call_id, content: m.content || "" };
      const last = out[out.length - 1];
      if (last && last.role === "user" && Array.isArray(last.content) && last.content[0].type === "tool_result") last.content.push(result);
      else out.push({ role: "user", content: [result] });
    } else if (m.role === "assistant") {
      let content = replies.get(replyKey(m));
      if (!content) {
        content = [];
        if (m.content) content.push({ type: "text", text: m.content });
        for (const call of m.tool_calls || []) {
          let input = {};
          try { input = JSON.parse(call.function.arguments || "{}"); } catch { /* keep {} */ }
          content.push({ type: "tool_use", id: call.id, name: call.function.name, input });
        }
      }
      out.push({ role: "assistant", content });
    } else {
      out.push({ role: "user", content: m.content || "" });
    }
  }
  return { system: system.join("\n\n"), messages: out };
}

async function claude(request, signal) {
  const key = setting("claude-key");
  if (!key) return failure(401, "No API key is set. Open the settings (the gear at the top right).");
  sdk ??= await import("https://esm.sh/@anthropic-ai/sdk");
  const Anthropic = sdk.default;
  // this page is the user's own client: the key is theirs and is sent
  // nowhere but to the API
  const client = new Anthropic({ apiKey: key, dangerouslyAllowBrowser: true });
  const { system, messages } = toClaude(request.messages);
  const tools = (request.tools || []).map((t) => ({
    name: t.function.name, description: t.function.description, input_schema: t.function.parameters,
  }));
  try {
    const stream = client.beta.messages.stream({
      model: setting("claude-model") || "claude-opus-5-5",
      max_tokens: 64000,
      system,
      messages,
      ...(tools.length ? { tools } : {}),
      thinking: { type: "adaptive" },
      // requests without tools are the input method's kana-to-kanji
      // conversions, where a quick answer matters more
      output_config: { effort: tools.length ? "medium" : "low" },
      // if a safety classifier declines, the API retries on another model
      betas: ["server-side-fallback-2026-07-01"],
      fallbacks: "default",
    }, { signal });
    const message = await stream.finalMessage();
    let text = message.content.filter((b) => b.type === "text").map((b) => b.text).join("");
    let calls = message.content.filter((b) => b.type === "tool_use").map((b) => ({
      id: b.id, type: "function", function: { name: b.name, arguments: JSON.stringify(b.input) },
    }));
    if (message.stop_reason === "refusal") {
      [text, calls] = ["(The model declined this request.)", []];
    } else if (message.stop_reason === "max_tokens") {
      // a tool call cut off in the middle cannot be run
      [text, calls] = [text + "\n(The answer was cut off because it got too long.)", []];
    }
    const result = { role: "assistant", content: text || null, ...(calls.length ? { tool_calls: calls } : {}) };
    if (message.stop_reason !== "refusal" && message.stop_reason !== "max_tokens") replies.set(replyKey(result), message.content);
    return reply(result);
  } catch (error) {
    if (signal.aborted) throw error;
    if (error instanceof Anthropic.AuthenticationError) return failure(401, "The API key was not accepted. Check it in the settings.");
    if (error instanceof Anthropic.RateLimitError) return failure(429, "The API's rate limit was reached. Try again in a moment.");
    if (error instanceof Anthropic.APIError) return failure(error.status || 500, error.message);
    throw error;
  }
}

// ----------------------------------------------------------------- demo
// No model at all: a few scripted answers, so that the page shows what
// tom-os does before any key is entered.
const BALLS = `-- bouncing balls: click or touch to add one
local w, h = gfx.mode("Balls")
local balls = {}
local colors = {0xE4572E, 0x17BEBB, 0xFFC914, 0x76B041, 0x9B5DE5}
local function add(x, y)
  balls[#balls + 1] = {x = x, y = y, vx = math.random(-4, 4), vy = math.random(-6, -2),
                       r = math.random(10, 26), c = colors[math.random(#colors)]}
end
for i = 1, 6 do add(math.random(40, w - 40), math.random(40, h // 2)) end
local was_down = false
while not gfx.key("escape") do
  local mx, my, down = gfx.mouse()
  if down and not was_down then add(mx, my) end
  was_down = down
  gfx.clear(0x10202A)
  for _, b in ipairs(balls) do
    b.vy = b.vy + 0.35
    b.x, b.y = b.x + b.vx, b.y + b.vy
    if b.x < b.r then b.x, b.vx = b.r, -b.vx end
    if b.x > w - b.r then b.x, b.vx = w - b.r, -b.vx end
    if b.y > h - b.r then b.y, b.vy = h - b.r, -b.vy * 0.9 end
    gfx.circle(math.floor(b.x), math.floor(b.y), b.r, b.c, true)
  end
  gfx.text(12, 10, "click to add a ball   Esc: quit", 0xFFFFFF)
  gfx.flip(60)
end
`;

const PAINT = `-- drawing pad: drag to draw, pick a colour at the top
local w, h = gfx.mode("Paint")
local colors = {0x1E2B2E, 0xE4572E, 0x17BEBB, 0xFFC914, 0x76B041, 0x9B5DE5, 0xFFFFFF}
local color, px, py = colors[1], nil, nil
gfx.clear(0xFFFFFF)
while not gfx.key("escape") do
  for i, c in ipairs(colors) do
    gfx.rect((i - 1) * 44 + 8, 8, 36, 36, c)
    gfx.box((i - 1) * 44 + 8, 8, 36, 36, c == color and 0x0E9F8E or 0xCFDAD8)
  end
  gfx.box(#colors * 44 + 8, 8, 70, 36, 0xCFDAD8)
  gfx.text(#colors * 44 + 20, 18, "clear", 0x1E2B2E)
  local x, y, down = gfx.mouse()
  if down and y < 52 then
    local i = (x - 8) // 44 + 1
    if colors[i] then color = colors[i] elseif i == #colors + 1 or i == #colors + 2 then gfx.rect(0, 52, w, h, 0xFFFFFF) end
    px = nil
  elseif down then
    if px then
      for d = -2, 2 do gfx.line(px + d, py, x + d, y, color) gfx.line(px, py + d, x, y + d, color) end
    end
    gfx.circle(x, y, 2, color, true)
    px, py = x, y
  else
    px = nil
  end
  gfx.flip(60)
end
`;

const DEMOS = [
  { name: "paint", words: /paint|draw|sketch/i, code: PAINT, about: "a drawing pad" },
  { name: "balls", words: /./, code: BALLS, about: "bouncing balls" },
];

function demo(request) {
  const messages = request.messages;
  if (!request.tools) return reply({ role: "assistant", content: messages[messages.length - 1].content }); // kana stays kana
  const lastUser = messages.map((m) => m.role).lastIndexOf("user");
  const asked = messages[lastUser].content || "";
  const steps = messages.slice(lastUser + 1).filter((m) => m.role === "tool").length;
  const pick = DEMOS.find((d) => d.words.test(asked));
  const call = (name, args, content) => reply({
    role: "assistant", content,
    tool_calls: [{ id: "demo_" + Date.now(), type: "function", function: { name, arguments: JSON.stringify(args) } }],
  });
  if (steps === 0) {
    return call("write_file", { path: `apps/${pick.name}.lua`, content: pick.code },
      `This is the demo: no model is connected, so I can only make ${pick.about} (or a drawing pad if you ask to draw).`);
  }
  if (steps === 1) return call("launch_app", { path: `apps/${pick.name}.lua` }, null);
  return reply({
    role: "assistant",
    content: "It is running. Drag the bar at the bottom up to come back here.\n" +
      "To have a real model write any app you ask for, open the settings (the gear at the top right) and choose Claude or another server.",
  });
}
