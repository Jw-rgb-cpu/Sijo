/* sijo_net.h - Sijo networking runtime: UDP transport with network-condition simulation,
 * delta snapshots against acknowledged baselines, fragmentation, reliable client->server RPCs,
 * and the three process modes (in-process loopback, --server PORT, --connect HOST:PORT).
 *
 * sijoc emits this file after all generated code that names user identifiers: the socket headers
 * define lower-case macros (h_addr, sa_handler, s6_addr...) that would otherwise clash with field
 * names. The generated program provides, before this file:
 *   SijoWorld, SIJO_NARCH, SIJO_MAXLEAF, SIJO_ARCH_BITS, SIJO_PROTO, SIJO_NSTAGES,
 *   sijo_archnet[SIJO_NARCH], sijo_arch_repl[],
 *   sijo_world_init, sijo_reset_arenas, sijo_game_start, sijo_print_counts,
 *   sijo_apply, sijo_tick, sijo_shards_update, sijo_client_frame, sijo_checksum,
 *   sijo_net_capture  (server world -> frame, using the compiler-inserted dirty flags)
 *   sijo_net_apply    (client world: previous frame -> new frame, as spawns/despawns/updates)
 *   sijo_net_hash_server, sijo_net_hash_client, sijo_net_verify, sijo_rpc_ok,
 *   sijo_save_state, sijo_load_state
 *
 * Wire format (all integers little-endian):
 *   every datagram   u8 type | u32 session | u16 datagram seq            (7 bytes)
 *   CONNECT    c->s  session = client nonce | u32 magic "SIJO" | u32 protocol hash
 *   ACCEPT     s->c  u32 nonce | u16 client id | u16 tick rate | u64 seed | u32 server tick
 *   REJECT     s->c  session = client nonce | u8 reason (1 protocol, 2 full, 3 game over)
 *   SNAP       s->c  u32 tick | u32 baseline tick (0xffffffff = none) | u32 RPC ack
 *                    | u16 fragment | u16 fragments | payload bytes (<= SIJO_FRAG)
 *                    fragment == fragments: XOR parity of all fragments (each zero-padded to SIJO_FRAG),
 *                    sent when the client reports loss; it rebuilds any one lost fragment
 *   CLIENT     c->s  u32 acked tick (0xffffffff = none) | u8 resync epoch | u8 recent loss %
 *                    | u32 first RPC seq | u8 count | RPC records (varint slot, gen, method, size, bytes)
 *   DISCONNECT either direction;  BYE s->c u32 final tick;  BYE_ACK c->s
 * Snapshot payload (bit-packed, LSB first): record* end
 *   record = tag:2 slot-delta:varint body;  1 despawn | 2 spawn gen:varint arch:SIJO_ARCH_BITS leaves
 *            | 3 update mask:ncomp leaves-of-changed-components;   end = tag 0
 *   a string or list leaf is an interned id and its bytes, or (in an update) a delta against the
 *   baseline's value: see sijo_put_leaf
 */
/* ---------------- platform layer: POSIX sockets or Winsock ---------------- */
#ifndef SIJO_WASM
#include <errno.h>
#include <signal.h>
#endif
#if defined(SIJO_WASM)
/* --target wasm32: no sockets (the game's own client is in-process); see sijo_wasm.h */
typedef socklen_t sijo_socklen;
static int sijo_sock_open_udp(int fam) { (void)fam; return -1; }
static void sijo_sock_close(int fd) { (void)fd; }
static void sijo_sock_nonblock(int fd) { (void)fd; }
static void sijo_sock_poll_in(int fd, int ms) { (void)fd; (void)ms; }
static int sijo_sock_retry(void) { return 0; }
static const char* sijo_sock_errstr(void) { return "no network on wasm32"; }
static void sijo_sleep(double secs) { (void)secs; }
static uint64_t sijo_pid(void) { return 1; }
static void sijo_platform_init(void) {}
static double sijo_now(void) { return sijo_wasm_now(); }
#elif defined(_WIN32)
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#ifndef SIO_UDP_CONNRESET
#define SIO_UDP_CONNRESET _WSAIOW(IOC_VENDOR, 12)
#endif
typedef int sijo_socklen;
static void sijo_net_startup(void) {
    static int done;
    if (!done) { WSADATA d; WSAStartup(MAKEWORD(2, 2), &d); done = 1; }
}
static int sijo_sock_open_udp(int fam) {
    sijo_net_startup();
    SOCKET s = socket(fam, SOCK_DGRAM, 0);
    if (s == INVALID_SOCKET) return -1;
    BOOL off = FALSE; DWORD ret = 0; /* no WSAECONNRESET from recvfrom after an ICMP port-unreachable */
    WSAIoctl(s, SIO_UDP_CONNRESET, &off, sizeof off, NULL, 0, &ret, NULL, NULL);
    return (int)s;
}
static void sijo_sock_close(int fd) { closesocket((SOCKET)fd); }
static void sijo_sock_nonblock(int fd) { u_long on = 1; ioctlsocket((SOCKET)fd, FIONBIO, &on); }
static void sijo_sock_poll_in(int fd, int ms) {
    WSAPOLLFD p; p.fd = (SOCKET)fd; p.events = POLLRDNORM; p.revents = 0;
    WSAPoll(&p, 1, ms);
}
static int sijo_sock_retry(void) { /* recvfrom failed for a reason that does not mean "no datagram" */
    int e = WSAGetLastError();
    return e == WSAECONNRESET || e == WSAEMSGSIZE || e == WSAEINTR;
}
static const char* sijo_sock_errstr(void) {
    static char b[48];
    snprintf(b, sizeof b, "winsock error %d", WSAGetLastError());
    return b;
}
static void sijo_sleep(double secs) { if (secs > 0) Sleep((DWORD)(secs * 1000.0 + 0.5)); }
static uint64_t sijo_pid(void) { return (uint64_t)GetCurrentProcessId(); }
#include <io.h>
#include <fcntl.h>
static LONG WINAPI sijo_on_crash(EXCEPTION_POINTERS* e) {
    DWORD c = e->ExceptionRecord->ExceptionCode;
    fflush(stdout);
    fprintf(stderr, "sijo panic: %s\n", c == EXCEPTION_STACK_OVERFLOW ? "stack overflow (recursion too deep?)" : "invalid memory access");
    fflush(stderr);
    _exit(134);
    return EXCEPTION_EXECUTE_HANDLER;
}
static void sijo_platform_init(void) { /* "\n" stays "\n": output is byte-identical to other platforms */
    _setmode(_fileno(stdout), _O_BINARY);
    _setmode(_fileno(stderr), _O_BINARY);
    SetUnhandledExceptionFilter(sijo_on_crash);
}
static double sijo_now(void) {
    static LARGE_INTEGER f;
    LARGE_INTEGER c;
    if (!f.QuadPart) QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&c);
    return (double)c.QuadPart / (double)f.QuadPart;
}
#else
#include <sys/types.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <netdb.h>
#include <poll.h>
#include <unistd.h>
#include <fcntl.h>
typedef socklen_t sijo_socklen;
static int sijo_sock_open_udp(int fam) { return socket(fam, SOCK_DGRAM, 0); }
static void sijo_sock_close(int fd) { close(fd); }
static void sijo_sock_nonblock(int fd) { fcntl(fd, F_SETFL, fcntl(fd, F_GETFL, 0) | O_NONBLOCK); }
static void sijo_sock_poll_in(int fd, int ms) {
    struct pollfd p; p.fd = fd; p.events = POLLIN; p.revents = 0;
    poll(&p, 1, ms);
}
static int sijo_sock_retry(void) { return errno == EINTR; }
static const char* sijo_sock_errstr(void) { return strerror(errno); }
static void sijo_sleep(double secs) {
    if (secs <= 0) return;
    struct timespec ts = { (time_t)secs, (long)((secs - (double)(time_t)secs) * 1e9) };
    nanosleep(&ts, NULL);
}
static uint64_t sijo_pid(void) { return (uint64_t)getpid(); }
/* a crash (stack overflow from deep recursion, or a bug in host code) still flushes what the game
 * printed and says what happened; runs on its own stack, since the thread's stack may be exhausted */
static void sijo_on_crash(int sig) {
    static const char so[] = "sijo panic: stack overflow (recursion too deep?) or invalid memory access\n";
    fflush(stdout);
    if (write(2, so, sizeof so - 1) < 0) { /* nothing more to do */ }
    (void)sig;
    _exit(134);
}
static void sijo_platform_init(void) {
    static char alt[64 * 1024];
    stack_t ss;
    memset(&ss, 0, sizeof ss);
    ss.ss_sp = alt;
    ss.ss_size = sizeof alt;
    sigaltstack(&ss, NULL);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = sijo_on_crash;
    sa.sa_flags = SA_ONSTACK;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, NULL);
    sigaction(SIGBUS, &sa, NULL);
}
static double sijo_now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}
#endif

#define SIJO_MTU 1192                  /* largest datagram before sealing */
#define SIJO_SEAL 40                   /* --key: 24-byte nonce + 16-byte tag */
#define SIJO_WIRE_MTU (SIJO_MTU + SIJO_SEAL) /* 1232: fits any IPv6 path (1280 - 48) without fragmentation */
#define SIJO_PKT_HDR 7
#define SIJO_SNAP_HDR (SIJO_PKT_HDR + 16)
#define SIJO_FRAG (SIJO_MTU - SIJO_SNAP_HDR)
#define SIJO_HIST 64                   /* frames of baseline history (~1 s at 60 Hz) */
#define SIJO_ASM_SLOTS 8               /* snapshots being reassembled at once (client) */
#define SIJO_MAX_SNAPSHOT (16u << 20)
#define SIJO_RPC_MAX_ARGS 1024
#define SIJO_RPC_QUEUE_UDP 4096        /* unacknowledged RPCs a networked client may hold */
#define SIJO_QUEUE_MAX 16384           /* datagrams in flight per socket (simulated delay / loopback) */
#define SIJO_NONE 0xffffffffu
#define SIJO_MAX_SLOT (1u << 24)       /* entity slots a snapshot may name (bounds client memory) */
#define SIJO_BYES 30
#define SIJO_MAGIC 0x4f4a4953u         /* "SIJO" */
typedef union { struct sockaddr sa; struct sockaddr_in in; struct sockaddr_in6 in6; struct sockaddr_storage ss; } SijoSockAddr;

enum { SIJO_PK_CONNECT = 1, SIJO_PK_ACCEPT, SIJO_PK_REJECT, SIJO_PK_SNAP, SIJO_PK_CLIENT, SIJO_PK_DISCONNECT, SIJO_PK_BYE, SIJO_PK_BYE_ACK };

static volatile sig_atomic_t sijo_stop_flag = 0;
static void sijo_on_signal(int sig) { (void)sig; sijo_stop_flag = 1; }

/* ---------------- byte-level writer / reader (packet headers, RPC records) ---------------- */
typedef struct { uint8_t* p; size_t n, cap; } SijoBuf;
static inline void sijo_w8(SijoBuf* b, uint32_t v) { if (b->n < b->cap) b->p[b->n] = (uint8_t)v; b->n++; }
static inline void sijo_w16(SijoBuf* b, uint32_t v) { sijo_w8(b, v & 0xff); sijo_w8(b, (v >> 8) & 0xff); }
static inline void sijo_w32(SijoBuf* b, uint32_t v) { sijo_w16(b, v & 0xffff); sijo_w16(b, v >> 16); }
static inline void sijo_w64(SijoBuf* b, uint64_t v) { sijo_w32(b, (uint32_t)v); sijo_w32(b, (uint32_t)(v >> 32)); }
static inline void sijo_wvar(SijoBuf* b, uint32_t v) {
    do { uint32_t g = v & 0x7f; v >>= 7; sijo_w8(b, g | (v ? 0x80u : 0u)); } while (v);
}
static inline void sijo_wbytes(SijoBuf* b, const void* d, size_t n) {
    if (n && b->n + n <= b->cap) memcpy(b->p + b->n, d, n);
    b->n += n;
}

typedef struct { const uint8_t* p; size_t n, pos; int bad; } SijoRB;
static inline uint32_t sijo_r8(SijoRB* r) {
    if (r->pos >= r->n) { r->bad = 1; return 0; }
    return r->p[r->pos++];
}
static inline uint32_t sijo_r16(SijoRB* r) { uint32_t a = sijo_r8(r); uint32_t b = sijo_r8(r); return a | (b << 8); }
static inline uint32_t sijo_r32(SijoRB* r) { uint32_t a = sijo_r16(r); uint32_t b = sijo_r16(r); return a | (b << 16); }
static inline uint64_t sijo_r64(SijoRB* r) { uint64_t a = sijo_r32(r); uint64_t b = sijo_r32(r); return a | (b << 32); }
static inline uint32_t sijo_rvar(SijoRB* r) {
    uint32_t v = 0;
    for (int sh = 0; sh < 35; sh += 7) {
        uint32_t g = sijo_r8(r);
        v |= (g & 0x7f) << sh;
        if (!(g & 0x80)) return v;
    }
    r->bad = 1;
    return 0;
}
static inline const uint8_t* sijo_rbytes(SijoRB* r, size_t n) {
    if (r->pos + n > r->n) { r->bad = 1; r->pos = r->n; return NULL; }
    const uint8_t* p = r->p + r->pos;
    r->pos += n;
    return p;
}

/* ---------------- bit-level writer / bounds-checked reader (snapshot payloads) ---------------- */
typedef struct { uint8_t* buf; size_t cap, len; uint64_t acc; int nacc; } SijoBW;
static void sijo_bw_reset(SijoBW* b) { b->len = 0; b->acc = 0; b->nacc = 0; }
static inline void sijo_bw_put(SijoBW* b, uint32_t v, int n) {
    if (n <= 0) return;
    if (n < 32) v &= (1u << n) - 1u;
    b->acc |= (uint64_t)v << b->nacc;
    b->nacc += n;
    if (b->len + 8 > b->cap) {
        size_t nc = b->cap ? b->cap * 2 : 4096;
        b->buf = (uint8_t*)sijo_xrealloc(b->buf, nc);
        b->cap = nc;
    }
    while (b->nacc >= 8) { b->buf[b->len++] = (uint8_t)b->acc; b->acc >>= 8; b->nacc -= 8; }
}
static inline void sijo_bw_var(SijoBW* b, uint32_t v) {
    do { uint32_t g = v & 0x7f; v >>= 7; sijo_bw_put(b, g | (v ? 0x80u : 0u), 8); } while (v);
}
static void sijo_bw_finish(SijoBW* b) { if (b->nacc > 0) sijo_bw_put(b, 0, 8 - b->nacc); }

typedef struct { const uint8_t* p; size_t nbits, bit; int bad; } SijoRd;
static inline uint32_t sijo_rd_get(SijoRd* r, int n) {
    if (n <= 0) return 0;
    if (r->bit + (size_t)n > r->nbits) { r->bad = 1; r->bit = r->nbits; return 0; }
    uint32_t v = 0;
    int got = 0;
    while (got < n) {
        size_t by = r->bit >> 3;
        int off = (int)(r->bit & 7);
        int take = 8 - off;
        if (take > n - got) take = n - got;
        v |= (((uint32_t)r->p[by] >> off) & ((1u << take) - 1u)) << got;
        got += take;
        r->bit += (size_t)take;
    }
    return v;
}
static inline uint32_t sijo_rd_var(SijoRd* r) {
    uint32_t v = 0;
    for (int sh = 0; sh < 35; sh += 7) {
        uint32_t g = sijo_rd_get(r, 8);
        v |= (g & 0x7f) << sh;
        if (!(g & 0x80)) return v;
    }
    r->bad = 1;
    return 0;
}

/* ---------------- delta snapshot codec ----------------
 * Encodes frame `cur` relative to `base` (NULL = empty baseline, i.e. full state). Both frames are
 * sorted by slot, so this is a merge walk; spawns and despawns are relative to the baseline too. */
typedef struct {
    uint64_t spawns, updates, despawns;
    /* string and list leaves sent: whole, as deltas against the acknowledged value (and the bytes
       those deltas took / would have taken whole), unchanged inside a changed component */
    uint64_t whole, deltas, delta_bytes, delta_whole_bytes, same;
} SijoDStats;
static int sijo_list_deltas = 1;      /* --no-list-deltas: send strings and lists whole */

/* a leaf value: `bits` bits, or for an interned leaf (SIJO_LEAF_STR: a string, or a list as its
   canonical bytes) the id and, unless it is 0 (""), either
     n:var (n > 0) and the n bytes,
     0 when the value equals the baseline's (an unchanged leaf of a changed component), or
     0 then a delta against the baseline's bytes: length:var and ops until that length, each
     op = (len << 1 | 1):var shift:zigzag-var  (copy len bytes from baseline offset pos + shift)
        | (len << 1):var bytes             (len literal bytes)
   The baseline is the frame the client acknowledged, which it still holds with every interned
   value it refers to (5.5.2), so an edit of a long list costs only its changed bytes. */
#define SIJO_LEAF_STR 255
#define SIJO_MAX_STR (1u << 20)
#define SIJO_DMIN 4                   /* shortest copy worth an op */
static inline uint32_t sijo_zz(int32_t v) { return ((uint32_t)v << 1) ^ (0u - ((uint32_t)v >> 31)); }
static inline int32_t sijo_unzz(uint32_t v) { return (int32_t)(v >> 1) ^ -(int32_t)(v & 1); }
static inline void sijo_dvar(SijoBlob* o, uint32_t v) {
    do { uint8_t g = (uint8_t)(v & 0x7f); v >>= 7; if (v) g |= 0x80; sijo_blob_put(o, &g, 1); } while (v);
}
static inline int32_t sijo_dmatch(const uint8_t* a, int32_t la, const uint8_t* b, int32_t lb, int32_t pos, int32_t src) {
    if (src < 0 || src >= la) return 0;
    int32_t n = 0;
    while (pos + n < lb && src + n < la && b[pos + n] == a[src + n]) n++;
    return n;
}
/* the ops turning bytes a into bytes b (after the length), greedy: at each position the longest of
   the copy at the previous copy's shift, at the same offset, and at the last place in a where the
   next 4 bytes occur; every input decides the output alone (deterministic on every platform) */
static void sijo_idelta(const uint8_t* a, int32_t la, const uint8_t* b, int32_t lb, SijoBlob* o) {
    static int32_t* head;
    static int32_t hcap;
    int32_t hb = 64;
    while (hb < 2 * la) hb *= 2;
    if (hb > hcap) { head = (int32_t*)sijo_xrealloc(head, (size_t)hb * sizeof(int32_t)); hcap = hb; }
    memset(head, 0, (size_t)hb * sizeof(int32_t));
    int sh = 0;
    while ((1 << sh) < hb) sh++;
#define SIJO_DH(p) ((uint32_t)(((uint32_t)(p)[0] | (uint32_t)(p)[1] << 8 | (uint32_t)(p)[2] << 16 | (uint32_t)(p)[3] << 24) * 2654435761u) >> (32 - sh))
    for (int32_t i = 0; i + 4 <= la; i++) head[SIJO_DH(a + i)] = i + 1;
    o->n = 0;
    sijo_dvar(o, (uint32_t)lb);
    int32_t pos = 0, lit = 0, last = 0;
    while (pos < lb) {
        int32_t best = sijo_dmatch(a, la, b, lb, pos, pos + last), bs = last;
        if (last != 0) {
            int32_t n0 = sijo_dmatch(a, la, b, lb, pos, pos);
            if (n0 > best) { best = n0; bs = 0; }
        }
        if (best < 64 && pos + 4 <= lb) {
            int32_t c = head[SIJO_DH(b + pos)] - 1;
            if (c >= 0) {
                int32_t nh = sijo_dmatch(a, la, b, lb, pos, c);
                if (nh > best) { best = nh; bs = c - pos; }
            }
        }
        if (best < SIJO_DMIN) { pos++; continue; }
        if (pos > lit) { sijo_dvar(o, (uint32_t)(pos - lit) << 1); sijo_blob_put(o, b + lit, (size_t)(pos - lit)); }
        sijo_dvar(o, (uint32_t)best << 1 | 1u);
        sijo_dvar(o, sijo_zz(bs));
        last = bs;
        pos += best;
        lit = pos;
    }
    if (pos > lit) { sijo_dvar(o, (uint32_t)(pos - lit) << 1); sijo_blob_put(o, b + lit, (size_t)(pos - lit)); }
#undef SIJO_DH
}
/* bits an interned leaf takes on the wire: v against the baseline's value bv (0: none). With `ops`,
   the delta ops are left there when the delta is the cheaper encoding (else ops->n = 0) */
static int64_t sijo_ileaf_bits(uint32_t v, uint32_t bv, const SijoSTab* stab, SijoBlob* ops) {
    if (ops) ops->n = 0;
    int64_t idb = 0;
    for (uint32_t x = v; ; x >>= 7) { idb += 8; if (x < 128) break; }
    if (!v) return idb;
    if (v == bv) return idb + 8;
    const SijoSTE* x = sijo_stab_find(stab, v);
    if (!x) sijo_panic("replicated string missing on the server");
    int64_t whole = idb + 8 * (int64_t)x->n;
    for (uint32_t n = (uint32_t)x->n; ; n >>= 7) { whole += 8; if (n < 128) break; }
    const SijoSTE* y = bv && sijo_list_deltas && x->n >= 8 ? sijo_stab_find(stab, bv) : NULL;
    if (!y) return whole;
    SijoBlob tmp = {0};
    SijoBlob* o = ops ? ops : &tmp;
    sijo_idelta((const uint8_t*)y->p, y->n, (const uint8_t*)x->p, x->n, o);
    int64_t delta = idb + 8 + 8 * (int64_t)o->n;
    if (!ops) free(tmp.p);
    if (delta >= whole) { if (ops) ops->n = 0; return whole; }
    return delta;
}
static void sijo_put_leaf(SijoBW* b, const SijoArchNet* an, int k, uint32_t v, uint32_t bv, const SijoSTab* stab, SijoDStats* st) {
    if (an->bits[k] != SIJO_LEAF_STR) { sijo_bw_put(b, v, an->bits[k]); return; }
    static SijoBlob ops;
    sijo_bw_var(b, v);
    if (!v) return;
    if (v == bv) { sijo_bw_var(b, 0); st->same++; return; }
    const SijoSTE* x = sijo_stab_find(stab, v);
    if (!x) sijo_panic("replicated string missing on the server");
    sijo_ileaf_bits(v, bv, stab, &ops);
    if (ops.n) {
        sijo_bw_var(b, 0);
        for (size_t i = 0; i < ops.n; i++) sijo_bw_put(b, ops.p[i], 8);
        st->deltas++;
        st->delta_bytes += ops.n + 1;
        st->delta_whole_bytes += (uint64_t)x->n + (x->n < 128 ? 1 : x->n < 16384 ? 2 : 3);
        return;
    }
    sijo_bw_var(b, (uint32_t)x->n);
    for (int32_t i = 0; i < x->n; i++) sijo_bw_put(b, (uint8_t)x->p[i], 8);
    st->whole++;
}
/* `bv`: the leaf's value in the baseline (0: none, a spawn) */
static uint32_t sijo_get_leaf(SijoRd* r, const SijoArchNet* an, int k, uint32_t bv, SijoSTab* stab) {
    if (an->bits[k] != SIJO_LEAF_STR) return sijo_rd_get(r, an->bits[k]);
    uint32_t id = sijo_rd_var(r);
    if (!id || r->bad) return 0;
    uint32_t n = sijo_rd_var(r);
    if (r->bad) return 0;
    if (n == 0) {
        if (!bv) { r->bad = 1; return 0; }
        if (id == bv) return id;
        /* a delta against the baseline's bytes */
        const SijoSTE* y = sijo_stab_find(stab, bv);
        uint32_t len = sijo_rd_var(r);
        if (!y || r->bad || len == 0 || len > SIJO_MAX_STR) { r->bad = 1; return 0; }
        char* tmp = (char*)sijo_xrealloc(NULL, len);
        uint32_t pos = 0;
        while (pos < len && !r->bad) {
            uint32_t op = sijo_rd_var(r), cnt = op >> 1;
            if (r->bad || cnt == 0 || cnt > len - pos) { r->bad = 1; break; }
            if (op & 1) {
                int64_t src = (int64_t)pos + sijo_unzz(sijo_rd_var(r));
                if (r->bad || src < 0 || src + cnt > (int64_t)y->n) { r->bad = 1; break; }
                memcpy(tmp + pos, y->p + src, cnt);
            } else {
                if ((size_t)cnt * 8 > r->nbits - r->bit) { r->bad = 1; break; }
                for (uint32_t i = 0; i < cnt; i++) tmp[pos + i] = (char)sijo_rd_get(r, 8);
            }
            pos += cnt;
        }
        if (!r->bad) sijo_stab_put(stab, id, tmp, (int32_t)len);
        free(tmp);
        return r->bad ? 0 : id;
    }
    if (n > SIJO_MAX_STR || (size_t)n * 8 > r->nbits - r->bit) { r->bad = 1; return 0; }
    char* tmp = (char*)sijo_xrealloc(NULL, n);
    for (uint32_t i = 0; i < n; i++) tmp[i] = (char)sijo_rd_get(r, 8);
    sijo_stab_put(stab, id, tmp, (int32_t)n);
    free(tmp);
    return id;
}
/* drop the strings no frame of a history refers to (every SIJO_HIST ticks) */
static void sijo_stab_mark_frames(SijoSTab* t, const SijoFrame* frames, int nframes) {
    for (int f = 0; f < nframes; f++) {
        const SijoFrame* fr = &frames[f];
        for (int32_t i = 0; i < fr->n; i++) {
            const SijoFEnt* e = &fr->e[i];
            const SijoArchNet* an = &sijo_archnet[e->arch];
            for (int k = 0; k < an->nleaf; k++)
                if (an->bits[k] == SIJO_LEAF_STR) sijo_stab_mark(t, fr->q[e->q + k]);
        }
    }
}
static void sijo_stab_gc(SijoSTab* t, const SijoFrame* frames, int nframes) {
    if (!t->n) return;
    if (++t->epoch == 0) t->epoch = 1;
    sijo_stab_mark_frames(t, frames, nframes);
    sijo_stab_sweep(t);
}

/* `cin` / `bin`: the entities of `cur` / `base` this client sees (NULL: all) */
static void sijo_delta_encode(SijoBW* b, const SijoFrame* cur, const SijoFrame* base, SijoDStats* st, const SijoSTab* stab, const SijoSet* cin, const SijoSet* bin) {
    int32_t i = 0, j = 0, nb = base ? base->n : 0;
    uint32_t last = 0;
    sijo_bw_reset(b);
    for (;;) {
        while (i < nb && !sijo_inset(bin, base->e[i].idx)) i++;
        while (j < cur->n && !sijo_inset(cin, cur->e[j].idx)) j++;
        if (!(i < nb || j < cur->n)) break;
        const SijoFEnt* be = i < nb ? &base->e[i] : NULL;
        const SijoFEnt* ce = j < cur->n ? &cur->e[j] : NULL;
        uint32_t bi = be ? be->idx : UINT32_MAX, ci = ce ? ce->idx : UINT32_MAX;
        if (bi < ci) {
            sijo_bw_put(b, 1, 2); sijo_bw_var(b, bi - last); last = bi;
            st->despawns++;
            i++;
            continue;
        }
        const SijoArchNet* an = &sijo_archnet[ce->arch];
        const uint32_t* cq = cur->q + ce->q;
        if (bi > ci || be->gen != ce->gen || be->arch != ce->arch) {
            /* new entity (or a new entity reusing the slot of a baseline entity) */
            sijo_bw_put(b, 2, 2); sijo_bw_var(b, ci - last); last = ci;
            sijo_bw_var(b, ce->gen);
            sijo_bw_put(b, ce->arch, SIJO_ARCH_BITS);
            for (int k = 0; k < an->nleaf; k++) sijo_put_leaf(b, an, k, cq[k], 0, stab, st);
            st->spawns++;
        } else {
            const uint32_t* bq = base->q + be->q;
            uint32_t mask = 0;
            for (int c = 0; c < an->ncomp; c++)
                for (int k = an->coff[c]; k < an->coff[c + 1]; k++)
                    if (cq[k] != bq[k]) { mask |= 1u << c; break; }
            if (mask) {
                sijo_bw_put(b, 3, 2); sijo_bw_var(b, ci - last); last = ci;
                sijo_bw_put(b, mask, an->ncomp);
                for (int c = 0; c < an->ncomp; c++)
                    if (mask & (1u << c))
                        for (int k = an->coff[c]; k < an->coff[c + 1]; k++) sijo_put_leaf(b, an, k, cq[k], bq[k], stab, st);
                st->updates++;
            }
        }
        if (bi == ci) i++;
        j++;
    }
    sijo_bw_put(b, 0, 2);
    sijo_bw_finish(b);
}

static void sijo_frame_copy_ent(SijoFrame* out, const SijoFrame* src, int32_t i) {
    const SijoFEnt* e = &src->e[i];
    int nl = sijo_archnet[e->arch].nleaf;
    uint32_t* q = sijo_frame_add(out, e->idx, e->gen, e->arch, nl);
    if (nl) memcpy(q, src->q + e->q, (size_t)nl * sizeof(uint32_t));
}

/* Rebuilds the sender's frame from `base` + payload. Returns 0 for malformed input. */
static int sijo_delta_decode(SijoRd* r, const SijoFrame* base, SijoFrame* out, int32_t tick, SijoSTab* stab) {
    int32_t i = 0, nb = base ? base->n : 0;
    uint32_t idx = 0;
    sijo_frame_reset(out, tick);
    for (;;) {
        uint32_t tag = sijo_rd_get(r, 2);
        if (r->bad) return 0;
        if (tag == 0) break;
        uint32_t d = sijo_rd_var(r);
        if (r->bad || d == 0 || d > SIJO_MAX_SLOT || idx + d > SIJO_MAX_SLOT) return 0;
        idx += d;
        while (i < nb && base->e[i].idx < idx) sijo_frame_copy_ent(out, base, i++);
        int has = i < nb && base->e[i].idx == idx;
        if (tag == 1) {
            if (!has) return 0;
            i++;
            continue;
        }
        if (tag == 2) {
            uint32_t gen = sijo_rd_var(r), arch = sijo_rd_get(r, SIJO_ARCH_BITS);
            if (r->bad || gen == 0 || arch >= SIJO_NARCH || !sijo_archnet[arch].repl) return 0;
            const SijoArchNet* an = &sijo_archnet[arch];
            uint32_t* q = sijo_frame_add(out, idx, gen, (int)arch, an->nleaf);
            for (int k = 0; k < an->nleaf; k++) q[k] = sijo_get_leaf(r, an, k, 0, stab);
            if (has) i++;
            continue;
        }
        if (!has) return 0;
        const SijoFEnt* be = &base->e[i++];
        const SijoArchNet* an = &sijo_archnet[be->arch];
        uint32_t mask = sijo_rd_get(r, an->ncomp);
        if (!mask) return 0;
        uint32_t* q = sijo_frame_add(out, idx, be->gen, be->arch, an->nleaf);
        const uint32_t* bq = base->q + be->q;
        for (int c = 0; c < an->ncomp; c++)
            for (int k = an->coff[c]; k < an->coff[c + 1]; k++) q[k] = (mask & (1u << c)) ? sijo_get_leaf(r, an, k, bq[k], stab) : bq[k];
    }
    while (i < nb) sijo_frame_copy_ent(out, base, i++);
    return !r->bad;
}

/* ---------------- transport: UDP sockets or an in-process loopback link ----------------
 * Every outgoing datagram can pass through a network-condition simulator (loss, duplication,
 * latency, jitter -> reordering) driven by a seeded PRNG, so runs are reproducible. */
/* an IPv6 address (IPv4 is stored IPv4-mapped, ::ffff:a.b.c.d) and a port, in network byte order */
typedef struct { uint8_t ip[16]; uint16_t port; } SijoAddr;
static inline int sijo_addr_eq(SijoAddr a, SijoAddr b) { return a.port == b.port && memcmp(a.ip, b.ip, 16) == 0; }
static inline int sijo_addr_is4(const SijoAddr* a) {
    static const uint8_t pre[12] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff};
    return memcmp(a->ip, pre, 12) == 0;
}
static inline void sijo_addr_set4(SijoAddr* a, const void* ip4, uint16_t port) {
    memset(a->ip, 0, 10);
    a->ip[10] = a->ip[11] = 0xff;
    memcpy(a->ip + 12, ip4, 4);
    a->port = port;
}
static const char* sijo_addr_str(SijoAddr a, char* buf, size_t n) {
    unsigned port = (unsigned)ntohs(a.port);
    if (sijo_addr_is4(&a)) {
        snprintf(buf, n, "%u.%u.%u.%u:%u", a.ip[12], a.ip[13], a.ip[14], a.ip[15], port);
        return buf;
    }
    /* [x:x:...]:port, the longest run of zero groups written as :: */
    unsigned g[8];
    for (int i = 0; i < 8; i++) g[i] = ((unsigned)a.ip[2 * i] << 8) | a.ip[2 * i + 1];
    int bs = -1, bl = 0;
    for (int i = 0; i < 8;) {
        if (g[i]) { i++; continue; }
        int j = i;
        while (j < 8 && !g[j]) j++;
        if (j - i > bl && j - i > 1) { bs = i; bl = j - i; }
        i = j;
    }
    char t[48];
    size_t o = 0;
    for (int i = 0; i < 8; i++) {
        if (i == bs) { o += (size_t)snprintf(t + o, sizeof t - o, "::"); i += bl - 1; continue; }
        o += (size_t)snprintf(t + o, sizeof t - o, "%s%x", (i && i != bs + bl) ? ":" : "", g[i]);
    }
    snprintf(buf, n, "[%s]:%u", t, port);
    return buf;
}

typedef struct { double due; uint64_t order; SijoAddr to, from; uint16_t len; uint8_t data[SIJO_WIRE_MTU]; } SijoDgram;
typedef struct { double loss, dup, lat_ms, jit_ms; uint64_t seed; } SijoSimCfg;

typedef struct SijoSock {
    int fd;                          /* -1: in-process loopback */
    struct SijoSock* peer;           /* loopback: where sent datagrams arrive */
    SijoAddr self;
    int fam;                         /* AF_INET6 (dual-stack) or AF_INET */
    SijoDgram* in; int32_t in_head, in_n, in_cap;   /* loopback inbox (ring) */
    SijoSimCfg sim; int sim_on; uint64_t rng, order;
    SijoDgram* dq; int32_t dq_n, dq_cap;            /* simulated in-flight datagrams: min-heap on (due, order) */
    uint64_t offered, sent, sent_bytes, recvd, recvd_bytes, sim_lost, sim_duped, overflow, errors;
    /* --key: every datagram sealed with XChaCha20-Poly1305 (header authenticated, body encrypted) */
    int crypt; uint8_t ckey[32]; uint8_t rkey[32]; uint64_t rctr; uint8_t rbuf[64]; int rused;
    uint64_t rejected;                /* datagrams that failed authentication (wrong key, tampering) */
} SijoSock;

static void sijo_sock_pair(SijoSock* a, SijoSock* b) {
    memset(a, 0, sizeof *a);
    memset(b, 0, sizeof *b);
    a->fd = b->fd = -1;
    a->peer = b; b->peer = a;
    uint32_t lo = htonl(0x7f000001u);
    sijo_addr_set4(&a->self, &lo, htons(1));
    sijo_addr_set4(&b->self, &lo, htons(2));
}
static void sijo_sock_free(SijoSock* s) {
    if (s->fd >= 0) sijo_sock_close(s->fd);
    free(s->in);
    free(s->dq);
    memset(s, 0, sizeof *s);
    s->fd = -1;
}
static void sijo_sock_sim(SijoSock* s, const SijoSimCfg* c, uint64_t salt) {
    s->sim = *c;
    s->sim_on = 1;
    s->rng = sijo_mix(c->seed ^ (salt * 0x9E3779B97F4A7C15ull));
}
/* a UDP socket: dual-stack IPv6 (reaches IPv4 peers through mapped addresses) when the system has
 * IPv6, IPv4 otherwise */
static int sijo_udp_open(SijoSock* s, int port) {
    memset(s, 0, sizeof *s);
    s->fd = -1;
    for (int attempt = 0; attempt < 2; attempt++) {
        int fam = attempt == 0 ? AF_INET6 : AF_INET;
        int fd = sijo_sock_open_udp(fam);
        if (fd < 0) continue;
        int sz = 4 << 20;
        setsockopt(fd, SOL_SOCKET, SO_RCVBUF, (const char*)&sz, sizeof sz);
        setsockopt(fd, SOL_SOCKET, SO_SNDBUF, (const char*)&sz, sizeof sz);
        SijoSockAddr u; /* union instead of pointer casts: the generated C stays alias-clean */
        memset(&u, 0, sizeof u);
        sijo_socklen len;
        if (fam == AF_INET6) {
            int off = 0;
            setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, (const char*)&off, sizeof off);
            u.in6.sin6_family = AF_INET6;
            u.in6.sin6_addr = in6addr_any;
            u.in6.sin6_port = htons((uint16_t)port);
            len = sizeof u.in6;
        } else {
            u.in.sin_family = AF_INET;
            u.in.sin_addr.s_addr = htonl(INADDR_ANY);
            u.in.sin_port = htons((uint16_t)port);
            len = sizeof u.in;
        }
        if (bind(fd, &u.sa, len) < 0) {
            int e = errno;
            sijo_sock_close(fd);
            errno = e;
            if (attempt == 0) continue;
            return -1;
        }
        sijo_sock_nonblock(fd);
        sijo_socklen al = sizeof u;
        getsockname(fd, &u.sa, &al);
        s->fd = fd;
        s->fam = fam;
        if (fam == AF_INET6) {
            memcpy(s->self.ip, &u.in6.sin6_addr, 16);
            s->self.port = u.in6.sin6_port;
        } else {
            sijo_addr_set4(&s->self, &u.in.sin_addr, u.in.sin_port);
        }
        return (int)ntohs(s->self.port);
    }
    return -1;
}
/* HOST:PORT, IPV4:PORT or [IPV6]:PORT */
static int sijo_resolve(const char* hostport, SijoAddr* out) {
    char host[256];
    const char* port;
    if (hostport[0] == '[') {
        const char* close = strchr(hostport, ']');
        if (!close || close[1] != ':' || !close[2] || (size_t)(close - hostport - 1) >= sizeof host) return 0;
        memcpy(host, hostport + 1, (size_t)(close - hostport - 1));
        host[close - hostport - 1] = 0;
        port = close + 2;
    } else {
        const char* colon = strrchr(hostport, ':');
        if (!colon || colon == hostport || (size_t)(colon - hostport) >= sizeof host || !colon[1]) return 0;
        if (memchr(hostport, ':', (size_t)(colon - hostport))) return 0; /* an IPv6 address needs [ ] */
        memcpy(host, hostport, (size_t)(colon - hostport));
        host[colon - hostport] = 0;
        port = colon + 1;
    }
    struct addrinfo hints, *res = NULL;
    memset(&hints, 0, sizeof hints);
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_DGRAM;
    if (getaddrinfo(host, port, &hints, &res) != 0 || !res) return 0;
    int ok = 0;
    for (struct addrinfo* r = res; r && !ok; r = r->ai_next) {
        SijoSockAddr u;
        memset(&u, 0, sizeof u);
        memcpy(&u, r->ai_addr, r->ai_addrlen < sizeof u ? r->ai_addrlen : sizeof u);
        if (u.sa.sa_family == AF_INET) {
            sijo_addr_set4(out, &u.in.sin_addr, u.in.sin_port);
            ok = 1;
        } else if (u.sa.sa_family == AF_INET6) {
            memcpy(out->ip, &u.in6.sin6_addr, 16);
            out->port = u.in6.sin6_port;
            ok = 1;
        }
    }
    freeaddrinfo(res);
    return ok;
}

static void sijo_inbox_push(SijoSock* s, SijoAddr from, const uint8_t* d, int len) {
    if (s->in_n == s->in_cap) {
        if (s->in_cap >= SIJO_QUEUE_MAX) { s->overflow++; return; }
        int32_t nc = s->in_cap ? s->in_cap * 2 : 64;
        SijoDgram* nd = (SijoDgram*)sijo_xrealloc(NULL, (size_t)nc * sizeof(SijoDgram));
        for (int32_t k = 0; k < s->in_n; k++) nd[k] = s->in[(s->in_head + k) % s->in_cap];
        free(s->in);
        s->in = nd; s->in_cap = nc; s->in_head = 0;
    }
    SijoDgram* g = &s->in[(s->in_head + s->in_n) % s->in_cap];
    g->from = from;
    g->len = (uint16_t)len;
    memcpy(g->data, d, (size_t)len);
    s->in_n++;
}
static void sijo_raw_send(SijoSock* s, SijoAddr to, const uint8_t* d, int len) {
    s->sent++;
    s->sent_bytes += (uint64_t)len;
    if (s->fd < 0) {
        if (s->peer) sijo_inbox_push(s->peer, s->self, d, len);
        return;
    }
    SijoSockAddr u;
    memset(&u, 0, sizeof u);
    sijo_socklen al;
    if (s->fam == AF_INET6) {
        u.in6.sin6_family = AF_INET6;
        memcpy(&u.in6.sin6_addr, to.ip, 16);
        u.in6.sin6_port = to.port;
        al = sizeof u.in6;
    } else {
        if (!sijo_addr_is4(&to)) { s->errors++; return; } /* an IPv6 peer, but this machine has no IPv6 */
        u.in.sin_family = AF_INET;
        memcpy(&u.in.sin_addr, to.ip + 12, 4);
        u.in.sin_port = to.port;
        al = sizeof u.in;
    }
    if (sendto(s->fd, (const char*)d, len, 0, &u.sa, al) < 0) s->errors++; /* full send buffer = loss */
}
static inline int sijo_dg_before(const SijoDgram* a, const SijoDgram* b) {
    return a->due < b->due || (a->due == b->due && a->order < b->order);
}
static void sijo_heap_push(SijoSock* s, const SijoDgram* g) {
    if (s->dq_n == s->dq_cap) {
        if (s->dq_cap >= SIJO_QUEUE_MAX) { s->overflow++; return; }
        s->dq_cap = s->dq_cap ? s->dq_cap * 2 : 64;
        s->dq = (SijoDgram*)sijo_xrealloc(s->dq, (size_t)s->dq_cap * sizeof(SijoDgram));
    }
    int32_t i = s->dq_n++;
    s->dq[i] = *g;
    while (i > 0) {
        int32_t p = (i - 1) / 2;
        if (!sijo_dg_before(&s->dq[i], &s->dq[p])) break;
        SijoDgram t = s->dq[i]; s->dq[i] = s->dq[p]; s->dq[p] = t;
        i = p;
    }
}
static void sijo_heap_pop(SijoSock* s, SijoDgram* out) {
    *out = s->dq[0];
    s->dq[0] = s->dq[--s->dq_n];
    int32_t i = 0;
    for (;;) {
        int32_t l = 2 * i + 1, r = l + 1, m = i;
        if (l < s->dq_n && sijo_dg_before(&s->dq[l], &s->dq[m])) m = l;
        if (r < s->dq_n && sijo_dg_before(&s->dq[r], &s->dq[m])) m = r;
        if (m == i) break;
        SijoDgram t = s->dq[i]; s->dq[i] = s->dq[m]; s->dq[m] = t;
        i = m;
    }
}
static double sijo_sim_u01(SijoSock* s) {
    s->rng += 0x9E3779B97F4A7C15ull;
    return (double)(sijo_mix(s->rng) >> 11) * (1.0 / 9007199254740992.0);
}
/* ---------------- encryption (--key) ----------------
 * A sealed datagram is the 7-byte header (kind, session, datagram number: readable, so it can be
 * routed, but authenticated), a random 24-byte nonce, the encrypted rest and a 16-byte tag. All
 * peers share the key; random 192-bit nonces never repeat in practice, so no counters need to be
 * coordinated. A datagram that does not authenticate is dropped (and counted). Replayed datagrams
 * are harmless: snapshots older than the newest are ignored, RPCs carry sequence numbers, a
 * repeated CONNECT gets the same ACCEPT. */
#define SIJO_HDR 7
static void sijo_seed_bytes(uint8_t* out, size_t n) {
    static uint64_t calls;
    uint64_t mix[4] = {(uint64_t)(sijo_now() * 1e9), sijo_pid(), (uint64_t)(uintptr_t)&mix, ++calls};
    size_t got = 0;
#ifndef _WIN32
    FILE* f = fopen("/dev/urandom", "rb");
    if (f) { got = fread(out, 1, n, f); fclose(f); }
#endif
    for (size_t i = got; i < n; i++) { /* no OS randomness: still unique per process and call */
        mix[i & 3] = sijo_mix(mix[i & 3] + 0x9E3779B97F4A7C15ull * (i + 1) + mix[(i + 1) & 3]);
        out[i] = (uint8_t)(mix[i & 3] >> 24);
    }
}
static void sijo_sock_key(SijoSock* s, const uint8_t key[32]) {
    s->crypt = 1;
    memcpy(s->ckey, key, 32);
    sijo_seed_bytes(s->rkey, 32); /* the nonce generator: ChaCha20 keystream under a random key */
    s->rctr = 0;
    s->rused = 64;
}
static void sijo_sock_nonce(SijoSock* s, uint8_t out[24]) {
    for (int i = 0; i < 24; i++) {
        if (s->rused == 64) {
            uint8_t n12[12] = {0};
            sijo_c_st32(n12 + 4, (uint32_t)(s->rctr >> 32));
            sijo_chacha_block(s->rkey, (uint32_t)s->rctr, n12, s->rbuf);
            s->rctr++;
            s->rused = 0;
        }
        out[i] = s->rbuf[s->rused++];
    }
}
static int sijo_seal(SijoSock* s, const uint8_t* d, int len, uint8_t* out) {
    if (len < SIJO_HDR) return -1;
    memcpy(out, d, SIJO_HDR);
    uint8_t* nonce = out + SIJO_HDR;
    sijo_sock_nonce(s, nonce);
    uint8_t* body = nonce + 24;
    size_t nb = (size_t)(len - SIJO_HDR);
    memcpy(body, d + SIJO_HDR, nb);
    sijo_xaead_seal(s->ckey, nonce, out, SIJO_HDR, body, nb, body + nb);
    return len + SIJO_SEAL;
}
/* in place: returns the plaintext length, or -1 for a datagram that does not authenticate */
static int sijo_unseal(SijoSock* s, uint8_t* d, int n) {
    if (n < SIJO_HDR + SIJO_SEAL || n > SIJO_WIRE_MTU) return -1;
    uint8_t* nonce = d + SIJO_HDR;
    uint8_t* body = nonce + 24;
    size_t nb = (size_t)(n - SIJO_HDR - SIJO_SEAL);
    if (!sijo_xaead_open(s->ckey, nonce, d, SIJO_HDR, body, nb, body + nb)) return -1;
    memmove(d + SIJO_HDR, body, nb);
    return (int)(SIJO_HDR + nb);
}
/* "--key" / SIJO_KEY: 64 hex digits */
static int sijo_parse_key(const char* h, uint8_t key[32]) {
    if (!h || strlen(h) != 64) return 0;
    for (int i = 0; i < 32; i++) {
        unsigned v = 0;
        for (int k = 0; k < 2; k++) {
            char c = h[2 * i + k];
            int x = c >= '0' && c <= '9' ? c - '0' : c >= 'a' && c <= 'f' ? c - 'a' + 10 : c >= 'A' && c <= 'F' ? c - 'A' + 10 : -1;
            if (x < 0) return 0;
            v = v * 16 + (unsigned)x;
        }
        key[i] = (uint8_t)v;
    }
    return 1;
}

/* send one datagram (through the simulator when enabled): each copy is lost independently and
   gets its own latency +- jitter, so duplicated and jittered datagrams also arrive reordered */
static void sijo_sock_send(SijoSock* s, SijoAddr to, const uint8_t* d, int len, double now) {
    if (len <= 0 || len > SIJO_MTU) return;
    uint8_t sealed[SIJO_WIRE_MTU];
    if (s->crypt) {
        len = sijo_seal(s, d, len, sealed);
        if (len < 0) return;
        d = sealed;
    }
    s->offered++;
    if (!s->sim_on) { sijo_raw_send(s, to, d, len); return; }
    int copies = 1;
    if (sijo_sim_u01(s) < s->sim.dup) { copies = 2; s->sim_duped++; }
    for (int c = 0; c < copies; c++) {
        if (sijo_sim_u01(s) < s->sim.loss) { s->sim_lost++; continue; }
        double delay = s->sim.lat_ms + s->sim.jit_ms * (2.0 * sijo_sim_u01(s) - 1.0);
        SijoDgram g;
        g.due = now + (delay > 0 ? delay : 0) / 1000.0;
        g.order = s->order++;
        g.to = to; g.from = s->self;
        g.len = (uint16_t)len;
        memcpy(g.data, d, (size_t)len);
        sijo_heap_push(s, &g);
    }
}
/* hand every simulated datagram whose delivery time has come to the real transport */
static void sijo_sock_pump(SijoSock* s, double now) {
    SijoDgram g;
    while (s->dq_n > 0 && s->dq[0].due <= now) {
        sijo_heap_pop(s, &g);
        sijo_raw_send(s, g.to, g.data, g.len);
    }
}
static double sijo_sock_next_due(const SijoSock* s) { return s->dq_n ? s->dq[0].due : 1e300; }
static int sijo_sock_recv_raw(SijoSock* s, SijoAddr* from, uint8_t* buf, int cap) {
    if (s->fd < 0) {
        if (!s->in_n) return -1;
        SijoDgram* g = &s->in[s->in_head];
        s->in_head = (s->in_head + 1) % s->in_cap;
        s->in_n--;
        int n = g->len < cap ? g->len : cap;
        memcpy(buf, g->data, (size_t)n);
        *from = g->from;
        s->recvd++;
        s->recvd_bytes += (uint64_t)n;
        return n;
    }
    for (;;) {
        SijoSockAddr u;
        memset(&u, 0, sizeof u);
        sijo_socklen al = sizeof u;
        int n = (int)recvfrom(s->fd, (char*)buf, cap, 0, &u.sa, &al);
        if (n < 0) {
            if (sijo_sock_retry()) continue;
            return -1;
        }
        if (u.sa.sa_family == AF_INET6 && al >= (sijo_socklen)sizeof u.in6) {
            memcpy(from->ip, &u.in6.sin6_addr, 16);
            from->port = u.in6.sin6_port;
        } else if (u.sa.sa_family == AF_INET && al >= (sijo_socklen)sizeof u.in) {
            sijo_addr_set4(from, &u.in.sin_addr, u.in.sin_port);
        } else {
            continue;
        }
        s->recvd++;
        s->recvd_bytes += (uint64_t)n;
        return (int)n;
    }
}
static int sijo_sock_recv(SijoSock* s, SijoAddr* from, uint8_t* buf, int cap) {
    for (;;) {
        int n = sijo_sock_recv_raw(s, from, buf, cap);
        if (n < 0 || !s->crypt) return n;
        int m = sijo_unseal(s, buf, n);
        if (m >= 0) return m;
        s->rejected++;
    }
}
static void sijo_sock_wait(SijoSock* s, double secs) {
    if (s->fd < 0 || secs <= 0) return;
    int ms = (int)ceil(secs * 1000.0);
    if (ms > 50) ms = 50;
    if (ms < 1) ms = 1;
    sijo_sock_poll_in(s->fd, ms);
}

/* receiver-side datagram statistics: unique / duplicate / loss, from 16-bit sequence numbers */
typedef struct { int init; int64_t max, min; uint64_t win, uniq, dups, late; } SijoSeqWin;
static void sijo_seq_note(SijoSeqWin* w, uint32_t s16) {
    if (!w->init) { w->init = 1; w->max = w->min = (int64_t)s16; w->win = 1; w->uniq = 1; return; }
    int32_t d = (int16_t)(uint16_t)(s16 - (uint32_t)(w->max & 0xffff));
    int64_t seq = w->max + d;
    if (d > 0) { w->win = d >= 64 ? 1 : (w->win << d) | 1; w->max = seq; w->uniq++; return; }
    if (-d >= 64) { w->late++; return; }
    uint64_t bit = 1ull << (-d);
    if (w->win & bit) { w->dups++; return; }
    w->win |= bit;
    w->uniq++;
    if (seq < w->min) w->min = seq;
}
/* loss over the last (up to) 64 datagrams, in percent */
static int sijo_seq_recent_loss(const SijoSeqWin* w) {
    if (!w->init) return 0;
    int64_t span = w->max - w->min + 1;
    if (span > 64) span = 64;
    uint64_t bits = span >= 64 ? w->win : (w->win & ((1ull << span) - 1));
    return (int)(100 * (span - __builtin_popcountll(bits)) / span);
}
static double sijo_seq_loss(const SijoSeqWin* w) {
    if (!w->init) return 0;
    double e = (double)(w->max - w->min + 1);
    double got = (double)(w->uniq + w->late);
    return e > got ? 1.0 - got / e : 0.0;
}

/* ======================= client ======================= */
typedef struct {
    int used, done, has_par;
    int32_t tick;
    uint32_t base, nfrag, got;
    uint32_t rack;                    /* newest RPC the server had executed when it sent this tick */
    size_t len;
    uint64_t* mask; size_t mcap;
    uint8_t* buf; size_t cap;
    uint8_t par[SIJO_FRAG];           /* parity fragment, when the server sent one */
} SijoAsm;
typedef struct { uint32_t seq, off, len, sends; } SijoRpcEnt;

typedef struct {
    SijoSock* sock; SijoAddr server;
    int state;                        /* 0 connecting, 1 connected */
    uint32_t nonce, session; int id, rate, reject; uint64_t seed; int32_t join_tick;
    SijoFrame frames[SIJO_HIST];      /* applied frames, by tick % SIJO_HIST: the possible baselines */
    SijoFrame scratch;
    SijoFrame shown, fill;            /* what the client world holds now; a reconstructed lost tick */
    int32_t applied;                  /* newest applied snapshot tick (-1: none) */
    uint8_t epoch; int resync, fails;
    SijoAsm as[SIJO_ASM_SLOTS];
    SijoCMap map;
    SijoRpcEnt* rq; int32_t rq_head, rq_n, rq_cap, rq_max;   /* reliable RPC queue (unacknowledged) */
    uint8_t* rqd; uint32_t rqd_n, rqd_cap;
    uint32_t rpc_seq, rpc_acked;
    int max_pkts, render_every;
    uint16_t pseq; SijoSeqWin rx; double last_recv;
    int bye, kicked; int32_t final_tick;
    uint64_t applied_n, complete, recovered, incomplete, superseded, frags, frag_dups, late_frags, nobase, malformed, dgrams, bytes;
    uint64_t sent_dgrams, sent_bytes, rpcs, rpc_resends, rpc_dropped, frames_run, filled;
    int view_on; float view[3];       /* Net.View of the last client frame, sent with every packet */
    /* client prediction: predictable RPCs run on the client world at once; after every applied
       frame the entities they touched get the frame's values back and the RPCs the server has not
       executed yet are run again on top, in order */
    int predict;
    struct { uint32_t seq, off, size; SijoH target; uint16_t fid; }* plog; int32_t plog_n, plog_cap;
    uint8_t* plogd; uint32_t plogd_n, plogd_cap;
    SijoH* touched; int32_t ntouched, touched_cap;
    uint32_t applied_rack, seen_rack;
    uint64_t predicted, replays;
} SijoClient;

static void sijo_cli_free(SijoClient* cl) {
    for (int k = 0; k < SIJO_HIST; k++) sijo_frame_free(&cl->frames[k]);
    sijo_frame_free(&cl->scratch);
    sijo_frame_free(&cl->shown);
    sijo_frame_free(&cl->fill);
    for (int k = 0; k < SIJO_ASM_SLOTS; k++) { free(cl->as[k].mask); free(cl->as[k].buf); }
    sijo_cmap_free(&cl->map);
    free(cl->plog);
    free(cl->plogd);
    free(cl->touched);
    free(cl->rq);
    free(cl->rqd);
    memset(cl, 0, sizeof *cl);
}
static void sijo_cli_init(SijoClient* cl, SijoSock* s, int udp) {
    memset(cl, 0, sizeof *cl);
    cl->sock = s;
    cl->applied = -1;
    cl->rpc_seq = 1;
    cl->rate = 60;
    cl->max_pkts = udp ? 16 : (1 << 30);   /* the loopback link delivers every RPC in the same tick */
    cl->rq_max = udp ? SIJO_RPC_QUEUE_UDP : (1 << 30);
    cl->predict = 1;
}
static void sijo_cli_touch(SijoClient* cl, SijoH h) {
    for (int32_t i = 0; i < cl->ntouched; i++) if (cl->touched[i].idx == h.idx && cl->touched[i].gen == h.gen) return;
    if (cl->ntouched == cl->touched_cap) {
        cl->touched_cap = cl->touched_cap ? cl->touched_cap * 2 : 16;
        cl->touched = (SijoH*)sijo_xrealloc(cl->touched, (size_t)cl->touched_cap * sizeof(SijoH));
    }
    cl->touched[cl->ntouched++] = h;
}
/* undo the predictions on the entities they touched (back to frame f), forget the RPCs the server
   has executed (seq <= rack) and run the others again */
static void sijo_cli_reconcile(SijoClient* cl, SijoWorld* C, const SijoFrame* f, uint32_t rack) {
    if (!cl->predict) return;
    if (rack > cl->applied_rack) cl->applied_rack = rack;
    int32_t k = 0;
    while (k < cl->plog_n && cl->plog[k].seq <= cl->applied_rack) k++;
    if (k) {
        memmove(cl->plog, cl->plog + k, (size_t)(cl->plog_n - k) * sizeof cl->plog[0]);
        cl->plog_n -= k;
        if (!cl->plog_n) cl->plogd_n = 0;
    }
    for (int32_t i = 0; i < cl->ntouched; i++) sijo_net_restore(C, &cl->map, f, cl->touched[i]);
    cl->ntouched = 0;
    SijoCmdBuf* b = &C->cmd[0];
    uint32_t n0 = b->n, d0 = b->dn;
    for (int32_t i = 0; i < cl->plog_n; i++) {
        SijoH h = cl->plog[i].target;
        if (!sijo_live(&C->ents, h)) continue;
        SijoCmd cmd;
        memset(&cmd, 0, sizeof cmd);
        cmd.kind = SIJO_CMD_CALL; cmd.target = h; cmd.src = SIJO_NULL; cmd.field = cl->plog[i].fid; cmd.size = (uint16_t)cl->plog[i].size;
        cmd.data = sijo_cmd_data(b, cl->plogd + cl->plog[i].off, cl->plog[i].size);
        sijo_exec_call(C, &cmd);
        sijo_cli_touch(cl, h);
        cl->replays++;
    }
    b->n = n0; b->dn = d0;
}

static void sijo_cli_rpc_ack(SijoClient* cl, uint32_t a) {
    if (a >= cl->rpc_seq || a <= cl->rpc_acked) return;
    cl->rpc_acked = a;
    while (cl->rq_head < cl->rq_n && cl->rq[cl->rq_head].seq <= a) cl->rq_head++;
    if (cl->rq_head == cl->rq_n) {
        cl->rq_head = cl->rq_n = 0;
        cl->rqd_n = 0;
    } else if (cl->rq_head >= 256 && cl->rq_head * 2 >= cl->rq_n) {
        uint32_t off0 = cl->rq[cl->rq_head].off;
        memmove(cl->rqd, cl->rqd + off0, cl->rqd_n - off0);
        cl->rqd_n -= off0;
        int32_t m = cl->rq_n - cl->rq_head;
        memmove(cl->rq, cl->rq + cl->rq_head, (size_t)m * sizeof(SijoRpcEnt));
        for (int32_t k = 0; k < m; k++) cl->rq[k].off -= off0;
        cl->rq_n = m;
        cl->rq_head = 0;
    }
}

/* client frame: client systems produced RPC requests (CALL commands): queue them reliably */
static void sijo_cli_collect_rpcs(SijoClient* cl, SijoWorld* c) {
    SijoCmdBuf* b = &c->cmd[0];
    for (int32_t i = 0; i < b->n; i++) {
        const SijoCmd* cm = &b->c[i];
        if (cm->kind != SIJO_CMD_CALL || !sijo_live(&c->ents, cm->target)) continue;
        uint32_t ci = cm->target.idx;
        if (ci >= (uint32_t)cl->map.rcap || !cl->map.rmap[ci]) continue;
        uint32_t si = cl->map.rmap[ci];
        if (cm->size > SIJO_RPC_MAX_ARGS || cl->rq_n - cl->rq_head >= cl->rq_max) { cl->rpc_dropped++; continue; }
        uint8_t tmp[SIJO_RPC_MAX_ARGS + 32];
        SijoBuf w = {tmp, 0, sizeof tmp};
        sijo_wvar(&w, si);
        sijo_wvar(&w, cl->map.sgen[si]);
        sijo_wvar(&w, cm->field);
        sijo_wvar(&w, cm->size);
        uint8_t args[SIJO_RPC_MAX_ARGS];
        if (cm->size) memcpy(args, b->data + cm->data, cm->size);
        sijo_rpc_canon(cm->field, args, cm->size); /* wire byte order */
        sijo_wbytes(&w, args, cm->size);
        if (cl->rq_n == cl->rq_cap) {
            cl->rq_cap = cl->rq_cap ? cl->rq_cap * 2 : 64;
            cl->rq = (SijoRpcEnt*)sijo_xrealloc(cl->rq, (size_t)cl->rq_cap * sizeof(SijoRpcEnt));
        }
        while (cl->rqd_n + w.n > cl->rqd_cap) {
            cl->rqd_cap = cl->rqd_cap ? cl->rqd_cap * 2 : 4096;
            cl->rqd = (uint8_t*)sijo_xrealloc(cl->rqd, cl->rqd_cap);
        }
        SijoRpcEnt* e = &cl->rq[cl->rq_n++];
        e->seq = cl->rpc_seq++;
        e->off = cl->rqd_n;
        e->len = (uint32_t)w.n;
        e->sends = 0;
        memcpy(cl->rqd + cl->rqd_n, tmp, w.n);
        cl->rqd_n += (uint32_t)w.n;
        cl->rpcs++;
        if (cl->predict && sijo_rpc_predictable(cm->field)) {
            /* run it here too, now: the client sees its own action without waiting for the server */
            if (cl->plog_n == cl->plog_cap) {
                cl->plog_cap = cl->plog_cap ? cl->plog_cap * 2 : 64;
                cl->plog = sijo_xrealloc(cl->plog, (size_t)cl->plog_cap * sizeof cl->plog[0]);
            }
            while (cl->plogd_n + cm->size > cl->plogd_cap) {
                cl->plogd_cap = cl->plogd_cap ? cl->plogd_cap * 2 : 1024;
                cl->plogd = (uint8_t*)sijo_xrealloc(cl->plogd, cl->plogd_cap);
            }
            cl->plog[cl->plog_n].seq = e->seq; cl->plog[cl->plog_n].off = cl->plogd_n; cl->plog[cl->plog_n].size = cm->size;
            cl->plog[cl->plog_n].target = cm->target; cl->plog[cl->plog_n].fid = cm->field;
            if (cm->size) memcpy(cl->plogd + cl->plogd_n, b->data + cm->data, cm->size);
            cl->plogd_n += cm->size;
            cl->plog_n++;
            sijo_exec_call(c, cm);
            sijo_cli_touch(cl, cm->target);
            cl->predicted++;
        }
    }
    b->n = 0;
    b->dn = 0;
}

/* acks + every unacknowledged RPC (split over several datagrams when they do not fit in one) */
static void sijo_cli_send(SijoClient* cl, double now) {
    int32_t k = cl->rq_head;
    int pk = 0;
    do {
        uint8_t pkt[SIJO_MTU];
        SijoBuf w = {pkt, 0, sizeof pkt};
        sijo_w8(&w, SIJO_PK_CLIENT); sijo_w32(&w, cl->session); sijo_w16(&w, cl->pseq++);
        sijo_w32(&w, (cl->applied >= 0 && !cl->resync) ? (uint32_t)cl->applied : SIJO_NONE);
        sijo_w8(&w, cl->epoch);
        sijo_w8(&w, (uint32_t)sijo_seq_recent_loss(&cl->rx));
        sijo_w8(&w, (uint32_t)cl->view_on);
        if (cl->view_on) for (int v = 0; v < 3; v++) sijo_w32(&w, sijo_f2bits(cl->view[v]));
        sijo_w32(&w, k < cl->rq_n ? cl->rq[k].seq : cl->rpc_seq);
        size_t cpos = w.n;
        sijo_w8(&w, 0);
        uint32_t cnt = 0;
        while (k < cl->rq_n && cnt < 255 && w.n + cl->rq[k].len <= SIJO_MTU) {
            sijo_wbytes(&w, cl->rqd + cl->rq[k].off, cl->rq[k].len);
            if (cl->rq[k].sends++) cl->rpc_resends++;
            k++;
            cnt++;
        }
        pkt[cpos] = (uint8_t)cnt;
        sijo_sock_send(cl->sock, cl->server, pkt, (int)w.n, now);
        cl->sent_dgrams++;
        cl->sent_bytes += w.n;
        if (!cnt) break; /* cannot happen (a record is at most SIJO_RPC_MAX_ARGS + 20 bytes), but never spin */
    } while (k < cl->rq_n && ++pk < cl->max_pkts);
}
static void sijo_cli_ctl(SijoClient* cl, int type, double now) {
    uint8_t pkt[16];
    SijoBuf w = {pkt, 0, sizeof pkt};
    sijo_w8(&w, (uint32_t)type); sijo_w32(&w, cl->session); sijo_w16(&w, cl->pseq++);
    sijo_sock_send(cl->sock, cl->server, pkt, (int)w.n, now);
    cl->sent_dgrams++;
    cl->sent_bytes += w.n;
}

/* fragment reassembly: a snapshot is identified by (tick, baseline, fragment count).
   Fragment index `fn` is the XOR parity of all fragments: with it, any single lost fragment is
   rebuilt. A rebuilt last fragment keeps its zero padding, which the decoder ignores (the payload
   ends with an explicit end record). */
static void sijo_cli_frag(SijoClient* cl, uint32_t tick, uint32_t base, uint32_t rack, uint32_t fi, uint32_t fn, const uint8_t* d, size_t len) {
    cl->frags++;
    int parity = fi == fn;
    if (fn == 0 || fi > fn || len == 0 || len > SIJO_FRAG || ((fi + 1 < fn || parity) && len != SIJO_FRAG) ||
        (size_t)fn * SIJO_FRAG > (size_t)SIJO_MAX_SNAPSHOT + SIJO_FRAG || tick > 0x7fffffffu) {
        cl->malformed++;
        return;
    }
    if ((int32_t)tick <= cl->applied) {
        if (base != tick) cl->late_frags++; /* base == tick: keep-alive while nothing changed */
        return;
    }
    SijoAsm* a = NULL;
    for (int k = 0; k < SIJO_ASM_SLOTS && !a; k++) {
        SijoAsm* s = &cl->as[k];
        if (s->used && s->tick == (int32_t)tick && s->base == base && s->nfrag == fn) a = s;
    }
    if (!a) {
        for (int k = 0; k < SIJO_ASM_SLOTS && !a; k++) if (!cl->as[k].used) a = &cl->as[k];
        if (!a) {
            SijoAsm* old = &cl->as[0];
            for (int k = 1; k < SIJO_ASM_SLOTS; k++) if (cl->as[k].tick < old->tick) old = &cl->as[k];
            if (old->tick > (int32_t)tick) { cl->late_frags++; return; } /* older than everything being reassembled */
            if (old->done) cl->superseded++;
            else cl->incomplete++;
            a = old;
        }
        size_t words = ((size_t)fn + 63) / 64, need = (size_t)fn * SIJO_FRAG;
        if (words > a->mcap) { a->mask = (uint64_t*)sijo_xrealloc(a->mask, words * 8); a->mcap = words; }
        if (need > a->cap) { a->buf = (uint8_t*)sijo_xrealloc(a->buf, need); a->cap = need; }
        memset(a->mask, 0, words * 8);
        a->used = 1; a->done = 0; a->has_par = 0; a->tick = (int32_t)tick; a->base = base; a->nfrag = fn; a->got = 0; a->len = 0;
        a->rack = 0;
    }
    if (rack != SIJO_NONE && rack > a->rack) a->rack = rack;
    if (parity) {
        if (a->done || a->has_par) { cl->frag_dups++; return; }
        memcpy(a->par, d, SIJO_FRAG);
        a->has_par = 1;
    } else {
        if (a->done || ((a->mask[fi / 64] >> (fi % 64)) & 1)) { cl->frag_dups++; return; }
        a->mask[fi / 64] |= 1ull << (fi % 64);
        uint8_t* dst = a->buf + (size_t)fi * SIJO_FRAG;
        memcpy(dst, d, len);
        if (len < SIJO_FRAG) memset(dst + len, 0, SIJO_FRAG - len); /* parity covers zero-padded fragments */
        if (fi + 1 == fn) a->len = (size_t)fi * SIJO_FRAG + len;
        a->got++;
    }
    if (a->got == fn) {
        a->done = 1;
        cl->complete++;
    } else if (a->has_par && a->got + 1 == fn) {
        uint32_t miss = 0;
        while ((a->mask[miss / 64] >> (miss % 64)) & 1) miss++;
        uint8_t* dst = a->buf + (size_t)miss * SIJO_FRAG;
        memcpy(dst, a->par, SIJO_FRAG);
        for (uint32_t f = 0; f < fn; f++) {
            if (f == miss) continue;
            const uint8_t* src = a->buf + (size_t)f * SIJO_FRAG;
            for (size_t k = 0; k < SIJO_FRAG; k++) dst[k] ^= src[k];
        }
        a->mask[miss / 64] |= 1ull << (miss % 64);
        if (miss + 1 == fn) a->len = (size_t)fn * SIJO_FRAG;
        a->got = fn;
        a->done = 1;
        cl->complete++;
        cl->recovered++;
    }
}

static void sijo_cli_packet(SijoClient* cl, SijoAddr from, const uint8_t* d, int n, double now) {
    if (!sijo_addr_eq(from, cl->server)) return;
    SijoRB r = {d, (size_t)n, 0, 0};
    uint32_t type = sijo_r8(&r), session = sijo_r32(&r), pseq = sijo_r16(&r);
    if (r.bad) return;
    if (cl->state == 0) {
        if (type == SIJO_PK_ACCEPT && session) {
            uint32_t nonce = sijo_r32(&r), id = sijo_r16(&r), rate = sijo_r16(&r);
            uint64_t seed = sijo_r64(&r);
            uint32_t tick = sijo_r32(&r);
            if (r.bad || nonce != cl->nonce) return;
            cl->session = session; cl->id = (int)id; cl->rate = rate ? (int)rate : 60; cl->seed = seed;
            cl->join_tick = (int32_t)tick; cl->state = 1; cl->last_recv = now;
            sijo_seq_note(&cl->rx, pseq);
        } else if (type == SIJO_PK_REJECT && session == cl->nonce) {
            uint32_t why = sijo_r8(&r);
            cl->reject = (r.bad || !why) ? 9 : (int)why;
        }
        return;
    }
    if (session != cl->session) return;
    cl->last_recv = now;
    cl->dgrams++;
    cl->bytes += (uint64_t)n;
    sijo_seq_note(&cl->rx, pseq);
    switch (type) {
    case SIJO_PK_SNAP: {
        uint32_t tick = sijo_r32(&r), base = sijo_r32(&r), rack = sijo_r32(&r), fi = sijo_r16(&r), fn = sijo_r16(&r);
        if (r.bad) return;
        sijo_cli_rpc_ack(cl, rack);
        /* a re-sent copy of the frame we already show, with RPCs the server took since: reconcile */
        if ((int32_t)tick == cl->applied && rack != SIJO_NONE && rack > cl->seen_rack) cl->seen_rack = rack;
        sijo_cli_frag(cl, tick, base, rack, fi, fn, d + r.pos, (size_t)n - r.pos);
        break;
    }
    case SIJO_PK_BYE: {
        uint32_t ft = sijo_r32(&r);
        if (!r.bad) { cl->bye = 1; cl->final_tick = (int32_t)ft; }
        break;
    }
    case SIJO_PK_DISCONNECT: cl->kicked = 1; break;
    default: break;
    }
}
static void sijo_cli_recv(SijoClient* cl, double now) {
    uint8_t buf[2048];
    SijoAddr from;
    for (;;) {
        int n = sijo_sock_recv(cl->sock, &from, buf, (int)sizeof buf);
        if (n < 0) break;
        if (n <= SIJO_MTU) sijo_cli_packet(cl, from, buf, n, now);
    }
}

static void sijo_cli_frame(SijoClient* cl, SijoWorld* C);
/* A lost tick between frames a and b: a's entities, with the leaves that allow it interpolated
   toward b (exact integer / IEEE arithmetic, so every client computes the same values). */
static void sijo_frame_lerp(SijoFrame* out, const SijoFrame* a, const SijoFrame* b, int32_t tick) {
    sijo_frame_reset(out, tick);
    int64_t num = (int64_t)tick - a->tick, den = (int64_t)b->tick - a->tick;
    int32_t j = 0;
    for (int32_t i = 0; i < a->n; i++) {
        const SijoFEnt* ea = &a->e[i];
        const SijoArchNet* an = &sijo_archnet[ea->arch];
        uint32_t* q = sijo_frame_add(out, ea->idx, ea->gen, ea->arch, an->nleaf);
        const uint32_t* qa = a->q + ea->q;
        if (an->nleaf) memcpy(q, qa, (size_t)an->nleaf * sizeof(uint32_t));
        while (j < b->n && b->e[j].idx < ea->idx) j++;
        if (j >= b->n || b->e[j].idx != ea->idx || b->e[j].gen != ea->gen || b->e[j].arch != ea->arch || den <= 0) continue;
        const uint32_t* qb = b->q + b->e[j].q;
        for (int k = 0; k < an->nleaf; k++) {
            if (an->lerp[k] == 1) {
                int64_t d = (int64_t)qb[k] - (int64_t)qa[k];
                int64_t r = d * num;
                q[k] = (uint32_t)((int64_t)qa[k] + (r >= 0 ? (r + den / 2) / den : -((-r + den / 2) / den)));
            } else if (an->lerp[k] == 2) {
                float fa = sijo_bits2f(qa[k]), fb = sijo_bits2f(qb[k]);
                if (fa == fa && fb == fb) q[k] = sijo_f2bits(fa + (fb - fa) * ((float)num / (float)den));
            }
        }
    }
}
static void sijo_frame_copy(SijoFrame* dst, const SijoFrame* src) {
    sijo_frame_reset(dst, src->tick);
    for (int32_t i = 0; i < src->n; i++) sijo_frame_copy_ent(dst, src, i);
}
#define SIJO_MAX_FILL 32              /* lost ticks a client reconstructs before giving up on a gap */

/* decode a complete snapshot against its baseline and move the client world to the new frame;
   ticks lost since the previous snapshot first run their client frames on interpolated values */
static int sijo_cli_apply(SijoClient* cl, SijoWorld* C, SijoAsm* a) {
    static const SijoFrame none;
    const SijoFrame* base = NULL;
    if (a->base != SIJO_NONE) {
        const SijoFrame* b = &cl->frames[a->base % SIJO_HIST];
        if (!b->valid || (uint32_t)b->tick != a->base) { cl->nobase++; return 0; }
        base = b;
    }
    SijoRd rd = {a->buf, a->len * 8, 0, 0};
    if (!sijo_delta_decode(&rd, base, &cl->scratch, a->tick, &C->stab)) { cl->malformed++; return 0; }
    const SijoFrame* prev = cl->applied >= 0 ? &cl->frames[(uint32_t)cl->applied % SIJO_HIST] : &none;
    if (cl->applied >= 0 && a->tick - cl->applied > 1 && a->tick - cl->applied <= SIJO_MAX_FILL + 1) {
        for (int32_t t = cl->applied + 1; t < a->tick; t++) {
            sijo_frame_lerp(&cl->fill, prev, &cl->scratch, t);
            sijo_net_apply(C, &cl->map, &cl->shown, &cl->fill);
            sijo_frame_swap(&cl->shown, &cl->fill);
            sijo_cli_reconcile(cl, C, &cl->shown, cl->applied_rack);
            sijo_cli_frame(cl, C);
            cl->filled++;
        }
    }
    sijo_net_apply(C, &cl->map, &cl->shown, &cl->scratch);
    sijo_frame_copy(&cl->shown, &cl->scratch);
    sijo_cli_reconcile(cl, C, &cl->shown, a->rack);
    SijoFrame* slot = &cl->frames[(uint32_t)a->tick % SIJO_HIST];
    sijo_frame_swap(slot, &cl->scratch);
    slot->valid = 1;
    cl->applied = a->tick;
    cl->applied_n++;
    if ((cl->applied_n & (SIJO_HIST - 1)) == 0) sijo_stab_gc(&C->stab, cl->frames, SIJO_HIST);
    cl->resync = 0;
    cl->fails = 0;
    return 1;
}
static void sijo_cli_frame(SijoClient* cl, SijoWorld* C) {
    sijo_client_order(C, &cl->map); /* rows in server-slot order: the same frame on every client */
    sijo_client_frame(C);
    cl->view_on = C->view_on;
    cl->view[0] = C->view_x; cl->view[1] = C->view_y; cl->view[2] = C->view_r;
    sijo_cli_collect_rpcs(cl, C);
    if (cl->render_every > 0 && C->tick % cl->render_every == 0) {
        printf("tick %d (client %d)\n", C->tick, cl->id);
        sijo_render_ascii(&C->draws, 64, 18);
        fflush(stdout);
    }
    sijo_reset_arenas(C);
    cl->frames_run++;
}
/* apply every complete snapshot newer than the current state, oldest first; one client frame each */
static int sijo_cli_update(SijoClient* cl, SijoWorld* C) {
    int n = 0;
    for (;;) {
        SijoAsm* best = NULL;
        for (int k = 0; k < SIJO_ASM_SLOTS; k++) {
            SijoAsm* a = &cl->as[k];
            if (!a->used) continue;
            if (a->tick <= cl->applied) {
                if (a->done) cl->superseded++;
                else cl->incomplete++; /* a fragment was lost; a newer snapshot already replaced it */
                a->used = 0;
                continue;
            }
            if (a->done && (!best || a->tick < best->tick)) best = a;
        }
        if (!best) {
            if (cl->seen_rack > cl->applied_rack && cl->applied >= 0) sijo_cli_reconcile(cl, C, &cl->shown, cl->seen_rack);
            break;
        }
        best->used = 0;
        if (sijo_cli_apply(cl, C, best)) {
            n++;
            sijo_cli_frame(cl, C);
        } else if (++cl->fails >= 8 && !cl->resync) {
            /* cannot decode against our baselines: ask for full state (new epoch) */
            cl->resync = 1;
            cl->epoch++;
            for (int k = 0; k < SIJO_HIST; k++)
                if (cl->applied < 0 || k != (int)((uint32_t)cl->applied % SIJO_HIST)) cl->frames[k].valid = 0;
        }
    }
    return n;
}
static void sijo_cli_step(SijoClient* cl, SijoWorld* C, double now) {
    sijo_cli_recv(cl, now);
    sijo_cli_update(cl, C);
    sijo_cli_send(cl, now);
}
static void sijo_cli_wait(SijoClient* cl, double until) {
    for (;;) {
        double now = sijo_now();
        sijo_sock_pump(cl->sock, now);
        sijo_cli_recv(cl, now);
        if (now >= until) return;
        double t = until, due = sijo_sock_next_due(cl->sock);
        if (due < t) t = due;
        sijo_sock_wait(cl->sock, t - now);
    }
}

/* ======================= server ======================= */
typedef struct {
    int used, state;                  /* state 1: playing, 2: finished (goodbye acknowledged or given up) */
    int id; SijoAddr addr; uint32_t nonce, session;
    int32_t acked;                    /* newest snapshot tick the client acknowledged (-1: none) */
    int32_t joined, age_max;
    uint8_t epoch;
    uint32_t rpc_next, rpc_tick;
    uint16_t pseq; SijoSeqWin rx;
    double last_recv;
    int confirmed;                    /* the client sent a packet after our ACCEPT: start sending snapshots */
    int loss_pct;                     /* recent server->client loss the client reports: enables parity */
    int byes, converged;
    uint64_t rounds, snaps, full, dgrams, bytes, payload, parity, age_sum, rpcs, rpc_dups, rpc_bad, rpc_held, resyncs, oversize;
    /* interest management: the client's Net.View and, per sent tick, how it was sent: stick[k] is
       that tick (-2: none), sview[k] 1 when it was filtered by the slots sets[k], 0 for everything.
       The mode is decided when the tick is first sent and kept for re-sends of it */
    int view_on; float view[3];
    uint64_t* sets; int32_t swords; int32_t stick[SIJO_HIST]; uint8_t sview[SIJO_HIST];
    uint64_t hidden;                  /* entity records left out because the client did not see them */
    /* bandwidth budget: per sent tick, the frame the client has once it applied that snapshot
       (ctick[k] == the tick), the newest such tick, and per slot how many ticks a change waited */
    SijoFrame* cf; int32_t ctick[SIJO_HIST]; int32_t dtick;
    uint32_t* wait; int32_t nwait;
    uint64_t deferred; uint32_t wait_max, pay_max;
} SijoPeer;

typedef struct { int valid, has_par; int32_t tick; uint32_t base; SijoBW bw; SijoDStats st; uint8_t par[SIJO_FRAG]; } SijoEnc;

typedef struct {
    SijoSock* sock; int udp, quiet, rate, max_clients, rpc_limit;
    uint64_t seed;
    double timeout;
    SijoFrame hist[SIJO_HIST];        /* captured frames by tick % SIJO_HIST, shared by all clients */
    int32_t tick;                     /* newest captured frame */
    SijoRepl repl;
    const SijoSTab* stab;             /* the server world's replicated strings (set by capture) */
    SijoWorld* world;                 /* the server world (positions for interest management) */
    SijoPeer* peer; int npeer, next_id;
    SijoEnc enc[4]; int enc_next;     /* encodings of this tick, reused by clients with the same baseline */
    int flushing; int32_t final_tick;
    /* bandwidth budget (--budget): payload bytes per snapshot and client while the game runs
       (ticking); the last tick (last_tick, -1: not known yet) and the flush go in full */
    int budget, ticking; int32_t last_tick;
    struct SijoCand* cand; int32_t* ord; int32_t ncand, ccap;
    uint64_t deferred;
    uint64_t rng;
    uint64_t payload, naive, snaps, rpcs, rpc_bytes, joins, leaves, stray;
    SijoDStats st;
} SijoServer;

static void sijo_peer_free_budget(SijoPeer* p) {
    if (p->cf) for (int k = 0; k < SIJO_HIST; k++) sijo_frame_free(&p->cf[k]);
    free(p->cf); p->cf = NULL;
    free(p->wait); p->wait = NULL; p->nwait = 0;
}
static void sijo_srv_free(SijoServer* sv) {
    for (int i = 0; i < sv->npeer; i++) { free(sv->peer[i].sets); sijo_peer_free_budget(&sv->peer[i]); }
    free(sv->cand); free(sv->ord);
    for (int k = 0; k < SIJO_HIST; k++) sijo_frame_free(&sv->hist[k]);
    sijo_repl_free(&sv->repl);
    free(sv->peer);
    for (int k = 0; k < 4; k++) free(sv->enc[k].bw.buf);
    memset(sv, 0, sizeof *sv);
}
static void sijo_srv_init(SijoServer* sv, SijoSock* s, int udp, int rate, int max_clients, double timeout, uint64_t seed, int quiet) {
    memset(sv, 0, sizeof *sv);
    sv->sock = s; sv->udp = udp; sv->rate = rate; sv->max_clients = max_clients; sv->timeout = timeout;
    sv->seed = seed; sv->quiet = quiet;
    sv->rpc_limit = udp ? 256 : 0; /* per client per tick; the rest stay unacknowledged and are re-sent */
    sv->last_tick = -1;
    sv->rng = sijo_mix((uint64_t)(sijo_now() * 1e9) ^ (sijo_pid() << 32));
}
static SijoPeer* sijo_srv_find_session(SijoServer* sv, SijoAddr a, uint32_t session) {
    for (int i = 0; i < sv->npeer; i++) if (sv->peer[i].used && sv->peer[i].session == session && sijo_addr_eq(sv->peer[i].addr, a)) return &sv->peer[i];
    return NULL;
}
/* playing clients; with `confirmed`, only those whose first packet after ACCEPT arrived */
static int sijo_srv_count(const SijoServer* sv, int confirmed) {
    int n = 0;
    for (int i = 0; i < sv->npeer; i++) n += sv->peer[i].used && sv->peer[i].state == 1 && (!confirmed || sv->peer[i].confirmed);
    return n;
}
static void sijo_srv_capture(SijoServer* sv, SijoWorld* S) {
    SijoFrame* f = &sv->hist[(uint32_t)S->tick % SIJO_HIST];
    sijo_net_capture(S, &sv->repl, f, S->tick);
    sv->stab = &S->stab;
    sv->world = S;
    f->valid = 1;
    if ((S->tick & (SIJO_HIST - 1)) == 0 && S->stab.n) {
        /* strings still referenced by a frame of the history, or by a frame some client was sent
           (a change held back by the bandwidth budget keeps an older value) */
        if (++S->stab.epoch == 0) S->stab.epoch = 1;
        sijo_stab_mark_frames(&S->stab, sv->hist, SIJO_HIST);
        for (int i = 0; i < sv->npeer; i++)
            if (sv->peer[i].used && sv->peer[i].cf) sijo_stab_mark_frames(&S->stab, sv->peer[i].cf, SIJO_HIST);
        sijo_stab_sweep(&S->stab);
    }
    sv->tick = S->tick;
    for (int k = 0; k < 4; k++) sv->enc[k].valid = 0;
}
static void sijo_srv_new_tick(SijoServer* sv) {
    for (int i = 0; i < sv->npeer; i++) sv->peer[i].rpc_tick = 0;
}
static void sijo_srv_send(SijoServer* sv, SijoPeer* p, const uint8_t* pkt, size_t n, double now) {
    sijo_sock_send(sv->sock, p->addr, pkt, (int)n, now);
    p->dgrams++;
    p->bytes += n;
}
static void sijo_srv_ctl(SijoServer* sv, SijoPeer* p, int type, uint32_t arg, double now) {
    uint8_t pkt[16];
    SijoBuf w = {pkt, 0, sizeof pkt};
    sijo_w8(&w, (uint32_t)type); sijo_w32(&w, p->session); sijo_w16(&w, p->pseq++); sijo_w32(&w, arg);
    sijo_srv_send(sv, p, pkt, w.n, now);
}

/* one snapshot of the newest frame for one client: a delta against the newest frame the client
   acknowledged (full state when it acknowledged nothing yet or its ack fell out of the history) */
/* whether `tick` was sent to the client filtered by its view (its slots are then in sets) */
static inline int sijo_peer_filtered(const SijoPeer* p, int32_t tick) {
    uint32_t k = (uint32_t)tick % SIJO_HIST;
    return tick >= 0 && p->stick[k] == tick && p->sview[k];
}
/* the slots a client with a view sees at `tick`: entities within its radius (kept until 10% beyond
   it, so an entity at the edge does not flicker), and every entity without a position */
static const uint64_t* sijo_peer_set(SijoServer* sv, SijoPeer* p, int32_t tick) {
    SijoWorld* S = sv->world;
    int32_t words = (S->ents.n + 63) / 64;
    if (words < 1) words = 1;
    if (words > p->swords) {
        uint64_t* ns = (uint64_t*)calloc((size_t)SIJO_HIST * (size_t)words, sizeof(uint64_t));
        if (!ns) sijo_panic("out of memory");
        for (int k = 0; k < SIJO_HIST && p->sets; k++) memcpy(ns + (size_t)k * words, p->sets + (size_t)k * p->swords, (size_t)p->swords * sizeof(uint64_t));
        free(p->sets);
        p->sets = ns;
        p->swords = words;
    }
    uint64_t* cur = p->sets + (size_t)((uint32_t)tick % SIJO_HIST) * p->swords;
    /* a tick's set is decided when the tick is first sent: re-sends of the same tick (the final
       frame while the server flushes) must describe exactly what the client may have received */
    if (sijo_peer_filtered(p, tick)) return cur;
    int32_t pt = tick - 1;
    const uint64_t* prev = pt >= 0 && sijo_peer_filtered(p, pt) ? p->sets + (size_t)((uint32_t)pt % SIJO_HIST) * p->swords : NULL;
    memset(cur, 0, (size_t)p->swords * sizeof(uint64_t));
    float r2 = p->view[2] * p->view[2], keep = 1.1f * p->view[2], k2 = keep * keep;
    for (int32_t i = 1; i < S->ents.n; i++) {
        if (S->ents.s[i].state != SIJO_ALIVE) continue;
        float x, y;
        int in = 1;
        if (sijo_net_pos(S, (uint32_t)i, &x, &y)) {
            float dx = x - p->view[0], dy = y - p->view[1], d = dx * dx + dy * dy;
            in = d <= r2 || (prev && ((prev[i >> 6] >> (i & 63)) & 1) && d <= k2);
        }
        if (in) cur[i >> 6] |= 1ull << (i & 63);
    }
    p->stick[(uint32_t)tick % SIJO_HIST] = tick;
    p->sview[(uint32_t)tick % SIJO_HIST] = 1;
    return cur;
}
/* ---------------- bandwidth budget (--budget BYTES) ----------------
 * Each client keeps its own history of frames: cf[tick] is what the client has once it applied
 * the snapshot of that tick. The next frame starts from the newest one (so the client never goes
 * back in time): removed entities always go, and every entity whose state changed is either sent
 * fresh or held at the value the client last got. A snapshot is a delta against the acknowledged
 * frame, so whatever is still on its way costs bytes again until it is acknowledged; that part is
 * paid first, then fresh changes in priority order (ticks the change waited x nearness to the
 * client's view; entities without a position count as near), stopping at the first that does not
 * fit. The last tick and the flush are not budgeted, so a run ends converged. */
struct SijoCand { uint32_t idx; int64_t cost; float score; int sel; };
typedef struct SijoCand SijoCand;
static inline int sijo_var_bits(uint32_t v) { int n = 8; while (v >= 128) { v >>= 7; n += 8; } return n; }
static int64_t sijo_leaf_bits(const SijoArchNet* an, int k, uint32_t v, uint32_t bv, const SijoSTab* stab) {
    if (an->bits[k] != SIJO_LEAF_STR) return an->bits[k];
    return sijo_ileaf_bits(v, bv, stab, NULL);
}
/* bits of the record that turns baseline entity `b` (of frame bf) into `t` (of frame tf); either may
   be NULL. `gap`: about the slot distance to the previous record */
static int64_t sijo_rec_bits(const SijoFrame* tf, const SijoFEnt* t, const SijoFrame* bf, const SijoFEnt* b, uint32_t gap, const SijoSTab* stab) {
    if (!t) return b ? 2 + sijo_var_bits(gap) : 0;
    const SijoArchNet* an = &sijo_archnet[t->arch];
    const uint32_t* tq = tf->q + t->q;
    int64_t n = 2 + sijo_var_bits(gap);
    if (!b || b->gen != t->gen || b->arch != t->arch) {
        n += sijo_var_bits(t->gen) + SIJO_ARCH_BITS;
        for (int k = 0; k < an->nleaf; k++) n += sijo_leaf_bits(an, k, tq[k], 0, stab);
        return n;
    }
    const uint32_t* bq = bf->q + b->q;
    int any = 0;
    for (int c = 0; c < an->ncomp; c++) {
        int ch = 0;
        for (int k = an->coff[c]; k < an->coff[c + 1] && !ch; k++) ch = tq[k] != bq[k];
        if (!ch) continue;
        any = 1;
        for (int k = an->coff[c]; k < an->coff[c + 1]; k++) n += sijo_leaf_bits(an, k, tq[k], bq[k], stab);
    }
    return any ? n + an->ncomp : 0;
}
static int sijo_fent_same(const SijoFrame* af, const SijoFEnt* a, const SijoFrame* bf, const SijoFEnt* b) {
    if (!a || !b || a->gen != b->gen || a->arch != b->arch) return 0;
    int nl = sijo_archnet[a->arch].nleaf;
    return !nl || memcmp(af->q + a->q, bf->q + b->q, (size_t)nl * sizeof(uint32_t)) == 0;
}
static const SijoFrame* sijo_peer_frame(const SijoPeer* p, int32_t tick) {
    if (tick < 0 || !p->cf || p->ctick[(uint32_t)tick % SIJO_HIST] != tick) return NULL;
    return &p->cf[(uint32_t)tick % SIJO_HIST];
}
static int sijo_cand_by_score(const void* a, const void* b) {
    const SijoCand* x = (const SijoCand*)a;
    const SijoCand* y = (const SijoCand*)b;
    if (x->score != y->score) return x->score > y->score ? -1 : 1;
    return x->idx < y->idx ? -1 : x->idx > y->idx;
}
static int sijo_cand_by_slot(const void* a, const void* b) {
    const SijoCand* x = (const SijoCand*)a;
    const SijoCand* y = (const SijoCand*)b;
    return x->idx < y->idx ? -1 : x->idx > y->idx;
}
/* one pass over the slots of the client's newest frame `prev` and of `cur` (only what the client
   sees), in slot order; `base` (the acknowledged frame) is looked up alongside */
typedef struct {
    const SijoFrame *prev, *cur, *base; const SijoSet* cin;
    int32_t i, j, b; uint32_t last;
    uint32_t s, gap; const SijoFEnt *pe, *ce, *be, *he;
} SijoBWalk;
static int sijo_bwalk_next(SijoBWalk* w) {
    int32_t np = w->prev ? w->prev->n : 0, nb = w->base ? w->base->n : 0;
    if (w->pe) w->i++;
    if (w->ce) w->j++;
    while (w->j < w->cur->n && !sijo_inset(w->cin, w->cur->e[w->j].idx)) w->j++;
    if (w->i >= np && w->j >= w->cur->n) return 0;
    uint32_t pi = w->i < np ? w->prev->e[w->i].idx : UINT32_MAX, ci = w->j < w->cur->n ? w->cur->e[w->j].idx : UINT32_MAX;
    w->s = pi < ci ? pi : ci;
    w->pe = pi == w->s ? &w->prev->e[w->i] : NULL;
    w->ce = ci == w->s ? &w->cur->e[w->j] : NULL;
    while (w->b < nb && w->base->e[w->b].idx < w->s) w->b++;
    w->be = w->b < nb && w->base->e[w->b].idx == w->s ? &w->base->e[w->b] : NULL;
    /* held: what the client keeps while this slot's change waits (an entity that is gone, or whose
       slot now holds another entity, is removed at once) */
    w->he = w->pe && w->ce && w->pe->gen == w->ce->gen && w->pe->arch == w->ce->arch ? w->pe : NULL;
    w->gap = w->s - w->last;
    w->last = w->s;
    return 1;
}
static void sijo_budget_frame(SijoServer* sv, SijoPeer* p, const SijoFrame* cur, const SijoSet* cin, const SijoFrame* base, int budgeted) {
    if (!p->cf) {
        p->cf = (SijoFrame*)calloc(SIJO_HIST, sizeof(SijoFrame));
        if (!p->cf) sijo_panic("out of memory");
    }
    const SijoFrame* prev = p->dtick < sv->tick ? sijo_peer_frame(p, p->dtick) : NULL;
    if (!prev) prev = base;
    SijoFrame* out = &p->cf[(uint32_t)sv->tick % SIJO_HIST];
    const SijoSTab* stab = sv->stab;
    float r = p->view[2] > 0 ? p->view[2] : 1;
    int64_t must = 0;
    sv->ncand = 0;
    SijoBWalk w;
    memset(&w, 0, sizeof w);
    w.prev = prev; w.cur = cur; w.base = base; w.cin = cin;
    while (sijo_bwalk_next(&w)) {
        int64_t hold = sijo_rec_bits(prev, w.he, base, w.be, w.gap, stab);
        must += hold;
        if (!w.ce || sijo_fent_same(cur, w.ce, prev, w.he)) continue;
        if (sv->ncand == sv->ccap) {
            sv->ccap = sv->ccap ? sv->ccap * 2 : 256;
            sv->cand = (SijoCand*)sijo_xrealloc(sv->cand, (size_t)sv->ccap * sizeof(SijoCand));
        }
        SijoCand* c = &sv->cand[sv->ncand++];
        c->idx = w.s;
        c->cost = sijo_rec_bits(cur, w.ce, base, w.be, w.gap, stab) - hold;
        c->sel = !budgeted;
        if (budgeted) {
            float wt = 1, x, y;
            if (p->view_on && sv->world) {
                wt = 4;
                if (sijo_net_pos(sv->world, w.s, &x, &y)) {
                    float dx = x - p->view[0], dy = y - p->view[1], d = sqrtf(dx * dx + dy * dy) / r;
                    wt = d >= 1 ? 1 : 1 + 3 * (1 - d);
                }
            }
            uint32_t waited = w.s < (uint32_t)p->nwait ? p->wait[w.s] : 0;
            c->score = (float)(waited + 1) * wt;
        }
    }
    if (budgeted && sv->ncand) {
        int64_t limit = (int64_t)sv->budget * 8 - 2, total = must;
        int any = 0;
        qsort(sv->cand, (size_t)sv->ncand, sizeof(SijoCand), sijo_cand_by_score);
        for (int32_t k = 0; k < sv->ncand; k++) {
            SijoCand* c = &sv->cand[k];
            /* the first change goes even when it alone is over the budget, unless older ones are
               still on their way (their acknowledgement makes room) */
            if (total + c->cost > limit && (any || must > 0)) break;
            c->sel = 1;
            total += c->cost;
            any = 1;
        }
        qsort(sv->cand, (size_t)sv->ncand, sizeof(SijoCand), sijo_cand_by_slot);
    }
    sijo_frame_reset(out, sv->tick);
    memset(&w, 0, sizeof w);
    w.prev = prev; w.cur = cur; w.base = base; w.cin = cin;
    int32_t k = 0;
    while (sijo_bwalk_next(&w)) {
        if (w.s >= (uint32_t)p->nwait) {
            int32_t nn = p->nwait ? p->nwait : 256;
            while ((uint32_t)nn <= w.s) nn *= 2;
            p->wait = (uint32_t*)sijo_xrealloc(p->wait, (size_t)nn * sizeof(uint32_t));
            memset(p->wait + p->nwait, 0, (size_t)(nn - p->nwait) * sizeof(uint32_t));
            p->nwait = nn;
        }
        if (k < sv->ncand && sv->cand[k].idx == w.s) {
            if (sv->cand[k++].sel) {
                sijo_frame_copy_ent(out, cur, (int32_t)(w.ce - cur->e));
                p->wait[w.s] = 0;
            } else {
                if (w.he) sijo_frame_copy_ent(out, prev, (int32_t)(w.he - prev->e));
                if (++p->wait[w.s] > p->wait_max) p->wait_max = p->wait[w.s];
                p->deferred++;
                sv->deferred++;
            }
            continue;
        }
        if (w.ce) sijo_frame_copy_ent(out, cur, (int32_t)(w.ce - cur->e));
        p->wait[w.s] = 0;
    }
    out->valid = 1;
    p->ctick[(uint32_t)sv->tick % SIJO_HIST] = sv->tick;
    p->dtick = sv->tick;
}
static void sijo_srv_snapshot(SijoServer* sv, SijoPeer* p, double now) {
    const SijoFrame* cur = &sv->hist[(uint32_t)sv->tick % SIJO_HIST];
    const SijoFrame* base = NULL;
    uint32_t bt = SIJO_NONE;
    if (p->acked >= 0 && p->acked <= sv->tick && sv->tick - p->acked < SIJO_HIST) {
        const SijoFrame* b = &sv->hist[(uint32_t)p->acked % SIJO_HIST];
        if (b->valid && b->tick == p->acked) { base = b; bt = (uint32_t)p->acked; }
    }
    /* interest management: a client with a view gets only what it sees, as a delta against what it
       was sent (and acknowledged) at the baseline tick */
    SijoSet cin = {NULL, 0}, bin = {NULL, 0};
    const SijoSet* cinp = NULL;
    const SijoSet* binp = NULL;
    /* the mode of a tick is decided when it is first sent: a client that acknowledged the full
       start state must not be thought to hold only its view because its view arrived before a
       re-send of that tick (and the other way round) */
    uint32_t sk = (uint32_t)sv->tick % SIJO_HIST;
    int filtered = p->stick[sk] == sv->tick ? p->sview[sk] : p->view_on && sv->world;
    if (filtered) {
        cin.w = sijo_peer_set(sv, p, sv->tick);
        cin.n = p->swords;
        cinp = &cin;
        if (base && sijo_peer_filtered(p, p->acked)) {
            bin.w = p->sets + (size_t)((uint32_t)p->acked % SIJO_HIST) * p->swords;
            bin.n = p->swords;
            binp = &bin;
        }
        for (int32_t i = 0; i < cur->n; i++) p->hidden += !sijo_inset(cinp, cur->e[i].idx);
    } else {
        p->stick[sk] = sv->tick;
        p->sview[sk] = 0;
    }
    SijoEnc* e = NULL;
    if (sv->budget > 0) {
        /* the baseline is the frame this client was sent at the acknowledged tick */
        base = p->acked >= 0 && p->acked <= sv->tick && sv->tick - p->acked < SIJO_HIST ? sijo_peer_frame(p, p->acked) : NULL;
        bt = base ? (uint32_t)p->acked : SIJO_NONE;
        if (!sijo_peer_frame(p, sv->tick)) {
            int budgeted = sv->ticking && !sv->flushing && sv->tick != sv->last_tick;
            sijo_budget_frame(sv, p, cur, cinp, base, budgeted);
        }
        e = &sv->enc[sv->enc_next++ & 3];
        memset(&e->st, 0, sizeof e->st);
        sijo_delta_encode(&e->bw, sijo_peer_frame(p, sv->tick), base, &e->st, sv->stab, NULL, NULL);
        e->valid = 0; /* this client's own frames */
        e->has_par = 0; e->tick = sv->tick; e->base = bt;
    }
    for (int k = 0; k < 4 && !e && !cinp; k++)
        if (sv->enc[k].valid && sv->enc[k].tick == sv->tick && sv->enc[k].base == bt) e = &sv->enc[k];
    if (!e) {
        e = &sv->enc[sv->enc_next++ & 3];
        memset(&e->st, 0, sizeof e->st);
        sijo_delta_encode(&e->bw, cur, base, &e->st, sv->stab, cinp, binp);
        e->valid = !cinp; /* a filtered encoding belongs to this client alone */
        e->has_par = 0; e->tick = sv->tick; e->base = bt;
    }
    size_t len = e->bw.len;
    /* budget compliance: deltas of the budgeted ticks (a full state the client has not acknowledged,
       like the start state sent before the game ran, is repeated as it was) */
    if (sv->budget > 0 && base && sv->ticking && !sv->flushing && sv->tick != sv->last_tick && len > p->pay_max) p->pay_max = (uint32_t)len;
    if (len > SIJO_MAX_SNAPSHOT) {
        if (!p->oversize++) fprintf(stderr, "sijo server: snapshot of %zu bytes exceeds the %u-byte limit; not sent\n", len, SIJO_MAX_SNAPSHOT);
        return;
    }
    uint32_t nfrag = (uint32_t)((len + SIJO_FRAG - 1) / SIJO_FRAG);
    if (!nfrag) nfrag = 1;
    /* a snapshot is only usable when every fragment arrives: on a lossy path add one XOR parity
       fragment, so any single lost fragment can be rebuilt (costs 1/n more bandwidth) */
    int parity = nfrag >= 2 && p->loss_pct >= 2;
    if (parity && !e->has_par) {
        memset(e->par, 0, SIJO_FRAG);
        for (uint32_t f = 0; f < nfrag; f++) {
            size_t off = (size_t)f * SIJO_FRAG, n = len - off < SIJO_FRAG ? len - off : SIJO_FRAG;
            for (size_t k = 0; k < n; k++) e->par[k] ^= e->bw.buf[off + k];
        }
        e->has_par = 1;
    }
    for (uint32_t f = 0; f < nfrag + (uint32_t)parity; f++) {
        uint8_t pkt[SIJO_MTU];
        SijoBuf w = {pkt, 0, sizeof pkt};
        sijo_w8(&w, SIJO_PK_SNAP); sijo_w32(&w, p->session); sijo_w16(&w, p->pseq++);
        sijo_w32(&w, (uint32_t)sv->tick); sijo_w32(&w, bt); sijo_w32(&w, p->rpc_next - 1);
        sijo_w16(&w, f); sijo_w16(&w, nfrag);
        if (f == nfrag) {
            sijo_wbytes(&w, e->par, SIJO_FRAG);
            p->parity++;
        } else {
            size_t off = (size_t)f * SIJO_FRAG, n = len - off < SIJO_FRAG ? len - off : SIJO_FRAG;
            sijo_wbytes(&w, e->bw.buf + off, n);
        }
        sijo_srv_send(sv, p, pkt, w.n, now);
    }
    if (bt == (uint32_t)sv->tick) return; /* keep-alive: the client already has this frame */
    p->snaps++;
    p->payload += len;
    if (!base) p->full++;
    else {
        int32_t age = sv->tick - p->acked;
        p->age_sum += (uint64_t)age;
        if (age > p->age_max) p->age_max = age;
    }
    sv->payload += len;
    sv->naive += cur->naive;
    sv->snaps++;
    sv->st.spawns += e->st.spawns;
    sv->st.updates += e->st.updates;
    sv->st.despawns += e->st.despawns;
    sv->st.whole += e->st.whole;
    sv->st.deltas += e->st.deltas;
    sv->st.delta_bytes += e->st.delta_bytes;
    sv->st.delta_whole_bytes += e->st.delta_whole_bytes;
    sv->st.same += e->st.same;
}
/* once per tick: a snapshot for every playing client; after the last tick (flushing), the final
   frame until the client acknowledges it, then goodbye */
static void sijo_srv_send_all(SijoServer* sv, double now) {
    for (int i = 0; i < sv->npeer; i++) {
        SijoPeer* p = &sv->peer[i];
        if (!p->used || p->state != 1 || !p->confirmed) continue;
        p->rounds++;
        if (sv->flushing && p->acked == sv->final_tick) {
            p->converged = 1;
            if (p->byes++ >= SIJO_BYES) { p->state = 2; continue; }
            sijo_srv_ctl(sv, p, SIJO_PK_BYE, (uint32_t)sv->final_tick, now);
            continue;
        }
        sijo_srv_snapshot(sv, p, now);
    }
}

static void sijo_srv_peer_line(SijoServer* sv, SijoPeer* p, const char* status) {
    char ab[64];
    uint64_t deltas = p->snaps - p->full;
    double rounds = p->rounds ? (double)p->rounds : 1.0;
    (void)sv;
    printf("  client %d (%s) %s: %llu snapshots (%llu full, baseline age avg %.1f / max %d ticks), %llu datagrams (%llu parity), %llu bytes = %.1f B/tick;"
           " client->server loss %.1f%% (%llu duplicates), client reports %d%% recent loss; RPCs %llu received (each once), %llu re-sent copies dropped, %llu rejected\n",
           p->id, sijo_addr_str(p->addr, ab, sizeof ab), status, (unsigned long long)p->snaps, (unsigned long long)p->full,
           deltas ? (double)p->age_sum / (double)deltas : 0.0, p->age_max, (unsigned long long)p->dgrams, (unsigned long long)p->parity,
           (unsigned long long)p->bytes, (double)p->bytes / rounds, 100.0 * sijo_seq_loss(&p->rx), (unsigned long long)p->rx.dups,
           p->loss_pct, (unsigned long long)p->rpcs, (unsigned long long)p->rpc_dups, (unsigned long long)p->rpc_bad);
    if (sv->budget > 0)
        printf("    budget %d B: largest delta %u B of payload while the game ran, %llu entity changes waited (longest %u ticks)\n",
               sv->budget, p->pay_max, (unsigned long long)p->deferred, p->wait_max);
}
static void sijo_srv_drop(SijoServer* sv, SijoPeer* p, const char* why) {
    if (!sv->quiet) {
        printf("sijo server: client %d %s at tick %d\n", p->id, why, sv->tick);
        sijo_srv_peer_line(sv, p, why);
        fflush(stdout);
    }
    p->used = 0;
    free(p->sets);
    p->sets = NULL;
    p->swords = 0;
    sijo_peer_free_budget(p);
    sv->leaves++;
}
static void sijo_srv_accept(SijoServer* sv, SijoPeer* p, double now) {
    uint8_t pkt[64];
    SijoBuf w = {pkt, 0, sizeof pkt};
    sijo_w8(&w, SIJO_PK_ACCEPT); sijo_w32(&w, p->session); sijo_w16(&w, p->pseq++);
    sijo_w32(&w, p->nonce); sijo_w16(&w, (uint32_t)p->id); sijo_w16(&w, (uint32_t)sv->rate); sijo_w64(&w, sv->seed);
    sijo_w32(&w, (uint32_t)sv->tick);
    sijo_srv_send(sv, p, pkt, w.n, now);
}
/* A CONNECT with a new nonce from an address that already has a connection may come from the
   client restarted on the same address, or from anyone forging that source address. The existing
   connection stays until the new session proves it receives at that address (its first CLIENT
   packet carries the session token from our ACCEPT); only then is the old one dropped. An unproven
   session never receives snapshots and times out; a later CONNECT replaces it. */
static void sijo_srv_connect(SijoServer* sv, SijoAddr from, uint32_t nonce, SijoRB* r, double now) {
    uint32_t magic = sijo_r32(r), proto = sijo_r32(r);
    if (r->bad || magic != SIJO_MAGIC || nonce == 0) return;
    for (int i = 0; i < sv->npeer; i++) {
        SijoPeer* q = &sv->peer[i];
        if (q->used && sijo_addr_eq(q->addr, from) && q->nonce == nonce) { sijo_srv_accept(sv, q, now); return; } /* our ACCEPT was lost: repeat it */
    }
    for (int i = 0; i < sv->npeer; i++) {
        SijoPeer* q = &sv->peer[i];
        if (q->used && sijo_addr_eq(q->addr, from) && !q->confirmed) sijo_srv_drop(sv, q, "superseded before confirming");
    }
    int reason = proto != SIJO_PROTO ? 1 : sv->flushing ? 3 : 0;
    SijoPeer* slot = NULL;
    if (!reason) {
        for (int i = 0; i < sv->npeer && !slot; i++) if (!sv->peer[i].used) slot = &sv->peer[i];
        if (!slot && sv->npeer < sv->max_clients) {
            sv->peer = (SijoPeer*)sijo_xrealloc(sv->peer, (size_t)(sv->npeer + 1) * sizeof(SijoPeer));
            slot = &sv->peer[sv->npeer++];
        }
        if (!slot) reason = 2;
    }
    if (reason) {
        uint8_t pkt[16];
        SijoBuf w = {pkt, 0, sizeof pkt};
        sijo_w8(&w, SIJO_PK_REJECT); sijo_w32(&w, nonce); sijo_w16(&w, 0); sijo_w8(&w, (uint32_t)reason);
        sijo_sock_send(sv->sock, from, pkt, (int)w.n, now);
        return;
    }
    memset(slot, 0, sizeof *slot);
    slot->used = 1; slot->state = 1; slot->id = ++sv->next_id; slot->addr = from; slot->nonce = nonce;
    do { sv->rng += 0x9E3779B97F4A7C15ull; slot->session = (uint32_t)sijo_mix(sv->rng); } while (!slot->session);
    slot->acked = -1; slot->joined = sv->tick; slot->rpc_next = 1; slot->last_recv = now;
    for (int k = 0; k < SIJO_HIST; k++) { slot->stick[k] = -2; slot->sview[k] = 0; slot->ctick[k] = -2; } /* no tick sent yet */
    slot->dtick = -2;
    sv->joins++;
    if (!sv->quiet) {
        char ab[64];
        printf("sijo server: client %d connected from %s at tick %d (%d playing)\n", slot->id, sijo_addr_str(from, ab, sizeof ab), sv->tick, sijo_srv_count(sv, 0));
        fflush(stdout);
    }
    sijo_srv_accept(sv, slot, now);
}

/* acks and reliable RPCs from one client */
static void sijo_srv_client_packet(SijoServer* sv, SijoWorld* S, SijoPeer* p, SijoRB* r) {
    uint32_t ack = sijo_r32(r), epoch = sijo_r8(r), loss = sijo_r8(r);
    if (r->bad) return;
    p->loss_pct = (int)(loss > 100 ? 100 : loss);
    uint32_t von = sijo_r8(r);
    if (von) {
        float v[3];
        for (int k = 0; k < 3; k++) v[k] = sijo_bits2f(sijo_r32(r));
        if (r->bad) return;
        /* a view must be a finite circle; anything else means "everything" */
        int ok = v[0] == v[0] && v[1] == v[1] && v[2] >= 0.0f && v[0] - v[0] == 0.0f && v[1] - v[1] == 0.0f && v[2] - v[2] == 0.0f;
        p->view_on = ok;
        if (ok) memcpy(p->view, v, sizeof v);
    } else {
        p->view_on = 0;
    }
    uint32_t first = sijo_r32(r), cnt = sijo_r8(r);
    if (r->bad) return;
    if (epoch != p->epoch) {
        /* the client lost its baselines and asks for full state; packets of an older epoch are ignored */
        if ((uint8_t)(epoch - p->epoch) < 128) { p->epoch = (uint8_t)epoch; p->acked = -1; p->resyncs++; }
    } else if (ack != SIJO_NONE && ack <= 0x7fffffffu && (int32_t)ack <= sv->tick && (int32_t)ack > p->acked) {
        p->acked = (int32_t)ack; /* acks only move forward: late or duplicated packets cannot regress them */
    }
    for (uint32_t k = 0; k < cnt; k++) {
        uint32_t seq = first + k;
        size_t start = r->pos;
        uint32_t si = sijo_rvar(r), gen = sijo_rvar(r), fid = sijo_rvar(r), size = sijo_rvar(r);
        if (r->bad || size > SIJO_RPC_MAX_ARGS) { p->rpc_bad++; return; }
        const uint8_t* args = sijo_rbytes(r, size);
        if (r->bad) { p->rpc_bad++; return; }
        if (seq < p->rpc_next) { p->rpc_dups++; continue; } /* re-sent copy of one we already have */
        if (seq != p->rpc_next) return;                      /* gap: the client re-sends in order */
        if (sv->rpc_limit && p->rpc_tick >= (uint32_t)sv->rpc_limit) { p->rpc_held++; return; }
        p->rpc_next++;
        p->rpc_tick++;
        p->rpcs++;
        sv->rpcs++;
        sv->rpc_bytes += r->pos - start;
        if (sv->flushing) continue; /* the game is over: acknowledge, but nothing executes any more */
        uint8_t tmp[SIJO_RPC_MAX_ARGS];
        if (size) memcpy(tmp, args, size);
        if (si == 0 || si >= (uint32_t)S->ents.n || !sijo_rpc_ok(fid, size, tmp)) { p->rpc_bad++; continue; }
        sijo_rpc_canon(fid, tmp, size); /* wire byte order -> this machine's */
        /* executed at the start of the next tick, under server authority (generation re-checked there) */
        SijoCmdBuf* sb = &S->cmd[0];
        uint32_t d = sijo_cmd_data(sb, tmp, size);
        SijoCmd* sc = sijo_cmd_push(sb);
        SijoH target = {si, gen};
        sc->kind = SIJO_CMD_CALL; sc->target = target; sc->src = SIJO_NULL; sc->field = (uint16_t)fid;
        sc->data = d; sc->size = (uint16_t)size;
    }
}
static void sijo_srv_packet(SijoServer* sv, SijoWorld* S, SijoAddr from, const uint8_t* d, int n, double now) {
    SijoRB r = {d, (size_t)n, 0, 0};
    uint32_t type = sijo_r8(&r), session = sijo_r32(&r), pseq = sijo_r16(&r);
    if (r.bad) return;
    if (type == SIJO_PK_CONNECT) { sijo_srv_connect(sv, from, session, &r, now); return; }
    SijoPeer* p = sijo_srv_find_session(sv, from, session);
    if (!p) { sv->stray++; return; }
    p->last_recv = now;
    sijo_seq_note(&p->rx, pseq);
    switch (type) {
    case SIJO_PK_CLIENT:
        if (!p->confirmed) {
            p->confirmed = 1;
            /* the session proved it receives at this address: an older connection there was a
               previous run of the same client */
            for (int i = 0; i < sv->npeer; i++) {
                SijoPeer* q = &sv->peer[i];
                if (q != p && q->used && sijo_addr_eq(q->addr, from)) sijo_srv_drop(sv, q, "reconnected");
            }
        }
        if (p->state == 1) sijo_srv_client_packet(sv, S, p, &r);
        break;
    case SIJO_PK_DISCONNECT: sijo_srv_drop(sv, p, "disconnected"); break;
    case SIJO_PK_BYE_ACK: if (sv->flushing) { p->state = 2; p->converged = 1; } break;
    default: break;
    }
}
static void sijo_srv_recv(SijoServer* sv, SijoWorld* S, double now) {
    uint8_t buf[2048];
    SijoAddr from;
    for (;;) {
        int n = sijo_sock_recv(sv->sock, &from, buf, (int)sizeof buf);
        if (n < 0) break;
        if (n <= SIJO_MTU) sijo_srv_packet(sv, S, from, buf, n, now);
    }
}
static void sijo_srv_timeouts(SijoServer* sv, double now) {
    if (!sv->udp) return;
    for (int i = 0; i < sv->npeer; i++) {
        SijoPeer* p = &sv->peer[i];
        if (p->used && p->state == 1 && now - p->last_recv > sv->timeout) sijo_srv_drop(sv, p, "timed out");
    }
}
static int sijo_srv_all_done(const SijoServer* sv) {
    for (int i = 0; i < sv->npeer; i++) if (sv->peer[i].used && sv->peer[i].state == 1) return 0;
    return 1;
}
/* real time: receive (and deliver simulated-late datagrams) until `until` */
static void sijo_srv_wait(SijoServer* sv, SijoWorld* S, double until) {
    for (;;) {
        double now = sijo_now();
        sijo_sock_pump(sv->sock, now);
        sijo_srv_recv(sv, S, now);
        if (now >= until) return;
        double t = until, due = sijo_sock_next_due(sv->sock);
        if (due < t) t = due;
        sijo_sock_wait(sv->sock, t - now);
    }
}
/* in-process loopback: the client is attached directly (no handshake); everything else is packets */
static void sijo_srv_attach_local(SijoServer* sv, SijoClient* cl) {
    sv->peer = (SijoPeer*)sijo_xrealloc(sv->peer, sizeof(SijoPeer));
    sv->npeer = 1;
    SijoPeer* p = &sv->peer[0];
    memset(p, 0, sizeof *p);
    p->used = 1; p->state = 1; p->confirmed = 1; p->id = ++sv->next_id; p->addr = cl->sock->self; p->nonce = 1; p->session = 1;
    p->acked = -1; p->joined = sv->tick; p->rpc_next = 1;
    for (int k = 0; k < SIJO_HIST; k++) { p->stick[k] = -2; p->sview[k] = 0; p->ctick[k] = -2; } /* no tick sent yet */
    p->dtick = -2;
    cl->server = sv->sock->self; cl->session = 1; cl->state = 1; cl->id = p->id; cl->rate = sv->rate; cl->seed = sv->seed;
}

/* ======================= process modes ======================= */
/* how a process steps its server world: alone, or as one of several shards (sijo_shard_net.h sets
   these hooks: start = every shard has the start state, step = one lockstep tick, finish = the
   coordinator gathers the whole world before the summary, stats = the shard summary line) */
static void sijo_step_local(SijoWorld* S) { sijo_apply(S); sijo_tick(S); }
static void (*sijo_step)(SijoWorld* S) = sijo_step_local;
static void (*sijo_start_hook)(SijoWorld* S) = NULL;
static void (*sijo_finish_hook)(SijoWorld* S) = NULL;
static void (*sijo_stats_hook)(SijoWorld* S) = NULL;
typedef struct {
    int ticks, shards, render, quiet, timing, summary, realtime, render_every;
    const char* load_path; const char* save_path; const char* live_dir;
    const char* profile_path;         /* --profile FILE: time every system on one thread, write FILE */
    uint64_t seed;
    int mode, port; const char* connect;
    int rate, wait_clients, max_clients;
    double wait_timeout, timeout, flush_timeout, connect_timeout;
    SijoSimCfg sim; int sim_on;
    int has_key; uint8_t key[32];
    int no_predict;
    int budget;                       /* server: payload bytes per snapshot and client (0: no limit) */
    /* shard processes (sijo_shard_net.h) */
    int shard_of, shard_id; const char* peers; const char* ckpt_dir; int ckpt_every; int recover;
    int die_at; double shard_timeout;
} SijoOpts;
static int sijo_shard_setup(SijoOpts* o);

static void sijo_print_sim(const char* who, const SijoSock* s, const SijoOpts* o) {
    if (s->crypt) printf("encryption (%s): XChaCha20-Poly1305 with the pre-shared key; %llu received datagrams rejected (bad key or tampered)\n", who, (unsigned long long)s->rejected);
    if (!o->sim_on) return;
    printf("network simulation (%s, loss %.0f%%, latency %.0f +- %.0f ms, duplication %.0f%%, seed %llu): %llu datagrams offered, %llu lost, %llu duplicated\n",
           who, 100.0 * o->sim.loss, o->sim.lat_ms, o->sim.jit_ms, 100.0 * o->sim.dup, (unsigned long long)o->sim.seed,
           (unsigned long long)s->offered, (unsigned long long)s->sim_lost, (unsigned long long)s->sim_duped);
}
static void sijo_print_world(SijoWorld* S, const SijoOpts* o, int ticks) {
    printf("ticks: %d, stages/tick: %d, threads: %d\n", ticks, SIJO_NSTAGES, S->nthreads);
    printf("entities alive:");
    sijo_print_counts(S);
    printf(" (spawned %lld, destroyed %lld)\n", (long long)S->spawned, (long long)S->destroyed);
    printf("deferred commands applied: %lld\n", (long long)S->cmds_applied);
    (void)o;
}
static void sijo_print_snapshots(const SijoServer* sv) {
    if (sv->naive)
        printf("network: %llu bytes in %llu snapshots (%.1f B/tick), naive full-state: %.1f B/tick -> %.1fx smaller\n",
               (unsigned long long)sv->payload, (unsigned long long)sv->snaps, (double)sv->payload / (double)sv->snaps,
               (double)sv->naive / (double)sv->snaps, (double)sv->naive / (double)(sv->payload ? sv->payload : 1));
    else
        printf("network: nothing to replicate (no client code reads simulation state)\n");
    printf("network records: %llu spawns, %llu updates, %llu despawns\n", (unsigned long long)sv->st.spawns,
           (unsigned long long)sv->st.updates, (unsigned long long)sv->st.despawns);
    if (sv->st.whole + sv->st.deltas)
        printf("strings and lists: %llu sent whole, %llu as deltas against the acknowledged value (%llu bytes instead of %llu)%s\n",
               (unsigned long long)sv->st.whole, (unsigned long long)sv->st.deltas, (unsigned long long)sv->st.delta_bytes,
               (unsigned long long)sv->st.delta_whole_bytes, sijo_list_deltas ? "" : "; deltas off (--no-list-deltas)");
    if (sv->budget > 0) {
        uint32_t wm = 0, pm = 0;
        for (int i = 0; i < sv->npeer; i++) {
            if (sv->peer[i].wait_max > wm) wm = sv->peer[i].wait_max;
            if (sv->peer[i].pay_max > pm) pm = sv->peer[i].pay_max;
        }
        printf("bandwidth budget: %d B of payload per snapshot and client (largest delta while the game ran: %u B); %llu entity changes waited for room (longest wait %u ticks)\n",
               sv->budget, pm, (unsigned long long)sv->deferred, wm);
    }
}
static void sijo_print_tail(SijoWorld* S, const SijoOpts* o, int ticks, double t0, double t1) {
    SIJO_RUN_END(S); /* [Persistent]: the store's last write */
    if (sijo_stats_hook) sijo_stats_hook(S);
    size_t peak = 0;
    for (int t = 0; t < SIJO_MAX_THREADS; t++) if (S->frame[t].peak > peak) peak = S->frame[t].peak;
    printf("frame arena peak: %zu bytes (reset every tick; loops and entities give back their temporaries)\n", peak);
    if (S->sheap.promoted) printf("strings: %d live on the heap (%zu bytes, peak %zu), %lld promoted from frame arenas, %lld freed\n", S->sheap.n, S->sheap.bytes, S->sheap.peak_bytes, (long long)S->sheap.promoted, (long long)S->sheap.freed);
    if (S->stab.interned) printf("replicated strings and lists: %llu distinct values sent, %u still referenced by the frame history\n", (unsigned long long)S->stab.interned, S->stab.n);
    printf("world checksum: %016llx\n", (unsigned long long)sijo_checksum(S));
    if (o->timing) printf("time: %.3f ms total, %.2f us/tick\n", (t1 - t0) * 1e3, (t1 - t0) * 1e6 / (ticks > 0 ? ticks : 1));
}

/* default: server and client worlds in one process, connected by an in-memory datagram link.
   Without --sim-* flags the link is lossless and immediate, so the client sees every tick and
   its RPCs reach the server for the next tick, exactly as a zero-latency network would. */
static void sijo_lb_net(SijoServer* sv, SijoClient* cl, SijoWorld* C, double vt) {
    sijo_srv_send_all(sv, vt);
    sijo_sock_pump(sv->sock, vt);
    sijo_cli_step(cl, C, vt);
    sijo_sock_pump(cl->sock, vt);
}
static int sijo_loopback_main(const SijoOpts* o) {
    static SijoWorld S, C;
    static SijoSock ss, cs;
    static SijoServer sv;
    static SijoClient cl;
    sijo_world_init(&S, 0, o->seed, o->shards);
    sijo_world_init(&C, 1, o->seed, 1);
    S.quiet = o->quiet; C.quiet = o->quiet;
    if (o->profile_path) { S.prof = 1; S.nthreads = 1; } /* per-row costs measured on one thread */
    if (o->live_dir) setvbuf(stdout, NULL, _IOLBF, 0);
    sijo_sock_pair(&ss, &cs);
    if (o->has_key) { sijo_sock_key(&ss, o->key); sijo_sock_key(&cs, o->key); }
    if (o->sim_on) { sijo_sock_sim(&ss, &o->sim, 1); sijo_sock_sim(&cs, &o->sim, 2); }
    sijo_srv_init(&sv, &ss, 0, o->rate, 1, o->timeout, o->seed, 1);
    sv.budget = o->budget;
    sijo_cli_init(&cl, &cs, 0);
    cl.predict = !o->no_predict;
    double t0 = sijo_now(), dt = 1.0 / o->rate;
    int start_tick = 0;
    if (o->load_path && sijo_load_state(&S, o->load_path)) start_tick = S.tick; /* live reload: continue where the previous build stopped */
    else sijo_game_start(&S);
    if (sijo_start_hook) sijo_start_hook(&S);
    sv.tick = S.tick;
    sijo_srv_attach_local(&sv, &cl);
    double vt = start_tick * dt;
    sijo_srv_capture(&sv, &S);
    sijo_lb_net(&sv, &cl, &C, vt);
    sijo_reset_arenas(&S);
    double next_frame = sijo_now();
    sv.ticking = 1;
    if (o->ticks > 0) sv.last_tick = start_tick + o->ticks;
    for (int t = start_tick + 1; o->ticks <= 0 || t <= start_tick + o->ticks; t++) {
        vt = t * dt;
        sijo_sock_pump(&ss, vt);
        sijo_sock_pump(&cs, vt);
        sijo_srv_recv(&sv, &S, vt); /* acks; RPC requests go to the command buffer for this tick */
        sijo_srv_new_tick(&sv);
        S.tick = t;
        sijo_step(&S);
        sijo_srv_capture(&sv, &S);
        sijo_lb_net(&sv, &cl, &C, vt);
        if (o->render_every > 0 && t % o->render_every == 0) { printf("tick %d\n", t); sijo_render_ascii(&C.draws, 64, 18); fflush(stdout); }
        sijo_reset_arenas(&S);
        if (o->live_dir) {
            /* sijoc live asks for a hand-over by creating <dir>/reload */
            char rp[1024], sp[1024];
            snprintf(rp, sizeof rp, "%s/reload", o->live_dir);
            FILE* rf = fopen(rp, "r");
            if (rf) {
                fclose(rf);
                remove(rp);
                snprintf(sp, sizeof sp, "%s/state", o->live_dir);
                sijo_save_state(&S, sp);
                fprintf(stderr, "[live] state saved at tick %d\n", t);
                fflush(stdout);
                return 42;
            }
        }
        if (o->realtime) {
            next_frame += dt;
            double now = sijo_now();
            if (next_frame > now) sijo_sleep(next_frame - now);
            else next_frame = now;
        }
    }
    /* a lossy/delayed simulated link may leave the client behind: keep sending the final frame */
    if (cl.applied != S.tick) {
        sv.flushing = 1;
        sv.final_tick = S.tick;
        for (int k = 0; k < 30 * o->rate && cl.applied != S.tick; k++) {
            vt += dt;
            sijo_sock_pump(&ss, vt);
            sijo_sock_pump(&cs, vt);
            sijo_srv_recv(&sv, &S, vt);
            sijo_lb_net(&sv, &cl, &C, vt);
        }
    }
    double t1 = sijo_now();
    if (sijo_finish_hook) sijo_finish_hook(&S);
    if (o->save_path) sijo_save_state(&S, o->save_path);
    if (o->profile_path) sijo_profile_write(&S, o->profile_path);
    /* the game is over: RPCs the server did not execute never will, so their predictions go */
    if (cl.applied >= 0) sijo_cli_reconcile(&cl, &C, &cl.shown, cl.rpc_seq);
    if (o->render) { printf("client view (last frame, %d sprites):\n", (int)C.draws.n); sijo_render_ascii(&C.draws, 64, 22); }
    if (!o->summary) return 0;
    int64_t vents = 0, vvals = 0, hents = 0;
    SijoSet vs = {NULL, 0};
    const SijoSet* vsp = NULL;
    if (sv.npeer && sijo_peer_filtered(&sv.peer[0], sv.tick)) {
        vs.w = sv.peer[0].sets + (size_t)((uint32_t)sv.tick % SIJO_HIST) * sv.peer[0].swords;
        vs.n = sv.peer[0].swords;
        vsp = &vs;
    }
    int bad = sijo_net_verify(&S, &C, &cl.map, vsp, &vents, &vvals);
    uint64_t h = sijo_net_hash_server(&S, NULL, &hents);
    printf("== sijo runtime summary ==\n");
    sijo_print_world(&S, o, o->ticks);
    sijo_print_snapshots(&sv);
    printf("network packets: %llu datagrams / %llu bytes server->client (incl. %d-byte snapshot headers, fragments of <= %d bytes), %llu / %llu client->server\n",
           (unsigned long long)ss.offered, (unsigned long long)sv.peer[0].bytes, SIJO_SNAP_HDR, SIJO_FRAG,
           (unsigned long long)cs.offered, (unsigned long long)cl.sent_bytes);
    if (o->sim_on) {
        sijo_print_sim("server->client", &ss, o);
        sijo_print_sim("client->server", &cs, o);
        printf("client: %llu snapshots applied, %llu lost ticks run on interpolated values, %llu completed by parity, %llu incomplete (fragment lost), %llu late fragments; %llu RPC re-sends\n",
               (unsigned long long)cl.applied_n, (unsigned long long)cl.filled, (unsigned long long)cl.recovered, (unsigned long long)cl.incomplete, (unsigned long long)cl.late_frags,
               (unsigned long long)cl.rpc_resends);
    }
    if (sv.rpcs) printf("client->server RPCs: %llu requests, %llu bytes (executed under server authority)\n", (unsigned long long)sv.rpcs, (unsigned long long)sv.rpc_bytes);
    if (cl.predicted) printf("client prediction: %llu RPCs run on the client at once, %llu re-runs after snapshots\n", (unsigned long long)cl.predicted, (unsigned long long)cl.replays);
    printf("replication check: %s (%lld entities, %lld values compared%s)\n", bad ? "MISMATCH" : "OK", (long long)vents, (long long)vvals, vsp ? ", those in the client's view" : "");
    printf("replicated state hash: %016llx (tick %d, %lld entities)\n", (unsigned long long)h, S.tick, (long long)hents);
    if (vsp) {
        int64_t ve = 0;
        uint64_t vh = sijo_net_hash_server(&S, vsp, &ve);
        printf("replicated state hash for client %d (its view): %016llx (tick %d, %lld entities; %llu entity records left out)\n", sv.peer[0].id, (unsigned long long)vh, S.tick, (long long)ve, (unsigned long long)sv.peer[0].hidden);
    }
    sijo_print_tail(&S, o, o->ticks, t0, t1);
    return bad ? 4 : 0;
}

/* --server PORT: the authoritative world only; any number of clients over UDP */
static int sijo_server_main(const SijoOpts* o) {
    static SijoSock sock;
    static SijoWorld S;
    static SijoServer sv;
    int port = sijo_udp_open(&sock, o->port);
    if (o->has_key) sijo_sock_key(&sock, o->key);
    if (port < 0) { fprintf(stderr, "sijo server: cannot bind udp port %d: %s\n", o->port, sijo_sock_errstr()); return 2; }
    if (o->sim_on) sijo_sock_sim(&sock, &o->sim, 1);
    sijo_world_init(&S, 0, o->seed, o->shards);
    S.quiet = o->quiet;
    if (o->profile_path) { S.prof = 1; S.nthreads = 1; }
    sijo_srv_init(&sv, &sock, 1, o->rate, o->max_clients, o->timeout, o->seed, o->quiet);
    sv.budget = o->budget;
    signal(SIGINT, sijo_on_signal);
    signal(SIGTERM, sijo_on_signal);
    int start_tick = 0;
    if (o->load_path && sijo_load_state(&S, o->load_path)) start_tick = S.tick;
    else sijo_game_start(&S);
    if (sijo_start_hook) sijo_start_hook(&S);
    sijo_srv_capture(&sv, &S);
    sijo_reset_arenas(&S);
    printf("sijo server: listening on udp port %d (%d Hz, protocol %08x)\n", port, sv.rate, (unsigned)SIJO_PROTO);
    fflush(stdout);
    double dt = 1.0 / sv.rate, t0 = sijo_now(), next = t0;
    if (o->wait_clients > 0) {
        double deadline = t0 + o->wait_timeout;
        while (sijo_srv_count(&sv, 1) < o->wait_clients && !sijo_stop_flag && sijo_now() < deadline) {
            next += dt;
            sijo_srv_wait(&sv, &S, next);
            double now = sijo_now();
            sijo_srv_send_all(&sv, now);
            sijo_srv_timeouts(&sv, now);
        }
        if (sijo_srv_count(&sv, 1) < o->wait_clients)
            fprintf(stderr, "sijo server: %d of %d clients connected; starting anyway\n", sijo_srv_count(&sv, 1), o->wait_clients);
        next = sijo_now();
    }
    double s0 = sijo_now();
    int t = start_tick;
    /* the game runs from here on; the start state (sent while waiting for clients) was not budgeted */
    sv.ticking = 1;
    if (o->ticks > 0) sv.last_tick = start_tick + o->ticks;
    int stopping = sijo_stop_flag;
    while ((o->ticks <= 0 || t < start_tick + o->ticks) && !stopping) {
        next += dt;
        sijo_srv_wait(&sv, &S, next); /* acks and RPCs (queued for this tick) */
        double now = sijo_now();
        if (now - next > 0.25) next = now; /* fell behind: do not catch up in a burst */
        t++;
        sijo_srv_new_tick(&sv);
        S.tick = t;
        /* a stop request ends the game after this tick (the other shards are told in its barriers) */
        stopping = sijo_stop_flag;
        if (stopping && S.sh) S.sh->stop = 1;
        sijo_step(&S);
        sijo_srv_capture(&sv, &S);
        /* a stop request ends the game after this tick, which is then sent in full */
        if (stopping) sv.last_tick = t;
        now = sijo_now(); /* the tick itself took time: simulated delays start when the datagrams leave */
        sijo_srv_send_all(&sv, now);
        sijo_srv_timeouts(&sv, now);
        sijo_reset_arenas(&S);
    }
    double s1 = sijo_now();
    /* flush: keep sending the final frame until every client acknowledged it, then say goodbye */
    sv.flushing = 1;
    sv.final_tick = S.tick;
    double deadline = sijo_now() + o->flush_timeout;
    while (!sijo_srv_all_done(&sv) && sijo_now() < deadline) {
        next += dt;
        sijo_srv_wait(&sv, &S, next);
        double now = sijo_now();
        if (now - next > 0.25) next = now;
        sijo_srv_send_all(&sv, now);
        sijo_srv_timeouts(&sv, now);
    }
    if (sijo_finish_hook) sijo_finish_hook(&S);
    if (o->profile_path) sijo_profile_write(&S, o->profile_path);
    if (o->save_path) sijo_save_state(&S, o->save_path);
    if (!o->summary) return 0;
    int64_t hents = 0;
    uint64_t h = sijo_net_hash_server(&S, NULL, &hents);
    int failed = 0, converged = 0;
    printf("== sijo server summary ==\n");
    sijo_print_world(&S, o, t - start_tick);
    printf("real time: %.2f s for %d ticks (%.1f ticks/s), %d client(s) joined, %d left or timed out\n", s1 - s0, t - start_tick,
           (s1 > s0 ? (t - start_tick) / (s1 - s0) : 0.0), (int)sv.joins, (int)sv.leaves);
    sijo_print_snapshots(&sv);
    if (sv.rpcs) printf("client->server RPCs: %llu requests, %llu bytes (executed under server authority)\n", (unsigned long long)sv.rpcs, (unsigned long long)sv.rpc_bytes);
    sijo_print_sim("server->clients", &sock, o);
    printf("clients at the end:\n");
    for (int i = 0; i < sv.npeer; i++) {
        SijoPeer* p = &sv.peer[i];
        if (!p->used) continue;
        char st[64];
        if (p->converged) { snprintf(st, sizeof st, "converged at tick %d", sv.final_tick); converged++; }
        else { snprintf(st, sizeof st, "NOT converged (acked tick %d)", p->acked); failed++; }
        sijo_srv_peer_line(&sv, p, st);
    }
    printf("convergence: %d of %d client(s) acknowledged the final tick\n", converged, converged + failed);
    printf("replicated state hash: %016llx (tick %d, %lld entities)\n", (unsigned long long)h, S.tick, (long long)hents);
    for (int i = 0; i < sv.npeer; i++) {
        SijoPeer* p = &sv.peer[i];
        if (!p->used || !p->sets || !sijo_peer_filtered(p, sv.final_tick)) continue;
        SijoSet vs = {p->sets + (size_t)((uint32_t)sv.final_tick % SIJO_HIST) * p->swords, p->swords};
        int64_t ve = 0;
        uint64_t vh = sijo_net_hash_server(&S, &vs, &ve);
        printf("replicated state hash for client %d (its view): %016llx (tick %d, %lld entities; %llu entity records left out)\n", p->id, (unsigned long long)vh, S.tick, (long long)ve, (unsigned long long)p->hidden);
    }
    sijo_print_tail(&S, o, t - start_tick, s0, s1);
    return failed ? 4 : 0;
}

/* --connect HOST:PORT: the client world only */
static int sijo_client_main(const SijoOpts* o) {
    static SijoSock sock;
    static SijoWorld C;
    static SijoClient cl;
    SijoAddr srv;
    if (!sijo_resolve(o->connect, &srv)) { fprintf(stderr, "sijo client: cannot resolve '%s' (expected HOST:PORT, or [IPV6]:PORT)\n", o->connect); return 2; }
    if (sijo_udp_open(&sock, 0) < 0) { fprintf(stderr, "sijo client: cannot open a udp socket: %s\n", sijo_sock_errstr()); return 2; }
    if (o->has_key) sijo_sock_key(&sock, o->key);
    if (o->sim_on) sijo_sock_sim(&sock, &o->sim, 2);
    sijo_cli_init(&cl, &sock, 1);
    cl.predict = !o->no_predict;
    cl.server = srv;
    cl.render_every = o->render_every;
    signal(SIGINT, sijo_on_signal);
    signal(SIGTERM, sijo_on_signal);
    uint64_t rs = sijo_mix((uint64_t)(sijo_now() * 1e9) ^ (sijo_pid() << 32) ^ sock.self.port);
    do { rs = sijo_mix(rs); cl.nonce = (uint32_t)rs; } while (!cl.nonce);
    double start = sijo_now(), next_try = start;
    char ab[64];
    while (cl.state == 0) {
        double now = sijo_now();
        if (cl.reject) {
            const char* why = cl.reject == 1 ? "different program or compiler version (protocol mismatch)" : cl.reject == 2 ? "server is full" : cl.reject == 3 ? "the game is over" : "rejected";
            fprintf(stderr, "sijo client: %s refused the connection: %s\n", sijo_addr_str(srv, ab, sizeof ab), why);
            return 6;
        }
        if (now - start > o->connect_timeout || sijo_stop_flag) {
            fprintf(stderr, "sijo client: no answer from %s%s\n", sijo_addr_str(srv, ab, sizeof ab),
                    sock.rejected ? " (its datagrams did not authenticate: different --key?)" : o->has_key ? " (a server without the same --key does not answer)" : "");
            return 3;
        }
        if (now >= next_try) {
            uint8_t pkt[32];
            SijoBuf w = {pkt, 0, sizeof pkt};
            sijo_w8(&w, SIJO_PK_CONNECT); sijo_w32(&w, cl.nonce); sijo_w16(&w, 0); sijo_w32(&w, SIJO_MAGIC); sijo_w32(&w, SIJO_PROTO);
            sijo_sock_send(&sock, srv, pkt, (int)w.n, now);
            next_try = now + 0.1;
        }
        sijo_cli_wait(&cl, next_try);
    }
    sijo_world_init(&C, 1, cl.seed, 1);
    C.quiet = o->quiet;
    printf("sijo client %d: connected to %s (server tick %d, %d Hz)\n", cl.id, sijo_addr_str(srv, ab, sizeof ab), cl.join_tick, cl.rate);
    fflush(stdout);
    double dt = 1.0 / cl.rate, next = sijo_now();
    int code = 0;
    for (;;) {
        next += dt;
        sijo_cli_wait(&cl, next);
        double now = sijo_now();
        if (now - next > 0.25) next = now;
        sijo_cli_update(&cl, &C);
        if (cl.bye) break;
        if (cl.kicked) { fprintf(stderr, "sijo client %d: disconnected by the server\n", cl.id); code = 5; break; }
        if (sijo_stop_flag) {
            for (int k = 0; k < 3; k++) sijo_cli_ctl(&cl, SIJO_PK_DISCONNECT, now);
            code = 130;
            break;
        }
        if (now - cl.last_recv > o->timeout) { fprintf(stderr, "sijo client %d: server timed out\n", cl.id); code = 5; break; }
        sijo_cli_send(&cl, now);
    }
    /* acknowledge the goodbye, answer repeated ones, and let (simulated) delayed datagrams leave;
       the loop below reuses cl.bye for repeated goodbyes, so remember that the game is over */
    int over = cl.bye;
    if (over) sijo_cli_ctl(&cl, SIJO_PK_BYE_ACK, sijo_now());
    double linger = sijo_now() + (over ? 0.2 + (o->sim.lat_ms + o->sim.jit_ms) / 1000.0 : 0.0);
    while (sijo_now() < linger || (sock.dq_n > 0 && sijo_now() < linger + 2.0)) {
        cl.bye = 0;
        sijo_cli_wait(&cl, sijo_now() + 0.01);
        if (cl.bye) sijo_cli_ctl(&cl, SIJO_PK_BYE_ACK, sijo_now());
    }
    if (over && cl.applied >= 0) sijo_cli_reconcile(&cl, &C, &cl.shown, cl.rpc_seq); /* game over: drop unconfirmed predictions */
    if (o->render) { printf("client view (last frame, %d sprites):\n", (int)C.draws.n); sijo_render_ascii(&C.draws, 64, 22); }
    if (!o->summary) return code;
    int64_t hents = 0;
    uint64_t h = cl.applied >= 0 ? sijo_net_hash_client(&C, &cl.map, &cl.frames[(uint32_t)cl.applied % SIJO_HIST], &hents) : 0;
    double ticks = cl.applied >= cl.join_tick ? (double)(cl.applied - cl.join_tick + 1) : 1.0;
    printf("== sijo client %d summary ==\n", cl.id);
    if (cl.predicted) printf("client prediction: %llu RPCs run on the client at once, %llu re-runs after snapshots\n", (unsigned long long)cl.predicted, (unsigned long long)cl.replays);
    printf("snapshots: %llu applied (%llu client frames, %llu for lost ticks), %llu completed by parity; not applied: %llu incomplete (fragment lost), %llu superseded, %llu without baseline, %llu malformed; %llu late fragments\n",
           (unsigned long long)cl.applied_n, (unsigned long long)cl.frames_run, (unsigned long long)cl.filled, (unsigned long long)cl.recovered, (unsigned long long)cl.incomplete,
           (unsigned long long)cl.superseded, (unsigned long long)cl.nobase, (unsigned long long)cl.malformed, (unsigned long long)cl.late_frags);
    printf("received: %llu datagrams, %llu bytes = %.1f B/tick; server->client loss %.1f%% (%llu duplicates), %llu fragments (%llu duplicate)\n",
           (unsigned long long)cl.dgrams, (unsigned long long)cl.bytes, (double)cl.bytes / ticks, 100.0 * sijo_seq_loss(&cl.rx),
           (unsigned long long)cl.rx.dups, (unsigned long long)cl.frags, (unsigned long long)cl.frag_dups);
    printf("sent: %llu datagrams, %llu bytes; RPCs: %llu sent, %llu re-sends, %llu dropped\n", (unsigned long long)cl.sent_dgrams,
           (unsigned long long)cl.sent_bytes, (unsigned long long)cl.rpcs, (unsigned long long)cl.rpc_resends, (unsigned long long)cl.rpc_dropped);
    sijo_print_sim("client->server", &sock, o);
    printf("replicated state hash: %016llx (tick %d, %lld entities)\n", (unsigned long long)h, cl.applied, (long long)hents);
    return code;
}

static void sijo_usage(void) {
    printf("options:\n"
           "  --ticks N  --seed S  --shards N  --render  --render-every N  --quiet  --timing  --no-summary  --realtime\n"
           "  --save-state FILE  --load-state FILE\n"
           "  --store DIR        keep [Persistent] fields in DIR across runs (written every --store-every N ticks, default 60, and at exit)\n"
           "  --profile FILE     time every system on one thread and write FILE for sijoc build --profile-use FILE\n"
           "networking (default: server and client in one process over an in-memory link):\n"
           "  --server PORT           run only the server world, on UDP PORT (0 = pick a free port)\n"
           "  --connect HOST:PORT     run only a client world connected to a server\n"
           "  --tick-rate HZ          server tick rate in real time (default 60; simulation dt stays 1/60)\n"
           "  --wait-clients N        server: wait for N clients before the first tick (--wait-timeout-ms)\n"
           "  --max-clients N         server: refuse further clients (default 256)\n"
           "  --timeout-ms MS         drop a peer that sent nothing for MS (default 5000)\n"
           "  --flush-timeout-ms MS   server: after the last tick, wait this long for clients to catch up\n"
           "  --no-predict            client: do not run predictable RPCs locally before the server confirms\n"
           "  --no-list-deltas        server: send changed strings and lists whole, not as deltas against what the\n"
           "                          client acknowledged\n"
           "  --budget BYTES          server: at most about BYTES of payload per snapshot and client; changes that do\n"
           "                          not fit wait, the longest-waiting (and nearest to the client's view) first\n"
           "  --key HEX               encrypt and authenticate every datagram (XChaCha20-Poly1305) with this\n"
           "                          pre-shared 32-byte key, 64 hex digits (or the SIJO_KEY environment variable)\n"
           "  --sim-loss P --sim-dup P --sim-latency-ms MS --sim-jitter-ms MS --sim-seed S\n"
           "                          simulate a bad network on every datagram this process sends\n"
           "distributed simulation (the same results as one process; see sijoc explain [shards]):\n"
           "  --shards N              run N shard worlds in this process\n"
           "  --shard-of N --shard-id I --peers HOST:PORT,...   this process is shard I of N (one\n"
           "                          HOST:PORT per shard, in shard order); shard 0 is the coordinator\n"
           "                          and clients' gateway (--server PORT, or the in-process client)\n"
           "  --checkpoint-dir DIR --checkpoint-every K   checkpoints every K ticks and message logs\n"
           "  --recover               start again from this shard's newest checkpoint (shards 1..N-1)\n"
           "  --shard-timeout-ms MS   warn when a barrier waits longer for a shard (default 30000)\n");
}

static char** sijo_argv; /* a shard process may start itself again with another environment */
static int sijo_main(int argc, char** argv) {
    sijo_platform_init();
    sijo_argv = argv;
    SijoOpts o;
    memset(&o, 0, sizeof o);
    o.ticks = 600; o.shards = 1; o.summary = 1; o.seed = 1; o.rate = 60; o.max_clients = 256;
    o.wait_timeout = 30; o.timeout = 5; o.flush_timeout = 10; o.connect_timeout = 10; o.sim.seed = 1;
    o.shard_timeout = 30; o.ckpt_every = 60;
    for (int i = 1; i < argc; i++) {
        const char* a = argv[i];
        int more = i + 1 < argc;
        /* numeric option values must be numbers (atoi would read "abc" as 0) */
        if (more && a[0] == '-' && a[1] == '-' && strcmp(a, "--load-state") && strcmp(a, "--save-state") && strcmp(a, "--live-dir") && strcmp(a, "--connect") && strcmp(a, "--profile") &&
            strcmp(a, "--peers") && strcmp(a, "--checkpoint-dir") && strcmp(a, "--store")) {
            const char* v = argv[i + 1];
            char* end = NULL;
            int takes = !strcmp(a, "--ticks") || !strcmp(a, "--render-every") || !strcmp(a, "--shards") || !strcmp(a, "--seed") || !strcmp(a, "--server") ||
                        !strcmp(a, "--tick-rate") || !strcmp(a, "--wait-clients") || !strcmp(a, "--wait-timeout-ms") || !strcmp(a, "--max-clients") ||
                        !strcmp(a, "--timeout-ms") || !strcmp(a, "--flush-timeout-ms") || !strcmp(a, "--budget") || !strncmp(a, "--sim-", 6) ||
                        !strcmp(a, "--shard-of") || !strcmp(a, "--shard-id") || !strcmp(a, "--checkpoint-every") || !strcmp(a, "--die-at-tick") || !strcmp(a, "--shard-timeout-ms") || !strcmp(a, "--store-every");
            if (takes) {
                (void)strtod(v, &end);
                if (end == v || *end) { fprintf(stderr, "option %s needs a number, not '%s'\n", a, v); return 2; }
                if (!strcmp(a, "--ticks") && v[0] == '-') { fprintf(stderr, "--ticks must be >= 0 (0 runs until stopped)\n"); return 2; }
            }
        }
        if (!strcmp(a, "--ticks") && more) o.ticks = atoi(argv[++i]);
        else if (!strcmp(a, "--load-state") && more) o.load_path = argv[++i];
        else if (!strcmp(a, "--save-state") && more) o.save_path = argv[++i];
        else if (!strcmp(a, "--profile") && more) o.profile_path = argv[++i];
        else if (!strcmp(a, "--store") && more) sijo_store_dir = argv[++i];
        else if (!strcmp(a, "--store-every") && more) { sijo_store_every = atoi(argv[++i]); if (sijo_store_every < 1) sijo_store_every = 1; }
        else if (!strcmp(a, "--live-dir") && more) o.live_dir = argv[++i];
        else if (!strcmp(a, "--render-every") && more) o.render_every = atoi(argv[++i]);
        else if (!strcmp(a, "--realtime")) o.realtime = 1;
        else if (!strcmp(a, "--shards") && more) o.shards = atoi(argv[++i]);
        else if (!strcmp(a, "--seed") && more) o.seed = (uint64_t)strtoull(argv[++i], NULL, 10);
        else if (!strcmp(a, "--render")) o.render = 1;
        else if (!strcmp(a, "--quiet")) o.quiet = 1;
        else if (!strcmp(a, "--timing")) o.timing = 1;
        else if (!strcmp(a, "--no-summary")) o.summary = 0;
        else if (!strcmp(a, "--server") && more) { o.mode = 1; o.port = atoi(argv[++i]); }
        else if (!strcmp(a, "--connect") && more) { o.mode = 2; o.connect = argv[++i]; }
        else if (!strcmp(a, "--tick-rate") && more) o.rate = atoi(argv[++i]);
        else if (!strcmp(a, "--wait-clients") && more) o.wait_clients = atoi(argv[++i]);
        else if (!strcmp(a, "--wait-timeout-ms") && more) o.wait_timeout = atof(argv[++i]) / 1000.0;
        else if (!strcmp(a, "--max-clients") && more) o.max_clients = atoi(argv[++i]);
        else if (!strcmp(a, "--timeout-ms") && more) o.timeout = atof(argv[++i]) / 1000.0;
        else if (!strcmp(a, "--flush-timeout-ms") && more) o.flush_timeout = atof(argv[++i]) / 1000.0;
        else if (!strcmp(a, "--sim-loss") && more) o.sim.loss = atof(argv[++i]);
        else if (!strcmp(a, "--sim-dup") && more) o.sim.dup = atof(argv[++i]);
        else if (!strcmp(a, "--sim-latency-ms") && more) o.sim.lat_ms = atof(argv[++i]);
        else if (!strcmp(a, "--sim-jitter-ms") && more) o.sim.jit_ms = atof(argv[++i]);
        else if (!strcmp(a, "--sim-seed") && more) o.sim.seed = (uint64_t)strtoull(argv[++i], NULL, 10);
        else if (!strcmp(a, "--key") && more) {
            if (!sijo_parse_key(argv[++i], o.key)) { fprintf(stderr, "--key needs 64 hex digits (a 32-byte key)\n"); return 2; }
            o.has_key = 1;
        }
        else if (!strcmp(a, "--no-predict")) o.no_predict = 1;
        else if (!strcmp(a, "--no-list-deltas")) sijo_list_deltas = 0;
        else if (!strcmp(a, "--budget") && more) o.budget = atoi(argv[++i]);
        else if (!strcmp(a, "--shard-of") && more) o.shard_of = atoi(argv[++i]);
        else if (!strcmp(a, "--shard-id") && more) o.shard_id = atoi(argv[++i]);
        else if (!strcmp(a, "--peers") && more) o.peers = argv[++i];
        else if (!strcmp(a, "--checkpoint-dir") && more) o.ckpt_dir = argv[++i];
        else if (!strcmp(a, "--checkpoint-every") && more) o.ckpt_every = atoi(argv[++i]);
        else if (!strcmp(a, "--recover")) o.recover = 1;
        else if (!strcmp(a, "--die-at-tick") && more) o.die_at = atoi(argv[++i]);
        else if (!strcmp(a, "--shard-timeout-ms") && more) o.shard_timeout = atof(argv[++i]) / 1000.0;
        else if (!strcmp(a, "--help") || !strcmp(a, "-h")) { sijo_usage(); return 0; }
        else { fprintf(stderr, "unknown option %s\n", a); return 2; }
    }
#ifdef _OPENMP
    if (omp_get_max_threads() > SIJO_MAX_THREADS) omp_set_num_threads(SIJO_MAX_THREADS); /* per-thread buffers are fixed-size */
#endif
    if (o.shards < 1) o.shards = 1;
    if (o.shards > SIJO_MAX_THREADS) o.shards = SIJO_MAX_THREADS;
    if (o.rate < 1) o.rate = 1;
    if (o.rate > 1000) o.rate = 1000;
    if (o.max_clients < 1) o.max_clients = 1;
    if (o.max_clients > 65535) o.max_clients = 65535;
    if (o.budget < 0) o.budget = 0;
    if (o.budget > 0 && o.budget < 64) o.budget = 64;
    o.sim.loss = o.sim.loss < 0 ? 0 : o.sim.loss > 1 ? 1 : o.sim.loss;
    o.sim.dup = o.sim.dup < 0 ? 0 : o.sim.dup > 1 ? 1 : o.sim.dup;
    if (o.sim.lat_ms < 0) o.sim.lat_ms = 0;
    if (o.sim.jit_ms < 0) o.sim.jit_ms = 0;
    o.sim_on = o.sim.loss > 0 || o.sim.dup > 0 || o.sim.lat_ms > 0 || o.sim.jit_ms > 0;
    if (!o.has_key && getenv("SIJO_KEY")) {
        if (!sijo_parse_key(getenv("SIJO_KEY"), o.key)) { fprintf(stderr, "SIJO_KEY needs 64 hex digits (a 32-byte key)\n"); return 2; }
        o.has_key = 1;
    }
    if (o.mode == 1 && (o.port < 0 || o.port > 65535)) { fprintf(stderr, "bad port %d\n", o.port); return 2; }
    if (o.shards > 1 || o.shard_of > 1) {
        int rc = sijo_shard_setup(&o);
        if (rc >= 0) return rc;
    }
    if (o.mode == 1) return sijo_server_main(&o);
    if (o.mode == 2) return sijo_client_main(&o);
    return sijo_loopback_main(&o);
}
