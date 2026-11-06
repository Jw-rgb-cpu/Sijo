//! Lexer for Sijo's C#-like surface syntax.

use crate::diag::{Diags, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum InterpPiece {
    Text(String),
    /// Source text of an embedded expression, its absolute byte offset, and an optional format spec (e.g. "F2").
    Expr { src: String, offset: usize, fmt: Option<String> },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(String),
    Int(i64),
    /// value, had explicit `f` suffix
    Float(f64, bool),
    Str(String),
    /// character literal: a Unicode code point
    Char(u32),
    Interp(Vec<InterpPiece>),
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

const PUNCTS: &[&str] = &[
    "<<=", ">>=", "??=", "=>", "==", "!=", "<=", ">=", "&&", "||", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=",
    "^=", "<<", ">>", "??", "?.", "::", "..", "{", "}", "(", ")", "[", "]", ";", ",", ".", ":", "?", "+", "-", "*", "/", "%",
    "&", "|", "^", "!", "~", "=", "<", ">", "@",
];

pub fn lex(src: &str, base: usize, diags: &mut Diags) -> Vec<Token> {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < bytes.len() {
        let c = bytes[i];
        // whitespace
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        // comments
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 >= bytes.len() {
                diags.error("E0001", Span::new(base + start, base + start + 2), "unterminated block comment");
                break;
            }
            i += 2;
            continue;
        }
        let start = i;
        // identifiers / keywords
        // identifiers (C#-style: Unicode letters such as Hangul are allowed)
        let uni_start = c >= 0x80 && src[i..].chars().next().map(|ch| ch.is_alphabetic()).unwrap_or(false);
        if c.is_ascii_alphabetic() || c == b'_' || uni_start {
            while i < bytes.len() {
                if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
                    i += 1;
                } else if bytes[i] >= 0x80 {
                    let ch = src[i..].chars().next().unwrap();
                    if !ch.is_alphanumeric() {
                        break;
                    }
                    i += ch.len_utf8();
                } else {
                    break;
                }
            }
            out.push(Token { tok: Tok::Ident(src[start..i].to_string()), span: Span::new(base + start, base + i) });
            continue;
        }
        // numbers
        if c.is_ascii_digit() || (c == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit()) {
            if c == b'0' && i + 1 < bytes.len() && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
                i += 2;
                let hs = i;
                while i < bytes.len() && (bytes[i].is_ascii_hexdigit() || bytes[i] == b'_') {
                    i += 1;
                }
                let txt: String = src[hs..i].chars().filter(|&c| c != '_').collect();
                let v = i64::from_str_radix(&txt, 16).unwrap_or_else(|_| {
                    diags.error("E0002", Span::new(base + start, base + i), "invalid hex literal");
                    0
                });
                out.push(Token { tok: Tok::Int(v), span: Span::new(base + start, base + i) });
                continue;
            }
            let mut is_float = false;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                is_float = true;
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                    i += 1;
                }
            }
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                let save = i;
                i += 1;
                if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
                    i += 1;
                }
                if i < bytes.len() && bytes[i].is_ascii_digit() {
                    is_float = true;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                } else {
                    i = save;
                }
            }
            let txt: String = src[start..i].chars().filter(|&c| c != '_').collect();
            let mut fsuffix = false;
            if i < bytes.len() && (bytes[i] == b'f' || bytes[i] == b'F') {
                fsuffix = true;
                is_float = true;
                i += 1;
            } else if i < bytes.len() && (bytes[i] == b'd' || bytes[i] == b'D' || bytes[i] == b'm' || bytes[i] == b'M') {
                is_float = true;
                i += 1;
            }
            let span = Span::new(base + start, base + i);
            if is_float {
                let v: f64 = txt.parse().unwrap_or_else(|_| {
                    diags.error("E0002", span, "invalid float literal");
                    0.0
                });
                out.push(Token { tok: Tok::Float(v, fsuffix), span });
            } else {
                let v: i64 = txt.parse().unwrap_or_else(|_| {
                    diags.error("E0002", span, "integer literal too large");
                    0
                });
                out.push(Token { tok: Tok::Int(v), span });
            }
            continue;
        }
        // interpolated string
        if c == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
            i += 2;
            let mut pieces = Vec::new();
            let mut text = String::new();
            let mut ok = false;
            while i < bytes.len() {
                let ch = bytes[i];
                if ch == b'"' {
                    i += 1;
                    ok = true;
                    break;
                }
                if ch == b'\\' {
                    let (s, n, err) = escape(&src[i..]);
                    if let Some(m) = err {
                        diags.error("E0005", Span::new(base + i, base + i + n), m);
                    }
                    text.push_str(&s);
                    i += n;
                    continue;
                }
                if ch == b'{' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                        text.push('{');
                        i += 2;
                        continue;
                    }
                    if !text.is_empty() {
                        pieces.push(InterpPiece::Text(std::mem::take(&mut text)));
                    }
                    i += 1;
                    let es = i;
                    let mut depth = 0i32;
                    let mut fmt_at: Option<usize> = None;
                    while i < bytes.len() {
                        let b = bytes[i];
                        if b == b'"' {
                            // a string literal inside the hole (C# 11): skip it
                            let mut j = i + 1;
                            while j < bytes.len() && bytes[j] != b'"' && bytes[j] != b'\n' {
                                j += if bytes[j] == b'\\' { 2 } else { 1 };
                            }
                            if j >= bytes.len() || bytes[j] != b'"' {
                                break;
                            }
                            i = j + 1;
                            continue;
                        }
                        if b == b'(' || b == b'[' || b == b'{' {
                            depth += 1;
                        } else if b == b')' || b == b']' {
                            depth -= 1;
                        } else if b == b'}' {
                            if depth == 0 {
                                break;
                            }
                            depth -= 1;
                        } else if b == b':' && depth == 0 && fmt_at.is_none() {
                            fmt_at = Some(i);
                        }
                        i += 1;
                    }
                    if i >= bytes.len() || bytes[i] != b'}' {
                        diags.error("E0003", Span::new(base + es - 1, base + i), "unterminated interpolation hole");
                        break;
                    }
                    let (esrc, fmt) = match fmt_at {
                        Some(f) => (src[es..f].to_string(), Some(src[f + 1..i].trim().to_string())),
                        None => (src[es..i].to_string(), None),
                    };
                    pieces.push(InterpPiece::Expr { src: esrc, offset: base + es, fmt });
                    i += 1;
                    continue;
                }
                if ch == b'}' && i + 1 < bytes.len() && bytes[i + 1] == b'}' {
                    text.push('}');
                    i += 2;
                    continue;
                }
                let chlen = utf8_len(ch);
                text.push_str(&src[i..i + chlen]);
                i += chlen;
            }
            if !ok {
                diags.error("E0003", Span::new(base + start, base + i), "unterminated interpolated string");
            }
            if !text.is_empty() {
                pieces.push(InterpPiece::Text(text));
            }
            out.push(Token { tok: Tok::Interp(pieces), span: Span::new(base + start, base + i) });
            continue;
        }
        // plain string
        if c == b'"' {
            i += 1;
            let mut text = String::new();
            let mut ok = false;
            while i < bytes.len() {
                let ch = bytes[i];
                if ch == b'"' {
                    i += 1;
                    ok = true;
                    break;
                }
                if ch == b'\\' {
                    let (s, n, err) = escape(&src[i..]);
                    if let Some(m) = err {
                        diags.error("E0005", Span::new(base + i, base + i + n), m);
                    }
                    text.push_str(&s);
                    i += n;
                    continue;
                }
                if ch == b'\n' {
                    break;
                }
                let chlen = utf8_len(ch);
                text.push_str(&src[i..i + chlen]);
                i += chlen;
            }
            if !ok {
                diags.error("E0003", Span::new(base + start, base + i), "unterminated string literal");
            }
            out.push(Token { tok: Tok::Str(text), span: Span::new(base + start, base + i) });
            continue;
        }
        // 'c': a character literal (one Unicode code point; escapes as in strings)
        if c == b'\'' {
            let mut j = i + 1;
            let mut text = String::new();
            let mut bad: Option<String> = None;
            while j < bytes.len() && bytes[j] != b'\'' && bytes[j] != b'\n' {
                if bytes[j] == b'\\' {
                    let (t, n, err) = escape(&src[j..]);
                    if err.is_some() && bad.is_none() {
                        bad = err;
                    }
                    text.push_str(&t);
                    j += n.max(1);
                } else {
                    let n = utf8_len(bytes[j]);
                    text.push_str(&src[j..(j + n).min(src.len())]);
                    j += n;
                }
            }
            let closed = j < bytes.len() && bytes[j] == b'\'';
            let span = Span::new(base + i, base + if closed { j + 1 } else { j });
            let mut chars = text.chars();
            let first = chars.next();
            if let Some(msg) = bad {
                diags.error("E0004", span, msg);
            } else if !closed {
                diags.error("E0004", span, "unterminated character literal");
            } else if first.is_none() {
                diags.error("E0004", span, "empty character literal");
            } else if chars.next().is_some() {
                diags.error_note("E0004", span, "too many characters in character literal", format!("a char holds one character; for text use the string \"{}\"", text.escape_default()));
            }
            out.push(Token { tok: Tok::Char(first.map(|c| c as u32).unwrap_or(0)), span });
            i = if closed { j + 1 } else { j };
            continue;
        }
        // `0...5` would otherwise lex as `0 .. .5`: say what is meant and read it as `..`
        if src[i..].starts_with("...") {
            diags.error_note("E0004", Span::new(base + i, base + i + 3), "'...' is not a Sijo operator", "a range is written with two dots: lo..hi (both ends included)");
            out.push(Token { tok: Tok::Punct(".."), span: Span::new(base + i, base + i + 3) });
            i += 3;
            while i < bytes.len() && bytes[i] == b'.' {
                i += 1;
            }
            continue;
        }
        // punctuation
        let mut matched = false;
        for p in PUNCTS {
            if src[i..].starts_with(p) {
                out.push(Token { tok: Tok::Punct(p), span: Span::new(base + i, base + i + p.len()) });
                i += p.len();
                matched = true;
                break;
            }
        }
        if !matched {
            let chlen = utf8_len(c);
            diags.error("E0004", Span::new(base + i, base + i + chlen), format!("unexpected character '{}'", &src[i..i + chlen]));
            i += chlen;
        }
    }
    out.push(Token { tok: Tok::Eof, span: Span::new(base + src.len(), base + src.len()) });
    out
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

/// A C# escape sequence at the start of `s` (which begins with a backslash): the text it stands
/// for, the bytes it takes, and an error message if it is not a valid escape.
fn escape(s: &str) -> (String, usize, Option<String>) {
    let b = s.as_bytes();
    if b.len() < 2 {
        return (String::new(), 1, Some("unterminated escape sequence".into()));
    }
    let simple = match b[1] {
        b'n' => Some('\n'),
        b't' => Some('\t'),
        b'r' => Some('\r'),
        b'0' => Some('\0'),
        b'a' => Some('\u{7}'),
        b'b' => Some('\u{8}'),
        b'f' => Some('\u{c}'),
        b'v' => Some('\u{b}'),
        b'\\' => Some('\\'),
        b'"' => Some('"'),
        b'\'' => Some('\''),
        b'{' => Some('{'),
        b'}' => Some('}'),
        _ => None,
    };
    if let Some(c) = simple {
        return (c.to_string(), 2, None);
    }
    let hex = |from: usize, max: usize| -> (u32, usize) {
        let mut v = 0u32;
        let mut n = 0;
        while n < max && from + n < b.len() && b[from + n].is_ascii_hexdigit() {
            v = v * 16 + (b[from + n] as char).to_digit(16).unwrap();
            n += 1;
        }
        (v, n)
    };
    let code_point = |v: u32, len: usize| -> (String, usize, Option<String>) {
        match char::from_u32(v) {
            Some(c) => (c.to_string(), len, None),
            None => (String::new(), len, Some(format!("'{}' is not a Unicode scalar value (UTF-16 surrogate halves are not allowed; write the code point with \\U)", &s[..len]))),
        }
    };
    match b[1] {
        // \x: one to four hex digits (C#)
        b'x' => {
            let (v, n) = hex(2, 4);
            if n == 0 {
                return (String::new(), 2, Some("'\\x' needs 1 to 4 hex digits".into()));
            }
            code_point(v, 2 + n)
        }
        b'u' | b'U' => {
            let want = if b[1] == b'u' { 4 } else { 8 };
            let (v, n) = hex(2, want);
            if n < want {
                return (String::new(), 2 + n, Some(format!("'\\{}' needs exactly {} hex digits", b[1] as char, want)));
            }
            // a UTF-16 surrogate pair written as two \u escapes is one code point
            if b[1] == b'u' && (0xD800..0xDC00).contains(&v) && s[6..].starts_with("\\u") {
                let (lo, m) = hex(8, 4);
                if m == 4 && (0xDC00..0xE000).contains(&lo) {
                    return code_point(0x10000 + ((v - 0xD800) << 10) + (lo - 0xDC00), 12);
                }
            }
            code_point(v, 2 + n)
        }
        _ => {
            let len = 1 + s[1..].chars().next().map(|c| c.len_utf8()).unwrap_or(0);
            (String::new(), len, Some(format!("unrecognized escape sequence '{}'", &s[..len])))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lex_basic() {
        let mut d = Diags::default();
        let t = lex("x += 1.5f; // hi\n y = $\"a{b:F2}c\";", 0, &mut d);
        assert!(!d.has_errors());
        assert_eq!(t[0].tok, Tok::Ident("x".into()));
        assert_eq!(t[1].tok, Tok::Punct("+="));
        assert_eq!(t[2].tok, Tok::Float(1.5, true));
        match &t[6].tok {
            Tok::Interp(p) => {
                assert_eq!(p.len(), 3);
                assert_eq!(p[1], InterpPiece::Expr { src: "b".into(), offset: 26, fmt: Some("F2".into()) });
            }
            other => panic!("{:?}", other),
        }
    }
}
