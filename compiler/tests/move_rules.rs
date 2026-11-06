//! Move rules (2.4): a move inside a lambda runs once per element, a move followed by
//! return/break/continue/fail does not reach the code after it, `?:` branches are alternatives,
//! coroutine locals kept in hidden fields across awaits move like locals, and bounds checks stay
//! where the loop body can change the index or the list.

use sijoc::diag::{Diags, Level};
use sijoc::driver::compile;
use std::path::{Path, PathBuf};
use std::process::Command;

const GAME: &str = "public static class Game { public static void Start() { } }";

/// `Gen.Size` takes the list, `Gen.Make` returns a new one
const GEN: &str = "public static class Gen
{
    public static int Size(own List<int> xs) => xs.Count;
    public static own List<int> Make(int n) { mut var xs = new List<int>(); for (int i = 0; i < n; i++) { xs.Add(i); } return xs; }
    public static void Fill(out List<int> xs) { xs = new List<int> { 7 }; }
}";

fn errors(src: &str) -> Vec<(&'static str, String)> {
    let mut d = Diags::default();
    let _ = compile(src, &mut d);
    d.list.iter().filter(|x| x.level == Level::Error).map(|x| (x.code, x.msg.clone())).collect()
}

/// a static method `F(bool c, int k)` with `body`
fn stat(body: &str) -> String {
    format!("{GEN}\npublic static class T {{ public static void F(bool c, int k) {{ {body} }} }}\n{GAME}")
}

/// an entity method `M(bool c, int k)` with `body`; `Take` moves its argument into the field `Keep`
fn ent(body: &str) -> String {
    format!(
        "{GEN}\npublic class Box : Entity {{ public mut List<int> Keep; public mut List<int> Other; public void Take(own List<int> xs) {{ Keep = xs; }}
           public void M(bool c, int k) {{ {body} }} public void Update(float dt) {{ M(true, 1); }} }}\n{GAME}"
    )
}

/// an async entity method `Run()` with `body`
fn co(body: &str) -> String {
    format!(
        "{GEN}\npublic class Box : Entity {{ public mut List<int> Keep; public mut List<List<int>> Rows; public mut int Total;
           public void Take(own List<int> xs) {{ Keep = xs; }} public async void Run() {{ {body} }} }}\n{GAME}"
    )
}

#[track_caller]
fn expect_error(src: &str, code: &str, needle: &str) {
    let got = errors(src);
    assert!(got.iter().any(|(c, m)| *c == code && m.contains(needle)), "expected {} containing '{}' but got {:?} for:\n{}", code, needle, got, src);
}

#[track_caller]
fn expect_ok(src: &str) {
    let got = errors(src);
    assert!(got.is_empty(), "expected no errors but got {:?} for:\n{}", got, src);
}

// ---------------------------------------------------------------------------
// lambdas
// ---------------------------------------------------------------------------

#[test]
fn a_move_inside_a_lambda_runs_for_every_element() {
    let lam = "moved inside a lambda that runs for every element";
    let pre = "var src = new List<int> { 1, 2, 3 }; var ys = new List<int> { 1, 2 };";
    for call in [
        "int n = src.Count(x => Gen.Size(ys) > x);",
        "bool b = src.Exists(x => Gen.Size(ys) > x);",
        "bool b = src.All(x => Gen.Size(ys) > x);",
        "int n = src.Sum(x => Gen.Size(ys));",
        "var w = src.Where(x => Gen.Size(ys) > x);",
        "var w = src.Select(x => x + Gen.Size(ys));",
        "var w = src.OrderBy(x => Gen.Size(ys) - x);",
        "int n = src.FirstOrDefault(x => Gen.Size(ys) > x);",
        "int n = src.FindIndex(x => Gen.Size(ys) > x);",
        // a nested lambda runs for every element of both lists
        "int n = src.Count(x => src.Count(z => Gen.Size(ys) > z) > x);",
        // only one branch of ?: runs, but it runs for every element
        "int n = src.Count(x => c ? Gen.Size(ys) > x : x > 0);",
    ] {
        expect_error(&stat(&format!("{pre} {call}")), "E0501", lam);
    }
    // a lambda that changes its list
    expect_error(&stat("mut var src = new List<int> { 1, 2, 3 }; var ys = new List<int> { 1 }; int n = src.RemoveAll(x => Gen.Size(ys) > x);"), "E0501", lam);
    expect_error(&stat("mut var src = new List<int> { 1, 2, 3 }; var ys = new List<int> { 1 }; src.Sort((a, b) => Gen.Size(ys) + a - b);"), "E0501", lam);
    // moving into a field or a list inside a lambda body
    expect_error(&ent("var src = new List<int> { 1, 2 }; var ys = new List<int> { 1 }; int n = src.Count(x => Gen.Size(ys) > x);"), "E0501", lam);
}

#[test]
fn a_lambda_cannot_move_the_list_it_iterates() {
    let msg = "moved inside a lambda that iterates over it";
    expect_error(&stat("var it2 = new List<int> { 1, 2, 3 }; int n = it2.Count(x => Gen.Size(it2) > x);"), "E0406", msg);
    expect_error(&stat("var xs = new List<int> { 1, 2, 3 }; var ys = new List<int> { 1 }; int n = xs.Count(x => ys.Count(y => Gen.Size(xs) > y) > x);"), "E0406", msg);
    expect_error(&ent("var xs = new List<int> { 1, 2, 3 }; int n = xs.Count(x => x > 0 && Gen.Size(xs) > x);"), "E0406", msg);
}

#[test]
fn lambdas_that_only_read_or_move_fresh_values_are_fine() {
    expect_ok(&stat(
        "var src = new List<int> { 1, 2, 3 }; var ys = new List<int> { 1, 2 };
         int n = src.Count(x => ys.Count > x) + src.Count(x => ys.Contains(x)) + src.Count(x => Gen.Size(Gen.Make(x)) > 0)
             + src.Count(x => Gen.Size(ys.Clone()) > x) + src.Sum(x => Gen.Size(new List<int> { x }));
         // the lambda only borrowed ys: it can move afterwards
         int m = Gen.Size(ys);
         Print($\"{n} {m}\");",
    ));
    // a list given a new value in the same run of the body may move
    expect_ok(&stat("var src = new List<int> { 1, 2 }; mut var ys = new List<int>(); int n = src.Count(x => Gen.Size(Gen.Make(x)) > 0 && ys.Count == 0);"));
}

#[test]
fn moving_an_element_is_only_a_borrow_error() {
    // lambda parameters and foreach variables are bound to each element again
    let src = stat("var xss = new List<List<int>>(); int n = xss.Count(row => Gen.Size(row) > 0); foreach (var r in xss) { int m = Gen.Size(r); }");
    let got = errors(&src);
    assert_eq!(got.iter().map(|(c, _)| *c).collect::<Vec<_>>(), ["E0504", "E0504"], "{:?}", got);
}

#[test]
fn foreach_bodies_still_count_as_loops() {
    expect_error(&stat("var src = new List<int> { 1, 2 }; var ys = new List<int> { 1 }; src.ForEach(x => { Print($\"{Gen.Size(ys)}\"); });"), "E0501", "moved list 'ys'");
    expect_error(&stat("var src = new List<int> { 1, 2 }; var ys = new List<int> { 1 }; foreach (var x in src) { int n = Gen.Size(ys); }"), "E0501", "moved list 'ys'");
    // the loop reads its list again on the next iteration
    expect_error(&stat("var xs = new List<int> { 1, 2 }; foreach (var x in xs) { if (x == 1) { int n = Gen.Size(xs); } }"), "E0501", "moved list 'xs'");
    // ... unless it is left right after the move
    expect_ok(&stat("var xs = new List<int> { 1, 2 }; foreach (var x in xs) { if (x == 1) { int n = Gen.Size(xs); break; } }"));
    expect_ok(&stat("var xs = new List<int> { 1, 2 }; foreach (var x in xs) { if (x == 1) { int n = Gen.Size(xs); return; } }"));
}

// ---------------------------------------------------------------------------
// return / break / continue / fail and alternatives
// ---------------------------------------------------------------------------

#[test]
fn a_move_before_leaving_does_not_reach_the_code_after() {
    // the adversarial cases
    expect_ok(&ent("var a = new List<int> { 1 }; if (c) { Take(a); return; } Print($\"a {a.Count}\");"));
    expect_ok(&ent("var a = new List<int> { 1 }; while (true) { Take(a); break; }"));
    expect_ok(&ent("var copy = Keep.Clone(); for (int i = 0; i < 3; i++) { if (i == 1) { Take(copy); return; } }"));
    expect_ok(&ent("var a = new List<int> { 1 }; for (int i = 0; i < 3; i++) { if (i == k) { Take(a); break; } }"));
    expect_ok(&ent("var a = new List<int> { 1 }; do { Take(a); break; } while (c);"));
    expect_ok(&ent("var a = new List<int> { 1 }; foreach (var b in Query<Box>()) { Take(a); return; }"));
    expect_ok(&ent("var a = new List<int> { 1 }; if (c) { Take(a); } else { Print($\"{a.Count}\"); }"));
    expect_ok(&ent("var a = new List<int> { 1 }; if (c) { Take(a); return; } else if (k > 0) { Take(a); return; } Print($\"{a.Count}\");"));
    // switch sections are alternatives; `break` leaves the switch, `return` the method
    expect_ok(&ent("var a = new List<int> { 1 }; switch (k) { case 1: Take(a); return; case 2: Print($\"{a.Count}\"); break; default: break; } Print($\"{a.Count}\");"));
    expect_ok(&ent("var a = new List<int> { 1 }; while (c) { switch (k) { case 1: Take(a); return; default: break; } Print($\"{a.Count}\"); }"));
    // a new value after the move
    expect_ok(&stat("mut var a = new List<int> { 1 }; int n = Gen.Size(a); a = new List<int>(); Print($\"{a.Count} {n}\");"));
    expect_ok(&stat("mut var a = new List<int> { 1 }; int n = Gen.Size(a); Gen.Fill(out a); Print($\"{a.Count} {n}\");"));
    expect_ok(&stat("mut var a = new List<int> { 1 }; for (int i = 0; i < 3; i++) { int n = Gen.Size(a); a = Gen.Make(i); }"));
}

#[test]
fn a_move_that_can_reach_a_later_use_is_still_an_error() {
    let used = "use of moved list 'a'";
    expect_error(&ent("var a = new List<int> { 1 }; if (c) { Take(a); } Print($\"{a.Count}\");"), "E0501", used);
    expect_error(&ent("var a = new List<int> { 1 }; while (c) { Take(a); }"), "E0501", used);
    expect_error(&ent("var a = new List<int> { 1 }; do { Take(a); } while (c);"), "E0501", used);
    expect_error(&ent("var a = new List<int> { 1 }; for (int i = 0; i < 3; i++) { Take(a); }"), "E0501", used);
    // continue goes to the next iteration, break to the code after the loop
    expect_error(&ent("var a = new List<int> { 1 }; for (int i = 0; i < 3; i++) { if (i == 1) { Take(a); continue; } }"), "E0501", used);
    expect_error(&ent("var a = new List<int> { 1 }; for (int i = 0; i < 3; i++) { if (i == 1) { Take(a); break; } } Print($\"{a.Count}\");"), "E0501", used);
    expect_error(&ent("var a = new List<int> { 1 }; while (true) { if (c) { Take(a); break; } } Print($\"{a.Count}\");"), "E0501", used);
    expect_error(&ent("var a = new List<int> { 1 }; foreach (var b in Query<Box>()) { Take(a); }"), "E0501", used);
    // break inside a switch leaves the switch only
    expect_error(&ent("var a = new List<int> { 1 }; switch (k) { case 1: Take(a); break; default: break; } Print($\"{a.Count}\");"), "E0501", used);
    expect_error(&ent("var a = new List<int> { 1 }; while (c) { switch (k) { case 1: Take(a); break; default: break; } }"), "E0501", used);
    // `fail` jumps to the end of the atomic block: the list has moved
    expect_error(&stat("var a = new List<int> { 1 }; mut int n = 0; atomic { n = Gen.Size(a); if (c) { fail; } } Print($\"{a.Count} {n}\");"), "E0501", used);
    // a move in a later iteration through a nested loop
    expect_error(&ent("var a = new List<int> { 1 }; for (int i = 0; i < 3; i++) { for (int j = 0; j < 3; j++) { if (j == 2) { Take(a); break; } } }"), "E0501", used);
}

#[test]
fn conditional_branches_are_alternatives() {
    expect_ok(&ent("var a = new List<int> { 1, 2 }; Print($\"{(c ? Gen.Size(a) : Gen.Size(a))}\");"));
    expect_ok(&stat("var a = new List<int> { 1, 2 }; int n = c ? Gen.Size(a) : k > 0 ? Gen.Size(a) : 0;"));
    expect_ok(&stat("var a = new List<int> { 1, 2 }; int n = k switch { 1 => Gen.Size(a), 2 => Gen.Size(a), _ => 0 };"));
    // but either branch may have moved it
    expect_error(&stat("var a = new List<int> { 1, 2 }; int n = c ? Gen.Size(a) : 0; Print($\"{a.Count} {n}\");"), "E0501", "use of moved list 'a'");
    expect_error(&stat("var a = new List<int> { 1, 2 }; int n = k switch { 1 => Gen.Size(a), _ => 0 }; Print($\"{a.Count} {n}\");"), "E0501", "use of moved list 'a'");
    expect_error(&stat("var a = new List<int> { 1, 2 }; bool b = c && Gen.Size(a) > 0; Print($\"{a.Count} {b}\");"), "E0501", "use of moved list 'a'");
    // the right operand runs after the left one
    expect_error(&stat("var a = new List<int> { 1, 2 }; bool b = Gen.Size(a) > 0 || a.Count > 0;"), "E0501", "use of moved list 'a'");
}

// ---------------------------------------------------------------------------
// coroutine locals kept in hidden fields across awaits
// ---------------------------------------------------------------------------

#[test]
fn coroutine_locals_across_awaits_move_like_locals() {
    expect_ok(&co("var ys = new List<int> { 1, 2 }; await NextTick(); Keep = ys;"));
    expect_ok(&co("var ys = new List<int> { 1, 2 }; await NextTick(); Take(ys);"));
    expect_ok(&co("mut var xs = new List<int> { 1, 2 }; await NextTick(); xs.Add(3); Total += Gen.Size(xs);"));
    expect_ok(&co("var ys = Gen.Make(3); await NextTick(); Rows.Add(ys); var zs = Gen.Make(2); await NextTick(); var local = zs; Total += local.Count;"));
    expect_ok(&co("while (true) { var ys = Gen.Make(3); await NextTick(); Keep = ys; }"));
    expect_ok(&co("var ys = Gen.Make(3); await NextTick(); if (Total > 0) { Keep = ys; return; } Total += ys.Count;"));
    // the errors name the local, not the hidden field
    for (body, msg) in [
        ("var ys = new List<int> { 1, 2 }; await NextTick(); Keep = ys; Print($\"{ys.Count}\");", "use of moved list 'ys'"),
        ("var ys = new List<int> { 1, 2 }; await NextTick(); Take(ys); Total += ys.Count;", "use of moved list 'ys'"),
        ("var ys = Gen.Make(2); while (true) { await NextTick(); Keep = ys; }", "use of moved list 'ys'"),
        ("var ys = Gen.Make(2); await NextTick(); var src = new List<int> { 1 }; Total += src.Count(x => Gen.Size(ys) > x);", "'ys' is moved inside a lambda"),
    ] {
        expect_error(&co(body), "E0501", msg);
        assert!(errors(&co(body)).iter().all(|(_, m)| !m.contains("__Run")), "{:?}", errors(&co(body)));
    }
    // an entity field still cannot move
    expect_error(&co("await NextTick(); Take(Keep);"), "E0502", "cannot move list field 'Keep'");
}

// ---------------------------------------------------------------------------
// generated code: built and run under the sanitizers
// ---------------------------------------------------------------------------

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

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sijoc-moves-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Compile Sijo source to C and then to an executable with the given extra C flags (None: no C
/// compiler, or a sanitizer it does not have).
fn build(src: &str, tag: &str, extra: &[&str]) -> Option<PathBuf> {
    let cc = cc()?;
    let mut d = Diags::default();
    let compiled = compile(src, &mut d).unwrap_or_else(|| panic!("{} failed to compile: {:?}", tag, d.list));
    let csrc = sijoc::codegen::generate(&compiled, &format!("{tag}.sijo"));
    let dir = out_dir(tag);
    let cfile = dir.join("prog.c");
    let exe = dir.join("prog");
    std::fs::write(&cfile, csrc).unwrap();
    let mut args: Vec<String> = vec!["-std=gnu11".into(), "-O1".into(), "-ffp-contract=off".into(), "-w".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    args.push(cfile.display().to_string());
    args.push("-o".into());
    args.push(exe.display().to_string());
    args.push("-lm".into());
    let out = Command::new(&cc).args(&args).output().unwrap();
    if !out.status.success() {
        if extra.iter().any(|f| f.starts_with("-fsanitize")) {
            return None;
        }
        panic!("C compilation failed for {}:\n{}", tag, String::from_utf8_lossy(&out.stderr));
    }
    Some(exe)
}

fn run(exe: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(exe).args(args).env("ASAN_OPTIONS", "detect_leaks=1").env("OMP_NUM_THREADS", "1").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (out.status.code().unwrap_or(-1), format!("{}{}", stdout, stderr))
}

const SAN: [&str; 3] = ["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"];

/// Builds with the sanitizers, or without them when the C compiler has none.
fn build_checked(src: &str, tag: &str) -> Option<PathBuf> {
    build(src, &format!("{tag}-asan"), &SAN).or_else(|| build(src, tag, &[]))
}

#[test]
fn moved_coroutine_lists_are_freed_once() {
    // lists that live across awaits move into a field, an own parameter, a nested list and a
    // local, tick after tick; one entity is destroyed with its hidden fields in between
    let src = format!(
        "{GEN}
public class Ship : Entity
{{
    public mut List<int> Keep;
    public mut List<List<int>> Rows;
    public mut int Total;

    public void Take(own List<int> xs) {{ Keep = xs; }}

    public async void Run()
    {{
        while (true)
        {{
            mut var xs = new List<int> {{ 1, 2 }};
            await NextTick();
            xs.Add(3);
            Total += Gen.Size(xs);
            var ys = Gen.Make(3);
            await NextTick();
            Keep = ys;
            var zs = Gen.Make(4);
            await NextTick();
            Take(zs);
            var rs = Gen.Make(2);
            var ws = Gen.Make(5);
            await NextTick();
            Rows.Add(rs);
            var local = ws;
            Total += local.Count;
            var kept = Gen.Make(2);
            await NextTick();
            Total += kept.Count;
            if (Rows.Count > 2)
            {{
                Rows.Clear();
            }}
            Print($\"total {{Total}} keep {{Keep.Count}} rows {{Rows.Count}}\");
        }}
    }}
}}

public static class Game
{{
    public static void Start()
    {{
        Spawn(new Ship {{ }});
        Spawn(new Ship {{ }});
    }}

    public static void Update(float dt)
    {{
        if (Time.Tick == 7)
        {{
            foreach (var s in Query<Ship>())
            {{
                Destroy(s);
                break;
            }}
        }}
    }}
}}"
    );
    let Some(exe) = build_checked(&src, "co-moves") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "23", "--no-summary"]);
    assert_eq!(code, 0, "{}", out);
    let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("total ")).collect();
    // a round takes five ticks: both ships finish the first one, then one is destroyed at tick 7
    // and the other goes on (its nested list is cleared when it holds three rows)
    let want = ["total 10 keep 4 rows 1", "total 10 keep 4 rows 1", "total 20 keep 4 rows 2", "total 30 keep 4 rows 0", "total 40 keep 4 rows 1"];
    assert_eq!(lines, want, "{}", out);
}

#[test]
fn moves_before_leaving_run_clean() {
    // the accepted early-exit and ?: moves do what they say (and free each list once)
    let src = format!(
        "{GEN}
public class Box : Entity
{{
    public mut List<int> Keep;

    public void Take(own List<int> xs) {{ Keep = xs; }}

    public void FindAndKeep(List<int> src)
    {{
        var copy = src.Clone();
        for (int i = 0; i < 3; i++)
        {{
            if (i == 1)
            {{
                Take(copy);
                return;
            }}
        }}
    }}

    public void Early(bool bTake)
    {{
        var a = new List<int> {{ 1, 2, 3 }};
        if (bTake)
        {{
            Take(a);
            return;
        }}
        Print($\"early {{a.Count}}\");
    }}

    public void Update(float dt)
    {{
        FindAndKeep(new List<int> {{ 1, 2 }});
        Print($\"keep {{Keep.Count}}\");
        var a = new List<int> {{ 1, 2, 3, 4 }};
        while (true)
        {{
            Take(a);
            break;
        }}
        Print($\"keep {{Keep.Count}}\");
        Early(Time.Tick % 2 == 0);
        Print($\"keep {{Keep.Count}}\");
        var b = new List<int> {{ 5 }};
        Print($\"tern {{(Time.Tick % 2 == 0 ? Gen.Size(b) : Gen.Size(b) + 10)}}\");
        var xs = new List<int> {{ 1, 2, 3 }};
        foreach (var x in xs)
        {{
            if (x == 2)
            {{
                Take(xs);
                break;
            }}
        }}
        Print($\"keep {{Keep.Count}}\");
    }}
}}

public static class Game
{{
    public static void Start()
    {{
        Spawn(new Box {{ }});
    }}
}}"
    );
    let Some(exe) = build_checked(&src, "early-moves") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "2", "--no-summary"]);
    assert_eq!(code, 0, "{}", out);
    let got: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let want = [
        "keep 2", "keep 4", "early 3", "keep 4", "tern 11", "keep 3", // odd tick: Early prints its list
        "keep 2", "keep 4", "keep 3", "tern 1", "keep 3", // even tick: Early takes it
    ];
    assert_eq!(got, want, "{}", out);
}

// ---------------------------------------------------------------------------
// bounds checks that must stay
// ---------------------------------------------------------------------------

fn bce_prog(body: &str) -> String {
    format!(
        "public static class Game
{{
    public static void Start() {{ }}
    public static void Update(float dt)
    {{
        mut var xs = new List<int>();
        for (int k = 0; k < 4; k++)
        {{
            xs.Add(k + 10);
        }}
        mut var d = new Dictionary<int, int>();
        d[1] = 100000;
        mut int s = 0;
        {body}
        Print($\"s {{s}}\");
    }}
}}"
    )
}

fn removed_checks(src: &str) -> usize {
    let mut d = Diags::default();
    let c = compile(src, &mut d).unwrap_or_else(|| panic!("failed to compile: {:?}", d.list));
    c.opt.bounds_checks_removed
}

#[test]
fn loops_that_change_the_list_or_the_index_keep_their_bounds_checks() {
    let bodies = [
        // the list shrinks inside the loop
        "for (int i = 0; i < xs.Count; i++) { xs.RemoveAll(x => x >= 0); s += xs[i]; }",
        "for (int i = 0; i < xs.Count; i++) { int r = xs.RemoveAll(x => x >= 12 - i); s += xs[i] + r; }",
        // an out argument writes the index
        "for (mut int i = 0; i < xs.Count; i++) { bool bOk = int.TryParse(\"100000\", out i); s += xs[i]; }",
        "for (mut int i = 0; i < xs.Count; i++) { bool bOk = d.TryGetValue(1, out i); s += xs[i]; }",
    ];
    for body in bodies {
        let src = bce_prog(body);
        assert_eq!(removed_checks(&src), 0, "a check was removed in:\n{}", body);
        let Some(exe) = build_checked(&src, "bce") else {
            eprintln!("skipping: no C compiler");
            return;
        };
        let (code, out) = run(&exe, &["--ticks", "1", "--no-summary"]);
        assert_ne!(code, 0, "{}\n{}", body, out);
        assert!(out.contains("list index out of range"), "{}\n{}", body, out);
        assert!(!out.contains("AddressSanitizer"), "{}\n{}", body, out);
    }
    // sorting keeps the length, but the check stays too (the lambda changes the list)
    assert_eq!(removed_checks(&bce_prog("for (int i = 0; i < xs.Count; i++) { xs.Sort((a, b) => b - a); s += xs[i]; }")), 0);
    // a loop that changes neither still loses its check
    assert_eq!(removed_checks(&bce_prog("for (int i = 0; i < xs.Count; i++) { s += xs[i] + xs.Count(x => x > i); }")), 1);
}

/// `g` is a 3x5 array whose elements count 1, 2, 3...
fn md_prog(body: &str) -> String {
    format!(
        "public static class Game
{{
    public static void Start() {{ }}
    public static void Update(float dt)
    {{
        mut int[,] g = new int[3, 5];
        mut int s = 0;
        for (int i = 0; i < g.Length; i++)
        {{
            s += i;
        }}
        {body}
        Print($\"s {{s}}\");
    }}
}}"
    )
}

#[test]
fn multi_dimensional_indexes_lose_only_the_checks_their_loops_prove() {
    // for x below GetLength(0), y below GetLength(1): both indexes of both accesses
    assert_eq!(removed_checks(&md_prog("for (int x = 0; x < g.GetLength(0); x++) { for (int y = 0; y < g.GetLength(1); y++) { g[x, y] = x + y; s += g[x, y]; } }")), 4);
    // known lengths: x in 0..2 and the constant 4 fit 3x5
    assert_eq!(removed_checks(&md_prog("for (int x = 0; x < 3; x++) { g[x, 4] = x; }")), 2);
    // the array is replaced in the loop: its lengths are not known any more
    assert_eq!(removed_checks(&md_prog("for (int x = 0; x < g.GetLength(0); x++) { s += g[x, 0]; g = new int[1, 1]; }")), 0);
    // indexes swapped: y runs to 4 but the first dimension has 3 rows. The check that matters
    // stays and fires (the other one is proven: x < 3 <= 5)
    let src = md_prog("for (int x = 0; x < g.GetLength(0); x++) { for (int y = 0; y < g.GetLength(1); y++) { s += g[y, x]; } }");
    assert_eq!(removed_checks(&src), 1);
    let Some(exe) = build_checked(&src, "mdbce") else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (code, out) = run(&exe, &["--ticks", "1", "--no-summary"]);
    assert_ne!(code, 0, "{}", out);
    assert!(out.contains("list index out of range"), "{}", out);
    assert!(!out.contains("AddressSanitizer"), "{}", out);
    // the proven loops run clean under the sanitizers
    let src = md_prog("for (int x = 0; x < g.GetLength(0); x++) { for (int y = 0; y < g.GetLength(1); y++) { g[x, y] = x * 5 + y; } } foreach (int v in g) { s += v; }");
    let Some(exe) = build_checked(&src, "mdbce-ok") else { return };
    let (code, out) = run(&exe, &["--ticks", "1", "--no-summary"]);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("s 210"), "{}", out);
}
