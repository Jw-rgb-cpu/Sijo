/* bench.h — shared helpers for the hand-written C and the OOP C++ baselines.
 *
 * Every baseline mirrors the measurement of the Sijo runtime exactly:
 *   t0 is taken before the world is created/spawned, t1 after the last tick,
 *   and the program prints   "time: <ms> ms total, <us> us/tick"   with
 *   us/tick = total / (ticks + 1)   (the Sijo runtime counts Game.Start as a tick).
 * The harness (bench/run.py) runs every binary with --ticks N and --ticks 1 and
 * reports the steady-state cost (T(N) - T(1)) / (N - 1), so start-up cost cancels.
 *
 * Integer arithmetic that Sijo defines as wrapping (int is i32, two's complement)
 * is done on uint32_t here so the baselines never rely on -fwrapv.
 */
#ifndef SIJO_BENCH_H
#define SIJO_BENCH_H

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#ifdef _OPENMP
#include <omp.h>
#endif

#ifdef __cplusplus
extern "C" {
#endif

typedef struct {
    int n;        /* entity count (workload size) */
    int ticks;
    int quiet;    /* suppress the per-100-tick "bench" lines */
    int timing;   /* print the "time:" line */
    float arena;  /* shooter only: arena size */
    int scatter;  /* OOP only: allocate the initial objects in a shuffled address order */
    int par_ships; /* shooter hand C only: run ship targeting in a parallel pass (not used by run.py) */
} BenchArgs;

static inline double bench_now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

static inline BenchArgs bench_args(int argc, char** argv, int default_n) {
    BenchArgs a;
    a.n = default_n;
    a.ticks = 600;
    a.quiet = 0;
    a.timing = 0;
    a.arena = 0.0f;
    a.scatter = 0;
    a.par_ships = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--n") && i + 1 < argc) a.n = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--ticks") && i + 1 < argc) a.ticks = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--arena") && i + 1 < argc) a.arena = (float)atof(argv[++i]);
        else if (!strcmp(argv[i], "--quiet")) a.quiet = 1;
        else if (!strcmp(argv[i], "--scatter")) a.scatter = 1;
        else if (!strcmp(argv[i], "--par-ships")) a.par_ships = 1;
        else if (!strcmp(argv[i], "--timing")) a.timing = 1;
        else {
            fprintf(stderr, "unknown option %s\n", argv[i]);
            exit(2);
        }
    }
    if (a.ticks < 1) a.ticks = 1;
    return a;
}

static inline int bench_threads(void) {
#ifdef _OPENMP
    return omp_get_max_threads();
#else
    return 1;
#endif
}

static inline void bench_report_time(const BenchArgs* a, double t0, double t1) {
    if (a->timing) printf("time: %.3f ms total, %.2f us/tick\n", (t1 - t0) * 1e3, (t1 - t0) * 1e6 / (a->ticks + 1));
}

/* deterministic Fisher-Yates permutation of 0..n-1 (for --scatter) */
static inline int32_t* bench_permutation(int32_t n) {
    int32_t* p = (int32_t*)malloc(sizeof(int32_t) * (size_t)(n > 0 ? n : 1));
    uint64_t x = 0x9E3779B97F4A7C15ull;
    for (int32_t i = 0; i < n; i++) p[i] = i;
    for (int32_t i = n - 1; i > 0; i--) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        int32_t j = (int32_t)(x % (uint64_t)(i + 1));
        int32_t t = p[i];
        p[i] = p[j];
        p[j] = t;
    }
    return p;
}

/* ---- the exact integer helpers used by the .sijo sources ---- */

/* Sim.Next / Flock.Next / Rules.Next: s * 1664525 + 1013904223 (wrapping) */
static inline int32_t nb_next(int32_t s) { return (int32_t)((uint32_t)s * 1664525u + 1013904223u); }

/* Unit(s) = (float)((s >> 8) & 65535) / 65536.0   (>> is arithmetic on i32 in Sijo) */
static inline float nb_unit(int32_t s) { return (float)((s >> 8) & 65535) / 65536.0f; }

/* Sijo's float -> int cast: NaN -> 0, saturating (same as sijo_f2i in the runtime) */
static inline int32_t nb_f2i(float f) {
    if (f != f) return 0;
    if (f >= 2147483647.0f) return INT32_MAX;
    if (f <= -2147483648.0f) return INT32_MIN;
    return (int32_t)f;
}

/* [Range(lo, hi)] contract on a float field: NaN is clamped to the lower bound */
static inline float nb_clamp_range(float x, float lo, float hi) { return x != x ? lo : (x < lo ? lo : (x > hi ? hi : x)); }

/* order-independent checksum: h += (e ^ 1540483477) * 668265261, all wrapping */
static inline int32_t nb_mix31(int32_t e, int32_t v) { return (int32_t)((uint32_t)e * 31u + (uint32_t)v); }
static inline int32_t nb_hash_add(int32_t h, int32_t e) {
    return (int32_t)((uint32_t)h + ((uint32_t)e ^ 1540483477u) * 668265261u);
}

#ifdef __cplusplus
}
#endif

#endif
