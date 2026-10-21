//! Port of FtsLib's `QueryParser` (KitveiHakodesh): the query string syntax.
//!
//! Rules (FtsLib's, unchanged):
//! - Every char the indexer treats as a word separator becomes a space
//!   (`SplitOnIndexSeparators`), then tokens split on space / tab / CR / LF.
//! - A bare `|` token joins its neighbours into one OR slot: `a | b c` =
//!   {a, b} AND c.
//! - `*` or `?` → wildcard (overrides `~` and `%`).
//! - `~` / `~N` suffix → fuzzy, N clamped to [`MAX_FUZZY_DISTANCE`] (FtsLib
//!   allowed 3).
//! - `%` at the start / end → FtsLib's grammatical prefixes / suffixes.
//! - Words are runs of letters and digits (FtsLib's word model = Otzaria's):
//!   a literal of any length is a word, one letter or a digit run included;
//!   only one whose term is over [`MAX_TERM_BYTES`] is dropped (the index has
//!   no such term; Otzaria's `RemoveLongFilter` drops it from queries too).
//! - Marks stripped, ASCII lowercased, digits kept, intra-word quotes kept as
//!   the canonical `"` the index stores, a trailing quote dropped, other
//!   chars dropped.
//!
//! One deviation, for vocalized search: with `keep_marks` (an index that keeps
//! nikud / te'amim) a plain word keeps its marks, so the walk can require or
//! ignore them. FtsLib's index has no marks, so its path never sets it.
//!
//! The parser only tokenizes: grouping (`|`) and the per-word options are the
//! engine's job ([`QueryToken::index`] numbers the words for per-word options).

use crate::chars::{is_digit, is_intra_word_quote, is_letter, is_strippable_mark, is_word_char, MAX_TERM_BYTES};
use unicode_general_category::{get_general_category, GeneralCategory};

/// FtsLib allowed `~3`; tantivy's Levenshtein DFA stops at 2.
pub const MAX_FUZZY_DISTANCE: u8 = 2;

/// One whitespace token of the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryToken {
    /// A bare `|` (one or more pipes): ORs its neighbours.
    Pipe,
    /// A word token. `index` is the place, from 0, of the word as the user
    /// typed it (see [`tokenize_query`]); the sub-words FtsLib's separators
    /// cut from one typed word share it. `term` is None when the token
    /// cannot search (no letters or digits, or a term over [`MAX_TERM_BYTES`]).
    Word { index: usize, term: Option<QueryTerm> },
}

/// What one word token searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryTerm {
    /// A literal word (letters and digits, any length up to [`MAX_TERM_BYTES`]).
    Word(String),
    /// A `*` / `?` pattern.
    Wildcard(String),
    /// `word~N`.
    Fuzzy { word: String, distance: u8 },
    /// `%word` / `word%` / `%word%`.
    Grammar { word: String, prefixes: bool, suffixes: bool },
}

/// Tokenizes `query` by FtsLib's rules.
///
/// Word index rule: the query is first cut into the words AS TYPED, on
/// space / tab / CR / LF and `|` (a `|` is never a word); [`QueryToken::Word::index`]
/// is the typed word's place. FtsLib's separator rules then split a typed
/// word into sub-words (maqaf, punctuation, a Hebrew/Latin boundary:
/// `ABC־תורה` → `abc`, `תורה`); every sub-word carries its typed word's
/// index, so per-word options keyed by index never shift. The split is the
/// same as on the whole string: these cuts already reset the separator
/// state. A typed word with nothing searchable (`,`, `א`) still takes its
/// index.
pub fn tokenize_query(query: &str, keep_marks: bool) -> Vec<QueryToken> {
    let mut out = Vec::new();
    let mut index = 0;
    let mut start = 0;
    let mut push_typed = |out: &mut Vec<QueryToken>, typed: &str| {
        if typed.is_empty() {
            return;
        }
        let split = split_on_index_separators(typed);
        let mut any = false;
        for raw in split.split(' ').filter(|t| !t.is_empty()) {
            out.push(QueryToken::Word { index, term: parse_term(raw, keep_marks) });
            any = true;
        }
        if !any {
            // Nothing left after the separators (e.g. `,`): still the
            // user's word, it holds its index and cannot search.
            out.push(QueryToken::Word { index, term: None });
        }
        index += 1;
    };
    for (i, c) in query.char_indices() {
        if matches!(c, '|' | ' ' | '\t' | '\r' | '\n') {
            push_typed(&mut out, &query[start..i]);
            if c == '|' && !matches!(out.last(), Some(QueryToken::Pipe)) {
                out.push(QueryToken::Pipe);
            }
            start = i + c.len_utf8();
        }
    }
    push_typed(&mut out, &query[start..]);
    out
}

fn is_non_spacing_mark(c: char) -> bool {
    c as u32 > 127 && get_general_category(c) == GeneralCategory::NonspacingMark
}

/// FtsLib `SplitOnIndexSeparators`, verbatim rules.
fn split_on_index_separators(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 8);
    let mut prev_letter: Option<char> = None;
    for c in query.chars() {
        let keep = c != '\u{05BE}'
            && (c.is_whitespace()
                || is_word_char(c)
                || is_strippable_mark(c)
                || is_intra_word_quote(c)
                || matches!(c, '*' | '?' | '~' | '%' | '|')
                || is_non_spacing_mark(c));
        if keep {
            if is_letter(c) {
                if let Some(p) = prev_letter {
                    if (c >= 'א') != (p >= 'א') {
                        out.push(' ');
                    }
                }
                prev_letter = Some(c);
            } else if !(is_strippable_mark(c) || is_digit(c) || is_intra_word_quote(c) || is_non_spacing_mark(c)) {
                // digits are script-neutral, as in the scanner
                prev_letter = None;
            }
            out.push(c);
        } else {
            out.push(' ');
            prev_letter = None;
        }
    }
    out
}

/// FtsLib `ParseToken` (ignore-quotes and walk options are applied by the
/// caller).
fn parse_term(raw: &str, keep_marks: bool) -> Option<QueryTerm> {
    let prefixes = raw.starts_with('%');
    let suffixes = raw.ends_with('%');
    let raw = if prefixes || suffixes { raw.trim_matches('%') } else { raw };

    let mut text = raw;
    let mut fuzzy = None;
    if let Some(tilde) = raw.rfind('~') {
        let suffix = &raw[tilde + 1..];
        let b = suffix.as_bytes();
        if b.is_empty() || (b.len() == 1 && (b'1'..=b'9').contains(&b[0])) {
            let d = if b.is_empty() { 1 } else { b[0] - b'0' };
            fuzzy = Some(d.min(MAX_FUZZY_DISTANCE));
            text = &raw[..tilde];
        }
    }

    let normalised = normalise(text, false);
    let word_chars = normalised.chars().filter(|&c| is_word_char(c)).count();
    let wildcard = normalised.contains(['*', '?']);
    if word_chars == 0 && !wildcard {
        return None;
    }
    if wildcard {
        return Some(QueryTerm::Wildcard(normalised));
    }
    if let Some(distance) = fuzzy {
        return Some(QueryTerm::Fuzzy { word: normalised, distance });
    }
    if prefixes || suffixes {
        return Some(QueryTerm::Grammar { word: normalised, prefixes, suffixes });
    }
    // The term as the index stores it (quotes kept): over the limit it is
    // not in the index (FtsLib `ParseToken`, Otzaria's RemoveLongFilter).
    if normalised.len() > MAX_TERM_BYTES {
        return None;
    }
    Some(QueryTerm::Word(if keep_marks { normalise(text, true) } else { normalised }))
}

/// FtsLib `Normalise`: strips marks (or keeps them, `keep_marks`), lowercases
/// ASCII, keeps letters, digits, `*` `?` and intra-word quotes (as `"`),
/// drops the rest; a trailing quote is punctuation.
fn normalise(token: &str, keep_marks: bool) -> String {
    let mut s = String::with_capacity(token.len());
    for c in token.chars() {
        if is_strippable_mark(c) {
            if keep_marks && !s.is_empty() {
                s.push(c);
            }
            continue;
        }
        if c == '*' || c == '?' {
            s.push(c);
        } else if is_word_char(c) {
            s.push(c.to_ascii_lowercase());
        } else if is_intra_word_quote(c) && !s.is_empty() {
            s.push('"');
        }
    }
    while s.ends_with('"') {
        s.pop();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(q: &str) -> Vec<Option<QueryTerm>> {
        tokenize_query(q, false)
            .into_iter()
            .map(|t| match t {
                QueryToken::Pipe => Some(QueryTerm::Word("|".into())),
                QueryToken::Word { term, .. } => term,
            })
            .collect()
    }

    #[test]
    fn ftslib_syntax() {
        use QueryTerm::*;
        assert_eq!(
            terms("שלו* ש?לום שלם~ שלם~2 שלם~3 %תורה תורה% %תורה% א תורה"),
            vec![
                Some(Wildcard("שלו*".into())),
                Some(Wildcard("ש?לום".into())),
                Some(Fuzzy { word: "שלם".into(), distance: 1 }),
                Some(Fuzzy { word: "שלם".into(), distance: 2 }),
                Some(Fuzzy { word: "שלם".into(), distance: 2 }),
                Some(Grammar { word: "תורה".into(), prefixes: true, suffixes: false }),
                Some(Grammar { word: "תורה".into(), prefixes: false, suffixes: true }),
                Some(Grammar { word: "תורה".into(), prefixes: true, suffixes: true }),
                Some(Word("א".into())),
                Some(Word("תורה".into())),
            ]
        );
    }

    #[test]
    fn every_word_is_a_word() {
        // one letter, digits, any length up to MAX_TERM_BYTES; only a typed
        // word with no letters or digits, or a term over the limit, is dropped
        let long = "א".repeat(64); // 128 bytes
        let too_long = "א".repeat(65);
        let quoted = format!("{}\"{}", "א".repeat(32), "א".repeat(32)); // 129 bytes
        let q = format!("משה ו ה' {long} , 12 ב5 a5ב שלום~12 {too_long} {quoted}");
        let got: Vec<_> = tokenize_query(&q, false)
            .into_iter()
            .map(|t| match t {
                QueryToken::Word { index, term: Some(QueryTerm::Word(w)) } => format!("{index}:{w}"),
                QueryToken::Word { index, term: None } => format!("{index}-"),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            got,
            [
                "0:משה".to_string(),
                "1:ו".into(),
                "2:ה".into(),
                format!("3:{long}"),
                "4-".into(),
                "5:12".into(),
                "6:ב5".into(),
                "7:a5".into(),
                "7:ב".into(),
                "8:שלום12".into(),
                "9-".into(),
                "10-".into(),
            ]
        );
    }

    #[test]
    fn separators_quotes_marks_and_pipes() {
        let t = tokenize_query("רמב\"ם|ABC־תּוֹרָה \"יצחק\" abcשלום", false);
        let words: Vec<_> = t
            .iter()
            .map(|t| match t {
                QueryToken::Pipe => "|".to_string(),
                QueryToken::Word { index, term: Some(QueryTerm::Word(w)) } => format!("{index}:{w}"),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(words, ["0:רמב\"ם", "|", "1:abc", "1:תורה", "2:יצחק", "3:abc", "3:שלום"]);
        // typed-word index: `|` (spaced or not) is no word; a
        // separator-only word keeps its index
        let idx: Vec<_> = tokenize_query("אב|גד א , הו־זח", false)
            .iter()
            .map(|t| match t {
                QueryToken::Pipe => "|".to_string(),
                QueryToken::Word { index, term } => format!("{index}{}", if term.is_some() { "" } else { "-" }),
            })
            .collect();
        assert_eq!(idx, ["0", "|", "1", "2", "3-", "4", "4"]);
        // keep_marks keeps a plain word's marks
        let t = tokenize_query("תּוֹרָה", true);
        assert_eq!(
            t,
            vec![QueryToken::Word { index: 0, term: Some(QueryTerm::Word("תּוֹרָה".into())) }]
        );
    }
}
