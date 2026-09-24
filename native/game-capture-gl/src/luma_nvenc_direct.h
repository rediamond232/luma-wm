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
/* Receiver-only pipelining: collect blocking bitstream output separately.
 * Returns 1 on success. Submit and finish remain on the receiver GL thread. */
int luma_nvenc_direct_start_output_worker(struct luma_nvenc_direct *capture);
/* Must run on the presenting GL thread with the original context current. */
int luma_nvenc_direct_submit(struct luma_nvenc_direct *capture, uint32_t source_width,
                             uint32_t source_height, uint64_t pts_ns);
/* Async variant for latency-critical present threads (game capture): the
 * caller paces and fences a GPU framebuffer copy, then maps/submits it only
 * after a later zero-timeout fence probe says it is complete. Harvesting
 * (blocking locks), transport copies and teardown all run on a harvest thread
 * that makes no GL or window-system calls, so it can neither stall the game
 * nor wedge its teardown. Requires
 * luma_nvenc_direct_start_harvest_worker() after create; without it
 * submit_async refuses work. Pixels still never touch the CPU: only
 * compressed access units cross to the transport. */
int luma_nvenc_direct_start_harvest_worker(struct luma_nvenc_direct *capture);
int luma_nvenc_direct_submit_async(struct luma_nvenc_direct *capture,
                                   uint32_t source_width, uint32_t source_height,
                                   uint64_t pts_ns);
/* Delete the shared framebuffer objects and textures. Runs on the presenting
 * thread with the game's context current; the harvest thread never touches
 * them (it may already be gone). Call after torn_down, before destroy. */
void luma_nvenc_direct_release_gl(struct luma_nvenc_direct *capture);
/* True once the encode worker finished teardown and exited. The owner then
 * collects the shell with luma_nvenc_direct_destroy() (which is safe: every
 * joined thread is already gone) and clears its pointer. */
int luma_nvenc_direct_torn_down(struct luma_nvenc_direct *capture);
/* Process teardown (or dlclose) is imminent: hurry any live session toward
 * its fast path so exit-time driver cleanup cannot wedge on our worker
 * context. Never blocks; the worker observes it on its next slice. */
void luma_nvenc_direct_notify_unload(struct luma_nvenc_direct *capture);
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
/* Release the shell after stop_and_join (or a fully finished capture) so a
 * re-armed session can drop the previous object. Only frees the shell: GL
 * resources must already be released by finish_on_gl_thread. */
void luma_nvenc_direct_destroy(struct luma_nvenc_direct *capture);

#ifdef __cplusplus
}
#endif
