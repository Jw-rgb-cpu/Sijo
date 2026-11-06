//! Range types (`T in lo..hi`): every write saturates, wherever the value comes from. These are the
//! holes an adversarial tester found: ranged locals that live across an await, field defaults
//! without an initializer, compile-time evaluation, built-in `out` writes, extern functions,
//! quantization steps that do not divide the range, and state restored into a narrower range.
//! Requires a C compiler (gcc or clang); the run tests are skipped if none is found.

use sijoc::diag::{Diags, Level};
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

fn out_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sijoc-ranges-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const SANITIZE: &[&str] = &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"];

/// Compile Sijo source to C and then to an executable (extra C files: extern functions).
fn build(src: &str, tag: &str, extra_c: &[(&str, &str)], flags: &[&str]) -> Option<PathBuf> {
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
    for (name, text) in extra_c {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        args.push(p.display().to_string());
    }
    args.push("-o".into());
    args.push(exe.display().to_string());
    args.push("-lm".into());
    let out = Command::new(&cc).args(&args).output().unwrap();
    if !out.status.success() {
        if flags.iter().any(|f| f.starts_with("-fsanitize")) {
            return None; // sanitizers not installed
        }
        panic!("C compilation failed for {}:\n{}", tag, String::from_utf8_lossy(&out.stderr));
    }
    Some(exe)
}

fn run(exe: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(exe).args(args).env("OMP_NUM_THREADS", "1").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    (out.status.code().unwrap_or(-1), format!("{}{}", stdout, stderr))
}

fn line<'a>(out: &'a str, prefix: &str) -> &'a str {
    out.lines().find(|l| l.starts_with(prefix)).unwrap_or_else(|| panic!("no '{}' line in output:\n{}", prefix, out))
}

/// Build (plain and under the sanitizers), run, and return the output of the plain build; both
/// must exit cleanly with a passing replication check and print the same lines.
fn run_checked(src: &str, tag: &str, extra_c: &[(&str, &str)], ticks: &str) -> Option<String> {
    let Some(exe) = build(src, tag, extra_c, &[]) else {
        eprintln!("skipping: no C compiler");
        return None;
    };
    let (code, out) = run(&exe, &["--ticks", ticks]);
    assert_eq!(code, 0, "{} exited with {}:\n{}", tag, code, out);
    assert!(line(&out, "replication check:").contains("OK"), "{}", out);
    if let Some(asan) = build(src, &format!("{tag}-asan"), extra_c, SANITIZE) {
        let (code, aout) = run(&asan, &["--ticks", ticks]);
        assert_eq!(code, 0, "{} failed under sanitizers:\n{}", tag, aout);
        let prints = |o: &str| o.lines().filter(|l| !l.starts_with("frame arena") && !l.starts_with("threads")).map(String::from).take_while(|l| !l.starts_with("== sijo")).collect::<Vec<_>>();
        assert_eq!(prints(&out), prints(&aout), "{}: the sanitizer build prints something else", tag);
    } else {
        eprintln!("skipping the sanitizer build of {}: sanitizers not available", tag);
    }
    Some(out)
}

fn diags(src: &str) -> Diags {
    let mut d = Diags::default();
    let _ = sijoc::driver::compile(src, &mut d);
    d
}

fn errors(src: &str) -> Vec<&'static str> {
    diags(src).list.iter().filter(|x| x.level == Level::Error).map(|x| x.code).collect()
}

/// a ranged local that lives across an await moves into a hidden entity field: the field keeps the
/// range, so every later write saturates (assignment, compound, ++/--, ref, TryParse, NaN)
#[test]
fn ranged_locals_keep_their_range_across_await() {
    let src = r#"
public static class U
{
    public static void Bump(ref int v)
    {
        v += 50;
    }
}

public class A : Entity
{
    public async void Run()
    {
        await Inner(99);
    }

    async void Inner(int in 0..3 k)
    {
        mut int in 0..10 n = 4;
        mut float in -1..1 f = 0.5;
        await NextTick();
        n++;
        n += 100;
        int a = n;
        U.Bump(ref n);
        int b = n;
        n = -7;
        int c = n--;
        int d = n;
        bool ok = int.TryParse("77", out n);
        int e = n;
        f += 9;
        float g = f;
        f = f / 0.0f - f / 0.0f;
        Print($"k={k} a={a} b={b} c={c} d={d} e={e} ok={ok} g={g} f={f}");
        await NextTick();
        Print($"later n={n} f={f}");
    }
}

public class B : Entity
{
    public async void Run()
    {
        mut int in 0..3 n = 10;
        await NextTick();
        n += 100;
        Print($"n={n}");
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new A { });
        Spawn(new B { });
    }
}
"#;
    // the hidden field holding a ranged local is no field of the program: no W0202 for it
    assert!(!diags(src).list.iter().any(|x| x.code == "W0202"), "{:?}", diags(src).list);
    let Some(out) = run_checked(src, "await", &[], "5") else { return };
    assert!(out.contains("k=3 a=10 b=10 c=0 d=0 e=10 ok=True g=1 f=-1"), "{}", out);
    assert!(out.contains("later n=10 f=-1"), "{}", out);
    assert!(out.contains("n=3\n"), "{}", out);
}

/// a ranged field without an initializer starts at its zero default saturated into the range (and
/// W0202 says so); replication then sends a value inside the range
#[test]
fn ranged_fields_without_initializer_start_saturated() {
    let src = r#"
public struct SV
{
    public float X;
    public float Y;
}

public class A : Entity
{
    public int in 1..10 Lvl;
    public SV in 1..5 P;
    public mut float in -3..-1 F;
    public int in -5..5 Zero;
    public void Update(float dt)
    {
        Print($"Lvl={Lvl} P={P.X},{P.Y} F={F} Zero={Zero}");
    }
    public client void Render()
    {
        Draw.Sprite(Lvl + Zero, P.X + F, P.Y);
    }
}

public static class Game
{
    public static void Start() { Spawn(new A { }); }
}
"#;
    let d = diags(src);
    let w: Vec<&str> = d.list.iter().filter(|x| x.code == "W0202").map(|x| x.msg.as_str()).collect();
    assert_eq!(w.len(), 3, "{:?}", d.list);
    assert!(w.iter().any(|m| m.contains("'Lvl'") && m.contains("zero default 0") && m.contains("1..10")), "{:?}", w);
    assert!(w.iter().any(|m| m.contains("'P'")), "{:?}", w);
    assert!(w.iter().any(|m| m.contains("'F'")), "{:?}", w);
    assert!(d.list.iter().filter(|x| x.code == "W0202").all(|x| x.notes.iter().any(|n| n.contains("it starts at"))), "{:?}", d.list);
    let Some(out) = run_checked(src, "defaults", &[], "2") else { return };
    assert!(out.contains("Lvl=1 P=1,1 F=-1 Zero=0"), "{}", out);
}

/// compile-time evaluation stores the saturated value and gives it back as the value of `=`,
/// `op=` and prefix `++`/`--` (postfix: the value before the write), as the generated code does
#[test]
fn ctfe_returns_the_saturated_value() {
    let src = r#"
public static class R
{
    public const int K = R.Pre(0);
    public const int K1 = R.Post(10);
    public const float K2 = R.FPre(0.5);
    public const float K3 = R.FPost(0.5);
    public const int K4 = R.Chain(3);
    public static int Pre(int x)
    {
        mut int in 0..10 y = x;
        return --y;
    }
    public static int Asg(int x)
    {
        mut int in 0..10 y = 0;
        return (y = x);
    }
    public static int Cmp(int x)
    {
        mut int in 0..10 y = 5;
        return (y += x);
    }
    public static int Post(int x)
    {
        mut int in 0..10 y = x;
        int old = y++;
        return old * 100 + y;
    }
    public static float FPre(float x)
    {
        mut float in -1..1 y = x;
        return ++y;
    }
    public static float FPost(float x)
    {
        mut float in -1..1 y = x;
        float a = y--;
        float b = y--;
        float c = y--;
        return a + b * 10 + c * 100 + y * 1000;
    }
    public static int Chain(int x)
    {
        mut int in 0..5 y = 0;
        mut int z = 0;
        z = y = x * 10;
        return z * 100 + (y -= 50);
    }
}

public class E : Entity
{
    public int F1 = R.Post(10);
    public float F2 = R.FPre(0.5);
    public int F3 = R.Asg(50);
    public void Update(float dt) { }
}

public static class Game
{
    public static void Start() { Spawn(new E { }); }
    public static void Update(float dt)
    {
        int z = Time.Tick - 1;
        foreach (var e in Query<E>())
        {
            Print($"const {R.K} {R.K1} {R.K2} {R.K3} {R.K4} | fields {e.F1} {e.F2} {e.F3} | ctfe {R.Pre(0)} {R.Asg(50)} {R.Cmp(50)}");
            Print($"runtime {R.Pre(z)} {R.Post(10 + z)} {R.FPre(0.5 + z)} {R.FPost(0.5 + z)} {R.Chain(3 + z)} | {R.Asg(50 + z)} {R.Cmp(50 + z)}");
        }
    }
}
"#;
    let Some(out) = run_checked(src, "ctfe", &[], "1") else { return };
    assert!(out.contains("const 0 1010 1 -1104.5 500 | fields 1010 1 10 | ctfe 0 10 10"), "{}", out);
    assert!(out.contains("runtime 0 1010 1 -1104.5 500 | 10 10"), "{}", out);
}

/// int.TryParse / float.TryParse / Dictionary.TryGetValue write through `out`: a ranged local
/// saturates after the write (also the 0 / default written on failure)
#[test]
fn builtin_out_writes_saturate() {
    let src = r#"
public static class Game
{
    public static void Start() { }
    public static void Update(float dt)
    {
        mut int in 0..10 a = 3;
        bool ok1 = int.TryParse("99", out a);
        mut float in -1..1 b = 0;
        bool ok2 = float.TryParse("-7.5", out b);
        mut var d = new Dictionary<int, int>();
        d[1] = 500;
        mut int in 0..10 c = 0;
        bool ok3 = d.TryGetValue(1, out c);
        mut int in 1..10 e = 5;
        bool ok4 = int.TryParse("nope", out e);
        mut int in 2..10 g = 5;
        bool ok5 = d.TryGetValue(7, out g);
        mut float in 0.5..1 h = 1;
        bool ok6 = float.TryParse("NaN", out h);
        Print($"{a} {b} {c} {e} {g} {h} {ok1} {ok2} {ok3} {ok4} {ok5} {ok6}");
    }
}
"#;
    let Some(out) = run_checked(src, "outwrites", &[], "1") else { return };
    assert!(out.contains("10 -1 10 1 2 0.5 True True True False False True"), "{}", out);
}

/// a ref argument is saturated into the caller's range once, when the call returns: inside the
/// callee the parameter has no range (the callee cannot know it), for locals and fields alike
#[test]
fn ref_writes_saturate_when_the_call_returns() {
    let src = r#"
public static class U
{
    public static int Hit(ref int hp)
    {
        hp += 900;
        int seen = hp;
        hp -= 950;
        return seen;
    }
}

public class A : Entity
{
    public mut int in 0..100 Hp = 80;
    public void Update(float dt)
    {
        int seen = U.Hit(ref Hp);
        Print($"field={Hp} seenInside={seen}");
    }
    public client void Render()
    {
        Draw.Sprite(Hp, 0, 0);
    }
}

public static class Game
{
    public static void Start() { Spawn(new A { }); }
    public static void Update(float dt)
    {
        mut int in 0..100 viaRef = 80;
        int seen = U.Hit(ref viaRef);
        mut int in 0..100 direct = 80;
        direct += 900;
        direct -= 950;
        Print($"viaRef={viaRef} seenInside={seen} direct={direct}");
    }
}
"#;
    let Some(out) = run_checked(src, "refs", &[], "2") else { return };
    assert!(out.contains("viaRef=30 seenInside=980 direct=0"), "{}", out);
    // tick 1: 80 + 900 - 950 = 30; tick 2: 30 + 900 - 950 = -20 -> 0
    assert!(out.contains("field=30 seenInside=980"), "{}", out);
    assert!(out.contains("field=0 seenInside=930"), "{}", out);
}

/// when [Precision] does not divide the range, the top quantization step lands past `hi`: the
/// client saturates the dequantized value, so it never sees a value outside the range
#[test]
fn clients_never_see_values_outside_the_range() {
    let src = r#"
public class A : Entity
{
    [Precision(0.4)] public mut float in 0..1 F = 1;
    [Precision(0.4)] public mut float in -1..0 G = 0;
    [Precision(0.6)] public mut float in 0..1 H = 0.85;
    public void Update(float dt)
    {
        if (Time.Tick == 2)
        {
            H = 0.95;
        }
    }
    public client void Render()
    {
        Print($"client F={F} G={G} H={H}");
        Draw.Sprite(0, F + H, G);
    }
}

public static class Game
{
    public static void Start() { Spawn(new A { }); }
}
"#;
    let Some(out) = run_checked(src, "precision", &[], "3") else { return };
    for l in out.lines().filter(|l| l.contains("client F=")) {
        let vals: Vec<f64> = l.split_whitespace().filter_map(|w| w.split('=').nth(1)).map(|v| v.parse().unwrap()).collect();
        assert!((0.0..=1.0).contains(&vals[0]) && (-1.0..=0.0).contains(&vals[1]) && (0.0..=1.0).contains(&vals[2]), "{}", l);
    }
    assert!(out.contains("client F=1 G=0 H=0.6"), "{}", out);
    assert!(out.contains("client F=1 G=0 H=1\n"), "{}", out);
}

/// the host does not know about range types: the call site saturates arguments for ranged extern
/// parameters and the result of a ranged extern function (NaN -> lo)
#[test]
fn extern_ranges_are_enforced_at_the_call_site() {
    let src = r#"
public static class Host
{
    public static extern("host_val") int in 0..10 Val(int x);
    public static extern("host_valf") float in -1..1 ValF(float x);
    public static extern("host_take") int Take(int in 0..5 x);
    public static extern("host_takef") float TakeF(float in 2..3 x, int in -4..-2 y);
}

public static class Game
{
    public static void Start() { }
    public static void Update(float dt)
    {
        Print($"{Host.Val(500)} {Host.Val(-3)} {Host.Val(7)} {Host.ValF(0.0)} {Host.ValF(0.5)} {Host.Take(99)} {Host.Take(-1)} {Host.TakeF(9, 0)}");
    }
}
"#;
    let ext = "#include <stdint.h>\nint32_t host_val(int32_t x) { return x; }\nfloat host_valf(float x) { return x == 0 ? x / x : x * 4; }\nint32_t host_take(int32_t x) { return x; }\nfloat host_takef(float x, int32_t y) { return x * 10 + (float)y; }\n";
    let Some(out) = run_checked(src, "externs", &[("ext.c", ext)], "1") else { return };
    assert!(out.contains("10 0 7 -1 1 5 0 28"), "{}", out);
}

/// a foreach variable reads the elements as they are: a range on it is an error, not ignored
#[test]
fn foreach_variable_takes_no_range() {
    let src = r#"
public static class Game
{
    public static void Start() { }
    public static void Update(float dt)
    {
        mut var ys = new List<int>();
        ys.Add(1);
        foreach (int in 5..6 y in ys)
        {
            Print($"y={y}");
        }
    }
}
"#;
    assert!(errors(src).contains(&"E0224"), "{:?}", diags(src).list);
    assert!(errors(&src.replace("int in 5..6 y", "int y")).is_empty());
}

/// a range added or narrowed since a state file was saved: the restored values are saturated
/// (and the load says so), so the next replication check passes
#[test]
fn restored_state_is_saturated_into_new_ranges() {
    let old = r#"
public class A : Entity
{
    public mut int in 0..1000 Hp = 500;
    public mut float F = 7.5;
    public mut int Free = 42;
    public void Update(float dt)
    {
        Print($"tick {Time.Tick} Hp={Hp} F={F} Free={Free}");
    }
    public client void Render()
    {
        Draw.Sprite(Hp, F, Free);
    }
}

public static class Game
{
    public static void Start()
    {
        Spawn(new A { });
    }
}
"#;
    let new = old.replace("int in 0..1000 Hp = 500", "int in 0..100 Hp = 50").replace("float F = 7.5", "float in -1..1 F = 0.5");
    let (Some(a), Some(b)) = (build(old, "state-old", &[], &[]), build(&new, "state-new", &[], &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let st = out_dir("state").join("state.txt");
    let st = st.to_str().unwrap();
    let (code, out) = run(&a, &["--ticks", "2", "--quiet", "--save-state", st]);
    assert_eq!(code, 0, "{}", out);
    let (code, out) = run(&b, &["--ticks", "2", "--load-state", st]);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("tick 3 Hp=100 F=1 Free=42"), "{}", out);
    assert!(line(&out, "replication check:").contains("OK"), "{}", out);
    let restored = line(&out, "[live] state restored");
    assert!(restored.contains("saturated:A.Hp") && restored.contains("saturated:A.F") && !restored.contains("Free"), "{}", restored);
    // the same schema: nothing to saturate, nothing reported
    let (code, out) = run(&a, &["--ticks", "2", "--load-state", st]);
    assert_eq!(code, 0, "{}", out);
    assert!(line(&out, "[live] state restored").contains("(same schema)"), "{}", out);
    assert!(out.contains("tick 3 Hp=500 F=7.5 Free=42"), "{}", out);
}

/// explain: concurrent ADDs to a ranged field saturate per command, so their order matters (it is
/// fixed); hidden coroutine fields are not listed as range contracts
#[test]
fn explain_says_ranged_adds_saturate_per_command() {
    let src = r#"
public class Ship : Entity
{
    public mut int in 0..100 Hp = 100;
    public mut int Score;
    public void Update(float dt)
    {
        foreach (var o in Query<Ship>())
        {
            if (o != this)
            {
                o.Hp -= 1;
                o.Score += 1;
            }
        }
    }
    public async void Run()
    {
        mut int in 0..3 n = 1;
        await NextTick();
        n += 1;
    }
}

public static class Game
{
    public static void Start() { Spawn(new Ship { }); Spawn(new Ship { }); }
}
"#;
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).unwrap_or_else(|| panic!("{:?}", d.list));
    let report = sijoc::explain::report(&c, &sijoc::diag::SourceMap::single("ships.sijo", src));
    let hp = line(&report, "  Ship.Update: writes Ship.Hp");
    assert!(hp.contains("saturated into 0..100 after each command") && hp.contains("fixed order") && !hp.contains("commutative"), "{}", hp);
    assert!(line(&report, "  Ship.Update: writes Ship.Score").contains("commutative"), "{}", report);
    let contracts = line(&report, "  range contracts enforced");
    assert!(contracts.contains("Ship.Hp") && !contracts.contains("__"), "{}", contracts);
}

/// the new saturation code paths compile without a single warning (-Wall -Wextra -pedantic -Werror)
#[test]
fn range_code_paths_are_warning_free() {
    let src = r#"
public static class Host
{
    public static extern("host_val") int in 0..10 Val(int in -5..5 x);
}

public class A : Entity
{
    public int in 1..10 Lvl;
    public mut int in 0..100 Hp = 50;
    [Precision(0.4)] public mut float in 0..1 F = 1;
    public async void Run()
    {
        mut int in 0..10 n = 4;
        await NextTick();
        bool ok = int.TryParse("5", out n);
        n += Host.Val(n) + (ok ? 1 : 0);
    }
    public void Update(float dt)
    {
        mut int in 0..10 a = 3;
        mut var d = new Dictionary<int, int>();
        bool ok = d.TryGetValue(1, out a) || int.TryParse("3", out a);
        Hp -= a + (ok ? 1 : 0);
    }
    public client void Render()
    {
        Draw.Sprite(Lvl, F, Hp);
    }
}

public static class Game
{
    public static void Start() { Spawn(new A { }); }
}
"#;
    let have = |c: &str| Command::new(c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    let mut d = Diags::default();
    let c = sijoc::driver::compile(src, &mut d).unwrap_or_else(|| panic!("{:?}", d.list));
    let dir = out_dir("werror");
    let cfile = dir.join("w.c");
    std::fs::write(&cfile, sijoc::codegen::generate(&c, "w.sijo")).unwrap();
    for cc in ["gcc", "clang"] {
        if !have(cc) {
            continue;
        }
        let out = Command::new(cc).args(["-std=gnu11", "-O2", "-ffp-contract=off", "-Wall", "-Wextra", "-pedantic", "-Werror", "-c"]).arg(&cfile).arg("-o").arg(dir.join("w.o")).output().unwrap();
        assert!(out.status.success(), "{}: warnings in generated C:\n{}", cc, String::from_utf8_lossy(&out.stderr));
    }
}
