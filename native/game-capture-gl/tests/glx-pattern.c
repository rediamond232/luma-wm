/* A harmless, deterministic GLX present source for the visual recorder test. */
#include <GL/gl.h>
#include <GL/glx.h>
#include <X11/Xlib.h>

#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static volatile sig_atomic_t running = 1;

static void stop(int unused) {
    (void)unused;
    running = 0;
}

int main(void) {
    Display *display = XOpenDisplay(NULL);
    if (display == NULL) {
        fputs("glx-pattern: XOpenDisplay failed\n", stderr);
        return 2;
    }
    const int attributes[] = {GLX_RGBA, GLX_DOUBLEBUFFER, GLX_DEPTH_SIZE, 0, None};
    XVisualInfo *visual = glXChooseVisual(display, DefaultScreen(display), (int *)attributes);
    if (visual == NULL) {
        fputs("glx-pattern: no suitable GLX visual\n", stderr);
        XCloseDisplay(display);
        return 2;
    }
    Colormap cmap = XCreateColormap(display, RootWindow(display, visual->screen), visual->visual,
                                    AllocNone);
    XSetWindowAttributes window_attributes = {
        .colormap = cmap,
        .event_mask = ExposureMask | StructureNotifyMask,
    };
    Window window = XCreateWindow(display, RootWindow(display, visual->screen), 32, 32, 640, 360, 0,
                                  visual->depth, InputOutput, visual->visual,
                                  CWColormap | CWEventMask, &window_attributes);
    XStoreName(display, window, "Luma GLX Visual Capture Fixture");
    XMapWindow(display, window);
    GLXContext context = glXCreateContext(display, visual, NULL, True);
    XFree(visual);
    if (context == NULL || !glXMakeCurrent(display, window, context)) {
        fputs("glx-pattern: GLX context setup failed\n", stderr);
        if (context != NULL) glXDestroyContext(display, context);
        XDestroyWindow(display, window);
        XCloseDisplay(display);
        return 2;
    }
    signal(SIGTERM, stop);
    signal(SIGINT, stop);
    while (running) {
        while (XPending(display)) {
            XEvent event;
            XNextEvent(display, &event);
        }
        XWindowAttributes geometry;
        XGetWindowAttributes(display, window, &geometry);
        int width = geometry.width > 0 ? geometry.width : 640;
        int height = geometry.height > 0 ? geometry.height : 360;
        glViewport(0, 0, width, height);
        glDisable(GL_DEPTH_TEST);
        glDisable(GL_SCISSOR_TEST);
        glMatrixMode(GL_PROJECTION);
        glLoadIdentity();
        glMatrixMode(GL_MODELVIEW);
        glLoadIdentity();
        /* GL origin is bottom-left: blue/yellow occupy the lower half. */
        glBegin(GL_QUADS);
        glColor3f(0.05f, 0.05f, 1.0f); /* bottom-left blue */
        glVertex2f(-1.0f, -1.0f); glVertex2f(0.0f, -1.0f);
        glVertex2f(0.0f, 0.0f); glVertex2f(-1.0f, 0.0f);
        glColor3f(1.0f, 0.95f, 0.05f); /* bottom-right yellow */
        glVertex2f(0.0f, -1.0f); glVertex2f(1.0f, -1.0f);
        glVertex2f(1.0f, 0.0f); glVertex2f(0.0f, 0.0f);
        glColor3f(1.0f, 0.05f, 0.05f); /* top-left red */
        glVertex2f(-1.0f, 0.0f); glVertex2f(0.0f, 0.0f);
        glVertex2f(0.0f, 1.0f); glVertex2f(-1.0f, 1.0f);
        glColor3f(0.05f, 1.0f, 0.05f); /* top-right green */
        glVertex2f(0.0f, 0.0f); glVertex2f(1.0f, 0.0f);
        glVertex2f(1.0f, 1.0f); glVertex2f(0.0f, 1.0f);
        glEnd();
        /* Deliberately leave a small scissor enabled at presentation. The
         * capture blit must ignore it while restoring it for the renderer. */
        glScissor(width / 4, height / 4, width / 2, height / 2);
        glEnable(GL_SCISSOR_TEST);
        glXSwapBuffers(display, window);
        if (!glIsEnabled(GL_SCISSOR_TEST)) {
            fputs("glx-pattern: capture hook did not restore scissor state\n", stderr);
            running = 0;
        }
        /* Avoid pointlessly monopolising a CPU core with vblank disabled. */
        usleep(1000);
    }
    glXMakeCurrent(display, None, NULL);
    glXDestroyContext(display, context);
    XDestroyWindow(display, window);
    XCloseDisplay(display);
    return 0;
}
