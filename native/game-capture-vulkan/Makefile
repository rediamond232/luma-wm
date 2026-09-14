CC ?= cc
CFLAGS ?= -O2 -g -fPIC -std=c11 -Wall -Wextra -Werror
LDFLAGS ?= -shared -pthread
BUILD_DIR := build
LIBRARY := $(BUILD_DIR)/libluma-vulkan-capture-layer.so

.PHONY: all clean
all: $(LIBRARY)

$(LIBRARY): luma_game_capture_layer.c
	mkdir -p $(BUILD_DIR)
	$(CC) $(CFLAGS) $< -o $@ $(LDFLAGS)

clean:
	rm -rf $(BUILD_DIR)
