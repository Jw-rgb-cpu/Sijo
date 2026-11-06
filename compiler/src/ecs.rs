//! Automatic ECS planning: systems, component clustering, archetypes, hazards, scheduling.

use crate::ast::BinOp;
use crate::effects::{EffectInfo, Effects};
use crate::hir::*;
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Clone, Debug)]
pub struct Component {
    pub class: ClassId,
    pub cname: String,
    pub fields: Vec<FieldId>,
    pub signature: Vec<String>,
    pub replicated: bool,
    pub cold: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SysKind {
    Entity,
    Global,
    Client,
    ClientGlobal,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Res {
    Comp(ClassId, usize),
    Exist(ClassId),
    Stdout,
}

#[derive(Clone, Debug)]
pub struct SystemPlan {
    pub name: String,
    pub kind: SysKind,
    pub func: FuncId,
    pub archetypes: Vec<ClassId>,
    pub eff: Effects,
    pub reads: BTreeSet<Res>,
    pub writes: BTreeSet<Res>,
    pub defers: BTreeSet<Res>,
    pub structural: BTreeSet<Res>,
    /// components to double-buffer (component ids)
    pub hazards: Vec<usize>,
    pub hazard_fields: Vec<FieldId>,
    pub stage: usize,
    pub parallel: bool,
    /// spawns go to per-thread pending tables merged in row order
    pub spawn_par: bool,
    /// fields of `this` that may hold a provisional handle of an entity spawned by this system
    /// (patched to the final handle after the merge, with the targets of queued commands)
    pub spawn_fix: Vec<FieldId>,
    /// the system uses spawned handles: provisional handles are patched after the merge
    pub spawn_handles: bool,
    pub why_serial: Vec<String>,
    pub conflicts: Vec<(usize, String)>,
    /// rows from which the entity loop runs on several threads (SIJO_PAR_MIN = 256 by default; a
    /// profile sets it from the measured cost of a row and of opening a parallel region)
    pub par_min: usize,
    /// what the profile said about this system (for explain)
    pub pgo: Option<String>,
}

#[derive(Debug, Default)]
pub struct EcsPlan {
    pub components: Vec<Component>,
    pub field_comp: HashMap<FieldId, usize>,
    pub dead_fields: BTreeSet<FieldId>,
    pub replicated: BTreeSet<FieldId>,
    /// archetype -> component ids (base class components first)
    pub arch_comps: BTreeMap<ClassId, Vec<usize>>,
    pub archetypes: Vec<ClassId>,
    pub systems: Vec<SystemPlan>,
    pub stages: Vec<Vec<usize>>,
    pub client_systems: Vec<usize>,
    /// archetypes whose existence is replicated to clients
    pub replicated_archetypes: BTreeSet<ClassId>,
    pub start_eff: Effects,
    /// a profile guided this plan (PGO): its fork cost and what it changed, for explain
    pub profile: Option<crate::profile::Profile>,
    pub pgo_notes: Vec<String>,
}

impl EcsPlan {
    pub fn comp_of(&self, f: FieldId) -> Option<usize> {
        self.field_comp.get(&f).copied()
    }
    pub fn is_dead(&self, f: FieldId) -> bool {
        self.dead_fields.contains(&f)
    }
}

pub fn plan(p: &Program, info: &EffectInfo, d: &mut crate::diag::Diags) -> EcsPlan {
    plan_impl(p, info, d, false, None, None)
}

/// Development build: no dead-field elimination (state must survive live reloads).
pub fn plan_keep_all(p: &Program, info: &EffectInfo, d: &mut crate::diag::Diags) -> EcsPlan {
    plan_impl(p, info, d, true, None, None)
}

/// `live`: fields that keep their storage even when the optimized program no longer reads them
/// (the reads the e-graph proved unnecessary: the stored world stays the same without it)
pub fn plan_live(p: &Program, info: &EffectInfo, d: &mut crate::diag::Diags, keep_all: bool, live: &BTreeSet<FieldId>) -> EcsPlan {
    plan_impl(p, info, d, keep_all, Some(live), None)
}

/// plan_live guided by an execution profile (PGO)
pub fn plan_profiled(p: &Program, info: &EffectInfo, d: &mut crate::diag::Diags, keep_all: bool, live: &BTreeSet<FieldId>, prof: Option<&crate::profile::Profile>) -> EcsPlan {
    plan_impl(p, info, d, keep_all, Some(live), prof)
}

/// entity rows from which a system's loop runs in parallel, without a profile
pub const DEFAULT_PAR_MIN: usize = 256;

/// a system is hot when it took at least this share of the profiled tick
const HOT_SHARE: f64 = 0.05;

fn plan_impl(p: &Program, info: &EffectInfo, d: &mut crate::diag::Diags, keep_all: bool, live: Option<&BTreeSet<FieldId>>, prof: Option<&crate::profile::Profile>) -> EcsPlan {
    let mut plan = EcsPlan::default();
    plan.archetypes = p.entity_classes().into_iter().filter(|&c| !p.classes[c].is_abstract).collect();

    // ---- 1. systems
    let mut systems: Vec<SystemPlan> = vec![];
    let mk = |name: String, kind: SysKind, func: FuncId, archetypes: Vec<ClassId>, eff: Effects| SystemPlan {
        name,
        kind,
        func,
        archetypes,
        eff,
        reads: BTreeSet::new(),
        writes: BTreeSet::new(),
        defers: BTreeSet::new(),
        structural: BTreeSet::new(),
        hazards: vec![],
        hazard_fields: vec![],
        stage: 0,
        parallel: false,
        spawn_par: false,
        spawn_fix: vec![],
        spawn_handles: false,
        why_serial: vec![],
        conflicts: vec![],
        par_min: DEFAULT_PAR_MIN,
        pgo: None,
    };
    for c in p.entity_classes() {
        for (mname, &f) in sorted_methods(p, c) {
            let func = &p.funcs[f];
            // a coroutine runs on its own unless another coroutine awaits it (or it takes arguments)
            let own_coroutine = func.co.is_some() && !p.awaited.contains(&f) && func.params.is_empty();
            let is_server_sys = (mname == "Update" || own_coroutine) && func.ctx == Ctx::Server;
            let is_client_sys = func.ctx == Ctx::Client && func.params.is_empty() && func.ret == Ty::Void;
            if !is_server_sys && !is_client_sys {
                continue;
            }
            // the archetypes that run this very method (a component's methods run on every
            // combination that has the component)
            let archs: Vec<ClassId> = p.archetypes_of(c).into_iter().filter(|&a| p.target_of(a, f) == f).collect();
            if archs.is_empty() {
                continue;
            }
            let mut eff = Effects::default();
            for &a in &archs {
                eff.merge(&info.get(f, Some(a)));
            }
            let kind = if is_server_sys { SysKind::Entity } else { SysKind::Client };
            systems.push(mk(format!("{}.{}", p.classes[c].name, mname), kind, f, archs, eff));
        }
    }
    if let Some(g) = p.game {
        if let Some(f) = p.game_update {
            systems.push(mk("Game.Update".into(), SysKind::Global, f, vec![], info.get(f, None)));
        }
        for (mname, &f) in sorted_methods(p, g) {
            let func = &p.funcs[f];
            if func.ctx == Ctx::Client && func.params.is_empty() && func.ret == Ty::Void {
                systems.push(mk(format!("Game.{}", mname), SysKind::ClientGlobal, f, vec![], info.get(f, None)));
            }
        }
    }
    if let Some(s) = p.game_start {
        plan.start_eff = info.get(s, None);
    }

    // ---- 2. replication + dead fields
    // a field is observable only if some expression reads it for a purpose other than
    // updating itself (`Shots++`, `Hp -= 1`, `Trail.Add(x)` alone do not make it live)
    let mut read_anywhere = observed_reads(p);
    // what the store keeps is read when the program saves it
    for (_, key, pers, flag) in &p.persist {
        read_anywhere.insert(*key);
        read_anywhere.insert(*flag);
        read_anywhere.extend(pers.iter().copied());
    }
    // the presence bits of open tables are read by Has<C>(), queries and component systems
    read_anywhere.extend(p.open.values().copied());
    for s in systems.iter().filter(|s| matches!(s.kind, SysKind::Client | SysKind::ClientGlobal)) {
        plan.replicated.extend(&s.eff.self_reads);
        plan.replicated.extend(&s.eff.remote_reads);
        for &a in &s.archetypes {
            plan.replicated_archetypes.insert(a);
        }
        for &q in &s.eff.queries {
            for a in p.archetypes_of(q) {
                plan.replicated_archetypes.insert(a);
            }
        }
    }
    for (fid, f) in p.fields.iter().enumerate() {
        match f.attrs.replicated {
            Some(true) => {
                plan.replicated.insert(fid);
            }
            Some(false) => {
                if plan.replicated.remove(&fid) {
                    d.error("E0604", f.span, format!("'{}' is marked [NotReplicated] but is read by client code", p.field_label(fid)));
                }
            }
            None => {}
        }
        if plan.replicated.contains(&fid) && p.has_delegate(&f.ty) {
            d.error_note(
                "E0268",
                f.span,
                format!("'{}' is read by client code, but it holds a delegate", p.field_label(fid)),
                "delegates run on the server and are not replicated: give client code the values it shows (a number, a string, an enum)",
            );
            plan.replicated.remove(&fid);
        }
        if f.ty.is_list() && plan.replicated.contains(&fid) && !list_replicable(p, &f.ty) {
            // a client sees only the entities it is sent: any element may read as null there
            d.error_note(
                "E0606",
                f.span,
                format!("'{}' is read by client code, but '{}' holds non-nullable entity references", p.field_label(fid), p.ty_name(&f.ty)),
                format!(
                    "a client reads a reference as null when it does not see the target (destroyed, or outside its view): declare it '{}'",
                    p.ty_name(&nullable_refs(&f.ty))
                ),
            );
            plan.replicated.remove(&fid);
        }
    }
    for &f in &plan.replicated {
        for a in p.archetypes_of(p.fields[f].class) {
            plan.replicated_archetypes.insert(a);
        }
    }
    for fid in 0..p.fields.len() {
        let pinned = live.map(|l| l.contains(&fid)).unwrap_or(false);
        if !keep_all && !read_anywhere.contains(&fid) && !plan.replicated.contains(&fid) && !pinned {
            plan.dead_fields.insert(fid);
        }
    }

    // ---- 3. component clustering by access signature
    let mut access: HashMap<FieldId, BTreeSet<usize>> = HashMap::new();
    for (si, s) in systems.iter().enumerate() {
        let e = &s.eff;
        for f in e.self_reads.iter().chain(&e.self_writes).chain(&e.remote_reads).chain(e.remote_writes.keys()).chain(&e.msg_writes) {
            access.entry(*f).or_default().insert(si);
        }
    }
    // fields reached at arbitrary rows (through entity handles, deferred commands and messages, or in
    // methods other entities call) keep their access-signature group: a handle access then touches
    // one record. Groups that systems only stream (their own row, query cursors) are split into one
    // column per field, so loops read only the arrays they use and self-only updates vectorize.
    let random = random_access_fields(p, info, &systems);
    for c in p.entity_classes() {
        let mut groups: Vec<(String, Vec<FieldId>, BTreeSet<usize>, bool)> = vec![];
        for &f in &p.classes[c].own_fields {
            if plan.dead_fields.contains(&f) {
                continue;
            }
            let sig = access.get(&f).cloned().unwrap_or_default();
            let repl = plan.replicated.contains(&f);
            let key = match &p.fields[f].attrs.component {
                Some(n) => format!("name:{}", n),
                // a replicated string is resent whenever its component changes: keep it alone
                None if repl && (p.contains_str(&p.fields[f].ty) || p.fields[f].ty.is_list()) => format!("str:{}", f),
                None => format!("sig:{:?}:{}", sig, repl),
            };
            match groups.iter_mut().find(|g| g.0 == key) {
                Some(g) => g.1.push(f),
                None => groups.push((key, vec![f], sig, repl)),
            }
        }
        let mut split = vec![];
        for (key, fields, sig, repl) in groups {
            let streamed = key.starts_with("sig:") && !sig.is_empty() && fields.iter().all(|f| !random.contains(f));
            if streamed && fields.len() > 1 {
                for f in fields {
                    split.push((format!("{}:{}", key, f), vec![f], sig.clone(), repl));
                }
            } else if let Some((h, c)) = prof.filter(|_| key.starts_with("sig:") && fields.len() > 1).and_then(|pr| count_split(p, pr, &fields)) {
                // PGO with field counts: fields accessed far more often than the others of their
                // record get a record of their own (the rarely used ones stop sharing cache lines)
                let names = |v: &[FieldId]| v.iter().map(|&f| p.fields[f].name.clone()).collect::<Vec<_>>().join(", ");
                let cnt = |v: &[FieldId]| v.iter().map(|&f| prof.and_then(|pr| pr.field_count(&p.field_label(f))).unwrap_or(0)).sum::<u64>();
                plan.pgo_notes.push(format!("{}: {{ {} }} split from {{ {} }} ({} vs {} accesses in the profile)", p.classes[c_of(p, &h)].name, names(&h), names(&c), cnt(&h), cnt(&c)));
                split.push((format!("{}:hot", key), h, sig.clone(), repl));
                split.push((format!("{}:cold", key), c, sig, repl));
            } else if let (true, Some(hot)) = (key.starts_with("sig:") && fields.len() > 1, prof.map(|pr| hot_loop_fields(p, &systems, pr))) {
                // PGO: fields that hot systems read for every pair of a query loop get a record of
                // their own, so the loop streams only them (the others still share one record)
                let (h, c): (Vec<FieldId>, Vec<FieldId>) = fields.iter().partition(|f| hot.contains(f));
                if !h.is_empty() && !c.is_empty() {
                    let names = |v: &[FieldId]| v.iter().map(|&f| p.fields[f].name.clone()).collect::<Vec<_>>().join(", ");
                    plan.pgo_notes.push(format!("{}: {{ {} }} split from {{ {} }} (read in a hot query loop; profile)", p.classes[c_of(p, &h)].name, names(&h), names(&c)));
                    split.push((format!("{}:hot", key), h, sig.clone(), repl));
                    split.push((format!("{}:cold", key), c, sig, repl));
                } else {
                    split.push((key, fields, sig, repl));
                }
            } else {
                split.push((key, fields, sig, repl));
            }
        }
        let groups = split;
        for (i, (key, fields, sig, repl)) in groups.into_iter().enumerate() {
            let mut signature: Vec<String> = sig.iter().map(|&si| systems[si].name.clone()).collect();
            if repl {
                signature.push("replicated".into());
            }
            let cold = sig.is_empty();
            let cname = match key.strip_prefix("name:") {
                Some(n) => format!("{}_{}", p.classes[c].name, n),
                None => format!("{}_C{}", p.classes[c].name, i),
            };
            // a component is replicated as a unit only if all its fields are
            let all_repl = fields.iter().all(|f| plan.replicated.contains(f));
            plan.components.push(Component { class: c, cname, fields: fields.clone(), signature, replicated: repl && all_repl, cold });
            let id = plan.components.len() - 1;
            for f in fields {
                plan.field_comp.insert(f, id);
            }
        }
    }
    // components with mixed replication (only via [Component] override) are not allowed
    for comp in &plan.components {
        let n = comp.fields.iter().filter(|f| plan.replicated.contains(f)).count();
        if n > 0 && n < comp.fields.len() {
            d.error("E0607", p.fields[comp.fields[0]].span, format!("component '{}' mixes replicated and non-replicated fields", comp.cname));
        }
    }
    for a in plan.archetypes.clone() {
        // base classes first, then the components of a combination
        let chain = p.layout_chain(a);
        let mut comps: Vec<usize> = vec![];
        for &x in &chain {
            for k in 0..plan.components.len() {
                if plan.components[k].class == x {
                    comps.push(k);
                }
            }
        }
        plan.arch_comps.insert(a, comps);
    }

    // ---- 4. resources, hazards, parallelism
    for s in systems.iter_mut() {
        let e = s.eff.clone();
        let comp = |f: &FieldId| plan.field_comp.get(f).copied();
        for f in &e.self_reads {
            if let Some(k) = comp(f) {
                for &a in &s.archetypes {
                    s.reads.insert(Res::Comp(a, k));
                }
            }
        }
        for f in &e.self_writes {
            if let Some(k) = comp(f) {
                for &a in &s.archetypes {
                    s.writes.insert(Res::Comp(a, k));
                }
            }
        }
        for f in &e.remote_reads {
            if let Some(k) = comp(f) {
                for a in p.archetypes_of(p.fields[*f].class) {
                    s.reads.insert(Res::Comp(a, k));
                }
            }
        }
        for f in e.remote_writes.keys().chain(&e.msg_writes) {
            if let Some(k) = comp(f) {
                for a in p.archetypes_of(p.fields[*f].class) {
                    s.defers.insert(Res::Comp(a, k));
                }
            }
        }
        for q in &e.queries {
            for a in p.archetypes_of(*q) {
                s.reads.insert(Res::Exist(a));
            }
        }
        let mut structural_archs: BTreeSet<ClassId> = BTreeSet::new();
        for c in e.spawns.iter().chain(&e.destroys) {
            structural_archs.extend(p.archetypes_of(*c));
        }
        if e.destroys_self {
            structural_archs.extend(s.archetypes.iter().copied());
        }
        // adding or removing a component moves rows between the tables of a class: every table
        // of the class may change
        if e.restructures_self {
            for &a in &s.archetypes {
                structural_archs.extend(p.family(a));
            }
        }
        for &c in &e.restructures {
            for a in p.archetypes_of(c) {
                structural_archs.extend(p.family(a));
            }
        }
        for a in structural_archs {
            s.structural.insert(Res::Exist(a));
            for &k in plan.arch_comps.get(&a).map(|v| v.as_slice()).unwrap_or(&[]) {
                s.structural.insert(Res::Comp(a, k));
            }
        }
        if e.prints {
            s.writes.insert(Res::Stdout);
        }
        if s.kind == SysKind::Entity {
            let hz: BTreeSet<FieldId> = e.self_writes.intersection(&e.remote_reads).copied().collect();
            let mut comps: BTreeSet<usize> = BTreeSet::new();
            for f in &hz {
                if let Some(k) = comp(f) {
                    comps.insert(k);
                }
            }
            s.hazard_fields = hz.into_iter().collect();
            s.hazards = comps.into_iter().collect();
            if !e.spawns.is_empty() {
                if spawns_parallel_ok(p, s.func) {
                    // ids are assigned when the per-thread spawn tables are merged in row order
                    s.spawn_par = true;
                } else if let Some(fix) = spawn_handles_patchable(p, s.func) {
                    // the handle is used in ways that can be patched after the merge
                    s.spawn_par = true;
                    s.spawn_handles = true;
                    s.spawn_fix = fix.into_iter().collect();
                } else {
                    s.why_serial.push("spawns entities and uses the returned handle beyond storing it in a field of this, comparing it or sending to it (or spawns inside atomic): ids are assigned in order".into());
                }
            }
            if e.prints {
                s.why_serial.push("prints (output order)".into());
            }
            s.parallel = s.why_serial.is_empty();
        } else {
            s.why_serial.push(match s.kind {
                SysKind::Global => "global system (runs once per tick)".into(),
                _ => "client system".into(),
            });
        }
    }

    // ---- 4b. PGO: the parallel threshold from the measured cost of a row
    if let Some(pr) = prof {
        plan.profile = Some(pr.clone());
        let fork = pr.fork_ns.max(200) as f64;
        for s in systems.iter_mut().filter(|s| s.kind == SysKind::Entity) {
            if let Some(c) = pr.ns_per_row(&s.name) {
                // parallel pays when the work is at least twice the cost of opening a region
                let min = ((2.0 * fork / c.max(0.01)).ceil() as usize).clamp(16, 1 << 24);
                s.par_min = min;
                s.pgo = Some(format!("profile: {:.1} ns per row, {:.0}% of the tick; a parallel region costs {:.1} us -> parallel from {} rows", c, 100.0 * pr.share(&s.name), fork / 1000.0, min));
            }
        }
        let sig = crate::profile::signature(systems.iter().filter(|s| matches!(s.kind, SysKind::Entity | SysKind::Global)).map(|s| s.name.as_str()));
        if sig != pr.signature {
            plan.pgo_notes.push("the profile comes from another version of the program: systems are matched by name".into());
        }
    }

    // ---- 5. stage scheduling (server systems, source order)
    let server: Vec<usize> = (0..systems.len()).filter(|&i| matches!(systems[i].kind, SysKind::Entity | SysKind::Global)).collect();
    for (pos, &b) in server.iter().enumerate() {
        let mut stage = 0;
        let mut conflicts = vec![];
        for &a in &server[..pos] {
            if let Some(why) = conflict(&systems[a], &systems[b], p, &plan) {
                stage = stage.max(systems[a].stage + 1);
                conflicts.push((a, why));
            }
        }
        systems[b].stage = stage;
        systems[b].conflicts = conflicts;
    }
    let nstages = server.iter().map(|&i| systems[i].stage + 1).max().unwrap_or(0);
    plan.stages = vec![vec![]; nstages];
    for &i in &server {
        plan.stages[systems[i].stage].push(i);
    }
    plan.client_systems = (0..systems.len()).filter(|&i| matches!(systems[i].kind, SysKind::Client | SysKind::ClientGlobal)).collect();
    plan.systems = systems;
    plan
}

/// every method of a class (overloads included) in declaration order
fn sorted_methods(p: &Program, c: ClassId) -> Vec<(String, &FuncId)> {
    let cd = &p.classes[c];
    let mut v: Vec<(String, &FuncId)> = cd.methods.iter().map(|(n, f)| (n.clone(), f)).chain(cd.overloads.iter().map(|f| (p.funcs[*f].name.clone(), f))).collect();
    v.sort_by_key(|(_, f)| (p.funcs[**f].span.lo, **f));
    v
}

fn res_name(p: &Program, plan: &EcsPlan, r: &Res) -> String {
    match r {
        Res::Comp(a, k) => format!("{}:{}", p.classes[*a].name, plan.components[*k].cname),
        Res::Exist(a) => format!("{} (existence)", p.classes[*a].name),
        Res::Stdout => "stdout".into(),
    }
}

/// Returns a reason if the two systems may not share a stage.
fn conflict(a: &SystemPlan, b: &SystemPlan, p: &Program, plan: &EcsPlan) -> Option<String> {
    let mutating = |s: &SystemPlan| -> BTreeSet<Res> { s.writes.union(&s.structural).cloned().collect() };
    let touching = |s: &SystemPlan| -> BTreeSet<Res> {
        s.reads.iter().chain(&s.writes).chain(&s.defers).chain(&s.structural).cloned().collect()
    };
    let check = |x: &BTreeSet<Res>, y: &BTreeSet<Res>, what: &str| -> Option<String> {
        x.intersection(y).next().map(|r| format!("{} {}", what, res_name(p, plan, r)))
    };
    check(&mutating(a), &touching(b), "writes")
        .or_else(|| check(&mutating(b), &touching(a), "is written by later system:"))
        .or_else(|| check(&a.defers, &b.reads, "defers writes to"))
        .or_else(|| check(&b.defers, &a.reads, "reads what the later system defers:"))
}

/// Entity fields read anywhere, excluding read-modify-write of the field itself.
pub fn observed_reads(p: &Program) -> BTreeSet<FieldId> {
    fn walk(e: &Expr, out: &mut BTreeSet<FieldId>) {
        match &e.kind {
            ExprKind::Assign { target, value, .. } => {
                walk_place(target, out);
                walk(value, out);
            }
            ExprKind::IncDec { target, .. } => walk_place(target, out),
            ExprKind::ListCall { list, op, args } => {
                if !op.mutates() {
                    walk(list, out);
                } else {
                    walk_place(list, out);
                }
                for a in args {
                    walk(a, out);
                }
            }
            ExprKind::EField { base, field } => {
                out.insert(*field);
                walk(base, out);
            }
            ExprKind::DictSet { dict, key, value } => {
                walk_place(dict, out);
                walk(key, out);
                walk(value, out);
            }
            ExprKind::DictCall { dict, op, args } if op.mutates() => {
                walk_place(dict, out);
                for a in args {
                    walk(a, out);
                }
            }
            _ => {
                for c in crate::sema::children(e) {
                    walk(c, out);
                }
            }
        }
    }
    // the place itself is not an observation, but sub-expressions (bases, indices) are
    fn walk_place(e: &Expr, out: &mut BTreeSet<FieldId>) {
        match &e.kind {
            ExprKind::EField { base, .. } => walk(base, out),
            ExprKind::Field { base, .. } => walk_place(base, out),
            // xs[i] = v checks i against the length: the collection is observed (a field that is
            // only ever written this way stays, so the out-of-range panic does too)
            ExprKind::ListIndex { list, index, .. } => {
                walk(list, out);
                walk(index, out);
            }
            _ => walk(e, out),
        }
    }
    let mut out = BTreeSet::new();
    for f in &p.funcs {
        visit_stmts(&f.body, &mut |e| walk(e, &mut out));
        if let Some(co) = &f.co {
            out.insert(co.state); // read by the resume dispatch
        }
    }
    out
}

/// Calls `f` on every top-level expression of every statement (not recursing into expressions).
pub fn visit_stmts(b: &Block, f: &mut dyn FnMut(&Expr)) {
    for s in &b.stmts {
        match s {
            Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => f(e),
            Stmt::If { cond, then, els } => {
                f(cond);
                visit_stmts(then, f);
                if let Some(e) = els {
                    visit_stmts(e, f);
                }
            }
            Stmt::Loop { cond, step, body, .. } => {
                if let Some(c) = cond {
                    f(c);
                }
                for e in step {
                    f(e);
                }
                visit_stmts(body, f);
            }
            Stmt::ForList { list, body, .. } => {
                f(list);
                visit_stmts(body, f);
            }
            Stmt::ForQuery { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => visit_stmts(body, f),
            Stmt::Switch { scrut, sections, default, .. } => {
                f(scrut);
                for (c, b) in sections {
                    f(c);
                    visit_stmts(b, f);
                }
                if let Some(d) = default {
                    visit_stmts(d, f);
                }
            }
            Stmt::Await { .. } => {
                for e in s.await_exprs() {
                    f(e);
                }
            }
            _ => {}
        }
    }
}

/// Can a system that spawns run in parallel? Yes when every Spawn reachable from it (through its own
/// and self/static helper calls) is a statement whose handle is discarded and none is inside atomic:
/// the spawns then go to per-thread tables merged in row order, so ids match the serial order.
fn spawns_parallel_ok(p: &Program, sys: FuncId) -> bool {
    let mut seen = BTreeSet::new();
    let mut stack = vec![sys];
    while let Some(f) = stack.pop() {
        if !seen.insert(f) {
            continue;
        }
        if !block_spawns_ok(&p.funcs[f].body, false) {
            return false;
        }
        crate::sema::visit_block(&p.funcs[f].body, &mut |e| {
            if let ExprKind::Call { func, dispatch: Dispatch::Static | Dispatch::SelfCall | Dispatch::BaseCall, .. } = &e.kind {
                stack.push(*func);
            }
            if let ExprKind::OpCall { func, .. } = &e.kind {
                stack.push(*func);
            }
            if let ExprKind::StructLit { ctor: Some(c), .. } = &e.kind {
                stack.push(*c);
            }
        });
        // virtual calls on self resolve per archetype: check the overrides in subclasses too
        if let FuncKind::EntityMethod(c) = p.funcs[f].kind {
            for a in p.archetypes_of(c) {
                stack.push(p.target_of(a, f));
            }
        }
    }
    true
}

fn has_spawn(e: &Expr) -> bool {
    let mut found = false;
    crate::sema::visit_expr(e, &mut |x| found |= matches!(x.kind, ExprKind::Spawn { .. }));
    found
}

fn block_spawns_ok(b: &Block, in_atomic: bool) -> bool {
    for s in &b.stmts {
        let ok = match s {
            Stmt::Expr(Expr { kind: ExprKind::Spawn { inits, .. }, .. }) => !in_atomic && !inits.iter().any(|(_, x)| has_spawn(x)),
            Stmt::Atomic { body, .. } => block_spawns_ok(body, true),
            other => {
                let mut exprs_ok = true;
                match other {
                    Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => exprs_ok = !has_spawn(e),
                    Stmt::If { cond, .. } => exprs_ok = !has_spawn(cond),
                    Stmt::Loop { cond, step, .. } => exprs_ok = !cond.as_ref().map(has_spawn).unwrap_or(false) && !step.iter().any(has_spawn),
                    Stmt::ForList { list, .. } => exprs_ok = !has_spawn(list),
                    Stmt::Switch { scrut, sections, .. } => exprs_ok = !has_spawn(scrut) && !sections.iter().any(|(c, _)| has_spawn(c)),
                    Stmt::Await { .. } => exprs_ok = !other.await_exprs().into_iter().any(has_spawn),
                    _ => {}
                }
                exprs_ok && other.sub_blocks().into_iter().all(|x| block_spawns_ok(x, in_atomic))
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Can a list field be replicated (as canonical bytes)? Not with entity references or dictionaries.
/// `ty` with every entity reference in it nullable
fn nullable_refs(ty: &Ty) -> Ty {
    match ty {
        Ty::Handle(c) => Ty::NHandle(*c),
        Ty::List(e) => Ty::List(Box::new(nullable_refs(e))),
        Ty::Array(e) => Ty::Array(Box::new(nullable_refs(e))),
        Ty::Dict(k, v) => Ty::Dict(Box::new(nullable_refs(k)), Box::new(nullable_refs(v))),
        t => t.clone(),
    }
}

/// Can a list, array or dictionary field travel to clients? Elements of every kind replicate as
/// canonical bytes, entity references as server slots that the client maps back to its handles
/// (null when it does not see the target): so references must be nullable.
fn list_replicable(p: &Program, ty: &Ty) -> bool {
    match ty {
        Ty::List(el) | Ty::Array(el) | Ty::MdArray(el, _) => list_replicable(p, el),
        Ty::Dict(..) => p.elem_of(ty).is_some_and(|e| list_replicable(p, &e)),
        Ty::Handle(_) | Ty::Null => false,
        Ty::Struct(s) => p.structs[*s].fields.iter().all(|f| list_replicable(p, &f.ty)),
        _ => true,
    }
}

/// Entity fields accessed at arbitrary rows: read through an entity handle (not `this`, not a query
/// cursor), written by deferred commands or messages, or touched on `this` by a method that other
/// entities call (it runs on their target's row).
fn random_access_fields(p: &Program, info: &EffectInfo, systems: &[SystemPlan]) -> BTreeSet<FieldId> {
    let mut out = BTreeSet::new();
    for s in systems {
        out.extend(s.eff.remote_writes.keys().copied());
        out.extend(s.eff.msg_writes.iter().copied());
    }
    for f in &p.funcs {
        crate::sema::visit_block(&f.body, &mut |e| match &e.kind {
            ExprKind::EField { base, field } => {
                let streamed = match &base.kind {
                    ExprKind::SelfRef => true,
                    ExprKind::Local(l) => matches!(f.locals.get(*l).map(|x| x.kind), Some(LocalKind::Cursor(_))),
                    _ => false,
                };
                if !streamed {
                    out.insert(*field);
                }
            }
            ExprKind::Call { func, recv: Some(r), dispatch: Dispatch::RemoteRead | Dispatch::RemoteDeferred | Dispatch::Rpc, .. } => {
                let class = r.ty.class_of().unwrap_or(0);
                for b in p.archetypes_of(class) {
                    let g = p.target_of(b, *func);
                    let s = info.get(g, Some(b));
                    out.extend(s.self_reads.iter().copied());
                    out.extend(s.self_writes.iter().copied());
                }
            }
            _ => {}
        });
    }
    out
}

/// Functions a system runs on its entity's behalf: itself, the methods it calls on `this` (all
/// overrides) and the static functions, operators and constructors it calls.
pub(crate) fn reachable_funcs(p: &Program, sys: FuncId) -> Vec<FuncId> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![sys];
    let mut out = vec![];
    while let Some(f) = stack.pop() {
        if !seen.insert(f) {
            continue;
        }
        out.push(f);
        crate::sema::visit_block(&p.funcs[f].body, &mut |e| match &e.kind {
            ExprKind::Call { func, dispatch: Dispatch::Static | Dispatch::SelfCall | Dispatch::BaseCall, .. } => stack.push(*func),
            ExprKind::OpCall { func, .. } => stack.push(*func),
            ExprKind::StructLit { ctor: Some(c), .. } => stack.push(*c),
            _ => {}
        });
        if let FuncKind::EntityMethod(c) = p.funcs[f].kind {
            for a in p.archetypes_of(c) {
                stack.push(p.target_of(a, f));
            }
        }
    }
    out
}

/// A system that uses the handles it spawns can still run in parallel when every use can be
/// patched once the per-thread spawn tables are merged: the handle (from Spawn, a local holding
/// it, or a field of `this` it was stored in) may only be stored in a handle field of `this` or a
/// local, compared with == / !=, tested with `is` (false for an entity spawned in this system, as
/// in a serial run), or be the target of a message, a remote write or Destroy. Returns the fields
/// of `this` to patch, or None. Spawns inside atomic keep the system serial.
fn spawn_handles_patchable(p: &Program, sys: FuncId) -> Option<BTreeSet<FieldId>> {
    let funcs = reachable_funcs(p, sys);
    for &f in &funcs {
        if !block_spawns_atomic_free(&p.funcs[f].body, false) {
            return None;
        }
    }
    let mut fix: BTreeSet<FieldId> = BTreeSet::new();
    // fields and locals may receive provisional handles from each other: iterate to a fixpoint
    loop {
        let before = fix.len();
        for &f in &funcs {
            let mut tainted: BTreeSet<LocalId> = BTreeSet::new();
            loop {
                let n = tainted.len();
                let ok = scan_block(p, &p.funcs[f].body, &fix, &mut tainted, &mut BTreeSet::new(), true);
                if !ok {
                    return None;
                }
                if tainted.len() == n {
                    break;
                }
            }
            let mut new_fix = BTreeSet::new();
            if !scan_block(p, &p.funcs[f].body, &fix, &mut tainted, &mut new_fix, false) {
                return None;
            }
            fix.extend(new_fix);
        }
        if fix.len() == before {
            break;
        }
    }
    Some(fix)
}

fn block_spawns_atomic_free(b: &Block, in_atomic: bool) -> bool {
    b.stmts.iter().all(|s| match s {
        Stmt::Atomic { body, .. } => block_spawns_atomic_free(body, true),
        other => {
            let mut spawn = false;
            crate::sema::visit_stmt(other, &mut |e| spawn |= matches!(e.kind, ExprKind::Spawn { .. }));
            !(in_atomic && spawn) && other.sub_blocks().into_iter().all(|x| block_spawns_atomic_free(x, in_atomic))
        }
    })
}

/// is this expression a (possibly) provisional handle value?
fn provisional(e: &Expr, fix: &BTreeSet<FieldId>, tainted: &BTreeSet<LocalId>) -> bool {
    match &e.kind {
        ExprKind::Spawn { .. } => true,
        ExprKind::Local(l) => tainted.contains(l),
        ExprKind::EField { base, field } => matches!(base.kind, ExprKind::SelfRef) && fix.contains(field),
        _ => false,
    }
}

/// Walks a block checking every provisional value is in an allowed position. `grow` collects
/// locals that receive one (Let / assignment); otherwise `new_fix` collects fields of `this`
/// that receive one. Returns false on any other use.
fn scan_block(p: &Program, b: &Block, fix: &BTreeSet<FieldId>, tainted: &mut BTreeSet<LocalId>, new_fix: &mut BTreeSet<FieldId>, grow: bool) -> bool {
    for s in &b.stmts {
        let ok = match s {
            Stmt::Let { local, init: Some(e) } if provisional(e, fix, tainted) => {
                if grow {
                    tainted.insert(*local);
                }
                scan_expr_children(p, e, fix, tainted, new_fix, grow)
            }
            Stmt::Expr(e) if matches!(e.kind, ExprKind::Spawn { .. }) => scan_expr_children(p, e, fix, tainted, new_fix, grow),
            Stmt::Return(Some(e), _) if provisional(e, fix, tainted) => false,
            _ => {
                let mut ok = true;
                for e in stmt_exprs(s) {
                    ok &= scan_expr(p, e, fix, tainted, new_fix, grow);
                }
                ok && s.sub_blocks().into_iter().all(|x| scan_block(p, x, fix, tainted, new_fix, grow))
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

fn stmt_exprs(s: &Stmt) -> Vec<&Expr> {
    match s {
        Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => vec![e],
        Stmt::If { cond, .. } => vec![cond],
        Stmt::Loop { cond, step, .. } => cond.iter().chain(step.iter()).collect(),
        Stmt::ForList { list, .. } => vec![list],
        Stmt::Switch { scrut, sections, .. } => std::iter::once(scrut).chain(sections.iter().map(|(c, _)| c)).collect(),
        Stmt::Await { .. } => s.await_exprs(),
        _ => vec![],
    }
}

/// children of `e` in value positions (a provisional child is an error there)
fn scan_expr_children(p: &Program, e: &Expr, fix: &BTreeSet<FieldId>, tainted: &mut BTreeSet<LocalId>, new_fix: &mut BTreeSet<FieldId>, grow: bool) -> bool {
    crate::sema::children(e).into_iter().all(|c| !provisional(c, fix, tainted) && scan_expr(p, c, fix, tainted, new_fix, grow))
}

fn scan_expr(p: &Program, e: &Expr, fix: &BTreeSet<FieldId>, tainted: &mut BTreeSet<LocalId>, new_fix: &mut BTreeSet<FieldId>, grow: bool) -> bool {
    let prov = |x: &Expr, t: &BTreeSet<LocalId>| provisional(x, fix, t);
    match &e.kind {
        // stored into a handle field of this / a local: patched after the merge / tracked
        ExprKind::Assign { op: None, target, value } if prov(value, tainted) => {
            match &target.kind {
                ExprKind::EField { base, field } if matches!(base.kind, ExprKind::SelfRef) && p.fields[*field].ty.is_handle() => {
                    if !grow {
                        new_fix.insert(*field);
                    }
                }
                ExprKind::Local(l) => {
                    if grow {
                        tainted.insert(*l);
                    }
                }
                _ => return false,
            }
            scan_expr_children(p, value, fix, tainted, new_fix, grow)
        }
        // comparisons and type tests
        ExprKind::Binary(BinOp::Eq | BinOp::Ne, a, b) => {
            (prov(a, tainted) || scan_expr(p, a, fix, tainted, new_fix, grow)) && (prov(b, tainted) || scan_expr(p, b, fix, tainted, new_fix, grow))
        }
        ExprKind::Is { expr, .. } => prov(expr, tainted) || scan_expr(p, expr, fix, tainted, new_fix, grow),
        // the entity a remote read / message / remote write / Destroy goes to
        ExprKind::EField { base, .. } if prov(base, tainted) => true,
        ExprKind::Destroy(Some(x)) if prov(x, tainted) => true,
        ExprKind::Call { recv: Some(r), args, .. } if prov(r, tainted) => args.iter().all(|a| !prov(&a.expr, tainted) && scan_expr(p, &a.expr, fix, tainted, new_fix, grow)),
        ExprKind::Spawn { inits, .. } => inits.iter().all(|(_, x)| !prov(x, tainted) && scan_expr(p, x, fix, tainted, new_fix, grow)),
        _ => scan_expr_children(p, e, fix, tainted, new_fix, grow),
    }
}

/// the class that declares the first of these fields
fn c_of(p: &Program, fields: &[FieldId]) -> ClassId {
    p.fields[fields[0]].class
}

/// Fields that hot systems (by the profile) read through a query cursor inside a query loop:
/// once per pair of entities, the accesses that dominate O(N^2) code.
/// a field accessed less than this fraction as often as the busiest field of its record is cold
pub const COLD_RATIO: u64 = 8;

/// With field counts in the profile: the fields of a record split into the busy ones and the ones
/// accessed `COLD_RATIO` times less (None: no counts for them, or nothing to split)
fn count_split(p: &Program, pr: &crate::profile::Profile, fields: &[FieldId]) -> Option<(Vec<FieldId>, Vec<FieldId>)> {
    let counts: Vec<u64> = fields.iter().map(|&f| pr.field_count(&p.field_label(f))).collect::<Option<Vec<_>>>()?;
    let max = *counts.iter().max()?;
    if max == 0 {
        return None;
    }
    let (h, c): (Vec<(FieldId, u64)>, Vec<(FieldId, u64)>) = fields.iter().copied().zip(counts).partition(|(_, n)| n * COLD_RATIO >= max);
    if h.is_empty() || c.is_empty() {
        return None;
    }
    Some((h.into_iter().map(|(f, _)| f).collect(), c.into_iter().map(|(f, _)| f).collect()))
}

fn hot_loop_fields(p: &Program, systems: &[SystemPlan], pr: &crate::profile::Profile) -> BTreeSet<FieldId> {
    let mut out = BTreeSet::new();
    for s in systems {
        if pr.share(&s.name) < HOT_SHARE {
            continue;
        }
        for f in reachable_funcs(p, s.func) {
            let func = &p.funcs[f];
            fn walk(b: &Block, func: &Func, in_loop: bool, out: &mut BTreeSet<FieldId>) {
                for st in &b.stmts {
                    let inner = in_loop || matches!(st, Stmt::ForQuery { .. });
                    if in_loop {
                        for e in stmt_exprs(st) {
                            cursor_reads(e, func, out);
                        }
                    }
                    for sub in st.sub_blocks() {
                        walk(sub, func, inner, out);
                    }
                }
            }
            walk(&func.body, func, false, &mut out);
        }
    }
    out
}

/// Systems of one stage that run side by side, one thread each, when every one of them is too
/// small for its own parallel loop (task parallelism between systems).
#[derive(Clone, Debug)]
pub struct TaskGroup {
    pub stage: usize,
    /// consecutive systems of the stage, in stage order (their commands merge in this order)
    pub members: Vec<usize>,
    /// profiled cost of a row per member (None: no profile for it)
    pub ns_per_row: Vec<Option<f64>>,
}

/// at most this many systems per group: each takes a per-thread slot (1..=62) for its commands,
/// frame arena and entity context
pub const MAX_GROUP: usize = 62;

/// Groups of consecutive systems in a stage that may run concurrently. A member is an entity
/// system that spawns nothing (spawned ids follow system order), builds no proximity grid (grids
/// are per system) and has no reason to run serially other than printing (a stage has at most one
/// printing system: stdout is a resource two systems conflict on). Systems of a stage never touch
/// what another one writes or changes structurally, so each can run on its own thread; deferred
/// commands go to a buffer per member and are merged in stage order, as a serial run makes them.
/// With a profile, a group whose members together take less than two parallel-region openings
/// per tick is not formed (the run-time check still skips it while any member is large).
pub fn task_groups(plan: &EcsPlan, spatial: &crate::spatial::SpatialPlan) -> Vec<TaskGroup> {
    let eligible = |i: usize| {
        let s = &plan.systems[i];
        s.kind == SysKind::Entity
            && s.eff.spawns.is_empty()
            && s.why_serial.iter().all(|w| w.starts_with("prints"))
            && !spatial.loops.iter().any(|l| l.systems.contains(&s.func))
    };
    let mut out = vec![];
    for (si, stage) in plan.stages.iter().enumerate() {
        let mut runs: Vec<Vec<usize>> = vec![vec![]];
        for &i in stage {
            if eligible(i) && runs.last().map(|r| r.len() < MAX_GROUP).unwrap_or(false) {
                runs.last_mut().unwrap().push(i);
            } else if eligible(i) {
                runs.push(vec![i]);
            } else {
                runs.push(vec![]);
            }
        }
        for members in runs.into_iter().filter(|r| r.len() >= 2) {
            let ns_per_row: Vec<Option<f64>> = members.iter().map(|&i| plan.profile.as_ref().and_then(|p| p.ns_per_row(&plan.systems[i].name))).collect();
            if let Some(pr) = &plan.profile {
                // too little work per tick for any parallel region: not worth the code
                let work: f64 = members.iter().filter_map(|&i| pr.ns_per_call(&plan.systems[i].name)).sum();
                if ns_per_row.iter().all(|c| c.is_some()) && work < 2.0 * pr.fork_ns.max(200) as f64 {
                    continue;
                }
            }
            out.push(TaskGroup { stage: si, members, ns_per_row });
        }
    }
    out
}

/// fields read through a query cursor in `e` (an assignment's target is written, not read: a
/// deferred `o.Hp -= d` on a hit does not make Hp part of the scan)
fn cursor_reads(e: &Expr, func: &Func, out: &mut BTreeSet<FieldId>) {
    let cursor = |b: &Expr| matches!(b.kind, ExprKind::Local(l) if matches!(func.locals.get(l).map(|x| x.kind), Some(LocalKind::Cursor(_))));
    match &e.kind {
        ExprKind::Assign { target, value, .. } => {
            place_reads(target, func, out);
            cursor_reads(value, func, out);
        }
        ExprKind::IncDec { target, .. } => place_reads(target, func, out),
        ExprKind::EField { base, field } if cursor(base) => {
            out.insert(*field);
        }
        _ => {
            for c in crate::sema::children(e) {
                cursor_reads(c, func, out);
            }
        }
    }
}

fn place_reads(e: &Expr, func: &Func, out: &mut BTreeSet<FieldId>) {
    match &e.kind {
        ExprKind::EField { base, .. } | ExprKind::Field { base, .. } => place_reads(base, func, out),
        ExprKind::ListIndex { list, index, .. } => {
            place_reads(list, func, out);
            cursor_reads(index, func, out);
        }
        ExprKind::Local(_) => {}
        _ => cursor_reads(e, func, out),
    }
}

/// Can a system be offloaded to a GPU as a compute kernel? (`sijoc build --gpu` does it for these,
/// docs/07 §7.4.) Yes when it is a pure self-write entity system: it reads and writes only
/// the plain-data fields of its own row (numbers, bools, enums, chars and structs of those), and
/// has no other effect: no commands, spawns, destroys, messages, queries or reads of other entities,
/// no random numbers, prints, allocations, coroutine frames or extern calls. Such a kernel needs
/// only the row's columns on the device and gives the same bits there if the device compiler keeps
/// IEEE single precision without contraction (the math functions are sijo's own, compiled for it).
/// Ok(bytes per row moved each way), or the reason it is not one.
pub fn gpu_candidate(p: &Program, plan: &EcsPlan, i: usize) -> Result<(usize, usize), String> {
    let s = &plan.systems[i];
    let e = &s.eff;
    if s.kind != SysKind::Entity {
        return Err("not an entity system".into());
    }
    let why = if e.prints {
        "prints"
    } else if e.random {
        "draws random numbers"
    } else if !e.spawns.is_empty() || !e.destroys.is_empty() || e.destroys_self {
        "spawns or destroys entities"
    } else if e.restructures_self || !e.restructures.is_empty() {
        "adds or removes components"
    } else if !e.remote_writes.is_empty() || !e.remote_calls.is_empty() || !e.msg_writes.is_empty() || !e.rpcs.is_empty() {
        "writes to or calls other entities"
    } else if !e.queries.is_empty() || !e.remote_reads.is_empty() {
        "reads other entities"
    } else if e.allocs {
        "allocates"
    } else if p.funcs[s.func].co.is_some() {
        "is a coroutine"
    } else if matches!(p.funcs[s.func].kind, FuncKind::EntityMethod(k) if p.classes[k].is_component) && s.archetypes.iter().any(|a| p.open.contains_key(a)) {
        "runs on an open table (rows without the component are skipped)"
    } else if reachable_funcs(p, s.func).iter().any(|&f| p.funcs[f].extern_name.is_some()) {
        "calls extern code"
    } else {
        ""
    };
    if !why.is_empty() {
        return Err(why.into());
    }
    fn size(p: &Program, t: &Ty, depth: usize) -> Option<usize> {
        match t {
            Ty::Int | Ty::Float | Ty::Char | Ty::Enum(_) => Some(4),
            Ty::Bool => Some(1),
            Ty::Struct(sid) if depth < 16 => p.structs[*sid].fields.iter().map(|f| size(p, &f.ty, depth + 1)).sum(),
            _ => None,
        }
    }
    let bytes = |fs: &BTreeSet<FieldId>| -> Result<usize, String> {
        fs.iter().filter(|f| !plan.is_dead(**f)).map(|&f| size(p, &p.fields[f].ty, 0).ok_or_else(|| format!("uses '{}', which is not plain data", p.field_label(f)))).sum()
    };
    let reads: BTreeSet<FieldId> = e.self_reads.union(&e.self_writes).copied().collect();
    Ok((bytes(&reads)?, bytes(&e.self_writes)?))
}
