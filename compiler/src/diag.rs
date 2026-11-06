//! Source spans and diagnostics.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub fn new(lo: usize, hi: usize) -> Span {
        Span { lo: lo as u32, hi: hi as u32 }
    }
    pub fn to(self, other: Span) -> Span {
        Span { lo: self.lo.min(other.lo), hi: self.hi.max(other.hi) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Hint,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub level: Level,
    pub code: &'static str,
    pub msg: String,
    pub span: Span,
    pub notes: Vec<String>,
}

#[derive(Default, Debug)]
pub struct Diags {
    pub list: Vec<Diagnostic>,
}

impl Diags {
    pub fn error(&mut self, code: &'static str, span: Span, msg: impl Into<String>) {
        self.list.push(Diagnostic { level: Level::Error, code, msg: msg.into(), span, notes: vec![] });
    }
    pub fn error_note(&mut self, code: &'static str, span: Span, msg: impl Into<String>, note: impl Into<String>) {
        self.list.push(Diagnostic { level: Level::Error, code, msg: msg.into(), span, notes: vec![note.into()] });
    }
    pub fn warn(&mut self, code: &'static str, span: Span, msg: impl Into<String>) {
        self.list.push(Diagnostic { level: Level::Warning, code, msg: msg.into(), span, notes: vec![] });
    }
    pub fn warn_note(&mut self, code: &'static str, span: Span, msg: impl Into<String>, note: impl Into<String>) {
        self.list.push(Diagnostic { level: Level::Warning, code, msg: msg.into(), span, notes: vec![note.into()] });
    }
    pub fn has_errors(&self) -> bool {
        self.list.iter().any(|d| d.level == Level::Error)
    }
    pub fn error_count(&self) -> usize {
        self.list.iter().filter(|d| d.level == Level::Error).count()
    }
    pub fn codes(&self) -> Vec<&'static str> {
        self.list.iter().map(|d| d.code).collect()
    }
    /// Drops the diagnostics from `start` on that repeat an earlier one exactly: code that is
    /// checked more than once (an interface's default implementation and the structs' copies of
    /// it) reports each mistake once.
    pub fn dedupe_from(&mut self, start: usize) {
        let mut i = start;
        while i < self.list.len() {
            let x = &self.list[i];
            if self.list[..i].iter().any(|y| y.level == x.level && y.code == x.code && y.span == x.span && y.msg == x.msg) {
                self.list.remove(i);
                continue;
            }
            i += 1;
        }
    }
}

/// Converts a byte offset to 1-based (line, column).
pub fn line_col(src: &str, off: u32) -> (usize, usize) {
    let off = (off as usize).min(src.len());
    let before = &src[..off];
    let line = before.matches('\n').count() + 1;
    let col = before.rfind('\n').map(|p| off - p).unwrap_or(off + 1);
    (line, col)
}

pub fn render(src: &str, file: &str, d: &Diagnostic) -> String {
    let (line, col) = line_col(src, d.span.lo);
    let lvl = match d.level {
        Level::Error => "error",
        Level::Warning => "warning",
        Level::Hint => "hint",
    };
    let mut out = format!("{}[{}]: {}\n  --> {}:{}:{}\n", lvl, d.code, d.msg, file, line, col);
    if let Some(text) = src.lines().nth(line - 1) {
        let width = ((d.span.hi.saturating_sub(d.span.lo)) as usize).clamp(1, text.len().saturating_sub(col - 1).max(1));
        out.push_str(&format!("{:>5} | {}\n      | {}{}\n", line, text, " ".repeat(col - 1), "^".repeat(width)));
    }
    for n in &d.notes {
        out.push_str(&format!("      = note: {}\n", n));
    }
    out
}

/// Separator placed between files of a project in the combined source. It is a comment, so the
/// lexer skips it; tools that rewrite the combined text (lint --fix) split on it afterwards.
pub const FILE_SEP: &str = "\n//@@sijo-file@@\n";

/// One source file inside the combined project text.
#[derive(Clone, Debug)]
pub struct SrcFile {
    pub path: String,
    /// byte offset of the file's first byte in `SourceMap::text`
    pub base: usize,
    pub len: usize,
}

/// A project is compiled as one text: every file, in path order, joined with `FILE_SEP`.
/// Spans are offsets into that text; the map turns them back into (file, line, column).
#[derive(Clone, Debug, Default)]
pub struct SourceMap {
    pub files: Vec<SrcFile>,
    pub text: String,
}

impl SourceMap {
    pub fn single(path: &str, src: &str) -> SourceMap {
        SourceMap::new(vec![(path.to_string(), src.to_string())])
    }
    pub fn new(files: Vec<(String, String)>) -> SourceMap {
        let mut sm = SourceMap::default();
        for (i, (path, src)) in files.into_iter().enumerate() {
            if i > 0 {
                sm.text.push_str(FILE_SEP);
            }
            sm.files.push(SrcFile { path, base: sm.text.len(), len: src.len() });
            sm.text.push_str(&src);
        }
        sm
    }
    /// index of the file containing `off` (offsets in separators belong to the previous file)
    pub fn file_index(&self, off: u32) -> usize {
        let off = off as usize;
        match self.files.iter().rposition(|f| f.base <= off) {
            Some(i) => i,
            None => 0,
        }
    }
    pub fn file_text(&self, i: usize) -> &str {
        let f = &self.files[i];
        &self.text[f.base..f.base + f.len]
    }
    pub fn find(&self, path: &str) -> Option<usize> {
        self.files.iter().position(|f| f.path == path)
    }
    /// (file index, offset inside that file)
    pub fn local(&self, off: u32) -> (usize, u32) {
        let i = self.file_index(off);
        let f = &self.files[i];
        (i, (off as usize).saturating_sub(f.base).min(f.len) as u32)
    }
    /// "path:line:col" of an offset
    pub fn position(&self, off: u32) -> (String, usize, usize) {
        let (i, lo) = self.local(off);
        let (l, c) = line_col(self.file_text(i), lo);
        (self.files[i].path.clone(), l, c)
    }
    pub fn render(&self, d: &Diagnostic) -> String {
        let (i, lo) = self.local(d.span.lo);
        let hi = (d.span.hi as usize).saturating_sub(self.files[i].base).min(self.files[i].len) as u32;
        let mut d2 = d.clone();
        d2.span = Span { lo, hi: hi.max(lo) };
        render(self.file_text(i), &self.files[i].path, &d2)
    }
    /// split a (possibly rewritten) combined text back into its files
    pub fn split(text: &str) -> Vec<String> {
        text.split(FILE_SEP).map(|s| s.to_string()).collect()
    }
    /// a short name for the project (first file stem, or the directory name)
    pub fn name(&self) -> String {
        let p = std::path::Path::new(self.files.first().map(|f| f.path.as_str()).unwrap_or("out"));
        p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "out".into())
    }
}
