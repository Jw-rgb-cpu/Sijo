//! Untyped syntax tree produced by the parser.

use crate::diag::Span;

#[derive(Clone, Debug, Default)]
pub struct File {
    pub items: Vec<TypeDecl>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeclKind {
    Struct,
    Class,
    Enum,
    /// `interface I { void M(int x); bool Done() => M2() > 0; }`: method signatures (and default
    /// implementations) that entity classes (and structs, for generic constraints) implement;
    /// `interface I<T>` is instantiated once per distinct type arguments
    Interface,
    /// `component Burning { ... }`: data and methods that entities gain and lose at run time
    /// (`e.Add(new Burning { ... })`, `e.Remove<Burning>()`; docs/04 §4.4.1)
    Component,
    /// `delegate int Op(int a, int b);`: a delegate type (its one member is the signature)
    Delegate,
}

#[derive(Clone, Debug, Default)]
pub struct Mods {
    pub is_public: bool,
    pub is_private: bool,
    pub is_protected: bool,
    pub is_static: bool,
    pub is_const: bool,
    pub is_readonly: bool,
    pub is_virtual: bool,
    pub is_override: bool,
    pub is_abstract: bool,
    pub is_async: bool,
    /// `client`: the method runs on clients (the server is the default)
    pub is_client: bool,
    /// `extern` / `extern("c_name")`: a C function the host program provides
    pub is_extern: bool,
    pub extern_name: Option<(String, Span)>,
    /// `own T Name(...)`: the caller receives ownership of the returned list, array or dictionary
    pub is_own: bool,
    /// `mut`: the field (or the struct method's `this`) may change; bindings are immutable by default
    pub is_mut: bool,
    /// span of the modifiers (empty when there are none)
    pub span: Option<Span>,
}

#[derive(Clone, Debug)]
pub struct Attr {
    pub name: String,
    pub args: Vec<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeDecl {
    pub kind: DeclKind,
    pub name: String,
    /// `struct Pair<T, U>`: a generic struct, instantiated once per distinct type arguments
    pub type_params: Vec<(String, Span)>,
    pub constraints: Vec<Constraint>,
    pub span: Span,
    pub name_span: Span,
    /// span of the opening `{` of the body
    pub body_open: Span,
    /// the first name in `: Base, I1, I2` (`Entity` or a base class; for structs and interfaces an
    /// interface)
    pub base: Option<TypeExpr>,
    /// the names after the first in `: Base, I1, I2<int>` (interfaces, generic ones with their
    /// type arguments)
    pub ifaces: Vec<TypeExpr>,
    pub attrs: Vec<Attr>,
    pub mods: Mods,
    pub members: Vec<Member>,
    pub variants: Vec<(String, Option<i64>, Span)>,
}

#[derive(Clone, Debug)]
pub enum Member {
    Field(FieldDecl),
    Method(MethodDecl),
}

#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub attrs: Vec<Attr>,
    pub mods: Mods,
    pub ty: TypeExpr,
    pub name: String,
    pub init: Option<Expr>,
    /// span of the field name
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MethodKind {
    Normal,
    Ctor,
    Operator(String),
}

#[derive(Clone, Debug)]
pub struct MethodDecl {
    pub attrs: Vec<Attr>,
    pub mods: Mods,
    pub kind: MethodKind,
    pub ret: TypeExpr,
    pub name: String,
    /// `Name<T, U>(...)`: a generic method, instantiated once per distinct type arguments
    pub type_params: Vec<(String, Span)>,
    /// `where T : struct, new()`: checked against the type arguments of every instance
    pub constraints: Vec<Constraint>,
    pub params: Vec<Param>,
    pub body: Option<Block>,
    /// `T Name => expr;` (get-only expression-bodied property)
    pub is_property: bool,
    pub span: Span,
    pub name_span: Span,
    /// body written as `=> expr;`
    pub expr_body: bool,
}

/// `where T : a, b`: the constraints on one type parameter
#[derive(Clone, Debug)]
pub struct Constraint {
    pub param: String,
    pub span: Span,
    pub kinds: Vec<(ConstraintKind, Span)>,
}

#[derive(Clone, Debug)]
pub enum ConstraintKind {
    /// `class` (or `class?`): a reference type (entity class, string, list, dictionary, array)
    Class,
    /// `struct`: a value type (number, bool, char, enum, struct)
    Struct,
    /// `new()`: a type with a parameterless constructor
    New,
    /// `notnull`: not a nullable entity reference
    NotNull,
    /// `unmanaged`: a value type with no references inside
    Unmanaged,
    /// `Entity`, a class name (that class or a class derived from it), or another type
    Type(TypeExpr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamMode {
    /// read-only borrow (the default)
    Value,
    /// `own T x`: the callee takes ownership; the caller's value moves into the call
    Own,
    Ref,
    Out,
}

#[derive(Clone, Debug)]
pub struct Param {
    /// `mut int x` / `mut own List<T> xs`: the callee may change its copy (or the list it owns)
    pub is_mut: bool,
    pub mode: ParamMode,
    pub ty: TypeExpr,
    pub name: String,
    pub span: Span,
    pub name_span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeExpr {
    pub name: String,
    pub args: Vec<TypeExpr>,
    pub nullable: bool,
    /// `int in 0..100`: every write to a value of this type saturates into [lo, hi] (both ends included)
    pub range: Option<RangeSpec>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RangeSpec {
    pub lo: RangeBound,
    pub hi: RangeBound,
    pub span: Span,
}

/// one end of a range: a number, or a constant (`MAX_HP`, `Rules.MAX_HP`, `-ARENA`)
#[derive(Clone, Debug, PartialEq)]
pub enum RangeBound {
    Num(f64),
    Const { neg: bool, path: String, span: Span },
}

impl TypeExpr {
    pub fn void(span: Span) -> TypeExpr {
        TypeExpr { name: "void".into(), args: vec![], nullable: false, range: None, span }
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Block(Block),
    /// `T x = e;`, `mut T x = e;`, `const T x = e;` (a local constant: its value is known at compile time)
    Var { ty: Option<TypeExpr>, name: String, init: Option<Expr>, span: Span, name_span: Span, is_mut: bool, is_const: bool },
    If { cond: Expr, then: Box<Stmt>, els: Option<Box<Stmt>>, span: Span },
    While { cond: Expr, body: Box<Stmt>, span: Span },
    DoWhile { body: Box<Stmt>, cond: Expr, span: Span },
    For { init: Vec<Stmt>, cond: Option<Expr>, step: Vec<Expr>, body: Box<Stmt>, span: Span },
    /// `foreach (var x in xs)`; `foreach (var (s, b) in Query<Ship, Burning>())` puts the names
    /// after the first in `more` (one cursor per type argument of the query)
    Foreach { name: String, name_span: Span, more: Vec<(String, Span)>, iter: Expr, body: Box<Stmt>, span: Span },
    Return { value: Option<Expr>, span: Span },
    Break(Span),
    Continue(Span),
    Expr(Expr, Span),
    Atomic { body: Block, span: Span },
    Fail(Span),
    Empty(Span),
    Switch { scrut: Expr, sections: Vec<SwitchSection>, span: Span },
}

/// `case P when G:` ... / `default:` ... (C# switch statement section)
#[derive(Clone, Debug)]
pub struct SwitchSection {
    pub labels: Vec<CaseLabel>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum CaseLabel {
    Default(Span),
    Case(Pattern, Option<Expr>, Span),
}

/// C# 9 patterns (subset): constants, relational, `and`/`or`/`not`, discard `_`.
#[derive(Clone, Debug)]
pub enum Pattern {
    Discard(Span),
    Const(Expr),
    Rel(BinOp, Expr, Span),
    Not(Box<Pattern>, Span),
    And(Box<Pattern>, Box<Pattern>),
    Or(Box<Pattern>, Box<Pattern>),
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Discard(s) | Pattern::Rel(_, _, s) | Pattern::Not(_, s) => *s,
            Pattern::Const(e) => e.span,
            Pattern::And(a, b) | Pattern::Or(a, b) => a.span().to(b.span()),
        }
    }
}

/// one arm of a switch expression: `P when G => value`
#[derive(Clone, Debug)]
pub struct SwitchArm {
    pub pat: Pattern,
    pub guard: Option<Expr>,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl BinOp {
    pub fn sym(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Rem => "%",
            Eq => "==",
            Ne => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            And => "&&",
            Or => "||",
            BitAnd => "&",
            BitOr => "|",
            BitXor => "^",
            Shl => "<<",
            Shr => ">>",
        }
    }
    pub fn is_compare(self) -> bool {
        matches!(self, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
    }
    pub fn op_name(self) -> &'static str {
        use BinOp::*;
        match self {
            Add => "op_add",
            Sub => "op_sub",
            Mul => "op_mul",
            Div => "op_div",
            Rem => "op_rem",
            Eq => "op_eq",
            Ne => "op_ne",
            Lt => "op_lt",
            Le => "op_le",
            Gt => "op_gt",
            Ge => "op_ge",
            _ => "op_other",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
    Plus,
}

#[derive(Clone, Debug)]
pub enum InterpPart {
    Text(String),
    Expr(Expr, Option<String>),
}

#[derive(Clone, Debug)]
pub struct Arg {
    pub mode: ParamMode,
    pub expr: Expr,
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    Float(f64, bool),
    Bool(bool),
    Null,
    /// `default(T)`, or `default` (typed by its context)
    Default(Option<TypeExpr>),
    /// `Pair<int>` before `.`: a generic type used to reach its static members
    GenericName(TypeExpr),
    Str(String),
    /// character literal (Unicode code point)
    Char(u32),
    Interp(Vec<InterpPart>),
    Name(String),
    This,
    Base,
    Member(Box<Expr>, String, Span),
    Call { callee: Box<Expr>, type_args: Vec<TypeExpr>, args: Vec<Arg> },
    /// `xs[i]`, `grid[x, y]` (one index per dimension)
    Index(Box<Expr>, Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    /// ++/-- ; (is_increment, is_prefix)
    IncDec(Box<Expr>, bool, bool),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Assign(Option<BinOp>, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Cast(TypeExpr, Box<Expr>),
    Is(Box<Expr>, TypeExpr, Option<(String, Span)>),
    New { ty: TypeExpr, args: Option<Vec<Arg>>, inits: Vec<(String, Expr, Span)> },
    /// collection initializer: `new List<int> { 1, 2 }`, `new Dictionary<K, V> { { k, v }, [k] = v }`
    /// (each item: its expressions, and whether it is the `[k] = v` form)
    NewColl { ty: TypeExpr, args: Option<Vec<Arg>>, items: Vec<(Vec<Expr>, bool)> },
    /// `scrutinee switch { arms }` (C# 8 switch expression)
    Switch(Box<Expr>, Vec<SwitchArm>),
    /// `x is <pattern>` with a non-type pattern (`x is > 0 and < 10`, `e is ETeam.Red or ETeam.Blue`)
    IsPattern(Box<Expr>, Box<Pattern>),
    /// `await Wait(t)` / `await NextTick()` / `await Until(cond)` inside an async entity method
    Await(Box<Expr>),
    /// `out int x` / `out var x` / `out _` in an argument list: declares a local (type None = var;
    /// name "_" = discard)
    OutDecl(Option<TypeExpr>, String, Span),
    /// `new T[n]`, `new T[] { a, b }`, `new T[2] { a, b }`, or `{ a, b }` initializing a `T[]`; with
    /// `rank` 2 or 3, `new T[w, h]`, `new T[,] { { a, b }, { c, d } }` or `{ { a, b }, { c, d } }`
    /// initializing a `T[,]`. `lens`: the lengths in the brackets (none or `rank` of them); `elems`:
    /// the initializer's elements in row-major order, and `shape` the lengths it spells out
    NewArray { elem: TypeExpr, rank: usize, lens: Vec<Expr>, elems: Option<Vec<Expr>>, shape: Vec<usize> },
    /// `x => e`, `(a, b) => e`, `(int a) => { ... }`: only as an argument of list/array methods
    Lambda { params: Vec<(Option<TypeExpr>, String, Span)>, body: Box<LambdaBody> },
}

#[derive(Clone, Debug)]
pub enum LambdaBody {
    Expr(Expr),
    Block(Block),
}

impl Stmt {
    /// Full source span of the statement (best effort; includes the trailing `;` for simple statements).
    pub fn span(&self) -> Span {
        match self {
            Stmt::Block(b) => b.span,
            Stmt::Var { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::DoWhile { span, .. }
            | Stmt::For { span, .. }
            | Stmt::Foreach { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Atomic { span, .. }
            | Stmt::Switch { span, .. } => *span,
            Stmt::Expr(_, s) => *s,
            Stmt::Break(s) | Stmt::Continue(s) | Stmt::Fail(s) | Stmt::Empty(s) => *s,
        }
    }
}
