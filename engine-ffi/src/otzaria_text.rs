//! C ABI of Otzaria's text and query helpers (`hebrew_fts::otzaria`, ported from
//! otzaria_search_engine 7.2.0): the pure text calls, the display matchers (highlight
//! pattern, smart pattern, user regex) and the library book `textHash`.

use std::collections::HashMap;
use std::ffi::c_char;
use std::ptr;

use hebrew_fts::otzaria::{self, BookNoteRow, HighlightMatch, HighlightPattern, UserRegexMatcher};
use hebrew_fts::user_regex::{self, UserRegexKind};
use serde_json::{json, Value};

use crate::{err, guard, out_string, str_arg};

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum TextRequest {
    SanitizeQuery { query: String },
    SplitQueryWords { query: String },
    QueryWordSpans { query: String },
    LiteralHighlightPattern { query: String },
    UserRegexPlan { raw: String },
}

/// Otzaria's pure text helpers. `request` is JSON `{"op": ..., ...}`:
/// - `sanitize_query` `{query}` -> string (`sanitizeQuery`)
/// - `split_query_words` `{query}` -> [word] (`splitQueryWords`)
/// - `query_word_spans` `{query}` -> [[word, start, end]] in UTF-16 units (`queryWordSpans`)
/// - `literal_highlight_pattern` `{query}` -> string | null (`generateLiteralHighlightPattern`)
/// - `user_regex_plan` `{raw}` -> {kind: invalid|single_word|word_sequence|cross_space,
///   pattern, words, reason} (`parseUserRegex`)
///
/// Returns a JSON string (free with `se_string_free`), null on error.
///
/// # Safety
/// `request` must be valid for `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn se_text_helper(request: *const u8, len: usize) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let request: TextRequest = serde_json::from_str(str_arg(request, len)?).map_err(err)?;
        let out = match request {
            TextRequest::SanitizeQuery { query } => json!(otzaria::sanitize_query(&query)),
            TextRequest::SplitQueryWords { query } => json!(otzaria::split_query_words(&query)),
            TextRequest::QueryWordSpans { query } => json!(otzaria::query_word_spans(&query)
                .into_iter()
                .map(|(w, s, e)| json!([w, s, e]))
                .collect::<Vec<_>>()),
            TextRequest::LiteralHighlightPattern { query } => json!(otzaria::literal_highlight_pattern(&query)),
            TextRequest::UserRegexPlan { raw } => {
                let plan = user_regex::plan(&raw);
                let kind = match plan.kind {
                    UserRegexKind::Invalid => "invalid",
                    UserRegexKind::SingleWord => "single_word",
                    UserRegexKind::WordSequence => "word_sequence",
                    UserRegexKind::CrossSpace => "cross_space",
                };
                json!({"kind": kind, "pattern": plan.pattern, "words": plan.words, "reason": plan.reason})
            }
        };
        Ok(out_string(out.to_string()))
    })
}

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum MatcherRequest {
    /// `generateHighlightPattern`.
    Highlight {
        query: String,
        #[serde(default)]
        distance: u32,
        #[serde(default)]
        custom_spacing: HashMap<String, String>,
        #[serde(default)]
        alternative_words: HashMap<String, Vec<String>>,
        #[serde(default)]
        search_options: HashMap<String, HashMap<String, bool>>,
        #[serde(default)]
        any_order: bool,
    },
    /// `generateSmartHighlightPattern`; `exact` = `SemanticLexicalMode.exact`.
    Smart { query: String, #[serde(default)] exact: bool },
    /// `prepareUserRegexMatcher`.
    UserRegex { raw: String },
}

/// A prepared display matcher (`se_matcher_open`).
pub enum DisplayMatcher {
    Pattern(HighlightPattern),
    UserRegex(UserRegexMatcher),
}

fn matches_json(ms: Vec<HighlightMatch>) -> Value {
    Value::Array(
        ms.into_iter()
            .map(|m| json!([m.start, m.end, m.word_ranges.iter().map(|r| json!([r.start, r.end])).collect::<Vec<_>>()]))
            .collect(),
    )
}

/// Prepares a display matcher. `request` is JSON with `kind`:
/// - `highlight` `{query, distance, custom_spacing, alternative_words, search_options, any_order}`
/// - `smart` `{query, exact}`
/// - `user_regex` `{raw}`
///
/// `*out_info` gets a JSON string (free with `se_string_free`): for a pattern
/// `{combined_pattern, word_patterns, word_boundary_eligible, allows_cross_line}`, for a
/// user regex `{}`, and `null` when there is nothing to match (Otzaria's `None`; the
/// handle is then null too). On error: null handle, `*out_info` untouched, `se_last_error`.
/// Free the handle with `se_matcher_free`.
///
/// # Safety
/// `request` must be valid for `len` bytes; `out_info` a valid pointer.
#[no_mangle]
pub unsafe extern "C" fn se_matcher_open(request: *const u8, len: usize, out_info: *mut *mut c_char) -> *mut DisplayMatcher {
    guard(ptr::null_mut(), || {
        if out_info.is_null() {
            return Err("null out_info".into());
        }
        let request: MatcherRequest = serde_json::from_str(str_arg(request, len)?).map_err(err)?;
        let matcher = match request {
            MatcherRequest::Highlight { query, distance, custom_spacing, alternative_words, search_options, any_order } => {
                // JSON object keys are strings (a tagged enum cannot read them as numbers)
                let alternative_words = alternative_words
                    .into_iter()
                    .map(|(k, v)| k.parse::<u32>().map(|k| (k, v)).map_err(|_| format!("alternative_words key {k:?} is not a word index")))
                    .collect::<Result<HashMap<u32, Vec<String>>, String>>()?;
                otzaria::highlight_pattern(&query, distance, &custom_spacing, &alternative_words, &search_options, any_order)
                    .map(DisplayMatcher::Pattern)
            }
            MatcherRequest::Smart { query, exact } => otzaria::smart_highlight_pattern(&query, exact).map(DisplayMatcher::Pattern),
            MatcherRequest::UserRegex { raw } => UserRegexMatcher::prepare(&raw).map(DisplayMatcher::UserRegex),
        };
        let info = match &matcher {
            None => Value::Null,
            Some(DisplayMatcher::Pattern(p)) => json!({
                "combined_pattern": p.combined_pattern,
                "word_patterns": p.word_patterns,
                "word_boundary_eligible": p.word_boundary_eligible,
                "allows_cross_line": p.matcher.allows_cross_line(),
            }),
            Some(DisplayMatcher::UserRegex(_)) => json!({}),
        };
        *out_info = out_string(info.to_string());
        Ok(matcher.map_or(ptr::null_mut(), |m| Box::into_raw(Box::new(m))))
    })
}

/// The matches of `data` (displayed text, markup included): JSON
/// `[[start, end, [[word_start, word_end], ...]], ...]` in UTF-16 units, word ranges
/// relative to `start`. `word_matches` = 1 runs `findWordMatches` (each word on its own),
/// 0 `findMatches`; `require_token_boundaries` (`n_bounds` bytes, 0 / 1 per word) is the
/// app's list (ignored by a user regex). Free with `se_string_free`; null on error.
///
/// # Safety
/// `matcher` from `se_matcher_open`; `data` valid for `len` bytes, `bounds` for `n_bounds`.
#[no_mangle]
pub unsafe extern "C" fn se_matcher_find(
    matcher: *const DisplayMatcher,
    data: *const u8,
    len: usize,
    word_matches: i32,
    bounds: *const u8,
    n_bounds: usize,
) -> *mut c_char {
    guard(ptr::null_mut(), || {
        let matcher = matcher.as_ref().ok_or("null matcher")?;
        let data = str_arg(data, len)?;
        let bounds: Vec<bool> = if bounds.is_null() || n_bounds == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(bounds, n_bounds).iter().map(|&b| b != 0).collect()
        };
        let found = match matcher {
            DisplayMatcher::Pattern(p) if word_matches != 0 => p.matcher.find_word_matches(data, &bounds),
            DisplayMatcher::Pattern(p) => p.matcher.find_matches(data, &bounds),
            DisplayMatcher::UserRegex(m) => m.find_matches(data),
        };
        Ok(out_string(matches_json(found).to_string()))
    })
}

/// Frees a matcher from `se_matcher_open`.
///
/// # Safety
/// `matcher` from `se_matcher_open`, freed once.
#[no_mangle]
pub unsafe extern "C" fn se_matcher_free(matcher: *mut DisplayMatcher) {
    if !matcher.is_null() {
        drop(Box::from_raw(matcher));
    }
}

#[derive(serde::Deserialize)]
struct NoteArg(i64, i64, bool, String);

/// Otzaria's `libraryBookContentFingerprint` after the read (`textHash`, byte-identical to
/// otzaria_search_engine 7.2.0): `rows` is the book's stored rows in `lineIndex, id` order
/// concatenated (`row_lens[i]` bytes each; zstd frames decoded, a TEXT value's own bytes,
/// NULL empty), `notes` JSON `[[lineIndex, charPos (-1 none), hasMarker, body], ...]` in
/// `lineIndex, noteIndex` order (library DB with `line_note` only; NULL bodies left out).
/// Returns 1 with `*out` set, 0 when the book has no text (no rows or empty: Otzaria's
/// `None`), -1 on error.
///
/// # Safety
/// `rows` valid for the sum of `row_lens` (`n_rows` entries), `notes` for `notes_len`.
#[no_mangle]
pub unsafe extern "C" fn se_book_content_fingerprint(
    rows: *const u8,
    row_lens: *const u64,
    n_rows: usize,
    notes: *const u8,
    notes_len: usize,
    out: *mut u64,
) -> i32 {
    guard(-1, || {
        if out.is_null() {
            return Err("null out".into());
        }
        let lens: &[u64] = if n_rows == 0 { &[] } else { std::slice::from_raw_parts(row_lens, n_rows) };
        let total: u64 = lens.iter().sum();
        let data: &[u8] = if total == 0 { &[] } else { std::slice::from_raw_parts(rows, total as usize) };
        let notes_text = str_arg(notes, notes_len)?;
        let notes: Vec<NoteArg> = if notes_text.is_empty() { Vec::new() } else { serde_json::from_str(notes_text).map_err(err)? };
        let notes: Vec<BookNoteRow> = notes
            .into_iter()
            .map(|NoteArg(line_index, char_pos, has_marker, body)| BookNoteRow { line_index, char_pos, has_marker, body })
            .collect();
        let mut at = 0usize;
        let slices = lens.iter().map(|&n| {
            let s = &data[at..at + n as usize];
            at += n as usize;
            s
        });
        Ok(match otzaria::book_content_fingerprint(slices, &notes) {
            Some(hash) => {
                *out = hash;
                1
            }
            None => 0,
        })
    })
}
