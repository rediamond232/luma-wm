#define _GNU_SOURCE

#include <dlfcn.h>
#include <dirent.h>
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ptrace.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/user.h>
#include <sys/wait.h>
#include <unistd.h>

/* Stop-the-world freeze budget: every target thread is held stopped while one
 * victim thread executes the two tiny remote calls (dlopen, hook-init spawn).
 * No signal can be delivered, no loader lock taken, no safepoint requested and
 * no watchdog can run mid-hijack. On resume only wall-clock time has jumped. */
#define LUMA_MAX_TIDS 1024

static int proc_uid(pid_t pid, uid_t *uid) {
    char path[64], line[256];
    snprintf(path, sizeof(path), "/proc/%ld/status", (long)pid);
    FILE *file = fopen(path, "re");
    if (file == NULL) return -1;
    int found = -1;
    while (fgets(line, sizeof(line), file) != NULL) {
        unsigned value = 0;
        if (sscanf(line, "Uid:\t%u", &value) == 1) {
            *uid = (uid_t)value;
            found = 0;
            break;
        }
    }
    fclose(file);
    return found;
}

static int target_is_x86_64(pid_t pid) {
    char path[64];
    snprintf(path, sizeof(path), "/proc/%ld/exe", (long)pid);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return 0;
    Elf64_Ehdr header;
    const ssize_t length = read(fd, &header, sizeof(header));
    close(fd);
    return length == (ssize_t)sizeof(header) &&
           memcmp(header.e_ident, ELFMAG, SELFMAG) == 0 &&
           header.e_ident[EI_CLASS] == ELFCLASS64 && header.e_machine == EM_X86_64;
}

static uintptr_t remote_module_bias(pid_t pid, const char *module_path) {
    char maps_path[64], line[PATH_MAX + 160];
    snprintf(maps_path, sizeof(maps_path), "/proc/%ld/maps", (long)pid);
    FILE *maps = fopen(maps_path, "re");
    if (maps == NULL) return 0;
    uintptr_t bias = 0;
    while (fgets(line, sizeof(line), maps) != NULL) {
        unsigned long start = 0, end = 0, offset = 0;
        char permissions[8], device[32], path[PATH_MAX];
        unsigned long inode = 0;
        path[0] = '\0';
        if (sscanf(line, "%lx-%lx %7s %lx %31s %lu %4095[^\n]", &start, &end,
                   permissions, &offset, device, &inode, path) != 7) {
            continue;
        }
        char *name = path;
        while (*name == ' ') ++name;
        if (strcmp(name, module_path) == 0) {
            bias = (uintptr_t)start - (uintptr_t)offset;
            break;
        }
    }
    fclose(maps);
    return bias;
}

static void report_remote_fault(pid_t pid) {
    struct user_regs_struct regs;
    siginfo_t info;
    if (ptrace(PTRACE_GETREGS, pid, NULL, &regs) != 0) return;
    void *fault = NULL;
    if (ptrace(PTRACE_GETSIGINFO, pid, NULL, &info) == 0) fault = info.si_addr;
    fprintf(stderr, "remote fault: rip=%#llx address=%p\n",
            (unsigned long long)regs.rip, fault);
    fprintf(stderr, "remote registers: rsp=%#llx rax=%#llx rdi=%#llx rsi=%#llx\n",
            (unsigned long long)regs.rsp, (unsigned long long)regs.rax,
            (unsigned long long)regs.rdi, (unsigned long long)regs.rsi);
    char path[64], line[PATH_MAX + 160];
    snprintf(path, sizeof(path), "/proc/%ld/maps", (long)pid);
    FILE *maps = fopen(path, "re");
    if (maps == NULL) return;
    while (fgets(line, sizeof(line), maps) != NULL) {
        unsigned long start = 0, end = 0;
        if (sscanf(line, "%lx-%lx", &start, &end) == 2 &&
            regs.rip >= start && regs.rip < end) {
            fprintf(stderr, "remote fault mapping: %s", line);
            break;
        }
    }
    fclose(maps);
}

static int list_task_ids(pid_t pid, pid_t *tids, size_t capacity, size_t *count) {
    char dirpath[64];
    snprintf(dirpath, sizeof(dirpath), "/proc/%ld/task", (long)pid);
    DIR *dir = opendir(dirpath);
    if (dir == NULL) return -1;
    size_t found = 0;
    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
        if (entry->d_name[0] == '.') continue;
        char *end = NULL;
        const long tid = strtol(entry->d_name, &end, 10);
        if (end == entry->d_name || *end != '\0' || tid <= 0 || tid > INT_MAX) continue;
        if (found >= capacity) {
            closedir(dir);
            errno = E2BIG;
            return -1;
        }
        tids[found++] = (pid_t)tid;
    }
    closedir(dir);
    if (found == 0) return -1;
    /* The group leader is the hijack victim; keep it first so every exit path
     * knows which TID owns saved registers. */
    for (size_t index = 0; index < found; ++index) {
        if (tids[index] == pid && index != 0) {
            const pid_t swap = tids[0];
            tids[0] = pid;
            tids[index] = swap;
            break;
        }
    }
    if (tids[0] != pid) return -1;
    *count = found;
    return 0;
}

/* Nonzero SigPnd/ShdPnd means a signal (e.g. HotSpot's implicit-null-check
 * SIGSEGV or a safepoint-poll trap) is already queued for this thread. Running
 * foreign code with a stale signal pending would deliver the handler into the
 * stub context with a garbage PC, which a JVM reads as a genuine crash. */
static int task_has_pending_signals(pid_t pid, pid_t tid) {
    char path[96];
    snprintf(path, sizeof(path), "/proc/%ld/task/%ld/status", (long)pid, (long)tid);
    FILE *file = fopen(path, "re");
    if (file == NULL) return -1;
    unsigned long long pending = 0, shared = 0;
    int seen = 0;
    char line[256];
    while (fgets(line, sizeof(line), file) != NULL) {
        if (strncmp(line, "SigPnd:", 7) == 0 || strncmp(line, "ShdPnd:", 7) == 0) {
            const char *hex = strchr(line, ':');
            hex = hex == NULL ? "" : hex + 1;
            while (*hex == ' ' || *hex == '\t') ++hex;
            const unsigned long long value = strtoull(hex, NULL, 16);
            if (strncmp(line, "SigPnd:", 7) == 0) pending = value;
            else shared = value;
            ++seen;
        }
    }
    fclose(file);
    if (seen < 2) return -1;
    return (pending != 0 || shared != 0) ? 1 : 0;
}

static int wait_tid_stop(pid_t tid, int *stop_sig) {
    for (;;) {
        int status = 0;
        const pid_t got = waitpid(tid, &status, __WALL);
        if (got != tid) {
            if (errno == EINTR) continue;
            return -1;
        }
        if (WIFSTOPPED(status)) {
            *stop_sig = WSTOPSIG(status);
            return 0;
        }
        errno = ESRCH;
        return -1;
    }
}

static void detach_world(const pid_t *tids, const int *signos, size_t count) {
    for (size_t index = 0; index < count; ++index) {
        int signo = (signos != NULL) ? signos[index] : 0;
        if (signo < 0) signo = 0;
        (void)ptrace(PTRACE_DETACH, tids[index], NULL, (void *)(long)signo);
    }
}

/* True only when [address, address + length) lies fully inside one mapping
 * with the write bit set. Guards the scratch-stack poke against guard pages
 * and read-only stacks. */
static int mapping_writable(pid_t pid, uintptr_t address, size_t length) {
    if (length == 0) return 0;
    char path[64];
    snprintf(path, sizeof(path), "/proc/%ld/maps", (long)pid);
    FILE *maps = fopen(path, "re");
    if (maps == NULL) return 0;
    char line[PATH_MAX + 160];
    int writable = 0;
    while (fgets(line, sizeof(line), maps) != NULL) {
        unsigned long start = 0, end = 0;
        char permissions[8] = {0};
        if (sscanf(line, "%lx-%lx %7s", &start, &end, permissions) != 3) continue;
        if ((uintptr_t)start <= address && address + length <= (uintptr_t)end) {
            writable = permissions[1] == 'w';
            break;
        }
        if ((uintptr_t)start > address) break;
    }
    fclose(maps);
    return writable;
}

static int poke_bytes(pid_t pid, uintptr_t address, const void *bytes, size_t length) {
    const unsigned char *source = bytes;
    for (size_t offset = 0; offset < length; offset += sizeof(long)) {
        long word = 0;
        const size_t count = length - offset < sizeof(word) ? length - offset : sizeof(word);
        if (count != sizeof(word)) {
            errno = 0;
            word = ptrace(PTRACE_PEEKDATA, pid, (void *)(address + offset), NULL);
            if (word == -1 && errno != 0) return -1;
        }
        memcpy(&word, source + offset, count);
        if (ptrace(PTRACE_POKEDATA, pid, (void *)(address + offset), (void *)word) != 0) {
            return -1;
        }
    }
    return 0;
}

static int inject_library(pid_t pid, const char *library) {
    void *local_dlopen = dlsym(RTLD_DEFAULT, "dlopen");
    Dl_info info;
    if (local_dlopen == NULL || dladdr(local_dlopen, &info) == 0 || info.dli_fbase == NULL ||
        info.dli_fname == NULL) {
        fputs("cannot resolve the local dlopen implementation\n", stderr);
        return 1;
    }
    char module_path[PATH_MAX];
    if (realpath(info.dli_fname, module_path) == NULL) {
        perror("realpath dlopen module");
        return 1;
    }
    const uintptr_t remote_bias = remote_module_bias(pid, module_path);
    if (remote_bias == 0) {
        fprintf(stderr, "target does not map the injector's %s; mixed libc injection is unsupported\n",
                module_path);
        return 1;
    }
    const uintptr_t remote_dlopen = remote_bias +
        ((uintptr_t)local_dlopen - (uintptr_t)info.dli_fbase);

    char canonical_library[PATH_MAX];
    if (realpath(library, canonical_library) == NULL) {
        perror("realpath capture library");
        return 1;
    }
    void *local_hook = dlopen(canonical_library, RTLD_NOW | RTLD_LOCAL);
    void *local_init = local_hook == NULL ? NULL : dlsym(local_hook, "luma_install_injected_hooks");
    Dl_info init_info;
    if (local_init == NULL || dladdr(local_init, &init_info) == 0 ||
        init_info.dli_fbase == NULL) {
        fputs("capture library has no injected-hook initializer\n", stderr);
        if (local_hook != NULL) dlclose(local_hook);
        return 1;
    }
    const uintptr_t init_offset = (uintptr_t)local_init - (uintptr_t)init_info.dli_fbase;

    /* Stop-the-world freeze: enumerate every thread and refuse to hijack a
     * target with queued signals before anything is stopped. A pending
     * SIGSEGV delivered into the stub context is read by HotSpot as a
     * genuine crash (hs_err_pid + abort). */
    pid_t tids[LUMA_MAX_TIDS];
    size_t ntids = 0;
    if (list_task_ids(pid, tids, LUMA_MAX_TIDS, &ntids) != 0) {
        fprintf(stderr, "cannot enumerate threads of PID %ld; process may be exiting\n",
                (long)pid);
        dlclose(local_hook);
        return 1;
    }
    for (size_t index = 0; index < ntids; ++index) {
        const int pending = task_has_pending_signals(pid, tids[index]);
        if (pending != 0) {
            if (pending > 0) {
                fprintf(stderr, "thread %ld has pending signals; refusing to hijack "
                        "(retry when the target is idle)\n", (long)tids[index]);
            } else {
                fprintf(stderr, "cannot inspect signals of thread %ld; refusing to hijack\n",
                        (long)tids[index]);
            }
            dlclose(local_hook);
            return 1;
        }
    }

    /* Freeze the world. Every TID is stopped so no signal is delivered, no
     * loader lock is taken, no safepoint is requested and no watchdog runs
     * while the victim executes foreign code. */
    int deliver[LUMA_MAX_TIDS] = {0};
    size_t nstopped = 0;
    int result = 1;
    struct user_regs_struct saved, call;
    long saved_instruction = 0;
    int have_saved = 0;
    for (size_t index = 0; index < ntids; ++index) {
        if (ptrace(PTRACE_ATTACH, tids[index], NULL, NULL) != 0) {
            if (errno == ESRCH) {
                fprintf(stderr, "thread %ld exited during freeze; retry the injection\n",
                        (long)tids[index]);
            } else {
                fprintf(stderr, "cannot attach to thread %ld of PID %ld: %s "
                        "(Yama/anti-cheat may forbid ptrace)\n",
                        (long)tids[index], (long)pid, strerror(errno));
            }
            goto detach;
        }
    }
    for (size_t index = 0; index < ntids; ++index) {
        int sig = 0;
        if (wait_tid_stop(tids[index], &sig) != 0) {
            fprintf(stderr, "thread %ld did not stop cleanly; retry the injection\n",
                    (long)tids[index]);
            goto detach;
        }
        ++nstopped;
        if (sig != SIGSTOP) {
            /* A foreign signal (not our attach stop) owns this thread now.
             * Deliver it on detach instead of running foreign code under it.
             * The attach-induced SIGSTOP itself must never be redelivered:
             * the tracer already consumed it, so detach uses 0 for every
             * cleanly frozen thread. */
            fprintf(stderr, "thread %ld stopped with signal %d; refusing to hijack "
                    "a live signal context (retry when idle)\n",
                    (long)tids[index], sig);
            deliver[index] = sig;
            goto detach;
        }
    }
    /* One retry round for threads spawned between enumeration and freeze. */
    {
        pid_t fresh[LUMA_MAX_TIDS];
        size_t nfresh = 0;
        if (list_task_ids(pid, fresh, LUMA_MAX_TIDS, &nfresh) == 0 && nfresh > ntids) {
            for (size_t index = 0; index < nfresh && ntids < LUMA_MAX_TIDS; ++index) {
                size_t known = 0;
                while (known < ntids && tids[known] != fresh[index]) ++known;
                if (known < ntids) continue;
                if (ptrace(PTRACE_ATTACH, fresh[index], NULL, NULL) != 0) continue;
                int sig = 0;
                if (wait_tid_stop(fresh[index], &sig) != 0) {
                    (void)ptrace(PTRACE_DETACH, fresh[index], NULL, NULL);
                    continue;
                }
                tids[ntids] = fresh[index];
                deliver[ntids] = 0;
                ++ntids;
                ++nstopped;
                if (sig != SIGSTOP) {
                    fprintf(stderr, "thread %ld spawned mid-freeze with signal %d; "
                            "aborting this attempt\n", (long)fresh[index], sig);
                    deliver[ntids - 1] = sig;
                    goto detach;
                }
            }
        }
    }
    const pid_t victim = tids[0];

    if (ptrace(PTRACE_GETREGS, victim, NULL, &saved) != 0) goto detach;
    errno = 0;
    saved_instruction = ptrace(PTRACE_PEEKTEXT, victim, (void *)saved.rip, NULL);
    if (saved_instruction == -1 && errno != 0) goto detach;
    have_saved = 1;

    const uintptr_t path_address = (saved.rsp - 1024U) & ~(uintptr_t)15U;
    const uintptr_t call_stack = (path_address - 1024U) & ~(uintptr_t)15U;
    const size_t path_length = strlen(library) + 1;
    if (call_stack >= (uintptr_t)saved.rsp ||
        !mapping_writable(pid, call_stack, (size_t)((uintptr_t)saved.rsp - call_stack)) ||
        !mapping_writable(pid, path_address, path_length)) {
        fputs("victim stack has no writable scratch room; refusing to hijack\n", stderr);
        goto restore;
    }
    /* Execute a real CALL so Intel CET shadow stacks, when enabled by the
     * target, observe a matching return address. */
    const unsigned char call_and_trap[] = {0xff, 0xd0, 0xcc}; /* call *%rax; int3 */
    if (poke_bytes(victim, path_address, library, path_length) != 0 ||
        poke_bytes(victim, saved.rip, call_and_trap, sizeof(call_and_trap)) != 0) {
        goto restore;
    }
    call = saved;
    call.rip = saved.rip;
    call.rsp = call_stack;
    call.rax = remote_dlopen;
    call.rdi = path_address;
    call.rsi = RTLD_NOW | RTLD_LOCAL;
    if (ptrace(PTRACE_SETREGS, victim, NULL, &call) != 0 ||
        ptrace(PTRACE_CONT, victim, NULL, NULL) != 0) {
        goto restore;
    }
    {
        int sig = 0;
        if (wait_tid_stop(victim, &sig) != 0 || sig != SIGTRAP) {
            fprintf(stderr, "target did not return cleanly from dlopen (signal=%d)\n", sig);
            /* The victim may be running inside dlopen; only the tracer-side
             * state is restored here, then every thread is detached so the
             * target keeps running instead of being left stopped. */
            if (have_saved) {
                (void)ptrace(PTRACE_POKETEXT, victim, (void *)saved.rip,
                             (void *)saved_instruction);
                (void)ptrace(PTRACE_SETREGS, victim, NULL, &saved);
                have_saved = 0;
            }
            goto detach;
        }
    }
    if (ptrace(PTRACE_GETREGS, victim, NULL, &call) != 0 || call.rax == 0) {
        fputs("remote dlopen rejected the capture library\n", stderr);
        goto restore;
    }

    /* Module scanning and LWJGL dispatch discovery must run after dlopen has
     * released the target loader's recursive constructor context. */
    const uintptr_t hook_bias = remote_module_bias(pid, canonical_library);
    if (hook_bias == 0) {
        fputs("capture library was loaded but its remote mapping was not found\n", stderr);
        goto restore;
    }
    call = saved;
    call.rip = saved.rip;
    call.rsp = call_stack;
    call.rax = hook_bias + init_offset;
    call.rdi = 0;
    if (ptrace(PTRACE_SETREGS, victim, NULL, &call) != 0 ||
        ptrace(PTRACE_CONT, victim, NULL, NULL) != 0) {
        goto restore;
    }
    {
        int sig = 0;
        if (wait_tid_stop(victim, &sig) != 0 || sig != SIGTRAP) {
            fprintf(stderr, "target did not return cleanly from hook initialization (signal=%d)\n", sig);
            if (sig != 0) report_remote_fault(pid);
            if (have_saved) {
                (void)ptrace(PTRACE_POKETEXT, victim, (void *)saved.rip,
                             (void *)saved_instruction);
                (void)ptrace(PTRACE_SETREGS, victim, NULL, &saved);
                have_saved = 0;
            }
            goto detach;
        }
    }
    if (ptrace(PTRACE_GETREGS, victim, NULL, &call) != 0 || (int32_t)call.rax != 0) {
        fputs("capture library could not install a supported graphics hook\n", stderr);
        goto restore;
    }
    result = 0;

restore:
    if (have_saved) {
        (void)ptrace(PTRACE_POKETEXT, victim, (void *)saved.rip, (void *)saved_instruction);
        (void)ptrace(PTRACE_SETREGS, victim, NULL, &saved);
    }
detach:
    /* Resume the world: every cleanly frozen thread was SIGSTOP-held by the
     * freeze, and the tracer consumed that stop, so detach with 0. Only a
     * thread refused for a foreign signal keeps its signal for delivery.
     * From inside the process only wall-clock time has jumped. */
    if (nstopped > ntids) nstopped = ntids;
    if (nstopped > 0) {
        detach_world(tids, deliver, nstopped);
    } else {
        /* Nothing was confirmed stopped (early attach failure): attempt a
         * best-effort detach of anything we may have attached. */
        for (size_t index = 0; index < ntids; ++index) {
            (void)ptrace(PTRACE_DETACH, tids[index], NULL, NULL);
        }
    }
    dlclose(local_hook);
    return result;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "Usage: %s PID /absolute/capture-hook.so\n", argv[0]);
        return 64;
    }
    char *end = NULL;
    const long parsed = strtol(argv[1], &end, 10);
    if (end == argv[1] || *end != '\0' || parsed <= 1 || parsed > INT_MAX || argv[2][0] != '/') {
        fputs("PID and absolute library path are required\n", stderr);
        return 64;
    }
    const pid_t pid = (pid_t)parsed;
    uid_t uid = (uid_t)-1;
    if (proc_uid(pid, &uid) != 0 || uid != geteuid()) {
        fputs("target must be an accessible process owned by the current user\n", stderr);
        return 77;
    }
    struct stat metadata;
    if (stat(argv[2], &metadata) != 0 || !S_ISREG(metadata.st_mode) || metadata.st_uid != geteuid()) {
        fputs("capture hook must be a current-user-owned regular file\n", stderr);
        return 66;
    }
    if (!target_is_x86_64(pid)) {
        fputs("this injector currently supports x86-64 targets only\n", stderr);
        return 65;
    }
    return inject_library(pid, argv[2]);
}
