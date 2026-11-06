//! Typed high-level IR. All names are resolved to ids.

use crate::ast::{BinOp, ParamMode, UnOp};
use crate::diag::Span;
use std::collections::{BTreeMap, HashMap};

pub type StructId = usize;
pub type ClassId = usize;
pub type EnumId = usize;
pub type FuncId = usize;
pub type LocalId = usize;
pub type FieldId = usize;
pub type ConstId = usize;
pub type DelegateId = usize;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    Void,
    Bool,
    Int,
    Float,
    Str,
    /// a Unicode code point (C#'s char; equal to it for every character of the BMP)
    Char,
    Struct(StructId),
    Enum(EnumId),
    /// Entity handle that is guaranteed live for the current system.
    Handle(ClassId),
    /// Nullable (possibly stale) entity handle.
    NHandle(ClassId),
    Null,
    List(Box<Ty>),
    /// Dictionary<K, V>: stored like a List<KeyValuePair<K, V>> plus a hash index
    Dict(Box<Ty>, Box<Ty>),
    /// T[]: a list whose length is fixed when it is created (same ownership rules as List<T>)
    Array(Box<Ty>),
    /// T[,] (rank 2) / T[,,] (rank 3): a fixed-size array stored like T[] with every element in
    /// row-major order (C#'s order) and the lengths of its dimensions kept with it
    MdArray(Box<Ty>, u8),
    /// a delegate (`Func<int, int>`, `Action<string>`, `delegate int Op(int a, int b);`): a value
    /// that holds one of the program's lambdas or methods with the values it captured
    Delegate(DelegateId),
    Error,
}

impl Ty {
    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }
    pub fn is_handle(&self) -> bool {
        matches!(self, Ty::Handle(_) | Ty::NHandle(_))
    }
    pub fn class_of(&self) -> Option<ClassId> {
        match self {
            Ty::Handle(c) | Ty::NHandle(c) => Some(*c),
            _ => None,
        }
    }
    /// an owning container (List or Dictionary): same ownership, regions and moves
    pub fn is_list(&self) -> bool {
        matches!(self, Ty::List(_) | Ty::Dict(..) | Ty::Array(_) | Ty::MdArray(..))
    }
    /// the number of dimensions of an array type (1 for T[], 0 for anything else)
    pub fn rank(&self) -> usize {
        match self {
            Ty::Array(_) => 1,
            Ty::MdArray(_, r) => *r as usize,
            _ => 0,
        }
    }
    pub fn is_dict(&self) -> bool {
        matches!(self, Ty::Dict(..))
    }
}

impl Program {
    /// the KeyValuePair<K, V> struct of a dictionary type
    pub fn kvp_of(&self, t: &Ty) -> Option<StructId> {
        match t {
            Ty::Dict(k, v) => self.kvp.get(&((**k).clone(), (**v).clone())).copied(),
            _ => None,
        }
    }
    /// element type when a container is seen as a list (a dictionary is a list of key/value pairs)
    pub fn elem_of(&self, t: &Ty) -> Option<Ty> {
        match t {
            Ty::List(e) | Ty::Array(e) | Ty::MdArray(e, _) => Some((**e).clone()),
            Ty::Dict(..) => self.kvp_of(t).map(Ty::Struct),
            _ => None,
        }
    }
    /// does a value of this type hold entity references (directly, in struct fields, list elements
    /// or dictionary keys and values)?
    pub fn has_handle(&self, t: &Ty) -> bool {
        self.has_handle_d(t, 0)
    }
    fn has_handle_d(&self, t: &Ty, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match t {
            Ty::Handle(_) | Ty::NHandle(_) => true,
            Ty::Struct(s) => self.structs[*s].fields.iter().any(|f| self.has_handle_d(&f.ty, depth + 1)),
            Ty::List(e) | Ty::Array(e) => self.has_handle_d(e, depth + 1),
            Ty::Dict(k, v) => self.has_handle_d(k, depth + 1) || self.has_handle_d(v, depth + 1),
            _ => false,
        }
    }
    /// does a value of this type contain strings (directly, in struct fields or list elements)?
    pub fn contains_str(&self, t: &Ty) -> bool {
        self.contains_str_d(t, 0)
    }
    fn contains_str_d(&self, t: &Ty, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match t {
            Ty::Str => true,
            Ty::Struct(s) => self.structs[*s].fields.iter().any(|f| self.contains_str_d(&f.ty, depth + 1)),
            // a delegate holds the strings the functions it can hold captured (and the boxes of the
            // delegates they captured, which live where strings live)
            Ty::Delegate(d) => self.delegates[*d].funcs.iter().any(|&f| {
                (0..self.caps_of(f)).any(|j| {
                    let t = &self.funcs[f].locals[self.funcs[f].params[j]].ty;
                    self.boxed_cap(t) || self.contains_str_d(t, depth + 1)
                })
            }),
            Ty::List(e) | Ty::Array(e) | Ty::MdArray(e, _) => self.contains_str_d(e, depth + 1),
            Ty::Dict(k, v) => self.contains_str_d(k, depth + 1) || self.contains_str_d(v, depth + 1),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct FieldAttrs {
    pub range: Option<(f64, f64)>,
    pub precision: Option<f64>,
    pub replicated: Option<bool>,
    pub spatial: bool,
    pub component: Option<String>,
    /// `[Persistent]`: kept in the store under the entity's key across runs (docs/05 §5.7.11)
    pub persistent: bool,
    /// `[PersistentKey]`: the string field that names the entity in the store
    pub persistent_key: bool,
}

#[derive(Clone, Debug)]
pub struct SField {
    pub name: String,
    pub ty: Ty,
    pub init: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct StructDef {
    pub name: String,
    pub span: Span,
    pub fields: Vec<SField>,
    pub methods: HashMap<String, FuncId>,
    /// the second and later methods of a name with other parameter types (overloads): `methods`
    /// keeps the first one declared
    pub overloads: Vec<FuncId>,
    pub ctors: Vec<FuncId>,
    pub ops: Vec<FuncId>,
}

#[derive(Clone, Debug)]
pub struct EField {
    pub name: String,
    pub class: ClassId,
    /// declared `mut`: other fields are set once (initializer or Spawn) and never change
    pub is_mut: bool,
    pub ty: Ty,
    pub init: Option<Expr>,
    pub attrs: FieldAttrs,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct ClassDef {
    pub name: String,
    pub span: Span,
    pub base: Option<ClassId>,
    pub is_entity: bool,
    pub is_static: bool,
    pub is_abstract: bool,
    /// `interface I`: no fields, only method signatures and default implementations; a handle of
    /// it holds any entity class that implements it (dispatched per archetype, like an abstract
    /// base class)
    pub is_interface: bool,
    /// interfaces this class implements (or, for an interface, extends)
    pub ifaces: Vec<ClassId>,
    /// `component C { ... }`: fields and methods an entity gains with `Add(new C { ... })` and
    /// loses with `Remove<C>()` (abstract: its rows live in the tables of the combinations below)
    pub is_component: bool,
    /// a combination archetype (made by `components::expand`): `combo_of` is the concrete class and
    /// `comps` the components attached (declaration order). Its table holds the class's columns and
    /// the components' columns; the entity moves between the tables of its class when it gains or
    /// loses a component (docs/04 §4.4.1)
    pub combo_of: Option<ClassId>,
    pub comps: Vec<ClassId>,
    pub own_fields: Vec<FieldId>,
    pub methods: HashMap<String, FuncId>,
    /// the second and later methods of a name with other parameter types (overloads): `methods`
    /// keeps the first one declared
    pub overloads: Vec<FuncId>,
    /// Declaration order index among entity classes (for deterministic ordering).
    pub order: usize,
}

/// A delegate type. `Func<...>`/`Action<...>` are the same type wherever they have the same
/// parameter and return types; each `delegate R Name(...)` declaration is a type of its own.
#[derive(Clone, Debug)]
pub struct DelegateDef {
    /// the declared name (None: `Func<...>` or `Action<...>`)
    pub name: Option<String>,
    pub params: Vec<Ty>,
    pub ret: Ty,
    /// the functions a value of this type can hold (lambdas, method groups) in the order they
    /// were made; a value's function number is its index here plus one (0: the default value,
    /// which does nothing and returns the default of the return type)
    pub funcs: Vec<FuncId>,
    /// the static function a call through a value of this type compiles to: it picks the held
    /// function by number and passes the captured values before the arguments
    pub invoke: FuncId,
    pub span: crate::diag::Span,
}

/// What a compiler-made function is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Synth {
    /// a lambda (or method group) stored as a delegate: its first `caps` parameters are the values
    /// it captured where it was made (in function `of`)
    Lambda { dg: DelegateId, caps: usize, of: Option<FuncId> },
    /// the call through a delegate value (`DelegateDef::invoke`)
    Invoke(DelegateId),
}

#[derive(Clone, Debug)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<(String, i64)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConstVal {
    Int(i32),
    Float(f32),
    Bool(bool),
    Struct(StructId, Vec<ConstVal>),
    Enum(EnumId, i64),
    Null,
    Str(String),
    /// a delegate value that captures nothing: function number `.1` of delegate type `.0` (0: the
    /// default value)
    Fn(DelegateId, usize),
}

#[derive(Clone, Debug)]
pub struct ConstDef {
    pub name: String,
    pub ty: Ty,
    pub value: Option<ConstVal>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuncKind {
    /// static method of a (static or entity) class
    Static(ClassId),
    StructMethod(StructId),
    StructStatic(StructId),
    StructCtor(StructId),
    StructOp(StructId),
    EntityMethod(ClassId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ctx {
    Server,
    Client,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    Normal,
    Param(ParamMode),
    /// `foreach (var x in Query<T>())` loop variable: compiled as an (archetype,row) cursor.
    Cursor(ClassId),
    /// `foreach (var x in list)` loop variable (read-only copy).
    ListElem,
}

#[derive(Clone, Debug)]
pub struct Local {
    pub name: String,
    pub ty: Ty,
    pub kind: LocalKind,
    pub span: Span,
    /// `int in 0..100 x`: every write saturates into [lo, hi]
    pub range: Option<(f64, f64)>,
    /// declared `mut` (bindings are immutable by default)
    pub is_mut: bool,
    /// changed somewhere after its declaration (assignment, ++/--, ref/out, mutating call)
    pub written: bool,
    /// a local `const`
    pub is_const: bool,
}

#[derive(Clone, Debug)]
pub struct Func {
    pub name: String,
    pub kind: FuncKind,
    pub params: Vec<LocalId>,
    pub ret: Ty,
    pub locals: Vec<Local>,
    pub body: Block,
    pub ctx: Ctx,
    pub pure_attr: bool,
    pub is_property: bool,
    pub is_override: bool,
    pub is_abstract: bool,
    pub span: Span,
    /// set by the type checker for struct methods: does the method write `this`?
    pub writes_self: bool,
    /// `async` entity method: a coroutine whose frame lives in hidden entity fields
    pub co: Option<CoInfo>,
    /// `static extern T F(...);`: a C function provided by the host program
    pub extern_name: Option<String>,
    /// `int in 0..100 F()`: the returned value saturates into [lo, hi]
    pub ret_range: Option<(f64, f64)>,
    /// declared `[Recursive]` (checked against the call graph)
    pub recursive_attr: bool,
    /// span of the name (diagnostics)
    pub name_span: Span,
    /// a function the compiler made (a lambda stored as a delegate, a delegate's call)
    pub synth: Option<Synth>,
}

/// Coroutine frame of an async entity method (all of it is ECS state).
#[derive(Clone, Debug)]
pub struct CoInfo {
    /// resume point: 0 = not started, k = after await #k, -1 = finished
    pub state: FieldId,
    /// remaining time of the current `await Wait(t)`
    pub timer: FieldId,
    pub points: usize,
    /// locals that live across an await, moved into entity fields
    pub hoisted: Vec<(LocalId, FieldId)>,
}

#[derive(Clone, Debug)]
pub enum AwaitKind {
    NextTick,
    Wait(Expr),
    Until(Expr),
    /// await M(args): another async method of this entity (a SelfCall whose arguments are hidden
    /// fields of the caller, set when the await starts), resumed every tick until it finishes
    Call(Expr),
    /// await Race(a, b, ...) (first arm to finish wins, the others are cancelled; `result` gets the
    /// winner's index) / await Sync(a, b, ...) (every arm runs until all have finished; `done` is
    /// the hidden bit set of finished arms)
    Group { race: bool, arms: Vec<AwaitArm>, done: Option<Expr>, result: Option<Expr>, rush: Option<Rush> },
}

/// await Rush(...): like Race, but the unfinished method arms keep running in the background:
/// `bg` is the caller's hidden bit set of background arms, `bits[j]` arm j's bit (0: not a method)
#[derive(Clone, Debug)]
pub struct Rush {
    pub bg: Expr,
    pub bits: Vec<u32>,
}

#[derive(Clone, Debug)]
pub enum AwaitArm {
    /// an async method of this entity, as in AwaitKind::Call
    Call(Expr),
    /// Wait(seconds) or NextTick() (seconds 0: one tick); `timer` is a hidden field of the caller
    Wait { secs: Expr, timer: Expr },
    Until(Expr),
}

impl AwaitKind {
    pub fn exprs(&self) -> Vec<&Expr> {
        match self {
            AwaitKind::Wait(e) | AwaitKind::Until(e) | AwaitKind::Call(e) => vec![e],
            AwaitKind::NextTick => vec![],
            AwaitKind::Group { arms, done, result, rush, .. } => {
                let mut v: Vec<&Expr> = done.iter().chain(result.iter()).chain(rush.iter().map(|r| &r.bg)).collect();
                for a in arms {
                    match a {
                        AwaitArm::Call(e) | AwaitArm::Until(e) => v.push(e),
                        AwaitArm::Wait { secs, timer } => {
                            v.push(secs);
                            v.push(timer);
                        }
                    }
                }
                v
            }
        }
    }
    pub fn exprs_mut(&mut self) -> Vec<&mut Expr> {
        match self {
            AwaitKind::Wait(e) | AwaitKind::Until(e) | AwaitKind::Call(e) => vec![e],
            AwaitKind::NextTick => vec![],
            AwaitKind::Group { arms, done, result, rush, .. } => {
                let mut v: Vec<&mut Expr> = done.iter_mut().chain(result.iter_mut()).chain(rush.iter_mut().map(|r| &mut r.bg)).collect();
                for a in arms {
                    match a {
                        AwaitArm::Call(e) | AwaitArm::Until(e) => v.push(e),
                        AwaitArm::Wait { secs, timer } => {
                            v.push(secs);
                            v.push(timer);
                        }
                    }
                }
                v
            }
        }
    }
}

impl Func {
    pub fn param_mode(&self, i: usize) -> ParamMode {
        match self.locals[self.params[i]].kind {
            LocalKind::Param(m) => m,
            _ => ParamMode::Value,
        }
    }
    pub fn owner_name<'a>(&self, p: &'a Program) -> &'a str {
        match self.kind {
            FuncKind::Static(c) | FuncKind::EntityMethod(c) => &p.classes[c].name,
            FuncKind::StructMethod(s) | FuncKind::StructStatic(s) | FuncKind::StructCtor(s) | FuncKind::StructOp(s) => {
                &p.structs[s].name
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Block {
    pub stmts: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Let { local: LocalId, init: Option<Expr> },
    Expr(Expr),
    If { cond: Expr, then: Block, els: Option<Block> },
    /// while/for. `step` runs after body and on `continue`.
    Loop { cond: Option<Expr>, step: Vec<Expr>, body: Block, do_while: bool },
    /// `foreach (var x in Query<T, C1, C2>())`: the archetypes of `class` that have every
    /// component in `with`; `alias` are the further cursors of `foreach (var (x, c1) in ...)` (the
    /// same row seen as each component)
    ForQuery { var: LocalId, class: ClassId, with: Vec<ClassId>, alias: Vec<LocalId>, body: Block },
    ForList { var: LocalId, idx: LocalId, list: Expr, body: Block },
    Return(Option<Expr>, Span),
    Break,
    Continue,
    Block(Block),
    Atomic { body: Block, id: usize, span: Span },
    Fail(Span),
    /// `await` in a coroutine: suspend until the next tick / for a time / until a condition holds.
    /// `state` and `timer` are the coroutine's frame fields (self fields).
    Await { point: usize, kind: AwaitKind, state: Expr, timer: Expr },
    /// `switch` statement lowered to a condition chain. `tmp` holds the scrutinee when it is not
    /// trivially re-evaluable (then the conditions read `Local(tmp)`); `break` exits the switch.
    Switch { tmp: Option<LocalId>, scrut: Expr, sections: Vec<(Expr, Block)>, default: Option<Block> },
}

impl Stmt {
    /// nested statement blocks, for walkers that only need to recurse
    pub fn sub_blocks(&self) -> Vec<&Block> {
        match self {
            Stmt::If { then, els, .. } => {
                let mut v = vec![then];
                v.extend(els.iter());
                v
            }
            Stmt::Loop { body, .. } | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => vec![body],
            Stmt::Switch { sections, default, .. } => sections.iter().map(|(_, b)| b).chain(default.iter()).collect(),
            _ => vec![],
        }
    }
    /// expressions evaluated by an await statement (frame fields first)
    pub fn await_exprs(&self) -> Vec<&Expr> {
        match self {
            Stmt::Await { kind, state, timer, .. } => {
                let mut v = vec![state, timer];
                v.extend(kind.exprs());
                v
            }
            _ => vec![],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// a range type's contract: args `[x, lo, hi]` (float literals); NaN goes to lo, structs member by member
    Saturate,
    MathSqrt,
    MathAbs,
    MathMin,
    MathMax,
    MathClamp,
    MathSin,
    MathCos,
    MathFloor,
    MathAtan2,
    MathTan,
    MathAtan,
    MathAsin,
    MathAcos,
    MathExp,
    MathLog,
    MathLog10,
    MathLog2,
    MathPow,
    MathRound,
    MathCeiling,
    MathTruncate,
    MathSign,
    DrawSprite,
    /// Draw.Text(x, y, text): a text label in the client's draw list
    DrawText,
    /// Net.View(x, y, radius) / Net.ClearView(): what this client looks at (interest management)
    NetView,
    NetClearView,
    Print,
    RandomFloat,
    RandomInt,
    TimeTick,
    TimeDt,
    // strings (results are frame strings; see runtime/sijo_str.h)
    StrLen,
    StrSub,
    StrSubFrom,
    StrContains,
    StrStarts,
    StrEnds,
    StrIndexOf,
    StrUpper,
    StrLower,
    StrTrim,
    StrReplace,
    StrIsEmpty,
    StrJoin,
    StrCompare,
    /// s[i]: the character at index i (code points; O(1) for ASCII strings)
    StrCharAt,
    /// foreach over a string: byte length, the character at a byte offset, its UTF-8 length
    StrByteLen,
    StrCharAtByte,
    StrCharBytes,
    /// new string(c, n)
    StrRepeat,
    /// s.Split(sep): List<string>
    StrSplit,
    /// s.LastIndexOf(t)
    StrLastIndexOf,
    /// s.PadLeft(n) / s.PadRight(n) (with an optional pad character)
    StrPadLeft,
    StrPadRight,
    /// char.IsDigit ... (ASCII digits; Unicode letters, white space and case of the common scripts)
    CharIsDigit,
    CharIsLetter,
    CharIsLetterOrDigit,
    CharIsWhiteSpace,
    CharIsUpper,
    CharIsLower,
    CharIsPunctuation,
    CharToUpper,
    CharToLower,
    /// a.CompareTo(b) for int/float/bool/enum: -1, 0 or 1 (float: NaN first, as .NET); with
    /// site 1, float Equals (NaN equals NaN)
    CompareVal,
    /// x.ToString() / string conversion in concatenation
    ToStr,
    ParseInt,
    ParseFloat,
    /// int.TryParse(s, out v) / float.TryParse(s, out v): args = [s, Local(v)]
    TryParseInt,
    TryParseFloat,
    /// a.GetLength(d): args = [array, dimension]; a dimension outside 0..Rank panics (C#'s
    /// IndexOutOfRangeException), a constant one is checked by the type checker
    ArrayGetLength,
}

impl Builtin {
    pub fn is_client_only(self) -> bool {
        matches!(self, Builtin::DrawSprite | Builtin::DrawText | Builtin::NetView | Builtin::NetClearView)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListOp {
    Add,
    Clear,
    RemoveAt,
    Contains,
    /// index of the first equal element or -1
    IndexOf,
    /// Insert(index, value): 0 <= index <= Count
    Insert,
    /// Remove(value): removes the first equal element, keeps the order; returns whether one was found
    Remove,
    Reverse,
    /// Sort(): stable, ascending in C#'s CompareTo order (int, float, bool, enum, string)
    Sort,
    /// Array.Fill(xs, v): every element becomes v
    Fill,
}

impl ListOp {
    /// whether the call changes the list (everything but the lookups)
    pub fn mutates(self) -> bool {
        !matches!(self, ListOp::Contains | ListOp::IndexOf)
    }
}

/// operations of ListLambda
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LamOp {
    /// Sort((a, b) => int): stable
    Sort,
    Find,
    FindLast,
    FindIndex,
    FindLastIndex,
    /// Exists / Any
    Exists,
    /// TrueForAll / All
    All,
    RemoveAll,
    Count,
    Sum,
    Max,
    Min,
    First,
    Last,
    FirstOrDefault,
    LastOrDefault,
    Where,
    Select,
    OrderBy,
    OrderByDesc,
}

impl LamOp {
    /// produces a new list (an allocation site)
    pub fn makes_list(self) -> bool {
        matches!(self, LamOp::Where | LamOp::Select | LamOp::OrderBy | LamOp::OrderByDesc)
    }
    /// changes the list it is called on
    pub fn mutates(self) -> bool {
        matches!(self, LamOp::Sort | LamOp::RemoveAll)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictOp {
    ContainsKey,
    Remove,
    Clear,
    /// GetValueOrDefault(k) / GetValueOrDefault(k, fallback): args = [key, fallback]
    GetOr,
    /// TryGetValue(k, out v): args = [key, Local(v)]
    TryGet,
    /// Add(k, v): insert, panics if the key exists (C#'s ArgumentException)
    Add,
}

impl DictOp {
    pub fn mutates(self) -> bool {
        matches!(self, DictOp::Remove | DictOp::Clear | DictOp::Add)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dispatch {
    /// plain static function / struct method / ctor
    Static,
    /// entity method on `this` (statically dispatched per archetype)
    SelfCall,
    /// base.M() on self
    BaseCall,
    /// read-only entity method on another entity (synchronous, reads snapshot)
    RemoteRead,
    /// effectful entity method on another entity (deferred message)
    RemoteDeferred,
    /// effectful entity method called from client code: compiled into a client->server RPC
    Rpc,
}

#[derive(Clone, Debug)]
pub struct HArg {
    pub mode: ParamMode,
    pub expr: Expr,
}

#[derive(Clone, Debug)]
pub enum IPart {
    Text(String),
    Expr(Expr, Option<String>),
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub ty: Ty,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    Str(String),
    Interp(Vec<IPart>),
    Local(LocalId),
    /// `this` inside an entity method (a live handle)
    SelfRef,
    /// `this` inside a struct method (place)
    StructSelf,
    Field { base: Box<Expr>, sid: StructId, idx: usize },
    EField { base: Box<Expr>, field: FieldId },
    Const(ConstId),
    EnumVal(EnumId, i64),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Assign { op: Option<BinOp>, target: Box<Expr>, value: Box<Expr> },
    IncDec { target: Box<Expr>, inc: bool, prefix: bool },
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Cast(Box<Expr>),
    Call { func: FuncId, recv: Option<Box<Expr>>, args: Vec<HArg>, dispatch: Dispatch },
    StructLit { sid: StructId, ctor: Option<FuncId>, args: Vec<HArg>, inits: Vec<(usize, Expr)> },
    /// `Spawn(new T { ... }, new C { ... })`: `comps` are the components the entity starts with
    /// (sorted; after `components::expand` `class` is the combination archetype that has them and
    /// `comps` is empty); `inits` may name their fields
    Spawn { class: ClassId, inits: Vec<(FieldId, Expr)>, comps: Vec<ClassId> },
    Destroy(Option<Box<Expr>>),
    /// `e.Add(new C { F = x })` / `Add(...)` on this: a structural change applied at the end of the
    /// system (deferred, in command order); adding a component the entity has replaces its values
    CompAdd { target: Box<Expr>, comp: ClassId, inits: Vec<(FieldId, Expr)> },
    /// `e.Remove<C>()`: deferred like CompAdd; removing a component the entity lacks does nothing
    CompRemove { target: Box<Expr>, comp: ClassId },
    /// a delegate value: function number `num` of `delegates[dg]` (`funcs[num - 1]`; 0 is the
    /// default value, which holds nothing) with the captured values `caps` (evaluated here, in order)
    Closure { dg: DelegateId, num: usize, caps: Vec<Expr> },
    /// the function number a delegate value holds (0: the default value)
    DgFn(Box<Expr>),
    /// captured value `cap` of a delegate value that holds `delegates[dg].funcs[idx]` (only in a
    /// delegate's invoke function, behind the check of `DgFn`)
    DgCap { d: Box<Expr>, dg: DelegateId, idx: usize, cap: usize },
    Count(ClassId),
    Is { expr: Box<Expr>, class: ClassId, bind: Option<LocalId> },
    ListNew { site: usize },
    /// new T[n] (elements: the default value) or new T[] { a, b } (len None). For new T[a, b] or
    /// new T[,] { { x, y }, { z, w } }, `dims` holds the length of every dimension of the
    /// multi-dimensional array (`len` is None; `elems`, when not empty, lists the elements in
    /// row-major order); it is empty otherwise
    ArrayNew { site: usize, len: Option<Box<Expr>>, elems: Vec<Expr>, dims: Vec<Expr> },
    /// a list/array operation with a lambda, compiled inline at the call site: `params` (locals of
    /// the enclosing function) take the elements, `body` is evaluated per element; `site` is the
    /// allocation of the resulting list (Where / Select / OrderBy)
    ListLambda { list: Box<Expr>, op: LamOp, params: Vec<LocalId>, body: Box<Expr>, site: usize },
    ListClone { list: Box<Expr>, site: usize },
    ListLen(Box<Expr>),
    /// `xs[i]`; on a multi-dimensional array `a[i, j]` the index is an MdIndex (and `checked` is
    /// false: its indexes are checked one by one)
    ListIndex { list: Box<Expr>, index: Box<Expr>, checked: bool },
    /// the indexes of `a[i, j]` / `a[i, j, k]`, only as the `index` of a ListIndex on a
    /// multi-dimensional array: the element's row-major position. Index d is checked against the
    /// length of dimension d unless `checked[d]` was cleared (proven in range)
    MdIndex { idx: Vec<Expr>, checked: Vec<bool> },
    ListCall { list: Box<Expr>, op: ListOp, args: Vec<Expr> },
    Builtin { f: Builtin, args: Vec<Expr>, site: usize },
    /// result of a struct operator call (kept separate for readability in codegen)
    OpCall { func: FuncId, args: Vec<Expr> },
    /// `d[k]` (panics if the key is missing, like C#'s KeyNotFoundException)
    DictGet { dict: Box<Expr>, key: Box<Expr> },
    /// `d[k] = v` (insert or replace); the value of the expression is `v`
    DictSet { dict: Box<Expr>, key: Box<Expr>, value: Box<Expr> },
    /// ContainsKey / Remove / Clear / GetValueOrDefault / TryGetValue (args after the dictionary)
    DictCall { dict: Box<Expr>, op: DictOp, args: Vec<Expr> },
    /// switch expression / `is` pattern: first arm whose condition holds; `default` if none
    /// (no default = the switch is exhaustive; an unmatched value panics, like C#'s SwitchExpressionException)
    Switch { tmp: Option<LocalId>, scrut: Box<Expr>, arms: Vec<(Expr, Expr)>, default: Option<Box<Expr>> },
}

impl Expr {
    pub fn new(kind: ExprKind, ty: Ty, span: Span) -> Expr {
        Expr { kind, ty, span }
    }
    pub fn err(span: Span) -> Expr {
        Expr { kind: ExprKind::Int(0), ty: Ty::Error, span }
    }
    /// set the default arm of a switch expression
    pub fn with_default(mut self, d: Option<Box<Expr>>) -> Expr {
        if let ExprKind::Switch { default, .. } = &mut self.kind {
            *default = d;
        }
        self
    }
}

/// Allocation sites of lists (filled by the type checker, classified by the memory planner).
#[derive(Clone, Debug)]
pub struct AllocSite {
    pub func: FuncId,
    pub elem: Ty,
    pub span: Span,
}

/// A resolved symbol (for IDE features and refactoring).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Sym {
    Struct(StructId),
    Class(ClassId),
    Enum(EnumId),
    SField(StructId, usize),
    Field(FieldId),
    Func(FuncId),
    Local(FuncId, LocalId),
    Const(ConstId),
    Variant(EnumId, usize),
}

/// Where every symbol is defined and referenced in the source.
#[derive(Debug, Default, Clone)]
pub struct SemIndex {
    /// (span of the identifier, symbol) for every definition and reference
    pub refs: Vec<(Span, Sym)>,
    pub defs: HashMap<Sym, Span>,
    /// full extent of each function (header + body)
    pub func_spans: Vec<(FuncId, Span)>,
}

impl SemIndex {
    pub fn at(&self, off: u32) -> Option<(Span, Sym)> {
        self.refs.iter().filter(|(sp, _)| sp.lo <= off && off <= sp.hi).min_by_key(|(sp, _)| sp.hi - sp.lo).copied()
    }
    pub fn occurrences(&self, sym: Sym) -> Vec<Span> {
        let mut v: Vec<Span> = self.refs.iter().filter(|(_, s)| *s == sym).map(|(sp, _)| *sp).collect();
        v.sort_by_key(|s| s.lo);
        v.dedup();
        v
    }
    pub fn func_at(&self, off: u32) -> Option<FuncId> {
        self.func_spans.iter().filter(|(_, sp)| sp.lo <= off && off <= sp.hi).min_by_key(|(_, sp)| sp.hi - sp.lo).map(|(f, _)| *f)
    }
}

#[derive(Debug, Default)]
pub struct Program {
    pub structs: Vec<StructDef>,
    /// interfaces a struct implements (for `where T : I`; a struct value is never an interface)
    pub struct_ifaces: HashMap<StructId, Vec<ClassId>>,
    /// instances of one generic interface (`IContainer<int>`, `IContainer<string>`) -> the first
    /// one, which holds the declaration's symbols in the IDE index
    pub iface_family: HashMap<ClassId, ClassId>,
    /// struct methods that are copies of an interface's default implementation -> that default
    pub default_copies: HashMap<FuncId, FuncId>,
    pub classes: Vec<ClassDef>,
    pub enums: Vec<EnumDef>,
    pub fields: Vec<EField>,
    pub funcs: Vec<Func>,
    pub consts: Vec<ConstDef>,
    pub alloc_sites: Vec<AllocSite>,
    pub game: Option<ClassId>,
    pub game_start: Option<FuncId>,
    pub game_update: Option<FuncId>,
    pub atomic_count: usize,
    pub builtin_sites: usize,
    pub index: SemIndex,
    /// KeyValuePair<K, V> struct per dictionary instantiation (also the entry layout)
    pub kvp: HashMap<(Ty, Ty), StructId>,
    /// methods declared `async`
    pub async_funcs: std::collections::BTreeSet<FuncId>,
    /// async methods other coroutines await: driven by their awaiter, not systems of their own
    pub awaited: std::collections::BTreeSet<FuncId>,
    /// entity fields some code changes after spawn (the others keep their initial value)
    pub written_fields: std::collections::BTreeSet<FieldId>,
    /// generic entity classes: the name as declared (`Pool<T>`) and the instances, each an entity
    /// class of its own (`Pool<int>`, `Pool<SVec2>`), in the order they were first used
    pub generic_classes: Vec<(String, Vec<ClassId>)>,
    /// an instance of a generic entity class -> its index in `generic_classes`
    pub generic_of: HashMap<ClassId, usize>,
    /// `Entity`: the type of `Owner` in component methods, a handle of any entity (only programs
    /// that declare components have it)
    pub entity_root: Option<ClassId>,
    pub delegates: Vec<DelegateDef>,
    /// classes with a `[PersistentKey]`: (class, key field, persistent fields in declaration order,
    /// the hidden flag "loaded from the store" every entity of the class has)
    pub persist: Vec<(ClassId, FieldId, Vec<FieldId>, FieldId)>,
    /// method names some class hierarchy, struct or interface declares with more than one list of
    /// parameter types: calls to them dispatch by name and parameter types (`virtual_target`)
    pub overloaded: std::collections::BTreeSet<String>,
    /// open tables (docs/04 §4.4.1): a class that can reach more combinations of components than
    /// `components::MAX_COMBOS` keeps one table with the columns of every component it can have
    /// (`comps`) and this hidden field, whose bit `i` says the entity has `comps[i]`
    pub open: BTreeMap<ClassId, FieldId>,
}

impl Program {
    /// Is `sub` equal to or derived from `sup`, or does it implement the interface `sup`, or is it
    /// a combination archetype with the component `sup`?
    pub fn is_subclass(&self, sub: ClassId, sup: ClassId) -> bool {
        if Some(sup) == self.entity_root {
            return self.classes[sub].is_entity;
        }
        // the components are the archetype's own (a subclass of an open class has its own)
        if self.classes[sub].comps.contains(&sup) {
            return true;
        }
        let mut c = Some(sub);
        while let Some(x) = c {
            if x == sup || self.classes[x].ifaces.iter().any(|&i| self.extends(i, sup, 0)) {
                return true;
            }
            c = self.classes[x].base;
        }
        false
    }
    /// archetype `a` is an open table that may have component `k`: (presence field, bit)
    pub fn open_bit(&self, a: ClassId, k: ClassId) -> Option<(FieldId, u32)> {
        let f = *self.open.get(&a)?;
        let i = self.classes[a].comps.iter().position(|&x| x == k)?;
        Some((f, i as u32))
    }
    /// the concrete class of an archetype (itself, or the class of a combination)
    pub fn root_class(&self, a: ClassId) -> ClassId {
        self.classes[a].combo_of.unwrap_or(a)
    }
    /// the archetypes of the class of `a`: the class and its combinations with components
    pub fn family(&self, a: ClassId) -> Vec<ClassId> {
        let r = self.root_class(a);
        (0..self.classes.len()).filter(|&x| x == r || self.classes[x].combo_of == Some(r)).collect()
    }
    pub fn is_component(&self, c: ClassId) -> bool {
        self.classes[c].is_component
    }
    /// The method that runs for `f` on archetype `a`: component methods are never virtual (a class
    /// method of the same name must not be chosen for them); class methods resolve per archetype
    pub fn target_of(&self, a: ClassId, f: FuncId) -> FuncId {
        match self.funcs[f].kind {
            FuncKind::EntityMethod(k) if self.classes[k].is_component => f,
            _ => self.virtual_target(a, f),
        }
    }
    /// The method that runs for a call of `f` on an entity of archetype `a` (virtual dispatch): the
    /// method of that name in `a`'s class chain (or the default implementation it uses), and for an
    /// overloaded name the one with `f`'s parameter types
    pub fn virtual_target(&self, a: ClassId, f: FuncId) -> FuncId {
        if self.overloaded.contains(&self.funcs[f].name) {
            self.resolve_sig(a, f).unwrap_or(f)
        } else {
            self.resolve_method(a, &self.funcs[f].name).unwrap_or(f)
        }
    }
    /// the parameter types and modes of a method: what tells overloads apart (ranges, the return
    /// type and `mut` do not)
    pub fn param_sig(&self, f: FuncId) -> Vec<(Ty, LocalKind)> {
        let func = &self.funcs[f];
        func.params.iter().map(|&l| (func.locals[l].ty.clone(), func.locals[l].kind)).collect()
    }
    pub fn same_params(&self, a: FuncId, b: FuncId) -> bool {
        self.param_sig(a) == self.param_sig(b)
    }
    /// the methods named `name` declared directly in class `c` (first declared first)
    pub fn own_methods_named(&self, c: ClassId, name: &str) -> Vec<FuncId> {
        let cd = &self.classes[c];
        cd.methods.get(name).copied().into_iter().chain(cd.overloads.iter().copied().filter(|&f| self.funcs[f].name == name)).collect()
    }
    /// every method an interface (or class) declares itself, overloads included, in declaration order
    pub fn iface_methods(&self, c: ClassId) -> Vec<FuncId> {
        let cd = &self.classes[c];
        let mut v: Vec<FuncId> = cd.methods.values().copied().chain(cd.overloads.iter().copied()).collect();
        v.sort_by_key(|&f| (self.funcs[f].span.lo, f));
        v
    }
    /// every method of a struct, overloads included
    pub fn struct_all_methods(&self, s: StructId) -> Vec<FuncId> {
        let sd = &self.structs[s];
        let mut v: Vec<FuncId> = sd.methods.values().copied().chain(sd.overloads.iter().copied()).collect();
        v.sort();
        v
    }
    /// the methods named `name` of struct `s` (first declared first)
    pub fn struct_methods_named(&self, s: StructId, name: &str) -> Vec<FuncId> {
        let sd = &self.structs[s];
        sd.methods.get(name).copied().into_iter().chain(sd.overloads.iter().copied().filter(|&f| self.funcs[f].name == name)).collect()
    }
    /// The overloads a call `x.name(...)` on class `c` chooses from: the methods of that name in the
    /// class chain (a method hides a base class method with the same parameters: it overrides it),
    /// then the interface methods with other parameters (the default implementations the class
    /// uses, or the signatures of an interface or abstract class)
    pub fn methods_named(&self, c: ClassId, name: &str) -> Vec<FuncId> {
        let mut out: Vec<FuncId> = vec![];
        let mut cur = Some(c);
        while let Some(x) = cur {
            for f in self.own_methods_named(x, name) {
                if !out.iter().any(|&g| self.same_params(f, g)) {
                    out.push(f);
                }
            }
            cur = self.classes[x].base;
        }
        // (an interface's static members belong to the interface: `I.M()`, `T.M()`)
        let ms = self.most_specific(&self.all_ifaces(c), name);
        // of the declarations with the same parameters, the default implementation first
        let mut ordered: Vec<FuncId> = ms.iter().copied().filter(|&f| !self.funcs[f].is_abstract).collect();
        ordered.extend(ms.iter().copied().filter(|&f| self.funcs[f].is_abstract));
        for f in ordered {
            if !out.iter().any(|&g| self.same_params(f, g)) {
                out.push(f);
            }
        }
        out
    }
    /// the method of `c` (class chain, then interfaces) with the name and parameter types of `f`
    pub fn resolve_sig(&self, c: ClassId, f: FuncId) -> Option<FuncId> {
        self.methods_named(c, &self.funcs[f].name).into_iter().find(|&g| self.same_params(f, g))
    }
    /// the archetypes a `Query<class, with...>()` visits
    pub fn query_archetypes(&self, class: ClassId, with: &[ClassId]) -> Vec<ClassId> {
        self.archetypes_of(class).into_iter().filter(|&a| with.iter().all(|&k| self.is_subclass(a, k))).collect()
    }
    /// interface `i` is `sup` or extends it (directly or through other interfaces)
    pub fn extends(&self, i: ClassId, sup: ClassId, depth: usize) -> bool {
        i == sup || (depth < 64 && self.classes[i].ifaces.iter().any(|&j| self.extends(j, sup, depth + 1)))
    }
    /// every interface `c` implements: its own, its base classes', and the ones those extend (in
    /// the order they are written, depth first)
    pub fn all_ifaces(&self, c: ClassId) -> Vec<ClassId> {
        let mut out: Vec<ClassId> = vec![];
        let mut stack: Vec<ClassId> = vec![];
        let mut cur = Some(c);
        while let Some(x) = cur {
            stack.splice(0..0, self.classes[x].ifaces.iter().rev().copied());
            cur = self.classes[x].base;
        }
        while let Some(i) = stack.pop() {
            if !out.contains(&i) {
                out.push(i);
                stack.extend(self.classes[i].ifaces.iter().rev().copied());
            }
        }
        out
    }
    /// All entity fields of a class including inherited ones (base first), then the fields of the
    /// components a combination archetype has.
    pub fn all_fields(&self, c: ClassId) -> Vec<FieldId> {
        self.layout_chain(c).iter().flat_map(|&x| self.classes[x].own_fields.clone()).collect()
    }
    /// the classes whose fields an archetype's table holds: base classes first, then its components
    pub fn layout_chain(&self, c: ClassId) -> Vec<ClassId> {
        let mut chain = vec![];
        let mut cur = Some(c);
        while let Some(x) = cur {
            chain.push(x);
            cur = self.classes[x].base;
        }
        chain.reverse();
        chain.extend(self.classes[c].comps.iter().copied());
        chain
    }
    pub fn find_field(&self, c: ClassId, name: &str) -> Option<FieldId> {
        let mut cur = Some(c);
        while let Some(x) = cur {
            for &f in &self.classes[x].own_fields {
                if self.fields[f].name == name {
                    return Some(f);
                }
            }
            cur = self.classes[x].base;
        }
        None
    }
    /// Resolve an entity method by name with virtual dispatch starting from class `c` (then the
    /// interfaces it implements: the default implementation it uses, or the signature for a call
    /// through an interface handle)
    pub fn resolve_method(&self, c: ClassId, name: &str) -> Option<FuncId> {
        self.resolve_in_chain(c, name).or_else(|| {
            let ms = self.most_specific(&self.all_ifaces(c), name);
            ms.iter().copied().find(|&f| !self.funcs[f].is_abstract).or(ms.first().copied())
        })
    }
    /// The declarations of interface method `name` among the interfaces `ifs` that no other
    /// declaration there overrides (an interface that extends another and declares the method
    /// again overrides it). One declaration with a body is the default implementation; several
    /// with a body (or one with a body beside an unrelated signature) are ambiguous.
    pub fn most_specific(&self, ifs: &[ClassId], name: &str) -> Vec<FuncId> {
        let decls: Vec<(ClassId, FuncId)> = ifs.iter().flat_map(|&i| self.own_methods_named(i, name).into_iter().filter(|&f| !matches!(self.funcs[f].kind, FuncKind::Static(_))).map(move |f| (i, f))).collect();
        // a declaration is overridden by one with the same parameters in an interface that extends its own
        decls.iter().filter(|(i, f)| !decls.iter().any(|(j, g)| j != i && self.extends(*j, *i, 0) && self.same_params(*f, *g))).map(|&(_, f)| f).collect()
    }
    /// `most_specific` among the declarations with the parameter types of `f`
    pub fn most_specific_sig(&self, ifs: &[ClassId], f: FuncId) -> Vec<FuncId> {
        self.most_specific(ifs, &self.funcs[f].name).into_iter().filter(|&g| self.same_params(f, g)).collect()
    }
    /// every interface a struct implements, with the ones those extend
    pub fn struct_all_ifaces(&self, s: StructId) -> Vec<ClassId> {
        let mut out: Vec<ClassId> = vec![];
        let mut stack: Vec<ClassId> = self.struct_ifaces.get(&s).cloned().unwrap_or_default();
        stack.reverse();
        while let Some(i) = stack.pop() {
            if !out.contains(&i) {
                out.push(i);
                stack.extend(self.classes[i].ifaces.iter().rev().copied());
            }
        }
        out
    }
    /// the method `name` of `c` or a base class (not an interface signature)
    pub fn resolve_in_chain(&self, c: ClassId, name: &str) -> Option<FuncId> {
        let mut cur = Some(c);
        while let Some(x) = cur {
            if let Some(&f) = self.classes[x].methods.get(name) {
                return Some(f);
            }
            cur = self.classes[x].base;
        }
        None
    }
    /// Concrete entity classes (archetypes) that are `c` or derive from it.
    pub fn archetypes_of(&self, c: ClassId) -> Vec<ClassId> {
        (0..self.classes.len())
            .filter(|&x| self.classes[x].is_entity && !self.classes[x].is_abstract && self.is_subclass(x, c))
            .collect()
    }
    pub fn entity_classes(&self) -> Vec<ClassId> {
        let mut v: Vec<ClassId> = (0..self.classes.len()).filter(|&x| self.classes[x].is_entity).collect();
        v.sort_by_key(|&c| self.classes[c].order);
        v
    }
    pub fn ty_name(&self, t: &Ty) -> String {
        match t {
            Ty::Void => "void".into(),
            Ty::Bool => "bool".into(),
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Str => "string".into(),
            Ty::Char => "char".into(),
            Ty::Struct(s) => self.structs[*s].name.clone(),
            Ty::Enum(e) => self.enums[*e].name.clone(),
            Ty::Handle(c) => self.classes[*c].name.clone(),
            Ty::NHandle(c) => format!("{}?", self.classes[*c].name),
            Ty::Null => "null".into(),
            Ty::List(t) => format!("List<{}>", self.ty_name(t)),
            // C# writes the outermost array's brackets first: int[][,] is an array of int[,]
            Ty::Array(_) | Ty::MdArray(..) => {
                let mut specs = String::new();
                let mut cur = t;
                while let Ty::Array(e) | Ty::MdArray(e, _) = cur {
                    specs.push_str(&format!("[{}]", ",".repeat(cur.rank() - 1)));
                    cur = e;
                }
                format!("{}{}", self.ty_name(cur), specs)
            }
            Ty::Dict(k, v) => format!("Dictionary<{}, {}>", self.ty_name(k), self.ty_name(v)),
            Ty::Delegate(d) => self.delegate_name(*d),
            Ty::Error => "<error>".into(),
        }
    }
    /// `Func<int, int>`, `Action<string>`, `Action`, or the declared name
    pub fn delegate_name(&self, d: DelegateId) -> String {
        let dd = &self.delegates[d];
        if let Some(n) = &dd.name {
            return n.clone();
        }
        let mut args: Vec<String> = dd.params.iter().map(|t| self.ty_name(t)).collect();
        if dd.ret == Ty::Void {
            return if args.is_empty() { "Action".into() } else { format!("Action<{}>", args.join(", ")) };
        }
        args.push(self.ty_name(&dd.ret));
        format!("Func<{}>", args.join(", "))
    }
    /// the number of captured values of a function a delegate holds (0 for anything else)
    pub fn caps_of(&self, f: FuncId) -> usize {
        match self.funcs[f].synth {
            Some(Synth::Lambda { caps, .. }) => caps,
            _ => 0,
        }
    }
    /// A captured value of this type is kept in a box (bytes in string memory, collected like a
    /// string) instead of inside the delegate value: a delegate may capture delegates of its own
    /// type (`x => g(f(x))`), which no fixed-size value can contain.
    pub fn boxed_cap(&self, t: &Ty) -> bool {
        self.has_delegate(t)
    }
    /// does a value of this type hold a delegate (directly, in struct fields or collections)?
    pub fn has_delegate(&self, t: &Ty) -> bool {
        self.has_delegate_d(t, 0)
    }
    fn has_delegate_d(&self, t: &Ty, depth: usize) -> bool {
        if depth > 64 {
            return false;
        }
        match t {
            Ty::Delegate(_) => true,
            Ty::Struct(s) => self.structs[*s].fields.iter().any(|f| self.has_delegate_d(&f.ty, depth + 1)),
            Ty::List(e) | Ty::Array(e) | Ty::MdArray(e, _) => self.has_delegate_d(e, depth + 1),
            Ty::Dict(k, v) => self.has_delegate_d(k, depth + 1) || self.has_delegate_d(v, depth + 1),
            _ => false,
        }
    }
    pub fn field_label(&self, f: FieldId) -> String {
        format!("{}.{}", self.classes[self.fields[f].class].name, self.fields[f].name)
    }
    /// a field as written in the source: `Pool<T>.Items` for the fields of every instance of the
    /// generic class `Pool<T>` (they share the declaration)
    pub fn source_field_label(&self, f: FieldId) -> String {
        match self.generic_of.get(&self.fields[f].class) {
            Some(&g) => format!("{}.{}", self.generic_classes[g].0, self.fields[f].name),
            None => self.field_label(f),
        }
    }
    pub fn func_label(&self, f: FuncId) -> String {
        format!("{}.{}", self.funcs[f].owner_name(self), self.funcs[f].name)
    }
}
