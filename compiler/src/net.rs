//! Network planning: which fields replicate, and how many bits each needs.
//!
//! Bit widths come from value ranges: declared range types (`int in 0..100`) (enforced by clamping at every
//! write) or ranges inferred from all writes to the field (constants / Math.Clamp).

use crate::ecs::EcsPlan;
use crate::hir::*;
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, PartialEq)]
pub enum Quant {
    Bool,
    /// value = lo + q
    Int { lo: i64, bits: u32 },
    IntRaw,
    /// value = min(lo + q * step, hi): when the step does not divide the range, the top step
    /// (q = maxq) would land past `hi`, so the dequantized value is saturated like every write
    Float { lo: f64, hi: f64, step: f64, bits: u32, maxq: u64 },
    FloatRaw,
    /// an entity reference: the server slot and the low 8 bits of its generation (0: null, dead
    /// or not replicated); the client maps it to its own handle after applying the snapshot
    Handle,
    /// a string: an id the server interns (exact: equal ids = equal text); a snapshot that sends
    /// the leaf sends the id and the UTF-8 bytes
    Str,
    /// a list, array or dictionary: its elements as canonical bytes (entity references as server
    /// slots), interned and sent like a string
    List,
}

impl Quant {
    pub fn bits(&self) -> u32 {
        match self {
            Quant::Bool => 1,
            Quant::Int { bits, .. } | Quant::Float { bits, .. } => *bits,
            Quant::IntRaw | Quant::FloatRaw | Quant::Handle | Quant::Str | Quant::List => 32,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Leaf {
    /// member path inside a struct-typed field (empty for scalars)
    pub path: Vec<usize>,
    pub path_names: Vec<String>,
    pub ty: Ty,
    pub quant: Quant,
    pub source: String,
}

#[derive(Clone, Debug)]
pub struct FieldNet {
    pub field: FieldId,
    pub leaves: Vec<Leaf>,
    pub bits: u32,
}

#[derive(Debug, Default)]
pub struct NetPlan {
    pub fields: BTreeMap<FieldId, FieldNet>,
    /// declared ranges, enforced at every write
    pub clamp: HashMap<FieldId, (f64, f64)>,
    pub inferred: HashMap<FieldId, (i64, i64)>,
    /// archetype -> (spatial field, index of X member in its struct)
    pub spatial: HashMap<ClassId, (FieldId, usize)>,
    /// enums whose values may lie outside their declared members (cast from non-constant ints)
    pub wide_enums: std::collections::HashSet<EnumId>,
}

fn bits_for(n: u64) -> u32 {
    // bits needed to represent 0..=n
    let mut b = 1;
    while b < 63 && (1u64 << b) <= n {
        b += 1;
    }
    b
}

pub fn plan(p: &Program, ecs: &EcsPlan, d: &mut crate::diag::Diags) -> NetPlan {
    let mut np = NetPlan::default();
    for (fid, f) in p.fields.iter().enumerate() {
        if let Some(r) = f.attrs.range {
            np.clamp.insert(fid, r);
            // the default value is a write like any other: it is clamped too, but say so
            if let Some(init) = &f.init {
                if let Ok(v) = crate::consteval::eval_expr(p, init) {
                    let c = crate::codegen::clamp_const(v.clone(), r.0, r.1);
                    if c != v {
                        d.warn_note(
                            "W0202",
                            init.span,
                            format!("the default value of '{}' is outside its range {}..{}", f.name, r.0, r.1),
                            format!("it starts at {} (clamped like every other write)", crate::opt::show_val(p, &c)),
                        );
                    }
                }
            } else if !f.name.starts_with("__") {
                // no initializer: the zero value is the default, saturated like a written one
                // (hidden coroutine fields hold ranged locals, which are always assigned first)
                if let Some(v) = crate::consteval::implicit_default(p, &f.ty) {
                    let c = crate::codegen::clamp_const(v.clone(), r.0, r.1);
                    if c != v {
                        d.warn_note(
                            "W0202",
                            f.span,
                            format!("'{}' has no initializer, and its zero default {} is outside its range {}..{}", f.name, crate::opt::show_val(p, &v), r.0, r.1),
                            format!("it starts at {} (clamped like every other write); give it an initializer to choose the start value", crate::opt::show_val(p, &c)),
                        );
                    }
                }
            }
        }
    }
    np.inferred = infer_int_ranges(p);
    for f in &p.funcs {
        crate::sema::visit_block(&f.body, &mut |e| {
            if let (ExprKind::Cast(inner), Ty::Enum(en)) = (&e.kind, &e.ty) {
                let in_range = matches!(inner.kind, ExprKind::Int(v) if p.enums[*en].variants.iter().any(|(_, x)| *x == v));
                if !in_range {
                    np.wide_enums.insert(*en);
                }
            }
        });
    }
    for &fid in &ecs.replicated {
        let f = &p.fields[fid];

        let mut leaves = vec![];
        collect_leaves(p, &f.ty, vec![], vec![], f, fid, &np, &mut leaves);
        let bits = leaves.iter().map(|l| l.quant.bits()).sum();
        np.fields.insert(fid, FieldNet { field: fid, leaves, bits });
    }
    // spatial field per archetype
    for &a in &ecs.archetypes {
        let fields = p.all_fields(a);
        let pick = fields
            .iter()
            .copied()
            .find(|&f| p.fields[f].attrs.spatial)
            .or_else(|| fields.iter().copied().find(|&f| matches!(p.fields[f].name.as_str(), "Pos" | "Position")));
        if let Some(f) = pick {
            if ecs.is_dead(f) {
                continue;
            }
            if let Ty::Struct(s) = p.fields[f].ty {
                if let Some(ix) = p.structs[s].fields.iter().position(|m| m.name == "X" && m.ty == Ty::Float) {
                    np.spatial.insert(a, (f, ix));
                }
            }
        }
    }
    np
}

#[allow(clippy::too_many_arguments)]
fn collect_leaves(p: &Program, ty: &Ty, path: Vec<usize>, names: Vec<String>, f: &EField, fid: FieldId, np: &NetPlan, out: &mut Vec<Leaf>) {
    match ty {
        Ty::Struct(s) => {
            for (i, m) in p.structs[*s].fields.iter().enumerate() {
                let mut pp = path.clone();
                pp.push(i);
                let mut nn = names.clone();
                nn.push(m.name.clone());
                collect_leaves(p, &m.ty, pp, nn, f, fid, np, out);
            }
        }
        Ty::Bool => out.push(Leaf { path, path_names: names, ty: ty.clone(), quant: Quant::Bool, source: "bool".into() }),
        Ty::Str => out.push(Leaf { path, path_names: names, ty: ty.clone(), quant: Quant::Str, source: "string: interned id, the text is sent with the id when the value changes".into() }),
        Ty::List(_) | Ty::Array(_) | Ty::Dict(..) | Ty::MdArray(..) => out.push(Leaf {
            path,
            path_names: names,
            ty: ty.clone(),
            quant: Quant::List,
            source: if p.has_handle(ty) {
                "elements as bytes, entity references as server slots that the client maps to its handles; re-encoded every tick".into()
            } else {
                "elements as bytes; re-encoded every tick".into()
            },
        }),
        Ty::Handle(_) | Ty::NHandle(_) => {
            out.push(Leaf { path, path_names: names, ty: ty.clone(), quant: Quant::Handle, source: "entity reference: server slot + generation, mapped by the client".into() })
        }
        Ty::Enum(e) if np.wide_enums.contains(e) => {
            out.push(Leaf { path, path_names: names, ty: ty.clone(), quant: Quant::IntRaw, source: "enum built from arbitrary ints by a cast: full 32 bits".into() });
        }
        Ty::Enum(e) => {
            let vs = &p.enums[*e].variants;
            let lo = vs.iter().map(|v| v.1).min().unwrap_or(0);
            let hi = vs.iter().map(|v| v.1).max().unwrap_or(0);
            out.push(Leaf {
                path,
                path_names: names,
                ty: ty.clone(),
                quant: Quant::Int { lo, bits: bits_for((hi - lo) as u64) },
                source: format!("enum {} variants", vs.len()),
            });
        }
        Ty::Char => {
            let (q, src) = match f.attrs.range {
                Some((lo, hi)) => {
                    let (lo, hi) = (lo.ceil().max(0.0) as i64, hi.floor().min(0x10FFFF as f64) as i64);
                    (Quant::Int { lo, bits: bits_for((hi - lo).max(0) as u64) }, format!("declared range {}..{}", lo, hi))
                }
                None => (Quant::Int { lo: 0, bits: bits_for(0x10FFFF) }, "a Unicode code point: 21 bits".into()),
            };
            out.push(Leaf { path, path_names: names, ty: ty.clone(), quant: q, source: src });
        }
        Ty::Int => {
            let (q, src) = if let Some((lo, hi)) = f.attrs.range {
                let (lo, hi) = (lo.ceil() as i64, hi.floor() as i64);
                (Quant::Int { lo, bits: bits_for((hi - lo).max(0) as u64) }, format!("declared range {}..{}", lo, hi))
            } else if let (true, Some((lo, hi))) = (path.is_empty(), np.inferred.get(&fid)) {
                (Quant::Int { lo: *lo, bits: bits_for((hi - lo) as u64) }, format!("inferred range [{}, {}] from all writes", lo, hi))
            } else {
                (Quant::IntRaw, "no range known: full 32 bits".into())
            };
            out.push(Leaf { path, path_names: names, ty: ty.clone(), quant: q, source: src });
        }
        Ty::Float => {
            let (q, src) = if let Some((lo, hi)) = f.attrs.range {
                let step = f.attrs.precision.unwrap_or(0.01);
                let maxq = ((hi - lo) / step).round() as u64;
                if bits_for(maxq) >= 32 {
                    (Quant::FloatRaw, format!("range {}..{} at precision {} needs >= 32 bits: sent raw", lo, hi, step))
                } else {
                    (
                        Quant::Float { lo, hi, step, bits: bits_for(maxq), maxq },
                        format!("declared range {}..{}, precision {}", lo, hi, step),
                    )
                }
            } else {
                (Quant::FloatRaw, "no range known: full 32 bits".into())
            };
            out.push(Leaf { path, path_names: names, ty: ty.clone(), quant: q, source: src });
        }
        _ => {}
    }
}

/// Int fields whose every write is a constant or Math.Clamp with constant bounds.
fn infer_int_ranges(p: &Program) -> HashMap<FieldId, (i64, i64)> {
    let mut ranges: HashMap<FieldId, Option<(i64, i64)>> = HashMap::new();
    let widen = |m: &mut HashMap<FieldId, Option<(i64, i64)>>, f: FieldId, r: Option<(i64, i64)>| {
        let e = m.entry(f).or_insert(r);
        *e = match (*e, r) {
            (Some((a, b)), Some((c, dd))) => Some((a.min(c), b.max(dd))),
            _ => None,
        };
    };
    for (fid, f) in p.fields.iter().enumerate() {
        if f.ty != Ty::Int || f.attrs.range.is_some() {
            continue;
        }
        let init = match &f.init {
            Some(Expr { kind: ExprKind::Int(n), .. }) => Some((*n, *n)),
            None => Some((0, 0)),
            _ => None,
        };
        widen(&mut ranges, fid, init);
    }
    // the presence bits of an open table: one per component it can have (set by Add and Remove)
    for (&c, &f) in &p.open {
        let n = p.classes[c].comps.len();
        let r = if n <= 30 { Some((0i64, (1i64 << n) - 1)) } else { None };
        widen(&mut ranges, f, r);
    }
    let value_range = |e: &Expr| -> Option<(i64, i64)> {
        match &e.kind {
            ExprKind::Int(n) => Some((*n, *n)),
            ExprKind::Builtin { f: Builtin::MathClamp, args, .. } => match (&args[1].kind, &args[2].kind) {
                (ExprKind::Int(lo), ExprKind::Int(hi)) if lo <= hi => Some((*lo, *hi)),
                _ => None,
            },
            _ => None,
        }
    };
    for f in &p.funcs {
        crate::sema::visit_block(&f.body, &mut |e| match &e.kind {
            ExprKind::Assign { op, target, value } => {
                if let ExprKind::EField { field, .. } = target.kind {
                    if p.fields[field].ty == Ty::Int {
                        let r = if op.is_none() { value_range(value) } else { None };
                        widen(&mut ranges, field, r);
                    }
                }
            }
            ExprKind::IncDec { target, .. } => {
                if let ExprKind::EField { field, .. } = target.kind {
                    widen(&mut ranges, field, None);
                }
            }
            ExprKind::Spawn { inits, .. } | ExprKind::CompAdd { inits, .. } => {
                for (fld, x) in inits {
                    if p.fields[*fld].ty == Ty::Int {
                        widen(&mut ranges, *fld, value_range(x));
                    }
                }
            }
            ExprKind::Call { args, .. } => {
                for a in args {
                    if matches!(a.mode, crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out) {
                        if let Some((fld, _)) = crate::effects::target_field(&a.expr) {
                            widen(&mut ranges, fld, None);
                        }
                    }
                }
            }
            _ => {}
        });
    }
    ranges.into_iter().filter_map(|(f, r)| r.map(|r| (f, r))).collect()
}
