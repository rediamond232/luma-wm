#define _GNU_SOURCE

#include <GL/gl.h>
#include <GL/glx.h>
#include <X11/Xlib.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <time.h>

typedef void (*fixture_swap_fn)(Display *, GLXDrawable);

/* Permit the isolated test's sibling injector under Yama ptrace_scope=1.
 * Production renderers make their own ptrace-policy decision. */
__attribute__((constructor)) static void permit_test_injector(void) {
    (void)prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY, 0, 0, 0);
}

/* The late-attach agent recognizes the same writable LWJGL2 dispatch shape
 * used by Badlion's 2.9.x native library. */
__attribute__((visibility("default"))) fixture_swap_fn luma_fixture_swap_slot = glXSwapBuffers;

__asm__(
    ".text\n"
    ".p2align 4\n"
    ".globl Java_org_lwjgl_opengl_LinuxContextImplementation_nSwapBuffers\n"
    ".type Java_org_lwjgl_opengl_LinuxContextImplementation_nSwapBuffers,@function\n"
    "Java_org_lwjgl_opengl_LinuxContextImplementation_nSwapBuffers:\n"
    "mov luma_fixture_swap_slot@GOTPCREL(%rip), %rdx\n"
    "mov (%rdx), %rdx\n"
    "jmp *%rdx\n"
    ".size Java_org_lwjgl_opengl_LinuxContextImplementation_nSwapBuffers, .-Java_org_lwjgl_opengl_LinuxContextImplementation_nSwapBuffers\n");

static uint64_t monotonic_ns(void) {
    struct timespec value;
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) return 0;
    return (uint64_t)value.tv_sec * UINT64_C(1000000000) + (uint64_t)value.tv_nsec;
}

__attribute__((visibility("default")))
void Java_LumaAttachFixture_run(void *environment, void *class_object, int seconds) {
    (void)environment;
    (void)class_object;
    Display *display = XOpenDisplay(NULL);
    if (display == NULL || seconds < 1) return;
    fputs("GLX_START\n", stderr);
    fflush(stderr);
    const int screen = DefaultScreen(display);
    int attributes[] = {GLX_RGBA, GLX_DOUBLEBUFFER, GLX_RED_SIZE, 8,
                        GLX_GREEN_SIZE, 8, GLX_BLUE_SIZE, 8, None};
    XVisualInfo *visual = glXChooseVisual(display, screen, attributes);
    if (visual == NULL) {
        XCloseDisplay(display);
        return;
    }
    Colormap colormap = XCreateColormap(display, RootWindow(display, screen), visual->visual,
                                        AllocNone);
    XSetWindowAttributes window_attributes = {0};
    window_attributes.colormap = colormap;
    window_attributes.event_mask = StructureNotifyMask;
    Window window = XCreateWindow(display, RootWindow(display, screen), 0, 0, 640, 360, 0,
                                  visual->depth, InputOutput, visual->visual,
                                  CWColormap | CWEventMask, &window_attributes);
    XStoreName(display, window, "Luma running-JVM attach fixture");
    XMapWindow(display, window);
    GLXContext context = glXCreateContext(display, visual, NULL, True);
    XFree(visual);
    if (context == NULL || !glXMakeCurrent(display, window, context)) {
        if (context != NULL) glXDestroyContext(display, context);
        XDestroyWindow(display, window);
        XFreeColormap(display, colormap);
        XCloseDisplay(display);
        return;
    }
    typedef void (*swap_interval_ext_fn)(Display *, GLXDrawable, int);
    const __GLXextFuncPtr interval_symbol =
        glXGetProcAddressARB((const GLubyte *)"glXSwapIntervalEXT");
    swap_interval_ext_fn set_swap_interval = NULL;
    _Static_assert(sizeof(interval_symbol) == sizeof(set_swap_interval),
                   "GLX extension pointer size mismatch");
    memcpy(&set_swap_interval, &interval_symbol, sizeof(set_swap_interval));
    if (set_swap_interval != NULL) set_swap_interval(display, window, 0);

    const uint64_t deadline = monotonic_ns() + (uint64_t)seconds * UINT64_C(1000000000);
    uint64_t frame = 0;
    while (monotonic_ns() < deadline) {
        const float red = (frame & 1U) == 0 ? 0.9f : 0.1f;
        const float blue = (frame & 1U) == 0 ? 0.1f : 0.9f;
        glViewport(0, 0, 640, 360);
        glClearColor(red, 0.2f, blue, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        luma_fixture_swap_slot(display, window);
        ++frame;
    }
    fprintf(stderr, "GLX_DONE %llu\n", (unsigned long long)frame);
    fflush(stderr);
    glXMakeCurrent(display, None, NULL);
    glXDestroyContext(display, context);
    XDestroyWindow(display, window);
    XFreeColormap(display, colormap);
    XCloseDisplay(display);
}
