//! Dynamic components (docs/04 §4.4.1): an archetype is a class together with the set of
//! components its entities have. Every `Add`/`Remove` in the program says which classes it can
//! reach (from the static type of the entity it names, or the class or component whose method
//! runs it on `this`), so the combinations each class can reach are enumerated here, at compile
//! time, and each becomes an archetype of its own (a subclass of the class with the components'
//! fields: `Ship+Burning`). Everything after this pass (effects, scheduling, tables, replication,
//! state files) sees ordinary archetypes; an entity that gains or loses a component moves from
//! one table of its class to another at the end of the system (`codegen::comp_changes`).

use crate::diag::{Diags, Span};
use crate::hir::*;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

/// combinations (tables) one class may have, its own included; a class that can reach more keeps
/// one open table instead (its rows carry a bit per component)
pub const MAX_COMBOS: usize = 16;
/// components one open table can carry (the bits of its presence field)
pub const MAX_OPEN_COMPS: usize = 32;
/// archetypes in a program: membership tests are 64-bit masks
pub const MAX_ARCHETYPES: usize = 64;

#[derive(Clone, Debug)]
enum Target {
    /// entities of these concrete classes
    Classes(BTreeSet<ClassId>),
    /// entities that have this component (whatever their class)
    Having(ClassId),
}

/// one `Add`/`Remove` site: which entities it can reach, what it does
#[derive(Clone, Debug)]
struct Site {
    target: Target,
    add: bool,
    comp: ClassId,
    span: Span,
}

/// `Spawn(new T { }, new C { })`: the classes and the component sets entities start with
fn spawn_sets(p: &Program) -> Vec<(ClassId, BTreeSet<ClassId>)> {
    let mut out = vec![];
    for f in &p.funcs {
        crate::sema::visit_block(&f.body, &mut |e| {
            if let ExprKind::Spawn { class, comps, .. } = &e.kind {
                if !comps.is_empty() {
                    out.push((*class, comps.iter().copied().collect()));
                }
            }
        });
    }
    out
}

fn sites(p: &Program, concrete: &BTreeSet<ClassId>) -> Vec<Site> {
    let mut out = vec![];
    for f in &p.funcs {
        let owner = match f.kind {
            FuncKind::EntityMethod(c) => Some(c),
            _ => None,
        };
        crate::sema::visit_block(&f.body, &mut |e| {
            let (target, comp, add) = match &e.kind {
                ExprKind::CompAdd { target, comp, .. } => (target, *comp, true),
                ExprKind::CompRemove { target, comp } => (target, *comp, false),
                _ => return,
            };
            // `this` (or `Owner`) is the entity the method runs for: its class, or any entity with
            // the component whose method it is
            let class = match (&target.kind, owner) {
                (ExprKind::SelfRef, Some(c)) => Some(c),
                _ => target.ty.class_of(),
            };
            let Some(c) = class else { return };
            let t = if p.classes[c].is_component {
                Target::Having(c)
            } else {
                Target::Classes(p.archetypes_of(c).into_iter().filter(|a| concrete.contains(a)).collect())
            };
            out.push(Site { target: t, add, comp, span: e.span });
        });
    }
    out
}

/// Adds the combination archetypes. Programs without components are left exactly as they are.
/// With `force_open` (`sijoc build --open-tables`) every class that can gain components keeps one
/// open table, however few combinations it has (the tests compare both layouts).
pub fn expand(p: &mut Program, d: &mut Diags, force_open: bool) {
    let comps: Vec<ClassId> = (0..p.classes.len()).filter(|&k| p.classes[k].is_component).collect();
    if comps.is_empty() {
        return;
    }
    let concrete: BTreeSet<ClassId> = p.entity_classes().into_iter().filter(|&c| !p.classes[c].is_abstract).collect();
    let sites = sites(p, &concrete);
    let spawns = spawn_sets(p);
    // which classes may have each component: the classes an Add reaches, through components that
    // themselves come and go (an Add in a method of A reaches whatever may have A), and the
    // classes spawned with it
    let mut may: BTreeMap<ClassId, BTreeSet<ClassId>> = comps.iter().map(|&k| (k, BTreeSet::new())).collect();
    for (c, set) in &spawns {
        for k in set {
            may.get_mut(k).unwrap().insert(*c);
        }
    }
    loop {
        let mut changed = false;
        for s in sites.iter().filter(|s| s.add) {
            let reach: BTreeSet<ClassId> = match &s.target {
                Target::Classes(cs) => cs.clone(),
                Target::Having(g) => may[g].clone(),
            };
            let set = may.get_mut(&s.comp).unwrap();
            let n = set.len();
            set.extend(reach);
            changed |= set.len() != n;
        }
        if !changed {
            break;
        }
    }
    // per class: the component sets reachable from none, applying every site that can reach an
    // entity of the class with that set (an Add or Remove in a method of A needs A)
    let mut combos: Vec<(ClassId, Vec<Vec<ClassId>>)> = vec![];
    let mut open: Vec<(ClassId, Vec<ClassId>)> = vec![];
    for &c in &concrete {
        let applies = |s: &Site, set: &BTreeSet<ClassId>| match &s.target {
            Target::Classes(cs) => cs.contains(&c),
            Target::Having(g) => set.contains(g) && may[g].contains(&c),
        };
        let mut seen: Vec<BTreeSet<ClassId>> = vec![BTreeSet::new()];
        let mut queue: VecDeque<BTreeSet<ClassId>> = VecDeque::from([BTreeSet::new()]);
        // the sets entities of the class are spawned with are starting points too
        for (sc, set) in &spawns {
            if *sc == c && !seen.contains(set) {
                seen.push(set.clone());
                queue.push_back(set.clone());
            }
        }
        let mut too_many = false;
        while let Some(set) = queue.pop_front() {
            for s in &sites {
                if !applies(s, &set) {
                    continue;
                }
                let mut next = set.clone();
                if s.add {
                    next.insert(s.comp);
                } else {
                    next.remove(&s.comp);
                }
                if !seen.contains(&next) {
                    seen.push(next.clone());
                    queue.push_back(next);
                }
            }
            if seen.len() > MAX_COMBOS {
                too_many = true;
                break;
            }
        }
        if too_many || (force_open && seen.len() > 1) {
            // one open table: the columns of every component the class may have, and a bit each
            let ks: Vec<ClassId> = may.iter().filter(|(_, cs)| cs.contains(&c)).map(|(k, _)| *k).collect();
            if ks.len() > MAX_OPEN_COMPS {
                let names: Vec<String> = ks.iter().map(|&k| p.classes[k].name.clone()).collect();
                d.error_note(
                    "E0266",
                    p.classes[c].span,
                    format!("entities of '{}' can have {} kinds of components ({}); at most {}", p.classes[c].name, ks.len(), names.join(", "), MAX_OPEN_COMPS),
                    format!("a class with more than {} combinations keeps one table with a bit per component: give '{}' fewer kinds of components (merge components that come and go together, or keep a flag field instead)", MAX_COMBOS, p.classes[c].name),
                );
                continue;
            }
            open.push((c, ks));
            continue;
        }
        // fewest components first, then in declaration order
        let mut sets: Vec<Vec<ClassId>> = seen.into_iter().filter(|s| !s.is_empty()).map(|s| s.into_iter().collect()).collect();
        sets.sort_by(|a, b| a.len().cmp(&b.len()).then(a.cmp(b)));
        if !sets.is_empty() {
            combos.push((c, sets));
        }
    }
    // open tables: the class itself has the components' columns, and the presence bits (a hidden
    // field, replicated: clients test Has<C>() too; one per hierarchy, where the first open class
    // declares it)
    for (c, ks) in open {
        let flag = match p.find_field(c, "__comps") {
            Some(f) => f,
            None => {
                let span = p.classes[c].span;
                p.fields.push(EField { name: "__comps".into(), class: c, is_mut: true, ty: Ty::Int, init: None, attrs: FieldAttrs { replicated: Some(true), ..Default::default() }, span });
                let f = p.fields.len() - 1;
                p.classes[c].own_fields.push(f);
                f
            }
        };
        p.written_fields.insert(flag);
        p.classes[c].comps = ks;
        p.open.insert(c, flag);
    }
    // the combination archetypes: subclasses of their class with the components' fields
    let mut made: HashMap<ClassId, Vec<ClassId>> = HashMap::new();
    for (c, sets) in combos {
        for set in sets {
            let name = std::iter::once(p.classes[c].name.clone()).chain(set.iter().map(|&k| p.classes[k].name.clone())).collect::<Vec<_>>().join("+");
            p.classes.push(ClassDef {
                name,
                span: p.classes[c].span,
                base: Some(c),
                is_entity: true,
                is_static: false,
                is_abstract: false,
                is_interface: false,
                ifaces: vec![],
                is_component: false,
                combo_of: Some(c),
                comps: set,
                own_fields: vec![],
                methods: HashMap::new(), overloads: vec![],
                order: 0,
            });
            made.entry(c).or_default().push(p.classes.len() - 1);
        }
    }
    // declaration order, each class followed by its combinations (systems visit archetypes in
    // this order, so the order of the rows a system visits stays deterministic)
    let mut old: Vec<ClassId> = (0..p.classes.len()).filter(|&x| p.classes[x].combo_of.is_none()).collect();
    old.sort_by_key(|&x| p.classes[x].order);
    let mut n = 0;
    for x in old {
        p.classes[x].order = n;
        n += 1;
        for &m in made.get(&x).map(|v| v.as_slice()).unwrap_or(&[]) {
            p.classes[m].order = n;
            n += 1;
        }
    }
    // a spawn with components makes its entity in the table of that combination
    let combo_of: Vec<(ClassId, Vec<ClassId>, ClassId)> = made.iter().flat_map(|(&c, ms)| ms.iter().map(move |&m| (c, m))).map(|(c, m)| (c, p.classes[m].comps.clone(), m)).collect();
    for f in 0..p.funcs.len() {
        let mut body = std::mem::take(&mut p.funcs[f].body);
        let open = p.open.clone();
        let have: HashMap<ClassId, Vec<ClassId>> = open.keys().map(|&c| (c, p.classes[c].comps.clone())).collect();
        crate::opt::visit_block_mut(&mut body, &mut |e| {
            let sp = e.span;
            if let ExprKind::Spawn { class, comps, inits } = &mut e.kind {
                if !comps.is_empty() {
                    if let Some(&(_, _, m)) = combo_of.iter().find(|(c, set, _)| c == class && set == comps) {
                        *class = m;
                        comps.clear();
                    } else if let Some(&flag) = open.get(class) {
                        // an open table: the entity starts with these bits
                        let bits: i64 = comps.iter().filter_map(|k| have[class].iter().position(|x| x == k)).fold(0i64, |m, i| m | (1i64 << i));
                        let bits = if bits >= 1i64 << 31 { bits - (1i64 << 32) } else { bits };
                        inits.push((flag, Expr::new(ExprKind::Int(bits), Ty::Int, sp)));
                        comps.clear();
                    }
                }
            }
        });
        p.funcs[f].body = body;
    }
    let total = p.entity_classes().into_iter().filter(|&c| !p.classes[c].is_abstract).count();
    if total > MAX_ARCHETYPES {
        let sp = sites.first().map(|s| s.span).unwrap_or_default();
        d.error_note(
            "E0266",
            sp,
            format!("the program has {} archetypes with its component combinations; at most {}", total, MAX_ARCHETYPES),
            "each class has one table per combination of components it can reach: give fewer components to fewer classes",
        );
    }
}

/// `Ship+Burning` -> the combination's class and components, for messages
pub fn describe(p: &Program, a: ClassId) -> String {
    let c = &p.classes[a];
    match c.combo_of {
        Some(base) => format!("{} with {}", p.classes[base].name, c.comps.iter().map(|&k| p.classes[k].name.clone()).collect::<Vec<_>>().join(", ")),
        None => c.name.clone(),
    }
}
