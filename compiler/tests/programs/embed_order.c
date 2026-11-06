/* tests/embed.rs: an uninterrupted game and a game restored at tick 30 must draw the same sprites in
 * the same order at every later tick */
#include <stdio.h>
#include <string.h>
#include "sijo_game.h"
static int lists[61][64], counts[61];
int main(void) {
    SijoGame* a = sijo_game_create(1);
    for (int t = 1; t <= 60; t++) {
        sijo_game_step(a);
        counts[t] = sijo_game_draw_count(a);
        for (int i = 0; i < counts[t] && i < 64; i++) lists[t][i] = sijo_game_draw(a, i).sprite;
        if (t == 30) sijo_game_save(a, "order30.txt");
    }
    SijoGame* b = sijo_game_create(1);
    if (!sijo_game_load(b, "order30.txt")) { printf("load failed\n"); return 1; }
    int bad = 0;
    for (int t = 31; t <= 60; t++) {
        sijo_game_step(b);
        int n = sijo_game_draw_count(b);
        int same = n == counts[t];
        for (int i = 0; same && i < n && i < 64; i++) same = sijo_game_draw(b, i).sprite == lists[t][i];
        if (!same) { bad++; printf("tick %d: draw order differs after restore\n", t); }
    }
    printf(bad ? "FAILED\n" : "draw order identical after restore\n");
    sijo_game_destroy(a);
    sijo_game_destroy(b);
    return bad != 0;
}
