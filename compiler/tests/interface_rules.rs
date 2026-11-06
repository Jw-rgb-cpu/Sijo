//! Interfaces beyond signatures: default implementations (C# 8 style) and generic interfaces.
//! Accepted and rejected programs with their exact error codes, and a run-time program that mixes
//! defaults, overrides, generic interfaces and Query over an interface (1 and 4 threads,
//! sanitizers).

use sijoc::diag::{Diags, Level};
use sijoc::driver::compile;
use std::path::{Path, PathBuf};
use std::process::Command;

const GAME: &str = "public static class Game { public static void Start() { } }";

fn errors(src: &str) -> Vec<(&'static str, String)> {
    let mut d = Diags::default();
    let _ = compile(&format!("{}\n{}", src, GAME), &mut d);
    d.list.iter().filter(|x| x.level == Level::Error).map(|x| (x.code, format!("{} {}", x.msg, x.notes.join(" ")))).collect()
}

#[track_caller]
fn ok(src: &str) {
    let got = errors(src);
    assert!(got.is_empty(), "expected no errors but got {:?} for:\n{}", got, src);
}

/// exactly one error, with this code
#[track_caller]
fn bad(src: &str, code: &str) {
    let got = errors(src);
    assert!(got.len() == 1 && got[0].0 == code, "expected one {} but got {:?} for:\n{}", code, got, src);
}

/// exactly one error, with this code and a message or note that says `what`
#[track_caller]
fn bad_says(src: &str, code: &str, what: &str) {
    let got = errors(src);
    assert!(got.len() == 1 && got[0].0 == code && got[0].1.contains(what), "expected one {} saying {:?} but got {:?} for:\n{}", code, what, got, src);
}

const HIT: &str = "public interface IHit { void Hit(int n); int Hp(); bool Down() => Hp() <= 0; void Finish() { Hit(Hp()); } } ";

#[test]
fn default_implementations_are_accepted() {
    // a class takes the defaults it does not declare; its own declaration wins
    ok(&format!("{}public class A : Entity, IHit {{ public mut int H = 3; public void Hit(int n) {{ H -= n; }} public int Hp() => H; }} public class B : Entity, IHit {{ public mut int H; public void Hit(int n) {{ H -= n; }} public int Hp() => H; public bool Down() => H < 5; }}", HIT));
    // calls through a handle, through Query and on `this` reach the default
    ok(&format!("{}public class A : Entity, IHit {{ public mut int H = 3; public void Hit(int n) {{ H -= n; }} public int Hp() => H; public void Update(float dt) {{ if (Down()) {{ Finish(); }} }} }} public static class G {{ public static void F(IHit? x) {{ if (x is IHit h && !h.Down()) {{ h.Finish(); }} foreach (var y in Query<IHit>()) {{ y.Finish(); }} }} }}", HIT));
    // an abstract class may leave a method without a default to its subclasses
    ok(&format!("{}public abstract class U : Entity, IHit {{ public mut int H = 3; public int Hp() => H; }} public class B : U {{ public void Hit(int n) {{ H -= n; }} }}", HIT));
    // a struct takes the defaults too (a copy checked against the struct), used through 'where T : I'
    ok(&format!("{}public struct S : IHit {{ public int H; public void Hit(int n) {{ }} public int Hp() => H; }} public static class G {{ public static bool F<T>(T x) where T : IHit => x.Down(); public static bool H() => F(new S {{ H = 2 }}) || new S().Down(); }}", HIT));
    // a default property, and a class implementing it with its own property
    ok("public interface IScore { int Base(); int Score => Base() * 10; } public class A : Entity, IScore { public int Base() => 2; } public class B : Entity, IScore { public int Base() => 1; public int Score => 7; } public static class G { public static int F(IScore? s) => s is IScore x ? x.Score : 0; }");
    // an interface that extends another may give its method a default (the most specific one wins)
    ok("public interface IA { int V(); } public interface IB : IA { int V() => 2; } public class C : Entity, IB { } public static class G { public static int F(IA? a) => a is IA x ? x.V() : 0; }");
    // and take a default away again: the class must declare it then
    bad_says("public interface IA { int V() => 1; } public interface IB : IA { int V(); } public class C : Entity, IB { }", "E0261", "'C' must implement 'IB.V'");
    // a default 'Update' is the entity's system, a default client method its client system
    ok("public interface ITick { void Step(); void Update(float dt) { Step(); } client void Show() { Draw.Text(0, 0, \"x\"); } } public class A : Entity, ITick { public mut int N; public void Step() { N += 1; } }");
    // 'Count()' of an interface is its method, not Count<T>()
    ok("public interface IBag { int Count(); bool Empty() => Count() == 0; } public class A : Entity, IBag { public int Count() => 0; }");
}

#[test]
fn default_implementations_see_only_the_interface() {
    // the implementer's fields are not visible: 'this' is the interface
    bad_says("public interface IHit { void Hit(int n); void Twice() { Hit(Hp); } } public class A : Entity, IHit { public mut int Hp; public void Hit(int n) { Hp -= n; } }", "E0101", "declare what it needs in 'IHit'");
    bad_says("public interface IHit { void Hit(int n); void Twice() { Hit(this.Hp); } } public class A : Entity, IHit { public mut int Hp; public void Hit(int n) { Hp -= n; } }", "E0108", "an interface has no fields");
    // nor its other methods
    bad_says("public interface IHit { void Hit(int n); void Twice() { Heal(1); } } public class A : Entity, IHit { public void Hit(int n) { } public void Heal(int n) { } }", "E0101", "unknown function 'Heal'");
    // a default is checked like any method body
    bad("public interface IHit { int Hp(); int Half() { return Hp() / 2 + true; } }", "E0239");
    bad("public interface IHit { int Hp(); int Half() { if (Hp() > 0) { return 1; } } }", "E0207");
}

#[test]
fn default_implementation_rules() {
    // two unrelated defaults of one method: the class decides
    bad_says("public interface IA { int V() => 1; } public interface IB { int V() => 2; } public class C : Entity, IA, IB { }", "E0261", "neither interface extends the other");
    ok("public interface IA { int V() => 1; } public interface IB { int V() => 2; } public class C : Entity, IA, IB { public int V() => 3; }");
    // a default does not implement an unrelated interface's method of the same name
    bad_says("public interface IA { int V(); } public interface IB { int V() => 2; } public class C : Entity, IA, IB { }", "E0261", "does not implement 'IA.V'");
    bad_says("public interface IA { int V() => 1; } public interface IB { int V() => 2; } public struct S : IA, IB { }", "E0261", "neither interface extends the other");
    // a redeclaration keeps the signature
    bad_says("public interface IA { int V(); } public interface IB : IA { float V() => 2; }", "E0261", "which it declares again");
    // an implementation is a property exactly when the interface's member is
    bad("public interface IScore { int Score => 1; } public class A : Entity, IScore { public int Score() => 2; }", "E0261");
    bad("public interface IScore { int Score(); } public class A : Entity, IScore { public int Score => 2; }", "E0261");
    // a field is not an implementation, and its name cannot be a method of the interface too
    bad_says("public interface IHit { int Hp(); bool Down() => Hp() <= 0; } public class A : Entity, IHit { public int Hp() => 1; public mut int Down; }", "E0104", "a field of 'A' and a method of its interface 'IHit'");
    bad("public interface IHit { int Hp => 1; } public struct S : IHit { public int Hp; }", "E0104");
    // a struct cannot use (or implement) a client method
    bad_says("public interface IDraw { client void Show() { } } public struct S : IDraw { }", "E0261", "a client method");
    // a default that calls a struct's 'mut' method changes 'this': it is declared 'mut' (a struct's
    // copy of it is a 'mut' method; entity classes change their own fields anyway)
    let grow = "public interface IGrow { void Grow(); mut void GrowTwice() { Grow(); Grow(); } } public struct SPlant : IGrow { public int N; public mut void Grow() { N += 1; } } ";
    ok(&format!("{}public class Tree : Entity, IGrow {{ public mut int N; public void Grow() {{ N += 1; }} }} public static class G {{ public static int F<T>(mut T x) where T : IGrow {{ x.GrowTwice(); return 0; }} public static int H() {{ mut SPlant p = new SPlant(); p.GrowTwice(); return p.N + F(p); }} }}", grow));
    bad_says(&format!("{}public static class G {{ public static int H() {{ SPlant p = new SPlant(); p.GrowTwice(); return p.N; }} }}", grow), "E0403", "");
    bad_says("public interface IGrow { void Grow(); void GrowTwice() { Grow(); } } public struct SPlant : IGrow { public int N; public mut void Grow() { N += 1; } }", "E0403", "in the default implementation 'IGrow.GrowTwice' that struct 'SPlant' uses");
    // 'mut' is for a default implementation: a signature leaves it to each implementation
    bad_says("public interface IBad { mut void M(); }", "E0028", "a default implementation");
    // still not in an interface: fields, 'static virtual', constructors, operators, generic and async
    // methods, '{ get; }' accessors
    bad("public interface IBad { int X; }", "E0262");
    bad("public interface IBad { static virtual void M() { } }", "E0262");
    // static members: a static method with a body is the interface's own function; 'static abstract'
    // is provided by each implementing type and reached through a type parameter
    ok("public interface IK { static int K() => 1; } public static class G { public static int F() => IK.K(); }");
    ok("public interface IZ<T> { static abstract T Zero(); } public struct SA : IZ<SA> { public int V; public static SA Zero() => new SA { V = 1 }; } public class E : Entity, IZ<int> { public static int Zero() => 0; } public static class G { public static T Z<T>() where T : IZ<T> => T.Zero(); public static int F() => Z<SA>().V; }");
    bad_says("public interface IZ { static abstract int Zero(); } public struct SA : IZ { public int V; }", "E0261", "must implement the static 'IZ.Zero()'");
    bad_says("public interface IZ { static abstract int Zero(); } public struct SA : IZ { public int Zero() => 1; }", "E0261", "must be static");
    bad_says("public interface IZ { static abstract int Zero(); } public class C : Entity, IZ { public static float Zero() => 1; }", "E0261", "does not match the static");
    bad_says("public interface IZ { static abstract int Zero(); } public static class G { public static int F() => IZ.Zero(); }", "E0262", "static abstract");
    // a static method of an interface is not inherited by the types that implement it
    bad("public interface IK { static int K() => 1; } public struct SA : IK { } public static class G { public static int F() => SA.K(); }", "E0101");
    bad("public interface IBad { void M<T>(T x) { } }", "E0262");
    bad("public interface IBad { async void M() { } }", "E0262");
    bad("public interface IBad { IBad() { } }", "E0262");
    bad("public struct S { } public interface IBad { S operator +(S a, S b) => a; }", "E0262");
    bad_says("public interface IBad { int Hp { get; } }", "E0016", "'int Hp();'");
    // a default is an implementation, not the declaration of a generic or abstract member
    bad("public interface IA { void M(); } public class C : Entity, IA { }", "E0261");
}

const BOX: &str = "public interface IBox<T> { void Put(T x); T Peek(); int Size(); bool Empty() => Size() == 0; } ";

#[test]
fn generic_interfaces_are_accepted() {
    // implemented by a class and by a struct, used as a handle, in Query, in 'is' and in 'where'
    ok(&format!(
        "{}public class Crate : Entity, IBox<int> {{ public mut int Top; public mut int N; public void Put(int x) {{ Top = x; N += 1; }} public int Peek() => Top; public int Size() => N; }} \
         public struct SCell : IBox<float> {{ public float V; public mut void Put(float x) {{ V = x; }} public float Peek() => V; public int Size() => 1; }} \
         public class Holder : Entity {{ public IBox<int>? Box; }} \
         public static class G {{ \
           public static int Sum() {{ mut int s = 0; foreach (var b in Query<IBox<int>>()) {{ s += b.Peek(); }} return s; }} \
           public static T First<T>(T x) where T : IBox<float> => x; \
           public static bool Has(IBox<int>? b) => b is IBox<int> x ? !x.Empty() : false; \
           public static float Cell() => First(new SCell {{ V = 2 }}).Peek(); }}",
        BOX
    ));
    // different type arguments are different interfaces; a type argument may be a struct or a list
    ok(&format!("{}public struct SP {{ public int X; }} public class A : Entity, IBox<SP> {{ public mut SP V; public void Put(SP x) {{ V = x; }} public SP Peek() => V; public int Size() => 1; }} public class B : Entity, IBox<string> {{ public mut string V = \"\"; public void Put(string x) {{ V = x; }} public string Peek() => V; public int Size() => V.Length; }}", BOX));
    // a generic interface may extend one (with its own type parameter) and be extended
    ok("public interface IGet<T> { T Get(); } public interface IPair<T> : IGet<T> { T Other(); T Sum() => Get(); } public interface INum : IPair<int> { } public class A : Entity, INum { public int Get() => 1; public int Other() => 2; } public static class G { public static int F(IGet<int>? g) => g is IGet<int> x ? x.Get() : 0; }");
    // a generic struct implements a generic interface with its own type parameter
    ok(&format!("{}public struct SStack<T> : IBox<T> {{ public T Last; public int N; public mut void Put(T x) {{ Last = x; N += 1; }} public T Peek() => Last; public int Size() => N; }} public static class G {{ public static int F<B>(mut B b) where B : IBox<int> {{ b.Put(4); return b.Peek() + (b.Empty() ? 1 : 0); }} public static int H() => F(new SStack<int>()); }}", BOX));
    // the type argument is inferred from a handle of the interface or of a class implementing it
    ok(&format!("{}public class Crate : Entity, IBox<int> {{ public void Put(int x) {{ }} public int Peek() => 1; public int Size() => 1; }} public static class G {{ public static T Top<T>(IBox<T> b) => b.Peek(); public static int F(Crate c, IBox<int> b) => Top(c) + Top(b); }}", BOX));
    // where constraints on the interface's type parameter
    ok("public interface INum<T> where T : struct { T Zero(); } public class A : Entity, INum<int> { public int Zero() => 0; }");
}

#[test]
fn generic_interface_rules() {
    // the implementation has the instance's signature
    bad_says(&format!("{}public class Crate : Entity, IBox<int> {{ public void Put(float x) {{ }} public int Peek() => 1; public int Size() => 1; }}", BOX), "E0261", "'Crate.Put' does not match 'IBox<int>.Put'");
    bad_says(&format!("{}public class Crate : Entity, IBox<int> {{ public void Put(int x) {{ }} public int Size() => 1; }}", BOX), "E0261", "'Crate' must implement 'IBox<int>.Peek'");
    // one class, two instances whose methods mention T: one method cannot implement both
    bad_says(&format!("{}public class Crate : Entity, IBox<int>, IBox<float> {{ public void Put(int x) {{ }} public int Peek() => 1; public int Size() => 1; }}", BOX), "E0261", "implements both 'IBox<int>' and 'IBox<float>'");
    // ... but methods whose parameters mention T are overloads: Put(int) and Put(float) implement both
    ok("public interface IPut<T> { void Put(T x); int Size(); bool Has(T x) => Size() > 0; } public class Crate : Entity, IPut<int>, IPut<float> { public void Put(int x) { } public void Put(float x) { } public int Size() => 1; } public static class G { public static bool F(Crate c) => c.Has(1) && c.Has(2.5f); }");
    bad_says("public interface IPut<T> { void Put(T x); } public class Crate : Entity, IPut<int>, IPut<float> { public void Put(int x) { } }", "E0261", "'Crate' must implement 'IPut<float>.Put(float)'");
    // two instances whose methods do not mention the type parameter are fine
    ok("public interface ITag<T> { int Id(); } public struct SA { } public struct SB { } public class C : Entity, ITag<SA>, ITag<SB> { public int Id() => 1; }");
    // type arguments: their number, and a generic interface named without them
    bad(&format!("{}public class Crate : Entity, IBox<int, int> {{ }}", BOX), "E0228");
    bad(&format!("{}public class Crate : Entity, IBox {{ }}", BOX), "E0228");
    bad(&format!("{}public static class G {{ public static void F(IBox? b) {{ }} }}", BOX), "E0228");
    bad("public interface IA { } public class C : Entity, IA<int> { }", "E0228");
    // an instance is checked with its type arguments: the default fails for string
    bad_says("public interface ISum<T> { T One(); T Two() => One() * 2; } public class A : Entity, ISum<int> { public int One() => 1; } public class B : Entity, ISum<string> { public string One() => \"a\"; }", "E0239", "in 'ISum<string>.Two', an instance of this generic interface");
    // where constraints are checked per instance
    bad("public interface INum<T> where T : struct { T Zero(); } public class A : Entity, INum<string> { public string Zero() => \"\"; }", "E0260");
    // built-in .NET interfaces are unknown types
    bad_says("public struct S : IEquatable<S> { }", "E0105", "no built-in .NET interfaces");
    bad("public class C : Entity, IComparable<C> { }", "E0105");
    // type parameters: distinct, not type names, invariant
    bad("public interface IA<T, T> { }", "E0103");
    bad("public interface IA<int> { }", "E0258");
    bad("public interface IA<out T> { T Get(); }", "E0015");
    // runaway instantiation and cycles
    bad("public interface IA<T> : IA<List<T>> { } public class C : Entity, IA<int> { }", "E0259");
    bad("public interface IA<T> : IA<T> { } public class C : Entity, IA<int> { }", "E0214");
    // the first name after ':' of a class is its base class, never an interface
    bad_says(&format!("{}public class Crate : IBox<int> {{ }}", BOX), "E0209", "write ': Entity, IBox<int>'");
    // a generic interface is not spawned, and a struct value is not one
    bad(&format!("{}public static class G {{ public static void F() {{ Spawn(new IBox<int>()); }} }}", BOX), "E0246");
    // structs, entity classes, static classes, interfaces and methods are generic; enums are not
    bad("public enum E<T> { A }", "E0258");
    // a generic entity class implements a generic interface with its own type parameter
    ok(&format!(
        "{}public class Pool<T> : Entity, IBox<T> where T : struct {{ public mut List<T> Items; public void Put(T x) {{ Items.Add(x); }} public T Peek() => Items[0]; public int Size() => Items.Count; }} \
         public static class Pools {{ public static void Make() {{ Spawn(new Pool<int> {{ }}); Spawn(new Pool<float> {{ }}); }} \
         public static void Fill() {{ foreach (var b in Query<IBox<int>>()) {{ b.Put(1); Print($\"{{b.Empty()}}\"); }} }} }}",
        BOX
    ));
}

// ---------------------------------------------------------------------------
// run time
// ---------------------------------------------------------------------------

fn cc() -> Option<String> {
    for c in [std::env::var("CC").unwrap_or_default(), "gcc".into(), "clang".into(), "cc".into()] {
        if !c.is_empty() && Command::new(&c).arg("--version").output().map(|o| o.status.success()).unwrap_or(false) {
            return Some(c);
        }
    }
    None
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn build(src_path: &Path, tag: &str, extra: &[&str]) -> Option<PathBuf> {
    let cc = cc()?;
    let src = std::fs::read_to_string(src_path).unwrap();
    let mut d = Diags::default();
    let c = compile(&src, &mut d).unwrap_or_else(|| panic!("{} failed to compile: {:?}", src_path.display(), d.list));
    let csrc = sijoc::codegen::generate(&c, &src_path.display().to_string());
    let dir = std::env::temp_dir().join(format!("sijoc-ifaces-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).unwrap();
    let (cfile, exe) = (dir.join("prog.c"), dir.join("prog"));
    std::fs::write(&cfile, csrc).unwrap();
    let mut args: Vec<String> = vec!["-std=gnu11".into(), "-O2".into(), "-ffp-contract=off".into(), "-w".into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    args.extend([cfile.display().to_string(), "-o".into(), exe.display().to_string(), "-lm".into()]);
    let out = Command::new(&cc).args(&args).output().unwrap();
    if !out.status.success() {
        if extra.iter().any(|f| f.starts_with("-fsanitize") || *f == "-fopenmp") {
            return None; // optional toolchain feature missing
        }
        panic!("C compilation failed for {}:\n{}", src_path.display(), String::from_utf8_lossy(&out.stderr));
    }
    Some(exe)
}

fn run(exe: &Path, args: &[&str], threads: usize) -> (i32, String) {
    let out = Command::new(exe).args(args).env("OMP_NUM_THREADS", threads.to_string()).output().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// Default implementations (on `this`, through handles, Query, deferred calls, a default
/// `Update` system), overrides, generic interfaces implemented by classes and structs, and Query
/// over a generic interface: expected output, the same world on 1 and 4 threads, sanitizers.
#[test]
fn interface_defaults_and_generic_interfaces_run() {
    let path = root().join("tests/programs/interface_defaults.sijo");
    let expected = std::fs::read_to_string(root().join("tests/programs/interface_defaults.expected")).unwrap();
    let Some(exe) = build(&path, "omp", &["-fopenmp"]).or_else(|| build(&path, "plain", &[])) else {
        eprintln!("skipping: no C compiler");
        return;
    };
    let mut sums = vec![];
    for t in [1, 4] {
        let (code, out) = run(&exe, &["--ticks", "8"], t);
        assert_eq!(code, 0, "{}", out);
        let printed: Vec<&str> = out.lines().take_while(|l| !l.starts_with("== sijo")).collect();
        assert_eq!(printed.join("\n").trim(), expected.trim(), "threads={}", t);
        sums.push(out.lines().find(|l| l.starts_with("world checksum:")).unwrap_or_default().to_string());
    }
    assert_eq!(sums[0], sums[1], "the world differs between 1 and 4 threads");
    if let Some(asan) = build(&path, "asan", &["-fsanitize=address,undefined", "-fno-sanitize-recover=all", "-g"]) {
        let (code, out) = run(&asan, &["--ticks", "8", "--quiet"], 1);
        assert_eq!(code, 0, "interface_defaults.sijo failed under sanitizers:\n{}", out);
    }
}
