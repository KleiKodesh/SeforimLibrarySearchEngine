//! Semantic search (issue #18): the vector sidecar of a line index
//! (`<index>/vectors/`, crate `seforim-vectors`: a usearch HNSW graph with i8
//! vectors + f16 full vectors for the rescore and the exact filtered scan)
//! and the hybrid fusion of a lexical search with a semantic one.
//!
//! The host supplies the vectors (one per line, from its own embedding
//! model) and the query vector; the engine never runs a model. Three modes:
//! - lexical only: the plain search, unchanged (the default);
//! - semantic only: [`LineIndex::semantic_search`], the top k lines by cosine
//!   similarity, filtered by books like the lexical search;
//! - hybrid: [`SearchResult::fuse_semantic`] fuses a finished lexical search
//!   (EVERY lexical hit) with a semantic top-k list, Otzaria's weighted fusion
//!   or RRF (`seforim_vectors::fusion`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use seforim_vectors::fusion::{self, Fusion, QueryTypeAlphas};
use seforim_vectors::{BuildParams, SearchParams, ThreadHooks, VectorStore};

pub use seforim_vectors::{ApplyStats, VectorRecord, VectorUpdate};

use crate::search::{CancelToken, SearchResult};
use crate::{Error, LineIndex, Result};

/// The sidecar directory's name inside the index directory.
pub const VECTORS_DIR: &str = "vectors";

/// The open vector store of an index (reopened when a newer generation is
/// published) and the lock that serializes writes.
#[derive(Default)]
pub struct VectorCache {
    store: Mutex<Option<Arc<VectorStore>>>,
    write: Mutex<()>,
}

/// A semantic search request (JSON at the C ABI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SemanticOptions {
    /// How many lines (a ranked cut list by nature).
    pub k: usize,
    /// Only these books (the lexical search's `books` filter). None = all.
    pub books: Option<Vec<u64>>,
    /// Graph candidates = ceil(k * oversample), rescored with the full vectors.
    pub oversample: f32,
    /// A filter leaving at most this many lines is an exact scan.
    pub exact_threshold: usize,
}

impl Default for SemanticOptions {
    fn default() -> Self {
        let p = SearchParams::new(10);
        SemanticOptions { k: p.k, books: None, oversample: p.oversample, exact_threshold: p.exact_threshold }
    }
}

/// One semantic hit: cosine similarity (-1..1), best first, ties by line id.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SemanticHit {
    pub line_id: u64,
    pub book_id: u64,
    pub score: f32,
}

/// A semantic search's hits and how it was served.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SemanticResult {
    pub hits: Vec<SemanticHit>,
    /// True: exact scan of the filtered lines; false: HNSW + rescore.
    pub exact: bool,
    /// Lines the filter allows.
    pub allowed: u64,
    /// The store generation searched.
    pub generation: u64,
}

/// The vector store of an index, as reported to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VectorInfo {
    pub dims: usize,
    pub lines: u64,
    pub books: u64,
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
    /// The lexical score (relevance score; for RRF in catalog order, None
    /// scores are reported as 0).
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
    /// The vector sidecar directory of this index.
    pub fn vectors_dir(&self) -> PathBuf {
        self.dir.join(VECTORS_DIR)
    }

    /// The current vector store (None: no vectors yet). A store published
    /// since the last call (by this or another process) is picked up here;
    /// a search holding the older one keeps it.
    pub fn vector_store(&self) -> Result<Option<Arc<VectorStore>>> {
        let dir = self.vectors_dir();
        let latest = seforim_vectors::latest_generation(&dir);
        let mut cached = self.vectors.store.lock().unwrap();
        if cached.as_ref().map(|s| s.generation()) != latest {
            *cached = VectorStore::open(&dir).map_err(map_err)?.map(Arc::new);
        }
        Ok(cached.clone())
    }

    /// The vector store's size (None: no vectors).
    pub fn vector_info(&self) -> Result<Option<VectorInfo>> {
        Ok(self.vector_store()?.map(|s| VectorInfo {
            dims: s.dims(),
            lines: s.len() as u64,
            books: s.book_counts().len() as u64,
            generation: s.generation(),
        }))
    }

    /// Adds / removes / replaces line vectors (by line or by whole book,
    /// following the per-book index updates of issue #29) and publishes a new
    /// store generation. `dims` must match an existing store. Runs in the
    /// OS background mode (issue #19), its graph workers too; writes are
    /// serialized per index. Searches keep running on the old generation.
    pub fn update_vectors(&self, dims: usize, update: &VectorUpdate, cancel: Option<&CancelToken>) -> Result<ApplyStats> {
        let _write = self.vectors.write.lock().unwrap();
        let _background = tantivy::indexer::BackgroundPriority::enter();
        let hooks = ThreadHooks { start: Some(crate::resources::begin_background), end: Some(crate::resources::end_background) };
        let flag = cancel.map(|c| c.flag());
        let stats = seforim_vectors::apply(&self.vectors_dir(), &BuildParams::new(dims), update, hooks, flag.as_deref())
            .map_err(map_err)?;
        // the next search opens the new generation
        let _ = self.vector_store();
        Ok(stats)
    }

    /// The top `options.k` lines by cosine similarity to `query` (the
    /// host's query vector, any length > 0 as long as it matches the store).
    /// No store: an empty result.
    pub fn semantic_search(&self, query: &[f32], options: &SemanticOptions, cancel: Option<&CancelToken>) -> Result<SemanticResult> {
        let Some(store) = self.vector_store()? else { return Ok(SemanticResult::default()) };
        let params = SearchParams {
            k: options.k,
            books: options.books.clone(),
            oversample: options.oversample,
            exact_threshold: options.exact_threshold,
        };
        let flag = cancel.map(|c| c.flag());
        let (hits, path) = store.search_with_path(query, &params, flag.as_deref()).map_err(map_err)?;
        Ok(SemanticResult {
            hits: hits.into_iter().map(|h| SemanticHit { line_id: h.line_id, book_id: h.book_id, score: h.score }).collect(),
            exact: path.exact,
            allowed: path.allowed,
            generation: store.generation(),
        })
    }
}

impl SearchResult {
    /// Hybrid ranking: EVERY lexical hit of this finished search fused with
    /// a semantic top-k list (from [`LineIndex::semantic_search`] with the
    /// same book filter). `query` is the lexical query string: the weighted
    /// fusion reads its type for alpha when none is given.
    ///
    /// Weighted fusion needs lexical scores: run the search in relevance
    /// order (`order: "relevance"`). RRF reads ranks only, so any order
    /// works (catalog order ranks by position). A streamed search must be
    /// complete ([`is_complete`](Self::is_complete)). A line hit twice
    /// (cross-line) counts once, at its best score / first rank.
    pub fn fuse_semantic(&self, query: &str, semantic: &[SemanticHit], options: &FusionOptions) -> Result<Vec<FusedHit>> {
        if !self.is_complete() {
            return Err(Error::Unsupported("hybrid fusion needs every lexical hit: pull the search to the end first".into()));
        }
        let scores = self.scores();
        let fusion = match *options {
            FusionOptions::Weighted { alpha, bm25_k } => {
                if scores.is_none() && !self.hits.is_empty() {
                    return Err(Error::Unsupported("weighted fusion needs lexical scores: search in relevance order".into()));
                }
                Fusion::Weighted { alpha, bm25_k }
            }
            FusionOptions::Rrf { k } => Fusion::Rrf { k },
        };
        let mut seen = std::collections::HashMap::with_capacity(self.hits.len());
        let mut lexical: Vec<(u64, f32)> = Vec::with_capacity(self.hits.len());
        for (i, hit) in self.hits.iter().enumerate() {
            let score = scores.map_or(0.0, |s| s[i]);
            match seen.get(&hit.line_id) {
                Some(&at) => {
                    let entry: &mut (u64, f32) = &mut lexical[at];
                    entry.1 = entry.1.max(score);
                }
                None => {
                    seen.insert(hit.line_id, lexical.len());
                    lexical.push((hit.line_id, score));
                }
            }
        }
        let places: std::collections::HashMap<u64, (u64, u64)> =
            self.hits.iter().rev().map(|h| (h.line_id, (h.book_id, h.position))).collect();
        let books: std::collections::HashMap<u64, u64> = semantic.iter().map(|h| (h.line_id, h.book_id)).collect();
        let semantic: Vec<(u64, f32)> = semantic.iter().map(|h| (h.line_id, h.score)).collect();
        let fused = fusion::fuse(&lexical, &semantic, fusion, query, &QueryTypeAlphas::default());
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

    fn vector(seed: u64, dims: usize) -> Vec<f32> {
        (0..dims).map(|i| (((seed * 31 + i as u64 * 17) % 97) as f32) - 48.0).collect()
    }

    #[test]
    fn semantic_and_hybrid_on_a_line_index() {
        let tmp = tempfile::tempdir().unwrap();
        let index = LineIndex::create(tmp.path(), IndexOptions::default()).unwrap();
        let lines = [(1u64, 10u64, "alpha beta"), (2, 10, "beta gamma"), (3, 20, "gamma delta"), (4, 20, "alpha")];
        {
            let mut w = index.writer(50_000_000, 1).unwrap();
            for (i, &(id, book, text)) in lines.iter().enumerate() {
                let doc = LineDoc { line_id: id, position: i as u64, book_id: book, text: text.into(), section_id: None, no_cross: false };
                w.add(&doc).unwrap();
            }
            w.commit(None).unwrap();
        }
        assert!(index.vector_info().unwrap().is_none());
        assert!(index.semantic_search(&[1.0; 8], &SemanticOptions::default(), None).unwrap().hits.is_empty());

        let update = VectorUpdate {
            add: lines.iter().map(|&(id, book, _)| VectorRecord { line_id: id, book_id: book, vector: vector(id, 8) }).collect(),
            ..Default::default()
        };
        let stats = index.update_vectors(8, &update, None).unwrap();
        assert_eq!((stats.lines, stats.books), (4, 2));
        assert_eq!(index.vector_info().unwrap().unwrap().lines, 4);

        // the query is line 3's own vector: line 3 first, score 1
        let r = index.semantic_search(&vector(3, 8), &SemanticOptions { k: 2, ..Default::default() }, None).unwrap();
        assert_eq!(r.hits[0].line_id, 3);
        assert!((r.hits[0].score - 1.0).abs() < 1e-3);
        assert!(r.exact);
        // book filter
        let opts = SemanticOptions { k: 4, books: Some(vec![10]), ..Default::default() };
        let r = index.semantic_search(&vector(3, 8), &opts, None).unwrap();
        assert!(r.hits.iter().all(|h| h.book_id == 10) && r.hits.len() == 2);
        // cancelled
        let token = CancelToken::new();
        token.cancel();
        assert!(matches!(index.semantic_search(&vector(3, 8), &SemanticOptions::default(), Some(&token)), Err(Error::Cancelled)));

        // hybrid: every lexical hit stays, the semantic-only line joins
        let lexical = index.search_options(&SearchOptions { query: "alpha".into(), order: crate::ResultsOrder::Relevance, ..Default::default() }).unwrap();
        let semantic = index.semantic_search(&vector(3, 8), &SemanticOptions { k: 1, ..Default::default() }, None).unwrap();
        let fused = lexical.fuse_semantic("alpha", &semantic.hits, &FusionOptions::default()).unwrap();
        let mut got: Vec<u64> = fused.iter().map(|f| f.line_id).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 3, 4]);
        let three = fused.iter().find(|f| f.line_id == 3).unwrap();
        assert_eq!((three.source, three.position, three.book_id), ("semantic", None, 20));
        // weighted needs relevance scores; RRF takes any order
        let catalog = index.search_options(&SearchOptions { query: "alpha".into(), ..Default::default() }).unwrap();
        assert!(catalog.fuse_semantic("alpha", &semantic.hits, &FusionOptions::default()).is_err());
        let rrf = catalog.fuse_semantic("alpha", &semantic.hits, &FusionOptions::Rrf { k: 60 }).unwrap();
        assert_eq!(rrf.len(), 3);
        // one-word query: alpha 0.7 (Otzaria's "short")
        let one = fused.iter().find(|f| f.line_id == 1).unwrap();
        let bm25 = one.lexical.unwrap();
        let want = 0.7 * fusion::normalize_bm25(bm25, 10.0) + 0.3 * one.semantic.map_or(0.0, fusion::normalize_cosine);
        assert!((one.fused - want).abs() < 1e-6);

        // a book's vectors replaced: the old lines go
        let update = VectorUpdate {
            remove_books: vec![20],
            add: vec![VectorRecord { line_id: 5, book_id: 20, vector: vector(5, 8) }],
            ..Default::default()
        };
        index.update_vectors(8, &update, None).unwrap();
        let r = index.semantic_search(&vector(3, 8), &SemanticOptions { k: 10, ..Default::default() }, None).unwrap();
        let mut ids: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2, 5]);
        // wrong dims refused
        assert!(index.update_vectors(4, &VectorUpdate::default(), None).is_err());
    }
}
