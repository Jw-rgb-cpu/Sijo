/* sijo_shard_net.h - shard process modes: N shard worlds in one process (--shards N), or
 * one shard per process (--shard-of N --shard-id I --peers ...) over a reliable channel on the
 * datagram transport of sijo_net.h; checkpoints, message logs and recovery. The world side of the
 * exchange is sijo_shard.h. */

static SijoOpts* sijo_sho;

/* ---------------- statistics ---------------- */
typedef struct { int64_t barriers, handoffs, rebalances, bytes, msgs, cmds, spawns, syncs, leaves; double sum_imb, max_imb; int64_t nimb; } SijoShStat;
static void sijo_sh_stat_add(SijoShStat* t, const SijoShard* s) {
    t->barriers = s->barriers > t->barriers ? s->barriers : t->barriers;
    t->handoffs += s->handoffs; t->bytes += s->sent_bytes; t->msgs += s->sent_msgs; t->cmds += s->xcmds;
    t->spawns += s->xspawns; t->syncs += s->syncs; t->leaves += s->leaves;
    if (s->rebalances > t->rebalances) t->rebalances = s->rebalances;
    if (s->id == 0) { t->sum_imb = s->sum_imb; t->max_imb = s->max_imb; t->nimb = s->nimb; }
}
static void sijo_sh_stat_print(const SijoShStat* t, int n, int ticks, const char* how) {
    double tk = ticks > 0 ? (double)ticks : 1.0;
    printf("shards: %d %s, %lld barriers (%.1f per tick), %lld handoffs, %lld rebalances, load imbalance avg %.2f / max %.2f\n",
           n, how, (long long)t->barriers, (double)t->barriers / tk, (long long)t->handoffs, (long long)t->rebalances,
           t->nimb ? t->sum_imb / (double)t->nimb : 1.0, t->max_imb > 0 ? t->max_imb : 1.0);
    printf("shard exchange: %lld bytes in %lld messages (%.1f B/tick), %lld commands, %lld spawns, %lld component copies, %lld halo exits\n",
           (long long)t->bytes, (long long)t->msgs, (double)t->bytes / tk, (long long)t->cmds, (long long)t->spawns, (long long)t->syncs, (long long)t->leaves);
}

/* ---------------- --shards N: every shard's world in this process ---------------- */
static void sijo_sh_xfer_local(int kind) {
    (void)kind;
    for (int i = 0; i < sijo_shd.nw; i++)
        for (int j = 0; j < sijo_shd.nw; j++) {
            if (i == j) continue;
            SijoShard* to = sijo_shd.ws[j]->sh;
            to->in[i] = sijo_shd.ws[i]->sh->out[j].p;
            to->inlen[i] = sijo_shd.ws[i]->sh->out[j].n;
        }
}
static void sijo_sh_step_inproc(SijoWorld* S) {
    sijo_sh_tick_all(S->tick);
    for (int i = 1; i < sijo_shd.nw; i++) sijo_reset_arenas(sijo_shd.ws[i]);
}
static void sijo_sh_finish_inproc(SijoWorld* S) {
    (void)S;
    sijo_sh_barrier(SIJO_SHK_GATHER); /* the coordinator now holds the whole world */
}
static int32_t sijo_sh_ticks;
static void sijo_sh_stats_inproc(SijoWorld* S) {
    SijoShStat t;
    memset(&t, 0, sizeof t);
    for (int i = 0; i < sijo_shd.nw; i++) sijo_sh_stat_add(&t, sijo_shd.ws[i]->sh);
    sijo_sh_stat_print(&t, sijo_shd.nw, S->tick - sijo_sh_ticks, "(worlds in this process)");
}
static void sijo_sh_start_inproc(SijoWorld* S) {
    int n = sijo_sho->shards;
    SijoWorld** ws = (SijoWorld**)calloc((size_t)n, sizeof(SijoWorld*));
    ws[0] = S;
    sijo_sh_init(S, 0, n);
    for (int i = 1; i < n; i++) {
        SijoWorld* W = (SijoWorld*)calloc(1, sizeof(SijoWorld));
        sijo_world_init(W, 0, S->seed, n);
        W->quiet = S->quiet; W->nthreads = S->nthreads;
        sijo_sh_init(W, i, n);
        if (sijo_sho->load_path) {
            if (!sijo_load_state_v(W, sijo_sho->load_path, 0)) { fprintf(stderr, "sijo shard %d: cannot load %s\n", i, sijo_sho->load_path); exit(2); }
            sijo_sh_init(W, i, n); /* loading replaced the world (and freed its shard state) */
        } else {
            sijo_game_start(W);
        }
        ws[i] = W;
    }
    sijo_sh_ticks = S->tick;
    for (int i = 0; i < n; i++) sijo_sh_initial(ws[i]);
    sijo_shd.ws = ws; sijo_shd.nw = n; sijo_shd.xfer = sijo_sh_xfer_local;
    sijo_step = sijo_sh_step_inproc;
    sijo_finish_hook = sijo_sh_finish_inproc;
    sijo_stats_hook = sijo_sh_stats_inproc;
}


/* ======================= shard processes: --shard-of N --shard-id I --peers A0,A1,... =======================
 * A reliable channel per pair of shards on the datagram transport of sijo_net.h (so --key encrypts
 * it and --sim-* drops, delays, reorders and duplicates its datagrams): messages are cut into
 * numbered fragments, the receiver acknowledges cumulatively plus a 64-fragment bitmap, the sender
 * re-sends what is not acknowledged (the RPC channel's sequence + re-send until ack, per fragment),
 * and messages are delivered whole and in order. Each process start has a random epoch; a peer
 * that comes back with a new epoch gets fresh streams and, when it recovers from a checkpoint,
 * the messages it missed from this shard's log. */
#define SIJO_PK_SHARD 9
#define SIJO_RL_DATA 1
#define SIJO_RL_ACK 2
#define SIJO_RL_HELLO 3
#define SIJO_RL_HDR (SIJO_HDR + 3 + 4 + 4 + 1 + 2)
#define SIJO_RL_FRAG (SIJO_MTU - SIJO_RL_HDR)
#define SIJO_RL_WIN 1024 /* fragments in flight per peer */
typedef struct { uint32_t seq; uint16_t len; uint8_t last, acked; double sent; int sends; uint8_t* data; } SijoRlOut;
typedef struct { uint8_t have, last; uint16_t len; uint8_t data[SIJO_RL_FRAG]; } SijoRlIn;
typedef struct { uint8_t* p; size_t n; } SijoRlMsg;
typedef struct { uint64_t bno; uint8_t* p; size_t n; } SijoRlLog;
typedef struct {
    SijoAddr addr;
    uint32_t epoch;          /* the peer's current incarnation (0: not heard of yet) */
    int knows_me;            /* the peer addressed something to this incarnation */
    /* sending */
    uint32_t snext, sbase;   /* next fragment number; every fragment below sbase is acknowledged */
    SijoRlOut* q; int32_t qh, qn, qcap; /* fragments sbase.. (deque) */
    double minrtt;           /* the network's round trip (the smallest seen, slowly forgetting) */
    /* receiving */
    uint32_t rnext;
    SijoRlIn* rw;            /* SIJO_RL_WIN slots, fragment f in slot f % WIN */
    SijoShW asmb;
    SijoRlMsg* mq; int32_t mh, mn, mcap;
    int ack_due;
    double last_rx, warned;
    /* messages sent since the oldest kept checkpoint (replayed to a peer that recovers) */
    SijoRlLog* log; int32_t nlog, clog;
    uint64_t last_bno;       /* newest barrier received from this peer */
    SijoRlMsg cur;           /* the message installed at the current barrier */
    uint64_t resends, frags_out, frags_in, dups;
} SijoRlPeer;
typedef struct {
    SijoSock sock;
    int id, n;
    uint32_t epoch;
    SijoRlPeer peer[SIJO_SH_MAXN];
    uint16_t pseq;
    uint64_t resume_bno;     /* recovering: replay everything after this barrier (else UINT64_MAX) */
    uint64_t replays;
    double last_rx;          /* when the last datagram arrived (a message in flight: do not sleep) */
} SijoRlNet;
static SijoRlNet sijo_rl;

static void sijo_rl_hdr(SijoBuf* b, uint32_t sub, int to) {
    sijo_w8(b, SIJO_PK_SHARD); sijo_w32(b, sijo_rl.epoch); sijo_w16(b, sijo_rl.pseq++);
    sijo_w8(b, sub); sijo_w8(b, (uint32_t)sijo_rl.id); sijo_w8(b, (uint32_t)to);
    sijo_w32(b, sijo_rl.peer[to].epoch);
}
static void sijo_rl_hello(int to, int reply) {
    uint8_t pkt[64];
    SijoBuf b = {pkt, 0, sizeof pkt};
    sijo_rl_hdr(&b, SIJO_RL_HELLO, to);
    sijo_w32(&b, SIJO_SH_PROTO); sijo_w32(&b, (uint32_t)sijo_rl.n); sijo_w64(&b, sijo_rl.resume_bno); sijo_w8(&b, (uint32_t)reply);
    sijo_sock_send(&sijo_rl.sock, sijo_rl.peer[to].addr, pkt, (int)b.n, sijo_now());
}
static void sijo_rl_xmit(int to, SijoRlOut* f, double now) {
    uint8_t pkt[SIJO_MTU];
    SijoBuf b = {pkt, 0, sizeof pkt};
    sijo_rl_hdr(&b, SIJO_RL_DATA, to);
    sijo_w32(&b, f->seq); sijo_w8(&b, f->last); sijo_w16(&b, f->len);
    sijo_wbytes(&b, f->data, f->len);
    sijo_sock_send(&sijo_rl.sock, sijo_rl.peer[to].addr, pkt, (int)b.n, now);
    if (f->sends) sijo_rl.peer[to].resends++;
    f->sends++;
    f->sent = now;
    sijo_rl.peer[to].frags_out++;
}
static void sijo_rl_ack(int to) {
    SijoRlPeer* p = &sijo_rl.peer[to];
    uint8_t pkt[64];
    SijoBuf b = {pkt, 0, sizeof pkt};
    sijo_rl_hdr(&b, SIJO_RL_ACK, to);
    uint64_t bits = 0;
    for (int k = 0; k < 64; k++) {
        uint32_t f = p->rnext + 1u + (uint32_t)k;
        if (p->rw && p->rw[f % SIJO_RL_WIN].have) bits |= 1ull << k;
    }
    sijo_w32(&b, p->rnext); sijo_w64(&b, bits);
    sijo_sock_send(&sijo_rl.sock, p->addr, pkt, (int)b.n, sijo_now());
    p->ack_due = 0;
}
/* a peer started again (new epoch): both streams restart from fragment 0 */
static void sijo_rl_reset(SijoRlPeer* p) {
    for (int32_t i = 0; i < p->qn; i++) free(p->q[(p->qh + i) % p->qcap].data);
    p->qh = p->qn = 0;
    p->snext = p->sbase = 0;
    p->rnext = 0;
    if (p->rw) memset(p->rw, 0, sizeof(SijoRlIn) * SIJO_RL_WIN);
    p->asmb.n = 0;
    for (int32_t i = 0; i < p->mn; i++) free(p->mq[(p->mh + i) % p->mcap].p);
    p->mh = p->mn = 0;
    p->knows_me = 0;
}
static void sijo_rl_send(int to, const uint8_t* d, size_t n) {
    SijoRlPeer* p = &sijo_rl.peer[to];
    size_t off = 0;
    double now = sijo_now();
    do {
        size_t k = n - off < SIJO_RL_FRAG ? n - off : SIJO_RL_FRAG;
        if (p->qn == p->qcap) {
            int32_t nc = p->qcap ? p->qcap * 2 : 256;
            SijoRlOut* nq = (SijoRlOut*)sijo_xrealloc(NULL, sizeof(SijoRlOut) * (size_t)nc);
            for (int32_t i = 0; i < p->qn; i++) nq[i] = p->q[(p->qh + i) % p->qcap];
            free(p->q);
            p->q = nq; p->qcap = nc; p->qh = 0;
        }
        SijoRlOut* f = &p->q[(p->qh + p->qn) % p->qcap];
        p->qn++;
        memset(f, 0, sizeof *f);
        f->seq = p->snext++;
        f->len = (uint16_t)k;
        f->last = off + k == n;
        f->data = (uint8_t*)sijo_xrealloc(NULL, k ? k : 1);
        memcpy(f->data, d + off, k);
        if (f->seq - p->sbase < SIJO_RL_WIN && p->epoch) sijo_rl_xmit(to, f, now);
        off += k;
    } while (off < n);
}
/* the messages this shard sent, kept for a peer that recovers from a checkpoint */
static void sijo_rl_log(int to, uint64_t bno, const uint8_t* d, size_t n) {
    SijoRlPeer* p = &sijo_rl.peer[to];
    if (p->nlog == p->clog) { p->clog = p->clog ? p->clog * 2 : 256; p->log = (SijoRlLog*)sijo_xrealloc(p->log, sizeof(SijoRlLog) * (size_t)p->clog); }
    SijoRlLog* e = &p->log[p->nlog++];
    e->bno = bno; e->n = n;
    e->p = (uint8_t*)sijo_xrealloc(NULL, n ? n : 1);
    memcpy(e->p, d, n);
}
static void sijo_rl_prune(uint64_t keep_after) {
    for (int j = 0; j < sijo_rl.n; j++) {
        SijoRlPeer* p = &sijo_rl.peer[j];
        int32_t k = 0;
        while (k < p->nlog && p->log[k].bno <= keep_after) free(p->log[k++].p);
        memmove(p->log, p->log + k, sizeof(SijoRlLog) * (size_t)(p->nlog - k));
        p->nlog -= k;
    }
}
static void sijo_rl_replay(int to, uint64_t after) {
    SijoRlPeer* p = &sijo_rl.peer[to];
    if (after != UINT64_MAX && p->nlog && p->log[0].bno > after + 1) {
        fprintf(stderr, "sijo shard %d: shard %d recovers from barrier %llu, but this shard's log starts at %llu (its checkpoints are older than the ones kept)\n",
                sijo_rl.id, to, (unsigned long long)after, (unsigned long long)p->log[0].bno);
        return;
    }
    int64_t k = 0;
    for (int32_t i = 0; i < p->nlog; i++) if (after != UINT64_MAX && p->log[i].bno > after) { sijo_rl_send(to, p->log[i].p, p->log[i].n); k++; }
    sijo_rl.replays += (uint64_t)k;
    if (k) fprintf(stderr, "sijo shard %d: replayed %lld logged messages to recovering shard %d (after barrier %llu)\n", sijo_rl.id, (long long)k, to, (unsigned long long)after);
}
static void sijo_rl_packet(SijoAddr from, const uint8_t* d, int len, double now) {
    SijoRB r = {d, (size_t)len, 0, 0};
    uint32_t type = sijo_r8(&r), epoch = sijo_r32(&r);
    sijo_r16(&r);
    uint32_t sub = sijo_r8(&r), src = sijo_r8(&r), dst = sijo_r8(&r), to_epoch = sijo_r32(&r);
    if (r.bad || type != SIJO_PK_SHARD || (int)dst != sijo_rl.id || (int)src >= sijo_rl.n || (int)src == sijo_rl.id || !epoch) return;
    SijoRlPeer* p = &sijo_rl.peer[src];
    (void)from;
    if (sub == SIJO_RL_HELLO) {
        uint32_t proto = sijo_r32(&r), n = sijo_r32(&r);
        uint64_t resume = sijo_r64(&r);
        uint32_t reply = sijo_r8(&r);
        if (r.bad) return;
        if (proto != SIJO_SH_PROTO || (int)n != sijo_rl.n) {
            fprintf(stderr, "sijo shard %d: shard %d runs a different program or shard count (protocol %08x/%u, expected %08x/%d)\n", sijo_rl.id, (int)src, proto, n, (unsigned)SIJO_SH_PROTO, sijo_rl.n);
            /* so that it refuses this one too, instead of waiting for it */
            for (int k = 0; k < 3; k++) sijo_rl_hello((int)src, 1);
            sijo_sock_pump(&sijo_rl.sock, sijo_now() + 1.0);
            exit(6);
        }
        if (epoch != p->epoch) {
            int restarted = p->epoch != 0;
            sijo_rl_reset(p);
            p->epoch = epoch;
            if (restarted) fprintf(stderr, "sijo shard %d: shard %d started again%s\n", sijo_rl.id, (int)src, resume != UINT64_MAX ? " from a checkpoint" : "");
            p->last_bno = resume != UINT64_MAX ? resume : 0;
            if (resume != UINT64_MAX) sijo_rl_replay((int)src, resume);
        }
        if (to_epoch == sijo_rl.epoch) p->knows_me = 1;
        if (!reply || to_epoch != sijo_rl.epoch) sijo_rl_hello((int)src, 1);
        p->last_rx = now;
        return;
    }
    if (epoch != p->epoch || to_epoch != sijo_rl.epoch) return; /* an older incarnation, or not addressed to this one */
    p->knows_me = 1;
    p->last_rx = now;
    if (sub == SIJO_RL_DATA) {
        uint32_t seq = sijo_r32(&r), last = sijo_r8(&r), n = sijo_r16(&r);
        const uint8_t* data = sijo_rbytes(&r, n);
        if (r.bad || n > SIJO_RL_FRAG) return;
        p->ack_due = 1;
        p->frags_in++;
        if (seq - p->rnext >= SIJO_RL_WIN) { p->dups++; return; } /* old (re-sent) or too far ahead */
        if (!p->rw) p->rw = (SijoRlIn*)calloc(SIJO_RL_WIN, sizeof(SijoRlIn));
        SijoRlIn* slot = &p->rw[seq % SIJO_RL_WIN];
        if (slot->have) { p->dups++; return; }
        slot->have = 1; slot->last = (uint8_t)last; slot->len = (uint16_t)n;
        memcpy(slot->data, data, n);
        while (p->rw[p->rnext % SIJO_RL_WIN].have) {
            SijoRlIn* f = &p->rw[p->rnext % SIJO_RL_WIN];
            sijo_shw_bytes(&p->asmb, f->data, f->len);
            f->have = 0;
            p->rnext++;
            if (f->last) {
                if (p->mn == p->mcap) {
                    int32_t nc = p->mcap ? p->mcap * 2 : 16;
                    SijoRlMsg* nm = (SijoRlMsg*)sijo_xrealloc(NULL, sizeof(SijoRlMsg) * (size_t)nc);
                    for (int32_t i = 0; i < p->mn; i++) nm[i] = p->mq[(p->mh + i) % p->mcap];
                    free(p->mq);
                    p->mq = nm; p->mcap = nc; p->mh = 0;
                }
                SijoRlMsg* m = &p->mq[(p->mh + p->mn) % p->mcap];
                p->mn++;
                m->n = p->asmb.n;
                m->p = (uint8_t*)sijo_xrealloc(NULL, p->asmb.n ? p->asmb.n : 1);
                memcpy(m->p, p->asmb.p, p->asmb.n);
                p->asmb.n = 0;
            }
        }
    } else if (sub == SIJO_RL_ACK) {
        uint32_t cum = sijo_r32(&r);
        uint64_t bits = sijo_r64(&r);
        if (r.bad || cum - p->sbase > p->snext - p->sbase) return;
        while (p->qn && p->q[p->qh].seq < cum) {
            SijoRlOut* f = &p->q[p->qh];
            /* a peer busy with its tick acknowledges late: the smallest round trip is the network's */
            if (f->sends == 1) { double rtt = now - f->sent; p->minrtt = p->minrtt <= 0 || rtt < p->minrtt ? rtt : p->minrtt + 0.01 * (rtt - p->minrtt); }
            free(f->data);
            p->qh = (p->qh + 1) % p->qcap;
            p->qn--;
        }
        p->sbase = cum;
        int top = -1; /* the highest fragment the bitmap acknowledges */
        for (int32_t i = 0; i < p->qn; i++) {
            SijoRlOut* f = &p->q[(p->qh + i) % p->qcap];
            uint32_t d2 = f->seq - cum - 1u;
            if (d2 < 64 && ((bits >> d2) & 1)) { f->acked = 1; top = (int)d2; }
        }
        /* fast re-send: a fragment below one that arrived was lost (if it was not just re-sent) */
        for (int32_t i = 0; i < p->qn && top >= 0; i++) {
            SijoRlOut* f = &p->q[(p->qh + i) % p->qcap];
            int d2 = (int)(f->seq - cum) - 1;
            if (d2 >= top) break;
            if (!f->acked && f->sends && now - f->sent > (p->minrtt > 0 ? p->minrtt : 0.001)) sijo_rl_xmit((int)src, f, now);
        }
    }
}
/* receive everything waiting, (re-)send what is due, acknowledge */
static void sijo_rl_pump(void) {
    uint8_t buf[2048];
    SijoAddr from;
    double now = sijo_now();
    sijo_sock_pump(&sijo_rl.sock, now);
    for (;;) {
        int k = sijo_sock_recv(&sijo_rl.sock, &from, buf, (int)sizeof buf);
        if (k < 0) break;
        sijo_rl.last_rx = now;
        sijo_rl_packet(from, buf, k, now);
    }
    for (int j = 0; j < sijo_rl.n; j++) {
        if (j == sijo_rl.id) continue;
        SijoRlPeer* p = &sijo_rl.peer[j];
        if (p->ack_due) sijo_rl_ack(j);
        if (!p->epoch) continue;
        double rto = p->minrtt > 0 ? 1.5 * p->minrtt + 0.001 : 0.02;
        for (int32_t i = 0; i < p->qn; i++) {
            SijoRlOut* f = &p->q[(p->qh + i) % p->qcap];
            if (f->seq - p->sbase >= SIJO_RL_WIN) break;
            if (f->acked) continue;
            double wait = rto * (double)(1 << (f->sends > 6 ? 6 : (f->sends ? f->sends - 1 : 0)));
            if (wait > 0.25) wait = 0.25;
            if (!f->sends || now - f->sent >= wait) sijo_rl_xmit(j, f, now);
        }
    }
}
static int sijo_rl_idle(void) {
    for (int j = 0; j < sijo_rl.n; j++) if (j != sijo_rl.id && sijo_rl.peer[j].qn) return 0;
    return 1;
}
static void sijo_rl_wait(double secs) {
    double due = sijo_sock_next_due(&sijo_rl.sock) - sijo_now();
    if (due < secs) secs = due;
    if (secs > 0.002) secs = 0.002;
    if (secs > 0) sijo_sock_wait(&sijo_rl.sock, secs);
}
/* every peer has an epoch and knows this shard's */
static void sijo_rl_handshake(void) {
    double next = 0, start = sijo_now(), warn = start + 5.0;
    for (;;) {
        int ok = 1;
        for (int j = 0; j < sijo_rl.n; j++) if (j != sijo_rl.id && (!sijo_rl.peer[j].epoch || !sijo_rl.peer[j].knows_me)) ok = 0;
        if (ok) return;
        double now = sijo_now();
        if (now >= next) {
            for (int j = 0; j < sijo_rl.n; j++) if (j != sijo_rl.id && (!sijo_rl.peer[j].epoch || !sijo_rl.peer[j].knows_me)) sijo_rl_hello(j, 0);
            next = now + 0.05;
        }
        if (now > warn) {
            fprintf(stderr, "sijo shard %d: waiting for the other shards", sijo_rl.id);
            for (int j = 0; j < sijo_rl.n; j++) if (j != sijo_rl.id && !sijo_rl.peer[j].epoch) fprintf(stderr, " %d", j);
            fprintf(stderr, "\n");
            warn = now + 5.0;
        }
        if (sijo_stop_flag) exit(130);
        sijo_rl_pump();
        sijo_rl_wait(0.002);
    }
}
/* one barrier: send this shard's message to every peer, take the peers' messages of this barrier */
static void sijo_sh_xfer_net(int kind) {
    (void)kind;
    SijoWorld* w = sijo_shd.ws[0];
    SijoShard* s = w->sh;
    for (int j = 0; j < s->n; j++) {
        if (j == s->id) continue;
        SijoRlPeer* p = &sijo_rl.peer[j];
        free(p->cur.p); p->cur.p = NULL; p->cur.n = 0;
        if (sijo_sho->ckpt_dir) sijo_rl_log(j, s->bno, s->out[j].p, s->out[j].n);
        sijo_rl_send(j, s->out[j].p, s->out[j].n);
    }
    double t0 = sijo_now();
    for (;;) {
        int missing = 0;
        for (int j = 0; j < s->n; j++) {
            if (j == s->id) continue;
            SijoRlPeer* p = &sijo_rl.peer[j];
            while (!p->cur.p && p->mn) {
                SijoRlMsg m = p->mq[p->mh];
                p->mh = (p->mh + 1) % p->mcap;
                p->mn--;
                uint64_t bno = 0;
                if (m.n >= 10) for (int k = 0; k < 8; k++) bno |= (uint64_t)m.p[2 + k] << (8 * k);
                if (bno < s->bno) { free(m.p); continue; } /* a message re-sent by a recovering shard */
                if (bno > s->bno) { fprintf(stderr, "sijo shard %d: shard %d is at barrier %llu, this shard at %llu\n", s->id, j, (unsigned long long)bno, (unsigned long long)s->bno); exit(9); }
                p->cur = m;
                p->last_bno = bno;
            }
            if (!p->cur.p) missing = 1;
        }
        if (!missing) break;
        double now = sijo_now();
        if (now - t0 > sijo_sho->shard_timeout) {
            for (int j = 0; j < s->n; j++)
                if (j != s->id && !sijo_rl.peer[j].cur.p && now - sijo_rl.peer[j].warned > sijo_sho->shard_timeout) {
                    fprintf(stderr, "sijo shard %d: no barrier %llu message from shard %d for %.0f s (waiting: restart it with --recover)\n", s->id, (unsigned long long)s->bno, j, now - t0);
                    sijo_rl.peer[j].warned = now;
                }
        }
        if (sijo_stop_flag && s->id != 0) exit(130);
        sijo_rl_pump();
        /* while fragments keep coming (or just after sending), poll without sleeping: waking up
           for every fragment costs more than the fragments (a VM: tens of microseconds each) */
        double now2 = sijo_now();
        if (now2 - sijo_rl.last_rx > 0.0003 && now2 - t0 > 0.0003) sijo_rl_wait(0.002);
    }
    for (int j = 0; j < s->n; j++) {
        if (j == s->id) continue;
        s->in[j] = sijo_rl.peer[j].cur.p;
        s->inlen[j] = sijo_rl.peer[j].cur.n;
    }
}
/* after the last barrier: until every peer acknowledged everything (or a few seconds) */
static void sijo_rl_linger(void) {
    double end = sijo_now() + 5.0;
    while (sijo_now() < end && (!sijo_rl_idle() || sijo_rl.sock.dq_n)) { sijo_rl_pump(); sijo_rl_wait(0.002); }
    double quiet = sijo_now() + 0.2; /* answer the peers' last re-sends */
    while (sijo_now() < quiet) { sijo_rl_pump(); sijo_rl_wait(0.002); }
}
static int sijo_rl_open(const SijoOpts* o) {
    memset(&sijo_rl, 0, sizeof sijo_rl);
    sijo_rl.id = o->shard_id; sijo_rl.n = o->shard_of;
    sijo_rl.resume_bno = UINT64_MAX;
    /* --peers: one HOST:PORT per shard, in shard order (this shard's entry is the port it binds) */
    const char* q = o->peers;
    for (int j = 0; j < o->shard_of; j++) {
        if (!q || !*q) { fprintf(stderr, "--peers needs %d HOST:PORT entries (one per shard, comma-separated)\n", o->shard_of); return 0; }
        const char* e = strchr(q, ',');
        char hp[300];
        size_t len = e ? (size_t)(e - q) : strlen(q);
        if (len >= sizeof hp) return 0;
        memcpy(hp, q, len); hp[len] = 0;
        if (!sijo_resolve(hp, &sijo_rl.peer[j].addr)) { fprintf(stderr, "--peers: cannot resolve '%s'\n", hp); return 0; }
        q = e ? e + 1 : NULL;
    }
    int port = ntohs(sijo_rl.peer[o->shard_id].addr.port);
    if (sijo_udp_open(&sijo_rl.sock, port) < 0) { fprintf(stderr, "sijo shard %d: cannot bind udp port %d: %s\n", o->shard_id, port, sijo_sock_errstr()); return 0; }
    /* the peers' addresses as this socket reaches them (IPv4 peers through a dual-stack socket) */
    if (o->has_key) sijo_sock_key(&sijo_rl.sock, o->key);
    if (o->sim_on) sijo_sock_sim(&sijo_rl.sock, &o->sim, 100 + (uint64_t)o->shard_id);
    uint64_t rs = sijo_mix((uint64_t)(sijo_now() * 1e9) ^ (sijo_pid() << 20) ^ (uint64_t)port);
    do { rs = sijo_mix(rs); sijo_rl.epoch = (uint32_t)rs; } while (!sijo_rl.epoch);
    return 1;
}

/* ---------------- checkpoints: the world (state file) and the shard's own state ---------------- */
static void sijo_ck_path(char* out, size_t n, const char* dir, int id, int32_t tick, const char* ext) {
    snprintf(out, n, "%s/shard%d-%08d.%s", dir, id, (int)tick, ext);
}
static int sijo_ck_write(SijoWorld* w) {
    SijoShard* s = w->sh;
    const char* dir = sijo_sho->ckpt_dir;
    char st[1024], me[1024], tmp[1100];
    sijo_ck_path(st, sizeof st, dir, s->id, w->tick, "state");
    sijo_ck_path(me, sizeof me, dir, s->id, w->tick, "meta");
    if (!sijo_save_state(w, st)) return 0;
    SijoShW b;
    memset(&b, 0, sizeof b);
    sijo_shw_bytes(&b, "SIJOSHCK", 8);
    sijo_shw_u32(&b, SIJO_SH_VERSION); sijo_shw_u32(&b, SIJO_SH_PROTO);
    sijo_shw_u32(&b, (uint32_t)w->tick); sijo_shw_u64(&b, s->bno); sijo_shw_u32(&b, (uint32_t)s->n); sijo_shw_u32(&b, (uint32_t)s->id);
    for (int k = 0; k < SIJO_SH_MAXN; k++) { sijo_shw_u32(&b, sijo_f2bits(s->bounds[k])); sijo_shw_u32(&b, sijo_f2bits(s->nbounds[k])); sijo_shw_u32(&b, (uint32_t)s->load[k]); }
    sijo_shw_u32(&b, (uint32_t)s->nb_pending); sijo_shw_u32(&b, (uint32_t)s->want_samples); sijo_shw_u32(&b, (uint32_t)s->last_rebal);
    sijo_shw_u64(&b, (uint64_t)s->rebalances); sijo_shw_u64(&b, sijo_d_bits(s->sum_imb)); sijo_shw_u64(&b, sijo_d_bits(s->max_imb)); sijo_shw_u64(&b, (uint64_t)s->nimb);
    sijo_shw_u32(&b, (uint32_t)w->ents.n);
    for (int32_t i = 0; i < w->ents.n; i++) { sijo_shw_u32(&b, (uint32_t)w->ents.shard[i]); sijo_shw_u8(&b, s->hv[i]); sijo_shw_u64(&b, s->mask[i]); }
    snprintf(tmp, sizeof tmp, "%s.tmp", me);
    FILE* f = fopen(tmp, "wb");
    if (!f) { sijo_shw_free(&b); return 0; }
    int ok = fwrite(b.p, 1, b.n, f) == b.n;
    ok &= fclose(f) == 0;
    sijo_shw_free(&b);
    if (!ok || rename(tmp, me) != 0) return 0; /* the meta file last: a checkpoint is complete once it exists */
    return 1;
}
static int sijo_ck_read_meta(SijoWorld* w, const char* path) {
    SijoShard* s = w->sh;
    FILE* f = fopen(path, "rb");
    if (!f) return 0;
    fseek(f, 0, SEEK_END); long len = ftell(f); fseek(f, 0, SEEK_SET);
    uint8_t* d = (uint8_t*)sijo_xrealloc(NULL, (size_t)(len > 0 ? len : 1));
    int ok = len > 0 && fread(d, 1, (size_t)len, f) == (size_t)len;
    fclose(f);
    SijoShR r = {d, (size_t)(len > 0 ? len : 0), 0, !ok, w, s->id};
    const uint8_t* mg = sijo_shr_bytes(&r, 8);
    if (!mg || memcmp(mg, "SIJOSHCK", 8) || sijo_shr_u32(&r) != SIJO_SH_VERSION || sijo_shr_u32(&r) != SIJO_SH_PROTO) { free(d); return 0; }
    int32_t tick = (int32_t)sijo_shr_u32(&r);
    s->bno = sijo_shr_u64(&r);
    if ((int)sijo_shr_u32(&r) != s->n || (int)sijo_shr_u32(&r) != s->id || tick != w->tick) { free(d); return 0; }
    for (int k = 0; k < SIJO_SH_MAXN; k++) { s->bounds[k] = sijo_bits2f(sijo_shr_u32(&r)); s->nbounds[k] = sijo_bits2f(sijo_shr_u32(&r)); s->load[k] = (int32_t)sijo_shr_u32(&r); }
    s->nb_pending = (int)sijo_shr_u32(&r); s->want_samples = (int)sijo_shr_u32(&r); s->last_rebal = (int32_t)sijo_shr_u32(&r);
    s->rebalances = (int64_t)sijo_shr_u64(&r); s->sum_imb = sijo_d_from_bits(sijo_shr_u64(&r)); s->max_imb = sijo_d_from_bits(sijo_shr_u64(&r)); s->nimb = (int64_t)sijo_shr_u64(&r);
    int32_t ns = (int32_t)sijo_shr_u32(&r);
    sijo_sh_fit(w);
    if (ns != w->ents.n) { free(d); return 0; }
    for (int32_t i = 0; i < ns; i++) { w->ents.shard[i] = (int32_t)sijo_shr_u32(&r); s->hv[i] = (uint8_t)sijo_shr_u8(&r); s->mask[i] = sijo_shr_u64(&r); }
    free(d);
    return !r.bad;
}
/* at the end of every K-th tick; the previous checkpoint is kept too (one being written when the
   process dies is not complete), older ones and the log before them go */
static int32_t sijo_ck_prev = -1, sijo_ck_prev2 = -1;
static uint64_t sijo_ck_prev_bno = 0;
static void sijo_ck_maybe(SijoWorld* w) {
    if (!sijo_sho->ckpt_dir || sijo_sho->ckpt_every <= 0 || w->tick % sijo_sho->ckpt_every) return;
    if (!sijo_ck_write(w)) { fprintf(stderr, "sijo shard %d: cannot write a checkpoint in %s\n", w->sh_id, sijo_sho->ckpt_dir); return; }
    if (sijo_ck_prev2 >= 0) {
        char pth[1024];
        sijo_ck_path(pth, sizeof pth, sijo_sho->ckpt_dir, w->sh_id, sijo_ck_prev2, "state"); remove(pth);
        sijo_ck_path(pth, sizeof pth, sijo_sho->ckpt_dir, w->sh_id, sijo_ck_prev2, "meta"); remove(pth);
    }
    /* a peer recovers from its newest complete checkpoint, at worst the previous one */
    if (sijo_ck_prev >= 0) sijo_rl_prune(sijo_ck_prev_bno);
    sijo_ck_prev2 = sijo_ck_prev;
    sijo_ck_prev = w->tick;
    sijo_ck_prev_bno = w->sh->bno;
}
/* --recover: the newest complete checkpoint of this shard */
static int sijo_ck_load(SijoWorld* w) {
    const char* dir = sijo_sho->ckpt_dir;
    int32_t best = -1;
    char pth[1024];
    /* checkpoints are named by tick: look for the newest one with a meta file */
    FILE* f = NULL;
    for (int32_t t = 0; t <= (1 << 30) / (sijo_sho->ckpt_every > 0 ? sijo_sho->ckpt_every : 1) && t < 100000000; t += sijo_sho->ckpt_every > 0 ? sijo_sho->ckpt_every : 1) {
        sijo_ck_path(pth, sizeof pth, dir, w->sh_id, t, "meta");
        f = fopen(pth, "rb");
        if (f) { fclose(f); best = t; }
        else if (best >= 0 && t > best + 4 * sijo_sho->ckpt_every) break;
        if (t > 50000000) break;
    }
    if (best < 0) return 0;
    for (int32_t t = best; t >= 0; t -= sijo_sho->ckpt_every > 0 ? sijo_sho->ckpt_every : 1) {
        char st[1024];
        sijo_ck_path(st, sizeof st, dir, w->sh_id, t, "state");
        sijo_ck_path(pth, sizeof pth, dir, w->sh_id, t, "meta");
        f = fopen(pth, "rb");
        if (!f) continue;
        fclose(f);
        int id = w->sh_id, n = w->sh->n;
        if (!sijo_load_state_v(w, st, 0)) continue;
        sijo_sh_init(w, id, n);
        if (!sijo_ck_read_meta(w, pth)) continue;
        sijo_ck_prev = t;
        sijo_ck_prev_bno = w->sh->bno;
        fprintf(stderr, "sijo shard %d: recovering from the checkpoint of tick %d (barrier %llu)\n", id, (int)t, (unsigned long long)w->sh->bno);
        return 1;
    }
    return 0;
}

/* ---------------- the coordinator (shard 0): the usual server / loopback process, sharded ---------------- */
static int32_t sijo_sh_last_tick = -1;
static void sijo_sh_step_net(SijoWorld* S) {
    if (sijo_sh_last_tick >= 0 && S->tick >= sijo_sh_last_tick) S->sh->stop = 1;
    sijo_sh_tick_all(S->tick);
    sijo_ck_maybe(S);
}
static void sijo_sh_finish_net(SijoWorld* S) {
    S->sh->stop = 1;
    sijo_sh_barrier(SIJO_SHK_GATHER);
    sijo_rl_linger();
}
static void sijo_sh_stats_net(SijoWorld* S) {
    SijoShard* s = S->sh;
    SijoShStat t;
    memset(&t, 0, sizeof t);
    sijo_sh_stat_add(&t, s);
    t.bytes += s->g_bytes; t.msgs += s->g_msgs; t.handoffs += s->g_handoffs; t.cmds += s->g_cmds; t.spawns += s->g_spawns; t.syncs += s->g_syncs; t.leaves += s->g_leaves;
    sijo_sh_stat_print(&t, s->n, S->tick - sijo_sh_ticks, "(processes)");
    uint64_t rs = 0, fo = 0;
    for (int j = 0; j < s->n; j++) { rs += sijo_rl.peer[j].resends; fo += sijo_rl.peer[j].frags_out; }
    printf("shard channel (coordinator): %llu fragments sent, %llu re-sent\n", (unsigned long long)fo, (unsigned long long)rs);
    if (sijo_sho->timing) {
        double k = 1e3 / (double)(S->tick - sijo_sh_ticks > 0 ? S->tick - sijo_sh_ticks : 1);
        printf("shard 0 time per tick: systems %.3f ms, build %.3f, waiting %.3f, install %.3f, apply %.3f\n", s->t_sys * k, s->t_build * k, s->t_wait * k, s->t_install * k, s->t_apply * k);
    }
    sijo_print_sim("shard links", &sijo_rl.sock, sijo_sho);
}
static SijoWorld* sijo_sh_coord_world;
static void sijo_sh_start_net(SijoWorld* S) {
    sijo_sh_init(S, 0, sijo_sho->shard_of);
    sijo_sh_coord_world = S;
    sijo_sh_ticks = S->tick;
    if (sijo_sho->ticks > 0) sijo_sh_last_tick = S->tick + sijo_sho->ticks;
    sijo_rl_handshake();
    sijo_sh_initial(S);
    static SijoWorld* one[1];
    one[0] = S;
    sijo_shd.ws = one; sijo_shd.nw = 1; sijo_shd.xfer = sijo_sh_xfer_net;
    sijo_step = sijo_sh_step_net;
    sijo_finish_hook = sijo_sh_finish_net;
    sijo_stats_hook = sijo_sh_stats_net;
}

/* ---------------- shards 1..N-1: the world and the barriers, nothing else ---------------- */
static int sijo_shard_worker_main(SijoOpts* o) {
    static SijoWorld W;
    signal(SIGINT, sijo_on_signal);
    signal(SIGTERM, sijo_on_signal);
    sijo_world_init(&W, 0, o->seed, o->shard_of);
    W.quiet = o->quiet;
    sijo_sh_init(&W, o->shard_id, o->shard_of);
    int recovering = 0;
    if (o->recover) {
        if (!o->ckpt_dir || !sijo_ck_load(&W)) { fprintf(stderr, "sijo shard %d: --recover needs a checkpoint in --checkpoint-dir\n", o->shard_id); return 2; }
        recovering = 1;
        sijo_rl.resume_bno = W.sh->bno;
    } else if (o->load_path) {
        if (!sijo_load_state_v(&W, o->load_path, 0)) { fprintf(stderr, "sijo shard %d: cannot load %s\n", o->shard_id, o->load_path); return 2; }
        sijo_sh_init(&W, o->shard_id, o->shard_of);
    } else {
        sijo_game_start(&W);
    }
    int32_t start = W.tick;
    sijo_rl_handshake();
    if (!recovering) sijo_sh_initial(&W);
    else sijo_sh_fit(&W);
    sijo_reset_arenas(&W);
    static SijoWorld* one[1];
    one[0] = &W;
    sijo_shd.ws = one; sijo_shd.nw = 1; sijo_shd.xfer = sijo_sh_xfer_net;
    sijo_sh_ticks = start;
    double t0 = sijo_now();
    int32_t t = start;
    /* the run ends with the coordinator's (stop flag in its barrier messages) or after --ticks */
    for (;;) {
        if (W.sh->peer_stop) break;
        t++;
        sijo_sh_tick_all(t);
        sijo_ck_maybe(&W);
        sijo_reset_arenas(&W);
        if (o->die_at > 0 && t == o->die_at && !o->recover) {
            fprintf(stderr, "sijo shard %d: dying at tick %d (--die-at-tick)\n", o->shard_id, t);
            fflush(stderr);
            _exit(77);
        }
    }
    sijo_sh_barrier(SIJO_SHK_GATHER);
    double t1 = sijo_now();
    sijo_rl_linger();
    if (o->summary) {
        SijoShard* s = W.sh;
        printf("== sijo shard %d summary ==\n", s->id);
        printf("ticks: %d (%s), %lld barriers, owned at the end: %d entities with a position\n", (int)(t - start), recovering ? "after recovering from a checkpoint" : "from the start", (long long)s->barriers, (int)sijo_sh_owned_spatial(&W));
        printf("sent: %lld bytes in %lld messages, %lld handoffs, %lld component copies, %lld commands, %lld spawns\n", (long long)s->sent_bytes, (long long)s->sent_msgs, (long long)s->handoffs, (long long)s->syncs, (long long)s->xcmds, (long long)s->xspawns);
        uint64_t rs = 0, fo = 0;
        for (int j = 0; j < s->n; j++) { rs += sijo_rl.peer[j].resends; fo += sijo_rl.peer[j].frags_out; }
        printf("channel: %llu fragments sent, %llu re-sent; %llu messages replayed to recovering shards\n", (unsigned long long)fo, (unsigned long long)rs, (unsigned long long)sijo_rl.replays);
        sijo_print_sim("shard links", &sijo_rl.sock, o);
        if (o->timing) {
            double k = 1e3 / (double)(t > start ? t - start : 1);
            printf("time: %.3f ms total, %.2f us/tick\n", (t1 - t0) * 1e3, (t1 - t0) * 1e6 / (t > start ? t - start : 1));
            printf("shard %d time per tick: systems %.3f ms, build %.3f, waiting %.3f, install %.3f, apply %.3f\n", s->id, s->t_sys * k, s->t_build * k, s->t_wait * k, s->t_install * k, s->t_apply * k);
        }
    }
    return 0;
}

/* returns an exit code (a shard process ran), or -1: continue in the usual mode with the hooks set */
static int sijo_shard_setup(SijoOpts* opts) {
    static SijoOpts kept; /* the hooks run after sijo_main parsed the options */
    kept = *opts;
    SijoOpts* o = &kept;
    sijo_sho = o;
    if (o->profile_path || o->live_dir) { fprintf(stderr, "sharding does not combine with --profile or live coding\n"); return 2; }
    if (o->shard_of <= 1) {
        sijo_start_hook = sijo_sh_start_inproc;
        return -1;
    }
    if (o->shard_of > SIJO_SH_MAXN) { fprintf(stderr, "--shard-of: at most %d shards\n", SIJO_SH_MAXN); return 2; }
    if (o->shard_id < 0 || o->shard_id >= o->shard_of) { fprintf(stderr, "--shard-id must be 0..%d\n", o->shard_of - 1); return 2; }
    if (o->shards > 1) { fprintf(stderr, "--shards (worlds in one process) and --shard-of (processes) do not combine\n"); return 2; }
    if (o->recover && o->shard_id == 0) { fprintf(stderr, "--recover: the coordinator (shard 0) cannot recover yet; restart the run\n"); return 2; }
#if !defined(_WIN32) && defined(_OPENMP)
    /* shard processes share the machine's cores: OpenMP's default busy wait after each parallel
       loop would take them from the other shards (10x slower barriers); unless the user chose a
       policy, start again with the passive one (it is read when the program starts) */
    if (!getenv("OMP_WAIT_POLICY") && sijo_argv) {
        setenv("OMP_WAIT_POLICY", "passive", 1);
        fflush(stdout);
        execvp(sijo_argv[0], sijo_argv);
    }
#endif
    if (!sijo_rl_open(o)) return 2;
    if (o->shard_id > 0) {
        if (o->mode != 0) { fprintf(stderr, "--server/--connect belong to the coordinator (shard 0)\n"); return 2; }
        return sijo_shard_worker_main(o);
    }
    if (o->mode == 2) { fprintf(stderr, "the coordinator is a server: --connect does not combine with --shard-of\n"); return 2; }
    sijo_start_hook = sijo_sh_start_net;
    return -1;
}
