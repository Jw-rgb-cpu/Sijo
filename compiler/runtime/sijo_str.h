/* sijo_str.h - immutable strings for Sijo.
 *
 * A string is a value {pointer, byte length, kind}. Text is UTF-8; lengths and indices seen by
 * programs count Unicode code points (equal to C#'s UTF-16 units for all BMP text, e.g. Hangul).
 *
 * Memory: every string produced at run time (concatenation, interpolation, Substring, ...) is
 * bump-allocated in the thread's frame arena, so creating temporaries costs no malloc/free and
 * needs no locks inside parallel systems. Strings stored in the game state survive because the
 * compiler generates a precise root visitor (every string slot in every archetype, struct and
 * list): at the end of each tick, before the frame arena is reset, frame strings that are still
 * referenced are copied to the string heap ("promotion"), heap strings that are referenced get
 * marked, and unmarked heap strings are freed. Collection only happens between ticks, where no
 * locals exist, so there are no stack maps, no write barriers and no reference counting.
 *
 * `k` is the storage kind (low two bits) plus SIJO_STR_ASCII when the text is known to be all
 * ASCII: then Length, s[i] and Substring index bytes directly (O(1)) instead of counting code points.
 */

enum { SIJO_STR_STATIC = 0, SIJO_STR_HEAP = 1, SIJO_STR_FRAME = 2, SIJO_STR_ASCII = 4 };
typedef struct { const char* p; int32_t n; int32_t k; } SijoStr;
#define SIJO_STR_EMPTY ((SijoStr){"", 0, SIJO_STR_STATIC | SIJO_STR_ASCII})
#define SIJO_SK(s) ((s).k & 3)
static inline int sijo_u8_ascii(const char* p, int32_t n) {
    unsigned char acc = 0;
    for (int32_t i = 0; i < n; i++) acc |= (unsigned char)p[i];
    return acc < 0x80;
}

typedef struct { uint32_t mark; int32_t n; } SijoSHdr;
typedef struct {
    SijoSHdr** v;
    int32_t n, cap;
    uint32_t epoch;
    size_t bytes, peak_bytes;
    int64_t promoted, freed;
    /* frame pointer -> promoted copy, so one frame string stored in many slots is copied once */
    const char** fk;
    SijoSHdr** fv;
    int32_t fcap, fn;
} SijoSHeap;

static inline SijoStr sijo_str_lit(const char* p, int32_t n) { SijoStr s = {p, n, SIJO_STR_STATIC}; return s; }
/* a literal the compiler knows to be ASCII */
static inline SijoStr sijo_str_lita(const char* p, int32_t n) { SijoStr s = {p, n, SIJO_STR_STATIC | SIJO_STR_ASCII}; return s; }

static SijoStr sijo_str_frame(SijoArena* a, const char* p, int32_t n) {
    if (n <= 0) return SIJO_STR_EMPTY;
    char* d = (char*)sijo_arena_alloc(a, (size_t)n + 1);
    memcpy(d, p, (size_t)n);
    d[n] = 0;
    SijoStr s = {d, n, SIJO_STR_FRAME | (sijo_u8_ascii(d, n) ? SIJO_STR_ASCII : 0)};
    a->str_end = d + n; /* the newest string: it may grow in place (sijo_str_cat) */
    a->str_used = a->head->used;
    return s;
}

static SijoSHdr* sijo_sheap_alloc(SijoSHeap* h, const char* p, int32_t n) {
    SijoSHdr* hd = (SijoSHdr*)sijo_xrealloc(NULL, sizeof(SijoSHdr) + (size_t)n + 1);
    hd->mark = h->epoch;
    hd->n = n;
    memcpy(hd + 1, p, (size_t)n);
    ((char*)(hd + 1))[n] = 0;
    if (h->n == h->cap) {
        h->cap = h->cap ? h->cap * 2 : 256;
        h->v = (SijoSHdr**)sijo_xrealloc(h->v, (size_t)h->cap * sizeof(SijoSHdr*));
    }
    h->v[h->n++] = hd;
    h->bytes += (size_t)n + 1 + sizeof(SijoSHdr);
    if (h->bytes > h->peak_bytes) h->peak_bytes = h->bytes;
    return hd;
}

/* a heap copy (used when loading state or decoding network data) */
static SijoStr sijo_str_heap(SijoSHeap* h, const char* p, int32_t n) {
    if (n <= 0) return SIJO_STR_EMPTY;
    SijoSHdr* hd = sijo_sheap_alloc(h, p, n);
    SijoStr s = {(const char*)(hd + 1), n, SIJO_STR_HEAP | (sijo_u8_ascii(p, n) ? SIJO_STR_ASCII : 0)};
    return s;
}

static void sijo_sheap_free(SijoSHeap* h) {
    for (int32_t i = 0; i < h->n; i++) free(h->v[i]);
    free(h->v);
    free(h->fk);
    free(h->fv);
    memset(h, 0, sizeof *h);
}

static void sijo_gc_begin(SijoSHeap* h) {
    h->epoch++;
    if (h->epoch == 0) h->epoch = 1;
    h->fn = 0;
    if (h->fcap) memset(h->fk, 0, (size_t)h->fcap * sizeof(char*));
}

/* root visitor: promote frame strings, mark heap strings */
static void sijo_str_visit(SijoSHeap* h, SijoStr* s) {
    if (SIJO_SK(*s) == SIJO_STR_HEAP) {
        ((SijoSHdr*)s->p - 1)->mark = h->epoch;
    } else if (SIJO_SK(*s) == SIJO_STR_FRAME) {
        if (h->fn * 2 >= h->fcap) { /* grow the promotion table */
            int32_t oc = h->fcap;
            const char** ok = h->fk;
            SijoSHdr** ov = h->fv;
            h->fcap = oc ? oc * 2 : 256;
            h->fk = (const char**)calloc((size_t)h->fcap, sizeof(char*));
            h->fv = (SijoSHdr**)calloc((size_t)h->fcap, sizeof(SijoSHdr*));
            if (!h->fk || !h->fv) sijo_panic("out of memory");
            for (int32_t i = 0; i < oc; i++) {
                if (!ok[i]) continue;
                uint32_t j = (uint32_t)(((uintptr_t)ok[i] >> 4) * 2654435761u) & (uint32_t)(h->fcap - 1);
                while (h->fk[j]) j = (j + 1) & (uint32_t)(h->fcap - 1);
                h->fk[j] = ok[i];
                h->fv[j] = ov[i];
            }
            free(ok);
            free(ov);
        }
        uint32_t j = (uint32_t)(((uintptr_t)s->p >> 4) * 2654435761u) & (uint32_t)(h->fcap - 1);
        while (h->fk[j] && !(h->fk[j] == s->p && h->fv[j]->n == s->n)) j = (j + 1) & (uint32_t)(h->fcap - 1);
        SijoSHdr* hd;
        if (h->fk[j]) {
            hd = h->fv[j];
        } else {
            hd = sijo_sheap_alloc(h, s->p, s->n);
            h->fk[j] = s->p;
            h->fv[j] = hd;
            h->fn++;
            h->promoted++;
        }
        s->p = (const char*)(hd + 1);
        s->k = SIJO_STR_HEAP | (s->k & SIJO_STR_ASCII);
    }
}

static void sijo_gc_sweep(SijoSHeap* h) {
    int32_t j = 0;
    for (int32_t i = 0; i < h->n; i++) {
        SijoSHdr* hd = h->v[i];
        if (hd->mark == h->epoch) {
            h->v[j++] = hd;
        } else {
            h->bytes -= (size_t)hd->n + 1 + sizeof(SijoSHdr);
            h->freed++;
            free(hd);
        }
    }
    h->n = j;
}

/* ---------------- operations (results live in the frame arena) ---------------- */
static inline bool sijo_str_eq(SijoStr a, SijoStr b) { return a.n == b.n && (a.p == b.p || memcmp(a.p, b.p, (size_t)a.n) == 0); }
static inline int32_t sijo_str_cmp(SijoStr a, SijoStr b) {
    int32_t m = a.n < b.n ? a.n : b.n;
    int c = memcmp(a.p, b.p, (size_t)m);
    if (c) return c < 0 ? -1 : 1;
    return a.n < b.n ? -1 : (a.n > b.n ? 1 : 0);
}
static int sijo_cmp_str(const void* a, const void* b) { return sijo_str_cmp(*(const SijoStr*)a, *(const SijoStr*)b); }
static inline uint64_t sijo_str_hash(SijoStr s) { return sijo_fnv(0xcbf29ce484222325ull, s.p, (size_t)s.n); }

/* `s += t` grows s in place when s is the newest frame string and nothing was allocated since:
 * a string-building loop then uses linear memory instead of leaving one copy per iteration until
 * the end of the tick. The arena remembers where that string ends exactly (a->str_end), so a
 * string that shares the start of a grown allocation (t = s + "x"; s += "y") is never extended
 * over bytes another string uses. Nothing reads past a string's length. */
static inline size_t sijo_round16(size_t n) { return (n + 15) & ~(size_t)15; }
static SijoStr sijo_str_cat(SijoArena* a, SijoStr x, SijoStr y) {
    if (x.n == 0) return y;
    if (y.n == 0) return x;
    SijoChunk* h = a ? a->head : NULL;
    int32_t asc = x.k & y.k & SIJO_STR_ASCII;
    if (h && SIJO_SK(x) == SIJO_STR_FRAME && x.p + x.n == a->str_end && h->used == a->str_used) {
        uint8_t* base = (uint8_t*)(h + 1);
        size_t off = (size_t)((const uint8_t*)x.p - base);
        size_t need = off + sijo_round16((size_t)x.n + (size_t)y.n + 1);
        if ((const uint8_t*)x.p >= base && off < h->cap && need <= h->cap) {
            memmove((char*)x.p + x.n, y.p, (size_t)y.n);
            ((char*)x.p)[x.n + y.n] = 0;
            if (need > h->used) {
                a->cur += need - h->used;
                if (a->cur > a->peak) a->peak = a->cur;
                h->used = need;
            }
            SijoStr s = {x.p, x.n + y.n, SIJO_STR_FRAME | asc};
            a->str_end = s.p + s.n;
            a->str_used = h->used;
            return s;
        }
    }
    char* d = (char*)sijo_arena_alloc(a, (size_t)x.n + (size_t)y.n + 1);
    memcpy(d, x.p, (size_t)x.n);
    memcpy(d + x.n, y.p, (size_t)y.n);
    d[x.n + y.n] = 0;
    SijoStr s = {d, x.n + y.n, SIJO_STR_FRAME | asc};
    a->str_end = d + s.n;
    a->str_used = a->head->used;
    return s;
}
/* s += <formatted number> without a temporary string (the text goes through a stack buffer) */
static SijoStr sijo_str_cat_i32(SijoArena* a, SijoStr x, int32_t v, int kind, int prec) {
    char t[SIJO_FMT_MAX];
    SijoStr y = {t, sijo_fmt_i32(t, v, kind, prec), SIJO_STR_STATIC | SIJO_STR_ASCII};
    if (x.n == 0) return sijo_str_frame(a, y.p, y.n);
    return sijo_str_cat(a, x, y);
}
static SijoStr sijo_str_cat_f32(SijoArena* a, SijoStr x, float v, int kind, int prec) {
    char t[SIJO_FMT_MAX];
    SijoStr y = {t, sijo_fmt_f32(t, v, kind, prec), SIJO_STR_STATIC | SIJO_STR_ASCII};
    if (x.n == 0) return sijo_str_frame(a, y.p, y.n);
    return sijo_str_cat(a, x, y);
}

/* code point helpers (UTF-8) */
static inline int32_t sijo_u8_count(const char* p, int32_t n) {
    int32_t c = 0;
    for (int32_t i = 0; i < n; i++) c += ((unsigned char)p[i] & 0xC0) != 0x80;
    return c;
}
/* byte offset of code point `cp` (n if past the end) */
static inline int32_t sijo_u8_offset(const char* p, int32_t n, int32_t cp) {
    int32_t i = 0;
    while (i < n && cp > 0) {
        i++;
        while (i < n && ((unsigned char)p[i] & 0xC0) == 0x80) i++;
        cp--;
    }
    return cp > 0 ? -1 : i;
}
static inline int32_t sijo_str_len(SijoStr s) { return (s.k & SIJO_STR_ASCII) ? s.n : sijo_u8_count(s.p, s.n); }
static inline int32_t sijo_str_off(SijoStr s, int32_t cp) { return (s.k & SIJO_STR_ASCII) ? (cp <= s.n ? cp : -1) : sijo_u8_offset(s.p, s.n, cp); }

/* a frame copy of bytes [b, e) of s (a substring of ASCII text is ASCII) */
static SijoStr sijo_str_slice(SijoArena* a, SijoStr s, int32_t b, int32_t e) {
    if (b == 0 && e == s.n) return s;
    if (e <= b) return SIJO_STR_EMPTY;
    char* d = (char*)sijo_arena_alloc(a, (size_t)(e - b) + 1);
    memcpy(d, s.p + b, (size_t)(e - b));
    d[e - b] = 0;
    SijoStr r = {d, e - b, SIJO_STR_FRAME | ((s.k & SIJO_STR_ASCII) || sijo_u8_ascii(d, e - b) ? SIJO_STR_ASCII : 0)};
    a->str_end = d + r.n;
    a->str_used = a->head->used;
    return r;
}
static SijoStr sijo_str_sub(SijoArena* a, SijoStr s, int32_t start, int32_t len) {
    int32_t total = sijo_str_len(s);
    if (start < 0 || len < 0 || start > total || len > total - start) sijo_panic("Substring: index and length must refer to a location within the string");
    int32_t b0 = sijo_str_off(s, start);
    int32_t b1 = sijo_str_off(s, start + len);
    return sijo_str_slice(a, s, b0, b1);
}
static SijoStr sijo_str_from(SijoArena* a, SijoStr s, int32_t start) {
    int32_t total = sijo_str_len(s);
    return sijo_str_sub(a, s, start, total - start);
}
static int32_t sijo_str_find(SijoStr s, SijoStr t) {
    if (t.n == 0) return 0;
    for (int32_t i = 0; i + t.n <= s.n; i++)
        if (memcmp(s.p + i, t.p, (size_t)t.n) == 0) return i;
    return -1;
}
static inline int32_t sijo_str_index_of(SijoStr s, SijoStr t) {
    int32_t b = sijo_str_find(s, t);
    return b < 0 ? -1 : ((s.k & SIJO_STR_ASCII) ? b : sijo_u8_count(s.p, b));
}
static int32_t sijo_str_last_index_of(SijoStr s, SijoStr t) {
    if (t.n == 0) return sijo_str_len(s) > 0 ? sijo_str_len(s) - 1 : 0; /* .NET: the last index, 0 for "" */
    for (int32_t i = s.n - t.n; i >= 0; i--)
        if (memcmp(s.p + i, t.p, (size_t)t.n) == 0) return (s.k & SIJO_STR_ASCII) ? i : sijo_u8_count(s.p, i);
    return -1;
}
static inline bool sijo_str_contains(SijoStr s, SijoStr t) { return sijo_str_find(s, t) >= 0; }
static inline bool sijo_str_starts(SijoStr s, SijoStr t) { return t.n <= s.n && memcmp(s.p, t.p, (size_t)t.n) == 0; }
static inline bool sijo_str_ends(SijoStr s, SijoStr t) { return t.n <= s.n && memcmp(s.p + s.n - t.n, t.p, (size_t)t.n) == 0; }

/* ---------------- characters (Unicode code points; tables from src/uchar.rs) ---------------- */
static int sijo_uc_in(const uint32_t (*t)[2], int n, uint32_t c) {
    int lo = 0, hi = n - 1;
    while (lo <= hi) {
        int m = (lo + hi) / 2;
        if (c < t[m][0]) hi = m - 1;
        else if (c > t[m][1]) lo = m + 1;
        else return 1;
    }
    return 0;
}
static inline bool sijo_ch_is_letter(int32_t c) { return c < 128 ? ((c | 32) >= 'a' && (c | 32) <= 'z') : sijo_uc_in(sijo_uc_letter, SIJO_UC_LETTER_N, (uint32_t)c); }
static inline bool sijo_ch_is_digit(int32_t c) { return c < 128 ? (c >= '0' && c <= '9') : sijo_uc_in(sijo_uc_digit, SIJO_UC_DIGIT_N, (uint32_t)c); }
static inline bool sijo_ch_is_space(int32_t c) { return c < 128 ? (c == ' ' || (c >= 9 && c <= 13)) : sijo_uc_in(sijo_uc_space, SIJO_UC_SPACE_N, (uint32_t)c); }
static inline bool sijo_ch_is_punct(int32_t c) { return sijo_uc_in(sijo_uc_punct, SIJO_UC_PUNCT_N, (uint32_t)c); }
static int32_t sijo_ch_upper(int32_t c) {
    if (c < 128) return (c >= 'a' && c <= 'z') ? c - 32 : c;
    for (int i = 0; i < SIJO_UC_CASE_N; i++) {
        const int32_t* e = sijo_uc_case[i];
        if (c >= e[0] && c <= e[1] && (c - e[0]) % e[3] == 0) return c + e[2];
    }
    return c;
}
static int32_t sijo_ch_lower(int32_t c) {
    if (c < 128) return (c >= 'A' && c <= 'Z') ? c + 32 : c;
    for (int i = 0; i < SIJO_UC_CASE_N; i++) {
        const int32_t* e = sijo_uc_case[i];
        int32_t l = c - e[2];
        if (e[4] && l >= e[0] && l <= e[1] && (l - e[0]) % e[3] == 0) return l;
    }
    return c;
}
static inline bool sijo_ch_is_upper(int32_t c) { return sijo_ch_lower(c) != c; }
static inline bool sijo_ch_is_lower(int32_t c) { return sijo_ch_upper(c) != c || sijo_uc_in(sijo_uc_loweronly, SIJO_UC_LOWERONLY_N, (uint32_t)c); }
/* (char)n: code points stay, other values wrap to 16 bits (C# unchecked); --checked: panic */
static inline int32_t sijo_to_char(int32_t v) {
#ifdef SIJO_CHECKED
    if (v < 0 || v > 0x10FFFF) sijo_panic("arithmetic overflow (checked): value out of range for char");
#endif
    return (v >= 0 && v <= 0x10FFFF) ? v : (v & 0xFFFF);
}
/* UTF-8; a surrogate half or an invalid value is written as U+FFFD */
static inline int sijo_u8_enc(char* d, int32_t c) {
    if (c < 0 || c > 0x10FFFF || (c >= 0xD800 && c <= 0xDFFF)) c = 0xFFFD;
    if (c < 0x80) { d[0] = (char)c; return 1; }
    if (c < 0x800) { d[0] = (char)(0xC0 | (c >> 6)); d[1] = (char)(0x80 | (c & 0x3F)); return 2; }
    if (c < 0x10000) { d[0] = (char)(0xE0 | (c >> 12)); d[1] = (char)(0x80 | ((c >> 6) & 0x3F)); d[2] = (char)(0x80 | (c & 0x3F)); return 3; }
    d[0] = (char)(0xF0 | (c >> 18)); d[1] = (char)(0x80 | ((c >> 12) & 0x3F)); d[2] = (char)(0x80 | ((c >> 6) & 0x3F)); d[3] = (char)(0x80 | (c & 0x3F));
    return 4;
}
/* the UTF-8 length of the character starting at p[0] (clamped to n; a stray byte is one char) */
static inline int32_t sijo_u8_len(const char* p, int32_t n) {
    unsigned char b = (unsigned char)p[0];
    int32_t k = b < 0xC0 ? 1 : b < 0xE0 ? 2 : b < 0xF0 ? 3 : 4;
    return k > n ? n : k;
}
static inline int32_t sijo_u8_dec(const char* p, int32_t n, int32_t* len) {
    unsigned char b = (unsigned char)p[0];
    int32_t k = sijo_u8_len(p, n);
    *len = k;
    if (b < 0x80) return b;
    if (k == 1 || b < 0xC0) return 0xFFFD;
    int32_t c = k == 2 ? (b & 0x1F) : k == 3 ? (b & 0x0F) : (b & 0x07);
    for (int32_t i = 1; i < k; i++) c = (c << 6) | ((unsigned char)p[i] & 0x3F);
    return c;
}
/* s[i] */
static int32_t sijo_str_char_at(SijoStr s, int32_t i) {
    if (s.k & SIJO_STR_ASCII) {
        if ((uint32_t)i >= (uint32_t)s.n) sijo_panic("index was outside the bounds of the string");
        return (unsigned char)s.p[i];
    }
    int32_t b = i < 0 ? -1 : sijo_u8_offset(s.p, s.n, i);
    if (b < 0 || b >= s.n) sijo_panic("index was outside the bounds of the string");
    int32_t len;
    return sijo_u8_dec(s.p + b, s.n - b, &len);
}
/* foreach (char c in s): the character at byte offset b, and its length in bytes */
static inline int32_t sijo_str_char_at_byte(SijoStr s, int32_t b) {
    int32_t len;
    return sijo_u8_dec(s.p + b, s.n - b, &len);
}
static inline int32_t sijo_str_char_bytes(SijoStr s, int32_t b) { return sijo_u8_len(s.p + b, s.n - b); }
static SijoStr sijo_str_of_char(SijoArena* a, int32_t c) {
    if (c >= 0 && c < 128) {
        SijoStr s = {sijo_uc_ascii1 + 2 * c, 1, SIJO_STR_STATIC | SIJO_STR_ASCII};
        return s;
    }
    char t[4];
    int n = sijo_u8_enc(t, c);
    return sijo_str_frame(a, t, n);
}
/* a char as a NUL-terminated C string (Print) */
static inline const char* sijo_char_utf8(char* t, int32_t c) {
    int n = sijo_u8_enc(t, c);
    t[n] = 0;
    return t;
}
/* new string(c, n) */
static SijoStr sijo_str_repeat(SijoArena* a, int32_t c, int32_t n) {
    if (n < 0) sijo_panic("new string: the count must not be negative");
    if (n == 0) return SIJO_STR_EMPTY;
    char t[4];
    int k = sijo_u8_enc(t, c);
    int64_t total = (int64_t)k * n;
    if (total > 0x7ffffff0) sijo_panic("new string: the string would be too long");
    char* d = (char*)sijo_arena_alloc(a, (size_t)total + 1);
    for (int32_t i = 0; i < n; i++) memcpy(d + (int64_t)i * k, t, (size_t)k);
    d[total] = 0;
    SijoStr s = {d, (int32_t)total, SIJO_STR_FRAME | (k == 1 && t[0] >= 0 ? SIJO_STR_ASCII : 0)};
    a->str_end = d + total;
    a->str_used = a->head->used;
    return s;
}

/* ToUpper/ToLower (invariant culture, tables from src/uchar.rs) */
static SijoStr sijo_str_case(SijoArena* a, SijoStr s, int upper) {
    if (s.n == 0) return s;
    if ((s.k & SIJO_STR_ASCII) || sijo_u8_ascii(s.p, s.n)) {
        char* d = (char*)sijo_arena_alloc(a, (size_t)s.n + 1);
        for (int32_t i = 0; i < s.n; i++) {
            char c = s.p[i];
            if (upper && c >= 'a' && c <= 'z') c = (char)(c - 32);
            if (!upper && c >= 'A' && c <= 'Z') c = (char)(c + 32);
            d[i] = c;
        }
        d[s.n] = 0;
        SijoStr r = {d, s.n, SIJO_STR_FRAME | SIJO_STR_ASCII};
        return r;
    }
    char* d = (char*)sijo_arena_alloc(a, (size_t)s.n * 2 + 1); /* a case pair never needs more than twice the bytes */
    int32_t o = 0;
    for (int32_t i = 0; i < s.n;) {
        int32_t len;
        int32_t c = sijo_u8_dec(s.p + i, s.n - i, &len);
        i += len;
        o += sijo_u8_enc(d + o, upper ? sijo_ch_upper(c) : sijo_ch_lower(c));
    }
    d[o] = 0;
    SijoStr r = {d, o, SIJO_STR_FRAME};
    return r;
}
/* Trim(): white space as char.IsWhiteSpace, at both ends */
static SijoStr sijo_str_trim(SijoArena* a, SijoStr s) {
    int32_t b = 0, e = s.n;
    while (b < e) {
        int32_t len;
        int32_t c = sijo_u8_dec(s.p + b, e - b, &len);
        if (!sijo_ch_is_space(c)) break;
        b += len;
    }
    while (e > b) {
        int32_t q = e - 1;
        while (q > b && ((unsigned char)s.p[q] & 0xC0) == 0x80) q--;
        int32_t len;
        int32_t c = sijo_u8_dec(s.p + q, e - q, &len);
        if (!sijo_ch_is_space(c)) break;
        e = q;
    }
    if (b == 0 && e == s.n) return s;
    (void)a;
    SijoStr r = {s.p + b, e - b, s.k};
    /* heap and frame strings are only referenced from their start (collector, in-place append) */
    if (SIJO_SK(s) != SIJO_STR_STATIC) return sijo_str_frame(a, s.p + b, e - b);
    return r;
}
/* PadLeft / PadRight */
static SijoStr sijo_str_pad(SijoArena* a, SijoStr s, int32_t width, int32_t c, int left) {
    if (width < 0) sijo_panic("PadLeft/PadRight: the total width must not be negative");
    int32_t len = sijo_str_len(s);
    if (width <= len) return s;
    SijoStr p = sijo_str_repeat(a, c, width - len);
    return left ? sijo_str_cat(a, p, s) : sijo_str_cat(a, s, p);
}
/* Split(sep): the pieces as a frame list (the caller copies it to where the result lives) */
static SijoList sijo_str_split(SijoArena* a, SijoStr s, SijoStr sep) {
    SijoList l = sijo_list_new(a, 0, (int32_t)sizeof(SijoStr), 4);
    if (sep.n == 0) {
        *(SijoStr*)sijo_list_push(a, &l, (int32_t)sizeof(SijoStr)) = s;
        return l;
    }
    int32_t start = 0;
    for (int32_t i = 0; i + sep.n <= s.n;) {
        if (memcmp(s.p + i, sep.p, (size_t)sep.n) == 0) {
            SijoStr piece = sijo_str_slice(a, s, start, i);
            *(SijoStr*)sijo_list_push(a, &l, (int32_t)sizeof(SijoStr)) = piece;
            i += sep.n;
            start = i;
        } else {
            i++;
        }
    }
    SijoStr last = sijo_str_slice(a, s, start, s.n);
    *(SijoStr*)sijo_list_push(a, &l, (int32_t)sizeof(SijoStr)) = last;
    return l;
}
static SijoStr sijo_str_replace(SijoArena* a, SijoStr s, SijoStr from, SijoStr to) {
    if (from.n == 0) sijo_panic("Replace: the string to replace must not be empty");
    int32_t cnt = 0;
    for (int32_t i = 0; i + from.n <= s.n;) {
        if (memcmp(s.p + i, from.p, (size_t)from.n) == 0) { cnt++; i += from.n; } else i++;
    }
    if (cnt == 0) return s;
    int32_t n = s.n + cnt * (to.n - from.n);
    char* d = (char*)sijo_arena_alloc(a, (size_t)n + 1);
    int32_t o = 0;
    for (int32_t i = 0; i < s.n;) {
        if (i + from.n <= s.n && memcmp(s.p + i, from.p, (size_t)from.n) == 0) {
            memcpy(d + o, to.p, (size_t)to.n);
            o += to.n;
            i += from.n;
        } else {
            d[o++] = s.p[i++];
        }
    }
    d[n] = 0;
    SijoStr r = {d, n, SIJO_STR_FRAME | (s.k & to.k & SIJO_STR_ASCII)};
    return r;
}

/* ---------------- formatting: a small builder in the frame arena ---------------- */
typedef struct { SijoArena* a; char* p; int32_t n, cap; } SijoSB;
static inline SijoSB sijo_sb(SijoArena* a) { SijoSB b = {a, NULL, 0, 0}; return b; }
static void sijo_sb_put(SijoSB* b, const char* p, int32_t n) {
    if (n <= 0) return;
    if (b->n + n + 1 > b->cap) {
        int32_t nc = b->cap ? b->cap * 2 : 64;
        while (nc < b->n + n + 1) nc *= 2;
        char* d = (char*)sijo_arena_alloc(b->a, (size_t)nc);
        if (b->n) memcpy(d, b->p, (size_t)b->n);
        b->p = d;
        b->cap = nc;
    }
    memcpy(b->p + b->n, p, (size_t)n);
    b->n += n;
}
static inline void sijo_sb_cstr(SijoSB* b, const char* s) { sijo_sb_put(b, s, (int32_t)strlen(s)); }
static inline void sijo_sb_str(SijoSB* b, SijoStr s) { sijo_sb_put(b, s.p, s.n); }
static inline void sijo_sb_char(SijoSB* b, int32_t c) {
    char t[4];
    int n = sijo_u8_enc(t, c);
    sijo_sb_put(b, t, n);
}
static inline void sijo_sb_int(SijoSB* b, int32_t v) {
    char t[16];
    int n = sijo_fmt_i32(t, v, 0, -1);
    sijo_sb_put(b, t, n);
}
static inline void sijo_sb_float(SijoSB* b, float v) {
    char t[32];
    int n = sijo_fmt_f32(t, v, 0, -1);
    sijo_sb_put(b, t, n);
}
static inline void sijo_sb_fmt_i32(SijoSB* b, int32_t v, int kind, int prec) { /* $"{v:D5}" ... */
    char t[SIJO_FMT_MAX];
    int n = sijo_fmt_i32(t, v, kind, prec);
    sijo_sb_put(b, t, n);
}
static inline void sijo_sb_fmt_f32(SijoSB* b, float v, int kind, int prec) { /* $"{v:F2}" ... */
    char t[SIJO_FMT_MAX];
    int n = sijo_fmt_f32(t, v, kind, prec);
    sijo_sb_put(b, t, n);
}
static inline SijoStr sijo_sb_done(SijoSB* b) {
    if (b->n == 0) return SIJO_STR_EMPTY;
    b->p[b->n] = 0;
    SijoStr s = {b->p, b->n, SIJO_STR_FRAME | (sijo_u8_ascii(b->p, b->n) ? SIJO_STR_ASCII : 0)};
    SijoChunk* h = b->a ? b->a->head : NULL;
    if (h && (uint8_t*)b->p + sijo_round16((size_t)b->cap) == (uint8_t*)(h + 1) + h->used) {
        b->a->str_end = s.p + s.n; /* its buffer is the newest allocation: it may grow in place */
        b->a->str_used = h->used;
    }
    return s;
}
static SijoStr sijo_str_of_int(SijoArena* a, int32_t v) {
    SijoSB b = sijo_sb(a);
    sijo_sb_int(&b, v);
    return sijo_sb_done(&b);
}
static SijoStr sijo_str_of_float(SijoArena* a, float v) {
    SijoSB b = sijo_sb(a);
    sijo_sb_float(&b, v);
    return sijo_sb_done(&b);
}
static inline SijoStr sijo_str_of_bool(bool v) { return v ? sijo_str_lita("True", 4) : sijo_str_lita("False", 5); }
static inline SijoStr sijo_str_of_cstr(const char* s) {
    int32_t n = (int32_t)strlen(s);
    SijoStr r = {s, n, SIJO_STR_STATIC | (sijo_u8_ascii(s, n) ? SIJO_STR_ASCII : 0)};
    return r;
}

/* int.TryParse / float.TryParse (C# invariant culture: surrounding whitespace, a sign; floats also
 * take '.', exponents, ',' group separators, NaN/Infinity), independent of the C library's locale */
static bool sijo_str_parse_int(SijoStr s, int32_t* out) {
    int32_t i = 0, n = s.n;
    while (i < n && sijo_num_ws(s.p[i])) i++;
    int neg = 0;
    if (i < n && (s.p[i] == '+' || s.p[i] == '-')) neg = s.p[i++] == '-';
    int64_t v = 0;
    int32_t d0 = i;
    for (; i < n && s.p[i] >= '0' && s.p[i] <= '9'; i++) {
        v = v * 10 + (s.p[i] - '0');
        if (v > 2147483648ll) return false;
    }
    if (i == d0) return false;
    while (i < n && sijo_num_ws(s.p[i])) i++;
    if (i != n) return false;
    if (neg) v = -v;
    if (v < INT32_MIN || v > INT32_MAX) return false;
    *out = (int32_t)v;
    return true;
}
static bool sijo_str_parse_float(SijoStr s, float* out) { return sijo_parse_f32(s.p, s.n, out); }
static int32_t sijo_str_to_int(SijoStr s) {
    int32_t v;
    if (!sijo_str_parse_int(s, &v)) sijo_panic("int.Parse: input string was not in a correct format");
    return v;
}
static float sijo_str_to_float(SijoStr s) {
    float v;
    if (!sijo_str_parse_float(s, &v)) sijo_panic("float.Parse: input string was not in a correct format");
    return v;
}

/* live-coding state files store strings as "s" + hex bytes (one whitespace-free token) */
static SijoStr sijo_str_unhex(SijoSHeap* h, const char* t) {
    if (!t || t[0] != 's') return SIJO_STR_EMPTY;
    t++;
    size_t n = strlen(t) / 2;
    if (n == 0) return SIJO_STR_EMPTY;
    char* tmp = (char*)sijo_xrealloc(NULL, n);
    for (size_t i = 0; i < n; i++) {
        unsigned v = 0;
        sscanf(t + 2 * i, "%2x", &v);
        tmp[i] = (char)v;
    }
    SijoStr s = sijo_str_heap(h, tmp, (int32_t)n);
    free(tmp);
    return s;
}

/* ---------------- strings in messages and RPC arguments ----------------
 * A string argument travels by value: a little-endian 32-bit byte length, the bytes, zero padding
 * to a multiple of 4. The receiver copies it into its frame arena (stored strings are promoted at
 * the end of the tick like any other). */
static inline uint32_t sijo_rd_le32(const uint8_t* p) { return (uint32_t)p[0] | ((uint32_t)p[1] << 8) | ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24); }
static void sijo_cmd_str(SijoCmdBuf* b, SijoStr s) {
    uint8_t len[4] = {(uint8_t)s.n, (uint8_t)(s.n >> 8), (uint8_t)(s.n >> 16), (uint8_t)(s.n >> 24)};
    static const uint8_t zero[4] = {0, 0, 0, 0};
    sijo_cmd_data(b, len, 4);
    sijo_cmd_data(b, s.p, (uint32_t)s.n);
    sijo_cmd_data(b, zero, (uint32_t)((4 - (s.n & 3)) & 3));
}
static SijoStr sijo_cmd_rd_str(SijoArena* a, const uint8_t** d, const uint8_t* end, int* bad) {
    if (*bad || end - *d < 4) { *bad = 1; return SIJO_STR_EMPTY; }
    uint32_t n = sijo_rd_le32(*d);
    *d += 4;
    if (n > (uint32_t)(end - *d)) { *bad = 1; return SIJO_STR_EMPTY; }
    SijoStr s = sijo_str_frame(a, (const char*)*d, (int32_t)n);
    *d += (n + 3u) & ~3u;
    if (*d > end) { *bad = 1; *d = end; }
    return s;
}

/* ---------------- replicated strings ----------------
 * Server: every distinct value of a replicated string leaf gets an id (0 is "", ids are never
 * reused), and snapshot frames hold the id like any other quantized value, so "changed" is an exact
 * comparison. Whenever a snapshot sends a string leaf it sends the id and, unless it is "", the
 * bytes. Client: id -> text, filled from snapshots. Both sides drop ids that no frame of their
 * history refers to any more (sijo_stab_gc in sijo_net.h). */
typedef struct { uint32_t id, h, mark; int32_t n; char* p; } SijoSTE;
typedef struct {
    SijoSTE* e; uint32_t n, cap;        /* entries, dense */
    uint32_t* byid; uint32_t* byc;      /* open-addressing indexes (entry + 1): by id, by content */
    uint32_t icap;                      /* size of both indexes (power of two) */
    uint32_t next, epoch;
    uint64_t interned, dropped;
} SijoSTab;

static inline uint32_t sijo_str_h32(SijoStr s) { uint64_t h = sijo_fnv(0xcbf29ce484222325ull, s.p, (size_t)s.n); return (uint32_t)(h ^ (h >> 32)); }
static inline uint32_t sijo_id_h(uint32_t id) { return id * 2654435761u; }

static void sijo_stab_index(SijoSTab* t, uint32_t i) {
    uint32_t m = t->icap - 1, j = sijo_id_h(t->e[i].id) & m;
    while (t->byid[j]) j = (j + 1) & m;
    t->byid[j] = i + 1;
    j = t->e[i].h & m;
    while (t->byc[j]) j = (j + 1) & m;
    t->byc[j] = i + 1;
}
static void sijo_stab_reindex(SijoSTab* t, uint32_t want) {
    uint32_t c = 64;
    while (c < want * 2) c *= 2;
    free(t->byid);
    free(t->byc);
    t->icap = c;
    t->byid = (uint32_t*)calloc(c, sizeof(uint32_t));
    t->byc = (uint32_t*)calloc(c, sizeof(uint32_t));
    if (!t->byid || !t->byc) sijo_panic("out of memory");
    for (uint32_t i = 0; i < t->n; i++) sijo_stab_index(t, i);
}
static const SijoSTE* sijo_stab_find(const SijoSTab* t, uint32_t id) {
    if (!t->icap) return NULL;
    uint32_t m = t->icap - 1, j = sijo_id_h(id) & m;
    while (t->byid[j]) {
        const SijoSTE* x = &t->e[t->byid[j] - 1];
        if (x->id == id) return x;
        j = (j + 1) & m;
    }
    return NULL;
}
static uint32_t sijo_stab_add(SijoSTab* t, uint32_t id, uint32_t h, const char* p, int32_t n) {
    if (t->n == t->cap) {
        t->cap = t->cap ? t->cap * 2 : 64;
        t->e = (SijoSTE*)sijo_xrealloc(t->e, (size_t)t->cap * sizeof(SijoSTE));
    }
    SijoSTE* x = &t->e[t->n];
    x->id = id; x->h = h; x->mark = t->epoch; x->n = n;
    x->p = (char*)sijo_xrealloc(NULL, (size_t)n + 1);
    memcpy(x->p, p, (size_t)n);
    x->p[n] = 0;
    t->n++;
    if (t->n * 2 > t->icap) sijo_stab_reindex(t, t->n);
    else sijo_stab_index(t, t->n - 1);
    return id;
}
/* server: the id of a string value (a new one for a value not seen recently) */
static uint32_t sijo_stab_intern(SijoSTab* t, SijoStr s) {
    if (s.n <= 0) return 0;
    uint32_t h = sijo_str_h32(s);
    if (t->icap) {
        uint32_t m = t->icap - 1, j = h & m;
        while (t->byc[j]) {
            SijoSTE* x = &t->e[t->byc[j] - 1];
            if (x->h == h && x->n == s.n && memcmp(x->p, s.p, (size_t)s.n) == 0) { x->mark = t->epoch; return x->id; }
            j = (j + 1) & m;
        }
    }
    if (++t->next == 0) t->next = 1;
    t->interned++;
    return sijo_stab_add(t, t->next, h, s.p, s.n);
}
/* client: the text of an id received in a snapshot (ids are never reused, so a known id is kept) */
static void sijo_stab_put(SijoSTab* t, uint32_t id, const char* p, int32_t n) {
    if (!id || sijo_stab_find(t, id)) return;
    SijoStr s = {p, n, 0};
    t->interned++;
    sijo_stab_add(t, id, sijo_str_h32(s), p, n);
}
/* the value of an id as a heap string of world `h` (stored in a field: the collector owns it) */
static SijoStr sijo_stab_str(const SijoSTab* t, SijoSHeap* h, uint32_t id) {
    if (!id) return SIJO_STR_EMPTY;
    const SijoSTE* x = sijo_stab_find(t, id);
    if (!x) sijo_panic("replicated string missing on the client");
    return sijo_str_heap(h, x->p, x->n);
}
static uint32_t sijo_stab_h32(const SijoSTab* t, uint32_t id) {
    if (!id) return sijo_str_h32(SIJO_STR_EMPTY);
    const SijoSTE* x = sijo_stab_find(t, id);
    return x ? x->h : 0;
}
static void sijo_stab_mark(SijoSTab* t, uint32_t id) {
    if (!id) return;
    const SijoSTE* x = sijo_stab_find(t, id);
    if (x) ((SijoSTE*)x)->mark = t->epoch;
}
static void sijo_stab_sweep(SijoSTab* t) {
    uint32_t j = 0;
    for (uint32_t i = 0; i < t->n; i++) {
        if (t->e[i].mark == t->epoch) t->e[j++] = t->e[i];
        else { free(t->e[i].p); t->dropped++; }
    }
    if (j != t->n) {
        t->n = j;
        sijo_stab_reindex(t, t->n);
    }
}
static void sijo_stab_free(SijoSTab* t) {
    for (uint32_t i = 0; i < t->n; i++) free(t->e[i].p);
    free(t->e);
    free(t->byid);
    free(t->byc);
    memset(t, 0, sizeof *t);
}

/* replicated lists, arrays and dictionaries travel as canonical bytes (element count, then every
   element: 32-bit little-endian numbers, 1-byte bools, length-prefixed strings, entity references
   as the server's slot | generation << 24, nested lists the same way; a dictionary is its
   key/value pairs in its own order), interned in the same table as strings */
typedef struct { uint8_t* p; size_t n, cap; } SijoBlob;
static void sijo_blob_put(SijoBlob* b, const void* d, size_t n) {
    if (b->n + n > b->cap) {
        size_t c = b->cap ? b->cap * 2 : 64;
        while (c < b->n + n) c *= 2;
        b->p = (uint8_t*)sijo_xrealloc(b->p, c);
        b->cap = c;
    }
    if (n) memcpy(b->p + b->n, d, n);
    b->n += n;
}
static inline void sijo_blob_u32(SijoBlob* b, uint32_t v) { uint8_t t[4] = {(uint8_t)v, (uint8_t)(v >> 8), (uint8_t)(v >> 16), (uint8_t)(v >> 24)}; sijo_blob_put(b, t, 4); }
static inline void sijo_blob_u8(SijoBlob* b, uint32_t v) { uint8_t t = (uint8_t)v; sijo_blob_put(b, &t, 1); }
/* overwrite the 32-bit value written at `at` (a count known only after the elements) */
static inline void sijo_blob_set32(SijoBlob* b, size_t at, uint32_t v) {
    b->p[at] = (uint8_t)v; b->p[at + 1] = (uint8_t)(v >> 8); b->p[at + 2] = (uint8_t)(v >> 16); b->p[at + 3] = (uint8_t)(v >> 24);
}
static inline int sijo_blob_rd32(const uint8_t** p, const uint8_t* e, uint32_t* v) {
    if (e - *p < 4) return 0;
    *v = sijo_rd_le32(*p);
    *p += 4;
    return 1;
}
static inline int sijo_blob_rd8(const uint8_t** p, const uint8_t* e, uint32_t* v) {
    if (e - *p < 1) return 0;
    *v = **p;
    *p += 1;
    return 1;
}
static uint32_t sijo_stab_intern_blob(SijoSTab* t, const SijoBlob* b) {
    SijoStr s = {(const char*)b->p, (int32_t)b->n, 0};
    return sijo_stab_intern(t, s);
}
static uint32_t sijo_blob_h32(const SijoBlob* b) {
    SijoStr s = {(const char*)b->p, (int32_t)b->n, 0};
    return sijo_str_h32(s);
}
