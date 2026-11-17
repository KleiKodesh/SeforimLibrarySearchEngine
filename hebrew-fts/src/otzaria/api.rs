//! The public face of the Otzaria helpers: what otzaria_search_engine 7.2.0's
//! `api::search_engine` exposes for them (`sanitize_query`, `split_query_words`,
//! `query_word_spans`, `generate_highlight_pattern`, `generate_literal_highlight_pattern`,
//! `generate_smart_highlight_pattern`, `parse_user_regex`, `prepare_user_regex_matcher`),
//! step for step. The user regex reuses this crate's own port of Otzaria's parser
//! ([`crate::user_regex`], [`crate::line_regex`]).

use std::collections::HashMap;
use std::sync::Arc;

use super::gap_phrase::Proximity;
use super::highlight_matcher::{Match, PreparedHighlightMatcher};
use super::{display_highlight, hebrew_query, smart_query};

/// Otzaria's `sanitize_query`.
pub fn sanitize_query(query: &str) -> String {
    hebrew_query::sanitize_query(query)
}

/// Otzaria's `split_query_words`.
pub fn split_query_words(query: &str) -> Vec<String> {
    hebrew_query::split_query_words(query)
}

/// Otzaria's `query_word_spans`: each word with its source range in UTF-16 units.
pub fn query_word_spans(query: &str) -> Vec<(String, u32, u32)> {
    hebrew_query::query_word_spans(query)
}

/// Otzaria's `generate_literal_highlight_pattern`.
pub fn literal_highlight_pattern(query: &str) -> Option<String> {
    display_highlight::build_literal_pattern(query)
}

/// A UTF-16 range (Otzaria's `HighlightRange`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HighlightRange {
    pub start: u32,
    pub end: u32,
}

/// One match (Otzaria's `HighlightMatch`): word ranges relative to `start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HighlightMatch {
    pub start: u32,
    pub end: u32,
    pub word_ranges: Vec<HighlightRange>,
}

/// Otzaria's `convert_highlight_matches`: a match whose offsets do not fit u32 is dropped.
fn convert(matches: Vec<Match>) -> Vec<HighlightMatch> {
    matches
        .into_iter()
        .filter_map(|m| {
            Some(HighlightMatch {
                start: m.start.try_into().ok()?,
                end: m.end.try_into().ok()?,
                word_ranges: m
                    .word_ranges
                    .into_iter()
                    .map(|r| Some(HighlightRange { start: r.start.try_into().ok()?, end: r.end.try_into().ok()? }))
                    .collect::<Option<Vec<_>>>()?,
            })
        })
        .collect()
}

/// A prepared display matcher (Otzaria's `HighlightMatcher`).
#[derive(Clone)]
pub struct HighlightMatcher(Arc<PreparedHighlightMatcher>);

impl HighlightMatcher {
    /// Whether a display caller may join adjacent lines to complete this pattern.
    pub fn allows_cross_line(&self) -> bool {
        self.0.allows_cross_line()
    }

    pub fn find_matches(&self, data: &str, require_token_boundaries: &[bool]) -> Vec<HighlightMatch> {
        convert(self.0.find_matches(data, require_token_boundaries))
    }

    pub fn find_word_matches(&self, data: &str, require_token_boundaries: &[bool]) -> Vec<HighlightMatch> {
        convert(self.0.find_word_matches(data, require_token_boundaries))
    }
}

/// Otzaria's `HighlightPattern`.
#[derive(Clone)]
pub struct HighlightPattern {
    pub combined_pattern: String,
    pub word_patterns: Vec<String>,
    pub word_boundary_eligible: Vec<bool>,
    pub matcher: HighlightMatcher,
}

/// Otzaria's `generate_highlight_pattern`.
pub fn highlight_pattern(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
    any_order: bool,
) -> Option<HighlightPattern> {
    display_highlight::build_display_highlight(query, distance, custom_spacing, alternative_words, search_options, any_order)
        .map(|hl| HighlightPattern {
            combined_pattern: hl.combined_pattern,
            word_patterns: hl.word_patterns,
            word_boundary_eligible: hl.word_boundary_eligible,
            matcher: HighlightMatcher(Arc::new(hl.matcher)),
        })
}

/// Otzaria's `generate_smart_highlight_pattern`; `exact` = its `SemanticLexicalMode::Exact`.
pub fn smart_highlight_pattern(query: &str, exact: bool) -> Option<HighlightPattern> {
    let exact = exact || smart_query::whole_query_quoted(query);
    let pattern = match smart_query::display_pattern(query, exact, |words| Ok::<_, std::convert::Infallible>(words.to_vec())) {
        Ok(pattern) => pattern,
        Err(never) => match never {},
    };
    pattern.map(|p| HighlightPattern {
        combined_pattern: p.combined_pattern,
        word_patterns: p.word_patterns,
        word_boundary_eligible: p.word_boundary_eligible,
        matcher: HighlightMatcher(p.matcher),
    })
}

/// The book-view matcher of a `/.../` query (Otzaria's `user_regex::DisplayMatcher`):
/// matches in displayed text (markup included), in UTF-16 units.
pub struct UserRegexMatcher(DisplayMatcher);

enum DisplayMatcher {
    Terms(PreparedHighlightMatcher),
    Line(crate::line_regex::LinePlan),
}

impl UserRegexMatcher {
    /// Otzaria's `prepare_user_regex_matcher`: `None` when the query is not a valid `/.../`.
    pub fn prepare(raw: &str) -> Option<Self> {
        match crate::user_regex::parse(raw) {
            Ok(regex) => {
                let branches = regex.words.iter().map(|p| vec![(String::new(), p.clone(), String::new())]).collect();
                let n = regex.words.len();
                PreparedHighlightMatcher::new(branches, Proximity::adjacent(n), vec![true; n]).ok().map(|m| UserRegexMatcher(DisplayMatcher::Terms(m)))
            }
            Err(crate::user_regex::UserRegexError::LineKind) => crate::user_regex::parse_line(raw).ok().map(|p| UserRegexMatcher(DisplayMatcher::Line(p))),
            Err(_) => None,
        }
    }

    /// Otzaria's `UserRegexMatcher::find_matches`.
    pub fn find_matches(&self, data: &str) -> Vec<HighlightMatch> {
        convert(match &self.0 {
            DisplayMatcher::Terms(matcher) => matcher.find_matches(data, &[]),
            DisplayMatcher::Line(plan) => {
                let view = crate::line_regex::LineView::of(data, true);
                plan.find(&view.text)
                    .iter()
                    .map(|range| {
                        let (start, end) = view.source_range(range);
                        let (start, end) = (start as usize, end as usize);
                        Match {
                            start,
                            end,
                            word_ranges: vec![super::highlight_matcher::Range { start: 0, end: end - start }],
                        }
                    })
                    .collect()
            }
        })
    }
}

// ---- index highlight patterns (the `generate_index_*_highlight_pattern` family) ----
//
// otzaria_search_engine reads each word's matched index terms with its own term scans
// and then builds the pattern from them. Here the terms come from the caller (this
// engine's search hands its walks' matched terms back, `SnippetSession::group_terms`);
// the steps after the scan are Otzaria's, verbatim.

fn to_pattern(p: smart_query::DisplayPattern) -> HighlightPattern {
    HighlightPattern {
        combined_pattern: p.combined_pattern,
        word_patterns: p.word_patterns,
        word_boundary_eligible: p.word_boundary_eligible,
        matcher: HighlightMatcher(p.matcher),
    }
}

fn from_display(hl: display_highlight::DisplayHighlight) -> HighlightPattern {
    HighlightPattern {
        combined_pattern: hl.combined_pattern,
        word_patterns: hl.word_patterns,
        word_boundary_eligible: hl.word_boundary_eligible,
        matcher: HighlightMatcher(Arc::new(hl.matcher)),
    }
}

/// otzaria_search_engine `SearchEngine::quoteless_variant`.
fn quoteless_variant(token: &str) -> Option<String> {
    if !token.contains(['"', '\'']) {
        return None;
    }
    let clean: String = token.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
    (!clean.is_empty()).then_some(clean)
}

/// Index terms as Otzaria's plain `text` field holds them (its highlight scans read that
/// field, never the vocalized one): a keep-marks index's terms lose their marks.
fn plain_terms(terms: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(terms.len());
    for t in terms {
        let plain = crate::chars::strip_term_marks(t).into_owned();
        if !out.contains(&plain) {
            out.push(plain);
        }
    }
    out
}

/// A token's display term set (`lexical_phrase_per_word_terms` /
/// `compute_index_fuzzy_highlight_pattern` per token): the matched terms, the token
/// itself, its quote-free form and the caller's dictionary forms.
fn token_terms(token: &str, matched: &[String]) -> std::collections::BTreeSet<String> {
    let mut set: std::collections::BTreeSet<String> = plain_terms(matched).into_iter().collect();
    set.insert(token.to_string());
    set.extend(quoteless_variant(token));
    set
}

/// Otzaria's `generate_index_highlight_pattern` after its term scan
/// (`compute_index_highlight_pattern`): `per_word_terms[i]` = the index terms query
/// word `i` matched (empty: the word paints its query shape), read as the plain `text`
/// field holds them (marks dropped).
pub fn index_highlight_pattern(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
    per_word_terms: &[Vec<String>],
    any_order: bool,
) -> Option<HighlightPattern> {
    let per_word: Vec<Vec<String>> = per_word_terms.iter().map(|t| plain_terms(t)).collect();
    display_highlight::build_display_highlight_from_terms(
        query,
        distance,
        custom_spacing,
        alternative_words,
        search_options,
        &per_word,
        any_order,
    )
    .map(from_display)
}

/// The tokens whose fuzzy terms [`fuzzy_index_highlight_pattern`] reads (the
/// `hebrew_query` analyzer's tokens), or None when they do not align with the query's
/// words (Otzaria then paints the query shape and reads no terms).
pub fn fuzzy_index_highlight_tokens(query: &str) -> Option<Vec<String>> {
    let tokens = smart_query::display_tokens(query);
    let words = hebrew_query::split_query_words(&hebrew_query::normalize_for_index(query));
    (tokens.len() == words.len()).then_some(tokens)
}

/// Otzaria's `generate_index_fuzzy_highlight_pattern` after its term scan:
/// `per_token_terms[i]` = the index terms within the edit distance of token `i` of
/// [`fuzzy_index_highlight_tokens`] (none at distance 0) plus the dictionary forms;
/// the token and its quote-free form are added here.
pub fn fuzzy_index_highlight_pattern(query: &str, per_token_terms: &[Vec<String>]) -> Option<HighlightPattern> {
    let per_word: Vec<Vec<String>> = match fuzzy_index_highlight_tokens(query) {
        None => Vec::new(),
        Some(tokens) => tokens
            .iter()
            .enumerate()
            .map(|(i, t)| token_terms(t, per_token_terms.get(i).map(Vec::as_slice).unwrap_or(&[])).into_iter().collect())
            .collect(),
    };
    display_highlight::build_display_highlight_from_terms(
        query,
        0,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &per_word,
        false,
    )
    .map(from_display)
}

/// The words whose fuzzy terms [`smart_index_highlight_pattern`] reads (every
/// alternative of every word outside a quotation); empty when the query is exact.
pub fn smart_index_highlight_words(query: &str, exact: bool) -> Vec<String> {
    let exact = exact || smart_query::whole_query_quoted(query);
    let mut words: Vec<String> = Vec::new();
    let _ = smart_query::display_pattern(query, exact, |choices| {
        for c in choices {
            if !words.contains(c) {
                words.push(c.clone());
            }
        }
        Ok::<_, std::convert::Infallible>(Vec::new())
    });
    words
}

/// Otzaria's `generate_index_smart_highlight_pattern` after its term scans:
/// `terms[word]` = the index terms within the edit distance of `word` (none at
/// distance 0) plus the dictionary forms, for the words of
/// [`smart_index_highlight_words`]; the word and its quote-free form are added here.
pub fn smart_index_highlight_pattern(query: &str, exact: bool, terms: &HashMap<String, Vec<String>>) -> Option<HighlightPattern> {
    let exact = exact || smart_query::whole_query_quoted(query);
    let pattern = smart_query::display_pattern(query, exact, |choices| {
        let mut set = std::collections::BTreeSet::new();
        for c in choices {
            set.extend(token_terms(c, terms.get(c).map(Vec::as_slice).unwrap_or(&[])));
        }
        Ok::<_, std::convert::Infallible>(set.into_iter().collect())
    });
    match pattern {
        Ok(p) => p.map(to_pattern),
        Err(never) => match never {},
    }
}
