//! Tests for the developer tools: linter (+ autofix), language server, live-coding state hand-over.

use sijoc::diag::Diags;
use sijoc::json::Json;
use sijoc::lint::{lint, LintConfig, Preset};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn codes(src: &str, preset: Preset) -> Vec<&'static str> {
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).unwrap_or_else(|| panic!("does not compile: {:?}", d.list));
    let cfg = LintConfig { preset, ..LintConfig::default() };
    lint(&c, src, &cfg).into_iter().map(|l| l.code).collect()
}

const STYLE_SAMPLE: &str = r#"
public struct Vec2 { public float X; public float Y; }
public enum Team { Red, Blue }
public static class Rules
{
    public const int MaxHp = 100;
    [Recursive] public static int Fact(int Num) { if (Num <= 1) { return 1; } return Num * Fact(Num - 1); }
}
public class Ship : Entity
{
    public mut float Speed;
    bool Moving;
    public int Unused;
    public void Update(float dt)
    {
        var count = Count<Ship>();
        bool ok = count > 3;
        if (ok == true) { Speed += 1; }
        int a = 3;
        int b = 2;
        float half = a / b;
        if (Speed == half)
        {
            Speed = 0;
        }
        int never = 5;
        Steer();
    }
    void Steer() { }
    int Helper(int unusedParam) { return 1; }
}
public static class Game { public static void Start() { } }
"#;

#[test]
fn standard_rules_fire() {
    let c = codes(STYLE_SAMPLE, Preset::Standard);
    for code in [
        "L102", // struct Vec2 -> SVec2
        "L103", // enum Team -> ETeam
        "L106", // MaxHp -> MAX_HP
        "L104", // parameter Num -> num
        "L105", // private Steer -> steer
        "L107", // bool ok -> bOk, private bool -> mb
        "L109", // private field m-prefix
        "L113", // a, b are not descriptive
        "L204", // Allman braces
        "L202", // var count
        "L307", // ok == true
        "L305", // integer division into float
        "L306", // float equality
        "L301", // never is never read
        "L302", // unusedParam
        "L303", // Helper is never called
        "L304", // Unused field is never read
    ] {
        assert!(c.contains(&code), "expected {} in {:?}", code, c);
    }
}

#[test]
fn dotnet_preset_uses_dotnet_naming() {
    let c = codes(STYLE_SAMPLE, Preset::Dotnet);
    for code in ["L102", "L103", "L107", "L113", "L202"] {
        assert!(!c.contains(&code), "{} should be off in the dotnet preset: {:?}", code, c);
    }
    assert!(c.contains(&"L109"), "private fields should be _camelCase in .NET style");
    let ok = "public class A : Entity { private mut int _count; public void Update(float dt) { _count += 1; Print(_count); } }
              public static class Game { public static void Start() { } }";
    assert!(!codes(ok, Preset::Dotnet).contains(&"L109"));
}

#[test]
fn suppression_comments() {
    let src = "public struct Vec2 { public float X; } // sijo-lint: disable-line L102
               // sijo-lint: disable L204
               public static class Game { public static void Start() { int x = 1; if (x > 0) { Print(x); } } }";
    let c = codes(src, Preset::Standard);
    assert!(!c.contains(&"L102") && !c.contains(&"L204"), "{:?}", c);
    // without the comments both rules fire
    let plain: String = src.lines().map(|l| l.split("// sijo-lint").next().unwrap()).collect::<Vec<_>>().join("\n");
    let c = codes(&plain, Preset::Standard);
    assert!(c.contains(&"L102") && c.contains(&"L204"), "{:?}", c);
}

#[test]
fn config_file_parsing() {
    let cfg = LintConfig::parse("preset = dotnet\nL204 = off\nunused-local = error # by name\nmax-complexity = 7\n").unwrap();
    assert_eq!(cfg.preset, Preset::Dotnet);
    assert_eq!(cfg.level("L204"), None);
    assert_eq!(cfg.level("L301"), Some(sijoc::diag::Level::Error));
    assert_eq!(cfg.max_complexity, 7);
    assert!(LintConfig::parse("L999 = off").is_err());
    // every key names a rule, a preset or a setting
    for bad in ["L201 = off", "braces = off", "L110 = warning"] {
        assert!(LintConfig::parse(bad).unwrap_err().contains("unknown rule"), "{}", bad);
    }
}

/// `.sijolint` is read the same way by lint, check and run: its levels apply to every command, and
/// an unknown rule stops every command
#[test]
fn sijolint_is_read_the_same_way_by_every_command() {
    let dir = std::env::temp_dir().join(format!("sijolint-cmds-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("a.sijo");
    std::fs::write(&file, "public static class Game\n{\n    public static void Start()\n    {\n        mut int count = 3;\n        Print($\"{count}\");\n    }\n}\n").unwrap();
    let sijoc = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_sijoc")).args(args).arg(&file).output().unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stderr).to_string())
    };
    std::fs::write(dir.join(".sijolint"), "L204 = off\nL316 = error\n").unwrap();
    for cmd in [vec!["lint"], vec!["check"]] {
        let (code, err) = sijoc(&cmd);
        assert!(err.contains("error[L316]"), "{:?}: {}", cmd, err);
        assert_eq!(code, 1, "{:?}: {}", cmd, err);
    }
    std::fs::write(dir.join(".sijolint"), "L999 = off\nL316 = error\n").unwrap();
    for cmd in [vec!["lint"], vec!["check"], vec!["run"], vec!["build"]] {
        let (code, err) = sijoc(&cmd);
        assert_eq!(code, 2, "{:?}: {}", cmd, err);
        assert!(err.contains("unknown rule 'L999'"), "{:?}: {}", cmd, err);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn naming_helpers() {
    use sijoc::lint::{to_camel, to_pascal, to_upper_snake};
    assert_eq!(to_upper_snake("HitRadiusSq"), "HIT_RADIUS_SQ");
    assert_eq!(to_pascal("MAX_HP"), "MaxHp");
    assert_eq!(to_camel("TryBuyPotion"), "tryBuyPotion");
    assert_eq!(to_pascal("HTTPServer"), "HttpServer");
    assert_eq!(to_pascal("HUDScale"), "HudScale");
    assert_eq!(to_pascal("TargetID"), "TargetID");
}

#[test]
fn unused_mut_fires_only_for_bindings_that_never_change() {
    let src = r#"public struct SCounter
{
    public int Count;

    public mut void Bump()
    {
        Count += 1;
    }
}

public class Ship : Entity
{
    public mut int Hp = 10;
    public mut int Armor = 5;
    public mut List<int> Log;
    public mut SCounter Hits;

    public void Update(float dt)
    {
        Hp -= 1;
        Hits.Bump();
        mut int kept = Armor;
        mut int steps = 0;
        steps += 1;
        mut SCounter counter = new SCounter();
        counter.Bump();
        mut List<int> seen = new List<int>();
        seen.Add(kept + steps + counter.Count);
        Log.Add(seen.Count + twice(Hp) + bump(Hits.Count));
    }

    int twice(mut int n)
    {
        return n * 2;
    }

    int bump(mut int n)
    {
        n += 1;
        return n;
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new Ship { });
    }
}
"#;
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).unwrap_or_else(|| panic!("does not compile: {:?}", d.list));
    for preset in [Preset::Standard, Preset::Dotnet] {
        let cfg = LintConfig { preset, ..LintConfig::default() };
        let mut msgs: Vec<String> = lint(&c, src, &cfg).into_iter().filter(|l| l.code == "L316").map(|l| l.msg).collect();
        msgs.sort();
        // Armor, kept and twice's parameter never change; Hp (assigned), Log and seen (Add), Hits and
        // counter (a `mut` method call), steps (+=) and bump's parameter do
        assert_eq!(
            msgs,
            ["'kept' is declared 'mut' but never changes", "'n' is declared 'mut' but never changes", "field 'Ship.Armor' is declared 'mut' but never changes after spawn"],
            "{:?}",
            preset
        );
    }
}

// ---------------------------------------------------------------------------
// autofix keeps the program's behaviour
// ---------------------------------------------------------------------------

fn cc() -> Option<String> {
    ["gcc", "clang", "cc"].iter().map(|s| s.to_string()).find(|c| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false))
}

fn build(src: &str, tag: &str) -> Option<PathBuf> {
    build_with(src, tag, false)
}

fn build_with(src: &str, tag: &str, dev: bool) -> Option<PathBuf> {
    let cc = cc()?;
    let mut d = Diags::default();
    let c = sijoc::driver::compile_with(src, &mut d, &sijoc::driver::Options { dev, ..Default::default() }).expect("compiles");
    let dir = std::env::temp_dir().join(format!("sijoc-tooling-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).unwrap();
    let cfile = dir.join("p.c");
    let exe = dir.join("p");
    std::fs::write(&cfile, sijoc::codegen::generate(&c, tag)).unwrap();
    let ok = Command::new(cc).args(["-std=gnu11", "-O1", "-ffp-contract=off", "-w"]).arg(&cfile).arg("-o").arg(&exe).arg("-lm").status().unwrap().success();
    assert!(ok, "C compilation failed");
    Some(exe)
}

fn run(exe: &Path, args: &[&str]) -> (String, String) {
    let o = Command::new(exe).args(args).env("OMP_NUM_THREADS", "1").output().unwrap();
    (String::from_utf8_lossy(&o.stdout).to_string(), String::from_utf8_lossy(&o.stderr).to_string())
}

fn checksum(out: &str) -> String {
    out.lines().find(|l| l.starts_with("world checksum:")).unwrap_or("none").to_string()
}

#[test]
fn fix_all_preserves_behaviour() {
    let src = std::fs::read_to_string(root().join("../examples/arena.sijo")).unwrap();
    let (fixed, n) = sijoc::lint::fix_all(&src, &LintConfig::default());
    assert!(n > 0 || src == fixed);
    let mut d = Diags::default();
    assert!(sijoc::driver::compile(&fixed, &mut d).is_some(), "fixed code must compile");
    let (Some(a), Some(b)) = (build(&src, "orig"), build(&fixed, "fixed")) else { return };
    let (oa, _) = run(&a, &["--ticks", "300", "--quiet"]);
    let (ob, _) = run(&b, &["--ticks", "300", "--quiet"]);
    assert_eq!(checksum(&oa), checksum(&ob));
}

// ---------------------------------------------------------------------------
// live coding: state hand-over
// ---------------------------------------------------------------------------

#[test]
fn state_hand_over_is_exact() {
    let src = std::fs::read_to_string(root().join("../examples/village.sijo")).unwrap();
    let Some(exe) = build(&src, "village-state") else { return };
    let state = exe.with_extension("state");
    let (straight, _) = run(&exe, &["--ticks", "300", "--quiet"]);
    run(&exe, &["--ticks", "150", "--quiet", "--save-state", state.to_str().unwrap()]);
    let (resumed, err) = run(&exe, &["--ticks", "150", "--quiet", "--load-state", state.to_str().unwrap()]);
    assert!(err.contains("state restored at tick 150"), "{}", err);
    assert_eq!(checksum(&straight), checksum(&resumed), "save + load must not change the simulation");
}

#[test]
fn state_migrates_between_schemas() {
    let v1 = "public class Counter : Entity { public mut int Count; public mut int Old; public Counter? Peer;
                public void Update(float dt) { Count += 1; Old += 1; } }
              public static class Game { public static void Start() { var a = Spawn(new Counter { }); Spawn(new Counter { Peer = a }); }
                public static void Update(float dt) { } }";
    let v2 = "public class Counter : Entity { public mut int Count; public mut int Bonus = 1000; public Counter? Peer;
                public void Update(float dt) { Count += 1; Bonus += 1; } }
              public static class Game { public static void Start() { }
                public static void Update(float dt) {
                  if (Time.Tick == 21) { foreach (var c in Query<Counter>()) { int peer = c.Peer is Counter p ? p.Count : -1; Print($\"count={c.Count} bonus={c.Bonus} peer={peer}\"); } } } }";
    // live coding compiles in dev mode (all fields keep storage)
    let (Some(e1), Some(e2)) = (build_with(v1, "mig-v1", true), build_with(v2, "mig-v2", true)) else { return };
    let state = e1.with_extension("state");
    run(&e1, &["--ticks", "20", "--quiet", "--save-state", state.to_str().unwrap()]);
    let (out, err) = run(&e2, &["--ticks", "1", "--load-state", state.to_str().unwrap(), "--no-summary"]);
    assert!(err.contains("dropped:Counter.Old") && err.contains("new:Counter.Bonus"), "{}", err);
    // Count continues (20 -> 21), the new field starts from its default, handles survive
    assert!(out.contains("count=21 bonus=1001 peer=-1"), "{}", out);
    assert!(out.contains("count=21 bonus=1001 peer=21"), "{}", out);
}

// ---------------------------------------------------------------------------
// language server protocol
// ---------------------------------------------------------------------------

struct Lsp {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next: i64,
}

impl Lsp {
    fn start() -> Lsp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sijoc")).arg("lsp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Lsp { child, out, next: 1 }
    }
    fn send(&mut self, j: &Json) {
        let s = j.to_string();
        let stdin = self.child.stdin.as_mut().unwrap();
        write!(stdin, "Content-Length: {}\r\n\r\n{}", s.len(), s).unwrap();
        stdin.flush().unwrap();
    }
    fn recv(&mut self) -> Json {
        let mut len = 0;
        loop {
            let mut line = String::new();
            self.out.read_line(&mut line).unwrap();
            let t = line.trim_end();
            if t.is_empty() {
                break;
            }
            if let Some(v) = t.strip_prefix("Content-Length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut buf = vec![0; len];
        self.out.read_exact(&mut buf).unwrap();
        Json::parse(std::str::from_utf8(&buf).unwrap()).unwrap()
    }
    fn request(&mut self, method: &str, params: Json) -> Json {
        self.call(method, params).get("result").clone()
    }
    fn notify(&mut self, method: &str, params: Json) {
        self.send(&Json::obj(vec![("jsonrpc", Json::str("2.0")), ("method", Json::str(method)), ("params", params)]));
    }
    /// the whole response (`result` or `error`)
    fn call(&mut self, method: &str, params: Json) -> Json {
        let id = self.next;
        self.next += 1;
        self.send(&Json::obj(vec![("jsonrpc", Json::str("2.0")), ("id", Json::num(id as f64)), ("method", Json::str(method)), ("params", params)]));
        loop {
            let m = self.recv();
            if m.get("id").as_i64() == Some(id) {
                return m;
            }
        }
    }
    /// starts a server with one open document (its diagnostics are read)
    fn open(uri: &str, text: &str) -> Lsp {
        let mut lsp = Lsp::start();
        lsp.request("initialize", Json::obj(vec![("capabilities", Json::obj(vec![]))]));
        lsp.notify("textDocument/didOpen", Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri)), ("languageId", Json::str("sijo")), ("version", Json::num(1)), ("text", Json::str(text))]))]));
        let diag = lsp.recv();
        assert!(!diag.path(&["params", "diagnostics"]).as_arr().iter().any(|d| d.get("severity").as_i64() == Some(1)), "{:?}", diag);
        lsp
    }
    fn close(mut self) {
        self.request("shutdown", Json::Null);
        self.notify("exit", Json::Null);
        assert!(self.child.wait().unwrap().success());
    }
    fn hover(&mut self, uri: &str, text: &str, needle: &str, delta: usize) -> String {
        let h = self.request("textDocument/hover", Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri))])), ("position", pos_of(text, needle, delta))]));
        h.path(&["contents", "value"]).as_str().unwrap_or("").to_string()
    }
    fn rename(&mut self, uri: &str, text: &str, needle: &str, delta: usize, new: &str) -> Json {
        self.call("textDocument/rename", Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri))])), ("position", pos_of(text, needle, delta)), ("newName", Json::str(new))]))
    }
    fn completion(&mut self, uri: &str, text: &str, needle: &str, delta: usize) -> Vec<String> {
        let c = self.request("textDocument/completion", Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri))])), ("position", pos_of(text, needle, delta))]));
        c.get("items").as_arr().iter().filter_map(|i| i.get("label").as_str().map(|s| s.to_string())).collect()
    }
}

/// applies LSP text edits (one document) and returns the new text
fn apply_edits(text: &str, edits: &[Json]) -> String {
    let off = |p: &Json| sijoc::lsp::pos_to_off(text, p.get("line").as_i64().unwrap() as u32, p.get("character").as_i64().unwrap() as u32);
    let mut es: Vec<(usize, usize, String)> = edits.iter().map(|e| (off(e.path(&["range", "start"])), off(e.path(&["range", "end"])), e.get("newText").as_str().unwrap().to_string())).collect();
    es.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = text.to_string();
    for (lo, hi, t) in es {
        out.replace_range(lo..hi, &t);
    }
    out
}

/// (line, original text) of every edit of a rename response for one document
fn edit_sites(text: &str, resp: &Json, uri: &str) -> Vec<(i64, String)> {
    let edits = resp.path(&["result", "changes", uri]).as_arr();
    let off = |p: &Json| sijoc::lsp::pos_to_off(text, p.get("line").as_i64().unwrap() as u32, p.get("character").as_i64().unwrap() as u32);
    let mut v: Vec<(i64, String)> = edits.iter().map(|e| (e.path(&["range", "start", "line"]).as_i64().unwrap() + 1, text[off(e.path(&["range", "start"]))..off(e.path(&["range", "end"]))].to_string())).collect();
    v.sort();
    v
}

fn pos_of(text: &str, needle: &str, delta: usize) -> Json {
    let off = text.find(needle).unwrap_or_else(|| panic!("'{}' not found", needle)) + delta;
    let (l, c) = sijoc::lsp::off_to_pos(text, off);
    Json::obj(vec![("line", Json::num(l)), ("character", Json::num(c))])
}

#[test]
fn language_server_features() {
    let text = std::fs::read_to_string(root().join("../examples/arena.sijo")).unwrap();
    let uri = "file:///tmp/arena.sijo";
    let mut lsp = Lsp::start();
    let caps = lsp.request("initialize", Json::obj(vec![("capabilities", Json::obj(vec![]))]));
    assert_eq!(caps.path(&["capabilities", "hoverProvider"]), &Json::Bool(true));
    lsp.notify("textDocument/didOpen", Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri)), ("languageId", Json::str("sijo")), ("version", Json::num(1)), ("text", Json::str(text.clone()))]))]));
    let diag = lsp.recv();
    assert_eq!(diag.get("method").as_str(), Some("textDocument/publishDiagnostics"));
    assert!(!diag.path(&["params", "diagnostics"]).as_arr().is_empty(), "lint findings are published");
    let td = Json::obj(vec![("uri", Json::str(uri))]);
    // hover shows compiler decisions
    let h = lsp.request("textDocument/hover", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(&text, " Hp = Rules.MAX_HP", 2))]));
    let md = h.path(&["contents", "value"]).as_str().unwrap_or("").to_string();
    assert!(md.contains("Ship.Hp") && md.contains("ECS") && md.contains("range contract"), "{}", md);
    // member completion on an entity handle
    let c = lsp.request("textDocument/completion", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(&text, "target.Pos - Pos", 7))]));
    let labels: Vec<String> = c.get("items").as_arr().iter().filter_map(|i| i.get("label").as_str().map(|s| s.to_string())).collect();
    assert!(labels.contains(&"Pos".to_string()) && labels.contains(&"Hp".to_string()), "{:?}", labels);
    // static members
    let c = lsp.request("textDocument/completion", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(&text, "Rules.Wrap", 6))]));
    let labels: Vec<String> = c.get("items").as_arr().iter().filter_map(|i| i.get("label").as_str().map(|s| s.to_string())).collect();
    assert!(labels.iter().any(|l| l == "Wrap") && labels.iter().any(|l| l.contains("ARENA") || l.contains("Arena")), "{:?}", labels);
    // definition + rename of a method
    let call = if text.contains("steer();") { "steer();" } else { "Steer();" };
    let def = lsp.request("textDocument/definition", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(&text, call, 1))]));
    assert!(def.get("range").get("start").get("line").as_i64().unwrap() > 0);
    let rn = lsp.request("textDocument/rename", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(&text, call, 1)), ("newName", Json::str("chooseTarget"))]));
    assert_eq!(rn.path(&["changes", uri]).as_arr().len(), 2, "definition + call");
    // inlay hints carry network/ECS decisions
    let full = Json::obj(vec![("start", Json::obj(vec![("line", Json::num(0)), ("character", Json::num(0))])), ("end", Json::obj(vec![("line", Json::num(9999)), ("character", Json::num(0))]))]);
    let ih = lsp.request("textDocument/inlayHint", Json::obj(vec![("textDocument", td.clone()), ("range", full)]));
    let hints: Vec<String> = ih.as_arr().iter().filter_map(|h| h.get("label").as_str().map(|s| s.to_string())).collect();
    assert!(hints.iter().any(|h| h.contains("net 26 bits")) && hints.iter().any(|h| h.contains("frame arena")), "{:?}", hints);
    // outline
    let sy = lsp.request("textDocument/documentSymbol", Json::obj(vec![("textDocument", td.clone())]));
    assert!(sy.as_arr().iter().any(|s| s.get("name").as_str() == Some("Ship")));
    // a broken edit publishes an error but keeps the last good analysis for hover
    let broken = text.replacen("Cooldown -= dt;", "Cooldown -= ;", 1);
    lsp.notify("textDocument/didChange", Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri)), ("version", Json::num(2))])), ("contentChanges", Json::Arr(vec![Json::obj(vec![("text", Json::str(broken))])]))]));
    let diag = lsp.recv();
    assert!(diag.path(&["params", "diagnostics"]).as_arr().iter().any(|d| d.get("severity").as_i64() == Some(1)));
    lsp.request("shutdown", Json::Null);
    lsp.notify("exit", Json::Null);
    assert!(lsp.child.wait().unwrap().success());
}

/// Hover and completion speak the current grammar: `client` and `own` in signatures, ranges in
/// field types, the new keywords.
#[test]
fn language_server_uses_the_current_grammar() {
    let text = r#"public class Ship : Entity
{
    public mut int in 0..100 Hp = 50;
    public mut List<int> Log;

    public void Update(float dt)
    {
        Hp -= 1;
        keep(Lists.Make(Hp));
    }

    public client void Render()
    {
        Draw.Sprite(Hp, 0, 0);
    }

    void keep(own List<int> xs)
    {
        Log = xs;
    }
}

public static class Lists
{
    public static own List<int> Make(int count)
    {
        mut List<int> xs = new List<int>();
        xs.Add(count);
        return xs;
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new Ship { });
    }
}
"#;
    let uri = "file:///tmp/grammar.sijo";
    let mut lsp = Lsp::start();
    lsp.request("initialize", Json::obj(vec![("capabilities", Json::obj(vec![]))]));
    lsp.notify("textDocument/didOpen", Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri)), ("languageId", Json::str("sijo")), ("version", Json::num(1)), ("text", Json::str(text))]))]));
    let diag = lsp.recv();
    assert!(!diag.path(&["params", "diagnostics"]).as_arr().iter().any(|d| d.get("severity").as_i64() == Some(1)), "{:?}", diag);
    let td = Json::obj(vec![("uri", Json::str(uri))]);
    let hover = |lsp: &mut Lsp, needle: &str, delta: usize| -> String {
        let h = lsp.request("textDocument/hover", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(text, needle, delta))]));
        h.path(&["contents", "value"]).as_str().unwrap_or("").to_string()
    };
    let md = hover(&mut lsp, "void Render", 6);
    assert!(md.contains("client void") && md.contains("Render()") && !md.contains("[Client]"), "{}", md);
    let md = hover(&mut lsp, "void keep", 6);
    assert!(md.contains("own List<int> xs"), "{}", md);
    let md = hover(&mut lsp, " Hp = 50", 2);
    assert!(md.contains("range contract") && md.contains("[0, 100]"), "{}", md);
    // keywords offered where a statement starts
    let c = lsp.request("textDocument/completion", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(text, "Hp -= 1", 0))]));
    let labels: Vec<String> = c.get("items").as_arr().iter().filter_map(|i| i.get("label").as_str().map(|s| s.to_string())).collect();
    for kw in ["mut", "own", "client", "extern"] {
        assert!(labels.iter().any(|l| l == kw), "no '{}' in {:?}", kw, labels);
    }
    assert!(!labels.iter().any(|l| l == "readonly"), "{:?}", labels);
    lsp.request("shutdown", Json::Null);
    lsp.notify("exit", Json::Null);
    assert!(lsp.child.wait().unwrap().success());
}

/// a small sijo.toml project in a fresh temp directory
fn make_project(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sijo-proj-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/units")).unwrap();
    std::fs::write(dir.join("sijo.toml"), "name = \"proj\"\n").unwrap();
    std::fs::write(
        dir.join("src/Game.sijo"),
        "public static class Game\n{\n    public static void Start()\n    {\n        Spawn(new Ship { Hp = Rules.START_HP });\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/Rules.sijo"), "public static class Rules\n{\n    public const int START_HP = 90;\n}\n").unwrap();
    std::fs::write(
        dir.join("src/units/Ship.sijo"),
        "public class Ship : Entity\n{\n    public mut int Hp;\n\n    public void Update(float dt)\n    {\n        Hp -= 3;\n    }\n}\n",
    )
    .unwrap();
    dir
}

#[test]
fn projects_compile_as_one_program() {
    let dir = make_project("cli");
    // any file of a sijo.toml project means the whole project
    let one = dir.join("src/units/Ship.sijo").display().to_string();
    let sm = sijoc::driver::load_project(&[one]).unwrap();
    assert_eq!(sm.files.len(), 3);
    let mut d = Diags::default();
    assert!(sijoc::driver::compile(&sm.text, &mut d).is_some(), "{:?}", d.list);
    // diagnostics map back to the right file and line
    std::fs::write(dir.join("src/units/Ship.sijo"), "public class Ship : Entity\n{\n    public mut int Hp;\n\n    public void Update(float dt)\n    {\n        Hp -= 3.5;\n    }\n}\n").unwrap();
    let sm = sijoc::driver::load_project(&[dir.display().to_string()]).unwrap();
    let mut d = Diags::default();
    assert!(sijoc::driver::compile(&sm.text, &mut d).is_none());
    let (file, line, _) = sm.position(d.list[0].span.lo);
    assert!(file.ends_with("Ship.sijo") && line == 7, "{} {}", file, line);
    assert!(sm.render(&d.list[0]).contains("Ship.sijo:7:"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn language_server_understands_projects() {
    let dir = make_project("lsp");
    let game = dir.join("src/Game.sijo");
    let text = std::fs::read_to_string(&game).unwrap();
    let uri = format!("file://{}", std::fs::canonicalize(&game).unwrap().display());
    let mut lsp = Lsp::start();
    lsp.request("initialize", Json::obj(vec![("capabilities", Json::obj(vec![]))]));
    lsp.notify("textDocument/didOpen", Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(&uri)), ("languageId", Json::str("sijo")), ("version", Json::num(1)), ("text", Json::str(text.clone()))]))]));
    // one publishDiagnostics per project file, none of them errors (Ship and Rules live in other files)
    let mut errors = 0;
    for _ in 0..3 {
        let m = lsp.recv();
        assert_eq!(m.get("method").as_str(), Some("textDocument/publishDiagnostics"));
        errors += m.path(&["params", "diagnostics"]).as_arr().iter().filter(|d| d.get("severity").as_i64() == Some(1)).count();
    }
    assert_eq!(errors, 0);
    let td = Json::obj(vec![("uri", Json::str(&uri))]);
    // go to definition jumps into another file
    let def = lsp.request("textDocument/definition", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(&text, "START_HP", 2))]));
    let target = def.get("uri").as_str().unwrap_or("").to_string();
    assert!(target.ends_with("Rules.sijo"), "{:?}", def);
    assert_eq!(def.path(&["range", "start", "line"]).as_i64(), Some(2));
    // hover on a type from another file
    let h = lsp.request("textDocument/hover", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(&text, "Ship {", 1))]));
    assert!(h.path(&["contents", "value"]).as_str().unwrap_or("").contains("Ship"));
    // rename edits every file that mentions the symbol
    let rn = lsp.request("textDocument/rename", Json::obj(vec![("textDocument", td.clone()), ("position", pos_of(&text, "Hp =", 0)), ("newName", Json::str("Health"))]));
    let files: Vec<String> = match rn.get("changes") {
        Json::Obj(kv) => kv.iter().map(|(k, _)| k.clone()).collect(),
        _ => vec![],
    };
    assert!(files.iter().any(|f| f.ends_with("Game.sijo")) && files.iter().any(|f| f.ends_with("Ship.sijo")), "{:?}", files);
    lsp.request("shutdown", Json::Null);
    lsp.notify("exit", Json::Null);
    assert!(lsp.child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nested_query_hint_skips_grid_accelerated_loops() {
    let src = std::fs::read_to_string(root().join("../examples/arena.sijo")).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    let lints = sijoc::lint::lint(&c, &src, &LintConfig::default());
    let l402: Vec<_> = lints.iter().filter(|l| l.code == "L402").collect();
    // Ship.Update's target search is a real O(N*M) scan; Bullet.Update's hit test is grid-accelerated
    assert_eq!(l402.len(), 1, "{:?}", l402.iter().map(|l| l.msg.clone()).collect::<Vec<_>>());
}

#[test]
fn nested_query_hint_explains_why_a_loop_is_not_accelerated() {
    let src = std::fs::read_to_string(root().join("tests/programs/proximity_hints.sijo")).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).unwrap();
    let lints = sijoc::lint::lint(&c, &src, &LintConfig::default());
    let notes: Vec<String> = lints.iter().filter(|l| l.code == "L402").map(|l| l.note.clone().unwrap_or_default()).collect();
    assert_eq!(notes.len(), 3, "{:?}", notes);
    assert!(notes[0].contains("depends on the other entity") && notes[0].contains("MAX_R"), "{}", notes[0]);
    assert!(notes[1].contains("must be the last statement"), "{}", notes[1]);
    assert!(notes[2].contains("is not one of them"), "{}", notes[2]);
    // the helper Update calls on `this` is accelerated (Update builds its grid)
    assert!(c.spatial.loops.iter().any(|l| c.prog.funcs[l.func].name == "countClose"));
    // the suggested rewrite of the first loop is accelerated
    let fixed = src.replace("if ((o.Pos - Pos).LengthSq < o.Range * o.Range)", "if ((o.Pos - Pos).LengthSq < 16 && (o.Pos - Pos).LengthSq < o.Range * o.Range)");
    let c2 = sijoc::driver::compile(&fixed, &mut Diags::default()).unwrap();
    assert_eq!(c2.spatial.loops.len(), 2);
    assert!(c2.spatial.loops.iter().any(|l| (l.radius - 4.0).abs() < 1e-9));
    // this entity's own radius sizes the grid at run time, unless the system changes it
    let own = src.replace("if ((o.Pos - Pos).LengthSq < o.Range * o.Range)", "if ((o.Pos - Pos).LengthSq < Range * Range)");
    let c3 = sijoc::driver::compile(&own, &mut Diags::default()).unwrap();
    assert!(c3.spatial.loops.iter().any(|l| !l.dyn_r2.is_empty()), "per-entity radius not accelerated");
    let moving = own.replace("public float Range;", "public mut float Range;").replace("Near = 0;", "Near = 0;\n        Range = Range * 0.5f + 1;");
    let c4 = sijoc::driver::compile(&moving, &mut Diags::default()).unwrap();
    assert!(c4.spatial.loops.iter().all(|l| l.dyn_r2.is_empty()));
    assert!(c4.spatial.rejected.iter().any(|(_, _, w)| w.contains("which the system running the loop writes")), "{:?}", c4.spatial.rejected);
}

// ---------------------------------------------------------------------------
// naming rules, unused `mut`, autofix limits
// ---------------------------------------------------------------------------

fn lints_of(src: &str) -> Vec<sijoc::lint::LintDiag> {
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).unwrap_or_else(|| panic!("does not compile: {:?}", d.list));
    lint(&c, src, &LintConfig::default())
}

/// a prefix written in the wrong case is the prefix (iShape -> IShape, not IIShape); names with
/// letters that have no case (Hangul) are not held to case conventions
#[test]
fn prefix_suggestions_fix_the_case_of_the_prefix() {
    let src = r#"public interface iShape
{
    float Area();
}

public enum eTeam
{
    Red,
    Blue
}

public struct S벡터
{
    public float X;
}

public struct sVec
{
    public float X;
}

public class Ship : Entity, iShape
{
    public eTeam Side;
    public S벡터 Where;
    public sVec Pos;
    private mut float m속도;

    public float Area()
    {
        return Where.X + Pos.X + m속도;
    }

    public void Update(float dt)
    {
        m속도 += Area();
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new Ship { Side = eTeam.Blue });
    }
}
"#;
    let ls = lints_of(src);
    let fix_of = |code: &str, name: &str| -> (String, Option<String>) {
        let d = ls.iter().find(|d| d.code == code && d.msg.contains(&format!("'{}'", name))).unwrap_or_else(|| panic!("no {} for {}: {:?}", code, name, ls.iter().map(|d| &d.msg).collect::<Vec<_>>()));
        (d.msg.clone(), d.fix.as_ref().map(|f| f.title.clone()))
    };
    let (msg, _) = fix_of("L114", "iShape");
    assert!(msg.contains("(e.g. 'IShape')"), "{}", msg);
    // one fix for the name (on its first finding, L101)
    let fixes: Vec<String> = ls.iter().filter(|d| d.msg.contains("'iShape'")).filter_map(|d| d.fix.as_ref().map(|f| f.title.clone())).collect();
    assert_eq!(fixes, ["rename 'iShape' to 'IShape'"]);
    let (msg, _) = fix_of("L103", "eTeam");
    assert!(msg.contains("(e.g. 'ETeam')"), "{}", msg);
    let (msg, _) = fix_of("L102", "sVec");
    assert!(msg.contains("(e.g. 'SVec')"), "{}", msg);
    // S벡터 starts with the prefix; m속도 is 'm' + a name without case
    for d in &ls {
        assert!(!d.msg.contains("S벡터") && !d.msg.contains("m속도"), "{}: {}", d.code, d.msg);
    }
    let (fixed, _) = sijoc::lint::fix_all(src, &LintConfig::default());
    assert!(fixed.contains("public interface IShape") && fixed.contains("Entity, IShape") && !fixed.contains("IIShape"), "{}", fixed);
    assert!(fixed.contains("public enum ETeam") && fixed.contains("public struct SVec") && fixed.contains("public struct S벡터"), "{}", fixed);
}

#[test]
fn unused_mut_skips_bodiless_declarations_and_sees_for_headers() {
    let src = r#"public interface IGrow
{
    void Grow(mut int k);
}

public abstract class Unit : Entity, IGrow
{
    public abstract void Grow(mut int k);

    public abstract int Step(mut int n);
}

public class Tank : Unit
{
    public mut int Size;

    public override void Grow(mut int k)
    {
        k += 1;
        Size += k;
    }

    public override int Step(mut int n)
    {
        return n;
    }

    public void Update(float dt)
    {
        for (mut int i = 0; i < 3; i++)
        {
            Size += i;
        }
        for (mut int j = 0; j < 3; j++)
        {
            j += 1;
        }
        for (mut int w = 0; w < 4; w += 2)
        {
            Game.Bump(ref w);
        }
        for (int m = 0; m < 2; m++)
        {
            Size += m;
        }
        Grow(Step(1));
    }
}

public static class Game
{
    public static void Bump(ref int v)
    {
        v += 1;
    }

    public static void Start()
    {
        Spawn(new Tank { });
    }
}
"#;
    let mut msgs: Vec<String> = lints_of(src).into_iter().filter(|l| l.code == "L316").map(|l| l.msg).collect();
    msgs.sort();
    // the interface's and the abstract methods' parameters have no body that could change them;
    // `i` changes only in the header's step (which needs no `mut`); j and w change in the body
    assert_eq!(msgs, ["'i' is declared 'mut' but only the loop header changes it", "'n' is declared 'mut' but never changes"]);
    // removing the reported `mut` keeps the program valid
    let fixed = src.replace("for (mut int i = 0", "for (int i = 0").replace("Step(mut int n)\n    {\n        return n;", "Step(int n)\n    {\n        return n;");
    let mut d = Diags::default();
    assert!(sijoc::driver::compile(&fixed, &mut d).is_some(), "{:?}", d.list);
}

/// names the running program shows are reported but not renamed by --fix: enum members (ToString),
/// entity classes (runtime summary, saved states), functions whose Random stream is keyed by their
/// name, extern functions (the C symbol); methods tied to others by an interface or override
const RUNTIME_NAMES: &str = r#"public enum team
{
    red,
    blue
}

public interface IHit
{
    void hit(int amount);
}

public class ship : Entity, IHit
{
    public team Side;
    public mut int Hp = 10;

    public void hit(int amount)
    {
        Hp -= amount;
    }

    public void Update(float dt)
    {
        hit(dice.roll());
        Print($"{Side} {Hp}");
        if (Hp < 0)
        {
            Destroy();
        }
    }
}

public static class dice
{
    public static int roll()
    {
        return Random.Int(1, 3);
    }

    public static int twice(int value)
    {
        return value * 2;
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new ship { Side = team.blue, Hp = dice.twice(5) });
    }
}
"#;

#[test]
fn fix_all_keeps_names_the_program_shows() {
    let ls = lints_of(RUNTIME_NAMES);
    let find = |name: &str| ls.iter().filter(|d| d.msg.contains(&format!("'{}'", name)) && d.code != "L113").collect::<Vec<_>>();
    for name in ["red", "blue", "ship", "roll", "dice", "hit"] {
        let ds = find(name);
        assert!(!ds.is_empty(), "no naming finding for '{}'", name);
        for d in ds {
            assert!(d.fix.is_none(), "'{}' must not be renamed automatically: {:?}", name, d.fix);
            assert!(d.note.as_deref().unwrap_or("").contains("not fixed automatically"), "{}: {:?}", name, d.note);
        }
    }
    // other names are still fixed: the enum type, a method without Random
    let (fixed, n) = sijoc::lint::fix_all(RUNTIME_NAMES, &LintConfig::default());
    assert!(n > 0);
    for kept in ["    red,", "    blue", "class ship", "class dice", "int roll()", "void hit(int amount)"] {
        assert!(fixed.contains(kept), "'{}' changed:\n{}", kept, fixed);
    }
    assert!(fixed.contains("public enum ETeam") && fixed.contains("dice.Twice(5)"), "{}", fixed);
    // and the program prints and computes the same
    let (Some(a), Some(b)) = (build(RUNTIME_NAMES, "names-orig"), build(&fixed, "names-fixed")) else { return };
    let (oa, _) = run(&a, &["--ticks", "20"]);
    let (ob, _) = run(&b, &["--ticks", "20"]);
    assert!(oa.contains("blue "), "{}", oa);
    assert_eq!(oa, ob);
    // an extern function without an explicit C name is the C symbol
    let ext = "public static class Host\n{\n    public static extern int now_ms();\n}\n\npublic static class Game\n{\n    public static void Start()\n    {\n        Print($\"{Host.now_ms()}\");\n    }\n}\n";
    let d = lints_of(ext).into_iter().find(|d| d.code == "L105").expect("L105 for now_ms");
    assert!(d.fix.is_none() && d.note.unwrap_or_default().contains("C function"));
}

// ---------------------------------------------------------------------------
// language server: rename, references, hover, completion in the current grammar
// ---------------------------------------------------------------------------

const IFACES: &str = r#"public interface IHit
{
    void Hit(int amount);
}

public interface IArmored : IHit
{
    int Armor();
}

public struct SDummy : IHit
{
    public int Hits;

    public void Hit(int amount)
    {
    }
}

public abstract class Unit : Entity, IArmored
{
    public mut int Hp = 100;

    public abstract void Hit(int amount);

    public virtual int Speed()
    {
        return 1;
    }

    public int Armor()
    {
        return 1;
    }
}

public class Tank : Unit
{
    public override void Hit(int amount)
    {
        Hp -= amount + Speed();
    }

    public override int Speed()
    {
        return 2;
    }

    public void Update(float dt)
    {
        Hit(1);
        foreach (var h in Query<IHit>())
        {
            h.Hit(2);
        }
    }
}

public class Ship : Entity, IHit
{
    public mut int Shield = 5;

    public void Hit(int amount)
    {
        Shield -= amount;
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new Tank { });
        Spawn(new Ship { });
    }
}
"#;

/// an interface method, its implementations (through base classes and structs too) and the calls
/// through any of them are one method for rename and find-references; so is an override chain
#[test]
fn language_server_renames_interface_methods_with_their_implementations() {
    let uri = "file:///tmp/ifaces.sijo";
    let text = IFACES;
    let mut lsp = Lsp::open(uri, text);
    let all = vec![
        (3, "Hit".to_string()),  // IHit.Hit
        (15, "Hit".to_string()), // SDummy.Hit
        (24, "Hit".to_string()), // Unit.Hit (abstract)
        (39, "Hit".to_string()), // Tank.Hit (override)
        (51, "Hit".to_string()), // the call on this
        (54, "Hit".to_string()), // the call through an IHit handle
        (63, "Hit".to_string()), // Ship.Hit
    ];
    for (needle, delta) in [("void Hit(int amount);\n}\n\npublic interface", 5), ("void Hit(int amount)\n    {\n        Shield", 5), ("h.Hit(2)", 2), ("override void Hit", 14)] {
        let r = lsp.rename(uri, text, needle, delta, "Strike");
        assert_eq!(edit_sites(text, &r, uri), all, "rename from {:?}: {:?}", needle, r);
        let refs = lsp.request(
            "textDocument/references",
            Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri))])), ("position", pos_of(text, needle, delta)), ("context", Json::obj(vec![("includeDeclaration", Json::Bool(true))]))]),
        );
        assert_eq!(refs.as_arr().len(), all.len(), "references from {:?}: {:?}", needle, refs);
        let renamed = apply_edits(text, r.path(&["result", "changes", uri]).as_arr());
        let mut d = Diags::default();
        assert!(sijoc::driver::compile(&renamed, &mut d).is_some(), "{:?}\n{}", d.list, renamed);
    }
    // virtual / override chain: the declaration, the override and the call
    let r = lsp.rename(uri, text, "override int Speed", 13, "Pace");
    assert_eq!(edit_sites(text, &r, uri), vec![(26, "Speed".to_string()), (41, "Speed".to_string()), (44, "Speed".to_string())], "{:?}", r);
    lsp.close();
}

const DEFAULTS: &str = r#"public interface IHit
{
    void Hit(int amount);
    int Hp();
    void Finish()
    {
        Hit(Hp());
    }
}

public interface IBox<T>
{
    T Peek();
    T PeekOr(T other) => Peek();
}

public struct SDummy : IHit, IBox<float>
{
    public void Hit(int amount)
    {
    }

    public int Hp() => 1;

    public float Peek() => 2;
}

public class Ship : Entity, IHit, IBox<int>
{
    public mut int Shield = 5;

    public void Hit(int amount)
    {
        Shield -= amount;
    }

    public int Hp() => Shield;

    public int Peek() => Shield;

    public void Update(float dt)
    {
        if (Shield > 3)
        {
            Finish();
        }
    }
}

public static class Game
{
    public static float Top<B>(B b) where B : IBox<float> => b.PeekOr(0);

    public static void Start()
    {
        Spawn(new Ship { });
        Print($"{Top(new SDummy())}");
    }

    public static void Update(float dt)
    {
        foreach (var b in Query<IBox<int>>())
        {
            Print($"{b.PeekOr(1)} {b.Peek()}");
        }
    }
}
"#;

/// default implementations and generic interfaces: a default, the calls of it (also where a
/// struct uses its copy) and the methods of every instance of a generic interface (with their
/// implementations and the calls through each instance) rename together
#[test]
fn language_server_renames_defaults_and_generic_interface_methods() {
    let uri = "file:///tmp/defaults.sijo";
    let text = DEFAULTS;
    let mut lsp = Lsp::open(uri, text);
    let sites = |v: &[i64], name: &str| -> Vec<(i64, String)> { v.iter().map(|&l| (l, name.to_string())).collect() };
    for (needle, delta, name, lines) in [
        ("void Finish()", 5, "Finish", vec![5, 45]),
        ("Finish();", 0, "Finish", vec![5, 45]),
        ("T Peek();", 2, "Peek", vec![13, 14, 25, 39, 64]),
        ("int Peek()", 4, "Peek", vec![13, 14, 25, 39, 64]),
        ("b.Peek()", 2, "Peek", vec![13, 14, 25, 39, 64]),
        ("T PeekOr", 2, "PeekOr", vec![14, 52, 64]),
        ("b.PeekOr(0)", 2, "PeekOr", vec![14, 52, 64]),
        ("b.PeekOr(1)", 2, "PeekOr", vec![14, 52, 64]),
    ] {
        let r = lsp.rename(uri, text, needle, delta, "Fresh");
        assert_eq!(edit_sites(text, &r, uri), sites(&lines, name), "rename from {:?}: {:?}", needle, r);
        let refs = lsp.request(
            "textDocument/references",
            Json::obj(vec![("textDocument", Json::obj(vec![("uri", Json::str(uri))])), ("position", pos_of(text, needle, delta)), ("context", Json::obj(vec![("includeDeclaration", Json::Bool(true))]))]),
        );
        assert_eq!(refs.as_arr().len(), lines.len(), "references from {:?}: {:?}", needle, refs);
        let renamed = apply_edits(text, r.path(&["result", "changes", uri]).as_arr());
        let mut d = Diags::default();
        assert!(sijoc::driver::compile(&renamed, &mut d).is_some(), "{:?}\n{}", d.list, renamed);
    }
    // hover: the interface and what a call of the default runs
    let h = lsp.hover(uri, text, "public interface IHit", 17);
    assert!(h.contains("interface IHit") && h.contains("implemented by `Ship`"), "{}", h);
    let h = lsp.hover(uri, text, "Finish();", 0);
    assert!(h.contains("IHit.Finish"), "{}", h);
    lsp.close();
}

const GENERIC_POOL: &str = r#"public struct SVec2
{
    public float X;
    public float Y;
}

public class Pool<T> : Entity where T : struct
{
    public mut List<T> Items;

    public void Put(T x)
    {
        Items.Add(x);
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new Pool<int> { });
        Spawn(new Pool<SVec2> { });
    }

    public static void Update(float dt)
    {
        foreach (var a in Query<Pool<int>>())
        {
            a.Put(1);
        }
        foreach (var b in Query<Pool<SVec2>>())
        {
            b.Put(new SVec2());
            Print($"{b.Items.Count}");
        }
    }
}
"#;

/// a generic entity class: hover lists its instances (each an entity class with an archetype of
/// its own); a member used through any instance is one symbol for rename and find-references
#[test]
fn language_server_shows_generic_class_instances() {
    let uri = "file:///tmp/generic-pool.sijo";
    let text = GENERIC_POOL;
    let mut lsp = Lsp::open(uri, text);
    let md = lsp.hover(uri, text, "Pool<T> : Entity", 0);
    for want in ["(generic entity class) Pool<T>", "2 instance(s)", "`Pool<int>`", "`Pool<SVec2>`", "archetype `Pool<SVec2>`"] {
        assert!(md.contains(want), "want {:?} in\n{}", want, md);
    }
    let md = lsp.hover(uri, text, "Query<Pool<SVec2>>", 6);
    assert!(md.contains("(generic entity class) Pool<T>"), "{}", md);
    let md = lsp.hover(uri, text, "Items;", 0);
    assert!(md.contains("(field) mut List<int> Pool<int>.Items") && md.contains("declared in the generic class `Pool<T>`"), "{}", md);
    let md = lsp.hover(uri, text, "Put(T x)", 0);
    assert!(md.contains("void Pool<int>.Put(int x)") && md.contains("instances: `Pool<int>`, `Pool<SVec2>`"), "{}", md);
    // the call through Pool<SVec2> renames the declaration and the call through Pool<int>
    let r = lsp.rename(uri, text, "b.Put", 2, "Store");
    assert_eq!(edit_sites(text, &r, uri), vec![(11, "Put".to_string()), (29, "Put".to_string()), (33, "Put".to_string())], "{:?}", r);
    let r = lsp.rename(uri, text, "b.Items", 2, "Stock");
    assert_eq!(edit_sites(text, &r, uri), vec![(9, "Items".to_string()), (13, "Items".to_string()), (34, "Items".to_string())], "{:?}", r);
    let renamed = apply_edits(text, r.path(&["result", "changes", uri]).as_arr());
    let mut d = Diags::default();
    assert!(sijoc::driver::compile(&renamed, &mut d).is_some(), "{:?}\n{}", d.list, renamed);
    lsp.close();
}

#[test]
fn language_server_rename_checks_the_new_name() {
    let uri = "file:///tmp/rename-check.sijo";
    let text = r#"public class Ship : Entity
{
    public mut int Hp = 100;
    public mut int Pos;

    public void Update(float dt)
    {
        mut int steer = 1;
        steer += 1;
        Hp += steer;
        Pos += Hp;
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new Ship { });
    }
}
"#;
    let mut lsp = Lsp::open(uri, text);
    let refused = |lsp: &mut Lsp, needle: &str, new: &str| -> String {
        let r = lsp.rename(uri, text, needle, 0, new);
        assert!(r.get("result").is_null(), "renaming to '{}' should be refused: {:?}", new, r);
        let msg = r.path(&["error", "message"]).as_str().unwrap_or("").to_string();
        assert!(!msg.is_empty(), "{:?}", r);
        msg
    };
    for kw in ["in", "foreach", "mut", "own", "int", "var", "List"] {
        let msg = refused(&mut lsp, "steer = 1", kw);
        assert!(msg.contains("keyword"), "{}", msg);
    }
    assert!(refused(&mut lsp, "steer = 1", "2fast").contains("not a valid name"));
    // the name is taken in the same scope
    let msg = refused(&mut lsp, "Hp = 100", "Pos");
    assert!(msg.contains("E0104"), "{}", msg);
    // the new name would capture the uses of another declaration
    let msg = refused(&mut lsp, "steer = 1", "Hp");
    assert!(msg.contains("different declaration"), "{}", msg);
    // names with Hangul (or any Unicode letters) are fine
    let r = lsp.rename(uri, text, "Hp = 100", 0, "체력");
    let renamed = apply_edits(text, r.path(&["result", "changes", uri]).as_arr());
    assert!(renamed.contains("public mut int 체력 = 100;") && renamed.contains("체력 += steer;") && renamed.contains("Pos += 체력;"), "{:?}", r);
    let r = lsp.rename(uri, text, "steer = 1", 0, "조향");
    assert_eq!(r.path(&["result", "changes", uri]).as_arr().len(), 3, "{:?}", r);
    lsp.close();
}

const SIGNATURES: &str = r#"public interface IHit
{
    void Hit(int in 0..50 amount);
}

public struct SVec
{
    public float X;
    public float Y;

    public mut void Scale(float k)
    {
        X *= k;
        Y *= k;
    }
}

public class Ship : Entity, IHit
{
    public const int MAX_HP = 100;
    public mut int in 0..MAX_HP Hp = 100;
    [Precision(0.01)] public mut SVec in 0..64 Pos;
    public IHit? Target;

    public void Hit(int in 0..50 amount)
    {
        Hp -= amount;
    }

    public void Update(float dt)
    {
        mut int in -5..5 steer = 0;
        steer += 1;
        Pos.Scale(1.01f);
        if (Target is IHit t)
        {
            t.Hit(steer);
        }
        foreach (var s in Query<Ship>())
        {
            Hp += s.Hp - steer;
        }
    }

    public client void Render()
    {
        Draw.Sprite(1, Pos.X, Pos.Y + Hp);
    }
}

public class Wall : Entity, IHit
{
    public mut int Armor = 3;

    public void Hit(int in 0..50 amount)
    {
        Armor -= amount;
    }
}

public static class Game
{
    public static own List<int> Make(own List<int> xs)
    {
        return xs;
    }

    public static void Start()
    {
        Spawn(new Ship { });
        Spawn(new Wall { });
    }
}
"#;

#[test]
fn language_server_hover_shows_declarations_as_written() {
    let uri = "file:///tmp/signatures.sijo";
    let text = SIGNATURES;
    let mut lsp = Lsp::open(uri, text);
    for (needle, delta, want) in [
        (" Hp = 100", 1, "(field) mut int in 0..100 Ship.Hp"),
        (" Pos;", 1, "(field) mut SVec in 0..64 Ship.Pos"),
        ("steer = 0", 0, "(local) mut int in -5..5 steer"),
        ("amount)\n    {\n        Hp", 0, "(parameter) int in 0..50 amount"),
        ("Hit(int in 0..50 amount);", 0, "void IHit.Hit(int in 0..50 amount)"),
        ("Make(own", 0, "own List<int> Game.Make(own List<int> xs)"),
        ("Scale(1.01f)", 0, "mut void SVec.Scale(float k)"),
        ("Render()", 0, "client void Ship.Render()"),
        ("xs)\n", 0, "(parameter) own List<int> xs"),
    ] {
        let md = lsp.hover(uri, text, needle, delta);
        assert!(md.contains(want), "hover on {:?}: want {:?} in\n{}", needle, want, md);
    }
    // a call through an interface handle runs one of the implementations: their effects, not "pure"
    let md = lsp.hover(uri, text, "t.Hit", 2);
    assert!(!md.contains("**effects**: pure"), "{}", md);
    assert!(md.contains("Ship.Hp") && md.contains("Wall.Armor") && md.contains("one of 2 implementations"), "{}", md);
    lsp.close();
}

#[test]
fn language_server_completes_range_bounds_with_constants() {
    let uri = "file:///tmp/ranges.sijo";
    let text = SIGNATURES;
    let mut lsp = Lsp::open(uri, text);
    for (needle, delta) in [("in 0..MAX_HP Hp", 3), ("0..MAX_HP Hp", 3), ("in -5..5 steer", 3)] {
        let labels = lsp.completion(uri, text, needle, delta);
        assert!(labels.iter().any(|l| l == "MAX_HP" || l == "Ship.MAX_HP"), "{:?}: {:?}", needle, labels);
        for not in ["public", "int", "mut", "Ship", "Math", "steer"] {
            assert!(!labels.iter().any(|l| l == not), "{:?}: '{}' offered in a range bound: {:?}", needle, not, labels);
        }
    }
    // a foreach's `in` is not a range: everything is offered there
    let labels = lsp.completion(uri, text, "Query<Ship>()", 0);
    assert!(labels.iter().any(|l| l == "Query") && labels.iter().any(|l| l == "steer"), "{:?}", labels);
    lsp.close();
}

/// member completion after a name with non-ASCII letters (the name must not be cut in the middle
/// of a character)
#[test]
fn language_server_completes_members_of_unicode_names() {
    let uri = "file:///tmp/unicode.sijo";
    let text = "public struct S벡터\n{\n    public float 가로;\n    public float 세로;\n}\n\npublic class 배 : Entity\n{\n    public mut S벡터 위치;\n\n    public void Update(float dt)\n    {\n        위치.가로 += 1;\n    }\n}\n\npublic static class Game\n{\n    public static void Start()\n    {\n        Spawn(new 배 { });\n    }\n}\n";
    let mut lsp = Lsp::open(uri, text);
    let labels = lsp.completion(uri, text, "위치.가로 +=", "위치.".len());
    assert!(labels.iter().any(|l| l == "가로") && labels.iter().any(|l| l == "세로"), "{:?}", labels);
    let md = lsp.hover(uri, text, "위치.가로 +=", 0);
    assert!(md.contains("(field) mut S벡터 배.위치"), "{}", md);
    lsp.close();
}

/// a replicated `T` field of a generic class gets L404 (unquantized float) only when every
/// instance sends it as a raw float: in `Pool<int>` it is an int, and a range cannot go on `T`
#[test]
fn unquantized_replication_of_a_generic_field_holds_in_every_instance() {
    let prog = |spawns: &str| {
        format!(
            r#"public class Pool<T> : Entity where T : struct
{{
    public T Seed;

    public client void Render()
    {{
        Print($"{{Seed}}");
    }}
}}

public static class Game
{{
    public static void Start()
    {{
        {}
    }}
}}
"#,
            spawns
        )
    };
    let n = |src: &str| codes(src, Preset::Standard).iter().filter(|c| **c == "L404").count();
    assert_eq!(n(&prog("Spawn(new Pool<int> { });\n        Spawn(new Pool<float> { });")), 0);
    assert_eq!(n(&prog("Spawn(new Pool<float> { });")), 1);
}
