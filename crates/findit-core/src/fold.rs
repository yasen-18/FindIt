//! Unicode-aware text folding used for case- and accent-insensitive matching.
//!
//! Folding works one source character at a time, so `fold(a + b) == fold(a) + fold(b)`.
//! That property lets the index store pre-folded names and lets the search engine
//! build folded paths by joining folded components.
//!
//! * Every character is canonically decomposed (NFD), so composed (`é`) and
//!   decomposed (`e` + U+0301) spellings fold to the same text.
//! * In loose mode, combining marks that follow a Latin, Greek or Cyrillic base
//!   letter are dropped (`café` matches `cafe`). Marks in other scripts
//!   (Devanagari vowel signs, Thai, ...) carry meaning and are kept.
//! * Case-insensitive mode lowercases with full Unicode rules and maps the Greek
//!   final sigma to the normal sigma.

use unicode_normalization::char::{decompose_canonical, is_combining_mark};

/// How strictly text is compared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FoldMode {
    pub case_sensitive: bool,
    pub diacritics_sensitive: bool,
}

impl FoldMode {
    /// Case- and accent-insensitive. This is the form stored in the index.
    pub const LOOSE: FoldMode = FoldMode { case_sensitive: false, diacritics_sensitive: false };

    pub fn is_loose(self) -> bool {
        !self.case_sensitive && !self.diacritics_sensitive
    }
}

/// Latin, Greek and Cyrillic letters: accents on these are stripped in loose mode.
fn strips_marks(c: char) -> bool {
    matches!(c as u32,
        0x41..=0x5A | 0x61..=0x7A | 0xC0..=0x24F | 0x250..=0x2AF
        | 0x370..=0x3FF | 0x400..=0x52F | 0x1C80..=0x1C8F
        | 0x1E00..=0x1FFF | 0x2C60..=0x2C7F | 0xA640..=0xA69F
        | 0xA720..=0xA7FF | 0xAB30..=0xAB6F)
}

/// Calls `emit(folded_char, source_byte_offset)` for every folded output character.
#[inline]
fn fold_chars<F: FnMut(char, usize)>(s: &str, mode: FoldMode, mut emit: F) {
    let strip = !mode.diacritics_sensitive;
    let mut after_lgc = false;
    for (pos, c) in s.char_indices() {
        if c.is_ascii() {
            after_lgc = c.is_ascii_alphabetic();
            emit(if mode.case_sensitive { c } else { c.to_ascii_lowercase() }, pos);
            continue;
        }
        decompose_canonical(c, |d| {
            if is_combining_mark(d) {
                if !(strip && after_lgc) {
                    emit(d, pos);
                }
                return;
            }
            after_lgc = strips_marks(d);
            if mode.case_sensitive {
                emit(d, pos);
            } else if d == 'ς' {
                emit('σ', pos);
            } else {
                for l in d.to_lowercase() {
                    // 'İ' lowercases to 'i' + U+0307; drop the dot like any other accent.
                    if strip && after_lgc && is_combining_mark(l) {
                        continue;
                    }
                    emit(l, pos);
                }
            }
        });
    }
}

/// Folds `s` into a new string.
pub fn fold(s: &str, mode: FoldMode) -> String {
    let mut out = String::with_capacity(s.len());
    fold_into(s, mode, &mut out);
    out
}

/// Folds `s` and appends the result to `out`.
pub fn fold_into(s: &str, mode: FoldMode, out: &mut String) {
    if s.is_ascii() {
        if mode.case_sensitive {
            out.push_str(s);
        } else {
            out.reserve(s.len());
            out.extend(s.bytes().map(|b| b.to_ascii_lowercase() as char));
        }
        return;
    }
    fold_chars(s, mode, |c, _| out.push(c));
}

/// Folds `s` and records, for every output byte, the byte offset of the source
/// character that produced it. Used to map highlight ranges back to the name.
pub fn fold_with_map(s: &str, mode: FoldMode) -> (String, Vec<u32>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    fold_chars(s, mode, |c, pos| {
        let before = out.len();
        out.push(c);
        map.resize(map.len() + (out.len() - before), pos as u32);
    });
    (out, map)
}

/// Maps the folded byte range `[start, end)` back to a byte range in `src`.
pub fn map_range(src: &str, map: &[u32], start: usize, end: usize) -> Option<(usize, usize)> {
    if start >= end || end > map.len() {
        return None;
    }
    let s = map[start] as usize;
    let last = map[end - 1] as usize;
    let mut e = last + src.get(last..)?.chars().next().map_or(0, char::len_utf8);
    // Include accents that were folded away, so "cafe" highlights all of "café".
    while let Some(c) = src.get(e..).and_then(|r| r.chars().next()) {
        if !is_combining_mark(c) {
            break;
        }
        e += c.len_utf8();
    }
    Some((s, e))
}

/// Simple one-to-one uppercase mapping, close to the NTFS upcase table: a character
/// whose uppercase form is longer than one character (like `ß`) stays unchanged.
#[inline]
pub fn simple_upper(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_uppercase();
    }
    let mut it = c.to_uppercase();
    match (it.next(), it.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// Case-insensitive comparison with Windows file-name semantics.
pub fn eq_ignore_case(a: &str, b: &str) -> bool {
    if a.len() == b.len() && a.eq_ignore_ascii_case(b) {
        return true;
    }
    let mut ai = a.chars();
    let mut bi = b.chars();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if simple_upper(x) == simple_upper(y) => {}
            _ => return false,
        }
    }
}

/// True for characters that are part of a word (used by whole-word matching and ranking).
#[inline]
pub fn is_word_char(c: char) -> bool {
    c.is_alphanumeric()
}

/// Returns the character that ends right before byte `pos` of valid UTF-8 `s`.
pub fn char_before(s: &[u8], pos: usize) -> Option<char> {
    if pos == 0 || pos > s.len() {
        return None;
    }
    let mut i = pos - 1;
    while i > 0 && (s[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    std::str::from_utf8(&s[i..pos]).ok()?.chars().next()
}

/// Returns the character that starts at byte `pos` of valid UTF-8 `s`.
pub fn char_at(s: &[u8], pos: usize) -> Option<char> {
    if pos >= s.len() {
        return None;
    }
    let len = match s[pos] {
        b if b < 0x80 => 1,
        b if b >= 0xF0 => 4,
        b if b >= 0xE0 => 3,
        _ => 2,
    };
    std::str::from_utf8(&s[pos..(pos + len).min(s.len())]).ok()?.chars().next()
}

/// True when byte range `[start, end)` of `s` starts and ends at word boundaries.
pub fn is_whole_word(s: &[u8], start: usize, end: usize) -> bool {
    !char_before(s, start).is_some_and(is_word_char) && !char_at(s, end).is_some_and(is_word_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: FoldMode = FoldMode::LOOSE;

    #[test]
    fn ascii_lowercases() {
        assert_eq!(fold("Invoice_2024.PDF", L), "invoice_2024.pdf");
    }

    #[test]
    fn composed_and_decomposed_accents_match() {
        let composed = "Caf\u{e9}";
        let decomposed = "Cafe\u{301}";
        assert_eq!(fold(composed, L), fold(decomposed, L));
        assert_eq!(fold(composed, L), "cafe");
        let strict = FoldMode { case_sensitive: false, diacritics_sensitive: true };
        assert_eq!(fold(composed, strict), fold(decomposed, strict));
        assert_ne!(fold(composed, strict), "cafe");
    }

    #[test]
    fn cyrillic_greek_and_final_sigma() {
        assert_eq!(fold("ДОКУМЕНТ", L), "документ");
        assert_eq!(fold("ΟΔΌΣ", L), "οδοσ");
        assert_eq!(fold("οδός", L), "οδοσ");
        assert_eq!(fold("Ёлка", L), fold("елка", L));
    }

    #[test]
    fn cjk_and_emoji_survive() {
        assert_eq!(fold("報告書📄", L), "報告書📄");
        let hangul = fold("한글", L);
        assert_eq!(hangul, fold("\u{1112}\u{1161}\u{11AB}\u{1100}\u{1173}\u{11AF}", L));
    }

    #[test]
    fn devanagari_marks_are_kept() {
        assert_ne!(fold("कि", L), fold("क", L));
    }

    #[test]
    fn turkish_dotted_i() {
        assert_eq!(fold("İstanbul", L), "istanbul");
    }

    #[test]
    fn case_sensitive_keeps_case() {
        let m = FoldMode { case_sensitive: true, diacritics_sensitive: false };
        assert_eq!(fold("Résumé", m), "Resume");
    }

    #[test]
    fn map_points_back_to_source() {
        let src = "Ünïcode";
        let (f, map) = fold_with_map(src, L);
        assert_eq!(f, "unicode");
        let pos = f.find("ico").unwrap();
        let (s, e) = map_range(src, &map, pos, pos + 3).unwrap();
        assert_eq!(&src[s..e], "ïco");
    }

    #[test]
    fn ignore_case_compare() {
        assert!(eq_ignore_case("README.md", "readme.MD"));
        assert!(eq_ignore_case("ДОКУМЕНТ", "документ"));
        assert!(!eq_ignore_case("a", "b"));
        assert!(!eq_ignore_case("ab", "a"));
    }

    #[test]
    fn word_boundaries() {
        let s = "my report-2024.pdf".as_bytes();
        assert!(is_whole_word(s, 3, 9));
        assert!(!is_whole_word(s, 4, 9));
    }
}
