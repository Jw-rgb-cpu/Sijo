/* particles_dod.c — hand-written data-oriented C for the "particles" workload.
 *
 * Same algorithm and float operation order as particles.sijo, written the way a
 * performance engineer would: one array per scalar field (pure SoA), restrict
 * pointers, the per-tick constant hoisted, the two rare branches (bounce,
 * respawn) written as selects so the loop is a straight-line kernel, and
 * `#pragma omp parallel for simd` so it is both multi-threaded and vectorized.
 * Selects do not change any float operation, so results are bit-identical.
 */
#include <math.h>
#include "../common/bench.h"
#include "../common/sijo_compat.h"

#define GRAVITY 9.81f
#define BOUNCE 0.8f
#define FRICTION 0.95f

typedef struct {
    int32_t n;
    float* px;
    float* py;
    float* vx;
    float* vy;
    float* life;
    int32_t* seed;
} Particles;

static void particles_init(Particles* p, int32_t n) {
    p->n = n;
    p->px = (float*)malloc(sizeof(float) * (size_t)n);
    p->py = (float*)malloc(sizeof(float) * (size_t)n);
    p->vx = (float*)malloc(sizeof(float) * (size_t)n);
    p->vy = (float*)malloc(sizeof(float) * (size_t)n);
    p->life = (float*)malloc(sizeof(float) * (size_t)n);
    p->seed = (int32_t*)malloc(sizeof(int32_t) * (size_t)n);
    for (int32_t i = 0; i < n; i++) {
        int32_t s1 = nb_next((int32_t)((uint32_t)i * 7919u + 17u));
        int32_t s2 = nb_next(s1);
        int32_t s3 = nb_next(s2);
        int32_t s4 = nb_next(s3);
        p->px[i] = nb_unit(s1) * 40.0f - 20.0f;
        p->py[i] = nb_unit(s2) * 10.0f;
        p->vx[i] = nb_unit(s3) * 20.0f - 10.0f;
        p->vy[i] = nb_unit(s4) * 15.0f;
        p->life[i] = nb_unit(s4) * 4.0f;
        p->seed[i] = s4;
    }
}

static void particles_update(Particles* p, float dt) {
    const int32_t n = p->n;
    float* restrict px = p->px;
    float* restrict py = p->py;
    float* restrict vx = p->vx;
    float* restrict vy = p->vy;
    float* restrict life = p->life;
    int32_t* restrict seed = p->seed;
    const float g = GRAVITY * dt;
#pragma omp parallel for simd schedule(static) if (n >= 256)
    for (int32_t i = 0; i < n; i++) {
        float x = px[i], y = py[i], u = vx[i], v = vy[i] - g;
        x = x + u * dt;
        y = y + v * dt;
        const int bounce = y < 0.0f;
        y = bounce ? -y : y;
        const float ub = u * FRICTION, vb = -v * BOUNCE;
        u = bounce ? ub : u;
        v = bounce ? vb : v;
        const float l = life[i] - dt;
        const int32_t s0 = seed[i];
        const int32_t s1 = nb_next(s0), s2 = nb_next(s1), s3 = nb_next(s2);
        /* respawn values are computed unconditionally: float ops inside a ?: arm cannot be
           speculated under -ftrapping-math (the default), which would block vectorization */
        const float ru = nb_unit(s1) * 20.0f - 10.0f;
        const float rv = nb_unit(s2) * 15.0f + 5.0f;
        const float rl = 1.0f + nb_unit(s3) * 3.0f;
        const int dead = l <= 0.0f;
        px[i] = dead ? 0.0f : x;
        py[i] = dead ? 1.0f : y;
        vx[i] = dead ? ru : u;
        vy[i] = dead ? rv : v;
        life[i] = dead ? rl : l;
        seed[i] = dead ? s3 : s0;
    }
}

static int32_t q(float v) { return nb_f2i(v * 8388608.0f); }

static int32_t particles_hash(const Particles* p) {
    int32_t h = 0;
    for (int32_t i = 0; i < p->n; i++) {
        int32_t e = q(p->px[i]);
        e = nb_mix31(e, q(p->py[i]));
        e = nb_mix31(e, q(p->vx[i]));
        e = nb_mix31(e, q(p->vy[i]));
        e = nb_mix31(e, q(p->life[i]));
        e = nb_mix31(e, p->seed[i]);
        h = nb_hash_add(h, e);
    }
    return h;
}

static uint64_t particles_world_checksum(const Particles* p) {
    uint64_t h = nc_begin();
    for (int32_t i = 0; i < p->n; i++) {
        h = nc_handle(h, i);
        h = nc_f(h, p->px[i]);
        h = nc_f(h, p->py[i]);
        h = nc_f(h, p->vx[i]);
        h = nc_f(h, p->vy[i]);
        h = nc_f(h, p->life[i]);
        h = nc_i(h, p->seed[i]);
    }
    return h;
}

int main(int argc, char** argv) {
    BenchArgs a = bench_args(argc, argv, 100000);
    const float dt = 1.0f / 60.0f;
    Particles p;
    double t0 = bench_now();
    particles_init(&p, a.n);
    for (int32_t t = 1; t <= a.ticks; t++) {
        particles_update(&p, dt);
        if (t % 100 == 0) {
            int32_t h = particles_hash(&p);
            if (!a.quiet) printf("bench tick=%d n=%d h=%d\n", t, p.n, h);
        }
    }
    double t1 = bench_now();
    printf("threads: %d\n", bench_threads());
    printf("world checksum: %016llx\n", (unsigned long long)particles_world_checksum(&p));
    bench_report_time(&a, t0, t1);
    return 0;
}
