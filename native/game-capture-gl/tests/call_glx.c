#define _GNU_SOURCE
#include <GL/glx.h>
#include <dlfcn.h>
#include <stdio.h>

typedef void (*swap_fn)(Display *, GLXDrawable);

int main(void) {
    void *symbol = dlsym(RTLD_DEFAULT, "glXSwapBuffers");
    swap_fn swap = NULL;
    if (symbol == NULL) {
        return 2;
    }
    _Static_assert(sizeof(symbol) == sizeof(swap), "dlsym pointer size mismatch");
    __builtin_memcpy(&swap, &symbol, sizeof(swap));
    swap(NULL, 7);
    /* A second drawable represents an overlay/auxiliary GL context. */
    swap(NULL, 8);
    puts("called");
    return 0;
}
