/* Test-only stand-in for Lunar's obs-gamecapture wrapper (libobs_glcapture.so).
 * It interposes glXSwapBuffers exactly like a capture shim does: with this
 * library preloaded, the game's PLT slot resolves here first and the real
 * driver is reached only through RTLD_NEXT. Luma must chain through the shim
 * (game -> Luma -> shim -> driver) instead of refusing the slot. */
#define _GNU_SOURCE

#include <GL/glx.h>
#include <dlfcn.h>
#include <stdatomic.h>
#include <string.h>

typedef void (*glx_swap_fn)(Display *, GLXDrawable);

atomic_ulong luma_test_shim_swaps = 0;

__attribute__((visibility("default")))
void glXSwapBuffers(Display *display, GLXDrawable drawable) {
    void *symbol = dlsym(RTLD_NEXT, "glXSwapBuffers");
    glx_swap_fn real = NULL;
    memcpy(&real, &symbol, sizeof(real));
    atomic_fetch_add_explicit(&luma_test_shim_swaps, 1, memory_order_relaxed);
    if (real != NULL) {
        real(display, drawable);
    }
}
