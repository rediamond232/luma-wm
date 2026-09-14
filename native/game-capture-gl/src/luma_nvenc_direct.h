#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque, OpenGL-context-bound direct NVENC capture state. */
struct luma_nvenc_direct;

/* Returns NULL unless a GPU-only encoder and authenticated stream are ready. */
struct luma_nvenc_direct *luma_nvenc_direct_create(uint32_t width, uint32_t height,
                                                     uint32_t fps, uint32_t quality,
                                                     const char *socket_path,
                                                     const char *token_hex);
/* Recorder-side consumers may wait for the authenticated transport before
 * releasing a launch gate. Never call this from an application's present thread. */
int luma_nvenc_direct_wait_ready(struct luma_nvenc_direct *capture, uint32_t timeout_ms);
/* Must run on the presenting GL thread with the original context current. */
int luma_nvenc_direct_submit(struct luma_nvenc_direct *capture, uint32_t source_width,
                             uint32_t source_height, uint64_t pts_ns);
/* Submit from a complete OpenGL read framebuffer owned by a recorder-side GPU
 * importer. This enables Vulkan DMA-BUF capture without CPU pixel readback. */
int luma_nvenc_direct_submit_framebuffer(struct luma_nvenc_direct *capture,
                                         uint32_t source_framebuffer,
                                         uint32_t source_width, uint32_t source_height,
                                         int flip_y, uint64_t pts_ns);
/* Safe from a JVM attach/shutdown thread. The presenting GL thread observes
 * this request and releases all context-bound encoder resources. */
void luma_nvenc_direct_request_stop(struct luma_nvenc_direct *capture);
int luma_nvenc_direct_stop_requested(struct luma_nvenc_direct *capture);
/* Must run on the presenting GL thread with the original context current. */
void luma_nvenc_direct_finish_on_gl_thread(struct luma_nvenc_direct *capture);
/* Attach-agent shutdown waits for the transport thread before HotSpot may
 * unload this shared object. It never destroys context-bound GL objects. */
void luma_nvenc_direct_stop_and_join(struct luma_nvenc_direct *capture);

#ifdef __cplusplus
}
#endif
