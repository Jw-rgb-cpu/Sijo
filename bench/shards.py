#!/usr/bin/env python3
"""bench/shards.py — tick time and exchange volume of shard processes (docs/05 5.7).

  python3 bench/shards.py                  # boids 16000 and 32000, 300 ticks, 3 rounds
  python3 bench/shards.py --quick          # boids 4000, 100 ticks, 1 round

The workload is bench/boids/boids.sijo with COUNT boids in a world scaled so that the density
(and so the neighbours per boid) stays that of 1000 boids in 200x200. Configurations on the
machine's cores: one process (1 and all threads), and 2 or 4 shard processes on localhost
(--shard-of N), each with cores/N OpenMP threads. Every run must print the same "bench" lines
and world checksum as the one-process run (verified). Time per tick = (T(N ticks) - T(1 tick))
/ (N - 1), minimum over the rounds, T as printed by the coordinator ("time: ... total").
Exchange volume: bytes and messages of all shards (the coordinator's summary), per tick.

Two variants of the workload: "hash" is bench/boids as is: Game.Update reads every boid's Pos, Vel
and Neighbours (an order-independent hash every 100 ticks), so it runs on the coordinator and every
shard sends it every changed boid each tick; "halo" prints only the count, so shards exchange only
the boids near their slab boundaries (Pos and Vel within the proven radius 12 + hysteresis 3).
"""
import argparse
import os
import re
import socket
import subprocess
import sys

BENCH = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(BENCH)
SIJOC = os.path.join(ROOT, "compiler", "target", "release", "sijoc")
BUILD = os.path.join(BENCH, "build", "shards")


def free_ports(n):
    socks = []
    for _ in range(n):
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.bind(("127.0.0.1", 0))
        socks.append(s)
    ports = [s.getsockname()[1] for s in socks]
    for s in socks:
        s.close()
    return ports


def build(count, variant="hash"):
    src = open(os.path.join(BENCH, "boids", "boids.sijo")).read()
    if variant == "halo":
        start = src.index("    public static void Update(float dt)\n    {\n        if (Time.Tick % 100 == 0)")
        end = src.index("\n}", src.index("Print($\"bench tick", start))
        src = src[:start] + """    public static void Update(float dt)
    {
        if (Time.Tick % 100 == 0)
        {
            Print($"bench tick={Time.Tick} n={Count<Boid>()}");
        }
    }""" + src[end:]
    world = int(200 * (count / 1000.0) ** 0.5)
    src = re.sub(r"public const int COUNT = \d+;", "public const int COUNT = %d;" % count, src)
    src = re.sub(r"public const float WORLD = \d+;", "public const float WORLD = %d;" % world, src)
    src = src.replace("in 0..200 Pos", "in 0..%d Pos" % world)
    os.makedirs(BUILD, exist_ok=True)
    path = os.path.join(BUILD, "boids%d%s.sijo" % (count, variant))
    open(path, "w").write(src)
    exe = os.path.join(BUILD, "boids%d%s" % (count, variant))
    subprocess.run([SIJOC, "build", path, "-o", exe + ".c", "--exe", exe], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return exe, world


def run_one(exe, ticks, threads):
    env = dict(os.environ, OMP_NUM_THREADS=str(threads), OMP_WAIT_POLICY="passive")
    out = subprocess.run([exe, "--ticks", str(ticks), "--timing"], env=env, capture_output=True, text=True, timeout=600).stdout
    return out


def run_shards(exe, ticks, n, threads):
    env = dict(os.environ, OMP_NUM_THREADS=str(threads), OMP_WAIT_POLICY="passive")
    peers = ",".join("127.0.0.1:%d" % p for p in free_ports(n))
    workers = [subprocess.Popen([exe, "--shard-of", str(n), "--shard-id", str(i), "--peers", peers, "--timing"], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True) for i in range(1, n)]
    out = subprocess.run([exe, "--shard-of", str(n), "--shard-id", "0", "--peers", peers, "--ticks", str(ticks), "--timing"], env=env, capture_output=True, text=True, timeout=600).stdout
    for w in workers:
        w.wait(timeout=60)
    return out


def total_ms(out):
    m = re.search(r"^time: ([0-9.]+) ms total", out, re.M)
    if not m:
        sys.exit("no time line in:\n" + out)
    return float(m.group(1))


def results(out):
    return [l for l in out.splitlines() if l.startswith("bench ") or l.startswith("world checksum")]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--sizes", default="")
    ap.add_argument("--variants", default="hash,halo")
    args = ap.parse_args()
    cores = os.cpu_count() or 4
    sizes = [4000] if args.quick else [16000, 32000]
    if args.sizes:
        sizes = [int(s) for s in args.sizes.split(",")]
    ticks = 100 if args.quick else 300
    rounds = 1 if args.quick else args.rounds
    configs = [("1 process, 1 thread", 1, 1), ("1 process, %d threads" % cores, 1, cores), ("2 shard processes x 1 thread", 2, 1),
               ("2 shard processes x %d threads" % (cores // 2), 2, max(1, cores // 2)), ("4 shard processes x %d thread" % max(1, cores // 4), 4, max(1, cores // 4))]
    rows = []
    for count, variant in [(c, v) for v in args.variants.split(",") for c in sizes]:
        exe, world = build(count, variant)
        ref = results(run_one(exe, ticks, 1))
        for label, n, threads in configs:
            best_n = best_1 = None
            vol = msgs = ""
            for _ in range(rounds):
                if n == 1:
                    o_n = run_one(exe, ticks, threads)
                    o_1 = run_one(exe, 1, threads)
                else:
                    o_n = run_shards(exe, ticks, n, threads)
                    o_1 = run_shards(exe, 1, n, threads)
                    m = re.search(r"shard exchange: (\d+) bytes in (\d+) messages", o_n)
                    vol = "%.1f KB" % (int(m.group(1)) / 1024.0 / ticks)
                    msgs = "%.1f" % (int(m.group(2)) / ticks)
                if results(o_n) != ref:
                    sys.exit("%s, %d boids: results differ from one process:\n%s\n---\n%s" % (label, count, "\n".join(ref), "\n".join(results(o_n))))
                t_n, t_1 = total_ms(o_n), total_ms(o_1)
                best_n = t_n if best_n is None else min(best_n, t_n)
                best_1 = t_1 if best_1 is None else min(best_1, t_1)
            per_tick = (best_n - best_1) / (ticks - 1)
            rows.append((variant, count, world, label, per_tick, vol, msgs))
            print("%-4s %6d boids (%dx%d)  %-32s %8.2f ms/tick  %s/tick  %s msgs/tick" % (variant, count, world, world, label, per_tick, vol or "-", msgs or "-"), flush=True)
    print()
    print("| workload | boids (world) | configuration | ms per tick | speed-up | shard exchange per tick (all shards) | messages per tick |")
    print("|---|---|---|---|---|---|---|")
    base = {}
    for variant, count, world, label, t, vol, msgs in rows:
        if label.startswith("1 process, 1 thread"):
            base[(variant, count)] = t
        print("| %s | %d (%dx%d) | %s | %.2f | %.2fx | %s | %s |" % (variant, count, world, world, label, t, base[(variant, count)] / t if t > 0 else 0, vol or "-", msgs or "-"))


if __name__ == "__main__":
    main()
