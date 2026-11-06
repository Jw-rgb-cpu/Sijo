//! The embedding API (`sijoc build --lib`): a C host and a C++ host drive generated games.
//! Checks load/save, rejected state files, panics caught as errors (also on OpenMP worker threads
//! and with more OpenMP threads than the runtime's per-thread tables), re-entry, leaks (RSS, and
//! LeakSanitizer when available), the C++ header, and two games with different API prefixes.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn sijoc(args: &[&str]) {
    let out = Command::new(env!("CARGO_BIN_EXE_sijoc")).args(args).output().unwrap();
    assert!(out.status.success(), "sijoc {:?} failed:\n{}{}", args, String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sijoc-embed-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cc(args: &[&str], cwd: &Path) -> bool {
    let out = Command::new("gcc").args(args).current_dir(cwd).output().unwrap();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if args.iter().any(|a| a.starts_with("-fsanitize") || *a == "-fopenmp") {
            return false; // optional toolchain feature
        }
        panic!("gcc {:?} failed:\n{}", args, err);
    }
    true
}

fn run(exe: &Path, args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> (i32, String) {
    let mut c = Command::new(exe);
    c.args(args).current_dir(cwd);
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

#[test]
fn c_host_save_load_panics_and_leaks() {
    if !have("gcc") {
        eprintln!("skipping: no gcc");
        return;
    }
    let d = dir("c");
    let game = root().join("tests/programs/embed_api.sijo");
    let host = root().join("tests/programs/embed_host.c");
    sijoc(&["build", game.to_str().unwrap(), "--lib", "-o", d.join("embed_api.c").to_str().unwrap()]);
    let (g, h) = (d.join("embed_api.c").display().to_string(), host.display().to_string());
    let omp = cc(&["-std=gnu11", "-O2", "-fopenmp", "-I.", &g, &h, "-o", "host_omp", "-lm"], &d);
    if !omp {
        cc(&["-std=gnu11", "-O2", "-I.", &g, &h, "-o", "host_omp", "-lm"], &d);
    }
    for (threads, env) in [("1", vec![]), ("4", vec![]), ("0", vec![("OMP_NUM_THREADS", "80")])] {
        let (code, out) = run(&d.join("host_omp"), &[threads], &d, &env);
        assert_eq!(code, 0, "threads {} {:?}:\n{}", threads, env, out);
        assert!(out.contains("all embedding checks passed") && out.contains("list index out of range (at"), "{}", out);
    }
    if cc(&["-std=gnu11", "-O1", "-g", "-fopenmp", "-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-I.", &g, &h, "-o", "host_asan", "-lm"], &d) {
        let (code, out) = run(&d.join("host_asan"), &["4"], &d, &[("ASAN_OPTIONS", "detect_leaks=1")]);
        assert_eq!(code, 0, "under ASan/UBSan/LSan:\n{}", out);
        assert!(!out.contains("LeakSanitizer") && !out.contains("runtime error"), "{}", out);
    }
}

#[test]
fn cpp_host_links_two_prefixed_games() {
    if !have("gcc") || !have("g++") {
        eprintln!("skipping: no gcc/g++");
        return;
    }
    let d = dir("cpp");
    let game = root().join("tests/programs/embed_api.sijo");
    for p in ["game_a", "game_b"] {
        sijoc(&["build", game.to_str().unwrap(), "--lib", "--lib-prefix", p, "-o", d.join(format!("{}.c", p)).to_str().unwrap()]);
        cc(&["-std=gnu11", "-O2", "-Wall", "-Wextra", "-Werror", "-c", &format!("{}.c", p), "-o", &format!("{}.o", p)], &d);
    }
    let src = root().join("tests/programs/embed_two.cpp");
    let out = Command::new("g++").args(["-std=c++17", "-O2", "-Wall", "-Wextra", "-pedantic", "-Werror", "-I."]).arg(&src).args(["game_a.o", "game_b.o", "-o", "two", "-lm"]).current_dir(&d).output().unwrap();
    assert!(out.status.success(), "C++ host failed to build:\n{}", String::from_utf8_lossy(&out.stderr));
    let (code, text) = run(&d.join("two"), &[], &d, &[]);
    assert_eq!(code, 0, "{}", text);
    assert!(text.contains("same"), "{}", text);
}

#[test]
fn externs_keep_the_standalone_main() {
    if !have("gcc") {
        eprintln!("skipping: no gcc");
        return;
    }
    let d = dir("ext");
    let game = root().join("tests/programs/embed_api.sijo");
    let ext = d.join("ext.c");
    std::fs::write(&ext, "#include <stdint.h>\nint32_t embt_panic_tick(void) { return -1; }\nvoid embt_poke(int32_t t) { (void)t; }\n").unwrap();
    // without a definition of the extern functions: a clear error instead of a linker error
    let out = Command::new(env!("CARGO_BIN_EXE_sijoc")).args(["build", game.to_str().unwrap(), "--exe", d.join("x").to_str().unwrap()]).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("--externs"), "{}", String::from_utf8_lossy(&out.stderr));
    sijoc(&["build", game.to_str().unwrap(), "--externs", ext.to_str().unwrap(), "-o", d.join("g.c").to_str().unwrap(), "--exe", d.join("g").to_str().unwrap()]);
    let (code, out) = run(&d.join("g"), &["--ticks", "30", "--quiet"], &d, &[]);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("world checksum:"), "{}", out);
}

#[test]
fn draw_order_is_identical_after_a_restore() {
    if !have("gcc") {
        eprintln!("skipping: no gcc");
        return;
    }
    let d = dir("order");
    let game = root().join("tests/programs/embed_order.sijo");
    sijoc(&["build", game.to_str().unwrap(), "--lib", "-o", d.join("embed_order.c").to_str().unwrap()]);
    let host = root().join("tests/programs/embed_order.c");
    cc(&["-std=gnu11", "-O2", "-I.", "embed_order.c", host.to_str().unwrap(), "-o", "order", "-lm"], &d);
    let (code, out) = run(&d.join("order"), &[], &d, &[]);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("draw order identical after restore"), "{}", out);
}
