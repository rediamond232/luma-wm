#define _POSIX_C_SOURCE 200809L
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GL/gl.h>
#include <stdio.h>
#include <signal.h>
#include <time.h>

static volatile sig_atomic_t running = 1;
static void stop(int sig) { (void)sig; running = 0; }
static double now(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec + (double)t.tv_nsec / 1e9;
}
int main(void) {
    EGLDisplay display = eglGetPlatformDisplay(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
    if (!eglInitialize(display, NULL, NULL) || !eglBindAPI(EGL_OPENGL_API)) return 2;
    EGLint attributes[] = {EGL_SURFACE_TYPE, EGL_PBUFFER_BIT, EGL_RENDERABLE_TYPE, EGL_OPENGL_BIT,
        EGL_RED_SIZE, 8, EGL_GREEN_SIZE, 8, EGL_BLUE_SIZE, 8, EGL_NONE};
    EGLConfig config; EGLint count;
    if (!eglChooseConfig(display, attributes, &config, 1, &count) || !count) return 3;
    const int width = 2560, height = 1440;
    EGLint size[] = {EGL_WIDTH, width, EGL_HEIGHT, height, EGL_NONE};
    EGLSurface surface = eglCreatePbufferSurface(display, config, size);
    EGLContext context = eglCreateContext(display, config, EGL_NO_CONTEXT, NULL);
    if (!eglMakeCurrent(display, surface, surface, context)) return 4;
    signal(SIGTERM, stop); signal(SIGINT, stop);
    fprintf(stderr, "READY EGL %s\n", glGetString(GL_RENDERER));
    const double start = now(); double report = start;
    unsigned frame = 0;
    while (running && now() - start < 12) {
        glEnable(GL_SCISSOR_TEST);
        const float colors[4][3] = {{0.05f, 0.05f, 1}, {1, 0.95f, 0.05f}, {1, 0.05f, 0.05f}, {0.05f, 1, 0.05f}};
        for (int i = 0; i < 4; ++i) {
            glScissor((i % 2) * width / 2, (i / 2) * height / 2, width / 2, height / 2);
            glClearColor(colors[i][0], colors[i][1], colors[i][2], 1);
            glClear(GL_COLOR_BUFFER_BIT);
        }
        glScissor(0, 0, 32, 32);
        glClearColor((float)(frame % 251) / 250, 0, 0, 1); glClear(GL_COLOR_BUFFER_BIT);
        if (!eglSwapBuffers(display, surface)) return 5;
        if (!glIsEnabled(GL_SCISSOR_TEST) || glGetError() != GL_NO_ERROR) return 6;
        ++frame;
        if (now() - report >= 0.25) { fprintf(stderr, "FRAME %u\n", frame); report = now(); }
        const struct timespec delay = {0, 1000000}; nanosleep(&delay, NULL);
    }
    fprintf(stderr, "FRAMES %u\n", frame);
    eglMakeCurrent(display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    eglDestroyContext(display, context); eglDestroySurface(display, surface); eglTerminate(display);
    return 0;
}
