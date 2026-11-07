//! Grids (Grid<T> = T[,], Grid3<T> = T[,,]): Step and Move against hand-written loops with the same
//! rules, the same results with 1 and 4 threads and under ASan/UBSan, 3D grids with Flood and Ray,
//! what explain says, and the rules for the lambdas.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sijoc-grid-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn sijoc(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_sijoc")).args(args).output().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn have_gcc() -> bool {
    Command::new("gcc").arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// builds `prog` to C, compiles it with `flags` and runs it with `threads` OpenMP threads
fn run(prog: &Path, d: &Path, tag: &str, flags: &[&str], threads: &str, ticks: &str) -> Option<String> {
    let c = d.join(format!("{}.c", tag));
    let (code, out) = sijoc(&["build", prog.to_str().unwrap(), "--lib", "-o", d.join("g.c").to_str().unwrap(), "--quiet"]);
    assert_eq!(code, 0, "{}", out);
    // the standalone program (main) for running: build without --lib
    let (code, out) = sijoc(&["build", prog.to_str().unwrap(), "-o", c.to_str().unwrap(), "--quiet"]);
    assert_eq!(code, 0, "{}", out);
    let exe = d.join(tag);
    let mut a: Vec<&str> = vec!["-std=gnu11", "-ffp-contract=off", "-w"];
    a.extend(flags);
    let cs = c.display().to_string();
    let es = exe.display().to_string();
    a.extend([cs.as_str(), "-o", es.as_str(), "-lm"]);
    let o = Command::new("gcc").args(&a).output().unwrap();
    if !o.status.success() {
        if flags.iter().any(|f| f.starts_with("-fsanitize")) {
            return None; // no sanitizer runtime here
        }
        panic!("gcc {:?} failed:\n{}", flags, String::from_utf8_lossy(&o.stderr));
    }
    let r = Command::new(&exe).args(["--ticks", ticks]).env("OMP_NUM_THREADS", threads).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&r.stdout), String::from_utf8_lossy(&r.stderr));
    assert!(r.status.success(), "{} failed:\n{}", tag, text);
    Some(text)
}

fn checksum(out: &str) -> String {
    out.lines().find(|l| l.starts_with("world checksum:")).unwrap_or("none").to_string()
}

#[test]
fn step_and_move_match_hand_written_loops_on_any_thread_count() {
    if !have_gcc() {
        eprintln!("skipping: no gcc");
        return;
    }
    let prog = root().join("tests/programs/grid_sand.sijo");
    let d = dir("sand");
    let one = run(&prog, &d, "one", &["-O2", "-fopenmp"], "1", "300").unwrap();
    let four = run(&prog, &d, "four", &["-O2", "-fopenmp"], "4", "300").unwrap();
    assert!(one.contains("tick 300: sand ") && one.contains("mismatches 0 "), "Step and Move must give what the loops give:\n{}", one);
    assert!(!one.lines().any(|l| l.contains("mismatches") && !l.contains("mismatches 0 ")), "{}", one);
    assert_eq!(checksum(&one), checksum(&four), "the same world with 1 and 4 threads");
    let printed = |s: &str| s.lines().filter(|l| l.starts_with("tick ")).collect::<Vec<_>>().join("\n");
    assert_eq!(printed(&one), printed(&four));
    for (tag, f) in [("asan", vec!["-O1", "-g", "-fsanitize=address,undefined", "-fno-sanitize-recover=undefined"]), ("asan-omp", vec!["-O1", "-g", "-fopenmp", "-fsanitize=address,undefined", "-fno-sanitize-recover=undefined"])] {
        if let Some(out) = run(&prog, &d, tag, &f, "4", "120") {
            assert!(!out.contains("runtime error") && !out.contains("ERROR: AddressSanitizer"), "{}:\n{}", tag, out);
            assert!(out.contains("tick 100: sand ") && out.contains("mismatches 0 "), "{}:\n{}", tag, out);
        }
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn three_d_grids_flood_and_ray() {
    if !have_gcc() {
        return;
    }
    let prog = root().join("tests/programs/grid3d.sijo");
    let d = dir("3d");
    let one = run(&prog, &d, "one", &["-O2", "-fopenmp"], "1", "30").unwrap();
    let four = run(&prog, &d, "four", &["-O2", "-fopenmp"], "4", "30").unwrap();
    // a 10x10 grid with a cross at x = 5 and y = 5: the 5x5 corner, then the 4x4 one
    assert!(one.contains("tick 30 flood 25016 bottom 320 ray 18 dims 20x16x12"), "{}", one);
    assert_eq!(checksum(&one), checksum(&four));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn explain_says_how_each_grid_operation_runs() {
    let (code, out) = sijoc(&["explain", root().join("tests/programs/grid_sand.sijo").to_str().unwrap()]);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("Step: map (each cell from itself), in place, cells in parallel; Random keyed by the cell"), "{}", out);
    assert!(out.contains("Move: proposals to the 8 neighbours resolved per cell (straight moves first)"), "{}", out);
    assert!(out.contains("Step: stencil (reads neighbours at offsets up to 1 x 1), into a second buffer, cells in parallel; 4 read(s) without bounds checks away from the edges"), "{}", out);
    assert!(out.contains("Ray: walks the cells of the line"), "{}", out);
}

#[test]
fn grid_lambdas_compute_values_only() {
    let d = dir("errors");
    let cases: &[(&str, &str)] = &[
        ("G.Step((x, y, c) => { Print(\"x\"); return c; });", "E0241"),
        ("G.Step((x, y, c) => c + Bump());", "E0707"),
        ("G.Step((x, y) => 1);", "E0241"),
        ("G.Move(0, (x, y, c) => Grid.Dir(2, 0));", "E0241"),
        ("G.Flood(1, 2);", "E0241"),
        ("G.Step((x, y, c) => Count += 1);", "E0707"),
    ];
    for (i, (stmt, want)) in cases.iter().enumerate() {
        let src = format!(
            "public class W : Entity\n{{\n    public mut Grid<int> G = new Grid<int>(4, 4);\n    public mut int Count;\n    public int Bump()\n    {{\n        Count += 1;\n        return 1;\n    }}\n    public void Update(float dt)\n    {{\n        {}\n    }}\n}}\npublic static class Game {{ public static void Start() {{ Spawn(new W {{ }}); }} }}\n",
            stmt
        );
        let f = d.join(format!("e{}.sijo", i));
        std::fs::write(&f, src).unwrap();
        let (code, out) = sijoc(&["check", f.to_str().unwrap()]);
        assert_ne!(code, 0, "{}: must be rejected:\n{}", stmt, out);
        assert!(out.contains(want), "{}: expected {}:\n{}", stmt, want, out);
    }
    let _ = std::fs::remove_dir_all(&d);
}
