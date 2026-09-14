#define GL_GLEXT_PROTOTYPES

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <drm_fourcc.h>
#include <xcb/composite.h>
#include <xcb/dri3.h>
#include <xcb/xcb.h>

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>
#include <unordered_set>
#include <unistd.h>

namespace {
template <typename Function>
Function egl_proc(const char *name) {
    const auto raw = eglGetProcAddress(name);
    Function function = nullptr;
    static_assert(sizeof(function) == sizeof(raw));
    std::memcpy(&function, &raw, sizeof(function));
    return function;
}

bool has_extension(const char *extensions, const char *wanted) {
    if (extensions == nullptr || wanted == nullptr || std::strchr(wanted, ' ') != nullptr) {
        return false;
    }
    const std::size_t length = std::strlen(wanted);
    for (const char *match = std::strstr(extensions, wanted); match != nullptr;
         match = std::strstr(match + length, wanted)) {
        if ((match == extensions || match[-1] == ' ') &&
            (match[length] == '\0' || match[length] == ' ')) {
            return true;
        }
    }
    return false;
}

std::uint64_t hash_pixels(const unsigned char *pixels, std::size_t length) {
    std::uint64_t hash = UINT64_C(1469598103934665603);
    for (std::size_t index = 0; index < length; ++index) {
        hash ^= pixels[index];
        hash *= UINT64_C(1099511628211);
    }
    return hash;
}
} // namespace

int main(int argc, char **argv) {
    if (argc < 2 || argc > 3) {
        std::fprintf(stderr, "Usage: %s WINDOW_ID [SECONDS]\n", argv[0]);
        return 64;
    }
    char *end = nullptr;
    errno = 0;
    const unsigned long parsed = std::strtoul(argv[1], &end, 0);
    const unsigned long seconds = argc == 3 ? std::strtoul(argv[2], &end, 10) : 3;
    if (errno != 0 || parsed == 0 || parsed > UINT32_MAX || seconds == 0 || seconds > 30) {
        return 64;
    }
    const xcb_window_t window = static_cast<xcb_window_t>(parsed);
    int screen = 0;
    xcb_connection_t *connection = xcb_connect(nullptr, &screen);
    if (connection == nullptr || xcb_connection_has_error(connection)) return 69;

    xcb_generic_error_t *error = nullptr;
    xcb_get_geometry_reply_t *geometry = xcb_get_geometry_reply(
        connection, xcb_get_geometry(connection, window), &error);
    if (geometry == nullptr) {
        std::fprintf(stderr, "window geometry failed (X11 error %u)\n",
                     error == nullptr ? 0 : error->error_code);
        std::free(error);
        xcb_disconnect(connection);
        return 65;
    }
    std::free(error);
    error = nullptr;

    const auto redirect = xcb_composite_redirect_window_checked(
        connection, window, XCB_COMPOSITE_REDIRECT_AUTOMATIC);
    error = xcb_request_check(connection, redirect);
    const bool redirected = error == nullptr;
    if (error != nullptr && error->error_code != XCB_ACCESS) {
        std::fprintf(stderr, "window redirect failed (X11 error %u)\n", error->error_code);
        std::free(error);
        std::free(geometry);
        xcb_disconnect(connection);
        return 70;
    }
    std::free(error);

    const xcb_pixmap_t pixmap = xcb_generate_id(connection);
    error = xcb_request_check(connection,
        xcb_composite_name_window_pixmap_checked(connection, window, pixmap));
    if (error != nullptr) {
        std::fprintf(stderr, "window pixmap naming failed (X11 error %u)\n", error->error_code);
        std::free(error);
        std::free(geometry);
        xcb_disconnect(connection);
        return 71;
    }

    auto reply = xcb_dri3_buffers_from_pixmap_reply(
        connection, xcb_dri3_buffers_from_pixmap(connection, pixmap), &error);
    if (reply == nullptr || error != nullptr || reply->nfd != 1) {
        std::fprintf(stderr, "DRI3 export failed (X11 error %u, planes %u)\n",
                     error == nullptr ? 0 : error->error_code,
                     reply == nullptr ? 0 : reply->nfd);
        std::free(error);
        std::free(reply);
        std::free(geometry);
        xcb_disconnect(connection);
        return 72;
    }
    int dmabuf = xcb_dri3_buffers_from_pixmap_reply_fds(connection, reply)[0];
    const std::uint32_t stride = xcb_dri3_buffers_from_pixmap_strides(reply)[0];
    const std::uint32_t offset = xcb_dri3_buffers_from_pixmap_offsets(reply)[0];

    EGLDisplay display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, nullptr, nullptr) ||
        !eglBindAPI(EGL_OPENGL_API)) {
        std::fprintf(stderr, "EGL display initialization failed (%#x)\n", eglGetError());
        return 73;
    }
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE,
    };
    EGLConfig config = nullptr;
    EGLint config_count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &config_count) ||
        config_count != 1) return 73;
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) return 73;

    const auto create_image = egl_proc<PFNEGLCREATEIMAGEKHRPROC>("eglCreateImageKHR");
    const auto destroy_image = egl_proc<PFNEGLDESTROYIMAGEKHRPROC>("eglDestroyImageKHR");
    const auto image_target =
        egl_proc<PFNGLEGLIMAGETARGETTEXTURE2DOESPROC>("glEGLImageTargetTexture2DOES");
    const char *extensions = eglQueryString(display, EGL_EXTENSIONS);
    if (create_image == nullptr || destroy_image == nullptr || image_target == nullptr ||
        !has_extension(extensions, "EGL_EXT_image_dma_buf_import") ||
        !has_extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers")) return 74;
    const EGLint image_attributes[] = {
        EGL_WIDTH, reply->width,
        EGL_HEIGHT, reply->height,
        EGL_LINUX_DRM_FOURCC_EXT, DRM_FORMAT_XRGB8888,
        EGL_DMA_BUF_PLANE0_FD_EXT, dmabuf,
        EGL_DMA_BUF_PLANE0_OFFSET_EXT, static_cast<EGLint>(offset),
        EGL_DMA_BUF_PLANE0_PITCH_EXT, static_cast<EGLint>(stride),
        EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, static_cast<EGLint>(reply->modifier & UINT32_MAX),
        EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, static_cast<EGLint>(reply->modifier >> 32),
        EGL_NONE,
    };
    EGLImageKHR image = create_image(display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT,
                                     nullptr, image_attributes);
    if (image == EGL_NO_IMAGE_KHR) {
        std::fprintf(stderr, "EGL DMA-BUF import failed (%#x)\n", eglGetError());
        return 75;
    }

    GLuint source_texture = 0, sample_texture = 0, source_fbo = 0, sample_fbo = 0;
    glGenTextures(1, &source_texture);
    glBindTexture(GL_TEXTURE_2D, source_texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    image_target(GL_TEXTURE_2D, image);
    glGenFramebuffers(1, &source_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, source_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           source_texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) return 76;

    constexpr GLsizei sample_width = 64;
    constexpr GLsizei sample_height = 64;
    glGenTextures(1, &sample_texture);
    glBindTexture(GL_TEXTURE_2D, sample_texture);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA8, sample_width, sample_height, 0,
                 GL_RGBA, GL_UNSIGNED_BYTE, nullptr);
    glGenFramebuffers(1, &sample_fbo);
    glBindFramebuffer(GL_FRAMEBUFFER, sample_fbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D,
                           sample_texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) return 76;

    unsigned char pixels[sample_width * sample_height * 4];
    std::unordered_set<std::uint64_t> hashes;
    std::uint64_t samples = 0, adjacent_identical = 0, previous = 0;
    const auto started = std::chrono::steady_clock::now();
    const auto deadline = started + std::chrono::seconds(seconds);
    auto next = started;
    while (std::chrono::steady_clock::now() < deadline) {
        glBindFramebuffer(GL_READ_FRAMEBUFFER, source_fbo);
        glBindFramebuffer(GL_DRAW_FRAMEBUFFER, sample_fbo);
        glBlitFramebuffer(0, 0, reply->width, reply->height,
                          0, 0, sample_width, sample_height, GL_COLOR_BUFFER_BIT, GL_NEAREST);
        glBindFramebuffer(GL_FRAMEBUFFER, sample_fbo);
        glReadPixels(0, 0, sample_width, sample_height, GL_RGBA, GL_UNSIGNED_BYTE, pixels);
        const std::uint64_t hash = hash_pixels(pixels, sizeof(pixels));
        if (samples != 0 && hash == previous) ++adjacent_identical;
        previous = hash;
        hashes.insert(hash);
        ++samples;
        next += std::chrono::nanoseconds(UINT64_C(1000000000) / 480);
        std::this_thread::sleep_until(next);
    }
    const double elapsed = std::chrono::duration<double>(
        std::chrono::steady_clock::now() - started).count();
    std::printf("window=%#x image=%ux%u modifier=%#llx elapsed=%.3f samples=%llu "
                "sample_rate=%.2f unique=%zu unique_rate=%.2f adjacent_identical=%llu\n",
                window, reply->width, reply->height,
                static_cast<unsigned long long>(reply->modifier), elapsed,
                static_cast<unsigned long long>(samples), samples / elapsed, hashes.size(),
                hashes.size() / elapsed, static_cast<unsigned long long>(adjacent_identical));

    glDeleteFramebuffers(1, &sample_fbo);
    glDeleteFramebuffers(1, &source_fbo);
    glDeleteTextures(1, &sample_texture);
    glDeleteTextures(1, &source_texture);
    destroy_image(display, image);
    eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroySurface(display, surface);
    eglDestroyContext(display, context);
    eglTerminate(display);
    close(dmabuf);
    std::free(reply);
    xcb_free_pixmap(connection, pixmap);
    if (redirected) {
        xcb_composite_unredirect_window(connection, window,
                                        XCB_COMPOSITE_REDIRECT_AUTOMATIC);
    }
    xcb_flush(connection);
    std::free(geometry);
    xcb_disconnect(connection);
    return 0;
}
