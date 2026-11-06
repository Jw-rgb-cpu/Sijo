//! Distributed simulation, end to end: the same program and seed must give the same world
//! checksum, replicated state hash and printed output on 1 process and on 2..4 shards, as worlds
//! in one process (--shards N) or as separate processes over localhost UDP (--shard-of N), for any
//! number of threads, through simulated loss, latency, jitter and duplication on the shard links,
//! with clients that join late (the coordinator is their gateway), and after a shard is killed and
//! restarted from its checkpoint (--recover). Covers the examples (arena with RPCs, boids with the
//! halo), and tests/programs/shards.sijo: many handoffs, remote writes, messages, spawns and
//! destroys across shards, strings, lists and dictionaries copied between shards, a replicated
//! system behind a spawn merge. Requires a C compiler; tests are skipped if none is found.

use sijoc::diag::Diags;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

fn cc() -> Option<String> {
    for c in [std::env::var("CC").unwrap_or_default(), "gcc".into(), "clang".into(), "cc".into()] {
        if !c.is_empty() && Command::new(&c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(c);
        }
    }
    None
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// `examples/x.sijo` or `tests/programs/x.sijo`, built with the given C flags (OpenMP when it is
/// available, so thread counts can vary; sanitizers on request)
fn build(rel: &str, tag: &str, flags: &[&str]) -> Option<PathBuf> {
    let cc = cc()?;
    let path = root().join(rel);
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap_or_else(|| panic!("{} failed to compile: {:?}", rel, d.list));
    static BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sijoc-shard-{}-{}-{}", std::process::id(), n, tag));
    std::fs::create_dir_all(&dir).unwrap();
    let cfile = dir.join("prog.c");
    let exe = dir.join("prog");
    std::fs::write(&cfile, sijoc::codegen::generate(&c, &path.display().to_string())).unwrap();
    for omp in [true, false] {
        let mut cmd = Command::new(&cc);
        cmd.args(["-std=gnu11", "-O2", "-ffp-contract=off", "-w"]).args(flags);
        if omp {
            cmd.arg("-fopenmp");
        }
        let out = cmd.arg(&cfile).arg("-o").arg(&exe).arg("-lm").output().unwrap();
        if out.status.success() {
            return Some(exe);
        }
        if !omp || flags.iter().any(|f| f.starts_with("-fsanitize")) {
            if flags.iter().any(|f| f.starts_with("-fsanitize")) {
                return None; // no sanitizer runtime here
            }
            panic!("C compilation failed:\n{}", String::from_utf8_lossy(&out.stderr));
        }
    }
    None
}

fn run(exe: &Path, args: &[&str], threads: usize) -> (i32, String) {
    let o = Command::new(exe).args(args).env("OMP_NUM_THREADS", threads.to_string()).output().unwrap();
    (o.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)))
}

/// the output that must not depend on the number of shards: everything except the shard
/// statistics, timings and memory statistics (string heap, frame arena)
fn comparable(out: &str) -> String {
    out.lines()
        .filter(|l| !l.starts_with("shard") && !l.starts_with("ticks:") && !l.starts_with("time:") && !l.starts_with("frame arena") && !l.starts_with("strings:") && !l.starts_with("sijo shard") && !l.starts_with("network simulation (shard") && !l.starts_with("encryption (shard"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn line(out: &str, prefix: &str) -> String {
    out.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no '{}' line in:\n{}", prefix, out)).to_string()
}

fn number_after(l: &str, word: &str) -> i64 {
    let words: Vec<&str> = l.split_whitespace().collect();
    let i = words.iter().position(|w| w.trim_matches(|c: char| !c.is_alphanumeric()) == word).unwrap_or_else(|| panic!("no '{}' in {}", word, l));
    words[i - 1].trim_matches(|c: char| !c.is_ascii_digit()).parse().unwrap()
}

/// free UDP ports on localhost (bound and released: the shard processes bind them right after)
fn ports(n: usize) -> Vec<u16> {
    let socks: Vec<std::net::UdpSocket> = (0..n).map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap()).collect();
    socks.iter().map(|s| s.local_addr().unwrap().port()).collect()
}

/// A child process whose output lines are collected by background threads; killed when dropped.
struct Proc {
    name: String,
    child: Child,
    rx: Receiver<String>,
    lines: Vec<String>,
}

impl Proc {
    fn spawn(name: &str, exe: &Path, args: &[&str], threads: usize) -> Proc {
        let mut child = Command::new(exe)
            .args(args)
            .env("OMP_NUM_THREADS", threads.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {}", exe.display(), e));
        let (tx, rx) = channel();
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        for stream in [Box::new(out) as Box<dyn Read + Send>, Box::new(err)] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                for l in BufReader::new(stream).lines().map_while(Result::ok) {
                    if tx.send(l).is_err() {
                        break;
                    }
                }
            });
        }
        Proc { name: name.to_string(), child, rx, lines: vec![] }
    }

    fn wait_for(&mut self, needle: &str, timeout: Duration) -> String {
        let end = Instant::now() + timeout;
        loop {
            if let Some(l) = self.lines.iter().find(|l| l.contains(needle)) {
                return l.clone();
            }
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(l) => self.lines.push(l),
                Err(_) if Instant::now() >= end => panic!("{}: no '{}' within {:?}; output:\n{}", self.name, needle, timeout, self.lines.join("\n")),
                Err(_) => {}
            }
        }
    }

    fn finish(&mut self, timeout: Duration) -> i32 {
        let end = Instant::now() + timeout;
        let code = loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                break s.code().unwrap_or(-1);
            }
            if Instant::now() >= end {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.drain();
                panic!("{} did not finish within {:?}; output:\n{}", self.name, timeout, self.lines.join("\n"));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        self.drain();
        code
    }

    fn drain(&mut self) {
        let end = Instant::now() + Duration::from_secs(2);
        while let Ok(l) = self.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            self.lines.push(l);
        }
    }

    fn output(&self) -> String {
        self.lines.join("\n")
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// N shard processes: the workers (1..N), then the coordinator with `coord` arguments. Returns the
/// coordinator's output once every process ended successfully.
struct Run {
    procs: Vec<Proc>,
}

fn peers(p: &[u16]) -> String {
    p.iter().map(|x| format!("127.0.0.1:{}", x)).collect::<Vec<_>>().join(",")
}

fn start_shards(exe: &Path, n: usize, coord: &[&str], worker: &[&str], threads: usize) -> Run {
    let p = peers(&ports(n));
    let ns = n.to_string();
    let mut procs = vec![];
    for i in 1..n {
        let id = i.to_string();
        let mut a = vec!["--shard-of", ns.as_str(), "--shard-id", id.as_str(), "--peers", p.as_str()];
        a.extend_from_slice(worker);
        procs.push(Proc::spawn(&format!("shard {}", i), exe, &a, threads));
    }
    let mut a = vec!["--shard-of", ns.as_str(), "--shard-id", "0", "--peers", p.as_str()];
    a.extend_from_slice(coord);
    procs.insert(0, Proc::spawn("coordinator", exe, &a, threads));
    Run { procs }
}

impl Run {
    fn finish(mut self, timeout: Duration) -> (String, Vec<String>) {
        let mut outs = vec![];
        for p in self.procs.iter_mut() {
            let code = p.finish(timeout);
            assert_eq!(code, 0, "{} failed (exit {}):\n{}", p.name, code, p.output());
            outs.push(p.output());
        }
        let coord = outs.remove(0);
        (coord, outs)
    }
}

const ARENA: &str = "../examples/arena.sijo";
const BOIDS: &str = "../examples/boids.sijo";
const SHARDS: &str = "tests/programs/shards.sijo";

/// --shards N: N shard worlds in one process (the same exchange as shard processes, in memory)
/// give the single-process output, checksum and replicated hash, for any thread count; also for
/// dynamic components, generic entity classes, multi-dimensional arrays and list messages.
#[test]
fn shard_worlds_match_one_process() {
    for (rel, ticks) in [(ARENA, "600"), (BOIDS, "300"), ("../examples/village.sijo", "600"), (SHARDS, "400"), ("tests/programs/spawnhandles.sijo", "120"), ("tests/programs/messages.sijo", "120"), ("tests/programs/shardwave.sijo", "200"), ("../bench/shooter/shooter.sijo", "120"),
        // language features that meet the shard layer: components, generic entity classes, multi-dimensional arrays,
        // list arguments of messages, collections of references
        ("tests/programs/components.sijo", "40"), ("tests/programs/genclasses.sijo", "60"), ("tests/programs/mdarrays.sijo", "40"), ("tests/programs/msglists.sijo", "40"), ("tests/programs/reflists.sijo", "120")] {
        let Some(exe) = build(rel, "worlds", &[]) else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let (code, one) = run(&exe, &["--ticks", ticks], 1);
        assert_eq!(code, 0, "{}", one);
        if rel == ARENA {
            assert!(one.contains("world checksum: 13d8c02780895d46"), "the arena example changed:\n{}", one);
        }
        for (shards, threads) in [("2", 1), ("3", 4), ("4", 2), ("4", 4)] {
            let (code, out) = run(&exe, &["--ticks", ticks, "--shards", shards], threads);
            assert_eq!(code, 0, "{} --shards {}:\n{}", rel, shards, out);
            assert_eq!(comparable(&one), comparable(&out), "{} on {} shards ({} threads) differs from one process", rel, shards, threads);
            assert!(out.contains(&format!("shards: {} (worlds in this process)", shards)), "{}", out);
        }
    }
}

/// The cross-shard program really crosses shards: entities change owner often, the halo and the
/// spawn merges are used, commands and spawns travel; one shard and many give the same results.
#[test]
fn cross_shard_writes_spawns_destroys_and_handoffs() {
    let Some(exe) = build(SHARDS, "cross", &[]) else { return };
    let (_, one) = run(&exe, &["--ticks", "400"], 1);
    let (code, out) = run(&exe, &["--ticks", "400", "--shards", "4"], 4);
    assert_eq!(code, 0, "{}", out);
    assert_eq!(comparable(&one), comparable(&out));
    let st = line(&out, "shards: 4");
    assert!(number_after(&st, "handoffs") > 500, "few handoffs: {}", st);
    assert!(number_after(&st, "barriers") >= 400 * 5, "a spawn merge splits stage 0: {}", st);
    let ex = line(&out, "shard exchange:");
    assert!(number_after(&ex, "commands") > 10000 && number_after(&ex, "spawns") > 1000 && number_after(&ex, "halo") > 100, "{}", ex);
    // prints of the partitioned runner system come out in the single-process order
    assert!(one.contains("tick 400: runner 155 hp"), "{}", one);
    let plan = Command::new(env!("CARGO_BIN_EXE_sijoc")).args(["explain"]).arg(root().join(SHARDS)).output().unwrap();
    let plan = String::from_utf8_lossy(&plan.stdout);
    assert!(plan.contains("Hive.Update: replicated"), "{}", plan);
    assert!(plan.contains("Runner.Update: partitioned"), "{}", plan);
    assert!(plan.contains("Runner_C0 {Pos} -> halo 6 + 1.5"), "{}", plan);
    assert!(plan.contains("Game.Update: coordinator"), "{}", plan);
    // a dynamic component's combination is an archetype of its own, partitioned like its class
    assert!(plan.contains("Runner+Stung by Pos.X") && plan.contains("Stung.Update: partitioned"), "{}", plan);
    assert!(one.contains("stung="), "{}", one);
    // a replicated Game.Update that spawns waits for the spawns of its stage to be merged
    let wave = Command::new(env!("CARGO_BIN_EXE_sijoc")).args(["explain"]).arg(root().join("tests/programs/shardwave.sijo")).output().unwrap();
    let wave = String::from_utf8_lossy(&wave.stdout);
    assert!(wave.contains("Game.Update: replicated") && wave.contains("plus spawn merges"), "{}", wave);
}

/// Shard processes over localhost UDP give the single-process results: arena (RPCs from the
/// coordinator's in-process client, executed by every shard), boids (halo), the cross-shard program.
#[test]
fn shard_processes_match_one_process() {
    for (rel, ticks, counts) in [(ARENA, "600", vec![2usize, 4]), (BOIDS, "300", vec![4]), (SHARDS, "300", vec![3])] {
        let Some(exe) = build(rel, "procs", &[]) else { return };
        let (_, one) = run(&exe, &["--ticks", ticks], 1);
        for n in counts {
            for threads in [1, 2] {
                let r = start_shards(&exe, n, &["--ticks", ticks], &[], threads);
                let (coord, workers) = r.finish(Duration::from_secs(120));
                assert_eq!(comparable(&one), comparable(&coord), "{} on {} shard processes ({} threads) differs from one process", rel, n, threads);
                assert!(coord.contains(&format!("shards: {} (processes)", n)), "{}", coord);
                for w in &workers {
                    assert!(w.contains("summary =="), "{}", w);
                }
            }
        }
    }
}

/// The shard links lose, delay, reorder and duplicate datagrams (--sim-*), and are encrypted
/// (--key): the reliable channel hides it, and the results stay those of one process (whose
/// in-process client link gets the same simulated faults).
#[test]
fn shard_processes_survive_network_faults() {
    let sim = ["--sim-loss", "0.2", "--sim-latency-ms", "2", "--sim-jitter-ms", "2", "--sim-dup", "0.05"];
    let key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    for (rel, ticks) in [(SHARDS, "200"), (ARENA, "300")] {
        let Some(exe) = build(rel, "faults", &[]) else { return };
        let mut one_args = vec!["--ticks", ticks, "--key", key];
        one_args.extend_from_slice(&sim);
        let (_, one) = run(&exe, &one_args, 1);
        let mut coord = vec!["--ticks", ticks, "--key", key];
        coord.extend_from_slice(&sim);
        let mut worker = vec!["--key", key];
        worker.extend_from_slice(&sim);
        let (out, _) = start_shards(&exe, 3, &coord, &worker, 1).finish(Duration::from_secs(180));
        assert_eq!(comparable(&one), comparable(&out), "{} through faulty shard links differs from one process", rel);
        let ch = line(&out, "shard channel");
        assert!(number_after(&ch, "re-sent") > 0, "nothing was lost? {}", ch);
        assert!(out.contains("network simulation (shard links, loss 20%"), "{}", out);
    }
}

/// The coordinator is the clients' gateway: clients (one joins late) receive the world the shards
/// simulate, with a bandwidth budget, and end with the server's replicated state; Net.View works.
#[test]
fn clients_follow_a_sharded_world() {
    let Some(exe) = build(BOIDS, "gateway", &[]) else { return };
    let (_, one) = run(&exe, &["--ticks", "300"], 1);
    let gp = ports(1)[0].to_string();
    let addr = format!("127.0.0.1:{}", gp);
    let r = start_shards(&exe, 3, &["--server", gp.as_str(), "--ticks", "300", "--tick-rate", "120", "--wait-clients", "1", "--budget", "1500", "--quiet"], &[], 1);
    let mut c1 = Proc::spawn("client 1", &exe, &["--connect", addr.as_str(), "--quiet"], 1);
    std::thread::sleep(Duration::from_millis(1200));
    let mut c2 = Proc::spawn("client 2 (late)", &exe, &["--connect", addr.as_str(), "--quiet"], 1);
    let (coord, _) = r.finish(Duration::from_secs(120));
    for c in [&mut c1, &mut c2] {
        assert_eq!(c.finish(Duration::from_secs(30)), 0, "{}", c.output());
    }
    // no RPCs in boids: the served world is the one-process world
    assert_eq!(line(&one, "world checksum:"), line(&coord, "world checksum:"));
    let want = line(&coord, "replicated state hash:");
    assert_eq!(line(&one, "replicated state hash:"), want);
    assert!(coord.contains("convergence: 2 of 2"), "{}", coord);
    assert!(coord.contains("bandwidth budget: 1500 B"), "{}", coord);
    let want_hash = want.split_whitespace().nth(3).unwrap().to_string();
    for c in [&c1, &c2] {
        assert!(line(&c.output(), "replicated state hash:").contains(&want_hash), "{}\nserver: {}", c.output(), want);
    }
    // interest management through the gateway
    let Some(view) = build("tests/programs/view.sijo", "gateway-view", &[]) else { return };
    let gp = ports(1)[0].to_string();
    let addr = format!("127.0.0.1:{}", gp);
    let r = start_shards(&view, 2, &["--server", gp.as_str(), "--ticks", "240", "--tick-rate", "120", "--wait-clients", "1", "--quiet"], &[], 1);
    let mut c = Proc::spawn("view client", &view, &["--connect", addr.as_str(), "--quiet"], 1);
    let (coord, _) = r.finish(Duration::from_secs(120));
    assert_eq!(c.finish(Duration::from_secs(30)), 0, "{}", c.output());
    let mine = line(&coord, "replicated state hash for client 1 (its view):");
    let h = mine.split_whitespace().nth(8).unwrap().trim_end_matches(':').to_string();
    assert!(line(&c.output(), "replicated state hash:").contains(&h), "{}\n{}", c.output(), mine);
}

/// A shard dies (--die-at-tick, an abrupt exit) and is started again with --recover: it loads its
/// newest checkpoint, the other shards replay the messages they logged since, and the run ends
/// with the single-process results. Also with a SIGKILL in the middle of a paced run, through a
/// lossy network.
#[test]
fn a_killed_shard_recovers_from_its_checkpoint() {
    let Some(exe) = build(SHARDS, "recover", &[]) else { return };
    let (_, one) = run(&exe, &["--ticks", "300"], 1);
    let dir = std::env::temp_dir().join(format!("sijoc-shard-ck-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.display().to_string();
    let ck = ["--checkpoint-dir", d.as_str(), "--checkpoint-every", "40"];
    let p = peers(&ports(3));
    let mut w1a = vec!["--shard-of", "3", "--shard-id", "1", "--peers", p.as_str()];
    w1a.extend_from_slice(&ck);
    let mut w2a = vec!["--shard-of", "3", "--shard-id", "2", "--peers", p.as_str(), "--die-at-tick", "130"];
    w2a.extend_from_slice(&ck);
    let mut ca = vec!["--shard-of", "3", "--shard-id", "0", "--peers", p.as_str(), "--ticks", "300"];
    ca.extend_from_slice(&ck);
    let mut w1 = Proc::spawn("shard 1", &exe, &w1a, 1);
    let mut w2 = Proc::spawn("shard 2", &exe, &w2a, 1);
    let mut c = Proc::spawn("coordinator", &exe, &ca, 1);
    assert_eq!(w2.finish(Duration::from_secs(60)), 77, "{}", w2.output());
    let mut w2r_args = vec!["--shard-of", "3", "--shard-id", "2", "--peers", p.as_str(), "--recover"];
    w2r_args.extend_from_slice(&ck);
    let mut w2r = Proc::spawn("shard 2 (recovered)", &exe, &w2r_args, 1);
    assert_eq!(c.finish(Duration::from_secs(120)), 0, "{}", c.output());
    assert_eq!(w1.finish(Duration::from_secs(30)), 0, "{}", w1.output());
    assert_eq!(w2r.finish(Duration::from_secs(30)), 0, "{}", w2r.output());
    assert!(w2r.output().contains("recovering from the checkpoint of tick 120"), "{}", w2r.output());
    assert!(w1.output().contains("logged messages to recovering shard 2"), "{}", w1.output());
    assert_eq!(comparable(&one), comparable(&c.output()), "the recovered run differs from one process");

    // SIGKILL in the middle of a paced run (the coordinator ticks in real time), lossy links
    let sim = ["--sim-loss", "0.1", "--sim-dup", "0.05"];
    let (_, one) = run(&exe, &["--ticks", "300", "--sim-loss", "0.1", "--sim-dup", "0.05"], 1);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let p = peers(&ports(3));
    let args = |id: &str, extra: &[&str]| -> Vec<String> {
        let mut a: Vec<String> = ["--shard-of", "3", "--shard-id", id, "--peers", p.as_str()].iter().map(|s| s.to_string()).collect();
        a.extend(ck.iter().chain(sim.iter()).chain(extra.iter()).map(|s| s.to_string()));
        a
    };
    let (v1, v2, v0, v2r) = (args("1", &[]), args("2", &[]), args("0", &["--ticks", "300", "--realtime"]), args("2", &["--recover"]));
    fn r(v: &[String]) -> Vec<&str> {
        v.iter().map(|s| s.as_str()).collect()
    }
    let mut w1 = Proc::spawn("shard 1", &exe, &r(&v1), 1);
    let mut w2 = Proc::spawn("shard 2", &exe, &r(&v2), 1);
    let mut c = Proc::spawn("coordinator", &exe, &r(&v0), 1);
    std::thread::sleep(Duration::from_millis(2500)); // ~tick 150 of 300 at 60 Hz
    w2.child.kill().unwrap();
    let _ = w2.child.wait();
    std::thread::sleep(Duration::from_millis(300));
    let mut w2r = Proc::spawn("shard 2 (recovered)", &exe, &r(&v2r), 1);
    assert_eq!(c.finish(Duration::from_secs(120)), 0, "{}", c.output());
    assert_eq!(w1.finish(Duration::from_secs(30)), 0, "{}", w1.output());
    assert_eq!(w2r.finish(Duration::from_secs(30)), 0, "{}", w2r.output());
    assert!(w2r.output().contains("recovering from the checkpoint"), "{}", w2r.output());
    assert_eq!(comparable(&one), comparable(&c.output()), "the run with a killed shard differs from one process");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The shard runtime (codecs, merges, handoffs, the channel) is clean under AddressSanitizer,
/// UndefinedBehaviorSanitizer and LeakSanitizer, in one process and as shard processes.
#[test]
fn shards_are_memory_safe_under_sanitizers() {
    let flags = ["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"];
    let Some(exe) = build(SHARDS, "asan", &flags) else {
        eprintln!("skipping: sanitizers not available");
        return;
    };
    let (code, one) = run(&exe, &["--ticks", "150"], 1);
    assert_eq!(code, 0, "{}", one);
    let (code, out) = run(&exe, &["--ticks", "150", "--shards", "3"], 2);
    assert_eq!(code, 0, "under sanitizers:\n{}", out);
    assert_eq!(comparable(&one), comparable(&out));
    let (coord, workers) = start_shards(&exe, 3, &["--ticks", "150", "--sim-loss", "0.1"], &["--sim-loss", "0.1"], 1).finish(Duration::from_secs(180));
    assert!(!coord.contains("ERROR: ") && !coord.contains("runtime error"), "{}", coord);
    for w in &workers {
        assert!(!w.contains("ERROR: ") && !w.contains("runtime error"), "{}", w);
    }
    assert_eq!(line(&one, "world checksum:").len(), line(&coord, "world checksum:").len());
}

/// Programs and options that cannot meet are refused instead of running out of step.
#[test]
fn mismatched_shards_refuse_each_other() {
    let Some(a) = build(ARENA, "mismatch-a", &[]) else { return };
    let Some(b) = build(BOIDS, "mismatch-b", &[]) else { return };
    let p = peers(&ports(2));
    let mut w = Proc::spawn("boids shard 1", &b, &["--shard-of", "2", "--shard-id", "1", "--peers", p.as_str()], 1);
    let mut c = Proc::spawn("arena coordinator", &a, &["--shard-of", "2", "--shard-id", "0", "--peers", p.as_str(), "--ticks", "10"], 1);
    // both refuse: the one that notices first tells the other before it leaves
    let cc = c.finish(Duration::from_secs(60));
    let wc = w.finish(Duration::from_secs(60));
    assert!(cc == 6 && wc == 6, "a different program was accepted (exit {} / {}):\n{}\n{}", cc, wc, c.output(), w.output());
    assert!(c.output().contains("different program") && w.output().contains("different program"));
    let (code, out) = run(&a, &["--shard-of", "2", "--shard-id", "5", "--peers", p.as_str()], 1);
    assert_eq!(code, 2, "{}", out);
    let (code, out) = run(&a, &["--shard-of", "2", "--shard-id", "0", "--peers", p.as_str(), "--recover"], 1);
    assert_eq!(code, 2, "{}", out);
}
