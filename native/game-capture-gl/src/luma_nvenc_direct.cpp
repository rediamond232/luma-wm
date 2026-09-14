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

#include <array>
#include <atomic>
#include <condition_variable>
#include <cstdarg>
#include <cstdio>
#include <cstdlib>
#include <cstdint>
#include <cstring>
#include <mutex>
#include <new>
#include <string>
#include <thread>
#include <vector>

namespace {
constexpr uint32_t kMagic = 0x4c474350; // LGCP, integer fields use network/big endian.
constexpr uint16_t kVersion = 1;
constexpr uint16_t kHello = 1, kServerHello = 2, kStart = 3, kAccessUnit = 4;
constexpr size_t kMaxAu = 64U * 1024U * 1024U;
constexpr size_t kQueueMax = 8;
constexpr size_t kAccessUnitHeaderBytes = 28;
// The producer is the game's present thread.  Reserve a small, fixed amount
// of RAM for its compressed-AU handoff instead of growing a container there.
// An AU larger than this deliberately drops: allowing it to allocate or wait
// would turn a transient IDR spike into a game-frame hitch.  The wire protocol
// still accepts up to kMaxAu; that remains the receiver's validation limit.
constexpr size_t kPacketSlotBytes = 8U * 1024U * 1024U;
constexpr size_t kSlots = 4;

bool debug_enabled() {
    static const bool enabled = [] {
        const char *value = getenv("LUMA_GAME_CAPTURE_DEBUG");
        return value && value[0] != '\0' && strcmp(value, "0") != 0;
    }();
    return enabled;
}

void debug_log(const char *format, ...) {
    if (!debug_enabled()) return;
    va_list args;
    va_start(args, format);
    fputs("luma game capture: ", stderr);
    vfprintf(stderr, format, args);
    fputc('\n', stderr);
    va_end(args);
}

uint32_t capture_qp(uint32_t requested) {
    constexpr uint32_t kDefaultQp = 20;
    return requested >= 1 && requested <= 51 ? requested : kDefaultQp;
}

uint16_t be16(uint16_t n) { return __builtin_bswap16(n); }
uint32_t be32(uint32_t n) { return __builtin_bswap32(n); }
uint64_t be64(uint64_t n) { return __builtin_bswap64(n); }

bool write_all(int fd, const uint8_t *data, size_t length) {
    while (length) {
        const ssize_t wrote = write(fd, data, length);
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
    bool busy{};
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
};

GlCopyFunctions resolve_gl_copy_functions() {
    return {
        resolve_gl_function<BindFramebuffer>("glBindFramebuffer"),
        resolve_gl_function<GenFramebuffers>("glGenFramebuffers"),
        resolve_gl_function<DeleteFramebuffers>("glDeleteFramebuffers"),
        resolve_gl_function<FramebufferTexture2D>("glFramebufferTexture2D"),
        resolve_gl_function<CheckFramebufferStatus>("glCheckFramebufferStatus"),
        resolve_gl_function<BlitFramebuffer>("glBlitFramebuffer"),
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
    std::array<size_t, kSlots> submitted{};
    size_t submitted_head{};
    size_t submitted_count{};
    size_t next{};
    bool initialized{};
    bool first_frame{true};
    std::atomic<bool> disabled{false};
    std::atomic<bool> stop_requested{false};
    std::atomic<bool> cleanup_started{false};
    std::atomic<bool> logged_failure{false};
    uint64_t next_due_ns{};
    uint64_t frame_interval_ns{};
    uint32_t width{}, height{}, fps{};
    GlCopyFunctions gl{};
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
    while (state->submitted_count != 0) {
        const size_t slot_index = state->submitted[state->submitted_head];
        Slot &slot = state->slots[slot_index];
        if (!slot.busy) {
            disable(state, "internal output ordering", NV_ENC_ERR_INVALID_CALL);
            return false;
        }
        NV_ENC_LOCK_BITSTREAM lock{}; lock.version = NV_ENC_LOCK_BITSTREAM_VER; lock.outputBitstream = slot.bitstream;
        /* Linux uses synchronous NVENC output in this path. doNotWait is an
         * asynchronous-mode contract and can expose an incompletely finalized
         * access unit on some driver versions when used with enableEncodeAsync
         * disabled. Let the driver complete the oldest submitted output. */
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
        // The only CPU copy is compressed H.264 into a small bounded handoff;
        // no frame pixels are copied/read back on the presenting thread.
        const bool queued = state->transport.enqueue(
            static_cast<const uint8_t *>(lock.bitstreamBufferPtr),
            lock.bitstreamSizeInBytes, lock.outputTimeStamp,
            lock.pictureType == NV_ENC_PIC_TYPE_IDR);
        const NVENCSTATUS unlock = state->api.nvEncUnlockBitstream(state->encoder, slot.bitstream);
        const NVENCSTATUS unmap = state->api.nvEncUnmapInputResource(state->encoder, slot.mapped);
        slot.mapped = nullptr; slot.busy = false;
        state->submitted_head = (state->submitted_head + 1) % kSlots;
        --state->submitted_count;
        if (!ok(unlock)) {
            disable(state, "NvEncUnlockBitstream", unlock);
            return false;
        }
        if (!ok(unmap)) {
            disable(state, "NvEncUnmapInputResource", unmap);
            return false;
        }
        if (!queued) {
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
}

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
        gl.blit_framebuffer == nullptr) {
        debug_log("direct NVENC could not resolve the OpenGL framebuffer copy functions");
        return nullptr;
    }
    // The hook retains this state until process exit: NVENC/GL destruction must
    // happen with the original context current, and no present call may join a
    // socket thread. A failed stream is disabled below and no further frames are
    // copied or encoded.
    auto *state = new luma_nvenc_direct; state->width = width; state->height = height; state->fps = fps; state->frame_interval_ns = UINT64_C(1000000000) / fps; state->gl = gl; state->transport.width = width; state->transport.height = height; state->transport.fps = fps; state->transport.path = socket_path;
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
    if (state->next_due_ns != 0 && pts_ns < state->next_due_ns) return 0;
    if (state->next_due_ns == 0 || pts_ns - state->next_due_ns >= state->frame_interval_ns) state->next_due_ns = pts_ns + state->frame_interval_ns;
    else state->next_due_ns += state->frame_interval_ns;
    if (!poll(state)) return 0;
    /* Reserve transport capacity conceptually before encoding. Every existing
     * NVENC submission will later occupy one queue entry, and the new frame
     * adds one more. This keeps overload drops ahead of the codec reference
     * chain instead of discarding an encoded access unit afterward. */
    if (!state->transport.can_admit(state->submitted_count)) return 0;
    Slot &slot = state->slots[state->next];
    if (slot.busy || state->submitted_count == kSlots) return 0;
    if (!copy_framebuffer(state->gl, source_framebuffer, source_read_buffer, slot.framebuffer,
                          state->width, state->height, flip_y)) {
        disable(state, "copy from source read framebuffer", NV_ENC_ERR_INVALID_CALL);
        return 0;
    }
    // NVENC's OpenGL device shares the current context. Flush submits the GPU
    // copy before NvEncMapInputResource without CPU-side glFinish/readback.
    glFlush();
    NV_ENC_MAP_INPUT_RESOURCE map{}; map.version = NV_ENC_MAP_INPUT_RESOURCE_VER; map.registeredResource = slot.registered;
    const NVENCSTATUS mapped = state->api.nvEncMapInputResource(state->encoder, &map);
    if (!ok(mapped)) { disable(state, "NvEncMapInputResource", mapped); return 0; }
    slot.mapped = map.mappedResource;
    NV_ENC_PIC_PARAMS picture{}; picture.version = NV_ENC_PIC_PARAMS_VER; picture.inputWidth = state->width; picture.inputHeight = state->height; picture.inputPitch = state->width; picture.inputBuffer = slot.mapped; picture.bufferFmt = map.mappedBufferFmt; picture.outputBitstream = slot.bitstream; picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME; picture.inputTimeStamp = pts_ns; picture.inputDuration = 1; if (state->first_frame) picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR;
    const NVENCSTATUS encoded = state->api.nvEncEncodePicture(state->encoder, &picture);
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
    slot.busy = true;
    state->submitted[(state->submitted_head + state->submitted_count) % kSlots] = state->next;
    ++state->submitted_count;
    state->first_frame = false;
    state->next = (state->next + 1) % kSlots;
    return 1;
}
}

extern "C" int luma_nvenc_direct_submit(luma_nvenc_direct *state, uint32_t source_width,
                                           uint32_t source_height, uint64_t pts_ns) {
    return submit_framebuffer(state, 0, GL_BACK, source_width, source_height, true, pts_ns);
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
}

extern "C" int luma_nvenc_direct_stop_requested(luma_nvenc_direct *state) {
    return state != nullptr && state->stop_requested.load(std::memory_order_acquire);
}

extern "C" void luma_nvenc_direct_finish_on_gl_thread(luma_nvenc_direct *state) {
    if (state == nullptr || state->cleanup_started.exchange(true, std::memory_order_acq_rel)) {
        return;
    }
    if (state->encoder != nullptr && state->submitted_count != 0) {
        /* Stop is no longer latency-sensitive. Ensure submitted GPU copies and
         * NVENC outputs reach the transport before the worker emits EOF. */
        glFinish();
        for (unsigned wait_ms = 0; state->submitted_count != 0 && wait_ms < 2000;
             ++wait_ms) {
            const size_t before = state->submitted_count;
            if (!poll(state)) break;
            if (state->submitted_count == before) usleep(1000);
        }
    }
    luma_nvenc_direct_stop_and_join(state);

    if (state->encoder != nullptr) {
        for (Slot &slot : state->slots) {
            if (slot.mapped != nullptr && state->api.nvEncUnmapInputResource != nullptr) {
                (void)state->api.nvEncUnmapInputResource(state->encoder, slot.mapped);
                slot.mapped = nullptr;
            }
            slot.busy = false;
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

    std::array<GLuint, kSlots> framebuffers{};
    std::array<GLuint, kSlots> textures{};
    for (size_t index = 0; index < state->slots.size(); ++index) {
        framebuffers[index] = state->slots[index].framebuffer;
        textures[index] = state->slots[index].texture;
        state->slots[index].framebuffer = 0;
        state->slots[index].texture = 0;
    }
    state->gl.delete_framebuffers(static_cast<GLsizei>(framebuffers.size()), framebuffers.data());
    glDeleteTextures(static_cast<GLsizei>(textures.size()), textures.data());

    if (state->encoder != nullptr && state->api.nvEncDestroyEncoder != nullptr) {
        (void)state->api.nvEncDestroyEncoder(state->encoder);
        state->encoder = nullptr;
    }
    if (state->library != nullptr) {
        dlclose(state->library);
        state->library = nullptr;
    }
    state->initialized = false;
    state->submitted_count = 0;
    debug_log("direct NVENC resources released on the presenting GL thread");
}

extern "C" void luma_nvenc_direct_stop_and_join(luma_nvenc_direct *state) {
    if (state == nullptr) return;
    luma_nvenc_direct_request_stop(state);
    state->transport.stop.store(true, std::memory_order_release);
    state->transport.wake.notify_one();
    if (state->transport.worker.joinable() &&
        state->transport.worker.get_id() != std::this_thread::get_id()) {
        state->transport.worker.join();
    }
}
