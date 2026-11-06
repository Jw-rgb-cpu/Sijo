/* host_term.c - terminal host for the Dodge game (examples/dodge).
 *
 * The game logic is Sijo; this file is the "engine": it owns the main loop, the keyboard and the
 * screen, and implements the functions the game declares `extern` (Host.sijo).
 *
 *   sijoc build examples/dodge --host examples/dodge/host_term.c --ldflags -lncurses --exe dodge
 *   ./dodge                      play (arrows or A/D move, R restart, Q quit)
 *   ./dodge --headless 900       no terminal: scripted input, prints the last frame (for tests)
 *
 * Build with -DDODGE_NO_CURSES to drop the ncurses dependency (headless only).
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "sijo_game.h"
#ifndef DODGE_NO_CURSES
#include <ncurses.h>
#endif

#define W 60
#define H 23

/* keys are "held" for a few frames after a terminal key event (terminals report no key-up) */
static int32_t held[3];
static int32_t frame;
static int headless;
static int bells;

bool dodge_key_down(int32_t key) { return key >= 0 && key < 3 && held[key] > 0; }
void dodge_bell(void) {
    bells++;
#ifndef DODGE_NO_CURSES
    if (!headless) beep();
#endif
}

/* world (0..W, 0..H, y up) -> character grid */
static void rasterize(SijoGame* g, char grid[H][W + 1]) {
    for (int y = 0; y < H; y++) {
        memset(grid[y], ' ', W);
        grid[y][W] = 0;
    }
    for (int32_t i = 0; i < sijo_game_draw_count(g); i++) {
        SijoGameDraw d = sijo_game_draw(g, i);
        int col = (int)(d.x + 0.5f), row = H - 1 - (int)(d.y + 0.5f);
        if (row < 0 || row >= H) continue;
        if (d.text) {
            for (int32_t k = 0; k < d.text_len && col + k < W; k++)
                if (col + k >= 0) grid[row][col + k] = (d.text[k] >= 32 && d.text[k] < 127) ? d.text[k] : '?';
        } else if (col >= 0 && col < W) {
            grid[row][col] = d.sprite == 1 ? 'A' : d.sprite == 2 ? 'o' : '*';
        }
    }
}

static int run_headless(int frames) {
    headless = 1;
    SijoGame* g = sijo_game_create(7);
    char grid[H][W + 1];
    for (frame = 0; frame < frames; frame++) {
        /* scripted player: weave left and right, restart when the game is over */
        memset(held, 0, sizeof held);
        held[(frame / 45) % 2 ? 0 : 1] = 1;
        if (frame % 120 == 0) held[2] = 1;
        sijo_game_step(g);
    }
    rasterize(g, grid);
    printf("+");
    for (int x = 0; x < W; x++) putchar('-');
    printf("+\n");
    for (int y = 0; y < H; y++) printf("|%s|\n", grid[y]);
    printf("+");
    for (int x = 0; x < W; x++) putchar('-');
    printf("+\n");
    printf("tick %d, bells %d, checksum %016llx\n", sijo_game_tick(g), bells, (unsigned long long)sijo_game_checksum(g));
    sijo_game_destroy(g);
    return 0;
}

int main(int argc, char** argv) {
    if (argc >= 3 && !strcmp(argv[1], "--headless")) return run_headless(atoi(argv[2]));
#ifdef DODGE_NO_CURSES
    fprintf(stderr, "built without ncurses: use --headless N\n");
    return 2;
#else
    SijoGame* g = sijo_game_create((uint64_t)time(NULL));
    initscr();
    cbreak();
    noecho();
    keypad(stdscr, TRUE);
    nodelay(stdscr, TRUE);
    curs_set(0);
    char grid[H][W + 1];
    struct timespec t0;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    double next = t0.tv_sec + t0.tv_nsec * 1e-9;
    for (frame = 0;; frame++) {
        for (int k = 0; k < 3; k++)
            if (held[k] > 0) held[k]--;
        int ch;
        int quit = 0;
        while ((ch = getch()) != ERR) {
            if (ch == KEY_LEFT || ch == 'a' || ch == 'A') held[0] = 8, held[1] = 0;
            if (ch == KEY_RIGHT || ch == 'd' || ch == 'D') held[1] = 8, held[0] = 0;
            if (ch == 'r' || ch == 'R') held[2] = 2;
            if (ch == 'q' || ch == 'Q') quit = 1;
        }
        if (quit) break;
        sijo_game_step(g); /* one 1/60 s tick: server systems, replication, client systems */
        rasterize(g, grid);
        erase();
        for (int y = 0; y < H; y++) mvaddstr(y, 0, grid[y]);
        mvhline(H, 0, '-', W);
        mvaddstr(H + 1, 0, "arrows/A D: move   R: restart   Q: quit");
        refresh();
        next += 1.0 / 60.0;
        struct timespec now;
        clock_gettime(CLOCK_MONOTONIC, &now);
        double wait = next - (now.tv_sec + now.tv_nsec * 1e-9);
        if (wait > 0) {
            struct timespec ts = {(time_t)wait, (long)((wait - (double)(time_t)wait) * 1e9)};
            nanosleep(&ts, NULL);
        }
    }
    endwin();
    printf("thanks for playing (tick %d)\n", sijo_game_tick(g));
    sijo_game_destroy(g);
    return 0;
#endif
}
