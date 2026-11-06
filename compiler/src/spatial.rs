//! Automatic spatial acceleration of proximity queries.
//!
//! Recognizes system loops of the shape
//!
//! ```text
//! foreach (var o in Query<T>()) {
//!     if (o == this) { continue; }        // optional: pure `if (...) { continue; }`
//!     SVec2 d = o.Pos - Pos;              // optional: side-effect-free locals
//!     if (d.LengthSq < R * R) { ... }     // last statement, no else
//! }
//! ```
//!
//! and proves, by symbolically inlining the struct operators/properties involved, that the guard is
//! `sum over components (o.P.c - this.Q.c)^2 < R^2` with a compile-time R (or the square root of
//! that sum compared with R: `(o.Pos - Pos).Length < R`, `V.Distance(o.Pos, Pos) < R`). Entities farther than R
//! on any gridded axis can then never pass the guard, and the statements before it have no effects,
//! so visiting only the entities in the neighbouring cells of a uniform grid — **in the same row
//! order as the full scan** — gives bit-identical results. The grid is a hash grid (collisions only
//! add candidates), rebuilt at the start of the system from the same snapshot the loop reads.

use crate::ast::{BinOp, UnOp};
use crate::ecs::{EcsPlan, SysKind};
use crate::hir::*;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct SpatialLoop {
    /// the function holding the loop: a system, or an entity method a system calls on `this`
    pub func: FuncId,
    /// the systems that reach it (each builds the grid when it starts and drops it when it ends)
    pub systems: Vec<FuncId>,
    /// the `foreach` cursor
    pub var: LocalId,
    pub class: ClassId,
    /// `Query<T, C1, C2>()`: the components every visited entity has (the grid covers only the
    /// tables of those combinations)
    pub with: Vec<ClassId>,
    /// position field of the queried entities and member paths of the gridded axes (1 or 2)
    pub other_field: FieldId,
    pub other_paths: Vec<Vec<usize>>,
    /// position field of `this` and the matching member paths
    pub self_field: FieldId,
    pub self_paths: Vec<Vec<usize>>,
    /// the radius (NaN for a per-entity radius, see dyn_r2)
    pub radius: f64,
    /// per-entity squared radius as a polynomial over `this`'s fields (coefficient, leaves); the
    /// grid's cell then comes from the largest value over the entities that run the loop
    pub dyn_r2: Vec<(f64, Vec<(FieldId, Vec<usize>)>)>,
    /// grid cell edge (slightly larger than the radius; 0 when computed at build time)
    pub cell: f32,
}

#[derive(Default, Debug, Clone)]
pub struct SpatialPlan {
    pub loops: Vec<SpatialLoop>,
    /// why candidate loops were not accelerated (for `sijoc explain` and lint L402): (system
    /// function, loop variable, reason)
    pub rejected: Vec<(FuncId, LocalId, String)>,
}

impl SpatialPlan {
    pub fn find(&self, func: FuncId, var: LocalId) -> Option<(usize, &SpatialLoop)> {
        self.loops.iter().enumerate().find(|(_, l)| l.func == func && l.var == var)
    }
}

/// Proximity loops in entity systems and in the entity methods they call on `this` (a helper's
/// loop reads the snapshot of the system that runs it, so that system builds the grid; anywhere
/// else, e.g. a message applied after the system, the grid is invalid and the loop scans).
pub fn plan(p: &Program, ecs: &EcsPlan) -> SpatialPlan {
    let mut sp = SpatialPlan::default();
    let mut seen = std::collections::HashSet::new();
    let mut done = std::collections::HashSet::new();
    for s in &ecs.systems {
        if s.kind != SysKind::Entity || !seen.insert(s.func) {
            continue;
        }
        for fid in crate::ecs::reachable_funcs(p, s.func) {
            let f = &p.funcs[fid];
            if !matches!(f.kind, FuncKind::EntityMethod(_)) || f.ctx != Ctx::Server {
                continue;
            }
            if done.insert(fid) {
                find_loops(p, fid, f, &f.body, &mut sp);
            }
            for l in sp.loops.iter_mut().filter(|l| l.func == fid) {
                if !l.systems.contains(&s.func) {
                    l.systems.push(s.func);
                }
            }
        }
    }
    // a per-entity radius sizes the grid when the system starts: it must not change while it runs
    let mut keep = vec![];
    for l in std::mem::take(&mut sp.loops) {
        let written = l.dyn_r2.iter().flat_map(|(_, ls)| ls.iter().map(|(f, _)| *f)).find(|f| {
            ecs.systems.iter().any(|s| l.systems.contains(&s.func) && s.eff.self_writes.contains(f))
        });
        match written {
            Some(f) => sp.rejected.push((
                l.func,
                l.var,
                format!("the radius uses '{}', which the system running the loop writes (the grid is sized when the system starts): add a constant bound, e.g. `d < MAX_R * MAX_R && ...`", p.field_label(f)),
            )),
            None => keep.push(l),
        }
    }
    sp.loops = keep;
    sp
}

fn find_loops(p: &Program, fid: FuncId, f: &Func, b: &Block, sp: &mut SpatialPlan) {
    for s in &b.stmts {
        if let Stmt::ForQuery { var, class, body, with, .. } = s {
            match analyze(p, f, *var, *class, body) {
                Ok(mut l) => {
                    l.func = fid;
                    // Query<T, C>(): a grid per table of the combinations that have C
                    l.with = with.clone();
                    sp.loops.push(l);
                }
                Err(why) => {
                    if let Some(why) = why.or_else(|| diagnose(p, *var, body)) {
                        sp.rejected.push((fid, *var, why));
                    }
                }
            }
        }
        for x in s.sub_blocks() {
            find_loops(p, fid, f, x, sp);
        }
    }
}

// ---------------------------------------------------------------------------
// symbolic values: polynomials over position leaves
// ---------------------------------------------------------------------------

/// (belongs to the loop cursor?, field, member path)
type Leaf = (bool, FieldId, Vec<usize>);
type Mono = Vec<Leaf>;
type Poly = BTreeMap<Mono, f64>;

#[derive(Clone, Debug)]
enum SymV {
    Num(Poly),
    Rec(Vec<SymV>),
    /// Math.Sqrt of a polynomial (a distance written with Length / Distance): only comparable
    Sqrt(Poly),
}

fn konst(c: f64) -> Poly {
    let mut m = Poly::new();
    if c != 0.0 {
        m.insert(vec![], c);
    }
    m
}
fn padd(a: &Poly, b: &Poly, sign: f64) -> Poly {
    let mut r = a.clone();
    for (k, v) in b {
        *r.entry(k.clone()).or_insert(0.0) += sign * v;
    }
    r.retain(|_, v| *v != 0.0);
    r
}
fn pmul(a: &Poly, b: &Poly) -> Option<Poly> {
    let mut r = Poly::new();
    for (ka, va) in a {
        for (kb, vb) in b {
            let mut k = ka.clone();
            k.extend(kb.iter().cloned());
            if k.len() > 2 {
                return None;
            }
            k.sort();
            *r.entry(k).or_insert(0.0) += va * vb;
        }
    }
    r.retain(|_, v| *v != 0.0);
    Some(r)
}

struct Sym<'a> {
    p: &'a Program,
    var: LocalId,
    env: BTreeMap<LocalId, SymV>,
    depth: usize,
}

impl<'a> Sym<'a> {
    fn leaf(&self, other: bool, field: FieldId, ty: &Ty, path: Vec<usize>) -> Option<SymV> {
        match ty {
            Ty::Float | Ty::Int => {
                let mut m = Poly::new();
                m.insert(vec![(other, field, path)], 1.0);
                Some(SymV::Num(m))
            }
            Ty::Struct(s) => {
                let mut v = vec![];
                for (i, m) in self.p.structs[*s].fields.iter().enumerate() {
                    let mut pp = path.clone();
                    pp.push(i);
                    v.push(self.leaf(other, field, &m.ty, pp)?);
                }
                Some(SymV::Rec(v))
            }
            _ => None,
        }
    }

    fn cval(&self, v: &ConstVal) -> Option<SymV> {
        Some(match v {
            ConstVal::Int(n) => SymV::Num(konst(*n as f64)),
            ConstVal::Float(f) => SymV::Num(konst(*f as f64)),
            ConstVal::Struct(_, vals) => SymV::Rec(vals.iter().map(|x| self.cval(x)).collect::<Option<Vec<_>>>()?),
            _ => return None,
        })
    }

    fn num(&mut self, e: &Expr, this: &Option<SymV>) -> Option<Poly> {
        match self.eval(e, this)? {
            SymV::Num(p) => Some(p),
            _ => None,
        }
    }

    fn eval(&mut self, e: &Expr, this: &Option<SymV>) -> Option<SymV> {
        self.depth += 1;
        if self.depth > 200 {
            return None;
        }
        let r = self.eval_inner(e, this);
        self.depth -= 1;
        r
    }

    fn eval_inner(&mut self, e: &Expr, this: &Option<SymV>) -> Option<SymV> {
        Some(match &e.kind {
            ExprKind::Float(f) => SymV::Num(konst(*f)),
            ExprKind::Int(n) => SymV::Num(konst(*n as f64)),
            ExprKind::Const(k) => self.cval(self.p.consts[*k].value.as_ref()?)?,
            ExprKind::Local(l) => self.env.get(l)?.clone(),
            ExprKind::StructSelf => this.clone()?,
            ExprKind::EField { base, field } => {
                let ty = &self.p.fields[*field].ty;
                match &base.kind {
                    ExprKind::Local(l) if *l == self.var => self.leaf(true, *field, ty, vec![])?,
                    ExprKind::SelfRef => self.leaf(false, *field, ty, vec![])?,
                    _ => return None,
                }
            }
            ExprKind::Field { base, idx, .. } => match self.eval(base, this)? {
                SymV::Rec(v) => v.get(*idx)?.clone(),
                _ => return None,
            },
            ExprKind::Unary(UnOp::Neg, x) => SymV::Num(padd(&Poly::new(), &self.num(x, this)?, -1.0)),
            ExprKind::Unary(UnOp::Plus, x) => self.eval(x, this)?,
            ExprKind::Binary(op @ (BinOp::Add | BinOp::Sub | BinOp::Mul), a, b) => {
                let (x, y) = (self.num(a, this)?, self.num(b, this)?);
                SymV::Num(match op {
                    BinOp::Add => padd(&x, &y, 1.0),
                    BinOp::Sub => padd(&x, &y, -1.0),
                    _ => pmul(&x, &y)?,
                })
            }
            ExprKind::OpCall { func, args } => {
                let av: Vec<SymV> = args.iter().map(|a| self.eval(a, this)).collect::<Option<_>>()?;
                self.inline(*func, None, av)?
            }
            ExprKind::Builtin { f: Builtin::MathSqrt, args, .. } if args.len() == 1 => SymV::Sqrt(self.num(&args[0], this)?),
            ExprKind::Call { func, recv, args, dispatch: Dispatch::Static } => {
                let rv = match recv {
                    Some(r) => Some(self.eval(r, this)?),
                    None => None,
                };
                let av: Vec<SymV> = args.iter().map(|a| self.eval(&a.expr, this)).collect::<Option<_>>()?;
                self.inline(*func, rv, av)?
            }
            ExprKind::StructLit { sid, ctor, args, inits } => {
                let mut rec = match crate::consteval::struct_default(self.p, *sid) {
                    Ok(v) => self.cval(&v)?,
                    Err(_) => return None,
                };
                if let Some(c) = ctor {
                    let av: Vec<SymV> = args.iter().map(|a| self.eval(&a.expr, this)).collect::<Option<_>>()?;
                    rec = self.run_ctor(*c, rec, av)?;
                }
                for (idx, x) in inits {
                    let v = self.eval(x, this)?;
                    if let SymV::Rec(fs) = &mut rec {
                        *fs.get_mut(*idx)? = v;
                    }
                }
                rec
            }
            _ => return None,
        })
    }

    /// `T F(params) => expr;` struct operators, properties and methods
    fn inline(&mut self, f: FuncId, recv: Option<SymV>, args: Vec<SymV>) -> Option<SymV> {
        let func = &self.p.funcs[f];
        if func.writes_self || func.params.len() != args.len() {
            return None;
        }
        let [Stmt::Return(Some(body), _)] = func.body.stmts.as_slice() else { return None };
        let saved = std::mem::take(&mut self.env);
        let saved_var = self.var;
        self.var = usize::MAX; // the callee cannot see the cursor
        for (i, a) in args.into_iter().enumerate() {
            self.env.insert(func.params[i], a);
        }
        let r = self.eval(body, &recv);
        self.env = saved;
        self.var = saved_var;
        r
    }

    /// constructors made of `Field = expr;` assignments
    fn run_ctor(&mut self, c: FuncId, mut rec: SymV, args: Vec<SymV>) -> Option<SymV> {
        let func = &self.p.funcs[c];
        if func.params.len() != args.len() {
            return None;
        }
        let saved = std::mem::take(&mut self.env);
        let saved_var = self.var;
        self.var = usize::MAX;
        for (i, a) in args.into_iter().enumerate() {
            self.env.insert(func.params[i], a);
        }
        let mut ok = true;
        for s in &func.body.stmts {
            match s {
                Stmt::Expr(Expr { kind: ExprKind::Assign { op: None, target, value }, .. }) => match &target.kind {
                    ExprKind::Field { base, idx, .. } if matches!(base.kind, ExprKind::StructSelf) => match self.eval(value, &None) {
                        Some(v) => {
                            if let SymV::Rec(fs) = &mut rec {
                                if let Some(slot) = fs.get_mut(*idx) {
                                    *slot = v;
                                    continue;
                                }
                            }
                            ok = false;
                        }
                        None => ok = false,
                    },
                    _ => ok = false,
                },
                _ => ok = false,
            }
            if !ok {
                break;
            }
        }
        self.env = saved;
        self.var = saved_var;
        if ok {
            Some(rec)
        } else {
            None
        }
    }
}

/// expressions safe to evaluate for entities the grid skips: no effects, cannot panic.
/// Either plain arithmetic over fields through inlinable struct helpers (symbolically evaluable),
/// or a pure expression without calls, indexing or builtins.
fn harmless(sym: &mut Sym, e: &Expr) -> bool {
    if sym.eval(e, &None).is_some() {
        return true;
    }
    if !crate::opt::is_pure_expr(e) {
        return false;
    }
    let mut ok = true;
    crate::sema::visit_expr(e, &mut |x| {
        if matches!(x.kind, ExprKind::ListIndex { .. } | ExprKind::DictGet { .. } | ExprKind::Builtin { .. } | ExprKind::Call { .. } | ExprKind::OpCall { .. }) {
            ok = false;
        }
    });
    ok
}

/// the bound of a distance guard: a constant, or a polynomial over `this`'s fields
#[derive(Clone, Debug)]
enum R2 {
    Const(f64),
    Selfp(Poly),
}

/// `D < R2`, `D <= R2`, `R2 > D`, `R2 >= D` with a constant R2 (or, with `dynamic`, one computed
/// from `this`'s fields): (D as a polynomial, R2)
fn guard_near(sym: &mut Sym, c: &Expr, dynamic: bool) -> Option<(Poly, R2)> {
    let ExprKind::Binary(op, a, b) = &c.kind else { return None };
    let (d, r) = match op {
        BinOp::Lt | BinOp::Le => (a, b),
        BinOp::Gt | BinOp::Ge => (b, a),
        _ => return None,
    };
    let r2 = match &r.kind {
        ExprKind::Float(x) => R2::Const(*x),
        ExprKind::Int(n) => R2::Const(*n as f64),
        _ if dynamic => {
            let rp = sym.num(r, &None)?;
            if rp.is_empty() || !rp.keys().all(|m| m.iter().all(|l| !l.0)) {
                return None;
            }
            R2::Selfp(rp)
        }
        _ => return None,
    };
    match sym.eval(d, &None)? {
        SymV::Num(poly) => Some((poly, r2)),
        // Sqrt(D) < R: D < R^2 up to rounding, which the grid's cell margin covers (Sqrt is
        // monotonic and correctly rounded); the loop still evaluates the guard as written
        SymV::Sqrt(poly) => {
            let r2 = match r2 {
                R2::Const(x) if x >= 0.0 => R2::Const(x * x),
                R2::Const(_) => return None,
                R2::Selfp(rp) => R2::Selfp(pmul(&rp, &rp)?),
            };
            Some((poly, r2))
        }
        SymV::Rec(_) => None,
    }
}

/// the negation of a near guard: `D >= R2`, `D > R2`, `R2 <= D`, `R2 < D`, `!(D < R2)`
fn guard_far(sym: &mut Sym, c: &Expr) -> Option<(Poly, R2)> {
    match &c.kind {
        ExprKind::Unary(crate::ast::UnOp::Not, inner) => guard_near(sym, inner, true),
        ExprKind::Binary(op @ (BinOp::Ge | BinOp::Gt | BinOp::Le | BinOp::Lt), a, b) => {
            let flipped = match op {
                BinOp::Ge => BinOp::Lt,
                BinOp::Gt => BinOp::Le,
                BinOp::Le => BinOp::Gt,
                _ => BinOp::Ge,
            };
            let near = Expr::new(ExprKind::Binary(flipped, a.clone(), b.clone()), Ty::Bool, c.span);
            guard_near(sym, &near, true)
        }
        _ => None,
    }
}

fn split_or<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match &e.kind {
        ExprKind::Binary(BinOp::Or, a, b) => {
            split_or(a, out);
            split_or(b, out);
        }
        _ => out.push(e),
    }
}

/// a reason for `sijoc explain` / the linter when the loop compares Sqrt(distance) with a radius
fn uses_sqrt_distance(body: &Block) -> Option<String> {
    let mut sqrt_cmp = false;
    crate::sema::visit_block(body, &mut |e| {
        if let ExprKind::Binary(BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, a, b) = &e.kind {
            for x in [a, b] {
                if matches!(x.kind, ExprKind::Builtin { f: Builtin::MathSqrt, .. }) {
                    sqrt_cmp = true;
                }
            }
        }
    });
    if sqrt_cmp {
        Some("the distance test uses Sqrt: compare squared distances instead ((o.Pos - Pos).LengthSq < R * R)".into())
    } else {
        None
    }
}

/// Why a loop with a distance test is not accelerated, when `analyze` gave no specific reason: a
/// squared distance compared with something that is not a constant, or a distance test in the
/// wrong place.
fn diagnose(p: &Program, var: LocalId, body: &Block) -> Option<String> {
    let mut sym = Sym { p, var, env: BTreeMap::new(), depth: 0 };
    let mut distance_vs_const = false;
    let mut distance_vs_other = false;
    let mut cmps: Vec<(Expr, Expr)> = vec![];
    crate::sema::visit_block(body, &mut |e| {
        if let ExprKind::Binary(BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge, a, b) = &e.kind {
            cmps.push(((**a).clone(), (**b).clone()));
        }
    });
    for (a, b) in &cmps {
        for (d, r) in [(a, b), (b, a)] {
            let Some(poly) = sym.num(d, &None) else { continue };
            // a squared coordinate of the queried entity: looks like a distance
            let distance = poly.keys().any(|m| matches!(m.as_slice(), [x, y] if x == y && x.0));
            if !distance {
                continue;
            }
            if matches!(r.kind, ExprKind::Float(_) | ExprKind::Int(_)) {
                distance_vs_const = true;
            } else {
                distance_vs_other = true;
            }
        }
    }
    if distance_vs_other && !distance_vs_const {
        return Some("the radius of the distance test depends on the other entity or on values computed in the loop, not only on a constant or this entity's fields: add a constant bound the grid can use, e.g. `if (d < MAX_R * MAX_R && d < r * r)`".into());
    }
    if distance_vs_const {
        return Some("the distance test must be the last statement (`if (d < R * R) { ... }`) or an early `if (d >= R * R) { continue; }`, and only side-effect-free statements may come before it (they run for every entity in a full scan)".into());
    }
    None
}

/// Ok(loop) if accelerable; Err(Some(reason)) for loops that look like proximity queries but are not provably so
fn analyze(p: &Program, f: &Func, var: LocalId, class: ClassId, body: &Block) -> Result<SpatialLoop, Option<String>> {
    let mut sym = Sym { p, var, env: BTreeMap::new(), depth: 0 };
    // The body must reach its effects only for neighbours: either it ends with `if (D < R2) {...}`,
    // or an early `if (D >= R2) continue;` comes first (the statements after it then run only for
    // neighbours). Everything before the guard must be side-effect free, because only grid
    // candidates are visited.
    let mut found: Option<(Poly, R2)> = None;
    for (i, s) in body.stmts.iter().enumerate() {
        let last = i + 1 == body.stmts.len();
        match s {
            Stmt::If { cond, then, els: None } if matches!(then.stmts.as_slice(), [Stmt::Continue]) => {
                // `if (A || D >= R2 || B) continue;`: a far-away disjunct proves the rest of the body.
                // Disjuncts before it run for every entity in a full scan, so they must be harmless;
                // the ones after it run only for neighbours (short-circuit), which the grid visits too.
                let mut disj = vec![];
                split_or(cond, &mut disj);
                let mut guard = None;
                for c in &disj {
                    if let Some(g) = guard_far(&mut sym, c) {
                        guard = Some(g);
                        break;
                    }
                    if !harmless(&mut sym, c) {
                        return Err(uses_sqrt_distance(body));
                    }
                }
                if guard.is_some() {
                    found = guard;
                    break;
                }
                // a harmless early-out (e.g. `if (o == this) continue;`): part of the prefix
            }
            Stmt::If { cond, els: None, .. } if last => {
                // `if (D < R2 && ...) { ... }`
                let mut conj = vec![];
                split_and(cond, &mut conj);
                // a constant bound if there is one, else one computed from this entity's fields
                found = conj.iter().find_map(|c| guard_near(&mut sym, c, false)).or_else(|| conj.iter().find_map(|c| guard_near(&mut sym, c, true)));
                break;
            }
            Stmt::Let { local, init: Some(e) } if harmless(&mut sym, e) => {
                if let Some(v) = sym.eval(e, &None) {
                    sym.env.insert(*local, v);
                }
            }
            _ => return Err(uses_sqrt_distance(body)),
        }
    }
    let Some((poly, bound)) = found else { return Err(uses_sqrt_distance(body)) };
    let (r2, dyn_r2) = match bound {
        R2::Const(v) => (v, vec![]),
        R2::Selfp(rp) => (f64::NAN, rp.into_iter().map(|(m, c)| (c, m.into_iter().map(|l| (l.1, l.2)).collect())).collect()),
    };
    // D must be exactly sum_c (o.F.c - this.G.c)^2
    let mut pairs: Vec<(Leaf, Leaf)> = vec![];
    for mono in poly.keys() {
        if let [x, y] = mono.as_slice() {
            if x == y && x.0 {
                let mate = poly.keys().find_map(|m| match m.as_slice() {
                    [a, b] if a == b && !a.0 && a.2 == x.2 => Some(a.clone()),
                    _ => None,
                });
                match mate {
                    Some(s) => pairs.push((x.clone(), s)),
                    None => return Err(Some("the guard is not a distance between this entity and the queried one".into())),
                }
            }
        }
    }
    if pairs.is_empty() {
        return Err(None);
    }
    let of = pairs[0].0 .1;
    let sf = pairs[0].1 .1;
    if pairs.iter().any(|(o, s)| o.1 != of || s.1 != sf) {
        return Err(Some("the distance mixes several position fields".into()));
    }
    let mut expect = Poly::new();
    for (o, s) in &pairs {
        let mut diff = Poly::new();
        diff.insert(vec![o.clone()], 1.0);
        diff.insert(vec![s.clone()], -1.0);
        expect = padd(&expect, &pmul(&diff, &diff).unwrap(), 1.0);
    }
    let same = expect.len() == poly.len() && expect.iter().all(|(k, v)| poly.get(k).map(|w| (w - v).abs() < 1e-12).unwrap_or(false));
    if !same {
        return Err(Some("the guard is not exactly a squared distance".into()));
    }
    if dyn_r2.is_empty() && (r2 < 0.0 || !r2.is_finite()) {
        return Err(None);
    }
    // gridded axes: float components only
    pairs.sort_by(|a, b| a.0 .2.cmp(&b.0 .2));
    let float_leaf = |l: &Leaf| leaf_ty(p, l) == Some(Ty::Float);
    let axes: Vec<&(Leaf, Leaf)> = pairs.iter().filter(|(o, s)| float_leaf(o) && float_leaf(s)).take(2).collect();
    if axes.is_empty() {
        return Err(Some("positions are not float vectors".into()));
    }
    // the loop must not move `this` while it runs (the query uses its position at loop entry)
    let mut moves_self = false;
    crate::sema::visit_block(body, &mut |e| match &e.kind {
        ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => {
            if let Some((fld, true)) = crate::effects::target_field(target) {
                if fld == sf {
                    moves_self = true;
                }
            }
        }
        ExprKind::Call { args, dispatch, .. } => {
            if matches!(dispatch, Dispatch::SelfCall | Dispatch::BaseCall) {
                moves_self = true;
            }
            for a in args {
                if matches!(a.mode, crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out) {
                    if let Some((fld, true)) = crate::effects::target_field(&a.expr) {
                        if fld == sf {
                            moves_self = true;
                        }
                    }
                }
            }
        }
        _ => {}
    });
    if moves_self {
        return Err(Some("the loop body may change this entity's position".into()));
    }
    let _ = f;
    let radius = r2.sqrt();
    let cell = if dyn_r2.is_empty() { ((radius * 1.001) + 1e-4).max(1e-3) as f32 } else { 0.0 };
    Ok(SpatialLoop {
        func: 0,
        systems: vec![],
        var,
        class,
        with: vec![],
        other_field: of,
        other_paths: axes.iter().map(|(o, _)| o.2.clone()).collect(),
        self_field: sf,
        self_paths: axes.iter().map(|(_, s)| s.2.clone()).collect(),
        radius,
        dyn_r2,
        cell,
    })
}

fn leaf_ty(p: &Program, l: &Leaf) -> Option<Ty> {
    let mut t = p.fields[l.1].ty.clone();
    for &i in &l.2 {
        let Ty::Struct(s) = t else { return None };
        t = p.structs[s].fields.get(i)?.ty.clone();
    }
    Some(t)
}

fn split_and<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match &e.kind {
        ExprKind::Binary(BinOp::And, a, b) => {
            split_and(a, out);
            split_and(b, out);
        }
        _ => out.push(e),
    }
}
