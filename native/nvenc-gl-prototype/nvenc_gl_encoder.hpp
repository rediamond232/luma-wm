#pragma once

#include <cstdint>
#include <functional>
#include <memory>

using GLuint = unsigned int;

namespace luma::nvenc_gl {

// A completed elementary-stream access unit. `bytes` is valid only during the
// callback; callers that queue it must copy it.
struct Packet {
    const std::uint8_t* bytes;
    std::uint32_t size;
    std::uint64_t pts;
    bool keyframe;
};

struct Config {
    std::uint32_t width;
    std::uint32_t height;
    std::uint32_t fps;
    std::uint32_t texture_count = 4;
    std::function<void(const Packet&)> on_packet;
};

// OpenGL-only NVENC session. The caller must have an EGL or GLX context current
// on the calling thread for its entire lifetime. Textures are GL_RGBA8 2D
// textures and must remain valid until shutdown().
class Encoder {
public:
    Encoder();
    ~Encoder();
    Encoder(const Encoder&) = delete;
    Encoder& operator=(const Encoder&) = delete;

    // Registers the supplied GL textures once. No CPU pixel transfer occurs.
    // `egl_context` is EGLContext cast to void* (eglGetCurrentContext()).
    bool initialize(void* egl_context, const Config&, const GLuint* textures, const char** error);

    // Submit an already-rendered texture. A non-blocking drain is attempted
    // first. Returns false when the ring is full; the hook should drop that
    // capture rather than stall the game's present thread.
    bool submit(std::uint32_t texture_index, std::uint64_t pts, bool force_idr, const char** error);

    // Attempts to collect all ready output without blocking. Call once per
    // present and before shutdown. Returns false only on a fatal NVENC error.
    bool poll(const char** error);
    bool shutdown(const char** error);

private:
    struct Impl;
    std::unique_ptr<Impl> impl_;
};

} // namespace luma::nvenc_gl
