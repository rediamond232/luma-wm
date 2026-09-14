#define _GNU_SOURCE
#include <GL/gl.h>
#include <GL/glx.h>
#include <X11/Xlib.h>
#include <stdint.h>
#include <stdio.h>
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
    const uint64_t deadline = now_ns() + UINT64_C(6000000000);
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
    return 0;
}
