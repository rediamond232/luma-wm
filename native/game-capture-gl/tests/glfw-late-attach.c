#define _GNU_SOURCE
#include <GL/gl.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

typedef int (*glfw_init_fn)(void);
typedef void (*glfw_terminate_fn)(void);
typedef void *(*glfw_create_window_fn)(int, int, const char *, void *, void *);
typedef void (*glfw_make_context_current_fn)(void *);
typedef void (*glfw_swap_buffers_fn)(void *);
typedef void (*glfw_poll_events_fn)(void);

static void *required(void *library, const char *name) {
    void *symbol = dlsym(library, name);
    if (symbol == NULL) {
        fprintf(stderr, "missing GLFW symbol %s\n", name);
        exit(2);
    }
    return symbol;
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    void *library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (library == NULL) return 2;
    glfw_init_fn init;
    glfw_terminate_fn terminate;
    glfw_create_window_fn create_window;
    glfw_make_context_current_fn make_current;
    glfw_swap_buffers_fn swap_buffers;
    glfw_poll_events_fn poll_events;
    void *symbol = required(library, "glfwInit");
    memcpy(&init, &symbol, sizeof(init));
    symbol = required(library, "glfwTerminate");
    memcpy(&terminate, &symbol, sizeof(terminate));
    symbol = required(library, "glfwCreateWindow");
    memcpy(&create_window, &symbol, sizeof(create_window));
    symbol = required(library, "glfwMakeContextCurrent");
    memcpy(&make_current, &symbol, sizeof(make_current));
    symbol = required(library, "glfwSwapBuffers");
    memcpy(&swap_buffers, &symbol, sizeof(swap_buffers));
    symbol = required(library, "glfwPollEvents");
    memcpy(&poll_events, &symbol, sizeof(poll_events));
    if (!init()) return 3;
    void *window = create_window(320, 240, "Luma GLFW late attach", NULL, NULL);
    if (window == NULL) return 4;
    make_current(window);
    printf("%ld\n", (long)getpid());
    fflush(stdout);
    for (unsigned frame = 0; frame < 1200; ++frame) {
        float phase = (float)(frame % 120) / 119.0f;
        glClearColor(phase, 0.2f, 1.0f - phase, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        swap_buffers(window);
        poll_events();
        usleep(10000);
    }
    terminate();
    dlclose(library);
    return 0;
}
