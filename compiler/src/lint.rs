//! `sijoc lint`: style, naming and correctness rules.
//!
//! Rule sources:
//!  * the Sijo coding standard (naming prefixes/suffixes, braces, `var`, member order ...; docs/10-tooling.md)
//!  * .NET analyzers (IDE0011, IDE0051, IDE0059/0060, IDE1006, CS0162, CS0642, CS1717 ...)
//!  * clang-tidy (bugprone-integer-division, bugprone-branch-clone, readability-* ...)
//!  * Sijo game rules that need the compiler's whole-program plan (bandwidth, parallelism, races)
//!
//! Configuration: a `.sijolint` file (found next to the source or in a parent directory),
//! inline `// sijo-lint: disable[-next-line|-line] L101 L202` comments, and presets.

use crate::ast::{self, BinOp, DeclKind, MethodKind, Stmt as AStmt};
use crate::diag::{line_col, Level, Span};
use crate::driver::Compiled;
use crate::hir::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Standard,
    Dotnet,
    /// the Sijo coding standard for game code: single letters are fine for numbers and vectors
    /// (x, y, r, t, n, i, v: coordinates, radii, times, counts, cell indexes); the rest as Standard
    Game,
}

pub struct RuleInfo {
    pub code: &'static str,
    pub name: &'static str,
    pub standard: Option<Level>,
    pub dotnet: Option<Level>,
    pub source: &'static str,
    pub desc: &'static str,
}

const W: Option<Level> = Some(Level::Warning);
const H: Option<Level> = Some(Level::Hint);
const OFF: Option<Level> = None;

pub const RULES: &[RuleInfo] = &[
    // ---- naming
    RuleInfo { code: "L101", name: "pascal-case-types", standard: W, dotnet: W, source: "Sijo style · IDE1006", desc: "types, enum members and public members use PascalCase" },
    RuleInfo { code: "L102", name: "struct-prefix-s", standard: W, dotnet: OFF, source: "Sijo style", desc: "struct names start with S (SVector2)" },
    RuleInfo { code: "L103", name: "enum-prefix-e", standard: W, dotnet: OFF, source: "Sijo style", desc: "enum names start with E (EDirection)" },
    RuleInfo { code: "L104", name: "camel-case-locals", standard: W, dotnet: W, source: "Sijo style · IDE1006", desc: "local variables and parameters use camelCase" },
    RuleInfo { code: "L105", name: "method-case", standard: W, dotnet: W, source: "Sijo style · IDE1006", desc: "public methods PascalCase; non-public methods camelCase (standard) / PascalCase (.NET)" },
    RuleInfo { code: "L106", name: "constant-case", standard: W, dotnet: W, source: "Sijo style · IDE1006", desc: "constants (const) use ALL_CAPS (standard) or PascalCase (.NET)" },
    RuleInfo { code: "L107", name: "bool-prefix-b", standard: W, dotnet: OFF, source: "Sijo style", desc: "bool variables start with b; private bool members with mb" },
    RuleInfo { code: "L108", name: "bool-method-prefix", standard: W, dotnet: OFF, source: "Sijo style", desc: "bool-returning methods/properties start with Is/Can/Has/Should" },
    RuleInfo { code: "L109", name: "private-field-prefix", standard: W, dotnet: W, source: "Sijo style · .NET _camelCase", desc: "private fields: mPascalCase (standard) or _camelCase (.NET)" },
    RuleInfo { code: "L112", name: "acronym-case", standard: H, dotnet: H, source: "Sijo style", desc: "acronyms are all caps only when no word follows (HudScale, TargetID)" },
    RuleInfo { code: "L113", name: "descriptive-names", standard: H, dotnet: OFF, source: "Sijo style", desc: "single-letter names only for trivial loop indices" },
    RuleInfo { code: "L114", name: "interface-prefix-i", standard: W, dotnet: W, source: "Sijo style · .NET naming", desc: "interface names start with I (IDamageable)" },
    // ---- style
    RuleInfo { code: "L202", name: "explicit-type", standard: W, dotnet: OFF, source: "Sijo style · IDE0008", desc: "use an explicit type instead of var unless the type is obvious (new/Spawn/cast)" },
    RuleInfo { code: "L204", name: "brace-on-own-line", standard: W, dotnet: W, source: "Sijo style · csharp_new_line_before_open_brace", desc: "opening braces go on their own line (Allman style)" },
    RuleInfo { code: "L205", name: "member-order", standard: W, dotnet: OFF, source: "Sijo style · SA1201", desc: "fields, properties, constructors, then methods (public before private)" },
    // ---- correctness (C# / C++ default linters)
    RuleInfo { code: "L301", name: "unused-local", standard: W, dotnet: W, source: "CS0168 · CS0219 · IDE0059 · clang -Wunused-variable", desc: "local variable is never read" },
    RuleInfo { code: "L302", name: "unused-parameter", standard: W, dotnet: W, source: "IDE0060 · misc-unused-parameters", desc: "parameter is never read" },
    RuleInfo { code: "L303", name: "unused-private-method", standard: W, dotnet: W, source: "IDE0051", desc: "non-public method is never called" },
    RuleInfo { code: "L304", name: "unread-field", standard: H, dotnet: H, source: "IDE0052 · sijoc dead-field elimination", desc: "field is written but never read (the compiler removes its storage)" },
    RuleInfo { code: "L305", name: "integer-division", standard: W, dotnet: W, source: "bugprone-integer-division", desc: "integer division whose result is used as a float" },
    RuleInfo { code: "L306", name: "float-equality", standard: W, dotnet: W, source: "clang -Wfloat-equal · CA2242-like", desc: "exact float comparison with == / !=" },
    RuleInfo { code: "L307", name: "bool-literal-compare", standard: W, dotnet: W, source: "IDE0100 · readability-simplify-boolean-expr", desc: "comparison with true/false" },
    RuleInfo { code: "L309", name: "identical-branches", standard: W, dotnet: W, source: "bugprone-branch-clone", desc: "if and else branches are identical" },
    RuleInfo { code: "L310", name: "self-assignment", standard: W, dotnet: W, source: "CS1717 · clang -Wself-assign", desc: "assignment of a variable to itself" },
    RuleInfo { code: "L311", name: "unreachable-code", standard: W, dotnet: W, source: "CS0162", desc: "statement after return/break/continue/fail" },
    RuleInfo { code: "L312", name: "constant-condition", standard: W, dotnet: W, source: "CA1508 · sijoc constant folding", desc: "condition is always true or always false" },
    RuleInfo { code: "L313", name: "simplify-bool-return", standard: H, dotnet: H, source: "readability-simplify-boolean-expr · IDE0075", desc: "if (c) return true; else return false;" },
    RuleInfo { code: "L316", name: "unused-mut", standard: W, dotnet: W, source: "rustc unused_mut", desc: "variable, parameter or field declared 'mut' never changes" },
    RuleInfo { code: "L314", name: "cognitive-complexity", standard: W, dotnet: W, source: "readability-function-cognitive-complexity · CA1502", desc: "function is too complex (configurable: max-complexity)" },
    // ---- Sijo game rules
    RuleInfo { code: "L401", name: "remote-set-race", standard: W, dotnet: W, source: "sijoc system rule", desc: "plain assignment to another entity's field: last writer wins if several entities write it" },
    RuleInfo { code: "L402", name: "nested-query", standard: H, dotnet: H, source: "sijoc", desc: "per-entity query over all entities is O(N*M) per tick" },
    RuleInfo { code: "L403", name: "print-in-system", standard: W, dotnet: W, source: "sijoc scheduler", desc: "Print in an entity system forces serial execution" },
    RuleInfo { code: "L404", name: "unquantized-replication", standard: W, dotnet: W, source: "sijoc network planner", desc: "replicated float without a range (float in lo..hi) is sent as 32 bits" },
    RuleInfo { code: "L405", name: "client-random", standard: W, dotnet: W, source: "sijoc determinism", desc: "Random in client code diverges from the server" },
    RuleInfo { code: "L406", name: "hazard-double-buffer", standard: H, dotnet: H, source: "sijoc ECS planner", desc: "field read on other entities and written on self: double-buffered (memcpy per tick)" },
    RuleInfo { code: "L407", name: "spawn-in-system", standard: H, dotnet: H, source: "sijoc scheduler", desc: "Spawn whose handle is used keeps an entity system serial" },
    RuleInfo { code: "L408", name: "alloc-in-loop", standard: W, dotnet: W, source: "sijoc memory planner", desc: "new list allocated on every loop iteration" },
    RuleInfo { code: "L409", name: "extern-in-system", standard: W, dotnet: W, source: "sijoc scheduler", desc: "a host function call (extern) keeps an entity system serial" },
];

/// How much a finding matters to the game, for ordering summaries: 3 = game rules (L4xx: what runs
/// in parallel, what is sent, what allocates), 1 = correctness (L3xx), 0 = names and layout.
pub fn importance(code: &str) -> u8 {
    match code.as_bytes().get(1) {
        Some(b'4') => 3,
        Some(b'3') => 1,
        _ => 0,
    }
}

pub fn rule(code: &str) -> Option<&'static RuleInfo> {
    RULES.iter().find(|r| r.code == code || r.name == code)
}

#[derive(Clone, Debug)]
pub struct LintConfig {
    pub preset: Preset,
    pub overrides: HashMap<&'static str, Option<Level>>,
    pub max_complexity: u32,
}

impl Default for LintConfig {
    fn default() -> Self {
        LintConfig { preset: Preset::Standard, overrides: HashMap::new(), max_complexity: 25 }
    }
}

impl LintConfig {
    pub fn level(&self, code: &str) -> Option<Level> {
        let r = rule(code)?;
        if let Some(l) = self.overrides.get(r.code) {
            return *l;
        }
        match self.preset {
            Preset::Standard | Preset::Game => r.standard,
            Preset::Dotnet => r.dotnet,
        }
    }

    pub fn parse(text: &str) -> Result<LintConfig, String> {
        let mut c = LintConfig::default();
        for (i, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                return Err(format!(".sijolint line {}: expected 'key = value'", i + 1));
            };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "preset" => {
                    c.preset = match v {
                        "standard" => Preset::Standard,
                        "dotnet" | "csharp" => Preset::Dotnet,
                        "game" => Preset::Game,
                        _ => return Err(format!(".sijolint line {}: unknown preset '{}' (standard | game | dotnet)", i + 1, v)),
                    }
                }
                "max-complexity" => c.max_complexity = v.parse().map_err(|_| format!(".sijolint line {}: expected a number", i + 1))?,
                _ => {
                    let r = rule(k).ok_or(format!(".sijolint line {}: unknown rule '{}'", i + 1, k))?;
                    let lvl = match v {
                        "off" | "none" => None,
                        "hint" | "info" => Some(Level::Hint),
                        "warning" | "warn" => Some(Level::Warning),
                        "error" => Some(Level::Error),
                        _ => return Err(format!(".sijolint line {}: level must be off | hint | warning | error", i + 1)),
                    };
                    c.overrides.insert(r.code, lvl);
                }
            }
        }
        Ok(c)
    }

    /// Looks for `.sijolint` in the source file's directory and its parents.
    pub fn for_file(path: &Path) -> Result<LintConfig, String> {
        let mut dir = path.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            let f = d.join(".sijolint");
            if f.is_file() {
                // messages name the file that was found
                let shown = f.display().to_string();
                let t = std::fs::read_to_string(&f).map_err(|e| format!("{}: {}", shown, e))?;
                return LintConfig::parse(&t).map_err(|e| e.replacen(".sijolint", &shown, 1));
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
        Ok(LintConfig::default())
    }
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub span: Span,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Fix {
    pub title: String,
    pub edits: Vec<Edit>,
}

#[derive(Clone, Debug)]
pub struct LintDiag {
    pub code: &'static str,
    pub level: Level,
    pub span: Span,
    pub msg: String,
    pub note: Option<String>,
    pub fix: Option<Fix>,
}

impl LintDiag {
    pub fn to_diagnostic(&self) -> crate::diag::Diagnostic {
        let mut notes = vec![];
        if let Some(n) = &self.note {
            notes.push(n.clone());
        }
        if let Some(f) = &self.fix {
            notes.push(format!("fix available: {} (sijoc lint --fix)", f.title));
        }
        if let Some(r) = rule(self.code) {
            notes.push(format!("{} [{}]", r.name, r.source));
        }
        crate::diag::Diagnostic { level: self.level, code: self.code, msg: self.msg.clone(), span: self.span, notes }
    }
}

// ---------------------------------------------------------------------------
// suppression comments
// ---------------------------------------------------------------------------

struct Suppressions {
    file: HashSet<String>,
    lines: HashMap<usize, HashSet<String>>,
}

fn parse_suppressions(src: &str) -> Suppressions {
    let mut s = Suppressions { file: HashSet::new(), lines: HashMap::new() };
    for (i, line) in src.lines().enumerate() {
        let Some(pos) = line.find("sijo-lint:") else { continue };
        let rest = line[pos + "sijo-lint:".len()..].trim();
        let mut parts = rest.split(|c: char| c.is_whitespace() || c == ',').filter(|x| !x.is_empty());
        let Some(cmd) = parts.next() else { continue };
        let mut codes: HashSet<String> = parts.map(|c| rule(c).map(|r| r.code.to_string()).unwrap_or(c.to_string())).collect();
        if codes.is_empty() {
            codes.insert("all".into());
        }
        match cmd {
            "disable" => s.file.extend(codes),
            "disable-next-line" => s.lines.entry(i + 2).or_default().extend(codes),
            "disable-line" => s.lines.entry(i + 1).or_default().extend(codes),
            _ => {}
        }
    }
    s
}

// ---------------------------------------------------------------------------
// naming helpers
// ---------------------------------------------------------------------------

pub fn words(name: &str) -> Vec<String> {
    let mut out = vec![];
    for part in name.split('_').filter(|p| !p.is_empty()) {
        let cs: Vec<char> = part.chars().collect();
        let mut cur = String::new();
        for i in 0..cs.len() {
            let c = cs[i];
            if i > 0 && c.is_ascii_uppercase() {
                let prev = cs[i - 1];
                let next_lower = cs.get(i + 1).map(|n| n.is_ascii_lowercase()).unwrap_or(false);
                if prev.is_ascii_lowercase() || prev.is_ascii_digit() || (prev.is_ascii_uppercase() && next_lower) {
                    out.push(std::mem::take(&mut cur));
                }
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn cap(w: &str) -> String {
    if w.len() <= 2 && w.chars().all(|c| c.is_ascii_uppercase()) {
        return w.to_string(); // ID, UI
    }
    let mut c = w.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase(),
        None => String::new(),
    }
}

pub fn to_pascal(name: &str) -> String {
    if is_upper_snake(name) && name.len() > 2 {
        // ALL_CAPS: every part is an ordinary word (MAX_HP -> MaxHp)
        return words(name)
            .iter()
            .map(|w| {
                let mut c = w.chars();
                c.next().map(|f| f.to_ascii_uppercase().to_string() + &c.as_str().to_ascii_lowercase()).unwrap_or_default()
            })
            .collect();
    }
    words(name).iter().map(|w| cap(w)).collect()
}

pub fn to_camel(name: &str) -> String {
    let ws = words(name);
    let mut s = String::new();
    for (i, w) in ws.iter().enumerate() {
        if i == 0 {
            s.push_str(&w.to_ascii_lowercase());
        } else {
            s.push_str(&cap(w));
        }
    }
    s
}

pub fn to_upper_snake(name: &str) -> String {
    words(name).iter().map(|w| w.to_ascii_uppercase()).collect::<Vec<_>>().join("_")
}

/// a letter without letter case (Hangul, CJK ...): case conventions say nothing about it (docs 10.4)
fn caseless(c: char) -> bool {
    c.is_alphabetic() && !c.is_lowercase() && !c.is_uppercase()
}
fn is_pascal(n: &str) -> bool {
    let mut c = n.chars();
    matches!(c.next(), Some(f) if f.is_ascii_uppercase()) && n.chars().all(|c| c.is_ascii_alphanumeric() || (!c.is_ascii() && c.is_alphanumeric()))
}
fn is_camel(n: &str) -> bool {
    let mut c = n.chars();
    matches!(c.next(), Some(f) if f.is_ascii_lowercase()) && n.chars().all(|c| c.is_ascii_alphanumeric() || (!c.is_ascii() && c.is_alphanumeric()))
}
fn is_upper_snake(n: &str) -> bool {
    let mut c = n.chars();
    matches!(c.next(), Some(f) if f.is_ascii_uppercase()) && n.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || caseless(c))
}
/// `p` is a prefix of `n` followed by the start of a word: an upper-case letter, or a letter
/// without case (`SVec`, `S벡터`, `bAlive`; not `Spline` or `Item`)
fn has_prefix(n: &str, p: &str) -> bool {
    n.len() > p.len() && n.starts_with(p) && n[p.len()..].chars().next().map(|c| c.is_uppercase() || caseless(c)).unwrap_or(false)
}
fn strip_prefixes<'a>(n: &'a str, ps: &[&str]) -> &'a str {
    for p in ps {
        if has_prefix(n, p) {
            return &n[p.len()..];
        }
    }
    n.trim_start_matches('_')
}
fn bool_word_prefix(n: &str) -> bool {
    ["Is", "Can", "Has", "Should", "is", "can", "has", "should"].iter().any(|p| has_prefix(n, p))
}
/// "Overlaps", "Contains": third-person verb (allowed by the standard when Is/Has reads unnaturally)
fn third_person_verb(n: &str) -> bool {
    let first = words(n).into_iter().next().unwrap_or_default().to_ascii_lowercase();
    first.len() > 3 && first.ends_with('s') && !first.ends_with("ss")
}
fn acronym_followed_by_word(n: &str) -> bool {
    let cs: Vec<char> = n.chars().collect();
    let mut run = 0;
    for i in 0..cs.len() {
        if cs[i].is_ascii_uppercase() {
            run += 1;
        } else {
            if run >= 3 && cs[i].is_ascii_lowercase() {
                return true;
            }
            run = 0;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// renaming: what a new name may be, which symbols rename together, which names the running
// program shows (shared by `lint --fix` and the language server's rename)
// ---------------------------------------------------------------------------

/// Words a renamed symbol cannot be called: keywords, modifiers (`mut`, `own`, `client` ... are
/// contextual, but a name that is sometimes a keyword is a trap), primitive types and the built-in
/// names that code reaches without a declaration.
pub const RESERVED_NAMES: &[&str] = &[
    // keywords
    "if", "else", "while", "do", "for", "foreach", "in", "return", "break", "continue", "new", "this", "base", "true", "false", "null", "class",
    "struct", "enum", "interface", "namespace", "using", "atomic", "fail", "is", "ref", "out", "operator", "switch", "case", "default", "var",
    "await", "when", "where", "and", "or", "not", "get", "set", "typeof", "sizeof", "nameof",
    // modifiers
    "public", "private", "protected", "internal", "static", "readonly", "const", "virtual", "override", "abstract", "sealed", "partial", "extern",
    "unsafe", "async", "client", "own", "mut",
    // types
    "int", "float", "bool", "void", "string", "double", "long", "uint", "byte", "short", "char", "object", "List", "Dictionary", "KeyValuePair",
    "Entity",
    // built-ins
    "Math", "Random", "Time", "Draw", "Net", "Array", "Print", "Spawn", "Destroy", "Query", "Wait", "NextTick", "Until", "Race", "Sync", "Rush",
];

pub fn is_reserved_name(s: &str) -> bool {
    RESERVED_NAMES.contains(&s) || s.starts_with("__")
}

/// Is `s` an identifier the lexer reads as one name? (ASCII letters, digits and `_`, and Unicode
/// letters such as Hangul; not starting with a digit)
pub fn is_identifier(s: &str) -> bool {
    let mut cs = s.chars();
    let Some(first) = cs.next() else { return false };
    (first.is_ascii_alphabetic() || first == '_' || (!first.is_ascii() && first.is_alphabetic())) && cs.all(|c| c.is_ascii_alphanumeric() || c == '_' || (!c.is_ascii() && c.is_alphanumeric()))
}

/// The methods that must keep one name together with `f`: an interface method (with or without a
/// default implementation), the interfaces that extend its interface and declare it again, the
/// same method of the other instances of a generic interface, every method implementing it (in
/// entity classes, through base classes, and in structs, including the structs' copies of a
/// default implementation), and the virtual / abstract / override chains of entity classes.
/// Sorted; contains `f`.
pub fn related_methods(p: &Program, f: FuncId) -> Vec<FuncId> {
    let name = p.funcs[f].name.clone();
    let method = |g: FuncId| matches!(p.funcs[g].kind, FuncKind::EntityMethod(_) | FuncKind::StructMethod(_));
    if !method(f) {
        return vec![f];
    }
    // edges between same-named methods
    let mut edges: Vec<(FuncId, FuncId)> = vec![];
    // overloads of one name are different methods: an override or implementation has the same
    // parameter types
    let same = |g: FuncId, h: FuncId| !p.overloaded.contains(&name) || p.same_params(g, h);
    for (c, cd) in p.classes.iter().enumerate() {
        if cd.is_static {
            continue;
        }
        for g in p.own_methods_named(c, &name) {
            if !method(g) {
                continue;
            }
            let mut cur = cd.base;
            while let Some(b) = cur {
                for h in p.own_methods_named(b, &name) {
                    if method(h) && same(g, h) {
                        edges.push((g, h));
                    }
                }
                cur = p.classes[b].base;
            }
            for i in p.all_ifaces(c) {
                for h in p.own_methods_named(i, &name) {
                    if same(g, h) {
                        edges.push((g, h));
                    }
                }
            }
        }
    }
    for s in p.struct_ifaces.keys() {
        for g in p.struct_methods_named(*s, &name) {
            for i in p.struct_all_ifaces(*s) {
                for h in p.own_methods_named(i, &name) {
                    if same(g, h) {
                        edges.push((g, h));
                    }
                }
            }
        }
    }
    // the instances of a generic interface share their declaration (the method at the same place)
    for (&i, &first) in &p.iface_family {
        let (a, b) = (p.iface_methods(i), p.iface_methods(first));
        for (g, h) in a.into_iter().zip(b) {
            if p.funcs[g].name == name && p.funcs[h].name == name {
                edges.push((g, h));
            }
        }
    }
    let mut group = vec![f];
    let mut i = 0;
    while i < group.len() {
        let x = group[i];
        for &(a, b) in &edges {
            for (from, to) in [(a, b), (b, a)] {
                if from == x && !group.contains(&to) {
                    group.push(to);
                }
            }
        }
        i += 1;
    }
    group.sort();
    group
}

/// The symbols a rename of `sym` must rename too (see `related_methods`); contains `sym`.
pub fn related_symbols(p: &Program, sym: Sym) -> Vec<Sym> {
    match sym {
        Sym::Func(f) => related_methods(p, f).into_iter().map(Sym::Func).collect(),
        _ => vec![sym],
    }
}

/// Why the name `old` of `sym` is visible when the program runs (so renaming it changes what the
/// program prints or does), if it is.
pub fn runtime_visible_name(p: &Program, sym: Sym, old: &str) -> Option<String> {
    match sym {
        Sym::Variant(e, _) => return Some(format!("'{}' is what ToString(), Print and $\"{{...}}\" show for this value of '{}', so renaming it changes the program's output", old, p.enums[e].name)),
        Sym::Class(c) if p.classes[c].is_entity && !p.classes[c].is_abstract && !p.classes[c].is_interface => {
            return Some(format!("the entity class name '{}' appears in the runtime summary and in saved game states (--save-state, live coding)", old));
        }
        Sym::Func(f) if p.funcs[f].extern_name.as_deref() == Some(old) => {
            return Some(format!("'{}' is also the name of the C function the host program defines (write extern(\"{}\") to keep it)", old, old));
        }
        _ => {}
    }
    if matches!(sym, Sym::Func(_) | Sym::Class(_) | Sym::Struct(_) | Sym::Enum(_)) {
        // the random numbers of a function are a stream keyed by its name (Class.Method, with the
        // type arguments of generic instances)
        let generic_base = |s: &str| s.split('<').next().unwrap_or("").to_string();
        for (g, gd) in p.funcs.iter().enumerate() {
            let named = match sym {
                Sym::Func(f) => generic_base(&gd.name) == old && generic_base(gd.owner_name(p)) == generic_base(p.funcs[f].owner_name(p)),
                _ => p.func_label(g).split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w == old),
            };
            if !named {
                continue;
            }
            let mut random = false;
            crate::sema::visit_block(&gd.body, &mut |e| random |= matches!(e.kind, ExprKind::Builtin { f: Builtin::RandomFloat | Builtin::RandomInt, .. }));
            if random {
                return Some(format!("the random numbers drawn in '{}' are seeded from its name, so renaming it changes them", p.func_label(g)));
            }
        }
    }
    None
}

#[derive(Clone)]
enum NameKind {
    Type(DeclKind),
    Variant,
    Const,
    Field { public: bool, ty: Ty },
    Method { public: bool, ret: Ty },
    Local { ty: Ty, loop_index: bool },
}

// ---------------------------------------------------------------------------
// the linter
// ---------------------------------------------------------------------------

struct Linter<'a> {
    c: &'a Compiled,
    p: &'a Program,
    src: &'a str,
    cfg: &'a LintConfig,
    out: Vec<LintDiag>,
    all_names: HashSet<String>,
}

pub fn lint(c: &Compiled, src: &str, cfg: &LintConfig) -> Vec<LintDiag> {
    let p = &c.prog;
    let mut all_names = HashSet::new();
    for (sp, _) in p.index.defs.iter().map(|(s, sp)| (sp, s)) {
        all_names.insert(text(src, *sp).to_string());
    }
    let mut l = Linter { c, p, src, cfg, out: vec![], all_names };
    l.naming();
    l.style();
    l.correctness();
    l.game_rules();
    // suppressions, de-duplication and ordering
    let sup = parse_suppressions(src);
    let mut seen = HashSet::new();
    let mut out: Vec<LintDiag> = l
        .out
        .into_iter()
        .filter(|d| {
            let line = line_col(src, d.span.lo).0;
            let on_line = sup.lines.get(&line).map(|s| s.contains(d.code) || s.contains("all")).unwrap_or(false);
            !(sup.file.contains(d.code) || sup.file.contains("all") || on_line)
        })
        .filter(|d| seen.insert((d.code, d.span.lo, d.msg.clone())))
        .collect();
    out.sort_by_key(|d| (d.span.lo, d.code));
    out
}

fn text(src: &str, sp: Span) -> &str {
    let lo = (sp.lo as usize).min(src.len());
    let hi = (sp.hi as usize).min(src.len()).max(lo);
    &src[lo..hi]
}

fn line_indent(src: &str, off: u32) -> String {
    let off = (off as usize).min(src.len());
    let start = src[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
    src[start..].chars().take_while(|c| *c == ' ' || *c == '\t').collect()
}

impl<'a> Linter<'a> {
    fn emit(&mut self, code: &'static str, span: Span, msg: impl Into<String>, note: Option<String>, fix: Option<Fix>) {
        if let Some(level) = self.cfg.level(code) {
            self.out.push(LintDiag { code, level, span, msg: msg.into(), note, fix });
        }
    }
    fn on(&self, code: &str) -> bool {
        self.cfg.level(code).is_some()
    }

    // ------------------------------------------------------------------ naming

    fn rename_fix(&self, sym: Sym, old: &str, new: &str) -> Option<Fix> {
        if new == old || new.is_empty() || self.all_names.contains(new) || is_reserved_name(new) {
            return None;
        }
        // a method tied to others by name (interface method and its implementations, virtual and
        // override chains) would need a coordinated rename: the editor's rename does that
        if related_symbols(self.p, sym).len() > 1 {
            return None;
        }
        if runtime_visible_name(self.p, sym, old).is_some() {
            return None;
        }
        let edits: Vec<Edit> = self.p.index.occurrences(sym).into_iter().filter(|sp| text(self.src, *sp) == old).map(|sp| Edit { span: sp, text: new.to_string() }).collect();
        if edits.is_empty() {
            return None;
        }
        Some(Fix { title: format!("rename '{}' to '{}'", old, new), edits })
    }

    fn check_name(&mut self, sym: Sym, span: Span, kind: NameKind) {
        let name = text(self.src, span).to_string();
        if name.is_empty() || name.starts_with("__") {
            return;
        }
        if !name.chars().next().map(|c| c.is_ascii()).unwrap_or(true) {
            return; // scripts without letter case (e.g. Hangul): case conventions do not apply
        }
        let standard = matches!(self.cfg.preset, Preset::Standard | Preset::Game);
        let game = self.cfg.preset == Preset::Game;
        let mut issues: Vec<(&'static str, String)> = vec![];
        let mut expected: String;
        match &kind {
            NameKind::Type(k) => {
                let (pref, code, what) = match k {
                    DeclKind::Struct => ("S", "L102", "struct"),
                    DeclKind::Enum => ("E", "L103", "enum"),
                    DeclKind::Class => ("", "", "class"),
                    DeclKind::Interface => ("I", "L114", "interface"),
                    DeclKind::Component => ("", "", "component"),
                    DeclKind::Delegate => ("", "", "delegate"),
                };
                let prefs = [pref];
                // the case is fixed first, so a prefix written in the wrong case counts as the
                // prefix: iShape -> IShape (not IIShape), eTeam -> ETeam
                let pascal = to_pascal(&name);
                let mut want = strip_prefixes(&pascal, if pref.is_empty() { &prefs[..0] } else { &prefs[..] }).to_string();
                if !is_pascal(&name) && self.on("L101") {
                    issues.push(("L101", format!("{} name '{}' should be PascalCase", what, name)));
                }
                if (standard || code == "L114") && !pref.is_empty() && self.on(code) {
                    if !has_prefix(&name, pref) {
                        issues.push((code, format!("{} name '{}' should start with '{}' (e.g. '{}{}')", what, name, pref, pref, want)));
                    }
                    want = format!("{}{}", pref, want);
                }
                expected = want;
            }
            NameKind::Variant => {
                if !is_pascal(&name) && self.on("L101") {
                    issues.push(("L101", format!("enum member '{}' should be PascalCase", name)));
                }
                expected = to_pascal(&name);
            }
            NameKind::Const => {
                if standard {
                    if !is_upper_snake(&name) {
                        issues.push(("L106", format!("constant '{}' should be ALL_CAPS_WITH_UNDERSCORES", name)));
                    }
                    expected = to_upper_snake(&name);
                } else {
                    if !is_pascal(&name) {
                        issues.push(("L106", format!("constant '{}' should be PascalCase", name)));
                    }
                    expected = to_pascal(&name);
                }
            }
            NameKind::Field { public, ty } => {
                let is_bool = *ty == Ty::Bool;
                let base = strip_prefixes(&name, &["mb", "m", "b"]);
                let base = if base.is_empty() { name.as_str() } else { base };
                if *public {
                    if !is_pascal(&name) && !(standard && has_prefix(&name, "b")) {
                        issues.push(("L101", format!("public field '{}' should be PascalCase", name)));
                    }
                    let mut want = to_pascal(base);
                    if standard && is_bool && self.on("L107") {
                        if !has_prefix(&name, "b") && !bool_word_prefix(&name) {
                            issues.push(("L107", format!("bool field '{}' should start with 'b' or Is/Can/Has/Should", name)));
                            want = format!("b{}", want);
                        } else if has_prefix(&name, "b") {
                            want = format!("b{}", want);
                        }
                    }
                    expected = want;
                } else if standard {
                    let want_prefix = if is_bool && self.on("L107") { "mb" } else { "m" };
                    if !has_prefix(&name, want_prefix) {
                        let code = if want_prefix == "mb" && has_prefix(&name, "m") { "L107" } else { "L109" };
                        issues.push((code, format!("private field '{}' should be '{}' + PascalCase", name, want_prefix)));
                    }
                    expected = format!("{}{}", want_prefix, to_pascal(base));
                } else {
                    if !(name.starts_with('_') && is_camel(&name[1..])) {
                        issues.push(("L109", format!("private field '{}' should be _camelCase", name)));
                    }
                    expected = format!("_{}", to_camel(base));
                }
            }
            NameKind::Method { public, ret } => {
                let base = name.as_str();
                let base = if base.is_empty() { name.as_str() } else { base };
                if *public || !standard {
                    if !is_pascal(&name) {
                        issues.push(("L105", format!("{} method '{}' should be PascalCase", if *public { "public" } else { "non-public" }, name)));
                    }
                    expected = to_pascal(base);
                } else {
                    if !is_camel(&name) {
                        issues.push(("L105", format!("non-public method '{}' should be camelCase", name)));
                    }
                    expected = to_camel(base);
                }
                if standard && *ret == Ty::Bool && self.on("L108") && !bool_word_prefix(&name) && !third_person_verb(&name) {
                    issues.push(("L108", format!("bool method '{}' should start with Is/Can/Has/Should (or be a verb like Overlaps)", name)));
                }
            }
            NameKind::Local { ty, loop_index } => {
                let is_bool = *ty == Ty::Bool;
                let base = if standard && is_bool { strip_prefixes(&name, &["b"]) } else { name.as_str() };
                let base = if base.is_empty() { name.as_str() } else { base };
                let camel_ok = is_camel(&name) || (standard && has_prefix(&name, "b"));
                if !camel_ok {
                    issues.push(("L104", format!("local '{}' should be camelCase", name)));
                }
                expected = to_camel(base);
                if standard && is_bool && self.on("L107") {
                    if !has_prefix(&name, "b") {
                        issues.push(("L107", format!("bool variable '{}' should start with 'b'", name)));
                    }
                    expected = format!("b{}", to_pascal(base));
                }
                // the game preset: a single letter is fine for a number or a vector (math and coordinates)
                let math = game && matches!(ty, Ty::Int | Ty::Float | Ty::Char | Ty::Struct(_) | Ty::Enum(_));
                if name.chars().count() == 1 && !loop_index && !math && !matches!(name.as_str(), "x" | "y" | "z") && self.on("L113") {
                    issues.push(("L113", format!("'{}' is not descriptive (single letters are for loop indices)", name)));
                }
            }
        }
        let core = strip_prefixes(&name, &["mb", "m", "b", "S", "E", "_"]);
        if !matches!(kind, NameKind::Const) && acronym_followed_by_word(core) && standard {
            issues.push(("L112", format!("'{}': write acronyms followed by a word in PascalCase (e.g. 'HudScale')", name)));
            expected = expected.replace(core, &to_pascal(core));
        }
        let mut fixed = false;
        // why the rename is reported but not applied automatically (the name is visible when the
        // program runs, so renaming it would change what the program does or prints)
        let kept = if expected == name {
            None
        } else if let Some(why) = runtime_visible_name(self.p, sym, &name) {
            Some(why)
        } else {
            let group = related_symbols(self.p, sym);
            (group.len() > 1).then(|| {
                let names: Vec<String> = group.iter().filter_map(|s| if let Sym::Func(f) = s { Some(self.p.func_label(*f)) } else { None }).collect();
                format!("'{}' renames together with {} (interface method, implementations, overrides): use the editor's rename", name, names.join(", "))
            })
        };
        for (code, msg) in issues {
            if !self.on(code) {
                continue;
            }
            let fix = if !fixed && code != "L108" && code != "L113" { self.rename_fix(sym, &name, &expected) } else { None };
            fixed |= fix.is_some();
            let note = kept.as_ref().filter(|_| code != "L108" && code != "L113").map(|why| format!("not fixed automatically: {}", why));
            self.emit(code, span, msg, note, fix);
        }
    }

    fn naming(&mut self) {
        let p = self.p;
        let ast = &self.c.ast;
        for d in &ast.items {
            let tsym = p.index.defs.iter().find(|(_, sp)| **sp == d.name_span).map(|(s, _)| *s);
            if let Some(sym) = tsym {
                if d.name != "Game" {
                    self.check_name(sym, d.name_span, NameKind::Type(d.kind));
                }
            }
            if d.kind == DeclKind::Enum {
                if let Some(Sym::Enum(e)) = tsym {
                    for (i, (_, _, vsp)) in d.variants.iter().enumerate() {
                        self.check_name(Sym::Variant(e, i), *vsp, NameKind::Variant);
                    }
                }
            }
            for m in &d.members {
                match m {
                    ast::Member::Field(f) => {
                        let Some(&(_, sym)) = p.index.refs.iter().find(|(sp, _)| *sp == f.span) else { continue };
                        let kind = match sym {
                            Sym::Const(_) => NameKind::Const,
                            Sym::Field(fid) => NameKind::Field { public: f.mods.is_public, ty: p.fields[fid].ty.clone() },
                            Sym::SField(s, i) => NameKind::Field { public: f.mods.is_public, ty: p.structs[s].fields[i].ty.clone() },
                            _ => continue,
                        };
                        self.check_name(sym, f.span, kind);
                    }
                    ast::Member::Method(md) => {
                        if !matches!(md.kind, MethodKind::Normal) {
                            // constructors and operators: check parameters only
                        }
                        let fsym = p.index.refs.iter().find(|(sp, s)| *sp == md.name_span && matches!(s, Sym::Func(_))).map(|(_, s)| *s);
                        let fid = match fsym {
                            Some(Sym::Func(f)) => Some(f),
                            _ => p.index.func_spans.iter().find(|(_, sp)| sp.lo == md.span.lo).map(|(f, _)| *f),
                        };
                        let Some(fid) = fid else { continue };
                        let func = &p.funcs[fid];
                        let engine = (md.name == "Update" || md.name == "Start") || md.mods.is_override || func.is_override;
                        if matches!(md.kind, MethodKind::Normal) && !engine {
                            let public = md.mods.is_public || matches!(func.kind, FuncKind::StructOp(_)) || d.kind == DeclKind::Interface;
                            self.check_name(Sym::Func(fid), md.name_span, NameKind::Method { public, ret: func.ret.clone() });
                        }
                        let is_op = matches!(md.kind, MethodKind::Operator(_));
                        for (i, prm) in md.params.iter().enumerate() {
                            if i < func.params.len() && !(is_op && prm.name.len() == 1) {
                                let l = func.params[i];
                                self.check_name(Sym::Local(fid, l), prm.name_span, NameKind::Local { ty: func.locals[l].ty.clone(), loop_index: false });
                            }
                        }
                    }
                }
            }
        }
        // locals (definition sites from the semantic index)
        let mut loop_vars: HashSet<u32> = HashSet::new();
        for d in &ast.items {
            for m in &d.members {
                if let ast::Member::Method(md) = m {
                    if let Some(b) = &md.body {
                        collect_for_init_vars(&b.stmts, &mut loop_vars);
                    }
                }
            }
        }
        let mut defs: Vec<(Sym, Span)> = p.index.defs.iter().map(|(s, sp)| (*s, *sp)).collect();
        defs.sort_by_key(|(_, sp)| sp.lo);
        for (sym, sp) in defs {
            if let Sym::Local(f, l) = sym {
                let loc = &p.funcs[f].locals[l];
                if matches!(loc.kind, LocalKind::Param(_)) {
                    continue;
                }
                self.check_name(sym, sp, NameKind::Local { ty: loc.ty.clone(), loop_index: loop_vars.contains(&sp.lo) });
            }
        }
    }

    // ------------------------------------------------------------------ style

    fn style(&mut self) {
        let ast = &self.c.ast;
        for d in &ast.items {
            self.check_allman(d.body_open, d.span.lo);
            // member order: fields, properties, constructors, methods (public before private)
            let mut last = (0u8, 0u8);
            let mut reported = false;
            for m in &d.members {
                let (rank, sub, sp) = match m {
                    ast::Member::Field(f) => (0, 0, f.span),
                    ast::Member::Method(md) => {
                        if let Some(b) = &md.body {
                            if !md.expr_body {
                                self.check_allman(b.span, md.span.lo);
                            }
                            self.style_block(&b.stmts);
                        }
                        let vis = if md.mods.is_public || d.kind == DeclKind::Interface { 0 } else { 1 };
                        match md.kind {
                            MethodKind::Ctor => (2, 0, md.name_span),
                            _ if md.is_property => (1, 0, md.name_span),
                            _ => (3, vis, md.name_span),
                        }
                    }
                };
                if (rank, sub) < last && !reported && d.kind != DeclKind::Enum {
                    let what = ["field", "property", "constructor", "method"][rank as usize];
                    self.emit(
                        "L205",
                        sp,
                        format!("{} '{}' is out of order", what, text(self.src, sp)),
                        Some("order members as: fields, properties, constructors, then methods (public before non-public)".into()),
                        None,
                    );
                    reported = true;
                }
                last = last.max((rank, sub));
            }
        }
    }

    fn check_allman(&mut self, open: Span, owner_lo: u32) {
        if text(self.src, open) != "{" {
            return;
        }
        let lo = open.lo as usize;
        let start = self.src[..lo].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if self.src[start..lo].trim().is_empty() {
            return;
        }
        // find the matching '}' to decide whether the block spans multiple lines
        let mut depth = 0i32;
        let mut close = None;
        for (i, ch) in self.src[lo..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(lo + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let multiline = close.map(|c| self.src[lo..c].contains('\n')).unwrap_or(false);
        let fix = if multiline {
            let ind = line_indent(self.src, owner_lo.max(start as u32));
            let mut ws_start = lo;
            while ws_start > 0 && matches!(self.src.as_bytes()[ws_start - 1], b' ' | b'\t') {
                ws_start -= 1;
            }
            Some(Fix { title: "move '{' to its own line".into(), edits: vec![Edit { span: Span::new(ws_start, lo), text: format!("\n{}", ind) }] })
        } else {
            None
        };
        self.emit("L204", open, "opening brace should be on its own line", None, fix);
    }

    /// bodies are blocks (the grammar requires them): their '{' goes on its own line
    fn body_allman(&mut self, body: &AStmt, owner_lo: u32) {
        if let AStmt::Block(b) = body {
            self.check_allman(b.span, owner_lo);
        }
    }

    fn style_block(&mut self, stmts: &[AStmt]) {
        let mut terminated: Option<&str> = None;
        for s in stmts {
            if let Some(kw) = terminated {
                if !matches!(s, AStmt::Empty(_)) {
                    self.emit("L311", s.span(), format!("unreachable code after '{}'", kw), None, None);
                    terminated = None;
                }
            }
            self.style_stmt(s);
            terminated = match s {
                AStmt::Return { .. } => Some("return"),
                AStmt::Break(_) => Some("break"),
                AStmt::Continue(_) => Some("continue"),
                AStmt::Fail(_) => Some("fail"),
                _ => terminated,
            };
        }
    }

    fn style_stmt(&mut self, s: &AStmt) {
        match s {
            AStmt::Block(b) => self.style_block(&b.stmts),
            AStmt::Var { ty, init, name_span, span, .. } => {
                if ty.is_none() {
                    let obvious = matches!(init.as_ref().map(|e| &e.kind), Some(ast::ExprKind::New { .. } | ast::ExprKind::NewColl { .. } | ast::ExprKind::NewArray { .. } | ast::ExprKind::Cast(..)))
                        || matches!(init.as_ref().map(|e| &e.kind), Some(ast::ExprKind::Call { callee, .. }) if matches!(&callee.kind, ast::ExprKind::Name(n) if n == "Spawn"));
                    if !obvious {
                        let tyname = match self.p.index.refs.iter().find(|(sp, _)| sp == name_span) {
                            Some((_, Sym::Local(f, l))) => Some(self.p.ty_name(&self.p.funcs[*f].locals[*l].ty)),
                            _ => None,
                        };
                        // `var x` or `mut var x`: the `var` right before the name
                        let head = text(self.src, Span { lo: span.lo, hi: name_span.lo.max(span.lo) });
                        let at = head.trim_end().strip_suffix("var").map(|h| h.len()).unwrap_or(0);
                        let var_span = Span::new(span.lo as usize + at, span.lo as usize + at + 3);
                        let fix = tyname.as_ref().filter(|_| text(self.src, var_span) == "var").map(|t| Fix { title: format!("replace 'var' with '{}'", t), edits: vec![Edit { span: var_span, text: t.clone() }] });
                        let msg = match &tyname {
                            Some(t) => format!("use the explicit type '{}' instead of 'var'", t),
                            None => "use an explicit type instead of 'var'".into(),
                        };
                        self.emit("L202", var_span, msg, Some("'var' is fine when the type is obvious: new T(...), Spawn(new T {...}), (T)x".into()), fix);
                    }
                }
                if let Some(e) = init {
                    self.style_expr(e);
                }
            }
            AStmt::If { cond, then, els, span } => {
                self.style_expr(cond);
                self.body_allman(then, span.lo);
                self.style_stmt(then);
                if let Some(e) = els {
                    self.body_allman(e, span.lo);
                    self.style_stmt(e);
                    let (a, b) = (norm(text(self.src, then.span())), norm(text(self.src, e.span())));
                    if a == b && !a.is_empty() {
                        self.emit("L309", e.span(), "if and else branches are identical", None, None);
                    }
                    if let (Some(x), Some(y)) = (returns_bool(then), returns_bool(e)) {
                        if x != y {
                            let c = text(self.src, cond.span);
                            let suggestion = if x { format!("return {};", c) } else { format!("return !({});", c) };
                            self.emit("L313", *span, format!("simplify to '{}'", suggestion), None, None);
                        }
                    }
                }
            }
            AStmt::While { cond, body, span } => {
                self.style_expr(cond);
                self.body_allman(body, span.lo);
                self.style_stmt(body);
            }
            AStmt::DoWhile { body, cond, span } => {
                self.body_allman(body, span.lo);
                self.style_stmt(body);
                self.style_expr(cond);
            }
            AStmt::For { init, cond, step, body, span } => {
                for s in init {
                    self.style_stmt(s);
                }
                if let Some(c) = cond {
                    self.style_expr(c);
                }
                for e in step {
                    self.style_expr(e);
                }
                self.body_allman(body, span.lo);
                self.style_stmt(body);
            }
            AStmt::Foreach { iter, body, span, .. } => {
                self.style_expr(iter);
                self.body_allman(body, span.lo);
                self.style_stmt(body);
            }
            AStmt::Return { value: Some(e), .. } => self.style_expr(e),
            AStmt::Expr(e, _) => self.style_expr(e),
            AStmt::Atomic { body, span } => {
                self.check_allman(body.span, span.lo);
                self.style_block(&body.stmts);
            }
            AStmt::Switch { scrut, sections, .. } => {
                self.style_expr(scrut);
                for sec in sections {
                    for l in &sec.labels {
                        if let ast::CaseLabel::Case(pat, guard, _) = l {
                            self.style_pattern(pat);
                            if let Some(g) = guard {
                                self.style_expr(g);
                            }
                        }
                    }
                    self.style_block(&sec.body);
                }
            }
            _ => {}
        }
    }

    fn style_pattern(&mut self, p: &ast::Pattern) {
        match p {
            ast::Pattern::Const(e) | ast::Pattern::Rel(_, e, _) => self.style_expr(e),
            ast::Pattern::Not(x, _) => self.style_pattern(x),
            ast::Pattern::And(a, b) | ast::Pattern::Or(a, b) => {
                self.style_pattern(a);
                self.style_pattern(b);
            }
            ast::Pattern::Discard(_) => {}
        }
    }

    fn style_expr(&mut self, e: &ast::Expr) {
        use ast::ExprKind as K;
        match &e.kind {
            K::Binary(op @ (BinOp::Eq | BinOp::Ne), a, b) => {
                let lit = |x: &ast::Expr| if let K::Bool(v) = x.kind { Some(v) } else { None };
                let (other, val) = match (lit(a), lit(b)) {
                    (_, Some(v)) => (Some(a), v),
                    (Some(v), _) => (Some(b), v),
                    _ => (None, false),
                };
                if let Some(o) = other {
                    let positive = (*op == BinOp::Eq) == val;
                    let ot = text(self.src, o.span);
                    let new = if positive { ot.to_string() } else { format!("!({})", ot) };
                    let fix = Fix { title: format!("simplify to '{}'", new), edits: vec![Edit { span: e.span, text: new.clone() }] };
                    self.emit("L307", e.span, format!("comparison with a bool literal; write '{}'", new), None, Some(fix));
                }
                self.style_expr(a);
                self.style_expr(b);
            }
            K::Assign(None, t, v) => {
                let (a, b) = (norm(text(self.src, t.span)), norm(text(self.src, v.span)));
                if a == b && matches!(t.kind, K::Name(_) | K::Member(..)) {
                    self.emit("L310", e.span, format!("'{}' is assigned to itself", a), None, None);
                }
                self.style_expr(t);
                self.style_expr(v);
            }
            K::Member(b, _, _) | K::Unary(_, b) | K::Cast(_, b) | K::IncDec(b, _, _) | K::Is(b, _, _) => self.style_expr(b),
            K::Call { callee, args, .. } => {
                self.style_expr(callee);
                for a in args {
                    self.style_expr(&a.expr);
                }
            }
            K::Index(a, ix) => {
                self.style_expr(a);
                for b in ix {
                    self.style_expr(b);
                }
            }
            K::Binary(_, a, b) | K::Assign(_, a, b) => {
                self.style_expr(a);
                self.style_expr(b);
            }
            K::Cond(a, b, c) => {
                self.style_expr(a);
                self.style_expr(b);
                self.style_expr(c);
            }
            K::New { args, inits, .. } => {
                for a in args.iter().flatten() {
                    self.style_expr(&a.expr);
                }
                for (_, x, _) in inits {
                    self.style_expr(x);
                }
            }
            K::Interp(parts) => {
                for part in parts {
                    if let ast::InterpPart::Expr(x, _) = part {
                        self.style_expr(x);
                    }
                }
            }
            K::Switch(x, arms) => {
                self.style_expr(x);
                for a in arms {
                    self.style_pattern(&a.pat);
                    if let Some(g) = &a.guard {
                        self.style_expr(g);
                    }
                    self.style_expr(&a.value);
                }
            }
            K::IsPattern(x, pat) => {
                self.style_expr(x);
                self.style_pattern(pat);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ correctness (HIR)

    /// name spans of `mut` for-header variables that neither the condition nor the body changes:
    /// `for (mut int i = 0; i < n; i++)` needs no `mut` (the step changes header variables without it)
    fn header_only_mut(&self) -> HashSet<u32> {
        let p = self.p;
        let mut out = HashSet::new();
        for d in &self.c.ast.items {
            for m in &d.members {
                let ast::Member::Method(md) = m else { continue };
                let Some(b) = &md.body else { continue };
                ast_visit_stmts(&b.stmts, &mut |s| {
                    let AStmt::For { init, cond, body, .. } = s else { return };
                    for v in init {
                        let AStmt::Var { is_mut: true, name_span, .. } = v else { continue };
                        let Some(&(_, sym @ Sym::Local(f, l))) = p.index.refs.iter().find(|(sp, s)| sp == name_span && matches!(s, Sym::Local(..))) else { continue };
                        // values only: changing a struct, list or dictionary has more forms
                        if !matches!(p.funcs[f].locals[l].ty, Ty::Int | Ty::Float | Ty::Char | Ty::Bool | Ty::Enum(_) | Ty::Str) {
                            continue;
                        }
                        let mut writes = HashSet::new();
                        if let Some(c) = cond {
                            ast_write_roots(c, &mut writes);
                        }
                        ast_visit_stmt_exprs(body, &mut |e| ast_write_root(e, &mut writes));
                        if !p.index.refs.iter().any(|(sp, s2)| *s2 == sym && writes.contains(sp)) {
                            out.insert(name_span.lo);
                        }
                    }
                });
            }
        }
        out
    }

    fn correctness(&mut self) {
        let p = self.p;
        let header_only = self.header_only_mut();
        // reads of locals
        for (fid, f) in p.funcs.iter().enumerate() {
            if f.extern_name.is_some() {
                continue; // host functions have no body: their parameters are used by C code
            }
            let read: HashSet<LocalId> = self.c.reads_before_opt.get(fid).cloned().unwrap_or_default();
            // signatures imposed from elsewhere: engine entry points, overrides, interface methods and
            // their implementations (and abstract methods, which have no body)
            let iface_impl = match f.kind {
                FuncKind::EntityMethod(c) => p.classes[c].is_interface || p.all_ifaces(c).iter().any(|&i| p.classes[i].methods.contains_key(&f.name)),
                FuncKind::StructMethod(s) => p.struct_ifaces.get(&s).map(|l| l.iter().any(|&i| p.classes[i].methods.contains_key(&f.name) || p.all_ifaces(i).iter().any(|&j| p.classes[j].methods.contains_key(&f.name)))).unwrap_or(false),
                _ => false,
            };
            let is_engine = f.name == "Update" || f.name == "Start" || f.is_override || f.is_abstract || iface_impl || p.funcs.iter().any(|g| g.name == f.name && g.is_override);
            for (l, loc) in f.locals.iter().enumerate() {
                // a coroutine local that lives across an await became a field: read if the field is
                if let Some((_, fld)) = f.co.as_ref().and_then(|co| co.hoisted.iter().find(|(x, _)| *x == l)) {
                    if !self.c.ecs.is_dead(*fld) {
                        continue;
                    }
                }
                if read.contains(&l) || loc.name.starts_with("__") || loc.name.starts_with('_') {
                    continue;
                }
                let Some(&sp) = p.index.defs.get(&Sym::Local(fid, l)) else { continue };
                match loc.kind {
                    LocalKind::Normal => self.emit("L301", sp, format!("local '{}' is assigned but never read", loc.name), None, None),
                    LocalKind::Param(crate::ast::ParamMode::Value) | LocalKind::Param(crate::ast::ParamMode::Own) if !is_engine && !matches!(f.kind, FuncKind::StructOp(_)) => {
                        self.emit("L302", sp, format!("parameter '{}' is never used", loc.name), None, None)
                    }
                    _ => {}
                }
            }
            // `mut` that nothing uses: the binding could say it never changes. A declaration without a
            // body (interface method, abstract method) has nothing that could change its parameters:
            // `mut` there is not checked against the implementations either.
            let bodiless = f.is_abstract;
            for (l, loc) in f.locals.iter().enumerate() {
                if !loc.is_mut || loc.name.starts_with("__") || (bodiless && matches!(loc.kind, LocalKind::Param(_))) {
                    continue;
                }
                if !matches!(loc.kind, LocalKind::Normal | LocalKind::Param(crate::ast::ParamMode::Value) | LocalKind::Param(crate::ast::ParamMode::Own)) {
                    continue;
                }
                let Some(&sp) = p.index.defs.get(&Sym::Local(fid, l)) else { continue };
                if !loc.written {
                    self.emit("L316", sp, format!("'{}' is declared 'mut' but never changes", loc.name), Some("remove 'mut'".into()), None);
                } else if header_only.contains(&sp.lo) {
                    self.emit(
                        "L316",
                        sp,
                        format!("'{}' is declared 'mut' but only the loop header changes it", loc.name),
                        Some("remove 'mut': the variables a for header declares change in its step without it".into()),
                        None,
                    );
                }
            }
            // integer division used as float, float equality
            crate::sema::visit_block(&f.body, &mut |e| match &e.kind {
                ExprKind::Binary(BinOp::Eq | BinOp::Ne, a, b) if a.ty == Ty::Float && b.ty == Ty::Float => {
                    let zero = |x: &Expr| matches!(x.kind, ExprKind::Float(v) if v == 0.0);
                    if !zero(a) && !zero(b) {
                        self.out_push("L306", e.span, "exact comparison of floats", Some("compare with a tolerance: Math.Abs(a - b) < epsilon".into()));
                    }
                }
                _ => {}
            });
            // cognitive complexity
            if let Some(&sp) = p.index.defs.get(&Sym::Func(fid)) {
                let cc = complexity(&f.body, 0);
                if cc > self.cfg.max_complexity {
                    self.emit("L314", sp, format!("'{}' has cognitive complexity {} (max {})", f.name, cc, self.cfg.max_complexity), Some("split it into smaller functions".into()), None);
                }
            }
        }
        for sp in self.c.int_div_float.clone() {
            self.out_push("L305", sp, "integer division result is converted to float (the fraction is lost)", Some("make one operand a float: (float)a / b".into()));
        }
        // unused non-public methods
        for d in self.c.ast.items.iter().filter(|d| d.kind != DeclKind::Interface) {
            for m in &d.members {
                let ast::Member::Method(md) = m else { continue };
                if md.mods.is_public || md.mods.is_override || md.mods.is_virtual || !matches!(md.kind, MethodKind::Normal) || md.name == "Update" || md.name == "Start" {
                    continue;
                }
                let Some(&(_, sym)) = p.index.refs.iter().find(|(sp, s)| *sp == md.name_span && matches!(s, Sym::Func(_))) else { continue };
                let Sym::Func(fid) = sym else { continue };
                if p.funcs[fid].ctx == Ctx::Client && p.funcs[fid].params.is_empty() {
                    continue; // client system
                }
                if p.index.occurrences(sym).len() <= 1 {
                    self.emit("L303", md.name_span, format!("non-public method '{}' is never called", md.name), None, None);
                }
            }
        }
        // the instances of a generic class share each field's declaration: a finding about the
        // declaration holds only if it holds in every instance
        let same_decl = |f: FieldId| -> Vec<FieldId> {
            match p.generic_of.get(&p.fields[f].class) {
                Some(_) => (0..p.fields.len()).filter(|&g| p.fields[g].span == p.fields[f].span && p.fields[g].name == p.fields[f].name).collect(),
                None => vec![f],
            }
        };
        // fields that are never read (the compiler removed their storage)
        for &f in &self.c.ecs.dead_fields {
            if p.fields[f].name.starts_with("__") || !same_decl(f).iter().all(|g| self.c.ecs.dead_fields.contains(g)) {
                continue; // compiler-generated (coroutine frames), or read in another instance
            }
            let sp = p.fields[f].span;
            self.emit("L304", sp, format!("field '{}' is written but never read; the compiler removed its storage", p.source_field_label(f)), None, None);
        }
        // `mut` fields that nothing changes after spawn
        for (f, fd) in p.fields.iter().enumerate() {
            if fd.is_mut && !fd.name.starts_with("__") && same_decl(f).iter().all(|g| !p.written_fields.contains(g)) {
                self.emit("L316", fd.span, format!("field '{}' is declared 'mut' but never changes after spawn", p.source_field_label(f)), Some("remove 'mut': the field keeps the value it was spawned with".into()), None);
            }
        }
        // constant conditions
        for (sp, v) in self.c.opt.const_conditions.clone() {
            if text(self.src, sp).trim() == "true" || text(self.src, sp).trim() == "false" {
                continue; // deliberate literal
            }
            self.emit("L312", sp, format!("condition is always {}", v), None, None);
        }
    }

    fn out_push(&mut self, code: &'static str, span: Span, msg: &str, note: Option<String>) {
        self.emit(code, span, msg.to_string(), note, None);
    }

    // ------------------------------------------------------------------ game rules

    fn game_rules(&mut self) {
        let p = self.p;
        let c = self.c;
        let in_systems = entity_system_callers(p, c);
        for (fid, f) in p.funcs.iter().enumerate() {
            let entity_server = matches!(f.kind, FuncKind::EntityMethod(_)) && f.ctx == Ctx::Server;
            let client = f.ctx == Ctx::Client;
            let mut hits: Vec<(&'static str, Span, String, Option<String>)> = vec![];
            // Spawn statements whose handle is discarded do not serialize the system
            let mut discarded: HashSet<Span> = HashSet::new();
            fn stmt_spawns(b: &Block, out: &mut HashSet<Span>) {
                for s in &b.stmts {
                    if let Stmt::Expr(e @ Expr { kind: ExprKind::Spawn { .. }, .. }) = s {
                        out.insert(e.span);
                    }
                    if !matches!(s, Stmt::Atomic { .. }) {
                        for x in s.sub_blocks() {
                            stmt_spawns(x, out);
                        }
                    }
                }
            }
            stmt_spawns(&f.body, &mut discarded);
            crate::sema::visit_block(&f.body, &mut |e| match &e.kind {
                ExprKind::Assign { op, target, .. } => {
                    if let Some((field, false)) = crate::effects::target_field(target) {
                        let commutative = matches!(op, Some(BinOp::Add) | Some(BinOp::Sub)) && target.ty.is_numeric();
                        if !commutative && !p.fields[field].ty.is_list() {
                            hits.push((
                                "L401",
                                e.span,
                                format!("writes '{}' of another entity with a plain assignment: if several entities do this in one system, the last one (in entity order) wins", p.field_label(field)),
                                Some("use += / -= (merged exactly), or call a method on that entity (delivered as a message)".into()),
                            ));
                        }
                    }
                }
                ExprKind::Builtin { f: Builtin::Print, .. } if entity_server => {
                    hits.push(("L403", e.span, "Print inside an entity system forces the whole system to run serially".into(), Some("log from Game.Update, or aggregate counts and print once".into())));
                }
                ExprKind::Call { func, .. } if p.funcs[*func].extern_name.is_some() && !p.funcs[*func].pure_attr && !p.funcs[*func].deferred_attr => {
                    if let Some(systems) = in_systems.get(&fid) {
                        hits.push((
                            "L409",
                            e.span,
                            format!("'{}' is a host function (extern): calling it here makes {} run serially (host calls happen in call order)", p.func_label(*func), systems.join(", ")),
                            Some(if p.funcs[*func].ret == Ty::Void {
                                "mark the extern [Deferred] (its calls are queued per thread and run in row order at the end of the system), or collect the values in fields and call it once from Game.Update".to_string()
                            } else {
                                "collect what it needs in fields and call it once from Game.Update, or mark it [Pure] if it has no effects".to_string()
                            }),
                        ));
                    }
                }
                ExprKind::Builtin { f: Builtin::RandomFloat | Builtin::RandomInt, .. } if client => {
                    hits.push(("L405", e.span, "Random in client code produces values the server never sees".into(), Some("derive client effects from replicated state, or compute the random value on the server".into())));
                }
                ExprKind::Spawn { .. } if entity_server && !discarded.contains(&e.span) => {
                    hits.push((
                        "L407",
                        e.span,
                        "this Spawn's handle is used (or it is inside atomic), so the system runs serially to assign ids in order".into(),
                        Some("Spawn as a statement (handle discarded) lets the system run in parallel".into()),
                    ));
                }
                _ => {}
            });
            if entity_server {
                let is_system = c.ecs.systems.iter().any(|s| s.func == fid);
                nested_queries(&f.body, &mut hits, p, &c.spatial, fid, is_system);
            }
            loop_allocs(&f.body, false, &mut hits);
            // lists stored in entity fields must be allocated per entity: only temporaries count
            hits.retain(|(code, sp, _, _)| {
                *code != "L408" || c.mem.sites.iter().any(|site| site.span == *sp && site.region == crate::memory::Region::Frame)
            });
            for (code, sp, msg, note) in hits {
                self.emit(code, sp, msg, note, None);
            }
        }
        let raw_of = |fid: FieldId| -> Vec<String> {
            let Some(fnet) = c.net.fields.get(&fid) else { return vec![] };
            fnet.leaves.iter().filter(|l| l.quant == crate::net::Quant::FloatRaw).map(|l| if l.path_names.is_empty() { p.fields[fid].name.clone() } else { format!("{}.{}", p.fields[fid].name, l.path_names.join(".")) }).collect()
        };
        for fid in c.net.fields.keys() {
            let raw = raw_of(*fid);
            // a field of a generic class (`T Seed`) is reported only when every instance sends it
            // unquantized (in `Pool<int>` it is an int)
            let shared = p.generic_of.contains_key(&p.fields[*fid].class)
                && (0..p.fields.len()).any(|g| g != *fid && p.fields[g].span == p.fields[*fid].span && p.fields[g].name == p.fields[*fid].name && raw_of(g).is_empty());
            if !raw.is_empty() && !shared {
                self.emit(
                    "L404",
                    p.fields[*fid].span,
                    format!("'{}' is replicated as {} unquantized 32-bit float(s): {}", p.source_field_label(*fid), raw.len(), raw.join(", ")),
                    Some("give it a range (public mut float in -500..500 X) and [Precision(step)] to cut bandwidth".into()),
                    None,
                );
            }
        }
        for s in &c.ecs.systems {
            for &f in &s.hazard_fields {
                let bytes: usize = s.hazards.len();
                let _ = bytes;
                self.emit(
                    "L406",
                    p.fields[f].span,
                    format!("'{}' is written on self and read on other entities in {}: the compiler double-buffers it (one memcpy per tick) to keep results order-independent", p.field_label(f), s.name),
                    None,
                    None,
                );
            }
        }
    }
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn returns_bool(s: &AStmt) -> Option<bool> {
    let inner = match s {
        AStmt::Block(b) if b.stmts.len() == 1 => &b.stmts[0],
        other => other,
    };
    match inner {
        AStmt::Return { value: Some(ast::Expr { kind: ast::ExprKind::Bool(v), .. }), .. } => Some(*v),
        _ => None,
    }
}

/// every statement of `stmts`, nested ones included (not inside lambdas)
fn ast_visit_stmts<'x>(stmts: &'x [AStmt], f: &mut dyn FnMut(&'x AStmt)) {
    for s in stmts {
        f(s);
        match s {
            AStmt::Block(b) | AStmt::Atomic { body: b, .. } => ast_visit_stmts(&b.stmts, f),
            AStmt::If { then, els, .. } => {
                ast_visit_stmts(std::slice::from_ref(&**then), f);
                if let Some(e) = els {
                    ast_visit_stmts(std::slice::from_ref(&**e), f);
                }
            }
            AStmt::While { body, .. } | AStmt::DoWhile { body, .. } | AStmt::Foreach { body, .. } => ast_visit_stmts(std::slice::from_ref(&**body), f),
            AStmt::For { init, body, .. } => {
                ast_visit_stmts(init, f);
                ast_visit_stmts(std::slice::from_ref(&**body), f);
            }
            AStmt::Switch { sections, .. } => {
                for sec in sections {
                    ast_visit_stmts(&sec.body, f);
                }
            }
            _ => {}
        }
    }
}

/// every expression of a statement, nested ones included (lambda bodies too)
fn ast_visit_stmt_exprs(s: &AStmt, f: &mut dyn FnMut(&ast::Expr)) {
    match s {
        AStmt::Block(b) | AStmt::Atomic { body: b, .. } => {
            for x in &b.stmts {
                ast_visit_stmt_exprs(x, f);
            }
        }
        AStmt::Var { init, .. } => {
            if let Some(e) = init {
                ast_visit_expr(e, f);
            }
        }
        AStmt::If { cond, then, els, .. } => {
            ast_visit_expr(cond, f);
            ast_visit_stmt_exprs(then, f);
            if let Some(e) = els {
                ast_visit_stmt_exprs(e, f);
            }
        }
        AStmt::While { cond, body, .. } | AStmt::DoWhile { body, cond, .. } => {
            ast_visit_expr(cond, f);
            ast_visit_stmt_exprs(body, f);
        }
        AStmt::For { init, cond, step, body, .. } => {
            for x in init {
                ast_visit_stmt_exprs(x, f);
            }
            if let Some(c) = cond {
                ast_visit_expr(c, f);
            }
            for e in step {
                ast_visit_expr(e, f);
            }
            ast_visit_stmt_exprs(body, f);
        }
        AStmt::Foreach { iter, body, .. } => {
            ast_visit_expr(iter, f);
            ast_visit_stmt_exprs(body, f);
        }
        AStmt::Return { value: Some(e), .. } | AStmt::Expr(e, _) => ast_visit_expr(e, f),
        AStmt::Switch { scrut, sections, .. } => {
            ast_visit_expr(scrut, f);
            for sec in sections {
                for l in &sec.labels {
                    if let ast::CaseLabel::Case(pat, guard, _) = l {
                        ast_visit_pattern(pat, f);
                        if let Some(g) = guard {
                            ast_visit_expr(g, f);
                        }
                    }
                }
                for x in &sec.body {
                    ast_visit_stmt_exprs(x, f);
                }
            }
        }
        _ => {}
    }
}

fn ast_visit_pattern(p: &ast::Pattern, f: &mut dyn FnMut(&ast::Expr)) {
    match p {
        ast::Pattern::Const(e) | ast::Pattern::Rel(_, e, _) => ast_visit_expr(e, f),
        ast::Pattern::Not(x, _) => ast_visit_pattern(x, f),
        ast::Pattern::And(a, b) | ast::Pattern::Or(a, b) => {
            ast_visit_pattern(a, f);
            ast_visit_pattern(b, f);
        }
        ast::Pattern::Discard(_) => {}
    }
}

fn ast_visit_expr(e: &ast::Expr, f: &mut dyn FnMut(&ast::Expr)) {
    use ast::ExprKind as K;
    f(e);
    match &e.kind {
        K::Interp(parts) => {
            for part in parts {
                if let ast::InterpPart::Expr(x, _) = part {
                    ast_visit_expr(x, f);
                }
            }
        }
        K::Member(b, _, _) | K::Unary(_, b) | K::IncDec(b, _, _) | K::Cast(_, b) | K::Is(b, _, _) | K::Await(b) => ast_visit_expr(b, f),
        K::Call { callee, args, .. } => {
            ast_visit_expr(callee, f);
            for a in args {
                ast_visit_expr(&a.expr, f);
            }
        }
        K::Index(a, ix) => {
            ast_visit_expr(a, f);
            for b in ix {
                ast_visit_expr(b, f);
            }
        }
        K::Binary(_, a, b) | K::Assign(_, a, b) => {
            ast_visit_expr(a, f);
            ast_visit_expr(b, f);
        }
        K::Cond(a, b, c) => {
            ast_visit_expr(a, f);
            ast_visit_expr(b, f);
            ast_visit_expr(c, f);
        }
        K::New { args, inits, .. } => {
            for a in args.iter().flatten() {
                ast_visit_expr(&a.expr, f);
            }
            for (_, x, _) in inits {
                ast_visit_expr(x, f);
            }
        }
        K::NewColl { args, items, .. } => {
            for a in args.iter().flatten() {
                ast_visit_expr(&a.expr, f);
            }
            for (xs, _) in items {
                for x in xs {
                    ast_visit_expr(x, f);
                }
            }
        }
        K::Switch(x, arms) => {
            ast_visit_expr(x, f);
            for a in arms {
                ast_visit_pattern(&a.pat, f);
                if let Some(g) = &a.guard {
                    ast_visit_expr(g, f);
                }
                ast_visit_expr(&a.value, f);
            }
        }
        K::IsPattern(x, pat) => {
            ast_visit_expr(x, f);
            ast_visit_pattern(pat, f);
        }
        K::NewArray { lens, elems, .. } => {
            for l in lens {
                ast_visit_expr(l, f);
            }
            for x in elems.iter().flatten() {
                ast_visit_expr(x, f);
            }
        }
        K::Lambda { body, .. } => match &**body {
            ast::LambdaBody::Expr(x) => ast_visit_expr(x, f),
            ast::LambdaBody::Block(b) => {
                for s in &b.stmts {
                    ast_visit_stmt_exprs(s, f);
                }
            }
        },
        _ => {}
    }
}

/// spans of the names whose variables an expression may change: assignment and ++/-- targets,
/// ref/out arguments and receivers of method calls (their roots: `v` of `v.X[i]`)
fn ast_write_roots(e: &ast::Expr, out: &mut HashSet<Span>) {
    ast_visit_expr(e, &mut |x| ast_write_root(x, out));
}

/// `ast_write_roots` for one expression node (not its children)
fn ast_write_root(x: &ast::Expr, out: &mut HashSet<Span>) {
    use ast::ExprKind as K;
    fn root(e: &ast::Expr) -> Option<Span> {
        match &e.kind {
            K::Name(_) => Some(e.span),
            K::Member(b, _, _) | K::Index(b, _) => root(b),
            _ => None,
        }
    }
    let mut add = |t: &ast::Expr| {
        if let Some(sp) = root(t) {
            out.insert(sp);
        }
    };
    match &x.kind {
        K::Assign(_, t, _) | K::IncDec(t, _, _) => add(t),
        K::Call { callee, args, .. } => {
            for a in args {
                if matches!(a.mode, crate::ast::ParamMode::Ref | crate::ast::ParamMode::Out) {
                    add(&a.expr);
                }
            }
            if let K::Member(b, _, _) = &callee.kind {
                add(b);
            }
        }
        _ => {}
    }
}

fn collect_for_init_vars(stmts: &[AStmt], out: &mut HashSet<u32>) {
    for s in stmts {
        match s {
            AStmt::For { init, body, .. } => {
                for i in init {
                    if let AStmt::Var { name_span, .. } = i {
                        out.insert(name_span.lo);
                    }
                }
                collect_for_init_vars(std::slice::from_ref(body), out);
            }
            AStmt::Block(b) => collect_for_init_vars(&b.stmts, out),
            AStmt::If { then, els, .. } => {
                collect_for_init_vars(std::slice::from_ref(then), out);
                if let Some(e) = els {
                    collect_for_init_vars(std::slice::from_ref(e), out);
                }
            }
            AStmt::While { body, .. } | AStmt::DoWhile { body, .. } | AStmt::Foreach { body, .. } => collect_for_init_vars(std::slice::from_ref(body), out),
            AStmt::Atomic { body, .. } => collect_for_init_vars(&body.stmts, out),
            AStmt::Switch { sections, .. } => {
                for sec in sections {
                    collect_for_init_vars(&sec.body, out);
                }
            }
            _ => {}
        }
    }
}

/// locals read anywhere (writes to a local are not reads)
fn visit_reads(b: &Block, read: &mut HashSet<LocalId>) {
    fn ex(e: &Expr, read: &mut HashSet<LocalId>) {
        match &e.kind {
            ExprKind::Local(l) => {
                read.insert(*l);
            }
            ExprKind::Assign { target, value, .. } => {
                place(target, read);
                ex(value, read);
            }
            ExprKind::IncDec { target, .. } => place(target, read),
            ExprKind::Is { expr, .. } => ex(expr, read),
            _ => {
                for c in crate::sema::children(e) {
                    ex(c, read);
                }
            }
        }
    }
    fn place(e: &Expr, read: &mut HashSet<LocalId>) {
        match &e.kind {
            ExprKind::Local(_) => {}
            ExprKind::Field { base, .. } => place(base, read),
            ExprKind::ListIndex { list, index, .. } => {
                place(list, read);
                ex(index, read);
            }
            _ => ex(e, read),
        }
    }
    crate::ecs::visit_stmts(b, &mut |e| ex(e, read));
    // list iteration variables and loop counters count as used when the loop exists
    fn lists(b: &Block, read: &mut HashSet<LocalId>) {
        for s in &b.stmts {
            match s {
                Stmt::ForList { idx, body, .. } => {
                    read.insert(*idx);
                    lists(body, read);
                }
                Stmt::ForQuery { var, body, .. } => {
                    let _ = var;
                    lists(body, read);
                }
                Stmt::If { then, els, .. } => {
                    lists(then, read);
                    if let Some(e) = els {
                        lists(e, read);
                    }
                }
                Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } => lists(body, read),
                Stmt::Switch { .. } => {
                    for x in s.sub_blocks() {
                        lists(x, read);
                    }
                }
                _ => {}
            }
        }
    }
    lists(b, read);
}

/// Simplified cognitive complexity (SonarSource / clang-tidy): +1 per branch/loop plus nesting.
fn complexity(b: &Block, nest: u32) -> u32 {
    let mut c = 0;
    for s in &b.stmts {
        match s {
            Stmt::If { cond, then, els } => {
                c += 1 + nest + bool_ops(cond);
                c += complexity(then, nest + 1);
                if let Some(e) = els {
                    c += 1 + complexity(e, nest + 1);
                }
            }
            Stmt::Loop { cond, body, .. } => {
                c += 1 + nest + cond.as_ref().map(bool_ops).unwrap_or(0);
                c += complexity(body, nest + 1);
            }
            Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } => c += 1 + nest + complexity(body, nest + 1),
            Stmt::Block(x) | Stmt::Atomic { body: x, .. } => c += complexity(x, nest),
            Stmt::Switch { sections, .. } => {
                c += 1 + nest + sections.iter().map(|(cond, _)| bool_ops(cond).saturating_sub(1)).sum::<u32>();
                for x in s.sub_blocks() {
                    c += complexity(x, nest + 1);
                }
            }
            _ => {}
        }
    }
    c
}

fn bool_ops(e: &Expr) -> u32 {
    let mut n = 0;
    let mut last: Option<BinOp> = None;
    crate::sema::visit_expr(e, &mut |x| {
        if let ExprKind::Binary(op @ (BinOp::And | BinOp::Or), _, _) = x.kind {
            if last != Some(op) {
                n += 1;
            }
            last = Some(op);
        }
    });
    n
}

fn nested_queries(b: &Block, hits: &mut Vec<(&'static str, Span, String, Option<String>)>, p: &Program, spatial: &crate::spatial::SpatialPlan, fid: FuncId, is_system: bool) {
    // a query loop inside an entity system runs once per entity: O(N*M), unless the compiler proved
    // it is a proximity query and visits only neighbouring grid cells (docs 4.12)
    fn walk(b: &Block, hits: &mut Vec<(&'static str, Span, String, Option<String>)>, p: &Program, spatial: &crate::spatial::SpatialPlan, fid: FuncId, is_system: bool) {
        for s in &b.stmts {
            match s {
                Stmt::ForQuery { var, class, body, .. } => {
                    if spatial.find(fid, *var).is_some() {
                        continue;
                    }
                    let sp = first_span(body).unwrap_or_default();
                    let why = spatial.rejected.iter().find(|(f, v, _)| *f == fid && v == var).map(|(_, _, w)| format!("not grid-accelerated: {}", w));
                    let why = why.or_else(|| {
                        (!is_system).then(|| {
                            format!(
                                "not grid-accelerated: distance loops are accelerated in systems (Update, coroutines) and in the methods they call on `this`; '{}' is not one of them (it runs on another entity, or outside a system): move the loop there",
                                p.funcs[fid].name
                            )
                        })
                    });
                    hits.push((
                        "L402",
                        sp,
                        format!("this loop visits every {} for every entity running the system (O(N*M) per tick)", p.classes[*class].name),
                        Some(why.unwrap_or_else(|| {
                            "fine for small counts; a distance test such as `if ((o.Pos - Pos).LengthSq < R * R) { ... }` or an early `if ((o.Pos - Pos).LengthSq >= R * R) { continue; }` lets the compiler visit only nearby entities (docs 4.12)".into()
                        })),
                    ));
                }
                Stmt::If { then, els, .. } => {
                    walk(then, hits, p, spatial, fid, is_system);
                    if let Some(e) = els {
                        walk(e, hits, p, spatial, fid, is_system);
                    }
                }
                Stmt::Loop { body, .. } | Stmt::Block(body) | Stmt::Atomic { body, .. } | Stmt::ForList { body, .. } => walk(body, hits, p, spatial, fid, is_system),
                Stmt::Switch { .. } => {
                    for x in s.sub_blocks() {
                        walk(x, hits, p, spatial, fid, is_system);
                    }
                }
                _ => {}
            }
        }
    }
    walk(b, hits, p, spatial, fid, is_system);
}

fn first_span(b: &Block) -> Option<Span> {
    for s in &b.stmts {
        let mut found = None;
        crate::sema::visit_stmt(s, &mut |e| {
            if found.is_none() && e.span != Span::default() {
                found = Some(e.span);
            }
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

fn loop_allocs(b: &Block, in_loop: bool, hits: &mut Vec<(&'static str, Span, String, Option<String>)>) {
    for s in &b.stmts {
        match s {
            Stmt::Loop { body, .. } | Stmt::ForQuery { body, .. } | Stmt::ForList { body, .. } => loop_allocs(body, true, hits),
            Stmt::If { then, els, .. } => {
                loop_allocs(then, in_loop, hits);
                if let Some(e) = els {
                    loop_allocs(e, in_loop, hits);
                }
            }
            Stmt::Block(x) | Stmt::Atomic { body: x, .. } => loop_allocs(x, in_loop, hits),
            Stmt::Switch { scrut, sections, .. } => {
                if in_loop {
                    let mut chk = |e: &Expr| {
                        if let ExprKind::ListNew { .. } | ExprKind::ListClone { .. } = e.kind {
                            hits.push(("L408", e.span, "a new list is allocated on every iteration".into(), Some("create it once before the loop and Clear() it".into())));
                        }
                    };
                    crate::sema::visit_expr(scrut, &mut chk);
                    for (c, _) in sections {
                        crate::sema::visit_expr(c, &mut chk);
                    }
                }
                for x in s.sub_blocks() {
                    loop_allocs(x, in_loop, hits);
                }
            }
            other if in_loop => crate::sema::visit_stmt(other, &mut |e| {
                if let ExprKind::ListNew { .. } | ExprKind::ListClone { .. } = e.kind {
                    hits.push(("L408", e.span, "a new list is allocated on every iteration".into(), Some("create it once before the loop and Clear() it".into())));
                }
            }),
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// applying fixes
// ---------------------------------------------------------------------------

/// Applies non-overlapping edits (first come, first served). Returns the new text and the number applied.
pub fn apply_fixes(src: &str, fixes: &[&Fix]) -> (String, usize) {
    let mut taken: Vec<Span> = vec![];
    let mut edits: Vec<&Edit> = vec![];
    let mut applied = 0;
    for f in fixes {
        let overlaps = f.edits.iter().any(|e| taken.iter().any(|t| e.span.lo < t.hi && t.lo < e.span.hi || (e.span.lo == t.lo && e.span.hi == t.hi)));
        if overlaps {
            continue;
        }
        for e in &f.edits {
            taken.push(e.span);
            edits.push(e);
        }
        applied += 1;
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.span.lo));
    let mut out = src.to_string();
    for e in edits {
        out.replace_range(e.span.lo as usize..e.span.hi as usize, &e.text);
    }
    (out, applied)
}

/// Repeatedly lints and applies safe fixes (renames in passes of their own) until nothing changes.
/// Never returns code that fails to compile. Returns (new source, number of fixes applied).
pub fn fix_all(src: &str, cfg: &LintConfig) -> (String, usize) {
    let mut src = src.to_string();
    let mut total = 0;
    for _pass in 0..12 {
        let mut d = crate::diag::Diags::default();
        let Some(c) = crate::driver::compile(&src, &mut d) else { break };
        let ds = lint(&c, &src, cfg);
        let mut fixes: Vec<&Fix> = ds.iter().filter_map(|d| d.fix.as_ref()).collect();
        if fixes.is_empty() {
            break;
        }
        if fixes.iter().any(|f| f.title.starts_with("rename")) {
            fixes.retain(|f| f.title.starts_with("rename"));
        }
        let compiles = |s: &str| crate::driver::compile(s, &mut crate::diag::Diags::default()).is_some();
        let (mut new_src, mut n) = apply_fixes(&src, &fixes);
        if n == 0 {
            break;
        }
        if !compiles(&new_src) {
            // one of the fixes breaks the program: keep the ones that compile on their own
            let good: Vec<&Fix> = fixes.iter().copied().filter(|f| compiles(&apply_fixes(&src, &[*f]).0)).collect();
            (new_src, n) = apply_fixes(&src, &good);
            if n > 1 && !compiles(&new_src) {
                (new_src, n) = apply_fixes(&src, &good[..1]);
            }
            if n == 0 {
                break;
            }
        }
        src = new_src;
        total += n;
    }
    (src, total)
}

/// Locals read in each function (computed before optimization, which may propagate them away).
pub fn local_reads(p: &Program) -> Vec<HashSet<LocalId>> {
    p.funcs
        .iter()
        .map(|f| {
            let mut read = HashSet::new();
            visit_reads(&f.body, &mut read);
            read
        })
        .collect()
}

/// `float x = a / b` with int operands (before constant folding hides it).
pub fn int_divisions_as_float(p: &Program) -> Vec<Span> {
    let mut v = vec![];
    for f in &p.funcs {
        // index arithmetic: `n / W` next to `n % W` turns a cell index into coordinates, and the
        // truncation is the point (Draw.Sprite(1, n % W, n / W))
        let mut rems: Vec<(Expr, Expr)> = vec![];
        crate::sema::visit_block(&f.body, &mut |e| {
            if let ExprKind::Binary(BinOp::Rem, a, b) = &e.kind {
                rems.push(((**a).clone(), (**b).clone()));
            }
        });
        crate::sema::visit_block(&f.body, &mut |e| {
            if let ExprKind::Cast(inner) = &e.kind {
                if e.ty == Ty::Float && inner.ty == Ty::Int {
                    if let ExprKind::Binary(BinOp::Div, a, b) = &inner.kind {
                        if !rems.iter().any(|(x, y)| same_operand(a, x) && same_operand(b, y)) {
                            v.push(inner.span);
                        }
                    }
                }
            }
        });
    }
    v
}

/// the same simple operand (a local, a constant, a field of this, a struct member of those)
fn same_operand(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Local(x), ExprKind::Local(y)) => x == y,
        (ExprKind::Int(x), ExprKind::Int(y)) => x == y,
        (ExprKind::Const(x), ExprKind::Const(y)) => x == y,
        (ExprKind::EField { base: ba, field: fa }, ExprKind::EField { base: bb, field: fb }) => fa == fb && (matches!((&ba.kind, &bb.kind), (ExprKind::SelfRef, ExprKind::SelfRef)) || same_operand(ba, bb)),
        (ExprKind::Field { base: ba, sid: sa, idx: ia }, ExprKind::Field { base: bb, sid: sb, idx: ib }) => sa == sb && ia == ib && same_operand(ba, bb),
        (ExprKind::Cast(x), _) => same_operand(x, b),
        (_, ExprKind::Cast(y)) => same_operand(a, y),
        _ => false,
    }
}

/// The functions each entity system runs synchronously (its method, the methods of `this` and the
/// static functions it calls, and what those call): function -> names of the systems.
fn entity_system_callers(p: &Program, c: &crate::driver::Compiled) -> HashMap<FuncId, Vec<String>> {
    let mut out: HashMap<FuncId, Vec<String>> = HashMap::new();
    for s in &c.ecs.systems {
        if s.kind != crate::ecs::SysKind::Entity {
            continue;
        }
        let mut seen: HashSet<FuncId> = HashSet::new();
        let mut todo: Vec<FuncId> = vec![s.func];
        for &a in &s.archetypes {
            todo.push(p.target_of(a, s.func));
        }
        while let Some(f) = todo.pop() {
            if !seen.insert(f) {
                continue;
            }
            crate::sema::visit_block(&p.funcs[f].body, &mut |e| {
                if let ExprKind::Call { func, dispatch, .. } = &e.kind {
                    match dispatch {
                        Dispatch::Static | Dispatch::RemoteRead => todo.push(*func),
                        Dispatch::SelfCall | Dispatch::BaseCall => {
                            todo.push(*func);
                            for &a in &s.archetypes {
                                todo.push(p.target_of(a, *func));
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
        for f in seen {
            let v = out.entry(f).or_default();
            if !v.contains(&s.name) {
                v.push(s.name.clone());
            }
        }
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}
