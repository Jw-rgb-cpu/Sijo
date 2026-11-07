/* sijo_crypto.h - XChaCha20-Poly1305 (draft-irtf-cfrg-xchacha) for the network layer.
 *
 * ChaCha20 and Poly1305 as in RFC 8439, HChaCha20 for the 192-bit nonce. Portable C: every word is
 * loaded and stored little-endian explicitly, Poly1305 uses 26-bit limbs (no 128-bit integers), and
 * the tag comparison takes the same time whatever the bytes. Self-contained (stdint/string only) so
 * tests can compile it on its own. */
#ifndef SIJO_CRYPTO_H
#define SIJO_CRYPTO_H
#include <stdint.h>
#ifndef SIJO_WASM
#include <string.h>
#endif

static inline uint32_t sijo_c_ld32(const uint8_t* p) { return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24); }
static inline void sijo_c_st32(uint8_t* p, uint32_t v) { p[0] = (uint8_t)v; p[1] = (uint8_t)(v >> 8); p[2] = (uint8_t)(v >> 16); p[3] = (uint8_t)(v >> 24); }
static inline void sijo_c_st64(uint8_t* p, uint64_t v) { sijo_c_st32(p, (uint32_t)v); sijo_c_st32(p + 4, (uint32_t)(v >> 32)); }
static inline uint32_t sijo_c_rotl(uint32_t x, int n) { return (x << n) | (x >> (32 - n)); }

#define SIJO_QR(a, b, c, d) \
    a += b; d ^= a; d = sijo_c_rotl(d, 16); \
    c += d; b ^= c; b = sijo_c_rotl(b, 12); \
    a += b; d ^= a; d = sijo_c_rotl(d, 8); \
    c += d; b ^= c; b = sijo_c_rotl(b, 7)

static void sijo_chacha_rounds(uint32_t x[16]) {
    for (int i = 0; i < 10; i++) {
        SIJO_QR(x[0], x[4], x[8], x[12]);
        SIJO_QR(x[1], x[5], x[9], x[13]);
        SIJO_QR(x[2], x[6], x[10], x[14]);
        SIJO_QR(x[3], x[7], x[11], x[15]);
        SIJO_QR(x[0], x[5], x[10], x[15]);
        SIJO_QR(x[1], x[6], x[11], x[12]);
        SIJO_QR(x[2], x[7], x[8], x[13]);
        SIJO_QR(x[3], x[4], x[9], x[14]);
    }
}
static void sijo_chacha_init(uint32_t s[16], const uint8_t key[32]) {
    s[0] = 0x61707865u; s[1] = 0x3320646eu; s[2] = 0x79622d32u; s[3] = 0x6b206574u; /* "expand 32-byte k" */
    for (int i = 0; i < 8; i++) s[4 + i] = sijo_c_ld32(key + 4 * i);
}
/* one 64-byte ChaCha20 block (RFC 8439 2.3: 32-bit counter, 96-bit nonce) */
static void sijo_chacha_block(const uint8_t key[32], uint32_t counter, const uint8_t nonce[12], uint8_t out[64]) {
    uint32_t s[16], x[16];
    sijo_chacha_init(s, key);
    s[12] = counter;
    for (int i = 0; i < 3; i++) s[13 + i] = sijo_c_ld32(nonce + 4 * i);
    memcpy(x, s, sizeof x);
    sijo_chacha_rounds(x);
    for (int i = 0; i < 16; i++) sijo_c_st32(out + 4 * i, x[i] + s[i]);
}
/* XOR `n` bytes with the keystream starting at block `counter` */
static void sijo_chacha_xor(const uint8_t key[32], uint32_t counter, const uint8_t nonce[12], uint8_t* d, size_t n) {
    uint8_t ks[64];
    while (n) {
        sijo_chacha_block(key, counter++, nonce, ks);
        size_t k = n < 64 ? n : 64;
        for (size_t i = 0; i < k; i++) d[i] ^= ks[i];
        d += k;
        n -= k;
    }
}
/* HChaCha20: a subkey from a key and the first 16 bytes of an extended nonce */
static void sijo_hchacha(const uint8_t key[32], const uint8_t n16[16], uint8_t out[32]) {
    uint32_t x[16];
    sijo_chacha_init(x, key);
    for (int i = 0; i < 4; i++) x[12 + i] = sijo_c_ld32(n16 + 4 * i);
    sijo_chacha_rounds(x);
    for (int i = 0; i < 4; i++) sijo_c_st32(out + 4 * i, x[i]);
    for (int i = 0; i < 4; i++) sijo_c_st32(out + 16 + 4 * i, x[12 + i]);
}

/* ---------------- Poly1305 (26-bit limbs) ---------------- */
typedef struct { uint32_t r[5], h[5], pad[4]; size_t left; uint8_t buf[16]; } SijoPoly;
static void sijo_poly_init(SijoPoly* p, const uint8_t key[32]) {
    uint32_t t0 = sijo_c_ld32(key), t1 = sijo_c_ld32(key + 4), t2 = sijo_c_ld32(key + 8), t3 = sijo_c_ld32(key + 12);
    p->r[0] = t0 & 0x3ffffff;
    p->r[1] = ((t0 >> 26) | (t1 << 6)) & 0x3ffff03;
    p->r[2] = ((t1 >> 20) | (t2 << 12)) & 0x3ffc0ff;
    p->r[3] = ((t2 >> 14) | (t3 << 18)) & 0x3f03fff;
    p->r[4] = (t3 >> 8) & 0x00fffff;
    for (int i = 0; i < 5; i++) p->h[i] = 0;
    for (int i = 0; i < 4; i++) p->pad[i] = sijo_c_ld32(key + 16 + 4 * i);
    p->left = 0;
}
static void sijo_poly_blocks(SijoPoly* p, const uint8_t* m, size_t n, uint32_t hibit) {
    const uint32_t r0 = p->r[0], r1 = p->r[1], r2 = p->r[2], r3 = p->r[3], r4 = p->r[4];
    const uint32_t s1 = r1 * 5, s2 = r2 * 5, s3 = r3 * 5, s4 = r4 * 5;
    uint32_t h0 = p->h[0], h1 = p->h[1], h2 = p->h[2], h3 = p->h[3], h4 = p->h[4];
    while (n >= 16) {
        h0 += sijo_c_ld32(m) & 0x3ffffff;
        h1 += (sijo_c_ld32(m + 3) >> 2) & 0x3ffffff;
        h2 += (sijo_c_ld32(m + 6) >> 4) & 0x3ffffff;
        h3 += (sijo_c_ld32(m + 9) >> 6) & 0x3ffffff;
        h4 += (sijo_c_ld32(m + 12) >> 8) | hibit;
        uint64_t d0 = (uint64_t)h0 * r0 + (uint64_t)h1 * s4 + (uint64_t)h2 * s3 + (uint64_t)h3 * s2 + (uint64_t)h4 * s1;
        uint64_t d1 = (uint64_t)h0 * r1 + (uint64_t)h1 * r0 + (uint64_t)h2 * s4 + (uint64_t)h3 * s3 + (uint64_t)h4 * s2;
        uint64_t d2 = (uint64_t)h0 * r2 + (uint64_t)h1 * r1 + (uint64_t)h2 * r0 + (uint64_t)h3 * s4 + (uint64_t)h4 * s3;
        uint64_t d3 = (uint64_t)h0 * r3 + (uint64_t)h1 * r2 + (uint64_t)h2 * r1 + (uint64_t)h3 * r0 + (uint64_t)h4 * s4;
        uint64_t d4 = (uint64_t)h0 * r4 + (uint64_t)h1 * r3 + (uint64_t)h2 * r2 + (uint64_t)h3 * r1 + (uint64_t)h4 * r0;
        uint32_t c = (uint32_t)(d0 >> 26); h0 = (uint32_t)d0 & 0x3ffffff;
        d1 += c; c = (uint32_t)(d1 >> 26); h1 = (uint32_t)d1 & 0x3ffffff;
        d2 += c; c = (uint32_t)(d2 >> 26); h2 = (uint32_t)d2 & 0x3ffffff;
        d3 += c; c = (uint32_t)(d3 >> 26); h3 = (uint32_t)d3 & 0x3ffffff;
        d4 += c; c = (uint32_t)(d4 >> 26); h4 = (uint32_t)d4 & 0x3ffffff;
        h0 += c * 5; c = h0 >> 26; h0 &= 0x3ffffff;
        h1 += c;
        m += 16;
        n -= 16;
    }
    p->h[0] = h0; p->h[1] = h1; p->h[2] = h2; p->h[3] = h3; p->h[4] = h4;
}
static void sijo_poly_update(SijoPoly* p, const uint8_t* m, size_t n) {
    if (p->left) {
        size_t k = 16 - p->left < n ? 16 - p->left : n;
        memcpy(p->buf + p->left, m, k);
        p->left += k; m += k; n -= k;
        if (p->left < 16) return;
        sijo_poly_blocks(p, p->buf, 16, 1u << 24);
        p->left = 0;
    }
    size_t full = n & ~(size_t)15;
    if (full) sijo_poly_blocks(p, m, full, 1u << 24);
    m += full; n -= full;
    if (n) { memcpy(p->buf, m, n); p->left = n; }
}
static void sijo_poly_finish(SijoPoly* p, uint8_t tag[16]) {
    if (p->left) {
        p->buf[p->left] = 1;
        for (size_t i = p->left + 1; i < 16; i++) p->buf[i] = 0;
        sijo_poly_blocks(p, p->buf, 16, 0);
    }
    uint32_t h0 = p->h[0], h1 = p->h[1], h2 = p->h[2], h3 = p->h[3], h4 = p->h[4], c;
    c = h1 >> 26; h1 &= 0x3ffffff; h2 += c;
    c = h2 >> 26; h2 &= 0x3ffffff; h3 += c;
    c = h3 >> 26; h3 &= 0x3ffffff; h4 += c;
    c = h4 >> 26; h4 &= 0x3ffffff; h0 += c * 5;
    c = h0 >> 26; h0 &= 0x3ffffff; h1 += c;
    uint32_t g0 = h0 + 5; c = g0 >> 26; g0 &= 0x3ffffff;
    uint32_t g1 = h1 + c; c = g1 >> 26; g1 &= 0x3ffffff;
    uint32_t g2 = h2 + c; c = g2 >> 26; g2 &= 0x3ffffff;
    uint32_t g3 = h3 + c; c = g3 >> 26; g3 &= 0x3ffffff;
    uint32_t g4 = h4 + c - (1u << 26);
    uint32_t mask = (g4 >> 31) - 1u; /* all ones when h >= p: use g = h - p */
    h0 = (h0 & ~mask) | (g0 & mask);
    h1 = (h1 & ~mask) | (g1 & mask);
    h2 = (h2 & ~mask) | (g2 & mask);
    h3 = (h3 & ~mask) | (g3 & mask);
    h4 = (h4 & ~mask) | (g4 & mask);
    h0 = h0 | (h1 << 26);
    h1 = (h1 >> 6) | (h2 << 20);
    h2 = (h2 >> 12) | (h3 << 14);
    h3 = (h3 >> 18) | (h4 << 8);
    uint64_t f = (uint64_t)h0 + p->pad[0]; h0 = (uint32_t)f;
    f = (uint64_t)h1 + p->pad[1] + (f >> 32); h1 = (uint32_t)f;
    f = (uint64_t)h2 + p->pad[2] + (f >> 32); h2 = (uint32_t)f;
    f = (uint64_t)h3 + p->pad[3] + (f >> 32); h3 = (uint32_t)f;
    sijo_c_st32(tag, h0); sijo_c_st32(tag + 4, h1); sijo_c_st32(tag + 8, h2); sijo_c_st32(tag + 12, h3);
}

/* ---------------- AEAD ---------------- */
static void sijo_aead_tag(const uint8_t key[32], const uint8_t nonce[12], const uint8_t* aad, size_t na, const uint8_t* ct, size_t nc, uint8_t tag[16]) {
    uint8_t block[64];
    static const uint8_t zero[16] = {0};
    sijo_chacha_block(key, 0, nonce, block); /* the one-time Poly1305 key */
    SijoPoly p;
    sijo_poly_init(&p, block);
    sijo_poly_update(&p, aad, na);
    if (na & 15) sijo_poly_update(&p, zero, 16 - (na & 15));
    sijo_poly_update(&p, ct, nc);
    if (nc & 15) sijo_poly_update(&p, zero, 16 - (nc & 15));
    uint8_t lens[16];
    sijo_c_st64(lens, (uint64_t)na);
    sijo_c_st64(lens + 8, (uint64_t)nc);
    sijo_poly_update(&p, lens, 16);
    sijo_poly_finish(&p, tag);
}
/* RFC 8439 AEAD_CHACHA20_POLY1305: encrypts d[0..n) in place, writes the 16-byte tag */
static void sijo_aead_seal(const uint8_t key[32], const uint8_t nonce[12], const uint8_t* aad, size_t na, uint8_t* d, size_t n, uint8_t tag[16]) {
    sijo_chacha_xor(key, 1, nonce, d, n);
    sijo_aead_tag(key, nonce, aad, na, d, n, tag);
}
/* decrypts in place when the tag is right; returns 0 (and leaves d as it was) when it is not */
static int sijo_aead_open(const uint8_t key[32], const uint8_t nonce[12], const uint8_t* aad, size_t na, uint8_t* d, size_t n, const uint8_t tag[16]) {
    uint8_t want[16];
    sijo_aead_tag(key, nonce, aad, na, d, n, want);
    uint32_t diff = 0;
    for (int i = 0; i < 16; i++) diff |= (uint32_t)(want[i] ^ tag[i]);
    if (diff) return 0;
    sijo_chacha_xor(key, 1, nonce, d, n);
    return 1;
}
/* XChaCha20-Poly1305: a 24-byte nonce (random nonces are safe) */
static void sijo_xaead_key(const uint8_t key[32], const uint8_t nonce[24], uint8_t sub[32], uint8_t n12[12]) {
    sijo_hchacha(key, nonce, sub);
    memset(n12, 0, 4);
    memcpy(n12 + 4, nonce + 16, 8);
}
static void sijo_xaead_seal(const uint8_t key[32], const uint8_t nonce[24], const uint8_t* aad, size_t na, uint8_t* d, size_t n, uint8_t tag[16]) {
    uint8_t sub[32], n12[12];
    sijo_xaead_key(key, nonce, sub, n12);
    sijo_aead_seal(sub, n12, aad, na, d, n, tag);
}
static int sijo_xaead_open(const uint8_t key[32], const uint8_t nonce[24], const uint8_t* aad, size_t na, uint8_t* d, size_t n, const uint8_t tag[16]) {
    uint8_t sub[32], n12[12];
    sijo_xaead_key(key, nonce, sub, n12);
    return sijo_aead_open(sub, n12, aad, na, d, n, tag);
}
#endif
