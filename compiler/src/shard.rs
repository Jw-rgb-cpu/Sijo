//! Distributed simulation: the plan that lets separate shard processes run one world and
//! still compute exactly what a single process computes.
//!
//! The model ("replicated structure, owner computes"):
//! - Every shard holds the same slot table and the same row order in every archetype table, because
//!   structural changes (spawn, destroy) are applied everywhere in the same global order. Only the
//!   data of some rows is valid on a shard: the rows it owns, plus copies other shards send.
//! - An entity system runs **partitioned**: each shard runs the rows it owns. Its deferred commands,
//!   spawns and prints carry a global order key (system, archetype position, row, sequence) that
//!   does not depend on the number of shards; at the end of the stage every shard merges all of
//!   them in key order and runs the command application (`sijo_apply`) **replicated**: every shard
//!   applies every command, so every shard computes the same results for the data it holds.
//! - Copies: a component is sent from its owner to the shards that read it. Code that runs on every
//!   shard (message bodies, RPCs, replicated systems) reads it on every row -> `to every shard`.
//!   The global system that only reads and prints runs on the coordinator, and client replication
//!   lives there too -> `to the coordinator`. A partitioned system that reads other entities only
//!   inside a proximity loop whose radius `spatial` proved -> a **halo**: only to the shards whose
//!   slab (plus the radius and the ownership hysteresis) contains the entity.
//! - Ownership: entities with a position are owned by the shard whose slab (quantiles of X) holds
//!   them; they move to another shard (handoff) when they leave the slab by more than the
//!   hysteresis. Entities without a position are owned by `slot % shards`.
//!
//! This module is the analysis (also shown by `sijoc explain`); `codegen` calls `gen_*` below for the
//! C side, and `runtime/sijo_shard.h` / `runtime/sijo_shard_net.h` are the runtime.

use crate::driver::Compiled;
use crate::ecs::SysKind;
use crate::effects::Effects;
use crate::hir::*;
use std::collections::{BTreeMap, BTreeSet};

/// component copies go to every shard
pub const R_ALL: u8 = 1;
/// ... to the coordinator (client replication, the coordinator's global system)
pub const R_COORD: u8 = 2;
/// ... to the shards whose slab is within the halo width
pub const R_HALO: u8 = 4;

/// where a server system runs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// every shard runs the rows it owns
    Part,
    /// every shard runs every row (exactly the same computation everywhere)
    Repl,
    /// only the coordinator (shard 0) runs it
    Coord,
}

impl Mode {
    pub fn code(self) -> u8 {
        match self {
            Mode::Part => 0,
            Mode::Repl => 1,
            Mode::Coord => 2,
        }
    }
}

/// A run of systems between two barriers. Every stage ends with a barrier (commands are exchanged
/// and applied); a stage is split when a replicated system must allocate spawned handles after
/// partitioned systems of the same stage have spawned (their spawns are merged first).
#[derive(Clone, Debug)]
pub struct Segment {
    pub stage: usize,
    pub systems: Vec<usize>,
    pub ends_stage: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ShardPlan {
    /// per system index (client systems: unused)
    pub mode: Vec<Mode>,
    pub why: Vec<String>,
    /// per component id: R_* bits
    pub routes: Vec<u8>,
    /// per component id: why it is sent where it is
    pub route_why: Vec<Vec<String>>,
    /// queried archetype -> halo radius
    pub halo_r: BTreeMap<ClassId, f64>,
    /// spatial loops (indices into SpatialPlan::loops) that read other entities from the halo:
    /// their scans skip rows whose halo copy is not valid on this shard
    pub halo_loops: BTreeSet<usize>,
    /// spatial loops that could not use the halo, and why
    pub loop_why: Vec<(usize, String)>,
    /// ownership hysteresis (world units past the slab before a handoff)
    pub hyst: f64,
    pub segments: Vec<Segment>,
    /// RPC methods exist: a barrier at the start of each tick carries the coordinator's RPCs
    pub rpc: bool,
    /// per stage: components written by its partitioned systems that are sent somewhere
    pub stage_writes: Vec<BTreeSet<usize>>,
    /// archetypes whose position deferred commands or messages change (no halo for them)
    pub moved_by_apply: BTreeSet<ClassId>,
    /// systems whose own code spawns (not only messages they send)
    pub own_spawns: BTreeSet<usize>,
}

impl ShardPlan {
    pub fn halo_only(&self, k: usize) -> bool {
        self.routes[k] & R_HALO != 0 && self.routes[k] & R_ALL == 0
    }
}

fn add_route(plan: &mut ShardPlan, k: usize, bit: u8, why: String) {
    plan.routes[k] |= bit;
    if !plan.route_why[k].contains(&why) {
        plan.route_why[k].push(why);
    }
}

/// does any function the system runs on its own behalf contain `Spawn`?
fn spawns_itself(p: &Program, sys: FuncId) -> bool {
    let mut found = false;
    for f in crate::ecs::reachable_funcs(p, sys) {
        crate::sema::visit_block(&p.funcs[f].body, &mut |e| found |= matches!(e.kind, ExprKind::Spawn { .. }));
    }
    found
}

/// methods that run when a command is applied: deferred messages and RPCs (every override)
fn message_funcs(p: &Program) -> Vec<(FuncId, ClassId)> {
    let mut called: BTreeSet<FuncId> = BTreeSet::new();
    for f in &p.funcs {
        crate::sema::visit_block(&f.body, &mut |e| {
            if let ExprKind::Call { func, dispatch: Dispatch::RemoteDeferred | Dispatch::Rpc, .. } = &e.kind {
                called.insert(*func);
            }
        });
    }
    let mut out = vec![];
    for &fid in &called {
        if let FuncKind::EntityMethod(c) = p.funcs[fid].kind {
            for a in p.archetypes_of(c) {
                let g = p.virtual_target(a, fid);
                out.push((g, a));
            }
        }
    }
    out
}

/// may this expression (with the calls it makes on `this`) write field `f` of `this`?
fn expr_writes(p: &Program, c: &Compiled, e: &Expr, f: FieldId, archs: &[ClassId]) -> bool {
    let mut w = false;
    crate::sema::visit_expr(e, &mut |x| match &x.kind {
        ExprKind::Assign { target, .. } | ExprKind::IncDec { target, .. } => {
            if crate::effects::target_field(target) == Some((f, true)) {
                w = true;
            }
        }
        ExprKind::Call { func, dispatch: Dispatch::SelfCall | Dispatch::BaseCall, .. } => {
            for &a in archs {
                let g = match p.funcs[*func].kind {
                    FuncKind::EntityMethod(_) => p.virtual_target(a, *func),
                    _ => *func,
                };
                if c.info.get(g, Some(a)).self_writes.contains(&f) || c.info.get(*func, Some(a)).self_writes.contains(&f) {
                    w = true;
                }
            }
        }
        _ => {}
    });
    w
}

fn stmt_writes(p: &Program, c: &Compiled, s: &Stmt, f: FieldId, archs: &[ClassId]) -> bool {
    let mut w = false;
    crate::sema::visit_stmt(s, &mut |e| {
        if !w && expr_writes(p, c, e, f, archs) {
            w = true;
        }
    });
    w
}

/// does the statement contain the loop (its `foreach` in `lfunc`, or a call that reaches `lfunc`)?
fn stmt_contains_loop(p: &Program, s: &Stmt, cur: FuncId, lfunc: FuncId, var: LocalId) -> bool {
    if cur == lfunc {
        if let Stmt::ForQuery { var: v, .. } = s {
            if *v == var {
                return true;
            }
        }
        return s.sub_blocks().iter().any(|b| b.stmts.iter().any(|x| stmt_contains_loop(p, x, cur, lfunc, var)));
    }
    let mut hit = false;
    crate::sema::visit_stmt(s, &mut |e| {
        if let ExprKind::Call { func, dispatch: Dispatch::SelfCall | Dispatch::BaseCall, .. } = &e.kind {
            if *func == lfunc || p.funcs[*func].name == p.funcs[lfunc].name {
                hit = true;
            }
        }
    });
    hit
}

/// Is the position field written (on `this`) before the proximity loop runs, on some path? The
/// halo is sized from where the querying entity was at the start of the stage, so it must still be
/// there when it looks for its neighbours. None: could not tell (treated as written).
fn written_before(p: &Program, c: &Compiled, body: &Block, cur: FuncId, lfunc: FuncId, var: LocalId, f: FieldId, archs: &[ClassId], depth: usize) -> Option<bool> {
    if depth > 8 {
        return None;
    }
    for s in &body.stmts {
        if !stmt_contains_loop(p, s, cur, lfunc, var) {
            if stmt_writes(p, c, s, f, archs) {
                return Some(true);
            }
            continue;
        }
        return match s {
            Stmt::ForQuery { var: v, .. } if cur == lfunc && *v == var => Some(false),
            Stmt::If { cond, then, els } => {
                if expr_writes(p, c, cond, f, archs) {
                    return Some(true);
                }
                for b in std::iter::once(then).chain(els.iter()) {
                    if b.stmts.iter().any(|x| stmt_contains_loop(p, x, cur, lfunc, var)) {
                        return written_before(p, c, b, cur, lfunc, var, f, archs, depth + 1);
                    }
                }
                None
            }
            Stmt::Block(b) => written_before(p, c, b, cur, lfunc, var, f, archs, depth + 1),
            Stmt::Expr(Expr { kind: ExprKind::Call { func, args, dispatch: Dispatch::SelfCall, .. }, .. }) if cur != lfunc => {
                if args.iter().any(|a| expr_writes(p, c, &a.expr, f, archs)) {
                    return Some(true);
                }
                let mut r = Some(false);
                for &a in archs {
                    let g = p.virtual_target(a, *func);
                    if g != lfunc {
                        return None;
                    }
                    match written_before(p, c, &p.funcs[g].body, g, lfunc, var, f, archs, depth + 1) {
                        Some(false) => {}
                        other => r = other,
                    }
                }
                r
            }
            // inside another loop, a later iteration runs after writes of an earlier one
            _ => None,
        };
    }
    None
}

/// spatial X of an archetype: (field, member index)
fn spatial_x(c: &Compiled, a: ClassId) -> Option<(FieldId, usize)> {
    c.net.spatial.get(&a).copied()
}

/// halo radius of a spatial loop run by system `si`, or why it cannot use the halo
fn halo_loop(c: &Compiled, si: usize, li: usize, moved: &BTreeSet<ClassId>) -> Result<f64, String> {
    let p = &c.prog;
    let s = &c.ecs.systems[si];
    let l = &c.spatial.loops[li];
    if !l.radius.is_finite() || l.radius < 0.0 {
        return Err("its radius depends on each entity: every copy goes to every shard".into());
    }
    // the queried entities: their X must be one of the gridded axes
    let mut axis = None;
    for a in p.query_archetypes(l.class, &l.with) {
        let Some((f, xi)) = spatial_x(c, a) else { return Err(format!("{} has no position to partition by", p.classes[a].name)) };
        if f != l.other_field {
            return Err(format!("the loop measures {}, not the position {} is partitioned by", p.field_label(l.other_field), p.classes[a].name));
        }
        let Some(m) = l.other_paths.iter().position(|pth| pth == &vec![xi]) else { return Err("the distance test does not bound X, the axis shards are cut along".into()) };
        if axis.map_or(false, |x| x != m) {
            return Err("archetypes disagree on the X axis".into());
        }
        axis = Some(m);
        if moved.contains(&a) {
            return Err(format!("deferred commands or messages move {} (its position changes after the copies are sent)", p.classes[a].name));
        }
    }
    let Some(m) = axis else { return Err("no archetype".into()) };
    // the querying entity: owned by the shard whose slab (plus hysteresis) holds it at stage start
    for &a in &s.archetypes {
        let Some((f, xi)) = spatial_x(c, a) else { return Err(format!("{} (the querying entity) has no position", p.classes[a].name)) };
        if f != l.self_field || l.self_paths.get(m) != Some(&vec![xi]) {
            return Err("the loop measures from a field that is not this entity's position".into());
        }
        if moved.contains(&a) {
            return Err(format!("deferred commands or messages move {}", p.classes[a].name));
        }
    }
    let archs = s.archetypes.clone();
    if archs.iter().any(|&a| c.info.get(s.func, Some(a)).self_writes.contains(&l.self_field)) || s.eff.self_writes.contains(&l.self_field) {
        match written_before(p, c, &p.funcs[s.func].body, s.func, l.func, l.var, l.self_field, &archs, 0) {
            Some(false) => {}
            _ => return Err(format!("{} may move this entity before the loop runs", s.name)),
        }
    }
    Ok(l.radius)
}

/// halo reads and other remote reads of a partitioned system
fn classify_reads(c: &Compiled, si: usize, cursors: &BTreeMap<(FuncId, LocalId), usize>) -> (BTreeMap<FieldId, Vec<usize>>, BTreeSet<FieldId>) {
    let p = &c.prog;
    let s = &c.ecs.systems[si];
    let mut halo: BTreeMap<FieldId, Vec<usize>> = BTreeMap::new();
    let mut other: BTreeSet<FieldId> = BTreeSet::new();
    for fid in crate::ecs::reachable_funcs(p, s.func) {
        crate::sema::visit_block(&p.funcs[fid].body, &mut |e| match &e.kind {
            ExprKind::EField { base, field } => match &base.kind {
                ExprKind::SelfRef => {}
                ExprKind::Local(v) if cursors.contains_key(&(fid, *v)) => halo.entry(*field).or_default().push(cursors[&(fid, *v)]),
                _ => {
                    other.insert(*field);
                }
            },
            ExprKind::Call { func, recv: Some(r), dispatch: Dispatch::RemoteRead, .. } => {
                let class = r.ty.class_of().unwrap_or(0);
                let li = match &r.kind {
                    ExprKind::Local(v) => cursors.get(&(fid, *v)).copied(),
                    _ => None,
                };
                for b in p.archetypes_of(class) {
                    let g = p.virtual_target(b, *func);
                    let sum = c.info.get(g, Some(b));
                    for f in &sum.self_reads {
                        match li {
                            Some(li) => halo.entry(*f).or_default().push(li),
                            None => {
                                other.insert(*f);
                            }
                        }
                    }
                    other.extend(sum.remote_reads.iter().copied());
                }
            }
            _ => {}
        });
    }
    (halo, other)
}

pub fn plan(c: &Compiled) -> ShardPlan {
    let p = &c.prog;
    let ecs = &c.ecs;
    let ncomp = ecs.components.len();
    let mut plan = ShardPlan { routes: vec![0; ncomp], route_why: vec![vec![]; ncomp], ..Default::default() };
    let comp = |f: FieldId| ecs.comp_of(f);
    let msgs = message_funcs(p);
    plan.rpc = p.funcs.iter().any(|f| {
        let mut r = false;
        crate::sema::visit_block(&f.body, &mut |e| r |= matches!(e.kind, ExprKind::Call { dispatch: Dispatch::Rpc, .. }));
        r
    });

    // ---- 1. where each system runs
    for (i, s) in ecs.systems.iter().enumerate() {
        // the system's own code spawns (a Game.Update too: its slots must come after the spawns
        // that partitioned systems of its stage made, see the segments below)
        let own = matches!(s.kind, SysKind::Entity | SysKind::Global) && spawns_itself(p, s.func);
        if own {
            plan.own_spawns.insert(i);
        }
        let (m, why) = match s.kind {
            SysKind::Entity if own && !s.spawn_par => (Mode::Repl, "uses the handles it spawns beyond what can be patched after the merge: every shard runs every row".to_string()),
            SysKind::Entity => (Mode::Part, "each shard runs the entities it owns".to_string()),
            SysKind::Global => {
                let e = &s.eff;
                let only_reads = e.spawns.is_empty() && e.destroys.is_empty() && !e.destroys_self && e.remote_writes.is_empty() && e.remote_calls.is_empty() && e.msg_writes.is_empty() && e.rpcs.is_empty() && e.self_writes.is_empty();
                if only_reads {
                    (Mode::Coord, "only reads and prints: runs on the coordinator".to_string())
                } else {
                    (Mode::Repl, "changes the world: every shard runs it".to_string())
                }
            }
            _ => (Mode::Part, String::new()),
        };
        plan.mode.push(m);
        plan.why.push(why);
    }

    // ---- 2. positions that commands and messages change (applied after the copies are sent)
    let mut apply_written: BTreeSet<FieldId> = BTreeSet::new();
    for s in &ecs.systems {
        if matches!(s.kind, SysKind::Entity | SysKind::Global) {
            apply_written.extend(s.eff.remote_writes.keys().copied());
            apply_written.extend(s.eff.msg_writes.iter().copied());
        }
    }
    for &(g, a) in &msgs {
        let e: Effects = c.info.get(g, Some(a));
        apply_written.extend(e.self_writes.iter().copied());
        apply_written.extend(e.remote_writes.keys().copied());
        apply_written.extend(e.msg_writes.iter().copied());
    }
    for &a in &ecs.archetypes {
        if let Some((f, _)) = spatial_x(c, a) {
            if apply_written.contains(&f) {
                plan.moved_by_apply.insert(a);
            }
        }
    }

    // ---- 3. halo loops
    let mut cursors: BTreeMap<(FuncId, LocalId), usize> = BTreeMap::new();
    let mut loop_r: BTreeMap<usize, f64> = BTreeMap::new();
    for (li, l) in c.spatial.loops.iter().enumerate() {
        // every partitioned system that runs the loop must be able to use the halo
        let mut ok: Result<f64, String> = Err("no partitioned system runs it".into());
        for (si, s) in ecs.systems.iter().enumerate() {
            if s.kind != SysKind::Entity || !l.systems.contains(&s.func) {
                continue;
            }
            if plan.mode[si] != Mode::Part {
                ok = Err(format!("{} runs on every shard", s.name));
                break;
            }
            match halo_loop(c, si, li, &plan.moved_by_apply) {
                Ok(r) => {
                    if ok.is_err() {
                        ok = Ok(r);
                    }
                }
                Err(e) => {
                    ok = Err(e);
                    break;
                }
            }
        }
        match ok {
            Ok(r) => {
                cursors.insert((l.func, l.var), li);
                loop_r.insert(li, r);
            }
            Err(e) => plan.loop_why.push((li, e)),
        }
    }

    // ---- 4. who reads what
    for (i, s) in ecs.systems.iter().enumerate() {
        match (s.kind, plan.mode[i]) {
            (SysKind::Entity, Mode::Part) => {
                let (halo, other) = classify_reads(c, i, &cursors);
                for &f in &s.eff.remote_reads {
                    let Some(k) = comp(f) else { continue };
                    match halo.get(&f) {
                        Some(lis) if !other.contains(&f) => {
                            for &li in lis {
                                let l = &c.spatial.loops[li];
                                for a in p.query_archetypes(l.class, &l.with) {
                                    let r = plan.halo_r.entry(a).or_insert(0.0);
                                    *r = r.max(loop_r[&li]);
                                }
                                plan.halo_loops.insert(li);
                            }
                            add_route(&mut plan, k, R_HALO, format!("the proximity loop of {} reads it (radius {} proven)", s.name, fmt_r(loop_r[&lis[0]])));
                        }
                        _ => {
                            let how = if halo.contains_key(&f) {
                                format!("{} reads it on other entities also outside its proximity loop", s.name)
                            } else if other.contains(&f) {
                                format!("{} reads it on other entities through handles or a scan without a proven radius", s.name)
                            } else {
                                format!("messages {} sends read it", s.name)
                            };
                            add_route(&mut plan, k, R_ALL, how);
                        }
                    }
                }
            }
            (SysKind::Entity | SysKind::Global, Mode::Repl) => {
                for &f in s.eff.self_reads.iter().chain(&s.eff.remote_reads) {
                    if let Some(k) = comp(f) {
                        add_route(&mut plan, k, R_ALL, format!("{} runs on every shard and reads it", s.name));
                    }
                }
            }
            (SysKind::Global, Mode::Coord) => {
                for &f in &s.eff.remote_reads {
                    if let Some(k) = comp(f) {
                        add_route(&mut plan, k, R_COORD, format!("{} (on the coordinator) reads it", s.name));
                    }
                }
            }
            _ => {}
        }
    }
    for &(g, a) in &msgs {
        let e = c.info.get(g, Some(a));
        for &f in e.self_reads.iter().chain(&e.remote_reads) {
            if let Some(k) = comp(f) {
                add_route(&mut plan, k, R_ALL, format!("message {} (applied on every shard) reads it", p.func_label(g)));
            }
        }
    }
    for (k, cm) in ecs.components.iter().enumerate() {
        if cm.replicated {
            add_route(&mut plan, k, R_COORD, "replicated to clients through the coordinator".into());
        }
    }
    // [Persistent]: every shard keeps the same store (it is read when entities spawn)
    for (_, key, pers, flag) in &p.persist {
        for &f in pers.iter().chain([key, flag]) {
            if let Some(k) = comp(f) {
                add_route(&mut plan, k, R_ALL, format!("[Persistent]: every shard keeps the store of {}", p.field_label(f)));
            }
        }
    }
    // a halo loop that reads a component every shard has anyway needs no row filter
    let halo_loops: Vec<usize> = plan.halo_loops.iter().copied().collect();
    for li in halo_loops {
        let l = &c.spatial.loops[li];
        let needs = ecs.systems.iter().enumerate().filter(|(_, s)| l.systems.contains(&s.func)).any(|(si, _)| {
            let (halo, _) = classify_reads(c, si, &cursors);
            halo.iter().any(|(f, lis)| lis.contains(&li) && comp(*f).map_or(false, |k| plan.halo_only(k)))
        });
        if !needs {
            plan.halo_loops.remove(&li);
        }
    }
    let rmax = plan.halo_r.values().cloned().fold(0.0, f64::max);
    plan.hyst = rmax * 0.25;

    // ---- 5. segments: a replicated system that spawns after partitioned spawns of its stage
    for (si, stage) in ecs.stages.iter().enumerate() {
        let mut cur = Segment { stage: si, systems: vec![], ends_stage: false };
        let mut part_spawned = false;
        for &i in stage {
            if plan.mode[i] == Mode::Repl && plan.own_spawns.contains(&i) && part_spawned {
                plan.segments.push(std::mem::replace(&mut cur, Segment { stage: si, systems: vec![], ends_stage: false }));
                part_spawned = false;
            }
            if plan.mode[i] == Mode::Part && plan.own_spawns.contains(&i) {
                part_spawned = true;
            }
            cur.systems.push(i);
        }
        cur.ends_stage = true;
        plan.segments.push(cur);
    }

    // ---- 6. what partitioned systems change, per stage (compared with the stage start)
    for stage in &ecs.stages {
        let mut set = BTreeSet::new();
        for &i in stage {
            if plan.mode[i] != Mode::Part {
                continue;
            }
            let s = &ecs.systems[i];
            for &a in &s.archetypes {
                for f in c.info.get(s.func, Some(a)).self_writes.iter().chain(&s.eff.self_writes) {
                    if let Some(k) = comp(*f) {
                        if plan.routes[k] != 0 {
                            set.insert(k);
                        }
                    }
                }
            }
        }
        plan.stage_writes.push(set);
    }
    plan
}

fn fmt_r(r: f64) -> String {
    let s = format!("{:.3}", r);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// the `[shards]` section of `sijoc explain`
pub fn report(c: &Compiled, plan: &ShardPlan) -> String {
    use std::fmt::Write;
    let p = &c.prog;
    let ecs = &c.ecs;
    let mut o = String::new();
    let _ = writeln!(o, "[shards] separate shard processes (--shard-of N --shard-id I --peers ...) or worlds (--shards N)");
    let mut part = vec![];
    for &a in &ecs.archetypes {
        match spatial_x(c, a) {
            Some((f, xi)) => {
                let xname = match &p.fields[f].ty {
                    Ty::Struct(s) => p.structs[*s].fields.get(xi).map(|m| m.name.clone()).unwrap_or_default(),
                    _ => String::new(),
                };
                part.push(format!("{} by {}.{} (slabs)", p.classes[a].name, p.fields[f].name, xname));
            }
            None => part.push(format!("{} by slot (no position)", p.classes[a].name)),
        }
    }
    let _ = writeln!(o, "  ownership: {}", part.join(", "));
    if plan.hyst > 0.0 {
        let _ = writeln!(o, "  handoff: an entity moves to another shard when it is more than {} past its slab (hysteresis = halo radius / 4)", fmt_r(plan.hyst));
    }
    for (i, s) in ecs.systems.iter().enumerate() {
        if !matches!(s.kind, SysKind::Entity | SysKind::Global) {
            continue;
        }
        let m = match plan.mode[i] {
            Mode::Part => "partitioned",
            Mode::Repl => "replicated",
            Mode::Coord => "coordinator",
        };
        let _ = writeln!(o, "  {}: {} — {}", s.name, m, plan.why[i]);
    }
    let _ = writeln!(o, "  commands: exchanged at the end of each stage in a global order (system, archetype, row, sequence) and applied on every shard");
    let mut any = false;
    for (k, cm) in ecs.components.iter().enumerate() {
        let r = plan.routes[k];
        if r == 0 {
            continue;
        }
        any = true;
        let fields: Vec<String> = cm.fields.iter().map(|&f| p.fields[f].name.clone()).collect();
        let mut to = vec![];
        if r & R_ALL != 0 {
            to.push("every shard".to_string());
        } else {
            if r & R_HALO != 0 {
                let rr = p.archetypes_of(cm.class).iter().filter_map(|a| plan.halo_r.get(a)).cloned().fold(0.0, f64::max);
                to.push(format!("halo {} + {}", fmt_r(rr), fmt_r(plan.hyst)));
            }
            if r & R_COORD != 0 {
                to.push("the coordinator".into());
            }
        }
        let _ = writeln!(o, "  {} {{{}}} -> {}: {}", cm.cname, fields.join(", "), to.join(" and "), plan.route_why[k].join("; "));
    }
    if !any {
        let _ = writeln!(o, "  no component is read on other shards: shards exchange only commands, spawns and handoffs");
    }
    for (li, why) in &plan.loop_why {
        let l = &c.spatial.loops[*li];
        let _ = writeln!(o, "  no halo for the proximity loop over {} in {}: {}", p.classes[l.class].name, p.func_label(l.func), why);
    }
    let nb = plan.segments.len() + usize::from(plan.rpc);
    let _ = writeln!(o, "  barriers per tick: {}{} (one per stage{})", nb, if plan.rpc { " (RPCs at the start)" } else { "" }, if plan.segments.len() > ecs.stages.len() { ", plus spawn merges" } else { "" });
    o
}

// ---------------------------------------------------------------------------
// C generation: the per-program half of the shard runtime (tables, exact component codecs, the
// segment runner). The generic half is runtime/sijo_shard.h.
// ---------------------------------------------------------------------------

use crate::codegen::{cid, Gen};
use std::fmt::Write as _;

impl<'a> Gen<'a> {
    /// exact, endian-neutral bytes of a value (component images: sync, handoff, spawn, gather)
    pub(crate) fn sh_pack(&self, ty: &Ty, v: &str, d: usize) -> String {
        match ty {
            Ty::Int | Ty::Char | Ty::Enum(_) => format!("sijo_shw_u32(o, (uint32_t)({v}));"),
            Ty::Float => format!("sijo_shw_u32(o, sijo_f2bits({v}));"),
            Ty::Bool => format!("sijo_shw_u8(o, ({v}) ? 1u : 0u);"),
            Ty::Handle(_) | Ty::NHandle(_) => format!("{{ SijoH x_ = {v}; sijo_shw_u32(o, x_.idx); sijo_shw_u32(o, x_.gen); }}"),
            Ty::Str => format!("{{ SijoStr x_ = {v}; sijo_shw_var(o, (uint64_t)(uint32_t)x_.n); sijo_shw_bytes(o, x_.p, (size_t)x_.n); }}"),
            Ty::Struct(s) => self.p.structs[*s].fields.iter().map(|m| self.sh_pack(&m.ty, &format!("({}).{}", v, cid(&m.name)), d)).collect::<Vec<_>>().join(" "),
            // T[,] / T[,,]: the lengths, then the elements like a list
            Ty::MdArray(el, r) => {
                let lens: String = (0..*r as usize).map(|k| format!("sijo_shw_u32(o, (uint32_t)({v}).d{k}); ")).collect();
                format!("{lens}{}", self.sh_pack(&Ty::List(el.clone()), v, d))
            }
            Ty::List(_) | Ty::Array(_) | Ty::Dict(..) => {
                let el = self.p.elem_of(ty).unwrap_or(Ty::Int);
                let et = self.elem_cty(&el);
                let inner = self.sh_pack(&el, &format!("((const {et}*)L{d}->data)[i{d}]"), d + 1);
                format!("{{ const SijoList* L{d} = &({v}); sijo_shw_var(o, (uint64_t)(uint32_t)L{d}->len); for (int32_t i{d} = 0; i{d} < L{d}->len; i{d}++) {{ {inner} }} }}")
            }
            // the function number, then what it captured
            Ty::Delegate(dg) => format!("sijo_d{dg}_shw(o, &({v}));"),
            // a type this codec does not know yet: loud on shards, harmless in one process
            other => format!("sijo_panic(\"shard codec: values of type {} cannot be copied between shards yet\");", self.p.ty_name(other).replace('"', "'")),
        }
    }

    pub(crate) fn sh_unpack(&self, ty: &Ty, place: &str, d: usize) -> String {
        match ty {
            Ty::Int | Ty::Char | Ty::Enum(_) => format!("{place} = ({})(int32_t)sijo_shr_u32(in);", self.cty(ty)),
            Ty::Float => format!("{place} = sijo_bits2f(sijo_shr_u32(in));"),
            Ty::Bool => format!("{place} = sijo_shr_u8(in) != 0;"),
            Ty::Handle(_) | Ty::NHandle(_) => format!("{place} = sijo_shr_handle(in);"),
            Ty::Str => format!("{{ uint32_t n_ = (uint32_t)sijo_shr_var(in); const uint8_t* p_ = sijo_shr_bytes(in, n_); {place} = p_ ? sijo_str_heap(&w->sheap, (const char*)p_, (int32_t)n_) : SIJO_STR_EMPTY; }}"),
            Ty::Struct(s) => self.p.structs[*s].fields.iter().map(|m| self.sh_unpack(&m.ty, &format!("({}).{}", place, cid(&m.name)), d)).collect::<Vec<_>>().join(" "),
            Ty::MdArray(el, r) => {
                let r = *r as usize;
                let lens: String = (0..3).map(|k| if k < r { format!("int64_t m{d}_{k} = (int64_t)sijo_shr_u32(in); ") } else { format!("int64_t m{d}_{k} = 1; ") }).collect();
                let fill = self.sh_unpack(&Ty::List(el.clone()), place, d);
                format!("{{ {lens}{fill} if (!sijo_md_lens_ok(m{d}_0, m{d}_1, m{d}_2, ({place}).len)) in->bad = 1; else {{ ({place}).d0 = (int32_t)m{d}_0; ({place}).d1 = (int32_t)m{d}_1; ({place}).d2 = (int32_t)m{d}_2; }} }}")
            }
            Ty::List(_) | Ty::Array(_) | Ty::Dict(..) => {
                let el = self.p.elem_of(ty).unwrap_or(Ty::Int);
                let et = self.elem_cty(&el);
                let inner = self.sh_unpack(&el, &format!("(*e{d})"), d + 1);
                let rebuild = if ty.is_dict() {
                    let (kind, _, ent) = self.dict_info(ty);
                    format!(" sijo_dict_rebuild(&{place}, {kind}, (int32_t)sizeof({ent}));")
                } else {
                    String::new()
                };
                format!("{{ uint64_t n{d} = sijo_shr_var(in); if (n{d} > in->n - in->pos) {{ in->bad = 1; n{d} = 0; }} {place} = sijo_list_new(NULL, 1, (int32_t)sizeof({et}), (int32_t)n{d}); for (int32_t i{d} = 0; i{d} < (int32_t)n{d}; i{d}++) {{ {et}* e{d} = ({et}*)sijo_list_push(NULL, &{place}, (int32_t)sizeof({et})); memset(e{d}, 0, sizeof({et})); {inner} }}{rebuild} }}")
            }
            Ty::Delegate(dg) => format!("sijo_d{dg}_shr(w, in, &({place}));"),
            other => format!("sijo_panic(\"shard codec: values of type {} cannot be copied between shards yet\");", self.p.ty_name(other).replace('"', "'")),
        }
    }

    /// the per-program half of the shard runtime, emitted after the systems
    pub(crate) fn shard_section(&mut self) {
        let sp = self.shard.clone();
        let c = self.c;
        let p = self.p;
        let ecs = &c.ecs;
        let archs = ecs.archetypes.clone();
        let mut o = String::new();
        o.push_str("\n/* ======================= distributed simulation: plan (src/shard.rs) ======================= */\n");
        let _ = writeln!(o, "#define SIJO_SH_NCOMP {}", ecs.components.len().max(1));
        let routes: Vec<String> = sp.routes.iter().map(|r| r.to_string()).collect();
        let _ = writeln!(o, "static const uint8_t sijo_sh_route[SIJO_SH_NCOMP] = {{ {} }};", if routes.is_empty() { "0".into() } else { routes.join(", ") });
        let nsys = ecs.systems.len().max(1);
        let modes: Vec<String> = (0..ecs.systems.len()).map(|i| sp.mode[i].code().to_string()).collect();
        let _ = writeln!(o, "#define SIJO_SH_NSYS {nsys}\nstatic const uint8_t sijo_sh_mode[SIJO_SH_NSYS] = {{ {} }};", if modes.is_empty() { "0".into() } else { modes.join(", ") });
        // a parallel system with spawns merges them per spawned archetype (sijo_tmerge_all): the
        // spawn order key then sorts by spawned archetype before the row
        let pspawn: Vec<String> = ecs.systems.iter().map(|s| u8::from(s.parallel && s.spawn_par).to_string()).collect();
        let _ = writeln!(o, "static const uint8_t sijo_sh_tspawn[SIJO_SH_NSYS] = {{ {} }};", if pspawn.is_empty() { "0".into() } else { pspawn.join(", ") });
        let _ = writeln!(o, "#define SIJO_SH_NSEG {}", sp.segments.len().max(1));
        let segst: Vec<String> = sp.segments.iter().map(|s| s.stage.to_string()).collect();
        let segend: Vec<String> = sp.segments.iter().map(|s| u8::from(s.ends_stage).to_string()).collect();
        let _ = writeln!(o, "static const int16_t sijo_sh_seg_stage[SIJO_SH_NSEG] = {{ {} }};", if segst.is_empty() { "0".into() } else { segst.join(", ") });
        let _ = writeln!(o, "static const uint8_t sijo_sh_seg_end[SIJO_SH_NSEG] = {{ {} }};", if segend.is_empty() { "1".into() } else { segend.join(", ") });
        let _ = writeln!(o, "#define SIJO_SH_RPC {}\n#define SIJO_SH_HYST {:?}", u8::from(sp.rpc), sp.hyst);
        // shards of different programs (or of a different plan) refuse each other
        let mut schema = String::new();
        for &a in &archs {
            let _ = write!(schema, "arch {}:", p.classes[a].name);
            for &k in &ecs.arch_comps[&a] {
                let fs: Vec<String> = ecs.components[k].fields.iter().map(|&f| format!("{}:{}", p.fields[f].name, p.ty_name(&p.fields[f].ty))).collect();
                let _ = write!(schema, " c{k}[{}]r{}", fs.join(","), sp.routes[k]);
            }
            schema.push(';');
        }
        for (i, s) in ecs.systems.iter().enumerate() {
            let _ = write!(schema, "sys {} {};", s.name, sp.mode[i].code());
        }
        let _ = write!(schema, "seg {} rpc {} hyst {:?}", sp.segments.len(), sp.rpc, sp.hyst);
        let mut h: u32 = 0x811c9dc5;
        for b in schema.bytes() {
            h ^= b as u32;
            h = h.wrapping_mul(0x01000193);
        }
        let _ = writeln!(o, "#define SIJO_SH_PROTO 0x{h:08x}u");
        // per archetype (ARCH ids)
        let maxc = archs.iter().map(|a| ecs.arch_comps[a].len()).max().unwrap_or(1).max(1);
        let _ = writeln!(o, "#define SIJO_SH_MAXC {maxc}");
        let mut ncomp = vec![];
        let mut cids = vec![];
        let mut halo = vec![];
        let mut spat = vec![];
        let mut hmask = vec![];
        for &a in &archs {
            let comps = &ecs.arch_comps[&a];
            ncomp.push(comps.len().to_string());
            let mut ids: Vec<String> = comps.iter().map(|k| k.to_string()).collect();
            while ids.len() < maxc {
                ids.push("-1".into());
            }
            cids.push(format!("{{ {} }}", ids.join(", ")));
            halo.push(format!("{:?}f", sp.halo_r.get(&a).copied().unwrap_or(0.0) as f32));
            spat.push(u8::from(c.net.spatial.contains_key(&a)).to_string());
            let hm: u64 = comps.iter().enumerate().filter(|(_, &k)| sp.halo_only(k)).fold(0, |m, (ci, _)| m | (1u64 << ci));
            hmask.push(format!("{hm}ull"));
        }
        let _ = writeln!(o, "static const int sijo_sh_ncomp[SIJO_NARCH] = {{ {} }};", if ncomp.is_empty() { "0".into() } else { ncomp.join(", ") });
        let _ = writeln!(o, "static const int16_t sijo_sh_cid[SIJO_NARCH][SIJO_SH_MAXC] = {{ {} }};", if cids.is_empty() { "{ -1 }".into() } else { cids.join(", ") });
        let _ = writeln!(o, "static const float sijo_sh_halo[SIJO_NARCH] = {{ {} }};", if halo.is_empty() { "0.0f".into() } else { halo.join(", ") });
        let _ = writeln!(o, "static const uint8_t sijo_sh_spatial[SIJO_NARCH] = {{ {} }};", if spat.is_empty() { "0".into() } else { spat.join(", ") });
        let _ = writeln!(o, "static const uint64_t sijo_sh_halomask[SIJO_NARCH] = {{ {} }};", if hmask.is_empty() { "0".into() } else { hmask.join(", ") });
        // per stage: which local components partitioned systems may change
        let nst = ecs.stages.len().max(1);
        let mut rows = vec![];
        for st in 0..nst {
            let set = sp.stage_writes.get(st).cloned().unwrap_or_default();
            let row: Vec<String> = archs
                .iter()
                .map(|a| {
                    let m: u64 = ecs.arch_comps[a].iter().enumerate().filter(|(_, k)| set.contains(k)).fold(0, |m, (ci, _)| m | (1u64 << ci));
                    format!("{m}ull")
                })
                .collect();
            rows.push(format!("{{ {} }}", if row.is_empty() { "0".into() } else { row.join(", ") }));
        }
        let _ = writeln!(o, "static const uint64_t sijo_sh_swrite[{nst}][SIJO_NARCH] = {{ {} }};", rows.join(", "));
        // archetype position of each system's row loops (the command / spawn / print order key)
        o.push_str("static int sijo_sh_apos(int j, int arch) {\n  switch (j) {\n");
        for (i, s) in ecs.systems.iter().enumerate() {
            if s.archetypes.is_empty() {
                continue;
            }
            let _ = write!(o, "  case {i}: switch (arch) {{");
            for (pos, a) in s.archetypes.iter().enumerate() {
                let _ = write!(o, " case ARCH_{}: return {pos};", self.aname(*a));
            }
            o.push_str(" } return 0;\n");
        }
        o.push_str("  }\n  (void)arch; return 0;\n}\n");
        self.w(&o);

        self.dg_helpers("shw");
        self.dg_helpers("shr");
        // per archetype: table access, codecs, position
        for &a in &archs {
            let n = self.aname(a);
            let comps = ecs.arch_comps[&a].clone();
            let mut o = String::new();
            let _ = writeln!(o, "static SijoArch_{n}* sijo_sh_tab_{n}(SijoWorld* w, int which, int tid) {{ return which == 0 ? &w->{n} : which == 1 ? &w->p{n} : &w->pt{n}[tid]; }}");
            let _ = writeln!(o, "static void sijo_sh_pack_{n}(SijoWorld* w, SijoArch_{n}* t, int32_t r, int ci, SijoShW* o) {{\n  (void)w;\n  switch (ci) {{");
            for (ci, &k) in comps.iter().enumerate() {
                let mut body = String::new();
                for &f in &ecs.components[k].fields {
                    body.push_str(&self.sh_pack(&p.fields[f].ty, &format!("t->c{k}[r].{}", self.fname(f)), 0));
                    body.push(' ');
                }
                let _ = writeln!(o, "  case {ci}: {body}break;");
            }
            o.push_str("  }\n}\n");
            let _ = writeln!(o, "static void sijo_sh_unpack_{n}(SijoWorld* w, SijoArch_{n}* t, int32_t r, int ci, SijoShR* in, int main_) {{\n  (void)main_;\n  switch (ci) {{");
            for (ci, &k) in comps.iter().enumerate() {
                let mut body = String::new();
                for &f in &ecs.components[k].fields {
                    let place = format!("t->c{k}[r].{}", self.fname(f));
                    if p.fields[f].ty.is_list() {
                        body.push_str(&self.free_code(&p.fields[f].ty, &place, 0));
                        body.push(' ');
                    }
                    body.push_str(&self.sh_unpack(&p.fields[f].ty, &place, 0));
                    body.push(' ');
                }
                if ecs.components[k].replicated {
                    let _ = write!(body, "if (main_) t->d{k}[r] = 1; ");
                }
                let _ = writeln!(o, "  case {ci}: {body}break;");
            }
            o.push_str("  }\n}\n");
            match c.net.spatial.get(&a) {
                Some(&(f, xi)) => {
                    let k = self.comp(f);
                    let xname = match &p.fields[f].ty {
                        Ty::Struct(s) => cid(&p.structs[*s].fields[xi].name),
                        _ => String::new(),
                    };
                    let _ = writeln!(o, "static float sijo_sh_x_{n}(SijoArch_{n}* t, int32_t r) {{ return (float)t->c{k}[r].{}.{xname}; }}", self.fname(f));
                }
                None => {
                    let _ = writeln!(o, "static float sijo_sh_x_{n}(SijoArch_{n}* t, int32_t r) {{ (void)t; (void)r; return 0.0f; }}");
                }
            }
            self.w(&o);
        }
        // dispatchers over archetypes
        let mut o = String::new();
        let sw = |o: &mut String, head: &str, each: &dyn Fn(&str) -> String, tail: &str| {
            o.push_str(head);
            o.push_str("  switch (arch) {\n");
            for &a in &archs {
                let n = self.aname(a);
                let _ = writeln!(o, "  case ARCH_{n}: {}", each(&n));
            }
            o.push_str("  }\n");
            o.push_str(tail);
        };
        sw(&mut o, "static void sijo_sh_pack(SijoWorld* w, int which, int tid, int arch, int32_t r, int ci, SijoShW* o) {\n", &|n| format!("sijo_sh_pack_{n}(w, sijo_sh_tab_{n}(w, which, tid), r, ci, o); return;"), "}\n");
        sw(&mut o, "static void sijo_sh_unpack(SijoWorld* w, int which, int tid, int arch, int32_t r, int ci, SijoShR* in) {\n", &|n| format!("sijo_sh_unpack_{n}(w, sijo_sh_tab_{n}(w, which, tid), r, ci, in, which == 0); return;"), "}\n");
        sw(&mut o, "static float sijo_sh_x(SijoWorld* w, int which, int tid, int arch, int32_t r) {\n", &|n| format!("return sijo_sh_x_{n}(sijo_sh_tab_{n}(w, which, tid), r);"), "  return 0.0f;\n}\n");
        sw(&mut o, "static int32_t sijo_sh_count(SijoWorld* w, int which, int tid, int arch) {\n", &|n| format!("return sijo_sh_tab_{n}(w, which, tid)->count;"), "  return 0;\n}\n");
        sw(&mut o, "static SijoH sijo_sh_ent(SijoWorld* w, int arch, int32_t r) {\n", &|n| format!("return w->{n}.ent[r];"), "  return SIJO_NULL;\n}\n");
        // a fresh pending row (spawn record from another shard), or a row moved from a per-thread
        // spawn table (this shard's own spawn): the slot is allocated here, in the global order
        sw(&mut o, "static SijoH sijo_sh_push_pending(SijoWorld* w, int arch, int32_t* row) {\n", &|n| format!("{{ int32_t r = sijo_push_{n}(&w->p{n}); SijoH h = sijo_slot_alloc(&w->ents, ARCH_{n}, r, SIJO_PENDING); sijo_shard_reset(&w->ents, h.idx); w->p{n}.ent[r] = h; w->spawned++; *row = r; return h; }}"), "  *row = -1; return SIJO_NULL;\n}\n");
        o.push_str("static SijoH sijo_sh_move_pt(SijoWorld* w, int arch, int tid, int32_t i) {\n  switch (arch) {\n");
        for &a in &archs {
            let n = self.aname(a);
            let mut cp = String::new();
            for &k in &ecs.arch_comps[&a] {
                let _ = write!(cp, " w->p{n}.c{k}[r] = s->c{k}[i];");
            }
            let _ = writeln!(o, "  case ARCH_{n}: {{ SijoArch_{n}* s = &w->pt{n}[tid]; int32_t r = sijo_push_{n}(&w->p{n});{cp} SijoH h = sijo_slot_alloc(&w->ents, ARCH_{n}, r, SIJO_PENDING); sijo_shard_reset(&w->ents, h.idx); w->p{n}.ent[r] = h; w->spawned++; s->ent[i] = h; return h; }}");
        }
        o.push_str("  }\n  return SIJO_NULL;\n}\n");
        o.push_str("/* the per-thread spawn tables were merged by the shard runtime: empty them */\nstatic void sijo_sh_pt_clear(SijoWorld* w) {\n");
        for &a in &archs {
            let n = self.aname(a);
            let _ = writeln!(o, "  for (int t = 0; t < SIJO_MAX_THREADS; t++) if (w->pt{n}[t].count) {{ int32_t u = w->pt{n}[t].count; w->pt{n}[t].count = 0; sijo_trim_{n}(&w->pt{n}[t], u); }}");
        }
        o.push_str("}\n");
        // provisional handles this shard stored in fields of `this` (patched after the merge)
        o.push_str("static void sijo_sh_fix(SijoWorld* w) {\n");
        let mut done: BTreeSet<(ClassId, FieldId)> = BTreeSet::new();
        for (i, s) in ecs.systems.iter().enumerate() {
            if sp.mode[i] != Mode::Part || !sp.own_spawns.contains(&i) || !s.spawn_handles {
                continue;
            }
            for &a in &s.archetypes {
                let present = p.all_fields(a);
                for &f in &s.spawn_fix {
                    if !present.contains(&f) || ecs.is_dead(f) || !done.insert((a, f)) {
                        continue;
                    }
                    let n = self.aname(a);
                    let k = self.comp(f);
                    let _ = writeln!(o, "  for (int32_t r = 0; r < w->{n}.count; r++) {{ SijoH* h = &w->{n}.c{k}[r].{}; if (h->idx & SIJO_PROV) *h = sijo_sh_prov(w, w->sh_id, *h); }}", self.fname(f));
                }
            }
        }
        o.push_str("  (void)w;\n}\n");
        // the systems of each segment
        o.push_str("static void sijo_sh_seg_run(SijoWorld* w, int seg) {\n  switch (seg) {\n");
        for (si, seg) in sp.segments.iter().enumerate() {
            let _ = write!(o, "  case {si}:");
            for &j in &seg.systems {
                let call = if sp.mode[j] == Mode::Coord { format!("if (w->sh_id == 0) sys{j}(w);") } else { format!("sys{j}(w);") };
                let _ = write!(o, " w->sh_sys = {j}; {call} sijo_sh_mark(w, {j});");
            }
            o.push_str(" break;\n");
        }
        o.push_str("  }\n  w->sh_sys = -1;\n}\n");
        self.w(&o);
    }
}
