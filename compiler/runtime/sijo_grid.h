/* sijo_grid.h - hash grid for compiler-accelerated proximity queries (see src/spatial.rs).
 *
 * Built once per system run: every row is appended to the buckets of the (up to 9) cells around its
 * own cell, rows in ascending order, and a stable counting sort groups the entries by bucket. The
 * bucket of a cell then lists, in ascending row order and without duplicates, every row whose cell
 * is adjacent to it: exactly the order and (a superset of) the set of entities a full scan would
 * see pass its distance guard. A query is one array slice. Hash collisions only add candidates
 * (which fail the guard), never remove them.
 */
#define SIJO_GRID_MIN 64
#define SIJO_GRID_PAR 8192 /* rows from which the build runs on several threads */
#define SIJO_GRID_MAXT 16
typedef struct {
    int32_t* start; /* nb + 1 offsets into rows */
    int32_t* rows;  /* rows grouped by bucket, ascending inside each bucket */
    uint32_t* ek;   /* row r's buckets: ek[9r .. 9r + cnt[r]) (fixed slots, so rows emit in parallel) */
    uint8_t* cnt;
    int32_t* hist;  /* parallel build: one bucket histogram per thread */
    int32_t part[SIJO_GRID_MAXT + 1];
    int32_t nb, n, cap, bcap, hcap, ok;
    float inv;
} SijoGrid;

static inline int32_t sijo_grid_cell(const SijoGrid* g, float v) { return sijo_f2i(floorf(v * g->inv)); }
static inline uint32_t sijo_grid_hash(const SijoGrid* g, int32_t cx, int32_t cy) {
    return (uint32_t)sijo_mix(((uint64_t)(uint32_t)cx << 32) | (uint64_t)(uint32_t)cy) & (uint32_t)(g->nb - 1);
}
static void sijo_grid_free(SijoGrid* g) {
    free(g->start);
    free(g->rows);
    free(g->ek);
    free(g->cnt);
    free(g->hist);
    memset(g, 0, sizeof *g);
}
static void sijo_grid_begin(SijoGrid* g, int32_t n, float cell) {
#ifdef SIJO_NO_SPATIAL /* compile with -DSIJO_NO_SPATIAL to force full scans (used to verify equivalence) */
    g->ok = 0;
    return;
#endif
    g->ok = n >= SIJO_GRID_MIN;
    if (!g->ok) return;
    g->inv = 1.0f / cell;
    int32_t nb = 64;
    while (nb < 2 * n) nb *= 2;
    if (nb + 1 > g->bcap) {
        g->bcap = nb + 1;
        g->start = (int32_t*)sijo_xrealloc(g->start, (size_t)g->bcap * sizeof(int32_t));
    }
    if (n > g->cap) {
        g->cap = n;
        g->rows = (int32_t*)sijo_xrealloc(g->rows, (size_t)g->cap * 9 * sizeof(int32_t));
        g->ek = (uint32_t*)sijo_xrealloc(g->ek, (size_t)g->cap * 9 * sizeof(uint32_t));
        g->cnt = (uint8_t*)sijo_xrealloc(g->cnt, (size_t)g->cap);
    }
    g->nb = nb;
    g->n = n;
}
/* row r sits in cell (cx, cy): its slots get the distinct buckets of the surrounding cells */
static inline void sijo_grid_emit(SijoGrid* g, int32_t r, int32_t cx, int32_t cy, int dims) {
    uint32_t* ek = g->ek + (size_t)r * 9;
    int n = 0;
    for (int dy = (dims > 1 ? -1 : 0); dy <= (dims > 1 ? 1 : 0); dy++)
        for (int dx = -1; dx <= 1; dx++) {
            uint32_t b = sijo_grid_hash(g, (int32_t)((uint32_t)cx + (uint32_t)dx), (int32_t)((uint32_t)cy + (uint32_t)dy));
            int dup = 0;
            for (int k = 0; k < n; k++) dup |= ek[k] == b;
            if (!dup) ek[n++] = b;
        }
    g->cnt[r] = (uint8_t)n;
}
/* stable counting sort of the entries (row order, then slot order) by bucket. With several
   threads, thread t counts and scatters the rows of the t-th contiguous range: offsets are taken
   bucket by bucket in thread order, so the result is the same as the serial sort. */
static void sijo_grid_end(SijoGrid* g, int nthreads) {
    int32_t n = g->n, nb = g->nb;
    int T = 1;
#ifdef _OPENMP
    if (n >= SIJO_GRID_PAR && nthreads > 1) T = nthreads < SIJO_GRID_MAXT ? nthreads : SIJO_GRID_MAXT;
#else
    (void)nthreads;
#endif
    if (T == 1) {
        memset(g->start, 0, (size_t)(nb + 1) * sizeof(int32_t));
        for (int32_t r = 0; r < n; r++)
            for (int k = 0; k < g->cnt[r]; k++) g->start[g->ek[(size_t)r * 9 + k] + 1]++;
        for (int32_t b = 0; b < nb; b++) g->start[b + 1] += g->start[b];
        for (int32_t r = 0; r < n; r++)
            for (int k = 0; k < g->cnt[r]; k++) g->rows[g->start[g->ek[(size_t)r * 9 + k]]++] = r;
        for (int32_t b = nb; b > 0; b--) g->start[b] = g->start[b - 1];
        g->start[0] = 0;
        return;
    }
    if ((int64_t)T * nb > g->hcap) {
        g->hcap = T * nb;
        g->hist = (int32_t*)sijo_xrealloc(g->hist, (size_t)g->hcap * sizeof(int32_t));
    }
    int32_t* hist = g->hist;
    int32_t* part = g->part;
#ifdef _OPENMP
    #pragma omp parallel num_threads(T)
#endif
    {
#ifdef _OPENMP
        int t = omp_get_thread_num();
#else
        int t = 0;
#endif
        int32_t lo = (int32_t)((int64_t)n * t / T), hi = (int32_t)((int64_t)n * (t + 1) / T);
        int32_t* H = hist + (size_t)t * nb;
        memset(H, 0, (size_t)nb * sizeof(int32_t));
        for (int32_t r = lo; r < hi; r++)
            for (int k = 0; k < g->cnt[r]; k++) H[g->ek[(size_t)r * 9 + k]]++;
#ifdef _OPENMP
        #pragma omp barrier
#endif
        /* bucket totals of this thread's bucket range, then their offsets */
        int32_t blo = (int32_t)((int64_t)nb * t / T), bhi = (int32_t)((int64_t)nb * (t + 1) / T);
        int32_t sum = 0;
        for (int32_t b = blo; b < bhi; b++)
            for (int u = 0; u < T; u++) sum += hist[(size_t)u * nb + b];
        part[t + 1] = sum;
#ifdef _OPENMP
        #pragma omp barrier
        #pragma omp single
#endif
        {
            part[0] = 0;
            for (int u = 0; u < T; u++) part[u + 1] += part[u];
        }
        int32_t run = part[t];
        for (int32_t b = blo; b < bhi; b++) {
            g->start[b] = run;
            for (int u = 0; u < T; u++) {
                int32_t c = hist[(size_t)u * nb + b];
                hist[(size_t)u * nb + b] = run;
                run += c;
            }
        }
#ifdef _OPENMP
        #pragma omp barrier
#endif
        for (int32_t r = lo; r < hi; r++)
            for (int k = 0; k < g->cnt[r]; k++) g->rows[H[g->ek[(size_t)r * 9 + k]]++] = r;
    }
    g->start[nb] = part[T];
}

/* ---------------- grids: 2D and 3D arrays (Grid<T> = T[,], Grid3<T> = T[,,]) ----------------
 * Step and Move run their cells in parallel from this many cells (when no parallel region is open) */
#ifndef SIJO_GRID_PAR_MIN
#define SIJO_GRID_PAR_MIN 4096
#endif
/* Grid.Dir(dx, dy, dz): 0 = stay, else 1 + (dx+1) + 3 (dy+1) + 9 (dz+1) */
static inline int32_t sijo_grid_dir(int32_t dx, int32_t dy, int32_t dz) {
    if (dx < -1 || dx > 1 || dy < -1 || dy > 1 || dz < -1 || dz > 1) sijo_panic("Grid.Dir: each of dx, dy, dz must be -1, 0 or 1");
    return (dx | dy | dz) ? 1 + (dx + 1) + 3 * (dy + 1) + 9 * (dz + 1) : 0;
}
/* a proposal of Move: Grid.Stay (0) or a Grid.Dir value; anything else panics */
static inline int32_t sijo_grid_dnorm(int32_t d) {
    if (d < 0 || d > 27) sijo_panic("Move: the lambda must return Grid.Dir(dx, dy) or Grid.Stay");
    return d == 14 ? 0 : d;
}
static inline int32_t sijo_grid_ddx(int32_t d) { return (d - 1) % 3 - 1; }
static inline int32_t sijo_grid_ddy(int32_t d) { return (d - 1) / 3 % 3 - 1; }
static inline int32_t sijo_grid_ddz(int32_t d) { return (d - 1) / 9 - 1; }
/* Random in the lambda of a grid operation: keyed by the operation (k: one draw of the entity's
 * sequence), the cell and the draw's number in the cell, so any thread draws the same numbers */
static inline uint64_t sijo_cell_key(uint64_t k, uint32_t site, uint32_t cell, uint32_t n) {
    return sijo_mix(k ^ (((uint64_t)site << 32) | n) ^ ((uint64_t)cell * 0x9E3779B97F4A7C15ull));
}
static inline float sijo_cell_randf(uint64_t k, uint32_t site, uint32_t cell, uint32_t n, float lo, float hi) {
    return lo + (hi - lo) * sijo_rand01(sijo_cell_key(k, site, cell, n));
}
static inline int32_t sijo_cell_randi(uint64_t k, uint32_t site, uint32_t cell, uint32_t n, int32_t lo, int32_t hi) {
    if (hi <= lo) return lo;
    return (int32_t)((int64_t)lo + (int64_t)(sijo_cell_key(k, site, cell, n) % (uint64_t)((int64_t)hi - (int64_t)lo)));
}
/* grid.Flood(x, y[, z], v): the region of cells equal (bit for bit) to the start cell, connected
 * through shared faces, becomes v, breadth first (+x, -x, +y, -y, +z, -z); returns the cells changed */
static int32_t sijo_grid_flood(SijoList* g, int rank, int32_t x, int32_t y, int32_t z, const void* v, int32_t esz) {
    int32_t d0 = g->d0, d1 = g->d1, d2 = rank == 3 ? g->d2 : 1;
    if (x < 0 || x >= d0 || y < 0 || y >= d1 || z < 0 || z >= d2) return 0;
    uint8_t* b = (uint8_t*)g->data;
    uint8_t from[16];
    int32_t start = (x * d1 + y) * d2 + z;
    memcpy(from, b + (size_t)start * esz, (size_t)esz);
    if (!memcmp(from, v, (size_t)esz)) return 0;
    int32_t* q = (int32_t*)sijo_xrealloc(NULL, (size_t)g->len * sizeof(int32_t));
    int32_t head = 0, tail = 0;
    memcpy(b + (size_t)start * esz, v, (size_t)esz);
    q[tail++] = start;
    while (head < tail) {
        int32_t i = q[head++];
        int32_t cz = i % d2, cy = i / d2 % d1, cx = i / d2 / d1;
        int32_t nb[6]; int nn = 0;
        if (cx + 1 < d0) nb[nn++] = i + d1 * d2;
        if (cx > 0) nb[nn++] = i - d1 * d2;
        if (cy + 1 < d1) nb[nn++] = i + d2;
        if (cy > 0) nb[nn++] = i - d2;
        if (rank == 3 && cz + 1 < d2) nb[nn++] = i + 1;
        if (rank == 3 && cz > 0) nb[nn++] = i - 1;
        for (int k = 0; k < nn; k++) {
            uint8_t* c = b + (size_t)nb[k] * esz;
            if (!memcmp(c, from, (size_t)esz)) {
                memcpy(c, v, (size_t)esz);
                q[tail++] = nb[k];
            }
        }
    }
    free(q);
    return tail;
}
