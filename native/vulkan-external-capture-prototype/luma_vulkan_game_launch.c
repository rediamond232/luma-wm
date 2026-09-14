#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s game [args...]\n", argv[0]);
        return 64;
    }
    if (setsid() < 0) {
        fprintf(stderr, "cannot isolate Vulkan game session: %s\n", strerror(errno));
        return 70;
    }
    /* The parent publishes this stable PID to both authenticated sidecars and
     * then resumes us only after their sockets are ready. execvp preserves it. */
    if (raise(SIGSTOP) != 0) {
        fprintf(stderr, "cannot gate Vulkan game launch: %s\n", strerror(errno));
        return 70;
    }
    execvp(argv[1], &argv[1]);
    fprintf(stderr, "cannot start Vulkan game %s: %s\n", argv[1], strerror(errno));
    return 127;
}
