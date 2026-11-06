//! C11 (GNU) code generation from the planned HIR.
//!
//! Layout of the generated program:
//!   runtime header | user structs/enums/consts | components | archetype tables | world
//!   | table ops & accessors | user functions (entity methods monomorphized per archetype)
//!   | deferred-command plumbing | systems & stage scheduler | replication | shards | state i/o
//!   | networking runtime (runtime/sijo_net.h: transport, snapshots, RPCs, process modes) | main
//!
//! The networking runtime comes after every piece of code that names user identifiers, because the
//! socket headers it includes define lower-case macros (`h_addr`, `sa_handler`...) that could
//! otherwise rewrite user field names.

use crate::ast::{BinOp, ParamMode, UnOp};
use crate::consteval;
use crate::driver::Compiled;
use crate::ecs::SysKind;
use crate::hir::*;
use crate::memory::{Cap, Region};
use crate::net::{Leaf, Quant};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write;

const RUNTIME: &str = include_str!("../runtime/sijo_rt.h");
const STR_RUNTIME: &str = include_str!("../runtime/sijo_str.h");
const DICT_RUNTIME: &str = include_str!("../runtime/sijo_dict.h");
const GRID_RUNTIME: &str = include_str!("../runtime/sijo_grid.h");
const NET_RUNTIME: &str = include_str!("../runtime/sijo_net.h");
const CRYPTO_RUNTIME: &str = include_str!("../runtime/sijo_crypto.h");
const EMBED_RUNTIME: &str = include_str!("../runtime/sijo_embed.c.h");
/// the shard runtime: types and helpers (before the generated code) | world logic (after it)
const SHARD_RUNTIME: &str = include_str!("../runtime/sijo_shard.h");
const SHARD_NET_RUNTIME: &str = include_str!("../runtime/sijo_shard_net.h");
const SHARD_SPLIT: &str = "/* @@SIJO_SHARD_BODY@@ */";
/// template of the public header written next to a `--lib` build (see `embed_header`)
const EMBED_HEADER: &str = include_str!("../runtime/sijo_game.h");
/// API names of a `--lib` build: sijo_game_create, sijo_game_step ... (`--lib-prefix` changes it)
pub const DEFAULT_LIB_PREFIX: &str = "sijo_game";

/// The public header of a `--lib` build: the embedding API (functions named `<prefix>_create`...)
/// and the prototypes of the host functions the game calls (extern).
pub fn embed_header(c: &Compiled, prefix: &str) -> String {
    let mut h = EMBED_HEADER
        .replace("GAMEAPI_HDR", &format!("{}.h", prefix))
        .replace("GAMEAPI_GUARD", &format!("{}_H", prefix.to_ascii_uppercase()))
        .replace("GAMEAPI_", &format!("{}_", prefix));
    h.push_str("\n/* host functions the game calls (extern in Sijo): define them in the host program */\n");
    h.push_str(&Gen::new(c).extern_decls());
    h.push_str("\n#ifdef __cplusplus\n}\n#endif\n#endif\n");
    h
}

/// warnings that generated code triggers by design (unused helpers, GNU statement expressions...),
/// silenced so that a host's -Wall -Wextra -Werror build stays clean; restored at the end
const DIAG_PUSH: &str = "#if defined(__GNUC__)
#pragma GCC diagnostic push
#if defined(__clang__)
#pragma clang diagnostic ignored \"-Wunknown-warning-option\"
#pragma clang diagnostic ignored \"-Wparentheses-equality\"
#pragma clang diagnostic ignored \"-Wgnu-statement-expression\"
#endif
#pragma GCC diagnostic ignored \"-Wpedantic\"
#pragma GCC diagnostic ignored \"-Wunused-function\"
#pragma GCC diagnostic ignored \"-Wunused-parameter\"
#pragma GCC diagnostic ignored \"-Wunused-variable\"
#pragma GCC diagnostic ignored \"-Wunused-but-set-variable\"
#pragma GCC diagnostic ignored \"-Wunused-value\"
#pragma GCC diagnostic ignored \"-Wunused-const-variable\"
#pragma GCC diagnostic ignored \"-Wmissing-field-initializers\"
#pragma GCC diagnostic ignored \"-Wunknown-pragmas\"
#pragma GCC diagnostic ignored \"-Wsign-compare\"
#pragma GCC diagnostic ignored \"-Wclobbered\"
#endif
";

const C_RESERVED: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern", "float", "for",
    "goto", "if", "inline", "int", "long", "register", "restrict", "return", "short", "signed", "sizeof", "static", "struct",
    "switch", "typedef", "union", "unsigned", "void", "volatile", "while", "bool", "true", "false", "main", "row", "w", "self",
    "NULL", "printf", "exit", "free", "malloc",
];

/// lower-case identifiers that the C headers we include define as macros
const C_MACROS: &[&str] = &[
    "linux", "unix", "i386", "errno", "stdin", "stdout", "stderr", "assert", "offsetof", "alignof", "alignas", "complex",
    "imaginary", "noreturn", "static_assert", "thread_local", "isinf", "isnan", "signbit", "isfinite", "isnormal", "fpclassify",
    "va_start", "va_end", "va_arg", "setjmp", "putc", "getc", "putchar", "getchar",
];

/// what `dg_cases` does with a boxed captured value
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DgMode {
    Read,
    Gc,
    Fill,
}

/// a type the program's C code declares: a struct, or a delegate type (function number + captures)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TypeNode {
    Struct(StructId),
    Delegate(DelegateId),
}

/// Non-ASCII identifier characters (e.g. Hangul) are spelled `_uXXXX_` in C.
fn mangle(s: &str) -> String {
    let mut o = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            o.push(ch);
        } else if ch == ' ' {
        } else if ch.is_ascii() {
            o.push_str(&format!("_x{:02x}_", ch as u32)); // KeyValuePair<int, string> etc.
        } else {
            o.push_str(&format!("_u{:04x}_", ch as u32));
        }
    }
    o
}

/// A class or struct name in the saved state, whose tokens are separated by white space: the
/// instance `Pair<int, string>` is written `Pair<int,string>`.
fn state_name(s: &str) -> String {
    s.replace(' ', "")
}

/// A C string literal (UTF-8 bytes kept as-is, which C allows inside string literals).
pub(crate) fn c_str(s: &str) -> String {
    let mut o = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '?' => o.push_str("\\?"), // avoid trigraphs
            c if (c as u32) < 0x20 => o.push_str(&format!("\\{:03o}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// the method arms of every await Rush(...) in a coroutine body, with their background bits
fn collect_rush(b: &Block, out: &mut Vec<(u32, Expr)>, bg: &mut Option<Expr>) {
    for s in &b.stmts {
        if let Stmt::Await { kind: AwaitKind::Group { arms, rush: Some(r), .. }, .. } = s {
            *bg = Some(r.bg.clone());
            for (j, a) in arms.iter().enumerate() {
                if let (AwaitArm::Call(call), b) = (a, r.bits[j]) {
                    if b != 0 {
                        out.push((b, call.clone()));
                    }
                }
            }
        }
        for x in s.sub_blocks() {
            collect_rush(x, out, bg);
        }
    }
}

pub(crate) fn cid(s: &str) -> String {
    // non-ASCII names, and generic method instances ("Max<int>")
    if !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return mangle(s);
    }
    // ALL_CAPS names (longer than one letter) may be macros from the C headers (NAN, EOF, INT32_MAX...)
    let macro_like = s.len() > 1 && !s.chars().any(|c| c.is_ascii_lowercase());
    if C_RESERVED.contains(&s) || C_MACROS.contains(&s) || macro_like || s.starts_with("sijo") || s.starts_with("Sijo") || s.starts_with("SIJO") {
        format!("{}_", s)
    } else {
        s.to_string()
    }
}

/// Like `generate`, plus `#line` directives mapping the C code to the .sijo sources (debuggers,
/// sanitizers and runtime panics then name .sijo lines). `cfile` is the generated file's name.
pub fn generate_with(c: &Compiled, path: &str, sm: &crate::diag::SourceMap, cfile: &str) -> String {
    let mut g = Gen::new(c);
    g.sm = Some(sm.clone());
    g.cfile = cfile.to_string();
    g.program(path);
    g.out
}

/// `generate_with`, optionally as an embeddable library (no main; the API of `embed_header`)
pub fn generate_full(c: &Compiled, path: &str, sm: &crate::diag::SourceMap, cfile: &str, lib: bool, prefix: &str) -> String {
    let mut g = Gen::new(c);
    g.sm = Some(sm.clone());
    g.cfile = cfile.to_string();
    g.lib = lib;
    g.prefix = prefix.to_string();
    g.program(path);
    g.out
}

pub fn generate(c: &Compiled, path: &str) -> String {
    let mut g = Gen::new(c);
    g.program(path);
    g.out
}

#[derive(Clone)]
struct FCtx {
    fid: FuncId,
    arch: Option<ClassId>,
    view: bool,
    cursors: HashMap<LocalId, (ClassId, String)>,
    break_labels: Vec<Option<String>>,
    atomic_fail: Option<String>,
    /// coroutine frame: lvalue of the resume point
    co_state: Option<String>,
    /// loop-invariant handle work hoisted out of query loops, by the span of the comparison or `is`:
    /// the C variable holding the row of the handle (comparisons) or the `is` result
    hoisted: HashMap<(u32, u32), String>,
    /// fields of `this` kept in locals for the whole method (see row_cache_fields)
    cache: BTreeSet<FieldId>,
    /// stores the cached fields the method writes back to the row (before every return)
    writeback: String,
    /// rewind of the frame arena at the start of a query loop's body (see loop_rewind)
    rewind_first: String,
    /// coroutines with Rush(...): the background bit set (C place) and (bit, method, call) of every
    /// method arm Rush may leave running
    rush_bg: Option<String>,
    rush: Vec<(u32, FuncId)>,
    /// the atomic blocks around the current statement, outermost first
    atomics: Vec<ALevel>,
    /// the loops (true) and switch statements (false) around the current statement, each with the
    /// number of atomic blocks around it: `break`/`continue`/`return` commit the blocks they leave
    jumps: Vec<(bool, usize)>,
    /// undo-log copies (`as{n}_{k}`) certainly made before the current statement (an earlier
    /// statement of an enclosing block made sure of them)
    copied: BTreeSet<(usize, usize)>,
    /// the code is shared by the tables of this group: `this` is reached through the view `V`
    shared: Option<usize>,
}

/// An atomic block being generated: its number (C names `afail{n}`, `as{n}_{k}`...) and the
/// collections its undo log covers.
#[derive(Clone)]
struct ALevel {
    n: usize,
    colls: Vec<ACol>,
}

/// A collection in an atomic block's undo log. `append`: every change only adds at the end, so the
/// log is the length on entry (`am{n}_{k}`); otherwise `as{n}_{k}` is a copy made just before the
/// first change (`at{n}_{k}` = made), put back on `fail`.
#[derive(Clone)]
struct ACol {
    root: crate::effects::CollRoot,
    append: bool,
    ty: Ty,
    /// C lvalue of the collection
    place: String,
}

/// Methods compiled once for several tables (docs/04 §4.4.1): the class methods of a class with
/// combination tables, or the methods of a component on every table that has it. Their code reaches
/// the entity's columns through a view (`SijoView_<id>`: the table's column pointers and archetype,
/// built once per system loop or call) instead of naming the table.
#[derive(Clone, Debug)]
pub(crate) struct ShareGroup {
    /// what shares the code, for comments and `explain`
    pub(crate) what: String,
    /// the archetype the shared code is generated for (every member gives the same code)
    pub(crate) rep: ClassId,
    pub(crate) members: Vec<ClassId>,
    /// the columns in the view
    pub(crate) clusters: Vec<usize>,
}

pub(crate) struct Gen<'a> {
    pub(crate) c: &'a Compiled,
    pub(crate) p: &'a Program,
    pub(crate) out: String,
    pub(crate) tmp: usize,
    pub(crate) arch_id: HashMap<ClassId, usize>,
    pub(crate) hazard_comps: BTreeSet<usize>,
    pub(crate) deferred_funcs: BTreeSet<FuncId>,
    /// stable RNG stream id per Random.* call site: hash(function, n-th call in that function)
    pub(crate) rng_keys: HashMap<usize, u32>,
    /// grid index per (accelerated proximity loop, queried archetype)
    pub(crate) grid_ids: BTreeMap<(usize, ClassId), usize>,
    /// sources for #line directives (None: no directives)
    pub(crate) sm: Option<crate::diag::SourceMap>,
    pub(crate) cfile: String,
    pub(crate) last_pos: Option<(usize, usize)>,
    /// --lib: no main(), an embedding API instead
    pub(crate) lib: bool,
    /// --lib API names: <prefix>_create ...
    pub(crate) prefix: String,
    /// --checked: integer overflow traps
    pub(crate) checked: bool,
    /// `--profile-fields`: count every entity field access for the profile
    pub(crate) field_counts: bool,
    /// `--gpu`: the systems whose row loops run on a compute device, and the function instances
    /// (function, archetype, view) those loops reach (compiled for the device too)
    pub(crate) gpu_systems: BTreeSet<usize>,
    pub(crate) device_funcs: BTreeSet<(FuncId, Option<ClassId>, bool)>,
    /// functions that may allocate from the frame arena (directly or through what they call)
    pub(crate) alloc_funcs: BTreeSet<FuncId>,
    /// the C spelling of each class name, unique among classes (a generic instance `Pool<int>` is
    /// spelled `Pool_x3c_int_x3e_`; a declared class of that name keeps it, the instance gets its id)
    pub(crate) cnames: Vec<String>,
    /// how the program runs on several shards (src/shard.rs)
    pub(crate) shard: crate::shard::ShardPlan,
    /// shared method code: the groups, and the group of each (method, archetype) instance
    pub(crate) groups: Vec<ShareGroup>,
    pub(crate) share: HashMap<(FuncId, ClassId), usize>,
}

/// (kind, precision) of a validated C# standard format string: kind is the C character code of
/// 'D' 'X' 'x' 'F' 'N' 'E' 'e' (0 for the default "G"/"R"), precision -1 when not given
pub fn fmt_spec(s: &str) -> (u32, i32) {
    let mut cs = s.chars();
    let k = cs.next().unwrap_or('G');
    let digits: String = cs.collect();
    let prec = if digits.is_empty() { -1 } else { digits.parse().unwrap_or(-1) };
    match k {
        'd' | 'D' => ('D' as u32, prec),
        'X' | 'x' | 'E' | 'e' => (k as u32, prec),
        'f' | 'F' => ('F' as u32, prec),
        'n' | 'N' => ('N' as u32, prec),
        _ => (0, -1),
    }
}

/// a range type's contract (in lo..hi) applied to a constant (field defaults): the same saturation as
/// sijo_clampi / sijo_clamprf at run time, members of a struct one level deep (like clamp_code)
pub fn clamp_const(v: ConstVal, lo: f64, hi: f64) -> ConstVal {
    match v {
        ConstVal::Int(x) => ConstVal::Int((x as i64).clamp(lo.ceil() as i64, hi.floor() as i64) as i32),
        ConstVal::Float(x) => ConstVal::Float(if x.is_nan() { lo as f32 } else { (x as f64).clamp(lo, hi) as f32 }),
        ConstVal::Struct(s, vals) => ConstVal::Struct(
            s,
            vals.into_iter()
                .map(|m| match m {
                    ConstVal::Int(_) | ConstVal::Float(_) => clamp_const(m, lo, hi),
                    other => other,
                })
                .collect(),
        ),
        other => other,
    }
}

/// runtime helper doing wrapping (C# unchecked) int arithmetic for `+ - *`
fn wrap_fn(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "sijo_add32",
        BinOp::Sub => "sijo_sub32",
        _ => "sijo_mul32",
    }
}

fn fnv32(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// what shares method code across tables (see ShareGroup), for `sijoc explain`
pub fn shared_code(c: &Compiled) -> Vec<String> {
    Gen::new(c).groups.iter().map(|g| g.what.clone()).collect()
}

impl<'a> Gen<'a> {
    fn new(c: &'a Compiled) -> Gen<'a> {
        let p = &c.prog;
        let arch_id = c.ecs.archetypes.iter().enumerate().map(|(i, &a)| (a, i)).collect();
        let mut hazard_comps = BTreeSet::new();
        for s in &c.ecs.systems {
            hazard_comps.extend(s.hazards.iter().copied());
        }
        let mut deferred_funcs = BTreeSet::new();
        for f in &p.funcs {
            crate::sema::visit_block(&f.body, &mut |e| {
                if let ExprKind::Call { func, dispatch: Dispatch::RemoteDeferred | Dispatch::Rpc, .. } = &e.kind {
                    deferred_funcs.insert(*func);
                }
            });
        }
        let mut rng_keys = HashMap::new();
        for (fid, f) in p.funcs.iter().enumerate() {
            let base = fnv32(&p.func_label(fid));
            let mut n: u32 = 0;
            crate::sema::visit_block(&f.body, &mut |e| {
                if let ExprKind::Builtin { f: Builtin::RandomFloat | Builtin::RandomInt, site, .. } = &e.kind {
                    rng_keys.insert(*site, base ^ n.wrapping_mul(0x9E37_79B9));
                    n += 1;
                }
            });
        }
        let mut grid_ids = BTreeMap::new();
        for (li, l) in c.spatial.loops.iter().enumerate() {
            for a in p.query_archetypes(l.class, &l.with) {
                if c.ecs.archetypes.contains(&a) {
                    let n = grid_ids.len();
                    grid_ids.insert((li, a), n);
                }
            }
        }
        let alloc_funcs = alloc_funcs(p);
        // --gpu: every GPU candidate (docs/07 §7.4) and what its row loop calls
        let mut gpu_systems = BTreeSet::new();
        let mut device_funcs = BTreeSet::new();
        if crate::consteval::GPU.load(std::sync::atomic::Ordering::Relaxed) {
            for (i, s) in c.ecs.systems.iter().enumerate() {
                if crate::ecs::gpu_candidate(p, &c.ecs, i).is_err() {
                    continue;
                }
                gpu_systems.insert(i);
                for &a in &s.archetypes {
                    for g in crate::ecs::reachable_funcs(p, s.func) {
                        match p.funcs[g].kind {
                            FuncKind::EntityMethod(_) => {
                                let t = p.target_of(a, g);
                                device_funcs.insert((t, Some(a), false));
                                device_funcs.insert((t, Some(a), true));
                            }
                            _ => {
                                device_funcs.insert((g, None, false));
                            }
                        }
                    }
                    device_funcs.insert((p.target_of(a, s.func), Some(a), false));
                }
            }
        }
        let mut cnames: Vec<String> = vec![];
        let mut order: Vec<ClassId> = (0..p.classes.len()).collect();
        // declared names first: an instance's spelling gives way to a class that is called so
        order.sort_by_key(|&c| p.generic_of.contains_key(&c));
        let mut taken: BTreeMap<String, ClassId> = BTreeMap::new();
        let mut by_id = vec![String::new(); p.classes.len()];
        for c in order {
            let mut n = mangle(&p.classes[c].name);
            while taken.contains_key(&n) {
                n = format!("{}_{}", n, c);
            }
            taken.insert(n.clone(), c);
            by_id[c] = n;
        }
        cnames.extend(by_id);
        let mut g = Gen { c, p, out: String::new(), tmp: 0, arch_id, hazard_comps, deferred_funcs, rng_keys, grid_ids, sm: None, cfile: "sijo_generated.c".into(), last_pos: None, lib: false, prefix: DEFAULT_LIB_PREFIX.into(), checked: crate::consteval::CHECKED.load(std::sync::atomic::Ordering::Relaxed), field_counts: crate::consteval::FIELD_COUNTS.load(std::sync::atomic::Ordering::Relaxed), gpu_systems, device_funcs, alloc_funcs, cnames, shard: crate::shard::plan(c), groups: vec![], share: HashMap::new() };
        g.plan_sharing();
        g
    }

    /// Which method instances share their code (see ShareGroup): a group when every instance of
    /// every method in it compiles to the same code on each member table (the same effects, override
    /// and row cache; not compiled for a compute device)
    fn plan_sharing(&mut self) {
        let archs = self.c.ecs.archetypes.clone();
        let mut cands: Vec<(String, Vec<ClassId>, Vec<usize>, Vec<FuncId>)> = vec![];
        let methods_of = |p: &Program, owner: &dyn Fn(ClassId) -> bool| -> Vec<FuncId> {
            (0..p.funcs.len()).filter(|&f| p.funcs[f].extern_name.is_none() && matches!(p.funcs[f].kind, FuncKind::EntityMethod(c) if owner(c))).collect()
        };
        // the class methods of a class with combination tables
        for &r in &archs {
            if self.p.classes[r].combo_of.is_some() {
                continue;
            }
            let members: Vec<ClassId> = self.p.family(r).into_iter().filter(|a| archs.contains(a)).collect();
            if members.len() < 2 {
                continue;
            }
            let p = self.p;
            let funcs = methods_of(p, &|c| !p.classes[c].is_component && p.is_subclass(r, c));
            let what = format!("the methods of {} on its {} tables", p.classes[r].name, members.len());
            cands.push((what, members, self.c.ecs.arch_comps[&r].clone(), funcs));
        }
        // the methods of a component on every table that has it
        for k in 0..self.p.classes.len() {
            if !self.p.classes[k].is_component {
                continue;
            }
            let members: Vec<ClassId> = self.p.archetypes_of(k).into_iter().filter(|a| archs.contains(a)).collect();
            if members.len() < 2 {
                continue;
            }
            let clusters: Vec<usize> = (0..self.c.ecs.components.len()).filter(|&i| self.c.ecs.components[i].class == k).collect();
            let funcs = methods_of(self.p, &|c| c == k);
            let what = format!("the methods of component {} on its {} tables", self.p.classes[k].name, members.len());
            cands.push((what, members, clusters, funcs));
        }
        for (what, members, clusters, funcs) in cands {
            if funcs.is_empty() {
                continue;
            }
            let rep = members[0];
            let same = funcs.iter().all(|&f| {
                let (e0, t0) = (self.c.info.get(f, Some(rep)), self.p.target_of(rep, f));
                let c0 = self.row_cache_fields(f, Some(rep), false);
                members.iter().all(|&m| {
                    self.c.info.get(f, Some(m)) == e0
                        && self.p.target_of(m, f) == t0
                        && self.row_cache_fields(f, Some(m), false) == c0
                        && !self.device_funcs.contains(&(f, Some(m), false))
                        && !self.device_funcs.contains(&(f, Some(m), true))
                })
            });
            if !same {
                continue;
            }
            let g = self.groups.len();
            for &f in &funcs {
                for &m in &members {
                    self.share.insert((f, m), g);
                }
            }
            self.groups.push(ShareGroup { what, rep, members, clusters });
        }
    }

    /// the table of `this` in the code being generated: the view of a shared method, or the table
    fn self_tab(&self, cx: &FCtx, a: ClassId) -> String {
        if cx.shared.is_some() {
            "V->".to_string()
        } else {
            format!("w->{}.", self.aname(a))
        }
    }

    /// the arguments before a method's own: `w`, the view (shared code), the row. `inview`: the
    /// caller's own view `V` serves (a call on `this` from shared code)
    fn inst_args(&self, f: FuncId, a: ClassId, row: &str, inview: bool) -> Vec<String> {
        match self.share.get(&(f, a)) {
            Some(_) if inview => vec!["w".into(), "V".into(), row.to_string()],
            Some(&g) => vec!["w".into(), format!("(SijoView_{g}[1]){{ sijo_view{g}_{}(w) }}", self.aname(a)), row.to_string()],
            None => vec!["w".into(), row.to_string()],
        }
    }

    /// `SIJO_AT(e)` when `e` can trap only in --checked builds (overflow): the location is free otherwise
    fn at(&self, e: String) -> String {
        if self.checked {
            format!("SIJO_AT({})", e)
        } else {
            e
        }
    }

    pub(crate) fn t(&mut self) -> String {
        self.tmp += 1;
        format!("_t{}", self.tmp)
    }

    pub(crate) fn w(&mut self, s: &str) {
        self.out.push_str(s);
    }

    // ------------------------------------------------------------------
    // names & types
    // ------------------------------------------------------------------

    /// user structs live in their own C namespace (`Nu_`), so they cannot clash with C or runtime names
    pub(crate) fn sname(&self, s: StructId) -> String {
        format!("Nu_{}", mangle(&self.p.structs[s].name))
    }
    /// archetype tables are world members `a_<Class>`; their pending tables are `pa_<Class>`
    pub(crate) fn aname(&self, a: ClassId) -> String {
        format!("a_{}", self.cnames[a])
    }
    /// the C struct of component `k` (generic class instances spell `<`, `,` and `>` out)
    pub(crate) fn ccomp(&self, k: usize) -> String {
        let n = &self.c.ecs.components[k].cname;
        if n.chars().any(|c| c.is_ascii() && !c.is_ascii_alphanumeric() && c != '_') {
            format!("C{}_{}", k, mangle(n))
        } else {
            format!("C{}_{}", k, n)
        }
    }
    pub(crate) fn fname(&self, f: FieldId) -> String {
        cid(&self.p.fields[f].name)
    }
    pub(crate) fn comp(&self, f: FieldId) -> usize {
        self.c.ecs.comp_of(f).expect("field without component")
    }
    pub(crate) fn cty(&self, t: &Ty) -> String {
        match t {
            Ty::Void => "void".into(),
            Ty::Bool => "bool".into(),
            Ty::Int | Ty::Enum(_) | Ty::Char => "int32_t".into(),
            Ty::Float => "float".into(),
            Ty::Struct(s) => self.sname(*s),
            Ty::Handle(_) | Ty::NHandle(_) | Ty::Null => "SijoH".into(),
            Ty::List(_) | Ty::Dict(..) | Ty::Array(_) | Ty::MdArray(..) => "SijoList".into(),
            Ty::Str => "SijoStr".into(),
            Ty::Delegate(d) => format!("SijoD{}", d),
            Ty::Error => "int32_t".into(),
        }
    }
    fn func_name(&self, f: FuncId) -> String {
        let func = &self.p.funcs[f];
        let name = match func.name.strip_prefix("operator") {
            Some(sym) => {
                let n = match sym {
                    "+" => "add", "-" => "sub", "*" => "mul", "/" => "div", "%" => "rem", "==" => "eq", "!=" => "ne",
                    "<" => "lt", ">" => "gt", "<=" => "le", ">=" => "ge", "!" => "not", "~" => "bnot", "&" => "and",
                    "|" => "or", "^" => "xor", _ => "x",
                };
                // unary and binary minus/plus share a symbol: disambiguate by arity
                format!("op_{}{}", n, func.params.len())
            }
            None => cid(&func.name),
        };
        format!("F{}_{}_{}", f, cid(func.owner_name(self.p)), name)
    }
    pub(crate) fn method_name(&self, f: FuncId, a: ClassId, view: bool) -> String {
        let func = &self.p.funcs[f];
        match self.share.get(&(f, a)) {
            Some(&g) => format!("M{}_{}_{}__V{}{}", f, cid(func.owner_name(self.p)), cid(&func.name), g, if view { "_v" } else { "" }),
            None => format!("M{}_{}_{}__{}{}", f, cid(func.owner_name(self.p)), cid(&func.name), self.aname(a), if view { "_v" } else { "" }),
        }
    }
    fn local_name(&self, f: FuncId, l: LocalId) -> String {
        format!("v{}_{}", l, cid(&self.p.funcs[f].locals[l].name).replace("__", "_"))
    }
    fn is_mutating_method(&self, f: FuncId) -> bool {
        match self.p.funcs[f].kind {
            FuncKind::EntityMethod(c) => {
                let archs = self.p.archetypes_of(c);
                if archs.is_empty() {
                    return self.c.info.get(f, Some(c)).mutates();
                }
                archs.iter().any(|&a| self.c.info.get(f, Some(a)).mutates())
            }
            _ => self.c.info.get(f, None).mutates(),
        }
    }
    fn mask(&self, c: ClassId) -> String {
        let mut m: u64 = 0;
        for a in self.p.archetypes_of(c) {
            m |= 1u64 << self.arch_id[&a];
        }
        format!("0x{:x}ull", m)
    }

    fn flit(f: f64) -> String {
        let x = f as f32;
        if x.is_nan() {
            "NAN".into()
        } else if x.is_infinite() {
            if x > 0.0 { "INFINITY".into() } else { "(-INFINITY)".into() }
        } else {
            let s = format!("{:?}", x);
            if x < 0.0 {
                format!("({}f)", s)
            } else {
                format!("{}f", s)
            }
        }
    }
    fn ilit(n: i64) -> String {
        let n = n as i32;
        if n == i32::MIN {
            "INT32_MIN".into()
        } else if n < 0 {
            format!("({})", n)
        } else {
            n.to_string()
        }
    }

    fn cval(&self, v: &ConstVal) -> String {
        match v {
            ConstVal::Int(n) => Self::ilit(*n as i64),
            ConstVal::Float(f) => Self::flit(*f as f64),
            ConstVal::Bool(b) => b.to_string(),
            ConstVal::Enum(_, x) => Self::ilit(*x),
            ConstVal::Null => "SIJO_NULL".into(),
            ConstVal::Str(s) => Self::str_lit(s),
            ConstVal::Struct(s, vals) => {
                let parts: Vec<String> = vals.iter().map(|v| self.cinit(v)).collect();
                format!("(({}){{{}}})", self.sname(*s), parts.join(", "))
            }
            ConstVal::Fn(d, n) => format!("((SijoD{}){{ .fn = {} }})", d, n),
        }
    }
    /// C type of a container's element (a dictionary allocation site stores its own type)
    pub(crate) fn elem_cty(&self, t: &Ty) -> String {
        match self.p.kvp_of(t) {
            Some(kvp) => self.sname(kvp),
            None => self.cty(t),
        }
    }
    /// (key kind, key C type, entry C type) of a dictionary type
    pub(crate) fn dict_info(&self, t: &Ty) -> (&'static str, String, String) {
        let Ty::Dict(k, _) = t else { return ("SIJO_KEY_I32", "int32_t".into(), "int32_t".into()) };
        let kind = match **k {
            Ty::Str => "SIJO_KEY_STR",
            Ty::Handle(_) | Ty::NHandle(_) => "SIJO_KEY_H",
            _ => "SIJO_KEY_I32",
        };
        (kind, self.cty(k), self.elem_cty(t))
    }
    fn str_lit(s: &str) -> String {
        if s.is_ascii() {
            format!("sijo_str_lita({}, {})", c_str(s), s.len())
        } else {
            format!("sijo_str_lit({}, {})", c_str(s), s.len())
        }
    }
    /// brace initializer usable in static declarations
    fn cinit(&self, v: &ConstVal) -> String {
        match v {
            ConstVal::Struct(_, vals) => {
                let parts: Vec<String> = vals.iter().map(|v| self.cinit(v)).collect();
                format!("{{{}}}", parts.join(", "))
            }
            ConstVal::Null => "{0, 0}".into(),
            ConstVal::Str(s) => format!("{{{}, {}, {}}}", c_str(s), s.len(), if s.is_ascii() { 4 } else { 0 }),
            ConstVal::Fn(_, n) => format!("{{ {} }}", n),
            other => self.cval(other),
        }
    }

    fn default_of(&self, t: &Ty) -> String {
        match t {
            Ty::Struct(s) => match consteval::struct_default(self.p, *s) {
                Ok(v) => self.cval(&v),
                Err(_) => format!("(({}){{0}})", self.sname(*s)),
            },
            Ty::Float => "0.0f".into(),
            Ty::Bool => "false".into(),
            Ty::Handle(_) | Ty::NHandle(_) => "SIJO_NULL".into(),
            Ty::List(_) | Ty::Dict(..) | Ty::Array(_) | Ty::MdArray(..) => "((SijoList){0})".into(),
            Ty::Str => "SIJO_STR_EMPTY".into(),
            Ty::Delegate(d) => format!("((SijoD{}){{0}})", d),
            _ => "0".into(),
        }
    }

    fn field_default(&self, f: FieldId) -> String {
        let fd = &self.p.fields[f];
        if let (Ty::Array(el), Some(Expr { kind: ExprKind::ArrayNew { len, elems, .. }, .. })) = (&fd.ty, &fd.init) {
            // T[] F = new T[n] / { a, b }: a heap array (checked to be constant by the type checker)
            let et = self.cty(el);
            let n = match len {
                Some(l) => match consteval::eval_expr(self.p, l) {
                    Ok(ConstVal::Int(n)) => n.max(0) as usize,
                    _ => 0,
                },
                None => elems.len(),
            };
            let vals: Vec<String> = if len.is_some() {
                vec![self.default_of(el); n]
            } else {
                elems.iter().map(|x| consteval::eval_expr(self.p, x).map(|v| self.cval(&v)).unwrap_or_else(|_| self.default_of(el))).collect()
            };
            let fills: String = vals.iter().enumerate().map(|(i, v)| format!("(({et}*)L_.data)[{i}] = {v}; ")).collect();
            return format!("({{ SijoList L_ = sijo_list_new(NULL, 1, (int32_t)sizeof({et}), {n}); L_.len = {n}; {fills}L_; }})");
        }
        if let (Ty::MdArray(el, _), Some(Expr { kind: ExprKind::ArrayNew { elems, dims, .. }, .. })) = (&fd.ty, &fd.init) {
            // T[,] F = new T[w, h] / { { a, b }, { c, d } }: a heap array (constant lengths and elements)
            let et = self.cty(el);
            let mut ds: Vec<i64> = dims
                .iter()
                .map(|d| match consteval::eval_expr(self.p, d) {
                    Ok(ConstVal::Int(n)) => n.max(0) as i64,
                    _ => 0,
                })
                .collect();
            ds.resize(3, 1);
            let zero = matches!(**el, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::NHandle(_) | Ty::Handle(_));
            let fills: String = if !elems.is_empty() {
                let vals = elems.iter().map(|x| consteval::eval_expr(self.p, x).map(|v| self.cval(&v)).unwrap_or_else(|_| self.default_of(el)));
                vals.enumerate().map(|(i, v)| format!("(({et}*)L_.data)[{i}] = {v}; ")).collect()
            } else if zero {
                String::new()
            } else {
                format!("{{ {et} d_ = {}; for (int32_t i_ = 0; i_ < L_.len; i_++) (({et}*)L_.data)[i_] = d_; }} ", self.default_of(el))
            };
            return format!("({{ SijoList L_ = sijo_md_new(NULL, 1, (int32_t)sizeof({et}), {}, {}, {}); {fills}L_; }})", ds[0], ds[1], ds[2]);
        }
        if fd.ty.is_list() {
            // entity-owned lists always live on the heap (they outlive the tick)
            return "((SijoList){NULL, 0, 0, 1})".into();
        }
        match &fd.init {
            Some(e) => match consteval::eval_expr(self.p, e) {
                Ok(v) => match self.c.net.clamp.get(&f) {
                    Some(&(lo, hi)) => self.cval(&clamp_const(v, lo, hi)),
                    None => self.cval(&v),
                },
                Err(_) => self.default_of(&fd.ty),
            },
            // no initializer: the zero value, saturated like a written default (W0202 says so)
            None => match (self.c.net.clamp.get(&f), consteval::implicit_default(self.p, &fd.ty)) {
                (Some(&(lo, hi)), Some(v)) => self.cval(&clamp_const(v, lo, hi)),
                _ => self.default_of(&fd.ty),
            },
        }
    }

    // ------------------------------------------------------------------
    // program skeleton
    // ------------------------------------------------------------------

    fn program(&mut self, path: &str) {
        let p = self.p;
        let _ = writeln!(self.out, "/* Generated by sijoc {} from {} — do not edit. */", env!("CARGO_PKG_VERSION"), path);
        self.w(DIAG_PUSH);
        if self.checked {
            self.w("#define SIJO_CHECKED 1 /* sijoc build --checked: integer overflow traps */\n");
        }
        self.w("/* Build: gcc -std=gnu11 -O2 -ffp-contract=off -fopenmp prog.c -lm  (or clang; see runtime notes below) */\n");
        self.w(RUNTIME);
        self.w(&crate::uchar::c_tables());
        self.w(STR_RUNTIME);
        self.w(DICT_RUNTIME);
        self.w(GRID_RUNTIME);
        if self.field_counts {
            let _ = writeln!(self.out, "/* sijoc build --profile-fields: every entity field access counts (per thread), written to --profile FILE */\nstatic uint64_t sijo_fcount[SIJO_MAX_THREADS][{}];\n#define SIJO_FC(f) (sijo_fcount[sijo_tid()][(f)]++)", self.p.fields.len().max(1));
        }
        if !self.gpu_systems.is_empty() {
            self.w("/* sijoc build --gpu: tables at least this large run their pure self-write systems on the device */\n#ifndef SIJO_GPU_MIN\n#define SIJO_GPU_MIN 4096\n#endif\n");
        }
        self.w("\n/* ======================= program types ======================= */\n");
        // enums
        for e in &p.enums {
            let _ = writeln!(self.out, "static const char* sijo_enum_{}(int32_t v) {{ switch (v) {{", cid(&e.name));
            // several names for one value: the first declared one, like .NET's ToString
            let mut seen = std::collections::HashSet::new();
            for (n, v) in &e.variants {
                if seen.insert(*v) {
                    let _ = writeln!(self.out, "  case {}: return \"{}\";", v, n);
                }
            }
            self.w("  default: { static SIJO_TLS char b[16]; sijo_fmt_i32(b, v, 0, -1); return b; } } }\n");
        }
        // structs and delegates (topological order: a value type before the types that hold it)
        for node in self.type_order() {
            match node {
                TypeNode::Struct(s) => {
                    let _ = writeln!(self.out, "typedef struct {} {{", self.sname(s));
                    for f in &p.structs[s].fields {
                        let _ = writeln!(self.out, "  {} {};", self.cty(&f.ty), cid(&f.name));
                    }
                    if p.structs[s].fields.is_empty() {
                        self.w("  uint8_t _empty;\n");
                    }
                    let _ = writeln!(self.out, "}} {};", self.sname(s));
                }
                TypeNode::Delegate(d) => {
                    // the function number (0: the default value, which does nothing) and the values
                    // the function captured, one struct per function that captures something
                    let _ = writeln!(self.out, "typedef struct SijoD{d} {{ /* {} */\n  int32_t fn;\n  union {{\n    uint8_t none_;", p.delegate_name(d));
                    for (k, &f) in p.delegates[d].funcs.iter().enumerate() {
                        let n = p.caps_of(f);
                        if n == 0 {
                            continue;
                        }
                        let fields: Vec<String> = (0..n).map(|j| format!("{} c{};", self.cap_cty(&p.funcs[f].locals[p.funcs[f].params[j]].ty), j)).collect();
                        let _ = writeln!(self.out, "    struct {{ {} }} f{}; /* {} */", fields.join(" "), k + 1, p.func_label(f));
                    }
                    let _ = writeln!(self.out, "  }} env;\n}} SijoD{d};");
                }
            }
        }
        // constants
        for (i, k) in p.consts.iter().enumerate() {
            if let Some(v) = &k.value {
                let _ = writeln!(self.out, "static const {} K{}_{} = {};", self.cty(&k.ty), i, cid(&k.name), self.cinit(v));
            }
        }
        // components
        self.w("\n/* ======================= components (planned by the compiler) ======================= */\n");
        for (k, comp) in self.c.ecs.components.iter().enumerate() {
            let _ = writeln!(self.out, "/* {}: {} */", comp.cname, comp.signature.join(", "));
            let _ = writeln!(self.out, "typedef struct {{");
            for &f in &comp.fields {
                let _ = writeln!(self.out, "  {} {};", self.cty(&p.fields[f].ty), self.fname(f));
            }
            let _ = writeln!(self.out, "}} {}; /* c{} */", self.ccomp(k), k);
        }
        // the values an Add(new C { ... }) carries to the end of the system (live fields of C)
        for k in self.component_classes() {
            let _ = writeln!(self.out, "typedef struct {{");
            let fields: Vec<FieldId> = self.comp_payload_fields(k).into_iter().filter(|&f| !self.packed_field(f)).collect();
            for &f in &fields {
                let _ = writeln!(self.out, "  {} {};", self.cty(&p.fields[f].ty), self.fname(f));
            }
            if fields.is_empty() {
                self.w("  uint8_t _empty;\n");
            }
            let _ = writeln!(self.out, "}} SijoAdd_{}; /* component {} */", mangle(&p.classes[k].name), p.classes[k].name);
        }
        // [Persistent]: one record per key (the values the store keeps), an open-addressing index
        for (i, (c, _, pers, _)) in self.p.persist.clone().iter().enumerate() {
            let _ = writeln!(self.out, "typedef struct {{ SijoStr key;");
            for &f in pers {
                let _ = writeln!(self.out, "  {} f{f}; /* {} */", self.cty(&p.fields[f].ty), p.field_label(f));
            }
            let _ = writeln!(self.out, "  uint8_t has[{}]; /* which values the record holds (a store from an older version may lack some) */", pers.len());
            let _ = writeln!(self.out, "}} SijoPRec{i}; /* {} */\ntypedef struct {{ SijoPRec{i}* r; int32_t n, cap; int32_t* ix; int32_t icap; int dirty; }} SijoPStore{i};", p.classes[*c].name);
        }
        // archetype tables
        self.w("\n/* ======================= archetype tables (SoA) ======================= */\n");
        for &a in &self.c.ecs.archetypes {
            let _ = writeln!(self.out, "#define ARCH_{} {}", self.aname(a), self.arch_id[&a]);
        }
        let _ = writeln!(self.out, "#define SIJO_NARCH {}", self.c.ecs.archetypes.len().max(1));
        for &a in &self.c.ecs.archetypes.clone() {
            // pcap: rows allocated in the hazard snapshots (main tables only, sized on first use);
            // wmax/wn: largest use in the current window of flushes (capacity trimming)
            let _ = writeln!(self.out, "typedef struct {{\n  int32_t count, cap, pcap, wmax, wn;\n  SijoH* ent;");
            for &k in &self.c.ecs.arch_comps[&a].clone() {
                let cn = &self.ccomp(k);
                let _ = writeln!(self.out, "  {cn}* c{k}; {cn}* c{k}_read;");
                if self.hazard_comps.contains(&k) {
                    let _ = writeln!(self.out, "  {cn}* c{k}_prev;");
                }
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "  uint8_t* d{k};");
                }
            }
            let _ = writeln!(self.out, "}} SijoArch_{};", self.aname(a));
        }
        // shared method code reaches its entity's columns through a view of the table
        for (g, grp) in self.groups.clone().iter().enumerate() {
            let _ = writeln!(self.out, "typedef struct {{ /* {} */\n  SijoH* ent; int arch;", grp.what);
            for &k in &grp.clusters {
                let cn = self.ccomp(k);
                let _ = writeln!(self.out, "  {cn}* c{k}; {cn}* c{k}_read;");
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "  uint8_t* d{k};");
                }
            }
            let _ = writeln!(self.out, "}} SijoView_{g};");
        }
        // world
        let _ = writeln!(self.out, "#define SIJO_NGRID {}", self.grid_ids.len().max(1));
        self.w("\ntypedef struct SijoWorld {\n  SijoSlots ents;\n");
        for &a in &self.c.ecs.archetypes {
            let n = self.aname(a);
            let _ = writeln!(self.out, "  SijoArch_{n} {n}; SijoArch_{n} p{n}; SijoArch_{n} pt{n}[SIJO_MAX_THREADS];");
        }
        self.w(
            "  SijoCmdBuf cmd[SIJO_MAX_THREADS];
  SijoArena frame[SIJO_MAX_THREADS];
  SijoTls tls[SIJO_MAX_THREADS]; /* per-thread entity context, one cache line each */
  int tspawn; /* a parallel system is running: Spawn goes to per-thread tables (pt*) */
  SijoH* dlist; int32_t dn, dcap;
  int32_t tick; float dt; uint64_t seed;
  int is_client, quiet, nthreads;
  SijoDrawList draws;
  /* distributed simulation (runtime/sijo_shard.h); all zero in a single world. sh_on: this world is
     one shard of several; the owner of each slot is ents.shard[slot] */
  struct SijoShard* sh; int sh_on, sh_id, sh_cap, sh_sys, nshards; uint8_t* sh_hv;
  int64_t cmds_applied;
  int64_t spawned, destroyed;
  SijoSHeap sheap; /* strings that survived a tick (see sijo_gc) */
  SijoSTab stab; /* replicated strings: server ids, client id -> text (runtime/sijo_str.h) */
  int view_on; float view_x, view_y, view_r; /* client: Net.View (interest management) */
  SijoGrid grid[SIJO_NGRID]; /* proximity-query grids (see src/spatial.rs) */
  SijoPanic panic; /* embedding API: the panic that stopped this world */
",
        );
        let nsys = self.c.ecs.systems.len().max(1);
        let nst = self.c.ecs.stages.len().max(1);
        if self.has_comps() {
            self.w("  int32_t* clist; int32_t cn, ccap; /* Add/Remove commands of the current flush (indices into cmd[0]) */\n");
        }
        for i in 0..self.p.persist.len() {
            let _ = writeln!(self.out, "  SijoPStore{i} pst{i}; /* [Persistent] store of {} (--store) */", self.p.classes[self.p.persist[i].0].name);
        }
        if !self.p.persist.is_empty() {
            self.w("  int pst_open, pst_writer; /* the store was read; this world writes it (not a client, shard 0) */\n");
        }
        let _ = writeln!(self.out, "  int prof; /* --profile: time every system (sijo_tick_prof) */\n  uint64_t prof_ns[{nsys}], prof_rows[{nsys}], prof_calls[{nsys}], prof_apply[{nst}];\n}} SijoWorld;");
        if self.has_comps() {
            self.w(
                r#"/* components: Add(new C { ... }) carries the component's values; both apply at the end of the system */
static void sijo_send_cadd(SijoWorld* w, SijoH h, uint16_t comp, const void* p, uint16_t size) {
  int t = sijo_tid(); SijoCmdBuf* b = &w->cmd[t];
  uint32_t d = sijo_cmd_data(b, p, size);
  SijoCmd* c = sijo_cmd_push(b);
  c->kind = SIJO_CMD_CADD; c->target = h; c->src = w->tls[t].cur; c->field = comp; c->size = size; c->data = d;
}
static void sijo_send_crem(SijoWorld* w, SijoH h, uint16_t comp) {
  int t = sijo_tid(); SijoCmd* c = sijo_cmd_push(&w->cmd[t]);
  c->kind = SIJO_CMD_CREM; c->target = h; c->src = w->tls[t].cur; c->field = comp;
}
"#,
            );
        }
        // the shard runtime's types and hooks (the per-program half comes after the systems)
        self.w(SHARD_RUNTIME.split(SHARD_SPLIT).next().unwrap_or(""));
        // generic senders
        self.w(
            r#"
static void sijo_send_destroy(SijoWorld* w, SijoH h) {
  int t = sijo_tid(); SijoCmd* c = sijo_cmd_push(&w->cmd[t]);
  c->kind = SIJO_CMD_DESTROY; c->target = h; c->src = w->tls[t].cur;
}
static void sijo_send_addi(SijoWorld* w, SijoH h, uint16_t field, uint16_t off, int32_t v) {
  int t = sijo_tid(); SijoCmd* c = sijo_cmd_push(&w->cmd[t]);
  c->kind = SIJO_CMD_ADDI; c->target = h; c->src = w->tls[t].cur; c->field = field; c->off = off; c->v.i = v;
}
static void sijo_send_addf(SijoWorld* w, SijoH h, uint16_t field, uint16_t off, float v) {
  int t = sijo_tid(); SijoCmd* c = sijo_cmd_push(&w->cmd[t]);
  c->kind = SIJO_CMD_ADDF; c->target = h; c->src = w->tls[t].cur; c->field = field; c->off = off; c->v.f = v;
}
static void sijo_send_set(SijoWorld* w, SijoH h, uint16_t field, uint16_t off, const void* p, uint16_t size) {
  int t = sijo_tid(); SijoCmdBuf* b = &w->cmd[t];
  uint32_t d = sijo_cmd_data(b, p, size);
  SijoCmd* c = sijo_cmd_push(b);
  c->kind = SIJO_CMD_SET; c->target = h; c->src = w->tls[t].cur; c->field = field; c->off = off; c->size = size; c->data = d;
}
static inline float sijo_randf(SijoWorld* w, uint32_t site, float lo, float hi) {
  int t = sijo_tid();
  uint64_t k = sijo_rng_key(w->seed, w->tick, w->tls[t].rng_ent, site, w->tls[t].rng_ctr++);
  return lo + (hi - lo) * sijo_rand01(k);
}
static inline int32_t sijo_randi(SijoWorld* w, uint32_t site, int32_t lo, int32_t hi) {
  int t = sijo_tid();
  uint64_t k = sijo_rng_key(w->seed, w->tick, w->tls[t].rng_ent, site, w->tls[t].rng_ctr++);
  if (hi <= lo) return lo;
  return (int32_t)((int64_t)lo + (int64_t)(k % (uint64_t)((int64_t)hi - (int64_t)lo)));
}
static inline uint64_t sijo_ekey(SijoH h) { return ((uint64_t)h.gen << 32) | h.idx; }
static inline bool sijo_is(SijoWorld* w, SijoH h, uint64_t mask) {
  SijoSlot* s = sijo_live(&w->ents, h);
  return s && ((mask >> s->arch) & 1);
}
/* entity references in snapshots (SIJO_SRVH, sijo_hmark): the server's value and, on a client, the
   server slot its handle stands for */
static inline uint32_t sijo_hq(SijoWorld* w, SijoH h, uint64_t mask) {
  SijoSlot* s = sijo_live(&w->ents, h);
  return (s && ((mask >> s->arch) & 1)) ? ((h.idx & 0xffffffu) | ((h.gen & 0xffu) << 24)) : 0u;
}
static inline uint32_t sijo_hv(SijoWorld* c, const SijoCMap* m, SijoH h) {
  if (!sijo_live(&c->ents, h) || h.idx >= (uint32_t)m->rcap) return 0u;
  uint32_t si = m->rmap[h.idx];
  return (si && si < (uint32_t)m->cap) ? (si | ((m->sgen[si] & 0xffu) << 24)) : 0u;
}
/* the row of a live entity of archetype `arch`, -1 otherwise (rows do not move during a system) */
static inline int32_t sijo_hrow(SijoWorld* w, SijoH h, int arch) {
  SijoSlot* s = sijo_live(&w->ents, h);
  return (s && s->arch == arch) ? s->row : -1;
}
"#,
        );
        self.prototypes();
        self.table_ops();
        self.accessors();
        self.field_apply();
        self.w("\n/* ======================= user code ======================= */\n");
        self.user_functions();
        self.deferred_plumbing();
        self.grids();
        self.systems();
        self.replication();
        self.shard_section();
        self.checksum();
        self.gc_roots();
        self.state_io();
        self.main_fn();
    }

    /// `.X.Y` member access path inside a field of type `t`
    pub(crate) fn member_path(&self, t: &Ty, path: &[usize]) -> String {
        let mut o = String::new();
        let mut cur = t.clone();
        for &i in path {
            if let Ty::Struct(s) = cur {
                o.push('.');
                o.push_str(&cid(&self.p.structs[s].fields[i].name));
                cur = self.p.structs[s].fields[i].ty.clone();
            }
        }
        o
    }

    /// one build function per (proximity loop, archetype): bucket every row by its cell
    fn grids(&mut self) {
        if self.grid_ids.is_empty() {
            return;
        }
        self.w("\n/* ======================= proximity-query grids ======================= */\n");
        for (&(li, a), &gid) in &self.grid_ids.clone() {
            let l = self.c.spatial.loops[li].clone();
            let n = self.aname(a);
            let k = self.comp(l.other_field);
            let fty = self.p.fields[l.other_field].ty.clone();
            let base = format!("w->{n}.c{k}_read[r].{}", self.fname(l.other_field));
            let x = format!("{base}{}", self.member_path(&fty, &l.other_paths[0]));
            let y = match l.other_paths.get(1) {
                Some(pth) => format!("sijo_grid_cell(g, {base}{})", self.member_path(&fty, pth)),
                None => "0".into(),
            };
            let dims = l.other_paths.len();
            // a per-entity radius: the cell edge from the largest squared radius among the entities
            // that run the loop (their radius fields do not change while the system runs)
            let cell = if l.dyn_r2.is_empty() {
                format!("{}f", Self::flit(l.cell as f64).trim_end_matches('f'))
            } else {
                let qc = match self.p.funcs[l.func].kind {
                    FuncKind::EntityMethod(c) => c,
                    _ => l.class,
                };
                let mut code = String::from("  double r2_ = 0.0;\n");
                for qa in self.p.archetypes_of(qc) {
                    if !self.c.ecs.archetypes.contains(&qa) {
                        continue;
                    }
                    let qn = self.aname(qa);
                    let terms: Vec<String> = l
                        .dyn_r2
                        .iter()
                        .map(|(coef, leaves)| {
                            let mut t = format!("{:?}", coef);
                            for (f, path) in leaves {
                                let fty = self.p.fields[*f].ty.clone();
                                let _ = write!(t, " * (double)w->{qn}.c{}[r].{}{}", self.comp(*f), self.fname(*f), self.member_path(&fty, path));
                            }
                            t
                        })
                        .collect();
                    let _ = writeln!(code, "  for (int32_t r = 0; r < w->{qn}.count; r++) {{ double v_ = {}; if (v_ > r2_) r2_ = v_; }}", terms.join(" + "));
                }
                code.push_str("  float cell_ = (float)(sqrt(r2_) * 1.001 + 1e-4); if (!(cell_ >= 1e-3f)) cell_ = 1e-3f;\n");
                let _ = write!(self.out, "/* per-entity radius of grid {gid} */\nstatic float sijo_grid_cell_{gid}(SijoWorld* w) {{\n{code}  return cell_;\n}}\n");
                format!("sijo_grid_cell_{gid}(w)")
            };
            // on a shard, rows whose halo copy is not valid here are left out of the grid (they are
            // farther than the radius from every entity this shard runs: src/shard.rs)
            let emit = if self.shard.halo_loops.contains(&li) {
                format!("{{ if (w->sh_hv && !w->sh_hv[w->{n}.ent[r].idx]) {{ g->cnt[r] = 0; continue; }} sijo_grid_emit(g, r, sijo_grid_cell(g, {x}), {y}, {dims}); }}")
            } else {
                format!("sijo_grid_emit(g, r, sijo_grid_cell(g, {x}), {y}, {dims});")
            };
            let _ = writeln!(
                self.out,
                "/* {}: Query<{}> within {:.3} */\nstatic void sijo_grid_build_{gid}(SijoWorld* w) {{\n  SijoGrid* g = &w->grid[{gid}]; int32_t n = w->{n}.count;\n  sijo_grid_begin(g, n, {cell});\n  if (!g->ok) return;\n  #pragma omp parallel for schedule(static) num_threads(w->nthreads) if (n >= SIJO_GRID_PAR)\n  for (int32_t r = 0; r < n; r++) {emit}\n  sijo_grid_end(g, w->nthreads);\n}}",
                self.p.func_label(l.func),
                self.p.classes[l.class].name,
                l.radius,
            );
        }
    }

    /// code visiting every string slot inside a value of type `ty` stored at `v`
    fn gc_visit(&self, ty: &Ty, v: &str, depth: usize) -> String {
        if let (Ty::Dict(..), Some(e)) = (ty, self.p.elem_of(ty)) {
            return self.gc_visit(&Ty::List(Box::new(e)), v, depth);
        }
        match ty {
            Ty::Str => format!("sijo_str_visit(&w->sheap, &({v}));"),
            Ty::Struct(s) => self.p.structs[*s]
                .fields
                .iter()
                .filter(|m| self.p.contains_str(&m.ty))
                .map(|m| self.gc_visit(&m.ty, &format!("({}).{}", v, cid(&m.name)), depth))
                .collect::<Vec<_>>()
                .join(" "),
            Ty::List(el) | Ty::Array(el) | Ty::MdArray(el, _) if self.p.contains_str(el) => {
                let et = self.cty(el);
                let inner = self.gc_visit(el, &format!("(({et}*)L{depth}->data)[i{depth}]"), depth + 1);
                format!("{{ SijoList* L{depth} = &({v}); for (int32_t i{depth} = 0; i{depth} < L{depth}->len; i{depth}++) {{ {inner} }} }}")
            }
            Ty::Delegate(d) if self.p.contains_str(ty) => format!("sijo_d{d}_gc(w, &({v}));"),
            _ => String::new(),
        }
    }

    /// The functions that walk delegate values of one kind of use (a delegate may hold delegates of
    /// its own type in boxes, so these are C functions that call each other, declared first)
    pub(crate) fn dg_helpers(&mut self, kind: &str) {
        if self.p.delegates.is_empty() {
            return;
        }
        let mut protos = String::new();
        let mut defs = String::new();
        for d in 0..self.p.delegates.len() {
            if self.p.delegates[d].invoke == usize::MAX {
                continue;
            }
            let v = "(*v)";
            let (sig, body) = match kind {
                "gc" => {
                    if !self.p.contains_str(&Ty::Delegate(d)) {
                        continue;
                    }
                    (format!("static void sijo_d{d}_gc(SijoWorld* w, SijoD{d}* v)"), self.dg_cases(d, v, DgMode::Gc, 0, &|t, x| if self.p.contains_str(t) { self.gc_visit(t, x, 1) } else { String::new() }))
                }
                "hash" => (format!("static uint64_t sijo_d{d}_hash(uint64_t h, const SijoD{d}* v)"), format!("h = sijo_fnv_u32(h, (uint32_t)v->fn); {} return h;", self.dg_cases(d, v, DgMode::Read, 0, &|t, x| self.hash_code(t, x, 1)))),
                "write" => {
                    let mut o = String::from("fprintf(f, \" {\"); switch (v->fn) { ");
                    for (k, &g) in self.p.delegates[d].funcs.iter().enumerate() {
                        let n = self.p.caps_of(g);
                        let label: String = self.dg_label(g).bytes().map(|b| format!("{:02x}", b)).collect();
                        let caps: Vec<String> = (0..n)
                            .map(|j| {
                                let t = &self.p.funcs[g].locals[self.p.funcs[g].params[j]].ty;
                                let place = format!("v->env.f{}.c{j}", k + 1);
                                if self.p.boxed_cap(t) {
                                    let (ct, u) = (self.cty(t), format!("u{j}"));
                                    format!("{{ {ct} {u}; memcpy(&{u}, ({place}).p, sizeof {u}); {} }}", self.write_code(t, &u, 1))
                                } else {
                                    self.write_code(t, &place, 1)
                                }
                            })
                            .collect();
                        let _ = write!(o, "case {}: fprintf(f, \" f{label}\"); {} break; ", k + 1, caps.join(" "));
                    }
                    o.push_str("default: fprintf(f, \" none\"); break; } fprintf(f, \" }\");");
                    (format!("static void sijo_d{d}_write(FILE* f, const SijoD{d}* v)"), o)
                }
                "read" => {
                    let mut o = String::from("memset(v, 0, sizeof *v); sijo_tok_expect(r, \"{\"); const char* L = sijo_tok(r); ");
                    let mut first = true;
                    for (k, &g) in self.p.delegates[d].funcs.iter().enumerate() {
                        let n = self.p.caps_of(g);
                        let label: String = self.dg_label(g).bytes().map(|b| format!("{:02x}", b)).collect();
                        let caps: Vec<String> = (0..n)
                            .map(|j| {
                                let t = &self.p.funcs[g].locals[self.p.funcs[g].params[j]].ty;
                                let cp = format!("v->env.f{}.c{j}", k + 1);
                                if self.p.boxed_cap(t) {
                                    let (ct, u) = (self.cty(t), format!("u{j}"));
                                    format!("{{ {ct} {u}; memset(&{u}, 0, sizeof {u}); {} {cp} = sijo_str_heap(&w->sheap, (const char*)&{u}, (int32_t)sizeof {u}); }}", self.read_code(t, &u, 1))
                                } else {
                                    self.read_code(t, &cp, 1)
                                }
                            })
                            .collect();
                        let _ = write!(o, "{}if (!strcmp(L, \"f{label}\")) {{ v->fn = {}; {} sijo_tok_expect(r, \"}}\"); }} ", if first { "" } else { "else " }, k + 1, caps.join(" "));
                        first = false;
                    }
                    let _ = write!(o, "{}sijo_tok_skip_group(r);", if first { "" } else { "else " });
                    (format!("static void sijo_d{d}_read(SijoWorld* w, SijoTok* r, SijoD{d}* v)"), o)
                }
                "shw" => (format!("static void sijo_d{d}_shw(SijoShW* o, const SijoD{d}* v)"), format!("sijo_shw_u32(o, (uint32_t)v->fn); {}", self.dg_cases(d, v, DgMode::Read, 0, &|t, x| self.sh_pack(t, x, 1)))),
                "shr" => {
                    let n = self.p.delegates[d].funcs.len();
                    (
                        format!("static void sijo_d{d}_shr(SijoWorld* w, SijoShR* in, SijoD{d}* v)"),
                        format!("memset(v, 0, sizeof *v); uint32_t fn_ = sijo_shr_u32(in); if (fn_ > {n}u) {{ in->bad = 1; fn_ = 0; }} v->fn = (int32_t)fn_; {}", self.dg_cases(d, v, DgMode::Fill, 0, &|t, x| self.sh_unpack(t, x, 1))),
                    )
                }
                _ => continue,
            };
            let _ = writeln!(protos, "{};", sig);
            let _ = writeln!(defs, "{} {{\n  (void)v; {}\n}}", sig, body);
        }
        if !protos.is_empty() {
            let _ = write!(self.out, "\n/* delegate values: {} */\n{}{}", kind, protos, defs);
        }
    }

    /// Precise roots for the string collector: every string slot in every archetype table.
    /// Runs between ticks (no locals are alive), so promoting a frame string just rewrites the slot.
    fn gc_roots(&mut self) {
        self.dg_helpers("gc");
        let mut body = String::new();
        for &a in &self.c.ecs.archetypes.clone() {
            let n = self.aname(a);
            for f in self.p.all_fields(a) {
                if self.c.ecs.is_dead(f) || !self.p.contains_str(&self.p.fields[f].ty) {
                    continue;
                }
                let k = self.comp(f);
                for table in [n.clone(), format!("p{}", n)] {
                    let visit = self.gc_visit(&self.p.fields[f].ty, &format!("w->{}.c{}[r].{}", table, k, self.fname(f)), 0);
                    let _ = writeln!(body, "  for (int32_t r = 0; r < w->{table}.count; r++) {{ {visit} }}");
                }
            }
        }
        // [Persistent]: the store's records keep their strings
        for (i, (_, _, pers, _)) in self.p.persist.clone().iter().enumerate() {
            let mut visits = self.gc_visit(&Ty::Str, &format!("w->pst{i}.r[j].key"), 0);
            for &f in pers {
                if self.p.contains_str(&self.p.fields[f].ty) {
                    visits.push(' ');
                    visits.push_str(&self.gc_visit(&self.p.fields[f].ty.clone(), &format!("w->pst{i}.r[j].f{f}"), 0));
                }
            }
            let _ = writeln!(body, "  for (int32_t j = 0; j < w->pst{i}.n; j++) {{ {visits} }}");
        }
        if body.is_empty() {
            self.w("\n/* the program stores no strings in its state: nothing to collect */\nstatic void sijo_gc(SijoWorld* w) { (void)w; }\n");
        } else {
            let _ = write!(
                self.out,
                "\n/* string collector: promote frame strings referenced by the state, free unreferenced heap strings */\nstatic void sijo_gc(SijoWorld* w) {{\n  sijo_gc_begin(&w->sheap);\n{body}  sijo_gc_sweep(&w->sheap);\n}}\n"
            );
        }
    }

    /// structs and delegate types, each after the value types it holds (struct members, captured
    /// values; collections hold pointers and need nothing)
    fn type_order(&self) -> Vec<TypeNode> {
        let mut done: Vec<TypeNode> = vec![];
        fn deps(p: &Program, n: TypeNode) -> Vec<TypeNode> {
            let mut out = vec![];
            let mut add = |t: &Ty| match t {
                Ty::Struct(s) => out.push(TypeNode::Struct(*s)),
                Ty::Delegate(d) => out.push(TypeNode::Delegate(*d)),
                _ => {}
            };
            match n {
                TypeNode::Struct(s) => p.structs[s].fields.iter().for_each(|f| add(&f.ty)),
                TypeNode::Delegate(d) => {
                    for &f in &p.delegates[d].funcs {
                        for j in 0..p.caps_of(f) {
                            let t = &p.funcs[f].locals[p.funcs[f].params[j]].ty;
                            if !p.boxed_cap(t) {
                                add(t);
                            }
                        }
                    }
                }
            }
            out
        }
        fn visit(p: &Program, n: TypeNode, done: &mut Vec<TypeNode>, depth: usize) {
            if done.contains(&n) || depth > 64 {
                return;
            }
            for x in deps(p, n) {
                visit(p, x, done, depth + 1);
            }
            if !done.contains(&n) {
                done.push(n);
            }
        }
        for s in self.struct_order() {
            visit(self.p, TypeNode::Struct(s), &mut done, 0);
        }
        for d in 0..self.p.delegates.len() {
            visit(self.p, TypeNode::Delegate(d), &mut done, 0);
        }
        done
    }

    /// `switch (v.fn)` over the functions a delegate value can hold, with `each(type, place)` applied
    /// to every captured value of the one it holds. A boxed capture (a delegate, or a struct that
    /// holds one) is copied out of its box first; `mode` says what happens to the box: `Read` only
    /// reads, `Gc` keeps the box alive (marks or promotes it like a string) and writes the visited
    /// copy back, `Fill` makes a new box in the string heap from the filled copy.
    pub(crate) fn dg_cases(&self, d: DelegateId, v: &str, mode: DgMode, depth: usize, each: &dyn Fn(&Ty, &str) -> String) -> String {
        let mut o = format!("switch (({v}).fn) {{ ");
        for (k, &f) in self.p.delegates[d].funcs.iter().enumerate() {
            let n = self.p.caps_of(f);
            if n == 0 {
                continue;
            }
            let body: Vec<String> = (0..n)
                .map(|j| {
                    let t = &self.p.funcs[f].locals[self.p.funcs[f].params[j]].ty;
                    let place = format!("({v}).env.f{}.c{j}", k + 1);
                    if !self.p.boxed_cap(t) {
                        return each(t, &place);
                    }
                    let (ct, u) = (self.cty(t), format!("u{depth}_{j}"));
                    let inner = each(t, &u);
                    match mode {
                        DgMode::Read => format!("{{ {ct} {u}; memcpy(&{u}, ({place}).p, sizeof {u}); {inner} }}"),
                        DgMode::Gc => format!("{{ sijo_str_visit(&w->sheap, &{place}); {ct} {u}; memcpy(&{u}, ({place}).p, sizeof {u}); {inner} memcpy((char*)({place}).p, &{u}, sizeof {u}); }}"),
                        DgMode::Fill => format!("{{ {ct} {u}; memset(&{u}, 0, sizeof {u}); {inner} {place} = sijo_str_heap(&w->sheap, (const char*)&{u}, (int32_t)sizeof {u}); }}"),
                    }
                })
                .collect();
            let _ = write!(o, "case {}: {{ {} }} break; ", k + 1, body.join(" "));
        }
        o.push_str("default: break; }");
        o
    }

    /// the C type a captured value has inside a delegate value (boxed ones are bytes in a string)
    fn cap_cty(&self, t: &Ty) -> String {
        if self.p.boxed_cap(t) {
            "SijoStr".into()
        } else {
            self.cty(t)
        }
    }

    /// a function a delegate value holds, as saved state names it: its place in the source and the
    /// types of what it captured (a later build that changed either reads the default value)
    pub(crate) fn dg_label(&self, f: FuncId) -> String {
        let n = self.p.caps_of(f);
        let caps: Vec<String> = (0..n).map(|j| self.type_sig(&self.p.funcs[f].locals[self.p.funcs[f].params[j]].ty)).collect();
        format!("{}({})", state_name(&self.p.func_label(f)), caps.join(","))
    }

    fn struct_order(&self) -> Vec<StructId> {
        let n = self.p.structs.len();
        let mut done = vec![false; n];
        let mut order = vec![];
        fn visit(p: &Program, s: StructId, done: &mut Vec<bool>, order: &mut Vec<StructId>, depth: usize) {
            if done[s] || depth > 64 {
                return;
            }
            for f in &p.structs[s].fields {
                if let Ty::Struct(t) = f.ty {
                    visit(p, t, done, order, depth + 1);
                }
            }
            if !done[s] {
                done[s] = true;
                order.push(s);
            }
        }
        for s in 0..n {
            visit(self.p, s, &mut done, &mut order, 0);
        }
        order
    }

    // ------------------------------------------------------------------
    // prototypes
    // ------------------------------------------------------------------

    fn sig(&self, f: FuncId, arch: Option<ClassId>, view: bool) -> String {
        let func = &self.p.funcs[f];
        let mut ps = vec!["SijoWorld* w".to_string()];
        let name = match func.kind {
            FuncKind::EntityMethod(_) => {
                if let Some(&g) = self.share.get(&(f, arch.unwrap())) {
                    ps.push(format!("const SijoView_{g}* V"));
                }
                ps.push("int32_t row".into());
                self.method_name(f, arch.unwrap(), view)
            }
            FuncKind::StructMethod(s) => {
                if func.writes_self {
                    ps.push(format!("{}* self", self.sname(s)));
                } else {
                    ps.push(format!("{} self", self.sname(s)));
                }
                self.func_name(f)
            }
            _ => self.func_name(f),
        };
        for &l in &func.params {
            let loc = &func.locals[l];
            let t = self.cty(&loc.ty);
            match loc.kind {
                LocalKind::Param(ParamMode::Ref) | LocalKind::Param(ParamMode::Out) => ps.push(format!("{}* {}", t, self.local_name(f, l))),
                _ => ps.push(format!("{} {}", t, self.local_name(f, l))),
            }
        }
        if func.ret.is_list() {
            ps.push("int region".into());
        }
        let ret = match func.kind {
            FuncKind::StructCtor(s) => self.sname(s),
            _ => self.cty(&func.ret),
        };
        let inl = match (func.kind, arch) {
            (FuncKind::EntityMethod(_), Some(a)) if !view && self.inline_in_rows(f, a) => "inline __attribute__((always_inline)) ",
            _ => "",
        };
        format!("static {}{} {}({})", inl, ret, name, ps.join(", "))
    }

    /// The method a partitioned system calls per row: its row function has two loops (all rows,
    /// and on a shard the rows it owns), so the method has two call sites and the C compiler would
    /// keep it out of line, a call per entity. Inlined into both, the loop over all rows is as fast
    /// as with one loop. Not for shared code (one copy for several tables by design) or [Recursive].
    fn inline_in_rows(&self, f: FuncId, a: ClassId) -> bool {
        !self.p.funcs[f].recursive_attr
            && self.share.get(&(f, a)).is_none()
            && self.c.ecs.systems.iter().enumerate().any(|(i, s)| {
                s.func == f && s.kind == SysKind::Entity && self.shard.mode[i] == crate::shard::Mode::Part && s.archetypes.contains(&a)
            })
    }

    /// (func, archetype, view) instances to generate
    fn instances(&self) -> Vec<(FuncId, Option<ClassId>, bool)> {
        let mut v = vec![];
        for (fid, f) in self.p.funcs.iter().enumerate() {
            if f.extern_name.is_some() {
                continue;
            }
            match f.kind {
                FuncKind::EntityMethod(c) => {
                    for a in self.p.archetypes_of(c) {
                        // shared code: generated once, for the group's representative
                        if self.share.get(&(fid, a)).is_some_and(|&g| self.groups[g].rep != a) {
                            continue;
                        }
                        v.push((fid, Some(a), false));
                        if !self.is_mutating_method(fid) {
                            v.push((fid, Some(a), true));
                        }
                    }
                }
                _ => v.push((fid, None, false)),
            }
        }
        v
    }

    fn rread_sig(&self, fid: FuncId) -> (String, Vec<String>) {
        let func = &self.p.funcs[fid];
        let ret = self.cty(&func.ret);
        let mut ps = vec!["SijoWorld* w".to_string(), "SijoH h".to_string()];
        let mut names = vec![];
        for &l in &func.params {
            let loc = &func.locals[l];
            let pt = self.cty(&loc.ty);
            let pn = format!("a{}", l);
            match loc.kind {
                LocalKind::Param(ParamMode::Ref) | LocalKind::Param(ParamMode::Out) => ps.push(format!("{}* {}", pt, pn)),
                _ => ps.push(format!("{} {}", pt, pn)),
            }
            names.push(pn);
        }
        if func.ret.is_list() {
            ps.push("int region".into());
            names.push("region".into());
        }
        (format!("static {} R{}({})", ret, fid, ps.join(", ")), names)
    }

    fn sender_sig(&self, fid: FuncId) -> String {
        let func = &self.p.funcs[fid];
        let mut ps = vec!["SijoWorld* w".to_string(), "SijoH h".to_string()];
        for &l in &func.params {
            ps.push(format!("{} a{}", self.cty(&func.locals[l].ty), l));
        }
        format!("static void S{}({})", fid, ps.join(", "))
    }

    /// C declarations of the host functions (extern) the program calls
    fn extern_decls(&self) -> String {
        let mut o = String::new();
        for f in &self.p.funcs {
            if let Some(name) = &f.extern_name {
                let mut ps = vec![];
                for &l in &f.params {
                    let loc = &f.locals[l];
                    if loc.ty == Ty::Str {
                        ps.push(format!("const char* {}", cid(&loc.name)));
                        ps.push(format!("int32_t {}_len", cid(&loc.name)));
                    } else {
                        ps.push(format!("{} {}", self.cty(&loc.ty), cid(&loc.name)));
                    }
                }
                let params = if ps.is_empty() { "void".to_string() } else { ps.join(", ") };
                let _ = writeln!(o, "extern {} {}({});", self.cty(&f.ret), name, params);
            }
        }
        o
    }

    fn prototypes(&mut self) {
        if self.p.persist.is_empty() {
            self.w("#define SIJO_TICK_END(w) ((void)(w))\n#define SIJO_RUN_END(w) ((void)(w))\n");
        } else {
            self.w("static void sijo_ptick(SijoWorld* w);\n#define SIJO_TICK_END(w) sijo_ptick(w)\nstatic void sijo_pflush(SijoWorld* w);\n#define SIJO_RUN_END(w) sijo_pflush(w)\n");
            self.w("static void sijo_pload(SijoWorld* w); /* [Persistent]: what spawned entities find in the store */\n");
            for i in 0..self.p.persist.len() {
                let _ = writeln!(self.out, "static void sijo_ptake{i}(SijoWorld* w, int arch, int32_t row);");
            }
        }
        self.w("\n/* host functions (extern): defined by the program that embeds the game */\n");
        let decls = self.extern_decls();
        self.w(&decls);
        self.w("\n/* prototypes */\n");
        for (f, a, view) in self.instances() {
            let s = self.sig(f, a, view);
            let _ = writeln!(self.out, "{};", s);
        }
        for fid in 0..self.p.funcs.len() {
            if matches!(self.p.funcs[fid].kind, FuncKind::EntityMethod(_)) && !self.is_mutating_method(fid) {
                let (s, _) = self.rread_sig(fid);
                let _ = writeln!(self.out, "{};", s);
            }
        }
        for &fid in &self.deferred_funcs.clone() {
            let s = self.sender_sig(fid);
            let _ = writeln!(self.out, "{};", s);
        }
        for &a in &self.c.ecs.archetypes.clone() {
            let n = self.aname(a);
            let _ = writeln!(self.out, "static void sijo_remove_{n}(SijoWorld* w, int32_t row);");
        }
    }

    // ------------------------------------------------------------------
    // archetype table operations
    // ------------------------------------------------------------------

    fn table_ops(&mut self) {
        self.w("\n/* ======================= table operations ======================= */\n");
        let archs = self.c.ecs.archetypes.clone();
        for &a in &archs {
            let n = self.aname(a);
            let comps = self.c.ecs.arch_comps[&a].clone();
            // resize the columns to nc >= count rows (snapshot columns are dropped when shrinking and
            // re-allocated by the next system that needs them)
            let _ = writeln!(self.out, "static void sijo_resize_{n}(SijoArch_{n}* t, int32_t nc) {{");
            let _ = writeln!(self.out, "  t->ent = (SijoH*)sijo_xrealloc(t->ent, (size_t)nc * sizeof(SijoH));");
            for &k in &comps {
                let cn = self.ccomp(k);
                let _ = writeln!(self.out, "  t->c{k} = ({cn}*)sijo_xrealloc(t->c{k}, (size_t)nc * sizeof({cn})); t->c{k}_read = t->c{k};");
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "  t->d{k} = (uint8_t*)sijo_xrealloc(t->d{k}, (size_t)nc);");
                }
            }
            let hz: Vec<usize> = comps.iter().copied().filter(|k| self.hazard_comps.contains(k)).collect();
            if !hz.is_empty() {
                self.w("  if (nc < t->cap) { ");
                for &k in &hz {
                    let _ = write!(self.out, "free(t->c{k}_prev); t->c{k}_prev = NULL; ");
                }
                self.w("t->pcap = 0; }\n");
            }
            self.w("  t->cap = nc;\n}\n");
            let _ = writeln!(self.out, "static void sijo_grow_{n}(SijoArch_{n}* t, int32_t need) {{\n  if (need <= t->cap) return;\n  int32_t nc = t->cap ? t->cap : 64; while (nc < need) nc *= 2;\n  sijo_resize_{n}(t, nc);\n}}");
            // hazard snapshot columns of a main table (pending and per-thread tables never need them)
            let _ = writeln!(self.out, "static void sijo_prev_fit_{n}(SijoArch_{n}* t) {{\n  if (t->pcap >= t->cap) return;");
            for &k in &hz {
                let cn = self.ccomp(k);
                let _ = writeln!(self.out, "  t->c{k}_prev = ({cn}*)sijo_xrealloc(t->c{k}_prev, (size_t)t->cap * sizeof({cn}));");
            }
            self.w("  t->pcap = t->cap;\n}\n");
            // capacity trimming: a table that used less than a quarter of its rows over a window of
            // 64 flushes gives the rest back (one large spawn or destroy wave does not pin memory)
            let _ = writeln!(self.out, "static void sijo_trim_{n}(SijoArch_{n}* t, int32_t used) {{\n  if (used > t->wmax) t->wmax = used;\n  if (++t->wn < 64) return;\n  if (t->cap > 1024 && (int64_t)t->wmax * 4 < (int64_t)t->cap) {{\n    int32_t nc = 64; while (nc < 2 * t->wmax || nc < t->count) nc *= 2;\n    if (nc < t->cap) sijo_resize_{n}(t, nc);\n  }}\n  t->wn = 0; t->wmax = 0;\n}}");
            // free a table and the lists its rows own
            let _ = writeln!(self.out, "static void sijo_free_table_{n}(SijoArch_{n}* t) {{\n  for (int32_t r = 0; r < t->count; r++) {{");
            for &k in &comps {
                for &f in &self.c.ecs.components[k].fields.clone() {
                    if self.p.fields[f].ty.is_list() {
                        let code = self.free_code(&self.p.fields[f].ty, &format!("t->c{k}[r].{}", self.fname(f)), 0);
                        let _ = writeln!(self.out, "    {code}");
                    }
                }
            }
            self.w("  }\n  free(t->ent);\n");
            for &k in &comps {
                let _ = writeln!(self.out, "  free(t->c{k});");
                if self.hazard_comps.contains(&k) {
                    let _ = writeln!(self.out, "  free(t->c{k}_prev);");
                }
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "  free(t->d{k});");
                }
            }
            self.w("  memset(t, 0, sizeof *t);\n}\n");
            // push defaults
            let _ = writeln!(self.out, "static int32_t sijo_push_{n}(SijoArch_{n}* t) {{\n  sijo_grow_{n}(t, t->count + 1);\n  int32_t r = t->count++;");
            for &k in &comps {
                let cn = self.ccomp(k);
                let mut inits = vec![];
                for &f in &self.c.ecs.components[k].fields.clone() {
                    inits.push(format!(".{} = {}", self.fname(f), self.field_default(f)));
                }
                let _ = writeln!(self.out, "  t->c{k}[r] = ({cn}){{ {} }};", inits.join(", "));
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "  t->d{k}[r] = 1;");
                }
            }
            self.w("  return r;\n}\n");
            // spawn (pending)
            let _ = writeln!(
                self.out,
                "static SijoH sijo_spawn_{n}(SijoWorld* w, int32_t* row, SijoArch_{n}** tb) {{\n  if (w->tspawn) {{ /* parallel system: this thread's table, ids assigned at the merge; until then a\n     provisional handle (archetype, thread, row) that no slot lookup accepts (see sijo_prov_resolve) */\n    SijoArch_{n}* t = &w->pt{n}[sijo_tid()];\n    *row = sijo_push_{n}(t); *tb = t;\n    if (w->sh_on) sijo_sh_note_spawn(w, ARCH_{n}, sijo_tid(), *row);\n    SijoH ph = {{SIJO_PROV | ((uint32_t)ARCH_{n} << 8) | (uint32_t)sijo_tid(), (uint32_t)*row + 1u}};\n    return ph;\n  }}\n  int32_t r = sijo_push_{n}(&w->p{n});\n  SijoH h = sijo_slot_alloc(&w->ents, ARCH_{n}, r, SIJO_PENDING);\n  sijo_shard_reset(&w->ents, h.idx);\n  w->p{n}.ent[r] = h; *row = r; *tb = &w->p{n}; w->spawned++;\n  return h;\n}}"
            );
            // atomic blocks: the rows spawned since a mark are dropped when the block fails, with the
            // lists they own (in the table sijo_spawn used: pending, or this thread's in a parallel system)
            let _ = writeln!(self.out, "static inline int32_t sijo_spawn_mark_{n}(SijoWorld* w) {{ return w->tspawn ? w->pt{n}[sijo_tid()].count : w->p{n}.count; }}");
            let _ = writeln!(self.out, "static void sijo_unspawn_{n}(SijoWorld* w, int32_t mark) {{\n  SijoArch_{n}* t = w->tspawn ? &w->pt{n}[sijo_tid()] : &w->p{n};\n  while (t->count > mark) {{\n    int32_t r = --t->count;");
            for &k in &comps {
                for &f in &self.c.ecs.components[k].fields.clone() {
                    if self.p.fields[f].ty.is_list() {
                        let code = self.free_code(&self.p.fields[f].ty, &format!("t->c{k}[r].{}", self.fname(f)), 0);
                        let _ = writeln!(self.out, "    {code}");
                    }
                }
            }
            self.w("    if (!w->tspawn) { sijo_slot_free(&w->ents, t->ent[r].idx); w->spawned--; }\n  }\n}\n");
            // client-side direct spawn
            let _ = writeln!(
                self.out,
                "static SijoH sijo_cspawn_{n}(SijoWorld* w, int32_t* row) {{\n  int32_t r = sijo_push_{n}(&w->{n});\n  SijoH h = sijo_slot_alloc(&w->ents, ARCH_{n}, r, SIJO_ALIVE);\n  w->{n}.ent[r] = h; *row = r;\n  return h;\n}}"
            );
            // remove (swap-remove)
            let _ = writeln!(self.out, "static void sijo_remove_{n}(SijoWorld* w, int32_t row) {{\n  SijoArch_{n}* t = &w->{n};");
            for &k in &comps {
                for &f in &self.c.ecs.components[k].fields.clone() {
                    if self.p.fields[f].ty.is_list() {
                        let code = self.free_code(&self.p.fields[f].ty, &format!("t->c{k}[row].{}", self.fname(f)), 0);
                        let _ = writeln!(self.out, "  {code}");
                    }
                }
            }
            let _ = writeln!(self.out, "  sijo_slot_free(&w->ents, t->ent[row].idx);\n  int32_t last = --t->count;\n  if (row != last) {{\n    t->ent[row] = t->ent[last];");
            for &k in &comps {
                let _ = writeln!(self.out, "    t->c{k}[row] = t->c{k}[last];");
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "    t->d{k}[row] = t->d{k}[last];");
                }
            }
            self.w("    w->ents.s[t->ent[row].idx].row = row;\n  }\n  w->destroyed++;\n}\n");
            // flush pending -> main; into an empty main table the buffers are swapped instead of
            // copying the rows (a large spawn wave, e.g. in Game.Start, then exists only once)
            let _ = writeln!(
                self.out,
                "static void sijo_flush_{n}(SijoWorld* w) {{\n  SijoArch_{n}* p = &w->p{n}; SijoArch_{n}* t = &w->{n};\n  int32_t used = p->count;\n  if (used && t->count == 0) {{\n    SijoH* e_ = t->ent; t->ent = p->ent; p->ent = e_;\n    int32_t c_ = t->cap; t->cap = p->cap; p->cap = c_;"
            );
            for &k in &comps {
                let cn = self.ccomp(k);
                let _ = writeln!(self.out, "    {{ {cn}* x_ = t->c{k}; t->c{k} = p->c{k}; p->c{k} = x_; t->c{k}_read = t->c{k}; p->c{k}_read = p->c{k}; }}");
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "    {{ uint8_t* x_ = t->d{k}; t->d{k} = p->d{k}; p->d{k} = x_; memset(t->d{k}, 1, (size_t)used); }}");
                }
            }
            self.w("    t->count = used;\n    for (int32_t i = 0; i < used; i++) { SijoSlot* s = &w->ents.s[t->ent[i].idx]; s->row = i; s->state = SIJO_ALIVE; }\n  } else if (used) {\n");
            let _ = writeln!(self.out, "    sijo_grow_{n}(t, t->count + used);\n    for (int32_t i = 0; i < used; i++) {{\n      int32_t r = t->count++;\n      t->ent[r] = p->ent[i];");
            for &k in &comps {
                let _ = writeln!(self.out, "      t->c{k}[r] = p->c{k}[i];");
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "      t->d{k}[r] = 1;");
                }
            }
            let _ = writeln!(self.out, "      SijoSlot* s = &w->ents.s[p->ent[i].idx]; s->row = r; s->state = SIJO_ALIVE;\n    }}\n  }}\n  p->count = 0;\n  sijo_trim_{n}(p, used);\n  sijo_trim_{n}(t, t->count);\n}}");
        }
        // destroy dispatcher
        self.w("static void sijo_destroy(SijoWorld* w, SijoH h) {\n  SijoSlot* s = sijo_live(&w->ents, h);\n  if (!s) return;\n  switch (s->arch) {\n");
        for &a in &archs {
            let n = self.aname(a);
            // [Persistent]: what a destroyed entity had goes into the store first
            let take = match self.p.persist.iter().position(|(c, ..)| self.p.root_class(a) == *c) {
                Some(i) => format!("if (sijo_store_dir && !w->is_client) sijo_ptake{i}(w, ARCH_{n}, s->row); "),
                None => String::new(),
            };
            let _ = writeln!(self.out, "  case ARCH_{n}: {take}sijo_remove_{n}(w, s->row); break;");
        }
        self.w("  }\n}\n");
        // per-thread spawn tables -> pending table, threads in order = rows in order
        for &a in &archs {
            let n = self.aname(a);
            let comps = self.c.ecs.arch_comps[&a].clone();
            // the rows move as they are: no default values first (a default array would leak)
            let _ = writeln!(self.out, "static void sijo_tmerge_{n}(SijoWorld* w) {{\n  for (int t = 0; t < SIJO_MAX_THREADS; t++) {{\n    SijoArch_{n}* s = &w->pt{n}[t];\n    for (int32_t i = 0; i < s->count; i++) {{\n      sijo_grow_{n}(&w->p{n}, w->p{n}.count + 1);\n      int32_t r = w->p{n}.count++;");
            for &k in &comps {
                let _ = writeln!(self.out, "      w->p{n}.c{k}[r] = s->c{k}[i];");
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "      w->p{n}.d{k}[r] = 1;");
                }
            }
            self.w("      SijoH h = sijo_slot_alloc(&w->ents, ARCH_");
            let _ = writeln!(self.out, "{n}, r, SIJO_PENDING);\n      sijo_shard_reset(&w->ents, h.idx); w->p{n}.ent[r] = h; w->spawned++;\n      s->ent[i] = h; /* the final handle of provisional (arch, t, i) */\n    }}\n    int32_t used = s->count; s->count = 0;\n    sijo_trim_{n}(s, used);\n  }}\n}}");
        }
        // provisional handle -> final handle, after the merge
        self.w("static SijoH sijo_prov_resolve(SijoWorld* w, SijoH h) {\n  uint32_t t = h.idx & 0xffu, j = h.gen - 1u;\n  switch ((h.idx >> 8) & 0xffu) {\n");
        for &a in &archs {
            let n = self.aname(a);
            let _ = writeln!(self.out, "  case ARCH_{n}: return j < (uint32_t)w->pt{n}[t].cap ? w->pt{n}[t].ent[j] : SIJO_NULL;");
        }
        self.w("  }\n  return SIJO_NULL;\n}\n");
        self.w("static void sijo_tmerge_all(SijoWorld* w) {\n");
        for &a in &archs {
            let _ = writeln!(self.out, "  sijo_tmerge_{}(w);", self.aname(a));
        }
        self.w("}\n");
        self.w("static void sijo_flush_all(SijoWorld* w) {\n");
        for &a in &archs {
            let _ = writeln!(self.out, "  sijo_flush_{}(w);", self.aname(a));
        }
        self.w("}\n");
        if self.has_comps() {
            self.comp_changes();
        }
    }

    // ------------------------------------------------------------------
    // components (docs/04 §4.4.1): rows move between the tables of a class
    // ------------------------------------------------------------------

    fn has_comps(&self) -> bool {
        self.p.classes.iter().any(|c| c.is_component)
    }
    /// open table `a` (docs/04 §4.4.1): the C test that row `row` has every component of `need`
    /// (None: not an open table, or nothing of `need` to test)
    fn open_has(&self, a: ClassId, need: &[ClassId], row: &str) -> Option<String> {
        let flag = *self.p.open.get(&a)?;
        let bits = need.iter().filter_map(|&k| self.p.open_bit(a, k)).fold(0u32, |m, (_, b)| m | (1u32 << b));
        if bits == 0 {
            return None;
        }
        Some(format!("(((uint32_t)w->{}.c{}[{row}].{} & 0x{bits:x}u) == 0x{bits:x}u)", self.aname(a), self.comp(flag), self.fname(flag)))
    }
    /// components a query over `class` with `with` needs on each row (the component classes)
    fn query_needs(&self, class: ClassId, with: &[ClassId]) -> Vec<ClassId> {
        std::iter::once(class).chain(with.iter().copied()).filter(|&k| self.p.classes[k].is_component).collect()
    }
    /// `Has<C>()` through a handle: `sijo_is` on the archetype, or the presence bit on open tables
    fn has_test(&self, h: &str, class: ClassId) -> String {
        if self.p.classes[class].is_component && self.p.archetypes_of(class).iter().any(|a| self.p.open.contains_key(a)) {
            format!("sijo_has{class}(w, {h})")
        } else {
            format!("sijo_is(w, {h}, {})", self.mask(class))
        }
    }
    fn component_classes(&self) -> Vec<ClassId> {
        (0..self.p.classes.len()).filter(|&k| self.p.classes[k].is_component).collect()
    }
    /// the fields of a component that have storage, in declaration order (the Add payload)
    fn comp_payload_fields(&self, k: ClassId) -> Vec<FieldId> {
        self.p.classes[k].own_fields.iter().copied().filter(|&f| !self.c.ecs.is_dead(f) && self.c.ecs.comp_of(f).is_some()).collect()
    }
    /// a component field whose value an `Add` carries packed after the plain values (strings,
    /// lists and arrays: bytes, as message arguments are, so the command can cross threads, shards
    /// and the network)
    fn packed_field(&self, f: FieldId) -> bool {
        let t = &self.p.fields[f].ty;
        t.is_list() || self.p.contains_str(t)
    }
    /// the command tag of a component
    fn comp_tag(&self, k: ClassId) -> String {
        k.to_string()
    }
    /// (from, component, add?, to): the moves the planned combinations allow
    fn comp_moves(&self) -> Vec<(ClassId, ClassId, bool, ClassId)> {
        let mut out = vec![];
        let archs = self.c.ecs.archetypes.clone();
        for &a in &archs {
            let fam: Vec<ClassId> = self.p.family(a).into_iter().filter(|b| archs.contains(b)).collect();
            let mine: BTreeSet<ClassId> = self.p.classes[a].comps.iter().copied().collect();
            for k in self.component_classes() {
                let add = !mine.contains(&k);
                let mut want = mine.clone();
                if add {
                    want.insert(k);
                } else {
                    want.remove(&k);
                }
                if let Some(&b) = fam.iter().find(|&&b| self.p.classes[b].comps.iter().copied().collect::<BTreeSet<_>>() == want) {
                    out.push((a, k, add, b));
                }
            }
        }
        out
    }

    /// `sijo_move_A_B`: the entity in row `row` of A goes to the end of B (B's other columns take
    /// their defaults), the last row of A fills the hole (swap-remove); the slot follows, so every
    /// handle of the entity stays valid. `sijo_comp_change` applies one Add/Remove command.
    fn comp_changes(&mut self) {
        let moves = self.comp_moves();
        let mut pairs: Vec<(ClassId, ClassId)> = moves.iter().map(|&(a, _, _, b)| (a, b)).collect();
        pairs.sort();
        pairs.dedup();
        for (a, b) in pairs {
            let (an, bn) = (self.aname(a), self.aname(b));
            let ac = self.c.ecs.arch_comps[&a].clone();
            let bc = self.c.ecs.arch_comps[&b].clone();
            let _ = writeln!(self.out, "static int32_t sijo_move_{an}_{bn}(SijoWorld* w, int32_t row) {{\n  SijoArch_{an}* s = &w->{an}; SijoArch_{bn}* t = &w->{bn};\n  int32_t r = sijo_push_{bn}(t);\n  SijoH h = s->ent[row]; t->ent[r] = h;");
            for &k in ac.iter().filter(|k| bc.contains(k)) {
                let _ = writeln!(self.out, "  t->c{k}[r] = s->c{k}[row];");
            }
            // the collections of the components the entity loses go with them
            for &k in ac.iter().filter(|k| !bc.contains(k)) {
                for &f in &self.c.ecs.components[k].fields.clone() {
                    if self.p.fields[f].ty.is_list() {
                        let _ = writeln!(self.out, "  {}", self.free_code(&self.p.fields[f].ty, &format!("s->c{k}[row].{}", self.fname(f)), 0));
                    }
                }
            }
            self.w("  int32_t last = --s->count;\n  if (row != last) {\n    s->ent[row] = s->ent[last];\n");
            for &k in &ac {
                let _ = writeln!(self.out, "    s->c{k}[row] = s->c{k}[last];");
                if self.c.ecs.components[k].replicated {
                    let _ = writeln!(self.out, "    s->d{k}[row] = s->d{k}[last];");
                }
            }
            let _ = writeln!(self.out, "    w->ents.s[s->ent[row].idx].row = row;\n  }}\n  {{ SijoSlot* sl = &w->ents.s[h.idx]; sl->arch = ARCH_{bn}; sl->row = r; }}\n  return r;\n}}");
        }
        self.w("/* one Add/Remove command, after the destroys of the flush (a destroyed entity gains nothing) */\nstatic void sijo_comp_change(SijoWorld* w, const SijoCmd* c, const uint8_t* data) {\n  SijoSlot* s = sijo_live(&w->ents, c->target);\n  if (!s) return;\n  int32_t row = s->row; (void)data;\n  if (c->kind == SIJO_CMD_CADD) {\n    switch (c->field) {\n");
        for k in self.component_classes() {
            let kn = mangle(&self.p.classes[k].name);
            let fields = self.comp_payload_fields(k);
            let packed: Vec<FieldId> = fields.iter().copied().filter(|&f| self.packed_field(f)).collect();
            let size_ok = if packed.is_empty() { "c->size != (uint16_t)sizeof v" } else { "c->size < (uint16_t)sizeof v" };
            let _ = writeln!(self.out, "    case {}: {{ /* {} */\n      SijoAdd_{kn} v; if ({size_ok}) return; memcpy(&v, data + c->data, sizeof v); (void)v;\n      switch (s->arch) {{", self.comp_tag(k), self.p.classes[k].name);
            for &a in &self.c.ecs.archetypes.clone() {
                // the table the entity ends in: its own (the values are replaced) or the one with C
                let dest = if self.p.classes[a].comps.contains(&k) {
                    Some((a, "row".to_string()))
                } else {
                    moves.iter().find(|m| m.0 == a && m.1 == k && m.2).map(|m| (m.3, format!("sijo_move_{}_{}(w, row)", self.aname(a), self.aname(m.3))))
                };
                let Some((b, r)) = dest else { continue };
                let bn = self.aname(b);
                let mut body = format!("int32_t r = {r}; (void)r;");
                // strings and collections follow the plain values (an entity that already had the
                // component gives up its old collections)
                if !packed.is_empty() {
                    let _ = write!(body, " const uint8_t* d = data + c->data + sizeof v; const uint8_t* e_ = data + c->data + c->size; int bad_ = 0;");
                    for &f in &packed {
                        let kk = self.comp(f);
                        let place = format!("w->{bn}.c{kk}[r].{}", self.fname(f));
                        let ty = self.p.fields[f].ty.clone();
                        // the old value (Add on an entity that has the component) or the default the
                        // new row was made with
                        if ty.is_list() {
                            let _ = write!(body, " {}", self.free_code(&ty, &place, 0));
                        }
                        let _ = write!(body, " {}", self.msg_unpack(&ty, &place, "1", 0));
                    }
                    let _ = write!(body, " if (bad_) sijo_panic(\"internal: a malformed Add command\");");
                }
                for &f in fields.iter().filter(|&&f| !packed.contains(&f)) {
                    let kk = self.comp(f);
                    let place = format!("w->{bn}.c{kk}[r].{}", self.fname(f));
                    let _ = write!(body, " {place} = v.{};", self.fname(f));
                    let cl = self.clamp_code(f, &format!("(&{place})"));
                    if !cl.is_empty() {
                        let _ = write!(body, " {cl}");
                    }
                }
                let mut dirty: Vec<usize> = fields.iter().map(|&f| self.comp(f)).filter(|&kk| self.c.ecs.components[kk].replicated).collect();
                // an open table: the row's bit
                if let Some((flag, bit)) = self.p.open_bit(b, k) {
                    let kk = self.comp(flag);
                    let _ = write!(body, " w->{bn}.c{kk}[r].{} |= (int32_t)(1u << {bit});", self.fname(flag));
                    if self.c.ecs.components[kk].replicated {
                        dirty.push(kk);
                    }
                }
                dirty.sort();
                dirty.dedup();
                for kk in dirty {
                    let _ = write!(body, " w->{bn}.d{kk}[r] = 1;");
                }
                let _ = writeln!(self.out, "      case ARCH_{}: {{ {body} return; }}", self.aname(a));
            }
            self.w("      default: sijo_panic(\"internal: a component change the compiler did not plan\");\n      }\n      return; }\n");
        }
        self.w("    }\n  } else {\n    switch (c->field) {\n");
        for k in self.component_classes() {
            let _ = writeln!(self.out, "    case {}: /* {} */\n      switch (s->arch) {{", self.comp_tag(k), self.p.classes[k].name);
            for &(a, kk, add, b) in &moves {
                if kk == k && !add {
                    let _ = writeln!(self.out, "      case ARCH_{an}: sijo_move_{an}_{bn}(w, row); return;", an = self.aname(a), bn = self.aname(b));
                }
            }
            // an open table: the bit goes, and the component's collections with it (the next Add
            // brings new values for every field)
            for a in self.c.ecs.archetypes.clone() {
                let Some((flag, bit)) = self.p.open_bit(a, k) else { continue };
                let n = self.aname(a);
                let fk = self.comp(flag);
                let mut body = format!("if (!(((uint32_t)w->{n}.c{fk}[row].{f} >> {bit}) & 1u)) return; w->{n}.c{fk}[row].{f} &= (int32_t)~(1u << {bit});", f = self.fname(flag));
                if self.c.ecs.components[fk].replicated {
                    let _ = write!(body, " w->{n}.d{fk}[row] = 1;");
                }
                for f in self.comp_payload_fields(k) {
                    let ty = self.p.fields[f].ty.clone();
                    if ty.is_list() {
                        let place = format!("w->{n}.c{}[row].{}", self.comp(f), self.fname(f));
                        let _ = write!(body, " {} {place} = {};", self.free_code(&ty, &place, 0), self.field_default(f));
                    }
                }
                let _ = writeln!(self.out, "      case ARCH_{n}: {{ {body} return; }}");
            }
            // removing a component the entity does not have does nothing
            self.w("      default: return;\n      }\n");
        }
        self.w("    }\n  }\n}\n");
    }

    /// `e.Add(new C { F = x })`: the payload (defaults, then the initializers in order) goes into
    /// the command buffer like a remote write
    fn comp_add(&mut self, target: &Expr, k: ClassId, inits: &[(FieldId, Expr)], cx: &mut FCtx) -> String {
        let hv = if matches!(target.kind, ExprKind::SelfRef) { self.self_handle(cx) } else { self.handle_of(target, cx) };
        let kn = mangle(&self.p.classes[k].name);
        let t = self.t();
        let mut o = format!("({{ SijoH {t}h = {hv}; SijoAdd_{kn} {t}; memset(&{t}, 0, sizeof {t}); ");
        let live = self.comp_payload_fields(k);
        let packed: Vec<FieldId> = live.iter().copied().filter(|&f| self.packed_field(f)).collect();
        // strings and collections: evaluated in order with the rest, packed after the plain values
        let mut pvals: Vec<(FieldId, String)> = vec![];
        // the temporaries among them (defaults, new lists) are freed once packed
        let mut temps: Vec<(FieldId, String)> = vec![];
        for &f in &live {
            if !inits.iter().any(|(g, _)| *g == f) {
                if packed.contains(&f) {
                    let ft = self.cty(&self.p.fields[f].ty);
                    let pt = format!("{t}p{f}");
                    let _ = write!(o, "{ft} {pt} = {}; ", self.field_default(f));
                    pvals.push((f, pt.clone()));
                    temps.push((f, pt));
                } else {
                    let _ = write!(o, "{t}.{} = {}; ", self.fname(f), self.field_default(f));
                }
            }
        }
        for (f, x) in inits {
            if !live.contains(f) {
                if !crate::opt::is_pure_expr(x) {
                    let v = self.expr(x, cx);
                    let _ = write!(o, "(void)({}); ", v);
                }
                continue;
            }
            let v = self.expr(x, cx);
            if packed.contains(f) {
                let ft = self.cty(&self.p.fields[*f].ty);
                let pt = format!("{t}p{f}");
                let _ = write!(o, "{ft} {pt} = {v}; ");
                pvals.push((*f, pt.clone()));
                if !matches!(x.kind, ExprKind::Local(_) | ExprKind::EField { .. } | ExprKind::Field { .. } | ExprKind::ListIndex { .. } | ExprKind::DictGet { .. }) {
                    temps.push((*f, pt));
                }
            } else {
                let _ = write!(o, "{t}.{} = {v}; ", self.fname(*f));
            }
        }
        if packed.is_empty() {
            let _ = write!(o, "sijo_send_cadd(w, {t}h, {}, &{t}, (uint16_t)sizeof {t}); }})", self.comp_tag(k));
            return o;
        }
        // the plain values, then each string and collection in field order (copies: the values
        // themselves stay with whoever made them)
        let _ = write!(o, "int {t}t = sijo_tid(); SijoCmdBuf* b = &w->cmd[{t}t]; uint32_t {t}s = sijo_cmd_data(b, &{t}, (uint32_t)sizeof {t}); ");
        for &f in &packed {
            let (_, pt) = pvals.iter().find(|(g, _)| *g == f).cloned().unwrap();
            o.push_str(&self.msg_pack(&self.p.fields[f].ty.clone(), &pt, 0));
        }
        for (f, pt) in &temps {
            let ty = self.p.fields[*f].ty.clone();
            if ty.is_list() {
                o.push_str(&self.free_code(&ty, pt, 0));
                o.push(' ');
            }
        }
        let _ = write!(o, "if (b->dn - {t}s > 65535u) sijo_panic(\"Add: the component's strings and lists take more than 64 KB\"); SijoCmd* {t}c = sijo_cmd_push(b); {t}c->kind = SIJO_CMD_CADD; {t}c->target = {t}h; {t}c->src = w->tls[{t}t].cur; {t}c->field = {}; {t}c->size = (uint16_t)(b->dn - {t}s); {t}c->data = {t}s; }})", self.comp_tag(k));
        o
    }

    fn accessors(&mut self) {
        // the views of the tables that share method code (rows do not move while it runs)
        for (g, grp) in self.groups.clone().iter().enumerate() {
            for &a in &grp.members {
                let n = self.aname(a);
                let mut init = format!(".ent = w->{n}.ent, .arch = ARCH_{n}");
                for &k in &grp.clusters {
                    let _ = write!(init, ", .c{k} = w->{n}.c{k}, .c{k}_read = w->{n}.c{k}_read");
                    if self.c.ecs.components[k].replicated {
                        let _ = write!(init, ", .d{k} = w->{n}.d{k}");
                    }
                }
                let _ = writeln!(self.out, "static inline SijoView_{g} sijo_view{g}_{n}(SijoWorld* w) {{ SijoView_{g} v = {{ {init} }}; return v; }}");
            }
        }
        // Has<C>() where open tables may hold C: the archetype, then the row's bit
        for k in self.component_classes() {
            let archs = self.p.archetypes_of(k);
            if !archs.iter().any(|a| self.p.open.contains_key(a)) {
                continue;
            }
            let _ = writeln!(self.out, "/* Has<{}>() */\nstatic inline bool sijo_has{k}(SijoWorld* w, SijoH h) {{\n  SijoSlot* s = sijo_live(&w->ents, h);\n  if (!s) return 0;\n  switch (s->arch) {{", self.p.classes[k].name);
            for &a in &archs {
                let test = self.open_has(a, &[k], "s->row").unwrap_or_else(|| "1".into());
                let _ = writeln!(self.out, "  case ARCH_{}: return {test};", self.aname(a));
            }
            self.w("  default: return 0;\n  }\n}\n");
        }
        self.w("\n/* component accessors through handles (read = start-of-system snapshot, w = current) */\n");
        for (k, comp) in self.c.ecs.components.clone().iter().enumerate() {
            let archs: Vec<ClassId> = self.p.archetypes_of(comp.class);
            for (suffix, arr) in [("", "_read"), ("_w", "")] {
                let _ = writeln!(
                    self.out,
                    "static inline {cn}* acc_{cn}{suffix}(SijoWorld* w, SijoH h) {{\n  SijoSlot* s = sijo_live(&w->ents, h);\n  if (!s) sijo_panic(\"access through a stale entity handle\");\n  switch (s->arch) {{",
                    cn = self.ccomp(k)
                );
                for &a in &archs {
                    let n = self.aname(a);
                    let _ = writeln!(self.out, "  case ARCH_{n}: return &w->{n}.c{k}{arr}[s->row];");
                }
                self.w("  }\n  sijo_panic(\"bad archetype\"); return NULL;\n}\n");
            }
        }
    }

    /// C expression: `v` saturated into a range type's [lo, hi] (the same functions as field contracts)
    fn saturate_code(&mut self, ty: &Ty, v: &str, lo: f64, hi: f64) -> String {
        match ty {
            Ty::Int => format!("sijo_clampi({v}, {}, {})", lo.ceil() as i64, hi.floor() as i64),
            Ty::Float => format!("sijo_clamprf({v}, {}, {})", Self::flit(lo), Self::flit(hi)),
            Ty::Struct(s) => {
                let t = self.t();
                let mut o = format!("({{ {} {t} = {v}; ", self.sname(*s));
                for m in &self.p.structs[*s].fields {
                    let mn = cid(&m.name);
                    match m.ty {
                        Ty::Int => {
                            let _ = write!(o, "{t}.{mn} = sijo_clampi({t}.{mn}, {}, {}); ", lo.ceil() as i64, hi.floor() as i64);
                        }
                        Ty::Float => {
                            let _ = write!(o, "{t}.{mn} = sijo_clamprf({t}.{mn}, {}, {}); ", Self::flit(lo), Self::flit(hi));
                        }
                        _ => {}
                    }
                }
                let _ = write!(o, "{t}; }})");
                o
            }
            _ => v.to_string(),
        }
    }

    /// C statements re-applying a ranged local's contract after a write (empty when it has none)
    fn local_clamp(&mut self, cx: &FCtx, target: &Expr) -> Option<String> {
        let ExprKind::Local(l) = target.kind else { return None };
        let (lo, hi) = self.p.funcs[cx.fid].locals[l].range?;
        let n = self.local_ref(cx, l);
        if target.ty == Ty::Int {
            // the optimizer may have proved that the new value stays inside one or both bounds
            let sides = self.c.opt.clamp_sides(cx.fid, target.span);
            return Some(match Self::int_clamp(&n, lo, hi, sides) {
                Some(v) => format!("{n} = {v};"),
                None => String::new(),
            });
        }
        let v = self.saturate_code(&target.ty.clone(), &n, lo, hi);
        Some(format!("{n} = {v};"))
    }

    /// C statements re-applying the range contract of a variable that a built-in wrote through `out`
    /// (int.TryParse, float.TryParse, Dictionary.TryGetValue): a ranged local, or the hidden entity
    /// field that holds one across an await (empty when there is no range)
    fn out_clamp(&mut self, target: &Expr, cx: &mut FCtx) -> String {
        if let Some(cl) = self.local_clamp(cx, target) {
            return cl;
        }
        if let ExprKind::EField { base, field } = &target.kind {
            if matches!(base.kind, ExprKind::SelfRef) && self.c.net.clamp.contains_key(field) && !self.c.ecs.is_dead(*field) {
                let pl = self.place(target, cx);
                return self.clamp_code(*field, &format!("(&{})", pl));
            }
        }
        String::new()
    }

    /// an int saturated into [lo, hi] on the given sides (None: neither side can be crossed)
    fn int_clamp(v: &str, lo: f64, hi: f64, (low, high): (bool, bool)) -> Option<String> {
        let (lo, hi) = (lo.ceil() as i64, hi.floor() as i64);
        match (low, high) {
            (true, true) => Some(format!("sijo_clampi({v}, {lo}, {hi})")),
            (true, false) => Some(format!("sijo_maxi({v}, {lo})")),
            (false, true) => Some(format!("sijo_mini({v}, {hi})")),
            (false, false) => None,
        }
    }

    /// clamp_code for one write of a field: an int field skips the sides the optimizer proved
    /// cannot be crossed (`site`: the write's target, or the value of a Spawn initializer)
    fn clamp_code_at(&self, f: FieldId, ptr: &str, fid: FuncId, site: crate::diag::Span) -> String {
        match (self.c.net.clamp.get(&f), &self.p.fields[f].ty) {
            (Some(&(lo, hi)), Ty::Int) => match Self::int_clamp(&format!("*({ptr})"), lo, hi, self.c.opt.clamp_sides(fid, site)) {
                Some(v) => format!("*({ptr}) = {v};"),
                None => String::new(),
            },
            _ => self.clamp_code(f, ptr),
        }
    }

    /// C statements clamping a field with a declared range, given a pointer expression to the field.
    pub(crate) fn clamp_code(&self, f: FieldId, ptr: &str) -> String {
        let Some(&(lo, hi)) = self.c.net.clamp.get(&f) else { return String::new() };
        match &self.p.fields[f].ty {
            Ty::Int => format!("*({ptr}) = sijo_clampi(*({ptr}), {}, {});", lo.ceil() as i64, hi.floor() as i64),
            Ty::Float => format!("*({ptr}) = sijo_clamprf(*({ptr}), {}, {});", Self::flit(lo), Self::flit(hi)),
            Ty::Struct(s) => {
                let mut o = String::new();
                for m in &self.p.structs[*s].fields {
                    let mn = cid(&m.name);
                    match m.ty {
                        Ty::Int => {
                            let _ = write!(o, "({ptr})->{mn} = sijo_clampi(({ptr})->{mn}, {}, {}); ", lo.ceil() as i64, hi.floor() as i64);
                        }
                        Ty::Float => {
                            let _ = write!(o, "({ptr})->{mn} = sijo_clamprf(({ptr})->{mn}, {}, {}); ", Self::flit(lo), Self::flit(hi));
                        }
                        _ => {}
                    }
                }
                o
            }
            _ => String::new(),
        }
    }

    fn field_apply(&mut self) {
        self.w("\n/* apply a deferred field command (remote write) */\nstatic void sijo_apply_field(SijoWorld* w, const SijoCmd* c) {\n  SijoSlot* s = sijo_live(&w->ents, c->target);\n  if (!s) return;\n  switch (c->field) {\n");
        for fid in 0..self.p.fields.len() {
            if self.c.ecs.is_dead(fid) || self.c.ecs.comp_of(fid).is_none() {
                continue;
            }
            let k = self.comp(fid);
            let cn = self.ccomp(k);
            let fty = self.cty(&self.p.fields[fid].ty);
            let fname = self.fname(fid);
            let _ = writeln!(self.out, "  case {}: {{ /* {} */", fid + 1, self.p.field_label(fid));
            let fc = self.p.fields[fid].class;
            if self.p.classes[fc].is_component {
                // (the type system only hands out component handles for entities that have it;
                // a write to an entity that lost the component meanwhile has nowhere to go)
                let _ = writeln!(self.out, "    if (!(({} >> s->arch) & 1)) break;", self.mask(fc));
            }
            let _ = writeln!(self.out, "    {cn}* comp = acc_{cn}_w(w, c->target);\n    uint8_t* p = (uint8_t*)&comp->{fname} + c->off;");
            // `+=` commands only reach numeric places (a bool field is only ever set)
            if self.p.fields[fid].ty == Ty::Bool {
                self.w("    if (c->kind == SIJO_CMD_SET) memcpy(p, w->cmd[0].data + c->data, c->size);\n");
            } else {
                self.w("    if (c->kind == SIJO_CMD_SET) memcpy(p, w->cmd[0].data + c->data, c->size);\n    else if (c->kind == SIJO_CMD_ADDI) { int32_t v; memcpy(&v, p, 4); v = sijo_add32(v, c->v.i); memcpy(p, &v, 4); }\n    else if (c->kind == SIJO_CMD_ADDF) { float v; memcpy(&v, p, 4); v += c->v.f; memcpy(p, &v, 4); }\n");
            }
            let cl = self.clamp_code(fid, &format!("({}*)&comp->{}", fty, fname));
            if !cl.is_empty() {
                let _ = writeln!(self.out, "    {}", cl);
            }
            if self.c.ecs.components[k].replicated {
                self.w("    switch (s->arch) {\n");
                for a in self.p.archetypes_of(self.p.fields[fid].class) {
                    let n = self.aname(a);
                    let _ = writeln!(self.out, "    case ARCH_{n}: w->{n}.d{k}[s->row] = 1; break;");
                }
                self.w("    }\n");
            }
            self.w("    break; }\n");
        }
        self.w("  }\n}\n");
    }

    // ------------------------------------------------------------------
    // user functions
    // ------------------------------------------------------------------

    fn user_functions(&mut self) {
        for (f, a, view) in self.instances() {
            // --gpu: what an offloaded row loop calls is compiled for the device as well
            let device = self.device_funcs.contains(&(f, a, view));
            if device {
                self.w("#pragma omp declare target\n");
            }
            self.function(f, a, view);
            if device {
                self.w("#pragma omp end declare target\n");
            }
        }
    }

    fn function(&mut self, f: FuncId, arch: Option<ClassId>, view: bool) {
        let func = &self.p.funcs[f];
        let sig = self.sig(f, arch, view);
        let _ = writeln!(self.out, "/* {} */\n{} {{", self.p.func_label(f), sig);
        // hoisted locals
        let mut list_locals = vec![];
        for (l, loc) in func.locals.iter().enumerate() {
            if loc.kind == LocalKind::Param(ParamMode::Own) {
                list_locals.push(l);
                continue;
            }
            if let (LocalKind::Param(ParamMode::Value), Some((lo, hi))) = (loc.kind, loc.range) {
                let n = self.local_name(f, l);
                let v = match loc.ty {
                    Ty::Int => format!("sijo_clampi({n}, {}, {})", lo.ceil() as i64, hi.floor() as i64),
                    _ => format!("sijo_clamprf({n}, {}, {})", Self::flit(lo), Self::flit(hi)),
                };
                let _ = writeln!(self.out, "  {n} = {v}; /* range of '{}' */", loc.name);
                continue;
            }
            if matches!(loc.kind, LocalKind::Param(_) | LocalKind::Cursor(_)) {
                continue;
            }
            if loc.ty == Ty::Error {
                continue;
            }
            let init = if loc.ty == Ty::Str { "SIJO_STR_EMPTY".to_string() } else { "{0}".to_string() };
            let _ = writeln!(self.out, "  {} {} = {};", self.cty(&loc.ty), self.local_name(f, l), init);
            // a foreach variable borrows the element (List<List<T>>): it owns nothing to free
            if loc.ty.is_list() && loc.kind != LocalKind::ListElem {
                list_locals.push(l);
            }
        }
        if let FuncKind::StructCtor(s) = func.kind {
            let _ = writeln!(self.out, "  {sn} self_v = {}; {sn}* self = &self_v;", self.default_of(&Ty::Struct(s)), sn = self.sname(s));
        }
        let mut cx = FCtx { fid: f, arch, view, cursors: HashMap::new(), break_labels: vec![], atomic_fail: None, co_state: None, hoisted: HashMap::new(), cache: BTreeSet::new(), writeback: String::new(), rewind_first: String::new(), rush_bg: None, rush: vec![], atomics: vec![], jumps: vec![], copied: BTreeSet::new(), shared: arch.and_then(|a| self.share.get(&(f, a)).copied()) };
        // the row of `this` in locals: one load per field on entry, one store per written field on
        // exit (lets the C compiler keep fields in registers and vectorize self-only updates)
        let cache = self.row_cache_fields(f, arch, view);
        if let (Some(a), false) = (arch, cache.is_empty()) {
            let writes = self.c.info.get(f, Some(a)).self_writes;
            for &fl in &cache {
                let mem = format!("{}c{}[row].{}", self.self_tab(&cx, a), self.comp(fl), self.fname(fl));
                let _ = writeln!(self.out, "  {} sf{fl}_ = {mem};", self.cty(&self.p.fields[fl].ty));
                if writes.contains(&fl) {
                    let _ = write!(cx.writeback, "{mem} = sf{fl}_; ");
                }
            }
            cx.cache = cache;
        }
        if let (Some(co), Some(_)) = (func.co.clone(), arch) {
            // resume where the last await suspended (0 = start, -1 = finished)
            let st = Expr::new(ExprKind::EField { base: Box::new(Expr::new(ExprKind::SelfRef, Ty::Void, func.span)), field: co.state }, Ty::Int, func.span);
            let place = self.place(&st, &mut cx);
            // arms an earlier Rush(...) left running: one step each tick, before resuming here
            let mut arms: Vec<(u32, Expr)> = vec![];
            let mut bg: Option<Expr> = None;
            collect_rush(&func.body, &mut arms, &mut bg);
            if let Some(bg) = bg {
                let bp = self.place(&bg, &mut cx);
                for (bit, call) in &arms {
                    let ExprKind::Call { func: g, .. } = &call.kind else { continue };
                    let gst = self.p.funcs[*g].co.as_ref().map(|c| c.state).expect("rushed method is a coroutine");
                    let se = Expr::new(ExprKind::EField { base: Box::new(Expr::new(ExprKind::SelfRef, Ty::Void, call.span)), field: gst }, Ty::Int, call.span);
                    let sp = self.place(&se, &mut cx);
                    let cv = self.expr(call, &mut cx);
                    let _ = writeln!(self.out, "  if ({bp} & {bit}u) {{ {cv}; if ({sp} == -1) {bp} &= ~{bit}u; }} /* left running by Rush */");
                    cx.rush.push((*bit, *g));
                }
                cx.rush_bg = Some(bp);
            }
            let mut sw = format!("  switch ({place}) {{ case 0: break;");
            for k in 1..=co.points {
                let _ = write!(sw, " case {k}: goto co{k};");
            }
            sw.push_str(" default: return; }
");
            self.out.push_str(&sw);
            cx.co_state = Some(place);
        }
        let body = func.body.clone();
        let code = self.block(&body, &mut cx, 1);
        self.out.push_str(&code);
        if let Some(stp) = &cx.co_state {
            let _ = writeln!(self.out, "  {stp} = -1; /* coroutine finished */");
        }
        if !cx.writeback.is_empty() {
            let _ = writeln!(self.out, "  {}", cx.writeback.trim_end());
        }
        // implicit end-of-function cleanup
        let mut tail = String::new();
        for &l in &list_locals {
            let code = self.free_code(&func.locals[l].ty, &self.local_name(f, l), 0);
            let _ = writeln!(tail, "  {code}");
        }
        if let FuncKind::StructCtor(_) = func.kind {
            tail.push_str("  return self_v;\n");
        }
        self.out.push_str(&tail);
        self.w("}\n");
        self.restore_line();
    }

    fn list_locals(&self, f: FuncId) -> Vec<LocalId> {
        self.p.funcs[f]
            .locals
            .iter()
            .enumerate()
            .filter(|(_, l)| l.ty.is_list() && (l.kind == LocalKind::Param(ParamMode::Own) || !matches!(l.kind, LocalKind::Param(_) | LocalKind::Cursor(_) | LocalKind::ListElem)))
            .map(|(i, _)| i)
            .collect()
    }

    fn ind(n: usize) -> String {
        "  ".repeat(n)
    }

    fn block(&mut self, b: &Block, cx: &mut FCtx, ind: usize) -> String {
        let mut o = String::new();
        // what a statement of this block makes sure of holds for the rest of it, not after it
        let copied = cx.copied.clone();
        for s in &b.stmts {
            if let Some(d) = self.line_directive(s) {
                o.push_str(&d);
            }
            o.push_str(&self.stmt(s, cx, ind));
        }
        cx.copied = copied;
        o
    }

    /// `#line N "file.sijo"` before a statement whose source line differs from the previous one
    fn line_directive(&mut self, s: &Stmt) -> Option<String> {
        let sm = self.sm.as_ref()?;
        let mut sp = None;
        match s {
            Stmt::Return(_, x) | Stmt::Atomic { span: x, .. } | Stmt::Fail(x) => sp = Some(*x),
            _ => {
                let mut first = |e: &Expr| {
                    if sp.is_none() && e.span != crate::diag::Span::default() {
                        sp = Some(e.span);
                    }
                };
                match s {
                    Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) => crate::sema::visit_expr(e, &mut first),
                    Stmt::If { cond, .. } => crate::sema::visit_expr(cond, &mut first),
                    Stmt::Loop { cond: Some(c), .. } => crate::sema::visit_expr(c, &mut first),
                    Stmt::ForList { list, .. } => crate::sema::visit_expr(list, &mut first),
                    Stmt::Switch { scrut, .. } => crate::sema::visit_expr(scrut, &mut first),
                    Stmt::Await { kind, .. } => {
                        for e in kind.exprs() {
                            crate::sema::visit_expr(e, &mut first);
                        }
                    }
                    _ => {}
                }
            }
        }
        let sp = sp?;
        let (fi, lo) = sm.local(sp.lo);
        let line = crate::diag::line_col(sm.file_text(fi), lo).0;
        if self.last_pos == Some((fi, line)) {
            return None;
        }
        self.last_pos = Some((fi, line));
        Some(format!("#line {} {}\n", line, c_str(&sm.files[fi].path)))
    }

    /// map the following generated code back to the C file
    fn restore_line(&mut self) {
        if self.sm.is_some() {
            let n = self.out.matches('\n').count() + 2;
            let _ = writeln!(self.out, "#line {} {}", n, c_str(&self.cfile));
            self.last_pos = None;
        }
    }

    fn stmt(&mut self, s: &Stmt, cx: &mut FCtx, ind: usize) -> String {
        if cx.atomics.is_empty() {
            return self.stmt_code(s, cx, ind);
        }
        let pre = self.atomic_ensure(s, cx, &Self::ind(ind));
        let code = self.stmt_code(s, cx, ind);
        format!("{pre}{code}")
    }

    fn stmt_code(&mut self, s: &Stmt, cx: &mut FCtx, ind: usize) -> String {
        let i = Self::ind(ind);
        match s {
            Stmt::Let { local, init } => {
                let name = self.local_name(cx.fid, *local);
                let lty = self.p.funcs[cx.fid].locals[*local].ty.clone();
                match init {
                    Some(e) => {
                        if lty.is_list() {
                            let src = self.list_src(e, cx);
                            let t = self.t();
                            let fr = self.free_code(&lty, &name, 0);
                            format!("{i}{{ SijoList {t} = {src}; {fr} {name} = {t}; }}\n")
                        } else {
                            let v = self.expr(e, cx);
                            format!("{i}{name} = {v};\n")
                        }
                    }
                    None => format!("{i}{name} = {};\n", self.default_of(&lty)),
                }
            }
            Stmt::Expr(e) => {
                let v = self.expr(e, cx);
                format!("{i}{};\n", v)
            }
            Stmt::If { cond, then, els } => {
                let c = self.expr(cond, cx);
                let t = self.block(then, cx, ind + 1);
                let mut o = format!("{i}if ({c}) {{\n{t}{i}}}");
                // `else if` chains stay flat (one level of C nesting however long the chain is)
                let mut rest = els.as_ref();
                while let Some(e) = rest {
                    if let [inner @ Stmt::If { cond: c2, then: t2, els: e2 }] = e.stmts.as_slice() {
                        let line = self.line_directive(inner).unwrap_or_default();
                        let c = self.expr(c2, cx);
                        let t = self.block(t2, cx, ind + 1);
                        if line.is_empty() {
                            let _ = write!(o, " else if ({c}) {{\n{t}{i}}}");
                        } else {
                            let _ = write!(o, " else\n{line}{i}if ({c}) {{\n{t}{i}}}");
                        }
                        rest = e2.as_ref();
                        continue;
                    }
                    let eb = self.block(e, cx, ind + 1);
                    let _ = write!(o, " else {{\n{eb}{i}}}");
                    break;
                }
                o.push('\n');
                o
            }
            Stmt::Loop { cond, step, body, do_while } => {
                let extra: Vec<&Expr> = cond.iter().chain(step.iter()).collect();
                let rw = self.loop_rewind(cx, body, &extra);
                let c = cond.as_ref().map(|c| self.expr(c, cx)).unwrap_or_else(|| "1".into());
                let steps: Vec<String> = step.iter().map(|e| self.expr(e, cx)).collect();
                cx.break_labels.push(None);
                cx.jumps.push((true, cx.atomics.len()));
                let b = self.block(body, cx, ind + 1);
                cx.break_labels.pop();
                cx.jumps.pop();
                let (open, first, close) = self.rewind_parts(&rw, &i);
                if *do_while {
                    format!("{open}{i}do {{\n{first}{b}{i}}} while ({c});\n{close}")
                } else {
                    format!("{open}{i}for (; {c}; {}) {{\n{first}{b}{i}}}\n{close}", steps.join(", "))
                }
            }
            Stmt::ForQuery { var, class, with, alias, body } => {
                let archs = self.p.query_archetypes(*class, with);
                let needs = self.query_needs(*class, with);
                let label = format!("qbrk{}", {
                    self.tmp += 1;
                    self.tmp
                });
                let mut o = String::new();
                let uses_break = archs.len() > 1;
                let spatial = if cx.arch.is_some() { self.c.spatial.find(cx.fid, *var).map(|(li, l)| (li, l.clone())) } else { None };
                let hoists = self.invariant_handle_uses(cx, *var, body);
                let rw = self.loop_rewind(cx, body, &[]);
                let (open, first, close) = self.rewind_parts(&rw, &i);
                o.push_str(&open);
                for a in archs {
                    let n = self.aname(a);
                    let rv = format!("r{}_{}", var, self.arch_id[&a]);
                    let mut inner = cx.clone();
                    inner.cursors.insert(*var, (a, rv.clone()));
                    // foreach (var (s, b) in Query<Ship, Burning>()): the same row seen as each type
                    for &l in alias {
                        inner.cursors.insert(l, (a, rv.clone()));
                    }
                    inner.rewind_first = first.clone();
                    // handles that cannot change while the loop runs are resolved once: compare rows
                    // instead of handles, test `is` once (rows are stable during a system)
                    let mut decl = String::new();
                    for (key, hx, is_class) in &hoists {
                        let hv = self.expr(hx, &mut cx.clone());
                        self.tmp += 1;
                        let v = format!("hh{}", self.tmp);
                        match is_class {
                            None => {
                                let _ = write!(decl, "int32_t {v} = sijo_hrow(w, {hv}, ARCH_{n}); ");
                            }
                            Some(c) => {
                                let _ = write!(decl, "bool {v} = {}; ", self.has_test(&hv, *c));
                            }
                        }
                        inner.hoisted.insert(*key, v);
                    }
                    if !decl.is_empty() {
                        let _ = writeln!(o, "{i}{{ {decl}");
                    }
                    inner.break_labels.push(if uses_break { Some(label.clone()) } else { None });
                    inner.jumps.push((true, inner.atomics.len()));
                    let rf = inner.rewind_first.clone();
                    let b = format!("{}{}", rf, self.block(body, &mut inner, ind + 1));
                    // a halo loop on a shard skips rows whose copy is not valid here (src/shard.rs)
                    let mut skip = match &spatial {
                        Some((li, _)) if self.shard.halo_loops.contains(li) => format!("{i}  if (w->sh_hv && !w->sh_hv[w->{n}.ent[{rv}].idx]) continue;\n"),
                        _ => String::new(),
                    };
                    // an open table: only the rows that have the components
                    let oskip = self.open_has(a, &needs, &rv).map(|t| format!("{i}  if (!{t}) continue;\n")).unwrap_or_default();
                    skip.push_str(&oskip);
                    let full = format!("for (int32_t {rv} = 0, {rv}_n = w->{n}.count; {rv} < {rv}_n; {rv}++) {{\n{skip}{b}{i}}}");
                    let gid = spatial.as_ref().and_then(|(li, _)| self.grid_ids.get(&(*li, a)).copied());
                    match (gid, &spatial) {
                        (Some(gid), Some((_, l))) => {
                            // only the neighbouring cells, in ascending row order: same visits as the full scan
                            let mut b2cx = inner.clone();
                            let b2 = format!("{}{}", inner.rewind_first, self.block(body, &mut b2cx, ind + 2));
                            let me = Expr::new(ExprKind::SelfRef, Ty::Void, Default::default());
                            let sp = self.efield_read(&me, l.self_field, cx);
                            let sty = self.p.fields[l.self_field].ty.clone();
                            let sx = format!("{sp}{}", self.member_path(&sty, &l.self_paths[0]));
                            let sy = match l.self_paths.get(1) {
                                Some(pth) => format!("sijo_grid_cell(G, {sp}{})", self.member_path(&sty, pth)),
                                None => "0".to_string(),
                            };
                            let _ = writeln!(
                                o,
                                "{i}if (w->grid[{gid}].ok) {{ /* spatial grid: rows of the neighbouring cells, ascending */\n{i}  const SijoGrid* G = &w->grid[{gid}]; uint32_t {rv}_b = sijo_grid_hash(G, sijo_grid_cell(G, {sx}), {sy});\n{i}  for (int32_t {rv}_q = G->start[{rv}_b], {rv}_e = G->start[{rv}_b + 1]; {rv}_q < {rv}_e; {rv}_q++) {{\n{i}    int32_t {rv} = G->rows[{rv}_q];\n{oskip}{b2}{i}  }}\n{i}}} else {full}"
                            );
                        }
                        _ => {
                            let _ = writeln!(o, "{i}{full}");
                        }
                    }
                    if !decl.is_empty() {
                        let _ = writeln!(o, "{i}}}");
                    }
                }
                if uses_break {
                    // the break may belong to a nested loop or switch: the label can stay unused
                    let _ = writeln!(o, "{i}{label}: __attribute__((unused));");
                }
                o.push_str(&close);
                o
            }
            Stmt::ForList { var, idx, list, body } => {
                let el = self.p.funcs[cx.fid].locals[*var].ty.clone();
                let et = self.cty(&el);
                let vn = self.local_name(cx.fid, *var);
                let iname = self.local_name(cx.fid, *idx);
                let is_place = matches!(list.kind, ExprKind::EField { .. } | ExprKind::Local(_) | ExprKind::Field { .. });
                let rw = self.loop_rewind(cx, body, &[]);
                cx.break_labels.push(None);
                cx.jumps.push((true, cx.atomics.len()));
                let b = self.block(body, cx, ind + 1);
                cx.break_labels.pop();
                cx.jumps.pop();
                let (open, first, close) = self.rewind_parts(&rw, &i);
                if is_place {
                    // re-read the list header each iteration: safe even if the body grows the list
                    let l = self.expr(list, cx);
                    format!("{open}{i}for ({iname} = 0; {iname} < ({l}).len; {iname}++) {{\n{first}{i}  {vn} = (({et}*)({l}).data)[{iname}];\n{b}{i}}}\n{close}")
                } else {
                    // the list is evaluated before the mark: it stays valid across rewinds
                    let l = self.expr(list, cx);
                    let tl = self.t();
                    format!("{i}{{ SijoList {tl} = {l};\n{open}{i}for ({iname} = 0; {iname} < {tl}.len; {iname}++) {{\n{first}{i}  {vn} = (({et}*){tl}.data)[{iname}];\n{b}{i}}}\n{close}{i}}}\n")
                }
            }
            Stmt::Return(v, _) => {
                let func = &self.p.funcs[cx.fid];
                let lists = self.list_locals(cx.fid);
                // returning from inside atomic blocks commits them (after the value is computed)
                let mut frees: String = self.atomic_leave(cx, 0);
                frees.push_str(&cx.writeback);
                frees.extend(lists.iter().map(|&l| format!("{} ", self.free_code(&self.p.funcs[cx.fid].locals[l].ty, &self.local_name(cx.fid, l), 0))));
                if let Some(stp) = &cx.co_state {
                    frees.push_str(&format!("{stp} = -1; "));
                }
                if let FuncKind::StructCtor(_) = func.kind {
                    return format!("{i}{{ {frees}return self_v; }}\n");
                }
                match v {
                    Some(e) => {
                        let rt = self.cty(&func.ret);
                        let val = if e.ty.is_list() { self.list_src(e, cx) } else { self.expr(e, cx) };
                        if frees.is_empty() {
                            format!("{i}return {val};\n")
                        } else {
                            format!("{i}{{ {rt} _ret = {val}; {frees}return _ret; }}\n")
                        }
                    }
                    None => format!("{i}{{ {frees}return; }}\n"),
                }
            }
            Stmt::Break => {
                // leaving atomic blocks commits them
                let commit = cx.jumps.last().map(|&(_, keep)| self.atomic_leave(cx, keep)).unwrap_or_default();
                let jump = match cx.break_labels.last().cloned().flatten() {
                    Some(l) => format!("goto {l};"),
                    None => "break;".to_string(),
                };
                if commit.is_empty() {
                    format!("{i}{jump}\n")
                } else {
                    format!("{i}{{ {commit}{jump} }}\n")
                }
            }
            Stmt::Continue => {
                let commit = cx.jumps.iter().rev().find(|j| j.0).map(|&(_, keep)| self.atomic_leave(cx, keep)).unwrap_or_default();
                if commit.is_empty() {
                    format!("{i}continue;\n")
                } else {
                    format!("{i}{{ {commit}continue; }}\n")
                }
            }
            Stmt::Block(b) => {
                let inner = self.block(b, cx, ind + 1);
                format!("{i}{{\n{inner}{i}}}\n")
            }
            Stmt::Atomic { body, id, .. } => self.atomic(body, *id, cx, ind),
            Stmt::Await { point, kind, timer, .. } => {
                let k = *point;
                let stp = cx.co_state.clone().unwrap_or_else(|| "/*no coroutine*/(void)0".into());
                let frees: String = self.list_locals(cx.fid).iter().map(|&l| format!("{} ", self.free_code(&self.p.funcs[cx.fid].locals[l].ty, &self.local_name(cx.fid, l), 0))).collect();
                let suspend = format!("{{ {stp} = {k}; {frees}return; }}");
                match kind {
                    AwaitKind::NextTick => format!("{i}{suspend} /* await NextTick() */
{i}co{k}: ;
"),
                    AwaitKind::Wait(t) => {
                        let tp = self.place(timer, cx);
                        let tv = self.expr(t, cx);
                        format!("{i}{tp} = sijo_wait_ticks({tv}, w->dt); {suspend} /* await Wait(...) */
{i}co{k}: if (({tp} -= 1.0f) > 0.0f) {{ {frees}return; }}
")
                    }
                    AwaitKind::Until(c) => {
                        let cv = self.expr(c, cx);
                        format!("{i}co{k}: if (!{}) {suspend} /* await Until(...) */
", paren(&cv))
                    }
                    AwaitKind::Call(call) => {
                        // start the awaited coroutine, then resume it every tick until it finishes
                        let ExprKind::Call { func: g, .. } = &call.kind else { unreachable!() };
                        let st = self.p.funcs[*g].co.as_ref().map(|c| c.state).expect("awaited method is a coroutine");
                        let se = Expr::new(ExprKind::EField { base: Box::new(Expr::new(ExprKind::SelfRef, Ty::Void, call.span)), field: st }, Ty::Int, call.span);
                        let sp = self.place(&se, cx);
                        let cv = self.expr(call, cx);
                        let name = self.p.funcs[*g].name.clone();
                        let unrush = self.unrush(cx, *g);
                        format!("{i}{unrush}{sp} = 0; /* await {name}(...) */
{i}co{k}: {cv}; if ({sp} != -1) {suspend}
")
                    }
                    AwaitKind::Group { race, arms, done, result, rush } => {
                        // start every arm, then evaluate the arms in order each tick (this one first)
                        let mut start = String::new();
                        let mut each = String::new();
                        let mut cancel = String::new();
                        let donep = done.as_ref().map(|d| self.place(d, cx));
                        for (j, a) in arms.iter().enumerate() {
                            let finished = |cond: &str| -> String {
                                match &donep {
                                    Some(dp) => format!("if (!({dp} & {bit})) {{ if ({cond}) {dp} |= {bit}; }}", bit = 1u32 << j),
                                    None => format!("if (win_ < 0 && ({cond})) win_ = {j};"),
                                }
                            };
                            match a {
                                AwaitArm::Call(call) => {
                                    let ExprKind::Call { func: g, .. } = &call.kind else { unreachable!() };
                                    let st = self.p.funcs[*g].co.as_ref().map(|c| c.state).expect("awaited method is a coroutine");
                                    let se = Expr::new(ExprKind::EField { base: Box::new(Expr::new(ExprKind::SelfRef, Ty::Void, call.span)), field: st }, Ty::Int, call.span);
                                    let sp = self.place(&se, cx);
                                    let cv = self.expr(call, cx);
                                    let unrush = self.unrush(cx, *g);
                                    let _ = write!(start, "{unrush}{sp} = 0; ");
                                    match rush {
                                        // Rush: an unfinished loser keeps running in the background
                                        Some(r) => {
                                            let bp = self.place(&r.bg, cx);
                                            let _ = write!(cancel, "if ({sp} != -1) {bp} |= {}u; ", r.bits[j]);
                                        }
                                        None => {
                                            let _ = write!(cancel, "if ({sp} != -1) {sp} = -1; ");
                                        }
                                    }
                                    let line = match &donep {
                                        Some(dp) => format!("if (!({dp} & {bit})) {{ {cv}; if ({sp} == -1) {dp} |= {bit}; }}", bit = 1u32 << j),
                                        None => format!("if (win_ < 0) {{ {cv}; if ({sp} == -1) win_ = {j}; }}"),
                                    };
                                    let _ = writeln!(each, "{i}  {line}");
                                }
                                AwaitArm::Wait { secs, timer } => {
                                    let tp = self.place(timer, cx);
                                    let tv = self.expr(secs, cx);
                                    // one more than a lone Wait: the arms are first evaluated in the tick the await starts
                                    let _ = write!(start, "{tp} = sijo_wait_ticks({tv}, w->dt) + 1.0f; ");
                                    let _ = writeln!(each, "{i}  {}", finished(&format!("({tp} -= 1.0f) <= 0.0f")));
                                }
                                AwaitArm::Until(c) => {
                                    let cv = self.expr(c, cx);
                                    let _ = writeln!(each, "{i}  {}", finished(&cv));
                                }
                            }
                        }
                        let what = if rush.is_some() { "Rush" } else if *race { "Race" } else { "Sync" };
                        match &donep {
                            Some(dp) => {
                                let all = (1u64 << arms.len()) - 1;
                                format!("{i}{start}{dp} = 0; /* await {what}(...) */
{i}co{k}: {{
{each}{i}  if ({dp} != {all}) {suspend}
{i}}}
")
                            }
                            None => {
                                let res = result.as_ref().map(|r| format!("{} = win_; ", self.place(r, cx))).unwrap_or_default();
                                format!("{i}{start}/* await {what}(...) */
{i}co{k}: {{
{i}  int32_t win_ = -1;
{each}{i}  if (win_ < 0) {suspend}
{i}  {cancel}{res}
{i}}}
")
                            }
                        }
                    }
                }
            }
            Stmt::Switch { tmp, scrut, sections, default } => {
                // `switch (0) { default: ... }` gives C# semantics for free: `break` leaves the
                // switch, `continue` reaches the enclosing loop
                let mut o = format!("{i}switch (0) {{ default: {{\n");
                if let Some(t) = tmp {
                    let v = self.expr(scrut, cx);
                    let _ = writeln!(o, "{i}  {} = {};", self.local_name(cx.fid, *t), v);
                }
                cx.break_labels.push(None);
                cx.jumps.push((false, cx.atomics.len()));
                let mut first = true;
                for (c, b) in sections {
                    let cc = self.expr(c, cx);
                    let bb = self.block(b, cx, ind + 2);
                    let _ = write!(o, "{}if ({}) {{\n{bb}{i}  }}", if first { format!("{i}  ") } else { " else ".to_string() }, cc);
                    first = false;
                }
                if let Some(d) = default {
                    let bb = self.block(d, cx, ind + 2);
                    if first {
                        let _ = write!(o, "{i}  {{\n{bb}{i}  }}");
                    } else {
                        let _ = write!(o, " else {{\n{bb}{i}  }}");
                    }
                }
                cx.break_labels.pop();
                cx.jumps.pop();
                let _ = writeln!(o, "\n{i}}} }}");
                o
            }
            Stmt::Fail(_) => match &cx.atomic_fail {
                Some(l) => format!("{i}goto {l};\n"),
                None => format!("{i}SIJO_AT(sijo_panic(\"fail outside atomic\"));\n"),
            },
        }
    }

    fn atomic(&mut self, body: &Block, id: usize, cx: &mut FCtx, ind: usize) -> String {
        let i = Self::ind(ind);
        let n = {
            self.tmp += 1;
            self.tmp
        };
        let fail = format!("afail{}", n);
        let done = format!("adone{}", n);
        if !crate::sema::fails_to_end(body) {
            // no `fail` of its own: nothing to undo (changes to collections are logged by the
            // enclosing blocks that can fail)
            cx.atomics.push(ALevel { n, colls: vec![] });
            let b = self.block(body, cx, ind + 1);
            cx.atomics.pop();
            return format!("{i}{{ /* atomic without fail: nothing to undo */\n{b}{i}}}\n");
        }
        let plan = self.c.info.atomic_saves.get(&id).cloned().unwrap_or_default();
        let (fields, locals) = (plan.fields, plan.locals);
        let mut save = String::new();
        let mut restore = String::new();
        if let Some(a) = cx.arch {
            let present = self.p.all_fields(a);
            for f in fields {
                if self.c.ecs.is_dead(f) || !present.contains(&f) {
                    continue;
                }
                let place = self.self_place(cx, a, f);
                let ty = self.cty(&self.p.fields[f].ty);
                let _ = write!(save, "{ty} s{n}_{f} = {place}; ");
                let _ = write!(restore, "{place} = s{n}_{f}; ");
            }
        }
        for l in locals {
            let name = self.local_name(cx.fid, l);
            let loc = &self.p.funcs[cx.fid].locals[l];
            let (ty, place) = match loc.kind {
                LocalKind::Param(ParamMode::Ref) | LocalKind::Param(ParamMode::Out) => (self.cty(&loc.ty), format!("(*{})", name)),
                _ => (self.cty(&loc.ty), name),
            };
            let _ = write!(save, "{ty} sl{n}_{l} = {place}; ");
            let _ = write!(restore, "{place} = sl{n}_{l}; ");
        }
        // collections: a length mark (only appended to) or a copy made before the first change
        let mut colls: Vec<ACol> = vec![];
        for (root, append) in plan.colls {
            let (ty, place) = match root {
                crate::effects::CollRoot::Local(l) => (self.p.funcs[cx.fid].locals[l].ty.clone(), self.local_ref(cx, l)),
                crate::effects::CollRoot::Field(f) => {
                    let Some(a) = cx.arch else { continue };
                    if self.c.ecs.is_dead(f) || !self.p.all_fields(a).contains(&f) {
                        continue;
                    }
                    (self.p.fields[f].ty.clone(), self.self_place(cx, a, f))
                }
            };
            colls.push(ACol { root, append, ty, place });
        }
        let mut coll_save = String::new();
        let mut coll_restore = String::new();
        for (k, c) in colls.iter().enumerate() {
            let place = &c.place;
            if c.append {
                let _ = write!(coll_save, "int32_t am{n}_{k} = ({place}).len; ");
                // elements added since the mark are dropped (lists inside them freed)
                let el = self.p.elem_of(&c.ty).unwrap_or(Ty::Int);
                let drop = if el.is_list() {
                    let fr = self.free_code(&el, &format!("((SijoList*)M_->data)[q_]"), 0);
                    format!("for (int32_t q_ = am{n}_{k}; q_ < M_->len; q_++) {{ {fr} }} ")
                } else {
                    String::new()
                };
                let _ = write!(coll_restore, "{{ SijoList* M_ = &({place}); if (M_->len > am{n}_{k}) {{ {drop}M_->len = am{n}_{k}; }} }} ");
            } else {
                let _ = write!(coll_save, "SijoList as{n}_{k} = {{0}}; uint8_t at{n}_{k} = 0; ");
                let fr = self.free_code(&c.ty, place, 0);
                let _ = write!(coll_restore, "if (at{n}_{k}) {{ {fr} {place} = as{n}_{k}; }} ");
            }
        }
        // rows spawned in the block (also by methods it calls, which may use the per-thread tables
        // of a parallel system) are dropped with the lists they own
        let mut pend_save = String::new();
        let mut pend_restore = String::new();
        for &a in &self.c.ecs.archetypes {
            let an = self.aname(a);
            let _ = write!(pend_save, "int32_t pm{n}_{an} = sijo_spawn_mark_{an}(w); ");
            let _ = write!(pend_restore, "sijo_unspawn_{an}(w, pm{n}_{an}); ");
        }
        let saved = cx.atomic_fail.replace(fail.clone());
        cx.atomics.push(ALevel { n, colls });
        let b = self.block(body, cx, ind + 1);
        let commit = self.atomic_commit(cx, cx.atomics.len() - 1);
        cx.atomics.pop();
        cx.atomic_fail = saved;
        let commit = if commit.is_empty() { String::new() } else { format!("{i}  {commit}/* commit: drop the copies */\n") };
        format!(
            "{i}{{ /* atomic: static undo log */\n{i}  {save}{coll_save}\n{i}  int32_t cm{n} = w->cmd[sijo_tid()].n; uint32_t dm{n} = w->cmd[sijo_tid()].dn; {pend_save}\n{b}{commit}{i}  goto {done};\n{i}  {fail}: __attribute__((unused)); {restore}{coll_restore}\n{i}  w->cmd[sijo_tid()].n = cm{n}; w->cmd[sijo_tid()].dn = dm{n}; {pend_restore}\n{i}  {done}: ;\n{i}}}\n"
        )
    }

    /// C code that commits the collections of the atomic block `cx.atomics[li]`: a copy made in it
    /// becomes the enclosing block's copy when that one has none yet (the collection was the same at
    /// both entries), otherwise it is freed.
    fn atomic_commit(&self, cx: &FCtx, li: usize) -> String {
        let lv = &cx.atomics[li];
        let n = lv.n;
        let mut o = String::new();
        for (k, c) in lv.colls.iter().enumerate().filter(|(_, c)| !c.append) {
            let fr = self.free_code(&c.ty, &format!("as{n}_{k}"), 0);
            let outer = cx.atomics[..li].iter().rev().find_map(|ol| ol.colls.iter().position(|x| x.root == c.root).map(|j| (ol.n, j, ol.colls[j].append)));
            match outer {
                Some((on, j, false)) => {
                    let _ = write!(o, "if (at{n}_{k}) {{ if (at{on}_{j}) {{ {fr} }} else {{ as{on}_{j} = as{n}_{k}; at{on}_{j} = 1; }} }} ");
                }
                _ => {
                    let _ = write!(o, "if (at{n}_{k}) {{ {fr} }} ");
                }
            }
        }
        o
    }

    /// C code committing the atomic blocks a jump leaves (those after the first `keep`), innermost first
    fn atomic_leave(&self, cx: &FCtx, keep: usize) -> String {
        (keep..cx.atomics.len()).rev().map(|li| self.atomic_commit(cx, li)).collect()
    }

    /// Before a statement inside atomic blocks: copy each collection it changes, if the block whose
    /// undo log covers it has no copy yet (append-only blocks need none: their mark is taken on entry,
    /// and the next enclosing block that copies gets the copy).
    fn atomic_ensure(&self, s: &Stmt, cx: &mut FCtx, i: &str) -> String {
        if cx.atomics.iter().all(|l| l.colls.iter().all(|c| c.append)) {
            return String::new();
        }
        let mut ch = BTreeMap::new();
        for e in crate::effects::stmt_own_exprs(s) {
            crate::effects::expr_coll_changes(self.p, &self.c.info.summaries, &[cx.arch], e, &mut ch);
        }
        let mut o = String::new();
        for root in ch.keys() {
            for lv in cx.atomics.iter().rev() {
                let Some((k, c)) = lv.colls.iter().enumerate().find(|(_, c)| c.root == *root) else { continue };
                if c.append {
                    continue;
                }
                let n = lv.n;
                if cx.copied.insert((n, k)) {
                    let copy = self.snap_code(&c.ty, &c.place, 0);
                    let _ = writeln!(o, "{i}if (!at{n}_{k}) {{ as{n}_{k} = {copy}; at{n}_{k} = 1; }} /* undo log: copy before the first change */");
                }
                break;
            }
        }
        o
    }

    /// C expression: a copy of the collection `src` and of the collections inside it, each copy in
    /// the memory region of what it copies (an atomic block's undo log puts it back in its place)
    fn snap_code(&self, ty: &Ty, src: &str, depth: usize) -> String {
        let a = self.arena();
        if ty.is_dict() {
            let (kind, _, et) = self.dict_info(ty);
            return format!("({{ SijoList S{depth} = {src}; sijo_dict_clone({a}, S{depth}.heap, &S{depth}, {kind}, (int32_t)sizeof({et})); }})");
        }
        let el = self.p.elem_of(ty).unwrap_or(Ty::Int);
        if !el.is_list() {
            let et = self.elem_cty(&el);
            return format!("({{ SijoList S{depth} = {src}; sijo_list_clone({a}, S{depth}.heap, &S{depth}, (int32_t)sizeof({et})); }})");
        }
        let inner = self.snap_code(&el, &format!("((SijoList*)S{depth}.data)[c{depth}]"), depth + 1);
        format!("({{ SijoList S{depth} = {src}; SijoList C{depth} = sijo_list_clone({a}, S{depth}.heap, &S{depth}, (int32_t)sizeof(SijoList)); for (int32_t c{depth} = 0; c{depth} < C{depth}.len; c{depth}++) ((SijoList*)C{depth}.data)[c{depth}] = {inner}; C{depth}; }})")
    }

    // ------------------------------------------------------------------
    // expressions
    // ------------------------------------------------------------------

    fn arena(&self) -> &'static str {
        "&w->frame[sijo_tid()]"
    }

    /// Code for a list value in a move position.
    fn list_src(&mut self, e: &Expr, cx: &mut FCtx) -> String {
        match &e.kind {
            ExprKind::Local(l) if !cx.cursors.contains_key(l) => format!("sijo_list_take(&{})", self.local_ref(cx, *l)),
            // a coroutine local kept in a hidden field across awaits moves like a local: the field
            // is emptied, so it is not freed a second time
            ExprKind::EField { base, field }
                if matches!(base.kind, ExprKind::SelfRef)
                    && cx.arch.is_some()
                    && self.p.funcs[cx.fid].co.as_ref().is_some_and(|co| co.hoisted.iter().any(|(_, x)| x == field)) =>
            {
                format!("sijo_list_take(&{})", self.place(e, cx))
            }
            _ => self.expr(e, cx),
        }
    }

    fn local_ref(&self, cx: &FCtx, l: LocalId) -> String {
        let loc = &self.p.funcs[cx.fid].locals[l];
        let n = self.local_name(cx.fid, l);
        match loc.kind {
            LocalKind::Param(ParamMode::Ref) | LocalKind::Param(ParamMode::Out) => format!("(*{})", n),
            _ => n,
        }
    }

    fn self_handle(&self, cx: &FCtx) -> String {
        match cx.arch {
            Some(a) => format!("{}ent[row]", self.self_tab(cx, a)),
            None => "SIJO_NULL".into(),
        }
    }

    /// handle expression for an entity-typed expression
    fn handle_of(&mut self, e: &Expr, cx: &mut FCtx) -> String {
        self.expr(e, cx)
    }

    /// C leaves the evaluation order of operands unspecified; C# evaluates left to right.
    /// When an operand has side effects, evaluate every non-constant operand into a temporary, in order.
    fn ordered_args(&mut self, items: Vec<(&Expr, String)>) -> (String, Vec<String>) {
        self.ordered_args_ex(items.into_iter().map(|(e, s)| (e, s, true)).collect())
    }

    /// like ordered_args; items with `false` (ref/out pointers) are never moved into temporaries
    fn ordered_args_ex(&mut self, items: Vec<(&Expr, String, bool)>) -> (String, Vec<String>) {
        let impure = items.iter().filter(|(e, _, _)| !crate::opt::is_pure_expr(e)).count();
        if impure == 0 || items.len() < 2 {
            return (String::new(), items.into_iter().map(|(_, s, _)| s).collect());
        }
        let mut pre = String::new();
        let mut out = vec![];
        for (e, s, hoist) in items {
            if !hoist || crate::opt::is_lit(e) || e.ty == Ty::Void || e.ty == Ty::Error || matches!(e.kind, ExprKind::Str(_)) {
                out.push(s);
            } else {
                let t = self.t();
                let _ = write!(pre, "{} {} = {}; ", self.cty(&e.ty), t, s);
                out.push(t);
            }
        }
        (pre, out)
    }

    fn wrap(pre: String, e: String) -> String {
        if pre.is_empty() {
            e
        } else {
            format!("({{ {} {}; }})", pre, e)
        }
    }

    fn expr(&mut self, e: &Expr, cx: &mut FCtx) -> String {
        match &e.kind {
            ExprKind::Closure { dg, num, caps } => {
                if caps.is_empty() {
                    return format!("((SijoD{dg}){{ .fn = {num} }})");
                }
                let a = self.arena();
                let vals: Vec<String> = caps
                    .iter()
                    .map(|c| {
                        let v = self.expr(c, cx);
                        if self.p.boxed_cap(&c.ty) {
                            // a delegate inside a delegate: its bytes in a frame string (a box)
                            let ct = self.cty(&c.ty);
                            format!("({{ {ct} b_ = {v}; sijo_str_frame({a}, (const char*)&b_, (int32_t)sizeof b_); }})")
                        } else {
                            v
                        }
                    })
                    .collect();
                format!("((SijoD{dg}){{ .fn = {num}, .env.f{num} = {{ {} }} }})", vals.join(", "))
            }
            ExprKind::DgFn(x) => format!("({}).fn", self.expr(x, cx)),
            ExprKind::DgCap { d, idx, cap, .. } => {
                let v = format!("({}).env.f{}.c{}", self.expr(d, cx), idx + 1, cap);
                if self.p.boxed_cap(&e.ty) {
                    let ct = self.cty(&e.ty);
                    format!("({{ {ct} u_; memcpy(&u_, ({v}).p, sizeof u_); u_; }})")
                } else {
                    v
                }
            }
            ExprKind::Int(n) => {
                if e.ty == Ty::Float {
                    Self::flit(*n as f64)
                } else {
                    Self::ilit(*n)
                }
            }
            ExprKind::Float(f) => Self::flit(*f),
            ExprKind::Bool(b) => b.to_string(),
            ExprKind::Null => "SIJO_NULL".into(),
            ExprKind::Str(s) => Self::str_lit(s),
            ExprKind::Interp(parts) => self.interp_str(parts, cx),
            ExprKind::EnumVal(_, v) => Self::ilit(*v),
            ExprKind::Const(k) => format!("K{}_{}", k, cid(&self.p.consts[*k].name)),
            ExprKind::Local(l) => match cx.cursors.get(l) {
                Some((a, rv)) => format!("w->{}.ent[{}]", self.aname(*a), rv),
                None => self.local_ref(cx, *l),
            },
            ExprKind::SelfRef => self.self_handle(cx),
            ExprKind::StructSelf => {
                if self.p.funcs[cx.fid].writes_self || matches!(self.p.funcs[cx.fid].kind, FuncKind::StructCtor(_)) {
                    "(*self)".into()
                } else {
                    "self".into()
                }
            }
            ExprKind::Field { base, sid, idx } => {
                let b = self.expr(base, cx);
                format!("({}).{}", b, cid(&self.p.structs[*sid].fields[*idx].name))
            }
            ExprKind::EField { base, field } => self.efield_read(base, *field, cx),
            ExprKind::Unary(op, x) => {
                let v = self.expr(x, cx);
                match op {
                    UnOp::Neg if x.ty == Ty::Int => self.at(format!("sijo_neg32({})", v)),
                    UnOp::Neg => format!("(-{})", paren(&v)),
                    UnOp::Not => format!("(!{})", paren(&v)),
                    UnOp::BitNot => format!("(~{})", paren(&v)),
                    UnOp::Plus => v,
                }
            }
            ExprKind::Binary(op, a, b) => {
                if let (BinOp::Eq | BinOp::Ne, Some(hv)) = (op, cx.hoisted.get(&(e.span.lo, e.span.hi)).cloned()) {
                    // cursor == loop-invariant handle: compare rows (see invariant_handle_uses)
                    let cur = [a, b].into_iter().find_map(|x| match &x.kind {
                        ExprKind::Local(l) => cx.cursors.get(l).map(|(_, rv)| rv.clone()),
                        _ => None,
                    });
                    if let Some(rv) = cur {
                        return format!("({} {} {})", rv, if *op == BinOp::Ne { "!=" } else { "==" }, hv);
                    }
                }
                self.binary(*op, a, b, cx)
            }
            ExprKind::Assign { op, target, value } => {
                let code = self.assign(*op, target, value, cx);
                match self.local_clamp(cx, target) {
                    Some(cl) => format!("({{ (void)({code}); {cl} {}; }})", self.local_ref(cx, local_of(target))),
                    None => code,
                }
            }
            ExprKind::IncDec { target, inc, prefix } => match self.local_clamp(cx, target) {
                Some(cl) => {
                    let n = self.local_ref(cx, local_of(target));
                    let code = self.incdec(target, *inc, true, cx);
                    if *prefix {
                        format!("({{ (void)({code}); {cl} {n}; }})")
                    } else {
                        let (t, ty) = (self.t(), self.cty(&target.ty));
                        format!("({{ {ty} {t} = {n}; (void)({code}); {cl} {t}; }})")
                    }
                }
                None => self.incdec(target, *inc, *prefix, cx),
            },
            ExprKind::Cond(c, a, b) => {
                let (c, a, b) = (self.expr(c, cx), self.expr(a, cx), self.expr(b, cx));
                format!("({} ? {} : {})", c, a, b)
            }
            ExprKind::Cast(x) => {
                let v = self.expr(x, cx);
                match (&x.ty, &e.ty) {
                    (Ty::Float, Ty::Char) => self.at(format!("sijo_to_char(sijo_f2i({}))", v)),
                    (Ty::Int, Ty::Char) => self.at(format!("sijo_to_char({})", v)),
                    (Ty::Float, Ty::Int) => self.at(format!("sijo_f2i({})", v)),
                    (_, Ty::Float) => format!("((float){})", paren(&v)),
                    _ => format!("((int32_t){})", paren(&v)),
                }
            }
            ExprKind::Call { func, recv, args, dispatch } => self.call(*func, recv.as_deref(), args, *dispatch, e, cx),
            ExprKind::OpCall { func, args } => {
                let items: Vec<(&Expr, String)> = args.iter().map(|a| (a, self.expr(a, cx))).collect();
                let (pre, vals) = self.ordered_args(items);
                Self::wrap(pre, format!("{}(w, {})", self.func_name(*func), vals.join(", ")))
            }
            ExprKind::StructLit { sid, ctor, args, inits } => {
                let sn = self.sname(*sid);
                let base = match ctor {
                    Some(c) => {
                        let items: Vec<(&Expr, String)> = args.iter().map(|a| (&a.expr, self.expr(&a.expr, cx))).collect();
                        let (pre, vals) = self.ordered_args(items);
                        let mut call_args = vec!["w".to_string()];
                        call_args.extend(vals);
                        Self::wrap(pre, format!("{}({})", self.func_name(*c), call_args.join(", ")))
                    }
                    None => {
                        // pure initializers: a single compound literal (defaults overlaid with inits)
                        if inits.iter().all(|(_, x)| crate::opt::is_pure_expr(x)) {
                            let fields = &self.p.structs[*sid].fields;
                            let defaults = consteval::struct_default(self.p, *sid).ok();
                            let mut parts = vec![];
                            for (i, f) in fields.iter().enumerate() {
                                let v = match inits.iter().find(|(idx, _)| *idx == i) {
                                    Some((_, x)) => self.expr(x, cx),
                                    None => match &defaults {
                                        Some(ConstVal::Struct(_, vals)) => self.cval(&vals[i]),
                                        _ => self.default_of(&f.ty),
                                    },
                                };
                                parts.push(format!(".{} = {}", cid(&f.name), v));
                            }
                            return format!("(({}){{ {} }})", sn, parts.join(", "));
                        }
                        self.default_of(&Ty::Struct(*sid))
                    }
                };
                if inits.is_empty() {
                    return base;
                }
                let t = self.t();
                let mut o = format!("({{ {sn} {t} = {base}; ");
                for (idx, x) in inits {
                    let v = self.expr(x, cx);
                    let _ = write!(o, "{t}.{} = {}; ", cid(&self.p.structs[*sid].fields[*idx].name), v);
                }
                let _ = write!(o, "{t}; }})");
                o
            }
            ExprKind::Spawn { class, inits, .. } => self.spawn(*class, inits, cx),
            ExprKind::Destroy(h) => {
                let hv = match h {
                    Some(h) => self.handle_of(h, cx),
                    None => self.self_handle(cx),
                };
                format!("sijo_send_destroy(w, {})", hv)
            }
            ExprKind::CompAdd { target, comp, inits } => self.comp_add(target, *comp, inits, cx),
            ExprKind::CompRemove { target, comp } => {
                let hv = if matches!(target.kind, ExprKind::SelfRef) { self.self_handle(cx) } else { self.handle_of(target, cx) };
                format!("sijo_send_crem(w, {}, {})", hv, self.comp_tag(*comp))
            }
            ExprKind::Count(c) => {
                let parts: Vec<String> = self
                    .p
                    .archetypes_of(*c)
                    .iter()
                    .map(|&a| match self.open_has(a, &self.query_needs(*c, &[]), "r_") {
                        // an open table: the rows with the component
                        Some(t) => format!("({{ int32_t n_ = 0; for (int32_t r_ = 0; r_ < w->{n}.count; r_++) n_ += {t}; n_; }})", n = self.aname(a)),
                        None => format!("w->{}.count", self.aname(a)),
                    })
                    .collect();
                if parts.is_empty() {
                    "0".into()
                } else {
                    format!("({})", parts.join(" + "))
                }
            }
            ExprKind::Is { expr, class, bind } => {
                let h = self.expr(expr, cx);
                let t = self.t();
                let bindc = bind.map(|b| format!("if (_ok{t}) {} = {t}; ", self.local_name(cx.fid, b))).unwrap_or_default();
                let test = match (cx.hoisted.get(&(e.span.lo, e.span.hi)), &expr.kind, cx.arch) {
                    (Some(v), _, _) => v.clone(), // tested once before the query loop
                    // Has<C>() / Get<C>() on this: the archetype the code is compiled for says
                    // shared code: the archetype of the view (or the bit, where an open table may say)
                    (None, ExprKind::SelfRef, Some(_)) if self.p.classes[*class].is_component && cx.shared.is_some() => {
                        if self.p.archetypes_of(*class).iter().any(|a| self.p.open.contains_key(a)) {
                            self.has_test(&t, *class)
                        } else {
                            format!("((({} >> V->arch) & 1) != 0)", self.mask(*class))
                        }
                    }
                    (None, ExprKind::SelfRef, Some(a)) if self.p.classes[*class].is_component => match self.p.open_bit(a, *class) {
                        // an open table: the row's bit
                        Some((flag, b)) => format!("((((uint32_t){}) >> {b}) & 1u)", self.self_place(cx, a, flag)),
                        None => self.p.is_subclass(a, *class).to_string(),
                    },
                    (None, _, _) => self.has_test(&t, *class),
                };
                format!("({{ SijoH {t} = {h}; bool _ok{t} = {test}; {bindc}_ok{t}; }})")
            }
            ExprKind::ListLambda { list, op, params, body, site } => self.lambda_op(e, list, *op, params, body, *site, cx),
            ExprKind::ArrayNew { site, len, elems, dims } => {
                let el = self.elem_cty(&self.p.alloc_sites[*site].elem);
                let ety = self.p.alloc_sites[*site].elem.clone();
                let heap = self.region_code(self.c.mem.sites[*site].region);
                let t = self.t();
                if !dims.is_empty() {
                    // new T[a, b(, c)]: the lengths in order (C#), then the elements in row-major order
                    let mut o = String::from("({ ");
                    let mut ds = vec![];
                    for (k, d) in dims.iter().enumerate() {
                        let v = self.expr(d, cx);
                        let _ = write!(o, "int32_t {t}d{k} = {v}; ");
                        ds.push(format!("{t}d{k}"));
                    }
                    ds.resize(3, "1".into());
                    let _ = write!(o, "SijoList {t} = SIJO_AT(sijo_md_new({}, {heap}, (int32_t)sizeof({el}), {}, {}, {})); ", self.arena(), ds[0], ds[1], ds[2]);
                    if elems.is_empty() {
                        let zero = matches!(ety, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::NHandle(_) | Ty::Handle(_));
                        if !zero {
                            let _ = write!(o, "{{ {el} {t}v = {}; for (int32_t {t}i = 0; {t}i < {t}.len; {t}i++) (({el}*){t}.data)[{t}i] = {t}v; }} ", self.default_of(&ety));
                        }
                    }
                    for (i, x) in elems.iter().enumerate() {
                        let v = if x.ty.is_list() { self.list_src(x, cx) } else { self.expr(x, cx) };
                        let _ = write!(o, "(({el}*){t}.data)[{i}] = {v}; ");
                    }
                    let _ = write!(o, "{t}; }})");
                    return o;
                }
                match len {
                    Some(n) => {
                        // new T[n]: n default elements (zero bits for numbers, chars, bools, enums, handles)
                        let nv = self.expr(n, cx);
                        let zero = matches!(ety, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::NHandle(_) | Ty::Handle(_));
                        let fill = if zero { String::new() } else { format!("{{ {el} {t}d = {}; for (int32_t {t}i = 0; {t}i < {t}.len; {t}i++) (({el}*){t}.data)[{t}i] = {t}d; }} ", self.default_of(&ety)) };
                        format!("({{ SijoList {t} = SIJO_AT(sijo_array_new({}, {heap}, (int32_t)sizeof({el}), {nv})); {fill}{t}; }})", self.arena())
                    }
                    None => {
                        // new T[] { a, b }: the elements in order
                        let mut o = format!("({{ SijoList {t} = sijo_array_new({}, {heap}, (int32_t)sizeof({el}), {}); ", self.arena(), elems.len());
                        for (i, x) in elems.iter().enumerate() {
                            // a collection element moves into the array (the move checker treats it as a move)
                            let v = if x.ty.is_list() { self.list_src(x, cx) } else { self.expr(x, cx) };
                            let _ = write!(o, "(({el}*){t}.data)[{i}] = {v}; ");
                        }
                        let _ = write!(o, "{t}; }})");
                        o
                    }
                }
            }
            ExprKind::ListNew { site } => {
                let el = self.elem_cty(&self.p.alloc_sites[*site].elem);
                let sp = &self.c.mem.sites[*site];
                let heap = self.region_code(sp.region);
                let cap = self.cap_code(&sp.reserve);
                format!("sijo_list_new({}, {}, (int32_t)sizeof({}), {})", self.arena(), heap, el, cap)
            }
            ExprKind::ListClone { list, site } => {
                let el = self.cty(&self.p.alloc_sites[*site].elem);
                let heap = self.region_code(self.c.mem.sites[*site].region);
                let l = self.expr(list, cx);
                let t = self.t();
                let lt = list.ty.clone();
                // dictionaries (entries + index), multi-dimensional arrays (+ lengths) and nested
                // collections copy through clone_code
                if lt.is_dict() || matches!(lt, Ty::MdArray(..)) || self.p.elem_of(&lt).map(|x| x.is_list()).unwrap_or(false) {
                    return self.clone_code(&lt, &l, &self.arena().to_string(), &heap, 0);
                }
                format!("({{ SijoList {t} = {l}; sijo_list_clone({}, {heap}, &{t}, (int32_t)sizeof({el})); }})", self.arena())
            }
            ExprKind::ListLen(l) => {
                let v = self.expr(l, cx);
                format!("({}).len", v)
            }
            ExprKind::ListIndex { list, index, checked } => {
                if let ExprKind::MdIndex { idx, checked } = &index.kind {
                    return self.md_index_value(e, list, idx, checked, cx);
                }
                let l = self.expr(list, cx);
                let i = self.expr(index, cx);
                let et = self.cty(&e.ty);
                if *checked {
                    format!("(({et}*)({l}).data)[SIJO_IDX({i}, ({l}).len)]")
                } else {
                    format!("(({et}*)({l}).data)[{i}]")
                }
            }
            // only reached for side effects (an element of a dead field): the indexes in order
            ExprKind::MdIndex { idx, .. } => {
                let parts: Vec<String> = idx.iter().map(|x| format!("(void)({});", self.expr(x, cx))).collect();
                format!("({{ {} 0; }})", parts.join(" "))
            }
            ExprKind::ListCall { list, op, args } => self.list_call(list, *op, args, cx),
            ExprKind::Builtin { f, args, site } => self.builtin(*f, args, *site, cx),
            ExprKind::DictGet { dict, key } => {
                let (kind, kt, et) = self.dict_info(&dict.ty);
                let d = self.expr(dict, cx);
                let k = self.expr(key, cx);
                let t = self.t();
                format!("({{ SijoList {t}d = {d}; {kt} {t}k = {k}; int32_t {t}i = sijo_dict_find(&{t}d, {kind}, (int32_t)sizeof({et}), &{t}k); if ({t}i < 0) SIJO_AT(sijo_panic(\"dictionary: the given key was not present\")); (({et}*){t}d.data)[{t}i].Value; }})")
            }
            ExprKind::DictSet { dict, key, value } => self.dict_set(dict, key, value, cx),
            ExprKind::DictCall { dict, op, args } => self.dict_call(e, dict, *op, args, cx),
            ExprKind::Switch { tmp, scrut, arms, default } => {
                let assign = match tmp {
                    Some(t) => format!("{} = {}; ", self.local_name(cx.fid, *t), self.expr(scrut, cx)),
                    None => String::new(),
                };
                let mut acc = match default {
                    Some(d) => self.expr(d, cx),
                    None => format!("({{ SIJO_AT(sijo_panic(\"switch expression: no arm matches the value\")); ({}){{0}}; }})", self.cty(&e.ty)),
                };
                for (c, v) in arms.iter().rev() {
                    let cc = self.expr(c, cx);
                    let vv = self.expr(v, cx);
                    acc = format!("({} ? {} : {})", cc, vv, acc);
                }
                if assign.is_empty() {
                    acc
                } else {
                    format!("({{ {assign}{acc}; }})")
                }
            }
        }
    }

    /// C statements freeing the collection at `place` and every collection inside it
    pub(crate) fn free_code(&self, ty: &Ty, place: &str, depth: usize) -> String {
        match self.p.elem_of(ty) {
            Some(el) if el.is_list() && !ty.is_dict() => {
                let inner = self.free_code(&el, &format!("((SijoList*)F{depth}->data)[f{depth}]"), depth + 1);
                format!("{{ SijoList* F{depth} = &({place}); for (int32_t f{depth} = 0; f{depth} < F{depth}->len; f{depth}++) {{ {inner} }} sijo_list_free(F{depth}); }}")
            }
            _ => format!("sijo_list_free(&({place}));"),
        }
    }

    /// C expression: a deep copy of the collection `src` in `heap` (C expression: 0 frame, 1 heap,
    /// or `region`); the collections inside it are copied into the same region
    fn clone_code(&self, ty: &Ty, src: &str, arena: &str, heap: &str, depth: usize) -> String {
        let el = self.p.elem_of(ty).unwrap_or(Ty::Int);
        if ty.is_dict() {
            let (kind, _, et) = self.dict_info(ty);
            return format!("({{ SijoList S{depth} = {src}; sijo_dict_clone({arena}, {heap}, &S{depth}, {kind}, (int32_t)sizeof({et})); }})");
        }
        // a multi-dimensional array keeps its lengths
        let clone = if matches!(ty, Ty::MdArray(..)) { "sijo_md_clone" } else { "sijo_list_clone" };
        if !el.is_list() {
            let et = self.elem_cty(&el);
            return format!("({{ SijoList S{depth} = {src}; {clone}({arena}, {heap}, &S{depth}, (int32_t)sizeof({et})); }})");
        }
        let inner = self.clone_code(&el, &format!("((SijoList*)S{depth}.data)[c{depth}]"), arena, heap, depth + 1);
        format!("({{ SijoList S{depth} = {src}; SijoList C{depth} = {clone}({arena}, {heap}, &S{depth}, (int32_t)sizeof(SijoList)); for (int32_t c{depth} = 0; c{depth} < C{depth}.len; c{depth}++) ((SijoList*)C{depth}.data)[c{depth}] = {inner}; C{depth}; }})")
    }

    /// C code of a bottom-up stable merge sort of `n` elements of C type `et` in array `a`, using a
    /// frame-arena buffer; `cmp(x, y)` returns C code comparing two element lvalues (<= 0 keeps order)
    fn merge_sort_code(&mut self, et: &str, a: &str, n: &str, cmp: &dyn Fn(&str, &str) -> String) -> String {
        self.tmp += 1;
        let u = format!("ms{}", self.tmp);
        let c = cmp(&format!("{a}[{u}i]"), &format!("{a}[{u}j]"));
        format!(
            "{{ int32_t {u}n = {n}; {et}* {u}b = ({et}*)sijo_arena_alloc({ar}, (size_t)({u}n > 0 ? {u}n : 1) * sizeof({et}));
  for (int32_t {u}w = 1; {u}w < {u}n; {u}w *= 2) {{
    for (int32_t {u}lo = 0; {u}lo < {u}n - {u}w; {u}lo += 2 * {u}w) {{
      int32_t {u}mid = {u}lo + {u}w, {u}hi = {u}n - {u}lo > 2 * {u}w ? {u}lo + 2 * {u}w : {u}n, {u}i = {u}lo, {u}j = {u}mid, {u}k = {u}lo;
      while ({u}i < {u}mid && {u}j < {u}hi) {{ if (({c}) <= 0) {u}b[{u}k++] = {a}[{u}i++]; else {u}b[{u}k++] = {a}[{u}j++]; }}
      while ({u}i < {u}mid) {u}b[{u}k++] = {a}[{u}i++];
      while ({u}j < {u}hi) {u}b[{u}k++] = {a}[{u}j++];
      for ({u}k = {u}lo; {u}k < {u}hi; {u}k++) {a}[{u}k] = {u}b[{u}k];
    }}
  }} }}",
            ar = self.arena()
        )
    }

    /// C comparison (-1/0/1) of two keys with a built-in order (C#'s Comparer<T>.Default)
    fn key_cmp(t: &Ty, x: &str, y: &str) -> String {
        match t {
            Ty::Str => format!("sijo_str_cmp({x}, {y})"),
            Ty::Float => format!("sijo_cmp_f32(&({x}), &({y}))"),
            _ => format!("((({x}) > ({y})) - (({x}) < ({y})))"),
        }
    }

    /// a list/array operation with a lambda, inline: the parameters take each element in turn
    #[allow(clippy::too_many_arguments)]
    fn lambda_op(&mut self, e: &Expr, list: &Expr, op: LamOp, params: &[LocalId], body: &Expr, site: usize, cx: &mut FCtx) -> String {
        let el = self.p.elem_of(&list.ty).unwrap_or(Ty::Int);
        let et = self.cty(&el);
        let p0 = self.local_name(cx.fid, params[0]);
        let t = self.t();
        let b = self.expr(body, cx);
        let ar = self.arena();
        // read-only operations work on a copy of the header (the body cannot change the list: E0406)
        let lv = if op.mutates() { String::new() } else { self.expr(list, cx) };
        let each = |rev: bool, inner: &str| -> String {
            if rev {
                format!("for (int32_t {t}i = {t}l.len - 1; {t}i >= 0; {t}i--) {{ {p0} = (({et}*){t}l.data)[{t}i]; {inner} }}")
            } else {
                format!("for (int32_t {t}i = 0; {t}i < {t}l.len; {t}i++) {{ {p0} = (({et}*){t}l.data)[{t}i]; {inner} }}")
            }
        };
        let rt = self.cty(&e.ty);
        match op {
            LamOp::Exists => format!("({{ SijoList {t}l = {lv}; bool {t}r = false; {} {t}r; }})", each(false, &format!("if ({b}) {{ {t}r = true; break; }}"))),
            LamOp::All => format!("({{ SijoList {t}l = {lv}; bool {t}r = true; {} {t}r; }})", each(false, &format!("if (!({b})) {{ {t}r = false; break; }}"))),
            LamOp::Count => format!("({{ SijoList {t}l = {lv}; int32_t {t}r = 0; {} {t}r; }})", each(false, &format!("if ({b}) {t}r++;"))),
            LamOp::FindIndex | LamOp::FindLastIndex => {
                let rev = op == LamOp::FindLastIndex;
                format!("({{ SijoList {t}l = {lv}; int32_t {t}r = -1; {} {t}r; }})", each(rev, &format!("if ({b}) {{ {t}r = {t}i; break; }}")))
            }
            LamOp::Find | LamOp::FindLast | LamOp::FirstOrDefault | LamOp::LastOrDefault | LamOp::First | LamOp::Last => {
                let rev = matches!(op, LamOp::FindLast | LamOp::LastOrDefault | LamOp::Last);
                let def = self.default_of(&e.ty);
                let must = matches!(op, LamOp::First | LamOp::Last);
                let check = if must { format!("if (!{t}f) SIJO_AT(sijo_panic(\"Sequence contains no matching element\")); ") } else { String::new() };
                format!(
                    "({{ SijoList {t}l = {lv}; {rt} {t}r = {def}; bool {t}f = false; {} {check}{t}r; }})",
                    each(rev, &format!("if ({b}) {{ {t}r = (({et}*){t}l.data)[{t}i]; {t}f = true; break; }}"))
                )
            }
            LamOp::Sum => {
                let add = if e.ty == Ty::Int { self.at(format!("sijo_add32({t}r, {t}v)")) } else { format!("{t}r + {t}v") };
                format!("({{ SijoList {t}l = {lv}; {rt} {t}r = 0; {} {t}r; }})", each(false, &format!("{rt} {t}v = {b}; {t}r = {add};")))
            }
            LamOp::Max | LamOp::Min => {
                // .NET: Max skips NaN unless every value is NaN; Min returns NaN as soon as one is NaN
                let better = match (op, &e.ty) {
                    (LamOp::Max, Ty::Float) => format!("{t}v > {t}r || {t}r != {t}r"),
                    (LamOp::Min, Ty::Float) => format!("{t}v < {t}r || {t}v != {t}v"),
                    (LamOp::Max, _) => format!("{t}v > {t}r"),
                    _ => format!("{t}v < {t}r"),
                };
                format!(
                    "({{ SijoList {t}l = {lv}; if (!{t}l.len) SIJO_AT(sijo_panic(\"Sequence contains no elements\")); {rt} {t}r = 0; {} {t}r; }})",
                    each(false, &format!("{rt} {t}v = {b}; if ({t}i == 0 || {better}) {t}r = {t}v;"))
                )
            }
            LamOp::Where | LamOp::Select => {
                let region = self.region_code(self.c.mem.sites[site].region);
                let (ut, push) = if op == LamOp::Where {
                    (et.clone(), format!("if ({b}) *({et}*)sijo_list_push({ar}, &{t}o, (int32_t)sizeof({et})) = {p0};"))
                } else {
                    let ut = self.cty(&self.p.elem_of(&e.ty).unwrap_or(Ty::Int));
                    (ut.clone(), format!("{ut} {t}v = {b}; *({ut}*)sijo_list_push({ar}, &{t}o, (int32_t)sizeof({ut})) = {t}v;"))
                };
                format!("({{ SijoList {t}l = {lv}; SijoList {t}o = sijo_list_new({ar}, {region}, (int32_t)sizeof({ut}), {t}l.len); {} {t}o; }})", each(false, &push))
            }
            LamOp::OrderBy | LamOp::OrderByDesc => {
                // keys computed once per element (in order), then a stable sort of positions by key
                let region = self.region_code(self.c.mem.sites[site].region);
                let kt = self.cty(&body.ty);
                let desc = op == LamOp::OrderByDesc;
                let kty = body.ty.clone();
                let sort = self.merge_sort_code("int32_t", &format!("{t}ix"), &format!("{t}l.len"), &|x, y| {
                    let c = Self::key_cmp(&kty, &format!("{t}k[{x}]"), &format!("{t}k[{y}]"));
                    if desc { format!("-({c})") } else { c }
                });
                format!(
                    "({{ SijoList {t}l = {lv}; {kt}* {t}k = ({kt}*)sijo_arena_alloc({ar}, (size_t)({t}l.len + 1) * sizeof({kt})); int32_t* {t}ix = (int32_t*)sijo_arena_alloc({ar}, (size_t)({t}l.len + 1) * sizeof(int32_t));
  {} {sort}
  SijoList {t}o = sijo_list_new({ar}, {region}, (int32_t)sizeof({et}), {t}l.len); {t}o.len = {t}l.len;
  for (int32_t {t}i = 0; {t}i < {t}l.len; {t}i++) (({et}*){t}o.data)[{t}i] = (({et}*){t}l.data)[{t}ix[{t}i]];
  {t}o; }})",
                    each(false, &format!("{t}k[{t}i] = {b}; {t}ix[{t}i] = {t}i;"))
                )
            }
            LamOp::Sort => {
                let p1 = self.local_name(cx.fid, params[1]);
                let lp = self.place(list, cx);
                let sort = self.merge_sort_code(&et, &format!("(({et}*){t}l->data)"), &format!("{t}l->len"), &|x, y| format!("({p0} = {x}, {p1} = {y}, ({b}))"));
                format!("({{ SijoList* {t}l = &{}; {sort} }})", paren(&lp))
            }
            LamOp::RemoveAll => {
                let lp = self.place(list, cx);
                format!(
                    "({{ SijoList* {t}l = &{}; int32_t {t}j = 0; for (int32_t {t}i = 0; {t}i < {t}l->len; {t}i++) {{ {p0} = (({et}*){t}l->data)[{t}i]; if (!({b})) (({et}*){t}l->data)[{t}j++] = {p0}; }} int32_t {t}r = {t}l->len - {t}j; {t}l->len = {t}j; {t}r; }})",
                    paren(&lp)
                )
            }
        }
    }

    /// can a value of this type point into a frame arena (strings, lists, values containing them)?
    fn holds_frame(&self, t: &Ty) -> bool {
        t.is_list() || self.p.contains_str(t)
    }

    /// may evaluating this expression allocate from the frame arena?
    fn expr_allocates(&self, e: &Expr) -> bool {
        let mut found = false;
        crate::sema::visit_expr(e, &mut |x| {
            found |= match &x.kind {
                ExprKind::Interp(_) | ExprKind::ListNew { .. } | ExprKind::ArrayNew { .. } | ExprKind::ListClone { .. } | ExprKind::DictSet { .. } => true,
                ExprKind::ListLambda { op, .. } => op.makes_list() || *op == LamOp::Sort,
                ExprKind::Binary(BinOp::Add, a, _) => a.ty == Ty::Str,
                ExprKind::Assign { op: Some(BinOp::Add), target, .. } => target.ty == Ty::Str,
                ExprKind::ListCall { op, .. } => matches!(op, ListOp::Add | ListOp::Insert),
                ExprKind::DictCall { op, .. } => *op == DictOp::Add,
                ExprKind::Builtin { f, .. } => matches!(
                    f,
                    Builtin::StrSub
                        | Builtin::StrSubFrom
                        | Builtin::StrUpper
                        | Builtin::StrLower
                        | Builtin::StrTrim
                        | Builtin::StrReplace
                        | Builtin::StrJoin
                        | Builtin::ToStr
                        | Builtin::StrRepeat
                        | Builtin::StrSplit
                        | Builtin::StrPadLeft
                        | Builtin::StrPadRight
                ),
                ExprKind::Call { func, .. } | ExprKind::OpCall { func, .. } => self.alloc_funcs.contains(func),
                ExprKind::StructLit { ctor: Some(c), .. } => self.alloc_funcs.contains(c),
                _ => false,
            };
        });
        found
    }

    /// the effects of a call as seen from the caller (union over the archetypes it may run on)
    fn call_effects(&self, func: FuncId, dispatch: Dispatch, recv: Option<&Expr>, arch: Option<ClassId>) -> crate::effects::Effects {
        let mut e = crate::effects::Effects::default();
        match dispatch {
            Dispatch::SelfCall | Dispatch::BaseCall => {
                if let Some(a) = arch {
                    let t = if dispatch == Dispatch::SelfCall { self.p.target_of(a, func) } else { func };
                    e.merge(&self.c.info.get(t, Some(a)));
                }
            }
            Dispatch::RemoteRead | Dispatch::RemoteDeferred | Dispatch::Rpc => {
                let class = recv.and_then(|r| r.ty.class_of()).unwrap_or(0);
                for b in self.p.archetypes_of(class) {
                    let g = self.p.target_of(b, func);
                    e.merge(&self.c.info.get(g, Some(b)));
                }
            }
            Dispatch::Static => e.merge(&self.c.info.get(func, None)),
        }
        e
    }

    /// Effects that let frame-arena memory outlive the code that allocated it: writes of strings or
    /// lists into entity fields (now or by deferred commands / messages), spawning entities that
    /// hold them, messages and RPCs carrying them.
    fn effects_keep_frame(&self, e: &crate::effects::Effects) -> bool {
        let field_holds = |f: &FieldId| self.holds_frame(&self.p.fields[*f].ty);
        e.self_writes.iter().any(field_holds)
            || e.remote_writes.keys().any(field_holds)
            || e.msg_writes.iter().any(field_holds)
            || e.spawns.iter().any(|&c| self.p.archetypes_of(c).into_iter().any(|a| self.p.all_fields(a).iter().any(field_holds)))
            || e.remote_calls.iter().chain(&e.rpcs).any(|&g| self.p.funcs[g].params.iter().any(|&l| self.holds_frame(&self.p.funcs[g].locals[l].ty)))
    }

    /// Can a loop give back the frame-arena memory of each iteration (rewind at the start of the
    /// next one and after the loop)? Only when nothing allocated in an iteration can be reached
    /// after it: no string/list value stored in a variable declared outside the body, an entity
    /// field, a message or an entity being spawned; no outer collection grows; no string/list is
    /// returned from inside; no await. Returns the C name of the mark when the loop also
    /// allocates (otherwise rewinding would only cost time).
    fn loop_rewind(&mut self, cx: &FCtx, body: &Block, extra: &[&Expr]) -> Option<String> {
        if cx.co_state.is_some() || block_has_await(body) {
            return None;
        }
        let mut allocates = extra.iter().any(|e| self.expr_allocates(e));
        crate::sema::visit_block(body, &mut |e| allocates |= self.expr_allocates(e));
        if !allocates {
            return None;
        }
        let mut inner: BTreeSet<LocalId> = BTreeSet::new();
        stmt_bound_locals(body, &mut inner);
        crate::sema::visit_block(body, &mut |e| match &e.kind {
            ExprKind::Is { bind: Some(b), .. } | ExprKind::Switch { tmp: Some(b), .. } => {
                inner.insert(*b);
            }
            _ => {}
        });
        // an atomic block's undo log may copy a collection declared outside the loop into this
        // iteration's frame memory, and `fail` puts the copy in its place
        if !cx.atomics.is_empty() || block_has_atomic(body) {
            let mut ch = BTreeMap::new();
            crate::effects::block_coll_changes(self.p, &self.c.info.summaries, &[cx.arch], body, &mut ch);
            for e in extra {
                crate::effects::expr_coll_changes(self.p, &self.c.info.summaries, &[cx.arch], e, &mut ch);
            }
            if ch.keys().any(|r| matches!(r, crate::effects::CollRoot::Local(l) if !inner.contains(l))) {
                return None;
            }
        }
        let prog = self.p;
        let locals = &prog.funcs[cx.fid].locals;
        // does writing this place keep frame memory past the iteration?
        let outlives = |t: &Expr, payload: bool| -> bool {
            let mut cur = t;
            loop {
                match &cur.kind {
                    ExprKind::Local(l) => return !inner.contains(l) && (payload || self.holds_frame(&locals[*l].ty)),
                    ExprKind::EField { field, .. } => return payload || self.holds_frame(&self.p.fields[*field].ty),
                    ExprKind::Field { base, .. } => cur = base,
                    ExprKind::ListIndex { list, .. } => cur = list,
                    _ => return payload,
                }
            }
        };
        let mut ok = true;
        let mut check = |e: &Expr| match &e.kind {
            ExprKind::Assign { target, value, .. } => {
                if outlives(target, false) && (self.holds_frame(&target.ty) || self.holds_frame(&value.ty)) {
                    ok = false;
                }
            }
            ExprKind::ListCall { list, op, args } => {
                let el = self.p.elem_of(&list.ty).unwrap_or(Ty::Void);
                match op {
                    // growing an outer list may move it into memory allocated after the mark
                    ListOp::Add | ListOp::Insert => {
                        let field_root = matches!(effects_root(list), Some(ExprKind::EField { .. }));
                        if outlives(list, true) && !(field_root && !self.holds_frame(&el)) {
                            ok = false;
                        }
                    }
                    ListOp::Fill => {
                        if self.holds_frame(&el) && outlives(list, true) {
                            ok = false;
                        }
                    }
                    _ => {}
                }
                let _ = args;
            }
            ExprKind::DictSet { dict, .. } | ExprKind::DictCall { dict, op: DictOp::Add, .. } => {
                let field_root = matches!(effects_root(dict), Some(ExprKind::EField { .. }));
                let entry_holds = self.p.elem_of(&dict.ty).map(|t| self.holds_frame(&t)).unwrap_or(true);
                if outlives(dict, true) && !(field_root && !entry_holds) {
                    ok = false;
                }
            }
            ExprKind::DictCall { op: DictOp::TryGet, args, .. } | ExprKind::Builtin { f: Builtin::TryParseInt | Builtin::TryParseFloat, args, .. } => {
                if let Some(x) = args.get(1) {
                    if self.holds_frame(&x.ty) && outlives(x, false) {
                        ok = false;
                    }
                }
            }
            ExprKind::Spawn { inits, .. } => {
                if inits.iter().any(|(_, x)| self.holds_frame(&x.ty)) {
                    ok = false;
                }
            }
            ExprKind::Call { func, recv, args, dispatch } => {
                for a in args {
                    if matches!(a.mode, ParamMode::Ref | ParamMode::Out) && self.holds_frame(&a.expr.ty) && outlives(&a.expr, false) {
                        ok = false;
                    }
                    if matches!(dispatch, Dispatch::RemoteDeferred | Dispatch::Rpc) && self.holds_frame(&a.expr.ty) {
                        ok = false;
                    }
                }
                if let Some(r) = recv {
                    if self.p.funcs[*func].writes_self && self.holds_frame(&r.ty) && outlives(r, false) {
                        ok = false;
                    }
                }
                if self.effects_keep_frame(&self.call_effects(*func, *dispatch, recv.as_deref(), cx.arch)) {
                    ok = false;
                }
            }
            ExprKind::OpCall { func, .. } => {
                if self.effects_keep_frame(&self.c.info.get(*func, None)) {
                    ok = false;
                }
            }
            ExprKind::StructLit { ctor: Some(c), .. } => {
                if self.effects_keep_frame(&self.c.info.get(*c, None)) {
                    ok = false;
                }
            }
            _ => {}
        };
        for e in extra {
            crate::sema::visit_expr(e, &mut check);
        }
        crate::sema::visit_block(body, &mut check);
        if !ok || block_returns_frame(body, &|t: &Ty| self.holds_frame(t)) {
            return None;
        }
        self.tmp += 1;
        Some(format!("am{}", self.tmp))
    }

    /// (before the loop, first statement of every iteration, after the loop)
    fn rewind_parts(&self, rw: &Option<String>, i: &str) -> (String, String, String) {
        match rw {
            Some(m) => {
                let a = self.arena();
                (
                    format!("{i}{{ SijoAMark {m} = sijo_arena_mark({a}); /* the loop gives back each iteration's frame memory */\n"),
                    format!("{i}  sijo_arena_rewind({a}, {m});\n"),
                    format!("{i}sijo_arena_rewind({a}, {m}); }}\n"),
                )
            }
            None => (String::new(), String::new(), String::new()),
        }
    }

    /// the C lvalue of a field of `this` (a local while the method keeps its row in locals)
    fn self_place(&self, cx: &FCtx, a: ClassId, f: FieldId) -> String {
        let lv = if cx.cache.contains(&f) { format!("sf{f}_") } else { format!("{}c{}[row].{}", self.self_tab(cx, a), self.comp(f), self.fname(f)) };
        self.fc_wrap(f, lv)
    }

    /// `--profile-fields`: the same lvalue, after counting one access of field `f` on this thread
    fn fc_wrap(&self, f: FieldId, lv: String) -> String {
        if self.field_counts {
            format!("(*(SIJO_FC({f}), &({lv})))")
        } else {
            lv
        }
    }

    /// Fields of `this` that a method instance may keep in locals: everything it reads or writes on
    /// its own row, when nothing else can touch that row while it runs (no call on `this` that
    /// accesses fields, no read of its own written fields through a handle, not a coroutine).
    fn row_cache_fields(&self, f: FuncId, arch: Option<ClassId>, view: bool) -> BTreeSet<FieldId> {
        let func = &self.p.funcs[f];
        let (Some(a), false, FuncKind::EntityMethod(_)) = (arch, view, func.kind) else { return BTreeSet::new() };
        if func.co.is_some() || func.extern_name.is_some() {
            return BTreeSet::new();
        }
        let eff = self.c.info.get(f, Some(a));
        if eff.self_writes.iter().any(|x| eff.remote_reads.contains(x)) {
            return BTreeSet::new();
        }
        let mut touches = false;
        crate::sema::visit_block(&func.body, &mut |e| {
            if let ExprKind::Call { func: g, dispatch: d @ (Dispatch::SelfCall | Dispatch::BaseCall), .. } = &e.kind {
                let t = if *d == Dispatch::SelfCall { self.p.target_of(a, *g) } else { *g };
                let s = self.c.info.get(t, Some(a));
                touches |= !s.self_reads.is_empty() || !s.self_writes.is_empty();
            }
        });
        if touches {
            return BTreeSet::new();
        }
        let present = self.p.all_fields(a);
        eff.self_reads
            .union(&eff.self_writes)
            .copied()
            .filter(|x| present.contains(x) && !self.c.ecs.is_dead(*x) && self.c.ecs.comp_of(*x).is_some())
            .collect()
    }

    /// Handle expressions that a query loop body compares with its cursor `var` (`other != Owner`) or
    /// tests with `is`, and that cannot change while the loop runs: a field of `this` that the body
    /// (and the methods it calls on `this`) never writes, or a local the body never assigns.
    /// Returns (span of the comparison / `is`, handle expression, class for `is`).
    fn invariant_handle_uses(&self, cx: &FCtx, var: LocalId, body: &Block) -> Vec<((u32, u32), Expr, Option<ClassId>)> {
        // a coroutine resume jumps into the body, past declarations made before the loop
        if block_has_await(body) {
            return vec![];
        }
        let mut wl: BTreeSet<LocalId> = BTreeSet::new();
        let mut ws: BTreeSet<FieldId> = BTreeSet::new();
        let mut calls: Vec<(FuncId, Dispatch)> = vec![];
        stmt_bound_locals(body, &mut wl);
        fn mark(t: &Expr, wl: &mut BTreeSet<LocalId>, ws: &mut BTreeSet<FieldId>) {
            match &t.kind {
                ExprKind::Local(l) => {
                    wl.insert(*l);
                }
                ExprKind::EField { base, field } => {
                    if matches!(base.kind, ExprKind::SelfRef) {
                        ws.insert(*field);
                    }
                }
                ExprKind::Field { base, .. } => mark(base, wl, ws),
                ExprKind::ListIndex { list, .. } => mark(list, wl, ws),
                _ => {}
            }
        }
        crate::sema::visit_block(body, &mut |e| match &e.kind {
            ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => mark(target, &mut wl, &mut ws),
            ExprKind::Call { func, args, dispatch, recv } => {
                for a in args {
                    if matches!(a.mode, ParamMode::Ref | ParamMode::Out) {
                        mark(&a.expr, &mut wl, &mut ws);
                    }
                }
                if let Some(r) = recv {
                    if self.p.funcs[*func].writes_self {
                        mark(r, &mut wl, &mut ws);
                    }
                }
                if matches!(dispatch, Dispatch::SelfCall | Dispatch::BaseCall) {
                    calls.push((*func, *dispatch));
                }
            }
            ExprKind::Is { bind: Some(b), .. } | ExprKind::Switch { tmp: Some(b), .. } => {
                wl.insert(*b);
            }
            ExprKind::Builtin { f: Builtin::TryParseInt | Builtin::TryParseFloat, args, .. } | ExprKind::DictCall { op: DictOp::TryGet, args, .. } => {
                if let Some(x) = args.get(1) {
                    mark(x, &mut wl, &mut ws);
                }
            }
            _ => {}
        });
        if let Some(a) = cx.arch {
            for (f, d) in calls {
                let target = if d == Dispatch::SelfCall { self.p.target_of(a, f) } else { f };
                ws.extend(self.c.info.get(target, Some(a)).self_writes);
            }
        }
        let locals = &self.p.funcs[cx.fid].locals;
        let invariant = |x: &Expr| -> bool {
            if !x.ty.is_handle() {
                return false;
            }
            match &x.kind {
                ExprKind::EField { base, field } => matches!(base.kind, ExprKind::SelfRef) && cx.arch.is_some() && !ws.contains(field),
                ExprKind::Local(l) => {
                    *l != var && !cx.cursors.contains_key(l) && !wl.contains(l) && matches!(locals[*l].kind, LocalKind::Normal | LocalKind::Param(ParamMode::Value))
                }
                _ => false,
            }
        };
        let is_cur = |x: &Expr| matches!(x.kind, ExprKind::Local(l) if l == var);
        let mut found: Vec<((u32, u32), Expr, Option<ClassId>)> = vec![];
        let mut count: HashMap<(u32, u32), usize> = HashMap::new();
        crate::sema::visit_block(body, &mut |e| {
            let key = (e.span.lo, e.span.hi);
            match &e.kind {
                ExprKind::Binary(op, a, b) => {
                    *count.entry(key).or_default() += 1;
                    if matches!(op, BinOp::Eq | BinOp::Ne) {
                        if is_cur(a) && invariant(b) {
                            found.push((key, (**b).clone(), None));
                        } else if is_cur(b) && invariant(a) {
                            found.push((key, (**a).clone(), None));
                        }
                    }
                }
                ExprKind::Is { expr, class, .. } => {
                    *count.entry(key).or_default() += 1;
                    if invariant(expr) {
                        found.push((key, (**expr).clone(), Some(*class)));
                    }
                }
                _ => {}
            }
        });
        // spans identify the expressions: skip synthetic or shared ones
        found.retain(|(k, _, _)| *k != (0, 0) && count.get(k) == Some(&1));
        found
    }

    fn region_code(&self, r: Region) -> String {
        match r {
            Region::Frame => "0".into(),
            Region::Heap => "1".into(),
            Region::Caller => "region".into(),
        }
    }

    fn cap_code(&self, caps: &[Cap]) -> String {
        if caps.is_empty() {
            return "0".into();
        }
        let parts: Vec<String> = caps
            .iter()
            .map(|c| match c {
                Cap::Const(n) => n.to_string(),
                Cap::Count(cl) => {
                    let ps: Vec<String> = self.p.archetypes_of(*cl).iter().map(|&a| format!("w->{}.count", self.aname(a))).collect();
                    if ps.is_empty() {
                        "0".into()
                    } else {
                        ps.join(" + ")
                    }
                }
            })
            .collect();
        format!("({})", parts.join(" + "))
    }

    fn efield_read(&mut self, base: &Expr, field: FieldId, cx: &mut FCtx) -> String {
        if self.c.ecs.is_dead(field) {
            return self.default_of(&self.p.fields[field].ty);
        }
        let k = self.comp(field);
        let fname = self.fname(field);
        let lv = match &base.kind {
            ExprKind::SelfRef => {
                let a = cx.arch.expect("self outside entity method");
                if cx.cache.contains(&field) {
                    format!("sf{field}_")
                } else {
                    let arr = if cx.view { format!("c{}_read", k) } else { format!("c{}", k) };
                    format!("{}{}[row].{}", self.self_tab(cx, a), arr, fname)
                }
            }
            ExprKind::Local(l) if cx.cursors.contains_key(l) => {
                let (a, rv) = cx.cursors[l].clone();
                format!("w->{}.c{}_read[{}].{}", self.aname(a), k, rv, fname)
            }
            _ => {
                let h = self.handle_of(base, cx);
                format!("acc_{}(w, {})->{}", self.ccomp(k), h, fname)
            }
        };
        self.fc_wrap(field, lv)
    }

    /// (archetype, row expression) if the expression statically denotes a table row
    fn static_row(&self, e: &Expr, cx: &FCtx) -> Option<(ClassId, String)> {
        match &e.kind {
            // shared code: the table of `this` is known at run time only
            ExprKind::SelfRef if cx.shared.is_some() => None,
            ExprKind::SelfRef => cx.arch.map(|a| (a, "row".to_string())),
            ExprKind::Local(l) => cx.cursors.get(l).cloned(),
            _ => None,
        }
    }

    fn binary(&mut self, op: BinOp, a: &Expr, b: &Expr, cx: &mut FCtx) -> String {
        // `o == this` between query cursors / self: compare rows, or fold when archetypes differ
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            if let (Some((aa, ar)), Some((ba, br))) = (self.static_row(a, cx), self.static_row(b, cx)) {
                let neg = op == BinOp::Ne;
                return if aa != ba {
                    (if neg { "true" } else { "false" }).to_string()
                } else {
                    format!("({} {} {})", ar, if neg { "!=" } else { "==" }, br)
                };
            }
        }
        let av = self.expr(a, cx);
        let bv = self.expr(b, cx);
        if matches!(op, BinOp::And | BinOp::Or) {
            // short-circuit: the right side must only run when needed
            return format!("({} {} {})", av, op.sym(), bv);
        }
        let (pre, v) = self.ordered_args(vec![(a, av), (b, bv)]);
        let (x, y) = (v[0].clone(), v[1].clone());
        let handles = a.ty.is_handle() || b.ty.is_handle() || a.ty == Ty::Null || b.ty == Ty::Null;
        if a.ty == Ty::Str {
            let code = match op {
                BinOp::Add => format!("sijo_str_cat({}, {}, {})", self.arena(), x, y),
                BinOp::Eq => format!("sijo_str_eq({}, {})", x, y),
                BinOp::Ne => format!("(!sijo_str_eq({}, {}))", x, y),
                _ => format!("(sijo_panic(\"bad string operator\"), {})", x),
            };
            return Self::wrap(pre, code);
        }
        let code = match op {
            BinOp::Eq | BinOp::Ne if handles => {
                let neg = if op == BinOp::Ne { "!" } else { "" };
                if a.ty == Ty::Null {
                    format!("({}sijo_hnull({}))", neg, y)
                } else if b.ty == Ty::Null {
                    format!("({}sijo_hnull({}))", neg, x)
                } else {
                    format!("({}sijo_heq({}, {}))", neg, x, y)
                }
            }
            BinOp::Div if a.ty == Ty::Int => match b.kind {
                ExprKind::Int(n) if n != 0 && n != -1 => format!("({} / {})", x, y),
                _ => format!("SIJO_AT(sijo_div({}, {}))", x, y),
            },
            BinOp::Rem if a.ty == Ty::Int => match b.kind {
                ExprKind::Int(n) if n != 0 && n != -1 => format!("({} % {})", x, y),
                _ => format!("SIJO_AT(sijo_rem({}, {}))", x, y),
            },
            BinOp::Rem => format!("fmodf({}, {})", x, y),
            BinOp::Shl => format!("sijo_shl({}, {})", x, y),
            BinOp::Shr => format!("sijo_shr({}, {})", x, y),
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor if a.ty == Ty::Bool => format!("((bool)({} {} {}))", x, op.sym(), y),
            BinOp::Add | BinOp::Sub | BinOp::Mul if a.ty == Ty::Int && b.ty == Ty::Int => self.at(format!("{}({}, {})", wrap_fn(op), x, y)),
            _ => format!("({} {} {})", x, op.sym(), y),
        };
        Self::wrap(pre, code)
    }

    /// compute `cur op v` for primitive compound assignment
    fn compound(&self, op: BinOp, ty: &Ty, cur: &str, v: &str) -> String {
        if *ty == Ty::Str {
            return format!("sijo_str_cat({}, {}, {})", self.arena(), cur, v);
        }
        match op {
            BinOp::Div if *ty == Ty::Int => format!("SIJO_AT(sijo_div({}, {}))", cur, v),
            BinOp::Rem if *ty == Ty::Int => format!("SIJO_AT(sijo_rem({}, {}))", cur, v),
            BinOp::Rem => format!("fmodf({}, {})", cur, v),
            BinOp::Shl => format!("sijo_shl({}, {})", cur, v),
            BinOp::Shr => format!("sijo_shr({}, {})", cur, v),
            BinOp::Add | BinOp::Sub | BinOp::Mul if *ty == Ty::Int => self.at(format!("{}({}, {})", wrap_fn(op), cur, v)),
            _ => format!("({} {} {})", cur, op.sym(), v),
        }
    }

    /// (entity field, is_self, base handle expr, path of struct member names) of an entity-field place
    fn efield_path<'e>(&self, e: &'e Expr) -> Option<(FieldId, &'e Expr, Vec<(StructId, usize)>)> {
        match &e.kind {
            ExprKind::EField { base, field } => Some((*field, base, vec![])),
            ExprKind::Field { base, sid, idx } => {
                let (f, b, mut path) = self.efield_path(base)?;
                path.push((*sid, *idx));
                Some((f, b, path))
            }
            _ => None,
        }
    }

    /// the row-major position of `a[i, j(, k)]` among the elements of the multi-dimensional array
    /// whose header is the C expression `l` (evaluated more than once: a pure place or a
    /// temporary); `ix` are the indexes as C expressions. Each index is checked against its
    /// dimension unless the optimizer proved it in range; in range, the position cannot overflow
    /// (it is below the element count).
    fn md_pos(l: &str, ix: &[String], checked: &[bool]) -> String {
        let at = |d: usize| if checked[d] { format!("SIJO_IDX({}, ({l}).d{d})", ix[d]) } else { format!("({})", ix[d]) };
        let mut pos = at(0);
        for d in 1..ix.len() {
            pos = format!("({pos} * ({l}).d{d} + {})", at(d));
        }
        pos
    }

    /// the value of `a[i, j(, k)]`: inline when the array is a variable or a field of this entity
    /// and the indexes have no side effects, else through temporaries (array, then the indexes
    /// left to right, as C# evaluates them)
    fn md_index_value(&mut self, e: &Expr, list: &Expr, idx: &[Expr], checked: &[bool], cx: &mut FCtx) -> String {
        let et = self.cty(&e.ty);
        let simple = match &list.kind {
            ExprKind::Local(_) => true,
            ExprKind::EField { base, .. } => matches!(base.kind, ExprKind::SelfRef),
            _ => false,
        };
        if simple && idx.iter().all(crate::opt::is_pure_expr) {
            let l = self.expr(list, cx);
            let ix: Vec<String> = idx.iter().map(|x| self.expr(x, cx)).collect();
            return format!("(({et}*)({l}).data)[{}]", Self::md_pos(&l, &ix, checked));
        }
        let t = self.t();
        let l = self.expr(list, cx);
        let mut o = format!("({{ SijoList {t}l = {l}; ");
        let mut ix = vec![];
        for (d, x) in idx.iter().enumerate() {
            let v = self.expr(x, cx);
            let _ = write!(o, "int32_t {t}i{d} = {v}; ");
            ix.push(format!("{t}i{d}"));
        }
        let _ = write!(o, "(({et}*){t}l.data)[{}]; }})", Self::md_pos(&format!("{t}l"), &ix, checked));
        o
    }

    /// `place` with the list index evaluated into a temporary first (code appended to `pre`)
    fn place_hoisted(&mut self, e: &Expr, cx: &mut FCtx, pre: &mut String) -> String {
        match &e.kind {
            ExprKind::Field { base, sid, idx } => format!("({}).{}", self.place_hoisted(base, cx, pre), cid(&self.p.structs[*sid].fields[*idx].name)),
            ExprKind::ListIndex { list, index, checked } => {
                if let ExprKind::MdIndex { idx, checked } = &index.kind {
                    let mut ix = vec![];
                    for x in idx {
                        let v = self.expr(x, cx);
                        let t = self.t();
                        let _ = write!(pre, "int32_t {t} = {v}; ");
                        ix.push(t);
                    }
                    let l = self.place(list, cx);
                    let et = self.cty(&e.ty);
                    return format!("(({et}*)({l}).data)[{}]", Self::md_pos(&l, &ix, checked));
                }
                let i = self.expr(index, cx);
                let t = self.t();
                let _ = write!(pre, "int32_t {t} = {i}; ");
                let l = self.place(list, cx);
                let et = self.cty(&e.ty);
                if *checked {
                    format!("(({et}*)({l}).data)[SIJO_IDX({t}, ({l}).len)]")
                } else {
                    format!("(({et}*)({l}).data)[{t}]")
                }
            }
            _ => self.place(e, cx),
        }
    }

    fn place(&mut self, e: &Expr, cx: &mut FCtx) -> String {
        // writable C lvalue for non-remote places
        match &e.kind {
            ExprKind::EField { base, field } if matches!(base.kind, ExprKind::SelfRef) => {
                let a = cx.arch.unwrap();
                self.self_place(cx, a, *field)
            }
            ExprKind::Field { base, sid, idx } => format!("({}).{}", self.place(base, cx), cid(&self.p.structs[*sid].fields[*idx].name)),
            ExprKind::ListIndex { list, index, checked } => {
                if let ExprKind::MdIndex { idx, checked } = &index.kind {
                    let l = self.place(list, cx);
                    let et = self.cty(&e.ty);
                    if idx.iter().all(crate::opt::is_pure_expr) {
                        let ix: Vec<String> = idx.iter().map(|x| self.expr(x, cx)).collect();
                        return format!("(({et}*)({l}).data)[{}]", Self::md_pos(&l, &ix, checked));
                    }
                    // indexes with side effects: evaluated once each, in order
                    let t = self.t();
                    let mut o = String::from("(*({ ");
                    let mut ix = vec![];
                    for (d, x) in idx.iter().enumerate() {
                        let v = self.expr(x, cx);
                        let _ = write!(o, "int32_t {t}i{d} = {v}; ");
                        ix.push(format!("{t}i{d}"));
                    }
                    let _ = write!(o, "&(({et}*)({l}).data)[{}]; }}))", Self::md_pos(&l, &ix, checked));
                    return o;
                }
                let l = self.place(list, cx);
                let i = self.expr(index, cx);
                let et = self.cty(&e.ty);
                if *checked {
                    format!("(({et}*)({l}).data)[SIJO_IDX({i}, ({l}).len)]")
                } else {
                    format!("(({et}*)({l}).data)[{i}]")
                }
            }
            _ => self.expr(e, cx),
        }
    }

    fn assign(&mut self, op: Option<BinOp>, target: &Expr, value: &Expr, cx: &mut FCtx) -> String {
        if let Some((field, _)) = crate::effects::target_field(target) {
            if self.c.ecs.is_dead(field) {
                return self.side_effects_only(&[target, value], cx);
            }
        }
        if let Some((field, base, path)) = self.efield_path(target) {
            let is_self = matches!(base.kind, ExprKind::SelfRef);
            if self.c.ecs.is_dead(field) {
                // storage eliminated: keep only side effects
                let v = self.expr(value, cx);
                return if crate::opt::is_pure_expr(value) { "((void)0)".into() } else { format!("((void)({}))", v) };
            }
            if is_self {
                return self.self_write(field, &path, op, target, value, cx);
            }
            return self.remote_write(field, base, &path, op, target, value, cx);
        }
        // locals, struct fields of locals, list elements
        if target.ty.is_list() {
            let p = self.place(target, cx);
            let src = self.list_src(value, cx);
            let t = self.t();
            let fr = self.free_code(&target.ty, &p, 0);
            return format!("({{ SijoList {t} = {src}; {fr} {p} = {t}; }})");
        }
        let ty = self.cty(&target.ty);
        // s += $"...{x}...": append each piece to s directly (numbers are formatted on the stack), so
        // that s can grow in place in the frame arena instead of copying itself every time
        if let (Some(BinOp::Add), Ty::Str, ExprKind::Interp(parts)) = (op, &target.ty, &value.kind) {
            if !has_list_index(target) {
                let p = self.place(target, cx);
                let (tp, acc) = (self.t(), self.t());
                let ar = self.arena();
                let mut o = format!("({{ SijoStr* {tp} = &{}; SijoStr {acc} = *{tp}; ", paren(&p));
                for part in parts {
                    match part {
                        IPart::Text(t) => {
                            if !t.is_empty() {
                                let _ = write!(o, "{acc} = sijo_str_cat({ar}, {acc}, {}); ", Self::str_lit(t));
                            }
                        }
                        IPart::Expr(x, spec) => {
                            let v = self.expr(x, cx);
                            let (k, pr) = spec.as_ref().map(|s| fmt_spec(s)).unwrap_or((0, -1));
                            let piece = match &x.ty {
                                Ty::Int => format!("sijo_str_cat_i32({ar}, {acc}, {v}, {k}, {pr})"),
                                Ty::Float => format!("sijo_str_cat_f32({ar}, {acc}, {v}, {k}, {pr})"),
                                Ty::Str => format!("sijo_str_cat({ar}, {acc}, {v})"),
                                Ty::Bool => format!("sijo_str_cat({ar}, {acc}, sijo_str_of_bool({v}))"),
                                Ty::Char => format!("sijo_str_cat({ar}, {acc}, sijo_str_of_char({ar}, {v}))"),
                                Ty::Enum(e) => format!("sijo_str_cat({ar}, {acc}, sijo_str_of_cstr(sijo_enum_{}({v})))", cid(&self.p.enums[*e].name)),
                                Ty::Handle(_) | Ty::NHandle(_) => format!("sijo_str_cat_i32({ar}, {acc}, (int32_t)({v}).idx, 0, -1)"),
                                _ => acc.clone(),
                            };
                            let _ = write!(o, "{acc} = {piece}; ");
                        }
                    }
                }
                let _ = write!(o, "*{tp} = {acc}; }})");
                return o;
            }
        }
        // a list element: C# evaluates the index, then the value, then stores. The index goes to a
        // temporary and the element address is computed after the value, which may grow the list
        if has_list_index(target) && (!crate::opt::is_pure_expr(value) || !index_pure(target)) {
            let mut pre = String::new();
            let p = self.place_hoisted(target, cx, &mut pre);
            let v = self.expr(value, cx);
            let (tc, tv) = (self.t(), self.t());
            return match op {
                None => format!("({{ {pre}{ty} {tv} = {v}; {p} = {tv}; {tv}; }})"),
                Some(o) => {
                    let vty = self.cty(&value.ty);
                    let c = self.compound(o, &target.ty, &tc, &tv);
                    format!("({{ {pre}{ty} {tc} = {p}; {vty} {tv} = {v}; {p} = {c}; }})")
                }
            };
        }
        let p = self.place(target, cx);
        let v = self.expr(value, cx);
        let Some(o) = op else { return format!("({} = {})", p, v) };
        let tp = self.t();
        if !crate::opt::is_pure_expr(value) {
            // C# evaluates `x op= f()` as: locate x, read x, evaluate f(), combine (C leaves this unsequenced)
            let (tc, tv) = (self.t(), self.t());
            let vty = self.cty(&value.ty);
            let c = self.compound(o, &target.ty, &tc, &tv);
            return format!("({{ {ty}* {tp} = &{}; {ty} {tc} = *{tp}; {vty} {tv} = {v}; *{tp} = {c}; }})", paren(&p));
        }
        if target.ty == Ty::Float && matches!(o, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div) {
            return format!("({} {}= {})", p, o.sym(), v);
        }
        // the place (e.g. a list element with a computed index) is evaluated once
        let c = self.compound(o, &target.ty, &format!("*{tp}"), &v);
        format!("({{ {ty}* {tp} = &{}; *{tp} = {c}; }})", paren(&p))
    }

    fn self_write(&mut self, field: FieldId, path: &[(StructId, usize)], op: Option<BinOp>, target: &Expr, value: &Expr, cx: &mut FCtx) -> String {
        let a = cx.arch.unwrap();
        let k = self.comp(field);
        let an = self.aname(a);
        let fty = self.cty(&self.p.fields[field].ty);
        let fplace = self.self_place(cx, a, field);
        let mut sub = fplace.clone();
        for (s, i) in path {
            sub = format!("{}.{}", sub, cid(&self.p.structs[*s].fields[*i].name));
        }
        let clamp = if path.is_empty() { self.clamp_code_at(field, &format!("(&{})", fplace), cx.fid, target.span) } else { self.clamp_code(field, &format!("(&{})", fplace)) };
        let dirty = if self.c.ecs.components[k].replicated { format!("{}d{}[row] = 1; ", self.self_tab(cx, a), k) } else { String::new() };
        let _ = an;
        let _ = fty;
        if target.ty.is_list() {
            let src = self.list_src(value, cx);
            let t = self.t();
            let fr = self.free_code(&target.ty, &sub, 0);
            return format!("({{ SijoList {t} = {src}; {fr} {sub} = {t}; }})");
        }
        let v = self.expr(value, cx);
        let assign = match op {
            None => format!("{} = {}", sub, v),
            Some(o) if !crate::opt::is_pure_expr(value) => {
                let (tc, tv) = (self.t(), self.t());
                let ty = self.cty(&target.ty);
                let vty = self.cty(&value.ty);
                let c = self.compound(o, &target.ty, &tc, &tv);
                format!("{ty} {tc} = {sub}; {vty} {tv} = {v}; {sub} = {c}")
            }
            Some(o) => {
                let c = self.compound(o, &target.ty, &sub, &v);
                format!("{} = {}", sub, c)
            }
        };
        let assign = if assign.contains("; ") && clamp.is_empty() && dirty.is_empty() {
            return format!("({{ {assign}; {sub}; }})");
        } else {
            assign
        };
        if clamp.is_empty() && dirty.is_empty() {
            format!("({})", assign)
        } else {
            format!("({{ {assign}; {clamp} {dirty}{sub}; }})")
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn remote_write(&mut self, field: FieldId, base: &Expr, path: &[(StructId, usize)], op: Option<BinOp>, target: &Expr, value: &Expr, cx: &mut FCtx) -> String {
        let h = self.handle_of(base, cx);
        let fty_s = self.cty(&self.p.fields[field].ty);
        let mut off = String::from("0");
        let mut prev = fty_s.clone();
        for (s, i) in path {
            let _ = s;
            off = format!("{} + offsetof({}, {})", off, prev, cid(&self.p.structs[*s].fields[*i].name));
            prev = self.cty(&self.p.structs[*s].fields[*i].ty);
        }
        let fid = field + 1;
        let v = self.expr(value, cx);
        let tty = self.cty(&target.ty);
        let ht = self.t();
        match (op, &target.ty) {
            (Some(BinOp::Add), Ty::Int) => format!("({{ SijoH {ht} = {h}; sijo_send_addi(w, {ht}, {fid}, {off}, {v}); }})"),
            (Some(BinOp::Sub), Ty::Int) => format!("({{ SijoH {ht} = {h}; sijo_send_addi(w, {ht}, {fid}, {off}, (int32_t)(0u - (uint32_t)({v}))); }})"),
            (Some(BinOp::Add), Ty::Float) => format!("({{ SijoH {ht} = {h}; sijo_send_addf(w, {ht}, {fid}, {off}, {v}); }})"),
            (Some(BinOp::Sub), Ty::Float) => format!("({{ SijoH {ht} = {h}; sijo_send_addf(w, {ht}, {fid}, {off}, -({v})); }})"),
            _ => {
                // SET of the new value computed from the start-of-system snapshot
                let nv = match op {
                    None => v,
                    Some(o) => {
                        let cur = self.expr(target, cx);
                        self.compound(o, &target.ty, &cur, &v)
                    }
                };
                let t = self.t();
                format!("({{ SijoH {ht} = {h}; {tty} {t} = {nv}; sijo_send_set(w, {ht}, {fid}, {off}, &{t}, (uint16_t)sizeof({t})); }})")
            }
        }
    }

    /// Evaluates only the parts of the expressions that have side effects (for eliminated storage).
    fn side_effects_only(&mut self, es: &[&Expr], cx: &mut FCtx) -> String {
        let mut parts = vec![];
        for e in es {
            let mut stack = vec![*e];
            while let Some(x) = stack.pop() {
                match &x.kind {
                    ExprKind::EField { base, .. } | ExprKind::Field { base, .. } => stack.push(base),
                    ExprKind::ListIndex { list, index, .. } => {
                        stack.push(list);
                        stack.push(index);
                    }
                    _ if !crate::opt::is_pure_expr(x) => parts.push(format!("(void)({});", self.expr(x, cx))),
                    _ => {}
                }
            }
        }
        if parts.is_empty() {
            "((void)0)".into()
        } else {
            format!("({{ {} }})", parts.join(" "))
        }
    }

    fn incdec(&mut self, target: &Expr, inc: bool, prefix: bool, cx: &mut FCtx) -> String {
        if let Some((field, _)) = crate::effects::target_field(target) {
            if self.c.ecs.is_dead(field) {
                return self.side_effects_only(&[target], cx);
            }
        }
        let one = if target.ty == Ty::Float { "1.0f" } else { "1" };
        if let Some((field, base, path)) = self.efield_path(target) {
            if self.c.ecs.is_dead(field) {
                return "((void)0)".into();
            }
            let lit = Expr::new(if target.ty == Ty::Float { ExprKind::Float(1.0) } else { ExprKind::Int(1) }, target.ty.clone(), target.span);
            let op = if inc { BinOp::Add } else { BinOp::Sub };
            if matches!(base.kind, ExprKind::SelfRef) {
                if prefix {
                    return self.self_write(field, &path, Some(op), target, &lit, cx);
                }
                let cur = self.expr(target, cx);
                let t = self.t();
                let ty = self.cty(&target.ty);
                let w = self.self_write(field, &path, Some(op), target, &lit, cx);
                return format!("({{ {ty} {t} = {cur}; {w}; {t}; }})");
            }
            return self.remote_write(field, base, &path, Some(op), target, &lit, cx);
        }
        let p = self.place(target, cx);
        let sym = if inc { "++" } else { "--" };
        let _ = one;
        if target.ty == Ty::Int {
            let f = if inc { "sijo_add32" } else { "sijo_sub32" };
            let tp = self.t();
            return if prefix {
                format!("({{ int32_t* {tp} = &{}; *{tp} = {f}(*{tp}, 1); }})", paren(&p))
            } else {
                format!("({{ int32_t* {tp} = &{}; int32_t {tp}o = *{tp}; *{tp} = {f}({tp}o, 1); {tp}o; }})", paren(&p))
            };
        }
        if prefix {
            format!("({}{})", sym, p)
        } else {
            format!("({}{})", p, sym)
        }
    }

    fn call(&mut self, f: FuncId, recv: Option<&Expr>, args: &[HArg], dispatch: Dispatch, e: &Expr, cx: &mut FCtx) -> String {
        let callee = &self.p.funcs[f];
        if let Some(name) = callee.extern_name.clone() {
            // a host C function: plain C arguments (strings as pointer + byte length). The host
            // cannot see range types, so the call site keeps their contracts: an argument for a
            // ranged parameter is saturated before the call, a ranged result after it.
            let items: Vec<(&Expr, String)> = args.iter().map(|a| (&a.expr, self.expr(&a.expr, cx))).collect();
            let ranges: Vec<Option<(f64, f64)>> = callee.params.iter().map(|&l| callee.locals[l].range).collect();
            let ret_range = callee.ret_range;
            let mut pre = String::new();
            let mut cargs = vec![];
            for (i, (x, v)) in items.into_iter().enumerate() {
                let t = self.t();
                let v = match ranges.get(i).copied().flatten() {
                    Some((lo, hi)) => self.saturate_code(&x.ty, &v, lo, hi),
                    None => v,
                };
                let _ = write!(pre, "{} {t} = {v}; ", self.cty(&x.ty));
                if x.ty == Ty::Str {
                    cargs.push(format!("{t}.p"));
                    cargs.push(format!("{t}.n"));
                } else {
                    cargs.push(t);
                }
            }
            let call = format!("{}({})", name, cargs.join(", "));
            let call = match ret_range {
                Some((lo, hi)) => self.saturate_code(&e.ty.clone(), &call, lo, hi),
                None => call,
            };
            return Self::wrap(pre, call);
        }
        // arguments
        let mut items: Vec<(&Expr, String, bool)> = vec![];
        // writes into self entity fields through ref/out or a mutating method bypass the normal
        // assignment path: re-apply range contracts and change tracking after the call
        let mut post = String::new();
        let self_arch = cx.arch;
        let cxs = cx.clone();
        let post_for = |g: &Gen, e: &Expr, post: &mut String| {
            if let ExprKind::Local(l) = e.kind {
                if let Some((lo, hi)) = g.p.funcs[cxs.fid].locals[l].range {
                    let n = g.local_ref(&cxs, l);
                    let v = match e.ty {
                        Ty::Int => format!("sijo_clampi({n}, {}, {})", lo.ceil() as i64, hi.floor() as i64),
                        _ => format!("sijo_clamprf({n}, {}, {})", Gen::flit(lo), Gen::flit(hi)),
                    };
                    let _ = write!(post, " {n} = {v};");
                }
            }
            if let Some((field, base, _)) = g.efield_path(e) {
                if matches!(base.kind, ExprKind::SelfRef) && !g.c.ecs.is_dead(field) {
                    if let Some(a) = self_arch {
                        let k = g.comp(field);
                        let place = g.self_place(&cxs, a, field);
                        post.push_str(&g.clamp_code(field, &format!("(&{})", place)));
                        if g.c.ecs.components[k].replicated {
                            let _ = write!(post, " {}d{}[row] = 1;", g.self_tab(&cxs, a), k);
                        }
                    }
                }
            }
        };
        for a in args {
            let code = match a.mode {
                ParamMode::Ref | ParamMode::Out => {
                    post_for(self, &a.expr, &mut post);
                    let pl = self.place(&a.expr, cx);
                    format!("&{}", paren(&pl))
                }
                // the callee takes the collection: steal it (the variable is left empty)
                ParamMode::Own => self.list_src(&a.expr, cx),
                _ => self.expr(&a.expr, cx),
            };
            let hoist = !matches!(a.mode, ParamMode::Ref | ParamMode::Out);
            items.push((&a.expr, code, hoist));
        }
        if let (Some(r), true) = (recv, callee.writes_self) {
            post_for(self, r, &mut post);
        }
        let (pre, mut vals) = self.ordered_args_ex(items);
        if callee.ret.is_list() {
            let r = self.c.mem.calls.get(&(cx.fid, e.span)).copied().unwrap_or(Region::Frame);
            vals.push(self.region_code(r));
        }
        let code = match dispatch {
            Dispatch::Static => match callee.kind {
                FuncKind::StructMethod(_) => {
                    let r = recv.unwrap();
                    let rc = if callee.writes_self {
                        let pl = self.place(r, cx);
                        format!("&{}", paren(&pl))
                    } else {
                        self.expr(r, cx)
                    };
                    let mut a = vec!["w".to_string(), rc];
                    a.extend(vals);
                    format!("{}({})", self.func_name(f), a.join(", "))
                }
                _ => {
                    let mut a = vec!["w".to_string()];
                    a.extend(vals);
                    format!("{}({})", self.func_name(f), a.join(", "))
                }
            },
            Dispatch::SelfCall | Dispatch::BaseCall => {
                let a = cx.arch.expect("self call outside entity method");
                let target = if dispatch == Dispatch::SelfCall { self.p.target_of(a, f) } else { f };
                let view = cx.view && !self.is_mutating_method(target);
                let mut args = self.inst_args(target, a, "row", cx.shared.is_some());
                args.extend(vals);
                format!("{}({})", self.method_name(target, a, view), args.join(", "))
            }
            Dispatch::RemoteRead => {
                let r = recv.unwrap();
                if let ExprKind::Local(l) = &r.kind {
                    if let Some((a, rv)) = cx.cursors.get(l).cloned() {
                        let target = self.p.target_of(a, f);
                        let mut args = self.inst_args(target, a, &rv, false);
                        args.extend(vals);
                        return Self::wrap(pre, format!("{}({})", self.method_name(target, a, true), args.join(", ")));
                    }
                }
                let h = self.handle_of(r, cx);
                let mut args = vec!["w".to_string(), h];
                args.extend(vals);
                format!("R{}({})", f, args.join(", "))
            }
            Dispatch::RemoteDeferred | Dispatch::Rpc => {
                // Rpc: same message format, but routed client -> server by the network layer
                let r = recv.unwrap();
                let h = self.handle_of(r, cx);
                let mut args = vec!["w".to_string(), h];
                args.extend(vals);
                format!("S{}({})", f, args.join(", "))
            }
        };
        let code = if post.trim().is_empty() {
            code
        } else if callee.ret == Ty::Void {
            format!("({{ {}; {} }})", code, post)
        } else {
            let t = self.t();
            format!("({{ {} {} = {}; {} {}; }})", self.cty(&callee.ret), t, code, post, t)
        };
        Self::wrap(pre, code)
    }

    fn spawn(&mut self, class: ClassId, inits: &[(FieldId, Expr)], cx: &mut FCtx) -> String {
        let an = self.aname(class);
        let t = self.t();
        let mut o = format!("({{ int32_t {t}r; SijoArch_{an}* {t}tb; SijoH {t} = sijo_spawn_{an}(w, &{t}r, &{t}tb); ");
        for (f, x) in inits {
            if self.c.ecs.is_dead(*f) {
                if !crate::opt::is_pure_expr(x) {
                    if x.ty.is_list() {
                        // a collection made for a field nobody reads: nobody owns it either
                        let v = self.list_src(x, cx);
                        let tmp = self.t();
                        let fr = self.free_code(&x.ty, &tmp, 0);
                        let _ = write!(o, "{{ SijoList {tmp} = {v}; {fr} }} ");
                    } else {
                        let v = self.expr(x, cx);
                        let _ = write!(o, "(void)({}); ", v);
                    }
                }
                continue;
            }
            let k = self.comp(*f);
            let place = format!("{t}tb->c{k}[{t}r].{}", self.fname(*f));
            if x.ty.is_list() {
                // replaces the field's initial collection (`int[] F = new int[4]` is allocated
                // when the row is created)
                let v = self.list_src(x, cx);
                let tmp = self.t();
                let fr = self.free_code(&x.ty, &place, 0);
                let _ = write!(o, "{{ SijoList {tmp} = {v}; {fr} {place} = {tmp}; }} ");
                continue;
            }
            let v = self.expr(x, cx);
            let _ = write!(o, "{place} = {v}; ");
            let cl = self.clamp_code_at(*f, &format!("(&{})", place), cx.fid, x.span);
            if !cl.is_empty() {
                let _ = write!(o, "{} ", cl);
            }
        }
        let _ = write!(o, "{t}; }})");
        o
    }

    fn list_call(&mut self, list: &Expr, op: ListOp, args: &[Expr], cx: &mut FCtx) -> String {
        let el = match &list.ty {
            Ty::List(t) | Ty::Array(t) => (**t).clone(),
            _ => Ty::Int,
        };
        let et = self.cty(&el);
        if let Some((field, _)) = crate::effects::target_field(list) {
            if self.c.ecs.is_dead(field) {
                let mut all: Vec<&Expr> = vec![list];
                all.extend(args.iter());
                let se = self.side_effects_only(&all, cx);
                return match op {
                    ListOp::Contains | ListOp::Remove => format!("({{ {}; false; }})", se),
                    ListOp::IndexOf => format!("({{ {}; -1; }})", se),
                    _ => se,
                };
            }
        }
        // self entity list fields are written through the current component
        let l = self.place(list, cx);
        // an element list (grid[i].Add(x)) that has no buffer yet grows in its outer list's region
        let adopt = match &list.kind {
            ExprKind::ListIndex { list: outer, .. } if matches!(op, ListOp::Add | ListOp::Insert) => {
                let o = self.place(outer, cx);
                format!("if (!({l}).cap) ({l}).heap = ({o}).heap; ")
            }
            _ => String::new(),
        };
        let nested = el.is_list();
        match op {
            ListOp::Add => {
                let v = if nested { self.list_src(&args[0], cx) } else { self.expr(&args[0], cx) };
                let t = self.t();
                format!("({{ {et} {t} = {v}; {adopt}*({et}*)sijo_list_push({}, &{}, (int32_t)sizeof({et})) = {t}; }})", self.arena(), paren(&l))
            }
            ListOp::Clear if nested => {
                let t = self.t();
                let fr = self.free_code(&el, &format!("((SijoList*){t}->data)[{t}i]"), 1);
                format!("({{ SijoList* {t} = &{}; for (int32_t {t}i = 0; {t}i < {t}->len; {t}i++) {{ {fr} }} {t}->len = 0; }})", paren(&l))
            }
            ListOp::Clear => format!("(({}).len = 0)", l),
            ListOp::RemoveAt if nested => {
                let i = self.expr(&args[0], cx);
                let t = self.t();
                let fr = self.free_code(&el, &format!("((SijoList*){t}->data)[{t}i]"), 1);
                format!("({{ SijoList* {t} = &{}; int32_t {t}i = SIJO_IDX({i}, {t}->len); {fr} sijo_list_remove_at({t}, {t}i, (int32_t)sizeof(SijoList)); }})", paren(&l))
            }
            ListOp::RemoveAt => {
                let i = self.expr(&args[0], cx);
                format!("SIJO_AT(sijo_list_remove_at(&{}, {}, (int32_t)sizeof({})))", paren(&l), i, et)
            }
            ListOp::Contains | ListOp::IndexOf | ListOp::Remove => {
                let v = self.expr(&args[0], cx);
                let t = self.t();
                // C#'s EqualityComparer<T>.Default: float NaN equals NaN, -0 equals +0
                let eq = if el.is_handle() {
                    format!("sijo_heq((({et}*){t}l->data)[{t}i], {t})")
                } else if el == Ty::Str {
                    format!("sijo_str_eq((({et}*){t}l->data)[{t}i], {t})")
                } else if el == Ty::Float {
                    format!("sijo_f32_eq((({et}*){t}l->data)[{t}i], {t})")
                } else {
                    format!("((({et}*){t}l->data)[{t}i] == {t})")
                };
                let find = format!("int32_t {t}r = -1; for (int32_t {t}i = 0; {t}i < {t}l->len; {t}i++) if ({eq}) {{ {t}r = {t}i; break; }}");
                match op {
                    ListOp::Contains => format!("({{ SijoList {t}lv = {l}; const SijoList* {t}l = &{t}lv; {et} {t} = {v}; {find} {t}r >= 0; }})"),
                    ListOp::IndexOf => format!("({{ SijoList {t}lv = {l}; const SijoList* {t}l = &{t}lv; {et} {t} = {v}; {find} {t}r; }})"),
                    _ => format!("({{ SijoList* {t}l = &{}; {et} {t} = {v}; {find} if ({t}r >= 0) sijo_list_remove_at({t}l, {t}r, (int32_t)sizeof({et})); {t}r >= 0; }})", paren(&l)),
                }
            }
            ListOp::Insert => {
                // C# order: the index, then the value, then the insertion
                let i = self.expr(&args[0], cx);
                let v = if nested { self.list_src(&args[1], cx) } else { self.expr(&args[1], cx) };
                let t = self.t();
                format!("({{ int32_t {t}i = {i}; {et} {t} = {v}; {adopt}*({et}*)SIJO_AT(sijo_list_insert({}, &{}, {t}i, (int32_t)sizeof({et}))) = {t}; }})", self.arena(), paren(&l))
            }
            ListOp::Reverse => format!("sijo_list_reverse(&{}, (int32_t)sizeof({et}))", paren(&l)),
            ListOp::Fill => {
                let v = self.expr(&args[0], cx);
                let t = self.t();
                format!("({{ SijoList* {t}l = &{}; {et} {t}v = {v}; for (int32_t {t}i = 0; {t}i < {t}l->len; {t}i++) (({et}*){t}l->data)[{t}i] = {t}v; }})", paren(&l))
            }
            ListOp::Sort => {
                let cmp = match &el {
                    Ty::Float => "sijo_cmp_f32",
                    Ty::Bool => "sijo_cmp_bool",
                    Ty::Str => "sijo_cmp_str",
                    _ => "sijo_cmp_i32", // int, enum
                };
                format!("sijo_list_sort(&{}, (int32_t)sizeof({et}), {cmp})", paren(&l))
            }
        }
    }

    fn dict_set(&mut self, dict: &Expr, key: &Expr, value: &Expr, cx: &mut FCtx) -> String {
        if let Some((field, _)) = crate::effects::target_field(dict) {
            if self.c.ecs.is_dead(field) {
                return self.side_effects_only(&[dict, key, value], cx);
            }
        }
        let (kind, kt, et) = self.dict_info(&dict.ty);
        let vt = self.cty(&value.ty);
        let k = self.expr(key, cx);
        let v = self.expr(value, cx);
        let place = self.place(dict, cx);
        let t = self.t();
        format!(
            "({{ {kt} {t}k = {k}; {vt} {t}v = {v}; SijoList* {t}d = &{}; int32_t {t}i = sijo_dict_slot({}, {t}d, {kind}, (int32_t)sizeof({et}), &{t}k, (int32_t)sizeof({kt})); (({et}*){t}d->data)[{t}i].Value = {t}v; {t}v; }})",
            paren(&place),
            self.arena()
        )
    }

    fn dict_call(&mut self, e: &Expr, dict: &Expr, op: DictOp, args: &[Expr], cx: &mut FCtx) -> String {
        if op.mutates() {
            if let Some((field, _)) = crate::effects::target_field(dict) {
                if self.c.ecs.is_dead(field) {
                    let mut all: Vec<&Expr> = vec![dict];
                    all.extend(args.iter());
                    let se = self.side_effects_only(&all, cx);
                    return if op == DictOp::Remove { format!("({{ {}; false; }})", se) } else { se };
                }
            }
        }
        let (kind, kt, et) = self.dict_info(&dict.ty);
        let t = self.t();
        let esz = format!("(int32_t)sizeof({et})");
        match op {
            DictOp::ContainsKey | DictOp::GetOr | DictOp::TryGet => {
                let d = self.expr(dict, cx);
                let k = self.expr(&args[0], cx);
                let head = format!("SijoList {t}d = {d}; {kt} {t}k = {k}; int32_t {t}i = sijo_dict_find(&{t}d, {kind}, {esz}, &{t}k);");
                match op {
                    DictOp::ContainsKey => format!("({{ {head} {t}i >= 0; }})"),
                    DictOp::GetOr => {
                        let vt = self.cty(&e.ty);
                        let def = self.expr(&args[1], cx);
                        format!("({{ {head} {vt} {t}r; if ({t}i >= 0) {t}r = (({et}*){t}d.data)[{t}i].Value; else {t}r = {def}; {t}r; }})")
                    }
                    _ => {
                        let out = self.expr(&args[1], cx);
                        let def = self.default_of(&args[1].ty);
                        let cl = self.out_clamp(&args[1], cx);
                        format!("({{ {head} if ({t}i >= 0) {out} = (({et}*){t}d.data)[{t}i].Value; else {out} = {def}; {cl}{}{t}i >= 0; }})", if cl.is_empty() { "" } else { " " })
                    }
                }
            }
            DictOp::Remove => {
                let k = self.expr(&args[0], cx);
                let place = self.place(dict, cx);
                format!("({{ {kt} {t}k = {k}; sijo_dict_remove(&{}, {kind}, {esz}, &{t}k); }})", paren(&place))
            }
            DictOp::Clear => {
                let place = self.place(dict, cx);
                format!("sijo_dict_clear(&{}, {esz})", paren(&place))
            }
            DictOp::Add => {
                let vt = self.cty(&args[1].ty);
                let k = self.expr(&args[0], cx);
                let v = self.expr(&args[1], cx);
                let place = self.place(dict, cx);
                format!(
                    "({{ {kt} {t}k = {k}; {vt} {t}v = {v}; SijoList* {t}d = &{}; if (sijo_dict_find({t}d, {kind}, {esz}, &{t}k) >= 0) SIJO_AT(sijo_panic(\"Dictionary.Add: an item with the same key has already been added\")); int32_t {t}i = sijo_dict_slot({}, {t}d, {kind}, {esz}, &{t}k, (int32_t)sizeof({kt})); (({et}*){t}d->data)[{t}i].Value = {t}v; }})",
                    paren(&place),
                    self.arena()
                )
            }
        }
    }

    fn builtin(&mut self, f: Builtin, args: &[Expr], site: usize, cx: &mut FCtx) -> String {
        if f == Builtin::Print {
            return self.print(&args[0], cx);
        }
        if f == Builtin::Saturate {
            let v = self.expr(&args[0], cx);
            let (ExprKind::Float(lo), ExprKind::Float(hi)) = (&args[1].kind, &args[2].kind) else { unreachable!("range bounds are literals") };
            return self.saturate_code(&args[0].ty, &v, *lo, *hi);
        }
        if matches!(f, Builtin::TryParseInt | Builtin::TryParseFloat) {
            let s = self.expr(&args[0], cx);
            let out = self.place(&args[1], cx);
            let func = if f == Builtin::TryParseInt { "sijo_str_parse_int" } else { "sijo_str_parse_float" };
            let t = self.t();
            let cl = self.out_clamp(&args[1], cx);
            if cl.is_empty() {
                return format!("({{ SijoStr {t} = {s}; {out} = 0; {func}({t}, &{}); }})", paren(&out));
            }
            return format!("({{ SijoStr {t} = {s}; {out} = 0; bool {t}ok = {func}({t}, &{}); {cl} {t}ok; }})", paren(&out));
        }
        if f == Builtin::ArrayGetLength {
            let rank = args[0].ty.rank();
            let l = self.expr(&args[0], cx);
            if let ExprKind::Int(k) = args[1].kind {
                // checked by the type checker: 0 <= k < rank
                return if rank == 1 { format!("({l}).len") } else { format!("({l}).d{k}") };
            }
            let d = self.expr(&args[1], cx);
            let t = self.t();
            return format!("({{ SijoList {t} = {l}; int32_t {t}d = {d}; SIJO_AT(sijo_md_len(&{t}, {rank}, {t}d)); }})");
        }
        let items: Vec<(&Expr, String)> = args.iter().map(|a| (a, self.expr(a, cx))).collect();
        let (pre, a) = self.ordered_args(items);
        let isf = args.first().map(|x| x.ty == Ty::Float).unwrap_or(false);
        let code = match f {
            Builtin::Saturate => unreachable!("handled above"),
            Builtin::MathSqrt => format!("sqrtf({})", a[0]),
            Builtin::MathSin => format!("sijo_sinf({})", a[0]),
            Builtin::MathCos => format!("sijo_cosf({})", a[0]),
            Builtin::MathFloor => format!("floorf({})", a[0]),
            Builtin::MathAtan2 => format!("sijo_atan2f({}, {})", a[0], a[1]),
            Builtin::MathTan => format!("sijo_tanf({})", a[0]),
            Builtin::MathAtan => format!("sijo_atanf({})", a[0]),
            Builtin::MathAsin => format!("sijo_asinf({})", a[0]),
            Builtin::MathAcos => format!("sijo_acosf({})", a[0]),
            Builtin::MathExp => format!("sijo_expf({})", a[0]),
            Builtin::MathLog => format!("sijo_logf({})", a[0]),
            Builtin::MathLog10 => format!("sijo_log10f({})", a[0]),
            Builtin::MathLog2 => format!("sijo_log2f({})", a[0]),
            Builtin::MathPow => format!("sijo_powf({}, {})", a[0], a[1]),
            Builtin::MathRound => format!("sijo_roundf({})", a[0]),
            Builtin::MathCeiling => format!("ceilf({})", a[0]),
            Builtin::MathTruncate => format!("truncf({})", a[0]),
            Builtin::MathSign => if isf { format!("SIJO_AT(sijo_signf({}))", a[0]) } else { format!("sijo_signi({})", a[0]) },
            Builtin::MathAbs => if isf { format!("fabsf({})", a[0]) } else { format!("sijo_absi({})", a[0]) },
            Builtin::MathMin => if isf { format!("sijo_minf({}, {})", a[0], a[1]) } else { format!("sijo_mini({}, {})", a[0], a[1]) },
            Builtin::MathMax => if isf { format!("sijo_maxf({}, {})", a[0], a[1]) } else { format!("sijo_maxi({}, {})", a[0], a[1]) },
            Builtin::MathClamp => {
                if isf {
                    format!("sijo_clampf({}, {}, {})", a[0], a[1], a[2])
                } else {
                    format!("sijo_clampi({}, {}, {})", a[0], a[1], a[2])
                }
            }
            Builtin::DrawSprite => format!("sijo_draw(&w->draws, {}, {}, {})", a[0], a[1], a[2]),
            Builtin::NetView => format!("(void)(w->view_on = 1, w->view_x = {}, w->view_y = {}, w->view_r = {})", a[0], a[1], a[2]),
            Builtin::NetClearView => "(void)(w->view_on = 0)".into(),
            Builtin::DrawText => {
                let t = self.t();
                format!("({{ SijoStr {t} = {}; sijo_draw_text(&w->draws, {}, {}, {t}.p, {t}.n); }})", a[2], a[0], a[1])
            }
            Builtin::RandomFloat => format!("sijo_randf(w, {}u, {}, {})", self.rng_keys.get(&site).copied().unwrap_or(site as u32), a[0], a[1]),
            Builtin::RandomInt => format!("sijo_randi(w, {}u, {}, {})", self.rng_keys.get(&site).copied().unwrap_or(site as u32), a[0], a[1]),
            Builtin::TimeTick => "w->tick".into(),
            Builtin::TimeDt => "w->dt".into(),
            Builtin::Print => unreachable!(),
            Builtin::StrLen => format!("sijo_str_len({})", a[0]),
            Builtin::StrSub => format!("SIJO_AT(sijo_str_sub({}, {}, {}, {}))", self.arena(), a[0], a[1], a[2]),
            Builtin::StrSubFrom => format!("SIJO_AT(sijo_str_from({}, {}, {}))", self.arena(), a[0], a[1]),
            Builtin::StrContains => format!("sijo_str_contains({}, {})", a[0], a[1]),
            Builtin::StrStarts => format!("sijo_str_starts({}, {})", a[0], a[1]),
            Builtin::StrEnds => format!("sijo_str_ends({}, {})", a[0], a[1]),
            Builtin::StrIndexOf => format!("sijo_str_index_of({}, {})", a[0], a[1]),
            Builtin::StrUpper => format!("sijo_str_case({}, {}, 1)", self.arena(), a[0]),
            Builtin::StrLower => format!("sijo_str_case({}, {}, 0)", self.arena(), a[0]),
            Builtin::StrTrim => format!("sijo_str_trim({}, {})", self.arena(), a[0]),
            Builtin::StrReplace => format!("SIJO_AT(sijo_str_replace({}, {}, {}, {}))", self.arena(), a[0], a[1], a[2]),
            Builtin::StrIsEmpty => format!("(({}).n == 0)", a[0]),
            Builtin::StrCompare => format!("sijo_str_cmp({}, {})", a[0], a[1]),
            Builtin::StrCharAt => format!("SIJO_AT(sijo_str_char_at({}, {}))", a[0], a[1]),
            Builtin::CompareVal => {
                let t = self.t();
                let ct = self.cty(&args[0].ty);
                if args[0].ty == Ty::Float {
                    let c = format!("sijo_cmp_f32(&{t}a, &{t}b)");
                    if site == 1 {
                        format!("({{ float {t}a = {}, {t}b = {}; {c} == 0; }})", a[0], a[1])
                    } else {
                        format!("({{ float {t}a = {}, {t}b = {}; {c}; }})", a[0], a[1])
                    }
                } else {
                    format!("({{ {ct} {t}a = {}, {t}b = {}; (int32_t)(({t}a > {t}b) - ({t}a < {t}b)); }})", a[0], a[1])
                }
            }
            Builtin::StrByteLen => format!("({}).n", a[0]),
            Builtin::StrCharAtByte => format!("sijo_str_char_at_byte({}, {})", a[0], a[1]),
            Builtin::StrCharBytes => format!("sijo_str_char_bytes({}, {})", a[0], a[1]),
            Builtin::StrRepeat => format!("SIJO_AT(sijo_str_repeat({}, {}, {}))", self.arena(), a[0], a[1]),
            Builtin::StrSplit => format!("sijo_str_split({}, {}, {})", self.arena(), a[0], a[1]),
            Builtin::StrLastIndexOf => format!("sijo_str_last_index_of({}, {})", a[0], a[1]),
            Builtin::StrPadLeft => format!("SIJO_AT(sijo_str_pad({}, {}, {}, {}, 1))", self.arena(), a[0], a[1], a[2]),
            Builtin::StrPadRight => format!("SIJO_AT(sijo_str_pad({}, {}, {}, {}, 0))", self.arena(), a[0], a[1], a[2]),
            Builtin::CharIsDigit => format!("sijo_ch_is_digit({})", a[0]),
            Builtin::CharIsLetter => format!("sijo_ch_is_letter({})", a[0]),
            Builtin::CharIsLetterOrDigit => format!("({{ int32_t _c = {}; sijo_ch_is_letter(_c) || sijo_ch_is_digit(_c); }})", a[0]),
            Builtin::CharIsWhiteSpace => format!("sijo_ch_is_space({})", a[0]),
            Builtin::CharIsUpper => format!("sijo_ch_is_upper({})", a[0]),
            Builtin::CharIsLower => format!("sijo_ch_is_lower({})", a[0]),
            Builtin::CharIsPunctuation => format!("sijo_ch_is_punct({})", a[0]),
            Builtin::CharToUpper => format!("sijo_ch_upper({})", a[0]),
            Builtin::CharToLower => format!("sijo_ch_lower({})", a[0]),
            Builtin::ParseInt => format!("SIJO_AT(sijo_str_to_int({}))", a[0]),
            Builtin::ParseFloat => format!("SIJO_AT(sijo_str_to_float({}))", a[0]),
            Builtin::TryParseInt | Builtin::TryParseFloat | Builtin::ArrayGetLength => unreachable!("handled above"),
            Builtin::ToStr => self.to_str_code(&args[0].ty, &a[0]),
            Builtin::StrJoin => {
                let el = match &args[1].ty {
                    Ty::List(e) | Ty::Array(e) => (**e).clone(),
                    _ => Ty::Str,
                };
                let et = self.cty(&el);
                let t = self.t();
                let put = self.sb_put(&el, &format!("(({et}*){t}l.data)[{t}i]"), None, &format!("&{t}b"));
                format!(
                    "({{ SijoStr {t}s = {}; SijoList {t}l = {}; SijoSB {t}b = sijo_sb({}); for (int32_t {t}i = 0; {t}i < {t}l.len; {t}i++) {{ if ({t}i) sijo_sb_str(&{t}b, {t}s); {put} }} sijo_sb_done(&{t}b); }})",
                    a[0],
                    a[1],
                    self.arena()
                )
            }
        };
        Self::wrap(pre, code)
    }

    /// value -> string (C#'s ToString())
    fn to_str_code(&self, ty: &Ty, v: &str) -> String {
        match ty {
            Ty::Int => format!("sijo_str_of_int({}, {})", self.arena(), v),
            Ty::Float => format!("sijo_str_of_float({}, {})", self.arena(), v),
            Ty::Bool => format!("sijo_str_of_bool({})", v),
            Ty::Char => format!("sijo_str_of_char({}, {})", self.arena(), v),
            Ty::Enum(e) => format!("sijo_str_of_cstr(sijo_enum_{}({}))", cid(&self.p.enums[*e].name), v),
            Ty::Handle(_) | Ty::NHandle(_) => format!("sijo_str_of_int({}, (int32_t)({}).idx)", self.arena(), v),
            _ => v.to_string(),
        }
    }

    /// append a value to a string builder
    fn sb_put(&self, ty: &Ty, v: &str, spec: Option<&String>, b: &str) -> String {
        let spec = spec.map(|s| fmt_spec(s)).filter(|(k, _)| *k != 0);
        match ty {
            Ty::Int => match spec {
                Some((k, p)) => format!("sijo_sb_fmt_i32({b}, {v}, {k}, {p});"),
                None => format!("sijo_sb_int({b}, {v});"),
            },
            Ty::Float => match spec {
                Some((k, p)) => format!("sijo_sb_fmt_f32({b}, {v}, {k}, {p});"),
                None => format!("sijo_sb_float({b}, {v});"),
            },
            Ty::Bool => format!("sijo_sb_str({b}, sijo_str_of_bool({v}));"),
            Ty::Char => format!("sijo_sb_char({b}, {v});"),
            Ty::Enum(e) => format!("sijo_sb_cstr({b}, sijo_enum_{}({v}));", cid(&self.p.enums[*e].name)),
            Ty::Handle(_) | Ty::NHandle(_) => format!("sijo_sb_int({b}, (int32_t)({v}).idx);"),
            Ty::Str => format!("sijo_sb_str({b}, {v});"),
            _ => String::new(),
        }
    }

    /// $"..." as a string value: built left to right in the frame arena
    fn interp_str(&mut self, parts: &[IPart], cx: &mut FCtx) -> String {
        let t = self.t();
        let mut o = format!("({{ SijoSB {t} = sijo_sb({}); ", self.arena());
        for part in parts {
            match part {
                IPart::Text(s) => {
                    if !s.is_empty() {
                        let _ = write!(o, "sijo_sb_put(&{t}, {}, {}); ", c_str(s), s.len());
                    }
                }
                IPart::Expr(x, spec) => {
                    let v = self.expr(x, cx);
                    let tv = self.t();
                    let _ = write!(o, "{{ {} {tv} = {v}; {} }} ", self.cty(&x.ty), self.sb_put(&x.ty, &tv, spec.as_ref(), &format!("&{t}")));
                }
            }
        }
        let _ = write!(o, "sijo_sb_done(&{t}); }})");
        o
    }

    fn print(&mut self, arg: &Expr, cx: &mut FCtx) -> String {
        // 1. pieces of the message
        enum Piece<'e> {
            Text(String),
            Val(&'e Expr, Option<String>),
        }
        let mut pieces: Vec<Piece> = vec![];
        match &arg.kind {
            ExprKind::Interp(parts) => {
                for part in parts {
                    match part {
                        IPart::Text(t) => pieces.push(Piece::Text(t.clone())),
                        IPart::Expr(x, spec) => match &x.kind {
                            ExprKind::Str(s) => pieces.push(Piece::Text(s.clone())),
                            _ => pieces.push(Piece::Val(x, spec.clone())),
                        },
                    }
                }
            }
            ExprKind::Str(s) => pieces.push(Piece::Text(s.clone())),
            _ => pieces.push(Piece::Val(arg, None)),
        }
        // 2. evaluate raw values left-to-right (temps if several have side effects)
        let raw: Vec<(&Expr, String)> = pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Val(x, _) => Some(*x),
                _ => None,
            })
            .map(|x| (x, self.expr(x, cx)))
            .collect();
        let (pre, vals) = self.ordered_args(raw);
        // 3. format
        let mut fmt = String::new();
        let mut args: Vec<String> = vec![];
        let mut bufs = String::new();
        let mut vi = 0;
        for p in &pieces {
            match p {
                Piece::Text(t) => fmt.push_str(&t.replace('%', "%%")),
                Piece::Val(x, spec) => {
                    let v = vals[vi].clone();
                    vi += 1;
                    let spec = spec.as_ref().map(|s| fmt_spec(s)).filter(|(k, _)| *k != 0);
                    match &x.ty {
                        Ty::Float => {
                            let (k, p) = spec.unwrap_or((0, -1));
                            let b = self.t();
                            let _ = write!(bufs, "char {}[SIJO_FMT_MAX]; ", b);
                            fmt.push_str("%s");
                            args.push(format!("(sijo_fmt_f32({b}, {v}, {k}, {p}), {b})"));
                        }
                        Ty::Int => match spec {
                            Some((k, p)) => {
                                let b = self.t();
                                let _ = write!(bufs, "char {}[SIJO_FMT_MAX]; ", b);
                                fmt.push_str("%s");
                                args.push(format!("(sijo_fmt_i32({b}, {v}, {k}, {p}), {b})"));
                            }
                            None => {
                                fmt.push_str("%d");
                                args.push(format!("(int)({})", v));
                            }
                        },
                        Ty::Bool => {
                            fmt.push_str("%s");
                            args.push(format!("(({}) ? \"True\" : \"False\")", v));
                        }
                        Ty::Char => {
                            let b = self.t();
                            let _ = write!(bufs, "char {}[5]; ", b);
                            fmt.push_str("%s");
                            args.push(format!("sijo_char_utf8({b}, {v})"));
                        }
                        Ty::Enum(e) => {
                            fmt.push_str("%s");
                            args.push(format!("sijo_enum_{}({})", cid(&self.p.enums[*e].name), v));
                        }
                        Ty::Handle(_) | Ty::NHandle(_) => {
                            fmt.push_str("%u");
                            args.push(format!("({}).idx", v));
                        }
                        Ty::Str => {
                            fmt.push_str("%.*s");
                            let b = self.t();
                            let _ = write!(bufs, "SijoStr {} = {}; ", b, v);
                            args.push(format!("(int)({}).n", b));
                            args.push(format!("({}).p", b));
                        }
                        _ => {}
                    }
                }
            }
        }
        let mut all = vec![c_str(&format!("%s{}\n", fmt)), "(w->is_client ? \"[client] \" : \"\")".to_string()];
        all.extend(args);
        format!("({{ {}{}if (!w->quiet) sijo_printf(w, {}); }})", pre, bufs, all.join(", "))
    }

    // ------------------------------------------------------------------
    // deferred calls & remote reads
    // ------------------------------------------------------------------

    fn deferred_plumbing(&mut self) {
        self.w("\n/* ======================= cross-entity calls ======================= */\n");
        // remote-read dispatchers for every non-mutating entity method
        for fid in 0..self.p.funcs.len() {
            let FuncKind::EntityMethod(c) = self.p.funcs[fid].kind else { continue };
            if self.is_mutating_method(fid) {
                continue;
            }
            let func = &self.p.funcs[fid];
            let ret = self.cty(&func.ret);
            let (sig, names) = self.rread_sig(fid);
            let _ = writeln!(self.out, "{} {{\n  SijoSlot* s = sijo_live(&w->ents, h);\n  if (!s) sijo_panic(\"call through a stale entity handle\");\n  switch (s->arch) {{", sig);
            for a in self.p.archetypes_of(c) {
                let target = self.p.target_of(a, fid);
                let view = !self.is_mutating_method(target);
                let mut args = self.inst_args(target, a, "s->row", false);
                args.extend(names.iter().cloned());
                let call = format!("{}({})", self.method_name(target, a, view), args.join(", "));
                if func.ret == Ty::Void {
                    let _ = writeln!(self.out, "  case ARCH_{}: {}; return;", self.aname(a), call);
                } else {
                    let _ = writeln!(self.out, "  case ARCH_{}: return {};", self.aname(a), call);
                }
            }
            let fallback = if func.ret == Ty::Void { String::new() } else { format!(" {{ {} z = {{0}}; return z; }}", ret) };
            let _ = writeln!(self.out, "  }}\n  sijo_panic(\"bad archetype\");{}\n}}", fallback);
        }
        // senders for deferred calls
        for &fid in &self.deferred_funcs.clone() {
            let func = &self.p.funcs[fid];
            let mut packs = String::new();
            let mut frees = String::new();
            for (i, &l) in func.params.iter().enumerate() {
                let ty = func.locals[l].ty.clone();
                if ty == Ty::Str {
                    // a string travels by value: its length (little-endian) and bytes, padded to 4
                    let _ = write!(packs, "sijo_cmd_str(b, a{l}); ");
                } else if ty.is_list() {
                    // a list or array travels as a copy (the receiver builds its own)
                    packs.push_str(&self.msg_pack(&ty, &format!("a{l}"), 0));
                    // an `own` argument was moved into this call: the message has the copy
                    if func.param_mode(i) == ParamMode::Own {
                        frees.push_str(&self.free_code(&ty, &format!("a{l}"), 0));
                    }
                } else {
                    let _ = write!(packs, "sijo_cmd_data(b, &a{l}, (uint32_t)sizeof a{l}); ");
                }
            }
            let sig = self.sender_sig(fid);
            let _ = writeln!(
                self.out,
                "{sig} {{\n  int t = sijo_tid(); SijoCmdBuf* b = &w->cmd[t];\n  uint32_t start = b->dn; {packs}\n  {frees}\n  if (b->dn - start > 65535u) sijo_panic(\"message arguments too large (more than 64 KB of strings, lists and arrays)\");\n  SijoCmd* c = sijo_cmd_push(b);\n  c->kind = SIJO_CMD_CALL; c->target = h; c->src = w->tls[t].cur; c->field = {fid}; c->data = start; c->size = (uint16_t)(b->dn - start);\n}}"
            );
        }
        // executor
        self.w("static void sijo_exec_call(SijoWorld* w, const SijoCmd* c) {\n  SijoSlot* s = sijo_live(&w->ents, c->target);\n  if (!s) return;\n  int32_t row = s->row; int16_t arch = s->arch;\n  const uint8_t* d = w->cmd[0].data + c->data; (void)d; (void)row; (void)arch;\n  switch (c->field) {\n");
        for &fid in &self.deferred_funcs.clone() {
            let func = &self.p.funcs[fid];
            let FuncKind::EntityMethod(cl) = func.kind else { continue };
            let _ = writeln!(self.out, "  case {fid}: {{");
            let has_str = func.params.iter().any(|&l| func.locals[l].ty == Ty::Str || func.locals[l].ty.is_list());
            let mut names = vec![];
            if has_str {
                // variable length: read in order, every length checked against the payload
                self.w("    const uint8_t* e_ = d + c->size; int bad_ = 0;\n");
                for (i, &l) in func.params.clone().iter().enumerate() {
                    let ty = self.p.funcs[fid].locals[l].ty.clone();
                    let pt = self.cty(&ty);
                    if ty == Ty::Str {
                        let _ = writeln!(self.out, "    SijoStr a{l} = sijo_cmd_rd_str(&w->frame[0], &d, e_, &bad_);");
                    } else if ty.is_list() {
                        // the receiver's copy: borrowed for the call (frame arena), or owned by the
                        // receiving method (heap: it may keep it in a field)
                        let heap = if self.p.funcs[fid].param_mode(i) == ParamMode::Own { "1" } else { "0" };
                        let rd = self.msg_unpack(&ty, &format!("a{l}"), heap, 0);
                        let _ = writeln!(self.out, "    SijoList a{l} = {{0}}; {rd}");
                    } else {
                        let _ = writeln!(self.out, "    {pt} a{l}; memset(&a{l}, 0, sizeof a{l}); if (e_ - d < (ptrdiff_t)sizeof a{l}) bad_ = 1; else {{ memcpy(&a{l}, d, sizeof a{l}); d += sizeof a{l}; }}");
                    }
                    names.push(format!("a{}", l));
                }
                let mut drop = String::new();
                for &l in &func.params {
                    if func.locals[l].ty.is_list() {
                        drop.push_str(&self.free_code(&func.locals[l].ty, &format!("a{l}"), 0));
                    }
                }
                let _ = writeln!(self.out, "    if (bad_ || d != e_) {{ {drop} break; }} /* malformed request */");
            } else {
                let sizes: Vec<String> = func.params.iter().map(|&l| format!("sizeof({})", self.cty(&func.locals[l].ty))).collect();
                let expect = if sizes.is_empty() { "0".to_string() } else { sizes.join(" + ") };
                let _ = writeln!(self.out, "    if (c->size != (uint16_t)({expect})) break; /* malformed request */");
                for &l in &func.params {
                    let pt = self.cty(&func.locals[l].ty);
                    let _ = writeln!(self.out, "    {pt} a{l}; memcpy(&a{l}, d, sizeof a{l}); d += sizeof a{l};");
                    names.push(format!("a{}", l));
                }
            }
            // a ranged parameter's contract holds whoever sent the call (an RPC comes from a client)
            for &l in &func.params.clone() {
                if let Some((lo, hi)) = self.p.funcs[fid].locals[l].range {
                    let ty = self.p.funcs[fid].locals[l].ty.clone();
                    let v = self.saturate_code(&ty, &format!("a{l}"), lo, hi);
                    let _ = writeln!(self.out, "    a{l} = {v};");
                }
            }
            self.w("    switch (arch) {\n");
            for a in self.p.archetypes_of(cl) {
                let target = self.p.target_of(a, fid);
                let mut args = self.inst_args(target, a, "row", false);
                args.extend(names.iter().cloned());
                let _ = writeln!(self.out, "    case ARCH_{}: {}({}); break;", self.aname(a), self.method_name(target, a, false), args.join(", "));
            }
            self.w("    }\n    break; }\n");
        }
        self.w("  }\n}\n");
        // command application at the end of a stage. With components, Add/Remove commands are
        // collected in command order and applied after the destroys (a destroyed entity gains
        // nothing), before the spawned entities join their tables
        let (cases, tail) = if self.has_comps() {
            (
                "    case SIJO_CMD_CADD: case SIJO_CMD_CREM:\n      if (w->cn == w->ccap) { w->ccap = w->ccap ? w->ccap * 2 : 64; w->clist = (int32_t*)sijo_xrealloc(w->clist, (size_t)w->ccap * sizeof(int32_t)); }\n      w->clist[w->cn++] = i; break;\n",
                "  for (int32_t i = 0; i < w->dn; i++) sijo_destroy(w, w->dlist[i]);\n  w->dn = 0;\n  for (int32_t i = 0; i < w->cn; i++) sijo_comp_change(w, &b->c[w->clist[i]], b->data);\n  w->cn = 0;\n  b->n = 0; b->dn = 0;\n",
            )
        } else {
            ("", "  b->n = 0; b->dn = 0;\n  for (int32_t i = 0; i < w->dn; i++) sijo_destroy(w, w->dlist[i]);\n  w->dn = 0;\n")
        };
        let apply = r#"
static void sijo_apply(SijoWorld* w) {
  SijoCmdBuf* b = &w->cmd[0];
  for (int32_t i = 0; i < b->n; i++) {
    SijoCmd c = b->c[i];
    w->tls[0].cur = c.target; w->tls[0].rng_ent = sijo_ekey(c.target); w->tls[0].rng_ctr = 0;
    w->cmds_applied++;
    switch (c.kind) {
    case SIJO_CMD_SET: case SIJO_CMD_ADDI: case SIJO_CMD_ADDF: sijo_apply_field(w, &c); break;
    case SIJO_CMD_CALL: sijo_exec_call(w, &c); break;
    case SIJO_CMD_DESTROY:
      if (w->dn == w->dcap) { w->dcap = w->dcap ? w->dcap * 2 : 64; w->dlist = (SijoH*)sijo_xrealloc(w->dlist, (size_t)w->dcap * sizeof(SijoH)); }
      w->dlist[w->dn++] = c.target; break;
SIJO_APPLY_COMP_CASES    }
    if (i > 50000000) sijo_panic("deferred command storm (a message loop between entities?)");
    b = &w->cmd[0];
  }
SIJO_APPLY_COMP_TAIL  sijo_flush_all(w);
  SIJO_APPLY_PERSIST
  w->tls[0].cur = SIJO_NULL;
}
static void sijo_merge_threads(SijoWorld* w) {
  for (int t = 1; t < SIJO_MAX_THREADS; t++) if (w->cmd[t].n) sijo_cmd_append(&w->cmd[0], &w->cmd[t]);
}
"#;
        let persist = if self.p.persist.is_empty() { "" } else { "if (sijo_store_dir && !w->is_client) sijo_pload(w);" };
        let apply = apply.replace("SIJO_APPLY_COMP_CASES", cases).replace("SIJO_APPLY_COMP_TAIL", tail).replace("SIJO_APPLY_PERSIST", persist);
        self.w(&apply);
    }

    // ------------------------------------------------------------------
    // systems & scheduler
    // ------------------------------------------------------------------

    fn systems(&mut self) {
        self.w("\n/* ======================= systems (scheduled by the compiler) ======================= */\n");
        let systems = self.c.ecs.systems.clone();
        let groups = crate::ecs::task_groups(&self.c.ecs, &self.c.spatial);
        let task_members: BTreeSet<usize> = groups.iter().flat_map(|g| g.members.iter().copied()).collect();
        for (i, s) in systems.iter().enumerate() {
            if matches!(s.kind, SysKind::Entity | SysKind::Client) {
                // the row loop of each archetype in a function of its own: outside the setjmp of the
                // parallel region (which would keep the loop from being vectorized), `dt` read once
                let e = &s.eff;
                // spawns and prints also name their entity: on shards it orders them (src/shard.rs)
                let needs_ctx = e.random || !e.remote_writes.is_empty() || !e.remote_calls.is_empty() || e.destroys_self || !e.destroys.is_empty() || e.restructures_self || !e.restructures.is_empty() || !e.rpcs.is_empty() || !e.msg_writes.is_empty() || self.shard.own_spawns.contains(&i) || e.prints;
                // a partitioned system on a shard runs only the rows this shard owns
                let part = s.kind == SysKind::Entity && self.shard.mode[i] == crate::shard::Mode::Part;
                for &a in &s.archetypes {
                    let n = self.aname(a);
                    let mname = self.method_name(s.func, a, false);
                    let takes_dt = s.kind == SysKind::Entity && !self.p.funcs[s.func].params.is_empty();
                    // shared code: the table's view, built once for the loop
                    let (vdecl, vp) = match self.share.get(&(s.func, a)) {
                        Some(&g) => (format!("\n  const SijoView_{g} v_ = sijo_view{g}_{n}(w);"), "&v_, "),
                        None => (String::new(), ""),
                    };
                    let call = if takes_dt { format!("{}(w, {vp}r, dt_)", mname) } else { format!("{}(w, {vp}r)", mname) };
                    let (tl, ctx) = if needs_ctx {
                        ("\n  SijoTls* tl = &w->tls[sijo_tid()];", format!("SijoH me = w->{n}.ent[r]; tl->cur = me; tl->rng_ent = sijo_ekey(me); tl->rng_ctr = 0;\n    "))
                    } else {
                        ("", String::new())
                    };
                    // each entity's temporaries go back to the arena after its call when nothing the
                    // system allocates can outlive the call (see effects_keep_frame)
                    let allocates = self.p.archetypes_of(a).into_iter().map(|b| self.p.target_of(b, s.func)).any(|g| self.alloc_funcs.contains(&g)) || self.alloc_funcs.contains(&s.func);
                    let (mark, rewind) = if allocates && !self.effects_keep_frame(&s.eff) {
                        ("\n  SijoArena* fa_ = &w->frame[sijo_tid()]; SijoAMark am_ = sijo_arena_mark(fa_);", "\n    sijo_arena_rewind(fa_, am_);")
                    } else {
                        ("", "")
                    };
                    // a component's system on an open table: the rows that have the component
                    let oskip = match self.p.funcs[s.func].kind {
                        FuncKind::EntityMethod(k) if self.p.classes[k].is_component => self.open_has(a, &[k], "r").map(|t| format!("if (!{t}) continue;\n    ")).unwrap_or_default(),
                        _ => String::new(),
                    };
                    let owned = if part {
                        format!("\n  if (w->sh_on) {{ /* a shard: only the rows it owns */\n    const int32_t* own_ = w->ents.shard; const int32_t me_ = w->sh_id;\n    for (int32_t r = lo_; r < hi_; r++) {{\n      if (own_[w->{n}.ent[r].idx] != me_) continue;\n      {oskip}{ctx}{call};{rewind}\n    }}\n    return;\n  }}")
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        self.out,
                        "__attribute__((noinline)) static void sys{i}_{n}(SijoWorld* w, int32_t lo_, int32_t hi_) {{\n  float dt_ = w->dt; (void)dt_;{vdecl}{tl}{mark}{owned}\n  for (int32_t r = lo_; r < hi_; r++) {{\n    {oskip}{ctx}{call};{rewind}\n  }}\n}}"
                    );
                    if self.gpu_systems.contains(&i) {
                        self.gpu_kernel(i, s, a, &call);
                    }
                }
            }
            self.system_fn(i, s, false);
            if task_members.contains(&i) {
                self.system_fn(i, s, true);
            }
        }
        // task groups: systems of a stage side by side, one thread each (see ecs::task_groups)
        if !groups.is_empty() {
            self.w("/* one member of a task group on this thread: its per-thread slot `vt` holds its commands,\n   frame arena and entity context; panics unwind here, as in a parallel loop */\nstatic void sijo_task_run(SijoWorld* w, int vt, void (*fn)(SijoWorld*), SijoPanic* pp, jmp_buf* oc) {\n  jmp_buf jb_; volatile int skip_ = 0;\n  sijo_tl_tid = vt; sijo_tl_panic = pp; sijo_tl_catch = NULL;\n  if (oc) { if (setjmp(jb_) != 0) skip_ = 1; else sijo_tl_catch = &jb_; }\n  if (!skip_) fn(w);\n  sijo_tl_catch = NULL; sijo_tl_tid = 0;\n}\n");
        }
        for (g, grp) in groups.iter().enumerate() {
            let names: Vec<String> = grp.members.iter().map(|&i| systems[i].name.clone()).collect();
            let _ = writeln!(self.out, "/* stage {}: {} side by side when each is below its parallel threshold */\nstatic void sijo_group{}(SijoWorld* w) {{", grp.stage, names.join(", "), g);
            let mut small = vec![];
            let mut work = vec![];
            for (j, &i) in grp.members.iter().enumerate() {
                let rows: Vec<String> = systems[i].archetypes.iter().map(|&a| format!("w->{}.count", self.aname(a))).collect();
                let _ = writeln!(self.out, "  int32_t n{j} = {};", if rows.is_empty() { "0".into() } else { rows.join(" + ") });
                let pm = if systems[i].par_min == crate::ecs::DEFAULT_PAR_MIN { "SIJO_PAR_MIN".to_string() } else { systems[i].par_min.to_string() };
                small.push(format!("n{j} < {pm}"));
                work.push(match grp.ns_per_row[j] {
                    Some(c) => format!("(double)n{j} * {:.3}", c),
                    None => format!("(double)n{j}"),
                });
            }
            let k = grp.members.len();
            let cond = match (&self.c.ecs.profile, grp.ns_per_row.iter().all(|c| c.is_some())) {
                // measured: side by side costs the largest member (or the even share of all of them)
                // plus one parallel region; one after another, each member costs its own loop, in
                // parallel (its share plus a region) when it reaches its threshold. Pick the cheaper.
                (Some(pr), true) => {
                    let fork = pr.fork_ns.max(200) as f64;
                    let mut sep = vec![];
                    for (j, &i) in grp.members.iter().enumerate() {
                        let _ = writeln!(self.out, "  double w{j} = {};", work[j]);
                        sep.push(format!("(n{j} >= {} ? w{j} / t_ + {fork:.1} : w{j})", systems[i].par_min));
                    }
                    let ws: Vec<String> = (0..k).map(|j| format!("w{j}")).collect();
                    let _ = writeln!(self.out, "  double t_ = (double)w->nthreads, all_ = {}, big_ = 0;", ws.join(" + "));
                    for j in 0..k {
                        let _ = writeln!(self.out, "  if (w{j} > big_) big_ = w{j};");
                    }
                    let _ = writeln!(self.out, "  double grp_ = (big_ > all_ / t_ ? big_ : all_ / t_) + {fork:.1}, sep_ = {};", sep.join(" + "));
                    "w->nthreads > 1 && grp_ < sep_".to_string()
                }
                // the row heuristic: every member below its threshold, together worth a region
                _ => format!("w->nthreads > 1 && {} && {} >= SIJO_PAR_MIN", small.join(" && "), (0..k).map(|j| format!("n{j}")).collect::<Vec<_>>().join(" + ")),
            };
            let _ = writeln!(self.out, "  if ({}) {{", cond);
            self.w("    SijoPanic* pp_ = sijo_tl_panic; jmp_buf* oc_ = sijo_tl_catch;\n");
            let _ = writeln!(self.out, "    #pragma omp parallel num_threads(w->nthreads < {k} ? w->nthreads : {k})\n    #pragma omp single\n    {{");
            // larger members first (by the profile), so the last task to start is a small one
            let mut order: Vec<usize> = (0..k).collect();
            if let Some(pr) = &self.c.ecs.profile {
                order.sort_by(|&x, &y| {
                    let c = |j: usize| pr.ns_per_call(&systems[grp.members[j]].name).unwrap_or(0.0);
                    c(y).partial_cmp(&c(x)).unwrap_or(std::cmp::Ordering::Equal).then(x.cmp(&y))
                });
            }
            for j in order {
                let _ = writeln!(self.out, "      #pragma omp task\n      sijo_task_run(w, {}, sys{}_task, pp_, oc_);", j + 1, grp.members[j]);
            }
            self.w("    }\n    sijo_tl_catch = oc_; sijo_tl_panic = pp_;\n    if (oc_ && pp_ && pp_->flag) longjmp(*oc_, 1);\n    /* slot j+1 holds member j's commands: appended in stage order */\n    sijo_merge_threads(w);\n  } else {\n");
            for &i in &grp.members {
                let _ = writeln!(self.out, "    sys{}(w);", i);
            }
            self.w("  }\n}\n");
        }
        // profiling run (--profile FILE): every system timed on one thread
        let nsys = systems.len().max(1);
        self.w("static void sijo_tick_prof(SijoWorld* w) {\n");
        for (si, stage) in self.c.ecs.stages.clone().iter().enumerate() {
            for &i in stage {
                let rows: Vec<String> = systems[i].archetypes.iter().map(|&a| format!("(int64_t)w->{}.count", self.aname(a))).collect();
                let rows = if rows.is_empty() { "1".to_string() } else { rows.join(" + ") };
                let _ = writeln!(self.out, "  {{ int64_t r_ = {rows}; double t_ = sijo_now(); sys{i}(w); w->prof_ns[{i}] += (uint64_t)((sijo_now() - t_) * 1e9); w->prof_rows[{i}] += (uint64_t)r_; w->prof_calls[{i}]++; }}");
            }
            let _ = writeln!(self.out, "  {{ double t_ = sijo_now(); sijo_apply(w); w->prof_apply[{si}] += (uint64_t)((sijo_now() - t_) * 1e9); }}");
        }
        self.w("  (void)w;\n}\n");
        let server: Vec<usize> = (0..systems.len()).filter(|&i| matches!(systems[i].kind, SysKind::Entity | SysKind::Global)).collect();
        let sig = crate::profile::signature(server.iter().map(|&i| systems[i].name.as_str()));
        let _ = writeln!(
            self.out,
            "/* the profile `sijoc build --profile-use FILE` reads (src/profile.rs) */\nstatic void sijo_profile_write(SijoWorld* w, const char* path) {{\n  double fork = 0;\n#ifdef _OPENMP\n  {{\n    int nt = sijo_nthreads(); volatile int sink = 0;\n    for (int i = 0; i < 200; i++) {{\n      #pragma omp parallel num_threads(nt)\n      {{ if (omp_get_thread_num() == 0) sink++; }}\n    }}\n    double t0 = sijo_now();\n    for (int i = 0; i < 2000; i++) {{\n      #pragma omp parallel num_threads(nt)\n      {{ if (omp_get_thread_num() == 0) sink++; }}\n    }}\n    fork = (sijo_now() - t0) / 2000 * 1e9;\n    (void)sink;\n  }}\n#endif\n  FILE* f = fopen(path, \"w\");\n  if (!f) {{ fprintf(stderr, \"cannot write the profile %s\\n\", path); return; }}\n  fprintf(f, \"{}\\nsignature %016llx\\nticks %d\\nthreads %d\\nfork_ns %llu\\n\", 0x{:016x}ull, (int)w->tick, sijo_nthreads(), (unsigned long long)fork);",
            crate::profile::HEADER, sig
        );
        for &i in &server {
            let _ = writeln!(self.out, "  fprintf(f, \"system %s calls %llu rows %llu ns %llu\\n\", {}, (unsigned long long)w->prof_calls[{i}], (unsigned long long)w->prof_rows[{i}], (unsigned long long)w->prof_ns[{i}]);", c_str(&systems[i].name));
        }
        for si in 0..self.c.ecs.stages.len() {
            let _ = writeln!(self.out, "  fprintf(f, \"apply {si} ns %llu\\n\", (unsigned long long)w->prof_apply[{si}]);");
        }
        // --profile-fields: accesses per field, summed over the threads
        if self.field_counts {
            for f in 0..self.p.fields.len() {
                if self.c.ecs.is_dead(f) || self.c.ecs.comp_of(f).is_none() {
                    continue;
                }
                let label = crate::profile::field_key(&self.p.field_label(f));
                let _ = writeln!(self.out, "  {{ unsigned long long n_ = 0; for (int t = 0; t < SIJO_MAX_THREADS; t++) n_ += sijo_fcount[t][{f}]; fprintf(f, \"field %s count %llu\\n\", {}, n_); }}", c_str(&label));
            }
        }
        self.w("  (void)w;\n  fclose(f);\n}\n");
        let _ = nsys;
        // tick
        let group_of: BTreeMap<usize, usize> = groups.iter().enumerate().map(|(g, grp)| (grp.members[0], g)).collect();
        // [Persistent]: every entity's values go into the store at the end of the tick
        let ptick = if self.p.persist.is_empty() { "" } else { "  sijo_ptick(w);\n" };
        let _ = write!(self.out, "static void sijo_tick(SijoWorld* w) {{\n  if (w->prof) {{ sijo_tick_prof(w);{} return; }}\n", if ptick.is_empty() { "" } else { " sijo_ptick(w);" });
        for (si, stage) in self.c.ecs.stages.clone().iter().enumerate() {
            let names: Vec<String> = stage.iter().map(|&i| systems[i].name.clone()).collect();
            let _ = writeln!(self.out, "  /* stage {}: {} */", si, names.join(", "));
            for &i in stage {
                if let Some(g) = group_of.get(&i) {
                    let _ = writeln!(self.out, "  sijo_group{}(w);", g);
                } else if !task_members.contains(&i) {
                    let _ = writeln!(self.out, "  sys{}(w);", i);
                }
            }
            self.w("  sijo_apply(w);\n");
        }
        self.w(ptick);
        // client rows in server-slot order: every client (early, late-joining, restored) then runs
        // its client systems in the same entity order, so draw lists, client prints and the order of
        // RPCs are identical, not just the replicated state
        self.w("}\n/* sort the rows of one table by key (unique), permuting every column */\nstatic int sijo_cmp_key(const void* a, const void* b) {\n  uint64_t x = *(const uint64_t*)a, y = *(const uint64_t*)b;\n  return (x > y) - (x < y);\n}\nstatic void sijo_permute(void* base, size_t esz, const uint64_t* kr, int32_t n) {\n  if (!base) return;\n  uint8_t* tmp = (uint8_t*)sijo_xrealloc(NULL, esz * (size_t)n);\n  for (int32_t i = 0; i < n; i++) memcpy(tmp + esz * (size_t)i, (uint8_t*)base + esz * (size_t)(uint32_t)kr[i], esz);\n  memcpy(base, tmp, esz * (size_t)n);\n  free(tmp);\n}\n");
        for &a in &self.c.ecs.archetypes.clone() {
            let n = self.aname(a);
            let comps = self.c.ecs.arch_comps[&a].clone();
            let mut cols = String::new();
            for &k in &comps {
                let cn = self.ccomp(k);
                let _ = write!(cols, "  sijo_permute(t->c{k}, sizeof({cn}), kr, n);\n");
                if self.c.ecs.components[k].replicated {
                    let _ = write!(cols, "  sijo_permute(t->d{k}, 1, kr, n);\n");
                }
            }
            let _ = write!(
                self.out,
                "static void sijo_order_{n}(SijoWorld* w, const SijoCMap* m) {{\n  SijoArch_{n}* t = &w->{n}; int32_t n = t->count;\n  if (n < 2) return;\n  uint64_t prev = 0; int sorted = 1;\n  for (int32_t r = 0; r < n && sorted; r++) {{ uint32_t ci = t->ent[r].idx; uint64_t k = ci < (uint32_t)m->rcap ? m->rmap[ci] : 0; if (r && k < prev) sorted = 0; prev = k; }}\n  if (sorted) return;\n  uint64_t* kr = (uint64_t*)sijo_xrealloc(NULL, sizeof(uint64_t) * (size_t)n);\n  for (int32_t r = 0; r < n; r++) {{ uint32_t ci = t->ent[r].idx; uint64_t k = ci < (uint32_t)m->rcap ? m->rmap[ci] : 0; kr[r] = (k << 32) | (uint32_t)r; }}\n  qsort(kr, (size_t)n, sizeof(uint64_t), sijo_cmp_key);\n  for (int32_t r = 0; r < n; r++) kr[r] &= 0xffffffffull;\n  sijo_permute(t->ent, sizeof(SijoH), kr, n);\n{cols}  for (int32_t r = 0; r < n; r++) w->ents.s[t->ent[r].idx].row = r;\n  free(kr);\n}}\n"
            );
        }
        self.w("static void sijo_client_order(SijoWorld* w, const SijoCMap* m) {\n");
        for &a in &self.c.ecs.archetypes.clone() {
            let _ = writeln!(self.out, "  sijo_order_{}(w, m);", self.aname(a));
        }
        self.w("  (void)w; (void)m;\n}\n");
        self.w("/* client frame: client systems only read replicated state; calls that modify the server\n   are left in the command buffer as RPC requests (queued reliably by sijo_cli_collect_rpcs) */\nstatic void sijo_client_frame(SijoWorld* w) {\n  w->draws.n = 0; w->draws.tn = 0;\n");
        for &i in &self.c.ecs.client_systems.clone() {
            let _ = writeln!(self.out, "  sys{}(w);", i);
        }
        self.w("  sijo_merge_threads(w);\n}\n");
    }


    /// `sys{i}`; with `task`, the variant a task group runs (see task_group): the rows on the
    /// calling thread, the commands left in that thread's buffer for the group to merge in order
    /// `--gpu`: the row loop of system `i` over archetype `a` as an OpenMP target region. The world
    /// goes to the device as the struct the methods address (`map(to: w[0:1])`); the columns the
    /// system reads go there (`map(to:)`), the ones it writes there and back (`map(tofrom:)`); every
    /// row is independent (a pure self-write system), so any order gives the same bits as long as
    /// the device compiler keeps IEEE single precision without contraction (sijo's own math
    /// functions are compiled for the device with the methods). Without a device the region runs on
    /// the host threads.
    fn gpu_kernel(&mut self, i: usize, s: &crate::ecs::SystemPlan, a: ClassId, call: &str) {
        let n = self.aname(a);
        let comps = self.c.ecs.arch_comps[&a].clone();
        let read: BTreeSet<usize> = s.eff.self_reads.iter().filter(|f| !self.c.ecs.is_dead(**f)).filter_map(|&f| self.c.ecs.comp_of(f)).filter(|k| comps.contains(k)).collect();
        let write: BTreeSet<usize> = s.eff.self_writes.iter().filter(|f| !self.c.ecs.is_dead(**f)).filter_map(|&f| self.c.ecs.comp_of(f)).filter(|k| comps.contains(k)).collect();
        let to: Vec<String> = read.iter().filter(|k| !write.contains(k)).map(|k| format!("w->{n}.c{k}[0:n]")).collect();
        let tofrom: Vec<String> = write.iter().map(|k| format!("w->{n}.c{k}[0:n]")).collect();
        let mut maps = String::from(" map(to: w[0:1])");
        if !to.is_empty() {
            let _ = write!(maps, " map(to: {})", to.join(", "));
        }
        if !tofrom.is_empty() {
            let _ = write!(maps, " map(tofrom: {})", tofrom.join(", "));
        }
        let _ = writeln!(
            self.out,
            "/* system {i} ({}) over {} on a compute device (sijoc build --gpu, docs/07 §7.4) */\nstatic void sijo_gpu{i}_{n}(SijoWorld* w, int32_t n) {{\n  float dt_ = w->dt; (void)dt_;\n  #pragma omp target teams distribute parallel for{maps}\n  for (int32_t r = 0; r < n; r++) {{\n    {call};\n  }}\n}}",
            s.name,
            self.p.classes[a].name
        );
    }

    fn system_fn(&mut self, i: usize, s: &crate::ecs::SystemPlan, task: bool) {
        let par_min = if s.par_min == crate::ecs::DEFAULT_PAR_MIN { "SIJO_PAR_MIN".to_string() } else { s.par_min.to_string() };
        // a task runs on its group slot: snapshot copies go to that slot's frame arena
        let arena = if task { "&w->frame[sijo_tid()]" } else { "&w->frame[0]" };
        if task {
            let _ = writeln!(self.out, "/* system {}: {} (as a task: one thread, commands merged by the group) */\nstatic void sys{}_task(SijoWorld* w) {{", i, s.name, i);
        } else {
            let _ = writeln!(self.out, "/* system {}: {} */\nstatic void sys{}(SijoWorld* w) {{", i, s.name, i);
        }
        match s.kind {
            SysKind::Entity | SysKind::Client => {
                // hazard snapshots: only the fields this system reads on other entities go through
                // the `_read` array, so only those are copied (the other columns of `_prev` are
                // never read); the copy runs on all threads
                for &a in &s.archetypes {
                    let n = self.aname(a);
                    for &k in &s.hazards {
                        if self.c.ecs.arch_comps[&a].contains(&k) {
                            let cn = self.ccomp(k);
                            let _ = writeln!(self.out, "  sijo_prev_fit_{n}(&w->{n});");
                            let fields = self.c.ecs.components[k].fields.clone();
                            let copy: Vec<FieldId> = fields.iter().copied().filter(|f| s.eff.remote_reads.contains(f)).collect();
                            let plain: Vec<FieldId> = copy.iter().copied().filter(|&f| !self.p.fields[f].ty.is_list()).collect();
                            if copy.len() == fields.len() {
                                let _ = writeln!(self.out, "  sijo_par_copy(w->nthreads, w->{n}.c{k}_prev, w->{n}.c{k}, (size_t)w->{n}.count * sizeof({cn}));");
                            } else if !plain.is_empty() {
                                let body: String = plain.iter().map(|&f| format!(" d_[r].{0} = s_[r].{0};", self.fname(f))).collect();
                                let _ = writeln!(
                                    self.out,
                                    "  {{ {cn}* d_ = w->{n}.c{k}_prev; const {cn}* s_ = w->{n}.c{k}; int32_t n_ = w->{n}.count;\n    #pragma omp parallel for schedule(static) if (n_ >= 16384) num_threads(w->nthreads)\n    for (int32_t r = 0; r < n_; r++) {{{body} }}\n  }}"
                                );
                            }
                            let _ = writeln!(self.out, "  w->{n}.c{k}_read = w->{n}.c{k}_prev;");
                            for &f in &copy {
                                if let Ty::List(el) | Ty::Array(el) | Ty::MdArray(el, _) = &self.p.fields[f].ty {
                                    let et = self.cty(el);
                                    let fname = self.fname(f);
                                    let fty = self.p.fields[f].ty.clone();
                                    let cl = if el.is_list() || matches!(fty, Ty::MdArray(..)) {
                                        self.clone_code(&fty, &format!("w->{n}.c{k}[r].{fname}"), arena, "0", 0)
                                    } else {
                                        format!("sijo_list_clone({arena}, 0, &w->{n}.c{k}[r].{fname}, (int32_t)sizeof({et}))")
                                    };
                                    let _ = writeln!(self.out, "  for (int32_t r = 0; r < w->{n}.count; r++) w->{n}.c{k}_prev[r].{fname} = {cl};");
                                }
                                if self.p.fields[f].ty.is_dict() {
                                    let (kind, _, et) = self.dict_info(&self.p.fields[f].ty);
                                    let fname = self.fname(f);
                                    let _ = writeln!(self.out, "  for (int32_t r = 0; r < w->{n}.count; r++) w->{n}.c{k}_prev[r].{fname} = sijo_dict_clone({arena}, 0, &w->{n}.c{k}[r].{fname}, {kind}, (int32_t)sizeof({et}));");
                                }
                            }
                        }
                    }
                }
                // proximity grids read by this system's loops (from the snapshot the loops read)
                let built: Vec<usize> = self.grid_ids.iter().filter(|((li, _), _)| self.c.spatial.loops[*li].systems.contains(&s.func)).map(|(_, &g)| g).collect();
                for &gid in &built {
                    let _ = writeln!(self.out, "  sijo_grid_build_{gid}(w);");
                }
                for &a in &s.archetypes {
                    let n = self.aname(a);
                    let _ = writeln!(self.out, "  {{ int32_t n = w->{n}.count;");
                    // the per-entity context (command source, RNG stream) is only kept when the
                    // system emits deferred commands or draws random numbers; the thread id is
                    // read once per thread, and each thread's context has its own cache line
                    let tspawn = s.parallel && s.spawn_par;
                    // on shards a partitioned system's spawns wait for the barrier, where every
                    // shard's spawns get their slots in one global order (sijo_sh_install)
                    let shspawn = !tspawn && s.kind == SysKind::Entity && self.shard.mode[i] == crate::shard::Mode::Part && self.shard.own_spawns.contains(&i);
                    if tspawn {
                        self.w("  w->tspawn = 1;\n");
                    } else if shspawn {
                        self.w("  if (w->sh_on) w->tspawn = 1;\n");
                    }
                    // --gpu: large tables of a pure self-write system go to the compute device (one
                    // process: a shard keeps the host loop, which visits only the rows it owns)
                    let gpu = self.gpu_systems.contains(&i) && !task;
                    if gpu {
                        let _ = writeln!(self.out, "  if (!w->sh_on && n >= SIJO_GPU_MIN) {{ sijo_gpu{i}_{n}(w, n); }} else {{");
                    }
                    if s.parallel && !task {
                        // each thread runs one contiguous block of rows (thread t after thread t-1, so
                        // per-thread commands and spawns concatenate in row order) and catches its own
                        // panics inside the region when the caller catches them (embedding API)
                        self.w("  SijoPanic* pp_ = sijo_tl_panic; jmp_buf* oc_ = sijo_tl_catch;\n");
                        let _ = writeln!(self.out, "  #pragma omp parallel if (n >= {}) num_threads(w->nthreads)\n  {{", par_min);
                        self.w("    int t_ = sijo_omp_tid(), nt_ = sijo_team();\n    int32_t lo_ = (int32_t)((int64_t)n * t_ / nt_), hi_ = (int32_t)((int64_t)n * (t_ + 1) / nt_);\n    sijo_tl_tid = t_;\n");
                        self.w("    jmp_buf jb_; volatile int skip_ = 0;\n    sijo_tl_panic = pp_; sijo_tl_catch = NULL;\n    if (oc_) { if (setjmp(jb_) != 0) skip_ = 1; else sijo_tl_catch = &jb_; }\n");
                        let _ = writeln!(self.out, "    if (!skip_) sys{i}_{n}(w, lo_, hi_);\n    sijo_tl_catch = NULL; sijo_tl_tid = 0;\n  }}");
                        self.w("  sijo_tl_catch = oc_; sijo_tl_panic = pp_;\n  if (oc_ && pp_ && pp_->flag) longjmp(*oc_, 1);\n");
                    } else {
                        let _ = writeln!(self.out, "  sys{i}_{n}(w, 0, n);");
                    }
                    if gpu {
                        self.w("  }\n");
                    }
                    if tspawn {
                        // static schedule: thread t ran a contiguous block of rows after thread t-1,
                        // so concatenating the per-thread tables reproduces the serial spawn order
                        self.w("  w->tspawn = 0; if (!w->sh_on) sijo_tmerge_all(w);\n");
                    } else if shspawn {
                        self.w("  w->tspawn = 0;\n");
                    }
                    if s.parallel && !task {
                        self.w("  sijo_merge_threads(w);\n");
                    }
                    if tspawn && s.spawn_handles {
                        // the provisional handles this system stored or sent become the final ones
                        // (on shards: at the barrier, see sijo_sh_fix)
                        self.w("  if (!w->sh_on) {\n");
                        let present = self.p.all_fields(a);
                        for &f in &s.spawn_fix {
                            if !present.contains(&f) || self.c.ecs.is_dead(f) {
                                continue;
                            }
                            let k = self.comp(f);
                            let _ = writeln!(self.out, "  for (int32_t r = 0; r < w->{n}.count; r++) {{ SijoH* h = &w->{n}.c{k}[r].{fname}; if (h->idx & SIJO_PROV) *h = sijo_prov_resolve(w, *h); }}", fname = self.fname(f));
                        }
                        self.w("  for (int32_t i = 0; i < w->cmd[0].n; i++) { SijoCmd* c = &w->cmd[0].c[i]; if (c->target.idx & SIJO_PROV) c->target = sijo_prov_resolve(w, c->target); }\n  }\n");
                    }
                    self.w("  }\n");
                }
                for &a in &s.archetypes {
                    let n = self.aname(a);
                    for &k in &s.hazards {
                        if self.c.ecs.arch_comps[&a].contains(&k) {
                            let _ = writeln!(self.out, "  w->{n}.c{k}_read = w->{n}.c{k};");
                        }
                    }
                }
                // the grids describe this system's snapshot only: code that runs later (messages,
                // other systems calling the same helper) scans instead
                for &gid in &built {
                    let _ = writeln!(self.out, "  w->grid[{gid}].ok = 0;");
                }
            }
            SysKind::Global => {
                let _ = writeln!(self.out, "  w->tls[0].cur = SIJO_NULL; w->tls[0].rng_ent = 0; w->tls[0].rng_ctr = 0;\n  {}(w, w->dt);", self.func_name(s.func));
            }
            SysKind::ClientGlobal => {
                let _ = writeln!(self.out, "  w->tls[0].rng_ent = 0; w->tls[0].rng_ctr = 0;\n  {}(w);", self.func_name(s.func));
            }
        }
        self.w("}\n");
    }

    // ------------------------------------------------------------------
    // replication
    // ------------------------------------------------------------------

    fn arch_leaves(&self, a: ClassId) -> Vec<(usize, Vec<(FieldId, Leaf)>)> {
        let mut v = vec![];
        for &k in &self.c.ecs.arch_comps[&a] {
            let comp = &self.c.ecs.components[k];
            if !comp.replicated {
                continue;
            }
            let mut leaves = vec![];
            for &f in &comp.fields {
                if let Some(fnet) = self.c.net.fields.get(&f) {
                    for l in &fnet.leaves {
                        leaves.push((f, l.clone()));
                    }
                }
            }
            if !leaves.is_empty() {
                v.push((k, leaves));
            }
        }
        v
    }

    fn leaf_place(&self, a: ClassId, k: usize, f: FieldId, l: &Leaf) -> String {
        let mut s = format!("w->{}.c{}[row].{}", self.aname(a), k, self.fname(f));
        for n in &l.path_names {
            s.push('.');
            s.push_str(&cid(n));
        }
        s
    }

    fn quant(&self, l: &Leaf, v: &str) -> String {
        match &l.quant {
            Quant::Bool => format!("(uint32_t)(({}) ? 1 : 0)", v),
            Quant::Int { lo, bits } => format!("sijo_qi({}, {}LL, {})", v, lo, bits),
            Quant::IntRaw => format!("(uint32_t)({})", v),
            Quant::Float { lo, step, maxq, .. } => format!("sijo_qf({}, {:?}, {:?}, {}u)", v, lo, step, maxq),
            Quant::FloatRaw => format!("sijo_f2bits({})", v),
            Quant::Handle => format!("sijo_hq(w, {}, {})", v, self.repl_mask()),
            Quant::Str => format!("sijo_stab_intern(&w->stab, {})", v),
            Quant::List => "0u /* list leaves: sijo_lq_* */".into(),
        }
    }

    /// archetypes whose entities exist on clients (a reference to anything else replicates as null)
    fn repl_mask(&self) -> String {
        let mut m: u64 = 0;
        for &a in &self.c.ecs.replicated_archetypes {
            if let Some(&i) = self.arch_id.get(&a) {
                m |= 1u64 << i;
            }
        }
        format!("0x{:x}ull", m)
    }

    fn dequant(&self, l: &Leaf, q: &str) -> String {
        match &l.quant {
            Quant::Bool => format!("({} != 0)", q),
            Quant::Int { lo, .. } => format!("(int32_t)((int64_t){} + ({}LL))", q, lo),
            Quant::IntRaw => format!("(int32_t){}", q),
            Quant::Float { lo, hi, step, .. } => format!("sijo_dqf({}, {:?}, {:?}, {:?})", q, lo, step, hi),
            Quant::FloatRaw => format!("sijo_bits2f({})", q),
            Quant::Handle => format!("sijo_hmark({})", q),
            Quant::Str => format!("sijo_stab_str(&w->stab, &w->sheap, {})", q),
            Quant::List => "0 /* list leaves: sijo_ldq_* */".into(),
        }
    }

    /// C expression: the 32-bit pattern of a leaf value of the given C expression
    fn value_bits(l: &Leaf, v: &str) -> String {
        match &l.quant {
            Quant::Bool => format!("(uint32_t)(({}) ? 1u : 0u)", v),
            Quant::Int { .. } | Quant::IntRaw => format!("(uint32_t)(int32_t)({})", v),
            Quant::Float { .. } | Quant::FloatRaw => format!("sijo_f2bits({})", v),
            Quant::Handle => format!("({}).idx", v),
            Quant::Str => format!("sijo_str_h32({})", v),
            Quant::List => "0u".into(),
        }
    }

    /// byte offsets (C expressions) of every bool inside a value of type `ty` starting at `base`
    fn bool_offsets(&self, ty: &Ty, base: &str, out: &mut Vec<String>) {
        match ty {
            Ty::Bool => out.push(base.to_string()),
            Ty::Struct(s) => {
                for m in &self.p.structs[*s].fields {
                    self.bool_offsets(&m.ty, &format!("{} + offsetof({}, {})", base, self.sname(*s), cid(&m.name)), out);
                }
            }
            _ => {}
        }
    }

    /// C code that counts leaves where the client differs from what the server should have sent
    fn verify_leaf(&self, a: ClassId, k: usize, f: FieldId, l: &Leaf) -> String {
        let sv = self.leaf_place(a, k, f, l).replacen("w->", "s->", 1).replace("[row]", "[srow]");
        let cv = self.leaf_place(a, k, f, l).replacen("w->", "c->", 1).replace("[row]", "[crow]");
        match &l.quant {
            Quant::Bool => format!("bad += (({sv}) != 0) != (({cv}) != 0);"),
            Quant::Int { .. } => {
                let q = self.quant(l, &sv);
                let dq = self.dequant(l, &format!("({})", q));
                format!("{{ int32_t dq = {dq}; bad += (dq != ({sv})) || (dq != ({cv})); }}")
            }
            Quant::IntRaw => format!("bad += ({sv}) != ({cv});"),
            Quant::Float { .. } => {
                let q = self.quant(l, &sv);
                let dq = self.dequant(l, &format!("({})", q));
                format!("bad += sijo_f2bits({dq}) != sijo_f2bits({cv});")
            }
            Quant::FloatRaw => format!("bad += sijo_f2bits({sv}) != sijo_f2bits({cv});"),
            Quant::Handle => format!("bad += sijo_hq_in(sijo_hq(s, {sv}, {}), in) != sijo_hv(c, m, {cv});", self.repl_mask()),
            Quant::Str => format!("bad += !sijo_str_eq({sv}, {cv});"),
            Quant::List => format!("bad += !sijo_leq_{}_f{f}(s, in, &({sv}), c, m, &({cv}));", self.aname(a)),
        }
    }

    /// clears the background bits of method `g` before it is awaited again (one frame per entity)
    fn unrush(&self, cx: &FCtx, g: FuncId) -> String {
        let mask: u32 = cx.rush.iter().filter(|(_, f)| *f == g).map(|(b, _)| *b).fold(0, |a, b| a | b);
        match (&cx.rush_bg, mask) {
            (Some(bp), m) if m != 0 => format!("{bp} &= ~{m}u; "),
            _ => String::new(),
        }
    }

    /// C statements appending the value `v` (an lvalue) of a message argument to the command
    /// buffer `b`: strings by value, lists and arrays as their element count (or the lengths of
    /// their dimensions) and elements, everything else as its bytes (the message stays in this
    /// process)
    fn msg_pack(&self, ty: &Ty, v: &str, d: usize) -> String {
        match ty {
            Ty::Str => format!("sijo_cmd_str(b, {v}); "),
            Ty::Struct(s) if self.p.contains_str(ty) => self.p.structs[*s].fields.iter().map(|m| self.msg_pack(&m.ty, &format!("({v}).{}", cid(&m.name)), d)).collect(),
            Ty::List(_) | Ty::Array(_) | Ty::MdArray(..) => {
                let el = self.p.elem_of(ty).unwrap_or(Ty::Int);
                let ec = self.elem_cty(&el);
                let head = match ty {
                    Ty::MdArray(_, r) => (0..*r as usize).map(|k| format!("sijo_cmd_data(b, &L{d}->d{k}, 4); ")).collect::<String>(),
                    _ => format!("sijo_cmd_data(b, &L{d}->len, 4); "),
                };
                let body = if el.is_list() || self.p.contains_str(&el) {
                    let inner = self.msg_pack(&el, &format!("(({ec}*)L{d}->data)[i{d}]"), d + 1);
                    format!("for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ {inner}}} ")
                } else {
                    format!("if (L{d}->len) sijo_cmd_data(b, L{d}->data, (uint32_t)((size_t)L{d}->len * sizeof({ec}))); ")
                };
                format!("{{ const SijoList* L{d} = &({v}); {head}{body}}} ")
            }
            _ => format!("sijo_cmd_data(b, &({v}), (uint32_t)sizeof({v})); "),
        }
    }

    /// C statements reading a message argument packed by msg_pack from `d`..`e_` into the place
    /// `v` (lists in the frame arena of the applying thread, or on the heap when `heap` is "1");
    /// `bad_` is set when the payload is malformed
    fn msg_unpack(&self, ty: &Ty, v: &str, heap: &str, d: usize) -> String {
        match ty {
            Ty::Str => format!("{v} = sijo_cmd_rd_str(&w->frame[0], &d, e_, &bad_); "),
            Ty::Struct(s) if self.p.contains_str(ty) => self.p.structs[*s].fields.iter().map(|m| self.msg_unpack(&m.ty, &format!("({v}).{}", cid(&m.name)), heap, d)).collect(),
            Ty::List(_) | Ty::Array(_) | Ty::MdArray(..) => {
                let el = self.p.elem_of(ty).unwrap_or(Ty::Int);
                let ec = self.elem_cty(&el);
                let rank = ty.rank().max(1);
                // the element count (or the lengths), each element taking at least one byte
                let mut o = format!("{{ int32_t m{d}[3] = {{0, 1, 1}}; for (int k{d} = 0; k{d} < {rank}; k{d}++) {{ if (e_ - d < 4) bad_ = 1; else {{ memcpy(&m{d}[k{d}], d, 4); d += 4; }} if (m{d}[k{d}] < 0) bad_ = 1; }} ");
                let _ = write!(o, "int64_t n{d} = bad_ ? 0 : sijo_md_count(m{d}[0], m{d}[1], m{d}[2]); if (n{d} < 0 || n{d} > (int64_t)(e_ - d)) {{ bad_ = 1; n{d} = 0; }} ");
                let new = if matches!(ty, Ty::MdArray(..)) {
                    format!("bad_ ? sijo_array_new(&w->frame[0], 0, (int32_t)sizeof({ec}), 0) : sijo_md_new(&w->frame[0], {heap}, (int32_t)sizeof({ec}), m{d}[0], m{d}[1], m{d}[2])")
                } else {
                    format!("sijo_array_new(&w->frame[0], {heap}, (int32_t)sizeof({ec}), (int32_t)n{d})")
                };
                let _ = write!(o, "SijoList L{d} = {new}; ");
                if el.is_list() || self.p.contains_str(&el) {
                    let inner = self.msg_unpack(&el, &format!("(({ec}*)L{d}.data)[i{d}]"), heap, d + 1);
                    let _ = write!(o, "for (int32_t i{d} = 0; i{d} < L{d}.len && !bad_; i{d}++) {{ {inner}}} ");
                } else {
                    let _ = write!(o, "size_t s{d} = (size_t)L{d}.len * sizeof({ec}); if ((size_t)(e_ - d) < s{d}) bad_ = 1; else if (s{d}) {{ memcpy(L{d}.data, d, s{d}); d += s{d}; }} ");
                }
                let _ = write!(o, "{v} = L{d}; }} ");
                o
            }
            _ => format!("if (e_ - d < (ptrdiff_t)sizeof({v})) bad_ = 1; else {{ memcpy(&({v}), d, sizeof({v})); d += sizeof({v}); }} "),
        }
    }

    /// C statements appending the canonical bytes of value `v` (type `ty`) to blob `b`. Entity
    /// references are server values: with `m == NULL` (server) the reference itself, 0 for null,
    /// dead, not replicated or (for the client whose set `in` is given) not seen; with a client map
    /// `m`, the server slot the client's handle stands for. Dictionary entries whose reference key is
    /// 0 are left out (a client cannot hold them).
    fn blob_ser(&self, ty: &Ty, v: &str, d: usize) -> String {
        match ty {
            Ty::Int | Ty::Char | Ty::Enum(_) => format!("sijo_blob_u32(b, (uint32_t)({v}));"),
            Ty::Float => format!("sijo_blob_u32(b, sijo_f2bits({v}));"),
            Ty::Bool => format!("sijo_blob_u8(b, ({v}) ? 1u : 0u);"),
            Ty::Handle(_) | Ty::NHandle(_) => format!("sijo_blob_u32(b, {});", self.blob_hval(v)),
            Ty::Str => format!("{{ SijoStr s{d} = {v}; sijo_blob_u32(b, (uint32_t)s{d}.n); sijo_blob_put(b, s{d}.p, (size_t)s{d}.n); }}"),
            Ty::Struct(s) => self.p.structs[*s].fields.iter().map(|m| self.blob_ser(&m.ty, &format!("({v}).{}", cid(&m.name)), d)).collect::<Vec<_>>().join(" "),
            Ty::Dict(k, val) => {
                let ec = self.elem_cty(ty);
                let e = format!("(({ec}*)L{d}->data)[i{d}]");
                if matches!(**k, Ty::Handle(_) | Ty::NHandle(_)) {
                    let kv = self.blob_hval(&format!("{e}.Key"));
                    let vs = self.blob_ser(val, &format!("{e}.Value"), d + 1);
                    format!("{{ const SijoList* L{d} = &({v}); size_t at{d} = b->n; uint32_t n{d} = 0; sijo_blob_u32(b, 0u); for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ uint32_t k{d} = {kv}; if (!k{d}) continue; n{d}++; sijo_blob_u32(b, k{d}); {vs} }} sijo_blob_set32(b, at{d}, n{d}); }}")
                } else {
                    let inner = self.blob_ser(&self.p.elem_of(ty).unwrap_or(Ty::Int), &e, d + 1);
                    format!("{{ const SijoList* L{d} = &({v}); sijo_blob_u32(b, (uint32_t)L{d}->len); for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ {inner} }} }}")
                }
            }
            Ty::List(el) | Ty::Array(el) => {
                let ec = self.cty(el);
                let inner = self.blob_ser(el, &format!("(({ec}*)L{d}->data)[i{d}]"), d + 1);
                format!("{{ const SijoList* L{d} = &({v}); sijo_blob_u32(b, (uint32_t)L{d}->len); for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ {inner} }} }}")
            }
            // the lengths of the dimensions, then the elements in row-major order
            Ty::MdArray(el, r) => {
                let ec = self.elem_cty(el);
                let inner = self.blob_ser(el, &format!("(({ec}*)L{d}->data)[i{d}]"), d + 1);
                let lens: String = (0..*r as usize).map(|k| format!("sijo_blob_u32(b, (uint32_t)L{d}->d{k}); ")).collect();
                format!("{{ const SijoList* L{d} = &({v}); {lens}for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ {inner} }} }}")
            }
            _ => String::new(),
        }
    }

    /// C expression: the server value of the entity reference `v` (see blob_ser)
    fn blob_hval(&self, v: &str) -> String {
        format!("(m ? sijo_hv(w, m, {v}) : sijo_hq_in(sijo_hq(w, {v}, {}), in))", self.repl_mask())
    }

    /// C statements parsing canonical bytes from p..e into the place `v` (heap lists, heap strings;
    /// entity references as server markers that sijo_lfix_* maps); `return 0` on malformed input
    fn blob_des(&self, ty: &Ty, v: &str, d: usize) -> String {
        match ty {
            Ty::Int | Ty::Char | Ty::Enum(_) => format!("{{ uint32_t u; if (!sijo_blob_rd32(&p, e, &u)) return 0; {v} = ({})u; }}", self.cty(ty)),
            Ty::Float => format!("{{ uint32_t u; if (!sijo_blob_rd32(&p, e, &u)) return 0; {v} = sijo_bits2f(u); }}"),
            Ty::Bool => format!("{{ uint32_t u; if (!sijo_blob_rd8(&p, e, &u)) return 0; {v} = u != 0; }}"),
            Ty::Handle(_) | Ty::NHandle(_) => format!("{{ uint32_t u; if (!sijo_blob_rd32(&p, e, &u)) return 0; {v} = sijo_hmark(u); }}"),
            Ty::Str => format!("{{ uint32_t n; if (!sijo_blob_rd32(&p, e, &n) || n > (uint32_t)(e - p)) return 0; {v} = sijo_str_heap(&w->sheap, (const char*)p, (int32_t)n); p += n; }}"),
            Ty::Struct(s) => self.p.structs[*s].fields.iter().map(|m| self.blob_des(&m.ty, &format!("({v}).{}", cid(&m.name)), d)).collect::<Vec<_>>().join(" "),
            Ty::List(_) | Ty::Array(_) | Ty::Dict(..) => {
                let el = self.p.elem_of(ty).unwrap_or(Ty::Int);
                // a list holds lists by value (SijoList); a dictionary holds its key/value entries
                let ec = if ty.is_dict() { self.elem_cty(ty) } else { self.cty(&el) };
                let inner = self.blob_des(&el, &format!("(({ec}*)L{d}.data)[i{d}]"), d + 1);
                let index = if ty.is_dict() {
                    let (kind, _, _) = self.dict_info(ty);
                    format!(" sijo_dict_rebuild(&L{d}, {kind}, (int32_t)sizeof({ec}));")
                } else {
                    String::new()
                };
                format!(
                    "{{ uint32_t n; if (!sijo_blob_rd32(&p, e, &n) || n > (uint32_t)(e - p) || n > (1u << 24)) return 0; SijoList L{d} = sijo_list_new(NULL, 1, (int32_t)sizeof({ec}), (int32_t)n); if (n) memset(L{d}.data, 0, (size_t)n * sizeof({ec})); L{d}.len = (int32_t)n; for (int32_t i{d} = 0; i{d} < L{d}.len; i{d}++) {{ {inner} }}{index} {v} = L{d}; }}"
                )
            }
            Ty::MdArray(el, r) => {
                let ec = self.elem_cty(el);
                let inner = self.blob_des(el, &format!("(({ec}*)L{d}.data)[i{d}]"), d + 1);
                let r = *r as usize;
                // every element takes at least one byte: the element count is bounded by the payload
                let rd: String = (0..3).map(|k| if k < r { format!("if (!sijo_blob_rd32(&p, e, &m[{k}]) || m[{k}] > (1u << 24)) return 0; ") } else { format!("m[{k}] = 1; ") }).collect();
                format!(
                    "{{ uint32_t m[3]; {rd}int64_t n = sijo_md_count(m[0], m[1], m[2]); if (n < 0 || n > (int64_t)(e - p) || n > (1 << 24)) return 0; SijoList L{d} = sijo_md_new(NULL, 1, (int32_t)sizeof({ec}), (int32_t)m[0], (int32_t)m[1], (int32_t)m[2]); for (int32_t i{d} = 0; i{d} < L{d}.len; i{d}++) {{ {inner} }} {v} = L{d}; }}"
                )
            }
            _ => "return 0;".into(),
        }
    }

    /// C statements mapping the server markers that blob_des left in the value `v` to the client's
    /// handles (null when the client does not hold the target); dictionary entries whose key maps
    /// to null are dropped and the index is rebuilt
    fn blob_fix(&self, ty: &Ty, v: &str, d: usize) -> String {
        if !self.p.has_handle(ty) {
            return String::new();
        }
        match ty {
            Ty::Handle(_) | Ty::NHandle(_) => format!("{v} = sijo_hresolve(m, {v});"),
            Ty::Struct(s) => self.p.structs[*s].fields.iter().map(|m| self.blob_fix(&m.ty, &format!("({v}).{}", cid(&m.name)), d)).collect::<Vec<_>>().join(" "),
            Ty::Dict(k, val) => {
                let (kind, _, ec) = self.dict_info(ty);
                let e = format!("(({ec}*)L{d}->data)[i{d}]");
                let fk = self.blob_fix(k, &format!("{e}.Key"), d + 1);
                let fv = self.blob_fix(val, &format!("{e}.Value"), d + 1);
                if matches!(**k, Ty::Handle(_) | Ty::NHandle(_)) {
                    format!("{{ SijoList* L{d} = &({v}); int32_t o{d} = 0; for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ {fk} {fv} if (sijo_hnull({e}.Key)) continue; if (o{d} != i{d}) (({ec}*)L{d}->data)[o{d}] = {e}; o{d}++; }} L{d}->len = o{d}; sijo_dict_rebuild(L{d}, {kind}, (int32_t)sizeof({ec})); }}")
                } else {
                    format!("{{ SijoList* L{d} = &({v}); for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ {fv} }} }}")
                }
            }
            Ty::List(el) | Ty::Array(el) => {
                let ec = self.cty(el);
                let inner = self.blob_fix(el, &format!("(({ec}*)L{d}->data)[i{d}]"), d + 1);
                format!("{{ SijoList* L{d} = &({v}); for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ {inner} }} }}")
            }
            _ => String::new(),
        }
    }

    /// serialize / quantize (intern) / dequantize / hash / compare a replicated collection field
    /// (and, when it holds entity references, map them to client handles)
    fn list_blob_fns(&mut self, n: &str, f: FieldId, ty: &Ty) {
        let ser = self.blob_ser(ty, "(*l)", 0);
        let des = self.blob_des(ty, "(*out)", 0);
        let free = self.free_code(ty, "(*out)", 0);
        let _ = writeln!(
            self.out,
            "static void sijo_lser_{n}_f{f}(SijoBlob* b, const SijoList* l, SijoWorld* w, const SijoCMap* m, const SijoSet* in) {{ (void)w; (void)m; (void)in; {ser} }}
static uint32_t sijo_lq_{n}_f{f}(SijoWorld* w, const SijoList* l) {{ SijoBlob bb = {{0}}; SijoBlob* b = &bb; sijo_lser_{n}_f{f}(b, l, w, NULL, NULL); uint32_t id = sijo_stab_intern_blob(&w->stab, b); free(bb.p); return id; }}
static uint32_t sijo_lh_{n}_f{f}(SijoWorld* w, const SijoCMap* m, const SijoSet* in, const SijoList* l) {{ SijoBlob bb = {{0}}; sijo_lser_{n}_f{f}(&bb, l, w, m, in); uint32_t h = sijo_blob_h32(&bb); free(bb.p); return h; }}
static int sijo_leq_{n}_f{f}(SijoWorld* s, const SijoSet* in, const SijoList* x, SijoWorld* c, const SijoCMap* m, const SijoList* y) {{ SijoBlob a = {{0}}, d = {{0}}; sijo_lser_{n}_f{f}(&a, x, s, NULL, in); sijo_lser_{n}_f{f}(&d, y, c, m, NULL); int eq = a.n == d.n && (a.n == 0 || memcmp(a.p, d.p, a.n) == 0); free(a.p); free(d.p); return eq; }}
static int sijo_ldes_{n}_f{f}(SijoWorld* w, SijoList* out, const uint8_t* p, const uint8_t* e) {{ {des} (void)w; return p == e; }}
static void sijo_ldq_{n}_f{f}(SijoWorld* w, SijoList* out, uint32_t id) {{
  const SijoSTE* x = id ? sijo_stab_find(&w->stab, id) : NULL;
  if (!x) sijo_panic(\"replicated list missing on the client\");
  {free}
  if (!sijo_ldes_{n}_f{f}(w, out, (const uint8_t*)x->p, (const uint8_t*)x->p + x->n)) sijo_panic(\"malformed replicated list\");
}}"
        );
        if self.p.has_handle(ty) {
            let fix = self.blob_fix(ty, "(*l)", 0);
            let _ = writeln!(self.out, "static void sijo_lfix_{n}_f{f}(const SijoCMap* m, SijoList* l) {{ {fix} }}");
        }
    }

    fn replication(&mut self) {
        self.w("\n/* ======================= replication (inferred + quantized) ======================= */\n");
        let archs = self.c.ecs.archetypes.clone();
        let mut maxleaf = 1;
        let mut repl_flags = vec![];
        for &a in &archs {
            let n = self.aname(a);
            let comps = self.arch_leaves(a);
            let total: usize = comps.iter().map(|(_, l)| l.len()).sum();
            maxleaf = maxleaf.max(total);
            let repl = self.c.ecs.replicated_archetypes.contains(&a);
            repl_flags.push(if repl { "1" } else { "0" });
            // a string leaf is marked SIJO_LEAF_STR (255): an id plus the bytes when it is sent
            let bits: Vec<String> = comps.iter().flat_map(|(_, ls)| ls.iter().map(|(_, l)| if matches!(l.quant, Quant::Str | Quant::List) { "255".to_string() } else { l.quant.bits().to_string() })).collect();
            // list leaves: canonical bytes (serialize, parse, intern, hash, compare)
            for (_, leaves) in &comps {
                for (f, l) in leaves {
                    if l.quant == Quant::List {
                        self.list_blob_fns(&n, *f, &l.ty);
                    }
                }
            }
            let raw: usize = comps.iter().flat_map(|(_, ls)| ls.iter()).count() * 4;
            let _ = writeln!(self.out, "#define SIJO_NLEAF_{n} {total}\n#define SIJO_NCOMP_{n} {}\n#define SIJO_RAWB_{n} {raw}", comps.len());
            let _ = writeln!(self.out, "static const uint8_t sijo_bits_{n}[] = {{ {}0 }};", bits.iter().map(|b| format!("{b}, ")).collect::<String>());
            // lost ticks: floats are interpolated between the snapshots around them, the rest held
            let lerp: String = comps
                .iter()
                .flat_map(|(_, ls)| ls.iter())
                .map(|(_, l)| match l.quant {
                    Quant::Float { .. } => "1, ",
                    Quant::FloatRaw => "2, ",
                    _ => "0, ",
                })
                .collect();
            let _ = writeln!(self.out, "static const uint8_t sijo_lerp_{n}[] = {{ {lerp}0 }};");
            // quantize all leaves of a row
            let _ = writeln!(self.out, "static void sijo_q_{n}(SijoWorld* w, int32_t row, uint32_t* q) {{");
            let mut j = 0;
            for (k, leaves) in &comps {
                for (f, l) in leaves {
                    let place = self.leaf_place(a, *k, *f, l);
                    if l.quant == Quant::List {
                        let _ = writeln!(self.out, "  q[{}] = sijo_lq_{n}_f{f}(w, &({place}));", j);
                    } else {
                        let _ = writeln!(self.out, "  q[{}] = {};", j, self.quant(l, &place));
                    }
                    j += 1;
                }
            }
            self.w("  (void)w; (void)row; (void)q;\n}\n");
            // dequantize selected components
            let _ = writeln!(self.out, "static void sijo_dq_{n}(SijoWorld* w, int32_t row, const uint32_t* q, uint32_t mask) {{");
            let mut j = 0;
            for (ci, (k, leaves)) in comps.iter().enumerate() {
                let _ = writeln!(self.out, "  if (mask & {}u) {{", 1u32 << ci);
                for (f, l) in leaves {
                    let place = self.leaf_place(a, *k, *f, l);
                    if l.quant == Quant::List {
                        let _ = writeln!(self.out, "    sijo_ldq_{n}_f{f}(w, &({place}), q[{j}]);");
                    } else {
                        let dq = self.dequant(l, &format!("q[{}]", j));
                        // a declared int range holds on clients too, whatever a datagram says
                        // (the bits of a quantized value can hold more than hi - lo)
                        let dq = match (self.c.net.clamp.get(f), &l.quant) {
                            (Some(&(lo, hi)), Quant::Int { .. }) if l.ty == Ty::Int => format!("sijo_clampi({dq}, {}, {})", lo.ceil() as i64, hi.floor() as i64),
                            _ => dq,
                        };
                        let _ = writeln!(self.out, "    {} = {};", place, dq);
                    }
                    j += 1;
                }
                self.w("  }\n");
            }
            self.w("  (void)w; (void)row; (void)q; (void)mask;\n}\n");
            // 32-bit patterns of the values a client ends up with: dequantized (server side, from
            // quantized values) and as stored (client side, from its world) -> replicated-state hash
            let _ = writeln!(self.out, "static void sijo_dqbits_{n}(const SijoSTab* t, const uint32_t* q, uint32_t* o, const SijoSet* in) {{");
            let mut j = 0;
            for (_, leaves) in &comps {
                for (_, l) in leaves {
                    if l.quant == Quant::Handle {
                        let _ = writeln!(self.out, "  o[{j}] = sijo_hq_in(q[{j}], in);");
                    } else if matches!(l.quant, Quant::Str | Quant::List) {
                        let _ = writeln!(self.out, "  o[{j}] = sijo_stab_h32(t, q[{j}]);");
                    } else {
                        let _ = writeln!(self.out, "  o[{j}] = {};", Self::value_bits(l, &self.dequant(l, &format!("q[{j}]"))));
                    }
                    j += 1;
                }
            }
            self.w("  (void)t; (void)q; (void)o; (void)in;\n}\n");
            let _ = writeln!(self.out, "static void sijo_vbits_{n}(SijoWorld* w, const SijoCMap* m, int32_t row, uint32_t* o) {{");
            let mut j = 0;
            for (k, leaves) in &comps {
                for (f, l) in leaves {
                    let place = self.leaf_place(a, *k, *f, l);
                    if l.quant == Quant::Handle {
                        let _ = writeln!(self.out, "  o[{j}] = sijo_hv(w, m, {place});");
                    } else if l.quant == Quant::List {
                        let _ = writeln!(self.out, "  o[{j}] = sijo_lh_{n}_f{f}(w, m, NULL, &({place}));");
                    } else {
                        let _ = writeln!(self.out, "  o[{j}] = {};", Self::value_bits(l, &place));
                    }
                    j += 1;
                }
            }
            self.w("  (void)w; (void)m; (void)row; (void)o;\n}\n");
            // per-component leaf offsets
            let mut offs = vec![];
            let mut o = 0;
            for (_, leaves) in &comps {
                offs.push(o.to_string());
                o += leaves.len();
            }
            offs.push(o.to_string());
            // one extra entry: with no replicated component gcc's -Warray-bounds sees coff[c + 1]
            // in loops that never run
            offs.push(o.to_string());
            let _ = writeln!(self.out, "static const uint16_t sijo_coff_{n}[] = {{ {} }};", offs.join(", "));
        }
        let _ = writeln!(self.out, "#define SIJO_MAXLEAF {}", maxleaf);
        let _ = writeln!(self.out, "static const uint8_t sijo_arch_repl[] = {{ {} }};", if repl_flags.is_empty() { "0".to_string() } else { repl_flags.join(", ") });
        // archetype descriptors for the generic snapshot codec (runtime/sijo_net.h)
        let mut arch_bits = 1;
        while (1usize << arch_bits) < archs.len() {
            arch_bits += 1;
        }
        let _ = writeln!(self.out, "#define SIJO_ARCH_BITS {}", arch_bits);
        let mut descs = vec![];
        for &a in &archs {
            let n = self.aname(a);
            let repl = u8::from(self.c.ecs.replicated_archetypes.contains(&a));
            descs.push(format!("{{ SIJO_NLEAF_{n}, SIJO_NCOMP_{n}, {repl}, sijo_bits_{n}, sijo_coff_{n}, sijo_lerp_{n} }}"));
        }
        if descs.is_empty() {
            descs.push("{ 0, 0, 0, NULL, NULL, NULL }".into());
        }
        let _ = writeln!(self.out, "static const SijoArchNet sijo_archnet[SIJO_NARCH] = {{\n  {}\n}};", descs.join(",\n  "));
        let repl_archs: Vec<ClassId> = archs.iter().copied().filter(|a| self.c.ecs.replicated_archetypes.contains(a)).collect();
        // methods the compiler turned into client->server RPCs (the server accepts nothing else)
        let mut rpc_funcs = BTreeSet::new();
        for f in &self.p.funcs {
            crate::sema::visit_block(&f.body, &mut |e| {
                if let ExprKind::Call { func, dispatch: Dispatch::Rpc, .. } = &e.kind {
                    rpc_funcs.insert(*func);
                }
            });
        }
        // protocol hash: peers built from a different program (or replication plan) refuse to talk
        // 3: strings and lists may travel as deltas against the acknowledged value
        // 2: collections of entity references and dictionaries replicate
        let mut schema = String::from("sijo-net-3|");
        for &a in &archs {
            let _ = write!(schema, "{}:{}", self.p.classes[a].name, self.c.ecs.replicated_archetypes.contains(&a));
            for (k, leaves) in self.arch_leaves(a) {
                let _ = write!(schema, "[c{}", k);
                for (f, l) in leaves {
                    let _ = write!(schema, " {}.{}={:?}", self.p.field_label(f), l.path_names.join("."), l.quant);
                    if matches!(l.quant, Quant::List | Quant::Str) {
                        // the canonical bytes depend on the element types
                        let _ = write!(schema, ":{}", self.type_sig(&l.ty));
                    }
                }
                schema.push(']');
            }
            schema.push('|');
        }
        for &fid in &rpc_funcs {
            let func = &self.p.funcs[fid];
            let ps: Vec<String> = func.params.iter().map(|&l| self.type_sig(&func.locals[l].ty)).collect();
            let _ = write!(schema, "rpc {} {}({});", fid, self.p.func_label(fid), ps.join(","));
        }
        let _ = writeln!(self.out, "#define SIJO_PROTO 0x{:08x}u", fnv32(&schema));

        // ---- server: capture the replicated state of a tick ----
        if repl_archs.is_empty() {
            // nothing is replicated: no per-tick pass over the entity slots
            self.w("/* server: nothing is replicated (no client code reads simulation state) */\nstatic void sijo_net_capture(SijoWorld* s, SijoRepl* r, SijoFrame* f, int32_t tick) {\n  (void)s; (void)r;\n  sijo_frame_reset(f, tick);\n}\n");
        } else {
        self.w(
            r#"/* server: quantize this tick's replicated state into a frame. Values are re-quantized only for
   components the compiler-inserted dirty flags mark as written since the previous capture. */
static void sijo_net_capture(SijoWorld* s, SijoRepl* r, SijoFrame* f, int32_t tick) {
  sijo_repl_reserve(r, s->ents.n, SIJO_MAXLEAF);
  sijo_frame_reset(f, tick);
  uint64_t naive = 0;
  for (int32_t i = 1; i < s->ents.n; i++) {
    SijoSlot* sl = &s->ents.s[i];
    int arch = sl->state == SIJO_ALIVE ? sl->arch : -1;
    if (arch < 0 || !sijo_arch_repl[arch]) { r->gen[i] = 0; continue; }
    int32_t row = sl->row; uint32_t* q = r->q + (size_t)i * SIJO_MAXLEAF;
    int fresh = r->gen[i] != sl->gen || r->arch[i] != (uint16_t)arch;
    switch (arch) {
"#,
        );
        for &a in &repl_archs {
            let n = self.aname(a);
            // components holding a list are re-encoded every tick: lists also change in place
            // (Add, element writes), which the dirty flags do not track
            let dirty: Vec<String> = self.arch_leaves(a).iter().map(|(k, ls)| if ls.iter().any(|(_, l)| l.quant == Quant::List) { "1".to_string() } else { format!("s->{n}.d{k}[row]") }).collect();
            let dirty = if dirty.is_empty() { "0".to_string() } else { dirty.join(" || ") };
            let _ = writeln!(self.out, "    case ARCH_{n}: naive += 4 + SIJO_RAWB_{n}; if (fresh || {dirty}) sijo_q_{n}(s, row, q); break;");
        }
        self.w("    }\n    (void)row; (void)fresh;\n    r->gen[i] = sl->gen; r->arch[i] = (uint16_t)arch;\n    int nl = sijo_archnet[arch].nleaf;\n    uint32_t* fq = sijo_frame_add(f, (uint32_t)i, sl->gen, arch, nl);\n    if (nl) memcpy(fq, q, (size_t)nl * sizeof(uint32_t));\n  }\n");
        for &a in &archs {
            let n = self.aname(a);
            for (k, _) in self.arch_leaves(a) {
                let _ = writeln!(self.out, "  if (s->{n}.count) memset(s->{n}.d{k}, 0, (size_t)s->{n}.count);");
            }
        }
        self.w("  f->naive = naive;\n}\n");
        }

        // ---- client prediction: RPC methods a client may run on its own copy right away ----
        // (they read only replicated state and change only their own entity: no spawns, destroys,
        // messages, remote writes, Random or Print, on every archetype they can run on)
        let mut predictable = vec![];
        for &fid in &rpc_funcs {
            let FuncKind::EntityMethod(c) = self.p.funcs[fid].kind else { continue };
            let ok = self.p.archetypes_of(c).into_iter().filter(|a| self.c.ecs.archetypes.contains(a)).all(|a| {
                let e = self.c.info.get(fid, Some(a));
                e.spawns.is_empty()
                    && !e.destroys_self
                    && !e.restructures_self
                    && e.restructures.is_empty()
                    && e.destroys.is_empty()
                    && e.remote_writes.is_empty()
                    && e.remote_calls.is_empty()
                    && e.msg_writes.is_empty()
                    && e.rpcs.is_empty()
                    && !e.prints
                    && !e.random
                    && e.self_reads.iter().chain(e.remote_reads.iter()).all(|f| self.c.ecs.replicated.contains(f))
            });
            if ok {
                predictable.push(fid);
            }
        }
        self.w("/* client prediction: RPC methods the client also runs on its own world at once */\nstatic int sijo_rpc_predictable(uint32_t fid) {\n  switch (fid) {\n");
        for fid in &predictable {
            let _ = writeln!(self.out, "  case {fid}: return 1; /* {} */", self.p.func_label(*fid));
        }
        self.w("  default: (void)fid; return 0;\n  }\n}\n");
        // restore one client entity's replicated values from a frame (undo predictions)
        self.w("/* client: put back the snapshot values of one entity (predictions are replayed afterwards) */\nstatic void sijo_net_restore(SijoWorld* c, const SijoCMap* m, const SijoFrame* f, SijoH h) {\n  SijoSlot* cs = sijo_live(&c->ents, h);\n  if (!cs || h.idx >= (uint32_t)m->rcap || !m->rmap[h.idx]) return;\n  uint32_t si = m->rmap[h.idx];\n  int32_t lo = 0, hi = f->n - 1;\n  while (lo <= hi) {\n    int32_t mid = (lo + hi) / 2;\n    if (f->e[mid].idx < si) lo = mid + 1; else if (f->e[mid].idx > si) hi = mid - 1; else { lo = mid; break; }\n  }\n  if (lo >= f->n || f->e[lo].idx != si || f->e[lo].arch != cs->arch) return;\n  const uint32_t* q = f->q + f->e[lo].q; int32_t r = cs->row; (void)q; (void)r;\n  switch (cs->arch) {\n");
        for &a in &repl_archs {
            let n = self.aname(a);
            let mut res = String::new();
            let mut j = 0;
            for (k, leaves) in self.arch_leaves(a) {
                for (f, l) in leaves {
                    if l.quant == Quant::Handle {
                        let place = self.leaf_place(a, k, f, &l).replacen("w->", "c->", 1).replace("[row]", "[r]");
                        let _ = write!(res, " {place} = sijo_hresolve(m, sijo_hmark(q[{j}]));");
                    } else if l.quant == Quant::List && self.p.has_handle(&l.ty) {
                        let place = self.leaf_place(a, k, f, &l).replacen("w->", "c->", 1).replace("[row]", "[r]");
                        let _ = write!(res, " sijo_lfix_{n}_f{f}(m, &({place}));");
                    }
                    j += 1;
                }
            }
            let _ = writeln!(self.out, "    case ARCH_{n}: sijo_dq_{n}(c, r, q, 0xffffffffu);{res} break;");
        }
        self.w("    default: break;\n  }\n}\n");

        // ---- interest management: where an entity is (for clients that set Net.View) ----
        self.w("/* the position of the entity in server slot i, for clients that set a Net.View (0: no position:\n   such entities are always sent) */\nstatic int sijo_net_pos(SijoWorld* w, uint32_t i, float* x, float* y) {\n  SijoSlot* sl = &w->ents.s[i];\n  if (sl->state != SIJO_ALIVE) return 0;\n  int32_t r = sl->row; (void)r; (void)x; (void)y;\n  switch (sl->arch) {\n");
        for &a in &archs {
            let Some(&(f, xi)) = self.c.net.spatial.get(&a) else { continue };
            let Ty::Struct(s) = self.p.fields[f].ty else { continue };
            let n = self.aname(a);
            let k = self.comp(f);
            let xname = cid(&self.p.structs[s].fields[xi].name);
            let y = match self.p.structs[s].fields.iter().position(|m| m.name == "Y" && m.ty == Ty::Float) {
                Some(yi) => format!("w->{n}.c{k}[r].{}.{}", self.fname(f), cid(&self.p.structs[s].fields[yi].name)),
                None => "0.0f".into(),
            };
            let _ = writeln!(self.out, "    case ARCH_{n}: *x = w->{n}.c{k}[r].{}.{xname}; *y = {y}; return 1;", self.fname(f));
        }
        self.w("    default: return 0;\n  }\n}\n");

        // ---- client: move the client world from one frame to the next ----
        self.w(
            r#"static void sijo_net_despawn(SijoWorld* c, SijoCMap* m, uint32_t si) {
  SijoH h = m->cmap[si];
  if (h.idx < (uint32_t)m->rcap) m->rmap[h.idx] = 0;
  sijo_destroy(c, h); m->cmap[si] = SIJO_NULL; m->sgen[si] = 0;
}
/* client: turn frame p into frame n in the client world (despawns, spawns, changed components).
   Walks server slots in ascending order, so client-side slot allocation is deterministic. */
static void sijo_net_apply(SijoWorld* c, SijoCMap* m, const SijoFrame* p, const SijoFrame* n) {
  c->tick = n->tick;
  if (n->n) sijo_cmap_reserve(m, (int32_t)n->e[n->n - 1].idx + 1);
  int32_t i = 0, j = 0, moved = 0; /* moved: entities came or went, so references may map differently */
  while (i < p->n || j < n->n) {
    uint32_t pi = i < p->n ? p->e[i].idx : UINT32_MAX, ni = j < n->n ? n->e[j].idx : UINT32_MAX;
    if (pi < ni) { sijo_net_despawn(c, m, pi); moved = 1; i++; continue; }
    const SijoFEnt* ne = &n->e[j];
    const uint32_t* nq = n->q + ne->q;
    if (pi == ni && p->e[i].gen == ne->gen && p->e[i].arch == ne->arch) {
      const uint32_t* pq = p->q + p->e[i].q;
      const SijoArchNet* an = &sijo_archnet[ne->arch];
      uint32_t mask = 0;
      for (int ci = 0; ci < an->ncomp; ci++)
        for (int k = an->coff[ci]; k < an->coff[ci + 1]; k++) if (nq[k] != pq[k]) { mask |= 1u << ci; break; }
      if (mask) {
        SijoSlot* cs = sijo_live(&c->ents, m->cmap[ni]);
        if (!cs) sijo_panic("replicated entity missing on the client");
        switch (cs->arch) {
"#,
        );
        for &a in &repl_archs {
            let n = self.aname(a);
            let _ = writeln!(self.out, "        case ARCH_{n}: sijo_dq_{n}(c, cs->row, nq, mask); break;");
        }
        self.w("        }\n      }\n      i++; j++; continue;\n    }\n    if (pi == ni) { sijo_net_despawn(c, m, pi); i++; }\n    moved = 1;\n    int32_t row = 0; SijoH h = SIJO_NULL;\n    switch (ne->arch) {\n");
        for &a in &repl_archs {
            let n = self.aname(a);
            let _ = writeln!(self.out, "    case ARCH_{n}: h = sijo_cspawn_{n}(c, &row); sijo_dq_{n}(c, row, nq, 0xffffffffu); break;");
        }
        self.w("    default: sijo_panic(\"bad archetype in snapshot\");\n    }\n    (void)nq; (void)row;\n    m->cmap[ni] = h; m->sgen[ni] = ne->gen;\n    sijo_cmap_rreserve(m, (int32_t)h.idx + 1); m->rmap[h.idx] = ni;\n    j++;\n  }\n");
        // entity references point at server slots: now that every entity of the frame exists, map
        // each reference the frame holds to the client's handle (null while the client does not see
        // the target; resolved again with every frame, so it appears when the target comes into view)
        let mut cases = String::new();
        let mut lists = false;
        for &a in &repl_archs {
            let n = self.aname(a);
            let mut j = 0;
            let mut body = String::new();
            for (k, leaves) in self.arch_leaves(a) {
                let (lo, nl) = (j, leaves.len());
                for (f, l) in leaves {
                    let place = self.leaf_place(a, k, f, &l).replacen("w->", "c->", 1).replace("[row]", "[r]");
                    if l.quant == Quant::Handle {
                        let _ = write!(body, " {place} = sijo_hresolve(m, sijo_hmark(q[{j}]));");
                    } else if l.quant == Quant::List && self.p.has_handle(&l.ty) {
                        // the references are in the collection's bytes: read them again when the
                        // entity set changed (a target came or went) or the component holding the
                        // collection was dequantized (it then holds server markers)
                        let _ = write!(body, " if (moved || !pq || memcmp(pq + {lo}, q + {lo}, {nl} * sizeof(uint32_t))) {{ sijo_ldq_{n}_f{f}(c, &({place}), q[{j}]); sijo_lfix_{n}_f{f}(m, &({place})); }}");
                        lists = true;
                    }
                    j += 1;
                }
            }
            if !body.is_empty() {
                let _ = writeln!(cases, "      case ARCH_{n}: {{ int32_t r = cs->row;{body} break; }}");
            }
        }
        if !cases.is_empty() {
            // pq: this entity's values in the previous frame (NULL when it is new there)
            let prev = if lists {
                "    while (pk < p->n && p->e[pk].idx < e->idx) pk++;\n    const uint32_t* pq = pk < p->n && p->e[pk].idx == e->idx && p->e[pk].gen == e->gen && p->e[pk].arch == e->arch ? p->q + p->e[pk].q : NULL;\n"
            } else {
                ""
            };
            let _ = writeln!(
                self.out,
                "  int32_t pk = 0; (void)pk;\n  for (int32_t j = 0; j < n->n; j++) {{\n    const SijoFEnt* e = &n->e[j]; const uint32_t* q = n->q + e->q;\n{prev}    SijoSlot* cs = sijo_live(&c->ents, m->cmap[e->idx]);\n    if (!cs) continue;\n    switch (cs->arch) {{\n{cases}      default: break;\n    }}\n    (void)q;\n  }}"
            );
        }
        self.w("  (void)moved;\n}\n");

        // ---- replicated-state hashes (server world vs client world, keyed by server slot) ----
        self.w("/* hash of what every client should hold: dequantized replicated values keyed by server slot */\nstatic uint64_t sijo_net_hash_server(SijoWorld* s, const SijoSet* in, int64_t* ents) {\n  uint64_t h = 0xcbf29ce484222325ull; int64_t n = 0; uint32_t q[SIJO_MAXLEAF] = {0}, v[SIJO_MAXLEAF] = {0};\n  for (int32_t i = 1; i < s->ents.n; i++) {\n    SijoSlot* sl = &s->ents.s[i];\n    if (sl->state != SIJO_ALIVE || !sijo_arch_repl[sl->arch] || !sijo_inset(in, (uint32_t)i)) continue;\n    switch (sl->arch) {\n");
        for &a in &repl_archs {
            let n = self.aname(a);
            let mut fixes = String::new();
            let mut j = 0;
            for (k, leaves) in self.arch_leaves(a) {
                for (f, l) in leaves {
                    if l.quant == Quant::List && self.p.has_handle(&l.ty) {
                        let place = self.leaf_place(a, k, f, &l).replacen("w->", "s->", 1).replace("[row]", "[sl->row]");
                        let _ = write!(fixes, " v[{j}] = sijo_lh_{n}_f{f}(s, NULL, in, &({place}));");
                    }
                    j += 1;
                }
            }
            let _ = writeln!(self.out, "    case ARCH_{n}: sijo_q_{n}(s, sl->row, q); sijo_dqbits_{n}(&s->stab, q, v, in);{fixes} break;");
        }
        self.w("    }\n    h = sijo_hash_ent(h, (uint32_t)i, sl->gen, (uint32_t)sl->arch, v, sijo_archnet[sl->arch].nleaf);\n    n++;\n  }\n  h = sijo_fnv_u64(h, (uint64_t)n);\n  *ents = n;\n  return h;\n}\n");
        self.w("/* the same hash computed from a client world (values as stored, entities mapped back to server slots) */\nstatic uint64_t sijo_net_hash_client(SijoWorld* c, const SijoCMap* m, const SijoFrame* f, int64_t* ents) {\n  uint64_t h = 0xcbf29ce484222325ull; uint32_t v[SIJO_MAXLEAF] = {0};\n  for (int32_t j = 0; j < f->n; j++) {\n    const SijoFEnt* e = &f->e[j];\n    SijoSlot* cs = e->idx < (uint32_t)m->cap ? sijo_live(&c->ents, m->cmap[e->idx]) : NULL;\n    if (!cs || cs->arch != e->arch) { h = sijo_fnv(h, \"missing\", 7); continue; }\n    switch (cs->arch) {\n");
        for &a in &repl_archs {
            let n = self.aname(a);
            let _ = writeln!(self.out, "    case ARCH_{n}: sijo_vbits_{n}(c, m, cs->row, v); break;");
        }
        self.w("    }\n    h = sijo_hash_ent(h, e->idx, e->gen, (uint32_t)e->arch, v, sijo_archnet[e->arch].nleaf);\n  }\n  int64_t n = 0;\n");
        for &a in &repl_archs {
            let _ = writeln!(self.out, "  n += c->{}.count;", self.aname(a));
        }
        self.w("  h = sijo_fnv_u64(h, (uint64_t)n);\n  *ents = n;\n  return h;\n}\n");

        // ---- verify (in-process mode: compare the client world with the server's quantized state) ----
        self.w("/* compare the client's view with the server's quantized state */\nstatic int sijo_net_verify(SijoWorld* s, SijoWorld* c, const SijoCMap* m, const SijoSet* in, int64_t* ents, int64_t* vals) {\n  int bad = 0;\n  for (int32_t i = 1; i < s->ents.n; i++) {\n    SijoSlot* sl = &s->ents.s[i];\n    if (sl->state != SIJO_ALIVE || !sijo_arch_repl[sl->arch] || !sijo_inset(in, (uint32_t)i)) continue;\n    SijoSlot* cs = i < m->cap ? sijo_live(&c->ents, m->cmap[i]) : NULL;\n    if (!cs) { bad++; continue; }\n    (*ents)++;\n    switch (sl->arch) {\n");
        for &a in &repl_archs {
            let n = self.aname(a);
            let mut checks = String::new();
            for (k, leaves) in self.arch_leaves(a) {
                for (f, l) in leaves {
                    checks.push_str(&self.verify_leaf(a, k, f, &l));
                    checks.push(' ');
                }
            }
            let _ = writeln!(self.out, "    case ARCH_{n}: {{ int32_t srow = sl->row, crow = cs->row; (void)srow; (void)crow; *vals += SIJO_NLEAF_{n}; {checks}break; }}");
        }
        self.w("    }\n  }\n  return bad;\n}\n");

        // ---- RPC allow-list ----
        self.w("/* server: accept only methods the compiler turned into RPCs, with exactly the right argument size;\n   bool arguments are normalized because a client can send any byte */\nstatic int sijo_rpc_ok(uint32_t fid, uint32_t size, uint8_t* d) {\n  (void)d; (void)size;\n  switch (fid) {\n");
        for &fid in &rpc_funcs {
            let func = &self.p.funcs[fid];
            if func.params.iter().any(|&l| func.locals[l].ty == Ty::Str) {
                // walk the arguments: fixed values at the running offset, strings by their length
                let mut o = format!("  case {fid}: {{ /* {} */\n    uint32_t at_ = 0;\n", self.p.func_label(fid));
                for &l in &func.params {
                    let ty = func.locals[l].ty.clone();
                    if ty == Ty::Str {
                        o.push_str("    { if (size - at_ < 4) return 0; uint32_t n_ = sijo_rd_le32(d + at_); at_ += 4; if (n_ > size - at_) return 0; at_ += (n_ + 3u) & ~3u; if (at_ > size) return 0; }\n");
                    } else {
                        let mut offs = vec![];
                        self.bool_offsets(&ty, "at_", &mut offs);
                        let _ = writeln!(o, "    if (size - at_ < (uint32_t)sizeof({ct})) return 0;", ct = self.cty(&ty));
                        for of in offs {
                            let _ = writeln!(o, "    d[{of}] = d[{of}] != 0;");
                        }
                        let _ = writeln!(o, "    at_ += (uint32_t)sizeof({});", self.cty(&ty));
                    }
                }
                o.push_str("    return at_ == size; }\n");
                self.w(&o);
                continue;
            }
            let mut expect = vec![];
            let mut fixes = vec![];
            for &l in &func.params {
                let ty = func.locals[l].ty.clone();
                let off = if expect.is_empty() { "0".to_string() } else { expect.join(" + ") };
                let mut offs = vec![];
                self.bool_offsets(&ty, &off, &mut offs);
                for o in offs {
                    fixes.push(format!("d[{o}] = d[{o}] != 0;"));
                }
                expect.push(format!("sizeof({})", self.cty(&ty)));
            }
            let expect = if expect.is_empty() { "0".to_string() } else { expect.join(" + ") };
            let _ = writeln!(self.out, "  case {fid}: /* {} */\n    if (size != (uint32_t)({expect})) return 0;\n    {}\n    return 1;", self.p.func_label(fid), fixes.join(" "));
        }
        self.w("  default: return 0;\n  }\n}\n");
        // ---- RPC argument byte order ----
        self.w("/* RPC arguments travel as little-endian bytes: a big-endian machine byte-swaps every 4-byte value\n   (int, float, enum, handle words) when it sends and when it receives; elsewhere a no-op */\nstatic void sijo_rpc_canon(uint32_t fid, uint8_t* d, uint32_t size) {\n  (void)fid; (void)d; (void)size;\n#if defined(__BYTE_ORDER__) && __BYTE_ORDER__ == __ORDER_BIG_ENDIAN__\n  switch (fid) {\n");
        for &fid in &rpc_funcs {
            let func = &self.p.funcs[fid];
            if func.params.iter().any(|&l| func.locals[l].ty == Ty::Str) {
                // string lengths are little-endian already; the values around them move with them
                let mut o = format!("  case {fid}: {{ uint32_t at_ = 0;");
                for &l in &func.params {
                    let ty = func.locals[l].ty.clone();
                    if ty == Ty::Str {
                        o.push_str(" if (size - at_ < 4) break; { uint32_t n_ = sijo_rd_le32(d + at_); at_ += 4 + ((n_ + 3u) & ~3u); } if (at_ > size) break;");
                    } else {
                        let mut words = vec![];
                        self.word_offsets(&ty, "at_", &mut words);
                        let _ = write!(o, " if (size - at_ < (uint32_t)sizeof({})) break;", self.cty(&ty));
                        for wo in words {
                            let _ = write!(o, " sijo_bswap4(d + ({wo}));");
                        }
                        let _ = write!(o, " at_ += (uint32_t)sizeof({});", self.cty(&ty));
                    }
                }
                o.push_str(" break; }\n");
                self.w(&o);
                continue;
            }
            let mut at = vec![];
            let mut words = vec![];
            for &l in &func.params {
                let ty = func.locals[l].ty.clone();
                let off = if at.is_empty() { "0".to_string() } else { at.join(" + ") };
                self.word_offsets(&ty, &off, &mut words);
                at.push(format!("sizeof({})", self.cty(&ty)));
            }
            let swaps: Vec<String> = words.iter().map(|o| format!("sijo_bswap4(d + ({o}));")).collect();
            let _ = writeln!(self.out, "  case {fid}: {} break;", swaps.join(" "));
        }
        self.w("  }\n#endif\n}\n");
    }

    /// byte offsets (C expressions) of the 4-byte scalars inside a value of type `ty`
    fn word_offsets(&self, ty: &Ty, base: &str, out: &mut Vec<String>) {
        match ty {
            Ty::Int | Ty::Float | Ty::Char | Ty::Enum(_) => out.push(base.to_string()),
            Ty::Handle(_) | Ty::NHandle(_) => {
                out.push(base.to_string());
                out.push(format!("{} + 4", base));
            }
            Ty::Struct(s) => {
                for m in &self.p.structs[*s].fields {
                    self.word_offsets(&m.ty, &format!("{} + offsetof({}, {})", base, self.sname(*s), cid(&m.name)), out);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------
    // checksum
    // ------------------------------------------------------------------

    fn hash_code(&self, ty: &Ty, v: &str, depth: usize) -> String {
        if let (Ty::Array(_), Some(e)) = (ty, self.p.elem_of(ty)) {
            return self.hash_code(&Ty::List(Box::new(e)), v, depth);
        }
        // the lengths of the dimensions, then the elements like a list
        if let (Ty::MdArray(_, r), Some(e)) = (ty, self.p.elem_of(ty)) {
            let lens: String = (0..*r as usize).map(|k| format!("h = sijo_fnv_u32(h, (uint32_t)({v}).d{k}); ")).collect();
            return format!("{{ {lens}{} }}", self.hash_code(&Ty::List(Box::new(e)), v, depth));
        }
        if let (Ty::Dict(..), Some(e)) = (ty, self.p.elem_of(ty)) {
            return self.hash_code(&Ty::List(Box::new(e)), v, depth);
        }
        match ty {
            Ty::Int | Ty::Char | Ty::Enum(_) => format!("h = sijo_fnv_u32(h, (uint32_t)({v}));"),
            Ty::Float => format!("h = sijo_fnv_f32(h, {v});"),
            Ty::Bool => format!("{{ uint8_t x = ({v}) ? 1 : 0; h = sijo_fnv(h, &x, 1); }}"),
            Ty::Handle(_) | Ty::NHandle(_) => format!("{{ SijoH x = {v}; h = sijo_fnv_u32(h, x.idx); h = sijo_fnv_u32(h, x.gen); }}"),
            Ty::Str => format!("{{ SijoStr x = {v}; h = sijo_fnv_u32(h, (uint32_t)x.n); h = sijo_fnv(h, x.p, (size_t)x.n); }}"),
            Ty::Struct(s) => self.p.structs[*s].fields.iter().map(|m| self.hash_code(&m.ty, &format!("({}).{}", v, cid(&m.name)), depth)).collect::<Vec<_>>().join(" "),
            Ty::Delegate(d) => format!("h = sijo_d{d}_hash(h, &({v}));"),
            Ty::List(el) => {
                let i = format!("i{}", depth);
                let et = self.cty(el);
                let inner = self.hash_code(el, &format!("(({}*)({}).data)[{}]", et, v, i), depth + 1);
                format!("{{ int32_t n = ({v}).len; h = sijo_fnv_u32(h, (uint32_t)n); for (int32_t {i} = 0; {i} < n; {i}++) {{ {inner} }} }}")
            }
            _ => String::new(),
        }
    }

    // ------------------------------------------------------------------
    // live coding: save / restore world state by field name (schema migration)
    // ------------------------------------------------------------------

    fn type_sig(&self, t: &Ty) -> String {
        match t {
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Bool => "bool".into(),
            Ty::Enum(e) => format!("enum:{}", self.p.enums[*e].name),
            Ty::Char => "char".into(),
            Ty::Handle(c) | Ty::NHandle(c) => format!("handle:{}", state_name(&self.p.classes[*c].name)),
            Ty::Str => "string".into(),
            Ty::Struct(s) => {
                let ms: Vec<String> = self.p.structs[*s].fields.iter().map(|f| format!("{}:{}", f.name, self.type_sig(&f.ty))).collect();
                format!("struct:{}{{{}}}", state_name(&self.p.structs[*s].name), ms.join(","))
            }
            Ty::List(el) => format!("list<{}>", self.type_sig(el)),
            Ty::Array(el) => format!("array<{}>", self.type_sig(el)),
            Ty::MdArray(el, r) => format!("array{}<{}>", r, self.type_sig(el)),
            Ty::Dict(k, v) => format!("dict<{},{}>", self.type_sig(k), self.type_sig(v)),
            Ty::Delegate(d) => {
                let dd = &self.p.delegates[*d];
                let ps: Vec<String> = dd.params.iter().map(|t| self.type_sig(t)).collect();
                let ret = if dd.ret == Ty::Void { "void".to_string() } else { self.type_sig(&dd.ret) };
                format!("delegate:{}({}):{}", dd.name.clone().unwrap_or_default(), ps.join(","), ret)
            }
            _ => "?".into(),
        }
    }

    fn write_code(&self, t: &Ty, v: &str, depth: usize) -> String {
        if let (Ty::Dict(..) | Ty::Array(_), Some(e)) = (t, self.p.elem_of(t)) {
            return self.write_code(&Ty::List(Box::new(e)), v, depth);
        }
        // T[,]: the lengths, then the elements like a list (" 2 3 [6 ...]")
        if let (Ty::MdArray(_, r), Some(e)) = (t, self.p.elem_of(t)) {
            let lens: String = (0..*r as usize).map(|k| format!("fprintf(f, \" %d\", (int)({v}).d{k}); ")).collect();
            return format!("{lens}{}", self.write_code(&Ty::List(Box::new(e)), v, depth));
        }
        match t {
            Ty::Int | Ty::Char | Ty::Enum(_) => format!("fprintf(f, \" %d\", (int)({v}));"),
            Ty::Float => format!("fprintf(f, \" %08x\", sijo_f2bits({v}));"),
            Ty::Bool => format!("fprintf(f, \" %d\", ({v}) ? 1 : 0);"),
            Ty::Handle(_) | Ty::NHandle(_) => format!("fprintf(f, \" %u:%u\", ({v}).idx, ({v}).gen);"),
            Ty::Str => format!("{{ SijoStr x = {v}; fprintf(f, \" s\"); for (int32_t k = 0; k < x.n; k++) fprintf(f, \"%02x\", (unsigned)(unsigned char)x.p[k]); }}"),
            Ty::Struct(s) => self.p.structs[*s].fields.iter().map(|m| self.write_code(&m.ty, &format!("({}).{}", v, cid(&m.name)), depth)).collect::<Vec<_>>().join(" "),
            // `{ <label as hex> captured values }`: a function is named by its place in the source
            Ty::Delegate(d) => format!("sijo_d{d}_write(f, &({v}));"),
            Ty::List(el) => {
                let i = format!("i{}", depth);
                let et = self.cty(el);
                let inner = self.write_code(el, &format!("(({et}*)({v}).data)[{i}]"), depth + 1);
                format!("fprintf(f, \" [%d\", ({v}).len); for (int32_t {i} = 0; {i} < ({v}).len; {i}++) {{ {inner} }} fprintf(f, \" ]\");")
            }
            _ => String::new(),
        }
    }

    fn read_code(&self, t: &Ty, place: &str, depth: usize) -> String {
        if let (Ty::Dict(..), Some(e)) = (t, self.p.elem_of(t)) {
            let (kind, _, et) = self.dict_info(t);
            let fill = self.read_code(&Ty::List(Box::new(e)), place, depth);
            return format!("{fill} sijo_dict_rebuild(&{place}, {kind}, (int32_t)sizeof({et}));");
        }
        if let (Ty::Array(_), Some(e)) = (t, self.p.elem_of(t)) {
            return self.read_code(&Ty::List(Box::new(e)), place, depth);
        }
        if let (Ty::MdArray(_, r), Some(e)) = (t, self.p.elem_of(t)) {
            let r = *r as usize;
            let lens: String = (0..3).map(|k| if k < r { format!("int64_t m{depth}_{k} = sijo_tok_i64(r); ") } else { format!("int64_t m{depth}_{k} = 1; ") }).collect();
            let fill = self.read_code(&Ty::List(Box::new(e)), place, depth);
            return format!(
                "{{ {lens}{fill} if (!sijo_md_lens_ok(m{depth}_0, m{depth}_1, m{depth}_2, ({place}).len)) r->bad = 1; else {{ ({place}).d0 = (int32_t)m{depth}_0; ({place}).d1 = (int32_t)m{depth}_1; ({place}).d2 = (int32_t)m{depth}_2; }} }}"
            );
        }
        match t {
            Ty::Int | Ty::Char | Ty::Enum(_) => format!("{place} = (int32_t)sijo_tok_i64(r);"),
            Ty::Float => format!("{place} = sijo_bits2f((uint32_t)sijo_tok_hex(r));"),
            Ty::Bool => format!("{place} = sijo_tok_i64(r) != 0;"),
            Ty::Handle(_) | Ty::NHandle(_) => format!("{place} = sijo_tok_handle(r);"),
            Ty::Str => format!("{place} = sijo_str_unhex(&w->sheap, sijo_tok(r));"),
            Ty::Struct(s) => self.p.structs[*s].fields.iter().map(|m| self.read_code(&m.ty, &format!("({}).{}", place, cid(&m.name)), depth)).collect::<Vec<_>>().join(" "),
            // a function this build does not have (renamed, moved, captures changed): the default value
            Ty::Delegate(d) => format!("sijo_d{d}_read(w, r, &({place}));"),
            Ty::List(el) => {
                let i = format!("i{}", depth);
                let et = self.cty(el);
                // push once per element, then fill every member of that element
                let inner = self.read_code(el, &format!("(*e{depth})"), depth + 1);
                format!("{{ int32_t n{depth} = sijo_tok_list(r); {place} = sijo_list_new(NULL, 1, (int32_t)sizeof({et}), n{depth}); for (int32_t {i} = 0; {i} < n{depth}; {i}++) {{ {et}* e{depth} = ({et}*)sijo_list_push(NULL, &{place}, (int32_t)sizeof({et})); memset(e{depth}, 0, sizeof({et})); {inner} }} sijo_tok_expect(r, \"]\"); }}")
            }
            _ => String::new(),
        }
    }

    /// `[Persistent]` (docs/05 §5.7.11): per class with a `[PersistentKey]`, an in-memory store of
    /// records by key, read from `--store DIR` the first time it is needed; spawned entities take
    /// their record's values at the end of the command application that made them (every shard
    /// the same way, in the same order); at the end of every tick and when an entity is destroyed
    /// its values go into the record; every `--store-every` ticks and when the world ends, the
    /// writing world (not a client, shard 0 of a sharded run) rewrites the files that changed
    /// (`DIR/<Class>.store`, fields by name and type signature, like state files).
    fn persistence(&mut self) {
        if self.p.persist.is_empty() {
            return;
        }
        let entries = self.p.persist.clone();
        let mut o = String::from("\n/* ======================= [Persistent] fields: the store (--store DIR) ======================= */\nstatic uint32_t sijo_pkh(SijoStr k) { uint32_t h = 2166136261u; for (int32_t i = 0; i < k.n; i++) { h ^= (uint8_t)k.p[i]; h *= 16777619u; } return h; }\n");
        o.push_str("static void sijo_tok_skip_value(SijoTok* r) { while (!r->bad && r->pos < r->len && !sijo_tok_peek_is(r, \"|\") && !sijo_tok_peek_is(r, \"rec\") && !sijo_tok_peek_is(r, \"end\")) sijo_tok(r); }\n");
        o.push_str("static void sijo_popen(SijoWorld* w);\n");
        for (i, (c, key, pers, flag)) in entries.iter().enumerate() {
            let cname = self.p.classes[*c].name.clone();
            let _ = write!(
                o,
                "static int32_t sijo_pfind{i}(SijoPStore{i}* s, SijoStr k) {{\n  if (!s->icap) return -1;\n  uint32_t m = (uint32_t)s->icap - 1u, j = sijo_pkh(k) & m;\n  while (s->ix[j] >= 0) {{ if (sijo_str_eq(s->r[s->ix[j]].key, k)) return s->ix[j]; j = (j + 1u) & m; }}\n  return -1;\n}}\n\
static void sijo_pindex{i}(SijoPStore{i}* s) {{\n  int32_t c = 16; while (c < 2 * (s->n + 1)) c *= 2;\n  free(s->ix); s->ix = (int32_t*)sijo_xrealloc(NULL, (size_t)c * sizeof(int32_t)); s->icap = c;\n  for (int32_t j = 0; j < c; j++) s->ix[j] = -1;\n  for (int32_t q = 0; q < s->n; q++) {{ uint32_t j = sijo_pkh(s->r[q].key) & (uint32_t)(c - 1); while (s->ix[j] >= 0) j = (j + 1u) & (uint32_t)(c - 1); s->ix[j] = q; }}\n}}\n\
static int32_t sijo_padd{i}(SijoWorld* w, SijoPStore{i}* s, SijoStr k) {{\n  if (s->n == s->cap) {{ s->cap = s->cap ? s->cap * 2 : 16; s->r = (SijoPRec{i}*)sijo_xrealloc(s->r, (size_t)s->cap * sizeof(SijoPRec{i})); }}\n  SijoPRec{i}* r = &s->r[s->n]; memset(r, 0, sizeof *r); r->key = sijo_str_heap(&w->sheap, k.p, k.n);\n  s->n++; sijo_pindex{i}(s);\n  return s->n - 1;\n}}\n"
            );
            // take: the entity's values into its record
            let _ = writeln!(o, "/* {cname}: an entity's [Persistent] values into the record of its key */\nstatic void sijo_ptake{i}(SijoWorld* w, int arch, int32_t row) {{\n  sijo_popen(w);\n  SijoPStore{i}* s = &w->pst{i};\n  switch (arch) {{");
            let fam: Vec<ClassId> = self.p.family(*c).into_iter().filter(|a| self.c.ecs.archetypes.contains(a)).collect();
            for &a in &fam {
                let n = self.aname(a);
                let kp = format!("w->{n}.c{}[row].{}", self.comp(*key), self.fname(*key));
                let fp = format!("w->{n}.c{}[row].{}", self.comp(*flag), self.fname(*flag));
                let _ = write!(o, "  case ARCH_{n}: {{\n    if (!{fp}) return;\n    int32_t j = sijo_pfind{i}(s, {kp}); if (j < 0) j = sijo_padd{i}(w, s, {kp});\n    SijoPRec{i}* r = &s->r[j];\n");
                for &f in pers {
                    let ty = self.p.fields[f].ty.clone();
                    let src = format!("w->{n}.c{}[row].{}", self.comp(f), self.fname(f));
                    if ty.is_list() {
                        let _ = writeln!(o, "    {} r->f{f} = {};", self.free_code(&ty, &format!("r->f{f}"), 0), self.clone_code(&ty, &src, "NULL", "1", 0));
                    } else {
                        let _ = writeln!(o, "    r->f{f} = {src};");
                    }
                }
                let _ = write!(o, "    memset(r->has, 1, sizeof r->has);\n");
                o.push_str("    s->dirty = 1;\n    break; }\n");
            }
            o.push_str("  default: break;\n  }\n}\n");
        }
        // load: entities that have not met the store yet
        o.push_str("static void sijo_pload(SijoWorld* w) {\n  sijo_popen(w);\n");
        for (i, (c, key, pers, flag)) in entries.iter().enumerate() {
            let fam: Vec<ClassId> = self.p.family(*c).into_iter().filter(|a| self.c.ecs.archetypes.contains(a)).collect();
            for &a in &fam {
                let n = self.aname(a);
                let kp = format!("w->{n}.c{}[row].{}", self.comp(*key), self.fname(*key));
                let fp = format!("w->{n}.c{}[row].{}", self.comp(*flag), self.fname(*flag));
                let _ = write!(o, "  for (int32_t row = 0; row < w->{n}.count; row++) {{\n    if ({fp}) continue;\n    {fp} = 1;\n    int32_t j = sijo_pfind{i}(&w->pst{i}, {kp});\n    if (j < 0) continue;\n    SijoPRec{i}* r = &w->pst{i}.r[j]; (void)r;\n");
                let mut dirty: BTreeSet<usize> = BTreeSet::new();
                for (q, &f) in pers.iter().enumerate() {
                    let ty = self.p.fields[f].ty.clone();
                    let k = self.comp(f);
                    let place = format!("w->{n}.c{k}[row].{}", self.fname(f));
                    let _ = write!(o, "    if (r->has[{q}]) {{ ");
                    if ty.is_list() {
                        let _ = write!(o, "{} {place} = {};", self.free_code(&ty, &place, 0), self.clone_code(&ty, &format!("r->f{f}"), "NULL", "1", 0));
                    } else {
                        let _ = write!(o, "{place} = r->f{f};");
                    }
                    let cl = self.clamp_code(f, &format!("(&{place})"));
                    if !cl.is_empty() {
                        let _ = write!(o, " {cl}");
                    }
                    o.push_str(" }\n");
                    if self.c.ecs.components[k].replicated {
                        dirty.insert(k);
                    }
                }
                for k in dirty {
                    let _ = writeln!(o, "    w->{n}.d{k}[row] = 1;");
                }
                o.push_str("  }\n");
            }
        }
        o.push_str("}\n");
        // end of a tick: every entity's values into the store; write it every --store-every ticks
        o.push_str("static void sijo_pwrite(SijoWorld* w);\nstatic void sijo_ptick(SijoWorld* w) {\n  if (!sijo_store_dir || w->is_client) return;\n");
        for (i, (c, ..)) in entries.iter().enumerate() {
            for a in self.p.family(*c).into_iter().filter(|a| self.c.ecs.archetypes.contains(a)) {
                let n = self.aname(a);
                let _ = writeln!(o, "  for (int32_t row = 0; row < w->{n}.count; row++) sijo_ptake{i}(w, ARCH_{n}, row);");
            }
        }
        o.push_str("  if ((!w->sh_on || w->sh_id == 0) && w->tick % sijo_store_every == 0) sijo_pwrite(w);\n}\n");
        // the files
        o.push_str("static void sijo_pwrite(SijoWorld* w) {\n  if (!sijo_store_dir || !w->pst_open) return;\n  char path[4096], tmp[4200];\n");
        for (i, (c, _, pers, _)) in entries.iter().enumerate() {
            let cname = self.p.classes[*c].name.clone();
            let file = state_name(&cname).replace(['<', '>', ',', '/', '\\', ':', '*', '?', '"', '|', '+'], "_");
            let _ = writeln!(o, "  if (w->pst{i}.dirty) {{\n    SijoPStore{i}* s = &w->pst{i};\n    snprintf(path, sizeof path, \"%s/{file}.store\", sijo_store_dir); snprintf(tmp, sizeof tmp, \"%s.tmp\", path);\n    FILE* f = fopen(tmp, \"w\");\n    if (!f) fprintf(stderr, \"cannot write the store %s\\n\", tmp);\n    else {{\n      fprintf(f, \"SIJO-STORE 1\\nclass %s\\nfields {}\\n\", {});", pers.len(), c_str(&state_name(&cname)));
            for &f in pers {
                let _ = writeln!(o, "      fprintf(f, \"field %s %s\\n\", {}, {});", c_str(&self.p.fields[f].name), c_str(&self.type_sig(&self.p.fields[f].ty)));
            }
            let _ = writeln!(o, "      fprintf(f, \"records %d\\n\", (int)s->n);\n      for (int32_t j = 0; j < s->n; j++) {{\n        SijoPRec{i}* r = &s->r[j]; fprintf(f, \"rec\"); {}", self.write_code(&Ty::Str, "r->key", 0));
            for (q, &f) in pers.iter().enumerate() {
                let _ = writeln!(o, "        fprintf(f, \" |\"); if (!r->has[{q}]) fprintf(f, \" ~\"); else {{ {} }}", self.write_code(&self.p.fields[f].ty.clone(), &format!("r->f{f}"), 0));
            }
            o.push_str("        fprintf(f, \"\\n\");\n      }\n      fprintf(f, \"end\\n\");\n      int ok = !ferror(f); fclose(f);\n      if (ok) { remove(path); if (rename(tmp, path) != 0) fprintf(stderr, \"cannot write the store %s\\n\", path); else s->dirty = 0; }\n    }\n  }\n");
        }
        o.push_str("}\n");
        // reading them (the first time a world needs the store)
        o.push_str("static void sijo_popen(SijoWorld* w) {\n  if (w->pst_open || !sijo_store_dir) return;\n  w->pst_open = 1;\n  char path[4096];\n");
        for (i, (c, _, pers, _)) in entries.iter().enumerate() {
            let cname = self.p.classes[*c].name.clone();
            let file = state_name(&cname).replace(['<', '>', ',', '/', '\\', ':', '*', '?', '"', '|', '+'], "_");
            let _ = writeln!(o, "  {{\n    snprintf(path, sizeof path, \"%s/{file}.store\", sijo_store_dir);\n    FILE* f = fopen(path, \"rb\");\n    if (f) {{\n      fseek(f, 0, SEEK_END); long len = ftell(f); fseek(f, 0, SEEK_SET);\n      char* buf = (char*)sijo_xrealloc(NULL, (size_t)len + 1); size_t got = fread(buf, 1, (size_t)len, f); buf[got] = 0; fclose(f);\n      SijoTok tr = {{buf, 0, got, 0}}; SijoTok* r = &tr;\n      sijo_tok_expect(r, \"SIJO-STORE\"); sijo_tok_expect(r, \"1\"); sijo_tok_expect(r, \"class\"); (void)sijo_tok(r); sijo_tok_expect(r, \"fields\");\n      int32_t nf = (int32_t)sijo_tok_i64(r); int map[256];\n      for (int32_t q = 0; q < nf && q < 256 && !r->bad; q++) {{\n        sijo_tok_expect(r, \"field\"); char fname[256], fsig[1024];\n        snprintf(fname, sizeof fname, \"%s\", sijo_tok(r)); snprintf(fsig, sizeof fsig, \"%s\", sijo_tok(r)); map[q] = -1;");
            for (q, &f) in pers.iter().enumerate() {
                let _ = writeln!(o, "        if (!strcmp(fname, {}) && !strcmp(fsig, {})) map[q] = {q};", c_str(&self.p.fields[f].name), c_str(&self.type_sig(&self.p.fields[f].ty)));
            }
            let _ = writeln!(o, "      }}\n      if (nf > 256) r->bad = 1;\n      sijo_tok_expect(r, \"records\"); int32_t nr = (int32_t)sijo_tok_i64(r);\n      SijoPStore{i}* s = &w->pst{i};\n      for (int32_t j = 0; j < nr && !r->bad; j++) {{\n        sijo_tok_expect(r, \"rec\"); SijoStr k = sijo_str_unhex(&w->sheap, sijo_tok(r));\n        int32_t at = sijo_pfind{i}(s, k); if (at < 0) at = sijo_padd{i}(w, s, k);\n        SijoPRec{i}* rec = &s->r[at];\n        for (int32_t q = 0; q < nf && !r->bad; q++) {{\n          sijo_tok_expect(r, \"|\");\n          switch (map[q]) {{");
            for (q, &f) in pers.iter().enumerate() {
                let ty = self.p.fields[f].ty.clone();
                let place = format!("rec->f{f}");
                let free = if ty.is_list() { self.free_code(&ty, &place, 0) } else { String::new() };
                let _ = writeln!(o, "          case {q}: if (sijo_tok_peek_is(r, \"~\")) {{ (void)sijo_tok(r); break; }} {free} {}; rec->has[{q}] = 1; break;", self.read_code(&ty, &place, 0));
            }
            let _ = writeln!(o, "          default: sijo_tok_skip_value(r); break;\n          }}\n        }}\n      }}\n      sijo_tok_expect(r, \"end\");\n      if (r->bad) fprintf(stderr, \"warning: the store %s is malformed; its records after the problem are ignored\\n\", path);\n      free(buf);\n    }}\n  }}");
        }
        o.push_str("}\n");
        // world end: the last write, then the records' memory
        o.push_str("/* the end of a run: the writing world writes what changed */\nstatic void sijo_pflush(SijoWorld* w) {\n  if (sijo_store_dir && w->pst_open && !w->is_client && (!w->sh_on || w->sh_id == 0)) sijo_pwrite(w);\n}\nstatic void sijo_pclose(SijoWorld* w) {\n  sijo_pflush(w);\n");
        for (i, (_, _, pers, _)) in entries.iter().enumerate() {
            let _ = writeln!(o, "  for (int32_t j = 0; j < w->pst{i}.n; j++) {{ SijoPRec{i}* r = &w->pst{i}.r[j]; (void)r;");
            for &f in pers {
                let ty = self.p.fields[f].ty.clone();
                if ty.is_list() {
                    let _ = writeln!(o, "    {}", self.free_code(&ty, &format!("r->f{f}"), 0));
                }
            }
            let _ = writeln!(o, "  }}\n  free(w->pst{i}.r); free(w->pst{i}.ix); memset(&w->pst{i}, 0, sizeof w->pst{i});");
        }
        o.push_str("}\n");
        self.out.push_str(&o);
    }

    /// a field's name in state files: component fields are named `Component.Field` (a class and a
    /// component may both have a field `Left`)
    fn state_field_name(&self, f: FieldId) -> String {
        let fd = &self.p.fields[f];
        if self.p.classes[fd.class].is_component {
            format!("{}.{}", self.p.classes[fd.class].name, fd.name)
        } else {
            fd.name.clone()
        }
    }

    fn state_io(&mut self) {
        self.w(STATE_IO_RUNTIME);
        self.dg_helpers("write");
        self.dg_helpers("read");
        self.persistence();
        if self.has_comps() {
            self.w("/* `Class+C1+C2` saved, not in this program: the archetype of Class whose components are the\n   largest subset of C1, C2 (-1: none). An open table of Class takes them all as bits (`obits`);\n   `dropped`: some saved component has no place */\nstatic int sijo_arch_fallback(const char* aname, int32_t* obits, int* dropped) {\n  char buf[256]; snprintf(buf, sizeof buf, \"%s\", aname);\n  const char* parts[32]; int np = 0; char* p = buf; parts[np++] = p;\n  while ((p = strchr(p, '+')) != NULL && np < 32) { *p++ = 0; parts[np++] = p; }\n  int best = -1, bestn = -1; *obits = 0; *dropped = 1;\n");
            for &a in &self.c.ecs.archetypes.clone() {
                let root = self.p.root_class(a);
                if self.p.open.contains_key(&a) {
                    // every saved component the open table has becomes a bit
                    let mut tests = String::new();
                    for (b, &k) in self.p.classes[a].comps.iter().enumerate() {
                        let _ = write!(tests, " if (!strcmp(parts[j], {})) {{ bits |= (int32_t)(1u << {b}); found = 1; }}", c_str(&self.p.classes[k].name));
                    }
                    let _ = writeln!(self.out, "  if (!strcmp(parts[0], {})) {{ int32_t bits = 0; int all = 1; for (int j = 1; j < np; j++) {{ int found = 0;{tests} all &= found; }} *obits = bits; *dropped = !all; return ARCH_{}; }}", c_str(&self.p.classes[root].name), self.aname(a));
                    continue;
                }
                let comps: Vec<String> = self.p.classes[a].comps.iter().map(|&k| c_str(&self.p.classes[k].name)).collect();
                let tests: String = comps.iter().map(|n| format!(" {{ int f_ = 0; for (int j = 1; j < np; j++) if (!strcmp(parts[j], {n})) f_ = 1; ok &= f_; }}")).collect();
                let _ = writeln!(self.out, "  if (!strcmp(parts[0], {})) {{ int ok = 1;{tests} if (ok && {} > bestn) {{ best = ARCH_{}; bestn = {}; }} }}", c_str(&self.p.classes[root].name), comps.len(), self.aname(a), comps.len());
            }
            self.w("  return best;\n}\n");
        }
        // save
        self.w("static int sijo_save_state(SijoWorld* w, const char* path) {\n  FILE* f = fopen(path, \"w\");\n  if (!f) return 0;\n");
        self.w("  fprintf(f, \"SIJO-STATE 1\\ntick %d\\nseed %llu\\nslots %d\\n\", w->tick, (unsigned long long)w->seed, w->ents.n);\n");
        self.w("  for (int32_t i = 1; i < w->ents.n; i++) fprintf(f, \"%u%c\", w->ents.s[i].gen, i % 16 == 0 ? '\\n' : ' ');\n");
        self.w("  fprintf(f, \"\\nfree %d\", w->ents.nfree);\n  for (int32_t i = 0; i < w->ents.nfree; i++) fprintf(f, \" %d\", w->ents.freel[i]);\n  fprintf(f, \"\\n\");\n");
        for &a in &self.c.ecs.archetypes.clone() {
            let n = self.aname(a);
            let fields: Vec<FieldId> = self.p.all_fields(a).into_iter().filter(|&f| !self.c.ecs.is_dead(f)).collect();
            let _ = writeln!(self.out, "  fprintf(f, \"arch %s %d %d\\n\", {}, w->{n}.count, {});", c_str(&state_name(&self.p.classes[a].name)), fields.len());
            for &f in &fields {
                let _ = writeln!(self.out, "  fprintf(f, \"field %s %s\\n\", {}, {});", c_str(&self.state_field_name(f)), c_str(&self.type_sig(&self.p.fields[f].ty)));
            }
            let _ = writeln!(self.out, "  for (int32_t r = 0; r < w->{n}.count; r++) {{\n    fprintf(f, \"row %u %u\", w->{n}.ent[r].idx, w->{n}.ent[r].gen);");
            for &f in &fields {
                let k = self.comp(f);
                let code = self.write_code(&self.p.fields[f].ty, &format!("w->{}.c{}[r].{}", n, k, self.fname(f)), 0);
                let _ = writeln!(self.out, "    fprintf(f, \" |\"); {}", code);
            }
            self.w("    fprintf(f, \"\\n\");\n  }\n");
        }
        self.w("  fprintf(f, \"end\\n\");\n  fclose(f);\n  return 1;\n}\n");
        // load
        self.w(STATE_LOAD_HEAD);
        for &a in &self.c.ecs.archetypes.clone() {
            let _ = writeln!(self.out, "    if (!strcmp(aname, {})) arch = ARCH_{};", c_str(&state_name(&self.p.classes[a].name)), self.aname(a));
        }
        if self.has_comps() {
            // a combination this program does not have (a component renamed or removed, or no
            // longer reachable): the table of the same class with the most of the saved components
            self.w("    int32_t obits_ = 0; int odrop_ = 0; (void)obits_;\n    if (arch < 0) { arch = sijo_arch_fallback(aname, &obits_, &odrop_); if (arch >= 0 && odrop_) sijo_note(report, \" components-dropped:\", aname, \"\"); }\n");
        }
        self.w("    for (int32_t j = 0; j < nfields && j < 256; j++) {\n      sijo_tok_expect(r, \"field\"); char fname[256], fsig[1024];\n      snprintf(fname, sizeof fname, \"%s\", sijo_tok(r)); snprintf(fsig, sizeof fsig, \"%s\", sijo_tok(r));\n      map[j] = -1;\n      switch (arch) {\n");
        for &a in &self.c.ecs.archetypes.clone() {
            let fields: Vec<FieldId> = self.p.all_fields(a).into_iter().filter(|&f| !self.c.ecs.is_dead(f)).collect();
            let _ = write!(self.out, "      case ARCH_{}:", self.aname(a));
            for (i, &f) in fields.iter().enumerate() {
                let _ = write!(self.out, " if (!strcmp(fname, {}) && !strcmp(fsig, {})) map[j] = {};", c_str(&self.state_field_name(f)), c_str(&self.type_sig(&self.p.fields[f].ty)), i);
            }
            self.w(" break;\n");
        }
        self.w("      }\n      if (map[j] < 0 && arch >= 0) sijo_note(report, \" dropped:\", aname, fname);\n    }\n");
        self.w("    if (arch < 0) sijo_note(report, \" dropped-class:\", aname, \"\");\n    switch (arch) {\n");
        for &a in &self.c.ecs.archetypes.clone() {
            let n = self.aname(a);
            let fields: Vec<FieldId> = self.p.all_fields(a).into_iter().filter(|&f| !self.c.ecs.is_dead(f)).collect();
            let sat_decl = if fields.iter().any(|f| self.c.net.clamp.contains_key(f)) { format!(", saturated[{}] = {{0}}", fields.len()) } else { String::new() };
            let _ = writeln!(self.out, "    case ARCH_{n}: {{\n      int present[{}] = {{0}}{sat_decl};\n      for (int32_t j = 0; j < nfields && j < 256; j++) if (map[j] >= 0) present[map[j]] = 1;", fields.len().max(1));
            for (i, &f) in fields.iter().enumerate() {
                // an open table's component columns and bits are there for the rows that have them
                if self.p.open.get(&a) == Some(&f) || (self.p.open.contains_key(&a) && self.p.classes[self.p.fields[f].class].is_component) {
                    continue;
                }
                let _ = writeln!(self.out, "      if (!present[{i}]) sijo_note(report, \" new:\", {}, \"\");", c_str(&self.p.field_label(f)));
            }
            let _ = writeln!(self.out, "      for (int32_t k = 0; k < count && !r->bad; k++) {{\n        sijo_tok_expect(r, \"row\"); uint32_t idx = (uint32_t)sijo_tok_i64(r), gen = (uint32_t)sijo_tok_i64(r);\n        if (idx == 0 || idx >= (uint32_t)w->ents.n || w->ents.s[idx].state == SIJO_ALIVE) {{ r->bad = 1; break; }}\n        int32_t row = sijo_push_{n}(&w->{n});\n        SijoH h = {{idx, gen}}; w->{n}.ent[row] = h;\n        {{ SijoSlot* sl = &w->ents.s[idx]; sl->gen = gen; sl->arch = ARCH_{n}; sl->row = row; sl->state = SIJO_ALIVE; }}\n        for (int32_t j = 0; j < nfields; j++) {{\n          sijo_tok_expect(r, \"|\");\n          switch (map[j]) {{");
            for (i, &f) in fields.iter().enumerate() {
                let k = self.comp(f);
                let place = format!("w->{}.c{}[row].{}", n, k, self.fname(f));
                let mut code = self.read_code(&self.p.fields[f].ty, &place, 0);
                if self.p.fields[f].ty.is_list() {
                    // the new row holds the field's initial collection (`int[] F = new int[4]`)
                    code = format!("{} {code}", self.free_code(&self.p.fields[f].ty, &place, 0));
                }
                // a range added or narrowed since the state was saved: saturate the old value like
                // every other write (and say so), so the contract holds from the first tick
                let cl = self.clamp_code(f, &format!("(&{place})"));
                if cl.is_empty() {
                    let _ = writeln!(self.out, "          case {i}: {code} break;");
                } else {
                    let ty = self.cty(&self.p.fields[f].ty);
                    let _ = writeln!(self.out, "          case {i}: {code} {{ {ty} old = {place}; {cl} if (memcmp(&old, &{place}, sizeof old)) saturated[{i}] = 1; }} break;");
                }
            }
            // a combination saved by a build of the program without the open table: its components as bits
            let obits = match self.p.open.get(&a).and_then(|fl| fields.iter().position(|x| x == fl).map(|i| (i, *fl))) {
                Some((i, fl)) => format!("        if (!present[{i}]) w->{n}.c{}[row].{} = obits_;\n", self.comp(fl), self.fname(fl)),
                None => String::new(),
            };
            let _ = write!(self.out, "          default: sijo_tok_skip_field(r);\n          }}\n        }}\n{obits}        restored++;\n      }}\n");
            for (i, &f) in fields.iter().enumerate() {
                if self.c.net.clamp.contains_key(&f) {
                    let _ = writeln!(self.out, "      if (saturated[{i}]) sijo_note(report, \" saturated:\", {}, \"\");", c_str(&self.p.field_label(f)));
                }
            }
            self.w("      break; }\n");
        }
        self.w(STATE_LOAD_TAIL);
    }

    fn checksum(&mut self) {
        self.dg_helpers("hash");
        self.w("\n/* deterministic world checksum (field by field, independent of padding) */\nstatic uint64_t sijo_checksum(SijoWorld* w) {\n  uint64_t h = 0xcbf29ce484222325ull;\n");
        for &a in &self.c.ecs.archetypes.clone() {
            let n = self.aname(a);
            let _ = writeln!(self.out, "  for (int32_t r = 0; r < w->{n}.count; r++) {{\n    h = sijo_fnv_u32(h, w->{n}.ent[r].idx); h = sijo_fnv_u32(h, w->{n}.ent[r].gen);");
            // declaration order, so the checksum does not depend on the planned layout
            for f in self.p.all_fields(a) {
                if self.c.ecs.is_dead(f) {
                    continue;
                }
                let k = self.comp(f);
                let code = self.hash_code(&self.p.fields[f].ty, &format!("w->{}.c{}[r].{}", n, k, self.fname(f)), 0);
                let _ = writeln!(self.out, "    {}", code);
            }
            self.w("  }\n");
        }
        self.w("  return h;\n}\n");
    }

    // ------------------------------------------------------------------
    // main
    // ------------------------------------------------------------------

    fn main_fn(&mut self) {
        let start = match self.p.game_start {
            Some(f) => format!("{}(w);", self.func_name(f)),
            None => String::new(),
        };
        let mut counts = String::new();
        for &a in &self.c.ecs.archetypes {
            let n = self.aname(a);
            let label = c_str(&format!(" {}=%d", self.p.classes[a].name));
            if self.p.classes[a].combo_of.is_some() {
                // a class can have many combinations of components: only those in use
                let _ = write!(counts, "if (w->{n}.count) printf({label}, (int)w->{n}.count); ");
            } else {
                let _ = write!(counts, "printf({label}, (int)w->{n}.count); ");
            }
        }
        let nstages = self.c.ecs.stages.len();
        let clist = if self.has_comps() { "free(w->clist); " } else { "" };
        // [Persistent]: the last write of the store, before the tables go
        let pclose = if self.p.persist.is_empty() { "" } else { "sijo_pclose(w);\n  " };
        let mut frees = String::new();
        for &a in &self.c.ecs.archetypes {
            let n = self.aname(a);
            let _ = write!(frees, "sijo_free_table_{n}(&w->{n}); sijo_free_table_{n}(&w->p{n}); for (int t = 0; t < SIJO_MAX_THREADS; t++) sijo_free_table_{n}(&w->pt{n}[t]);\n  ");
        }
        let _ = write!(
            self.out,
            r#"
/* ======================= entry point ======================= */
static void sijo_world_init(SijoWorld* w, int is_client, uint64_t seed, int shards) {{
  memset(w, 0, sizeof *w);
  sijo_slots_init(&w->ents);
  w->is_client = is_client; w->seed = seed; w->dt = 1.0f / 60.0f;
  w->nshards = shards;
  w->nthreads = sijo_nthreads();
}}
/* everything a world owns: tables (and the lists their rows own), strings, buffers, arenas */
static void sijo_world_free(SijoWorld* w) {{
  {pclose}{frees}sijo_slots_free(&w->ents);
  for (int t = 0; t < SIJO_MAX_THREADS; t++) {{ sijo_cmdbuf_free(&w->cmd[t]); sijo_arena_free(&w->frame[t]); }}
  free(w->dlist); {clist}sijo_drawlist_free(&w->draws);
  sijo_sh_free(w);
  sijo_sheap_free(&w->sheap);
  sijo_stab_free(&w->stab);
  for (int g = 0; g < SIJO_NGRID; g++) sijo_grid_free(&w->grid[g]);
  memset(w, 0, sizeof *w);
}}
static void sijo_reset_arenas(SijoWorld* w) {{
  sijo_gc(w); /* strings still referenced by the game state leave the frame arena first */
  for (int t = 0; t < SIJO_MAX_THREADS; t++) sijo_arena_reset(&w->frame[t]);
}}
static void sijo_game_start(SijoWorld* w) {{
  w->tick = 0; w->tls[0].cur = SIJO_NULL; w->tls[0].rng_ent = 0; w->tls[0].rng_ctr = 0;
  {start}
  sijo_apply(w);
}}
static void sijo_print_counts(SijoWorld* w) {{ {counts}(void)w; }}
#define SIJO_NSTAGES {nstages}
"#
        );
        // the shard runtime's world logic (exchange, merge, handoff, rebalance, checkpoints)
        self.w(SHARD_RUNTIME.split(SHARD_SPLIT).nth(1).unwrap_or(""));
        // process modes, transport and protocol (after all code that names user identifiers)
        self.w(CRYPTO_RUNTIME);
        self.w(NET_RUNTIME);
        self.w(SHARD_NET_RUNTIME);
        if self.lib {
            let h = embed_header(self.c, &self.prefix);
            self.w(&h);
            let api = EMBED_RUNTIME.replace("GAMEAPI_", &format!("{}_", self.prefix));
            self.w(&api);
        } else {
            self.w("\nint main(int argc, char** argv) { return sijo_main(argc, argv); }\n");
        }
        self.w("#if defined(__GNUC__)\n#pragma GCC diagnostic pop\n#endif\n");
    }
}

/// the place is (a struct member of) a list element
fn has_list_index(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::ListIndex { .. } => true,
        ExprKind::Field { base, .. } => has_list_index(base),
        _ => false,
    }
}

/// the index expressions of a place have no side effects
fn index_pure(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::ListIndex { list, index, .. } => crate::opt::is_pure_expr(index) && index_pure(list),
        ExprKind::Field { base, .. } => index_pure(base),
        _ => true,
    }
}

/// the local a ranged write targets (callers checked it is one)
fn local_of(e: &Expr) -> LocalId {
    match e.kind {
        ExprKind::Local(l) => l,
        _ => unreachable!("not a local"),
    }
}

fn paren(s: &str) -> String {
    format!("({})", s)
}

const STATE_IO_RUNTIME: &str = r#"
/* ======================= live coding: world state save / restore ======================= */
typedef struct { char* buf; size_t pos, len; int bad; } SijoTok;
static int sijo_is_ws(char c) { return c == ' ' || c == '\n' || c == '\r' || c == '\t'; }
static const char* sijo_tok(SijoTok* r) {
  while (r->pos < r->len && sijo_is_ws(r->buf[r->pos])) r->pos++;
  if (r->pos >= r->len) { r->bad = 1; return ""; }
  char* s = r->buf + r->pos;
  while (r->pos < r->len && !sijo_is_ws(r->buf[r->pos])) r->pos++;
  if (r->pos < r->len) r->buf[r->pos++] = 0;
  return s;
}
/* look at the next token without consuming it */
static int sijo_tok_peek_is(SijoTok* r, const char* w) {
  size_t p = r->pos;
  while (p < r->len && sijo_is_ws(r->buf[p])) p++;
  size_t n = strlen(w);
  return p + n <= r->len && !strncmp(r->buf + p, w, n) && (p + n == r->len || sijo_is_ws(r->buf[p + n]) || r->buf[p + n] == 0);
}
static int64_t sijo_tok_i64(SijoTok* r) { return strtoll(sijo_tok(r), NULL, 10); }
static uint64_t sijo_tok_hex(SijoTok* r) { return strtoull(sijo_tok(r), NULL, 16); }
static SijoH sijo_tok_handle(SijoTok* r) { SijoH h = {0, 0}; const char* t = sijo_tok(r); sscanf(t, "%u:%u", &h.idx, &h.gen); return h; }
static int32_t sijo_tok_list(SijoTok* r) {
  const char* t = sijo_tok(r);
  if (t[0] != '[') { r->bad = 1; return 0; }
  long n = strtol(t + 1, NULL, 10);
  if (n < 0 || (size_t)n > r->len - r->pos) { r->bad = 1; return 0; } /* every element takes at least one byte */
  return (int32_t)n;
}
static void sijo_tok_expect(SijoTok* r, const char* w) { if (strcmp(sijo_tok(r), w)) r->bad = 1; }
/* skip to the `}` that closes a group whose `{` was read (a delegate value this build cannot name) */
static void sijo_tok_skip_group(SijoTok* r) {
  for (int depth = 1; depth > 0 && !r->bad && r->pos < r->len; ) {
    const char* t = sijo_tok(r);
    if (!strcmp(t, "{")) depth++;
    else if (!strcmp(t, "}")) depth--;
  }
}
/* skip the tokens of a field that no longer exists */
static void sijo_tok_skip_field(SijoTok* r) {
  while (!r->bad && r->pos < r->len && !sijo_tok_peek_is(r, "|") && !sijo_tok_peek_is(r, "row") && !sijo_tok_peek_is(r, "arch") && !sijo_tok_peek_is(r, "end")) sijo_tok(r);
}
static void sijo_note(char* report, const char* what, const char* a, const char* b) {
  if (strlen(report) + strlen(what) + strlen(a) + strlen(b) + 2 >= 4000) return;
  strcat(report, what); strcat(report, a); if (b[0]) { strcat(report, "."); strcat(report, b); }
}
"#;

const STATE_LOAD_HEAD: &str = r#"static void sijo_world_init(SijoWorld* w, int is_client, uint64_t seed, int shards);
static void sijo_world_free(SijoWorld* w);
/* parse a state file into a fresh world (anything malformed or out of range rejects the file) */
static int sijo_load_state_into(SijoWorld* w, const char* path, int verbose) {
  FILE* f = fopen(path, "rb");
  if (!f) return 0;
  fseek(f, 0, SEEK_END); long len = ftell(f); fseek(f, 0, SEEK_SET);
  if (len <= 0 || len > (1l << 30)) { fclose(f); return 0; }
  SijoTok rr = { (char*)malloc((size_t)len + 1), 0, (size_t)len, 0 }; SijoTok* r = &rr;
  if (!rr.buf) { fclose(f); return 0; }
  if (fread(rr.buf, 1, (size_t)len, f) != (size_t)len) { fclose(f); free(rr.buf); return 0; }
  rr.buf[len] = 0; fclose(f);
  sijo_tok_expect(r, "SIJO-STATE"); sijo_tok_expect(r, "1");
  sijo_tok_expect(r, "tick"); w->tick = (int32_t)sijo_tok_i64(r);
  sijo_tok_expect(r, "seed"); w->seed = (uint64_t)strtoull(sijo_tok(r), NULL, 10);
  sijo_tok_expect(r, "slots"); int64_t ns64 = sijo_tok_i64(r);
  if (r->bad || w->tick < 0 || ns64 < 1 || ns64 > (1 << 24) || ns64 > (int64_t)len) { free(rr.buf); return 0; }
  int32_t ns = (int32_t)ns64;
  while (w->ents.cap < ns) { w->ents.cap *= 2; w->ents.s = (SijoSlot*)sijo_xrealloc(w->ents.s, (size_t)w->ents.cap * sizeof(SijoSlot)); }
  memset(w->ents.s, 0, (size_t)w->ents.cap * sizeof(SijoSlot));
  for (int32_t i = 1; i < ns; i++) { w->ents.s[i].gen = (uint32_t)sijo_tok_i64(r); w->ents.s[i].state = SIJO_FREE; sijo_shard_reset(&w->ents, (uint32_t)i); }
  w->ents.n = ns;
  sijo_tok_expect(r, "free"); int64_t nf = sijo_tok_i64(r);
  if (nf < 0 || nf >= ns) r->bad = 1;
  w->ents.nfree = 0;
  for (int32_t i = 0; i < nf && !r->bad; i++) {
    if (w->ents.nfree == w->ents.capfree) { w->ents.capfree = w->ents.capfree ? w->ents.capfree * 2 : 64; w->ents.freel = (int32_t*)sijo_xrealloc(w->ents.freel, (size_t)w->ents.capfree * sizeof(int32_t)); }
    int64_t fi = sijo_tok_i64(r);
    if (fi < 1 || fi >= ns) { r->bad = 1; break; }
    w->ents.freel[w->ents.nfree++] = (int32_t)fi;
  }
  int64_t restored = 0, dropped_rows = 0; char report[4096]; report[0] = 0;
  while (!r->bad) {
    const char* t = sijo_tok(r);
    if (r->bad || !strcmp(t, "end")) break;
    if (strcmp(t, "arch")) { r->bad = 1; break; }
    char aname[256]; snprintf(aname, sizeof aname, "%s", sijo_tok(r));
    int32_t count = (int32_t)sijo_tok_i64(r), nfields = (int32_t)sijo_tok_i64(r);
    int map[256]; int arch = -1;
    if (nfields < 0 || nfields > 256 || count < 0 || count >= ns) { r->bad = 1; break; }
"#;

const STATE_LOAD_TAIL: &str = r#"    default:
      for (int32_t k = 0; k < count && !r->bad; k++) {
        sijo_tok_expect(r, "row"); uint32_t idx = (uint32_t)sijo_tok_i64(r); sijo_tok_i64(r);
        for (int32_t j = 0; j < nfields; j++) { sijo_tok_expect(r, "|"); sijo_tok_skip_field(r); }
        /* the entity's class no longer exists: its slot becomes free (old handles go stale) */
        if (idx < (uint32_t)w->ents.n) {
          w->ents.s[idx].gen++;
          if (w->ents.nfree == w->ents.capfree) { w->ents.capfree = w->ents.capfree ? w->ents.capfree * 2 : 64; w->ents.freel = (int32_t*)sijo_xrealloc(w->ents.freel, (size_t)w->ents.capfree * sizeof(int32_t)); }
          w->ents.freel[w->ents.nfree++] = (int32_t)idx;
        }
        dropped_rows++;
      }
    }
  }
  free(rr.buf);
  if (r->bad) { if (verbose) fprintf(stderr, "[live] state file is malformed or does not fit; nothing was loaded\n"); return 0; }
  if (verbose) fprintf(stderr, "[live] state restored at tick %d: %lld entities%s%s\n", w->tick, (long long)restored, dropped_rows ? " (entities of removed classes dropped)" : "", report[0] ? report : " (same schema)");
  return 1;
}
/* load a state file: on success the world is replaced, on failure it is left as it was */
static int sijo_load_state_v(SijoWorld* w, const char* path, int verbose) {
  SijoWorld* t = (SijoWorld*)malloc(sizeof *t);
  if (!t) return 0;
  sijo_world_init(t, w->is_client, w->seed, w->nshards);
  t->quiet = w->quiet; t->nthreads = w->nthreads; t->dt = w->dt;
  int ok = sijo_load_state_into(t, path, verbose);
  if (ok) { sijo_world_free(w); *w = *t; } else sijo_world_free(t);
  free(t);
  return ok;
}
static int sijo_load_state(SijoWorld* w, const char* path) { return sijo_load_state_v(w, path, 1); }
"#;

fn block_has_await(b: &Block) -> bool {
    b.stmts.iter().any(|s| matches!(s, Stmt::Await { .. }) || s.sub_blocks().into_iter().any(block_has_await))
}

fn block_has_atomic(b: &Block) -> bool {
    b.stmts.iter().any(|s| matches!(s, Stmt::Atomic { .. }) || s.sub_blocks().into_iter().any(block_has_atomic))
}

/// locals (re)bound by statements of a block: `var x = ...`, foreach variables, switch temporaries
fn stmt_bound_locals(b: &Block, out: &mut BTreeSet<LocalId>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { local, .. } => {
                out.insert(*local);
            }
            Stmt::ForList { var, idx, .. } => {
                out.insert(*var);
                out.insert(*idx);
            }
            Stmt::Switch { tmp: Some(t), .. } => {
                out.insert(*t);
            }
            Stmt::ForQuery { var, .. } => {
                out.insert(*var);
            }
            _ => {}
        }
        for sb in s.sub_blocks() {
            stmt_bound_locals(sb, out);
        }
    }
}

/// the root of a place expression (a local, a field of an entity...)
fn effects_root(e: &Expr) -> Option<&ExprKind> {
    match &e.kind {
        ExprKind::Field { base, .. } => effects_root(base),
        ExprKind::ListIndex { list, .. } => effects_root(list),
        k => Some(k),
    }
}

/// does the block return a value that may point into the frame arena?
fn block_returns_frame(b: &Block, holds: &dyn Fn(&Ty) -> bool) -> bool {
    b.stmts.iter().any(|s| match s {
        Stmt::Return(Some(e), _) => holds(&e.ty),
        _ => s.sub_blocks().into_iter().any(|x| block_returns_frame(x, holds)),
    })
}

/// Functions that may allocate from the frame arena, directly or through the functions they call
/// (entity methods: any method of that name, since calls on `this` and others dispatch by archetype).
fn alloc_funcs(p: &Program) -> BTreeSet<FuncId> {
    let direct = |e: &Expr| -> bool {
        match &e.kind {
            ExprKind::Interp(_) | ExprKind::ListNew { .. } | ExprKind::ArrayNew { .. } | ExprKind::ListClone { .. } | ExprKind::DictSet { .. } => true,
            ExprKind::ListLambda { op, .. } => op.makes_list() || *op == LamOp::Sort,
            ExprKind::Binary(BinOp::Add, a, _) => a.ty == Ty::Str,
            ExprKind::Assign { op: Some(BinOp::Add), target, .. } => target.ty == Ty::Str,
            ExprKind::ListCall { op, .. } => matches!(op, ListOp::Add | ListOp::Insert),
            ExprKind::DictCall { op, .. } => *op == DictOp::Add,
            ExprKind::Builtin { f, .. } => matches!(
                f,
                Builtin::StrSub
                    | Builtin::StrSubFrom
                    | Builtin::StrUpper
                    | Builtin::StrLower
                    | Builtin::StrTrim
                    | Builtin::StrReplace
                    | Builtin::StrJoin
                    | Builtin::ToStr
                    | Builtin::StrRepeat
                    | Builtin::StrSplit
                    | Builtin::StrPadLeft
                    | Builtin::StrPadRight
            ),
            _ => false,
        }
    };
    let mut out: BTreeSet<FuncId> = BTreeSet::new();
    let mut calls: Vec<Vec<FuncId>> = vec![];
    for (fid, f) in p.funcs.iter().enumerate() {
        let mut own = false;
        let mut cs = vec![];
        crate::sema::visit_block(&f.body, &mut |e| {
            own |= direct(e);
            match &e.kind {
                ExprKind::Call { func, dispatch, .. } => {
                    if matches!(dispatch, Dispatch::Static) {
                        cs.push(*func);
                    } else if !matches!(dispatch, Dispatch::RemoteDeferred | Dispatch::Rpc) {
                        let name = &p.funcs[*func].name;
                        cs.extend((0..p.funcs.len()).filter(|&g| &p.funcs[g].name == name && matches!(p.funcs[g].kind, FuncKind::EntityMethod(_))));
                    }
                }
                ExprKind::OpCall { func, .. } => cs.push(*func),
                ExprKind::StructLit { ctor: Some(c), .. } => cs.push(*c),
                _ => {}
            }
        });
        if own {
            out.insert(fid);
        }
        calls.push(cs);
    }
    loop {
        let mut changed = false;
        for (fid, cs) in calls.iter().enumerate() {
            if !out.contains(&fid) && cs.iter().any(|g| out.contains(g)) {
                out.insert(fid);
                changed = true;
            }
        }
        if !changed {
            return out;
        }
    }
}
