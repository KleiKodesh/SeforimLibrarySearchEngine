//! Semantic search (issue #18), like Otzaria: an exact scan over Otzaria's
//! own downloaded vector set (crate `seforim-vectors`: `.oxv` segments, base
//! + deltas, read-only, from the directory the host points to, e.g.
//! `%APPDATA%\otzaria\vectors`; nothing is converted or built), and the
//! hybrid fusion of a lexical search with a semantic one.
//!
//! The host supplies the query vector (Otzaria's query model) and the line
//! mapping: per Otzaria book name (`id:<bookId>`), its book id and the line
//! id of each line ordinal ([`LineIndex::set_vector_mapping`], kept in
//! memory); the engine never runs a model. Three modes:
//! - lexical only: the plain search, unchanged (the default);
//! - semantic only: [`LineIndex::semantic_search`], the top k lines by cosine
//!   similarity, filtered by books like the lexical search;
//! - hybrid: [`SearchResult::fuse_semantic`] fuses a finished lexical search
//!   with a semantic top-k list as Otzaria's hybrid search does: the lexical
//!   side is the BM25 of Otzaria's exact query (`relevance::hybrid_bm25`),
//!   fused by Otzaria's weighted (Balanced) or RRF (Fast) profile
//!   (`seforim_vectors::fusion`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use seforim_vectors::fusion::{self, Fusion, QueryTypeAlphas};
use seforim_vectors::set::{LineMap, SegmentSet};

pub use seforim_vectors::oxv::{OxvBookMap, OxvInfo};
pub use seforim_vectors::set::{LineSearch as SemanticOptions, SetInfo as VectorInfo, DEFAULT_WINDOW_PENALTY};
pub use seforim_vectors::CHUNK_BORROWED;

use crate::search::{CancelToken, SearchResult};
use crate::{Error, LineIndex, Result};

/// The vector set of an index: where it is (the host's directory or file),
/// the open set (dropped by `release_memory`, reopened lazily) and the
/// host's line mapping (kept in memory).
#[derive(Default)]
pub struct VectorCache {
    state: Mutex<VectorState>,
}

#[derive(Default)]
struct VectorState {
    path: Option<PathBuf>,
    set: Option<Arc<SegmentSet>>,
    map: Arc<LineMap>,
}

impl VectorCache {
    /// Drops the open set (issue #40): its mappings are released when the
    /// last search holding it ends; the next call reopens it. The host's
    /// line mapping stays.
    pub(crate) fn release(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).set = None;
    }
}

/// One semantic hit, a line: the vector's similarity minus this
/// occurrence's penalty, best first.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SemanticHit {
    pub line_id: u64,
    pub book_id: u64,
    pub score: f32,
    /// The occurrence's window (0 = the line's first; n its n-th window; bit
    /// 31 = found through a neighbour's vector, Otzaria's alias).
    #[serde(default)]
    pub chunk: u32,
}

/// A semantic search's hits and counts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SemanticResult {
    pub hits: Vec<SemanticHit>,
    /// Vectors the scan kept (Otzaria's top_k).
    pub vectors: u64,
    /// Records of those vectors read (admitted books only, under a filter).
    pub records: u64,
    /// Records the host's mapping does not cover (counted, never guessed).
    pub unmapped: u64,
    /// Slots the scan visited.
    pub scanned: u64,
    /// Scan threads used.
    pub threads: usize,
    /// The set generation searched (0: a single `.oxv`).
    pub generation: u64,
}


/// How to fuse (JSON at the C ABI: `{"strategy":"weighted","alpha":0.5}` or
/// `{"strategy":"rrf","k":60}`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "strategy", rename_all = "lowercase")]
pub enum FusionOptions {
    /// Otzaria's weighted fusion. `alpha` = lexical weight (0..1); None =
    /// Otzaria's table by query type (quoted phrase 1.0, exact reference
    /// 0.85, short 0.7, mixed 0.5, conceptual 0.3, unknown 0.5).
    Weighted {
        #[serde(default)]
        alpha: Option<f32>,
        #[serde(default = "default_bm25_k")]
        bm25_k: f32,
    },
    /// Reciprocal rank fusion.
    Rrf {
        #[serde(default = "default_rrf_k")]
        k: u32,
    },
}

fn default_bm25_k() -> f32 {
    fusion::DEFAULT_BM25_SATURATION_K
}

fn default_rrf_k() -> u32 {
    fusion::DEFAULT_RRF_K
}

impl Default for FusionOptions {
    fn default() -> Self {
        FusionOptions::Weighted { alpha: None, bm25_k: default_bm25_k() }
    }
}

/// One hybrid result.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FusedHit {
    pub line_id: u64,
    pub book_id: u64,
    /// The lexical hit's catalog position (None: a semantic-only line; the
    /// host reads it from its catalog if it needs one).
    pub position: Option<u64>,
    pub fused: f32,
    /// The lexical score: the BM25 of Otzaria's exact query (None: the exact
    /// query does not match the line).
    pub lexical: Option<f32>,
    /// The cosine similarity.
    pub semantic: Option<f32>,
    /// "lexical", "semantic" or "both".
    pub source: &'static str,
}

fn map_err(e: seforim_vectors::Error) -> Error {
    match e {
        seforim_vectors::Error::Io(e) => Error::Io(e),
        seforim_vectors::Error::Cancelled => Error::Cancelled,
        other => Error::Unsupported(other.to_string()),
    }
}

impl LineIndex {
    /// Opens Otzaria's vector set read-only (a set directory as Otzaria
    /// installs it, e.g. `%APPDATA%\otzaria\vectors`, or one base `.oxv`)
    /// and keeps it for [`semantic_search`](Self::semantic_search). Replaces
    /// a set opened before; the line mapping stays.
    pub fn open_vectors(&self, path: &Path) -> Result<VectorInfo> {
        let set = Arc::new(SegmentSet::open(path).map_err(map_err)?);
        let mut state = self.vectors.state.lock().unwrap();
        let info = set.info(Some(&state.map));
        state.path = Some(path.to_path_buf());
        state.set = Some(set);
        Ok(info)
    }

    /// Forgets the vector set (the line mapping stays).
    pub fn close_vectors(&self) {
        let mut state = self.vectors.state.lock().unwrap();
        state.path = None;
        state.set = None;
    }

    /// Registers the host's line mapping: per Otzaria book name, its book id
    /// and the line id of each line ordinal (the vectors' hint). `replace`
    /// drops every book mapped before; otherwise the books given are added
    /// or replaced.
    pub fn set_vector_mapping(&self, books: Vec<OxvBookMap>, replace: bool) {
        let mut state = self.vectors.state.lock().unwrap();
        Arc::make_mut(&mut state.map).set(books, replace);
    }

    /// The open set and the mapping (reopened after `release_memory`).
    fn vector_set(&self) -> Result<Option<(Arc<SegmentSet>, Arc<LineMap>)>> {
        let mut state = self.vectors.state.lock().unwrap();
        if state.set.is_none() {
            if let Some(path) = state.path.clone() {
                state.set = Some(Arc::new(SegmentSet::open(&path).map_err(map_err)?));
            }
        }
        Ok(state.set.clone().map(|s| (s, state.map.clone())))
    }

    /// The open set (None: none opened), with the mapping's counts.
    pub fn vector_info(&self) -> Result<Option<VectorInfo>> {
        Ok(self.vector_set()?.map(|(set, map)| set.info(Some(&map))))
    }

    /// The top `options.k` lines for `query` (the host's query vector, the
    /// set's width; normalized here if it is not a unit vector): Otzaria's
    /// exact scan over the set, the scanned vectors mapped to the host's
    /// line ids, one result per line. No set: an empty result.
    pub fn semantic_search(&self, query: &[f32], options: &SemanticOptions, cancel: Option<&CancelToken>) -> Result<SemanticResult> {
        let Some((set, map)) = self.vector_set()? else { return Ok(SemanticResult::default()) };
        let norm = query.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>().sqrt();
        let unit;
        let query = if (norm - 1.0).abs() > 1e-4 {
            unit = seforim_vectors::normalized(query).map_err(map_err)?;
            &unit[..]
        } else {
            query
        };
        let flag = cancel.map(|c| c.flag());
        let r = set.search_lines(query, &map, options, flag.as_deref()).map_err(map_err)?;
        Ok(SemanticResult {
            hits: r.hits.into_iter().map(|h| SemanticHit { line_id: h.line_id, book_id: h.book_id, score: h.score, chunk: h.chunk }).collect(),
            vectors: r.vectors,
            records: r.records,
            unmapped: r.unmapped,
            scanned: r.scanned,
            threads: r.threads,
            generation: set.generation(),
        })
    }
}

impl SearchResult {
    /// Hybrid ranking (issue #18): this finished search fused with a
    /// semantic top-k list (from [`LineIndex::semantic_search`] with the same
    /// book filter), as Otzaria's hybrid search fuses them. `query` is the
    /// lexical query string.
    ///
    /// The lexical list is Otzaria's: the hits ranked by the BM25 of
    /// Otzaria's exact query for `query` (`relevance::hybrid_bm25`: one word
    /// = its spellings as one term, several = the exact phrase), best first,
    /// then catalog order; the weighted fusion reads its query type for
    /// alpha when none is given. The search's own order and scores play no
    /// part, so any order works. Named deviations: Otzaria's lexical list is
    /// its exact query's top hits; here it is every hit of this search that
    /// the exact query matches. A hit the exact query does not match (a
    /// wildcard / fuzzy / far-apart word match) is no lexical candidate, as
    /// for Otzaria: it fuses as a semantic-only line when the semantic list
    /// has it, and otherwise is kept after every fused line (fused 0, no
    /// lexical score, source "lexical", in the search's order), since the
    /// engine never drops a hit. A streamed search must be complete
    /// ([`is_complete`](Self::is_complete)).
    pub fn fuse_semantic(&self, query: &str, semantic: &[SemanticHit], options: &FusionOptions) -> Result<Vec<FusedHit>> {
        if !self.is_complete() {
            return Err(Error::Unsupported("hybrid fusion needs every lexical hit: pull the search to the end first".into()));
        }
        let fusion = match *options {
            FusionOptions::Weighted { alpha, bm25_k } => Fusion::Weighted { alpha, bm25_k },
            FusionOptions::Rrf { k } => Fusion::Rrf { k },
        };
        let exact = match &self.hybrid {
            Some(h) if !self.hits.is_empty() => {
                crate::relevance::hybrid_bm25(&h.searcher, h.field, h.marked_terms, h.positions, query)?
            }
            _ => std::collections::HashMap::new(),
        };
        // the hits the exact query matches, each line once, best BM25 first,
        // then the search's (catalog) order
        let mut seen = std::collections::HashSet::with_capacity(self.hits.len());
        let mut lexical: Vec<(u64, f32)> = Vec::with_capacity(exact.len().min(self.hits.len()));
        let mut unmatched: Vec<u64> = Vec::new();
        for hit in &self.hits {
            if !seen.insert(hit.line_id) {
                continue;
            }
            match exact.get(&hit.line_id) {
                Some(&score) => lexical.push((hit.line_id, score)),
                None => unmatched.push(hit.line_id),
            }
        }
        lexical.sort_by(|a, b| b.1.total_cmp(&a.1));
        let places: std::collections::HashMap<u64, (u64, u64)> =
            self.hits.iter().rev().map(|h| (h.line_id, (h.book_id, h.position))).collect();
        let books: std::collections::HashMap<u64, u64> = semantic.iter().map(|h| (h.line_id, h.book_id)).collect();
        let semantic: Vec<(u64, f32)> = semantic.iter().map(|h| (h.line_id, h.score)).collect();
        let mut fused = fusion::fuse(&lexical, &semantic, fusion, query, &QueryTypeAlphas::default());
        let placed: std::collections::HashSet<u64> = fused.iter().map(|f| f.line_id).collect();
        fused.extend(unmatched.into_iter().filter(|id| !placed.contains(id)).map(|line_id| fusion::Fused {
            line_id,
            fused: 0.0,
            lexical: None,
            semantic: None,
            source: fusion::Source::Lexical,
        }));
        Ok(fused
            .into_iter()
            .map(|f| {
                let (book_id, position) = match places.get(&f.line_id) {
                    Some(&(b, p)) => (b, Some(p)),
                    None => (books.get(&f.line_id).copied().unwrap_or(0), None),
                };
                FusedHit {
                    line_id: f.line_id,
                    book_id,
                    position,
                    fused: f.fused,
                    lexical: f.lexical,
                    semantic: f.semantic,
                    source: match f.source {
                        fusion::Source::Lexical => "lexical",
                        fusion::Source::Semantic => "semantic",
                        fusion::Source::Both => "both",
                    },
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IndexOptions, LineDoc, SearchOptions};

    fn axis(a: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; 8];
        v[a] = 1.0;
        v
    }

    /// Lines 1..4 (books 10, 20) indexed; an Otzaria set (one base `.oxv`)
    /// holding books `id:10` (line ordinals 0, 1) and `id:20` (0, 1, a
    /// second window of ordinal 1 on axis 5).
    fn index_with_vectors(tmp: &Path) -> LineIndex {
        let index = LineIndex::create(&tmp.join("index"), IndexOptions::default()).unwrap();
        let lines = [(1u64, 10u64, "alpha beta"), (2, 10, "beta gamma"), (3, 20, "gamma delta"), (4, 20, "alpha")];
        {
            let mut w = index.writer(50_000_000, 1).unwrap();
            for (i, &(id, book, text)) in lines.iter().enumerate() {
                let doc = LineDoc { line_id: id, position: i as u64, book_id: book, text: text.into(), section_id: None, no_cross: false };
                w.add(&doc).unwrap();
            }
            w.commit(None).unwrap();
        }
        let oxv = tmp.join("lib.oxv");
        seforim_vectors::oxv::write_test_segment(
            &oxv,
            8,
            false,
            "v1",
            &[("id:10", vec![(0, 0, axis(1)), (1, 0, axis(2))]), ("id:20", vec![(0, 0, axis(3)), (1, 0, axis(4)), (1, 1, axis(5))])],
        )
        .unwrap();
        index.open_vectors(&oxv).unwrap();
        index.set_vector_mapping(
            vec![
                OxvBookMap { name: "id:10".into(), book_id: 10, line_ids: vec![1, 2] },
                OxvBookMap { name: "id:20".into(), book_id: 20, line_ids: vec![3, 4] },
            ],
            true,
        );
        index
    }

    #[test]
    fn semantic_and_hybrid_on_a_line_index() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = LineIndex::create(&tmp.path().join("bare"), IndexOptions::default()).unwrap();
        assert!(bare.vector_info().unwrap().is_none());
        assert!(bare.semantic_search(&[1.0; 8], &SemanticOptions::default(), None).unwrap().hits.is_empty());

        let index = index_with_vectors(tmp.path());
        let info = index.vector_info().unwrap().unwrap();
        assert_eq!((info.dims, info.slots_live, info.books, info.books_mapped, info.books_unmapped), (8, 5, 2, 2, 0));

        // the query is line 3's own vector (not unit length: normalized): line 3 first, score 1
        let mut q = axis(3);
        q[3] = 2.0;
        let r = index.semantic_search(&q, &SemanticOptions { k: 2, ..Default::default() }, None).unwrap();
        assert_eq!((r.hits[0].line_id, r.hits[0].book_id), (3, 20));
        assert!((r.hits[0].score - 1.0).abs() < 1e-5);
        // a second window: 0.04 off, chunk 1
        let r = index.semantic_search(&axis(5), &SemanticOptions { k: 1, ..Default::default() }, None).unwrap();
        assert_eq!((r.hits[0].line_id, r.hits[0].chunk), (4, 1));
        assert!((r.hits[0].score - 0.96).abs() < 1e-5);
        // book filter
        let opts = SemanticOptions { k: 4, books: Some(vec![10]), ..Default::default() };
        let r = index.semantic_search(&axis(3), &opts, None).unwrap();
        assert!(r.hits.iter().all(|h| h.book_id == 10) && r.hits.len() == 2);
        // cancelled
        let token = CancelToken::new();
        token.cancel();
        assert!(matches!(index.semantic_search(&axis(3), &SemanticOptions::default(), Some(&token)), Err(Error::Cancelled)));
        // an unmapped book's records are counted
        index.set_vector_mapping(vec![OxvBookMap { name: "id:10".into(), book_id: 10, line_ids: vec![1, 2] }], true);
        let r = index.semantic_search(&axis(3), &SemanticOptions { k: 1, ..Default::default() }, None).unwrap();
        assert_eq!((r.hits.len(), r.unmapped), (0, 1));
        index.set_vector_mapping(vec![OxvBookMap { name: "id:20".into(), book_id: 20, line_ids: vec![3, 4] }], false);

        // hybrid: every lexical hit stays, the semantic-only line joins
        let lexical = index.search_options(&SearchOptions { query: "alpha".into(), order: crate::ResultsOrder::Relevance, ..Default::default() }).unwrap();
        let semantic = index.semantic_search(&axis(3), &SemanticOptions { k: 1, ..Default::default() }, None).unwrap();
        let fused = lexical.fuse_semantic("alpha", &semantic.hits, &FusionOptions::default()).unwrap();
        let mut got: Vec<u64> = fused.iter().map(|f| f.line_id).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 3, 4]);
        let three = fused.iter().find(|f| f.line_id == 3).unwrap();
        assert_eq!((three.source, three.position, three.book_id), ("semantic", None, 20));
        // the search's order plays no part: catalog order fuses the same
        let catalog = index.search_options(&SearchOptions { query: "alpha".into(), ..Default::default() }).unwrap();
        assert_eq!(catalog.fuse_semantic("alpha", &semantic.hits, &FusionOptions::default()).unwrap(), fused);
        let rrf = catalog.fuse_semantic("alpha", &semantic.hits, &FusionOptions::Rrf { k: 60 }).unwrap();
        assert_eq!(rrf.iter().filter(|f| f.lexical.is_some()).count(), 2);
        // one-word query: alpha 0.7 (Otzaria's "short")
        let one = fused.iter().find(|f| f.line_id == 1).unwrap();
        let bm25 = one.lexical.unwrap();
        let both = if one.semantic.is_some() { fusion::BALANCED_AGREEMENT_BONUS } else { 0.0 };
        let want = 0.7 * fusion::normalize_bm25(bm25, 10.0) + 0.3 * one.semantic.map_or(0.0, fusion::normalize_cosine) + both;
        assert!((one.fused - want).abs() < 1e-6);

        // a bad set path is an error; closing forgets the set
        assert!(index.open_vectors(&tmp.path().join("missing")).is_err());
        index.close_vectors();
        assert!(index.vector_info().unwrap().is_none());
    }


    fn tantivy_scores(index: &LineIndex, q: &dyn tantivy::query::Query) -> std::collections::HashMap<u64, f32> {
        let searcher = index.reader.searcher();
        let top = searcher.search(q, &tantivy::collector::TopDocs::with_limit(100).order_by_score()).unwrap();
        top.into_iter()
            .map(|(s, a)| {
                let ids = searcher.segment_reader(a.segment_ord).fast_fields().u64("line_id").unwrap();
                (ids.first(a.doc_id).unwrap(), s)
            })
            .collect()
    }

    /// Issue #18: the lexical side of hybrid fusion is Otzaria's exact-query
    /// BM25, not the relevance order's tier boosts (every top row used to
    /// show lexical 1000.0).
    #[test]
    fn hybrid_lexical_side_is_otzarias_exact_query_bm25() {
        use tantivy::query::{PhraseQuery, TermQuery};
        use tantivy::schema::IndexRecordOption;
        use tantivy::Term;
        let tmp = tempfile::tempdir().unwrap();
        let index = LineIndex::create(tmp.path(), IndexOptions::default()).unwrap();
        let lines = [
            (1u64, "alpha beta gamma delta"),
            (2, "beta alpha"),
            (3, "alpha beta alpha beta"),
            (4, "gamma"),
            (5, "alphabet beta"),
        ];
        {
            let mut w = index.writer(50_000_000, 1).unwrap();
            for (i, &(id, text)) in lines.iter().enumerate() {
                let doc = LineDoc { line_id: id, position: i as u64, book_id: 1, text: text.into(), section_id: None, no_cross: false };
                w.add(&doc).unwrap();
            }
            w.commit(None).unwrap();
        }
        let field = index.fields.text;
        let term = |w: &str| Term::from_field_text(field, w);

        // one word: tantivy's TermQuery without frequencies (Otzaria's)
        let want = tantivy_scores(&index, &TermQuery::new(term("alpha"), IndexRecordOption::Basic));
        let r = index.search_options(&SearchOptions { query: "alpha".into(), order: crate::ResultsOrder::Relevance, ..Default::default() }).unwrap();
        assert!(r.scores().unwrap().iter().all(|&s| s >= 1000.0), "relevance order scores tier boosts");
        let fused = r.fuse_semantic("alpha", &[], &FusionOptions::default()).unwrap();
        assert_eq!(fused.len(), 3);
        for f in &fused {
            let bm25 = f.lexical.unwrap();
            assert!(bm25 < 100.0, "lexical {bm25}: the tier boost leaked in");
            assert!((bm25 - want[&f.line_id]).abs() < 1e-5);
            assert!((f.fused - 0.7 * fusion::normalize_bm25(bm25, 10.0)).abs() < 1e-6);
        }
        // shorter line, higher BM25, first
        assert_eq!(fused[0].line_id, 2);

        // two words: the exact phrase's BM25 (tantivy's PhraseQuery)
        let want = tantivy_scores(&index, &PhraseQuery::new(vec![term("alpha"), term("beta")]));
        let r = index.search_options(&SearchOptions { query: "alpha beta".into(), ..Default::default() }).unwrap();
        // the search (words anywhere) hits 1, 2, 3; the phrase matches 1 and 3
        let fused = r.fuse_semantic("alpha beta", &[], &FusionOptions::default()).unwrap();
        let got: Vec<(u64, Option<f32>)> = fused.iter().map(|f| (f.line_id, f.lexical)).collect();
        assert_eq!(got.len(), 3);
        assert_eq!(got[2], (2, None), "a hit the exact query misses comes last, no lexical score");
        assert_eq!(fused[2].fused, 0.0);
        for &(id, l) in &got[..2] {
            assert!((l.unwrap() - want[&id]).abs() < 1e-5);
        }

        // a hit the exact query misses but the semantic list has: semantic only
        let semantic = [SemanticHit { line_id: 2, book_id: 1, score: 0.6, chunk: 0 }];
        let fused = r.fuse_semantic("alpha beta", &semantic, &FusionOptions::default()).unwrap();
        let two = fused.iter().find(|f| f.line_id == 2).unwrap();
        assert_eq!((two.source, two.lexical, two.position), ("semantic", None, Some(1)));
        // two words: alpha 0.7; semantic-only 0.3 * 0.8
        assert!((two.fused - 0.3 * 0.8).abs() < 1e-6);

        // a line both found gets the agreement bonus
        let semantic = [SemanticHit { line_id: 3, book_id: 1, score: 0.0, chunk: 0 }];
        let fused = r.fuse_semantic("alpha beta", &semantic, &FusionOptions::default()).unwrap();
        let three = fused.iter().find(|f| f.line_id == 3).unwrap();
        let want3 = 0.7 * fusion::normalize_bm25(want[&3], 10.0) + 0.3 * 0.5 + 0.10;
        assert_eq!(three.source, "both");
        assert!((three.fused - want3).abs() < 1e-5);

        // RRF: the lexical ranks are the BM25 order; cosine 0 (0.5 < 0.55) left out
        let fused = r.fuse_semantic("alpha beta", &semantic, &FusionOptions::Rrf { k: 60 }).unwrap();
        let best = if want[&1] >= want[&3] { 1 } else { 3 };
        assert_eq!(fused[0].line_id, best);
        assert!((fused[0].fused - 1.0 / 61.0).abs() < 1e-7);
        assert!(fused.iter().all(|f| f.source != "both"));
    }

    /// Issue #40: release_memory drops the caches (sidecar, vector set,
    /// regex, line search, reader) and everything reopens lazily with the
    /// same answers, also while searches run on other threads.
    #[test]
    fn release_memory_keeps_answers_and_is_safe_mid_search() {
        let tmp = tempfile::tempdir().unwrap();
        let index = std::sync::Arc::new(LineIndex::create(tmp.path(), IndexOptions::default()).unwrap());
        {
            let mut w = index.writer(50_000_000, 1).unwrap();
            for i in 0..200u64 {
                let text = format!("alpha{} beta gamma{}", i % 7, i % 5);
                w.add(&LineDoc { line_id: i + 1, position: i, book_id: 1 + i % 4, text, section_id: None, no_cross: false })
                    .unwrap();
            }
            w.commit(None).unwrap();
            w.merge_all().unwrap();
        }
        assert!(index.build_fm_sidecar().unwrap());
        let oxv = tmp.path().join("v.oxv");
        let recs: Vec<(u32, u32, Vec<f32>)> = (0..8u32).map(|i| (i, 0, axis(i as usize))).collect();
        seforim_vectors::oxv::write_test_segment(&oxv, 8, false, "v1", &[("id:1", recs)]).unwrap();
        index.open_vectors(&oxv).unwrap();
        index.set_vector_mapping(vec![OxvBookMap { name: "id:1".into(), book_id: 1, line_ids: (1..=8).collect() }], true);
        let queries = ["*pha3", "beta~1", "/alpha[12]/", "/beta gamma[34]/", "alpha1 gamma2"];
        let ids = |index: &LineIndex, q: &str| -> Vec<u64> {
            let r = index.search_options(&SearchOptions { query: q.into(), user_regex: true, ..Default::default() }).unwrap();
            r.hits.iter().map(|h| h.line_id).collect()
        };
        let before: Vec<Vec<u64>> = queries.iter().map(|q| ids(&index, q)).collect();
        assert!(before.iter().all(|h| !h.is_empty()), "{:?}", before.iter().map(|h| h.len()).collect::<Vec<_>>());
        let sem = index.semantic_search(&axis(3), &SemanticOptions::default(), None).unwrap();
        assert_eq!(sem.hits[0].line_id, 4);
        // a session open across the release keeps working
        let mut open = index.search_options(&SearchOptions { query: "*pha3".into(), ..Default::default() }).unwrap();
        index.release_memory().unwrap();
        assert!(index.vectors.state.lock().unwrap().set.is_none());
        assert_eq!(open.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), before[0]);
        assert!(open.session.locate("alpha3 beta gamma1").is_some());
        // after: same answers, caches reopened lazily
        for (q, want) in queries.iter().zip(&before) {
            assert_eq!(&ids(&index, q), want, "{q}");
        }
        assert!(index.has_fm_sidecar());
        assert_eq!(index.semantic_search(&axis(3), &SemanticOptions::default(), None).unwrap(), sem);
        // releases racing searches on other threads
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let workers: Vec<_> = (0..3)
            .map(|t| {
                let (index, stop, before) = (index.clone(), stop.clone(), before.clone());
                std::thread::spawn(move || {
                    let mut runs = 0;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) || runs < 5 {
                        let k = (runs + t) % queries.len();
                        assert_eq!(ids(&index, queries[k]), before[k], "{}", queries[k]);
                        index.semantic_search(&axis(3), &SemanticOptions::default(), None).unwrap();
                        runs += 1;
                    }
                })
            })
            .collect();
        for _ in 0..50 {
            index.release_memory().unwrap();
            std::thread::yield_now();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for w in workers {
            w.join().unwrap();
        }
    }
}
