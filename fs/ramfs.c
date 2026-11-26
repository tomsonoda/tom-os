/* In-memory flat filesystem: path -> bytes. Contents are lost on reboot. */
#include <stdlib.h>
#include <string.h>
#include "../kernel/tomos.h"

typedef struct file {
    struct file *next;
    char *name;
    char *data;
    size_t len;
} file;

static file *files;

static const char *norm(const char *path) {
    for (;;) {
        if (path[0] == '/') path++;
        else if (path[0] == '.' && path[1] == '/') path += 2;
        else return path;
    }
}

static file *find(const char *path) {
    path = norm(path);
    for (file *f = files; f; f = f->next)
        if (strcmp(f->name, path) == 0) return f;
    return NULL;
}

int fs_write(const char *path, const char *data, size_t len) {
    path = norm(path);
    if (!*path || strlen(path) > 200) return -1;
    char *copy = malloc(len + 1);
    if (!copy) return -1;
    memcpy(copy, data, len);
    copy[len] = 0;

    file *f = find(path);
    if (f) {
        free(f->data);
    } else {
        f = calloc(1, sizeof *f);
        if (!f || !(f->name = strdup(path))) {
            free(f);
            free(copy);
            return -1;
        }
        /* keep the list sorted by name so listings are stable */
        file **pp = &files;
        while (*pp && strcmp((*pp)->name, path) < 0) pp = &(*pp)->next;
        f->next = *pp;
        *pp = f;
    }
    f->data = copy;
    f->len = len;
    return 0;
}

const char *fs_read(const char *path, size_t *len) {
    file *f = find(path);
    if (!f) return NULL;
    if (len) *len = f->len;
    return f->data;
}

int fs_delete(const char *path) {
    path = norm(path);
    for (file **pp = &files; *pp; pp = &(*pp)->next) {
        file *f = *pp;
        if (strcmp(f->name, path) == 0) {
            *pp = f->next;
            free(f->name);
            free(f->data);
            free(f);
            return 0;
        }
    }
    return -1;
}

int fs_rename(const char *from, const char *to) {
    file *f = find(from);
    if (!f) return -1;
    if (fs_write(to, f->data, f->len) != 0) return -1;
    if (strcmp(norm(from), norm(to)) != 0) fs_delete(from);
    return 0;
}

const char *fs_next(void **it, size_t *len) {
    file *f = *it ? ((file *)*it)->next : files;
    *it = f;
    if (!f) return NULL;
    if (len) *len = f->len;
    return f->name;
}
