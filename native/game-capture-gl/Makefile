CC ?= cc
CXX ?= c++
CFLAGS ?= -O3 -pipe -fPIC -fvisibility=hidden -Wall -Wextra -Wpedantic -Werror
LDFLAGS ?= -shared -Wl,-z,relro,-z,now,-Bsymbolic-functions

OUT := build/libluma-game-capture-gl.so
SRC := src/luma_game_capture_gl.c src/luma_nvenc_direct.cpp
OBJECTS := build/luma_game_capture_gl.o build/luma_nvenc_direct.o

.PHONY: all clean test integration-glx visual-integration-glx runtime-inject-lwjgl2-glx generic-inject-integration-glx

all: $(OUT)

$(OUT): $(OBJECTS)
	$(CXX) $(LDFLAGS) -o $@ $^ -ldl -lGL -lEGL -pthread

build/luma_game_capture_gl.o: src/luma_game_capture_gl.c | build
	$(CC) $(CFLAGS) -c -o $@ $<

build/luma_nvenc_direct.o: src/luma_nvenc_direct.cpp src/luma_nvenc_direct.h | build
	$(CXX) $(CFLAGS) -std=c++20 -c -o $@ $<

build:
	mkdir -p $@

test: $(OUT)
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

# Exercises the only runtime-injection route: remote dlopen of the capture .so,
# including LWJGL2's cached GLX dispatch slot inside a running JVM.
runtime-inject-lwjgl2-glx: $(OUT)
	./tests/attach-runtime-glx.sh

generic-inject-integration-glx: $(OUT)
	$(MAKE) -C ../game-capture-inject all
	./tests/generic-inject-glx.sh

tests/glx-pattern: tests/glx-pattern.c
	$(CC) -O2 -pipe -Wall -Wextra -Wpedantic -Werror -o $@ $< -lX11 -lGL

tests/check-pattern-ppm: tests/check-pattern-ppm.c
	$(CC) -O2 -pipe -Wall -Wextra -Wpedantic -Werror -o $@ $<

clean:
	rm -rf build tests/glx-pattern tests/check-pattern-ppm
