//! Recursive-descent parser for the C#-like syntax.

use crate::ast::*;
use crate::diag::{Diags, Span};
use crate::lexer::{lex, InterpPiece, Tok, Token};

pub fn parse(src: &str, diags: &mut Diags) -> File {
    let toks = lex(src, 0, diags);
    let mut p = Parser { toks, pos: 0, diags, src, depth: 0, in_pattern: false, panic: false, fatal: false, brace: 0, ctx_types: vec![] };
    p.file()
}

struct Parser<'a> {
    toks: Vec<Token>,
    pos: usize,
    diags: &'a mut Diags,
    /// the whole source (token spans index it): a missing ';' at the end of a line is told by the line break
    src: &'a str,
    depth: usize,
    /// parsing a switch pattern: `x => ...` there is an arm, not a lambda
    in_pattern: bool,
    /// a syntax error was reported and the parser has not found its footing again (a ';', '{' or '}'
    /// it expected, or skipping to the end of the statement or member): errors until then are echoes
    panic: bool,
    /// the nesting limit was hit or the end of the file reached after an error: nothing more to report
    fatal: bool,
    /// `{` consumed minus `}` consumed: tells the '}' that closes the enclosing block while skipping
    brace: usize,
    /// `mut`, `own` or `client` read as a type name (`mut x = 3;`): an error unless the file declares it
    ctx_types: Vec<(String, Span)>,
}

const MAX_NESTING: usize = 1500;
/// links of one `if ... else if ...` chain
const MAX_ELSE_IF: usize = 2 * MAX_NESTING;

const MODIFIERS: &[&str] = &[
    "public", "private", "protected", "internal", "static", "readonly", "const", "virtual", "override", "abstract",
    "sealed", "partial", "new", "extern", "unsafe", "async", "client", "own", "mut",
];

/// The attributes Sijo has. They tell the compiler's planners about data and check what a
/// declaration claims; removing one never changes what the server simulates.
/// dimensions of the largest multi-dimensional array (`T[,,]`)
pub const MAX_RANK: usize = 3;

/// the name of an array type of the given rank in TypeExpr: `[]`, `[,]`, `[,,]`
pub fn array_type_name(rank: usize) -> String {
    format!("[{}]", ",".repeat(rank.max(1) - 1))
}

/// the rank of an array TypeExpr name (`[]` 1, `[,]` 2, ...), 0 for other types
pub fn array_type_rank(name: &str) -> usize {
    if name.starts_with('[') && name.ends_with(']') && name[1..name.len() - 1].chars().all(|c| c == ',') {
        name.len() - 1
    } else {
        0
    }
}

const FIELD_ATTRS: &[&str] = &["Precision", "Replicated", "NotReplicated", "Spatial", "Component", "Persistent", "PersistentKey"];
const METHOD_ATTRS: &[&str] = &["Pure", "Recursive"];

const KEYWORDS: &[&str] = &[
    "if", "else", "while", "do", "for", "foreach", "in", "return", "break", "continue", "new", "this", "base", "true",
    "false", "null", "class", "struct", "enum", "interface", "namespace", "using", "atomic", "fail", "is", "ref", "out", "operator",
    "switch", "case", "default",
];

/// C# modifier keywords (Sijo's own `client`, `own`, `mut` and C#'s contextual `async`, `partial` can still be names)
const RESERVED_MODIFIERS: &[&str] = &[
    "public", "private", "protected", "internal", "static", "readonly", "const", "virtual", "override", "abstract", "sealed", "extern", "unsafe",
];

/// why `s` cannot name a declaration (a keyword or a built-in type name)
fn reserved_name(s: &str) -> Option<&'static str> {
    if is_prim_type(s) {
        Some("a type name")
    } else if KEYWORDS.contains(&s) || RESERVED_MODIFIERS.contains(&s) {
        Some("a keyword")
    } else {
        None
    }
}

pub fn is_type_keyword(s: &str) -> bool {
    is_prim_type(s) || s == "List" || s == "Dictionary"
}

fn is_prim_type(s: &str) -> bool {
    matches!(s, "int" | "float" | "bool" | "void" | "string" | "double" | "long" | "uint" | "byte" | "short" | "char")
}

impl<'a> Parser<'a> {
    // ---------- token helpers ----------
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_at(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }
    fn span(&self) -> Span {
        self.toks[self.pos].span
    }
    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            self.toks[0].span
        } else {
            self.toks[self.pos - 1].span
        }
    }
    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
            match t.tok {
                Tok::Punct("{") => self.brace += 1,
                Tok::Punct("}") => self.brace = self.brace.saturating_sub(1),
                _ => {}
            }
        }
        t
    }

    // ---------- errors and recovery ----------
    fn report(&mut self, code: &'static str, sp: Span, msg: String, note: Option<String>, syntax: bool) {
        if self.panic || self.fatal {
            return;
        }
        match note {
            Some(n) => self.diags.error_note(code, sp, msg, n),
            None => self.diags.error(code, sp, msg),
        }
        if syntax {
            self.panic = true;
        }
        if self.at_eof() {
            // everything still open is unclosed now: one report is enough
            self.fatal = true;
        }
    }
    /// an error that leaves the parse on track (a wrong modifier, a misplaced attribute...)
    fn err(&mut self, code: &'static str, sp: Span, msg: impl Into<String>) {
        self.report(code, sp, msg.into(), None, false);
    }
    fn err_note(&mut self, code: &'static str, sp: Span, msg: impl Into<String>, note: impl Into<String>) {
        self.report(code, sp, msg.into(), Some(note.into()), false);
    }
    /// a syntax error: the parser has lost its place until it finds a ';', '{' or '}' again
    fn syntax(&mut self, code: &'static str, sp: Span, msg: impl Into<String>) {
        self.report(code, sp, msg.into(), None, true);
    }
    /// the nesting limit: report once and stop parsing
    fn too_deep(&mut self, sp: Span, msg: &str) {
        if !self.diags.codes().contains(&"E0025") {
            self.fatal = false;
            self.panic = false;
            self.diags.error("E0025", sp, msg);
        }
        self.fatal = true;
        self.pos = self.toks.len() - 1;
    }
    /// is there a line break between the previous token and the current one?
    fn newline_before(&self) -> bool {
        if self.pos == 0 {
            return false;
        }
        let (a, b) = (self.toks[self.pos - 1].span.hi as usize, self.toks[self.pos].span.lo as usize);
        a <= b && b <= self.src.len() && self.src.as_bytes()[a..b].contains(&b'\n')
    }

    /// After a syntax error in a statement or member (`level`: the braces open in the enclosing
    /// block or type body): skip past the next ';', past a `{ ... }` body that starts while
    /// skipping, or up to the '}' that closes the enclosing block. One mistake, one error.
    fn recover(&mut self, level: usize) {
        if !self.panic {
            return;
        }
        let start = self.brace;
        loop {
            match self.peek() {
                Tok::Eof => {
                    self.fatal = true;
                    break;
                }
                Tok::Punct(";") if self.brace <= level => {
                    self.bump();
                    break;
                }
                Tok::Punct("}") if self.brace <= level => break,
                Tok::Punct("}") => {
                    self.bump();
                    if self.brace == level && start == level {
                        break;
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
        self.panic = false;
    }

    /// After a syntax error between type declarations: skip to the next declaration.
    fn recover_top(&mut self, level: usize) {
        if !self.panic {
            return;
        }
        loop {
            let at_level = self.brace == level;
            match self.peek() {
                Tok::Eof => break,
                Tok::Punct("}") if self.brace <= level => break,
                Tok::Punct(";") if at_level => {
                    self.bump();
                    break;
                }
                Tok::Punct("[") if at_level => break,
                Tok::Ident(s) if at_level && (MODIFIERS.contains(&s.as_str()) || matches!(s.as_str(), "class" | "struct" | "enum" | "interface" | "using" | "namespace")) => break,
                Tok::Ident(_) if at_level && self.is_component_decl() => break,
                Tok::Punct("}") => {
                    self.bump();
                    if self.brace == level {
                        break;
                    }
                }
                _ => {
                    self.bump();
                }
            }
        }
        self.panic = false;
    }
    fn is_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(q) if *q == p)
    }
    fn is_punct_at(&self, n: usize, p: &str) -> bool {
        matches!(self.peek_at(n), Tok::Punct(q) if *q == p)
    }
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.peek(), Tok::Ident(s) if s == k)
    }
    fn is_kw_at(&self, n: usize, k: &str) -> bool {
        matches!(self.peek_at(n), Tok::Ident(s) if s == k)
    }
    /// `component Name {` / `component Name :`: `component` is a contextual keyword (a name elsewhere)
    fn is_component_decl(&self) -> bool {
        self.is_kw("component") && matches!(self.peek_at(1), Tok::Ident(_)) && (self.is_punct_at(2, "{") || self.is_punct_at(2, ":") || self.is_punct_at(2, "<"))
    }
    fn eat_punct(&mut self, p: &str) -> bool {
        if self.is_punct(p) {
            self.bump();
            if matches!(p, ";" | "{" | "}") {
                self.panic = false;
            }
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.bump();
            true
        } else {
            false
        }
    }
    fn expect_punct(&mut self, p: &str) -> Span {
        if self.is_punct(p) {
            let sp = self.bump().span;
            // back on track: what follows is checked again
            if matches!(p, ";" | "{" | "}") {
                self.panic = false;
            }
            sp
        } else {
            let sp = self.span();
            let msg = format!("expected '{}', found {}", p, describe(self.peek()));
            // a ';' missing at the end of a line: the next line starts the next statement
            let lost = !(p == ";" && self.newline_before());
            self.report("E0010", sp, msg, None, lost);
            sp
        }
    }
    fn expect_ident(&mut self) -> (String, Span) {
        match self.peek().clone() {
            Tok::Ident(s) => {
                let sp = self.bump().span;
                (s, sp)
            }
            other => {
                let sp = self.span();
                self.syntax("E0011", sp, format!("expected identifier, found {}", describe(&other)));
                ("<error>".into(), sp)
            }
        }
    }
    /// the name a declaration declares: an identifier that is not a keyword or a built-in type
    fn expect_name(&mut self) -> (String, Span) {
        let (name, sp) = self.expect_ident();
        if let Some(what) = reserved_name(&name) {
            self.err("E0011", sp, format!("'{}' is {} and cannot be used as a name", name, what));
        }
        (name, sp)
    }
    fn at_eof(&self) -> bool {
        matches!(self.peek(), Tok::Eof)
    }

    /// Splits a `>>` token into two `>` tokens (for nested generics).
    fn split_shr(&mut self) {
        if self.is_punct(">>") {
            let sp = self.span();
            self.toks[self.pos] = Token { tok: Tok::Punct(">"), span: Span::new(sp.lo as usize, sp.lo as usize + 1) };
            self.toks.insert(self.pos + 1, Token { tok: Tok::Punct(">"), span: Span::new(sp.lo as usize + 1, sp.hi as usize) });
        }
    }

    // ---------- file / declarations ----------
    fn file(&mut self) -> File {
        let mut f = File::default();
        while !self.at_eof() {
            if self.eat_kw("using") {
                self.qualified();
                self.expect_punct(";");
                continue;
            }
            if self.eat_kw("namespace") {
                self.qualified();
                if self.eat_punct("{") {
                    let level = self.brace;
                    while !self.is_punct("}") && !self.at_eof() {
                        if let Some(d) = self.type_decl() {
                            f.items.push(d);
                        }
                        self.recover_top(level);
                    }
                    self.expect_punct("}");
                } else {
                    self.expect_punct(";");
                }
                continue;
            }
            if let Some(d) = self.type_decl() {
                f.items.push(d);
            }
            self.recover_top(0);
        }
        // `mut x = 3;` reads `mut` as a type: say what is missing when no such type is declared
        for (n, sp) in std::mem::take(&mut self.ctx_types) {
            if !f.items.iter().any(|d| d.name == n) {
                let how = match n.as_str() {
                    "mut" => "'mut' is followed by a type and a name: mut var x = 3; or mut int x = 3;",
                    "own" => "'own' is followed by a list, array or dictionary type and a name: own List<int> xs",
                    _ => "'client' is followed by the method's return type: public client void Render()",
                };
                self.err_note("E0018", sp, format!("missing type after '{}'", n), how);
            }
        }
        f
    }

    fn qualified(&mut self) -> String {
        let (mut s, _) = self.expect_ident();
        while self.eat_punct(".") {
            let (n, _) = self.expect_ident();
            s.push('.');
            s.push_str(&n);
        }
        s
    }

    fn attrs(&mut self) -> Vec<Attr> {
        let mut out = Vec::new();
        while self.is_punct("[") {
            self.bump();
            loop {
                let (name, sp) = self.expect_ident();
                let mut args = Vec::new();
                if self.eat_punct("(") {
                    if !self.is_punct(")") {
                        loop {
                            args.push(self.expr());
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                    }
                    self.expect_punct(")");
                }
                out.push(Attr { name, args, span: sp.to(self.prev_span()) });
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct("]");
        }
        out
    }

    /// At `own`, `client` or `mut`: is it the modifier? These are contextual (a type may be called
    /// that): in `X Y;`, `X Y = ...`, `X Y,`, `X Y)`, `X Y(...)` and `X Y => ...` the word is the type
    /// and Y the name; otherwise a type and a name still follow (`own List<int> xs`, `mut int x`).
    fn contextual_mod(&self) -> bool {
        matches!(self.peek_at(1), Tok::Ident(_)) && !matches!(self.peek_at(2), Tok::Punct(";" | "=" | "," | ")" | "(" | "=>"))
    }

    fn mods(&mut self) -> Mods {
        let mut m = Mods::default();
        let mut seen: Vec<&'static str> = vec![];
        loop {
            let kw: &'static str = match self.peek() {
                Tok::Ident(s) => match MODIFIERS.iter().find(|k| **k == s.as_str()) {
                    Some(k) => k,
                    None => break,
                },
                _ => break,
            };
            if matches!(kw, "own" | "client" | "mut") && !self.contextual_mod() {
                break;
            }
            let sp = self.bump().span;
            m.span = Some(match m.span { Some(s0) => s0.to(sp), None => sp });
            if kw == "extern" {
                let name = self.extern_name();
                let esp = sp.to(self.prev_span());
                if !seen.contains(&"extern") {
                    seen.push(kw);
                    m.is_extern = true;
                    m.extern_name = name;
                } else if m.extern_name.is_some() && name.is_some() {
                    self.err_note("E0028", esp, "an extern function has one C name", "write 'extern' once: extern(\"c_name\")");
                } else {
                    self.err("E0028", esp, "duplicate modifier 'extern'");
                    m.extern_name = m.extern_name.take().or(name);
                }
                continue;
            }
            if seen.contains(&kw) {
                self.err("E0028", sp, format!("duplicate modifier '{}'", kw));
                continue;
            }
            if let Some(other) = conflicting_mod(kw, &seen) {
                let why = match (other, kw) {
                    ("virtual", "override") | ("override", "virtual") => "an override is virtual already: remove 'virtual'",
                    ("virtual", "abstract") | ("abstract", "virtual") => "an abstract method is virtual already: remove 'virtual'",
                    _ => "a member has one access level: 'public', 'private' or 'protected'",
                };
                self.err_note("E0028", sp, format!("'{}' and '{}' cannot be combined", other, kw), why);
            }
            seen.push(kw);
            match kw {
                "public" => m.is_public = true,
                "private" => m.is_private = true,
                "protected" => m.is_protected = true,
                "static" => m.is_static = true,
                "const" => m.is_const = true,
                "readonly" => {
                    m.is_readonly = true;
                    self.err_note("E0028", sp, "'readonly' is not part of Sijo", "a constant is 'const' (any type, computed at compile time); fields and variables are already immutable unless declared 'mut'");
                }
                "virtual" => m.is_virtual = true,
                "override" => m.is_override = true,
                "abstract" => m.is_abstract = true,
                "async" => m.is_async = true,
                "client" => m.is_client = true,
                "own" => m.is_own = true,
                "mut" => m.is_mut = true,
                other => {
                    let why = match other {
                        "internal" => "Sijo has 'public', 'private' and 'protected'",
                        "new" => "members cannot hide inherited ones; give the member another name",
                        _ => "it has no meaning in Sijo",
                    };
                    self.err_note("E0028", sp, format!("'{}' is not part of Sijo", other), why);
                }
            }
        }
        m
    }

    /// the optional `("c_name")` after `extern`
    fn extern_name(&mut self) -> Option<(String, Span)> {
        if !self.is_punct("(") {
            return None;
        }
        self.bump();
        let name = match self.peek().clone() {
            Tok::Str(n) => {
                let nsp = self.bump().span;
                Some((n, nsp))
            }
            other => {
                let esp = self.span();
                self.err("E0254", esp, format!("extern takes an optional C function name: extern(\"name\"), found {}", describe(&other)));
                if !self.is_punct(")") {
                    self.bump();
                }
                None
            }
        };
        self.expect_punct(")");
        name
    }

    /// Checks the attributes of one declaration: `what` is "field", "constant", "struct member",
    /// "method", "type", "parameter" or "type parameter".
    fn check_attrs(&mut self, attrs: &[Attr], what: &str) {
        let mut seen: Vec<&str> = vec![];
        for a in attrs {
            // attributes from C# and Unity whose meaning Sijo writes as a modifier or a range type
            let sijo_form = match a.name.as_str() {
                "Client" => Some("write the 'client' modifier: public client void Render()"),
                "Server" => Some("code runs on the server unless it is 'client'; remove [Server]"),
                "Range" => Some("write the range in the type: public mut int in 0..100 Hp; (both ends included)"),
                "DllImport" => Some("write the 'extern' modifier: public static extern(\"c_name\") bool IsKeyDown(EKey key);"),
                _ => None,
            };
            if let Some(how) = sijo_form {
                self.err_note("E0029", a.span, format!("[{}] is not an attribute in Sijo", a.name), how);
                continue;
            }
            let name = a.name.as_str();
            let allowed = match what {
                "field" => FIELD_ATTRS,
                "method" => METHOD_ATTRS,
                _ => &[],
            };
            if !allowed.contains(&name) {
                if FIELD_ATTRS.contains(&name) && what == "constant" {
                    self.err_note("E0029", a.span, format!("[{}] does not apply to a constant", name), "a constant is a compile-time value: it is not stored, replicated or quantized");
                } else if FIELD_ATTRS.contains(&name) && what == "struct member" {
                    self.err_note("E0029", a.span, format!("[{}] does not apply to a struct member", name), "put it on the entity field that holds the struct");
                } else if FIELD_ATTRS.contains(&name) || METHOD_ATTRS.contains(&name) {
                    let on = if FIELD_ATTRS.contains(&name) { "fields" } else { "methods" };
                    self.err("E0029", a.span, format!("[{}] applies to {}, not to a {}", name, on, what));
                } else {
                    self.err_note(
                        "E0029",
                        a.span,
                        format!("unknown attribute [{}]", name),
                        "fields take [Precision(p)], [Replicated], [NotReplicated], [Spatial], [Component(\"Name\")]; methods take [Pure], [Recursive]",
                    );
                }
                continue;
            }
            if seen.contains(&name) {
                self.err("E0029", a.span, format!("duplicate attribute [{}]", name));
                continue;
            }
            if (name == "Replicated" && seen.contains(&"NotReplicated")) || (name == "NotReplicated" && seen.contains(&"Replicated")) {
                self.err("E0029", a.span, "[Replicated] and [NotReplicated] contradict each other: keep one");
            }
            seen.push(name);
            let bad_args = match name {
                "Precision" => match a.args.as_slice() {
                    [e] => !matches!(e.kind, ExprKind::Int(n) if n > 0) && !matches!(e.kind, ExprKind::Float(f, _) if f > 0.0 && f.is_finite()),
                    _ => true,
                },
                "Component" => !matches!(a.args.as_slice(), [Expr { kind: ExprKind::Str(_), .. }]),
                _ => !a.args.is_empty(),
            };
            if bad_args {
                let msg = match name {
                    "Precision" => "[Precision(p)] takes one positive number: the step replicated values are rounded to, e.g. [Precision(0.01)]".to_string(),
                    "Component" => "[Component(\"Name\")] takes one string: the name of the component".to_string(),
                    _ => format!("[{}] takes no arguments", name),
                };
                self.err("E0029", a.span, msg);
            }
        }
    }

    fn type_decl(&mut self) -> Option<TypeDecl> {
        let attrs = self.attrs();
        self.check_attrs(&attrs, "type");
        let mods = self.mods();
        let start = self.span();
        let kind = if self.eat_kw("struct") {
            DeclKind::Struct
        } else if self.eat_kw("class") {
            DeclKind::Class
        } else if self.eat_kw("enum") {
            DeclKind::Enum
        } else if self.eat_kw("interface") {
            DeclKind::Interface
        } else if self.is_component_decl() {
            self.bump();
            DeclKind::Component
        } else if self.is_kw("delegate") && !matches!(self.peek_at(1), Tok::Punct(_)) {
            // `delegate R Name(params);`
            self.bump();
            let ret = self.type_expr();
            let (name, nsp) = self.expect_name();
            if self.is_punct("<") {
                let sp = self.span();
                self.syntax("E0012", sp, "a delegate type cannot be generic: declare it with concrete types, or use Func<...>/Action<...>");
                let _ = self.type_params();
            }
            let params = self.params();
            self.expect_punct(";");
            let span = start.to(self.prev_span());
            let md = MethodDecl { attrs: vec![], mods: Mods::default(), kind: MethodKind::Normal, ret, name: name.clone(), type_params: vec![], constraints: vec![], params, body: None, is_property: false, span, name_span: nsp, expr_body: false };
            return Some(TypeDecl { kind: DeclKind::Delegate, name, type_params: vec![], constraints: vec![], span, name_span: nsp, body_open: span, base: None, ifaces: vec![], attrs, mods, members: vec![Member::Method(md)], variants: vec![] });
        } else {
            let sp = self.span();
            self.syntax("E0012", sp, format!("expected 'struct', 'class', 'interface', 'component', 'delegate' or 'enum', found {}", describe(self.peek())));
            // recovery: skip a token (the caller skips to the next declaration)
            self.bump();
            return None;
        };
        let (name, nsp) = self.expect_name();
        let type_params = if self.is_punct("<") { self.type_params() } else { vec![] };
        let mut base = None;
        let mut ifaces = vec![];
        if self.eat_punct(":") {
            loop {
                let sp0 = self.span();
                let b = self.qualified();
                // `IContainer<int>`, `Pool<int>`: a generic interface or base class with its type arguments
                let mut args = vec![];
                if self.is_punct("<") {
                    self.bump();
                    loop {
                        args.push(self.type_expr());
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                    self.split_shr();
                    self.expect_punct(">");
                }
                let te = TypeExpr { name: b, args, nullable: false, range: None, span: sp0.to(self.prev_span()) };
                if base.is_none() {
                    base = Some(te);
                } else {
                    ifaces.push(te);
                }
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        let constraints = self.where_clauses();
        if !self.is_punct("{") {
            // no body: the caller skips to the next declaration
            self.expect_punct("{");
            return None;
        }
        let body_open = self.expect_punct("{");
        let level = self.brace;
        let mut decl = TypeDecl { kind, name, type_params, constraints, span: start.to(nsp), name_span: nsp, body_open, base, ifaces, attrs, mods, members: vec![], variants: vec![] };
        if kind == DeclKind::Enum {
            let mut next = 0i64;
            while !self.is_punct("}") && !self.at_eof() {
                let (v, vsp) = self.expect_name();
                let mut val = None;
                if self.eat_punct("=") {
                    let neg = self.eat_punct("-");
                    match self.peek().clone() {
                        Tok::Int(n) => {
                            self.bump();
                            val = Some(if neg { -n } else { n });
                        }
                        _ => {
                            let sp = self.span();
                            self.err("E0013", sp, "enum values must be integer literals");
                            self.bump();
                        }
                    }
                }
                let value = val.unwrap_or(next);
                if value > i32::MAX as i64 || value < i32::MIN as i64 {
                    self.err("E0013", vsp, "enum value does not fit in a 32-bit int");
                }
                next = value + 1;
                decl.variants.push((v, Some(value), vsp));
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct("}");
            self.eat_punct(";");
            return Some(decl);
        }
        while !self.is_punct("}") && !self.at_eof() {
            let before = self.pos;
            self.member(&mut decl);
            self.recover(level);
            if self.pos == before {
                self.bump();
            }
        }
        self.expect_punct("}");
        self.eat_punct(";");
        Some(decl)
    }

    fn member(&mut self, decl: &mut TypeDecl) {
        let attrs = self.attrs();
        let mods = self.mods();
        let start = self.span();
        // nested types are not supported
        if self.is_kw("class") || self.is_kw("struct") || self.is_kw("enum") || self.is_kw("interface") || self.is_component_decl() {
            self.err("E0014", start, "nested type declarations are not supported");
            let _ = self.type_decl();
            return;
        }
        // constructor: Name '('
        if let Tok::Ident(n) = self.peek().clone() {
            if n == decl.name && self.is_punct_at(1, "(") {
                let name_span = self.bump().span;
                self.check_attrs(&attrs, "method");
                let params = self.params();
                let expr_body = self.is_punct("=>");
                let body = self.method_body();
                decl.members.push(Member::Method(MethodDecl {
                    attrs,
                    mods,
                    kind: MethodKind::Ctor,
                    ret: TypeExpr::void(start),
                    name: n,
                    type_params: vec![],
                    constraints: vec![],
                    params,
                    body,
                    is_property: false,
                    span: start.to(self.prev_span()),
                    name_span,
                    expr_body,
                }));
                return;
            }
        }
        let ret = self.type_expr();
        // operator
        if self.is_kw("operator") {
            let op_span = self.bump().span;
            let op = match self.peek().clone() {
                Tok::Punct(p) => {
                    self.bump();
                    p.to_string()
                }
                other => {
                    let sp = self.span();
                    self.syntax("E0015", sp, format!("expected operator symbol, found {}", describe(&other)));
                    "?".into()
                }
            };
            self.check_attrs(&attrs, "method");
            let params = self.params();
            let expr_body = self.is_punct("=>");
            let body = self.method_body();
            decl.members.push(Member::Method(MethodDecl {
                attrs,
                mods,
                kind: MethodKind::Operator(op.clone()),
                ret,
                name: format!("operator{}", op),
                type_params: vec![],
                constraints: vec![],
                params,
                body,
                is_property: false,
                span: start.to(self.prev_span()),
                name_span: op_span,
                expr_body,
            }));
            return;
        }
        let (name, nsp) = self.expect_name();
        let type_params = if self.is_punct("<") { self.type_params() } else { vec![] };
        if !type_params.is_empty() && !self.is_punct("(") {
            let sp = self.span();
            self.syntax("E0015", sp, "expected '(' after the type parameters of a generic method");
        }
        if self.is_punct("(") {
            self.check_attrs(&attrs, "method");
            let params = self.params();
            let constraints = self.where_clauses();
            let expr_body = self.is_punct("=>");
            let body = self.method_body();
            decl.members.push(Member::Method(MethodDecl {
                attrs,
                mods,
                kind: MethodKind::Normal,
                ret,
                name,
                type_params,
                constraints,
                params,
                body,
                is_property: false,
                span: start.to(nsp),
                name_span: nsp,
                expr_body,
            }));
            return;
        }
        if self.is_punct("=>") {
            // expression-bodied get-only property
            self.check_attrs(&attrs, "method");
            let body = self.method_body();
            decl.members.push(Member::Method(MethodDecl {
                attrs,
                mods,
                kind: MethodKind::Normal,
                ret,
                name,
                type_params: vec![],
                constraints: vec![],
                params: vec![],
                body,
                is_property: true,
                span: start.to(nsp),
                name_span: nsp,
                expr_body: true,
            }));
            return;
        }
        if self.is_punct("{") {
            if decl.kind == DeclKind::Interface {
                self.err_note("E0016", nsp, "property accessors '{ get; }' are not supported", format!("an interface asks for a value with a method, 'int {}();', or gives a default property, 'int {} => expr;'", name, name));
            } else {
                self.err("E0016", nsp, "property accessors '{ get; set; }' are not supported; use a field or 'T Name => expr;'");
            }
            // skip braces
            let mut depth = 0;
            loop {
                if self.is_punct("{") {
                    depth += 1;
                } else if self.is_punct("}") {
                    depth -= 1;
                    if depth == 0 {
                        self.bump();
                        break;
                    }
                } else if self.at_eof() {
                    break;
                }
                self.bump();
            }
            return;
        }
        // field: one per declaration
        let what = if mods.is_const {
            "constant"
        } else if decl.kind == DeclKind::Struct {
            "struct member"
        } else {
            "field"
        };
        self.check_attrs(&attrs, what);
        let init = if self.eat_punct("=") { Some(self.init_expr(Some(&ret))) } else { None };
        decl.members.push(Member::Field(FieldDecl { attrs: attrs.clone(), mods: mods.clone(), ty: ret.clone(), name, init, span: nsp }));
        while self.is_punct(",") {
            let csp = self.bump().span;
            let (n2, s2) = self.expect_name();
            self.err_note("E0030", csp.to(s2), format!("declare '{}' in its own declaration", n2), "a declaration declares one field or variable: write each on its own line");
            let init = if self.eat_punct("=") { Some(self.init_expr(Some(&ret))) } else { None };
            decl.members.push(Member::Field(FieldDecl { attrs: attrs.clone(), mods: mods.clone(), ty: ret.clone(), name: n2, init, span: s2 }));
        }
        self.expect_punct(";");
    }

    /// `<T, U>` after a method name
    fn type_params(&mut self) -> Vec<(String, Span)> {
        self.expect_punct("<");
        let mut out = vec![];
        loop {
            let attrs = self.attrs();
            self.check_attrs(&attrs, "type parameter");
            if self.is_kw("in") || self.is_kw("out") {
                let sp = self.bump().span;
                self.err_note("E0015", sp, "variance ('in'/'out') on type parameters is not supported", "Sijo's generic interfaces are invariant: IContainer<Orc> and IContainer<Unit> are different interfaces; write the type parameter without 'in'/'out'");
            }
            // (sema explains a type parameter that reuses a type name such as 'int': E0258)
            let (n, sp) = self.expect_ident();
            if !is_prim_type(&n) && reserved_name(&n).is_some() {
                self.err("E0011", sp, format!("'{}' is a keyword and cannot be used as a name", n));
            }
            out.push((n, sp));
            if !self.eat_punct(",") {
                break;
            }
        }
        self.split_shr();
        self.expect_punct(">");
        out
    }

    /// `where T : class, new() where U : Base`: checked against the type arguments of each instance
    /// (the body is checked per instance anyway, so constraints only make the requirement explicit)
    fn where_clauses(&mut self) -> Vec<Constraint> {
        let mut out = vec![];
        while self.is_kw("where") {
            self.bump();
            let (param, span) = self.expect_ident();
            self.expect_punct(":");
            let mut kinds = vec![];
            loop {
                let sp = self.span();
                let kind = if self.eat_kw("class") {
                    self.eat_punct("?");
                    ConstraintKind::Class
                } else if self.eat_kw("struct") {
                    ConstraintKind::Struct
                } else if self.eat_kw("new") {
                    self.expect_punct("(");
                    self.expect_punct(")");
                    ConstraintKind::New
                } else if self.eat_kw("default") {
                    self.err("E0015", sp, "the 'default' constraint only applies to overrides, which cannot be generic in Sijo");
                    ConstraintKind::Class
                } else {
                    let t = self.type_expr();
                    match t.name.as_str() {
                        "notnull" if t.args.is_empty() => ConstraintKind::NotNull,
                        "unmanaged" if t.args.is_empty() => ConstraintKind::Unmanaged,
                        _ => ConstraintKind::Type(t),
                    }
                };
                kinds.push((kind, sp.to(self.prev_span())));
                if !self.eat_punct(",") {
                    break;
                }
            }
            out.push(Constraint { param, span, kinds });
        }
        out
    }

    fn params(&mut self) -> Vec<Param> {
        self.expect_punct("(");
        let mut ps = Vec::new();
        if !self.is_punct(")") {
            loop {
                let attrs = self.attrs();
                self.check_attrs(&attrs, "parameter");
                let start = self.span();
                // `mut`, `ref`/`out`/`own` in any order: `mut own` and `own mut` are the same
                let mut is_mut = false;
                let mut mode = ParamMode::Value;
                let mut mode_kw: Option<&'static str> = None;
                loop {
                    let kw: &'static str = if (self.is_kw("mut") || self.is_kw("own")) && self.contextual_mod() {
                        if self.is_kw("mut") { "mut" } else { "own" }
                    } else if self.is_kw("ref") {
                        "ref"
                    } else if self.is_kw("out") {
                        "out"
                    } else if self.is_kw("in") {
                        "in"
                    } else if self.is_kw("this") {
                        "this"
                    } else {
                        break;
                    };
                    let sp = self.bump().span;
                    match kw {
                        "mut" if is_mut => self.err("E0028", sp, "duplicate modifier 'mut'"),
                        "mut" => is_mut = true,
                        "in" => self.err_note("E0028", sp, "'in' is not a parameter mode in Sijo", "parameters are read-only borrows unless they are 'ref', 'out' or 'own'; remove 'in'"),
                        "this" => self.err("E0028", sp, "extension methods ('this' parameters) are not part of Sijo"),
                        _ => match mode_kw {
                            Some(k) if k == kw => self.err("E0028", sp, format!("duplicate modifier '{}'", kw)),
                            Some(k) => self.err_note("E0028", sp, format!("'{}' and '{}' cannot be combined", k, kw), "a parameter is passed one way: 'ref', 'out' or 'own' (or neither: a read-only borrow)"),
                            None => {
                                mode_kw = Some(kw);
                                mode = match kw {
                                    "ref" => ParamMode::Ref,
                                    "out" => ParamMode::Out,
                                    _ => ParamMode::Own,
                                };
                            }
                        },
                    }
                }
                let ty = self.type_expr();
                let (name, nsp) = self.expect_name();
                if self.is_punct("=") {
                    let sp = self.span();
                    self.err("E0017", sp, "default parameter values are not supported");
                    self.bump();
                    self.expr();
                }
                ps.push(Param { is_mut, mode, ty, name, span: start.to(nsp), name_span: nsp });
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        self.expect_punct(")");
        ps
    }

    fn method_body(&mut self) -> Option<Block> {
        if self.eat_punct("=>") {
            let e = self.expr();
            let sp = e.span;
            self.expect_punct(";");
            return Some(Block { stmts: vec![Stmt::Return { value: Some(e), span: sp }], span: sp });
        }
        if self.eat_punct(";") {
            return None;
        }
        Some(self.block())
    }

    // ---------- types ----------
    fn type_expr(&mut self) -> TypeExpr {
        let start = self.span();
        let name = match self.peek().clone() {
            // `public mut in 0..5 X;`: the range is there, its type is not
            Tok::Ident(s) if s == "in" && self.range_ahead(0) => {
                self.err_note("E0018", start, "missing type before the range", "a range type is 'T in lo..hi', e.g. 'int in 0..5'");
                "<error>".into()
            }
            Tok::Ident(s) if KEYWORDS.contains(&s.as_str()) => {
                self.syntax("E0018", start, format!("expected type, found keyword '{}'", s));
                self.bump();
                "<error>".into()
            }
            Tok::Ident(s) => {
                self.bump();
                let mut s = s;
                while self.is_punct(".") && matches!(self.peek_at(1), Tok::Ident(_)) {
                    self.bump();
                    let (n, _) = self.expect_ident();
                    s.push('.');
                    s.push_str(&n);
                }
                if matches!(s.as_str(), "mut" | "own" | "client") {
                    self.ctx_types.push((s.clone(), start));
                }
                s
            }
            other => {
                self.syntax("E0018", start, format!("expected type, found {}", describe(&other)));
                "<error>".into()
            }
        };
        let mut args = Vec::new();
        if self.is_punct("<") {
            self.bump();
            loop {
                args.push(self.type_expr());
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.split_shr();
            self.expect_punct(">");
        }
        let nullable = self.eat_punct("?");
        let mut t = TypeExpr { name, args, nullable, range: None, span: start.to(self.prev_span()) };
        // T[] (T[][]: an array of arrays), T[,], T[,,]
        let specs = self.rank_specs();
        t = Self::array_of(t, &specs, start, self.prev_span());
        if self.range_ahead(0) {
            t.range = self.range_spec();
            // `int in 0..1 in 0..2`
            while self.range_ahead(0) {
                let sp = self.span();
                let _ = self.range_spec();
                self.err("E0224", sp.to(self.prev_span()), "a type takes one range: 'T in lo..hi'");
            }
            t.span = start.to(self.prev_span());
        }
        t
    }

    /// at `in`: the range of a range type, `in lo..hi`. The malformed forms `in (lo..hi)` and
    /// `in a..b..c` get one error each and are read as far as they go.
    fn range_spec(&mut self) -> Option<RangeSpec> {
        let rsp = self.bump().span;
        let paren = self.span();
        let in_paren = self.eat_punct("(");
        let lo = self.range_bound();
        self.expect_punct("..");
        let hi = self.range_bound();
        if self.is_punct("..") {
            let sp = self.span();
            while self.eat_punct("..") {
                let _ = self.range_bound();
            }
            self.err_note("E0224", sp.to(self.prev_span()), "a range has two bounds", "write 'lo..hi' (both ends included)");
        }
        if in_paren {
            self.expect_punct(")");
            self.err_note("E0224", paren.to(self.prev_span()), "a range is written without parentheses", "write 'T in lo..hi', e.g. 'int in 0..5'");
        }
        let span = rsp.to(self.prev_span());
        let (lo, hi) = (lo?, hi?);
        if let (RangeBound::Num(a), RangeBound::Num(b)) = (&lo, &hi) {
            if a > b {
                self.err("E0224", span, format!("the range {}..{} is empty: write the smaller bound first", fmt_num(*a), fmt_num(*b)));
                return None;
            }
        }
        Some(RangeSpec { lo, hi, span })
    }

    /// `in` + a bound + `..`: a range type `T in lo..hi`, not `foreach (T x in xs)` (a foreach
    /// `in` follows the variable's name, never its type). `in (lo..hi)` too, to report it.
    fn range_ahead(&self, n: usize) -> bool {
        if !self.is_kw_at(n, "in") {
            return false;
        }
        let mut k = n + 1;
        if self.is_punct_at(k, "(") {
            k += 1;
        }
        if self.is_punct_at(k, "-") {
            k += 1;
        }
        match self.peek_at(k) {
            Tok::Int(_) | Tok::Float(..) => true,
            // a constant: Name or Type.NAME, then `..`
            Tok::Ident(_) => {
                k += 1;
                while self.is_punct_at(k, ".") && matches!(self.peek_at(k + 1), Tok::Ident(_)) {
                    k += 2;
                }
                self.is_punct_at(k, "..")
            }
            _ => false,
        }
    }

    /// lookahead over the ranges range_spec reads, from offset `n`: the offset after them
    fn scan_ranges(&self, mut n: usize) -> usize {
        while self.range_ahead(n) {
            n += 1;
            let paren = self.is_punct_at(n, "(");
            if paren {
                n += 1;
            }
            loop {
                if self.is_punct_at(n, "-") {
                    n += 1;
                }
                match self.peek_at(n) {
                    Tok::Int(_) | Tok::Float(..) => n += 1,
                    Tok::Ident(_) => {
                        n += 1;
                        while self.is_punct_at(n, ".") && matches!(self.peek_at(n + 1), Tok::Ident(_)) {
                            n += 2;
                        }
                    }
                    _ => break,
                }
                if !self.is_punct_at(n, "..") {
                    break;
                }
                n += 1;
            }
            if paren && self.is_punct_at(n, ")") {
                n += 1;
            }
        }
        n
    }

    /// one bound of a range: a number literal or a constant, optionally negative
    fn range_bound(&mut self) -> Option<RangeBound> {
        let start = self.span();
        let neg = self.eat_punct("-");
        let b = match self.peek().clone() {
            Tok::Int(n) => RangeBound::Num(if neg { -(n as f64) } else { n as f64 }),
            Tok::Float(f, _) => RangeBound::Num(if neg { -f } else { f }),
            Tok::Ident(_) => {
                let path = self.qualified();
                return Some(RangeBound::Const { neg, path, span: start.to(self.prev_span()) });
            }
            other => {
                let sp = self.span();
                self.syntax("E0224", sp, format!("a range bound is a number or a constant, found {}", describe(&other)));
                return None;
            }
        };
        self.bump();
        Some(b)
    }

    /// Lookahead: if a type starts at token offset `n`, return the offset after it.
    fn scan_type(&self, n: usize) -> Option<usize> {
        let mut pend = 0;
        let r = self.scan_type_p(n, &mut pend)?;
        if pend > 0 { None } else { Some(r) }
    }

    /// `pend`: closing `>` still owed to enclosing type argument lists (a `>>` token closes two)
    fn scan_type_p(&self, mut n: usize, pend: &mut u32) -> Option<usize> {
        match self.peek_at(n) {
            Tok::Ident(s) if !KEYWORDS.contains(&s.as_str()) || is_prim_type(s) => {}
            _ => return None,
        }
        n += 1;
        while self.is_punct_at(n, ".") && matches!(self.peek_at(n + 1), Tok::Ident(_)) {
            n += 2;
        }
        if self.is_punct_at(n, "<") {
            n = self.scan_type_args_p(n, pend)?;
            if *pend > 0 {
                return Some(n);
            }
        }
        if self.is_punct_at(n, "?") {
            n += 1;
        }
        // an array type T[] (or T[,], which type_expr rejects with a clear message)
        while self.is_punct_at(n, "[") {
            let mut m = n + 1;
            while self.is_punct_at(m, ",") {
                m += 1;
            }
            if !self.is_punct_at(m, "]") {
                break;
            }
            n = m + 1;
        }
        Some(self.scan_ranges(n))
    }

    /// the rank specifiers after a type: `[]` 1, `[,]` 2, `[,,]` 3 (more dimensions: one E0019 each)
    fn rank_specs(&mut self) -> Vec<usize> {
        let mut specs = vec![];
        while self.is_punct("[") && (self.is_punct_at(1, "]") || self.is_punct_at(1, ",")) {
            let s0 = self.span();
            self.bump();
            let mut rank = 1;
            while self.eat_punct(",") {
                rank += 1;
            }
            self.expect_punct("]");
            if rank > MAX_RANK {
                self.err_note("E0019", s0.to(self.prev_span()), format!("an array has at most {} dimensions (found {})", MAX_RANK, rank), "use T[,,] or nest arrays: T[][,,]");
                rank = MAX_RANK;
            }
            specs.push(rank);
        }
        specs
    }

    /// `elem` with the rank specifiers `specs` applied as C# reads them: the first is the outermost
    /// array (`int[][,]` is a one-dimensional array of `int[,]`)
    fn array_of(mut t: TypeExpr, specs: &[usize], start: Span, end: Span) -> TypeExpr {
        for &r in specs.iter().rev() {
            t = TypeExpr { name: array_type_name(r), args: vec![t], nullable: false, range: None, span: start.to(end) };
        }
        t
    }

    /// `{ { a, b }, { c, d } }` for an array of rank `rank` (at `{`): the elements in row-major
    /// order and the lengths the braces spell out. Rows of different lengths are an error (E0241).
    fn md_init(&mut self, rank: usize) -> (Vec<Expr>, Vec<usize>) {
        let mut elems = vec![];
        let mut shape: Vec<Option<usize>> = vec![None; rank];
        self.md_rows(rank, 0, &mut elems, &mut shape);
        (elems, shape.into_iter().map(|n| n.unwrap_or(0)).collect())
    }

    fn md_rows(&mut self, rank: usize, depth: usize, elems: &mut Vec<Expr>, shape: &mut Vec<Option<usize>>) {
        let open = self.span();
        if !self.is_punct("{") {
            // an element where a row was expected
            let e = self.expr();
            self.err_note("E0241", e.span, format!("a nested initializer '{{ ... }}' is expected (the array has {} dimensions)", rank), "write each row in braces: { { 1, 2 }, { 3, 4 } }");
            return;
        }
        self.bump();
        let mut n = 0;
        while !self.is_punct("}") && !self.at_eof() {
            if depth + 1 < rank {
                self.md_rows(rank, depth + 1, elems, shape);
            } else if self.is_punct("{") {
                let s = self.span();
                self.skip_group();
                self.err_note("E0241", s.to(self.prev_span()), format!("too many nested initializers: the array has {} dimensions", rank), "write the elements of the innermost rows without braces");
            } else {
                elems.push(self.expr());
            }
            n += 1;
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct("}");
        match shape[depth] {
            None => shape[depth] = Some(n),
            Some(m) if m != n => {
                self.err_note("E0241", open.to(self.prev_span()), format!("an array initializer of length {} is expected (found {})", m, n), "every row of a multi-dimensional array has the same length");
            }
            _ => {}
        }
    }

    /// `{ a, b, c }` (a trailing comma is allowed)
    fn array_elems(&mut self) -> Vec<Expr> {
        self.expect_punct("{");
        let mut v = vec![];
        while !self.is_punct("}") && !self.at_eof() {
            v.push(self.expr());
            if !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct("}");
        v
    }

    /// an initializer: `{ a, b }` is an array literal when the declared type is an array
    fn init_expr(&mut self, ty: Option<&TypeExpr>) -> Expr {
        if let (Some(t), true) = (ty, self.is_punct("{")) {
            let rank = array_type_rank(&t.name);
            if rank == 1 {
                let start = self.span();
                let elems = self.array_elems();
                let shape = vec![elems.len()];
                return Expr { kind: ExprKind::NewArray { elem: t.args[0].clone(), rank, lens: vec![], elems: Some(elems), shape }, span: start.to(self.prev_span()) };
            }
            if rank > 1 {
                let start = self.span();
                let (elems, shape) = self.md_init(rank);
                return Expr { kind: ExprKind::NewArray { elem: t.args[0].clone(), rank, lens: vec![], elems: Some(elems), shape }, span: start.to(self.prev_span()) };
            }
        }
        self.expr()
    }

    /// skips a balanced `(...)`, `[...]` or `{...}` group starting at the current token
    fn skip_group(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.peek() {
                Tok::Punct("(") | Tok::Punct("[") | Tok::Punct("{") => depth += 1,
                Tok::Punct(")") | Tok::Punct("]") | Tok::Punct("}") => depth -= 1,
                Tok::Eof => return,
                _ => {}
            }
            self.bump();
            if depth <= 0 {
                return;
            }
        }
    }

    /// at `(`: is this `( ... ) =>`, a lambda's parameter list?
    fn paren_lambda_ahead(&self) -> bool {
        let mut depth = 0i32;
        let mut n = 0;
        loop {
            match self.peek_at(n) {
                Tok::Punct("(") => depth += 1,
                Tok::Punct(")") => {
                    depth -= 1;
                    if depth == 0 {
                        return self.is_punct_at(n + 1, "=>");
                    }
                }
                Tok::Punct(";") | Tok::Punct("{") | Tok::Punct("}") | Tok::Eof => return false,
                _ => {}
            }
            n += 1;
        }
    }

    /// after a lambda's parameters, at `=>`: the body (an expression or a block)
    fn lambda_rest(&mut self, start: Span, params: Vec<(Option<TypeExpr>, String, Span)>) -> Expr {
        self.expect_punct("=>");
        let body = if self.is_punct("{") { LambdaBody::Block(self.block()) } else { LambdaBody::Expr(self.expr()) };
        Expr { kind: ExprKind::Lambda { params, body: Box::new(body) }, span: start.to(self.prev_span()) }
    }

    fn scan_type_args(&self, n: usize) -> Option<usize> {
        let mut pend = 0;
        let r = self.scan_type_args_p(n, &mut pend)?;
        if pend > 0 { None } else { Some(r) }
    }

    fn scan_type_args_p(&self, mut n: usize, pend: &mut u32) -> Option<usize> {
        // at '<'
        n += 1;
        loop {
            n = self.scan_type_p(n, pend)?;
            if *pend > 0 {
                break;
            }
            if self.is_punct_at(n, ",") {
                n += 1;
                continue;
            }
            break;
        }
        if *pend > 0 {
            // the `>>` that closed the last argument closes this list too
            *pend -= 1;
            Some(n)
        } else if self.is_punct_at(n, ">") {
            Some(n + 1)
        } else if self.is_punct_at(n, ">>") {
            *pend += 1;
            Some(n + 1)
        } else {
            None
        }
    }

    // ---------- statements ----------
    fn block(&mut self) -> Block {
        if !self.is_punct("{") {
            // no body: the caller skips the rest of the member or statement
            let sp = self.expect_punct("{");
            return Block { stmts: vec![], span: sp };
        }
        let start = self.expect_punct("{");
        let level = self.brace;
        let mut stmts = Vec::new();
        while !self.is_punct("}") && !self.at_eof() {
            let before = self.pos;
            self.stmt_into(&mut stmts);
            self.recover(level);
            if self.pos == before {
                self.bump();
            }
        }
        let end = self.expect_punct("}");
        Block { stmts, span: start.to(end) }
    }

    fn stmt(&mut self) -> Stmt {
        let mut v = Vec::new();
        self.stmt_into(&mut v);
        if v.len() == 1 {
            v.pop().unwrap()
        } else {
            let sp = self.prev_span();
            Stmt::Block(Block { stmts: v, span: sp })
        }
    }

    fn is_decl_start(&self) -> bool {
        if (self.is_kw("mut") || self.is_kw("const")) && matches!(self.peek_at(1), Tok::Ident(_)) {
            return true;
        }
        if self.is_kw("var") && matches!(self.peek_at(1), Tok::Ident(_)) {
            return true;
        }
        if let Some(n) = self.scan_type(0) {
            if let Tok::Ident(_) = self.peek_at(n) {
                // (a keyword there is still a declaration: var_decls says it cannot be a name)
                return self.is_punct_at(n + 1, "=") || self.is_punct_at(n + 1, ";") || self.is_punct_at(n + 1, ",");
            }
        }
        false
    }

    fn var_decls(&mut self, out: &mut Vec<Stmt>) {
        let start = self.span();
        // `mut` is contextual: in `mut x = ...` it is a type called `mut`
        let is_mut = self.is_kw("mut") && self.contextual_mod();
        if is_mut {
            self.bump();
        }
        let is_const = self.is_kw("const");
        if is_const {
            let sp = self.bump().span;
            if is_mut {
                self.err("E0028", sp, "a constant cannot be 'mut'");
            }
            if self.is_kw("var") {
                let sp = self.span();
                self.err_note("E0028", sp, "a local constant names its type", "write 'const int N = 5;' (the type of a constant is part of what it promises)");
            }
        }
        let ty = if self.eat_kw("var") { None } else { Some(self.type_expr()) };
        let mut first = true;
        loop {
            let (name, nsp) = self.expect_name();
            if !first {
                self.err_note("E0030", nsp, format!("declare '{}' in its own statement", name), "a declaration declares one field or variable: write each on its own line");
            }
            first = false;
            let init = if self.eat_punct("=") { Some(self.init_expr(ty.as_ref())) } else { None };
            let span = start.to(self.prev_span());
            out.push(Stmt::Var { ty: ty.clone(), name, init, span, name_span: nsp, is_mut: is_mut && !is_const, is_const });
            if !self.eat_punct(",") {
                break;
            }
        }
    }

    /// the body of `if`, `else`, a loop: always a block `{ ... }` (`else if` continues a chain)
    fn body(&mut self, what: &str) -> Stmt {
        if self.is_punct("{") {
            return Stmt::Block(self.block());
        }
        let start = self.span();
        if self.panic {
            // the header was not understood: the enclosing block skips what follows
            return Stmt::Empty(start);
        }
        let s = self.stmt();
        let sp = start.to(self.prev_span());
        let note = if matches!(s, Stmt::Empty(_)) {
            "an empty body is '{ }': a ';' right after the condition ends the statement"
        } else {
            "wrap the body in braces: { ... }"
        };
        self.err_note("E0031", sp, format!("the body of '{}' must be a block", what), note);
        s
    }

    fn stmt_into(&mut self, out: &mut Vec<Stmt>) {
        if self.depth > MAX_NESTING {
            let sp = self.span();
            self.too_deep(sp, "expression or statement nested too deeply");
            return;
        }
        self.depth += 1;
        self.stmt_inner(out);
        self.depth -= 1;
    }

    fn stmt_inner(&mut self, out: &mut Vec<Stmt>) {
        let start = self.span();
        if self.is_punct("{") {
            out.push(Stmt::Block(self.block()));
            return;
        }
        if self.eat_punct(";") {
            out.push(Stmt::Empty(start));
            return;
        }
        if self.eat_kw("if") {
            // an `else if` chain is read in a loop: its links are not nested in the source, so they
            // do not count toward the nesting limit (each link's body still does)
            let mut links: Vec<(Span, Expr, Stmt)> = vec![];
            let mut link_start = start;
            let els = loop {
                self.expect_punct("(");
                let cond = self.expr();
                self.expect_punct(")");
                let then = self.body("if");
                links.push((link_start, cond, then));
                if !self.eat_kw("else") {
                    break None;
                }
                if !self.is_kw("if") {
                    break Some(Box::new(self.body("else")));
                }
                // later passes still recurse once per link (and C output grows with the chain's indentation)
                if links.len() >= MAX_ELSE_IF {
                    let sp = self.span();
                    self.too_deep(sp, "'else if' chain is too long");
                    break None;
                }
                link_start = self.bump().span;
            };
            let end = self.prev_span();
            let mut stmt = None;
            let mut els = els;
            while let Some((sp, cond, then)) = links.pop() {
                let s = Stmt::If { cond, then: Box::new(then), els: stmt.take().map(Box::new).or(els.take()), span: sp.to(end) };
                stmt = Some(s);
            }
            out.extend(stmt);
            return;
        }
        if self.eat_kw("while") {
            self.expect_punct("(");
            let cond = self.expr();
            self.expect_punct(")");
            let body = Box::new(self.body("while"));
            out.push(Stmt::While { cond, body, span: start.to(self.prev_span()) });
            return;
        }
        if self.eat_kw("do") {
            let body = Box::new(self.body("do"));
            if !self.eat_kw("while") {
                let sp = self.span();
                self.syntax("E0020", sp, "expected 'while' after 'do' body");
            }
            self.expect_punct("(");
            let cond = self.expr();
            self.expect_punct(")");
            self.expect_punct(";");
            out.push(Stmt::DoWhile { body, cond, span: start.to(self.prev_span()) });
            return;
        }
        if self.eat_kw("for") {
            self.expect_punct("(");
            let mut init = Vec::new();
            if !self.is_punct(";") {
                if self.is_decl_start() || self.is_kw("var") {
                    self.var_decls(&mut init);
                } else {
                    loop {
                        let e = self.expr();
                        let sp = e.span;
                        init.push(Stmt::Expr(e, sp));
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                }
            }
            self.expect_punct(";");
            let cond = if self.is_punct(";") { None } else { Some(self.expr()) };
            self.expect_punct(";");
            let mut step = Vec::new();
            if !self.is_punct(")") {
                loop {
                    step.push(self.expr());
                    if !self.eat_punct(",") {
                        break;
                    }
                }
            }
            self.expect_punct(")");
            let body = Box::new(self.body("for"));
            out.push(Stmt::For { init, cond, step, body, span: start.to(self.prev_span()) });
            return;
        }
        if self.eat_kw("foreach") {
            self.expect_punct("(");
            let mut more = vec![];
            let (name, name_span) = if self.is_kw("var") && self.is_punct_at(1, "(") {
                // foreach (var (s, b) in Query<Ship, Burning>()): one name per type argument
                self.bump();
                self.expect_punct("(");
                let first = self.expect_name();
                while self.eat_punct(",") {
                    more.push(self.expect_name());
                }
                self.expect_punct(")");
                first
            } else {
                if !self.eat_kw("var") {
                    let t = self.type_expr();
                    if let Some(r) = &t.range {
                        self.err_note("E0224", r.span, "a foreach variable reads the elements as they are; it takes no range", "copy the element into a ranged local inside the loop");
                    }
                }
                self.expect_name()
            };
            if !self.eat_kw("in") {
                let sp = self.span();
                self.syntax("E0021", sp, "expected 'in' in foreach");
            }
            let iter = self.expr();
            self.expect_punct(")");
            let body = Box::new(self.body("foreach"));
            out.push(Stmt::Foreach { name, name_span, more, iter, body, span: start.to(self.prev_span()) });
            return;
        }
        if self.eat_kw("return") {
            let value = if self.is_punct(";") { None } else { Some(self.expr()) };
            self.expect_punct(";");
            out.push(Stmt::Return { value, span: start.to(self.prev_span()) });
            return;
        }
        if self.eat_kw("break") {
            let end = self.expect_punct(";");
            out.push(Stmt::Break(start.to(end)));
            return;
        }
        if self.eat_kw("continue") {
            let end = self.expect_punct(";");
            out.push(Stmt::Continue(start.to(end)));
            return;
        }
        if self.eat_kw("switch") {
            self.expect_punct("(");
            let scrut = self.expr();
            self.expect_punct(")");
            let mut sections: Vec<SwitchSection> = Vec::new();
            if !self.is_punct("{") {
                self.expect_punct("{");
                out.push(Stmt::Switch { scrut, sections, span: start.to(self.prev_span()) });
                return;
            }
            self.expect_punct("{");
            let level = self.brace;
            while !self.is_punct("}") && !self.at_eof() {
                let sec_start = self.span();
                let mut labels = Vec::new();
                loop {
                    let lsp = self.span();
                    if self.is_kw("default") && self.is_punct_at(1, ":") {
                        self.bump();
                        self.bump();
                        labels.push(CaseLabel::Default(lsp));
                    } else if self.eat_kw("case") {
                        let pat = self.pattern();
                        let guard = if self.is_kw("when") {
                            self.bump();
                            Some(self.expr())
                        } else {
                            None
                        };
                        self.expect_punct(":");
                        labels.push(CaseLabel::Case(pat, guard, lsp.to(self.prev_span())));
                    } else {
                        break;
                    }
                }
                if labels.is_empty() {
                    let sp = self.span();
                    self.syntax("E0026", sp, "expected 'case' or 'default:' in switch");
                    self.bump();
                    self.recover(level);
                    continue;
                }
                let mut body = Vec::new();
                while !self.is_punct("}") && !self.is_kw("case") && !(self.is_kw("default") && self.is_punct_at(1, ":")) && !self.at_eof() {
                    let before = self.pos;
                    self.stmt_into(&mut body);
                    self.recover(level);
                    if self.pos == before {
                        self.bump();
                    }
                }
                sections.push(SwitchSection { labels, body, span: sec_start.to(self.prev_span()) });
            }
            self.expect_punct("}");
            out.push(Stmt::Switch { scrut, sections, span: start.to(self.prev_span()) });
            return;
        }
        if self.is_kw("atomic") && self.is_punct_at(1, "{") {
            self.bump();
            let body = self.block();
            out.push(Stmt::Atomic { body, span: start.to(self.prev_span()) });
            return;
        }
        if self.is_kw("fail") && self.is_punct_at(1, ";") {
            self.bump();
            let end = self.bump().span;
            out.push(Stmt::Fail(start.to(end)));
            return;
        }
        if self.is_decl_start() {
            let first = out.len();
            self.var_decls(out);
            let end = self.expect_punct(";");
            for s in &mut out[first..] {
                if let Stmt::Var { span, .. } = s {
                    *span = span.to(end);
                }
            }
            return;
        }
        let e = self.expr();
        let end = self.expect_punct(";");
        let sp = e.span.to(end);
        out.push(Stmt::Expr(e, sp));
    }

    // ---------- expressions ----------
    pub fn expr(&mut self) -> Expr {
        if self.depth > MAX_NESTING {
            let sp = self.span();
            self.too_deep(sp, "expression or statement nested too deeply");
            return Expr { kind: ExprKind::Int(0), span: sp };
        }
        self.depth += 1;
        let r = self.expr_inner();
        self.depth -= 1;
        r
    }

    fn expr_inner(&mut self) -> Expr {
        let lhs = self.conditional();
        let op = match self.peek() {
            Tok::Punct("=") => Some(None),
            Tok::Punct("+=") => Some(Some(BinOp::Add)),
            Tok::Punct("-=") => Some(Some(BinOp::Sub)),
            Tok::Punct("*=") => Some(Some(BinOp::Mul)),
            Tok::Punct("/=") => Some(Some(BinOp::Div)),
            Tok::Punct("%=") => Some(Some(BinOp::Rem)),
            Tok::Punct("&=") => Some(Some(BinOp::BitAnd)),
            Tok::Punct("|=") => Some(Some(BinOp::BitOr)),
            Tok::Punct("^=") => Some(Some(BinOp::BitXor)),
            Tok::Punct("<<=") => Some(Some(BinOp::Shl)),
            Tok::Punct(">>=") => Some(Some(BinOp::Shr)),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let rhs = self.expr();
            let span = lhs.span.to(rhs.span);
            return Expr { kind: ExprKind::Assign(op, Box::new(lhs), Box::new(rhs)), span };
        }
        lhs
    }

    fn conditional(&mut self) -> Expr {
        let c = self.binary(0);
        if self.is_punct("?") {
            self.bump();
            let a = self.expr();
            self.expect_punct(":");
            let b = self.expr();
            let span = c.span.to(b.span);
            return Expr { kind: ExprKind::Cond(Box::new(c), Box::new(a), Box::new(b)), span };
        }
        c
    }

    fn binop_info(&self) -> Option<(BinOp, u8)> {
        let t = match self.peek() {
            Tok::Punct(p) => *p,
            _ => return None,
        };
        Some(match t {
            "||" => (BinOp::Or, 1),
            "&&" => (BinOp::And, 2),
            "|" => (BinOp::BitOr, 3),
            "^" => (BinOp::BitXor, 4),
            "&" => (BinOp::BitAnd, 5),
            "==" => (BinOp::Eq, 6),
            "!=" => (BinOp::Ne, 6),
            "<" => (BinOp::Lt, 7),
            ">" => (BinOp::Gt, 7),
            "<=" => (BinOp::Le, 7),
            ">=" => (BinOp::Ge, 7),
            "<<" => (BinOp::Shl, 8),
            ">>" => (BinOp::Shr, 8),
            "+" => (BinOp::Add, 9),
            "-" => (BinOp::Sub, 9),
            "*" => (BinOp::Mul, 10),
            "/" => (BinOp::Div, 10),
            "%" => (BinOp::Rem, 10),
            _ => return None,
        })
    }

    fn binary(&mut self, min_prec: u8) -> Expr {
        let mut chain = 0usize;
        let mut lhs = self.unary_sw();
        loop {
            // `is` pattern binds at relational precedence (7)
            if self.is_kw("is") && 7 >= min_prec {
                self.bump();
                if self.eat_kw("null") {
                    let sp = lhs.span.to(self.prev_span());
                    let null = Expr { kind: ExprKind::Null, span: self.prev_span() };
                    lhs = Expr { kind: ExprKind::Binary(BinOp::Eq, Box::new(lhs), Box::new(null)), span: sp };
                    continue;
                }
                if self.is_kw("not") && self.is_kw_at(1, "null") {
                    self.bump();
                    self.bump();
                    let sp = lhs.span.to(self.prev_span());
                    let null = Expr { kind: ExprKind::Null, span: self.prev_span() };
                    lhs = Expr { kind: ExprKind::Binary(BinOp::Ne, Box::new(lhs), Box::new(null)), span: sp };
                    continue;
                }
                if self.is_pattern_start() {
                    let pat = self.pattern();
                    let sp = lhs.span.to(self.prev_span());
                    lhs = Expr { kind: ExprKind::IsPattern(Box::new(lhs), Box::new(pat)), span: sp };
                    continue;
                }
                let ty = self.type_expr();
                if self.is_kw("or") || self.is_kw("and") {
                    // `x is ETeam.Red or ETeam.Blue`: the "type" was a constant
                    let first = Pattern::Const(type_to_expr(&ty));
                    let mut pat = first;
                    loop {
                        if self.is_kw("or") {
                            self.bump();
                            let b = self.pattern_and();
                            pat = Pattern::Or(Box::new(pat), Box::new(b));
                        } else if self.is_kw("and") {
                            self.bump();
                            let b = self.pattern_not();
                            pat = Pattern::And(Box::new(pat), Box::new(b));
                        } else {
                            break;
                        }
                    }
                    let sp = lhs.span.to(self.prev_span());
                    lhs = Expr { kind: ExprKind::IsPattern(Box::new(lhs), Box::new(pat)), span: sp };
                    continue;
                }
                let bind = match self.peek().clone() {
                    Tok::Ident(n) if !KEYWORDS.contains(&n.as_str()) => {
                        let sp = self.bump().span;
                        Some((n, sp))
                    }
                    _ => None,
                };
                let sp = lhs.span.to(self.prev_span());
                lhs = Expr { kind: ExprKind::Is(Box::new(lhs), ty, bind), span: sp };
                continue;
            }
            let (op, prec) = match self.binop_info() {
                Some(x) => x,
                None => break,
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            let rhs = self.binary(prec + 1);
            let span = lhs.span.to(rhs.span);
            lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), span };
            chain += 1;
            if chain > 20 * MAX_NESTING {
                self.too_deep(span, "expression is too long");
                break;
            }
        }
        lhs
    }

    // ---------- patterns ----------
    fn pattern(&mut self) -> Pattern {
        let mut a = self.pattern_and();
        while self.is_kw("or") {
            self.bump();
            let b = self.pattern_and();
            a = Pattern::Or(Box::new(a), Box::new(b));
        }
        a
    }
    fn pattern_and(&mut self) -> Pattern {
        let mut a = self.pattern_not();
        while self.is_kw("and") {
            self.bump();
            let b = self.pattern_not();
            a = Pattern::And(Box::new(a), Box::new(b));
        }
        a
    }
    fn pattern_not(&mut self) -> Pattern {
        let start = self.span();
        if self.is_kw("not") {
            self.bump();
            let p = self.pattern_not();
            let sp = start.to(p.span());
            return Pattern::Not(Box::new(p), sp);
        }
        if self.is_punct("(") {
            self.bump();
            let p = self.pattern();
            self.expect_punct(")");
            return p;
        }
        if self.is_kw("_") {
            self.bump();
            return Pattern::Discard(start);
        }
        let rel = match self.peek() {
            Tok::Punct("<") => Some(BinOp::Lt),
            Tok::Punct("<=") => Some(BinOp::Le),
            Tok::Punct(">") => Some(BinOp::Gt),
            Tok::Punct(">=") => Some(BinOp::Ge),
            _ => None,
        };
        let outer = self.in_pattern;
        self.in_pattern = true;
        let p = if let Some(op) = rel {
            self.bump();
            let e = self.binary(8);
            let sp = start.to(e.span);
            Pattern::Rel(op, e, sp)
        } else {
            Pattern::Const(self.binary(3))
        };
        self.in_pattern = outer;
        p
    }
    /// does the token after `is` start a (non-type) pattern?
    fn is_pattern_start(&self) -> bool {
        match self.peek() {
            Tok::Punct("<") | Tok::Punct("<=") | Tok::Punct(">") | Tok::Punct(">=") | Tok::Punct("(") | Tok::Punct("-") => true,
            Tok::Int(_) | Tok::Float(..) | Tok::Str(_) | Tok::Char(_) => true,
            Tok::Ident(s) => matches!(s.as_str(), "not" | "_" | "true" | "false"),
            _ => false,
        }
    }

    /// unary operand, optionally followed by `switch { ... }` (C# switch expression)
    fn unary_sw(&mut self) -> Expr {
        let mut e = self.unary();
        while self.is_kw("switch") && self.is_punct_at(1, "{") {
            self.bump();
            self.bump();
            let mut arms = Vec::new();
            while !self.is_punct("}") && !self.at_eof() {
                let astart = self.span();
                let pat = self.pattern();
                let guard = if self.is_kw("when") {
                    self.bump();
                    let outer = self.in_pattern;
                    self.in_pattern = true; // `when b => ...`: the arrow belongs to the arm
                    let g = self.expr();
                    self.in_pattern = outer;
                    Some(g)
                } else {
                    None
                };
                self.expect_punct("=>");
                let value = self.expr();
                arms.push(SwitchArm { pat, guard, value, span: astart.to(self.prev_span()) });
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.expect_punct("}");
            let span = e.span.to(self.prev_span());
            e = Expr { kind: ExprKind::Switch(Box::new(e), arms), span };
        }
        e
    }

    fn unary(&mut self) -> Expr {
        let start = self.span();
        let op = match self.peek() {
            Tok::Punct("-") => Some(UnOp::Neg),
            Tok::Punct("!") => Some(UnOp::Not),
            Tok::Punct("~") => Some(UnOp::BitNot),
            Tok::Punct("+") => Some(UnOp::Plus),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            self.depth += 1;
            let e = if self.depth > MAX_NESTING { self.expr() } else { self.unary() };
            self.depth -= 1;
            let span = start.to(e.span);
            // fold negative literals immediately so i32::MIN etc. parse naturally
            if op == UnOp::Neg {
                match e.kind {
                    ExprKind::Int(n) => return Expr { kind: ExprKind::Int(-n), span },
                    ExprKind::Float(f, s) => return Expr { kind: ExprKind::Float(-f, s), span },
                    _ => {}
                }
            }
            return Expr { kind: ExprKind::Unary(op, Box::new(e)), span };
        }
        if self.is_kw("await") && (matches!(self.peek_at(1), Tok::Ident(_)) || self.is_punct_at(1, "(")) {
            self.bump();
            let e = self.unary();
            let span = start.to(e.span);
            return Expr { kind: ExprKind::Await(Box::new(e)), span };
        }
        if self.is_punct("++") || self.is_punct("--") {
            let inc = self.is_punct("++");
            self.bump();
            let e = self.unary();
            let span = start.to(e.span);
            return Expr { kind: ExprKind::IncDec(Box::new(e), inc, true), span };
        }
        // cast
        if self.is_punct("(") {
            if let Some(n) = self.scan_type(1) {
                if self.is_punct_at(n, ")") {
                    let is_kw_type = matches!(self.peek_at(1), Tok::Ident(s) if is_prim_type(s));
                    let follows = match self.peek_at(n + 1) {
                        Tok::Ident(s) => !matches!(s.as_str(), "is" | "in"),
                        Tok::Int(_) | Tok::Float(..) | Tok::Str(_) | Tok::Char(_) | Tok::Interp(_) => true,
                        Tok::Punct("(") | Tok::Punct("!") | Tok::Punct("~") => true,
                        Tok::Punct("-") | Tok::Punct("+") => is_kw_type,
                        _ => false,
                    };
                    if follows {
                        self.bump();
                        let ty = self.type_expr();
                        self.expect_punct(")");
                        let e = self.unary();
                        let span = start.to(e.span);
                        return Expr { kind: ExprKind::Cast(ty, Box::new(e)), span };
                    }
                }
            }
        }
        self.postfix()
    }

    fn args(&mut self) -> Vec<Arg> {
        self.expect_punct("(");
        let mut out = Vec::new();
        if !self.is_punct(")") {
            loop {
                let mode = if self.eat_kw("ref") {
                    ParamMode::Ref
                } else if self.eat_kw("out") {
                    ParamMode::Out
                } else if self.is_kw("in") {
                    let sp = self.bump().span;
                    self.err_note("E0028", sp, "'in' is not an argument mode in Sijo", "arguments are borrowed unless they are 'ref' or 'out'; remove 'in'");
                    ParamMode::Value
                } else {
                    ParamMode::Value
                };
                let expr = if mode == ParamMode::Out && self.is_out_decl() { self.out_decl() } else { self.expr() };
                out.push(Arg { mode, expr });
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        self.expect_punct(")");
        out
    }

    /// after `out`: `var x`, `T x` or `_` followed by `,` or `)`
    fn is_out_decl(&self) -> bool {
        let ends = |s: &Self, n: usize| s.is_punct_at(n, ",") || s.is_punct_at(n, ")");
        if matches!(self.peek_at(0), Tok::Ident(n) if n == "_") && ends(self, 1) {
            return true;
        }
        if self.is_kw("var") && matches!(self.peek_at(1), Tok::Ident(_)) && ends(self, 2) {
            return true;
        }
        if let Some(n) = self.scan_type(0) {
            if let Tok::Ident(name) = self.peek_at(n) {
                return !KEYWORDS.contains(&name.as_str()) && ends(self, n + 1);
            }
        }
        false
    }

    fn out_decl(&mut self) -> Expr {
        let start = self.span();
        if matches!(self.peek_at(0), Tok::Ident(n) if n == "_") {
            self.bump();
            return Expr { kind: ExprKind::OutDecl(None, "_".into(), start), span: start };
        }
        let ty = if self.eat_kw("var") { None } else { Some(self.type_expr()) };
        let (name, nsp) = self.expect_name();
        Expr { kind: ExprKind::OutDecl(ty, name, nsp), span: start.to(self.prev_span()) }
    }

    fn generic_args_then_call(&mut self) -> Option<Vec<TypeExpr>> {
        if self.is_punct("<") {
            if let Some(n) = self.scan_type_args(0) {
                if self.is_punct_at(n, "(") {
                    self.bump();
                    let mut tys = Vec::new();
                    loop {
                        tys.push(self.type_expr());
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                    self.split_shr();
                    self.expect_punct(">");
                    return Some(tys);
                }
            }
        }
        None
    }

    fn postfix(&mut self) -> Expr {
        let mut e = self.primary();
        loop {
            if self.is_punct(".") {
                self.bump();
                let (name, nsp) = self.expect_ident();
                let span = e.span.to(nsp);
                e = Expr { kind: ExprKind::Member(Box::new(e), name, nsp), span };
                if let Some(tys) = self.generic_args_then_call() {
                    let args = self.args();
                    let span = e.span.to(self.prev_span());
                    e = Expr { kind: ExprKind::Call { callee: Box::new(e), type_args: tys, args }, span };
                }
                continue;
            }
            if self.is_punct("(") {
                let args = self.args();
                let span = e.span.to(self.prev_span());
                e = Expr { kind: ExprKind::Call { callee: Box::new(e), type_args: vec![], args }, span };
                continue;
            }
            if self.is_punct("[") {
                self.bump();
                let mut idx = vec![self.expr()];
                while self.eat_punct(",") {
                    idx.push(self.expr());
                }
                let end = self.expect_punct("]");
                let span = e.span.to(end);
                e = Expr { kind: ExprKind::Index(Box::new(e), idx), span };
                continue;
            }
            if self.is_punct("++") || self.is_punct("--") {
                let inc = self.is_punct("++");
                let end = self.bump().span;
                let span = e.span.to(end);
                e = Expr { kind: ExprKind::IncDec(Box::new(e), inc, false), span };
                continue;
            }
            break;
        }
        e
    }

    fn primary(&mut self) -> Expr {
        let start = self.span();
        match self.peek().clone() {
            Tok::Int(n) => {
                self.bump();
                Expr { kind: ExprKind::Int(n), span: start }
            }
            Tok::Float(f, s) => {
                self.bump();
                Expr { kind: ExprKind::Float(f, s), span: start }
            }
            Tok::Str(s) => {
                self.bump();
                Expr { kind: ExprKind::Str(s), span: start }
            }
            Tok::Char(c) => {
                self.bump();
                Expr { kind: ExprKind::Char(c), span: start }
            }
            Tok::Interp(pieces) => {
                self.bump();
                let mut parts = Vec::new();
                for p in pieces {
                    match p {
                        InterpPiece::Text(t) => parts.push(InterpPart::Text(t)),
                        InterpPiece::Expr { src, offset, fmt } => {
                            let toks = lex(&src, offset, self.diags);
                            let mut sub = Parser { toks, pos: 0, diags: &mut *self.diags, src: self.src, depth: self.depth, in_pattern: false, panic: self.panic, fatal: self.fatal, brace: 0, ctx_types: vec![] };
                            let e = sub.expr();
                            if !sub.at_eof() {
                                let sp = sub.span();
                                sub.syntax("E0022", sp, "unexpected tokens in interpolation hole");
                            }
                            parts.push(InterpPart::Expr(e, fmt));
                        }
                    }
                }
                Expr { kind: ExprKind::Interp(parts), span: start }
            }
            Tok::Punct("(") => {
                if !self.in_pattern && self.paren_lambda_ahead() {
                    // (a, b) => ...  /  (int a, int b) => ...  /  () => ...
                    self.bump();
                    let mut params = vec![];
                    while !self.is_punct(")") && !self.at_eof() {
                        let ty = match self.scan_type(0) {
                            Some(n) if matches!(self.peek_at(n), Tok::Ident(_)) => Some(self.type_expr()),
                            _ => None,
                        };
                        let (n, nsp) = self.expect_name();
                        params.push((ty, n, nsp));
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                    self.expect_punct(")");
                    return self.lambda_rest(start, params);
                }
                self.bump();
                let e = self.expr();
                self.expect_punct(")");
                Expr { kind: e.kind, span: start.to(self.prev_span()) }
            }
            Tok::Ident(n) if self.is_punct_at(1, "=>") && !self.in_pattern => {
                self.bump();
                self.lambda_rest(start, vec![(None, n, start)])
            }
            Tok::Ident(s) => match s.as_str() {
                "true" => {
                    self.bump();
                    Expr { kind: ExprKind::Bool(true), span: start }
                }
                "false" => {
                    self.bump();
                    Expr { kind: ExprKind::Bool(false), span: start }
                }
                "null" => {
                    self.bump();
                    Expr { kind: ExprKind::Null, span: start }
                }
                "default" => {
                    self.bump();
                    let ty = if self.eat_punct("(") {
                        let t = self.type_expr();
                        self.expect_punct(")");
                        Some(t)
                    } else {
                        None
                    };
                    Expr { kind: ExprKind::Default(ty), span: start.to(self.prev_span()) }
                }
                "this" => {
                    self.bump();
                    Expr { kind: ExprKind::This, span: start }
                }
                "base" => {
                    self.bump();
                    Expr { kind: ExprKind::Base, span: start }
                }
                "new" => {
                    self.bump();
                    if let Some(n) = self.scan_type(0) {
                        // `new int[4]`, `new int[4][]`, `new int[w, h]` (scan_type stops before `[4]`), or
                        // `new int[] { 1, 2 }`, `new int[,] { { 1 }, { 2 } }`
                        let sized = self.is_punct_at(n, "[") && !self.is_punct_at(n + 1, "]") && !self.is_punct_at(n + 1, ",");
                        let listed = !sized && n >= 2 && self.is_punct_at(n - 1, "]") && self.is_punct_at(n, "{") && {
                            let mut m = n - 2;
                            while m > 0 && self.is_punct_at(m, ",") {
                                m -= 1;
                            }
                            self.is_punct_at(m, "[")
                        };
                        if sized || listed {
                            let mut elem = self.type_expr();
                            let mut lens = vec![];
                            let rank;
                            if sized {
                                let s0 = self.span();
                                self.expect_punct("[");
                                loop {
                                    lens.push(self.expr());
                                    if !self.eat_punct(",") {
                                        break;
                                    }
                                }
                                self.expect_punct("]");
                                if lens.len() > MAX_RANK {
                                    self.err_note("E0019", s0.to(self.prev_span()), format!("an array has at most {} dimensions (found {})", MAX_RANK, lens.len()), "use T[,,] or nest arrays: T[][,,]");
                                    lens.truncate(MAX_RANK);
                                }
                                rank = lens.len();
                                // new int[3][]: an array of 3 (empty) arrays; new int[3][,]: of int[,]
                                let specs = self.rank_specs();
                                let es = elem.span;
                                elem = Self::array_of(elem, &specs, es, self.prev_span());
                            } else {
                                // `new T[] {` / `new T[,] {`: type_expr took the brackets; the element type is inside
                                rank = array_type_rank(&elem.name).max(1);
                                if array_type_rank(&elem.name) > 0 {
                                    elem = elem.args.remove(0);
                                }
                            }
                            let (elems, shape) = if !self.is_punct("{") {
                                (None, vec![])
                            } else if rank == 1 {
                                let es = self.array_elems();
                                let shape = vec![es.len()];
                                (Some(es), shape)
                            } else {
                                let (es, shape) = self.md_init(rank);
                                (Some(es), shape)
                            };
                            return Expr { kind: ExprKind::NewArray { elem, rank, lens, elems, shape }, span: start.to(self.prev_span()) };
                        }
                    }
                    let ty = self.type_expr();
                    let args = if self.is_punct("(") { Some(self.args()) } else { None };
                    let coll = matches!(ty.name.as_str(), "List" | "Sijo.List" | "Dictionary" | "Sijo.Dictionary");
                    if coll && self.eat_punct("{") {
                        let mut items = vec![];
                        while !self.is_punct("}") && !self.at_eof() {
                            if self.eat_punct("{") {
                                let mut xs = vec![];
                                while !self.is_punct("}") && !self.at_eof() {
                                    xs.push(self.expr());
                                    if !self.eat_punct(",") {
                                        break;
                                    }
                                }
                                self.expect_punct("}");
                                items.push((xs, false));
                            } else if self.eat_punct("[") {
                                let k = self.expr();
                                self.expect_punct("]");
                                self.expect_punct("=");
                                let v = self.expr();
                                items.push((vec![k, v], true));
                            } else {
                                items.push((vec![self.expr()], false));
                            }
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                        self.expect_punct("}");
                        return Expr { kind: ExprKind::NewColl { ty, args, items }, span: start.to(self.prev_span()) };
                    }
                    let mut inits = Vec::new();
                    if self.eat_punct("{") {
                        while !self.is_punct("}") && !self.at_eof() {
                            let (n, nsp) = self.expect_ident();
                            self.expect_punct("=");
                            let v = self.expr();
                            inits.push((n, v, nsp));
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                        self.expect_punct("}");
                    } else if args.is_none() {
                        let sp = self.span();
                        self.syntax("E0023", sp, "expected '(' or '{' after 'new T'");
                    }
                    Expr { kind: ExprKind::New { ty, args, inits }, span: start.to(self.prev_span()) }
                }
                _ => {
                    self.bump();
                    let e = Expr { kind: ExprKind::Name(s.clone()), span: start };
                    if let Some(tys) = self.generic_args_then_call() {
                        let args = self.args();
                        let span = start.to(self.prev_span());
                        return Expr { kind: ExprKind::Call { callee: Box::new(e), type_args: tys, args }, span };
                    }
                    // `Pair<int>.Of(1)`
                    if self.is_punct("<") && self.scan_type_args(0).map(|n| self.is_punct_at(n, ".")).unwrap_or(false) {
                        self.bump();
                        let mut args = vec![];
                        loop {
                            args.push(self.type_expr());
                            if !self.eat_punct(",") {
                                break;
                            }
                        }
                        self.split_shr();
                        self.expect_punct(">");
                        let span = start.to(self.prev_span());
                        return Expr { kind: ExprKind::GenericName(TypeExpr { name: s, args, nullable: false, range: None, span }), span };
                    }
                    e
                }
            },
            other => {
                self.syntax("E0024", start, format!("expected expression, found {}", describe(&other)));
                // a token that ends the statement or a group stays for the caller
                if !matches!(other, Tok::Punct(";" | "}" | ")" | "]" | ",") | Tok::Eof) {
                    self.bump();
                }
                Expr { kind: ExprKind::Int(0), span: start }
            }
        }
    }
}

/// `A.B.C` parsed as a type -> the equivalent member-access expression
pub fn type_to_expr(t: &TypeExpr) -> Expr {
    let mut parts = t.name.split('.');
    let first = parts.next().unwrap_or("").to_string();
    let mut e = Expr { kind: ExprKind::Name(first), span: t.span };
    for p in parts {
        e = Expr { kind: ExprKind::Member(Box::new(e), p.to_string(), t.span), span: t.span };
    }
    e
}

/// a range bound as written: `100`, `-0.5`
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{}", v) }
}

/// a modifier already written that `kw` contradicts
fn conflicting_mod(kw: &str, seen: &[&'static str]) -> Option<&'static str> {
    const ACCESS: &[&str] = &["public", "private", "protected"];
    const PAIRS: &[(&str, &str)] = &[("virtual", "override"), ("virtual", "abstract")];
    seen.iter().copied().find(|&s| (ACCESS.contains(&kw) && ACCESS.contains(&s)) || PAIRS.iter().any(|&(a, b)| (kw == a && s == b) || (kw == b && s == a)))
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Ident(s) => format!("'{}'", s),
        Tok::Int(n) => format!("'{}'", n),
        Tok::Float(f, _) => format!("'{}'", f),
        Tok::Str(_) => "string literal".into(),
        Tok::Char(_) => "character literal".into(),
        Tok::Interp(_) => "interpolated string".into(),
        Tok::Punct(p) => format!("'{}'", p),
        Tok::Eof => "end of file".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> File {
        let mut d = Diags::default();
        let f = parse(src, &mut d);
        assert!(!d.has_errors(), "{:?}", d.list);
        f
    }

    #[test]
    fn parses_struct_and_class() {
        let f = ok(r#"
            using Sijo;
            namespace Game;
            public struct Vec2 {
                public float X;
                public float Y;
                public Vec2(float x, float y) { X = x; Y = y; }
                public static Vec2 operator +(Vec2 a, Vec2 b) => new Vec2(a.X + b.X, a.Y + b.Y);
                public float LenSq => X * X + Y * Y;
            }
            public class Ship : Entity {
                public mut int in 0..100 Hp = 100;
                public Ship? Target;
                public void Update(float dt) {
                    var n = Count<Ship>();
                    foreach (var s in Query<Ship>()) { if (s != this && s.Hp < 3) { s.Hp -= 1; } }
                    if (Target is Ship t) { t.Hp += (int)(dt * 2); }
                    for (int i = 0; i < n; i++) { Hp = i > 2 ? 1 : 2; }
                    mut List<int> xs = new List<int>();
                    Print($"hp={Hp} n={n:F2}");
                    atomic { Hp -= 1; if (Hp < 0) { fail; } }
                }
            }
            enum Team { Red, Blue = 4, Green }
        "#);
        assert_eq!(f.items.len(), 3);
        assert_eq!(f.items[0].members.len(), 5);
        let hp = match &f.items[1].members[0] { Member::Field(x) => x, _ => panic!("field") };
        assert!(hp.mods.is_mut);
        assert_eq!(hp.ty.range.as_ref().map(|r| (r.lo.clone(), r.hi.clone())), Some((RangeBound::Num(0.0), RangeBound::Num(100.0))));
        assert_eq!(f.items[2].variants[2].1, Some(5));
    }

    #[test]
    fn generic_vs_compare() {
        let f = ok("class A { void F() { bool b = x < y; int c = Count<A>(); b = a < b && c > d; } }");
        assert_eq!(f.items.len(), 1);
    }

    #[test]
    fn reports_errors() {
        let mut d = Diags::default();
        parse("class A { void F() { int x = ; } }", &mut d);
        assert!(d.has_errors());
    }
}
