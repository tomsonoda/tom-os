//! The agent: conversation state, LLM calls (chat completions over plain
//! HTTP) and the tools the model can use.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::json::{self, Json};
use crate::rt::{self, KCell, StrBuf, console, cstr, lossy, ui};
use crate::apps::{self, Started};
use crate::{fs, http, kprint, net, sys, tcp, wm};

const MAX_STEPS: usize = 24;
const TOOL_RESULT_MAX: usize = 12000;

const SYSTEM_PROMPT: &str = include_str!("prompts/system.txt");
const TOOLS_JSON: &str = include_str!("prompts/tools.json");
const CONVERT_PROMPT: &str = include_str!("prompts/convert.txt");

// ----------------------------------------------------------- conversation

struct ToolCall {
    id: Vec<u8>,
    name: Vec<u8>,
    args: Vec<u8>,
}

struct Message {
    role: &'static str,
    /// `None` for assistant messages that only call tools.
    content: Option<Vec<u8>>,
    calls: Vec<ToolCall>,
    tool_call_id: Option<Vec<u8>>,
}

/// Only the shell's thread uses the conversation; a turn takes it out of
/// the cell while it runs.
static CONVERSATION: KCell<Vec<Message>> = KCell::new(Vec::new());

/// Forgets the conversation.
pub fn reset() {
    CONVERSATION.with(|c| c.clear());
}

// -------------------------------------------------------------- settings

/// LLM connection settings: defaults, overridden by kernel arguments at
/// boot and by the /set command at run time.
const DEFAULTS: [(&str, &str); 3] = [("llm", "10.0.2.2:8080"), ("model", "default"), ("path", "/v1/chat/completions")];

static SETTINGS: KCell<Option<[Vec<u8>; 3]>> = KCell::new(None);

fn with_settings<R>(f: impl FnOnce(&mut [Vec<u8>; 3]) -> R) -> R {
    SETTINGS.with(|s| {
        f(s.get_or_insert_with(|| {
            DEFAULTS.map(|(key, default)| {
                let mut v = [0u8; 128];
                let key = cstr(key.as_bytes());
                unsafe { sys::cmdline_get(key.as_ptr() as *const c_char, v.as_mut_ptr(), v.len()) };
                let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
                if end > 0 { v[..end].to_vec() } else { default.as_bytes().to_vec() }
            })
        }))
    })
}

/// The value of "llm", "model" or "path".
pub fn setting(key: &str) -> Vec<u8> {
    let Some(i) = DEFAULTS.iter().position(|d| d.0 == key) else { return Vec::new() };
    with_settings(|values| values[i].clone())
}

/// Changes a setting; false if the key or the value is not acceptable.
pub fn set(key: &str, value: &[u8]) -> bool {
    let Some(i) = DEFAULTS.iter().position(|d| d.0 == key) else { return false };
    if value.is_empty() || value.len() >= 128 {
        return false;
    }
    with_settings(|values| values[i] = value.to_vec());
    true
}

/// The LLM server as (address, port, "ip:port" text), or an error message.
fn endpoint() -> Result<(u32, u16, String), String> {
    let text = lossy(&setting("llm"));
    let (host, port) = match text.split_once(':') {
        Some((h, p)) => (h, p.parse().unwrap_or(0)),
        None => (text.as_str(), 80),
    };
    let Some(ip) = net::parse_ip(host) else {
        return Err(format!("bad llm= address '{host}' (expected IP:port)"));
    };
    Ok((ip, port, format!("{host}:{port}")))
}

// ------------------------------------------------------------------- LLM

fn build_request(conv: &[Message], model: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 * 1024);
    b.push(b'{');
    json::field(&mut b, "model", model);
    b.extend_from_slice(b",\"stream\":false,\"messages\":[{");
    json::field(&mut b, "role", b"system");
    b.push(b',');
    let mode = if wm::active() {
        "\n\nGraphical mode is ON: gfx is available and apps run in windows."
    } else {
        "\n\nGraphical mode is OFF (text mode): gfx is not available."
    };
    json::field(&mut b, "content", format!("{SYSTEM_PROMPT}{mode}").as_bytes());
    b.push(b'}');
    for m in conv {
        b.extend_from_slice(b",{");
        json::field(&mut b, "role", m.role.as_bytes());
        b.extend_from_slice(b",\"content\":");
        match &m.content {
            Some(c) if !c.is_empty() || m.calls.is_empty() => json::quote(&mut b, c),
            _ => b.extend_from_slice(b"null"),
        }
        if let Some(id) = &m.tool_call_id {
            b.push(b',');
            json::field(&mut b, "tool_call_id", id);
        }
        if !m.calls.is_empty() {
            b.extend_from_slice(b",\"tool_calls\":[");
            for (k, call) in m.calls.iter().enumerate() {
                if k > 0 {
                    b.push(b',');
                }
                b.push(b'{');
                json::field(&mut b, "id", &call.id);
                b.extend_from_slice(b",\"type\":\"function\",\"function\":{");
                json::field(&mut b, "name", &call.name);
                b.push(b',');
                json::field(&mut b, "arguments", &call.args);
                b.extend_from_slice(b"}}");
            }
            b.push(b']');
        }
        b.push(b'}');
    }
    b.extend_from_slice(b"],\"tools\":");
    b.extend_from_slice(TOOLS_JSON.as_bytes());
    b.push(b'}');
    b
}

fn clip(s: &[u8], max: usize) -> String {
    lossy(&s[..s.len().min(max)])
}

/// Calls the model with the conversation and appends its reply.
fn llm_call(conv: &mut Vec<Message>) -> Result<(), String> {
    let (ip, port, shown) = endpoint()?;
    let req = build_request(conv, &setting("model"));
    let path = lossy(&setting("path"));
    let (status, body) = http::post_json(ip, port, &path, &req, 600_000).map_err(|e| {
        let mut msg = format!("LLM request to {shown} failed while {}: {}", http::phase().text(), e.text());
        if e == net::NetError::Timeout && http::phase() == http::Phase::Connecting {
            msg.push_str(" (is the LLM server running? check /info and /set llm)");
        }
        msg
    })?;
    drop(req);

    let root = json::parse(&body);
    let message = root.as_ref().and_then(|r| r.get("choices")).and_then(|c| c.at(0)).and_then(|c| c.get("message"));
    let Some(m) = message.filter(|_| status == 200) else {
        let err = root.as_ref().and_then(|r| r.get("error"));
        let detail = err.and_then(|e| e.get("message")).and_then(Json::as_str).or(err.and_then(Json::as_str));
        return Err(match detail {
            Some(d) if status == 200 => format!("LLM error: {}", clip(d, 300)),
            Some(d) => format!("LLM returned HTTP {status}: {}", clip(d, 300)),
            None if body.is_empty() => format!("LLM returned HTTP {status}: (empty response)"),
            None => format!("LLM returned HTTP {status}: {}", clip(&body, 300)),
        });
    };

    let calls = m.get("tool_calls").map_or(&[][..], Json::items);
    conv.push(Message {
        role: "assistant",
        content: m.get("content").and_then(Json::as_str).map(<[u8]>::to_vec),
        calls: calls
            .iter()
            .map(|c| {
                let f = c.get("function");
                let text = |v: Option<&Json>, default: &[u8]| v.and_then(Json::as_str).unwrap_or(default).to_vec();
                ToolCall {
                    id: text(c.get("id"), b"call"),
                    name: text(f.and_then(|f| f.get("name")), b""),
                    args: text(f.and_then(|f| f.get("arguments")), b"{}"),
                }
            })
            .collect(),
        tool_call_id: None,
    });
    Ok(())
}

// ----------------------------------------------------------------- tools

fn note(text: &str) {
    kprint!("{}{text}\n{}", ui::NOTE, ui::END);
}

fn exec_tool(call: &ToolCall) -> Vec<u8> {
    let Some(args) = json::parse(&call.args).filter(Json::is_object) else {
        return b"error: arguments are not a valid JSON object".to_vec();
    };
    let path = args.get("path").and_then(Json::as_str);
    let shown = path.map_or(String::from("?"), lossy);
    let missing = || format!("error: no such file: {}", path.map_or(String::from("(missing path)"), lossy)).into_bytes();

    let mut out = match call.name.as_slice() {
        b"write_file" => match (path, args.get("content").and_then(Json::as_str)) {
            (Some(p), Some(content)) => {
                if fs::write(p, content) {
                    note(&format!("  [write_file] {shown} ({} bytes)", content.len()));
                    format!("wrote {} bytes to {shown}", content.len()).into_bytes()
                } else {
                    b"error: could not write file (bad path or out of memory)".to_vec()
                }
            }
            _ => b"error: 'path' and 'content' are required".to_vec(),
        },
        b"read_file" => {
            note(&format!("  [read_file] {shown}"));
            path.and_then(fs::read).unwrap_or_else(missing)
        }
        b"list_files" => {
            note("  [list_files]");
            let mut list = Vec::new();
            for (name, len) in fs::list() {
                list.extend_from_slice(&name);
                list.extend_from_slice(format!(" ({len} bytes)\n").as_bytes());
            }
            if list.is_empty() { b"(no files)".to_vec() } else { list }
        }
        b"delete_file" => {
            note(&format!("  [delete_file] {shown}"));
            match path {
                Some(p) if fs::delete(p) => {
                    format!("deleted {shown}").into_bytes()
                }
                _ => missing(),
            }
        }
        b"run_lua" => match args.get("code").and_then(Json::as_str) {
            None => b"error: 'code' is required".to_vec(),
            Some(code) => {
                kprint!("{}  [run_lua] {} bytes ... ", ui::NOTE, code.len());
                let (mut out, mut err) = (StrBuf::new(), StrBuf::new());
                let r = unsafe { sys::lua_run(c"run_lua".as_ptr(), code.as_ptr(), code.len(), 0, 10_000, out.as_ptr(), err.as_ptr()) };
                kprint!("{}\n{}", if r == 0 { "ok" } else { "error" }, ui::END);
                let mut text = out.as_bytes().to_vec();
                if r != 0 {
                    if !text.is_empty() {
                        text.push(b'\n');
                    }
                    text.extend_from_slice(b"error: ");
                    text.extend_from_slice(err.as_bytes());
                } else if text.is_empty() {
                    text.extend_from_slice(b"(finished with no output)");
                }
                text
            }
        },
        b"launch_app" => match path {
            None => b"error: 'path' is required".to_vec(),
            Some(p) => {
                note(&format!("  [launch_app] {shown}"));
                match apps::launch(p) {
                    Ok(Started::Finished) => b"the app ran and exited normally".to_vec(),
                    Ok(Started::Running) => b"the app started without errors and is running in its own window".to_vec(),
                    Err(e) => {
                        kprint!("{}  [app error] {e}\n{}", ui::ERROR, ui::END);
                        format!("the app stopped with an error: {e}").into_bytes()
                    }
                }
            }
        },
        other => format!("error: unknown tool '{}'", lossy(other)).into_bytes(),
    };
    if out.len() > TOOL_RESULT_MAX {
        let mut cut = TOOL_RESULT_MAX;
        while cut > 0 && (out[cut] & 0xC0) == 0x80 {
            cut -= 1; // keep whole UTF-8 characters
        }
        out.truncate(cut);
        out.extend_from_slice(b"\n[output truncated]");
    }
    out
}

// ---------------------------------------------------------- kana to kanji

fn convert_kana(kana: &[u8], avoid: Option<&[u8]>) -> Result<Vec<u8>, String> {
    if !net::up() {
        return Err(String::from("no LLM for conversion"));
    }
    let (ip, port, _) = endpoint().map_err(|_| String::from("no LLM for conversion"))?;
    let mut system = CONVERT_PROMPT.as_bytes().to_vec();
    if let Some(a) = avoid {
        system.extend_from_slice(b" The user did not want this conversion: ");
        system.extend_from_slice(a);
        system.extend_from_slice(b" - reply with the next most likely different one.");
    }
    let mut req = Vec::new();
    req.push(b'{');
    json::field(&mut req, "model", &setting("model"));
    req.extend_from_slice(b",\"stream\":false,\"messages\":[{");
    json::field(&mut req, "role", b"system");
    req.push(b',');
    json::field(&mut req, "content", &system);
    req.extend_from_slice(b"},{");
    json::field(&mut req, "role", b"user");
    req.push(b',');
    json::field(&mut req, "content", kana);
    req.extend_from_slice(b"}]}");

    let (status, body) = http::post_json(ip, port, &lossy(&setting("path")), &req, 60_000).map_err(|e| String::from(e.text()))?;
    let root = json::parse(&body);
    let text = root
        .as_ref()
        .and_then(|r| r.get("choices"))
        .and_then(|c| c.at(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Json::as_str)
        .filter(|_| status == 200)
        .ok_or_else(|| String::from("conversion failed"))?;

    // one line, without surrounding blanks or quotes
    let open = "「".as_bytes();
    let close = "」".as_bytes();
    let mut t = text;
    loop {
        if let [b' ' | b'\n' | b'"', rest @ ..] = t {
            t = rest;
        } else if t.starts_with(open) {
            t = &t[open.len()..];
        } else {
            break;
        }
    }
    let mut t = &t[..t.iter().position(|&b| b == b'\n').unwrap_or(t.len())];
    while let [rest @ .., b' ' | b'"'] = t {
        t = rest;
    }
    if t.ends_with(close) && !kana.windows(open.len()).any(|w| w == open) {
        t = &t[..t.len() - close.len()];
    }
    Ok(t.to_vec())
}

/// Asks the LLM to turn kana into kanji; `avoid` is a previous answer to
/// replace. Appends the result to `out`, or the reason to `err`.
///
/// # Safety
/// `kana` must be a NUL-terminated string, `avoid` one or NULL, and `out`
/// and `err` valid strbufs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn agent_convert_kana(kana: *const c_char, avoid: *const c_char, out: *mut c_void, err: *mut c_void) -> i32 {
    let kana = unsafe { rt::from_c(kana) };
    let avoid = if avoid.is_null() { None } else { Some(unsafe { rt::from_c(avoid) }) };
    match convert_kana(kana, avoid) {
        Ok(text) => {
            unsafe { rt::sb_append(out, &text) };
            0
        }
        Err(e) => {
            unsafe { rt::sb_append(err, e.as_bytes()) };
            -1
        }
    }
}

// ------------------------------------------------------------------ turn

static WAIT_START: AtomicU64 = AtomicU64::new(0);

/// Status line while waiting for the model; writing a whole app can take a
/// minute or two, so show that time is passing.
fn show_waiting() {
    let what = match http::phase() {
        http::Phase::Waiting => "thinking",
        http::Phase::Receiving => "thinking, reply started",
        other => other.text(),
    };
    let secs = (unsafe { sys::time_ms() } - WAIT_START.load(Ordering::Relaxed)) / 1000;
    kprint!("{}\r\x1b[2m({what}... {secs} s, Ctrl-C to cancel)\x1b[0m\x1b[K{}", ui::STATUS, ui::END);
}

fn run_turn(conv: &mut Vec<Message>, user_text: &[u8]) {
    let mark = conv.len();
    conv.push(Message { role: "user", content: Some(user_text.to_vec()), calls: Vec::new(), tool_call_id: None });
    unsafe { sys::console_clear_break() };

    for _ in 0..MAX_STEPS {
        WAIT_START.store(unsafe { sys::time_ms() }, Ordering::Relaxed);
        http::reset_phase();
        show_waiting();
        tcp::set_wait_hook(Some(show_waiting));
        let result = llm_call(conv);
        tcp::set_wait_hook(None);
        kprint!("{}\r\x1b[2K{}", ui::STATUS, ui::END);
        if let Err(e) = result {
            kprint!("{}[error] {e}\n{}", ui::ERROR, ui::END);
            conv.truncate(mark); // drop the unfinished exchange so the history stays well-formed
            return;
        }
        let Some(reply) = conv.last() else { return };
        if let Some(text) = reply.content.as_deref().filter(|t| !t.is_empty()) {
            console(ui::REPLY.as_bytes());
            console(text);
            console(b"\n");
            console(ui::END.as_bytes());
        }
        if reply.calls.is_empty() {
            return;
        }
        let at = conv.len() - 1;
        for k in 0..conv[at].calls.len() {
            let result = exec_tool(&conv[at].calls[k]);
            let id = conv[at].calls[k].id.clone();
            conv.push(Message { role: "tool", content: Some(result), calls: Vec::new(), tool_call_id: Some(id) });
        }
    }
    kprint!("{}[stopped after {MAX_STEPS} steps]\n{}", ui::ERROR, ui::END);
}

/// One request from the user: the model answers, possibly through several
/// rounds of tool calls.
pub fn turn(user_text: &[u8]) {
    if !net::up() {
        kprint!(
            "{}[error] no network device found. Start QEMU with -netdev user,id=n0 -device virtio-net-pci,netdev=n0\n{}",
            ui::ERROR,
            ui::END
        );
        return;
    }
    // the conversation leaves the cell for the turn: no borrow is held while waiting
    let mut conv = CONVERSATION.with(core::mem::take);
    run_turn(&mut conv, user_text);
    CONVERSATION.with(|c| *c = conv);
}
