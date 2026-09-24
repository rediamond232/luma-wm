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

static uint64_t now_ns(void) {
    struct timespec value;
    clock_gettime(CLOCK_MONOTONIC, &value);
    return (uint64_t)value.tv_sec * UINT64_C(1000000000) + (uint64_t)value.tv_nsec;
}

int main(void) {
    Display *display = XOpenDisplay(NULL);
    if (display == NULL) return 2;
    int attributes[] = {GLX_RGBA, GLX_DOUBLEBUFFER, GLX_RED_SIZE, 8,
                        GLX_GREEN_SIZE, 8, GLX_BLUE_SIZE, 8, None};
    XVisualInfo *visual = glXChooseVisual(display, DefaultScreen(display), attributes);
    if (visual == NULL) return 3;
    Colormap colormap = XCreateColormap(display, DefaultRootWindow(display), visual->visual, AllocNone);
    XSetWindowAttributes window_attributes = {0};
    window_attributes.colormap = colormap;
    Window window = XCreateWindow(display, DefaultRootWindow(display), 0, 0, 640, 360, 0,
        visual->depth, InputOutput, visual->visual, CWColormap, &window_attributes);
    XMapWindow(display, window);
    GLXContext context = glXCreateContext(display, visual, NULL, True);
    XFree(visual);
    if (context == NULL || !glXMakeCurrent(display, window, context)) return 4;
    typedef void (*swap_interval_fn)(Display *, GLXDrawable, int);
    const __GLXextFuncPtr symbol = glXGetProcAddressARB((const GLubyte *)"glXSwapIntervalEXT");
    swap_interval_fn interval = NULL;
    memcpy(&interval, &symbol, sizeof(interval));
    if (interval != NULL) interval(display, window, 0);
    glClearColor(0.1f, 0.7f, 0.2f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);
    glXSwapBuffers(display, window); /* Resolve the PLT slot before injection. */
    if (prctl(PR_SET_PTRACER, PR_SET_PTRACER_ANY, 0, 0, 0) != 0) return 5;
    puts("READY");
    fflush(stdout);
    /* Re-record fixtures raise this through the environment; default keeps
     * every existing test's timing. */
    unsigned long runtime_s = 6;
    const char *runtime_text = getenv("LUMA_TEST_RUNTIME_S");
    if (runtime_text != NULL && runtime_text[0] != '\0') {
        char *end = NULL;
        const unsigned long parsed = strtoul(runtime_text, &end, 10);
        if (end != runtime_text && *end == '\0' && parsed >= 5 && parsed <= 120) {
            runtime_s = parsed;
        }
    }
    const uint64_t deadline = now_ns() + (uint64_t)runtime_s * UINT64_C(1000000000);
    const uint64_t loop_start = now_ns();
    uint64_t frame = 0;
    while (now_ns() < deadline) {
        glClearColor((frame & 1U) ? 0.8f : 0.1f, 0.2f, (frame & 1U) ? 0.1f : 0.8f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glXSwapBuffers(display, window);
        ++frame;
    }
    glXMakeCurrent(display, None, NULL);
    glXDestroyContext(display, context);
    XDestroyWindow(display, window);
    XFreeColormap(display, colormap);
    XCloseDisplay(display);
    const uint64_t elapsed_ns = now_ns() - loop_start;
    fprintf(stderr, "FRAMES %lu in %llums (%lu fps)\n", (unsigned long)frame,
            (unsigned long long)(elapsed_ns / UINT64_C(1000000)),
            (unsigned long)(frame * UINT64_C(1000000000) / (elapsed_ns ? elapsed_ns : 1)));
    return 0;
}
