//! Parser rules: contextual modifiers, reserved names, modifier and attribute checks, extern C
//! names, and error recovery (one mistake gives one error, not a flood).

use sijoc::diag::{Diags, Level};
use sijoc::driver::compile;

const GAME: &str = "public static class Game { public static void Start() { } }";

fn errors(src: &str) -> Vec<&'static str> {
    let mut d = Diags::default();
    let _ = compile(src, &mut d);
    d.list.iter().filter(|x| x.level == Level::Error).map(|x| x.code).collect()
}

fn with_game(body: &str) -> String {
    format!("{}\n{}", body, GAME)
}

#[track_caller]
fn expect_error(body: &str, code: &str) {
    let src = with_game(body);
    let got = errors(&src);
    assert!(got.contains(&code), "expected {} but got {:?} for:\n{}", code, got, body);
}

#[track_caller]
fn expect_ok(body: &str) {
    let src = with_game(body);
    let got = errors(&src);
    assert!(got.is_empty(), "expected no errors but got {:?} for:\n{}", got, body);
}

/// exactly these errors, in this order: one mistake, one error
#[track_caller]
fn expect_errors(body: &str, codes: &[&str]) {
    let src = with_game(body);
    let got = errors(&src);
    assert_eq!(got, codes, "for:\n{}", body);
}

#[track_caller]
fn expect_only(body: &str, code: &str) {
    expect_errors(body, &[code]);
}

fn in_method(stmts: &str) -> String {
    format!("public static class U {{ public static int F(List<int> ys) {{ {} return 0; }} }}", stmts)
}

// ---------- 1. `client`, `own`, `mut` are contextual: types may be called that ----------

#[test]
fn contextual_words_can_name_types_used_everywhere() {
    expect_ok(
        "public struct client { public int X; }
         public struct own { public int X; }
         public struct mut { public int X; }
         public class Ship : Entity
         {
             public client C;
             public mut own O;
             public mut mut M;
             public void Update(float dt) { O.X = C.X + 1; M.X = 2; }
             public client void Render() { }
         }
         public static class U
         {
             public static own Make(own a, mut own b, mut m) => a;
             public static client Pick(client c) => c;
             public static mut Same(mut m) => m;
             public static int H()
             {
                 own o = new own();
                 mut own p = new own();
                 p.X = 3;
                 mut mut q = new mut();
                 q.X = 1;
                 mut r = new mut();
                 return o.X + p.X + q.X + r.X;
             }
         }",
    );
}

#[test]
fn contextual_words_are_still_modifiers_before_a_type_and_name() {
    expect_ok(
        "public class Ship : Entity
         {
             public mut int in 0..5 Hp;
             public mut List<int> Trail;
             public void Update(float dt) { Hp += 1; Trail.Add(1); }
             public client void Render() { }
         }
         public static class U
         {
             public static own List<int> Make() => new List<int>();
             public static int Take(mut own List<int> xs) { xs.Add(1); return xs.Count; }
             public static int Dec(mut int n) { n -= 1; return n; }
         }",
    );
}

#[test]
fn a_contextual_word_without_a_type_says_so() {
    expect_only(&in_method("mut x = 3;"), "E0018");
    expect_only("public static class U { public static int F(own xs) => 1; }", "E0018");
    // declared after its first use: still a type
    expect_ok("public static class U { public static int F() { mut m = new mut(); return m.X; } } public struct mut { public int X; }");
}

// ---------- 2. keywords and built-in type names are not names ----------

#[test]
fn keywords_cannot_name_declarations() {
    expect_only("public class A : Entity { public int in; }", "E0011");
    expect_only("public class A : Entity { public int foreach = 3; }", "E0011");
    expect_only("public class A : Entity { public int return; }", "E0011");
    expect_only("public static class H { public static extern void int(); }", "E0011");
    expect_only("public static class U { public static int F(int static) => 1; }", "E0011");
    expect_only(&in_method("int in = 3;"), "E0011");
    expect_only(&in_method("foreach (var if in ys) { }"), "E0011");
    expect_only("public enum EK { A, while }", "E0011");
    expect_only("public struct float { public int X; }", "E0011");
}

#[test]
fn a_misspelled_range_reads_as_a_range() {
    // `MAXV` is a constant bound: one error, about the bound (it is not visible from Ship)
    let got = errors(&with_game("public class Ship : Entity { public mut int in MAXV..10 A; }"));
    assert_eq!(got, ["E0224"]);
    expect_ok("public static class R { public const int MAXV = 7; } public class Ship : Entity { public mut int in R.MAXV..10 A; public void Update(float dt) { A += 1; } }");
}

// ---------- 3. error recovery ----------

#[test]
fn one_mistake_in_a_member_gives_one_error() {
    expect_only("public class A : Entity { public mut in 0..5 X; }", "E0018");
    expect_only("public class A : Entity { public float W = 1 2 3; }", "E0010");
    expect_only("public class A : Entity { public mut int in (0..5) A; }", "E0224");
    expect_only("public class A : Entity { public mut float in 0..1..2 X; }", "E0224");
    expect_only("public class A : Entity { public mut int in 0..1 in 0..2 X; }", "E0224");
    expect_only("public class A : Entity { public void F(int x y) { } public void G() { } }", "E0010");
    expect_only("public class A : Entity { public void F() x { } public void G() { } }", "E0010");
}

#[test]
fn one_mistake_in_a_statement_gives_one_error() {
    expect_only(&in_method("int x = 1 2 3 4 5 6;"), "E0010");
    expect_only(&in_method("float in 0..1..2 y = 0;"), "E0224");
    expect_only(&in_method("int in 0..1 in 0..2 z = 0;"), "E0224");
    expect_only(&in_method("mut int in (0..5) w = 0;"), "E0224");
    expect_only(&in_method("if (ys.Count 1) { int a = 1; }"), "E0010");
    expect_only(&in_method("var f = new List<int> { 1 2 }; int k = 3;"), "E0010");
    expect_only(&in_method("int x = ;"), "E0024");
}

#[test]
fn recovery_keeps_reporting_independent_mistakes() {
    expect_errors("public class A : Entity { public float W = 1 2 3; public int V = 4 5; }", &["E0010", "E0010"]);
    expect_errors(
        "public static class U { public static int F() { return 1 2; } public static int G() { return 3 4; } }",
        &["E0010", "E0010"],
    );
    expect_errors(&in_method("var f = new List<int> { 1 2 }; int y = 3 4;"), &["E0010", "E0010"]);
    // a ';' missing at the end of a line: the next line is read as the next statement
    expect_errors(&in_method("int a = 1\n int b = 2 3;"), &["E0010", "E0010"]);
}

#[test]
fn past_the_nesting_limit_there_is_one_error() {
    let deep = in_method(&format!("{}{}", "{".repeat(1600), "}".repeat(1600)));
    expect_only(&deep, "E0025");
    let deep_if = in_method(&format!("{}{}", "if (ys.Count > 0) { ".repeat(1600), "}".repeat(1600)));
    expect_only(&deep_if, "E0025");
    let unclosed = in_method(&"{".repeat(1600));
    expect_only(&unclosed, "E0025");
}

// ---------- 4. parameter modifiers in either order ----------

#[test]
fn parameter_modifiers_in_either_order() {
    expect_ok(
        "public static class U
         {
             public static int A(own mut List<int> xs) { xs.Add(1); return xs.Count; }
             public static int B(mut own List<int> xs) { xs.Add(1); return xs.Count; }
         }",
    );
    expect_only("public static class U { public static void F(ref mut int x) { x = 1; } }", "E0028");
    expect_only("public static class U { public static void F(mut ref int x) { x = 1; } }", "E0028");
    expect_only("public static class U { public static void F(out mut int x) { x = 1; } }", "E0028");
    expect_only("public static class U { public static void F(own ref List<int> xs) { } }", "E0028");
    expect_only("public static class U { public static void F(ref ref int x) { x = 1; } }", "E0028");
    expect_only("public static class U { public static int F(mut mut int x) => x; }", "E0028");
}

// ---------- 5. duplicate and conflicting modifiers ----------

#[test]
fn duplicate_and_conflicting_modifiers() {
    expect_only("public class A : Entity { public public int X; }", "E0028");
    expect_only("public class A : Entity { public mut mut int X; }", "E0028");
    expect_only("public class A : Entity { public client client void R() { } }", "E0028");
    expect_only("public class A : Entity { public async async void R() { } }", "E0028");
    expect_only("public class A : Entity { public private int X; }", "E0028");
    expect_only("public abstract class A : Entity { public virtual override void R() { } }", "E0028");
    expect_only("public abstract class A : Entity { public abstract virtual void R(); }", "E0028");
    expect_only("public static class H { public static extern extern void F(); }", "E0028");
    expect_only("public static class H { public static extern(\"a\") extern(\"b\") void F(); }", "E0028");
    expect_only("public static class U { public static static int F() => 1; }", "E0028");
}

// ---------- 6. attributes on the right targets, with the right arguments ----------

const VEC: &str = "public struct SV2 { public float X; public float Y; } public struct SI2 { public int X; public int Y; }";

#[test]
fn spatial_marks_a_float_vector_field() {
    expect_ok(&format!("{VEC} public class A : Entity {{ [Spatial] public mut SV2 Pos; public void Update(float dt) {{ Pos.X += dt; }} }}"));
    expect_only(&format!("{VEC} public class A : Entity {{ [Spatial] public mut int P; }}"), "E0029");
    expect_only(&format!("{VEC} public class A : Entity {{ [Spatial] public mut SI2 P; }}"), "E0029");
}

#[test]
fn precision_applies_to_floats() {
    expect_ok(&format!(
        "{VEC} public class A : Entity {{ [Precision(0.5)] public mut float in 0..5 F; [Precision(0.1)] public mut SV2 in 0..64 Pos;
           public void Update(float dt) {{ F += dt; Pos.X += dt; }} }}"
    ));
    expect_only(&format!("{VEC} public class A : Entity {{ [Precision(0.5)] public mut string Name; }}"), "E0029");
    expect_only(&format!("{VEC} public class A : Entity {{ [Precision(0.5)] public mut int in 0..9 N; }}"), "E0029");
    expect_only(&format!("{VEC} public class A : Entity {{ [Precision(0.5)] public mut SI2 P; }}"), "E0029");
}

#[test]
fn attributes_do_not_apply_to_constants_struct_members_or_parameters() {
    expect_only("public static class U { [Spatial] public const int K = 3; }", "E0029");
    expect_only("public static class U { [Precision(3)] public const float K = 3; }", "E0029");
    expect_only("public struct SP { [Precision(0.1)] public float X; public float Y; }", "E0029");
    expect_only("public static class U { public static int F([Pure] int x) => x; }", "E0029");
}

#[test]
fn attribute_arguments_are_checked() {
    for a in ["[Pure(1)]", "[Recursive(2)]"] {
        expect_only(&format!("public static class U {{ {a} public static int F() => 1; }}"), "E0029");
    }
    for a in ["[Spatial(1)]", "[Replicated(true)]", "[NotReplicated(1)]", "[Precision]", "[Precision(0.1, 0.2)]", "[Precision(0)]", "[Precision(-1)]", "[Component]", "[Component(1)]", "[Component(\"A\", \"B\")]"] {
        expect_only(&format!("public class A : Entity {{ {a} public mut float F; }}"), "E0029");
    }
    expect_only("public class A : Entity { [Replicated, Replicated] public int R; }", "E0029");
    expect_only("public class A : Entity { [Replicated] [NotReplicated] public int R; }", "E0029");
    expect_ok("public class A : Entity { [Component(\"Body\")] [Replicated] public int R; [Precision(0.01)] public mut float in 0..1 F; public void Update(float dt) { F += dt; } }");
}

// ---------- 7. extern C names ----------

#[test]
fn extern_c_names_do_not_clash_with_c() {
    for f in [
        "extern(\"printf\") void P(string s)",
        "extern(\"main\") int M()",
        "extern(\"sijo_game_create\") int Q()",
        "extern(\"malloc\") int A(int n)",
        "extern(\"__hidden\") void H()",
        "extern void auto()",
        "extern void register()",
    ] {
        expect_only(&format!("public static class H {{ public static {f}; }}"), "E0254");
    }
    expect_ok("public static class H { public static extern(\"engine_key_down\") bool IsKeyDown(int k); public static extern void PlaySound(string name, float volume); }");
}

// ---------- 8. `...` is not a range ----------

#[test]
fn three_dots_are_an_error_not_a_fraction() {
    expect_only(&in_method("mut int in 0...5 b = 3; b += 1;"), "E0004");
    expect_ok(&in_method("mut int in 0..5 b = 3; b += 1; mut float in 0.5..1.5 c = 1; c += 0.25f;"));
}

// ---------- 9. long `else if` chains are not nesting ----------

#[test]
fn long_else_if_chains_compile() {
    let mut s = String::from("mut int x = ys.Count; if (x == -1) { x = 1; }");
    for i in 0..2000 {
        s.push_str(&format!(" else if (x == {i}) {{ x = {i} + 100; }}"));
    }
    s.push_str(" else { x = 0; } ys.Add(x);");
    let body = format!("public static class U {{ public static int F(mut own List<int> ys) {{ {} return ys.Count; }} }}", s);
    expect_ok(&body);
}

// ---------- foreach variables take no range ----------

#[test]
fn foreach_variables_take_no_range() {
    expect_only(&in_method("mut int s = 0; foreach (int in 5..6 y in ys) { s += y; }"), "E0224");
    expect_error(&in_method("foreach (float in 0..1 y in ys) { }"), "E0224");
}
