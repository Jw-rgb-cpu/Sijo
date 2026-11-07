//! `sijoc lsp`: a Language Server Protocol server (IntelliSense for any LSP editor).
//!
//! Features: diagnostics (compiler + linter) on every change, completion, hover with the
//! compiler's planning decisions, go to definition, find references, rename, quick fixes
//! (+ "fix all"), inlay hints (components, network bits, regions, schedule), document outline.

use crate::diag::{Diagnostic, Diags, Level, SourceMap, Span};
use crate::driver::{compile, Compiled};
use crate::ecs::SysKind;
use crate::hir::*;
use crate::json::Json;
use crate::lint::{self, LintConfig, LintDiag};
use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::rc::Rc;

pub fn serve() -> i32 {
    let stdin = io::stdin();
    let mut inp = stdin.lock();
    let mut out = io::stdout();
    let mut srv = Server::default();
    while let Some(msg) = read_msg(&mut inp) {
        let Ok(j) = Json::parse(&msg) else { continue };
        let mut outbox: Vec<Json> = vec![];
        srv.handle(&j, &mut outbox);
        for m in &outbox {
            send(&mut out, m);
        }
        if srv.exit {
            return if srv.shutdown { 0 } else { 1 };
        }
    }
    0
}

fn read_msg(r: &mut impl BufRead) -> Option<String> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let t = line.trim_end();
        if t.is_empty() {
            break;
        }
        if let Some(v) = t.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok();
        }
    }
    let mut buf = vec![0u8; len?];
    r.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

fn send(out: &mut impl Write, j: &Json) {
    let s = j.to_string();
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", s.len(), s);
    let _ = out.flush();
}

/// A compiled project (one file, or every file of a sijo.toml project) seen from one document.
/// Offsets in the compiler's results refer to the combined project text `src`.
#[derive(Clone)]
struct Analysis {
    c: Rc<Compiled>,
    src: Rc<String>,
    sm: Rc<SourceMap>,
    lints: Rc<Vec<LintDiag>>,
    /// index of the requesting document in `sm`
    fi: usize,
}

impl Analysis {
    fn base(&self) -> usize {
        self.sm.files[self.fi].base
    }
    /// this document's text as it was analysed
    fn text(&self) -> &str {
        self.sm.file_text(self.fi)
    }
    fn in_doc(&self, off: usize) -> bool {
        self.sm.file_index(off as u32) == self.fi
    }
    /// range of a combined-text span, relative to the file containing it
    fn range(&self, sp: Span) -> Json {
        let (i, lo) = self.sm.local(sp.lo);
        let f = &self.sm.files[i];
        let hi = (sp.hi as usize).saturating_sub(f.base).min(f.len) as u32;
        range_json(self.sm.file_text(i), Span { lo, hi: hi.max(lo) })
    }
    fn pos(&self, off: usize) -> Json {
        let (i, lo) = self.sm.local(off as u32);
        pos_json(self.sm.file_text(i), lo as usize)
    }
    fn uri(&self, i: usize) -> String {
        path_to_uri(&self.sm.files[i].path)
    }
    /// LSP location of a combined-text span (may be in another file of the project)
    fn location(&self, sp: Span) -> Json {
        let i = self.sm.file_index(sp.lo);
        Json::obj(vec![("uri", Json::Str(self.uri(i))), ("range", self.range(sp))])
    }
}

#[derive(Default)]
struct Doc {
    text: String,
    path: String,
    /// last successful analysis (may be stale while the user is typing)
    good: Option<Analysis>,
}

#[derive(Default)]
pub struct Server {
    docs: HashMap<String, Doc>,
    pub exit: bool,
    pub shutdown: bool,
}

// ---------------------------------------------------------------------------
// positions (LSP uses UTF-16 code units)
// ---------------------------------------------------------------------------

pub fn off_to_pos(text: &str, off: usize) -> (u32, u32) {
    let off = off.min(text.len());
    let line_start = text[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line = text[..line_start].matches('\n').count() as u32;
    let col: usize = text[line_start..off].chars().map(|c| c.len_utf16()).sum();
    (line, col as u32)
}

pub fn pos_to_off(text: &str, line: u32, col: u32) -> usize {
    let mut start = 0usize;
    for _ in 0..line {
        match text[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return text.len(),
        }
    }
    let mut units = 0u32;
    for (i, c) in text[start..].char_indices() {
        if units >= col || c == '\n' {
            return start + i;
        }
        units += c.len_utf16() as u32;
    }
    text.len()
}

fn pos_json(text: &str, off: usize) -> Json {
    let (l, c) = off_to_pos(text, off);
    Json::obj(vec![("line", Json::num(l)), ("character", Json::num(c))])
}

fn range_json(text: &str, sp: Span) -> Json {
    Json::obj(vec![("start", pos_json(text, sp.lo as usize)), ("end", pos_json(text, sp.hi as usize))])
}

fn uri_to_path(uri: &str) -> String {
    let p = uri.strip_prefix("file://").unwrap_or(uri);
    let mut out = Vec::new();
    let b = p.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&p[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn path_to_uri(path: &str) -> String {
    let abs = std::fs::canonicalize(path).map(|p| p.display().to_string()).unwrap_or_else(|_| path.to_string());
    let mut out = String::from("file://");
    for b in abs.bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn same_file(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn severity(l: Level) -> f64 {
    match l {
        Level::Error => 1.0,
        Level::Warning => 2.0,
        Level::Hint => 4.0,
    }
}

// ---------------------------------------------------------------------------
// request dispatch
// ---------------------------------------------------------------------------

impl Server {
    pub fn handle(&mut self, j: &Json, out: &mut Vec<Json>) {
        let method = j.get("method").as_str().unwrap_or("").to_string();
        let id = j.get("id").clone();
        let params = j.get("params");
        let result: Option<Json> = match method.as_str() {
            "initialize" => Some(Json::obj(vec![
                (
                    "capabilities",
                    Json::obj(vec![
                        ("textDocumentSync", Json::num(1)),
                        ("hoverProvider", Json::Bool(true)),
                        ("definitionProvider", Json::Bool(true)),
                        ("referencesProvider", Json::Bool(true)),
                        ("renameProvider", Json::Bool(true)),
                        ("documentSymbolProvider", Json::Bool(true)),
                        ("codeActionProvider", Json::Bool(true)),
                        ("inlayHintProvider", Json::Bool(true)),
                        ("completionProvider", Json::obj(vec![("triggerCharacters", Json::Arr(vec![Json::str(".")]))])),
                    ]),
                ),
                ("serverInfo", Json::obj(vec![("name", Json::str("sijoc")), ("version", Json::str(env!("CARGO_PKG_VERSION")))])),
            ])),
            "initialized" | "$/cancelRequest" | "workspace/didChangeConfiguration" | "textDocument/didSave" => None,
            "shutdown" => {
                self.shutdown = true;
                Some(Json::Null)
            }
            "exit" => {
                self.exit = true;
                None
            }
            "textDocument/didOpen" => {
                let td = params.get("textDocument");
                let uri = td.get("uri").as_str().unwrap_or("").to_string();
                let text = td.get("text").as_str().unwrap_or("").to_string();
                self.update(&uri, text, out);
                None
            }
            "textDocument/didChange" => {
                let uri = params.path(&["textDocument", "uri"]).as_str().unwrap_or("").to_string();
                if let Some(ch) = params.get("contentChanges").as_arr().last() {
                    let text = ch.get("text").as_str().unwrap_or("").to_string();
                    self.update(&uri, text, out);
                }
                None
            }
            "textDocument/didClose" => {
                let uri = params.path(&["textDocument", "uri"]).as_str().unwrap_or("").to_string();
                self.docs.remove(&uri);
                None
            }
            "textDocument/hover" => Some(self.hover(params)),
            "textDocument/definition" => Some(self.definition(params)),
            "textDocument/references" => Some(self.references(params)),
            "textDocument/rename" => match self.rename(params) {
                Ok(r) => Some(r),
                Err(msg) => {
                    // a rename that cannot be done says why (the editor shows the message)
                    if !id.is_null() {
                        out.push(Json::obj(vec![
                            ("jsonrpc", Json::str("2.0")),
                            ("id", id),
                            ("error", Json::obj(vec![("code", Json::num(-32803)), ("message", Json::Str(msg))])),
                        ]));
                    }
                    return;
                }
            },
            "textDocument/completion" => Some(self.completion(params)),
            "textDocument/documentSymbol" => Some(self.symbols(params)),
            "textDocument/codeAction" => Some(self.code_actions(params)),
            "textDocument/inlayHint" => Some(self.inlay_hints(params)),
            _ => {
                if !id.is_null() {
                    out.push(Json::obj(vec![
                        ("jsonrpc", Json::str("2.0")),
                        ("id", id),
                        ("error", Json::obj(vec![("code", Json::num(-32601)), ("message", Json::str(format!("method not found: {}", method)))])),
                    ]));
                }
                return;
            }
        };
        if let (Some(r), false) = (result, id.is_null()) {
            out.push(Json::obj(vec![("jsonrpc", Json::str("2.0")), ("id", id), ("result", r)]));
        }
    }

    fn update(&mut self, uri: &str, text: String, out: &mut Vec<Json>) {
        let path = uri_to_path(uri);
        {
            let doc = self.docs.entry(uri.to_string()).or_default();
            doc.text = text.clone();
            doc.path = path.clone();
        }
        // the project this document belongs to: all files of a sijo.toml project (open documents
        // use the editor's text), or just this document
        let root = crate::driver::project_root(std::path::Path::new(&path));
        let mut files: Vec<(String, String)> = vec![];
        if let Some(r) = &root {
            let disk = crate::driver::project_files(&[r.display().to_string()]);
            for f in disk {
                let fp = f.display().to_string();
                let open = self.docs.values().find(|d| same_file(&d.path, &fp));
                let src = match open {
                    Some(d) => d.text.clone(),
                    None => std::fs::read_to_string(&f).unwrap_or_default(),
                };
                files.push((fp, src));
            }
            if !files.iter().any(|(fp, _)| same_file(fp, &path)) {
                files.push((path.clone(), text.clone())); // a new, not yet saved file
            }
        } else {
            files.push((path.clone(), text.clone()));
        }
        let sm = SourceMap::new(files);
        let mut d = Diags::default();
        // sijo.toml's `order` changes the schedule the hovers show
        let order = root.as_ref().map(|r| crate::driver::project_config(&[r.display().to_string()]).map(|c| c.order).unwrap_or_default()).unwrap_or_default();
        let compiled = crate::driver::compile_with(&sm.text, &mut d, &crate::driver::Options { order, ..Default::default() });
        let mut diags: Vec<Diagnostic> = d.list.clone();
        // the .sijolint that applies, as `sijoc lint/check/run` read it: an error in it is shown at
        // the top of this document (an unknown rule: the file is ignored here, the commands stop)
        let me = sm.files.iter().position(|f| f.path == path).map(|i| sm.files[i].base).unwrap_or(0);
        let cfg = match LintConfig::for_file(std::path::Path::new(&path)) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("{} (the language server ignores the file; sijoc lint, check and run stop)", e);
                diags.push(Diagnostic { level: Level::Error, code: "sijolint", msg, span: Span::new(me, me), notes: vec![] });
                LintConfig::default()
            }
        };
        let sm = Rc::new(sm);
        let mut analysis: Option<Analysis> = None;
        if let Some(c) = compiled {
            let lints = lint::lint(&c, &sm.text, &cfg);
            diags.extend(lints.iter().map(|l| l.to_diagnostic()));
            analysis = Some(Analysis { c: Rc::new(c), src: Rc::new(sm.text.clone()), sm: sm.clone(), lints: Rc::new(lints), fi: 0 });
        }
        // every open document of the project shares the new analysis
        if let Some(a) = &analysis {
            for doc in self.docs.values_mut() {
                if let Some(i) = sm.files.iter().position(|f| same_file(&f.path, &doc.path)) {
                    let fresh = root.is_some() || doc.path == path;
                    if fresh {
                        doc.good = Some(Analysis { fi: i, ..a.clone() });
                    }
                }
            }
        }
        // publish per file (an empty list clears old diagnostics)
        for (i, f) in sm.files.iter().enumerate() {
            let ftext = sm.file_text(i);
            let items: Vec<Json> = diags
                .iter()
                .filter(|dg| sm.file_index(dg.span.lo) == i)
                .map(|dg| {
                    let mut msg = dg.msg.clone();
                    for n in &dg.notes {
                        msg.push('\n');
                        msg.push_str(n);
                    }
                    let lo = (dg.span.lo as usize).saturating_sub(f.base).min(f.len);
                    let hi = (dg.span.hi as usize).saturating_sub(f.base).min(f.len).max(lo);
                    Json::obj(vec![
                        ("range", range_json(ftext, Span::new(lo, hi))),
                        ("severity", Json::num(severity(dg.level))),
                        ("code", Json::str(dg.code)),
                        ("source", Json::str("sijoc")),
                        ("message", Json::Str(msg)),
                    ])
                })
                .collect();
            let furi = if f.path == path { uri.to_string() } else { path_to_uri(&f.path) };
            out.push(Json::obj(vec![
                ("jsonrpc", Json::str("2.0")),
                ("method", Json::str("textDocument/publishDiagnostics")),
                ("params", Json::obj(vec![("uri", Json::Str(furi)), ("diagnostics", Json::Arr(items))])),
            ]));
        }
    }

    /// (analysis, byte offset in the analysed source) for a position request
    fn at(&self, params: &Json) -> Option<(&Analysis, usize)> {
        let uri = params.path(&["textDocument", "uri"]).as_str()?;
        let doc = self.docs.get(uri)?;
        let a = doc.good.as_ref()?;
        let line = params.path(&["position", "line"]).as_i64()? as u32;
        let ch = params.path(&["position", "character"]).as_i64()? as u32;
        Some((a, a.base() + pos_to_off(a.text(), line, ch)))
    }

    fn hover(&self, params: &Json) -> Json {
        let Some((a, off)) = self.at(params) else { return Json::Null };
        let Some((sp, sym)) = a.c.prog.index.at(off as u32) else { return Json::Null };
        let md = describe(a, sym);
        Json::obj(vec![
            ("contents", Json::obj(vec![("kind", Json::str("markdown")), ("value", Json::Str(md))])),
            ("range", a.range(sp)),
        ])
    }

    fn definition(&self, params: &Json) -> Json {
        let Some((a, off)) = self.at(params) else { return Json::Null };
        let uri = params.path(&["textDocument", "uri"]).clone();
        let Some((_, sym)) = a.c.prog.index.at(off as u32) else { return Json::Null };
        match a.c.prog.index.defs.get(&sym) {
            Some(sp) if a.in_doc(sp.lo as usize) => Json::obj(vec![("uri", uri), ("range", a.range(*sp))]),
            Some(sp) => a.location(*sp),
            None => Json::Null,
        }
    }

    fn references(&self, params: &Json) -> Json {
        let Some((a, off)) = self.at(params) else { return Json::Arr(vec![]) };
        let uri = params.path(&["textDocument", "uri"]).clone();
        let Some((_, sym)) = a.c.prog.index.at(off as u32) else { return Json::Arr(vec![]) };
        // an interface method, its implementations and the calls through either are one method
        Json::Arr(
            occurrences(&a.c.prog, sym)
                .into_iter()
                .map(|sp| if a.in_doc(sp.lo as usize) { Json::obj(vec![("uri", uri.clone()), ("range", a.range(sp))]) } else { a.location(sp) })
                .collect(),
        )
    }

    fn rename(&self, params: &Json) -> Result<Json, String> {
        let uri = params.path(&["textDocument", "uri"]).as_str().unwrap_or("").to_string();
        let doc = self.docs.get(&uri).ok_or("this document is not open")?;
        let a = doc.good.as_ref().ok_or("the program does not compile yet: fix the errors first, then rename")?;
        if a.text() != doc.text {
            // the positions of the last good analysis no longer match the text
            return Err("the program does not compile with the latest edits: fix the errors first, then rename".into());
        }
        let (a, off) = self.at(params).ok_or("nothing to rename here")?;
        let new = params.get("newName").as_str().unwrap_or("").trim();
        let (sp, sym) = a.c.prog.index.at(off as u32).ok_or("nothing to rename here: put the cursor on a name declared in the program")?;
        let old = a.src[sp.lo as usize..sp.hi as usize].to_string();
        if !lint::is_identifier(new) {
            return Err(format!("'{}' is not a valid name: a name starts with a letter or '_' and has only letters, digits and '_'", new));
        }
        if lint::is_reserved_name(new) {
            let why = if new.starts_with("__") { "names starting with '__' are reserved for the compiler" } else { "it is a keyword, a type or a built-in name" };
            return Err(format!("cannot rename '{}' to '{}': {}", old, new, why));
        }
        if new == old {
            return Ok(Json::obj(vec![("changes", Json::Obj(vec![]))]));
        }
        // every occurrence of the symbol and of the methods that share its name (interface method,
        // implementations, overrides), as written in the source
        let spans: Vec<Span> = occurrences(&a.c.prog, sym).into_iter().filter(|s| a.src[s.lo as usize..s.hi as usize] == old).collect();
        check_rename(a, &spans, &old, new)?;
        let mut by_file: Vec<(String, Vec<Json>)> = vec![];
        for s in spans {
            let fu = if a.in_doc(s.lo as usize) { uri.clone() } else { a.uri(a.sm.file_index(s.lo)) };
            let edit = Json::obj(vec![("range", a.range(s)), ("newText", Json::str(new))]);
            match by_file.iter_mut().find(|(u, _)| *u == fu) {
                Some((_, v)) => v.push(edit),
                None => by_file.push((fu, vec![edit])),
            }
        }
        Ok(Json::obj(vec![("changes", Json::Obj(by_file.into_iter().map(|(u, v)| (u, Json::Arr(v))).collect()))]))
    }

    fn symbols(&self, params: &Json) -> Json {
        let Some(uri) = params.path(&["textDocument", "uri"]).as_str() else { return Json::Arr(vec![]) };
        let Some(a) = self.docs.get(uri).and_then(|d| d.good.as_ref()) else { return Json::Arr(vec![]) };
        let mut out = vec![];
        for d in a.c.ast.items.iter().filter(|d| a.in_doc(d.span.lo as usize)) {
            let kind = match d.kind {
                crate::ast::DeclKind::Struct => 23,
                crate::ast::DeclKind::Enum => 10,
                crate::ast::DeclKind::Class => 5,
                crate::ast::DeclKind::Interface => 11,
                crate::ast::DeclKind::Component => 5,
                crate::ast::DeclKind::Delegate => 12,
            };
            let mut children = vec![];
            let mut end = d.body_open.hi;
            for (n, _, sp) in &d.variants {
                children.push(sym_json(a, n, 22, *sp, *sp));
                end = end.max(sp.hi);
            }
            for m in &d.members {
                match m {
                    crate::ast::Member::Field(f) => {
                        let k = if f.mods.is_const { 14 } else { 8 };
                        children.push(sym_json(a, &f.name, k, f.span, f.span));
                        end = end.max(f.span.hi);
                    }
                    crate::ast::Member::Method(md) => {
                        let k = match md.kind {
                            crate::ast::MethodKind::Ctor => 9,
                            crate::ast::MethodKind::Operator(_) => 25,
                            _ if md.is_property => 7,
                            _ => 6,
                        };
                        let full = md.body.as_ref().map(|b| md.span.to(b.span)).unwrap_or(md.span);
                        children.push(sym_json(a, &md.name, k, full, md.name_span));
                        end = end.max(full.hi);
                    }
                }
            }
            let mut sym = sym_json(a, &d.name, kind, Span { lo: d.span.lo, hi: end }, d.name_span);
            if let Json::Obj(kv) = &mut sym {
                kv.push(("children".into(), Json::Arr(children)));
            }
            out.push(sym);
        }
        Json::Arr(out)
    }

    fn code_actions(&self, params: &Json) -> Json {
        let Some(uri) = params.path(&["textDocument", "uri"]).as_str() else { return Json::Arr(vec![]) };
        let Some(doc) = self.docs.get(uri) else { return Json::Arr(vec![]) };
        let Some(a) = doc.good.as_ref() else { return Json::Arr(vec![]) };
        if a.text() != doc.text {
            return Json::Arr(vec![]); // stale: edits would land in the wrong place
        }
        let r = params.get("range");
        let pos = |k: &str| (a.base() + pos_to_off(a.text(), r.path(&[k, "line"]).as_i64().unwrap_or(0) as u32, r.path(&[k, "character"]).as_i64().unwrap_or(0) as u32)) as u32;
        let (lo, hi) = (pos("start"), pos("end"));
        let mut actions = vec![];
        for d in a.lints.iter() {
            let Some(fix) = &d.fix else { continue };
            if d.span.hi < lo || d.span.lo > hi || !a.in_doc(d.span.lo as usize) {
                continue;
            }
            if fix.edits.iter().any(|e| !a.in_doc(e.span.lo as usize)) {
                continue;
            }
            let edits: Vec<Json> = fix.edits.iter().map(|e| Json::obj(vec![("range", a.range(e.span)), ("newText", Json::str(e.text.clone()))])).collect();
            actions.push(Json::obj(vec![
                ("title", Json::str(format!("{} ({})", fix.title, d.code))),
                ("kind", Json::str("quickfix")),
                ("edit", Json::obj(vec![("changes", Json::Obj(vec![(uri.to_string(), Json::Arr(edits))]))])),
            ]));
        }
        if a.lints.iter().any(|d| d.fix.is_some() && a.in_doc(d.span.lo as usize)) {
            let cfg = LintConfig::for_file(std::path::Path::new(&doc.path)).unwrap_or_default();
            let (fixed, n) = lint::fix_all(&a.src, &cfg);
            let parts = SourceMap::split(&fixed);
            let mine = parts.get(a.fi).cloned().unwrap_or_default();
            if n > 0 && parts.len() == a.sm.files.len() && mine != a.text() {
                let whole = Span { lo: 0, hi: a.text().len() as u32 };
                let edit = Json::obj(vec![("range", range_json(a.text(), whole)), ("newText", Json::Str(mine))]);
                actions.push(Json::obj(vec![
                    ("title", Json::str(format!("Fix all auto-fixable lint issues ({} fixes)", n))),
                    ("kind", Json::str("source.fixAll")),
                    ("edit", Json::obj(vec![("changes", Json::Obj(vec![(uri.to_string(), Json::Arr(vec![edit]))]))])),
                ]));
            }
        }
        Json::Arr(actions)
    }

    fn inlay_hints(&self, params: &Json) -> Json {
        let Some(uri) = params.path(&["textDocument", "uri"]).as_str() else { return Json::Arr(vec![]) };
        let Some(doc) = self.docs.get(uri) else { return Json::Arr(vec![]) };
        let Some(a) = doc.good.as_ref() else { return Json::Arr(vec![]) };
        if a.text() != doc.text {
            return Json::Arr(vec![]);
        }
        let hints = inlay_hints(a);
        Json::Arr(
            hints
                .into_iter()
                .filter(|(off, _, _)| a.in_doc(*off))
                .map(|(off, label, kind)| Json::obj(vec![("position", a.pos(off)), ("label", Json::Str(label)), ("kind", Json::num(kind)), ("paddingLeft", Json::Bool(true))]))
                .collect(),
        )
    }

    fn completion(&self, params: &Json) -> Json {
        let Some(uri) = params.path(&["textDocument", "uri"]).as_str() else { return Json::Arr(vec![]) };
        let Some(doc) = self.docs.get(uri) else { return Json::Arr(vec![]) };
        let line = params.path(&["position", "line"]).as_i64().unwrap_or(0) as u32;
        let ch = params.path(&["position", "character"]).as_i64().unwrap_or(0) as u32;
        let off = pos_to_off(&doc.text, line, ch);
        let items = match doc.good.as_ref() {
            Some(a) => complete(a, &doc.text, off, a.base() + off),
            None => keyword_items(),
        };
        Json::obj(vec![("isIncomplete", Json::Bool(false)), ("items", Json::Arr(items))])
    }
}

/// Where a symbol is written, together with the methods that share its name (an interface method,
/// its implementations in classes, base classes and structs, overrides; see `lint::related_methods`).
fn occurrences(p: &Program, sym: Sym) -> Vec<Span> {
    let mut v: Vec<Span> = lint::related_symbols(p, sym).into_iter().flat_map(|s| p.index.occurrences(s)).collect();
    v.sort_by_key(|s| (s.lo, s.hi));
    v.dedup();
    v
}

/// A rename must leave a program that compiles (the new name may not clash with a declaration in the
/// same scope: E0104 ...) and in which every other name still means what it meant (a local renamed
/// to a field's name would capture the field's uses). Checked by compiling the renamed program and
/// comparing its symbol index with the current one.
fn check_rename(a: &Analysis, spans: &[Span], old: &str, new: &str) -> Result<(), String> {
    let mut edits = spans.to_vec();
    edits.sort_by_key(|s| s.lo);
    edits.dedup();
    let mut text = String::with_capacity(a.src.len() + edits.len() * new.len());
    let mut last = 0usize;
    for s in &edits {
        text.push_str(&a.src[last..s.lo as usize]);
        text.push_str(new);
        last = s.hi as usize;
    }
    text.push_str(&a.src[last..]);
    let delta = new.len() as i64 - old.len() as i64;
    let shift = |off: u32| -> u32 { (off as i64 + edits.iter().filter(|s| s.hi <= off).count() as i64 * delta) as u32 };
    let where_old = |sp: Span| {
        let (file, line, _) = a.sm.position(sp.lo);
        let file = std::path::Path::new(&file).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(file);
        format!("{}:{}", file, line)
    };
    let mut d = Diags::default();
    let Some(c) = compile(&text, &mut d) else {
        let parts = SourceMap::split(&text);
        let sm2 = SourceMap::new(a.sm.files.iter().map(|f| f.path.clone()).zip(parts).collect());
        let msg = match d.list.iter().find(|x| x.level == Level::Error) {
            Some(e) => {
                let (file, line, _) = sm2.position(e.span.lo);
                let file = std::path::Path::new(&file).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(file);
                format!("{}: {} ({}:{})", e.code, e.msg, file, line)
            }
            None => "the renamed program does not compile".into(),
        };
        return Err(format!("cannot rename '{}' to '{}': {}", old, new, msg));
    };
    // same references, each to the "same" declaration (ids may differ, the grouping may not)
    let mut now: HashMap<(u32, u32), Vec<Sym>> = HashMap::new();
    for (sp, s) in &c.prog.index.refs {
        now.entry((sp.lo, sp.hi)).or_default().push(*s);
    }
    let mut taken: HashMap<(u32, u32), usize> = HashMap::new();
    let mut fwd: HashMap<Sym, Sym> = HashMap::new();
    let mut back: HashMap<Sym, Sym> = HashMap::new();
    for (sp, s) in &a.c.prog.index.refs {
        let key = (shift(sp.lo), shift(sp.hi));
        let k = taken.entry(key).or_insert(0);
        let t = now.get(&key).and_then(|v| v.get(*k)).copied();
        *k += 1;
        let shown = || text.get(key.0 as usize..key.1 as usize).unwrap_or("").to_string();
        let Some(t) = t else {
            return Err(format!("cannot rename '{}' to '{}': '{}' at {} would no longer refer to its declaration", old, new, shown(), where_old(*sp)));
        };
        if *fwd.entry(*s).or_insert(t) != t || *back.entry(t).or_insert(*s) != *s {
            return Err(format!("cannot rename '{}' to '{}': '{}' at {} would then refer to a different declaration", old, new, shown(), where_old(*sp)));
        }
    }
    if c.prog.index.refs.len() != a.c.prog.index.refs.len() {
        return Err(format!("cannot rename '{}' to '{}': the new name would change what other code refers to", old, new));
    }
    Ok(())
}

fn sym_json(a: &Analysis, name: &str, kind: u32, full: Span, sel: Span) -> Json {
    Json::obj(vec![("name", Json::str(name)), ("kind", Json::num(kind)), ("range", a.range(full)), ("selectionRange", a.range(sel))])
}

// ---------------------------------------------------------------------------
// hover text: what the compiler knows and decided about a symbol
// ---------------------------------------------------------------------------

fn field_insights(a: &Analysis, f: FieldId) -> Vec<String> {
    let c = &a.c;
    let p = &c.prog;
    let mut v = vec![];
    if c.ecs.is_dead(f) {
        v.push("**storage removed** — the field is never read (dead-field elimination)".into());
        return v;
    }
    if let Some(k) = c.ecs.comp_of(f) {
        let comp = &c.ecs.components[k];
        let fields: Vec<&str> = comp.fields.iter().map(|&x| p.fields[x].name.as_str()).collect();
        let sig = if comp.cold { "cold (no system touches it)".to_string() } else { comp.signature.join(", ") };
        v.push(format!("**ECS**: component `{}` {{ {} }} — accessed by {}", comp.cname, fields.join(", "), sig));
    }
    match c.net.fields.get(&f) {
        Some(n) => {
            let leaves: Vec<String> = n
                .leaves
                .iter()
                .map(|l| format!("{}{} bits ({})", if l.path_names.is_empty() { String::new() } else { format!("{}: ", l.path_names.join(".")) }, l.quant.bits(), l.source))
                .collect();
            v.push(format!("**network**: replicated, {} bits — {}", n.bits, leaves.join("; ")));
        }
        None => v.push("**network**: server-only (no client code reads it)".into()),
    }
    if let Some((lo, hi)) = c.net.clamp.get(&f) {
        v.push(format!("**range contract**: every write is clamped to [{}, {}]", lo, hi));
    }
    for s in &c.ecs.systems {
        if s.hazard_fields.contains(&f) {
            v.push(format!("**hazard**: written on self and read on other entities in `{}` → double-buffered", s.name));
        }
        if let Some(comm) = s.eff.remote_writes.get(&f) {
            v.push(format!("written on other entities by `{}` → deferred {}", s.name, if *comm { "ADD (merged exactly)" } else { "SET (last writer wins)" }));
        }
    }
    v
}

/// ` in 0..100` (a range type's contract, as declared), or nothing
fn range_txt(r: Option<(f64, f64)>) -> String {
    let num = |v: f64| if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{}", v) };
    match r {
        Some((lo, hi)) => format!(" in {}..{}", num(lo), num(hi)),
        None => String::new(),
    }
}

/// a local or parameter as declared: `mut int in -5..5 steer`, `mut own List<int> xs`, `ref int v`
fn local_sig(p: &Program, loc: &Local) -> String {
    let mut s = String::new();
    if loc.is_const {
        s.push_str("const ");
    }
    if loc.is_mut {
        s.push_str("mut ");
    }
    s.push_str(match loc.kind {
        LocalKind::Param(crate::ast::ParamMode::Ref) => "ref ",
        LocalKind::Param(crate::ast::ParamMode::Out) => "out ",
        LocalKind::Param(crate::ast::ParamMode::Own) => "own ",
        _ => "",
    });
    format!("{}{}{} {}", s, p.ty_name(&loc.ty), range_txt(loc.range), loc.name)
}

/// the declaration of a function in the syntax tree (modifiers the program does not keep)
fn ast_method(a: &Analysis, f: FuncId) -> Option<&crate::ast::MethodDecl> {
    let sp = a.c.prog.funcs[f].name_span;
    a.c.ast.items.iter().flat_map(|d| d.members.iter()).find_map(|m| match m {
        crate::ast::Member::Method(md) if md.name_span == sp => Some(md),
        _ => None,
    })
}

/// for a member of a generic class instance: which declaration it comes from and the other instances
fn generic_note(p: &Program, cl: ClassId) -> String {
    match p.generic_of.get(&cl) {
        Some(&g) => {
            let (name, insts) = &p.generic_classes[g];
            let names: Vec<String> = insts.iter().map(|&i| format!("`{}`", p.classes[i].name)).collect();
            format!("\n\ndeclared in the generic class `{}`, shown for `{}` (instances: {})", name, p.classes[cl].name, names.join(", "))
        }
        None => String::new(),
    }
}

fn describe(a: &Analysis, sym: Sym) -> String {
    let c = &a.c;
    let p = &c.prog;
    let code = |s: String| format!("```sijo\n{}\n```", s);
    match sym {
        Sym::Field(f) => {
            let fd = &p.fields[f];
            let m = if fd.is_mut { "mut " } else { "" };
            let mut s = code(format!("(field) {}{}{} {}", m, p.ty_name(&fd.ty), range_txt(fd.attrs.range), p.field_label(f)));
            s.push_str(&generic_note(p, fd.class));
            for line in field_insights(a, f) {
                s.push_str(&format!("\n\n{}", line));
            }
            s
        }
        Sym::SField(st, i) => code(format!("(field) {} {}.{}", p.ty_name(&p.structs[st].fields[i].ty), p.structs[st].name, p.structs[st].fields[i].name)),
        Sym::Const(k) => {
            let kd = &p.consts[k];
            let v = kd.value.as_ref().map(|v| format!(" = {}", crate::opt::show_val(p, v))).unwrap_or_default();
            format!("{}\n\nevaluated at compile time", code(format!("(const) {} {}{}", p.ty_name(&kd.ty), kd.name, v)))
        }
        Sym::Variant(e, i) => code(format!("(enum member) {}.{} = {}", p.enums[e].name, p.enums[e].variants[i].0, p.enums[e].variants[i].1)),
        Sym::Struct(s) => {
            let sd = &p.structs[s];
            let fields: Vec<String> = sd.fields.iter().map(|f| format!("{} {}", p.ty_name(&f.ty), f.name)).collect();
            format!("{}\n\nvalue type (copied, never heap-allocated): {{ {} }}", code(format!("struct {}", sd.name)), fields.join("; "))
        }
        Sym::Enum(e) => {
            let vs: Vec<String> = p.enums[e].variants.iter().map(|(n, v)| format!("{} = {}", n, v)).collect();
            code(format!("enum {} {{ {} }}", p.enums[e].name, vs.join(", ")))
        }
        Sym::Class(cl) => {
            let cd = &p.classes[cl];
            if cd.is_static {
                return code(format!("static class {}", cd.name));
            }
            if cd.is_interface {
                // a generic interface: the index names its first instance; show them all
                let mut insts: Vec<ClassId> = (0..p.classes.len()).filter(|i| p.iface_family.get(i).is_some() && p.iface_family.get(i) == p.iface_family.get(&cl)).collect();
                if insts.is_empty() {
                    insts.push(cl);
                }
                let ext: Vec<String> = cd.ifaces.iter().map(|&i| p.classes[i].name.clone()).collect();
                let name = if insts.len() > 1 || p.iface_family.contains_key(&cl) { format!("{}<...>", cd.name.split('<').next().unwrap_or(&cd.name)) } else { cd.name.clone() };
                let head = if ext.is_empty() || insts.len() > 1 { format!("interface {}", name) } else { format!("interface {} : {}", name, ext.join(", ")) };
                let mut s = code(head);
                if p.iface_family.contains_key(&cl) {
                    let names: Vec<String> = insts.iter().map(|&i| format!("`{}`", p.classes[i].name)).collect();
                    s.push_str(&format!("\n\ngeneric: one interface per type argument, used here as {}", names.join(", ")));
                }
                let mut archs: Vec<ClassId> = insts.iter().flat_map(|&i| p.archetypes_of(i)).collect();
                archs.sort();
                archs.dedup();
                let names: Vec<String> = archs.iter().map(|&a| format!("`{}`", p.classes[a].name)).collect();
                s.push_str(&if names.is_empty() { "\n\nno entity class implements it yet".to_string() } else { format!("\n\nimplemented by {} (calls dispatch per archetype)", names.join(", ")) });
                return s;
            }
            let base = cd.base.map(|b| p.classes[b].name.clone()).unwrap_or("Entity".into());
            // a generic class: every instance is an entity class of its own
            if let Some(&g) = p.generic_of.get(&cl) {
                let (name, insts) = &p.generic_classes[g];
                let names: Vec<String> = insts.iter().map(|&i| format!("`{}`", p.classes[i].name)).collect();
                let mut s = code(format!("(generic entity class) {}", name));
                s.push_str(&format!("\n\n{} instance(s), each an entity class with its own archetype: {}", insts.len(), names.join(", ")));
                for &i in insts {
                    let b = p.classes[i].base.map(|b| p.classes[b].name.clone()).unwrap_or("Entity".into());
                    s.push_str(&format!("\n\n**`{}`** : {}", p.classes[i].name, b));
                    for arch in p.archetypes_of(i) {
                        if let Some(comps) = c.ecs.arch_comps.get(&arch) {
                            let cn: Vec<String> = comps.iter().map(|&k| c.ecs.components[k].cname.clone()).collect();
                            s.push_str(&format!("\n- archetype `{}`: {}", p.classes[arch].name, cn.join(", ")));
                        }
                    }
                }
                return s;
            }
            let mut s = if cd.is_component {
                code(format!("(component) {}", cd.name)) + "\n\nadded and removed at run time: `e.Add(new C { ... })`, `e.Remove<C>()` (applied at the end of the system); its rows live in the tables below"
            } else {
                code(format!("(entity class) {} : {}", cd.name, base))
            };
            for arch in p.archetypes_of(cl) {
                if let Some(comps) = c.ecs.arch_comps.get(&arch) {
                    let names: Vec<String> = comps.iter().map(|&k| c.ecs.components[k].cname.clone()).collect();
                    s.push_str(&format!("\n\n**archetype `{}`** (SoA table): {}", p.classes[arch].name, names.join(", ")));
                }
            }
            if p.archetypes_of(cl).iter().any(|x| c.ecs.replicated_archetypes.contains(x)) {
                s.push_str("\n\nspawns/despawns are replicated to clients");
            }
            s
        }
        Sym::Func(f) => {
            let fd = &p.funcs[f];
            let params: Vec<String> = fd.params.iter().map(|&l| local_sig(p, &fd.locals[l])).collect();
            let md = ast_method(a, f);
            // the modifiers that say what the code does, in the order they are written
            let mut mods = String::new();
            if matches!(fd.kind, FuncKind::Static(_) | FuncKind::StructStatic(_)) {
                mods.push_str("static ");
            }
            if fd.extern_name.is_some() {
                mods.push_str("extern ");
            }
            if fd.ctx == Ctx::Client {
                mods.push_str("client ");
            }
            if p.async_funcs.contains(&f) {
                mods.push_str("async ");
            }
            if matches!(fd.kind, FuncKind::StructMethod(_)) && md.map(|m| m.mods.is_mut).unwrap_or(fd.writes_self) {
                mods.push_str("mut ");
            }
            if md.map(|m| m.mods.is_own).unwrap_or(fd.ret.is_list()) {
                mods.push_str("own ");
            }
            let mut s = code(format!("{}{}{} {}({})", mods, p.ty_name(&fd.ret), range_txt(fd.ret_range), p.func_label(f), params.join(", ")));
            if let FuncKind::EntityMethod(cl) | FuncKind::Static(cl) = fd.kind {
                s.push_str(&generic_note(p, cl));
            }
            if let Some(cname) = &fd.extern_name {
                s.push_str(&format!(
                    "\n\n**host function** (C: `{}`), defined by the program that embeds the game. {}",
                    cname,
                    if fd.pure_attr {
                        "Declared [Pure]: may run in parallel."
                    } else if fd.deferred_attr {
                        "Declared [Deferred]: calls are queued with their arguments and made in row order at the end of the stage, so the systems calling it stay parallel (on the client it is called at once)."
                    } else {
                        "Treated as an external effect: systems calling it run serially and it cannot be used inside atomic ([Deferred] on a void extern keeps them parallel)."
                    }
                ));
                return s;
            }
            // what a call can run: on each archetype, the method this name resolves to there (an
            // interface or abstract method has no body of its own, or a default implementation the
            // archetype may use; a virtual one may be overridden)
            let mut eff = crate::effects::Effects::default();
            let mut targets: Vec<FuncId> = vec![];
            match fd.kind {
                FuncKind::EntityMethod(cl) => {
                    for arch in p.archetypes_of(cl) {
                        let g = p.virtual_target(arch, f);
                        if !targets.contains(&g) {
                            targets.push(g);
                        }
                        eff.merge(&c.info.get(g, Some(arch)));
                    }
                }
                _ => eff = c.info.get(f, None),
            }
            let bodiless = fd.is_abstract;
            let dispatches = targets.iter().any(|&g| g != f);
            s.push_str(&format!("\n\n**effects**: {}", crate::effects::describe_effects(p, &eff)));
            if dispatches {
                targets.sort();
                let names: Vec<String> = targets.iter().map(|&g| format!("`{}`", p.func_label(g))).collect();
                if targets.len() == 1 {
                    s.push_str(&format!("\n\n**dispatch**: a call runs {} (the effects above are its effects)", names[0]));
                } else {
                    s.push_str(&format!("\n\n**dispatch**: a call runs one of {} implementations: {} (the effects above are their union)", targets.len(), names.join(", ")));
                }
            } else if bodiless && targets.is_empty() {
                s.push_str("\n\n**dispatch**: no entity class implements it yet");
            }
            for sys in &c.ecs.systems {
                if sys.func == f {
                    let how = match sys.kind {
                        SysKind::Entity if sys.parallel => "data-parallel over entities".to_string(),
                        SysKind::Client | SysKind::ClientGlobal => "client system".to_string(),
                        _ => format!("serial: {}", sys.why_serial.join("; ")),
                    };
                    s.push_str(&format!("\n\n**system** `{}` — stage {} — {}", sys.name, sys.stage, how));
                }
            }
            if !eff.touches_world() && !eff.draws && fd.ret != Ty::Void && !dispatches && !bodiless {
                s.push_str("\n\npure: calls with constant arguments are evaluated at compile time");
            }
            s
        }
        Sym::Local(f, l) => {
            let loc = &p.funcs[f].locals[l];
            let what = match loc.kind {
                LocalKind::Param(_) => "parameter",
                LocalKind::Cursor(_) => "query variable",
                LocalKind::ListElem => "foreach variable",
                LocalKind::Normal => "local",
            };
            let mut s = code(format!("({}) {}", what, local_sig(p, loc)));
            if let LocalKind::Cursor(cl) = loc.kind {
                s.push_str(&format!("\n\nreads other `{}` entities as of the start of the system; writes to them are deferred", p.classes[cl].name));
            }
            if matches!(loc.ty, Ty::NHandle(_)) {
                s.push_str("\n\nmay be null or refer to a destroyed entity: check with `if (x is T y)`");
            }
            if let Some(r) = c.mem.locals.get(&(f, l)) {
                let rs = match r {
                    crate::memory::Region::Frame => "frame arena (freed in bulk at the end of the tick)",
                    crate::memory::Region::Heap => "heap (moved into an entity field)",
                    crate::memory::Region::Caller => "caller's region (returned)",
                };
                s.push_str(&format!("\n\n**memory**: {}", rs));
            }
            s
        }
    }
}

// ---------------------------------------------------------------------------
// inlay hints
// ---------------------------------------------------------------------------

/// (byte offset, label, kind: 1 = type, 2 = parameter/info)
fn inlay_hints(a: &Analysis) -> Vec<(usize, String, u32)> {
    let c = &a.c;
    let p = &c.prog;
    let mut v = vec![];
    for (f, fd) in p.fields.iter().enumerate() {
        let label = if c.ecs.is_dead(f) {
            "removed: never read".to_string()
        } else {
            let comp = c.ecs.comp_of(f).map(|k| c.ecs.components[k].cname.clone()).unwrap_or_default();
            match c.net.fields.get(&f) {
                Some(n) => format!("{} · net {} bits", comp, n.bits),
                None => format!("{} · server-only", comp),
            }
        };
        v.push((fd.span.hi as usize, label, 2));
    }
    for s in &c.ecs.systems {
        if let Some(sp) = p.index.defs.get(&Sym::Func(s.func)) {
            let label = match s.kind {
                SysKind::Entity => format!("stage {} · {}", s.stage, if s.parallel { "parallel" } else { "serial" }),
                SysKind::Global => format!("stage {} · global", s.stage),
                _ => "client system".to_string(),
            };
            v.push((sp.hi as usize, label, 2));
        }
    }
    for site in &c.mem.sites {
        if site.func == usize::MAX {
            continue;
        }
        let r = match site.region {
            crate::memory::Region::Frame => "frame arena",
            crate::memory::Region::Heap => "heap (entity-owned)",
            crate::memory::Region::Caller => "caller's region",
        };
        let res = if site.reserve.is_empty() { String::new() } else { " · reserved".to_string() };
        v.push((site.span.hi as usize, format!("→ {}{}", r, res), 2));
    }
    for f in &p.funcs {
        crate::sema::visit_block(&f.body, &mut |e| match &e.kind {
            ExprKind::Assign { op, target, .. } => {
                if let Some((_, false)) = crate::effects::target_field(target) {
                    let comm = matches!(op, Some(crate::ast::BinOp::Add | crate::ast::BinOp::Sub)) && target.ty.is_numeric();
                    v.push((e.span.hi as usize, if comm { "⇢ deferred ADD".into() } else { "⇢ deferred SET".into() }, 2));
                }
            }
            ExprKind::Call { dispatch: Dispatch::RemoteDeferred, .. } => v.push((e.span.hi as usize, "⇢ message".into(), 2)),
            ExprKind::Call { dispatch: Dispatch::Rpc, .. } => v.push((e.span.hi as usize, "⇢ RPC to server".into(), 2)),
            _ => {}
        });
    }
    // types of `var` declarations
    for (sym, sp) in &p.index.defs {
        if let Sym::Local(f, l) = sym {
            let loc = &p.funcs[*f].locals[*l];
            if loc.kind == LocalKind::Normal {
                let before = &a.src[..sp.lo as usize];
                let line_start = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
                if before[line_start..].trim_end().ends_with("var") {
                    v.push((sp.hi as usize, format!(": {}", p.ty_name(&loc.ty)), 1));
                }
            }
        }
    }
    v.sort_by_key(|x| x.0);
    v
}

// ---------------------------------------------------------------------------
// completion
// ---------------------------------------------------------------------------

const KEYWORDS: &[&str] = &[
    "public", "private", "static", "const", "mut", "own", "client", "extern", "async", "struct", "class", "enum", "override", "virtual", "abstract", "if", "else", "while", "do",
    "for", "foreach", "in", "return", "break", "continue", "var", "new", "this", "base", "true", "false", "null", "is", "ref", "out", "atomic",
    "fail", "int", "float", "bool", "void", "string", "List", "Dictionary", "Entity", "using", "namespace", "switch", "case", "default", "when",
];

fn item(label: &str, kind: u32, detail: String) -> Json {
    Json::obj(vec![("label", Json::str(label)), ("kind", Json::num(kind)), ("detail", Json::Str(detail))])
}

fn keyword_items() -> Vec<Json> {
    KEYWORDS.iter().map(|k| item(k, 14, "keyword".into())).collect()
}

fn builtin_members(ns: &str) -> Vec<Json> {
    let list: &[(&str, &str)] = match ns {
        "Math" => &[
            ("Sqrt", "float Sqrt(float x)"),
            ("Abs", "Abs(x)"),
            ("Min", "Min(a, b)"),
            ("Max", "Max(a, b)"),
            ("Clamp", "Clamp(x, lo, hi)"),
            ("Sin", "float Sin(float x)"),
            ("Cos", "float Cos(float x)"),
            ("Floor", "float Floor(float x)"),
            ("Atan2", "float Atan2(float y, float x)"),
            ("Tan", "float Tan(float x)"),
            ("Atan", "float Atan(float x)"),
            ("Asin", "float Asin(float x)"),
            ("Acos", "float Acos(float x)"),
            ("Exp", "float Exp(float x)"),
            ("Log", "float Log(float x) — natural logarithm"),
            ("Log10", "float Log10(float x)"),
            ("Log2", "float Log2(float x)"),
            ("Pow", "float Pow(float x, float y)"),
            ("Round", "float Round(float x) — halves to even, like C#"),
            ("Ceiling", "float Ceiling(float x)"),
            ("Truncate", "float Truncate(float x)"),
            ("Sign", "int Sign(x)"),
            ("PI", "const float PI"),
            ("E", "const float E"),
            ("Tau", "const float Tau"),
        ],
        "Random" => &[("Float", "float Float(float lo, float hi) — deterministic"), ("Int", "int Int(int lo, int hi) — deterministic")],
        "int" => &[("MaxValue", "const int MaxValue"), ("MinValue", "const int MinValue"), ("Parse", "int Parse(string s)"), ("TryParse", "bool TryParse(string s, out int value)")],
        "float" => &[
            ("NaN", "const float NaN"),
            ("PositiveInfinity", "const float PositiveInfinity"),
            ("NegativeInfinity", "const float NegativeInfinity"),
            ("Epsilon", "const float Epsilon"),
            ("MaxValue", "const float MaxValue"),
            ("MinValue", "const float MinValue"),
            ("Parse", "float Parse(string s)"),
            ("TryParse", "bool TryParse(string s, out float value)"),
        ],
        "char" => &[
            ("IsDigit", "bool IsDigit(char c)"),
            ("IsLetter", "bool IsLetter(char c)"),
            ("IsLetterOrDigit", "bool IsLetterOrDigit(char c)"),
            ("IsWhiteSpace", "bool IsWhiteSpace(char c)"),
            ("IsUpper", "bool IsUpper(char c)"),
            ("IsLower", "bool IsLower(char c)"),
            ("IsPunctuation", "bool IsPunctuation(char c)"),
            ("ToUpper", "char ToUpper(char c)"),
            ("ToLower", "char ToLower(char c)"),
            ("MaxValue", "const char MaxValue"),
            ("MinValue", "const char MinValue"),
        ],
        "string" => &[
            ("IsNullOrEmpty", "bool IsNullOrEmpty(string s)"),
            ("Join", "string Join(string separator, List<T> values)"),
            ("Concat", "string Concat(a, b, ...)"),
            ("Compare", "int Compare(string a, string b) — ordinal"),
        ],
        "Array" => &[
            ("Sort", "void Sort(T[] xs) — stable"),
            ("Reverse", "void Reverse(T[] xs)"),
            ("IndexOf", "int IndexOf(T[] xs, T value)"),
            ("Fill", "void Fill(T[] xs, T value)"),
            ("Copy", "T[] Copy(T[] xs)"),
        ],
        "Time" => &[("Tick", "int Tick"), ("Dt", "float Dt")],
        "Draw" => &[("Sprite", "client void Sprite(int id, float x, float y)")],
        _ => &[],
    };
    list.iter().map(|(n, d)| item(n, if ns == "Time" { 10 } else { 3 }, d.to_string())).collect()
}

fn members_of(a: &Analysis, ty: &Ty) -> Vec<Json> {
    let p = &a.c.prog;
    let mut v = vec![];
    match ty {
        Ty::Struct(s) => {
            for f in &p.structs[*s].fields {
                v.push(item(&f.name, 5, p.ty_name(&f.ty)));
            }
            let mut ms: Vec<(&String, &FuncId)> = p.structs[*s].methods.iter().collect();
            ms.sort();
            for (n, &f) in ms {
                if matches!(p.funcs[f].kind, FuncKind::StructMethod(_)) {
                    v.push(item(n, if p.funcs[f].is_property { 10 } else { 2 }, p.ty_name(&p.funcs[f].ret)));
                }
            }
        }
        Ty::Handle(c) => {
            for f in p.all_fields(*c) {
                let info = field_insights(a, f).into_iter().take(2).collect::<Vec<_>>().join(" · ").replace("**", "");
                v.push(item(&p.fields[f].name, 5, format!("{} — {}", p.ty_name(&p.fields[f].ty), info)));
            }
            let mut cur = Some(*c);
            let mut seen = std::collections::HashSet::new();
            while let Some(x) = cur {
                let mut ms: Vec<(&String, &FuncId)> = p.classes[x].methods.iter().collect();
                ms.sort();
                for (n, &f) in ms {
                    if matches!(p.funcs[f].kind, FuncKind::EntityMethod(_)) && seen.insert(n.clone()) {
                        v.push(item(n, if p.funcs[f].is_property { 10 } else { 2 }, p.ty_name(&p.funcs[f].ret)));
                    }
                }
                cur = p.classes[x].base;
            }
        }
        Ty::NHandle(c) => {
            v.push(item("is", 14, format!("check first: if (x is {} y) {{ ... }}", p.classes[*c].name)));
        }
        Ty::Str => {
            for (n, d) in [
                ("Length", "int — number of characters"),
                ("Substring", "string Substring(int start[, int length])"),
                ("Contains", "bool Contains(string)"),
                ("StartsWith", "bool StartsWith(string)"),
                ("EndsWith", "bool EndsWith(string)"),
                ("IndexOf", "int IndexOf(string or char)"),
                ("LastIndexOf", "int LastIndexOf(string or char)"),
                ("Split", "List<string> Split(string or char separator)"),
                ("PadLeft", "string PadLeft(int width[, char c])"),
                ("PadRight", "string PadRight(int width[, char c])"),
                ("Replace", "string Replace(string from, string to)"),
                ("Trim", "string Trim()"),
                ("ToUpper", "string ToUpper()"),
                ("ToLower", "string ToLower()"),
                ("CompareTo", "int CompareTo(string)"),
                ("Equals", "bool Equals(string)"),
            ] {
                v.push(item(n, if n == "Length" { 10 } else { 2 }, d.into()));
            }
        }
        Ty::Int | Ty::Float | Ty::Bool | Ty::Enum(_) => v.push(item("ToString", 2, "string ToString()".into())),
        Ty::Char => {
            v.push(item("ToString", 2, "string ToString()".into()));
            v.push(item("CompareTo", 2, "int CompareTo(char other)".into()));
            v.push(item("Equals", 2, "bool Equals(char other)".into()));
        }
        Ty::Dict(k, val) => {
            let (kn, vn) = (p.ty_name(k), p.ty_name(val));
            for (n, d) in [
                ("Count", "int".to_string()),
                ("Add", format!("void Add({}, {})", kn, vn)),
                ("ContainsKey", format!("bool ContainsKey({})", kn)),
                ("Remove", format!("bool Remove({})", kn)),
                ("Clear", "void Clear()".into()),
                ("TryGetValue", format!("bool TryGetValue({}, out {})", kn, vn)),
                ("GetValueOrDefault", format!("{} GetValueOrDefault({}[, {}])", vn, kn, vn)),
                ("Keys", format!("foreach (var k in d.Keys) — {}", kn)),
                ("Values", format!("foreach (var v in d.Values) — {}", vn)),
            ] {
                v.push(item(n, if matches!(n, "Count" | "Keys" | "Values") { 10 } else { 2 }, d));
            }
        }
        Ty::List(el) => {
            let et = p.ty_name(el);
            for (n, d) in [("Count", "int".to_string()), ("Add", format!("void Add({})", et)), ("Clear", "void Clear()".into()), ("RemoveAt", "void RemoveAt(int)".into()), ("Contains", format!("bool Contains({})", et)), ("IndexOf", format!("int IndexOf({})", et)), ("Insert", format!("void Insert(int, {})", et)), ("Remove", format!("bool Remove({})", et)), ("Reverse", "void Reverse()".into()), ("Sort", "void Sort()".into()), ("Clone", format!("List<{}> Clone()", et))] {
                v.push(item(n, if n == "Count" { 10 } else { 2 }, d));
            }
        }
        Ty::Array(_) | Ty::MdArray(..) => {
            let tn = p.ty_name(ty);
            for (n, d) in [("Length", "int".to_string()), ("Rank", "int".to_string()), ("GetLength", "int GetLength(int dimension)".to_string()), ("GetUpperBound", "int GetUpperBound(int dimension)".to_string()), ("Clone", format!("{} Clone()", tn))] {
                v.push(item(n, if matches!(n, "Length" | "Rank") { 10 } else { 2 }, d));
            }
            // a 2D or 3D array is a grid (docs/01 §1.14.2)
            let rank = ty.rank();
            if rank >= 2 {
                let el = p.elem_of(ty).map(|e| p.ty_name(&e)).unwrap_or_default();
                let cs = ["x", "y", "z"][..rank].join(", ");
                let mut grid: Vec<(&str, String)> = vec![("Width", "int".into()), ("Height", "int".into())];
                if rank == 3 {
                    grid.push(("Depth", "int".into()));
                }
                grid.extend([
                    ("InBounds", format!("bool InBounds({cs})")),
                    ("Get", format!("{el} Get({cs}, {el} fallback)")),
                    ("Fill", format!("void Fill({el} value)")),
                    ("Flood", format!("int Flood({cs}, {el} value): fills the connected region of equal cells")),
                    ("Step", format!("void Step(({cs}, c) => new value): every cell from the grid as it was, in parallel")),
                    ("Move", format!("void Move({el} empty, ({cs}, c) => Grid.Dir(..) or Grid.Stay): cells move into free neighbours")),
                    ("Ray", format!("int Ray({cs} from, {cs} to, c => hit): steps to the first hit, or -1")),
                ]);
                for (n, d) in grid {
                    v.push(item(n, if matches!(n, "Width" | "Height" | "Depth") { 10 } else { 2 }, d));
                }
            }
        }
        _ => {}
    }
    v
}

/// members reachable from a type name: enum members, constants, static methods, built-in namespaces
fn static_members(a: &Analysis, name: &str) -> Option<Vec<Json>> {
    let p = &a.c.prog;
    if ["Math", "Random", "Time", "Draw", "int", "float", "char", "string", "Array"].contains(&name) {
        return Some(builtin_members(name));
    }
    if let Some(e) = p.enums.iter().position(|e| e.name == name) {
        return Some(p.enums[e].variants.iter().map(|(n, v)| item(n, 20, format!("= {}", v))).collect());
    }
    let decl = a.c.ast.items.iter().find(|d| d.name == name)?;
    let mut v = vec![];
    for m in &decl.members {
        match m {
            crate::ast::Member::Field(f) if f.mods.is_const => {
                let val = p.consts.iter().find(|k| k.span == f.span).and_then(|k| k.value.as_ref()).map(|v| format!(" = {}", crate::opt::show_val(p, v))).unwrap_or_default();
                v.push(item(&f.name, 21, format!("const{}", val)));
            }
            crate::ast::Member::Method(md) if md.mods.is_static && matches!(md.kind, crate::ast::MethodKind::Normal) => {
                v.push(item(&md.name, 3, format!("static {}", md.ret.name)));
            }
            _ => {}
        }
    }
    Some(v)
}

/// type of a name inside function `func` (locals, then fields of `this`)
fn type_of_name(a: &Analysis, func: Option<FuncId>, name: &str) -> Option<Ty> {
    let p = &a.c.prog;
    let f = func?;
    let fd = &p.funcs[f];
    if let Some(l) = fd.locals.iter().rposition(|l| l.name == name) {
        return Some(fd.locals[l].ty.clone());
    }
    match (name, fd.kind) {
        ("this", FuncKind::EntityMethod(c)) => return Some(Ty::Handle(c)),
        ("this", FuncKind::StructMethod(s) | FuncKind::StructCtor(s)) => return Some(Ty::Struct(s)),
        (_, FuncKind::EntityMethod(c)) => {
            if let Some(fid) = p.find_field(c, name) {
                return Some(p.fields[fid].ty.clone());
            }
        }
        (_, FuncKind::StructMethod(s) | FuncKind::StructCtor(s)) => {
            if let Some(x) = p.structs[s].fields.iter().find(|x| x.name == name) {
                return Some(x.ty.clone());
            }
        }
        _ => {}
    }
    None
}

fn member_type(a: &Analysis, ty: &Ty, name: &str) -> Option<Ty> {
    let p = &a.c.prog;
    match ty {
        Ty::Struct(s) => {
            if let Some(f) = p.structs[*s].fields.iter().find(|f| f.name == name) {
                return Some(f.ty.clone());
            }
            p.structs[*s].methods.get(name).map(|&f| p.funcs[f].ret.clone())
        }
        Ty::Handle(c) => {
            if let Some(f) = p.find_field(*c, name) {
                return Some(p.fields[f].ty.clone());
            }
            p.resolve_method(*c, name).map(|f| p.funcs[f].ret.clone())
        }
        Ty::List(_) if name == "Count" => Some(Ty::Int),
        Ty::Array(_) | Ty::MdArray(..) if matches!(name, "Length" | "Rank" | "GetLength" | "GetUpperBound") => Some(Ty::Int),
        Ty::MdArray(..) if matches!(name, "Width" | "Height" | "Depth" | "Flood" | "Ray") => Some(Ty::Int),
        Ty::Array(_) | Ty::MdArray(..) if name == "Clone" => Some(ty.clone()),
        Ty::Str if name == "Length" => Some(Ty::Int),
        _ => None,
    }
}

/// Is `before` (the text before the name being typed) where a range type's bound goes: right after
/// `int in`, `float in -`, `SVec in` or `int in 0..` / `int in MIN_X..`?
fn in_range_bound(a: &Analysis, before: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut t = before.trim_end();
    if let Some(r) = t.strip_suffix("..") {
        // the upper bound: skip the lower one (a number or a constant, maybe qualified or negative)
        let r = r.trim_end();
        let lo = r.trim_end_matches(|c: char| ident(c) || c == '.');
        if lo.len() == r.len() {
            return false;
        }
        t = lo.trim_end();
    }
    t = t.strip_suffix('-').unwrap_or(t).trim_end();
    let Some(r) = t.strip_suffix("in") else { return false };
    if r.ends_with(ident) {
        return false; // a name ending in "in" (`main`)
    }
    // `in` right after a type: a range type (a foreach has its variable there)
    let r = r.trim_end();
    let ty = &r[r.trim_end_matches(ident).len()..];
    ty == "int" || ty == "float" || a.c.prog.structs.iter().any(|s| s.name == ty)
}

/// the constants a range bound at `aoff` can name: local constants, the enclosing type's (by name)
/// and other types' (qualified), numbers only
fn range_constants(a: &Analysis, func: Option<FuncId>, aoff: usize) -> Vec<Json> {
    let p = &a.c.prog;
    let mut v = vec![];
    if let Some(f) = func {
        for (l, loc) in p.funcs[f].locals.iter().enumerate() {
            let before = p.index.defs.get(&Sym::Local(f, l)).map(|sp| (sp.lo as usize) < aoff).unwrap_or(false);
            if loc.is_const && before && matches!(loc.ty, Ty::Int | Ty::Float) {
                v.push(item(&loc.name, 21, format!("const {}", p.ty_name(&loc.ty))));
            }
        }
    }
    let here = a.c.ast.items.iter().filter(|d| (d.span.lo as usize) <= aoff).last().map(|d| d.name.clone());
    for d in &a.c.ast.items {
        for m in &d.members {
            let crate::ast::Member::Field(fd) = m else { continue };
            let Some(k) = p.consts.iter().find(|k| k.span == fd.span) else { continue };
            if !fd.mods.is_const || !matches!(k.ty, Ty::Int | Ty::Float) {
                continue;
            }
            let label = if here.as_deref() == Some(d.name.as_str()) { k.name.clone() } else { format!("{}.{}", d.name, k.name) };
            let val = k.value.as_ref().map(|x| format!(" = {}", crate::opt::show_val(p, x))).unwrap_or_default();
            v.push(item(&label, 21, format!("const {}{}", p.ty_name(&k.ty), val)));
        }
    }
    v
}

/// `text`/`off`: the editor's current document and cursor; `goff`: the cursor in the analysed project text
fn complete(a: &Analysis, text: &str, off: usize, goff: usize) -> Vec<Json> {
    let p = &a.c.prog;
    let before = &text[..off.min(text.len())];
    // identifier being typed (names may have Unicode letters: 체력)
    let ident_start = |s: &str| s.trim_end_matches(|c: char| c.is_alphanumeric() || c == '_').len();
    let mut rest = &before[..ident_start(before)];
    // member chain: a.b.c.
    let mut chain: Vec<String> = vec![];
    while rest.ends_with('.') {
        let r = rest[..rest.len() - 1].trim_end_matches("()");
        let s = ident_start(r);
        if s == r.len() {
            break;
        }
        chain.insert(0, r[s..].to_string());
        rest = &r[..s];
    }
    // position inside the analysed source (identical when the text compiles)
    let aoff = goff.min(a.src.len());
    let func = p.index.func_at(aoff as u32);
    // a range type's bound (`int in |`, `int in 0..|`): a number or a constant, nothing else
    if in_range_bound(a, rest) {
        return match chain.as_slice() {
            [] => range_constants(a, func, aoff),
            [ty] => static_members(a, ty).unwrap_or_default().into_iter().filter(|i| i.get("kind").as_i64() == Some(21)).collect(),
            _ => vec![],
        };
    }
    if !chain.is_empty() {
        let mut ty: Option<Ty> = None;
        for (i, seg) in chain.iter().enumerate() {
            if i == 0 {
                if let Some(t) = type_of_name(a, func, seg) {
                    ty = Some(t);
                    continue;
                }
                if chain.len() == 1 {
                    if let Some(items) = static_members(a, seg) {
                        return items;
                    }
                }
                return vec![];
            }
            ty = member_type(a, ty.as_ref().unwrap(), seg);
            if ty.is_none() {
                return vec![];
            }
        }
        return ty.map(|t| members_of(a, &t)).unwrap_or_default();
    }
    // scope completion
    let mut items = keyword_items();
    for (n, d) in [
        ("Spawn", "Spawn(new T { ... }) — created at the end of the system"),
        ("Destroy", "Destroy() / Destroy(h) — deferred"),
        ("Print", "Print(x)"),
        ("Query", "foreach (var x in Query<T>())"),
        ("Count", "Count<T>()"),
        ("Add", "Add(new C { ... }) — attach a component at the end of the system"),
        ("Remove", "Remove<C>() — detach a component at the end of the system"),
        ("Has", "Has<C>() — has the component (start of the system)"),
        ("Get", "Get<C>() is C c — the component, or null"),
    ] {
        items.push(item(n, 3, d.into()));
    }
    for ns in ["Math", "Random", "Time", "Draw"] {
        items.push(item(ns, 9, "built-in".into()));
    }
    for s in &p.structs {
        items.push(item(&s.name, 22, "struct".into()));
    }
    for e in &p.enums {
        items.push(item(&e.name, 13, "enum".into()));
    }
    for (ci, c) in p.classes.iter().enumerate() {
        // combinations of components and the `Owner` type are the compiler's, not names to write
        if c.combo_of.is_some() || Some(ci) == p.entity_root {
            continue;
        }
        let what = if c.is_static {
            "static class"
        } else if c.is_component {
            "component"
        } else {
            "entity class"
        };
        items.push(item(&c.name, 7, what.into()));
    }
    if let Some(f) = func {
        let fd = &p.funcs[f];
        for (l, loc) in fd.locals.iter().enumerate() {
            if loc.name.starts_with("__") {
                continue;
            }
            let declared_before = p.index.defs.get(&Sym::Local(f, l)).map(|sp| (sp.lo as usize) < aoff).unwrap_or(true);
            if declared_before {
                items.push(item(&loc.name, 6, p.ty_name(&loc.ty)));
            }
        }
        match fd.kind {
            FuncKind::EntityMethod(c) => items.extend(members_of(a, &Ty::Handle(c))),
            FuncKind::StructMethod(s) | FuncKind::StructCtor(s) => items.extend(members_of(a, &Ty::Struct(s))),
            _ => {}
        }
    }
    items
}
