//! Compile-time evaluation (CTFE) of HIR expressions.
//!
//! Semantics mirror the generated C exactly: 32-bit wrapping integers, IEEE binary32 floats,
//! saturating float->int casts, masked shift counts. Anything that touches world state,
//! randomness, time, I/O or transcendental libm functions is "not constant".

use crate::ast::{BinOp, UnOp};
use crate::hir::*;

pub enum EvalErr {
    NotConst(String),
    Pending,
}

const FUEL: u64 = 1_000_000;

struct Interp<'a> {
    p: &'a Program,
    fuel: u64,
    depth: usize,
}

enum Flow {
    Normal,
    Return(Option<ConstVal>),
    Break,
    Continue,
}

struct Frame {
    locals: Vec<Option<ConstVal>>,
    this: Option<ConstVal>,
    /// the range types of the function's variables (every write saturates)
    ranges: Vec<Option<(f64, f64)>>,
}

/// a value written into a variable: saturated by its range type, if any
fn ranged(loc: &Local, v: ConstVal) -> ConstVal {
    match loc.range {
        Some((lo, hi)) => crate::codegen::clamp_const(v, lo, hi),
        None => v,
    }
}

pub fn eval_expr(p: &Program, e: &Expr) -> Result<ConstVal, String> {
    let mut it = Interp { p, fuel: FUEL, depth: 0 };
    let mut fr = Frame { locals: vec![], this: None, ranges: vec![] };
    it.expr(e, &mut fr).map_err(|e| match e {
        EvalErr::NotConst(m) => m,
        EvalErr::Pending => "depends on a constant that is not evaluated yet".into(),
    })
}

pub fn eval_expr_raw(p: &Program, e: &Expr) -> Result<ConstVal, EvalErr> {
    let mut it = Interp { p, fuel: FUEL, depth: 0 };
    let mut fr = Frame { locals: vec![], this: None, ranges: vec![] };
    it.expr(e, &mut fr)
}

/// Try to evaluate a call with constant arguments (used by the optimizer).
pub fn eval_call(p: &Program, f: FuncId, recv: Option<ConstVal>, args: Vec<ConstVal>) -> Option<ConstVal> {
    let mut it = Interp { p, fuel: FUEL / 10, depth: 0 };
    it.call(f, recv, args).ok().flatten()
}

fn nc<T>(m: &str) -> Result<T, EvalErr> {
    Err(EvalErr::NotConst(m.to_string()))
}

pub fn f2i(f: f32) -> i32 {
    // saturating, NaN -> 0 (matches sijo_f2i in the runtime)
    f as i32
}

pub fn zero_value(p: &Program, t: &Ty) -> Option<ConstVal> {
    Some(match t {
        Ty::Int | Ty::Char => ConstVal::Int(0),
        Ty::Float => ConstVal::Float(0.0),
        Ty::Bool => ConstVal::Bool(false),
        Ty::Enum(e) => ConstVal::Enum(*e, 0),
        Ty::NHandle(_) | Ty::Null => ConstVal::Null,
        Ty::Str => ConstVal::Str(String::new()),
        Ty::Delegate(d) => ConstVal::Fn(*d, 0),
        Ty::Struct(s) => ConstVal::Struct(*s, p.structs[*s].fields.iter().map(|f| zero_value(p, &f.ty)).collect::<Option<Vec<_>>>()?),
        _ => return None,
    })
}

/// The value a field of this type starts with when it has no initializer (structs: their member
/// initializers), before a range type saturates it.
pub fn implicit_default(p: &Program, t: &Ty) -> Option<ConstVal> {
    match t {
        Ty::Struct(s) => struct_default(p, *s).ok(),
        _ => zero_value(p, t),
    }
}

/// Default value of a struct: zero, then field initializers.
pub fn struct_default(p: &Program, s: StructId) -> Result<ConstVal, String> {
    struct_default_raw(p, s).map_err(|e| match e {
        EvalErr::NotConst(m) => m,
        EvalErr::Pending => "depends on a constant that is not evaluated yet".into(),
    })
}

pub fn struct_default_raw(p: &Program, s: StructId) -> Result<ConstVal, EvalErr> {
    let mut vals = vec![];
    for f in &p.structs[s].fields {
        match &f.init {
            Some(e) => vals.push(eval_expr_raw(p, e)?),
            None => vals.push(zero_value(p, &f.ty).ok_or(EvalErr::NotConst("field type has no constant default".into()))?),
        }
    }
    Ok(ConstVal::Struct(s, vals))
}

impl<'a> Interp<'a> {
    fn tick(&mut self) -> Result<(), EvalErr> {
        if self.fuel == 0 {
            return nc("evaluation exceeded the compile-time fuel limit");
        }
        self.fuel -= 1;
        Ok(())
    }

    fn call(&mut self, f: FuncId, recv: Option<ConstVal>, args: Vec<ConstVal>) -> Result<Option<ConstVal>, EvalErr> {
        if self.depth > 64 {
            return nc("recursion too deep for compile-time evaluation");
        }
        let func = &self.p.funcs[f];
        match func.kind {
            FuncKind::EntityMethod(_) => return nc("entity methods are not compile-time evaluable"),
            FuncKind::Static(c) if self.p.classes[c].is_entity && func.ctx == Ctx::Client => {
                return nc("client functions are not compile-time evaluable")
            }
            _ => {}
        }
        let mut fr = Frame { locals: vec![None; func.locals.len()], this: recv, ranges: func.locals.iter().map(|l| l.range).collect() };
        for (i, a) in args.into_iter().enumerate() {
            fr.locals[func.params[i]] = Some(ranged(&func.locals[func.params[i]], a));
        }
        if let FuncKind::StructCtor(s) = func.kind {
            if fr.this.is_none() {
                fr.this = Some(struct_default_raw(self.p, s)?);
            }
        }
        self.depth += 1;
        let flow = self.block(&func.body, &mut fr)?;
        self.depth -= 1;
        if let FuncKind::StructCtor(_) = func.kind {
            return Ok(fr.this);
        }
        match flow {
            Flow::Return(v) => Ok(v),
            _ => Ok(None),
        }
    }

    fn block(&mut self, b: &Block, fr: &mut Frame) -> Result<Flow, EvalErr> {
        for s in &b.stmts {
            match self.stmt(s, fr)? {
                Flow::Normal => {}
                other => return Ok(other),
            }
        }
        Ok(Flow::Normal)
    }

    fn stmt(&mut self, s: &Stmt, fr: &mut Frame) -> Result<Flow, EvalErr> {
        self.tick()?;
        match s {
            Stmt::Let { local, init } => {
                let v = match init {
                    Some(e) => self.expr(e, fr)?,
                    None => return nc("uninitialized local"),
                };
                fr.locals[*local] = Some(v);
                Ok(Flow::Normal)
            }
            Stmt::Expr(e) => {
                self.expr_any(e, fr)?;
                Ok(Flow::Normal)
            }
            Stmt::If { cond, then, els } => {
                if self.bool(cond, fr)? {
                    self.block(then, fr)
                } else if let Some(e) = els {
                    self.block(e, fr)
                } else {
                    Ok(Flow::Normal)
                }
            }
            Stmt::Loop { cond, step, body, do_while } => {
                let mut first = true;
                loop {
                    self.tick()?;
                    let go = if *do_while && first {
                        true
                    } else {
                        match cond {
                            Some(c) => self.bool(c, fr)?,
                            None => true,
                        }
                    };
                    first = false;
                    if !go {
                        break;
                    }
                    match self.block(body, fr)? {
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        _ => {}
                    }
                    for e in step {
                        self.expr_any(e, fr)?;
                    }
                }
                Ok(Flow::Normal)
            }
            Stmt::Return(v, _) => {
                let r = match v {
                    Some(e) => Some(self.expr(e, fr)?),
                    None => None,
                };
                Ok(Flow::Return(r))
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
            Stmt::Block(b) => self.block(b, fr),
            Stmt::Switch { tmp, scrut, sections, default } => {
                if let Some(t) = tmp {
                    let v = self.expr(scrut, fr)?;
                    if fr.locals.len() <= *t {
                        fr.locals.resize(*t + 1, None);
                    }
                    fr.locals[*t] = Some(v);
                }
                let mut chosen = default.as_ref();
                for (c, b) in sections {
                    if self.bool(c, fr)? {
                        chosen = Some(b);
                        break;
                    }
                }
                match chosen {
                    Some(b) => match self.block(b, fr)? {
                        Flow::Break => Ok(Flow::Normal),
                        f => Ok(f),
                    },
                    None => Ok(Flow::Normal),
                }
            }
            _ => nc("statement is not compile-time evaluable"),
        }
    }

    fn bool(&mut self, e: &Expr, fr: &mut Frame) -> Result<bool, EvalErr> {
        match self.expr(e, fr)? {
            ConstVal::Bool(b) => Ok(b),
            _ => nc("expected bool"),
        }
    }

    /// Evaluate for side effects (assignments) - allows void.
    fn expr_any(&mut self, e: &Expr, fr: &mut Frame) -> Result<Option<ConstVal>, EvalErr> {
        match &e.kind {
            ExprKind::Assign { op, target, value } => {
                let v = self.expr(value, fr)?;
                let nv = match op {
                    None => v,
                    Some(bop) => {
                        let cur = self.expr(target, fr)?;
                        let (cur, v) = promote(cur, v);
                        binop(*bop, cur, v)?
                    }
                };
                let nv = coerce_val(nv, &target.ty);
                self.store(target, nv, fr)?;
                // the value of `x = v` / `x op= v` is what x holds now (saturated by its range type)
                Ok(Some(self.expr(target, fr)?))
            }
            ExprKind::IncDec { target, inc, prefix } => {
                let cur = self.expr(target, fr)?;
                let one = match cur {
                    ConstVal::Float(_) => ConstVal::Float(1.0),
                    _ => ConstVal::Int(1),
                };
                let nv = binop(if *inc { BinOp::Add } else { BinOp::Sub }, cur.clone(), one)?;
                self.store(target, nv, fr)?;
                // prefix: the stored (saturated) value; postfix: the value before the write
                Ok(Some(if *prefix { self.expr(target, fr)? } else { cur }))
            }
            ExprKind::Call { func, recv, args, .. } => {
                let (rv, place) = match recv {
                    Some(r) => (Some(self.expr(r, fr)?), Some(r)),
                    None => (None, None),
                };
                let mut av = vec![];
                for a in args {
                    if a.mode == crate::ast::ParamMode::Ref || a.mode == crate::ast::ParamMode::Out {
                        return nc("ref/out arguments are not compile-time evaluable");
                    }
                    av.push(self.expr(&a.expr, fr)?);
                }
                let writes = self.p.funcs[*func].writes_self;
                if writes {
                    if self.depth > 64 {
                        return nc("recursion too deep for compile-time evaluation");
                    }
                    // run method with `this` copied in and out
                    let fdef = &self.p.funcs[*func];
                    let mut frame = Frame { locals: vec![None; fdef.locals.len()], this: rv, ranges: fdef.locals.iter().map(|l| l.range).collect() };
                    for (i, a) in av.into_iter().enumerate() {
                        frame.locals[fdef.params[i]] = Some(ranged(&fdef.locals[fdef.params[i]], a));
                    }
                    self.depth += 1;
                    let flow = self.block(&fdef.body, &mut frame)?;
                    self.depth -= 1;
                    if let (Some(pl), Some(t)) = (place, frame.this.clone()) {
                        self.store(pl, t, fr)?;
                    }
                    return Ok(match flow {
                        Flow::Return(v) => v,
                        _ => None,
                    });
                }
                self.call(*func, rv, av)
            }
            _ => self.expr(e, fr).map(Some),
        }
    }

    fn store(&mut self, target: &Expr, v: ConstVal, fr: &mut Frame) -> Result<(), EvalErr> {
        match &target.kind {
            ExprKind::Local(l) => {
                // a ranged variable saturates on every write, as at run time
                let v = match fr.ranges.get(*l).copied().flatten() {
                    Some((lo, hi)) => crate::codegen::clamp_const(v, lo, hi),
                    None => v,
                };
                fr.locals[*l] = Some(v);
                Ok(())
            }
            ExprKind::StructSelf => {
                fr.this = Some(v);
                Ok(())
            }
            ExprKind::Field { base, idx, .. } => {
                let mut b = self.expr(base, fr)?;
                if let ConstVal::Struct(_, ref mut vals) = b {
                    vals[*idx] = v;
                } else {
                    return nc("field store on non-struct");
                }
                self.store(base, b, fr)
            }
            _ => nc("assignment target is not compile-time evaluable"),
        }
    }

    fn expr(&mut self, e: &Expr, fr: &mut Frame) -> Result<ConstVal, EvalErr> {
        self.tick()?;
        let v = match &e.kind {
            ExprKind::Int(n) => ConstVal::Int(*n as i32),
            ExprKind::Float(f) => ConstVal::Float(*f as f32),
            ExprKind::Bool(b) => ConstVal::Bool(*b),
            ExprKind::Null => ConstVal::Null,
            ExprKind::Str(s) => ConstVal::Str(s.clone()),
            ExprKind::EnumVal(en, v) => ConstVal::Enum(*en, *v),
            ExprKind::Const(c) => match &self.p.consts[*c].value {
                Some(v) => v.clone(),
                None => return Err(EvalErr::Pending),
            },
            ExprKind::Local(l) => match fr.locals.get(*l).cloned().flatten() {
                Some(v) => v,
                None => return nc("local is not a compile-time value"),
            },
            ExprKind::StructSelf => match &fr.this {
                Some(v) => v.clone(),
                None => return nc("'this' is not available"),
            },
            ExprKind::Field { base, idx, .. } => match self.expr(base, fr)? {
                ConstVal::Struct(_, vals) => vals[*idx].clone(),
                _ => return nc("field access on non-struct"),
            },
            ExprKind::Unary(op, x) => {
                let v = self.expr(x, fr)?;
                match (op, v) {
                    (UnOp::Neg, ConstVal::Int(n)) if checked() && n == i32::MIN => return nc("arithmetic overflow (checked)"),
                    (UnOp::Neg, ConstVal::Int(n)) => ConstVal::Int(n.wrapping_neg()),
                    (UnOp::Neg, ConstVal::Float(f)) => ConstVal::Float(-f),
                    (UnOp::Not, ConstVal::Bool(b)) => ConstVal::Bool(!b),
                    (UnOp::BitNot, ConstVal::Int(n)) => ConstVal::Int(!n),
                    (UnOp::Plus, v) => v,
                    _ => return nc("bad unary operand"),
                }
            }
            ExprKind::Binary(op, a, b) => {
                if *op == BinOp::And || *op == BinOp::Or {
                    let av = self.bool(a, fr)?;
                    if (*op == BinOp::And && !av) || (*op == BinOp::Or && av) {
                        return Ok(ConstVal::Bool(av));
                    }
                    return Ok(ConstVal::Bool(self.bool(b, fr)?));
                }
                let av = self.expr(a, fr)?;
                let bv = self.expr(b, fr)?;
                let (av, bv) = promote(av, bv);
                binop(*op, av, bv)?
            }
            ExprKind::Cond(c, a, b) => {
                if self.bool(c, fr)? {
                    self.expr(a, fr)?
                } else {
                    self.expr(b, fr)?
                }
            }
            ExprKind::Switch { tmp, scrut, arms, default } => {
                if let Some(t) = tmp {
                    let v = self.expr(scrut, fr)?;
                    if fr.locals.len() <= *t {
                        fr.locals.resize(*t + 1, None);
                    }
                    fr.locals[*t] = Some(v);
                }
                let mut out = None;
                for (c, v) in arms {
                    if self.bool(c, fr)? {
                        out = Some(self.expr(v, fr)?);
                        break;
                    }
                }
                match (out, default) {
                    (Some(v), _) => v,
                    (None, Some(d)) => self.expr(d, fr)?,
                    (None, None) => return nc("switch expression: no arm matches the value"),
                }
            }
            ExprKind::Cast(x) => {
                let v = self.expr(x, fr)?;
                match (&e.ty, v) {
                    (Ty::Float, ConstVal::Int(n)) => ConstVal::Float(n as f32),
                    (Ty::Float, ConstVal::Float(f)) => ConstVal::Float(f),
                    (Ty::Int, ConstVal::Float(f)) if checked() && !(f >= -2147483648.0 && f < 2147483648.0) => return nc("arithmetic overflow (checked)"),
                    (Ty::Int, ConstVal::Float(f)) => ConstVal::Int(f2i(f)),
                    (Ty::Int, ConstVal::Int(n)) => ConstVal::Int(n),
                    (Ty::Int, ConstVal::Enum(_, v)) => ConstVal::Int(v as i32),
                    (Ty::Enum(en), ConstVal::Int(n)) => ConstVal::Enum(*en, n as i64),
                    (Ty::Char, ConstVal::Int(n)) if checked() && !(0..=0x10FFFF).contains(&n) => return nc("arithmetic overflow (checked)"),
                    (Ty::Char, ConstVal::Int(n)) => ConstVal::Int(crate::uchar::to_char(n)),
                    (Ty::Char, ConstVal::Float(f)) => ConstVal::Int(crate::uchar::to_char(f2i(f))),
                    _ => return nc("unsupported cast"),
                }
            }
            // a delegate that captures nothing is a constant (its function number)
            ExprKind::Closure { dg, num, caps } if caps.is_empty() => ConstVal::Fn(*dg, *num),
            ExprKind::DgFn(x) => match self.expr(x, fr)? {
                ConstVal::Fn(_, n) => ConstVal::Int(n as i32),
                _ => return Err(EvalErr::NotConst("a delegate call".into())),
            },
            ExprKind::StructLit { sid, ctor, args, inits } => {
                let mut v = struct_default_raw(self.p, *sid)?;
                if let Some(c) = ctor {
                    let mut av = vec![];
                    for a in args {
                        av.push(self.expr(&a.expr, fr)?);
                    }
                    v = match self.call(*c, Some(v), av)? {
                        Some(v) => v,
                        None => return nc("constructor did not produce a value"),
                    };
                }
                for (idx, ie) in inits {
                    let iv = self.expr(ie, fr)?;
                    if let ConstVal::Struct(_, ref mut vals) = v {
                        vals[*idx] = iv;
                    }
                }
                v
            }
            ExprKind::OpCall { func, args } => {
                let mut av = vec![];
                for a in args {
                    av.push(self.expr(a, fr)?);
                }
                match self.call(*func, None, av)? {
                    Some(v) => v,
                    None => return nc("operator returned no value"),
                }
            }
            ExprKind::Call { .. } | ExprKind::Assign { .. } | ExprKind::IncDec { .. } => match self.expr_any(e, fr)? {
                Some(v) => v,
                None => return nc("void call used as a value"),
            },
            ExprKind::Builtin { f: Builtin::Saturate, args, .. } => {
                // the bounds are exact (f64) literals, like the field contracts' clamp_const
                let v = self.expr(&args[0], fr)?;
                let (ExprKind::Float(lo), ExprKind::Float(hi)) = (&args[1].kind, &args[2].kind) else { return nc("malformed range") };
                crate::codegen::clamp_const(v, *lo, *hi)
            }
            ExprKind::Builtin { f, args, .. } => {
                let mut av = vec![];
                for a in args {
                    av.push(self.expr(a, fr)?);
                }
                // a char is a code point (ConstVal::Int typed char): its text, not its number
                if let (Builtin::ToStr, Some(Ty::Char), [ConstVal::Int(c)]) = (f, args.first().map(|a| &a.ty), av.as_slice()) {
                    return Ok(ConstVal::Str(crate::uchar::char_string(*c)));
                }
                let uc = crate::uchar::to_upper;
                match (f, av.as_slice()) {
                    (Builtin::CharIsDigit, [ConstVal::Int(c)]) => return Ok(ConstVal::Bool(crate::uchar::is_digit(*c as u32))),
                    (Builtin::CharIsLetter, [ConstVal::Int(c)]) => return Ok(ConstVal::Bool(crate::uchar::is_letter(*c as u32))),
                    (Builtin::CharIsLetterOrDigit, [ConstVal::Int(c)]) => return Ok(ConstVal::Bool(crate::uchar::is_letter(*c as u32) || crate::uchar::is_digit(*c as u32))),
                    (Builtin::CharIsWhiteSpace, [ConstVal::Int(c)]) => return Ok(ConstVal::Bool(crate::uchar::is_white_space(*c as u32))),
                    (Builtin::CharIsUpper, [ConstVal::Int(c)]) => return Ok(ConstVal::Bool(crate::uchar::is_upper(*c as u32))),
                    (Builtin::CharIsLower, [ConstVal::Int(c)]) => return Ok(ConstVal::Bool(crate::uchar::is_lower(*c as u32))),
                    (Builtin::CharIsPunctuation, [ConstVal::Int(c)]) => return Ok(ConstVal::Bool(crate::uchar::is_punctuation(*c as u32))),
                    (Builtin::CharToUpper, [ConstVal::Int(c)]) => return Ok(ConstVal::Int(uc(*c as u32) as i32)),
                    (Builtin::CharToLower, [ConstVal::Int(c)]) => return Ok(ConstVal::Int(crate::uchar::to_lower(*c as u32) as i32)),
                    (Builtin::StrCharAt, [ConstVal::Str(s), ConstVal::Int(i)]) => {
                        return match (*i >= 0).then(|| s.chars().nth(*i as usize)).flatten() {
                            Some(c) => Ok(ConstVal::Int(c as i32)),
                            None => nc("index was outside the bounds of the string"),
                        };
                    }
                    _ => {}
                }
                match (f, av.as_slice()) {
                    (Builtin::MathSqrt, [ConstVal::Float(x)]) => ConstVal::Float(x.sqrt()),
                    (Builtin::MathFloor, [ConstVal::Float(x)]) => ConstVal::Float(x.floor()),
                    (Builtin::MathAbs, [ConstVal::Float(x)]) => ConstVal::Float(x.abs()),
                    (Builtin::MathAbs, [ConstVal::Int(x)]) if checked() && *x == i32::MIN => return nc("arithmetic overflow (checked)"),
                    (Builtin::MathAbs, [ConstVal::Int(x)]) => ConstVal::Int(x.wrapping_abs()),
                    (Builtin::MathMin, [ConstVal::Int(a), ConstVal::Int(b)]) => ConstVal::Int(*a.min(b)),
                    (Builtin::MathMax, [ConstVal::Int(a), ConstVal::Int(b)]) => ConstVal::Int(*a.max(b)),
                    (Builtin::MathMin, [ConstVal::Float(a), ConstVal::Float(b)]) => ConstVal::Float(crate::detmath::minf(*a, *b)),
                    (Builtin::MathMax, [ConstVal::Float(a), ConstVal::Float(b)]) => ConstVal::Float(crate::detmath::maxf(*a, *b)),
                    (Builtin::MathSin, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::sinf(*x)),
                    (Builtin::MathTan, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::tanf(*x)),
                    (Builtin::MathAtan, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::atanf(*x)),
                    (Builtin::MathAtan2, [ConstVal::Float(y), ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::atan2f(*y, *x)),
                    (Builtin::MathAsin, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::asinf(*x)),
                    (Builtin::MathAcos, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::acosf(*x)),
                    (Builtin::MathExp, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::expf(*x)),
                    (Builtin::MathLog, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::logf(*x)),
                    (Builtin::MathLog10, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::log10f(*x)),
                    (Builtin::MathLog2, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::log2f(*x)),
                    (Builtin::MathPow, [ConstVal::Float(x), ConstVal::Float(y)]) => ConstVal::Float(crate::detmath::powf(*x, *y)),
                    (Builtin::MathRound, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::roundf(*x)),
                    (Builtin::MathCeiling, [ConstVal::Float(x)]) => ConstVal::Float(x.ceil()),
                    (Builtin::MathTruncate, [ConstVal::Float(x)]) => ConstVal::Float(x.trunc()),
                    (Builtin::MathSign, [ConstVal::Float(x)]) if !x.is_nan() => ConstVal::Int((*x > 0.0) as i32 - (*x < 0.0) as i32),
                    (Builtin::MathSign, [ConstVal::Int(x)]) => ConstVal::Int(x.signum()),
                    (Builtin::MathCos, [ConstVal::Float(x)]) => ConstVal::Float(crate::detmath::cosf(*x)),
                    (Builtin::MathClamp, [ConstVal::Int(x), ConstVal::Int(lo), ConstVal::Int(hi)]) => {
                        ConstVal::Int(if x < lo { *lo } else if x > hi { *hi } else { *x })
                    }
                    (Builtin::MathClamp, [ConstVal::Float(x), ConstVal::Float(lo), ConstVal::Float(hi)]) => {
                        ConstVal::Float(if x < lo { *lo } else if x > hi { *hi } else { *x })
                    }
                    // strings: lengths and indices count code points (like the runtime)
                    (Builtin::StrLen, [ConstVal::Str(s)]) => ConstVal::Int(s.chars().count() as i32),
                    (Builtin::StrIsEmpty, [ConstVal::Str(s)]) => ConstVal::Bool(s.is_empty()),
                    (Builtin::StrContains, [ConstVal::Str(s), ConstVal::Str(t)]) => ConstVal::Bool(s.contains(t.as_str())),
                    (Builtin::StrStarts, [ConstVal::Str(s), ConstVal::Str(t)]) => ConstVal::Bool(s.starts_with(t.as_str())),
                    (Builtin::StrEnds, [ConstVal::Str(s), ConstVal::Str(t)]) => ConstVal::Bool(s.ends_with(t.as_str())),
                    (Builtin::StrIndexOf, [ConstVal::Str(s), ConstVal::Str(t)]) => {
                        ConstVal::Int(s.find(t.as_str()).map(|b| s[..b].chars().count() as i32).unwrap_or(-1))
                    }
                    (Builtin::StrUpper, [ConstVal::Str(s)]) => ConstVal::Str(crate::uchar::str_upper(s)),
                    (Builtin::StrLower, [ConstVal::Str(s)]) => ConstVal::Str(crate::uchar::str_lower(s)),
                    (Builtin::StrCompare, [ConstVal::Str(a), ConstVal::Str(b)]) => ConstVal::Int(match a.as_bytes().cmp(b.as_bytes()) {
                        std::cmp::Ordering::Less => -1,
                        std::cmp::Ordering::Equal => 0,
                        std::cmp::Ordering::Greater => 1,
                    }),
                    (Builtin::ToStr, [ConstVal::Int(n)]) => ConstVal::Str(n.to_string()),
                    (Builtin::ToStr, [ConstVal::Bool(b)]) => ConstVal::Str(if *b { "True" } else { "False" }.into()),
                    (Builtin::ToStr, [ConstVal::Str(s)]) => ConstVal::Str(s.clone()),
                    (Builtin::ToStr, [ConstVal::Enum(e, v)]) => match self.p.enums[*e].variants.iter().find(|(_, x)| x == v) {
                        Some((n, _)) => ConstVal::Str(n.clone()),
                        None => return nc("enum value has no name"),
                    },
                    _ => return nc("this builtin is not compile-time evaluable"),
                }
            }
            _ => return nc("expression depends on runtime state"),
        };
        Ok(v)
    }
}

fn promote(a: ConstVal, b: ConstVal) -> (ConstVal, ConstVal) {
    match (&a, &b) {
        (ConstVal::Int(x), ConstVal::Float(_)) => (ConstVal::Float(*x as f32), b),
        (ConstVal::Float(_), ConstVal::Int(y)) => (a, ConstVal::Float(*y as f32)),
        _ => (a, b),
    }
}

fn coerce_val(v: ConstVal, t: &Ty) -> ConstVal {
    match (v, t) {
        (ConstVal::Int(n), Ty::Float) => ConstVal::Float(n as f32),
        (v, _) => v,
    }
}

/// `sijoc build --checked`: integer overflow traps (C# checked), so CTFE must not fold an
/// overflowing operation into a wrapped constant
pub static CHECKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `sijoc build --profile-fields`: the program counts every entity field access (PGO)
pub static FIELD_COUNTS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `sijoc build --gpu`: offload the pure self-write systems to a compute device (OpenMP target)
pub static GPU: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// `sijoc build --target wasm32`: the generated C carries its own C library (runtime/sijo_wasm.h)
pub static WASM: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn checked() -> bool {
    CHECKED.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn binop(op: BinOp, a: ConstVal, b: ConstVal) -> Result<ConstVal, EvalErr> {
    use BinOp::*;
    use ConstVal as V;
    if let (V::Int(x), V::Int(y), true) = (&a, &b, checked()) {
        let over = match op {
            Add => x.checked_add(*y).is_none(),
            Sub => x.checked_sub(*y).is_none(),
            Mul => x.checked_mul(*y).is_none(),
            Div | Rem => *x == i32::MIN && *y == -1,
            _ => false,
        };
        if over {
            return nc("arithmetic overflow (checked)");
        }
    }
    Ok(match (a, b) {
        (V::Int(x), V::Int(y)) => match op {
            Add => V::Int(x.wrapping_add(y)),
            Sub => V::Int(x.wrapping_sub(y)),
            Mul => V::Int(x.wrapping_mul(y)),
            Div => {
                if y == 0 {
                    return nc("division by zero");
                }
                V::Int(x.wrapping_div(y))
            }
            Rem => {
                if y == 0 {
                    return nc("division by zero");
                }
                V::Int(x.wrapping_rem(y))
            }
            BitAnd => V::Int(x & y),
            BitOr => V::Int(x | y),
            BitXor => V::Int(x ^ y),
            Shl => V::Int(x.wrapping_shl(y as u32 & 31)),
            Shr => V::Int(x.wrapping_shr(y as u32 & 31)),
            Eq => V::Bool(x == y),
            Ne => V::Bool(x != y),
            Lt => V::Bool(x < y),
            Le => V::Bool(x <= y),
            Gt => V::Bool(x > y),
            Ge => V::Bool(x >= y),
            And | Or => return nc("bad operands"),
        },
        (V::Float(x), V::Float(y)) => match op {
            Add => V::Float(x + y),
            Sub => V::Float(x - y),
            Mul => V::Float(x * y),
            Div => V::Float(x / y),
            Rem => V::Float(x % y),
            Eq => V::Bool(x == y),
            Ne => V::Bool(x != y),
            Lt => V::Bool(x < y),
            Le => V::Bool(x <= y),
            Gt => V::Bool(x > y),
            Ge => V::Bool(x >= y),
            _ => return nc("bad float operator"),
        },
        (V::Str(x), V::Str(y)) => match op {
            Add => V::Str(x + &y),
            Eq => V::Bool(x == y),
            Ne => V::Bool(x != y),
            _ => return nc("bad string operator"),
        },
        (V::Bool(x), V::Bool(y)) => match op {
            Eq => V::Bool(x == y),
            Ne => V::Bool(x != y),
            BitAnd | And => V::Bool(x & y),
            BitOr | Or => V::Bool(x | y),
            BitXor => V::Bool(x ^ y),
            _ => return nc("bad bool operator"),
        },
        (V::Enum(_, x), V::Enum(_, y)) => match op {
            Eq => V::Bool(x == y),
            Ne => V::Bool(x != y),
            Lt => V::Bool(x < y),
            Le => V::Bool(x <= y),
            Gt => V::Bool(x > y),
            Ge => V::Bool(x >= y),
            _ => return nc("bad enum operator"),
        },
        (V::Null, V::Null) => match op {
            Eq => V::Bool(true),
            Ne => V::Bool(false),
            _ => return nc("bad operator"),
        },
        _ => return nc("operands are not compile-time comparable"),
    })
}
