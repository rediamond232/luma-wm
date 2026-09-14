#include "nvenc_gl_encoder.hpp"

#include <EGL/egl.h>
#include <iostream>

int main() {
    // This confirms headers, dynamic-NVENC wiring, and ABI compile without
    // creating a game hook or requiring a display/GPU context in CI.
    std::cout << "NVENC/OpenGL prototype compiled. Runtime needs a current EGL context.\n";
    std::cout << "eglGetCurrentContext=" << eglGetCurrentContext() << '\n';
    return 0;
}
