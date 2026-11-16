//! Hybrid ranking: lexical + semantic results fused into one order. A port
//! of Otzaria's `otzaria-semantic-search` crate (`hybrid/fusion.rs`,
//! `hybrid/ranking.rs`, `hybrid/coordinator.rs::fuse_candidates`,
//! `config/profiles.rs`), see [`fuse_with`]:
//!
//! - Weighted (the "Balanced" profile, Otzaria's default): BM25 normalized
//!   `x / (k + x)` (k = 10), cosine `(x + 1) / 2` (threshold 0), fused
//!   `alpha * lexical + (1 - alpha) * semantic`, a missing side counting 0,
//!   plus 0.10 when both sides found the line; alpha by query type
//!   (Otzaria's table, see [`QueryTypeAlphas`]) unless given.
//! - RRF (the "Fast" profile): `sum 1 / (k + rank)` over both lists (k = 60),
//!   rank from 1; a semantic line under 0.55 normalized is left out.
//!
//! The lexical list is Otzaria's: lines ranked by the BM25 of its exact
//! query (computed by `seforim-engine`, `relevance::hybrid_bm25`).

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

/// How to fuse: the two Otzaria profiles the engine offers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fusion {
    /// The "Balanced" profile (Otzaria's default): weighted, semantic
    /// threshold 0, agreement bonus 0.10. `alpha` = lexical weight; None =
    /// from the query type.
    Weighted { alpha: Option<f32>, bm25_k: f32 },
    /// The "Fast" profile: RRF, semantic threshold 0.55 (a line under it is
    /// left out), no bonus.
    Rrf { k: u32 },
}

impl Default for Fusion {
    fn default() -> Self {
        Fusion::Weighted { alpha: None, bm25_k: DEFAULT_BM25_SATURATION_K }
    }
}

/// The "Balanced" profile's `agreement_bonus`: added to a line both sides
/// found (weighted fusion only).
pub const BALANCED_AGREEMENT_BONUS: f32 = 0.10;
/// The "Balanced" profile's `semantic_threshold`.
pub const BALANCED_SEMANTIC_THRESHOLD: f32 = 0.0;
/// The "Fast" profile's `semantic_threshold`.
pub const FAST_SEMANTIC_THRESHOLD: f32 = 0.55;

/// Otzaria's `normalize_semantic_with_threshold` for one cosine.
pub fn normalize_cosine_with_threshold(x: f32, threshold: f32) -> f32 {
    let n = normalize_cosine(x);
    if n < threshold {
        0.0
    } else {
        n
    }
}

/// Each line once, at its best score and that score's rank (Otzaria's
/// dedup in `fuse_candidates`), in rank order: `(rank, line, score)`.
fn dedup(list: &[(u64, f32)]) -> Vec<(usize, u64, f32)> {
    let mut by_id: HashMap<u64, (usize, f32)> = HashMap::with_capacity(list.len());
    for (rank, &(id, s)) in list.iter().enumerate() {
        match by_id.entry(id) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((rank, s));
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if s.total_cmp(&e.get().1).is_gt() {
                    e.insert((rank, s));
                }
            }
        }
    }
    let mut out: Vec<(usize, u64, f32)> = by_id.into_iter().map(|(id, (rank, s))| (rank, id, s)).collect();
    out.sort_by_key(|e| e.0);
    out
}

/// Otzaria's `fuse_candidates` in hybrid mode with the Balanced (weighted)
/// or Fast (RRF) profile. Both lists ranked best first: the lexical one with
/// raw BM25 scores, the semantic one with cosines.
///
/// - Weighted: lexical `alpha * bm25 / (k + bm25)`; semantic
///   `(1 - alpha) * (cos + 1) / 2` (threshold 0); a line both found adds
///   both plus the agreement bonus 0.10.
/// - RRF: lexical `1 / (k + rank)`, semantic `1 / (k + rank)` (ranks from 1
///   over each deduplicated list); a semantic line under the threshold 0.55
///   is left out (its rank still counts); no bonus.
///
/// Order (Otzaria's `best_first`): fused descending, then the semantic
/// list's order (a line it did not place after every line it did), then
/// line id.
pub fn fuse_with(lexical: &[(u64, f32)], semantic: &[(u64, f32)], fusion: Fusion, alpha: f32) -> Vec<Fused> {
    let (rrf_k, bm25_k, threshold) = match fusion {
        Fusion::Weighted { bm25_k, .. } => (None, bm25_k, BALANCED_SEMANTIC_THRESHOLD),
        Fusion::Rrf { k } => (Some(k.max(1)), DEFAULT_BM25_SATURATION_K, FAST_SEMANTIC_THRESHOLD),
    };
    // each line with its place in the semantic list
    let mut out: Vec<(Fused, Option<usize>)> = Vec::with_capacity(lexical.len() + semantic.len());
    let mut at: HashMap<u64, usize> = HashMap::with_capacity(lexical.len() + semantic.len());
    for (_, id, s) in dedup(lexical) {
        let rank = out.len() as f32 + 1.0;
        let fused = match rrf_k {
            Some(k) => 1.0 / (k as f32 + rank),
            None => alpha * normalize_bm25(s, bm25_k),
        };
        at.insert(id, out.len());
        out.push((Fused { line_id: id, fused, lexical: Some(s), semantic: None, source: Source::Lexical }, None));
    }
    for (semantic_rank, (position, id, s)) in dedup(semantic).into_iter().enumerate() {
        let normalized = normalize_cosine_with_threshold(s, threshold);
        if normalized <= 0.0 && rrf_k.is_some() {
            continue;
        }
        let contribution = match rrf_k {
            Some(k) => 1.0 / (k as f32 + semantic_rank as f32 + 1.0),
            None => (1.0 - alpha) * normalized,
        };
        match at.get(&id) {
            Some(&i) => {
                let (f, place) = &mut out[i];
                f.source = Source::Both;
                f.semantic = Some(s);
                *place = Some(position);
                f.fused += contribution;
                if rrf_k.is_none() {
                    f.fused += BALANCED_AGREEMENT_BONUS;
                }
            }
            None => {
                at.insert(id, out.len());
                out.push((
                    Fused { line_id: id, fused: contribution, lexical: None, semantic: Some(s), source: Source::Semantic },
                    Some(position),
                ));
            }
        }
    }
    out.sort_by(|(a, pa), (b, pb)| {
        b.fused
            .total_cmp(&a.fused)
            .then_with(|| match (pa, pb) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then_with(|| a.line_id.cmp(&b.line_id))
    });
    out.into_iter().map(|(f, _)| f).collect()
}

/// Fuses with `fusion`; `query` decides the weighted alpha when not given.
pub fn fuse(lexical: &[(u64, f32)], semantic: &[(u64, f32)], fusion: Fusion, query: &str, alphas: &QueryTypeAlphas) -> Vec<Fused> {
    let alpha = match fusion {
        Fusion::Weighted { alpha: Some(a), .. } => a,
        _ => alpha_for(&analyze_query(query), alphas),
    };
    fuse_with(lexical, semantic, fusion, alpha.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lexical: &[(u64, f32)], semantic: &[(u64, f32)], fusion: Fusion, query: &str) -> Vec<Fused> {
        fuse(lexical, semantic, fusion, query, &QueryTypeAlphas::default())
    }

    #[test]
    fn weighted_matches_a_hand_computed_example() {
        // lexical BM25 (ranked): line 2 = 30 -> 0.75, line 1 = 10 -> 0.5
        // semantic cosine: line 1 = 0.6 -> 0.8, line 3 = 0.2 -> 0.6
        // alpha 0.5: 1 = 0.25 + 0.4 + 0.10 (agreement) = 0.75, 2 = 0.375, 3 = 0.3
        let w = Fusion::Weighted { alpha: Some(0.5), bm25_k: 10.0 };
        let f = run(&[(2, 30.0), (1, 10.0)], &[(1, 0.6), (3, 0.2)], w, "");
        let got: Vec<(u64, f32, Source)> = f.iter().map(|e| (e.line_id, e.fused, e.source)).collect();
        let want = [(1, 0.75, Source::Both), (2, 0.375, Source::Lexical), (3, 0.3, Source::Semantic)];
        assert_eq!(got.len(), 3);
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g.0, w.0);
            assert!((g.1 - w.1).abs() < 1e-6, "{g:?}");
            assert_eq!(g.2, w.2);
        }
    }

    #[test]
    fn agreement_bonus_lifts_a_line_both_found() {
        // one word: alpha 0.7 (Otzaria's "short")
        // line 1 lexical only: BM25 90 -> 0.9 * 0.7 = 0.63
        // line 2 BM25 10 -> 0.5 * 0.7 = 0.35, cosine 0.8 -> 0.9 * 0.3 = 0.27, + 0.10 = 0.72
        // line 3 semantic only: cosine 0 -> 0.5 * 0.3 = 0.15
        let w = Fusion::Weighted { alpha: None, bm25_k: 10.0 };
        let f = run(&[(1, 90.0), (2, 10.0)], &[(2, 0.8), (3, 0.0)], w, "one");
        assert_eq!(f.iter().map(|e| e.line_id).collect::<Vec<_>>(), vec![2, 1, 3]);
        assert!((f[0].fused - 0.72).abs() < 1e-6);
        assert!((f[1].fused - 0.63).abs() < 1e-6);
        assert!((f[2].fused - 0.15).abs() < 1e-6);
        assert_eq!(f[0].source, Source::Both);
    }

    #[test]
    fn rrf_matches_a_hand_computed_example() {
        // k = 60: lexical [1, 2], semantic [3, 1, 4]; line 4's cosine 0.0
        // normalizes to 0.5 < 0.55: left out. 1 = 1/61 + 1/62, 2 = 1/62, 3 = 1/61
        let f = run(&[(1, 9.0), (2, 5.0)], &[(3, 0.9), (1, 0.8), (4, 0.0)], Fusion::Rrf { k: 60 }, "");
        assert_eq!(f.iter().map(|e| e.line_id).collect::<Vec<_>>(), vec![1, 3, 2]);
        assert!((f[0].fused - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-7);
        assert!((f[1].fused - 1.0 / 61.0).abs() < 1e-7);
        assert!((f[2].fused - 1.0 / 62.0).abs() < 1e-7);
    }

    #[test]
    fn ties_go_by_semantic_order_then_line_id() {
        let f = run(&[(9, 10.0), (4, 10.0)], &[], Fusion::Weighted { alpha: Some(1.0), bm25_k: 10.0 }, "");
        assert_eq!(f.iter().map(|e| e.line_id).collect::<Vec<_>>(), vec![4, 9]);
        // alpha 0: two semantic lines tie at 0.75; the semantic order decides
        let f = run(&[], &[(9, 0.5), (4, 0.5)], Fusion::Weighted { alpha: Some(0.0), bm25_k: 10.0 }, "");
        assert_eq!(f.iter().map(|e| e.line_id).collect::<Vec<_>>(), vec![9, 4]);
    }

    #[test]
    fn a_line_twice_counts_at_its_best() {
        let f = run(&[(1, 5.0), (1, 10.0)], &[], Fusion::Weighted { alpha: Some(1.0), bm25_k: 10.0 }, "");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].lexical, Some(10.0));
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
