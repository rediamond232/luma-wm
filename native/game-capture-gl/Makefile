CC ?= cc
CXX ?= c++
CFLAGS ?= -O3 -pipe -fPIC -fvisibility=hidden -Wall -Wextra -Wpedantic -Werror
LDFLAGS ?= -shared -Wl,-z,relro,-z,now,-z,defs,-Bsymbolic-functions

OUT := build/libluma-game-capture-gl.so
SRC := src/luma_game_capture_gl.c src/luma_gl_export.cpp
OBJECTS := build/luma_game_capture_gl.o build/luma_gl_export.o
RECEIVER := build/luma-opengl-capture-receiver

.PHONY: all clean test integration-glx visual-integration-glx launcher-child-integration-glx runtime-inject-lwjgl2-glx generic-inject-integration-glx shim-interpose-integration-glx rerecord-integration-glx

all: $(OUT) $(RECEIVER)

.PHONY: shared-vram-integration-egl
shared-vram-integration-egl: all tests/egl-pattern tests/check-pattern-ppm
	bash ./tests/shared-vram-egl.sh

tests/egl-pattern: tests/egl-pattern.c
	$(CC) -O2 -Wall -Wextra -Wpedantic -Werror -o $@ $< -lEGL -lGL

test integration-glx visual-integration-glx launcher-child-integration-glx runtime-inject-lwjgl2-glx generic-inject-integration-glx shim-interpose-integration-glx rerecord-integration-glx: $(RECEIVER)

$(OUT): $(OBJECTS)
	$(CXX) $(LDFLAGS) -o $@ $^ -ldl -lGL -lEGL -lvulkan -pthread

build/luma_gl_export.o: src/luma_gl_export.cpp src/luma_capture_pacing.h src/luma_gl_shared_memory.h src/luma_gl_vram.h src/luma_nvenc_direct.h ../vulkan-external-capture-prototype/luma_vk_export_protocol.h | build
	$(CXX) $(CFLAGS) -std=c++20 -c -o $@ $<

$(RECEIVER): ../vulkan-external-capture-prototype/luma_vk_capture_receiver.cpp ../vulkan-external-capture-prototype/luma_vk_export_protocol.h src/luma_gl_shared_memory.h build/luma_nvenc_direct.o
	$(CXX) $(CFLAGS) -std=c++20 -o $@ $< build/luma_nvenc_direct.o -ldl -lGL -lEGL -pthread

build/luma_game_capture_gl.o: src/luma_game_capture_gl.c | build
	$(CC) $(CFLAGS) -c -o $@ $<

build/luma_nvenc_direct.o: src/luma_nvenc_direct.cpp src/luma_nvenc_direct.h | build
	$(CXX) $(CFLAGS) -std=c++20 -c -o $@ $<

build:
	mkdir -p $@

tests/capture-pacing: tests/capture-pacing.c src/luma_capture_pacing.h
	$(CC) -O2 -Wall -Wextra -Wpedantic -Werror -o $@ $<

test: $(OUT) tests/capture-pacing
	./tests/capture-pacing
	./tests/launcher-smoke.sh "$(CURDIR)/$(OUT)"
	./tests/present-event.sh "$(CURDIR)/$(OUT)"
	./tests/stop-does-not-kill-game.sh

# Requires the active user's X11/NVIDIA session and a release muxer. This is
# intentionally opt-in and reports exit 77 when those host prerequisites are
# unavailable.
integration-glx: $(OUT)
	./tests/direct-runtime-glx.sh

# A visual, end-to-end fixture. Unlike integration-glx, this target draws a
# deterministic four-colour image and verifies the decoded recording.
visual-integration-glx: $(OUT) tests/glx-pattern tests/check-pattern-ppm
	./tests/visual-runtime-glx.sh

# Verifies the launcher-safe profile mode: the named GL child inherits the
# preload and authenticates as a descendant without runtime ptrace attach.
launcher-child-integration-glx: $(OUT) tests/glx-pattern tests/check-pattern-ppm
	LUMA_GAME_CAPTURE_TARGET_PROCESS_NAME=glx-pattern ./tests/visual-runtime-glx.sh

# Exercises the only runtime-injection route: remote dlopen of the capture .so,
# including LWJGL2's cached GLX dispatch slot inside a running JVM.
runtime-inject-lwjgl2-glx: $(OUT)
	./tests/attach-runtime-glx.sh

generic-inject-integration-glx: $(OUT)
	$(MAKE) -C ../game-capture-inject all
	./tests/generic-inject-glx.sh

# Lunar wraps its JVM with obs-gamecapture, so the swap slot points at the
# shim instead of libGL. This fixture preloads a fake libobs_glcapture.so and
# requires Luma to chain through it (game -> Luma -> shim -> driver).
shim-interpose-integration-glx: $(OUT)
	$(MAKE) -C ../game-capture-inject all
	./tests/shim-interpose-glx.sh

# Repeat recordings on one process skip ptrace: the resident hook re-arms
# itself on its own present thread when the second session config appears.
rerecord-integration-glx: $(OUT)
	$(MAKE) -C ../game-capture-inject all
	./tests/rerecord-glx.sh

tests/glx-pattern: tests/glx-pattern.c
	$(CC) -O2 -pipe -Wall -Wextra -Wpedantic -Werror -o $@ $< -lX11 -lGL

tests/check-pattern-ppm: tests/check-pattern-ppm.c
	$(CC) -O2 -pipe -Wall -Wextra -Wpedantic -Werror -o $@ $<

tests/glfw-late-attach: tests/glfw-late-attach.c
	$(CC) -O2 -pipe -Wall -Wextra -Wpedantic -Werror -o $@ $< -ldl -lGL

clean:
	rm -rf build tests/glx-pattern tests/check-pattern-ppm tests/glfw-late-attach tests/egl-pattern tests/capture-pacing
