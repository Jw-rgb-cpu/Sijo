/* sijo_wasm.h - the part of the C library a Sijo game uses, for `sijoc build --target wasm32`
 * (clang --target=wasm32 -nostdlib: no libc, no WASI). Included first, instead of the system
 * headers, when SIJO_WASM is defined. The JS loader that sijoc writes next to the .wasm provides the
 * imports below (module "env"): output, a clock, files kept in memory, and the unwinder of panics.
 *
 * - memory: a size-class allocator over memory.grow (blocks of 2^k bytes, freed blocks reused)
 * - stdio: stdout/stderr go to the loader line by line; fopen/fread/fwrite work on files the loader
 *   keeps (game.save(name) / game.load(name)); printf-style formatting for the runtime's messages
 * - math: the runtime has its own deterministic Math (sijo_rt.h); sqrt/floor/ceil/trunc/fabs are
 *   WebAssembly instructions (exact IEEE), fmodf is the exact remainder (as C's fmodf)
 * - setjmp/longjmp: a panic in game code unwinds to the embedding API call through a JS exception
 *   (the loader catches it, restores the stack pointer and calls sijo_wasm_unwound), so a panic on
 *   the web stops the game with sijo_game_error set instead of trapping the module
 * - sockets, signals, processes: not there (in-process client only); the calls fail */
#ifndef SIJO_WASM_H
#define SIJO_WASM_H
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>
#include <stdarg.h>
#include <limits.h>
#include <float.h>

#define SIJO_WIMPORT(name) __attribute__((import_module("env"), import_name(#name)))
SIJO_WIMPORT(sijo_wasm_write) void sijo_wasm_write(int fd, const char* p, int n);
SIJO_WIMPORT(sijo_wasm_now) double sijo_wasm_now(void);
SIJO_WIMPORT(sijo_wasm_unwind) void sijo_wasm_unwind(void);
SIJO_WIMPORT(sijo_wasm_exit) void sijo_wasm_exit(int code);
SIJO_WIMPORT(sijo_wasm_file_size) int sijo_wasm_file_size(const char* path, int n);
SIJO_WIMPORT(sijo_wasm_file_read) void sijo_wasm_file_read(const char* path, int n, char* dst);
SIJO_WIMPORT(sijo_wasm_file_write) void sijo_wasm_file_write(const char* path, int n, const char* src, int len);
SIJO_WIMPORT(sijo_wasm_file_remove) int sijo_wasm_file_remove(const char* path, int n);

/* ---------------- memory ---------------- */
extern unsigned char __heap_base;
static uintptr_t sijo_w_brk;
static void* sijo_w_free[32]; /* free blocks by class: block of 2^(k+4) bytes, 16-byte header */
static unsigned sijo_w_class(size_t n) {
    unsigned k = 0;
    size_t s = 16;
    while (s < n) { s <<= 1; k++; }
    return k;
}
static void* sijo_w_more(size_t bytes) {
    if (!sijo_w_brk) sijo_w_brk = ((uintptr_t)&__heap_base + 15) & ~(uintptr_t)15;
    uintptr_t end = sijo_w_brk + bytes;
    if (end < sijo_w_brk) return NULL;
    size_t have = __builtin_wasm_memory_size(0) * 65536ul;
    if (end > have) {
        size_t pages = (end - have + 65535) / 65536;
        if (__builtin_wasm_memory_grow(0, pages) == (size_t)-1) return NULL;
    }
    void* p = (void*)sijo_w_brk;
    sijo_w_brk = end;
    return p;
}
void* malloc(size_t n) {
    if (n > ((size_t)1 << 30)) return NULL;
    unsigned k = sijo_w_class(n + 16);
    unsigned char* b = (unsigned char*)sijo_w_free[k];
    if (b) {
        sijo_w_free[k] = *(void**)(b + 16);
    } else {
        b = (unsigned char*)sijo_w_more((size_t)16 << k);
        if (!b) return NULL;
    }
    *(uint32_t*)b = k;
    return b + 16;
}
void free(void* p) {
    if (!p) return;
    unsigned char* b = (unsigned char*)p - 16;
    unsigned k = *(uint32_t*)b;
    *(void**)(b + 16) = sijo_w_free[k];
    sijo_w_free[k] = b;
}
__attribute__((no_builtin)) void* memcpy(void* restrict d, const void* restrict s, size_t n) {
    unsigned char* a = (unsigned char*)d;
    const unsigned char* b = (const unsigned char*)s;
#if defined(__wasm_bulk_memory__)
    __builtin_memcpy(a, b, n);
#else
    while (n >= 8) { uint64_t t; __builtin_memcpy(&t, b, 8); __builtin_memcpy(a, &t, 8); a += 8; b += 8; n -= 8; }
    while (n--) *a++ = *b++;
#endif
    return d;
}
__attribute__((no_builtin)) void* memmove(void* d, const void* s, size_t n) {
    unsigned char* a = (unsigned char*)d;
    const unsigned char* b = (const unsigned char*)s;
    if (a == b || n == 0) return d;
    if (a < b) {
        while (n--) *a++ = *b++;
    } else {
        a += n; b += n;
        while (n--) *--a = *--b;
    }
    return d;
}
__attribute__((no_builtin)) void* memset(void* d, int c, size_t n) {
    unsigned char* a = (unsigned char*)d;
    while (n--) *a++ = (unsigned char)c;
    return d;
}
__attribute__((no_builtin)) int memcmp(const void* x, const void* y, size_t n) {
    const unsigned char* a = (const unsigned char*)x;
    const unsigned char* b = (const unsigned char*)y;
    for (; n; n--, a++, b++) if (*a != *b) return *a - *b;
    return 0;
}
__attribute__((no_builtin)) void* realloc(void* p, size_t n) {
    if (!p) return malloc(n);
    unsigned k = *(uint32_t*)((unsigned char*)p - 16);
    size_t cap = ((size_t)16 << k) - 16;
    if (n <= cap) return p;
    void* q = malloc(n);
    if (!q) return NULL;
    memcpy(q, p, cap);
    free(p);
    return q;
}
__attribute__((no_builtin)) void* calloc(size_t a, size_t b) {
    if (b && a > SIZE_MAX / b) return NULL;
    void* p = malloc(a * b);
    if (p) memset(p, 0, a * b);
    return p;
}
void* memchr(const void* s, int c, size_t n) {
    const unsigned char* a = (const unsigned char*)s;
    for (; n; n--, a++) if (*a == (unsigned char)c) return (void*)a;
    return NULL;
}

/* ---------------- strings ---------------- */
__attribute__((no_builtin)) size_t strlen(const char* s) { const char* e = s; while (*e) e++; return (size_t)(e - s); }
int strcmp(const char* a, const char* b) { while (*a && *a == *b) { a++; b++; } return (unsigned char)*a - (unsigned char)*b; }
static inline int strncmp(const char* a, const char* b, size_t n) {
    for (; n; n--, a++, b++) { if (*a != *b) return (unsigned char)*a - (unsigned char)*b; if (!*a) return 0; }
    return 0;
}
char* strchr(const char* s, int c) { for (;; s++) { if (*s == (char)c) return (char*)s; if (!*s) return NULL; } }
static inline char* strrchr(const char* s, int c) { const char* r = NULL; for (;; s++) { if (*s == (char)c) r = s; if (!*s) return (char*)r; } }
char* strcpy(char* d, const char* s) { char* r = d; while ((*d++ = *s++)) {} return r; }
char* strcat(char* d, const char* s) { strcpy(d + strlen(d), s); return d; }
static inline char* strerror(int e) { (void)e; return (char*)"not available on wasm32"; }
static inline int sijo_w_isspace(int c) { return c == ' ' || (c >= '\t' && c <= '\r'); }
static inline unsigned long long strtoull(const char* s, char** end, int base) {
    while (sijo_w_isspace(*s)) s++;
    if (*s == '+') s++;
    if ((base == 0 || base == 16) && s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) { s += 2; base = 16; }
    if (base == 0) base = 10;
    unsigned long long v = 0;
    for (;; s++) {
        int d = *s >= '0' && *s <= '9' ? *s - '0' : *s >= 'a' && *s <= 'z' ? *s - 'a' + 10 : *s >= 'A' && *s <= 'Z' ? *s - 'A' + 10 : 99;
        if (d >= base) break;
        v = v * (unsigned)base + (unsigned)d;
    }
    if (end) *end = (char*)s;
    return v;
}
static inline long long strtoll(const char* s, char** end, int base) {
    while (sijo_w_isspace(*s)) s++;
    int neg = *s == '-';
    if (*s == '-' || *s == '+') s++;
    unsigned long long v = strtoull(s, end, base);
    return neg ? -(long long)v : (long long)v;
}
static inline long strtol(const char* s, char** end, int base) { return (long)strtoll(s, end, base); }
static inline int atoi(const char* s) { return (int)strtol(s, NULL, 10); }
/* command-line numbers only (not game math): decimal digits, a fraction and an exponent */
static inline double strtod(const char* s, char** end) {
    while (sijo_w_isspace(*s)) s++;
    int neg = *s == '-';
    if (*s == '-' || *s == '+') s++;
    double v = 0, sc = 1;
    while (*s >= '0' && *s <= '9') v = v * 10 + (*s++ - '0');
    if (*s == '.') { s++; while (*s >= '0' && *s <= '9') { sc /= 10; v += (*s++ - '0') * sc; } }
    if (*s == 'e' || *s == 'E') {
        char* e2;
        long x = strtol(s + 1, &e2, 10);
        if (e2 != s + 1) { s = e2; while (x > 0) { v *= 10; x--; } while (x < 0) { v /= 10; x++; } }
    }
    if (end) *end = (char*)s;
    return neg ? -v : v;
}
static inline double atof(const char* s) { return strtod(s, NULL); }
static inline char* getenv(const char* n) { (void)n; return NULL; }
static inline int abs(int x) { return x < 0 ? -x : x; }

/* ---------------- sorting ---------------- */
/* merge sort (stable; qsort's order of equal elements is unspecified, the runtime breaks ties) */
static void qsort(void* base, size_t n, size_t sz, int (*cmp)(const void*, const void*)) {
    if (n < 2) return;
    unsigned char* a = (unsigned char*)base;
    unsigned char* t = (unsigned char*)malloc(n * sz);
    if (!t) return;
    for (size_t w = 1; w < n; w *= 2) {
        for (size_t lo = 0; lo < n; lo += 2 * w) {
            size_t mid = lo + w < n ? lo + w : n, hi = lo + 2 * w < n ? lo + 2 * w : n;
            size_t i = lo, j = mid, k = lo;
            while (i < mid && j < hi) {
                if (cmp(a + j * sz, a + i * sz) < 0) { memcpy(t + k * sz, a + j * sz, sz); j++; }
                else { memcpy(t + k * sz, a + i * sz, sz); i++; }
                k++;
            }
            if (i < mid) memcpy(t + k * sz, a + i * sz, (mid - i) * sz), k += mid - i;
            if (j < hi) memcpy(t + k * sz, a + j * sz, (hi - j) * sz);
        }
        memcpy(a, t, n * sz);
    }
    free(t);
}

/* ---------------- math ---------------- */
#define INFINITY (__builtin_inff())
#define NAN (__builtin_nanf(""))
#define HUGE_VAL (__builtin_huge_val())
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
#define signbit(x) __builtin_signbit(x)
static inline double sqrt(double x) { return __builtin_sqrt(x); }
static inline float sqrtf(float x) { return __builtin_sqrtf(x); }
static inline double floor(double x) { return __builtin_floor(x); }
static inline float floorf(float x) { return __builtin_floorf(x); }
static inline double ceil(double x) { return __builtin_ceil(x); }
static inline float ceilf(float x) { return __builtin_ceilf(x); }
static inline double trunc(double x) { return __builtin_trunc(x); }
static inline float truncf(float x) { return __builtin_truncf(x); }
static inline double fabs(double x) { return __builtin_fabs(x); }
static inline float fabsf(float x) { return __builtin_fabsf(x); }
static inline float copysignf(float x, float y) { return __builtin_copysignf(x, y); }
static inline double copysign(double x, double y) { return __builtin_copysign(x, y); }
static inline double round(double x) { double t = trunc(x); if (fabs(x - t) >= 0.5) t += x < 0 ? -1.0 : 1.0; return t; }
/* the exact remainder x - n*y with n = trunc(x / y), as C's fmodf (integer steps on the bits) */
static float fmodf(float x, float y) {
    uint32_t ux, uy;
    __builtin_memcpy(&ux, &x, 4);
    __builtin_memcpy(&uy, &y, 4);
    int ex = (int)(ux >> 23 & 0xff), ey = (int)(uy >> 23 & 0xff);
    uint32_t sx = ux & 0x80000000u, i;
    if ((uy << 1) == 0 || __builtin_isnan(y) || ex == 0xff) return (x * y) / (x * y);
    if ((ux << 1) <= (uy << 1)) {
        if ((ux << 1) == (uy << 1)) return 0 * x;
        return x;
    }
    if (!ex) { for (i = ux << 9; i >> 31 == 0; ex--, i <<= 1) {} ux <<= -ex + 1; }
    else { ux &= 0xffffffffu >> 9; ux |= 1u << 23; }
    if (!ey) { for (i = uy << 9; i >> 31 == 0; ey--, i <<= 1) {} uy <<= -ey + 1; }
    else { uy &= 0xffffffffu >> 9; uy |= 1u << 23; }
    for (; ex > ey; ex--) {
        i = ux - uy;
        if (i >> 31 == 0) { if (i == 0) return 0 * x; ux = i; }
        ux <<= 1;
    }
    i = ux - uy;
    if (i >> 31 == 0) { if (i == 0) return 0 * x; ux = i; }
    for (; ux >> 23 == 0; ux <<= 1, ex--) {}
    if (ex > 0) { ux -= 1u << 23; ux |= (uint32_t)ex << 23; }
    else { ux >>= -ex + 1; }
    ux |= sx;
    float r;
    __builtin_memcpy(&r, &ux, 4);
    return r;
}
/* exp in double: only for non-game statistics; game math uses the runtime's own */
static inline double exp(double x) {
    double r = 1, t = 1;
    for (int k = 1; k < 40; k++) { t *= x / k; r += t; }
    return r;
}

/* ---------------- time ---------------- */
typedef long long time_t;
struct timespec { time_t tv_sec; long tv_nsec; };
#define CLOCK_MONOTONIC 1
#define CLOCK_REALTIME 0
static inline int clock_gettime(int id, struct timespec* ts) {
    (void)id;
    double t = sijo_wasm_now();
    ts->tv_sec = (time_t)t;
    ts->tv_nsec = (long)((t - (double)ts->tv_sec) * 1e9);
    return 0;
}
static inline time_t time(time_t* t) { time_t v = (time_t)sijo_wasm_now(); if (t) *t = v; return v; }
static inline int nanosleep(const struct timespec* a, struct timespec* b) { (void)a; (void)b; return 0; }
typedef long clock_t;
static inline clock_t clock(void) { return (clock_t)(sijo_wasm_now() * 1e6); }

/* ---------------- setjmp / longjmp: unwinding through the JS loader ---------------- */
typedef struct { int unused; } jmp_buf[1];
/* every catch point returns 0 here: game code only comes back to one through a panic, and that
 * unwinds all the way to the API call (the loader restores the stack and the API's state) */
#define setjmp(jb) ((void)(jb), 0)
__attribute__((noreturn)) static inline void longjmp(jmp_buf jb, int v) {
    (void)jb; (void)v;
    sijo_wasm_unwind();
    __builtin_trap();
}

/* ---------------- processes, signals, errno ---------------- */
static int errno;
#define EINTR 4
#define EAGAIN 11
#define EWOULDBLOCK 11
__attribute__((noreturn)) static inline void exit(int code) { sijo_wasm_exit(code); __builtin_trap(); }
__attribute__((noreturn)) static inline void _exit(int code) { exit(code); }
__attribute__((noreturn)) static inline void abort(void) { __builtin_trap(); }
typedef int sig_atomic_t;
typedef void (*sighandler_t)(int);
#define SIGINT 2
#define SIGTERM 15
#define SIGSEGV 11
#define SIGBUS 7
#define SIGPIPE 13
#define SIG_DFL ((sighandler_t)0)
#define SIG_IGN ((sighandler_t)1)
#define SA_ONSTACK 1
typedef struct { void* ss_sp; int ss_flags; size_t ss_size; } stack_t;
struct sigaction { sighandler_t sa_handler; int sa_flags; int sa_mask; };
static inline sighandler_t signal(int s, sighandler_t h) { (void)s; (void)h; return SIG_DFL; }
static inline int sigaction(int s, const struct sigaction* a, struct sigaction* o) { (void)s; (void)a; (void)o; return -1; }
static inline int sigaltstack(const stack_t* a, stack_t* o) { (void)a; (void)o; return -1; }
static inline int sigemptyset(int* m) { *m = 0; return 0; }
static inline int getpid(void) { return 1; }
static inline int write(int fd, const void* p, size_t n) { sijo_wasm_write(fd, (const char*)p, (int)n); return (int)n; }
static inline int read(int fd, void* p, size_t n) { (void)fd; (void)p; (void)n; return -1; }
static inline int close(int fd) { (void)fd; return 0; }
static inline int isatty(int fd) { (void)fd; return 0; }

/* ---------------- stdio ---------------- */
/* printf-style functions have external linkage: the C compiler rewrites printf("x\n") into
 * puts("x"), fprintf(f, "%s", s) into fputs(s, f) and so on, and calls them by name */
typedef struct SijoWFile {
    int fd;           /* 1 stdout, 2 stderr, 0 a file in memory */
    int writing, err;
    char* buf;
    size_t n, cap, pos;
    char path[256];
} FILE;
static FILE sijo_w_out = {1, 1, 0, NULL, 0, 0, 0, ""}, sijo_w_errf = {2, 1, 0, NULL, 0, 0, 0, ""};
#define stdout (&sijo_w_out)
#define stderr (&sijo_w_errf)
#define EOF (-1)
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#define _IOFBF 0
#define _IOLBF 1
#define _IONBF 2
static inline int setvbuf(FILE* f, char* b, int m, size_t s) { (void)f; (void)b; (void)m; (void)s; return 0; }
static void sijo_w_put(FILE* f, const char* p, size_t n) {
    if (!n) return;
    if (f->n + n > f->cap) {
        size_t c = f->cap ? f->cap : 256;
        while (c < f->n + n) c *= 2;
        char* nb = (char*)realloc(f->buf, c);
        if (!nb) { f->err = 1; return; }
        f->buf = nb;
        f->cap = c;
    }
    memcpy(f->buf + f->n, p, n);
    f->n += n;
    if (f->fd) {
        /* the console: whole lines */
        size_t last = f->n;
        while (last > 0 && f->buf[last - 1] != '\n') last--;
        if (last > 0) {
            sijo_wasm_write(f->fd, f->buf, (int)last);
            memmove(f->buf, f->buf + last, f->n - last);
            f->n -= last;
        }
    }
}
static inline int fflush(FILE* f) {
    if (!f) { fflush(stdout); fflush(stderr); return 0; }
    if (f->fd && f->n) { sijo_wasm_write(f->fd, f->buf, (int)f->n); f->n = 0; }
    return 0;
}
static FILE* fopen(const char* path, const char* mode) {
    FILE* f = (FILE*)calloc(1, sizeof(FILE));
    if (!f) return NULL;
    size_t pl = strlen(path);
    if (pl >= sizeof f->path) { free(f); return NULL; }
    memcpy(f->path, path, pl + 1);
    f->writing = mode[0] == 'w' || mode[0] == 'a';
    if (!f->writing) {
        int n = sijo_wasm_file_size(path, (int)pl);
        if (n < 0) { free(f); return NULL; }
        f->buf = (char*)malloc((size_t)n + 1);
        if (!f->buf) { free(f); return NULL; }
        sijo_wasm_file_read(path, (int)pl, f->buf);
        f->n = f->cap = (size_t)n;
    }
    return f;
}
static inline int fclose(FILE* f) {
    if (!f) return EOF;
    if (f->fd) return fflush(f);
    if (f->writing) sijo_wasm_file_write(f->path, (int)strlen(f->path), f->buf ? f->buf : "", (int)f->n);
    int e = f->err;
    free(f->buf);
    free(f);
    return e ? EOF : 0;
}
size_t fwrite(const void* p, size_t sz, size_t n, FILE* f) { sijo_w_put(f, (const char*)p, sz * n); return f->err ? 0 : n; }
static inline size_t fread(void* p, size_t sz, size_t n, FILE* f) {
    if (!sz) return 0;
    size_t want = sz * n, have = f->n > f->pos ? f->n - f->pos : 0;
    size_t got = (want < have ? want : have) / sz * sz;
    memcpy(p, f->buf + f->pos, got);
    f->pos += got;
    return got / sz;
}
static inline int fgetc(FILE* f) { return f->pos < f->n ? (unsigned char)f->buf[f->pos++] : EOF; }
static inline int getc(FILE* f) { return fgetc(f); }
static inline char* fgets(char* s, int n, FILE* f) {
    int i = 0;
    while (i < n - 1) { int c = fgetc(f); if (c == EOF) break; s[i++] = (char)c; if (c == '\n') break; }
    if (!i) return NULL;
    s[i] = 0;
    return s;
}
static inline int fseek(FILE* f, long off, int wh) {
    long b = wh == SEEK_SET ? 0 : wh == SEEK_CUR ? (long)f->pos : (long)f->n;
    if (b + off < 0) return -1;
    f->pos = (size_t)(b + off);
    return 0;
}
static inline long ftell(FILE* f) { return f->writing ? (long)f->n : (long)f->pos; }
static inline int feof(FILE* f) { return f->pos >= f->n; }
static inline int ferror(FILE* f) { return f->err; }
int fputs(const char* s, FILE* f) { sijo_w_put(f, s, strlen(s)); return 0; }
int fputc(int c, FILE* f) { char ch = (char)c; sijo_w_put(f, &ch, 1); return c; }
int putc(int c, FILE* f) { return fputc(c, f); }
int putchar(int c) { return fputc(c, stdout); }
int puts(const char* s) { fputs(s, stdout); fputc('\n', stdout); return 0; }
static inline int remove(const char* p) { return sijo_wasm_file_remove(p, (int)strlen(p)); }
static inline int rename(const char* a, const char* b) {
    FILE* f = fopen(a, "rb");
    if (!f) return -1;
    sijo_wasm_file_write(b, (int)strlen(b), f->buf ? f->buf : "", (int)f->n);
    fclose(f);
    return remove(a);
}
static inline void perror(const char* s) { fputs(s, stderr); fputs(": not available on wasm32\n", stderr); }
static inline int fileno(FILE* f) { return f->fd; }

/* formatting: what the runtime's messages use (d i u x X o c s p %, flags, width, precision,
 * hh h l ll z j t); %f %e %g print statistics (timings), not game values, and are approximate */
typedef struct { char* p; size_t n, cap; } SijoWOut;
static void sijo_w_emit(SijoWOut* o, char c) { if (o->n + 1 < o->cap) o->p[o->n] = c; o->n++; }
static void sijo_w_pad(SijoWOut* o, char c, int k) { while (k-- > 0) sijo_w_emit(o, c); }
static void sijo_w_field(SijoWOut* o, const char* s, int len, int width, int left, char padc, int signlen) {
    int pad = width > len ? width - len : 0;
    if (!left && padc == ' ') sijo_w_pad(o, ' ', pad);
    for (int i = 0; i < signlen; i++) sijo_w_emit(o, s[i]);
    if (!left && padc == '0') sijo_w_pad(o, '0', pad);
    for (int i = signlen; i < len; i++) sijo_w_emit(o, s[i]);
    if (left) sijo_w_pad(o, ' ', pad);
}
int vsnprintf(char* buf, size_t cap, const char* fmt, va_list ap) {
    SijoWOut o = {buf, 0, cap};
    for (const char* f = fmt; *f; f++) {
        if (*f != '%') { sijo_w_emit(&o, *f); continue; }
        f++;
        int left = 0, plus = 0, space = 0, alt = 0, zero = 0, width = 0, prec = -1, lng = 0;
        for (;; f++) {
            if (*f == '-') left = 1; else if (*f == '+') plus = 1; else if (*f == ' ') space = 1;
            else if (*f == '#') alt = 1; else if (*f == '0') zero = 1; else break;
        }
        if (*f == '*') { width = va_arg(ap, int); if (width < 0) { left = 1; width = -width; } f++; }
        else while (*f >= '0' && *f <= '9') width = width * 10 + (*f++ - '0');
        if (*f == '.') {
            f++;
            prec = 0;
            if (*f == '*') { prec = va_arg(ap, int); f++; }
            else while (*f >= '0' && *f <= '9') prec = prec * 10 + (*f++ - '0');
        }
        for (;; f++) {
            if (*f == 'l') lng++; else if (*f == 'h') {} else if (*f == 'z' || *f == 't') lng = sizeof(size_t) == 8 ? 2 : 1;
            else if (*f == 'j') lng = 2; else break;
        }
        char tmp[128];
        int n = 0;
        char c = *f;
        if (c == '%') { sijo_w_emit(&o, '%'); continue; }
        if (c == 'c') { tmp[0] = (char)va_arg(ap, int); sijo_w_field(&o, tmp, 1, width, left, ' ', 0); continue; }
        if (c == 's') {
            const char* s = va_arg(ap, const char*);
            if (!s) s = "(null)";
            int len = 0;
            while (s[len] && (prec < 0 || len < prec)) len++;
            sijo_w_field(&o, s, len, width, left, ' ', 0);
            continue;
        }
        if (c == 'd' || c == 'i' || c == 'u' || c == 'x' || c == 'X' || c == 'o' || c == 'p') {
            unsigned long long v;
            int neg = 0;
            if (c == 'p') { v = (uintptr_t)va_arg(ap, void*); alt = 1; c = 'x'; }
            else if (c == 'd' || c == 'i') {
                long long s = lng >= 2 ? va_arg(ap, long long) : lng == 1 ? va_arg(ap, long) : va_arg(ap, int);
                neg = s < 0;
                v = neg ? 0ull - (unsigned long long)s : (unsigned long long)s;
            } else {
                v = lng >= 2 ? va_arg(ap, unsigned long long) : lng == 1 ? va_arg(ap, unsigned long) : va_arg(ap, unsigned int);
            }
            unsigned base = c == 'o' ? 8 : (c == 'x' || c == 'X') ? 16 : 10;
            const char* dig = c == 'X' ? "0123456789ABCDEF" : "0123456789abcdef";
            char d[32];
            int k = 0;
            do { d[k++] = dig[v % base]; v /= base; } while (v);
            if (prec >= 0) zero = 0;
            while (k < prec) d[k++] = '0';
            if (neg) tmp[n++] = '-'; else if (plus) tmp[n++] = '+'; else if (space) tmp[n++] = ' ';
            if (alt && base == 16) { tmp[n++] = '0'; tmp[n++] = c == 'X' ? 'X' : 'x'; }
            int signlen = n;
            while (k) tmp[n++] = d[--k];
            sijo_w_field(&o, tmp, n, width, left, zero && !left ? '0' : ' ', signlen);
            continue;
        }
        if (c == 'f' || c == 'F' || c == 'e' || c == 'g' || c == 'G' || c == 'E') {
            double v = va_arg(ap, double);
            if (prec < 0) prec = 6;
            if (prec > 17) prec = 17;
            if (v != v) { sijo_w_field(&o, "nan", 3, width, left, ' ', 0); continue; }
            if (v < 0) { tmp[n++] = '-'; v = -v; } else if (plus) tmp[n++] = '+';
            if (v > 1e18) { memcpy(tmp + n, "inf", 3); n += 3; sijo_w_field(&o, tmp, n, width, left, ' ', 0); continue; }
            double sc = 1;
            for (int i = 0; i < prec; i++) sc *= 10;
            unsigned long long all = (unsigned long long)(v * sc + 0.5);
            unsigned long long ip = all / (unsigned long long)sc, fp = all % (unsigned long long)sc;
            char d[32];
            int k = 0;
            do { d[k++] = (char)('0' + ip % 10); ip /= 10; } while (ip);
            int signlen = n;
            while (k) tmp[n++] = d[--k];
            if (prec > 0) {
                tmp[n++] = '.';
                for (int i = prec - 1; i >= 0; i--) { tmp[n + i] = (char)('0' + fp % 10); fp /= 10; }
                n += prec;
            }
            sijo_w_field(&o, tmp, n, width, left, zero && !left ? '0' : ' ', signlen);
            continue;
        }
        if (!c) break;
    }
    if (cap) buf[o.n < cap ? o.n : cap - 1] = 0;
    return (int)o.n;
}
int snprintf(char* b, size_t n, const char* fmt, ...) { va_list ap; va_start(ap, fmt); int r = vsnprintf(b, n, fmt, ap); va_end(ap); return r; }
int sprintf(char* b, const char* fmt, ...) { va_list ap; va_start(ap, fmt); int r = vsnprintf(b, (size_t)1 << 20, fmt, ap); va_end(ap); return r; }
int vfprintf(FILE* f, const char* fmt, va_list ap) {
    char small[512];
    va_list ap2;
    va_copy(ap2, ap);
    int n = vsnprintf(small, sizeof small, fmt, ap);
    if (n < (int)sizeof small) {
        sijo_w_put(f, small, (size_t)n);
    } else {
        char* big = (char*)malloc((size_t)n + 1);
        if (big) { vsnprintf(big, (size_t)n + 1, fmt, ap2); sijo_w_put(f, big, (size_t)n); free(big); }
    }
    va_end(ap2);
    return n;
}
int fprintf(FILE* f, const char* fmt, ...) { va_list ap; va_start(ap, fmt); int r = vfprintf(f, fmt, ap); va_end(ap); return r; }
int printf(const char* fmt, ...) { va_list ap; va_start(ap, fmt); int r = vfprintf(stdout, fmt, ap); va_end(ap); return r; }
int vprintf(const char* fmt, va_list ap) { return vfprintf(stdout, fmt, ap); }
/* the two patterns the runtime reads: "%2x" (hex byte) and "%u:%u" (a handle) */
static int sscanf(const char* s, const char* fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int got = 0;
    for (const char* f = fmt; *f; f++) {
        if (*f != '%') { if (*s != *f) break; s++; continue; }
        f++;
        int width = 0;
        while (*f >= '0' && *f <= '9') width = width * 10 + (*f++ - '0');
        unsigned base = *f == 'x' ? 16 : 10;
        unsigned v = 0;
        int k = 0;
        for (; *s && (!width || k < width); s++, k++) {
            int d = *s >= '0' && *s <= '9' ? *s - '0' : *s >= 'a' && *s <= 'f' ? *s - 'a' + 10 : *s >= 'A' && *s <= 'F' ? *s - 'A' + 10 : 99;
            if (d >= (int)base) break;
            v = v * base + (unsigned)d;
        }
        if (!k) break;
        *va_arg(ap, unsigned*) = v;
        got++;
    }
    va_end(ap);
    return got;
}

/* ---------------- sockets: none (the in-process client is the only one) ---------------- */
typedef uint32_t socklen_t;
typedef uint16_t sa_family_t;
typedef uint16_t in_port_t;
struct in_addr { uint32_t s_addr; };
struct in6_addr { unsigned char s6_addr[16]; };
struct sockaddr { sa_family_t sa_family; char sa_data[14]; };
struct sockaddr_in { sa_family_t sin_family; in_port_t sin_port; struct in_addr sin_addr; unsigned char sin_zero[8]; };
struct sockaddr_in6 { sa_family_t sin6_family; in_port_t sin6_port; uint32_t sin6_flowinfo; struct in6_addr sin6_addr; uint32_t sin6_scope_id; };
struct sockaddr_storage { sa_family_t ss_family; char pad[126]; };
struct addrinfo { int ai_flags, ai_family, ai_socktype, ai_protocol; socklen_t ai_addrlen; struct sockaddr* ai_addr; char* ai_canonname; struct addrinfo* ai_next; };
struct pollfd { int fd; short events, revents; };
static const struct in6_addr in6addr_any = {{0}};
#define AF_UNSPEC 0
#define AF_INET 2
#define AF_INET6 10
#define SOCK_DGRAM 2
#define SOL_SOCKET 1
#define SO_SNDBUF 7
#define SO_RCVBUF 8
#define SO_REUSEADDR 2
#define IPPROTO_UDP 17
#define IPPROTO_IPV6 41
#define IPV6_V6ONLY 26
#define INADDR_ANY 0u
#define INADDR_LOOPBACK 0x7f000001u
#define AI_PASSIVE 1
#define AI_NUMERICHOST 4
#define POLLIN 1
#define F_GETFL 3
#define F_SETFL 4
#define O_NONBLOCK 04000
#define MSG_DONTWAIT 0x40
#define INET6_ADDRSTRLEN 46
#define INET_ADDRSTRLEN 16
#define NI_MAXHOST 1025
#define NI_MAXSERV 32
#define NI_NUMERICHOST 1
#define NI_NUMERICSERV 2
static inline uint16_t htons(uint16_t v) { return (uint16_t)(v << 8 | v >> 8); }
static inline uint16_t ntohs(uint16_t v) { return htons(v); }
static inline uint32_t htonl(uint32_t v) { return __builtin_bswap32(v); }
static inline uint32_t ntohl(uint32_t v) { return __builtin_bswap32(v); }
static inline int socket(int a, int b, int c) { (void)a; (void)b; (void)c; return -1; }
static inline int bind(int s, const struct sockaddr* a, socklen_t l) { (void)s; (void)a; (void)l; return -1; }
static inline int connect(int s, const struct sockaddr* a, socklen_t l) { (void)s; (void)a; (void)l; return -1; }
static inline int setsockopt(int s, int l, int n, const void* v, socklen_t vl) { (void)s; (void)l; (void)n; (void)v; (void)vl; return -1; }
static inline int getsockname(int s, struct sockaddr* a, socklen_t* l) { (void)s; (void)a; (void)l; return -1; }
static inline long sendto(int s, const void* b, size_t n, int fl, const struct sockaddr* a, socklen_t l) { (void)s; (void)b; (void)n; (void)fl; (void)a; (void)l; return -1; }
static inline long recvfrom(int s, void* b, size_t n, int fl, struct sockaddr* a, socklen_t* l) { (void)s; (void)b; (void)n; (void)fl; (void)a; (void)l; return -1; }
static inline int poll(struct pollfd* p, unsigned long n, int ms) { (void)p; (void)n; (void)ms; return 0; }
static inline int fcntl(int fd, int cmd, ...) { (void)fd; (void)cmd; return -1; }
static inline int getaddrinfo(const char* h, const char* s, const struct addrinfo* hi, struct addrinfo** r) { (void)h; (void)s; (void)hi; *r = NULL; return -1; }
static inline void freeaddrinfo(struct addrinfo* a) { (void)a; }
static inline const char* gai_strerror(int e) { (void)e; return "no network on wasm32"; }
static inline int getnameinfo(const struct sockaddr* a, socklen_t al, char* h, socklen_t hl, char* s, socklen_t sl, int f) { (void)a; (void)al; (void)f; if (hl) h[0] = 0; if (sl) s[0] = 0; return -1; }
static inline const char* inet_ntop(int af, const void* src, char* dst, socklen_t n) { (void)af; (void)src; if (n) dst[0] = 0; return dst; }
static inline int inet_pton(int af, const char* src, void* dst) { (void)af; (void)src; (void)dst; return 0; }
#endif
