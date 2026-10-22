//! Expanding leading-`*` wildcards and fuzzy words through the FM-index
//! sidecar instead of an FST walk (FtsLib's `IsFmWildcard` /
//! `ExpandWildcardWithFm` / `ExpandFuzzy`).
//!
//! The sidecar only finds CANDIDATES (its text is the quote-stripped form of
//! each term). Every candidate is verified with the very automaton the walk
//! would have run, and the cap is applied with the walk's ordering, so the
//! result is the same term list the walk returns, found in a fraction of the
//! time: the walk visits every dictionary branch for a leading `*`, the
//! sidecar's backward search touches only the matching rows.

use std::cmp::Reverse;

use hebrew_fts::fuzzy::{Distance, LevenshteinDfa};
use hebrew_fts::wildcard::WildcardAutomaton;
use hebrew_fts::marks::{strip_mark_bytes, zero_width_chars};
use tantivy::termdict::TermDictionary;

use crate::fm_index::FmIndex;
use crate::fm_sidecar::text_form;
use crate::Result;

/// FtsLib `IsFmWildcard`: the leading-star shape the sidecar answers
/// exactly. No `?`, one literal core with `*` only at the start (suffix) or
/// at both ends (infix), core at least the anchor minimum. The core must be
/// representable in the sidecar's text: with the quote transparent the
/// pattern's quotes are dropped first; otherwise a quoted core goes to the
/// walk.
pub fn fm_wildcard_shape(pattern: &str, ignore_quotes: bool) -> Option<(String, bool)> {
    let stripped: String;
    let pattern = if ignore_quotes {
        stripped = pattern.chars().filter(|&c| !hebrew_fts::chars::is_term_quote(c)).collect();
        stripped.as_str()
    } else {
        pattern
    };
    if pattern.contains('?') || !pattern.starts_with('*') {
        return None;
    }
    let core = pattern.trim_matches('*');
    if core.chars().count() < hebrew_fts::wildcard::MIN_ANCHOR_LENGTH
        || core.contains('*')
        || core.contains(['"', '\''])
    {
        return None;
    }
    Some((core.to_string(), !pattern.ends_with('*')))
}

/// The dictionary term with rowid `rowid` (= ordinal + 1).
fn term_of(dict: &TermDictionary, rowid: u32, buf: &mut Vec<u8>) -> Result<bool> {
    buf.clear();
    Ok(dict.ord_to_term((rowid - 1) as u64, buf)?)
}

/// Keeps the best `cap` slots of `ranked` under the walk's order: lowest
/// rank, then shortest (quotes zero-width, and marks on an index that keeps
/// them), then highest doc count, then the stripped bytes. As in the walk's
/// cap, a term and its quoted or vocalized variants are ONE slot (FtsLib's
/// index stored only the quote-free term), ranked by their summed doc count;
/// a kept slot keeps all of them. Also returns whether the cap left out a
/// matching term (the walk's truncation flag).
fn cap_like_the_walk(
    dict: &TermDictionary,
    mut ranked: Vec<(u32, Vec<u8>)>,
    cap: usize,
    marked_terms: bool,
) -> Result<(Vec<Vec<u8>>, bool)> {
    let zero_width = zero_width_chars(marked_terms);
    let mut forms: std::collections::HashMap<Vec<u8>, (u32, usize, u64, Vec<Vec<u8>>)> =
        std::collections::HashMap::new();
    for (rank, term) in &ranked {
        let form = zero_width.strip(term).into_owned();
        let slot = forms
            .entry(form)
            .or_insert((u32::MAX, zero_width.term_len(term), 0, Vec::new()));
        slot.0 = slot.0.min(*rank);
        slot.3.push(term.clone());
    }
    if forms.len() <= cap {
        ranked.sort_unstable_by(|a, b| a.1.cmp(&b.1));
        return Ok((ranked.into_iter().map(|(_, t)| t).collect(), false));
    }
    let mut slots: Vec<(u32, usize, Reverse<u64>, Vec<u8>, Vec<Vec<u8>>)> = Vec::with_capacity(forms.len());
    for (form, (rank, chars, _, terms)) in forms {
        let mut doc_freq = 0u64;
        for t in &terms {
            doc_freq += u64::from(dict.get(t)?.map(|i| i.doc_freq).unwrap_or(0));
        }
        slots.push((rank, chars, Reverse(doc_freq), form, terms));
    }
    slots.sort_unstable_by(|a, b| (a.0, a.1, a.2, &a.3).cmp(&(b.0, b.1, b.2, &b.3)));
    slots.truncate(cap);
    let mut terms: Vec<Vec<u8>> = slots.into_iter().flat_map(|s| s.4).collect();
    terms.sort_unstable();
    Ok((terms, true))
}

/// FtsLib `ExpandWildcardWithFm`, verified: the terms the wildcard walk
/// would match, capped like the walk. `core`/`suffix_only` from
/// [`fm_wildcard_shape`]; `automaton` the walk's (built on mark-free text:
/// with `marked_terms` a candidate is verified on its mark-free bytes, as
/// the walk reads it through `MarkFree`).
pub fn expand_wildcard(
    fm: &FmIndex,
    dict: &TermDictionary,
    automaton: &WildcardAutomaton,
    core: &str,
    suffix_only: bool,
    cap: usize,
    marked_terms: bool,
) -> Result<(Vec<Vec<u8>>, bool)> {
    let mut buf = Vec::new();
    let mut kept = Vec::new();
    for rowid in fm.find_row_ids(core, suffix_only) {
        if !term_of(dict, rowid, &mut buf)? {
            continue;
        }
        let ok = if marked_terms { automaton.accepts(&strip_mark_bytes(&buf)) } else { automaton.accepts(&buf) };
        if ok {
            kept.push((0u32, buf.clone()));
        }
    }
    cap_like_the_walk(dict, kept, cap, marked_terms)
}

/// FtsLib `ExpandFuzzy`, verified: the terms within `max_distance` edits of
/// `word` by the walk's DFA, closest first, capped like the walk.
///
/// The sidecar is searched with the quote-stripped word: dropping quotes from
/// both strings never increases their edit distance, so the sidecar's hits
/// are a superset of the walk's; the DFA then gives each its real distance
/// (on the mark-free form when the DFA was built for a marks-keeping index).
pub fn expand_fuzzy(
    fm: &FmIndex,
    dict: &TermDictionary,
    dfa: &LevenshteinDfa,
    word: &str,
    max_distance: u8,
    cap: usize,
    marked_terms: bool,
) -> Result<(Vec<Vec<u8>>, bool)> {
    let mut buf = Vec::new();
    let mut kept = Vec::new();
    for (rowid, _) in fm.find_fuzzy(&text_form(word), max_distance as usize) {
        if !term_of(dict, rowid, &mut buf)? {
            continue;
        }
        if let Distance::Exact(d) = dfa.distance(&buf) {
            if d <= max_distance {
                kept.push((d as u32, buf.clone()));
            }
        }
    }
    cap_like_the_walk(dict, kept, cap, marked_terms)
}

/// Edit distance over chars (insert, delete, substitute, and a swap of two
/// adjacent chars as one edit: optimal string alignment, FtsLib's
/// `Levenshtein.Distance`), or None when it exceeds `max`.
pub fn levenshtein_within(a: &[char], b: &[char], max: usize) -> Option<usize> {
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev2 = vec![0usize; b.len() + 1]; // row i - 1 (swaps)
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        let mut row_min = cur[0];
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
            if i > 0 && j > 0 && ca == b[j - 1] && a[i - 1] == cb {
                cur[j + 1] = cur[j + 1].min(prev2[j - 1] + 1);
            }
            row_min = row_min.min(cur[j + 1]);
        }
        // still sound with swaps: a swap cell of the next row, prev[j - 1] + 1,
        // is never below cur[j] (<= prev[j - 1] + 1)
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev2, &mut prev);
        std::mem::swap(&mut prev, &mut cur);
    }
    (prev[b.len()] <= max).then_some(prev[b.len()])
}

/// FtsLib `ExpandFuzzy` with the quote transparent: the sidecar's candidates
/// (quote-free forms) are confirmed with the plain Levenshtein distance on
/// the quote-free forms, exactly FtsLib's `Levenshtein.Distance` on its
/// quote-free terms; distance up to 3 as FtsLib. Capped like the walk.
pub fn expand_fuzzy_quote_free(
    fm: &FmIndex,
    dict: &TermDictionary,
    word: &str,
    max_distance: u8,
    cap: usize,
    marked_terms: bool,
) -> Result<(Vec<Vec<u8>>, bool)> {
    let query: Vec<char> = text_form(word).chars().collect();
    let mut buf = Vec::new();
    let mut kept = Vec::new();
    for (rowid, _) in fm.find_fuzzy(&text_form(word), max_distance as usize) {
        if !term_of(dict, rowid, &mut buf)? {
            continue;
        }
        let Ok(term) = std::str::from_utf8(&buf) else { continue };
        let form: Vec<char> = text_form(term).chars().collect();
        if let Some(d) = levenshtein_within(&form, &query, max_distance as usize) {
            kept.push((d as u32, buf.clone()));
        }
    }
    cap_like_the_walk(dict, kept, cap, marked_terms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_is_one_edit() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(levenshtein_within(&c("abcd"), &c("bacd"), 2), Some(1));
        assert_eq!(levenshtein_within(&c("abcd"), &c("abdc"), 2), Some(1));
        assert_eq!(levenshtein_within(&c("abcd"), &c("badc"), 1), None);
        assert_eq!(levenshtein_within(&c("abcd"), &c("badc"), 2), Some(2));
        // optimal string alignment: no edit inside a swapped pair
        assert_eq!(levenshtein_within(&c("ca"), &c("abc"), 3), Some(3));
    }

    #[test]
    fn levenshtein_is_ftslibs() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(levenshtein_within(&c("יצחק"), &c("יציק"), 1), Some(1));
        assert_eq!(levenshtein_within(&c("יצחק"), &c("קחצי"), 2), None); // reversed: two ends substituted, the middle swapped = 3
        assert_eq!(levenshtein_within(&c("שלום"), &c("שלומות"), 3), Some(3));
        assert_eq!(levenshtein_within(&c("אב"), &c("אבגדה"), 2), None);
    }

    #[test]
    fn wildcard_shape_follows_ftslib() {
        assert_eq!(fm_wildcard_shape("*לום", false), Some(("לום".into(), true)));
        assert_eq!(fm_wildcard_shape("*יצח*", false), Some(("יצח".into(), false)));
        assert_eq!(fm_wildcard_shape("שלו*", false), None); // trailing only: the walk
        assert_eq!(fm_wildcard_shape("*ש", false), None); // anchor too short
        assert_eq!(fm_wildcard_shape("*ש?ום", false), None); // optionals: the walk
        assert_eq!(fm_wildcard_shape("*אב*גד*", false), None); // inner star
        assert_eq!(fm_wildcard_shape("*רש\"י", false), None); // quoted core, quotes significant
        assert_eq!(fm_wildcard_shape("*רש\"י", true), Some(("רשי".into(), true)));
    }
}
