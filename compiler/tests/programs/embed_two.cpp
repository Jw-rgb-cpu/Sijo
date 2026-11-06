// Two games built with --lib-prefix game_a / game_b linked into one C++ program (tests/embed.rs).
#include <cstdio>
#include "game_a.h"
#include "game_b.h"
extern "C" int32_t embt_panic_tick(void) { return -1; }
extern "C" void embt_poke(int32_t) {}
int main() {
    SijoGame* a = game_a_create(5);
    SijoGame* b = game_b_create(5);
    for (int i = 0; i < 50; i++) { game_a_step(a); game_b_step(b); }
    unsigned long long ca = game_a_checksum(a), cb = game_b_checksum(b);
    std::printf("a=%016llx b=%016llx %s\n", ca, cb, ca == cb ? "same" : "DIFFERENT");
    SijoGameDraw d = game_a_draw(a, 0);
    std::printf("draws %d first sprite %d\n", game_a_draw_count(a), d.sprite);
    game_a_destroy(a); game_b_destroy(b);
    return ca != cb;
}
