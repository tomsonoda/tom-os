/* Cooperative threads. A thread runs until it calls sched_yield() or
 * cpu_idle(); the Lua runtime yields from its instruction-count hook, so
 * running apps share the CPU without being able to hog it. */
#include <stdlib.h>
#include <string.h>
#include "tomos.h"

#define MAX_THREADS 16
#define STACK_SIZE (1024 * 1024)

enum { T_FREE, T_RUN, T_DEAD };

struct thread {
    u64 rsp;
    void *stack;
    int state;
    int idle; /* last gave up the CPU because it was waiting */
    int kill;
    tty *tty;
    struct window *win;
    void *data;
    void (*fn)(void *);
    void *arg;
};

void ctx_switch(u64 *save_rsp, u64 new_rsp);

static thread threads[MAX_THREADS] = {{.state = T_RUN}}; /* slot 0: the boot thread */
static thread *cur = &threads[0];

tty *thread_tty(void) { return cur->tty; }
void thread_set_tty(tty *t) { cur->tty = t; }
struct window *thread_window(void) { return cur->win; }
void thread_set_window(struct window *w) { cur->win = w; }
void *thread_data(void) { return cur->data; }
void thread_set_data(void *p) { cur->data = p; }
int thread_killed(void) { return cur->kill; }
void thread_kill(thread *t) {
    if (t && t->state == T_RUN) t->kill = 1;
}

/* Device polling shared by every thread, at most once per millisecond. */
static void tick(void) {
    static u64 last = ~0ULL;
    static int busy;
    u64 now = time_ms();
    if (now == last || busy) return;
    last = now;
    busy = 1;
    console_poll_input();
    net_poll();
    sound_poll();
    if (wm_active()) wm_tick();
    busy = 0;
}

static void switch_next(int idle) {
    cur->idle = idle;
    tick();

    int all_idle = 1;
    for (int i = 0; i < MAX_THREADS; i++) {
        thread *t = &threads[i];
        if (t->state == T_DEAD && t != cur) { /* reap: never free the stack we are on */
            free(t->stack);
            memset(t, 0, sizeof *t);
        }
        if (t->state == T_RUN && !t->idle) all_idle = 0;
    }
    if (idle && all_idle) cpu_halt();

    int start = (int)(cur - threads);
    for (int i = 1; i <= MAX_THREADS; i++) {
        thread *t = &threads[(start + i) % MAX_THREADS];
        if (t->state != T_RUN) continue;
        if (t != cur) {
            thread *prev = cur;
            cur = t;
            ctx_switch(&prev->rsp, t->rsp);
        }
        return;
    }
}

void sched_yield(void) { switch_next(0); }
void cpu_idle(void) { switch_next(1); }

void thread_exit(void) {
    cur->state = T_DEAD;
    for (;;) switch_next(1);
}

static void thread_entry(void) {
    cur->fn(cur->arg);
    thread_exit();
}

thread *thread_spawn(void (*fn)(void *), void *arg, tty *t) {
    for (int i = 1; i < MAX_THREADS; i++) {
        thread *th = &threads[i];
        if (th->state != T_FREE) continue;
        void *stack = malloc(STACK_SIZE);
        if (!stack) return NULL;
        memset(th, 0, sizeof *th);
        th->stack = stack;
        th->fn = fn;
        th->arg = arg;
        th->tty = t;
        /* Initial frame for ctx_switch: six saved registers, then the entry
         * point as return address, then a zero as the entry's own caller. */
        u64 *sp = (u64 *)(((u64)stack + STACK_SIZE) & ~15ULL);
        *--sp = 0;
        *--sp = (u64)thread_entry;
        for (int r = 0; r < 6; r++) *--sp = 0;
        th->rsp = (u64)sp;
        th->state = T_RUN;
        return th;
    }
    return NULL;
}

homeview *thread_home(void) {
    tty *t = thread_tty();
    return t ? t->home : NULL;
}
