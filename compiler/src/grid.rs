//! Grids: the cell-parallel operations of 2D and 3D arrays (`Grid<T>` is `T[,]`, `Grid3<T>` is
//! `T[,,]`; docs/01 §1.14.2, docs/04 §4.13).
//!
//! `g.Step((x, y, c) => v)` computes every cell's new value from the grid as it was before the step
//! (reads of `g` in the lambda see the old cells, as the other entities of a system do), so cells can
//! be computed in any order: in parallel, with the same bits. The compiler looks at the lambda:
//! - a *map* reads no other cell (only `c`): the step writes in place;
//! - a *stencil* reads `g[x + i, y + j]` with constant offsets: the new cells go to a second buffer,
//!   and the cells at least the largest offset away from the edges (the interior) are computed
//!   without bounds checks (the edge cells keep them);
//! - other reads of `g` (computed coordinates) keep their checks.
//!
//! `g.Move(empty, (x, y, c) => Grid.Dir(dx, dy))` moves cells into free neighbouring cells: every
//! cell proposes a direction (or `Grid.Stay`), and a free cell (equal to `empty`) takes the first
//! proposer in a fixed order of directions (straight moves before diagonal ones), the cell it came
//! from becoming `empty`. Each cell's outcome follows from the old grid alone (the proposals of its
//! neighbours are computed again where needed), so a move is parallel and deterministic as well.
//!
//! `Random` in these lambdas is keyed by the cell (and the step): the same numbers on any thread.

use crate::diag::{Diags, Span};
use crate::driver::Compiled;
use crate::effects::EffectInfo;
use crate::hir::*;

/// What a Step's lambda reads of its grid.
#[derive(Clone, Debug, Default)]
pub struct Shape {
    /// reads cells of the grid other than through `c`
    pub reads_grid: bool,
    /// the largest constant offset read in each dimension
    pub radius: [i64; 3],
    /// reads with coordinates that are not `x + constant` (they keep their bounds checks)
    pub other_reads: usize,
    /// constant-offset reads (computed without bounds checks in the interior)
    pub offset_reads: usize,
    /// draws random numbers (keyed by the cell)
    pub random: bool,
}

/// the same grid: a local, or a field of this entity
pub fn same_grid(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Local(x), ExprKind::Local(y)) => x == y,
        (ExprKind::EField { base: ba, field: fa }, ExprKind::EField { base: bb, field: fb }) => {
            fa == fb && matches!((&ba.kind, &bb.kind), (ExprKind::SelfRef, ExprKind::SelfRef))
        }
        _ => false,
    }
}

/// `p`, `p + k`, `k + p` or `p - k` with a constant k: Some(k)
pub fn offset(e: &Expr, p: LocalId) -> Option<i64> {
    let is_p = |x: &Expr| matches!(x.kind, ExprKind::Local(l) if l == p);
    let k = |x: &Expr| if let ExprKind::Int(n) = x.kind { Some(n) } else { None };
    match &e.kind {
        ExprKind::Local(l) if *l == p => Some(0),
        ExprKind::Binary(crate::ast::BinOp::Add, a, b) if is_p(a) => k(b),
        ExprKind::Binary(crate::ast::BinOp::Add, a, b) if is_p(b) => k(a),
        ExprKind::Binary(crate::ast::BinOp::Sub, a, b) if is_p(a) => k(b).map(|n| -n),
        _ => None,
    }
}

/// the constant offsets of a read `g[...]` relative to the cell (x, y, z), if it is one
fn read_offsets(idx: &[Expr], params: &[LocalId]) -> Option<Vec<i64>> {
    idx.iter().enumerate().map(|(d, x)| params.get(d).and_then(|&p| offset(x, p))).collect()
}

/// the offsets of `Get(x + i, y + j[, z + k], fallback)` when the fallback has no effects
fn get_offsets(args: &[Expr], params: &[LocalId], rank: usize) -> Option<Vec<i64>> {
    if args.len() != rank + 2 || !crate::opt::is_pure_expr(&args[rank + 1]) {
        return None;
    }
    read_offsets(&args[1..1 + rank], &params[..rank.min(params.len())])
}

pub fn shape(grid: &Expr, params: &[LocalId], body: &Expr) -> Shape {
    let rank = grid.ty.rank();
    let mut s = Shape::default();
    crate::sema::visit_expr(body, &mut |e| match &e.kind {
        ExprKind::ListIndex { list, index, .. } if same_grid(list, grid) => {
            s.reads_grid = true;
            match &index.kind {
                ExprKind::MdIndex { idx, .. } => match read_offsets(idx, &params[..rank.min(params.len())]) {
                    Some(off) => {
                        s.offset_reads += 1;
                        for (d, k) in off.iter().enumerate() {
                            s.radius[d] = s.radius[d].max(k.abs());
                        }
                    }
                    None => s.other_reads += 1,
                },
                _ => s.other_reads += 1,
            }
        }
        // Get(x + i, y + j, fallback): a constant-offset read too (the fallback is for the edges)
        ExprKind::Builtin { f: Builtin::GridGet, args, .. } if same_grid(&args[0], grid) => {
            s.reads_grid = true;
            match get_offsets(args, params, rank) {
                Some(off) => {
                    s.offset_reads += 1;
                    for (d, k) in off.iter().enumerate() {
                        s.radius[d] = s.radius[d].max(k.abs());
                    }
                }
                None => s.other_reads += 1,
            }
        }
        ExprKind::Builtin { f: Builtin::GridInBounds, args, .. } if same_grid(&args[0], grid) => {
            s.reads_grid = true;
            s.other_reads += 1;
        }
        ExprKind::Builtin { f: Builtin::RandomFloat | Builtin::RandomInt, .. } => s.random = true,
        // any other use of the grid (passed to a function, its length...) reads it as a whole
        ExprKind::Local(_) | ExprKind::EField { .. } if same_grid(e, grid) => s.reads_grid = true,
        _ => {}
    });
    s
}

/// The body for the interior cells: reads at constant offsets need no bounds checks there (the cell
/// is at least `radius` away from every edge).
pub fn interior_body(grid: &Expr, params: &[LocalId], body: &Expr) -> Expr {
    let rank = grid.ty.rank();
    let mut b = body.clone();
    crate::opt::visit_expr_mut(&mut b, &mut |e| {
        // Get at a constant offset is an unchecked read in the interior
        if let ExprKind::Builtin { f: Builtin::GridGet, args, .. } = &e.kind {
            if same_grid(&args[0], grid) && get_offsets(args, params, rank).is_some() {
                let idx: Vec<Expr> = args[1..1 + rank].to_vec();
                let md = Expr::new(ExprKind::MdIndex { idx, checked: vec![false; rank] }, Ty::Int, e.span);
                e.kind = ExprKind::ListIndex { list: Box::new(args[0].clone()), index: Box::new(md), checked: false };
                return;
            }
        }
        if let ExprKind::ListIndex { list, index, .. } = &mut e.kind {
            if same_grid(list, grid) {
                if let ExprKind::MdIndex { idx, checked } = &mut index.kind {
                    if read_offsets(idx, &params[..rank.min(params.len())]).is_some() {
                        for c in checked.iter_mut() {
                            *c = false;
                        }
                    }
                }
            }
        }
    });
    b
}

/// The move directions of a grid of `rank` dimensions in the order a free cell picks among the cells
/// that want it: straight moves first (+y, -y, +x, -x, +z, -z), then two-axis diagonals, then
/// three-axis ones; among equals +y before -y, then +x before -x, then +z before -z.
pub fn move_dirs(rank: usize) -> Vec<[i64; 3]> {
    let mut v = vec![];
    for dz in [-1i64, 0, 1] {
        for dy in [-1i64, 0, 1] {
            for dx in [-1i64, 0, 1] {
                if (dx, dy, dz) == (0, 0, 0) || (rank < 3 && dz != 0) {
                    continue;
                }
                v.push([dx, dy, dz]);
            }
        }
    }
    let r = |k: i64| match k {
        1 => 0,
        -1 => 1,
        _ => 2,
    };
    v.sort_by_key(|d| (d.iter().filter(|k| **k != 0).count(), r(d[1]), r(d[0]), r(d[2])));
    v
}

/// the number Grid.Dir(dx, dy, dz) stands for
pub fn dir_code(d: [i64; 3]) -> i64 {
    if d == [0, 0, 0] {
        0
    } else {
        1 + (d[0] + 1) + 3 * (d[1] + 1) + 9 * (d[2] + 1)
    }
}

/// The lambdas of Step, Move and Ray compute a value from the grid and the program's state: they
/// cannot change anything, print, spawn or call what does (each cell may run on another thread,
/// and Move computes a proposal more than once). Random draws are allowed (keyed by the cell).
pub fn check(p: &Program, info: &EffectInfo, d: &mut Diags) {
    for f in &p.funcs {
        crate::sema::visit_block(&f.body, &mut |e| {
            let ExprKind::ListLambda { op, body, .. } = &e.kind else { return };
            if !op.is_grid() {
                return;
            }
            let what = match op {
                LamOp::GridStep => "Step",
                LamOp::GridMove => "Move",
                _ => "Ray",
            };
            let mut bad: Option<(Span, String)> = None;
            crate::sema::visit_expr(body, &mut |x| {
                if bad.is_some() {
                    return;
                }
                let why = match &x.kind {
                    ExprKind::Assign { .. } | ExprKind::IncDec { .. } => Some("it assigns".to_string()),
                    ExprKind::ListCall { op, .. } if op.mutates() => Some("it changes a list".into()),
                    ExprKind::ListLambda { op, .. } if op.mutates() || op.is_grid() => Some("it runs a list or grid operation that changes something or runs per cell itself".into()),
                    ExprKind::DictSet { .. } => Some("it changes a dictionary".into()),
                    ExprKind::Spawn { .. } | ExprKind::Destroy(_) | ExprKind::CompAdd { .. } | ExprKind::CompRemove { .. } => Some("it spawns, destroys or changes entities".into()),
                    ExprKind::Builtin { f: Builtin::Print | Builtin::DrawSprite | Builtin::DrawText | Builtin::NetView | Builtin::NetClearView, .. } => Some("it prints or draws (output happens in order, cells do not)".into()),
                    ExprKind::Call { func, dispatch, .. } => {
                        let g = &p.funcs[*func];
                        // an entity method's effects are per archetype: all of them
                        let mut e = info.get(*func, None);
                        if let FuncKind::EntityMethod(cl) = g.kind {
                            for a in p.archetypes_of(cl) {
                                e.merge(&info.get(p.target_of(a, *func), Some(a)));
                            }
                        }
                        if g.extern_name.is_some() && !g.pure_attr {
                            Some(format!("it calls the host function '{}' (only [Pure] ones)", p.func_label(*func)))
                        } else if matches!(dispatch, Dispatch::RemoteDeferred | Dispatch::Rpc) || e.mutates() || e.prints || !e.spawns.is_empty() || !e.host_queue.is_empty() {
                            Some(format!("it calls '{}', which changes or outputs something", p.func_label(*func)))
                        } else if e.random {
                            Some(format!("it calls '{}', which draws random numbers from the entity's sequence (draw them in the lambda: they are keyed by the cell there)", p.func_label(*func)))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(w) = why {
                    bad = Some((x.span, w));
                }
            });
            if let Some((sp, why)) = bad {
                d.error_note(
                    "E0707",
                    sp,
                    format!("the lambda of {} computes a value for each cell and cannot have effects: {}", what, why),
                    "cells are computed in any order, on several threads; compute what the cells need before the call, and act on the result after it",
                );
            }
        });
    }
}

/// `sijoc explain`: every grid operation and how it runs
pub fn report(c: &Compiled, sm: &crate::diag::SourceMap) -> String {
    let p = &c.prog;
    let mut o = String::new();
    for (fid, f) in p.funcs.iter().enumerate() {
        crate::sema::visit_block(&f.body, &mut |e| {
            let ExprKind::ListLambda { list, op, params, body, .. } = &e.kind else { return };
            if !op.is_grid() {
                return;
            }
            let (file, line, _) = sm.position(e.span.lo);
            let at = if sm.files.len() > 1 { format!("{}:{}", file, line) } else { format!("line {}", line) };
            let rank = list.ty.rank();
            let s = shape(list, params, body);
            let rad: Vec<String> = s.radius[..rank].iter().map(|r| r.to_string()).collect();
            let rnd = if s.random { "; Random keyed by the cell" } else { "" };
            let text = match op {
                LamOp::GridStep if !s.reads_grid => format!("Step: map (each cell from itself), in place, cells in parallel{rnd}"),
                LamOp::GridStep => format!(
                    "Step: stencil (reads neighbours at offsets up to {}), into a second buffer, cells in parallel; {} read(s) without bounds checks away from the edges{}{rnd}",
                    rad.join(" x "),
                    s.offset_reads,
                    if s.other_reads > 0 { format!(", {} read(s) at computed cells keep their checks", s.other_reads) } else { String::new() }
                ),
                LamOp::GridMove => format!("Move: proposals to the {} neighbours resolved per cell (straight moves first), into a second buffer, cells in parallel{rnd}", if rank == 3 { 26 } else { 8 }),
                _ => format!("Ray: walks the cells of the line until the hit test holds{rnd}"),
            };
            o.push_str(&format!("  {}: {} ({}D grid): {}\n", p.func_label(fid), at, rank, text));
        });
    }
    if o.is_empty() {
        "  (no Step, Move or Ray on a grid)\n".into()
    } else {
        o
    }
}
