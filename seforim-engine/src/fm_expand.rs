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
use hebrew_fts::walk::ZERO_WIDTH_BYTES;
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
        stripped = pattern.chars().filter(|&c| c != '"').collect();
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
        || core.contains('"')
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

/// Keeps the best `cap` of `ranked` under the walk's order: lowest rank,
/// then shortest (quote zero-width), then highest doc count, then bytes.
fn cap_like_the_walk(dict: &TermDictionary, mut ranked: Vec<(u32, Vec<u8>)>, cap: usize) -> Result<Vec<Vec<u8>>> {
    if ranked.len() <= cap {
        ranked.sort_unstable_by(|a, b| a.1.cmp(&b.1));
        return Ok(ranked.into_iter().map(|(_, t)| t).collect());
    }
    let mut keyed: Vec<(u32, usize, Reverse<u32>, Vec<u8>)> = Vec::with_capacity(ranked.len());
    for (rank, term) in ranked {
        let doc_freq = dict.get(&term)?.map(|i| i.doc_freq).unwrap_or(0);
        keyed.push((rank, ZERO_WIDTH_BYTES.term_len(&term), Reverse(doc_freq), term));
    }
    keyed.sort_unstable();
    keyed.truncate(cap);
    let mut terms: Vec<Vec<u8>> = keyed.into_iter().map(|e| e.3).collect();
    terms.sort_unstable();
    Ok(terms)
}

/// FtsLib `ExpandWildcardWithFm`, verified: the terms the wildcard walk
/// would match, capped like the walk. `core`/`suffix_only` from
/// [`fm_wildcard_shape`]; `automaton` the walk's.
pub fn expand_wildcard(
    fm: &FmIndex,
    dict: &TermDictionary,
    automaton: &WildcardAutomaton,
    core: &str,
    suffix_only: bool,
    cap: usize,
) -> Result<Vec<Vec<u8>>> {
    let mut buf = Vec::new();
    let mut kept = Vec::new();
    for rowid in fm.find_row_ids(core, suffix_only) {
        if term_of(dict, rowid, &mut buf)? && automaton.accepts(&buf) {
            kept.push((0u32, buf.clone()));
        }
    }
    cap_like_the_walk(dict, kept, cap)
}

/// FtsLib `ExpandFuzzy`, verified: the terms within `max_distance` edits of
/// `word` by the walk's DFA, closest first, capped like the walk.
///
/// The sidecar is searched with the quote-stripped word: dropping quotes from
/// both strings never increases their edit distance, so the sidecar's hits
/// are a superset of the walk's; the DFA then gives each its real distance.
pub fn expand_fuzzy(
    fm: &FmIndex,
    dict: &TermDictionary,
    dfa: &LevenshteinDfa,
    word: &str,
    max_distance: u8,
    cap: usize,
) -> Result<Vec<Vec<u8>>> {
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
    cap_like_the_walk(dict, kept, cap)
}

#[cfg(test)]
mod tests {
    use super::*;

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
