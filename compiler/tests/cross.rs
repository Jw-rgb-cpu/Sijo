//! Cross-platform identity: the same Sijo program must print byte-identical output (every Print line,
//! the world checksum, the replicated state hash) when its generated C is built by another compiler,
//! for another architecture or operating system. Each target runs only when its toolchain is
//! installed, so this is skipped on machines without them:
//!   clang                                   (another compiler, -O3 -march=native)
//!   aarch64-linux-gnu-gcc + qemu-aarch64    (ARM64: fused multiply-add is the default there)
//!   s390x-linux-gnu-gcc + qemu-s390x        (big-endian)
//!   x86_64-w64-mingw32-gcc + wine64         (Windows, Winsock)
//! On Ubuntu: apt install clang qemu-user gcc-aarch64-linux-gnu gcc-s390x-linux-gnu mingw-w64 wine64

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn wine() -> Option<String> {
    ["wine64", "/usr/lib/wine/wine64", "wine"].iter().find(|w| Path::new(w).exists() || have(w)).map(|s| s.to_string())
}

struct Target {
    name: &'static str,
    cc: &'static str,
    flags: &'static [&'static str],
    libs: &'static [&'static str],
    runner: Option<String>,
    exe_suffix: &'static str,
}

fn targets() -> Vec<Target> {
    let mut v = vec![];
    if have("clang") {
        v.push(Target { name: "clang-O3-native", cc: "clang", flags: &["-O3", "-march=native"], libs: &["-lm"], runner: None, exe_suffix: "" });
    }
    if have("aarch64-linux-gnu-gcc") && have("qemu-aarch64") {
        v.push(Target { name: "aarch64", cc: "aarch64-linux-gnu-gcc", flags: &["-O2", "-static"], libs: &["-lm"], runner: Some("qemu-aarch64".into()), exe_suffix: "" });
    }
    if have("s390x-linux-gnu-gcc") && have("qemu-s390x") {
        v.push(Target { name: "s390x", cc: "s390x-linux-gnu-gcc", flags: &["-O2", "-static"], libs: &["-lm"], runner: Some("qemu-s390x".into()), exe_suffix: "" });
    }
    if let (true, Some(w)) = (have("x86_64-w64-mingw32-gcc"), wine()) {
        v.push(Target { name: "win64", cc: "x86_64-w64-mingw32-gcc", flags: &["-O2"], libs: &["-lm", "-lws2_32"], runner: Some(w), exe_suffix: ".exe" });
    }
    v
}

fn generate(src: &Path, dir: &Path) -> PathBuf {
    let text = std::fs::read_to_string(src).unwrap();
    let mut d = sijoc::diag::Diags::default();
    let c = sijoc::driver::compile(&text, &mut d).unwrap_or_else(|| panic!("{} failed to compile: {:?}", src.display(), d.list));
    let cfile = dir.join("prog.c");
    std::fs::write(&cfile, sijoc::codegen::generate(&c, &src.display().to_string())).unwrap();
    cfile
}

fn compile(cc: &str, flags: &[&str], libs: &[&str], cfile: &Path, exe: &Path) -> bool {
    let out = Command::new(cc).args(["-std=gnu11", "-w"]).args(flags).arg(cfile).arg("-o").arg(exe).args(libs).output().unwrap();
    if !out.status.success() {
        panic!("{} failed:\n{}", cc, String::from_utf8_lossy(&out.stderr));
    }
    true
}

fn run(runner: Option<&str>, exe: &Path, args: &[&str]) -> String {
    let mut cmd = match runner {
        Some(r) => {
            let mut c = Command::new(r);
            c.arg(exe);
            c
        }
        None => Command::new(exe),
    };
    let out = cmd.args(args).env("WINEDEBUG", "-all").env("OMP_NUM_THREADS", "1").output().unwrap();
    assert!(out.status.success(), "{} failed: {}", exe.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn programs_print_identical_output_on_every_platform() {
    let ts = targets();
    if ts.is_empty() {
        eprintln!("skipping: no cross toolchain installed");
        return;
    }
    let programs: &[(&str, &[&str])] = &[
        ("../examples/arena.sijo", &["--ticks", "300", "--quiet"]),
        ("../examples/boids.sijo", &["--ticks", "60", "--quiet"]),
        ("../examples/village.sijo", &["--ticks", "300", "--quiet"]),
        ("tests/programs/semantics.sijo", &["--ticks", "2", "--no-summary"]),
        ("tests/programs/formatting.sijo", &["--ticks", "1", "--no-summary"]),
        ("tests/programs/strings.sijo", &["--ticks", "120"]),
        ("tests/programs/dictionaries.sijo", &["--ticks", "60"]),
        ("tests/programs/coroutines.sijo", &["--ticks", "130"]),
        ("tests/programs/spatial.sijo", &["--ticks", "30", "--quiet"]),
        ("tests/programs/chars.sijo", &["--ticks", "60"]),
        ("tests/programs/arrays.sijo", &["--ticks", "30"]),
        ("tests/programs/nested.sijo", &["--ticks", "18"]),
        ("tests/programs/messages.sijo", &["--ticks", "10"]),
        ("tests/programs/rpcstrings.sijo", &["--ticks", "100"]),
        ("tests/programs/hoisting.sijo", &["--ticks", "30"]),
        ("tests/programs/targets.sijo", &["--ticks", "150", "--sim-loss", "0.2"]),
        ("tests/programs/reflists.sijo", &["--ticks", "150", "--sim-loss", "0.2"]),
        ("tests/programs/biglists.sijo", &["--ticks", "120", "--sim-loss", "0.2"]),
        ("tests/programs/lambdas.sijo", &["--ticks", "10"]),
        ("tests/programs/spawnhandles.sijo", &["--ticks", "40"]),
        ("tests/programs/awaitcalls.sijo", &["--ticks", "30"]),
        ("tests/programs/genclasses.sijo", &["--ticks", "12"]),
        ("tests/programs/components.sijo", &["--ticks", "12"]),
        ("tests/programs/mdarrays.sijo", &["--ticks", "30"]),
        ("tests/programs/msglists.sijo", &["--ticks", "12"]),
    ];
    for (rel, args) in programs {
        let src = root().join(rel);
        let stem = src.file_stem().unwrap().to_string_lossy().to_string();
        let dir = std::env::temp_dir().join(format!("sijoc-cross-{}-{}", std::process::id(), stem));
        std::fs::create_dir_all(&dir).unwrap();
        let cfile = generate(&src, &dir);
        let native = dir.join("native");
        compile("gcc", &["-O2"], &["-lm"], &cfile, &native);
        let reference = run(None, &native, args);
        for t in &ts {
            let exe = dir.join(format!("{}{}", t.name, t.exe_suffix));
            compile(t.cc, t.flags, t.libs, &cfile, &exe);
            let out = run(t.runner.as_deref(), &exe, args);
            assert!(out == reference, "{}: output on {} differs from x86-64 gcc -O2:\n--- x86-64\n{}\n--- {}\n{}", stem, t.name, reference, t.name, out);
        }
    }
}
