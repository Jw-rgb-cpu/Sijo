//! The compiler must reject unsafe or ill-formed programs with a specific error code.

use sijoc::diag::{Diags, Level};
use sijoc::driver::compile;

const GAME: &str = "public static class Game { public static void Start() { } }";

fn codes(src: &str) -> Vec<&'static str> {
    let mut d = Diags::default();
    let _ = compile(src, &mut d);
    d.list.iter().map(|x| x.code).collect()
}

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

#[test]
fn accepts_minimal_program() {
    expect_ok("public class A : Entity { public mut int X; public void Update(float dt) { X += 1; } }");
}

#[test]
fn null_handles_must_be_checked() {
    expect_error("public class A : Entity { public A? T; public mut int Hp; public void Update(float dt) { T.Hp -= 1; } }", "E0301");
    expect_ok("public class A : Entity { public A? T; public mut int Hp; public void Update(float dt) { if (T is A t) { t.Hp -= 1; } } }");
}

#[test]
fn is_binding_is_not_visible_in_else_branch() {
    expect_error(
        "public class A : Entity { public A? T; public mut int Hp; public void Update(float dt) { if (T is A t) { } else { t.Hp -= 1; } } }",
        "E0101",
    );
}

#[test]
fn entity_fields_must_hold_nullable_handles() {
    expect_error("public class A : Entity { public A T; }", "E0302");
}

#[test]
fn entities_are_not_created_with_new() {
    expect_error("public class A : Entity { public void Update(float dt) { var x = new A(); } }", "E0212");
}

#[test]
fn no_mutable_globals() {
    let src = "public static class Game { public static int Counter; public static void Start() { } }";
    assert!(errors(src).contains(&"E0206"));
}

#[test]
fn implicit_float_to_int_is_an_error() {
    expect_error("public class A : Entity { public void Update(float dt) { int x = dt; } }", "E0211");
}

#[test]
fn exclusive_ref_arguments_must_not_alias() {
    let body = "public static class U { public static void Swap(ref int a, ref int b) { int t = a; a = b; b = t; } }
                public class A : Entity { public mut int X; public mut int Y; public void Update(float dt) { U.Swap(ref X, ref X); } }";
    expect_error(body, "E0402");
    let ok = "public static class U { public static void Swap(ref int a, ref int b) { int t = a; a = b; b = t; } }
              public class A : Entity { public mut int X; public mut int Y; public void Update(float dt) { U.Swap(ref X, ref Y); } }";
    expect_ok(ok);
}

#[test]
fn use_after_move_is_an_error() {
    let body = "public class A : Entity { public mut List<int> L; public void Update(float dt) { mut var p = new List<int>(); L = p; p.Add(1); } }";
    expect_error(body, "E0501");
}

#[test]
fn cannot_move_out_of_a_field() {
    let body = "public class A : Entity { public List<int> L; public void Update(float dt) { var p = L; } }";
    expect_error(body, "E0502");
}

#[test]
fn cannot_modify_a_list_while_iterating_it() {
    let body = "public class A : Entity { public void Update(float dt) { mut var xs = new List<int>(); xs.Add(1); foreach (var x in xs) { xs.Add(x); } } }";
    expect_error(body, "E0406");
}

#[test]
fn client_code_cannot_modify_state() {
    let body = "public class A : Entity { public mut int Hp; public client void Render() { Hp = 3; } }";
    expect_error(body, "E0601");
}

#[test]
fn server_code_cannot_draw() {
    let body = "public class A : Entity { public float X; public void Update(float dt) { Draw.Sprite(1, X, X); } }";
    expect_error(body, "E0602");
}

#[test]
fn deferred_messages_cannot_return_values() {
    let body = "public class A : Entity { public mut int Hp;
                  public int Hit() { Hp -= 1; return Hp; }
                  public void Update(float dt) { foreach (var o in Query<A>()) { int x = o.Hit(); } } }";
    expect_error(body, "E0247");
}

#[test]
fn atomic_cannot_contain_print() {
    let body = "public class A : Entity { public mut int G; public void Update(float dt) { atomic { G -= 1; Print(\"x\"); } } }";
    expect_error(body, "E0701");
}

/// exactly these errors, each with a note saying how to fix it
#[track_caller]
fn expect_only(body: &str, want: &[&str]) {
    let src = with_game(body);
    let mut d = Diags::default();
    let _ = compile(&src, &mut d);
    let errs: Vec<_> = d.list.iter().filter(|x| x.level == Level::Error).collect();
    let got: Vec<&str> = errs.iter().map(|x| x.code).collect();
    assert_eq!(got, want, "for:\n{}", body);
    for e in errs {
        assert!(!e.notes.is_empty(), "{} has no note: {}", e.code, e.msg);
    }
}

#[test]
fn atomic_blocks_change_collections_nest_and_return() {
    // every list, array and dictionary change on locals, `ref` parameters and fields of this
    expect_ok(
        "public struct P { public int X; public mut void Move() { X += 1; } }
         public class A : Entity { public mut List<int> L; public mut int[] Arr = new int[2]; public mut Dictionary<int, int> D;
           public mut List<List<int>> G; public mut List<P> Ps;
           void Grow() { L.Add(1); G.Add(new List<int>()); }
           static void Push(ref List<int> xs) { xs.Add(1); }
           public void Update(float dt) {
             mut var xs = new List<int> { 1, 2 }; mut var d = new Dictionary<string, int>();
             atomic {
               xs.Add(3); xs.Insert(0, 4); xs.RemoveAt(0); xs.Remove(3); xs.Sort(); xs.Reverse(); xs[0] = 5; xs[1] += 2; xs[0]++;
               xs.RemoveAll(x => x > 9); xs.Sort((a, b) => a - b); Push(ref xs); xs = new List<int>(); xs.Clear();
               d[\"a\"] = 1; d.Add(\"b\", 2); d.Remove(\"a\"); d.Clear(); d = new Dictionary<string, int>();
               L.Add(1); L.Sort(); L.Clear(); L = new List<int> { 3 }; Arr[0] = 1; Array.Fill(Arr, 2); D[1] = 2; D.Remove(1);
               G[0].Add(1); G[0][0] = 2; G.RemoveAt(0); G.Add(new List<int> { 1 }); Ps[0].Move(); Push(ref L); Grow();
               if (L.Count > 3) { fail; }
             } } }",
    );
    // nested blocks, `return` (with and without a value), `break` and `continue` leaving a block
    expect_ok(
        "public class A : Entity { public mut int G; public mut List<int> L;
           int Take(int n) { atomic { G -= n; atomic { L.Add(n); if (G < 0) { fail; } return L.Count; } } return 0; }
           public void Update(float dt) {
             for (int i = 0; i < 4; i++) { atomic { G += i; if (i == 1) { continue; } if (i == 3) { break; } atomic { L.Sort(); if (G > 5) { fail; } } } }
             while (true) { atomic { atomic { if (G > 0) { break; } } G += 1; } }
             switch (G) { case 1: atomic { G = 2; break; } default: break; }
             int n = Take(2); if (n > 0) { atomic { return; } } } }",
    );
    // a coroutine: blocks between awaits, on a local kept across them
    expect_ok(
        "public class A : Entity { public mut int G; public async void Run() {
           mut var seen = new List<int>(); for (int k = 0; k < 3; k++) { atomic { seen.Add(k); seen.Sort(); G += 1; if (k == 1) { fail; } } await NextTick(); } } }",
    );
    // spawns, remote writes and messages in a block are deferred, so they roll back too
    expect_ok(
        "public class B : Entity { public mut int N; public mut List<int> Xs; public void Hit() { N += 1; Print(\"hit\"); } }
         public class A : Entity { public B? T; void Make() { Spawn(new B { Xs = new List<int> { 1 } }); }
           public void Update(float dt) { atomic { Make(); Spawn(new B { N = 1 }); if (T is B t) { t.N += 1; t.Hit(); Destroy(t); } fail; } } }",
    );
}

#[test]
fn atomic_blocks_reject_irreversible_and_cross_tick_effects() {
    // output cannot be taken back: one error, from the innermost block around it
    expect_only("public class A : Entity { public mut int G; public void Update(float dt) { atomic { G -= 1; Print(\"x\"); } } }", &["E0701"]);
    expect_only("public class A : Entity { public mut int G; public void Update(float dt) { atomic { atomic { atomic { G -= 1; Print(\"x\"); } } } } }", &["E0701"]);
    expect_only(
        "public class A : Entity { public mut int G; void Say() { Print($\"{G}\"); } public void Update(float dt) { atomic { G += 1; atomic { Say(); } } } }",
        &["E0701"],
    );
    expect_only("public static class H { public static extern int Now(); } public class A : Entity { public mut int G; public void Update(float dt) { atomic { G = H.Now(); } } }", &["E0701"]);
    expect_only("public class A : Entity { public int X; public client void Render() { atomic { Draw.Sprite(1, X, 0); } } }", &["E0701"]);
    // a transaction cannot span ticks
    expect_only("public class A : Entity { public mut int N; public async void Run() { atomic { N += 1; await NextTick(); } } }", &["E0803"]);
    expect_only("public class A : Entity { public mut int N; public async void Run() { atomic { atomic { await Wait(1); } } } }", &["E0803"]);
    // fail needs a block; struct methods have no undo log (once, for the outermost block)
    expect_only("public class A : Entity { public void Update(float dt) { if (true) { fail; } } }", &["E0703"]);
    expect_only("public struct S { public int X; public mut void M() { atomic { X = 1; atomic { X = 2; } } } }", &["E0705"]);
    // break and continue still need a loop (a block is not one)
    expect_only("public class A : Entity { public mut int G; public void Update(float dt) { atomic { G = 1; break; } } }", &["E0233"]);
    // a list changed in a block while it is iterated is still an error
    expect_error("public class A : Entity { public mut List<int> L; public void Update(float dt) { foreach (var x in L) { atomic { L.Sort(); fail; } } } }", "E0406");
}

#[test]
fn fail_outside_atomic() {
    let body = "public class A : Entity { public void Update(float dt) { fail; } }";
    expect_error(body, "E0703");
}

#[test]
fn pure_contract_is_checked() {
    let body = "public static class U { [Pure] public static int F(int x) { Print(\"side effect\"); return x; } }";
    expect_error(body, "E0706");
}

#[test]
fn missing_return() {
    let body = "public static class U { public static int F(int x) { if (x > 0) { return 1; } } }";
    expect_error(body, "E0207");
}

#[test]
fn mutating_method_on_temporary() {
    let body = "public struct V { public float X; public mut void Bump() { X += 1; } public static V Make() => new V(); }
                public class A : Entity { public void Update(float dt) { V.Make().Bump(); } }";
    expect_error(body, "E0403");
}

#[test]
fn struct_params_are_immutable_unless_mut() {
    // was E0405 (struct parameters were read-only borrows); a value parameter is now immutable
    // (E0310) unless it is declared 'mut', which makes it a copy the method may change
    let body = "public struct V { public float X; }
                public static class U { public static void F(V v) { v.X = 3; } }";
    expect_error(body, "E0310");
    expect_ok("public struct V { public float X; } public static class U { public static float F(mut V v) { v.X = 3; return v.X; } }");
}

#[test]
fn constants_must_be_compile_time_evaluable() {
    let body = "public static class K { public const float R = Random.Float(0, 1); }";
    expect_error(body, "E0221");
}

#[test]
fn ctfe_evaluates_pure_functions() {
    let src = with_game("public static class K { public static int Sq(int x) => x * x; public const int A = 3; public const int B = Sq(A) + 1; }");
    let mut d = Diags::default();
    let c = compile(&src, &mut d).expect("compiles");
    let b = c.prog.consts.iter().find(|k| k.name == "B").unwrap();
    assert_eq!(b.value, Some(sijoc::hir::ConstVal::Int(10)));
}

#[test]
fn warns_on_read_after_deferred_write() {
    let src = with_game(
        "public class A : Entity { public mut int Hp; public void Update(float dt) { foreach (var o in Query<A>()) { o.Hp -= 1; if (o.Hp < 0) { Destroy(); } } } }",
    );
    assert!(codes(&src).contains(&"W0201"));
}

#[test]
fn missing_game_entry_point() {
    let got = errors("public class A : Entity { }");
    assert!(got.contains(&"E0222"));
}

#[test]
fn parse_errors_are_reported() {
    let got = errors("public class A : Entity { public void Update(float dt) { int x = ; } }");
    assert!(got.iter().any(|c| c.starts_with("E00")), "{:?}", got);
}

#[test]
fn hazard_is_detected_and_double_buffered() {
    let src = with_game(
        "public class B : Entity { public mut float X; public void Update(float dt) { mut float s = 0; foreach (var o in Query<B>()) { s += o.X; } X = s * 0.5; } }",
    );
    let mut d = Diags::default();
    let c = compile(&src, &mut d).expect("compiles");
    let sys = c.ecs.systems.iter().find(|s| s.name == "B.Update").unwrap();
    assert_eq!(sys.hazards.len(), 1);
    assert!(sys.parallel);
}

#[test]
fn replication_is_inferred_from_client_reads() {
    let src = with_game(
        "public class S : Entity { public mut float X; public mut float Secret; public void Update(float dt) { X += Secret; Secret += 1; }
           public client void Render() { Draw.Sprite(1, X, X); } }",
    );
    let mut d = Diags::default();
    let c = compile(&src, &mut d).expect("compiles");
    let names: Vec<String> = c.ecs.replicated.iter().map(|&f| c.prog.fields[f].name.clone()).collect();
    assert_eq!(names, vec!["X".to_string()]);
}

#[test]
fn range_contract_sets_quantization_bits() {
    let src = with_game(
        "public class S : Entity { public int in 0..100 Hp; public client void Render() { Draw.Sprite(Hp, 0, 0); } }",
    );
    let mut d = Diags::default();
    let c = compile(&src, &mut d).expect("compiles");
    let f = c.net.fields.values().next().unwrap();
    assert_eq!(f.bits, 7);
}

#[test]
fn escape_analysis_places_lists() {
    let src = with_game(
        "public class S : Entity { public mut List<int> Keep;
           public void Update(float dt) { mut var tmp = new List<int>(); tmp.Add(1); mut var kept = new List<int>(); kept.Add(2); Keep = kept; } }",
    );
    let mut d = Diags::default();
    let c = compile(&src, &mut d).expect("compiles");
    let regions: Vec<sijoc::memory::Region> = c.mem.sites.iter().map(|s| s.region).collect();
    assert_eq!(regions, vec![sijoc::memory::Region::Frame, sijoc::memory::Region::Heap]);
}

#[test]
fn client_calls_to_mutating_methods_become_rpcs() {
    let src = with_game(
        "public class S : Entity { public mut int Hp; public void Heal() { Hp += 1; }
           public client void Input() { if (Hp < 5) { Heal(); } } }",
    );
    let mut d = Diags::default();
    let c = compile(&src, &mut d).expect("compiles");
    let sys = c.ecs.systems.iter().find(|s| s.name == "S.Input").unwrap();
    assert_eq!(sys.eff.rpcs.len(), 1);
    assert!(sys.eff.self_writes.is_empty(), "the RPC's writes happen on the server, not in client code");
}

#[test]
fn rpcs_cannot_return_values() {
    let body = "public class S : Entity { public mut int Hp; public int Heal() { Hp += 1; return Hp; }
                  public client void Input() { int x = Heal(); } }";
    expect_error(body, "E0608");
}

#[test]
fn client_cannot_call_mutating_static_functions() {
    let body = "public static class U { public static void Kill(S s) { Destroy(s); } }
                public class S : Entity { public client void Input() { U.Kill(this); } }";
    expect_error(body, "E0601");
}

#[test]
fn ctors_and_operators_cannot_take_ref() {
    expect_error("public struct V { public int A; public V(ref int a) { A = a; } }", "E0251");
}

#[test]
fn other_entities_fields_cannot_be_passed_by_ref() {
    let body = "public static class U { public static void Inc(ref int x) { x += 1; } }
                public class A : Entity { public mut int Hp; public void Update(float dt) { foreach (var o in Query<A>()) { U.Inc(ref o.Hp); } } }";
    expect_error(body, "E0407");
}

#[test]
fn other_entities_structs_cannot_be_mutated_in_place() {
    let body = "public struct V { public int A; public mut void Bump() { A += 1; } }
                public class B : Entity { public mut V P; public void Update(float dt) { foreach (var o in Query<B>()) { o.P.Bump(); } } }";
    expect_error(body, "E0403");
}

#[test]
fn abstract_methods_must_be_implemented() {
    let body = "public abstract class A : Entity { public abstract int M(); } public class C : A { }";
    expect_error(body, "E0250");
}

#[test]
fn definite_assignment() {
    expect_error("public static class U { public static void F(out int x) { if (x > 0) { x = 1; } } }", "E0306");
    expect_error("public static class U { public static void F(bool b, out int x) { if (b) { x = 1; } } }", "E0308");
    // a local assigned later must be 'mut' (an immutable one needs its value at the declaration: E0310)
    expect_error("public static class U { public static int F() { mut int x; return x; } }", "E0306");
    expect_ok("public static class U { public static void F(bool b, out int x) { if (b) { x = 1; } else { x = 2; } } }");
}

#[test]
fn list_iteration_cannot_be_modified_through_ref() {
    let body = "public static class U { public static void Push(ref List<int> l) { l.Add(1); } }
                public class A : Entity { public void Update(float dt) { mut var xs = new List<int>(); foreach (var x in xs) { U.Push(ref xs); } } }";
    expect_error(body, "E0406");
}

#[test]
fn int_ranges_are_validated() {
    expect_error("public class A : Entity { public int in 0..3000000000 X; }", "E0224");
    expect_error("public class A : Entity { public int in 0.2..0.7 X; }", "E0224");
}

#[test]
fn deep_nesting_is_an_error_not_a_crash() {
    let deep = format!("public static class U {{ public static int F() => {}1{}; }}", "(".repeat(3000), ")".repeat(3000));
    let got = errors(&with_game(&deep));
    assert!(got.contains(&"E0025"), "{:?}", got);
}

const ENUM3: &str = "public enum ETeam { Red, Blue, Green }";

#[test]
fn switch_expressions_must_be_exhaustive() {
    expect_error(&format!("{ENUM3} public static class U {{ public static int F(ETeam t) => t switch {{ ETeam.Red => 1, ETeam.Blue => 2 }}; }}"), "E0237");
    expect_ok(&format!("{ENUM3} public static class U {{ public static int F(ETeam t) => t switch {{ ETeam.Red => 1, ETeam.Blue => 2, ETeam.Green => 3 }}; }}"));
    // interval analysis over ints: < 0, 0, > 0 covers every value
    expect_ok("public static class U { public static int F(int x) => x switch { < 0 => -1, 0 => 0, > 0 => 1 }; }");
    expect_error("public static class U { public static int F(int x) => x switch { < 0 => -1, > 0 => 1 }; }", "E0237");
    expect_error("public static class U { public static int F(bool b) => b switch { true => 1 }; }", "E0237");
    expect_error("public static class U { public static int F(float x) => x switch { < 0 => -1, >= 0 => 1 }; }", "E0237");
}

#[test]
fn switch_patterns_must_be_reachable() {
    expect_error("public static class U { public static int F(int x) => x switch { < 10 => 1, < 5 => 2, _ => 3 }; }", "E0238");
    expect_error("public static class U { public static int F(int x) => x switch { _ => 1, 3 => 2 }; }", "E0238");
    expect_error("public static class U { public static int F(int x) => x switch { > 5 and < 3 => 1, _ => 2 }; }", "E0238");
    expect_error("public static class U { public static int F(int x) { switch (x) { case 1: return 1; case 1: return 2; default: return 3; } } }", "E0238");
    // a guarded arm does not cover its values
    expect_ok("public static class U { public static int F(int x, bool b) => x switch { 1 when b => 1, 1 => 2, _ => 3 }; }");
}

#[test]
fn switch_sections_cannot_fall_through() {
    expect_error("public static class U { public static int F(mut int x) { switch (x) { case 1: x += 1; case 2: return 2; default: return 0; } } }", "E0236");
    expect_ok("public static class U { public static int F(int x) { switch (x) { case 1: case 2: return 2; default: return 0; } } }");
    // a switch whose every section returns ends the method
    expect_ok("public static class U { public static int F(int x) { switch (x) { case 1: return 1; default: return 0; } } }");
}

#[test]
fn switch_break_and_continue() {
    expect_ok("public static class U { public static int F() { mut int k = 0; for (int i = 0; i < 3; i++) { switch (i) { case 1: continue; default: k += 1; break; } } return k; } }");
    expect_error("public static class U { public static void F(int x) { switch (x) { default: continue; } } }", "E0233");
}

#[test]
fn switch_types_and_constants() {
    expect_error("public struct SV { public int X; } public static class U { public static int F(SV v) => v switch { _ => 1 }; }", "E0239");
    expect_error("public static class U { public static int F(int x, int y) => x switch { y => 1, _ => 2 }; }", "E0239");
    expect_ok(&format!("{ENUM3} public static class U {{ public static bool F(ETeam t) => t is ETeam.Red or ETeam.Blue; public static bool G(int x) => x is > 0 and < 10; }}"));
}

#[test]
fn switch_statements_must_handle_every_value() {
    // was warning W0237 for enum switches only; now error E0237 for every switch statement
    // without 'default:' that misses a value (as for switch expressions)
    let src = with_game(&format!("{ENUM3} public static class U {{ public static int F(ETeam t) {{ switch (t) {{ case ETeam.Red: return 1; }} return 0; }} }}"));
    assert!(errors(&src).contains(&"E0237"), "{:?}", codes(&src));
    assert!(!codes(&src).contains(&"W0237"), "{:?}", codes(&src));
    expect_error("public static class U { public static int F(int x) { switch (x) { case 1: return 1; case 2: return 2; } return 0; } }", "E0237");
    expect_error("public static class U { public static int F(bool b) { switch (b) { case true: return 1; } return 0; } }", "E0237");
    // accepted: a default section, or cases that cover every value
    expect_ok(&format!("{ENUM3} public static class U {{ public static int F(ETeam t) {{ switch (t) {{ case ETeam.Red: return 1; default: return 0; }} }} }}"));
    expect_ok(&format!("{ENUM3} public static class U {{ public static int F(ETeam t) {{ switch (t) {{ case ETeam.Red: return 1; case ETeam.Blue: return 2; case ETeam.Green: return 3; }} return 0; }} }}"));
    expect_ok("public static class U { public static int F(int x) { switch (x) { case 1: return 1; default: return 0; } } }");
    expect_ok("public static class U { public static int F(bool b) { switch (b) { case true: return 1; case false: return 0; } return 2; } }");
    expect_ok("public static class U { public static int F(bool b) { switch (b) { case true: return 1; default: return 0; } } }");
}

#[test]
fn strings_are_values() {
    expect_ok("public static class U { public static string F(int x) { mut string s = \"a\" + x; s += \"!\"; return s.ToUpper() + $\"{s.Length}\"; } }");
    expect_ok("public class A : Entity { public mut string Name = \"x\"; public mut List<string> Log; public void Update(float dt) { Name = Name + \"y\"; Log.Add(Name); } }");
    expect_error("public static class U { public static int F(string s) => s - 1; }", "E0239");
    expect_error("public static class U { public static bool F(string s) => s < \"b\"; }", "E0239");
    expect_error("public static class U { public static string F(string s) => s.Reverse(); }", "E0101");
}

#[test]
fn strings_are_replicated_and_can_be_sent() {
    expect_ok("public class A : Entity { public string Name; public client void Render() { if (Name.Length > 0) { Draw.Sprite(1, 0, 0); } } }");
    // string RPC arguments travel by value
    expect_ok("public class A : Entity { public mut int Hp; public void Say(string s) { Hp += s.Length; } public client void Input() { Say(\"hi\"); } }");
    // a struct holding a string would need its own wire format
    expect_error(
        "public struct Msg { public string T; } public class A : Entity { public mut int Hp; public void Say(Msg m) { Hp += m.T.Length; } public client void Input() { Say(new Msg { T = \"hi\" }); } }",
        "E0249",
    );
}

#[test]
fn dictionary_rules() {
    expect_ok("public static class U { public static int F() { mut var d = new Dictionary<string, int>(); d[\"a\"] = 1; d[\"a\"] += 2; mut int v; if (d.TryGetValue(\"a\", out v)) { return v; } return d.GetValueOrDefault(\"b\", 5); } }");
    // keys must be hashable value types
    expect_error("public struct SV { public int X; } public static class U { public static void F() { var d = new Dictionary<SV, int>(); } }", "E0228");
    expect_error("public static class U { public static void F() { var d = new Dictionary<float, int>(); } }", "E0228");
    // modifying while iterating
    expect_error("public static class U { public static void F() { mut var d = new Dictionary<int, int>(); foreach (var kv in d) { d.Remove(kv.Key); } } }", "E0406");
    // entity-owned dictionaries must use nullable handles
    expect_error("public class A : Entity { public Dictionary<A, int> M; }", "E0302");
    // another entity's dictionary is not writable directly
    expect_error("public class A : Entity { public mut Dictionary<int, int> M; public void Update(float dt) { foreach (var o in Query<A>()) { o.M[1] = 2; } } }", "E0405");
    // d[k]++ is rejected with a suggestion
    expect_error("public static class U { public static void F() { mut var d = new Dictionary<int, int>(); d[1] = 0; d[1]++; } }", "E0253");
    // TryGetValue out value is definitely assigned
    expect_ok("public static class U { public static int F(Dictionary<int, int> d) { mut int v; d.TryGetValue(1, out v); return v; } }");
}

#[test]
fn coroutine_rules() {
    expect_ok("public class A : Entity { public mut int N; public async void Run() { for (int i = 0; i < 3; i++) { N += i; await Wait(0.5); } await Until(N > 2); await NextTick(); } }");
    // handles may die while suspended
    expect_error("public class A : Entity { public mut A? T; public async void Run() { if (T is A t) { await NextTick(); t.T = null; } } }", "E0802");
    expect_error("public class A : Entity { public async void Run() { foreach (var o in Query<A>()) { await NextTick(); } } }", "E0802");
    expect_error("public class A : Entity { public void Run() { await NextTick(); } }", "E0803");
    // value parameters are allowed now (such a coroutine runs when another one awaits it)
    expect_ok("public class A : Entity { public async void Run(int x) { await NextTick(); } }");
    expect_error("public class A : Entity { public async void Run(out int x) { x = 1; await NextTick(); } }", "E0801");
    expect_error("public static class S { public static async void Run() { } }", "E0801");
    expect_error("public class A : Entity { public int N; public async void Run() { atomic { await NextTick(); } } }", "E0803");
    expect_error("public class A : Entity { public async void Run() { await Sleep(1); } }", "E0803");
}

#[test]
fn extern_functions_are_checked() {
    expect_ok("public static class H { public static extern float Now(); public static extern(\"c_log\") void Log(string s, int level); }");
    expect_error("public static class H { public static extern List<int> Bad(); }", "E0254");
    expect_error("public static class H { public static extern void Bad(ref int x); }", "E0254");
    expect_error("public static class H { public static extern(\"not valid\") void Bad(); }", "E0254");
    expect_error("public class A : Entity { public extern void Bad(); }", "E0254");
}

#[test]
fn try_parse_assigns_its_out_local() {
    expect_ok("public static class U { public static int F(string s) { mut int v; if (int.TryParse(s, out v)) { return v; } return v + 1; } }");
    expect_error("public static class U { public static int F(string s) { mut float v; int.TryParse(s, out v); return 0; } }", "E0253");
}

#[test]
fn warns_when_a_range_default_is_outside_the_range() {
    let src = with_game("public class U : Entity { public mut int in 0..100 Hp = 150; public void Update(float dt) { Hp -= 1; } }");
    assert!(codes(&src).contains(&"W0202"), "{:?}", codes(&src));
    let ok = with_game("public class U : Entity { public mut int in 0..100 Hp = 100; public void Update(float dt) { Hp -= 1; } }");
    assert!(!codes(&ok).contains(&"W0202"));
}

#[test]
fn escapes_follow_csharp() {
    // valid C# escapes compile; unknown ones are errors (also a non-ASCII character after '\')
    expect_ok(r#"public static class S { public static string A() => "\a\b\f\v\0\t\x41é\U0001F600😀"; }"#);
    expect_error(r#"public static class S { public static string A() => "x\qy"; }"#, "E0005");
    expect_error("public static class S { public static string A() => \"x\\한y\"; }", "E0005");
    expect_error(r#"public static class S { public static string A() => "\u12"; }"#, "E0005");
    expect_error(r#"public static class S { public static string A() => "\uD800"; }"#, "E0005");
}

#[test]
fn rejects_integer_division_by_constant_zero() {
    expect_error("public static class S { public static int A(int x) => x / 0; }", "E0256");
    expect_error("public static class S { const int Z = 0; public static int A(int x) => x % Z; }", "E0256");
    expect_error("public static class S { public static int A(mut int x) { x /= 0; return x; } }", "E0256");
    expect_ok("public static class S { public static float A(float x) => x / 0; }");
}

#[test]
fn builtin_constants() {
    expect_ok("public static class S { public static int A() => int.MaxValue - int.MinValue; public static float B() => float.NaN + float.Epsilon + float.MaxValue + float.PositiveInfinity + Math.PI + Math.E; }");
}

#[test]
fn atomic_may_send_messages_whose_body_prints() {
    // the message is a deferred command: it runs after the block, or not at all on `fail`
    expect_ok(
        "public class Pal : Entity { public mut int Notes; public void Note() { Notes += 1; Print($\"note {Notes}\"); } }
         public class Bank : Entity { public mut int Gold = 50;
           public void Update(float dt) { atomic { Gold -= 30; foreach (var p in Query<Pal>()) { p.Note(); } if (Gold < 0) { fail; } } } }",
    );
}

#[test]
fn remote_adds_are_not_snapshot_reads() {
    // others only add to Heat: no hazard, so no double buffer
    let src = with_game(
        "public class Cell : Entity { public mut float Heat; public void Update(float dt) { Heat = Heat * 0.5;
           foreach (var o in Query<Cell>()) { if (o != this) { o.Heat += 1; } } } }",
    );
    let mut d = Diags::default();
    let c = compile(&src, &mut d).expect("compiles");
    assert!(c.ecs.systems.iter().all(|s| s.hazards.is_empty()), "remote += made a hazard");
}

#[test]
fn structs_cannot_contain_themselves() {
    // found by the fuzzer: the C compiler failed on the generated code (CS0523 in C#)
    expect_error("public struct S { public S Inner; }", "E0257");
    expect_error("public struct A { public B Bv; } public struct B { public A Av; }", "E0257");
    expect_ok("public struct V { public float X; } public struct P { public V Pos; public V Vel; }");
}

#[test]
fn out_variable_declarations() {
    let body = "public class A : Entity { public Dictionary<int, int> D; public mut int S;
        public static bool Two(out int a, out float b) { a = 1; b = 2; return true; }
        public void Update(float dt) {
            if (D.TryGetValue(1, out int v) && v > 0) { S += v; }
            S += v;
            if (!int.TryParse(\"7\", out var n)) { return; }
            if (Two(out _, out float f)) { S += n + (int)f; }
        } }";
    expect_ok(body);
    // the variable is in the enclosing block: declaring it twice is an error, as in C#
    expect_error(
        "public class A : Entity { public void Update(float dt) { if (int.TryParse(\"1\", out int x)) { } if (int.TryParse(\"2\", out int x)) { } } }",
        "E0103",
    );
    expect_error("public class A : Entity { public Dictionary<int, int> D; public void Update(float dt) { D.TryGetValue(1, out float x); } }", "E0200");
    // in a loop condition the variable belongs to the loop (C#): two loops may reuse the name
    expect_ok("public class A : Entity { public Dictionary<int, int> D; public mut int K; public void Update(float dt) {
        while (K < 3 && D.TryGetValue(K, out int n)) { K += n; }
        while (K < 6 && D.TryGetValue(K, out int n)) { K += n; }
        for (int i = 0; i < 2 && int.TryParse(\"1\", out var one); i++) { K += one; } } }");
    expect_error(
        "public class A : Entity { public mut int K; public void Update(float dt) { while (K < 1 && int.TryParse(\"3\", out int v)) { K += v; } K += v; } }",
        "E0101",
    );
    // reading a variable that a short-circuited call may not have assigned: C#'s CS0165
    expect_error(
        "public class A : Entity { public mut int K; public void Update(float dt) { bool b = K > 0 && int.TryParse(\"3\", out int v); K += v; } }",
        "E0306",
    );
    expect_error(
        "public class A : Entity { public static bool F(out A? a) { a = null; return false; } public void Update(float dt) { F(out A a); } }",
        "E0303",
    );
}

#[test]
fn arrays_and_lambdas_have_clear_errors() {
    expect_ok("public class A : Entity { public mut int[] G = new int[4]; public string[] N = { \"a\", \"b\" }; public void Update(float dt) {
        mut int[] xs = new int[4]; var ys = new float[] { 1, 2 }; mut int[] zs = { 1, 2, 3 }; xs[0] = zs.Length; Array.Sort(zs);
        foreach (var y in ys) { G[1] += (int)y; } } }");
    expect_error("public class A : Entity { public void Update(float dt) { int[,,,] g = new int[2, 2, 2, 2]; } }", "E0019");
    expect_error("public class A : Entity { public void Update(float dt) { var xs = new int[2] { 1, 2, 3 }; } }", "E0241");
    expect_error("public class A : Entity { public int[] G = new int[Random.Int(1, 3)]; }", "E0221");
    expect_error("public class A : Entity { public A[] Others = new A[3]; }", "E0302");
    expect_error("public class A : Entity { public void Update(float dt) { var xs = new A[3]; } }", "E0302");
    expect_error("public class A : Entity { public void Update(float dt) { int[] xs = new int[2]; xs.Add(1); } }", "E0101");
    expect_error("public class A : Entity { public void Update(float dt) { int[] xs = new int[2]; int n = xs.Count; } }", "E0108");
    // lambdas: arguments of list/array methods only
    expect_ok("public class A : Entity { public mut List<A?> L; public mut List<int> N; public void Update(float dt) { L.Sort((a, b) => 0); var x = L.Find(a => a == this); mut int k = 3; N.RemoveAll(v => v > k); N.ForEach(v => k += v); } }");
    expect_error("public class A : Entity { public void Update(float dt) { var f = (int a) => a + 1; } }", "E0027");
    expect_error("public class A : Entity { public List<int> N; public void Update(float dt) { bool b = N.Exists(v => { N.Add(v); return true; }); } }", "E0241");
    expect_error("public class A : Entity { public mut List<int> N; public void Update(float dt) { bool b = N.Exists(v => N.Remove(v)); } }", "E0406");
    expect_error("public class A : Entity { public int[] N = new int[2]; public void Update(float dt) { int r = N.RemoveAll(v => v > 0); } }", "E0101");
    expect_error("public class A : Entity { public List<int> N; public void Update(float dt) { var s = N.Select(v => v > 0 ? \"a\" : 1); } }", "E0200");
    expect_error("public class A : Entity { public mut List<int> N; public void Update(float dt) { N.Sort((a, b) => a > b); } }", "E0200");
    // one clear message per unsupported form, not a cascade of parse errors
    let src = with_game("public class A : Entity { public void Update(float dt) { int[,,,] xs = new int[4, 4, 4, 4]; int k = 1; } }");
    let got = errors(&src);
    assert_eq!(got, vec!["E0019", "E0019"], "{:?}", got);
}

#[test]
fn multi_dimensional_arrays_follow_csharp_rules() {
    // locals, initializers, fields, parameters, returns, rank 3, nested arrays, Clone, GetLength
    expect_ok(
        "public struct S { public int X; public string N; }
        public static class G {
            public static own int[,] Make(int w, int h) { mut int[,] g = new int[w, h]; g[w - 1, h - 1] = 1; return g; }
            public static int Sum(int[,] g) { mut int s = 0; foreach (int v in g) { s += v; } return s; }
            public static void Bump(ref int[,] g) { g[0, 0] += 1; }
            public static int Keep(own int[,,] c) => c.Length;
            public static T First<T>(T[,] g) => g[0, 0]; }
        public class A : Entity {
            public mut int[,] Cells = new int[4, 4];
            public string[,] Names = { { \"a\", \"b\" }, { \"c\", \"d\" } };
            public mut A?[,] Peers = new A?[2, 2];
            public mut List<int>[,] Bins = new List<int>[2, 2];
            public void Update(float dt) {
                mut int[,] g = G.Make(3, 4); int[,] lit = { { 1, 2 }, { 3, 4 } }; var l2 = new int[,] { { 1 }, { 2 } }; var l3 = new int[2, 1] { { 1 }, { 2 } };
                g[1, 2] = lit[1, 1] + l2[1, 0] + l3[0, 0] + g.GetLength(0) + g.GetUpperBound(1) + g.Rank + g.Length;
                g[0, 0]++; g[0, 1] *= 2; G.Bump(ref g); Cells[1, 1] = G.Sum(g); Bins[0, 1].Add(G.First(g));
                mut S[,] ss = new S[2, 2]; ss[1, 1].X = 3; ss[0, 0] = new S { X = 1, N = \"x\" };
                mut float[,,] cube = new float[2, 2, 2]; cube[1, 1, 1] = 0.5f; int n = G.Keep(new int[1, 2, 3]);
                int[][,] jag = new int[2][,]; int[,][] rows = new int[2, 2][]; int j = jag[0].Length + rows[1, 1].Length;
                var copy = (int[,])g.Clone(); int d = 1; int len = copy.GetLength(d); Peers[0, 1] = this; } }",
    );
    // the number of indexes is the rank
    expect_error("public class A : Entity { public void Update(float dt) { int[,] g = new int[2, 2]; int a = g[1]; } }", "E0239");
    expect_error("public class A : Entity { public void Update(float dt) { int[,] g = new int[2, 2]; int a = g[1, 1, 1]; } }", "E0239");
    expect_error("public class A : Entity { public void Update(float dt) { int[] g = new int[2]; int a = g[1, 1]; } }", "E0239");
    // a constant dimension must exist
    expect_error("public class A : Entity { public void Update(float dt) { int[,] g = new int[2, 2]; int a = g.GetLength(2); } }", "E0241");
    // initializers are rectangular and match the lengths written next to them
    expect_error("public class A : Entity { public void Update(float dt) { int[,] g = { { 1, 2 }, { 3 } }; } }", "E0241");
    expect_error("public class A : Entity { public void Update(float dt) { int[,] g = { 1, 2 }; } }", "E0241");
    expect_error("public class A : Entity { public void Update(float dt) { var g = new int[2, 3] { { 1, 2 }, { 3, 4 } }; } }", "E0241");
    // no list or Array methods on a T[,] (as in C#), no Count
    expect_error("public class A : Entity { public void Update(float dt) { int[,] g = new int[2, 2]; var w = g.Where(x => x > 0); } }", "E0101");
    expect_error("public class A : Entity { public void Update(float dt) { mut int[,] g = new int[2, 2]; Array.Sort(g); } }", "E0241");
    expect_error("public class A : Entity { public void Update(float dt) { int[,] g = new int[2, 2]; int n = g.Count; } }", "E0108");
    // fields: nullable handles, constant lengths; elements of an immutable array do not change
    expect_error("public class A : Entity { public A?[,] Ok = new A?[1, 1]; public A[,] Hs = new A[2, 2]; }", "E0302");
    expect_error("public class A : Entity { public int[,] G = new int[Random.Int(1, 3), 2]; }", "E0221");
    expect_error("public class A : Entity { public void Update(float dt) { int[,] g = new int[2, 2]; g[0, 0] = 1; } }", "E0310");
    expect_error("public class A : Entity { public A? O; public mut int[,] G = new int[2, 2]; public void Update(float dt) { if (O is A o) { o.G[0, 0] = 1; } } }", "E0405");
    // ownership: like any other array
    expect_error("public static class G { public static int Keep(own int[,] g) => g.Length; } public class A : Entity { public void Update(float dt) { int[,] g = new int[2, 2]; int n = G.Keep(g); int m = g[0, 0]; } }", "E0501");
    expect_error("public class A : Entity { public static int[,] Make() => new int[1, 1]; }", "E0505");
    // T[,,,]: at most three dimensions, one error each
    let got = errors(&with_game("public class A : Entity { public int[,,,] G; }"));
    assert_eq!(got, vec!["E0019"], "{:?}", got);
}

#[test]
fn methods_named_like_builtins_take_their_calls() {
    // a method hides the built-in of its name for the calls it can take; the built-in keeps the
    // others (Count<T>(), Destroy(), Spawn(new T {...}), Query<T>() in foreach, Print(x))
    expect_ok(
        "public enum ETeam { Red, Blue }
        public class Unit : Entity {
            public ETeam Team; public mut int Hp = 3;
            public int Count(ETeam t) { mut int n = 0; foreach (var u in Query<Unit>()) { if (u.Team == t) { n++; } } return n; }
            public void Destroy(int delay) { Hp -= delay; }
            public void Print(string s, int n) { Hp += n; }
            public void Update(float dt) { int reds = Count(ETeam.Red); int all = Count<Unit>(); Destroy(1); Print($\"{reds} {all}\"); Print(\"x\", 0); if (Hp <= 0) { Destroy(); } } }
        public static class Waves {
            public static void Spawn(int n) { for (int i = 0; i < n; i++) { Spawn(new Unit { Team = ETeam.Red }); } }
            public static own List<int> Query(int n) { mut var xs = new List<int>(); xs.Add(n); return xs; }
            public static void Run() { Spawn(2); foreach (int x in Query(3)) { Print($\"{x}\"); } foreach (var u in Query<Unit>()) { Print(\"u\"); } } }",
    );
    // a call that fits neither: the method's own error, not the built-in's usage
    expect_error("public enum ETeam { Red } public class Unit : Entity { public int Count(ETeam t) => 0; public void Update(float dt) { int n = Count(ETeam.Red, 2); } }", "E0241");
    let src = with_game("public enum ETeam { Red } public class Unit : Entity { public int Count(ETeam t) => 0; public void Update(float dt) { int n = Count(ETeam.Red, 2); } }");
    let mut d = Diags::default();
    let _ = compile(&src, &mut d);
    assert!(d.list.iter().any(|x| x.msg.contains("'Count' expects 1 argument")), "{:?}", d.list.iter().map(|x| &x.msg).collect::<Vec<_>>());
    // calling itself through the hidden built-in: E0312 says why
    let src = with_game("public static class Log { public static void Print(string s) { Print(\"> \" + s); } }");
    let mut d = Diags::default();
    let _ = compile(&src, &mut d);
    let e = d.list.iter().find(|x| x.code == "E0312").expect("E0312");
    assert!(e.notes.iter().any(|n| n.contains("not the built-in 'Print'")), "{:?}", e.notes);
}

#[test]
fn chained_decrements_of_a_value_are_one_error() {
    // `----1` is `--(--1)`: one error for the chain, with a hint about the signs
    for minus in [4usize, 1600] {
        let src = with_game(&format!("public class A : Entity {{ public mut int X; public void Update(float dt) {{ X = {}1; }} }}", "-".repeat(minus)));
        let mut d = Diags::default();
        let _ = compile(&src, &mut d);
        let errs: Vec<_> = d.list.iter().filter(|x| x.level == Level::Error).collect();
        assert_eq!(errs.len(), 1, "{} minus signs: {:?}", minus, errs.iter().map(|x| &x.msg).collect::<Vec<_>>());
        assert_eq!(errs[0].code, "E0405");
        assert!(errs[0].notes.iter().any(|n| n.contains("- -1")), "{:?}", errs[0].notes);
    }
    let got = errors(&with_game("public class A : Entity { public mut int X; public void Update(float dt) { X = ----X; } }"));
    assert_eq!(got, vec!["E0405"], "{:?}", got);
    let got = errors(&with_game("public class A : Entity { public int X; public void Update(float dt) { int y = ----X; } }"));
    assert_eq!(got.len(), 1, "{:?}", got);
    expect_ok("public class A : Entity { public mut int X; public void Update(float dt) { X = - - -1; X = -(-X); --X; } }");
}

#[test]
fn messages_copy_lists_and_arrays() {
    let recv = "public struct T { public int N; public string L; }
        public class Box : Entity { public mut int N; public mut List<int> Kept;
            public void Take(List<int> xs, int[] ys, int[,] g, List<string> ss, List<T> ts, List<List<int>> nn) { N += xs.Count + ys.Length + g.Length + ss.Count + ts.Count + nn.Count; }
            public void Keep(own List<int> xs) { Kept = xs; }
            public void Look(Dictionary<int, int> d) { N += d.Count; }
            public void Set(ref int x) { N += x; } }";
    expect_ok(&format!(
        "{recv} public class S : Entity {{ public Box? B; public void Update(float dt) {{ if (B is Box b) {{
            var xs = new List<int> {{ 1 }}; int[] ys = {{ 2 }}; var g = new int[1, 1]; var ss = new List<string> {{ \"a\" }}; var ts = new List<T>(); var nn = new List<List<int>>();
            b.Take(xs, ys, g, ss, ts, nn); mut var k = new List<int> {{ 1 }}; b.Keep(k); }} }} }}"
    ));
    expect_error(&format!("{recv} public class S : Entity {{ public Box? B; public void Update(float dt) {{ if (B is Box b) {{ var d = new Dictionary<int, int>(); b.Look(d); }} }} }}"), "E0248");
    expect_error(&format!("{recv} public class S : Entity {{ public Box? B; public mut int V; public void Update(float dt) {{ if (B is Box b) {{ b.Set(ref V); }} }} }}"), "E0248");
    // the sender's list moved into the message
    expect_error(&format!("{recv} public class S : Entity {{ public Box? B; public void Update(float dt) {{ if (B is Box b) {{ mut var k = new List<int>(); b.Keep(k); k.Add(1); }} }} }}"), "E0501");
    // RPCs still take plain values only
    expect_error(&format!("{recv} public class S : Entity {{ public Box? B; public client void Input() {{ if (B is Box b) {{ var xs = new List<int>(); b.Keep(xs); }} }} }}"), "E0608");
}

#[test]
fn list_methods_follow_ownership_rules() {
    let ok = "public class A : Entity { public mut List<int> L; public void Update(float dt) {
        L.Insert(0, 1); bool r = L.Remove(1); int i = L.IndexOf(2); L.Reverse(); L.Sort();
        foreach (var x in L) { int k = L.IndexOf(x); } } }";
    expect_ok(ok);
    expect_error("public struct V { public int X; } public class A : Entity { public void Update(float dt) { mut var vs = new List<V>(); vs.Sort(); } }", "E0239");
    expect_error("public class A : Entity { public mut List<int> L; public void Update(float dt) { foreach (var x in L) { L.Sort(); } } }", "E0406");
    // list changes inside atomic are rolled back on `fail`
    expect_ok("public class A : Entity { public void Update(float dt) { atomic { mut var t = new List<int>(); t.Reverse(); } } }");
    expect_error("public class A : Entity { public mut List<int> L; public A? O; public void Update(float dt) { if (O is A o) { o.L.Sort(); } } }", "E0405");
}

#[test]
fn chars_follow_csharp_conversions() {
    expect_ok("public class A : Entity { public mut char C = 'x'; public mut int N; public void Update(float dt) {
        C++; N = C - 'a'; N += C; float f = C; C = (char)(C + 1); string s = \"ab\" + C; char d = s[0];
        if (C >= 'a' && C != d) { N = s.Length; } foreach (char c in s) { N += c; } } }");
    // int -> char needs a cast; a string is not a char; one character per literal
    expect_error("public class A : Entity { public mut char C; public void Update(float dt) { C = 65; } }", "E0211");
    expect_error("public class A : Entity { public mut char C; public void Update(float dt) { C = \"a\"; } }", "E0200");
    expect_error("public class A : Entity { public char C; public void Update(float dt) { C = 'ab'; } }", "E0004");
    expect_error("public class A : Entity { public char C; public void Update(float dt) { C = ''; } }", "E0004");
    // C# rejects c += 1 (the int result does not convert back implicitly)
    expect_error("public class A : Entity { public mut char C; public void Update(float dt) { C += 1; } }", "E0211");
    // strings are immutable: s[i] is not assignable
    expect_error("public class A : Entity { public mut string S = \"ab\"; public void Update(float dt) { S[0] = 'x'; } }", "E0405");
    expect_error("public class A : Entity { public void Update(float dt) { string s = \"a\"; var x = s.ToCharArray(); } }", "E0241");
}

#[test]
fn nested_collections_follow_ownership_rules() {
    expect_ok("public class A : Entity { public mut List<List<int>> G; public mut int[][] J = new int[2][]; public void Update(float dt) {
        mut var row = new List<int>(); row.Add(1); G.Add(row); G[0].Add(2); J[1] = new int[3]; J[1][2] = G[0][1];
        foreach (var r in G) { int n = r.Count; } var c = G.Clone(); G.RemoveAt(0); } }");
    // a list moved into an element cannot be used afterwards
    expect_error("public class A : Entity { public mut List<List<int>> G; public void Update(float dt) { mut var row = new List<int>(); G.Add(row); row.Add(1); } }", "E0501");
    // moving an element out would leave two owners
    expect_error("public class A : Entity { public List<List<int>> G; public void Update(float dt) { var row = G[0]; } }", "E0502");
    // element lists are not compared by value
    expect_error("public class A : Entity { public List<List<int>> G; public void Update(float dt) { var row = new List<int>(); bool b = G.Contains(row); } }", "E0239");
    expect_error("public class A : Entity { public mut List<List<int>> G; public void Update(float dt) { G.Sort(); } }", "E0239");
    // a foreach variable is a read-only view of the element
    expect_error("public class A : Entity { public mut List<List<int>> G; public void Update(float dt) { foreach (var r in G) { r.Add(1); } } }", "E0405");
    expect_error("public class A : Entity { public List<Dictionary<int, int>> D; }", "E0228");
}

#[test]
fn awaiting_async_methods_follows_the_rules() {
    expect_ok("public class A : Entity { public mut int N; public async void Run() { await Step(2); await Step(N); } async void Step(int k) { N += k; await NextTick(); } }");
    expect_error("public class A : Entity { public async void Run() { await Run(); } }", "E0804");
    expect_error("public class A : Entity { public async void Run() { await B(); } async void B() { await C(); } async void C() { await B(); } }", "E0804");
    expect_error("public class A : Entity { public mut int N; public async void Run() { await Step(2); } async void Step(int k) { k += 1; N = k; await NextTick(); } }", "E0405");
    expect_error("public class A : Entity { public async void Run(ref int k) { await NextTick(); } }", "E0801");
    expect_error("public class A : Entity { public void F() { } public async void Run() { await F(); } }", "E0803");
}

#[test]
fn generic_method_rules() {
    // an instance is checked with its type arguments: an operation the type lacks is an error there
    expect_error(
        "public struct P { public int X; } public static class U { public static T Max<T>(T a, T b) => a > b ? a : b; public static int F() { P p = new P(); return Max(1, 2) + Max(p, p).X; } }",
        "E0239",
    );
    expect_ok("public static class U { public static T Max<T>(T a, T b) => a.CompareTo(b) >= 0 ? a : b; public static int F() => Max(1, 2) + (int)Max(1.5f, 2); }");
    // a type parameter that no argument mentions must be written
    expect_error("public static class U { public static T Zero<T>() => default(T); public static int F() => Zero(); }", "E0259");
    expect_ok("public static class U { public static T Zero<T>() => default(T); public static int F() => Zero<int>(); }");
    // conflicting inferences
    expect_error("public static class U { public static T Max<T>(T a, T b) => a; public static void F() { var x = Max(1, \"s\"); } }", "E0259");
    // wrong number of type arguments
    expect_error("public static class U { public static T Id<T>(T a) => a; public static int F() => Id<int, int>(1); }", "E0241");
    // generic methods are not virtual, async or extern
    expect_error("public class A : Entity { public virtual T Id<T>(T a) => a; }", "E0258");
    expect_error("public class A : Entity { public async void Go<T>() { await NextTick(); } }", "E0258");
    // no overloading by genericity
    expect_error("public static class U { public static int Id(int a) => a; public static T Id<T>(T a) => a; }", "E0104");
    // a type parameter is not nullable unless it is an entity class
    expect_error("public static class U { public static int F<T>(T? a) => 0; public static int G() => F<int>(1); }", "E0210");
    // runaway recursive instantiation
    expect_error("public static class U { public static int F<T>(T a, int n) => n == 0 ? 0 : F(new List<T>(), n - 1); public static int G() => F(1, 3); }", "E0259");
    // a type parameter cannot reuse a type name
    expect_error("public static class U { public static int F<int>(int a) => a; }", "E0258");
    // default(T) of a non-nullable entity reference or a collection
    expect_error("public class A : Entity { } public static class U { public static A F() => default(A); }", "E0301");
    expect_error("public static class U { public static own List<int> F() => default; }", "E0200");
}

#[test]
fn generic_constraints_are_checked() {
    // each constraint is checked against the type arguments of every instance
    let base = "public class Unit : Entity { public int Hp; } public class Tower : Unit { } public class Rock : Entity { } public struct P { public int X; } public struct H { public Unit? U; } ";
    let ok = [
        "public static class U { public static int Sum<T>(List<T> xs) where T : struct => xs.Count; public static int F() => Sum(new List<int>()) + Sum(new List<P>()); }",
        "public static class U { public static int N<T>(T? x) where T : Unit => x is T u ? u.Hp : 0; public static int F(Tower? t) => N(t); }",
        "public static class U { public static int N<T>(List<T?> xs) where T : Entity => xs.Count; public static int F(List<Rock?> r) => N(r); }",
        "public static class U { public static T Mk<T>() where T : struct, new() => default(T); public static int F() => Mk<P>().X; }",
        "public static class U { public static int K<T>(T a) where T : unmanaged => 1; public static int F() => K(new P()) + K(1.5f); }",
        "public static class U { public static int L<T>(T a) where T : class => 1; public static int F(Rock r) => L(\"s\") + L(r) + L(new List<int>()); }",
        "public struct Box<T> where T : struct { public T V; } public static class U { public static int F() { Box<int> b = new Box<int>(); return b.V; } }",
    ];
    for src in ok {
        expect_ok(&format!("{}{}", base, src));
    }
    let bad = [
        ("public static class U { public static int Sum<T>(List<T> xs) where T : struct => xs.Count; public static int F() => Sum(new List<string>()); }", "E0260"),
        ("public static class U { public static int N<T>(T? x) where T : Unit => 0; public static int F(Rock? r) => N(r); }", "E0260"),
        ("public static class U { public static int N<T>(T x) where T : Entity => 0; public static int F() => N(3); }", "E0260"),
        ("public static class U { public static int K<T>(T a) where T : unmanaged => 1; public static int F() => K(new H()); }", "E0260"),
        ("public static class U { public static int L<T>(T a) where T : class => 1; public static int F() => L(1); }", "E0260"),
        ("public static class U { public static int M<T>(T a) where T : new() => 1; public static int F() => M(\"s\"); }", "E0260"),
        ("public static class U { public static int Q<T>(T a) where T : notnull => 1; public static int F(Rock? r) => Q(r); }", "E0260"),
        ("public struct Box<T> where T : struct { public T V; } public static class U { public static int F(Box<string> b) => 0; }", "E0260"),
        // the clause names a type parameter, once; class and struct exclude each other
        ("public static class U { public static int F<T>(T a) where X : struct => 1; }", "E0260"),
        ("public static class U { public static int F<T>(T a) where T : struct where T : new() => 1; }", "E0260"),
        ("public static class U { public static int F<T>(T a) where T : class, struct => 1; }", "E0260"),
        ("public static class U { public static int F(int a) where T : struct => 1; }", "E0260"),
        // a type constraint is Entity or an entity class
        ("public static class U { public static int F<T>(T a) where T : P => 1; }", "E0260"),
        ("public static class U { public static int F<T>(T a) where T : IComparable<T> => 1; }", "E0105"),
    ];
    for (src, code) in bad {
        expect_error(&format!("{}{}", base, src), code);
    }
}

#[test]
fn interface_rules() {
    let i = "public interface IHit { void Hit(int n); } ";
    expect_ok(&format!("{}public class A : Entity, IHit {{ public mut int Hp; public void Hit(int n) {{ Hp -= n; }} }} public static class G {{ public static void F() {{ Spawn(new A()); foreach (var h in Query<IHit>()) {{ h.Hit(1); }} }} }}", i));
    // an abstract class may leave the methods to its subclasses; an interface may extend another
    expect_ok(&format!("{}public interface IHeal : IHit {{ void Heal(int n); }} public abstract class U : Entity, IHeal {{ public void Heal(int n) {{ }} }} public class B : U {{ public void Hit(int n) {{ }} }} public static class G {{ public static void F(IHeal? x) {{ if (x is IHeal h) {{ h.Hit(1); h.Heal(2); }} }} }}", i));
    // a method with a body is the default implementation; a generic interface is instantiated
    expect_ok(&format!("{}public interface IBox<T> {{ T Get(); bool Some() => true; }} public class A : Entity, IHit, IBox<int> {{ public void Hit(int n) {{ }} public int Get() => 1; }}", i));
    let bad = [
        // every method must be implemented, with the same signature
        ("public class A : Entity, IHit { }", "E0261"),
        ("public class A : Entity, IHit { public void Hit(float n) { } }", "E0261"),
        ("public interface IHeal : IHit { void Heal(int n); } public class A : Entity, IHeal { public void Heal(int n) { } }", "E0261"),
        ("public struct S : IHit { }", "E0261"),
        ("public struct S : IHit { public static void Hit(int n) { } }", "E0261"),
        // an interface has signatures and default implementations only (tests/interface_rules.rs)
        ("public interface IBad { int X; }", "E0262"),
        ("public interface IBad { async void M() { } }", "E0262"),
        ("public interface IBad { static virtual void M() { } }", "E0262"),
        ("public interface IBad { void M<T>(T x); }", "E0262"),
        // the base class comes first; the rest must be interfaces
        ("public class A : IHit { public void Hit(int n) { } }", "E0209"),
        ("public class B : Entity { } public class A : Entity, B { }", "E0209"),
        ("public struct P { } public struct S : P { }", "E0209"),
        ("public class B : Entity { } public interface IBad : B { }", "E0209"),
        ("public interface IA : IB { } public interface IB : IA { }", "E0214"),
        // generic interfaces are instantiated per type arguments; .NET's are unknown types
        ("public interface IG<T> { T Get(); } public class A : Entity, IG<int> { public float Get() => 1; }", "E0261"),
        ("public struct S : IEquatable<S> { }", "E0105"),
        // an interface is not spawned, and a struct value is not an interface
        ("public static class G { public static void F() { Spawn(new IHit()); } }", "E0246"),
        ("public struct S : IHit { public void Hit(int n) { } } public static class G { public static void F() { IHit? h = new S(); } }", "E0200"),
        // the signature includes parameter modes, ranges and 'client'
        ("public interface IGive { void Give(own List<int> xs); } public class A : Entity, IGive { public void Give(List<int> xs) { } }", "E0261"),
        ("public interface IScore { int in 0..100 Score(); } public class A : Entity, IScore { public int Score() { return 1; } }", "E0261"),
        ("public interface IDraw { client void Draw(); } public class A : Entity, IDraw { public void Draw() { } }", "E0261"),
    ];
    for (src, code) in bad {
        expect_error(&format!("{}{}", i, src), code);
    }
}

#[test]
fn collection_initializers() {
    expect_ok("public static class U { public static int F() { var xs = new List<int> { 1, 2, 3 }; var d = new Dictionary<string, int> { { \"a\", 1 }, [\"b\"] = 2 }; return xs.Count + d.Count; } }");
    // a dictionary initializer only initializes a variable
    expect_error("public static class U { public static int G(Dictionary<int, int> d) => d.Count; public static int F() => G(new Dictionary<int, int> { [1] = 2 }); }", "E0245");
    expect_error("public static class U { public static int F() { var xs = new List<int> { { 1, 2 } }; return xs.Count; } }", "E0245");
    expect_error("public static class U { public static int F() { var d = new Dictionary<int, int> { 1, 2 }; return d.Count; } }", "E0245");
}

#[test]
fn generic_struct_rules() {
    expect_ok("public struct Box<T> { public T V; public Box(T v) { V = v; } public T Get() => V; } public static class U { public static int F() => new Box<int>(3).Get() + Box2().V; static Box<int> Box2() => default; }");
    // wrong number of type arguments
    expect_error("public struct Box<T> { public T V; } public static class U { public static int F() { Box<int, int> b = default; return 0; } }", "E0228");
    // a struct instance that contains itself
    expect_error("public struct Node<T> { public T V; public Node<T> Next; } public static class U { public static void F() { Node<int> n = default; } }", "E0257");
    // runaway instantiation
    expect_error("public struct W<T> { public W<Box<T>> Inner; } public struct Box<T> { public T V; } public static class U { public static void F() { W<int> w = default; } }", "E0259");
    // structs, entity classes, static classes, interfaces and methods are generic; enums are not
    expect_ok("public static class A<T> { }");
    expect_error("public enum E<T> { A }", "E0258");
    // an instance is checked with its type arguments
    expect_error("public struct S { } public struct Box<T> { public T V; public int Twice() => V * 2; } public static class U { public static int F() => new Box<int>().Twice() + new Box<S>().Twice(); }", "E0239");
    // lists are still not struct fields, through a type parameter either
    expect_error("public struct Box<T> { public T V; } public static class U { public static void F() { Box<List<int>> b = default; } }", "E0216");
}

/// Generic entity classes: each instance (`Pool<int>`) is an entity class of its own, declared and
/// checked with its type arguments; mistakes are reported once, at the use or the declaration.
#[test]
fn generic_entity_class_rules() {
    let pool = "public struct V { public float X; } public interface IScore { int Score(); } \
                public class Pool<T> : Entity, IScore where T : struct { public mut List<T> Items; public T Seed; public mut int Ticks; \
                  public void Update(float dt) { Ticks += 1; Items.Add(Seed); } public void Put(T x) { Items.Add(x); } \
                  public virtual int Score() => Items.Count; public static int Cap() => 3; } ";
    let ok = [
        // spawn, handles, Query, Count, is, remote calls, static methods; two instances
        "public class Home : Entity { public Pool<int>? Mine; public void Update(float dt) { if (Mine is Pool<int> p) { p.Put(1); } } } \
         public static class G { public static void F() { Pool<int>? a = Spawn(new Pool<int> { Seed = 2 }); Spawn(new Pool<V> { }); Spawn(new Home { Mine = a }); \
           foreach (var p in Query<Pool<V>>()) { p.Put(new V()); } mut int n = Count<Pool<int>>() + Pool<int>.Cap(); \
           foreach (var s in Query<IScore>()) { if (s is Pool<int> q) { n += q.Score(); } } } }",
        // a class derived from an instance overrides a method of the generic base
        "public class IntPool : Pool<int> { public mut int Sum; public override int Score() => base.Score() + Sum; } \
         public static class G { public static void F() { Spawn(new IntPool { Seed = 1 }); } }",
        // a generic class derived from a class (or an instance), overriding; abstract generic classes
        "public class Marker : Entity { public mut int Marks; public virtual void Mark() { Marks += 1; } } \
         public class Labeled<T> : Marker { public mut T Value; public override void Mark() { Marks += 2; } } \
         public class Layer<T> : Labeled<List<T>> { public void Update(float dt) { Value.Add(default(T)); } } \
         public abstract class Brain<T> : Entity { public mut T Memory; public abstract T Think(T x); public void Update(float dt) { Memory = Think(Memory); } } \
         public class Doubler : Brain<int> { public override int Think(int x) => x * 2; } \
         public static class G { public static void F() { Spawn(new Labeled<string> { Value = \"a\" }); Spawn(new Layer<int> { }); Spawn(new Doubler { }); } }",
        // `where T : Entity`: nullable references to T, queries of T
        "public class Ship : Entity { public mut int Hp; } \
         public class Hunter<T> : Entity where T : Ship { public mut T? Prey; public void Update(float dt) { if (Prey is T p) { p.Hp -= 1; return; } foreach (var s in Query<T>()) { Prey = s; } } } \
         public static class G { public static void F() { Spawn(new Hunter<Ship> { }); } }",
        // a generic method instantiates a generic class with its own type argument
        "public static class G { public static Pool<T>? Make<T>(T seed) where T : struct => Spawn(new Pool<T> { Seed = seed }); public static void F() { Make(1); Make(1.5f); } }",
        // generic methods of generic classes, coroutines, client code
        "public class Pair<A, B> : Entity { public mut A First; public mut B Second; public int Tally<C>(C x) => 1; \
           public async void Run() { await NextTick(); First = default(A); } public client void Render() { Draw.Text(0, 0, $\"{First}{Tally(2)}\"); } } \
         public static class G { public static void F() { Spawn(new Pair<int, string> { Second = \"s\" }); } }",
        // a generic class nobody uses is not checked (like generic methods and structs)
        "public class Unused<T> : Entity { public void Update(float dt) { T x = 1; } }",
    ];
    for src in ok {
        expect_ok(&format!("{}{}", pool, src));
    }
    let bad = [
        // a generic class is used with its type arguments, as many as it declares
        ("public static class G { public static void F() { Spawn(new Pool { }); } }", "E0228"),
        ("public static class G { public static int F() => Count<Pool>(); }", "E0228"),
        ("public static class G { public static int F() => Pool.Cap(); }", "E0228"),
        ("public static class G { public static bool F(Pool<int>? p) => p is Pool; }", "E0228"),
        ("public class D : Pool { }", "E0228"),
        ("public static class G { public static void F() { Spawn(new Pool<int, int> { }); } }", "E0228"),
        // 'where' constraints are checked per instance, at the use (also as a base class)
        ("public static class G { public static void F() { Spawn(new Pool<string> { }); } }", "E0260"),
        ("public class D : Pool<string> { }", "E0260"),
        // an instance whose members cannot exist with its type arguments
        ("public class Ship : Entity { } public class Box<T> : Entity { public T Item; } public static class G { public static void F() { Spawn(new Box<Ship> { }); } }", "E0302"),
        ("public class Ref<T> : Entity { public T? Target; } public static class G { public static void F() { Spawn(new Ref<int> { }); } }", "E0210"),
        ("public class Twice<T> : Entity { public mut T V; public void Update(float dt) { V = V * 2; } } public static class G { public static void F() { Spawn(new Twice<string> { }); } }", "E0239"),
        // runaway instantiation, inheritance cycles, a type parameter as the base class
        ("public class Node<T> : Entity { public Node<List<T>>? Next; } public static class G { public static void F() { Spawn(new Node<int> { }); } }", "E0259"),
        ("public class Loop<T> : Loop<T> { } public static class G { public static void F() { Spawn(new Loop<int> { }); } }", "E0214"),
        ("public class Over<T> : T { }", "E0209"),
        // what a generic class cannot declare
        ("public static class Game<T> { public static void Start() { } }", "E0258"),
        ("public class C<T> : Entity { public const int MAX = 3; }", "E0258"),
        ("public class C<T> : Entity { public int F<T>(T x) => 1; }", "E0258"),
        ("public class C<T> : Entity { public static extern int F(); }", "E0258"),
        ("public class C<T, T> : Entity { }", "E0103"),
        ("public class C<int> : Entity { }", "E0258"),
        // instances follow the rules of entity classes
        ("public static class G { public static void F() { var p = new Pool<int>(); } }", "E0212"),
        ("public abstract class A<T> : Entity { } public static class G { public static void F() { Spawn(new A<int> { }); } }", "E0246"),
        ("public class D : Pool<int> { public override int Score() => 1.5f; }", "E0211"),
        ("public class B<T> : Entity { public virtual void Hit(T x) { } } public class D : B<int> { public override void Hit(float x) { } }", "E0220"),
        ("public class Q<T> : Entity, IScore { } public static class G { public static void F() { Spawn(new Q<int> { }); } }", "E0261"),
        // a class or interface that is not generic takes no type arguments after ':'
        ("public class Ship : Entity { } public class D : Ship<int> { }", "E0228"),
        ("public class D : Entity, IScore<int> { }", "E0228"),
    ];
    for (src, code) in bad {
        expect_error(&format!("{}{}", pool, src), code);
    }
    // one error per mistake: a mistake that does not depend on the type arguments is reported once
    let src = with_game("public class P<T> : Entity { public void Update(float dt) { int x = \"s\"; } } public static class G { public static void F() { Spawn(new P<int> { }); Spawn(new P<float> { }); } }");
    assert_eq!(errors(&src), vec!["E0200"], "{}", src);
    // a mistake in an instance says which instance
    let mut d = Diags::default();
    let _ = compile(&with_game("public class P<T> : Entity { public mut T V; public void Update(float dt) { V = V * 2; } } public static class G { public static void F() { Spawn(new P<int> { }); Spawn(new P<bool> { }); } }"), &mut d);
    let errs: Vec<_> = d.list.iter().filter(|x| x.level == Level::Error).collect();
    assert_eq!(errs.len(), 1, "{:?}", d.list);
    assert!(errs[0].notes.iter().any(|n| n.contains("'P<bool>.Update'")), "{:?}", errs[0]);
}

#[test]
fn race_and_sync_rules() {
    let co = "public class A : Entity { public mut int N; public async void Walk(int k) { while (N < k) { N++; await NextTick(); } } public async void Idle() { await Wait(1); } ";
    expect_ok(&format!("{}public async void Run() {{ mut int w = await Race(Walk(3), Wait(0.5f), Until(N > 9), NextTick()); await Sync(Walk(w), Idle()); w = await Race(Idle(), NextTick()); }} }}", co));
    // a Race sets a variable like an assignment: an immutable one is E0310
    expect_error(&format!("{}public async void Run() {{ int w = 0; w = await Race(Idle(), NextTick()); }} }}", co), "E0310");
    // one method cannot run twice at the same time (one frame per entity)
    expect_error(&format!("{}public async void Run() {{ await Sync(Walk(1), Walk(2)); }} }}", co), "E0804");
    // Sync has no value; plain awaits neither
    expect_error(&format!("{}public async void Run() {{ int w = await Sync(Walk(1), Idle()); }} }}", co), "E0803");
    expect_error(&format!("{}public async void Run() {{ int w = await Wait(1); }} }}", co), "E0803");
    // arms are async methods of this entity, Wait, Until or NextTick
    expect_error(&format!("{}public void Plain() {{ }} public async void Run() {{ await Race(Plain(), Idle()); }} }}", co), "E0803");
    expect_error(&format!("{}public async void Run() {{ await Race(Idle()); }} }}", co), "E0803");
    // the value is an int
    expect_error(&format!("{}public async void Run() {{ float w = await Race(Idle(), NextTick()); }} }}", co), "E0200");
    // Rush leaves arms running: only in a coroutine that runs as its own system
    expect_ok(&format!("{}public async void Run() {{ int w = await Rush(Walk(3), Wait(0.5f)); await Rush(Idle(), Walk(w)); }} }}", co));
    expect_error(&format!("{}public async void Lead() {{ await Rush(Walk(3), Wait(0.5f)); }} public async void Run() {{ await Lead(); }} }}", co), "E0805");
    expect_error(&format!("{}public async void Go(int n) {{ await Rush(Walk(n), Idle()); }} }}", co), "E0805");
}

#[test]
fn replicated_list_rules() {
    // lists of values (strings, structs, nested lists) replicate
    expect_ok("public struct S { public string N; public float W; } public class A : Entity { public List<int> Xs; public List<S> Ss; public List<List<int>> G; public int[] Slots = new int[3]; public client void Render() { Draw.Text(0, 0, $\"{Xs.Count}{Ss.Count}{G.Count}{Slots[0]}\"); } }");
    // so do collections of entity references: lists, arrays, structs in lists, nested lists, and
    // dictionaries with reference keys or values (a client maps every element to its own handle)
    expect_ok("public struct L { public A? To; public int W; } public class A : Entity { public List<A?> Friends; public A?[] Wing = new A?[2]; public List<L> Links; public List<List<A?>> Squads; public Dictionary<A?, int> Threat; public Dictionary<string, A?> Named; public client void Render() { Draw.Text(0, 0, $\"{Friends.Count}{Wing.Length}{Links.Count}{Squads.Count}{Threat.Count}{Named.Count}\"); } }");
    // a client may not see the target of a reference: references in replicated collections must be nullable
    expect_error("public class A : Entity { public List<List<A>> Squads; public client void Render() { Draw.Text(0, 0, $\"{Squads.Count}\"); } }", "E0606");
    // the same field is fine while no client code reads it
    expect_ok("public class A : Entity { public mut List<List<A>> Squads; public int N; public void Update(float dt) { Squads.Clear(); } public client void Render() { Draw.Text(0, 0, $\"{N}\"); } }");
}

#[test]
fn net_view_is_client_only() {
    expect_ok("public struct V { public float X; public float Y; } public class A : Entity { public V Focus; public client void Look() { Net.View(Focus.X, Focus.Y, 20); } }");
    expect_error("public class A : Entity { public float X; public void Update(float dt) { Net.View(X, 0, 20); } }", "E0602");
}

// ---- the grammar: C# syntax Sijo does not have, attributes, declarations and bodies

#[test]
fn csharp_modes_and_modifiers_sijo_lacks_are_errors() {
    // 'in' parameters and arguments: plain parameters already borrow
    expect_error("public static class U { public static int F(in int x) => x; }", "E0028");
    expect_error("public static class U { public static int F(int x) => x; public static int G() { int a = 1; return F(in a); } }", "E0028");
    expect_ok("public static class U { public static int F(int x) => x; public static int G() { int a = 1; return F(a); } }");
    // readonly / static readonly: a constant is 'const' (any type, computed at compile time)
    expect_error("public class A : Entity { public readonly int X = 1; }", "E0028");
    expect_error("public static class K { public static readonly int R = 3; }", "E0028");
    expect_ok("public static class K { public const int R = 3; public const float H = R * 0.5f; }");
    // modifiers with no meaning in Sijo
    expect_error("internal class A : Entity { }", "E0028");
    expect_error("public sealed class A : Entity { }", "E0028");
    expect_error("public partial class A : Entity { }", "E0028");
    expect_error("public static class U { public unsafe static int F() => 1; }", "E0028");
    expect_error("public static class U { public static int F(this int x) => x; }", "E0028");
    // 'mut' where the binding already writes the caller's variable, or only borrows a collection
    expect_error("public static class U { public static void F(mut ref int x) { x = 1; } }", "E0028");
    expect_error("public static class U { public static void F(mut out int x) { x = 1; } }", "E0028");
    expect_error("public static class U { public static int F(mut List<int> xs) => xs.Count; }", "E0028");
    expect_ok("public static class U { public static void F(ref int x) { x = 1; } public static void G(out int x) { x = 1; } public static int H(mut own List<int> xs) { xs.Add(1); return xs.Count; } }");
    // struct members follow the variable or field that holds the struct
    expect_error("public struct V { public mut int X; }", "E0028");
    expect_ok("public struct V { public int X; } public class A : Entity { public mut V P; public void Update(float dt) { P.X += 1; } }");
    // 'mut' on a method is for struct methods that change 'this'
    expect_error("public class A : Entity { public mut void F() { } }", "E0028");
    // modifiers that do not apply to a field
    expect_error("public class A : Entity { public own List<int> L; }", "E0028");
    expect_error("public class A : Entity { public client int X; }", "E0028");
    expect_error("public static class K { public const mut int X = 1; }", "E0028");
    expect_ok("public class A : Entity { public mut List<int> L; public mut int X; public client void Render() { Draw.Text(0, 0, $\"{X}{L.Count}\"); } public void Update(float dt) { X += 1; L.Add(X); } }");
}

#[test]
fn new_member_modifier_is_an_error() {
    expect_error("public class B : Entity { public virtual int F() => 1; } public class C : B { public new int F() => 2; }", "E0028");
    expect_error("public class B : Entity { public new int X; }", "E0028");
}

#[test]
fn only_planner_and_checked_attributes_remain() {
    // unknown attributes
    expect_error("public class A : Entity { [Foo] public int X; }", "E0029");
    expect_error("public static class U { [Inline] public static int F() => 1; }", "E0029");
    // C# and Unity attributes whose meaning Sijo writes as a modifier or a range type
    expect_error("public class A : Entity { [Range(0, 10)] public int X; }", "E0029");
    expect_error("public class A : Entity { [Client] public void Render() { } }", "E0029");
    expect_error("public class A : Entity { [Server] public void Update(float dt) { } }", "E0029");
    expect_error("public static class H { [DllImport(\"c_now\")] public static extern float Now(); }", "E0029");
    expect_error("public static class H { [Extern] public static extern float Now(); }", "E0029");
    // method attributes on fields and field attributes on methods
    expect_error("public class A : Entity { [Pure] public int X; }", "E0029");
    expect_error("public class A : Entity { [Recursive] public int X; }", "E0029");
    expect_error("public class A : Entity { [Replicated] public void F() { } }", "E0029");
    expect_error("public static class U { [Precision(0.5)] public static float F() => 1; }", "E0029");
    // accepted: [Precision], [Replicated], [NotReplicated], [Spatial], [Component] on fields; [Pure], [Recursive] on methods
    expect_ok(
        "public struct P { public float X; public float Y; }
         public class A : Entity { [Spatial] public P Pos; [Component(\"Body\")] public int Hp; [Replicated] public int R; [NotReplicated] public int Q;
           [Precision(0.01)] public mut float in 0..10 F; public void Update(float dt) { F += dt; } }
         public static class U { [Pure] public static int Sq(int x) => x * x; [Recursive] public static int Fact(int n) => n <= 1 ? 1 : n * Fact(n - 1); }",
    );
    expect_ok("public static class H { public static extern float Now(); public static extern(\"c_log\") void Log(string s); }");
}

#[test]
fn one_declaration_declares_one_name() {
    expect_error("public static class U { public static int F() { int a = 1, b = 2; return a + b; } }", "E0030");
    expect_error("public static class U { public static int F() { mut int a, b; a = 1; b = 2; return a + b; } }", "E0030");
    expect_error("public class A : Entity { public int X, Y; }", "E0030");
    expect_error("public static class U { public static int F() { mut int k = 0; for (int i = 0, j = 3; i < j; i++) { k += i; } return k; } }", "E0030");
    expect_ok("public class A : Entity { public int X; public int Y; } public static class U { public static int F() { int a = 1; int b = 2; return a + b; } }");
}

#[test]
fn bodies_are_blocks() {
    let f = |body: &str| format!("public static class U {{ public static int F(int x, List<int> xs) {{ mut int k = 0; {} return k; }} }}", body);
    expect_error(&f("if (x > 0) k = 1;"), "E0031");
    expect_error(&f("if (x > 0) { k = 1; } else k = 2;"), "E0031");
    expect_error(&f("while (k < x) k++;"), "E0031");
    expect_error(&f("do k++; while (k < x);"), "E0031");
    expect_error(&f("for (int i = 0; i < x; i++) k += i;"), "E0031");
    expect_error(&f("foreach (var v in xs) k += v;"), "E0031");
    // a ';' right after the condition ends the statement: an empty body is '{ }'
    expect_error(&f("if (x > 0); { k = 1; }"), "E0031");
    expect_error(&f("while (k > x);"), "E0031");
    // accepted: blocks, and 'else if' continues a chain
    expect_ok(&f(
        "if (x > 0) { k = 1; } else if (x < 0) { k = -1; } else { k = 0; }
         while (k < x) { k++; } do { k++; } while (k < x); for (int i = 0; i < x; i++) { k += i; } foreach (var v in xs) { k += v; } if (x > 9) { }",
    ));
}

// ---- range types

#[test]
fn ranges_belong_to_storage_of_numbers() {
    // not on struct members (put it on the entity field holding the struct) or constants
    expect_error("public struct V { public float in 0..1 X; }", "E0224");
    expect_error("public static class K { public const int in 0..10 N = 3; }", "E0224");
    // a ref/out argument keeps the range of the variable it refers to
    expect_error("public static class U { public static void F(ref int in 0..10 x) { x = 1; } }", "E0224");
    expect_error("public static class U { public static void F(out int in 0..10 x) { x = 1; } }", "E0224");
    // a range belongs to a declaration, not to a generic argument, an element type or a cast
    expect_error("public class A : Entity { public List<int in 0..9> L; }", "E0224");
    expect_error("public static class U { public static int F() { var xs = new List<int in 0..9>(); return xs.Count; } }", "E0224");
    expect_error("public static class U { public static int F(float x) { int y = (int in 0..10)x; return y; } }", "E0224");
    // numbers only: entity fields may hold a struct of numbers, other storage int or float
    expect_error("public class A : Entity { public bool in 0..1 B; }", "E0224");
    expect_error("public class A : Entity { public int[] in 0..9 Xs = new int[2]; }", "E0224");
    expect_error("public struct S { public int N; public string T; } public class A : Entity { public S in 0..9 V; }", "E0224");
    expect_error("public struct V { public int X; } public static class U { public static int F() { V in 0..9 v = new V(); return v.X; } }", "E0224");
    // the bounds: literals, smaller first, integers for an int that fit in 32 bits
    expect_error("public class A : Entity { public int in 10..0 X; }", "E0224");
    expect_error("public static class K { public const int N = 9; } public class A : Entity { public int in 0..N X; }", "E0224");
    expect_error("public class A : Entity { public int in 0.2..0.7 X; }", "E0224");
    expect_error("public class A : Entity { public int in 0..3000000000 X; }", "E0224");
    expect_error("public class A : Entity { public int in -3000000000..0 X; }", "E0224");
    // accepted: locals, value parameters, return types; entity fields of int, float or a struct of numbers
    expect_ok(
        "public struct SV { public float X; public int Y; }
         public class A : Entity { public mut int in 0..100 Hp = 100; public mut float in -1.5..1.5 Tilt; [Precision(0.01)] public mut SV in 0..64 Pos;
           public void Update(float dt) { Hp -= 1; Tilt += dt; Pos.X += dt; } }
         public static class U { public static int in 0..9 Digit(int in -100..100 n) { mut int in 0..9 d = n % 10; d += 1; float in 0..1 t = 0.5f; return d + (int)t; } }",
    );
}

// ---- immutable by default

#[test]
fn bindings_are_immutable_unless_mut() {
    let u = "public struct V { public int X; public mut void Bump() { X += 1; } }
             public static class H { public static void Inc(ref int x) { x += 1; } public static void Set(out int x) { x = 1; } }
             public static class U { ";
    let e = |body: &str| format!("{}public static void F() {{ {} }} }}", u, body);
    // locals: assignment, compound assignment, ++, ref and out arguments
    expect_error(&e("int x = 1; x = 2;"), "E0310");
    expect_error(&e("int x = 1; x += 2;"), "E0310");
    expect_error(&e("int x = 1; x++;"), "E0310");
    expect_error(&e("int x = 1; H.Inc(ref x);"), "E0310");
    expect_error(&e("int x = 1; H.Set(out x);"), "E0310");
    // TryParse / TryGetValue into an existing local
    expect_error(&e("int v = 0; bool ok = int.TryParse(\"1\", out v);"), "E0310");
    expect_error(&e("var d = new Dictionary<int, int>(); int v = 0; bool ok = d.TryGetValue(1, out v);"), "E0310");
    // collections: Add, Clear, an element set, a dictionary set
    expect_error(&e("var xs = new List<int>(); xs.Add(1);"), "E0310");
    expect_error(&e("var xs = new List<int>(); xs.Clear();"), "E0310");
    expect_error(&e("int[] xs = new int[2]; xs[0] = 1;"), "E0310");
    expect_error(&e("var d = new Dictionary<int, int>(); d[1] = 2;"), "E0310");
    // a struct member changes with the variable that holds the struct
    expect_error(&e("V v = new V(); v.X = 1;"), "E0310");
    // an immutable local needs its value at the declaration
    expect_error(&e("int x;"), "E0310");
    // accepted: the 'mut' versions
    expect_ok(&e(
        "mut int x = 1; x = 2; x += 2; x++; H.Inc(ref x); H.Set(out x); bool ok = int.TryParse(\"1\", out x);
         mut var d = new Dictionary<int, int>(); d[1] = 2; bool found = d.TryGetValue(1, out x);
         mut var xs = new List<int>(); xs.Add(x); xs.Clear(); mut int[] ys = new int[2]; ys[0] = 1;
         mut V v = new V(); v.X = 1; v.Bump(); mut int later; later = x;",
    ));
    // accepted: an inline 'out int v' / 'out var v' is initialized by the call
    expect_ok(&format!("{}public static int F(Dictionary<int, int> d) {{ if (int.TryParse(\"1\", out int v) && d.TryGetValue(v, out var w)) {{ H.Set(out int z); return v + w + z; }} return 0; }} }}", u));
}

#[test]
fn parameters_are_immutable_unless_mut() {
    expect_error("public static class U { public static int F(int n) { n = 1; return n; } }", "E0310");
    expect_error("public static class U { public static int F(int n) { n++; return n; } }", "E0310");
    // a borrowed collection parameter is read-only (take it 'mut own' or 'ref' to change it)
    expect_error("public static class U { public static int F(List<int> xs) { xs.Add(1); return xs.Count; } }", "E0405");
    expect_ok("public static class U { public static int F(mut int n) { n++; n = n * 2; return n; } public static int G(mut own List<int> xs) { xs.Add(1); return xs.Count; } }");
}

#[test]
fn own_parameters_are_immutable_unless_mut() {
    expect_error("public static class U { public static int F(own List<int> xs) { xs.Add(1); return xs.Count; } }", "E0310");
    expect_error("public static class U { public static int F(own List<int> xs) { xs = new List<int>(); return xs.Count; } }", "E0310");
}

#[test]
fn entity_fields_are_immutable_unless_mut() {
    // a write to this entity's field, or to another entity's
    expect_error("public class A : Entity { public int Hp; public void Update(float dt) { Hp = 1; } }", "E0310");
    expect_error("public class A : Entity { public int Hp; public void Update(float dt) { Hp -= 1; } }", "E0310");
    expect_error("public class A : Entity { public int Hp; public void Update(float dt) { foreach (var o in Query<A>()) { o.Hp -= 1; } } }", "E0310");
    expect_error("public class A : Entity { public List<int> L; public void Update(float dt) { L.Add(1); } }", "E0310");
    expect_error("public struct V { public float X; } public class A : Entity { public V P; public void Update(float dt) { P.X += dt; } }", "E0310");
    expect_ok(
        "public struct V { public float X; } public class A : Entity { public mut int Hp; public mut List<int> L; public mut V P;
           public void Update(float dt) { Hp -= 1; L.Add(Hp); P.X += dt; foreach (var o in Query<A>()) { o.Hp -= 1; } } }",
    );
    // accepted: a field initializer and Spawn's initializer set immutable fields
    expect_ok(
        "public class A : Entity { public int Hp = 5; public int Team; public float Speed = 2.5f; }
         public class B : Entity { public void Update(float dt) { var a = Spawn(new A { Hp = 3, Team = 1 }); } }",
    );
}

#[test]
fn struct_methods_that_change_this_are_mut() {
    // a struct method that changes 'this' must say so
    expect_error("public struct V { public int X; public void Bump() { X += 1; } }", "E0310");
    expect_error("public struct V { public int X; public void Reset() { this = new V(); } }", "E0310");
    // a mut method needs a mutable place: not an immutable local, and not 'this' of a non-mut method
    expect_error("public struct V { public int X; public mut void Bump() { X += 1; } } public static class U { public static int F() { V v = new V(); v.Bump(); return v.X; } }", "E0403");
    expect_error("public struct V { public int X; public mut void Bump() { X += 1; } public void Twice() { Bump(); Bump(); } }", "E0403");
    expect_error("public struct V { public int X; public mut void Bump() { X += 1; } } public class A : Entity { public V P; public void Update(float dt) { P.Bump(); } }", "E0403");
    // accepted: mut methods on mutable places, a mut method calling another, a constructor writing 'this'
    expect_ok(
        "public struct V { public int X; public V(int x) { X = x; } public mut void Bump() { X += 1; } public mut void Twice() { Bump(); Bump(); } public int Get() => X; }
         public class A : Entity { public mut V P; public void Update(float dt) { P.Bump(); mut V v = new V(2); v.Twice(); P.X += v.Get(); } }",
    );
}

#[test]
fn for_header_variables_change_only_in_the_step() {
    expect_error("public static class U { public static int F() { mut int k = 0; for (int i = 0; i < 3; i++) { i += 1; k += i; } return k; } }", "E0310");
    expect_error("public static class U { public static int F() { mut int k = 0; for (int i = 0; i < 3; i++) { k += i; i = 5; } return k; } }", "E0310");
    expect_ok("public static class U { public static int F() { mut int k = 0; for (int i = 0; i < 10; i += 2) { k += i; } for (int j = 3; j > 0; j--) { k -= j; } return k; } }");
    // foreach variables are immutable too (a read-only view of the element)
    expect_error("public static class U { public static int F(List<int> xs) { mut int k = 0; foreach (var x in xs) { x = 1; k += x; } return k; } }", "E0405");
}

// ---- recursion is marked

#[test]
fn recursion_is_marked_recursive() {
    // direct and mutual recursion
    expect_error("public static class U { public static int F(int n) => n == 0 ? 0 : F(n - 1); }", "E0312");
    let mutual = "public static class U { public static int A(int n) => n == 0 ? 0 : B(n - 1); public static int B(int n) => A(n); }";
    let got = errors(&with_game(mutual));
    assert_eq!(got, vec!["E0312", "E0312"], "both functions of the cycle are reported: {:?}", got);
    // a call on an entity may dispatch to an override that calls back
    let virt = "public class A : Entity { public virtual int Step(int n) => 0; public int Run(int n) => Step(n); }
                public class B : A { public override int Step(int n) => n == 0 ? 0 : Run(n - 1); }";
    expect_error(virt, "E0312");
    // accepted with [Recursive]
    expect_ok("public static class U { [Recursive] public static int F(int n) => n == 0 ? 0 : F(n - 1); }");
    expect_ok("public static class U { [Recursive] public static int A(int n) => n == 0 ? 0 : B(n - 1); [Recursive] public static int B(int n) => A(n); }");
    expect_ok(
        "public class A : Entity { public virtual int Step(int n) => 0; [Recursive] public int Run(int n) => Step(n); }
         public class B : A { [Recursive] public override int Step(int n) => n == 0 ? 0 : Run(n - 1); }",
    );
    // [Recursive] on a function that never calls itself: accepted with a warning
    let src = with_game("public static class U { [Recursive] public static int F(int n) => n + 1; }");
    assert!(errors(&src).is_empty(), "{:?}", codes(&src));
    assert!(codes(&src).contains(&"W0312"), "{:?}", codes(&src));
}

#[test]
fn base_calls_are_not_recursion() {
    // base.Update runs exactly the base method, never this override again
    let src = with_game(
        "public class A : Entity { public mut int N; public virtual void Update(float dt) { N += 1; } }
         public class B : A { public override void Update(float dt) { base.Update(dt); N += 2; } }",
    );
    assert!(errors(&src).is_empty(), "{:?}", codes(&src));
    assert!(!codes(&src).contains(&"W0312"), "{:?}", codes(&src));
}

#[test]
fn deferred_messages_are_not_recursion() {
    // o.Hit(n) is a message that runs after this system, not on this call's stack
    let src = with_game(
        "public class A : Entity { public mut int Hp; public void Hit(int n) { Hp -= n; foreach (var o in Query<A>()) { if (o != this) { o.Hit(n); } } } }",
    );
    assert!(errors(&src).is_empty(), "{:?}", codes(&src));
    assert!(!codes(&src).contains(&"E0312"), "{:?}", codes(&src));
}

// ---- ownership in signatures

#[test]
fn returned_collections_are_own() {
    expect_error("public static class U { public static List<int> Make() => new List<int>(); }", "E0505");
    expect_error("public static class U { public static int[] Make() => new int[3]; }", "E0505");
    expect_error("public static class U { public static Dictionary<int, int> Make() => new Dictionary<int, int>(); }", "E0505");
    expect_error("public class A : Entity { public List<int> Evens => new List<int>(); }", "E0505");
    // 'own' applies to collections only
    expect_error("public static class U { public static own int F() => 1; }", "E0505");
    expect_error("public static class U { public static int F(own int x) => x; }", "E0505");
    expect_error("public struct V { public int X; } public static class U { public static int F(own V v) => v.X; }", "E0505");
    // an async method cannot take ownership (its parameters live in the entity across ticks)
    expect_error("public class A : Entity { public async void Run(own List<int> xs) { await NextTick(); } }", "E0505");
    expect_ok(
        "public static class U { public static own List<int> Make() => new List<int>(); public static own int[] Arr() => new int[3];
           public static own Dictionary<int, int> Map() => new Dictionary<int, int>(); }
         public class A : Entity { public own List<int> Evens => new List<int>(); }",
    );
}

#[test]
fn own_parameters_take_the_collection() {
    let u = "public static class U { public static int Sum(own List<int> xs) => xs.Count; public static own List<int> Make() => new List<int>(); ";
    // the caller's variable moves into the call: using it afterwards is E0501
    expect_error(&format!("{}public static int F() {{ var xs = new List<int>(); int n = U.Sum(xs); return n + xs.Count; }} }}", u), "E0501");
    expect_error(&format!("{}public static int F() {{ mut var xs = new List<int>(); int n = U.Sum(xs); xs.Add(1); return n; }} }}", u), "E0501");
    // an entity's list field cannot move out: E0502 (pass L.Clone())
    expect_error(&format!("{} }} public class A : Entity {{ public List<int> L; public void Update(float dt) {{ int n = U.Sum(L); }} }}", u), "E0502");
    // a borrowed parameter is not the callee's to give away: E0504
    expect_error(&format!("{}public static int F(List<int> xs) => U.Sum(xs); }}", u), "E0504");
    // accepted: a fresh list, a call result, a clone, a local at its last use, an own parameter passed on
    expect_ok(&format!(
        "{}public static int F(List<int> b) {{ mut var xs = new List<int>(); xs.Add(1); int n = U.Sum(new List<int>()) + U.Sum(Make()) + U.Sum(b.Clone()); return n + U.Sum(xs); }}
           public static int Pass(own List<int> xs) => U.Sum(xs); }}
         public class A : Entity {{ public List<int> L; public void Update(float dt) {{ int n = U.Sum(L.Clone()); }} }}",
        u
    ));
    // accepted: an own parameter stored into a mut field
    expect_ok("public class A : Entity { public mut List<int> L; public void Take(own List<int> xs) { L = xs; } public void Update(float dt) { Take(new List<int>()); } }");
}

#[test]
fn range_bounds_can_be_constants() {
    // a bound is a number or a constant computed from numbers and other constants, named in the
    // same type or as Type.NAME, optionally negative
    expect_ok(
        "public static class Rules { public const int MAX_HP = 100; public const float ARENA = 64; public const float HALF = ARENA / 2; }
         public class Ship : Entity { public const int CAP = Rules.MAX_HP + 20;
           public mut int in 0..Rules.MAX_HP Hp; public mut float in -Rules.HALF..Rules.HALF X; public mut int in 0..CAP Score;
           public static int in 0..CAP Clamp(int v) => v;
           public void Update(float dt) { mut int in -Rules.MAX_HP..CAP k = 0; k += 1; Hp -= 1; X += dt; Score += Clamp(k); } }",
    );
    let bad = [
        // a constant computed by a call is evaluated later (CTFE): not a bound
        "public static class R { public static int F() => 3; public const int A = F(); } public class S : Entity { public mut int in 0..R.A X; }",
        "public class S : Entity { public mut int in 0..Nope X; }",
        "public static class R { public const int A = 3; } public class S : Entity { public mut int in 0..R.B X; }",
        "public static class R { public const string A = \"x\"; } public class S : Entity { public mut int in 0..R.A X; }",
        // after evaluation the range must not be empty
        "public static class R { public const int A = 3; } public class S : Entity { public mut int in 5..R.A X; }",
    ];
    for src in bad {
        expect_error(src, "E0224");
    }
}

#[test]
fn local_constants() {
    // a local const takes a compile-time value (constants, CTFE of pure calls) and never changes
    expect_ok("public static class R { public const int MAX = 10; public static int Sq(int x) => x * x; }
        public class S : Entity { public mut int V; public void Update(float dt) { const int N = R.MAX * 2; const int Q = R.Sq(3); const string T = \"hi\"; V = N + Q + T.Length; } }");
    expect_error("public class S : Entity { public mut int V; public void Update(float dt) { const int N = V; V = N; } }", "E0215");
    expect_error("public class S : Entity { public mut int V; public void Update(float dt) { const int N; V = 1; } }", "E0215");
    expect_error("public class S : Entity { public mut int V; public void Update(float dt) { const int N = 1; N = 2; V = N; } }", "E0405");
    expect_error("public class S : Entity { public void Update(float dt) { const var N = 1; } }", "E0028");
    expect_error("public class S : Entity { public void Update(float dt) { mut const int N = 1; } }", "E0028");
}

#[test]
fn a_name_is_a_field_or_a_method_not_both() {
    expect_error("public struct SV { public float X; public float Y; public float X() => 1; }", "E0104");
    expect_error("public class A : Entity { public int Hp; public int Hp() => 2; }", "E0104");
    expect_ok("public struct SV { public float X; public float Y; public float Len() => X + Y; }");
}

#[test]
fn precision_without_a_range_is_a_warning() {
    let src = with_game("public class A : Entity { [Precision(0.1)] public mut float F; public void Update(float dt) { F += dt; } public client void Render() { Draw.Sprite(0, F, 0); } }");
    assert!(codes(&src).contains(&"W0203"), "{:?}", codes(&src));
    assert!(errors(&src).is_empty());
    let ranged = with_game("public class A : Entity { [Precision(0.1)] public mut float in 0..10 F; public void Update(float dt) { F += dt; } public client void Render() { Draw.Sprite(0, F, 0); } }");
    assert!(!codes(&ranged).contains(&"W0203"));
}

#[test]
fn overloading_rules() {
    // overloads differ in parameter types or modes; the best fit is chosen
    expect_ok("public static class U { public static int F(int a) => 1; public static int F(float a) => 2; public static int F(int a, int b) => 3; public static int G() => F(1) + F(1.5f) + F(1, 2) + F('c'); }");
    expect_ok("public static class U { public static bool R(string s, out int v) => int.TryParse(s, out v); public static bool R(string s, out float v) => float.TryParse(s, out v); public static bool G() => R(\"1\", out int a) && R(\"2\", out float b); }");
    expect_ok("public class A : Entity { public mut int N; public void Hit(int n) { N -= n; } public void Hit(int n, int k) { N -= n * k; } } public class B : Entity { public A? T; public void Update(float dt) { if (T is A a) { a.Hit(1); a.Hit(2, 3); } } }");
    expect_ok("public struct SV { public float X; public SV Add(SV o) => new SV { X = X + o.X }; public SV Add(float d) => new SV { X = X + d }; } public static class U { public static float G() => new SV().Add(1).Add(new SV()).X; }");
    // the same parameter types twice (the return type does not count)
    expect_error("public static class U { public static int F(int a) => 1; public static float F(int b) => 2; }", "E0104");
    // no overload fits, or two fit equally well
    expect_error("public static class U { public static int F(int a) => 1; public static int F(string a) => 2; public static int G() => F(true); }", "E0267");
    expect_error("public static class U { public static int F(int a) => 1; public static int F(string a) => 2; public static int G() => F(1, 2); }", "E0267");
    expect_error("public static class U { public static int F(int a, float b) => 1; public static int F(float a, int b) => 2; public static int G() => F(1, 1); }", "E0267");
    // a ref argument picks by exact type
    expect_error("public static class U { public static void F(ref int a) { a = 1; } public static void F(ref float a) { a = 1; } public static void G() { mut char c = 'a'; F(ref c); } }", "E0267");
    // what has one declaration per name
    expect_error("public class A : Entity { public void Update(float dt) { } public void Update(int n) { } }", "E0267");
    expect_error("public class A : Entity { public int Hp => 1; public int Hp(int x) => x; }", "E0267");
    expect_error("public class A : Entity { public async void Go() { await NextTick(); } public void Go(int n) { } }", "E0267");
    expect_error("public static class H { public static extern int Key(int k); public static extern int Key(float k); }", "E0267");
    expect_error("public static class U { public static int F(int a) => a; public static T F<T>(T a) => a; }", "E0104");
    // an override names the base method by its parameter types; a method that does not say
    // 'override' and takes other parameters is a new overload
    expect_error("public abstract class B : Entity { public virtual int W(int x) => x; } public class C : B { public override int W(float x) => 1; }", "E0220");
    expect_error("public abstract class B : Entity { public virtual int W(int x) => x; } public class C : B { public override float W(int x) => 1; }", "E0220");
    expect_ok("public abstract class B : Entity { public virtual int W(int x) => x; } public class C : B { public int W(bool b) => 1; public int G() => W(1) + W(true); }");
    // an abstract overload must be implemented
    expect_error("public abstract class B : Entity { public abstract int W(int x); public abstract int W(float x); } public class C : B { public override int W(int x) => 1; }", "E0250");
}

#[test]
fn delegate_rules() {
    // values: lambdas, static methods, methods bound to a reference; calls; fields, lists, structs
    expect_ok("public delegate int Op(int a, int b); public static class U { public static int Add(int a, int b) => a + b; public static int Ap(Op o) => o(1, 2); public static Func<int, int> Adder(int n) => x => x + n; public static int G() => Ap(Add) + Ap((a, b) => a * b) + Adder(3)(4); }");
    expect_ok("public class A : Entity { public mut int N; public Func<int, int> Twice = x => x * 2; public mut List<Action> Todo; public void Hit(int n) { N -= n; } public void Update(float dt) { Action<int> h = Hit; h(1); Todo.Add(() => { N++; }); foreach (var t in Todo) { t(); } N += Twice(2); } }");
    expect_ok("public struct SOp { public Func<float, float> F; } public static class U { public static float G() { var s = new SOp { F = x => x / 2 }; return s.F(3) + s.F.Invoke(1); } }");
    expect_ok("public static class U { public static Func<int, int> Compose(Func<int, int> f, Func<int, int> g) => x => g(f(x)); public static int G() => Compose(x => x + 1, x => x * 2)(3); }");
    expect_ok("public static class U { public static T Ap<T>(Func<T, T> f, T x) => f(x); public static int G() => Ap(x => x + 1, 2) + Ap<int>(x => x, 1); }");
    // a lambda needs a delegate type
    expect_error("public static class U { public static void G() { var f = (int x) => x * 2; } }", "E0027");
    // captures: copies of immutable locals, parameters and this
    expect_error("public static class U { public static Func<int> G() { mut int n = 1; n++; return () => n; } }", "E0268");
    expect_error("public static class U { public static Func<int> G(List<int> xs) => () => xs.Count; }", "E0268");
    expect_error("public static class U { public static void G(ref int a) { Func<int> f = () => a; } }", "E0268");
    expect_error("public struct SV { public int X; public mut void Grow() { Func<int> f = () => X; X += f(); } }", "E0268");
    expect_error("public abstract class B : Entity { public virtual int V() => 1; } public class C : B { public override int V() => 2; public int W() { Func<int> f = () => base.V(); return f(); } }", "E0268");
    // signatures: the parameter count, the method group's types, what a delegate returns
    expect_error("public static class U { public static void G() { Func<int, int> f = (a, b) => a; } }", "E0268");
    expect_error("public static class U { public static int F(float x) => 1; public static void G() { Func<int, int> f = F; } }", "E0268");
    expect_error("public class A : Entity { } public static class U { public static void G() { Func<A> f = default; } }", "E0268");
    expect_error("public static class U { public static void G() { Func<List<int>> f = default; } }", "E0268");
    expect_error("public delegate void D(ref int x);", "E0268");
    expect_error("public static class U { public static void G() { Func<int>? f = default; } }", "E0210");
    // where a delegate cannot go: components, messages, RPC arguments, client code, replication
    expect_error("public component C { public Action F; }", "E0264");
    expect_error("public class A : Entity { public mut int N; public void Do(Action a) { N++; a(); } } public class B : Entity { public A? T; public void Update(float dt) { if (T is A a) { a.Do(() => { }); } } }", "E0248");
    expect_error("public class A : Entity { public Func<int> F = () => 1; public client void Render() { Draw.Text(0, 0, $\"{F()}\"); } }", "E0268");
    expect_error("public class A : Entity { public client void Render() { Func<int> f = () => 1; } }", "E0268");
    // no comparison, printing or Contains
    expect_error("public static class U { public static bool G(Action a, Action b) => a == b; }", "E0239");
    expect_error("public static class U { public static void G(Action a) { Print(a); } }", "E0239");
}

#[test]
fn generic_static_classes() {
    // one set of functions and constants per type argument, reached as Name<T>.Member
    expect_ok("public static class Box<T> { public const int Size = 2; public const T None = default(T); public static T Pick(bool a, T x, T y) => a ? x : y; public static T Id(T x) => Pick(true, x, x); } public static class U { public static int F() => Box<int>.Pick(false, 1, 2) + Box<string>.Size + Box<int>.None + Box<float>.Id(1).CompareTo(1.0f); }");
    // a type parameter reaches its type argument's static members (and the built-in ones)
    expect_ok("public static class P<T> { public static T Parse(string s) => T.Parse(s); public static T Max() => T.MaxValue; } public static class U { public static int F() => P<int>.Parse(\"3\") + P<int>.Max(); }");
    // a static class is not a type, and needs its type arguments
    expect_error("public static class Box<T> { public static T Id(T x) => x; } public static class U { public static void F() { Box<int> b = default; } }", "E0229");
    expect_error("public static class Box<T> { public static T Id(T x) => x; } public static class U { public static int F() => Box.Id(1); }", "E0228");
    // no instance state in a static class; Game is not generic
    expect_error("public static class Box<T> { public mut T Last; }", "E0206");
    expect_error("public static class Box<T> { public const int X; }", "E0215");
    // an instance is checked with its type arguments
    expect_error("public static class Box<T> { public static T Neg(T x) => -x; } public static class U { public static string F() => Box<string>.Neg(\"a\"); }", "E0239");
}

#[test]
fn unused_generic_code_is_checked() {
    // what is wrong whatever the type arguments is reported without any instance
    expect_error("public class Pool<T> : Entity { public mut int N; public void Put(T x) { N = N + \"a\"; } }", "E0200");
    expect_error("public class Pool<T> : Entity { public int M() => Nope(3); }", "E0101");
    expect_error("public struct SBox<T> { public T V; public int F() { int y = 1; y = 2; return y; } }", "E0310");
    expect_error("public static class U { public static T Id<T>(T x) { int a = \"s\"; return x; } }", "E0200");
    expect_error("public static class Util<T> { public static int F() => G(); }", "E0101");
    expect_error("public class Pool<T> : Entity { public int M() { if (true) { return 1; } } }", "E0207");
    // operations on T are checked per instance (nothing to say without one)
    expect_ok("public class Pool<T> : Entity { public mut List<T> Items; public T Best() { mut T b = Items[0]; foreach (var x in Items) { if (x > b) { b = x; } } return b; } }");
    expect_ok("public struct SBox<T> { public T V; public int Len() => V.Length + V.Count(); public T Neg() => -V; }");
    expect_ok("public static class U { public static T Twice<T>(T x) => x + x; public static T Z<T>() => T.Zero(); }");
    // one report for an error that shows in the instances too
    let src = with_game("public class Pool<T> : Entity { public int M() => Nope(3); } public static class G { public static void F() { Spawn(new Pool<int>()); } }");
    assert_eq!(errors(&src), vec!["E0101"]);
}

#[test]
fn persistence_rules() {
    expect_ok("public class P : Entity { [PersistentKey] public string Account; [Persistent] public mut int Gold; [Persistent] public mut List<string> Items; [Persistent] public mut Dictionary<string, int> Seen; }");
    // a subclass shares its base's key and adds fields
    expect_ok("public class P : Entity { [PersistentKey] public string Account; [Persistent] public mut int Gold; } public class Q : P { [Persistent] public mut float Xp; }");
    expect_error("public class P : Entity { [PersistentKey] public int Id; [Persistent] public mut int Gold; }", "E0269");
    expect_error("public class P : Entity { [Persistent] public mut int Gold; }", "E0269");
    expect_error("public class P : Entity { [PersistentKey] public string A; [PersistentKey] public string B; }", "E0269");
    expect_error("public class P : Entity { [PersistentKey] public string Account; [Persistent] public P? Friend; }", "E0269");
    expect_error("public class P : Entity { [PersistentKey] public string Account; [Persistent] public Action Then; }", "E0269");
    expect_error("public class P : Entity { [PersistentKey, Persistent] public string Account; }", "E0269");
    expect_error("public component C { [Persistent] public int X; }", "E0269");
}
