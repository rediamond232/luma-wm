#define GL_GLEXT_PROTOTYPES

#include "luma_vk_export_protocol.h"
#include "../game-capture-gl/src/luma_nvenc_direct.h"

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <GL/glext.h>

#include <algorithm>
#include <atomic>
#include <array>
#include <cerrno>
#include <climits>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <poll.h>
#include <string>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

namespace {
std::atomic<bool> stopping{false};

void stop_handler(int) { stopping.store(true, std::memory_order_release); }

template <typename Function>
Function egl_proc(const char *name) {
    const auto raw = eglGetProcAddress(name);
    Function function = nullptr;
    static_assert(sizeof(function) == sizeof(raw));
    memcpy(&function, &raw, sizeof(function));
    return function;
}

struct EglState {
    EGLDisplay display{EGL_NO_DISPLAY};
    EGLContext context{EGL_NO_CONTEXT};
    EGLSurface surface{EGL_NO_SURFACE};
    PFNEGLCREATEIMAGEKHRPROC create_image{};
    PFNEGLDESTROYIMAGEKHRPROC destroy_image{};
    PFNEGLCREATESYNCKHRPROC create_sync{};
    PFNEGLDESTROYSYNCKHRPROC destroy_sync{};
    PFNEGLWAITSYNCKHRPROC wait_sync{};
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

bool extension_present(const char *extensions, const char *wanted) {
    if (!extensions || !wanted || strchr(wanted, ' ')) return false;
    const size_t length = strlen(wanted);
    for (const char *match = strstr(extensions, wanted); match; match = strstr(match + length, wanted)) {
        if ((match == extensions || match[-1] == ' ') &&
            (match[length] == '\0' || match[length] == ' ')) return true;
    }
    return false;
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
        EGL_CONTEXT_MAJOR_VERSION, 3,
        EGL_CONTEXT_MINOR_VERSION, 0,
        EGL_NONE,
    };
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, context_attributes);
    const EGLint surface_attributes[] = {EGL_WIDTH, 1, EGL_HEIGHT, 1, EGL_NONE};
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
    if (!extension_present(extensions, "EGL_EXT_image_dma_buf_import") ||
        !extension_present(extensions, "EGL_ANDROID_native_fence_sync")) {
        EglState temporary{display, context, surface};
        destroy_egl(temporary);
        return false;
    }
    out.display = display;
    out.context = context;
    out.surface = surface;
    out.create_image = egl_proc<PFNEGLCREATEIMAGEKHRPROC>("eglCreateImageKHR");
    out.destroy_image = egl_proc<PFNEGLDESTROYIMAGEKHRPROC>("eglDestroyImageKHR");
    out.create_sync = egl_proc<PFNEGLCREATESYNCKHRPROC>("eglCreateSyncKHR");
    out.destroy_sync = egl_proc<PFNEGLDESTROYSYNCKHRPROC>("eglDestroySyncKHR");
    out.wait_sync = egl_proc<PFNEGLWAITSYNCKHRPROC>("eglWaitSyncKHR");
    out.image_target_texture =
        egl_proc<PFNGLEGLIMAGETARGETTEXTURE2DOESPROC>("glEGLImageTargetTexture2DOES");
    out.modifier_import =
        extension_present(extensions, "EGL_EXT_image_dma_buf_import_modifiers");
    if (out.create_image && out.destroy_image && out.create_sync && out.destroy_sync &&
        out.wait_sync && out.image_target_texture) return true;
    destroy_egl(out);
    return false;
}

EGLImageKHR import_image(EglState &egl, const luma_vk_export_message &message, int dmabuf) {
    EGLint attributes[20];
    size_t count = 0;
    auto add = [&](EGLint name, EGLint value) {
        attributes[count++] = name;
        attributes[count++] = value;
    };
    add(EGL_WIDTH, static_cast<EGLint>(message.width));
    add(EGL_HEIGHT, static_cast<EGLint>(message.height));
    add(EGL_LINUX_DRM_FOURCC_EXT, static_cast<EGLint>(message.drm_fourcc));
    add(EGL_DMA_BUF_PLANE0_FD_EXT, dmabuf);
    add(EGL_DMA_BUF_PLANE0_OFFSET_EXT, static_cast<EGLint>(message.offset));
    add(EGL_DMA_BUF_PLANE0_PITCH_EXT, static_cast<EGLint>(message.pitch));
    if (egl.modifier_import && message.modifier != UINT64_MAX) {
        add(EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            static_cast<EGLint>(message.modifier & UINT32_MAX));
        add(EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            static_cast<EGLint>(message.modifier >> 32));
    }
    attributes[count] = EGL_NONE;
    return egl.create_image(egl.display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT,
                            nullptr, attributes);
}

bool initialize_for_image(EglState &egl, const luma_vk_export_message &message, int dmabuf,
                          EGLImageKHR &image) {
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
                image = import_image(candidate, message, dmabuf);
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
            image = import_image(candidate, message, dmabuf);
            if (image != EGL_NO_IMAGE_KHR) {
                egl = candidate;
                return true;
            }
            destroy_egl(candidate);
        }
    }
    return false;
}

bool wait_native_fence(EglState &egl, int &fence_fd) {
    const EGLint attributes[] = {EGL_SYNC_NATIVE_FENCE_FD_ANDROID, fence_fd, EGL_NONE};
    EGLSyncKHR sync = egl.create_sync(egl.display, EGL_SYNC_NATIVE_FENCE_ANDROID, attributes);
    if (sync == EGL_NO_SYNC_KHR) return false;
    /* EGL owns the descriptor after successful native-fence import. */
    fence_fd = -1;
    const bool waited = egl.wait_sync(egl.display, sync, 0) == EGL_TRUE;
    (void)egl.destroy_sync(egl.display, sync);
    return waited;
}

int bind_export_socket(const char *path) {
    if (!path || path[0] != '/' || strlen(path) >= sizeof(sockaddr_un::sun_path)) return -1;
    const int fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    int enabled = 1;
    if (setsockopt(fd, SOL_SOCKET, SO_PASSCRED, &enabled, sizeof(enabled)) != 0) {
        close(fd);
        return -1;
    }
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    memcpy(address.sun_path, path, strlen(path) + 1);
    if (bind(fd, reinterpret_cast<sockaddr *>(&address), sizeof(address)) != 0 ||
        chmod(path, 0600) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

struct ReceivedFrame {
    luma_vk_export_message message{};
    int dmabuf{-1};
    int fence{-1};
    sockaddr_un peer{};
    socklen_t peer_length{};
};

struct ImportedSlot {
    bool used{};
    uint64_t stream_id{};
    uint32_t slot{};
    uint32_t width{};
    uint32_t height{};
    EGLImageKHR image{EGL_NO_IMAGE_KHR};
    GLuint texture{};
    GLuint framebuffer{};
};

struct PendingAck {
    bool used{};
    GLsync fence{};
    ReceivedFrame frame{};
};

void destroy_imported(EglState &egl, ImportedSlot &slot) {
    if (slot.framebuffer) glDeleteFramebuffers(1, &slot.framebuffer);
    if (slot.texture) glDeleteTextures(1, &slot.texture);
    if (slot.image != EGL_NO_IMAGE_KHR && egl.destroy_image)
        (void)egl.destroy_image(egl.display, slot.image);
    slot = {};
}

bool receive_frame(int socket_fd, pid_t expected_pid, ReceivedFrame &frame) {
    char control[CMSG_SPACE(sizeof(int) * 2) + CMSG_SPACE(sizeof(ucred))];
    iovec io{&frame.message, sizeof(frame.message)};
    msghdr header{};
    header.msg_name = &frame.peer;
    header.msg_namelen = sizeof(frame.peer);
    header.msg_iov = &io;
    header.msg_iovlen = 1;
    header.msg_control = control;
    header.msg_controllen = sizeof(control);
    const ssize_t count = recvmsg(socket_fd, &header, MSG_CMSG_CLOEXEC);
    if (count < 0) return errno == EINTR || errno == EAGAIN ? false : false;
    frame.peer_length = header.msg_namelen;
    ucred credentials{};
    bool have_credentials = false;
    int descriptors[2] = {-1, -1};
    size_t descriptor_count = 0;
    for (cmsghdr *item = CMSG_FIRSTHDR(&header); item; item = CMSG_NXTHDR(&header, item)) {
        if (item->cmsg_level != SOL_SOCKET) continue;
        if (item->cmsg_type == SCM_CREDENTIALS && item->cmsg_len >= CMSG_LEN(sizeof(ucred))) {
            memcpy(&credentials, CMSG_DATA(item), sizeof(credentials));
            have_credentials = true;
        } else if (item->cmsg_type == SCM_RIGHTS) {
            const size_t available = (item->cmsg_len - CMSG_LEN(0)) / sizeof(int);
            const int *received = reinterpret_cast<const int *>(CMSG_DATA(item));
            for (size_t index = 0; index < available; ++index) {
                if (descriptor_count < 2) descriptors[descriptor_count++] = received[index];
                else close(received[index]);
            }
        }
    }
    const bool valid = count == static_cast<ssize_t>(sizeof(frame.message)) &&
        !(header.msg_flags & (MSG_TRUNC | MSG_CTRUNC)) && have_credentials &&
        credentials.uid == geteuid() && credentials.pid == expected_pid && descriptor_count == 2 &&
        frame.message.magic == LUMA_VK_EXPORT_MAGIC &&
        frame.message.version == LUMA_VK_PROTOCOL_VERSION &&
        frame.message.size == sizeof(frame.message) && frame.message.width > 0 &&
        frame.message.height > 0 && frame.message.width <= 16384 && frame.message.height <= 16384 &&
        frame.message.pitch > 0 && frame.message.drm_fourcc != 0 && frame.message.pts_ns != 0;
    if (!valid) {
        for (int descriptor : descriptors) if (descriptor >= 0) close(descriptor);
        return false;
    }
    frame.dmabuf = descriptors[0];
    frame.fence = descriptors[1];
    return true;
}

void acknowledge(int fd, const ReceivedFrame &frame) {
    const luma_vk_ack_message ack{
        LUMA_VK_ACK_MAGIC, LUMA_VK_PROTOCOL_VERSION, sizeof(luma_vk_ack_message),
        frame.message.generation, frame.message.slot, 0,
    };
    (void)sendto(fd, &ack, sizeof(ack), MSG_DONTWAIT | MSG_NOSIGNAL,
                 reinterpret_cast<const sockaddr *>(&frame.peer), frame.peer_length);
}

void retire_ack_fences(int socket_fd, std::array<PendingAck, 64> &pending, bool wait_all) {
    if (wait_all) glFinish();
    for (PendingAck &item : pending) {
        if (!item.used) continue;
        const GLenum status = wait_all ? GL_ALREADY_SIGNALED
            : glClientWaitSync(item.fence, 0, 0);
        if (status != GL_ALREADY_SIGNALED && status != GL_CONDITION_SATISFIED) continue;
        glDeleteSync(item.fence);
        acknowledge(socket_fd, item.frame);
        item = {};
    }
}

long positive_number(const char *text) {
    char *end = nullptr;
    errno = 0;
    const long value = strtol(text, &end, 10);
    return errno == 0 && end != text && *end == '\0' && value > 0 ? value : -1;
}
}

int main(int argc, char **argv) {
    const char *export_socket = nullptr, *stream_socket = nullptr, *token = nullptr;
    long expected_pid = -1, fps = -1, quality = 20;
    for (int index = 1; index < argc; ++index) {
        auto value = [&](const char *option) -> const char * {
            if (strcmp(argv[index], option) != 0 || index + 1 >= argc) return nullptr;
            return argv[++index];
        };
        if (const char *v = value("--export-socket")) export_socket = v;
        else if (const char *v = value("--stream-socket")) stream_socket = v;
        else if (const char *v = value("--token")) token = v;
        else if (const char *v = value("--expected-pid")) expected_pid = positive_number(v);
        else if (const char *v = value("--fps")) fps = positive_number(v);
        else if (const char *v = value("--quality")) quality = positive_number(v);
        else {
            fputs("invalid Vulkan capture receiver arguments\n", stderr);
            return 64;
        }
    }
    if (!export_socket || !stream_socket || !token || expected_pid <= 1 ||
        fps < 30 || fps > 480 || quality < 1 || quality > 51) {
        fputs("Vulkan receiver requires sockets, token, PID, FPS 30..480 and quality 1..51\n", stderr);
        return 64;
    }
    char quality_text[16];
    snprintf(quality_text, sizeof(quality_text), "%ld", quality);
    (void)setenv("LUMA_GAME_CAPTURE_QUALITY", quality_text, 1);
    signal(SIGINT, stop_handler);
    signal(SIGTERM, stop_handler);
    const int socket_fd = bind_export_socket(export_socket);
    if (socket_fd < 0) {
        fprintf(stderr, "cannot bind Vulkan export socket %s: %s\n", export_socket, strerror(errno));
        return 70;
    }

    EglState egl;
    luma_nvenc_direct *encoder = nullptr;
    uint32_t width = 0, height = 0;
    uint64_t received_count = 0, encoded_count = 0;
    std::array<ImportedSlot, 64> imported_slots{};
    std::array<PendingAck, 64> pending_acks{};
    while (!stopping.load(std::memory_order_acquire)) {
        retire_ack_fences(socket_fd, pending_acks, false);
        pollfd wait{socket_fd, POLLIN, 0};
        const bool have_pending = std::any_of(pending_acks.begin(), pending_acks.end(),
                                              [](const PendingAck &item) { return item.used; });
        const int ready = poll(&wait, 1, have_pending ? 1 : 100);
        if (ready <= 0 || !(wait.revents & POLLIN)) continue;
        ReceivedFrame frame;
        if (!receive_frame(socket_fd, static_cast<pid_t>(expected_pid), frame)) continue;
        ++received_count;
        ImportedSlot *imported = nullptr;
        for (ImportedSlot &candidate : imported_slots)
            if (candidate.used && candidate.stream_id == frame.message.stream_id &&
                candidate.slot == frame.message.slot) {
                imported = &candidate;
                break;
            }
        bool new_import = false;
        if (!imported) {
            for (ImportedSlot &candidate : imported_slots) if (!candidate.used) {
                imported = &candidate;
                break;
            }
            if (!imported) {
                fputs("Vulkan import cache is full; dropping export\n", stderr);
                close(frame.fence); close(frame.dmabuf); acknowledge(socket_fd, frame);
                continue;
            }
            EGLImageKHR image = EGL_NO_IMAGE_KHR;
            if (egl.display == EGL_NO_DISPLAY) {
                if (!initialize_for_image(egl, frame.message, frame.dmabuf, image)) {
                    fputs("no EGL device could import the Vulkan DMA-BUF\n", stderr);
                    close(frame.fence); close(frame.dmabuf); acknowledge(socket_fd, frame);
                    continue;
                }
            } else {
                image = import_image(egl, frame.message, frame.dmabuf);
            }
            if (image == EGL_NO_IMAGE_KHR) {
                close(frame.fence); close(frame.dmabuf); acknowledge(socket_fd, frame);
                continue;
            }
            *imported = {true, frame.message.stream_id, frame.message.slot,
                         frame.message.width, frame.message.height, image, 0, 0};
            glGenTextures(1, &imported->texture);
            glBindTexture(GL_TEXTURE_2D, imported->texture);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            egl.image_target_texture(GL_TEXTURE_2D, imported->image);
            glGenFramebuffers(1, &imported->framebuffer);
            glBindFramebuffer(GL_FRAMEBUFFER, imported->framebuffer);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0,
                                   GL_TEXTURE_2D, imported->texture, 0);
            if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
                destroy_imported(egl, *imported);
                close(frame.fence); close(frame.dmabuf); acknowledge(socket_fd, frame);
                continue;
            }
            new_import = true;
        }
        close(frame.dmabuf);
        frame.dmabuf = -1;
        if (!wait_native_fence(egl, frame.fence)) {
            if (frame.fence >= 0) close(frame.fence);
            if (new_import) destroy_imported(egl, *imported);
            acknowledge(socket_fd, frame);
            continue;
        }
        if (!encoder) {
            width = frame.message.width;
            height = frame.message.height;
            encoder = luma_nvenc_direct_create(width, height, static_cast<uint32_t>(fps),
                                                static_cast<uint32_t>(quality),
                                                stream_socket, token);
            if (encoder && !luma_nvenc_direct_wait_ready(encoder, 2000)) {
                fputs("Vulkan NVENC transport did not become ready\n", stderr);
            }
        }
        const bool encoded = encoder && imported->width == width && imported->height == height &&
            luma_nvenc_direct_submit_framebuffer(encoder, imported->framebuffer, width, height, 0,
                                                  frame.message.pts_ns) != 0;
        if (encoded) {
            ++encoded_count;
            PendingAck *pending = nullptr;
            for (PendingAck &candidate : pending_acks) if (!candidate.used) {
                pending = &candidate;
                break;
            }
            if (pending) {
                pending->fence = glFenceSync(GL_SYNC_GPU_COMMANDS_COMPLETE, 0);
                if (pending->fence) {
                    pending->used = true;
                    pending->frame = frame;
                    glFlush();
                } else {
                    glFinish();
                    acknowledge(socket_fd, frame);
                }
            } else {
                glFinish();
                acknowledge(socket_fd, frame);
            }
        } else {
            acknowledge(socket_fd, frame);
        }
    }
    retire_ack_fences(socket_fd, pending_acks, true);
    if (encoder) luma_nvenc_direct_finish_on_gl_thread(encoder);
    fprintf(stderr, "Luma Vulkan receiver processed %llu exports and submitted %llu frames\n",
            (unsigned long long)received_count, (unsigned long long)encoded_count);
    for (ImportedSlot &slot : imported_slots) if (slot.used) destroy_imported(egl, slot);
    destroy_egl(egl);
    close(socket_fd);
    unlink(export_socket);
    return 0;
}
