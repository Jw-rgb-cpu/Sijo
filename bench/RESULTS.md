# Sijo benchmark results

**Claim under test:** *"You write simple OOP code, and the compiler produces code close to
hand-written data-oriented C, and clearly faster than the idiomatic object-oriented C++
approach (Unreal-Actor / classic GameObject style), plus automatic multi-core scaling."*

Three workloads × the implementations below (Sijo, hand-written data-oriented C, idiomatic OOP
C++; OOP C# when a .NET 8 SDK is installed), same algorithm and float operation order everywhere,
**bit-identical results verified on every run**. Reproducible with `bench/run.sh` (see the end).

## Summary

The numbers come from the run recorded in the generated tables below (sijoc 0.1.0). It ran on a
shared cloud VM (1-minute load average 0.8–1.4 on 4 cores), so **the ratios are the meaningful numbers**.

**Sijo ÷ hand-written C, 1 thread** (lower is better; < 1 = Sijo faster)

| workload | `O2` (sijoc's flags) | `O3-native` |
|---|---|---|
| boids 4k | **0.20×** (automatic grid) | 0.21× |
| particles 100k | **0.76×** | 1.20× |
| particles 1M | **0.74×** | 1.03× |
| shooter 500 ships | **0.83×** | 0.83× |
| shooter 2000 ships | **0.77×** | 0.78× |

**`O2`, steady-state time per tick**

| workload | Sijo 1T | hand C 1T | OOP C++ 1T | Sijo 4T (scaling) | hand C 4T |
|---|---|---|---|---|---|
| boids 4k | **3.92 ms** (grid) | 19.7 ms | 20.8 ms | **1.24 ms** (3.2×) | 5.45 ms |
| particles 100k | **188 µs** | 248 µs | 325 µs | **56 µs** (3.4×) | 69 µs |
| particles 1M | **1.91 ms** | 2.58 ms | 3.75 ms | **565 µs** (3.4×) | 709 µs |
| shooter 500 ships | **324 µs** | 393 µs | 487 µs | **131 µs** (2.5×) | 284 µs |
| shooter 2000 ships | **3.70 ms** | 4.79 ms | 5.77 ms | **1.11 ms** (3.3×) | 3.79 ms |

**The claim, checked**

| part of the claim | boids | particles | shooter |
|---|---|---|---|
| close to hand-written DOD C (1 thread) | **yes** — and 5× faster with the grid the compiler proved and inserted (a hand-written grid would win too) | **yes** — 0.76× / 0.74× at `O2`; 1.0–1.2× at `O3-native`, where the hand-written loop uses branch-free selects | **yes** — 0.77–0.83× |
| clearly faster than OOP C++ (1 thread) | **yes** (5.3× with the grid) | **yes** — 1.7× / 2.0× | **yes** — 1.5× / 1.6× |
| automatic multi-core scaling (1 → 4 threads) | 3.2× | 3.4× / 3.4× | 2.5× / 3.3× |
| Sijo on 4 threads vs single-threaded OOP C++ | 17× | 5.8× / 6.6× | 3.7× / 5.2× |

**What is still open**: peak memory at particles 1M is 45 MB vs 25 MB for the hand-written C
(slot table, entity handle column and per-row bookkeeping). At `O3-native` both particle loops are
vectorized with 32-byte AVX2 vectors (gcc: `loop vectorized using 32 byte vectors`; the generated
one after a runtime alias check, as the generated C carries no `restrict`), and the hand-written
loop is 1.0–1.2× faster. On 4 threads Sijo scales 2.1–3.4×, least at boids 1k, where a tick is only
363 µs of work; the hand-written C's parallel loops scale 3.5–3.6× in the same run.

## Compiler intelligence (task groups, PGO, e-graph)

Measured on the same kind of 4-vCPU VM while it was otherwise idle (it is noisy: the numbers are
the minimum of 5–7 alternating runs). Every pair below printed the same output and world checksum.

| what | workload | without | with |
|---|---|---|---|
| task groups (systems of a stage side by side) | four independent systems of 200 entities each, ~0.55 us per entity (each below the 256-row parallel threshold, so without task groups they run one after another) | 451 us/tick (4 threads) | **128 us/tick** (3.5x) |
| PGO: per-system parallel threshold from the profile | same program built with `--profile-use` (each system parallel from 16 rows; the group runs when the profiled costs say it is cheaper) | 451 us/tick | **133 us/tick** |
| PGO: hot query-loop fields in their own component | shooter, 2000 ships (`Ship { Pos, Hp }` -> `{ Pos }`, `{ Hp }`: the O(N^2) scans stream 8 B per ship instead of 12 B) | 4584 us (1T) / 2162 us (4T) | **4298 us / 1922 us** (-6% / -11%) |
| same | shooter, 500 ships (everything L1/L2-resident) | 335 us / 180 us | 360 us / 174 us (within noise) |

The e-graph (docs/07 §7.3) changes no measured hot loop here: the benchmark loops are float code,
which it leaves to the IEEE-exact identities. It removes bounds checks, saturations and decided
branches in int code (see `compiler/tests/programs/ranges.sijo` and `sijoc explain`).

How to profile: run the built program with `--profile FILE` (one thread; writes per-system costs and
the cost of a parallel region on that machine), then `sijoc build --profile-use FILE`.

---

## Shard processes (distributed simulation)

`bench/shards.py`: bench/boids with the world scaled to keep the density of 1000 boids in 200×200
(so every boid has the same number of neighbours at every size), on the 4-vCPU VM over localhost
UDP. One process with 1 and 4 OpenMP threads against 2 and 4 shard processes (`--shard-of N`,
each with 4/N threads, `OMP_WAIT_POLICY=passive`). Time per tick as in the rest of this file
((T(300 ticks) − T(1 tick)) / 299, minimum of 3 rounds, measured by the coordinator); every run
prints the same `bench` lines and world checksum as the one-process run (checked by the script).
Exchange = bytes and messages of all shards' barrier messages per tick (payload of the shard
channel, before UDP/IP headers).

Two variants of the same workload:
- **hash**: bench/boids as is. Its `Game.Update` reads every boid's `Pos`, `Vel` and `Neighbours`
  (the hash every 100 ticks), so it runs on the coordinator and every shard sends the coordinator
  every boid that changed, every tick (`explain`: "-> the coordinator").
- **halo**: `Game.Update` prints only the count. The shards exchange only boids within
  12 (the proven radius) + 3 (hysteresis) of a slab boundary.

| workload | boids (world) | configuration | ms per tick | speed-up vs 1 thread | shard exchange per tick (all shards) | messages per tick |
|---|---|---|---|---|---|---|
| hash | 16000 (800x800) | 1 process, 1 thread | 5.92 | 1.00x | - | - |
| hash | 16000 (800x800) | 1 process, 4 threads | 2.53 | 2.34x | - | - |
| hash | 16000 (800x800) | 2 shard processes x 1 thread | 4.75 | 1.25x | 163.2 KB | 4.0 |
| hash | 16000 (800x800) | 2 shard processes x 2 threads | 3.85 | 1.54x | 163.2 KB | 4.0 |
| hash | 16000 (800x800) | 4 shard processes x 1 thread | 2.80 | 2.11x | 266.7 KB | 24.0 |
| hash | 32000 (1131x1131) | 1 process, 1 thread | 12.72 | 1.00x | - | - |
| hash | 32000 (1131x1131) | 1 process, 4 threads | 5.23 | 2.43x | - | - |
| hash | 32000 (1131x1131) | 2 shard processes x 1 thread | 9.65 | 1.32x | 324.4 KB | 4.0 |
| hash | 32000 (1131x1131) | 2 shard processes x 2 threads | 7.40 | 1.72x | 324.4 KB | 4.0 |
| hash | 32000 (1131x1131) | 4 shard processes x 1 thread | 6.08 | 2.09x | 517.8 KB | 24.0 |
| halo | 16000 (800x800) | 1 process, 1 thread | 6.08 | 1.00x | - | - |
| halo | 16000 (800x800) | 1 process, 4 threads | 2.63 | 2.31x | - | - |
| halo | 16000 (800x800) | 2 shard processes x 1 thread | 4.12 | 1.47x | 10.5 KB | 2.0 |
| halo | 16000 (800x800) | 2 shard processes x 2 threads | 3.20 | 1.90x | 10.5 KB | 2.0 |
| halo | 16000 (800x800) | 4 shard processes x 1 thread | 2.33 | 2.61x | 35.4 KB | 12.0 |
| halo | 32000 (1131x1131) | 1 process, 1 thread | 12.79 | 1.00x | - | - |
| halo | 32000 (1131x1131) | 1 process, 4 threads | 5.24 | 2.44x | - | - |
| halo | 32000 (1131x1131) | 2 shard processes x 1 thread | 8.48 | 1.51x | 17.7 KB | 2.0 |
| halo | 32000 (1131x1131) | 2 shard processes x 2 threads | 5.93 | 2.16x | 17.7 KB | 2.0 |
| halo | 32000 (1131x1131) | 4 shard processes x 1 thread | 4.53 | 2.82x | 52.6 KB | 12.0 |

- The halo variant: 4 shard processes are faster than one process with 4 threads on the same
  cores (32000 boids: 4.53 ms against 5.24 ms, 2.8× one thread). The exchange is 18–53 KB a tick.
- The hash variant moves almost the whole world to the coordinator every tick (≈20 bytes per
  boid): 4 shards still give 2.1× over one thread, a little less than 4 threads in one process (2.3–2.4×).
  A program whose global system reads every entity pays this; one that only replicates to
  clients pays it for the replicated fields.
- Where the time goes (16000 boids, 4 shard processes, `--timing`, per tick and shard): systems
  1.8–1.9 ms (a quarter of 6.1 ms, plus the owned-row check over all rows); building the messages
  (stage-start images, compare, pack) 0.22–0.23 ms for halo, 0.42–0.48 ms for hash; waiting at the
  barriers 0.11–0.18 ms for halo, 0.31–0.45 ms for hash; installing what arrived 0.01 ms, on the
  coordinator 0.15 ms for hash; applying commands 0.02–0.04 ms.
- Barriers: one per stage (1 per tick for halo, 2 for hash), so 2–4 messages per tick on 2 shards and 12–24 on 4. The
  barrier wait polls without sleeping while fragments arrive; with one VM wake-up per fragment,
  boids 4000 on 2 processes would spend 0.92 ms of every tick waiting instead of 0.11 ms.

## What is measured

### Workloads (same algorithm, same float operation order everywhere)

| workload | what it stresses | sizes | source |
|---|---|---|---|
| **boids** | O(N²) neighbour scan with Jacobi semantics (neighbours are read as of the start of the tick — the compiler double-buffers `Pos`/`Vel`); compute-bound, branchy inner loop, data fits in L1/L2 | 1k, 2k, 4k boids | `boids/boids.sijo` = `examples/boids.sijo` minus the `client` method `Render`, with integer-LCG initial state |
| **particles** | self-only update (gravity, integrate, floor bounce, lifetime, in-place respawn from a per-particle LCG); memory layout, vectorization, per-entity overhead, parallel speed-up | 100k, 1M particles | `particles/particles.sijo` |
| **shooter** | `examples/arena.sijo` scaled up: nearest-enemy targeting (O(N²)), bullets spawned every 0.6 s, bullets hit every ship in range (deferred commutative `Hp -= Damage`, `Kills += 1`), ships destroyed at 0 hp and replaced → constant spawn/destroy churn of both archetypes and cross-entity writes | 500 ships (arena 256), 2000 ships (arena 512) | `shooter/shooter.sijo` |

The shooter's rules were made independent of iteration order (nearest-enemy ties broken
by `Id`; a bullet damages *all* ships in range), so implementations that store entities in
different orders (Sijo's swap-remove tables, `std::vector` erase-remove, a fixed ship pool)
must still produce identical results — and do.

### Implementations

| | what it is | threads |
|---|---|---|
| **Sijo** | the `.sijo` file compiled by `sijoc build` (unmodified generated C, compiled with the flags below) | OpenMP, automatic (`OMP_NUM_THREADS` = 1, 2, 4) |
| **hand C (DOD)** | what a performance engineer would write: one array per scalar field (pure SoA), `restrict`, hoisted constants; boids: ping-pong buffers (no copy); particles: branch-free selects + `omp parallel for simd`; shooter: fixed ship pool with generation counters, swap-remove bullet arrays, integer atomics for cross-entity writes | OpenMP on the entity systems (boids, particles, shooter bullets); the shooter ship system stays serial because it spawns, as a straightforward OpenMP port would (Sijo runs it in parallel, see the shooter analysis) |
| **OOP C++** | classic GameObject / Unreal-Actor style (`common/oop.hpp`): `class Ship : public Actor` with `virtual void Tick(float)`, every actor a separate `std::unique_ptr` allocation owned by `std::vector`, tick groups, deferred Spawn/Destroy with BeginPlay/EndPlay, typed registries (`std::vector<Ship*>`), Unreal-style weak pointers (index + serial) between actors | single-threaded (as gameplay code is) |
| **OOP C++ aged heap** | the same binary with `--scatter`: identical iteration order, but the initial objects are allocated in a shuffled order, so consecutive actors are far apart in memory — closer to a long-running game than a fresh heap | single-threaded |
| **OOP C#** | the same design in C# (.NET 8, `List<T>`, classes, Unity-like), default runtime settings (tiered JIT + dynamic PGO, workstation GC); measured when a .NET 8 SDK is found (not in the current tables) | single-threaded |

**How Jacobi semantics are emulated in the OOP versions** (Sijo: an entity reads other
entities as they were at the start of the system, sees its own writes immediately, and
writes to other entities are applied at the end of the system):
* Actors whose fields are read by *other actors of the same tick group* override
  `PreTick()` and copy those fields into `Prev…` members (`Boid`: `PrevPos`, `PrevVel`;
  `Ship`: `PrevPos`). The world calls `PreTick()` on all of them before ticking the group,
  and other actors read only the `Prev…` members. That is one extra virtual call and copy
  per actor per tick — the price of determinism in OOP.
* Cross-entity writes (`ship->Hp -= Damage`, `owner->Kills += 1`) are done immediately;
  that is equivalent to Sijo's deferred commutative adds because nothing in the bullet
  tick group reads `Hp` or `Kills`.
* Spawn/Destroy are deferred to the end of the tick group (the framework does this for
  every actor, like engines do), which is exactly Sijo's structural-change rule.

### Methodology

* **Same compiler, same flags for everything**: the generated Sijo C, the hand-written C
  and the OOP C++ are compiled by the harness with one flag set per configuration:
  * `O2` = exactly sijoc's own driver flags (`-std=gnu11 -O2 -fwrapv`,
    `-fopenmp`) plus `-ffp-contract=off` (a no-op on plain x86-64; it only matters with
    FMA). This is what `sijoc build` produces.
  * `O3-native` = `-O3 -march=native -fno-trapping-math` (+ the same semantic flags): the
    best case for gcc. `-fno-trapping-math` and `-ffp-contract=off` change no float value
    (no `-ffast-math`); they let gcc if-convert and vectorize branchy float loops. All
    results stay bit-identical — the harness checks it.
* **Timing**: every program takes t0 before creating/spawning the world and t1 after the
  last tick (this is what the Sijo runtime's `--timing` does). Each case runs with
  `--ticks N` and `--ticks 1`; steady-state time per tick = (T(N) − T(1)) / (N − 1), so
  start-up (spawning, JIT, first-touch page faults) cancels out. Start-up is reported
  separately.
* **Noise control** (this is a shared 4-vCPU cloud VM; other sessions compile and test on
  it): cases run round-robin in 3 rounds (`--reps`); the wrapper `common/maxrss.c` reports each run's
  CPU time, and a run during which *other* processes used more than 0.3 cores is
  discarded and repeated once the machine is quiet; the reported number is the minimum
  over the rounds (interference only adds time). Medians are in `results/latest-full.json`.
  Remaining run-to-run spread is still 20–100 % on some cases (host-level noise invisible
  inside the VM), so treat differences below ~15 % as noise.
* **Peak RSS** from `wait4()` of the measured process (not of the Python harness).
* The Sijo runtime's per-tick work that is not simulation (replication snapshot,
  client frame, RPC pump) is *included* — it is part of what sijoc produces. None of the
  benchmark programs has `client` code, so nothing is actually replicated.

### Correctness

Every implementation prints, every 100 ticks, an order-independent integer hash of the
whole entity state (each float quantized with `(int)(v * 2^20…2^23)`, i.e. all mantissa bits
for |v| ≥ 1) and, for boids/particles, a byte-exact copy of the Sijo runtime's FNV world
checksum over every field of every entity; for the shooter, the entity counts and the
total numbers of spawns and destroys. **Every run of every implementation produced exactly the Sijo reference
values** — at every thread
count, in both configurations. Floating-point results are bit-identical because every
implementation performs the same IEEE operations in the same order (no `-ffast-math`, no
FMA contraction, no reassociation; the hand-written C keeps the boids accumulation order
and only uses selects where they cannot change a value).

## Results

_The numbers quoted in the prose of this file come from the run recorded below. Re-running `bench/run.sh` rewrites only the tables between the GENERATED markers._

<!-- BEGIN GENERATED TABLES (bench/run.py) -->
_Generated by `bench/run.py` on 2026-11-06 06:14 — Intel(R) Xeon(R) Processor @ 2.10GHz, 4 cores; gcc (Ubuntu 13.3.0-6ubuntu2~24.04.1) 13.3.0; g++ (Ubuntu 13.3.0-6ubuntu2~24.04.1) 13.3.0; 3 round-robin rounds, per-tick = (min T(N) − min T(1)) / (N − 1); sijoc 0.1.0; suite took 362 s._

### Configuration `O2`

C: `-std=gnu11 -O2 -fwrapv -ffp-contract=off -fopenmp` · C++: `-std=gnu++17 -O2 -fwrapv -ffp-contract=off -fopenmp` (the generated Sijo C, the hand-written C and the OOP C++ all use these).

**Steady-state time per tick, 1 thread** (lower is better). *Sijo ÷ C* = Sijo time / hand-written C time; *OOP ÷ Sijo* = speedup of Sijo over the OOP C++ version.

| workload | size | ticks | Sijo | hand C (DOD) | OOP C++ | OOP C++ aged heap | Sijo ÷ C | OOP ÷ Sijo |
|---|---|---|---|---|---|---|---|---|
| boids | 1k | 300 | 363 µs | 1.29 ms | 1.35 ms | 1.36 ms | 0.28× | 3.7× |
| boids | 2k | 200 | 1.15 ms | 5.01 ms | 5.26 ms | 5.27 ms | 0.23× | 4.6× |
| boids | 4k | 100 | 3.92 ms | 19.7 ms | 20.8 ms | 20.9 ms | 0.20× | 5.3× |
| particles | 100k | 300 | 188 µs | 248 µs | 325 µs | 672 µs | 0.76× | 1.73× |
| particles | 1M | 100 | 1.91 ms | 2.58 ms | 3.75 ms | 12.0 ms | 0.74× | 1.97× |
| shooter | 500 ships (arena 256) | 600 | 324 µs | 393 µs | 487 µs | — | 0.83× | 1.50× |
| shooter | 2000 ships (arena 512) | 200 | 3.70 ms | 4.79 ms | 5.77 ms | — | 0.77× | 1.56× |

**Thread scaling** (OMP_NUM_THREADS; time per tick, speedup vs. the same implementation on 1 thread in parentheses). *OOP ÷ Sijo* compares the single-threaded OOP C++ with Sijo on the most threads.

| workload | size | Sijo 1T | Sijo 2T | Sijo 4T | C 1T | C 2T | C 4T | Sijo ÷ C (4T) | OOP ÷ Sijo (4T) |
|---|---|---|---|---|---|---|---|---|---|
| boids | 1k | 363 µs | 246 µs (1.5×) | 169 µs (2.1×) | 1.29 ms | 660 µs (2.0×) | 362 µs (3.6×) | 0.47× | 8.0× |
| boids | 2k | 1.15 ms | 695 µs (1.7×) | 433 µs (2.7×) | 5.01 ms | 2.53 ms (2.0×) | 1.44 ms (3.5×) | 0.30× | 12× |
| boids | 4k | 3.92 ms | 2.15 ms (1.8×) | 1.24 ms (3.2×) | 19.7 ms | 9.91 ms (2.0×) | 5.45 ms (3.6×) | 0.23× | 17× |
| particles | 100k | 188 µs | 111 µs (1.7×) | 56 µs (3.4×) | 248 µs | 125 µs (2.0×) | 69 µs (3.6×) | 0.81× | 5.8× |
| particles | 1M | 1.91 ms | 1.02 ms (1.9×) | 565 µs (3.4×) | 2.58 ms | 1.36 ms (1.9×) | 709 µs (3.6×) | 0.80× | 6.6× |
| shooter | 500 ships (arena 256) | 324 µs | 202 µs (1.6×) | 131 µs (2.5×) | 393 µs | 322 µs (1.2×) | 284 µs (1.4×) | 0.46× | 3.7× |
| shooter | 2000 ships (arena 512) | 3.70 ms | 1.98 ms (1.9×) | 1.11 ms (3.3×) | 4.79 ms | 4.05 ms (1.2×) | 3.79 ms (1.3×) | 0.29× | 5.2× |

**Peak RSS and start-up** (start-up = spawning the initial entities, plus JIT for C#).

| workload | size | Sijo RSS | hand C (DOD) RSS | OOP C++ RSS | Sijo start-up | hand C (DOD) start-up | OOP C++ start-up |
|---|---|---|---|---|---|---|---|
| boids | 1k | 2.6 MB | 2.2 MB | 3.8 MB | 0.0 ms | 0.0 ms | 0.0 ms |
| boids | 2k | 2.9 MB | 2.2 MB | 4.1 MB | 0.0 ms | 0.0 ms | 0.0 ms |
| boids | 4k | 3.1 MB | 2.3 MB | 4.2 MB | 0.0 ms | 0.0 ms | 0.4 ms |
| particles | 100k | 7.1 MB | 4.3 MB | 15.7 MB | 3.2 ms | 1.1 ms | 10.7 ms |
| particles | 1M | 45.1 MB | 24.9 MB | 118.9 MB | 33.0 ms | 11.5 ms | 102.8 ms |
| shooter | 500 ships (arena 256) | 2.7 MB | 2.0 MB | 3.8 MB | 0.0 ms | 0.0 ms | 0.0 ms |
| shooter | 2000 ships (arena 512) | 3.0 MB | 2.3 MB | 4.2 MB | 0.1 ms | 0.0 ms | 0.0 ms |

**Correctness**: every run above printed the same per-100-tick state hash as the Sijo reference, and the same Sijo-compatible world checksum (boids, particles) / entity and spawn/destroy counts (shooter).

| workload | size | runs checked | all identical | reference check value |
|---|---|---|---|---|
| boids | 1k | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `fb7d4addd24b469a` |
| boids | 2k | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `63ba30d5a4761e9e` |
| boids | 4k | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `e2e9c4715dbd0b3d` |
| particles | 100k | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `6a8a20f483891f1e` |
| particles | 1M | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `5fadf98a0cff28ae` |
| shooter | 500 ships (arena 256) | 21 (dod@1, dod@2, dod@4, oop@1, sijo@1, sijo@2, sijo@4) | yes | `Ship=500 Bullet=399 (spawned 9340, destroyed 8441)` |
| shooter | 2000 ships (arena 512) | 21 (dod@1, dod@2, dod@4, oop@1, sijo@1, sijo@2, sijo@4) | yes | `Ship=2000 Bullet=1123 (spawned 13288, destroyed 10165)` |

_Run-to-run spread (max−min of the 3 long runs ÷ min): worst case 80% (boids 1000 dod@4); 1-min load average at the start of the rounds: 0.82–1.27 on 4 cores. This is a shared cloud VM (other sessions may compile on it); treat differences below ~10–15% as noise._

### Configuration `O3-native`

C: `-std=gnu11 -O3 -march=native -fno-trapping-math -fwrapv -ffp-contract=off -fopenmp` · C++: `-std=gnu++17 -O3 -march=native -fno-trapping-math -fwrapv -ffp-contract=off -fopenmp` (the generated Sijo C, the hand-written C and the OOP C++ all use these).

**Steady-state time per tick, 1 thread** (lower is better). *Sijo ÷ C* = Sijo time / hand-written C time; *OOP ÷ Sijo* = speedup of Sijo over the OOP C++ version.

| workload | size | ticks | Sijo | hand C (DOD) | OOP C++ | OOP C++ aged heap | Sijo ÷ C | OOP ÷ Sijo |
|---|---|---|---|---|---|---|---|---|
| boids | 1k | 300 | 341 µs | 1.16 ms | 1.32 ms | 1.32 ms | 0.29× | 3.9× |
| boids | 2k | 200 | 1.10 ms | 4.61 ms | 5.16 ms | 5.19 ms | 0.24× | 4.7× |
| boids | 4k | 100 | 3.73 ms | 17.8 ms | 20.9 ms | 20.0 ms | 0.21× | 5.6× |
| particles | 100k | 300 | 109 µs | 91 µs | 309 µs | 572 µs | 1.20× | 2.84× |
| particles | 1M | 100 | 1.16 ms | 1.12 ms | 3.98 ms | 12.8 ms | 1.03× | 3.4× |
| shooter | 500 ships (arena 256) | 600 | 330 µs | 397 µs | 464 µs | — | 0.83× | 1.40× |
| shooter | 2000 ships (arena 512) | 200 | 3.80 ms | 4.88 ms | 5.78 ms | — | 0.78× | 1.52× |

**Thread scaling** (OMP_NUM_THREADS; time per tick, speedup vs. the same implementation on 1 thread in parentheses). *OOP ÷ Sijo* compares the single-threaded OOP C++ with Sijo on the most threads.

| workload | size | Sijo 1T | Sijo 2T | Sijo 4T | C 1T | C 2T | C 4T | Sijo ÷ C (4T) | OOP ÷ Sijo (4T) |
|---|---|---|---|---|---|---|---|---|---|
| boids | 1k | 341 µs | 259 µs (1.3×) | 193 µs (1.8×) | 1.16 ms | 623 µs (1.9×) | 330 µs (3.5×) | 0.58× | 6.8× |
| boids | 2k | 1.10 ms | 655 µs (1.7×) | 418 µs (2.6×) | 4.61 ms | 2.36 ms (2.0×) | 1.23 ms (3.7×) | 0.34× | 12× |
| boids | 4k | 3.73 ms | 2.06 ms (1.8×) | 1.19 ms (3.1×) | 17.8 ms | 9.19 ms (1.9×) | 4.85 ms (3.7×) | 0.25× | 18× |
| particles | 100k | 109 µs | 52 µs (2.1×) | 31 µs (3.5×) | 91 µs | 45 µs (2.0×) | 27 µs (3.4×) | 1.18× | 9.8× |
| particles | 1M | 1.16 ms | 592 µs (2.0×) | 304 µs (3.8×) | 1.12 ms | 581 µs (1.9×) | 291 µs (3.9×) | 1.05× | 13× |
| shooter | 500 ships (arena 256) | 330 µs | 205 µs (1.6×) | 134 µs (2.5×) | 397 µs | 345 µs (1.2×) | 294 µs (1.3×) | 0.45× | 3.5× |
| shooter | 2000 ships (arena 512) | 3.80 ms | 2.09 ms (1.8×) | 1.16 ms (3.3×) | 4.88 ms | 4.21 ms (1.2×) | 3.92 ms (1.2×) | 0.30× | 5.0× |

**Peak RSS and start-up** (start-up = spawning the initial entities, plus JIT for C#).

| workload | size | Sijo RSS | hand C (DOD) RSS | OOP C++ RSS | Sijo start-up | hand C (DOD) start-up | OOP C++ start-up |
|---|---|---|---|---|---|---|---|
| boids | 1k | 2.7 MB | 2.2 MB | 3.8 MB | 0.0 ms | 0.0 ms | 0.0 ms |
| boids | 2k | 2.8 MB | 2.0 MB | 4.1 MB | 0.0 ms | 0.0 ms | 0.0 ms |
| boids | 4k | 3.1 MB | 2.2 MB | 4.4 MB | 0.0 ms | 0.5 ms | 0.0 ms |
| particles | 100k | 7.0 MB | 4.3 MB | 15.6 MB | 3.1 ms | 1.1 ms | 8.7 ms |
| particles | 1M | 45.0 MB | 25.0 MB | 118.9 MB | 33.1 ms | 12.4 ms | 109.7 ms |
| shooter | 500 ships (arena 256) | 2.7 MB | 2.1 MB | 3.9 MB | 0.0 ms | 0.0 ms | 0.0 ms |
| shooter | 2000 ships (arena 512) | 3.0 MB | 2.1 MB | 4.2 MB | 0.3 ms | 0.0 ms | 0.0 ms |

**Correctness**: every run above printed the same per-100-tick state hash as the Sijo reference, and the same Sijo-compatible world checksum (boids, particles) / entity and spawn/destroy counts (shooter).

| workload | size | runs checked | all identical | reference check value |
|---|---|---|---|---|
| boids | 1k | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `fb7d4addd24b469a` |
| boids | 2k | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `63ba30d5a4761e9e` |
| boids | 4k | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `e2e9c4715dbd0b3d` |
| particles | 100k | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `6a8a20f483891f1e` |
| particles | 1M | 24 (dod@1, dod@2, dod@4, oop-scatter@1, oop@1, sijo@1, sijo@2, sijo@4) | yes | `5fadf98a0cff28ae` |
| shooter | 500 ships (arena 256) | 21 (dod@1, dod@2, dod@4, oop@1, sijo@1, sijo@2, sijo@4) | yes | `Ship=500 Bullet=399 (spawned 9340, destroyed 8441)` |
| shooter | 2000 ships (arena 512) | 21 (dod@1, dod@2, dod@4, oop@1, sijo@1, sijo@2, sijo@4) | yes | `Ship=2000 Bullet=1123 (spawned 13288, destroyed 10165)` |

_Run-to-run spread (max−min of the 3 long runs ÷ min): worst case 176% (particles 100000 dod@4); 1-min load average at the start of the rounds: 0.87–1.40 on 4 cores. This is a shared cloud VM (other sessions may compile on it); treat differences below ~10–15% as noise._

<!-- END GENERATED TABLES -->

## Why — analysis per workload

All numbers below are configuration `O2` (sijoc's own flags) unless marked `O3-native`.

### boids: the compiler replaces the O(N²) scan with a grid

4k boids, 1 thread: Sijo 3.92 ms, hand C 19.7 ms, OOP C++ 20.8 ms (aged heap 20.9 ms).
4 threads: Sijo 1.24 ms, hand C 5.45 ms.

* **Sijo does less work.** `Boid.Update` compares squared distances with a constant radius; the
  compiler proves that symbolically and turns the neighbour loop into a hash-grid query
  (docs/04 §4.12). The result is bit-identical to the full scan (the harness checks it against the
  hand C and OOP versions, which scan all pairs). A hand-written grid would be as fast; the point
  is that nobody had to write it.
* **The full-scan inner loop is the same in the C and C++ versions.** Per pair: two loads, two
  subtractions, a multiply-add, a compare and a rarely taken branch. Nothing is vectorized: the
  accumulation order must be kept for bit-exact results, and a branch-free version is slower (six
  dependent adds per pair cost more than a well-predicted branch).
* **OOP overheads are O(N), the work is O(N²):** the virtual `Tick`, the `PreTick` snapshot pass
  that emulates Jacobi semantics and the registry walk are small next to the scan; at 4k boids all
  objects stay L2-resident even on an aged heap.
* **Threads:** the system rule makes `Boid.Update` data-parallel with no source changes; Sijo scales
  3.2× on 4 threads at 4k (hand C 3.6×), and the results stay bit-identical.

### particles: per-entity overhead and memory layout

100k / 1M particles, 1 thread: Sijo 188 µs / 1.91 ms, hand C 248 µs / 2.58 ms, OOP C++ 325 µs /
3.75 ms (aged heap 672 µs / 12.0 ms). 4 threads: Sijo 56 µs / 565 µs, hand C 69 µs / 709 µs.

The per-particle work is ~20 flops, so per-entity overhead shows. The generated row loop:
* reads the thread's context once per thread, and only in systems that need it (`Random`,
  deferred commands); per-thread state is padded to cache lines, so threads share no lines;
* is plain strict-aliasing C (type punning only through `memcpy`/unions), so gcc keeps fields in
  registers; the row's fields are read into locals and written back once, and the row loop sits
  outside the `setjmp` region, so the self-only system vectorizes;
* has the entity's method inlined: the row function also holds the loop a shard runs over the rows
  it owns, so the method has two call sites and is marked `always_inline` (out of line, the call per
  particle costs ~1.5× at 1M);
* runs no replication pass, because the program replicates nothing.

**OOP C++** pays for (a) size: each particle is a separate 56-byte object (vtable pointer, actor
bookkeeping, 24 bytes of payload) in a 64-byte malloc chunk, plus pointers and object-table entries
in the world's lists — ~4× the bytes of the SoA arrays (RSS 119 MB vs 25 MB); (b) a virtual call
per particle, which blocks inlining and vectorization; (c) on an aged heap a cache miss per
particle: 12.0 ms vs 3.75 ms at 1M.

### shooter: O(N²) targeting, spawn/destroy churn, cross-entity writes

500 ships, 1 thread: Sijo 324 µs, hand C 393 µs, OOP C++ 487 µs; 2000 ships: Sijo 3.70 ms, hand C
4.79 ms, OOP C++ 5.77 ms. 4 threads at 2000 ships: Sijo 1.11 ms, hand C 3.79 ms.

* **The dominant cost is nearest-enemy targeting in `Ship.Update` (O(N²)).** That system spawns
  bullets, and Sijo still runs it in parallel: per-thread spawn tables are merged in row order, so
  entity ids and results are the same as on one thread (docs/04 §4.8). The hand-written C keeps the
  ship system serial, as a straightforward OpenMP port would (`shooter_dod --par-ships` is the
  variant with a separate parallel targeting pass).
* **Hazard and handles:** `Ship.Pos` is written on self and read on other ships, so it is
  double-buffered (`sijoc explain`: `double-buffered [Ship_C0]`); the copy covers only the fields
  other entities read and is shared by the threads. Loop-invariant handle checks are hoisted out
  of the inner query loops.
* **OOP is competitive at this size:** a few thousand ships are a few hundred KB of objects,
  allocated mostly contiguously; the churn (`new`/`delete`, erase-remove and registry erase in OOP
  vs pending tables and swap-remove in Sijo) is small next to the O(N²) work.
* **Deferred writes and churn are exact in Sijo:** cross-entity `Hp -= Damage` and `Kills += 1`
  are merged deterministically; spawn/destroy counts, entity state and hit counts are identical
  across all implementations and thread counts.

### `O3-native`: what a best-case gcc build changes

* **boids:** Sijo 3.73 ms, hand C 17.8 ms, OOP C++ 20.9 ms at 4k: the grid decides it.
* **particles:** both loops are vectorized (AVX2); the hand-written loop, which computes the
  respawn values with branch-free selects, is 1.0–1.2× faster (91 µs vs 109 µs at 100k, 1.12 ms vs
  1.16 ms at 1M).
* **shooter:** Sijo 3.80 ms, hand C 4.88 ms, OOP C++ 5.78 ms at 2000 ships.

### memory

Peak RSS (tables above): the hand C is the floor (payload only). Sijo adds a slot table
(16 B/entity), an entity handle column and per-row bookkeeping: particles 1M 45 MB vs hand C
25 MB — less than OOP C++ (119 MB). For the small workloads everything is a few MB.

## Multi-core scaling in one place

| workload | size | Sijo 1T → 4T | hand C 1T → 4T |
|---|---|---|---|
| boids | 1k | 2.1× | 3.6× |
| boids | 4k | 3.2× | 3.6× |
| particles | 100k | 3.4× | 3.6× |
| particles | 1M | 3.4× | 3.6× |
| shooter | 2000 ships | 3.3× | 1.3× |

(× = time on 1 thread ÷ time on 4 threads, `O2`. The hand-written shooter keeps its ship system
serial (see the shooter analysis); the VM is shared, so no implementation reaches 4×.)

## Reproducing

```bash
bench/run.sh                      # O2 (sijoc's flags), 3 rounds; rewrites the tables in bench/RESULTS.md
bench/run.sh --quick              # smoke test: smallest sizes, 1 round (~1 min)
bench/run.sh --native             # also O3-native (what produced this file)
bench/run.sh --native --dotnet ~/.dotnet/dotnet   # with the C# variant
python3 bench/shards.py           # shard processes 1/2/4 on boids 16000 and 32000 (the shard section)
python3 bench/shards.py --quick   # boids 4000, 1 round
```

Needs Rust/cargo (builds `sijoc`), gcc/g++ with OpenMP, Python 3; optionally a .NET 8 SDK
for the C# variant (`--dotnet PATH`, `$DOTNET`, or `dotnet` on `PATH`; it is skipped
otherwise). Raw data of the run (every sample, medians, load averages, interference retries) is in
`bench/results/latest-full.json`; the tables alone are in `bench/results/latest-full.md`.

Layout:

```
bench/
  run.py, run.sh                  harness (build, run, validate, report)
  shards.py                       shard processes: tick time and exchange volume (results/shards.md)
  common/bench.h                  timing/args/integer helpers shared by the C/C++ baselines
  common/sijo_compat.h            Sijo-runtime-compatible world checksum
  common/oop.hpp                  the Actor/World framework of the OOP C++ versions
  common/maxrss.c                 peak-RSS + CPU-time wrapper used by the harness
  boids/      boids.sijo      boids_dod.c      boids_oop.cpp
  particles/  particles.sijo  particles_dod.c  particles_oop.cpp
  shooter/    shooter.sijo    shooter_dod.c    shooter_oop.cpp
  csharp/     Bench.csproj, Framework.cs, Workloads.cs   (all three workloads)
  RESULTS.md  this file (tables between the GENERATED markers are rewritten by run.py)
```

Size variants of the `.sijo` programs are made by substituting `COUNT` (boids,
particles) or `SHIPS`/`ARENA` (shooter) into a copy under `bench/build/gen/`; the
generated C files are kept there for inspection.
