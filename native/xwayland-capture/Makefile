CXX ?= c++
CXXFLAGS ?= -O3 -g
CXXFLAGS += -std=c++20 -Wall -Wextra -Werror -Wpedantic
DRM_CFLAGS := $(shell pkg-config --cflags libdrm)

.PHONY: all clean

all: luma-xwayland-capture

luma-xwayland-capture: luma_xwayland_capture.cpp \
		../game-capture-gl/src/luma_nvenc_direct.cpp \
		../game-capture-gl/src/luma_nvenc_direct.h
	$(CXX) $(CXXFLAGS) $(DRM_CFLAGS) luma_xwayland_capture.cpp \
		../game-capture-gl/src/luma_nvenc_direct.cpp -o $@ \
		-lxcb -lxcb-composite -lxcb-dri3 -lxcb-present -lEGL -lGL -ldl -pthread

clean:
	rm -f luma-xwayland-capture
