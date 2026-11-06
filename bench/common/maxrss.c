/* maxrss.c — run a command and report its peak resident set size.
 *
 *   maxrss <program> [args...]   ->  child's stdout/stderr pass through, then
 *                                    "maxrss_kb=<n> cpu_s=<user+sys seconds>" on stderr;
 *                                    exit code = child's.
 *
 * Why not measure from Python directly: on Linux, ru_maxrss survives execve(), so a
 * child forked from the (~17 MB) Python harness reports at least Python's RSS.
 * This wrapper is tiny, so the floor it adds is ~1-2 MB.
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char** argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: maxrss <program> [args...]\n");
        return 2;
    }
    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 2;
    }
    if (pid == 0) {
        execvp(argv[1], argv + 1);
        perror("execvp");
        _exit(127);
    }
    int status = 0;
    struct rusage ru;
    if (wait4(pid, &status, 0, &ru) < 0) {
        perror("wait4");
        return 2;
    }
    fflush(stdout);
    fprintf(stderr, "maxrss_kb=%ld cpu_s=%.6f\n", ru.ru_maxrss,
            (double)ru.ru_utime.tv_sec + (double)ru.ru_utime.tv_usec * 1e-6 + (double)ru.ru_stime.tv_sec + (double)ru.ru_stime.tv_usec * 1e-6);
    if (WIFEXITED(status)) return WEXITSTATUS(status);
    return 128 + (WIFSIGNALED(status) ? WTERMSIG(status) : 0);
}
