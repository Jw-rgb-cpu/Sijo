//! Equality saturation (egg-style) over int and bool expressions, with an interval analysis.
//!
//! An e-graph holds many equivalent forms of an expression at once: each e-class is a set of
//! e-nodes that compute the same value, and each e-node's children are e-classes. Rewrite rules
//! add equal forms (they never remove one), so the order in which they fire does not matter; when
//! nothing new appears (or a size limit is hit) the cheapest form is extracted.
//!
//! The e-class analysis tracks, for every class, an interval of the values it can take and whether
//! evaluating it can panic. Intervals come from the language: range types (`int in 0..100`, every
//! write saturates), immutable locals (their initializer), loop counters, collection lengths. With
//! them the optimizer decides comparisons, drops saturations that cannot change a value, narrows
//! clamps to one side, turns `%`/`/` by powers of two into masks and shifts when the operand is not
//! negative, and the caller removes bounds checks and field clamps.
//!
//! Soundness rules (the result must be bit-identical to the unoptimized program):
//! - int arithmetic wraps (C# unchecked), which is a commutative ring: reassociation is exact.
//!   Under `--checked` overflow traps, so the ring rules are off and an op is panic-free only when
//!   its interval proves that it cannot overflow.
//! - only pure expression trees go in; atoms are pure sub-expressions the e-graph does not model.
//! - a rule that drops an operand (`x * 0`, `x - x`) requires that operand to be panic-free, and a
//!   rule that swaps the evaluation order of two operands requires one of them to be panic-free
//!   (so the first panic, and its message, stay the same).
//! - floats are not modeled (IEEE arithmetic has few exact identities; NaN payloads reach hashes).

use std::collections::HashMap;

pub type Id = u32;

const I32_MIN: i64 = i32::MIN as i64;
const I32_MAX: i64 = i32::MAX as i64;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Op {
    Int(i32),
    Bool(bool),
    /// a sub-expression the e-graph does not look into (index into the atom table)
    Atom(u32),
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    And,
    Or,
    Xor,
    Neg,
    BNot,
    Min,
    Max,
    Abs,
    /// x < lo ? lo : (x > hi ? hi : x) — a range type's contract or Math.Clamp with constant bounds
    Clamp(i32, i32),
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    LAnd,
    LOr,
    LNot,
    BEq,
    BNe,
    /// c ? a : b
    Sel,
}

impl Op {
    pub fn arity(self) -> usize {
        use Op::*;
        match self {
            Int(_) | Bool(_) | Atom(_) => 0,
            Neg | BNot | Abs | Clamp(..) | LNot => 1,
            Sel => 3,
            _ => 2,
        }
    }
    /// does the class hold a bool (otherwise an int)?
    pub fn is_bool(self) -> bool {
        use Op::*;
        matches!(self, Bool(_) | Lt | Le | Gt | Ge | Eq | Ne | LAnd | LOr | LNot | BEq | BNe)
    }
    fn name(self) -> &'static str {
        use Op::*;
        match self {
            Int(_) => "int",
            Bool(_) => "bool",
            Atom(_) => "atom",
            Add => "add",
            Sub => "sub",
            Mul => "mul",
            Div => "div",
            Rem => "rem",
            Shl => "shl",
            Shr => "shr",
            And => "and",
            Or => "or",
            Xor => "xor",
            Neg => "neg",
            BNot => "bnot",
            Min => "min",
            Max => "max",
            Abs => "abs",
            Clamp(..) => "clamp",
            Lt => "lt",
            Le => "le",
            Gt => "gt",
            Ge => "ge",
            Eq => "eq",
            Ne => "ne",
            LAnd => "land",
            LOr => "lor",
            LNot => "lnot",
            BEq => "beq",
            BNe => "bne",
            Sel => "sel",
        }
    }
    fn from_name(s: &str) -> Option<Op> {
        use Op::*;
        Some(match s {
            "add" => Add,
            "sub" => Sub,
            "mul" => Mul,
            "div" => Div,
            "rem" => Rem,
            "shl" => Shl,
            "shr" => Shr,
            "and" => And,
            "or" => Or,
            "xor" => Xor,
            "neg" => Neg,
            "bnot" => BNot,
            "min" => Min,
            "max" => Max,
            "abs" => Abs,
            "lt" => Lt,
            "le" => Le,
            "gt" => Gt,
            "ge" => Ge,
            "eq" => Eq,
            "ne" => Ne,
            "land" => LAnd,
            "lor" => LOr,
            "lnot" => LNot,
            "beq" => BEq,
            "bne" => BNe,
            "sel" => Sel,
            "true" => Bool(true),
            "false" => Bool(false),
            _ => return s.parse::<i32>().ok().map(Int),
        })
    }
    /// extraction cost of the operation itself (children add theirs)
    fn cost(self) -> u32 {
        use Op::*;
        match self {
            Int(_) | Bool(_) => 1,
            Atom(_) => 0, // the atom table holds the cost
            Mul => 3,
            Div | Rem => 10,
            Min | Max | Abs | Sel => 2,
            Clamp(..) => 4,
            _ => 1,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ENode {
    pub op: Op,
    pub args: [Id; 3],
}

impl ENode {
    pub fn leaf(op: Op) -> ENode {
        ENode { op, args: [0; 3] }
    }
    pub fn new(op: Op, kids: &[Id]) -> ENode {
        let mut args = [0; 3];
        args[..kids.len()].copy_from_slice(kids);
        ENode { op, args }
    }
    pub fn kids(&self) -> &[Id] {
        &self.args[..self.op.arity()]
    }
}

/// What the analysis knows about a class: the values it can take (ints: an interval inside i32;
/// bools: 0..1 / 0..0 / 1..1) and whether evaluating it can panic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Data {
    pub boolean: bool,
    pub lo: i64,
    pub hi: i64,
    pub safe: bool,
    /// no side effects (an effectful class keeps its place among the others: never dropped,
    /// duplicated or swapped)
    pub pure: bool,
}

impl Data {
    pub fn constant(&self) -> Option<i64> {
        (self.lo == self.hi).then_some(self.lo)
    }
}

/// facts about an atom, supplied by the caller
#[derive(Clone, Copy, Debug)]
pub struct AtomInfo {
    pub boolean: bool,
    pub lo: i64,
    pub hi: i64,
    /// cannot panic (implies pure)
    pub safe: bool,
    pub pure: bool,
    pub cost: u32,
}

pub struct EGraph {
    nodes: Vec<ENode>,
    /// class each node was added to (follow `find`)
    node_class: Vec<Id>,
    memo: HashMap<ENode, Id>,
    parent: Vec<Id>,
    /// per root class: indices of its (canonical, distinct) nodes
    members: Vec<Vec<usize>>,
    data: Vec<Data>,
    pub atoms: Vec<AtomInfo>,
    /// --checked: integer overflow traps
    checked: bool,
    changed: bool,
    /// rewrites that added something new, per rule name (deterministic order of first use)
    pub fired: Vec<(&'static str, usize)>,
}

impl EGraph {
    pub fn new(checked: bool) -> EGraph {
        EGraph { nodes: vec![], node_class: vec![], memo: HashMap::new(), parent: vec![], members: vec![], data: vec![], atoms: vec![], checked, changed: false, fired: vec![] }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn find(&self, mut c: Id) -> Id {
        while self.parent[c as usize] != c {
            c = self.parent[c as usize];
        }
        c
    }

    fn find_compress(&mut self, c: Id) -> Id {
        let r = self.find(c);
        let mut x = c;
        while self.parent[x as usize] != r {
            let nx = self.parent[x as usize];
            self.parent[x as usize] = r;
            x = nx;
        }
        r
    }

    fn canon(&self, n: &ENode) -> ENode {
        let mut m = *n;
        for i in 0..n.op.arity() {
            m.args[i] = self.find(n.args[i]);
        }
        m
    }

    pub fn data(&self, c: Id) -> Data {
        self.data[self.find(c) as usize]
    }

    /// adds a node (or finds the class that already has it)
    pub fn add(&mut self, n: ENode) -> Id {
        let n = self.canon(&n);
        if let Some(&c) = self.memo.get(&n) {
            return self.find(c);
        }
        let id = self.parent.len() as Id;
        self.parent.push(id);
        self.nodes.push(n);
        self.node_class.push(id);
        self.memo.insert(n, id);
        self.members.push(vec![self.nodes.len() - 1]);
        let d = self.eval(&n);
        self.data.push(d);
        self.changed = true;
        id
    }

    pub fn add_atom(&mut self, info: AtomInfo) -> Id {
        self.atoms.push(info);
        self.add(ENode::leaf(Op::Atom(self.atoms.len() as u32 - 1)))
    }

    /// merges two classes; the smaller id stays the root (deterministic)
    pub fn union(&mut self, a: Id, b: Id) -> bool {
        let (a, b) = (self.find_compress(a), self.find_compress(b));
        if a == b {
            return false;
        }
        let (root, child) = if a < b { (a, b) } else { (b, a) };
        if self.data[root as usize].boolean != self.data[child as usize].boolean {
            return false; // never merge an int class with a bool class (a rule bug otherwise)
        }
        self.parent[child as usize] = root;
        let moved = std::mem::take(&mut self.members[child as usize]);
        self.members[root as usize].extend(moved);
        let (x, y) = (self.data[root as usize], self.data[child as usize]);
        self.data[root as usize] = merge(x, y);
        self.changed = true;
        true
    }

    // ---- analysis

    fn eval(&self, n: &ENode) -> Data {
        let mut d = self.eval_values(n);
        d.pure = match n.op {
            Op::Atom(a) => self.atoms[a as usize].pure,
            _ => n.kids().iter().all(|&k| self.data[self.find(k) as usize].pure),
        };
        // a panic-free class has no side effects either
        d.safe &= d.pure;
        d
    }

    fn eval_values(&self, n: &ENode) -> Data {
        use Op::*;
        let k = |i: usize| self.data[self.find(n.args[i]) as usize];
        let int = |lo: i64, hi: i64, safe: bool| Data { boolean: false, lo, hi, safe, pure: true };
        let boolv = |lo: i64, hi: i64, safe: bool| Data { boolean: true, lo, hi, safe, pure: true };
        // an int result: exact bounds inside i32 or anything (it wraps); under --checked it traps
        let fit = |lo: i64, hi: i64, kids_safe: bool, checked: bool| {
            if lo >= I32_MIN && hi <= I32_MAX {
                int(lo, hi, kids_safe)
            } else {
                int(I32_MIN, I32_MAX, kids_safe && !checked)
            }
        };
        let ck = self.checked;
        // two known operands: the exact (wrapping) result, like constant folding at run time
        if matches!(n.op, Add | Sub | Mul | Shl | Neg) {
            let kids: Vec<Data> = n.kids().iter().map(|&c| self.data[self.find(c) as usize]).collect();
            if let Some(vals) = kids.iter().map(|d| d.constant().map(|v| v as i32)).collect::<Option<Vec<i32>>>() {
                let safe = kids.iter().all(|d| d.safe);
                let (v, over) = match (n.op, vals.as_slice()) {
                    (Add, [a, b]) => a.overflowing_add(*b),
                    (Sub, [a, b]) => a.overflowing_sub(*b),
                    (Mul, [a, b]) => a.overflowing_mul(*b),
                    (Shl, [a, b]) => (a.wrapping_shl(*b as u32 & 31), false),
                    (Neg, [a]) => a.overflowing_neg(),
                    _ => unreachable!(),
                };
                return int(v as i64, v as i64, safe && !(ck && over));
            }
        }
        match n.op {
            Int(c) => int(c as i64, c as i64, true),
            Bool(b) => boolv(b as i64, b as i64, true),
            Atom(a) => {
                let i = self.atoms[a as usize];
                Data { boolean: i.boolean, lo: i.lo, hi: i.hi, safe: i.safe, pure: i.pure }
            }
            Add => {
                let (a, b) = (k(0), k(1));
                fit(a.lo + b.lo, a.hi + b.hi, a.safe && b.safe, ck)
            }
            Sub => {
                let (a, b) = (k(0), k(1));
                fit(a.lo - b.hi, a.hi - b.lo, a.safe && b.safe, ck)
            }
            Mul => {
                let (a, b) = (k(0), k(1));
                let ps = [a.lo * b.lo, a.lo * b.hi, a.hi * b.lo, a.hi * b.hi];
                fit(*ps.iter().min().unwrap(), *ps.iter().max().unwrap(), a.safe && b.safe, ck)
            }
            Neg => {
                let a = k(0);
                fit(-a.hi, -a.lo, a.safe, ck)
            }
            Div | Rem => {
                let (a, b) = (k(0), k(1));
                let Some(c) = b.constant() else { return int(I32_MIN, I32_MAX, false) };
                if c == 0 {
                    return int(I32_MIN, I32_MAX, false);
                }
                // MIN / -1 wraps (unchecked) or traps (checked)
                let safe = a.safe && b.safe && !(ck && c == -1 && a.lo == I32_MIN);
                if n.op == Div {
                    if c == -1 {
                        return fit(-a.hi, -a.lo, safe, false);
                    }
                    let (x, y) = (a.lo / c, a.hi / c); // truncation is monotonic
                    int(x.min(y), x.max(y), safe)
                } else {
                    let m = c.abs() - 1;
                    if a.lo >= 0 {
                        int(0, a.hi.min(m), safe)
                    } else if a.hi <= 0 {
                        int(a.lo.max(-m), 0, safe)
                    } else {
                        int(-m, m, safe)
                    }
                }
            }
            Shl => {
                let (a, b) = (k(0), k(1));
                let safe = a.safe && b.safe;
                match b.constant() {
                    Some(s) => {
                        let s = s & 31;
                        let (lo, hi) = (a.lo << s, a.hi << s);
                        if lo >= I32_MIN && hi <= I32_MAX {
                            int(lo, hi, safe)
                        } else {
                            int(I32_MIN, I32_MAX, safe)
                        }
                    }
                    None => int(I32_MIN, I32_MAX, safe),
                }
            }
            Shr => {
                let (a, b) = (k(0), k(1));
                let safe = a.safe && b.safe;
                match b.constant() {
                    Some(s) => int(a.lo >> (s & 31), a.hi >> (s & 31), safe),
                    None if a.lo >= 0 => int(0, a.hi, safe),
                    None => int(a.lo.min(-1), a.hi.max(0), safe),
                }
            }
            And => {
                let (a, b) = (k(0), k(1));
                let safe = a.safe && b.safe;
                match (a.lo >= 0, b.lo >= 0) {
                    (true, true) => int(0, a.hi.min(b.hi), safe),
                    (true, false) => int(0, a.hi, safe),
                    (false, true) => int(0, b.hi, safe),
                    _ => int(I32_MIN, I32_MAX, safe),
                }
            }
            Or | Xor => {
                let (a, b) = (k(0), k(1));
                let safe = a.safe && b.safe;
                if a.lo >= 0 && b.lo >= 0 {
                    let m = a.hi.max(b.hi);
                    let mut p: i64 = 1;
                    while p <= m {
                        p <<= 1;
                    }
                    let lo = if n.op == Or { a.lo.max(b.lo) } else { 0 };
                    int(lo, p - 1, safe)
                } else {
                    int(I32_MIN, I32_MAX, safe)
                }
            }
            BNot => {
                let a = k(0);
                int(!a.hi, !a.lo, a.safe)
            }
            Min => {
                let (a, b) = (k(0), k(1));
                int(a.lo.min(b.lo), a.hi.min(b.hi), a.safe && b.safe)
            }
            Max => {
                let (a, b) = (k(0), k(1));
                int(a.lo.max(b.lo), a.hi.max(b.hi), a.safe && b.safe)
            }
            Abs => {
                let a = k(0);
                if a.lo >= 0 {
                    int(a.lo, a.hi, a.safe)
                } else if a.lo == I32_MIN {
                    // Abs(int.MinValue) wraps to itself (traps under --checked)
                    int(I32_MIN, I32_MAX, a.safe && !ck)
                } else if a.hi <= 0 {
                    int(-a.hi, -a.lo, a.safe)
                } else {
                    int(0, (-a.lo).max(a.hi), a.safe)
                }
            }
            Clamp(lo, hi) => {
                let a = k(0);
                let c = |v: i64| v.clamp(lo as i64, hi as i64);
                int(c(a.lo), c(a.hi), a.safe)
            }
            Lt | Le | Gt | Ge | Eq | Ne => {
                let (a, b) = (k(0), k(1));
                let safe = a.safe && b.safe;
                let (t, f) = match n.op {
                    Lt => (a.hi < b.lo, a.lo >= b.hi),
                    Le => (a.hi <= b.lo, a.lo > b.hi),
                    Gt => (a.lo > b.hi, a.hi <= b.lo),
                    Ge => (a.lo >= b.hi, a.hi < b.lo),
                    Eq => (a.lo == a.hi && b.lo == b.hi && a.lo == b.lo, a.hi < b.lo || b.hi < a.lo),
                    _ => (a.hi < b.lo || b.hi < a.lo, a.lo == a.hi && b.lo == b.hi && a.lo == b.lo),
                };
                if t {
                    boolv(1, 1, safe)
                } else if f {
                    boolv(0, 0, safe)
                } else {
                    boolv(0, 1, safe)
                }
            }
            BEq | BNe => {
                let (a, b) = (k(0), k(1));
                let safe = a.safe && b.safe;
                match (a.constant(), b.constant()) {
                    (Some(x), Some(y)) => {
                        let eq = (x == y) as i64;
                        let v = if n.op == BEq { eq } else { 1 - eq };
                        boolv(v, v, safe)
                    }
                    _ => boolv(0, 1, safe),
                }
            }
            LNot => {
                let a = k(0);
                boolv(1 - a.hi, 1 - a.lo, a.safe)
            }
            LAnd => {
                // the right side only runs when the left is true
                let (a, b) = (k(0), k(1));
                match a.constant() {
                    Some(0) => boolv(0, 0, a.safe),
                    Some(_) => boolv(b.lo, b.hi, a.safe && b.safe),
                    None => boolv(0, b.hi, a.safe && b.safe),
                }
            }
            LOr => {
                let (a, b) = (k(0), k(1));
                match a.constant() {
                    Some(1) => boolv(1, 1, a.safe),
                    Some(_) => boolv(b.lo, b.hi, a.safe && b.safe),
                    None => boolv(b.lo, 1, a.safe && b.safe),
                }
            }
            Sel => {
                let (c, a, b) = (k(0), k(1), k(2));
                let mk = |lo, hi, safe| Data { boolean: a.boolean, lo, hi, safe, pure: true };
                match c.constant() {
                    Some(1) => mk(a.lo, a.hi, c.safe && a.safe),
                    Some(_) => mk(b.lo, b.hi, c.safe && b.safe),
                    None => mk(a.lo.min(b.lo), a.hi.max(b.hi), c.safe && a.safe && b.safe),
                }
            }
        }
    }

    /// Restores the invariants after unions: congruent nodes share a class, every node is
    /// canonical, and the analysis is a fixpoint. Then classes whose value is known (and that
    /// cannot panic) get their constant as a node.
    pub fn rebuild(&mut self) {
        for _round in 0..16 {
            // congruence closure
            loop {
                self.memo.clear();
                let mut pending = vec![];
                for i in 0..self.nodes.len() {
                    let n = self.canon(&self.nodes[i]);
                    self.nodes[i] = n;
                    let c = self.find(self.node_class[i]);
                    match self.memo.get(&n) {
                        Some(&d) if self.find(d) != c => pending.push((d, c)),
                        Some(_) => {}
                        None => {
                            self.memo.insert(n, c);
                        }
                    }
                }
                if pending.is_empty() {
                    break;
                }
                for (a, b) in pending {
                    self.union(a, b);
                }
            }
            // members: distinct canonical nodes per root class
            for m in self.members.iter_mut() {
                m.clear();
            }
            let mut seen: HashMap<ENode, ()> = HashMap::new();
            for i in 0..self.nodes.len() {
                if seen.insert(self.nodes[i], ()).is_none() {
                    let c = self.find(self.node_class[i]) as usize;
                    self.members[c].push(i);
                }
            }
            // analysis fixpoint: intervals narrow (intersection over nodes), safety grows
            for _ in 0..32 {
                let mut again = false;
                for c in 0..self.parent.len() {
                    if self.parent[c] as usize != c || self.members[c].is_empty() {
                        continue;
                    }
                    let mut d = self.data[c];
                    for &i in &self.members[c] {
                        d = merge(d, self.eval(&self.nodes[i]));
                    }
                    if d != self.data[c] {
                        self.data[c] = d;
                        again = true;
                    }
                }
                if !again {
                    break;
                }
            }
            // known values become constants (only when evaluating the class cannot panic: the
            // constant must not hide a panic)
            let mut adds = vec![];
            for c in 0..self.parent.len() {
                if self.parent[c] as usize != c || self.members[c].is_empty() {
                    continue;
                }
                let d = self.data[c];
                if let (Some(v), true) = (d.constant(), d.safe) {
                    let op = if d.boolean { Op::Bool(v != 0) } else { Op::Int(v as i32) };
                    if !self.members[c].iter().any(|&i| self.nodes[i].op == op) {
                        adds.push((c as Id, op));
                    }
                }
            }
            if adds.is_empty() {
                return;
            }
            for (c, op) in adds {
                let k = self.add(ENode::leaf(op));
                self.union(c, k);
            }
        }
    }

    // ---- extraction

    /// cheapest node per class (tree cost) and its cost; ties go to the earlier node
    pub fn extract(&self) -> Vec<Option<(u32, usize)>> {
        let mut best: Vec<Option<(u32, usize)>> = vec![None; self.parent.len()];
        loop {
            let mut again = false;
            for c in 0..self.parent.len() {
                if self.parent[c] as usize != c {
                    continue;
                }
                for &i in &self.members[c] {
                    let n = &self.nodes[i];
                    let mut cost = match n.op {
                        Op::Atom(a) => self.atoms[a as usize].cost,
                        op => op.cost(),
                    };
                    let mut ok = true;
                    for &k in n.kids() {
                        match best[self.find(k) as usize] {
                            Some((kc, _)) => cost = cost.saturating_add(kc),
                            None => ok = false,
                        }
                    }
                    if !ok {
                        continue;
                    }
                    let better = match best[c] {
                        None => true,
                        Some((bc, bi)) => cost < bc || (cost == bc && i < bi && bi != i),
                    };
                    if better && best[c] != Some((cost, i)) {
                        best[c] = Some((cost, i));
                        again = true;
                    }
                }
            }
            if !again {
                return best;
            }
        }
    }

    pub fn node(&self, i: usize) -> ENode {
        self.nodes[i]
    }

    fn fire(&mut self, name: &'static str) {
        match self.fired.iter_mut().find(|(n, _)| *n == name) {
            Some(x) => x.1 += 1,
            None => self.fired.push((name, 1)),
        }
    }

    // ---- saturation

    /// Applies the rules until nothing new appears or a limit is reached.
    pub fn saturate(&mut self, rules: &[Rule], node_limit: usize, iters: usize) {
        self.rebuild();
        // backoff (as in egg): a rule that matches too often in one iteration (associativity and
        // commutativity on long sums) sits out the next ones, with a doubled limit afterwards
        let mut limit = vec![48usize; rules.len()];
        let mut banned_until = vec![0usize; rules.len()];
        for it in 0..iters {
            self.changed = false;
            let mut banned = false;
            let mut acts: Vec<(Id, Action)> = vec![];
            let roots: Vec<Id> = (0..self.parent.len() as Id).filter(|&c| self.parent[c as usize] == c).collect();
            for (ri, r) in rules.iter().enumerate() {
                if (r.ring && self.checked) || it < banned_until[ri] {
                    banned |= it < banned_until[ri];
                    continue;
                }
                let mut found = vec![];
                for &c in &roots {
                    for s in self.ematch(&r.lhs, c, Subst::default()) {
                        if r.cond.map(|f| f(self, &s)).unwrap_or(true) {
                            found.push((c, Action::Pat(r.name, &r.rhs, s)));
                        }
                    }
                }
                if found.len() > limit[ri] {
                    banned_until[ri] = it + 2;
                    limit[ri] *= 2;
                    banned = true;
                    continue;
                }
                acts.extend(found);
            }
            for &c in &roots {
                for &i in &self.members[c as usize].clone() {
                    let n = self.nodes[i];
                    if let Some((name, node)) = self.custom(&n) {
                        acts.push((c, Action::Node(name, node)));
                    }
                }
            }
            for (c, a) in acts {
                match a {
                    Action::Pat(name, rhs, s) => {
                        let before = self.nodes.len();
                        let r = self.inst(rhs, &s);
                        if self.union(c, r) || self.nodes.len() != before {
                            self.fire(name);
                        }
                    }
                    Action::Node(name, n) => {
                        let before = self.nodes.len();
                        let r = match n {
                            Custom::Class(x) => x,
                            Custom::Node(n) => self.add(n),
                            Custom::Two(op, x, lit) => {
                                let l = self.add(ENode::leaf(Op::Int(lit)));
                                self.add(ENode::new(op, &[x, l]))
                            }
                        };
                        if self.union(c, r) || self.nodes.len() != before {
                            self.fire(name);
                        }
                    }
                }
                if self.nodes.len() > node_limit {
                    break;
                }
            }
            self.rebuild();
            if (!self.changed && !banned) || self.nodes.len() > node_limit {
                return;
            }
        }
    }

    fn inst(&mut self, p: &Pat, s: &Subst) -> Id {
        match p {
            Pat::Var(v) => s.get(*v).expect("unbound pattern variable"),
            Pat::Node(op, kids) => {
                let ids: Vec<Id> = kids.iter().map(|k| self.inst(k, s)).collect();
                self.add(ENode::new(*op, &ids))
            }
        }
    }

    fn ematch(&self, p: &Pat, c: Id, s: Subst) -> Vec<Subst> {
        let c = self.find(c);
        match p {
            Pat::Var(v) => match s.get(*v) {
                Some(b) if self.find(b) == c => vec![s],
                Some(_) => vec![],
                None => vec![s.with(*v, c)],
            },
            Pat::Node(op, kids) => {
                let mut out = vec![];
                for &i in &self.members[c as usize] {
                    let n = &self.nodes[i];
                    if n.op != *op {
                        continue;
                    }
                    let mut partial = vec![s];
                    for (k, kp) in kids.iter().enumerate() {
                        let mut next = vec![];
                        for ps in partial {
                            next.extend(self.ematch(kp, n.args[k], ps));
                        }
                        partial = next;
                        if partial.is_empty() {
                            break;
                        }
                    }
                    out.extend(partial);
                }
                out
            }
        }
    }

    /// rewrites that need the analysis or computed constants
    fn custom(&self, n: &ENode) -> Option<(&'static str, Custom)> {
        use Op::*;
        let d = |i: usize| self.data(n.args[i]);
        let kid = |i: usize| self.find(n.args[i]);
        match n.op {
            // a saturation that cannot change the value, or only from one side
            Clamp(lo, hi) => {
                let a = d(0);
                if a.lo >= lo as i64 && a.hi <= hi as i64 {
                    Some(("clamp-in-range", Custom::Class(kid(0))))
                } else if a.lo >= lo as i64 {
                    Some(("clamp-upper-only", Custom::Two(Min, kid(0), hi)))
                } else if a.hi <= hi as i64 {
                    Some(("clamp-lower-only", Custom::Two(Max, kid(0), lo)))
                } else {
                    None
                }
            }
            Min | Max => {
                let (a, b) = (d(0), d(1));
                let first = if n.op == Min { a.hi <= b.lo } else { a.lo >= b.hi };
                let second = if n.op == Min { b.hi <= a.lo } else { b.lo >= a.hi };
                if first && b.safe {
                    Some(("minmax-decided", Custom::Class(kid(0))))
                } else if second && a.safe {
                    Some(("minmax-decided", Custom::Class(kid(1))))
                } else {
                    None
                }
            }
            Abs => {
                let a = d(0);
                if a.lo >= 0 {
                    Some(("abs-nonneg", Custom::Class(kid(0))))
                } else if a.hi <= 0 && a.lo > I32_MIN {
                    Some(("abs-nonpos", Custom::Node(ENode::new(Neg, &[kid(0)]))))
                } else {
                    None
                }
            }
            // x % 2^k == x & (2^k - 1) and x / 2^k == x >> k when x >= 0
            Rem | Div => {
                let (a, b) = (d(0), d(1));
                let c = b.constant()?;
                if a.lo < 0 || c < 2 || c & (c - 1) != 0 {
                    return None;
                }
                if n.op == Rem {
                    Some(("rem-pow2", Custom::Two(And, kid(0), (c - 1) as i32)))
                } else {
                    Some(("div-pow2", Custom::Two(Shr, kid(0), c.trailing_zeros() as i32)))
                }
            }
            // a mask that keeps every bit the value can have
            And => {
                let (a, b) = (d(0), d(1));
                let m = b.constant()?;
                (m >= 0 && m & (m + 1) == 0 && a.lo >= 0 && a.hi <= m).then(|| ("mask-noop", Custom::Class(kid(0))))
            }
            _ => None,
        }
    }
}

fn merge(a: Data, b: Data) -> Data {
    let (lo, hi) = (a.lo.max(b.lo), a.hi.min(b.hi));
    // an empty intersection means the code cannot run (or a rule is wrong): keep what we had
    let (lo, hi) = if lo <= hi { (lo, hi) } else { (a.lo, a.hi) };
    Data { boolean: a.boolean, lo, hi, safe: a.safe || b.safe, pure: a.pure || b.pure }
}

enum Action<'r> {
    Pat(&'static str, &'r Pat, Subst),
    Node(&'static str, Custom),
}

enum Custom {
    Class(Id),
    Node(ENode),
    /// op(x, literal)
    Two(Op, Id, i32),
}

// ---------------------------------------------------------------------------
// patterns and rules
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum Pat {
    Var(u8),
    Node(Op, Vec<Pat>),
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Subst {
    vals: [Option<Id>; 4],
}

impl Subst {
    pub fn get(&self, v: u8) -> Option<Id> {
        self.vals[v as usize]
    }
    fn with(mut self, v: u8, c: Id) -> Subst {
        self.vals[v as usize] = Some(c);
        self
    }
}

type Cond = fn(&EGraph, &Subst) -> bool;

pub struct Rule {
    pub name: &'static str,
    lhs: Pat,
    rhs: Pat,
    cond: Option<Cond>,
    /// valid only for wrapping arithmetic (off under --checked)
    ring: bool,
}

/// parses "(add ?a (neg ?b))"; variables are ?a ?b ?c ?d
fn parse_pat(s: &str) -> Pat {
    fn atom(tok: &str) -> Pat {
        if let Some(v) = tok.strip_prefix('?') {
            return Pat::Var(v.as_bytes()[0] - b'a');
        }
        Pat::Node(Op::from_name(tok).unwrap_or_else(|| panic!("bad pattern token {}", tok)), vec![])
    }
    fn go(toks: &[String], i: &mut usize) -> Pat {
        let t = toks[*i].clone();
        *i += 1;
        if t != "(" {
            return atom(&t);
        }
        let op = Op::from_name(&toks[*i]).unwrap_or_else(|| panic!("bad operator {}", toks[*i]));
        *i += 1;
        let mut kids = vec![];
        while toks[*i] != ")" {
            kids.push(go(toks, i));
        }
        *i += 1;
        assert_eq!(kids.len(), op.arity(), "arity of {}", op.name());
        Pat::Node(op, kids)
    }
    let toks: Vec<String> = s.replace('(', " ( ").replace(')', " ) ").split_whitespace().map(|x| x.to_string()).collect();
    let mut i = 0;
    go(&toks, &mut i)
}

fn safe(g: &EGraph, s: &Subst, v: u8) -> bool {
    s.get(v).map(|c| g.data(c).safe).unwrap_or(false)
}
fn safe_a(g: &EGraph, s: &Subst) -> bool {
    safe(g, s, 0)
}
fn pure(g: &EGraph, s: &Subst, v: u8) -> bool {
    s.get(v).map(|c| g.data(c).pure).unwrap_or(false)
}
/// swapping ?a and ?b: neither has side effects (the other could read them), and one cannot
/// panic (the first panic stays the same)
fn swap_ok(g: &EGraph, s: &Subst) -> bool {
    pure(g, s, 0) && pure(g, s, 1) && (safe(g, s, 0) || safe(g, s, 1))
}
/// ?a appears twice on the left and once on the right: it must not have side effects
fn pure_a(g: &EGraph, s: &Subst) -> bool {
    pure(g, s, 0)
}
fn both_safe(g: &EGraph, s: &Subst) -> bool {
    safe(g, s, 0) && safe(g, s, 1)
}

pub fn rules() -> Vec<Rule> {
    // (name, lhs, rhs, condition, ring-only)
    let table: &[(&'static str, &str, &str, Option<Cond>, bool)] = &[
        // a commutative ring (int arithmetic wraps)
        ("comm-add", "(add ?a ?b)", "(add ?b ?a)", Some(swap_ok), true),
        ("assoc-add", "(add (add ?a ?b) ?c)", "(add ?a (add ?b ?c))", None, true),
        ("assoc-add-r", "(add ?a (add ?b ?c))", "(add (add ?a ?b) ?c)", None, true),
        ("comm-mul", "(mul ?a ?b)", "(mul ?b ?a)", Some(swap_ok), true),
        ("assoc-mul", "(mul (mul ?a ?b) ?c)", "(mul ?a (mul ?b ?c))", None, true),
        ("assoc-mul-r", "(mul ?a (mul ?b ?c))", "(mul (mul ?a ?b) ?c)", None, true),
        ("sub-as-add", "(sub ?a ?b)", "(add ?a (neg ?b))", None, true),
        ("add-neg-as-sub", "(add ?a (neg ?b))", "(sub ?a ?b)", None, true),
        ("neg-neg", "(neg (neg ?a))", "?a", None, true),
        ("neg-sub", "(neg (sub ?a ?b))", "(sub ?b ?a)", Some(swap_ok), true),
        ("neg-mul", "(mul ?a -1)", "(neg ?a)", None, true),
        ("factor", "(add (mul ?a ?b) (mul ?a ?c))", "(mul ?a (add ?b ?c))", Some(pure_a), true),
        ("sub-self", "(sub ?a ?a)", "0", Some(safe_a), true),
        ("zero-sub", "(sub 0 ?a)", "(neg ?a)", None, true),
        // identities that hold even when overflow traps
        ("add-0", "(add ?a 0)", "?a", None, false),
        ("sub-0", "(sub ?a 0)", "?a", None, false),
        ("mul-1", "(mul ?a 1)", "?a", None, false),
        ("mul-0", "(mul ?a 0)", "0", Some(safe_a), false),
        ("div-1", "(div ?a 1)", "?a", None, false),
        ("rem-1", "(rem ?a 1)", "0", Some(safe_a), false),
        // bits
        ("comm-and", "(and ?a ?b)", "(and ?b ?a)", Some(swap_ok), false),
        ("comm-or", "(or ?a ?b)", "(or ?b ?a)", Some(swap_ok), false),
        ("comm-xor", "(xor ?a ?b)", "(xor ?b ?a)", Some(swap_ok), false),
        ("and-self", "(and ?a ?a)", "?a", None, false),
        ("or-self", "(or ?a ?a)", "?a", None, false),
        ("xor-self", "(xor ?a ?a)", "0", Some(safe_a), false),
        ("and-0", "(and ?a 0)", "0", Some(safe_a), false),
        ("and-all", "(and ?a -1)", "?a", None, false),
        ("or-0", "(or ?a 0)", "?a", None, false),
        ("or-all", "(or ?a -1)", "-1", Some(safe_a), false),
        ("xor-0", "(xor ?a 0)", "?a", None, false),
        ("bnot-bnot", "(bnot (bnot ?a))", "?a", None, false),
        ("shl-0", "(shl ?a 0)", "?a", None, false),
        ("shr-0", "(shr ?a 0)", "?a", None, false),
        // min / max
        ("min-self", "(min ?a ?a)", "?a", None, false),
        ("max-self", "(max ?a ?a)", "?a", None, false),
        ("comm-min", "(min ?a ?b)", "(min ?b ?a)", Some(swap_ok), false),
        ("comm-max", "(max ?a ?b)", "(max ?b ?a)", Some(swap_ok), false),
        // comparisons of ints (a total order: !(a < b) == b <= a)
        ("gt-as-lt", "(gt ?a ?b)", "(lt ?b ?a)", Some(swap_ok), false),
        ("ge-as-le", "(ge ?a ?b)", "(le ?b ?a)", Some(swap_ok), false),
        ("lt-as-gt", "(lt ?a ?b)", "(gt ?b ?a)", Some(swap_ok), false),
        ("le-as-ge", "(le ?a ?b)", "(ge ?b ?a)", Some(swap_ok), false),
        ("comm-eq", "(eq ?a ?b)", "(eq ?b ?a)", Some(swap_ok), false),
        ("comm-ne", "(ne ?a ?b)", "(ne ?b ?a)", Some(swap_ok), false),
        ("lt-self", "(lt ?a ?a)", "false", Some(safe_a), false),
        ("le-self", "(le ?a ?a)", "true", Some(safe_a), false),
        ("eq-self", "(eq ?a ?a)", "true", Some(safe_a), false),
        ("ne-self", "(ne ?a ?a)", "false", Some(safe_a), false),
        ("not-lt", "(lnot (lt ?a ?b))", "(ge ?a ?b)", None, false),
        ("not-le", "(lnot (le ?a ?b))", "(gt ?a ?b)", None, false),
        ("not-gt", "(lnot (gt ?a ?b))", "(le ?a ?b)", None, false),
        ("not-ge", "(lnot (ge ?a ?b))", "(lt ?a ?b)", None, false),
        ("not-eq", "(lnot (eq ?a ?b))", "(ne ?a ?b)", None, false),
        ("not-ne", "(lnot (ne ?a ?b))", "(eq ?a ?b)", None, false),
        // booleans (&& and || evaluate the right side only when needed)
        ("land-true", "(land true ?a)", "?a", None, false),
        ("land-true-r", "(land ?a true)", "?a", None, false),
        ("land-false", "(land false ?a)", "false", None, false),
        ("land-false-r", "(land ?a false)", "false", Some(safe_a), false),
        ("lor-false", "(lor false ?a)", "?a", None, false),
        ("lor-false-r", "(lor ?a false)", "?a", None, false),
        ("lor-true", "(lor true ?a)", "true", None, false),
        ("lor-true-r", "(lor ?a true)", "true", Some(safe_a), false),
        ("land-self", "(land ?a ?a)", "?a", None, false),
        ("lor-self", "(lor ?a ?a)", "?a", None, false),
        ("comm-land", "(land ?a ?b)", "(land ?b ?a)", Some(both_safe), false),
        ("comm-lor", "(lor ?a ?b)", "(lor ?b ?a)", Some(both_safe), false),
        ("lnot-lnot", "(lnot (lnot ?a))", "?a", None, false),
        ("beq-true", "(beq ?a true)", "?a", None, false),
        ("beq-false", "(beq ?a false)", "(lnot ?a)", None, false),
        ("bne-false", "(bne ?a false)", "?a", None, false),
        ("bne-true", "(bne ?a true)", "(lnot ?a)", None, false),
        ("beq-self", "(beq ?a ?a)", "true", Some(safe_a), false),
        // selection
        ("sel-true", "(sel true ?a ?b)", "?a", None, false),
        ("sel-false", "(sel false ?a ?b)", "?b", None, false),
        ("sel-same", "(sel ?a ?b ?b)", "?b", Some(safe_a), false),
        ("sel-not", "(sel (lnot ?a) ?b ?c)", "(sel ?a ?c ?b)", None, false),
        ("sel-as-cond", "(sel ?a true false)", "?a", None, false),
        ("sel-as-not", "(sel ?a false true)", "(lnot ?a)", None, false),
        ("sel-land", "(sel ?a ?b false)", "(land ?a ?b)", None, false),
        ("sel-lor", "(sel ?a true ?b)", "(lor ?a ?b)", None, false),
    ];
    table.iter().map(|(name, l, r, cond, ring)| Rule { name, lhs: parse_pat(l), rhs: parse_pat(r), cond: *cond, ring: *ring }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(g: &mut EGraph, lo: i64, hi: i64, safe: bool) -> Id {
        g.add_atom(AtomInfo { boolean: false, lo, hi, safe, pure: true, cost: 1 })
    }
    fn n(g: &mut EGraph, op: Op, kids: &[Id]) -> Id {
        g.add(ENode::new(op, kids))
    }
    fn lit(g: &mut EGraph, v: i32) -> Id {
        g.add(ENode::leaf(Op::Int(v)))
    }
    fn best_cost(g: &EGraph, c: Id) -> u32 {
        g.extract()[g.find(c) as usize].unwrap().0
    }

    #[test]
    fn reassociates_constants_and_cancels() {
        let rules = rules();
        let mut g = EGraph::new(false);
        let x = atom(&mut g, I32_MIN, I32_MAX, true);
        let one = lit(&mut g, 1);
        let two = lit(&mut g, 2);
        let a = n(&mut g, Op::Add, &[x, one]);
        let b = n(&mut g, Op::Add, &[a, two]); // (x + 1) + 2
        let c = n(&mut g, Op::Sub, &[b, x]); // ... - x
        g.saturate(&rules, 2000, 8);
        assert_eq!(g.data(c).constant(), Some(3));
        let three = lit(&mut g, 3);
        g.rebuild();
        assert_eq!(g.find(c), g.find(three));
    }

    #[test]
    fn dropping_needs_a_panic_free_operand() {
        let rules = rules();
        let mut g = EGraph::new(false);
        let risky = atom(&mut g, I32_MIN, I32_MAX, false);
        let zero = lit(&mut g, 0);
        let m = n(&mut g, Op::Mul, &[risky, zero]);
        let s = n(&mut g, Op::Sub, &[risky, risky]);
        g.saturate(&rules, 2000, 8);
        assert_ne!(g.find(m), g.find(zero), "x * 0 must keep a possibly panicking x");
        assert_ne!(g.find(s), g.find(zero));
    }

    #[test]
    fn ranges_decide_saturations_and_comparisons() {
        let rules = rules();
        let mut g = EGraph::new(false);
        let hp = atom(&mut g, 0, 100, true);
        let ten = lit(&mut g, 10);
        let sub = n(&mut g, Op::Sub, &[hp, ten]); // [-10, 90]
        let sat = n(&mut g, Op::Clamp(0, 100), &[sub]);
        let zero = lit(&mut g, 0);
        let ge = n(&mut g, Op::Ge, &[hp, zero]);
        let k = lit(&mut g, 8);
        let rem = n(&mut g, Op::Rem, &[hp, k]);
        g.saturate(&rules, 2000, 8);
        assert_eq!(g.data(ge).constant(), Some(1));
        // only the lower bound can be crossed: max(hp - 10, 0)
        let best = g.extract();
        let (_, i) = best[g.find(sat) as usize].unwrap();
        assert_eq!(g.node(i).op, Op::Max);
        let (_, i) = best[g.find(rem) as usize].unwrap();
        assert_eq!(g.node(i).op, Op::And);
        assert!(best_cost(&g, rem) < 10);
    }

    #[test]
    fn checked_mode_keeps_overflow_traps() {
        let rules = rules();
        let mut g = EGraph::new(true);
        let x = atom(&mut g, I32_MIN, I32_MAX, true);
        let big = lit(&mut g, i32::MAX);
        let a = n(&mut g, Op::Add, &[x, big]);
        let d = n(&mut g, Op::Sub, &[a, big]); // (x + MAX) - MAX traps for x > 0
        let one = lit(&mut g, 1);
        let wrap = n(&mut g, Op::Add, &[big, one]); // MAX + 1 traps: never a constant
        g.saturate(&rules, 2000, 8);
        assert_ne!(g.find(d), g.find(x));
        assert!(!g.data(a).safe);
        assert!(!g.data(wrap).safe);
        let min = lit(&mut g, i32::MIN);
        g.rebuild();
        assert_ne!(g.find(wrap), g.find(min));
        // unchecked arithmetic wraps: MAX + 1 is MIN
        let mut u = EGraph::new(false);
        let (b2, o2) = (lit(&mut u, i32::MAX), lit(&mut u, 1));
        let w2 = n(&mut u, Op::Add, &[b2, o2]);
        u.saturate(&rules, 2000, 8);
        assert_eq!(u.data(w2).constant(), Some(I32_MIN));
    }

    #[test]
    fn side_effects_keep_their_place() {
        let rules = rules();
        let mut g = EGraph::new(false);
        let call = g.add_atom(AtomInfo { boolean: false, lo: I32_MIN, hi: I32_MAX, safe: false, pure: false, cost: 5 });
        let x = atom(&mut g, 0, 10, true);
        let a = n(&mut g, Op::Add, &[x, call]); // x + F(): x is read before F runs
        let swapped = n(&mut g, Op::Add, &[call, x]);
        let zero = lit(&mut g, 0);
        let m = n(&mut g, Op::Mul, &[call, zero]);
        g.saturate(&rules, 2000, 8);
        assert_ne!(g.find(a), g.find(swapped));
        assert_ne!(g.find(m), g.find(zero));
        let f = g.add(ENode::leaf(Op::Bool(false)));
        let cb = g.add_atom(AtomInfo { boolean: true, lo: 0, hi: 1, safe: false, pure: false, cost: 5 });
        let and = n(&mut g, Op::LAnd, &[f, cb]); // false && F() never calls F
        g.saturate(&rules, 2000, 8);
        assert_eq!(g.find(and), g.find(f));
    }

    #[test]
    fn short_circuit_keeps_the_right_side_lazy() {
        let rules = rules();
        let mut g = EGraph::new(false);
        let risky = g.add_atom(AtomInfo { boolean: true, lo: 0, hi: 1, safe: false, pure: true, cost: 1 });
        let f = g.add(ENode::leaf(Op::Bool(false)));
        let t = g.add(ENode::leaf(Op::Bool(true)));
        let a = n(&mut g, Op::LAnd, &[f, risky]); // false && risky == false
        let b = n(&mut g, Op::LAnd, &[risky, f]); // risky && false: risky still runs
        let c = n(&mut g, Op::LOr, &[risky, t]);
        g.saturate(&rules, 2000, 8);
        assert_eq!(g.find(a), g.find(f));
        assert_ne!(g.find(b), g.find(f));
        assert_ne!(g.find(c), g.find(t));
    }
}
