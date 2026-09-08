#define _GNU_SOURCE
#include <assert.h>
#include <fcntl.h>
#include <gbm.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include <wayland-client.h>
#include <xf86drm.h>
#include <drm_fourcc.h>

#include "capture-source.h"
#include "copy-capture.h"
#include "linux-dmabuf.h"

static struct wl_output *output;
static struct ext_output_image_capture_source_manager_v1 *sources;
static struct ext_image_copy_capture_manager_v1 *capture_manager;
static struct zwp_linux_dmabuf_v1 *linux_dmabuf;
static uint32_t width, height;
static dev_t capture_device;
static bool have_device, constraints_done, frame_done, frame_failed;
static uint32_t selected_format;
static uint64_t selected_modifier;

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
    if (!strcmp(interface, wl_output_interface.name) && !output) {
        output = wl_registry_bind(registry, name, &wl_output_interface, version < 2 ? version : 2);
        wl_output_add_listener(output, &output_listener, NULL);
    } else if (!strcmp(interface, ext_output_image_capture_source_manager_v1_interface.name)) {
        sources = wl_registry_bind(registry, name,
            &ext_output_image_capture_source_manager_v1_interface, 1);
    } else if (!strcmp(interface, ext_image_copy_capture_manager_v1_interface.name)) {
        capture_manager = wl_registry_bind(registry, name,
            &ext_image_copy_capture_manager_v1_interface, 1);
    } else if (!strcmp(interface, zwp_linux_dmabuf_v1_interface.name)) {
        linux_dmabuf = wl_registry_bind(registry, name, &zwp_linux_dmabuf_v1_interface,
                                        version < 4 ? version : 4);
    }
}
static void registry_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_remove,
};

static void session_buffer_size(void *data, struct ext_image_copy_capture_session_v1 *session,
                                uint32_t w, uint32_t h) {
    (void)data; (void)session; width = w; height = h;
}
static void session_shm_format(void *data, struct ext_image_copy_capture_session_v1 *session,
                               uint32_t format) {
    (void)data; (void)session; (void)format;
}
static void session_dmabuf_device(void *data, struct ext_image_copy_capture_session_v1 *session,
                                  struct wl_array *device) {
    (void)data; (void)session;
    assert(device->size == sizeof(capture_device));
    memcpy(&capture_device, device->data, sizeof(capture_device));
    have_device = true;
}
static void session_dmabuf_format(void *data, struct ext_image_copy_capture_session_v1 *session,
                                  uint32_t format, struct wl_array *modifiers) {
    (void)data; (void)session;
    if (format != DRM_FORMAT_ARGB8888 && format != DRM_FORMAT_XRGB8888) {
        return;
    }
    uint64_t *modifier;
    wl_array_for_each(modifier, modifiers) {
        if (!selected_format) {
            selected_format = format;
            selected_modifier = *modifier;
        }
        if (*modifier == DRM_FORMAT_MOD_LINEAR) {
            selected_format = format;
            selected_modifier = *modifier;
            return;
        }
    }
}
static void session_done(void *data, struct ext_image_copy_capture_session_v1 *session) {
    (void)data; (void)session; constraints_done = true;
}
static void session_stopped(void *data, struct ext_image_copy_capture_session_v1 *session) {
    (void)data; (void)session; frame_failed = true;
}
static const struct ext_image_copy_capture_session_v1_listener session_listener = {
    .buffer_size = session_buffer_size,
    .shm_format = session_shm_format,
    .dmabuf_device = session_dmabuf_device,
    .dmabuf_format = session_dmabuf_format,
    .done = session_done,
    .stopped = session_stopped,
};

static void frame_transform(void *data, struct ext_image_copy_capture_frame_v1 *frame,
                            uint32_t transform) {
    (void)data; (void)frame; assert(transform == WL_OUTPUT_TRANSFORM_NORMAL);
}
static void frame_damage(void *data, struct ext_image_copy_capture_frame_v1 *frame,
                         int32_t x, int32_t y, int32_t w, int32_t h) {
    (void)data; (void)frame; (void)x; (void)y; (void)w; (void)h;
}
static void frame_presentation(void *data, struct ext_image_copy_capture_frame_v1 *frame,
                               uint32_t hi, uint32_t lo, uint32_t ns) {
    (void)data; (void)frame; (void)hi; (void)lo; assert(ns < 1000000000u);
}
static void frame_ready(void *data, struct ext_image_copy_capture_frame_v1 *frame) {
    (void)data; (void)frame; frame_done = true;
}
static void frame_failed_event(void *data, struct ext_image_copy_capture_frame_v1 *frame,
                               uint32_t reason) {
    (void)data; (void)frame; (void)reason; frame_failed = true;
}
static const struct ext_image_copy_capture_frame_v1_listener frame_listener = {
    .transform = frame_transform,
    .damage = frame_damage,
    .presentation_time = frame_presentation,
    .ready = frame_ready,
    .failed = frame_failed_event,
};

static int open_render_node(dev_t device) {
    char path[64];
    struct stat status;
    for (int minor = 128; minor < 256; minor++) {
        snprintf(path, sizeof(path), "/dev/dri/renderD%d", minor);
        if (stat(path, &status) == 0 && status.st_rdev == device) {
            return open(path, O_RDWR | O_CLOEXEC);
        }
    }
    return -1;
}

int main(int argc, char **argv) {
    assert(argc == 2);
    alarm(20);
    struct wl_display *display = wl_display_connect(NULL);
    assert(display);
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    assert(wl_display_roundtrip(display) >= 0);
    assert(output && sources && capture_manager && linux_dmabuf);

    struct ext_image_capture_source_v1 *source =
        ext_output_image_capture_source_manager_v1_create_source(sources, output);
    struct ext_image_copy_capture_session_v1 *session =
        ext_image_copy_capture_manager_v1_create_session(capture_manager, source, 0);
    ext_image_copy_capture_session_v1_add_listener(session, &session_listener, NULL);
    while (!constraints_done && !frame_failed) assert(wl_display_dispatch(display) >= 0);
    assert(!frame_failed && width > 0 && height > 0 && have_device && selected_format);

    int node_fd = open_render_node(capture_device);
    assert(node_fd >= 0);
    struct gbm_device *gbm = gbm_create_device(node_fd);
    assert(gbm);
    uint64_t modifiers[] = { selected_modifier };
    uint32_t usage = GBM_BO_USE_RENDERING;
    if (selected_modifier == DRM_FORMAT_MOD_LINEAR) usage |= GBM_BO_USE_LINEAR;
    struct gbm_bo *bo = gbm_bo_create_with_modifiers2(
        gbm, width, height, selected_format, modifiers, 1, usage);
    assert(bo);

    struct zwp_linux_buffer_params_v1 *params = zwp_linux_dmabuf_v1_create_params(linux_dmabuf);
    int planes = gbm_bo_get_plane_count(bo);
    assert(planes > 0 && planes <= 4);
    uint64_t modifier = gbm_bo_get_modifier(bo);
    for (int plane = 0; plane < planes; plane++) {
        int fd = gbm_bo_get_fd_for_plane(bo, plane);
        assert(fd >= 0);
        zwp_linux_buffer_params_v1_add(params, fd, (uint32_t)plane,
            gbm_bo_get_offset(bo, plane), gbm_bo_get_stride_for_plane(bo, plane),
            (uint32_t)(modifier >> 32), (uint32_t)modifier);
        close(fd);
    }
    struct wl_buffer *buffer = zwp_linux_buffer_params_v1_create_immed(
        params, (int32_t)width, (int32_t)height, selected_format, 0);
    assert(buffer);
    zwp_linux_buffer_params_v1_destroy(params);

    struct ext_image_copy_capture_frame_v1 *frame =
        ext_image_copy_capture_session_v1_create_frame(session);
    ext_image_copy_capture_frame_v1_add_listener(frame, &frame_listener, NULL);
    ext_image_copy_capture_frame_v1_attach_buffer(frame, buffer);
    ext_image_copy_capture_frame_v1_damage_buffer(frame, 0, 0, (int32_t)width, (int32_t)height);
    ext_image_copy_capture_frame_v1_capture(frame);
    while (!frame_done && !frame_failed) assert(wl_display_dispatch(display) >= 0);
    assert(frame_done && !frame_failed);

    FILE *file = fopen(argv[1], "wb");
    assert(file);
    fprintf(file, "%u %u %u %llu\n", width, height, selected_format,
            (unsigned long long)modifier);
    assert(fclose(file) == 0);
    gbm_bo_destroy(bo);
    gbm_device_destroy(gbm);
    close(node_fd);
    wl_display_disconnect(display);
    return 0;
}
