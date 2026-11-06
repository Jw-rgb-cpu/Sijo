//! Compiler intelligence: value ranges, the e-graph optimizer, profile-guided decisions and
//! task parallelism between systems. Every optimization here must leave program output and world
//! checksums bit-identical; most tests build the same program with and without the optimization.

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
    let d = std::env::temp_dir().join(format!("sijoc-intel-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// C source of a program (panics on compile errors)
fn gen(src: &str, name: &str) -> String {
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).unwrap_or_else(|| panic!("{} failed to compile: {:?}", name, d.list));
    sijoc::codegen::generate(&c, name)
}

/// Compile C source to an executable; None when no C compiler (or the requested feature) exists.
fn build_c(csrc: &str, tag: &str, extra: &[&str]) -> Option<PathBuf> {
    let cc = cc()?;
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
        if extra.iter().any(|f| f.starts_with("-fsanitize") || *f == "-fopenmp") {
            return None;
        }
        panic!("C compilation failed ({}):\n{}", tag, String::from_utf8_lossy(&out.stderr));
    }
    Some(exe)
}

fn run(exe: &Path, args: &[&str], threads: usize) -> (i32, String) {
    let out = Command::new(exe).args(args).env("OMP_NUM_THREADS", threads.to_string()).output().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn printed(out: &str) -> String {
    out.lines().take_while(|l| !l.starts_with("== sijo")).collect::<Vec<_>>().join("\n")
}

fn line<'a>(out: &'a str, prefix: &str) -> &'a str {
    out.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no '{}' line in output:\n{}", prefix, out))
}

/// A range type holds from the first value on: a field without an initializer starts at the
/// default value saturated into its range, and loading a saved world (live coding after the
/// range was narrowed) saturates too.
#[test]
fn range_contract_holds_for_defaults_and_loaded_state() {
    let prog = |range: &str| {
        format!(
            "public class Unit : Entity\n{{\n    public mut int in {range} A;\n    public int in -3..-1 B;\n    public mut V in 2..9 P;\n    public void Update(float dt)\n    {{\n        if (Time.Tick == 1) {{ Print($\"A={{A}} B={{B}} P={{P.X}},{{P.Y}}\"); }}\n        A = A + 40;\n    }}\n}}\npublic struct V {{ public int X; public float Y; }}\npublic static class Game\n{{\n    public static void Start() {{ Spawn(new Unit {{ }}); }}\n}}\n"
        )
    };
    let wide = prog("5..100");
    let Some(exe) = build_c(&gen(&wide, "wide.sijo"), "range-wide", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "2"], 1);
    assert_eq!(code, 0, "{}", out);
    assert_eq!(printed(&out).lines().last().unwrap(), "A=5 B=-1 P=2,2", "{}", out);
    // after two ticks A = 85; a program whose range is 5..50 loads it as 50
    let dir = out_dir("range-state");
    let st = dir.join("state.txt").display().to_string();
    let (code, out) = run(&exe, &["--ticks", "2", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0, "{}", out);
    let narrow = prog("5..50").replace("if (Time.Tick == 1)", "if (Time.Tick == 3)");
    let exe2 = build_c(&gen(&narrow, "narrow.sijo"), "range-narrow", &[]).unwrap();
    let (code, out) = run(&exe2, &["--ticks", "1", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", out);
    assert!(printed(&out).contains("A=50 B=-1"), "{}", out);
}

/// Build a program through the command line (a separate process: `--checked` and the optimizer
/// switches are process-wide), returning the C source and the executable.
fn cli_build(src_path: &Path, tag: &str, flags: &[&str]) -> Option<(String, PathBuf)> {
    cc()?;
    let dir = out_dir(tag);
    let (cfile, exe) = (dir.join("prog.c"), dir.join("prog"));
    let out = Command::new(env!("CARGO_BIN_EXE_sijoc"))
        .arg("build")
        .arg(src_path)
        .args(flags)
        .arg("-o")
        .arg(&cfile)
        .arg("--exe")
        .arg(&exe)
        .output()
        .unwrap();
    assert!(out.status.success(), "sijoc build {:?} failed:\n{}{}", flags, String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Some((std::fs::read_to_string(&cfile).unwrap(), exe))
}

/// The e-graph and the value-range facts change the generated code, never the results: the
/// range program prints its expected output with and without them, on 1 and 4 threads, and the
/// optimized C has fewer bounds checks and saturations.
#[test]
fn egraph_and_value_ranges_keep_results() {
    let path = root().join("tests/programs/ranges.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/ranges.expected")).unwrap();
    let Some((opt_c, opt)) = cli_build(&path, "ranges-opt", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (plain_c, plain) = cli_build(&path, "ranges-plain", &["--no-egraph"]).unwrap();
    let mut sums = vec![];
    for (exe, threads) in [(&opt, 1), (&opt, 4), (&plain, 1), (&plain, 4)] {
        let (code, out) = run(exe, &["--ticks", "60"], threads);
        assert_eq!(code, 0, "{}", out);
        assert_eq!(printed(&out).trim(), expected.trim(), "threads={} {}", threads, exe.display());
        sums.push(line(&out, "world checksum:").to_string());
    }
    assert!(sums.windows(2).all(|w| w[0] == w[1]), "{:?}", sums);
    let count = |c: &str| c.matches("SIJO_IDX(").count() + c.matches("sijo_clampi(").count();
    assert!(count(&opt_c) + 6 <= count(&plain_c), "bounds checks + saturations: {} optimized vs {} plain", count(&opt_c), count(&plain_c));
    // (a + MAX) + 1 - a wraps to MIN whatever a is
    let mixed = opt_c.split("static int32_t F3_Calc_Mixed(SijoWorld* w, int32_t v0_a) {").nth(1).unwrap();
    assert!(mixed[..mixed.find("\n}").unwrap()].contains("return INT32_MIN;"), "{}", &mixed[..300.min(mixed.len())]);
    // the explain report names what the e-graph did
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    let report = sijoc::explain::report(&c, &sijoc::diag::SourceMap::single("ranges.sijo", &src));
    for needle in ["e-graph (equality saturation)", "rem-pow2", "2 condition(s) decided", "2 bounds check(s) removed"] {
        assert!(report.contains(needle), "explain is missing '{}':\n{}", needle, report);
    }
}

/// Under --checked an overflow traps, so the e-graph must not fold it away: (a + MAX) - MAX is
/// `a` in wrapping arithmetic but traps for a > 0. Both builds stop at the same place. (Without
/// --checked, `big - 1 - a` with `big = a + MAX` folds to MAX - 1 through the local's definition.)
#[test]
fn checked_builds_keep_overflow_traps_through_the_egraph() {
    let dir = out_dir("checked-egraph");
    let path = dir.join("chk.sijo");
    std::fs::write(
        &path,
        "public static class M\n{\n    public static int Wrap(int a) => (a + 2147483647) - 2147483647;\n    public static int Mixed(int a)\n    {\n        int big = a + 2147483647;\n        return big - 1 - a;\n    }\n}\npublic static class Game\n{\n    public static void Start() { }\n    public static void Update(float dt)\n    {\n        int z = Time.Tick - 1;\n        Print($\"wrap {M.Wrap(z)}\");\n        Print($\"mixed {M.Mixed(z - 5)}\");\n        Print($\"wrap {M.Wrap(z + 1)}\");\n    }\n}\n",
    )
    .unwrap();
    let Some((_, opt)) = cli_build(&path, "checked-opt", &["--checked"]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (_, plain) = cli_build(&path, "checked-plain", &["--checked", "--no-egraph"]).unwrap();
    let (c1, o1) = run(&opt, &["--ticks", "1"], 1);
    let (c2, o2) = run(&plain, &["--ticks", "1"], 1);
    assert_ne!(c1, 0, "{}", o1);
    assert_eq!(c1, c2);
    assert!(o1.contains("wrap 0\nmixed 2147483646\n") && o1.contains("overflow") && o1.contains("chk.sijo:3"), "{}", o1);
    assert_eq!(printed(&o1), printed(&o2));
    // unchecked, the same program wraps
    let (_, wrapping) = cli_build(&path, "checked-wrap", &[]).unwrap();
    let (code, out) = run(&wrapping, &["--ticks", "1"], 1);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("wrap 0\nmixed 2147483646\nwrap 1\n"), "{}", out);
}

/// A method a client calls over RPC saturates its ranged parameters on the server: the client is
/// not trusted to have applied the contract (the optimizer relies on parameter ranges).
#[test]
fn rpc_arguments_saturate_on_the_server() {
    let src = "public class Ship : Entity\n{\n    public mut int Fuel;\n    public void Refuel(int in 0..10 amount) { Fuel += amount; }\n    public client void Input() { Refuel(3); }\n}\npublic static class Game\n{\n    public static void Start() { Spawn(new Ship { }); }\n}\n";
    let c = gen(src, "rpc.sijo");
    let exec = c.split("static void sijo_exec_call(").nth(1).unwrap();
    assert!(exec.contains("= sijo_clampi(a"), "{}", &exec[..exec.len().min(1500)]);
}

/// Distance tests written with a square root (`(o.Pos - Pos).Length < R`, a static Distance
/// helper compared with this entity's radius) use the proximity grid, with the same results as the
/// full scan on any number of threads.
#[test]
fn sqrt_distance_loops_use_the_grid() {
    let path = root().join("tests/programs/sqrtnear.sijo");
    let src = std::fs::read_to_string(&path).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    assert_eq!(c.spatial.loops.len(), 2, "rejected: {:?}", c.spatial.rejected);
    assert!(c.spatial.loops.iter().any(|l| (l.radius - 1.5).abs() < 1e-9));
    assert!(c.spatial.loops.iter().any(|l| !l.dyn_r2.is_empty()));
    let csrc = sijoc::codegen::generate(&c, "sqrtnear.sijo");
    let Some(grid) = build_c(&csrc, "sqrtnear-grid", &["-fopenmp"]).or_else(|| build_c(&csrc, "sqrtnear-grid1", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let scan = build_c(&csrc, "sqrtnear-scan", &["-DSIJO_NO_SPATIAL"]).unwrap();
    let (code, a) = run(&grid, &["--ticks", "60"], 4);
    assert_eq!(code, 0, "{}", a);
    let (_, b) = run(&grid, &["--ticks", "60"], 1);
    let (_, s) = run(&scan, &["--ticks", "60"], 1);
    assert!(printed(&a).contains("tick 60: near"), "{}", a);
    assert_eq!(printed(&a), printed(&s));
    for out in [&a, &b] {
        assert_eq!(line(out, "world checksum:"), line(&s, "world checksum:"));
    }
}

/// Profile-guided optimization: a run with `--profile FILE` writes per-system costs; a build with
/// `--profile-use FILE` sets each system's parallel threshold from them and gives the fields a hot
/// query loop reads per pair a component of their own. Same results with and without the profile.
#[test]
fn profile_guided_build_keeps_results() {
    let path = root().join("../bench/shooter/shooter.sijo");
    let Some((_, plain)) = cli_build(&path, "pgo-plain", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let dir = out_dir("pgo-profile");
    let prof = dir.join("shooter.profile");
    let (code, out) = run(&plain, &["--ticks", "60", "--quiet", "--profile", prof.to_str().unwrap()], 4);
    assert_eq!(code, 0, "{}", out);
    let p = sijoc::profile::Profile::read(&prof).unwrap();
    let ship = p.system("Ship.Update").expect("Ship.Update profiled");
    assert_eq!(ship.calls, 60);
    assert!(ship.rows >= 60 * 500 && ship.ns > 0, "{:?}", ship);
    assert!(p.system("Game.Update").is_some());
    // the plan with the profile
    let src = std::fs::read_to_string(&path).unwrap();
    let tune = sijoc::driver::Tuning { profile: Some(p.clone()), ..Default::default() };
    let c = sijoc::driver::compile_tuned(&src, &mut Diags::default(), &sijoc::driver::Options::default(), &tune).unwrap();
    let field = |n: &str| c.prog.fields.iter().position(|f| f.name == n && c.prog.classes[f.class].name == "Ship").unwrap();
    assert_ne!(c.ecs.comp_of(field("Pos")), c.ecs.comp_of(field("Hp")), "Ship.Pos (read per pair in the hot loops) gets its own record");
    let ship_sys = c.ecs.systems.iter().find(|s| s.name == "Ship.Update").unwrap();
    assert!(ship_sys.pgo.as_deref().unwrap_or("").contains(&format!("parallel from {} rows", ship_sys.par_min)), "{:?}", ship_sys.pgo);
    let report = sijoc::explain::report(&c, &sijoc::diag::SourceMap::single("shooter.sijo", &src));
    assert!(report.contains("profile:") && report.contains("pgo: Ship: { Pos }"), "{}", report);
    // without a profile the layout is the usual one
    let c0 = sijoc::driver::compile(&src, &mut Diags::default()).unwrap();
    let f0 = |n: &str| c0.prog.fields.iter().position(|f| f.name == n && c0.prog.classes[f.class].name == "Ship").unwrap();
    assert_eq!(c0.ecs.comp_of(f0("Pos")), c0.ecs.comp_of(f0("Hp")));
    // same results
    let (_, guided) = cli_build(&path, "pgo-guided", &["--profile-use", prof.to_str().unwrap()]).unwrap();
    let (_, a) = run(&plain, &["--ticks", "90", "--quiet"], 1);
    for t in [1, 4] {
        let (code, b) = run(&guided, &["--ticks", "90", "--quiet"], t);
        assert_eq!(code, 0, "{}", b);
        assert_eq!(line(&a, "world checksum:"), line(&b, "world checksum:"), "threads={}", t);
    }
}

/// Systems of one stage that are each too small for a parallel loop run side by side as tasks:
/// same output and checksum on 1 thread (one after another) and on 4, and as the reference compiler
/// gives without task groups.
#[test]
fn task_groups_keep_results() {
    let path = root().join("tests/programs/tasks.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/tasks.expected")).unwrap();
    let src = std::fs::read_to_string(&path).unwrap();
    let c = sijoc::driver::compile(&src, &mut Diags::default()).unwrap();
    let groups = sijoc::ecs::task_groups(&c.ecs, &c.spatial);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].members.len(), 6, "{:?}", groups);
    let csrc = sijoc::codegen::generate(&c, "tasks.sijo");
    assert!(csrc.contains("sijo_group0(w);"));
    let Some(exe) = build_c(&csrc, "tasks-omp", &["-fopenmp"]).or_else(|| build_c(&csrc, "tasks", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sums = vec![];
    for t in [1, 2, 4] {
        let (code, out) = run(&exe, &["--ticks", "100"], t);
        assert_eq!(code, 0, "{}", out);
        assert_eq!(printed(&out).trim(), expected.trim(), "threads={}", t);
        sums.push(line(&out, "world checksum:").to_string());
    }
    assert!(sums.windows(2).all(|w| w[0] == w[1]), "{:?}", sums);
    if let Some(asan) = build_c(&csrc, "tasks-asan", &["-fopenmp", "-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "60", "--quiet"], 4);
        assert_eq!(code, 0, "tasks.sijo failed under sanitizers:\n{}", out);
    }
}

/// PGO with field counts: a `--profile-fields` build counts every entity field access and writes
/// the counts to the profile; `--profile-use` then gives the busiest fields of a record a record of
/// their own. Counting and the new layout change nothing in the results.
#[test]
fn field_access_counts_guide_the_layout() {
    let path = root().join("../bench/shooter/shooter.sijo");
    let Some((counted_c, counted)) = cli_build(&path, "pgo-fields", &["--profile-fields"]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    assert!(counted_c.contains("SIJO_FC("), "the counted build instruments field accesses");
    let (plain_c, plain) = cli_build(&path, "pgo-fields-plain", &[]).unwrap();
    assert!(!plain_c.contains("SIJO_FC("), "an ordinary build does not");
    let dir = out_dir("pgo-fields-profile");
    let prof = dir.join("shooter.profile");
    let (code, out) = run(&counted, &["--ticks", "60", "--quiet", "--profile", prof.to_str().unwrap()], 4);
    assert_eq!(code, 0, "{}", out);
    let (_, base) = run(&plain, &["--ticks", "60", "--quiet"], 1);
    assert_eq!(line(&out, "world checksum:"), line(&base, "world checksum:"), "counting changes nothing");
    let p = sijoc::profile::Profile::read(&prof).unwrap();
    let pos = p.field_count("Ship.Pos").expect("Ship.Pos counted");
    let hp = p.field_count("Ship.Hp").expect("Ship.Hp counted");
    assert!(pos > 8 * hp && hp > 0, "Ship.Pos {} vs Ship.Hp {}", pos, hp);
    // the plan: the busy field leaves the record of the rarely used one
    let src = std::fs::read_to_string(&path).unwrap();
    let tune = sijoc::driver::Tuning { profile: Some(p.clone()), ..Default::default() };
    let c = sijoc::driver::compile_tuned(&src, &mut Diags::default(), &sijoc::driver::Options::default(), &tune).unwrap();
    let field = |n: &str| c.prog.fields.iter().position(|f| f.name == n && c.prog.classes[f.class].name == "Ship").unwrap();
    assert_ne!(c.ecs.comp_of(field("Pos")), c.ecs.comp_of(field("Hp")));
    assert!(c.ecs.pgo_notes.iter().any(|n| n.contains("accesses in the profile")), "{:?}", c.ecs.pgo_notes);
    let (_, guided) = cli_build(&path, "pgo-fields-guided", &["--profile-use", prof.to_str().unwrap()]).unwrap();
    for t in [1, 4] {
        let (code, b) = run(&guided, &["--ticks", "60", "--quiet"], t);
        assert_eq!(code, 0, "{}", b);
        assert_eq!(line(&base, "world checksum:"), line(&b, "world checksum:"), "threads={}", t);
    }
}

/// GPU offloading: `sijoc build --gpu` turns the row loops of pure self-write systems into OpenMP
/// target regions (the reached methods compiled for the device too). Wherever they run (a device
/// if the compiler has an offload target and the machine one, else the host threads), the results
/// are the ones of the ordinary build.
#[test]
fn gpu_offloaded_systems_keep_results() {
    let path = root().join("tests/programs/ranges.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/ranges.expected")).unwrap();
    let src = std::fs::read_to_string(&path).unwrap();
    let c = sijoc::driver::compile(&src, &mut Diags::default()).unwrap();
    let cands: Vec<String> = (0..c.ecs.systems.len()).filter(|&i| sijoc::ecs::gpu_candidate(&c.prog, &c.ecs, i).is_ok()).map(|i| c.ecs.systems[i].name.clone()).collect();
    assert_eq!(cands, vec!["Unit.Update".to_string()]);
    // every table offloads (SIJO_GPU_MIN=1): the device path runs from the first tick
    let Some((gpu_c, gpu)) = cli_build(&path, "gpu", &["--gpu", "--cflags", "-DSIJO_GPU_MIN=1"]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    assert!(gpu_c.contains("#pragma omp target teams distribute parallel for map(to: w[0:1])"), "an offloaded row loop");
    assert!(gpu_c.contains("#pragma omp declare target"), "the methods it calls are compiled for the device");
    let (plain_c, plain) = cli_build(&path, "gpu-plain", &[]).unwrap();
    assert!(!plain_c.contains("omp target"), "an ordinary build has no target regions");
    let (_, base) = run(&plain, &["--ticks", "60"], 1);
    for t in [1, 4] {
        let (code, out) = run(&gpu, &["--ticks", "60"], t);
        assert_eq!(code, 0, "{}", out);
        assert_eq!(printed(&out).trim(), expected.trim(), "threads={}", t);
        assert_eq!(line(&out, "world checksum:"), line(&base, "world checksum:"), "threads={}", t);
    }
}

/// A partitioned system's row function has two loops (all rows, and on a shard the rows it owns),
/// so the method has two call sites; it is marked always_inline, otherwise the C compiler keeps it
/// out of line and the loop over all rows pays a call per entity.
#[test]
fn partitioned_system_methods_are_inlined_into_their_row_loops() {
    let src = std::fs::read_to_string(root().join("../bench/particles/particles.sijo")).unwrap();
    let c = gen(&src, "particles.sijo");
    let decl = c.lines().find(|l| l.contains("_Particle_Update__") && l.ends_with(");")).expect("the method's prototype");
    assert!(decl.starts_with("static inline __attribute__((always_inline)) void "), "{}", decl);
    assert!(c.contains("if (w->sh_on) { /* a shard: only the rows it owns */"), "the row function keeps its shard loop");
}
