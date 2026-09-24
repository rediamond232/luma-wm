#include "luma_nvenc_direct.h"

#include <GL/gl.h>
#include <EGL/egl.h>
#include <ffnvcodec/nvEncodeAPI.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <pthread.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <csignal>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstdint>
#include <cstring>
#include <ctime>
#include <mutex>
#include <new>
#include <string>
#include <thread>
#include <vector>

namespace {
extern "C" void luma_game_capture_debug_message(const char *message) __attribute__((weak));

constexpr uint32_t kMagic = 0x4c474350; // LGCP, integer fields use network/big endian.
constexpr uint16_t kVersion = 1;
constexpr uint16_t kHello = 1, kServerHello = 2, kStart = 3, kAccessUnit = 4;
constexpr size_t kMaxAu = 64U * 1024U * 1024U;
constexpr size_t kQueueMax = 16;
constexpr size_t kAccessUnitHeaderBytes = 28;
// Reserve a fixed amount of RAM for compressed-AU handoff instead of growing a
// container on either the game hook or receiver collector path.
// An AU larger than this deliberately drops: allowing it to allocate or wait
// would turn a transient IDR spike into a game-frame hitch.  The wire protocol
// still accepts up to kMaxAu; that remains the receiver's validation limit.
constexpr size_t kPacketSlotBytes = 8U * 1024U * 1024U;
constexpr size_t kSlots = 12;
constexpr size_t kCollectorLead = kSlots - 1;
constexpr uint64_t kSlowSubmitNs = UINT64_C(750000);
constexpr uint64_t kMaxBackoffNs = UINT64_C(1000000000) / 30;

bool debug_enabled() {
    static const bool enabled = [] {
        const char *value = getenv("LUMA_GAME_CAPTURE_DEBUG");
        return value && value[0] != '\0' && strcmp(value, "0") != 0;
    }();
    return enabled;
}

void debug_log(const char *format, ...) {
    if (!debug_enabled() && luma_game_capture_debug_message == nullptr) return;
    char message[512];
    va_list args;
    va_start(args, format);
    const int prefix = snprintf(message, sizeof(message), "luma game capture: ");
    if (prefix > 0 && static_cast<size_t>(prefix) < sizeof(message)) {
        (void)vsnprintf(message + prefix, sizeof(message) - static_cast<size_t>(prefix),
                        format, args);
    }
    va_end(args);
    const size_t length = strnlen(message, sizeof(message));
    if (length + 1 < sizeof(message)) {
        message[length] = '\n';
        message[length + 1] = '\0';
    }
    if (luma_game_capture_debug_message != nullptr) {
        luma_game_capture_debug_message(message);
    } else {
        fputs(message, stderr);
    }
}

bool bench_enabled() {
    static const bool enabled = [] {
        const char *value = getenv("LUMA_GAME_CAPTURE_BENCH");
        return value && value[0] != '\0' && strcmp(value, "0") != 0;
    }();
    return enabled;
}

uint64_t bench_ns() {
    timespec value{};
    clock_gettime(CLOCK_MONOTONIC, &value);
    return static_cast<uint64_t>(value.tv_sec) * UINT64_C(1000000000) +
           static_cast<uint64_t>(value.tv_nsec);
}

// Present-thread stage costs, accumulated only when LUMA_GAME_CAPTURE_BENCH
// is set. Dumped to the debug log every 512 submitted frames.
struct BenchTotals {
    std::atomic<uint64_t> pace{0};
    std::atomic<uint64_t> poll{0};
    std::atomic<uint64_t> copy{0};
    std::atomic<uint64_t> map_encode{0};
    std::atomic<uint64_t> enqueue{0};
    std::atomic<uint64_t> frames{0};
};

BenchTotals &bench() {
    static BenchTotals totals;
    return totals;
}

void bench_dump() {
    BenchTotals &totals = bench();
    const uint64_t frames =
        totals.frames.exchange(0, std::memory_order_acq_rel);
    if (frames == 0) return;
    const uint64_t pace = totals.pace.exchange(0, std::memory_order_acq_rel);
    const uint64_t poll = totals.poll.exchange(0, std::memory_order_acq_rel);
    const uint64_t copy = totals.copy.exchange(0, std::memory_order_acq_rel);
    const uint64_t map_encode =
        totals.map_encode.exchange(0, std::memory_order_acq_rel);
    const uint64_t enqueue =
        totals.enqueue.exchange(0, std::memory_order_acq_rel);
    // Direct stderr write: bench output must not depend on debug logging.
    fprintf(stderr,
            "luma game capture: bench submit avg us over %llu frames: "
            "pace=%.1f poll=%.1f copy=%.1f map_encode=%.1f enqueue=%.1f\n",
            (unsigned long long)frames, pace / 1000.0 / frames,
            poll / 1000.0 / frames, copy / 1000.0 / frames,
            map_encode / 1000.0 / frames, enqueue / 1000.0 / frames);
}

uint32_t capture_qp(uint32_t requested) {
    constexpr uint32_t kDefaultQp = 20;
    return requested >= 1 && requested <= 51 ? requested : kDefaultQp;
}

uint16_t be16(uint16_t n) { return __builtin_bswap16(n); }
uint32_t be32(uint32_t n) { return __builtin_bswap32(n); }
uint64_t be64(uint64_t n) { return __builtin_bswap64(n); }

bool write_all(int fd, const uint8_t *data, size_t length) {
    // MSG_NOSIGNAL: a dead muxer/ffmpeg must fail these writes with EPIPE,
    // never SIGPIPE the game. The callers already treat a short write as a
    // graceful transport failure that disables capture and leaves the game
    // running.
    while (length) {
        const ssize_t wrote = send(fd, data, length, MSG_NOSIGNAL);
        if (wrote <= 0) return false;
        data += wrote;
        length -= static_cast<size_t>(wrote);
    }
    return true;
}
bool read_all(int fd, uint8_t *data, size_t length) {
    while (length) {
        const ssize_t got = read(fd, data, length);
        if (got <= 0) return false;
        data += got;
        length -= static_cast<size_t>(got);
    }
    return true;
}
bool write_message(int fd, uint32_t &sequence, uint16_t kind, const uint8_t *payload, uint32_t length) {
    uint8_t header[16];
    const uint32_t magic = be32(kMagic), payload_len = be32(length), seq = be32(sequence++);
    const uint16_t version = be16(kVersion), message_kind = be16(kind);
    memcpy(header, &magic, 4); memcpy(header + 4, &version, 2); memcpy(header + 6, &message_kind, 2);
    memcpy(header + 8, &payload_len, 4); memcpy(header + 12, &seq, 4);
    return write_all(fd, header, sizeof(header)) && (!length || write_all(fd, payload, length));
}
bool read_server_hello(int fd) {
    uint8_t header[16];
    if (!read_all(fd, header, sizeof(header))) return false;
    uint32_t magic; uint16_t version, kind; uint32_t length, sequence;
    memcpy(&magic, header, 4); memcpy(&version, header + 4, 2); memcpy(&kind, header + 6, 2);
    memcpy(&length, header + 8, 4); memcpy(&sequence, header + 12, 4);
    (void)sequence;
    magic = be32(magic); version = be16(version); kind = be16(kind); length = be32(length);
    if (magic != kMagic || version != kVersion || kind != kServerHello || length != 8) return false;
    uint8_t session[8];
    return read_all(fd, session, sizeof(session));
}
bool parse_token(const char *text, std::array<uint8_t, 32> &out) {
    if (!text || strlen(text) != 64) return false;
    for (size_t i = 0; i < out.size(); ++i) {
        const char high = text[i * 2], low = text[i * 2 + 1];
        auto hex = [](char c) -> int { if (c >= '0' && c <= '9') return c - '0'; if (c >= 'a' && c <= 'f') return c - 'a' + 10; if (c >= 'A' && c <= 'F') return c - 'A' + 10; return -1; };
        const int a = hex(high), b = hex(low);
        if (a < 0 || b < 0) return false;
        out[i] = static_cast<uint8_t>((a << 4) | b);
    }
    return true;
}

struct PacketSlot {
    // Filled before Transport::ready is published, by the transport worker.
    // enqueue() never calls a vector method, so it cannot allocate.
    std::vector<uint8_t> bytes;
    uint32_t length{};
    bool in_use{};
};

struct Transport {
    std::mutex mutex;
    std::condition_variable wake;
    std::array<PacketSlot, kQueueMax> slots;
    std::array<size_t, kQueueMax> ready_slots{};
    size_t ready_head{};
    size_t ready_count{};
    std::atomic<bool> ready{false};
    std::atomic<bool> failed{false};
    std::atomic<bool> stop{false};
    std::thread worker;
    uint32_t width, height, fps;
    std::array<uint8_t, 32> token;
    std::string path;

    bool allocate_packet_pool() {
        try {
            for (PacketSlot &slot : slots) {
                slot.bytes.resize(kAccessUnitHeaderBytes + kPacketSlotBytes);
            }
        } catch (const std::bad_alloc &) {
            debug_log("could not allocate %zu-byte direct capture packet pool", kQueueMax * kPacketSlotBytes);
            return false;
        }
        return true;
    }

    bool enqueue(const uint8_t *bytes, uint32_t length, uint64_t pts, bool keyframe) {
        if (!ready.load(std::memory_order_acquire) || length == 0 || length > kMaxAu || length > kPacketSlotBytes) return false;
        /* The worker never holds this mutex during socket I/O. Once NVENC has
         * encoded a reference frame it must reach the stream; dropping it here
         * would corrupt every dependent P frame. Admission is handled before
         * encode, so this lock only serializes bounded queue bookkeeping. */
        std::lock_guard<std::mutex> lock(mutex);
        if (ready_count == kQueueMax) return false;
        PacketSlot *slot = nullptr;
        for (PacketSlot &candidate : slots) {
            if (!candidate.in_use) {
                slot = &candidate;
                break;
            }
        }
        // The queue and slot lifecycle are both protected by mutex.  This is
        // defensive against future worker changes: an occupied slot must
        // always correspond to a queued or actively-written AU.
        if (slot == nullptr) return false;
        // Keep the protocol's fixed header in the same preallocated slot as
        // the compressed AU. The transport worker can write it directly to
        // the socket without allocating or rebuilding a second payload.
        const uint64_t timestamp = be64(pts);
        const uint64_t duration = be64(UINT64_C(1000000000) / fps);
        const uint32_t flags = be32(keyframe ? 1U : 0U);
        memcpy(slot->bytes.data(), &timestamp, 8);
        memcpy(slot->bytes.data() + 8, &timestamp, 8);
        memcpy(slot->bytes.data() + 16, &duration, 8);
        memcpy(slot->bytes.data() + 24, &flags, 4);
        memcpy(slot->bytes.data() + kAccessUnitHeaderBytes, bytes, length);
        slot->length = static_cast<uint32_t>(kAccessUnitHeaderBytes + length);
        slot->in_use = true;
        ready_slots[(ready_head + ready_count) % kQueueMax] = static_cast<size_t>(slot - slots.data());
        ++ready_count;
        wake.notify_one();
        return true;
    }

    bool can_admit(size_t encoder_in_flight) {
        std::lock_guard<std::mutex> lock(mutex);
        size_t transport_in_flight = 0;
        for (const PacketSlot &slot : slots) {
            transport_in_flight += slot.in_use ? 1U : 0U;
        }
        /* ready_count excludes the slot the worker is currently writing. Count
         * slot ownership instead, otherwise one AU can pass admission while all
         * packet storage is occupied and be discarded after it was encoded. */
        return ready.load(std::memory_order_acquire) &&
               transport_in_flight + encoder_in_flight < kQueueMax;
    }
};
int connect_stream(const std::string &path) {
    if (path.empty() || path.size() >= sizeof(sockaddr_un::sun_path)) return -1;
    const int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    sockaddr_un address{}; address.sun_family = AF_UNIX; memcpy(address.sun_path, path.c_str(), path.size() + 1);
    if (connect(fd, reinterpret_cast<sockaddr *>(&address), sizeof(address)) != 0) { close(fd); return -1; }
    return fd;
}
void transport_main(Transport *transport) {
    (void)pthread_setname_np(pthread_self(), "luma-cap-tx");
    // A dead muxer or ffmpeg must never take the game down with it. Every
    // socket write here uses MSG_NOSIGNAL, and ignoring SIGPIPE process-wide
    // covers any other library write in this process the same way robust
    // applications (OBS included) already do.
    signal(SIGPIPE, SIG_IGN);
    const int fd = connect_stream(transport->path);
    if (fd < 0) { transport->failed.store(true, std::memory_order_release); return; }
    uint32_t sequence = 0;
    uint8_t hello[40]; memcpy(hello, transport->token.data(), 32);
    const uint32_t pid = be32(static_cast<uint32_t>(getpid())), caps = be32(1); // bit 0 = H.264 Annex-B
    memcpy(hello + 32, &pid, 4); memcpy(hello + 36, &caps, 4);
    if (!write_message(fd, sequence, kHello, hello, sizeof(hello)) || !read_server_hello(fd)) { transport->failed.store(true, std::memory_order_release); close(fd); return; }
    uint8_t start[17] = {1};
    const uint32_t values[] = {be32(transport->width), be32(transport->height), be32(transport->fps), be32(1)};
    memcpy(start + 1, values, sizeof(values));
    if (!write_message(fd, sequence, kStart, start, sizeof(start))) { transport->failed.store(true, std::memory_order_release); close(fd); return; }
    // Do this in the worker before allowing the NVENC/present thread to
    // enqueue.  It is intentionally a bounded pool: enqueue only copies into
    // already-owned storage and never grows a vector or deque.
    if (!transport->allocate_packet_pool()) {
        transport->failed.store(true, std::memory_order_release);
        close(fd);
        return;
    }
    transport->ready.store(true, std::memory_order_release);
    for (;;) {
        size_t slot_index = 0;
        {
            std::unique_lock<std::mutex> lock(transport->mutex);
            transport->wake.wait(lock, [&] {
                return transport->ready_count != 0 ||
                       transport->stop.load(std::memory_order_acquire);
            });
            if (transport->ready_count == 0 &&
                transport->stop.load(std::memory_order_acquire)) {
                break;
            }
            slot_index = transport->ready_slots[transport->ready_head];
            transport->ready_head = (transport->ready_head + 1) % kQueueMax;
            --transport->ready_count;
        }
        PacketSlot &packet = transport->slots[slot_index];
        const bool written = write_message(fd, sequence, kAccessUnit, packet.bytes.data(), packet.length);
        {
            std::lock_guard<std::mutex> lock(transport->mutex);
            packet.length = 0;
            packet.in_use = false;
        }
        if (!written) break;
    }
    transport->ready.store(false, std::memory_order_release);
    transport->failed.store(true, std::memory_order_release);
    close(fd);
}

using CreateInstance = NVENCSTATUS(NVENCAPI *)(NV_ENCODE_API_FUNCTION_LIST *);

struct Slot {
    GLuint texture{};
    GLuint framebuffer{};
    NV_ENC_REGISTERED_PTR registered{};
    NV_ENC_INPUT_PTR mapped{};
    NV_ENC_OUTPUT_PTR bitstream{};
    // Claimed by the present thread for copy+encode, released by the harvest
    // thread after the access unit is queued. Plain acquire/release pairing.
    std::atomic<bool> busy{false};
    // Set by the harvest thread once the access unit is safely queued. The
    // present thread unmaps the slot on reclaim (only it holds a GL
    // context); NvEncUnmapInputResource fails without one, so the harvest
    // thread must never unmap.
    std::atomic<bool> harvested{true};
    // Present-thread-only copy stage. The framebuffer blit is queued first;
    // NVENC mapping waits until a later present observes this fence signaled,
    // avoiding an implicit GPU wait immediately after every game frame.
    GLsync copy_fence{};
    uint64_t copy_pts_ns{};
    bool copy_pending{};
};

// Do not rely on libGL exporting modern entry points.  With GLVND, and in
// particular for an EGL game, extension entry points are normally obtained
// through eglGetProcAddress instead.  Direct capture needs core GL 3.0 read
// framebuffer support: silently falling back to the application's current FBO
// would encode a shadow map/HUD target rather than the image being presented.
using BindFramebuffer = void (*)(GLenum target, GLuint framebuffer);
using GenFramebuffers = void (*)(GLsizei count, GLuint *framebuffers);
using DeleteFramebuffers = void (*)(GLsizei count, const GLuint *framebuffers);
using FramebufferTexture2D = void (*)(GLenum target, GLenum attachment, GLenum textarget,
                                     GLuint texture, GLint level);
using CheckFramebufferStatus = GLenum (*)(GLenum target);
using BlitFramebuffer = void (*)(GLint src_x0, GLint src_y0, GLint src_x1, GLint src_y1,
                                 GLint dst_x0, GLint dst_y0, GLint dst_x1, GLint dst_y1,
                                 GLbitfield mask, GLenum filter);
using FenceSync = GLsync (*)(GLenum condition, GLbitfield flags);
using ClientWaitSync = GLenum (*)(GLsync sync, GLbitfield flags, GLuint64 timeout);
using DeleteSync = void (*)(GLsync sync);

template <typename Function>
Function resolve_gl_function(const char *name) {
    void *symbol = dlsym(RTLD_DEFAULT, name);
    if (symbol == nullptr) {
        const __eglMustCastToProperFunctionPointerType proc = eglGetProcAddress(name);
        // Linux uses representation-compatible data and function pointers for
        // GL dispatch entries. memcpy keeps the conversion warning-free under
        // the strict native build flags.
        static_assert(sizeof(symbol) == sizeof(proc), "GL dispatch pointer size mismatch");
        memcpy(&symbol, &proc, sizeof(symbol));
    }
    Function function = nullptr;
    static_assert(sizeof(function) == sizeof(symbol), "GL function pointer size mismatch");
    memcpy(&function, &symbol, sizeof(function));
    return function;
}

struct GlCopyFunctions {
    BindFramebuffer bind_framebuffer{};
    GenFramebuffers gen_framebuffers{};
    DeleteFramebuffers delete_framebuffers{};
    FramebufferTexture2D framebuffer_texture_2d{};
    CheckFramebufferStatus check_framebuffer_status{};
    BlitFramebuffer blit_framebuffer{};
    FenceSync fence_sync{};
    ClientWaitSync client_wait_sync{};
    DeleteSync delete_sync{};
};

GlCopyFunctions resolve_gl_copy_functions() {
    return {
        resolve_gl_function<BindFramebuffer>("glBindFramebuffer"),
        resolve_gl_function<GenFramebuffers>("glGenFramebuffers"),
        resolve_gl_function<DeleteFramebuffers>("glDeleteFramebuffers"),
        resolve_gl_function<FramebufferTexture2D>("glFramebufferTexture2D"),
        resolve_gl_function<CheckFramebufferStatus>("glCheckFramebufferStatus"),
        resolve_gl_function<BlitFramebuffer>("glBlitFramebuffer"),
        resolve_gl_function<FenceSync>("glFenceSync"),
        resolve_gl_function<ClientWaitSync>("glClientWaitSync"),
        resolve_gl_function<DeleteSync>("glDeleteSync"),
    };
}


bool is_gl_3_or_newer() {
    const GLubyte *version_text = glGetString(GL_VERSION);
    if (version_text == nullptr) return false;
    unsigned major = 0;
    return sscanf(reinterpret_cast<const char *>(version_text), "%u", &major) == 1 && major >= 3;
}

struct CopyState {
    GLint texture_binding{};
    GLint read_framebuffer{};
    GLint draw_framebuffer{};
    GLint read_buffer{};
    GLboolean scissor_enabled{};
};

bool copy_framebuffer(const GlCopyFunctions &gl, GLuint source_framebuffer,
                      GLenum source_read_buffer, GLuint destination_framebuffer,
                      uint32_t width, uint32_t height, bool flip_y) {
    // READ_FRAMEBUFFER, DRAW_FRAMEBUFFER and READ_BUFFER are independent state.
    // Preserve all of them so the capture copy remains transparent to the game.
    CopyState saved{};
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &saved.texture_binding);
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &saved.read_framebuffer);
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &saved.draw_framebuffer);
    glGetIntegerv(GL_READ_BUFFER, &saved.read_buffer);
    saved.scissor_enabled = glIsEnabled(GL_SCISSOR_TEST);

    // OpenGL's framebuffer origin is bottom-left while the NVENC OpenGL input
    // is consumed top-to-bottom. A reversed destination rectangle performs the
    // required vertical flip in the GPU blit, avoiding a shader or CPU readback.
    gl.bind_framebuffer(GL_READ_FRAMEBUFFER, source_framebuffer);
    glReadBuffer(source_read_buffer);
    gl.bind_framebuffer(GL_DRAW_FRAMEBUFFER, destination_framebuffer);
    /* glBlitFramebuffer obeys the destination scissor test. Games commonly
     * leave scissoring enabled after their final HUD draw; applying that state
     * to our private encoder FBO would update only part of the texture and mix
     * old pixels with the current frame. */
    glDisable(GL_SCISSOR_TEST);
    const GLint destination_y0 = flip_y ? static_cast<GLint>(height) : 0;
    const GLint destination_y1 = flip_y ? 0 : static_cast<GLint>(height);
    gl.blit_framebuffer(0, 0, static_cast<GLint>(width), static_cast<GLint>(height),
                        0, destination_y0, static_cast<GLint>(width), destination_y1,
                        GL_COLOR_BUFFER_BIT, GL_NEAREST);

    gl.bind_framebuffer(GL_DRAW_FRAMEBUFFER, static_cast<GLuint>(saved.draw_framebuffer));
    gl.bind_framebuffer(GL_READ_FRAMEBUFFER, static_cast<GLuint>(saved.read_framebuffer));
    glReadBuffer(static_cast<GLenum>(saved.read_buffer));
    glBindTexture(GL_TEXTURE_2D, static_cast<GLuint>(saved.texture_binding));
    if (saved.scissor_enabled) glEnable(GL_SCISSOR_TEST);
    return true;
}
}
struct luma_nvenc_direct {
    void *library{};
    void *encoder{};
    NV_ENCODE_API_FUNCTION_LIST api{};
    std::array<Slot, kSlots> slots{};
    // Submission order ring, owned by the harvest thread. The present thread
    // only appends (counted) and claims FREE slots through busy.
    std::array<size_t, kSlots> submitted{};
    size_t submitted_head{};
    size_t submitted_tail{};
    // Produced by the present thread, consumed by the harvest thread.
    std::atomic<size_t> submitted_count{};
    size_t next{};
    size_t copy_next{};
    size_t encode_next{};
    bool initialized{};
    bool first_frame{true};
    std::atomic<bool> disabled{false};
    std::atomic<bool> stop_requested{false};
    std::atomic<bool> cleanup_started{false};
    std::atomic<bool> logged_failure{false};
    // Set by the unload notifier: hurry the harvest thread to teardown.
    // Exit-time driver cleanup cannot wait on encoder locks.
    std::atomic<bool> shutdown_now{false};
    // Set by the harvest thread as its last act before exiting. The
    // presenting thread collects the shell (destroy + NULL) once it observes
    // this; the harvest thread itself is already gone, so no join is needed.
    std::atomic<bool> torn_down{false};
    uint64_t next_due_ns{};
    uint64_t frame_interval_ns{};
    // Present-thread adaptive admission. Synchronous Linux NVENC may block
    // EncodePicture when its hardware queue saturates; back off capture work
    // until submissions are cheap again instead of pacing the game to NVENC.
    uint64_t adaptive_interval_ns{};
    uint64_t next_encode_ns{};
    unsigned fast_encode_streak{};
    unsigned admission_attempts{};
    unsigned admission_drops{};
    uint32_t width{}, height{}, fps{};
    GlCopyFunctions gl{};
    // Harvest thread: drains submitted access units with blocking driver
    // waits. It makes no GL or window-system calls, so it can never stall
    // the game or wedge its teardown; only the present thread touches GL.
    std::thread harvest_worker;
    // Receiver-only output collector. Unlike the legacy hook worker it never
    // destroys the encoder: GL resource teardown stays on the receiver thread.
    std::thread output_worker;
    // Receiver submissions may wait for collector/transport capacity without
    // blocking the game/import thread. The application-present path never waits
    // on these condition variables.
    std::mutex scheduler_mutex;
    std::condition_variable scheduler_wake;
    std::mutex output_mutex;
    std::condition_variable output_wake;
    Transport transport{};
};
namespace {
bool ok(NVENCSTATUS status) { return status == NV_ENC_SUCCESS; }

void disable(luma_nvenc_direct *state, const char *stage, NVENCSTATUS status) {
    state->disabled.store(true, std::memory_order_release);
    if (!state->logged_failure.exchange(true, std::memory_order_acq_rel)) {
        debug_log("direct NVENC disabled at %s (status=%d)", stage, static_cast<int>(status));
    }
}

bool poll(luma_nvenc_direct *state) {
    // Linux NVENC is synchronous. Bitstreams must be locked in the order their
    // successful EncodePicture calls were submitted. The texture ring wraps,
    // so its array order is not necessarily submission order.
    //
    // Blocking locks are correct here: poll runs on the harvest thread (or a
    // synchronous caller's own encoding thread), never on a latency-critical
    // presenting thread. doNotWait is an asynchronous-mode contract that
    // Linux drivers may ignore; in synchronous mode the lock waits for the
    // oldest submitted output to complete.
    while (state->submitted_count.load(std::memory_order_acquire) != 0) {
        const size_t slot_index = state->submitted[state->submitted_head];
        Slot &slot = state->slots[slot_index];
        if (!slot.busy.load(std::memory_order_acquire)) {
            disable(state, "internal output ordering", NV_ENC_ERR_INVALID_CALL);
            return false;
        }
        NV_ENC_LOCK_BITSTREAM lock{}; lock.version = NV_ENC_LOCK_BITSTREAM_VER; lock.outputBitstream = slot.bitstream;
        lock.doNotWait = 0;
        const NVENCSTATUS result = state->api.nvEncLockBitstream(state->encoder, &lock);
        if (result == NV_ENC_ERR_LOCK_BUSY) return true;
        // NEED_MORE_INPUT is never lockable. With the explicitly configured
        // IP-only stream below it is unexpected, so fail closed rather than
        // risking a permanently wedged output slot or an invalid lock.
        if (result == NV_ENC_ERR_NEED_MORE_INPUT) {
            disable(state, "NvEncLockBitstream returned NEED_MORE_INPUT", result);
            return false;
        }
        if (!ok(result)) {
            disable(state, "NvEncLockBitstream", result);
            return false;
        }
    const uint64_t enqueue_start = bench_enabled() ? bench_ns() : 0;
    const bool enqueued = state->transport.enqueue(
        static_cast<const uint8_t *>(lock.bitstreamBufferPtr),
        lock.bitstreamSizeInBytes, lock.outputTimeStamp,
        lock.pictureType == NV_ENC_PIC_TYPE_IDR);
    if (bench_enabled()) {
        bench().enqueue.fetch_add(bench_ns() - enqueue_start,
                                  std::memory_order_relaxed);
    }
    const NVENCSTATUS unlock = state->api.nvEncUnlockBitstream(state->encoder, slot.bitstream);
        const NVENCSTATUS unmap = state->api.nvEncUnmapInputResource(state->encoder, slot.mapped);
        slot.mapped = nullptr;
        slot.busy.store(false, std::memory_order_release);
        state->submitted_head = (state->submitted_head + 1) % kSlots;
        state->submitted_count.fetch_sub(1, std::memory_order_acq_rel);
        if (!ok(unlock)) {
            disable(state, "NvEncUnlockBitstream", unlock);
            return false;
        }
        if (!ok(unmap)) {
            disable(state, "NvEncUnmapInputResource", unmap);
            return false;
        }
        if (!enqueued) {
            /* Continuing after losing an encoded reference frame produces a
             * syntactically valid but undecodable H.264 stream. Stop the codec
             * chain here; the already-delivered prefix remains independently
             * decodable. Admission above makes this an exceptional path. */
            disable(state, "compressed access-unit transport", NV_ENC_ERR_OUT_OF_MEMORY);
            return false;
        }
    }
    return true;
}

// Harvest-thread harvest: lock, enqueue and unlock submitted access units,
// then hand each slot back. The legacy worker leaves unmapping to its GL
// owner; the receiver collector has a shared GL context and releases inputs
// itself, without making the submission context wait for resource recycling.
bool poll_harvest(luma_nvenc_direct *state, size_t limit = kSlots, bool release_inputs = false) {
    while (limit-- != 0 && state->submitted_count.load(std::memory_order_acquire) != 0) {
        const size_t slot_index = state->submitted[state->submitted_head];
        Slot &slot = state->slots[slot_index];
        if (!slot.busy.load(std::memory_order_acquire)) {
            disable(state, "internal output ordering", NV_ENC_ERR_INVALID_CALL);
            return false;
        }
        NV_ENC_LOCK_BITSTREAM lock{};
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = slot.bitstream;
        lock.doNotWait = 0;
        const NVENCSTATUS result = state->api.nvEncLockBitstream(state->encoder, &lock);
        if (result == NV_ENC_ERR_LOCK_BUSY) return true;
        if (result == NV_ENC_ERR_NEED_MORE_INPUT) {
            disable(state, "NvEncLockBitstream returned NEED_MORE_INPUT", result);
            return false;
        }
        if (!ok(result)) {
            disable(state, "NvEncLockBitstream", result);
            return false;
        }
        const bool enqueued = state->transport.enqueue(
            static_cast<const uint8_t *>(lock.bitstreamBufferPtr),
            lock.bitstreamSizeInBytes, lock.outputTimeStamp,
            lock.pictureType == NV_ENC_PIC_TYPE_IDR);
        const NVENCSTATUS unlock = state->api.nvEncUnlockBitstream(state->encoder, slot.bitstream);
        if (!ok(unlock)) {
            disable(state, "NvEncUnlockBitstream", unlock);
            return false;
        }
        if (!enqueued) {
            disable(state, "compressed access-unit transport", NV_ENC_ERR_OUT_OF_MEMORY);
            return false;
        }
        if (release_inputs) {
            const NVENCSTATUS unmapped = state->api.nvEncUnmapInputResource(state->encoder, slot.mapped);
            if (!ok(unmapped)) {
                disable(state, "output input reclaim", unmapped);
                return false;
            }
            slot.mapped = nullptr;
        }
        slot.harvested.store(true, std::memory_order_release);
        state->submitted_head = (state->submitted_head + 1) % kSlots;
        state->submitted_count.fetch_sub(1, std::memory_order_acq_rel);
        if (release_inputs) slot.busy.store(false, std::memory_order_release);
        state->scheduler_wake.notify_one();
    }
    return true;
}

}

// Release the encoder, its registered resources and the shared GL objects.
// The synchronous API calls this with a current context in the game's share
// group. The harvest thread calls it without one: driver-handle release is
// best effort there (errors are ignored, teardown always completes), while
// the FBO/texture deletes are repeated on the presenting thread through
// luma_nvenc_direct_release_gl().
void release_encoder_resources(luma_nvenc_direct *state, bool release_gl);

extern "C" struct luma_nvenc_direct *luma_nvenc_direct_create(uint32_t width, uint32_t height, uint32_t fps, uint32_t quality, const char *socket_path, const char *token_hex) {
    if (!width || !height || width > 16'384 || height > 16'384 || (width & 1U) != 0 ||
        (height & 1U) != 0 || !fps || fps > 480 || !socket_path) {
        debug_log("direct NVENC requires even dimensions up to 16384 and FPS from 1 through 480");
        return nullptr;
    }
    if (!is_gl_3_or_newer()) {
        debug_log("direct NVENC requires OpenGL 3.0 read-framebuffer support");
        return nullptr;
    }
    const GlCopyFunctions gl = resolve_gl_copy_functions();
    if (gl.bind_framebuffer == nullptr || gl.gen_framebuffers == nullptr ||
        gl.delete_framebuffers == nullptr ||
        gl.framebuffer_texture_2d == nullptr || gl.check_framebuffer_status == nullptr ||
        gl.blit_framebuffer == nullptr || gl.fence_sync == nullptr ||
        gl.client_wait_sync == nullptr || gl.delete_sync == nullptr) {
        debug_log("direct NVENC could not resolve the OpenGL framebuffer copy functions");
        return nullptr;
    }
    // The hook retains this state until process exit: NVENC/GL destruction must
    // happen with the original context current, and no present call may join a
    // socket thread. A failed stream is disabled below and no further frames are
    // copied or encoded.
    auto *state = new luma_nvenc_direct; state->width = width; state->height = height; state->fps = fps; state->frame_interval_ns = UINT64_C(1000000000) / fps; state->adaptive_interval_ns = state->frame_interval_ns; state->gl = gl; state->transport.width = width; state->transport.height = height; state->transport.fps = fps; state->transport.path = socket_path;
    if (!parse_token(token_hex, state->transport.token)) { debug_log("invalid stream token"); delete state; return nullptr; }
    state->library = dlopen("libnvidia-encode.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!state->library) { debug_log("could not load libnvidia-encode.so.1: %s", dlerror()); delete state; return nullptr; }
    auto create = reinterpret_cast<CreateInstance>(dlsym(state->library, "NvEncodeAPICreateInstance"));
    state->api.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open{}; open.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER; open.deviceType = NV_ENC_DEVICE_TYPE_OPENGL; open.device = nullptr; open.apiVersion = NVENCAPI_VERSION;
    if (!create) { debug_log("NvEncodeAPICreateInstance symbol unavailable"); delete state; return nullptr; }
    NVENCSTATUS status = create(&state->api);
    if (!ok(status)) { debug_log("NvEncodeAPICreateInstance failed (status=%d)", static_cast<int>(status)); delete state; return nullptr; }
    status = state->api.nvEncOpenEncodeSessionEx(&open, &state->encoder);
    if (!ok(status)) { debug_log("NvEncOpenEncodeSessionEx(OpenGL) failed (status=%d)", static_cast<int>(status)); delete state; return nullptr; }

    NV_ENC_PRESET_CONFIG preset{};
    preset.version = NV_ENC_PRESET_CONFIG_VER;
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    if (!state->api.nvEncGetEncodePresetConfigEx) { debug_log("NvEncGetEncodePresetConfigEx unavailable"); delete state; return nullptr; }
    status = state->api.nvEncGetEncodePresetConfigEx(state->encoder, NV_ENC_CODEC_H264_GUID,
                                                     NV_ENC_PRESET_P1_GUID,
                                                     NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                                                     &preset);
    if (!ok(status)) { debug_log("NvEncGetEncodePresetConfigEx failed (status=%d)", static_cast<int>(status)); delete state; return nullptr; }
    NV_ENC_CONFIG config = preset.presetCfg;
    config.version = NV_ENC_CONFIG_VER;
    // P1/ULL is not enough by itself: make the synchronous contract explicit.
    // This is a single-pass, zero-reorder IP stream; B frames, lookahead and
    // multipass work would add frame delay and NEED_MORE_INPUT semantics.
    config.gopLength = fps * 2;
    config.frameIntervalP = 1;
    config.rcParams.enableLookahead = 0;
    config.rcParams.disableIadapt = 1;
    config.rcParams.disableBadapt = 1;
    config.rcParams.enableTemporalAQ = 0;
    config.rcParams.zeroReorderDelay = 1;
    config.rcParams.multiPass = NV_ENC_MULTI_PASS_DISABLED;
    // Do not leave the driver to select an implicit (and potentially
    // lossless/unbounded) rate-control mode at 480 FPS. This is the same
    // fixed-QP contract as Luma's screen recorder.
    const uint32_t qp = capture_qp(quality);
    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CONSTQP;
    config.rcParams.constQP.qpIntra = qp;
    config.rcParams.constQP.qpInterP = qp;
    config.rcParams.constQP.qpInterB = qp;
    config.encodeCodecConfig.h264Config.repeatSPSPPS = 1;
    config.encodeCodecConfig.h264Config.outputAUD = 1;
    config.encodeCodecConfig.h264Config.idrPeriod = config.gopLength;
    /* A single-reference IP chain is the lowest-overhead mode and avoids the
     * driver's multi-reference list producing streams that FFmpeg's software
     * decoder rejects at very high declared frame rates. */
    config.encodeCodecConfig.h264Config.maxNumRefFrames = 1;
    config.encodeCodecConfig.h264Config.numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
    config.encodeCodecConfig.h264Config.numRefL1 = NV_ENC_NUM_REF_FRAMES_1;
    NV_ENC_INITIALIZE_PARAMS init{}; init.version = NV_ENC_INITIALIZE_PARAMS_VER; init.encodeGUID = NV_ENC_CODEC_H264_GUID; init.presetGUID = NV_ENC_PRESET_P1_GUID; init.encodeWidth = width; init.encodeHeight = height; init.darWidth = width; init.darHeight = height; init.frameRateNum = fps; init.frameRateDen = 1; init.enableEncodeAsync = 0; init.enablePTD = 1; init.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY; init.encodeConfig = &config;
    status = state->api.nvEncInitializeEncoder(state->encoder, &init);
    if (!ok(status)) { debug_log("NvEncInitializeEncoder failed (status=%d)", static_cast<int>(status)); delete state; return nullptr; }
    GLint prior_texture = 0;
    GLint prior_read_framebuffer = 0;
    GLint prior_draw_framebuffer = 0;
    std::array<GLuint, kSlots> textures{};
    std::array<GLuint, kSlots> framebuffers{};
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &prior_texture);
    glGetIntegerv(GL_READ_FRAMEBUFFER_BINDING, &prior_read_framebuffer);
    glGetIntegerv(GL_DRAW_FRAMEBUFFER_BINDING, &prior_draw_framebuffer);
    // Slot has bookkeeping between texture members; writing kSlots directly
    // from &slots[0].texture corrupts those fields. Generate into contiguous
    // storage, then assign each texture explicitly.
    glGenTextures(static_cast<GLsizei>(textures.size()), textures.data());
    gl.gen_framebuffers(static_cast<GLsizei>(framebuffers.size()), framebuffers.data());
    for (size_t index = 0; index < state->slots.size(); ++index) {
        auto &slot = state->slots[index];
        slot.texture = textures[index];
        slot.framebuffer = framebuffers[index];
        glBindTexture(GL_TEXTURE_2D, slot.texture); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST); glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST); glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, width, height, 0, GL_RGBA, GL_UNSIGNED_BYTE, nullptr);
        gl.bind_framebuffer(GL_DRAW_FRAMEBUFFER, slot.framebuffer);
        gl.framebuffer_texture_2d(GL_DRAW_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                                  slot.texture, 0);
        if (gl.check_framebuffer_status(GL_DRAW_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
            debug_log("direct NVENC could not create a complete framebuffer for slot %zu", index);
            gl.bind_framebuffer(GL_DRAW_FRAMEBUFFER,
                                static_cast<GLuint>(prior_draw_framebuffer));
            gl.bind_framebuffer(GL_READ_FRAMEBUFFER,
                                static_cast<GLuint>(prior_read_framebuffer));
            glBindTexture(GL_TEXTURE_2D, static_cast<GLuint>(prior_texture));
            delete state;
            return nullptr;
        }
        NV_ENC_INPUT_RESOURCE_OPENGL_TEX texture{slot.texture, GL_TEXTURE_2D};
        NV_ENC_REGISTER_RESOURCE resource{}; resource.version = NV_ENC_REGISTER_RESOURCE_VER; resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_OPENGL_TEX; resource.width = width; resource.height = height; resource.pitch = width * 4; resource.resourceToRegister = &texture; resource.bufferFormat = NV_ENC_BUFFER_FORMAT_ABGR; resource.bufferUsage = NV_ENC_INPUT_IMAGE;
        status = state->api.nvEncRegisterResource(state->encoder, &resource);
        if (!ok(status)) { debug_log("NvEncRegisterResource failed (status=%d)", static_cast<int>(status)); delete state; return nullptr; }
        slot.registered = resource.registeredResource;
        NV_ENC_CREATE_BITSTREAM_BUFFER bitstream{}; bitstream.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        status = state->api.nvEncCreateBitstreamBuffer(state->encoder, &bitstream);
        if (!ok(status)) { debug_log("NvEncCreateBitstreamBuffer failed (status=%d)", static_cast<int>(status)); delete state; return nullptr; }
        slot.bitstream = bitstream.bitstreamBuffer;
    }
    gl.bind_framebuffer(GL_DRAW_FRAMEBUFFER, static_cast<GLuint>(prior_draw_framebuffer));
    gl.bind_framebuffer(GL_READ_FRAMEBUFFER, static_cast<GLuint>(prior_read_framebuffer));
    glBindTexture(GL_TEXTURE_2D, static_cast<GLuint>(prior_texture));
    state->initialized = true;
    state->transport.worker = std::thread(transport_main, &state->transport);
    return state;
}

extern "C" int luma_nvenc_direct_submit(luma_nvenc_direct *state, uint32_t source_width,
                                           uint32_t source_height, uint64_t pts_ns);

namespace {
int submit_framebuffer(luma_nvenc_direct *state, GLuint source_framebuffer,
                       GLenum source_read_buffer, uint32_t source_width,
                       uint32_t source_height, bool flip_y, uint64_t pts_ns) {
    if (!state || !state->initialized || state->disabled.load(std::memory_order_acquire) || state->transport.failed.load(std::memory_order_acquire) || !state->transport.ready.load(std::memory_order_acquire)) return 0;
    // The hook supplies the actual GLX drawable/EGL surface dimensions. Unlike
    // GL_VIEWPORT, these do not change for ordinary game rendering passes.
    if ((source_width & 1U) != 0 || (source_height & 1U) != 0 || source_width != state->width || source_height != state->height) {
        disable(state, "present surface resized (capture resolution is fixed)", NV_ENC_ERR_INVALID_PARAM);
        return 0;
    }
    // External exporters have already paced these actual source frames. A
    // second clock can drift after a dropped frame and reject valid input.
    if (!state->output_worker.joinable()) {
        if (state->next_due_ns != 0 && pts_ns < state->next_due_ns) return 0;
        if (state->next_due_ns == 0 || pts_ns - state->next_due_ns >= state->frame_interval_ns) state->next_due_ns = pts_ns + state->frame_interval_ns;
        else state->next_due_ns += state->frame_interval_ns;
    }
    const bool bench_on = bench_enabled();
    uint64_t pace_start = bench_on ? bench_ns() : 0;
    const uint64_t poll_start = bench_on ? bench_ns() : 0;
    if (!state->output_worker.joinable() && !poll(state)) return 0;
    if (bench_on) bench().poll.fetch_add(bench_ns() - poll_start, std::memory_order_relaxed);
    if (bench_on) pace_start = bench_ns();
    /* Reserve transport capacity conceptually before encoding. Every existing
     * NVENC submission will later occupy one queue entry, and the new frame
     * adds one more. This keeps overload drops ahead of the codec reference
     * chain instead of discarding an encoded access unit afterward. */
    size_t in_flight = state->submitted_count.load(std::memory_order_acquire);
    Slot *slot_ptr = &state->slots[state->next];
    if (state->output_worker.joinable()) {
        // This runs only on the receiver's private encode worker. Waiting here
        // preserves a frame already accepted into the bounded staging FIFO;
        // it cannot stall the game or the receiver's import/ACK context.
        for (;;) {
            if (state->disabled.load(std::memory_order_acquire) ||
                state->stop_requested.load(std::memory_order_acquire) ||
                state->transport.failed.load(std::memory_order_acquire)) {
                return 0;
            }
            in_flight = state->submitted_count.load(std::memory_order_acquire);
            slot_ptr = &state->slots[state->next];
            if (in_flight < kSlots &&
                !slot_ptr->busy.load(std::memory_order_acquire) &&
                state->transport.can_admit(in_flight)) {
                break;
            }
            std::unique_lock<std::mutex> lock(state->scheduler_mutex);
            state->scheduler_wake.wait_for(lock, std::chrono::milliseconds(1));
        }
    } else if (!state->transport.can_admit(in_flight) ||
               slot_ptr->busy.load(std::memory_order_acquire) || in_flight >= kSlots) {
        return 0;
    }
    Slot &slot = *slot_ptr;
    if (bench_on) bench().pace.fetch_add(bench_ns() - pace_start, std::memory_order_relaxed);
    const uint64_t copy_start = bench_on ? bench_ns() : 0;
    if (!copy_framebuffer(state->gl, source_framebuffer, source_read_buffer, slot.framebuffer,
                          state->width, state->height, flip_y)) {
        disable(state, "copy from source read framebuffer", NV_ENC_ERR_INVALID_CALL);
        return 0;
    }
    // NVENC's OpenGL device shares the current context. Flush submits the GPU
    // copy before NvEncMapInputResource without CPU-side glFinish/readback.
    glFlush();
    if (bench_on) bench().copy.fetch_add(bench_ns() - copy_start, std::memory_order_relaxed);
    const uint64_t map_start = bench_on ? bench_ns() : 0;
    NV_ENC_MAP_INPUT_RESOURCE map{}; map.version = NV_ENC_MAP_INPUT_RESOURCE_VER; map.registeredResource = slot.registered;
    const NVENCSTATUS mapped = state->api.nvEncMapInputResource(state->encoder, &map);
    if (!ok(mapped)) { disable(state, "NvEncMapInputResource", mapped); return 0; }
    slot.mapped = map.mappedResource;
    NV_ENC_PIC_PARAMS picture{}; picture.version = NV_ENC_PIC_PARAMS_VER; picture.inputWidth = state->width; picture.inputHeight = state->height; picture.inputPitch = state->width; picture.inputBuffer = slot.mapped; picture.bufferFmt = map.mappedBufferFmt; picture.outputBitstream = slot.bitstream; picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME; picture.inputTimeStamp = pts_ns; picture.inputDuration = 1; if (state->first_frame) picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
    const NVENCSTATUS encoded = state->api.nvEncEncodePicture(state->encoder, &picture);
    if (bench_on) bench().map_encode.fetch_add(bench_ns() - map_start, std::memory_order_relaxed);
    if (encoded == NV_ENC_ERR_NEED_MORE_INPUT) {
        // This would require a later input and a strict in-order lock protocol.
        // We configured IP-only zero-reorder specifically to avoid it; retaining
        // the mapped slot and disabling is the safe nonblocking failure mode.
        disable(state, "NvEncEncodePicture returned NEED_MORE_INPUT", encoded);
        return 0;
    }
    if (!ok(encoded)) {
        const NVENCSTATUS unmapped = state->api.nvEncUnmapInputResource(state->encoder, slot.mapped);
        slot.mapped = nullptr;
        if (!ok(unmapped)) debug_log("NvEncUnmapInputResource after encode failure failed (status=%d)", static_cast<int>(unmapped));
        disable(state, "NvEncEncodePicture", encoded);
        return 0;
    }
    slot.harvested.store(false, std::memory_order_release);
    slot.busy.store(true, std::memory_order_release);
    state->submitted[state->submitted_tail] = state->next;
    state->submitted_tail = (state->submitted_tail + 1) % kSlots;
    state->submitted_count.fetch_add(1, std::memory_order_release);
    state->output_wake.notify_one();
    state->first_frame = false;
    state->next = (state->next + 1) % kSlots;
    if (bench_on &&
        bench().frames.fetch_add(1, std::memory_order_relaxed) + 1 >= 512) {
        bench_dump();
    }
    return 1;
}
}

// Harvest thread: drains submitted access units with blocking driver waits
// and tears the session down on stop. It makes no GL or window-system calls,
// so it can neither stall the game nor wedge its teardown; only the present
// thread creates and probes the copy fences.
void harvest_worker_main(luma_nvenc_direct *state) {
    while (!state->stop_requested.load(std::memory_order_acquire) &&
           !state->disabled.load(std::memory_order_acquire) &&
           !state->shutdown_now.load(std::memory_order_acquire)) {
        if (state->submitted_count.load(std::memory_order_acquire) == 0) {
            usleep(200);
            continue;
        }
        const uint64_t poll_start = bench_enabled() ? bench_ns() : 0;
        if (!poll_harvest(state)) {
            break;
        }
        if (bench_enabled()) {
            bench().poll.fetch_add(bench_ns() - poll_start, std::memory_order_relaxed);
        }
    }
    // Drain on stop: harvest everything already submitted (blocking is fine
    // here). Process unload skips the wait: presents have ended and exit-time
    // driver cleanup cannot block on encoder locks.
    if (!state->shutdown_now.load(std::memory_order_acquire)) {
        poll_harvest(state);
    }
    state->transport.stop.store(true, std::memory_order_release);
    state->transport.wake.notify_all();
    if (state->transport.worker.joinable() &&
        state->transport.worker.get_id() != std::this_thread::get_id()) {
        state->transport.worker.join();
    }
    release_encoder_resources(state, false);
    state->torn_down.store(true, std::memory_order_release);
}

extern "C" int luma_nvenc_direct_start_harvest_worker(luma_nvenc_direct *state) {
    if (state == nullptr || !state->initialized) {
        return -1;
    }
    try {
        state->harvest_worker = std::thread(harvest_worker_main, state);
    } catch (...) {
        return -1;
    }
    state->harvest_worker.detach();
    return 0;
}

extern "C" int luma_nvenc_direct_torn_down(luma_nvenc_direct *state) {
    return state != nullptr && state->torn_down.load(std::memory_order_acquire);
}

// One-line state census for stall forensics (LUMA_GAME_CAPTURE_BENCH only).
// Called from the present thread every 512th due present; all atomics.
void bench_census(luma_nvenc_direct *state) {
    size_t transport_in_use = 0;
    for (const PacketSlot &slot : state->transport.slots) {
        transport_in_use += slot.in_use ? 1 : 0;
    }
    char slots[160];
    size_t pos = 0;
    for (size_t i = 0; i < state->slots.size() && pos < sizeof(slots) - 8; ++i) {
        const Slot &slot = state->slots[i];
        pos += static_cast<size_t>(snprintf(slots + pos, sizeof(slots) - pos, "%u%u%u%u ",
                                            slot.busy.load(std::memory_order_acquire) ? 1 : 0,
                                            slot.harvested.load(std::memory_order_acquire) ? 1 : 0,
                                            slot.mapped != nullptr ? 1 : 0,
                                            slot.copy_pending ? 1 : 0));
    }
    fprintf(stderr,
            "luma game capture: census count=%zu copy=%zu encode=%zu "
            "slots[busy,harvested,mapped,pending]=%s"
            "transport_in_use=%zu/%zu ready=%d failed=%d disabled=%d stop=%d torn=%d\n",
            state->submitted_count.load(std::memory_order_acquire), state->copy_next,
            state->encode_next, slots,
            transport_in_use, kQueueMax,
            state->transport.ready.load(std::memory_order_acquire) ? 1 : 0,
            state->transport.failed.load(std::memory_order_acquire) ? 1 : 0,
            state->disabled.load(std::memory_order_acquire) ? 1 : 0,
            state->stop_requested.load(std::memory_order_acquire) ? 1 : 0,
            state->torn_down.load(std::memory_order_acquire) ? 1 : 0);
}

namespace {
// Submit at most one completed copy per present. The zero-timeout fence probe
// is the important part: NvEncMapInputResource must never be asked to wait for
// a blit that was only just queued by the game thread. If the oldest copy is
// still on the GPU, this present simply leaves it queued and returns.
int encode_ready_copy(luma_nvenc_direct *state, bool bench_on, uint64_t present_ns) {
    Slot &slot = state->slots[state->encode_next];
    if (!slot.copy_pending) return 0;
    if (state->next_encode_ns != 0 && present_ns < state->next_encode_ns) return 0;

    const size_t in_flight = state->submitted_count.load(std::memory_order_acquire);
    if (in_flight >= kSlots || !state->transport.can_admit(in_flight)) return 0;

    const GLenum wait = state->gl.client_wait_sync(slot.copy_fence, 0, 0);
    if (wait == GL_TIMEOUT_EXPIRED) return 0;
    if (wait != GL_ALREADY_SIGNALED && wait != GL_CONDITION_SATISFIED) {
        disable(state, "OpenGL copy fence wait", NV_ENC_ERR_GENERIC);
        return 0;
    }
    state->gl.delete_sync(slot.copy_fence);
    slot.copy_fence = nullptr;

    const uint64_t map_start = bench_ns();
    NV_ENC_MAP_INPUT_RESOURCE map{};
    map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
    map.registeredResource = slot.registered;
    const NVENCSTATUS mapped = state->api.nvEncMapInputResource(state->encoder, &map);
    if (!ok(mapped)) {
        slot.copy_pending = false;
        disable(state, "NvEncMapInputResource", mapped);
        return 0;
    }
    slot.mapped = map.mappedResource;
    NV_ENC_PIC_PARAMS picture{};
    picture.version = NV_ENC_PIC_PARAMS_VER;
    picture.inputWidth = state->width;
    picture.inputHeight = state->height;
    picture.inputPitch = state->width;
    picture.inputBuffer = slot.mapped;
    picture.bufferFmt = map.mappedBufferFmt;
    picture.outputBitstream = slot.bitstream;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picture.inputTimeStamp = slot.copy_pts_ns;
    picture.inputDuration = 1;
    if (state->first_frame) picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
    const NVENCSTATUS encoded = state->api.nvEncEncodePicture(state->encoder, &picture);
    const uint64_t map_encode_ns = bench_ns() - map_start;
    if (bench_on) {
        bench().map_encode.fetch_add(map_encode_ns, std::memory_order_relaxed);
    }
    if (encoded == NV_ENC_ERR_NEED_MORE_INPUT) {
        slot.copy_pending = false;
        disable(state, "NvEncEncodePicture returned NEED_MORE_INPUT", encoded);
        return 0;
    }
    if (!ok(encoded)) {
        const NVENCSTATUS unmapped =
            state->api.nvEncUnmapInputResource(state->encoder, slot.mapped);
        slot.mapped = nullptr;
        slot.copy_pending = false;
        if (!ok(unmapped)) {
            debug_log("NvEncUnmapInputResource after encode failure failed (status=%d)",
                      static_cast<int>(unmapped));
        }
        disable(state, "NvEncEncodePicture", encoded);
        return 0;
    }

    if (map_encode_ns > kSlowSubmitNs) {
        const uint64_t safe_interval =
            std::min(kMaxBackoffNs, std::max(state->frame_interval_ns, map_encode_ns * 2));
        if (safe_interval > state->adaptive_interval_ns) {
            state->adaptive_interval_ns = safe_interval;
            debug_log("NVENC submit took %.2f ms; capture admission backed off to %.1f FPS",
                      map_encode_ns / 1000000.0,
                      1000000000.0 / state->adaptive_interval_ns);
        }
        state->fast_encode_streak = 0;
    } else if (state->adaptive_interval_ns > state->frame_interval_ns &&
               ++state->fast_encode_streak >= 256) {
        const uint64_t reduced = state->adaptive_interval_ns * 9 / 10;
        state->adaptive_interval_ns = std::max(state->frame_interval_ns, reduced);
        state->fast_encode_streak = 0;
    }
    state->next_encode_ns = present_ns + state->adaptive_interval_ns;

    slot.copy_pending = false;
    slot.busy.store(true, std::memory_order_release);
    slot.harvested.store(false, std::memory_order_release);
    state->submitted[state->submitted_tail] = state->encode_next;
    state->submitted_tail = (state->submitted_tail + 1) % kSlots;
    state->submitted_count.fetch_add(1, std::memory_order_release);
    state->first_frame = false;
    state->encode_next = (state->encode_next + 1) % kSlots;
    if (bench_on && bench().frames.fetch_add(1, std::memory_order_relaxed) + 1 >= 512) {
        bench_dump();
    }
    return 1;
}

void record_admission(luma_nvenc_direct *state, bool dropped) {
    ++state->admission_attempts;
    state->admission_drops += dropped ? 1U : 0U;
    if (state->admission_attempts < 64) return;
    if (state->admission_drops >= 8) {
        const uint64_t increased = state->adaptive_interval_ns +
                                   std::max(UINT64_C(250000),
                                            state->adaptive_interval_ns / 4);
        const uint64_t limited = std::min(kMaxBackoffNs, increased);
        if (limited > state->adaptive_interval_ns) {
            state->adaptive_interval_ns = limited;
            state->fast_encode_streak = 0;
            debug_log("capture queue was saturated (%u/64 drops); admission backed off to %.1f FPS",
                      state->admission_drops,
                      1000000000.0 / state->adaptive_interval_ns);
        }
    }
    state->admission_attempts = 0;
    state->admission_drops = 0;
}
}

// Present-thread half of async capture. A due frame is blitted into a private
// texture and fenced, then the call returns. Later presents encode only copies
// whose fence is already signaled. Harvesting, compressed transport and
// teardown remain on workers; overload drops instead of waiting in the game.
extern "C" int luma_nvenc_direct_submit_async(luma_nvenc_direct *state, uint32_t source_width,
                                              uint32_t source_height, uint64_t pts_ns) {
    if (state == nullptr || !state->initialized || state->disabled.load(std::memory_order_acquire) ||
        state->transport.failed.load(std::memory_order_acquire) ||
        !state->transport.ready.load(std::memory_order_acquire)) {
        return 0;
    }
    if (state->stop_requested.load(std::memory_order_acquire) ||
        state->torn_down.load(std::memory_order_acquire)) {
        return 0;
    }
    if ((source_width & 1U) != 0 || (source_height & 1U) != 0 || source_width != state->width ||
        source_height != state->height) {
        disable(state, "present surface resized (capture resolution is fixed)", NV_ENC_ERR_INVALID_PARAM);
        return 0;
    }
    const bool bench_on = bench_enabled();
    const int encoded = encode_ready_copy(state, bench_on, pts_ns);
    if (state->disabled.load(std::memory_order_acquire)) return 0;

    if (state->next_due_ns != 0 && pts_ns < state->next_due_ns) return encoded;
    state->next_due_ns = pts_ns + state->adaptive_interval_ns;
    if (bench_on) {
        static std::atomic<uint64_t> due_count{0};
        if (due_count.fetch_add(1, std::memory_order_relaxed) % 2048 == 0) {
            bench_census(state);
        }
    }
    const uint64_t pace_start = bench_on ? bench_ns() : 0;
    Slot &slot = state->slots[state->copy_next];
    if (slot.copy_pending) {
        record_admission(state, true);
        return encoded;
    }
    // Reclaim a harvested slot: unmap here, where this thread holds the GL
    // context. The harvest thread must never unmap (see poll_harvest).
    if (slot.busy.load(std::memory_order_acquire)) {
        if (!slot.harvested.load(std::memory_order_acquire)) {
            record_admission(state, true);
            return encoded;
        }
        if (slot.mapped != nullptr) {
            const NVENCSTATUS unmapped =
                state->api.nvEncUnmapInputResource(state->encoder, slot.mapped);
            slot.mapped = nullptr;
            if (!ok(unmapped)) {
                disable(state, "NvEncUnmapInputResource on reclaim", unmapped);
                return 0;
            }
        }
        slot.busy.store(false, std::memory_order_release);
    }
    if (bench_on) bench().pace.fetch_add(bench_ns() - pace_start, std::memory_order_relaxed);
    const uint64_t copy_start = bench_on ? bench_ns() : 0;
    if (!copy_framebuffer(state->gl, 0, GL_BACK, slot.framebuffer, state->width, state->height,
                          true)) {
        disable(state, "copy from source read framebuffer", NV_ENC_ERR_INVALID_CALL);
        return 0;
    }
    slot.copy_fence = state->gl.fence_sync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
    if (slot.copy_fence == nullptr) {
        disable(state, "create OpenGL copy fence", NV_ENC_ERR_GENERIC);
        return 0;
    }
    slot.copy_pts_ns = pts_ns;
    slot.copy_pending = true;
    state->copy_next = (state->copy_next + 1) % kSlots;
    // Make the blit and fence visible to the GPU, but never wait for either.
    glFlush();
    if (bench_on) bench().copy.fetch_add(bench_ns() - copy_start, std::memory_order_relaxed);
    record_admission(state, false);
    return encoded;
}

extern "C" int luma_nvenc_direct_submit(luma_nvenc_direct *state, uint32_t source_width,
                                        uint32_t source_height, uint64_t pts_ns) {
    return submit_framebuffer(state, 0, GL_BACK, source_width, source_height, true, pts_ns);
}

extern "C" int luma_nvenc_direct_start_output_worker(luma_nvenc_direct *state) {
    if (!state || state->output_worker.joinable() || state->harvest_worker.joinable()) return 0;
    // NVENC's GL unmap requires a current context, but must not run on the
    // submission context: it can synchronize unrelated, later GPU copies.
    const EGLDisplay display = eglGetCurrentDisplay();
    const EGLContext parent = eglGetCurrentContext();
    EGLint config_id = 0, count = 0;
    EGLConfig config = nullptr;
    if (display == EGL_NO_DISPLAY || parent == EGL_NO_CONTEXT ||
        !eglQueryContext(display, parent, EGL_CONFIG_ID, &config_id)) return 0;
    const EGLint config_attributes[] = {EGL_CONFIG_ID, config_id, EGL_NONE};
    if (!eglChooseConfig(display, config_attributes, &config, 1, &count) || count != 1) return 0;
    const EGLint attributes[] = {EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE};
    const EGLContext context = eglCreateContext(display, config, parent, attributes);
    const EGLint pbuffer[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    const EGLSurface surface = eglCreatePbufferSurface(display, config, pbuffer);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE) {
        if (surface != EGL_NO_SURFACE) eglDestroySurface(display, surface);
        if (context != EGL_NO_CONTEXT) eglDestroyContext(display, context);
        return 0;
    }
    try {
        state->output_worker = std::thread([state, display, context, surface] {
            if (!eglBindAPI(EGL_OPENGL_API) || !eglMakeCurrent(display, surface, surface, context)) {
                disable(state, "output GL context", NV_ENC_ERR_INVALID_DEVICE);
                eglDestroySurface(display, surface);
                eglDestroyContext(display, context);
                return;
            }
            uint64_t last_output = bench_ns();
            bool drain = true;
            while (!state->stop_requested.load(std::memory_order_acquire) &&
                   !state->disabled.load(std::memory_order_acquire)) {
                // Keep several frames in flight so NVENC stays fed, but wake on
                // new work and bound how long low-rate output sits uncollected.
                // This avoids the old 100 us polling loop entirely.
                const uint64_t collect_after = std::clamp<uint64_t>(
                    state->frame_interval_ns * 2, 2000000, 10000000);
                {
                    std::unique_lock<std::mutex> lock(state->output_mutex);
                    state->output_wake.wait_for(
                        lock, std::chrono::nanoseconds(collect_after), [state] {
                            return state->stop_requested.load(std::memory_order_acquire) ||
                                   state->disabled.load(std::memory_order_acquire) ||
                                   state->submitted_count.load(std::memory_order_acquire) >=
                                       kCollectorLead;
                        });
                }
                if (state->submitted_count.load(std::memory_order_acquire) >= kCollectorLead ||
                    (state->submitted_count.load(std::memory_order_acquire) != 0 &&
                     bench_ns() - last_output >= collect_after)) {
                    if (!poll_harvest(state, 1, true)) { drain = false; break; }
                    last_output = bench_ns();
                }
            }
            // Preserve the complete encoded prefix before transport EOF.
            if (drain) (void)poll_harvest(state, kSlots, true);
            eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
            eglDestroySurface(display, surface);
            eglDestroyContext(display, context);
        });
    } catch (...) {
        eglDestroySurface(display, surface);
        eglDestroyContext(display, context);
        return 0;
    }
    return 1;
}

extern "C" int luma_nvenc_direct_wait_ready(luma_nvenc_direct *state, uint32_t timeout_ms) {
    if (!state) return 0;
    for (uint32_t elapsed = 0; elapsed < timeout_ms; ++elapsed) {
        if (state->transport.ready.load(std::memory_order_acquire)) return 1;
        if (state->transport.failed.load(std::memory_order_acquire)) return 0;
        usleep(1000);
    }
    return state->transport.ready.load(std::memory_order_acquire) ? 1 : 0;
}

extern "C" int luma_nvenc_direct_submit_framebuffer(luma_nvenc_direct *state,
                                                        uint32_t source_framebuffer,
                                                        uint32_t source_width,
                                                        uint32_t source_height,
                                                        int flip_y, uint64_t pts_ns) {
    return submit_framebuffer(state, static_cast<GLuint>(source_framebuffer),
                              GL_COLOR_ATTACHMENT0, source_width, source_height,
                              flip_y != 0, pts_ns);
}

extern "C" void luma_nvenc_direct_request_stop(luma_nvenc_direct *state) {
    if (state == nullptr) return;
    debug_log("stopping direct capture transport");
    state->disabled.store(true, std::memory_order_release);
    state->stop_requested.store(true, std::memory_order_release);
    state->scheduler_wake.notify_all();
    state->output_wake.notify_all();
}

extern "C" int luma_nvenc_direct_stop_requested(luma_nvenc_direct *state) {
    return state != nullptr && state->stop_requested.load(std::memory_order_acquire);
}

extern "C" void luma_nvenc_direct_notify_unload(luma_nvenc_direct *state) {
    if (state == nullptr) {
        return;
    }
    state->stop_requested.store(true, std::memory_order_release);
    state->shutdown_now.store(true, std::memory_order_release);
}

// Release the encoder, its registered resources and the shared GL objects.
// The synchronous API calls this with a current context in the game's share
// group. The harvest thread calls it with release_gl=false: driver-handle
// release is best effort there (no current context), while the FBO/texture
// deletes are repeated on the presenting thread through
// luma_nvenc_direct_release_gl().
void release_encoder_resources(luma_nvenc_direct *state, bool release_gl) {
    if (state->encoder != nullptr) {
        for (Slot &slot : state->slots) {
            if (slot.mapped != nullptr && state->api.nvEncUnmapInputResource != nullptr) {
                (void)state->api.nvEncUnmapInputResource(state->encoder, slot.mapped);
                slot.mapped = nullptr;
            }
            slot.busy.store(false, std::memory_order_release);
        }
        for (Slot &slot : state->slots) {
            if (slot.bitstream != nullptr && state->api.nvEncDestroyBitstreamBuffer != nullptr) {
                (void)state->api.nvEncDestroyBitstreamBuffer(state->encoder, slot.bitstream);
                slot.bitstream = nullptr;
            }
            if (slot.registered != nullptr && state->api.nvEncUnregisterResource != nullptr) {
                (void)state->api.nvEncUnregisterResource(state->encoder, slot.registered);
                slot.registered = nullptr;
            }
        }
    }

    if (release_gl) {
        std::array<GLuint, kSlots> framebuffers{};
        std::array<GLuint, kSlots> textures{};
        for (size_t index = 0; index < state->slots.size(); ++index) {
            if (state->slots[index].copy_fence != nullptr) {
                state->gl.delete_sync(state->slots[index].copy_fence);
                state->slots[index].copy_fence = nullptr;
            }
            state->slots[index].copy_pending = false;
            framebuffers[index] = state->slots[index].framebuffer;
            textures[index] = state->slots[index].texture;
            state->slots[index].framebuffer = 0;
            state->slots[index].texture = 0;
        }
        state->gl.delete_framebuffers(static_cast<GLsizei>(framebuffers.size()), framebuffers.data());
        glDeleteTextures(static_cast<GLsizei>(textures.size()), textures.data());
    }

    if (state->encoder != nullptr && state->api.nvEncDestroyEncoder != nullptr) {
        (void)state->api.nvEncDestroyEncoder(state->encoder);
        state->encoder = nullptr;
    }
    if (state->library != nullptr) {
        dlclose(state->library);
        state->library = nullptr;
    }
    state->initialized = false;
    state->submitted_count.store(0, std::memory_order_release);
}

extern "C" void luma_nvenc_direct_finish_on_gl_thread(luma_nvenc_direct *state) {
    if (state == nullptr || state->cleanup_started.exchange(true, std::memory_order_acq_rel)) {
        return;
    }
    if (state->output_worker.joinable()) {
        luma_nvenc_direct_request_stop(state);
        state->output_worker.join();
    } else if (state->encoder != nullptr && state->submitted_count.load(std::memory_order_acquire) != 0) {
        /* Stop is no longer latency-sensitive. Ensure submitted GPU copies and
         * NVENC outputs reach the transport before the worker emits EOF. */
        glFinish();
        for (unsigned wait_ms = 0; state->submitted_count.load(std::memory_order_acquire) != 0 && wait_ms < 2000;
             ++wait_ms) {
            const size_t before = state->submitted_count.load(std::memory_order_acquire);
            if (!poll(state)) break;
            if (state->submitted_count.load(std::memory_order_acquire) == before) usleep(1000);
        }
    }
    luma_nvenc_direct_stop_and_join(state);
    release_encoder_resources(state, true);
    debug_log("direct NVENC resources released on the presenting GL thread");
}

extern "C" void luma_nvenc_direct_stop_and_join(luma_nvenc_direct *state) {
    if (state == nullptr) return;
    luma_nvenc_direct_request_stop(state);
    if (state->output_worker.joinable()) state->output_worker.join();
    state->transport.stop.store(true, std::memory_order_release);
    state->transport.wake.notify_one();
    if (state->transport.worker.joinable() &&
        state->transport.worker.get_id() != std::this_thread::get_id()) {
        state->transport.worker.join();
    }
}

extern "C" void luma_nvenc_direct_release_gl(luma_nvenc_direct *state) {
    if (state == nullptr) {
        return;
    }
    std::array<GLuint, kSlots> framebuffers{};
    std::array<GLuint, kSlots> textures{};
    for (size_t index = 0; index < state->slots.size(); ++index) {
        if (state->slots[index].copy_fence != nullptr) {
            state->gl.delete_sync(state->slots[index].copy_fence);
            state->slots[index].copy_fence = nullptr;
        }
        state->slots[index].copy_pending = false;
        framebuffers[index] = state->slots[index].framebuffer;
        textures[index] = state->slots[index].texture;
        state->slots[index].framebuffer = 0;
        state->slots[index].texture = 0;
    }
    state->gl.delete_framebuffers(static_cast<GLsizei>(framebuffers.size()), framebuffers.data());
    glDeleteTextures(static_cast<GLsizei>(textures.size()), textures.data());
}

extern "C" void luma_nvenc_direct_destroy(luma_nvenc_direct *state) {
    if (state == nullptr) return;
    luma_nvenc_direct_stop_and_join(state);
    delete state;
}
