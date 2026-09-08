#define _POSIX_C_SOURCE 200809L
// Controlled same-surface unmap/remap and abrupt-disconnect fixture.
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
static struct wl_compositor *compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *wm;
static struct wl_surface *surface;
static struct xdg_toplevel *toplevel;
static int width = 320, height = 200, visible = 1;
static uint32_t color = 0xffc08040;
static unsigned commits;
static const char *app_id = "org.customwm.RemapTest";

static uint32_t configured_color(const char *name, uint32_t fallback) {
    const char *value = getenv(name);
    if (!value) return fallback;
    if (*value == '#') value++;
    errno = 0;
    char *end = NULL;
    unsigned long parsed = strtoul(value, &end, 16);
    if (errno || end == value || *end || parsed > 0xffffffUL) return fallback;
    return 0xff000000U | (uint32_t)parsed;
}
struct pixels { void *data; size_t length; };
static void release(void *data, struct wl_buffer *buffer) {
    struct pixels *pixels = data; munmap(pixels->data, pixels->length); free(pixels); wl_buffer_destroy(buffer);
}
static const struct wl_buffer_listener buffer_listener = {release};
static void draw(void) {
    if (!visible || width <= 0 || height <= 0 || width > 8192 || height > 8192) return;
    char path[] = "/tmp/luma-remap-buffer-XXXXXX";
    int fd = mkstemp(path); if (fd < 0) exit(2); unlink(path);
    size_t length = (size_t)width * height * 4;
    if (ftruncate(fd, (off_t)length)) exit(2);
    void *data = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (data == MAP_FAILED) exit(2);
    for (size_t i = 0; i < length / 4; ++i) ((uint32_t *)data)[i] = color;
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int)length);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, width, height, width * 4, WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool); close(fd);
    struct pixels *pixels = malloc(sizeof(*pixels)); if (!pixels) exit(2);
    *pixels = (struct pixels){data, length};
    wl_buffer_add_listener(buffer, &buffer_listener, pixels);
    wl_surface_attach(surface, buffer, 0, 0);
    wl_surface_damage(surface, 0, 0, width, height);
    wl_surface_commit(surface); commits++;
}
static void ping(void *d, struct xdg_wm_base *base, uint32_t serial) { (void)d; xdg_wm_base_pong(base, serial); }
static const struct xdg_wm_base_listener wm_listener = {ping};
static void configure(void *d, struct xdg_surface *s, uint32_t serial) { (void)d; xdg_surface_ack_configure(s, serial); draw(); }
static const struct xdg_surface_listener surface_listener = {configure};
static void size(void *d, struct xdg_toplevel *t, int32_t w, int32_t h, struct wl_array *states) { (void)d; (void)t; (void)states; if (w > 0) width = w; if (h > 0) height = h; }
static void close_window(void *d, struct xdg_toplevel *t) { (void)d; (void)t; _exit(0); }
static const struct xdg_toplevel_listener toplevel_listener = {.configure = size, .close = close_window};
static void format(void *d, struct wl_shm *s, uint32_t f) { (void)d; (void)s; (void)f; }
static const struct wl_shm_listener shm_listener = {format};
static void global(void *d, struct wl_registry *r, uint32_t id, const char *interface, uint32_t version) {
    (void)d; (void)version;
    if (!strcmp(interface, "wl_compositor")) compositor = wl_registry_bind(r, id, &wl_compositor_interface, 4);
    else if (!strcmp(interface, "wl_shm")) { shm = wl_registry_bind(r, id, &wl_shm_interface, 1); wl_shm_add_listener(shm, &shm_listener, NULL); }
    else if (!strcmp(interface, "xdg_wm_base")) { wm = wl_registry_bind(r, id, &xdg_wm_base_interface, 1); xdg_wm_base_add_listener(wm, &wm_listener, NULL); }
}
static void removed(void *d, struct wl_registry *r, uint32_t id) { (void)d; (void)r; (void)id; }
static const struct wl_registry_listener registry_listener = {global, removed};
static void roundtrip(struct wl_display *display) { if (wl_display_roundtrip(display) < 0) exit(2); }
int main(int argc, char **argv) {
    if (argc != 3 || !getenv("WM_REMAP_TEST")) return 2;
    if (getenv("WM_REMAP_APP_ID")) app_id = getenv("WM_REMAP_APP_ID");
    color = configured_color("WM_REMAP_COLOR", color);
    struct wl_display *display = wl_display_connect(NULL); if (!display) return 2;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL); roundtrip(display);
    if (!compositor || !shm || !wm) return 2;
    surface = wl_compositor_create_surface(compositor);
    struct xdg_surface *xdg = xdg_wm_base_get_xdg_surface(wm, surface);
    xdg_surface_add_listener(xdg, &surface_listener, NULL);
    toplevel = xdg_surface_get_toplevel(xdg);
    xdg_toplevel_add_listener(toplevel, &toplevel_listener, NULL);
    xdg_toplevel_set_app_id(toplevel, app_id);
    xdg_toplevel_set_title(toplevel, "Same-surface remap fixture");
    wl_surface_commit(surface); roundtrip(display); roundtrip(display);
    char previous[32] = "";
    for (;;) {
        char command[32] = "";
        FILE *input = fopen(argv[1], "r");
        if (input) { (void)fgets(command, sizeof(command), input); fclose(input); }
        if (command[0] && strcmp(command, previous)) {
            if (!strcmp(command, "quit")) _exit(0);
            if (!strcmp(command, "hide")) { visible = 0; wl_surface_attach(surface, NULL, 0, 0); wl_surface_commit(surface); }
            else if (!strcmp(command, "show")) {
                visible = 1;
                color = configured_color("WM_REMAP_SHOW_COLOR", 0xff20c080);
                // xdg-shell discards title and app-id on unmap. Re-submit
                // them before beginning the next initial configure sequence.
                xdg_toplevel_set_app_id(toplevel, app_id);
                xdg_toplevel_set_title(toplevel, "Same-surface remap fixture");
                wl_surface_commit(surface);
            }
            roundtrip(display); roundtrip(display);
            FILE *receipt = fopen(argv[2], "w"); if (!receipt) return 2;
            fprintf(receipt, "%s %u\n", command, commits); fclose(receipt);
            strcpy(previous, command);
        }
        while (wl_display_prepare_read(display)) if (wl_display_dispatch_pending(display) < 0) return 2;
        wl_display_flush(display);
        struct pollfd fd = {wl_display_get_fd(display), POLLIN, 0};
        if (poll(&fd, 1, 20) > 0) { if (wl_display_read_events(display) < 0) return 2; }
        else wl_display_cancel_read(display);
        if (wl_display_dispatch_pending(display) < 0) return 2;
    }
}
