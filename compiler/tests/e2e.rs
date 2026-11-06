//! End-to-end tests: Sijo source -> C -> native binary -> run.
//! Requires a C compiler (gcc or clang); tests are skipped if none is found.

use sijoc::diag::Diags;
use std::path::{Path, PathBuf};
use std::process::Command;

fn cc() -> Option<String> {
    for c in [std::env::var("CC").unwrap_or_default(), "gcc".into(), "clang".into(), "cc".into()] {
        if c.is_empty() {
            continue;
        }
        if Command::new(&c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(c);
        }
    }
    None
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sijoc-e2e-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Compile a .sijo file to C and then to an executable with the given extra C flags.
fn build(src_path: &Path, tag: &str, extra: &[&str]) -> Option<PathBuf> {
    let cc = cc()?;
    let src = std::fs::read_to_string(src_path).unwrap();
    let mut d = Diags::default();
    let compiled = sijoc::driver::compile(&src, &mut d);
    let c = compiled.unwrap_or_else(|| panic!("{} failed to compile: {:?}", src_path.display(), d.list));
    let csrc = sijoc::codegen::generate(&c, &src_path.display().to_string());
    let dir = out_dir(tag);
    let cfile = dir.join("prog.c");
    let exe = dir.join("prog");
    std::fs::write(&cfile, csrc).unwrap();
    let mut args: Vec<String> = vec!["-std=gnu11".into(), "-O2".into(), "-ffp-contract=off".into(), "-w".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    args.push(cfile.display().to_string());
    args.push("-o".into());
    args.push(exe.display().to_string());
    args.push("-lm".into());
    let out = Command::new(&cc).args(&args).output().unwrap();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if extra.iter().any(|f| f.starts_with("-fsanitize") || *f == "-fopenmp") {
            return None; // optional toolchain feature missing
        }
        panic!("C compilation failed for {}:\n{}", src_path.display(), err);
    }
    Some(exe)
}

fn run(exe: &Path, args: &[&str], threads: usize) -> (i32, String) {
    let out = Command::new(exe).args(args).env("OMP_NUM_THREADS", threads.to_string()).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (out.status.code().unwrap_or(-1), format!("{}{}", stdout, stderr))
}

fn line<'a>(out: &'a str, prefix: &str) -> &'a str {
    out.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no '{}' line in output:\n{}", prefix, out))
}

fn example(name: &str) -> PathBuf {
    root().join("..").join("examples").join(name)
}

fn check_example(name: &str, ticks: &str) {
    let path = example(name);
    let Some(exe) = build(&path, &format!("{}-omp", name), &["-fopenmp"]).or_else(|| build(&path, name, &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sums = vec![];
    for (threads, shards) in [(1, "1"), (4, "1"), (4, "3")] {
        let (code, out) = run(&exe, &["--ticks", ticks, "--quiet", "--shards", shards], threads);
        assert_eq!(code, 0, "{} exited with {}:\n{}", name, code, out);
        assert!(line(&out, "replication check:").contains("OK"), "{}", out);
        sums.push(line(&out, "world checksum:").to_string());
    }
    assert!(sums.windows(2).all(|w| w[0] == w[1]), "{}: nondeterministic results {:?}", name, sums);
}

#[test]
fn arena_runs_deterministically() {
    check_example("arena.sijo", "400");
}

#[test]
fn arena_client_input_reaches_the_server_as_rpc() {
    let path = example("arena.sijo");
    let Some(exe) = build(&path, "arena-rpc", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "300", "--quiet"], 1);
    assert_eq!(code, 0, "{}", out);
    let l = line(&out, "client->server RPCs:");
    let n: u64 = l.split_whitespace().nth(2).unwrap().parse().unwrap();
    assert!(n > 0, "{}", l);
}

#[test]
fn boids_runs_deterministically() {
    check_example("boids.sijo", "60");
}

#[test]
fn village_runs_deterministically() {
    check_example("village.sijo", "600");
}

#[test]
fn semantics_match_expected_output() {
    let path = root().join("tests/programs/semantics.sijo");
    let Some(exe) = build(&path, "semantics", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "1", "--no-summary"], 1);
    assert_eq!(code, 0, "{}", out);
    // compile-time evaluation must agree with runtime evaluation
    let mut other = vec![];
    for l in out.lines() {
        if let Some(rest) = l.strip_prefix("pair ") {
            let parts: Vec<&str> = rest.split_whitespace().collect();
            assert_eq!(parts.len(), 3, "bad pair line {}", l);
            assert_eq!(parts[1], parts[2], "CTFE and runtime disagree for '{}'", parts[0]);
        } else {
            other.push(l);
        }
    }
    let expected = std::fs::read_to_string(root().join("tests/programs/semantics.expected")).unwrap();
    assert_eq!(other.join("\n").trim(), expected.trim());
}

#[test]
fn generated_code_is_memory_safe_under_sanitizers() {
    for (file, ticks) in [("arena.sijo", "300"), ("village.sijo", "400"), ("boids.sijo", "5")] {
        let path = example(file);
        let flags = ["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"];
        let Some(exe) = build(&path, &format!("{}-asan", file), &flags) else {
            eprintln!("skipping: sanitizers not available");
            return;
        };
        let (code, out) = run(&exe, &["--ticks", ticks, "--quiet"], 1);
        assert_eq!(code, 0, "{} failed under sanitizers:\n{}", file, out);
    }
    let path = root().join("tests/programs/semantics.sijo");
    if let Some(exe) = build(&path, "semantics-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&exe, &["--ticks", "2", "--no-summary"], 1);
        assert_eq!(code, 0, "semantics failed under sanitizers:\n{}", out);
    }
}

#[test]
fn explain_reports_key_decisions() {
    let src = std::fs::read_to_string(example("arena.sijo")).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).expect("compiles");
    let report = sijoc::explain::report(&c, &sijoc::diag::SourceMap::single("arena.sijo", &src));
    for needle in ["hazard: Ship.Pos", "deferred ADD", "Frame arena", "13 bits", "dead fields eliminated", "Rules.DamageFor"] {
        assert!(report.contains(needle), "report is missing '{}':\n{}", needle, report);
    }
}

#[test]
fn regressions_from_adversarial_testing() {
    let path = root().join("tests/programs/regress.sijo");
    for (tag, flags) in [("regress", vec![]), ("regress-asan", vec!["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"])] {
        let Some(exe) = build(&path, tag, &flags) else {
            eprintln!("skipping {}", tag);
            continue;
        };
        for threads in [1, 4] {
            let (code, out) = run(&exe, &["--ticks", "3"], threads);
            assert_eq!(code, 0, "{}", out);
            let checks: Vec<&str> = out.lines().filter(|l| l.starts_with("check ")).collect();
            assert!(checks.len() >= 9, "{}", out);
            for c in checks {
                let parts: Vec<&str> = c.split_whitespace().collect();
                assert_eq!(parts[2], parts[3], "regression '{}' failed: {}", parts[1], c);
            }
            assert!(line(&out, "replication check:").contains("OK"), "{}", out);
        }
    }
}

#[test]
fn hangul_identifiers_compile_and_run() {
    let dir = out_dir("hangul-src");
    let path = dir.join("hangul.sijo");
    std::fs::write(
        &path,
        "public struct 벡터 { public float X; public float Y; }\n\
         public class 배 : Entity { public mut 벡터 위치; public mut int 체력 = 3;\n\
           public void Update(float dt) { 체력 -= 1; 위치.X += 1; if (체력 == 0) { Print($\"배 침몰: {위치.X} \\\"끝\\\" 100%\"); Destroy(); } }\n\
           public client void Render() { Draw.Sprite(체력, 위치.X, 위치.Y); } }\n\
         public static class Game { public static void Start() { Spawn(new 배 { }); } }\n",
    )
    .unwrap();
    let Some(exe) = build(&path, "hangul", &[]) else { return };
    let (code, out) = run(&exe, &["--ticks", "5"], 1);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("배 침몰: 3 \"끝\" 100%"), "{}", out);
    assert!(out.contains("배=0"), "{}", out);
}

/// Strings: expected output, memory safety under ASan/UBSan, determinism across threads, and
/// an exact save/restore round trip (string state is written by value and re-created on load).
#[test]
fn strings_work_and_survive_ticks() {
    let path = root().join("tests/programs/strings.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/strings.expected")).unwrap();
    let Some(exe) = build(&path, "strings-omp", &["-fopenmp"]).or_else(|| build(&path, "strings", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "300"], 4);
    assert_eq!(code, 0, "{}", out);
    let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
    assert_eq!(printed.join("\n").trim(), expected.trim());
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, one) = run(&exe, &["--ticks", "300", "--quiet"], 1);
    assert_eq!(sum(&one), sum(&out), "thread count changed the result");
    assert!(line(&out, "strings:").contains("freed"), "the collector should free strings that are no longer referenced");
    // save at tick 150, restore, run 150 more: same as 300 straight
    let dir = out_dir("strings-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "150", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "150", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&out));
    if let Some(asan) = build(&path, "strings-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "200", "--quiet"], 1);
        assert_eq!(code, 0, "strings failed under sanitizers:\n{}", out);
    }
}

/// Dictionaries: expected output, ASan/UBSan, determinism across threads, exact save/restore.
#[test]
fn dictionaries_work() {
    let path = root().join("tests/programs/dictionaries.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/dictionaries.expected")).unwrap();
    let Some(exe) = build(&path, "dict-omp", &["-fopenmp"]).or_else(|| build(&path, "dict", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "21"], 4);
    assert_eq!(code, 0, "{}", out);
    let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
    assert_eq!(printed.join("\n").trim(), expected.trim());
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, a) = run(&exe, &["--ticks", "300", "--quiet"], 4);
    let (_, b) = run(&exe, &["--ticks", "300", "--quiet"], 1);
    assert_eq!(sum(&a), sum(&b), "thread count changed the result");
    let dir = out_dir("dict-state");
    let st = dir.join("state.txt").display().to_string();
    run(&exe, &["--ticks", "150", "--quiet", "--save-state", &st], 1);
    let (code, resumed) = run(&exe, &["--ticks", "150", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&a), "save/restore changed the result");
    if let Some(asan) = build(&path, "dict-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "300", "--quiet"], 1);
        assert_eq!(code, 0, "dictionaries failed under sanitizers:\n{}", out);
    }
}

/// Coroutines: expected timeline, determinism, and a save/restore in the middle of running coroutines.
#[test]
fn coroutines_resume_across_ticks() {
    let path = root().join("tests/programs/coroutines.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/coroutines.expected")).unwrap();
    let Some(exe) = build(&path, "co-omp", &["-fopenmp"]).or_else(|| build(&path, "co", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "130"], 4);
    assert_eq!(code, 0, "{}", out);
    let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
    assert_eq!(printed.join("\n").trim(), expected.trim());
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, a) = run(&exe, &["--ticks", "200", "--quiet"], 4);
    let (_, b) = run(&exe, &["--ticks", "200", "--quiet"], 1);
    assert_eq!(sum(&a), sum(&b), "thread count changed the result");
    // tick 50: guards are mid-lap, the director is waiting: the frames are part of the saved state
    let dir = out_dir("co-state");
    let st = dir.join("state.txt").display().to_string();
    run(&exe, &["--ticks", "50", "--quiet", "--save-state", &st], 1);
    let (code, resumed) = run(&exe, &["--ticks", "150", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&a), "restoring coroutine frames changed the result");
    if let Some(asan) = build(&path, "co-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "200", "--quiet"], 1);
        assert_eq!(code, 0, "coroutines failed under sanitizers:\n{}", out);
    }
}

/// Grid-accelerated proximity loops give bit-identical results to full scans (the same program
/// built with -DSIJO_NO_SPATIAL), across archetypes, break/continue and conjunctive guards, and for
/// a crowd whose grid is built in parallel.
#[test]
fn spatial_grid_matches_full_scan() {
    for (path, ticks) in [(root().join("tests/programs/spatial.sijo"), "120"), (root().join("tests/programs/crowd.sijo"), "12"), (example("boids.sijo"), "150"), (example("arena.sijo"), "300")] {
        let src = std::fs::read_to_string(&path).unwrap();
        let mut d = Diags::default();
        let c = sijoc::driver::compile(&src, &mut d).unwrap();
        assert!(!c.spatial.loops.is_empty(), "{}: no proximity loop was accelerated", path.display());
        if path.ends_with("spatial.sijo") {
            assert_eq!(c.spatial.loops.len(), 3, "the if-guard, <= guard and early-continue loops are all accelerated");
        }
        let tag = path.file_stem().unwrap().to_string_lossy().to_string();
        let Some(grid) = build(&path, &format!("sp-grid-{tag}"), &["-fopenmp"]).or_else(|| build(&path, &format!("sp-grid1-{tag}"), &[])) else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let scan = build(&path, &format!("sp-scan-{tag}"), &["-DSIJO_NO_SPATIAL"]).unwrap();
        let (c1, a) = run(&grid, &["--ticks", ticks, "--quiet"], 4);
        let (c2, b) = run(&scan, &["--ticks", ticks, "--quiet"], 1);
        assert_eq!((c1, c2), (0, 0), "{}\n{}", a, b);
        assert_eq!(line(&a, "world checksum:"), line(&b, "world checksum:"), "{}: the grid changed the results", path.display());
    }
}

/// Runtime errors name the .sijo source line (#line directives + SIJO_AT).
#[test]
fn panics_report_sijo_source_lines() {
    let Some(cc) = cc() else { return };
    let src = "public static class Game\n{\n    public static void Start()\n    {\n        var xs = new List<int>();\n        int i = 3;\n        Print(xs[i]);\n    }\n}\n";
    let sm = sijoc::diag::SourceMap::single("boom.sijo", src);
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&sm.text, &mut d).unwrap();
    let dir = out_dir("panic-loc");
    let cfile = dir.join("boom.c");
    std::fs::write(&cfile, sijoc::codegen::generate_with(&c, "boom.sijo", &sm, "boom.c")).unwrap();
    let exe = dir.join("boom");
    let out = Command::new(&cc).args(["-std=gnu11", "-O1", "-ffp-contract=off", "-w"]).arg(&cfile).arg("-o").arg(&exe).arg("-lm").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (code, out) = run(&exe, &["--ticks", "1"], 1);
    assert_eq!(code, 101, "{}", out);
    assert!(out.contains("list index out of range") && out.contains("at boom.sijo:7"), "{}", out);
}

/// A game embedded in a C host through the library API: extern input/sound, draw list, RPCs.
/// The Dodge demo runs headless (no ncurses) with scripted input and must be reproducible.
#[test]
fn dodge_runs_embedded_in_a_c_host() {
    let Some(cc) = cc() else { return };
    let proj = example("dodge");
    let sm = sijoc::driver::load_project(&[proj.display().to_string()]).unwrap();
    assert!(sm.files.len() >= 5);
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&sm.text, &mut d).unwrap_or_else(|| panic!("{:?}", d.list));
    let dir = out_dir("dodge");
    std::fs::write(dir.join("dodge.c"), sijoc::codegen::generate_full(&c, "dodge", &sm, "dodge.c", true, sijoc::codegen::DEFAULT_LIB_PREFIX)).unwrap();
    std::fs::write(dir.join("sijo_game.h"), sijoc::codegen::embed_header(&c, sijoc::codegen::DEFAULT_LIB_PREFIX)).unwrap();
    let exe = dir.join("dodge");
    let out = Command::new(&cc)
        .args(["-std=gnu11", "-O1", "-ffp-contract=off", "-w", "-DDODGE_NO_CURSES"])
        .arg(format!("-I{}", dir.display()))
        .arg(dir.join("dodge.c"))
        .arg(proj.join("host_term.c"))
        .arg("-o")
        .arg(&exe)
        .arg("-lm")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (code, a) = run(&exe, &["--headless", "900"], 1);
    assert_eq!(code, 0, "{}", a);
    assert!(a.contains("SCORE") && a.contains("WAVE") && a.contains('A'), "{}", a);
    let (_, b) = run(&exe, &["--headless", "900"], 4);
    assert_eq!(a, b, "the embedded game is not reproducible");
}

/// Systems that spawn without using the handle run in parallel and stay bit-identical.
#[test]
fn parallel_spawns_are_deterministic() {
    let path = root().join("tests/programs/spawns.sijo");
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    let sys = |name: &str| c.ecs.systems.iter().find(|s| s.name == name).unwrap().clone();
    assert!(sys("Emitter.Update").parallel && sys("Emitter.Update").spawn_par, "spawning without using the handle should be parallel");
    assert!(sys("Keeper.Update").parallel && sys("Keeper.Update").spawn_handles, "storing the spawned handle in a field is patched after the merge");
    // a spawned handle stored in a list (or passed to a function, formatted...) cannot be patched
    let src2 = "public class S : Entity { public mut int L; public void Update(float dt) { L -= 1; } }
        public class K : Entity { public mut List<S?> Kids; public void Update(float dt) { Kids.Add(Spawn(new S { L = 3 })); } }
        public static class Game { public static void Start() { Spawn(new K()); } }";
    let mut d2 = Diags::default();
    let c2 = sijoc::driver::compile(src2, &mut d2).unwrap_or_else(|| panic!("{:?}", d2.list));
    let k = c2.ecs.systems.iter().find(|s| s.name == "K.Update").unwrap();
    assert!(!k.parallel, "a spawned handle kept in a list must keep the system serial");
    let Some(exe) = build(&path, "spawns-omp", &["-fopenmp"]) else {
        eprintln!("skipping: no OpenMP");
        return;
    };
    let mut outs = vec![];
    for t in [1, 2, 4] {
        let (code, out) = run(&exe, &["--ticks", "200", "--quiet"], t);
        assert_eq!(code, 0, "{}", out);
        outs.push((line(&out, "entities alive:").to_string(), line(&out, "world checksum:").to_string()));
    }
    assert!(outs.windows(2).all(|w| w[0] == w[1]), "{:?}", outs);
}

/// Number formatting and parsing match .NET 8 (invariant culture) byte for byte: the expected file
/// is the output of the same C# code. Also under the sanitizers (the bignum formatter).
#[test]
fn number_formatting_matches_dotnet() {
    let path = root().join("tests/programs/formatting.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/formatting.expected")).unwrap();
    let Some(exe) = build(&path, "fmt", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "1", "--no-summary"], 1);
    assert_eq!(code, 0, "{}", out);
    assert_eq!(out.trim(), expected.trim());
    if let Some(asan) = build(&path, "fmt-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "1", "--no-summary"], 1);
        assert_eq!(code, 0, "formatting failed under sanitizers:\n{}", out);
        assert_eq!(out.trim(), expected.trim());
    }
}

/// The same program gives the same bits at every optimization level and -march: no fused
/// multiply-add contraction (on an FMA-capable CPU, -march=native would otherwise change results).
#[test]
fn float_results_do_not_depend_on_optimization_flags() {
    for name in ["boids.sijo", "arena.sijo"] {
        let path = example(name);
        let mut sums = vec![];
        for (tag, flags) in [("O0", vec!["-O0"]), ("O3native", vec!["-O3", "-march=native"]), ("O2fast", vec!["-O2", "-ffp-contract=fast"])] {
            let Some(exe) = build(&path, &format!("{}-{}", name, tag), &flags) else {
                eprintln!("skipping: no C compiler");
                return;
            };
            let (code, out) = run(&exe, &["--ticks", "200", "--quiet"], 1);
            assert_eq!(code, 0, "{}", out);
            sums.push((tag, line(&out, "world checksum:").to_string()));
        }
        assert!(sums.windows(2).all(|w| w[0].1 == w[1].1), "{}: results depend on C compiler flags: {:?}", name, sums);
    }
}

/// C# parity: list-element assignment order (also when the value grows the list), undefined enum
/// values, ranges on field defaults, whole-tick Wait. Lines a-e are .NET 8's output.
#[test]
fn csharp_parity_program() {
    let path = root().join("tests/programs/parity.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/parity.expected")).unwrap();
    for (tag, flags) in [("parity", vec![]), ("parity-asan", vec!["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"])] {
        let Some(exe) = build(&path, tag, &flags) else {
            eprintln!("skipping {}", tag);
            continue;
        };
        let (code, out) = run(&exe, &["--ticks", "120", "--no-summary"], 1);
        assert_eq!(code, 0, "{}", out);
        assert_eq!(out.trim(), expected.trim(), "{}", tag);
    }
}

/// Unbounded recursion ends like in C#: a message, exit code 134, and the output printed so far.
#[test]
fn stack_overflow_reports_and_keeps_output() {
    let dir = out_dir("stackoverflow");
    let path = dir.join("so.sijo");
    std::fs::write(
        &path,
        "public static class R { [Recursive] public static int Down(int n) { if (n == 0) { return 0; } return Down(n - 1) * 3 + n; } }
         public static class Game { public static void Start() { Print(\"before\"); Print($\"{R.Down(100000000 + Time.Tick)}\"); } }",
    )
    .unwrap();
    let Some(exe) = build(&path, "so", &["-O0"]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let out = Command::new(&exe).args(["--ticks", "1", "--no-summary"]).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(134), "{}{}", stdout, stderr);
    assert!(stdout.contains("before"), "output before the crash was lost: {}", stdout);
    assert!(stderr.contains("stack overflow"), "{}", stderr);
}

/// `s += ...` in a loop grows the newest frame string in place: linear memory (not one copy per
/// iteration kept until the end of the tick: that would be 800+ MB for 10,000 appends), without ever
/// writing over bytes another string uses.
#[test]
fn string_appends_are_linear_and_alias_safe() {
    let dir = out_dir("strappend");
    let path = dir.join("sb.sijo");
    std::fs::write(
        &path,
        "public static class Game { public static void Start() {
           mut string text = \"\"; for (int i = 0; i < 10000; i++) { text += \"abcde \"; }
           mut string csv = \"\"; for (int i = 0; i < 20000; i++) { csv += $\"{i},{i * 0.5f:F1};\"; }
           mut string a = \"x\"; string b = a + \"y\"; a += \"z\"; mut string c = a; c += \"!\";
           mut string t = \"  hello  \".Trim(); t += \"?\"; string u = text.Substring(0, 5); text += u;
           Print($\"len={text.Length} csv={csv.Length} {csv.Substring(0, 24)} a={a} b={b} c={c} t={t} u={u}\"); } }",
    )
    .unwrap();
    for (tag, flags) in [("sb", vec![]), ("sb-asan", vec!["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"])] {
        let Some(exe) = build(&path, tag, &flags) else {
            eprintln!("skipping {}", tag);
            continue;
        };
        let (code, out) = run(&exe, &["--ticks", "1"], 1);
        assert_eq!(code, 0, "{}", out);
        assert!(out.contains("len=60005 csv=246670 0,0.0;1,0.5;2,1.0;3,1.5; a=xz b=xy c=xz! t=hello? u=abcde"), "{}", out);
        let peak: u64 = line(&out, "frame arena peak:").split_whitespace().nth(3).unwrap().parse().unwrap();
        assert!(peak < 4_000_000, "string building used {} bytes of frame arena", peak);
    }
}

/// sijoc build --checked: int overflow, int.MinValue / -1 and out-of-range float->int panic (C#
/// checked), with the .SIJO line; the default build wraps.
#[test]
fn checked_builds_trap_overflow() {
    if cc().is_none() {
        eprintln!("skipping: no C compiler");
        return;
    }
    let dir = out_dir("checked");
    let path = dir.join("chk.sijo");
    std::fs::write(&path, "public static class Game { public static void Start() { int z = Time.Tick; int big = 2147483600 + z; Print(\"before\");\n int k = big + 100;\n Print($\"after {k}\"); } }").unwrap();
    for (flag, want_code, want) in [("", 0, "after -2147483596"), ("--checked", 101, "arithmetic overflow")] {
        let exe = dir.join(format!("chk{}", flag.trim_start_matches('-')));
        let mut args = vec!["build".to_string(), path.display().to_string(), "-o".into(), dir.join("chk.c").display().to_string(), "--exe".into(), exe.display().to_string()];
        if !flag.is_empty() {
            args.push(flag.to_string());
        }
        let b = Command::new(env!("CARGO_BIN_EXE_sijoc")).args(&args).output().unwrap();
        assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
        let (code, out) = run(&exe, &["--ticks", "1", "--no-summary"], 1);
        assert_eq!(code, want_code, "{}", out);
        assert!(out.contains("before") && out.contains(want), "{}", out);
        if !flag.is_empty() {
            assert!(out.contains("chk.sijo:2"), "panic location: {}", out);
        }
    }
}

/// The generated C compiles without a single warning under -Wall -Wextra -pedantic -Werror on gcc
/// and clang (found by the fuzzer: unused query break labels, a -Warray-bounds false positive in
/// the snapshot codec for archetypes without replicated components).
#[test]
fn generated_c_is_warning_free() {
    let progs = ["tests/programs/regress.sijo", "tests/programs/semantics.sijo", "tests/programs/spatial.sijo", "tests/programs/parity.sijo", "../examples/village.sijo", "tests/programs/atomic.sijo", "tests/programs/atomic_entities.sijo", "tests/programs/persist.sijo"];
    let have = |c: &str| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    let dir = out_dir("werror");
    for p in progs {
        let path = root().join(p);
        let src = std::fs::read_to_string(&path).unwrap();
        let mut d = Diags::default();
        let c = sijoc::driver::compile(&src, &mut d).unwrap_or_else(|| panic!("{} failed to compile", p));
        let cfile = dir.join("w.c");
        std::fs::write(&cfile, sijoc::codegen::generate(&c, &path.display().to_string())).unwrap();
        for cc in ["gcc", "clang"] {
            if !have(cc) {
                continue;
            }
            for opt in ["-O0", "-O2"] {
                let out = Command::new(cc)
                    .args(["-std=gnu11", opt, "-ffp-contract=off", "-Wall", "-Wextra", "-pedantic", "-Werror", "-c"])
                    .arg(&cfile)
                    .arg("-o")
                    .arg(dir.join("w.o"))
                    .output()
                    .unwrap();
                assert!(out.status.success(), "{} {} {}: warnings in generated C:\n{}", cc, opt, p, String::from_utf8_lossy(&out.stderr));
            }
        }
    }
}

/// Loop-invariant handle comparisons / `is` tests hoisted out of query loops, partial hazard
/// snapshots and pending tables swapped into empty main tables give the same results as the
/// straightforward code (the expected output predates these optimizations); also under the
/// sanitizers and on any number of threads.
#[test]
fn hoisted_handles_and_partial_snapshots_keep_results() {
    let path = root().join("tests/programs/hoisting.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/hoisting.expected")).unwrap();
    let Some(exe) = build(&path, "hoisting-omp", &["-fopenmp"]).or_else(|| build(&path, "hoisting", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sums = vec![];
    for t in [1, 3, 4] {
        let (code, out) = run(&exe, &["--ticks", "30"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
        sums.push(line(&out, "world checksum:").to_string());
    }
    assert!(sums.windows(2).all(|w| w[0] == w[1]), "{:?}", sums);
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    let csrc = sijoc::codegen::generate(&c, "hoisting.sijo");
    assert!(csrc.contains("sijo_hrow(w, "), "the invariant handle comparison should be hoisted");
    if let Some(asan) = build(&path, "hoisting-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "30", "--quiet"], 1);
        assert_eq!(code, 0, "hoisting.sijo failed under sanitizers:\n{}", out);
    }
}

/// char: expected output (checked against C# semantics), any thread count, an exact save/restore of
/// char and string state, and the sanitizers (UTF-8 decoding, s[i], Split, case mapping).
#[test]
fn chars_and_string_indexing_work() {
    let path = root().join("tests/programs/chars.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/chars.expected")).unwrap();
    let Some(exe) = build(&path, "chars-omp", &["-fopenmp"]).or_else(|| build(&path, "chars", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "60"], 4);
    assert_eq!(code, 0, "{}", out);
    let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
    assert_eq!(printed.join("\n").trim(), expected.trim());
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, one) = run(&exe, &["--ticks", "60", "--quiet"], 1);
    assert_eq!(sum(&one), sum(&out), "thread count changed the result");
    let dir = out_dir("chars-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "30", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "30", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&out));
    if let Some(asan) = build(&path, "chars-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "60", "--quiet"], 1);
        assert_eq!(code, 0, "chars failed under sanitizers:\n{}", out);
    }
}

/// Arrays: expected output (C# semantics), any thread count, exact save/restore of array fields,
/// and the sanitizers (allocation regions, field initializers, double-buffered array fields).
#[test]
fn arrays_work() {
    let path = root().join("tests/programs/arrays.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/arrays.expected")).unwrap();
    let Some(exe) = build(&path, "arrays-omp", &["-fopenmp"]).or_else(|| build(&path, "arrays", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "30"], 4);
    assert_eq!(code, 0, "{}", out);
    let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
    assert_eq!(printed.join("\n").trim(), expected.trim());
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, one) = run(&exe, &["--ticks", "30", "--quiet"], 1);
    assert_eq!(sum(&one), sum(&out), "thread count changed the result");
    let dir = out_dir("arrays-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "15", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "15", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&out));
    if let Some(asan) = build(&path, "arrays-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "30", "--quiet"], 1);
        assert_eq!(code, 0, "arrays failed under sanitizers:\n{}", out);
    }
    // an index out of range names the .sijo line
    let src = "public class A : Entity { public mut int[] G = new int[3]; public mut int I; public void Update(float dt) { I += 1; G[I] = 1; } }\npublic static class Game { public static void Start() { Spawn(new A()); } }";
    let dir = out_dir("arrays-oob");
    let p = dir.join("oob.sijo");
    std::fs::write(&p, src).unwrap();
    if let Some(exe) = build(&p, "arrays-oob", &[]) {
        let (code, out) = run(&exe, &["--ticks", "5", "--quiet"], 1);
        assert_ne!(code, 0);
        assert!(out.contains("index out of range"), "{}", out);
    }
}

/// Multi-dimensional arrays: the expected output on 1 and 4 threads, plain and under
/// ASan/UBSan/LSan (fields freed on destroy, replaced arrays freed, nested lists), the client
/// world equal to the server's (the lengths are replicated with the elements), a saved state that
/// resumes to the same checksum, and an index outside its own dimension but inside the element
/// count panics.
#[test]
fn multi_dimensional_arrays_work() {
    let path = root().join("tests/programs/mdarrays.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/mdarrays.expected")).unwrap();
    check_printed(&path, "mdarrays", "30", &expected);
    let Some(exe) = build(&path, "mdarrays-state", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "30", "--quiet"], 1);
    assert_eq!(code, 0, "{}", out);
    assert!(line(&out, "replication check:").contains("OK"), "{}", out);
    let (code, lossy) = run(&exe, &["--ticks", "30", "--quiet", "--sim-loss", "0.3", "--sim-dup", "0.1"], 4);
    assert_eq!(code, 0, "{}", lossy);
    assert_eq!(line(&lossy, "replicated state hash:"), line(&out, "replicated state hash:"), "{}", lossy);
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let dir = out_dir("mdarrays-st");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "15", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "15", "--quiet", "--load-state", &st], 4);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&out));
    // loading replaces the arrays the field initializers made for the new rows: no leaks
    if let Some(asan) = build(&path, "mdarrays-state-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, resumed) = run(&asan, &["--ticks", "15", "--quiet", "--load-state", &st], 1);
        assert_eq!(code, 0, "{}", resumed);
        assert_eq!(sum(&resumed), sum(&out));
    }
    // G[0, 4] of a 3x4 array: position 4 is one of the 12 elements, but 4 is no second index
    let src = "public class A : Entity { public mut int[,] G = new int[3, 4]; public mut int I; public void Update(float dt) { I += 1; G[0, I] = I; } }\npublic static class Game { public static void Start() { Spawn(new A()); } }";
    let p = inline_program("mdoob", src);
    if let Some(exe) = build(&p, "mdoob", &[]) {
        let (code, out) = run(&exe, &["--ticks", "6", "--quiet"], 1);
        assert_ne!(code, 0, "{}", out);
        assert!(out.contains("index out of range"), "{}", out);
    }
}

/// Loops whose iterations cannot leak frame memory give it back (memory bounded by one
/// iteration); loops whose values outlive an iteration keep it. Same results, sanitizer-clean.
#[test]
fn frame_arena_is_rewound_per_iteration() {
    let path = root().join("tests/programs/rewind.sijo");
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    let csrc = sijoc::codegen::generate(&c, "rewind.sijo");
    assert_eq!(csrc.matches("sijo_arena_mark(&w->frame").count(), 2, "only the temporary-only loops rewind");
    let Some(exe) = build(&path, "rewind-omp", &["-fopenmp"]).or_else(|| build(&path, "rewind", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "3"], 4);
    assert_eq!(code, 0, "{}", out);
    assert_eq!(out.lines().filter(|l| l.ends_with(" 6041960 01234k19")).count(), 12, "{}", out);
    let peak: usize = line(&out, "frame arena peak:").split_whitespace().nth(3).unwrap().parse().unwrap();
    assert!(peak < 64 * 1024, "frame arena peak {} bytes: the loop's temporaries were not given back", peak);
    if let Some(asan) = build(&path, "rewind-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g", "-fopenmp"]) {
        let (code, out) = run(&asan, &["--ticks", "3", "--quiet"], 4);
        assert_eq!(code, 0, "rewind.sijo failed under sanitizers:\n{}", out);
    }
}

/// Nested collections: expected output, threads, save/restore, and the sanitizers including the
/// leak checker (element lists are freed with their outer list, by RemoveAt, Clear, reassignment).
#[test]
fn nested_collections_work() {
    let path = root().join("tests/programs/nested.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/nested.expected")).unwrap();
    let Some(exe) = build(&path, "nested-omp", &["-fopenmp"]).or_else(|| build(&path, "nested", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "18"], 4);
    assert_eq!(code, 0, "{}", out);
    let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
    assert_eq!(printed.join("\n").trim(), expected.trim());
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, one) = run(&exe, &["--ticks", "18", "--quiet"], 1);
    assert_eq!(sum(&one), sum(&out), "thread count changed the result");
    let dir = out_dir("nested-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "9", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "9", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&out));
    if let Some(asan) = build(&path, "nested-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "18", "--quiet"], 1);
        assert_eq!(code, 0, "nested.sijo failed under sanitizers:\n{}", out);
    }
}

/// Strings sent to other entities in messages arrive intact (copied by value), on any thread count
/// and under the sanitizers.
#[test]
fn messages_carry_strings_by_value() {
    let path = root().join("tests/programs/messages.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/messages.expected")).unwrap();
    let Some(exe) = build(&path, "messages-omp", &["-fopenmp"]).or_else(|| build(&path, "messages", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "10"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
    }
    if let Some(asan) = build(&path, "messages-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "10", "--quiet"], 1);
        assert_eq!(code, 0, "messages.sijo failed under sanitizers:\n{}", out);
    }
}

/// Lists, arrays (also multi-dimensional) and lists of strings, structs and lists as message
/// arguments: copied into the command buffer, rebuilt for the receiver (borrowed: frame arena;
/// `own`: the receiver's to keep), the sender's moved list freed, a message dropped by `fail`;
/// the same on 1 and 4 threads and clean under ASan/UBSan/LSan.
#[test]
fn messages_carry_lists_by_value() {
    let path = root().join("tests/programs/msglists.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/msglists.expected")).unwrap();
    check_printed(&path, "msglists", "12", &expected);
}

/// Parallel systems that store, compare and send to the handles they spawn: provisional handles are
/// patched after the merge, so the results equal the serial run (the expected output and world
/// checksum come from the compiler that ran this system serially).
#[test]
fn spawned_handles_are_patched_after_a_parallel_system() {
    let path = root().join("tests/programs/spawnhandles.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/spawnhandles.expected")).unwrap();
    let Some(exe) = build(&path, "spawnh-omp", &["-fopenmp"]) else {
        eprintln!("skipping: no OpenMP");
        return;
    };
    for t in [1, 2, 4] {
        let (code, out) = run(&exe, &["--ticks", "40"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
        assert_eq!(line(&out, "world checksum:"), "world checksum: 35f233e2794d0519", "threads={}", t);
        assert!(line(&out, "entities alive:").contains("spawned 20300, destroyed 19072"), "{}", out);
    }
    if let Some(asan) = build(&path, "spawnh-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g", "-fopenmp"]) {
        let (code, out) = run(&asan, &["--ticks", "40", "--quiet"], 4);
        assert_eq!(code, 0, "spawnhandles.sijo failed under sanitizers:\n{}", out);
    }
}

/// Lambdas passed to list and array methods, inline: expected output, threads, sanitizers.
#[test]
fn lambdas_work_with_lists_and_arrays() {
    let path = root().join("tests/programs/lambdas.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/lambdas.expected")).unwrap();
    let Some(exe) = build(&path, "lambdas-omp", &["-fopenmp"]).or_else(|| build(&path, "lambdas", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "10"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
    }
    if let Some(asan) = build(&path, "lambdas-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "10", "--quiet"], 1);
        assert_eq!(code, 0, "lambdas.sijo failed under sanitizers:\n{}", out);
    }
}

/// Coroutines awaiting other coroutines: expected timeline, threads, a save/restore while nested
/// coroutines are suspended, sanitizers.
#[test]
fn coroutines_await_other_coroutines() {
    let path = root().join("tests/programs/awaitcalls.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/awaitcalls.expected")).unwrap();
    let Some(exe) = build(&path, "awaitcalls-omp", &["-fopenmp"]).or_else(|| build(&path, "awaitcalls", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "30"], 4);
    assert_eq!(code, 0, "{}", out);
    let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
    assert_eq!(printed.join("\n").trim(), expected.trim());
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, one) = run(&exe, &["--ticks", "30", "--quiet"], 1);
    assert_eq!(sum(&one), sum(&out));
    let dir = out_dir("awaitcalls-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "9", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "21", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&out));
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    let names: Vec<&str> = c.ecs.systems.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Guard.Patrol") && !names.contains(&"Guard.MoveTo"), "{:?}", names);
    if let Some(asan) = build(&path, "awaitcalls-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "30", "--quiet"], 1);
        assert_eq!(code, 0, "awaitcalls.sijo failed under sanitizers:\n{}", out);
    }
}

/// Generic methods (instantiated per type arguments, inferred or written) and collection
/// initializers: expected output, threads, sanitizers.
#[test]
fn generic_methods_and_collection_initializers() {
    let path = root().join("tests/programs/generics.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/generics.expected")).unwrap();
    let Some(exe) = build(&path, "generics-omp", &["-fopenmp"]).or_else(|| build(&path, "generics", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "4"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
    }
    if let Some(asan) = build(&path, "generics-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "4", "--quiet"], 1);
        assert_eq!(code, 0, "generics.sijo failed under sanitizers:\n{}", out);
    }
}

/// Generic entity classes: every instance (Pool<int>, Pool<SVec2>, Hunter<Orc>, Pair<string,
/// string>, ...) is an entity class with its own archetype. Two instances query and write to other
/// entities remotely, a class derives from an instance, client code reads instance fields
/// (replicated and checked), a coroutine runs in an instance; expected output, 1 and 4 threads,
/// save + load of the state (the instances are named in its schema), sanitizers.
#[test]
fn generic_entity_classes_are_classes_of_their_own() {
    let path = root().join("tests/programs/genclasses.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/genclasses.expected")).unwrap();
    // explain shows the instances as ordinary classes with readable names
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).expect("compiles");
    assert!(!d.list.iter().any(|x| x.level == sijoc::diag::Level::Warning), "{:?}", d.list);
    let report = sijoc::explain::report(&c, &sijoc::diag::SourceMap::single("genclasses.sijo", &src));
    for want in ["generic class Pool<T>: 2 instance(s), each an entity class of its own: Pool<int>, Pool<SVec2>", "archetype Pool<int>", "archetype Hunter<Elf>", "archetype Pair<string, string>", "Hunter<Orc>.Update over [Hunter<Orc>]"] {
        assert!(report.contains(want), "explain is missing {:?}:\n{}", want, report);
    }
    let Some(exe) = build(&path, "genclasses-omp", &["-fopenmp"]).or_else(|| build(&path, "genclasses", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sums = vec![];
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "9"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
        assert!(line(&out, "replication check:").contains("OK"), "{}", out);
        assert!(line(&out, "entities alive:").contains("Pool<int>=1 Pool<SVec2>=1 IntPool=1"), "{}", out);
        sums.push(line(&out, "world checksum:").to_string());
    }
    assert_eq!(sums[0], sums[1], "nondeterministic: {:?}", sums);
    // the saved state names every instance; save + load does not change the simulation
    let state = exe.with_extension("state");
    let (_, straight) = run(&exe, &["--ticks", "12", "--quiet"], 4);
    let (code, _) = run(&exe, &["--ticks", "6", "--quiet", "--save-state", state.to_str().unwrap()], 4);
    assert_eq!(code, 0);
    let saved = std::fs::read_to_string(&state).unwrap();
    for want in ["arch Pool<int> 1 ", "arch Pool<SVec2> 1 ", "arch Hunter<Orc> 1 ", "arch Pair<string,string> 1 ", "field Prey handle:Orc", "field Mine handle:Pool<int>"] {
        assert!(saved.contains(want), "state has no {:?}:\n{}", want, saved);
    }
    let (code, resumed) = run(&exe, &["--ticks", "6", "--quiet", "--load-state", state.to_str().unwrap()], 4);
    assert_eq!(code, 0, "{}", resumed);
    assert!(resumed.contains("state restored at tick 6"), "{}", resumed);
    assert_eq!(line(&straight, "world checksum:"), line(&resumed, "world checksum:"));
    if let Some(asan) = build(&path, "genclasses-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        for t in [1, 4] {
            let (code, out) = run(&asan, &["--ticks", "9", "--quiet"], t);
            assert_eq!(code, 0, "genclasses.sijo failed under sanitizers:\n{}", out);
        }
    }
}

/// Interfaces: interface handles, Query<I>(), `is I`, interface extension, abstract classes that
/// leave methods to subclasses, `where T : I` with structs: expected output, threads, sanitizers.
#[test]
fn interfaces_dispatch_per_archetype() {
    let path = root().join("tests/programs/interfaces.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/interfaces.expected")).unwrap();
    let Some(exe) = build(&path, "interfaces-omp", &["-fopenmp"]).or_else(|| build(&path, "interfaces", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "8"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
    }
    if let Some(asan) = build(&path, "interfaces-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "8", "--quiet"], 1);
        assert_eq!(code, 0, "interfaces.sijo failed under sanitizers:\n{}", out);
    }
}

/// Method overloading: best-fit choice among overloads (exact type, widening, derived class, null),
/// overrides of one overload, a new overload in a derived class, remote calls of overloads, a
/// default implementation calling another overload, and one class implementing two instances of
/// a generic interface: expected output, threads, sanitizers.
#[test]
fn overloads_pick_the_best_fit() {
    let path = root().join("tests/programs/overloads.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/overloads.expected")).unwrap();
    let Some(exe) = build(&path, "overloads-omp", &["-fopenmp"]).or_else(|| build(&path, "overloads", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "4"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
    }
    if let Some(asan) = build(&path, "overloads-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "4", "--quiet"], 1);
        assert_eq!(code, 0, "overloads.sijo failed under sanitizers:\n{}", out);
    }
}

/// Delegates: lambdas, method groups and bound methods as values, captures (locals, `this`, live
/// references that die), composition (delegates captured in boxes), delegates in fields, lists and
/// structs: expected output, threads 1/4, shard worlds, saved state that restores to the same run,
/// sanitizers.
#[test]
fn delegates_hold_lambdas_and_methods() {
    let path = root().join("tests/programs/delegates.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/delegates.expected")).unwrap();
    let Some(exe) = build(&path, "delegates-omp", &["-fopenmp"]).or_else(|| build(&path, "delegates", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sums = vec![];
    for (t, shards) in [(1, "1"), (4, "1"), (4, "3")] {
        let (code, out) = run(&exe, &["--ticks", "4", "--shards", shards], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={} shards={}", t, shards);
        sums.push(line(&out, "world checksum:").to_string());
    }
    assert!(sums.iter().all(|s| *s == sums[0]), "{:?}", sums);
    // delegate fields (with captured references and strings) survive a save and restore
    let dir = out_dir("delegates-state");
    let state = dir.join("t2.state");
    let (code, out) = run(&exe, &["--ticks", "2", "--quiet", "--save-state", state.to_str().unwrap()], 1);
    assert_eq!(code, 0, "{}", out);
    let (code, resumed) = run(&exe, &["--ticks", "2", "--quiet", "--load-state", state.to_str().unwrap()], 1);
    assert_eq!(code, 0, "{}", resumed);
    let (_, straight) = run(&exe, &["--ticks", "4", "--quiet"], 1);
    assert_eq!(line(&resumed, "world checksum:"), line(&straight, "world checksum:"));
    if let Some(asan) = build(&path, "delegates-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "6", "--quiet"], 1);
        assert_eq!(code, 0, "delegates.sijo failed under sanitizers:\n{}", out);
    }
}

/// Generic static classes (functions and constants per type argument) and static interface
/// members (`static abstract` reached through `T.Zero()`, an interface's own static function):
/// expected output, threads, sanitizers.
#[test]
fn generic_static_classes_and_static_interface_members() {
    let path = root().join("tests/programs/statics.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/statics.expected")).unwrap();
    let Some(exe) = build(&path, "statics-omp", &["-fopenmp"]).or_else(|| build(&path, "statics", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "3"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
    }
    if let Some(asan) = build(&path, "statics-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "3", "--quiet"], 1);
        assert_eq!(code, 0, "statics.sijo failed under sanitizers:\n{}", out);
    }
}

/// [Persistent] fields: a run with `--store DIR` writes what [PersistentKey] entities hold; the next
/// run loads it when they spawn (a respawn within a run takes the latest values). From the same
/// store, every thread and shard count gives the same world and writes the same store; a field
/// whose type changed in a later version keeps its initializer (the store migrates by name and
/// type, like state files).
#[test]
fn persistent_fields_outlive_the_run() {
    let path = root().join("tests/programs/persist.sijo");
    let Some(exe) = build(&path, "persist-omp", &["-fopenmp"]).or_else(|| build(&path, "persist", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let dir = out_dir("persist-store");
    let store = dir.join("store");
    let _ = std::fs::remove_dir_all(&store);
    std::fs::create_dir_all(&store).unwrap();
    let st = store.to_str().unwrap().to_string();
    let (code, first) = run(&exe, &["--ticks", "8", "--store", &st], 1);
    assert_eq!(code, 0, "{}", first);
    // bob, destroyed at tick 5 with 55 gold and respawned at 7, takes his record back
    assert!(first.contains("t8 bob gold 56 items 1 level 1 seen 1"), "{}", first);
    let file = std::fs::read_to_string(store.join("Player.store")).unwrap();
    assert!(file.starts_with("SIJO-STORE 1\nclass Player\nfields 3\n"), "{}", file);
    assert!(file.contains("rec s616e6e | 18 |"), "{}", file);
    // the next run starts from it, whatever the threads and shards
    let mut outs = vec![];
    for (t, shards) in [(1, "1"), (4, "1"), (4, "3")] {
        let copy = dir.join(format!("copy-{t}-{shards}"));
        let _ = std::fs::remove_dir_all(&copy);
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::copy(store.join("Player.store"), copy.join("Player.store")).unwrap();
        let (code, out) = run(&exe, &["--ticks", "4", "--shards", shards, "--store", copy.to_str().unwrap()], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().filter(|l| l.starts_with('t') && l[1..].starts_with(|c: char| c.is_ascii_digit())).collect();
        outs.push((printed.join("\n"), line(&out, "world checksum:").to_string(), std::fs::read_to_string(copy.join("Player.store")).unwrap()));
    }
    assert!(outs[0].0.starts_with("t1 ann gold 19 items 2 level 1 seen 1\nt1 bob gold 57 items 1"), "{}", outs[0].0);
    assert!(outs.windows(2).all(|w| w[0] == w[1]), "{:?}", outs);
    // a later version: Level became a float (another type signature), Gold still loads
    let src = std::fs::read_to_string(&path).unwrap().replace("[Persistent] public mut int in 0..100 Level = 1;", "[Persistent] public mut float Level = 2.5f;");
    let p2 = dir.join("persist2.sijo");
    std::fs::write(&p2, src).unwrap();
    let exe2 = build(&p2, "persist2", &[]).unwrap();
    let copy = dir.join("copy-v2");
    let _ = std::fs::remove_dir_all(&copy);
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::copy(store.join("Player.store"), copy.join("Player.store")).unwrap();
    let (code, out) = run(&exe2, &["--ticks", "1", "--store", copy.to_str().unwrap()], 1);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("t1 ann gold 19 items 2 level 2.5 seen 1"), "{}", out);
}

/// await Race(...) / await Sync(...): expected timeline, threads, sanitizers.
#[test]
fn coroutines_race_and_sync() {
    let path = root().join("tests/programs/race.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/race.expected")).unwrap();
    let Some(exe) = build(&path, "race-omp", &["-fopenmp"]).or_else(|| build(&path, "race", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "30"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
    }
    // saved while Sync(...) is suspended (tick 10), restored and run to the end: same world
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, full) = run(&exe, &["--ticks", "30", "--quiet"], 1);
    let dir = out_dir("race-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "10", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "20", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&full));
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    let names: Vec<&str> = c.ecs.systems.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Guard.Run") && !names.contains(&"Guard.MoveTo") && !names.contains(&"Guard.Blink"), "{:?}", names);
    if let Some(asan) = build(&path, "race-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "30", "--quiet"], 1);
        assert_eq!(code, 0, "race.sijo failed under sanitizers:\n{}", out);
    }
}

/// await Rush(...): unfinished method arms keep running in the background; expected timeline,
/// threads, a save/restore while an arm runs in the background, sanitizers.
#[test]
fn coroutines_rush_leaves_arms_running() {
    let path = root().join("tests/programs/rush.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/rush.expected")).unwrap();
    let Some(exe) = build(&path, "rush-omp", &["-fopenmp"]).or_else(|| build(&path, "rush", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "25"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
    }
    // saved at tick 8, while Walk runs in the background, then resumed
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (_, full) = run(&exe, &["--ticks", "25", "--quiet"], 1);
    let dir = out_dir("rush-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "8", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "17", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&full));
    if let Some(asan) = build(&path, "rush-asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "25", "--quiet"], 1);
        assert_eq!(code, 0, "rush.sijo failed under sanitizers:\n{}", out);
    }
}

/// atomic blocks: every list/array/dictionary operation on locals, `ref` parameters and fields
/// (also through called methods and collections of collections) rolled back by `fail` and kept
/// without it, nested blocks, `return`/`break`/`continue` committing, loops that allocate around
/// and inside blocks, a coroutine, and a parallel system whose helper spawns entities with lists
/// (dropped from the per-thread tables on `fail`). Expected output, 1 and 4 threads, sanitizers
/// with the leak checker, equal world checksums and a save/restore in the middle.
#[test]
fn atomic_blocks_roll_back_collections_and_nest() {
    let path = root().join("tests/programs/atomic_entities.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/atomic_entities.expected")).unwrap();
    check_printed(&path, "atomic-entities", "4", &expected);
    let path = root().join("tests/programs/atomic.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/atomic.expected")).unwrap();
    check_printed(&path, "atomic", "5", &expected);
    let Some(exe) = build(&path, "atomic-sum-omp", &["-fopenmp"]).or_else(|| build(&path, "atomic-sum", &[])) else {
        return;
    };
    let sum = |out: &str| line(out, "world checksum:").to_string();
    let (code, one) = run(&exe, &["--ticks", "5", "--quiet"], 1);
    assert_eq!(code, 0, "{}", one);
    let (code, four) = run(&exe, &["--ticks", "5", "--quiet"], 4);
    assert_eq!(code, 0, "{}", four);
    assert_eq!(sum(&one), sum(&four), "thread count changed the result");
    let dir = out_dir("atomic-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "2", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "3", "--quiet", "--load-state", &st], 4);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(sum(&resumed), sum(&one));
}

/// Writes an inline program to its own directory (tests run in parallel).
fn inline_program(tag: &str, src: &str) -> PathBuf {
    let path = out_dir(&format!("{}-src", tag)).join(format!("{}.sijo", tag));
    std::fs::write(&path, src).unwrap();
    path
}

/// Builds `path` with OpenMP (or without when it is missing) and under ASan/UBSan/LSan, runs both
/// on 1 and 4 threads and checks the printed lines.
fn check_printed(path: &Path, tag: &str, ticks: &str, expected: &str) {
    let Some(exe) = build(path, &format!("{}-omp", tag), &["-fopenmp"]).or_else(|| build(path, tag, &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut exes = vec![(exe, "plain")];
    let asan = ["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"];
    let omp_asan = [asan[0], asan[1], asan[2], "-fopenmp"];
    if let Some(e) = build(path, &format!("{}-asan-omp", tag), &omp_asan).or_else(|| build(path, &format!("{}-asan", tag), &asan)) {
        exes.push((e, "sanitizers"));
    }
    for (exe, how) in &exes {
        for threads in [1, 4] {
            let out = Command::new(exe).args(["--ticks", ticks, "--no-summary"]).env("OMP_NUM_THREADS", threads.to_string()).env("ASAN_OPTIONS", "detect_leaks=1").output().unwrap();
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert_eq!(out.status.code(), Some(0), "{} ({}, {} threads):\n{}", tag, how, threads, text);
            assert_eq!(text.trim(), expected.trim(), "{} ({}, {} threads)", tag, how, threads);
        }
    }
}

const RANGES_SRC: &str = r#"public struct SVec2
{
    public float X;
    public float Y;

    public SVec2(float x, float y)
    {
        X = x;
        Y = y;
    }
}

public class Mover : Entity
{
    // a range on a struct field applies to each member
    public mut SVec2 in 0..10 Pos;
    public mut int in -5..5 Level;

    public void Update(float dt)
    {
        if (Time.Tick == 3)
        {
            Pos = new SVec2(-20, 30.5f);
            Level = -100;
            return;
        }
        Pos.X += 4;
        Pos.Y -= 3;
        Level += Time.Tick < 3 ? -4 : 4;
    }
}

public static class Clamp
{
    public static int Percent(int in 0..100 value)
    {
        return value;
    }

    public static int Twice(mut int in 0..50 value)
    {
        value *= 2;
        return value;
    }

    public static float in -1..1 Unit(float x)
    {
        return x * 2;
    }

    public static int in 0..9 Digit(int value)
    {
        return value;
    }

    public static void Bump(ref int target, int by)
    {
        target += by;
    }
}

public static class Game
{
    public static void Start()
    {
        // values from Time.Tick are only known at run time
        int zero = Time.Tick;
        mut int in 0..10 count = 15 + zero;
        Print($"init {count}");
        count = -3 + zero;
        Print($"assign {count}");
        count += 7;
        count *= 3;
        Print($"compound {count}");
        count++;
        Print($"inc {count}");
        count = zero;
        count--;
        Print($"dec {count}");
        count -= 100;
        Print($"sub {count}");
        mut float in -1.5..2.5 ratio = 3.75f + zero;
        Print($"float init {ratio}");
        ratio -= 10;
        Print($"float compound {ratio}");
        ratio++;
        ratio++;
        ratio++;
        ratio++;
        ratio++;
        Print($"float inc {ratio}");
        float none = zero;
        ratio = none / none;
        Print($"float nan {ratio}");
        ratio = 1.25f;
        ratio /= none;
        Print($"float inf {ratio}");
        mut int in 0..10 bumped = 4 + zero;
        Clamp.Bump(ref bumped, 20);
        Print($"ref up {bumped}");
        Clamp.Bump(ref bumped, -30);
        Print($"ref down {bumped}");
        Print($"param {Clamp.Percent(150 + zero)} {Clamp.Percent(-3 + zero)} {Clamp.Percent(42 + zero)}");
        Print($"mut param {Clamp.Twice(40 + zero)} {Clamp.Twice(-8 + zero)} {Clamp.Twice(7 + zero)}");
        Print($"return {Clamp.Unit(0.25f + zero)} {Clamp.Unit(3 + zero)} {Clamp.Unit(-7 + zero)} {Clamp.Unit(none / none)}");
        Print($"int return {Clamp.Digit(12 + zero)} {Clamp.Digit(-1 + zero)} {Clamp.Digit(5 + zero)}");
        Spawn(new Mover { Pos = new SVec2(-2, 12), Level = 9 });
    }

    public static void Update(float dt)
    {
        foreach (var mover in Query<Mover>())
        {
            Print($"tick {Time.Tick}: pos={mover.Pos.X},{mover.Pos.Y} level={mover.Level}");
        }
    }
}
"#;

/// Range types saturate every write: locals (initializer, assignment, compound assignment, ++/--,
/// a `ref` argument the callee writes), value parameters (also one the callee changes), return
/// values, float NaN (to lo) and infinity, and a struct-typed entity field (each member), at spawn
/// and in a system.
#[test]
fn range_types_saturate_every_write() {
    let path = inline_program("ranges", RANGES_SRC);
    let expected = "init 10\nassign 0\ncompound 10\ninc 10\ndec 0\nsub 0\n\
        float init 2.5\nfloat compound -1.5\nfloat inc 2.5\nfloat nan -1.5\nfloat inf 2.5\n\
        ref up 10\nref down 0\nparam 100 0 42\nmut param 50 0 14\nreturn 0.5 1 -1 -1\nint return 9 0 5\n\
        tick 1: pos=4,7 level=1\ntick 2: pos=8,4 level=-3\ntick 3: pos=0,10 level=-5\ntick 4: pos=4,7 level=-1\n\
        tick 5: pos=8,4 level=3\ntick 6: pos=10,1 level=5\ntick 7: pos=10,0 level=5";
    check_printed(&path, "ranges", "7", expected);
}

const CTFE_RANGES_SRC: &str = r#"public static class Clamp
{
    public static int Assign(int start)
    {
        mut int in 0..10 count = start;
        count = start + 40;
        return count;
    }

    public static int Compound(int start)
    {
        mut int in 0..10 count = start;
        count += 40;
        return count;
    }

    public static int Increment(int start)
    {
        mut int in 0..10 count = start;
        count++;
        return count;
    }

    public static int Param(int in 0..10 value)
    {
        return value;
    }

    public static int Twice(mut int in 0..50 value)
    {
        value *= 2;
        return value;
    }

    public static float Ratio(float start)
    {
        mut float in -1.5..2.5 ratio = start;
        ratio -= 10;
        return ratio;
    }

    public static float in -1..1 Unit(float x)
    {
        return x * 2;
    }
}

public static class Game
{
    public static void Start()
    {
        // constant arguments: evaluated at compile time; `+ zero`: at run time
        int zero = Time.Tick;
        Print($"pair assign {Clamp.Assign(5)} {Clamp.Assign(5 + zero)}");
        Print($"pair compound {Clamp.Compound(5)} {Clamp.Compound(5 + zero)}");
        Print($"pair increment {Clamp.Increment(10)} {Clamp.Increment(10 + zero)}");
        Print($"pair param {Clamp.Param(50)} {Clamp.Param(50 + zero)}");
        Print($"pair twice {Clamp.Twice(40)} {Clamp.Twice(40 + zero)}");
        Print($"pair ratio {Clamp.Ratio(1)} {Clamp.Ratio(1 + zero)}");
        Print($"pair unit {Clamp.Unit(3)} {Clamp.Unit(3 + zero)}");
    }
}
"#;

/// Calls with constant arguments are evaluated at compile time: ranges on their parameters and
/// locals must saturate there exactly as at run time.
#[test]
fn range_types_saturate_in_compile_time_evaluation() {
    let path = inline_program("ctfe-ranges", CTFE_RANGES_SRC);
    let Some(exe) = build(&path, "ctfe-ranges", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "1", "--no-summary"], 1);
    assert_eq!(code, 0, "{}", out);
    let want = [("assign", "10"), ("compound", "10"), ("increment", "10"), ("param", "10"), ("twice", "50"), ("ratio", "-1.5"), ("unit", "1")];
    let pairs: Vec<Vec<&str>> = out.lines().filter_map(|l| l.strip_prefix("pair ")).map(|l| l.split_whitespace().collect()).collect();
    assert_eq!(pairs.len(), want.len(), "{}", out);
    for (p, (name, value)) in pairs.iter().zip(want) {
        assert_eq!((p[0], p[2]), (name, value), "run time: {}", out);
        assert_eq!(p[1], p[2], "'{}': compile-time evaluation and run time disagree:\n{}", name, out);
    }
}

const OWN_SRC: &str = r#"public class Bag : Entity
{
    public int Seed;
    public mut List<int> Items;
    public mut List<int> Extra;

    public void Update(float dt)
    {
        // a list built in a function, moved into an own parameter and stored in a mut field
        List<int> built = Lists.Make(Seed + Time.Tick);
        keep(built);
        // a call result passed directly; the own parameter is changed and returned
        Extra = Lists.Grow(Lists.Make(Seed));
        Extra = Lists.Grow(Extra.Clone());
    }

    void keep(own List<int> xs)
    {
        Items = xs;
    }
}

public static class Lists
{
    public static own List<int> Make(int count)
    {
        mut List<int> xs = new List<int>();
        for (int i = 0; i < count; i++)
        {
            xs.Add(i * 10);
        }
        return xs;
    }

    public static own List<int> Grow(mut own List<int> xs)
    {
        xs.Add(1);
        return xs;
    }

    // takes the list and drops it: freed when the call ends
    public static int Total(own List<int> xs)
    {
        mut int sum = 0;
        foreach (int x in xs)
        {
            sum += x;
        }
        return sum;
    }

    public static string Show(List<int> xs)
    {
        mut string text = "";
        foreach (int x in xs)
        {
            text += $"{x} ";
        }
        return $"[{text.Trim()}]";
    }
}

public static class Game
{
    public static void Start()
    {
        mut List<int> start = Lists.Make(3);
        start.Add(7);
        mut List<int> grown = Lists.Grow(start);
        grown = Lists.Grow(grown);
        Print($"start {Lists.Show(grown)} total {Lists.Total(grown)}");
        for (int i = 0; i < 3; i++)
        {
            Spawn(new Bag { Seed = i, Items = Lists.Make(i), Extra = new List<int>() });
        }
    }

    public static void Update(float dt)
    {
        if (Time.Tick > 3)
        {
            return;
        }
        foreach (var bag in Query<Bag>())
        {
            Print($"tick {Time.Tick} bag {bag.Seed}: items={Lists.Show(bag.Items)} extra={Lists.Show(bag.Extra)} total={Lists.Total(bag.Items.Clone())}");
        }
    }
}
"#;

/// `own` parameters: a list built in a function and moved into an own parameter is stored in a
/// `mut` entity field (replacing the previous list), an own parameter is changed and returned, call
/// results are passed directly, and a list taken and dropped is freed. ASan/LSan fail the test on a
/// leak or a double free; also on 4 threads (the entity system is parallel).
#[test]
fn own_parameters_move_lists_without_leaks() {
    let path = inline_program("own", OWN_SRC);
    let src = std::fs::read_to_string(&path).unwrap();
    let c = sijoc::driver::compile(&src, &mut Diags::default()).unwrap();
    assert!(c.ecs.systems.iter().any(|s| s.name == "Bag.Update" && s.parallel), "Bag.Update should be data-parallel");
    let expected = "start [0 10 20 7 1 1] total 39\n\
        tick 1 bag 0: items=[0] extra=[1 1] total=0\ntick 1 bag 1: items=[0 10] extra=[0 1 1] total=10\ntick 1 bag 2: items=[0 10 20] extra=[0 10 1 1] total=30\n\
        tick 2 bag 0: items=[0 10] extra=[1 1] total=10\ntick 2 bag 1: items=[0 10 20] extra=[0 1 1] total=30\ntick 2 bag 2: items=[0 10 20 30] extra=[0 10 1 1] total=60\n\
        tick 3 bag 0: items=[0 10 20] extra=[1 1] total=30\ntick 3 bag 1: items=[0 10 20 30] extra=[0 1 1] total=60\ntick 3 bag 2: items=[0 10 20 30 40] extra=[0 10 1 1] total=100";
    // 40 ticks: the lists of every tick are replaced (and must be freed) 40 times
    check_printed(&path, "own", "40", expected);
}

const MUT_METHODS_SRC: &str = r#"public struct SCounter
{
    public int Count;
    public float Scale;

    public SCounter(int count, float scale)
    {
        Count = count;
        Scale = scale;
    }

    public mut void Add(int amount)
    {
        Count += amount;
    }

    public mut void Halve()
    {
        Scale *= 0.5f;
        Add(1);
    }

    public int Total()
    {
        return Count * 100 + (int)(Scale * 10);
    }
}

public class Holder : Entity
{
    public int Step;
    public mut SCounter Counter;

    public void Update(float dt)
    {
        Counter.Add(Step);
        Counter.Halve();
    }
}

public static class Game
{
    public static void Start()
    {
        mut SCounter local = new SCounter(5, 8);
        local.Add(-2);
        local.Halve();
        SCounter copy = local;
        local.Add(10);
        Print($"local {local.Count} {local.Scale} copy {copy.Count} {copy.Scale} total {local.Total()}");
        Spawn(new Holder { Step = 3, Counter = new SCounter(1, 64) });
    }

    public static void Update(float dt)
    {
        foreach (var holder in Query<Holder>())
        {
            Print($"tick {Time.Tick}: {holder.Counter.Count} {holder.Counter.Scale} total {holder.Counter.Total()}");
        }
    }
}
"#;

/// `mut` struct methods change the variable they are called on: a mutable local (a copy taken
/// before keeps its value), a mutable entity field, and a `mut` method calling another one.
#[test]
fn mut_struct_methods_change_locals_and_fields() {
    let path = inline_program("mutmethods", MUT_METHODS_SRC);
    let expected = "local 14 4 copy 4 4 total 1440\n\
        tick 1: 5 32 total 820\ntick 2: 9 16 total 1060\ntick 3: 13 8 total 1380\ntick 4: 17 4 total 1740\n\
        tick 5: 21 2 total 2120\ntick 6: 25 1 total 2510\ntick 7: 29 0.5 total 2905";
    check_printed(&path, "mutmethods", "7", expected);
}
