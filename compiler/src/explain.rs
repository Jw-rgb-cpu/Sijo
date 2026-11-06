//! `sijoc explain`: a human-readable report of every planning decision and why it was made.

use crate::driver::Compiled;
use crate::ecs::SysKind;
use crate::effects::describe_effects;
use crate::hir::*;
use crate::memory::{Cap, Region};
use crate::net::Quant;
use std::fmt::Write;

pub fn report(c: &Compiled, sm: &crate::diag::SourceMap) -> String {
    let p = &c.prog;
    let mut o = String::new();
    let line = |sp: crate::diag::Span| {
        let (f, l, _) = sm.position(sp.lo);
        if sm.files.len() > 1 {
            format!("{}:{}", f, l)
        } else {
            l.to_string()
        }
    };
    let names: Vec<&str> = sm.files.iter().map(|f| f.path.as_str()).collect();
    let _ = writeln!(o, "== sijoc explain: {} ==\n", names.join(", "));

    // ---------------- optimizer
    let s = &c.opt;
    let _ = writeln!(o, "[optimizer]");
    let _ = writeln!(
        o,
        "  constants folded: {}, propagated: {}, compile-time calls (CTFE): {}, algebraic: {}, branches removed: {}, bounds checks removed: {}",
        s.folded, s.propagated, s.ctfe_calls, s.algebraic, s.branches, s.bounds_checks_removed
    );
    for ex in &s.ctfe_examples {
        let _ = writeln!(o, "  ctfe: {}", ex);
    }
    if s.egraph_exprs > 0 || s.egraph_branches > 0 || !s.clamp_need.is_empty() {
        // commutativity, associativity and the sub/add forms only reorder: show the others
        let mut rules: Vec<(&str, usize)> = s.egraph_rules.iter().copied().filter(|(n, _)| !n.starts_with("comm-") && !n.starts_with("assoc-") && !n.contains("-as-")).collect();
        rules.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let top: Vec<String> = rules.iter().take(10).map(|(n, k)| format!("{} {}", n, k)).collect();
        let _ = writeln!(
            o,
            "  e-graph (equality saturation): {} expression(s) replaced by a cheaper equal form ({} e-nodes); simplifying rules: {}",
            s.egraph_exprs,
            s.egraph_nodes,
            if top.is_empty() { "-".into() } else { top.join(", ") }
        );
        for (sp, ex) in &s.egraph_examples {
            let _ = writeln!(o, "    line {}: {}", line(*sp), ex);
        }
        let removed = s.clamp_need.values().filter(|n| !n.0 && !n.1).count();
        let narrowed = s.clamp_need.values().filter(|n| n.0 != n.1).count();
        let _ = writeln!(
            o,
            "  value ranges (range types, immutable locals, loop counters, array lengths): {} condition(s) decided, {} bounds check(s) removed, write saturations removed at {} site(s) and narrowed to one bound at {}",
            s.egraph_branches, s.range_bce, removed, narrowed
        );
    }
    for k in &p.consts {
        if let Some(v) = &k.value {
            let _ = writeln!(o, "  const {} = {}", k.name, crate::opt::show_val(p, v));
        }
    }
    o.push('\n');

    // ---------------- ECS
    let e = &c.ecs;
    let _ = writeln!(o, "[ecs] {} archetype(s), {} component(s)", e.archetypes.len(), e.components.len());
    for (name, insts) in &p.generic_classes {
        let names: Vec<&str> = insts.iter().map(|&i| p.classes[i].name.as_str()).collect();
        let what = if insts.first().map(|&i| p.classes[i].is_static).unwrap_or(false) { "a static class" } else { "an entity class" };
        let _ = writeln!(o, "  generic class {}: {} instance(s), each {} of its own: {}", name, insts.len(), what, if names.is_empty() { "(not used)".to_string() } else { names.join(", ") });
    }
    for &a in &e.archetypes {
        let comps = &e.arch_comps[&a];
        let _ = writeln!(o, "  archetype {}{}", p.classes[a].name, if e.replicated_archetypes.contains(&a) { "  (replicated to clients)" } else { "" });
        for &k in comps {
            let comp = &e.components[k];
            let fields: Vec<String> = comp.fields.iter().map(|&f| format!("{}: {}", p.fields[f].name, p.ty_name(&p.fields[f].ty))).collect();
            let sig = if comp.cold { "cold: no system touches it".to_string() } else { format!("accessed by {}", comp.signature.join(", ")) };
            let _ = writeln!(o, "    {} {{ {} }}  -- {}", comp.cname, fields.join(", "), sig);
        }
    }
    if !e.dead_fields.is_empty() {
        let names: Vec<String> = e.dead_fields.iter().map(|&f| p.field_label(f)).collect();
        let _ = writeln!(o, "  dead fields eliminated (never read): {}", names.join(", "));
    }
    // dynamic components: which classes can have each one, and the tables of each class
    let comps: Vec<ClassId> = (0..p.classes.len()).filter(|&k| p.classes[k].is_component).collect();
    if !comps.is_empty() {
        let _ = writeln!(o, "  components added and removed at run time (a table per combination; Add/Remove move the row at the end of the system):");
        for &k in &comps {
            let mut on: Vec<&str> = p.archetypes_of(k).iter().map(|&a| p.classes[p.root_class(a)].name.as_str()).collect();
            on.dedup();
            let what = if on.is_empty() { "never added".to_string() } else { format!("on {}", on.join(", ")) };
            let _ = writeln!(o, "    component {}: {}", p.classes[k].name, what);
        }
        for &a in &e.archetypes {
            if p.classes[a].combo_of.is_none() {
                let n = p.family(a).len();
                if p.open.contains_key(&a) {
                    let ks: Vec<&str> = p.classes[a].comps.iter().map(|&k| p.classes[k].name.as_str()).collect();
                    let _ = writeln!(
                        o,
                        "    class {}: one open table (more than {} combinations, or --open-tables): every row has the columns of {} and a bit per component; Add/Remove set the bit in place, component systems and queries skip rows without it, the methods are compiled once",
                        p.classes[a].name,
                        crate::components::MAX_COMBOS,
                        ks.join(", ")
                    );
                } else if n > 1 {
                    let _ = writeln!(o, "    class {}: {} combination tables (at most {})", p.classes[a].name, n, crate::components::MAX_COMBOS);
                }
            }
        }
        let shared = crate::codegen::shared_code(c);
        if !shared.is_empty() {
            let _ = writeln!(o, "    compiled once (through a view of each table's columns): {}", shared.join("; "));
        }
    }
    o.push('\n');

    // ---------------- systems & schedule
    let _ = writeln!(o, "[systems] server tick = {} stage(s)", e.stages.len());
    for (si, stage) in e.stages.iter().enumerate() {
        let _ = writeln!(o, "  stage {}:", si);
        for &i in stage {
            let s = &e.systems[i];
            let archs: Vec<&str> = s.archetypes.iter().map(|&a| p.classes[a].name.as_str()).collect();
            let par = if s.parallel {
                "parallel (data-parallel over entities)".to_string()
            } else {
                format!("serial: {}", s.why_serial.join("; "))
            };
            let over = if archs.is_empty() { String::new() } else { format!(" over [{}]", archs.join(", ")) };
            let _ = writeln!(o, "    {}{}  -- {}", s.name, over, par);
            let _ = writeln!(o, "      effects: {}", describe_effects(p, &s.eff));
            if !s.hazard_fields.is_empty() {
                let hz: Vec<String> = s.hazard_fields.iter().map(|&f| p.field_label(f)).collect();
                let comps: Vec<&str> = s.hazards.iter().map(|&k| e.components[k].cname.as_str()).collect();
                let _ = writeln!(
                    o,
                    "      hazard: {} written on self and read on other entities -> double-buffered [{}] (Jacobi semantics, order-independent)",
                    hz.join(", "),
                    comps.join(", ")
                );
            }
            for (a, why) in &s.conflicts {
                let _ = writeln!(o, "      after {}: {}", e.systems[*a].name, why);
            }
            if let Some(pg) = &s.pgo {
                let _ = writeln!(o, "      {}", pg);
            }
        }
    }
    for g in crate::ecs::task_groups(e, &c.spatial) {
        let names: Vec<&str> = g.members.iter().map(|&i| e.systems[i].name.as_str()).collect();
        let _ = writeln!(
            o,
            "  task group (stage {}): {} run side by side, one thread each, {}; their deferred commands merge in this order",
            g.stage,
            names.join(", "),
            if g.ns_per_row.iter().all(|c| c.is_some()) {
                "when the profiled costs say that beats running them one after another (each with its own parallel loop)"
            } else {
                "when every one is below its parallel threshold and together they have SIJO_PAR_MIN rows"
            }
        );
    }
    let gpu: Vec<String> = (0..e.systems.len())
        .filter_map(|i| crate::ecs::gpu_candidate(p, e, i).ok().map(|(r, w)| format!("{} ({} B/row in, {} B/row out)", e.systems[i].name, r, w)))
        .collect();
    if !gpu.is_empty() {
        let _ = writeln!(o, "  GPU offload candidates (pure self-write systems; `sijoc build --gpu` runs their row loops as OpenMP target regions): {}", gpu.join(", "));
    }
    if let Some(pr) = &e.profile {
        let _ = writeln!(o, "  profile: {} ticks, {} system(s), a parallel region costs {:.1} us with {} threads", pr.ticks, pr.systems.len(), pr.fork_ns as f64 / 1000.0, pr.threads);
    }
    for n in &e.pgo_notes {
        let _ = writeln!(o, "  pgo: {}", n);
    }
    if !e.client_systems.is_empty() {
        let _ = writeln!(o, "  client systems (run on each client):");
        for &i in &e.client_systems {
            let s = &e.systems[i];
            let _ = writeln!(o, "    {}  -- effects: {}", s.name, describe_effects(p, &s.eff));
        }
    }
    let _ = writeln!(o, "  Game.Start effects: {}", describe_effects(p, &e.start_eff));
    o.push('\n');

    // ---------------- deferred interactions
    let mut deferred = vec![];
    for s in &e.systems {
        if s.kind != SysKind::Entity && s.kind != SysKind::Global {
            continue;
        }
        for (f, comm) in &s.eff.remote_writes {
            // a ranged field saturates after each command, so ADDs no longer commute (+5 then -5
            // at the top of the range is not -5 then +5): still deterministic, in the fixed order
            let how = match (*comm, c.net.clamp.get(f)) {
                (true, Some((lo, hi))) => format!("ADD, saturated into {}..{} after each command, applied in a fixed order (sender row order): deterministic, but the order matters", lo, hi),
                (true, None) => "ADD (commutative: concurrent writes merge exactly)".to_string(),
                (false, _) => "SET (last writer in entity order wins)".to_string(),
            };
            deferred.push(format!("  {}: writes {} on other entities -> deferred {}", s.name, p.field_label(*f), how));
        }
        for f in &s.eff.remote_calls {
            deferred.push(format!("  {}: calls {} on other entities -> delivered as a message at system end", s.name, p.func_label(*f)));
        }
    }
    if !deferred.is_empty() {
        let _ = writeln!(o, "[deferred] cross-entity effects (the same messages route across threads and server shards)");
        for d in deferred {
            let _ = writeln!(o, "{}", d);
        }
        o.push('\n');
    }

    // ---------------- delegates
    if p.delegates.iter().any(|d| !d.funcs.is_empty()) {
        let _ = writeln!(o, "[delegates] a call through a value runs the function it holds (chosen by number, statically dispatched; effects = all of them)");
        for d in &p.delegates {
            if d.funcs.is_empty() {
                continue;
            }
            let tn = p.ty_name(&Ty::Delegate(p.delegates.iter().position(|x| std::ptr::eq(x, d)).unwrap_or(0)));
            let _ = writeln!(o, "  {} ({} function(s))", tn, d.funcs.len());
            for &f in &d.funcs {
                let fd = &p.funcs[f];
                let n = p.caps_of(f);
                let caps: Vec<String> = fd.params[..n]
                    .iter()
                    .map(|&l| {
                        let loc = &fd.locals[l];
                        let alive = if matches!(loc.ty, Ty::Handle(_)) { ", call skipped once it is destroyed" } else { "" };
                        let boxed = if p.boxed_cap(&loc.ty) { ", boxed" } else { "" };
                        format!("{}: {}{}{}", loc.name, p.ty_name(&loc.ty), boxed, alive)
                    })
                    .collect();
                let what = if fd.synth.is_none() { "method" } else if fd.name.ends_with("__group") { "bound method" } else { "lambda" };
                let cap = if caps.is_empty() { "captures nothing".to_string() } else { format!("captures {}", caps.join(", ")) };
                let _ = writeln!(o, "    {} {} (line {}): {}", what, p.func_label(f), line(fd.span), cap);
            }
        }
        o.push('\n');
    }

    // ---------------- memory
    let m = &c.mem;
    let _ = writeln!(o, "[memory] {} allocation site(s)", m.sites.len());
    for (i, site) in m.sites.iter().enumerate() {
        if site.func == usize::MAX {
            continue;
        }
        let region = match site.region {
            Region::Frame => "Frame arena (bump alloc, reset every tick, no free; loops rewind it)",
            Region::Heap => "Heap (entity-owned, freed automatically)",
            Region::Caller => "caller's region (region-polymorphic return)",
        };
        let res = if site.reserve.is_empty() {
            String::new()
        } else {
            let caps: Vec<String> = site
                .reserve
                .iter()
                .map(|c| match c {
                    Cap::Const(n) => n.to_string(),
                    Cap::Count(cl) => format!("Count<{}>()", p.classes[*cl].name),
                })
                .collect();
            format!(", reserve {} (no reallocation)", caps.join(" + "))
        };
        let elem = p.ty_name(&p.alloc_sites[i].elem);
        let _ = writeln!(o, "  line {}: List<{}> in {} -> {}{}", line(site.span), elem, p.func_label(site.func), region, res);
        let _ = writeln!(o, "      reason: {}", site.reason);
    }
    o.push('\n');

    // ---------------- network
    let n = &c.net;
    let _ = writeln!(o, "[network] server-authoritative; replication inferred from client code");
    let mut total_bits_by_arch: Vec<(ClassId, u32, u32)> = vec![];
    for &a in &e.archetypes {
        let mut bits = 0;
        let mut raw = 0;
        for f in p.all_fields(a) {
            if let Some(fnet) = n.fields.get(&f) {
                bits += fnet.bits;
                raw += fnet.leaves.len() as u32 * 32;
            }
        }
        if bits > 0 {
            total_bits_by_arch.push((a, bits, raw));
        }
    }
    for (fid, fnet) in &n.fields {
        let leaves: Vec<String> = fnet
            .leaves
            .iter()
            .map(|l| {
                let name = if l.path_names.is_empty() { String::new() } else { format!(".{}", l.path_names.join(".")) };
                let q = match &l.quant {
                    Quant::Bool => "1 bit".to_string(),
                    Quant::Int { bits, .. } => format!("{} bits", bits),
                    Quant::Float { bits, step, .. } => format!("{} bits (step {})", bits, step),
                    Quant::IntRaw | Quant::FloatRaw => "32 bits".to_string(),
                    Quant::Handle => "32 bits (entity reference)".to_string(),
                    Quant::Str => "string id + text (or a delta) when it changes".to_string(),
                    Quant::List => "collection id + elements (or element deltas) when they change".to_string(),
                };
                format!("{}{}: {} [{}]", p.fields[*fid].name, name, q, l.source)
            })
            .collect();
        let _ = writeln!(o, "  replicate {} ({} bits): {}", p.field_label(*fid), fnet.bits, leaves.join("; "));
    }
    let not_repl: Vec<String> = (0..p.fields.len())
        .filter(|f| !n.fields.contains_key(f) && !e.is_dead(*f) && p.archetypes_of(p.fields[*f].class).iter().any(|a| e.replicated_archetypes.contains(a)))
        .map(|f| p.field_label(f))
        .collect();
    if !not_repl.is_empty() {
        let _ = writeln!(o, "  server-only (never sent, client code does not read them): {}", not_repl.join(", "));
    }
    for (a, bits, raw) in total_bits_by_arch {
        let _ = writeln!(o, "  {} full state: {} bits vs {} bits unquantized ({:.1}x smaller before delta compression)", p.classes[a].name, bits, raw, raw as f64 / bits as f64);
    }
    for s in &e.systems {
        for f in &s.eff.rpcs {
            let _ = writeln!(o, "  automatic RPC: {} (client) calls {} -> sent to the server, validated and executed under server authority", s.name, p.func_label(*f));
        }
    }
    // hidden coroutine fields (ranged locals that live across an await) are not the program's fields
    let mut clamps: Vec<String> = n.clamp.iter().filter(|(f, _)| !p.fields[**f].name.starts_with("__")).map(|(f, (lo, hi))| format!("{} in [{}, {}]", p.field_label(*f), lo, hi)).collect();
    clamps.sort();
    if !clamps.is_empty() {
        let _ = writeln!(o, "  range contracts enforced at every write (saturating): {}", clamps.join(", "));
    }
    let mut sp: Vec<String> = n.spatial.iter().map(|(a, (f, _))| format!("{} by {}.X", p.classes[*a].name, p.fields[*f].name)).collect();
    sp.sort();
    if !sp.is_empty() {
        let _ = writeln!(o, "  spatial sharding key: {}", sp.join(", "));
    }
    // ---------------- spatial queries
    let _ = writeln!(o, "\n[spatial queries]");
    if c.spatial.loops.is_empty() && c.spatial.rejected.is_empty() {
        let _ = writeln!(o, "  (no proximity loops over Query<T>() in systems)");
    }
    for l in &c.spatial.loops {
        let axes: Vec<String> = l
            .other_paths
            .iter()
            .map(|pth| {
                let mut t = p.fields[l.other_field].ty.clone();
                let mut names = vec![];
                for &i in pth {
                    if let Ty::Struct(s) = t.clone() {
                        names.push(p.structs[s].fields[i].name.clone());
                        t = p.structs[s].fields[i].ty.clone();
                    }
                }
                names.join(".")
            })
            .collect();
        let _ = writeln!(
            o,
            "  {}: foreach over {} uses a uniform grid on {}.{{{}}}, {}: only neighbouring cells are visited, in row order (same results as the full scan)",
            p.func_label(l.func),
            p.classes[l.class].name,
            p.field_label(l.other_field),
            axes.join(", "),
            if l.dyn_r2.is_empty() { format!("radius {:.3}, cell {:.3}", l.radius, l.cell) } else { "per-entity radius (the cell is sized from the largest one when the system starts)".to_string() }
        );
    }
    for (f, _, why) in &c.spatial.rejected {
        let _ = writeln!(o, "  {}: proximity loop kept as a full scan: {}", p.func_label(*f), why);
    }
    // ---------------- distributed simulation
    let _ = writeln!(o);
    o.push_str(&crate::shard::report(c, &crate::shard::plan(c)));
    o
}
