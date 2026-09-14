CC ?= cc
CFLAGS ?= -O2 -pipe -Wall -Wextra -Wpedantic -Werror

.PHONY: all clean

all: build/luma-game-inject

build/luma-game-inject: luma_game_inject.c | build
	$(CC) $(CFLAGS) -o $@ $< -ldl

build:
	mkdir -p $@

clean:
	rm -rf build
