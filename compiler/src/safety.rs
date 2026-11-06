//! Ownership & aliasing checks on HIR:
//!  * law of exclusivity at call sites (E0402)
//!  * list moves: use-after-move (E0501), moving out of fields (E0502) or borrowed params (E0504)
//!  * mutating a list while iterating it (E0406)

use crate::ast::ParamMode;
use crate::diag::{Diags, Span};
use crate::effects::EffectInfo;
use crate::hir::*;
use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq)]
enum Root {
    Local(LocalId),
    SelfField(FieldId),
    StructSelf,
}

#[derive(Clone, Debug, PartialEq)]
enum Proj {
    Field(usize),
    Index,
}

#[derive(Clone, Debug)]
struct Path {
    root: Root,
    proj: Vec<Proj>,
}

fn path_of(e: &Expr) -> Option<Path> {
    match &e.kind {
        ExprKind::Local(l) => Some(Path { root: Root::Local(*l), proj: vec![] }),
        ExprKind::StructSelf => Some(Path { root: Root::StructSelf, proj: vec![] }),
        ExprKind::EField { base, field } if matches!(base.kind, ExprKind::SelfRef) => Some(Path { root: Root::SelfField(*field), proj: vec![] }),
        ExprKind::Field { base, idx, .. } => {
            let mut p = path_of(base)?;
            p.proj.push(Proj::Field(*idx));
            Some(p)
        }
        ExprKind::ListIndex { list, .. } => {
            let mut p = path_of(list)?;
            p.proj.push(Proj::Index);
            Some(p)
        }
        _ => None,
    }
}

fn overlaps(a: &Path, b: &Path) -> bool {
    a.root == b.root && a.proj.iter().zip(&b.proj).all(|(x, y)| x == y)
}

fn describe(p: &Program, f: &Func, e: &Expr) -> String {
    match &e.kind {
        ExprKind::Local(l) => f.locals[*l].name.clone(),
        ExprKind::EField { field, base } => match hoisted_local(f, *field) {
            Some(l) if matches!(base.kind, ExprKind::SelfRef) => f.locals[l].name.clone(),
            _ => p.fields[*field].name.clone(),
        },
        ExprKind::Field { base, sid, idx } => format!("{}.{}", describe(p, f, base), p.structs[*sid].fields[*idx].name),
        ExprKind::StructSelf => "this".into(),
        ExprKind::ListIndex { list, .. } => format!("{}[..]", describe(p, f, list)),
        _ => "expression".into(),
    }
}

pub fn check(p: &Program, info: &EffectInfo, d: &mut Diags) {
    for f in &p.funcs {
        // exclusivity
        crate::sema::visit_block(&f.body, &mut |e| {
            if let ExprKind::Call { func, recv, args, .. } = &e.kind {
                let mut excl: Vec<(Path, &Expr)> = vec![];
                let mut shared: Vec<(Path, &Expr)> = vec![];
                if let Some(r) = recv {
                    if let Some(pp) = path_of(r) {
                        if p.funcs[*func].writes_self {
                            excl.push((pp, r));
                        } else {
                            shared.push((pp, r));
                        }
                    }
                }
                let mut moved: Vec<(Path, &Expr)> = vec![];
                for a in args {
                    if let Some(pp) = path_of(&a.expr) {
                        match a.mode {
                            ParamMode::Ref | ParamMode::Out => excl.push((pp, &a.expr)),
                            // an `own` argument leaves the caller: nothing else in the call may refer to it
                            ParamMode::Own => moved.push((pp, &a.expr)),
                            _ => {
                                if matches!(a.expr.ty, Ty::Struct(_) | Ty::List(_) | Ty::Array(_) | Ty::MdArray(..) | Ty::Dict(..)) {
                                    shared.push((pp, &a.expr));
                                }
                            }
                        }
                    }
                }
                for (mp, me) in &moved {
                    for (op, oe) in shared.iter().chain(excl.iter()) {
                        if overlaps(mp, op) || overlaps(op, mp) {
                            d.error_note(
                                "E0402",
                                oe.span,
                                format!("'{}' is moved into this call by an 'own' argument and also passed to it", describe(p, f, me)),
                                "the callee owns the moved collection and may change or free it while it reads the other argument: pass a copy (.Clone())",
                            );
                        }
                    }
                }
                // a borrowed collection of this entity that the callee changes: the borrow would see
                // (or keep a pointer into) storage the call reallocates or frees
                if matches!(e.kind, ExprKind::Call { dispatch: Dispatch::SelfCall | Dispatch::BaseCall, .. }) {
                    for (sp, se) in &shared {
                        if let (Root::SelfField(fld), true) = (&sp.root, se.ty.is_list()) {
                            if call_writes_self_field(p, info, *func, *fld) {
                                d.error_note(
                                    "E0402",
                                    se.span,
                                    format!("'{}' is lent to '{}', which changes it", p.fields[*fld].name, p.funcs[*func].name),
                                    format!("the call would read the field while it replaces or changes it: pass a copy ({}.Clone()) or let '{}' read the field itself", p.fields[*fld].name, p.funcs[*func].name),
                                );
                            }
                        }
                    }
                }
                for i in 0..excl.len() {
                    for j in 0..excl.len() {
                        if i < j && overlaps(&excl[i].0, &excl[j].0) {
                            d.error_note(
                                "E0402",
                                excl[j].1.span,
                                format!("'{}' is passed as an exclusive (ref) argument while also being borrowed exclusively", describe(p, f, excl[j].1)),
                                "two mutable references to the same storage would alias",
                            );
                        }
                    }
                    for s in &shared {
                        if overlaps(&excl[i].0, &s.0) {
                            d.error_note(
                                "E0402",
                                excl[i].1.span,
                                format!("'{}' is borrowed mutably and immutably in the same call", describe(p, f, excl[i].1)),
                                "a 'ref' argument may not overlap any other argument",
                            );
                        }
                    }
                }
            }
        });
        // move sources must be owned locals or fresh values
        crate::sema::visit_block(&f.body, &mut |e| {
            let check_src = |src: &Expr, d: &mut Diags| check_move_source(p, f, src, d);
            match &e.kind {
                ExprKind::Assign { op: None, value, .. } if value.ty.is_list() => check_src(value, d),
                ExprKind::Spawn { inits, .. } => {
                    for (_, x) in inits {
                        if x.ty.is_list() {
                            check_src(x, d);
                        }
                    }
                }
                ExprKind::ListCall { op: ListOp::Add | ListOp::Insert, args, .. } => {
                    for x in args {
                        if x.ty.is_list() {
                            check_src(x, d);
                        }
                    }
                }
                ExprKind::ArrayNew { elems, .. } => {
                    for x in elems {
                        if x.ty.is_list() {
                            check_src(x, d);
                        }
                    }
                }
                ExprKind::Call { args, .. } => {
                    for a in args {
                        if a.mode == ParamMode::Own {
                            check_src(&a.expr, d);
                        }
                    }
                }
                _ => {}
            }
        });
        check_lets_and_returns(p, f, &f.body, d);
        // use after move
        let mut mv = Mv { f, d: &mut *d, reported: HashSet::new(), breaks: vec![], conts: vec![], fails: vec![], lams: vec![] };
        mv.block(&f.body, Some(HashSet::new()));
        // iteration invalidation (also: a lambda body must not change the list it runs over)
        iter_block(p, info, f, &f.body, &mut vec![], d);
        let mut lambdas: Vec<(Path, Span, Expr)> = vec![];
        crate::sema::visit_block(&f.body, &mut |e| {
            if let ExprKind::ListLambda { list, body, .. } = &e.kind {
                if let Some(pp) = path_of(list) {
                    lambdas.push((pp, list.span, (**body).clone()));
                }
            }
        });
        for (pp, sp, body) in lambdas {
            let b = Block { stmts: vec![Stmt::Expr(body)] };
            iter_block(p, info, f, &b, &mut vec![(pp, sp)], d);
        }
        definite_assignment(f, d);
    }
}

fn check_lets_and_returns(p: &Program, f: &Func, b: &Block, d: &mut Diags) {
    for s in &b.stmts {
        match s {
            Stmt::Let { init: Some(e), .. } | Stmt::Return(Some(e), _) if e.ty.is_list() => check_move_source(p, f, e, d),
            Stmt::If { then, els, .. } => {
                check_lets_and_returns(p, f, then, d);
                if let Some(e) = els {
                    check_lets_and_returns(p, f, e, d);
                }
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } => {
                check_lets_and_returns(p, f, body, d)
            }
            Stmt::Switch { sections, default, .. } => {
                for (_, x) in sections {
                    check_lets_and_returns(p, f, x, d);
                }
                if let Some(x) = default {
                    check_lets_and_returns(p, f, x, d);
                }
            }
            _ => {}
        }
    }
}

fn check_move_source(p: &Program, f: &Func, src: &Expr, d: &mut Diags) {
    match &src.kind {
        ExprKind::ListNew { .. } | ExprKind::ArrayNew { .. } | ExprKind::ListClone { .. } | ExprKind::Call { .. } => {}
        ExprKind::ListLambda { op, .. } if op.makes_list() => {}
        ExprKind::Local(l) => match f.locals[*l].kind {
            LocalKind::Normal | LocalKind::Param(ParamMode::Own) => {}
            _ => d.error_note(
                "E0504",
                src.span,
                format!("cannot move '{}': it is borrowed, not owned", f.locals[*l].name),
                format!("use '{}.Clone()' to make an owned copy", f.locals[*l].name),
            ),
        },
        // a coroutine local kept in a hidden field across awaits is still the method's own
        ExprKind::EField { field, base } if matches!(base.kind, ExprKind::SelfRef) && hoisted_local(f, *field).is_some() => {}
        ExprKind::EField { field, .. } => d.error_note(
            "E0502",
            src.span,
            format!("cannot move list field '{}' out of its entity", p.fields[*field].name),
            format!("use '{}.Clone()' to copy it", p.fields[*field].name),
        ),
        _ => d.error("E0502", src.span, "cannot move this list value; use .Clone()"),
    }
}

/// The local an expression names: a local, or a coroutine local that lives in a hidden entity
/// field across awaits (sema rewrote its uses to `this.__M_x`; it still moves like a local).
fn local_of(f: &Func, e: &Expr) -> Option<LocalId> {
    match &e.kind {
        ExprKind::Local(l) => Some(*l),
        ExprKind::EField { base, field } if matches!(base.kind, ExprKind::SelfRef) => hoisted_local(f, *field),
        _ => None,
    }
}

/// the coroutine local kept in the hidden field `field`
fn hoisted_local(f: &Func, field: FieldId) -> Option<LocalId> {
    f.co.as_ref()?.hoisted.iter().find(|(_, x)| *x == field).map(|(l, _)| *l)
}

/// Use after move: a forward flow analysis over the locals that may have been moved on some path
/// to a point. `None` is an unreachable point (after return / break / continue / fail): what was
/// moved on a path that never falls through does not reach the code after it.
type Moved = Option<HashSet<LocalId>>;

fn either(a: Moved, b: Moved) -> Moved {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(mut x), Some(y)) => {
            x.extend(y);
            Some(x)
        }
    }
}

/// a lambda body being checked (it runs once per element)
struct Lam {
    /// the local whose elements the lambda runs over
    over: Option<LocalId>,
    /// locals given a new value earlier in the same run of the body (moving those is fine)
    fresh: HashSet<LocalId>,
}

struct Mv<'a> {
    f: &'a Func,
    d: &'a mut Diags,
    reported: HashSet<LocalId>,
    /// states at the `break`s of the innermost loop or switch, the `continue`s of the innermost
    /// loop and the `fail`s of the enclosing atomic block
    breaks: Vec<HashSet<LocalId>>,
    conts: Vec<HashSet<LocalId>>,
    fails: Vec<HashSet<LocalId>>,
    /// enclosing lambda bodies, innermost last
    lams: Vec<Lam>,
}

impl<'a> Mv<'a> {
    fn name(&self, l: LocalId) -> &str {
        &self.f.locals[l].name
    }

    /// the local whose storage holds the collection `e` (`xs`, `xs[i]`), if any
    fn root_local(&self, e: &Expr) -> Option<LocalId> {
        match path_of(e)?.root {
            Root::Local(l) => Some(l),
            Root::SelfField(fld) => hoisted_local(self.f, fld),
            Root::StructSelf => None,
        }
    }

    /// a read of `l`
    fn read(&mut self, l: LocalId, sp: Span, st: &HashSet<LocalId>) {
        if st.contains(&l) && self.reported.insert(l) {
            let n = self.name(l).to_string();
            self.d.error_note(
                "E0501",
                sp,
                format!("use of moved list '{}'", n),
                format!("its buffer was moved earlier; write '... = {}.Clone()' at the move if you need both", n),
            );
        }
    }

    /// `x` is a value in a move position: a list local named there is moved from now on
    fn take(&mut self, x: &Expr, st: &mut HashSet<LocalId>) {
        let Some(l) = local_of(self.f, x).filter(|_| x.ty.is_list()) else { return self.expr(x, st) };
        self.read(l, x.span, st);
        if let Some(lam) = self.lams.last() {
            let n = self.name(l).to_string();
            if self.lams.iter().any(|m| m.over == Some(l)) {
                if self.reported.insert(l) {
                    self.d.error_note(
                        "E0406",
                        x.span,
                        format!("'{}' is moved inside a lambda that iterates over it", n),
                        format!("the lambda reads the next elements of '{}' after this call takes the list: pass a copy ('{}.Clone()')", n, n),
                    );
                }
            } else if !lam.fresh.contains(&l) && self.reported.insert(l) {
                self.d.error_note(
                    "E0501",
                    x.span,
                    format!("'{}' is moved inside a lambda that runs for every element", n),
                    format!("the first element moves the list and the next ones would use it after the move: pass '{}.Clone()', or move it once outside the lambda", n),
                );
            }
        }
        st.insert(l);
    }

    /// `l` gets a new value
    fn assign(&mut self, l: LocalId, st: &mut HashSet<LocalId>) {
        st.remove(&l);
        if let Some(lam) = self.lams.last_mut() {
            lam.fresh.insert(l);
        }
    }

    /// what the innermost lambda body assigned so far (restored after a part that may not run)
    fn fresh_save(&self) -> Option<HashSet<LocalId>> {
        self.lams.last().map(|l| l.fresh.clone())
    }

    fn fresh_load(&mut self, s: Option<HashSet<LocalId>>) {
        if let (Some(l), Some(s)) = (self.lams.last_mut(), s) {
            l.fresh = s;
        }
    }

    fn expr(&mut self, e: &Expr, st: &mut HashSet<LocalId>) {
        if let Some(l) = local_of(self.f, e) {
            return self.read(l, e.span, st);
        }
        match &e.kind {
            ExprKind::Assign { op: None, target, value } if value.ty.is_list() => {
                self.take(value, st);
                match local_of(self.f, target) {
                    Some(l) => self.assign(l, st),
                    None => self.expr(target, st),
                }
            }
            ExprKind::Spawn { inits, .. } => {
                for (_, x) in inits {
                    self.take(x, st);
                }
            }
            // xs.Add(list) / xs.Insert(i, list): the list moves into the element
            ExprKind::ListCall { list, op: ListOp::Add | ListOp::Insert, args } => {
                self.expr(list, st);
                for x in args {
                    self.take(x, st);
                }
            }
            // new T[][] { list }
            ExprKind::ArrayNew { len, elems, dims, .. } => {
                for l in len.iter().map(|l| &**l).chain(dims.iter()) {
                    self.expr(l, st);
                }
                for x in elems {
                    self.take(x, st);
                }
            }
            // f(list) into an `own` parameter: the list moves into the call; `out` gives a new value
            ExprKind::Call { recv, args, .. } => {
                if let Some(r) = recv {
                    self.expr(r, st);
                }
                let mut outs = vec![];
                for a in args {
                    match (a.mode, local_of(self.f, &a.expr)) {
                        (ParamMode::Own, _) => self.take(&a.expr, st),
                        (ParamMode::Out, Some(l)) => outs.push(l),
                        _ => self.expr(&a.expr, st),
                    }
                }
                for l in outs {
                    self.assign(l, st);
                }
            }
            // c ? a : b: only one branch runs
            ExprKind::Cond(c, a, b) => {
                self.expr(c, st);
                let f0 = self.fresh_save();
                let mut sb = st.clone();
                self.expr(a, st);
                self.fresh_load(f0.clone());
                self.expr(b, &mut sb);
                self.fresh_load(f0);
                st.extend(sb);
            }
            // the right operand may not run
            ExprKind::Binary(crate::ast::BinOp::And | crate::ast::BinOp::Or, a, b) => {
                self.expr(a, st);
                let f0 = self.fresh_save();
                self.expr(b, st);
                self.fresh_load(f0);
            }
            // switch expression: conditions in order, then one arm
            ExprKind::Switch { scrut, arms, default, .. } => {
                self.expr(scrut, st);
                let f0 = self.fresh_save();
                let mut conds = st.clone();
                let mut out = HashSet::new();
                for (c, v) in arms {
                    self.expr(c, &mut conds);
                    let mut sv = conds.clone();
                    self.expr(v, &mut sv);
                    out.extend(sv);
                    self.fresh_load(f0.clone());
                }
                if let Some(x) = default {
                    let mut sv = conds.clone();
                    self.expr(x, &mut sv);
                    out.extend(sv);
                    self.fresh_load(f0);
                }
                out.extend(conds);
                *st = out;
            }
            // the body runs once per element (zero or more times): a move of an outer local in
            // it is an error, and the second run sees what the first one moved
            ExprKind::ListLambda { list, params, body, .. } => {
                self.expr(list, st);
                let over = self.root_local(list);
                let mut all = st.clone();
                for _ in 0..2 {
                    let mut s = all.clone();
                    // the parameters are bound to each element (moving one is E0504)
                    for x in params {
                        s.remove(x);
                    }
                    self.lams.push(Lam { over, fresh: params.iter().copied().collect() });
                    self.expr(body, &mut s);
                    self.lams.pop();
                    all.extend(s);
                }
                *st = all;
            }
            _ => {
                // evaluate children in order (approximation of evaluation order)
                for c in crate::sema::children(e) {
                    self.expr(c, st);
                }
            }
        }
    }

    fn block(&mut self, b: &Block, mut st: Moved) -> Moved {
        for s in &b.stmts {
            if st.is_none() {
                break; // unreachable
            }
            st = self.stmt(s, st);
        }
        st
    }

    fn stmt(&mut self, s: &Stmt, st: Moved) -> Moved {
        let Some(mut cur) = st else { return None };
        match s {
            Stmt::Let { local, init } => {
                if let Some(e) = init {
                    self.take(e, &mut cur);
                }
                self.assign(*local, &mut cur);
                Some(cur)
            }
            Stmt::Expr(e) => {
                self.expr(e, &mut cur);
                Some(cur)
            }
            Stmt::Return(v, _) => {
                // returning a local moves it out of a function that ends here: a read
                if let Some(e) = v {
                    self.expr(e, &mut cur);
                }
                None
            }
            Stmt::Break => {
                self.breaks.push(cur);
                None
            }
            Stmt::Continue => {
                self.conts.push(cur);
                None
            }
            Stmt::Fail(_) => {
                self.fails.push(cur);
                None
            }
            Stmt::If { cond, then, els } => {
                self.expr(cond, &mut cur);
                let a = self.block(then, Some(cur.clone()));
                let b = match els {
                    Some(e) => self.block(e, Some(cur)),
                    None => Some(cur),
                };
                either(a, b)
            }
            Stmt::Loop { cond, step, body, do_while } => {
                let infinite = cond.is_none() || matches!(cond, Some(Expr { kind: ExprKind::Bool(true), .. }));
                self.looped(cur, infinite, &mut |me, head| {
                    let mut h = head;
                    if !*do_while {
                        if let Some(c) = cond {
                            me.expr(c, &mut h);
                        }
                    }
                    let at_cond = h.clone();
                    let end = me.block(body, Some(h));
                    let mut back = end;
                    for c in std::mem::take(&mut me.conts) {
                        back = either(back, Some(c));
                    }
                    if let Some(b) = back.as_mut() {
                        for e in step {
                            me.expr(e, b);
                        }
                    }
                    if *do_while {
                        if let (Some(c), Some(b)) = (cond, back.as_mut()) {
                            me.expr(c, b);
                        }
                        // leaves when the condition (evaluated after the body) is false
                        (back.clone(), back)
                    } else {
                        (back, Some(at_cond))
                    }
                })
            }
            Stmt::ForQuery { body, .. } => self.looped(cur, false, &mut |me, head| {
                let mut back = me.block(body, Some(head.clone()));
                for c in std::mem::take(&mut me.conts) {
                    back = either(back, Some(c));
                }
                (back, Some(head))
            }),
            Stmt::ForList { var, list, body, .. } => {
                self.expr(list, &mut cur);
                // every iteration reads the list again: moving it inside the loop is a use after
                // move unless the loop is left right after
                let over = self.root_local(list);
                self.looped(cur, false, &mut |me, mut head| {
                    if let Some(l) = over {
                        me.read(l, list.span, &head);
                    }
                    head.remove(var); // bound to the next element (moving it is E0504)
                    let mut back = me.block(body, Some(head.clone()));
                    for c in std::mem::take(&mut me.conts) {
                        back = either(back, Some(c));
                    }
                    (back, Some(head))
                })
            }
            Stmt::Block(x) => self.block(x, Some(cur)),
            Stmt::Atomic { body, .. } => {
                let saved = std::mem::take(&mut self.fails);
                let mut out = self.block(body, Some(cur));
                // `fail` jumps to the end of the block (the undo log restores values, not moves)
                for f in std::mem::replace(&mut self.fails, saved) {
                    out = either(out, Some(f));
                }
                out
            }
            Stmt::Await { .. } => {
                for e in s.await_exprs() {
                    self.expr(e, &mut cur);
                }
                Some(cur)
            }
            Stmt::Switch { scrut, sections, default, .. } => {
                self.expr(scrut, &mut cur);
                let saved = std::mem::take(&mut self.breaks);
                // sections are alternatives: a local is moved afterwards if any section moved it
                let mut conds = cur;
                let mut out: Moved = None;
                for (c, x) in sections {
                    self.expr(c, &mut conds);
                    out = either(out, self.block(x, Some(conds.clone())));
                }
                out = match default {
                    Some(x) => either(out, self.block(x, Some(conds))),
                    None => either(out, Some(conds)),
                };
                // `break` leaves the switch
                for b in std::mem::replace(&mut self.breaks, saved) {
                    out = either(out, Some(b));
                }
                out
            }
        }
    }

    /// A loop: `iter` runs the body once from the state at the loop head and returns the state
    /// that goes back to the head and the state that leaves through the condition. Repeated until
    /// the head state is stable, so a move reaches the next iteration only along paths that
    /// continue the loop. Leaves through the condition (unless `infinite`) or a `break`.
    fn looped(&mut self, entry: HashSet<LocalId>, infinite: bool, iter: &mut dyn FnMut(&mut Self, HashSet<LocalId>) -> (Moved, Moved)) -> Moved {
        let saved = (std::mem::take(&mut self.breaks), std::mem::take(&mut self.conts));
        let mut head = entry.clone();
        let mut exit: Moved;
        loop {
            self.breaks.clear();
            self.conts.clear();
            let (back, out) = iter(self, head.clone());
            exit = if infinite { None } else { out };
            for b in std::mem::take(&mut self.breaks) {
                exit = either(exit, Some(b));
            }
            let mut next = entry.clone();
            if let Some(b) = back {
                next.extend(b);
            }
            if next == head {
                break;
            }
            head = next;
        }
        self.breaks = saved.0;
        self.conts = saved.1;
        exit
    }
}

fn overlapping(iterating: &[(Path, Span)], p: &Path) -> bool {
    iterating.iter().any(|(ip, _)| ip.root == p.root && (ip.proj.starts_with(&p.proj) || p.proj.starts_with(&ip.proj)))
}

/// Does a call (directly or through anything it calls) write the self field `fld`?
fn call_writes_self_field(p: &Program, info: &EffectInfo, callee: FuncId, fld: FieldId) -> bool {
    match p.funcs[callee].kind {
        FuncKind::EntityMethod(c) => {
            let mut archs = p.archetypes_of(c);
            if archs.is_empty() {
                archs.push(c);
            }
            archs.iter().any(|&a| {
                let g = p.virtual_target(a, callee);
                info.get(g, Some(a)).self_writes.contains(&fld)
            })
        }
        _ => false,
    }
}

fn iter_block(p: &Program, info: &EffectInfo, f: &Func, b: &Block, iterating: &mut Vec<(Path, Span)>, d: &mut Diags) {
    for s in &b.stmts {
        if let Stmt::ForList { list, body, .. } = s {
            if let Some(pp) = path_of(list) {
                iterating.push((pp, list.span));
                iter_block(p, info, f, body, iterating, d);
                iterating.pop();
                continue;
            }
        }
        if !iterating.is_empty() {
            let mut hit: Vec<Span> = vec![];
            let mut chk = |e: &Expr| {
                match &e.kind {
                    ExprKind::ListCall { list, op, .. } if op.mutates() => {
                        if path_of(list).map(|tp| overlapping(iterating, &tp)).unwrap_or(false) {
                            hit.push(e.span);
                        }
                    }
                    ExprKind::Assign { target, .. } if target.ty.is_list() => {
                        if path_of(target).map(|tp| overlapping(iterating, &tp)).unwrap_or(false) {
                            hit.push(e.span);
                        }
                    }
                    ExprKind::DictSet { dict, .. } => {
                        if path_of(dict).map(|tp| overlapping(iterating, &tp)).unwrap_or(false) {
                            hit.push(e.span);
                        }
                    }
                    ExprKind::DictCall { dict, op, .. } if op.mutates() => {
                        if path_of(dict).map(|tp| overlapping(iterating, &tp)).unwrap_or(false) {
                            hit.push(e.span);
                        }
                    }
                    ExprKind::Call { func, args, dispatch, .. } => {
                        for a in args {
                            if matches!(a.mode, ParamMode::Ref | ParamMode::Out) && path_of(&a.expr).map(|tp| overlapping(iterating, &tp)).unwrap_or(false) {
                                hit.push(a.expr.span);
                            }
                        }
                        if matches!(dispatch, Dispatch::SelfCall | Dispatch::BaseCall) {
                            for (ip, _) in iterating.iter() {
                                if let Root::SelfField(fld) = ip.root {
                                    if call_writes_self_field(p, info, *func, fld) {
                                        hit.push(e.span);
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            };
            // only this statement's own expressions; nested blocks are visited recursively below
            match s {
                Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => crate::sema::visit_expr(e, &mut chk),
                Stmt::If { cond, .. } => crate::sema::visit_expr(cond, &mut chk),
                Stmt::Loop { cond, step, .. } => {
                    if let Some(c) = cond {
                        crate::sema::visit_expr(c, &mut chk);
                    }
                    for e in step {
                        crate::sema::visit_expr(e, &mut chk);
                    }
                }
                Stmt::ForList { list, .. } => crate::sema::visit_expr(list, &mut chk),
                Stmt::Switch { scrut, sections, .. } => {
                    crate::sema::visit_expr(scrut, &mut chk);
                    for (c, _) in sections {
                        crate::sema::visit_expr(c, &mut chk);
                    }
                }
                _ => {}
            }
            for sp in hit {
                d.error_note("E0406", sp, "cannot modify a list while iterating over it", "collect changes in another list and apply them after the loop");
            }
        }
        match s {
            Stmt::If { then, els, .. } => {
                iter_block(p, info, f, then, iterating, d);
                if let Some(e) = els {
                    iter_block(p, info, f, e, iterating, d);
                }
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } => {
                iter_block(p, info, f, body, iterating, d)
            }
            Stmt::Switch { sections, default, .. } => {
                for (_, x) in sections {
                    iter_block(p, info, f, x, iterating, d);
                }
                if let Some(x) = default {
                    iter_block(p, info, f, x, iterating, d);
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// definite assignment (C# rules): locals must be assigned before use,
// `out` parameters must be assigned before the method returns
// ---------------------------------------------------------------------------

type Assigned = Option<HashSet<LocalId>>; // None = unreachable

fn join(a: Assigned, b: Assigned) -> Assigned {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(x), Some(y)) => Some(x.intersection(&y).copied().collect()),
    }
}

struct Da<'a> {
    f: &'a Func,
    reported: HashSet<LocalId>,
    errors: Vec<(&'static str, Span, String)>,
}

impl<'a> Da<'a> {
    fn read(&mut self, st: &Assigned, l: LocalId, sp: Span) {
        if let Some(set) = st {
            if !set.contains(&l) && self.reported.insert(l) {
                let loc = &self.f.locals[l];
                let what = if matches!(loc.kind, LocalKind::Param(ParamMode::Out)) { "out parameter" } else { "local" };
                self.errors.push(("E0306", sp, format!("use of unassigned {} '{}'", what, loc.name)));
            }
        }
    }

    fn expr(&mut self, e: &Expr, st: &mut Assigned) {
        match &e.kind {
            ExprKind::Local(l) => self.read(st, *l, e.span),
            ExprKind::ListLambda { list, params, body, .. } => {
                self.expr(list, st);
                // the body runs zero or more times, with its parameters bound to elements
                let mut inner = st.clone();
                if let Some(set) = inner.as_mut() {
                    set.extend(params.iter().copied());
                }
                self.expr(body, &mut inner);
            }
            ExprKind::Assign { op, target, value } => {
                match &target.kind {
                    ExprKind::Local(l) => {
                        if op.is_some() {
                            self.read(st, *l, target.span);
                        }
                        self.expr(value, st);
                        if let Some(set) = st {
                            set.insert(*l);
                        }
                    }
                    _ => {
                        // assigning a field of a struct local initializes the local (zero-filled)
                        self.place(target, st);
                        self.expr(value, st);
                    }
                }
            }
            ExprKind::Binary(crate::ast::BinOp::And | crate::ast::BinOp::Or, ..) => {
                let (t, f) = self.cond(e, st.clone());
                *st = join(t, f);
            }
            ExprKind::Cond(c, a, b) => {
                let (mut sa, mut sb) = self.cond(c, st.clone());
                self.expr(a, &mut sa);
                self.expr(b, &mut sb);
                *st = join(sa, sb);
            }
            ExprKind::Switch { scrut, arms, default, .. } => {
                self.expr(scrut, st);
                let mut cond_st = st.clone();
                let mut out: Assigned = None;
                for (c, v) in arms {
                    self.expr(c, &mut cond_st);
                    let mut sv = cond_st.clone();
                    self.expr(v, &mut sv);
                    out = join(out, sv);
                }
                match default {
                    Some(d) => {
                        let mut sv = cond_st;
                        self.expr(d, &mut sv);
                        out = join(out, sv);
                    }
                    None => out = join(out, cond_st),
                }
                *st = out;
            }
            ExprKind::Is { expr, bind, .. } => {
                self.expr(expr, st);
                if let (Some(b), Some(set)) = (bind, st.as_mut()) {
                    set.insert(*b); // only visible where the test succeeded
                }
            }
            ExprKind::Builtin { f: Builtin::TryParseInt | Builtin::TryParseFloat, args, .. } => {
                self.expr(&args[0], st);
                if let (ExprKind::Local(l), Some(set)) = (&args[1].kind, st.as_mut()) {
                    set.insert(*l); // 0 when the text is not a number, like C#
                }
            }
            ExprKind::DictCall { dict, op: DictOp::TryGet, args } => {
                self.expr(dict, st);
                self.expr(&args[0], st);
                if let (ExprKind::Local(l), Some(set)) = (&args[1].kind, st.as_mut()) {
                    set.insert(*l); // assigned on both outcomes (the default when missing)
                }
            }
            ExprKind::Call { recv, args, .. } => {
                if let Some(r) = recv {
                    self.expr(r, st);
                }
                for a in args {
                    match (&a.mode, &a.expr.kind) {
                        (ParamMode::Out, ExprKind::Local(_)) => {}
                        _ => self.expr(&a.expr, st),
                    }
                }
                for a in args {
                    if let (ParamMode::Out, ExprKind::Local(l)) = (&a.mode, &a.expr.kind) {
                        if let Some(set) = st {
                            set.insert(*l);
                        }
                    }
                }
            }
            _ => {
                for c in crate::sema::children(e) {
                    self.expr(c, st);
                }
            }
        }
    }

    /// C#'s "definitely assigned when true / when false" for a boolean expression: what holds on
    /// each outcome (`if (a && d.TryGetValue(k, out var v)) { v ... }` assigns v when true)
    fn cond(&mut self, e: &Expr, st: Assigned) -> (Assigned, Assigned) {
        use crate::ast::{BinOp, UnOp};
        match &e.kind {
            ExprKind::Binary(BinOp::And, a, b) => {
                let (at, af) = self.cond(a, st);
                let (bt, bf) = self.cond(b, at);
                (bt, join(af, bf))
            }
            ExprKind::Binary(BinOp::Or, a, b) => {
                let (at, af) = self.cond(a, st);
                let (bt, bf) = self.cond(b, af);
                (join(at, bt), bf)
            }
            ExprKind::Unary(UnOp::Not, a) => {
                let (t, f) = self.cond(a, st);
                (f, t)
            }
            _ => {
                let mut s = st;
                self.expr(e, &mut s);
                (s.clone(), s)
            }
        }
    }

    fn place(&mut self, e: &Expr, st: &mut Assigned) {
        match &e.kind {
            ExprKind::Local(l) => {
                if let Some(set) = st {
                    set.insert(*l);
                }
            }
            ExprKind::Field { base, .. } => self.place(base, st),
            _ => self.expr(e, st),
        }
    }

    fn check_outs(&mut self, st: &Assigned, sp: Span) {
        if let Some(set) = st {
            for &l in &self.f.params {
                if matches!(self.f.locals[l].kind, LocalKind::Param(ParamMode::Out)) && !set.contains(&l) && self.reported.insert(l + 1_000_000) {
                    self.errors.push(("E0308", sp, format!("out parameter '{}' must be assigned before the method returns", self.f.locals[l].name)));
                }
            }
        }
    }

    /// returns the state after the block; `loops` collects states at break statements
    fn block(&mut self, b: &Block, mut st: Assigned, breaks: &mut Vec<Assigned>) -> Assigned {
        for s in &b.stmts {
            st = self.stmt(s, st, breaks);
        }
        st
    }

    fn stmt(&mut self, s: &Stmt, mut st: Assigned, breaks: &mut Vec<Assigned>) -> Assigned {
        match s {
            Stmt::Let { local, init } => {
                if let Some(e) = init {
                    self.expr(e, &mut st);
                    if let Some(set) = st.as_mut() {
                        set.insert(*local);
                    }
                } else if let Some(set) = st.as_mut() {
                    // value types are zero-initialized, handles must be initialized (checked by sema)
                    set.remove(local);
                }
                st
            }
            Stmt::Expr(e) => {
                self.expr(e, &mut st);
                st
            }
            Stmt::If { cond, then, els } => {
                let (t, f) = self.cond(cond, st);
                let a = self.block(then, t, breaks);
                let b = match els {
                    Some(e) => self.block(e, f, breaks),
                    None => f,
                };
                join(a, b)
            }
            Stmt::Loop { cond, step, body, do_while } => {
                let entry = st.clone();
                let mut inner_breaks = vec![];
                let after_cond = if *do_while {
                    let s3 = self.block(body, st, &mut inner_breaks);
                    match cond {
                        Some(c) => self.cond(c, s3).1,
                        None => s3,
                    }
                } else {
                    let (t, f) = match cond {
                        Some(c) => self.cond(c, st),
                        None => (st.clone(), st),
                    };
                    let mut s2 = self.block(body, t, &mut inner_breaks);
                    for e in step {
                        self.expr(e, &mut s2);
                    }
                    f
                };
                let infinite = cond.is_none() || matches!(cond, Some(Expr { kind: ExprKind::Bool(true), .. }));
                let mut out = if infinite { None } else { after_cond };
                for b in inner_breaks {
                    out = join(out, b);
                }
                let _ = entry;
                out
            }
            Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } => {
                if let Stmt::ForList { list, .. } = s {
                    self.expr(list, &mut st);
                }
                let mut inner_breaks = vec![];
                let mut body_st = st.clone();
                if let (Stmt::ForList { var, idx, .. }, Some(set)) = (s, body_st.as_mut()) {
                    set.insert(*var);
                    set.insert(*idx);
                }
                if let (Stmt::ForQuery { var, .. }, Some(set)) = (s, body_st.as_mut()) {
                    set.insert(*var);
                }
                self.block(body, body_st, &mut inner_breaks);
                st
            }
            Stmt::Return(v, sp) => {
                if let Some(e) = v {
                    self.expr(e, &mut st);
                }
                self.check_outs(&st, *sp);
                None
            }
            Stmt::Break => {
                breaks.push(st);
                None
            }
            Stmt::Continue => None,
            Stmt::Block(b) => self.block(b, st, breaks),
            Stmt::Atomic { body, .. } => {
                let after = self.block(body, st.clone(), breaks);
                // a failed transaction restores the state from before the block
                join(after, st)
            }
            Stmt::Fail(_) => None,
            Stmt::Await { .. } => {
                for e in s.await_exprs() {
                    self.expr(e, &mut st);
                }
                st
            }
            Stmt::Switch { tmp, scrut, sections, default } => {
                self.expr(scrut, &mut st);
                if let (Some(t), Some(set)) = (tmp, st.as_mut()) {
                    set.insert(*t);
                }
                // conditions are evaluated in order until one holds; `break` leaves the switch
                let mut cond_st = st;
                let mut sw_breaks: Vec<Assigned> = vec![];
                let mut out: Assigned = None;
                for (c, b) in sections {
                    self.expr(c, &mut cond_st);
                    let end = self.block(b, cond_st.clone(), &mut sw_breaks);
                    out = join(out, end);
                }
                match default {
                    Some(b) => {
                        let end = self.block(b, cond_st, &mut sw_breaks);
                        out = join(out, end);
                    }
                    None => out = join(out, cond_st),
                }
                for b in sw_breaks {
                    out = join(out, b);
                }
                out
            }
        }
    }
}

fn definite_assignment(f: &Func, d: &mut Diags) {
    let mut init: HashSet<LocalId> = HashSet::new();
    for (l, loc) in f.locals.iter().enumerate() {
        match loc.kind {
            LocalKind::Param(ParamMode::Out) => {}
            LocalKind::Param(_) | LocalKind::Cursor(_) | LocalKind::ListElem => {
                init.insert(l);
            }
            LocalKind::Normal => {
                // locals start "assigned" until their declaration is reached (sema guarantees scoping)
                init.insert(l);
            }
        }
    }
    let mut da = Da { f, reported: HashSet::new(), errors: vec![] };
    let mut breaks = vec![];
    let end = da.block(&f.body, Some(init), &mut breaks);
    da.check_outs(&end, f.span);
    for (code, sp, msg) in da.errors {
        d.error(code, sp, msg);
    }
}
