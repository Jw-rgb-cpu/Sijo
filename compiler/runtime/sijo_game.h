/* GAMEAPI_HDR - embedding API of a Sijo game built with `sijoc build <game> --lib -o game.c`.
 *
 * The game runs the authoritative simulation and its local client in one process (the same code
 * path as the default executable): each step is one fixed 1/60 s tick of server systems, delta
 * replication, and client systems. Client code draws with Draw.Sprite / Draw.Text; read the draw
 * list after each step. Host functions declared in Sijo with [Extern] are plain C functions you
 * define (their prototypes are at the end of this header).
 *
 * Building the generated C (gcc or clang, also MinGW on Windows):
 *   cc -std=gnu11 -O2 -ffp-contract=off -c game.c            add -fopenmp for parallel systems,
 *   cc ... -fPIC -shared game.c host_externs.c -o libgame.so    -lm (and -lws2_32 on Windows) to link.
 *   For a Windows DLL define SIJO_GAME_BUILD_DLL when compiling game.c and SIJO_GAME_USE_DLL in hosts.
 *   Results are bit-identical on every platform and with any optimization flags; -ffast-math and
 *   x87 float math are rejected at compile time.
 * Threads: one game may be used by one thread at a time; separate games are independent. Parallel
 *   systems use up to 64 OpenMP threads (GAMEAPI_set_threads).
 * Errors: a runtime panic in game code (list index out of range, missing dictionary key, division
 *   by zero, out of memory...) makes the call return 0 and GAMEAPI_error the message; the game then
 *   stops stepping until a saved state is loaded. Calling the game from inside one of its own host
 *   callbacks returns -1.
 */
#ifndef GAMEAPI_GUARD
#define GAMEAPI_GUARD
#include <stdint.h>
#include <stdbool.h>
#ifndef SIJO_API
#if defined(_WIN32) && defined(SIJO_GAME_BUILD_DLL)
#define SIJO_API __declspec(dllexport)
#elif defined(_WIN32) && defined(SIJO_GAME_USE_DLL)
#define SIJO_API __declspec(dllimport)
#elif defined(__GNUC__)
#define SIJO_API __attribute__((visibility("default")))
#else
#define SIJO_API
#endif
#endif
#ifdef __cplusplus
extern "C" {
#endif

typedef struct SijoGame SijoGame;

#ifndef SIJO_GAME_DRAW_DEFINED
#define SIJO_GAME_DRAW_DEFINED
/* one draw command of the last client frame: a sprite (text == NULL) or a text label */
typedef struct SijoGameDraw {
    int32_t sprite;
    float x, y;
    const char* text; /* UTF-8, not NUL-terminated; valid until the next step */
    int32_t text_len; /* -1: no such draw command (index out of range) */
} SijoGameDraw;
#endif

SIJO_API SijoGame* GAMEAPI_create(uint64_t seed);        /* runs Game.Start (check GAMEAPI_error); NULL: out of memory */
SIJO_API int GAMEAPI_step(SijoGame* g);                  /* one tick: 1 ok, 0 stopped by a panic, -1 bad call */
SIJO_API const char* GAMEAPI_error(const SijoGame* g);   /* the panic message (with .sijo file:line), or NULL */
SIJO_API int32_t GAMEAPI_tick(const SijoGame* g);
SIJO_API int32_t GAMEAPI_draw_count(const SijoGame* g);
SIJO_API SijoGameDraw GAMEAPI_draw(const SijoGame* g, int32_t i);
SIJO_API uint64_t GAMEAPI_checksum(SijoGame* g);         /* layout-independent hash of the world state */
SIJO_API int GAMEAPI_save(SijoGame* g, const char* path); /* 1 on success */
SIJO_API int GAMEAPI_load(SijoGame* g, const char* path); /* 1: the world is replaced by the saved one (also
                                                            restarts a stopped game); 0: unchanged */
SIJO_API void GAMEAPI_set_threads(SijoGame* g, int n);   /* threads for parallel systems, 1..64 */
SIJO_API void GAMEAPI_set_panic_hook(void (*hook)(const char* msg, void* user), void* user); /* all games */
SIJO_API int GAMEAPI_main(int argc, char** argv);        /* the standalone command line: --server PORT,
                                                            --connect HOST:PORT, --ticks N ... */
SIJO_API void GAMEAPI_destroy(SijoGame* g);              /* frees everything the game owns; NULL is ignored */
