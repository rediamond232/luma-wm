// Game-side implementation of the capture lifecycle. No encoder is linked
// into this library: only GPU copies and a bounded shared-VRAM handoff live
// here. The opaque lifecycle ABI is retained for the existing C hook.
#define GL_GLEXT_PROTOTYPES
#include "luma_nvenc_direct.h"
#include "../../vulkan-external-capture-prototype/luma_vk_export_protocol.h"
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <array>
#include <atomic>
#include <cerrno>
#include <cstddef>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <time.h>
#include "luma_gl_shared_memory.h"
#include "luma_gl_vram.h"
#include "luma_capture_pacing.h"

extern "C" void luma_game_capture_debug_message(const char *);
namespace {
constexpr size_t kSlots = 4;
uint64_t now_ns() {
    timespec t{}; clock_gettime(CLOCK_MONOTONIC, &t);
    return uint64_t(t.tv_sec) * 1000000000 + t.tv_nsec;
}
struct Slot {
    VkSemaphore vk_semaphore{};
    GLuint semaphore{};
    int semaphore_fd{-1};
    bool needs_acquire{};
    VkImage vk_image{};
    VkDeviceMemory vk_memory{};
    GLuint gl_memory{};
    uint64_t allocation_size{};
    GLuint texture{}, fbo{};
    int fd{-1};
    uint64_t generation{}, pts{};
    GLsync fence{};
    bool sent{};
};
}
struct luma_nvenc_direct {
    LumaVram vram;
    LumaGlMemory gl_memory;
    unsigned char uuid[16]{};
    std::array<Slot, kSlots> slots{};
    int socket_fd{-1};
    uint32_t width{}, height{};
    uint64_t interval{}, due{}, stream{}, sequence{}, copied{}, dropped{}, probe_due{};
    uint64_t copy_ns{}, max_copy_ns{};
    std::atomic<bool> stopped{false};
};

namespace {
void cleanup(luma_nvenc_direct *s) {
    char message[240];
    snprintf(message, sizeof(message), "luma-game-capture: GPU export copied=%llu full_pool_drops=%llu copy_submit_avg_us=%llu copy_submit_max_us=%llu\n",
             static_cast<unsigned long long>(s->copied), static_cast<unsigned long long>(s->dropped),
             static_cast<unsigned long long>(s->copied ? s->copy_ns / s->copied / 1000 : 0),
             static_cast<unsigned long long>(s->max_copy_ns / 1000));
    luma_game_capture_debug_message(message);
    for (auto &slot : s->slots) {
        if (slot.fence) glDeleteSync(slot.fence);
        if (slot.fbo) glDeleteFramebuffers(1, &slot.fbo);
        if (slot.texture) glDeleteTextures(1, &slot.texture);
        if (slot.gl_memory) s->gl_memory.destroy(1, &slot.gl_memory);
        if (slot.semaphore) s->gl_memory.delete_semaphore(1, &slot.semaphore);
        if (slot.semaphore_fd >= 0) close(slot.semaphore_fd);
        if (slot.vk_semaphore) vkDestroySemaphore(s->vram.device, slot.vk_semaphore, nullptr);
        if (slot.fd >= 0) close(slot.fd);
        if (slot.vk_image) vkDestroyImage(s->vram.device, slot.vk_image, nullptr);
        if (slot.vk_memory) vkFreeMemory(s->vram.device, slot.vk_memory, nullptr);
        slot = {};
    }
    if (s->socket_fd >= 0) close(s->socket_fd);
    s->socket_fd = -1;
}
void pump(luma_nvenc_direct *s) {
    for (size_t n = 0; n < kSlots; ++n) {
        luma_vk_ack_message ack{};
        const auto received = recv(s->socket_fd, &ack, sizeof(ack), MSG_DONTWAIT | MSG_TRUNC);
        if (received < 0) break;
        if (received == sizeof(ack) && ack.magic == LUMA_VK_ACK_MAGIC &&
            ack.version == LUMA_VK_PROTOCOL_VERSION && ack.size == sizeof(ack) && ack.slot < kSlots) {
            auto &slot = s->slots[ack.slot];
            if (slot.sent && slot.generation == ack.generation) slot.sent = false;
        }
    }
    for (size_t n = 0; n < kSlots; ++n) {
        // Send in capture order even when ACKs free slots out of order.
        size_t i = kSlots;
        for (size_t j = 0; j < kSlots; ++j)
            if (s->slots[j].fence && (i == kSlots || s->slots[j].generation < s->slots[i].generation)) i = j;
        if (i == kSlots) break;
        auto &slot = s->slots[i];
        GLenum result = glClientWaitSync(slot.fence, 0, 0);
        if (result == GL_TIMEOUT_EXPIRED) break;
        if (result != GL_ALREADY_SIGNALED && result != GL_CONDITION_SATISFIED) {
            s->stopped.store(true); return;
        }
        // Completion was observed without waiting; the receiver needs no
        // imported native fence. Pixels remain entirely GPU resident.
        luma_vk_export_message frame{};
        frame.magic = LUMA_VK_EXPORT_MAGIC; frame.version = LUMA_VK_PROTOCOL_VERSION;
        frame.size = sizeof(frame); frame.generation = slot.generation;
        frame.pts_ns = slot.pts; frame.stream_id = s->stream; frame.slot = uint32_t(i);
        frame.width = s->width; frame.height = s->height;
        frame.reserved = LUMA_EXPORT_COPY_COMPLETE | LUMA_EXPORT_FLIP_Y | LUMA_EXPORT_OPAQUE_MEMORY;
        frame.allocation_size = slot.allocation_size; memcpy(frame.device_uuid, s->uuid, 16);
        alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int) * 2)]{};
        iovec io{&frame, sizeof(frame)}; msghdr msg{};
        msg.msg_iov = &io; msg.msg_iovlen = 1; msg.msg_control = control; msg.msg_controllen = sizeof(control);
        cmsghdr *c = CMSG_FIRSTHDR(&msg);
        c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS; c->cmsg_len = CMSG_LEN(sizeof(int) * 2);
        const int fds[] = {slot.fd, slot.semaphore_fd};
        memcpy(CMSG_DATA(c), fds, sizeof(fds));
        slot.sent = sendmsg(s->socket_fd, &msg, MSG_DONTWAIT | MSG_NOSIGNAL) == sizeof(frame);
        if (!slot.sent && (errno == ECONNREFUSED || errno == ENOENT || errno == EPIPE)) s->stopped.store(true);
        glDeleteSync(slot.fence); slot.fence = nullptr;
    }
}
}

extern "C" luma_nvenc_direct *luma_nvenc_direct_create(uint32_t w, uint32_t h, uint32_t fps,
                                                       uint32_t, const char *path, const char *) {
    if (!w || !h || (w & 1) || (h & 1) || w > 16384 || h > 16384 || !fps || fps > 480 ||
        !path || strlen(path) >= sizeof(sockaddr_un::sun_path)) return nullptr;
    auto *s = new luma_nvenc_direct;
    s->width = w; s->height = h; s->interval = 1000000000 / fps; s->stream = now_ns();
    bool good = s->gl_memory.load();
    if (good) { s->gl_memory.uuid(GL_DEVICE_UUID_EXT, s->uuid); good = s->vram.initialize(s->uuid); }
    GLint texture{}, read{}, draw{};
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &texture);
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read);
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw);
    for (auto &slot : s->slots) if (good) {
        good = s->vram.allocate(w, h, slot.vk_image, slot.vk_memory, slot.allocation_size, slot.fd) &&
            s->gl_memory.texture(slot.fd, slot.allocation_size, w, h, slot.gl_memory, slot.texture) &&
            s->vram.semaphore(slot.vk_semaphore, slot.semaphore_fd) &&
            s->gl_memory.semaphore(slot.semaphore_fd, slot.semaphore);
        if (good) {
            glGenFramebuffers(1, &slot.fbo); glBindFramebuffer(GL_DRAW_FRAMEBUFFER, slot.fbo);
            glFramebufferTexture2D(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, slot.texture, 0);
            good = glCheckFramebufferStatus(GL_DRAW_FRAMEBUFFER) == GL_FRAMEBUFFER_COMPLETE;
        }
    }
    glBindTexture(GL_TEXTURE_2D, GLuint(texture));
    glBindFramebuffer(GL_READ_FRAMEBUFFER, GLuint(read));
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, GLuint(draw));
    if (good) {
        s->socket_fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        sockaddr_un local{}, remote{}; local.sun_family = remote.sun_family = AF_UNIX;
        snprintf(local.sun_path + 1, sizeof(local.sun_path) - 1, "luma-gl-%d-%llu", getpid(),
                 static_cast<unsigned long long>(s->stream));
        strcpy(remote.sun_path, path);
        good = s->socket_fd >= 0 && bind(s->socket_fd, reinterpret_cast<sockaddr *>(&local),
            offsetof(sockaddr_un, sun_path) + 1 + strlen(local.sun_path + 1)) == 0 &&
            connect(s->socket_fd, reinterpret_cast<sockaddr *>(&remote), sizeof(remote)) == 0;
    }
    if (!good) {
        luma_game_capture_debug_message("luma-game-capture: GPU export setup failed; no in-game encoder fallback\n");
        cleanup(s); delete s; return nullptr;
    }
    luma_game_capture_debug_message("luma-game-capture: shared VRAM pool ready; NVENC runs in recorder process\n");
    return s;
}
extern "C" int luma_nvenc_direct_start_harvest_worker(luma_nvenc_direct *) { return 0; }
extern "C" int luma_nvenc_direct_submit_async(luma_nvenc_direct *s, uint32_t w, uint32_t h, uint64_t pts) {
    if (!s || s->stopped.load()) return 0;
    if (w != s->width || h != s->height) { s->stopped.store(true); return 0; }
    pump(s);
    if (s->stopped.load() || !luma_capture_due(pts, s->interval, &s->due)) return 0;
    Slot *slot = nullptr;
    for (auto &candidate : s->slots) if (!candidate.fence && !candidate.sent) { slot = &candidate; break; }
    if (!slot) {
        ++s->dropped;
        // A stopped receiver may leave every slot awaiting ACK. Probe only
        // once a second, nonblocking, so launch-time capture retires too.
        if (pts >= s->probe_due) {
            s->probe_due = pts + 1000000000;
            if (send(s->socket_fd, "", 0, MSG_DONTWAIT | MSG_NOSIGNAL) < 0 &&
                (errno == ECONNREFUSED || errno == ENOENT || errno == EPIPE)) s->stopped.store(true);
        }
        return 0;
    }
    const uint64_t copy_start = now_ns();
    if (slot->needs_acquire) s->gl_memory.acquire(slot->semaphore, slot->texture);
    GLint read{}, draw{}, buffer{}, default_buffer{};
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &read);
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &draw);
    glGetIntegerv(GL_READ_BUFFER, &buffer);
    GLboolean scissor = glIsEnabled(GL_SCISSOR_TEST);
    GLboolean srgb = glIsEnabled(GL_FRAMEBUFFER_SRGB);
    glDisable(GL_SCISSOR_TEST); glDisable(GL_FRAMEBUFFER_SRGB);
    glBindFramebuffer(GL_READ_FRAMEBUFFER, 0);
    glGetIntegerv(GL_READ_BUFFER, &default_buffer); glReadBuffer(GL_BACK);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, slot->fbo);
    glBlitFramebuffer(0, 0, w, h, 0, 0, w, h, GL_COLOR_BUFFER_BIT, GL_NEAREST);
    glBindFramebuffer(GL_DRAW_FRAMEBUFFER, GLuint(draw));
    glReadBuffer(GLenum(default_buffer));
    glBindFramebuffer(GL_READ_FRAMEBUFFER, GLuint(read)); glReadBuffer(GLenum(buffer));
    if (scissor) glEnable(GL_SCISSOR_TEST);
    if (srgb) glEnable(GL_FRAMEBUFFER_SRGB);
    s->gl_memory.release(slot->semaphore, slot->texture);
    slot->needs_acquire = true;
    slot->fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    if (!slot->fence) { s->stopped.store(true); return 0; }
    slot->generation = ++s->sequence; slot->pts = pts; ++s->copied;
    glFlush();
    const uint64_t elapsed = now_ns() - copy_start;
    s->copy_ns += elapsed;
    if (elapsed > s->max_copy_ns) s->max_copy_ns = elapsed;
    return 1;
}
extern "C" void luma_nvenc_direct_request_stop(luma_nvenc_direct *s) { if (s) s->stopped.store(true); }
extern "C" int luma_nvenc_direct_stop_requested(luma_nvenc_direct *s) { return s && s->stopped.load(); }
extern "C" int luma_nvenc_direct_torn_down(luma_nvenc_direct *s) { return s && s->stopped.load(); }
extern "C" void luma_nvenc_direct_notify_unload(luma_nvenc_direct *s) { if (s) s->stopped.store(true); }
extern "C" void luma_nvenc_direct_stop_and_join(luma_nvenc_direct *s) { if (s) s->stopped.store(true); }
extern "C" void luma_nvenc_direct_release_gl(luma_nvenc_direct *s) { if (s) cleanup(s); }
extern "C" void luma_nvenc_direct_destroy(luma_nvenc_direct *s) { delete s; }
