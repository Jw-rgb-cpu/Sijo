// Embedding API checks (tests/embed.rs): save/load replaces the world, bad files change nothing,
// re-entry from a host callback is refused, a panic in a parallel system stops the game with a
// message and loading a save restarts it, create/load/destroy does not leak.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "sijo_game.h"
static int panic_tick = -1;
static SijoGame* reenter_game;
static int reenter_result = 99;
int32_t embt_panic_tick(void) { return panic_tick; }
void embt_poke(int32_t tick) { (void)tick; if (reenter_game) { reenter_result = sijo_game_step(reenter_game); reenter_game = NULL; } }
static int fails;
#define CHECK(c, ...) do { if (!(c)) { fails++; printf("FAIL %s:%d: ", __FILE__, __LINE__); printf(__VA_ARGS__); printf("\n"); } } while (0)
static long rss_kb(void) { long a = 0, b = 0; FILE* f = fopen("/proc/self/statm", "r"); if (f) { if (fscanf(f, "%ld %ld", &a, &b) != 2) b = 0; fclose(f); } return b * 4; }
static void write_file(const char* p, const char* s, size_t n) { FILE* f = fopen(p, "wb"); fwrite(s, 1, n, f); fclose(f); }
static char* read_file(const char* p, size_t* n) { FILE* f = fopen(p, "rb"); fseek(f, 0, SEEK_END); *n = (size_t)ftell(f); fseek(f, 0, SEEK_SET); char* b = malloc(*n + 1); if (fread(b, 1, *n, f) != *n) *n = 0; fclose(f); return b; }
int main(int argc, char** argv) {
    int threads = argc > 1 ? atoi(argv[1]) : 0;
    /* 1. save at 100, run to 200, load, run to 200 again: identical (load replaces the world) */
    SijoGame* g = sijo_game_create(7);
    CHECK(g && !sijo_game_error(g), "create");
    if (threads) sijo_game_set_threads(g, threads);
    for (int t = 0; t < 100; t++) CHECK(sijo_game_step(g) == 1, "step %d", t);
    uint64_t c100 = sijo_game_checksum(g);
    CHECK(sijo_game_save(g, "s100.txt") == 1, "save");
    for (int t = 0; t < 100; t++) sijo_game_step(g);
    uint64_t c200 = sijo_game_checksum(g);
    CHECK(sijo_game_load(g, "s100.txt") == 1, "load");
    CHECK(sijo_game_tick(g) == 100 && sijo_game_checksum(g) == c100, "after load: tick %d checksum %016llx want %016llx", sijo_game_tick(g), (unsigned long long)sijo_game_checksum(g), (unsigned long long)c100);
    for (int t = 0; t < 100; t++) sijo_game_step(g);
    CHECK(sijo_game_checksum(g) == c200, "replay after load diverged");
    /* into another instance */
    SijoGame* h = sijo_game_create(99);
    CHECK(sijo_game_load(h, "s100.txt") == 1 && sijo_game_checksum(h) == c100, "load into other instance");
    sijo_game_destroy(h);
    /* 2. bad files leave the game unchanged */
    size_t n; char* s = read_file("s100.txt", &n);
    int32_t tick0 = sijo_game_tick(g); uint64_t cs0 = sijo_game_checksum(g);
    write_file("bad_trunc.txt", s, n / 2);
    write_file("bad_garbage.txt", "hello world", 11);
    write_file("bad_empty.txt", "", 0);
    const char* huge = "SIJO-STATE 1\ntick 5\nseed 7\nslots 2000000000\n";
    write_file("bad_huge.txt", huge, strlen(huge));
    char* dup = malloc(n + 200); memcpy(dup, s, n); dup[n] = 0;
    char* row = strstr(dup, "\nrow "); char* rowend = row ? strchr(row + 1, '\n') : NULL;
    if (row && rowend) { size_t rl = (size_t)(rowend - row); memmove(row + rl, row, strlen(row) + 1); } /* the same row twice */
    write_file("bad_dup.txt", dup, strlen(dup));
    const char* bads[] = {"missing.txt", "bad_trunc.txt", "bad_garbage.txt", "bad_empty.txt", "bad_huge.txt", "bad_dup.txt"};
    for (int i = 0; i < 6; i++) {
        CHECK(sijo_game_load(g, bads[i]) == 0, "%s accepted", bads[i]);
        CHECK(sijo_game_tick(g) == tick0 && sijo_game_checksum(g) == cs0, "%s changed the game", bads[i]);
        CHECK(sijo_game_step(g) == 1, "step after rejected %s", bads[i]);
        tick0 = sijo_game_tick(g); cs0 = sijo_game_checksum(g);
    }
    free(s); free(dup);
    /* 3. re-entering the game from a host callback is refused */
    reenter_game = g;
    for (int t = 0; t < 30; t++) sijo_game_step(g);
    CHECK(reenter_result == -1, "re-entrant step returned %d", reenter_result);
    /* 4. a panic (in a parallel system) stops the game with a message; loading a save restarts it */
    sijo_game_save(g, "before_panic.txt");
    int32_t pt = sijo_game_tick(g) + 5;
    panic_tick = pt;
    int rc = 1, steps = 0;
    while (rc == 1 && steps < 20) { rc = sijo_game_step(g); steps++; }
    const char* err = sijo_game_error(g);
    CHECK(rc == 0 && err && strstr(err, "list index out of range") && strstr(err, "embed_api.sijo:"), "panic: rc %d err %s", rc, err ? err : "(null)");
    printf("caught: %s\n", err ? err : "(none)");
    CHECK(sijo_game_step(g) == 0, "stopped game stepped");
    panic_tick = -1;
    CHECK(sijo_game_load(g, "before_panic.txt") == 1 && sijo_game_error(g) == NULL, "load after panic");
    CHECK(sijo_game_step(g) == 1, "step after recovery");
    sijo_game_destroy(g);
    /* 5. create/destroy does not leak */
    long r0 = 0;
    for (int i = 0; i < 60; i++) {
        SijoGame* x = sijo_game_create((uint64_t)i);
        for (int t = 0; t < 20; t++) sijo_game_step(x);
        sijo_game_load(x, "s100.txt");
        sijo_game_destroy(x);
        if (i == 9) r0 = rss_kb();
    }
    long r1 = rss_kb();
#if !defined(__SANITIZE_ADDRESS__) /* ASan keeps freed memory in quarantine; LeakSanitizer checks instead */
    CHECK(r1 - r0 < 20000, "rss grew from %ld to %ld KB over 50 create/load/destroy cycles", r0, r1);
#endif
    printf("rss %ld -> %ld KB\n", r0, r1);
    sijo_game_destroy(NULL);
    SijoGameDraw d = sijo_game_draw(NULL, 0);
    CHECK(d.text_len == -1, "draw of NULL game");
    printf(fails ? "FAILED (%d)\n" : "all embedding checks passed\n", fails);
    return fails != 0;
}
