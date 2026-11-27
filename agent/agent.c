/* The agent: conversation state, LLM calls (OpenAI-compatible chat
 * completions over plain HTTP) and the tools the model can use. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

#define MAX_STEPS 24
#define TOOL_RESULT_MAX 12000

static const char SYSTEM_PROMPT[] =
    "You are the built-in agent of tom-os, a tiny bare-metal operating system whose only "
    "interface is this conversation. There are no pre-installed applications: when the user "
    "wants a tool, a game or any other program, you write it on the spot as a Lua app and the "
    "user runs it immediately.\n"
    "\n"
    "Environment:\n"
    "- Apps are Lua 5.4 scripts saved as apps/<name>.lua in a flat in-memory filesystem "
    "(lost on reboot). The user starts one with /run <name> and can stop it with Ctrl-C.\n"
    "- Available libraries: base, string, table, math, utf8, coroutine, plus:\n"
    "  io.write, io.read (formats 'l', 'n', 'a', 'L', or a byte count), io.lines, "
    "io.open(path, mode) with file:read/write/lines/seek/close.\n"
    "  os.time, os.date, os.clock, os.remove, os.rename, os.exit.\n"
    "  tomos.cls() clears the text screen; tomos.getkey([timeout_ms]) returns one key without "
    "waiting for Enter ('a', 'up', 'down', 'left', 'right', 'enter', 'esc', 'backspace', "
    "'tab') or nil on timeout; tomos.sleep(ms); tomos.readline([prompt]); tomos.time_ms(); "
    "tomos.files(); tomos.read_file(path); tomos.write_file(path, data); tomos.gui is true "
    "when graphics are available.\n"
    "  require('name') loads name.lua, apps/name.lua or lib/name.lua.\n"
    "- Text apps get an 80x25 character terminal. ANSI escape sequences work (colors, cursor "
    "movement, clear screen). The screen shows ASCII and Japanese (kana, kanji, full-width "
    "symbols: two columns each, 16x16 pixels in gfx.text); other characters appear as '?'. "
    "Write app text in the user's language. The user can type Japanese into apps too "
    "(io.read and tomos.readline get UTF-8).\n"
    "- Graphics (only when graphical mode is on, see the end of this message): every app runs "
    "on its own screen (or window); the user switches between apps.\n"
    "  gfx.mode([title]) turns the app's screen into a pixel canvas as large as possible; "
    "gfx.mode(width, height [, title]) asks for a size (for example 640x480; smaller canvases "
    "are shown enlarged). It returns the width and height you got; lay out the app from "
    "them, or from gfx.size(). Colors are integers 0xRRGGBB.\n"
    "  Drawing goes to a hidden buffer: gfx.clear([color]), gfx.rect(x, y, w, h, color) "
    "filled, gfx.box(x, y, w, h, color) outline, gfx.line(x1, y1, x2, y2, color), "
    "gfx.circle(x, y, r, color [, filled]), gfx.pixel(x, y, color), gfx.text(x, y, string, "
    "color [, scale]) with an 8x16 font, gfx.rgb(r, g, b), gfx.size().\n"
    "  gfx.flip([fps]) shows the buffer and waits for the next frame (default 60 per second); "
    "call it once per frame.\n"
    "  gfx.key(name) is true while a key is held: 'left', 'right', 'up', 'down', 'space', "
    "'enter', 'escape', 'shift', 'ctrl', letters and digits. Use it for game controls; "
    "tomos.getkey(0) still returns typed keys one at a time. gfx.mouse() returns x, y, "
    "left_down, right_down in canvas coordinates; a click counts as down for at least one "
    "call, so check it once per frame and react when it changes from up to down.\n"
    "  A typical game: gfx.mode(...); then loop { read gfx.key, update, gfx.clear, draw, "
    "gfx.flip() } until gfx.key('escape').\n"
    "- Not available: os.execute, io.popen, the debug and package libraries, networking, "
    "sound, images loaded from files.\n"
    "\n"
    "How to work:\n"
    "- To build an app: write it with write_file, check it with run_lua when a non-interactive "
    "check is possible (for example loading the file with loadfile to catch syntax errors, or "
    "testing pure functions), fix any errors, then either call launch_app so the user can use "
    "it right away or tell them the /run command.\n"
    "- run_lua has no keyboard input and no window: io.read and tomos.getkey return nil there, "
    "gfx calls fail, and it stops after 10 seconds. Never run an interactive app with run_lua; "
    "use launch_app for that. In graphical mode launch_app returns a few seconds after the app "
    "starts; errors that happen later are shown in the app's window.\n"
    "- When the user asks for a game or anything visual and graphical mode is on, use gfx. "
    "In text mode use the terminal instead.\n"
    "- Every interactive app must have an obvious way to quit (for example the q key).\n"
    "- For questions that need no program, just answer briefly.\n"
    "- Reply in the language the user writes in. Keep answers short; the screen is small.";

static const char TOOLS_JSON[] =
    "[{\"type\":\"function\",\"function\":{\"name\":\"write_file\","
    "\"description\":\"Create or overwrite a file in the in-memory filesystem. Apps go in "
    "apps/<name>.lua.\","
    "\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"},"
    "\"content\":{\"type\":\"string\"}},\"required\":[\"path\",\"content\"]}}},"
    "{\"type\":\"function\",\"function\":{\"name\":\"read_file\","
    "\"description\":\"Read a file from the in-memory filesystem.\","
    "\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},"
    "\"required\":[\"path\"]}}},"
    "{\"type\":\"function\",\"function\":{\"name\":\"list_files\","
    "\"description\":\"List all files with their sizes.\","
    "\"parameters\":{\"type\":\"object\",\"properties\":{}}}},"
    "{\"type\":\"function\",\"function\":{\"name\":\"delete_file\","
    "\"description\":\"Delete a file.\","
    "\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},"
    "\"required\":[\"path\"]}}},"
    "{\"type\":\"function\",\"function\":{\"name\":\"run_lua\","
    "\"description\":\"Run Lua code non-interactively and return what it printed, or the error "
    "with a traceback. No keyboard input; 10 second limit.\","
    "\"parameters\":{\"type\":\"object\",\"properties\":{\"code\":{\"type\":\"string\"}},"
    "\"required\":[\"code\"]}}},"
    "{\"type\":\"function\",\"function\":{\"name\":\"launch_app\","
    "\"description\":\"Start a saved Lua app on the user's console. The user interacts with it "
    "directly; this returns when the app exits, with the error if it crashed.\","
    "\"parameters\":{\"type\":\"object\",\"properties\":{\"path\":{\"type\":\"string\"}},"
    "\"required\":[\"path\"]}}}]";

/* ----------------------------------------------------------- conversation */
typedef struct {
    char *id, *name, *args;
} toolcall;

typedef struct {
    const char *role;
    char *content; /* may be NULL for assistant messages that only call tools */
    toolcall *calls;
    int ncalls;
    char *tool_call_id;
} message;

static message *msgs;
static int nmsgs, cap_msgs;

static message *msg_add(const char *role, const char *content) {
    if (nmsgs == cap_msgs) {
        int ncap = cap_msgs ? cap_msgs * 2 : 32;
        message *n = realloc(msgs, (size_t)ncap * sizeof *n);
        if (!n) return NULL;
        msgs = n;
        cap_msgs = ncap;
    }
    message *m = &msgs[nmsgs++];
    memset(m, 0, sizeof *m);
    m->role = role;
    m->content = content ? strdup(content) : NULL;
    return m;
}

static void msg_free(message *m) {
    free(m->content);
    free(m->tool_call_id);
    for (int i = 0; i < m->ncalls; i++) {
        free(m->calls[i].id);
        free(m->calls[i].name);
        free(m->calls[i].args);
    }
    free(m->calls);
}

static void msgs_truncate(int n) {
    while (nmsgs > n) msg_free(&msgs[--nmsgs]);
}

void agent_reset(void) { msgs_truncate(0); }

/* -------------------------------------------------------------- settings */
/* LLM connection settings: defaults, overridden by kernel arguments at boot
 * and by the /set command at run time. */
static struct {
    const char *key;
    char value[128];
} settings[] = {
    {"llm", "10.0.2.2:8080"},
    {"model", "default"},
    {"path", "/v1/chat/completions"},
};
#define NSETTINGS (sizeof settings / sizeof *settings)

static void settings_load(void) {
    static int loaded;
    if (loaded) return;
    loaded = 1;
    for (size_t i = 0; i < NSETTINGS; i++)
        cmdline_get(settings[i].key, settings[i].value, sizeof settings[i].value);
}

const char *agent_setting(const char *key) {
    settings_load();
    for (size_t i = 0; i < NSETTINGS; i++)
        if (strcmp(settings[i].key, key) == 0) return settings[i].value;
    return "";
}

int agent_set(const char *key, const char *value) {
    settings_load();
    for (size_t i = 0; i < NSETTINGS; i++) {
        if (strcmp(settings[i].key, key) != 0) continue;
        if (!*value || strlen(value) >= sizeof settings[i].value) return -1;
        strcpy(settings[i].value, value);
        return 0;
    }
    return -1;
}

/* ------------------------------------------------------------------- LLM */
static void put_field(strbuf *b, const char *key, const char *val) {
    json_quote(b, key, strlen(key));
    sb_putc(b, ':');
    json_quote(b, val, strlen(val));
}

static void build_request(strbuf *b, const char *model) {
    sb_putc(b, '{');
    put_field(b, "model", model);
    sb_puts(b, ",\"stream\":false,\"messages\":[{");
    put_field(b, "role", "system");
    sb_putc(b, ',');
    {
        strbuf sys;
        sb_init(&sys);
        sb_puts(&sys, SYSTEM_PROMPT);
        sb_puts(&sys, wm_active() ? "\n\nGraphical mode is ON: gfx is available and apps run in windows."
                                  : "\n\nGraphical mode is OFF (text mode): gfx is not available.");
        put_field(b, "content", sb_cstr(&sys));
        sb_free(&sys);
    }
    sb_putc(b, '}');
    for (int i = 0; i < nmsgs; i++) {
        message *m = &msgs[i];
        sb_puts(b, ",{");
        put_field(b, "role", m->role);
        sb_puts(b, ",\"content\":");
        if (m->content && (*m->content || !m->ncalls)) json_quote(b, m->content, strlen(m->content));
        else sb_puts(b, "null");
        if (m->tool_call_id) {
            sb_putc(b, ',');
            put_field(b, "tool_call_id", m->tool_call_id);
        }
        if (m->ncalls) {
            sb_puts(b, ",\"tool_calls\":[");
            for (int k = 0; k < m->ncalls; k++) {
                if (k) sb_putc(b, ',');
                sb_putc(b, '{');
                put_field(b, "id", m->calls[k].id);
                sb_puts(b, ",\"type\":\"function\",\"function\":{");
                put_field(b, "name", m->calls[k].name);
                sb_putc(b, ',');
                put_field(b, "arguments", m->calls[k].args);
                sb_puts(b, "}}");
            }
            sb_putc(b, ']');
        }
        sb_putc(b, '}');
    }
    sb_puts(b, "],\"tools\":");
    sb_puts(b, TOOLS_JSON);
    sb_putc(b, '}');
}

/* Calls the model with the current conversation and appends its reply.
 * Returns 0 on success; on failure describes the reason in `errmsg`. */
static int llm_call(strbuf *errmsg) {
    char endpoint[64];
    const char *model = agent_setting("model"), *path = agent_setting("path");
    snprintf(endpoint, sizeof endpoint, "%s", agent_setting("llm"));

    u32 ip;
    u16 port = 80;
    char *colon = strchr(endpoint, ':');
    if (colon) {
        *colon = 0;
        port = (u16)atoi(colon + 1);
    }
    if (!net_parse_ip(endpoint, &ip)) {
        sb_printf(errmsg, "bad llm= address '%s' (expected IP:port)", endpoint);
        return -1;
    }

    strbuf req, resp;
    sb_init(&req);
    sb_init(&resp);
    build_request(&req, model);
    int status = 0;
    int r = http_post_json(ip, port, path, sb_cstr(&req), req.len, &resp, &status, 600000);
    sb_free(&req);
    if (r < 0) {
        sb_printf(errmsg, "LLM request to %s:%u failed while %s: %s", endpoint, port, http_phase,
                  net_strerror(r));
        if (r == NET_ETIMEOUT && !strcmp(http_phase, "connecting"))
            sb_puts(errmsg, " (is the LLM server running? check /info and /set llm)");
        sb_free(&resp);
        return -1;
    }

    jv *root = json_parse(sb_cstr(&resp), resp.len);
    jv *err = json_get(root, "error");
    jv *m = json_get(json_at(json_get(root, "choices"), 0), "message");
    if (status != 200 || !m) {
        const char *detail = json_str(json_get(err, "message"));
        if (!detail) detail = json_str(err);
        if (status == 200 && detail) sb_printf(errmsg, "LLM error: %.300s", detail);
        else
            sb_printf(errmsg, "LLM returned HTTP %d: %.300s", status,
                      detail ? detail : resp.len ? resp.p : "(empty response)");
        json_free(root);
        sb_free(&resp);
        return -1;
    }

    message *am = msg_add("assistant", json_str(json_get(m, "content")));
    jv *calls = json_get(m, "tool_calls");
    int n = 0;
    for (jv *c = calls && calls->type == J_ARR ? calls->child : NULL; c; c = c->next) n++;
    if (am && n) {
        am->calls = calloc((size_t)n, sizeof *am->calls);
        for (jv *c = calls->child; am->calls && c; c = c->next) {
            jv *fn = json_get(c, "function");
            const char *id = json_str(json_get(c, "id"));
            const char *name = json_str(json_get(fn, "name"));
            jv *args = json_get(fn, "arguments");
            toolcall *tc = &am->calls[am->ncalls++];
            tc->id = strdup(id ? id : "call");
            tc->name = strdup(name ? name : "");
            tc->args = strdup(json_str(args) ? json_str(args) : "{}");
        }
    }
    json_free(root);
    sb_free(&resp);
    if (!am) sb_puts(errmsg, "out of memory");
    return am ? 0 : -1;
}

/* ----------------------------------------------------------------- tools */
static void exec_tool(const toolcall *tc, strbuf *out) {
    jv *args = json_parse(tc->args, strlen(tc->args));
    const char *path = json_str(json_get(args, "path"));

    if (!args || args->type != J_OBJ) {
        sb_puts(out, "error: arguments are not a valid JSON object");
    } else if (strcmp(tc->name, "write_file") == 0) {
        jv *content = json_get(args, "content");
        if (!path || !json_str(content)) {
            sb_puts(out, "error: 'path' and 'content' are required");
        } else if (fs_write(path, content->str, content->len) != 0) {
            sb_puts(out, "error: could not write file (bad path or out of memory)");
        } else {
            kprintf("  [write_file] %s (%zu bytes)\n", path, content->len);
            sb_printf(out, "wrote %zu bytes to %s", content->len, path);
        }
    } else if (strcmp(tc->name, "read_file") == 0) {
        size_t len;
        const char *d = path ? fs_read(path, &len) : NULL;
        kprintf("  [read_file] %s\n", path ? path : "?");
        if (d) sb_put(out, d, len);
        else sb_printf(out, "error: no such file: %s", path ? path : "(missing path)");
    } else if (strcmp(tc->name, "list_files") == 0) {
        void *it = NULL;
        size_t len;
        const char *name;
        kprintf("  [list_files]\n");
        while ((name = fs_next(&it, &len))) sb_printf(out, "%s (%zu bytes)\n", name, len);
        if (!out->len) sb_puts(out, "(no files)");
    } else if (strcmp(tc->name, "delete_file") == 0) {
        kprintf("  [delete_file] %s\n", path ? path : "?");
        if (path && fs_delete(path) == 0) sb_printf(out, "deleted %s", path);
        else sb_printf(out, "error: no such file: %s", path ? path : "(missing path)");
    } else if (strcmp(tc->name, "run_lua") == 0) {
        jv *code = json_get(args, "code");
        if (!json_str(code)) {
            sb_puts(out, "error: 'code' is required");
        } else {
            strbuf err;
            sb_init(&err);
            kprintf("  [run_lua] %zu bytes ... ", code->len);
            int r = lua_run("run_lua", code->str, code->len, 0, 10000, out, &err);
            kprintf("%s\n", r == 0 ? "ok" : "error");
            if (r != 0) sb_printf(out, "%serror: %s", out->len ? "\n" : "", sb_cstr(&err));
            else if (!out->len) sb_puts(out, "(finished with no output)");
            sb_free(&err);
        }
    } else if (strcmp(tc->name, "launch_app") == 0) {
        if (!path) {
            sb_puts(out, "error: 'path' is required");
        } else {
            strbuf err;
            sb_init(&err);
            kprintf("  [launch_app] %s\n", path);
            int r = app_launch(path, &err);
            if (r == 0) sb_puts(out, "the app ran and exited normally");
            else if (r == 1)
                sb_puts(out, "the app started without errors and is running in its own window");
            else {
                kprintf("  [app error] %s\n", sb_cstr(&err));
                sb_printf(out, "the app stopped with an error: %s", sb_cstr(&err));
            }
            sb_free(&err);
        }
    } else {
        sb_printf(out, "error: unknown tool '%s'", tc->name);
    }
    json_free(args);
    if (out->len > TOOL_RESULT_MAX) {
        out->len = TOOL_RESULT_MAX;
        out->p[out->len] = 0;
        sb_puts(out, "\n[output truncated]");
    }
}

/* ------------------------------------------------------ kana to kanji */
static const char CONVERT_PROMPT[] =
    "You are the kana-to-kanji converter of a Japanese input method. The user message is "
    "Japanese typed in hiragana (it may also contain ASCII or katakana). Reply with exactly the "
    "same words written the way a Japanese writer would: kanji, hiragana and katakana where "
    "appropriate, like an IME conversion. Do not translate, answer, add, remove or explain "
    "anything, and do not use quotes. Reply with the converted text only.";

int agent_convert_kana(const char *kana, const char *avoid, strbuf *out, strbuf *err) {
    char endpoint[64];
    snprintf(endpoint, sizeof endpoint, "%s", agent_setting("llm"));
    u32 ip;
    u16 port = 80;
    char *colon = strchr(endpoint, ':');
    if (colon) {
        *colon = 0;
        port = (u16)atoi(colon + 1);
    }
    if (!net_up() || !net_parse_ip(endpoint, &ip)) {
        sb_puts(err, "no LLM for conversion");
        return -1;
    }
    strbuf req, resp, sys;
    sb_init(&req);
    sb_init(&resp);
    sb_init(&sys);
    sb_puts(&sys, CONVERT_PROMPT);
    if (avoid) {
        sb_puts(&sys, " The user did not want this conversion: ");
        sb_puts(&sys, avoid);
        sb_puts(&sys, " - reply with the next most likely different one.");
    }
    sb_putc(&req, '{');
    put_field(&req, "model", agent_setting("model"));
    sb_puts(&req, ",\"stream\":false,\"messages\":[{");
    put_field(&req, "role", "system");
    sb_putc(&req, ',');
    put_field(&req, "content", sb_cstr(&sys));
    sb_puts(&req, "},{");
    put_field(&req, "role", "user");
    sb_putc(&req, ',');
    put_field(&req, "content", kana);
    sb_puts(&req, "}]}");
    sb_free(&sys);

    int status = 0;
    int r = http_post_json(ip, port, agent_setting("path"), sb_cstr(&req), req.len, &resp, &status, 60000);
    sb_free(&req);
    if (r < 0) {
        sb_puts(err, net_strerror(r));
        sb_free(&resp);
        return -1;
    }
    jv *root = json_parse(sb_cstr(&resp), resp.len);
    const char *text = json_str(json_get(json_get(json_at(json_get(root, "choices"), 0), "message"), "content"));
    if (status != 200 || !text) {
        sb_puts(err, "conversion failed");
        r = -1;
    } else {
        /* one line, without surrounding blanks or quotes */
        while (*text == ' ' || *text == '\n' || *text == '"' || !strncmp(text, "「", 3))
            text += *text == ' ' || *text == '\n' || *text == '"' ? 1 : 3;
        size_t len = strcspn(text, "\n");
        while (len && (text[len - 1] == ' ' || text[len - 1] == '"')) len--;
        if (len >= 3 && !strncmp(text + len - 3, "」", 3) && !strstr(kana, "「")) len -= 3;
        sb_put(out, text, len);
        r = 0;
    }
    json_free(root);
    sb_free(&resp);
    return r;
}

/* ------------------------------------------------------------------ turn */
/* Status line while waiting for the model; writing a whole app can take a
 * minute or two, so show that time is passing. */
static u64 wait_start;

static void show_waiting(void) {
    const char *what = !strcmp(http_phase, "waiting for the reply") ? "thinking"
                     : !strcmp(http_phase, "receiving the reply") ? "thinking, reply started"
                     : http_phase;
    kprintf("\r\x1b[2m(%s... %llu s, Ctrl-C to cancel)\x1b[0m\x1b[K", what,
            (time_ms() - wait_start) / 1000);
}

void agent_turn(const char *user_text) {
    int mark = nmsgs;
    if (!net_up()) {
        kprintf("[error] no network device found. Start QEMU with "
                "-netdev user,id=n0 -device virtio-net-pci,netdev=n0\n");
        return;
    }
    if (!msg_add("user", user_text)) return;
    console_clear_break();

    for (int step = 0; step < MAX_STEPS; step++) {
        wait_start = time_ms();
        http_phase = "connecting";
        show_waiting();
        net_wait_hook = show_waiting;
        strbuf errmsg;
        sb_init(&errmsg);
        int r = llm_call(&errmsg);
        net_wait_hook = NULL;
        kprintf("\r\x1b[2K");
        if (r != 0) kprintf("[error] %s\n", sb_cstr(&errmsg));
        sb_free(&errmsg);
        if (r != 0) {
            /* drop the unfinished exchange so the history stays well-formed */
            msgs_truncate(mark);
            return;
        }
        message *am = &msgs[nmsgs - 1];
        if (am->content && *am->content) kprintf("%s\n", am->content);
        if (!am->ncalls) return;

        int ncalls = am->ncalls, idx = nmsgs - 1;
        for (int k = 0; k < ncalls; k++) {
            strbuf out;
            sb_init(&out);
            exec_tool(&msgs[idx].calls[k], &out);
            message *tm = msg_add("tool", sb_cstr(&out));
            if (tm) tm->tool_call_id = strdup(msgs[idx].calls[k].id);
            sb_free(&out);
        }
    }
    kprintf("[stopped after %d steps]\n", MAX_STEPS);
}
