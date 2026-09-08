#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>

#include "wlr-screencopy.h"

static struct wl_shm *shm;
static struct wl_output *output;
static struct zwlr_screencopy_manager_v1 *manager;
static uint32_t format;
static int32_t width, height, stride;
static bool constraints_done, frame_ready, frame_failed, buffer_released;

static void output_geometry(void *data, struct wl_output *wl_output, int32_t x, int32_t y,
                            int32_t physical_width, int32_t physical_height, int32_t subpixel,
                            const char *make, const char *model, int32_t transform) {
    (void)data; (void)wl_output; (void)x; (void)y; (void)physical_width;
    (void)physical_height; (void)subpixel; (void)make; (void)model; (void)transform;
}
static void output_mode(void *data, struct wl_output *wl_output, uint32_t flags, int32_t w,
                        int32_t h, int32_t refresh) {
    (void)data; (void)wl_output; (void)flags; (void)w; (void)h; (void)refresh;
}
static void output_done(void *data, struct wl_output *wl_output) {
    (void)data; (void)wl_output;
}
static void output_scale(void *data, struct wl_output *wl_output, int32_t scale) {
    (void)data; (void)wl_output; (void)scale;
}
static const struct wl_output_listener output_listener = {
    .geometry = output_geometry,
    .mode = output_mode,
    .done = output_done,
    .scale = output_scale,
};

static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
                            const char *interface, uint32_t version) {
    (void)data;
    if (!strcmp(interface, wl_shm_interface.name)) {
        shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    } else if (!strcmp(interface, wl_output_interface.name) && !output) {
        output = wl_registry_bind(registry, name, &wl_output_interface, version < 2 ? version : 2);
        wl_output_add_listener(output, &output_listener, NULL);
    } else if (!strcmp(interface, zwlr_screencopy_manager_v1_interface.name)) {
        manager = wl_registry_bind(registry, name, &zwlr_screencopy_manager_v1_interface,
                                   version < 3 ? version : 3);
    }
}
static void registry_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_remove,
};

static void frame_buffer(void *data, struct zwlr_screencopy_frame_v1 *frame, uint32_t f,
                         uint32_t w, uint32_t h, uint32_t s) {
    (void)data; (void)frame;
    format = f; width = (int32_t)w; height = (int32_t)h; stride = (int32_t)s;
}
static void frame_flags(void *data, struct zwlr_screencopy_frame_v1 *frame, uint32_t flags) {
    (void)data; (void)frame; (void)flags;
}
static void frame_ready_event(void *data, struct zwlr_screencopy_frame_v1 *frame,
                              uint32_t hi, uint32_t lo, uint32_t ns) {
    (void)data; (void)frame; (void)hi; (void)lo; assert(ns < 1000000000u);
    frame_ready = true;
}
static void frame_failed_event(void *data, struct zwlr_screencopy_frame_v1 *frame) {
    (void)data; (void)frame; frame_failed = true;
}
static void frame_damage(void *data, struct zwlr_screencopy_frame_v1 *frame,
                         uint32_t x, uint32_t y, uint32_t w, uint32_t h) {
    (void)data; (void)frame; (void)x; (void)y; (void)w; (void)h;
}
static void frame_linux_dmabuf(void *data, struct zwlr_screencopy_frame_v1 *frame,
                               uint32_t f, uint32_t w, uint32_t h) {
    (void)data; (void)frame; (void)f; (void)w; (void)h;
}
static void frame_buffer_done(void *data, struct zwlr_screencopy_frame_v1 *frame) {
    (void)data; (void)frame; constraints_done = true;
}
static const struct zwlr_screencopy_frame_v1_listener frame_listener = {
    .buffer = frame_buffer,
    .flags = frame_flags,
    .ready = frame_ready_event,
    .failed = frame_failed_event,
    .damage = frame_damage,
    .linux_dmabuf = frame_linux_dmabuf,
    .buffer_done = frame_buffer_done,
};

static void buffer_release(void *data, struct wl_buffer *buffer) {
    (void)data; (void)buffer; buffer_released = true;
}
static const struct wl_buffer_listener buffer_listener = { .release = buffer_release };

static struct zwlr_screencopy_frame_v1 *new_frame(struct wl_display *display) {
    constraints_done = false;
    frame_ready = false;
    frame_failed = false;
    struct zwlr_screencopy_frame_v1 *frame =
        zwlr_screencopy_manager_v1_capture_output(manager, 0, output);
    zwlr_screencopy_frame_v1_add_listener(frame, &frame_listener, NULL);
    while (!constraints_done && !frame_failed) assert(wl_display_dispatch(display) >= 0);
    assert(!frame_failed);
    return frame;
}

int main(int argc, char **argv) {
    assert(argc == 5);
    alarm(20);
    struct wl_display *display = wl_display_connect(NULL);
    assert(display);
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    assert(wl_display_roundtrip(display) >= 0);
    assert(shm && output && manager);

    struct zwlr_screencopy_frame_v1 *first = new_frame(display);
    assert(format == WL_SHM_FORMAT_ARGB8888 && width > 0 && height > 0 && stride >= width * 4);
    size_t bytes = (size_t)stride * (size_t)height;
    int fd = memfd_create("luma-legacy-screencopy", MFD_CLOEXEC);
    assert(fd >= 0 && ftruncate(fd, (off_t)bytes) == 0);
    uint8_t *pixels = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    assert(pixels != MAP_FAILED);
    memset(pixels, 0xcd, bytes);
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int32_t)bytes);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(
        pool, 0, width, height, stride, WL_SHM_FORMAT_ARGB8888);
    wl_buffer_add_listener(buffer, &buffer_listener, NULL);
    zwlr_screencopy_frame_v1_copy(first, buffer);
    while (!frame_ready && !frame_failed) assert(wl_display_dispatch(display) >= 0);
    assert(frame_ready && !frame_failed);
    while (!buffer_released) assert(wl_display_dispatch(display) >= 0);
    zwlr_screencopy_frame_v1_destroy(first);

    struct zwlr_screencopy_frame_v1 *second = new_frame(display);
    buffer_released = false;
    zwlr_screencopy_frame_v1_copy_with_damage(second, buffer);
    assert(wl_display_roundtrip(display) >= 0);
    FILE *marker = fopen(argv[2], "wb");
    assert(marker && fclose(marker) == 0);
    while (!frame_ready && !frame_failed) assert(wl_display_dispatch(display) >= 0);
    assert(frame_ready && !frame_failed);
    while (!buffer_released) assert(wl_display_dispatch(display) >= 0);
    zwlr_screencopy_frame_v1_destroy(second);

    unsigned generation = 0;
    do {
        struct zwlr_screencopy_frame_v1 *next = new_frame(display);
        buffer_released = false;
        zwlr_screencopy_frame_v1_copy_with_damage(next, buffer);
        assert(wl_display_roundtrip(display) >= 0);
        marker = fopen(argv[3], "wb");
        assert(marker);
        assert(fprintf(marker, "%u\n", ++generation) > 0);
        assert(fclose(marker) == 0);
        while (!frame_ready && !frame_failed) assert(wl_display_dispatch(display) >= 0);
        assert(frame_ready && !frame_failed);
        while (!buffer_released) assert(wl_display_dispatch(display) >= 0);
        zwlr_screencopy_frame_v1_destroy(next);
    } while (access(argv[4], F_OK) != 0);

    FILE *file = fopen(argv[1], "wb");
    assert(file);
    fprintf(file, "P6\n%d %d\n255\n", width, height);
    for (int32_t y = 0; y < height; y++) {
        for (int32_t x = 0; x < width; x++) {
            uint8_t *pixel = pixels + (size_t)y * (size_t)stride + (size_t)x * 4;
            uint8_t rgb[] = { pixel[2], pixel[1], pixel[0] };
            assert(fwrite(rgb, 1, sizeof(rgb), file) == sizeof(rgb));
        }
    }
    assert(fclose(file) == 0);
    wl_buffer_destroy(buffer);
    wl_shm_pool_destroy(pool);
    munmap(pixels, bytes);
    close(fd);
    wl_display_disconnect(display);
    return 0;
}
