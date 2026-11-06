#!/usr/bin/env python3
"""bench/run.py — builds and runs the Sijo benchmark suite, validates that every
implementation produces identical results, and writes the tables of bench/RESULTS.md.

  python3 bench/run.py                 # default: -O2 (sijoc's own flags), 3 rounds
  python3 bench/run.py --native        # also an -O3 -march=native configuration
  python3 bench/run.py --quick         # small sizes, 1 repetition (smoke test, < 1 min)
  python3 bench/run.py --dotnet PATH   # path to the dotnet executable for the C# variant

Measurement protocol (identical for every implementation):
  every program takes t0 before creating/spawning the world and t1 after the last
  tick and prints "time: <ms> ms total" (this is what the Sijo runtime does).
  Each case is run with --ticks N and with --ticks 1, in several rounds; within a round
  every case runs once (round-robin), so interference from other processes on the
  machine is spread over all implementations. The steady-state cost is
      per_tick = (min T(N) - min T(1)) / (N - 1)
  so start-up (spawning, JIT, page faults of the first tick) cancels out; interference
  can only add time, hence the minimum (medians are kept in the JSON).
  Peak RSS comes from wait4() (ru_maxrss) of the --ticks N runs.
  Thread counts are set with OMP_NUM_THREADS; OOP C++ and C# are single-threaded.

Validation: every run of every implementation must print exactly the same
"bench ..." lines as the Sijo reference (an order-independent integer hash of all
entity state every 100 ticks) and, where applicable, the same Sijo-compatible
world checksum / entity counts. Floats are compared bit for bit through the hash.
"""
import argparse
import datetime
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import time

BENCH = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(BENCH)
COMPILER = os.path.join(ROOT, "compiler")
SIJOC = os.path.join(COMPILER, "target", "release", "sijoc")
BUILD = os.path.join(BENCH, "build")

# exactly the flags sijoc's driver uses (compiler/src/driver.rs), plus -ffp-contract=off
# (a no-op for plain x86-64 builds; it keeps -march=native builds from fusing a*b+c
# differently in different implementations, which would change float results).
SIJOC_SEMANTIC = ["-fwrapv"]
CONFIGS = {
    "O2": ["-O2"],
    "O3-native": ["-O3", "-march=native", "-fno-trapping-math"],
}

IMPLS = ["sijo", "dod", "oop", "oop-scatter", "cs"]
IMPL_LABEL = {
    "sijo": "Sijo",
    "dod": "hand C (DOD)",
    "oop": "OOP C++",
    "oop-scatter": "OOP C++ aged heap",
    "cs": "OOP C#",
}
THREADED = {"sijo", "dod"}


def wl_boids_subst(src, size):
    return re.sub(r"public const int COUNT = \d+;", "public const int COUNT = %d;" % size, src)


def wl_particles_subst(src, size):
    return re.sub(r"public const int COUNT = \d+;", "public const int COUNT = %d;" % size, src)


def shooter_arena(size):
    return {500: 256, 1000: 362, 2000: 512}.get(size, 256)


def wl_shooter_subst(src, size):
    src = re.sub(r"public const int SHIPS = \d+;", "public const int SHIPS = %d;" % size, src)
    return re.sub(r"public const float ARENA = \d+;", "public const float ARENA = %d;" % shooter_arena(size), src)


WORKLOADS = {
    "boids": {
        "dir": "boids",
        "subst": wl_boids_subst,
        "sizes": [1000, 2000, 4000],
        "quick_sizes": [1000],
        "ticks": {1000: 300, 2000: 200, 4000: 100},
        "extra_args": lambda size: [],
        "check": "world checksum",
        "unit": "boids",
        "scatter": True,
    },
    "particles": {
        "dir": "particles",
        "subst": wl_particles_subst,
        "sizes": [100000, 1000000],
        "quick_sizes": [100000],
        "ticks": {100000: 300, 1000000: 100},
        "extra_args": lambda size: [],
        "check": "world checksum",
        "unit": "particles",
        "scatter": True,
    },
    "shooter": {
        "dir": "shooter",
        "subst": wl_shooter_subst,
        "sizes": [500, 2000],
        "quick_sizes": [500],
        "ticks": {500: 600, 2000: 200},
        "extra_args": lambda size: ["--arena", str(shooter_arena(size))],
        "check": "entities alive",
        "unit": "ships",
    },
}


def log(msg):
    print(msg, flush=True)


def run_cmd(cmd, cwd=None, env=None):
    r = subprocess.run(cmd, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    if r.returncode != 0:
        raise SystemExit("command failed (%d): %s\n%s" % (r.returncode, " ".join(cmd), r.stdout))
    return r.stdout


MAXRSS = os.path.join(BUILD, "maxrss")


def cpu_busy_jiffies():
    """System-wide busy CPU time (all cores) in USER_HZ ticks, including steal."""
    with open("/proc/stat") as f:
        v = [int(x) for x in f.readline().split()[1:]]
    user, nice, system, idle, iowait, irq, softirq, steal = (v + [0] * 8)[:8]
    return user + nice + system + irq + softirq + steal


HZ = os.sysconf("SC_CLK_TCK")
QUIET_CORES = 0.35       # background (the runner's own tooling) is ~0.05-0.1 cores
DISTURBED_CORES = 0.30   # CPU used by *other* processes during a run, averaged over the run


def wait_for_quiet(max_wait=90.0):
    """Waits until the other processes on the machine use < QUIET_CORES cores. Returns seconds waited."""
    t0 = time.time()
    while True:
        b0 = cpu_busy_jiffies()
        time.sleep(1.0)
        busy = (cpu_busy_jiffies() - b0) / HZ
        if busy < QUIET_CORES or time.time() - t0 > max_wait:
            return time.time() - t0


STATS = {"retries": 0, "waited_s": 0.0, "accepted_disturbed": 0}


def timed_run(cmd, env, max_tries=3):
    """Runs cmd under the maxrss wrapper, returns (stdout, peak RSS in KiB).

    The wrapper also reports the child's CPU time; together with the system-wide busy time
    from /proc/stat this tells how much CPU *other* processes used during the run. A run in
    which they used more than DISTURBED_CORES cores on average is discarded and repeated
    after the machine is quiet again (the machine is shared with other sessions)."""
    for attempt in range(max_tries):
        b0, w0 = cpu_busy_jiffies(), time.time()
        r = subprocess.run([MAXRSS] + cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        wall, busy = time.time() - w0, (cpu_busy_jiffies() - b0) / HZ
        if r.returncode != 0:
            raise SystemExit("benchmark failed (%d): %s\n%s%s" % (r.returncode, " ".join(cmd), r.stdout, r.stderr))
        m = re.search(r"maxrss_kb=(\d+) cpu_s=([0-9.]+)", r.stderr)
        rss, own = (int(m.group(1)), float(m.group(2))) if m else (0, busy)
        others = max(busy - own, 0.0) / max(wall, 1e-3)
        # short runs (< 50 ms) are too short for a meaningful jiffy-based estimate
        if wall < 0.05 or others < DISTURBED_CORES:
            return r.stdout, rss
        STATS["retries"] += 1
        log("    (other processes used %.2f cores during %s; waiting for a quiet machine and repeating)" % (others, os.path.basename(cmd[0])))
        STATS["waited_s"] += wait_for_quiet()
    STATS["accepted_disturbed"] += 1
    return r.stdout, rss


def parse(out):
    m = re.search(r"^time: ([0-9.]+) ms total", out, re.M)
    if not m:
        raise SystemExit("no timing line in output:\n" + out)
    res = {"total_ms": float(m.group(1))}
    res["bench"] = re.findall(r"^bench .*$", out, re.M)
    m = re.search(r"^world checksum: ([0-9a-f]+)", out, re.M)
    res["world checksum"] = m.group(1) if m else None
    m = re.search(r"^entities alive: (.*)$", out, re.M)
    res["entities alive"] = m.group(1).strip() if m else None
    return res


def gcc_version(cc):
    try:
        return subprocess.run([cc, "--version"], stdout=subprocess.PIPE, text=True).stdout.splitlines()[0]
    except OSError:
        return "?"


def cpu_model():
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "?"


def compiler_commit():
    """Last commit that touched compiler/ (what sijoc was built from), plus '+dirty' if it has local changes;
    empty outside a git checkout or before the first commit."""
    c = subprocess.run(["git", "-C", ROOT, "log", "-1", "--format=%h", "--", "compiler"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True).stdout.strip()
    if not c:
        return ""
    dirty = subprocess.run(["git", "-C", ROOT, "status", "--porcelain", "--", "compiler"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True).stdout.strip()
    return c + ("+dirty" if dirty else "")


def sijoc_version():
    """`sijoc --version` without the program name, e.g. '0.1.0'."""
    out = subprocess.run([SIJOC, "--version"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True).stdout.strip()
    return out.split()[-1] if out else "?"


def sijoc_label(meta):
    """'0.1.0' or '0.1.0 @ abc1234' (results recorded before sijoc_version existed: the commit)."""
    commit = meta.get("compiler_commit", meta.get("sijoc_commit", ""))
    if "sijoc_version" not in meta:
        return "@ " + commit
    return meta["sijoc_version"] + (" @ " + commit if commit else "")


def find_dotnet(arg):
    cands = [arg, os.environ.get("DOTNET"), shutil.which("dotnet"), os.path.expanduser("~/.dotnet/dotnet")]
    for c in cands:
        if c and os.path.isfile(c) and os.access(c, os.X_OK):
            return c
    return None


class Builder:
    def __init__(self, cfg_name, cc, cxx):
        self.cfg = cfg_name
        self.opt = CONFIGS[cfg_name]
        self.cc = cc
        self.cxx = cxx
        self.dir = os.path.join(BUILD, cfg_name)
        os.makedirs(self.dir, exist_ok=True)

    def cflags(self):
        return ["-std=gnu11"] + self.opt + SIJOC_SEMANTIC + ["-ffp-contract=off", "-fopenmp"]

    def cxxflags(self):
        return ["-std=gnu++17"] + self.opt + SIJOC_SEMANTIC + ["-ffp-contract=off", "-fopenmp"]

    def sijo(self, wl, size):
        """sijoc emits C for the size variant; we compile it with this configuration's flags."""
        w = WORKLOADS[wl]
        src_path = os.path.join(BENCH, w["dir"], wl + ".sijo")
        with open(src_path) as f:
            src = w["subst"](f.read(), size)
        gen_dir = os.path.join(BUILD, "gen")
        os.makedirs(gen_dir, exist_ok=True)
        sijo_path = os.path.join(gen_dir, "%s_%d.sijo" % (wl, size))
        with open(sijo_path, "w") as f:
            f.write(src)
        c_path = os.path.join(gen_dir, "%s_%d.c" % (wl, size))
        exe_sijoc = os.path.join(gen_dir, "%s_%d.sijoc-exe" % (wl, size))
        # sijoc build also compiles with its own default flags (-O2 ...); we keep the C file
        run_cmd([SIJOC, "build", sijo_path, "-o", c_path, "--exe", exe_sijoc])
        exe = os.path.join(self.dir, "sijo_%s_%d" % (wl, size))
        run_cmd([self.cc] + self.cflags() + ["-w", c_path, "-o", exe, "-lm"])
        return {"sijo": exe}

    def dod(self, wl):
        w = WORKLOADS[wl]
        exe = os.path.join(self.dir, "dod_%s" % wl)
        run_cmd([self.cc] + self.cflags() + ["-Wall", "-Wextra", os.path.join(BENCH, w["dir"], wl + "_dod.c"), "-o", exe, "-lm"])
        return exe

    def oop(self, wl):
        w = WORKLOADS[wl]
        exe = os.path.join(self.dir, "oop_%s" % wl)
        run_cmd([self.cxx] + self.cxxflags() + ["-Wall", "-Wextra", os.path.join(BENCH, w["dir"], wl + "_oop.cpp"), "-o", exe, "-lm"])
        return exe


def build_csharp(dotnet):
    out_dir = os.path.join(BUILD, "csharp")
    env = dict(os.environ)
    env["DOTNET_CLI_TELEMETRY_OPTOUT"] = "1"
    env["DOTNET_NOLOGO"] = "1"
    env["DOTNET_SKIP_FIRST_TIME_EXPERIENCE"] = "1"
    run_cmd([dotnet, "build", os.path.join(BENCH, "csharp", "Bench.csproj"), "-c", "Release", "-o", out_dir, "--nologo", "-v", "q",
             "--disable-build-servers"], env=env)
    # make sure no MSBuild/Roslyn server keeps running (and using CPU) during the measurements
    subprocess.run([dotnet, "build-server", "shutdown"], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return os.path.join(out_dir, "Bench.dll")


def run_once(cmd, ticks, threads):
    """One long (--ticks N) and one short (--ticks 1) run of a case."""
    env = dict(os.environ)
    env["OMP_NUM_THREADS"] = str(threads)
    env["DOTNET_CLI_TELEMETRY_OPTOUT"] = "1"
    # C#: JIT everything fully optimized up front. With the default tiered JIT the first few
    # hundred ms run tier-0 code, which runs of 0.3-3 s cannot amortize (a 3000-tick shooter
    # run with default tiering reaches the same steady state as this setting).
    env["DOTNET_TieredCompilation"] = "0"
    out, kb = timed_run(cmd + ["--ticks", str(ticks), "--timing"], env)
    p = parse(out)
    out1, _ = timed_run(cmd + ["--ticks", "1", "--timing", "--quiet"], env)
    return p, kb, parse(out1)["total_ms"]


def summarize(samples, ticks):
    """samples: list of (parsed long run, rss_kb, short_ms), one per round.

    Primary estimator: per_tick = (min T(N) - min T(1)) / (N - 1). The machine is a shared VM
    (other sessions compile on it), and external interference can only add time, so the
    minimum over rounds is the most faithful estimate of each program's own cost. The
    median-based estimate is kept in the JSON for reference."""
    longs = [x[0]["total_ms"] for x in samples]
    shorts = [x[2] for x in samples]
    per_min = max(min(longs) - min(shorts), 0.0) * 1000.0 / (ticks - 1)
    per_med = max(statistics.median(longs) - statistics.median(shorts), 0.0) * 1000.0 / (ticks - 1)
    return {
        "per_tick_us": per_min,
        "per_tick_median_us": per_med,
        "startup_ms": max(min(shorts) - per_min / 1000.0, 0.0),
        "long_ms": longs,
        "short_ms": shorts,
        "spread_pct": (max(longs) - min(longs)) / min(longs) * 100.0 if min(longs) > 0 else 0.0,
        "rss_kb": statistics.median([x[1] for x in samples]),
        "outputs": [x[0] for x in samples],
    }


def loadavg():
    try:
        with open("/proc/loadavg") as f:
            return float(f.read().split()[0])
    except OSError:
        return -1.0


def fmt_us(us):
    if us >= 10000:
        return "%.1f ms" % (us / 1000.0)
    if us >= 1000:
        return "%.2f ms" % (us / 1000.0)
    return "%.0f µs" % us


def fmt_ratio(x):
    if x >= 10:
        return "%.0f×" % x
    if x >= 3:
        return "%.1f×" % x
    return "%.2f×" % x


def size_label(wl, size):
    if wl == "particles":
        return "%dk" % (size // 1000) if size < 1000000 else "%dM" % (size // 1000000)
    if wl == "shooter":
        return "%d ships (arena %d)" % (size, shooter_arena(size))
    return "%dk" % (size // 1000) if size % 1000 == 0 else str(size)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--reps", type=int, default=3, help="rounds per size (default 3; --quick: 1)")
    ap.add_argument("--quick", action="store_true")
    ap.add_argument("--native", action="store_true", help="also run the -O3 -march=native configuration")
    ap.add_argument("--only-native", action="store_true", help="run only the -O3 -march=native configuration")
    ap.add_argument("--workloads", default="boids,particles,shooter")
    ap.add_argument("--threads", default="1,2,4")
    ap.add_argument("--dotnet", default=None)
    ap.add_argument("--no-csharp", action="store_true")
    ap.add_argument("--no-report", action="store_true", help="do not rewrite RESULTS.md")
    ap.add_argument("--report-only", action="store_true",
                    help="do not run anything: re-render the tables of RESULTS.md from results/latest-full.json")
    args = ap.parse_args()

    if args.report_only:
        with open(os.path.join(BENCH, "results", "latest-full.json")) as f:
            results = json.load(f)
        results["meta"].setdefault("compiler_commit", compiler_commit())
        md = render(results)
        with open(os.path.join(BENCH, "results", "latest-full.md"), "w") as f:
            f.write(md)
        update_results_md(md)
        log("== re-rendered bench/results/latest-full.md and the tables in bench/RESULTS.md")
        return 0

    reps = 1 if args.quick and args.reps == 3 else args.reps
    threads = [int(t) for t in args.threads.split(",")]
    workloads = [w for w in args.workloads.split(",") if w]
    configs = ["O3-native"] if args.only_native else (["O2", "O3-native"] if args.native else ["O2"])
    cc = os.environ.get("CC", "gcc")
    cxx = os.environ.get("CXX", "g++")

    log("== building sijoc")
    run_cmd(["cargo", "build", "--release", "--quiet"], cwd=COMPILER)
    os.makedirs(BUILD, exist_ok=True)
    run_cmd([cc, "-O2", os.path.join(BENCH, "common", "maxrss.c"), "-o", MAXRSS])

    dotnet = None if args.no_csharp else find_dotnet(args.dotnet)
    cs_dll = None
    if dotnet:
        log("== building C# (%s)" % dotnet)
        cs_dll = build_csharp(dotnet)
    else:
        log("== C#: no dotnet found (pass --dotnet PATH); skipping the C# variant")

    t_start = time.time()
    results = {"meta": {
        "date": datetime.datetime.now().strftime("%Y-%m-%d %H:%M"),
        "cpu": cpu_model(),
        "cores": os.cpu_count(),
        "cc": gcc_version(cc),
        "cxx": gcc_version(cxx),
        "dotnet": (subprocess.run([dotnet, "--version"], stdout=subprocess.PIPE, text=True).stdout.strip() if dotnet else None),
        "reps": reps,
        "threads": threads,
        "sijoc_version": sijoc_version(),
        "sijoc_commit": subprocess.run(["git", "-C", ROOT, "rev-parse", "--short", "HEAD"], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True).stdout.strip(),
        "compiler_commit": compiler_commit(),
    }, "configs": {}}

    for cfg in configs:
        log("== configuration %s: %s" % (cfg, " ".join(CONFIGS[cfg])))
        b = Builder(cfg, cc, cxx)
        cres = {"flags_c": " ".join(b.cflags()), "flags_cxx": " ".join(b.cxxflags()), "workloads": {}}
        results["configs"][cfg] = cres
        for wl in workloads:
            w = WORKLOADS[wl]
            sizes = w["quick_sizes"] if args.quick else w["sizes"]
            dod_exe, oop_exe = b.dod(wl), b.oop(wl)
            wres = cres["workloads"][wl] = {}
            for size in sizes:
                ticks = w["ticks"][size]
                if args.quick:
                    ticks = min(ticks, 200)
                exes = b.sijo(wl, size)
                base_args = ["--n", str(size)] + w["extra_args"](size)
                cases = []
                for t in threads:
                    cases.append(("sijo", t, [exes["sijo"]]))
                for t in threads:
                    cases.append(("dod", t, [dod_exe] + base_args))
                cases.append(("oop", 1, [oop_exe] + base_args))
                if w.get("scatter"):
                    cases.append(("oop-scatter", 1, [oop_exe] + base_args + ["--scatter"]))
                if cs_dll and cfg == configs[0]:
                    cases.append(("cs", 1, [dotnet, cs_dll, wl] + base_args))
                for name in sorted(k for k in exes if k != "sijo"):
                    for t in threads:
                        cases.append((name, t, [exes[name]]))
                sres = wres[str(size)] = {"ticks": ticks, "runs": {}, "loadavg": []}
                # round-robin: every round runs every case once, so slow phases caused by other
                # processes on the machine hit all implementations alike
                samples = {i: [] for i in range(len(cases))}
                for rnd in range(reps):
                    STATS["waited_s"] += wait_for_quiet()
                    sres["loadavg"].append(loadavg())
                    for i, (impl, t, cmd) in enumerate(cases):
                        samples[i].append(run_once(cmd, ticks, t))
                    log("  %-9s %-22s round %d/%d done (load %.2f)" % (cfg, wl + " " + size_label(wl, size), rnd + 1, reps, loadavg()))
                ref = samples[0][0][0]
                for i, (impl, t, cmd) in enumerate(cases):
                    m = summarize(samples[i], ticks)
                    key = "%s@%d" % (impl, t)
                    o = m["outputs"][0]
                    ok = all(x["bench"] == ref["bench"] and x[w["check"]] == ref[w["check"]] for x in m["outputs"])
                    ok = ok and len(ref["bench"]) > 0
                    m["valid"] = ok
                    m["bench_lines"] = len(o["bench"])
                    m["check_value"] = o[w["check"]]
                    del m["outputs"]
                    sres["runs"][key] = m
                    log("  %-9s %-14s %-12s thr=%d  %10s/tick (median %10s)  startup %7.1f ms  rss %7.1f MB  spread %4.0f%%  %s" % (
                        cfg, wl + " " + size_label(wl, size)[:7], impl, t, fmt_us(m["per_tick_us"]), fmt_us(m["per_tick_median_us"]),
                        m["startup_ms"], m["rss_kb"] / 1024.0, m["spread_pct"], "OK" if ok else "MISMATCH"))
                    if not ok:
                        log("    reference: %s | %s" % (ref["bench"][-1:] if ref["bench"] else "-", ref[w["check"]]))
                        log("    this run : %s" % m["check_value"])
    results["meta"]["suite_seconds"] = round(time.time() - t_start, 1)
    results["meta"]["interference"] = dict(STATS)

    os.makedirs(os.path.join(BENCH, "results"), exist_ok=True)
    tag = "quick" if args.quick else "full"
    with open(os.path.join(BENCH, "results", "latest-%s.json" % tag), "w") as f:
        json.dump(results, f, indent=1)
    md = render(results)
    with open(os.path.join(BENCH, "results", "latest-%s.md" % tag), "w") as f:
        f.write(md)
    if not args.no_report and not args.quick:
        update_results_md(md)
    log("== done in %.0f s; tables in bench/results/latest-%s.md%s" % (
        time.time() - t_start, tag, "" if args.no_report or args.quick else " and bench/RESULTS.md"))
    bad = [(cfg, wl, s, k) for cfg, c in results["configs"].items() for wl, ws in c["workloads"].items()
           for s, ss in ws.items() for k, r in ss["runs"].items() if not r["valid"]]
    if bad:
        log("VALIDATION FAILURES: %s" % bad)
        return 1
    return 0


def render(results):
    meta = results["meta"]
    L = []
    L.append("_Generated by `bench/run.py` on %s — %s, %s cores; %s; %s%s; %d round-robin rounds, per-tick = (min T(N) − min T(1)) / (N − 1); sijoc %s; suite took %.0f s._" % (
        meta["date"], meta["cpu"], meta["cores"], meta["cc"], meta["cxx"],
        ("; .NET SDK " + meta["dotnet"]) if meta["dotnet"] else "", meta["reps"], sijoc_label(meta), meta["suite_seconds"]))
    L.append("")
    for cfg, c in results["configs"].items():
        L.append("### Configuration `%s`" % cfg)
        L.append("")
        L.append("C: `%s` · C++: `%s` (the generated Sijo C, the hand-written C and the OOP C++ all use these)." % (c["flags_c"], c["flags_cxx"]))
        L.append("")
        # main table: single-thread
        L.append("**Steady-state time per tick, 1 thread** (lower is better). *Sijo ÷ C* = Sijo time / hand-written C time; "
                 "*OOP ÷ Sijo* = speedup of Sijo over the OOP C++ version.")
        L.append("")
        has_cs = any("cs@1" in s["runs"] for ws in c["workloads"].values() for s in ws.values())
        has_sc = any("oop-scatter@1" in s["runs"] for ws in c["workloads"].values() for s in ws.values())
        hdr = "| workload | size | ticks | Sijo | hand C (DOD) | OOP C++ |" + (" OOP C++ aged heap |" if has_sc else "") + \
            (" OOP C# |" if has_cs else "") + " Sijo ÷ C | OOP ÷ Sijo |"
        L.append(hdr)
        L.append("|" + "---|" * (hdr.count("|") - 1))
        for wl, ws in c["workloads"].items():
            for size, s in ws.items():
                r = s["runs"]
                n1, d1, o1 = r["sijo@1"]["per_tick_us"], r["dod@1"]["per_tick_us"], r["oop@1"]["per_tick_us"]
                row = "| %s | %s | %d | %s | %s | %s |" % (wl, size_label(wl, int(size)), s["ticks"], fmt_us(n1), fmt_us(d1), fmt_us(o1))
                if has_sc:
                    row += " %s |" % (fmt_us(r["oop-scatter@1"]["per_tick_us"]) if "oop-scatter@1" in r else "—")
                if has_cs:
                    row += " %s |" % (fmt_us(r["cs@1"]["per_tick_us"]) if "cs@1" in r else "—")
                row += " %s | %s |" % (fmt_ratio(n1 / d1), fmt_ratio(o1 / n1))
                L.append(row)
        L.append("")
        # threads table
        L.append("**Thread scaling** (OMP_NUM_THREADS; time per tick, speedup vs. the same implementation on 1 thread in parentheses). "
                 "*OOP ÷ Sijo* compares the single-threaded OOP C++ with Sijo on the most threads.")
        L.append("")
        tlist = sorted({int(k.split("@")[1]) for ws in c["workloads"].values() for s in ws.values() for k in s["runs"] if k.startswith("sijo@")})
        hdr = "| workload | size |" + "".join(" Sijo %dT |" % t for t in tlist) + "".join(" C %dT |" % t for t in tlist) + \
            " Sijo ÷ C (%dT) | OOP ÷ Sijo (%dT) |" % (tlist[-1], tlist[-1])
        L.append(hdr)
        L.append("|" + "---|" * (hdr.count("|") - 1))
        for wl, ws in c["workloads"].items():
            for size, s in ws.items():
                r = s["runs"]
                row = "| %s | %s |" % (wl, size_label(wl, int(size)))
                for impl in ("sijo", "dod"):
                    base = r["%s@1" % impl]["per_tick_us"]
                    for t in tlist:
                        v = r["%s@%d" % (impl, t)]["per_tick_us"]
                        row += " %s%s |" % (fmt_us(v), "" if t == 1 else " (%.1f×)" % (base / v))
                nt = r["sijo@%d" % tlist[-1]]["per_tick_us"]
                row += " %s | %s |" % (fmt_ratio(nt / r["dod@%d" % tlist[-1]]["per_tick_us"]), fmt_ratio(r["oop@1"]["per_tick_us"] / nt))
                L.append(row)
        L.append("")
        # memory/startup table
        L.append("**Peak RSS and start-up** (start-up = spawning the initial entities, plus JIT for C#).")
        L.append("")
        impls = ["sijo", "dod", "oop"] + (["cs"] if has_cs else [])  # (aged heap: same RSS as OOP C++)
        hdr = "| workload | size |" + "".join(" %s RSS |" % IMPL_LABEL[i] for i in impls) + "".join(" %s start-up |" % IMPL_LABEL[i] for i in impls)
        L.append(hdr)
        L.append("|" + "---|" * (hdr.count("|") - 1))
        for wl, ws in c["workloads"].items():
            for size, s in ws.items():
                r = s["runs"]
                row = "| %s | %s |" % (wl, size_label(wl, int(size)))
                for i in impls:
                    k = "%s@1" % i
                    row += (" %.1f MB |" % (r[k]["rss_kb"] / 1024.0)) if k in r else " — |"
                for i in impls:
                    k = "%s@1" % i
                    row += (" %.1f ms |" % r[k]["startup_ms"]) if k in r else " — |"
                L.append(row)
        L.append("")
        # validation
        L.append("**Correctness**: every run above printed the same per-100-tick state hash as the Sijo reference, and the same "
                 "%s." % "Sijo-compatible world checksum (boids, particles) / entity and spawn/destroy counts (shooter)")
        L.append("")
        hdr = "| workload | size | runs checked | all identical | reference check value |"
        L.append(hdr)
        L.append("|---|---|---|---|---|")
        for wl, ws in c["workloads"].items():
            for size, s in ws.items():
                r = s["runs"]
                allok = all(x["valid"] for x in r.values())
                L.append("| %s | %s | %d (%s) | %s | `%s` |" % (wl, size_label(wl, int(size)), len(r) * results["meta"]["reps"],
                                                             ", ".join(sorted(r.keys())), "yes" if allok else "**NO**", r["sijo@1"]["check_value"]))
        L.append("")
        # noise
        worst = max(((r["spread_pct"], wl, size, k) for wl, ws in c["workloads"].items() for size, s in ws.items() for k, r in s["runs"].items()),
                    default=(0, "", "", ""))
        loads = [x for ws in c["workloads"].values() for s in ws.values() for x in s.get("loadavg", [])]
        L.append("_Run-to-run spread (max−min of the %d long runs ÷ min): worst case %.0f%% (%s %s %s); 1-min load average at the "
                 "start of the rounds: %.2f–%.2f on %s cores. This is a shared cloud VM (other sessions may compile on it); treat "
                 "differences below ~10–15%% as noise._" % (results["meta"]["reps"], worst[0], worst[1], worst[2], worst[3],
                                                          min(loads, default=0), max(loads, default=0), results["meta"]["cores"]))
        L.append("")
    return "\n".join(L) + "\n"


BEGIN = "<!-- BEGIN GENERATED TABLES (bench/run.py) -->"
END = "<!-- END GENERATED TABLES -->"


def update_results_md(md):
    path = os.path.join(BENCH, "RESULTS.md")
    block = BEGIN + "\n" + md + END
    if os.path.exists(path):
        with open(path) as f:
            text = f.read()
        if BEGIN in text and END in text:
            pre, rest = text.split(BEGIN, 1)
            _, post = rest.split(END, 1)
            text = pre + block + post
        else:
            text = text.rstrip() + "\n\n" + block + "\n"
    else:
        text = "# Sijo benchmark results\n\n" + block + "\n"
    with open(path, "w") as f:
        f.write(text)


if __name__ == "__main__":
    sys.exit(main())
