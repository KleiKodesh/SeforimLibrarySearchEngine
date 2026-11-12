//! Hybrid ranking: lexical + semantic results fused into one order. A port
//! of Otzaria's `otzaria-semantic-search` crate (`hybrid/fusion.rs`,
//! `hybrid/ranking.rs`, `config/profiles.rs`):
//!
//! - Weighted: BM25 normalized `x / (k + x)` (k = 10), cosine normalized
//!   `(x + 1) / 2`, fused `alpha * lexical + (1 - alpha) * semantic`, a
//!   missing side counting 0; alpha by query type (Otzaria's table, see
//!   [`QueryTypeAlphas`]) unless given.
//! - RRF: `sum 1 / (k + rank)` over both lists (k = 60), rank from 1.
//!
//! Order: fused score descending, then line id ascending (Otzaria's
//! `sort_by_score_desc`). Unlike Otzaria, the lexical side is every lexical
//! hit (the engine never cuts lexical hits); the semantic side is a ranked
//! top-k list by nature.

use std::cmp::Ordering;
use std::collections::HashMap;

/// Otzaria's default BM25 saturation constant.
pub const DEFAULT_BM25_SATURATION_K: f32 = 10.0;
/// Otzaria's RRF constant.
pub const DEFAULT_RRF_K: u32 = 60;

/// Otzaria's query types (`ranking.rs::QueryType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    ExactReference,
    Conceptual,
    Mixed,
    Short,
    Unknown,
}

/// Otzaria's alpha (lexical weight) per query type (`QueryTypeAlphas`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueryTypeAlphas {
    pub quoted_phrase: f32,
    pub exact_reference: f32,
    pub short: f32,
    pub mixed: f32,
    pub conceptual: f32,
    pub unknown: f32,
}

impl Default for QueryTypeAlphas {
    fn default() -> Self {
        QueryTypeAlphas { quoted_phrase: 1.0, exact_reference: 0.85, short: 0.7, mixed: 0.5, conceptual: 0.3, unknown: 0.5 }
    }
}

/// What the alpha table reads off a query (Otzaria's `analyze_query`, the
/// parts that decide the type).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueryFeatures {
    pub token_count: usize,
    pub has_quoted_phrase: bool,
    pub has_numbers: bool,
    pub query_type: QueryType,
}

/// Otzaria's `analyze_query`: whitespace tokens; a quoted phrase makes it an
/// exact reference; up to 2 tokens: exact reference with a digit, else
/// short; 5 or more: conceptual; else mixed; none: unknown.
pub fn analyze_query(query: &str) -> QueryFeatures {
    let has_quoted_phrase = has_quoted_phrase(query);
    let tokens: Vec<&str> = query.split_whitespace().collect();
    let token_count = tokens.len();
    let has_numbers = tokens.iter().any(|t| t.chars().any(|c| c.is_ascii_digit()));
    let query_type = if token_count == 0 {
        QueryType::Unknown
    } else if has_quoted_phrase {
        QueryType::ExactReference
    } else if token_count <= 2 {
        if has_numbers {
            QueryType::ExactReference
        } else {
            QueryType::Short
        }
    } else if token_count >= 5 {
        QueryType::Conceptual
    } else {
        QueryType::Mixed
    };
    QueryFeatures { token_count, has_quoted_phrase, has_numbers, query_type }
}

/// Otzaria's `quoted_phrases(..)` non-empty: a pair of quote marks (ASCII
/// `"`, U+05F4, U+201C, U+201D, U+201E) enclosing text, where a quote
/// between two Hebrew letters (U+05D0..U+05EA; marks U+0591..U+05BD,
/// U+05BF..U+05C7 skipped when looking back) is an acronym's, not a delimiter.
pub fn has_quoted_phrase(query: &str) -> bool {
    let is_quote = |c: char| matches!(c, '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}' | '\u{201E}');
    if !query.chars().any(is_quote) {
        return false;
    }
    let is_letter = |c: Option<char>| c.is_some_and(|c| ('\u{05D0}'..='\u{05EA}').contains(&c));
    let mut open: Option<usize> = None;
    let mut previous = None;
    let mut chars = query.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let next = chars.peek().map(|&(_, c)| c);
        if is_quote(c) && !(is_letter(previous) && is_letter(next)) {
            match open.take() {
                Some(start) => {
                    if !query[start..at].trim().is_empty() {
                        return true;
                    }
                }
                None => open = Some(at + c.len_utf8()),
            }
        }
        if !matches!(c, '\u{0591}'..='\u{05BD}' | '\u{05BF}'..='\u{05C7}') {
            previous = Some(c);
        }
    }
    false
}

/// Otzaria's `compute_alpha_with`.
pub fn alpha_for(features: &QueryFeatures, alphas: &QueryTypeAlphas) -> f32 {
    match features.query_type {
        QueryType::ExactReference if features.has_quoted_phrase => alphas.quoted_phrase,
        QueryType::ExactReference => alphas.exact_reference,
        QueryType::Short => alphas.short,
        QueryType::Mixed => alphas.mixed,
        QueryType::Conceptual => alphas.conceptual,
        QueryType::Unknown => alphas.unknown,
    }
}

/// Otzaria's `normalize_bm25_scores` for one score.
pub fn normalize_bm25(x: f32, k: f32) -> f32 {
    if x.is_nan() || x <= 0.0 {
        0.0
    } else if x.is_infinite() {
        1.0
    } else {
        (x / (k + x)).clamp(0.0, 1.0)
    }
}

/// Otzaria's `normalize_semantic_scores` for one cosine.
pub fn normalize_cosine(x: f32) -> f32 {
    if x.is_nan() {
        0.0
    } else {
        ((x + 1.0) / 2.0).clamp(0.0, 1.0)
    }
}

/// Which side found a fused line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Lexical,
    Semantic,
    Both,
}

/// One fused result. The raw scores are the inputs (BM25 / cosine).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fused {
    pub line_id: u64,
    pub fused: f32,
    pub lexical: Option<f32>,
    pub semantic: Option<f32>,
    pub source: Source,
}

/// How to fuse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fusion {
    /// `alpha` = lexical weight; None = from the query type.
    Weighted { alpha: Option<f32>, bm25_k: f32 },
    Rrf { k: u32 },
}

impl Default for Fusion {
    fn default() -> Self {
        Fusion::Weighted { alpha: None, bm25_k: DEFAULT_BM25_SATURATION_K }
    }
}

/// Otzaria's `fuse_weighted` over raw scores (normalized here).
pub fn fuse_weighted(lexical: &[(u64, f32)], semantic: &[(u64, f32)], alpha: f32, bm25_k: f32) -> Vec<Fused> {
    let mut map: HashMap<u64, (Option<f32>, Option<f32>)> = HashMap::with_capacity(lexical.len() + semantic.len());
    for &(id, s) in lexical {
        map.entry(id).or_default().0 = Some(s);
    }
    for &(id, s) in semantic {
        map.entry(id).or_default().1 = Some(s);
    }
    let mut out: Vec<Fused> = map
        .into_iter()
        .filter_map(|(id, (l, s))| {
            let source = source_of(l, s)?;
            let fused = alpha * l.map_or(0.0, |x| normalize_bm25(x, bm25_k)) + (1.0 - alpha) * s.map_or(0.0, normalize_cosine);
            Some(Fused { line_id: id, fused, lexical: l, semantic: s, source })
        })
        .collect();
    out.sort_by(by_fused);
    out
}

/// Otzaria's `fuse_rrf`: each list in its given order (best first).
pub fn fuse_rrf(lexical: &[(u64, f32)], semantic: &[(u64, f32)], k: u32) -> Vec<Fused> {
    let mut map: HashMap<u64, (Option<f32>, Option<f32>, f32)> = HashMap::with_capacity(lexical.len() + semantic.len());
    for (i, &(id, s)) in lexical.iter().enumerate() {
        let e = map.entry(id).or_insert((None, None, 0.0));
        e.0 = Some(s);
        e.2 += 1.0 / (k as f32 + (i + 1) as f32);
    }
    for (i, &(id, s)) in semantic.iter().enumerate() {
        let e = map.entry(id).or_insert((None, None, 0.0));
        e.1 = Some(s);
        e.2 += 1.0 / (k as f32 + (i + 1) as f32);
    }
    let mut out: Vec<Fused> = map
        .into_iter()
        .filter_map(|(id, (l, s, fused))| Some(Fused { line_id: id, fused, lexical: l, semantic: s, source: source_of(l, s)? }))
        .collect();
    out.sort_by(by_fused);
    out
}

/// Fuses with `fusion`; `query` decides the weighted alpha when not given.
pub fn fuse(lexical: &[(u64, f32)], semantic: &[(u64, f32)], fusion: Fusion, query: &str, alphas: &QueryTypeAlphas) -> Vec<Fused> {
    match fusion {
        Fusion::Weighted { alpha, bm25_k } => {
            let alpha = alpha.unwrap_or_else(|| alpha_for(&analyze_query(query), alphas)).clamp(0.0, 1.0);
            fuse_weighted(lexical, semantic, alpha, bm25_k)
        }
        Fusion::Rrf { k } => fuse_rrf(lexical, semantic, k),
    }
}

fn source_of(l: Option<f32>, s: Option<f32>) -> Option<Source> {
    match (l, s) {
        (Some(_), Some(_)) => Some(Source::Both),
        (Some(_), None) => Some(Source::Lexical),
        (None, Some(_)) => Some(Source::Semantic),
        (None, None) => None,
    }
}

fn by_fused(a: &Fused, b: &Fused) -> Ordering {
    b.fused.total_cmp(&a.fused).then_with(|| a.line_id.cmp(&b.line_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_matches_a_hand_computed_example() {
        // lexical BM25: line 1 = 10 -> 0.5, line 2 = 30 -> 0.75
        // semantic cosine: line 1 = 0.6 -> 0.8, line 3 = 0.2 -> 0.6
        // alpha 0.5: 1 = 0.65, 2 = 0.375, 3 = 0.3
        let f = fuse_weighted(&[(1, 10.0), (2, 30.0)], &[(1, 0.6), (3, 0.2)], 0.5, 10.0);
        let got: Vec<(u64, f32, Source)> = f.iter().map(|e| (e.line_id, e.fused, e.source)).collect();
        let want = [(1, 0.65, Source::Both), (2, 0.375, Source::Lexical), (3, 0.3, Source::Semantic)];
        assert_eq!(got.len(), 3);
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g.0, w.0);
            assert!((g.1 - w.1).abs() < 1e-6, "{g:?}");
            assert_eq!(g.2, w.2);
        }
    }

    #[test]
    fn rrf_matches_a_hand_computed_example() {
        // k = 60: lexical [1, 2], semantic [3, 1]
        // 1 = 1/61 + 1/62, 2 = 1/62, 3 = 1/61
        let f = fuse_rrf(&[(1, 9.0), (2, 5.0)], &[(3, 0.9), (1, 0.8)], 60);
        assert_eq!(f.iter().map(|e| e.line_id).collect::<Vec<_>>(), vec![1, 3, 2]);
        assert!((f[0].fused - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-7);
        assert!((f[1].fused - 1.0 / 61.0).abs() < 1e-7);
    }

    #[test]
    fn ties_go_by_line_id() {
        let f = fuse_weighted(&[(9, 10.0), (4, 10.0)], &[], 1.0, 10.0);
        assert_eq!(f.iter().map(|e| e.line_id).collect::<Vec<_>>(), vec![4, 9]);
    }

    #[test]
    fn normalizers_follow_otzaria() {
        assert_eq!(normalize_bm25(0.0, 10.0), 0.0);
        assert_eq!(normalize_bm25(10.0, 10.0), 0.5);
        assert_eq!(normalize_bm25(90.0, 10.0), 0.9);
        assert_eq!(normalize_cosine(-1.0), 0.0);
        assert_eq!(normalize_cosine(1.0), 1.0);
    }

    #[test]
    fn query_types_follow_otzaria() {
        let a = QueryTypeAlphas::default();
        let t = |q: &str| alpha_for(&analyze_query(q), &a);
        assert_eq!(t(""), 0.5);
        assert_eq!(t("one"), 0.7);
        assert_eq!(t("page 12"), 0.85);
        assert_eq!(t("a b c"), 0.5);
        assert_eq!(t("a b c d e"), 0.3);
        assert_eq!(t("say \"a b\" here"), 1.0);
        // an acronym's gershayim between Hebrew letters is not a phrase
        assert!(!has_quoted_phrase("\u{05E8}\u{05DE}\u{05D1}\"\u{05DD} x"));
        assert!(has_quoted_phrase("\u{201E}x y\u{201C}"));
    }
}
