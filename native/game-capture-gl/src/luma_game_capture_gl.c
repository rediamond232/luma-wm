/*
 * Luma OpenGL game-capture probe.
 *
 * This is deliberately a transparent, opt-in LD_PRELOAD shim or HotSpot agent.
 * It never reads pixels, changes game state, hides itself, or alters a swap result. It
 * selects one presenting context/surface and emits an inexpensive metadata
 * datagram only for that owner, so overlays cannot become the capture source.
 */
#define _GNU_SOURCE

#include <EGL/egl.h>
#include <GL/glx.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <link.h>
#include <limits.h>
#include <pthread.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/mman.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#include "luma_nvenc_direct.h"

enum luma_capture_api {
    LUMA_CAPTURE_API_GLX = 1,
    LUMA_CAPTURE_API_EGL = 2,
};

/* Native-endian, fixed 64-byte datagram. See PROTOCOL.md. */
struct luma_present_event_v1 {
    uint64_t magic;
    uint16_t version;
    uint16_t length;
    uint32_t api;
    uint64_t sequence;
    uint64_t monotonic_ns;
    uint64_t native_display;
    uint64_t native_surface;
    uint32_t width;
    uint32_t height;
    uint64_t reserved;
};

_Static_assert(sizeof(struct luma_present_event_v1) == 64,
               "Luma present protocol v1 must remain fixed size");

#define LUMA_PRESENT_MAGIC UINT64_C(0x31504c414d554c)

static _Atomic uint64_t sequence_number = 0;
static _Atomic int socket_fd = -2; /* -2 uninitialized, -1 disabled */
static _Atomic(struct luma_nvenc_direct *) direct_capture;
/* 0 = untouched, 1 = creating, 2 = ready, 3 = permanently unavailable. */
static _Atomic int direct_capture_state = 0;
static _Atomic(uint32_t) *injected_control;
static _Atomic int injected_install_state;
static char injected_stream_socket[sizeof(((struct sockaddr_un *)0)->sun_path)];
static char injected_token[65];
static char injected_debug_log[PATH_MAX];
static uint32_t injected_fps;
static uint32_t injected_quality;
static _Thread_local int in_hook;

/*
 * A game can have several GL contexts: launchers, overlays and auxiliary
 * windows are common.  Select the first presenting context/surface exactly
 * once, then leave every other one completely alone.  A capture texture and
 * its NVENC registration are context-bound, so changing owners later would
 * be incorrect as well as expensive.
 */
struct luma_capture_owner {
    uint32_t api;
    uintptr_t display;
    uintptr_t surface;
    uintptr_t context;
};

static struct luma_capture_owner capture_owner;
/* 0 = undecided, 1 = being published, 2 = selected. */
static _Atomic int capture_owner_state = 0;
static _Atomic int target_pid = 0; /* 0 uninitialized, -1 disabled. */
static _Atomic int debug_fd = -2;  /* -2 uninitialized, -1 disabled. */
static _Atomic int first_glx_hook_seen;

static void debug_message(const char *message) {
    int fd = atomic_load_explicit(&debug_fd, memory_order_acquire);
    if (fd == -2) {
        const char *path = injected_debug_log[0] != '\0'
                               ? injected_debug_log
                               : getenv("LUMA_GAME_CAPTURE_DEBUG_LOG");
        int created = -1;
        if (path != NULL && path[0] == '/') {
            created = open(path, O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0600);
        } else if (getenv("LUMA_GAME_CAPTURE_DEBUG") != NULL) {
            created = STDERR_FILENO;
        }
        int expected = -2;
        if (!atomic_compare_exchange_strong_explicit(&debug_fd, &expected, created,
                                                     memory_order_release, memory_order_acquire)) {
            if (created >= 0 && created != STDERR_FILENO) {
                close(created);
            }
            fd = expected;
        } else {
            fd = created;
        }
    }
    if (fd >= 0) {
        /* write(2) is deliberately used instead of stdio in a GL interposer. */
        (void)syscall(SYS_write, fd, message, strlen(message));
    }
}

static int capture_target_process(void) {
    int configured = atomic_load_explicit(&target_pid, memory_order_acquire);
    if (configured == 0) {
        const char *value = getenv("LUMA_GAME_CAPTURE_TARGET_PID");
        char *end = NULL;
        long parsed = value == NULL ? -1 : strtol(value, &end, 10);
        const int selected = (value != NULL && end != value && *end == '\0' && parsed > 0 &&
                              parsed <= INT32_MAX)
                                 ? (int)parsed
                                 : -1;
        int expected = 0;
        if (!atomic_compare_exchange_strong_explicit(&target_pid, &expected, selected,
                                                     memory_order_release, memory_order_acquire)) {
            configured = expected;
        } else {
            configured = selected;
        }
    }
    return configured == (int)getpid();
}

static int claim_capture_owner(uint32_t api, uintptr_t display, uintptr_t surface,
                               uintptr_t context) {
    int state = atomic_load_explicit(&capture_owner_state, memory_order_acquire);
    if (state == 2) {
        return capture_owner.api == api && capture_owner.display == display &&
               capture_owner.surface == surface && capture_owner.context == context;
    }
    if (state != 0) {
        /* Do not make a game present thread wait for another thread's setup. */
        return 0;
    }

    int expected = 0;
    if (!atomic_compare_exchange_strong_explicit(&capture_owner_state, &expected, 1,
                                                 memory_order_acq_rel, memory_order_acquire)) {
        return 0;
    }
    capture_owner = (struct luma_capture_owner){
        .api = api,
        .display = display,
        .surface = surface,
        .context = context,
    };
    atomic_store_explicit(&capture_owner_state, 2, memory_order_release);
    debug_message("luma-game-capture: selected one OpenGL present context\n");
    return 1;
}

static uint64_t monotonic_ns(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return 0;
    }
    return (uint64_t)now.tv_sec * UINT64_C(1000000000) + (uint64_t)now.tv_nsec;
}

static int connect_event_socket(void) {
    const char *path = getenv("LUMA_GAME_CAPTURE_SOCKET");
    if (path == NULL || path[0] == '\0' || strlen(path) >= sizeof(((struct sockaddr_un *)0)->sun_path)) {
        return -1;
    }

    int fd = socket(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (fd < 0) {
        return -1;
    }

    struct sockaddr_un address;
    memset(&address, 0, sizeof(address));
    address.sun_family = AF_UNIX;
    memcpy(address.sun_path, path, strlen(path) + 1);
    if (connect(fd, (const struct sockaddr *)&address, sizeof(address)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int event_socket(void) {
    int current = atomic_load_explicit(&socket_fd, memory_order_acquire);
    if (current != -2) {
        return current;
    }

    int created = connect_event_socket();
    int expected = -2;
    if (!atomic_compare_exchange_strong_explicit(&socket_fd, &expected, created,
                                                 memory_order_release, memory_order_acquire)) {
        if (created >= 0) {
            close(created);
        }
        return expected;
    }
    return created;
}

static void emit_present(uint32_t api, uintptr_t display, uintptr_t surface,
                         uint32_t width, uint32_t height) {
    const int fd = event_socket();
    if (fd < 0) {
        return;
    }

    const struct luma_present_event_v1 event = {
        .magic = LUMA_PRESENT_MAGIC,
        .version = 1,
        .length = sizeof(event),
        .api = api,
        .sequence = atomic_fetch_add_explicit(&sequence_number, 1, memory_order_relaxed) + 1,
        .monotonic_ns = monotonic_ns(),
        .native_display = (uint64_t)display,
        .native_surface = (uint64_t)surface,
        .width = width,
        .height = height,
    };

    /* Dropping a full/unavailable socket is intentional: presentation must never block. */
    (void)send(fd, &event, sizeof(event), MSG_DONTWAIT | MSG_NOSIGNAL);
}

static int direct_capture_requested(void) {
    const char *socket_path = injected_stream_socket[0] != '\0'
                                  ? injected_stream_socket
                                  : getenv("LUMA_GAME_CAPTURE_STREAM_SOCKET");
    const char *token = injected_token[0] != '\0'
                            ? injected_token
                            : getenv("LUMA_GAME_CAPTURE_TOKEN");
    return socket_path != NULL && socket_path[0] != '\0' && token != NULL && token[0] != '\0';
}

/*
 * This remains disabled unless both stream settings exist. The direct path
 * copies the current default framebuffer into its own GPU texture before the
 * real present call, then submits that texture to NVENC without CPU readback.
 */
static void submit_direct_capture(uint32_t width, uint32_t height) {
    if (injected_control != NULL &&
        atomic_load_explicit(injected_control, memory_order_acquire) != 0) {
        struct luma_nvenc_direct *capture =
            atomic_load_explicit(&direct_capture, memory_order_acquire);
        if (capture != NULL) {
            luma_nvenc_direct_request_stop(capture);
        }
    }
    const char *socket_path = injected_stream_socket[0] != '\0'
                                  ? injected_stream_socket
                                  : getenv("LUMA_GAME_CAPTURE_STREAM_SOCKET");
    const char *token = injected_token[0] != '\0'
                            ? injected_token
                            : getenv("LUMA_GAME_CAPTURE_TOKEN");
    if (socket_path == NULL || token == NULL || width == 0 || height == 0) {
        return;
    }
    int state = atomic_load_explicit(&direct_capture_state, memory_order_acquire);
    if (state == 0) {
        int expected = 0;
        if (atomic_compare_exchange_strong_explicit(&direct_capture_state, &expected, 1,
                                                    memory_order_acq_rel, memory_order_acquire)) {
            uint32_t fps = injected_fps != 0 ? injected_fps : 480;
            uint32_t quality = injected_quality != 0 ? injected_quality : 20;
            const char *fps_text = injected_fps == 0 ? getenv("LUMA_GAME_CAPTURE_FPS") : NULL;
            if (fps_text != NULL) {
                char *end = NULL;
                const unsigned long parsed = strtoul(fps_text, &end, 10);
                if (end != fps_text && *end == '\0' && parsed >= 1 && parsed <= 1000) {
                    fps = (uint32_t)parsed;
                }
            }
            const char *quality_text = injected_quality == 0
                                           ? getenv("LUMA_GAME_CAPTURE_QUALITY")
                                           : NULL;
            if (quality_text != NULL) {
                char *end = NULL;
                const unsigned long parsed = strtoul(quality_text, &end, 10);
                if (end != quality_text && *end == '\0' && parsed >= 1 && parsed <= 51) {
                    quality = (uint32_t)parsed;
                }
            }
            struct luma_nvenc_direct *created =
                luma_nvenc_direct_create(width, height, fps, quality, socket_path, token);
            atomic_store_explicit(&direct_capture, created, memory_order_release);
            atomic_store_explicit(&direct_capture_state, created != NULL ? 2 : 3,
                                  memory_order_release);
            if (created == NULL) {
                debug_message("luma-game-capture: direct NVENC setup unavailable; capture disabled\n");
            }
        }
    }
    if (atomic_load_explicit(&direct_capture_state, memory_order_acquire) == 2) {
        struct luma_nvenc_direct *capture = atomic_load_explicit(&direct_capture, memory_order_acquire);
        if (capture != NULL) {
            if (luma_nvenc_direct_stop_requested(capture)) {
                luma_nvenc_direct_finish_on_gl_thread(capture);
                atomic_store_explicit(&direct_capture_state, 3, memory_order_release);
                debug_message("luma-game-capture: stopped and released direct capture resources\n");
                return;
            }
            (void)luma_nvenc_direct_submit(capture, width, height, monotonic_ns());
        }
    }
}

typedef void (*glx_swap_buffers_fn)(Display *, GLXDrawable);
typedef EGLBoolean (*egl_swap_buffers_fn)(EGLDisplay, EGLSurface);

static glx_swap_buffers_fn real_glx_swap;
static egl_swap_buffers_fn real_egl_swap;
/* A late JVM agent cannot use normal ELF interposition. Its LWJGL2 dispatch
 * slot is patched explicitly and the displaced target is published here
 * before that atomic slot update becomes visible to the presenting thread. */
static _Atomic(uintptr_t) attached_glx_original;
static _Atomic(uintptr_t) attached_egl_original;
static pthread_once_t glx_swap_once = PTHREAD_ONCE_INIT;
static pthread_once_t egl_swap_once = PTHREAD_ONCE_INIT;

static glx_swap_buffers_fn resolve_glx_swap(void) {
    const uintptr_t attached =
        atomic_load_explicit(&attached_glx_original, memory_order_acquire);
    if (attached != 0) {
        glx_swap_buffers_fn resolved = NULL;
        _Static_assert(sizeof(attached) == sizeof(resolved),
                       "GLX function pointers must fit uintptr_t");
        memcpy(&resolved, &attached, sizeof(resolved));
        return resolved;
    }
    void *symbol = dlsym(RTLD_NEXT, "glXSwapBuffers");
    glx_swap_buffers_fn resolved = NULL;
    _Static_assert(sizeof(symbol) == sizeof(resolved), "POSIX function pointers must fit dlsym results");
    memcpy(&resolved, &symbol, sizeof(resolved));
    return resolved;
}

static egl_swap_buffers_fn resolve_egl_swap(void) {
    const uintptr_t attached =
        atomic_load_explicit(&attached_egl_original, memory_order_acquire);
    if (attached != 0) {
        egl_swap_buffers_fn resolved = NULL;
        _Static_assert(sizeof(attached) == sizeof(resolved),
                       "EGL function pointers must fit uintptr_t");
        memcpy(&resolved, &attached, sizeof(resolved));
        return resolved;
    }
    void *symbol = dlsym(RTLD_NEXT, "eglSwapBuffers");
    egl_swap_buffers_fn resolved = NULL;
    _Static_assert(sizeof(symbol) == sizeof(resolved), "POSIX function pointers must fit dlsym results");
    memcpy(&resolved, &symbol, sizeof(resolved));
    return resolved;
}

static void initialize_glx_swap(void) {
    real_glx_swap = resolve_glx_swap();
    if (real_glx_swap == NULL) {
        debug_message("luma-game-capture: could not resolve glXSwapBuffers\n");
    }
}

static void initialize_egl_swap(void) {
    real_egl_swap = resolve_egl_swap();
    if (real_egl_swap == NULL) {
        debug_message("luma-game-capture: could not resolve eglSwapBuffers\n");
    }
}

__attribute__((visibility("default")))
void glXSwapBuffers(Display *display, GLXDrawable drawable) {
    if (atomic_exchange_explicit(&first_glx_hook_seen, 1, memory_order_acq_rel) == 0) {
        debug_message("luma-game-capture: generic GLX hook received its first present\n");
    }
    (void)pthread_once(&glx_swap_once, initialize_glx_swap);
    if (in_hook) {
        if (real_glx_swap != NULL) {
            real_glx_swap(display, drawable);
        }
        return;
    }
    if (real_glx_swap == NULL) {
        return;
    }
    if (!capture_target_process()) {
        real_glx_swap(display, drawable);
        return;
    }
    in_hook = 1;
    unsigned int width = 0;
    unsigned int height = 0;
    const int needs_initial_size =
        atomic_load_explicit(&capture_owner_state, memory_order_acquire) == 0 &&
        direct_capture_requested();
    if (needs_initial_size && display != NULL) {
        (void)glXQueryDrawable(display, drawable, GLX_WIDTH, &width);
        (void)glXQueryDrawable(display, drawable, GLX_HEIGHT, &height);
    }
    if (needs_initial_size && (width == 0 || height == 0)) {
        /* Do not permanently bind a direct encoder to an unmapped 0x0 surface. */
        real_glx_swap(display, drawable);
        in_hook = 0;
        return;
    }
    if (claim_capture_owner(LUMA_CAPTURE_API_GLX, (uintptr_t)display, (uintptr_t)drawable,
                            (uintptr_t)glXGetCurrentContext())) {
        if (!needs_initial_size && display != NULL) {
            (void)glXQueryDrawable(display, drawable, GLX_WIDTH, &width);
            (void)glXQueryDrawable(display, drawable, GLX_HEIGHT, &height);
        }
        submit_direct_capture(width, height);
        real_glx_swap(display, drawable);
        emit_present(LUMA_CAPTURE_API_GLX, (uintptr_t)display, (uintptr_t)drawable, width, height);
    } else {
        real_glx_swap(display, drawable);
    }
    in_hook = 0;
}

__attribute__((visibility("default")))
EGLBoolean eglSwapBuffers(EGLDisplay display, EGLSurface surface) {
    (void)pthread_once(&egl_swap_once, initialize_egl_swap);
    if (in_hook) {
        return real_egl_swap != NULL ? real_egl_swap(display, surface) : EGL_FALSE;
    }
    if (real_egl_swap == NULL) {
        return EGL_FALSE;
    }
    if (!capture_target_process()) {
        return real_egl_swap(display, surface);
    }
    in_hook = 1;
    EGLBoolean result = EGL_FALSE;
    EGLint width = 0;
    EGLint height = 0;
    const int needs_initial_size =
        atomic_load_explicit(&capture_owner_state, memory_order_acquire) == 0 &&
        direct_capture_requested();
    if (needs_initial_size) {
        (void)eglQuerySurface(display, surface, EGL_WIDTH, &width);
        (void)eglQuerySurface(display, surface, EGL_HEIGHT, &height);
    }
    if (needs_initial_size && (width <= 0 || height <= 0)) {
        result = real_egl_swap(display, surface);
        in_hook = 0;
        return result;
    }
    if (claim_capture_owner(LUMA_CAPTURE_API_EGL, (uintptr_t)display, (uintptr_t)surface,
                            (uintptr_t)eglGetCurrentContext())) {
        if (!needs_initial_size) {
            (void)eglQuerySurface(display, surface, EGL_WIDTH, &width);
            (void)eglQuerySurface(display, surface, EGL_HEIGHT, &height);
        }
        submit_direct_capture(width > 0 ? (uint32_t)width : 0, height > 0 ? (uint32_t)height : 0);
        result = real_egl_swap(display, surface);
        if (result == EGL_TRUE) {
            emit_present(LUMA_CAPTURE_API_EGL, (uintptr_t)display, (uintptr_t)surface,
                         width > 0 ? (uint32_t)width : 0, height > 0 ? (uint32_t)height : 0);
        }
    } else {
        result = real_egl_swap(display, surface);
    }
    in_hook = 0;
    return result;
}

/*
 * Late-load configuration and graphics-API hooks
 * ----------------------------------------------
 *
 * Generic .so injection scans already-resolved GLX/EGL ELF relocations and
 * recognizes LWJGL2's exported Linux swap JNI bridge, whose glXSwapBuffers
 * pointer lives in a separate writable dispatch slot.
 *
 * This deliberately does not try to hide the library, bypass an anti-cheat,
 * or patch arbitrary instruction streams. Unsupported dispatch shapes are
 * rejected.
 */

struct luma_attach_config {
    pid_t pid;
    uint32_t fps;
    uint32_t quality;
    char stream_socket[sizeof(((struct sockaddr_un *)0)->sun_path)];
    char token[65];
    char debug_log[PATH_MAX];
};

static int parse_positive_u32(const char *text, uint32_t minimum, uint32_t maximum,
                              uint32_t *result) {
    char *end = NULL;
    errno = 0;
    const unsigned long value = strtoul(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || value < minimum || value > maximum) {
        return -1;
    }
    *result = (uint32_t)value;
    return 0;
}

static int valid_token(const char *token) {
    if (strlen(token) != 64) {
        return 0;
    }
    for (size_t index = 0; index < 64; ++index) {
        const char value = token[index];
        if (!((value >= '0' && value <= '9') || (value >= 'a' && value <= 'f') ||
              (value >= 'A' && value <= 'F'))) {
            return 0;
        }
    }
    return 1;
}

static int copy_config_value(char *destination, size_t capacity, const char *value) {
    const size_t length = strlen(value);
    if (length == 0 || length >= capacity) {
        return -1;
    }
    memcpy(destination, value, length + 1);
    return 0;
}

static int read_attach_config(const char *path, struct luma_attach_config *config) {
    if (path == NULL || path[0] != '/' || strlen(path) >= PATH_MAX) {
        return -1;
    }
    const int fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) {
        return -1;
    }
    struct stat metadata;
    if (fstat(fd, &metadata) != 0 || !S_ISREG(metadata.st_mode) ||
        metadata.st_uid != geteuid() || (metadata.st_mode & 0077) != 0 ||
        metadata.st_size <= 0 || metadata.st_size > 4096) {
        close(fd);
        return -1;
    }
    char bytes[4097];
    size_t length = 0;
    while (length < (size_t)metadata.st_size) {
        const ssize_t count = read(fd, bytes + length, (size_t)metadata.st_size - length);
        if (count <= 0) {
            close(fd);
            return -1;
        }
        length += (size_t)count;
    }
    close(fd);
    bytes[length] = '\0';

    unsigned seen = 0;
    char *save = NULL;
    for (char *line = strtok_r(bytes, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        char *separator = strchr(line, '=');
        if (separator == NULL || separator == line) {
            return -1;
        }
        *separator = '\0';
        const char *value = separator + 1;
        if (strcmp(line, "version") == 0) {
            if ((seen & 1U) != 0 || strcmp(value, "1") != 0) return -1;
            seen |= 1U;
        } else if (strcmp(line, "pid") == 0) {
            uint32_t pid = 0;
            if ((seen & 2U) != 0 || parse_positive_u32(value, 1, INT32_MAX, &pid) != 0)
                return -1;
            config->pid = (pid_t)pid;
            seen |= 2U;
        } else if (strcmp(line, "stream_socket") == 0) {
            if ((seen & 4U) != 0 || value[0] != '/' ||
                copy_config_value(config->stream_socket, sizeof(config->stream_socket), value) !=
                    0)
                return -1;
            seen |= 4U;
        } else if (strcmp(line, "token") == 0) {
            if ((seen & 8U) != 0 || !valid_token(value) ||
                copy_config_value(config->token, sizeof(config->token), value) != 0)
                return -1;
            seen |= 8U;
        } else if (strcmp(line, "fps") == 0) {
            if ((seen & 16U) != 0 || parse_positive_u32(value, 30, 480, &config->fps) != 0)
                return -1;
            seen |= 16U;
        } else if (strcmp(line, "quality") == 0) {
            if ((seen & 32U) != 0 || parse_positive_u32(value, 1, 51, &config->quality) != 0)
                return -1;
            seen |= 32U;
        } else if (strcmp(line, "debug_log") == 0) {
            if ((seen & 64U) != 0 || value[0] != '/' ||
                copy_config_value(config->debug_log, sizeof(config->debug_log), value) != 0)
                return -1;
            seen |= 64U;
        } else {
            return -1;
        }
    }
    return seen == 127U && config->pid == getpid() ? 0 : -1;
}

static int activate_attach_config(const struct luma_attach_config *config) {
    (void)snprintf(injected_stream_socket, sizeof(injected_stream_socket), "%s",
                   config->stream_socket);
    (void)snprintf(injected_token, sizeof(injected_token), "%s", config->token);
    (void)snprintf(injected_debug_log, sizeof(injected_debug_log), "%s", config->debug_log);
    injected_fps = config->fps;
    injected_quality = config->quality;
    atomic_store_explicit(&target_pid, (int)config->pid, memory_order_release);
    return 0;
}

struct generic_hook_search {
    uintptr_t self_base;
    size_t glx_patched;
    size_t egl_patched;
};

static uintptr_t dynamic_address(ElfW(Addr) base, ElfW(Addr) value) {
    return value < base ? (uintptr_t)base + (uintptr_t)value : (uintptr_t)value;
}

static int graphics_target(const char *symbol, uintptr_t original) {
    Dl_info info = {0};
    if (original == 0 || dladdr((void *)original, &info) == 0 || info.dli_fname == NULL) {
        return 0;
    }
    const char *name = strrchr(info.dli_fname, '/');
    name = name == NULL ? info.dli_fname : name + 1;
    if (strcmp(symbol, "glXSwapBuffers") == 0) {
        return strncmp(name, "libGL", 5) == 0 || strncmp(name, "libOpenGL", 9) == 0;
    }
    return strncmp(name, "libEGL", 6) == 0;
}

static int mapping_protection(const void *address) {
    FILE *maps = fopen("/proc/self/maps", "re");
    if (maps == NULL) return 0;
    char line[512], permissions[5];
    unsigned long long start = 0, end = 0;
    int protection = 0;
    while (fgets(line, sizeof(line), maps) != NULL) {
        if (sscanf(line, "%llx-%llx %4s", &start, &end, permissions) == 3 &&
            (uintptr_t)address >= (uintptr_t)start && (uintptr_t)address < (uintptr_t)end) {
            if (permissions[0] == 'r') protection |= PROT_READ;
            if (permissions[1] == 'w') protection |= PROT_WRITE;
            if (permissions[2] == 'x') protection |= PROT_EXEC;
            break;
        }
    }
    fclose(maps);
    return protection;
}

static int patch_graphics_relocations(struct dl_phdr_info *info, size_t size, void *opaque) {
    (void)size;
    struct generic_hook_search *search = opaque;
    if ((uintptr_t)info->dlpi_addr == search->self_base) return 0;
    const char *base_name = info->dlpi_name == NULL ? "" : strrchr(info->dlpi_name, '/');
    base_name = base_name == NULL ? info->dlpi_name : base_name + 1;
    if (base_name != NULL &&
        (strncmp(base_name, "libGL", 5) == 0 || strncmp(base_name, "libEGL", 6) == 0 ||
         strncmp(base_name, "libOpenGL", 9) == 0 || strncmp(base_name, "libluma", 7) == 0)) {
        return 0;
    }

    const ElfW(Dyn) *dynamic = NULL;
    for (ElfW(Half) index = 0; index < info->dlpi_phnum; ++index) {
        if (info->dlpi_phdr[index].p_type == PT_DYNAMIC) {
            dynamic = (const ElfW(Dyn) *)(info->dlpi_addr + info->dlpi_phdr[index].p_vaddr);
            break;
        }
    }
    if (dynamic == NULL) return 0;
    const ElfW(Sym) *symbols = NULL;
    const char *strings = NULL;
    const ElfW(Rela) *plt = NULL, *rela = NULL;
    size_t plt_size = 0, rela_size = 0;
    for (const ElfW(Dyn) *entry = dynamic; entry->d_tag != DT_NULL; ++entry) {
        switch (entry->d_tag) {
            case DT_SYMTAB: symbols = (const ElfW(Sym) *)dynamic_address(info->dlpi_addr, entry->d_un.d_ptr); break;
            case DT_STRTAB: strings = (const char *)dynamic_address(info->dlpi_addr, entry->d_un.d_ptr); break;
            case DT_JMPREL: plt = (const ElfW(Rela) *)dynamic_address(info->dlpi_addr, entry->d_un.d_ptr); break;
            case DT_PLTRELSZ: plt_size = (size_t)entry->d_un.d_val; break;
            case DT_RELA: rela = (const ElfW(Rela) *)dynamic_address(info->dlpi_addr, entry->d_un.d_ptr); break;
            case DT_RELASZ: rela_size = (size_t)entry->d_un.d_val; break;
            default: break;
        }
    }
    if (symbols == NULL || strings == NULL) return 0;
    const long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0) return 0;
    const ElfW(Rela) *tables[2] = {plt, rela};
    const size_t sizes[2] = {plt_size, rela_size};
    for (size_t table = 0; table < 2; ++table) {
        for (size_t index = 0; tables[table] != NULL && index < sizes[table] / sizeof(ElfW(Rela)); ++index) {
            const ElfW(Rela) *relocation = &tables[table][index];
            const unsigned type = (unsigned)ELF64_R_TYPE(relocation->r_info);
            if (type != R_X86_64_JUMP_SLOT && type != R_X86_64_GLOB_DAT) continue;
            const char *name = strings + symbols[ELF64_R_SYM(relocation->r_info)].st_name;
            const int is_glx = strcmp(name, "glXSwapBuffers") == 0;
            const int is_egl = strcmp(name, "eglSwapBuffers") == 0;
            if (!is_glx && !is_egl) continue;
            uintptr_t *slot = (uintptr_t *)(info->dlpi_addr + relocation->r_offset);
            const uintptr_t original = __atomic_load_n(slot, __ATOMIC_ACQUIRE);
            if (!graphics_target(name, original)) continue;
            void *hook_pointer = NULL;
            if (is_glx) {
                glx_swap_buffers_fn hook = glXSwapBuffers;
                memcpy(&hook_pointer, &hook, sizeof(hook_pointer));
                uintptr_t expected = 0;
                (void)atomic_compare_exchange_strong_explicit(&attached_glx_original, &expected,
                    original, memory_order_acq_rel, memory_order_acquire);
            } else {
                egl_swap_buffers_fn hook = eglSwapBuffers;
                memcpy(&hook_pointer, &hook, sizeof(hook_pointer));
                uintptr_t expected = 0;
                (void)atomic_compare_exchange_strong_explicit(&attached_egl_original, &expected,
                    original, memory_order_acq_rel, memory_order_acquire);
            }
            void *page = (void *)((uintptr_t)slot & ~((uintptr_t)page_size - 1U));
            const int original_protection = mapping_protection(slot);
            if (original_protection == 0) continue;
            if (mprotect(page, (size_t)page_size, PROT_READ | PROT_WRITE) != 0) continue;
            __atomic_store_n(slot, (uintptr_t)hook_pointer, __ATOMIC_RELEASE);
            (void)mprotect(page, (size_t)page_size, original_protection);
            if (is_glx) {
                ++search->glx_patched;
                debug_message("luma-game-capture: patched one resolved GLX relocation\n");
            } else {
                ++search->egl_patched;
                debug_message("luma-game-capture: patched one resolved EGL relocation\n");
            }
        }
    }
    return 0;
}

static int install_generic_graphics_hooks(void) {
    Dl_info self = {0};
    void *self_symbol = NULL;
    glx_swap_buffers_fn self_function = glXSwapBuffers;
    memcpy(&self_symbol, &self_function, sizeof(self_symbol));
    if (dladdr(self_symbol, &self) == 0 || self.dli_fbase == NULL) return -1;
    struct generic_hook_search search = {.self_base = (uintptr_t)self.dli_fbase};
    (void)dl_iterate_phdr(patch_graphics_relocations, &search);
    if (search.glx_patched == 0 && search.egl_patched == 0) {
        debug_message("luma-game-capture: no resolved GLX/EGL present relocation found\n");
        return -1;
    }
    debug_message("luma-game-capture: installed generic late GLX/EGL relocation hooks\n");
    return 0;
}

/* LWJGL2 caches glXSwapBuffers outside normal ELF relocations, so generic
 * remote dlopen also recognizes its exported bridge and writable slot. */
static int install_lwjgl2_glx_hook(void);

static void *injected_hook_worker(void *opaque) {
    (void)opaque;
    char config_path[PATH_MAX], control_path[PATH_MAX];
    (void)snprintf(config_path, sizeof(config_path), "/run/user/%u/luma-game-inject-%ld.conf",
                   (unsigned)geteuid(), (long)getpid());
    (void)snprintf(control_path, sizeof(control_path), "/run/user/%u/luma-game-inject-%ld.ctl",
                   (unsigned)geteuid(), (long)getpid());
    struct luma_attach_config config = {0};
    if (read_attach_config(config_path, &config) != 0) goto fail;
    int control_fd = open(control_path, O_RDWR | O_CLOEXEC | O_NOFOLLOW);
    struct stat metadata;
    if (control_fd < 0 || fstat(control_fd, &metadata) != 0 || !S_ISREG(metadata.st_mode) ||
        metadata.st_uid != geteuid() || (metadata.st_mode & 0077) != 0 || metadata.st_size != 4) {
        if (control_fd >= 0) close(control_fd);
        goto fail;
    }
    void *mapped = mmap(NULL, 4, PROT_READ, MAP_SHARED, control_fd, 0);
    close(control_fd);
    if (mapped == MAP_FAILED || activate_attach_config(&config) != 0) {
        if (mapped != MAP_FAILED) (void)munmap(mapped, 4);
        goto fail;
    }
    injected_control = mapped;
    debug_message("luma-game-capture: remote injection configuration activated\n");
    debug_message("luma-game-capture: scanning for LWJGL2 presentation dispatch\n");
    int hook_status = install_lwjgl2_glx_hook();
    if (hook_status != 0) {
        debug_message("luma-game-capture: scanning generic GLX/EGL relocations\n");
        hook_status = install_generic_graphics_hooks();
    }
    if (hook_status != 0) {
        debug_message("luma-game-capture: injected graphics hook setup failed\n");
        void *mapped = injected_control;
        injected_control = NULL;
        (void)munmap(mapped, 4);
        goto fail;
    }
    /* Removing the handoff file acknowledges that hook discovery completed. */
    (void)unlink(config_path);
    atomic_store_explicit(&injected_install_state, 2, memory_order_release);
    return NULL;

fail:
    atomic_store_explicit(&injected_install_state, -1, memory_order_release);
    return NULL;
}

/* Called by the ptrace helper only after remote dlopen has returned. Complex
 * libc and loader work runs on a normal pthread stack: HotSpot's interrupted
 * Java-thread stack and signal machinery are not safe places to perform it. */
__attribute__((visibility("default")))
int luma_install_injected_hooks(void) {
    int expected = 0;
    if (!atomic_compare_exchange_strong_explicit(&injected_install_state, &expected, 1,
                                                 memory_order_acq_rel,
                                                 memory_order_acquire)) {
        return -1;
    }
    pthread_t worker;
    if (pthread_create(&worker, NULL, injected_hook_worker, NULL) != 0) {
        atomic_store_explicit(&injected_install_state, -1, memory_order_release);
        return -1;
    }
    (void)pthread_detach(worker);
    return 0;
}

static int address_has_permissions(const void *address, int require_write) {
    FILE *maps = fopen("/proc/self/maps", "re");
    if (maps == NULL) {
        return 0;
    }
    const uintptr_t target = (uintptr_t)address;
    char line[512];
    int permitted = 0;
    while (fgets(line, sizeof(line), maps) != NULL) {
        unsigned long long start = 0;
        unsigned long long end = 0;
        char permissions[5] = {0};
        if (sscanf(line, "%llx-%llx %4s", &start, &end, permissions) == 3 &&
            target >= (uintptr_t)start && target + sizeof(uintptr_t) <= (uintptr_t)end &&
            permissions[0] == 'r' && (!require_write || permissions[1] == 'w')) {
            permitted = 1;
            break;
        }
    }
    fclose(maps);
    return permitted;
}

struct lwjgl_module_search {
    void *swap_bridge;
};

static int find_lwjgl_swap_bridge(struct dl_phdr_info *info, size_t size, void *opaque) {
    (void)size;
    const char *name = info->dlpi_name;
    const char *base = name == NULL ? NULL : strrchr(name, '/');
    base = base == NULL ? name : base + 1;
    if (base == NULL || (strcmp(base, "liblwjgl64.so") != 0 &&
                         strcmp(base, "liblwjgl.so") != 0)) {
        return 0;
    }
    void *handle = dlopen(name, RTLD_NOW | RTLD_NOLOAD);
    if (handle == NULL) {
        return 0;
    }
    struct lwjgl_module_search *search = opaque;
    search->swap_bridge =
        dlsym(handle, "Java_org_lwjgl_opengl_LinuxContextImplementation_nSwapBuffers");
    dlclose(handle);
    return search->swap_bridge != NULL ? 1 : 0;
}

static int install_lwjgl2_glx_hook(void) {
    struct lwjgl_module_search search = {0};
    (void)dl_iterate_phdr(find_lwjgl_swap_bridge, &search);
    if (search.swap_bridge == NULL) {
        debug_message("luma-game-capture: running JVM has no supported LWJGL2 GLX bridge\n");
        return -1;
    }

    const unsigned char *code = search.swap_bridge;
    void **dispatch_slot = NULL;
    for (size_t index = 0; index + 10 <= 64; ++index) {
        if (code[index] != 0x48 || code[index + 1] != 0x8b || code[index + 2] != 0x15) {
            continue;
        }
        int32_t displacement = 0;
        memcpy(&displacement, code + index + 3, sizeof(displacement));
        void ***holder = (void ***)(code + index + 7 + displacement);
        if (holder == NULL || !address_has_permissions(holder, 0)) {
            continue;
        }
        void **candidate = NULL;
        memcpy(&candidate, holder, sizeof(candidate));
        if (candidate == NULL || !address_has_permissions(candidate, 1)) {
            continue;
        }
        void *candidate_target = NULL;
        memcpy(&candidate_target, candidate, sizeof(candidate_target));
        Dl_info target_info = {0};
        const char *target_name = NULL;
        if (candidate_target != NULL && dladdr(candidate_target, &target_info) != 0 &&
            target_info.dli_fname != NULL) {
            target_name = strrchr(target_info.dli_fname, '/');
            target_name = target_name == NULL ? target_info.dli_fname : target_name + 1;
        }
        if (target_name != NULL &&
            (strncmp(target_name, "libGL", 5) == 0 ||
             strncmp(target_name, "libOpenGL", 9) == 0)) {
            dispatch_slot = candidate;
            break;
        }
    }
    if (dispatch_slot == NULL) {
        debug_message("luma-game-capture: LWJGL2 GLX dispatch slot pattern is unsupported\n");
        return -1;
    }

    void *hook = NULL;
    glx_swap_buffers_fn hook_function = glXSwapBuffers;
    _Static_assert(sizeof(hook) == sizeof(hook_function), "GLX hook pointer size mismatch");
    memcpy(&hook, &hook_function, sizeof(hook));
    const uintptr_t original = __atomic_load_n((uintptr_t *)dispatch_slot, __ATOMIC_ACQUIRE);
    atomic_store_explicit(&attached_glx_original, original, memory_order_release);
    __atomic_store_n((uintptr_t *)dispatch_slot, (uintptr_t)hook, __ATOMIC_RELEASE);
    debug_message("luma-game-capture: attached to LWJGL2 GLX swap dispatch\n");
    return 0;
}
