//! Dynamic components (docs/04 §4.4.1): `component C { ... }`, `e.Add(new C { ... })`,
//! `e.Remove<C>()`, `e.Has<C>()`, `e.Get<C>() is C c`, `Query<T, C>()`. Compile rules (one error
//! per mistake, with its code) and run-time behaviour: the expected timeline of a status-effect
//! program, a random add/remove workload of a few thousand entities on 1 and 4 threads and under
//! ASan/UBSan/LSan, save/restore in the middle, state migration, replication (with loss) and the
//! spatial grid on tables of component combinations.
//! The run tests need a C compiler (gcc or clang) and are skipped without one.

use sijoc::diag::{Diags, Level};
use std::path::{Path, PathBuf};
use std::process::Command;

const GAME: &str = "public static class Game { public static void Start() { } }";

fn errors(src: &str) -> Vec<&'static str> {
    let mut d = Diags::default();
    let _ = sijoc::driver::compile(src, &mut d);
    d.list.iter().filter(|x| x.level == Level::Error).map(|x| x.code).collect()
}

/// the diagnostics with their notes, for checking what an error says
fn messages(src: &str) -> String {
    let mut d = Diags::default();
    let _ = sijoc::driver::compile(src, &mut d);
    d.list.iter().map(|x| format!("{} {} {}", x.code, x.msg, x.notes.join(" "))).collect::<Vec<_>>().join("\n")
}

const BURNING: &str = "public component Burning { public mut int Left = 3; public int Power = 1;
    public void Update(float dt) { Left -= 1; if (Left <= 0) { Remove<Burning>(); } } }";

#[track_caller]
fn expect_error(body: &str, code: &str) {
    let src = format!("{}\n{}\n{}", BURNING, body, GAME);
    let got = errors(&src);
    assert_eq!(got, vec![code], "expected exactly {} for:\n{}", code, body);
}

#[track_caller]
fn expect_ok(body: &str) {
    let src = format!("{}\n{}\n{}", BURNING, body, GAME);
    let got = errors(&src);
    assert!(got.is_empty(), "expected no errors but got {:?} for:\n{}\n{}", got, body, messages(&src));
}

const SHIP: &str = "public class Ship : Entity { public mut int Hp = 10; public void Update(float dt) { } }";

#[test]
fn components_are_added_removed_tested_and_queried() {
    expect_ok("public class Ship : Entity { public mut int Hp = 10;
        public void Update(float dt) {
            if (!Has<Burning>()) { Add(new Burning { Left = 2 }); }
            if (Get<Burning>() is Burning b) { Hp -= b.Power; b.Left += 1; }
            foreach (var o in Query<Ship>()) { if (o != this) { o.Add(new Burning { }); o.Remove<Burning>(); } }
            foreach (var (s, b) in Query<Ship, Burning>()) { Hp += b.Left + s.Hp; }
            foreach (var c in Query<Burning>()) { Hp += c.Left; }
            Hp += Count<Burning>();
        } }");
    // Owner: the entity a component belongs to (a handle of any entity)
    expect_ok("public component Doom { public mut int T = 3;
        public void Update(float dt) { T -= 1; if (T <= 0) { Destroy(Owner); } if (Owner is Ship s) { s.Hp -= 1; } if (Owner.Has<Burning>()) { Owner.Remove<Burning>(); } } }
        public class Ship : Entity { public mut int Hp = 10; public void Update(float dt) { Add(new Doom { }); } }");
    // from static code, with a receiver
    expect_ok("public class Ship : Entity { public mut int Hp = 10; }
        public static class Rules { public static void Burn(Ship s) { s.Add(new Burning { Left = 1 }); } }");
}

#[test]
fn only_components_are_added_or_removed() {
    expect_error(&format!("public struct SV {{ public float X; }} {SHIP}
        public class Pilot : Entity {{ public Ship? S; public void Update(float dt) {{ if (S is Ship s) {{ s.Add(new SV {{ X = 1 }}); }} }} }}"), "E0265");
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ Remove<Ship>(); }} }}"), "E0265");
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ if (Has<Ship>()) {{ }} }} }}"), "E0265");
    // a component is created by adding it, never on its own or with Spawn
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ var b = new Burning {{ }}; }} }}"), "E0265");
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ Spawn(new Burning {{ }}); }} }}"), "E0265");
    // Add without a receiver needs an entity
    expect_error("public static class Rules { public static void F() { Add(new Burning { }); } }", "E0265");
    // an unknown field of the component
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ Add(new Burning {{ Heat = 1 }}); }} }}"), "E0108");
    // in a component, Destroy() / Destroy(this) would be ambiguous
    expect_error("public component Fade { public void Update(float dt) { Destroy(); } } public class Ship : Entity { public void Update(float dt) { Add(new Fade { }); } }", "E0265");
    expect_error("public component Fade { public void Update(float dt) { Destroy(this); } } public class Ship : Entity { public void Update(float dt) { Add(new Fade { }); } }", "E0265");
    let m = messages(&format!("public component Fade {{ public void Update(float dt) {{ Destroy(); }} }} {SHIP} {GAME}"));
    assert!(m.contains("Remove<Fade>()") && m.contains("Destroy(Owner)"), "{}", m);
}

#[test]
fn components_are_not_stored_as_values() {
    expect_error("public class Ship : Entity { public Burning? B; }", "E0263");
    expect_error("public struct SFx { public Burning? B; }", "E0263");
    expect_error(&format!("{SHIP} public static class Rules {{ public static void F(Burning b) {{ }} }}"), "E0263");
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public Burning? Find() => null; }}"), "E0263");
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ mut var xs = new List<Burning>(); }} }}"), "E0263");
    let m = messages(&format!("public class Ship : Entity {{ public Burning? B; }} {BURNING} {GAME}"));
    assert!(m.contains("Get<Burning>() is Burning"), "{}", m);
    // a local of the component type is fine (it lives inside one system)
    expect_ok(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ foreach (var s in Query<Ship>()) {{ Burning? b = s.Get<Burning>(); if (b is Burning x) {{ x.Left += 1; }} }} }} }}"));
}

#[test]
fn get_needs_an_is_check() {
    expect_error("public class Ship : Entity { public mut int Hp; public void Update(float dt) { Hp = Get<Burning>().Left; } }", "E0301");
    let m = messages(&format!("public class Ship : Entity {{ public mut int Hp; public void Update(float dt) {{ foreach (var o in Query<Ship>()) {{ Hp = o.Get<Burning>().Left; }} }} }} {BURNING} {GAME}"));
    assert!(m.contains("if (o.Get<Burning>() is Burning c)"), "{}", m);
    // `is` tests the class of an entity, components are tested with Has / Get
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public Ship? S; public void Update(float dt) {{ if (S is Burning b) {{ }} }} }}"), "E0235");
    let m = messages(&format!("{SHIP} public class Pilot : Entity {{ public Ship? S; public void Update(float dt) {{ if (S is Burning b) {{ }} }} }} {BURNING} {GAME}"));
    assert!(m.contains("Has<Burning>()"), "{}", m);
    // in a component, `this` is the component; the entity is Owner
    let m = messages(&format!("public component Fx {{ public void Update(float dt) {{ if (this is Ship s) {{ }} }} }} {SHIP} {GAME}"));
    assert!(m.contains("E0235") && m.contains("Owner is Ship"), "{}", m);
}

#[test]
fn component_fields_follow_the_mut_rules() {
    // Power is not mut: it is set by Add and never changes
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ foreach (var s in Query<Ship>()) {{ if (s.Get<Burning>() is Burning b) {{ b.Power = 3; }} }} }} }}"), "E0310");
    expect_error("public component Slow { public float K = 1; public void Update(float dt) { K = 2; } } public class Ship : Entity { public void Update(float dt) { Add(new Slow { K = 0.5 }); } }", "E0310");
    // ranges hold: an initializer outside the range saturates (W0202 only for the default)
    expect_ok("public component Heat { public mut int in 0..10 T = 5; public void Update(float dt) { T += 20; } }
        public class Ship : Entity { public void Update(float dt) { Add(new Heat { T = 50 }); } }");
}

#[test]
fn component_declarations_are_checked() {
    expect_error("public component Fx : Entity { }", "E0264");
    expect_error("public static component Fx { }", "E0264");
    // a component's coroutine: its frame lives with the component (Remove ends it, Add starts it again)
    expect_ok("public component Fx { public mut int N; public async void Run() { await NextTick(); N += 1; } }");
    expect_error("public component Fx { public virtual void Tick() { } }", "E0264");
    // strings, lists and arrays travel in the Add command like message arguments; dictionaries and
    // delegates do not
    expect_ok("public component Fx { public mut List<int> Hits; public string Name = \"x\"; public mut int[] Slots = new int[2]; }");
    expect_error("public component Fx { public mut Dictionary<int, int> Seen; }", "E0264");
    expect_error("public component Fx { public Action Then; }", "E0264");
    // Spawn's further arguments are the components the entity starts with
    expect_ok(&format!("{SHIP} public static class G {{ public static void F() {{ Spawn(new Ship {{ }}, new Burning {{ Left = 2 }}); }} }}"));
    expect_error(&format!("{SHIP} public static class G {{ public static void F() {{ Spawn(new Ship {{ }}, new Ship {{ }}); }} }}"), "E0265");
    expect_error(&format!("{SHIP} public static class G {{ public static void F() {{ Spawn(new Ship {{ }}, new Burning {{ }}, new Burning {{ }}); }} }}"), "E0104");
    expect_error(&format!("{SHIP} public static class G {{ public static void F() {{ Spawn(new Ship {{ }}, new Burning {{ Nope = 1 }}); }} }}"), "E0108");
    expect_error("public component Fx { public Fx() { } }", "E0217");
    expect_error("public class Ship : Burning { }", "E0209");
    expect_error("public component Fx { public void Update(int n) { } }", "E0223");
    // Owner only means something in a component
    expect_error("public class Ship : Entity { public void Update(float dt) { if (Owner is Ship s) { } } }", "E0101");
    // `component` stays a name elsewhere
    expect_ok("public class Ship : Entity { public mut int component; public void Update(float dt) { component += 1; } }");
}

#[test]
fn queries_name_components_after_the_class() {
    expect_error(&format!("{SHIP} public class Rock : Entity {{ }} public class Pilot : Entity {{ public void Update(float dt) {{ foreach (var s in Query<Ship, Rock>()) {{ }} }} }}"), "E0231");
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ foreach (var (s, b, c) in Query<Ship, Burning>()) {{ }} }} }}"), "E0231");
    // a component named twice: said so (not "not a component")
    let twice = format!("{SHIP} public class Pilot : Entity {{ public void Update(float dt) {{ foreach (var s in Query<Ship, Burning, Burning>()) {{ }} }} }}");
    expect_error(&twice, "E0231");
    assert!(messages(&format!("{}\n{}\n{}", BURNING, twice, GAME)).contains("'Burning' is listed twice"));
    expect_error(&format!("{SHIP} public class Pilot : Entity {{ public mut List<int> Xs; public void Update(float dt) {{ foreach (var (a, b) in Xs) {{ }} }} }}"), "E0231");
}

#[test]
fn clients_cannot_change_components() {
    expect_error("public class Ship : Entity { public client void Render() { Add(new Burning { }); } }", "E0601");
    // ...but a client calling a method that does becomes an RPC; clients read components
    expect_ok("public class Ship : Entity { public mut int Hp;
        public void Ignite() { Add(new Burning { }); }
        public client void Input() { if (!Has<Burning>()) { Ignite(); } }
        public client void Render() { if (Get<Burning>() is Burning b) { Draw.Sprite(b.Left, 0, 0); } } }");
}

#[test]
fn combinations_beyond_sixteen_share_one_open_table() {
    // five components that every ship can gain and lose: 32 combinations, more than 16 tables, so
    // Ship keeps one open table with a bit per component
    let comps: String = (0..5).map(|i| format!("public component C{i} {{ public mut int T; }}")).collect();
    let adds: String = (0..5).map(|i| format!("if (Hp == {i}) {{ Add(new C{i} {{ }}); }} ")).collect();
    let src = format!("{comps} public class Ship : Entity {{ public mut int Hp; public void Update(float dt) {{ {adds} }} }} {GAME}");
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).expect("compiles");
    let names: Vec<String> = c.ecs.archetypes.iter().map(|&a| c.prog.classes[a].name.clone()).collect();
    assert_eq!(names, vec!["Ship"]);
    let ship = c.ecs.archetypes[0];
    assert!(c.prog.open.contains_key(&ship));
    let sm = sijoc::diag::SourceMap::single("open.sijo", &src);
    let report = sijoc::explain::report(&c, &sm);
    assert!(report.contains("class Ship: one open table") && report.contains("columns of C0, C1, C2, C3, C4"), "{}", report);
    // more kinds of components than the bits of the presence field
    let comps: String = (0..33).map(|i| format!("public component C{i} {{ public mut int T; }}")).collect();
    let adds: String = (0..33).map(|i| format!("if (Hp == {i}) {{ Add(new C{i} {{ }}); }} ")).collect();
    let src = format!("{comps} public class Ship : Entity {{ public mut int Hp; public void Update(float dt) {{ {adds} }} }} {GAME}");
    assert_eq!(errors(&src), vec!["E0266"]);
    let m = messages(&src);
    assert!(m.contains("33 kinds of components") && m.contains("at most 32"), "{}", m);
    // four are fine; combinations only reachable through a component's own method are counted
    // exactly (B can only be added while A is there)
    let src = "public component A { public void Update(float dt) { Add(new B { }); Remove<A>(); } } public component B { }
        public class Ship : Entity { public void Update(float dt) { Add(new A { }); } }"
        .to_string()
        + GAME;
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).expect("compiles");
    let names: Vec<String> = c.ecs.archetypes.iter().map(|&a| c.prog.classes[a].name.clone()).collect();
    assert_eq!(names, vec!["Ship", "Ship+A", "Ship+B", "Ship+A+B"]);
}

#[test]
fn explain_lists_combinations_and_component_systems() {
    let src = std::fs::read_to_string(root().join("tests/programs/components.sijo")).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).expect("compiles");
    let sm = sijoc::diag::SourceMap::single("components.sijo", &src);
    let report = sijoc::explain::report(&c, &sm);
    assert!(report.contains("archetype Ship+Burning"), "{}", report);
    assert!(report.contains("Burning.Update over ["), "{}", report);
    assert!(report.contains("adds/removes components of self"), "{}", report);
    // the component's Update runs on every class that can have it
    let sys = c.ecs.systems.iter().find(|s| s.name == "Burning.Update").unwrap();
    let names: Vec<&str> = sys.archetypes.iter().map(|&a| c.prog.classes[a].name.as_str()).collect();
    assert!(names.contains(&"Ship+Burning") && names.contains(&"Fighter+Burning") && names.contains(&"Rock+Burning"), "{:?}", names);
    assert!(!names.contains(&"Ship"), "{:?}", names);
    // the methods are compiled once for all the tables of a class (or of a component)
    assert!(report.contains("compiled once (through a view of each table's columns): the methods of Ship on its 8 tables"), "{}", report);
    assert!(report.contains("the methods of component Burning on its 10 tables"), "{}", report);
}

// ---------------------------------------------------------------------------
// run time
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

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sijoc-components-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const SANITIZE: &[&str] = &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"];

/// Sijo source -> C -> executable; None without a C compiler (or without OpenMP / the sanitizers,
/// when they are asked for)
fn build(src: &str, tag: &str, flags: &[&str]) -> Option<PathBuf> {
    let cc = cc()?;
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).unwrap_or_else(|| panic!("{} failed to compile: {:?}", tag, d.list));
    let dir = out_dir(tag);
    let cfile = dir.join("prog.c");
    let exe = dir.join("prog");
    std::fs::write(&cfile, sijoc::codegen::generate(&c, &format!("{tag}.sijo"))).unwrap();
    let mut args: Vec<String> = vec!["-std=gnu11".into(), "-O2".into(), "-ffp-contract=off".into(), "-w".into()];
    args.extend(flags.iter().map(|s| s.to_string()));
    args.push(cfile.display().to_string());
    args.push("-o".into());
    args.push(exe.display().to_string());
    args.push("-lm".into());
    let out = Command::new(&cc).args(&args).output().unwrap();
    if !out.status.success() {
        if flags.iter().any(|f| f.starts_with("-fsanitize") || *f == "-fopenmp") {
            return None;
        }
        panic!("C compilation failed for {}:\n{}", tag, String::from_utf8_lossy(&out.stderr));
    }
    Some(exe)
}

/// the same with every class that can gain components in one open table (`--open-tables`)
fn build_open(src: &str, tag: &str, flags: &[&str]) -> Option<PathBuf> {
    let cc = cc()?;
    let mut d = Diags::default();
    let tune = sijoc::driver::Tuning { open_tables: true, ..Default::default() };
    let c = sijoc::driver::compile_tuned(src, &mut d, &sijoc::driver::Options::default(), &tune).unwrap_or_else(|| panic!("{} failed to compile: {:?}", tag, d.list));
    let dir = out_dir(tag);
    let cfile = dir.join("prog.c");
    let exe = dir.join("prog");
    std::fs::write(&cfile, sijoc::codegen::generate(&c, &format!("{tag}.sijo"))).unwrap();
    let mut args: Vec<String> = vec!["-std=gnu11".into(), "-O2".into(), "-ffp-contract=off".into(), "-w".into(), "-fopenmp".into()];
    args.extend(flags.iter().map(|s| s.to_string()));
    args.push(cfile.display().to_string());
    args.push("-o".into());
    args.push(exe.display().to_string());
    args.push("-lm".into());
    let out = Command::new(&cc).args(&args).output().unwrap();
    if !out.status.success() {
        return None;
    }
    Some(exe)
}

/// the printed lines of each tick, in sorted order (an open table visits a class's entities in row
/// order, combination tables one table after another)
fn per_tick(lines: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = vec![];
    let mut cur: Option<String> = None;
    for l in lines.lines().filter(|l| !l.trim().is_empty()) {
        let tick = l.split(|c: char| c == ':' || c == ' ').next().unwrap_or("").to_string();
        if cur.as_ref() != Some(&tick) {
            out.push(vec![]);
            cur = Some(tick);
        }
        out.last_mut().unwrap().push(l.to_string());
    }
    for t in &mut out {
        t.sort();
    }
    out
}

/// OpenMP when the compiler has it
fn build_par(src: &str, tag: &str, flags: &[&str]) -> Option<PathBuf> {
    let mut f: Vec<&str> = flags.to_vec();
    f.push("-fopenmp");
    build(src, &format!("{tag}-omp"), &f).or_else(|| build(src, tag, flags))
}

fn run(exe: &Path, args: &[&str], threads: usize) -> (i32, String) {
    let out = Command::new(exe).args(args).env("OMP_NUM_THREADS", threads.to_string()).env("ASAN_OPTIONS", "detect_leaks=1").output().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

fn line<'a>(out: &'a str, prefix: &str) -> &'a str {
    out.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no '{}' line in output:\n{}", prefix, out))
}

/// The status-effect timeline of components.sijo (every line checked by hand against the rules of
/// docs/04 §4.4.1): 1 and 4 threads, sanitizers, a save in the middle restored to the same end,
/// and the client's view of components (replication check).
#[test]
fn status_effects_follow_the_system_rule() {
    let path = root().join("tests/programs/components.sijo");
    let src = std::fs::read_to_string(&path).unwrap();
    let expected = std::fs::read_to_string(root().join("tests/programs/components.expected")).unwrap();
    let Some(exe) = build_par(&src, "status", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut exes = vec![exe.clone()];
    if let Some(asan) = build_par(&src, "status-asan", SANITIZE) {
        exes.push(asan);
    }
    for exe in &exes {
        for t in [1, 4] {
            let (code, out) = run(exe, &["--ticks", "12", "--no-summary"], t);
            assert_eq!(code, 0, "{}", out);
            assert_eq!(out.trim(), expected.trim(), "threads={} {}", t, exe.display());
            let (code, out) = run(exe, &["--ticks", "12", "--quiet"], t);
            assert_eq!(code, 0, "{}", out);
            assert!(line(&out, "replication check:").contains("OK"), "{}", out);
        }
    }
    // saved at tick 6 (ship 2 burning, ship 3 shielded, the rock burning, the coroutine waiting)
    let (_, full) = run(&exe, &["--ticks", "12", "--quiet"], 1);
    let st = out_dir("status-state").join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "6", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let saved = std::fs::read_to_string(&st).unwrap();
    assert!(saved.contains("arch Ship+Burning ") && saved.contains("field Burning.Left int"), "{}", saved);
    let (code, resumed) = run(&exe, &["--ticks", "6", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(line(&resumed, "world checksum:"), line(&full, "world checksum:"));
    // the printed lines of ticks 7..12 are those of the full run
    let tail: Vec<&str> = expected.lines().skip_while(|l| !l.starts_with("7:")).collect();
    let printed: Vec<&str> = resumed.lines().filter(|l| !l.starts_with("[live]") && !l.starts_with("==")).take_while(|l| !l.starts_with("ticks:")).collect();
    assert_eq!(printed, tail);
}

const WORKLOAD: &str = r#"
public struct SVec2
{
    public float X;
    public float Y;

    public SVec2(float x, float y)
    {
        X = x;
        Y = y;
    }
}

public component Burning
{
    public mut float Left = 1;
    public int in 0..15 Power = 1;
    public Ship? Source;

    public void Update(float dt)
    {
        Left -= dt;
        if (Owner is Ship s)
        {
            s.Hp -= Power;
        }
        if (Left <= 0)
        {
            Remove<Burning>();
        }
    }

    public void Fuel(float more)
    {
        Left += more;
    }
}

public component Frozen
{
    public mut int Ticks = 3;
    public SVec2 At;

    public void Update(float dt)
    {
        Ticks -= 1;
        if (Ticks <= 0)
        {
            Remove<Frozen>();
            if (Owner is Ship s)
            {
                s.Add(new Burning { Left = 0.1, Power = 2 });
            }
        }
    }
}

public component Marked
{
}

public class Ship : Entity
{
    public int Id;
    public mut int Hp = 100000;
    public mut SVec2 Pos;
    public mut List<int> Log;
    public mut string Tag = "s";
    public mut int Seen;

    public void Update(float dt)
    {
        Pos = new SVec2(Pos.X + 0.5, Pos.Y);
        int r = Random.Int(0, 100);
        if (r < 5)
        {
            Add(new Burning { Left = 0.3, Power = r % 7, Source = this });
        }
        else if (r < 8)
        {
            Add(new Frozen { Ticks = 2 + r % 3, At = Pos });
        }
        else if (r < 10)
        {
            Remove<Burning>();
        }
        else if (r < 12)
        {
            Add(new Marked { });
        }
        else if (r < 13)
        {
            Remove<Marked>();
        }
        if (r == 50 && Get<Burning>() is Burning b)
        {
            b.Left += 0.25;
            b.Fuel(0.1);
        }
        if (r == 60)
        {
            Log.Add(Time.Tick);
            Tag = $"t{Time.Tick}";
        }
        if (r == 70)
        {
            // set another ship on fire (deferred, on whatever table it is in)
            foreach (var o in Query<Ship, Frozen>())
            {
                if (o.Id == (Id * 7) % 3000)
                {
                    o.Add(new Burning { Left = 0.2, Power = 3 });
                }
            }
        }
        if (Has<Frozen>() && Has<Burning>())
        {
            Seen += Tag.Length;
        }
    }

    public client void Render()
    {
        if (Get<Frozen>() is Frozen f)
        {
            Draw.Sprite(f.Ticks, Pos.X, Pos.Y);
        }
    }
}

public class Rock : Entity
{
    public mut int Cracks;

    public void Update(float dt)
    {
        if (Random.Int(0, 50) == 0)
        {
            Add(new Burning { Left = 0.05 });
        }
        if (Get<Burning>() is Burning b)
        {
            Cracks += b.Power;
        }
    }
}

public static class Game
{
    public static void Start()
    {
        for (int i = 0; i < 3000; i++)
        {
            Spawn(new Ship { Id = i, Pos = new SVec2(i, 0) });
        }
        for (int i = 0; i < 500; i++)
        {
            Spawn(new Rock { });
        }
    }

    public static void Update(float dt)
    {
        if (Time.Tick % 50 == 0)
        {
            mut int fb = 0;
            foreach (var (s, f, b) in Query<Ship, Frozen, Burning>())
            {
                fb += f.Ticks + b.Power;
            }
            mut int hp = 0;
            foreach (var s in Query<Ship>())
            {
                hp += s.Hp + s.Seen + s.Log.Count;
            }
            Print($"tick {Time.Tick}: burning={Count<Burning>()} frozen={Count<Frozen>()} marked={Count<Marked>()} fb={fb} hp={hp}");
        }
    }
}
"#;

/// A few thousand entities gaining and losing three components at random every tick (adds on self
/// and on others, removes, re-adds that replace values, writes and messages to components, lists
/// and strings on the class moving with their rows): the same output and world on 1 and 4 threads
/// and 3 simulated shards, clean under the sanitizers, and a save in the middle restores exactly.
#[test]
fn random_component_churn_is_deterministic() {
    let Some(exe) = build_par(WORKLOAD, "churn", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut outs = vec![];
    for (t, shards) in [(1, "1"), (4, "1"), (4, "3")] {
        let (code, out) = run(&exe, &["--ticks", "200", "--shards", shards], t);
        assert_eq!(code, 0, "{}", out);
        assert!(line(&out, "replication check:").contains("OK"), "{}", out);
        let printed: Vec<&str> = out.lines().filter(|l| l.starts_with("tick ")).collect();
        outs.push((printed.join("\n"), line(&out, "world checksum:").to_string()));
    }
    assert!(outs.windows(2).all(|w| w[0] == w[1]), "nondeterministic: {:?}", outs);
    // every combination of the three components occurs
    let (_, out) = run(&exe, &["--ticks", "200"], 4);
    assert!(line(&out, "entities alive:").contains("Ship+Burning+Frozen+Marked="), "{}", out);
    // save at tick 90, restore, run to 200: the same world
    let st = out_dir("churn-state").join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "90", "--quiet", "--save-state", &st], 4);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "110", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(line(&resumed, "world checksum:"), outs[0].1);
    // replication over a lossy link (30% loss, duplicates): the client converges
    let (code, out) = run(&exe, &["--ticks", "120", "--quiet", "--sim-loss", "0.3", "--sim-dup", "0.1"], 4);
    assert_eq!(code, 0, "{}", out);
    assert!(line(&out, "replication check:").contains("OK"), "{}", out);
    if let Some(asan) = build_par(WORKLOAD, "churn-asan", SANITIZE) {
        for t in [1, 4] {
            let (code, out) = run(&asan, &["--ticks", "60", "--quiet"], t);
            assert_eq!(code, 0, "sanitizers, {} threads:\n{}", t, out);
        }
    }
}

/// The C generated for components (payloads, row moves, the command applier) compiles without
/// warnings, as the rest of the generated code does (e2e: generated_c_is_warning_free).
#[test]
fn generated_component_code_is_warning_free() {
    let have = |c: &str| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    let dir = out_dir("werror");
    let status = std::fs::read_to_string(root().join("tests/programs/components.sijo")).unwrap();
    let open = std::fs::read_to_string(root().join("tests/programs/opentable.sijo")).unwrap();
    // (the last two: every class in an open table, `--open-tables`)
    for (tag, src, open_tables) in [("status", status.clone(), false), ("churn", WORKLOAD.to_string(), false), ("opentable", open, false), ("status-open", status, true), ("churn-open", WORKLOAD.to_string(), true)] {
        let mut d = Diags::default();
        let tune = sijoc::driver::Tuning { open_tables, ..Default::default() };
        let c = sijoc::driver::compile_tuned(&src, &mut d, &sijoc::driver::Options::default(), &tune).expect("compiles");
        let cfile = dir.join(format!("{tag}.c"));
        std::fs::write(&cfile, sijoc::codegen::generate(&c, tag)).unwrap();
        for cc in ["gcc", "clang"] {
            if !have(cc) {
                continue;
            }
            for opt in ["-O0", "-O2"] {
                let out = Command::new(cc).args(["-std=gnu11", opt, "-ffp-contract=off", "-Wall", "-Wextra", "-pedantic", "-Werror", "-c"]).arg(&cfile).arg("-o").arg(dir.join(format!("{tag}.o"))).output().unwrap();
                assert!(out.status.success(), "{} {} {}: warnings in generated C:\n{}", cc, opt, tag, String::from_utf8_lossy(&out.stderr));
            }
        }
    }
}

/// Live coding: a state saved by one version of the program loads into the next one by names,
/// including combinations the new program no longer has (a component removed from the program).
#[test]
fn state_migrates_when_components_change() {
    // (Left and Hp are read, so they have storage in this optimized build)
    let v1 = "public component Burning { public mut int Left = 5; public void Update(float dt) { Left -= 1; if (Left < -1000) { Remove<Burning>(); } } }
        public component Wet { public mut int Drops = 2; }
        public class Ship : Entity { public int Id; public mut int Hp = 50;
          public void Update(float dt) { Hp -= 1; if (Hp < -1000) { Destroy(); } if (Time.Tick == 2 && Id == 1) { Add(new Burning { Left = 100 }); Add(new Wet { Drops = 7 }); } if (Time.Tick == 2 && Id == 2) { Add(new Wet { Drops = 9 }); } } }
        public static class Game { public static void Start() { Spawn(new Ship { Id = 1 }); Spawn(new Ship { Id = 2 }); Spawn(new Ship { Id = 3 }); } }";
    // Wet is gone, Burning gained a field
    let v2 = "public component Burning { public mut int Left = 5; public int Power = 4; public void Update(float dt) { Left -= 1; } }
        public class Ship : Entity { public int Id; public mut int Hp = 50;
          public void Update(float dt) { Hp -= 1; if (Time.Tick == 99) { Add(new Burning { }); } } }
        public static class Game { public static void Start() { }
          public static void Update(float dt) { foreach (var s in Query<Ship>()) { string b = s.Get<Burning>() is Burning x ? $\"burning {x.Left} power {x.Power}\" : \"-\"; Print($\"ship {s.Id} hp={s.Hp} {b}\"); } } }";
    let (Some(e1), Some(e2)) = (build(v1, "mig-v1", &[]), build(v2, "mig-v2", &[])) else { return };
    let st = out_dir("mig-state").join("state.txt").display().to_string();
    let (code, _) = run(&e1, &["--ticks", "10", "--quiet", "--save-state", &st], 1);
    assert_eq!(code, 0);
    let (code, out) = run(&e2, &["--ticks", "1", "--load-state", &st, "--no-summary"], 1);
    assert_eq!(code, 0, "{}", out);
    // ship 1 was Ship+Burning+Wet: now Ship+Burning (Left continues 100 - 8 - 1, Power its default)
    assert!(out.contains("ship 1 hp=39 burning 91 power 4"), "{}", out);
    // ship 2 was Ship+Wet: now a plain Ship
    assert!(out.contains("ship 2 hp=39 -") && out.contains("ship 3 hp=39 -"), "{}", out);
    assert!(out.contains("components-dropped:Ship+Burning+Wet") && out.contains("components-dropped:Ship+Wet"), "{}", out);
}

/// The proximity grid (docs/04 §4.12) is built over every table of a class, combinations included:
/// the grid run and the full scan give the same world.
#[test]
fn spatial_grid_covers_component_tables() {
    let src = r#"
public struct SVec2
{
    public float X;
    public float Y;

    public SVec2(float x, float y)
    {
        X = x;
        Y = y;
    }

    public float LengthSq => X * X + Y * Y;

    public static SVec2 operator -(SVec2 a, SVec2 b) => new SVec2(a.X - b.X, a.Y - b.Y);
}

public component Glowing
{
    public mut int T = 10;

    public void Update(float dt)
    {
        T -= 1;
        if (T <= 0)
        {
            Remove<Glowing>();
        }
    }
}

public class Mote : Entity
{
    public mut SVec2 Pos;
    public mut int Near;
    public mut int Lit;

    public void Update(float dt)
    {
        Pos = new SVec2(Pos.X + Random.Float(-1, 1), Pos.Y + Random.Float(-1, 1));
        mut int n = 0;
        foreach (var o in Query<Mote>())
        {
            if ((o.Pos - Pos).LengthSq < 4)
            {
                n += 1;
            }
        }
        Near = n;
        // the same kind of loop over the combination tables that have Glowing only
        mut int lit = 0;
        foreach (var (o, g) in Query<Mote, Glowing>())
        {
            if ((o.Pos - Pos).LengthSq < 9)
            {
                lit += g.T;
            }
        }
        Lit = lit;
        if (n > 3 && !Has<Glowing>())
        {
            Add(new Glowing { T = n });
        }
    }
}

public static class Game
{
    public static void Start()
    {
        for (int i = 0; i < 1500; i++)
        {
            Spawn(new Mote { Pos = new SVec2((i * 37) % 100, (i * 91) % 100) });
        }
    }

    public static void Update(float dt)
    {
        if (Time.Tick % 20 == 0)
        {
            mut int near = 0;
            mut int lit = 0;
            foreach (var m in Query<Mote>())
            {
                near += m.Near;
                lit += m.Lit;
            }
            Print($"tick {Time.Tick}: glowing={Count<Glowing>()} near={near} lit={lit}");
        }
    }
}
"#;
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).expect("compiles");
    assert_eq!(c.spatial.loops.len(), 2, "both distance loops (Query<Mote>, Query<Mote, Glowing>) should be grid-accelerated: {:?}", c.spatial.rejected);
    assert_eq!(c.spatial.loops.iter().filter(|l| !l.with.is_empty()).count(), 1);
    let (Some(grid), Some(scan)) = (build_par(src, "grid", &[]), build_par(src, "scan", &["-DSIJO_NO_SPATIAL"])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let (c1, a) = run(&grid, &["--ticks", "60"], 4);
    let (c2, b) = run(&scan, &["--ticks", "60"], 1);
    assert_eq!((c1, c2), (0, 0), "{}\n{}", a, b);
    assert!(line(&a, "entities alive:").contains("Mote+Glowing="), "{}", a);
    assert_eq!(line(&a, "world checksum:"), line(&b, "world checksum:"));
    let pa: Vec<&str> = a.lines().filter(|l| l.starts_with("tick ")).collect();
    let pb: Vec<&str> = b.lines().filter(|l| l.starts_with("tick ")).collect();
    assert_eq!(pa, pb);
    assert!(pa.iter().any(|l| !l.ends_with("lit=0")), "{:?}", pa);
    // the grids on the combination tables also hold on shards (halo copies of those tables)
    let (c3, s3) = run(&grid, &["--ticks", "60", "--shards", "3"], 4);
    assert_eq!(c3, 0, "{}", s3);
    assert_eq!(line(&s3, "world checksum:"), line(&b, "world checksum:"));
}

/// Entities spawned with components, component fields holding strings, lists and arrays (copied
/// by Add, freed with the component), and component coroutines (Remove ends one, Add starts it
/// again): the hand-checked timeline of compfields.sijo on 1 and 4 threads and 3 shard worlds, a
/// save in the middle that restores to the same end, and no leaks under the sanitizers.
#[test]
fn component_strings_lists_coroutines_and_spawned_components() {
    let path = root().join("tests/programs/compfields.sijo");
    let src = std::fs::read_to_string(&path).unwrap();
    let expected = std::fs::read_to_string(root().join("tests/programs/compfields.expected")).unwrap();
    let Some(exe) = build_par(&src, "compfields", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sums = vec![];
    for (t, shards) in [(1, "1"), (4, "1"), (4, "3")] {
        let (code, out) = run(&exe, &["--ticks", "10", "--shards", shards], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={} shards={}", t, shards);
        sums.push(line(&out, "world checksum:").to_string());
    }
    assert!(sums.windows(2).all(|w| w[0] == w[1]), "{:?}", sums);
    // a save while coroutines wait and components hold lists restores to the same end
    let st = out_dir("compfields-state").join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "5", "--quiet", "--save-state", &st], 4);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "5", "--quiet", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(line(&resumed, "world checksum:"), sums[0]);
    if let Some(asan) = build(&src, "compfields-asan", SANITIZE) {
        let (code, out) = run(&asan, &["--ticks", "12", "--quiet"], 1);
        assert_eq!(code, 0, "compfields.sijo failed under sanitizers:\n{}", out);
    }
}

/// A class that can reach 64 combinations keeps one open table (opentable.sijo checks Has, Get,
/// queries and counts against the bits every hero applied; the expected output was checked tick by
/// tick against a separate model of the rules): 1 and 4 threads, 3 simulated shards, the client's
/// view, a save in the middle, sanitizers.
#[test]
fn open_tables_hold_more_than_sixteen_combinations() {
    let path = root().join("tests/programs/opentable.sijo");
    let src = std::fs::read_to_string(&path).unwrap();
    let expected = std::fs::read_to_string(root().join("tests/programs/opentable.expected")).unwrap();
    let Some(exe) = build_par(&src, "opentable", &[]) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sums = vec![];
    for (t, shards) in [(1, "1"), (4, "1"), (4, "3")] {
        let (code, out) = run(&exe, &["--ticks", "16", "--shards", shards], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={} shards={}", t, shards);
        assert!(line(&out, "replication check:").contains("OK"), "{}", out);
        sums.push(line(&out, "world checksum:").to_string());
    }
    assert!(sums.windows(2).all(|w| w[0] == w[1]), "{:?}", sums);
    let st = out_dir("opentable-state").join("state.txt").display().to_string();
    let (code, _) = run(&exe, &["--ticks", "7", "--quiet", "--save-state", &st], 4);
    assert_eq!(code, 0);
    let (code, resumed) = run(&exe, &["--ticks", "9", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(line(&resumed, "world checksum:"), sums[0]);
    let tail: Vec<&str> = expected.lines().skip(7).collect();
    let printed: Vec<&str> = resumed.lines().filter(|l| l.starts_with('t') && l.contains("heroes")).collect();
    assert_eq!(printed, tail);
    if let Some(asan) = build_par(&src, "opentable-asan", SANITIZE) {
        for t in [1, 4] {
            let (code, out) = run(&asan, &["--ticks", "70", "--quiet"], t);
            assert_eq!(code, 0, "sanitizers, {} threads:\n{}", t, out);
        }
    }
}

/// `--open-tables` puts every class that can gain components in one open table: the hand-checked
/// timeline of components.sijo and the strings, lists, coroutines and spawns of compfields.sijo
/// print the same lines each tick (an open table visits its rows in row order, so the order within
/// a tick can differ), and the random churn is as deterministic, replicates, migrates from a state
/// saved with combination tables and, without its one write whose winner depends on the visiting
/// order, prints exactly what the combination tables print.
#[test]
fn open_tables_match_combination_tables() {
    let src = std::fs::read_to_string(root().join("tests/programs/components.sijo")).unwrap();
    let expected = std::fs::read_to_string(root().join("tests/programs/components.expected")).unwrap();
    let Some(exe) = build_open(&src, "open-status", &[]) else {
        eprintln!("skipping: no C compiler with OpenMP");
        return;
    };
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "12", "--no-summary"], t);
        assert_eq!(code, 0, "{}", out);
        assert_eq!(per_tick(&out), per_tick(&expected), "threads={}", t);
        let (code, out) = run(&exe, &["--ticks", "12", "--quiet"], t);
        assert_eq!(code, 0, "{}", out);
        assert!(line(&out, "replication check:").contains("OK"), "{}", out);
    }
    if let Some(asan) = build_open(&src, "open-status-asan", SANITIZE) {
        let (code, out) = run(&asan, &["--ticks", "12", "--no-summary"], 4);
        assert_eq!(code, 0, "{}", out);
        assert_eq!(per_tick(&out), per_tick(&expected));
    }
    let src = std::fs::read_to_string(root().join("tests/programs/compfields.sijo")).unwrap();
    let expected = std::fs::read_to_string(root().join("tests/programs/compfields.expected")).unwrap();
    let exe = build_open(&src, "open-compfields", &[]).unwrap();
    for (t, shards) in [(1, "1"), (4, "3")] {
        let (code, out) = run(&exe, &["--ticks", "10", "--shards", shards], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(per_tick(&printed.join("\n")), per_tick(&expected), "threads={} shards={}", t, shards);
    }
    // the churn: one table per class, deterministic, replicated over a lossy link
    let open = build_open(WORKLOAD, "open-churn", &[]).unwrap();
    let mut outs = vec![];
    for (t, shards) in [(1, "1"), (4, "1"), (4, "3")] {
        let (code, out) = run(&open, &["--ticks", "200", "--shards", shards], t);
        assert_eq!(code, 0, "{}", out);
        assert!(line(&out, "replication check:").contains("OK"), "{}", out);
        let printed: Vec<&str> = out.lines().filter(|l| l.starts_with("tick ")).collect();
        outs.push((printed.join("\n"), line(&out, "world checksum:").to_string()));
    }
    assert!(outs.windows(2).all(|w| w[0] == w[1]), "nondeterministic: {:?}", outs);
    let (_, out) = run(&open, &["--ticks", "200"], 4);
    assert!(line(&out, "entities alive:").contains("Ship=3000 Rock=500"), "{}", out);
    let (code, out) = run(&open, &["--ticks", "120", "--quiet", "--sim-loss", "0.3", "--sim-dup", "0.1"], 4);
    assert_eq!(code, 0, "{}", out);
    assert!(line(&out, "replication check:").contains("OK"), "{}", out);
    // a state saved with combination tables: every row lands in the open table with its bits, and
    // the next printed tick is the one the combination tables print
    let closed = build_par(WORKLOAD, "closed-churn", &[]).unwrap();
    let (_, full) = run(&closed, &["--ticks", "200"], 4);
    let st = out_dir("open-churn-state").join("state.txt").display().to_string();
    let (code, _) = run(&closed, &["--ticks", "90", "--quiet", "--save-state", &st], 4);
    assert_eq!(code, 0);
    let (code, resumed) = run(&open, &["--ticks", "10", "--load-state", &st], 1);
    assert_eq!(code, 0, "{}", resumed);
    assert_eq!(line(&resumed, "tick 100:"), line(&full, "tick 100:"));
    assert!(!resumed.contains("components-dropped"), "{}", resumed);
    // the same totals as the combination tables when no write's winner depends on the order
    let fair = WORKLOAD.replace("if (r == 70)", "if (r == 1000)");
    let a = build_par(&fair, "fair-closed", &[]).unwrap();
    let b = build_open(&fair, "fair-open", &[]).unwrap();
    let ticks = |exe: &Path| {
        let (code, out) = run(exe, &["--ticks", "200", "--no-summary"], 4);
        assert_eq!(code, 0, "{}", out);
        out.lines().filter(|l| l.starts_with("tick ")).map(String::from).collect::<Vec<_>>()
    };
    assert_eq!(ticks(&a), ticks(&b));
    if let Some(asan) = build_open(WORKLOAD, "open-churn-asan", SANITIZE) {
        for t in [1, 4] {
            let (code, out) = run(&asan, &["--ticks", "60", "--quiet"], t);
            assert_eq!(code, 0, "sanitizers, {} threads:\n{}", t, out);
        }
    }
}

/// Shared method code: a class's methods are compiled once for all its combination tables and a
/// component's methods once for every table that has it (through a view of the table's columns),
/// so the C does not grow with tables times methods.
#[test]
fn methods_are_compiled_once_across_tables() {
    let src = std::fs::read_to_string(root().join("tests/programs/components.sijo")).unwrap();
    let mut d = Diags::default();
    let c = sijoc::driver::compile(&src, &mut d).expect("compiles");
    let code = sijoc::codegen::generate(&c, "components.sijo");
    // no method instance names a combination table any more
    let per_table = code.lines().filter(|l| l.starts_with("static ") && l.contains("__a_") && l.contains("_x2b_") && l.contains(" M")).count();
    assert_eq!(per_table, 0, "per-table method instances left");
    assert!(code.contains("SijoView_0") && code.contains("static inline SijoView_0 sijo_view0_a_Ship_x2b_Burning(SijoWorld* w)"));
    let shared = sijoc::codegen::shared_code(&c);
    assert!(shared.iter().any(|g| g == "the methods of Ship on its 8 tables"), "{:?}", shared);
    assert!(shared.iter().any(|g| g == "the methods of component Stunned on its 8 tables"), "{:?}", shared);
}
