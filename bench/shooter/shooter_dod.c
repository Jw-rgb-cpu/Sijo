/* shooter_dod.c — hand-written data-oriented C for the "shooter" workload.
 *
 * Same rules and float operation order as shooter.sijo. Expert layout:
 *  - ships live in a fixed pool of SHIPS rows (SoA). A dead ship is replaced in
 *    place at the end of the ship system and its row's generation is bumped, so
 *    (row, generation) works as a handle for bullet owners;
 *  - bullets are dense SoA arrays: spawned by appending, destroyed by a
 *    swap-remove compaction pass after the bullet system;
 *  - Jacobi reads of ship positions in the ship system use ping-pong buffers;
 *  - the bullet system runs in parallel (OpenMP), the ship system serially, as a
 *    straightforward OpenMP port would: it spawns bullets (Sijo runs it in
 *    parallel with per-thread spawn tables merged in row order; --par-ships is
 *    the variant with a separate parallel targeting pass). Cross-entity writes
 *    (hp, kills) are integer atomics: the sums commute, so the result does not
 *    depend on the thread count.
 */
#include <math.h>
#include "../common/bench.h"

#define MAX_HP 100
#define HIT_RADIUS_SQ 2.25f
#define BULLET_SPEED 20.0f
#define RELOAD 0.6f
#define DAMAGE 11
#define BULLET_LIFE 1.5f

typedef struct {
    int32_t n;
    float* px[2];   /* ping-pong position buffers */
    float* py[2];
    int cur;
    float* vx;
    float* vy;
    int32_t* hp;
    uint8_t* side;
    float* cooldown;
    int32_t* kills;
    int32_t* id;
    uint32_t* gen;  /* generation of the ship in this row (bullet owner handles) */
    int32_t* dead;  /* rows to replace at the end of the ship system */
    int32_t* target; /* --par-ships: per-ship target row from the parallel pass */
    int32_t ndead;
} Ships;

typedef struct {
    int32_t n, cap;
    float* px;
    float* py;
    float* vx;
    float* vy;
    float* life;
    int32_t* owner_row;
    uint32_t* owner_gen;
    int32_t* damage;
    uint8_t* gone;
} Bullets;

typedef struct {
    float arena;
    int32_t nships;
    Ships s;
    Bullets b;
    int64_t spawned, destroyed;
    int par_ships;
} Game;

static float wrap(float x, float arena) {
    if (x < 0.0f) x += arena;
    if (x >= arena) x -= arena;
    return x;
}

static void ship_reset(Game* g, int32_t row, int32_t id, uint8_t side) {
    int32_t s1 = nb_next((int32_t)((uint32_t)id * 7919u + 17u));
    int32_t s2 = nb_next(s1);
    int32_t s3 = nb_next(s2);
    int32_t s4 = nb_next(s3);
    Ships* s = &g->s;
    s->px[s->cur][row] = nb_unit(s1) * g->arena;
    s->py[s->cur][row] = nb_unit(s2) * g->arena;
    s->vx[row] = nb_unit(s3) * 6.0f - 3.0f;
    s->vy[row] = nb_unit(s4) * 6.0f - 3.0f;
    s->hp[row] = MAX_HP;
    s->side[row] = side;
    s->cooldown[row] = nb_unit(s4) * RELOAD;
    s->kills[row] = 0;
    s->id[row] = id;
    s->gen[row] += 1;
    g->spawned++;
}

static void bullets_grow(Bullets* b) {
    int32_t nc = b->cap ? b->cap * 2 : 1024;
    b->px = (float*)realloc(b->px, sizeof(float) * (size_t)nc);
    b->py = (float*)realloc(b->py, sizeof(float) * (size_t)nc);
    b->vx = (float*)realloc(b->vx, sizeof(float) * (size_t)nc);
    b->vy = (float*)realloc(b->vy, sizeof(float) * (size_t)nc);
    b->life = (float*)realloc(b->life, sizeof(float) * (size_t)nc);
    b->owner_row = (int32_t*)realloc(b->owner_row, sizeof(int32_t) * (size_t)nc);
    b->owner_gen = (uint32_t*)realloc(b->owner_gen, sizeof(uint32_t) * (size_t)nc);
    b->damage = (int32_t*)realloc(b->damage, sizeof(int32_t) * (size_t)nc);
    b->gone = (uint8_t*)realloc(b->gone, (size_t)nc);
    b->cap = nc;
}

static void game_init(Game* g, int32_t nships, float arena) {
    memset(g, 0, sizeof *g);
    g->arena = arena;
    g->nships = nships;
    Ships* s = &g->s;
    s->n = nships;
    for (int k = 0; k < 2; k++) {
        s->px[k] = (float*)calloc((size_t)nships, sizeof(float));
        s->py[k] = (float*)calloc((size_t)nships, sizeof(float));
    }
    s->vx = (float*)calloc((size_t)nships, sizeof(float));
    s->vy = (float*)calloc((size_t)nships, sizeof(float));
    s->hp = (int32_t*)calloc((size_t)nships, sizeof(int32_t));
    s->side = (uint8_t*)calloc((size_t)nships, 1);
    s->cooldown = (float*)calloc((size_t)nships, sizeof(float));
    s->kills = (int32_t*)calloc((size_t)nships, sizeof(int32_t));
    s->id = (int32_t*)calloc((size_t)nships, sizeof(int32_t));
    s->gen = (uint32_t*)calloc((size_t)nships, sizeof(uint32_t));
    s->dead = (int32_t*)calloc((size_t)nships, sizeof(int32_t));
    s->target = (int32_t*)calloc((size_t)nships, sizeof(int32_t));
    for (int32_t i = 0; i < nships; i++) ship_reset(g, i, i, (uint8_t)(i % 2 == 0 ? 0 : 1));
    bullets_grow(&g->b);
}

/* steer: nearest enemy (reads the start-of-system snapshot), ties broken by id */
static inline int32_t nearest_enemy(int32_t i, int32_t n, const float* restrict rpx, const float* restrict rpy,
                                    const uint8_t* restrict side, const int32_t* restrict id) {
    const float x = rpx[i], y = rpy[i];
    const uint8_t me = side[i];
    int32_t best = -1, bestId = 0;
    float bestD = 1000000000.0f;
    for (int32_t j = 0; j < n; j++) {
        if (side[j] != me) {
            const float ox = rpx[j] - x, oy = rpy[j] - y;
            const float d = ox * ox + oy * oy;
            if (d < bestD || (d == bestD && id[j] < bestId)) {
                bestD = d;
                bestId = id[j];
                best = j;
            }
        }
    }
    return best;
}

/* stage 0: ship system (serial: it spawns) */
static void ships_update(Game* g, float dt) {
    Ships* s = &g->s;
    Bullets* b = &g->b;
    const int32_t n = s->n;
    const float arena = g->arena;
    const float* restrict rpx = s->px[s->cur];
    const float* restrict rpy = s->py[s->cur];
    float* restrict wpx = s->px[s->cur ^ 1];
    float* restrict wpy = s->py[s->cur ^ 1];
    const uint8_t* restrict side = s->side;
    const int32_t* restrict id = s->id;
    s->ndead = 0;
    int32_t* restrict target = s->target;
    if (g->par_ships) {
        /* --par-ships (a variant, not used by run.py): targeting only reads
           the start-of-system snapshot, so it can run as a parallel pass; the loop below
           (which spawns) stays serial. Same results. */
#pragma omp parallel for schedule(static) if (n >= 256)
        for (int32_t i = 0; i < n; i++) target[i] = nearest_enemy(i, n, rpx, rpy, side, id);
    }
    for (int32_t i = 0; i < n; i++) {
        const float x = rpx[i], y = rpy[i];
        const int32_t best = g->par_ships ? target[i] : nearest_enemy(i, n, rpx, rpy, side, id);
        const float nx = wrap(x + s->vx[i] * dt, arena);
        const float ny = wrap(y + s->vy[i] * dt, arena);
        wpx[i] = nx;
        wpy[i] = ny;
        float cd = s->cooldown[i] - dt;
        if (cd <= 0.0f && best >= 0) {
            const float dx = rpx[best] - nx, dy = rpy[best] - ny;
            const float len = sqrtf(dx * dx + dy * dy);
            if (len > 0.001f) {
                if (b->n == b->cap) bullets_grow(b);
                const int32_t k = b->n++;
                const float sc = BULLET_SPEED / len;
                b->px[k] = nx;
                b->py[k] = ny;
                b->vx[k] = dx * sc;
                b->vy[k] = dy * sc;
                b->life[k] = BULLET_LIFE;
                b->owner_row[k] = i;
                b->owner_gen[k] = s->gen[i];
                b->damage[k] = DAMAGE;
                g->spawned++;
            }
            cd = RELOAD;
        }
        s->cooldown[i] = cd;
        if (s->hp[i] <= 0) s->dead[s->ndead++] = i;
    }
    s->cur ^= 1;
    /* deferred structural changes: each dead ship is replaced by a fresh one */
    for (int32_t k = 0; k < s->ndead; k++) {
        const int32_t row = s->dead[k];
        g->destroyed++;
        ship_reset(g, row, s->id[row] + g->nships, s->side[row]);
    }
}

/* stage 1: bullet system (parallel) */
static void bullets_update(Game* g, float dt) {
    Ships* s = &g->s;
    Bullets* b = &g->b;
    const int32_t nb = b->n, ns = s->n;
    const float arena = g->arena;
    const float* restrict spx = s->px[s->cur];
    const float* restrict spy = s->py[s->cur];
    const uint32_t* restrict sgen = s->gen;
    int32_t* restrict hp = s->hp;
    int32_t* restrict kills = s->kills;
    float* restrict px = b->px;
    float* restrict py = b->py;
    const float* restrict vx = b->vx;
    const float* restrict vy = b->vy;
    float* restrict life = b->life;
    uint8_t* restrict gone = b->gone;
#pragma omp parallel for schedule(static) if (nb >= 256)
    for (int32_t i = 0; i < nb; i++) {
        const float x = px[i] + vx[i] * dt, y = py[i] + vy[i] * dt;
        const float l = life[i] - dt;
        px[i] = x;
        py[i] = y;
        life[i] = l;
        if (l <= 0.0f || x < 0.0f || y < 0.0f || x > arena || y > arena) {
            gone[i] = 1;
            continue;
        }
        const int32_t orow = b->owner_row[i];
        const int32_t owner = sgen[orow] == b->owner_gen[i] ? orow : -1; /* -1: owner gone */
        const int32_t dmg = b->damage[i];
        int hit = 0;
        for (int32_t j = 0; j < ns; j++) {
            const float dx = spx[j] - x, dy = spy[j] - y;
            if (j != owner && dx * dx + dy * dy < HIT_RADIUS_SQ) {
#pragma omp atomic
                hp[j] -= dmg;
                if (owner >= 0) {
#pragma omp atomic
                    kills[owner] += 1;
                }
                hit = 1;
            }
        }
        gone[i] = (uint8_t)hit;
    }
    /* deferred destroys: swap-remove compaction */
    int32_t i = 0, n = nb;
    while (i < n) {
        if (gone[i]) {
            n--;
            px[i] = px[n];
            py[i] = py[n];
            b->vx[i] = b->vx[n];
            b->vy[i] = b->vy[n];
            life[i] = life[n];
            b->owner_row[i] = b->owner_row[n];
            b->owner_gen[i] = b->owner_gen[n];
            b->damage[i] = b->damage[n];
            gone[i] = gone[n];
            g->destroyed++;
        } else {
            i++;
        }
    }
    b->n = n;
}

static int32_t q(float v) { return nb_f2i(v * 1048576.0f); }

static void game_summary(const Game* g, int32_t tick, int quiet) {
    const Ships* s = &g->s;
    const Bullets* b = &g->b;
    int32_t hs = 0, hb = 0, kills = 0;
    for (int32_t i = 0; i < s->n; i++) {
        int32_t e = s->id[i];
        e = nb_mix31(e, s->hp[i]);
        e = nb_mix31(e, s->kills[i]);
        e = nb_mix31(e, q(s->px[s->cur][i]));
        e = nb_mix31(e, q(s->py[s->cur][i]));
        e = nb_mix31(e, q(s->cooldown[i]));
        hs = nb_hash_add(hs, e);
        kills = (int32_t)((uint32_t)kills + (uint32_t)s->kills[i]);
    }
    for (int32_t i = 0; i < b->n; i++) {
        int32_t e = q(b->px[i]);
        e = nb_mix31(e, q(b->py[i]));
        e = nb_mix31(e, q(b->vx[i]));
        e = nb_mix31(e, q(b->vy[i]));
        e = nb_mix31(e, q(b->life[i]));
        hb = nb_hash_add(hb, e);
    }
    if (!quiet) printf("bench tick=%d ships=%d bullets=%d kills=%d hs=%d hb=%d\n", tick, s->n, b->n, kills, hs, hb);
}

int main(int argc, char** argv) {
    BenchArgs a = bench_args(argc, argv, 500);
    if (a.arena <= 0.0f) a.arena = 256.0f;
    const float dt = 1.0f / 60.0f;
    static Game g;
    double t0 = bench_now();
    game_init(&g, a.n, a.arena);
    g.par_ships = a.par_ships;
    for (int32_t t = 1; t <= a.ticks; t++) {
        ships_update(&g, dt);
        bullets_update(&g, dt);
        if (t % 100 == 0) game_summary(&g, t, a.quiet);
    }
    double t1 = bench_now();
    printf("threads: %d\n", bench_threads());
    printf("entities alive: Ship=%d Bullet=%d (spawned %lld, destroyed %lld)\n", g.s.n, g.b.n, (long long)g.spawned, (long long)g.destroyed);
    bench_report_time(&a, t0, t1);
    return 0;
}
