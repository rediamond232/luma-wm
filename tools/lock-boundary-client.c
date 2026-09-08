// Verify globals and focus boundaries on the dedicated nested test backend.
#include <wayland-client.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static struct wl_compositor *compositor;
static struct wl_output *output;
static struct wl_seat *seat;
static struct wl_keyboard *keyboard;
static unsigned lock_globals, input_globals, entered;
static void keymap(void *d, struct wl_keyboard *k, uint32_t f, int fd, uint32_t s) { (void)d; (void)k; (void)f; (void)s; close(fd); }
static void enter(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *s, struct wl_array *keys) { (void)d; (void)k; (void)serial; (void)s; (void)keys; entered++; }
static void leave(void *d, struct wl_keyboard *k, uint32_t serial, struct wl_surface *s) { (void)d; (void)k; (void)serial; (void)s; }
static void key(void *d, struct wl_keyboard *k, uint32_t serial, uint32_t time, uint32_t key, uint32_t state) { (void)d; (void)k; (void)serial; (void)time; (void)key; (void)state; }
static void modifiers(void *d, struct wl_keyboard *k, uint32_t serial, uint32_t dep, uint32_t lat, uint32_t lock, uint32_t group) { (void)d; (void)k; (void)serial; (void)dep; (void)lat; (void)lock; (void)group; }
static void repeat(void *d, struct wl_keyboard *k, int32_t rate, int32_t delay) { (void)d; (void)k; (void)rate; (void)delay; }
static const struct wl_keyboard_listener keyboard_listener = {keymap, enter, leave, key, modifiers, repeat};
static void capabilities(void *d, struct wl_seat *s, uint32_t caps) {
    (void)d;
    if ((caps & WL_SEAT_CAPABILITY_KEYBOARD) && !keyboard) {
        keyboard = wl_seat_get_keyboard(s);
        wl_keyboard_add_listener(keyboard, &keyboard_listener, NULL);
    }
}
static void seat_name(void *d, struct wl_seat *s, const char *name) { (void)d; (void)s; (void)name; }
static const struct wl_seat_listener seat_listener = {capabilities, seat_name};
static void global(void *d, struct wl_registry *registry, uint32_t id, const char *interface, uint32_t version) {
    (void)d;
    if (!strcmp(interface, "wl_compositor")) compositor = wl_registry_bind(registry, id, &wl_compositor_interface, version < 4 ? version : 4);
    else if (!strcmp(interface, "wl_output") && !output) output = wl_registry_bind(registry, id, &wl_output_interface, 1);
    else if (!strcmp(interface, "wl_seat") && !seat) {
        seat = wl_registry_bind(registry, id, &wl_seat_interface, version < 5 ? version : 5);
        wl_seat_add_listener(seat, &seat_listener, NULL);
    } else if (!strcmp(interface, "ext_session_lock_manager_v1")) lock_globals++;
    else if (!strcmp(interface, "zwp_virtual_keyboard_manager_v1") || !strcmp(interface, "zwlr_virtual_pointer_manager_v1")) input_globals++;
}
static void removed(void *d, struct wl_registry *registry, uint32_t id) { (void)d; (void)registry; (void)id; }
static const struct wl_registry_listener registry_listener = {global, removed};
static void sync_display(struct wl_display *display) {
    if (wl_display_roundtrip(display) < 0) { fprintf(stderr, "lock client disconnected: %d\n", wl_display_get_error(display)); exit(2); }
}
int main(int argc, char **argv) {
    if (argc != 2 || !getenv("WM_LOCK_REJECTION_TEST")) return 2;
    struct wl_display *display = wl_display_connect(NULL);
    if (!display) return 2;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    sync_display(display); sync_display(display);
    if (!compositor || !output || !keyboard) {
        fprintf(stderr, "required desktop globals missing\n"); return 2;
    }
    if (lock_globals || input_globals || entered) {
        fprintf(stderr, "unexpected globals/focus: lock=%u virtual-input=%u entered=%u\n", lock_globals, input_globals, entered);
        return 1;
    }
    // Creating an ordinary unmapped surface must not acquire keyboard focus.
    struct wl_surface *surface = wl_compositor_create_surface(compositor);
    sync_display(display);
    if (entered) return 1;
    wl_surface_destroy(surface);
    sync_display(display);
    FILE *receipt = fopen(argv[1], "w");
    if (!receipt) return 2;
    fprintf(receipt, "{\"lock_globals\":%u,\"input_globals\":%u,\"entered\":%u}\n", lock_globals, input_globals, entered);
    fclose(receipt);
    wl_display_disconnect(display);
    return 0;
}
