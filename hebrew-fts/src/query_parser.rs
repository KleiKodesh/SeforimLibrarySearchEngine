//! The query string syntax: FtsLib's `QueryParser` operators over Otzaria's
//! word rule (FtsLib's parser follows the same rules).
//!
//! Rules:
//! - A paired reading `(X) [Y]` keeps only Y (Otzaria's
//!   `keep_second_of_paired_readings`).
//! - The query is cut into words as typed, on space / tab / CR / LF and `|`.
//!   A bare `|` joins its neighbours into one OR slot: `a | b c` = {a, b}
//!   AND c.
//! - A typed word is cut into pieces at every char that is not a word char,
//!   a quote, a transparent char or a query operator (`?`, `%`). On each
//!   piece: `%` at the start / end = FtsLib's grammatical prefixes /
//!   suffixes; a `~` / `~N` suffix = fuzzy, N clamped to
//!   [`MAX_FUZZY_DISTANCE`] (FtsLib allowed 3).
//! - The rest of a piece is cut into words by the text's word rule
//!   ([`crate::word_rule`], `*` and `?` counting as word chars): other
//!   transparent chars (`. [ ] ~ ^ $ \ +`, bidi controls) join and vanish,
//!   quotes are kept as the index keeps them (`'`, `"`, a trailing geresh
//!   kept, a trailing gershayim dropped), typographic quotes and
//!   presentation forms fold, marks are stripped, every script is lowercased
//!   and none splits from another.
//! - `*` or `?` in a word = wildcard (overrides `~` and `%`).
//! - A literal of any length is a word, one letter or a digit run included.
//!   One whose term has [`MAX_TERM_BYTES`](crate::chars::MAX_TERM_BYTES)
//!   bytes or more has no index term: it is [`QueryTerm::Unindexed`], a
//!   word that keeps its one position (the text keeps the position of such a
//!   word too).
//!
//! One addition, for vocalized search: with `keep_marks` (an index that keeps
//! nikud / te'amim) a plain word keeps its marks, so the walk can require or
//! ignore them. FtsLib's index has no marks, so its path never sets it.
//!
//! The parser only tokenizes: grouping (`|`) and the per-word options are the
//! engine's job ([`QueryToken::index`] numbers the words for per-word options).

use crate::chars::{is_indexable_len, is_quote, is_transparent, is_word_char};
use crate::word_rule::{keep_second_of_paired_readings, next_token_boundaries, Normalizer};

/// FtsLib allowed `~3`; tantivy's Levenshtein DFA stops at 2.
pub const MAX_FUZZY_DISTANCE: u8 = 2;

/// One whitespace token of the query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryToken {
    /// A bare `|` (one or more pipes): ORs its neighbours.
    Pipe,
    /// A word token. `index` is the place, from 0, of the word as the user
    /// typed it (see [`tokenize_query`]); the words cut from one typed word
    /// share it. `term` is None when the token cannot search (no letters or
    /// digits).
    Word { index: usize, term: Option<QueryTerm> },
}

/// What one word token searches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryTerm {
    /// A literal word (letters and digits, any length).
    Word(String),
    /// A literal whose term is too long to be indexed: it matches no term
    /// but keeps its one position (the text keeps such a word's position).
    Unindexed(String),
    /// A `*` / `?` pattern.
    Wildcard(String),
    /// `word~N`.
    Fuzzy { word: String, distance: u8 },
    /// `%word` / `word%` / `%word%`.
    Grammar { word: String, prefixes: bool, suffixes: bool },
}

/// Tokenizes `query`.
///
/// Word index rule: after the paired readings are reduced, the query is cut
/// into the words AS TYPED, on space / tab / CR / LF and `|` (a `|` is never a
/// word); [`QueryToken::Word::index`] is the typed word's place. The word
/// rule then cuts a typed word into words (maqaf, punctuation: two words
/// joined by a maqaf are two words); every word carries its typed word's
/// index, so per-word options keyed by index never shift. A typed word with
/// nothing searchable (`,`) still takes its index.
pub fn tokenize_query(query: &str, keep_marks: bool) -> Vec<QueryToken> {
    let query = keep_second_of_paired_readings(query);
    let mut out = Vec::new();
    let mut index = 0;
    let mut start = 0;
    let mut push_typed = |out: &mut Vec<QueryToken>, typed: &str| {
        if typed.is_empty() {
            return;
        }
        let mut any = false;
        for piece in typed.split(|c: char| !is_piece_char(c)).filter(|t| !t.is_empty()) {
            for term in parse_piece(piece, keep_marks) {
                out.push(QueryToken::Word { index, term });
                any = true;
            }
        }
        if !any {
            // Nothing searchable: the typed word keeps its index.
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

/// A char that stays inside a piece: a word char, a quote, a transparent
/// char, or a query operator (`?` wildcard, `%` grammar; `*` and `~` are
/// transparent chars).
fn is_piece_char(c: char) -> bool {
    is_word_char(c) || is_quote(c) || is_transparent(c) || c == '?' || c == '%'
}

/// One piece: its operators, then its words by the word rule. Every word of
/// the piece takes its operators. A piece with no word gives one None.
fn parse_piece(raw: &str, keep_marks: bool) -> Vec<Option<QueryTerm>> {
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

    let plain = Normalizer { keep_marks: false, wildcards: true };
    let marked = Normalizer { keep_marks: true, wildcards: true };
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some((s, e)) = next_token_boundaries(text, pos, true) {
        pos = e;
        let word = &text[s..e];
        let normalised = plain.normalize(word).text;
        if normalised.contains(['*', '?']) {
            out.push(Some(QueryTerm::Wildcard(normalised)));
        } else if let Some(distance) = fuzzy {
            out.push(Some(QueryTerm::Fuzzy { word: normalised, distance }));
        } else if prefixes || suffixes {
            out.push(Some(QueryTerm::Grammar { word: normalised, prefixes, suffixes }));
        } else {
            let searched = if keep_marks { marked.normalize(word) } else { plain.normalize(word) };
            out.push(Some(if is_indexable_len(searched.term_bytes) {
                QueryTerm::Word(searched.text)
            } else {
                QueryTerm::Unindexed(searched.text)
            }));
        }
    }
    if out.is_empty() {
        out.push(None);
    }
    out
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

    fn words(q: &str) -> Vec<String> {
        tokenize_query(q, false)
            .into_iter()
            .map(|t| match t {
                QueryToken::Pipe => "|".to_string(),
                QueryToken::Word { index, term: Some(QueryTerm::Word(w)) } => format!("{index}:{w}"),
                QueryToken::Word { index, term: Some(QueryTerm::Unindexed(_)) } => format!("{index}:long"),
                QueryToken::Word { index, term: None } => format!("{index}-"),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    const SHALOM: &str = "\u{05E9}\u{05DC}\u{05D5}\u{05DD}";
    const TORAH: &str = "\u{05EA}\u{05D5}\u{05E8}\u{05D4}";

    #[test]
    fn ftslib_syntax() {
        use QueryTerm::*;
        let q = format!(
            "\u{05E9}\u{05DC}\u{05D5}* \u{05E9}?\u{05DC}\u{05D5}\u{05DD} {SHALOM}~ {SHALOM}~2 {SHALOM}~3 %{TORAH} {TORAH}% %{TORAH}% \u{05D0} {TORAH}"
        );
        assert_eq!(
            terms(&q),
            vec![
                Some(Wildcard("\u{05E9}\u{05DC}\u{05D5}*".into())),
                Some(Wildcard("\u{05E9}?\u{05DC}\u{05D5}\u{05DD}".into())),
                Some(Fuzzy { word: SHALOM.into(), distance: 1 }),
                Some(Fuzzy { word: SHALOM.into(), distance: 2 }),
                Some(Fuzzy { word: SHALOM.into(), distance: 2 }),
                Some(Grammar { word: TORAH.into(), prefixes: true, suffixes: false }),
                Some(Grammar { word: TORAH.into(), prefixes: false, suffixes: true }),
                Some(Grammar { word: TORAH.into(), prefixes: true, suffixes: true }),
                Some(Word("\u{05D0}".into())),
                Some(Word(TORAH.into())),
            ]
        );
    }

    #[test]
    fn every_word_is_a_word_long_ones_keep_a_slot() {
        let long = "\u{05D0}".repeat(63); // 126 bytes
        let too_long = "\u{05D0}".repeat(64); // 128 bytes
        let quoted = format!("{}\"{}", "\u{05D0}".repeat(32), "\u{05D0}".repeat(31)); // 127 bytes
        let q = format!("\u{05DE} \u{05D5} \u{05D4}' {long} , 12 \u{05D1}5 a5\u{05D1} {SHALOM}~12 {too_long} {quoted}");
        assert_eq!(
            words(&q),
            [
                "0:\u{05DE}".to_string(),
                "1:\u{05D5}".into(),
                "2:\u{05D4}'".into(),
                format!("3:{long}"),
                "4-".into(),
                "5:12".into(),
                "6:\u{05D1}5".into(),
                "7:a5\u{05D1}".into(),
                format!("8:{SHALOM}12"),
                "9:long".into(),
                format!("10:{quoted}"),
            ]
        );
    }

    #[test]
    fn separators_quotes_marks_and_pipes() {
        let t = words(&format!("\u{05E8}\u{05DE}\u{05D1}\"\u{05DD}|ABC\u{05BE}\u{05EA}\u{05BC}\u{05D5}\u{05B9}\u{05E8}\u{05B8}\u{05D4} \"\u{05D9}\u{05E6}\u{05D7}\u{05E7}\" abc{SHALOM}"));
        assert_eq!(
            t,
            [
                "0:\u{05E8}\u{05DE}\u{05D1}\"\u{05DD}".to_string(),
                "|".into(),
                "1:abc".into(),
                format!("1:{TORAH}"),
                "2:\u{05D9}\u{05E6}\u{05D7}\u{05E7}".into(),
                format!("3:abc{SHALOM}"),
            ]
        );
        let idx = words("\u{05D0}\u{05D1}|\u{05D2}\u{05D3} \u{05D0} , \u{05D4}\u{05D5}\u{05BE}\u{05D6}\u{05D7}");
        assert_eq!(idx, ["0:\u{05D0}\u{05D1}", "|", "1:\u{05D2}\u{05D3}", "2:\u{05D0}", "3-", "4:\u{05D4}\u{05D5}", "4:\u{05D6}\u{05D7}"]);
        let t = tokenize_query("\u{05EA}\u{05BC}\u{05D5}\u{05B9}\u{05E8}\u{05B8}\u{05D4}", true);
        assert_eq!(t, vec![QueryToken::Word { index: 0, term: Some(QueryTerm::Word("\u{05EA}\u{05BC}\u{05D5}\u{05B9}\u{05E8}\u{05B8}\u{05D4}".into())) }]);
    }

    #[test]
    fn otzaria_word_rule_in_queries() {
        // rule 1: period, brackets, bidi controls join
        assert_eq!(words("\u{05E4}.\u{05D1}.\u{05D9} \u{05D0}[\u{05D1}]\u{05D2} \u{05D3}\u{200F}\u{05D4}"), ["0:\u{05E4}\u{05D1}\u{05D9}", "1:\u{05D0}\u{05D1}\u{05D2}", "2:\u{05D3}\u{05D4}"]);
        // rule 3: trailing geresh kept, trailing gershayim dropped, two gereshes = gershayim
        assert_eq!(words("\u{05EA}\u{05D5}\u{05E1}' \u{05D9}\u{05E6}\u{05D7}\u{05E7}\" \u{05E8}''\u{05D9}"), ["0:\u{05EA}\u{05D5}\u{05E1}'", "1:\u{05D9}\u{05E6}\u{05D7}\u{05E7}", "2:\u{05E8}\"\u{05D9}"]);
        // rule 4: typographic quotes, presentation forms
        assert_eq!(words("\u{05E8}\u{05E9}\u{201D}\u{05D9} \u{FB2B}\u{05DC}\u{05D5}\u{05DD}"), ["0:\u{05E8}\u{05E9}\"\u{05D9}", format!("1:{SHALOM}").as_str()]);
        // rule 2: other scripts are words, lowercased
        assert_eq!(words("CAF\u{00C9}"), ["0:caf\u{00E9}"]);
        // rule 5: a paired reading keeps its second reading
        assert_eq!(words("\u{05D0} (\u{05D1}\u{05D2}) [\u{05D3}\u{05D4}] \u{05D5}"), ["0:\u{05D0}", "1:\u{05D3}\u{05D4}", "2:\u{05D5}"]);
        // wildcards keep their meaning
        assert_eq!(terms("\u{05D0}.\u{05D1}*"), [Some(QueryTerm::Wildcard("\u{05D0}\u{05D1}*".into()))]);
    }
}
