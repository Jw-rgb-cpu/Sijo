/* sijo_dict.h - Dictionary<K, V>.
 *
 * A dictionary is a SijoList of entries {K Key; V Value;} (the KeyValuePair<K, V> struct), so it
 * shares the list's ownership rules (frame arena or entity-owned heap, moves, freeing) and
 * `foreach (var kv in d)` is a plain loop over the entries. After the `cap` entries the same
 * buffer holds a chained hash index: `nb` bucket heads and `cap` next links. Iteration order is
 * insertion order, except that Remove moves the last entry into the hole (deterministic).
 * Lookups never modify the dictionary, so parallel systems can read other entities' snapshots.
 */
enum { SIJO_KEY_I32 = 0, SIJO_KEY_H = 1, SIJO_KEY_STR = 2 };

static inline uint32_t sijo_key_hash(int kind, const void* k) {
    if (kind == SIJO_KEY_I32) {
        uint32_t x;
        memcpy(&x, k, 4);
        return (uint32_t)sijo_mix(x);
    }
    if (kind == SIJO_KEY_H) {
        SijoH h;
        memcpy(&h, k, sizeof h);
        return (uint32_t)sijo_mix(((uint64_t)h.idx << 32) | h.gen);
    }
    SijoStr s;
    memcpy(&s, k, sizeof s);
    return (uint32_t)sijo_str_hash(s);
}
static inline bool sijo_key_eq(int kind, const void* a, const void* b) {
    if (kind == SIJO_KEY_I32) return memcmp(a, b, 4) == 0;
    if (kind == SIJO_KEY_H) {
        SijoH x, y;
        memcpy(&x, a, sizeof x);
        memcpy(&y, b, sizeof y);
        return sijo_heq(x, y);
    }
    SijoStr x, y;
    memcpy(&x, a, sizeof x);
    memcpy(&y, b, sizeof y);
    return sijo_str_eq(x, y);
}
static inline uint8_t* sijo_dict_entry(const SijoList* d, int32_t esz, int32_t i) { return (uint8_t*)d->data + (size_t)i * esz; }
static inline int32_t* sijo_dict_buckets(const SijoList* d, int32_t esz) { return (int32_t*)((uint8_t*)d->data + (size_t)d->cap * esz); }
static inline int32_t* sijo_dict_next(const SijoList* d, int32_t esz) { return sijo_dict_buckets(d, esz) + d->nb; }

static void sijo_dict_reindex(SijoList* d, int kind, int32_t esz) {
    int32_t* b = sijo_dict_buckets(d, esz);
    int32_t* nx = sijo_dict_next(d, esz);
    for (int32_t i = 0; i < d->nb; i++) b[i] = -1;
    for (int32_t i = 0; i < d->len; i++) {
        uint32_t h = sijo_key_hash(kind, sijo_dict_entry(d, esz, i)) & (uint32_t)(d->nb - 1);
        nx[i] = b[h];
        b[h] = i;
    }
}

/* room for `need` entries plus an index (entries are moved to a new buffer when it grows) */
static void sijo_dict_reserve(SijoArena* a, SijoList* d, int kind, int32_t esz, int32_t need) {
    if (d->nb > 0 && need <= d->cap) return;
    int32_t cap = 8;
    while (cap < need || cap < d->len) cap *= 2;
    int32_t nb = cap; /* power of two, average chain length <= 1 */
    size_t bytes = (size_t)cap * esz + (size_t)nb * 4 + (size_t)cap * 4;
    void* nd = d->heap ? sijo_xrealloc(NULL, bytes) : sijo_arena_alloc(a, bytes);
    if (d->len) memcpy(nd, d->data, (size_t)d->len * esz);
    if (d->heap && d->data) free(d->data);
    d->data = nd;
    d->cap = cap;
    d->nb = nb;
    sijo_dict_reindex(d, kind, esz);
}

static int32_t sijo_dict_find(const SijoList* d, int kind, int32_t esz, const void* key) {
    if (d->len == 0) return -1;
    if (d->nb == 0) { /* copied without its index: scan (read-only) */
        for (int32_t i = 0; i < d->len; i++)
            if (sijo_key_eq(kind, sijo_dict_entry(d, esz, i), key)) return i;
        return -1;
    }
    uint32_t h = sijo_key_hash(kind, key) & (uint32_t)(d->nb - 1);
    const int32_t* nx = sijo_dict_next(d, esz);
    for (int32_t i = sijo_dict_buckets(d, esz)[h]; i >= 0; i = nx[i])
        if (sijo_key_eq(kind, sijo_dict_entry(d, esz, i), key)) return i;
    return -1;
}

/* entry index for `key`, inserting a zeroed entry with that key if it is missing */
static int32_t sijo_dict_slot(SijoArena* a, SijoList* d, int kind, int32_t esz, const void* key, int32_t ksz) {
    int32_t i = sijo_dict_find(d, kind, esz, key);
    if (i >= 0) return i;
    sijo_dict_reserve(a, d, kind, esz, d->len + 1);
    i = d->len++;
    uint8_t* e = sijo_dict_entry(d, esz, i);
    memset(e, 0, (size_t)esz);
    memcpy(e, key, (size_t)ksz);
    uint32_t h = sijo_key_hash(kind, key) & (uint32_t)(d->nb - 1);
    int32_t* b = sijo_dict_buckets(d, esz);
    sijo_dict_next(d, esz)[i] = b[h];
    b[h] = i;
    return i;
}

static bool sijo_dict_remove(SijoList* d, int kind, int32_t esz, const void* key) {
    int32_t last = d->len - 1;
    if (d->nb == 0) {
        int32_t i = sijo_dict_find(d, kind, esz, key);
        if (i < 0) return false;
        if (i != last) memcpy(sijo_dict_entry(d, esz, i), sijo_dict_entry(d, esz, last), (size_t)esz);
        d->len--;
        return true;
    }
    int32_t* b = sijo_dict_buckets(d, esz);
    int32_t* nx = sijo_dict_next(d, esz);
    uint32_t mask = (uint32_t)(d->nb - 1);
    uint32_t h = sijo_key_hash(kind, key) & mask;
    int32_t prev = -1, i = b[h];
    while (i >= 0 && !sijo_key_eq(kind, sijo_dict_entry(d, esz, i), key)) {
        prev = i;
        i = nx[i];
    }
    if (i < 0) return false;
    if (prev < 0) b[h] = nx[i]; else nx[prev] = nx[i];
    if (i != last) { /* move the last entry into the hole and redirect the link that pointed at it */
        uint32_t h2 = sijo_key_hash(kind, sijo_dict_entry(d, esz, last)) & mask;
        if (b[h2] == last) {
            b[h2] = i;
        } else {
            int32_t j = b[h2];
            while (nx[j] != last) j = nx[j];
            nx[j] = i;
        }
        nx[i] = nx[last];
        memcpy(sijo_dict_entry(d, esz, i), sijo_dict_entry(d, esz, last), (size_t)esz);
    }
    d->len--;
    return true;
}

static void sijo_dict_clear(SijoList* d, int32_t esz) {
    d->len = 0;
    if (d->nb) {
        int32_t* b = sijo_dict_buckets(d, esz);
        for (int32_t i = 0; i < d->nb; i++) b[i] = -1;
    }
}

/* deep copy with its own index (hazard snapshots) */
static SijoList sijo_dict_clone(SijoArena* a, int heap, const SijoList* s, int kind, int32_t esz) {
    SijoList d = {0, 0, 0, (uint8_t)heap};
    if (s->len <= 0) return d;
    sijo_dict_reserve(a, &d, kind, esz, s->len);
    memcpy(d.data, s->data, (size_t)s->len * esz);
    d.len = s->len;
    sijo_dict_reindex(&d, kind, esz);
    return d;
}

/* entries were filled like a list (state loading): build the index */
static void sijo_dict_rebuild(SijoList* d, int kind, int32_t esz) {
    d->nb = 0;
    if (d->len) sijo_dict_reserve(NULL, d, kind, esz, d->len);
}
