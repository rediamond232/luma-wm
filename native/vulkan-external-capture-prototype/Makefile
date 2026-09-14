CC ?= cc
CXX ?= c++
CFLAGS ?= -O2 -g
CFLAGS += -std=c11 -fPIC -Wall -Wextra -Werror -Wpedantic
CXXFLAGS ?= -O3 -g
CXXFLAGS += -std=c++20 -Wall -Wextra -Werror -Wpedantic
DRM_CFLAGS := $(shell pkg-config --cflags libdrm)

.PHONY: all clean integration-vkcube

all: libluma_vk_dmabuf_capture.so luma-vulkan-capture-receiver luma-vulkan-game-launch

integration-vkcube: all
	./tests/integration-vkcube.sh

libluma_vk_dmabuf_capture.so: luma_vk_dmabuf_capture_layer.c luma_vk_export_protocol.h
	$(CC) $(CFLAGS) $(DRM_CFLAGS) $< -o $@ -shared -pthread

luma-vulkan-capture-receiver: luma_vk_capture_receiver.cpp luma_vk_export_protocol.h \
		../game-capture-gl/src/luma_nvenc_direct.cpp \
		../game-capture-gl/src/luma_nvenc_direct.h
	$(CXX) $(CXXFLAGS) luma_vk_capture_receiver.cpp \
		../game-capture-gl/src/luma_nvenc_direct.cpp -o $@ -lEGL -lGL -ldl -pthread

luma-vulkan-game-launch: luma_vulkan_game_launch.c
	$(CC) -O2 -pipe -std=c11 -Wall -Wextra -Werror -Wpedantic $< -o $@

clean:
	rm -f libluma_vk_dmabuf_capture.so luma-vulkan-capture-receiver luma-vulkan-game-launch
