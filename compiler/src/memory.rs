//! Compile-time memory planning: escape analysis assigns every list allocation to a region
//! (Frame arena, entity-owned Heap, or the caller's region) and infers reserve capacities.

use crate::ast::BinOp;
use crate::diag::Span;
use crate::hir::*;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Region {
    Frame,
    Caller,
    Heap,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cap {
    Const(i64),
    Count(ClassId),
}

#[derive(Clone, Debug)]
pub struct SitePlan {
    pub func: FuncId,
    pub span: Span,
    pub region: Region,
    pub reason: String,
    pub reserve: Vec<Cap>,
}

#[derive(Debug, Default)]
pub struct MemPlan {
    /// indexed by alloc site id
    pub sites: Vec<SitePlan>,
    /// region of list-returning calls, keyed by (caller, call span)
    pub calls: HashMap<(FuncId, Span), Region>,
    /// region of each list local (for drop decisions)
    pub locals: HashMap<(FuncId, LocalId), Region>,
    /// functions that receive a hidden region parameter
    pub region_param: Vec<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Dest {
    Temp,
    Local(LocalId),
    Field,
    Return,
}

enum Producer {
    Site(usize),
    Call(Span),
}

struct FnMem {
    parent: Vec<usize>,
    producers: Vec<(Producer, Dest)>,
    escapes: Vec<Region>,
}

impl FnMem {
    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            let r = self.find(self.parent[x]);
            self.parent[x] = r;
        }
        self.parent[x]
    }
    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra] = rb;
            let m = self.escapes[ra].max(self.escapes[rb]);
            self.escapes[rb] = m;
        }
    }
    fn escape(&mut self, l: usize, r: Region) {
        let root = self.find(l);
        if r > self.escapes[root] {
            self.escapes[root] = r;
        }
    }
}

pub fn plan(p: &Program) -> MemPlan {
    let mut mp = MemPlan {
        sites: p
            .alloc_sites
            .iter()
            .map(|s| SitePlan { func: s.func, span: s.span, region: Region::Frame, reason: String::new(), reserve: vec![] })
            .collect(),
        calls: HashMap::new(),
        locals: HashMap::new(),
        region_param: p.funcs.iter().map(|f| f.ret.is_list()).collect(),
    };
    for (fid, f) in p.funcs.iter().enumerate() {
        let n = f.locals.len();
        let mut fm = FnMem { parent: (0..n).collect(), producers: vec![], escapes: vec![Region::Frame; n] };
        // a list written through a ref/out parameter lives in the caller's storage, whose
        // lifetime is unknown here: allocate it on the heap (the caller frees it as usual)
        // an `own` parameter's collection came from the heap (see the call side below)
        for &l in &f.params {
            if f.locals[l].ty.is_list() && matches!(f.locals[l].kind, LocalKind::Param(crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out | crate::ast::ParamMode::Own)) {
                fm.escapes[l] = Region::Heap;
            }
        }
        walk_block(&f.body, &mut fm);
        // resolve
        let mut local_region = vec![Region::Frame; n];
        for l in 0..n {
            let r = fm.find(l);
            local_region[l] = fm.escapes[r];
            if f.locals[l].ty.is_list() {
                mp.locals.insert((fid, l), fm.escapes[r]);
            }
        }
        let producers = std::mem::take(&mut fm.producers);
        for (prod, dest) in producers {
            let (region, reason) = match dest {
                Dest::Temp => (Region::Frame, "temporary, never stored".to_string()),
                Dest::Field => (Region::Heap, "stored in an entity field: owned by the entity, freed on Destroy".to_string()),
                Dest::Return => (Region::Caller, "returned: allocated in the caller's region".to_string()),
                Dest::Local(l) => match local_region[l] {
                    Region::Frame => (Region::Frame, format!("local '{}' does not escape the tick", f.locals[l].name)),
                    Region::Heap => (Region::Heap, format!("local '{}' is moved into an entity field", f.locals[l].name)),
                    Region::Caller => (Region::Caller, format!("local '{}' is returned to the caller", f.locals[l].name)),
                },
            };
            match prod {
                Producer::Site(s) => {
                    mp.sites[s].region = region;
                    mp.sites[s].reason = reason;
                    if let Dest::Local(l) = dest {
                        mp.sites[s].reserve = infer_capacity(&f.body, l);
                    }
                }
                Producer::Call(sp) => {
                    mp.calls.insert((fid, sp), region);
                }
            }
        }
    }
    mp
}

fn walk_block(b: &Block, fm: &mut FnMem) {
    for s in &b.stmts {
        match s {
            Stmt::Let { local, init: Some(e) } => {
                if e.ty.is_list() {
                    if let ExprKind::Local(m) = e.kind {
                        fm.union(*local, m);
                    }
                }
                walk_expr(e, Dest::Local(*local), fm);
            }
            Stmt::Expr(e) => walk_expr(e, Dest::Temp, fm),
            Stmt::Return(Some(e), _) => {
                if let ExprKind::Local(m) = e.kind {
                    if e.ty.is_list() {
                        fm.escape(m, Region::Caller);
                    }
                }
                walk_expr(e, Dest::Return, fm);
            }
            Stmt::If { cond, then, els } => {
                walk_expr(cond, Dest::Temp, fm);
                walk_block(then, fm);
                if let Some(e) = els {
                    walk_block(e, fm);
                }
            }
            Stmt::Loop { cond, step, body, .. } => {
                if let Some(c) = cond {
                    walk_expr(c, Dest::Temp, fm);
                }
                for e in step {
                    walk_expr(e, Dest::Temp, fm);
                }
                walk_block(body, fm);
            }
            Stmt::ForList { list, body, .. } => {
                walk_expr(list, Dest::Temp, fm);
                walk_block(body, fm);
            }
            Stmt::ForQuery { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => walk_block(body, fm),
            Stmt::Switch { scrut, sections, default, .. } => {
                walk_expr(scrut, Dest::Temp, fm);
                for (c, b) in sections {
                    walk_expr(c, Dest::Temp, fm);
                    walk_block(b, fm);
                }
                if let Some(d) = default {
                    walk_block(d, fm);
                }
            }
            _ => {}
        }
    }
}

/// where a collection stored into an element of `place` lives: the region of the outermost
/// collection (a local or an entity field)
fn elem_dest(place: &Expr) -> Dest {
    match &place.kind {
        ExprKind::Local(l) => Dest::Local(*l),
        ExprKind::EField { .. } => Dest::Field,
        ExprKind::ListIndex { list, .. } => elem_dest(list),
        ExprKind::Field { base, .. } => elem_dest(base),
        _ => Dest::Temp,
    }
}

/// a collection value `x` flows into storage `dest`: unify or escape its local
fn flow(x: &Expr, dest: Dest, fm: &mut FnMem) {
    if let (ExprKind::Local(m), true) = (&x.kind, x.ty.is_list()) {
        match dest {
            Dest::Local(l) => fm.union(*m, l),
            Dest::Field => fm.escape(*m, Region::Heap),
            Dest::Return => fm.escape(*m, Region::Caller),
            Dest::Temp => {}
        }
    }
}

fn walk_expr(e: &Expr, dest: Dest, fm: &mut FnMem) {
    match &e.kind {
        // nested collections: an element list goes where its outer collection goes
        ExprKind::ListCall { list, op: ListOp::Add | ListOp::Insert, args } if args.iter().any(|a| a.ty.is_list()) => {
            let d = elem_dest(list);
            walk_expr(list, Dest::Temp, fm);
            for a in args {
                if a.ty.is_list() {
                    flow(a, d, fm);
                    walk_expr(a, d, fm);
                } else {
                    walk_expr(a, Dest::Temp, fm);
                }
            }
        }
        ExprKind::Assign { target, value, .. } if value.ty.is_list() && matches!(target.kind, ExprKind::ListIndex { .. }) => {
            let d = elem_dest(target);
            flow(value, d, fm);
            walk_expr(target, Dest::Temp, fm);
            walk_expr(value, d, fm);
        }
        ExprKind::ListNew { site } => fm.producers.push((Producer::Site(*site), dest)),
        ExprKind::ListLambda { op, site, list, body, args, .. } => {
            if op.makes_list() {
                fm.producers.push((Producer::Site(*site), dest));
            }
            walk_expr(list, Dest::Temp, fm);
            for a in args {
                walk_expr(a, Dest::Temp, fm);
            }
            walk_expr(body, Dest::Temp, fm);
        }
        ExprKind::ArrayNew { site, len, elems, dims } => {
            fm.producers.push((Producer::Site(*site), dest));
            for l in len.iter().map(|l| &**l).chain(dims.iter()) {
                walk_expr(l, Dest::Temp, fm);
            }
            for x in elems {
                // { new int[2], row }: the element collections live with the array
                flow(x, dest, fm);
                walk_expr(x, if x.ty.is_list() { dest } else { Dest::Temp }, fm);
            }
        }
        ExprKind::ListClone { list, site } => {
            fm.producers.push((Producer::Site(*site), dest));
            walk_expr(list, Dest::Temp, fm);
        }
        ExprKind::Call { recv, args, .. } => {
            if e.ty.is_list() {
                fm.producers.push((Producer::Call(e.span), dest));
            }
            if let Some(r) = recv {
                walk_expr(r, Dest::Temp, fm);
            }
            for a in args {
                // an `own` parameter may keep what it receives (store it in a field): heap
                if a.mode == crate::ast::ParamMode::Own {
                    flow(&a.expr, Dest::Field, fm);
                    walk_expr(&a.expr, Dest::Field, fm);
                } else {
                    walk_expr(&a.expr, Dest::Temp, fm);
                }
            }
        }
        ExprKind::Assign { target, value, .. } => {
            let d = match &target.kind {
                ExprKind::Local(l) => {
                    if let ExprKind::Local(m) = value.kind {
                        if value.ty.is_list() {
                            fm.union(*l, m);
                        }
                    }
                    Dest::Local(*l)
                }
                ExprKind::EField { .. } => {
                    if let ExprKind::Local(m) = value.kind {
                        if value.ty.is_list() {
                            fm.escape(m, Region::Heap);
                        }
                    }
                    Dest::Field
                }
                _ => Dest::Temp,
            };
            walk_expr(target, Dest::Temp, fm);
            walk_expr(value, d, fm);
        }
        ExprKind::Spawn { inits, .. } => {
            for (_, x) in inits {
                if let ExprKind::Local(m) = x.kind {
                    if x.ty.is_list() {
                        fm.escape(m, Region::Heap);
                    }
                }
                walk_expr(x, Dest::Field, fm);
            }
        }
        _ => {
            for c in crate::sema::children(e) {
                walk_expr(c, Dest::Temp, fm);
            }
        }
    }
}

/// Capacity inference: unconditional-or-not `L.Add(..)` directly inside `foreach Query<T>` or
/// a constant-bounded `for` loop reserves the loop's trip count (an upper bound; over-reserving
/// in the frame arena is free because the arena is reset each tick).
fn infer_capacity(body: &Block, l: LocalId) -> Vec<Cap> {
    let mut caps = vec![];
    cap_block(body, l, &mut caps);
    caps
}

fn adds_directly(b: &Block, l: LocalId) -> bool {
    // Add calls in this loop body, not inside nested loops
    let mut found = false;
    for s in &b.stmts {
        match s {
            Stmt::Loop { .. } | Stmt::ForQuery { .. } | Stmt::ForList { .. } => {}
            Stmt::If { cond, then, els } => {
                crate::sema::visit_expr(cond, &mut |e| found |= is_add(e, l));
                found |= adds_directly(then, l) || els.as_ref().map(|e| adds_directly(e, l)).unwrap_or(false);
            }
            Stmt::Block(x) | Stmt::Atomic { body: x, .. } => found |= adds_directly(x, l),
            other => crate::sema::visit_stmt(other, &mut |e| found |= is_add(e, l)),
        }
    }
    found
}

fn is_add(e: &Expr, l: LocalId) -> bool {
    matches!(&e.kind, ExprKind::ListCall { list, op: ListOp::Add, .. } if matches!(list.kind, ExprKind::Local(x) if x == l))
}

fn cap_block(b: &Block, l: LocalId, caps: &mut Vec<Cap>) {
    for (k, s) in b.stmts.iter().enumerate() {
        match s {
            Stmt::ForQuery { class, body, .. } => {
                if adds_directly(body, l) {
                    caps.push(Cap::Count(*class));
                } else {
                    cap_block(body, l, caps);
                }
            }
            Stmt::Loop { cond: Some(c), body, do_while: false, .. } => {
                let mut done = false;
                if k > 0 {
                    if let Stmt::Let { local: i, init: Some(Expr { kind: ExprKind::Int(a), .. }) } = &b.stmts[k - 1] {
                        if let ExprKind::Binary(BinOp::Lt, lhs, rhs) = &c.kind {
                            if let (ExprKind::Local(x), ExprKind::Int(n)) = (&lhs.kind, &rhs.kind) {
                                if x == i && adds_directly(body, l) && n > a {
                                    caps.push(Cap::Const(n - a));
                                    done = true;
                                }
                            }
                        }
                    }
                }
                if !done {
                    cap_block(body, l, caps);
                }
            }
            Stmt::If { then, els, .. } => {
                cap_block(then, l, caps);
                if let Some(e) = els {
                    cap_block(e, l, caps);
                }
            }
            Stmt::Block(x) | Stmt::Atomic { body: x, .. } | Stmt::Loop { body: x, .. } | Stmt::ForList { body: x, .. } => cap_block(x, l, caps),
            Stmt::Switch { sections, default, .. } => {
                for (_, x) in sections {
                    cap_block(x, l, caps);
                }
                if let Some(x) = default {
                    cap_block(x, l, caps);
                }
            }
            _ => {}
        }
    }
}
