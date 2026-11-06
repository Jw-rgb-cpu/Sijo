//! Character classification and case mapping (`char.IsLetter`, `char.ToUpper`, `string.ToUpper`...).
//!
//! One set of range tables serves both the compiler (compile-time evaluation) and the generated
//! program: `c_tables()` emits them into the C runtime, so results are identical everywhere.
//! ASCII and Latin-1 follow .NET's invariant culture exactly; Latin Extended-A, Greek, Cyrillic and
//! Armenian case pairs and the letters of the common scripts (Hangul, kana, CJK, Hebrew, Arabic,
//! Devanagari, Thai, Georgian, fullwidth forms) are covered by the tables below.

/// Unicode letters (categories Lu Ll Lt Lm Lo) of the covered scripts
pub const LETTER: &[(u32, u32)] = &[
    (0x41, 0x5A), (0x61, 0x7A), (0xAA, 0xAA), (0xB5, 0xB5), (0xBA, 0xBA), (0xC0, 0xD6), (0xD8, 0xF6), (0xF8, 0x2C1),
    (0x2C6, 0x2D1), (0x2E0, 0x2E4), (0x370, 0x374), (0x376, 0x377), (0x37A, 0x37D), (0x37F, 0x37F), (0x386, 0x386),
    (0x388, 0x38A), (0x38C, 0x38C), (0x38E, 0x3A1), (0x3A3, 0x3F5), (0x3F7, 0x481), (0x48A, 0x52F), (0x531, 0x556),
    (0x561, 0x587), (0x5D0, 0x5EA), (0x620, 0x64A), (0x671, 0x6D3), (0x904, 0x939), (0xE01, 0xE30), (0x10A0, 0x10C5),
    (0x10D0, 0x10FA), (0x1100, 0x11FF), (0x1E00, 0x1FBC), (0x3041, 0x3096), (0x30A1, 0x30FA), (0x3131, 0x318E),
    (0x3400, 0x4DBF), (0x4E00, 0x9FFF), (0xAC00, 0xD7A3), (0xF900, 0xFAFF), (0xFF21, 0xFF3A), (0xFF41, 0xFF5A),
    (0xFF66, 0xFF9F),
];

/// decimal digits (Nd) of the covered scripts
pub const DIGIT: &[(u32, u32)] = &[(0x30, 0x39), (0x660, 0x669), (0x6F0, 0x6F9), (0x966, 0x96F), (0xE50, 0xE59), (0xFF10, 0xFF19)];

/// white space as .NET's char.IsWhiteSpace (complete)
pub const SPACE: &[(u32, u32)] = &[
    (0x9, 0xD), (0x20, 0x20), (0x85, 0x85), (0xA0, 0xA0), (0x1680, 0x1680), (0x2000, 0x200A), (0x2028, 0x2029),
    (0x202F, 0x202F), (0x205F, 0x205F), (0x3000, 0x3000),
];

/// punctuation (Pc Pd Ps Pe Pi Pf Po); symbols such as $ + < = > ^ ` | ~ are not punctuation
pub const PUNCT: &[(u32, u32)] = &[
    (0x21, 0x23), (0x25, 0x2A), (0x2C, 0x2F), (0x3A, 0x3B), (0x3F, 0x40), (0x5B, 0x5D), (0x5F, 0x5F), (0x7B, 0x7B),
    (0x7D, 0x7D), (0xA1, 0xA1), (0xA7, 0xA7), (0xAB, 0xAB), (0xB6, 0xB7), (0xBB, 0xBB), (0xBF, 0xBF), (0x37E, 0x37E),
    (0x387, 0x387), (0x55A, 0x55F), (0x589, 0x58A), (0x5BE, 0x5BE), (0x5C0, 0x5C0), (0x5C3, 0x5C3), (0x5C6, 0x5C6),
    (0x5F3, 0x5F4), (0x60C, 0x60D), (0x61B, 0x61B), (0x61E, 0x61F), (0x66A, 0x66D), (0x6D4, 0x6D4), (0x964, 0x965),
    (0x970, 0x970), (0xE4F, 0xE4F), (0xE5A, 0xE5B), (0x2010, 0x2027), (0x2030, 0x2043), (0x2045, 0x2051),
    (0x2053, 0x205E), (0x3001, 0x3003), (0x3008, 0x3011), (0x3014, 0x301F), (0x30FB, 0x30FB), (0xFF01, 0xFF03),
    (0xFF05, 0xFF0A), (0xFF0C, 0xFF0F), (0xFF1A, 0xFF1B), (0xFF1F, 0xFF20), (0xFF3B, 0xFF3D), (0xFF3F, 0xFF3F),
    (0xFF5B, 0xFF5B), (0xFF5D, 0xFF5D), (0xFF5F, 0xFF65),
];

/// Lower-case letters and their upper case: (lo, hi, delta to upper, step, invertible).
/// step 1: every code point of [lo, hi]; step 2: every other one, starting at lo.
/// Not invertible: several lower-case letters share the upper case (µ and μ, ς and σ, ı and i,
/// ſ and s): ToLower of that upper-case letter uses the invertible entry.
pub const CASE: &[(u32, u32, i32, u32, bool)] = &[
    (0x61, 0x7A, -32, 1, true),
    (0xB5, 0xB5, 743, 1, false),
    (0xE0, 0xF6, -32, 1, true),
    (0xF8, 0xFE, -32, 1, true),
    (0xFF, 0xFF, 121, 1, true),
    (0x101, 0x12F, -1, 2, true),
    (0x131, 0x131, -232, 1, false),
    (0x133, 0x137, -1, 2, true),
    (0x13A, 0x148, -1, 2, true),
    (0x14B, 0x177, -1, 2, true),
    (0x17A, 0x17E, -1, 2, true),
    (0x17F, 0x17F, -300, 1, false),
    (0x3AC, 0x3AC, -38, 1, true),
    (0x3AD, 0x3AF, -37, 1, true),
    (0x3B1, 0x3C1, -32, 1, true),
    (0x3C2, 0x3C2, -31, 1, false),
    (0x3C3, 0x3CB, -32, 1, true),
    (0x3CC, 0x3CC, -64, 1, true),
    (0x3CD, 0x3CE, -63, 1, true),
    (0x430, 0x44F, -32, 1, true),
    (0x450, 0x45F, -80, 1, true),
    (0x461, 0x481, -1, 2, true),
    (0x48B, 0x4BF, -1, 2, true),
    (0x4C2, 0x4CE, -1, 2, true),
    (0x4D1, 0x52F, -1, 2, true),
    (0x561, 0x586, -48, 1, true),
    (0xFF41, 0xFF5A, -32, 1, true),
];

/// lower-case letters without an upper-case form
pub const LOWER_ONLY: &[(u32, u32)] = &[(0xDF, 0xDF), (0x138, 0x138), (0x149, 0x149)];

fn in_ranges(t: &[(u32, u32)], c: u32) -> bool {
    t.iter().any(|&(lo, hi)| c >= lo && c <= hi)
}

fn upper_of(c: u32) -> Option<u32> {
    CASE.iter()
        .find(|&&(lo, hi, _, step, _)| c >= lo && c <= hi && (c - lo) % step == 0)
        .map(|&(_, _, d, _, _)| (c as i64 + d as i64) as u32)
}

fn lower_of(c: u32) -> Option<u32> {
    CASE.iter()
        .find(|&&(lo, hi, d, step, inv)| {
            let l = c as i64 - d as i64;
            inv && l >= lo as i64 && l <= hi as i64 && (l as u32 - lo) % step == 0
        })
        .map(|&(_, _, d, _, _)| (c as i64 - d as i64) as u32)
}

pub fn is_letter(c: u32) -> bool {
    in_ranges(LETTER, c)
}
pub fn is_digit(c: u32) -> bool {
    in_ranges(DIGIT, c)
}
pub fn is_white_space(c: u32) -> bool {
    in_ranges(SPACE, c)
}
pub fn is_punctuation(c: u32) -> bool {
    in_ranges(PUNCT, c)
}
pub fn is_upper(c: u32) -> bool {
    lower_of(c).is_some()
}
pub fn is_lower(c: u32) -> bool {
    upper_of(c).is_some() || in_ranges(LOWER_ONLY, c)
}
pub fn to_upper(c: u32) -> u32 {
    upper_of(c).unwrap_or(c)
}
pub fn to_lower(c: u32) -> u32 {
    lower_of(c).unwrap_or(c)
}

/// (char)n: a code point stays as it is; anything else wraps to 16 bits like C#'s unchecked cast
pub fn to_char(n: i32) -> i32 {
    if (0..=0x10FFFF).contains(&n) {
        n
    } else {
        n & 0xFFFF
    }
}

/// a char as text: surrogate halves (which UTF-8 cannot hold) become U+FFFD
pub fn char_string(c: i32) -> String {
    char::from_u32(c as u32).unwrap_or('\u{FFFD}').to_string()
}

pub fn str_upper(s: &str) -> String {
    s.chars().map(|c| char::from_u32(to_upper(c as u32)).unwrap_or(c)).collect()
}
pub fn str_lower(s: &str) -> String {
    s.chars().map(|c| char::from_u32(to_lower(c as u32)).unwrap_or(c)).collect()
}

/// The tables as C declarations, emitted ahead of the string runtime (runtime/sijo_str.h).
pub fn c_tables() -> String {
    let ranges = |name: &str, t: &[(u32, u32)]| {
        let items: Vec<String> = t.iter().map(|(a, b)| format!("{{0x{:x}u, 0x{:x}u}}", a, b)).collect();
        format!("static const uint32_t {}[][2] = {{{}}};\n#define {}_N {}\n", name, items.join(", "), name.to_ascii_uppercase(), t.len())
    };
    let mut o = String::from("/* character classes and case pairs (generated from src/uchar.rs) */\n");
    o.push_str(&ranges("sijo_uc_letter", LETTER));
    o.push_str(&ranges("sijo_uc_digit", DIGIT));
    o.push_str(&ranges("sijo_uc_space", SPACE));
    o.push_str(&ranges("sijo_uc_punct", PUNCT));
    o.push_str(&ranges("sijo_uc_loweronly", LOWER_ONLY));
    let items: Vec<String> = CASE.iter().map(|(lo, hi, d, st, inv)| format!("{{0x{:x}, 0x{:x}, {}, {}, {}}}", lo, hi, d, st, *inv as i32)).collect();
    o.push_str(&format!("static const int32_t sijo_uc_case[][5] = {{{}}};\n#define SIJO_UC_CASE_N {}\n", items.join(", "), CASE.len()));
    // the one-character strings of ASCII, each followed by a NUL (char -> string without allocating)
    let lit: String = (0..128).map(|i| format!("\\{:03o}\\000", i)).collect();
    o.push_str(&format!("static const char sijo_uc_ascii1[] = \"{}\";\n", lit));
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn case_pairs_round_trip() {
        for c in 0..0x11000u32 {
            if let Some(u) = upper_of(c) {
                if CASE.iter().any(|e| e.4 && c >= e.0 && c <= e.1 && (c - e.0) % e.3 == 0) {
                    assert_eq!(lower_of(u), Some(c), "{:x}", c);
                }
            }
        }
        assert_eq!(to_upper('ÿ' as u32), 0x178);
        assert_eq!(to_lower(0x3A3), 0x3C3);
        assert_eq!(to_upper(0x3C2), 0x3A3);
        assert_eq!(str_upper("straße ÿ ıi"), "STRAßE Ÿ II");
        assert!(is_letter('가' as u32) && is_letter('a' as u32) && !is_letter('1' as u32));
        assert!(is_punctuation('!' as u32) && !is_punctuation('+' as u32) && !is_punctuation('$' as u32));
    }
}
