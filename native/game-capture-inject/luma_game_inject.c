#define _GNU_SOURCE

#include <dlfcn.h>
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

    if (ptrace(PTRACE_ATTACH, pid, NULL, NULL) != 0) {
        fprintf(stderr, "cannot attach to PID %ld: %s (Yama/anti-cheat may forbid ptrace)\n",
                (long)pid, strerror(errno));
        dlclose(local_hook);
        return 1;
    }
    int status = 0, result = 1;
    struct user_regs_struct saved, call;
    long saved_instruction = 0;
    if (waitpid(pid, &status, 0) != pid || !WIFSTOPPED(status)) goto detach;
    if (ptrace(PTRACE_GETREGS, pid, NULL, &saved) != 0) goto detach;
    errno = 0;
    saved_instruction = ptrace(PTRACE_PEEKTEXT, pid, (void *)saved.rip, NULL);
    if (saved_instruction == -1 && errno != 0) goto detach;

    const uintptr_t path_address = (saved.rsp - 1024U) & ~(uintptr_t)15U;
    const uintptr_t call_stack = (path_address - 1024U) & ~(uintptr_t)15U;
    /* Execute a real CALL so Intel CET shadow stacks, when enabled by the
     * target, observe a matching return address. */
    const unsigned char call_and_trap[] = {0xff, 0xd0, 0xcc}; /* call *%rax; int3 */
    if (poke_bytes(pid, path_address, library, strlen(library) + 1) != 0 ||
        poke_bytes(pid, saved.rip, call_and_trap, sizeof(call_and_trap)) != 0) {
        goto restore;
    }
    call = saved;
    call.rip = saved.rip;
    call.rsp = call_stack;
    call.rax = remote_dlopen;
    call.rdi = path_address;
    call.rsi = RTLD_NOW | RTLD_LOCAL;
    if (ptrace(PTRACE_SETREGS, pid, NULL, &call) != 0 ||
        ptrace(PTRACE_CONT, pid, NULL, NULL) != 0) {
        goto restore;
    }
    if (waitpid(pid, &status, 0) != pid || !WIFSTOPPED(status) || WSTOPSIG(status) != SIGTRAP) {
        fprintf(stderr, "target did not return cleanly from dlopen (status=%#x signal=%d)\n",
                status, WIFSTOPPED(status) ? WSTOPSIG(status) : 0);
        goto restore;
    }
    if (ptrace(PTRACE_GETREGS, pid, NULL, &call) != 0 || call.rax == 0) {
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
    if (ptrace(PTRACE_SETREGS, pid, NULL, &call) != 0 ||
        ptrace(PTRACE_CONT, pid, NULL, NULL) != 0) {
        goto restore;
    }
    if (waitpid(pid, &status, 0) != pid || !WIFSTOPPED(status) || WSTOPSIG(status) != SIGTRAP) {
        fprintf(stderr, "target did not return cleanly from hook initialization (status=%#x signal=%d)\n",
                status, WIFSTOPPED(status) ? WSTOPSIG(status) : 0);
        if (WIFSTOPPED(status)) report_remote_fault(pid);
        goto restore;
    }
    if (ptrace(PTRACE_GETREGS, pid, NULL, &call) != 0 || (int32_t)call.rax != 0) {
        fputs("capture library could not install a supported graphics hook\n", stderr);
        goto restore;
    }
    result = 0;

restore:
    (void)ptrace(PTRACE_POKETEXT, pid, (void *)saved.rip, (void *)saved_instruction);
    (void)ptrace(PTRACE_SETREGS, pid, NULL, &saved);
detach:
    (void)ptrace(PTRACE_DETACH, pid, NULL, NULL);
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
