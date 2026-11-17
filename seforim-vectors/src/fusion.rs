//! Hybrid ranking: lexical + semantic candidates fused into one order. A
//! port of Otzaria's `otzaria-semantic-search` (rev d2a782a, pinned by
//! otzaria_search_engine 5babbce): `config/profiles.rs` ([`RankingProfile`]),
//! `hybrid/ranking.rs` ([`analyze_query`], bonuses), `hybrid/fusion.rs`
//! (normalizers, `best_first`), `hybrid/hebrew_normalizer.rs`
//! ([`normalize_query`]) and `hybrid/coordinator.rs` (`fuse_candidates`:
//! [`fuse_candidates`]; `semantic_top_k`: [`semantic_top_k`]).

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Otzaria's default BM25 saturation constant.
pub const DEFAULT_BM25_SATURATION_K: f32 = 10.0;
/// coordinator.rs `MAX_SEMANTIC_CANDIDATES`.
pub const MAX_SEMANTIC_CANDIDATES: usize = 10_000;
/// fusion.rs `SEMANTIC_RELEVANCE_THRESHOLD`: the threshold a non-finite one falls back to.
pub const SEMANTIC_RELEVANCE_THRESHOLD: f32 = 0.55;

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
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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

/// How the two sides' scores combine (profiles.rs `FusionStrategy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "lowercase")]
pub enum FusionStrategy {
    /// Weighted by alpha, BM25 `x / (k + x)`.
    Weighted,
    /// `1 / (k + rank)` from each side.
    Rrf { k: u32 },
    /// Weighted, BM25 min-max normalized when its scores run high.
    Adaptive,
}

/// Otzaria's ranking profile (profiles.rs `RankingProfile`, the fields that
/// rank; JSON at the C ABI). [`Default`] = otzaria_search_engine's
/// `SemanticRankingOptions::defaults()` (the app's default `SemanticRankingConfig`):
/// RRF k 60, threshold 0.55, window multiplier 2, foundational bonus 0.002
/// and share 0.5.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RankingProfile {
    pub fusion_strategy: FusionStrategy,
    pub alpha_override: Option<f32>,
    pub alpha_by_query_type: QueryTypeAlphas,
    pub bm25_saturation_k: f32,
    pub semantic_threshold: f32,
    pub agreement_bonus: f32,
    pub phrase_match_bonus: f32,
    pub rare_term_bonus: f32,
    pub section_coverage_bonus: f32,
    pub duplicate_penalty: f32,
    pub metadata_ranking_enabled: bool,
    pub candidate_window_multiplier: f32,
    pub foundational_bonus: f32,
    pub foundational_candidate_share: f32,
    pub window_penalty: f32,
    pub alias_penalty: f32,
}

impl Default for RankingProfile {
    fn default() -> Self {
        RankingProfile {
            fusion_strategy: FusionStrategy::Rrf { k: 60 },
            alpha_override: None,
            alpha_by_query_type: QueryTypeAlphas::default(),
            bm25_saturation_k: 10.0,
            semantic_threshold: 0.55,
            agreement_bonus: 0.1,
            phrase_match_bonus: 0.0,
            rare_term_bonus: 0.0,
            section_coverage_bonus: 0.0,
            duplicate_penalty: 0.0,
            metadata_ranking_enabled: false,
            candidate_window_multiplier: 2.0,
            foundational_bonus: 0.002,
            foundational_candidate_share: 0.5,
            window_penalty: 0.04,
            alias_penalty: 0.04,
        }
    }
}

impl RankingProfile {
    /// profiles.rs `SearchProfile::Fast`.
    pub fn fast() -> Self {
        RankingProfile {
            fusion_strategy: FusionStrategy::Rrf { k: 60 },
            semantic_threshold: 0.55,
            agreement_bonus: 0.05,
            candidate_window_multiplier: 1.5,
            foundational_bonus: 0.0,
            foundational_candidate_share: 0.0,
            ..RankingProfile::default()
        }
    }

    /// profiles.rs `SearchProfile::Balanced`.
    pub fn balanced() -> Self {
        RankingProfile {
            fusion_strategy: FusionStrategy::Weighted,
            semantic_threshold: 0.0,
            agreement_bonus: 0.10,
            candidate_window_multiplier: 2.0,
            foundational_bonus: 0.0,
            foundational_candidate_share: 0.0,
            ..RankingProfile::default()
        }
    }

    /// profiles.rs `SearchProfile::Best`.
    pub fn best() -> Self {
        RankingProfile {
            fusion_strategy: FusionStrategy::Adaptive,
            semantic_threshold: 0.55,
            agreement_bonus: 0.12,
            phrase_match_bonus: 0.10,
            rare_term_bonus: 0.05,
            section_coverage_bonus: 0.03,
            duplicate_penalty: 0.08,
            metadata_ranking_enabled: true,
            candidate_window_multiplier: 3.0,
            foundational_bonus: 0.0,
            foundational_candidate_share: 0.0,
            ..RankingProfile::default()
        }
    }

    /// profiles.rs `RankingProfile::validate`: refuses a parameter the ranking is not defined for.
    pub fn validate(&self) -> Result<(), String> {
        let unit = |name: &str, v: f32| if v.is_finite() && (0.0..=1.0).contains(&v) { Ok(()) } else { Err(format!("{name} must be in 0..=1")) };
        if let Some(a) = self.alpha_override {
            unit("alpha_override", a)?;
        }
        let t = &self.alpha_by_query_type;
        for (n, v) in [("quoted_phrase", t.quoted_phrase), ("exact_reference", t.exact_reference), ("short", t.short), ("mixed", t.mixed), ("conceptual", t.conceptual), ("unknown", t.unknown)] {
            unit(n, v)?;
        }
        if !(self.bm25_saturation_k.is_finite() && self.bm25_saturation_k > 0.0) {
            return Err("bm25_saturation_k must be positive".into());
        }
        for (n, v) in [
            ("semantic_threshold", self.semantic_threshold),
            ("agreement_bonus", self.agreement_bonus),
            ("phrase_match_bonus", self.phrase_match_bonus),
            ("rare_term_bonus", self.rare_term_bonus),
            ("section_coverage_bonus", self.section_coverage_bonus),
            ("duplicate_penalty", self.duplicate_penalty),
            ("foundational_bonus", self.foundational_bonus),
            ("foundational_candidate_share", self.foundational_candidate_share),
        ] {
            unit(n, v)?;
        }
        if !(self.candidate_window_multiplier.is_finite() && (1.0..=10.0).contains(&self.candidate_window_multiplier)) {
            return Err("candidate_window_multiplier must be in 1..=10".into());
        }
        for (n, v) in [("window_penalty", self.window_penalty), ("alias_penalty", self.alias_penalty)] {
            if !(v.is_finite() && (0.0..=0.2).contains(&v)) {
                return Err(format!("{n} must be in 0..=0.2"));
            }
        }
        if let FusionStrategy::Rrf { k: 0 } = self.fusion_strategy {
            return Err("rrf k must be at least 1".into());
        }
        Ok(())
    }
}

/// coordinator.rs `semantic_top_k`: how many semantic candidates a page of
/// `limit` (offset `offset`) fetches.
pub fn semantic_top_k(limit: usize, offset: usize, profile: &RankingProfile) -> usize {
    let multiplier = if profile.candidate_window_multiplier.is_finite() { profile.candidate_window_multiplier.clamp(1.0, 10.0) } else { 2.0 };
    let window = ((limit as f64) * multiplier as f64).ceil().min(usize::MAX as f64) as usize;
    let requested = offset.saturating_add(window).max(1);
    requested.min(MAX_SEMANTIC_CANDIDATES)
}

/// hebrew_normalizer.rs `normalize` (`normalize_for_embedding`): nikud and
/// te'amim stripped, U+05F3 / U+05F4 as ASCII quotes, whitespace collapsed.
pub fn normalize_query(query: &str) -> String {
    let kept: String = query
        .chars()
        .filter(|&c| {
            !(('\u{0591}'..='\u{05BD}').contains(&c) || c == '\u{05BF}' || ('\u{05C1}'..='\u{05C2}').contains(&c) || ('\u{05C4}'..='\u{05C5}').contains(&c) || c == '\u{05C7}')
        })
        .filter(|&c| !('\u{0591}'..='\u{05AF}').contains(&c))
        .map(|c| match c {
            '\u{05F3}' => '\'',
            '\u{05F4}' => '"',
            c => c,
        })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What ranking reads off a query (ranking.rs `QueryFeatures`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryFeatures {
    pub token_count: usize,
    pub has_quoted_phrase: bool,
    pub has_numbers: bool,
    pub query_type: QueryType,
    /// Tokens of more than 5 chars.
    pub rare_tokens: Vec<String>,
    pub quoted_phrases: Vec<String>,
}

/// ranking.rs `analyze_query`.
pub fn analyze_query(query: &str) -> QueryFeatures {
    let quoted_phrases = quoted_phrases(query);
    let has_quoted_phrase = !quoted_phrases.is_empty();
    let tokens: Vec<&str> = query.split_whitespace().collect();
    let token_count = tokens.len();
    let has_numbers = tokens.iter().any(|t| t.chars().any(|c| c.is_ascii_digit()));
    let rare_tokens = tokens.iter().filter(|t| t.chars().count() > 5).map(|t| t.to_string()).collect();
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
    QueryFeatures { token_count, has_quoted_phrase, has_numbers, query_type, rare_tokens, quoted_phrases }
}

/// ranking.rs `quoted_phrases`: the phrases `query` quotes, trimmed, in order
/// (U+05F4, U+201C..U+201E count as `"`; a quote between two Hebrew letters,
/// marks skipped looking back, is an acronym's and part of the text).
pub fn quoted_phrases(query: &str) -> Vec<String> {
    let mut phrases = Vec::new();
    if !query.contains(['"', '\u{05F4}', '\u{201C}', '\u{201D}', '\u{201E}']) {
        return phrases;
    }
    let is_quote = |c: char| matches!(c, '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}' | '\u{201E}');
    let is_letter = |c: Option<char>| c.is_some_and(|c| ('\u{05D0}'..='\u{05EA}').contains(&c));
    let mut open: Option<usize> = None;
    let mut previous = None;
    let mut chars = query.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let next = chars.peek().map(|&(_, c)| c);
        if is_quote(c) && !(is_letter(previous) && is_letter(next)) {
            match open.take() {
                Some(start) => {
                    let phrase = query[start..at].trim();
                    if !phrase.is_empty() {
                        phrases.push(phrase.to_string());
                    }
                }
                None => open = Some(at + c.len_utf8()),
            }
        }
        if !matches!(c, '\u{0591}'..='\u{05BD}' | '\u{05BF}'..='\u{05C7}') {
            previous = Some(c);
        }
    }
    phrases
}

/// Whether a query quotes a phrase.
pub fn has_quoted_phrase(query: &str) -> bool {
    !quoted_phrases(query).is_empty()
}

/// ranking.rs `compute_alpha_with`.
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

/// coordinator.rs `setup`'s requested alpha: the override, else the table, clamped.
pub fn requested_alpha(features: &QueryFeatures, profile: &RankingProfile) -> f32 {
    profile.alpha_override.unwrap_or_else(|| alpha_for(features, &profile.alpha_by_query_type)).clamp(0.0, 1.0)
}

/// fusion.rs `normalize_bm25_scores` for one score.
pub fn normalize_bm25(x: f32, k: f32) -> f32 {
    if x.is_nan() || x <= 0.0 {
        0.0
    } else if x.is_infinite() {
        1.0
    } else {
        (x / (k + x)).clamp(0.0, 1.0)
    }
}

/// fusion.rs `normalize_bm25_adaptive`: min-max when the best score is over
/// `2k` and above the least, else the saturating curve.
pub fn normalize_bm25_adaptive(scores: &[f32], k: f32) -> Vec<f32> {
    let (mut max, mut min) = (f32::NEG_INFINITY, f32::INFINITY);
    for &s in scores {
        if !s.is_nan() && s > 0.0 {
            max = max.max(s);
            min = min.min(s);
        }
    }
    if max > 2.0 * k && max > min {
        scores.iter().map(|&x| if x.is_nan() || x <= 0.0 { 0.0 } else { ((x - min) / (max - min)).clamp(0.0, 1.0) }).collect()
    } else {
        scores.iter().map(|&x| normalize_bm25(x, k)).collect()
    }
}

/// fusion.rs `normalize_semantic_scores` for one cosine.
pub fn normalize_cosine(x: f32) -> f32 {
    if x.is_nan() {
        0.0
    } else {
        ((x + 1.0) / 2.0).clamp(0.0, 1.0)
    }
}

/// fusion.rs `normalize_semantic_with_threshold` for one cosine.
pub fn normalize_cosine_with_threshold(x: f32, threshold: f32) -> f32 {
    let n = normalize_cosine(x);
    if n < threshold {
        0.0
    } else {
        n
    }
}

/// ranking.rs `compute_phrase_match_bonus`.
pub fn phrase_match_bonus(text: &str, phrases: &[String]) -> f32 {
    if phrases.is_empty() {
        return 0.0;
    }
    phrases.iter().filter(|p| text.contains(p.as_str())).count() as f32 / phrases.len() as f32
}

/// ranking.rs `compute_rare_term_bonus`.
pub fn rare_term_bonus(text: &str, rare: &[String]) -> f32 {
    if rare.is_empty() {
        return 0.0;
    }
    rare.iter().filter(|t| text.contains(t.as_str())).count() as f32 / rare.len() as f32
}

/// Which side found a fused line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Lexical,
    Semantic,
    Both,
}

/// The mode that actually ran (coordinator.rs `fuse`'s `mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Hybrid,
    LexicalOnly,
    SemanticOnly,
}

/// One lexical candidate (Otzaria's `LexicalCandidate`, what ranking reads).
#[derive(Debug, Clone, PartialEq)]
pub struct Lexical {
    pub line_id: u64,
    pub book_id: u64,
    pub bm25: f32,
    pub section_id: u64,
    /// 0: unknown (no duplicate penalty).
    pub line_hash: u64,
    /// The line's plain text (phrase / rare term bonuses).
    pub text: String,
    pub foundational: bool,
}

/// One semantic candidate (Otzaria's `SemanticCandidate`).
#[derive(Debug, Clone, PartialEq)]
pub struct Semantic {
    pub line_id: u64,
    pub book_id: u64,
    pub score: f32,
    pub section_id: u64,
    pub line_hash: u64,
    pub foundational: bool,
}

/// One fused result (Otzaria's `FusedCandidate`, what ranking reads).
#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub line_id: u64,
    pub book_id: u64,
    pub fused: f32,
    pub lexical: Option<f32>,
    pub semantic: Option<f32>,
    pub source: Source,
    pub foundational: bool,
    pub semantic_position: Option<u32>,
    section_id: u64,
    line_hash: u64,
    text: String,
}

/// fusion.rs `best_first`: fused descending, foundational first, the
/// semantic order (a line it did not place after every line it did), line
/// id (the book: Otzaria's file path tie-break, a line id here is unique).
fn best_first(a: &Fused, b: &Fused) -> Ordering {
    b.fused
        .total_cmp(&a.fused)
        .then_with(|| b.foundational.cmp(&a.foundational))
        .then_with(|| match (a.semantic_position, b.semantic_position) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        })
        .then_with(|| a.line_id.cmp(&b.line_id))
        .then_with(|| a.book_id.cmp(&b.book_id))
}

/// coordinator.rs `fuse_candidates`, step for step (lines merged by line id;
/// Otzaria merges by (file path, line id): a line id is unique here). The
/// metadata ranker's bonus is 0: its primary-source keywords never match a
/// library book's path (`id:<n>`) and its era / category signals read query
/// facets, which a book-id filter does not have.
pub fn fuse_candidates(lexical: Vec<Lexical>, semantic: Vec<Semantic>, alpha: f32, mode: Mode, profile: &RankingProfile, features: &QueryFeatures) -> Vec<Fused> {
    // each line once, at its best score and that score's rank
    let mut lex_by: HashMap<u64, (usize, Lexical)> = HashMap::new();
    for (rank, c) in lexical.into_iter().enumerate() {
        match lex_by.entry(c.line_id) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((rank, c));
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if c.bm25.total_cmp(&e.get().1.bm25).is_gt() {
                    e.insert((rank, c));
                }
            }
        }
    }
    let mut lexical: Vec<(usize, Lexical)> = lex_by.into_values().collect();
    lexical.sort_by_key(|(r, _)| *r);
    let mut sem_by: HashMap<u64, (usize, Semantic)> = HashMap::new();
    for (rank, c) in semantic.into_iter().enumerate() {
        match sem_by.entry(c.line_id) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((rank, c));
            }
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if c.score.total_cmp(&e.get().1.score).is_gt() {
                    e.insert((rank, c));
                }
            }
        }
    }
    let mut semantic: Vec<(usize, Semantic)> = sem_by.into_values().collect();
    semantic.sort_by_key(|(r, _)| *r);

    let bm25: Vec<f32> = lexical.iter().map(|(_, c)| c.bm25).collect();
    let norm_bm25: Vec<f32> = match profile.fusion_strategy {
        FusionStrategy::Adaptive => normalize_bm25_adaptive(&bm25, profile.bm25_saturation_k),
        _ => bm25.iter().map(|&x| normalize_bm25(x, profile.bm25_saturation_k)).collect(),
    };
    let threshold = if profile.semantic_threshold.is_finite() { profile.semantic_threshold.clamp(0.0, 1.0) } else { SEMANTIC_RELEVANCE_THRESHOLD };
    let rrf_k = match profile.fusion_strategy {
        FusionStrategy::Rrf { k } => Some(k.max(1)),
        _ => None,
    };
    let foundational_on = profile.foundational_bonus > 0.0;

    let mut out: Vec<Fused> = Vec::with_capacity(lexical.len() + semantic.len());
    let mut at: HashMap<u64, usize> = HashMap::with_capacity(lexical.len() + semantic.len());
    for ((_, c), &normalized) in lexical.into_iter().zip(&norm_bm25) {
        let lexical_rank = out.len() as u32 + 1;
        let fused = rrf_k.map(|k| 1.0 / (k as f32 + lexical_rank as f32)).unwrap_or(alpha * normalized);
        at.insert(c.line_id, out.len());
        out.push(Fused {
            line_id: c.line_id,
            book_id: c.book_id,
            fused,
            lexical: Some(c.bm25),
            semantic: None,
            source: Source::Lexical,
            foundational: foundational_on && c.foundational,
            semantic_position: None,
            section_id: c.section_id,
            line_hash: c.line_hash,
            text: c.text,
        });
    }
    for (semantic_rank, (position, c)) in semantic.into_iter().enumerate() {
        let position = u32::try_from(position).unwrap_or(u32::MAX);
        let normalized = normalize_cosine_with_threshold(c.score, threshold);
        if normalized <= 0.0 && rrf_k.is_some() && mode == Mode::Hybrid {
            continue;
        }
        let contribution = rrf_k.map(|k| 1.0 / (k as f32 + semantic_rank as f32 + 1.0)).unwrap_or((1.0 - alpha) * normalized);
        let metadata_bonus = 0.0;
        match at.get(&c.line_id) {
            Some(&i) => {
                let e = &mut out[i];
                e.source = Source::Both;
                e.semantic = Some(c.score);
                e.semantic_position = Some(position);
                e.fused += contribution + metadata_bonus;
                if mode == Mode::Hybrid && rrf_k.is_none() {
                    e.fused += profile.agreement_bonus.max(0.0);
                }
                if foundational_on && !e.foundational {
                    e.foundational = c.foundational;
                }
            }
            None => {
                at.insert(c.line_id, out.len());
                out.push(Fused {
                    line_id: c.line_id,
                    book_id: c.book_id,
                    fused: contribution + metadata_bonus,
                    lexical: None,
                    semantic: Some(c.score),
                    source: Source::Semantic,
                    foundational: foundational_on && c.foundational,
                    semantic_position: Some(position),
                    section_id: c.section_id,
                    line_hash: c.line_hash,
                    text: String::new(),
                });
            }
        }
    }
    if foundational_on {
        for c in out.iter_mut().filter(|c| c.foundational) {
            c.fused += profile.foundational_bonus;
        }
    }
    if rrf_k.is_none() {
        let mut sections: HashMap<(u64, u64), usize> = HashMap::new();
        for c in &out {
            *sections.entry((c.book_id, c.section_id)).or_default() += 1;
        }
        for c in &mut out {
            c.fused += profile.phrase_match_bonus.max(0.0) * phrase_match_bonus(&c.text, &features.quoted_phrases);
            c.fused += profile.rare_term_bonus.max(0.0) * rare_term_bonus(&c.text, &features.rare_tokens);
            if sections.get(&(c.book_id, c.section_id)).copied().unwrap_or(0) > 1 {
                c.fused += profile.section_coverage_bonus.max(0.0);
            }
        }
    }
    out.sort_by(best_first);
    if rrf_k.is_none() && profile.duplicate_penalty > 0.0 {
        let mut seen = HashSet::new();
        for c in &mut out {
            if c.line_hash != 0 && !seen.insert(c.line_hash) {
                c.fused -= profile.duplicate_penalty;
            }
        }
        out.sort_by(best_first);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(line_id: u64, bm25: f32) -> Lexical {
        Lexical { line_id, book_id: 1, bm25, section_id: line_id, line_hash: 0, text: String::new(), foundational: false }
    }
    fn sem(line_id: u64, score: f32) -> Semantic {
        Semantic { line_id, book_id: 1, score, section_id: line_id, line_hash: 0, foundational: false }
    }
    fn ids(f: &[Fused]) -> Vec<u64> {
        f.iter().map(|e| e.line_id).collect()
    }
    fn none() -> QueryFeatures {
        analyze_query("")
    }

    #[test]
    fn weighted_matches_a_hand_computed_example() {
        // Balanced: BM25 line 2 = 30 -> 0.75, line 1 = 10 -> 0.5; cosine line 1 = 0.6 -> 0.8,
        // line 3 = 0.2 -> 0.6; alpha 0.5: 1 = 0.25 + 0.4 + 0.10 = 0.75, 2 = 0.375, 3 = 0.3
        let f = fuse_candidates(vec![lex(2, 30.0), lex(1, 10.0)], vec![sem(1, 0.6), sem(3, 0.2)], 0.5, Mode::Hybrid, &RankingProfile::balanced(), &none());
        assert_eq!(ids(&f), vec![1, 2, 3]);
        for (e, w) in f.iter().zip([0.75, 0.375, 0.3]) {
            assert!((e.fused - w).abs() < 1e-6, "{e:?}");
        }
        assert_eq!(f[0].source, Source::Both);
    }

    #[test]
    fn rrf_with_otzarias_defaults_and_the_foundational_bonus() {
        // engine defaults: RRF 60, threshold 0.55, foundational bonus 0.002
        // lexical [1, 2]; semantic [3, 1, 4], line 4's cosine 0 -> 0.5 < 0.55 left out
        // 1 = 1/61 + 1/62, 3 = 1/61 (+ 0.002: foundational), 2 = 1/62
        let mut s3 = sem(3, 0.9);
        s3.foundational = true;
        let f = fuse_candidates(vec![lex(1, 9.0), lex(2, 5.0)], vec![s3, sem(1, 0.8), sem(4, 0.0)], 0.0, Mode::Hybrid, &RankingProfile::default(), &none());
        assert_eq!(ids(&f), vec![1, 3, 2]);
        assert!((f[0].fused - (1.0 / 61.0 + 1.0 / 62.0)).abs() < 1e-7);
        assert!((f[1].fused - (1.0 / 61.0 + 0.002)).abs() < 1e-7);
        assert!((f[2].fused - 1.0 / 62.0).abs() < 1e-7);
        // lexical-only mode keeps a semantic line under the threshold
        let f = fuse_candidates(vec![], vec![sem(4, 0.0)], 1.0, Mode::LexicalOnly, &RankingProfile::default(), &none());
        assert_eq!(ids(&f), vec![4]);
    }

    #[test]
    fn best_profile_bonuses_and_penalty() {
        // Adaptive: BM25 [40, 20] > 2k -> min-max [1, 0]; alpha 1 (lexical only)
        // phrase bonus 0.10 * 1 (text has the phrase), rare 0.05 * 1/1, section coverage 0.03 (two
        // lines in one section), duplicate penalty 0.08 for the second line with the same hash
        let p = RankingProfile::best();
        let q = analyze_query("\"ab cd\" abcdefg");
        assert_eq!((q.quoted_phrases.clone(), q.rare_tokens.clone()), (vec!["ab cd".to_string()], vec!["abcdefg".to_string()]));
        let mut a = lex(1, 40.0);
        a.text = "x ab cd abcdefg".into();
        a.section_id = 7;
        a.line_hash = 9;
        let mut b = lex(2, 20.0);
        b.section_id = 7;
        b.line_hash = 9;
        let f = fuse_candidates(vec![a, b], vec![], 1.0, Mode::LexicalOnly, &p, &q);
        assert_eq!(ids(&f), vec![1, 2]);
        assert!((f[0].fused - (1.0 + 0.10 + 0.05 + 0.03)).abs() < 1e-6, "{}", f[0].fused);
        assert!((f[1].fused - (0.0 + 0.03 - 0.08)).abs() < 1e-6, "{}", f[1].fused);
    }

    #[test]
    fn ties_go_foundational_then_semantic_order_then_line_id() {
        let w = RankingProfile { alpha_override: Some(1.0), ..RankingProfile::balanced() };
        let f = fuse_candidates(vec![lex(9, 10.0), lex(4, 10.0)], vec![], 1.0, Mode::Hybrid, &w, &none());
        assert_eq!(ids(&f), vec![4, 9]);
        let f = fuse_candidates(vec![], vec![sem(9, 0.5), sem(4, 0.5)], 0.0, Mode::Hybrid, &RankingProfile::balanced(), &none());
        assert_eq!(ids(&f), vec![9, 4]);
    }

    #[test]
    fn top_k_follows_the_multiplier() {
        assert_eq!(semantic_top_k(21, 0, &RankingProfile::default()), 42);
        assert_eq!(semantic_top_k(3, 0, &RankingProfile::fast()), 5);
        assert_eq!(semantic_top_k(10_000, 0, &RankingProfile::best()), MAX_SEMANTIC_CANDIDATES);
        assert_eq!(semantic_top_k(0, 0, &RankingProfile::default()), 1);
    }

    #[test]
    fn query_features_follow_otzaria() {
        let a = QueryTypeAlphas::default();
        let t = |q: &str| alpha_for(&analyze_query(q), &a);
        assert_eq!(t(""), 0.5);
        assert_eq!(t("one"), 0.7);
        assert_eq!(t("page 12"), 0.85);
        assert_eq!(t("a b c"), 0.5);
        assert_eq!(t("a b c d e"), 0.3);
        assert_eq!(t("say \"a b\" here"), 1.0);
        assert!(!has_quoted_phrase("\u{05E8}\u{05DE}\u{05D1}\"\u{05DD} x"));
        assert!(has_quoted_phrase("\u{201E}x y\u{201C}"));
        assert_eq!(normalize_query("  a\u{05B0}b\u{05F4}c   d "), "ab\"c d");
        assert!(RankingProfile::default().validate().is_ok() && RankingProfile::best().validate().is_ok());
        assert!(RankingProfile { candidate_window_multiplier: 11.0, ..Default::default() }.validate().is_err());
    }
}
