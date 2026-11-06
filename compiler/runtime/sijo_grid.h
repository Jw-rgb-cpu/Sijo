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
