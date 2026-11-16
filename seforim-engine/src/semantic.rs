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
//! - hybrid: [`LineIndex::hybrid_session`] = Otzaria's hybrid session: per
//!   page a doubling candidate window of both sides, the lexical side the BM25
//!   top window of Otzaria's smart exact query
//!   (`relevance::hybrid_lexical_window`), fused by Otzaria's weighted
//!   (Balanced) or RRF (Fast) profile (`seforim_vectors::fusion`).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use seforim_vectors::fusion::{self, Fusion, QueryTypeAlphas};
use seforim_vectors::set::{LineMap, SegmentSet};

pub use seforim_vectors::oxv::{OxvBookMap, OxvInfo};
pub use seforim_vectors::chunker::BookNote;
pub use seforim_vectors::set::{BookVerify, LineSearch as SemanticOptions, SetInfo as VectorInfo, DEFAULT_WINDOW_PENALTY};
pub use seforim_vectors::CHUNK_BORROWED;

use crate::search::CancelToken;
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

    /// Maps one book WITH the text check (Otzaria's resolver): the book's raw
    /// rows (`lineIndex, id` order; `row_line_ids[i]` the line id of row `i`) and
    /// its `line_note` rows rebuild its lines as Otzaria indexes them (a row
    /// holding a newline is several lines of one line id), every record of the
    /// book in the open set is checked against its recomputed chunk key (moved
    /// lines found within 16 lines), and records that fail are dropped and
    /// counted. `identity`: the query model's chunking identity (None: the
    /// set's). Needs an open set.
    pub fn map_vector_book_rows(
        &self,
        name: &str,
        book_id: u64,
        row_line_ids: &[u64],
        rows: &[&[u8]],
        notes: &[BookNote],
        identity: Option<u64>,
    ) -> Result<BookVerify> {
        let Some((set, _)) = self.vector_set()? else {
            return Err(Error::Unsupported("open the vector set first".into()));
        };
        let (map, verify) = set.map_book_rows(name, book_id, row_line_ids, rows, notes, identity).map_err(map_err)?;
        self.set_vector_mapping(vec![map], false);
        Ok(verify)
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

/// Otzaria's `MAX_SEMANTIC_CANDIDATE_WINDOW` (api/search_engine.rs): the
/// largest window a hybrid session fuses.
pub const MAX_HYBRID_WINDOW: u32 = 10_000;

/// A hybrid search request (JSON at the C ABI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HybridOptions {
    /// Only these books (both sides). None = all.
    pub books: Option<Vec<u64>>,
    /// The fusion profile (Otzaria's Balanced weighted fusion by default).
    pub fusion: FusionOptions,
    /// Semantic scan threads (0 = Otzaria's default).
    pub threads: usize,
    pub window_penalty: f32,
    pub alias_penalty: f32,
}

impl Default for HybridOptions {
    fn default() -> Self {
        HybridOptions {
            books: None,
            fusion: FusionOptions::default(),
            threads: 0,
            window_penalty: DEFAULT_WINDOW_PENALTY,
            alias_penalty: DEFAULT_WINDOW_PENALTY,
        }
    }
}

/// One page of a hybrid session.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HybridPage {
    pub hits: Vec<FusedHit>,
    /// True when another page follows (Otzaria asks one past the page).
    pub more: bool,
    /// The window fused last (Otzaria's candidate window).
    pub window: u32,
    /// Lines the lexical query matches (the whole index, the book filter).
    pub lexical_total: u64,
    /// True when no larger window can add a result.
    pub exhausted: bool,
}

/// A step of [`HybridSession::page`]: the page, or (an index without
/// positions, a query of several words) the line texts the lexical side
/// needs first ([`HybridSession::add_texts`], then ask again).
#[derive(Debug, Clone, PartialEq)]
pub enum HybridStep {
    Page(HybridPage),
    NeedTexts(Vec<u64>),
}

/// Otzaria's hybrid search session (`semantic_search` with a session, api/
/// search_engine.rs, otzaria_search_engine 5babbce): results come in pages;
/// to serve a page the session fuses a candidate window of both sides, the
/// window growing as Otzaria grows it (`max(2 * (offset + limit) + 1, 2 *
/// window)`, at most [`MAX_HYBRID_WINDOW`]) until it holds one result past
/// the page or nothing more can come. Each window: the lexical side's best
/// `window` lines ([`crate::relevance`] `hybrid_lexical_window`: Otzaria's
/// smart exact query, BM25, the book filter) and the semantic side's best
/// `window` vectors (the exact scan, `k = window`), fused (Otzaria's
/// weighted / RRF fusion) and cut to `window`; the results the session does
/// not hold yet are appended in fused order (earlier results keep their
/// place). The window is exhausted when both sides returned less than asked
/// and the fusion fit the window, or at the maximum window.
pub struct HybridSession {
    source: crate::relevance::HybridSource,
    book_field: tantivy::schema::Field,
    position_field: tantivy::schema::Field,
    set: Option<(Arc<SegmentSet>, Arc<LineMap>)>,
    query: String,
    vector: Vec<f32>,
    options: HybridOptions,
    window: u32,
    entries: Vec<FusedHit>,
    held: std::collections::HashSet<u64>,
    exhausted: bool,
    lexical_total: u64,
    texts: std::collections::HashMap<u64, String>,
}

impl LineIndex {
    /// Opens Otzaria's hybrid search session for `query` (the lexical
    /// string) and `vector` (its embedding by the host's query model; empty or
    /// no open vector set: the lexical side alone, as Otzaria falls back).
    pub fn hybrid_session(&self, query: &str, vector: &[f32], options: HybridOptions) -> Result<HybridSession> {
        let set = if vector.is_empty() { None } else { self.vector_set()? };
        let vector = if vector.is_empty() || (vector.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>().sqrt() - 1.0).abs() <= 1e-4 {
            vector.to_vec()
        } else {
            seforim_vectors::normalized(vector).map_err(map_err)?
        };
        Ok(HybridSession {
            source: self.hybrid_source(None),
            book_field: self.fields.book_id,
            position_field: self.fields.position,
            set,
            query: query.to_string(),
            vector,
            options,
            window: 0,
            entries: Vec::new(),
            held: Default::default(),
            exhausted: false,
            lexical_total: 0,
            texts: Default::default(),
        })
    }
}

impl HybridSession {
    /// Line texts the lexical side asked for ([`HybridStep::NeedTexts`]).
    pub fn add_texts(&mut self, texts: impl IntoIterator<Item = (u64, String)>) {
        self.texts.extend(texts);
    }

    /// The results `offset .. offset + limit` (Otzaria's page), growing the
    /// window as needed.
    pub fn page(&mut self, offset: u32, limit: u32, cancel: Option<&CancelToken>) -> Result<HybridStep> {
        let wanted = offset as usize + limit as usize + 1;
        while self.entries.len() < wanted && !self.exhausted {
            if cancel.is_some_and(|c| c.is_cancelled()) {
                return Err(Error::Cancelled);
            }
            let requested = (u64::from(offset) + u64::from(limit)).saturating_mul(2).saturating_add(1).max(u64::from(self.window).saturating_mul(2));
            let window = requested.min(u64::from(MAX_HYBRID_WINDOW)) as u32;
            match self.expand(window, cancel)? {
                Some(need) => return Ok(HybridStep::NeedTexts(need)),
                None => self.window = window,
            }
        }
        let start = (offset as usize).min(self.entries.len());
        let end = (start + limit as usize).min(self.entries.len());
        Ok(HybridStep::Page(HybridPage {
            hits: self.entries[start..end].to_vec(),
            more: self.entries.len() > end,
            window: self.window,
            lexical_total: self.lexical_total,
            exhausted: self.exhausted,
        }))
    }

    /// One window fused (Otzaria's `expand_semantic_session`); Some = texts needed.
    fn expand(&mut self, window: u32, cancel: Option<&CancelToken>) -> Result<Option<Vec<u64>>> {
        let books = self.options.books.as_deref();
        let lexical = match crate::relevance::hybrid_lexical_window(
            &self.source,
            self.book_field,
            self.position_field,
            &self.query,
            books,
            window as usize,
            &self.texts,
        )? {
            Ok(l) => l,
            Err(crate::relevance::NeedTexts(ids)) => return Ok(Some(ids)),
        };
        let (candidates, total) = lexical;
        self.lexical_total = total;
        let lexical_done = candidates.len() < window as usize;
        let (semantic, semantic_done) = match &self.set {
            Some((set, map)) => {
                let opts = SemanticOptions {
                    k: window as usize,
                    books: self.options.books.clone(),
                    threads: self.options.threads,
                    window_penalty: self.options.window_penalty,
                    alias_penalty: self.options.alias_penalty,
                };
                let flag = cancel.map(|c| c.flag());
                let r = set.search_lines(&self.vector, map, &opts, flag.as_deref()).map_err(map_err)?;
                let done = (r.vectors as usize) < window as usize;
                (r.hits, done)
            }
            None => (Vec::new(), true),
        };
        let fusion = match self.options.fusion {
            FusionOptions::Weighted { alpha, bm25_k } => Fusion::Weighted { alpha, bm25_k },
            FusionOptions::Rrf { k } => Fusion::Rrf { k },
        };
        let lex: Vec<(u64, f32)> = candidates.iter().map(|c| (c.line_id, c.bm25)).collect();
        let sem: Vec<(u64, f32)> = semantic.iter().map(|h| (h.line_id, h.score)).collect();
        let mut fused = fusion::fuse(&lex, &sem, fusion, &self.query, &QueryTypeAlphas::default());
        let fused_count = fused.len();
        fused.truncate(window as usize);
        self.exhausted = (lexical_done && semantic_done && fused_count <= window as usize) || window >= MAX_HYBRID_WINDOW;
        let places: std::collections::HashMap<u64, (u64, u64)> = candidates.iter().map(|c| (c.line_id, (c.book_id, c.position))).collect();
        let books: std::collections::HashMap<u64, u64> = semantic.iter().map(|h| (h.line_id, h.book_id)).collect();
        for f in fused {
            if !self.held.insert(f.line_id) {
                continue;
            }
            let (book_id, position) = match places.get(&f.line_id) {
                Some(&(b, p)) => (b, Some(p)),
                None => (books.get(&f.line_id).copied().unwrap_or(0), None),
            };
            self.entries.push(FusedHit {
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
            });
        }
        Ok(None)
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

        // hybrid: the semantic-only line joins the lexical ones
        let mut h = index.hybrid_session("alpha", &axis(3), HybridOptions::default()).unwrap();
        let p = page(&mut h, 0, 10, &[]);
        let mut got: Vec<u64> = p.hits.iter().map(|f| f.line_id).collect();
        got.sort_unstable();
        assert!(got.contains(&1) && got.contains(&4) && got.contains(&3), "{got:?}");
        let three = p.hits.iter().find(|f| f.line_id == 3).unwrap();
        assert_eq!((three.source, three.position, three.book_id), ("semantic", None, 20));
        let one = p.hits.iter().find(|f| f.line_id == 1).unwrap();
        let both = if one.semantic.is_some() { fusion::BALANCED_AGREEMENT_BONUS } else { 0.0 };
        let want = 0.7 * fusion::normalize_bm25(one.lexical.unwrap(), 10.0) + 0.3 * one.semantic.map_or(0.0, fusion::normalize_cosine) + both;
        assert!((one.fused - want).abs() < 1e-6);
        let rrf = HybridOptions { fusion: FusionOptions::Rrf { k: 60 }, ..Default::default() };
        let p = page(&mut index.hybrid_session("alpha", &axis(3), rrf).unwrap(), 0, 10, &[]);
        assert_eq!(p.hits.iter().filter(|f| f.lexical.is_some()).count(), 2);

        // a bad set path is an error; closing forgets the set
        assert!(index.open_vectors(&tmp.path().join("missing")).is_err());
        index.close_vectors();
        assert!(index.vector_info().unwrap().is_none());
    }


    /// A page of `s`, the line texts given from `texts` when the session asks.
    fn page(s: &mut HybridSession, offset: u32, limit: u32, texts: &[(u64, &str)]) -> HybridPage {
        loop {
            match s.page(offset, limit, None).unwrap() {
                HybridStep::Page(p) => return p,
                HybridStep::NeedTexts(ids) => {
                    let given: Vec<(u64, String)> =
                        texts.iter().filter(|(id, _)| ids.contains(id)).map(|(id, t)| (*id, t.to_string())).collect();
                    assert_eq!(given.len(), ids.len(), "texts asked for unknown lines {ids:?}");
                    s.add_texts(given);
                }
            }
        }
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

    const LINES: [(u64, &str); 6] = [
        (1, "alpha beta gamma delta"),
        (2, "beta alpha"),
        (3, "alpha beta alpha beta"),
        (4, "gamma"),
        (5, "alphabet beta"),
        (6, "alpha beta"),
    ];

    fn build(positions: bool) -> (tempfile::TempDir, LineIndex) {
        let tmp = tempfile::tempdir().unwrap();
        let index = LineIndex::create(tmp.path(), IndexOptions { positions, ..IndexOptions::default() }).unwrap();
        {
            let mut w = index.writer(50_000_000, 1).unwrap();
            for (i, &(id, text)) in LINES.iter().enumerate() {
                let book = if id == 6 { 2 } else { 1 };
                let doc = LineDoc { line_id: id, position: i as u64, book_id: book, text: text.into(), section_id: None, no_cross: false };
                w.add(&doc).unwrap();
            }
            w.commit(None).unwrap();
        }
        (tmp, index)
    }

    /// Issue #18, Otzaria's hybrid lexical side: its exact query's BM25 over
    /// the whole index (tantivy's own TermQuery / PhraseQuery: Otzaria's
    /// queries), the best `window` lines, the book filter, the window growing
    /// as Otzaria grows it; the phrase of an index without positions scored
    /// from the line texts, equal to the positions index's phrase BM25.
    #[test]
    fn hybrid_session_is_otzarias() {
        use tantivy::query::{PhraseQuery, TermQuery};
        use tantivy::schema::IndexRecordOption;
        use tantivy::Term;
        let (_t, index) = build(true);
        let field = index.fields.text;
        let term = |w: &str| Term::from_field_text(field, w);

        // one word: TermQuery without frequencies, best first, Balanced fusion
        let want = tantivy_scores(&index, &TermQuery::new(term("alpha"), IndexRecordOption::Basic));
        let mut s = index.hybrid_session("alpha", &[], HybridOptions::default()).unwrap();
        let p = page(&mut s, 0, 10, &[]);
        assert_eq!((p.hits.len(), p.lexical_total, p.more, p.window, p.exhausted), (4, 4, false, 21, true));
        let mut by_score: Vec<(u64, f32)> = want.iter().map(|(&k, &v)| (k, v)).collect();
        by_score.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        assert_eq!(p.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), by_score.iter().map(|x| x.0).collect::<Vec<_>>());
        for h in &p.hits {
            let bm25 = h.lexical.unwrap();
            assert!((bm25 - want[&h.line_id]).abs() < 1e-5);
            // one word = Otzaria's "short" query: alpha 0.7
            assert!((h.fused - 0.7 * fusion::normalize_bm25(bm25, 10.0)).abs() < 1e-6);
            assert_eq!(h.source, "lexical");
        }

        // two words: the exact phrase's BM25 (PhraseQuery), lines 1, 3, 6
        let want = tantivy_scores(&index, &PhraseQuery::new(vec![term("alpha"), term("beta")]));
        assert_eq!(want.len(), 3);
        let mut s = index.hybrid_session("alpha beta", &[], HybridOptions::default()).unwrap();
        let all = page(&mut s, 0, 10, &[]);
        assert_eq!(all.hits.len(), 3);
        for h in &all.hits {
            assert!((h.lexical.unwrap() - want[&h.line_id]).abs() < 1e-5);
        }
        // pages: limit 1 asks a window of 3 (2 * (0 + 1) + 1), then 2 * 3
        let mut s = index.hybrid_session("alpha beta", &[], HybridOptions::default()).unwrap();
        let p0 = page(&mut s, 0, 1, &[]);
        assert_eq!((p0.window, p0.more, p0.hits[0].line_id), (3, true, all.hits[0].line_id));
        let p2 = page(&mut s, 2, 1, &[]);
        assert_eq!((p2.window, p2.more, p2.hits[0].line_id), (7, false, all.hits[2].line_id));
        // the book filter
        let mut s = index.hybrid_session("alpha beta", &[], HybridOptions { books: Some(vec![2]), ..Default::default() }).unwrap();
        let p = page(&mut s, 0, 10, &[]);
        assert_eq!(p.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![6]);
        assert_eq!(p.lexical_total, 1);

        // an index without positions: the same phrase scores, from the texts
        let (_t2, flat) = build(false);
        let mut s = flat.hybrid_session("alpha beta", &[], HybridOptions::default()).unwrap();
        assert!(matches!(s.page(0, 10, None).unwrap(), HybridStep::NeedTexts(ids) if ids == vec![1, 2, 3, 6]));
        let p = page(&mut s, 0, 10, &LINES);
        assert_eq!(p.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), all.hits.iter().map(|h| h.line_id).collect::<Vec<_>>());
        for h in &p.hits {
            assert!((h.lexical.unwrap() - want[&h.line_id]).abs() < 1e-4, "{} {:?} {}", h.line_id, h.lexical, want[&h.line_id]);
        }
    }

    /// Otzaria's `smart_query::alternative`: a placeholder spelling of the
    /// divine name (up to two prefixes, marks kept) gives the full spelling.
    #[test]
    fn placeholder_alternative_is_otzarias() {
        use crate::relevance::placeholder_alternative as alt;
        let (y, h, v, q, d) = ('\u{05D9}', '\u{05D4}', '\u{05D5}', '\u{05E7}', '\u{05D3}');
        let full = format!("{y}{h}{v}{h}");
        assert_eq!(alt(&format!("{y}{q}{v}{q}")), Some(full.clone()));
        assert_eq!(alt(&format!("{y}{d}{v}{d}")), Some(full.clone()));
        assert_eq!(alt(&format!("{y}{q}{v}{d}")), None, "the two placeholders differ");
        // two prefixes (vav, lamed), a mark kept after the yod
        let lamed = '\u{05DC}';
        assert_eq!(alt(&format!("{v}{lamed}{y}\u{05B0}{q}{v}{q}")), Some(format!("{v}{lamed}{y}\u{05B0}{h}{v}{h}")));
        // three prefixes: no
        assert_eq!(alt(&format!("{v}{v}{lamed}{y}{q}{v}{q}")), None);
        assert_eq!(alt(&full), None);
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
