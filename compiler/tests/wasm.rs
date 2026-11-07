//! `sijoc build --target wasm32`: the game as a WebAssembly module with its own C library and a JS
//! loader. Run in node: the same checksum as the native build of the same game (determinism across
//! targets), host functions from JS (strings, arrays, [Deferred] calls in order), field accessors,
//! save/load, and a panic that stops the game with an error instead of trapping the module.
//! Skipped without clang (with the wasm32 target and wasm-ld) or node.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sijoc-wasm-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// clang that links wasm32 modules, and node
fn toolchain() -> bool {
    let d = dir("probe");
    std::fs::write(d.join("t.c"), "int f(int x) { return x + 1; }\n").unwrap();
    let clang = Command::new("clang")
        .args(["--target=wasm32", "-nostdlib", "-Wl,--no-entry", "-Wl,--export=f", "t.c", "-o", "t.wasm"])
        .current_dir(&d)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let node = Command::new("node").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    if !clang || !node {
        eprintln!("skipping: needs clang with the wasm32 target and wasm-ld, and node");
    }
    clang && node
}

fn sijoc(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_sijoc")).args(args).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "sijoc {:?} failed:\n{}", args, text);
    text
}

fn node(d: &Path, script: &str) -> String {
    std::fs::write(d.join("t.mjs"), script).unwrap();
    let out = Command::new("node").arg("t.mjs").current_dir(d).output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "node failed:\n{}", text);
    text
}

/// the native library's checksum after `ticks` steps with `seed`
fn native_checksum(game: &Path, d: &Path, seed: u64, ticks: u32) -> Option<String> {
    let have_gcc = Command::new("gcc").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    if !have_gcc {
        return None;
    }
    let n = d.join("native");
    std::fs::create_dir_all(&n).unwrap();
    sijoc(&["build", game.to_str().unwrap(), "--lib", "-o", n.join("g.c").to_str().unwrap(), "--quiet"]);
    std::fs::write(
        n.join("host.c"),
        format!("#include <stdio.h>\n#include \"sijo_game.h\"\nint main(void) {{ SijoGame* g = sijo_game_create({seed}); for (int i = 0; i < {ticks}; i++) sijo_game_step(g); printf(\"%016llx\\n\", (unsigned long long)sijo_game_checksum(g)); return 0; }}\n"),
    )
    .unwrap();
    let ok = Command::new("gcc").args(["-std=gnu11", "-O2", "-ffp-contract=off", "-w", "g.c", "host.c", "-I.", "-lm", "-o", "host"]).current_dir(&n).status().unwrap().success();
    assert!(ok, "native build failed");
    let out = Command::new(n.join("host")).output().unwrap();
    Some(String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or("").to_string())
}

#[test]
fn wasm_build_matches_the_native_checksum() {
    if !toolchain() {
        return;
    }
    for ex in ["arena", "boids", "village"] {
        let d = dir(ex);
        let game = root().join(format!("../examples/{}.sijo", ex));
        let msg = sijoc(&["build", game.to_str().unwrap(), "--target", "wasm32", "-o", d.join("web/g.c").to_str().unwrap()]);
        for f in ["g.c", "sijo_game.h", "g.wasm", "g.js", "index.html"] {
            assert!(d.join("web").join(f).is_file(), "{} missing: {}", f, msg);
        }
        let out = node(
            &d,
            "const { load } = await import(\"./web/g.js\");\nconst game = (await load({ print() {} })).create(42n);\nfor (let i = 0; i < 240; i++) if (game.step() !== 1) throw new Error(game.error());\nconsole.log(game.checksum().toString(16).padStart(16, \"0\"));\n",
        );
        let wasm_sum = out.lines().last().unwrap_or("").to_string();
        if let Some(native) = native_checksum(&game, &d, 42, 240) {
            assert_eq!(wasm_sum, native, "{}: the wasm build must compute the same world as the native one", ex);
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}

#[test]
fn wasm_host_functions_fields_save_load_and_panics() {
    if !toolchain() {
        return;
    }
    let d = dir("api");
    let game = root().join("tests/programs/wasm_game.sijo");
    sijoc(&["build", game.to_str().unwrap(), "--target", "wasm32", "-o", d.join("web/p.c").to_str().unwrap(), "--quiet"]);
    let out = node(
        &d,
        r#"const { load } = await import("./web/p.js");
const log = [];
const host = {
  host_log: (s, cells) => log.push(`${s} [${Array.from(cells).join(",")}]`),
  host_shake: (who, a) => log.push(`shake ${who} ${a}`),
  host_key: (code) => code === 32,
};
const game = (await load(host)).create(7n);
for (let i = 0; i < 30; i++) if (game.step() !== 1) throw new Error("early stop");
const saved = game.save("s30"), sum30 = game.checksum();
let r = 1;
for (let i = 0; i < 30 && r === 1; i++) r = game.step();
console.log("panic", r, game.tick(), game.error());
console.log("again", game.step());
console.log("log", log.join(" | "));
console.log("fields", game.fields.World_count(), Array.from(game.fields.World_G(0)).join(","), game.fields.World_Name(0));
console.log("draws", JSON.stringify(game.draws()));
console.log("load", game.load(saved), game.tick(), game.checksum() === sum30, game.error(), game.step());
"#,
    );
    assert!(out.contains("panic 0 50 list index out of range (at "), "a panic stops the game with its message:\n{}", out);
    assert!(out.contains("wasm_game.sijo:18)"), "the message names the .sijo line:\n{}", out);
    assert!(out.contains("again 0"), "a stopped game does not step:\n{}", out);
    assert!(out.contains("log tick 0 [0,0,0,0,0,0,0,0] | shake 20 0.5 | tick 25 [0,25,50,75,100,125,150,175] | shake 40 0.5"), "host calls in order:\n{}", out);
    assert!(out.contains("fields 1 0,50,100,150,200,250,300,350 w"), "field accessors:\n{}", out);
    assert!(out.contains(r#"{"sprite":-1,"x":1,"y":1,"text":"space"}"#) && out.contains(r#"{"sprite":3,"x":2,"y":2,"text":null}"#), "draw list:\n{}", out);
    assert!(out.contains("load true 30 true null 1"), "loading a saved state restarts the stopped game:\n{}", out);
    let _ = std::fs::remove_dir_all(&d);
}
