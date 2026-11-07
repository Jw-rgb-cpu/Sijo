//! Name resolution and type checking: AST -> HIR.

use crate::ast::{self, BinOp, DeclKind, MethodKind, ParamMode, UnOp};
use crate::consteval;
use crate::diag::{Diags, Span};
use crate::hir::*;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum TypeRef {
    Struct(StructId),
    Class(ClassId),
    Enum(EnumId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Struct(StructId),
    Entity(ClassId),
    Static(ClassId),
}

struct FnState {
    owner: Owner,
    is_static: bool,
    scopes: Vec<HashMap<String, LocalId>>,
    locals: Vec<Local>,
    ret: Ty,
    loop_depth: usize,
    switch_depth: usize,
    atomic_depth: usize,
    writes_self: bool,
    fid: Option<FuncId>,
    /// inside an async method: number of awaits so far and the locals in scope at any await
    is_async: bool,
    awaits: usize,
    live_across: std::collections::BTreeSet<LocalId>,
    /// the scope that `out var x` declarations of the current statement go to (the enclosing
    /// block, so the variable is visible after the statement, as in C#)
    stmt_scope: usize,
    discards: usize,
    /// `out var x` locals declared since the current statement started (declared before it)
    out_decls: Vec<LocalId>,
    /// await Rush(...): the hidden bit set of background arms and the next free bit
    rush_bg: Option<Expr>,
    rush_next: u32,
    /// `int in 0..100 F()`: the range every returned value saturates into
    ret_range: Option<(f64, f64)>,
    /// lowering a for loop's step: the variables its header declared may change there
    step_locals: Vec<LocalId>,
    /// every variable a for header declared (immutable in the body unless 'mut')
    for_locals: Vec<LocalId>,
    /// the struct method was declared `mut` (it may change `this`)
    mut_self: bool,
    /// inside a lambda that becomes a delegate value: the first local of the lambda (locals before
    /// it are the enclosing function's, so using one captures it)
    lam_base: Option<usize>,
}

/// a range with its bounds evaluated
struct RangeVals {
    lo: f64,
    hi: f64,
    span: Span,
}

pub struct Checker<'a> {
    p: Program,
    /// constants' initializers by (type, name), for range bounds that name a constant
    const_src: HashMap<(TypeRef, String), ast::Expr>,
    /// local `const` initializers, checked to be compile-time values after pass 5
    local_consts: Vec<(Expr, Span)>,
    d: &'a mut Diags,
    types: HashMap<String, TypeRef>,
    consts: HashMap<(TypeRef, String), ConstId>,
    st: Option<FnState>,
    /// switch statements/expressions whose exhaustiveness is checked once constants are known
    switch_checks: Vec<SwitchCheck>,
    /// (awaiting method, awaited method, span) for the cycle check
    await_edges: Vec<(FuncId, FuncId, Span)>,
    /// coroutines with an await Rush(...): they must run as systems (their background arms are
    /// stepped each tick when the system runs)
    rush_funcs: Vec<(FuncId, Span)>,
    /// generic methods by (declaring type, name): index into `tmpls`
    templates: HashMap<(TypeRef, String), usize>,
    tmpls: Vec<Template>,
    /// type parameters of the generic method instance being declared or checked
    tenv: Vec<(String, Ty)>,
    /// generic method instances whose bodies are still to be checked
    inst_queue: Vec<Instance>,
    /// the second and later instances of a template add nothing to the IDE index (the first one
    /// already maps every name in the template's source)
    quiet: bool,
    /// generic structs by name: index into `stmpls`
    stemplates: HashMap<String, usize>,
    stmpls: Vec<StructTemplate>,
    /// struct instance -> (template, type arguments)
    struct_inst: HashMap<StructId, (usize, Vec<Ty>)>,
    /// field initializers of struct instances still to be checked, then verified constant
    inst_field_inits: Vec<(StructId, usize, ast::Expr, Vec<(String, Ty)>, bool)>,
    inst_field_checks: Vec<(StructId, usize, Span)>,
    /// generic interfaces by name: index into `itmpls`
    itemplates: HashMap<String, usize>,
    itmpls: Vec<IfaceTemplate>,
    /// interface instance -> (template, type arguments)
    iface_inst: HashMap<ClassId, (usize, Vec<Ty>)>,
    /// interface instances named before pass 3 (in a `: I<int>` list): their methods are declared
    /// when pass 3 starts, once constants can be looked up
    iface_pending: Vec<ClassId>,
    /// pass 3 has started: a new interface instance declares its methods at once
    members_open: bool,
    /// every interface's methods are declared: a new struct instance that implements interfaces
    /// takes their default implementations at once
    defaults_open: bool,
    /// an interface's default implementation -> its declaration and the type arguments of its
    /// interface (a struct that uses the default gets a copy, checked against the struct)
    default_src: HashMap<FuncId, (ast::MethodDecl, Vec<(String, Ty)>)>,
    /// generic entity classes by name: index into `ctmpls`
    ctemplates: HashMap<String, usize>,
    ctmpls: Vec<ClassTemplate>,
    /// class instance -> (template, type arguments)
    class_inst: HashMap<ClassId, (usize, Vec<Ty>)>,
    /// class instances created while class bases are resolved (pass 2): their members are
    /// declared when pass 3 starts (constants and every class are known then)
    pending_members: Vec<ClassId>,
    members_ready: bool,
    /// the per-class checks of pass 3 (overrides, field/method names) ran: a later instance runs
    /// them when its members are declared
    class_checks_done: bool,
    /// entity field initializers of class instances still to be checked: (class, field, initializer,
    /// type parameters, quiet), then verified constant
    inst_efield_inits: Vec<(ClassId, FieldId, ast::Expr, Vec<(String, Ty)>, bool)>,
    inst_efield_checks: Vec<(FieldId, Span)>,
    /// `int[K.N] F`: the length expressions, evaluated once the constants are known
    fixed_lens: Vec<(FieldId, ast::Expr, TypeRef)>,
    /// `delegate R Name(...)` declarations: name -> delegate type (its signature is filled when
    /// pass 3 starts)
    named_delegates: HashMap<String, DelegateId>,
    delegate_decls: Vec<(DelegateId, ast::MethodDecl)>,
    /// the hidden static class that owns the delegates' invoke functions
    dg_class: Option<ClassId>,
    /// lambdas made so far in each function (they are named `F__lambda1`, `F__lambda2`, ...)
    lambda_count: HashMap<String, usize>,
    /// resolving the `Util<int>` of `Util<int>.F()`: an instance of a generic static class is a
    /// place for functions and constants there, not a type
    static_ok: bool,
    /// constants of generic static class instances: checked and evaluated with the others
    inst_const_inits: Vec<(ConstId, TypeRef, ast::Expr, Vec<(String, Ty)>)>,
    /// the run that checks unused generic code: the extents of those declarations
    dry: Vec<Span>,
    /// making such an instance: its type arguments are all the error type
    dry_force: bool,
}

/// A generic interface: declared once per distinct list of type arguments, like a generic struct.
struct IfaceTemplate {
    decl: ast::TypeDecl,
    /// the members, checked once (signatures and default implementations only)
    members: Vec<ast::Member>,
    /// declaration order among entity classes (the systems of default `Update` methods run there)
    order: usize,
    instances: Vec<(Vec<Ty>, ClassId)>,
}

/// A generic entity class: each distinct list of type arguments is its own entity class (an
/// archetype of its own, like a C++ template instance), declared on first use.
struct ClassTemplate {
    decl: ast::TypeDecl,
    /// sort key of the declaration: instances sort right after it (`ClassDef::order`)
    order: usize,
    instances: Vec<(Vec<Ty>, ClassId)>,
}

/// `ClassDef::order` of declared classes is a multiple of this; the instances of a generic class
/// take the keys after their template's
const ORDER_STRIDE: usize = 1 << 16;

struct StructTemplate {
    decl: ast::TypeDecl,
    instances: Vec<(Vec<Ty>, StructId)>,
}

/// A generic method: declared and checked once per distinct list of type arguments (C++-style),
/// so an operation a type argument lacks is reported at the instantiation that needs it.
struct Template {
    md: ast::MethodDecl,
    owner: Owner,
    kind: FuncKind,
    instances: Vec<(Vec<Ty>, FuncId)>,
    /// type parameters of the generic class instance that declares the method (empty otherwise)
    outer: Vec<(String, Ty)>,
    /// the declaring class instance adds nothing to the IDE index (it is not the first instance)
    quiet: bool,
}

struct Instance {
    fid: FuncId,
    owner: Owner,
    md: ast::MethodDecl,
    env: Vec<(String, Ty)>,
    quiet: bool,
    /// the note on errors in this instance (default: "in 'F<int>', an instance of this generic method")
    note: Option<String>,
}

/// instances of one generic method before the compiler assumes runaway recursion (`F<List<T>>`)
const MAX_INSTANCES: usize = 64;

/// pattern shape kept for exhaustiveness / reachability analysis
#[derive(Clone, Debug)]
enum PK {
    Any,
    Val(Expr),
    Rel(BinOp, Expr),
    Not(Box<PK>),
    And(Box<PK>, Box<PK>),
    Or(Box<PK>, Box<PK>),
}

#[derive(Debug)]
struct SwitchCheck {
    ty: Ty,
    /// (pattern, has `when` guard, span) in source order; `default:` labels are not included
    arms: Vec<(PK, bool, Span)>,
    has_default: bool,
    is_expr: bool,
    span: Span,
}

const NAMESPACES: &[&str] = &["Math", "Draw", "Random", "Time", "string", "String", "int", "float", "char", "Array", "Net", "Grid"];
/// the methods of a 2D or 3D array, a grid (Grid<T> is T[,], Grid3<T> is T[,,]); `Grid` is a
/// built-in name only while the program declares no type of that name (Grid.Dir, Grid.Stay)
const GRID_METHODS: &[&str] = &["InBounds", "Get", "Fill", "Flood", "Step", "Move", "Ray"];

/// what entities do with components: `e.Add(new C { ... })`, `e.Remove<C>()`, `e.Has<C>()`, `e.Get<C>()`
const COMP_OPS: &[&str] = &["Add", "Remove", "Has", "Get"];

pub fn check(file: &ast::File, d: &mut Diags) -> Program {
    let p = check_with(file, d, None);
    let unused = unused_generics(&p, file);
    if unused.is_empty() {
        return p;
    }
    // Generic code nothing uses is checked once more, in a separate run that instantiates it with
    // its type parameters unknown (the error type, which takes any operation without a message):
    // what is wrong whatever the type arguments (unknown names, wrong argument counts, type
    // mismatches between known types, missing returns, writes to immutable bindings) is reported.
    let mut d2 = Diags::default();
    let _ = check_with(file, &mut d2, Some(&unused));
    for x in d2.list {
        let Some((what, _)) = unused.iter().find(|(_, ext)| ext.lo <= x.span.lo && x.span.hi <= ext.hi) else { continue };
        if d.list.iter().any(|y| y.code == x.code && y.span == x.span && y.msg == x.msg) {
            continue;
        }
        let mut x = x;
        x.notes.retain(|n| !n.contains("an instance of this generic"));
        x.notes.push(format!("in '{}', which nothing uses: checked with its type parameters unknown", what));
        d.list.push(x);
    }
    p
}

/// The generic classes, structs and methods no code instantiates, with the source extent of
/// each declaration
fn unused_generics(p: &Program, file: &ast::File) -> Vec<(String, Span)> {
    let mut out = vec![];
    let extent = |d: &ast::TypeDecl| -> Span {
        let mut sp = d.span;
        for m in &d.members {
            match m {
                ast::Member::Field(f) => {
                    sp = sp.to(f.span);
                    if let Some(e) = &f.init {
                        sp = sp.to(e.span);
                    }
                }
                ast::Member::Method(md) => sp = sp.to(method_extent(md)),
            }
        }
        sp
    };
    for d in &file.items {
        if d.type_params.is_empty() || !matches!(d.kind, DeclKind::Class | DeclKind::Struct) {
            continue;
        }
        let used = match d.kind {
            DeclKind::Class => p.generic_classes.iter().any(|(n, insts)| n.split('<').next() == Some(d.name.as_str()) && !insts.is_empty()),
            _ => p.structs.iter().any(|s| s.name.starts_with(&format!("{}<", d.name)) && s.span == d.span),
        };
        if !used {
            let tps: Vec<&str> = d.type_params.iter().map(|(n, _)| n.as_str()).collect();
            out.push((format!("{}<{}>", d.name, tps.join(", ")), extent(d)));
        }
    }
    // generic methods: a declaration none of whose instances exists (in a used type)
    for d in &file.items {
        for m in &d.members {
            let ast::Member::Method(md) = m else { continue };
            if md.type_params.is_empty() || out.iter().any(|(_, ext)| ext.lo <= md.span.lo && md.span.hi <= ext.hi) {
                continue;
            }
            if !p.funcs.iter().any(|f| f.span == md.span && f.name.starts_with(&format!("{}<", md.name))) {
                let tps: Vec<&str> = md.type_params.iter().map(|(n, _)| n.as_str()).collect();
                out.push((format!("{}.{}<{}>", d.name, md.name, tps.join(", ")), method_extent(md)));
            }
        }
    }
    out
}

/// a method's declaration with its body
fn method_extent(md: &ast::MethodDecl) -> Span {
    match &md.body {
        Some(b) => md.span.to(b.span),
        None => md.span,
    }
}

fn check_with(file: &ast::File, d: &mut Diags, dry: Option<&[(String, Span)]>) -> Program {
    let mut c = Checker { p: Program::default(), const_src: HashMap::new(), local_consts: vec![], d, types: HashMap::new(), consts: HashMap::new(), st: None, switch_checks: vec![], await_edges: vec![], rush_funcs: vec![], templates: HashMap::new(), tmpls: vec![], tenv: vec![], inst_queue: vec![], quiet: false, stemplates: HashMap::new(), stmpls: vec![], struct_inst: HashMap::new(), inst_field_inits: vec![], inst_field_checks: vec![], itemplates: HashMap::new(), itmpls: vec![], iface_inst: HashMap::new(), iface_pending: vec![], members_open: false, defaults_open: false, default_src: HashMap::new(), ctemplates: HashMap::new(), ctmpls: vec![], class_inst: HashMap::new(), pending_members: vec![], members_ready: false, class_checks_done: false, inst_efield_inits: vec![], inst_efield_checks: vec![], fixed_lens: vec![], named_delegates: HashMap::new(), delegate_decls: vec![], dg_class: None, lambda_count: HashMap::new(), static_ok: false, inst_const_inits: vec![], dry: vec![], dry_force: false };
    c.dry = dry.map(|v| v.iter().map(|(_, sp)| *sp).collect()).unwrap_or_default();
    c.run(file);
    c.p
}

/// `[Order(n)]` is checked once per generic class: on its first instance
fn quiet_order(k: usize) -> bool {
    k == 0
}

fn attr<'b>(attrs: &'b [ast::Attr], name: &str) -> Option<&'b ast::Attr> {
    attrs.iter().find(|a| a.name == name || a.name == format!("{}Attribute", name))
}

fn lit_num(e: &ast::Expr) -> Option<f64> {
    match &e.kind {
        ast::ExprKind::Int(n) => Some(*n as f64),
        ast::ExprKind::Float(f, _) => Some(*f),
        ast::ExprKind::Unary(UnOp::Neg, x) => lit_num(x).map(|v| -v),
        _ => None,
    }
}

impl<'a> Checker<'a> {
    fn run(&mut self, file: &ast::File) {
        // ---- pass 1: type names
        // declaration order of classes (instances of a generic class sort right after it)
        let mut entity_order = 0;
        for decl in &file.items {
            if self.types.contains_key(&decl.name) || self.named_delegates.contains_key(&decl.name) || self.stemplates.contains_key(&decl.name) || self.itemplates.contains_key(&decl.name) || self.ctemplates.contains_key(&decl.name) || (NAMESPACES.contains(&decl.name.as_str()) && decl.name != "Grid") || decl.name == "Entity" {
                self.d.error("E0102", decl.span, format!("duplicate or reserved type name '{}'", decl.name));
                continue;
            }
            if decl.type_params.is_empty() && !decl.constraints.is_empty() {
                self.d.error("E0260", decl.constraints[0].span, format!("'where' constraints on '{}', which has no type parameters", decl.name));
            }
            if decl.kind == DeclKind::Delegate {
                if self.named_delegates.contains_key(&decl.name) || matches!(decl.name.as_str(), "Func" | "Action") {
                    self.d.error("E0102", decl.span, format!("duplicate or reserved type name '{}'", decl.name));
                    continue;
                }
                let Some(ast::Member::Method(md)) = decl.members.first() else { continue };
                self.p.delegates.push(DelegateDef { name: Some(decl.name.clone()), params: vec![], ret: Ty::Void, funcs: vec![], invoke: usize::MAX, span: decl.span });
                let id = self.p.delegates.len() - 1;
                self.named_delegates.insert(decl.name.clone(), id);
                self.delegate_decls.push((id, md.clone()));
                continue;
            }
            if !decl.type_params.is_empty() {
                if decl.kind == DeclKind::Interface {
                    // `interface IContainer<T>`: instantiated per distinct type arguments
                    if decl.mods.is_static {
                        self.d.error("E0262", decl.name_span, "an interface cannot be static");
                    }
                    let members = self.interface_members(decl);
                    self.itmpls.push(IfaceTemplate { decl: decl.clone(), members, order: entity_order * ORDER_STRIDE, instances: vec![] });
                    self.itemplates.insert(decl.name.clone(), self.itmpls.len() - 1);
                    entity_order += 1;
                    continue;
                }
                if decl.kind == DeclKind::Class && decl.mods.is_static && decl.name == "Game" {
                    self.d.error_note("E0258", decl.name_span, "static class 'Game' cannot be generic", "Game is where the program starts: it has one Start and one Update");
                    continue;
                }
                if decl.kind == DeclKind::Class {
                    if !self.check_type_params(&decl.type_params) {
                        continue;
                    }
                    self.ctmpls.push(ClassTemplate { decl: decl.clone(), order: entity_order * ORDER_STRIDE, instances: vec![] });
                    let tps: Vec<&str> = decl.type_params.iter().map(|(n, _)| n.as_str()).collect();
                    self.p.generic_classes.push((format!("{}<{}>", decl.name, tps.join(", ")), vec![]));
                    self.ctemplates.insert(decl.name.clone(), self.ctmpls.len() - 1);
                    entity_order += 1;
                    continue;
                }
                if decl.kind != DeclKind::Struct {
                    self.d.error_note(
                        "E0258",
                        decl.name_span,
                        format!("'{}': only structs, entity classes, interfaces and methods can be generic", decl.name),
                        if decl.kind == DeclKind::Component { "a component is one kind of data an entity gains and loses at run time: declare one component per type" } else { "an enum is a set of named integers" },
                    );
                    continue;
                }
                self.stmpls.push(StructTemplate { decl: decl.clone(), instances: vec![] });
                self.stemplates.insert(decl.name.clone(), self.stmpls.len() - 1);
                continue;
            }
            let r = match decl.kind {
                DeclKind::Struct => {
                    self.p.structs.push(StructDef {
                        name: decl.name.clone(),
                        span: decl.span,
                        fields: vec![],
                        methods: HashMap::new(), overloads: vec![],
                        ctors: vec![],
                        ops: vec![],
                    });
                    TypeRef::Struct(self.p.structs.len() - 1)
                }
                DeclKind::Enum => {
                    let variants = decl.variants.iter().map(|(n, v, _)| (n.clone(), v.unwrap_or(0))).collect();
                    self.p.enums.push(EnumDef { name: decl.name.clone(), variants });
                    TypeRef::Enum(self.p.enums.len() - 1)
                }
                DeclKind::Delegate => continue,
                DeclKind::Class | DeclKind::Interface | DeclKind::Component => {
                    let iface = decl.kind == DeclKind::Interface;
                    let comp = decl.kind == DeclKind::Component;
                    if iface && decl.mods.is_static {
                        self.d.error("E0262", decl.name_span, "an interface cannot be static");
                    }
                    if comp && (decl.mods.is_static || decl.mods.is_abstract) {
                        self.d.error_note("E0264", decl.name_span, format!("a component cannot be {}", if decl.mods.is_static { "static" } else { "abstract" }), "a component is data and methods that entities gain with Add(new C { ... }) and lose with Remove<C>()");
                    }
                    if comp && (decl.base.is_some() || !decl.ifaces.is_empty()) {
                        let sp = decl.base.as_ref().map(|b| b.span).unwrap_or(decl.name_span);
                        self.d.error_note("E0264", sp, format!("component '{}' cannot derive from anything", decl.name), "components are combined on an entity at run time instead: e.Add(new A { ... }); e.Add(new B { ... });");
                    }
                    let is_entity = iface || comp || !decl.mods.is_static;
                    let run_order = self.order_attr(&decl.attrs, is_entity && !iface, true);
                    self.p.classes.push(ClassDef {
                        name: decl.name.clone(),
                        span: decl.span,
                        base: None,
                        is_entity,
                        is_static: !iface && !comp && decl.mods.is_static,
                        is_abstract: iface || comp || decl.mods.is_abstract,
                        is_interface: iface,
                        ifaces: vec![],
                        is_component: comp,
                        combo_of: None,
                        comps: vec![],
                        own_fields: vec![],
                        methods: HashMap::new(), overloads: vec![],
                        order: entity_order * ORDER_STRIDE,
                        run_order,
                    });
                    entity_order += 1;
                    TypeRef::Class(self.p.classes.len() - 1)
                }
            };
            self.types.insert(decl.name.clone(), r);
            self.def(decl.name_span, Self::type_sym(r));
            if let TypeRef::Enum(e) = r {
                for (i, (_, _, vsp)) in decl.variants.iter().enumerate() {
                    self.def(*vsp, Sym::Variant(e, i));
                }
            }
        }
        // `Owner` in a component method: a handle of whatever entity has the component
        if file.items.iter().any(|d| d.kind == DeclKind::Component) {
            self.p.classes.push(ClassDef {
                name: "Entity".into(),
                span: Span::default(),
                base: None,
                is_entity: true,
                is_static: false,
                is_abstract: true,
                is_interface: false,
                ifaces: vec![],
                is_component: false,
                combo_of: None,
                comps: vec![],
                own_fields: vec![],
                methods: HashMap::new(), overloads: vec![],
                order: entity_order,
                run_order: 0,
            });
            self.p.entity_root = Some(self.p.classes.len() - 1);
        }
        // ---- pass 2: class bases and interfaces
        for decl in &file.items {
            if decl.kind == DeclKind::Component {
                continue;
            }
            let names: Vec<ast::TypeExpr> = decl.base.iter().chain(decl.ifaces.iter()).cloned().collect();
            match self.types.get(&decl.name) {
                // a struct implements interfaces (checked for `where T : I`)
                Some(&TypeRef::Struct(sid)) if decl.kind == DeclKind::Struct => {
                    for te in &names {
                        match self.header_iface(te) {
                            Ok(i) => self.p.struct_ifaces.entry(sid).or_default().push(i),
                            Err(true) => {}
                            Err(false) => self.d.error_note("E0209", te.span, format!("'{}' is not an interface", te.name), "a struct cannot derive from anything; it can implement interfaces"),
                        }
                    }
                    continue;
                }
                Some(&TypeRef::Class(cid)) if self.p.classes[cid].is_interface => {
                    for te in &names {
                        match self.header_iface(te) {
                            Ok(i) => self.p.classes[cid].ifaces.push(i),
                            Err(true) => {}
                            Err(false) => self.d.error("E0209", te.span, format!("interface '{}' can only extend interfaces ('{}' is not one)", decl.name, te.name)),
                        }
                    }
                    continue;
                }
                Some(&TypeRef::Class(_)) => {}
                _ => continue,
            }
            let Some(&TypeRef::Class(cid)) = self.types.get(&decl.name) else { continue };
            self.class_header(cid, decl);
        }
        // interfaces that extend themselves
        for c in 0..self.p.classes.len() {
            if self.p.classes[c].is_interface && self.p.classes[c].ifaces.clone().iter().any(|&i| self.p.extends(i, c, 0)) {
                self.d.error("E0214", self.p.classes[c].span, "interface inheritance cycle");
                self.p.classes[c].ifaces.clear();
            }
        }
        // inheritance cycles
        for c in 0..self.p.classes.len() {
            let mut seen = 0;
            let mut cur = self.p.classes[c].base;
            while let Some(x) = cur {
                seen += 1;
                if x == c || seen > self.p.classes.len() {
                    self.d.error("E0214", self.p.classes[c].span, "inheritance cycle");
                    self.p.classes[c].base = None;
                    break;
                }
                cur = self.p.classes[x].base;
            }
        }
        // ---- pass 3: members (fields, consts, signatures)
        for ti in 0..self.stmpls.len() {
            let (tps, cons, name) = (self.stmpls[ti].decl.type_params.clone(), self.stmpls[ti].decl.constraints.clone(), self.stmpls[ti].decl.name.clone());
            self.check_where(&tps, &cons, &name);
        }
        for ti in 0..self.ctmpls.len() {
            let (tps, cons, name) = (self.ctmpls[ti].decl.type_params.clone(), self.ctmpls[ti].decl.constraints.clone(), self.ctmpls[ti].decl.name.clone());
            self.check_where(&tps, &cons, &name);
            self.check_class_template(ti);
        }
        // constants' initializers by (type, name): range bounds may name them before they are evaluated
        for decl in &file.items {
            let Some(&tref) = self.types.get(&decl.name) else { continue };
            for m in &decl.members {
                if let ast::Member::Field(f) = m {
                    if let (true, Some(e)) = (f.mods.is_const, &f.init) {
                        self.const_src.insert((tref, f.name.clone()), e.clone());
                    }
                }
            }
        }
        // generic interfaces: their type parameters and constraints, then the methods of the
        // instances the `: I<int>` lists named
        for ti in 0..self.itmpls.len() {
            let (tps, cons, name) = (self.itmpls[ti].decl.type_params.clone(), self.itmpls[ti].decl.constraints.clone(), self.itmpls[ti].decl.name.clone());
            self.check_type_params(&tps);
            self.check_where(&tps, &cons, &name);
        }
        self.members_open = true;
        for c in std::mem::take(&mut self.iface_pending) {
            self.iface_members_of(c);
        }
        // generic class instances named as base classes (`class IntPool : Pool<int>`) declare their
        // members now, before the classes derived from them; later instances do it when created
        self.members_ready = true;
        for c in std::mem::take(&mut self.pending_members) {
            self.instance_members(c);
        }
        // `delegate R Name(...)`: the signatures, now that every type is known
        for (id, md) in std::mem::take(&mut self.delegate_decls) {
            let mut params = vec![];
            for p in &md.params {
                if p.mode != ParamMode::Value || p.is_mut || p.ty.range.is_some() {
                    self.d.error_note("E0268", p.span, "a delegate's parameters are plain value parameters", "no ref/out/own, 'mut' or range on them (the lambda or method it holds decides that)");
                }
                params.push(self.resolve_type(&ast::TypeExpr { range: None, ..p.ty.clone() }));
            }
            let ret = self.resolve_type(&ast::TypeExpr { range: None, ..md.ret.clone() });
            self.check_delegate_sig(&params, &ret, md.span);
            self.p.delegates[id].params = params;
            self.p.delegates[id].ret = ret;
            self.make_invoke(id);
        }
        let mut bodies: Vec<(FuncId, Owner, ast::MethodDecl)> = vec![];
        let mut const_inits: Vec<(ConstId, TypeRef, ast::Expr)> = vec![];
        let mut field_inits: Vec<(TypeRef, usize, ast::Expr)> = vec![];
        for decl in &file.items {
            let Some(&tref) = self.types.get(&decl.name) else { continue };
            if let TypeRef::Enum(_) = tref {
                continue;
            }
            let checked: Vec<ast::Member>;
            let members: &[ast::Member] = if decl.kind == DeclKind::Interface {
                checked = self.interface_members(decl);
                &checked
            } else {
                &decl.members
            };
            for m in members {
                match m {
                    ast::Member::Field(f) => {
                        self.check_field_mods(&f.mods);
                        if f.mods.is_const {
                            if f.mods.is_mut {
                                self.d.error("E0028", f.span, "a constant cannot be 'mut'");
                            }
                            if let Some(r) = &f.ty.range {
                                self.d.error("E0224", r.span, "a constant takes no range: its value is fixed");
                            }
                            let ty = self.resolve_type(&ast::TypeExpr { range: None, ..f.ty.clone() });
                            self.p.consts.push(ConstDef { name: f.name.clone(), ty, value: None, span: f.span });
                            let id = self.p.consts.len() - 1;
                            self.def(f.span, Sym::Const(id));
                            if self.consts.insert((tref, f.name.clone()), id).is_some() {
                                self.d.error("E0104", f.span, format!("duplicate member '{}'", f.name));
                            }
                            match &f.init {
                                Some(e) => const_inits.push((id, tref, e.clone())),
                                None => self.d.error("E0215", f.span, "constant requires an initializer"),
                            }
                            continue;
                        }
                        if f.mods.is_static {
                            self.d.error_note(
                                "E0206",
                                f.span,
                                format!("mutable static field '{}' is not allowed", f.name),
                                "global mutable state is a data race; store it on an entity (e.g. a 'GameState : Entity') or make it 'const'",
                            );
                            continue;
                        }
                        let ty = self.resolve_type(&ast::TypeExpr { range: None, ..f.ty.clone() });
                        self.check_comp_storage(&ty, f.ty.span, "a field");
                        if let TypeRef::Class(cid) = tref {
                            if self.p.classes[cid].is_component && (!crate::effects::message_copyable(&self.p, &ty, 0) || self.p.has_delegate(&ty)) {
                                self.d.error_note(
                                    "E0264",
                                    f.ty.span,
                                    format!("component field '{}' cannot hold a {}", f.name, if self.p.has_delegate(&ty) { "delegate" } else { "dictionary" }),
                                    "an Add carries a copy of the component's values like a message does (numbers, strings, structs, nullable entity references, lists and arrays of those), not dictionaries or delegates: keep those on the entity",
                                );
                            }
                        }
                        match tref {
                            TypeRef::Struct(sid) => {
                                if f.mods.is_mut {
                                    self.d.error_note("E0028", f.span, "struct members take no 'mut'", "a struct member changes when the variable or field holding the struct is 'mut'");
                                }
                                if let Some(r) = &f.ty.range {
                                    self.d.error_note("E0224", r.span, "struct members take no range", "put the range on the entity field that holds the struct: it applies to every member");
                                }
                                if ty.is_list() {
                                    self.d.error("E0216", f.span, "struct fields cannot be List<T> or Dictionary<K, V> (structs are plain values)");
                                }
                                if let Ty::Handle(_) = ty {
                                    self.d.error("E0302", f.ty.span, "entity references stored in fields must be nullable (add '?')");
                                }
                                if self.p.structs[sid].fields.iter().any(|x| x.name == f.name) {
                                    self.d.error("E0104", f.span, format!("duplicate field '{}'", f.name));
                                }
                                self.p.structs[sid].fields.push(SField { name: f.name.clone(), ty, init: None, span: f.span });
                                let fi = self.p.structs[sid].fields.len() - 1;
                                self.def(f.span, Sym::SField(sid, fi));
                                if let Some(e) = &f.init {
                                    field_inits.push((tref, self.p.structs[sid].fields.len() - 1, e.clone()));
                                }
                            }
                            TypeRef::Class(cid) => {
                                if !self.p.classes[cid].is_entity {
                                    continue;
                                }
                                if let Ty::Handle(_) = ty {
                                    self.d.error_note(
                                        "E0302",
                                        f.ty.span,
                                        "entity references stored in fields must be nullable (add '?')",
                                        "the referenced entity can be destroyed at any time; check it with 'if (F is T x)'",
                                    );
                                }
                                if let Ty::List(el) | Ty::Array(el) | Ty::MdArray(el, _) = &ty {
                                    if let Ty::Handle(_) = **el {
                                        self.d.error("E0302", f.ty.span, "lists and arrays stored in entity fields must hold nullable handles (List<T?>, T?[])");
                                    }
                                }
                                if let Ty::Dict(k, v) = &ty {
                                    if matches!(**k, Ty::Handle(_)) || matches!(**v, Ty::Handle(_)) {
                                        self.d.error("E0302", f.ty.span, "dictionaries stored in entity fields must hold nullable handles (Dictionary<T?, ...>)");
                                    }
                                }
                                if self.p.find_field(cid, &f.name).is_some() {
                                    self.d.error("E0104", f.span, format!("duplicate field '{}' (possibly inherited)", f.name));
                                }
                                let mut attrs = self.field_attrs(&f.attrs, f.ty.range.as_ref(), &ty, f.span, tref);
                                attrs.public = f.mods.is_public;
                                self.p.fields.push(EField { fixed_len: None, name: f.name.clone(), class: cid, is_mut: f.mods.is_mut, ty, init: None, attrs, span: f.span });
                                let fid = self.p.fields.len() - 1;
                                if let Some(l) = &f.fixed_len {
                                    self.fixed_lens.push((fid, l.clone(), tref));
                                }
                                self.p.classes[cid].own_fields.push(fid);
                                self.def(f.span, Sym::Field(fid));
                                if let Some(e) = &f.init {
                                    field_inits.push((tref, fid, e.clone()));
                                }
                            }
                            TypeRef::Enum(_) => {}
                        }
                    }
                    ast::Member::Method(md) => {
                        let (owner, kind) = match (tref, &md.kind) {
                            (TypeRef::Struct(s), MethodKind::Ctor) => (Owner::Struct(s), FuncKind::StructCtor(s)),
                            (TypeRef::Struct(s), MethodKind::Operator(_)) => (Owner::Struct(s), FuncKind::StructOp(s)),
                            (TypeRef::Struct(s), _) if md.mods.is_static => (Owner::Struct(s), FuncKind::StructStatic(s)),
                            (TypeRef::Struct(s), _) => (Owner::Struct(s), FuncKind::StructMethod(s)),
                            (TypeRef::Class(c), MethodKind::Ctor) => {
                                if self.p.classes[c].is_component {
                                    self.d.error("E0217", md.span, "components cannot declare constructors; use field initializers and Add(new C { ... })");
                                } else {
                                    self.d.error("E0217", md.span, "entity classes cannot declare constructors; use field initializers and Spawn(new T { ... })");
                                }
                                continue;
                            }
                            (TypeRef::Class(c), MethodKind::Operator(_)) => {
                                self.d.error("E0218", md.span, "operators can only be declared in structs");
                                let _ = c;
                                continue;
                            }
                            (TypeRef::Class(c), _) if md.mods.is_static || self.p.classes[c].is_static => {
                                (Owner::Static(c), FuncKind::Static(c))
                            }
                            (TypeRef::Class(c), _) => (Owner::Entity(c), FuncKind::EntityMethod(c)),
                            (TypeRef::Enum(_), _) => continue,
                        };
                        let dup = match tref {
                            TypeRef::Struct(s) => self.p.structs[s].methods.contains_key(&md.name),
                            TypeRef::Class(c) => self.p.classes[c].methods.contains_key(&md.name),
                            TypeRef::Enum(_) => false,
                        } || self.templates.contains_key(&(tref, md.name.clone()));
                        if md.type_params.is_empty() && !md.constraints.is_empty() {
                            self.d.error("E0260", md.constraints[0].span, format!("'where' constraints on '{}', which has no type parameters", md.name));
                        }
                        if let (Owner::Entity(c), true) = (owner, decl.kind == DeclKind::Component) {
                            let bad = if md.mods.is_virtual {
                                Some("virtual")
                            } else if md.mods.is_override {
                                Some("override")
                            } else if md.mods.is_abstract {
                                Some("abstract")
                            } else if !md.type_params.is_empty() {
                                Some("generic")
                            } else {
                                None
                            };
                            if let Some(w) = bad {
                                let note = format!("component methods run on every entity that has '{}'; they are not inherited or overridden", self.p.classes[c].name);
                                self.d.error_note("E0264", md.name_span, format!("component method '{}' cannot be {}", md.name, w), note);
                                continue;
                            }
                        }
                        if !md.type_params.is_empty() {
                            if dup {
                                self.d.error_note("E0104", md.span, format!("duplicate method '{}'", md.name), "a generic method cannot be overloaded: give it its own name");
                            } else {
                                self.register_template(tref, owner, kind, md);
                            }
                            continue;
                        }
                        if dup && self.templates.contains_key(&(tref, md.name.clone())) {
                            self.d.error_note("E0104", md.span, format!("duplicate method '{}'", md.name), "a generic method cannot be overloaded: give it its own name");
                            continue;
                        }
                        let fid = self.declare_func(md, owner, kind);
                        match kind {
                            FuncKind::StructCtor(s) => self.p.structs[s].ctors.push(fid),
                            FuncKind::StructOp(s) => self.p.structs[s].ops.push(fid),
                            FuncKind::StructMethod(s) | FuncKind::StructStatic(s) => self.add_method(TypeRef::Struct(s), fid, md),
                            FuncKind::Static(c) | FuncKind::EntityMethod(c) => self.add_method(TypeRef::Class(c), fid, md),
                        }
                        if md.mods.is_extern {
                                self.declare_extern(fid, md);
                                continue;
                            }
                        if decl.kind == DeclKind::Interface && md.body.is_some() {
                            self.default_src.insert(fid, (md.clone(), vec![]));
                        }
                        match &md.body {
                            Some(_) => bodies.push((fid, owner, md.clone())),
                            None => {
                                if !md.mods.is_abstract {
                                    self.d.error("E0219", md.span, "method requires a body");
                                }
                            }
                        }
                    }
                }
            }
        }
        // every interface's methods are known: structs take the default implementations of the
        // interface methods they do not declare
        self.defaults_open = true;
        let mut with_ifaces: Vec<StructId> = self.p.struct_ifaces.keys().copied().collect();
        with_ifaces.sort();
        for s in with_ifaces {
            self.struct_defaults(s);
        }
        // a struct that contains itself (directly or through other struct fields) has no finite
        // size: C#'s CS0523
        for s in 0..self.p.structs.len() {
            let mut path = vec![];
            if let Some(cycle) = struct_cycle(&self.p, s, s, &mut path) {
                let f = &self.p.structs[s].fields[cycle];
                let names: Vec<String> = std::iter::once(self.p.structs[s].name.clone()).chain(path.iter().map(|&t: &StructId| self.p.structs[t].name.clone())).collect();
                self.d.error_note(
                    "E0257",
                    f.span,
                    format!("struct field '{}.{}' makes the struct contain itself", self.p.structs[s].name, f.name),
                    format!("{} -> {}: a struct is a value, so it cannot hold a value of its own type; store an index, or make it an entity class and store a nullable reference", names.join(" -> "), self.p.structs[s].name),
                );
                // break the cycle: later passes walk struct fields recursively
                self.p.structs[s].fields[cycle].ty = Ty::Error;
            }
        }
        // operator signature sanity
        for s in 0..self.p.structs.len() {
            for &op in &self.p.structs[s].ops.clone() {
                let f = &self.p.funcs[op];
                let mentions = f.params.iter().any(|&l| f.locals[l].ty == Ty::Struct(s));
                if !mentions {
                    let sp = f.span;
                    self.d.error("E0218", sp, "an operator must take its declaring struct as a parameter");
                }
            }
        }
        // override signature checks
        for c in 0..self.p.classes.len() {
            self.override_checks(c);
        }
        // a name is a field or a method of a type, not both
        for sid in 0..self.p.structs.len() {
            let st = &self.p.structs[sid];
            let clash: Vec<(String, Span)> = st.fields.iter().filter(|f| st.methods.contains_key(&f.name)).map(|f| (f.name.clone(), f.span)).collect();
            for (n, sp) in clash {
                let sn = self.p.structs[sid].name.clone();
                self.d.error("E0104", sp, format!("'{}' is both a field and a method of '{}'", n, sn));
            }
        }
        for cid in 0..self.p.classes.len() {
            self.name_clash(cid);
        }
        self.class_checks_done = true;
        if !self.dry.is_empty() {
            self.dry_instances();
        }
        // ---- pass 4: bodies
        for (fid, owner, md) in bodies {
            self.check_body(fid, owner, &md);
        }
        self.check_instances();
        // ---- pass 5: constants and initializers (compile-time evaluated)
        // type-check field initializers first: constants may construct structs whose
        // defaults come from these initializers (and initializers may use constants)
        let mut field_checks: Vec<(TypeRef, usize, Span)> = vec![];
        for (tref, idx, e) in field_inits {
            match tref {
                TypeRef::Struct(s) => {
                    let ty = self.p.structs[s].fields[idx].ty.clone();
                    let he = self.check_detached(tref, &e, &ty);
                    self.p.structs[s].fields[idx].init = Some(he);
                    field_checks.push((tref, idx, e.span));
                }
                TypeRef::Class(_) => {
                    let ty = self.p.fields[idx].ty.clone();
                    // an array field may start with a constant size or constant elements
                    let array_init = matches!(ty, Ty::Array(_) | Ty::MdArray(..)) && matches!(e.kind, ast::ExprKind::NewArray { .. });
                    if ty.is_list() && !array_init {
                        self.d.error("E0221", e.span, "list fields start empty; initialize them in code");
                        continue;
                    }
                    let he = self.check_detached(tref, &e, &ty);
                    self.p.fields[idx].init = Some(he);
                    field_checks.push((tref, idx, e.span));
                }
                TypeRef::Enum(_) => {}
            }
        }
        let mut pending: Vec<(ConstId, Expr)> = vec![];
        for (id, tref, e) in const_inits {
            let ty = self.p.consts[id].ty.clone();
            let he = self.check_detached(tref, &e, &ty);
            pending.push((id, he));
        }
        // constants of generic static class instances, in the instance's type arguments (an
        // initializer may name further instances: until none are left)
        while !self.inst_const_inits.is_empty() {
            for (id, tref, e, env) in std::mem::take(&mut self.inst_const_inits) {
                let ty = self.p.consts[id].ty.clone();
                let saved = std::mem::replace(&mut self.tenv, env);
                let he = self.check_detached(tref, &e, &ty);
                self.tenv = saved;
                pending.push((id, he));
            }
        }
        self.check_instances();
        // constants may refer to each other in any order: iterate to a fixpoint
        loop {
            let before = pending.len();
            let mut rest = vec![];
            for (id, he) in pending {
                match consteval::eval_expr_raw(&self.p, &he) {
                    Ok(v) => self.p.consts[id].value = Some(v),
                    Err(consteval::EvalErr::Pending) => rest.push((id, he)),
                    Err(consteval::EvalErr::NotConst(msg)) => {
                        self.d.error_note("E0221", he.span, "constant initializer is not compile-time evaluable", msg);
                    }
                }
            }
            pending = rest;
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        for (_, he) in pending {
            self.d.error("E0221", he.span, "constant initializer is part of a dependency cycle");
        }
        // `int[K.N] F`: the length is a constant, and an initializer has exactly that many elements
        for (fid, e, tref) in std::mem::take(&mut self.fixed_lens) {
            let he = self.check_detached(tref, &e, &Ty::Int);
            let n = match consteval::eval_expr_raw(&self.p, &he) {
                Ok(ConstVal::Int(n)) if n >= 0 => n,
                Ok(ConstVal::Int(n)) => {
                    self.d.error("E0241", e.span, format!("an array cannot have {} elements", n));
                    continue;
                }
                _ => {
                    self.d.error_note("E0215", e.span, "the length of a fixed-length array field must be a constant", "write a number or a const: public mut int[K.CELLS] Cells;");
                    continue;
                }
            };
            self.p.fields[fid].fixed_len = Some(n);
            let init_len = match self.p.fields[fid].init.as_ref().map(|x| &x.kind) {
                Some(ExprKind::ArrayNew { len: Some(l), .. }) => match consteval::eval_expr_raw(&self.p, l) {
                    Ok(ConstVal::Int(m)) => Some((m as i64, l.span)),
                    _ => None,
                },
                Some(ExprKind::ArrayNew { len: None, elems, .. }) => Some((elems.len() as i64, self.p.fields[fid].init.as_ref().unwrap().span)),
                _ => None,
            };
            if let Some((m, sp)) = init_len {
                if m != n as i64 {
                    self.d.error_note("E0241", sp, format!("'{}' is declared with {} elements, but its initializer has {}", self.p.fields[fid].name, n, m), "give the initializer the declared length, or leave it out (the elements start with their default)");
                }
            }
        }
        field_checks.extend(std::mem::take(&mut self.inst_field_checks).into_iter().map(|(s, i, sp)| (TypeRef::Struct(s), i, sp)));
        field_checks.extend(std::mem::take(&mut self.inst_efield_checks).into_iter().map(|(f, sp)| (TypeRef::Class(self.p.fields[f].class), f, sp)));
        for (tref, idx, sp) in field_checks {
            let he = match tref {
                TypeRef::Struct(s) => self.p.structs[s].fields[idx].init.clone(),
                _ => self.p.fields[idx].init.clone(),
            };
            if let Some(he) = he {
                if let ExprKind::ArrayNew { len, elems, dims, .. } = &he.kind {
                    for x in len.iter().map(|l| &**l).chain(dims.iter()).chain(elems.iter()) {
                        if let Err(msg) = consteval::eval_expr(&self.p, x) {
                            self.d.error_note("E0221", x.span, "the size and elements of an array field's initializer must be compile-time constants", msg);
                        }
                    }
                    continue;
                }
                if let Err(msg) = consteval::eval_expr(&self.p, &he) {
                    self.d.error_note("E0221", sp, "field initializer must be a compile-time constant", msg);
                }
            }
        }
        // persistence: a class whose entities keep [Persistent] fields across runs has one
        // [PersistentKey] in its chain; each such class gets a hidden flag (loaded from the store)
        for c in 0..self.p.classes.len() {
            if !self.p.classes[c].is_entity || self.p.classes[c].is_interface || self.p.classes[c].is_component {
                if self.p.classes[c].is_component {
                    if let Some(&f) = self.p.classes[c].own_fields.iter().find(|&&f| self.p.fields[f].attrs.persistent || self.p.fields[f].attrs.persistent_key) {
                        let sp = self.p.fields[f].span;
                        self.d.error_note("E0269", sp, "a component's fields are not persistent", "keep what outlives the run on the entity class, under its [PersistentKey]");
                    }
                }
                continue;
            }
            let own_keys: Vec<FieldId> = self.p.classes[c].own_fields.iter().copied().filter(|&f| self.p.fields[f].attrs.persistent_key).collect();
            let chain = self.p.all_fields(c);
            let keys: Vec<FieldId> = chain.iter().copied().filter(|&f| self.p.fields[f].attrs.persistent_key).collect();
            if own_keys.len() + keys.len() > own_keys.len() + 1 || own_keys.len() > 1 {
                let sp = self.p.fields[*keys.last().unwrap()].span;
                self.d.error("E0269", sp, format!("'{}' has more than one [PersistentKey]", self.p.classes[c].name));
                continue;
            }
            let pers: Vec<FieldId> = chain.iter().copied().filter(|&f| self.p.fields[f].attrs.persistent).collect();
            if keys.is_empty() {
                if let Some(&f) = self.p.classes[c].own_fields.iter().find(|&&f| self.p.fields[f].attrs.persistent) {
                    let sp = self.p.fields[f].span;
                    let cn = self.p.classes[c].name.clone();
                    self.d.error_note("E0269", sp, format!("'{}' has [Persistent] fields but no [PersistentKey]", cn), "the store keeps an entity's values under its key: add '[PersistentKey] public string Account;' (or another string field)");
                }
                continue;
            }
            if self.p.classes[c].is_abstract {
                continue;
            }
            // the flag lives where the key is declared (subclasses share it)
            let kc = self.p.fields[keys[0]].class;
            let flag = match self.p.classes[kc].own_fields.iter().copied().find(|&f| self.p.fields[f].name == "__persisted") {
                Some(f) => f,
                None => {
                    let span = self.p.classes[kc].span;
                    let e = self.hidden_field(kc, "__persisted".into(), Ty::Bool, span);
                    let ExprKind::EField { field, .. } = e.kind else { continue };
                    field
                }
            };
            self.p.persist.push((c, keys[0], pers, flag));
        }
        // concrete entity classes must implement every abstract method they inherit
        for c in 0..self.p.classes.len() {
            if !self.p.classes[c].is_entity || self.p.classes[c].is_abstract {
                continue;
            }
            let mut cur = self.p.classes[c].base;
            while let Some(b) = cur {
                for bf in self.p.iface_methods(b) {
                    if let Some(f) = self.p.resolve_sig(c, bf) {
                        if self.p.funcs[f].is_abstract {
                            let sp = self.p.classes[c].span;
                            let cn = self.p.classes[c].name.clone();
                            let n = if self.p.overloaded.contains(&self.p.funcs[bf].name) { self.sig_text(bf) } else { self.p.funcs[bf].name.clone() };
                            self.d.error("E0250", sp, format!("'{}' must implement the abstract method '{}'", cn, n));
                        }
                    }
                }
                cur = self.p.classes[b].base;
            }
        }
        // an interface that extends another may declare its methods again (to give one a default
        // implementation, or to take the default away) only with the same signature
        for i in 0..self.p.classes.len() {
            if !self.p.classes[i].is_interface {
                continue;
            }
            for f in self.p.iface_methods(i) {
                let n = self.p.funcs[f].name.clone();
                let Some(g) = self.p.all_ifaces(i).into_iter().find_map(|j| self.p.own_methods_named(j, &n).into_iter().find(|&g| self.p.same_params(f, g) && !same_iface_signature(&self.p, f, g))) else { continue };
                let (fl, gl, sp) = (self.p.func_label(f), self.p.func_label(g), self.p.funcs[f].span);
                self.d.error_note(
                    "E0261",
                    sp,
                    format!("'{}' does not match '{}', which it declares again (parameter types and modes, ranges, return type, 'client' or property differ)", fl, gl),
                    "an interface may declare a method of an interface it extends again (to give it a default implementation) only with the same signature",
                );
            }
        }
        // interfaces: every method of every interface a class or struct implements must be there,
        // with the same signature, or have a default implementation (an abstract class may leave
        // them to its subclasses)
        for c in 0..self.p.classes.len() {
            if !self.p.classes[c].is_entity || self.p.classes[c].is_interface {
                continue;
            }
            let (cn, csp) = (self.p.classes[c].name.clone(), self.p.classes[c].span);
            let ifs = self.one_instance_each(self.p.all_ifaces(c), Owner::Entity(c), &cn, csp);
            // a name the class's interfaces declare with several parameter lists (two instances
            // of one generic interface: Put(int), Put(float)) is overloaded in the class
            let decls: Vec<FuncId> = ifs.iter().flat_map(|&i| self.p.iface_methods(i)).collect();
            self.mark_overloaded(&decls);
            let mut done: Vec<FuncId> = vec![];
            let mut field_done: Vec<String> = vec![];
            for &i in &ifs {
                let mut wants = self.p.iface_methods(i);
                wants.sort_by_key(|&f| (self.p.funcs[f].name.clone(), f));
                for want in wants {
                    let n = self.p.funcs[want].name.clone();
                    let (cn, inn, sp) = (self.p.classes[c].name.clone(), self.p.classes[i].name.clone(), self.p.classes[c].span);
                    // `static abstract`: the class declares a static method with that signature (a
                    // static method with a body belongs to the interface)
                    if matches!(self.p.funcs[want].kind, FuncKind::Static(_)) {
                        if self.p.funcs[want].is_abstract && !self.p.classes[c].is_abstract {
                            let ms: Vec<FuncId> = self.p.methods_named(c, &n).into_iter().filter(|&g| self.p.same_params(g, want) && !matches!(self.p.funcs[g].kind, FuncKind::Static(k) if self.p.classes[k].is_interface)).collect();
                            match ms.first() {
                                Some(&g) if !matches!(self.p.funcs[g].kind, FuncKind::Static(_)) => {
                                    let gsp = self.p.funcs[g].span;
                                    self.d.error_note("E0261", gsp, format!("'{}.{}' implements the static '{}.{}' and must be static", cn, n, inn, n), "write 'public static ...'");
                                }
                                Some(&g) if !same_signature(&self.p, g, want) => {
                                    let gsp = self.p.funcs[g].span;
                                    self.d.error("E0261", gsp, format!("'{}.{}' does not match the static '{}.{}' (parameter types, ranges or return type differ)", cn, n, inn, n));
                                }
                                Some(_) => {}
                                None => self.d.error_note("E0261", sp, format!("'{}' must implement the static '{}.{}'", cn, inn, self.sig_text(want)), format!("add 'public static {} {}' to '{}'", self.p.ty_name(&self.p.funcs[want].ret), self.sig_text(want), cn)),
                            }
                        }
                        continue;
                    }
                    // a name is a field or a method of a type, not both
                    if let Some(fd) = self.p.find_field(c, &n) {
                        let here = self.p.fields[fd].class == c || self.p.classes[c].ifaces.iter().any(|&j| self.p.extends(j, i, 0));
                        if here && !field_done.contains(&n) {
                            field_done.push(n.clone());
                            let fsp = self.p.fields[fd].span;
                            self.d.error_note("E0104", fsp, format!("'{}' is a field of '{}' and a method of its interface '{}'", n, cn, inn), "a name is a field or a method of a type, not both: rename the field");
                        }
                        continue;
                    }
                    let in_chain = |p: &Program| -> Vec<FuncId> {
                        let mut v = vec![];
                        let mut cur = Some(c);
                        while let Some(x) = cur {
                            v.extend(p.own_methods_named(x, &n));
                            cur = p.classes[x].base;
                        }
                        v
                    };
                    let chain = in_chain(&self.p);
                    let wl = if self.p.overloaded.contains(&n) { self.sig_text(want) } else { n.clone() };
                    // without overloads the method of that name implements it; with overloads the
                    // one with its parameter types
                    let imp = if self.p.overloaded.contains(&n) { chain.iter().copied().find(|&f| self.p.same_params(f, want)) } else { chain.first().copied() };
                    match imp {
                        Some(f) if !same_iface_signature(&self.p, f, want) => {
                            let fsp = self.p.funcs[f].span;
                            self.d.error("E0261", fsp, format!("'{}.{}' does not match '{}.{}' (parameter types and modes, ranges, return type, 'client' or property differ)", cn, wl, inn, wl));
                        }
                        Some(f) if self.p.funcs[f].is_abstract && !self.p.classes[c].is_abstract => {
                            self.d.error("E0261", sp, format!("'{}' must implement '{}.{}'", cn, inn, wl));
                        }
                        Some(_) => {}
                        None if done.iter().any(|&g| self.p.funcs[g].name == n && self.p.same_params(g, want)) => {}
                        None => {
                            done.push(want);
                            let ms = self.p.most_specific_sig(&ifs, want);
                            let defaults: Vec<FuncId> = ms.iter().copied().filter(|&f| !self.p.funcs[f].is_abstract).collect();
                            if ms.len() == 1 && defaults.len() == 1 {
                                continue; // the interface's default implementation
                            }
                            if self.p.classes[c].is_abstract {
                                continue;
                            }
                            let shown = if self.p.overloaded.contains(&n) { format!("{} {}", self.p.ty_name(&self.p.funcs[want].ret), self.sig_text(want)) } else { format!("{} {}(...)", self.p.ty_name(&self.p.funcs[want].ret), n) };
                            if defaults.is_empty() {
                                self.d.error_note("E0261", sp, format!("'{}' must implement '{}.{}'", cn, inn, wl), format!("add 'public {}' to '{}' or a base class", shown, cn));
                            } else {
                                let why = self.ambiguous_default(&ms);
                                self.d.error_note("E0261", sp, format!("'{}' must implement '{}': {}", cn, wl, why), format!("declare 'public {}' in '{}' (it may do what one of the defaults does)", shown, cn));
                            }
                        }
                    }
                }
            }
        }
        let mut simpl: Vec<StructId> = self.p.struct_ifaces.keys().copied().collect();
        simpl.sort();
        for s in simpl {
            let (sn, ssp) = (self.p.structs[s].name.clone(), self.p.structs[s].span);
            let ifs = self.one_instance_each(self.p.struct_all_ifaces(s), Owner::Struct(s), &sn, ssp);
            let decls: Vec<FuncId> = ifs.iter().flat_map(|&i| self.p.iface_methods(i)).collect();
            self.mark_overloaded(&decls);
            let mut done: Vec<String> = vec![];
            for &i in &ifs {
                let mut wants = self.p.iface_methods(i);
                wants.sort_by_key(|&f| (self.p.funcs[f].name.clone(), f));
                for want in wants {
                    let n = self.p.funcs[want].name.clone();
                    let key = format!("{}#{:?}", n, self.p.param_sig(want));
                    let (sn, inn, sp) = (self.p.structs[s].name.clone(), self.p.classes[i].name.clone(), self.p.structs[s].span);
                    if matches!(self.p.funcs[want].kind, FuncKind::Static(_)) {
                        if self.p.funcs[want].is_abstract {
                            match self.p.struct_methods_named(s, &n).into_iter().find(|&g| self.p.same_params(g, want)) {
                                Some(g) if !matches!(self.p.funcs[g].kind, FuncKind::StructStatic(_)) => {
                                    let gsp = self.p.funcs[g].span;
                                    self.d.error_note("E0261", gsp, format!("'{}.{}' implements the static '{}.{}' and must be static", sn, n, inn, n), "write 'public static ...'");
                                }
                                Some(g) if !same_signature(&self.p, g, want) => {
                                    let gsp = self.p.funcs[g].span;
                                    self.d.error("E0261", gsp, format!("'{}.{}' does not match the static '{}.{}' (parameter types, ranges or return type differ)", sn, n, inn, n));
                                }
                                Some(_) => {}
                                None => self.d.error_note("E0261", sp, format!("'{}' must implement the static '{}.{}'", sn, inn, self.sig_text(want)), format!("add 'public static {} {}' to '{}'", self.p.ty_name(&self.p.funcs[want].ret), self.sig_text(want), sn)),
                            }
                        }
                        continue;
                    }
                    if done.contains(&n) || done.contains(&key) {
                        continue;
                    }
                    if let Some(fl) = self.p.structs[s].fields.iter().find(|f| f.name == n) {
                        done.push(n.clone());
                        let fsp = fl.span;
                        self.d.error_note("E0104", fsp, format!("'{}' is a field of '{}' and a method of its interface '{}'", n, sn, inn), "a name is a field or a method of a type, not both: rename the field");
                        continue;
                    }
                    if self.p.funcs[want].ctx == Ctx::Client {
                        done.push(n.clone());
                        self.d.error_note("E0261", sp, format!("struct '{}' cannot implement '{}.{}', a client method", sn, inn, n), "client code runs in entity classes; a struct implements interfaces whose methods run on the server");
                        continue;
                    }
                    let own = self.p.struct_methods_named(s, &n);
                    let imp = if self.p.overloaded.contains(&n) { own.iter().copied().find(|&f| self.p.same_params(f, want)) } else { own.first().copied() };
                    let n = if self.p.overloaded.contains(&n) { self.sig_text(want) } else { n };
                    match imp {
                        // the copy of the interface's default implementation
                        Some(f) if self.p.default_copies.contains_key(&f) => {}
                        Some(f) if matches!(self.p.funcs[f].kind, FuncKind::StructStatic(_)) => self.d.error("E0261", self.p.funcs[f].span, format!("'{}.{}' implements '{}.{}' and cannot be static", sn, n, inn, n)),
                        Some(f) if !same_iface_signature(&self.p, f, want) => self.d.error("E0261", self.p.funcs[f].span, format!("'{}.{}' does not match '{}.{}' (parameter types and modes, ranges, return type, 'client' or property differ)", sn, n, inn, n)),
                        Some(_) => {}
                        None => {
                            done.push(key);
                            let ms = self.p.most_specific_sig(&ifs, want);
                            if ms.iter().any(|&f| !self.p.funcs[f].is_abstract) {
                                let why = self.ambiguous_default(&ms);
                                self.d.error_note("E0261", sp, format!("'{}' must implement '{}': {}", sn, n, why), format!("declare 'public {} {}(...)' in '{}' (it may do what one of the defaults does)", self.p.ty_name(&self.p.funcs[want].ret), n, sn));
                            } else {
                                self.d.error("E0261", sp, format!("'{}' must implement '{}.{}'", sn, inn, n));
                            }
                        }
                    }
                }
            }
        }
        // ---- pass 6: entry points and system signatures
        self.check_entry_points();
        // await cycles: a coroutine frame exists once per entity, so A awaiting B awaiting A would
        // resume itself
        let edges = std::mem::take(&mut self.await_edges);
        for &(from, to, sp) in &edges {
            let mut seen = std::collections::BTreeSet::new();
            let mut stack = vec![to];
            while let Some(x) = stack.pop() {
                if x == from {
                    let (a, b) = (self.p.funcs[from].name.clone(), self.p.funcs[to].name.clone());
                    self.d.error_note("E0804", sp, format!("'{}' awaits '{}', which (indirectly) awaits '{}' again", a, b, a), "async methods may not await each other in a cycle");
                    break;
                }
                if seen.insert(x) {
                    stack.extend(edges.iter().filter(|e| e.0 == x).map(|e| e.1));
                }
            }
        }
        // Rush leaves arms running: they are stepped when the coroutine's system runs, so a method
        // that is only driven by an awaiting coroutine (or takes arguments) cannot rush
        for (f, sp) in std::mem::take(&mut self.rush_funcs) {
            if self.p.awaited.contains(&f) || !self.p.funcs[f].params.is_empty() {
                let n = self.p.funcs[f].name.clone();
                self.d.error_note(
                    "E0805",
                    sp,
                    format!("'{}' uses Rush(...) but does not run as its own coroutine system", n),
                    "the arms Rush leaves running are stepped every tick by the coroutine's system: use Rush in a parameterless async method that no other coroutine awaits, or use Race",
                );
            }
        }
        // local constants: their initializers must be known at compile time (now that constants are)
        for (e, sp) in std::mem::take(&mut self.local_consts) {
            if let Err(err) = consteval::eval_expr_raw(&self.p, &e) {
                let why = match err {
                    consteval::EvalErr::NotConst(w) => w,
                    consteval::EvalErr::Pending => "it depends on a value that is not known yet".into(),
                };
                self.d.error_note("E0215", sp, "a local constant needs a value known at compile time", why);
            }
        }
        // ---- pass 7: switch exhaustiveness and reachability (needs constant values)
        let checks = std::mem::take(&mut self.switch_checks);
        for c in &checks {
            self.analyze_switch(c);
        }
        // every lambda and method group is known: the delegates' invoke functions
        self.build_invokes();
    }

    /// The interfaces and the base class of the class `cid` declared by `decl` (a generic class's
    /// declaration for its instances, resolved in the instance's type arguments).
    fn class_header(&mut self, cid: ClassId, decl: &ast::TypeDecl) {
        // the name as declared (an instance's errors name its generic class; a note names the instance)
        let cname = decl.name.clone();
        for te in &decl.ifaces {
            match self.header_iface(te) {
                Ok(i) if !self.p.classes[cid].is_static => self.p.classes[cid].ifaces.push(i),
                Ok(_) => self.d.error("E0213", te.span, format!("static class '{}' cannot implement '{}'", cname, te.name)),
                Err(true) => {}
                Err(false) => self.d.error_note("E0209", te.span, format!("'{}' is not an interface", te.name), "a class has one base class, written first; the other names after ':' are interfaces"),
            }
        }
        if self.p.classes[cid].is_static {
            if let Some(b) = &decl.base {
                self.d.error("E0213", b.span, format!("static class '{}' cannot derive from '{}'", cname, b.name));
            }
            return;
        }
        let is_iface = |this: &Self, n: &str| this.itemplates.contains_key(n) || matches!(this.types.get(n), Some(&TypeRef::Class(i)) if this.p.classes[i].is_interface);
        match &decl.base {
            Some(b) if (b.name == "Entity" || b.name == "Sijo.Entity") && b.args.is_empty() => {}
            // `class IntPool : Pool<int>`: an instance of a generic class
            Some(b) if self.ctemplates.contains_key(&b.name) => {
                let ti = self.ctemplates[&b.name];
                if let Ty::Handle(bc) = self.class_instance(ti, b) {
                    self.p.classes[cid].base = Some(bc);
                }
            }
            // reported at the template (check_class_template)
            Some(b) if b.args.is_empty() && self.tenv.iter().any(|(n, _)| *n == b.name) => {}
            Some(b) if is_iface(self, &b.name) => {
                let written = type_text(b);
                self.d.error_note(
                    "E0209",
                    b.span,
                    format!("class '{}' must derive from Entity or an entity class first ('{}' is an interface)", cname, written),
                    format!("write ': Entity, {}'", written),
                )
            }
            Some(b) if !b.args.is_empty() => match self.types.get(&b.name) {
                Some(&TypeRef::Class(bc)) if self.p.classes[bc].is_entity => {
                    self.d.error_note("E0228", b.span, format!("class '{}' is not generic", b.name), format!("write ': {}' without type arguments", b.name))
                }
                _ => self.d.error("E0209", b.span, format!("'{}' is not an entity class", type_text(b))),
            },
            Some(b) => match self.types.get(&b.name) {
                Some(&TypeRef::Class(bc)) if self.p.classes[bc].is_component => self.d.error_note(
                    "E0209",
                    b.span,
                    format!("class '{}' cannot derive from the component '{}'", cname, b.name),
                    format!("derive from Entity and add the component at run time: e.Add(new {} {{ ... }})", b.name),
                ),
                Some(&TypeRef::Class(bc)) if self.p.classes[bc].is_entity && !self.p.classes[bc].is_interface => {
                    self.p.classes[cid].base = Some(bc);
                    self.reference(b.span, Sym::Class(bc));
                }
                _ => self.d.error("E0209", b.span, format!("'{}' is not an entity class", b.name)),
            },
            None => self.d.error_note(
                "E0209",
                decl.span,
                format!("class '{}' must derive from Entity or be declared static", cname),
                "Sijo has no garbage-collected objects: use 'struct' for values, ': Entity' for world objects",
            ),
        }
    }

    /// Adds a declared method to its struct or class. A second method of one name is an overload
    /// when its parameter types differ (C#'s rule: the return type does not tell overloads apart);
    /// properties, coroutines, `extern` functions and the system methods (`Update`, `Game.Start`)
    /// have one declaration per name.
    fn add_method(&mut self, owner: TypeRef, fid: FuncId, md: &ast::MethodDecl) {
        let existing: Vec<FuncId> = match owner {
            TypeRef::Struct(s) => self.p.struct_methods_named(s, &md.name),
            TypeRef::Class(c) => self.p.own_methods_named(c, &md.name),
            TypeRef::Enum(_) => return,
        };
        if existing.is_empty() {
            match owner {
                TypeRef::Struct(s) => self.p.structs[s].methods.insert(md.name.clone(), fid),
                TypeRef::Class(c) => self.p.classes[c].methods.insert(md.name.clone(), fid),
                TypeRef::Enum(_) => None,
            };
            return;
        }
        if let Some(&g) = existing.iter().find(|&&g| self.p.same_params(fid, g)) {
            let _ = g;
            self.d.error_note(
                "E0104",
                md.name_span,
                format!("duplicate method '{}' with the same parameter types", md.name),
                "overloads differ in the types or modes (ref, out, own) of their parameters; the return type does not tell them apart",
            );
            return;
        }
        if let Some(why) = self.not_overloadable(fid, &existing, owner, md.mods.is_extern) {
            self.d.error_note("E0267", md.name_span, format!("'{}' cannot be overloaded: {}", md.name, why), "give the other method its own name");
            return;
        }
        match owner {
            TypeRef::Struct(s) => self.p.structs[s].overloads.push(fid),
            TypeRef::Class(c) => self.p.classes[c].overloads.push(fid),
            TypeRef::Enum(_) => {}
        }
        self.p.overloaded.insert(md.name.clone());
    }

    /// why method `f` cannot sit beside the other methods of its name (`others`), if it cannot
    fn not_overloadable(&self, f: FuncId, others: &[FuncId], owner: TypeRef, is_extern: bool) -> Option<&'static str> {
        let all: Vec<FuncId> = std::iter::once(f).chain(others.iter().copied()).collect();
        let name = self.p.funcs[f].name.as_str();
        if all.iter().any(|&g| self.p.funcs[g].is_property) {
            return Some("a property is the only member of its name");
        }
        if all.iter().any(|&g| self.p.async_funcs.contains(&g)) {
            return Some("a coroutine's frame is named after its method, so an async method has one declaration");
        }
        if is_extern || all.iter().any(|&g| self.p.funcs[g].extern_name.is_some()) {
            return Some("an extern function is one C function (C has no overloading)");
        }
        let is_game = matches!(owner, TypeRef::Class(c) if self.types.get("Game") == Some(&TypeRef::Class(c)));
        let is_entity = matches!(owner, TypeRef::Class(c) if self.p.classes[c].is_entity);
        if name == "Update" && (is_entity || is_game) {
            return Some("'Update(float dt)' is the system of its class");
        }
        if name == "Start" && is_game {
            return Some("'Game.Start()' is where the program starts");
        }
        None
    }

    /// an override has the signature of the method it overrides
    fn override_checks(&mut self, c: ClassId) {
        let Some(b) = self.p.classes[c].base else { return };
        let mut own = self.p.iface_methods(c);
        own.retain(|&f| matches!(self.p.funcs[f].kind, FuncKind::EntityMethod(_)));
        for fid in own {
            let name = self.p.funcs[fid].name.clone();
            let base_ms = self.p.methods_named(b, &name);
            if base_ms.is_empty() {
                continue;
            }
            // the base method with the same parameter types is the one this overrides
            match base_ms.iter().copied().find(|&bf| self.p.same_params(fid, bf)) {
                Some(bf) if !same_signature(&self.p, fid, bf) => {
                    let sp = self.p.funcs[fid].span;
                    self.d.error_note(
                        "E0220",
                        sp,
                        format!("override of '{}' has a different signature than the base method", name),
                        "parameter types and modes (ref, out, own), ranges, the return type and 'client' must all match",
                    );
                }
                Some(_) => {}
                // other parameter types: a new overload, unless it says it overrides
                None if self.p.funcs[fid].is_override || base_ms.iter().any(|&bf| self.not_overloadable(fid, &[bf], TypeRef::Class(b), false).is_some()) => {
                    let sp = self.p.funcs[fid].span;
                    let sigs: Vec<String> = base_ms.iter().map(|&bf| self.sig_text(bf)).collect();
                    self.d.error_note(
                        "E0220",
                        sp,
                        format!("'{}' has a different signature than the base method", self.sig_text(fid)),
                        format!("the base class declares {}; an override has the same parameter types and modes (ref, out, own), ranges, return type and 'client'", sigs.join(", ")),
                    );
                }
                None => {
                    self.p.overloaded.insert(name);
                }
            }
        }
    }

    /// names that `decls` declare with more than one list of parameter types
    fn mark_overloaded(&mut self, decls: &[FuncId]) {
        for (i, &f) in decls.iter().enumerate() {
            if decls[..i].iter().any(|&g| self.p.funcs[g].name == self.p.funcs[f].name && !self.p.same_params(f, g)) {
                self.p.overloaded.insert(self.p.funcs[f].name.clone());
            }
        }
    }

    /// `Hit(int, float)`: a method's name and parameter types, for messages about overloads
    fn sig_text(&self, f: FuncId) -> String {
        let func = &self.p.funcs[f];
        let ps: Vec<String> = func
            .params
            .iter()
            .map(|&l| {
                let kw = match func.locals[l].kind {
                    LocalKind::Param(ParamMode::Ref) => "ref ",
                    LocalKind::Param(ParamMode::Out) => "out ",
                    LocalKind::Param(ParamMode::Own) => "own ",
                    _ => "",
                };
                format!("{}{}", kw, self.p.ty_name(&func.locals[l].ty))
            })
            .collect();
        format!("{}({})", func.name, ps.join(", "))
    }

    /// a name is a field or a method of a class, not both
    fn name_clash(&mut self, cid: ClassId) {
        let clash: Vec<(String, Span)> = self.p.classes[cid].own_fields.iter().map(|&f| &self.p.fields[f]).filter(|f| self.p.classes[cid].methods.contains_key(&f.name)).map(|f| (f.name.clone(), f.span)).collect();
        for (n, sp) in clash {
            let cn = self.p.classes[cid].name.clone();
            self.d.error("E0104", sp, format!("'{}' is both a field and a method of '{}'", n, cn));
        }
    }

    fn check_entry_points(&mut self) {
        let game = self.types.get("Game").copied();
        match game {
            Some(TypeRef::Class(g)) if self.p.classes[g].is_static => {
                self.p.game = Some(g);
                match self.p.classes[g].methods.get("Start").copied() {
                    Some(f) if self.p.funcs[f].params.is_empty() && self.p.funcs[f].ret == Ty::Void => {
                        self.p.game_start = Some(f)
                    }
                    Some(f) => {
                        let sp = self.p.funcs[f].span;
                        self.d.error("E0222", sp, "Game.Start must be 'static void Start()'")
                    }
                    None => self.d.error("E0222", self.p.classes[g].span, "static class Game must define 'static void Start()'"),
                }
                if let Some(f) = self.p.classes[g].methods.get("Update").copied() {
                    let ok = self.p.funcs[f].params.len() == 1
                        && self.p.funcs[f].locals[self.p.funcs[f].params[0]].ty == Ty::Float
                        && self.p.funcs[f].ret == Ty::Void;
                    if ok {
                        self.p.game_update = Some(f);
                    } else {
                        let sp = self.p.funcs[f].span;
                        self.d.error("E0222", sp, "Game.Update must be 'static void Update(float dt)'");
                    }
                }
            }
            _ => self.d.error("E0222", Span::default(), "program must declare 'static class Game' with 'static void Start()'"),
        }
        for c in 0..self.p.classes.len() {
            if !self.p.classes[c].is_entity {
                continue;
            }
            if let Some(&f) = self.p.classes[c].methods.get("Update") {
                let func = &self.p.funcs[f];
                let ok = func.params.len() == 1 && func.locals[func.params[0]].ty == Ty::Float && func.ret == Ty::Void;
                if !ok {
                    let sp = func.span;
                    self.d.error("E0223", sp, "entity systems must have the signature 'void Update(float dt)'");
                }
                if func.ctx == Ctx::Client {
                    let sp = func.span;
                    self.d.error("E0223", sp, "Update is a server system; use another name for client code");
                }
            }
        }
    }

    /// checks a range `in lo..hi` against the type it constrains; structs (every member) only for fields
    /// the value of a range bound; a constant is looked up in `scope` (or `Type.NAME`) and must be
    /// computed from numbers and other constants
    fn bound_value(&mut self, b: &ast::RangeBound, scope: Option<TypeRef>) -> Option<f64> {
        let (neg, path, span) = match b {
            ast::RangeBound::Num(v) => return Some(*v),
            ast::RangeBound::Const { neg, path, span } => (*neg, path.clone(), *span),
        };
        match self.const_number(&path, scope, span, 0) {
            Ok(v) => Some(if neg { -v.0 } else { v.0 }),
            Err(why) => {
                self.d.error_note("E0224", span, format!("range bound '{}' is not a numeric constant", path), why);
                None
            }
        }
    }

    /// (value, is_int) of constant `path` ("NAME" in `scope`, or "Type.NAME")
    fn const_number(&self, path: &str, scope: Option<TypeRef>, span: Span, depth: u32) -> Result<(f64, bool), String> {
        if depth > 32 {
            return Err("the constants refer to each other in a cycle".into());
        }
        let (tref, name) = match path.rsplit_once('.') {
            Some((t, n)) => match self.types.get(t) {
                Some(&tr) => (tr, n.to_string()),
                None => return Err(format!("there is no type '{}'", t)),
            },
            None => match scope {
                Some(tr) => (tr, path.to_string()),
                None => return Err(format!("write the type: Type.{}", path)),
            },
        };
        let Some(e) = self.const_src.get(&(tref, name.clone())).cloned() else {
            return Err(format!("there is no constant '{}' here (a bound is a number or a const)", path));
        };
        self.const_ast_number(&e, Some(tref), span, depth + 1)
    }

    fn const_ast_number(&self, e: &ast::Expr, scope: Option<TypeRef>, span: Span, depth: u32) -> Result<(f64, bool), String> {
        use ast::ExprKind as K;
        let why = || "a range bound's constant must be computed from numbers and other constants (no calls)".to_string();
        Ok(match &e.kind {
            K::Int(n) => (*n as f64, true),
            K::Float(f, _) => (*f, false),
            K::Unary(ast::UnOp::Neg, x) => {
                let (v, i) = self.const_ast_number(x, scope, span, depth)?;
                (-v, i)
            }
            K::Unary(ast::UnOp::Plus, x) => self.const_ast_number(x, scope, span, depth)?,
            K::Cast(t, x) => {
                let (v, _) = self.const_ast_number(x, scope, span, depth)?;
                match t.name.as_str() {
                    "int" => (v.trunc(), true),
                    "float" => (v as f32 as f64, false),
                    _ => return Err(why()),
                }
            }
            K::Binary(op, a, b) => {
                let (x, xi) = self.const_ast_number(a, scope, span, depth)?;
                let (y, yi) = self.const_ast_number(b, scope, span, depth)?;
                let int = xi && yi;
                let v = match op {
                    ast::BinOp::Add => x + y,
                    ast::BinOp::Sub => x - y,
                    ast::BinOp::Mul => x * y,
                    ast::BinOp::Div if y == 0.0 => return Err("division by zero".into()),
                    ast::BinOp::Div if int => (x / y).trunc(),
                    ast::BinOp::Div => x / y,
                    _ => return Err(why()),
                };
                (if int { v } else { v as f32 as f64 }, int)
            }
            K::Name(n) => self.const_number(n, scope, span, depth)?,
            K::Member(base, n, _) => match &base.kind {
                K::Name(t) => self.const_number(&format!("{}.{}", t, n), scope, span, depth)?,
                _ => return Err(why()),
            },
            _ => return Err(why()),
        })
    }

    fn check_range(&mut self, r: &ast::RangeSpec, ty: &Ty, allow_struct: bool, scope: Option<TypeRef>) -> Option<(f64, f64)> {
        let (lo, hi) = (self.bound_value(&r.lo, scope), self.bound_value(&r.hi, scope));
        let (Some(lo), Some(hi)) = (lo, hi) else { return None };
        if lo > hi {
            self.d.error("E0224", r.span, format!("the range {}..{} is empty: write the smaller bound first", lo, hi));
            return None;
        }
        let r = &RangeVals { lo, hi, span: r.span };
        let ok_ty = match ty {
            Ty::Int | Ty::Float | Ty::Error => true,
            Ty::Struct(s) if allow_struct => self.p.structs[*s].fields.iter().all(|f| f.ty.is_numeric()),
            _ => false,
        };
        if !ok_ty {
            let what = if allow_struct { "int, float or a struct of numbers" } else { "int and float" };
            self.d.error("E0224", r.span, format!("a range applies to {} (this is '{}')", what, self.p.ty_name(ty)));
            return None;
        }
        if *ty == Ty::Int {
            let (ilo, ihi) = (r.lo.ceil(), r.hi.floor());
            if ilo < i32::MIN as f64 || ihi > i32::MAX as f64 {
                self.d.error("E0224", r.span, "the range of an int must lie within the 32-bit int range");
                return None;
            }
            if ilo > ihi {
                self.d.error("E0224", r.span, "the range contains no integer values");
                return None;
            }
        }
        Some((r.lo, r.hi))
    }

    fn scope_of(owner: Owner) -> TypeRef {
        match owner {
            Owner::Struct(s) => TypeRef::Struct(s),
            Owner::Entity(c) | Owner::Static(c) => TypeRef::Class(c),
        }
    }

    /// modifiers that mean nothing on a field
    fn check_field_mods(&mut self, m: &ast::Mods) {
        let sp = m.span.unwrap_or_default();
        for (on, what) in [(m.is_client, "client"), (m.is_extern, "extern"), (m.is_own, "own"), (m.is_async, "async"), (m.is_virtual, "virtual"), (m.is_override, "override"), (m.is_abstract, "abstract")] {
            if on {
                let why = if what == "own" { ": an entity always owns its fields" } else { "" };
                self.d.error("E0028", sp, format!("'{}' does not apply to a field{}", what, why));
            }
        }
    }

    fn field_attrs(&mut self, attrs: &[ast::Attr], range: Option<&ast::RangeSpec>, ty: &Ty, span: Span, scope: TypeRef) -> FieldAttrs {
        let _ = span;
        let mut fa = FieldAttrs::default();
        if let Some(r) = range {
            fa.range = self.check_range(r, ty, true, Some(scope));
        }
        /// does a value of type `t` hold a float (itself or a member at any depth)? None: not known
        /// yet (a struct declared after this entity has no members yet)
        fn holds_float(p: &Program, t: &Ty, depth: u32) -> Option<bool> {
            match t {
                Ty::Float | Ty::Error => Some(true),
                Ty::Struct(s) if depth < 16 && !p.structs[*s].fields.is_empty() => {
                    let mut known = true;
                    for f in &p.structs[*s].fields {
                        match holds_float(p, &f.ty, depth + 1) {
                            Some(true) => return Some(true),
                            Some(false) => {}
                            None => known = false,
                        }
                    }
                    known.then_some(false)
                }
                Ty::Struct(_) => None,
                _ => Some(false),
            }
        }
        if let Some(a) = attr(attrs, "Precision") {
            match a.args.first().and_then(lit_num) {
                Some(p) if p > 0.0 => fa.precision = Some(p),
                _ => self.d.error("E0029", a.span, "[Precision(p)] takes one positive number"),
            }
            if fa.range.is_none() && range.is_none() {
                self.d.warn("W0203", a.span, "[Precision] has no effect without a range: the value is sent as a raw 32-bit float (give it a range: float in lo..hi)");
            }
            if holds_float(&self.p, ty, 0) == Some(false) {
                self.d.error_note(
                    "E0029",
                    a.span,
                    format!("[Precision] applies to float fields and structs with float members (this is '{}')", self.p.ty_name(ty)),
                    "it is the step replicated float values are rounded to; ints are sent exactly",
                );
            }
        }
        if attr(attrs, "Replicated").is_some() {
            fa.replicated = Some(true);
        }
        if attr(attrs, "NotReplicated").is_some() {
            fa.replicated = Some(false);
        }
        if let Some(a) = attr(attrs, "Spatial") {
            fa.spatial = true;
            // the planners read the position as the float X (and Y) members of this field (net.rs, spatial.rs)
            let is_pos = match ty {
                Ty::Struct(s) if self.p.structs[*s].fields.is_empty() => None,
                Ty::Struct(s) => Some(["X", "Y"].iter().all(|n| self.p.structs[*s].fields.iter().any(|f| f.name == *n && f.ty == Ty::Float))),
                Ty::Error => None,
                _ => Some(false),
            };
            if is_pos == Some(false) {
                self.d.error_note(
                    "E0029",
                    a.span,
                    format!("[Spatial] marks the entity's position: a vector field with float X and Y (this is '{}')", self.p.ty_name(ty)),
                    "sharding and interest management place the entity by the X and Y of this field, e.g. 'public mut SVec2 Pos'",
                );
                fa.spatial = false;
            }
        }
        if let Some(a) = attr(attrs, "PersistentKey") {
            fa.persistent_key = true;
            if *ty != Ty::Str && *ty != Ty::Error {
                self.d.error_note("E0269", a.span, format!("[PersistentKey] names an entity in the store with a string (this is '{}')", self.p.ty_name(ty)), "e.g. 'public string Account;' set when the entity is spawned");
            }
        }
        if let Some(a) = attr(attrs, "Persistent") {
            fa.persistent = true;
            if fa.persistent_key {
                self.d.error("E0269", a.span, "the key is not stored as a value: a field is [Persistent] or [PersistentKey]");
            }
            // what outlives the run: values, strings and collections of them
            fn storable(p: &Program, t: &Ty, depth: usize) -> bool {
                if depth > 16 {
                    return false;
                }
                match t {
                    Ty::Handle(_) | Ty::NHandle(_) | Ty::Delegate(_) => false,
                    Ty::Struct(s) => p.structs[*s].fields.iter().all(|f| storable(p, &f.ty, depth + 1)),
                    Ty::List(e) | Ty::Array(e) | Ty::MdArray(e, _) => storable(p, e, depth + 1),
                    Ty::Dict(k, v) => storable(p, k, depth + 1) && storable(p, v, depth + 1),
                    _ => true,
                }
            }
            if !storable(&self.p, ty, 0) {
                self.d.error_note("E0269", a.span, format!("a [Persistent] field cannot hold '{}'", self.p.ty_name(ty)), "entity references and delegates mean nothing in another run: store a key, an id or the values themselves");
            }
        }
        if let Some(a) = attr(attrs, "Component") {
            if let Some(ast::Expr { kind: ast::ExprKind::Str(s), .. }) = a.args.first() {
                fa.component = Some(s.clone());
            } else {
                self.d.error("E0029", a.span, "[Component(\"Name\")] takes one string");
            }
        }
        fa
    }

    /// `public static extern float Now();` (optionally `extern("c_name")`)
    fn declare_extern(&mut self, fid: FuncId, md: &ast::MethodDecl) {
        let name = match &md.mods.extern_name {
            Some((s, _)) => s.clone(),
            None => md.name.clone(),
        };
        let valid_c = name.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        let f = &self.p.funcs[fid];
        let mut why = vec![];
        if !matches!(f.kind, FuncKind::Static(_)) {
            why.push("it must be a static method of a static class".to_string());
        }
        if md.body.is_some() {
            why.push("it must not have a body (the host program defines it)".into());
        }
        if !valid_c {
            why.push(format!("'{}' is not a valid C identifier", name));
        }
        // the C name sits next to the runtime, the C library and the generated code in one C file
        const C_KEYWORDS: &[&str] = &[
            "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern", "float", "for", "goto", "if",
            "inline", "int", "long", "register", "restrict", "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union",
            "unsigned", "void", "volatile", "while", "bool", "true", "false", "nullptr", "alignas", "alignof", "constexpr", "static_assert",
            "thread_local", "typeof", "typeof_unqual", "asm",
        ];
        const C_LIBRARY: &[&str] = &[
            // stdio.h, stdlib.h, string.h
            "NULL", "errno", "stdin", "stdout", "stderr", "printf", "fprintf", "sprintf", "snprintf", "vprintf", "vfprintf", "vsnprintf", "scanf", "sscanf", "fscanf", "puts", "fputs", "putchar",
            "fputc", "putc", "getchar", "fgetc", "getc", "fgets", "fopen", "fclose", "fread", "fwrite", "fflush", "fseek", "ftell", "rewind",
            "remove", "rename", "perror", "setvbuf", "tmpfile", "malloc", "calloc", "realloc", "free", "exit", "abort", "atexit", "_Exit",
            "_exit", "quick_exit", "getenv", "system", "atoi", "atol", "atoll", "atof", "strtol", "strtoll", "strtoul", "strtoull", "strtod",
            "strtof", "rand", "srand", "qsort", "bsearch", "abs", "labs", "llabs", "div", "memcpy", "memmove", "memset", "memcmp", "memchr",
            "strlen", "strcmp", "strncmp", "strcpy", "strncpy", "strcat", "strncat", "strchr", "strrchr", "strstr", "strdup", "strerror",
            "strtok", "strspn", "strcspn",
            // math.h
            "sin", "cos", "tan", "asin", "acos", "atan", "atan2", "sinh", "cosh", "tanh", "asinh", "acosh", "atanh", "exp", "exp2", "log",
            "log2", "log10", "pow", "sqrt", "cbrt", "hypot", "ceil", "floor", "round", "trunc", "fmod", "fabs", "fmin", "fmax", "copysign",
            "sinf", "cosf", "tanf", "asinf", "acosf", "atanf", "atan2f", "expf", "logf", "log2f", "powf", "sqrtf", "ceilf", "floorf",
            "roundf", "truncf", "fmodf", "fabsf", "fminf", "fmaxf", "copysignf", "isnan", "isinf", "signbit",
            // time.h, setjmp.h, signal.h and the POSIX calls the networking runtime makes
            "time", "clock", "difftime", "mktime", "localtime", "gmtime", "strftime", "nanosleep", "clock_gettime", "setjmp", "longjmp",
            "signal", "raise", "sigaction", "read", "write", "open", "close", "sleep", "usleep", "getpid", "fcntl", "ioctl", "socket",
            "bind", "connect", "listen", "accept", "send", "recv", "sendto", "recvfrom", "poll", "select", "getaddrinfo", "freeaddrinfo",
            "setsockopt", "getsockname", "htons", "htonl", "ntohs", "ntohl",
        ];
        let generated = {
            // F12_Game_Start, M3_Ship_Update..., R4, S4, sys0, acc_..., ARCH_...
            let b = name.as_bytes();
            let digit_after = |k: usize| b.len() > k && b[k].is_ascii_digit();
            (matches!(b.first(), Some(b'F' | b'M')) && digit_after(1) && name[1..].trim_start_matches(|c: char| c.is_ascii_digit()).starts_with('_'))
                || (matches!(b.first(), Some(b'R' | b'S')) && digit_after(1) && name[1..].chars().all(|c| c.is_ascii_digit()))
                || (name.starts_with("sys") && digit_after(3))
                || name.starts_with("acc_")
                || name.starts_with("ARCH_")
        };
        let clash = if !valid_c {
            None
        } else if C_KEYWORDS.contains(&name.as_str()) {
            Some("is a C keyword")
        } else if name.starts_with("sijo_") || name.starts_with("Sijo") || name.starts_with("SIJO_") || name.starts_with("omp_") {
            Some("is reserved for the Sijo runtime (names starting with 'sijo_', 'Sijo', 'SIJO_' or 'omp_')")
        } else if name == "main" {
            Some("is the C program's entry point")
        } else if C_LIBRARY.contains(&name.as_str()) {
            Some("is defined by the C library")
        } else if name.starts_with("__") || (name.starts_with('_') && name[1..].starts_with(|c: char| c.is_ascii_uppercase())) {
            Some("is reserved in C (names starting with '__' or '_' and a capital letter)")
        } else if generated {
            Some("is a name of the generated C code")
        } else {
            None
        };
        if let Some(c) = clash {
            let whose = if md.mods.extern_name.is_some() { "the C name" } else { "its C name (the method name)" };
            why.push(format!("{} '{}' {}: choose another, e.g. extern(\"host_{}\")", whose, name, c, name.trim_start_matches('_')));
        }
        if !matches!(f.ret, Ty::Void | Ty::Int | Ty::Float | Ty::Bool | Ty::Enum(_)) {
            why.push("the return type must be void, int, float, bool or an enum".into());
        }
        for &l in &f.params {
            let loc = &f.locals[l];
            // an array or list of numbers is lent to the host as a pointer and a length (C: `const
            // int32_t* xs, int32_t xs_len`), like a string: read-only and only during the call
            let scalar = |t: &Ty| matches!(t, Ty::Int | Ty::Float | Ty::Bool | Ty::Enum(_) | Ty::Char);
            let ok_ty = matches!(loc.ty, Ty::Int | Ty::Float | Ty::Bool | Ty::Enum(_) | Ty::Str)
                || matches!(&loc.ty, Ty::Array(e) | Ty::List(e) if scalar(e));
            if !ok_ty || !matches!(loc.kind, LocalKind::Param(ParamMode::Value)) {
                why.push(format!("parameter '{}' must be a plain int, float, bool, enum, string, or an array or list of int, float, bool, char or enum", loc.name));
            }
        }
        if f.deferred_attr && f.ret != Ty::Void {
            why.push("[Deferred] needs a void extern: its calls are made later, at the end of the system, so nothing can wait for a result".into());
        }
        if f.deferred_attr && f.pure_attr {
            why.push("[Deferred] and [Pure] do not go together: a pure function has no effects to queue".into());
        }
        if !why.is_empty() {
            self.d.error_note("E0254", md.name_span, format!("invalid extern function '{}'", md.name), why.join("; "));
        }
        self.p.funcs[fid].extern_name = Some(name);
    }

    fn declare_func(&mut self, md: &ast::MethodDecl, owner: Owner, kind: FuncKind) -> FuncId {
        let ret = if md.kind == MethodKind::Ctor { Ty::Void } else { self.resolve_type(&ast::TypeExpr { range: None, ..md.ret.clone() }) };
        let ret_range = match &md.ret.range {
            Some(r) => self.check_range(r, &ret, false, Some(Self::scope_of(owner))),
            None => None,
        };
        // ownership: a returned list, array or dictionary always goes to the caller, and says so
        let msp = md.mods.span.unwrap_or(md.name_span);
        if ret.is_list() && !md.mods.is_own && !md.mods.is_extern {
            self.d.error_note(
                "E0505",
                md.ret.span,
                format!("'{}' returns a {}: write 'own {}' to show that the caller receives it", md.name, coll_word(&ret), self.p.ty_name(&ret)),
                "the caller owns every returned collection (and frees it); 'own' makes that visible in the signature",
            );
        }
        if md.mods.is_own && !ret.is_list() && ret != Ty::Error {
            self.d.error_note("E0505", msp, "'own' applies to a returned list, array or dictionary", "other values are copied: remove 'own'");
        }
        // an interface's default implementation may be 'mut': the structs that use it get a 'mut'
        // copy (it may call their 'mut' methods)
        let iface_default = matches!(owner, Owner::Entity(c) if self.p.classes[c].is_interface) && md.body.is_some();
        if md.mods.is_mut && matches!(owner, Owner::Entity(c) if self.p.classes[c].is_interface) && md.body.is_none() {
            self.d.error_note(
                "E0028",
                msp,
                "'mut' on an interface method applies to a default implementation that changes 'this' of the structs that use it",
                "a signature leaves that to each implementation: a struct declares its own method 'mut' when it changes 'this'",
            );
        } else if md.mods.is_mut && !matches!(kind, FuncKind::StructMethod(_)) && !iface_default {
            self.d.error_note("E0028", msp, "'mut' on a method applies to struct methods that change 'this'", "entity methods change their own 'mut' fields; static methods have no 'this'");
        }
        if md.mods.is_client && !matches!(owner, Owner::Entity(_) | Owner::Static(_)) {
            self.d.error("E0603", msp, "'client' applies to entity or static methods");
        }
        self.check_comp_storage(&ret, md.ret.span, "a return value");
        let mut locals = vec![];
        let mut params = vec![];
        for p in &md.params {
            let ty = self.resolve_type(&ast::TypeExpr { range: None, ..p.ty.clone() });
            if ty == Ty::Void {
                self.d.error("E0225", p.span, "parameter cannot be void");
            }
            self.check_comp_storage(&ty, p.ty.span, "a parameter");
            let mut range = None;
            if let Some(r) = &p.ty.range {
                if p.mode == ParamMode::Value {
                    range = self.check_range(r, &ty, false, Some(Self::scope_of(owner)));
                } else {
                    self.d.error_note("E0224", r.span, "a range applies to a value parameter", "a 'ref'/'out' argument keeps the range of the variable it refers to");
                }
            }
            if p.mode == ParamMode::Own && !ty.is_list() && ty != Ty::Error {
                self.d.error_note("E0505", p.span, "'own' applies to lists, arrays and dictionaries", "other values are copied into the call: remove 'own'");
            }
            if p.mode == ParamMode::Own && md.mods.is_async {
                self.d.error("E0505", p.span, "an async method cannot take ownership of a collection (its parameters live in the entity across ticks)");
            }
            if p.is_mut && md.mods.is_async {
                self.d.error_note("E0028", p.span, "parameters of an async method take no 'mut'", "they live in the entity across ticks and are read-only: copy one into a 'mut' local to change it");
            }
            if p.is_mut {
                if matches!(p.mode, ParamMode::Ref | ParamMode::Out) {
                    self.d.error_note("E0028", p.span, format!("'{}' parameters take no 'mut'", if p.mode == ParamMode::Ref { "ref" } else { "out" }), "they already write the caller's variable");
                } else if p.mode == ParamMode::Value && ty.is_list() {
                    self.d.error_note("E0028", p.span, "a borrowed collection cannot be 'mut'", "take it with 'mut own' (the caller hands it over) or 'ref' (change the caller's)");
                }
            }
            if p.mode == ParamMode::Out && matches!(ty, Ty::Handle(_)) {
                self.d.error("E0303", p.span, "'out' parameters cannot be non-nullable handles");
            }
            if locals.iter().any(|l: &Local| l.name == p.name) {
                self.d.error("E0103", p.span, format!("duplicate parameter '{}'", p.name));
            }
            locals.push(Local { name: p.name.clone(), ty, kind: LocalKind::Param(p.mode), span: p.span, range, is_mut: p.is_mut, written: false, is_const: false });
            params.push(locals.len() - 1);
        }
        if matches!(md.kind, MethodKind::Operator(_) | MethodKind::Ctor) {
            for p in &md.params {
                if matches!(p.mode, ParamMode::Ref | ParamMode::Out) {
                    self.d.error("E0251", p.span, "constructors and operators cannot take ref/out parameters");
                }
            }
        }
        if let MethodKind::Operator(op) = &md.kind {
            let arity_ok = match op.as_str() {
                "+" | "-" => params.len() == 1 || params.len() == 2,
                "!" | "~" => params.len() == 1,
                _ => params.len() == 2,
            };
            if !arity_ok || !md.mods.is_static {
                self.d.error("E0218", md.span, format!("operator {} must be 'public static' with the right number of parameters", op));
            }
        }
        let fid_next = self.p.funcs.len();
        for (i, p) in md.params.iter().enumerate() {
            if i < params.len() {
                self.def(p.name_span, Sym::Local(fid_next, params[i]));
            }
        }
        match (&md.kind, owner) {
            (MethodKind::Operator(_), _) => {}
            // a constructor's name is a reference to its struct (renaming the type renames it)
            (MethodKind::Ctor, Owner::Struct(s)) => self.reference(md.name_span, Sym::Struct(s)),
            _ => self.def(md.name_span, Sym::Func(fid_next)),
        }
        let client = md.mods.is_client;
        if md.is_property && md.mods.is_static {
            self.d.error("E0226", md.span, "static properties are not supported; use a const or a method");
        }
        if md.mods.is_async {
            self.p.async_funcs.insert(self.p.funcs.len());
        }
        self.p.funcs.push(Func {
            name: md.name.clone(),
            kind,
            params,
            ret,
            locals,
            body: Block::default(),
            ctx: if client { Ctx::Client } else { Ctx::Server },
            pure_attr: attr(&md.attrs, "Pure").is_some(),
            deferred_attr: attr(&md.attrs, "Deferred").is_some(),
            is_property: md.is_property,
            is_override: md.mods.is_override,
            is_abstract: md.mods.is_abstract && md.body.is_none(),
            span: md.span,
            // a struct method changes `this` exactly when it is declared `mut` (known before bodies)
            writes_self: matches!(kind, FuncKind::StructMethod(_)) && md.mods.is_mut,
            co: None,
            extern_name: None,
            ret_range,
            recursive_attr: attr(&md.attrs, "Recursive").is_some(),
            name_span: md.name_span,
            synth: None,
        });
        self.p.funcs.len() - 1
    }

    // ======================================================================
    // generic methods
    // ======================================================================

    fn register_template(&mut self, tref: TypeRef, owner: Owner, kind: FuncKind, md: &ast::MethodDecl) {
        let mut why = vec![];
        if md.mods.is_virtual || md.mods.is_override || md.mods.is_abstract {
            why.push("it cannot be virtual, override or abstract (each instance is a separate method)");
        }
        if md.mods.is_async {
            why.push("it cannot be async (a coroutine has one frame per entity)");
        }
        if md.mods.is_extern {
            why.push("it cannot be extern (C has no generics)");
        }
        if !why.is_empty() {
            self.d.error("E0258", md.name_span, format!("generic method '{}': {}", md.name, why.join("; ")));
            return;
        }
        if md.body.is_none() {
            self.d.error("E0219", md.span, "method requires a body");
            return;
        }
        let mut seen: Vec<&str> = vec![];
        for (n, sp) in &md.type_params {
            if seen.contains(&n.as_str()) {
                self.d.error("E0103", *sp, format!("duplicate type parameter '{}'", n));
                return;
            }
            let builtin = matches!(n.as_str(), "int" | "float" | "bool" | "void" | "string" | "char" | "List" | "Dictionary" | "Entity");
            if builtin || self.types.contains_key(n) {
                self.d.error("E0258", *sp, format!("type parameter '{}' has the name of a type", n));
                return;
            }
            seen.push(n);
        }
        self.check_where(&md.type_params, &md.constraints, &md.name);
        self.tmpls.push(Template { md: md.clone(), owner, kind, instances: vec![], outer: vec![], quiet: false });
        self.templates.insert((tref, md.name.clone()), self.tmpls.len() - 1);
    }

    /// An interface holds method signatures and default implementations: a signature becomes an
    /// abstract method of the interface, a method with a body (or a get-only property `int P =>
    /// ...;`) its default implementation. No fields, static members, constructors, operators,
    /// generic or async methods.
    fn interface_members(&mut self, decl: &ast::TypeDecl) -> Vec<ast::Member> {
        let mut out = vec![];
        for m in &decl.members {
            match m {
                ast::Member::Field(f) => self.d.error_note("E0262", f.span, format!("interface '{}' cannot have fields", decl.name), "an interface lists methods; put the data in the classes that implement it (an interface asks for a value with a method: 'int Hp();')"),
                ast::Member::Method(md) => {
                    let why = if md.mods.is_static && md.mods.is_virtual {
                        Some("'static virtual' is not supported: declare 'static abstract' (each implementing type provides it) or a static method with a body (a function of the interface)")
                    } else if md.mods.is_static && (md.mods.is_client || md.is_property) {
                        Some("a static interface member is a plain static method (no 'client', no property)")
                    } else if md.kind != MethodKind::Normal {
                        Some("an interface declares methods only")
                    } else if !md.type_params.is_empty() {
                        Some("interface methods cannot be generic (make the interface generic: interface I<T>)")
                    } else if md.mods.is_async {
                        Some("interface methods cannot be async")
                    } else {
                        None
                    };
                    match why {
                        Some(w) => self.d.error("E0262", md.name_span, format!("'{}.{}': {}", decl.name, md.name, w)),
                        None => {
                            let mut md = md.clone();
                            // a signature; a body is the default implementation (for a static
                            // method: a function of the interface itself, `I.M()`)
                            md.mods.is_abstract = md.body.is_none();
                            out.push(ast::Member::Method(md));
                        }
                    }
                }
            }
        }
        out
    }

    /// type parameters of a generic interface or class: distinct, and not the names of types
    /// (false: an error was reported)
    fn check_type_params(&mut self, tps: &[(String, Span)]) -> bool {
        let mut ok = true;
        let mut seen: Vec<&str> = vec![];
        for (n, sp) in tps {
            if seen.contains(&n.as_str()) {
                self.d.error("E0103", *sp, format!("duplicate type parameter '{}'", n));
                ok = false;
                continue;
            }
            let builtin = matches!(n.as_str(), "int" | "float" | "bool" | "void" | "string" | "char" | "List" | "Dictionary" | "Entity");
            if builtin || self.types.contains_key(n) || self.stemplates.contains_key(n) || self.itemplates.contains_key(n) || self.ctemplates.contains_key(n) {
                self.d.error("E0258", *sp, format!("type parameter '{}' has the name of a type", n));
                ok = false;
            }
            seen.push(n);
        }
        ok
    }

    /// An entry of a `: A, I<int>` list that must name an interface: the interface (the instance,
    /// for a generic one). `Err(true)`: an error was reported; `Err(false)`: it is not an
    /// interface (the caller says why).
    fn header_iface(&mut self, te: &ast::TypeExpr) -> Result<ClassId, bool> {
        let name_sp = Span::new(te.span.lo as usize, te.span.lo as usize + te.name.len());
        if let Some(&ti) = self.itemplates.get(&te.name) {
            return match self.iface_instance(ti, te) {
                Ty::Handle(c) => Ok(c),
                _ => Err(true),
            };
        }
        match self.types.get(&te.name).copied() {
            Some(TypeRef::Class(i)) if self.p.classes[i].is_interface => {
                if !te.args.is_empty() {
                    self.d.error("E0228", te.span, format!("interface '{}' is not generic", te.name));
                    return Err(true);
                }
                self.reference(name_sp, Sym::Class(i));
                Ok(i)
            }
            None if !te.args.is_empty() && !self.stemplates.contains_key(&te.name) => {
                self.d.error_note(
                    "E0105",
                    name_sp,
                    format!("unknown type '{}'", te.name),
                    "Sijo has no built-in .NET interfaces (IEquatable<T>, IComparable<T>, ...): declare the interface in the program, e.g. 'interface IEquatable<T> { bool Same(T other); }'",
                );
                Err(true)
            }
            _ => Err(false),
        }
    }

    /// `IContainer<int>`: the instance of a generic interface for these type arguments, declared
    /// on first use (extended interfaces now; methods now, or when pass 3 starts; default
    /// implementations checked later, like generic method instances).
    fn iface_instance(&mut self, ti: usize, t: &ast::TypeExpr) -> Ty {
        let names: Vec<String> = self.itmpls[ti].decl.type_params.iter().map(|(n, _)| n.clone()).collect();
        let tname = self.itmpls[ti].decl.name.clone();
        if t.args.len() != names.len() {
            self.d.error_note(
                "E0228",
                t.span,
                format!("'{}' takes {} type argument(s), found {}", tname, names.len(), t.args.len()),
                format!("write them after the name: {}<{}>", tname, names.join(", ")),
            );
            return Ty::Error;
        }
        let sp = Span::new(t.span.lo as usize, t.span.lo as usize + tname.len());
        let tys: Vec<Ty> = t.args.iter().map(|a| self.resolve_type(a)).collect();
        if tys.iter().any(|x| *x == Ty::Error) {
            return Ty::Error;
        }
        if let Some(x) = tys.iter().find(|x| matches!(x, Ty::Void | Ty::Null)) {
            let xn = self.p.ty_name(x);
            self.d.error("E0259", t.span, format!("'{}' is not a valid type argument", xn));
            return Ty::Error;
        }
        let cons = self.itmpls[ti].decl.constraints.clone();
        if !self.satisfies_where(&cons, &names, &tys, &tname, t.span) {
            return Ty::Error;
        }
        let wrap = |c: ClassId| if t.nullable { Ty::NHandle(c) } else { Ty::Handle(c) };
        if let Some(&(_, c)) = self.itmpls[ti].instances.iter().find(|(k, _)| *k == tys) {
            let first = self.itmpls[ti].instances[0].1;
            self.reference(sp, Sym::Class(first));
            return wrap(c);
        }
        if self.itmpls[ti].instances.len() >= MAX_INSTANCES {
            self.d.error_note("E0259", t.span, format!("too many instances of the generic interface '{}'", tname), "a generic interface that extends itself with ever larger type arguments (I<List<T>>) never ends");
            return Ty::Error;
        }
        let before = self.d.list.len();
        let labels: Vec<String> = tys.iter().map(|x| self.p.ty_name(x)).collect();
        let decl = self.itmpls[ti].decl.clone();
        let order = self.itmpls[ti].order;
        self.p.classes.push(ClassDef {
            name: format!("{}<{}>", tname, labels.join(", ")),
            span: decl.span,
            base: None,
            is_entity: true,
            is_static: false,
            is_abstract: true,
            is_interface: true,
            ifaces: vec![],
            own_fields: vec![],
            methods: HashMap::new(), overloads: vec![],
            order,
            run_order: 0,
            is_component: false,
            combo_of: None,
            comps: vec![],
        });
        let c = self.p.classes.len() - 1;
        let first = self.itmpls[ti].instances.first().map(|x| x.1).unwrap_or(c);
        self.itmpls[ti].instances.push((tys.clone(), c));
        self.iface_inst.insert(c, (ti, tys.clone()));
        self.p.iface_family.insert(c, first);
        // the declaration's names belong to the first instance (whatever code first used it)
        let saved = (std::mem::replace(&mut self.tenv, names.into_iter().zip(tys).collect()), self.quiet);
        self.quiet = first != c;
        self.def(decl.name_span, Sym::Class(c));
        for te in decl.base.iter().chain(decl.ifaces.iter()) {
            match self.header_iface(te) {
                Ok(i) => self.p.classes[c].ifaces.push(i),
                Err(true) => {}
                Err(false) => self.d.error("E0209", te.span, format!("interface '{}' can only extend interfaces ('{}' is not one)", tname, te.name)),
            }
        }
        self.tenv = saved.0;
        self.quiet = saved.1;
        self.reference(sp, Sym::Class(first));
        if self.p.classes[c].ifaces.clone().iter().any(|&i| self.p.extends(i, c, 0)) {
            self.d.error("E0214", decl.span, format!("interface inheritance cycle ('{}' extends itself)", self.p.classes[c].name));
            self.p.classes[c].ifaces.clear();
        }
        if self.members_open {
            self.iface_members_of(c);
        } else {
            self.iface_pending.push(c);
        }
        self.dedupe_since(before);
        wrap(c)
    }

    /// The methods of a generic interface instance, declared with its type arguments; default
    /// implementations are checked with the generic method instances.
    fn iface_members_of(&mut self, c: ClassId) {
        let (ti, tys) = self.iface_inst[&c].clone();
        let names: Vec<String> = self.itmpls[ti].decl.type_params.iter().map(|(n, _)| n.clone()).collect();
        let env: Vec<(String, Ty)> = names.into_iter().zip(tys).collect();
        let first = self.p.iface_family.get(&c) == Some(&c);
        let before = self.d.list.len();
        let saved = (std::mem::replace(&mut self.tenv, env.clone()), self.quiet);
        self.quiet = !first;
        let members = self.itmpls[ti].members.clone();
        for m in &members {
            let ast::Member::Method(md) = m else { continue };
            let (owner, kind) = if md.mods.is_static { (Owner::Static(c), FuncKind::Static(c)) } else { (Owner::Entity(c), FuncKind::EntityMethod(c)) };
            let fid = self.declare_func(md, owner, kind);
            self.add_method(TypeRef::Class(c), fid, md);
            if md.mods.is_static && md.body.is_some() {
                let note = format!("in '{}.{}', an instance of this generic interface", self.p.classes[c].name, md.name);
                self.inst_queue.push(Instance { fid, owner, md: md.clone(), env: env.clone(), quiet: !first, note: Some(note) });
            } else if md.body.is_some() {
                self.default_src.insert(fid, (md.clone(), env.clone()));
                let note = format!("in '{}.{}', an instance of this generic interface", self.p.classes[c].name, md.name);
                self.inst_queue.push(Instance { fid, owner: Owner::Entity(c), md: md.clone(), env: env.clone(), quiet: !first, note: Some(note) });
            }
        }
        self.tenv = saved.0;
        self.quiet = saved.1;
        self.dedupe_since(before);
    }

    /// Removes the diagnostics reported since `before` that repeat an earlier one (an error in a
    /// generic declaration that does not depend on the type arguments is reported once).
    fn dedupe_since(&mut self, before: usize) {
        let mut i = before;
        while i < self.d.list.len() {
            let x = &self.d.list[i];
            if self.d.list[..i].iter().any(|y| y.code == x.code && y.span == x.span && y.msg == x.msg) {
                self.d.list.remove(i);
                continue;
            }
            i += 1;
        }
    }

    /// A struct that implements an interface uses the interface's default implementation of each
    /// method it does not declare: a copy of the default becomes a method of the struct, checked
    /// against the struct and called statically (a struct value is never an interface value).
    fn struct_defaults(&mut self, s: StructId) {
        let ifs = self.p.struct_all_ifaces(s);
        let mut decls: Vec<FuncId> = ifs.iter().flat_map(|&i| self.p.iface_methods(i)).filter(|&f| !matches!(self.p.funcs[f].kind, FuncKind::Static(_))).collect();
        decls.sort_by_key(|&f| (self.p.funcs[f].name.clone(), f));
        let mut done: Vec<FuncId> = vec![];
        for want in decls {
            let n = self.p.funcs[want].name.clone();
            if done.iter().any(|&g| self.p.funcs[g].name == n && self.p.same_params(g, want)) {
                continue;
            }
            done.push(want);
            if self.p.struct_methods_named(s, &n).iter().any(|&g| self.p.same_params(g, want)) || self.p.structs[s].fields.iter().any(|f| f.name == n) {
                continue;
            }
            let [d] = self.p.most_specific_sig(&ifs, want)[..] else { continue };
            if self.p.funcs[d].is_abstract || self.p.funcs[d].ctx == Ctx::Client {
                continue;
            }
            let Some((md, env)) = self.default_src.get(&d).cloned() else { continue };
            let saved = (std::mem::replace(&mut self.tenv, env.clone()), self.quiet);
            self.quiet = true;
            // the signature was checked with the interface
            let before = self.d.list.len();
            let fid = self.declare_func(&md, Owner::Struct(s), FuncKind::StructMethod(s));
            self.d.list.truncate(before);
            self.tenv = saved.0;
            self.quiet = saved.1;
            if self.p.structs[s].methods.contains_key(&n) {
                self.p.structs[s].overloads.push(fid);
                self.p.overloaded.insert(n.clone());
            } else {
                self.p.structs[s].methods.insert(n.clone(), fid);
            }
            self.p.default_copies.insert(fid, d);
            let note = format!("in the default implementation '{}' that struct '{}' uses", self.p.func_label(d), self.p.structs[s].name);
            self.inst_queue.push(Instance { fid, owner: Owner::Struct(s), md, env, quiet: true, note: Some(note) });
        }
    }

    /// A class or struct has one method of each name, so it implements two instances of one
    /// generic interface only when their methods are the same (signatures that do not mention the
    /// type parameter, no default implementations). Of two that are not, the one its methods
    /// match is kept (the first written, if both or neither); the other is reported once.
    fn one_instance_each(&mut self, ifs: Vec<ClassId>, owner: Owner, who: &str, sp: Span) -> Vec<ClassId> {
        // how many methods of interface `i` the type's own methods do not match
        let misses = |p: &Program, i: ClassId| -> usize {
            p.iface_methods(i)
                .into_iter()
                .filter(|&f| {
                    let n = &p.funcs[f].name;
                    let imp = match owner {
                        Owner::Entity(c) => p.methods_named(c, n).into_iter().find(|&g| p.same_params(f, g) && !p.classes[match p.funcs[g].kind { FuncKind::EntityMethod(k) => k, _ => c }].is_interface),
                        Owner::Struct(s) => p.struct_methods_named(s, n).into_iter().find(|&g| p.same_params(f, g)),
                        Owner::Static(_) => None,
                    };
                    imp.map(|g| !same_iface_signature(p, g, f)).unwrap_or(false)
                })
                .count()
        };
        let mut skip: Vec<ClassId> = vec![];
        for (k, &i) in ifs.iter().enumerate() {
            let Some(&fam) = self.p.iface_family.get(&i) else { continue };
            if skip.contains(&i) {
                continue;
            }
            for &j in &ifs[k + 1..] {
                if skip.contains(&j) || self.p.iface_family.get(&j) != Some(&fam) {
                    continue;
                }
                // methods with other parameter types are overloads of one another; one method
                // cannot implement two with the same parameters unless they are the same signature
                // without default implementations
                let mut mine = self.p.iface_methods(i);
                // a static method with a body belongs to its interface instance
                mine.retain(|&f| !(matches!(self.p.funcs[f].kind, FuncKind::Static(_)) && !self.p.funcs[f].is_abstract));
                mine.sort_by_key(|&f| (self.p.funcs[f].name.clone(), f));
                let clash = mine.into_iter().find(|&f| {
                    let n = &self.p.funcs[f].name;
                    self.p.own_methods_named(j, n).into_iter().any(|g| self.p.same_params(f, g) && (!same_iface_signature(&self.p, f, g) || !self.p.funcs[f].is_abstract || !self.p.funcs[g].is_abstract))
                });
                if let Some(cf) = clash {
                    let n = self.p.funcs[cf].name.clone();
                    let (keep, drop) = if misses(&self.p, j) < misses(&self.p, i) { (j, i) } else { (i, j) };
                    let (a, b) = (self.p.classes[keep].name.clone(), self.p.classes[drop].name.clone());
                    let tmpl = a.split('<').next().unwrap_or(&a).to_string();
                    self.d.error_note(
                        "E0261",
                        sp,
                        format!("'{}' implements both '{}' and '{}': one method '{}' cannot implement both", who, a, b, n),
                        format!("their '{}' has the same parameter types in both (only the return type or the default implementation differs), so no overload tells them apart: implement one instance of '{}'", n, tmpl),
                    );
                    skip.push(drop);
                    if drop == i {
                        break;
                    }
                }
            }
        }
        ifs.into_iter().filter(|i| !skip.contains(i)).collect()
    }

    /// why the default implementations of one method leave it to the class or struct
    fn ambiguous_default(&self, ms: &[FuncId]) -> String {
        let labels: Vec<String> = ms.iter().map(|&f| format!("'{}'", self.p.func_label(f))).collect();
        let defaults = ms.iter().filter(|&&f| !self.p.funcs[f].is_abstract).count();
        if defaults > 1 {
            format!("{} each have a default implementation and neither interface extends the other", labels.join(" and "))
        } else {
            let d = ms.iter().find(|&&f| !self.p.funcs[f].is_abstract).copied().unwrap_or(ms[0]);
            let others: Vec<String> = ms.iter().filter(|&&f| f != d).map(|&f| format!("'{}'", self.p.func_label(f))).collect();
            format!("the default implementation '{}' does not implement {} (its interface does not extend that one)", self.p.func_label(d), others.join(" and "))
        }
    }

    /// `where` clauses of a generic method or struct: they must name its type parameters, once each,
    /// and a type constraint must be `Entity` or an entity class (something a type argument can be)
    fn check_where(&mut self, tps: &[(String, Span)], cons: &[ast::Constraint], what: &str) {
        let mut seen: Vec<&str> = vec![];
        for c in cons {
            if !tps.iter().any(|(n, _)| *n == c.param) {
                self.d.error("E0260", c.span, format!("'{}' is not a type parameter of '{}'", c.param, what));
                continue;
            }
            if seen.contains(&c.param.as_str()) {
                self.d.error("E0260", c.span, format!("more than one 'where' clause for '{}'", c.param));
            }
            seen.push(&c.param);
            let has = |k: fn(&ast::ConstraintKind) -> bool| c.kinds.iter().any(|(x, _)| k(x));
            if has(|k| matches!(k, ast::ConstraintKind::Class)) && has(|k| matches!(k, ast::ConstraintKind::Struct | ast::ConstraintKind::Unmanaged)) {
                self.d.error("E0260", c.span, format!("'{}' cannot be both a reference type ('class') and a value type ('struct'/'unmanaged')", c.param));
            }
            for (k, ksp) in &c.kinds {
                let ast::ConstraintKind::Type(te) = k else { continue };
                if te.name == "Entity" && te.args.is_empty() && !te.nullable {
                    continue;
                }
                let env: Vec<(String, Ty)> = tps.iter().map(|(n, _)| (n.clone(), Ty::Error)).collect();
                let saved = std::mem::replace(&mut self.tenv, env);
                let ty = self.resolve_type(te);
                self.tenv = saved;
                match ty {
                    Ty::Error | Ty::Handle(_) => {}
                    _ => {
                        let tn = self.p.ty_name(&ty);
                        self.d.error_note(
                            "E0260",
                            *ksp,
                            format!("'{}' cannot be a constraint", tn),
                            "a type constraint is Entity or an entity class (a type argument is that class or derives from it); for value types write 'struct' or 'unmanaged'",
                        );
                    }
                }
            }
        }
    }

    /// Checks the type arguments of an instance against the `where` clauses (reported at `sp`).
    fn satisfies_where(&mut self, cons: &[ast::Constraint], names: &[String], tys: &[Ty], what: &str, sp: Span) -> bool {
        for c in cons {
            let Some(i) = names.iter().position(|n| *n == c.param) else { continue };
            let t = tys[i].clone();
            for (k, _) in &c.kinds {
                let why = match k {
                    ast::ConstraintKind::Struct => (!matches!(t, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::Struct(_))).then(|| "it is a reference type".to_string()),
                    ast::ConstraintKind::Class => (!matches!(t, Ty::Handle(_) | Ty::NHandle(_) | Ty::Str | Ty::List(_) | Ty::Dict(..) | Ty::Array(_) | Ty::MdArray(..))).then(|| "it is a value type".to_string()),
                    ast::ConstraintKind::New => match &t {
                        Ty::Str => Some("a string has no parameterless constructor".into()),
                        Ty::Array(_) | Ty::MdArray(..) => Some("an array needs a length".into()),
                        Ty::Handle(c) | Ty::NHandle(c) if self.p.classes[*c].is_abstract => Some("it is abstract".into()),
                        _ => None,
                    },
                    ast::ConstraintKind::NotNull => matches!(t, Ty::NHandle(_)).then(|| "it is a nullable entity reference".to_string()),
                    ast::ConstraintKind::Unmanaged => (!self.unmanaged(&t, 0)).then(|| "it is or contains a reference (an entity reference, string or collection)".to_string()),
                    ast::ConstraintKind::Type(te) if te.name == "Entity" && te.args.is_empty() && !te.nullable => (!t.is_handle()).then(|| "it is not an entity class".to_string()),
                    ast::ConstraintKind::Type(te) => {
                        // resolved again in this instance's terms; errors were reported at the declaration
                        let env: Vec<(String, Ty)> = names.iter().cloned().zip(tys.iter().cloned()).collect();
                        let (saved, n) = (std::mem::replace(&mut self.tenv, env), self.d.list.len());
                        let want = self.resolve_type(te);
                        self.d.list.truncate(n);
                        self.tenv = saved;
                        match (want, t.class_of(), &t) {
                            (Ty::Handle(b), Some(c), _) if self.p.is_subclass(c, b) => None,
                            (Ty::Handle(b), None, Ty::Struct(s)) if self.p.classes[b].is_interface => {
                                let imp = self.p.struct_ifaces.get(s).map(|l| l.iter().any(|&i| self.p.extends(i, b, 0))).unwrap_or(false);
                                (!imp).then(|| format!("struct '{}' does not implement '{}'", self.p.structs[*s].name, self.p.classes[b].name))
                            }
                            (Ty::Handle(b), _, _) if self.p.classes[b].is_interface => Some(format!("it does not implement '{}'", self.p.classes[b].name)),
                            (Ty::Handle(b), _, _) => Some(format!("it is not '{}' or a class derived from it", self.p.classes[b].name)),
                            _ => None,
                        }
                    }
                };
                if let Some(why) = why {
                    let tn = self.p.ty_name(&t);
                    self.d.error_note("E0260", sp, format!("type argument '{}' does not satisfy 'where {} : {}' of '{}'", tn, c.param, constraint_text(k), what), why);
                    return false;
                }
            }
        }
        true
    }

    /// a value type with no references inside (numbers, bool, char, enums, structs of those)
    fn unmanaged(&self, t: &Ty, depth: usize) -> bool {
        match t {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) => true,
            Ty::Struct(s) if depth < 64 => self.p.structs[*s].fields.iter().all(|f| self.unmanaged(&f.ty, depth + 1)),
            _ => false,
        }
    }

    /// The class (`c` or a base class) that declares the generic method `name`.
    fn find_template_class(&self, c: ClassId, name: &str) -> Option<ClassId> {
        let mut cur = Some(c);
        while let Some(x) = cur {
            if self.templates.contains_key(&(TypeRef::Class(x), name.to_string())) {
                return Some(x);
            }
            cur = self.p.classes[x].base;
        }
        None
    }

    /// A call of a generic method: its type arguments (written, or inferred from the arguments),
    /// the instance for them (declared on first use, body checked later) and the checked arguments.
    fn generic_call(&mut self, ti: usize, type_args: &[ast::TypeExpr], args: &[ast::Arg], sp: Span, name_sp: Span) -> Option<(FuncId, Vec<HArg>)> {
        let names: Vec<String> = self.tmpls[ti].md.type_params.iter().map(|(n, _)| n.clone()).collect();
        let tname = self.tmpls[ti].md.name.clone();
        let mut pre: Vec<Option<Expr>> = vec![];
        let fail = |this: &mut Self, pre: &mut Vec<Option<Expr>>| {
            for (i, a) in args.iter().enumerate() {
                if pre.get(i).map(|x| x.is_none()).unwrap_or(true) && a.mode != ParamMode::Out {
                    let _ = this.expr(&a.expr, None);
                }
            }
            None
        };
        let tys: Vec<Ty> = if !type_args.is_empty() {
            if type_args.len() != names.len() {
                self.d.error("E0241", sp, format!("'{}' takes {} type argument(s), found {}", tname, names.len(), type_args.len()));
                return fail(self, &mut pre);
            }
            type_args.iter().map(|t| self.resolve_type(t)).collect()
        } else {
            // infer from the arguments whose parameter types mention a type parameter
            let ptys: Vec<ast::TypeExpr> = self.tmpls[ti].md.params.iter().map(|p| p.ty.clone()).collect();
            let mut bind: Vec<Option<Ty>> = vec![None; names.len()];
            for (i, a) in args.iter().enumerate() {
                pre.push(None);
                let Some(pt) = ptys.get(i) else { continue };
                // a lambda takes its type from the parameter once T is known from the others
                if a.mode == ParamMode::Out || !mentions_tparam(pt, &names) || matches!(a.expr.kind, ast::ExprKind::Lambda { .. }) {
                    continue;
                }
                let he = self.expr(&a.expr, None);
                if let Err(msg) = self.infer_tparams(pt, &he.ty, &names, &mut bind) {
                    self.d.error_note("E0259", a.expr.span, format!("cannot infer the type arguments of '{}': {}", tname, msg), format!("write them explicitly: {}<...>(...)", tname));
                    pre[i] = Some(he);
                    return fail(self, &mut pre);
                }
                pre[i] = Some(he);
            }
            if let Some(i) = bind.iter().position(|b| b.is_none()) {
                self.d.error_note(
                    "E0259",
                    sp,
                    format!("cannot infer the type argument '{}' of '{}' from the arguments", names[i], tname),
                    format!("write the type arguments explicitly: {}<{}>(...)", tname, names.join(", ")),
                );
                return fail(self, &mut pre);
            }
            bind.into_iter().map(|b| b.unwrap()).collect()
        };
        if tys.iter().any(|t| *t == Ty::Error) {
            return fail(self, &mut pre);
        }
        if let Some(t) = tys.iter().find(|t| matches!(t, Ty::Void | Ty::Null)) {
            let tn = self.p.ty_name(t);
            self.d.error("E0259", sp, format!("'{}' is not a valid type argument", tn));
            return fail(self, &mut pre);
        }
        let cons = self.tmpls[ti].md.constraints.clone();
        if !self.satisfies_where(&cons, &names, &tys, &tname, sp) {
            return fail(self, &mut pre);
        }
        let f = match self.tmpls[ti].instances.iter().find(|(k, _)| *k == tys).map(|(_, f)| *f) {
            Some(f) => f,
            None => {
                if self.tmpls[ti].instances.len() >= MAX_INSTANCES {
                    self.d.error_note("E0259", sp, format!("too many instances of the generic method '{}'", tname), "a generic method that calls itself with ever larger type arguments (F<List<T>>) never ends");
                    return fail(self, &mut pre);
                }
                let mut md = self.tmpls[ti].md.clone();
                let labels: Vec<String> = tys.iter().map(|t| self.p.ty_name(t)).collect();
                md.name = format!("{}<{}>", md.name, labels.join(", "));
                let env: Vec<(String, Ty)> = self.tmpls[ti].outer.iter().cloned().chain(names.iter().cloned().zip(tys.iter().cloned())).collect();
                let (owner, kind) = (self.tmpls[ti].owner, self.tmpls[ti].kind);
                let quiet = !self.tmpls[ti].instances.is_empty() || self.tmpls[ti].quiet;
                let saved = (std::mem::replace(&mut self.tenv, env.clone()), self.quiet);
                self.quiet = quiet || saved.1;
                let f = self.declare_func(&md, owner, kind);
                self.tenv = saved.0;
                self.quiet = saved.1;
                self.tmpls[ti].instances.push((tys.clone(), f));
                self.inst_queue.push(Instance { fid: f, owner, md, env, quiet, note: None });
                f
            }
        };
        let first = self.tmpls[ti].instances[0].1;
        self.reference(name_sp, Sym::Func(first));
        let hargs = self.check_args_pre(f, args, pre, sp);
        Some((f, hargs))
    }

    /// `Pair<int>`: the instance of a generic struct for these type arguments, declared on first use
    /// (fields and method signatures now, method bodies and field initializers later).
    fn struct_instance(&mut self, ti: usize, t: &ast::TypeExpr) -> Ty {
        let names: Vec<String> = self.stmpls[ti].decl.type_params.iter().map(|(n, _)| n.clone()).collect();
        let tname = self.stmpls[ti].decl.name.clone();
        if t.nullable {
            self.d.error("E0210", t.span, "only entity handles can be nullable");
            return Ty::Error;
        }
        if t.args.len() != names.len() {
            self.d.error("E0228", t.span, format!("'{}' takes {} type argument(s), found {}", tname, names.len(), t.args.len()));
            return Ty::Error;
        }
        let sp = Span::new(t.span.lo as usize, t.span.lo as usize + tname.len());
        let tys: Vec<Ty> = if self.dry_force { vec![Ty::Error; names.len()] } else { t.args.iter().map(|a| self.resolve_type(a)).collect() };
        if tys.iter().any(|x| *x == Ty::Error) && !self.dry_force {
            return Ty::Error;
        }
        if let Some(x) = tys.iter().find(|x| matches!(x, Ty::Void | Ty::Null)) {
            let xn = self.p.ty_name(x);
            self.d.error("E0259", t.span, format!("'{}' is not a valid type argument", xn));
            return Ty::Error;
        }
        let cons = self.stmpls[ti].decl.constraints.clone();
        if !self.dry_force && !self.satisfies_where(&cons, &names, &tys, &tname, t.span) {
            return Ty::Error;
        }
        if let Some(&(_, s)) = self.stmpls[ti].instances.iter().find(|(k, _)| *k == tys) {
            let first = self.stmpls[ti].instances[0].1;
            self.reference(sp, Sym::Struct(first));
            return Ty::Struct(s);
        }
        if self.stmpls[ti].instances.len() >= MAX_INSTANCES {
            self.d.error_note("E0259", t.span, format!("too many instances of the generic struct '{}'", tname), "a generic struct whose fields use ever larger type arguments (S<S<T>>) never ends");
            return Ty::Error;
        }
        let labels: Vec<String> = tys.iter().map(|x| self.p.ty_name(x)).collect();
        let decl = self.stmpls[ti].decl.clone();
        self.p.structs.push(StructDef { name: format!("{}<{}>", tname, labels.join(", ")), span: decl.span, fields: vec![], methods: HashMap::new(), overloads: vec![], ctors: vec![], ops: vec![] });
        let sid = self.p.structs.len() - 1;
        let quiet = !self.stmpls[ti].instances.is_empty();
        self.stmpls[ti].instances.push((tys.clone(), sid));
        self.struct_inst.insert(sid, (ti, tys.clone()));
        let first = self.stmpls[ti].instances[0].1;
        self.reference(sp, Sym::Struct(first));
        let env: Vec<(String, Ty)> = names.into_iter().zip(tys).collect();
        let saved = (std::mem::replace(&mut self.tenv, env.clone()), self.quiet);
        self.quiet = quiet || saved.1;
        self.def(decl.name_span, Sym::Struct(sid));
        for m in &decl.members {
            match m {
                ast::Member::Field(f) => {
                    if f.mods.is_const || f.mods.is_static {
                        self.d.error("E0258", f.span, "a generic struct cannot declare constants or static fields (put them in a static class)");
                        continue;
                    }
                    let ty = self.resolve_type(&f.ty);
                    if ty.is_list() {
                        self.d.error("E0216", f.span, "struct fields cannot be List<T> or Dictionary<K, V> (structs are plain values)");
                    }
                    if let Ty::Handle(_) = ty {
                        self.d.error("E0302", f.ty.span, "entity references stored in fields must be nullable (add '?')");
                    }
                    if self.p.structs[sid].fields.iter().any(|x| x.name == f.name) {
                        self.d.error("E0104", f.span, format!("duplicate field '{}'", f.name));
                    }
                    self.p.structs[sid].fields.push(SField { name: f.name.clone(), ty, init: None, span: f.span });
                    let fi = self.p.structs[sid].fields.len() - 1;
                    self.def(f.span, Sym::SField(sid, fi));
                    if let Some(e) = &f.init {
                        self.inst_field_inits.push((sid, fi, e.clone(), env.clone(), self.quiet));
                    }
                }
                ast::Member::Method(md) => {
                    if !md.type_params.is_empty() {
                        self.d.error("E0258", md.name_span, "a generic struct's methods cannot have type parameters of their own; add them to the struct");
                        continue;
                    }
                    let kind = match &md.kind {
                        MethodKind::Ctor => FuncKind::StructCtor(sid),
                        MethodKind::Operator(_) => FuncKind::StructOp(sid),
                        _ if md.mods.is_static => FuncKind::StructStatic(sid),
                        _ => FuncKind::StructMethod(sid),
                    };
                    if md.mods.is_extern {
                        self.d.error("E0258", md.name_span, "a generic struct's methods cannot be extern (C has no generics)");
                        continue;
                    }
                    let owner = Owner::Struct(sid);
                    let fid = self.declare_func(md, owner, kind);
                    match kind {
                        FuncKind::StructCtor(_) => self.p.structs[sid].ctors.push(fid),
                        FuncKind::StructOp(_) => {
                            self.p.structs[sid].ops.push(fid);
                            let f = &self.p.funcs[fid];
                            if !f.params.iter().any(|&l| f.locals[l].ty == Ty::Struct(sid)) {
                                self.d.error("E0218", md.span, "an operator must take its declaring struct as a parameter");
                            }
                        }
                        _ => self.add_method(TypeRef::Struct(sid), fid, md),
                    }
                    match &md.body {
                        Some(_) => {
                            let q = self.quiet;
                            self.inst_queue.push(Instance { fid, owner, md: md.clone(), env: env.clone(), quiet: q, note: None });
                        }
                        None => self.d.error("E0219", md.span, "method requires a body"),
                    }
                }
            }
        }
        // `struct Stack<T> : IContainer<T>`: the interfaces in this instance's terms
        let before = self.d.list.len();
        for te in decl.base.iter().chain(decl.ifaces.iter()) {
            match self.header_iface(te) {
                Ok(i) => self.p.struct_ifaces.entry(sid).or_default().push(i),
                Err(true) => {}
                Err(false) => self.d.error_note("E0209", te.span, format!("'{}' is not an interface", te.name), "a struct cannot derive from anything; it can implement interfaces"),
            }
        }
        self.dedupe_since(before);
        self.tenv = saved.0;
        self.quiet = saved.1;
        if self.defaults_open && self.p.struct_ifaces.contains_key(&sid) {
            self.struct_defaults(sid);
        }
        // `struct Node<T> { Node<T> Next; }`: an instance that contains itself
        let mut path = vec![];
        if let Some(cycle) = struct_cycle(&self.p, sid, sid, &mut path) {
            let f = &self.p.structs[sid].fields[cycle];
            if !self.quiet {
                self.d.error_note(
                    "E0257",
                    f.span,
                    format!("struct field '{}.{}' makes the struct contain itself", self.p.structs[sid].name, f.name),
                    "a struct is a value, so it cannot hold a value of its own type; store an index, or make it an entity class and store a nullable reference",
                );
            }
            self.p.structs[sid].fields[cycle].ty = Ty::Error;
        }
        Ty::Struct(sid)
    }

    /// `Pool<int>`: the instance of a generic entity class for these type arguments, an entity
    /// class of its own (its own archetype), declared on first use: its base class and interfaces
    /// now, its fields and method signatures now or (while class bases are being resolved) when
    /// pass 3 starts, its method bodies and field initializers with the other instances.
    fn class_instance(&mut self, ti: usize, t: &ast::TypeExpr) -> Ty {
        let names: Vec<String> = self.ctmpls[ti].decl.type_params.iter().map(|(n, _)| n.clone()).collect();
        let tname = self.ctmpls[ti].decl.name.clone();
        let sp = Span::new(t.span.lo as usize, t.span.lo as usize + tname.len());
        if t.args.len() != names.len() {
            let example = format!("{}<{}>", tname, vec!["int"; names.len()].join(", "));
            if t.args.is_empty() {
                self.d.error_note("E0228", t.span, format!("generic class '{}' needs {} type argument(s)", tname, names.len()), format!("each instance is a class of its own: write the type arguments, e.g. '{}'", example));
            } else {
                self.d.error("E0228", t.span, format!("'{}' takes {} type argument(s), found {}", tname, names.len(), t.args.len()));
            }
            return Ty::Error;
        }
        let tys: Vec<Ty> = if self.dry_force { vec![Ty::Error; names.len()] } else { t.args.iter().map(|a| self.resolve_type(a)).collect() };
        if tys.iter().any(|x| *x == Ty::Error) && !self.dry_force {
            return Ty::Error;
        }
        if let Some(x) = tys.iter().find(|x| matches!(x, Ty::Void | Ty::Null)) {
            let xn = self.p.ty_name(x);
            self.d.error("E0259", t.span, format!("'{}' is not a valid type argument", xn));
            return Ty::Error;
        }
        let cons = self.ctmpls[ti].decl.constraints.clone();
        if !self.dry_force && !self.satisfies_where(&cons, &names, &tys, &tname, t.span) {
            return Ty::Error;
        }
        let handle = |c: ClassId| if t.nullable { Ty::NHandle(c) } else { Ty::Handle(c) };
        if self.ctmpls[ti].decl.mods.is_static && !self.static_ok {
            self.d.error("E0229", t.span, format!("static class '{}' cannot be used as a type", tname));
            return Ty::Error;
        }
        if let Some(&(_, c)) = self.ctmpls[ti].instances.iter().find(|(k, _)| *k == tys) {
            let first = self.ctmpls[ti].instances[0].1;
            self.reference(sp, Sym::Class(first));
            return handle(c);
        }
        if self.ctmpls[ti].instances.len() >= MAX_INSTANCES {
            self.d.error_note("E0259", t.span, format!("too many instances of the generic class '{}'", tname), "a generic class that uses itself with ever larger type arguments (C<List<T>>) never ends");
            return Ty::Error;
        }
        let labels: Vec<String> = tys.iter().map(|x| self.p.ty_name(x)).collect();
        let decl = self.ctmpls[ti].decl.clone();
        let k = self.ctmpls[ti].instances.len();
        let is_static = decl.mods.is_static;
        let run_order = self.order_attr(&decl.attrs, !is_static, quiet_order(k));
        self.p.classes.push(ClassDef {
            name: format!("{}<{}>", tname, labels.join(", ")),
            span: decl.span,
            base: None,
            is_entity: !is_static,
            is_static,
            is_abstract: decl.mods.is_abstract,
            is_interface: false,
            ifaces: vec![],
            own_fields: vec![],
            methods: HashMap::new(), overloads: vec![],
            order: self.ctmpls[ti].order + 1 + k,
            run_order,
            is_component: false,
            combo_of: None,
            comps: vec![],
        });
        let cid = self.p.classes.len() - 1;
        let quiet = k > 0;
        self.ctmpls[ti].instances.push((tys.clone(), cid));
        self.class_inst.insert(cid, (ti, tys.clone()));
        self.p.generic_classes[ti].1.push(cid);
        self.p.generic_of.insert(cid, ti);
        let first = self.ctmpls[ti].instances[0].1;
        self.reference(sp, Sym::Class(first));
        let env: Vec<(String, Ty)> = names.into_iter().zip(tys).collect();
        let saved = (std::mem::replace(&mut self.tenv, env), self.quiet);
        self.quiet = quiet || saved.1;
        let before = self.d.list.len();
        self.def(decl.name_span, Sym::Class(cid));
        self.class_header(cid, &decl);
        self.tenv = saved.0;
        self.quiet = saved.1;
        // `class C<T> : C<T>`
        let mut cur = self.p.classes[cid].base;
        let mut seen = 0;
        while let Some(x) = cur {
            seen += 1;
            if x == cid || seen > self.p.classes.len() {
                self.d.error("E0214", self.p.classes[cid].span, "inheritance cycle");
                self.p.classes[cid].base = None;
                break;
            }
            cur = self.p.classes[x].base;
        }
        let note = format!("in '{}', an instance of this generic class", self.p.classes[cid].name);
        self.instance_diags(before, note);
        if self.members_ready {
            self.instance_members(cid);
        } else {
            self.pending_members.push(cid);
        }
        handle(cid)
    }

    /// The checking run for unused generic code: an instance of each unused generic class, struct
    /// and method whose type arguments are the error type (its bodies are checked with the others)
    fn dry_instances(&mut self) {
        let inside = |dry: &[Span], sp: Span| dry.iter().any(|e| e.lo <= sp.lo && sp.hi <= e.hi);
        self.dry_force = true;
        for ti in 0..self.ctmpls.len() {
            let d = self.ctmpls[ti].decl.clone();
            if inside(&self.dry, d.span) && self.ctmpls[ti].instances.is_empty() {
                let any = ast::TypeExpr { name: "int".into(), args: vec![], nullable: false, range: None, span: d.name_span };
                let t = ast::TypeExpr { name: d.name.clone(), args: vec![any; d.type_params.len()], nullable: false, range: None, span: d.name_span };
                self.static_ok = true;
                let _ = self.class_instance(ti, &t);
                self.static_ok = false;
            }
        }
        for ti in 0..self.stmpls.len() {
            let d = self.stmpls[ti].decl.clone();
            if inside(&self.dry, d.span) && self.stmpls[ti].instances.is_empty() {
                let any = ast::TypeExpr { name: "int".into(), args: vec![], nullable: false, range: None, span: d.name_span };
                let t = ast::TypeExpr { name: d.name.clone(), args: vec![any; d.type_params.len()], nullable: false, range: None, span: d.name_span };
                let _ = self.struct_instance(ti, &t);
            }
        }
        // generic methods (also those of the instances just made): one instance per declaration
        let mut seen: Vec<Span> = vec![];
        for ti in 0..self.tmpls.len() {
            let sp = self.tmpls[ti].md.span;
            if !inside(&self.dry, sp) || !self.tmpls[ti].instances.is_empty() || seen.contains(&sp) {
                continue;
            }
            seen.push(sp);
            let mut md = self.tmpls[ti].md.clone();
            let names: Vec<String> = md.type_params.iter().map(|(n, _)| n.clone()).collect();
            md.name = format!("{}<{}>", md.name, vec!["?"; names.len()].join(", "));
            let env: Vec<(String, Ty)> = self.tmpls[ti].outer.iter().cloned().chain(names.iter().map(|n| (n.clone(), Ty::Error))).collect();
            let (owner, kind) = (self.tmpls[ti].owner, self.tmpls[ti].kind);
            let saved = std::mem::replace(&mut self.tenv, env.clone());
            let f = self.declare_func(&md, owner, kind);
            self.tenv = saved;
            self.tmpls[ti].instances.push((vec![Ty::Error; names.len()], f));
            self.inst_queue.push(Instance { fid: f, owner, md, env, quiet: true, note: None });
        }
        self.dry_force = false;
    }

    /// What a generic class may not declare, whatever its type arguments: reported once, at the
    /// declaration (instances skip these members).
    fn check_class_template(&mut self, ti: usize) {
        let decl = self.ctmpls[ti].decl.clone();
        if let Some(b) = decl.base.as_ref().filter(|b| b.args.is_empty() && decl.type_params.iter().any(|(n, _)| *n == b.name)) {
            self.d.error_note("E0209", b.span, format!("class '{}' cannot derive from its type parameter '{}'", decl.name, b.name), "derive from Entity or an entity class, and keep a field of type T (or T? with 'where T : Entity')");
        }
        for m in &decl.members {
            match m {
                ast::Member::Field(f) => {
                    self.check_field_mods(&f.mods);
                    if f.mods.is_const && decl.mods.is_static && f.init.is_none() {
                        self.d.error("E0215", f.span, "constant requires an initializer");
                    } else if f.mods.is_const && !decl.mods.is_static {
                        self.d.error_note("E0258", f.span, format!("generic class '{}' cannot declare constants", decl.name), "put them in a static class (a generic static class may have them: 'static class Limits<T>'), or in a non-generic base class");
                    } else if !f.mods.is_const && !f.mods.is_static && decl.mods.is_static {
                        self.d.error_note("E0206", f.span, format!("static class '{}' cannot have the field '{}'", decl.name, f.name), "a static class holds constants and functions; store state on an entity");
                    } else if f.mods.is_static && !f.mods.is_const {
                        self.d.error_note(
                            "E0206",
                            f.span,
                            format!("mutable static field '{}' is not allowed", f.name),
                            "global mutable state is a data race; store it on an entity (e.g. a 'GameState : Entity') or make it 'const'",
                        );
                    }
                }
                ast::Member::Method(md) => {
                    match &md.kind {
                        MethodKind::Ctor if decl.mods.is_static => self.d.error("E0217", md.span, "a static class cannot declare constructors"),
                        MethodKind::Ctor => self.d.error("E0217", md.span, "entity classes cannot declare constructors; use field initializers and Spawn(new T { ... })"),
                        MethodKind::Operator(_) => self.d.error("E0218", md.span, "operators can only be declared in structs"),
                        MethodKind::Normal if md.mods.is_extern => {
                            self.d.error_note("E0258", md.name_span, format!("'{}.{}': a generic class's methods cannot be extern", decl.name, md.name), "C has no generics: put the extern function in a static class")
                        }
                        MethodKind::Normal => {
                            if md.type_params.is_empty() && !md.constraints.is_empty() {
                                self.d.error("E0260", md.constraints[0].span, format!("'where' constraints on '{}', which has no type parameters", md.name));
                            }
                            if let Some((n, sp)) = md.type_params.iter().find(|(n, _)| decl.type_params.iter().any(|(x, _)| x == n)) {
                                self.d.error_note("E0258", *sp, format!("type parameter '{}' of '{}' hides the type parameter '{}' of '{}'", n, md.name, n, decl.name), "give the method's type parameter another name");
                            }
                            if md.body.is_none() && !md.mods.is_abstract {
                                self.d.error("E0219", md.span, "method requires a body");
                            }
                        }
                    }
                }
            }
        }
    }

    /// The fields and method signatures of a generic class instance (bodies and field
    /// initializers are queued with the other instances).
    fn instance_members(&mut self, cid: ClassId) {
        let (ti, tys) = self.class_inst[&cid].clone();
        let decl = self.ctmpls[ti].decl.clone();
        let env: Vec<(String, Ty)> = decl.type_params.iter().map(|(n, _)| n.clone()).zip(tys).collect();
        let quiet = self.ctmpls[ti].instances[0].1 != cid;
        let saved = (std::mem::replace(&mut self.tenv, env.clone()), self.quiet);
        self.quiet = quiet || saved.1;
        let before = self.d.list.len();
        let tref = TypeRef::Class(cid);
        for m in &decl.members {
            match m {
                // a generic static class's constants: one per instance, typed and evaluated in it
                ast::Member::Field(f) if f.mods.is_const && self.p.classes[cid].is_static => {
                    let ty = self.resolve_type(&ast::TypeExpr { range: None, ..f.ty.clone() });
                    self.p.consts.push(ConstDef { name: f.name.clone(), ty, value: None, span: f.span });
                    let id = self.p.consts.len() - 1;
                    self.def(f.span, Sym::Const(id));
                    if self.consts.insert((tref, f.name.clone()), id).is_some() {
                        self.d.error("E0104", f.span, format!("duplicate member '{}'", f.name));
                    }
                    // (a missing initializer is reported at the template)
                    if let Some(e) = &f.init {
                        self.inst_const_inits.push((id, tref, e.clone(), env.clone()));
                    }
                }
                // reported once, at the template (check_class_template)
                ast::Member::Field(f) if f.mods.is_const || f.mods.is_static => {}
                ast::Member::Field(_) if self.p.classes[cid].is_static => {}
                ast::Member::Method(md) if md.kind != MethodKind::Normal || md.mods.is_extern => {}
                ast::Member::Field(f) => {
                    let ty = self.resolve_type(&ast::TypeExpr { range: None, ..f.ty.clone() });
                    if let Ty::Handle(_) = ty {
                        self.d.error_note(
                            "E0302",
                            f.ty.span,
                            "entity references stored in fields must be nullable (add '?')",
                            "the referenced entity can be destroyed at any time; check it with 'if (F is T x)'",
                        );
                    }
                    if let Ty::List(el) | Ty::Array(el) = &ty {
                        if let Ty::Handle(_) = **el {
                            self.d.error("E0302", f.ty.span, "lists and arrays stored in entity fields must hold nullable handles (List<T?>, T?[])");
                        }
                    }
                    if let Ty::Dict(k, v) = &ty {
                        if matches!(**k, Ty::Handle(_)) || matches!(**v, Ty::Handle(_)) {
                            self.d.error("E0302", f.ty.span, "dictionaries stored in entity fields must hold nullable handles (Dictionary<T?, ...>)");
                        }
                    }
                    if self.p.find_field(cid, &f.name).is_some() {
                        self.d.error("E0104", f.span, format!("duplicate field '{}' (possibly inherited)", f.name));
                    }
                    let mut attrs = self.field_attrs(&f.attrs, f.ty.range.as_ref(), &ty, f.span, tref);
                    attrs.public = f.mods.is_public;
                    self.p.fields.push(EField { fixed_len: None, name: f.name.clone(), class: cid, is_mut: f.mods.is_mut, ty, init: None, attrs, span: f.span });
                    let fid = self.p.fields.len() - 1;
                    if let Some(l) = &f.fixed_len {
                        self.fixed_lens.push((fid, l.clone(), tref));
                    }
                    self.p.classes[cid].own_fields.push(fid);
                    self.def(f.span, Sym::Field(fid));
                    if let Some(e) = &f.init {
                        self.inst_efield_inits.push((cid, fid, e.clone(), env.clone(), self.quiet));
                    }
                }
                ast::Member::Method(md) => {
                    let (owner, kind) = if md.mods.is_static || self.p.classes[cid].is_static { (Owner::Static(cid), FuncKind::Static(cid)) } else { (Owner::Entity(cid), FuncKind::EntityMethod(cid)) };
                    let dup = self.p.classes[cid].methods.contains_key(&md.name) || self.templates.contains_key(&(tref, md.name.clone()));
                    if !md.type_params.is_empty() {
                        if dup {
                            self.d.error_note("E0104", md.span, format!("duplicate method '{}'", md.name), "a generic method cannot be overloaded: give it its own name");
                        } else if md.type_params.iter().any(|(n, _)| env.iter().any(|(x, _)| x == n)) {
                            // reported at the template
                        } else {
                            self.register_template(tref, owner, kind, md);
                            if let Some(&i) = self.templates.get(&(tref, md.name.clone())) {
                                self.tmpls[i].outer = env.clone();
                                self.tmpls[i].quiet = self.quiet;
                            }
                        }
                        continue;
                    }
                    if self.templates.contains_key(&(tref, md.name.clone())) {
                        self.d.error_note("E0104", md.span, format!("duplicate method '{}'", md.name), "a generic method cannot be overloaded: give it its own name");
                        continue;
                    }
                    let fid = self.declare_func(md, owner, kind);
                    self.add_method(TypeRef::Class(cid), fid, md);
                    if md.body.is_some() {
                        let q = self.quiet;
                        self.inst_queue.push(Instance { fid, owner, md: md.clone(), env: env.clone(), quiet: q, note: None });
                    }
                }
            }
        }
        self.tenv = saved.0;
        self.quiet = saved.1;
        if self.class_checks_done {
            self.override_checks(cid);
            self.name_clash(cid);
        }
        let note = format!("in '{}', an instance of this generic class", self.p.classes[cid].name);
        self.instance_diags(before, note);
    }

    /// Infers type parameters from a parameter type and the type of the argument passed to it.
    fn infer_tparams(&self, pt: &ast::TypeExpr, at: &Ty, names: &[String], bind: &mut [Option<Ty>]) -> Result<(), String> {
        if let Some(i) = names.iter().position(|n| *n == pt.name).filter(|_| pt.args.is_empty()) {
            let t = match at {
                Ty::Error | Ty::Null | Ty::Void => return Ok(()),
                Ty::Handle(c) | Ty::NHandle(c) if pt.nullable => Ty::Handle(*c),
                _ if pt.nullable => return Ok(()),
                t => t.clone(),
            };
            let merged = match (&bind[i], &t) {
                (None, _) => t.clone(),
                (Some(x), y) if x == y => t.clone(),
                (Some(Ty::Int), Ty::Float) | (Some(Ty::Float), Ty::Int) => Ty::Float,
                (Some(Ty::Int), Ty::Char) | (Some(Ty::Char), Ty::Int) => Ty::Int,
                (Some(Ty::Handle(a) | Ty::NHandle(a)), Ty::Handle(b) | Ty::NHandle(b)) => {
                    let nullable = matches!(bind[i], Some(Ty::NHandle(_))) || matches!(t, Ty::NHandle(_));
                    let c = if self.is_subclass(*b, *a) {
                        *a
                    } else if self.is_subclass(*a, *b) {
                        *b
                    } else {
                        return Err(format!("'{}' is both '{}' and '{}'", names[i], self.p.classes[*a].name, self.p.classes[*b].name));
                    };
                    if nullable { Ty::NHandle(c) } else { Ty::Handle(c) }
                }
                (Some(x), y) => return Err(format!("'{}' is both '{}' and '{}'", names[i], self.p.ty_name(x), self.p.ty_name(y))),
            };
            bind[i] = Some(merged);
            return Ok(());
        }
        match (pt.name.as_str(), at) {
            ("List" | "Sijo.List", Ty::List(el)) | ("[]", Ty::Array(el)) if pt.args.len() == 1 => self.infer_tparams(&pt.args[0], el, names, bind),
            (n, Ty::MdArray(el, r)) if pt.args.len() == 1 && crate::parser::array_type_rank(n) == *r as usize => self.infer_tparams(&pt.args[0], el, names, bind),
            ("Dictionary" | "Sijo.Dictionary", Ty::Dict(k, v)) if pt.args.len() == 2 => {
                self.infer_tparams(&pt.args[0], k, names, bind)?;
                self.infer_tparams(&pt.args[1], v, names, bind)
            }
            (n, Ty::Struct(s)) => match (self.stemplates.get(n), self.struct_inst.get(s)) {
                (Some(&ti), Some((tj, tys))) if ti == *tj && tys.len() == pt.args.len() => {
                    for (a, t) in pt.args.iter().zip(tys.clone()) {
                        self.infer_tparams(a, &t, names, bind)?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            },
            // `IContainer<T> c` given an `IContainer<int>` handle, or a class that implements
            // exactly one instance of `IContainer`
            (n, Ty::Handle(c) | Ty::NHandle(c)) if self.itemplates.contains_key(n) => {
                let ti = self.itemplates[n];
                let mut cands: Vec<ClassId> = std::iter::once(*c).chain(self.p.all_ifaces(*c)).filter(|x| self.iface_inst.get(x).map(|(t, _)| *t == ti).unwrap_or(false)).collect();
                cands.dedup();
                match cands[..] {
                    [i] => {
                        let tys = self.iface_inst[&i].1.clone();
                        if tys.len() == pt.args.len() {
                            for (a, t) in pt.args.iter().zip(tys) {
                                self.infer_tparams(a, &t, names, bind)?;
                            }
                        }
                        Ok(())
                    }
                    [] => Ok(()),
                    _ => {
                        let all: Vec<String> = cands.iter().map(|&i| format!("'{}'", self.p.classes[i].name)).collect();
                        Err(format!("'{}' implements {}", self.p.classes[*c].name, all.join(" and ")))
                    }
                }
            }
            _ => Ok(()),
        }
    }

    fn is_subclass(&self, a: ClassId, b: ClassId) -> bool {
        let mut cur = Some(a);
        while let Some(x) = cur {
            if x == b {
                return true;
            }
            cur = self.p.classes[x].base;
        }
        false
    }

    /// Checks the bodies of the generic method instances created so far (which may create more).
    fn check_instances(&mut self) {
        while !self.inst_queue.is_empty() || !self.inst_field_inits.is_empty() || !self.inst_efield_inits.is_empty() {
            if !self.inst_efield_inits.is_empty() {
                let (c, f, e, env, quiet) = self.inst_efield_inits.remove(0);
                let saved = (std::mem::replace(&mut self.tenv, env), self.quiet);
                self.quiet = quiet;
                let before = self.d.list.len();
                let ty = self.p.fields[f].ty.clone();
                // an array field may start with a constant size or constant elements
                let array_init = matches!(ty, Ty::Array(_)) && matches!(e.kind, ast::ExprKind::NewArray { .. });
                if ty.is_list() && !array_init {
                    self.d.error("E0221", e.span, "list fields start empty; initialize them in code");
                } else {
                    let he = self.check_detached(TypeRef::Class(c), &e, &ty);
                    self.p.fields[f].init = Some(he);
                    self.inst_efield_checks.push((f, e.span));
                }
                self.tenv = saved.0;
                self.quiet = saved.1;
                let note = format!("in '{}', an instance of this generic class", self.p.classes[c].name);
                self.instance_diags(before, note);
                continue;
            }
            if !self.inst_field_inits.is_empty() {
                let (s, idx, e, env, quiet) = self.inst_field_inits.remove(0);
                let saved = (std::mem::replace(&mut self.tenv, env), self.quiet);
                self.quiet = quiet;
                let ty = self.p.structs[s].fields[idx].ty.clone();
                let he = self.check_detached(TypeRef::Struct(s), &e, &ty);
                self.p.structs[s].fields[idx].init = Some(he);
                self.inst_field_checks.push((s, idx, e.span));
                self.tenv = saved.0;
                self.quiet = saved.1;
                continue;
            }
            let it = self.inst_queue.remove(0);
            let saved = (std::mem::replace(&mut self.tenv, it.env), self.quiet);
            self.quiet = it.quiet;
            let before = self.d.list.len();
            self.check_body(it.fid, it.owner, &it.md);
            self.tenv = saved.0;
            self.quiet = saved.1;
            let note = match (&it.note, it.owner) {
                (Some(n), _) => n.clone(),
                (None, Owner::Struct(s)) if self.struct_inst.contains_key(&s) => format!("in '{}.{}', an instance of this generic struct", self.p.structs[s].name, it.md.name),
                (None, Owner::Entity(c) | Owner::Static(c)) if self.class_inst.contains_key(&c) => format!("in '{}.{}', an instance of this generic class", self.p.classes[c].name, it.md.name),
                _ => format!("in '{}', an instance of this generic method", it.md.name),
            };
            self.instance_diags(before, note);
        }
    }

    /// Diagnostics from checking an instance (since `before`): an error that does not depend on
    /// the type arguments is reported once; the others say which instance they come from.
    fn instance_diags(&mut self, before: usize, note: String) {
        let mut i = before;
        while i < self.d.list.len() {
            let x = &self.d.list[i];
            if self.d.list[..before].iter().any(|y| y.code == x.code && y.span == x.span && y.msg == x.msg) {
                self.d.list.remove(i);
                continue;
            }
            // the innermost instance names the place (instances declared while another is)
            let named = x.notes.iter().any(|n| n.contains(", an instance of this generic")) || x.msg.starts_with("too many instances");
            if x.level == crate::diag::Level::Error && !named {
                self.d.list[i].notes.push(note.clone());
            }
            i += 1;
        }
    }

    fn resolve_type(&mut self, t: &ast::TypeExpr) -> Ty {
        if let Some(r) = &t.range {
            self.d.error_note(
                "E0224",
                r.span,
                "a range belongs to the declared type of a field, variable, parameter or return value",
                "it is the contract of that storage: every write to it saturates into the range",
            );
        }
        if let Some(ty) = self.tenv.iter().rev().find(|(n, _)| *n == t.name).map(|(_, ty)| ty.clone()) {
            if !t.args.is_empty() {
                self.d.error("E0228", t.span, format!("type parameter '{}' is not generic", t.name));
            }
            return match (t.nullable, ty) {
                (false, ty) => ty,
                (true, Ty::Handle(c) | Ty::NHandle(c)) => Ty::NHandle(c),
                (true, Ty::Error) => Ty::Error,
                (true, ty) => {
                    let tn = self.p.ty_name(&ty);
                    self.d.error_note(
                        "E0210",
                        t.span,
                        format!("only entity handles can be nullable ('{}' is '{}' here)", t.name, tn),
                        format!("'{}?' is a reference to an entity: constrain the type parameter ('where {} : Entity' or an entity class) so that every type argument is one", t.name, t.name),
                    );
                    Ty::Error
                }
            };
        }
        let base = match t.name.as_str() {
            "int" => Ty::Int,
            "float" => Ty::Float,
            "bool" => Ty::Bool,
            "void" => Ty::Void,
            "string" => Ty::Str,
            "char" => Ty::Char,
            "double" | "long" | "uint" | "byte" | "short" | "decimal" => {
                self.d.error_note(
                    "E0227",
                    t.span,
                    format!("type '{}' is not supported", t.name),
                    "Sijo has 'int' (32-bit), 'float' (32-bit), 'bool' and 'char'",
                );
                Ty::Error
            }
            name if crate::parser::array_type_rank(name) > 0 => {
                let el = self.resolve_type(&t.args[0]);
                if el.is_dict() {
                    self.d.error("E0228", t.span, "arrays of dictionaries are not supported");
                    return Ty::Error;
                }
                if el == Ty::Void {
                    self.d.error("E0228", t.span, "invalid array element type");
                    return Ty::Error;
                }
                if el == Ty::Error {
                    return Ty::Error;
                }
                return match crate::parser::array_type_rank(name) {
                    1 => Ty::Array(Box::new(el)),
                    r => Ty::MdArray(Box::new(el), r as u8),
                };
            }
            "Func" | "Action" => {
                let mut tys: Vec<Ty> = t.args.iter().map(|a| self.resolve_type(a)).collect();
                if tys.iter().any(|x| *x == Ty::Error) {
                    return Ty::Error;
                }
                let ret = if t.name == "Func" {
                    match tys.pop() {
                        Some(r) => r,
                        None => {
                            self.d.error_note("E0228", t.span, "Func<...> needs at least the return type", "Func<int> returns an int; a delegate that returns nothing is Action or Action<T>");
                            return Ty::Error;
                        }
                    }
                } else {
                    Ty::Void
                };
                if t.nullable {
                    self.d.error_note("E0210", t.span, "a delegate is never null", "its default value does nothing and returns the default of its return type");
                }
                if !self.check_delegate_sig(&tys, &ret, t.span) {
                    return Ty::Error;
                }
                return Ty::Delegate(self.delegate_type(tys, ret, t.span));
            }
            name if self.named_delegates.contains_key(name) => {
                if !t.args.is_empty() {
                    self.d.error("E0228", t.span, format!("delegate '{}' is not generic", name));
                }
                if t.nullable {
                    self.d.error_note("E0210", t.span, "a delegate is never null", "its default value does nothing and returns the default of its return type");
                }
                return Ty::Delegate(self.named_delegates[name]);
            }
            "List" | "Sijo.List" => {
                if t.args.len() != 1 {
                    self.d.error("E0228", t.span, "List<T> takes exactly one type argument");
                    return Ty::Error;
                }
                let el = self.resolve_type(&t.args[0]);
                match el {
                    Ty::Dict(..) => {
                        self.d.error("E0228", t.span, "lists of dictionaries are not supported");
                        Ty::Error
                    }
                    Ty::Void => {
                        self.d.error("E0228", t.span, "invalid list element type");
                        Ty::Error
                    }
                    el => Ty::List(Box::new(el)),
                }
            }
            "Dictionary" | "Sijo.Dictionary" => {
                if t.args.len() != 2 {
                    self.d.error("E0228", t.span, "Dictionary<K, V> takes exactly two type arguments");
                    return Ty::Error;
                }
                let k = self.resolve_type(&t.args[0]);
                let v = self.resolve_type(&t.args[1]);
                if k == Ty::Error || v == Ty::Error {
                    return Ty::Error;
                }
                if !matches!(k, Ty::Int | Ty::Char | Ty::Enum(_) | Ty::Str | Ty::Handle(_) | Ty::NHandle(_)) {
                    let kn = self.p.ty_name(&k);
                    self.d.error("E0228", t.args[0].span, format!("dictionary keys must be int, char, an enum, string or an entity handle (found '{}')", kn));
                    return Ty::Error;
                }
                if v.is_list() || v == Ty::Void {
                    self.d.error("E0228", t.args[1].span, "dictionary values cannot be lists or dictionaries in this version");
                    return Ty::Error;
                }
                if !self.p.kvp.contains_key(&(k.clone(), v.clone())) {
                    let name = format!("KeyValuePair<{}, {}>", self.p.ty_name(&k), self.p.ty_name(&v));
                    self.p.structs.push(StructDef {
                        name,
                        span: t.span,
                        fields: vec![
                            SField { name: "Key".into(), ty: k.clone(), init: None, span: t.span },
                            SField { name: "Value".into(), ty: v.clone(), init: None, span: t.span },
                        ],
                        methods: HashMap::new(), overloads: vec![],
                        ctors: vec![],
                        ops: vec![],
                    });
                    let sid = self.p.structs.len() - 1;
                    self.p.kvp.insert((k.clone(), v.clone()), sid);
                }
                Ty::Dict(Box::new(k), Box::new(v))
            }
            name if self.stemplates.contains_key(name) => {
                let ti = self.stemplates[name];
                return self.struct_instance(ti, t);
            }
            name if self.itemplates.contains_key(name) => {
                let ti = self.itemplates[name];
                return self.iface_instance(ti, t);
            }
            name if self.ctemplates.contains_key(name) => {
                let ti = self.ctemplates[name];
                return self.class_instance(ti, t);
            }
            name => match self.types.get(name).copied().inspect(|tr| {
                let sp = Span::new(t.span.lo as usize, t.span.lo as usize + name.len());
                self.p.index.refs.push((sp, Self::type_sym(*tr)));
            }) {
                Some(TypeRef::Struct(s)) => Ty::Struct(s),
                Some(TypeRef::Enum(e)) => Ty::Enum(e),
                Some(TypeRef::Class(c)) => {
                    if self.p.classes[c].is_static {
                        self.d.error("E0229", t.span, format!("static class '{}' cannot be used as a type", name));
                        Ty::Error
                    } else if t.nullable {
                        return Ty::NHandle(c);
                    } else {
                        Ty::Handle(c)
                    }
                }
                None => {
                    let hint = match &*name.to_string() {
                        "double" | "long" | "short" | "byte" | "uint" | "ulong" | "decimal" => Some("numbers are 'int' (32-bit) and 'float' (32-bit, deterministic on every platform)"),
                        "object" | "dynamic" => Some("there is no object type: use a concrete type"),
                        _ => None,
                    };
                    match hint {
                        Some(h) => self.d.error_note("E0105", t.span, format!("unknown type '{}'", name), h),
                        None => self.d.error("E0105", t.span, format!("unknown type '{}'", name)),
                    }
                    Ty::Error
                }
            },
        };
        if !t.args.is_empty() && !t.name.ends_with("List") && !t.name.ends_with("Dictionary") {
            self.d.error("E0228", t.span, format!("type '{}' is not generic", t.name));
        }
        if t.nullable && base != Ty::Error {
            self.d.error("E0210", t.span, "only entity handles can be nullable");
        }
        base
    }

    // ======================================================================
    // function bodies
    // ======================================================================

    fn check_body(&mut self, fid: FuncId, owner: Owner, md: &ast::MethodDecl) {
        let func = &self.p.funcs[fid];
        let is_static = matches!(func.kind, FuncKind::Static(_) | FuncKind::StructStatic(_) | FuncKind::StructOp(_));
        let mut scope = HashMap::new();
        for &l in &func.params {
            scope.insert(func.locals[l].name.clone(), l);
        }
        self.st = Some(FnState {
            owner,
            is_static,
            scopes: vec![scope],
            locals: func.locals.clone(),
            ret: func.ret.clone(),
            loop_depth: 0,
            switch_depth: 0,
            atomic_depth: 0,
            writes_self: false,
            fid: Some(fid),
            is_async: md.mods.is_async,
            awaits: 0,
            live_across: Default::default(),
            stmt_scope: 0,
            discards: 0,
            out_decls: vec![],
            rush_bg: None,
            rush_next: 0,
            ret_range: func.ret_range,
            step_locals: vec![],
            for_locals: vec![],
            mut_self: md.mods.is_mut || matches!(func.kind, FuncKind::StructCtor(_)),
            lam_base: None,
        });
        if md.mods.is_async {
            let f = &self.p.funcs[fid];
            let params_ok = f.params.iter().all(|&l| {
                let loc = &f.locals[l];
                matches!(loc.kind, LocalKind::Param(ParamMode::Value)) && !loc.ty.is_list() && !matches!(loc.ty, Ty::Handle(_))
            });
            let ok = matches!(f.kind, FuncKind::EntityMethod(_)) && params_ok && f.ret == Ty::Void && f.ctx == Ctx::Server && !md.mods.is_virtual && !md.mods.is_override && !md.mods.is_static;
            if !ok {
                self.d.error_note(
                    "E0801",
                    md.name_span,
                    "async methods must be non-virtual 'async void Name(...)' methods of an entity class; parameters are plain values (no ref/out, lists or non-nullable handles)",
                    "a coroutine runs once per entity; for a global script, put it on a director entity spawned in Game.Start",
                );
                self.st().is_async = false;
            }
        }
        let body = md.body.as_ref().unwrap();
        if !self.quiet {
            self.p.index.func_spans.push((fid, md.span.to(body.span)));
        }
        let hb = self.block(body);
        let st = self.st.take().unwrap();
        let ret = self.p.funcs[fid].ret.clone();
        if ret != Ty::Void && ret != Ty::Error && !always_returns(&hb) {
            self.d.error("E0207", md.span, format!("not all code paths return a value in '{}'", md.name));
        }
        let is_async = st.is_async;
        let awaits = st.awaits;
        let live: Vec<LocalId> = st.live_across.iter().copied().collect();
        let f = &mut self.p.funcs[fid];
        f.body = hb;
        f.locals = st.locals;
        // a struct method changes `this` exactly when it is declared `mut` (writes were checked)
        f.writes_self = if matches!(f.kind, FuncKind::StructMethod(_)) { md.mods.is_mut } else { st.writes_self };
        if is_async {
            self.lower_coroutine(fid, owner, md, awaits, live);
        }
    }

    /// One async method call of this entity as an await target: its arguments go into hidden fields
    /// of the caller (assigned now, in `out`), the call itself is returned. `tag` names the fields.
    fn await_call(&mut self, inner: &ast::Expr, name: &str, tag: &str, sp: Span, out: &mut Vec<Stmt>) -> Option<(FuncId, Expr)> {
        let c = self.self_class()?;
        let g = self.p.resolve_method(c, name)?;
        let me = self.st().fid;
        let call = self.expr(inner, None);
        let ExprKind::Call { func, recv, args: hargs, dispatch } = call.kind else { return None };
        if Some(g) == me {
            self.d.error_note("E0804", inner.span, format!("'{}' awaits itself", name), "a coroutine has one frame per entity: loop inside it instead");
            return None;
        }
        // the arguments are evaluated once, when the await starts, into hidden fields of the
        // caller: every resume passes the same values
        let me_ty = Ty::Handle(c);
        let mut fargs = vec![];
        for (i, a) in hargs.into_iter().enumerate() {
            let fname = format!("__{}_await{}_{}", self.p.funcs[me.unwrap_or(0)].name, tag, i);
            let fe = self.hidden_field(c, fname, a.expr.ty.clone(), sp);
            let ty = a.expr.ty.clone();
            out.push(Stmt::Expr(Expr::new(ExprKind::Assign { op: None, target: Box::new(fe.clone()), value: Box::new(a.expr) }, ty, sp)));
            fargs.push(HArg { mode: a.mode, expr: fe });
        }
        let _ = me_ty;
        self.p.awaited.insert(g);
        if let Some(m) = me {
            self.await_edges.push((m, g, inner.span));
        }
        Some((g, Expr::new(ExprKind::Call { func, recv, args: fargs, dispatch }, Ty::Void, inner.span)))
    }

    fn is_async_method(&self, name: &str) -> bool {
        self.self_class().and_then(|c| self.p.resolve_method(c, name)).map(|g| self.p.async_funcs.contains(&g)).unwrap_or(false)
    }

    /// A hidden (never replicated) field of entity class `c`, as `this.<name>`.
    fn hidden_field(&mut self, c: ClassId, name: String, ty: Ty, sp: Span) -> Expr {
        self.p.fields.push(EField { fixed_len: None, name, class: c, is_mut: true, ty: ty.clone(), init: None, attrs: FieldAttrs { replicated: Some(false), ..Default::default() }, span: sp });
        let fid = self.p.fields.len() - 1;
        self.p.classes[c].own_fields.push(fid);
        Expr::new(ExprKind::EField { base: Box::new(Expr::new(ExprKind::SelfRef, Ty::Handle(c), sp)), field: fid }, ty, sp)
    }

    fn await_stmt(&mut self, inner: &ast::Expr, sp: Span, out: &mut Vec<Stmt>, result: Option<Expr>) {
        if !self.st().is_async {
            self.d.error_note("E0803", sp, "'await' can only be used in an async entity method", "declare the method as 'public async void Name()' on an entity class");
            return;
        }
        if self.st().atomic_depth > 0 {
            self.d.error_note(
                "E0803",
                sp,
                "'await' cannot be used inside 'atomic' (a transaction cannot span ticks)",
                "end the atomic block before the await and start another one after it, or set a flag in the block and await outside",
            );
            return;
        }
        let call = match &inner.kind {
            ast::ExprKind::Call { callee, args, .. } => match &callee.kind {
                ast::ExprKind::Name(n) if self.lookup_local(n).is_none() => Some((n.as_str(), args)),
                _ => None,
            },
            _ => None,
        };
        if result.is_some() && !matches!(call, Some(("Race" | "Rush", _))) {
            self.d.error_note("E0803", sp, "this 'await' has no value", "only 'await Race(...)' and 'await Rush(...)' give a value: the index of the arm that finished first");
            return;
        }
        let kind = match call {
            Some(("NextTick", args)) if args.is_empty() => AwaitKind::NextTick,
            Some(("Wait", args)) if args.len() == 1 => {
                let e = self.expr(&args[0].expr, Some(&Ty::Float));
                AwaitKind::Wait(self.coerce(e, &Ty::Float))
            }
            Some(("Until", args)) if args.len() == 1 => {
                self.push_scope();
                let c = self.cond_expr(&args[0].expr);
                self.pop_scope();
                AwaitKind::Until(c)
            }
            Some((name, _)) if self.is_async_method(name) => {
                let point = self.st().awaits + 1;
                let Some((_, call)) = self.await_call(inner, name, &point.to_string(), sp, out) else { return };
                AwaitKind::Call(call)
            }
            Some((which @ ("Race" | "Sync" | "Rush"), args)) => {
                let race = which != "Sync";
                if args.len() < 2 || args.len() > 16 {
                    self.d.error_note("E0803", inner.span, format!("{}(...) takes 2 to 16 arms", which), "each arm is an async method of this entity, Wait(seconds), Until(condition) or NextTick()");
                    return;
                }
                let c = self.self_class().unwrap();
                let point = self.st().awaits + 1;
                let fname = self.st().fid.map(|f| self.p.funcs[f].name.clone()).unwrap_or_default();
                let mut arms = vec![];
                let mut seen: Vec<FuncId> = vec![];
                for (j, a) in args.iter().enumerate() {
                    let arm = match &a.expr.kind {
                        ast::ExprKind::Call { callee, args: aargs, .. } => match &callee.kind {
                            ast::ExprKind::Name(n) if self.lookup_local(n).is_none() => Some((n.as_str(), aargs)),
                            _ => None,
                        },
                        _ => None,
                    };
                    let arm = match arm {
                        Some(("Wait", wargs)) if wargs.len() == 1 && !self.is_async_method("Wait") => {
                            let e = self.expr(&wargs[0].expr, Some(&Ty::Float));
                            let secs = self.coerce(e, &Ty::Float);
                            let timer = self.hidden_field(c, format!("__{}_await{}_t{}", fname, point, j), Ty::Float, sp);
                            AwaitArm::Wait { secs, timer }
                        }
                        Some(("NextTick", nargs)) if nargs.is_empty() && !self.is_async_method("NextTick") => {
                            let secs = Expr::new(ExprKind::Float(0.0), Ty::Float, a.expr.span);
                            let timer = self.hidden_field(c, format!("__{}_await{}_t{}", fname, point, j), Ty::Float, sp);
                            AwaitArm::Wait { secs, timer }
                        }
                        Some(("Until", uargs)) if uargs.len() == 1 && !self.is_async_method("Until") => {
                            self.push_scope();
                            let cnd = self.cond_expr(&uargs[0].expr);
                            self.pop_scope();
                            AwaitArm::Until(cnd)
                        }
                        Some((name, _)) if self.is_async_method(name) => {
                            let Some((g, call)) = self.await_call(&a.expr, name, &format!("{}_{}", point, j), sp, out) else { return };
                            if seen.contains(&g) {
                                self.d.error_note("E0804", a.expr.span, format!("'{}' is awaited twice in one {}(...)", name, which), "a coroutine has one frame per entity, so one method cannot run twice at the same time");
                                return;
                            }
                            seen.push(g);
                            AwaitArm::Call(call)
                        }
                        _ => {
                            self.d.error_note("E0803", a.expr.span, format!("unsupported arm of {}(...)", which), "each arm is an async method of this entity, Wait(seconds), Until(condition) or NextTick()");
                            return;
                        }
                    };
                    arms.push(arm);
                }
                let done = if race { None } else { Some(self.hidden_field(c, format!("__{}_await{}_done", fname, point), Ty::Int, sp)) };
                if !race && result.is_some() {
                    self.d.error_note("E0803", sp, "Sync(...) has no value", "only Race(...) and Rush(...) give a value: the index of the arm that finished first");
                    return;
                }
                let rush = if which == "Rush" {
                    if self.st().rush_bg.is_none() {
                        let bg = self.hidden_field(c, format!("__{}_bg", fname), Ty::Int, sp);
                        self.st().rush_bg = Some(bg);
                    }
                    let mut bits = vec![];
                    for a in &arms {
                        if matches!(a, AwaitArm::Call(_)) {
                            if self.st().rush_next >= 31 {
                                self.d.error("E0803", sp, "too many Rush(...) method arms in one coroutine (at most 31)");
                                return;
                            }
                            let b = 1u32 << self.st().rush_next;
                            self.st().rush_next += 1;
                            bits.push(b);
                        } else {
                            bits.push(0);
                        }
                    }
                    if let Some(f) = self.st().fid {
                        self.rush_funcs.push((f, inner.span));
                    }
                    Some(Rush { bg: self.st().rush_bg.clone().unwrap(), bits })
                } else {
                    None
                };
                AwaitKind::Group { race, arms, done, result: result.clone(), rush }
            }
            _ => {
                self.d.error_note(
                    "E0803",
                    inner.span,
                    "unsupported await",
                    "use 'await NextTick()', 'await Wait(seconds)', 'await Until(condition)', 'await M(...)' for another async method of this entity, or 'await Race(...)' / 'await Sync(...)' / 'await Rush(...)' of those",
                );
                return;
            }
        };
        // every local in scope survives the suspension: it must be storable in the entity
        let mut in_scope: Vec<LocalId> = self.st().scopes.iter().flat_map(|m| m.values().copied()).collect();
        in_scope.sort();
        in_scope.dedup();
        for l in in_scope {
            let loc = self.st().locals[l].clone();
            match loc.kind {
                LocalKind::Cursor(c) => {
                    let cn = self.p.classes[c].name.clone();
                    self.d.error_note(
                        "E0802",
                        sp,
                        format!("'await' inside 'foreach (var {} in Query<{}>())'", loc.name, cn),
                        "the set of entities can change while the coroutine waits; collect what you need first, or await after the loop",
                    );
                }
                LocalKind::ListElem if !loc.name.starts_with("__") => {
                    self.d.error_note("E0802", sp, format!("'await' inside 'foreach' over a collection ('{}')", loc.name), "iterate with an index loop over a copy, or await after the loop");
                }
                LocalKind::Normal if loc.name.starts_with("__") => {}
                LocalKind::Normal => {
                    if let Ty::Handle(c) = loc.ty {
                        let cn = self.p.classes[c].name.clone();
                        self.d.error_note(
                            "E0802",
                            sp,
                            format!("entity handle '{}' is in scope across this 'await'", loc.name),
                            format!("the {} may be destroyed while the coroutine waits: store it as '{}?' and check it again with 'is' after the await", cn, cn),
                        );
                    } else {
                        self.st().live_across.insert(l);
                    }
                }
                _ => {}
            }
        }
        self.st().awaits += 1;
        let point = self.st().awaits;
        let placeholder = Expr::err(sp);
        out.push(Stmt::Await { point, kind, state: placeholder.clone(), timer: placeholder });
    }

    /// Turn an async entity method into a resumable system: hidden fields hold the resume point,
    /// the wait timer and every local that is in scope at an await.
    fn lower_coroutine(&mut self, fid: FuncId, owner: Owner, md: &ast::MethodDecl, points: usize, live: Vec<LocalId>) {
        let Owner::Entity(c) = owner else { return };
        let fname = self.p.funcs[fid].name.clone();
        let sp = md.name_span;
        let new_field = |me: &mut Self, name: String, ty: Ty| -> FieldId {
            me.p.fields.push(EField { fixed_len: None, name, class: c, is_mut: true, ty, init: None, attrs: FieldAttrs { replicated: Some(false), ..Default::default() }, span: sp });
            let id = me.p.fields.len() - 1;
            me.p.classes[c].own_fields.push(id);
            id
        };
        let state = new_field(self, format!("__{}_state", fname), Ty::Int);
        let timer = new_field(self, format!("__{}_timer", fname), Ty::Float);
        let mut hoisted = vec![];
        for l in live {
            let loc = self.p.funcs[fid].locals[l].clone();
            if loc.kind != LocalKind::Normal {
                continue;
            }
            let f = new_field(self, format!("__{}_{}", fname, loc.name.trim_start_matches('_')), loc.ty.clone());
            // a ranged local keeps its contract in the entity: every write to the field saturates
            self.p.fields[f].attrs.range = loc.range;
            hoisted.push((l, f));
        }
        let me_ty = Ty::Handle(c);
        let map: HashMap<LocalId, FieldId> = hoisted.iter().copied().collect();
        let mut body = std::mem::take(&mut self.p.funcs[fid].body);
        rewrite_hoisted(&mut body, &map, &me_ty, &self.p.fields);
        let fexpr = |f: FieldId, ty: Ty| Expr::new(ExprKind::EField { base: Box::new(Expr::new(ExprKind::SelfRef, me_ty.clone(), sp)), field: f }, ty, sp);
        fill_await_frames(&mut body, &fexpr(state, Ty::Int), &fexpr(timer, Ty::Float));
        let f = &mut self.p.funcs[fid];
        f.body = body;
        f.co = Some(CoInfo { state, timer, points, hoisted });
    }

    /// Type-check an expression outside of any function (constant / field initializers).
    fn check_detached(&mut self, tref: TypeRef, e: &ast::Expr, ty: &Ty) -> Expr {
        let owner = match tref {
            TypeRef::Struct(s) => Owner::Struct(s),
            TypeRef::Class(c) => Owner::Static(c),
            TypeRef::Enum(_) => Owner::Static(0),
        };
        self.st = Some(FnState {
            owner,
            is_static: true,
            scopes: vec![HashMap::new()],
            locals: vec![],
            ret: Ty::Void,
            loop_depth: 0,
            switch_depth: 0,
            atomic_depth: 0,
            writes_self: false,
            fid: None,
            is_async: false,
            awaits: 0,
            live_across: Default::default(),
            stmt_scope: 0,
            discards: 0,
            out_decls: vec![],
            rush_bg: None,
            rush_next: 0,
            ret_range: None,
            step_locals: vec![],
            for_locals: vec![],
            mut_self: false,
            lam_base: None,
        });
        let he = self.expr(e, Some(ty));
        let he = self.coerce(he, ty);
        self.st = None;
        he
    }

    /// `[Order(n)]` on an entity class (n an integer literal): where its systems run among the
    /// systems of other classes. `allowed`: the declaration is an entity class or component;
    /// `report`: say what is wrong (once for a generic class)
    fn order_attr(&mut self, attrs: &[ast::Attr], allowed: bool, report: bool) -> i64 {
        let Some(a) = attr(attrs, "Order") else { return 0 };
        if !allowed {
            if report {
                self.d.error_note("E0029", a.span, "[Order(n)] applies to entity classes and components", "it orders the systems (Update and coroutines) a class has among the systems of other classes");
            }
            return 0;
        }
        let v = match a.args.as_slice() {
            [ast::Expr { kind: ast::ExprKind::Int(n), .. }] => Some(*n),
            [ast::Expr { kind: ast::ExprKind::Unary(ast::UnOp::Neg, x), .. }] => match x.kind {
                ast::ExprKind::Int(n) => Some(-n),
                _ => None,
            },
            _ => None,
        };
        match v {
            Some(n) => n,
            None => {
                if report {
                    self.d.error_note("E0029", a.span, "[Order(n)] takes one integer literal", "smaller numbers run first: [Order(-1)] before the default 0, [Order(10)] after it");
                }
                0
            }
        }
    }

    fn def(&mut self, sp: Span, sym: Sym) {
        if self.quiet {
            return;
        }
        self.p.index.refs.push((sp, sym));
        self.p.index.defs.insert(sym, sp);
    }
    fn reference(&mut self, sp: Span, sym: Sym) {
        if self.quiet {
            return;
        }
        // a member of a generic class instance is the member of its first instance in the index
        // (the declaration the source shows): renaming it renames every use
        let sym = self.first_instance_member(sym);
        self.p.index.refs.push((sp, sym));
    }
    fn first_instance_member(&self, sym: Sym) -> Sym {
        let class = match sym {
            Sym::Field(f) => self.p.fields[f].class,
            Sym::Func(f) => match self.p.funcs[f].kind {
                FuncKind::EntityMethod(c) | FuncKind::Static(c) => c,
                _ => return sym,
            },
            _ => return sym,
        };
        let Some(&(ti, _)) = self.class_inst.get(&class) else { return sym };
        let first = self.ctmpls[ti].instances[0].1;
        if first == class {
            return sym;
        }
        let found = match sym {
            Sym::Field(f) => self.p.classes[first].own_fields.iter().copied().find(|&g| self.p.fields[g].name == self.p.fields[f].name).map(Sym::Field),
            // the method at the same place in the declaration (overloads share a name)
            Sym::Func(f) => {
                let at = self.p.iface_methods(class).iter().position(|&g| g == f);
                at.and_then(|i| self.p.iface_methods(first).get(i).copied()).filter(|&g| self.p.funcs[g].name == self.p.funcs[f].name).map(Sym::Func)
            }
            _ => None,
        };
        found.unwrap_or(sym)
    }
    fn type_sym(t: TypeRef) -> Sym {
        match t {
            TypeRef::Struct(s) => Sym::Struct(s),
            TypeRef::Class(c) => Sym::Class(c),
            TypeRef::Enum(e) => Sym::Enum(e),
        }
    }

    fn st(&mut self) -> &mut FnState {
        self.st.as_mut().unwrap()
    }

    fn push_scope(&mut self) {
        self.st().scopes.push(HashMap::new());
    }
    fn pop_scope(&mut self) {
        self.st().scopes.pop();
    }

    fn lookup_local(&self, name: &str) -> Option<LocalId> {
        let st = self.st.as_ref()?;
        for s in st.scopes.iter().rev() {
            if let Some(&l) = s.get(name) {
                return Some(l);
            }
        }
        None
    }

    fn declare_local(&mut self, name: &str, ty: Ty, kind: LocalKind, span: Span) -> LocalId {
        if self.lookup_local(name).is_some() {
            self.d.error("E0103", span, format!("a local named '{}' is already in scope (shadowing is not allowed)", name));
        }
        // a local may hold a component handle for the system it runs in, but not a collection of them
        if ty.is_list() {
            self.check_comp_storage(&ty, span, "a collection");
        }
        let st = self.st();
        // compiler temporaries are written by the code they come from
        st.locals.push(Local { name: name.to_string(), ty, kind, span, range: None, is_mut: name.starts_with("__"), written: false, is_const: false });
        let id = st.locals.len() - 1;
        st.scopes.last_mut().unwrap().insert(name.to_string(), id);
        let fid = st.fid;
        if let Some(f) = fid {
            if !name.starts_with("__") {
                self.def(span, Sym::Local(f, id));
            }
        }
        id
    }

    fn block(&mut self, b: &ast::Block) -> Block {
        self.push_scope();
        let mut out = Block::default();
        for s in &b.stmts {
            self.stmt(s, &mut out.stmts);
        }
        self.pop_scope();
        out
    }

    fn stmt_block(&mut self, s: &ast::Stmt) -> Block {
        match s {
            ast::Stmt::Block(b) => self.block(b),
            other => {
                self.push_scope();
                let mut out = Block::default();
                self.stmt(other, &mut out.stmts);
                self.pop_scope();
                out
            }
        }
    }

    fn stmt(&mut self, s: &ast::Stmt, out: &mut Vec<Stmt>) {
        let (outer, mark) = {
            let st = self.st();
            let o = st.stmt_scope;
            st.stmt_scope = st.scopes.len().saturating_sub(1);
            (o, st.out_decls.len())
        };
        let pos = out.len();
        self.stmt_inner(s, out);
        let st = self.st();
        st.stmt_scope = outer;
        // `out var x` inside this statement: declare x (unassigned) just before it
        let decls: Vec<LocalId> = st.out_decls.drain(mark..).collect();
        for (k, l) in decls.into_iter().enumerate() {
            out.insert(pos + k, Stmt::Let { local: l, init: None });
        }
    }

    /// `out var x` declared from here until the end of the current statement go to the innermost
    /// scope (a loop's own scope), restored when the statement ends
    fn loop_out_scope(&mut self) {
        let st = self.st();
        st.stmt_scope = st.scopes.len() - 1;
    }

    fn stmt_inner(&mut self, s: &ast::Stmt, out: &mut Vec<Stmt>) {
        // xs.ForEach(x => ...) / Array.ForEach(xs, x => ...): a foreach loop over xs
        if let ast::Stmt::Expr(e, ssp) = s {
            if let ast::ExprKind::Call { callee, args, .. } = &e.kind {
                if let ast::ExprKind::Member(base, name, _) = &callee.kind {
                    if name == "ForEach" {
                        let target = match (&base.kind, args.len()) {
                            (ast::ExprKind::Name(n), 2) if n == "Array" && self.lookup_local("Array").is_none() => Some((args[0].expr.clone(), &args[1])),
                            (_, 1) => Some(((**base).clone(), &args[0])),
                            _ => None,
                        };
                        if let Some((iter, lam)) = target {
                            if let ast::ExprKind::Lambda { params, body } = &lam.expr.kind {
                                if params.len() != 1 {
                                    self.d.error("E0241", lam.expr.span, "ForEach takes a lambda with one parameter: x => ...");
                                    return;
                                }
                                let (_, pname, psp) = params[0].clone();
                                let stmt_body = match &**body {
                                    ast::LambdaBody::Expr(x) => ast::Stmt::Expr(x.clone(), x.span),
                                    ast::LambdaBody::Block(b) => ast::Stmt::Block(b.clone()),
                                };
                                let fe = ast::Stmt::Foreach { name: pname, name_span: psp, more: vec![], iter, body: Box::new(stmt_body), span: *ssp };
                                return self.stmt_inner(&fe, out);
                            }
                        }
                    }
                }
            }
        }
        match s {
            ast::Stmt::Block(b) => {
                let hb = self.block(b);
                out.push(Stmt::Block(hb));
            }
            ast::Stmt::Empty(_) => {}
            ast::Stmt::Var { ty, name, init: Some(ast::Expr { kind: ast::ExprKind::NewColl { ty: cty, args, items }, span: isp }), span: vsp, name_span, is_mut, .. }
                if cty.name.ends_with("Dictionary") =>
            {
                // `var d = new Dictionary<K, V> { { k, v }, [k2] = v2 };` is the declaration followed by
                // d.Add(k, v); d[k2] = v2; (C#: Add throws on a duplicate key, the indexer replaces)
                let empty = ast::Expr { kind: ast::ExprKind::New { ty: cty.clone(), args: Some(args.clone().unwrap_or_default()), inits: vec![] }, span: *isp };
                self.stmt(&ast::Stmt::Var { ty: ty.clone(), name: name.clone(), init: Some(empty), span: *vsp, name_span: *name_span, is_mut: true, is_const: false }, out);
                let me = || ast::Expr { kind: ast::ExprKind::Name(name.clone()), span: *name_span };
                for (xs, indexer) in items {
                    let isp = xs.first().map(|x| x.span).unwrap_or(*isp).to(xs.last().map(|x| x.span).unwrap_or(*isp));
                    if xs.len() != 2 {
                        self.d.error("E0245", isp, "a dictionary initializer lists key/value pairs: { key, value } or [key] = value");
                        continue;
                    }
                    let e = if *indexer {
                        let target = ast::Expr { kind: ast::ExprKind::Index(Box::new(me()), vec![xs[0].clone()]), span: isp };
                        ast::Expr { kind: ast::ExprKind::Assign(None, Box::new(target), Box::new(xs[1].clone())), span: isp }
                    } else {
                        let callee = ast::Expr { kind: ast::ExprKind::Member(Box::new(me()), "Add".into(), isp), span: isp };
                        let args = xs.iter().map(|x| ast::Arg { mode: ParamMode::Value, expr: x.clone() }).collect();
                        ast::Expr { kind: ast::ExprKind::Call { callee: Box::new(callee), type_args: vec![], args }, span: isp }
                    };
                    self.stmt(&ast::Stmt::Expr(e, isp), out);
                }
            }
            // `int w = await Race(...)`: the variable, then the await that sets it
            ast::Stmt::Var { ty, name, init: Some(ast::Expr { kind: ast::ExprKind::Await(inner), span: asp }), name_span, is_mut, .. } => {
                let lty = ty.as_ref().map(|t| self.resolve_type(t)).unwrap_or(Ty::Int);
                if !matches!(lty, Ty::Int | Ty::Error) {
                    let tn = self.p.ty_name(&lty);
                    self.d.error("E0200", *name_span, format!("'await Race(...)' gives an int (the winning arm), not '{}'", tn));
                }
                let local = self.declare_local(name, Ty::Int, LocalKind::Normal, *name_span);
                self.st().locals[local].is_mut = *is_mut;
                out.push(Stmt::Let { local, init: Some(Expr::new(ExprKind::Int(-1), Ty::Int, *asp)) });
                let target = Expr::new(ExprKind::Local(local), Ty::Int, *name_span);
                self.await_stmt(inner, *asp, out, Some(target));
            }
            ast::Stmt::Var { ty, name, init, span: _, name_span, is_mut, is_const } => {
                let span = name_span;
                let declared = ty.as_ref().map(|t| self.resolve_type(&ast::TypeExpr { range: None, ..t.clone() }));
                let hinit = init.as_ref().map(|e| self.expr(e, declared.as_ref()));
                let lty = match (&declared, &hinit) {
                    (Some(t), _) => t.clone(),
                    (None, Some(e)) => match &e.ty {
                        Ty::Null => {
                            self.d.error("E0230", *span, "cannot infer the type of 'null'; write the handle type explicitly (e.g. 'Ship? x = null')");
                            Ty::Error
                        }
                        Ty::Void => {
                            self.d.error("E0230", *span, "cannot assign a void expression");
                            Ty::Error
                        }
                        t => t.clone(),
                    },
                    (None, None) => {
                        self.d.error("E0230", *span, "implicitly typed variable requires an initializer");
                        Ty::Error
                    }
                };
                if hinit.is_none() {
                    if let Ty::Handle(_) = lty {
                        self.d.error("E0303", *span, "non-nullable handle variables must be initialized");
                    }
                }
                let hinit = hinit.map(|e| self.coerce(e, &lty));
                let scope = Some(Self::scope_of(self.st.as_ref().unwrap().owner));
                let range = match ty.as_ref().and_then(|t| t.range.clone()) {
                    Some(r) => self.check_range(&r, &lty, false, scope),
                    None => None,
                };
                if hinit.is_none() && *is_const {
                    self.d.error("E0215", *span, "constant requires an initializer");
                } else if hinit.is_none() && !*is_mut {
                    self.d.error_note("E0310", *span, format!("immutable variable '{}' has no value", name), "give it a value here, or declare it 'mut' to assign it later");
                }
                if let (true, Some(e)) = (*is_const, &hinit) {
                    // checked once the program's constants have values (pass 7)
                    self.local_consts.push((e.clone(), *span));
                }
                let hinit = match range {
                    Some((lo, hi)) => hinit.map(|e| saturate(e, lo, hi)),
                    None => hinit,
                };
                let local = self.declare_local(name, lty, LocalKind::Normal, *span);
                {
                    let l = &mut self.st().locals[local];
                    l.is_mut = *is_mut;
                    l.range = range;
                    l.is_const = *is_const;
                }
                out.push(Stmt::Let { local, init: hinit });
            }
            ast::Stmt::If { cond, then, els, .. } => {
                self.push_scope();
                let c = self.cond_expr(cond);
                let t = self.stmt_block(then);
                self.pop_scope();
                let e = els.as_ref().map(|x| self.stmt_block(x));
                out.push(Stmt::If { cond: c, then: t, els: e });
            }
            ast::Stmt::While { cond, body, .. } => {
                self.push_scope();
                // `out var x` in a loop condition belongs to the loop (C#), not the enclosing block
                self.loop_out_scope();
                let c = self.cond_expr(cond);
                self.st().loop_depth += 1;
                let b = self.stmt_block(body);
                self.st().loop_depth -= 1;
                self.pop_scope();
                out.push(Stmt::Loop { cond: Some(c), step: vec![], body: b, do_while: false });
            }
            ast::Stmt::DoWhile { body, cond, .. } => {
                self.st().loop_depth += 1;
                let b = self.stmt_block(body);
                self.st().loop_depth -= 1;
                self.push_scope();
                self.loop_out_scope();
                let c = self.cond_expr(cond);
                self.pop_scope();
                out.push(Stmt::Loop { cond: Some(c), step: vec![], body: b, do_while: true });
            }
            ast::Stmt::For { init, cond, step, body, .. } => {
                self.push_scope();
                let mut inner = vec![];
                let first_local = self.st().locals.len();
                for s in init {
                    // the header's variables are written by the step: no 'mut' needed there
                    if let ast::Stmt::Var { init: None, name_span, name, .. } = s {
                        self.d.error_note("E0310", *name_span, format!("loop variable '{}' has no value", name), "give it its first value in the header: for (int i = 0; ...)");
                    }
                    self.stmt(s, &mut inner);
                }
                let header: Vec<LocalId> = (first_local..self.st().locals.len()).filter(|&l| !self.st().locals[l].name.starts_with("__")).collect();
                self.st().for_locals.extend(header.iter().copied());
                self.loop_out_scope();
                let c = cond.as_ref().map(|c| {
                    let e = self.expr(c, Some(&Ty::Bool));
                    self.coerce(e, &Ty::Bool)
                });
                let saved = std::mem::replace(&mut self.st().step_locals, header);
                let st: Vec<Expr> = step.iter().map(|e| self.expr(e, None)).collect();
                self.st().step_locals = saved;
                self.st().loop_depth += 1;
                let b = self.stmt_block(body);
                self.st().loop_depth -= 1;
                self.pop_scope();
                inner.push(Stmt::Loop { cond: c, step: st, body: b, do_while: false });
                out.push(Stmt::Block(Block { stmts: inner }));
            }
            ast::Stmt::Foreach { name, iter, body, name_span: span, more, .. } => {
                // Query<T>() / Query<T, C1, C2>() (the entities of T that have the components C1, C2)
                if let ast::ExprKind::Call { callee, type_args, args } = &iter.kind {
                    if let ast::ExprKind::Name(n) = &callee.kind {
                        if n == "Query" && self.lookup_local("Query").is_none() && !self.builtin_hidden(n, type_args, args) {
                            if type_args.is_empty() || !args.is_empty() {
                                self.d.error("E0231", iter.span, "usage: Query<T>() or Query<T, C>() (entities of T that have the component C)");
                                return;
                            }
                            let ty = self.resolve_type(&type_args[0]);
                            let Ty::Handle(c) = ty else {
                                if ty != Ty::Error {
                                    self.d.error("E0231", iter.span, "Query<T>() requires an entity class");
                                }
                                return;
                            };
                            let mut with = vec![];
                            for t in &type_args[1..] {
                                match self.resolve_type(t) {
                                    Ty::Handle(k) if self.p.classes[k].is_component && k != c && !with.contains(&k) => with.push(k),
                                    Ty::Handle(k) if self.p.classes[k].is_component => {
                                        self.d.error_note("E0231", t.span, format!("'{}' is listed twice", t.name), "each type in Query<T, C1, C2>() is named once");
                                        return;
                                    }
                                    Ty::Error => return,
                                    _ => {
                                        self.d.error_note("E0231", t.span, format!("'{}' is not a component", t.name), "Query<T, C1, C2>() visits the entities of T that have every component C1, C2 (declared 'component C { ... }')");
                                        return;
                                    }
                                }
                            }
                            if !more.is_empty() && more.len() + 1 != type_args.len() {
                                let sp = more[0].1.to(more[more.len() - 1].1);
                                self.d.error_note("E0231", sp, format!("{} names for a query of {} types", more.len() + 1, type_args.len()), "write one name per type argument: foreach (var (s, b) in Query<Ship, Burning>())");
                                return;
                            }
                            self.push_scope();
                            let var = self.declare_local(name, Ty::Handle(c), LocalKind::Cursor(c), *span);
                            let mut alias = vec![];
                            for ((n2, sp2), &k) in more.iter().zip(with.iter()) {
                                alias.push(self.declare_local(n2, Ty::Handle(k), LocalKind::Cursor(k), *sp2));
                            }
                            self.st().loop_depth += 1;
                            let b = self.stmt_block(body);
                            self.st().loop_depth -= 1;
                            self.pop_scope();
                            out.push(Stmt::ForQuery { var, class: c, with, alias, body: b });
                            return;
                        }
                    }
                }
                if !more.is_empty() {
                    self.d.error_note("E0231", more[0].1, "only a query loop takes several names", "foreach (var (s, b) in Query<Ship, Burning>()): one cursor per type argument");
                    return;
                }
                // foreach (var k in d.Keys) / (var v in d.Values): iterate the key/value pairs
                if let ast::ExprKind::Member(base, which, _) = &iter.kind {
                    if which == "Keys" || which == "Values" {
                        let hb = self.expr(base, None);
                        if let (Ty::Dict(k, v), Some(kvp)) = (hb.ty.clone(), self.p.kvp_of(&hb.ty)) {
                            self.push_scope();
                            let iname = format!("__i{}", self.st().locals.len());
                            let idx = self.declare_local(&iname, Ty::Int, LocalKind::Normal, *span);
                            let kvname = format!("__kv{}", self.st().locals.len());
                            let kv = self.declare_local(&kvname, Ty::Struct(kvp), LocalKind::ListElem, *span);
                            let (ety, fidx) = if which == "Keys" { (*k, 0) } else { (*v, 1) };
                            let var = self.declare_local(name, ety.clone(), LocalKind::ListElem, *span);
                            let get = Expr::new(
                                ExprKind::Field { base: Box::new(Expr::new(ExprKind::Local(kv), Ty::Struct(kvp), *span)), sid: kvp, idx: fidx },
                                ety,
                                *span,
                            );
                            self.st().loop_depth += 1;
                            let mut b = self.stmt_block(body);
                            self.st().loop_depth -= 1;
                            self.pop_scope();
                            b.stmts.insert(0, Stmt::Let { local: var, init: Some(get) });
                            out.push(Stmt::ForList { var: kv, idx, list: hb, body: b });
                            return;
                        }
                        if hb.ty != Ty::Error {
                            let tn = self.p.ty_name(&hb.ty);
                            self.d.error("E0231", iter.span, format!("'{}' has no '{}'", tn, which));
                        }
                        return;
                    }
                }
                let list = self.expr(iter, None);
                if list.ty == Ty::Str {
                    // foreach (char c in s): one pass over the UTF-8 bytes
                    //   { var s_ = s; int p_ = 0; while (p_ < bytes(s_)) { char c = at(s_, p_); p_ += len(s_, p_); body } }
                    let lsp = list.span;
                    self.push_scope();
                    let sname = format!("__s{}", self.st().locals.len());
                    let tmp = self.declare_local(&sname, Ty::Str, LocalKind::Normal, *span);
                    let pname = format!("__p{}", self.st().locals.len());
                    let pos = self.declare_local(&pname, Ty::Int, LocalKind::Normal, *span);
                    let var = self.declare_local(name, Ty::Char, LocalKind::ListElem, *span);
                    self.st().loop_depth += 1;
                    let mut b = self.stmt_block(body);
                    self.st().loop_depth -= 1;
                    self.pop_scope();
                    let te = || Expr::new(ExprKind::Local(tmp), Ty::Str, lsp);
                    let pe = || Expr::new(ExprKind::Local(pos), Ty::Int, lsp);
                    let bi = |f: Builtin, t: Ty| Expr::new(ExprKind::Builtin { f, args: vec![te(), pe()], site: 0 }, t, lsp);
                    let blen = Expr::new(ExprKind::Builtin { f: Builtin::StrByteLen, args: vec![te()], site: 0 }, Ty::Int, lsp);
                    let cond = Expr::new(ExprKind::Binary(BinOp::Lt, Box::new(pe()), Box::new(blen)), Ty::Bool, lsp);
                    let adv = Expr::new(ExprKind::Assign { op: Some(BinOp::Add), target: Box::new(pe()), value: Box::new(bi(Builtin::StrCharBytes, Ty::Int)) }, Ty::Int, lsp);
                    b.stmts.insert(0, Stmt::Expr(adv));
                    b.stmts.insert(0, Stmt::Let { local: var, init: Some(bi(Builtin::StrCharAtByte, Ty::Char)) });
                    let stmts = vec![
                        Stmt::Let { local: tmp, init: Some(list) },
                        Stmt::Let { local: pos, init: Some(Expr::new(ExprKind::Int(0), Ty::Int, lsp)) },
                        Stmt::Loop { cond: Some(cond), step: vec![], body: b, do_while: false },
                    ];
                    out.push(Stmt::Block(Block { stmts }));
                    return;
                }
                let el = match &list.ty {
                    // a T[,] yields its elements in row-major order (as C#)
                    Ty::List(el) | Ty::Array(el) | Ty::MdArray(el, _) => (**el).clone(),
                    Ty::Dict(..) => match self.p.kvp_of(&list.ty) {
                        Some(s) => Ty::Struct(s),
                        None => Ty::Error,
                    },
                    Ty::Error => Ty::Error,
                    t => {
                        let tn = self.p.ty_name(t);
                        self.d.error("E0231", iter.span, format!("cannot iterate over '{}' (expected List<T>, Dictionary<K, V>, string or Query<T>())", tn));
                        Ty::Error
                    }
                };
                self.push_scope();
                let iname = format!("__i{}", self.st().locals.len());
                let idx = self.declare_local(&iname, Ty::Int, LocalKind::Normal, *span);
                let var = self.declare_local(name, el, LocalKind::ListElem, *span);
                self.st().loop_depth += 1;
                let b = self.stmt_block(body);
                self.st().loop_depth -= 1;
                self.pop_scope();
                out.push(Stmt::ForList { var, idx, list, body: b });
            }
            ast::Stmt::Return { value, span } => {
                let ret = self.st().ret.clone();
                let v = match value {
                    Some(e) => {
                        if ret == Ty::Void {
                            self.d.error("E0232", *span, "void function cannot return a value");
                        }
                        let he = self.expr(e, Some(&ret));
                        let he = self.coerce(he, &ret);
                        Some(match self.st().ret_range {
                            Some((lo, hi)) => saturate(he, lo, hi),
                            None => he,
                        })
                    }
                    None => {
                        if ret != Ty::Void && ret != Ty::Error {
                            self.d.error("E0232", *span, "missing return value");
                        }
                        None
                    }
                };
                out.push(Stmt::Return(v, *span));
            }
            ast::Stmt::Break(sp) | ast::Stmt::Continue(sp) => {
                let is_break = matches!(s, ast::Stmt::Break(_));
                let st = self.st();
                if st.loop_depth == 0 && !(is_break && st.switch_depth > 0) {
                    let what = if is_break { "break outside of a loop or switch" } else { "continue outside of a loop" };
                    let note = if st.atomic_depth > 0 {
                        "an 'atomic' block is not a loop: let it end, 'return', or undo it with 'fail'"
                    } else if is_break {
                        "'break' leaves the innermost loop or switch section"
                    } else {
                        "'continue' starts the next iteration of the innermost loop"
                    };
                    self.d.error_note("E0233", *sp, what, note);
                }
                out.push(if matches!(s, ast::Stmt::Break(_)) { Stmt::Break } else { Stmt::Continue });
            }
            ast::Stmt::Expr(e, _) if matches!(e.kind, ast::ExprKind::Await(_)) => {
                let ast::ExprKind::Await(inner) = &e.kind else { unreachable!() };
                self.await_stmt(inner, e.span, out, None);
            }
            // `w = await Race(...)`
            ast::Stmt::Expr(ast::Expr { kind: ast::ExprKind::Assign(None, target, value), span }, _) if matches!(value.kind, ast::ExprKind::Await(_)) => {
                let ast::ExprKind::Await(inner) = &value.kind else { unreachable!() };
                let t = self.expr(target, None);
                self.check_assignable(&t);
                if !matches!(t.ty, Ty::Int | Ty::Error) {
                    let tn = self.p.ty_name(&t.ty);
                    self.d.error("E0200", target.span, format!("'await Race(...)' gives an int (the winning arm), not '{}'", tn));
                }
                self.await_stmt(inner, *span, out, Some(t));
            }
            ast::Stmt::Expr(e, _) => {
                let he = self.expr(e, None);
                match &he.kind {
                    ExprKind::Assign { .. } | ExprKind::IncDec { .. } | ExprKind::Call { .. } | ExprKind::Spawn { .. }
                    | ExprKind::Destroy(_) | ExprKind::CompAdd { .. } | ExprKind::CompRemove { .. } | ExprKind::ListCall { .. } | ExprKind::Builtin { .. } | ExprKind::DictSet { .. } | ExprKind::DictCall { .. }
                    | ExprKind::ListLambda { .. } => {}
                    _ if he.ty == Ty::Error => {}
                    _ => self.d.error("E0234", e.span, "only assignments, calls, increments and Spawn/Destroy can be used as statements"),
                }
                out.push(Stmt::Expr(he));
            }
            ast::Stmt::Atomic { body, span } => {
                // atomic blocks nest (an inner `fail` undoes the inner block only); `return`,
                // `break` and `continue` leave a block normally, so they commit it
                self.st().atomic_depth += 1;
                let b = self.block(body);
                self.st().atomic_depth -= 1;
                let id = self.p.atomic_count;
                self.p.atomic_count += 1;
                out.push(Stmt::Atomic { body: b, id, span: *span });
            }
            ast::Stmt::Switch { scrut, sections, span } => {
                let hs = self.expr(scrut, None);
                let ty = hs.ty.clone();
                self.check_switch_type(&ty, scrut.span);
                let (tmp, subj) = self.switch_subject(&hs);
                let mut check = SwitchCheck { ty: ty.clone(), arms: vec![], has_default: false, is_expr: false, span: *span };
                let mut hsections = vec![];
                let mut default: Option<Block> = None;
                self.st().switch_depth += 1;
                for sec in sections {
                    self.push_scope();
                    let mut conds: Vec<Expr> = vec![];
                    let mut is_default = false;
                    for lab in &sec.labels {
                        match lab {
                            ast::CaseLabel::Default(sp) => {
                                if check.has_default {
                                    self.d.error("E0238", *sp, "the switch statement already has a 'default:' label");
                                }
                                check.has_default = true;
                                is_default = true;
                            }
                            ast::CaseLabel::Case(pat, guard, sp) => {
                                let (c, k) = self.lower_pattern(pat, &subj, &ty);
                                let c = match guard {
                                    Some(g) => {
                                        let hg = self.cond_expr(g);
                                        Expr::new(ExprKind::Binary(BinOp::And, Box::new(c), Box::new(hg)), Ty::Bool, *sp)
                                    }
                                    None => c,
                                };
                                conds.push(c);
                                check.arms.push((k, guard.is_some(), pat.span()));
                            }
                        }
                    }
                    let mut body = Block::default();
                    for st in &sec.body {
                        self.stmt(st, &mut body.stmts);
                    }
                    self.pop_scope();
                    if !ends_flow(&body) {
                        let lsp = match sec.labels.last() {
                            Some(ast::CaseLabel::Default(sp)) | Some(ast::CaseLabel::Case(_, _, sp)) => *sp,
                            None => sec.span,
                        };
                        self.d.error_note(
                            "E0236",
                            lsp,
                            "control cannot fall through from one switch section to the next",
                            "end the section with 'break;', 'return', 'continue' or 'fail'",
                        );
                    }
                    if is_default {
                        default = Some(body);
                    } else {
                        let mut it = conds.into_iter();
                        let first = it.next().unwrap_or_else(|| Expr::new(ExprKind::Bool(false), Ty::Bool, sec.span));
                        let cond = it.fold(first, |a, b| Expr::new(ExprKind::Binary(BinOp::Or, Box::new(a), Box::new(b)), Ty::Bool, sec.span));
                        hsections.push((cond, body));
                    }
                }
                self.st().switch_depth -= 1;
                self.switch_checks.push(check);
                out.push(Stmt::Switch { tmp, scrut: hs, sections: hsections, default });
            }
            ast::Stmt::Fail(sp) => {
                if self.st().atomic_depth == 0 {
                    self.d.error_note(
                        "E0703",
                        *sp,
                        "'fail' is only allowed inside an 'atomic' block",
                        "put the changes that 'fail' should undo, and the 'fail', in 'atomic { ... }'",
                    );
                }
                out.push(Stmt::Fail(*sp));
            }
        }
    }

    /// Checks a condition; `is` bindings that hold when the condition is true stay in the current scope.
    fn cond_expr(&mut self, e: &ast::Expr) -> Expr {
        let he = self.cond_inner(e);
        self.coerce(he, &Ty::Bool)
    }

    fn cond_inner(&mut self, e: &ast::Expr) -> Expr {
        match &e.kind {
            ast::ExprKind::Is(..) => self.expr_allow_bind(e),
            ast::ExprKind::Binary(BinOp::And, a, b) => {
                let ha = self.cond_inner(a);
                let ha = self.coerce(ha, &Ty::Bool);
                let hb = self.cond_inner(b);
                let hb = self.coerce(hb, &Ty::Bool);
                Expr::new(ExprKind::Binary(BinOp::And, Box::new(ha), Box::new(hb)), Ty::Bool, e.span)
            }
            _ => {
                // bindings inside other shapes are not definitely assigned when true -> throwaway scope
                self.push_scope();
                let he = self.expr(e, Some(&Ty::Bool));
                self.pop_scope();
                he
            }
        }
    }

    fn expr_allow_bind(&mut self, e: &ast::Expr) -> Expr {
        let ast::ExprKind::Is(inner, ty, bind) = &e.kind else { unreachable!() };
        // `x is ETeam.Red` / `x is MAX`: a constant pattern that parsed like a type name
        if bind.is_none() && ty.args.is_empty() && !ty.nullable && !self.types.contains_key(&ty.name) && !self.ctemplates.contains_key(&ty.name) && !crate::parser::is_type_keyword(&ty.name) && !self.tenv.iter().any(|(n, _)| *n == ty.name) {
            let pat = ast::Pattern::Const(crate::parser::type_to_expr(ty));
            return self.is_pattern(inner, &pat, e.span);
        }
        let he = self.expr(inner, None);
        let t = self.resolve_type(&ast::TypeExpr { nullable: false, ..ty.clone() });
        let Ty::Handle(c) = t else {
            if t != Ty::Error {
                self.d.error("E0235", ty.span, "'is' patterns test entity handles against an entity class");
            }
            return Expr::err(e.span);
        };
        match he.ty.class_of() {
            Some(src) => {
                if !self.p.is_subclass(c, src) && !self.p.is_subclass(src, c) {
                    let (a, b) = (self.p.classes[src].name.clone(), self.p.classes[c].name.clone());
                    if self.p.classes[c].is_component {
                        self.d.error_note("E0235", e.span, format!("'{}' can never be a '{}'", a, b), format!("'{b}' is a component: test it with 'x.Has<{b}>()' or 'x.Get<{b}>() is {b} c'"));
                    } else if self.p.classes[src].is_component {
                        self.d.error_note("E0235", e.span, format!("'{}' can never be a '{}'", a, b), format!("in a method of the component '{a}', 'Owner' is the entity: 'Owner is {b} x'"));
                    } else {
                        self.d.error("E0235", e.span, format!("'{}' can never be a '{}'", a, b));
                    }
                }
            }
            None => {
                if he.ty != Ty::Error {
                    let tn = self.p.ty_name(&he.ty);
                    self.d.error("E0235", inner.span, format!("'is' requires an entity handle, found '{}'", tn));
                }
            }
        }
        let bind_id = bind.as_ref().map(|(n, sp)| self.declare_local(n, Ty::Handle(c), LocalKind::Normal, *sp));
        Expr::new(ExprKind::Is { expr: Box::new(he), class: c, bind: bind_id }, Ty::Bool, e.span)
    }

    // ======================================================================
    // coercions
    // ======================================================================

    fn can_coerce(&self, from: &Ty, to: &Ty) -> bool {
        if from == to || *from == Ty::Error || *to == Ty::Error {
            return true;
        }
        match (from, to) {
            (Ty::Int, Ty::Float) | (Ty::Char, Ty::Int) | (Ty::Char, Ty::Float) => true,
            (Ty::Handle(d), Ty::Handle(c)) | (Ty::Handle(d), Ty::NHandle(c)) | (Ty::NHandle(d), Ty::NHandle(c)) => {
                self.p.is_subclass(*d, *c)
            }
            (Ty::Null, Ty::NHandle(_)) => true,
            _ => false,
        }
    }

    fn coerce(&mut self, e: Expr, to: &Ty) -> Expr {
        if e.ty == *to || e.ty == Ty::Error || *to == Ty::Error {
            return e;
        }
        if self.can_coerce(&e.ty, to) {
            if e.ty == Ty::Char {
                // char widens to int (its code) and float
                if let ExprKind::Int(n) = e.kind {
                    if *to == Ty::Float {
                        return Expr::new(ExprKind::Float(n as f64), Ty::Float, e.span);
                    }
                    return Expr::new(ExprKind::Int(n), to.clone(), e.span);
                }
                return Expr::new(ExprKind::Cast(Box::new(e.clone())), to.clone(), e.span);
            }
            if e.ty == Ty::Int && *to == Ty::Float {
                if let ExprKind::Int(n) = e.kind {
                    return Expr::new(ExprKind::Float(n as f64), Ty::Float, e.span);
                }
                return Expr::new(ExprKind::Cast(Box::new(e.clone())), Ty::Float, e.span);
            }
            let mut e = e;
            e.ty = to.clone();
            return e;
        }
        let (a, b) = (self.p.ty_name(&e.ty), self.p.ty_name(to));
        if let (Ty::NHandle(_), Ty::Handle(_)) = (&e.ty, to) {
            self.d.error_note(
                "E0301",
                e.span,
                format!("cannot use nullable handle '{}' where a live '{}' is required", a, b),
                "check it first: 'if (x is T t) { ... }'",
            );
        } else if let (Ty::Float, Ty::Int) = (&e.ty, to) {
            self.d.error_note("E0211", e.span, "cannot implicitly convert 'float' to 'int'", "use an explicit cast: (int)x");
        } else if let (Ty::Int, Ty::Char) = (&e.ty, to) {
            self.d.error_note("E0211", e.span, "cannot implicitly convert 'int' to 'char'", "use an explicit cast: (char)x");
        } else if let (Ty::Str, Ty::Char) = (&e.ty, to) {
            self.d.error_note("E0200", e.span, "type mismatch: expected 'char', found 'string'", "a character literal uses single quotes: 'a'");
        } else {
            self.d.error("E0200", e.span, format!("type mismatch: expected '{}', found '{}'", b, a));
        }
        Expr::err(e.span)
    }

    // ======================================================================
    // expressions
    // ======================================================================

    fn self_class(&self) -> Option<ClassId> {
        match self.st.as_ref()?.owner {
            Owner::Entity(c) => Some(c),
            _ => None,
        }
    }

    /// An `out` argument: an existing place, or a declaration `out T x` / `out var x` / `out _`
    /// whose local goes to the scope of the enclosing statement.
    fn out_arg(&mut self, e: &ast::Expr, want: &Ty) -> Expr {
        let ast::ExprKind::OutDecl(ty, name, nsp) = &e.kind else {
            return self.expr(e, Some(want));
        };
        if self.st.is_none() {
            self.d.error("E0252", e.span, "an 'out' variable can only be declared inside a method");
            return Expr::err(e.span);
        }
        let lty = match ty {
            Some(t) => self.resolve_type(t),
            None => want.clone(),
        };
        if lty == Ty::Error {
            return Expr::err(e.span);
        }
        if let Ty::Handle(_) = lty {
            self.d.error_note("E0303", *nsp, "an 'out' variable cannot be a non-nullable entity handle", "declare it as nullable ('Ship? x') and check it with 'is' before use");
            return Expr::err(e.span);
        }
        let discard = name == "_";
        let lname = if discard {
            let st = self.st();
            st.discards += 1;
            format!("__discard{}", st.discards)
        } else {
            name.clone()
        };
        let l = self.declare_local(&lname, lty.clone(), LocalKind::Normal, *nsp);
        // move it from the innermost scope (a condition may use a throwaway one) to the statement's
        let st = self.st();
        let target = st.stmt_scope.min(st.scopes.len() - 1);
        st.scopes.last_mut().unwrap().remove(&lname);
        st.scopes[target].insert(lname, l);
        st.out_decls.push(l);
        Expr::new(ExprKind::Local(l), lty, e.span)
    }

    fn expr(&mut self, e: &ast::Expr, expect: Option<&Ty>) -> Expr {
        use ast::ExprKind as A;
        let sp = e.span;
        match &e.kind {
            A::OutDecl(..) => {
                self.d.error("E0252", sp, "a variable can only be declared here as an 'out' argument ('out int x', 'out var x')");
                Expr::err(sp)
            }
            A::Int(n) => {
                if expect == Some(&Ty::Float) {
                    return Expr::new(ExprKind::Float(*n as f64), Ty::Float, sp);
                }
                if *n > i32::MAX as i64 || *n < i32::MIN as i64 {
                    self.d.error("E0002", sp, "integer literal does not fit in 32-bit int");
                }
                Expr::new(ExprKind::Int(*n), Ty::Int, sp)
            }
            A::Float(f, _) => Expr::new(ExprKind::Float(*f), Ty::Float, sp),
            A::Bool(b) => Expr::new(ExprKind::Bool(*b), Ty::Bool, sp),
            A::Null => Expr::new(ExprKind::Null, Ty::Null, sp),
            A::GenericName(t) => {
                self.d.error("E0200", sp, format!("'{}<...>' is a type, not a value", t.name));
                Expr::err(sp)
            }
            A::Default(t) => {
                let ty = match t {
                    Some(t) => self.resolve_type(t),
                    None => match expect {
                        Some(t) => t.clone(),
                        None => {
                            self.d.error("E0200", sp, "'default' needs a type here: write default(T)");
                            return Expr::err(sp);
                        }
                    },
                };
                match &ty {
                    Ty::Error => Expr::err(sp),
                    Ty::Handle(c) => {
                        let cn = self.p.classes[*c].name.clone();
                        self.d.error_note("E0301", sp, format!("the default of the entity reference '{}' is null, which it cannot hold", cn), format!("use the nullable type: default({}?)", cn));
                        Expr::err(sp)
                    }
                    Ty::List(_) | Ty::Dict(..) | Ty::Array(_) | Ty::MdArray(..) => {
                        self.d.error_note("E0200", sp, "a collection has no default value (C#'s is null; Sijo collections are never null)", "create an empty one: new List<T>(), new T[0]");
                        Expr::err(sp)
                    }
                    _ => match consteval::zero_value(&self.p, &ty) {
                        Some(z) => crate::opt::to_expr(&self.p, &z, &ty, sp),
                        None => {
                            let tn = self.p.ty_name(&ty);
                            self.d.error("E0200", sp, format!("type '{}' has no default value", tn));
                            Expr::err(sp)
                        }
                    },
                }
            }
            A::Str(s) => Expr::new(ExprKind::Str(s.clone()), Ty::Str, sp),
            A::Char(c) => Expr::new(ExprKind::Int(*c as i64), Ty::Char, sp),
            A::Interp(parts) => {
                let mut hp = vec![];
                for p in parts {
                    match p {
                        ast::InterpPart::Text(t) => hp.push(IPart::Text(t.clone())),
                        ast::InterpPart::Expr(x, fmt) => {
                            let hx = self.expr(x, None);
                            match &hx.ty {
                                Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::Handle(_) | Ty::NHandle(_) | Ty::Str | Ty::Error => {}
                                t => {
                                    let tn = self.p.ty_name(t);
                                    self.d.error("E0239", x.span, format!("cannot format a value of type '{}'", tn));
                                }
                            }
                            if let Some(f) = fmt {
                                if let Err(msg) = check_format_spec(f, &hx.ty) {
                                    self.d.error_note(
                                        "E0255",
                                        x.span,
                                        msg,
                                        "supported: int D[n] X[n] x[n] F[n] N[n] E[n] e[n] G; float F[n] N[n] E[n] e[n] G R (C# invariant culture, n <= 99)",
                                    );
                                }
                            }
                            hp.push(IPart::Expr(hx, fmt.clone()));
                        }
                    }
                }
                Expr::new(ExprKind::Interp(hp), Ty::Str, sp)
            }
            A::This => match self.st.as_ref().map(|s| (s.owner, s.is_static)) {
                Some((Owner::Entity(c), false)) => Expr::new(ExprKind::SelfRef, Ty::Handle(c), sp),
                Some((Owner::Struct(s), false)) => Expr::new(ExprKind::StructSelf, Ty::Struct(s), sp),
                _ => {
                    self.d.error("E0106", sp, "'this' is not available in a static context");
                    Expr::err(sp)
                }
            },
            A::Base => {
                self.d.error("E0106", sp, "'base' can only be used to call a base method: base.M(...)");
                Expr::err(sp)
            }
            // `F` / `T.F` / `x.M` where a delegate is expected: the method as a delegate value
            A::Name(n) if matches!(expect, Some(Ty::Delegate(_))) && self.lookup_local(n).is_none() => {
                let Some(Ty::Delegate(dg)) = expect else { unreachable!() };
                match self.method_group(None, n, *dg, sp, sp) {
                    Some(e) => e,
                    None => self.name_expr(n, sp),
                }
            }
            A::Member(base, name, nsp) if matches!(expect, Some(Ty::Delegate(_))) => {
                let Some(Ty::Delegate(dg)) = expect else { unreachable!() };
                match self.method_group(Some(base), name, *dg, sp, *nsp) {
                    Some(e) => e,
                    None => self.member_expr(base, name, *nsp, sp),
                }
            }
            A::Name(n) => self.name_expr(n, sp),
            A::Member(base, name, nsp) => self.member_expr(base, name, *nsp, sp),
            A::Call { callee, type_args, args } => self.call_expr(callee, type_args, args, sp),
            A::Index(base, idxs) => {
                let hb = self.expr(base, None);
                if let Ty::MdArray(el, r) = hb.ty.clone() {
                    // a[i, j]: one int index per dimension
                    let mut hidx = vec![];
                    for x in idxs {
                        let h = self.expr(x, Some(&Ty::Int));
                        hidx.push(self.coerce(h, &Ty::Int));
                    }
                    if idxs.len() != r as usize {
                        let tn = self.p.ty_name(&hb.ty);
                        self.d.error_note("E0239", sp, format!("wrong number of indexes: '{}' takes {}, found {}", tn, r, idxs.len()), "index every dimension: grid[x, y]");
                        return Expr::err(sp);
                    }
                    let n = hidx.len();
                    let index = Expr::new(ExprKind::MdIndex { idx: hidx, checked: vec![true; n] }, Ty::Int, sp);
                    return Expr::new(ExprKind::ListIndex { list: Box::new(hb), index: Box::new(index), checked: false }, *el, sp);
                }
                if idxs.len() != 1 {
                    for x in idxs {
                        let _ = self.expr(x, None);
                    }
                    if hb.ty != Ty::Error {
                        let tn = self.p.ty_name(&hb.ty);
                        let note = if matches!(hb.ty, Ty::Array(_) | Ty::List(_)) { "an array of arrays takes one index per pair of brackets: xs[i][j]; a T[,] takes xs[i, j]" } else { "write one index: x[i]" };
                        self.d.error_note("E0239", sp, format!("wrong number of indexes: '{}' takes 1, found {}", tn, idxs.len()), note);
                    }
                    return Expr::err(sp);
                }
                let idx = &idxs[0];
                if let Ty::Dict(k, v) = hb.ty.clone() {
                    let hk = self.expr(idx, Some(&k));
                    let hk = self.coerce(hk, &k);
                    return Expr::new(ExprKind::DictGet { dict: Box::new(hb), key: Box::new(hk) }, *v, sp);
                }
                let hi = self.expr(idx, Some(&Ty::Int));
                let hi = self.coerce(hi, &Ty::Int);
                match &hb.ty {
                    Ty::List(el) | Ty::Array(el) => {
                        let el = (**el).clone();
                        Expr::new(ExprKind::ListIndex { list: Box::new(hb), index: Box::new(hi), checked: true }, el, sp)
                    }
                    Ty::Error => Expr::err(sp),
                    Ty::Str => Expr::new(ExprKind::Builtin { f: Builtin::StrCharAt, args: vec![hb, hi], site: 0 }, Ty::Char, sp),
                    t => {
                        let tn = self.p.ty_name(t);
                        self.d.error("E0239", sp, format!("type '{}' cannot be indexed", tn));
                        Expr::err(sp)
                    }
                }
            }
            A::Unary(op, x) => {
                let hx = self.expr(x, expect);
                // -c, +c and ~c on a char work on its code (int), as in C#
                let hx = if hx.ty == Ty::Char && *op != UnOp::Not { self.coerce(hx, &Ty::Int) } else { hx };
                match (op, &hx.ty) {
                    (_, Ty::Error) => Expr::err(sp),
                    (UnOp::Neg | UnOp::Plus, Ty::Int | Ty::Float) => {
                        let t = hx.ty.clone();
                        if *op == UnOp::Plus {
                            return hx;
                        }
                        Expr::new(ExprKind::Unary(*op, Box::new(hx)), t, sp)
                    }
                    (UnOp::Not, Ty::Bool) => Expr::new(ExprKind::Unary(*op, Box::new(hx)), Ty::Bool, sp),
                    (UnOp::BitNot, Ty::Int) => Expr::new(ExprKind::Unary(*op, Box::new(hx)), Ty::Int, sp),
                    (_, Ty::Struct(s)) => {
                        let sym = match op {
                            UnOp::Neg => "-",
                            UnOp::Plus => "+",
                            UnOp::Not => "!",
                            UnOp::BitNot => "~",
                        };
                        let s = *s;
                        match self.find_op(s, None, sym, &[hx.ty.clone()]) {
                            Some(f) => {
                                let rt = self.p.funcs[f].ret.clone();
                                Expr::new(ExprKind::OpCall { func: f, args: vec![hx] }, rt, sp)
                            }
                            None => {
                                let tn = self.p.ty_name(&hx.ty);
                                self.d.error("E0239", sp, format!("operator '{}' is not defined for '{}'", sym, tn));
                                Expr::err(sp)
                            }
                        }
                    }
                    (_, t) => {
                        let tn = self.p.ty_name(t);
                        self.d.error("E0239", sp, format!("invalid operand type '{}' for unary operator", tn));
                        Expr::err(sp)
                    }
                }
            }
            A::IncDec(target, inc, prefix) => {
                let ht = self.expr(target, None);
                // the operand already has its error: `----1` is `--(--1)`, one message for the chain
                if ht.ty == Ty::Error {
                    return Expr::err(sp);
                }
                if matches!(ht.kind, ExprKind::DictGet { .. }) {
                    self.d.error_note("E0253", sp, "++/-- cannot be applied to a dictionary element", "write d[key] += 1");
                    return Expr::err(sp);
                }
                if !ht.ty.is_numeric() && ht.ty != Ty::Char {
                    self.d.error("E0239", sp, "++/-- require an int, float or char");
                    return Expr::err(sp);
                }
                match self.mutable_place(&ht) {
                    Ok(()) => self.assignable_result(&ht, Ok(())),
                    Err(why) => {
                        if why == NOT_A_PLACE {
                            let sym = if *inc { "++" } else { "--" };
                            // `--1`, `----x`: probably meant as signs
                            let signs = !*inc && matches!(ht.kind, ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Unary(..) | ExprKind::IncDec { .. });
                            let note = if signs { "'--' decrements a variable; to negate twice, separate the signs: '- -1' or '-(-1)'" } else { "++ and -- change a variable, a field or a list element" };
                            self.d.error_note("E0405", sp, format!("the operand of '{}' must be a variable, field or list element", sym), note);
                        } else {
                            self.assignable_result(&ht, Err(why));
                        }
                        return Expr::err(sp);
                    }
                }
                let t = ht.ty.clone();
                Expr::new(ExprKind::IncDec { target: Box::new(ht), inc: *inc, prefix: *prefix }, t, sp)
            }
            A::Binary(op, a, b) => self.binary(*op, a, b, sp),
            A::Assign(op, target, value) => {
                let ht = self.expr(target, None);
                if let ExprKind::DictGet { dict, key } = ht.kind {
                    return self.dict_assign(*dict, *key, *op, value, ht.ty, sp);
                }
                self.check_assignable(&ht);
                let tty = ht.ty.clone();
                match op {
                    None => {
                        let hv = self.expr(value, Some(&tty));
                        let hv = self.coerce(hv, &tty);
                        Expr::new(ExprKind::Assign { op: None, target: Box::new(ht), value: Box::new(hv) }, tty, sp)
                    }
                    Some(bop) => {
                        if let Ty::Struct(s) = tty {
                            let hv = self.expr(value, None);
                            let sym = bop.sym();
                            match self.find_op(s, None, sym, &[tty.clone(), hv.ty.clone()]) {
                                Some(f) => {
                                    let rt = self.p.funcs[f].ret.clone();
                                    let args = self.coerce_args_to(f, vec![ht.clone(), hv]);
                                    let call = Expr::new(ExprKind::OpCall { func: f, args }, rt, sp);
                                    let call = self.coerce(call, &tty);
                                    return Expr::new(
                                        ExprKind::Assign { op: None, target: Box::new(ht), value: Box::new(call) },
                                        tty,
                                        sp,
                                    );
                                }
                                None => {
                                    let tn = self.p.ty_name(&tty);
                                    self.d.error("E0239", sp, format!("operator '{}' is not defined for '{}'", sym, tn));
                                    return Expr::err(sp);
                                }
                            }
                        }
                        if tty == Ty::Str && *bop == BinOp::Add {
                            let hv = self.expr(value, None);
                            let hv = self.to_str(hv);
                            return Expr::new(ExprKind::Assign { op: Some(BinOp::Add), target: Box::new(ht), value: Box::new(hv) }, tty, sp);
                        }
                        let hv = self.expr(value, Some(&tty));
                        // x op= c with a char c: its code (x += s[i] sums codes, as in C#)
                        let hv = if hv.ty == Ty::Char && matches!(tty, Ty::Int | Ty::Float) { self.coerce(hv, &tty) } else { hv };
                        // c op= x on a char computes with its code: the int result needs a cast (C#'s CS0266)
                        let lt = if tty == Ty::Char { Ty::Int } else { tty.clone() };
                        let rt = if hv.ty == Ty::Char { Ty::Int } else { hv.ty.clone() };
                        let res = self.arith_type(*bop, &lt, &rt, sp);
                        if res != Ty::Error && tty != Ty::Error && !self.can_coerce(&res, &tty) {
                            let (a, b) = (self.p.ty_name(&res), self.p.ty_name(&tty));
                            self.d.error("E0211", sp, format!("result of '{}' is '{}' which cannot be assigned to '{}'", bop.sym(), a, b));
                        }
                        let hv = if tty == Ty::Float { self.coerce(hv, &Ty::Float) } else { hv };
                        Expr::new(ExprKind::Assign { op: Some(*bop), target: Box::new(ht), value: Box::new(hv) }, tty, sp)
                    }
                }
            }
            A::Cond(c, a, b) => {
                self.push_scope();
                let hc = self.cond_inner(c);
                let hc = self.coerce(hc, &Ty::Bool);
                let ha = self.expr(a, expect);
                self.pop_scope();
                let hb = self.expr(b, expect.or(Some(&ha.ty)));
                let t = self.unify(&ha.ty, &hb.ty, sp);
                let ha = self.coerce(ha, &t);
                let hb = self.coerce(hb, &t);
                Expr::new(ExprKind::Cond(Box::new(hc), Box::new(ha), Box::new(hb)), t, sp)
            }
            A::Cast(ty, x) => {
                let t = self.resolve_type(ty);
                let hx = self.expr(x, None);
                let ok = matches!(
                    (&hx.ty, &t),
                    (Ty::Int | Ty::Float | Ty::Enum(_) | Ty::Char, Ty::Int)
                        | (Ty::Int | Ty::Float | Ty::Char, Ty::Float)
                        | (Ty::Int, Ty::Enum(_))
                        | (Ty::Int | Ty::Float, Ty::Char)
                ) || hx.ty == t
                    || hx.ty == Ty::Error
                    || t == Ty::Error;
                if !ok {
                    let (a, b) = (self.p.ty_name(&hx.ty), self.p.ty_name(&t));
                    self.d.error("E0240", sp, format!("cannot cast '{}' to '{}'", a, b));
                    return Expr::err(sp);
                }
                if hx.ty == t {
                    return hx;
                }
                Expr::new(ExprKind::Cast(Box::new(hx)), t, sp)
            }
            A::Is(..) => {
                // bindings in non-condition position are not definitely assigned; keep them local
                self.push_scope();
                let r = self.expr_allow_bind(e);
                self.pop_scope();
                r
            }
            A::New { ty, args, inits } => self.new_expr(ty, args.as_ref(), inits, sp, false),
            A::NewColl { ty, args, items } => {
                let t = self.resolve_type(ty);
                match t {
                    Ty::List(el) => {
                        if args.as_ref().map(|a| !a.is_empty()).unwrap_or(false) {
                            self.d.error("E0245", sp, "use 'new List<T> { ... }' (capacity is inferred by the compiler)");
                        }
                        let el = *el;
                        let mut elems = vec![];
                        for (xs, indexer) in items {
                            if xs.len() != 1 || *indexer {
                                self.d.error("E0245", xs.first().map(|x| x.span).unwrap_or(sp), "a list initializer lists its elements: new List<int> { 1, 2, 3 }");
                                continue;
                            }
                            let h = self.expr(&xs[0], Some(&el));
                            elems.push(self.coerce(h, &el));
                        }
                        // a list literal is an array literal that may grow
                        let site = self.new_alloc_site(el.clone(), sp);
                        Expr::new(ExprKind::ArrayNew { site, len: None, elems, dims: vec![] }, Ty::List(Box::new(el)), sp)
                    }
                    Ty::Dict(..) => {
                        self.d.error_note(
                            "E0245",
                            sp,
                            "a dictionary initializer can only initialize a variable",
                            "declare it: 'var d = new Dictionary<K, V> { [k] = v };', or add the entries with d[k] = v",
                        );
                        Expr::err(sp)
                    }
                    Ty::Error => Expr::err(sp),
                    _ => {
                        self.d.error("E0245", sp, "collection initializers apply to List<T> and Dictionary<K, V>");
                        Expr::err(sp)
                    }
                }
            }
            A::NewArray { elem, rank, lens, elems, shape } if *rank > 1 => {
                let at = ast::TypeExpr { name: crate::parser::array_type_name(*rank), args: vec![elem.clone()], nullable: false, range: None, span: elem.span };
                let aty = self.resolve_type(&at);
                let Ty::MdArray(el, _) = aty.clone() else { return Expr::err(sp) };
                let el = *el;
                let hlens: Vec<Expr> = lens
                    .iter()
                    .map(|l| {
                        let h = self.expr(l, Some(&Ty::Int));
                        self.coerce(h, &Ty::Int)
                    })
                    .collect();
                let helems: Vec<Expr> = elems
                    .iter()
                    .flatten()
                    .map(|x| {
                        let h = self.expr(x, Some(&el));
                        self.coerce(h, &el)
                    })
                    .collect();
                let dims = match elems {
                    Some(_) => {
                        // C#: lengths written next to an initializer are constants equal to its shape
                        for (d, l) in hlens.iter().enumerate() {
                            match crate::consteval::eval_expr(&self.p, l) {
                                Ok(ConstVal::Int(n)) if n >= 0 && Some(&(n as usize)) == shape.get(d) => {}
                                _ => self.d.error(
                                    "E0241",
                                    l.span,
                                    format!("an array initializer of length {} is expected in dimension {} (the length must be a constant equal to the number of rows)", shape.get(d).copied().unwrap_or(0), d),
                                ),
                            }
                        }
                        if helems.len() != shape.iter().product::<usize>() {
                            return Expr::err(sp); // ragged rows: reported by the parser
                        }
                        shape.iter().map(|&n| Expr::new(ExprKind::Int(n as i64), Ty::Int, sp)).collect()
                    }
                    None => {
                        if matches!(el, Ty::Handle(_)) {
                            let en = self.p.ty_name(&el);
                            self.d.error_note("E0302", sp, format!("the elements of 'new {}[...]' would start as null", en), format!("use a nullable element type: new {}?[...]", en));
                        }
                        hlens
                    }
                };
                let site = self.new_alloc_site(el, sp);
                Expr::new(ExprKind::ArrayNew { site, len: None, elems: helems, dims }, aty, sp)
            }
            A::NewArray { elem, lens, elems, .. } => {
                let at = ast::TypeExpr { name: "[]".into(), args: vec![elem.clone()], nullable: false, range: None, span: elem.span };
                let Ty::Array(el) = self.resolve_type(&at) else { return Expr::err(sp) };
                let el = *el;
                let len = lens.first();
                let hlen = len.map(|l| {
                    let h = self.expr(l, Some(&Ty::Int));
                    self.coerce(h, &Ty::Int)
                });
                let helems: Vec<Expr> = elems
                    .iter()
                    .flatten()
                    .map(|x| {
                        let h = self.expr(x, Some(&el));
                        self.coerce(h, &el)
                    })
                    .collect();
                if let (Some(l), Some(es)) = (&hlen, elems) {
                    // C#: the given length must be a constant equal to the number of elements
                    match crate::consteval::eval_expr(&self.p, l) {
                        Ok(ConstVal::Int(n)) if n >= 0 && n as usize == es.len() => {}
                        _ => self.d.error("E0241", sp, format!("an array initializer of length {} is expected (the length must be a constant equal to the number of elements)", es.len())),
                    }
                }
                if elems.is_none() && matches!(el, Ty::Handle(_)) {
                    let en = self.p.ty_name(&el);
                    self.d.error_note("E0302", sp, format!("the elements of 'new {}[n]' would start as null", en), format!("use a nullable element type: new {}?[n]", en));
                }
                let site = self.new_alloc_site(el.clone(), sp);
                let len = if elems.is_some() { None } else { hlen.map(Box::new) };
                Expr::new(ExprKind::ArrayNew { site, len, elems: helems, dims: vec![] }, Ty::Array(Box::new(el)), sp)
            }
            A::Switch(scrut, arms) => self.switch_expr(scrut, arms, expect, sp),
            A::Await(_) => {
                self.d.error_note("E0803", sp, "'await' must be a statement of its own", "write 'await Wait(1.0);' on its own line");
                Expr::err(sp)
            }
            A::IsPattern(x, pat) => self.is_pattern(x, pat, sp),
            A::Lambda { params, body } => match expect {
                Some(Ty::Delegate(dg)) => self.lambda_value(params, body, *dg, sp),
                Some(Ty::Error) => Expr::err(sp),
                _ => {
                    self.d.error_note(
                        "E0027",
                        sp,
                        "a lambda needs a delegate type here",
                        "give the variable, field or parameter a delegate type (Func<int, int> f = x => x * 2;, Action a = () => ...;), or pass the lambda to a list method (Sort, Find, Where, ...)",
                    );
                    Expr::err(sp)
                }
            },
        }
    }

    // ======================================================================
    // switch and patterns
    // ======================================================================

    fn check_switch_type(&mut self, ty: &Ty, sp: Span) {
        match ty {
            Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::Str | Ty::Handle(_) | Ty::NHandle(_) | Ty::Error => {}
            t => {
                let tn = self.p.ty_name(t);
                self.d.error("E0239", sp, format!("cannot switch on a value of type '{}' (int, float, bool, char, enum, string or handle expected)", tn));
            }
        }
    }

    /// the expression the patterns test: the scrutinee itself when re-reading it is free and has no
    /// side effects, otherwise a hidden local assigned once
    fn switch_subject(&mut self, hs: &Expr) -> (Option<LocalId>, Expr) {
        let dup = matches!(
            hs.kind,
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Null | ExprKind::Str(_) | ExprKind::EnumVal(..) | ExprKind::Const(_) | ExprKind::Local(_) | ExprKind::SelfRef
        );
        if dup || hs.ty == Ty::Error {
            return (None, hs.clone());
        }
        let name = format!("__sw{}", self.st().locals.len());
        let l = self.declare_local(&name, hs.ty.clone(), LocalKind::Normal, hs.span);
        (Some(l), Expr::new(ExprKind::Local(l), hs.ty.clone(), hs.span))
    }

    fn is_const_expr(e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Null | ExprKind::Str(_) | ExprKind::EnumVal(..) | ExprKind::Const(_) => true,
            ExprKind::Unary(_, x) | ExprKind::Cast(x) => Self::is_const_expr(x),
            ExprKind::Binary(_, a, b) => Self::is_const_expr(a) && Self::is_const_expr(b),
            _ => false,
        }
    }

    /// condition testing `subj` against `pat`, plus its shape for exhaustiveness analysis
    fn lower_pattern(&mut self, pat: &ast::Pattern, subj: &Expr, ty: &Ty) -> (Expr, PK) {
        let bool_e = |k: ExprKind, sp: Span| Expr::new(k, Ty::Bool, sp);
        match pat {
            ast::Pattern::Discard(sp) => (bool_e(ExprKind::Bool(true), *sp), PK::Any),
            ast::Pattern::Const(e) => {
                let sp = e.span;
                if matches!(e.kind, ast::ExprKind::Null) {
                    if !matches!(ty, Ty::NHandle(_) | Ty::Error) {
                        let tn = self.p.ty_name(ty);
                        self.d.error("E0239", sp, format!("'null' pattern on a non-nullable '{}'", tn));
                    }
                    let n = Expr::new(ExprKind::Null, Ty::Null, sp);
                    let c = bool_e(ExprKind::Binary(BinOp::Eq, Box::new(subj.clone()), Box::new(n.clone())), sp);
                    return (c, PK::Val(n));
                }
                if matches!(ty, Ty::Handle(_) | Ty::NHandle(_)) {
                    self.d.error("E0239", sp, "handles can only be matched against 'null' (use 'is T x' for type tests)");
                    return (bool_e(ExprKind::Bool(false), sp), PK::Any);
                }
                let he = self.expr(e, Some(ty));
                let he = self.coerce(he, ty);
                if he.ty != Ty::Error && !Self::is_const_expr(&he) {
                    self.d.error("E0239", sp, "a case pattern must be a compile-time constant");
                }
                let c = bool_e(ExprKind::Binary(BinOp::Eq, Box::new(subj.clone()), Box::new(he.clone())), sp);
                (c, PK::Val(he))
            }
            ast::Pattern::Rel(op, e, sp) => {
                if !matches!(ty, Ty::Int | Ty::Float | Ty::Char | Ty::Error) {
                    let tn = self.p.ty_name(ty);
                    self.d.error("E0239", *sp, format!("relational patterns need an int, float or char value, found '{}'", tn));
                    return (bool_e(ExprKind::Bool(false), *sp), PK::Any);
                }
                let he = self.expr(e, Some(ty));
                let he = self.coerce(he, ty);
                if he.ty != Ty::Error && !Self::is_const_expr(&he) {
                    self.d.error("E0239", e.span, "a relational pattern needs a compile-time constant");
                }
                let c = bool_e(ExprKind::Binary(*op, Box::new(subj.clone()), Box::new(he.clone())), *sp);
                (c, PK::Rel(*op, he))
            }
            ast::Pattern::Not(p, sp) => {
                let (c, k) = self.lower_pattern(p, subj, ty);
                (bool_e(ExprKind::Unary(UnOp::Not, Box::new(c)), *sp), PK::Not(Box::new(k)))
            }
            ast::Pattern::And(a, b) | ast::Pattern::Or(a, b) => {
                let (ca, ka) = self.lower_pattern(a, subj, ty);
                let (cb, kb) = self.lower_pattern(b, subj, ty);
                let sp = pat.span();
                if matches!(pat, ast::Pattern::And(..)) {
                    (bool_e(ExprKind::Binary(BinOp::And, Box::new(ca), Box::new(cb)), sp), PK::And(Box::new(ka), Box::new(kb)))
                } else {
                    (bool_e(ExprKind::Binary(BinOp::Or, Box::new(ca), Box::new(cb)), sp), PK::Or(Box::new(ka), Box::new(kb)))
                }
            }
        }
    }

    fn switch_expr(&mut self, scrut: &ast::Expr, arms: &[ast::SwitchArm], expect: Option<&Ty>, sp: Span) -> Expr {
        let hs = self.expr(scrut, None);
        let ty = hs.ty.clone();
        self.check_switch_type(&ty, scrut.span);
        if arms.is_empty() {
            self.d.error("E0237", sp, "a switch expression needs at least one arm");
            return Expr::err(sp);
        }
        let (tmp, subj) = self.switch_subject(&hs);
        let mut check = SwitchCheck { ty: ty.clone(), arms: vec![], has_default: false, is_expr: true, span: sp };
        let mut harms: Vec<(Expr, Expr)> = vec![];
        let mut default: Option<Expr> = None;
        let mut rty: Option<Ty> = expect.cloned();
        let mut first_ty: Option<Ty> = None;
        for arm in arms {
            self.push_scope();
            let (c, k) = self.lower_pattern(&arm.pat, &subj, &ty);
            let c = match &arm.guard {
                Some(g) => {
                    let hg = self.cond_expr(g);
                    Expr::new(ExprKind::Binary(BinOp::And, Box::new(c), Box::new(hg)), Ty::Bool, arm.span)
                }
                None => c,
            };
            let v = self.expr(&arm.value, rty.as_ref());
            self.pop_scope();
            let t = match &first_ty {
                None => v.ty.clone(),
                Some(t) => self.unify(t, &v.ty, arm.value.span),
            };
            if rty.is_none() || expect.is_none() {
                rty = Some(t.clone());
            }
            first_ty = Some(t);
            let is_catch_all = matches!(k, PK::Any) && arm.guard.is_none();
            check.arms.push((k, arm.guard.is_some(), arm.pat.span()));
            if is_catch_all && default.is_none() {
                default = Some(v);
            } else if default.is_none() {
                harms.push((c, v));
            }
        }
        let t = first_ty.unwrap_or(Ty::Error);
        let t = match expect {
            Some(x) if self.can_coerce(&t, x) => x.clone(),
            _ => t,
        };
        if t == Ty::Void || t == Ty::Null {
            self.d.error("E0237", sp, "switch expression arms must produce a value");
        }
        if t.is_list() {
            self.d.error("E0237", sp, "a switch expression cannot produce a list; use a switch statement");
        }
        let harms: Vec<(Expr, Expr)> = harms.into_iter().map(|(c, v)| (c, self.coerce(v, &t))).collect();
        let default = default.map(|v| Box::new(self.coerce(v, &t)));
        self.switch_checks.push(check);
        if harms.is_empty() && tmp.is_none() && default.is_some() {
            // only a discard arm and a side-effect-free scrutinee
            return *default.unwrap();
        }
        Expr::new(ExprKind::Switch { tmp, scrut: Box::new(hs), arms: harms, default: None }, t, sp).with_default(default)
    }

    fn is_pattern(&mut self, x: &ast::Expr, pat: &ast::Pattern, sp: Span) -> Expr {
        let hs = self.expr(x, None);
        let ty = hs.ty.clone();
        self.check_switch_type(&ty, x.span);
        let (tmp, subj) = self.switch_subject(&hs);
        let (c, _) = self.lower_pattern(pat, &subj, &ty);
        if tmp.is_none() {
            return c;
        }
        let f = Expr::new(ExprKind::Bool(false), Ty::Bool, sp);
        let t = Expr::new(ExprKind::Bool(true), Ty::Bool, sp);
        Expr::new(ExprKind::Switch { tmp, scrut: Box::new(hs), arms: vec![(c, t)], default: Some(Box::new(f)) }, Ty::Bool, sp)
    }

    /// constant value of a pattern operand as an integer point (ints, enums, bools, null)
    fn pat_point(&self, e: &Expr) -> Option<i64> {
        match consteval::eval_expr_raw(&self.p, e).ok()? {
            ConstVal::Int(i) => Some(i as i64),
            ConstVal::Enum(_, v) => Some(v),
            ConstVal::Bool(b) => Some(b as i64),
            ConstVal::Null => Some(0),
            _ => None,
        }
    }

    fn pat_set(&self, k: &PK, universe: &ISet, ty: &Ty) -> Option<ISet> {
        Some(match k {
            PK::Any => universe.clone(),
            PK::Val(e) => {
                let v = self.pat_point(e)?;
                ISet::point(v).intersect(universe)
            }
            PK::Rel(op, e) => {
                if *ty != Ty::Int {
                    return None;
                }
                let v = self.pat_point(e)?;
                let (lo, hi) = (i32::MIN as i64, i32::MAX as i64);
                let r = match op {
                    BinOp::Lt => ISet::range(lo, v - 1),
                    BinOp::Le => ISet::range(lo, v),
                    BinOp::Gt => ISet::range(v + 1, hi),
                    BinOp::Ge => ISet::range(v, hi),
                    _ => return None,
                };
                r.intersect(universe)
            }
            PK::Not(p) => universe.minus(&self.pat_set(p, universe, ty)?),
            PK::And(a, b) => self.pat_set(a, universe, ty)?.intersect(&self.pat_set(b, universe, ty)?),
            PK::Or(a, b) => self.pat_set(a, universe, ty)?.union(&self.pat_set(b, universe, ty)?),
        })
    }

    fn analyze_switch(&mut self, c: &SwitchCheck) {
        let universe = match &c.ty {
            Ty::Int => Some(ISet::range(i32::MIN as i64, i32::MAX as i64)),
            Ty::Char => Some(ISet::range(0, 0x10FFFF)),
            Ty::Bool => Some(ISet::range(0, 1)),
            Ty::Enum(e) => {
                let mut u = ISet::default();
                for (_, v) in &self.p.enums[*e].variants {
                    u = u.union(&ISet::point(*v));
                }
                Some(u)
            }
            Ty::NHandle(_) => Some(ISet::range(0, 1)),
            Ty::Handle(_) => Some(ISet::point(1)),
            _ => None,
        };
        let mut covered = ISet::default();
        let mut catch_all = false;
        // float/string: only discards make a switch exhaustive; duplicates are found by value
        let mut seen_vals: Vec<ConstVal> = vec![];
        for (k, guarded, sp) in &c.arms {
            if catch_all {
                self.d.error("E0238", *sp, "unreachable pattern: a previous arm already matches every value");
                continue;
            }
            if matches!(k, PK::Any) && !guarded {
                catch_all = true;
                continue;
            }
            match &universe {
                Some(u) => {
                    let Some(set) = self.pat_set(k, u, &c.ty) else { continue };
                    if set.is_empty() {
                        self.d.error("E0238", *sp, "this pattern never matches");
                    } else if set.subset_of(&covered) {
                        self.d.error("E0238", *sp, "unreachable pattern: the values it matches are handled by previous cases");
                    } else if !guarded {
                        covered = covered.union(&set);
                    }
                }
                None => {
                    if let PK::Val(e) = k {
                        if let Ok(v) = consteval::eval_expr_raw(&self.p, e) {
                            if seen_vals.contains(&v) && !guarded {
                                self.d.error("E0238", *sp, "duplicate case value");
                            } else if !guarded {
                                seen_vals.push(v);
                            }
                        }
                    }
                }
            }
        }
        if c.has_default || catch_all {
            return;
        }
        let missing: Option<String> = match &universe {
            Some(u) => {
                let rest = u.minus(&covered);
                if rest.is_empty() {
                    None
                } else {
                    Some(match &c.ty {
                        Ty::Enum(e) => {
                            let en = &self.p.enums[*e];
                            let names: Vec<String> = en
                                .variants
                                .iter()
                                .filter(|(_, v)| rest.contains(*v))
                                .map(|(n, _)| format!("{}.{}", en.name, n))
                                .collect();
                            names.join(", ")
                        }
                        Ty::Bool => (if rest.contains(0) { "false" } else { "true" }).to_string(),
                        Ty::NHandle(_) => (if rest.contains(0) { "null" } else { "not null" }).to_string(),
                        _ => format!("e.g. {}", rest.first().unwrap_or(0)),
                    })
                }
            }
            None => Some("other values (add a '_' arm)".to_string()),
        };
        if let Some(m) = missing {
            if c.is_expr {
                self.d.error_note("E0237", c.span, format!("the switch expression does not handle all values: {}", m), "add the missing arms or a discard arm '_ => ...'");
            } else {
                self.d.error_note("E0237", c.span, format!("the switch statement does not handle all values: {}", m), "add the missing cases or a 'default:' section");
            }
        }
    }

    fn unify(&mut self, a: &Ty, b: &Ty, sp: Span) -> Ty {
        if a == b {
            return a.clone();
        }
        if self.can_coerce(a, b) {
            return b.clone();
        }
        if self.can_coerce(b, a) {
            return a.clone();
        }
        match (a, b) {
            (Ty::Null, Ty::Handle(c)) | (Ty::Handle(c), Ty::Null) => Ty::NHandle(*c),
            (Ty::Handle(x) | Ty::NHandle(x), Ty::Handle(y) | Ty::NHandle(y)) => {
                // common ancestor
                let mut cur = Some(*x);
                while let Some(c) = cur {
                    if self.p.is_subclass(*y, c) {
                        let nullable = matches!(a, Ty::NHandle(_)) || matches!(b, Ty::NHandle(_));
                        return if nullable { Ty::NHandle(c) } else { Ty::Handle(c) };
                    }
                    cur = self.p.classes[c].base;
                }
                self.d.error("E0200", sp, "branches have incompatible handle types");
                Ty::Error
            }
            _ => {
                let (x, y) = (self.p.ty_name(a), self.p.ty_name(b));
                self.d.error("E0200", sp, format!("branches have incompatible types '{}' and '{}'", x, y));
                Ty::Error
            }
        }
    }

    fn arith_type(&mut self, op: BinOp, a: &Ty, b: &Ty, sp: Span) -> Ty {
        use BinOp::*;
        if *a == Ty::Error || *b == Ty::Error {
            return Ty::Error;
        }
        match op {
            Add | Sub | Mul | Div | Rem => match (a, b) {
                (Ty::Int, Ty::Int) => Ty::Int,
                (Ty::Float | Ty::Int, Ty::Float | Ty::Int) => Ty::Float,
                _ => {
                    let (x, y) = (self.p.ty_name(a), self.p.ty_name(b));
                    self.d.error("E0239", sp, format!("operator '{}' cannot be applied to '{}' and '{}'", op.sym(), x, y));
                    Ty::Error
                }
            },
            BitAnd | BitOr | BitXor => match (a, b) {
                (Ty::Int, Ty::Int) => Ty::Int,
                (Ty::Bool, Ty::Bool) => Ty::Bool,
                _ => {
                    self.d.error("E0239", sp, format!("operator '{}' requires int or bool operands", op.sym()));
                    Ty::Error
                }
            },
            Shl | Shr => match (a, b) {
                (Ty::Int, Ty::Int) => Ty::Int,
                _ => {
                    self.d.error("E0239", sp, "shift operators require int operands");
                    Ty::Error
                }
            },
            _ => Ty::Error,
        }
    }

    fn binary(&mut self, op: BinOp, a: &ast::Expr, b: &ast::Expr, sp: Span) -> Expr {
        use BinOp::*;
        let ha = self.expr(a, None);
        let hb = self.expr(b, if ha.ty == Ty::Float { Some(&Ty::Float) } else { None });
        let ha = if hb.ty == Ty::Float && ha.ty == Ty::Int { self.coerce(ha, &Ty::Float) } else { ha };
        if ha.ty == Ty::Error || hb.ty == Ty::Error {
            return Expr::err(sp);
        }
        // struct operators
        let sa = if let Ty::Struct(s) = ha.ty { Some(s) } else { None };
        let sb = if let Ty::Struct(s) = hb.ty { Some(s) } else { None };
        if sa.is_some() || sb.is_some() {
            let sym = op.sym();
            match self.find_op(sa.or(sb).unwrap(), sb, sym, &[ha.ty.clone(), hb.ty.clone()]) {
                Some(f) => {
                    let rt = self.p.funcs[f].ret.clone();
                    let args = self.coerce_args_to(f, vec![ha, hb]);
                    return Expr::new(ExprKind::OpCall { func: f, args }, rt, sp);
                }
                None => {
                    let (x, y) = (self.p.ty_name(&ha.ty), self.p.ty_name(&hb.ty));
                    self.d.error("E0239", sp, format!("operator '{}' is not defined for '{}' and '{}'", sym, x, y));
                    return Expr::err(sp);
                }
            }
        }
        let mk = |op, a, b, t| Expr::new(ExprKind::Binary(op, Box::new(a), Box::new(b)), t, sp);
        if op == Add && (ha.ty == Ty::Str || hb.ty == Ty::Str) {
            let ha = self.to_str(ha);
            let hb = self.to_str(hb);
            return mk(Add, ha, hb, Ty::Str);
        }
        // char operands of arithmetic, bitwise and ordering operators are their codes (int), as in
        // C# ('a' + 1 is 98, c - '0' is a digit's value); two chars compare by code
        let (ha, hb) = if (ha.ty == Ty::Char || hb.ty == Ty::Char) && !matches!(op, And | Or) {
            let both = ha.ty == Ty::Char && hb.ty == Ty::Char;
            if both && matches!(op, Eq | Ne) {
                (ha, hb)
            } else {
                let to = if ha.ty == Ty::Float || hb.ty == Ty::Float { Ty::Float } else { Ty::Int };
                let ca = if ha.ty == Ty::Char { self.coerce(ha, &to) } else { ha };
                let cb = if hb.ty == Ty::Char { self.coerce(hb, &to) } else { hb };
                (ca, cb)
            }
        } else {
            (ha, hb)
        };
        match op {
            And | Or => {
                let ha = self.coerce(ha, &Ty::Bool);
                let hb = self.coerce(hb, &Ty::Bool);
                mk(op, ha, hb, Ty::Bool)
            }
            Eq | Ne => {
                let ok = match (&ha.ty, &hb.ty) {
                    (x, y) if x == y => matches!(x, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::Handle(_) | Ty::NHandle(_) | Ty::Str),
                    (Ty::Int | Ty::Float, Ty::Int | Ty::Float) => true,
                    (Ty::Handle(x) | Ty::NHandle(x), Ty::Handle(y) | Ty::NHandle(y)) => {
                        self.p.is_subclass(*x, *y) || self.p.is_subclass(*y, *x)
                    }
                    (Ty::Null, Ty::Handle(_) | Ty::NHandle(_)) | (Ty::Handle(_) | Ty::NHandle(_), Ty::Null) => true,
                    _ => false,
                };
                if !ok {
                    let (x, y) = (self.p.ty_name(&ha.ty), self.p.ty_name(&hb.ty));
                    self.d.error("E0239", sp, format!("cannot compare '{}' with '{}'", x, y));
                    return Expr::err(sp);
                }
                mk(op, ha, hb, Ty::Bool)
            }
            Lt | Le | Gt | Ge => {
                let ok = matches!((&ha.ty, &hb.ty), (Ty::Int | Ty::Float, Ty::Int | Ty::Float))
                    || (matches!(ha.ty, Ty::Enum(_)) && ha.ty == hb.ty);
                if !ok {
                    let (x, y) = (self.p.ty_name(&ha.ty), self.p.ty_name(&hb.ty));
                    self.d.error("E0239", sp, format!("cannot order '{}' and '{}'", x, y));
                    return Expr::err(sp);
                }
                mk(op, ha, hb, Ty::Bool)
            }
            _ => {
                let t = self.arith_type(op, &ha.ty, &hb.ty, sp);
                if t == Ty::Error {
                    return Expr::err(sp);
                }
                let ha = self.coerce(ha, &t);
                let hb = self.coerce(hb, &t);
                // C# rejects integer division by a constant zero (CS0020) instead of failing at run time
                if t == Ty::Int && matches!(op, Div | Rem) && matches!(crate::consteval::eval_expr(&self.p, &hb), Ok(ConstVal::Int(0))) {
                    self.d.error("E0256", hb.span, "division by constant zero");
                }
                mk(op, ha, hb, t)
            }
        }
    }

    fn find_op(&self, s1: StructId, s2: Option<StructId>, sym: &str, args: &[Ty]) -> Option<FuncId> {
        let name = format!("operator{}", sym);
        let mut cands = self.p.structs[s1].ops.clone();
        if let Some(s2) = s2 {
            if s2 != s1 {
                cands.extend(self.p.structs[s2].ops.clone());
            }
        }
        // exact match first, then with int->float coercion
        for pass in 0..2 {
            for &f in &cands {
                let func = &self.p.funcs[f];
                if func.name != name || func.params.len() != args.len() {
                    continue;
                }
                let ok = func.params.iter().zip(args).all(|(&l, a)| {
                    let pt = &func.locals[l].ty;
                    if pass == 0 {
                        pt == a
                    } else {
                        self.can_coerce(a, pt)
                    }
                });
                if ok {
                    return Some(f);
                }
            }
        }
        None
    }

    fn coerce_args_to(&mut self, f: FuncId, args: Vec<Expr>) -> Vec<Expr> {
        let tys: Vec<Ty> = self.p.funcs[f].params.iter().map(|&l| self.p.funcs[f].locals[l].ty.clone()).collect();
        args.into_iter().zip(tys).map(|(a, t)| self.coerce(a, &t)).collect()
    }

    fn name_expr(&mut self, n: &str, sp: Span) -> Expr {
        if let Some(l) = self.lookup_local(n) {
            let ty = self.st.as_ref().unwrap().locals[l].ty.clone();
            if let Some(f) = self.st.as_ref().unwrap().fid {
                self.reference(sp, Sym::Local(f, l));
            }
            return Expr::new(ExprKind::Local(l), ty, sp);
        }
        let (owner, is_static) = match self.st.as_ref() {
            Some(s) => (s.owner, s.is_static),
            None => return Expr::err(sp),
        };
        match owner {
            Owner::Struct(s) => {
                if let Some(idx) = self.p.structs[s].fields.iter().position(|f| f.name == n) {
                    if is_static {
                        self.d.error("E0106", sp, format!("cannot access instance field '{}' from a static context", n));
                        return Expr::err(sp);
                    }
                    let ty = self.p.structs[s].fields[idx].ty.clone();
                    self.reference(sp, Sym::SField(s, idx));
                    let base = Expr::new(ExprKind::StructSelf, Ty::Struct(s), sp);
                    return Expr::new(ExprKind::Field { base: Box::new(base), sid: s, idx }, ty, sp);
                }
                if let Some(&f) = self.p.structs[s].methods.get(n) {
                    if self.p.funcs[f].is_property && !is_static {
                        self.reference(sp, Sym::Func(f));
                        let base = Expr::new(ExprKind::StructSelf, Ty::Struct(s), sp);
                        let rt = self.p.funcs[f].ret.clone();
                        return Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(base)), args: vec![], dispatch: Dispatch::Static }, rt, sp);
                    }
                }
                if let Some(&c) = self.consts.get(&(TypeRef::Struct(s), n.to_string())) {
                    self.reference(sp, Sym::Const(c));
                    return Expr::new(ExprKind::Const(c), self.p.consts[c].ty.clone(), sp);
                }
            }
            Owner::Entity(c) | Owner::Static(c) => {
                if let Some(f) = self.p.find_field(c, n) {
                    if is_static || !self.p.classes[c].is_entity {
                        self.d.error("E0106", sp, format!("cannot access instance field '{}' from a static context", n));
                        return Expr::err(sp);
                    }
                    let ty = self.p.fields[f].ty.clone();
                    self.reference(sp, Sym::Field(f));
                    let base = Expr::new(ExprKind::SelfRef, Ty::Handle(c), sp);
                    return Expr::new(ExprKind::EField { base: Box::new(base), field: f }, ty, sp);
                }
                if let Some(f) = self.p.resolve_method(c, n) {
                    if self.p.funcs[f].is_property && !is_static {
                        self.reference(sp, Sym::Func(f));
                        let rt = self.p.funcs[f].ret.clone();
                        let base = Expr::new(ExprKind::SelfRef, Ty::Handle(c), sp);
                        return Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(base)), args: vec![], dispatch: Dispatch::SelfCall }, rt, sp);
                    }
                }
                // `Owner` in a component method: the entity the component belongs to
                if let (true, false, Some(root)) = (n == "Owner" && self.p.classes[c].is_component, is_static, self.p.entity_root) {
                    return Expr::new(ExprKind::SelfRef, Ty::Handle(root), sp);
                }
                let mut cur = Some(c);
                while let Some(x) = cur {
                    if let Some(&k) = self.consts.get(&(TypeRef::Class(x), n.to_string())) {
                        self.reference(sp, Sym::Const(k));
                        return Expr::new(ExprKind::Const(k), self.p.consts[k].ty.clone(), sp);
                    }
                    cur = self.p.classes[x].base;
                }
            }
        }
        if let Some(tps) = self.ctemplates.get(n).map(|&ti| &self.ctmpls[ti].decl.type_params).or_else(|| self.stemplates.get(n).map(|&ti| &self.stmpls[ti].decl.type_params)) {
            let example = format!("{}<{}>", n, vec!["int"; tps.len()].join(", "));
            self.d.error_note("E0228", sp, format!("generic type '{}' needs {} type argument(s)", n, tps.len()), format!("write them, e.g. '{}'", example));
        } else if self.types.contains_key(n) || NAMESPACES.contains(&n) {
            self.d.error("E0107", sp, format!("'{}' is a type, not a value", n));
        } else if let Some(note) = self.default_body_note(n) {
            self.d.error_note("E0101", sp, format!("unknown name '{}'", n), note);
        } else {
            self.d.error("E0101", sp, format!("unknown name '{}'", n));
        }
        Expr::err(sp)
    }

    /// In an interface's default implementation, which sees only the interface: how to reach the
    /// implementer's `name`.
    fn default_body_note(&self, name: &str) -> Option<String> {
        let c = self.self_class().filter(|&c| self.p.classes[c].is_interface)?;
        let cn = &self.p.classes[c].name;
        Some(format!(
            "a default implementation sees only its interface: 'this' is an '{}', not the class or struct that implements it; declare what it needs in '{}' (e.g. 'int {}();') and call that",
            cn, cn, name
        ))
    }

    /// Resolve `Name` used as the base of a member access to a type (if it is not a value).
    fn type_base(&self, base: &ast::Expr) -> Option<Result<TypeRef, &'static str>> {
        if let ast::ExprKind::Name(n) = &base.kind {
            if self.lookup_local(n).is_some() {
                return None;
            }
            // a field or property of self shadows type names
            if let Some(st) = self.st.as_ref() {
                match st.owner {
                    Owner::Entity(c) => {
                        if self.p.find_field(c, n).is_some() {
                            return None;
                        }
                    }
                    Owner::Struct(s) => {
                        if self.p.structs[s].fields.iter().any(|f| &f.name == n) {
                            return None;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(&t) = self.types.get(n) {
                return Some(Ok(t));
            }
            if let Some(ns) = NAMESPACES.iter().find(|x| *x == n) {
                return Some(Err(ns));
            }
            // `T.Zero()`: a type parameter stands for its type argument (each instance is checked
            // with its own), so this reaches that type's static members
            if let Some((_, ty)) = self.tenv.iter().rev().find(|(x, _)| x == n) {
                return match ty {
                    Ty::Struct(s) => Some(Ok(TypeRef::Struct(*s))),
                    Ty::Handle(c) | Ty::NHandle(c) => Some(Ok(TypeRef::Class(*c))),
                    Ty::Enum(e) => Some(Ok(TypeRef::Enum(*e))),
                    Ty::Int => Some(Err("int")),
                    Ty::Float => Some(Err("float")),
                    Ty::Char => Some(Err("char")),
                    Ty::Str => Some(Err("string")),
                    // unknown (unused generic code): anything goes, nothing to report
                    Ty::Error => Some(Err("?")),
                    _ => None,
                };
            }
        }
        None
    }

    /// `Util<int>` before `.`: a type, or an instance of a generic static class
    fn resolve_static_ok(&mut self, t: &ast::TypeExpr) -> Ty {
        let saved = std::mem::replace(&mut self.static_ok, true);
        let ty = self.resolve_type(t);
        self.static_ok = saved;
        ty
    }

    fn member_expr(&mut self, base: &ast::Expr, name: &str, nsp: Span, sp: Span) -> Expr {
        // `Limits<int>.Max`: a constant of a generic static class instance (or generic struct)
        if let ast::ExprKind::GenericName(t) = &base.kind {
            let tr = match self.resolve_static_ok(t) {
                Ty::Struct(s) => TypeRef::Struct(s),
                Ty::Handle(c) => TypeRef::Class(c),
                _ => return Expr::err(sp),
            };
            if let Some(&k) = self.consts.get(&(tr, name.to_string())) {
                self.reference(nsp, Sym::Const(k));
                return Expr::new(ExprKind::Const(k), self.p.consts[k].ty.clone(), sp);
            }
            self.d.error("E0108", nsp, format!("'{}' has no constant named '{}' (methods must be called)", type_text(t), name));
            return Expr::err(sp);
        }
        match self.type_base(base) {
            Some(Err("Time")) => {
                return match name {
                    "Tick" => Expr::new(ExprKind::Builtin { f: Builtin::TimeTick, args: vec![], site: 0 }, Ty::Int, sp),
                    "Dt" => Expr::new(ExprKind::Builtin { f: Builtin::TimeDt, args: vec![], site: 0 }, Ty::Float, sp),
                    _ => {
                        self.d.error("E0108", nsp, format!("'Time' has no member '{}' (available: Tick, Dt)", name));
                        Expr::err(sp)
                    }
                };
            }
            Some(Err("?")) => return Expr::err(sp),
            Some(Err(ns)) => {
                // C# constants of the built-in types and Math (float for Math, like MathF)
                let c = match (ns, name) {
                    ("int", "MaxValue") => Some(ConstVal::Int(i32::MAX)),
                    ("int", "MinValue") => Some(ConstVal::Int(i32::MIN)),
                    ("char", "MaxValue") => return Expr::new(ExprKind::Int(0xFFFF), Ty::Char, sp),
                    ("char", "MinValue") => return Expr::new(ExprKind::Int(0), Ty::Char, sp),
                    ("float", "MaxValue") => Some(ConstVal::Float(f32::MAX)),
                    ("float", "MinValue") => Some(ConstVal::Float(f32::MIN)),
                    ("float", "Epsilon") => Some(ConstVal::Float(f32::from_bits(1))),
                    ("float", "NaN") => Some(ConstVal::Float(f32::NAN)),
                    ("float", "PositiveInfinity") => Some(ConstVal::Float(f32::INFINITY)),
                    ("float", "NegativeInfinity") => Some(ConstVal::Float(f32::NEG_INFINITY)),
                    ("Math", "PI") => Some(ConstVal::Float(std::f32::consts::PI)),
                    ("Math", "E") => Some(ConstVal::Float(std::f32::consts::E)),
                    ("Math", "Tau") => Some(ConstVal::Float(std::f32::consts::TAU)),
                    ("Grid", "Stay") => Some(ConstVal::Int(0)),
                    _ => None,
                };
                match c {
                    Some(ConstVal::Int(v)) => return Expr::new(ExprKind::Int(v as i64), Ty::Int, sp),
                    Some(ConstVal::Float(v)) => return Expr::new(ExprKind::Float(v as f64), Ty::Float, sp),
                    _ => {}
                }
                self.d.error("E0108", nsp, format!("'{}.{}' must be called", ns, name));
                return Expr::err(sp);
            }
            Some(Ok(TypeRef::Enum(e))) => {
                self.reference(base.span, Sym::Enum(e));
                if let Some(i) = self.p.enums[e].variants.iter().position(|(n, _)| n == name) {
                    self.reference(nsp, Sym::Variant(e, i));
                }
                return match self.p.enums[e].variants.iter().find(|(n, _)| n == name) {
                    Some((_, v)) => Expr::new(ExprKind::EnumVal(e, *v), Ty::Enum(e), sp),
                    None => {
                        let en = self.p.enums[e].name.clone();
                        self.d.error("E0108", nsp, format!("enum '{}' has no member '{}'", en, name));
                        Expr::err(sp)
                    }
                };
            }
            Some(Ok(t)) => {
                self.reference(base.span, Self::type_sym(t));
                let mut cur = Some(t);
                while let Some(tr) = cur {
                    if let Some(&c) = self.consts.get(&(tr, name.to_string())) {
                        self.reference(nsp, Sym::Const(c));
                        return Expr::new(ExprKind::Const(c), self.p.consts[c].ty.clone(), sp);
                    }
                    cur = match tr {
                        TypeRef::Class(c) => self.p.classes[c].base.map(TypeRef::Class),
                        _ => None,
                    };
                }
                self.d.error("E0108", nsp, format!("no constant named '{}'", name));
                return Expr::err(sp);
            }
            None => {}
        }
        let hb = self.expr(base, None);
        match hb.ty.clone() {
            Ty::Error => Expr::err(sp),
            Ty::Struct(s) => {
                if let Some(idx) = self.p.structs[s].fields.iter().position(|f| f.name == name) {
                    let ty = self.p.structs[s].fields[idx].ty.clone();
                    self.reference(nsp, Sym::SField(s, idx));
                    return Expr::new(ExprKind::Field { base: Box::new(hb), sid: s, idx }, ty, sp);
                }
                if let Some(&f) = self.p.structs[s].methods.get(name) {
                    self.reference(nsp, Sym::Func(f));
                    if self.p.funcs[f].is_property {
                        let rt = self.p.funcs[f].ret.clone();
                        return Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(hb)), args: vec![], dispatch: Dispatch::Static }, rt, sp);
                    }
                    self.d.error("E0108", nsp, format!("method '{}' must be called", name));
                    return Expr::err(sp);
                }
                let sn = self.p.structs[s].name.clone();
                self.d.error("E0108", nsp, format!("'{}' has no field '{}'", sn, name));
                Expr::err(sp)
            }
            Ty::Handle(c) => {
                if let Some(f) = self.p.find_field(c, name) {
                    let ty = self.p.fields[f].ty.clone();
                    self.reference(nsp, Sym::Field(f));
                    return Expr::new(ExprKind::EField { base: Box::new(hb), field: f }, ty, sp);
                }
                if let Some(f) = self.p.resolve_method(c, name) {
                    self.reference(nsp, Sym::Func(f));
                    if self.p.funcs[f].is_property {
                        let rt = self.p.funcs[f].ret.clone();
                        let disp = if matches!(hb.kind, ExprKind::SelfRef) { Dispatch::SelfCall } else { Dispatch::RemoteRead };
                        return Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(hb)), args: vec![], dispatch: disp }, rt, sp);
                    }
                }
                let cn = self.p.classes[c].name.clone();
                if self.p.classes[c].is_interface {
                    self.d.error_note("E0108", nsp, format!("'{}' has no field or property '{}'", cn, name), format!("an interface has no fields: declare 'int {}();' in '{}' (or a default property 'int {} => ...;') and implement it", name, cn, name));
                } else {
                    self.d.error("E0108", nsp, format!("'{}' has no field or property '{}'", cn, name));
                }
                Expr::err(sp)
            }
            Ty::NHandle(c) => {
                let cn = self.p.classes[c].name.clone();
                let note = self.get_note(base, &cn).unwrap_or_else(|| format!("check it first: 'if (x is {} y) {{ ... y.{} ... }}'", cn, name));
                self.d.error_note("E0301", base.span, format!("cannot access '{}' through a nullable handle '{}?'", name, cn), note);
                Expr::err(sp)
            }
            Ty::Str => {
                if name == "Length" {
                    return Expr::new(ExprKind::Builtin { f: Builtin::StrLen, args: vec![hb], site: 0 }, Ty::Int, sp);
                }
                self.d.error("E0108", nsp, format!("string has no property '{}' (available: Length)", name));
                Expr::err(sp)
            }
            Ty::Dict(..) => {
                if name == "Count" {
                    return Expr::new(ExprKind::ListLen(Box::new(hb)), Ty::Int, sp);
                }
                if name == "Keys" || name == "Values" {
                    self.d.error("E0108", nsp, format!("'{}' can be used in 'foreach (var x in d.{})'", name, name));
                    return Expr::err(sp);
                }
                self.d.error("E0108", nsp, format!("Dictionary has no property '{}' (available: Count, Keys, Values)", name));
                Expr::err(sp)
            }
            Ty::List(_) => {
                if name == "Count" {
                    return Expr::new(ExprKind::ListLen(Box::new(hb)), Ty::Int, sp);
                }
                self.d.error("E0108", nsp, format!("List<T> has no property '{}' (available: Count)", name));
                Expr::err(sp)
            }
            Ty::Array(_) | Ty::MdArray(..) => {
                if name == "Length" {
                    // the number of elements (of all dimensions)
                    return Expr::new(ExprKind::ListLen(Box::new(hb)), Ty::Int, sp);
                }
                // a grid's sizes: Width = GetLength(0), Height = GetLength(1), Depth = GetLength(2)
                if let Some(d) = ["Width", "Height", "Depth"].iter().position(|x| *x == name) {
                    let rank = hb.ty.rank();
                    if rank >= 2 && d < rank {
                        let k = Expr::new(ExprKind::Int(d as i64), Ty::Int, sp);
                        return Expr::new(ExprKind::Builtin { f: Builtin::ArrayGetLength, args: vec![hb, k], site: 0 }, Ty::Int, sp);
                    }
                }
                if name == "Rank" {
                    let r = Expr::new(ExprKind::Int(hb.ty.rank() as i64), Ty::Int, sp);
                    if crate::opt::is_pure_expr(&hb) {
                        return r;
                    }
                    // still evaluate the array (a call): its length is read and dropped
                    let len = Expr::new(ExprKind::ListLen(Box::new(hb)), Ty::Int, sp);
                    let keep = Expr::new(ExprKind::Binary(BinOp::Ge, Box::new(len), Box::new(Expr::new(ExprKind::Int(0), Ty::Int, sp))), Ty::Bool, sp);
                    return Expr::new(ExprKind::Cond(Box::new(keep), Box::new(r.clone()), Box::new(r)), Ty::Int, sp);
                }
                if name == "GetLength" || name == "GetUpperBound" || name == "Clone" {
                    self.d.error_note("E0108", nsp, format!("'{}' is a method", name), format!("call it: {}({})", name, if name == "Clone" { "" } else { "0" }));
                    return Expr::err(sp);
                }
                self.d.error("E0108", nsp, format!("an array has no property '{}' (available: Length, Rank; methods GetLength(d), GetUpperBound(d), Clone())", name));
                Expr::err(sp)
            }
            t => {
                let tn = self.p.ty_name(&t);
                self.d.error("E0108", nsp, format!("type '{}' has no members", tn));
                Expr::err(sp)
            }
        }
    }

    fn check_args(&mut self, f: FuncId, args: &[ast::Arg], sp: Span) -> Vec<HArg> {
        self.check_args_pre(f, args, vec![], sp)
    }

    /// A call of one of the overloads `cands` of a method (one candidate: no choice to make). Like
    /// C#: the overloads that take this many arguments with these `ref`/`out` keywords and whose
    /// parameters every argument converts to; of those, the one whose every conversion is at least
    /// as good as the others' (an exact type beats a widening, a more derived class beats its
    /// base). None, or no single best one, is an error (E0267) that lists the overloads.
    fn call_overload(&mut self, cands: &[FuncId], args: &[ast::Arg], sp: Span, nsp: Span) -> Option<(FuncId, Vec<HArg>)> {
        let r = self.call_overload_inner(cands, args, sp, nsp)?;
        let f = r.0;
        if self.p.funcs[f].is_abstract && matches!(self.p.funcs[f].kind, FuncKind::Static(_)) {
            let l = self.p.func_label(f);
            self.d.error_note("E0262", nsp, format!("'{}' is a static abstract member: each implementing type provides it", l), format!("call it on a type: 'T.{}(...)' in a generic method with 'where T : {}', or 'S.{}(...)' on an implementing type", self.p.funcs[f].name, self.p.funcs[f].owner_name(&self.p), self.p.funcs[f].name));
            return None;
        }
        Some(r)
    }

    fn call_overload_inner(&mut self, cands: &[FuncId], args: &[ast::Arg], sp: Span, nsp: Span) -> Option<(FuncId, Vec<HArg>)> {
        if cands.len() == 1 {
            return Some((cands[0], self.check_args(cands[0], args, sp)));
        }
        let name = self.p.funcs[cands[0]].name.clone();
        let fit: Vec<FuncId> = cands.iter().copied().filter(|&f| self.p.funcs[f].params.len() == args.len()).collect();
        if fit.len() == 1 {
            return Some((fit[0], self.check_args(fit[0], args, sp)));
        }
        let list = |this: &Self| -> String { cands.iter().map(|&f| format!("'{}'", this.sig_text(f))).collect::<Vec<_>>().join(", ") };
        if fit.is_empty() {
            let l = list(self);
            self.d.error_note("E0267", nsp, format!("no overload of '{}' takes {} argument(s)", name, args.len()), format!("the overloads are {}", l));
            for a in args {
                if a.mode != ParamMode::Out {
                    let _ = self.expr(&a.expr, None);
                }
            }
            return None;
        }
        // the arguments' types, checked once without an expected type (a lambda, `default` and
        // `out var x` fit any parameter)
        let mut pre: Vec<Option<Expr>> = vec![];
        let mut tys: Vec<Option<Ty>> = vec![];
        for a in args {
            match &a.expr.kind {
                ast::ExprKind::OutDecl(t, _, _) => {
                    let before = self.d.list.len();
                    tys.push(t.as_ref().map(|t| self.resolve_type(t)));
                    self.d.list.truncate(before);
                    pre.push(None);
                }
                ast::ExprKind::Lambda { .. } | ast::ExprKind::Default(None) => {
                    tys.push(None);
                    pre.push(None);
                }
                _ => {
                    let he = self.expr(&a.expr, None);
                    tys.push(Some(he.ty.clone()));
                    pre.push(Some(he));
                }
            }
        }
        if tys.iter().any(|t| *t == Some(Ty::Error)) {
            // already reported: check against the first overload that takes this many arguments
            let f = fit[0];
            return Some((f, self.check_args_pre(f, args, pre, sp)));
        }
        let mut ranked: Vec<(FuncId, Vec<u32>)> = vec![];
        for &f in &fit {
            let func = &self.p.funcs[f];
            let mut r = vec![];
            let mut ok = true;
            for (i, a) in args.iter().enumerate() {
                let l = func.params[i];
                let (pt, pm) = (func.locals[l].ty.clone(), func.param_mode_of(l));
                let by_ref = matches!(pm, ParamMode::Ref | ParamMode::Out);
                if by_ref != matches!(a.mode, ParamMode::Ref | ParamMode::Out) || (by_ref && pm != a.mode) {
                    ok = false;
                    break;
                }
                match &tys[i] {
                    None => {
                        // a lambda fits a delegate parameter that takes as many arguments
                        if let ast::ExprKind::Lambda { params, .. } = &a.expr.kind {
                            if !self.lambda_fits(&pt, params.len()) {
                                ok = false;
                                break;
                            }
                        }
                        r.push(0);
                    }
                    Some(t) if by_ref => {
                        if *t != pt {
                            ok = false;
                            break;
                        }
                        r.push(0);
                    }
                    Some(t) => match self.conv_rank(t, &pt) {
                        Some(k) => r.push(k),
                        None => {
                            ok = false;
                            break;
                        }
                    },
                }
            }
            if ok {
                ranked.push((f, r));
            }
        }
        let shown: Vec<String> = tys.iter().map(|t| t.as_ref().map(|t| self.p.ty_name(t)).unwrap_or_else(|| "_".into())).collect();
        if ranked.is_empty() {
            let l = list(self);
            self.d.error_note("E0267", nsp, format!("no overload of '{}' takes arguments of type ({})", name, shown.join(", ")), format!("the overloads are {}", l));
            return None;
        }
        let better = |a: &[u32], b: &[u32]| a.iter().zip(b).all(|(x, y)| x <= y) && a != b;
        let best: Vec<FuncId> = ranked.iter().filter(|(f, r)| ranked.iter().all(|(g, q)| g == f || better(r, q))).map(|(f, _)| *f).collect();
        let [f] = best[..] else {
            let tied: Vec<String> = ranked.iter().filter(|(_, r)| !ranked.iter().any(|(_, q)| better(q, r))).map(|(g, _)| format!("'{}'", self.sig_text(*g))).collect();
            self.d.error_note(
                "E0267",
                nsp,
                format!("the call of '{}' with arguments of type ({}) is ambiguous", name, shown.join(", ")),
                format!("{} fit equally well: convert an argument to the parameter type you mean, e.g. '(float)x'", tied.join(" and ")),
            );
            return None;
        };
        Some((f, self.check_args_pre(f, args, pre, sp)))
    }

    /// How good the implicit conversion of an argument of type `from` to a parameter of type `to`
    /// is (0: the same type; smaller is better), or None if there is none
    fn conv_rank(&self, from: &Ty, to: &Ty) -> Option<u32> {
        if from == to {
            return Some(0);
        }
        if !self.can_coerce(from, to) {
            return None;
        }
        // how many steps up the class chain (an interface: farther than any base class)
        let dist = |d: ClassId, c: ClassId| -> u32 {
            let mut k = 0;
            let mut cur = Some(d);
            while let Some(x) = cur {
                if x == c {
                    return k;
                }
                k += 1;
                cur = self.p.classes[x].base;
            }
            64
        };
        let depth = |c: ClassId| -> u32 {
            let mut k = 0;
            let mut cur = self.p.classes[c].base;
            while let Some(x) = cur {
                k += 1;
                cur = self.p.classes[x].base;
            }
            k
        };
        Some(match (from, to) {
            (Ty::Char, Ty::Int) => 1,
            (Ty::Int, Ty::Float) | (Ty::Char, Ty::Float) => 2,
            (Ty::Handle(d), Ty::Handle(c)) | (Ty::NHandle(d), Ty::NHandle(c)) => 1 + dist(*d, *c),
            (Ty::Handle(d), Ty::NHandle(c)) => 2 + dist(*d, *c),
            // null: the most derived class first (C#'s better conversion target)
            (Ty::Null, Ty::NHandle(c)) => 200 - depth(*c).min(100),
            _ => 100,
        })
    }

    /// does a lambda with `n` parameters fit a parameter of type `t`? (a delegate taking `n`)
    fn lambda_fits(&self, t: &Ty, n: usize) -> bool {
        match t {
            Ty::Delegate(d) => self.p.delegates[*d].params.len() == n,
            Ty::Error => true,
            _ => false,
        }
    }

    /// `pre[i]`: argument i, already checked (without an expected type) to infer type arguments
    fn check_args_pre(&mut self, f: FuncId, args: &[ast::Arg], mut pre: Vec<Option<Expr>>, sp: Span) -> Vec<HArg> {
        let func = &self.p.funcs[f];
        let n = func.params.len();
        let ptys: Vec<(Ty, ParamMode)> = func.params.iter().map(|&l| (func.locals[l].ty.clone(), func.param_mode_of(l))).collect();
        let fname = func.name.clone();
        if args.len() != n {
            self.d.error("E0241", sp, format!("'{}' expects {} argument(s), found {}", fname, n, args.len()));
        }
        let mut out = vec![];
        for (i, a) in args.iter().enumerate() {
            let (pt, pm) = ptys.get(i).cloned().unwrap_or((Ty::Error, ParamMode::Value));
            let he = match pre.get_mut(i).and_then(|x| x.take()) {
                Some(he) => he,
                None if a.mode == ParamMode::Out => self.out_arg(&a.expr, &pt),
                None => self.expr(&a.expr, Some(&pt)),
            };
            let am = a.mode;
            match pm {
                ParamMode::Ref | ParamMode::Out => {
                    if am != pm {
                        let kw = if pm == ParamMode::Ref { "ref" } else { "out" };
                        self.d.error("E0401", a.expr.span, format!("argument {} must be passed with '{}'", i + 1, kw));
                    }
                    if he.ty != pt && he.ty != Ty::Error && pt != Ty::Error {
                        let (x, y) = (self.p.ty_name(&he.ty), self.p.ty_name(&pt));
                        self.d.error("E0200", a.expr.span, format!("'{}' argument must have exactly type '{}', found '{}'", if pm == ParamMode::Ref { "ref" } else { "out" }, y, x));
                    }
                    // `out int v` declares v right there: the call gives it its value
                    if !matches!(a.expr.kind, ast::ExprKind::OutDecl(..)) {
                        self.check_assignable(&he);
                    }
                    if let Some((_, false)) = crate::effects::target_field(&he) {
                        self.d.error_note(
                            "E0407",
                            a.expr.span,
                            "a field of another entity cannot be passed by ref/out",
                            "writes to other entities are deferred to the end of the system; read it into a local, or call a method on that entity",
                        );
                    }
                    out.push(HArg { mode: pm, expr: he });
                }
                _ => {
                    if am == ParamMode::Ref || am == ParamMode::Out {
                        self.d.error("E0401", a.expr.span, format!("argument {} is not a ref/out parameter", i + 1));
                    }
                    let he = self.coerce(he, &pt);
                    out.push(HArg { mode: pm, expr: he });
                }
            }
        }
        out
    }

    /// The method an unqualified call `n(...)` names in the current struct or class (or a base
    /// class): its number of parameters and of type parameters (0: not generic).
    fn own_method_arity(&self, n: &str) -> Option<Vec<(usize, usize)>> {
        let owner = self.st.as_ref()?.owner;
        let (plain, tref) = match owner {
            Owner::Struct(s) => (self.p.struct_methods_named(s, n), Some(TypeRef::Struct(s))),
            Owner::Entity(c) | Owner::Static(c) => (self.p.methods_named(c, n), self.find_template_class(c, n).map(TypeRef::Class)),
        };
        if !plain.is_empty() {
            return Some(plain.iter().map(|&f| (self.p.funcs[f].params.len(), 0)).collect());
        }
        let ti = *self.templates.get(&(tref?, n.to_string()))?;
        Some(vec![(self.tmpls[ti].md.params.len(), self.tmpls[ti].md.type_params.len())])
    }

    /// `Spawn`, `Destroy`, `Print`, `Count` and `Query` are built-in functions, but a struct or class
    /// may declare a method with one of these names. Like C# member lookup, an unqualified call then
    /// goes to that method; the built-in still takes the calls the method cannot: a different number
    /// of (type) arguments in the built-in's own form (`Count<T>()` next to `int Count(ETeam t)`,
    /// `Destroy()` next to `void Destroy(float delay)`), and `Spawn(new T { ... })` of an entity
    /// class, which no method can receive (entities are only created by Spawn).
    fn builtin_hidden(&self, n: &str, type_args: &[ast::TypeExpr], args: &[ast::Arg]) -> bool {
        if !matches!(n, "Spawn" | "Destroy" | "Print" | "Count" | "Query") {
            return false;
        }
        let Some(arities) = self.own_method_arity(n) else { return false };
        if n == "Spawn" {
            if let [a] = args {
                if let ast::ExprKind::New { ty, .. } = &a.expr.kind {
                    if matches!(self.types.get(ty.name.as_str()), Some(TypeRef::Class(c)) if self.p.classes[*c].is_entity) {
                        return false;
                    }
                }
            }
        }
        let fits_method = arities.iter().any(|&(np, ntp)| args.len() == np && (type_args.is_empty() || type_args.len() == ntp));
        let builtin_form = match n {
            "Destroy" => type_args.is_empty() && args.len() <= 1,
            "Spawn" | "Print" => type_args.is_empty() && args.len() == 1,
            _ => !type_args.is_empty() && args.is_empty(),
        };
        fits_method || !builtin_form
    }

    fn call_expr(&mut self, callee: &ast::Expr, type_args: &[ast::TypeExpr], args: &[ast::Arg], sp: Span) -> Expr {
        match &callee.kind {
            ast::ExprKind::Name(n) if self.lookup_local(n).is_none() => {
                if COMP_OPS.contains(&n.as_str()) && (self.comp_op_form(n, type_args, args) || !self.has_own_method(n)) {
                    return self.comp_op(None, n, type_args, args, sp, callee.span);
                }
                let hidden = self.builtin_hidden(n, type_args, args);
                match n.as_str() {
                    _ if hidden => {}
                    "Spawn" => return self.spawn_expr(args, sp),
                    "Destroy" => {
                        // in a component, `this` is the component: say which of the two is meant
                        if let Some(k) = self.self_class().filter(|&k| self.p.classes[k].is_component) {
                            if args.is_empty() || matches!(args[0].expr.kind, ast::ExprKind::This) {
                                let kn = self.p.classes[k].name.clone();
                                self.d.error_note("E0265", sp, format!("in the component '{}', Destroy({}) is ambiguous", kn, if args.is_empty() { "" } else { "this" }), format!("to take the component off the entity write 'Remove<{}>();', to destroy the entity 'Destroy(Owner);'", kn));
                                return Expr::err(sp);
                            }
                        }
                        if args.is_empty() {
                            if self.self_class().is_none() || self.st.as_ref().unwrap().is_static {
                                self.d.error("E0242", sp, "Destroy() without an argument is only valid in an entity method");
                            }
                            return Expr::new(ExprKind::Destroy(None), Ty::Void, sp);
                        }
                        if args.len() != 1 {
                            self.d.error("E0241", sp, "Destroy takes zero or one argument");
                            return Expr::err(sp);
                        }
                        let h = self.expr(&args[0].expr, None);
                        match h.ty {
                            Ty::Handle(_) | Ty::Error => {}
                            Ty::NHandle(_) => {
                                self.d.error_note("E0301", h.span, "cannot destroy a nullable handle", "check it first: 'if (x is T t) Destroy(t);'");
                            }
                            _ => self.d.error("E0200", h.span, "Destroy expects an entity handle"),
                        }
                        return Expr::new(ExprKind::Destroy(Some(Box::new(h))), Ty::Void, sp);
                    }
                    "Print" => {
                        if args.len() != 1 {
                            self.d.error("E0241", sp, "Print takes exactly one argument");
                            return Expr::err(sp);
                        }
                        let a = self.expr(&args[0].expr, None);
                        match &a.ty {
                            Ty::Str | Ty::Int | Ty::Float | Ty::Bool | Ty::Enum(_) | Ty::Error => {}
                            t => {
                                let tn = self.p.ty_name(t);
                                self.d.error("E0239", a.span, format!("cannot print a value of type '{}'", tn));
                            }
                        }
                        let site = self.next_site();
                        return Expr::new(ExprKind::Builtin { f: Builtin::Print, args: vec![a], site }, Ty::Void, sp);
                    }
                    // `Count()` without a type argument calls the type's own method `Count` if it has
                    // one (`int Count();` of an interface)
                    "Count" if !type_args.is_empty() || !self.has_own_method("Count") => {
                        if type_args.len() != 1 || !args.is_empty() {
                            self.d.error("E0241", sp, "usage: Count<T>()");
                            return Expr::err(sp);
                        }
                        return match self.resolve_type(&type_args[0]) {
                            Ty::Handle(c) => Expr::new(ExprKind::Count(c), Ty::Int, sp),
                            Ty::Error => Expr::err(sp),
                            _ => {
                                self.d.error("E0231", sp, "Count<T>() requires an entity class");
                                Expr::err(sp)
                            }
                        };
                    }
                    "Query" => {
                        self.d.error("E0231", sp, "Query<T>() can only be used directly in 'foreach (var x in Query<T>())'");
                        return Expr::err(sp);
                    }
                    _ => {}
                }
                // own methods
                let (owner, is_static) = {
                    let st = self.st.as_ref().unwrap();
                    (st.owner, st.is_static)
                };
                match owner {
                    Owner::Struct(s) => {
                        let cands = self.p.struct_methods_named(s, n);
                        if !cands.is_empty() {
                            let Some((f, hargs)) = self.call_overload(&cands, args, sp, callee.span) else { return Expr::err(sp) };
                            self.reference(callee.span, Sym::Func(f));
                            let fstatic = matches!(self.p.funcs[f].kind, FuncKind::StructStatic(_));
                            if !fstatic && is_static {
                                self.d.error("E0106", sp, format!("cannot call instance method '{}' from a static context", n));
                                return Expr::err(sp);
                            }
                            let rt = self.p.funcs[f].ret.clone();
                            let recv = if fstatic { None } else { Some(Box::new(Expr::new(ExprKind::StructSelf, Ty::Struct(s), sp))) };
                            if let Some(r) = &recv {
                                self.check_mut_receiver(r, f, sp);
                            }
                            return Expr::new(ExprKind::Call { func: f, recv, args: hargs, dispatch: Dispatch::Static }, rt, sp);
                        }
                    }
                    Owner::Entity(c) | Owner::Static(c) => {
                        let cands = self.p.methods_named(c, n);
                        if !cands.is_empty() {
                            let Some((f, hargs)) = self.call_overload(&cands, args, sp, callee.span) else { return Expr::err(sp) };
                            self.reference(callee.span, Sym::Func(f));
                            let fstatic = matches!(self.p.funcs[f].kind, FuncKind::Static(_));
                            if !fstatic && (is_static || !self.p.classes[c].is_entity) {
                                self.d.error("E0106", sp, format!("cannot call instance method '{}' from a static context", n));
                                return Expr::err(sp);
                            }
                            let rt = self.p.funcs[f].ret.clone();
                            if fstatic {
                                return Expr::new(ExprKind::Call { func: f, recv: None, args: hargs, dispatch: Dispatch::Static }, rt, sp);
                            }
                            let recv = Expr::new(ExprKind::SelfRef, Ty::Handle(c), sp);
                            return Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(recv)), args: hargs, dispatch: Dispatch::SelfCall }, rt, sp);
                        }
                    }
                }
                let tref = match owner {
                    Owner::Struct(s) => Some(TypeRef::Struct(s)),
                    Owner::Entity(c) | Owner::Static(c) => self.find_template_class(c, n).map(TypeRef::Class),
                };
                if let Some(&ti) = tref.and_then(|t| self.templates.get(&(t, n.clone()))) {
                    let Some((f, hargs)) = self.generic_call(ti, type_args, args, sp, callee.span) else { return Expr::err(sp) };
                    let rt = self.p.funcs[f].ret.clone();
                    return match self.p.funcs[f].kind {
                        FuncKind::Static(_) | FuncKind::StructStatic(_) => Expr::new(ExprKind::Call { func: f, recv: None, args: hargs, dispatch: Dispatch::Static }, rt, sp),
                        _ if is_static || matches!(owner, Owner::Static(_)) => {
                            self.d.error("E0106", sp, format!("cannot call instance method '{}' from a static context", n));
                            Expr::err(sp)
                        }
                        FuncKind::StructMethod(s) => {
                            let recv = Expr::new(ExprKind::StructSelf, Ty::Struct(s), sp);
                            self.check_mut_receiver(&recv, f, sp);
                            Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(recv)), args: hargs, dispatch: Dispatch::Static }, rt, sp)
                        }
                        _ => {
                            let c = self.self_class().unwrap();
                            let recv = Expr::new(ExprKind::SelfRef, Ty::Handle(c), sp);
                            Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(recv)), args: hargs, dispatch: Dispatch::SelfCall }, rt, sp)
                        }
                    };
                }
                // `F(...)` with a field F of delegate type: a call through the delegate
                if let Some(target) = self.own_delegate_field(n, callee.span) {
                    return self.invoke_expr(target, args, sp);
                }
                match self.default_body_note(n) {
                    Some(note) => self.d.error_note("E0101", callee.span, format!("unknown function '{}'", n), note),
                    None => self.d.error("E0101", callee.span, format!("unknown function '{}'", n)),
                }
                for a in args {
                    let _ = self.expr(&a.expr, None);
                }
                Expr::err(sp)
            }
            ast::ExprKind::Member(base, name, nsp) => {
                if let ast::ExprKind::Base = base.kind {
                    let Some(c) = self.self_class() else {
                        self.d.error("E0106", sp, "'base' is only valid in entity methods");
                        return Expr::err(sp);
                    };
                    let Some(b) = self.p.classes[c].base else {
                        self.d.error("E0106", sp, "class has no base class");
                        return Expr::err(sp);
                    };
                    let cands = self.p.methods_named(b, name);
                    if cands.is_empty() {
                        self.d.error("E0101", *nsp, format!("base class has no method '{}'", name));
                        return Expr::err(sp);
                    }
                    let Some((f, hargs)) = self.call_overload(&cands, args, sp, *nsp) else { return Expr::err(sp) };
                    self.reference(*nsp, Sym::Func(f));
                    let rt = self.p.funcs[f].ret.clone();
                    let recv = Expr::new(ExprKind::SelfRef, Ty::Handle(c), sp);
                    return Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(recv)), args: hargs, dispatch: Dispatch::BaseCall }, rt, sp);
                }
                let tb = match &base.kind {
                    ast::ExprKind::GenericName(t) => match self.resolve_static_ok(t) {
                        Ty::Struct(s) => Some(Ok(TypeRef::Struct(s))),
                        Ty::Handle(c) if self.class_inst.contains_key(&c) => Some(Ok(TypeRef::Class(c))),
                        Ty::Error => return Expr::err(sp),
                        _ => {
                            self.d.error("E0101", *nsp, format!("'{}' has no static method '{}'", t.name, name));
                            return Expr::err(sp);
                        }
                    },
                    _ => self.type_base(base),
                };
                match tb {
                    Some(Err("?")) => {
                        for a in args {
                            if a.mode != ParamMode::Out {
                                let _ = self.expr(&a.expr, None);
                            }
                        }
                        return Expr::err(sp);
                    }
                    Some(Err(ns)) => return self.builtin_call(ns, name, args, sp, *nsp),
                    Some(Ok(TypeRef::Struct(s))) => {
                        self.reference(base.span, Sym::Struct(s));
                        if let Some(&ti) = self.templates.get(&(TypeRef::Struct(s), name.clone())) {
                            if matches!(self.tmpls[ti].kind, FuncKind::StructStatic(_)) {
                                let Some((f, hargs)) = self.generic_call(ti, type_args, args, sp, *nsp) else { return Expr::err(sp) };
                                let rt = self.p.funcs[f].ret.clone();
                                return Expr::new(ExprKind::Call { func: f, recv: None, args: hargs, dispatch: Dispatch::Static }, rt, sp);
                            }
                        }
                        let cands: Vec<FuncId> = self.p.struct_methods_named(s, name).into_iter().filter(|&f| matches!(self.p.funcs[f].kind, FuncKind::StructStatic(_))).collect();
                        if !cands.is_empty() {
                            let Some((f, hargs)) = self.call_overload(&cands, args, sp, *nsp) else { return Expr::err(sp) };
                            self.reference(*nsp, Sym::Func(f));
                            let rt = self.p.funcs[f].ret.clone();
                            return Expr::new(ExprKind::Call { func: f, recv: None, args: hargs, dispatch: Dispatch::Static }, rt, sp);
                        }
                        self.d.error("E0101", *nsp, format!("'{}' has no static method '{}'", self.p.structs[s].name, name));
                        return Expr::err(sp);
                    }
                    Some(Ok(TypeRef::Class(c))) => {
                        self.reference(base.span, Sym::Class(c));
                        if let Some(&ti) = self.find_template_class(c, name).and_then(|x| self.templates.get(&(TypeRef::Class(x), name.clone()))) {
                            if matches!(self.tmpls[ti].kind, FuncKind::Static(_)) {
                                let Some((f, hargs)) = self.generic_call(ti, type_args, args, sp, *nsp) else { return Expr::err(sp) };
                                let rt = self.p.funcs[f].ret.clone();
                                return Expr::new(ExprKind::Call { func: f, recv: None, args: hargs, dispatch: Dispatch::Static }, rt, sp);
                            }
                        }
                        let cands: Vec<FuncId> = self.p.methods_named(c, name).into_iter().filter(|&f| matches!(self.p.funcs[f].kind, FuncKind::Static(_))).collect();
                        if !cands.is_empty() {
                            let Some((f, hargs)) = self.call_overload(&cands, args, sp, *nsp) else { return Expr::err(sp) };
                            self.reference(*nsp, Sym::Func(f));
                            let rt = self.p.funcs[f].ret.clone();
                            return Expr::new(ExprKind::Call { func: f, recv: None, args: hargs, dispatch: Dispatch::Static }, rt, sp);
                        }
                        self.d.error("E0101", *nsp, format!("'{}' has no static method '{}'", self.p.classes[c].name, name));
                        return Expr::err(sp);
                    }
                    Some(Ok(TypeRef::Enum(_))) => {
                        self.d.error("E0101", *nsp, "enums have no methods");
                        return Expr::err(sp);
                    }
                    None => {}
                }
                let hb = self.expr(base, None);
                let tmpl = match &hb.ty {
                    Ty::Struct(s) => self.templates.get(&(TypeRef::Struct(*s), name.clone())).copied(),
                    Ty::Handle(c) => self.find_template_class(*c, name).and_then(|x| self.templates.get(&(TypeRef::Class(x), name.clone()))).copied(),
                    _ => None,
                };
                if let Some(ti) = tmpl.filter(|&ti| matches!(self.tmpls[ti].kind, FuncKind::StructMethod(_) | FuncKind::EntityMethod(_))) {
                    let Some((f, hargs)) = self.generic_call(ti, type_args, args, sp, *nsp) else { return Expr::err(sp) };
                    let rt = self.p.funcs[f].ret.clone();
                    let disp = match hb.kind {
                        _ if matches!(hb.ty, Ty::Struct(_)) => Dispatch::Static,
                        ExprKind::SelfRef => Dispatch::SelfCall,
                        _ => Dispatch::RemoteRead,
                    };
                    return Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(hb)), args: hargs, dispatch: disp }, rt, sp);
                }
                if let Ty::Delegate(_) = hb.ty {
                    if name == "Invoke" {
                        return self.invoke_expr(hb, args, sp);
                    }
                    self.d.error_note("E0101", *nsp, format!("a delegate has no method '{}'", name), "call it: f(x) or f.Invoke(x)");
                    return Expr::err(sp);
                }
                // `x.F(...)` with a field F of delegate type
                let dfield = match &hb.ty {
                    Ty::Handle(c) if self.p.methods_named(*c, name).is_empty() => self.p.find_field(*c, name).filter(|&f| matches!(self.p.fields[f].ty, Ty::Delegate(_))).map(|f| (f, self.p.fields[f].ty.clone())),
                    _ => None,
                };
                if let Some((f, ty)) = dfield {
                    self.reference(*nsp, Sym::Field(f));
                    let target = Expr::new(ExprKind::EField { base: Box::new(hb), field: f }, ty, sp);
                    return self.invoke_expr(target, args, sp);
                }
                let sfield = match &hb.ty {
                    Ty::Struct(s) if self.p.struct_methods_named(*s, name).is_empty() => self.p.structs[*s].fields.iter().position(|f| f.name == *name && matches!(f.ty, Ty::Delegate(_))).map(|i| (*s, i)),
                    _ => None,
                };
                if let Some((s, i)) = sfield {
                    self.reference(*nsp, Sym::SField(s, i));
                    let ty = self.p.structs[s].fields[i].ty.clone();
                    let target = Expr::new(ExprKind::Field { base: Box::new(hb), sid: s, idx: i }, ty, sp);
                    return self.invoke_expr(target, args, sp);
                }
                match hb.ty.clone() {
                    Ty::Error => Expr::err(sp),
                    Ty::Struct(s) => match self.p.struct_methods_named(s, name).into_iter().filter(|&f| matches!(self.p.funcs[f].kind, FuncKind::StructMethod(_))).collect::<Vec<_>>() {
                        cands if !cands.is_empty() => {
                            let Some((f, hargs)) = self.call_overload(&cands, args, sp, *nsp) else { return Expr::err(sp) };
                            self.reference(*nsp, Sym::Func(f));
                            let rt = self.p.funcs[f].ret.clone();
                            self.check_mut_receiver(&hb, f, sp);
                            Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(hb)), args: hargs, dispatch: Dispatch::Static }, rt, sp)
                        }
                        _ => {
                            let sn = self.p.structs[s].name.clone();
                            self.d.error("E0101", *nsp, format!("'{}' has no instance method '{}'", sn, name));
                            Expr::err(sp)
                        }
                    },
                    Ty::Handle(c) if COMP_OPS.contains(&name.as_str()) && (self.comp_op_form(name, type_args, args) || self.p.resolve_method(c, name).is_none()) => {
                        self.comp_op(Some(hb), name, type_args, args, sp, *nsp)
                    }
                    Ty::Handle(c) => match self.p.methods_named(c, name).into_iter().filter(|&f| matches!(self.p.funcs[f].kind, FuncKind::EntityMethod(_))).collect::<Vec<_>>() {
                        cands if !cands.is_empty() => {
                            let Some((f, hargs)) = self.call_overload(&cands, args, sp, *nsp) else { return Expr::err(sp) };
                            self.reference(*nsp, Sym::Func(f));
                            let rt = self.p.funcs[f].ret.clone();
                            let disp = if matches!(hb.kind, ExprKind::SelfRef) { Dispatch::SelfCall } else { Dispatch::RemoteRead };
                            Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(hb)), args: hargs, dispatch: disp }, rt, sp)
                        }
                        _ => {
                            let cn = self.p.classes[c].name.clone();
                            match self.default_body_note(name).filter(|_| matches!(hb.kind, ExprKind::SelfRef)) {
                                Some(note) => self.d.error_note("E0101", *nsp, format!("'{}' has no method '{}'", cn, name), note),
                                None => self.d.error("E0101", *nsp, format!("'{}' has no method '{}'", cn, name)),
                            }
                            Expr::err(sp)
                        }
                    },
                    Ty::NHandle(c) => {
                        let cn = self.p.classes[c].name.clone();
                        let note = self.get_note(base, &cn).unwrap_or_else(|| format!("check it first: 'if (x is {} y) y.{}(...);'", cn, name));
                        self.d.error_note("E0301", base.span, format!("cannot call '{}' through a nullable handle '{}?'", name, cn), note);
                        Expr::err(sp)
                    }
                    Ty::Array(_) | Ty::MdArray(..) if matches!(name.as_str(), "GetLength" | "GetUpperBound" | "Clone") => self.array_method(hb, name, args, sp),
                    Ty::List(el) | Ty::Array(el) if is_lambda_method(name, args) => self.lambda_call(hb, (*el).clone(), name, args, sp, *nsp),
                    Ty::List(el) => self.list_call(hb, (*el).clone(), name, args, sp, *nsp),
                    Ty::Array(_) => {
                        self.d.error_note(
                            "E0101",
                            *nsp,
                            format!("an array has no method '{}'", name),
                            "use Array.Sort(xs), Array.Reverse(xs), Array.IndexOf(xs, v), Array.Fill(xs, v), Array.Copy(xs), xs.Clone(), xs.GetLength(0) or xs.Length",
                        );
                        Expr::err(sp)
                    }
                    Ty::MdArray(_, r) if GRID_METHODS.contains(&name.as_str()) && r >= 2 => self.grid_call(hb, name, args, sp, *nsp),
                    Ty::MdArray(..) => {
                        self.d.error_note(
                            "E0101",
                            *nsp,
                            format!("a multi-dimensional array has no method '{}'", name),
                            "available: GetLength(d), GetUpperBound(d), Clone(), and the properties Length and Rank; loop over the elements with foreach or nested for loops (as in C#, a T[,] has no LINQ methods)",
                        );
                        Expr::err(sp)
                    }
                    Ty::Str => self.str_call(hb, name, args, sp, *nsp),
                    Ty::Dict(..) => self.dict_call(hb, name, args, sp, *nsp),
                    Ty::Char if name == "CompareTo" || name == "Equals" => {
                        if args.len() != 1 {
                            self.d.error("E0241", sp, format!("{}(char other) takes one argument", name));
                            return Expr::err(sp);
                        }
                        let o = self.expr(&args[0].expr, Some(&Ty::Char));
                        let o = self.coerce(o, &Ty::Char);
                        if name == "Equals" {
                            return Expr::new(ExprKind::Binary(BinOp::Eq, Box::new(hb), Box::new(o)), Ty::Bool, sp);
                        }
                        // the difference of the codes, like .NET's Char.CompareTo
                        let a = self.coerce(hb, &Ty::Int);
                        let b = self.coerce(o, &Ty::Int);
                        Expr::new(ExprKind::Binary(BinOp::Sub, Box::new(a), Box::new(b)), Ty::Int, sp)
                    }
                    Ty::Int | Ty::Float | Ty::Bool | Ty::Enum(_) if name == "CompareTo" || name == "Equals" => {
                        if args.len() != 1 {
                            self.d.error("E0241", sp, format!("{}(other) takes one argument", name));
                            return Expr::err(sp);
                        }
                        let t = hb.ty.clone();
                        let o = self.expr(&args[0].expr, Some(&t));
                        let o = self.coerce(o, &t);
                        if name == "Equals" {
                            // .NET: float.NaN.Equals(float.NaN) is true
                            if t == Ty::Float {
                                return Expr::new(ExprKind::Builtin { f: Builtin::CompareVal, args: vec![hb, o], site: 1 }, Ty::Bool, sp);
                            }
                            return Expr::new(ExprKind::Binary(BinOp::Eq, Box::new(hb), Box::new(o)), Ty::Bool, sp);
                        }
                        Expr::new(ExprKind::Builtin { f: Builtin::CompareVal, args: vec![hb, o], site: 0 }, Ty::Int, sp)
                    }
                    Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) if name == "ToString" => {
                        if !args.is_empty() {
                            self.d.error("E0241", sp, "ToString() takes no arguments (use $\"{x:F2}\" for formatting)");
                        }
                        Expr::new(ExprKind::Builtin { f: Builtin::ToStr, args: vec![hb], site: 0 }, Ty::Str, sp)
                    }
                    t => {
                        let tn = self.p.ty_name(&t);
                        self.d.error("E0101", *nsp, format!("type '{}' has no methods", tn));
                        Expr::err(sp)
                    }
                }
            }
            _ => {
                // a delegate value: a local, an element, the result of a call
                let target = self.expr(callee, None);
                match target.ty {
                    Ty::Delegate(_) => self.invoke_expr(target, args, sp),
                    Ty::Error => Expr::err(sp),
                    _ => {
                        self.d.error("E0243", callee.span, "expression is not callable");
                        Expr::err(sp)
                    }
                }
            }
        }
    }

    fn list_call(&mut self, list: Expr, el: Ty, name: &str, args: &[ast::Arg], sp: Span, nsp: Span) -> Expr {
        // (operation, argument types, result)
        let (op, argtys, ret) = match name {
            "Add" => (Some(ListOp::Add), vec![el.clone()], Ty::Void),
            "Clear" => (Some(ListOp::Clear), vec![], Ty::Void),
            "RemoveAt" => (Some(ListOp::RemoveAt), vec![Ty::Int], Ty::Void),
            "Contains" => (Some(ListOp::Contains), vec![el.clone()], Ty::Bool),
            "IndexOf" => (Some(ListOp::IndexOf), vec![el.clone()], Ty::Int),
            "Insert" => (Some(ListOp::Insert), vec![Ty::Int, el.clone()], Ty::Void),
            "Remove" => (Some(ListOp::Remove), vec![el.clone()], Ty::Bool),
            "Reverse" => (Some(ListOp::Reverse), vec![], Ty::Void),
            "Sort" => (Some(ListOp::Sort), vec![], Ty::Void),
            "Clone" => (None, vec![], list.ty.clone()),
            _ => {
                self.d.error("E0101", nsp, format!("List<T> has no method '{}' (available: Add, Insert, Remove, RemoveAt, Clear, Contains, IndexOf, Reverse, Sort, Clone, Count)", name));
                return Expr::err(sp);
            }
        };
        if args.len() != argtys.len() {
            self.d.error("E0241", sp, format!("List.{} expects {} argument(s)", name, argtys.len()));
            return Expr::err(sp);
        }
        if matches!(op, Some(ListOp::Contains | ListOp::IndexOf | ListOp::Remove)) && !matches!(el, Ty::Int | Ty::Float | Ty::Bool | Ty::Enum(_) | Ty::Handle(_) | Ty::NHandle(_) | Ty::Str) {
            self.d.error("E0239", sp, format!("{} requires an element type with built-in equality (int, float, bool, enum, string or an entity reference)", name));
        }
        if el.is_list() && matches!(op, Some(ListOp::Contains | ListOp::IndexOf | ListOp::Remove)) {
            self.d.error_note("E0239", sp, format!("'{}' compares elements, and lists are not compared by value", name), "compare an element's contents in a loop, or RemoveAt(i) by position");
            return Expr::err(sp);
        }
        if op == Some(ListOp::Sort) && !matches!(el, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::Str | Ty::Error) {
            let tn = self.p.ty_name(&el);
            self.d.error_note("E0239", sp, format!("Sort() needs an element type with a built-in order, not '{}'", tn), "lists of int, float, bool, char, enum or string can be sorted; sort other values by a key you keep in a parallel list");
        }
        let hargs: Vec<Expr> = args
            .iter()
            .zip(&argtys)
            .map(|(a, t)| {
                let e = self.expr(&a.expr, Some(t));
                self.coerce(e, t)
            })
            .collect();
        match op {
            Some(op) => {
                if op.mutates() {
                    self.check_assignable(&list);
                }
                Expr::new(ExprKind::ListCall { list: Box::new(list), op, args: hargs }, ret, sp)
            }
            None => {
                let site = self.new_alloc_site(el, sp);
                Expr::new(ExprKind::ListClone { list: Box::new(list), site }, ret, sp)
            }
        }
    }

    /// string conversion used by concatenation (C#: "a" + 5 == "a5")
    fn to_str(&mut self, e: Expr) -> Expr {
        match &e.ty {
            Ty::Str | Ty::Error => e,
            Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) => {
                let sp = e.span;
                Expr::new(ExprKind::Builtin { f: Builtin::ToStr, args: vec![e], site: 0 }, Ty::Str, sp)
            }
            t => {
                let tn = self.p.ty_name(t);
                self.d.error("E0239", e.span, format!("cannot convert '{}' to string", tn));
                Expr::err(e.span)
            }
        }
    }

    /// `d[k] = v` and `d[k] op= v`
    fn dict_assign(&mut self, dict: Expr, key: Expr, op: Option<BinOp>, value: &ast::Expr, vty: Ty, sp: Span) -> Expr {
        self.check_assignable(&dict);
        let hv = match op {
            None => {
                let hv = self.expr(value, Some(&vty));
                self.coerce(hv, &vty)
            }
            Some(bop) => {
                if !crate::opt::is_pure_expr(&key) || !crate::opt::is_pure_expr(&dict) {
                    self.d.error_note("E0253", sp, "compound assignment to a dictionary element needs a side-effect-free key", "store the key in a local first");
                }
                let cur = Expr::new(ExprKind::DictGet { dict: Box::new(dict.clone()), key: Box::new(key.clone()) }, vty.clone(), sp);
                if vty == Ty::Str && bop == BinOp::Add {
                    let hv = self.expr(value, None);
                    let hv = self.to_str(hv);
                    Expr::new(ExprKind::Binary(BinOp::Add, Box::new(cur), Box::new(hv)), Ty::Str, sp)
                } else {
                    let hv = self.expr(value, Some(&vty));
                    let res = self.arith_type(bop, &vty, &hv.ty, sp);
                    if res == Ty::Error {
                        return Expr::err(sp);
                    }
                    if !self.can_coerce(&res, &vty) {
                        let (a, b) = (self.p.ty_name(&res), self.p.ty_name(&vty));
                        self.d.error("E0211", sp, format!("result of '{}' is '{}' which cannot be assigned to '{}'", bop.sym(), a, b));
                        return Expr::err(sp);
                    }
                    let cur = self.coerce(cur, &res);
                    let hv = self.coerce(hv, &res);
                    let bin = Expr::new(ExprKind::Binary(bop, Box::new(cur), Box::new(hv)), res, sp);
                    self.coerce(bin, &vty)
                }
            }
        };
        Expr::new(ExprKind::DictSet { dict: Box::new(dict), key: Box::new(key), value: Box::new(hv) }, vty, sp)
    }

    fn dict_call(&mut self, d: Expr, name: &str, args: &[ast::Arg], sp: Span, nsp: Span) -> Expr {
        let Ty::Dict(k, v) = d.ty.clone() else { return Expr::err(sp) };
        let (k, v) = (*k, *v);
        let arg = |me: &mut Self, i: usize, t: &Ty| {
            let e = me.expr(&args[i].expr, Some(t));
            me.coerce(e, t)
        };
        if (name, args.len()) == ("Clone", 0) {
            // a deep copy, like List.Clone (the copy is a new collection the caller owns)
            let site = self.new_alloc_site(v.clone(), sp);
            let ty = d.ty.clone();
            return Expr::new(ExprKind::ListClone { list: Box::new(d), site }, ty, sp);
        }
        let (op, hargs, ret) = match (name, args.len()) {
            ("ContainsKey", 1) => (DictOp::ContainsKey, vec![arg(self, 0, &k)], Ty::Bool),
            ("Remove", 1) => (DictOp::Remove, vec![arg(self, 0, &k)], Ty::Bool),
            ("Clear", 0) => (DictOp::Clear, vec![], Ty::Void),
            ("GetValueOrDefault", 1) => {
                let key = arg(self, 0, &k);
                let def = match consteval::zero_value(&self.p, &v) {
                    Some(z) => crate::opt::to_expr(&self.p, &z, &v, sp),
                    None => Expr::new(ExprKind::Null, v.clone(), sp),
                };
                (DictOp::GetOr, vec![key, def], v.clone())
            }
            ("GetValueOrDefault", 2) => {
                let key = arg(self, 0, &k);
                let def = arg(self, 1, &v);
                (DictOp::GetOr, vec![key, def], v.clone())
            }
            ("Add", 2) => {
                let key = arg(self, 0, &k);
                let val = arg(self, 1, &v);
                (DictOp::Add, vec![key, val], Ty::Void)
            }
            ("TryGetValue", 2) => {
                let key = arg(self, 0, &k);
                if args[1].mode != ParamMode::Out {
                    self.d.error("E0241", sp, "TryGetValue(key, out value): the second argument needs 'out'");
                    return Expr::err(sp);
                }
                let out = self.out_arg(&args[1].expr, &v);
                let ok = matches!(out.kind, ExprKind::Local(l) if matches!(self.st.as_ref().unwrap().locals[l].kind, LocalKind::Normal | LocalKind::Param(ParamMode::Out)));
                if ok && !matches!(args[1].expr.kind, ast::ExprKind::OutDecl(..)) {
                    self.check_assignable(&out);
                }
                if !ok {
                    self.d.error_note("E0253", args[1].expr.span, "TryGetValue writes into a local variable", format!("pass 'out {} value' (or 'out var value') to declare one", self.p.ty_name(&v)));
                    return Expr::err(sp);
                }
                if out.ty != v {
                    let (a, b) = (self.p.ty_name(&out.ty), self.p.ty_name(&v));
                    self.d.error("E0200", args[1].expr.span, format!("type mismatch: expected '{}', found '{}'", b, a));
                    return Expr::err(sp);
                }
                (DictOp::TryGet, vec![key, out], Ty::Bool)
            }
            _ => {
                self.d.error(
                    "E0101",
                    nsp,
                    format!("Dictionary has no method '{}' with {} argument(s) (available: Add, ContainsKey, Remove, Clear, TryGetValue, GetValueOrDefault, Clone, Count, indexer)", name, args.len()),
                );
                return Expr::err(sp);
            }
        };
        if op.mutates() {
            self.check_assignable(&d);
        }
        Expr::new(ExprKind::DictCall { dict: Box::new(d), op, args: hargs }, ret, sp)
    }

    fn str_call(&mut self, s: Expr, name: &str, args: &[ast::Arg], sp: Span, nsp: Span) -> Expr {
        // a char argument where a string is expected is the one-character string (IndexOf('x'),
        // Contains(c), Replace('a', 'b'), Split(',') ... have char overloads in C#)
        let arg = |me: &mut Self, i: usize, t: &Ty| {
            let e = me.expr(&args[i].expr, Some(t));
            if *t == Ty::Str && e.ty == Ty::Char {
                return me.to_str(e);
            }
            me.coerce(e, t)
        };
        match (name, args.len()) {
            ("Split", 1) => {
                let sep = arg(self, 0, &Ty::Str);
                let parts = Expr::new(ExprKind::Builtin { f: Builtin::StrSplit, args: vec![s, sep], site: 0 }, Ty::List(Box::new(Ty::Str)), sp);
                let site = self.new_alloc_site(Ty::Str, sp);
                return Expr::new(ExprKind::ListClone { list: Box::new(parts), site }, Ty::List(Box::new(Ty::Str)), sp);
            }
            ("PadLeft" | "PadRight", 1 | 2) => {
                let f = if name == "PadLeft" { Builtin::StrPadLeft } else { Builtin::StrPadRight };
                let w = arg(self, 0, &Ty::Int);
                let c = if args.len() == 2 { arg(self, 1, &Ty::Char) } else { Expr::new(ExprKind::Int(' ' as i64), Ty::Char, sp) };
                return Expr::new(ExprKind::Builtin { f, args: vec![s, w, c], site: 0 }, Ty::Str, sp);
            }
            ("ToCharArray", 0) => {
                self.d.error_note("E0241", sp, "ToCharArray is not available", "index the string directly (s[i], s.Length) or iterate it: foreach (char c in s)");
                return Expr::err(sp);
            }
            _ => {}
        }
        let (f, n, ret) = match (name, args.len()) {
            ("Substring", 1) => (Builtin::StrSubFrom, 1, Ty::Str),
            ("Substring", 2) => (Builtin::StrSub, 2, Ty::Str),
            ("Contains", 1) => (Builtin::StrContains, 1, Ty::Bool),
            ("StartsWith", 1) => (Builtin::StrStarts, 1, Ty::Bool),
            ("EndsWith", 1) => (Builtin::StrEnds, 1, Ty::Bool),
            ("IndexOf", 1) => (Builtin::StrIndexOf, 1, Ty::Int),
            ("LastIndexOf", 1) => (Builtin::StrLastIndexOf, 1, Ty::Int),
            ("ToUpper", 0) | ("ToUpperInvariant", 0) => (Builtin::StrUpper, 0, Ty::Str),
            ("ToLower", 0) | ("ToLowerInvariant", 0) => (Builtin::StrLower, 0, Ty::Str),
            ("Trim", 0) => (Builtin::StrTrim, 0, Ty::Str),
            ("Replace", 2) => (Builtin::StrReplace, 2, Ty::Str),
            ("CompareTo", 1) => (Builtin::StrCompare, 1, Ty::Int),
            ("ToString", 0) => return s,
            ("Equals", 1) => {
                let o = arg(self, 0, &Ty::Str);
                return Expr::new(ExprKind::Binary(BinOp::Eq, Box::new(s), Box::new(o)), Ty::Bool, sp);
            }
            _ => {
                self.d.error(
                    "E0101",
                    nsp,
                    format!("string has no method '{}' with {} argument(s) (available: Substring, Contains, StartsWith, EndsWith, IndexOf, LastIndexOf, Replace, Split, Trim, PadLeft, PadRight, ToUpper, ToLower, CompareTo, Equals, Length, s[i])", name, args.len()),
                );
                return Expr::err(sp);
            }
        };
        let mut hargs = vec![s];
        for i in 0..n {
            let t = if matches!(f, Builtin::StrSub | Builtin::StrSubFrom) { Ty::Int } else { Ty::Str };
            hargs.push(arg(self, i, &t));
        }
        Expr::new(ExprKind::Builtin { f, args: hargs, site: 0 }, ret, sp)
    }

    /// a.Clone(), a.GetLength(d), a.GetUpperBound(d) on an array of any rank
    fn array_method(&mut self, a: Expr, name: &str, args: &[ast::Arg], sp: Span) -> Expr {
        let rank = a.ty.rank();
        if name == "Clone" {
            if !args.is_empty() {
                self.d.error("E0241", sp, "Clone() takes no arguments");
                return Expr::err(sp);
            }
            // a copy that lives where the result goes, like (T[,])a.Clone() in C#
            let el = self.p.elem_of(&a.ty).unwrap_or(Ty::Error);
            let site = self.new_alloc_site(el, sp);
            let t = a.ty.clone();
            return Expr::new(ExprKind::ListClone { list: Box::new(a), site }, t, sp);
        }
        if args.len() != 1 {
            self.d.error("E0241", sp, format!("{}(int dimension) takes one argument", name));
            return Expr::err(sp);
        }
        let d = self.expr(&args[0].expr, Some(&Ty::Int));
        let mut d = self.coerce(d, &Ty::Int);
        if let Ok(ConstVal::Int(k)) = crate::consteval::eval_expr(&self.p, &d) {
            if k < 0 || k as usize >= rank {
                let tn = self.p.ty_name(&a.ty);
                self.d.error_note("E0241", d.span, format!("'{}' has {} dimension(s): there is no dimension {}", tn, rank, k), format!("the dimensions are numbered 0 to {}", rank - 1));
                return Expr::err(sp);
            }
            d = Expr::new(ExprKind::Int(k as i64), Ty::Int, d.span);
        }
        let len = if rank == 1 && matches!(d.kind, ExprKind::Int(0)) {
            Expr::new(ExprKind::ListLen(Box::new(a)), Ty::Int, sp)
        } else {
            Expr::new(ExprKind::Builtin { f: Builtin::ArrayGetLength, args: vec![a, d], site: 0 }, Ty::Int, sp)
        };
        if name == "GetUpperBound" {
            return Expr::new(ExprKind::Binary(BinOp::Sub, Box::new(len), Box::new(Expr::new(ExprKind::Int(1), Ty::Int, sp))), Ty::Int, sp);
        }
        len
    }

    /// The methods of a grid (a 2D or 3D array, docs/01 §1.14.2): InBounds, Get, Fill, Flood, and the
    /// cell-parallel Step, Move and Ray whose lambda the code generator inlines (src/grid.rs).
    fn grid_call(&mut self, g: Expr, name: &str, args: &[ast::Arg], sp: Span, nsp: Span) -> Expr {
        let rank = g.ty.rank();
        let el = self.p.elem_of(&g.ty).unwrap_or(Ty::Error);
        let scalar = matches!(el, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_));
        let coords = ["x", "y", "z"];
        let cs = coords[..rank].join(", ");
        let ints = |me: &mut Self, xs: &[ast::Arg]| -> Vec<Expr> {
            xs.iter()
                .map(|a| {
                    let x = me.expr(&a.expr, Some(&Ty::Int));
                    me.coerce(x, &Ty::Int)
                })
                .collect()
        };
        let usage = |what: &str| -> String {
            match what {
                "InBounds" => format!("InBounds({})", cs),
                "Get" => format!("Get({}, fallback)", cs),
                "Fill" => "Fill(value)".into(),
                "Flood" => format!("Flood({}, value)", cs),
                "Step" => format!("Step(({}, c) => new value of the cell)", cs),
                "Move" => format!("Move(empty, ({}, c) => Grid.Dir(..) or Grid.Stay)", cs),
                _ => format!("Ray({}, {}, c => hit)", coords[..rank].iter().map(|c| format!("{}0", c)).collect::<Vec<_>>().join(", "), coords[..rank].iter().map(|c| format!("{}1", c)).collect::<Vec<_>>().join(", ")),
            }
        };
        let wrong = |me: &mut Self| {
            me.d.error_note("E0241", sp, format!("'{}' on a {}D grid takes other arguments", name, rank), format!("write {}", usage(name)));
            Expr::err(sp)
        };
        match name {
            "InBounds" => {
                if args.len() != rank {
                    return wrong(self);
                }
                let mut a = vec![g];
                a.extend(ints(self, args));
                Expr::new(ExprKind::Builtin { f: Builtin::GridInBounds, args: a, site: 0 }, Ty::Bool, sp)
            }
            "Get" => {
                if args.len() != rank + 1 {
                    return wrong(self);
                }
                let mut a = vec![g];
                a.extend(ints(self, &args[..rank]));
                let fb = self.expr(&args[rank].expr, Some(&el));
                a.push(self.coerce(fb, &el));
                Expr::new(ExprKind::Builtin { f: Builtin::GridGet, args: a, site: 0 }, el, sp)
            }
            "Fill" => {
                if args.len() != 1 {
                    return wrong(self);
                }
                self.check_assignable(&g);
                let v = self.expr(&args[0].expr, Some(&el));
                let v = self.coerce(v, &el);
                Expr::new(ExprKind::ListCall { list: Box::new(g), op: ListOp::Fill, args: vec![v] }, Ty::Void, sp)
            }
            "Flood" => {
                if args.len() != rank + 1 {
                    return wrong(self);
                }
                if !scalar {
                    self.d.error_note("E0239", sp, "Flood needs cells of a number, bool, char or enum type", "it compares cells with the start cell");
                    return Expr::err(sp);
                }
                self.check_assignable(&g);
                let mut a = ints(self, &args[..rank]);
                let v = self.expr(&args[rank].expr, Some(&el));
                a.push(self.coerce(v, &el));
                Expr::new(ExprKind::ListCall { list: Box::new(g), op: ListOp::GridFlood, args: a }, Ty::Int, sp)
            }
            "Step" | "Move" | "Ray" => {
                let (op, nlead, ncoord) = match name {
                    "Step" => (LamOp::GridStep, 0, rank),
                    "Move" => (LamOp::GridMove, 1, rank),
                    _ => (LamOp::GridRay, 2 * rank, 0),
                };
                if args.len() != nlead + 1 {
                    return wrong(self);
                }
                if op != LamOp::GridStep && !scalar {
                    self.d.error_note("E0239", sp, format!("{} needs cells of a number, bool, char or enum type", name), "it compares cells (with the empty value, or the hit test reads them)");
                    return Expr::err(sp);
                }
                if op != LamOp::GridRay {
                    self.check_assignable(&g);
                }
                let lead: Vec<Expr> = match op {
                    LamOp::GridMove => {
                        let e = self.expr(&args[0].expr, Some(&el));
                        vec![self.coerce(e, &el)]
                    }
                    LamOp::GridRay => ints(self, &args[..nlead]),
                    _ => vec![],
                };
                let la = &args[nlead];
                let ast::ExprKind::Lambda { params: ps, body } = &la.expr.kind else {
                    self.d.error_note("E0241", la.expr.span, format!("'{}' expects a lambda", name), format!("write {}", usage(name)));
                    return Expr::err(sp);
                };
                if ps.len() != ncoord + 1 {
                    self.d.error_note("E0241", la.expr.span, format!("the lambda of '{}' takes {} parameter(s)", name, ncoord + 1), format!("write {}", usage(name)));
                    return Expr::err(sp);
                }
                self.push_scope();
                let mut params = vec![];
                for (i, (ty, n, psp)) in ps.iter().enumerate() {
                    let want = if i < ncoord { Ty::Int } else { el.clone() };
                    if let Some(t) = ty {
                        let tt = self.resolve_type(t);
                        if tt != want && tt != Ty::Error {
                            let (a, b) = (self.p.ty_name(&tt), self.p.ty_name(&want));
                            self.d.error("E0200", *psp, format!("lambda parameter '{}' is declared '{}' but is '{}'", n, a, b));
                        }
                    }
                    params.push(self.declare_local(n, want, LocalKind::ListElem, *psp));
                }
                let b = match &**body {
                    ast::LambdaBody::Expr(x) => self.expr(x, None),
                    ast::LambdaBody::Block(bl) => match bl.stmts.as_slice() {
                        [ast::Stmt::Return { value: Some(x), .. }] => self.expr(x, None),
                        _ => {
                            self.d.error_note("E0241", bl.span, "this lambda must compute a value: write '(..) => expression' or '{ return expression; }'", "use ?:, switch expressions and helper functions for the cases");
                            Expr::err(sp)
                        }
                    },
                };
                self.pop_scope();
                if b.ty == Ty::Error {
                    return Expr::err(sp);
                }
                let (b, ret) = match op {
                    LamOp::GridStep => (self.coerce(b, &el), Ty::Void),
                    LamOp::GridMove => (self.coerce(b, &Ty::Int), Ty::Void),
                    _ => (self.coerce(b, &Ty::Bool), Ty::Int),
                };
                Expr::new(ExprKind::ListLambda { list: Box::new(g), op, params, body: Box::new(b), site: usize::MAX, args: lead }, ret, sp)
            }
            _ => {
                self.d.error("E0101", nsp, format!("a grid has no method '{}'", name));
                Expr::err(sp)
            }
        }
    }

    /// Array.Sort / Reverse / IndexOf / Fill / Copy (a list operation on the array)
    fn array_call(&mut self, name: &str, args: &[ast::Arg], sp: Span, nsp: Span) -> Expr {
        let Some(first) = args.first() else {
            self.d.error("E0241", sp, format!("Array.{} needs the array as its first argument", name));
            return Expr::err(sp);
        };
        let xs = self.expr(&first.expr, None);
        let el = match &xs.ty {
            Ty::Array(e) => (**e).clone(),
            Ty::Error => return Expr::err(sp),
            Ty::MdArray(..) => {
                let tn = self.p.ty_name(&xs.ty);
                let note = if name == "Copy" { "copy it with a.Clone()" } else { "loop over the elements with nested for loops (C#'s Array functions need a one-dimensional array too)" };
                self.d.error_note("E0241", first.expr.span, format!("Array.{} expects a one-dimensional array, found '{}'", name, tn), note);
                return Expr::err(sp);
            }
            t => {
                let tn = self.p.ty_name(t);
                self.d.error("E0241", first.expr.span, format!("Array.{} expects an array, found '{}'", name, tn));
                return Expr::err(sp);
            }
        };
        if args.len() == 2 && matches!(args[1].expr.kind, ast::ExprKind::Lambda { .. }) {
            let lname = match name {
                "TrueForAll" => "TrueForAll",
                n => n,
            };
            if matches!(lname, "Sort" | "Find" | "FindLast" | "FindIndex" | "FindLastIndex" | "Exists" | "TrueForAll") {
                return self.lambda_call(xs, el, lname, &args[1..], sp, nsp);
            }
            if lname == "ForEach" {
                self.d.error_note("E0241", sp, "Array.ForEach must be a statement of its own", "write: Array.ForEach(xs, x => ...);");
                return Expr::err(sp);
            }
        }
        let (op, argtys, ret) = match (name, args.len()) {
            ("Sort", 1) => (ListOp::Sort, vec![], Ty::Void),
            ("Reverse", 1) => (ListOp::Reverse, vec![], Ty::Void),
            ("IndexOf", 2) => (ListOp::IndexOf, vec![el.clone()], Ty::Int),
            ("Fill", 2) => (ListOp::Fill, vec![el.clone()], Ty::Void),
            ("Copy", 1) => {
                // a copy that lives where the result goes (like (T[])xs.Clone())
                let site = self.new_alloc_site(el, sp);
                let t = xs.ty.clone();
                return Expr::new(ExprKind::ListClone { list: Box::new(xs), site }, t, sp);
            }
            _ => {
                self.d.error("E0101", nsp, format!("unknown function 'Array.{}' with {} argument(s) (available: Sort(xs), Reverse(xs), IndexOf(xs, v), Fill(xs, v), Copy(xs))", name, args.len()));
                return Expr::err(sp);
            }
        };
        if op == ListOp::Sort && !matches!(el, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::Str | Ty::Error) {
            let tn = self.p.ty_name(&el);
            self.d.error("E0239", sp, format!("Array.Sort needs an element type with a built-in order, not '{}'", tn));
        }
        if el.is_list() && matches!(op, ListOp::IndexOf | ListOp::Fill) {
            self.d.error_note("E0239", sp, format!("Array.{} on an array of collections would compare or share collections", name), "loop over the elements instead");
            return Expr::err(sp);
        }
        if op.mutates() {
            self.check_assignable(&xs);
        }
        let mut hargs = vec![];
        for (a, t) in args[1..].iter().zip(argtys.iter()) {
            let h = self.expr(&a.expr, Some(t));
            hargs.push(self.coerce(h, t));
        }
        Expr::new(ExprKind::ListCall { list: Box::new(xs), op, args: hargs }, ret, sp)
    }

    /// A list/array method that takes a lambda (or one of the LINQ-style forms without it):
    /// type-checks the lambda with the element type and produces an inline ListLambda.
    fn lambda_call(&mut self, list: Expr, el: Ty, name: &str, args: &[ast::Arg], sp: Span, nsp: Span) -> Expr {
        let is_list = matches!(list.ty, Ty::List(_));
        let lt = list.ty.clone();
        // forms without a lambda
        if args.is_empty() {
            match name {
                "Any" => {
                    let n = Expr::new(ExprKind::ListLen(Box::new(list)), Ty::Int, sp);
                    return Expr::new(ExprKind::Binary(BinOp::Gt, Box::new(n), Box::new(Expr::new(ExprKind::Int(0), Ty::Int, sp))), Ty::Bool, sp);
                }
                "Count" => return Expr::new(ExprKind::ListLen(Box::new(list)), Ty::Int, sp),
                "ToList" | "ToArray" => {
                    let rt = if name == "ToList" { Ty::List(Box::new(el.clone())) } else { Ty::Array(Box::new(el.clone())) };
                    let site = self.new_alloc_site(el, sp);
                    return Expr::new(ExprKind::ListClone { list: Box::new(list), site }, rt, sp);
                }
                _ => {}
            }
        }
        let op = match name {
            "Sort" => LamOp::Sort,
            "Find" => LamOp::Find,
            "FindLast" => LamOp::FindLast,
            "FindIndex" => LamOp::FindIndex,
            "FindLastIndex" => LamOp::FindLastIndex,
            "Exists" | "Any" => LamOp::Exists,
            "TrueForAll" | "All" => LamOp::All,
            "RemoveAll" => LamOp::RemoveAll,
            "Count" => LamOp::Count,
            "Sum" => LamOp::Sum,
            "Max" => LamOp::Max,
            "Min" => LamOp::Min,
            "First" => LamOp::First,
            "Last" => LamOp::Last,
            "FirstOrDefault" => LamOp::FirstOrDefault,
            "LastOrDefault" => LamOp::LastOrDefault,
            "Where" => LamOp::Where,
            "Select" => LamOp::Select,
            "OrderBy" => LamOp::OrderBy,
            "OrderByDescending" => LamOp::OrderByDesc,
            "ForEach" => {
                self.d.error_note("E0241", sp, "ForEach must be a statement of its own", "write: xs.ForEach(x => ...);");
                return Expr::err(sp);
            }
            _ => {
                self.d.error("E0101", nsp, format!("no method '{}' taking a lambda", name));
                return Expr::err(sp);
            }
        };
        if op == LamOp::RemoveAll && !is_list {
            self.d.error("E0101", nsp, "an array has no RemoveAll (its length is fixed)");
            return Expr::err(sp);
        }
        if el.is_list() && matches!(op, LamOp::Sort | LamOp::RemoveAll | LamOp::Where | LamOp::OrderBy | LamOp::OrderByDesc | LamOp::Find | LamOp::FindLast | LamOp::First | LamOp::Last | LamOp::FirstOrDefault | LamOp::LastOrDefault) {
            self.d.error_note("E0239", sp, format!("'{}' on a collection of collections would copy or drop element collections", name), "loop over the elements instead");
            return Expr::err(sp);
        }
        let nparams = if op == LamOp::Sort { 2 } else { 1 };
        if args.len() > 1 {
            self.d.error("E0241", sp, format!("'{}' takes one lambda argument", name));
            return Expr::err(sp);
        }
        self.push_scope();
        let mut params = vec![];
        let body = match args.first().map(|a| &a.expr.kind) {
            Some(ast::ExprKind::Lambda { params: ps, body }) => {
                if ps.len() != nparams {
                    self.d.error("E0241", args[0].expr.span, format!("'{}' takes a lambda with {} parameter(s)", name, nparams));
                    self.pop_scope();
                    return Expr::err(sp);
                }
                for (ty, n, psp) in ps {
                    if let Some(t) = ty {
                        let tt = self.resolve_type(t);
                        if tt != el && tt != Ty::Error {
                            let (a, b) = (self.p.ty_name(&tt), self.p.ty_name(&el));
                            self.d.error("E0200", *psp, format!("lambda parameter '{}' is declared '{}' but the elements are '{}'", n, a, b));
                        }
                    }
                    params.push(self.declare_local(n, el.clone(), LocalKind::ListElem, *psp));
                }
                match &**body {
                    ast::LambdaBody::Expr(x) => self.expr(x, None),
                    ast::LambdaBody::Block(b) => match b.stmts.as_slice() {
                        [ast::Stmt::Return { value: Some(x), .. }] => self.expr(x, None),
                        _ => {
                            self.d.error_note("E0241", b.span, "this lambda must compute a value: write 'x => expression' or '{ return expression; }'", "a lambda with statements is only possible in ForEach");
                            Expr::err(sp)
                        }
                    },
                }
            }
            Some(_) => {
                self.d.error("E0241", args[0].expr.span, format!("'{}' expects a lambda", name));
                self.pop_scope();
                return Expr::err(sp);
            }
            None => {
                // First() / Sum() / Max()...: the element itself, a predicate that always holds
                let nm = format!("__it{}", self.st().locals.len());
                let l = self.declare_local(&nm, el.clone(), LocalKind::ListElem, sp);
                params.push(l);
                match op {
                    LamOp::Sum | LamOp::Max | LamOp::Min => Expr::new(ExprKind::Local(l), el.clone(), sp),
                    LamOp::First | LamOp::Last | LamOp::FirstOrDefault | LamOp::LastOrDefault => Expr::new(ExprKind::Bool(true), Ty::Bool, sp),
                    _ => {
                        self.d.error("E0241", sp, format!("'{}' needs a lambda", name));
                        self.pop_scope();
                        return Expr::err(sp);
                    }
                }
            }
        };
        self.pop_scope();
        if body.ty == Ty::Error {
            return Expr::err(sp);
        }
        let handle_or = |t: &Ty| match t {
            Ty::Handle(c) => Ty::NHandle(*c),
            other => other.clone(),
        };
        let (body, ret) = match op {
            LamOp::GridStep | LamOp::GridMove | LamOp::GridRay => unreachable!("grid operations go through grid_call"),
            LamOp::Sort => (self.coerce(body, &Ty::Int), Ty::Void),
            LamOp::Exists | LamOp::All => (self.coerce(body, &Ty::Bool), Ty::Bool),
            LamOp::RemoveAll | LamOp::Count | LamOp::FindIndex | LamOp::FindLastIndex => (self.coerce(body, &Ty::Bool), Ty::Int),
            LamOp::Find | LamOp::FindLast | LamOp::FirstOrDefault | LamOp::LastOrDefault => (self.coerce(body, &Ty::Bool), handle_or(&el)),
            LamOp::First | LamOp::Last => (self.coerce(body, &Ty::Bool), el.clone()),
            LamOp::Where => (self.coerce(body, &Ty::Bool), lt.clone()),
            LamOp::Sum | LamOp::Max | LamOp::Min => {
                let b = if body.ty == Ty::Char { self.coerce(body, &Ty::Int) } else { body };
                if !matches!(b.ty, Ty::Int | Ty::Float) {
                    let tn = self.p.ty_name(&b.ty);
                    self.d.error("E0239", b.span, format!("'{}' needs int or float values, found '{}'", name, tn));
                    return Expr::err(sp);
                }
                let t = b.ty.clone();
                (b, t)
            }
            LamOp::Select => {
                if body.ty == Ty::Void || body.ty.is_list() || body.ty == Ty::Null {
                    let tn = self.p.ty_name(&body.ty);
                    self.d.error("E0239", body.span, format!("Select cannot produce '{}' values", tn));
                    return Expr::err(sp);
                }
                let t = Ty::List(Box::new(body.ty.clone()));
                (body, t)
            }
            LamOp::OrderBy | LamOp::OrderByDesc => {
                if !matches!(body.ty, Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_) | Ty::Str) {
                    let tn = self.p.ty_name(&body.ty);
                    self.d.error("E0239", body.span, format!("'{}' needs a key with a built-in order (int, float, bool, char, enum, string), found '{}'", name, tn));
                    return Expr::err(sp);
                }
                (body, Ty::List(Box::new(el.clone())))
            }
        };
        if op.mutates() {
            self.check_assignable(&list);
        }
        let site = if op.makes_list() {
            let elem = match &ret {
                Ty::List(e) => (**e).clone(),
                _ => el.clone(),
            };
            self.new_alloc_site(elem, sp)
        } else {
            usize::MAX
        };
        Expr::new(ExprKind::ListLambda { list: Box::new(list), op, params, body: Box::new(body), site, args: vec![] }, ret, sp)
    }

    fn next_site(&mut self) -> usize {
        self.p.builtin_sites += 1;
        self.p.builtin_sites
    }

    fn new_alloc_site(&mut self, elem: Ty, span: Span) -> usize {
        let func = self.st.as_ref().and_then(|s| s.fid).unwrap_or(usize::MAX);
        self.p.alloc_sites.push(AllocSite { func, elem, span });
        self.p.alloc_sites.len() - 1
    }

    fn builtin_call(&mut self, ns: &str, name: &str, args: &[ast::Arg], sp: Span, nsp: Span) -> Expr {
        if ns == "Array" {
            return self.array_call(name, args, sp, nsp);
        }
        if ns == "Grid" {
            if name != "Dir" || !(2..=3).contains(&args.len()) {
                self.d.error_note("E0101", nsp, format!("'Grid.{}' does not exist", name), "Grid.Dir(dx, dy) / Grid.Dir(dx, dy, dz) (each -1, 0 or 1) is a move direction for Move, Grid.Stay none");
                return Expr::err(sp);
            }
            let mut hs: Vec<Expr> = vec![];
            for a in args {
                let x = self.expr(&a.expr, Some(&Ty::Int));
                hs.push(self.coerce(x, &Ty::Int));
            }
            if hs.len() == 2 {
                hs.push(Expr::new(ExprKind::Int(0), Ty::Int, sp));
            }
            // a constant direction is a number at compile time
            let ks: Vec<Option<i64>> = hs.iter().map(|h| if let ExprKind::Int(k) = h.kind { Some(k) } else { None }).collect();
            if ks.iter().all(|k| k.is_some()) {
                let v: Vec<i64> = ks.into_iter().map(|k| k.unwrap()).collect();
                if v.iter().any(|k| !(-1..=1).contains(k)) {
                    self.d.error_note("E0241", sp, "a direction moves at most one cell along each axis", "each of dx, dy, dz is -1, 0 or 1");
                    return Expr::err(sp);
                }
                let code = if v.iter().all(|k| *k == 0) { 0 } else { 1 + (v[0] + 1) + 3 * (v[1] + 1) + 9 * (v[2] + 1) };
                return Expr::new(ExprKind::Int(code), Ty::Int, sp);
            }
            return Expr::new(ExprKind::Builtin { f: Builtin::GridDir, args: hs, site: 0 }, Ty::Int, sp);
        }
        if matches!((ns, name), ("int" | "float", "TryParse")) {
            let (f, vt) = if ns == "int" { (Builtin::TryParseInt, Ty::Int) } else { (Builtin::TryParseFloat, Ty::Float) };
            if args.len() != 2 || args[1].mode != ParamMode::Out {
                self.d.error("E0241", sp, format!("usage: {}.TryParse(string text, out {} value)", ns, ns));
                return Expr::err(sp);
            }
            let s = self.expr(&args[0].expr, Some(&Ty::Str));
            let s = self.coerce(s, &Ty::Str);
            let out = self.out_arg(&args[1].expr, &vt);
            let ok = matches!(out.kind, ExprKind::Local(l) if matches!(self.st.as_ref().unwrap().locals[l].kind, LocalKind::Normal | LocalKind::Param(ParamMode::Out | ParamMode::Ref)));
            if ok && !matches!(args[1].expr.kind, ast::ExprKind::OutDecl(..)) {
                self.check_assignable(&out);
            }
            if !ok || out.ty != vt {
                if out.ty != Ty::Error {
                    self.d.error_note("E0253", args[1].expr.span, format!("TryParse writes into a local '{}' variable", ns), format!("pass 'out {} value' (or 'out var value') to declare one", ns));
                }
                return Expr::err(sp);
            }
            return Expr::new(ExprKind::Builtin { f, args: vec![s, out], site: 0 }, Ty::Bool, sp);
        }
        let hargs: Vec<Expr> = args.iter().map(|a| self.expr(&a.expr, None)).collect();
        let any_float = hargs.iter().any(|a| a.ty == Ty::Float);
        let numeric_ty = if any_float { Ty::Float } else { Ty::Int };
        let (f, params, ret): (Builtin, Vec<Ty>, Ty) = match (ns, name) {
            ("Math", "Sqrt") => (Builtin::MathSqrt, vec![Ty::Float], Ty::Float),
            ("Math", "Sin") => (Builtin::MathSin, vec![Ty::Float], Ty::Float),
            ("Math", "Cos") => (Builtin::MathCos, vec![Ty::Float], Ty::Float),
            ("Math", "Floor") => (Builtin::MathFloor, vec![Ty::Float], Ty::Float),
            ("Math", "Atan2") => (Builtin::MathAtan2, vec![Ty::Float, Ty::Float], Ty::Float),
            ("Math", "Tan") => (Builtin::MathTan, vec![Ty::Float], Ty::Float),
            ("Math", "Atan") => (Builtin::MathAtan, vec![Ty::Float], Ty::Float),
            ("Math", "Asin") => (Builtin::MathAsin, vec![Ty::Float], Ty::Float),
            ("Math", "Acos") => (Builtin::MathAcos, vec![Ty::Float], Ty::Float),
            ("Math", "Exp") => (Builtin::MathExp, vec![Ty::Float], Ty::Float),
            ("Math", "Log") => (Builtin::MathLog, vec![Ty::Float], Ty::Float),
            ("Math", "Log10") => (Builtin::MathLog10, vec![Ty::Float], Ty::Float),
            ("Math", "Log2") => (Builtin::MathLog2, vec![Ty::Float], Ty::Float),
            ("Math", "Pow") => (Builtin::MathPow, vec![Ty::Float, Ty::Float], Ty::Float),
            ("Math", "Round") => (Builtin::MathRound, vec![Ty::Float], Ty::Float),
            ("Math", "Ceiling") => (Builtin::MathCeiling, vec![Ty::Float], Ty::Float),
            ("Math", "Truncate") => (Builtin::MathTruncate, vec![Ty::Float], Ty::Float),
            ("Math", "Sign") => (Builtin::MathSign, vec![numeric_ty.clone()], Ty::Int),
            ("Math", "Abs") => (Builtin::MathAbs, vec![numeric_ty.clone()], numeric_ty.clone()),
            ("Math", "Min") => (Builtin::MathMin, vec![numeric_ty.clone(); 2], numeric_ty.clone()),
            ("Math", "Max") => (Builtin::MathMax, vec![numeric_ty.clone(); 2], numeric_ty.clone()),
            ("Math", "Clamp") => (Builtin::MathClamp, vec![numeric_ty.clone(); 3], numeric_ty.clone()),
            ("Draw", "Sprite") => (Builtin::DrawSprite, vec![Ty::Int, Ty::Float, Ty::Float], Ty::Void),
            ("Draw", "Text") => (Builtin::DrawText, vec![Ty::Float, Ty::Float, Ty::Str], Ty::Void),
            ("Net", "View") => (Builtin::NetView, vec![Ty::Float, Ty::Float, Ty::Float], Ty::Void),
            ("Net", "ClearView") => (Builtin::NetClearView, vec![], Ty::Void),
            ("Random", "Float") => (Builtin::RandomFloat, vec![Ty::Float, Ty::Float], Ty::Float),
            ("Random", "Int") => (Builtin::RandomInt, vec![Ty::Int, Ty::Int], Ty::Int),
            ("string" | "String", "IsNullOrEmpty") => (Builtin::StrIsEmpty, vec![Ty::Str], Ty::Bool),
            ("string" | "String", "Compare") | ("string" | "String", "CompareOrdinal") => (Builtin::StrCompare, vec![Ty::Str, Ty::Str], Ty::Int),
            ("string" | "String", "Concat") => {
                // string.Concat(a, b, ...) == a + b + ...
                let mut it = hargs.into_iter();
                let Some(first) = it.next() else { return Expr::new(ExprKind::Str(String::new()), Ty::Str, sp) };
                let mut acc = self.to_str(first);
                for a in it {
                    let b = self.to_str(a);
                    acc = Expr::new(ExprKind::Binary(BinOp::Add, Box::new(acc), Box::new(b)), Ty::Str, sp);
                }
                return acc;
            }
            ("string" | "String", "Join") => {
                let ok = hargs.len() == 2
                    && hargs[0].ty == Ty::Str
                    && matches!(&hargs[1].ty, Ty::List(e) | Ty::Array(e) if matches!(**e, Ty::Str | Ty::Int | Ty::Float | Ty::Bool | Ty::Char | Ty::Enum(_)));
                if !ok {
                    if hargs.iter().all(|a| a.ty != Ty::Error) {
                        self.d.error("E0241", sp, "usage: string.Join(string separator, List<T> values) (T: string, int, float, bool or enum)");
                    }
                    return Expr::err(sp);
                }
                return Expr::new(ExprKind::Builtin { f: Builtin::StrJoin, args: hargs, site: 0 }, Ty::Str, sp);
            }
            ("char", "IsDigit") => (Builtin::CharIsDigit, vec![Ty::Char], Ty::Bool),
            ("char", "IsLetter") => (Builtin::CharIsLetter, vec![Ty::Char], Ty::Bool),
            ("char", "IsLetterOrDigit") => (Builtin::CharIsLetterOrDigit, vec![Ty::Char], Ty::Bool),
            ("char", "IsWhiteSpace") => (Builtin::CharIsWhiteSpace, vec![Ty::Char], Ty::Bool),
            ("char", "IsUpper") => (Builtin::CharIsUpper, vec![Ty::Char], Ty::Bool),
            ("char", "IsLower") => (Builtin::CharIsLower, vec![Ty::Char], Ty::Bool),
            ("char", "IsPunctuation") => (Builtin::CharIsPunctuation, vec![Ty::Char], Ty::Bool),
            ("char", "ToUpper") | ("char", "ToUpperInvariant") => (Builtin::CharToUpper, vec![Ty::Char], Ty::Char),
            ("char", "ToLower") | ("char", "ToLowerInvariant") => (Builtin::CharToLower, vec![Ty::Char], Ty::Char),
            ("int", "Parse") => (Builtin::ParseInt, vec![Ty::Str], Ty::Int),
            ("float", "Parse") => (Builtin::ParseFloat, vec![Ty::Str], Ty::Float),
            _ => {
                self.d.error("E0101", nsp, format!("unknown function '{}.{}'", ns, name));
                return Expr::err(sp);
            }
        };
        if hargs.len() != params.len() {
            self.d.error("E0241", sp, format!("'{}.{}' expects {} argument(s), found {}", ns, name, params.len(), hargs.len()));
            return Expr::err(sp);
        }
        let hargs: Vec<Expr> = hargs.into_iter().zip(params.iter()).map(|(a, t)| self.coerce(a, t)).collect();
        let site = self.next_site();
        Expr::new(ExprKind::Builtin { f, args: hargs, site }, ret, sp)
    }

    fn spawn_expr(&mut self, args: &[ast::Arg], sp: Span) -> Expr {
        if args.is_empty() {
            self.d.error("E0241", sp, "Spawn takes the entity and the components it starts with: Spawn(new T { ... }, new C { ... })");
            return Expr::err(sp);
        }
        let a = &args[0].expr;
        let ast::ExprKind::New { ty, args: cargs, inits } = &a.kind else {
            self.d.error("E0244", a.span, "Spawn expects 'new T { ... }' where T is an entity class");
            return Expr::err(sp);
        };
        let mut e = self.new_expr(ty, cargs.as_ref(), inits, sp, true);
        // `Spawn(new T { ... }, new C { ... })`: components the entity has from the start
        for x in &args[1..] {
            let ast::ExprKind::New { ty: kt, args: kargs, inits: kinits } = &x.expr.kind else {
                self.d.error_note("E0265", x.expr.span, "Spawn's further arguments are components: 'new C { ... }'", "Spawn(new Ship { Hp = 3 }, new Burning { Left = 5 })");
                continue;
            };
            let k = match self.resolve_type(kt) {
                Ty::Handle(k) if self.p.classes[k].is_component => k,
                Ty::Error => continue,
                other => {
                    let tn = self.p.ty_name(&other);
                    self.d.error_note("E0265", kt.span, format!("'{}' is not a component", tn), "Spawn's further arguments are components the entity starts with: 'new C { ... }'");
                    continue;
                }
            };
            if kargs.as_ref().map(|a| !a.is_empty()).unwrap_or(false) {
                self.d.error("E0217", x.expr.span, "components have no constructors; use an initializer list");
            }
            self.reference(Span::new(kt.span.lo as usize, kt.span.lo as usize + kt.name.len()), Sym::Class(k));
            let mut more = vec![];
            for (n, ie, nsp) in kinits {
                match self.p.find_field(k, n) {
                    Some(f) => {
                        self.reference(*nsp, Sym::Field(f));
                        let ft = self.p.fields[f].ty.clone();
                        let he = self.expr(ie, Some(&ft));
                        let he = self.coerce(he, &ft);
                        if more.iter().any(|(y, _)| *y == f) {
                            self.d.error("E0104", *nsp, format!("field '{}' initialized twice", n));
                        }
                        more.push((f, he));
                    }
                    None => {
                        let kn = self.p.classes[k].name.clone();
                        self.d.error("E0108", *nsp, format!("'{}' has no field '{}'", kn, n));
                    }
                }
            }
            if let ExprKind::Spawn { inits, comps, .. } = &mut e.kind {
                if comps.contains(&k) {
                    let kn = self.p.classes[k].name.clone();
                    self.d.error("E0104", x.expr.span, format!("component '{}' given twice", kn));
                    continue;
                }
                comps.push(k);
                comps.sort();
                inits.extend(more);
            }
        }
        e
    }

    fn new_expr(&mut self, ty: &ast::TypeExpr, args: Option<&Vec<ast::Arg>>, inits: &[(String, ast::Expr, Span)], sp: Span, in_spawn: bool) -> Expr {
        let t = self.resolve_type(ty);
        match t {
            Ty::Error => Expr::err(sp),
            Ty::Str => {
                // new string(c, n): c repeated n times
                let a = args.cloned().unwrap_or_default();
                if a.len() != 2 || !inits.is_empty() || in_spawn {
                    self.d.error("E0241", sp, "usage: new string(char c, int count)");
                    return Expr::err(sp);
                }
                let c = self.expr(&a[0].expr, Some(&Ty::Char));
                let c = self.coerce(c, &Ty::Char);
                let n = self.expr(&a[1].expr, Some(&Ty::Int));
                let n = self.coerce(n, &Ty::Int);
                Expr::new(ExprKind::Builtin { f: Builtin::StrRepeat, args: vec![c, n], site: 0 }, Ty::Str, sp)
            }
            Ty::List(el) => {
                if in_spawn {
                    self.d.error("E0244", sp, "Spawn expects an entity class");
                }
                if args.map(|a| !a.is_empty()).unwrap_or(false) || !inits.is_empty() {
                    self.d.error("E0245", sp, "use 'new List<T>()' (capacity is inferred by the compiler)");
                }
                let site = self.new_alloc_site((*el).clone(), sp);
                Expr::new(ExprKind::ListNew { site }, Ty::List(el), sp)
            }
            Ty::Dict(..) => {
                if in_spawn {
                    self.d.error("E0244", sp, "Spawn expects an entity class");
                }
                if args.map(|a| !a.is_empty()).unwrap_or(false) || !inits.is_empty() {
                    self.d.error("E0245", sp, "use 'new Dictionary<K, V>()'");
                }
                // the allocation site's element type is the dictionary type itself (entries are key/value pairs)
                let site = self.new_alloc_site(t.clone(), sp);
                Expr::new(ExprKind::ListNew { site }, t, sp)
            }
            Ty::Struct(s) => {
                if in_spawn {
                    self.d.error("E0244", sp, "Spawn expects an entity class, not a struct");
                }
                let mut ctor = None;
                let mut hargs = vec![];
                if let Some(args) = args {
                    if !args.is_empty() || !self.p.structs[s].ctors.is_empty() {
                        // choose ctor by arity
                        let cands: Vec<FuncId> =
                            self.p.structs[s].ctors.iter().copied().filter(|&f| self.p.funcs[f].params.len() == args.len()).collect();
                        match cands.len() {
                            0 if args.is_empty() => {}
                            0 => {
                                let sn = self.p.structs[s].name.clone();
                                self.d.error("E0241", sp, format!("'{}' has no constructor taking {} argument(s)", sn, args.len()));
                                return Expr::err(sp);
                            }
                            1 => {
                                ctor = Some(cands[0]);
                                hargs = self.check_args(cands[0], args, sp);
                            }
                            _ => {
                                self.d.error("E0241", sp, "ambiguous constructor overload (overloads must differ in arity)");
                                return Expr::err(sp);
                            }
                        }
                    }
                }
                let mut hinits = vec![];
                for (n, e, nsp) in inits {
                    match self.p.structs[s].fields.iter().position(|f| &f.name == n) {
                        Some(idx) => {
                            self.reference(*nsp, Sym::SField(s, idx));
                            let ft = self.p.structs[s].fields[idx].ty.clone();
                            let he = self.expr(e, Some(&ft));
                            let he = self.coerce(he, &ft);
                            hinits.push((idx, he));
                        }
                        None => {
                            let sn = self.p.structs[s].name.clone();
                            self.d.error("E0108", *nsp, format!("'{}' has no field '{}'", sn, n));
                        }
                    }
                }
                Expr::new(ExprKind::StructLit { sid: s, ctor, args: hargs, inits: hinits }, Ty::Struct(s), sp)
            }
            Ty::Handle(c) if self.p.classes[c].is_component => {
                let cn = self.p.classes[c].name.clone();
                let what = if in_spawn { format!("cannot spawn the component '{}'", cn) } else { format!("a component '{}' cannot be created on its own", cn) };
                self.d.error_note("E0265", sp, what, format!("a component belongs to an entity: 'e.Add(new {} {{ ... }})' (or 'Add(...)' in an entity method)", cn));
                Expr::err(sp)
            }
            Ty::Handle(c) => {
                if !in_spawn {
                    self.d.error_note(
                        "E0212",
                        sp,
                        "entities cannot be created with 'new'",
                        format!("use 'Spawn(new {} {{ ... }})'", self.p.classes[c].name),
                    );
                    return Expr::err(sp);
                }
                if self.p.classes[c].is_interface {
                    self.d.error_note("E0246", sp, format!("cannot spawn the interface '{}'", self.p.classes[c].name), "spawn a class that implements it");
                } else if self.p.classes[c].is_abstract {
                    self.d.error("E0246", sp, "cannot spawn an abstract entity class");
                }
                if args.map(|a| !a.is_empty()).unwrap_or(false) {
                    self.d.error("E0217", sp, "entity classes have no constructors; use an initializer list");
                }
                let mut hinits = vec![];
                for (n, e, nsp) in inits {
                    match self.p.find_field(c, n) {
                        Some(f) => {
                            self.reference(*nsp, Sym::Field(f));
                            let ft = self.p.fields[f].ty.clone();
                            let he = self.expr(e, Some(&ft));
                            let he = self.coerce(he, &ft);
                            if hinits.iter().any(|(x, _)| *x == f) {
                                self.d.error("E0104", *nsp, format!("field '{}' initialized twice", n));
                            }
                            hinits.push((f, he));
                        }
                        None => {
                            let cn = self.p.classes[c].name.clone();
                            self.d.error("E0108", *nsp, format!("'{}' has no field '{}'", cn, n));
                        }
                    }
                }
                Expr::new(ExprKind::Spawn { class: c, inits: hinits, comps: vec![] }, Ty::NHandle(c), sp)
            }
            other => {
                let tn = self.p.ty_name(&other);
                self.d.error("E0212", sp, format!("cannot create a value of type '{}' with 'new'", tn));
                Expr::err(sp)
            }
        }
    }

    /// Validates that `e` is a writable place, records struct self-writes.
    /// records that the variable or field at the root of the place `e` changes (for L316 unused-mut)
    fn note_write(&mut self, e: &Expr) {
        let mut root = e;
        while let ExprKind::Field { base, .. } | ExprKind::ListIndex { list: base, .. } = &root.kind {
            root = base;
        }
        match &root.kind {
            ExprKind::Local(l) => {
                if let Some(loc) = self.st.as_mut().and_then(|s| s.locals.get_mut(*l)) {
                    loc.written = true;
                }
            }
            ExprKind::EField { field, .. } => {
                self.p.written_fields.insert(*field);
            }
            _ => {}
        }
    }

    /// a `mut` struct method called on `r`: the receiver must be a place that may change here
    fn check_mut_receiver(&mut self, r: &Expr, callee: FuncId, sp: Span) {
        if r.ty == Ty::Error || !self.p.funcs[callee].writes_self {
            return;
        }
        if let Some((_, false)) = crate::effects::target_field(r) {
            self.d.error_note("E0403", sp, "mutating method called on a value that cannot be mutated here", "the value is a field of another entity: writes to other entities are deferred, so it cannot be mutated in place; assign a new value instead");
            return;
        }
        match self.mutable_place(r) {
            Ok(()) => self.note_write(r),
            Err(why) => {
                let why = if root_is_struct_self(r) {
                    let me = self.st.as_ref().and_then(|s| s.fid).map(|f| self.p.funcs[f].name.clone()).unwrap_or_default();
                    format!("'{}' changes 'this', so '{}' must be declared 'mut' too", self.p.funcs[callee].name, me)
                } else {
                    why
                };
                self.d.error_note("E0403", sp, "mutating method called on a value that cannot be mutated here", why);
            }
        }
    }

    fn check_assignable(&mut self, e: &Expr) {
        if e.ty == Ty::Error {
            return;
        }
        let fid = self.st.as_ref().and_then(|s| s.fid);
        let res = self.mutable_place(e);
        let _ = fid;
        self.assignable_result(e, res);
    }

    /// may the place `e` change here? (immutable bindings, constants, borrows, async parameters,
    /// for-header variables outside the step, `this` of a non-mut struct method)
    fn mutable_place(&self, e: &Expr) -> Result<(), String> {
        let fid = self.st.as_ref().and_then(|s| s.fid);
        let res = {
            let st = self.st.as_ref().unwrap();
            let locals = &st.locals;
            let mut root = e;
            while let ExprKind::Field { base, .. } | ExprKind::ListIndex { list: base, .. } = &root.kind {
                root = base;
            }
            match &root.kind {
                ExprKind::Local(l) if st.is_async && matches!(locals[*l].kind, LocalKind::Param(ParamMode::Value)) => {
                    Err(format!("'{}' is a parameter of an async method: it is read-only (copy it into a local)", locals[*l].name))
                }
                // the variables a for header declares change in its step (and only there)
                ExprKind::Local(l) if st.step_locals.contains(l) => Ok(()),
                ExprKind::Local(l) if locals[*l].kind == LocalKind::Normal && !locals[*l].is_mut && st.for_locals.contains(l) => Err(format!(
                    "'{}' is the loop variable: it changes only in the loop header (declare it 'mut' to change it in the body)",
                    locals[*l].name
                )),
                ExprKind::StructSelf if !st.mut_self => Err(match fid.map(|f| self.p.funcs[f].name.clone()) {
                    Some(n) => format!("'{}' changes 'this': declare the method 'mut' (public mut void {}(...))", n, n),
                    None => "'this' cannot change here".into(),
                }),
                _ => place_check_locals(&self.p, locals, e),
            }
        };
        res
    }

    fn assignable_result(&mut self, e: &Expr, res: Result<(), String>) {
        let fid = self.st.as_ref().and_then(|s| s.fid);
        match res {
            Ok(()) => {
                if root_is_struct_self(e) {
                    self.st().writes_self = true;
                }
                self.note_write(e);
            }
            Err(why) => {
                let _ = fid;
                // immutable bindings (E0310) vs places that can never be written (E0405)
                let mutability = why.contains("is immutable") || why.contains("changes 'this'") || why.contains("is the loop variable");
                let what = if mutability { "cannot change this value" } else { "cannot assign to this expression" };
                self.d.error_note(if mutability { "E0310" } else { "E0405" }, e.span, what, why);
            }
        }
    }
}

// ======================================================================
// components: Add / Remove / Has / Get (docs/04 §4.4.1)
// ======================================================================
impl<'a> Checker<'a> {
    // ======================================================================
    // delegates: lambdas and methods as values (docs/01 §1.14.1)
    // ======================================================================

    /// What a delegate's signature cannot have. Reports and returns false for each problem.
    fn check_delegate_sig(&mut self, params: &[Ty], ret: &Ty, sp: Span) -> bool {
        let mut ok = true;
        for t in params {
            if *t == Ty::Void {
                self.d.error("E0268", sp, "a delegate's parameter cannot be void");
                ok = false;
            }
            self.check_comp_storage(t, sp, "a parameter");
        }
        if ret.is_list() {
            self.d.error_note("E0268", sp, format!("a delegate cannot return a {}", coll_word(ret)), "the caller would own a collection made by whichever function the delegate holds: return a value, or fill a list the caller passes in");
            ok = false;
        }
        if let Ty::Handle(c) = ret {
            let cn = self.p.classes[*c].name.clone();
            self.d.error_note("E0268", sp, format!("a delegate cannot return the live entity reference '{}'", cn), format!("its default value returns null: return '{}?'", cn));
            ok = false;
        }
        self.check_comp_storage(ret, sp, "a return value");
        ok
    }

    /// `Func<...>`/`Action<...>` with these types: one delegate type per signature
    fn delegate_type(&mut self, params: Vec<Ty>, ret: Ty, sp: Span) -> DelegateId {
        if let Some(d) = self.p.delegates.iter().position(|d| d.name.is_none() && d.params == params && d.ret == ret) {
            return d;
        }
        self.p.delegates.push(DelegateDef { name: None, params, ret, funcs: vec![], invoke: usize::MAX, span: sp });
        let d = self.p.delegates.len() - 1;
        self.make_invoke(d);
        d
    }

    /// the static function a call through a value of delegate type `d` compiles to:
    /// `Invoke(d, a1, ..., an)`; its body (a choice by function number) is made once every lambda
    /// of the program is known (`build_invokes`)
    fn make_invoke(&mut self, d: DelegateId) {
        let owner = match self.dg_class {
            Some(c) => c,
            None => {
                self.p.classes.push(ClassDef {
                    name: "delegate".into(),
                    span: Span::default(),
                    base: None,
                    is_entity: false,
                    is_static: true,
                    is_abstract: false,
                    is_interface: false,
                    ifaces: vec![],
                    is_component: false,
                    combo_of: None,
                    comps: vec![],
                    own_fields: vec![],
                    methods: HashMap::new(),
                    overloads: vec![],
                    order: usize::MAX / 2,
                    run_order: 0,
                });
                let c = self.p.classes.len() - 1;
                self.dg_class = Some(c);
                c
            }
        };
        let dd = self.p.delegates[d].clone();
        let sp = dd.span;
        let mut locals = vec![Local { name: "d".into(), ty: Ty::Delegate(d), kind: LocalKind::Param(ParamMode::Value), span: sp, range: None, is_mut: false, written: false, is_const: false }];
        for (i, t) in dd.params.iter().enumerate() {
            locals.push(Local { name: format!("a{}", i), ty: t.clone(), kind: LocalKind::Param(ParamMode::Value), span: sp, range: None, is_mut: false, written: false, is_const: false });
        }
        let params = (0..locals.len()).collect();
        self.p.funcs.push(Func {
            name: format!("Invoke{}", d),
            kind: FuncKind::Static(owner),
            params,
            ret: dd.ret.clone(),
            locals,
            body: Block::default(),
            ctx: Ctx::Server,
            pure_attr: false,
            deferred_attr: false,
            is_property: false,
            is_override: false,
            is_abstract: false,
            span: sp,
            writes_self: false,
            co: None,
            extern_name: None,
            ret_range: None,
            recursive_attr: false,
            name_span: sp,
            synth: Some(Synth::Invoke(d)),
        });
        self.p.delegates[d].invoke = self.p.funcs.len() - 1;
    }

    /// the function number of `f` among the functions values of delegate type `d` hold
    fn delegate_fn(&mut self, d: DelegateId, f: FuncId) -> usize {
        match self.p.delegates[d].funcs.iter().position(|&g| g == f) {
            Some(i) => i + 1,
            None => {
                self.p.delegates[d].funcs.push(f);
                self.p.delegates[d].funcs.len()
            }
        }
    }

    /// the context of the function being checked (field initializers run on the server)
    fn cur_ctx(&self) -> Ctx {
        self.st.as_ref().and_then(|s| s.fid).map(|f| self.p.funcs[f].ctx).unwrap_or(Ctx::Server)
    }

    /// A lambda where a delegate of type `dg` is expected. Its body is checked here, in the
    /// enclosing function (so it sees the enclosing locals, fields and methods), then it moves to a
    /// function of its own: the enclosing locals it uses become its first parameters (captured by
    /// value when the delegate is made), and so does `this`, through which the body then reaches
    /// its entity like any other entity reference (snapshot reads, deferred writes and calls).
    fn lambda_value(&mut self, lps: &[(Option<ast::TypeExpr>, String, Span)], body: &ast::LambdaBody, dg: DelegateId, sp: Span) -> Expr {
        let (ptys, ret) = (self.p.delegates[dg].params.clone(), self.p.delegates[dg].ret.clone());
        let dname = self.p.delegate_name(dg);
        if lps.len() != ptys.len() {
            self.d.error("E0268", sp, format!("'{}' takes {} argument(s), but the lambda has {} parameter(s)", dname, ptys.len(), lps.len()));
            return Expr::err(sp);
        }
        if self.st.is_none() {
            return Expr::err(sp);
        }
        if self.cur_ctx() == Ctx::Client {
            self.d.error_note("E0268", sp, "client code cannot make a delegate", "delegates run on the server: make the delegate in server code, or call a method directly here");
            return Expr::err(sp);
        }
        let (owner, encl, mut_self) = {
            let st = self.st.as_ref().unwrap();
            (st.owner, st.fid, st.mut_self)
        };
        let base = self.st.as_ref().unwrap().locals.len();
        let (refs_before, sites_before) = (self.p.index.refs.len(), self.p.alloc_sites.len());
        // the enclosing statement's state, restored after the lambda
        let st = self.st.as_mut().unwrap();
        let saved_ret = std::mem::replace(&mut st.ret, ret.clone());
        let saved_range = st.ret_range.take();
        let saved_depths = (st.loop_depth, st.switch_depth, st.atomic_depth, st.is_async, st.stmt_scope, st.writes_self);
        let saved_for = std::mem::take(&mut st.for_locals);
        let saved_step = std::mem::take(&mut st.step_locals);
        let saved_out = std::mem::take(&mut st.out_decls);
        let saved_base = st.lam_base.replace(base);
        let saved_rush = st.rush_bg.take();
        st.loop_depth = 0;
        st.switch_depth = 0;
        st.atomic_depth = 0;
        st.is_async = false;
        self.push_scope();
        let depth = self.st().scopes.len() - 1;
        self.st().stmt_scope = depth;
        let mut lparams = vec![];
        for (i, (t, n, psp)) in lps.iter().enumerate() {
            if let Some(t) = t {
                let tt = self.resolve_type(t);
                if tt != ptys[i] && tt != Ty::Error && ptys[i] != Ty::Error {
                    let (a, b) = (self.p.ty_name(&tt), self.p.ty_name(&ptys[i]));
                    self.d.error("E0200", *psp, format!("lambda parameter '{}' is declared '{}' but '{}' passes '{}'", n, a, dname, b));
                }
            }
            lparams.push(self.declare_local(n, ptys[i].clone(), LocalKind::Param(ParamMode::Value), *psp));
        }
        let mut hb = match body {
            ast::LambdaBody::Expr(x) if ret == Ty::Void => {
                let he = self.expr(x, None);
                Block { stmts: vec![Stmt::Expr(he)] }
            }
            ast::LambdaBody::Expr(x) => {
                let he = self.expr(x, Some(&ret));
                let he = self.coerce(he, &ret);
                Block { stmts: vec![Stmt::Return(Some(he), x.span)] }
            }
            ast::LambdaBody::Block(b) => {
                let hb = self.block(b);
                if ret != Ty::Void && ret != Ty::Error && !always_returns(&hb) {
                    self.d.error("E0207", b.span, "not all code paths of the lambda return a value");
                }
                hb
            }
        };
        self.pop_scope();
        let st = self.st.as_mut().unwrap();
        st.ret = saved_ret;
        st.ret_range = saved_range;
        (st.loop_depth, st.switch_depth, st.atomic_depth, st.is_async, st.stmt_scope, st.writes_self) = saved_depths;
        st.for_locals = saved_for;
        st.step_locals = saved_step;
        st.out_decls = saved_out;
        st.lam_base = saved_base;
        st.rush_bg = saved_rush;
        let lam_locals: Vec<Local> = st.locals.drain(base..).collect();
        // what the body uses from outside: enclosing locals and `this` (one capture per type: in
        // a component, `this` is the component and `Owner` the entity)
        let mut caps: Vec<(LocalId, Span)> = vec![];
        let mut selfs: Vec<(Ty, Span)> = vec![];
        let mut base_call = None;
        visit_block(&hb, &mut |e| match &e.kind {
            ExprKind::Local(l) if *l < base && !caps.iter().any(|(x, _)| x == l) => caps.push((*l, e.span)),
            ExprKind::SelfRef | ExprKind::StructSelf if !selfs.iter().any(|(t, _)| *t == e.ty) => selfs.push((e.ty.clone(), e.span)),
            ExprKind::Destroy(None) => {
                if let Owner::Entity(c) = owner {
                    if !selfs.iter().any(|(t, _)| *t == Ty::Handle(c)) {
                        selfs.push((Ty::Handle(c), e.span));
                    }
                }
            }
            ExprKind::Call { dispatch: Dispatch::BaseCall, .. } => base_call = Some(e.span),
            _ => {}
        });
        let mut bad = false;
        if let Some(bsp) = base_call {
            self.d.error_note("E0268", bsp, "a lambda stored as a delegate cannot call 'base'", "it reaches its entity through a reference, which dispatches virtually: call a method that calls 'base' instead");
            bad = true;
        }
        for &(l, usp) in &caps {
            let loc = self.st.as_ref().unwrap().locals[l].clone();
            let why = if matches!(loc.kind, LocalKind::Param(ParamMode::Ref | ParamMode::Out)) {
                Some((format!("the lambda uses '{}', a ref/out parameter", loc.name), "a delegate keeps a copy of what it uses and may run after the call has returned: copy the value into a local first".to_string()))
            } else if loc.ty.is_list() {
                Some((format!("the lambda uses the {} '{}'", coll_word(&loc.ty), loc.name), "a delegate keeps copies of the values it uses (numbers, strings, structs, entity references, delegates), not collections: keep the collection on an entity and pass a reference".to_string()))
            } else if loc.is_mut {
                Some((format!("the lambda uses '{}', which is 'mut'", loc.name), format!("a delegate keeps a copy made when it is created, so later changes to '{}' would not reach it: copy the value into an immutable local first ('{} {}0 = {};')", loc.name, self.p.ty_name(&loc.ty), loc.name, loc.name)))
            } else {
                None
            };
            if let Some((msg, note)) = why {
                self.d.error_note("E0268", usp, msg, note);
                bad = true;
            }
        }
        if selfs.iter().any(|(t, _)| matches!(t, Ty::Struct(_))) && mut_self {
            let ssp = selfs.iter().find(|(t, _)| matches!(t, Ty::Struct(_))).unwrap().1;
            self.d.error_note("E0268", ssp, "a lambda in a 'mut' struct method cannot use 'this'", "the method may change 'this' after the delegate copies it: copy what the lambda needs into immutable locals");
            bad = true;
        }
        if bad {
            return Expr::err(sp);
        }
        // the lambda's own function: captures first, then its parameters and locals
        let mut locals: Vec<Local> = vec![];
        let mut map: HashMap<LocalId, LocalId> = HashMap::new();
        for &(l, _) in &caps {
            let mut loc = self.st.as_ref().unwrap().locals[l].clone();
            loc.kind = LocalKind::Param(ParamMode::Value);
            loc.is_mut = false;
            loc.written = false;
            loc.range = None;
            loc.is_const = false;
            map.insert(l, locals.len());
            locals.push(loc);
        }
        let mut self_locals: Vec<(Ty, LocalId)> = vec![];
        for (t, ssp) in &selfs {
            self_locals.push((t.clone(), locals.len()));
            locals.push(Local { name: "this".into(), ty: t.clone(), kind: LocalKind::Param(ParamMode::Value), span: *ssp, range: None, is_mut: false, written: false, is_const: false });
        }
        let ncaps = locals.len();
        for (i, loc) in lam_locals.into_iter().enumerate() {
            map.insert(base + i, locals.len());
            locals.push(loc);
        }
        let mut params: Vec<LocalId> = (0..ncaps).collect();
        params.extend(lparams.iter().map(|l| map[l]));
        let self_handle = match owner {
            Owner::Entity(c) => self_locals.iter().find(|(t, _)| *t == Ty::Handle(c)).map(|(t, l)| (t.clone(), *l)),
            _ => None,
        };
        remap_lambda_block(&mut hb, &map, &self_locals, self_handle.as_ref());
        let ename = encl.map(|f| self.p.funcs[f].name.clone()).unwrap_or_else(|| "init".into());
        let key = encl.map(|f| self.p.func_label(f)).unwrap_or_else(|| format!("init:{:?}", owner));
        let k = {
            let c = self.lambda_count.entry(key).or_insert(0);
            *c += 1;
            *c
        };
        let kind = match owner {
            Owner::Struct(s) => FuncKind::StructStatic(s),
            Owner::Entity(c) | Owner::Static(c) => FuncKind::Static(c),
        };
        let fid = self.p.funcs.len();
        self.p.funcs.push(Func {
            name: format!("{}__lambda{}", ename, k),
            kind,
            params,
            ret: ret.clone(),
            locals,
            body: hb,
            ctx: Ctx::Server,
            pure_attr: false,
            deferred_attr: false,
            is_property: false,
            is_override: false,
            is_abstract: false,
            span: sp,
            writes_self: false,
            co: None,
            extern_name: None,
            ret_range: None,
            recursive_attr: false,
            name_span: sp,
            synth: Some(Synth::Lambda { dg, caps: ncaps, of: encl }),
        });
        // what the body declared belongs to the lambda's function now
        if let Some(ef) = encl {
            for (_, sym) in self.p.index.refs[refs_before..].iter_mut() {
                if let Sym::Local(f, l) = sym {
                    if *f == ef && *l >= base {
                        *sym = Sym::Local(fid, map[l]);
                    }
                }
            }
            let moved: Vec<(Sym, Span)> = self.p.index.defs.iter().filter(|(k, _)| matches!(k, Sym::Local(f, l) if *f == ef && *l >= base)).map(|(k, v)| (*k, *v)).collect();
            for (k, v) in moved {
                self.p.index.defs.remove(&k);
                if let Sym::Local(_, l) = k {
                    if let Some(&nl) = map.get(&l) {
                        self.p.index.defs.insert(Sym::Local(fid, nl), v);
                    }
                }
            }
        }
        for a in self.p.alloc_sites[sites_before..].iter_mut() {
            if encl.map(|f| a.func == f).unwrap_or(a.func == usize::MAX) {
                a.func = fid;
            }
        }
        if !self.quiet {
            self.p.index.func_spans.push((fid, sp));
        }
        let num = self.delegate_fn(dg, fid);
        let mut cap_exprs: Vec<Expr> = caps.iter().map(|&(l, usp)| Expr::new(ExprKind::Local(l), self.st.as_ref().unwrap().locals[l].ty.clone(), usp)).collect();
        for (t, ssp) in &selfs {
            let k = if matches!(t, Ty::Struct(_)) { ExprKind::StructSelf } else { ExprKind::SelfRef };
            cap_exprs.push(Expr::new(k, t.clone(), *ssp));
        }
        Expr::new(ExprKind::Closure { dg, num, caps: cap_exprs }, Ty::Delegate(dg), sp)
    }

    /// `F` or `T.F` (or `x.M`, `M` on this) where a delegate of type `dg` is expected: the method
    /// whose parameter and return types are the delegate's (`None` when no method has the name)
    fn method_group(&mut self, base: Option<&ast::Expr>, name: &str, dg: DelegateId, sp: Span, nsp: Span) -> Option<Expr> {
        let st_owner = self.st.as_ref()?.owner;
        let is_static_ctx = self.st.as_ref()?.is_static;
        let snapshot = (self.d.list.len(), self.p.index.refs.len());
        // the candidates and the receiver of an instance method (None: static)
        let (cands, recv): (Vec<FuncId>, Option<Expr>) = match base {
            None => match st_owner {
                Owner::Struct(s) => (self.p.struct_methods_named(s, name), None),
                Owner::Entity(c) | Owner::Static(c) => {
                    if self.p.find_field(c, name).is_some() {
                        return None;
                    }
                    let ms = self.p.methods_named(c, name);
                    let recv = if matches!(st_owner, Owner::Entity(_)) && !is_static_ctx { Some(Expr::new(ExprKind::SelfRef, Ty::Handle(c), nsp)) } else { None };
                    (ms, recv)
                }
            },
            Some(b) => match self.type_base(b) {
                Some(Ok(TypeRef::Class(c))) => (self.p.methods_named(c, name).into_iter().filter(|&f| matches!(self.p.funcs[f].kind, FuncKind::Static(_))).collect(), None),
                Some(Ok(TypeRef::Struct(s))) => (self.p.struct_methods_named(s, name).into_iter().filter(|&f| matches!(self.p.funcs[f].kind, FuncKind::StructStatic(_))).collect(), None),
                Some(_) => return None,
                None => {
                    // `ship.Hit`: an entity method bound to the reference
                    if !matches!(b.kind, ast::ExprKind::Name(_) | ast::ExprKind::This | ast::ExprKind::Member(..)) {
                        return None;
                    }
                    let hb = self.expr(b, None);
                    let Ty::Handle(c) = hb.ty else {
                        self.d.list.truncate(snapshot.0);
                        self.p.index.refs.truncate(snapshot.1);
                        return None;
                    };
                    if self.p.find_field(c, name).is_some() {
                        self.d.list.truncate(snapshot.0);
                        self.p.index.refs.truncate(snapshot.1);
                        return None;
                    }
                    (self.p.methods_named(c, name).into_iter().filter(|&f| matches!(self.p.funcs[f].kind, FuncKind::EntityMethod(_))).collect(), Some(hb))
                }
            },
        };
        if cands.is_empty() {
            self.d.list.truncate(snapshot.0);
            self.p.index.refs.truncate(snapshot.1);
            return None;
        }
        let (ptys, ret) = (self.p.delegates[dg].params.clone(), self.p.delegates[dg].ret.clone());
        let fits = |p: &Program, f: FuncId| {
            let func = &p.funcs[f];
            func.ret == ret && !func.is_property && func.params.len() == ptys.len() && func.params.iter().zip(&ptys).all(|(&l, t)| func.locals[l].ty == *t && func.locals[l].kind == LocalKind::Param(ParamMode::Value))
        };
        let Some(f) = cands.iter().copied().find(|&f| fits(&self.p, f)) else {
            let list: Vec<String> = cands.iter().map(|&f| format!("'{} {}'", self.p.ty_name(&self.p.funcs[f].ret), self.sig_text(f))).collect();
            let dn = self.p.delegate_name(dg);
            self.d.error_note("E0268", nsp, format!("no method '{}' has the signature of '{}'", name, dn), format!("a method group needs exactly the delegate's parameter and return types (plain value parameters); found {}", list.join(", ")));
            return Some(Expr::err(sp));
        };
        self.reference(nsp, Sym::Func(f));
        if self.cur_ctx() == Ctx::Client {
            self.d.error_note("E0268", sp, "client code cannot make a delegate", "delegates run on the server: make the delegate in server code, or call the method directly here");
            return Some(Expr::err(sp));
        }
        match self.p.funcs[f].kind {
            FuncKind::Static(_) | FuncKind::StructStatic(_) => {
                if self.p.funcs[f].extern_name.is_some() {
                    self.d.error_note("E0268", nsp, format!("the extern function '{}' cannot be a delegate value", name), "wrap it in a lambda: x => F(x)");
                    return Some(Expr::err(sp));
                }
                let num = self.delegate_fn(dg, f);
                Some(Expr::new(ExprKind::Closure { dg, num, caps: vec![] }, Ty::Delegate(dg), sp))
            }
            FuncKind::EntityMethod(c) => {
                let Some(recv) = recv else {
                    self.d.error("E0106", nsp, format!("cannot use instance method '{}' from a static context", name));
                    return Some(Expr::err(sp));
                };
                if self.p.async_funcs.contains(&f) {
                    self.d.error_note("E0268", nsp, format!("the coroutine '{}' cannot be a delegate value", name), "a coroutine runs as its entity's system; start it from a method the delegate calls");
                    return Some(Expr::err(sp));
                }
                // a function of its own that calls the method on the captured reference
                let rt = recv.ty.clone();
                let mut locals = vec![Local { name: "target".into(), ty: rt.clone(), kind: LocalKind::Param(ParamMode::Value), span: nsp, range: None, is_mut: false, written: false, is_const: false }];
                for (i, t) in ptys.iter().enumerate() {
                    locals.push(Local { name: format!("a{}", i), ty: t.clone(), kind: LocalKind::Param(ParamMode::Value), span: nsp, range: None, is_mut: false, written: false, is_const: false });
                }
                let args: Vec<HArg> = ptys.iter().enumerate().map(|(i, t)| HArg { mode: ParamMode::Value, expr: Expr::new(ExprKind::Local(i + 1), t.clone(), nsp) }).collect();
                let call = Expr::new(ExprKind::Call { func: f, recv: Some(Box::new(Expr::new(ExprKind::Local(0), rt, nsp))), args, dispatch: Dispatch::RemoteRead }, ret.clone(), nsp);
                let body = if ret == Ty::Void { Block { stmts: vec![Stmt::Expr(call)] } } else { Block { stmts: vec![Stmt::Return(Some(call), nsp)] } };
                // named after the method it calls (an interface's method: where it is made)
                let owner_c = match st_owner {
                    _ if !self.p.classes[c].is_interface => c,
                    Owner::Entity(k) | Owner::Static(k) => k,
                    Owner::Struct(_) => c,
                };
                let params = (0..locals.len()).collect();
                self.p.funcs.push(Func {
                    name: format!("{}__group", self.p.funcs[f].name),
                    kind: FuncKind::Static(owner_c),
                    params,
                    ret,
                    locals,
                    body,
                    ctx: Ctx::Server,
                    pure_attr: false,
            deferred_attr: false,
                    is_property: false,
                    is_override: false,
                    is_abstract: false,
                    span: sp,
                    writes_self: false,
                    co: None,
                    extern_name: None,
                    ret_range: None,
                    recursive_attr: false,
                    name_span: nsp,
                    synth: Some(Synth::Lambda { dg, caps: 1, of: self.st.as_ref().and_then(|s| s.fid) }),
                });
                let g = self.p.funcs.len() - 1;
                let num = self.delegate_fn(dg, g);
                Some(Expr::new(ExprKind::Closure { dg, num, caps: vec![recv] }, Ty::Delegate(dg), sp))
            }
            _ => {
                self.d.error_note("E0268", nsp, format!("the struct method '{}' cannot be a delegate value", name), "use a lambda that calls it on a copy: x => s.M(x)");
                Some(Expr::err(sp))
            }
        }
    }

    /// `f(args)` / `f.Invoke(args)` for a delegate value `f`: a call of the delegate type's invoke
    /// function, which runs whatever function the value holds
    fn invoke_expr(&mut self, target: Expr, args: &[ast::Arg], sp: Span) -> Expr {
        let Ty::Delegate(dg) = target.ty else { return Expr::err(sp) };
        let (ptys, ret, inv) = (self.p.delegates[dg].params.clone(), self.p.delegates[dg].ret.clone(), self.p.delegates[dg].invoke);
        if self.cur_ctx() == Ctx::Client {
            self.d.error_note("E0268", sp, "client code cannot call a delegate", "delegates run on the server: call methods directly in client code");
            return Expr::err(sp);
        }
        if args.len() != ptys.len() {
            let dn = self.p.delegate_name(dg);
            self.d.error("E0241", sp, format!("'{}' takes {} argument(s), found {}", dn, ptys.len(), args.len()));
            return Expr::err(sp);
        }
        let mut hargs = vec![HArg { mode: ParamMode::Value, expr: target }];
        for (i, a) in args.iter().enumerate() {
            if a.mode != ParamMode::Value {
                self.d.error("E0401", a.expr.span, "a delegate's arguments are plain values (no ref/out)");
            }
            let he = self.expr(&a.expr, Some(&ptys[i]));
            let he = self.coerce(he, &ptys[i]);
            hargs.push(HArg { mode: ParamMode::Value, expr: he });
        }
        Expr::new(ExprKind::Call { func: inv, recv: None, args: hargs, dispatch: Dispatch::Static }, ret, sp)
    }

    /// a field of delegate type named by an unqualified call `F(...)` in the current class or struct
    fn own_delegate_field(&mut self, n: &str, sp: Span) -> Option<Expr> {
        let st = self.st.as_ref()?;
        if st.is_static {
            return None;
        }
        match st.owner {
            Owner::Entity(c) => {
                let f = self.p.find_field(c, n)?;
                if !matches!(self.p.fields[f].ty, Ty::Delegate(_)) {
                    return None;
                }
                Some(self.name_expr(n, sp))
            }
            Owner::Struct(s) => {
                let i = self.p.structs[s].fields.iter().position(|f| f.name == n)?;
                if !matches!(self.p.structs[s].fields[i].ty, Ty::Delegate(_)) {
                    return None;
                }
                Some(self.name_expr(n, sp))
            }
            Owner::Static(_) => None,
        }
    }

    /// The invoke function of each delegate type: it runs the function the value holds (by number),
    /// passing the captured values before the arguments. A value that captured a live entity
    /// reference does nothing (and returns the default) once that entity is gone, like a message
    /// to a destroyed entity.
    fn build_invokes(&mut self) {
        for dg in 0..self.p.delegates.len() {
            let inv = self.p.delegates[dg].invoke;
            if inv == usize::MAX {
                continue;
            }
            let (fns, ret, ptys, sp) = (self.p.delegates[dg].funcs.clone(), self.p.delegates[dg].ret.clone(), self.p.delegates[dg].params.clone(), self.p.delegates[dg].span);
            let d = || Expr::new(ExprKind::Local(0), Ty::Delegate(dg), sp);
            let mut stmts = vec![];
            for (k, &f) in fns.iter().enumerate() {
                let ncaps = self.p.caps_of(f);
                let cap_tys: Vec<Ty> = self.p.funcs[f].params[..ncaps].iter().map(|&l| self.p.funcs[f].locals[l].ty.clone()).collect();
                let mut args: Vec<HArg> = cap_tys.iter().enumerate().map(|(j, t)| HArg { mode: ParamMode::Value, expr: Expr::new(ExprKind::DgCap { d: Box::new(d()), dg, idx: k, cap: j }, t.clone(), sp) }).collect();
                args.extend(ptys.iter().enumerate().map(|(i, t)| HArg { mode: ParamMode::Value, expr: Expr::new(ExprKind::Local(i + 1), t.clone(), sp) }));
                let call = Expr::new(ExprKind::Call { func: f, recv: None, args, dispatch: Dispatch::Static }, ret.clone(), sp);
                let mut body = if ret == Ty::Void { vec![Stmt::Expr(call), Stmt::Return(None, sp)] } else { vec![Stmt::Return(Some(call), sp)] };
                // every captured live reference must still be alive
                let mut alive: Option<Expr> = None;
                for (j, t) in cap_tys.iter().enumerate() {
                    if let Ty::Handle(c) = t {
                        let x = Expr::new(ExprKind::DgCap { d: Box::new(d()), dg, idx: k, cap: j }, Ty::NHandle(*c), sp);
                        let is = Expr::new(ExprKind::Is { expr: Box::new(x), class: *c, bind: None }, Ty::Bool, sp);
                        alive = Some(match alive {
                            None => is,
                            Some(a) => Expr::new(ExprKind::Binary(BinOp::And, Box::new(a), Box::new(is)), Ty::Bool, sp),
                        });
                    }
                }
                if let Some(cond) = alive {
                    body = vec![Stmt::If { cond, then: Block { stmts: body }, els: None }];
                }
                let num = Expr::new(ExprKind::DgFn(Box::new(d())), Ty::Int, sp);
                let cond = Expr::new(ExprKind::Binary(BinOp::Eq, Box::new(num), Box::new(Expr::new(ExprKind::Int(k as i64 + 1), Ty::Int, sp))), Ty::Bool, sp);
                stmts.push(Stmt::If { cond, then: Block { stmts: body }, els: None });
            }
            if ret != Ty::Void {
                let z = consteval::zero_value(&self.p, &ret).map(|v| crate::opt::to_expr(&self.p, &v, &ret, sp)).unwrap_or_else(|| Expr::err(sp));
                stmts.push(Stmt::Return(Some(z), sp));
            }
            self.p.funcs[inv].body = Block { stmts };
        }
    }
}

/// Moves a lambda's body into its own function: locals get their new numbers, `this` becomes the
/// captured reference (or struct copy) of its type, and calls of `this`'s methods become calls on
/// that reference.
fn remap_lambda_block(b: &mut Block, map: &HashMap<LocalId, LocalId>, selfs: &[(Ty, LocalId)], self_handle: Option<&(Ty, LocalId)>) {
    for s in &mut b.stmts {
        remap_lambda_stmt(s, map, selfs, self_handle);
    }
}

fn remap_lambda_stmt(s: &mut Stmt, map: &HashMap<LocalId, LocalId>, selfs: &[(Ty, LocalId)], self_handle: Option<&(Ty, LocalId)>) {
    let m = |l: &mut LocalId| {
        if let Some(&n) = map.get(l) {
            *l = n;
        }
    };
    match s {
        Stmt::Let { local, .. } => m(local),
        Stmt::ForQuery { var, alias, .. } => {
            m(var);
            for a in alias.iter_mut() {
                m(a);
            }
        }
        Stmt::ForList { var, idx, .. } => {
            m(var);
            m(idx);
        }
        Stmt::Switch { tmp: Some(t), .. } => m(t),
        _ => {}
    }
    for blk in stmt_blocks_mut(s) {
        remap_lambda_block(blk, map, selfs, self_handle);
    }
    let mut fix = |e: &mut Expr| remap_lambda_expr(e, map, selfs, self_handle);
    match s {
        Stmt::Let { init: Some(e), .. } | Stmt::Expr(e) | Stmt::Return(Some(e), _) => crate::opt::visit_expr_mut(e, &mut fix),
        Stmt::If { cond, .. } => crate::opt::visit_expr_mut(cond, &mut fix),
        Stmt::Loop { cond, step, .. } => {
            if let Some(c) = cond {
                crate::opt::visit_expr_mut(c, &mut fix);
            }
            for x in step.iter_mut() {
                crate::opt::visit_expr_mut(x, &mut fix);
            }
        }
        Stmt::ForList { list, .. } => crate::opt::visit_expr_mut(list, &mut fix),
        Stmt::Switch { scrut, sections, .. } => {
            crate::opt::visit_expr_mut(scrut, &mut fix);
            for (c, _) in sections.iter_mut() {
                crate::opt::visit_expr_mut(c, &mut fix);
            }
        }
        _ => {}
    }
}

fn remap_lambda_expr(e: &mut Expr, map: &HashMap<LocalId, LocalId>, selfs: &[(Ty, LocalId)], self_handle: Option<&(Ty, LocalId)>) {
    match &mut e.kind {
        ExprKind::Local(l) => {
            if let Some(&n) = map.get(l) {
                *l = n;
            }
        }
        ExprKind::SelfRef | ExprKind::StructSelf => {
            if let Some((_, l)) = selfs.iter().find(|(t, _)| *t == e.ty) {
                e.kind = ExprKind::Local(*l);
            }
        }
        ExprKind::Destroy(target @ None) => {
            if let Some((t, l)) = self_handle {
                *target = Some(Box::new(Expr::new(ExprKind::Local(*l), t.clone(), e.span)));
            }
        }
        ExprKind::Call { dispatch, .. } if *dispatch == Dispatch::SelfCall => *dispatch = Dispatch::RemoteRead,
        ExprKind::ListLambda { params, .. } => {
            for p in params.iter_mut() {
                if let Some(&n) = map.get(p) {
                    *p = n;
                }
            }
        }
        ExprKind::Is { bind: Some(b), .. } => {
            if let Some(&n) = map.get(b) {
                *b = n;
            }
        }
        ExprKind::Switch { tmp: Some(t), .. } => {
            if let Some(&n) = map.get(t) {
                *t = n;
            }
        }
        _ => {}
    }
}

impl<'a> Checker<'a> {
    /// the component a stored type refers to (a handle, or the elements of a collection)
    fn comp_in(&self, t: &Ty, depth: usize) -> Option<ClassId> {
        if depth > 32 {
            return None;
        }
        match t {
            Ty::Handle(c) | Ty::NHandle(c) if self.p.classes[*c].is_component => Some(*c),
            Ty::List(e) | Ty::Array(e) => self.comp_in(e, depth + 1),
            Ty::Dict(k, v) => self.comp_in(k, depth + 1).or_else(|| self.comp_in(v, depth + 1)),
            _ => None,
        }
    }

    /// a component exists only as part of an entity, seen from code that runs while the entity has
    /// it: fields, parameters, return values and collections cannot hold one (E0263)
    fn check_comp_storage(&mut self, t: &Ty, sp: Span, what: &str) {
        if let Some(k) = self.comp_in(t, 0) {
            let kn = self.p.classes[k].name.clone();
            self.d.error_note(
                "E0263",
                sp,
                format!("{} cannot hold the component '{}'", what, kn),
                format!("a component is part of an entity and can be removed at any time: keep the entity (a nullable handle such as 'Ship?') and look the component up with 'e.Get<{}>() is {} c' where you need it", kn, kn),
            );
        }
    }

    fn comp_named(&self, name: &str) -> Option<ClassId> {
        match self.types.get(name) {
            Some(&TypeRef::Class(k)) if self.p.classes[k].is_component => Some(k),
            _ => None,
        }
    }

    /// `Add(new C { ... })` / `Remove<C>()` / `Has<C>()` / `Get<C>()` with a component C
    fn comp_op_form(&self, name: &str, type_args: &[ast::TypeExpr], args: &[ast::Arg]) -> bool {
        match name {
            "Add" => type_args.is_empty() && args.len() == 1 && matches!(&args[0].expr.kind, ast::ExprKind::New { ty, .. } if ty.args.is_empty() && self.comp_named(&ty.name).is_some()),
            _ => type_args.len() == 1 && args.is_empty() && type_args[0].args.is_empty() && self.comp_named(&type_args[0].name).is_some(),
        }
    }

    /// does the type whose method is being checked have a method `name` (its own, inherited, from an
    /// interface, or a generic one)? It then wins over a built-in or a component operation.
    fn has_own_method(&self, name: &str) -> bool {
        let Some(st) = self.st.as_ref() else { return false };
        match st.owner {
            Owner::Struct(s) => self.p.structs[s].methods.contains_key(name) || self.templates.contains_key(&(TypeRef::Struct(s), name.to_string())),
            Owner::Entity(c) | Owner::Static(c) => self.p.resolve_method(c, name).is_some() || self.find_template_class(c, name).is_some(),
        }
    }

    /// the note for a nullable handle that came from `x.Get<C>()`
    fn get_note(&self, base: &ast::Expr, cn: &str) -> Option<String> {
        let ast::ExprKind::Call { callee, type_args, .. } = &base.kind else { return None };
        let recv = match &callee.kind {
            ast::ExprKind::Member(r, n, _) if n == "Get" => expr_text(r),
            ast::ExprKind::Name(n) if n == "Get" => String::new(),
            _ => return None,
        };
        if type_args.len() != 1 {
            return None;
        }
        let dot = if recv.is_empty() { String::new() } else { format!("{}.", recv) };
        Some(format!("Get<{cn}>() is null when the entity has no '{cn}': check it first, 'if ({dot}Get<{cn}>() is {cn} c) {{ ... c ... }}' (or test {dot}Has<{cn}>())"))
    }

    fn comp_op(&mut self, recv: Option<Expr>, name: &str, type_args: &[ast::TypeExpr], args: &[ast::Arg], sp: Span, nsp: Span) -> Expr {
        let usage = match name {
            "Add" => "usage: e.Add(new C { F = x }) with a component C",
            "Remove" => "usage: e.Remove<C>() with a component C",
            "Has" => "usage: e.Has<C>() with a component C",
            _ => "usage: if (e.Get<C>() is C c) { ... } with a component C",
        };
        // the entity: the receiver, or `this` in an entity or component method
        let target = match recv {
            Some(r) => r,
            None => {
                let st = self.st.as_ref().unwrap();
                match (st.owner, st.is_static) {
                    (Owner::Entity(c), false) => Expr::new(ExprKind::SelfRef, Ty::Handle(c), sp),
                    _ => {
                        self.d.error_note("E0265", nsp, format!("'{}' without a receiver is only valid in an entity or component method", name), format!("name the entity: 'e.{}'", if name == "Add" { "Add(new C { ... })".to_string() } else { format!("{}<C>()", name) }));
                        return Expr::err(sp);
                    }
                }
            }
        };
        if name == "Add" {
            let new = match (type_args.is_empty(), args) {
                (true, [a]) => match &a.expr.kind {
                    ast::ExprKind::New { ty, args: cargs, inits } => Some((ty, cargs, inits, a.expr.span)),
                    _ => None,
                },
                _ => None,
            };
            let Some((ty, cargs, inits, esp)) = new else {
                self.d.error_note("E0265", sp, "Add expects 'new C { ... }' where C is a component", usage);
                for a in args {
                    let _ = self.expr(&a.expr, None);
                }
                return Expr::err(sp);
            };
            let k = match self.resolve_type(ty) {
                Ty::Handle(k) if self.p.classes[k].is_component && !ty.nullable => k,
                Ty::Error => return Expr::err(sp),
                t => {
                    let tn = self.p.ty_name(&t);
                    self.d.error_note("E0265", esp, format!("'{}' is not a component", tn), "only components can be added to an entity: declare 'component C { ... }' (an entity class is spawned with Spawn(new T { ... }))");
                    return Expr::err(sp);
                }
            };
            if cargs.as_ref().map(|a| !a.is_empty()).unwrap_or(false) {
                self.d.error("E0217", esp, "components have no constructors; use an initializer list: new C { F = x }");
            }
            let mut hinits: Vec<(FieldId, Expr)> = vec![];
            for (n, e, isp) in inits {
                match self.p.classes[k].own_fields.iter().copied().find(|&f| self.p.fields[f].name == *n) {
                    Some(f) => {
                        self.reference(*isp, Sym::Field(f));
                        let ft = self.p.fields[f].ty.clone();
                        let he = self.expr(e, Some(&ft));
                        let he = self.coerce(he, &ft);
                        if hinits.iter().any(|(x, _)| *x == f) {
                            self.d.error("E0104", *isp, format!("field '{}' initialized twice", n));
                        }
                        hinits.push((f, he));
                    }
                    None => {
                        let kn = self.p.classes[k].name.clone();
                        self.d.error("E0108", *isp, format!("component '{}' has no field '{}'", kn, n));
                    }
                }
            }
            return Expr::new(ExprKind::CompAdd { target: Box::new(target), comp: k, inits: hinits }, Ty::Void, sp);
        }
        if type_args.len() != 1 || !args.is_empty() {
            self.d.error_note("E0265", sp, format!("{}<C>() takes one component type and no arguments", name), usage);
            return Expr::err(sp);
        }
        let k = match self.resolve_type(&type_args[0]) {
            Ty::Handle(k) if self.p.classes[k].is_component && !type_args[0].nullable => k,
            Ty::Error => return Expr::err(sp),
            t => {
                let tn = self.p.ty_name(&t);
                let note = if matches!(t, Ty::Handle(_)) && name != "Remove" {
                    format!("to test the class of an entity use 'is': 'if (e is {} x)'", tn)
                } else {
                    "components are declared 'component C { ... }'".to_string()
                };
                self.d.error_note("E0265", type_args[0].span, format!("'{}' is not a component", tn), note);
                return Expr::err(sp);
            }
        };
        match name {
            "Remove" => Expr::new(ExprKind::CompRemove { target: Box::new(target), comp: k }, Ty::Void, sp),
            "Has" => Expr::new(ExprKind::Is { expr: Box::new(target), class: k, bind: None }, Ty::Bool, sp),
            _ => {
                // the same entity seen as the component, or null: `e.Get<C>() is C c`
                let hit = Expr::new(ExprKind::Is { expr: Box::new(target.clone()), class: k, bind: None }, Ty::Bool, sp);
                if crate::opt::is_pure_expr(&target) {
                    let mut val = target;
                    val.ty = Ty::NHandle(k);
                    return Expr::new(ExprKind::Cond(Box::new(hit), Box::new(val), Box::new(Expr::new(ExprKind::Null, Ty::NHandle(k), sp))), Ty::NHandle(k), sp);
                }
                // evaluate the receiver once
                let tname = format!("__get{}", self.st().locals.len());
                let tty = target.ty.clone();
                let tmp = self.declare_local(&tname, tty.clone(), LocalKind::Normal, sp);
                let tl = Expr::new(ExprKind::Local(tmp), tty, sp);
                let test = Expr::new(ExprKind::Is { expr: Box::new(tl.clone()), class: k, bind: None }, Ty::Bool, sp);
                let mut val = tl;
                val.ty = Ty::NHandle(k);
                Expr::new(ExprKind::Switch { tmp: Some(tmp), scrut: Box::new(target), arms: vec![(test, val)], default: Some(Box::new(Expr::new(ExprKind::Null, Ty::NHandle(k), sp))) }, Ty::NHandle(k), sp)
            }
        }
    }
}

/// a short source form of a receiver (`ship`, `this.Target`) for notes; `e` for anything longer
fn expr_text(e: &ast::Expr) -> String {
    match &e.kind {
        ast::ExprKind::Name(n) => n.clone(),
        ast::ExprKind::This => "this".into(),
        ast::ExprKind::Member(b, n, _) => format!("{}.{}", expr_text(b), n),
        _ => "e".into(),
    }
}

trait ParamModeOf {
    fn param_mode_of(&self, l: LocalId) -> ParamMode;
}
impl ParamModeOf for Func {
    fn param_mode_of(&self, l: LocalId) -> ParamMode {
        match self.locals[l].kind {
            LocalKind::Param(m) => m,
            _ => ParamMode::Value,
        }
    }
}

pub fn root_is_struct_self(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::StructSelf => true,
        ExprKind::Field { base, .. } => root_is_struct_self(base),
        _ => false,
    }
}

/// The calls of the program as written (before optimization folds any away): (caller, callee,
/// call span). base.M() and static calls reach that method; calls on an entity reach every
/// override of the method too (they dispatch on its archetype).
pub fn call_edges(p: &Program) -> Vec<(FuncId, FuncId, Span)> {
    let mut out = vec![];
    for (fid, f) in p.funcs.iter().enumerate() {
        visit_block(&f.body, &mut |e| match &e.kind {
            ExprKind::Call { func, dispatch: Dispatch::Static | Dispatch::BaseCall, .. } | ExprKind::OpCall { func, .. } => out.push((fid, *func, e.span)),
            ExprKind::Call { func, .. } => {
                out.push((fid, *func, e.span));
                let name = &p.funcs[*func].name;
                for (h, other) in p.funcs.iter().enumerate() {
                    if &other.name == name && matches!(other.kind, FuncKind::EntityMethod(_)) && other.is_override {
                        out.push((fid, h, e.span));
                    }
                }
            }
            ExprKind::StructLit { ctor: Some(c), .. } => out.push((fid, *c, e.span)),
            _ => {}
        });
    }
    out
}

/// Functions that can call themselves on the stack, with the edge that starts each cycle. `edges`
/// come from call_edges before optimization; `p` is the program after effect analysis, which
/// decided which calls are deferred messages or RPCs (they run later, not on this call's stack).
pub fn recursive_funcs(p: &Program, edges: &[(FuncId, FuncId, Span)]) -> HashMap<FuncId, FuncId> {
    let n = p.funcs.len();
    let mut later = std::collections::HashSet::new();
    for (fid, f) in p.funcs.iter().enumerate() {
        visit_block(&f.body, &mut |e| {
            if let ExprKind::Call { dispatch: Dispatch::RemoteDeferred | Dispatch::Rpc, .. } = &e.kind {
                later.insert((fid, e.span));
            }
        });
    }
    let mut g: Vec<std::collections::BTreeSet<FuncId>> = vec![Default::default(); n];
    for &(a, b, sp) in edges {
        if a < n && b < n && !later.contains(&(a, sp)) {
            g[a].insert(b);
        }
    }
    let mut rec = HashMap::new();
    for f in 0..n {
        for &first in &g[f] {
            let mut stack = vec![first];
            let mut seen = std::collections::HashSet::new();
            let mut found = false;
            while let Some(x) = stack.pop() {
                if x == f {
                    found = true;
                    break;
                }
                if seen.insert(x) {
                    stack.extend(g[x].iter().copied());
                }
            }
            if found {
                rec.insert(f, first);
                break;
            }
        }
    }
    rec
}

/// `[Recursive]` must mark exactly the functions that can call themselves: recursion costs one C
/// stack frame per level, and the attribute makes that visible where the function is declared
pub fn check_recursive(p: &Program, edges: &[(FuncId, FuncId, Span)], d: &mut Diags) {
    let rec = recursive_funcs(p, edges);
    let mut seen = std::collections::HashSet::new();
    for (fid, f) in p.funcs.iter().enumerate() {
        // a lambda or a delegate's invoke function has nowhere to write [Recursive]
        if f.extern_name.is_some() || f.synth.is_some() || !seen.insert(f.name_span) {
            continue;
        }
        match (rec.get(&fid), f.recursive_attr) {
            (Some(&via), false) => {
                let how = match p.funcs[via].synth {
                    Some(Synth::Invoke(d)) => format!("itself through a delegate of type '{}' (one of the functions it can hold calls '{}')", p.delegate_name(d), f.name),
                    Some(Synth::Lambda { .. }) => format!("itself through a lambda ('{}')", p.funcs[via].name),
                    None if via == fid => "itself".to_string(),
                    None => format!("itself through '{}'", p.funcs[via].name),
                };
                // `void Print(string s) { Print(...); }`: the method hides the built-in of its name
                let note = if via == fid && matches!(f.name.as_str(), "Spawn" | "Destroy" | "Print" | "Count" | "Query") {
                    format!("inside its class, a call '{}(...)' with this method's number of arguments calls the method, not the built-in '{}': rename the method to reach the built-in", f.name, f.name)
                } else {
                    "each level of recursion takes a stack frame; [Recursive] makes that visible where the function is declared".to_string()
                };
                d.error_note("E0312", f.name_span, format!("'{}' calls {}: mark it [Recursive]", f.name, how), note);
            }
            (None, true) => d.warn("W0312", f.name_span, format!("'{}' is marked [Recursive] but never calls itself", f.name)),
            _ => {}
        }
    }
}

/// Do two methods have the same signature for overriding and implementing? Parameter types,
/// modes (ref/out/own: the calling convention) and ranges, the return type and its range, and the
/// context. `mut` on a parameter is the callee's own business and does not count.
pub fn same_signature(p: &Program, a: FuncId, b: FuncId) -> bool {
    let (fa, fb) = (&p.funcs[a], &p.funcs[b]);
    let ps = |f: &Func| -> Vec<(Ty, LocalKind, Option<(f64, f64)>)> { f.params.iter().map(|&l| (f.locals[l].ty.clone(), f.locals[l].kind, f.locals[l].range)).collect() };
    ps(fa) == ps(fb) && fa.ret == fb.ret && fa.ret_range == fb.ret_range && fa.ctx == fb.ctx
}

/// an implementation of an interface method (or a redeclaration of it) has the same signature and
/// is a property exactly when the interface's is (the two are called differently)
pub fn same_iface_signature(p: &Program, a: FuncId, b: FuncId) -> bool {
    same_signature(p, a, b) && p.funcs[a].is_property == p.funcs[b].is_property
}

/// `IContainer<int>` as written
fn type_text(t: &ast::TypeExpr) -> String {
    let mut s = if t.name == "[]" { format!("{}[]", t.args.first().map(type_text).unwrap_or_default()) } else { t.name.clone() };
    if t.name != "[]" && !t.args.is_empty() {
        s.push('<');
        s.push_str(&t.args.iter().map(type_text).collect::<Vec<_>>().join(", "));
        s.push('>');
    }
    if t.nullable {
        s.push('?');
    }
    s
}

/// a range type's contract on a value: saturate it into [lo, hi]
pub fn saturate(e: Expr, lo: f64, hi: f64) -> Expr {
    let (ty, sp) = (e.ty.clone(), e.span);
    let lit = |v: f64| Expr::new(ExprKind::Float(v), Ty::Float, sp);
    Expr::new(ExprKind::Builtin { f: Builtin::Saturate, args: vec![e, lit(lo), lit(hi)], site: 0 }, ty, sp)
}

/// "list", "array" or "dictionary"
fn coll_word(t: &Ty) -> &'static str {
    match t {
        Ty::Array(_) | Ty::MdArray(..) => "array",
        Ty::Dict(..) => "dictionary",
        _ => "list",
    }
}

/// Is `e` a mutable place? Returns a human-readable reason if not.
/// why a value that is no storage at all cannot be written (a literal, an operator's result, a call)
const NOT_A_PLACE: &str = "expression is not a variable, field or list element";

pub fn place_check_locals(p: &Program, locals: &[Local], e: &Expr) -> Result<(), String> {
    match &e.kind {
        ExprKind::Local(l) => {
            let loc = &locals[*l];
            match loc.kind {
                LocalKind::Normal if loc.is_const => Err(format!("'{}' is a constant: its value is fixed", loc.name)),
                LocalKind::Normal if loc.is_mut => Ok(()),
                LocalKind::Normal => Err(format!("'{}' is immutable: declare it 'mut' to change it (mut {} {} = ...)", loc.name, p.ty_name(&loc.ty), loc.name)),
                LocalKind::Param(ParamMode::Ref) | LocalKind::Param(ParamMode::Out) => Ok(()),
                LocalKind::Param(ParamMode::Own) if loc.is_mut => Ok(()),
                LocalKind::Param(ParamMode::Own) => Err(format!("'{}' is immutable: declare the parameter 'mut own' to change the collection it owns", loc.name)),
                LocalKind::Param(ParamMode::Value) => match loc.ty {
                    Ty::List(_) | Ty::Dict(..) | Ty::Array(_) | Ty::MdArray(..) => Err(format!(
                        "parameter '{}' is a read-only borrow; take it 'mut own' or 'ref' to change it",
                        loc.name
                    )),
                    _ if loc.is_mut => Ok(()),
                    _ => Err(format!("parameter '{}' is immutable: declare it 'mut' to change its copy", loc.name)),
                },
                LocalKind::Cursor(_) => Err(format!("foreach variable '{}' cannot be reassigned", loc.name)),
                LocalKind::ListElem => Err(format!("foreach variable '{}' is a read-only copy of a list element", loc.name)),
            }
        }
        ExprKind::StructSelf => Ok(()),
        ExprKind::Field { base, .. } => match &base.kind {
            // fields of struct values held in entity fields / list elements / locals
            ExprKind::EField { .. } | ExprKind::ListIndex { .. } | ExprKind::Local(_) | ExprKind::StructSelf | ExprKind::Field { .. } => {
                place_check_locals(p, locals, base)
            }
            _ => Err("cannot assign to a field of a temporary value".into()),
        },
        ExprKind::EField { base, field } => {
            let fd = &p.fields[*field];
            if !fd.is_mut {
                return Err(format!(
                    "field '{}' is immutable: it keeps the value it was spawned with; declare it 'mut' to change it (mut {} {})",
                    p.field_label(*field),
                    p.ty_name(&fd.ty),
                    fd.name
                ));
            }
            if p.fields[*field].ty.is_list() && !matches!(base.kind, ExprKind::SelfRef) {
                return Err("lists and dictionaries owned by another entity cannot be modified directly; call a method on that entity".into());
            }
            match &base.kind {
                ExprKind::SelfRef => Ok(()),
                _ if base.ty.is_handle() => Ok(()), // remote write -> deferred command
                _ => Err("invalid entity field target".into()),
            }
        }
        ExprKind::ListIndex { list, .. } => place_check_locals(p, locals, list),
        ExprKind::Const(_) => Err("constants cannot be modified".into()),
        _ => Err(NOT_A_PLACE.into()),
    }
}

/// coroutine lowering: hoisted locals become self fields (`Let` -> assignment)
fn rewrite_hoisted(b: &mut Block, map: &HashMap<LocalId, FieldId>, me: &Ty, fields: &[EField]) {
    if map.is_empty() {
        return;
    }
    let field_of = |l: LocalId, sp: Span| {
        map.get(&l).map(|&f| Expr::new(ExprKind::EField { base: Box::new(Expr::new(ExprKind::SelfRef, me.clone(), sp)), field: f }, fields[f].ty.clone(), sp))
    };
    for s in b.stmts.iter_mut() {
        if let Stmt::Let { local, init } = s {
            if let Some(target) = field_of(*local, Span::default()) {
                let ty = target.ty.clone();
                // `T x;` assigns nothing (definite assignment already checked the reads)
                *s = match init.take() {
                    Some(value) => Stmt::Expr(Expr::new(ExprKind::Assign { op: None, target: Box::new(target), value: Box::new(value) }, ty, Span::default())),
                    None => Stmt::Block(Block::default()),
                };
            }
        }
    }
    crate::opt::visit_block_mut(b, &mut |e| {
        if let ExprKind::Local(l) = e.kind {
            if let Some(x) = field_of(l, e.span) {
                *e = x;
            }
        }
    });
    for s in b.stmts.iter_mut() {
        for x in stmt_blocks_mut(s) {
            rewrite_hoisted(x, map, me, fields);
        }
    }
}

fn stmt_blocks_mut(s: &mut Stmt) -> Vec<&mut Block> {
    match s {
        Stmt::If { then, els, .. } => {
            let mut v = vec![then];
            v.extend(els.iter_mut());
            v
        }
        Stmt::Loop { body, .. } | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => vec![body],
        Stmt::Switch { sections, default, .. } => sections.iter_mut().map(|(_, b)| b).chain(default.iter_mut()).collect(),
        _ => vec![],
    }
}

fn fill_await_frames(b: &mut Block, state: &Expr, timer: &Expr) {
    for s in b.stmts.iter_mut() {
        if let Stmt::Await { state: st, timer: tm, .. } = s {
            *st = state.clone();
            *tm = timer.clone();
        }
        for x in stmt_blocks_mut(s) {
            fill_await_frames(x, state, timer);
        }
    }
}

/// a set of integers as sorted disjoint closed intervals (pattern exhaustiveness analysis)
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ISet(Vec<(i64, i64)>);

impl ISet {
    pub fn range(lo: i64, hi: i64) -> ISet {
        if lo > hi {
            ISet(vec![])
        } else {
            ISet(vec![(lo, hi)])
        }
    }
    pub fn point(v: i64) -> ISet {
        ISet(vec![(v, v)])
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn contains(&self, v: i64) -> bool {
        self.0.iter().any(|&(a, b)| a <= v && v <= b)
    }
    pub fn first(&self) -> Option<i64> {
        self.0.first().map(|r| r.0)
    }
    pub fn union(&self, o: &ISet) -> ISet {
        let mut v: Vec<(i64, i64)> = self.0.iter().chain(o.0.iter()).copied().collect();
        v.sort();
        let mut out: Vec<(i64, i64)> = vec![];
        for (a, b) in v {
            match out.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        ISet(out)
    }
    pub fn intersect(&self, o: &ISet) -> ISet {
        let mut out = vec![];
        for &(a, b) in &self.0 {
            for &(c, d) in &o.0 {
                let (lo, hi) = (a.max(c), b.min(d));
                if lo <= hi {
                    out.push((lo, hi));
                }
            }
        }
        ISet(out).union(&ISet(vec![]))
    }
    pub fn minus(&self, o: &ISet) -> ISet {
        let mut cur = self.0.clone();
        for &(c, d) in &o.0 {
            let mut next = vec![];
            for (a, b) in cur {
                if d < a || c > b {
                    next.push((a, b));
                    continue;
                }
                if a < c {
                    next.push((a, c - 1));
                }
                if d < b {
                    next.push((d + 1, b));
                }
            }
            cur = next;
        }
        ISet(cur)
    }
    pub fn subset_of(&self, o: &ISet) -> bool {
        self.minus(o).is_empty()
    }
}

/// is the end of the block unreachable? (C#: a switch section must not fall through)
pub fn ends_flow(b: &Block) -> bool {
    match b.stmts.last() {
        None => false,
        Some(s) => match s {
            Stmt::Break | Stmt::Continue | Stmt::Return(..) | Stmt::Fail(_) => true,
            Stmt::Block(b) => ends_flow(b),
            // `fail` continues after the block
            Stmt::Atomic { body, .. } => ends_flow(body) && !fails_to_end(body),
            Stmt::If { then, els: Some(e), .. } => ends_flow(then) && ends_flow(e),
            Stmt::Loop { cond, body, .. } => {
                let infinite = matches!(cond, None | Some(Expr { kind: ExprKind::Bool(true), .. }));
                infinite && !contains_break(body)
            }
            Stmt::Switch { sections, default: Some(d), .. } => {
                always_returns(d) && sections.iter().all(|(_, b)| always_returns(b))
            }
            _ => false,
        },
    }
}

pub fn always_returns(b: &Block) -> bool {
    b.stmts.last().map(stmt_returns).unwrap_or(false)
}

/// does a `fail` of this atomic block's body (not of a block nested in it) jump to its end?
pub fn fails_to_end(b: &Block) -> bool {
    b.stmts.iter().any(|s| match s {
        Stmt::Fail(_) => true,
        Stmt::Atomic { .. } => false,
        other => other.sub_blocks().into_iter().any(fails_to_end),
    })
}

fn stmt_returns(s: &Stmt) -> bool {
    match s {
        Stmt::Return(..) => true,
        Stmt::Block(b) => always_returns(b),
        Stmt::Atomic { body, .. } => always_returns(body) && !fails_to_end(body),
        Stmt::If { then, els: Some(e), .. } => always_returns(then) && always_returns(e),
        Stmt::Switch { sections, default: Some(d), .. } => always_returns(d) && sections.iter().all(|(_, b)| always_returns(b)),
        Stmt::Loop { cond, body, .. } => {
            let infinite = match cond {
                None => true,
                Some(Expr { kind: ExprKind::Bool(true), .. }) => true,
                _ => false,
            };
            infinite && !contains_break(body)
        }
        _ => false,
    }
}

/// C# standard numeric format strings supported in `{x:...}` holes (the runtime formats exactly
/// like .NET's invariant culture: sijo_fmt_i32 / sijo_fmt_f32)
/// The index of a field of `root` whose struct type leads back to `root` through struct-valued
/// fields (`path` gets the structs in between), or None.
fn struct_cycle(p: &Program, root: StructId, cur: StructId, path: &mut Vec<StructId>) -> Option<usize> {
    fn reaches(p: &Program, root: StructId, cur: StructId, seen: &mut Vec<bool>, path: &mut Vec<StructId>) -> bool {
        if cur == root {
            return true;
        }
        if seen[cur] {
            return false;
        }
        seen[cur] = true;
        path.push(cur);
        for f in &p.structs[cur].fields {
            if let Ty::Struct(t) = f.ty {
                if reaches(p, root, t, seen, path) {
                    return true;
                }
            }
        }
        path.pop();
        false
    }
    let mut seen = vec![false; p.structs.len()];
    for (i, f) in p.structs[cur].fields.iter().enumerate() {
        if let Ty::Struct(t) = f.ty {
            path.clear();
            if reaches(p, root, t, &mut seen, path) {
                return Some(i);
            }
        }
    }
    None
}

fn check_format_spec(f: &str, ty: &Ty) -> Result<(), String> {
    let mut cs = f.chars();
    let k = cs.next().unwrap_or(' ');
    let digits: String = cs.collect();
    if !digits.is_empty() && (digits.len() > 2 || !digits.chars().all(|c| c.is_ascii_digit())) {
        return Err(format!("'{}' is not a supported format: the precision must be a number from 0 to 99", f));
    }
    let ok = match ty {
        Ty::Int => matches!(k, 'D' | 'd' | 'X' | 'x' | 'F' | 'f' | 'N' | 'n' | 'E' | 'e' | 'G' | 'g'),
        Ty::Float => matches!(k, 'F' | 'f' | 'N' | 'n' | 'E' | 'e' | 'G' | 'g' | 'R' | 'r'),
        Ty::Error => true,
        _ => false,
    };
    if !ok {
        return Err(match ty {
            Ty::Int | Ty::Float => format!("'{}' is not a supported format for a value of this type", f),
            _ => "format strings apply to int and float values only".to_string(),
        });
    }
    if matches!(k, 'G' | 'g' | 'R' | 'r') && !digits.is_empty() {
        return Err(format!("'{}': precision for G/R is not supported (use F, N or E)", f));
    }
    Ok(())
}

fn contains_break(b: &Block) -> bool {
    b.stmts.iter().any(|s| match s {
        Stmt::Break => true,
        Stmt::If { then, els, .. } => contains_break(then) || els.as_ref().map(contains_break).unwrap_or(false),
        Stmt::Block(b) | Stmt::Atomic { body: b, .. } => contains_break(b),
        _ => false,
    })
}

// ---------------------------------------------------------------------------
/// Whether a parameter type mentions one of the type parameters.
fn mentions_tparam(t: &ast::TypeExpr, names: &[String]) -> bool {
    names.contains(&t.name) || t.args.iter().any(|a| mentions_tparam(a, names))
}

// generic visitors
// ---------------------------------------------------------------------------

pub fn visit_block(b: &Block, f: &mut dyn FnMut(&Expr)) {
    for s in &b.stmts {
        visit_stmt(s, f);
    }
}

pub fn visit_stmt(s: &Stmt, f: &mut dyn FnMut(&Expr)) {
    match s {
        Stmt::Let { init, .. } => {
            if let Some(e) = init {
                visit_expr(e, f)
            }
        }
        Stmt::Expr(e) => visit_expr(e, f),
        Stmt::If { cond, then, els } => {
            visit_expr(cond, f);
            visit_block(then, f);
            if let Some(e) = els {
                visit_block(e, f);
            }
        }
        Stmt::Loop { cond, step, body, .. } => {
            if let Some(c) = cond {
                visit_expr(c, f);
            }
            for e in step {
                visit_expr(e, f);
            }
            visit_block(body, f);
        }
        Stmt::ForQuery { body, .. } => visit_block(body, f),
        Stmt::ForList { list, body, .. } => {
            visit_expr(list, f);
            visit_block(body, f);
        }
        Stmt::Return(Some(e), _) => visit_expr(e, f),
        Stmt::Block(b) | Stmt::Atomic { body: b, .. } => visit_block(b, f),
        Stmt::Switch { scrut, sections, default, .. } => {
            visit_expr(scrut, f);
            for (c, b) in sections {
                visit_expr(c, f);
                visit_block(b, f);
            }
            if let Some(d) = default {
                visit_block(d, f);
            }
        }
        Stmt::Await { .. } => {
            for e in s.await_exprs() {
                visit_expr(e, f);
            }
        }
        _ => {}
    }
}

/// A fixed-length array field (`int[K.N] F`) keeps its length: it cannot be moved into an `own`
/// parameter (that leaves it empty) or passed by `ref`/`out` (the callee could store another array).
pub fn check_fixed_len_args(p: &Program, d: &mut Diags) {
    for f in &p.funcs {
        visit_block(&f.body, &mut |e| {
            if let ExprKind::Call { args, .. } = &e.kind {
                for a in args {
                    if let (ParamMode::Own | ParamMode::Ref | ParamMode::Out, ExprKind::EField { field, .. }) = (a.mode, &a.expr.kind) {
                        if let Some(n) = p.fields[*field].fixed_len {
                            d.error_note(
                                "E0241",
                                a.expr.span,
                                format!("'{}' has a fixed length ({} elements): it cannot be passed as '{}'", p.field_label(*field), n, match a.mode { ParamMode::Own => "own", ParamMode::Ref => "ref", _ => "out" }),
                                "pass it as a plain (borrowed) argument and change its elements, or pass a copy: F.Clone()",
                            );
                        }
                    }
                }
            }
        });
    }
}

pub fn visit_expr(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    for c in children(e) {
        visit_expr(c, f);
    }
}

pub fn children(e: &Expr) -> Vec<&Expr> {
    match &e.kind {
        ExprKind::Interp(parts) => parts
            .iter()
            .filter_map(|p| match p {
                IPart::Expr(x, _) => Some(x),
                _ => None,
            })
            .collect(),
        ExprKind::Field { base, .. } | ExprKind::EField { base, .. } => vec![base],
        ExprKind::Unary(_, x) | ExprKind::Cast(x) | ExprKind::ListLen(x) => vec![x],
        ExprKind::Binary(_, a, b) => vec![a, b],
        ExprKind::Assign { target, value, .. } => vec![target, value],
        ExprKind::IncDec { target, .. } => vec![target],
        ExprKind::Cond(a, b, c) => vec![a, b, c],
        ExprKind::Call { recv, args, .. } => {
            let mut v: Vec<&Expr> = recv.iter().map(|r| &**r).collect();
            v.extend(args.iter().map(|a| &a.expr));
            v
        }
        ExprKind::OpCall { args, .. } | ExprKind::Builtin { args, .. } => args.iter().collect(),
        ExprKind::StructLit { args, inits, .. } => {
            let mut v: Vec<&Expr> = args.iter().map(|a| &a.expr).collect();
            v.extend(inits.iter().map(|(_, e)| e));
            v
        }
        ExprKind::Spawn { inits, .. } => inits.iter().map(|(_, e)| e).collect(),
        ExprKind::Destroy(Some(x)) => vec![x],
        ExprKind::CompAdd { target, inits, .. } => std::iter::once(&**target).chain(inits.iter().map(|(_, e)| e)).collect(),
        ExprKind::CompRemove { target, .. } => vec![target],
        ExprKind::Is { expr, .. } => vec![expr],
        ExprKind::ListClone { list, .. } => vec![list],
        ExprKind::ArrayNew { len, elems, dims, .. } => len.iter().map(|l| &**l).chain(dims.iter()).chain(elems.iter()).collect(),
        ExprKind::ListLambda { list, body, args, .. } => {
            let mut v: Vec<&Expr> = vec![list];
            v.extend(args.iter());
            v.push(body);
            v
        }
        ExprKind::ListIndex { list, index, .. } => vec![list, index],
        ExprKind::MdIndex { idx, .. } => idx.iter().collect(),
        ExprKind::ListCall { list, args, .. } => {
            let mut v: Vec<&Expr> = vec![list];
            v.extend(args.iter());
            v
        }
        ExprKind::Switch { scrut, arms, default, .. } => {
            let mut v: Vec<&Expr> = vec![scrut];
            for (c, x) in arms {
                v.push(c);
                v.push(x);
            }
            if let Some(d) = default {
                v.push(d);
            }
            v
        }
        ExprKind::DictGet { dict, key } => vec![dict, key],
        ExprKind::DictSet { dict, key, value } => vec![dict, key, value],
        ExprKind::DictCall { dict, args, .. } => {
            let mut v: Vec<&Expr> = vec![dict];
            v.extend(args.iter());
            v
        }
        ExprKind::Closure { caps, .. } => caps.iter().collect(),
        ExprKind::DgFn(x) => vec![x],
        ExprKind::DgCap { d, .. } => vec![d],
        _ => vec![],
    }
}

/// a list/array method call that takes a lambda (or a LINQ-style form that List has no method for)
fn is_lambda_method(name: &str, args: &[ast::Arg]) -> bool {
    let lam = args.len() == 1 && matches!(args[0].expr.kind, ast::ExprKind::Lambda { .. });
    match name {
        "Sort" => lam,
        "Find" | "FindLast" | "FindIndex" | "FindLastIndex" | "Exists" | "TrueForAll" | "RemoveAll" | "ForEach" | "Where" | "Select" | "OrderBy" | "OrderByDescending" => true,
        "Any" | "All" | "Count" | "Sum" | "Max" | "Min" | "First" | "Last" | "FirstOrDefault" | "LastOrDefault" => args.is_empty() || lam,
        "ToList" | "ToArray" => args.is_empty(),
        _ => false,
    }
}

fn constraint_text(k: &ast::ConstraintKind) -> String {
    match k {
        ast::ConstraintKind::Class => "class".into(),
        ast::ConstraintKind::Struct => "struct".into(),
        ast::ConstraintKind::New => "new()".into(),
        ast::ConstraintKind::NotNull => "notnull".into(),
        ast::ConstraintKind::Unmanaged => "unmanaged".into(),
        ast::ConstraintKind::Type(t) => t.name.clone(),
    }
}
