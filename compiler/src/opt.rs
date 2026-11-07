//! HIR-level optimizations: constant folding & propagation, CTFE of pure calls,
//! algebraic simplification, branch elimination, bounds-check elimination.

use crate::ast::{BinOp, ParamMode, UnOp};
use crate::diag::Span;
use crate::consteval::{self, binop};
use crate::hir::*;
use std::collections::{HashMap, HashSet};

#[derive(Default, Debug, Clone)]
pub struct OptStats {
    pub folded: usize,
    pub propagated: usize,
    pub ctfe_calls: usize,
    pub algebraic: usize,
    pub branches: usize,
    pub bounds_checks_removed: usize,
    pub ctfe_examples: Vec<String>,
    /// conditions that folded to a constant: (span, value) — reported by the linter
    pub const_conditions: Vec<(crate::diag::Span, bool)>,
    /// e-graph: expressions replaced by a cheaper equal form, e-nodes built, rules that took part
    pub egraph_exprs: usize,
    pub egraph_nodes: usize,
    pub egraph_rules: Vec<(&'static str, usize)>,
    /// e-graph: `if` / loop conditions it decided (value ranges, algebra); not lint findings
    pub egraph_branches: usize,
    /// bounds checks removed because the index range lies inside an array of known length
    pub range_bce: usize,
    /// examples for `sijoc explain`: (span of the expression, its new form)
    pub egraph_examples: Vec<(crate::diag::Span, String)>,
    /// writes to ranged ints (a local or a field of `this`) whose value provably stays inside one
    /// or both bounds: (function, target span) -> (clamp at the lower bound, clamp at the upper)
    pub clamp_need: HashMap<(FuncId, u32, u32), (bool, bool)>,
    /// entity fields the program reads as written: they keep their storage (see optimize_with)
    pub observed_fields: std::collections::BTreeSet<FieldId>,
}

impl OptStats {
    /// which sides of a ranged write's saturation are still needed (both when nothing is known)
    pub fn clamp_sides(&self, f: FuncId, target: crate::diag::Span) -> (bool, bool) {
        self.clamp_need.get(&(f, target.lo, target.hi)).copied().unwrap_or((true, true))
    }
}

pub fn optimize(p: &mut Program) -> OptStats {
    optimize_with(p, true)
}

/// `egraph`: run equality saturation (the int/bool algebra and value-range facts)
pub fn optimize_with(p: &mut Program, egraph: bool) -> OptStats {
    let mut st = OptStats::default();
    // the fields the program reads as written keep their storage, whatever the optimizer proves
    // about those reads: the stored world (checksum, saved states) does not depend on how much the
    // optimizer removes, with or without the e-graph
    st.observed_fields = crate::ecs::observed_reads(p);
    let rules = crate::egraph::rules();
    let checked = consteval::CHECKED.load(std::sync::atomic::Ordering::Relaxed);
    for fid in 0..p.funcs.len() {
        let mut body = std::mem::take(&mut p.funcs[fid].body);
        // folding, propagation and CTFE, interleaved with equality saturation (each enables the other)
        for _ in 0..4 {
            let before = (st.folded, st.propagated, st.ctfe_calls, st.algebraic, st.branches, st.egraph_exprs, st.egraph_branches);
            let consts = const_locals(&body, &p.funcs[fid]);
            fold_block(p, &mut body, &consts, &mut st);
            if egraph {
                EgPass::new(p, fid, &body, &rules, checked, &mut st).block(&mut body);
            }
            if before == (st.folded, st.propagated, st.ctfe_calls, st.algebraic, st.branches, st.egraph_exprs, st.egraph_branches) {
                break;
            }
        }
        bce_block(p, &mut body, &mut st);
        p.funcs[fid].body = body;
    }
    st.algebraic += st.egraph_exprs;
    st
}

// ---------------------------------------------------------------------------
// literals
// ---------------------------------------------------------------------------

pub fn is_lit(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Null | ExprKind::EnumVal(..) => true,
        ExprKind::StructLit { ctor: None, args, inits, .. } => args.is_empty() && inits.iter().all(|(_, x)| is_lit(x)),
        _ => false,
    }
}

pub fn to_expr(p: &Program, v: &ConstVal, ty: &Ty, sp: crate::diag::Span) -> Expr {
    let kind = match v {
        ConstVal::Int(n) => {
            if *ty == Ty::Float {
                return Expr::new(ExprKind::Float(*n as f32 as f64), Ty::Float, sp);
            }
            ExprKind::Int(*n as i64)
        }
        ConstVal::Float(f) => ExprKind::Float(*f as f64),
        ConstVal::Bool(b) => ExprKind::Bool(*b),
        ConstVal::Null => ExprKind::Null,
        ConstVal::Str(s) => return Expr::new(ExprKind::Str(s.clone()), Ty::Str, sp),
        ConstVal::Fn(d, n) => return Expr::new(ExprKind::Closure { dg: *d, num: *n, caps: vec![] }, Ty::Delegate(*d), sp),
        ConstVal::Enum(e, x) => ExprKind::EnumVal(*e, *x),
        ConstVal::Struct(s, vals) => {
            let inits = vals
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let fty = p.structs[*s].fields[i].ty.clone();
                    (i, to_expr(p, v, &fty, sp))
                })
                .collect();
            ExprKind::StructLit { sid: *s, ctor: None, args: vec![], inits }
        }
    };
    let t = match v {
        ConstVal::Struct(s, _) => Ty::Struct(*s),
        _ if *ty == Ty::Error => match v {
            ConstVal::Int(_) => Ty::Int,
            ConstVal::Float(_) => Ty::Float,
            ConstVal::Bool(_) => Ty::Bool,
            _ => ty.clone(),
        },
        _ => ty.clone(),
    };
    Expr::new(kind, t, sp)
}

fn lit_val(p: &Program, e: &Expr) -> Option<ConstVal> {
    if !is_lit(e) {
        return None;
    }
    consteval::eval_expr(p, e).ok()
}

/// No side effects and no dependence on world state that could change between evaluations.
pub fn is_pure_expr(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Assign { .. }
        | ExprKind::IncDec { .. }
        | ExprKind::Call { .. }
        | ExprKind::Spawn { .. }
        | ExprKind::Destroy(_)
        | ExprKind::CompAdd { .. }
        | ExprKind::CompRemove { .. }
        | ExprKind::ListCall { .. }
        | ExprKind::ListNew { .. }
        | ExprKind::ArrayNew { .. }
        | ExprKind::ListLambda { .. }
        | ExprKind::ListClone { .. }
        | ExprKind::OpCall { .. }
        | ExprKind::DictGet { .. }
        | ExprKind::DictSet { .. }
        | ExprKind::DictCall { .. }
        | ExprKind::Is { .. } => false,
        ExprKind::Builtin { f, .. } if matches!(f, Builtin::Print | Builtin::RandomFloat | Builtin::RandomInt | Builtin::DrawSprite | Builtin::DrawText | Builtin::NetView | Builtin::NetClearView) => false,
        // may panic on bad input: keep them where they are
        ExprKind::Builtin { f, .. }
            if matches!(
                f,
                Builtin::StrSub
                    | Builtin::StrSubFrom
                    | Builtin::StrReplace
                    | Builtin::ParseInt
                    | Builtin::ParseFloat
                    | Builtin::TryParseInt
                    | Builtin::TryParseFloat
                    | Builtin::StrCharAt
                    | Builtin::StrRepeat
                    | Builtin::StrPadLeft
                    | Builtin::StrPadRight
                    | Builtin::StrSplit
            ) =>
        {
            false
        }
        ExprKind::Binary(BinOp::Div | BinOp::Rem, _, b) if b.ty == Ty::Int && !matches!(b.kind, ExprKind::Int(n) if n != 0) => false,
        _ => crate::sema::children(e).into_iter().all(is_pure_expr),
    }
}

// ---------------------------------------------------------------------------
// constant locals
// ---------------------------------------------------------------------------

fn const_locals(b: &Block, f: &Func) -> HashMap<LocalId, Expr> {
    let written = written_locals(b);
    let mut inits: HashMap<LocalId, Expr> = HashMap::new();
    let mut lets: HashMap<LocalId, usize> = HashMap::new();
    collect_lets(b, &mut inits, &mut lets);
    let mut out = HashMap::new();
    for (l, e) in inits {
        if written.contains(&l) || lets.get(&l).copied().unwrap_or(0) != 1 {
            continue;
        }
        if f.locals[l].kind != LocalKind::Normal {
            continue;
        }
        if is_lit(&e) && !matches!(e.kind, ExprKind::StructLit { .. }) {
            out.insert(l, e);
        }
    }
    out
}

/// Locals whose value changes after their declaration: assigned (as a whole or a part), ++/--,
/// passed ref / out / own, the receiver of a (possibly mutating) method, changed by a list or
/// dictionary operation, or the out variable of TryParse / TryGetValue.
fn written_locals(b: &Block) -> HashSet<LocalId> {
    let mut written: HashSet<LocalId> = HashSet::new();
    crate::sema::visit_block(b, &mut |e| match &e.kind {
        ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => {
            if let Some(l) = root_local(target) {
                written.insert(l);
            }
        }
        ExprKind::Call { args, recv, func, .. } => {
            for a in args {
                if matches!(a.mode, ParamMode::Ref | ParamMode::Out | ParamMode::Own) {
                    if let Some(l) = root_local(&a.expr) {
                        written.insert(l);
                    }
                }
            }
            let _ = func;
            if let Some(r) = recv {
                // a mutating struct method could change the receiver
                if let Some(l) = root_local(r) {
                    written.insert(l);
                }
            }
        }
        ExprKind::ListCall { list, .. } => {
            if let Some(l) = root_local(list) {
                written.insert(l);
            }
        }
        ExprKind::DictSet { dict, .. } => {
            if let Some(l) = root_local(dict) {
                written.insert(l);
            }
        }
        ExprKind::Builtin { f: Builtin::TryParseInt | Builtin::TryParseFloat, args, .. } => {
            if let Some(l) = args.get(1).and_then(root_local) {
                written.insert(l);
            }
        }
        ExprKind::DictCall { dict, args, op } => {
            if let Some(l) = root_local(dict) {
                written.insert(l);
            }
            if *op == DictOp::TryGet {
                if let Some(l) = args.get(1).and_then(root_local) {
                    written.insert(l);
                }
            }
        }
        ExprKind::ListLambda { list, op, .. } if op.mutates() => {
            if let Some(l) = root_local(list) {
                written.insert(l);
            }
        }
        _ => {}
    });
    written
}

/// Locals given a new value as a whole (assigned, or passed `ref`/`out`/`own`). Writing an element
/// of an array does not count: it keeps the array's lengths.
fn replaced_locals(b: &Block) -> HashSet<LocalId> {
    let mut out: HashSet<LocalId> = HashSet::new();
    crate::sema::visit_block(b, &mut |e| match &e.kind {
        ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => {
            if let ExprKind::Local(l) = target.kind {
                out.insert(l);
            }
        }
        ExprKind::Call { args, .. } => {
            for a in args {
                if let (ParamMode::Ref | ParamMode::Out | ParamMode::Own, ExprKind::Local(l)) = (a.mode, &a.expr.kind) {
                    out.insert(*l);
                }
            }
        }
        _ => {}
    });
    out
}

fn collect_lets(b: &Block, inits: &mut HashMap<LocalId, Expr>, lets: &mut HashMap<LocalId, usize>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { local, init } => {
                *lets.entry(*local).or_insert(0) += 1;
                if let Some(e) = init {
                    inits.insert(*local, e.clone());
                }
            }
            Stmt::If { then, els, .. } => {
                collect_lets(then, inits, lets);
                if let Some(e) = els {
                    collect_lets(e, inits, lets);
                }
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } => {
                // locals declared inside loops are re-initialized each iteration; still single Let
                collect_lets(body, inits, lets)
            }
            Stmt::Switch { sections, default, .. } => {
                for (_, x) in sections {
                    collect_lets(x, inits, lets);
                }
                if let Some(x) = default {
                    collect_lets(x, inits, lets);
                }
            }
            _ => {}
        }
    }
}

pub fn root_local(e: &Expr) -> Option<LocalId> {
    match &e.kind {
        ExprKind::Local(l) => Some(*l),
        ExprKind::Field { base, .. } => root_local(base),
        ExprKind::ListIndex { list, .. } => root_local(list),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// folding
// ---------------------------------------------------------------------------

fn fold_block(p: &Program, b: &mut Block, consts: &HashMap<LocalId, Expr>, st: &mut OptStats) {
    let stmts = std::mem::take(&mut b.stmts);
    for mut s in stmts {
        match &mut s {
            Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => fold_expr(p, e, consts, st),
            Stmt::If { cond, then, els } => {
                fold_expr(p, cond, consts, st);
                fold_block(p, then, consts, st);
                if let Some(e) = els {
                    fold_block(p, e, consts, st);
                }
                if let ExprKind::Bool(c) = cond.kind {
                    st.branches += 1;
                    st.const_conditions.push((cond.span, c));
                    if c {
                        b.stmts.push(Stmt::Block(std::mem::take(then)));
                    } else if let Some(e) = els {
                        b.stmts.push(Stmt::Block(std::mem::take(e)));
                    }
                    continue;
                }
            }
            Stmt::Loop { cond, step, body, do_while } => {
                if let Some(c) = cond {
                    fold_expr(p, c, consts, st);
                }
                for e in step.iter_mut() {
                    fold_expr(p, e, consts, st);
                }
                fold_block(p, body, consts, st);
                if let Some(Expr { kind: ExprKind::Bool(false), span, .. }) = cond {
                    if !*do_while {
                        st.branches += 1;
                        st.const_conditions.push((*span, false));
                        continue;
                    }
                }
            }
            Stmt::ForQuery { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => fold_block(p, body, consts, st),
            Stmt::ForList { list, body, .. } => {
                fold_expr(p, list, consts, st);
                fold_block(p, body, consts, st);
            }
            Stmt::Await { kind: AwaitKind::Wait(e) | AwaitKind::Until(e), .. } => fold_expr(p, e, consts, st),
            Stmt::Await { kind: AwaitKind::Group { arms, .. }, .. } => {
                for a in arms.iter_mut() {
                    if let crate::hir::AwaitArm::Wait { secs: e, .. } | crate::hir::AwaitArm::Until(e) = a {
                        fold_expr(p, e, consts, st);
                    }
                }
            }
            Stmt::Switch { scrut, sections, default, .. } => {
                fold_expr(p, scrut, consts, st);
                for (c, x) in sections.iter_mut() {
                    fold_expr(p, c, consts, st);
                    fold_block(p, x, consts, st);
                }
                if let Some(x) = default {
                    fold_block(p, x, consts, st);
                }
                // sections whose condition folded to false can never run
                let before = sections.len();
                sections.retain(|(c, _)| !matches!(c.kind, ExprKind::Bool(false)));
                st.branches += before - sections.len();
            }
            _ => {}
        }
        b.stmts.push(s);
    }
}

fn fold_expr(p: &Program, e: &mut Expr, consts: &HashMap<LocalId, Expr>, st: &mut OptStats) {
    // children first (but never replace assignment targets / ref args with literals)
    match &mut e.kind {
        ExprKind::Assign { target, value, .. } => {
            fold_place(p, target, consts, st);
            fold_expr(p, value, consts, st);
            return;
        }
        ExprKind::IncDec { target, .. } => {
            fold_place(p, target, consts, st);
            return;
        }
        ExprKind::Call { recv, args, .. } => {
            if let Some(r) = recv {
                fold_place(p, r, consts, st);
            }
            for a in args.iter_mut() {
                if matches!(a.mode, ParamMode::Ref | ParamMode::Out) {
                    fold_place(p, &mut a.expr, consts, st);
                } else {
                    fold_expr(p, &mut a.expr, consts, st);
                }
            }
        }
        ExprKind::ListCall { list, args, .. } => {
            fold_place(p, list, consts, st);
            for a in args.iter_mut() {
                fold_expr(p, a, consts, st);
            }
            return;
        }
        ExprKind::Is { expr, .. } => {
            fold_expr(p, expr, consts, st);
            return;
        }
        _ => {
            for c in children_mut(e) {
                fold_expr(p, c, consts, st);
            }
        }
    }
    let sp = e.span;
    let replacement: Option<Expr> = match &e.kind {
        ExprKind::Local(l) => consts.get(l).map(|x| {
            st.propagated += 1;
            let mut x = x.clone();
            x.span = sp;
            x
        }),
        ExprKind::Const(c) => match &p.consts[*c].value {
            Some(v) if !matches!(v, ConstVal::Struct(..)) => {
                st.folded += 1;
                Some(to_expr(p, v, &e.ty, sp))
            }
            _ => None,
        },
        ExprKind::Unary(_, x) => match &x.kind {
            _ => lit_val(p, x).and_then(|_| {
                consteval::eval_expr(p, e).ok().map(|v| {
                    st.folded += 1;
                    to_expr(p, &v, &e.ty, sp)
                })
            }),
        },
        ExprKind::Binary(op, a, b) => {
            let (la, lb) = (lit_val(p, a), lit_val(p, b));
            match (la, lb) {
                (Some(x), Some(y)) => {
                    let (x, y) = promote_pair(x, y);
                    match binop(*op, x, y) {
                        Ok(v) => {
                            st.folded += 1;
                            Some(to_expr(p, &v, &e.ty, sp))
                        }
                        Err(_) => None,
                    }
                }
                (la, lb) => float_identity(*op, a, b, la.as_ref(), lb.as_ref(), st),
            }
        }
        ExprKind::Cond(c, a, b) => match c.kind {
            ExprKind::Bool(true) => {
                st.branches += 1;
                Some((**a).clone())
            }
            ExprKind::Bool(false) => {
                st.branches += 1;
                Some((**b).clone())
            }
            _ => None,
        },
        ExprKind::Cast(x) => {
            if is_lit(x) {
                consteval::eval_expr(p, e).ok().map(|v| {
                    st.folded += 1;
                    to_expr(p, &v, &e.ty, sp)
                })
            } else {
                None
            }
        }
        ExprKind::Builtin { f, args, .. }
            if matches!(
                f,
                Builtin::MathSqrt
                    | Builtin::Saturate
                    | Builtin::MathAbs
                    | Builtin::MathMin
                    | Builtin::MathMax
                    | Builtin::MathClamp
                    | Builtin::MathFloor
                    | Builtin::MathSin
                    | Builtin::MathCos
                    | Builtin::MathTan
                    | Builtin::MathAtan
                    | Builtin::MathAtan2
                    | Builtin::MathAsin
                    | Builtin::MathAcos
                    | Builtin::MathExp
                    | Builtin::MathLog
                    | Builtin::MathLog10
                    | Builtin::MathLog2
                    | Builtin::MathPow
                    | Builtin::MathRound
                    | Builtin::MathCeiling
                    | Builtin::MathTruncate
                    | Builtin::MathSign
            ) =>
        {
            if args.iter().all(is_lit) {
                consteval::eval_expr(p, e).ok().map(|v| {
                    st.folded += 1;
                    to_expr(p, &v, &e.ty, sp)
                })
            } else {
                None
            }
        }
        ExprKind::StructLit { ctor, args, inits, .. } => {
            let all = args.iter().all(|a| is_lit(&a.expr)) && inits.iter().all(|(_, x)| is_lit(x));
            if all && (ctor.is_some() || !is_lit(e)) {
                match consteval::eval_expr(p, e) {
                    Ok(v) => {
                        st.ctfe_calls += 1;
                        Some(to_expr(p, &v, &e.ty, sp))
                    }
                    Err(_) => None,
                }
            } else {
                None
            }
        }
        ExprKind::OpCall { func, args } => {
            if args.iter().all(is_lit) {
                let av: Option<Vec<ConstVal>> = args.iter().map(|a| lit_val(p, a)).collect();
                av.and_then(|av| consteval::eval_call(p, *func, None, av)).map(|v| {
                    st.ctfe_calls += 1;
                    to_expr(p, &v, &e.ty, sp)
                })
            } else {
                None
            }
        }
        ExprKind::Call { func, recv, args, dispatch: Dispatch::Static } => {
            let callee = &p.funcs[*func];
            let ok_kind = callee.extern_name.is_none() && match callee.kind {
                FuncKind::Static(_) | FuncKind::StructStatic(_) => recv.is_none(),
                FuncKind::StructMethod(_) => !callee.writes_self && recv.as_ref().map(|r| is_lit(r)).unwrap_or(false),
                _ => false,
            };
            if ok_kind && callee.ret != Ty::Void && args.iter().all(|a| a.mode == ParamMode::Value && is_lit(&a.expr)) {
                let av: Option<Vec<ConstVal>> = args.iter().map(|a| lit_val(p, &a.expr)).collect();
                let rv = recv.as_ref().and_then(|r| lit_val(p, r));
                av.and_then(|av| consteval::eval_call(p, *func, rv, av)).map(|v| {
                    st.ctfe_calls += 1;
                    if st.ctfe_examples.len() < 8 {
                        st.ctfe_examples.push(format!("{}(...) = {}", p.func_label(*func), show_val(p, &v)));
                    }
                    to_expr(p, &v, &e.ty, sp)
                })
            } else {
                None
            }
        }
        _ => None,
    };
    if let Some(r) = replacement {
        *e = r;
    }
}

pub fn show_val(p: &Program, v: &ConstVal) -> String {
    match v {
        ConstVal::Int(n) => n.to_string(),
        ConstVal::Float(f) => format!("{}", f),
        ConstVal::Bool(b) => b.to_string(),
        ConstVal::Null => "null".into(),
        ConstVal::Str(s) => format!("{:?}", s),
        ConstVal::Fn(d, 0) => format!("default({})", p.delegate_name(*d)),
        ConstVal::Fn(d, n) => format!("{} {}", p.delegate_name(*d), p.func_label(p.delegates[*d].funcs[*n - 1])),
        ConstVal::Enum(e, x) => p.enums[*e].variants.iter().find(|(_, v)| v == x).map(|(n, _)| format!("{}.{}", p.enums[*e].name, n)).unwrap_or(x.to_string()),
        ConstVal::Struct(s, vals) => format!("{}({})", p.structs[*s].name, vals.iter().map(|v| show_val(p, v)).collect::<Vec<_>>().join(", ")),
    }
}

fn promote_pair(a: ConstVal, b: ConstVal) -> (ConstVal, ConstVal) {
    match (&a, &b) {
        (ConstVal::Int(x), ConstVal::Float(_)) => (ConstVal::Float(*x as f32), b),
        (ConstVal::Float(_), ConstVal::Int(y)) => (a, ConstVal::Float(*y as f32)),
        _ => (a, b),
    }
}

fn fold_place(p: &Program, e: &mut Expr, consts: &HashMap<LocalId, Expr>, st: &mut OptStats) {
    match &mut e.kind {
        ExprKind::Field { base, .. } => fold_place(p, base, consts, st),
        ExprKind::EField { base, .. } => fold_expr(p, base, consts, st),
        ExprKind::ListIndex { list, index, .. } => {
            fold_place(p, list, consts, st);
            fold_expr(p, index, consts, st);
        }
        _ => {}
    }
}

/// The exact IEEE identities `x * 1`, `1 * x`, `x / 1`, `x - 0` (int and bool algebra is the
/// e-graph's: see EgPass). `x + 0` is not one of them: -0 + 0 is +0.
fn float_identity(op: BinOp, a: &Expr, b: &Expr, la: Option<&ConstVal>, lb: Option<&ConstVal>, st: &mut OptStats) -> Option<Expr> {
    use ConstVal as V;
    if a.ty != Ty::Float && b.ty != Ty::Float {
        return None;
    }
    let r = match (op, la, lb) {
        (BinOp::Mul, _, Some(V::Float(f))) | (BinOp::Div, _, Some(V::Float(f))) if *f == 1.0 && a.ty == Ty::Float => Some(a.clone()),
        (BinOp::Mul, Some(V::Float(f)), _) if *f == 1.0 && b.ty == Ty::Float => Some(b.clone()),
        (BinOp::Sub, _, Some(V::Float(f))) if *f == 0.0 && f.is_sign_positive() && a.ty == Ty::Float => Some(a.clone()),
        _ => None,
    };
    if r.is_some() {
        st.algebraic += 1;
    }
    r
}

// ---------------------------------------------------------------------------
// bounds-check elimination
// ---------------------------------------------------------------------------

#[derive(PartialEq, Clone, Debug)]
enum ListRef {
    Local(LocalId),
    SelfField(FieldId),
    /// `h.F` through a handle or query cursor held in a local: another entity's list field. A
    /// system reads other entities as of its start, lists owned by another entity cannot be changed
    /// in place (E0405) and writes to them are deferred, so it stays put unless `h` changes.
    Other(LocalId, FieldId),
}

fn list_ref(e: &Expr) -> Option<ListRef> {
    match &e.kind {
        ExprKind::Local(l) => Some(ListRef::Local(*l)),
        ExprKind::EField { base, field } if matches!(base.kind, ExprKind::SelfRef) => Some(ListRef::SelfField(*field)),
        ExprKind::EField { base, field } => match base.kind {
            ExprKind::Local(h) => Some(ListRef::Other(h, *field)),
            _ => None,
        },
        _ => None,
    }
}

/// Does writing or mutating `target` change the list `lr`? Another entity's list field counts as
/// changed by any write to that field (through `this` or any handle: the handle may name this entity).
fn touches(target: &Expr, lr: &ListRef) -> bool {
    match lr {
        ListRef::Other(h, f) => {
            matches!(&target.kind, ExprKind::EField { field, .. } if field == f) || matches!(target.kind, ExprKind::Local(x) if x == *h)
        }
        _ => list_ref(target).as_ref() == Some(lr),
    }
}

/// statements that suspend a coroutine: the loop resumes on a later tick, when lists may have changed
fn has_await(b: &Block) -> bool {
    b.stmts.iter().any(|s| matches!(s, Stmt::Await { .. }) || s.sub_blocks().into_iter().any(has_await))
}

/// The list a counting loop `for (i = <n >= 0>; i < L.Count; i++)` (or `i < L.GetLength(d)`) runs
/// over, when the body changes neither `i` nor `L`: `L[i]` needs no bounds check, and `L` itself can
/// be read once before the loop (the condition reads it before the first iteration anyway).
/// `init` is the statement before the loop. Returns (index local, the list, dimension).
pub fn counting_loop<'a>(init: &Stmt, cond: &'a Option<Expr>, step: &[Expr], do_while: bool, body: &Block) -> Option<(LocalId, &'a Expr, Option<usize>)> {
    let (Stmt::Let { local, init: Some(Expr { kind: ExprKind::Int(n), .. }) }, Some(c), false) = (init, cond, do_while) else { return None };
    if *n < 0 {
        return None;
    }
    let ExprKind::Binary(BinOp::Lt, lhs, rhs) = &c.kind else { return None };
    let (l, dim) = match &rhs.kind {
        ExprKind::ListLen(l) => (&**l, None),
        ExprKind::Builtin { f: Builtin::ArrayGetLength, args, .. } => match args[1].kind {
            ExprKind::Int(d) => (&args[0], Some(d as usize)),
            _ => return None,
        },
        _ => return None,
    };
    let ExprKind::Local(i) = lhs.kind else { return None };
    let step_ok = step.len() == 1
        && match &step[0].kind {
            ExprKind::IncDec { target, inc: true, .. } => matches!(target.kind, ExprKind::Local(x) if x == i),
            ExprKind::Assign { op: Some(BinOp::Add), target, value } => matches!(target.kind, ExprKind::Local(x) if x == i) && matches!(value.kind, ExprKind::Int(1)),
            _ => false,
        };
    if i != *local || !step_ok {
        return None;
    }
    let lr = list_ref(l)?;
    if loop_disturbs(body, i, &lr) {
        return None;
    }
    Some((i, l, dim))
}

/// `counting_loop`, and also `for (i = 0; i < N; i++)` with a constant N over the fixed-length array
/// field (`int[K.M] F`, M >= N) that the body indexes with `i`: the list is the first such field
/// indexed in the body, and the last item is the declared length to verify before the loop.
pub fn counting_loop_fixed<'a>(p: &Program, init: &Stmt, cond: &'a Option<Expr>, step: &[Expr], do_while: bool, body: &'a Block) -> Option<(LocalId, &'a Expr, Option<usize>, Option<i32>)> {
    if let Some((i, l, d)) = counting_loop(init, cond, step, do_while, body) {
        return Some((i, l, d, None));
    }
    let (Stmt::Let { local, init: Some(Expr { kind: ExprKind::Int(n0), .. }) }, Some(c), false) = (init, cond, do_while) else { return None };
    let ExprKind::Binary(BinOp::Lt, lhs, rhs) = &c.kind else { return None };
    let ExprKind::Local(i) = lhs.kind else { return None };
    let bound = match &rhs.kind {
        ExprKind::Int(n) => *n,
        ExprKind::Const(k) => match &p.consts[*k].value {
            Some(ConstVal::Int(n)) => *n as i64,
            _ => return None,
        },
        _ => return None,
    };
    let step_ok = step.len() == 1
        && match &step[0].kind {
            ExprKind::IncDec { target, inc: true, .. } => matches!(target.kind, ExprKind::Local(x) if x == i),
            ExprKind::Assign { op: Some(BinOp::Add), target, value } => matches!(target.kind, ExprKind::Local(x) if x == i) && matches!(value.kind, ExprKind::Int(1)),
            _ => false,
        };
    if i != *local || *n0 < 0 || !step_ok {
        return None;
    }
    // the first fixed-length field indexed by the counter
    let mut found: Option<&'a Expr> = None;
    fn find<'b>(b: &'b Block, i: LocalId, p: &Program, out: &mut Option<&'b Expr>) {
        for s in &b.stmts {
            let mut es: Vec<&'b Expr> = vec![];
            match s {
                Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => es.push(e),
                Stmt::If { cond, .. } => es.push(cond),
                Stmt::Loop { cond: Some(c), .. } => es.push(c),
                _ => {}
            }
            for e in es {
                visit_ref(e, &mut |x: &'b Expr| {
                    if out.is_some() {
                        return;
                    }
                    if let ExprKind::ListIndex { list, index, .. } = &x.kind {
                        if matches!(index.kind, ExprKind::Local(y) if y == i) {
                            if let ExprKind::EField { field, base } = &list.kind {
                                if p.fields[*field].fixed_len.is_some() && matches!(base.kind, ExprKind::SelfRef | ExprKind::Local(_)) {
                                    *out = Some(&**list);
                                }
                            }
                        }
                    }
                });
            }
            for sb in s.sub_blocks() {
                find(sb, i, p, out);
            }
        }
    }
    find(body, i, p, &mut found);
    let l = found?;
    let ExprKind::EField { field, .. } = &l.kind else { return None };
    let n = p.fields[*field].fixed_len?;
    if bound > n as i64 {
        return None;
    }
    let lr = list_ref(l)?;
    if loop_disturbs(body, i, &lr) {
        return None;
    }
    Some((i, l, None, Some(n)))
}

/// visits `e` and its sub-expressions with references that live as long as `e`
fn visit_ref<'b>(e: &'b Expr, f: &mut dyn FnMut(&'b Expr)) {
    f(e);
    for c in children_ref(e) {
        visit_ref(c, f);
    }
}

fn children_ref(e: &Expr) -> Vec<&Expr> {
    crate::sema::children(e)
}

/// Why a counting loop over a list keeps the bounds checks of `L[i]` (for `sijoc explain`), or None
/// when it is not a loop of that shape.
pub fn loop_check_reason(init: Option<&Stmt>, cond: &Option<Expr>, step: &[Expr], do_while: bool, body: &Block) -> Option<(String, Option<Span>)> {
    let c = cond.as_ref()?;
    let ExprKind::Binary(op, lhs, rhs) = &c.kind else { return None };
    let l = match &rhs.kind {
        ExprKind::ListLen(l) => &**l,
        ExprKind::Builtin { f: Builtin::ArrayGetLength, args, .. } => &args[0],
        _ => return None,
    };
    let ExprKind::Local(i) = lhs.kind else { return None };
    if do_while {
        return Some(("a do-while loop runs its body before the first test".into(), None));
    }
    if *op != BinOp::Lt {
        return Some((format!("the condition is `i {} length`, not `i < length`", op.sym()), None));
    }
    match init {
        Some(Stmt::Let { local, init: Some(Expr { kind: ExprKind::Int(n), .. }) }) if *local == i => {
            if *n < 0 {
                return Some(("the counter starts below 0".into(), None));
            }
        }
        _ => return Some(("the counter does not start at a constant 0 or more right before the loop".into(), None)),
    }
    let step_ok = step.len() == 1
        && match &step[0].kind {
            ExprKind::IncDec { target, inc: true, .. } => matches!(target.kind, ExprKind::Local(x) if x == i),
            ExprKind::Assign { op: Some(BinOp::Add), target, value } => matches!(target.kind, ExprKind::Local(x) if x == i) && matches!(value.kind, ExprKind::Int(1)),
            _ => false,
        };
    if !step_ok {
        return Some(("the step is not `i++` or `i += 1`".into(), None));
    }
    let Some(lr) = list_ref(l) else {
        return Some(("the list is not a local, a field of this entity or a field read through a handle or cursor held in a local".into(), None));
    };
    loop_disturbance(body, i, &lr)
}

fn bce_block(p: &Program, b: &mut Block, st: &mut OptStats) {
    // look for: Let i = <non-negative int>; Loop { cond: i < L.Count, step: i++ / i += 1 }
    // (or i < A.GetLength(d) on a multi-dimensional array: A[.., i, ..] with i in dimension d)
    for k in 1..b.stmts.len() {
        let (before, after) = b.stmts.split_at_mut(k);
        if let Stmt::Loop { cond, step, body, do_while } = &mut after[0] {
            let found = counting_loop_fixed(p, &before[k - 1], cond, step, *do_while, body).map(|(i, l, dim, _)| (i, list_ref(l).unwrap(), dim));
            if let Some((i, lr, dim)) = found {
                let n = match dim {
                    None => unchecked_indexes(body, i, &lr),
                    Some(d) => unchecked_md_indexes(body, i, &lr, d),
                };
                st.bounds_checks_removed += n;
            }
        }
    }
    for s in b.stmts.iter_mut() {
        match s {
            Stmt::If { then, els, .. } => {
                bce_block(p, then, st);
                if let Some(e) = els {
                    bce_block(p, e, st);
                }
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } => {
                bce_block(p, body, st)
            }
            Stmt::Switch { sections, default, .. } => {
                for (_, x) in sections.iter_mut() {
                    bce_block(p, x, st);
                }
                if let Some(x) = default {
                    bce_block(p, x, st);
                }
            }
            _ => {}
        }
    }
}

/// Can the loop body change the index `i` or the length of the list `lr`? (then `lr[i]` keeps its check)
fn loop_disturbs(body: &Block, i: LocalId, lr: &ListRef) -> bool {
    loop_disturbance(body, i, lr).is_some()
}

/// What in the loop body may change the index `i` or the list `lr`, if anything
fn loop_disturbance(body: &Block, i: LocalId, lr: &ListRef) -> Option<(String, Option<Span>)> {
    if has_await(body) {
        return Some(("the body awaits (the loop resumes on a later tick)".into(), None));
    }
    let mut why: Option<(String, Option<Span>)> = None;
    let is_i = |x: Option<&Expr>| matches!(x.map(|x| &x.kind), Some(ExprKind::Local(x)) if *x == i);
    crate::sema::visit_block(body, &mut |e| {
        if why.is_some() {
            return;
        }
        match &e.kind {
            ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => {
                if is_i(Some(target)) {
                    why = Some(("the body changes the counter".into(), Some(e.span)));
                } else if touches(target, lr) {
                    why = Some(("the body assigns the list".into(), Some(e.span)));
                }
            }
            ExprKind::ListCall { list, op, .. } => {
                if op.mutates() && touches(list, lr) {
                    why = Some(("the body changes the list's length or contents through a list method".into(), Some(e.span)));
                }
            }
            // xs.RemoveAll(x => ...) / xs.Sort((a, b) => ...)
            ExprKind::ListLambda { list, op, .. } => {
                if op.mutates() && touches(list, lr) {
                    why = Some(("the body changes the list through a list method".into(), Some(e.span)));
                }
            }
            // int.TryParse(s, out i) / d.TryGetValue(k, out i) write their out argument
            ExprKind::Builtin { f: Builtin::TryParseInt | Builtin::TryParseFloat, args, .. } | ExprKind::DictCall { op: DictOp::TryGet, args, .. } => {
                if is_i(args.get(1)) {
                    why = Some(("the body writes the counter through `out`".into(), Some(e.span)));
                }
            }
            ExprKind::Call { args, dispatch, .. } => {
                for a in args {
                    if matches!(a.mode, ParamMode::Ref | ParamMode::Out) && (is_i(Some(&a.expr)) || touches(&a.expr, lr)) {
                        why = Some(("the body passes the counter or the list by `ref`/`out`".into(), Some(e.span)));
                    }
                    // the list moves into an `own` parameter (it is empty afterwards)
                    if a.mode == ParamMode::Own && touches(&a.expr, lr) {
                        why = Some(("the body moves the list into an `own` parameter".into(), Some(e.span)));
                    }
                }
                if matches!(lr, ListRef::SelfField(_) | ListRef::Other(..)) && matches!(dispatch, Dispatch::SelfCall | Dispatch::BaseCall) {
                    // a self method could modify the list field
                    why = Some(("the body calls a method of this entity, which may change the field".into(), Some(e.span)));
                }
            }
            _ => {}
        }
    });
    why
}

fn unchecked_indexes(body: &mut Block, i: LocalId, lr: &ListRef) -> usize {
    let mut n = 0;
    visit_block_mut(body, &mut |e| {
        if let ExprKind::ListIndex { list, index, checked } = &mut e.kind {
            if *checked && matches!(index.kind, ExprKind::Local(x) if x == i) && list_ref(list).as_ref() == Some(lr) {
                *checked = false;
                n += 1;
            }
        }
    });
    n
}

/// `A[.., i, ..]` with `i` in dimension `d` of the multi-dimensional array `lr` (the loop runs i
/// from 0 below A.GetLength(d)): that index loses its check
fn unchecked_md_indexes(body: &mut Block, i: LocalId, lr: &ListRef, d: usize) -> usize {
    let mut n = 0;
    visit_block_mut(body, &mut |e| {
        if let ExprKind::ListIndex { list, index, .. } = &mut e.kind {
            if list_ref(list).as_ref() != Some(lr) {
                return;
            }
            if let ExprKind::MdIndex { idx, checked } = &mut index.kind {
                if d < idx.len() && checked[d] && matches!(idx[d].kind, ExprKind::Local(x) if x == i) {
                    checked[d] = false;
                    n += 1;
                }
            }
        }
    });
    n
}

// ---------------------------------------------------------------------------
// mutable visitors
// ---------------------------------------------------------------------------

pub fn children_mut(e: &mut Expr) -> Vec<&mut Expr> {
    match &mut e.kind {
        ExprKind::Interp(parts) => parts
            .iter_mut()
            .filter_map(|p| match p {
                IPart::Expr(x, _) => Some(x),
                _ => None,
            })
            .collect(),
        ExprKind::Field { base, .. } | ExprKind::EField { base, .. } => vec![&mut **base],
        ExprKind::Unary(_, x) | ExprKind::Cast(x) | ExprKind::ListLen(x) => vec![&mut **x],
        ExprKind::Binary(_, a, b) => vec![&mut **a, &mut **b],
        ExprKind::Assign { target, value, .. } => vec![&mut **target, &mut **value],
        ExprKind::IncDec { target, .. } => vec![&mut **target],
        ExprKind::Cond(a, b, c) => vec![&mut **a, &mut **b, &mut **c],
        ExprKind::Call { recv, args, .. } => {
            let mut v: Vec<&mut Expr> = vec![];
            if let Some(r) = recv {
                v.push(&mut **r);
            }
            v.extend(args.iter_mut().map(|a| &mut a.expr));
            v
        }
        ExprKind::OpCall { args, .. } | ExprKind::Builtin { args, .. } => args.iter_mut().collect(),
        ExprKind::StructLit { args, inits, .. } => {
            let mut v: Vec<&mut Expr> = args.iter_mut().map(|a| &mut a.expr).collect();
            v.extend(inits.iter_mut().map(|(_, e)| e));
            v
        }
        ExprKind::Spawn { inits, .. } => inits.iter_mut().map(|(_, e)| e).collect(),
        ExprKind::Destroy(Some(x)) => vec![&mut **x],
        ExprKind::CompAdd { target, inits, .. } => std::iter::once(&mut **target).chain(inits.iter_mut().map(|(_, e)| e)).collect(),
        ExprKind::CompRemove { target, .. } => vec![&mut **target],
        ExprKind::Is { expr, .. } => vec![&mut **expr],
        ExprKind::ListClone { list, .. } => vec![&mut **list],
        ExprKind::ArrayNew { len, elems, dims, .. } => len.iter_mut().map(|l| &mut **l).chain(dims.iter_mut()).chain(elems.iter_mut()).collect(),
        ExprKind::ListLambda { list, body, args, .. } => {
            let mut v: Vec<&mut Expr> = vec![&mut **list];
            v.extend(args.iter_mut());
            v.push(&mut **body);
            v
        }
        ExprKind::ListIndex { list, index, .. } => vec![&mut **list, &mut **index],
        ExprKind::MdIndex { idx, .. } => idx.iter_mut().collect(),
        ExprKind::ListCall { list, args, .. } => {
            let mut v: Vec<&mut Expr> = vec![&mut **list];
            v.extend(args.iter_mut());
            v
        }
        ExprKind::DictGet { dict, key } => vec![&mut **dict, &mut **key],
        ExprKind::DictSet { dict, key, value } => vec![&mut **dict, &mut **key, &mut **value],
        ExprKind::DictCall { dict, args, .. } => {
            let mut v: Vec<&mut Expr> = vec![&mut **dict];
            v.extend(args.iter_mut());
            v
        }
        ExprKind::Switch { scrut, arms, default, .. } => {
            let mut v: Vec<&mut Expr> = vec![&mut **scrut];
            for (c, x) in arms.iter_mut() {
                v.push(c);
                v.push(x);
            }
            if let Some(d) = default {
                v.push(&mut **d);
            }
            v
        }
        ExprKind::Closure { caps, .. } => caps.iter_mut().collect(),
        ExprKind::DgFn(x) => vec![&mut **x],
        ExprKind::DgCap { d, .. } => vec![&mut **d],
        _ => vec![],
    }
}

pub fn visit_expr_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Expr)) {
    f(e);
    for c in children_mut(e) {
        visit_expr_mut(c, f);
    }
}

pub fn visit_block_mut(b: &mut Block, f: &mut dyn FnMut(&mut Expr)) {
    for s in b.stmts.iter_mut() {
        match s {
            Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => visit_expr_mut(e, f),
            Stmt::If { cond, then, els } => {
                visit_expr_mut(cond, f);
                visit_block_mut(then, f);
                if let Some(e) = els {
                    visit_block_mut(e, f);
                }
            }
            Stmt::Loop { cond, step, body, .. } => {
                if let Some(c) = cond {
                    visit_expr_mut(c, f);
                }
                for e in step.iter_mut() {
                    visit_expr_mut(e, f);
                }
                visit_block_mut(body, f);
            }
            Stmt::ForList { list, body, .. } => {
                visit_expr_mut(list, f);
                visit_block_mut(body, f);
            }
            Stmt::ForQuery { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => visit_block_mut(body, f),
            Stmt::Switch { scrut, sections, default, .. } => {
                visit_expr_mut(scrut, f);
                for (c, x) in sections.iter_mut() {
                    visit_expr_mut(c, f);
                    visit_block_mut(x, f);
                }
                if let Some(x) = default {
                    visit_block_mut(x, f);
                }
            }
            Stmt::Await { kind, state, timer, .. } => {
                visit_expr_mut(state, f);
                visit_expr_mut(timer, f);
                for e in kind.exprs_mut() {
                    visit_expr_mut(e, f);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// e-graph rewriting with value ranges (see egraph.rs)
// ---------------------------------------------------------------------------

use crate::egraph::{self as eg, AtomInfo, EGraph, ENode, Op};

const EG_NODE_LIMIT: usize = 1000;

/// an e-graph plus the HIR expressions of its atoms
struct Hg {
    g: EGraph,
    exprs: Vec<Expr>,
    keys: HashMap<String, eg::Id>,
    /// nesting of local definitions being added
    depth: usize,
    /// equal reads share one class: only in an expression without side effects (a call in the
    /// middle may change a field between two reads of it); stable locals and constants always
    share: bool,
}

impl Hg {
    fn new(checked: bool) -> Hg {
        Hg { g: EGraph::new(checked), exprs: vec![], keys: HashMap::new(), depth: 0, share: true }
    }
}
const EG_ITERS: usize = 8;

/// One function: every maximal pure int/bool expression goes through equality saturation. On the
/// way the pass collects facts about locals in statement order (an immutable local's value lies in
/// its initializer's range; a `for` counter lies between its start and its bound inside the body;
/// an array's length is fixed when it is created) and uses them for bounds checks, write clamps and
/// conditions.
struct EgPass<'a> {
    p: &'a Program,
    fid: FuncId,
    rules: &'a [eg::Rule],
    checked: bool,
    st: &'a mut OptStats,
    /// ranges of immutable locals and loop counters
    local_iv: HashMap<LocalId, (i64, i64)>,
    /// exact lengths of array locals that are never replaced
    array_len: HashMap<LocalId, i64>,
    /// exact lengths of the dimensions of multi-dimensional array locals that are never replaced
    md_dims: HashMap<LocalId, Vec<i64>>,
    /// locals whose value changes after the declaration (no facts from the initializer)
    written: HashSet<LocalId>,
    /// locals replaced as a whole after the declaration (an array keeps its lengths otherwise)
    replaced: HashSet<LocalId>,
    lets: HashMap<LocalId, usize>,
    /// locals kept in coroutine frame fields across awaits: a live reload may resume them with a
    /// value another version of the program computed, so nothing is assumed about them
    hoisted: HashSet<LocalId>,
    /// definitions of immutable locals that read only literals and other such locals: the e-graph
    /// may see through them (`int big = a + K; ... big + 1 - a`)
    local_def: HashMap<LocalId, Expr>,
}

impl<'a> EgPass<'a> {
    fn new(p: &'a Program, fid: FuncId, body: &Block, rules: &'a [eg::Rule], checked: bool, st: &'a mut OptStats) -> EgPass<'a> {
        let mut inits = HashMap::new();
        let mut lets = HashMap::new();
        collect_lets(body, &mut inits, &mut lets);
        let hoisted = p.funcs[fid].co.as_ref().map(|c| c.hoisted.iter().map(|(l, _)| *l).collect()).unwrap_or_default();
        EgPass { p, fid, rules, checked, st, local_iv: HashMap::new(), array_len: HashMap::new(), md_dims: HashMap::new(), written: written_locals(body), replaced: replaced_locals(body), lets, hoisted, local_def: HashMap::new() }
    }

    fn func(&self) -> &Func {
        &self.p.funcs[self.fid]
    }

    fn block(&mut self, b: &mut Block) {
        let stmts = std::mem::take(&mut b.stmts);
        let mut prev_let: Option<(LocalId, Option<(i64, i64)>)> = None;
        for mut s in stmts {
            let mut this_let = None;
            match &mut s {
                Stmt::Let { local, init: Some(e) } => {
                    self.expr(e);
                    let l = *local;
                    let iv = if e.ty == Ty::Int { self.interval(e) } else { None };
                    this_let = Some((l, iv));
                    let stable = !self.written.contains(&l) && self.lets.get(&l) == Some(&1) && !self.hoisted.contains(&l) && self.func().locals[l].kind == LocalKind::Normal;
                    if stable {
                        if (e.ty == Ty::Int || e.ty == Ty::Bool) && self.stable_expr(e, 0) {
                            self.local_def.insert(l, e.clone());
                        }
                        if let Some(iv) = iv {
                            let iv = match self.func().locals[l].range {
                                Some((lo, hi)) => (iv.0.max(lo.ceil() as i64), iv.1.min(hi.floor() as i64)),
                                None => iv,
                            };
                            if iv.0 <= iv.1 {
                                self.local_iv.insert(l, iv);
                            }
                        }
                        if let (Ty::Array(_), ExprKind::ArrayNew { len, elems, .. }) = (&e.ty, &e.kind) {
                            let n = match len {
                                Some(x) => match x.kind {
                                    ExprKind::Int(n) if n >= 0 => Some(n),
                                    _ => None,
                                },
                                None => Some(elems.len() as i64),
                            };
                            if let Some(n) = n {
                                self.array_len.insert(l, n);
                            }
                        }
                    }
                    // `mut int[,] g = new int[8, 8];`: writing elements keeps the lengths; only
                    // replacing the whole array (or passing it ref/out/own) loses them
                    let kept = !self.replaced.contains(&l) && self.lets.get(&l) == Some(&1) && !self.hoisted.contains(&l) && self.func().locals[l].kind == LocalKind::Normal;
                    if let (true, Ty::MdArray(..), ExprKind::ArrayNew { dims, .. }) = (kept, &e.ty, &e.kind) {
                        let ns: Vec<i64> = dims
                            .iter()
                            .filter_map(|d| match d.kind {
                                ExprKind::Int(n) if n >= 0 => Some(n),
                                _ => None,
                            })
                            .collect();
                        let fits = ns.iter().try_fold(1i64, |acc, &n| acc.checked_mul(n).filter(|&p| p <= i32::MAX as i64)).is_some();
                        if ns.len() == dims.len() && fits {
                            self.md_dims.insert(l, ns);
                        }
                    }
                }
                Stmt::Expr(e) | Stmt::Return(Some(e), _) => self.expr(e),
                Stmt::If { cond, then, els } => {
                    self.expr(cond);
                    if let ExprKind::Bool(c) = cond.kind {
                        // decided here (fold_block already took the literal conditions of the source)
                        self.st.egraph_branches += 1;
                        let mut chosen = if c { std::mem::take(then) } else { els.take().unwrap_or_default() };
                        self.block(&mut chosen);
                        b.stmts.push(Stmt::Block(chosen));
                        prev_let = None;
                        continue;
                    }
                    self.block(then);
                    if let Some(e) = els {
                        self.block(e);
                    }
                }
                Stmt::Loop { cond, step, body, do_while } => {
                    if let Some(c) = cond {
                        self.expr(c);
                    }
                    for e in step.iter_mut() {
                        self.expr(e);
                    }
                    if let (Some(Expr { kind: ExprKind::Bool(false), .. }), false) = (&cond, *do_while) {
                        self.st.egraph_branches += 1;
                        prev_let = None;
                        continue;
                    }
                    // for (i = a; i < b; i++) with i changed only by the step: a <= i < b in the body
                    let counter = self.counter_fact(prev_let, cond.as_ref(), step, body);
                    let saved = counter.map(|(i, iv)| (i, self.local_iv.insert(i, iv)));
                    self.block(body);
                    if let Some((i, old)) = saved {
                        match old {
                            Some(v) => self.local_iv.insert(i, v),
                            None => self.local_iv.remove(&i),
                        };
                    }
                }
                Stmt::ForQuery { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => self.block(body),
                Stmt::ForList { list, body, .. } => {
                    self.expr(list);
                    self.block(body);
                }
                Stmt::Switch { scrut, sections, default, .. } => {
                    self.expr(scrut);
                    for (c, x) in sections.iter_mut() {
                        self.expr(c);
                        self.block(x);
                    }
                    if let Some(x) = default {
                        self.block(x);
                    }
                }
                Stmt::Await { kind, .. } => {
                    if let AwaitKind::Wait(e) | AwaitKind::Until(e) = kind {
                        self.expr(e);
                    }
                }
                _ => {}
            }
            prev_let = this_let;
            b.stmts.push(s);
        }
    }

    /// a local that never changes after its (single) declaration
    fn stable_local(&self, l: LocalId) -> bool {
        let loc = &self.func().locals[l];
        !self.written.contains(&l)
            && !self.hoisted.contains(&l)
            && match loc.kind {
                LocalKind::Normal => self.lets.get(&l) == Some(&1),
                LocalKind::Param(ParamMode::Value) => true,
                _ => false,
            }
    }

    /// an int/bool expression of literals, constants and stable locals (its value never changes)
    fn stable_expr(&self, e: &Expr, depth: usize) -> bool {
        if depth > 32 || expr_size(e) > 40 {
            return false;
        }
        let ok = match &e.kind {
            ExprKind::Int(_) | ExprKind::Bool(_) | ExprKind::Const(_) => true,
            ExprKind::Local(l) => self.stable_local(*l),
            ExprKind::Binary(op, _, b) => !matches!(op, BinOp::Div | BinOp::Rem) || matches!(b.kind, ExprKind::Int(n) if n != 0),
            ExprKind::Unary(..) | ExprKind::Cond(..) => true,
            _ => false,
        };
        ok && crate::sema::children(e).into_iter().all(|c| self.stable_expr(c, depth + 1))
    }

    /// the range of a `for` counter inside the loop body
    fn counter_fact(&mut self, prev: Option<(LocalId, Option<(i64, i64)>)>, cond: Option<&Expr>, step: &[Expr], body: &Block) -> Option<(LocalId, (i64, i64))> {
        let (i, Some(start)) = prev? else { return None };
        if self.func().locals[i].ty != Ty::Int || self.hoisted.contains(&i) {
            return None;
        }
        let ExprKind::Binary(op @ (BinOp::Lt | BinOp::Le), lhs, bound) = &cond?.kind else { return None };
        if !matches!(lhs.kind, ExprKind::Local(x) if x == i) || !is_pure_expr(bound) {
            return None;
        }
        let step_ok = step.len() == 1
            && match &step[0].kind {
                ExprKind::IncDec { target, inc: true, .. } => matches!(target.kind, ExprKind::Local(x) if x == i),
                ExprKind::Assign { op: Some(BinOp::Add), target, value } => matches!(target.kind, ExprKind::Local(x) if x == i) && matches!(value.kind, ExprKind::Int(1)),
                _ => false,
            };
        if !step_ok || writes_local(body, i) {
            return None;
        }
        let (_, bhi) = self.interval(bound)?;
        // i < b <= MAX: i + 1 never wraps. With <=, i == MAX would wrap: need b < MAX.
        let hi = if *op == BinOp::Lt { bhi - 1 } else if bhi < i32::MAX as i64 { bhi } else { return None };
        (start.0 <= hi).then_some((i, (start.0, hi)))
    }

    /// the range of an int expression's values (without changing it)
    fn interval(&mut self, e: &Expr) -> Option<(i64, i64)> {
        let mut h = Hg::new(self.checked);
        h.share = is_pure_expr(e);
        let mut work = e.clone();
        let root = self.graph(&mut work, &mut h, false);
        h.g.rebuild();
        let d = h.g.data(root);
        (!d.boolean).then_some((d.lo, d.hi))
    }

    fn modeled_root(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Binary(..) | ExprKind::Unary(..) | ExprKind::Cond(..) => matches!(e.ty, Ty::Int | Ty::Bool),
            ExprKind::Builtin { f: Builtin::Saturate | Builtin::MathMin | Builtin::MathMax | Builtin::MathAbs | Builtin::MathClamp, .. } => e.ty == Ty::Int,
            _ => false,
        }
    }

    /// optimizes an expression in a value position
    fn expr(&mut self, e: &mut Expr) {
        if Self::modeled_root(e) {
            self.saturate(e);
            return;
        }
        match &mut e.kind {
            ExprKind::Assign { op, target, value } => {
                self.place(target);
                self.expr(value);
                let nv = match op {
                    None => (**value).clone(),
                    Some(o) => Expr::new(ExprKind::Binary(*o, target.clone(), value.clone()), target.ty.clone(), e.span),
                };
                self.note_clamp(target, &nv);
            }
            ExprKind::IncDec { target, inc, .. } => {
                self.place(target);
                let one = Expr::new(ExprKind::Int(1), Ty::Int, e.span);
                let nv = Expr::new(ExprKind::Binary(if *inc { BinOp::Add } else { BinOp::Sub }, target.clone(), Box::new(one)), target.ty.clone(), e.span);
                self.note_clamp(target, &nv);
            }
            ExprKind::Call { recv, args, .. } => {
                if let Some(r) = recv {
                    self.place(r);
                }
                for a in args.iter_mut() {
                    if a.mode == ParamMode::Value {
                        self.expr(&mut a.expr);
                    } else {
                        self.place(&mut a.expr);
                    }
                }
            }
            ExprKind::ListCall { list, args, .. } => {
                self.place(list);
                for a in args.iter_mut() {
                    self.expr(a);
                }
            }
            ExprKind::ListIndex { .. } => self.index(e),
            ExprKind::Spawn { inits, .. } | ExprKind::CompAdd { inits, .. } => {
                for (f, x) in inits.iter_mut() {
                    self.expr(x);
                    if x.ty == Ty::Int {
                        if let Some(r) = self.p.fields[*f].attrs.range {
                            let x = x.clone();
                            self.note_clamp_range(r, &x, x.span);
                        }
                    }
                }
            }
            _ => {
                for c in children_mut(e) {
                    self.expr(c);
                }
            }
        }
    }

    /// a place (assignment target, ref/out argument, receiver): only the values inside it change
    fn place(&mut self, e: &mut Expr) {
        match &mut e.kind {
            ExprKind::Field { base, .. } => self.place(base),
            ExprKind::EField { base, .. } => self.expr(base),
            ExprKind::ListIndex { .. } => self.index(e),
            ExprKind::DictGet { dict, key } => {
                self.place(dict);
                self.expr(key);
            }
            _ => {}
        }
    }

    /// `xs[i]`: the index check goes away when i lies inside an array of known length
    fn index(&mut self, e: &mut Expr) {
        let ExprKind::ListIndex { list, index, checked } = &mut e.kind else { return };
        self.place(list);
        self.expr(index);
        // a[i, j] on an array of known lengths: each index that lies in its dimension loses its check
        if let ExprKind::MdIndex { idx, checked: dchecked } = &mut index.kind {
            let ExprKind::Local(l) = list.kind else { return };
            let Some(dims) = self.md_dims.get(&l).cloned() else { return };
            for (d, x) in idx.iter().enumerate() {
                if !dchecked[d] {
                    continue;
                }
                if let Some((lo, hi)) = self.interval(x) {
                    if lo >= 0 && hi < dims[d] {
                        dchecked[d] = false;
                        self.st.bounds_checks_removed += 1;
                        self.st.range_bce += 1;
                    }
                }
            }
            return;
        }
        if !*checked {
            return;
        }
        let ExprKind::Local(l) = list.kind else { return };
        let Some(&n) = self.array_len.get(&l) else { return };
        let ix = (**index).clone();
        if let Some((lo, hi)) = self.interval(&ix) {
            if lo >= 0 && hi < n {
                *checked = false;
                self.st.bounds_checks_removed += 1;
                self.st.range_bce += 1;
            }
        }
    }

    /// a write to a ranged int local or field of `this`: which bounds can the new value cross?
    fn note_clamp(&mut self, target: &Expr, nv: &Expr) {
        if target.ty != Ty::Int {
            return;
        }
        let range = match &target.kind {
            ExprKind::Local(l) => self.func().locals[*l].range,
            ExprKind::EField { base, field } if matches!(base.kind, ExprKind::SelfRef) => self.p.fields[*field].attrs.range,
            _ => None,
        };
        let Some(r) = range else { return };
        self.note_clamp_range(r, nv, target.span);
    }

    /// records which bounds of `range` the value `nv` can cross, for the write at `site`
    fn note_clamp_range(&mut self, (lo, hi): (f64, f64), nv: &Expr, site: crate::diag::Span) {
        let (lo, hi) = (lo.ceil() as i64, hi.floor() as i64);
        let (vlo, vhi) = self.interval(nv).unwrap_or((i32::MIN as i64, i32::MAX as i64));
        let need = (vlo < lo, vhi > hi);
        let key = (self.fid, site.lo, site.hi);
        let e = self.st.clamp_need.entry(key).or_insert(need);
        *e = (e.0 || need.0, e.1 || need.1);
    }

    /// facts about an atom
    fn atom_info(&self, e: &Expr) -> AtomInfo {
        let full = (i32::MIN as i64, i32::MAX as i64);
        let boolean = e.ty == Ty::Bool;
        let cost = expr_size(e).min(64) as u32;
        let pure = is_pure_expr(e);
        let safe = pure && self.cannot_panic(e);
        if boolean {
            return AtomInfo { boolean, lo: 0, hi: 1, safe, pure, cost };
        }
        let int_range = |r: Option<(f64, f64)>| r.map(|(lo, hi)| (lo.ceil() as i64, hi.floor() as i64)).filter(|(a, b)| a <= b);
        let (lo, hi) = match &e.kind {
            ExprKind::Local(l) => {
                let r = int_range(self.func().locals[*l].range).unwrap_or(full);
                match self.local_iv.get(l) {
                    Some(&(a, b)) => (a.max(r.0), b.min(r.1)),
                    None => r,
                }
            }
            ExprKind::EField { field, .. } if e.ty == Ty::Int => int_range(self.p.fields[*field].attrs.range).unwrap_or(full),
            // a member of a ranged struct field: the range applies to every int member
            ExprKind::Field { base, sid, idx } if e.ty == Ty::Int && self.p.structs[*sid].fields[*idx].ty == Ty::Int => match &base.kind {
                ExprKind::EField { field, .. } => int_range(self.p.fields[*field].attrs.range).unwrap_or(full),
                _ => full,
            },
            ExprKind::ListLen(x) => match &x.kind {
                ExprKind::Local(l) if self.array_len.contains_key(l) => {
                    let n = self.array_len[l];
                    (n, n)
                }
                _ => (0, full.1),
            },
            ExprKind::Count(_) => (0, full.1),
            ExprKind::Builtin { f: Builtin::StrLen | Builtin::StrByteLen, .. } => (0, full.1),
            ExprKind::Builtin { f: Builtin::ArrayGetLength, args, .. } => match (&args[0].kind, &args[1].kind) {
                (ExprKind::Local(l), ExprKind::Int(d)) if self.md_dims.get(l).and_then(|ns| ns.get(*d as usize)).is_some() => {
                    let n = self.md_dims[l][*d as usize];
                    (n, n)
                }
                _ => (0, full.1),
            },
            _ => full,
        };
        let (lo, hi) = if lo <= hi { (lo, hi) } else { full };
        AtomInfo { boolean, lo, hi, safe, pure, cost }
    }

    /// evaluating `e` cannot panic (stale handles, indexes, division, checked overflow)
    fn cannot_panic(&self, e: &Expr) -> bool {
        let ok = match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Null | ExprKind::Str(_) | ExprKind::EnumVal(..) | ExprKind::Const(_) | ExprKind::Local(_) | ExprKind::SelfRef | ExprKind::StructSelf | ExprKind::Count(_) => true,
            ExprKind::Field { .. } | ExprKind::ListLen(_) | ExprKind::Cond(..) => true,
            // this entity and query cursors are live rows; other handles may be stale
            ExprKind::EField { base, .. } => match &base.kind {
                ExprKind::SelfRef => true,
                ExprKind::Local(l) => matches!(self.func().locals[*l].kind, LocalKind::Cursor(_)),
                _ => false,
            },
            ExprKind::Unary(UnOp::Neg, x) => !(self.checked && x.ty == Ty::Int),
            ExprKind::Unary(..) => true,
            ExprKind::Binary(op, a, _) => match op {
                BinOp::Div | BinOp::Rem => a.ty != Ty::Int,
                BinOp::Add | BinOp::Sub | BinOp::Mul => !(self.checked && a.ty == Ty::Int),
                _ => true,
            },
            ExprKind::Cast(x) => !(self.checked && x.ty == Ty::Float && e.ty == Ty::Int),
            ExprKind::Builtin { f, .. } => matches!(
                f,
                Builtin::TimeDt | Builtin::TimeTick | Builtin::Saturate | Builtin::MathMin | Builtin::MathMax | Builtin::MathClamp | Builtin::MathFloor | Builtin::MathSqrt | Builtin::StrLen | Builtin::StrByteLen
            ) || (*f == Builtin::MathAbs && !(self.checked && e.ty == Ty::Int)),
            _ => false,
        };
        ok && crate::sema::children(e).into_iter().all(|c| self.cannot_panic(c))
    }

    /// Adds an expression to the e-graph. With `optimize`, the parts it does not model are
    /// optimized on their own first (they become atoms).
    fn graph(&mut self, e: &mut Expr, g: &mut Hg, optimize: bool) -> eg::Id {
        let int2 = |a: &Expr, b: &Expr| a.ty == Ty::Int && b.ty == Ty::Int;
        let op = match &e.kind {
            ExprKind::Int(n) if e.ty == Ty::Int && *n >= i32::MIN as i64 && *n <= i32::MAX as i64 => return g.g.add(ENode::leaf(Op::Int(*n as i32))),
            ExprKind::Bool(b) if e.ty == Ty::Bool => return g.g.add(ENode::leaf(Op::Bool(*b))),
            ExprKind::Unary(UnOp::Plus, x) if x.ty == e.ty && matches!(e.ty, Ty::Int | Ty::Bool) => {
                let ExprKind::Unary(_, x) = &mut e.kind else { unreachable!() };
                return self.graph(x, g, optimize);
            }
            ExprKind::Binary(bop, a, b) => match bop {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Shl | BinOp::Shr | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor if int2(a, b) && e.ty == Ty::Int => Some(match bop {
                    BinOp::Add => Op::Add,
                    BinOp::Sub => Op::Sub,
                    BinOp::Mul => Op::Mul,
                    BinOp::Shl => Op::Shl,
                    BinOp::Shr => Op::Shr,
                    BinOp::BitAnd => Op::And,
                    BinOp::BitOr => Op::Or,
                    _ => Op::Xor,
                }),
                // division by a literal (other divisors may be zero: those stay atoms)
                BinOp::Div | BinOp::Rem if int2(a, b) && e.ty == Ty::Int && matches!(b.kind, ExprKind::Int(n) if n != 0) => Some(if *bop == BinOp::Div { Op::Div } else { Op::Rem }),
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne if int2(a, b) => Some(match bop {
                    BinOp::Lt => Op::Lt,
                    BinOp::Le => Op::Le,
                    BinOp::Gt => Op::Gt,
                    BinOp::Ge => Op::Ge,
                    BinOp::Eq => Op::Eq,
                    _ => Op::Ne,
                }),
                BinOp::Eq | BinOp::Ne if a.ty == Ty::Bool && b.ty == Ty::Bool => Some(if *bop == BinOp::Eq { Op::BEq } else { Op::BNe }),
                BinOp::And | BinOp::Or if a.ty == Ty::Bool && b.ty == Ty::Bool => Some(if *bop == BinOp::And { Op::LAnd } else { Op::LOr }),
                _ => None,
            },
            ExprKind::Unary(UnOp::Neg, x) if x.ty == Ty::Int && e.ty == Ty::Int => Some(Op::Neg),
            ExprKind::Unary(UnOp::BitNot, x) if x.ty == Ty::Int && e.ty == Ty::Int => Some(Op::BNot),
            ExprKind::Unary(UnOp::Not, x) if x.ty == Ty::Bool => Some(Op::LNot),
            ExprKind::Cond(c, a, b) if c.ty == Ty::Bool && a.ty == e.ty && b.ty == e.ty && matches!(e.ty, Ty::Int | Ty::Bool) => Some(Op::Sel),
            ExprKind::Builtin { f, args, .. } if e.ty == Ty::Int && args.iter().all(|a| a.ty == Ty::Int || matches!(f, Builtin::Saturate)) => match (f, args.as_slice()) {
                (Builtin::MathMin, [_, _]) => Some(Op::Min),
                (Builtin::MathMax, [_, _]) => Some(Op::Max),
                (Builtin::MathAbs, [_]) => Some(Op::Abs),
                (Builtin::Saturate, [x, lo, hi]) if x.ty == Ty::Int => match (&lo.kind, &hi.kind) {
                    (ExprKind::Float(lo), ExprKind::Float(hi)) => int_bounds(lo.ceil(), hi.floor()).map(|(a, b)| Op::Clamp(a, b)),
                    _ => None,
                },
                (Builtin::MathClamp, [_, lo, hi]) => match (&lo.kind, &hi.kind) {
                    (ExprKind::Int(lo), ExprKind::Int(hi)) => int_bounds(*lo as f64, *hi as f64).map(|(a, b)| Op::Clamp(a, b)),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        let Some(op) = op else {
            if optimize {
                // not modeled here: optimize its insides on their own, then it is a leaf
                if Self::modeled_root(e) {
                    // the root kind is modeled but not these operand types (handles, floats)
                    for c in children_mut(e) {
                        self.expr(c);
                    }
                } else {
                    self.expr(e);
                }
            }
            return self.atom(e, g);
        };
        let kids: Vec<&mut Expr> = match &mut e.kind {
            ExprKind::Binary(_, a, b) => vec![&mut **a, &mut **b],
            ExprKind::Unary(_, x) => vec![&mut **x],
            ExprKind::Cond(c, a, b) => vec![&mut **c, &mut **a, &mut **b],
            ExprKind::Builtin { args, .. } => match op {
                Op::Clamp(..) => vec![&mut args[0]],
                _ => args.iter_mut().collect(),
            },
            _ => unreachable!(),
        };
        let ids: Vec<eg::Id> = kids.into_iter().map(|k| self.graph(k, g, optimize)).collect();
        g.g.add(ENode::new(op, &ids))
    }

    /// a leaf; equal simple atoms (`x`, `this.Hp`, `o.Pos.X`, `xs.Count`) share one class
    fn atom(&mut self, e: &Expr, g: &mut Hg) -> eg::Id {
        let info = self.atom_info(e);
        let unchanging = match e.kind {
            ExprKind::Local(l) => self.stable_local(l),
            ExprKind::Const(_) => true,
            _ => false,
        };
        let key = if info.pure && (g.share || unchanging) { atom_key(e) } else { None };
        if let Some(k) = &key {
            if let Some(i) = g.keys.get(k) {
                return *i;
            }
        }
        g.exprs.push(e.clone());
        let id = g.g.add_atom(info);
        if let Some(k) = key {
            g.keys.insert(k, id);
        }
        // see through an immutable local's definition (bounded: definitions are small and only
        // refer to earlier locals)
        if let ExprKind::Local(l) = e.kind {
            if let Some(def) = self.local_def.get(&l).cloned() {
                if g.depth < 8 {
                    g.depth += 1;
                    let mut d = def;
                    let di = self.graph(&mut d, g, false);
                    g.depth -= 1;
                    g.g.union(id, di);
                }
            }
        }
        id
    }

    /// equality saturation of one pure expression; replaced when the cheapest form is cheaper
    fn saturate(&mut self, e: &mut Expr) {
        let mut h = Hg::new(self.checked);
        h.share = is_pure_expr(e);
        let root = self.graph(e, &mut h, true);
        let g = &mut h.g;
        let before = g.extract()[g.find(root) as usize].map(|b| b.0).unwrap_or(u32::MAX);
        g.saturate(self.rules, EG_NODE_LIMIT, EG_ITERS);
        self.st.egraph_nodes += g.len();
        let best = h.g.extract();
        let Some((after, _)) = best[h.g.find(root) as usize] else { return };
        if after >= before {
            return;
        }
        let new = rebuild_expr(&h, &best, root, e.span);
        self.st.egraph_exprs += 1;
        for (name, n) in &h.g.fired {
            match self.st.egraph_rules.iter_mut().find(|(r, _)| r == name) {
                Some(x) => x.1 += n,
                None => self.st.egraph_rules.push((name, *n)),
            }
        }
        if self.st.egraph_examples.len() < 12 {
            self.st.egraph_examples.push((e.span, show_expr(self.p, self.func(), &new)));
        }
        *e = new;
    }
}

fn int_bounds(lo: f64, hi: f64) -> Option<(i32, i32)> {
    (lo <= hi && lo >= i32::MIN as f64 && hi <= i32::MAX as f64).then_some((lo as i32, hi as i32))
}

/// does the block change local `i` (other than through the loop step)?
fn writes_local(b: &Block, i: LocalId) -> bool {
    written_locals(b).contains(&i)
}

fn expr_size(e: &Expr) -> usize {
    1 + crate::sema::children(e).into_iter().map(expr_size).sum::<usize>()
}

/// a structural key for atoms that may be shared (same value wherever they appear in one pure
/// expression); None for anything else
fn atom_key(e: &Expr) -> Option<String> {
    fn go(e: &Expr, out: &mut String) -> bool {
        use std::fmt::Write;
        match &e.kind {
            ExprKind::Local(l) => {
                let _ = write!(out, "L{}", l);
            }
            ExprKind::SelfRef => out.push('S'),
            ExprKind::StructSelf => out.push('T'),
            ExprKind::Const(c) => {
                let _ = write!(out, "K{}", c);
            }
            ExprKind::Count(c) => {
                let _ = write!(out, "N{}", c);
            }
            ExprKind::EField { base, field } => {
                let _ = write!(out, "F{}(", field);
                if !go(base, out) {
                    return false;
                }
                out.push(')');
            }
            ExprKind::Field { base, sid, idx } => {
                let _ = write!(out, "M{}.{}(", sid, idx);
                if !go(base, out) {
                    return false;
                }
                out.push(')');
            }
            ExprKind::ListLen(x) => {
                out.push_str("len(");
                if !go(x, out) {
                    return false;
                }
                out.push(')');
            }
            ExprKind::Builtin { f: Builtin::TimeTick, args, .. } if args.is_empty() => out.push_str("tick"),
            _ => return false,
        }
        true
    }
    let mut s = String::new();
    go(e, &mut s).then(|| {
        s.push_str(match e.ty {
            Ty::Bool => ":b",
            _ => ":i",
        });
        s
    })
}

/// the expression for the cheapest node of class `c`
fn rebuild_expr(h: &Hg, best: &[Option<(u32, usize)>], c: eg::Id, sp: crate::diag::Span) -> Expr {
    let g = &h.g;
    let (_, i) = best[g.find(c) as usize].expect("extracted class");
    let n = g.node(i);
    let k = |j: usize| Box::new(rebuild_expr(h, best, n.args[j], sp));
    let bin = |op: BinOp, ty: Ty| Expr::new(ExprKind::Binary(op, k(0), k(1)), ty, sp);
    let int_lit = |v: i64| Expr::new(ExprKind::Int(v), Ty::Int, sp);
    let builtin = |f: Builtin, args: Vec<Expr>| Expr::new(ExprKind::Builtin { f, args, site: 0 }, Ty::Int, sp);
    match n.op {
        Op::Int(v) => int_lit(v as i64),
        Op::Bool(b) => Expr::new(ExprKind::Bool(b), Ty::Bool, sp),
        Op::Atom(a) => h.exprs[a as usize].clone(),
        Op::Add => bin(BinOp::Add, Ty::Int),
        Op::Sub => bin(BinOp::Sub, Ty::Int),
        Op::Mul => bin(BinOp::Mul, Ty::Int),
        Op::Div => bin(BinOp::Div, Ty::Int),
        Op::Rem => bin(BinOp::Rem, Ty::Int),
        Op::Shl => bin(BinOp::Shl, Ty::Int),
        Op::Shr => bin(BinOp::Shr, Ty::Int),
        Op::And => bin(BinOp::BitAnd, Ty::Int),
        Op::Or => bin(BinOp::BitOr, Ty::Int),
        Op::Xor => bin(BinOp::BitXor, Ty::Int),
        Op::Lt => bin(BinOp::Lt, Ty::Bool),
        Op::Le => bin(BinOp::Le, Ty::Bool),
        Op::Gt => bin(BinOp::Gt, Ty::Bool),
        Op::Ge => bin(BinOp::Ge, Ty::Bool),
        Op::Eq | Op::BEq => bin(BinOp::Eq, Ty::Bool),
        Op::Ne | Op::BNe => bin(BinOp::Ne, Ty::Bool),
        Op::LAnd => bin(BinOp::And, Ty::Bool),
        Op::LOr => bin(BinOp::Or, Ty::Bool),
        Op::Neg => Expr::new(ExprKind::Unary(UnOp::Neg, k(0)), Ty::Int, sp),
        Op::BNot => Expr::new(ExprKind::Unary(UnOp::BitNot, k(0)), Ty::Int, sp),
        Op::LNot => Expr::new(ExprKind::Unary(UnOp::Not, k(0)), Ty::Bool, sp),
        Op::Min => builtin(Builtin::MathMin, vec![*k(0), *k(1)]),
        Op::Max => builtin(Builtin::MathMax, vec![*k(0), *k(1)]),
        Op::Abs => builtin(Builtin::MathAbs, vec![*k(0)]),
        Op::Clamp(lo, hi) => builtin(Builtin::MathClamp, vec![*k(0), int_lit(lo as i64), int_lit(hi as i64)]),
        Op::Sel => {
            let ty = if g.data(n.args[1]).boolean { Ty::Bool } else { Ty::Int };
            Expr::new(ExprKind::Cond(k(0), k(1), k(2)), ty, sp)
        }
    }
}

/// a short source-like rendering of a (small) expression, for `sijoc explain`
pub fn show_expr(p: &Program, f: &Func, e: &Expr) -> String {
    let s = |x: &Expr| show_expr(p, f, x);
    match &e.kind {
        ExprKind::Int(n) => n.to_string(),
        ExprKind::Bool(b) => b.to_string(),
        ExprKind::Local(l) => f.locals.get(*l).map(|x| x.name.clone()).unwrap_or_else(|| "?".into()),
        ExprKind::SelfRef | ExprKind::StructSelf => "this".into(),
        ExprKind::EField { base, field } => match base.kind {
            ExprKind::SelfRef => p.fields[*field].name.clone(),
            _ => format!("{}.{}", s(base), p.fields[*field].name),
        },
        ExprKind::Field { base, sid, idx } => format!("{}.{}", s(base), p.structs[*sid].fields[*idx].name),
        ExprKind::Const(c) => p.consts[*c].name.clone(),
        ExprKind::ListLen(x) => format!("{}.Count", s(x)),
        ExprKind::Binary(op, a, b) => format!("({} {} {})", s(a), op.sym(), s(b)),
        ExprKind::Unary(op, x) => {
            let o = match op {
                UnOp::Neg => "-",
                UnOp::Not => "!",
                UnOp::BitNot => "~",
                UnOp::Plus => "+",
            };
            format!("{}{}", o, s(x))
        }
        ExprKind::Cond(c, a, b) => format!("({} ? {} : {})", s(c), s(a), s(b)),
        ExprKind::Builtin { f: Builtin::MathMin, args, .. } => format!("Math.Min({}, {})", s(&args[0]), s(&args[1])),
        ExprKind::Builtin { f: Builtin::MathMax, args, .. } => format!("Math.Max({}, {})", s(&args[0]), s(&args[1])),
        ExprKind::Builtin { f: Builtin::MathAbs, args, .. } => format!("Math.Abs({})", s(&args[0])),
        ExprKind::Builtin { f: Builtin::MathClamp, args, .. } => format!("Math.Clamp({}, {}, {})", s(&args[0]), s(&args[1]), s(&args[2])),
        _ => "…".into(),
    }
}
