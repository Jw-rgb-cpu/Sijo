/* ======================= embedding API (sijoc build --lib) ======================= */
struct SijoGame {
    SijoWorld S, C;
    SijoSock ss, cs;
    SijoServer sv;
    SijoClient cl;
    double dt;
    int failed; /* a panic stopped the game: it no longer steps (load a saved state or destroy it) */
    int busy;   /* inside an API call (a host callback must not re-enter the game) */
    const char* load_path; int load_ok;
};

static void sijo_game_connect(SijoGame* g) {
    g->sv.tick = g->S.tick;
    sijo_srv_attach_local(&g->sv, &g->cl);
    sijo_srv_capture(&g->sv, &g->S);
    sijo_lb_net(&g->sv, &g->cl, &g->C, g->S.tick * g->dt);
    sijo_reset_arenas(&g->S);
}

/* the in-process client side, fresh: the next snapshot it receives carries the full state */
static void sijo_game_client_reset(SijoGame* g) {
    sijo_world_free(&g->C);
    sijo_cli_free(&g->cl);
    sijo_srv_free(&g->sv);
    sijo_sock_free(&g->ss);
    sijo_sock_free(&g->cs);
    sijo_world_init(&g->C, 1, g->S.seed, 1);
    g->C.nthreads = g->S.nthreads;
    sijo_sock_pair(&g->ss, &g->cs);
    sijo_srv_init(&g->sv, &g->ss, 0, 60, 1, 5.0, g->S.seed, 1);
    sijo_cli_init(&g->cl, &g->cs, 0);
}

/* run game code; a panic in it (also on a worker thread of a parallel system) unwinds to here */
static int sijo_game_run(SijoGame* g, void (*fn)(SijoGame*)) {
    if (g->failed) return 0;
    jmp_buf jb;
    jmp_buf* oc = sijo_tl_catch;
    SijoPanic* op = sijo_tl_panic;
    g->busy = 1;
    sijo_tl_panic = &g->S.panic;
    if (setjmp(jb) == 0) {
        sijo_tl_catch = &jb;
        fn(g);
        sijo_tl_catch = oc;
        sijo_tl_panic = op;
        g->busy = 0;
        return 1;
    }
    sijo_tl_catch = oc;
    sijo_tl_panic = op;
    g->busy = 0;
    g->failed = 1;
    return 0;
}

static void sijo_game_do_start(SijoGame* g) {
    sijo_game_start(&g->S);
    sijo_game_connect(g);
}

SIJO_API SijoGame* GAMEAPI_create(uint64_t seed) {
    SijoGame* g = (SijoGame*)calloc(1, sizeof *g);
    if (!g) return NULL;
    g->dt = 1.0 / 60.0;
    sijo_world_init(&g->S, 0, seed, 1);
    sijo_world_init(&g->C, 1, seed, 1);
    sijo_sock_pair(&g->ss, &g->cs);
    sijo_srv_init(&g->sv, &g->ss, 0, 60, 1, 5.0, seed, 1);
    sijo_cli_init(&g->cl, &g->cs, 0);
    sijo_game_run(g, sijo_game_do_start); /* a panic in Game.Start: see GAMEAPI_error */
    return g;
}

static void sijo_game_do_step(SijoGame* g) {
    int t = g->S.tick + 1;
    double vt = t * g->dt;
    sijo_sock_pump(&g->ss, vt);
    sijo_sock_pump(&g->cs, vt);
    sijo_srv_recv(&g->sv, &g->S, vt); /* client input -> RPCs for this tick */
    sijo_srv_new_tick(&g->sv);
    g->S.tick = t;
    sijo_apply(&g->S);
    sijo_tick(&g->S);
    sijo_srv_capture(&g->sv, &g->S);
    sijo_lb_net(&g->sv, &g->cl, &g->C, vt);
    sijo_reset_arenas(&g->S);
}

SIJO_API int GAMEAPI_step(SijoGame* g) {
    if (!g || g->busy) return -1;
    return sijo_game_run(g, sijo_game_do_step);
}

SIJO_API const char* GAMEAPI_error(const SijoGame* g) { return (g && g->failed) ? g->S.panic.msg : NULL; }
SIJO_API int32_t GAMEAPI_tick(const SijoGame* g) { return g ? g->S.tick : 0; }
SIJO_API int32_t GAMEAPI_draw_count(const SijoGame* g) { return g ? g->C.draws.n : 0; }
SIJO_API SijoGameDraw GAMEAPI_draw(const SijoGame* g, int32_t i) {
    SijoGameDraw r = {-1, 0, 0, NULL, -1}; /* text_len -1: no such draw command */
    if (!g || i < 0 || i >= g->C.draws.n) return r;
    const SijoDraw* d = &g->C.draws.d[i];
    r.sprite = d->sprite;
    r.x = d->x;
    r.y = d->y;
    r.text_len = 0;
    if (d->tlen > 0) {
        r.text = g->C.draws.text + d->toff;
        r.text_len = d->tlen;
    }
    return r;
}
SIJO_API uint64_t GAMEAPI_checksum(SijoGame* g) { return g ? sijo_checksum(&g->S) : 0; }
SIJO_API int GAMEAPI_save(SijoGame* g, const char* path) {
    if (!g || !path || g->busy) return 0;
    return sijo_save_state(&g->S, path);
}
static void sijo_game_do_load(SijoGame* g) {
    g->load_ok = sijo_load_state_v(&g->S, g->load_path, 0); /* replaces the world only on success */
    if (g->load_ok) {
        sijo_game_client_reset(g);
        sijo_game_connect(g);
    }
}
SIJO_API int GAMEAPI_load(SijoGame* g, const char* path) {
    if (!g || !path || g->busy) return 0;
    int was_failed = g->failed;
    g->failed = 0; /* loading a saved state is how a stopped game continues */
    g->S.panic.flag = 0;
    g->load_path = path;
    g->load_ok = 0;
    if (!sijo_game_run(g, sijo_game_do_load)) return 0;
    if (!g->load_ok) {
        g->failed = was_failed;
        return 0;
    }
    return 1;
}
SIJO_API void GAMEAPI_set_threads(SijoGame* g, int n) {
    if (!g) return;
    if (n < 1) n = 1;
    if (n > SIJO_MAX_THREADS) n = SIJO_MAX_THREADS;
    g->S.nthreads = g->C.nthreads = n;
}
SIJO_API void GAMEAPI_set_panic_hook(void (*hook)(const char* msg, void* user), void* user) {
    sijo_panic_hook = hook;
    sijo_panic_user = user;
}
SIJO_API int GAMEAPI_main(int argc, char** argv) { return sijo_main(argc, argv); }
SIJO_API void GAMEAPI_destroy(SijoGame* g) {
    if (!g || g->busy) return;
    sijo_world_free(&g->S);
    sijo_world_free(&g->C);
    sijo_cli_free(&g->cl);
    sijo_srv_free(&g->sv);
    sijo_sock_free(&g->ss);
    sijo_sock_free(&g->cs);
    free(g);
}
