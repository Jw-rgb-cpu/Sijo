/* sijo_compat.h — reproduces the Sijo runtime's "world checksum" for workloads
 * whose rows never move (no Destroy), so the baselines can be compared with the
 * Sijo build bit for bit (in addition to the layout-independent "bench" lines).
 *
 * The runtime hashes, per archetype and row: the handle (idx, gen) and then every
 * stored field in component order, each as 4 little-endian bytes (FNV-1a 64).
 * With spawn order == row order and no destroys, row i has handle {i + 1, 1}.
 */
#ifndef SIJO_COMPAT_H
#define SIJO_COMPAT_H

#include <stdint.h>
#include <string.h>

static inline uint64_t nc_fnv(uint64_t h, const void* p, size_t n) {
    const uint8_t* b = (const uint8_t*)p;
    for (size_t i = 0; i < n; i++) {
        h ^= b[i];
        h *= 0x100000001b3ull;
    }
    return h;
}
static inline uint64_t nc_begin(void) { return 0xcbf29ce484222325ull; }
static inline uint64_t nc_handle(uint64_t h, int32_t row) {
    uint32_t idx = (uint32_t)row + 1u, gen = 1u;
    h = nc_fnv(h, &idx, 4);
    return nc_fnv(h, &gen, 4);
}
static inline uint64_t nc_f(uint64_t h, float x) { return nc_fnv(h, &x, 4); }
static inline uint64_t nc_i(uint64_t h, int32_t x) { return nc_fnv(h, &x, 4); }

#endif
