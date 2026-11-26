/* Running apps. In text mode an app takes over the console until it exits.
 * In graphics mode each app gets its own thread, terminal and window. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

#define MAX_APPS 10

typedef struct app {
    int id;
    char path[200];
    char *code;
    size_t len;
    tty *tty;
    window *win;
    thread *thr;
    int done, rc;
    strbuf err;
    int refs; /* the app's thread, plus anyone waiting for its result */
} app;

static app *apps[MAX_APPS];
static int next_id = 1;

/* Finds the source for "name", "apps/name.lua" or a full path. */
static const char *resolve(const char *name, char *path, size_t n, size_t *len) {
    static const char *const patterns[] = {"%s", "apps/%s.lua", "apps/%s"};
    for (size_t i = 0; i < sizeof patterns / sizeof *patterns; i++) {
        snprintf(path, n, patterns[i], name);
        const char *src = fs_read(path, len);
        if (src) return src;
    }
    return NULL;
}

static void app_unref(app *a) {
    if (--a->refs > 0) return;
    sb_free(&a->err);
    free(a->code);
    free(a);
}

static void app_main(void *arg) {
    app *a = arg;
    thread_set_window(a->win);
    a->rc = lua_run(a->path, a->code, a->len, 1, 0, NULL, &a->err);
    a->done = 1;

    if (!thread_killed()) {
        int failed = a->rc != 0;
        if (failed || !a->win->canvas) {
            /* keep the window so the output or the error can be read */
            if (a->win->canvas) wm_set_text(a->win);
            if (failed) kprintf("\x1b[0m\n\x1b[1;31m[error]\x1b[0m %s\n", sb_cstr(&a->err));
            kprintf("\x1b[0m\n[%s - press a key to close this window]", failed ? "stopped" : "finished");
            console_clear_break();
            console_getc(-1);
        }
    }
    wm_destroy(a->win);
    tty_destroy(a->tty);
    for (int i = 0; i < MAX_APPS; i++)
        if (apps[i] == a) apps[i] = NULL;
    app_unref(a);
}

static int launch_windowed(const char *path, const char *src, size_t len, strbuf *err) {
    int slot = 0;
    while (slot < MAX_APPS && apps[slot]) slot++;
    app *a = slot < MAX_APPS ? calloc(1, sizeof *a) : NULL;
    if (!a) {
        sb_puts(err, "too many apps are running; close one first");
        return -1;
    }
    snprintf(a->path, sizeof a->path, "%s", path);
    sb_init(&a->err);
    a->code = strndup(src, len); /* private copy: the app may rewrite its own file */
    a->len = len;
    a->tty = tty_create(80, 25);
    a->id = next_id++;
    a->refs = 2;
    if (a->code && a->tty) a->win = wm_create(path, a->tty, NULL);
    if (a->win) a->thr = thread_spawn(app_main, a, a->tty);
    if (!a->thr) {
        sb_puts(err, "could not start the app (out of memory or too many windows)");
        wm_destroy(a->win);
        tty_destroy(a->tty);
        a->refs = 1;
        app_unref(a);
        return -1;
    }
    a->win->owner = a->thr; /* closing the window stops the app */
    apps[slot] = a;

    /* Give it a moment so start-up errors can be reported to the caller. */
    u64 until = time_ms() + 2500;
    while (!a->done && time_ms() < until) cpu_idle();
    int r = !a->done ? 1 : a->rc == 0 ? 0 : -1;
    if (r < 0) sb_puts(err, sb_cstr(&a->err));
    app_unref(a);
    return r;
}

int app_launch(const char *name, strbuf *err) {
    char path[200];
    size_t len;
    const char *src = resolve(name, path, sizeof path, &len);
    if (!src) {
        sb_printf(err, "no such app: %s", name);
        return -1;
    }
    if (wm_active()) return launch_windowed(path, src, len, err);

    char *copy = strndup(src, len);
    if (!copy) {
        sb_puts(err, "out of memory");
        return -1;
    }
    int r = lua_run(path, copy, len, 1, 0, NULL, err);
    free(copy);
    console_puts("\x1b[0m\n");
    return r;
}

void app_list(void) {
    int n = 0;
    for (int i = 0; i < MAX_APPS; i++) {
        if (!apps[i]) continue;
        kprintf("  %2d  %s%s\n", apps[i]->id, apps[i]->path, apps[i]->done ? "  (finished)" : "");
        n++;
    }
    if (!n) console_puts("  (no apps are running)\n");
}

int app_kill(int id) {
    for (int i = 0; i < MAX_APPS; i++) {
        if (apps[i] && apps[i]->id == id) {
            thread_kill(apps[i]->thr);
            return 0;
        }
    }
    return -1;
}
