#define GL_GLEXT_PROTOTYPES

#include "../game-capture-gl/src/luma_nvenc_direct.h"

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>
#include <drm_fourcc.h>
#include <xcb/composite.h>
#include <xcb/dri3.h>
#include <xcb/present.h>
#include <xcb/xcb.h>

#include <array>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <poll.h>
#include <string>
#include <sys/stat.h>
#include <thread>
#include <unistd.h>

namespace {
constexpr std::size_t kImportSlots = 8;
std::atomic<bool> stopping{false};

void stop_handler(int) { stopping.store(true, std::memory_order_release); }

template <typename Function>
Function egl_proc(const char *name) {
    const auto raw = eglGetProcAddress(name);
    Function function = nullptr;
    static_assert(sizeof(function) == sizeof(raw));
    std::memcpy(&function, &raw, sizeof(function));
    return function;
}

bool has_extension(const char *extensions, const char *wanted) {
    if (!extensions || !wanted || std::strchr(wanted, ' ')) return false;
    const std::size_t length = std::strlen(wanted);
    for (const char *match = std::strstr(extensions, wanted); match;
         match = std::strstr(match + length, wanted)) {
        if ((match == extensions || match[-1] == ' ') &&
            (match[length] == '\0' || match[length] == ' ')) return true;
    }
    return false;
}

std::uint64_t monotonic_ns() {
    return std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
}

long positive_number(const char *text) {
    char *end = nullptr;
    errno = 0;
    const long value = std::strtol(text, &end, 0);
    return errno == 0 && end != text && *end == '\0' && value > 0 ? value : -1;
}

struct ExportedFrame {
    std::uint32_t width{};
    std::uint32_t height{};
    std::uint32_t fourcc{};
    std::uint32_t stride{};
    std::uint32_t offset{};
    std::uint64_t modifier{};
    int fd{-1};
};

struct EglState {
    EGLDisplay display{EGL_NO_DISPLAY};
    EGLContext context{EGL_NO_CONTEXT};
    EGLSurface surface{EGL_NO_SURFACE};
    PFNEGLCREATEIMAGEKHRPROC create_image{};
    PFNEGLDESTROYIMAGEKHRPROC destroy_image{};
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target_texture{};
    bool modifier_import{};
};

void destroy_egl(EglState &egl) {
    if (egl.display != EGL_NO_DISPLAY) {
        (void)eglMakeCurrent(egl.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        if (egl.surface != EGL_NO_SURFACE) (void)eglDestroySurface(egl.display, egl.surface);
        if (egl.context != EGL_NO_CONTEXT) (void)eglDestroyContext(egl.display, egl.context);
        (void)eglTerminate(egl.display);
    }
    egl = {};
}

bool create_context(EGLDisplay display, EglState &out) {
    if (display == EGL_NO_DISPLAY || !eglInitialize(display, nullptr, nullptr) ||
        !eglBindAPI(EGL_OPENGL_API)) return false;
    const EGLint config_attributes[] = {
        EGL_SURFACE_TYPE, EGL_PBUFFER_BIT,
        EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8,
        EGL_NONE,
    };
    EGLConfig config = nullptr;
    EGLint count = 0;
    if (!eglChooseConfig(display, config_attributes, &config, 1, &count) || count != 1) {
        (void)eglTerminate(display);
        return false;
    }
    const EGLint context_attributes[] = {
        EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 0, EGL_NONE,
    };
    const EGLint surface_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    EGLSurface surface = context == EGL_NO_CONTEXT
        ? EGL_NO_SURFACE : eglCreatePbufferSurface(display, config, surface_attributes);
    if (context == EGL_NO_CONTEXT || surface == EGL_NO_SURFACE ||
        !eglMakeCurrent(display, surface, surface, context)) {
        if (surface != EGL_NO_SURFACE) (void)eglDestroySurface(display, surface);
        if (context != EGL_NO_CONTEXT) (void)eglDestroyContext(display, context);
        (void)eglTerminate(display);
        return false;
    }
    const char *extensions = eglQueryString(display, EGL_EXTENSIONS);
    if (!has_extension(extensions, "EGL_EXT_image_dma_buf_import")) {
        EglState temporary{display, context, surface};
        destroy_egl(temporary);
        return false;
    }
    out.display = display;
    out.context = context;
    out.surface = surface;
    out.create_image = egl_proc<PFNEGLCREATEIMAGEKHRPROC>("eglCreateImageKHR");
    out.destroy_image = egl_proc<PFNEGLDESTROYIMAGEKHRPROC>("eglDestroyImageKHR");
    out.image_target_texture =
        egl_proc<PFNGLEGLIMAGETARGETTEXTURE2DOESPROC>("glEGLImageTargetTexture2DOES");
    out.modifier_import = has_extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers");
    if (out.create_image && out.destroy_image && out.image_target_texture) return true;
    destroy_egl(out);
    return false;
}

EGLImageKHR import_image(EglState &egl, const ExportedFrame &frame) {
    EGLint attributes[24];
    std::size_t count = 0;
    const auto add = [&](EGLint name, EGLint value) {
        attributes[count++] = name;
        attributes[count++] = value;
    };
    add(EGL_WIDTH, static_cast<EGLint>(frame.width));
    add(EGL_HEIGHT, static_cast<EGLint>(frame.height));
    add(EGL_LINUX_DRM_FOURCC_EXT, static_cast<EGLint>(frame.fourcc));
    add(EGL_DMA_BUF_PLANE0_FD_EXT, frame.fd);
    add(EGL_DMA_BUF_PLANE0_OFFSET_EXT, static_cast<EGLint>(frame.offset));
    add(EGL_DMA_BUF_PLANE0_PITCH_EXT, static_cast<EGLint>(frame.stride));
    if (egl.modifier_import && frame.modifier != UINT64_MAX) {
        add(EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            static_cast<EGLint>(frame.modifier & UINT32_MAX));
        add(EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            static_cast<EGLint>(frame.modifier >> 32));
    }
    attributes[count] = EGL_NONE;
    return egl.create_image(egl.display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT,
                            nullptr, attributes);
}

bool initialize_for_image(EglState &egl, const ExportedFrame &frame, EGLImageKHR &image) {
    const auto query_devices = egl_proc<PFNEGLQUERYDEVICESEXTPROC>("eglQueryDevicesEXT");
    const auto platform_display =
        egl_proc<PFNEGLGETPLATFORMDISPLAYEXTPROC>("eglGetPlatformDisplayEXT");
    if (query_devices && platform_display) {
        EGLDeviceEXT devices[16];
        EGLint count = 0;
        if (query_devices(16, devices, &count)) {
            for (EGLint index = 0; index < count; ++index) {
                EglState candidate;
                EGLDisplay display = platform_display(EGL_PLATFORM_DEVICE_EXT, devices[index], nullptr);
                if (!create_context(display, candidate)) continue;
                image = import_image(candidate, frame);
                if (image != EGL_NO_IMAGE_KHR) {
                    egl = candidate;
                    return true;
                }
                destroy_egl(candidate);
            }
        }
    }
    if (platform_display) {
        EglState candidate;
        EGLDisplay display = platform_display(EGL_PLATFORM_SURFACELESS_MESA,
                                              EGL_DEFAULT_DISPLAY, nullptr);
        if (create_context(display, candidate)) {
            image = import_image(candidate, frame);
            if (image != EGL_NO_IMAGE_KHR) {
                egl = candidate;
                return true;
            }
            destroy_egl(candidate);
        }
    }
    return false;
}

struct ImportSlot {
    GLuint texture{};
    GLuint framebuffer{};
    EGLImageKHR image{EGL_NO_IMAGE_KHR};
    GLsync fence{};
};

void release_slot(EglState &egl, ImportSlot &slot) {
    if (slot.fence) glDeleteSync(slot.fence);
    if (slot.image != EGL_NO_IMAGE_KHR) (void)egl.destroy_image(egl.display, slot.image);
    slot.fence = nullptr;
    slot.image = EGL_NO_IMAGE_KHR;
}

ImportSlot *available_slot(EglState &egl, std::array<ImportSlot, kImportSlots> &slots) {
    for (ImportSlot &slot : slots) {
        if (!slot.fence) return &slot;
        const GLenum state = glClientWaitSync(slot.fence, 0, 0);
        if (state == GL_ALREADY_SIGNALED || state == GL_CONDITION_SATISFIED) {
            release_slot(egl, slot);
            return &slot;
        }
    }
    return nullptr;
}

bool export_window_frame(xcb_connection_t *connection, xcb_window_t window,
                         ExportedFrame &out) {
    const xcb_pixmap_t pixmap = xcb_generate_id(connection);
    xcb_generic_error_t *error = nullptr;
    error = xcb_request_check(connection,
        xcb_composite_name_window_pixmap_checked(connection, window, pixmap));
    if (error) {
        std::free(error);
        return false;
    }
    auto *reply = xcb_dri3_buffers_from_pixmap_reply(
        connection, xcb_dri3_buffers_from_pixmap(connection, pixmap), &error);
    xcb_free_pixmap(connection, pixmap);
    xcb_flush(connection);
    if (!reply || error || reply->nfd != 1 || reply->width == 0 || reply->height == 0) {
        std::free(error);
        std::free(reply);
        return false;
    }
    const std::uint32_t fourcc = reply->depth == 32 ? DRM_FORMAT_ARGB8888
                                                    : DRM_FORMAT_XRGB8888;
    out = {
        reply->width,
        reply->height,
        fourcc,
        xcb_dri3_buffers_from_pixmap_strides(reply)[0],
        xcb_dri3_buffers_from_pixmap_offsets(reply)[0],
        reply->modifier,
        xcb_dri3_buffers_from_pixmap_reply_fds(connection, reply)[0],
    };
    std::free(reply);
    return true;
}

bool wait_for_socket(const char *path) {
    struct stat metadata {};
    for (unsigned attempt = 0; attempt < 500 && !stopping.load(); ++attempt) {
        if (stat(path, &metadata) == 0 && S_ISSOCK(metadata.st_mode)) return true;
        std::this_thread::sleep_for(std::chrono::milliseconds(10));
    }
    return false;
}
} // namespace

int main(int argc, char **argv) {
    const char *socket_path = nullptr;
    const char *token = nullptr;
    long window_number = -1, fps = -1, quality = 20;
    for (int index = 1; index < argc; ++index) {
        const auto value = [&](const char *option) -> const char * {
            if (std::strcmp(argv[index], option) != 0 || index + 1 >= argc) return nullptr;
            return argv[++index];
        };
        if (const char *text = value("--window")) window_number = positive_number(text);
        else if (const char *text = value("--stream-socket")) socket_path = text;
        else if (const char *text = value("--token")) token = text;
        else if (const char *text = value("--fps")) fps = positive_number(text);
        else if (const char *text = value("--quality")) quality = positive_number(text);
        else {
            std::fputs("invalid Xwayland capture arguments\n", stderr);
            return 64;
        }
    }
    if (window_number <= 0 || window_number > UINT32_MAX || !socket_path || !token ||
        std::strlen(token) != 64 || fps < 30 || fps > 480 || quality < 1 || quality > 51) {
        std::fputs("Xwayland capture requires window, socket, token, FPS 30..480 and quality 1..51\n",
                   stderr);
        return 64;
    }
    std::signal(SIGINT, stop_handler);
    std::signal(SIGTERM, stop_handler);
    if (!wait_for_socket(socket_path)) {
        std::fputs("Xwayland capture muxer socket did not appear\n", stderr);
        return 70;
    }

    int screen = 0;
    xcb_connection_t *connection = xcb_connect(nullptr, &screen);
    if (!connection || xcb_connection_has_error(connection)) {
        std::fputs("cannot connect to Luma Xwayland display\n", stderr);
        return 69;
    }
    const auto *composite = xcb_get_extension_data(connection, &xcb_composite_id);
    const auto *dri3 = xcb_get_extension_data(connection, &xcb_dri3_id);
    const auto *present = xcb_get_extension_data(connection, &xcb_present_id);
    if (!composite || !composite->present || !dri3 || !dri3->present ||
        !present || !present->present) {
        std::fputs("Xwayland requires Composite, DRI3, and Present for direct capture\n", stderr);
        xcb_disconnect(connection);
        return 69;
    }
    const xcb_window_t window = static_cast<xcb_window_t>(window_number);
    xcb_generic_error_t *error = nullptr;
    auto *geometry = xcb_get_geometry_reply(connection, xcb_get_geometry(connection, window), &error);
    if (!geometry) {
        std::fprintf(stderr, "Xwayland target window is unavailable (X11 error %u)\n",
                     error ? error->error_code : 0);
        std::free(error);
        xcb_disconnect(connection);
        return 65;
    }
    std::free(error);
    error = nullptr;
    if (geometry->width < 2 || geometry->height < 2) {
        std::fputs("Xwayland capture target is minimized or too small\n", stderr);
        std::free(geometry);
        xcb_disconnect(connection);
        return 65;
    }
    const xcb_void_cookie_t redirect = xcb_composite_redirect_window_checked(
        connection, window, XCB_COMPOSITE_REDIRECT_AUTOMATIC);
    error = xcb_request_check(connection, redirect);
    const bool redirected = !error;
    if (error && error->error_code != XCB_ACCESS) {
        std::fprintf(stderr, "cannot redirect Xwayland window (X11 error %u)\n", error->error_code);
        std::free(error);
        std::free(geometry);
        xcb_disconnect(connection);
        return 70;
    }
    std::free(error);

    const xcb_present_event_t event_id = xcb_generate_id(connection);
    error = xcb_request_check(connection, xcb_present_select_input_checked(
        connection, event_id, window, XCB_PRESENT_EVENT_MASK_COMPLETE_NOTIFY));
    if (error) {
        std::fprintf(stderr, "cannot observe Xwayland presents (X11 error %u)\n", error->error_code);
        std::free(error);
        std::free(geometry);
        xcb_disconnect(connection);
        return 70;
    }
    xcb_flush(connection);

    EglState egl;
    std::array<ImportSlot, kImportSlots> slots{};
    luma_nvenc_direct *encoder = nullptr;
    const std::uint32_t source_width = geometry->width, source_height = geometry->height;
    // NVENC requires even dimensions. Crop at most the final row/column rather
    // than rejecting an otherwise valid odd-sized game window.
    const std::uint32_t width = source_width & ~1U, height = source_height & ~1U;
    std::free(geometry);
    const std::uint64_t interval = UINT64_C(1000000000) / static_cast<std::uint64_t>(fps);
    std::uint64_t next_due = 0, presents = 0, admitted = 0, encoded = 0;
    std::uint64_t export_drops = 0, pool_drops = 0;

    while (!stopping.load(std::memory_order_acquire)) {
        pollfd descriptor{xcb_get_file_descriptor(connection), POLLIN, 0};
        const int ready = poll(&descriptor, 1, 100);
        if (ready < 0 && errno != EINTR) break;
        xcb_generic_event_t *event = nullptr;
        while (!stopping.load(std::memory_order_acquire) &&
               (event = xcb_poll_for_event(connection)) != nullptr) {
            if ((event->response_type & 0x7fU) == XCB_GE_GENERIC) {
                const auto *complete = reinterpret_cast<xcb_present_complete_notify_event_t *>(event);
                if (complete->extension == present->major_opcode &&
                    complete->event_type == XCB_PRESENT_COMPLETE_NOTIFY &&
                    complete->event == event_id &&
                    complete->kind == XCB_PRESENT_COMPLETE_KIND_PIXMAP) {
                    ++presents;
                    const std::uint64_t now = monotonic_ns();
                    if (next_due && now < next_due) {
                        std::free(event);
                        continue;
                    }
                    next_due = (!next_due || now - next_due >= interval) ? now + interval
                                                                         : next_due + interval;
                    ++admitted;
                    ExportedFrame frame;
                    if (!export_window_frame(connection, window, frame) ||
                        frame.width != source_width || frame.height != source_height) {
                        if (frame.fd >= 0) close(frame.fd);
                        ++export_drops;
                        std::free(event);
                        continue;
                    }
                    EGLImageKHR image = EGL_NO_IMAGE_KHR;
                    if (egl.display == EGL_NO_DISPLAY) {
                        if (!initialize_for_image(egl, frame, image)) {
                            close(frame.fd);
                            std::fputs("no EGL device can import the Xwayland DMA-BUF\n", stderr);
                            stopping.store(true);
                            std::free(event);
                            continue;
                        }
                        // ImportSlot is not a packed GLuint array; generate explicitly.
                        std::array<GLuint, kImportSlots> textures{}, framebuffers{};
                        glGenTextures(static_cast<GLsizei>(textures.size()), textures.data());
                        glGenFramebuffers(static_cast<GLsizei>(framebuffers.size()), framebuffers.data());
                        for (std::size_t index = 0; index < slots.size(); ++index) {
                            slots[index].texture = textures[index];
                            slots[index].framebuffer = framebuffers[index];
                        }
                    }
                    ImportSlot *slot = available_slot(egl, slots);
                    if (!slot) {
                        if (image != EGL_NO_IMAGE_KHR)
                            (void)egl.destroy_image(egl.display, image);
                        close(frame.fd);
                        ++pool_drops;
                        std::free(event);
                        continue;
                    }
                    if (image == EGL_NO_IMAGE_KHR) image = import_image(egl, frame);
                    close(frame.fd);
                    if (image == EGL_NO_IMAGE_KHR) {
                        ++export_drops;
                        std::free(event);
                        continue;
                    }
                    slot->image = image;
                    glBindTexture(GL_TEXTURE_2D, slot->texture);
                    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
                    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
                    egl.image_target_texture(GL_TEXTURE_2D, image);
                    glBindFramebuffer(GL_FRAMEBUFFER, slot->framebuffer);
                    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                           GL_TEXTURE_2D, slot->texture, 0);
                    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
                        release_slot(egl, *slot);
                        ++export_drops;
                        std::free(event);
                        continue;
                    }
                    if (!encoder) {
                        encoder = luma_nvenc_direct_create(width, height,
                            static_cast<std::uint32_t>(fps), static_cast<std::uint32_t>(quality),
                            socket_path, token);
                        if (!encoder || !luma_nvenc_direct_wait_ready(encoder, 2000)) {
                            std::fputs("Xwayland NVENC transport did not become ready\n", stderr);
                            stopping.store(true);
                            std::free(event);
                            continue;
                        }
                    }
                    if (luma_nvenc_direct_submit_framebuffer(
                            encoder, slot->framebuffer, width, height, 0, now)) {
                        ++encoded;
                        slot->fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
                        glFlush();
                        if (!slot->fence) release_slot(egl, *slot);
                    } else {
                        // A fatal NVENC failure can occur after the GPU blit
                        // was submitted. This is an exceptional shutdown path;
                        // finish it before releasing the imported source.
                        glFinish();
                        release_slot(egl, *slot);
                        ++pool_drops;
                    }
                }
            }
            std::free(event);
        }
        if (xcb_connection_has_error(connection)) break;
    }

    if (egl.display != EGL_NO_DISPLAY) glFinish();
    if (encoder) luma_nvenc_direct_finish_on_gl_thread(encoder);
    if (egl.display != EGL_NO_DISPLAY) {
        std::array<GLuint, kImportSlots> textures{}, framebuffers{};
        for (std::size_t index = 0; index < slots.size(); ++index) {
            release_slot(egl, slots[index]);
            textures[index] = slots[index].texture;
            framebuffers[index] = slots[index].framebuffer;
        }
        glDeleteFramebuffers(static_cast<GLsizei>(framebuffers.size()), framebuffers.data());
        glDeleteTextures(static_cast<GLsizei>(textures.size()), textures.data());
    }
    destroy_egl(egl);
    if (redirected) xcb_composite_unredirect_window(connection, window,
                                                     XCB_COMPOSITE_REDIRECT_AUTOMATIC);
    xcb_disconnect(connection);
    std::fprintf(stderr,
        "Luma Xwayland capture saw %llu presents, admitted %llu, encoded %llu, "
        "export drops %llu, pool drops %llu\n",
        static_cast<unsigned long long>(presents), static_cast<unsigned long long>(admitted),
        static_cast<unsigned long long>(encoded), static_cast<unsigned long long>(export_drops),
        static_cast<unsigned long long>(pool_drops));
    return encoder && encoded ? 0 : 74;
}
