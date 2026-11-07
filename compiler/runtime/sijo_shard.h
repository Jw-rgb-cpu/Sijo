/* sijo_shard.h - distributed simulation, the world side. The model is described in
 * src/shard.rs and docs/05-networking.md (5.7); the transport and process modes are in
 * sijo_shard_net.h.
 *
 * Every shard keeps the same slot table and row order (structural changes are applied everywhere
 * in one global order) and owns some rows. At each barrier a shard sends every other shard one
 * message: the deferred commands, spawns and prints of the rows it ran (with their global order
 * key), changed components of its rows for the shards that read them, ownership handoffs and its
 * load. Then every shard merges what it received and applies all commands, exactly as one process
 * would. This file has two parts: types and hooks the generated code uses (emitted after the world
 * type), and the world logic (emitted after the generated per-program part, src/shard.rs). */
#include <stdarg.h>
#define SIJO_SH_MAXN 64      /* shards (a halo mask has one bit per shard) */
#define SIJO_SH_VERSION 1    /* message format of the shard exchange */
struct SijoShard;

/* growable byte writer: little-endian, varints */
typedef struct { uint8_t* p; size_t n, cap; } SijoShW;
static void sijo_shw_need(SijoShW* b, size_t k) {
    if (b->n + k <= b->cap) return;
    size_t nc = b->cap ? b->cap * 2 : 256;
    while (nc < b->n + k) nc *= 2;
    b->p = (uint8_t*)sijo_xrealloc(b->p, nc);
    b->cap = nc;
}
static inline void sijo_shw_u8(SijoShW* b, uint32_t v) { sijo_shw_need(b, 1); b->p[b->n++] = (uint8_t)v; }
static inline void sijo_shw_u16(SijoShW* b, uint32_t v) { sijo_shw_need(b, 2); b->p[b->n] = (uint8_t)v; b->p[b->n + 1] = (uint8_t)(v >> 8); b->n += 2; }
static inline void sijo_shw_u32(SijoShW* b, uint32_t v) {
    sijo_shw_need(b, 4);
    b->p[b->n] = (uint8_t)v; b->p[b->n + 1] = (uint8_t)(v >> 8); b->p[b->n + 2] = (uint8_t)(v >> 16); b->p[b->n + 3] = (uint8_t)(v >> 24);
    b->n += 4;
}
static inline void sijo_shw_u64(SijoShW* b, uint64_t v) { sijo_shw_u32(b, (uint32_t)v); sijo_shw_u32(b, (uint32_t)(v >> 32)); }
static inline void sijo_shw_var(SijoShW* b, uint64_t v) {
    while (v >= 0x80) { sijo_shw_u8(b, (uint32_t)(v & 0x7f) | 0x80); v >>= 7; }
    sijo_shw_u8(b, (uint32_t)v);
}
static inline void sijo_shw_bytes(SijoShW* b, const void* p, size_t n) {
    if (!n) return;
    sijo_shw_need(b, n);
    memcpy(b->p + b->n, p, n);
    b->n += n;
}
static void sijo_shw_free(SijoShW* b) { free(b->p); memset(b, 0, sizeof *b); }

/* bounds-checked reader; handles from another shard may be provisional (sender's spawn) */
typedef struct { const uint8_t* p; size_t n, pos; int bad; struct SijoWorld* w; int sender; } SijoShR;
static inline uint32_t sijo_shr_u8(SijoShR* r) { if (r->pos + 1 > r->n) { r->bad = 1; return 0; } return r->p[r->pos++]; }
static inline uint32_t sijo_shr_u16(SijoShR* r) { uint32_t a = sijo_shr_u8(r); return a | (sijo_shr_u8(r) << 8); }
static inline uint32_t sijo_shr_u32(SijoShR* r) {
    if (r->pos + 4 > r->n) { r->bad = 1; r->pos = r->n; return 0; }
    const uint8_t* q = r->p + r->pos;
    r->pos += 4;
    return (uint32_t)q[0] | ((uint32_t)q[1] << 8) | ((uint32_t)q[2] << 16) | ((uint32_t)q[3] << 24);
}
static inline uint64_t sijo_shr_u64(SijoShR* r) { uint64_t a = sijo_shr_u32(r); return a | ((uint64_t)sijo_shr_u32(r) << 32); }
static inline uint64_t sijo_shr_var(SijoShR* r) {
    uint64_t v = 0;
    for (int s = 0; s < 64; s += 7) {
        uint32_t b = sijo_shr_u8(r);
        if (r->bad) return 0;
        v |= (uint64_t)(b & 0x7f) << s;
        if (!(b & 0x80)) return v;
    }
    r->bad = 1;
    return 0;
}
static inline const uint8_t* sijo_shr_bytes(SijoShR* r, uint64_t n) {
    if (n > r->n - r->pos) { r->bad = 1; r->pos = r->n; return NULL; }
    const uint8_t* q = r->p + r->pos;
    r->pos += (size_t)n;
    return q;
}
static SijoH sijo_sh_prov(SijoWorld* w, int sender, SijoH h);
static inline SijoH sijo_shr_handle(SijoShR* r) {
    SijoH h;
    h.idx = sijo_shr_u32(r);
    h.gen = sijo_shr_u32(r);
    if (h.idx & SIJO_PROV) h = sijo_sh_prov(r->w, r->sender, h);
    return h;
}
static void sijo_sh_note_spawn(SijoWorld* w, int arch, int tid, int32_t row);
static void sijo_sh_capture_print(SijoWorld* w, const char* fmt, va_list ap);
static void sijo_sh_mark(SijoWorld* w, int j);
static void sijo_sh_free(SijoWorld* w);

/* Print: on a shard, prints of the systems are kept with their order key and written by the
   coordinator in the order one process would print them; prints that every shard computes (the
   command application, Game.Start) are written by the coordinator only */
static void sijo_printf(SijoWorld* w, const char* fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    if (w->sh_on) {
        if (w->sh_cap) { sijo_sh_capture_print(w, fmt, ap); va_end(ap); return; }
        if (w->sh_id != 0) { va_end(ap); return; }
    }
    vprintf(fmt, ap);
    va_end(ap);
}
/* @@SIJO_SHARD_BODY@@ */

/* ======================= distributed simulation: world logic (runtime/sijo_shard.h) ======================= */
enum { SIJO_SHK_RPC = 1, SIJO_SHK_SEG = 2, SIJO_SHK_STAGE = 3, SIJO_SHK_GATHER = 4 };

/* the order of a spawn: (system, archetype position, spawned archetype for systems that merge per
   archetype, spawner row, n-th spawn of that row) - what one process would allocate slots in */
typedef struct { uint16_t j, apos, x; uint32_t row, seq; } SijoSKey;
static int sijo_skey_cmp(const SijoSKey* a, const SijoSKey* b) {
    if (a->j != b->j) return a->j < b->j ? -1 : 1;
    if (a->apos != b->apos) return a->apos < b->apos ? -1 : 1;
    if (a->x != b->x) return a->x < b->x ? -1 : 1;
    if (a->row != b->row) return a->row < b->row ? -1 : 1;
    if (a->seq != b->seq) return a->seq < b->seq ? -1 : 1;
    return 0;
}
typedef struct { uint64_t k; SijoH h; } SijoPMap;
typedef struct { SijoSKey* v; int32_t n, cap; } SijoSNotes;

typedef struct SijoShard {
    int id, n;
    float bounds[SIJO_SH_MAXN];   /* slab s holds X in [bounds[s-1], bounds[s]) */
    float nbounds[SIJO_SH_MAXN];
    int nb_pending, want_samples, sending_samples;
    int32_t last_rebal;
    int32_t load[SIJO_SH_MAXN];
    float* mysamp; int32_t nmysamp, mycount;
    /* per slot: a valid halo copy here (receiver side); shards holding one (owner side) */
    uint8_t* hv; uint64_t* mask; int32_t scap;
    int stage; int32_t tick;
    uint64_t bno; /* barriers so far: the same count on every shard (lockstep) */
    /* system boundaries in the command buffer of the current stage */
    int32_t* mark_j; int32_t* mark_n; int nmark, cmark;
    /* stage-start images of owned rows: offsets per (archetype, row), or UINT32_MAX */
    SijoShW cap; uint32_t* capoff[SIJO_NARCH]; int32_t capn[SIJO_NARCH];
    SijoSNotes notes[SIJO_NARCH][SIJO_MAX_THREADS];
    SijoH sp_last[SIJO_MAX_THREADS]; uint32_t sp_seq[SIJO_MAX_THREADS];
    /* captured prints */
    SijoShW pr; SijoH pr_last; uint32_t pr_seq; int32_t npr;
    /* one message per peer and barrier */
    SijoShW out[SIJO_SH_MAXN];
    const uint8_t* in[SIJO_SH_MAXN]; size_t inlen[SIJO_SH_MAXN];
    SijoPMap* pmap; int32_t npmap, cpmap;
    SijoShW tmp, cur;
    int stop, peer_stop;
    /* statistics */
    int64_t handoffs, handoffs_in, rebalances, barriers, sent_bytes, sent_msgs, xcmds, xspawns, syncs, leaves;
    double sum_imb, max_imb; int64_t nimb;
    int64_t g_bytes, g_msgs, g_handoffs, g_cmds, g_spawns, g_syncs, g_leaves; /* the other shards' (gathered) */
    double t_sys, t_build, t_wait, t_install, t_apply; /* seconds in each phase (--timing) */
} SijoShard;

static void sijo_sh_init(SijoWorld* w, int id, int n) {
    SijoShard* s = (SijoShard*)calloc(1, sizeof *s);
    if (!s) { fprintf(stderr, "sijo: out of memory\n"); exit(101); }
    s->id = id; s->n = n; s->last_rebal = -1000000;
    w->sh = s; w->sh_on = n > 1 || id > 0; w->sh_id = id; w->nshards = n;
}
static void sijo_sh_free(SijoWorld* w) {
    SijoShard* s = w->sh;
    if (!s) return;
    free(s->mysamp); free(s->hv); free(s->mask); free(s->mark_j); free(s->mark_n);
    sijo_shw_free(&s->cap); sijo_shw_free(&s->pr); sijo_shw_free(&s->tmp); sijo_shw_free(&s->cur);
    for (int a = 0; a < SIJO_NARCH; a++) {
        free(s->capoff[a]);
        for (int t = 0; t < SIJO_MAX_THREADS; t++) free(s->notes[a][t].v);
    }
    for (int j = 0; j < SIJO_SH_MAXN; j++) sijo_shw_free(&s->out[j]);
    free(s->pmap);
    free(s);
    w->sh = NULL; w->sh_on = 0; w->sh_hv = NULL;
}
/* per-slot arrays cover every slot */
static void sijo_sh_fit(SijoWorld* w) {
    SijoShard* s = w->sh;
    if (w->ents.n > 0) (void)sijo_shard_ref(&w->ents, (uint32_t)(w->ents.cap - 1));
    if (s->scap < w->ents.cap) {
        int32_t nc = w->ents.cap;
        s->hv = (uint8_t*)sijo_xrealloc(s->hv, (size_t)nc);
        s->mask = (uint64_t*)sijo_xrealloc(s->mask, (size_t)nc * sizeof(uint64_t));
        memset(s->hv + s->scap, 0, (size_t)(nc - s->scap));
        memset(s->mask + s->scap, 0, (size_t)(nc - s->scap) * sizeof(uint64_t));
        s->scap = nc;
    }
    w->sh_hv = s->hv;
}
static inline int sijo_sh_slab(const SijoShard* s, float x) {
    int k = 0;
    while (k < s->n - 1 && x >= s->bounds[k]) k++;
    return k;
}
static inline uint64_t sijo_sh_all(const SijoShard* s) { return (s->n >= 64 ? ~0ull : ((1ull << s->n) - 1)) & ~(1ull << s->id); }

/* every shard runs this after each command application: entities that appeared (spawns merged,
   spawns of replicated code) get an owner, and every shard has their full state */
static void sijo_sh_after_apply(SijoWorld* w) {
    SijoShard* s = w->sh;
    if (!s) return;
    sijo_sh_fit(w);
    for (int a = 0; a < SIJO_NARCH; a++) {
        int32_t cnt = sijo_sh_count(w, 0, 0, a);
        for (int32_t r = 0; r < cnt; r++) {
            SijoH h = sijo_sh_ent(w, a, r);
            if (w->ents.shard[h.idx] >= 0) continue;
            int o;
            if (sijo_sh_spatial[a]) { float x = sijo_sh_x(w, 0, 0, a, r); o = x == x ? sijo_sh_slab(s, x) : 0; }
            else o = (int)(h.idx % (uint32_t)s->n);
            w->ents.shard[h.idx] = o;
            s->hv[h.idx] = 1;
            s->mask[h.idx] = o == s->id ? sijo_sh_all(s) : 0;
        }
    }
}
static int sijo_cmpf_sh(const void* a, const void* b) { float x = *(const float*)a, y = *(const float*)b; return (x > y) - (x < y); }
/* the first partition: every shard holds the whole start state, so all compute the same quantiles */
static void sijo_sh_initial(SijoWorld* w) {
    SijoShard* s = w->sh;
    sijo_sh_fit(w);
    int32_t m = 0, cap = 0;
    for (int a = 0; a < SIJO_NARCH; a++) if (sijo_sh_spatial[a]) cap += sijo_sh_count(w, 0, 0, a);
    float* xs = (float*)sijo_xrealloc(NULL, sizeof(float) * (size_t)(cap + 1));
    for (int a = 0; a < SIJO_NARCH; a++) {
        if (!sijo_sh_spatial[a]) continue;
        for (int32_t r = 0; r < sijo_sh_count(w, 0, 0, a); r++) { float x = sijo_sh_x(w, 0, 0, a, r); if (x == x) xs[m++] = x; }
    }
    qsort(xs, (size_t)m, sizeof(float), sijo_cmpf_sh);
    for (int k = 0; k < s->n - 1; k++) s->bounds[k] = m ? xs[(int64_t)(k + 1) * m / s->n] : 0.0f;
    free(xs);
    for (int32_t i = 0; i < w->ents.cap; i++) w->ents.shard[i] = -1;
    sijo_sh_after_apply(w);
}

/* ---------------- hooks of the generated code ---------------- */
static void sijo_sh_note_spawn(SijoWorld* w, int arch, int tid, int32_t row) {
    SijoShard* s = w->sh;
    SijoH cur = w->tls[tid].cur;
    SijoSNotes* nt = &s->notes[arch][tid];
    while (nt->cap <= row) { nt->cap = nt->cap ? nt->cap * 2 : 64; nt->v = (SijoSKey*)sijo_xrealloc(nt->v, (size_t)nt->cap * sizeof(SijoSKey)); }
    if (nt->n <= row) nt->n = row + 1;
    if (!sijo_heq(cur, s->sp_last[tid])) { s->sp_last[tid] = cur; s->sp_seq[tid] = 0; }
    SijoSKey k;
    memset(&k, 0, sizeof k);
    int j = w->sh_sys < 0 ? 0 : w->sh_sys;
    k.j = (uint16_t)j;
    if (cur.idx && cur.idx < (uint32_t)w->ents.n) {
        SijoSlot* sl = &w->ents.s[cur.idx];
        k.apos = (uint16_t)sijo_sh_apos(j, sl->arch);
        k.row = (uint32_t)sl->row;
    }
    k.x = sijo_sh_tspawn[j] ? (uint16_t)arch : 0;
    k.seq = s->sp_seq[tid]++;
    nt->v[row] = k;
}
static void sijo_sh_capture_print(SijoWorld* w, const char* fmt, va_list ap) {
    SijoShard* s = w->sh;
    char buf[1024];
    va_list aq;
    va_copy(aq, ap);
    int len = vsnprintf(buf, sizeof buf, fmt, ap);
    if (len < 0) len = 0;
    SijoH cur = w->tls[sijo_tid()].cur;
    if (!sijo_heq(cur, s->pr_last)) { s->pr_last = cur; s->pr_seq = 0; }
    int j = w->sh_sys < 0 ? 0 : w->sh_sys;
    uint32_t apos = 0, row = 0;
    if (cur.idx && cur.idx < (uint32_t)w->ents.n) { apos = (uint32_t)sijo_sh_apos(j, w->ents.s[cur.idx].arch); row = (uint32_t)w->ents.s[cur.idx].row; }
    sijo_shw_u16(&s->pr, (uint32_t)j); sijo_shw_u16(&s->pr, apos); sijo_shw_u32(&s->pr, row); sijo_shw_u32(&s->pr, s->pr_seq++);
    sijo_shw_u32(&s->pr, (uint32_t)len);
    if ((size_t)len < sizeof buf) sijo_shw_bytes(&s->pr, buf, (size_t)len);
    else { char* big = (char*)sijo_xrealloc(NULL, (size_t)len + 1); vsnprintf(big, (size_t)len + 1, fmt, aq); sijo_shw_bytes(&s->pr, big, (size_t)len); free(big); }
    va_end(aq);
    s->npr++;
}
static void sijo_sh_mark(SijoWorld* w, int j) {
    SijoShard* s = w->sh;
    if (!s) return;
    if (s->nmark == s->cmark) {
        s->cmark = s->cmark ? s->cmark * 2 : 16;
        s->mark_j = (int32_t*)sijo_xrealloc(s->mark_j, (size_t)s->cmark * sizeof(int32_t));
        s->mark_n = (int32_t*)sijo_xrealloc(s->mark_n, (size_t)s->cmark * sizeof(int32_t));
    }
    s->mark_j[s->nmark] = j;
    s->mark_n[s->nmark] = w->cmd[0].n;
    s->nmark++;
}
static uint64_t sijo_sh_pkey(int sender, uint32_t idx, uint32_t gen) {
    /* provisional handle (SIJO_PROV | arch << 8 | thread, row + 1) of a shard */
    return ((uint64_t)(uint32_t)sender << 48) | ((uint64_t)((idx >> 8) & 0xffu) << 40) | ((uint64_t)(idx & 0xffu) << 32) | (uint64_t)(gen - 1u);
}
static SijoH sijo_sh_prov(SijoWorld* w, int sender, SijoH h) {
    SijoShard* s = w->sh;
    if (!s) return SIJO_NULL;
    uint64_t k = sijo_sh_pkey(sender, h.idx, h.gen);
    int32_t lo = 0, hi = s->npmap - 1;
    while (lo <= hi) {
        int32_t m = (lo + hi) / 2;
        if (s->pmap[m].k == k) return s->pmap[m].h;
        if (s->pmap[m].k < k) lo = m + 1; else hi = m - 1;
    }
    return SIJO_NULL;
}
static int sijo_pmap_cmp(const void* a, const void* b) { uint64_t x = ((const SijoPMap*)a)->k, y = ((const SijoPMap*)b)->k; return (x > y) - (x < y); }

/* ---------------- stage start: images of what this stage's partitioned systems may change ---------------- */
static void sijo_sh_capture(SijoWorld* w, int stage) {
    SijoShard* s = w->sh;
    s->stage = stage;
    s->cap.n = 0;
    for (int a = 0; a < SIJO_NARCH; a++) {
        uint64_t m = sijo_sh_swrite[stage][a];
        int32_t cnt = sijo_sh_count(w, 0, 0, a);
        if (!m) { s->capn[a] = 0; continue; }
        if (s->capn[a] < cnt || !s->capoff[a]) s->capoff[a] = (uint32_t*)sijo_xrealloc(s->capoff[a], sizeof(uint32_t) * (size_t)(cnt + 1));
        s->capn[a] = cnt;
        for (int32_t r = 0; r < cnt; r++) {
            SijoH h = sijo_sh_ent(w, a, r);
            if (w->ents.shard[h.idx] != s->id) { s->capoff[a][r] = UINT32_MAX; continue; }
            s->capoff[a][r] = (uint32_t)s->cap.n;
            for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) {
                if (!((m >> ci) & 1)) continue;
                size_t at = s->cap.n;
                sijo_shw_u32(&s->cap, 0);
                sijo_sh_pack(w, 0, 0, a, r, ci, &s->cap);
                uint32_t len = (uint32_t)(s->cap.n - at - 4);
                memcpy(s->cap.p + at, &len, 4); /* local only: host order is fine */
            }
        }
    }
}

/* ---------------- the barrier message ----------------
 * header: version u8, kind u8, barrier number u64, tick u32, sender u16, stop u8
 * then six sections, each [u32 byte length][content]:
 *   0 commands  [count] { j u16, apos u16, row u32, kind u8, target idx u32 gen u32, field u16, off u16,
 *                         size u16, v u32, data[size] }                      (RPCs: the coordinator's)
 *   1 spawns    [count] { j u16, apos u16, x u16, row u32, seq u32, arch u16, thread u8, ptrow u32,
 *                         len u32, the images of every component }
 *   2 load      load u32, has-samples u8 [, owned count u32, k u32, k floats]
 *   3 prints    [count] { j u16, apos u16, row u32, seq u32, len u32, text }          (to shard 0)
 *   4 copies    [count] { slot gap var, gen var, flag u8 (1 enters the halo, 2 leaves), component
 *                         mask var, images of the components in the mask }
 *   5 handoffs  [count] { idx u32, gen u32, new owner u8, halo mask u64, has-state u8 [, images] }
 *               (at the end of a run, the gathered state goes to the coordinator the same way) */
#define SIJO_SH_NSEC 6
static inline void sijo_shw_patch32(SijoShW* b, size_t at, uint32_t v) {
    uint8_t* q = b->p + at;
    q[0] = (uint8_t)v; q[1] = (uint8_t)(v >> 8); q[2] = (uint8_t)(v >> 16); q[3] = (uint8_t)(v >> 24);
}
/* commands whose arguments are bytes in the buffer: field writes, messages, added components */
static inline int sijo_sh_cmd_has_data(uint32_t kind) { return kind == SIJO_CMD_SET || kind == SIJO_CMD_CALL || kind == SIJO_CMD_CADD || kind == SIJO_CMD_HOST; }
static inline size_t sijo_sh_sec_begin(SijoShW* b) { size_t at = b->n; sijo_shw_u32(b, 0); return at; }
static inline void sijo_sh_sec_end(SijoShW* b, size_t at) { sijo_shw_patch32(b, at, (uint32_t)(b->n - at - 4)); }

static void sijo_sh_put_cmd(SijoShW* b, const SijoCmdBuf* cb, const SijoCmd* c, uint32_t j, uint32_t apos, uint32_t row) {
    sijo_shw_u16(b, j); sijo_shw_u16(b, apos); sijo_shw_u32(b, row);
    sijo_shw_u8(b, c->kind); sijo_shw_u32(b, c->target.idx); sijo_shw_u32(b, c->target.gen);
    sijo_shw_u16(b, c->field); sijo_shw_u16(b, c->off); sijo_shw_u16(b, c->size);
    uint32_t v;
    memcpy(&v, &c->v, 4);
    sijo_shw_u32(b, v);
    if (sijo_sh_cmd_has_data(c->kind) && c->size) sijo_shw_bytes(b, cb->data + c->data, c->size);
}
/* the system that pushed command i of the stage (from the marks) */
static int sijo_sh_cmd_sys(const SijoShard* s, int32_t i, int* k) {
    while (*k < s->nmark && i >= s->mark_n[*k]) (*k)++;
    return *k < s->nmark ? s->mark_j[*k] : -1;
}
static void sijo_sh_cmd_key(SijoWorld* w, const SijoCmd* c, int j, uint32_t* apos, uint32_t* row) {
    *apos = 0; *row = 0;
    if (c->src.idx && !(c->src.idx & SIJO_PROV) && c->src.idx < (uint32_t)w->ents.n) {
        *apos = (uint32_t)sijo_sh_apos(j, w->ents.s[c->src.idx].arch);
        *row = (uint32_t)w->ents.s[c->src.idx].row;
    }
}
/* the halo of shard j: its slab widened by the hysteresis and the radius (+1% for rounding) */
static int sijo_sh_inside(const SijoShard* s, int j, float x, float r) {
    float wd = (float)SIJO_SH_HYST + r * 1.01f + 1e-3f;
    float lo = j > 0 ? s->bounds[j - 1] - wd : -INFINITY;
    float hi = j < s->n - 1 ? s->bounds[j] + wd : INFINITY;
    return x >= lo && x < hi;
}
static int32_t sijo_sh_owned_spatial(SijoWorld* w) {
    int32_t load = 0;
    for (int a = 0; a < SIJO_NARCH; a++) {
        if (!sijo_sh_spatial[a]) continue;
        int32_t cnt = sijo_sh_count(w, 0, 0, a);
        for (int32_t r = 0; r < cnt; r++) if (w->ents.shard[sijo_sh_ent(w, a, r).idx] == w->sh_id) load++;
    }
    return load;
}

/* the sections every peer gets the same: commands, spawns, load */
static void sijo_sh_build_common(SijoWorld* w, int kind, SijoShW* b) {
    SijoShard* s = w->sh;
    int me = s->id;
    b->n = 0;
    sijo_shw_u8(b, SIJO_SH_VERSION); sijo_shw_u8(b, (uint32_t)kind); sijo_shw_u64(b, s->bno);
    sijo_shw_u32(b, (uint32_t)s->tick); sijo_shw_u16(b, (uint32_t)me); sijo_shw_u8(b, (uint32_t)s->stop);
    /* 0 commands */
    size_t sec = sijo_sh_sec_begin(b);
    size_t cat = sijo_sh_sec_begin(b);
    uint32_t nc = 0;
    if (kind == SIJO_SHK_RPC && me == 0) {
        for (int32_t i = 0; i < w->cmd[0].n; i++) { sijo_sh_put_cmd(b, &w->cmd[0], &w->cmd[0].c[i], 0, 0, 0); nc++; }
    } else if (kind == SIJO_SHK_STAGE) {
        int k = 0;
        for (int32_t i = 0; i < w->cmd[0].n; i++) {
            int j = sijo_sh_cmd_sys(s, i, &k);
            if (j < 0 || sijo_sh_mode[j] != 0) continue; /* replicated systems: every shard has them */
            uint32_t apos, row;
            sijo_sh_cmd_key(w, &w->cmd[0].c[i], j, &apos, &row);
            sijo_sh_put_cmd(b, &w->cmd[0], &w->cmd[0].c[i], (uint32_t)j, apos, row);
            nc++;
        }
        s->xcmds += nc;
    }
    sijo_shw_patch32(b, cat, nc);
    sijo_sh_sec_end(b, sec);
    /* 1 spawns of the partitioned systems (every shard gives them slots in key order) */
    sec = sijo_sh_sec_begin(b);
    cat = sijo_sh_sec_begin(b);
    uint32_t nsp = 0;
    if (kind == SIJO_SHK_STAGE || kind == SIJO_SHK_SEG) {
        for (int a = 0; a < SIJO_NARCH; a++)
            for (int t = 0; t < SIJO_MAX_THREADS; t++) {
                int32_t cnt = sijo_sh_count(w, 2, t, a);
                for (int32_t i = 0; i < cnt; i++) {
                    SijoSKey* key = &s->notes[a][t].v[i];
                    sijo_shw_u16(b, key->j); sijo_shw_u16(b, key->apos); sijo_shw_u16(b, key->x);
                    sijo_shw_u32(b, key->row); sijo_shw_u32(b, key->seq);
                    sijo_shw_u16(b, (uint32_t)a); sijo_shw_u8(b, (uint32_t)t); sijo_shw_u32(b, (uint32_t)i);
                    size_t lat = sijo_sh_sec_begin(b);
                    for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) sijo_sh_pack(w, 2, t, a, i, ci, b);
                    sijo_sh_sec_end(b, lat);
                    nsp++;
                }
            }
        s->xspawns += nsp;
    }
    sijo_shw_patch32(b, cat, nsp);
    sijo_sh_sec_end(b, sec);
    /* 2 load, and position samples for new slab bounds after an imbalance */
    sec = sijo_sh_sec_begin(b);
    int32_t load = kind == SIJO_SHK_STAGE ? sijo_sh_owned_spatial(w) : 0;
    sijo_shw_u32(b, (uint32_t)load);
    if (kind == SIJO_SHK_STAGE) s->load[me] = load;
    if (kind == SIJO_SHK_GATHER) { /* this shard's exchange statistics, for the coordinator's summary */
        sijo_shw_u64(b, (uint64_t)s->sent_bytes); sijo_shw_u64(b, (uint64_t)s->sent_msgs); sijo_shw_u64(b, (uint64_t)s->handoffs);
        sijo_shw_u64(b, (uint64_t)s->xcmds); sijo_shw_u64(b, (uint64_t)s->xspawns); sijo_shw_u64(b, (uint64_t)s->syncs); sijo_shw_u64(b, (uint64_t)s->leaves);
    }
    s->sending_samples = 0;
    if (kind == SIJO_SHK_STAGE && s->want_samples) {
        s->want_samples = 0;
        s->sending_samples = 1;
        int32_t m = 0;
        float* xs = (float*)sijo_xrealloc(NULL, sizeof(float) * (size_t)(load + 1));
        for (int a = 0; a < SIJO_NARCH; a++) {
            if (!sijo_sh_spatial[a]) continue;
            int32_t cnt = sijo_sh_count(w, 0, 0, a);
            for (int32_t r = 0; r < cnt; r++) {
                if (w->ents.shard[sijo_sh_ent(w, a, r).idx] != me) continue;
                float x = sijo_sh_x(w, 0, 0, a, r);
                if (x == x && m < load) xs[m++] = x;
            }
        }
        qsort(xs, (size_t)m, sizeof(float), sijo_cmpf_sh);
        int32_t k = m < 256 ? m : 256;
        s->mysamp = (float*)sijo_xrealloc(s->mysamp, sizeof(float) * (size_t)(k + 1));
        for (int32_t i = 0; i < k; i++) s->mysamp[i] = xs[(int64_t)i * m / k];
        s->nmysamp = k; s->mycount = m;
        free(xs);
        sijo_shw_u8(b, 1); sijo_shw_u32(b, (uint32_t)m); sijo_shw_u32(b, (uint32_t)k);
        for (int32_t i = 0; i < k; i++) sijo_shw_u32(b, sijo_f2bits(s->mysamp[i]));
    } else {
        sijo_shw_u8(b, 0);
    }
    sijo_sh_sec_end(b, sec);
}

static void sijo_sh_build(SijoWorld* w, int kind) {
    SijoShard* s = w->sh;
    int me = s->id, n = s->n;
    s->barriers++;
    s->bno++;
    /* new slab bounds take effect at the same barrier on every shard */
    if (kind == SIJO_SHK_STAGE && s->nb_pending) { memcpy(s->bounds, s->nbounds, sizeof s->bounds); s->nb_pending = 0; }
    sijo_sh_build_common(w, kind, &s->tmp);
    size_t at[SIJO_SH_MAXN], cat[SIJO_SH_MAXN];
    uint32_t cnt[SIJO_SH_MAXN];
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        s->out[j].n = 0;
        sijo_shw_bytes(&s->out[j], s->tmp.p, s->tmp.n);
    }
    /* 3 prints of the partitioned systems: to the coordinator */
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        SijoShW* b = &s->out[j];
        at[j] = sijo_sh_sec_begin(b);
        cat[j] = sijo_sh_sec_begin(b);
        uint32_t np = 0;
        if (j == 0 && kind == SIJO_SHK_STAGE) {
            SijoShR r = {s->pr.p, s->pr.n, 0, 0, w, me};
            while (r.pos < r.n && !r.bad) {
                size_t st = r.pos;
                uint32_t sj = sijo_shr_u16(&r); sijo_shr_u16(&r); sijo_shr_u32(&r); sijo_shr_u32(&r);
                uint32_t len = sijo_shr_u32(&r);
                sijo_shr_bytes(&r, len);
                if (sj < SIJO_SH_NSYS && sijo_sh_mode[sj] == 0) { sijo_shw_bytes(b, s->pr.p + st, r.pos - st); np++; }
            }
        }
        sijo_shw_patch32(b, cat[j], np);
        sijo_sh_sec_end(b, at[j]);
    }
    /* 4 copies of changed components of owned rows, for the shards that read them */
    uint32_t lastslot[SIJO_SH_MAXN];
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        at[j] = sijo_sh_sec_begin(&s->out[j]);
        cat[j] = sijo_sh_sec_begin(&s->out[j]);
        cnt[j] = 0; lastslot[j] = 0;
    }
    if (kind == SIJO_SHK_STAGE) {
        for (int a = 0; a < SIJO_NARCH; a++) {
            uint64_t sw = sijo_sh_swrite[s->stage][a], hm = sijo_sh_halomask[a];
            uint64_t allm = 0, coordm = 0;
            for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) {
                uint8_t rt = sijo_sh_route[sijo_sh_cid[a][ci]];
                if (rt & 1) allm |= 1ull << ci;
                if (rt & 2) coordm |= 1ull << ci;
            }
            if (!((allm | coordm) & sw) && !hm) continue;
            int32_t cntr = sijo_sh_count(w, 0, 0, a);
            float hr = sijo_sh_halo[a];
            for (int32_t r = 0; r < cntr; r++) {
                SijoH h = sijo_sh_ent(w, a, r);
                if (w->ents.shard[h.idx] != me) continue;
                /* what this stage changed: today's images against the stage-start ones */
                uint64_t changed = 0;
                size_t offs[65];
                s->cur.n = 0;
                const uint8_t* cp = (sw && r < s->capn[a] && s->capoff[a][r] != UINT32_MAX) ? s->cap.p + s->capoff[a][r] : NULL;
                for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) {
                    offs[ci] = s->cur.n;
                    if (!((sw >> ci) & 1)) continue;
                    sijo_sh_pack(w, 0, 0, a, r, ci, &s->cur);
                    size_t nlen = s->cur.n - offs[ci];
                    if (cp) {
                        uint32_t olen;
                        memcpy(&olen, cp, 4);
                        if (nlen != olen || memcmp(cp + 4, s->cur.p + offs[ci], nlen)) changed |= 1ull << ci;
                        cp += 4 + olen;
                    } else {
                        changed |= 1ull << ci;
                    }
                }
                offs[sijo_sh_ncomp[a]] = s->cur.n;
                float x = hm ? sijo_sh_x(w, 0, 0, a, r) : 0.0f;
                for (int j = 0; j < n; j++) {
                    if (j == me) continue;
                    uint64_t send = (allm | (j == 0 ? coordm : 0)) & changed;
                    int flag = 0;
                    if (hm) {
                        int in = x == x && sijo_sh_inside(s, j, x, hr);
                        int was = (int)((s->mask[h.idx] >> j) & 1);
                        if (in && !was) { send |= hm; flag = 1; s->mask[h.idx] |= 1ull << j; }
                        else if (in) send |= hm & changed;
                        else if (was) { flag = 2; s->mask[h.idx] &= ~(1ull << j); s->leaves++; }
                    }
                    if (!send && flag != 2) continue;
                    SijoShW* b = &s->out[j];
                    sijo_shw_var(b, (uint64_t)(h.idx - lastslot[j]));
                    lastslot[j] = h.idx;
                    sijo_shw_var(b, h.gen);
                    sijo_shw_u8(b, (uint32_t)flag);
                    sijo_shw_var(b, send);
                    for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) {
                        if (!((send >> ci) & 1)) continue;
                        if ((sw >> ci) & 1) {
                            int cj = ci + 1;
                            while (cj < sijo_sh_ncomp[a] && !((sw >> cj) & 1)) cj++;
                            sijo_shw_bytes(b, s->cur.p + offs[ci], offs[cj] - offs[ci]);
                        } else {
                            sijo_sh_pack(w, 0, 0, a, r, ci, b);
                        }
                    }
                    cnt[j]++;
                    s->syncs++;
                }
            }
        }
    }
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        sijo_shw_patch32(&s->out[j], cat[j], cnt[j]);
        sijo_sh_sec_end(&s->out[j], at[j]);
    }
    /* 5 handoffs: entities more than the hysteresis past their slab move; every shard learns the
       new owner, the new owner gets the whole state (and which shards hold a halo copy) */
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        at[j] = sijo_sh_sec_begin(&s->out[j]);
        cat[j] = sijo_sh_sec_begin(&s->out[j]);
        cnt[j] = 0;
    }
    if (kind == SIJO_SHK_STAGE) {
        for (int a = 0; a < SIJO_NARCH; a++) {
            if (!sijo_sh_spatial[a]) continue;
            int32_t cntr = sijo_sh_count(w, 0, 0, a);
            for (int32_t r = 0; r < cntr; r++) {
                SijoH h = sijo_sh_ent(w, a, r);
                if (w->ents.shard[h.idx] != me) continue;
                float x = sijo_sh_x(w, 0, 0, a, r);
                if (x != x) continue;
                float lo = me > 0 ? s->bounds[me - 1] - (float)SIJO_SH_HYST : -INFINITY;
                float hi = me < n - 1 ? s->bounds[me] + (float)SIJO_SH_HYST : INFINITY;
                if (x >= lo && x < hi) continue;
                int to = sijo_sh_slab(s, x);
                if (to == me) continue;
                uint64_t m = (s->mask[h.idx] | (1ull << me)) & ~(1ull << to);
                for (int j = 0; j < n; j++) {
                    if (j == me) continue;
                    SijoShW* b = &s->out[j];
                    sijo_shw_u32(b, h.idx); sijo_shw_u32(b, h.gen); sijo_shw_u8(b, (uint32_t)to); sijo_shw_u64(b, m);
                    sijo_shw_u8(b, j == to);
                    if (j == to) for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) sijo_sh_pack(w, 0, 0, a, r, ci, b);
                    cnt[j]++;
                }
                w->ents.shard[h.idx] = to;
                s->hv[h.idx] = 1;
                s->mask[h.idx] = 0;
                s->handoffs++;
            }
        }
    } else if (kind == SIJO_SHK_GATHER && me != 0) {
        SijoShW* b = &s->out[0];
        for (int a = 0; a < SIJO_NARCH; a++) {
            int32_t cntr = sijo_sh_count(w, 0, 0, a);
            for (int32_t r = 0; r < cntr; r++) {
                SijoH h = sijo_sh_ent(w, a, r);
                if (w->ents.shard[h.idx] != me) continue;
                sijo_shw_u32(b, h.idx); sijo_shw_u32(b, h.gen); sijo_shw_u8(b, (uint32_t)me); sijo_shw_u64(b, 0); sijo_shw_u8(b, 1);
                for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) sijo_sh_pack(w, 0, 0, a, r, ci, b);
                cnt[0]++;
            }
        }
    }
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        sijo_shw_patch32(&s->out[j], cat[j], cnt[j]);
        sijo_sh_sec_end(&s->out[j], at[j]);
        s->sent_bytes += (int64_t)s->out[j].n;
        s->sent_msgs++;
    }
}

/* ---------------- installing what the other shards sent ---------------- */
typedef struct { SijoSKey k; int sender; int arch, tid; int32_t ptrow; const uint8_t* img; size_t len; } SijoSRec;
static int sijo_srec_cmp(const void* a, const void* b) {
    const SijoSRec* x = (const SijoSRec*)a; const SijoSRec* y = (const SijoSRec*)b;
    int c = sijo_skey_cmp(&x->k, &y->k);
    if (c) return c;
    return (x->sender > y->sender) - (x->sender < y->sender);
}
typedef struct { uint64_t k; int sender; int32_t order; SijoCmd c; const uint8_t* data; } SijoCRec;
static int sijo_crec_cmp(const void* a, const void* b) {
    const SijoCRec* x = (const SijoCRec*)a; const SijoCRec* y = (const SijoCRec*)b;
    if (x->k != y->k) return x->k < y->k ? -1 : 1;
    if (x->sender != y->sender) return x->sender < y->sender ? -1 : 1;
    return (x->order > y->order) - (x->order < y->order);
}
typedef struct { uint64_t k1; uint32_t seq; int sender; const uint8_t* text; uint32_t len; } SijoPRec;
static int sijo_prec_cmp(const void* a, const void* b) {
    const SijoPRec* x = (const SijoPRec*)a; const SijoPRec* y = (const SijoPRec*)b;
    if (x->k1 != y->k1) return x->k1 < y->k1 ? -1 : 1;
    if (x->seq != y->seq) return x->seq < y->seq ? -1 : 1;
    return (x->sender > y->sender) - (x->sender < y->sender);
}
typedef struct { float x; double wt; } SijoQ;
static int sijo_q_cmp(const void* a, const void* b) {
    const SijoQ* x = (const SijoQ*)a; const SijoQ* y = (const SijoQ*)b;
    return (x->x > y->x) - (x->x < y->x);
}
static void sijo_sh_fail(SijoWorld* w, const char* what, int sender) {
    fprintf(stderr, "sijo shard %d: malformed %s from shard %d (tick %d)\n", w->sh_id, what, sender, (int)w->tick);
    fflush(stderr);
    exit(9);
}
static int sijo_sh_rd_cmd(SijoShR* r, SijoCRec* o) {
    uint64_t j = sijo_shr_u16(r), apos = sijo_shr_u16(r), row = sijo_shr_u32(r);
    memset(&o->c, 0, sizeof o->c);
    o->c.kind = (uint16_t)sijo_shr_u8(r);
    o->c.target.idx = sijo_shr_u32(r); o->c.target.gen = sijo_shr_u32(r);
    o->c.field = (uint16_t)sijo_shr_u16(r); o->c.off = (uint16_t)sijo_shr_u16(r); o->c.size = (uint16_t)sijo_shr_u16(r);
    uint32_t v = sijo_shr_u32(r);
    memcpy(&o->c.v, &v, 4);
    o->data = NULL;
    if (sijo_sh_cmd_has_data(o->c.kind) && o->c.size) o->data = sijo_shr_bytes(r, o->c.size);
    o->k = (j << 48) | (apos << 32) | row;
    return !r->bad;
}
static void sijo_sh_install(SijoWorld* w, int kind) {
    SijoShard* s = w->sh;
    int me = s->id, n = s->n;
    SijoShR sec[SIJO_SH_MAXN][SIJO_SH_NSEC];
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        SijoShR r = {s->in[j], s->inlen[j], 0, 0, w, j};
        uint32_t ver = sijo_shr_u8(&r), k = sijo_shr_u8(&r);
        uint64_t bno = sijo_shr_u64(&r);
        uint32_t tick = sijo_shr_u32(&r), from = sijo_shr_u16(&r), stop = sijo_shr_u8(&r);
        if (r.bad || ver != SIJO_SH_VERSION || (int)k != kind || bno != s->bno || (int32_t)tick != s->tick || (int)from != j) {
            fprintf(stderr, "sijo shard %d: barrier message from shard %d is version %u kind %u barrier %llu tick %u; expected version %d kind %d barrier %llu tick %d\n", me, j, ver, k,
                    (unsigned long long)bno, tick, SIJO_SH_VERSION, kind, (unsigned long long)s->bno, (int)s->tick);
            sijo_sh_fail(w, "barrier header", j);
        }
        if (j == 0 && stop) s->peer_stop = 1;
        for (int q = 0; q < SIJO_SH_NSEC; q++) {
            uint32_t len = sijo_shr_u32(&r);
            const uint8_t* p = sijo_shr_bytes(&r, len);
            if (r.bad) sijo_sh_fail(w, "section", j);
            SijoShR x = {p, len, 0, 0, w, j};
            sec[j][q] = x;
        }
    }
    /* spawns: this shard's (per-thread tables) and the others', in the global order (the map of
       provisional handles lives until the stage's commands are resolved) */
    {
        int64_t total = 0;
        uint32_t nsp[SIJO_SH_MAXN];
        for (int j = 0; j < n; j++) if (j != me) { nsp[j] = sijo_shr_u32(&sec[j][1]); total += nsp[j]; }
        for (int a = 0; a < SIJO_NARCH; a++) for (int t = 0; t < SIJO_MAX_THREADS; t++) total += sijo_sh_count(w, 2, t, a);
        if (total) {
            SijoSRec* v = (SijoSRec*)sijo_xrealloc(NULL, sizeof(SijoSRec) * (size_t)total);
            int64_t m = 0;
            for (int a = 0; a < SIJO_NARCH; a++)
                for (int t = 0; t < SIJO_MAX_THREADS; t++)
                    for (int32_t i = 0; i < sijo_sh_count(w, 2, t, a); i++) {
                        SijoSRec* e = &v[m++];
                        e->k = s->notes[a][t].v[i]; e->sender = me; e->arch = a; e->tid = t; e->ptrow = i; e->img = NULL; e->len = 0;
                    }
            for (int j = 0; j < n; j++) {
                if (j == me) continue;
                SijoShR* r = &sec[j][1];
                for (uint32_t i = 0; i < nsp[j]; i++) {
                    SijoSRec* e = &v[m++];
                    e->k.j = (uint16_t)sijo_shr_u16(r); e->k.apos = (uint16_t)sijo_shr_u16(r); e->k.x = (uint16_t)sijo_shr_u16(r);
                    e->k.row = sijo_shr_u32(r); e->k.seq = sijo_shr_u32(r);
                    e->arch = (int)sijo_shr_u16(r); e->tid = (int)sijo_shr_u8(r); e->ptrow = (int32_t)sijo_shr_u32(r);
                    e->sender = j;
                    uint32_t len = sijo_shr_u32(r);
                    e->img = sijo_shr_bytes(r, len);
                    e->len = len;
                    if (r->bad || e->arch >= SIJO_NARCH || e->tid >= SIJO_MAX_THREADS) sijo_sh_fail(w, "spawn", j);
                }
            }
            qsort(v, (size_t)m, sizeof(SijoSRec), sijo_srec_cmp);
            s->pmap = (SijoPMap*)sijo_xrealloc(s->pmap, sizeof(SijoPMap) * (size_t)(s->npmap + m + 1));
            /* every spawn's slot first, in key order; then the received images (they may name
               entities spawned later in the same barrier) */
            int32_t* rows = (int32_t*)sijo_xrealloc(NULL, sizeof(int32_t) * (size_t)(m + 1));
            for (int64_t i = 0; i < m; i++) {
                SijoSRec* e = &v[i];
                SijoH h;
                if (e->sender == me) { h = sijo_sh_move_pt(w, e->arch, e->tid, e->ptrow); rows[i] = -1; }
                else h = sijo_sh_push_pending(w, e->arch, &rows[i]);
                s->pmap[s->npmap].k = sijo_sh_pkey(e->sender, SIJO_PROV | ((uint32_t)e->arch << 8) | (uint32_t)e->tid, (uint32_t)e->ptrow + 1u);
                s->pmap[s->npmap].h = h;
                s->npmap++;
            }
            qsort(s->pmap, (size_t)s->npmap, sizeof(SijoPMap), sijo_pmap_cmp);
            for (int64_t i = 0; i < m; i++) {
                SijoSRec* e = &v[i];
                if (e->sender == me) continue;
                SijoShR ir = {e->img, e->len, 0, 0, w, e->sender};
                for (int ci = 0; ci < sijo_sh_ncomp[e->arch]; ci++) sijo_sh_unpack(w, 1, 0, e->arch, rows[i], ci, &ir);
                if (ir.bad || ir.pos != ir.n) sijo_sh_fail(w, "spawn images", e->sender);
            }
            free(rows);
            free(v);
            sijo_sh_pt_clear(w);
            for (int a = 0; a < SIJO_NARCH; a++) for (int t = 0; t < SIJO_MAX_THREADS; t++) s->notes[a][t].n = 0;
            for (int t = 0; t < SIJO_MAX_THREADS; t++) s->sp_last[t] = SIJO_NULL;
        }
        if (kind == SIJO_SHK_STAGE || kind == SIJO_SHK_SEG) sijo_sh_fix(w);
    }
    /* component copies */
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        SijoShR* r = &sec[j][4];
        uint32_t nrec = sijo_shr_u32(r);
        uint32_t slot = 0;
        for (uint32_t i = 0; i < nrec && !r->bad; i++) {
            slot += (uint32_t)sijo_shr_var(r);
            uint32_t gen = (uint32_t)sijo_shr_var(r);
            uint32_t flag = sijo_shr_u8(r);
            uint64_t m = sijo_shr_var(r);
            if (r->bad || slot == 0 || slot >= (uint32_t)w->ents.n || w->ents.s[slot].gen != gen || w->ents.s[slot].state != SIJO_ALIVE) sijo_sh_fail(w, "component copy", j);
            int a = w->ents.s[slot].arch;
            int32_t row = w->ents.s[slot].row;
            for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) if ((m >> ci) & 1) sijo_sh_unpack(w, 0, 0, a, row, ci, r);
            if (flag == 1) s->hv[slot] = 1;
            else if (flag == 2) s->hv[slot] = 0;
        }
        if (r->bad) sijo_sh_fail(w, "component copies", j);
    }
    /* handoffs (and, at the end of a run, the gathered state) */
    for (int j = 0; j < n; j++) {
        if (j == me) continue;
        SijoShR* r = &sec[j][5];
        uint32_t nh = sijo_shr_u32(r);
        for (uint32_t i = 0; i < nh && !r->bad; i++) {
            uint32_t idx = sijo_shr_u32(r), gen = sijo_shr_u32(r), to = sijo_shr_u8(r);
            uint64_t m = sijo_shr_u64(r);
            uint32_t data = sijo_shr_u8(r);
            if (r->bad || idx == 0 || idx >= (uint32_t)w->ents.n || w->ents.s[idx].gen != gen || w->ents.s[idx].state != SIJO_ALIVE || (int)to >= n) sijo_sh_fail(w, "handoff", j);
            if (kind != SIJO_SHK_GATHER) w->ents.shard[idx] = (int32_t)to;
            if (data) {
                int a = w->ents.s[idx].arch;
                int32_t row = w->ents.s[idx].row;
                for (int ci = 0; ci < sijo_sh_ncomp[a]; ci++) sijo_sh_unpack(w, 0, 0, a, row, ci, r);
                if (kind != SIJO_SHK_GATHER) { s->hv[idx] = 1; s->mask[idx] = m & ~(1ull << me); s->handoffs_in++; }
            }
        }
        if (r->bad) sijo_sh_fail(w, "handoffs", j);
    }
    /* commands in the global order (the next sijo_apply runs them on every shard) */
    if (kind == SIJO_SHK_RPC || kind == SIJO_SHK_STAGE) {
        int64_t own = w->cmd[0].n, tot = own;
        uint32_t ncmd[SIJO_SH_MAXN];
        for (int j = 0; j < n; j++) if (j != me) { ncmd[j] = sijo_shr_u32(&sec[j][0]); tot += ncmd[j]; }
        SijoCRec* v = (SijoCRec*)sijo_xrealloc(NULL, sizeof(SijoCRec) * (size_t)(tot + 1));
        int64_t m = 0;
        int k = 0;
        for (int32_t i = 0; i < own; i++) {
            SijoCmd* c = &w->cmd[0].c[i];
            SijoCRec* e = &v[m++];
            if (kind == SIJO_SHK_STAGE) {
                int j = sijo_sh_cmd_sys(s, i, &k);
                if (j < 0) j = 0;
                uint32_t apos, row;
                sijo_sh_cmd_key(w, c, j, &apos, &row);
                e->k = ((uint64_t)(uint32_t)j << 48) | ((uint64_t)apos << 32) | row;
            } else {
                e->k = 0;
            }
            e->sender = me; e->order = i; e->c = *c;
            e->data = sijo_sh_cmd_has_data(c->kind) && c->size ? w->cmd[0].data + c->data : NULL;
        }
        for (int j = 0; j < n; j++) {
            if (j == me) continue;
            for (uint32_t i = 0; i < ncmd[j]; i++) {
                SijoCRec* e = &v[m++];
                if (!sijo_sh_rd_cmd(&sec[j][0], e)) sijo_sh_fail(w, "command", j);
                if (kind == SIJO_SHK_RPC) e->k = 0;
                e->sender = j; e->order = (int32_t)i;
            }
        }
        qsort(v, (size_t)m, sizeof(SijoCRec), sijo_crec_cmp);
        SijoCmdBuf nb;
        memset(&nb, 0, sizeof nb);
        for (int64_t i = 0; i < m; i++) {
            SijoCRec* e = &v[i];
            SijoCmd c = e->c;
            if (c.target.idx & SIJO_PROV) c.target = sijo_sh_prov(w, e->sender, c.target);
            if (e->data) c.data = sijo_cmd_data(&nb, e->data, c.size);
            *sijo_cmd_push(&nb) = c;
        }
        free(v);
        sijo_cmdbuf_free(&w->cmd[0]);
        w->cmd[0] = nb;
        s->nmark = 0;
        s->npmap = 0;
    }
    /* the coordinator prints what the systems printed, in the order of one process */
    if (kind == SIJO_SHK_STAGE) {
        if (me == 0) {
            int64_t tot = s->npr;
            uint32_t npr[SIJO_SH_MAXN];
            for (int j = 1; j < n; j++) { npr[j] = sijo_shr_u32(&sec[j][3]); tot += npr[j]; }
            if (tot) {
                SijoPRec* v = (SijoPRec*)sijo_xrealloc(NULL, sizeof(SijoPRec) * (size_t)tot);
                int64_t m = 0;
                for (int j = 0; j < n; j++) {
                    SijoShR own = {s->pr.p, s->pr.n, 0, 0, w, 0};
                    SijoShR* r = j == me ? &own : &sec[j][3];
                    uint32_t cnt = j == me ? (uint32_t)s->npr : npr[j];
                    for (uint32_t i = 0; i < cnt; i++) {
                        SijoPRec* e = &v[m++];
                        uint64_t jj = sijo_shr_u16(r), ap = sijo_shr_u16(r), row = sijo_shr_u32(r);
                        e->k1 = (jj << 48) | (ap << 32) | row;
                        e->seq = sijo_shr_u32(r);
                        e->len = sijo_shr_u32(r);
                        e->text = sijo_shr_bytes(r, e->len);
                        e->sender = j;
                    }
                    if (r->bad) sijo_sh_fail(w, "prints", j);
                }
                qsort(v, (size_t)m, sizeof(SijoPRec), sijo_prec_cmp);
                for (int64_t i = 0; i < m; i++) if (v[i].text) fwrite(v[i].text, 1, v[i].len, stdout);
                free(v);
            }
        }
        s->pr.n = 0; s->npr = 0; s->pr_last = SIJO_NULL;
    }
    if (kind == SIJO_SHK_GATHER && me == 0) {
        for (int j = 1; j < n; j++) {
            SijoShR* r = &sec[j][2];
            sijo_shr_u32(r);
            s->g_bytes += (int64_t)sijo_shr_u64(r); s->g_msgs += (int64_t)sijo_shr_u64(r); s->g_handoffs += (int64_t)sijo_shr_u64(r);
            s->g_cmds += (int64_t)sijo_shr_u64(r); s->g_spawns += (int64_t)sijo_shr_u64(r); s->g_syncs += (int64_t)sijo_shr_u64(r); s->g_leaves += (int64_t)sijo_shr_u64(r);
        }
    }
    /* loads; new slab bounds from everyone's samples (the same computation on every shard) */
    if (kind == SIJO_SHK_STAGE) {
        SijoQ* q = NULL;
        int64_t nq = 0, cq = 0;
        double total = 0;
        int any = s->sending_samples;
        for (int j = 0; j < n; j++) {
            float* tmpx = NULL;
            const float* xs = NULL;
            int32_t k = 0, c = 0;
            if (j == me) {
                if (s->sending_samples) { xs = s->mysamp; k = s->nmysamp; c = s->mycount; }
            } else {
                SijoShR* r = &sec[j][2];
                s->load[j] = (int32_t)sijo_shr_u32(r);
                if (sijo_shr_u8(r)) {
                    any = 1;
                    c = (int32_t)sijo_shr_u32(r); k = (int32_t)sijo_shr_u32(r);
                    if (k < 0 || (uint64_t)k * 4 > r->n) sijo_sh_fail(w, "samples", j);
                    tmpx = (float*)sijo_xrealloc(NULL, sizeof(float) * (size_t)(k + 1));
                    for (int32_t i = 0; i < k; i++) tmpx[i] = sijo_bits2f(sijo_shr_u32(r));
                    xs = tmpx;
                }
                if (r->bad) sijo_sh_fail(w, "load", j);
            }
            for (int32_t i = 0; i < k; i++) {
                if (nq == cq) { cq = cq ? cq * 2 : 256; q = (SijoQ*)sijo_xrealloc(q, sizeof(SijoQ) * (size_t)cq); }
                q[nq].x = xs[i]; q[nq].wt = (double)c / (double)k; nq++;
            }
            free(tmpx);
        }
        if (any && nq) {
            /* weighted quantiles (a stable merge keeps equal positions in shard order) */
            SijoQ* t2 = (SijoQ*)sijo_xrealloc(NULL, sizeof(SijoQ) * (size_t)nq);
            for (int64_t wdt = 1; wdt < nq; wdt *= 2) {
                for (int64_t lo = 0; lo < nq; lo += 2 * wdt) {
                    int64_t mid = lo + wdt < nq ? lo + wdt : nq, hi = lo + 2 * wdt < nq ? lo + 2 * wdt : nq, a = lo, b = mid, o = lo;
                    while (a < mid && b < hi) t2[o++] = q[b].x < q[a].x ? q[b++] : q[a++];
                    while (a < mid) t2[o++] = q[a++];
                    while (b < hi) t2[o++] = q[b++];
                }
                memcpy(q, t2, sizeof(SijoQ) * (size_t)nq);
            }
            free(t2);
            for (int64_t i = 0; i < nq; i++) total += q[i].wt;
            double acc = 0;
            int b = 0;
            for (int64_t i = 0; i < nq && b < n - 1; i++) {
                acc += q[i].wt;
                while (b < n - 1 && acc >= total * (double)(b + 1) / (double)n) { s->nbounds[b] = q[i].x; b++; }
            }
            while (b < n - 1) { s->nbounds[b] = q[nq - 1].x; b++; }
            s->nb_pending = 1;
            s->rebalances++;
            s->last_rebal = s->tick;
        }
        free(q);
        s->sending_samples = 0;
    }
}

/* after the last barrier of a tick: the load imbalance (max / mean owned entities with a
   position); above 1.25 the next barrier carries position samples and new bounds follow */
static void sijo_sh_balance(SijoWorld* w) {
    SijoShard* s = w->sh;
    int64_t tot = 0, mx = 0;
    for (int j = 0; j < s->n; j++) { tot += s->load[j]; if (s->load[j] > mx) mx = s->load[j]; }
    double imb = tot ? (double)mx / ((double)tot / s->n) : 1.0;
    s->sum_imb += imb; s->nimb++;
    if (imb > s->max_imb) s->max_imb = imb;
    if (s->n > 1 && imb > 1.25 && !s->want_samples && !s->nb_pending && s->tick - s->last_rebal >= 30) s->want_samples = 1;
}

/* ---------------- the lockstep driver: every local world runs one tick ----------------
   In-process (--shards N) this process holds every shard's world; a shard process holds one and
   its transport (sijo_shard_net.h) exchanges the messages with the other processes. */
typedef struct {
    SijoWorld** ws; int nw;     /* the worlds this process runs; ws[0] is the coordinator in-process */
    void (*xfer)(int kind);     /* deliver every local world's out[] and fill every in[] */
    int32_t tick;
} SijoShDrv;
static SijoShDrv sijo_shd;
static void sijo_sh_barrier(int kind) {
    double t0 = sijo_now();
    for (int i = 0; i < sijo_shd.nw; i++) sijo_sh_build(sijo_shd.ws[i], kind);
    double t1 = sijo_now();
    sijo_shd.xfer(kind);
    double t2 = sijo_now();
    for (int i = 0; i < sijo_shd.nw; i++) sijo_sh_install(sijo_shd.ws[i], kind);
    double t3 = sijo_now();
    SijoShard* s = sijo_shd.ws[0]->sh;
    s->t_build += t1 - t0; s->t_wait += t2 - t1; s->t_install += t3 - t2;
}
static void sijo_sh_apply_all(void) {
    double t0 = sijo_now();
    for (int i = 0; i < sijo_shd.nw; i++) { sijo_apply(sijo_shd.ws[i]); sijo_sh_after_apply(sijo_shd.ws[i]); }
    sijo_shd.ws[0]->sh->t_apply += sijo_now() - t0;
}
static void sijo_sh_tick_all(int32_t tick) {
    sijo_shd.tick = tick;
    for (int i = 0; i < sijo_shd.nw; i++) { sijo_shd.ws[i]->tick = tick; sijo_shd.ws[i]->sh->tick = tick; }
    if (SIJO_SH_RPC) {
        /* the coordinator's client RPCs, executed by every shard at the start of the tick */
        sijo_sh_barrier(SIJO_SHK_RPC);
    }
    sijo_sh_apply_all();
    for (int seg = 0; seg < SIJO_SH_NSEG; seg++) {
        int first = seg == 0 || sijo_sh_seg_end[seg - 1];
        double t0 = sijo_now();
        for (int i = 0; i < sijo_shd.nw; i++) {
            SijoWorld* w = sijo_shd.ws[i];
            sijo_sh_fit(w);
            if (first) sijo_sh_capture(w, sijo_sh_seg_stage[seg]);
            w->sh_cap = 1;
            sijo_sh_seg_run(w, seg);
            w->sh_cap = 0;
        }
        sijo_shd.ws[0]->sh->t_sys += sijo_now() - t0;
        sijo_sh_barrier(sijo_sh_seg_end[seg] ? SIJO_SHK_STAGE : SIJO_SHK_SEG);
        if (sijo_sh_seg_end[seg]) sijo_sh_apply_all();
    }
    for (int i = 0; i < sijo_shd.nw; i++) sijo_sh_balance(sijo_shd.ws[i]);
    for (int i = 0; i < sijo_shd.nw; i++) SIJO_TICK_END(sijo_shd.ws[i]);
}
