/* boids_dod.c — hand-written data-oriented C for the "boids" workload.
 *
 * Same algorithm and float operation order as boids.sijo. Expert layout:
 * one array per scalar (SoA), so the hot distance test streams only px/py
 * (8 bytes per boid); Jacobi semantics via ping-pong buffers (read the tick's
 * start state from buffer `r`, write the new state to buffer `w`, swap) —
 * no copy at all; OpenMP over boids. The inner loop is a conditional
 * accumulation whose order must be preserved for bit-exact results, so it is
 * left scalar (vectorizing across j would reorder the float sums).
 */
#include <math.h>
#include "../common/bench.h"
#include "../common/sijo_compat.h"

#define WORLD 200.0f
#define VIEW_SQ 144.0f
#define MAX_SPEED 30.0f

typedef struct {
    float* px;
    float* py;
    float* vx;
    float* vy;
} BoidState;

typedef struct {
    int32_t n;
    BoidState buf[2];
    int cur;            /* buf[cur] holds the current state */
    int32_t* neighbours;
} Flock;

static void alloc_state(BoidState* s, int32_t n) {
    s->px = (float*)malloc(sizeof(float) * (size_t)n);
    s->py = (float*)malloc(sizeof(float) * (size_t)n);
    s->vx = (float*)malloc(sizeof(float) * (size_t)n);
    s->vy = (float*)malloc(sizeof(float) * (size_t)n);
}

static void flock_init(Flock* f, int32_t n) {
    f->n = n;
    f->cur = 0;
    alloc_state(&f->buf[0], n);
    alloc_state(&f->buf[1], n);
    f->neighbours = (int32_t*)calloc((size_t)n, sizeof(int32_t));
    BoidState* s = &f->buf[0];
    for (int32_t i = 0; i < n; i++) {
        int32_t s1 = nb_next((int32_t)((uint32_t)i * 7919u + 17u));
        int32_t s2 = nb_next(s1);
        int32_t s3 = nb_next(s2);
        int32_t s4 = nb_next(s3);
        s->px[i] = nb_clamp_range(nb_unit(s1) * WORLD, 0.0f, WORLD);
        s->py[i] = nb_clamp_range(nb_unit(s2) * WORLD, 0.0f, WORLD);
        s->vx[i] = nb_unit(s3) * 20.0f - 10.0f;
        s->vy[i] = nb_unit(s4) * 20.0f - 10.0f;
    }
}

static void flock_update(Flock* f, float dt) {
    const int32_t n = f->n;
    const float* restrict rpx = f->buf[f->cur].px;
    const float* restrict rpy = f->buf[f->cur].py;
    const float* restrict rvx = f->buf[f->cur].vx;
    const float* restrict rvy = f->buf[f->cur].vy;
    float* restrict wpx = f->buf[f->cur ^ 1].px;
    float* restrict wpy = f->buf[f->cur ^ 1].py;
    float* restrict wvx = f->buf[f->cur ^ 1].vx;
    float* restrict wvy = f->buf[f->cur ^ 1].vy;
    int32_t* restrict nb = f->neighbours;
#pragma omp parallel for schedule(static) if (n >= 256)
    for (int32_t i = 0; i < n; i++) {
        const float x = rpx[i], y = rpy[i];
        float cx = 0.0f, cy = 0.0f, ax = 0.0f, ay = 0.0f, wx = 0.0f, wy = 0.0f;
        int32_t count = 0;
        for (int32_t j = 0; j < n; j++) {
            if (j == i) continue;
            const float ox = rpx[j] - x, oy = rpy[j] - y;
            const float d = ox * ox + oy * oy;
            if (d < VIEW_SQ) {
                cx = cx + rpx[j];
                cy = cy + rpy[j];
                ax = ax + rvx[j];
                ay = ay + rvy[j];
                if (d < 9.0f) {
                    wx = wx - ox;
                    wy = wy - oy;
                }
                count++;
            }
        }
        nb[i] = count;
        float vx = rvx[i], vy = rvy[i];
        if (count > 0) {
            const float inv = 1.0f / (float)count;
            const float tx = cx * inv - x, ty = cy * inv - y;
            const float gx = ax * inv - vx, gy = ay * inv - vy;
            vx = vx + tx * 0.6f * dt + gx * 1.5f * dt + wx * 4.0f * dt;
            vy = vy + ty * 0.6f * dt + gy * 1.5f * dt + wy * 4.0f * dt;
        }
        const float l2 = vx * vx + vy * vy;
        if (l2 > MAX_SPEED * MAX_SPEED) {
            const float s = MAX_SPEED / sqrtf(l2);
            vx = vx * s;
            vy = vy * s;
        }
        const float npx = nb_clamp_range(x + vx * dt, 0.0f, WORLD);
        const float npy = nb_clamp_range(y + vy * dt, 0.0f, WORLD);
        if (npx <= 0.0f || npx >= WORLD) vx = -vx;
        if (npy <= 0.0f || npy >= WORLD) vy = -vy;
        wpx[i] = npx;
        wpy[i] = npy;
        wvx[i] = vx;
        wvy[i] = vy;
    }
    f->cur ^= 1;
}

static int32_t q(float v) { return nb_f2i(v * 8388608.0f); }

int main(int argc, char** argv) {
    BenchArgs a = bench_args(argc, argv, 1000);
    const float dt = 1.0f / 60.0f;
    Flock f;
    double t0 = bench_now();
    flock_init(&f, a.n);
    for (int32_t t = 1; t <= a.ticks; t++) {
        flock_update(&f, dt);
        if (t % 100 == 0) {
            const BoidState* s = &f.buf[f.cur];
            int32_t crowded = 0, h = 0;
            for (int32_t i = 0; i < f.n; i++) {
                if (f.neighbours[i] > 6) crowded++;
                int32_t e = q(s->px[i]);
                e = nb_mix31(e, q(s->py[i]));
                e = nb_mix31(e, q(s->vx[i]));
                e = nb_mix31(e, q(s->vy[i]));
                e = nb_mix31(e, f.neighbours[i]);
                h = nb_hash_add(h, e);
            }
            if (!a.quiet) printf("bench tick=%d n=%d crowded=%d h=%d\n", t, f.n, crowded, h);
        }
    }
    double t1 = bench_now();
    const BoidState* s = &f.buf[f.cur];
    uint64_t h = nc_begin();
    for (int32_t i = 0; i < f.n; i++) {
        h = nc_handle(h, i);
        h = nc_f(h, s->px[i]);
        h = nc_f(h, s->py[i]);
        h = nc_f(h, s->vx[i]);
        h = nc_f(h, s->vy[i]);
        h = nc_i(h, f.neighbours[i]);
    }
    printf("threads: %d\n", bench_threads());
    printf("world checksum: %016llx\n", (unsigned long long)h);
    bench_report_time(&a, t0, t1);
    return 0;
}
