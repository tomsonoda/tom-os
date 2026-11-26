/* Lua runtime for tom-os apps: sandboxed state per run, with `io`, `os` and
 * `tomos` libraries backed by the console and the RAM filesystem. */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"
#include "../third_party/lua/lua.h"
#include "../third_party/lua/lauxlib.h"
#include "../third_party/lua/lualib.h"

enum { RUN_OK, ABORT_BREAK, ABORT_TIMEOUT, ABORT_EXIT };

static int g_abort;       /* sticky: once set, every hook call raises again */
static int g_interactive; /* console input allowed */
static u64 g_deadline;    /* 0 = none */
static size_t g_mem, g_mem_limit;

static const char *abort_msg(void) {
    switch (g_abort) {
    case ABORT_BREAK: return "interrupted (Ctrl-C)";
    case ABORT_TIMEOUT: return "time limit exceeded";
    default: return "exit";
    }
}

static int check_abort(void) {
    if (!g_abort) {
        if (console_break_pending()) g_abort = ABORT_BREAK;
        else if (g_deadline && time_ms() > g_deadline) g_abort = ABORT_TIMEOUT;
    }
    return g_abort;
}

static void hook(lua_State *L, lua_Debug *ar) {
    (void)ar;
    if (check_abort()) luaL_error(L, "%s", abort_msg());
}

static int raise_break(lua_State *L) {
    g_abort = ABORT_BREAK;
    return luaL_error(L, "%s", abort_msg());
}

static void *l_alloc(void *ud, void *ptr, size_t osize, size_t nsize) {
    (void)ud;
    if (!ptr) osize = 0;
    if (nsize == 0) {
        free(ptr);
        g_mem -= osize;
        return NULL;
    }
    if (nsize > osize && g_mem + (nsize - osize) > g_mem_limit) return NULL;
    void *np = realloc(ptr, nsize);
    if (np) g_mem = g_mem - osize + nsize;
    return np;
}

/* ---------------------------------------------------------------- reader */
typedef struct {
    strbuf buf;
    size_t pos;
    int kind; /* 0 file, 1 stdin, 2 stdout/stderr */
    int writable, readable, dirty, closed;
    char *path;
} lfile;

#define FILE_MT "tomos.file"

/* Pull one more line from the console into the stdin buffer. 0 on EOF. */
static int stdin_fill(lua_State *L, lfile *f) {
    static char line[2048];
    if (f->kind != 1 || !g_interactive) return 0;
    if (f->pos >= f->buf.len) {
        f->buf.len = 0;
        f->pos = 0;
    }
    int n = console_readline(line, sizeof line);
    if (n == KEY_BREAK) return raise_break(L);
    sb_put(&f->buf, line, (size_t)n);
    sb_putc(&f->buf, '\n');
    return 1;
}

static int read_line(lua_State *L, lfile *f, int keep_nl) {
    for (;;) {
        const char *base = sb_cstr(&f->buf) + f->pos;
        size_t avail = f->buf.len - f->pos;
        const char *nl = avail ? memchr(base, '\n', avail) : NULL;
        if (nl) {
            size_t n = (size_t)(nl - base);
            lua_pushlstring(L, base, n + (keep_nl ? 1 : 0));
            f->pos += n + 1;
            return 1;
        }
        if (stdin_fill(L, f)) continue;
        if (!avail) return 0;
        lua_pushlstring(L, base, avail);
        f->pos = f->buf.len;
        return 1;
    }
}

static int read_number(lua_State *L, lfile *f) {
    for (;;) {
        const char *p = sb_cstr(&f->buf);
        while (f->pos < f->buf.len && strchr(" \t\r\n", p[f->pos])) f->pos++;
        if (f->pos < f->buf.len) break;
        if (!stdin_fill(L, f)) return 0;
    }
    const char *tok = f->buf.p + f->pos;
    size_t n = 0;
    while (f->pos + n < f->buf.len && !strchr(" \t\r\n", tok[n]) && n < 63) n++;
    char tmp[64];
    memcpy(tmp, tok, n);
    tmp[n] = 0;
    f->pos += n;
    return lua_stringtonumber(L, tmp) != 0;
}

/* Implements file:read / io.read for the formats starting at stack index `first`. */
static int g_read(lua_State *L, lfile *f, int first) {
    int nargs = lua_gettop(L) - first + 1, n = 0;
    if (f->closed || !f->readable) return luaL_error(L, "file is not readable");
    if (nargs <= 0) {
        if (!read_line(L, f, 0)) luaL_pushfail(L);
        return 1;
    }
    for (int i = first; n < nargs; i++, n++) {
        int ok;
        if (lua_type(L, i) == LUA_TNUMBER) {
            size_t want = (size_t)luaL_checkinteger(L, i);
            if (f->pos >= f->buf.len && want > 0) stdin_fill(L, f);
            size_t avail = f->buf.len - f->pos;
            if (want > avail) want = avail;
            ok = avail > 0 || want == 0;
            if (ok) {
                lua_pushlstring(L, sb_cstr(&f->buf) + f->pos, want);
                f->pos += want;
            }
        } else {
            const char *fmt = luaL_checkstring(L, i);
            if (*fmt == '*') fmt++;
            switch (*fmt) {
            case 'n': ok = read_number(L, f); break;
            case 'l': ok = read_line(L, f, 0); break;
            case 'L': ok = read_line(L, f, 1); break;
            case 'a':
                lua_pushlstring(L, sb_cstr(&f->buf) + f->pos, f->buf.len - f->pos);
                f->pos = f->buf.len;
                ok = 1;
                break;
            default: return luaL_argerror(L, i, "invalid format");
            }
        }
        if (!ok) {
            luaL_pushfail(L);
            return n + 1;
        }
    }
    return n;
}

static int g_write(lua_State *L, lfile *f, int first) {
    int top = lua_gettop(L);
    if (f->closed || !f->writable) return luaL_error(L, "file is not writable");
    for (int i = first; i <= top; i++) {
        size_t len;
        const char *s = luaL_checklstring(L, i, &len);
        if (f->kind == 2) console_write(s, len);
        else {
            sb_put(&f->buf, s, len);
            f->dirty = 1;
        }
    }
    return 0;
}

static void file_flush(lfile *f) {
    if (f->kind == 0 && f->dirty && f->path) {
        fs_write(f->path, sb_cstr(&f->buf), f->buf.len);
        f->dirty = 0;
    }
}

static lfile *new_file(lua_State *L, int kind) {
    lfile *f = lua_newuserdatauv(L, sizeof *f, 0);
    memset(f, 0, sizeof *f);
    f->kind = kind;
    f->readable = kind != 2;
    f->writable = kind == 2;
    luaL_setmetatable(L, FILE_MT);
    return f;
}

static lfile *to_file(lua_State *L) { return luaL_checkudata(L, 1, FILE_MT); }

static int f_read(lua_State *L) { return g_read(L, to_file(L), 2); }

static int f_write(lua_State *L) {
    g_write(L, to_file(L), 2);
    lua_settop(L, 1);
    return 1;
}

static int f_flush(lua_State *L) {
    file_flush(to_file(L));
    lua_settop(L, 1);
    return 1;
}

static int f_close(lua_State *L) {
    lfile *f = to_file(L);
    if (f->kind == 0 && !f->closed) {
        file_flush(f);
        sb_free(&f->buf);
        free(f->path);
        f->path = NULL;
        f->closed = 1;
    }
    lua_pushboolean(L, 1);
    return 1;
}

static int f_gc(lua_State *L) {
    lfile *f = to_file(L);
    if (f->kind == 0 && !f->closed) file_flush(f);
    sb_free(&f->buf);
    free(f->path);
    f->path = NULL;
    f->closed = 1;
    return 0;
}

static int f_seek(lua_State *L) {
    static const char *const names[] = {"set", "cur", "end", NULL};
    lfile *f = to_file(L);
    int whence = luaL_checkoption(L, 2, "cur", names);
    lua_Integer off = luaL_optinteger(L, 3, 0);
    lua_Integer base = whence == 0 ? 0 : whence == 1 ? (lua_Integer)f->pos : (lua_Integer)f->buf.len;
    lua_Integer np = base + off;
    if (np < 0) np = 0;
    if (np > (lua_Integer)f->buf.len) np = (lua_Integer)f->buf.len;
    f->pos = (size_t)np;
    lua_pushinteger(L, np);
    return 1;
}

static int f_setvbuf(lua_State *L) {
    lua_pushboolean(L, 1);
    return 1;
}

static int lines_iter(lua_State *L) {
    lfile *f = lua_touserdata(L, lua_upvalueindex(1));
    int autoclose = lua_toboolean(L, lua_upvalueindex(2));
    if (f->closed) return 0;
    if (read_line(L, f, 0)) return 1;
    if (autoclose) {
        sb_free(&f->buf);
        free(f->path);
        f->path = NULL;
        f->closed = 1;
    }
    return 0;
}

static int f_lines(lua_State *L) {
    to_file(L);
    lua_pushvalue(L, 1);
    lua_pushboolean(L, 0);
    lua_pushcclosure(L, lines_iter, 2);
    return 1;
}

static int f_tostring(lua_State *L) {
    lfile *f = to_file(L);
    lua_pushfstring(L, f->closed ? "file (closed)" : "file (%p)", (void *)f);
    return 1;
}

static const luaL_Reg file_methods[] = {
    {"read", f_read}, {"write", f_write}, {"lines", f_lines}, {"close", f_close},
    {"flush", f_flush}, {"seek", f_seek}, {"setvbuf", f_setvbuf}, {NULL, NULL}};

/* -------------------------------------------------------------------- io */
static lfile *std_file(lua_State *L, const char *name) {
    lua_getfield(L, LUA_REGISTRYINDEX, name);
    lfile *f = lua_touserdata(L, -1);
    lua_pop(L, 1);
    return f;
}

static int io_open(lua_State *L) {
    const char *path = luaL_checkstring(L, 1);
    const char *mode = luaL_optstring(L, 2, "r");
    size_t len = 0;
    const char *data = fs_read(path, &len);
    int plus = strchr(mode, '+') != NULL;
    if (mode[0] == 'r' && !data) {
        luaL_pushfail(L);
        lua_pushfstring(L, "%s: No such file", path);
        lua_pushinteger(L, ENOENT);
        return 3;
    }
    if (mode[0] != 'r' && mode[0] != 'w' && mode[0] != 'a')
        return luaL_argerror(L, 2, "invalid mode");
    lfile *f = new_file(L, 0);
    f->path = strdup(path);
    f->readable = mode[0] == 'r' || plus;
    f->writable = mode[0] != 'r' || plus;
    if (mode[0] != 'w' && data) sb_put(&f->buf, data, len);
    if (mode[0] == 'w') f->dirty = 1; /* create/truncate even if nothing is written */
    return 1;
}

static int io_read(lua_State *L) { return g_read(L, std_file(L, "tomos.stdin"), 1); }

static int io_write(lua_State *L) {
    g_write(L, std_file(L, "tomos.stdout"), 1);
    lua_getfield(L, LUA_REGISTRYINDEX, "tomos.stdout");
    return 1;
}

static int io_lines(lua_State *L) {
    if (lua_isnoneornil(L, 1)) {
        lua_getfield(L, LUA_REGISTRYINDEX, "tomos.stdin");
        lua_pushboolean(L, 0);
    } else {
        const char *path = luaL_checkstring(L, 1);
        lua_settop(L, 1);
        lua_pushcfunction(L, io_open);
        lua_pushvalue(L, 1);
        lua_call(L, 1, 2);
        if (lua_isnil(L, -2)) return luaL_error(L, "%s: No such file", path);
        lua_pop(L, 1);
        lua_pushboolean(L, 1);
    }
    lua_pushcclosure(L, lines_iter, 2);
    return 1;
}

static int io_close(lua_State *L) {
    if (lua_isnoneornil(L, 1)) return 0;
    return f_close(L);
}

static int io_flush(lua_State *L) {
    (void)L;
    return 0;
}

static int io_type(lua_State *L) {
    lfile *f = luaL_testudata(L, 1, FILE_MT);
    if (!f) luaL_pushfail(L);
    else lua_pushstring(L, f->closed ? "closed file" : "file");
    return 1;
}

static const luaL_Reg io_funcs[] = {
    {"open", io_open}, {"read", io_read}, {"write", io_write}, {"lines", io_lines},
    {"close", io_close}, {"flush", io_flush}, {"type", io_type}, {NULL, NULL}};

static void open_io(lua_State *L) {
    luaL_newmetatable(L, FILE_MT);
    luaL_newlib(L, file_methods);
    lua_setfield(L, -2, "__index");
    lua_pushcfunction(L, f_gc);
    lua_setfield(L, -2, "__gc");
    lua_pushcfunction(L, f_close);
    lua_setfield(L, -2, "__close");
    lua_pushcfunction(L, f_tostring);
    lua_setfield(L, -2, "__tostring");
    lua_pop(L, 1);

    luaL_newlib(L, io_funcs);
    new_file(L, 1);
    lua_pushvalue(L, -1);
    lua_setfield(L, LUA_REGISTRYINDEX, "tomos.stdin");
    lua_setfield(L, -2, "stdin");
    new_file(L, 2);
    lua_pushvalue(L, -1);
    lua_setfield(L, LUA_REGISTRYINDEX, "tomos.stdout");
    lua_pushvalue(L, -1);
    lua_setfield(L, -3, "stderr");
    lua_setfield(L, -2, "stdout");
    lua_setglobal(L, "io");
}

/* -------------------------------------------------------------------- os */
static void civil_from_unix(lua_Integer t, int *Y, int *M, int *D, int *h, int *m, int *s, int *wd,
                            int *yd) {
    lua_Integer days = t / 86400, rem = t % 86400;
    if (rem < 0) {
        rem += 86400;
        days--;
    }
    *h = (int)(rem / 3600);
    *m = (int)(rem % 3600 / 60);
    *s = (int)(rem % 60);
    *wd = (int)(((days % 7) + 11) % 7); /* 1970-01-01 was a Thursday; 0 = Sunday */
    lua_Integer z = days + 719468;
    lua_Integer era = (z >= 0 ? z : z - 146096) / 146097;
    lua_Integer doe = z - era * 146097;
    lua_Integer yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    lua_Integer y = yoe + era * 400;
    lua_Integer doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    lua_Integer mp = (5 * doy + 2) / 153;
    *D = (int)(doy - (153 * mp + 2) / 5 + 1);
    *M = (int)(mp < 10 ? mp + 3 : mp - 9);
    *Y = (int)(y + (*M <= 2));
    static const int cum[12] = {0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334};
    int leap = (*Y % 4 == 0 && *Y % 100 != 0) || *Y % 400 == 0;
    *yd = cum[*M - 1] + *D + (leap && *M > 2);
}

static lua_Integer unix_from_civil(lua_Integer y, lua_Integer m, lua_Integer d) {
    y -= m <= 2;
    lua_Integer era = (y >= 0 ? y : y - 399) / 400;
    lua_Integer yoe = y - era * 400;
    lua_Integer doy = (153 * (m + (m > 2 ? -3 : 9)) + 2) / 5 + d - 1;
    lua_Integer doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    return era * 146097 + doe - 719468;
}

static lua_Integer field_int(lua_State *L, const char *key, lua_Integer def) {
    lua_getfield(L, 1, key);
    lua_Integer v = lua_isnil(L, -1) ? def : (lua_Integer)lua_tonumber(L, -1);
    lua_pop(L, 1);
    return v;
}

static int os_time(lua_State *L) {
    if (lua_istable(L, 1)) {
        lua_Integer days = unix_from_civil(field_int(L, "year", 1970), field_int(L, "month", 1),
                                           field_int(L, "day", 1));
        lua_pushinteger(L, days * 86400 + field_int(L, "hour", 12) * 3600 +
                               field_int(L, "min", 0) * 60 + field_int(L, "sec", 0));
    } else {
        lua_pushinteger(L, (lua_Integer)rtc_unix_time());
    }
    return 1;
}

static int os_clock(lua_State *L) {
    lua_pushnumber(L, (lua_Number)time_ms() / 1000.0);
    return 1;
}

static int os_date(lua_State *L) {
    static const char *const wdays[] = {"Sunday", "Monday", "Tuesday", "Wednesday",
                                        "Thursday", "Friday", "Saturday"};
    static const char *const months[] = {"January", "February", "March", "April",
                                         "May", "June", "July", "August",
                                         "September", "October", "November", "December"};
    const char *fmt = luaL_optstring(L, 1, "%c");
    lua_Integer t = luaL_opt(L, luaL_checkinteger, 2, (lua_Integer)rtc_unix_time());
    int Y, M, D, h, m, s, wd, yd;
    civil_from_unix(t, &Y, &M, &D, &h, &m, &s, &wd, &yd);
    if (*fmt == '!') fmt++;
    if (strncmp(fmt, "*t", 2) == 0) {
        lua_createtable(L, 0, 9);
#define SETI(k, v) (lua_pushinteger(L, v), lua_setfield(L, -2, k))
        SETI("year", Y); SETI("month", M); SETI("day", D); SETI("hour", h); SETI("min", m);
        SETI("sec", s); SETI("wday", wd + 1); SETI("yday", yd);
        lua_pushboolean(L, 0);
        lua_setfield(L, -2, "isdst");
        return 1;
    }
    luaL_Buffer b;
    char tmp[64];
    luaL_buffinit(L, &b);
    for (; *fmt; fmt++) {
        if (*fmt != '%' || !fmt[1]) {
            luaL_addchar(&b, *fmt);
            continue;
        }
        tmp[0] = 0;
        switch (*++fmt) {
        case 'Y': snprintf(tmp, sizeof tmp, "%d", Y); break;
        case 'y': snprintf(tmp, sizeof tmp, "%02d", Y % 100); break;
        case 'm': snprintf(tmp, sizeof tmp, "%02d", M); break;
        case 'd': snprintf(tmp, sizeof tmp, "%02d", D); break;
        case 'H': snprintf(tmp, sizeof tmp, "%02d", h); break;
        case 'I': snprintf(tmp, sizeof tmp, "%02d", h % 12 ? h % 12 : 12); break;
        case 'M': snprintf(tmp, sizeof tmp, "%02d", m); break;
        case 'S': snprintf(tmp, sizeof tmp, "%02d", s); break;
        case 'p': snprintf(tmp, sizeof tmp, "%s", h < 12 ? "AM" : "PM"); break;
        case 'j': snprintf(tmp, sizeof tmp, "%03d", yd); break;
        case 'w': snprintf(tmp, sizeof tmp, "%d", wd); break;
        case 'A': snprintf(tmp, sizeof tmp, "%s", wdays[wd]); break;
        case 'a': snprintf(tmp, sizeof tmp, "%.3s", wdays[wd]); break;
        case 'B': snprintf(tmp, sizeof tmp, "%s", months[M - 1]); break;
        case 'b': snprintf(tmp, sizeof tmp, "%.3s", months[M - 1]); break;
        case 'F': case 'x': snprintf(tmp, sizeof tmp, "%d-%02d-%02d", Y, M, D); break;
        case 'T': case 'X': snprintf(tmp, sizeof tmp, "%02d:%02d:%02d", h, m, s); break;
        case 'c': snprintf(tmp, sizeof tmp, "%d-%02d-%02d %02d:%02d:%02d", Y, M, D, h, m, s); break;
        case 'Z': snprintf(tmp, sizeof tmp, "UTC"); break;
        case '%': snprintf(tmp, sizeof tmp, "%%"); break;
        default: snprintf(tmp, sizeof tmp, "%%%c", *fmt); break;
        }
        luaL_addstring(&b, tmp);
    }
    luaL_pushresult(&b);
    return 1;
}

static int os_difftime(lua_State *L) {
    lua_pushnumber(L, luaL_checknumber(L, 1) - luaL_optnumber(L, 2, 0));
    return 1;
}

static int os_getenv(lua_State *L) {
    luaL_pushfail(L);
    return 1;
}

static int os_exit(lua_State *L) {
    g_abort = ABORT_EXIT;
    return luaL_error(L, "%s", abort_msg());
}

static int os_remove(lua_State *L) {
    const char *path = luaL_checkstring(L, 1);
    if (fs_delete(path) == 0) {
        lua_pushboolean(L, 1);
        return 1;
    }
    luaL_pushfail(L);
    lua_pushfstring(L, "%s: No such file", path);
    return 2;
}

static int os_rename(lua_State *L) {
    const char *from = luaL_checkstring(L, 1), *to = luaL_checkstring(L, 2);
    if (fs_rename(from, to) == 0) {
        lua_pushboolean(L, 1);
        return 1;
    }
    luaL_pushfail(L);
    lua_pushfstring(L, "%s: No such file", from);
    return 2;
}

static const luaL_Reg os_funcs[] = {
    {"time", os_time}, {"clock", os_clock}, {"date", os_date}, {"difftime", os_difftime},
    {"getenv", os_getenv}, {"exit", os_exit}, {"remove", os_remove}, {"rename", os_rename},
    {NULL, NULL}};

/* ----------------------------------------------------------------- tomos */
static int t_cls(lua_State *L) {
    (void)L;
    console_puts("\x1b[2J\x1b[H");
    return 0;
}

static int t_sleep(lua_State *L) {
    lua_Number ms = luaL_checknumber(L, 1);
    u64 end = time_ms() + (u64)(ms > 0 ? ms : 0);
    while (time_ms() < end) {
        if (check_abort()) return luaL_error(L, "%s", abort_msg());
        net_poll();
        cpu_idle();
    }
    return 0;
}

/* tomos.getkey([timeout_ms]) -> key name or character, nil on timeout */
static int t_getkey(lua_State *L) {
    int timeout = lua_isnoneornil(L, 1) ? -1 : (int)luaL_checknumber(L, 1);
    if (!g_interactive) {
        luaL_pushfail(L);
        return 1;
    }
    int c;
    if (timeout < 0 && g_deadline == 0) {
        c = console_getc(-1);
    } else { /* poll in slices so the time limit still applies */
        u64 end = timeout < 0 ? ~0ULL : time_ms() + (u64)timeout;
        for (;;) {
            c = console_getc(timeout < 0 || end - time_ms() > 50 ? 50 : (int)(end - time_ms()));
            if (c != KEY_TIMEOUT) break;
            if (check_abort()) return luaL_error(L, "%s", abort_msg());
            if (time_ms() >= end) break;
        }
    }
    if (c == KEY_BREAK) return raise_break(L);
    if (c == KEY_TIMEOUT) {
        luaL_pushfail(L);
        return 1;
    }
    if (c == 0x1B) {
        int c2 = console_getc(30);
        if (c2 == '[' || c2 == 'O') {
            int c3 = console_getc(30);
            const char *name = c3 == 'A' ? "up" : c3 == 'B' ? "down" : c3 == 'C' ? "right"
                             : c3 == 'D' ? "left" : "esc";
            lua_pushstring(L, name);
        } else {
            lua_pushliteral(L, "esc");
        }
        return 1;
    }
    if (c == '\n') lua_pushliteral(L, "enter");
    else if (c == '\b') lua_pushliteral(L, "backspace");
    else if (c == '\t') lua_pushliteral(L, "tab");
    else {
        char buf[4];
        int n = 0, extra = c >= 0xF0 ? 3 : c >= 0xE0 ? 2 : c >= 0xC0 ? 1 : 0;
        buf[n++] = (char)c;
        while (extra-- > 0) {
            int cc = console_getc(30);
            if (cc < 0) break;
            buf[n++] = (char)cc;
        }
        lua_pushlstring(L, buf, (size_t)n);
    }
    return 1;
}

static int t_readline(lua_State *L) {
    size_t plen;
    const char *prompt = luaL_optlstring(L, 1, "", &plen);
    console_write(prompt, plen);
    lua_settop(L, 0);
    lfile *in = std_file(L, "tomos.stdin");
    if (!read_line(L, in, 0)) luaL_pushfail(L);
    return 1;
}

static int t_time_ms(lua_State *L) {
    lua_pushinteger(L, (lua_Integer)time_ms());
    return 1;
}

static int t_files(lua_State *L) {
    void *it = NULL;
    const char *name;
    int i = 1;
    lua_newtable(L);
    while ((name = fs_next(&it, NULL))) {
        lua_pushstring(L, name);
        lua_rawseti(L, -2, i++);
    }
    return 1;
}

static int t_read_file(lua_State *L) {
    size_t len;
    const char *d = fs_read(luaL_checkstring(L, 1), &len);
    if (d) lua_pushlstring(L, d, len);
    else luaL_pushfail(L);
    return 1;
}

static int t_write_file(lua_State *L) {
    size_t len;
    const char *path = luaL_checkstring(L, 1);
    const char *d = luaL_checklstring(L, 2, &len);
    lua_pushboolean(L, fs_write(path, d, len) == 0);
    return 1;
}

static const luaL_Reg tomos_funcs[] = {
    {"cls", t_cls}, {"sleep", t_sleep}, {"getkey", t_getkey}, {"readline", t_readline},
    {"time_ms", t_time_ms}, {"files", t_files}, {"read_file", t_read_file},
    {"write_file", t_write_file}, {NULL, NULL}};

/* require(): loads modules from the RAM filesystem. */
static int l_require(lua_State *L) {
    const char *name = luaL_checkstring(L, 1);
    lua_settop(L, 1);
    luaL_getsubtable(L, LUA_REGISTRYINDEX, "tomos.loaded");
    if (lua_getfield(L, 2, name) != LUA_TNIL) return 1;
    lua_pop(L, 1);
    if (lua_getglobal(L, name) == LUA_TTABLE) return 1; /* built-in libraries */
    lua_pop(L, 1);

    static const char *const patterns[] = {"%s.lua", "apps/%s.lua", "lib/%s.lua", "%s"};
    char rel[128], path[160];
    snprintf(rel, sizeof rel, "%s", name);
    for (char *p = rel; *p; p++)
        if (*p == '.') *p = '/';
    for (size_t i = 0; i < sizeof patterns / sizeof *patterns; i++) {
        size_t len;
        snprintf(path, sizeof path, patterns[i], rel);
        const char *src = fs_read(path, &len);
        if (!src) continue;
        lua_pushfstring(L, "@%s", path);
        if (luaL_loadbuffer(L, src, len, lua_tostring(L, -1)) != LUA_OK) return lua_error(L);
        lua_pushstring(L, name);
        lua_call(L, 1, 1);
        if (lua_isnil(L, -1)) {
            lua_pop(L, 1);
            lua_pushboolean(L, 1);
        }
        lua_pushvalue(L, -1);
        lua_setfield(L, 2, name);
        return 1;
    }
    return luaL_error(L, "module '%s' not found", name);
}

/* ------------------------------------------------------------------- run */
typedef struct {
    const char *name, *code;
    size_t len;
} chunk;

static int msgh(lua_State *L) {
    const char *msg = lua_tostring(L, 1);
    if (!msg) msg = "(error object is not a string)";
    if (g_abort) lua_pushstring(L, msg); /* no traceback for Ctrl-C / exit / timeout */
    else luaL_traceback(L, L, msg, 1);
    return 1;
}

static int pmain(lua_State *L) {
    chunk *c = lua_touserdata(L, 1);
    static const luaL_Reg libs[] = {
        {LUA_GNAME, luaopen_base}, {LUA_COLIBNAME, luaopen_coroutine},
        {LUA_TABLIBNAME, luaopen_table}, {LUA_STRLIBNAME, luaopen_string},
        {LUA_MATHLIBNAME, luaopen_math}, {LUA_UTF8LIBNAME, luaopen_utf8}, {NULL, NULL}};
    for (const luaL_Reg *l = libs; l->func; l++) {
        luaL_requiref(L, l->name, l->func, 1);
        lua_pop(L, 1);
    }
    open_io(L);
    luaL_newlib(L, os_funcs);
    lua_setglobal(L, "os");
    luaL_newlib(L, tomos_funcs);
    lua_pushliteral(L, TOMOS_VERSION);
    lua_setfield(L, -2, "version");
    lua_pushinteger(L, 80);
    lua_setfield(L, -2, "width");
    lua_pushinteger(L, 25);
    lua_setfield(L, -2, "height");
    lua_setglobal(L, "tomos");
    lua_pushcfunction(L, l_require);
    lua_setglobal(L, "require");

    lua_sethook(L, hook, LUA_MASKCOUNT, 2000);
    lua_pushcfunction(L, msgh);
    lua_pushfstring(L, "@%s", c->name);
    if (luaL_loadbuffer(L, c->code, c->len, lua_tostring(L, -1)) != LUA_OK) return lua_error(L);
    lua_remove(L, -2);
    if (lua_pcall(L, 0, 0, -2) != LUA_OK) return lua_error(L);
    return 0;
}

int lua_run(const char *name, const char *code, size_t len, int interactive, int timeout_ms,
            strbuf *out, strbuf *err) {
    static int running;
    if (running) {
        sb_puts(err, "a Lua program is already running");
        return -1;
    }
    size_t avail = mem_total() - mem_used();
    g_mem = 0;
    g_mem_limit = avail / 2;
    lua_State *L = lua_newstate(l_alloc, NULL);
    if (!L) {
        sb_puts(err, "out of memory");
        return -1;
    }
    running = 1;
    g_abort = RUN_OK;
    g_interactive = interactive;
    g_deadline = timeout_ms > 0 ? time_ms() + (u64)timeout_ms : 0;
    console_clear_break();
    if (!interactive) console_capture(out, 16384);

    chunk c = {name, code, len};
    lua_pushcfunction(L, pmain);
    lua_pushlightuserdata(L, &c);
    int status = lua_pcall(L, 1, 0, 0);
    int rc = 0;
    if (status != LUA_OK && g_abort != ABORT_EXIT) {
        const char *msg = lua_tostring(L, -1);
        sb_puts(err, msg ? msg : "unknown error");
        rc = -1;
    }
    /* Don't let finalizers of a runaway program run forever either. */
    g_deadline = time_ms() + 2000;
    lua_close(L);
    console_capture(NULL, 0);
    running = 0;
    return rc;
}
