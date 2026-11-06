//! Real networking, end to end: a `--server` process and several `--connect` client processes talk
//! over localhost UDP through a simulated bad network (loss, latency, jitter, duplication), and every
//! client must end with exactly the server's replicated state (same hash, keyed by server entity).
//! Also covers late joiners, protocol mismatch, client timeouts and the in-process lossy link.
//! Requires a C compiler (gcc or clang); tests are skipped if none is found.

use sijoc::diag::Diags;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

const LOSSY: &[&str] = &["--sim-loss", "0.2", "--sim-latency-ms", "30", "--sim-jitter-ms", "10", "--sim-dup", "0.02"];

fn cc() -> Option<String> {
    for c in [std::env::var("CC").unwrap_or_default(), "gcc".into(), "clang".into(), "cc".into()] {
        if !c.is_empty() && Command::new(&c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(c);
        }
    }
    None
}

fn example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("examples").join(name)
}

/// Compiles an example to a native executable (single-threaded, so several processes can share the CPU).
fn build(name: &str) -> Option<PathBuf> {
    let path = example(name);
    build_src(name, &std::fs::read_to_string(&path).unwrap(), &path)
}

fn build_src(name: &str, src: &str, path: &Path) -> Option<PathBuf> {
    build_flags(name, src, path, &[])
}

/// Like build_src, with extra C flags (OpenMP, sanitizers): None when the C compiler has no support
/// for them (or there is no C compiler).
fn build_flags(name: &str, src: &str, path: &Path, flags: &[&str]) -> Option<PathBuf> {
    let cc = cc()?;
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).unwrap_or_else(|| panic!("{} failed to compile: {:?}", name, d.list));
    // tests run in parallel and may build the same example: one directory per build
    static BUILDS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = BUILDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("sijoc-net-{}-{}-{}", std::process::id(), n, name.replace('.', "_")));
    std::fs::create_dir_all(&dir).unwrap();
    let cfile = dir.join("prog.c");
    let exe = dir.join("prog");
    std::fs::write(&cfile, sijoc::codegen::generate(&c, &path.display().to_string())).unwrap();
    let out = Command::new(&cc).args(["-std=gnu11", "-O2", "-ffp-contract=off", "-w"]).args(flags).arg(&cfile).arg("-o").arg(&exe).arg("-lm").output().unwrap();
    if !out.status.success() && !flags.is_empty() {
        return None; // optional toolchain feature missing
    }
    assert!(out.status.success(), "C compilation failed:\n{}", String::from_utf8_lossy(&out.stderr));
    Some(exe)
}

fn program(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs").join(name)
}

const SANITIZE: &[&str] = &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"];

/// In-process runs of a test program (server and client world in one process): lossless, over a
/// lossy duplicating link, and budgeted on a lossy link, each on 1 and 4 threads (when OpenMP is
/// there) and once more under ASan/UBSan/LSan. Every run must end with the client equal to the
/// server (the replication check compares every replicated value) and the thread count must not
/// change anything. Returns the lossless output.
fn in_process_runs(name: &str, ticks: &str) -> Option<String> {
    let path = program(name);
    let src = std::fs::read_to_string(&path).unwrap();
    let exe = build_flags(name, &src, &path, &["-fopenmp"]).or_else(|| build_src(name, &src, &path))?;
    let modes: [&[&str]; 3] = [&[], &["--sim-loss", "0.3", "--sim-latency-ms", "40", "--sim-jitter-ms", "30", "--sim-dup", "0.1"], &["--budget", "200", "--sim-loss", "0.2", "--sim-latency-ms", "30", "--sim-jitter-ms", "10"]];
    let run = |exe: &Path, mode: &[&str], threads: &str| -> String {
        let mut args = vec!["--ticks", ticks, "--quiet"];
        args.extend_from_slice(mode);
        let out = Command::new(exe).args(&args).env("OMP_NUM_THREADS", threads).output().unwrap();
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success(), "{} {:?} on {} threads failed:\n{}", name, mode, threads, text);
        assert!(text.lines().any(|l| l.starts_with("replication check: OK")), "{} {:?}:\n{}", name, mode, text);
        text
    };
    // what the run computed and sent (arena peaks and the like are per thread)
    let keep = |t: &str| {
        let keys = ["entities alive:", "network", "strings and lists:", "client", "replication check:", "replicated state hash", "world checksum:"];
        t.lines().filter(|l| keys.iter().any(|k| l.starts_with(k))).map(|l| l.to_string()).collect::<Vec<_>>()
    };
    let mut plain = String::new();
    for mode in modes {
        let one = run(&exe, mode, "1");
        assert_eq!(keep(&one), keep(&run(&exe, mode, "4")), "{} {:?}: the thread count changed the run", name, mode);
        if mode.is_empty() {
            plain = one;
        }
    }
    if let Some(asan) = build_flags(&format!("{}-asan", name), &src, &path, SANITIZE) {
        for mode in &modes[1..] {
            run(&asan, mode, "1");
        }
    }
    Some(plain)
}

/// A child process whose output lines are collected by background threads; killed when dropped
/// (so a failing assertion never leaves servers or clients running).
struct Proc {
    name: String,
    child: Child,
    rx: Receiver<String>,
    lines: Vec<String>,
}

impl Proc {
    fn spawn(name: &str, exe: &Path, args: &[&str]) -> Proc {
        let mut child = Command::new(exe)
            .args(args)
            .env("OMP_NUM_THREADS", "1")
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

    /// Waits until a line containing `needle` is printed.
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

    /// Waits for the process to exit (killing it after `timeout`) and returns its exit code.
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
        // the reader threads end at EOF; give them a moment to deliver the last lines
        let end = Instant::now() + Duration::from_secs(2);
        while let Ok(l) = self.rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            self.lines.push(l);
        }
    }

    /// collects the lines printed so far, without waiting
    fn poll(&mut self) {
        while let Ok(l) = self.rx.try_recv() {
            self.lines.push(l);
        }
    }

    fn line(&self, prefix: &str) -> String {
        self.lines.iter().find(|l| l.starts_with(prefix)).cloned().unwrap_or_else(|| panic!("{}: no '{}' line in:\n{}", self.name, prefix, self.output()))
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

fn start_server(exe: &Path, extra: &[&str]) -> (Proc, String) {
    let mut args = vec!["--server", "0"];
    args.extend_from_slice(extra);
    let mut s = Proc::spawn("server", exe, &args);
    let l = s.wait_for("listening on udp port", Duration::from_secs(20));
    let port = l.split("port ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().to_string();
    (s, format!("127.0.0.1:{}", port))
}

fn client(n: usize, exe: &Path, addr: &str, extra: &[&str]) -> Proc {
    let seed = (100 + n).to_string();
    let mut args = vec!["--connect", addr, "--quiet", "--sim-seed", seed.as_str()];
    args.extend_from_slice(extra);
    Proc::spawn(&format!("client {}", n), exe, &args)
}

/// "client 2 (127.0.0.1:1234) converged at tick 240: 307 snapshots (8 full, ...), 314 datagrams, ..."
fn number_before(line: &str, word: &str) -> u64 {
    let words: Vec<&str> = line.split_whitespace().collect();
    let i = words.iter().position(|w| w.trim_matches(|c: char| !c.is_alphanumeric()) == word).unwrap_or_else(|| panic!("no '{}' in {}", word, line));
    words[i - 1].trim_matches(|c: char| !c.is_ascii_digit()).parse().unwrap()
}

/// Server + 2 clients from the start + 1 late joiner, all through a lossy simulated network.
fn converge(name: &str, ticks: &str, late_join_ms: u64) -> (Proc, Vec<Proc>) {
    converge_exe(&build(name).unwrap(), ticks, late_join_ms)
}

fn converge_exe(exe: &Path, ticks: &str, late_join_ms: u64) -> (Proc, Vec<Proc>) {
    converge_with(exe, ticks, late_join_ms, &[])
}

fn converge_with(exe: &Path, ticks: &str, late_join_ms: u64, server_extra: &[&str]) -> (Proc, Vec<Proc>) {
    let exe = exe.to_path_buf();
    let mut sargs = vec!["--ticks", ticks, "--wait-clients", "2", "--quiet", "--sim-seed", "7"];
    sargs.extend_from_slice(LOSSY);
    sargs.extend_from_slice(server_extra);
    let (mut server, addr) = start_server(&exe, &sargs);
    let mut clients = vec![client(1, &exe, &addr, LOSSY), client(2, &exe, &addr, LOSSY)];
    std::thread::sleep(Duration::from_millis(late_join_ms));
    clients.push(client(3, &exe, &addr, LOSSY));
    let code = server.finish(Duration::from_secs(90));
    assert_eq!(code, 0, "server failed:\n{}", server.output());
    for c in clients.iter_mut() {
        let code = c.finish(Duration::from_secs(30));
        assert_eq!(code, 0, "{} failed:\n{}", c.name, c.output());
    }
    let want = server.line("replicated state hash:");
    assert!(server.line("convergence:").contains("3 of 3"), "{}", server.output());
    for c in &clients {
        assert_eq!(c.line("replicated state hash:"), want, "{} diverged from the server.\nserver:\n{}\n{}:\n{}", c.name, server.output(), c.name, c.output());
    }
    (server, clients)
}

#[test]
fn arena_clients_converge_over_lossy_udp_with_rpcs_and_a_late_joiner() {
    if cc().is_none() {
        eprintln!("skipping: no C compiler");
        return;
    }
    let (server, clients) = converge("arena.sijo", "240", 1500);
    // client input reached the server as reliable RPCs despite 20% loss each way
    let rpcs = server.line("client->server RPCs:");
    assert!(number_before(&rpcs, "requests") > 0, "{}", rpcs);
    let sent = |c: &Proc| -> u64 { c.line("sent:").split("RPCs: ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().parse().unwrap() };
    assert!(clients.iter().any(|c| sent(c) > 0), "no client sent RPCs");
    // the late joiner connected mid-game and still converged (it started from a full snapshot)
    let joined = clients[2].line("sijo client");
    let tick: i64 = joined.split("server tick ").nth(1).and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()).unwrap().parse().unwrap();
    assert!(tick > 0, "late joiner did not join mid-game: {}", joined);
    // the clients observed the simulated loss and still recovered
    for c in &clients {
        let rx = c.line("received:");
        let loss: f64 = rx.split("loss ").nth(1).unwrap().split('%').next().unwrap().parse().unwrap();
        assert!(loss > 5.0 && loss < 40.0, "{}: implausible loss: {}", c.name, rx);
    }
}

#[test]
fn boids_fragmented_snapshots_converge_over_lossy_udp() {
    if cc().is_none() {
        eprintln!("skipping: no C compiler");
        return;
    }
    let (server, clients) = converge("boids.sijo", "90", 700);
    // 1200 boids: every snapshot is several datagrams (fragmentation + reassembly)
    let per_client: Vec<&String> = server.lines.iter().filter(|l| l.trim_start().starts_with("client ") && l.contains("converged")).collect();
    assert_eq!(per_client.len(), 3, "{}", server.output());
    for l in per_client {
        assert!(number_before(l, "datagrams") >= 3 * number_before(l, "snapshots"), "snapshots were not fragmented: {}", l);
    }
    // on a lossy path the server adds a parity fragment, which rebuilds snapshots that lost one fragment
    let rebuilt: u64 = clients.iter().map(|c| number_before(&c.line("snapshots:"), "completed")).sum();
    assert!(rebuilt > 0, "no snapshot was completed by parity:\n{}", clients[0].output());
    // networking never changes what the server computes: same checksum as the in-process run
    let exe = build("boids.sijo").unwrap();
    let out = Command::new(&exe).args(["--ticks", "90", "--quiet"]).env("OMP_NUM_THREADS", "1").output().unwrap();
    let local = String::from_utf8_lossy(&out.stdout);
    let sum = local.lines().find(|l| l.starts_with("world checksum:")).unwrap();
    assert_eq!(server.line("world checksum:"), sum);
    let hash = local.lines().find(|l| l.starts_with("replicated state hash:")).unwrap();
    assert_eq!(server.line("replicated state hash:"), hash);
}

#[test]
fn in_process_lossy_link_converges_deterministically() {
    for (name, ticks) in [("arena.sijo", "300"), ("boids.sijo", "40")] {
        let Some(exe) = build(name) else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let args = ["--ticks", ticks, "--quiet", "--sim-loss", "0.3", "--sim-latency-ms", "40", "--sim-jitter-ms", "30", "--sim-dup", "0.1"];
        let run = || {
            let o = Command::new(&exe).args(args).env("OMP_NUM_THREADS", "1").output().unwrap();
            assert_eq!(o.status.code(), Some(0), "{}", String::from_utf8_lossy(&o.stdout));
            String::from_utf8_lossy(&o.stdout).to_string()
        };
        let a = run();
        assert!(a.contains("replication check: OK"), "{}", a);
        assert!(a.contains(" lost,"), "{}", a);
        // the simulated network is seeded: identical runs (minus wall-clock timing)
        assert_eq!(a, run());
    }
}

#[test]
fn rpcs_with_arguments_arrive_exactly_once_over_a_lossy_duplicating_link() {
    // value arguments of every kind (int, bool, struct holding a bool) through the reliable channel
    let src = "public struct Tuning { public bool Fast; public int Level; public Tuning(bool fast, int level) { Fast = fast; Level = level; } }
        public class Ship : Entity {
          public mut int in 0..100000 Hp = 50;
          public void Update(float dt) { }
          public client void Render() { Draw.Sprite(Hp, 0, 0); }
          public client void Input() { if (Time.Tick % 7 == 0) { Tune(3, true, new Tuning(true, 2)); } }
          public void Tune(int amount, bool up, Tuning t) { if (up && t.Fast) { Hp = Math.Min(Hp + amount + t.Level, 100000); } } }
        public static class Game { public static void Start() { Spawn(new Ship { }); Spawn(new Ship { }); } }";
    let dir = std::env::temp_dir();
    let Some(exe) = build_src("rpcargs", src, &dir.join("rpcargs.sijo")) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let bad: &[&str] = &["--sim-loss", "0.25", "--sim-latency-ms", "30", "--sim-jitter-ms", "20", "--sim-dup", "0.1"];
    let mut sargs = vec!["--ticks", "200", "--wait-clients", "2", "--quiet", "--sim-seed", "3"];
    sargs.extend_from_slice(bad);
    let (mut server, addr) = start_server(&exe, &sargs);
    let mut clients = vec![client(1, &exe, &addr, bad), client(2, &exe, &addr, bad)];
    assert_eq!(server.finish(Duration::from_secs(60)), 0, "{}", server.output());
    for c in clients.iter_mut() {
        assert_eq!(c.finish(Duration::from_secs(30)), 0, "{}", c.output());
        assert_eq!(c.line("replicated state hash:"), server.line("replicated state hash:"), "{}", c.output());
        // every RPC the client queued was executed exactly once, despite loss, re-sends and duplicates
        let id = c.line("== sijo client").split_whitespace().nth(3).unwrap().to_string();
        let sent: u64 = c.line("sent:").split("RPCs: ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().parse().unwrap();
        let resends: u64 = c.line("sent:").split("sent, ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().parse().unwrap();
        let mine = server.lines.iter().find(|l| l.trim_start().starts_with(&format!("client {} (", id))).unwrap_or_else(|| panic!("{}", server.output()));
        let got: u64 = mine.split("RPCs ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().parse().unwrap();
        assert!(sent > 0 && resends > 0, "{}", c.output());
        assert_eq!(got, sent, "client {}: sent {} RPCs, server received {}\n{}", id, sent, got, server.output());
        assert!(mine.contains(" 0 rejected"), "{}", mine);
    }
}

#[test]
fn mismatched_program_is_rejected_and_dead_clients_time_out() {
    let (Some(arena), Some(village)) = (build("arena.sijo"), build("village.sijo")) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (mut server, addr) = start_server(&arena, &["--ticks", "200", "--wait-clients", "2", "--timeout-ms", "500"]);
    // a client built from a different program has a different protocol hash: refused
    let mut wrong = client(9, &village, &addr, &[]);
    assert_eq!(wrong.finish(Duration::from_secs(20)), 6, "{}", wrong.output());
    assert!(wrong.output().contains("protocol mismatch"), "{}", wrong.output());
    // one client vanishes without a word (killed); the server times it out and carries on
    let mut doomed = client(1, &arena, &addr, &[]);
    let mut good = client(2, &arena, &addr, &[]);
    doomed.wait_for("connected to", Duration::from_secs(20));
    std::thread::sleep(Duration::from_millis(500));
    doomed.child.kill().unwrap();
    assert_eq!(server.finish(Duration::from_secs(60)), 0, "{}", server.output());
    assert_eq!(good.finish(Duration::from_secs(20)), 0, "{}", good.output());
    assert!(server.output().contains("timed out"), "{}", server.output());
    assert!(server.line("convergence:").contains("1 of 1"), "{}", server.output());
    assert_eq!(good.line("replicated state hash:"), server.line("replicated state hash:"));
}

#[test]
fn rpcs_carry_strings_exactly_once() {
    // string arguments travel as length + bytes (UTF-8, any length up to the RPC limit) and are
    // copied on the server; Hp depends on their content, so equal replicated hashes prove it
    let src = "public class Ship : Entity {
          public mut int in 0..100000 Hp = 50;
          public mut int Words;
          public void Update(float dt) { }
          public client void Render() { Draw.Sprite(Hp, 0, 0); }
          public client void Input() { if (Time.Tick % 5 == 0) { Say($\"héllo {Time.Tick} 안녕\", 2, \"!\"); } }
          public void Say(string text, int times, string tail) { Words += times; Hp = Math.Min(Hp + text.Length * times + tail.Length + (text.EndsWith(\"녕\") ? 1 : 0), 100000); } }
        public static class Game { public static void Start() { Spawn(new Ship { }); Spawn(new Ship { }); } }";
    let dir = std::env::temp_dir();
    let Some(exe) = build_src("rpcstr", src, &dir.join("rpcstr.sijo")) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let bad: &[&str] = &["--sim-loss", "0.2", "--sim-latency-ms", "20", "--sim-jitter-ms", "10", "--sim-dup", "0.1"];
    let mut sargs = vec!["--ticks", "150", "--wait-clients", "1", "--quiet", "--sim-seed", "5"];
    sargs.extend_from_slice(bad);
    let (mut server, addr) = start_server(&exe, &sargs);
    let mut c = client(1, &exe, &addr, bad);
    assert_eq!(server.finish(Duration::from_secs(60)), 0, "{}", server.output());
    assert_eq!(c.finish(Duration::from_secs(30)), 0, "{}", c.output());
    assert_eq!(c.line("replicated state hash:"), server.line("replicated state hash:"), "{}", c.output());
    let id = c.line("== sijo client").split_whitespace().nth(3).unwrap().to_string();
    let sent: u64 = c.line("sent:").split("RPCs: ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().parse().unwrap();
    let mine = server.lines.iter().find(|l| l.trim_start().starts_with(&format!("client {} (", id))).unwrap_or_else(|| panic!("{}", server.output()));
    let got: u64 = mine.split("RPCs ").nth(1).and_then(|r| r.split_whitespace().next()).unwrap().parse().unwrap();
    assert!(sent > 0, "{}", c.output());
    assert_eq!(got, sent, "{}", server.output());
    assert!(mine.contains(" 0 rejected"), "{}", mine);
}

#[test]
fn entity_references_replicate_and_map_to_client_handles() {
    // Ship.Target is read by client code: it travels as server slot + generation and every client
    // (one joining late) maps it to its own handle; targets keep dying and being replaced
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/targets.sijo");
    let Some(exe) = build_src("targets", &std::fs::read_to_string(&path).unwrap(), &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (server, _clients) = converge_exe(&exe, "200", 800);
    assert!(server.line("entities alive:").contains("destroyed"), "{}", server.output());
}

#[test]
fn string_fields_replicate_over_lossy_udp() {
    // client code reads string fields (also inside a struct): interned ids plus the text when it
    // changes; names change back and forth, some are empty or not ASCII, labels come and go, and a
    // client joins late (full state with every current string)
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/strrepl.sijo");
    let Some(exe) = build_src("strrepl", &std::fs::read_to_string(&path).unwrap(), &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (server, _clients) = converge_exe(&exe, "300", 800);
    assert!(server.line("entities alive:").contains("destroyed"), "{}", server.output());
    // the in-process run compares every replicated value, strings included
    let out = Command::new(&exe).args(["--ticks", "300", "--quiet", "--sim-loss", "0.3", "--sim-dup", "0.1"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", text);
    assert!(text.lines().any(|l| l.starts_with("replication check: OK")), "{}", text);
}

#[test]
fn lost_ticks_still_run_client_code() {
    // 30% of the snapshots are lost: the client reconstructs those ticks (floats interpolated
    // between the snapshots around them) and runs their client frames, so the input it sends every
    // 10 ticks is never skipped, and the run stays deterministic
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/lostticks.sijo");
    let Some(exe) = build_src("lostticks", &std::fs::read_to_string(&path).unwrap(), &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let run = || {
        let out = Command::new(&exe).args(["--ticks", "600", "--quiet", "--sim-loss", "0.3", "--sim-dup", "0.1"]).output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let text = run();
    let cl = text.lines().find(|l| l.starts_with("client:")).unwrap_or_else(|| panic!("{}", text));
    let applied = number_before(cl, "snapshots");
    let filled = number_before(cl, "lost");
    assert!(filled > 100, "{}", cl);
    assert_eq!(applied + filled, 600, "one client frame per server tick: {}", cl);
    let rpcs = text.lines().find(|l| l.starts_with("client->server RPCs:")).unwrap();
    // 60 inputs; only the one of the last tick may still be in flight when the run ends
    assert!(number_before(rpcs, "requests") >= 59, "{}", rpcs);
    assert_eq!(run(), text, "lossy in-process runs are deterministic");
}

#[test]
fn list_fields_replicate_over_lossy_udp() {
    // client code reads lists and arrays that change in place, hold strings, structs and lists:
    // their bytes travel when they change; every client (one joins late) ends equal to the server
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/listrepl.sijo");
    let Some(exe) = build_src("listrepl", &std::fs::read_to_string(&path).unwrap(), &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (server, _clients) = converge_exe(&exe, "250", 800);
    assert!(server.line("entities alive:").contains("destroyed"), "{}", server.output());
    let out = Command::new(&exe).args(["--ticks", "300", "--quiet", "--sim-loss", "0.3", "--sim-dup", "0.1"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", text);
    assert!(text.lines().any(|l| l.starts_with("replication check: OK")), "{}", text);
}

#[test]
fn forged_connect_does_not_replace_a_live_client() {
    // The test plays a client by hand (UDP socket, protocol bytes) and then sends a CONNECT with a
    // new nonce from the same address, as an attacker forging that address would. The server must
    // keep the live session until the new one proves it receives there (sends a CLIENT packet with
    // the session token of its ACCEPT).
    let path = example("boids.sijo");
    let src = std::fs::read_to_string(&path).unwrap();
    let Some(exe) = build_src("boids-forge", &src, &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let c = sijoc::driver::compile(&src, &mut Diags::default()).unwrap();
    let csrc = sijoc::codegen::generate(&c, "boids.sijo");
    let proto_hex = csrc.lines().find_map(|l| l.strip_prefix("#define SIJO_PROTO 0x")).unwrap().trim_end_matches('u').to_string();
    let proto = u32::from_str_radix(&proto_hex, 16).unwrap();
    let (mut server, addr) = start_server(&exe, &["--ticks", "100000", "--wait-clients", "1"]);
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    sock.connect(&addr).unwrap();
    let connect = |nonce: u32| {
        let mut p = vec![1u8];
        p.extend_from_slice(&nonce.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes());
        p.extend_from_slice(b"SIJO"); // magic
        p.extend_from_slice(&proto.to_le_bytes());
        sock.send(&p).unwrap();
    };
    let client_pkt = |session: u32, pseq: u16| {
        let mut p = vec![5u8];
        p.extend_from_slice(&session.to_le_bytes());
        p.extend_from_slice(&pseq.to_le_bytes());
        p.extend_from_slice(&0xffff_ffffu32.to_le_bytes()); // no snapshot acknowledged
        p.extend_from_slice(&[0, 0, 0]); // epoch, loss, no Net.View
        p.extend_from_slice(&1u32.to_le_bytes()); // first RPC number
        p.push(0); // no RPCs
        sock.send(&p).unwrap();
    };
    // packets of one kind for one session, received during `ms`, while acknowledging as `ack_as`
    let mut buf = [0u8; 2048];
    let mut pseq = 0u16;
    let pump = |ms: u64, ack_as: u32, buf: &mut [u8; 2048], pseq: &mut u16, want: (u8, u32)| -> usize {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut n = 0;
        while Instant::now() < end {
            client_pkt(ack_as, *pseq);
            *pseq = pseq.wrapping_add(1);
            if let Ok(len) = sock.recv(buf) {
                if len >= 5 && buf[0] == want.0 && u32::from_le_bytes(buf[1..5].try_into().unwrap()) == want.1 {
                    n += 1;
                }
            }
        }
        n
    };
    let accept = |nonce: u32, buf: &mut [u8; 2048]| -> u32 {
        for _ in 0..100 {
            connect(nonce);
            if let Ok(len) = sock.recv(buf) {
                if len >= 9 && buf[0] == 2 && u32::from_le_bytes(buf[7..11].try_into().unwrap()) == nonce {
                    return u32::from_le_bytes(buf[1..5].try_into().unwrap());
                }
            }
        }
        panic!("no ACCEPT for nonce {nonce:#x}");
    };
    let first = accept(0x1111_1111, &mut buf);
    assert!(pump(600, first, &mut buf, &mut pseq, (4, first)) > 0, "no snapshots for the first session");
    // forged CONNECT: accepted as a pending session, the live one keeps its snapshots
    let second = accept(0x2222_2222, &mut buf);
    assert_ne!(first, second);
    assert!(pump(600, first, &mut buf, &mut pseq, (4, first)) > 0, "the live session lost its snapshots to a forged CONNECT");
    server.poll();
    assert!(server.output().contains("client 2 connected"), "{}", server.output());
    assert!(!server.output().contains("reconnected"), "{}", server.output());
    // the new session confirms (it does receive at this address): it replaces the old one
    assert!(pump(800, second, &mut buf, &mut pseq, (4, second)) > 0, "the confirmed session gets no snapshots");
    server.wait_for("reconnected", Duration::from_secs(5));
}

#[test]
fn encrypted_clients_converge_and_others_are_ignored() {
    // --key: every datagram is sealed with XChaCha20-Poly1305; clients with the key converge over
    // a lossy link, a client with another key and a plaintext CONNECT get no answer
    let Some(exe) = build("arena.sijo") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let key = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let other = "ff0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let mut sargs = vec!["--ticks", "240", "--wait-clients", "2", "--quiet", "--sim-seed", "7", "--key", key];
    sargs.extend_from_slice(LOSSY);
    let (mut server, addr) = start_server(&exe, &sargs);
    let mut cargs = vec!["--key", key];
    cargs.extend_from_slice(LOSSY);
    let mut clients = vec![client(1, &exe, &addr, &cargs), client(2, &exe, &addr, &cargs)];
    // another key: its datagrams do not authenticate, so the server never answers
    let mut wrong = client(3, &exe, &addr, &["--key", other, "--timeout-ms", "1500"]);
    // a plaintext CONNECT is dropped too
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(400))).unwrap();
    sock.connect(&addr).unwrap();
    let mut p = vec![1u8, 1, 2, 3, 4, 0, 0];
    p.extend_from_slice(b"SIJO"); // magic
    p.extend_from_slice(&0u32.to_le_bytes());
    sock.send(&p).unwrap();
    let mut buf = [0u8; 2048];
    assert!(sock.recv(&mut buf).is_err(), "the server answered a plaintext CONNECT");
    let code = server.finish(Duration::from_secs(90));
    assert_eq!(code, 0, "server failed:\n{}", server.output());
    for c in clients.iter_mut() {
        let code = c.finish(Duration::from_secs(30));
        assert_eq!(code, 0, "{} failed:\n{}", c.name, c.output());
    }
    let want = server.line("replicated state hash:");
    for c in &clients {
        assert_eq!(c.line("replicated state hash:"), want, "{} diverged:\n{}", c.name, c.output());
    }
    let enc = server.line("encryption");
    assert!(number_before(&enc, "received") > 0, "the wrong-key client's datagrams were not counted: {}", enc);
    let code = wrong.finish(Duration::from_secs(30));
    assert_eq!(code, 3, "a client with another key connected:\n{}", wrong.output());
    assert!(wrong.output().contains("no answer"), "{}", wrong.output());
}

#[test]
fn crypto_matches_published_test_vectors() {
    // RFC 8439 2.8.2 (ChaCha20-Poly1305) and draft-irtf-cfrg-xchacha A.3.1 (XChaCha20-Poly1305),
    // plus the HChaCha20 vector (2.2.1); then a tampered tag must not open
    let Some(cc) = cc() else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let dir = std::env::temp_dir().join(format!("sijoc-crypto-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = r#"
#include <stdio.h>
#include "sijo_crypto.h"
static void hex(const char* s, uint8_t* o) { while (s[0] && s[1]) { unsigned v; sscanf(s, "%2x", &v); *o++ = (uint8_t)v; s += 2; } }
static void out(const uint8_t* d, size_t n) { for (size_t i = 0; i < n; i++) printf("%02x", d[i]); printf("\n"); }
int main(void) {
    const char* pt = "Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it.";
    size_t n = strlen(pt);
    uint8_t key[32], aad[12], n12[12], n24[24], d[256], tag[16], sub[32], hk[32], hn[16];
    for (int i = 0; i < 32; i++) key[i] = (uint8_t)(0x80 + i);
    hex("50515253c0c1c2c3c4c5c6c7", aad);
    hex("070000004041424344454647", n12);
    memcpy(d, pt, n); sijo_aead_seal(key, n12, aad, 12, d, n, tag); out(d, n); out(tag, 16);
    for (int i = 0; i < 24; i++) n24[i] = (uint8_t)(0x40 + i);
    memcpy(d, pt, n); sijo_xaead_seal(key, n24, aad, 12, d, n, tag); out(d, n); out(tag, 16);
    for (int i = 0; i < 32; i++) hk[i] = (uint8_t)i;
    hex("000000090000004a0000000031415927", hn); sijo_hchacha(hk, hn, sub); out(sub, 32);
    int ok = sijo_xaead_open(key, n24, aad, 12, d, n, tag) && memcmp(d, pt, n) == 0;
    sijo_xaead_seal(key, n24, aad, 12, d, n, tag); tag[15] ^= 0x80;
    printf("%d %d\n", ok, sijo_xaead_open(key, n24, aad, 12, d, n, tag));
    return 0;
}
"#;
    std::fs::write(dir.join("t.c"), src).unwrap();
    let rt = Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime");
    let exe = dir.join("t");
    let o = Command::new(&cc).args(["-std=gnu11", "-O2", "-Wall", "-Wextra", "-Werror", "-I"]).arg(&rt).arg(dir.join("t.c")).arg("-o").arg(&exe).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let r = Command::new(&exe).output().unwrap();
    let lines: Vec<String> = String::from_utf8_lossy(&r.stdout).lines().map(|l| l.to_string()).collect();
    assert_eq!(lines[0], "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d63dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b3692ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc3ff4def08e4b7a9de576d26586cec64b6116");
    assert_eq!(lines[1], "1ae10b594f09e26a7e902ecbd0600691");
    assert_eq!(lines[2], "bd6d179d3e83d43b9576579493c0e939572a1700252bfaccbed2902c21396cbb731c7f1b0b4aa6440bf3a82f4eda7e39ae64c6708c54c216cb96b72e1213b4522f8c9ba40db5d945b11b69b982c1bb9e3f3fac2bc369488f76b2383565d3fff921f9664c97637da9768812f615c68b13b52e");
    assert_eq!(lines[3], "c0875924c1c7987947deafd8780acf49");
    assert_eq!(lines[4], "82413b4227b27bfed30e42508a877d73a0f9e4d58a74a853c12ec41326d3ecdc");
    assert_eq!(lines[5], "1 0", "open of a sealed message / of a tampered tag");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clients_with_a_view_get_only_what_they_see() {
    // Net.View: each client is sent the movers within its view circle (plus entities without a
    // position); over a lossy link, with a late joiner, every client must end equal to the server's
    // state restricted to that client's view, and far less is sent than without views
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/view.sijo");
    let Some(exe) = build_src("view", &std::fs::read_to_string(&path).unwrap(), &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sargs = vec!["--ticks", "300", "--wait-clients", "2", "--quiet", "--sim-seed", "7"];
    sargs.extend_from_slice(LOSSY);
    let (mut server, addr) = start_server(&exe, &sargs);
    let mut clients = vec![client(1, &exe, &addr, LOSSY), client(2, &exe, &addr, LOSSY)];
    std::thread::sleep(Duration::from_millis(800));
    clients.push(client(3, &exe, &addr, LOSSY));
    let code = server.finish(Duration::from_secs(90));
    assert_eq!(code, 0, "server failed:\n{}", server.output());
    assert!(server.line("convergence:").contains("3 of 3"), "{}", server.output());
    let global = server.line("replicated state hash:");
    for c in clients.iter_mut() {
        let code = c.finish(Duration::from_secs(30));
        assert_eq!(code, 0, "{} failed:\n{}", c.name, c.output());
        let id = c.line("== sijo client").split_whitespace().nth(3).unwrap().to_string();
        let want = server.line(&format!("replicated state hash for client {} (its view):", id));
        let got = c.line("replicated state hash:");
        let hash = |l: &str| l.split_whitespace().find(|w| w.len() == 16 && w.chars().all(|c| c.is_ascii_hexdigit())).unwrap().to_string();
        assert_eq!(hash(&got), hash(&want), "{} diverged from its view:\nserver:\n{}\n{}:\n{}", c.name, server.output(), c.name, c.output());
        assert_ne!(hash(&got), hash(&global), "{} was sent everything", c.name);
        assert!(number_before(&want, "entity") > 1000, "{}", want);
    }
}

#[test]
fn a_view_set_while_waiting_for_clients_converges() {
    // The server re-sends the start state while it waits for the second client. The first client
    // acknowledges it in full, then its Net.View arrives: re-sends of that tick must still count
    // as full, or the entities outside its view are never removed from that client
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/view.sijo");
    let Some(exe) = build_src("view-wait", &std::fs::read_to_string(&path).unwrap(), &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sargs = vec!["--ticks", "120", "--wait-clients", "2", "--quiet", "--sim-seed", "7"];
    sargs.extend_from_slice(LOSSY);
    let (mut server, addr) = start_server(&exe, &sargs);
    let mut clients = vec![client(1, &exe, &addr, LOSSY)];
    std::thread::sleep(Duration::from_millis(600));
    clients.push(client(2, &exe, &addr, LOSSY));
    let code = server.finish(Duration::from_secs(90));
    assert_eq!(code, 0, "server failed:\n{}", server.output());
    for c in clients.iter_mut() {
        let code = c.finish(Duration::from_secs(30));
        assert_eq!(code, 0, "{} failed:\n{}", c.name, c.output());
        let id = c.line("== sijo client").split_whitespace().nth(3).unwrap().to_string();
        let want = server.line(&format!("replicated state hash for client {} (its view):", id));
        let got = c.line("replicated state hash:");
        let hash = |l: &str| l.split_whitespace().find(|w| w.len() == 16 && w.chars().all(|c| c.is_ascii_hexdigit())).unwrap().to_string();
        assert_eq!(hash(&got), hash(&want), "{} diverged from its view:\nserver:\n{}\n{}:\n{}", c.name, server.output(), c.name, c.output());
    }
}

#[test]
fn predicted_rpcs_show_at_once_and_reconcile() {
    // Boost() is predictable: the client runs it at once, so each client frame sees every boost it
    // asked for although the server confirms it a round trip later; snapshots restore the server's
    // values (the server also changes Energy) and re-run what is still unconfirmed; the run ends
    // equal to the server, in-process (latency, jitter, loss) and over UDP
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/predict.sijo");
    let Some(exe) = build_src("predict", &std::fs::read_to_string(&path).unwrap(), &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let run = |extra: &[&str]| {
        let mut args = vec!["--ticks", "200", "--sim-latency-ms", "80", "--sim-jitter-ms", "20", "--sim-loss", "0.2"];
        args.extend_from_slice(extra);
        let out = Command::new(&exe).args(&args).output().unwrap();
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    // "[client] t<tick> sees <boosts> of <asked>"
    let seen = |text: &str| -> Vec<(u64, u64)> {
        text.lines()
            .filter(|l| l.starts_with("[client] t") && l.contains(" sees "))
            .map(|l| {
                let w: Vec<&str> = l.split_whitespace().collect();
                (w[3].parse().unwrap(), w[5].parse().unwrap())
            })
            .collect()
    };
    let with = run(&[]);
    let s = seen(&with);
    assert!(s.len() > 20, "{}", with);
    assert!(s.iter().all(|(b, k)| b == k), "with prediction every boost shows at once: {:?}", s);
    assert!(with.lines().any(|l| l.starts_with("replication check: OK")), "{}", with);
    assert!(number_before(with.lines().find(|l| l.starts_with("client prediction:")).unwrap(), "re-runs") > 0);
    let without = run(&["--no-predict"]);
    assert!(seen(&without).iter().any(|(b, k)| b < k), "without prediction the client should lag");
    assert!(without.lines().any(|l| l.starts_with("replication check: OK")), "{}", without);
    // over UDP: server + 2 clients (one late) on a lossy link converge
    let (_server, _clients) = converge_exe(&exe, "240", 600);
}

#[test]
fn a_prediction_the_server_never_runs_is_dropped_at_the_end() {
    // The game ends on a tick whose client frame asks for a Boost: the server acknowledges that
    // RPC with the final ack and says goodbye without running it, so the client must drop its
    // prediction when the game is over (it kept it before: the client ended one boost ahead)
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/predict.sijo");
    let Some(exe) = build_src("predict-end", &std::fs::read_to_string(&path).unwrap(), &path) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for ticks in ["119", "126"] {
        let (mut server, addr) = start_server(&exe, &["--ticks", ticks, "--wait-clients", "1", "--quiet"]);
        let mut c = client(1, &exe, &addr, &[]);
        let code = server.finish(Duration::from_secs(60));
        assert_eq!(code, 0, "server failed:\n{}", server.output());
        let code = c.finish(Duration::from_secs(30));
        assert_eq!(code, 0, "client failed:\n{}", c.output());
        assert_eq!(c.line("replicated state hash:"), server.line("replicated state hash:"), "--ticks {}: the client kept a prediction:\nserver:\n{}\nclient:\n{}", ticks, server.output(), c.output());
    }
}

#[test]
fn budgeted_snapshots_stay_small_and_still_converge() {
    // --budget: each snapshot carries about that many bytes; the changes that do not fit wait, the
    // longest-waiting first. In-process and over lossy UDP with a late joiner (whose full state is
    // budgeted too), every delta stays near the budget, far less is sent, and every client still
    // ends with exactly the server's state (the last tick goes in full)
    let Some(exe) = build("boids.sijo") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let run = |extra: &[&str]| {
        let mut args = vec!["--ticks", "200", "--quiet"];
        args.extend_from_slice(extra);
        let out = Command::new(&exe).args(&args).env("OMP_NUM_THREADS", "1").output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(out.status.success(), "{}", text);
        text
    };
    let line = |text: &str, prefix: &str| text.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no '{}' in:\n{}", prefix, text)).to_string();
    let per_tick = |l: &str| -> f64 { l.split('(').nth(1).unwrap().split(" B/tick").next().unwrap().parse().unwrap() };
    let full = run(&[]);
    let budgeted = run(&["--budget", "800"]);
    assert!(line(&budgeted, "replication check:").contains("OK"), "{}", budgeted);
    assert_eq!(line(&budgeted, "replicated state hash:"), line(&full, "replicated state hash:"));
    assert_eq!(line(&budgeted, "world checksum:"), line(&full, "world checksum:"), "the budget changed the simulation");
    let b = line(&budgeted, "bandwidth budget:");
    let largest: u64 = b.split("ran: ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
    assert!(largest <= 840, "a delta went far over the budget: {}", b);
    assert!(number_before(&b, "entity") > 1000, "nothing waited: {}", b);
    let (f, g) = (per_tick(&line(&full, "network:")), per_tick(&line(&budgeted, "network:")));
    assert!(g * 3.0 < f, "budgeted {} B/tick vs {} B/tick", g, f);
    // lossy in-process link with a view (interest management and the budget together)
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/view.sijo");
    let vexe = build_src("view", &std::fs::read_to_string(&path).unwrap(), &path).unwrap();
    let out = Command::new(&vexe).args(["--ticks", "300", "--quiet", "--budget", "120", "--sim-loss", "0.2", "--sim-latency-ms", "30", "--sim-jitter-ms", "10"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success() && line(&text, "replication check:").contains("OK"), "{}", text);
    assert!(number_before(&line(&text, "bandwidth budget:"), "entity") > 0, "{}", text);
    // over UDP: the server budgets every client separately
    let (server, _clients) = converge_with(&exe, "150", 700, &["--budget", "1000"]);
    let per_client: Vec<&String> = server.lines.iter().filter(|l| l.trim_start().starts_with("budget 1000 B:")).collect();
    assert_eq!(per_client.len(), 3, "{}", server.output());
    for l in per_client {
        let largest: u64 = l.split("largest delta ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert!(largest <= 1050, "{}", l);
    }
}

#[test]
fn collections_of_references_replicate_and_map_to_client_handles() {
    // Lists, arrays, structs in lists, nested lists and dictionaries hold entity references that
    // client code follows. Each element travels as server slot + generation and every client maps
    // it to its own handle (null once the target died, dictionary entries of dead keys left out);
    // ships keep dying and their slots are reused, and a predicted RPC edits a list of references
    let Some(plain) = in_process_runs("reflists.sijo", "300") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    assert!(plain.lines().any(|l| l.starts_with("entities alive:") && l.contains("destroyed")), "{}", plain);
    assert!(plain.lines().any(|l| l.starts_with("client prediction:")), "{}", plain);
    // over UDP: server + 2 clients + a late joiner on a lossy link
    let path = program("reflists.sijo");
    let exe = build_src("reflists", &std::fs::read_to_string(&path).unwrap(), &path).unwrap();
    let (server, _clients) = converge_exe(&exe, "240", 800);
    assert!(number_before(&server.line("client->server RPCs:"), "requests") > 0, "{}", server.output());
}

#[test]
fn collections_of_references_follow_the_view() {
    // Net.View: a reference to a mover the client does not see reads as null there and a
    // dictionary entry keyed by it is not there; both come back when the mover is seen again. Every
    // client (one joins late) ends equal to the server's state restricted to its view
    let Some(_) = in_process_runs("refview.sijo", "300") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let path = program("refview.sijo");
    let exe = build_src("refview", &std::fs::read_to_string(&path).unwrap(), &path).unwrap();
    let mut sargs = vec!["--ticks", "300", "--wait-clients", "2", "--quiet", "--sim-seed", "7"];
    sargs.extend_from_slice(LOSSY);
    let (mut server, addr) = start_server(&exe, &sargs);
    let mut clients = vec![client(1, &exe, &addr, LOSSY), client(2, &exe, &addr, LOSSY)];
    std::thread::sleep(Duration::from_millis(800));
    clients.push(client(3, &exe, &addr, LOSSY));
    assert_eq!(server.finish(Duration::from_secs(90)), 0, "server failed:\n{}", server.output());
    assert!(server.line("convergence:").contains("3 of 3"), "{}", server.output());
    let global = server.line("replicated state hash:");
    let hash = |l: &str| l.split_whitespace().find(|w| w.len() == 16 && w.chars().all(|c| c.is_ascii_hexdigit())).unwrap().to_string();
    for c in clients.iter_mut() {
        assert_eq!(c.finish(Duration::from_secs(30)), 0, "{} failed:\n{}", c.name, c.output());
        let id = c.line("== sijo client").split_whitespace().nth(3).unwrap().to_string();
        let want = server.line(&format!("replicated state hash for client {} (its view):", id));
        let got = c.line("replicated state hash:");
        assert_eq!(hash(&got), hash(&want), "{} diverged from its view:\nserver:\n{}\n{}:\n{}", c.name, server.output(), c.name, c.output());
        assert_ne!(hash(&got), hash(&global), "{} was sent everything", c.name);
    }
}

#[test]
fn large_lists_travel_as_element_deltas() {
    // Lists of 200-512 elements (and a long string, and a list of 240 references) change a little
    // every tick: a few cells written, a sliding window, a growing log that is cut back, now and
    // then a sort. Each change goes as a delta against what the client acknowledged when that is
    // smaller, so the bandwidth drops by an order of magnitude; the state the client ends with is
    // the same bit for bit
    let Some(plain) = in_process_runs("biglists.sijo", "300") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let path = program("biglists.sijo");
    let exe = build_src("biglists", &std::fs::read_to_string(&path).unwrap(), &path).unwrap();
    let line = |text: &str, prefix: &str| text.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no '{}' in:\n{}", prefix, text)).to_string();
    let per_tick = |l: &str| -> f64 { l.split('(').nth(1).unwrap().split(" B/tick").next().unwrap().parse().unwrap() };
    let run = |extra: &[&str]| {
        let mut args = vec!["--ticks", "300", "--quiet"];
        args.extend_from_slice(extra);
        let out = Command::new(&exe).args(&args).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(out.status.success() && line(&text, "replication check:").contains("OK"), "{}", text);
        text
    };
    let lossy: &[&str] = &["--sim-loss", "0.2", "--sim-latency-ms", "30", "--sim-jitter-ms", "10"];
    let whole = run(&["--no-list-deltas"]);
    let mut lossy_whole = vec!["--no-list-deltas"];
    lossy_whole.extend_from_slice(lossy);
    let (lossy_d, lossy_w) = (run(lossy), run(&lossy_whole));
    for (d, w) in [(&plain, &whole), (&lossy_d, &lossy_w)] {
        assert_eq!(line(d, "replicated state hash:"), line(w, "replicated state hash:"));
        assert_eq!(line(d, "world checksum:"), line(w, "world checksum:"));
        let (bd, bw) = (per_tick(&line(d, "network:")), per_tick(&line(w, "network:")));
        eprintln!("biglists: {} B/tick with element deltas, {} B/tick sent whole", bd, bw);
        assert!(bd * 8.0 < bw, "element deltas: {} B/tick, whole lists: {} B/tick", bd, bw);
    }
    // most changed lists went as deltas, and they were far smaller than the lists
    let st = line(&plain, "strings and lists:");
    assert!(number_before(&st, "as") > 10 * number_before(&st, "sent"), "{}", st);
    let took = number_before(&st, "bytes");
    let would: u64 = st.split("instead of ").nth(1).unwrap().split(')').next().unwrap().parse().unwrap();
    assert!(took * 20 < would, "{}", st);
    // over UDP: lossy link and a late joiner (full state, then deltas), compared with whole lists
    // (bytes each client received per tick); then with a budget
    let received = |c: &Proc| -> f64 { c.line("received:").split(" = ").nth(1).unwrap().split(" B/tick").next().unwrap().parse().unwrap() };
    let (server, clients) = converge_exe(&exe, "200", 700);
    let (wserver, wclients) = converge_with(&exe, "200", 700, &["--no-list-deltas"]);
    assert_eq!(server.line("replicated state hash:"), wserver.line("replicated state hash:"));
    let d: f64 = clients.iter().map(received).sum();
    let w: f64 = wclients.iter().map(received).sum();
    eprintln!("biglists over UDP: {} B/tick per client with element deltas, {} sent whole", d / 3.0, w / 3.0);
    assert!(d * 5.0 < w, "over UDP: {} B/tick with element deltas, {} sent whole", d, w);
    let (bserver, _) = converge_with(&exe, "150", 700, &["--budget", "1500"]);
    assert!(bserver.line("bandwidth budget:").contains("waited"), "{}", bserver.output());
}

#[test]
fn dictionaries_replicate_with_their_order() {
    // Dictionaries keyed by enums, strings and entity references, with struct values holding
    // strings: the pairs travel in the dictionary's own order and the client rebuilds the index
    // (lookups with TryGetValue / GetValueOrDefault in client code); heroes come and go
    let Some(plain) = in_process_runs("dictrepl.sijo", "300") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    assert!(plain.lines().any(|l| l.starts_with("entities alive:") && l.contains("destroyed")), "{}", plain);
    let path = program("dictrepl.sijo");
    let exe = build_src("dictrepl", &std::fs::read_to_string(&path).unwrap(), &path).unwrap();
    let (_server, _clients) = converge_exe(&exe, "200", 700);
}
