//! Effect and access inference over the call graph.
//!
//! For every function (and, for entity methods, every archetype it can run on) we compute
//! which entity fields are read/written on `this` vs. on other entities, which archetypes are
//! queried/spawned/destroyed, and whether it prints, draws or uses randomness.
//! These summaries drive scheduling, ECS clustering, replication and safety checks.

use crate::ast::BinOp;
use crate::diag::{Diags, Span};
use crate::hir::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Effects {
    pub self_reads: BTreeSet<FieldId>,
    pub self_writes: BTreeSet<FieldId>,
    pub remote_reads: BTreeSet<FieldId>,
    /// field -> all writes are commutative (+=/-=/++/--)
    pub remote_writes: BTreeMap<FieldId, bool>,
    pub queries: BTreeSet<ClassId>,
    pub spawns: BTreeSet<ClassId>,
    pub destroys_self: bool,
    pub destroys: BTreeSet<ClassId>,
    /// `Add`/`Remove` of a component on this entity: it may move to another table of its class
    pub restructures_self: bool,
    /// `e.Add(...)`/`e.Remove<C>()` on other entities (the static class of `e`)
    pub restructures: BTreeSet<ClassId>,
    pub remote_calls: BTreeSet<FuncId>,
    /// fields written by deferred messages (method calls on other entities) when they are delivered
    pub msg_writes: BTreeSet<FieldId>,
    /// entity methods invoked on the server through client->server RPC
    pub rpcs: BTreeSet<FuncId>,
    pub prints: bool,
    pub draws: bool,
    pub random: bool,
    pub allocs: bool,
}

impl Effects {
    pub fn merge(&mut self, o: &Effects) {
        self.self_reads.extend(&o.self_reads);
        self.self_writes.extend(&o.self_writes);
        self.remote_reads.extend(&o.remote_reads);
        for (f, c) in &o.remote_writes {
            add_remote_write(&mut self.remote_writes, *f, *c);
        }
        self.queries.extend(&o.queries);
        self.spawns.extend(&o.spawns);
        self.destroys_self |= o.destroys_self;
        self.destroys.extend(&o.destroys);
        self.restructures_self |= o.restructures_self;
        self.restructures.extend(&o.restructures);
        self.remote_calls.extend(&o.remote_calls);
        self.msg_writes.extend(&o.msg_writes);
        self.rpcs.extend(&o.rpcs);
        self.prints |= o.prints;
        self.draws |= o.draws;
        self.random |= o.random;
        self.allocs |= o.allocs;
    }
    /// Does this summary mutate the world (anything beyond reading)?
    pub fn mutates(&self) -> bool {
        !self.self_writes.is_empty()
            || !self.remote_writes.is_empty()
            || !self.spawns.is_empty()
            || self.destroys_self
            || !self.destroys.is_empty()
            || self.restructures_self
            || !self.restructures.is_empty()
            || !self.remote_calls.is_empty()
            || !self.msg_writes.is_empty()
            || self.prints
    }
    pub fn touches_world(&self) -> bool {
        self.mutates() || !self.self_reads.is_empty() || !self.remote_reads.is_empty() || !self.queries.is_empty() || self.random
    }
}

fn add_remote_write(m: &mut BTreeMap<FieldId, bool>, f: FieldId, commutative: bool) {
    let e = m.entry(f).or_insert(commutative);
    *e = *e && commutative;
}

#[derive(Clone, Debug)]
struct CallRef {
    func: FuncId,
    dispatch: Dispatch,
    recv_class: Option<ClassId>,
    span: Span,
}

#[derive(Default)]
struct LocalInfo {
    eff: Effects,
    calls: Vec<CallRef>,
}

pub type Key = (FuncId, Option<ClassId>);

#[derive(Default)]
pub struct EffectInfo {
    pub summaries: HashMap<Key, Effects>,
    /// the static undo log of each atomic block (by id)
    pub atomic_saves: HashMap<usize, AtomicPlan>,
}

/// A collection an atomic block can change: a list/array/dictionary local (also a `ref`/`out`
/// parameter) or a collection field of `this` (also a coroutine's hidden field)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CollRoot {
    Local(LocalId),
    Field(FieldId),
}

/// What an atomic block records on entry so that `fail` can put everything back.
#[derive(Clone, Debug, Default)]
pub struct AtomicPlan {
    /// value fields of `this` written in the block: saved on entry
    pub fields: Vec<FieldId>,
    /// value locals declared outside the block and assigned in it: saved on entry
    pub locals: Vec<LocalId>,
    /// collections the block changes, with `true` when every change is an `Add` on the collection
    /// itself (the undo is then a length mark taken on entry); the others are copied just before
    /// their first change in the block (copy-on-write) and the copy is put back on `fail`
    pub colls: Vec<(CollRoot, bool)>,
}

/// The collection whose storage `e` (`xs`, `xs[i]`, `xs[i].X`, `Items`) lives in, if it is a local
/// or a field of `this`.
pub fn coll_root(e: &Expr) -> Option<CollRoot> {
    let mut cur = e;
    loop {
        match &cur.kind {
            ExprKind::Local(l) => return cur.ty.is_list().then_some(CollRoot::Local(*l)),
            ExprKind::EField { base, field } => return (matches!(base.kind, ExprKind::SelfRef) && cur.ty.is_list()).then_some(CollRoot::Field(*field)),
            ExprKind::Field { base, .. } => cur = base,
            ExprKind::ListIndex { list, .. } => cur = list,
            _ => return None,
        }
    }
}

/// Collections that evaluating `e` changes (entries are and-ed: `true` = only appended to): list,
/// array and dictionary methods, element writes, replacing the collection, `ref`/`out` arguments,
/// struct methods that change an element, and methods of `this` (on the archetypes `archs`) that
/// change one of its collection fields. Moving a local into an `own` parameter is not a change: the
/// local is then dead (E0501), so what it holds afterwards is never seen.
pub fn expr_coll_changes(p: &Program, sums: &HashMap<Key, Effects>, archs: &[Option<ClassId>], e: &Expr, out: &mut BTreeMap<CollRoot, bool>) {
    fn note(out: &mut BTreeMap<CollRoot, bool>, r: Option<CollRoot>, append: bool) {
        if let Some(r) = r {
            let e = out.entry(r).or_insert(append);
            *e = *e && append;
        }
    }
    crate::sema::visit_expr(e, &mut |x| match &x.kind {
        ExprKind::ListCall { list, op, .. } if op.mutates() => {
            // `xs.Add(v)` on the collection itself only appends; on an element (`grid[i].Add(v)`) it
            // changes what the outer collection owns
            let direct = matches!(list.kind, ExprKind::Local(_) | ExprKind::EField { .. });
            note(out, coll_root(list), *op == ListOp::Add && direct);
        }
        ExprKind::ListLambda { list, op, .. } if op.mutates() => note(out, coll_root(list), false),
        ExprKind::DictSet { dict, .. } => note(out, coll_root(dict), false),
        ExprKind::DictCall { dict, op, .. } if op.mutates() => note(out, coll_root(dict), false),
        ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => note(out, coll_root(target), false),
        ExprKind::Call { func, recv, args, dispatch } => {
            for a in args {
                if matches!(a.mode, crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out) {
                    note(out, coll_root(&a.expr), false);
                }
            }
            if let Some(r) = recv {
                if p.funcs[*func].writes_self {
                    note(out, coll_root(r), false);
                }
            }
            if matches!(dispatch, Dispatch::SelfCall | Dispatch::BaseCall) {
                for &arch in archs {
                    let target = match (dispatch, arch) {
                        (Dispatch::SelfCall, Some(a)) => p.virtual_target(a, *func),
                        _ => *func,
                    };
                    if let Some(s) = sums.get(&(target, arch)) {
                        for &f in &s.self_writes {
                            if p.fields[f].ty.is_list() {
                                note(out, Some(CollRoot::Field(f)), false);
                            }
                        }
                    }
                }
            }
        }
        _ => {}
    });
}

/// `expr_coll_changes` of every statement of `b` and of the blocks nested in it
pub fn block_coll_changes(p: &Program, sums: &HashMap<Key, Effects>, archs: &[Option<ClassId>], b: &Block, out: &mut BTreeMap<CollRoot, bool>) {
    for s in &b.stmts {
        for e in stmt_own_exprs(s) {
            expr_coll_changes(p, sums, archs, e, out);
        }
        for x in s.sub_blocks() {
            block_coll_changes(p, sums, archs, x, out);
        }
    }
}

/// The expressions a statement evaluates itself (not those of the blocks it contains); for an
/// `else if` chain, the conditions of the whole chain (code generation keeps the chain flat).
pub fn stmt_own_exprs(s: &Stmt) -> Vec<&Expr> {
    match s {
        Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => vec![e],
        Stmt::If { cond, els, .. } => {
            let mut v = vec![cond];
            let mut rest = els.as_ref();
            while let Some(b) = rest {
                match b.stmts.as_slice() {
                    [Stmt::If { cond, els, .. }] => {
                        v.push(cond);
                        rest = els.as_ref();
                    }
                    _ => break,
                }
            }
            v
        }
        Stmt::Loop { cond, step, .. } => cond.iter().chain(step.iter()).collect(),
        Stmt::ForList { list, .. } => vec![list],
        Stmt::Switch { scrut, sections, .. } => std::iter::once(scrut).chain(sections.iter().map(|(c, _)| c)).collect(),
        Stmt::Await { .. } => s.await_exprs(),
        _ => vec![],
    }
}

impl EffectInfo {
    pub fn get(&self, f: FuncId, arch: Option<ClassId>) -> Effects {
        self.summaries.get(&(f, arch)).cloned().unwrap_or_default()
    }
}

/// The entity field an assignment target ultimately refers to, and whether it is on `this`.
pub fn target_field(e: &Expr) -> Option<(FieldId, bool)> {
    match &e.kind {
        ExprKind::EField { base, field } => Some((*field, matches!(base.kind, ExprKind::SelfRef))),
        ExprKind::Field { base, .. } => target_field(base),
        ExprKind::ListIndex { list, .. } => target_field(list),
        _ => None,
    }
}

fn local_walk(p: &Program, f: &Func) -> LocalInfo {
    let mut li = LocalInfo::default();
    walk_block(p, &f.body, &mut li);
    if f.extern_name.is_some() && !f.pure_attr {
        li.eff.prints = true; // host code: effects outside the world
    }
    li
}

fn walk_block(p: &Program, b: &Block, li: &mut LocalInfo) {
    for s in &b.stmts {
        walk_stmt(p, s, li);
    }
}

fn walk_stmt(p: &Program, s: &Stmt, li: &mut LocalInfo) {
    match s {
        Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => walk_expr(p, e, li, false),
        Stmt::If { cond, then, els } => {
            walk_expr(p, cond, li, false);
            walk_block(p, then, li);
            if let Some(e) = els {
                walk_block(p, e, li);
            }
        }
        Stmt::Loop { cond, step, body, .. } => {
            if let Some(c) = cond {
                walk_expr(p, c, li, false);
            }
            for e in step {
                walk_expr(p, e, li, false);
            }
            walk_block(p, body, li);
        }
        Stmt::ForQuery { class, with, body, .. } => {
            li.eff.queries.insert(*class);
            li.eff.queries.extend(with.iter().copied());
            walk_block(p, body, li);
        }
        Stmt::ForList { list, body, .. } => {
            walk_expr(p, list, li, false);
            walk_block(p, body, li);
        }
        Stmt::Block(b) | Stmt::Atomic { body: b, .. } => walk_block(p, b, li),
        Stmt::Switch { scrut, sections, default, .. } => {
            walk_expr(p, scrut, li, false);
            for (c, b) in sections {
                walk_expr(p, c, li, false);
                walk_block(p, b, li);
            }
            if let Some(d) = default {
                walk_block(p, d, li);
            }
        }
        Stmt::Await { state, timer, .. } => {
            // the coroutine frame is written (resume point, timer) and read on resume
            record_write(p, state, None, li);
            record_write(p, timer, None, li);
            for e in s.await_exprs() {
                walk_expr(p, e, li, false);
            }
        }
        _ => {}
    }
}

/// `as_target`: the expression is being written (do not count the outermost access as a read).
fn walk_expr(p: &Program, e: &Expr, li: &mut LocalInfo, as_target: bool) {
    match &e.kind {
        ExprKind::EField { base, field } => {
            if !as_target {
                if matches!(base.kind, ExprKind::SelfRef) {
                    li.eff.self_reads.insert(*field);
                } else {
                    li.eff.remote_reads.insert(*field);
                }
            }
            walk_expr(p, base, li, false);
        }
        ExprKind::Field { base, .. } => walk_expr(p, base, li, as_target),
        ExprKind::ListIndex { list, index, .. } => {
            walk_expr(p, list, li, as_target);
            walk_expr(p, index, li, false);
        }
        ExprKind::Assign { op, target, value } => {
            record_write(p, target, *op, li);
            // `other.F += x` on a number is a deferred ADD: it never reads the snapshot of F, so it is
            // not a remote read (no double buffer for fields that others only add to)
            let remote_add = matches!(op, Some(BinOp::Add) | Some(BinOp::Sub))
                && matches!(target.ty, Ty::Int | Ty::Float)
                && matches!(target_field(target), Some((_, false)));
            if op.is_some() && !remote_add {
                walk_expr(p, target, li, false); // compound reads the target
            } else {
                walk_expr(p, target, li, true);
            }
            walk_expr(p, value, li, false);
        }
        ExprKind::IncDec { target, .. } => {
            record_write(p, target, Some(BinOp::Add), li);
            walk_expr(p, target, li, false);
        }
        ExprKind::ListCall { list, op, args } => {
            if op.mutates() {
                record_write(p, list, None, li);
            }
            walk_expr(p, list, li, op.mutates());
            if op.mutates() {
                // mutating calls also read the list (length, buffer)
                walk_expr(p, list, li, false);
            }
            for a in args {
                walk_expr(p, a, li, false);
            }
        }
        ExprKind::Call { func, recv, args, dispatch } => {
            if p.funcs[*func].extern_name.is_some() && !p.funcs[*func].pure_attr {
                // host code: unknown effects outside the world (keeps the system serial, not in atomic)
                li.eff.prints = true;
            }
            let recv_class = recv.as_ref().and_then(|r| r.ty.class_of());
            li.calls.push(CallRef { func: *func, dispatch: *dispatch, recv_class, span: e.span });
            if let Some(r) = recv {
                // struct receivers that are mutated count as writes
                if p.funcs[*func].writes_self {
                    record_write(p, r, None, li);
                }
                walk_expr(p, r, li, false);
            }
            for a in args {
                if matches!(a.mode, crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out) {
                    record_write(p, &a.expr, None, li);
                    walk_expr(p, &a.expr, li, a.mode == crate::ast::ParamMode::Out);
                } else {
                    walk_expr(p, &a.expr, li, false);
                }
            }
        }
        ExprKind::OpCall { func, args } => {
            li.calls.push(CallRef { func: *func, dispatch: Dispatch::Static, recv_class: None, span: e.span });
            for a in args {
                walk_expr(p, a, li, false);
            }
        }
        ExprKind::StructLit { ctor, args, inits, .. } => {
            if let Some(c) = ctor {
                li.calls.push(CallRef { func: *c, dispatch: Dispatch::Static, recv_class: None, span: e.span });
            }
            for a in args {
                walk_expr(p, &a.expr, li, false);
            }
            for (_, x) in inits {
                walk_expr(p, x, li, false);
            }
        }
        ExprKind::Spawn { class, inits, .. } => {
            li.eff.spawns.insert(*class);
            for (_, x) in inits {
                walk_expr(p, x, li, false);
            }
        }
        ExprKind::CompAdd { target, .. } | ExprKind::CompRemove { target, .. } => {
            if matches!(target.kind, ExprKind::SelfRef) {
                li.eff.restructures_self = true;
            } else if let Some(c) = target.ty.class_of() {
                li.eff.restructures.insert(c);
            }
            for c in crate::sema::children(e) {
                walk_expr(p, c, li, false);
            }
        }
        ExprKind::Destroy(None) => li.eff.destroys_self = true,
        ExprKind::Destroy(Some(h)) => {
            if matches!(h.kind, ExprKind::SelfRef) {
                li.eff.destroys_self = true;
            } else if let Some(c) = h.ty.class_of() {
                li.eff.destroys.insert(c);
            }
            walk_expr(p, h, li, false);
        }
        ExprKind::Count(c) => {
            li.eff.queries.insert(*c);
        }
        ExprKind::Is { expr, class, .. } => {
            // a liveness check observes whether entities exist (conflicts with Spawn/Destroy)
            li.eff.queries.insert(*class);
            // (`Owner`, the only value of the type `Entity`, is the entity the code runs for: it
            // is alive, and observing it reads no other entity's existence)
            if let Some(src) = expr.ty.class_of().filter(|&c| Some(c) != p.entity_root) {
                li.eff.queries.insert(src);
            }
            walk_expr(p, expr, li, false);
        }
        ExprKind::Builtin { f, args, .. } => {
            match f {
                Builtin::Print => li.eff.prints = true,
                Builtin::DrawSprite | Builtin::DrawText | Builtin::NetView | Builtin::NetClearView => li.eff.draws = true,
                Builtin::RandomFloat | Builtin::RandomInt => li.eff.random = true,
                _ => {}
            }
            for a in args {
                walk_expr(p, a, li, false);
            }
        }
        ExprKind::DictSet { dict, key, value } => {
            record_write(p, dict, None, li);
            walk_expr(p, dict, li, true);
            walk_expr(p, dict, li, false);
            walk_expr(p, key, li, false);
            walk_expr(p, value, li, false);
        }
        ExprKind::DictCall { dict, op, args } => {
            if op.mutates() {
                record_write(p, dict, None, li);
                walk_expr(p, dict, li, true);
            }
            walk_expr(p, dict, li, false);
            for a in args {
                walk_expr(p, a, li, false);
            }
        }
        ExprKind::ListNew { .. } => li.eff.allocs = true,
        ExprKind::ListLambda { list, op, body, .. } => {
            if op.mutates() {
                record_write(p, list, None, li);
            }
            if op.makes_list() || *op == LamOp::Sort {
                li.eff.allocs = true;
            }
            walk_expr(p, list, li, false);
            walk_expr(p, body, li, false);
        }
        ExprKind::ArrayNew { len, elems, dims, .. } => {
            li.eff.allocs = true;
            for l in len.iter().map(|l| &**l).chain(dims.iter()) {
                walk_expr(p, l, li, false);
            }
            for x in elems {
                walk_expr(p, x, li, false);
            }
        }
        ExprKind::ListClone { list, .. } => {
            li.eff.allocs = true;
            walk_expr(p, list, li, false);
        }
        _ => {
            for c in crate::sema::children(e) {
                walk_expr(p, c, li, false);
            }
        }
    }
}

fn record_write(p: &Program, target: &Expr, op: Option<BinOp>, li: &mut LocalInfo) {
    if let Some((f, is_self)) = target_field(target) {
        if is_self {
            li.eff.self_writes.insert(f);
        } else {
            let direct_numeric = matches!(target.kind, ExprKind::EField { .. } | ExprKind::Field { .. }) && target.ty.is_numeric();
            let commutative = direct_numeric && matches!(op, Some(BinOp::Add) | Some(BinOp::Sub));
            let _ = p;
            add_remote_write(&mut li.eff.remote_writes, f, commutative);
        }
    }
}

/// Map a callee summary into the caller according to the dispatch kind.
fn map_callee(p: &Program, sums: &HashMap<Key, Effects>, c: &CallRef, arch: Option<ClassId>, out: &mut Effects) -> bool {
    match c.dispatch {
        Dispatch::Static => {
            if let Some(s) = sums.get(&(c.func, None)) {
                out.merge(s);
            }
            false
        }
        Dispatch::SelfCall | Dispatch::BaseCall => {
            let target = match (c.dispatch, arch) {
                (Dispatch::SelfCall, Some(a)) => p.target_of(a, c.func),
                _ => c.func,
            };
            if let Some(s) = sums.get(&(target, arch)) {
                out.merge(s);
            }
            false
        }
        Dispatch::Rpc => {
            // runs on the server; the client only sends the request
            out.rpcs.insert(c.func);
            false
        }
        Dispatch::RemoteRead | Dispatch::RemoteDeferred => {
            let class = c.recv_class.unwrap_or(0);
            let mut deferred = false;
            let mut acc = Effects::default();
            for b in p.archetypes_of(class) {
                let g = p.target_of(b, c.func);
                if let Some(s) = sums.get(&(g, Some(b))) {
                    if s.mutates() {
                        deferred = true;
                    }
                    acc.merge(s);
                    if s.mutates() {
                        acc.remote_calls.insert(g);
                    }
                }
            }
            // seen from the caller, the callee's `this` is another entity
            let mut m = Effects::default();
            m.remote_reads.extend(&acc.self_reads);
            m.remote_reads.extend(&acc.remote_reads);
            // writes performed when the message is delivered
            m.msg_writes.extend(&acc.self_writes);
            m.msg_writes.extend(acc.remote_writes.keys());
            m.msg_writes.extend(&acc.msg_writes);
            m.queries = acc.queries.clone();
            m.spawns = acc.spawns.clone();
            if acc.destroys_self {
                m.destroys.insert(class);
            }
            m.destroys.extend(&acc.destroys);
            if acc.restructures_self {
                m.restructures.insert(class);
            }
            m.restructures.extend(&acc.restructures);
            m.remote_calls = acc.remote_calls.clone();
            m.prints = acc.prints;
            m.draws = acc.draws;
            m.random = acc.random;
            m.allocs = acc.allocs;
            out.merge(&m);
            deferred
        }
    }
}

fn all_keys(p: &Program) -> Vec<Key> {
    let mut keys: Vec<Key> = vec![];
    for (fid, f) in p.funcs.iter().enumerate() {
        match f.kind {
            FuncKind::EntityMethod(c) => {
                let archs = p.archetypes_of(c);
                if archs.is_empty() {
                    keys.push((fid, Some(c)));
                }
                for a in archs {
                    keys.push((fid, Some(a)));
                }
            }
            _ => keys.push((fid, None)),
        }
    }
    keys
}

fn fixpoint(p: &Program, keys: &[Key], locals: &[LocalInfo]) -> HashMap<Key, Effects> {
    let mut sums: HashMap<Key, Effects> = keys.iter().map(|k| (*k, locals[k.0].eff.clone())).collect();
    for _round in 0..64 {
        let mut changed = false;
        for k in keys {
            let mut e = locals[k.0].eff.clone();
            for c in &locals[k.0].calls {
                map_callee(p, &sums, c, k.1, &mut e);
            }
            if sums.get(k) != Some(&e) {
                sums.insert(*k, e);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    sums
}

/// Does an entity-method call (on self or another entity) mutate the world, for any archetype it may hit?
fn call_mutates(p: &Program, sums: &HashMap<Key, Effects>, c: &CallRef, caller: FuncId) -> bool {
    let classes: Vec<ClassId> = match c.dispatch {
        Dispatch::SelfCall | Dispatch::BaseCall => match p.funcs[caller].kind {
            FuncKind::EntityMethod(cc) => p.archetypes_of(cc),
            _ => vec![],
        },
        Dispatch::RemoteRead => c.recv_class.map(|rc| p.archetypes_of(rc)).unwrap_or_default(),
        _ => return false,
    };
    classes.into_iter().any(|a| {
        let g = if c.dispatch == Dispatch::BaseCall { c.func } else { p.target_of(a, c.func) };
        sums.get(&(g, Some(a))).map(|s| s.mutates()).unwrap_or(false)
    })
}

pub fn analyze(p: &mut Program, d: &mut Diags) -> EffectInfo {
    let keys = all_keys(p);
    let locals: Vec<LocalInfo> = p.funcs.iter().map(|f| local_walk(p, f)).collect();
    let sums = fixpoint(p, &keys, &locals);
    // client code calling mutating entity methods -> automatic client->server RPC
    let mut rpc_sites: Vec<(FuncId, Span)> = vec![];
    for (fid, li) in locals.iter().enumerate() {
        if p.funcs[fid].ctx != Ctx::Client {
            continue;
        }
        for c in &li.calls {
            if !matches!(c.dispatch, Dispatch::SelfCall | Dispatch::BaseCall | Dispatch::RemoteRead) || p.funcs[c.func].ctx == Ctx::Client {
                continue;
            }
            if call_mutates(p, &sums, c, fid) {
                rpc_sites.push((fid, c.span));
                let callee = &p.funcs[c.func];
                if callee.ret != Ty::Void {
                    d.error_note(
                        "E0608",
                        c.span,
                        format!("'{}' modifies server state, so calling it from client code is an RPC and cannot return a value", callee.name),
                        "RPCs are fire-and-forget requests; the client sees the outcome through replication",
                    );
                }
                for (i, &l) in callee.params.iter().enumerate() {
                    let loc = &callee.locals[l];
                    if matches!(callee.param_mode(i), crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out) || loc.ty.is_list() || loc.ty.is_handle() || p.has_delegate(&loc.ty) {
                        d.error("E0608", c.span, format!("RPC '{}': parameter '{}' must be a plain value (no ref/out, lists or entity handles)", callee.name, loc.name));
                    }
                }
            }
        }
    }
    for (fid, sp) in rpc_sites {
        let mut body = std::mem::take(&mut p.funcs[fid].body);
        crate::opt::visit_block_mut(&mut body, &mut |e| {
            if let ExprKind::Call { dispatch, .. } = &mut e.kind {
                if e.span == sp && matches!(dispatch, Dispatch::SelfCall | Dispatch::BaseCall | Dispatch::RemoteRead) {
                    *dispatch = Dispatch::Rpc;
                }
            }
        });
        p.funcs[fid].body = body;
    }
    let locals: Vec<LocalInfo> = p.funcs.iter().map(|f| local_walk(p, f)).collect();
    let sums = fixpoint(p, &keys, &locals);
    // decide remote dispatch per call site
    let mut deferred_sites: Vec<(FuncId, Span)> = vec![];
    for (fid, li) in locals.iter().enumerate() {
        for c in &li.calls {
            if c.dispatch == Dispatch::RemoteRead {
                let mut tmp = Effects::default();
                if map_callee(p, &sums, c, None, &mut tmp) {
                    deferred_sites.push((fid, c.span));
                    let callee = &p.funcs[c.func];
                    for (i, &l) in callee.params.iter().enumerate() {
                        let loc = &callee.locals[l];
                        let tn = p.ty_name(&loc.ty);
                        if matches!(callee.param_mode(i), crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out) {
                            d.error_note(
                                "E0248",
                                c.span,
                                format!("'{}' is delivered as a message to another entity, so parameter '{}' cannot be ref/out", callee.name, loc.name),
                                "messages are applied at the end of the system, when the caller's variables are gone; pass the value and let the receiver keep the result",
                            );
                        } else if p.has_delegate(&loc.ty) {
                            d.error_note(
                                "E0248",
                                c.span,
                                format!("'{}' is delivered as a message to another entity, so parameter '{}' cannot hold a delegate ('{}')", callee.name, loc.name, tn),
                                "a delegate's function runs on the server that made it; send the values it needs and let the receiver decide what to run",
                            );
                        } else if loc.ty.is_list() && !message_copyable(p, &loc.ty, 0) {
                            d.error_note(
                                "E0248",
                                c.span,
                                format!("'{}' is delivered as a message to another entity, so parameter '{}' cannot be a dictionary ('{}')", callee.name, loc.name, tn),
                                "a message copies lists and arrays (of values, strings, structs, handles and other lists) but not dictionaries; pass the entries as a list",
                            );
                        }
                    }
                    if p.funcs[c.func].ret != Ty::Void {
                        d.error_note(
                            "E0247",
                            c.span,
                            format!("'{}' modifies its entity, so calling it on another entity is a deferred message and cannot return a value", p.funcs[c.func].name),
                            "split it into a read-only query method and a void command method",
                        );
                    }
                }
            }
        }
    }
    for (fid, sp) in deferred_sites {
        let mut body = std::mem::take(&mut p.funcs[fid].body);
        mark_deferred(&mut body, sp);
        p.funcs[fid].body = body;
    }
    let info = EffectInfo { summaries: sums, atomic_saves: HashMap::new() };
    let mut info = info;
    context_checks(p, &info, d);
    atomic_analysis(p, &mut info, d);
    read_after_deferred_write(p, d);
    info
}

/// Can a message argument of this type be copied into the command buffer? Lists and arrays (of
/// any rank) of values, strings, structs, entity handles and of such lists: the message carries
/// a copy and the receiver gets its own. Dictionaries cannot (their index is not copied).
pub fn message_copyable(p: &Program, t: &Ty, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    match t {
        Ty::Dict(..) => false,
        Ty::List(_) | Ty::Array(_) | Ty::MdArray(..) => p.elem_of(t).map(|e| message_copyable(p, &e, depth + 1)).unwrap_or(false),
        _ => true,
    }
}

fn mark_deferred(b: &mut Block, sp: Span) {
    crate::opt::visit_block_mut(b, &mut |e| {
        if let ExprKind::Call { dispatch, .. } = &mut e.kind {
            if *dispatch == Dispatch::RemoteRead && e.span == sp {
                *dispatch = Dispatch::RemoteDeferred;
            }
        }
    });
}

fn func_keys(p: &Program, fid: FuncId) -> Vec<Option<ClassId>> {
    match p.funcs[fid].kind {
        FuncKind::EntityMethod(c) => {
            let a = p.archetypes_of(c);
            if a.is_empty() {
                vec![Some(c)]
            } else {
                a.into_iter().map(Some).collect()
            }
        }
        _ => vec![None],
    }
}

fn union_summary(p: &Program, info: &EffectInfo, fid: FuncId) -> Effects {
    let mut e = Effects::default();
    for a in func_keys(p, fid) {
        e.merge(&info.get(fid, a));
    }
    e
}

fn context_checks(p: &Program, info: &EffectInfo, d: &mut Diags) {
    for (fid, f) in p.funcs.iter().enumerate() {
        let li = local_walk(p, f);
        if f.ctx == Ctx::Client {
            // direct mutations
            let e = &li.eff;
            if !e.self_writes.is_empty() || !e.remote_writes.is_empty() || !e.spawns.is_empty() || e.destroys_self || !e.destroys.is_empty() || e.restructures_self || !e.restructures.is_empty() {
                d.error_note(
                    "E0601",
                    f.span,
                    format!("client function '{}' modifies simulation state", p.func_label(fid)),
                    "the server is authoritative: clients only read replicated state; call an entity method that makes the change (from client code it becomes an automatic RPC)",
                );
            }
            for c in &li.calls {
                let callee = &p.funcs[c.func];
                if c.dispatch == Dispatch::Rpc {
                    for &l in &callee.params {
                        let t = &callee.locals[l].ty;
                        if *t != Ty::Str && p.contains_str(t) {
                            d.error_note(
                                "E0249",
                                c.span,
                                format!("'{}' is sent to the server as an RPC, so parameter '{}' cannot be a struct holding strings", callee.name, callee.locals[l].name),
                                "pass the strings as separate string parameters",
                            );
                        }
                    }
                }
                if callee.ctx == Ctx::Client || c.dispatch == Dispatch::Rpc {
                    continue;
                }
                let s = union_summary(p, info, c.func);
                let bad = !s.self_writes.is_empty() || !s.remote_writes.is_empty() || !s.spawns.is_empty() || s.destroys_self || !s.destroys.is_empty() || s.restructures_self || !s.restructures.is_empty() || !s.remote_calls.is_empty();
                if bad {
                    d.error("E0601", c.span, format!("client code calls '{}', which modifies simulation state", p.func_label(c.func)));
                }
            }
        } else {
            if li.eff.draws {
                d.error_note(
                    "E0602",
                    f.span,
                    format!("'{}' uses a client-only API (Draw.*, Net.View) but is not 'client'", p.func_label(fid)),
                    "rendering and views belong to clients; declare the method 'client' (public client void Render())",
                );
            }
            for c in &li.calls {
                if p.funcs[c.func].ctx == Ctx::Client {
                    d.error("E0605", c.span, format!("server code cannot call client function '{}'", p.func_label(c.func)));
                }
            }
        }
        if f.pure_attr {
            let s = union_summary(p, info, fid);
            if s.touches_world() || s.draws || s.allocs {
                d.error_note("E0706", f.span, format!("'{}' is marked [Pure] but has effects", p.func_label(fid)), describe_effects(p, &s));
            }
        }
    }
}

pub fn describe_effects(p: &Program, e: &Effects) -> String {
    let mut parts = vec![];
    let names = |s: &BTreeSet<FieldId>| s.iter().map(|f| p.field_label(*f)).collect::<Vec<_>>().join(", ");
    if !e.self_reads.is_empty() {
        parts.push(format!("reads self {{{}}}", names(&e.self_reads)));
    }
    if !e.self_writes.is_empty() {
        parts.push(format!("writes self {{{}}}", names(&e.self_writes)));
    }
    if !e.remote_reads.is_empty() {
        parts.push(format!("reads others {{{}}}", names(&e.remote_reads)));
    }
    if !e.remote_writes.is_empty() {
        let w: Vec<String> = e
            .remote_writes
            .iter()
            .map(|(f, c)| format!("{}{}", p.field_label(*f), if *c { " (+=)" } else { "" }))
            .collect();
        parts.push(format!("defers writes {{{}}}", w.join(", ")));
    }
    let cls = |s: &BTreeSet<ClassId>| s.iter().map(|c| p.classes[*c].name.clone()).collect::<Vec<_>>().join(", ");
    if !e.queries.is_empty() {
        parts.push(format!("queries {{{}}}", cls(&e.queries)));
    }
    if !e.spawns.is_empty() {
        parts.push(format!("spawns {{{}}}", cls(&e.spawns)));
    }
    if e.destroys_self {
        parts.push("destroys self".into());
    }
    if !e.destroys.is_empty() {
        parts.push(format!("destroys {{{}}}", cls(&e.destroys)));
    }
    if e.restructures_self {
        parts.push("adds/removes components of self".into());
    }
    if !e.restructures.is_empty() {
        parts.push(format!("adds/removes components of {{{}}}", cls(&e.restructures)));
    }
    if !e.remote_calls.is_empty() {
        let c: Vec<String> = e.remote_calls.iter().map(|f| p.func_label(*f)).collect();
        parts.push(format!("sends {{{}}}", c.join(", ")));
    }
    if !e.msg_writes.is_empty() {
        parts.push(format!("messages write {{{}}}", names(&e.msg_writes)));
    }
    if !e.rpcs.is_empty() {
        let c: Vec<String> = e.rpcs.iter().map(|f| p.func_label(*f)).collect();
        parts.push(format!("rpc to server {{{}}}", c.join(", ")));
    }
    if e.prints {
        parts.push("io".into());
    }
    if e.draws {
        parts.push("draw".into());
    }
    if e.random {
        parts.push("random".into());
    }
    if parts.is_empty() {
        "pure".into()
    } else {
        parts.join("; ")
    }
}

fn atomic_analysis(p: &Program, info: &mut EffectInfo, d: &mut Diags) {
    for (fid, f) in p.funcs.iter().enumerate() {
        let archs = func_keys(p, fid);
        let mut found: Vec<(usize, Span, Block)> = vec![];
        collect_atomics(&f.body, &mut found);
        let mut reported: Vec<Span> = vec![];
        for (id, sp, body) in found {
            if matches!(f.kind, FuncKind::StructMethod(_) | FuncKind::StructCtor(_) | FuncKind::StructOp(_) | FuncKind::StructStatic(_)) {
                // once per outermost block
                if !reported.iter().any(|r| r.lo <= sp.lo && sp.hi <= r.hi) {
                    d.error_note(
                        "E0705",
                        sp,
                        "'atomic' is only supported in entity and static class methods",
                        "put the atomic block in the method that calls this struct method (a struct method's changes to `this` are rolled back with the variable or field that holds the struct)",
                    );
                    reported.push(sp);
                }
                continue;
            }
            // the effects of the block with (`all`) and without (`own`) the blocks nested in it
            let effects_of = |b: &Block| {
                let mut li = LocalInfo::default();
                walk_block(p, b, &mut li);
                let mut e = li.eff.clone();
                for &arch in &archs {
                    for c in &li.calls {
                        // a message to another entity is a deferred command: it runs after the block
                        // (and is rolled back with it on `fail`), so what its body prints is not
                        // inside the transaction
                        let mut ce = Effects::default();
                        if map_callee(p, &info.summaries, c, arch, &mut ce) {
                            ce.prints = false;
                            ce.draws = false;
                        }
                        e.merge(&ce);
                    }
                }
                e
            };
            let e = effects_of(&body);
            // an irreversible effect is reported once, by the innermost block around it
            let own = effects_of(&without_atomics(&body));
            if own.prints || own.draws {
                d.error_note(
                    "E0701",
                    sp,
                    "'atomic' blocks cannot contain irreversible effects (Print, Draw.*, Net.View or extern host calls)",
                    "move the output after the block (it runs only when the block did not fail)",
                );
            }
            // value fields are saved on entry; collection fields follow the collection undo log
            let fields: Vec<FieldId> = e.self_writes.iter().copied().filter(|&x| !p.fields[x].ty.is_list()).collect();
            let mut declared = BTreeSet::new();
            collect_lets(&body, &mut declared);
            // collections changed in the block (and in the blocks nested in it), except locals
            // declared inside it
            let mut colls: BTreeMap<CollRoot, bool> = BTreeMap::new();
            block_coll_changes(p, &info.summaries, &archs, &body, &mut colls);
            colls.retain(|r, _| !matches!(r, CollRoot::Local(l) if declared.contains(l)));
            // outer value locals assigned inside the block
            let mut assigned = BTreeSet::new();
            crate::sema::visit_block(&body, &mut |x| match &x.kind {
                ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => {
                    if let Some(l) = root_local(target) {
                        assigned.insert(l);
                    }
                }
                ExprKind::DictCall { op, args, .. } => {
                    if *op == DictOp::TryGet {
                        if let Some(l) = args.get(1).and_then(root_local) {
                            assigned.insert(l);
                        }
                    }
                }
                ExprKind::Builtin { f: Builtin::TryParseInt | Builtin::TryParseFloat, args, .. } => {
                    if let Some(l) = args.get(1).and_then(root_local) {
                        assigned.insert(l);
                    }
                }
                ExprKind::Call { args, recv, func, .. } => {
                    for a in args {
                        if matches!(a.mode, crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out) {
                            if let Some(l) = root_local(&a.expr) {
                                assigned.insert(l);
                            }
                        }
                    }
                    if let Some(r) = recv {
                        if p.funcs[*func].writes_self {
                            if let Some(l) = root_local(r) {
                                assigned.insert(l);
                            }
                        }
                    }
                }
                _ => {}
            });
            let locals: Vec<LocalId> = assigned.difference(&declared).copied().filter(|l| !f.locals[*l].ty.is_list()).collect();
            info.atomic_saves.insert(id, AtomicPlan { fields, locals, colls: colls.into_iter().collect() });
        }
    }
}

fn root_local(e: &Expr) -> Option<LocalId> {
    match &e.kind {
        ExprKind::Local(l) => Some(*l),
        ExprKind::Field { base, .. } | ExprKind::ListIndex { list: base, .. } => root_local(base),
        _ => None,
    }
}

fn collect_lets(b: &Block, out: &mut BTreeSet<LocalId>) {
    for s in &b.stmts {
        match s {
            Stmt::Let { local, .. } => {
                out.insert(*local);
            }
            Stmt::If { then, els, .. } => {
                collect_lets(then, out);
                if let Some(e) = els {
                    collect_lets(e, out);
                }
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => collect_lets(body, out),
            Stmt::ForQuery { var, body, .. } => {
                out.insert(*var);
                collect_lets(body, out);
            }
            Stmt::ForList { var, idx, body, .. } => {
                out.insert(*var);
                out.insert(*idx);
                collect_lets(body, out);
            }
            Stmt::Switch { tmp, sections, default, .. } => {
                if let Some(t) = tmp {
                    out.insert(*t);
                }
                for (_, b) in sections {
                    collect_lets(b, out);
                }
                if let Some(d) = default {
                    collect_lets(d, out);
                }
            }
            _ => {}
        }
    }
    crate::sema::visit_block(b, &mut |e| {
        if let ExprKind::Is { bind: Some(l), .. } = &e.kind {
            out.insert(*l);
        }
        if let ExprKind::Switch { tmp: Some(l), .. } = &e.kind {
            out.insert(*l);
        }
    });
}

/// `b` with the atomic blocks nested in it emptied
fn without_atomics(b: &Block) -> Block {
    let mut b = b.clone();
    fn strip(b: &mut Block) {
        for s in b.stmts.iter_mut() {
            if matches!(s, Stmt::Atomic { .. }) {
                *s = Stmt::Block(Block::default());
                continue;
            }
            match s {
                Stmt::If { then, els, .. } => {
                    strip(then);
                    if let Some(e) = els {
                        strip(e);
                    }
                }
                Stmt::Loop { body, .. } | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } | Stmt::Block(body) => strip(body),
                Stmt::Switch { sections, default, .. } => {
                    for (_, x) in sections.iter_mut() {
                        strip(x);
                    }
                    if let Some(x) = default {
                        strip(x);
                    }
                }
                _ => {}
            }
        }
    }
    strip(&mut b);
    b
}

fn collect_atomics(b: &Block, out: &mut Vec<(usize, Span, Block)>) {
    for s in &b.stmts {
        match s {
            Stmt::Atomic { body, id, span } => {
                out.push((*id, *span, body.clone()));
                collect_atomics(body, out);
            }
            Stmt::If { then, els, .. } => {
                collect_atomics(then, out);
                if let Some(e) = els {
                    collect_atomics(e, out);
                }
            }
            Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } => collect_atomics(body, out),
            Stmt::Switch { sections, default, .. } => {
                for (_, b) in sections {
                    collect_atomics(b, out);
                }
                if let Some(d) = default {
                    collect_atomics(d, out);
                }
            }
            _ => {}
        }
    }
}

/// W0201: reading a field of another entity after deferring a write to that field
/// in the same function — the read observes the value from the start of the system.
fn read_after_deferred_write(p: &Program, d: &mut Diags) {
    for f in &p.funcs {
        let mut written: BTreeSet<FieldId> = BTreeSet::new();
        let mut warned: BTreeSet<FieldId> = BTreeSet::new();
        rad_block(&f.body, &mut written, &mut warned, p, d);
    }
}

fn rad_block(b: &Block, written: &mut BTreeSet<FieldId>, warned: &mut BTreeSet<FieldId>, p: &Program, d: &mut Diags) {
    for s in &b.stmts {
        match s {
            Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => rad_expr(e, written, warned, p, d),
            Stmt::If { cond, then, els } => {
                rad_expr(cond, written, warned, p, d);
                rad_block(then, written, warned, p, d);
                if let Some(e) = els {
                    rad_block(e, written, warned, p, d);
                }
            }
            Stmt::Loop { cond, step, body, .. } => {
                if let Some(c) = cond {
                    rad_expr(c, written, warned, p, d);
                }
                rad_block(body, written, warned, p, d);
                for e in step {
                    rad_expr(e, written, warned, p, d);
                }
            }
            Stmt::ForQuery { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => rad_block(body, written, warned, p, d),
            Stmt::ForList { list, body, .. } => {
                rad_expr(list, written, warned, p, d);
                rad_block(body, written, warned, p, d);
            }
            Stmt::Switch { scrut, sections, default, .. } => {
                rad_expr(scrut, written, warned, p, d);
                for (c, b) in sections {
                    rad_expr(c, written, warned, p, d);
                    rad_block(b, written, warned, p, d);
                }
                if let Some(x) = default {
                    rad_block(x, written, warned, p, d);
                }
            }
            _ => {}
        }
    }
}

fn rad_expr(e: &Expr, written: &mut BTreeSet<FieldId>, warned: &mut BTreeSet<FieldId>, p: &Program, d: &mut Diags) {
    match &e.kind {
        ExprKind::Assign { target, value, .. } => {
            rad_expr(value, written, warned, p, d);
            if let Some((f, false)) = target_field(target) {
                written.insert(f);
            }
            return;
        }
        ExprKind::IncDec { target, .. } => {
            if let Some((f, false)) = target_field(target) {
                written.insert(f);
            }
            return;
        }
        ExprKind::EField { base, field } if !matches!(base.kind, ExprKind::SelfRef) => {
            if written.contains(field) && warned.insert(*field) {
                d.warn_note(
                    "W0201",
                    e.span,
                    format!("reading '{}' of another entity after writing it in the same system", p.field_label(*field)),
                    "writes to other entities are applied at the end of the system; this read sees the value from the start of the system",
                );
            }
        }
        _ => {}
    }
    for c in crate::sema::children(e) {
        rad_expr(c, written, warned, p, d);
    }
}
