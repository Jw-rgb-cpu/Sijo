//! C code for grid operations (src/grid.rs): Step, Move and Ray inline their lambda into loops over
//! the cells; Flood, InBounds, Get and Grid.Dir are runtime calls (runtime/sijo_grid.h).
//!
//! Step and Move write a new buffer (a map writes in place) and run the outermost dimension in
//! parallel when the grid is large and no parallel region is open yet; each thread catches its own
//! panics as a parallel system does. The lambda's parameters are the function's locals: the parallel
//! region makes them private to each thread.

use super::*;

impl<'a> Gen<'a> {
    pub(super) fn grid_lambda(&mut self, e: &Expr, list: &Expr, op: LamOp, params: &[LocalId], body: &Expr, args: &[Expr], cx: &mut FCtx) -> String {
        let el = self.p.elem_of(&list.ty).unwrap_or(Ty::Int);
        let et = self.cty(&el);
        let t = self.t();
        let names: Vec<String> = params.iter().map(|&l| self.local_ref(cx, l)).collect();
        // the operation's place in the entity's random sequence is named by its source position
        let site = (e.span.lo as u64).wrapping_mul(2654435761) as u32 ^ 0x4752_4944;
        // Random in the lambda: keyed by this operation (one draw of the entity's sequence), the cell
        // and the draw's number within the cell
        let shape = crate::grid::shape(list, params, body);
        let (key, ri, rc) = (format!("{t}k"), format!("{t}ri"), format!("{t}rc"));
        let saved = cx.cell_rng.replace((key.clone(), ri.clone(), rc.clone()));
        let out = match op {
            LamOp::GridRay => self.grid_ray(list, &names, body, args, &t, &et, cx),
            _ => self.grid_cells(list, op, params, &names, body, args, &t, &el, &et, &shape, cx),
        };
        cx.cell_rng = saved;
        let rng = if shape.random { format!("uint64_t {key} = sijo_rng_next(w, {site}u); uint32_t {ri} = 0, {rc} = 0; (void){ri}; (void){rc}; ") } else { format!("uint32_t {ri} = 0, {rc} = 0; (void){ri}; (void){rc}; ") };
        format!("({{ {rng}{out} }})")
    }

    /// Ray(x0, y0, x1, y1, c => hit): the n + 1 cells (n = the longest axis distance) of the line,
    /// rounded to the nearest cell, from the start until the hit test holds (its step) or the line
    /// ends or leaves the grid (-1)
    #[allow(clippy::too_many_arguments)]
    fn grid_ray(&mut self, list: &Expr, names: &[String], body: &Expr, args: &[Expr], t: &str, et: &str, cx: &mut FCtx) -> String {
        let rank = list.ty.rank();
        let g = self.expr(list, cx);
        let mut o = format!("SijoList {t}o = {g}; ");
        let vs: Vec<String> = args.iter().map(|a| self.expr(a, cx)).collect();
        for (i, v) in vs.iter().enumerate() {
            let _ = write!(o, "int64_t {t}a{i} = {v}; ");
        }
        let b = self.expr(body, cx);
        let c = &names[0];
        let dims: Vec<String> = (0..rank).map(|d| format!("{t}o.d{d}")).collect();
        let _ = write!(o, "int64_t {t}n = 0; ");
        for d in 0..rank {
            let _ = write!(o, "int64_t {t}e{d} = {t}a{} - {t}a{d}; if (({t}e{d} < 0 ? -{t}e{d} : {t}e{d}) > {t}n) {t}n = {t}e{d} < 0 ? -{t}e{d} : {t}e{d}; ", d + rank);
        }
        let _ = write!(o, "int32_t {t}r = -1; for (int64_t {t}s = 0; {t}s <= {t}n; {t}s++) {{ ");
        for d in 0..rank {
            // nearest cell, halves away from zero: exact integer arithmetic
            let _ = write!(o, "int64_t {t}p{d} = {t}a{d} + ({t}n ? (2 * {t}s * {t}e{d} + ({t}e{d} < 0 ? -{t}n : {t}n)) / (2 * {t}n) : 0); if ({t}p{d} < 0 || {t}p{d} >= {}) break; ", dims[d]);
        }
        let idx = (1..rank).fold(format!("{t}p0"), |acc, d| format!("({acc} * {} + {t}p{d})", dims[d]));
        let (ri, rc) = (cx.cell_rng.as_ref().unwrap().1.clone(), cx.cell_rng.as_ref().unwrap().2.clone());
        let _ = write!(o, "{ri} = (uint32_t)({idx}); {rc} = 0; {c} = (({et}*){t}o.data)[{idx}]; if ({b}) {{ {t}r = (int32_t){t}s; break; }} }} {t}r;");
        o
    }

    /// Step and Move: every cell from the grid as it was
    #[allow(clippy::too_many_arguments)]
    fn grid_cells(&mut self, list: &Expr, op: LamOp, params: &[LocalId], names: &[String], body: &Expr, args: &[Expr], t: &str, el: &Ty, et: &str, shape: &crate::grid::Shape, cx: &mut FCtx) -> String {
        let rank = list.ty.rank();
        let gp = self.place(list, cx);
        let in_place = op == LamOp::GridStep && !shape.reads_grid;
        // the interior: the cells at least the largest read offset away from every edge
        let rad = if op == LamOp::GridStep { shape.radius } else { [1, 1, 1] };
        let interior_ok = shape.offset_reads > 0;
        let border_body = self.expr(body, cx);
        let interior_body = if interior_ok { self.expr(&crate::grid::interior_body(list, params, body), cx) } else { border_body.clone() };
        let (ri, rc) = (cx.cell_rng.as_ref().unwrap().1.clone(), cx.cell_rng.as_ref().unwrap().2.clone());
        let c = names[rank].clone();
        let xs: Vec<String> = names[..rank].to_vec();
        // the loops run on variables of their own: computing a cell assigns the lambda's parameters
        // (Move computes neighbours' proposals too)
        let lv: Vec<String> = (0..rank).map(|d| format!("{t}L{d}")).collect();
        let dim = |d: usize| if d < rank { format!("{t}d{d}") } else { "1".to_string() };
        let pos = |v: &[String]| (1..rank).fold(format!("({})", v[0]), |acc, d| format!("({acc} * {t}d{d} + {})", v[d]));
        let mut o = format!("SijoList* {t}g = &({gp}); SijoList {t}o = *{t}g; ");
        for d in 0..rank {
            let _ = write!(o, "int32_t {t}d{d} = {t}o.d{d}; ");
        }
        if in_place {
            let _ = write!(o, "SijoList {t}n = {t}o; ");
        } else {
            let _ = write!(o, "SijoList {t}n = {t}o; {t}n.data = {t}o.len ? ({t}o.heap ? sijo_xrealloc(NULL, (size_t){t}o.len * sizeof({et})) : sijo_arena_alloc({}, (size_t){t}o.len * sizeof({et}))) : NULL; {t}n.cap = {t}o.len; ", self.arena());
        }
        let ocell = |i: &str| format!("(({et}*){t}o.data)[{i}]");
        let ncell = |i: &str| format!("(({et}*){t}n.data)[{i}]");
        // the code for one cell; `interior` picks the body without bounds checks
        let cell = |interior: bool, me: &Self| -> String {
            let b = if interior { &interior_body } else { &border_body };
            let i = format!("{t}i");
            if op == LamOp::GridStep {
                let set: String = (0..rank).map(|d| format!("{} = {}; ", xs[d], lv[d])).collect();
                return format!("{{ {set}int32_t {i} = {}; {ri} = (uint32_t){i}; {rc} = 0; {c} = {}; {} = {b}; }}", pos(&lv), ocell(&i), ncell(&i));
            }
            // Move: the proposal of cell (s...) with value sc
            let eval = |coords: &[String], idx: &str, val: &str| -> String {
                let mut a = String::new();
                for (d, x) in xs.iter().enumerate() {
                    let _ = write!(a, "{x} = {}; ", coords[d]);
                }
                format!("sijo_grid_dnorm(({{ {a}{c} = {val}; {ri} = (uint32_t)({idx}); {rc} = 0; {b}; }}))")
            };
            let _ = me;
            let dirs = crate::grid::move_dirs(rank);
            let nd = dirs.len();
            let inb = |v: &[String]| (0..rank).map(|d| format!("{} >= 0 && {} < {t}d{d}", v[d], v[d])).collect::<Vec<_>>().join(" && ");
            let src: Vec<String> = (0..rank).map(|d| format!("{t}s{d}")).collect();
            let tgt: Vec<String> = (0..rank).map(|d| format!("{t}t{d}")).collect();
            let decl_src = |from: &[String]| (0..rank).map(|d| format!("int32_t {t}s{d} = {} - {t}D{d}[{t}q];", from[d])).collect::<Vec<_>>().join(" ");
            let me_xs: Vec<String> = (0..rank).map(|d| format!("{t}m{d}")).collect();
            let mut s = format!("{{ ");
            for d in 0..rank {
                let _ = write!(s, "int32_t {t}m{d} = {}; ", lv[d]);
            }
            let _ = write!(s, "int32_t {i} = {}; {et} {t}c0 = {}; {et} {t}out = {t}c0; ", pos(&me_xs), ocell(&i));
            // a free cell takes the first neighbour that proposes to move into it
            let _ = write!(
                s,
                "if ({t}c0 == {t}E) {{ for (int {t}q = 0; {t}q < {nd}; {t}q++) {{ {} if (!({})) continue; int32_t {t}si = {}; {et} {t}sc = {}; if ({t}sc == {t}E) continue; if ({} == {t}DC[{t}q]) {{ {t}out = {t}sc; break; }} }} }} ",
                decl_src(&me_xs),
                inb(&src),
                pos(&src),
                ocell(&format!("{t}si")),
                eval(&src, &format!("{t}si"), &format!("{t}sc"))
            );
            // an occupied cell leaves when the free cell it proposes takes it
            let _ = write!(s, "else {{ int32_t {t}pd = {}; if ({t}pd != 0) {{ ", eval(&me_xs, &i, &format!("{t}c0")));
            let comps = ["sijo_grid_ddx", "sijo_grid_ddy", "sijo_grid_ddz"];
            for d in 0..rank {
                let _ = write!(s, "int32_t {t}t{d} = {t}m{d} + {}({t}pd); ", comps[d]);
            }
            let _ = write!(
                s,
                "if (({}) && {} == {t}E) {{ for (int {t}q = 0; {t}q < {nd}; {t}q++) {{ {} if (!({})) continue; int32_t {t}si = {}; {et} {t}sc = {}; if ({t}sc == {t}E) continue; int32_t {t}p2 = {t}si == {i} ? {t}pd : {}; if ({t}p2 == {t}DC[{t}q]) {{ if ({t}si == {i}) {t}out = {t}E; break; }} }} }} }} }} ",
                inb(&tgt),
                ocell(&pos(&tgt)),
                decl_src(&tgt),
                inb(&src),
                pos(&src),
                ocell(&format!("{t}si")),
                eval(&src, &format!("{t}si"), &format!("{t}sc"))
            );
            let _ = write!(s, "{} = {t}out; }}", ncell(&i));
            s
        };
        if op == LamOp::GridMove {
            let e0 = self.expr(&args[0], cx);
            let _ = write!(o, "{et} {t}E = {e0}; ");
            let dirs = crate::grid::move_dirs(rank);
            for d in 0..rank {
                let v: Vec<String> = dirs.iter().map(|x| x[d].to_string()).collect();
                let _ = write!(o, "static const int8_t {t}D{d}[] = {{{}}}; ", v.join(", "));
            }
            let codes: Vec<String> = dirs.iter().map(|x| crate::grid::dir_code(*x).to_string()).collect();
            let _ = write!(o, "static const int32_t {t}DC[] = {{{}}}; ", codes.join(", "));
        }
        let ci = cell(true, self);
        let cb = cell(false, self);
        // loops over the cells of outer rows lo..hi: the innermost dimension in three runs (edge,
        // interior, edge) so that the interior has no test per cell
        let inner = rank - 1;
        let loops = |lo: &str, hi: &str| -> String {
            let mut s = String::new();
            let mut conds = vec![];
            for d in 0..inner {
                let (from, to) = if d == 0 { (lo.to_string(), hi.to_string()) } else { ("0".to_string(), dim(d)) };
                let _ = write!(s, "for (int32_t {} = {from}; {} < {to}; {}++) {{ ", lv[d], lv[d], lv[d]);
                conds.push(format!("{} >= {} && {} < {} - {}", lv[d], rad[d], lv[d], dim(d), rad[d]));
            }
            let r = rad[inner];
            let x = &lv[inner];
            let _ = write!(
                s,
                "int {t}in = {}; int32_t {t}b1 = {t}in && {} > 2 * {r} ? {r} : {}, {t}b2 = {t}in && {} > 2 * {r} ? {} - {r} : {}; int32_t {x}; for ({x} = 0; {x} < {t}b1; {x}++) {cb} for ({x} = {t}b1; {x} < {t}b2; {x}++) {ci} for ({x} = {t}b2 > {t}b1 ? {t}b2 : {t}b1; {x} < {}; {x}++) {cb} ",
                if conds.is_empty() || !interior_ok { if interior_ok { "1".into() } else { "0".to_string() } } else { conds.join(" && ") },
                dim(inner),
                dim(inner),
                dim(inner),
                dim(inner),
                dim(inner),
                dim(inner)
            );
            for _ in 0..inner {
                s.push_str("} ");
            }
            s
        };
        let serial = loops("0", &format!("{t}d0"));
        let parallel = loops(&format!("{t}lo"), &format!("{t}hi"));
        let private = names.join(", ");
        let _ = write!(
            o,
            "\n#ifdef _OPENMP\nint {t}par = !omp_in_parallel() && w->nthreads > 1 && {t}o.len >= SIJO_GRID_PAR_MIN;\n#else\nint {t}par = 0;\n#endif\nif ({t}par) {{ SijoPanic* {t}pp = sijo_tl_panic; jmp_buf* {t}oc = sijo_tl_catch;\n#pragma omp parallel num_threads(w->nthreads) private({private}, {ri}, {rc})\n{{ int {t}t = sijo_omp_tid(), {t}nt = sijo_team(); int32_t {t}lo = (int32_t)((int64_t){t}d0 * {t}t / {t}nt), {t}hi = (int32_t)((int64_t){t}d0 * ({t}t + 1) / {t}nt); sijo_tl_tid = {t}t; jmp_buf {t}jb; volatile int {t}sk = 0; sijo_tl_panic = {t}pp; sijo_tl_catch = NULL; if ({t}oc) {{ if (setjmp({t}jb) != 0) {t}sk = 1; else sijo_tl_catch = &{t}jb; }} if (!{t}sk) {{ {parallel}}} sijo_tl_catch = NULL; sijo_tl_tid = 0; }}\nsijo_tl_catch = {t}oc; sijo_tl_panic = {t}pp; if ({t}oc && {t}pp && {t}pp->flag) longjmp(*{t}oc, 1); }} else {{ {serial}}} "
        );
        if !in_place {
            let _ = write!(o, "if ({t}o.heap) free({t}o.data); *{t}g = {t}n; ");
        }
        // a replicated field changed
        if let ExprKind::EField { base, field } = &list.kind {
            if matches!(base.kind, ExprKind::SelfRef) {
                if let Some(a) = cx.arch {
                    let k = self.comp(*field);
                    if self.c.ecs.components[k].replicated {
                        let _ = write!(o, "{}d{}[row] = 1; ", self.self_tab(cx, a), k);
                    }
                }
            }
        }
        let _ = el;
        o.push_str("(void)0;");
        o
    }
}
