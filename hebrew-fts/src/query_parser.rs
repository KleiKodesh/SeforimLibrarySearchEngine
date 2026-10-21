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
//! - A plain literal of fewer than 2 or 30+ letters is dropped (the index has
//!   none).
//! - Marks stripped, ASCII lowercased, intra-word quotes kept as the canonical
//!   `"` the index stores, a trailing quote dropped, other chars dropped.
//!
//! One deviation, for vocalized search: with `keep_marks` (an index that keeps
//! nikud / te'amim) a plain word keeps its marks, so the walk can require or
//! ignore them. FtsLib's index has no marks, so its path never sets it.
//!
//! The parser only tokenizes: grouping (`|`) and the per-word options are the
//! engine's job ([`QueryToken::index`] numbers the words for per-word options).

use crate::chars::{is_intra_word_quote, is_letter, is_strippable_mark};
use unicode_general_category::{get_general_category, GeneralCategory};

/// FtsLib allowed `~3`; tantivy's Levenshtein DFA stops at 2.
pub const MAX_FUZZY_DISTANCE: u8 = 2;

/// One whitespace token of the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryToken {
    /// A bare `|` (one or more pipes): ORs its neighbours.
    Pipe,
    /// A word token. `index` counts the word tokens from 0 (pipes excluded,
    /// dropped words included), so it is the word's place as the user typed
    /// it. `term` is None when the token cannot search (no letters, too
    /// short / long).
    Word { index: usize, term: Option<QueryTerm> },
}

/// What one word token searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryTerm {
    /// A literal word (2..29 letters).
    Word(String),
    /// A `*` / `?` pattern.
    Wildcard(String),
    /// `word~N`.
    Fuzzy { word: String, distance: u8 },
    /// `%word` / `word%` / `%word%`.
    Grammar { word: String, prefixes: bool, suffixes: bool },
}

/// Tokenizes `query` by FtsLib's rules.
pub fn tokenize_query(query: &str, keep_marks: bool) -> Vec<QueryToken> {
    let split = split_on_index_separators(query).replace('|', " | ");
    let mut out = Vec::new();
    let mut index = 0;
    for raw in split.split([' ', '\t', '\r', '\n']).filter(|t| !t.is_empty()) {
        if raw.chars().all(|c| c == '|') {
            out.push(QueryToken::Pipe);
            continue;
        }
        out.push(QueryToken::Word {
            index,
            term: parse_term(raw, keep_marks),
        });
        index += 1;
    }
    out
}

fn is_non_spacing_mark(c: char) -> bool {
    c as u32 > 127 && get_general_category(c) == GeneralCategory::NonspacingMark
}

/// FtsLib `SplitOnIndexSeparators`, verbatim rules.
fn split_on_index_separators(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 8);
    let mut prev_letter: Option<char> = None;
    let mut prev: Option<char> = None;
    for c in query.chars() {
        let keep = c != '\u{05BE}'
            && (c.is_whitespace()
                || is_letter(c)
                || is_strippable_mark(c)
                || is_intra_word_quote(c)
                || matches!(c, '*' | '?' | '~' | '%' | '|')
                || (c.is_ascii_digit() && prev == Some('~'))
                || is_non_spacing_mark(c));
        if keep {
            if is_letter(c) {
                if let Some(p) = prev_letter {
                    if (c >= 'א') != (p >= 'א') {
                        out.push(' ');
                    }
                }
                prev_letter = Some(c);
            } else if !(is_strippable_mark(c) || is_intra_word_quote(c) || is_non_spacing_mark(c)) {
                prev_letter = None;
            }
            out.push(c);
        } else {
            out.push(' ');
            prev_letter = None;
        }
        prev = Some(c);
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
    let letters = normalised.chars().filter(|&c| is_letter(c)).count();
    let wildcard = normalised.contains(['*', '?']);
    if letters == 0 && !wildcard {
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
    if !(2..30).contains(&letters) {
        return None;
    }
    Some(QueryTerm::Word(if keep_marks { normalise(text, true) } else { normalised }))
}

/// FtsLib `Normalise`: strips marks (or keeps them, `keep_marks`), lowercases
/// ASCII, keeps `*` `?` and intra-word quotes (as `"`), drops the rest; a
/// trailing quote is punctuation.
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
        } else if is_letter(c) {
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
                None,
                Some(Word("תורה".into())),
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
        assert_eq!(words, ["0:רמב\"ם", "|", "1:abc", "2:תורה", "3:יצחק", "4:abc", "5:שלום"]);
        // keep_marks keeps a plain word's marks
        let t = tokenize_query("תּוֹרָה", true);
        assert_eq!(
            t,
            vec![QueryToken::Word { index: 0, term: Some(QueryTerm::Word("תּוֹרָה".into())) }]
        );
    }
}
