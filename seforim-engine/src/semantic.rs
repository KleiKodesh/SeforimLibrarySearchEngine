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
use seforim_vectors::fusion;
pub use seforim_vectors::fusion::{FusionStrategy, QueryTypeAlphas, RankingProfile};
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
    /// The semantic match's window (Otzaria's `matched_window`: 0 = the
    /// line's first; n its n-th window; bit 31 = found through a
    /// neighbour's vector) for the host's passage highlights; 0 for a line
    /// the semantic side did not find.
    #[serde(default)]
    pub matched_window: u32,
    /// The line's section (the `section_id` field; Otzaria's provenance
    /// section, its SameSection grouping key with the book).
    #[serde(default)]
    pub section_id: u64,
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

/// Otzaria's `SemanticRetrievalMode` (and its `SemanticExecutedMode`, the
/// mode that ran): JSON `"hybrid"`, `"semantic_only"`, `"lexical_only"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalMode {
    /// Both sides fused (default).
    #[default]
    Hybrid,
    /// The semantic side alone: the lexical candidates are discarded; the
    /// lexical side is only counted, once a session.
    SemanticOnly,
    /// The lexical side alone: no vector scan.
    LexicalOnly,
}

/// Otzaria's `SemanticLexicalMode`: JSON `"exact"` / `"fuzzy"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalMode {
    /// Otzaria's smart exact query (default).
    #[default]
    Exact,
    /// Otzaria's smart fuzzy query (`build_smart_fuzzy_query`): every word
    /// within `fuzzy_max_distance` edits, a quoted phrase's words also
    /// literal and adjacent. A query that is one quotation is exact.
    Fuzzy,
}

/// A hybrid search request (JSON at the C ABI).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct HybridOptions {
    /// Only these books (both sides). None = all.
    pub books: Option<Vec<u64>>,
    /// The foundational books (Otzaria's `/base` facet: the host's base
    /// books), for the profile's foundational bonus and foundational query.
    pub foundational_books: Option<Vec<u64>>,
    /// Otzaria's ranking profile (default: otzaria_search_engine's defaults).
    pub ranking: RankingProfile,
    /// Semantic scan threads (0 = Otzaria's default).
    pub threads: usize,
    /// Otzaria's retrieval mode (default hybrid).
    pub retrieval: RetrievalMode,
    /// Otzaria's lexical mode (default exact).
    pub lexical_mode: LexicalMode,
    /// The fuzzy lexical mode's edit distance (0 to 2; default 1). 0 is
    /// Otzaria's distance-0 fuzzy word: each word exact, quotes zero-width,
    /// or its quote-free form (the app's smart search).
    pub fuzzy_max_distance: u8,
    /// Otzaria's `match_nikud` on every lexical candidate and count: typed
    /// nikud required on its letter, other marks free, only lines carrying
    /// a mark. Needs an index built with `keep_marks`.
    pub match_nikud: bool,
    /// Otzaria's `match_taamim`: the same for te'amim.
    pub match_teamim: bool,
}

impl Default for HybridOptions {
    fn default() -> Self {
        HybridOptions {
            books: None,
            foundational_books: None,
            ranking: RankingProfile::default(),
            threads: 0,
            retrieval: RetrievalMode::Hybrid,
            lexical_mode: LexicalMode::Exact,
            fuzzy_max_distance: 1,
            match_nikud: false,
            match_teamim: false,
        }
    }
}

/// How a hybrid session served its results (Otzaria's
/// `SemanticSearchResponse` fields beyond the results).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HybridMeta {
    /// Lines the lexical query matches (the whole index, the book filter).
    pub lexical_total: u64,
    /// The lexical side hit an expansion cap (Otzaria's `truncated`).
    pub lexical_truncated: bool,
    /// Candidates the last window fused (Otzaria's `total_count`; the
    /// fallback: the lexical total).
    pub total_count: u64,
    /// The counts are exact (only the fallback without a vector set).
    pub counts_are_exact: bool,
    pub requested_mode: RetrievalMode,
    /// The mode that ran (a hybrid search whose semantic side failed or was
    /// skipped ran lexical only).
    pub executed_mode: RetrievalMode,
    /// The semantic side answered.
    pub semantic_available: bool,
    /// Why the semantic side did not answer, and the session's notes (stale
    /// results dropped, window capped), joined by `"; "`.
    pub fallback_reason: Option<String>,
    /// `"not_configured"` (no vector set open: the lexical fallback) or
    /// `"query_failed"` (the semantic side failed); None with no failure.
    pub fallback_kind: Option<&'static str>,
    /// A window was capped at [`MAX_HYBRID_WINDOW`].
    pub candidate_window_truncated: bool,
    /// Semantic results dropped because their line is not in the index
    /// (Otzaria's `stale_primaries_dropped`).
    pub stale_dropped: u32,
}

/// One page of a hybrid session.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HybridPage {
    pub hits: Vec<FusedHit>,
    /// True when another page follows (Otzaria asks one past the page).
    pub more: bool,
    /// The window fused last (Otzaria's candidate window; 0 in the fallback).
    pub window: u32,
    /// True when no larger window can add a result.
    pub exhausted: bool,
    #[serde(flatten)]
    pub meta: HybridMeta,
}

/// One fused window ([`HybridSession::window`]): every result of the
/// window in fused order (up to `window`, stale lines left out), for a host
/// that groups results itself (Otzaria's grouping counts the window in
/// groups: the host grows the window and admits groups as Otzaria's
/// session does).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct HybridWindow {
    pub hits: Vec<FusedHit>,
    /// The window fused (capped at [`MAX_HYBRID_WINDOW`]).
    pub window: u32,
    /// The lexical side returned less than the window (or was not asked).
    pub lexical_done: bool,
    /// The semantic side returned less than its top_k (or did not run).
    pub semantic_done: bool,
    /// Candidates the fusion produced before the cut to `window`.
    pub fused_count: u64,
    #[serde(flatten)]
    pub meta: HybridMeta,
}

/// A step of [`HybridSession::page`]: the page, or (an index without
/// positions, a query of several words) the line texts the lexical side
/// needs first ([`HybridSession::add_texts`], then ask again).
#[derive(Debug, Clone, PartialEq)]
pub enum HybridStep {
    Page(HybridPage),
    NeedTexts(Vec<u64>),
}

/// A step of [`HybridSession::window`] (see [`HybridStep`]).
#[derive(Debug, Clone, PartialEq)]
pub enum HybridWindowStep {
    Window(HybridWindow),
    NeedTexts(Vec<u64>),
}

/// The fuzzy lexical side: Otzaria's smart fuzzy query run once as the
/// engine's search in relevance order (its score is Otzaria's fuzzy-mode
/// score, [`crate::relevance`]), ranked once the texts it needs are in.
struct FuzzyLexical {
    result: Option<crate::search::SearchResult>,
    ranked: Option<Vec<crate::relevance::LexicalCandidate>>,
    truncated: bool,
}

/// Otzaria's hybrid search session (`search_semantic` with a session and
/// `expand_semantic_session`, api/search_engine.rs, otzaria_search_engine
/// 7.2.0): results come in pages; to serve a page the session fuses a
/// candidate window of both sides, the window growing as Otzaria grows it
/// (`max(2 * (offset + limit) + 1, 2 * window)`, at most
/// [`MAX_HYBRID_WINDOW`]) until it holds one result past the page or nothing
/// more can come. Each window: the lexical side's best `window` lines (exact:
/// [`crate::relevance`] `hybrid_lexical_window`, Otzaria's smart exact query,
/// BM25; fuzzy: Otzaria's smart fuzzy query; the book filter; vocalized with
/// match_nikud / match_teamim) and the semantic side's best vectors (the
/// exact scan, Otzaria's `top_k`), fused as the coordinator fuses them for
/// the retrieval mode (hybrid, semantic only: the lexical candidates
/// discarded and the lexical side counted once; lexical only: no scan) and
/// cut to `window`; the results the session does not hold yet are appended
/// in fused order (earlier results keep their place), a semantic line no
/// longer in the index dropped and counted. The window is exhausted when
/// both sides returned less than asked and the fusion fit the window, or at
/// the maximum window. No vector set open: Otzaria's lexical fallback
/// (`semantic_lexical_fallback_response`).
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
    texts: std::collections::HashMap<u64, String>,
    /// The query as Otzaria's coordinator reads it: `smart_query::canonical`, normalized.
    features: fusion::QueryFeatures,
    alpha: f32,
    /// The lexical mode that runs (a whole-query quotation is exact).
    lexical_mode: LexicalMode,
    vocalized: Option<crate::relevance::Vocalized>,
    fuzzy: Option<FuzzyLexical>,
    /// The session's state (Otzaria's `SessionState`, absorbed per window).
    meta: HybridMeta,
    /// The semantic side's failure of the last window.
    failure: Option<String>,
    capped_request: Option<u64>,
    lexical_counted: bool,
    /// The fallback's ranked lexical lines (no vector set), once computed.
    fallback: Option<(Vec<crate::relevance::LexicalCandidate>, u64, bool)>,
}

/// otzaria_search_engine `smart_query::canonical`: each placeholder spelling
/// of the divine name in the query replaced by its full spelling (the copy
/// the semantic side scores; the lexical query keeps the input).
pub fn canonical_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let mut last = 0;
    hebrew_fts::scanner::scan_with(query, true, |_, span| {
        if let Some(alt) = crate::relevance::placeholder_alternative(&query[span.byte_start..span.byte_end]) {
            out.push_str(&query[last..span.byte_start]);
            out.push_str(&alt);
            last = span.byte_end;
        }
    });
    out.push_str(&query[last..]);
    out
}

/// The index tokenizer's words of `text` (Otzaria's `index_token_texts`).
fn token_words(text: &str, keep_marks: bool) -> Vec<String> {
    let mut words = Vec::new();
    hebrew_fts::scanner::scan_with(text, keep_marks, |w, span| {
        if span.term_bytes < hebrew_fts::chars::MAX_TERM_BYTES && !w.is_empty() {
            words.push(w.to_string());
        }
    });
    words
}

/// otzaria_search_engine `whole_query_quoted`: one quotation (the
/// coordinator's `quoted_phrases`) whose words are the whole query's.
pub fn whole_query_quoted(query: &str) -> bool {
    let phrases = fusion::quoted_phrases(query);
    if phrases.len() != 1 {
        return false;
    }
    let words = token_words(query, false);
    !words.is_empty() && words == token_words(&phrases[0], false)
}

const NOT_CONFIGURED: &str = "semantic sidecar has not been configured";

impl LineIndex {
    /// Opens Otzaria's hybrid search session for `query` (the lexical
    /// string) and `vector` (its embedding by the host's query model). No
    /// open vector set: Otzaria's lexical fallback. An empty vector with a
    /// set open: the semantic side fails (Otzaria's failed query), the
    /// search degrades as Otzaria's does.
    pub fn hybrid_session(&self, query: &str, vector: &[f32], options: HybridOptions) -> Result<HybridSession> {
        options.ranking.validate().map_err(Error::Unsupported)?;
        let set = self.vector_set()?;
        let vector = if vector.is_empty() || (vector.iter().map(|x| f64::from(*x) * f64::from(*x)).sum::<f64>().sqrt() - 1.0).abs() <= 1e-4 {
            vector.to_vec()
        } else {
            seforim_vectors::normalized(vector).map_err(map_err)?
        };
        let vocalized = if options.match_nikud || options.match_teamim {
            Some(crate::relevance::Vocalized {
                match_nikud: options.match_nikud,
                match_teamim: options.match_teamim,
                marked: self.marked_field()?,
            })
        } else {
            None
        };
        // only a quotation covering the whole query is one verbatim lookup
        let lexical_mode = if whole_query_quoted(query) { LexicalMode::Exact } else { options.lexical_mode };
        let fuzzy = match lexical_mode {
            LexicalMode::Fuzzy => Some(self.fuzzy_lexical(query, &options)?),
            LexicalMode::Exact => None,
        };
        let features = fusion::analyze_query(&fusion::normalize_query(&canonical_query(query)));
        let alpha = fusion::requested_alpha(&features, &options.ranking);
        let meta = HybridMeta { requested_mode: options.retrieval, ..Default::default() };
        Ok(HybridSession {
            features,
            alpha,
            // the lexical side reads the attached user index too (Otzaria's one
            // index holds the user documents)
            source: self.hybrid_source(self.user_index().as_deref()),
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
            texts: Default::default(),
            lexical_mode,
            vocalized,
            fuzzy,
            meta,
            failure: None,
            capped_request: None,
            lexical_counted: false,
            fallback: None,
        })
    }

    /// Otzaria's smart fuzzy query (`build_smart_fuzzy_query`) as the
    /// engine's search: every word of the query (the index tokenizer's,
    /// quoted ones included) a fuzzy word within the distance (a placeholder
    /// spelling of the divine name also its full spelling, Otzaria's
    /// `build_smart_fuzzy_alternatives`), each quoted phrase also literal and
    /// adjacent, the book filter, match_nikud / match_teamim; relevance
    /// order (Otzaria's fuzzy-mode score).
    fn fuzzy_lexical(&self, query: &str, options: &HybridOptions) -> Result<FuzzyLexical> {
        let d = options.fuzzy_max_distance;
        if d > 2 {
            return Err(Error::Unsupported(format!("fuzzy distance is limited to 0..=2, got {d}")));
        }
        let keep_marks = options.match_nikud || options.match_teamim;
        let words = token_words(query, keep_marks);
        if words.is_empty() {
            return Ok(FuzzyLexical { result: None, ranked: Some(Vec::new()), truncated: false });
        }
        let mut q = String::new();
        let mut overrides = std::collections::BTreeMap::new();
        for (i, w) in words.iter().enumerate() {
            if i > 0 {
                q.push(' ');
            }
            // distance 0 has no `~N` syntax: the word option makes the word
            // (and its alternative) a fuzzy word at distance 0, as Otzaria's
            // distance-0 automaton (the word, quotes zero-width, or its
            // quote-free form)
            let zero = d == 0;
            let typed = |w: &str| if zero { w.to_string() } else { format!("{w}~{d}") };
            q.push_str(&typed(w));
            let alternatives = crate::relevance::placeholder_alternative(w).map(|alt| vec![typed(&alt)]).unwrap_or_default();
            if zero || !alternatives.is_empty() {
                overrides.insert(
                    i,
                    crate::WordOptions { alternatives, fuzzy_distance: zero.then_some(0), ..Default::default() },
                );
            }
        }
        for phrase in fusion::quoted_phrases(query) {
            let literal = token_words(&phrase, keep_marks);
            if !literal.is_empty() {
                q.push_str(&format!(" \"{}\"", literal.join(" ")));
            }
        }
        let search = crate::SearchOptions {
            query: q,
            words: overrides,
            match_nikud: options.match_nikud,
            match_teamim: options.match_teamim,
            books: options.books.clone(),
            order: crate::ResultsOrder::Relevance,
            ..Default::default()
        };
        // an index without positions: the adjacency boost of several words
        // is decided from the line texts (rank_fuzzy asks for them)
        let mut request = search.to_request(self.options.keep_marks)?;
        request.fuzzy_phrase_from_texts = true;
        let result = self.search(&request)?;
        if result.scores().is_none() && !result.hits.is_empty() {
            return Err(Error::Unsupported(
                "the fuzzy lexical side of this query is ranked from positions: this index has none".into(),
            ));
        }
        // above distance 0 the exact tier's BM25 reads term frequencies, which
        // an index without positions does not keep
        if d > 0 && result.text_phrase().is_some() {
            return Err(Error::Unsupported(
                "the fuzzy lexical side of several words above distance 0 is ranked from term frequencies: this index has none".into(),
            ));
        }
        let truncated = result.truncated();
        Ok(FuzzyLexical { result: Some(result), ranked: None, truncated })
    }
}

/// What one window of the session found (Otzaria's `Expanded` + the
/// fusion's results before admission).
struct RawWindow {
    hits: Vec<FusedHit>,
    stale: std::collections::HashSet<u64>,
    lexical_done: bool,
    semantic_done: bool,
    fused_count: u64,
    exhausted: bool,
}

impl HybridSession {
    /// Line texts the lexical side asked for ([`HybridStep::NeedTexts`]).
    pub fn add_texts(&mut self, texts: impl IntoIterator<Item = (u64, String)>) {
        self.texts.extend(texts);
    }

    /// The results `offset .. offset + limit` (Otzaria's page), growing the
    /// window as needed.
    pub fn page(&mut self, offset: u32, limit: u32, cancel: Option<&CancelToken>) -> Result<HybridStep> {
        if self.set.is_none() {
            return self.fallback_page(offset, limit, cancel);
        }
        let wanted = offset as usize + limit as usize + 1;
        while self.entries.len() < wanted && !self.exhausted {
            if cancel.is_some_and(|c| c.is_cancelled()) {
                return Err(Error::Cancelled);
            }
            let requested = (u64::from(offset) + u64::from(limit)).saturating_mul(2).saturating_add(1).max(u64::from(self.window).saturating_mul(2));
            let window = requested.min(u64::from(MAX_HYBRID_WINDOW)) as u32;
            let raw = match self.expand(window, cancel)? {
                Ok(raw) => raw,
                Err(need) => return Ok(HybridStep::NeedTexts(need)),
            };
            if requested > u64::from(MAX_HYBRID_WINDOW) {
                self.capped_request = Some(requested);
            }
            for f in raw.hits {
                if !self.held.insert(f.line_id) {
                    continue;
                }
                // stale: held, so it is counted once, never shown
                if raw.stale.contains(&f.line_id) {
                    self.meta.stale_dropped = self.meta.stale_dropped.saturating_add(1);
                    continue;
                }
                self.entries.push(f);
            }
            self.window = window;
            self.exhausted = raw.exhausted;
        }
        let start = (offset as usize).min(self.entries.len());
        let end = (start + limit as usize).min(self.entries.len());
        Ok(HybridStep::Page(HybridPage {
            hits: self.entries[start..end].to_vec(),
            more: self.entries.len() > end,
            window: self.window,
            exhausted: self.exhausted,
            meta: self.output_meta(),
        }))
    }

    /// One fused window of `window` candidates (capped at
    /// [`MAX_HYBRID_WINDOW`]), independent of the pages: for a host that
    /// groups the results itself (Otzaria's grouping counts its window in
    /// groups; see [`HybridWindow`]). Without a vector set: the fallback's
    /// ranked lexical lines (fused = 1 / rank). It updates the session's
    /// state (counts, mode, the cap note) as a page's window does: use either
    /// `window` or `page` on one session, not both. Its `stale_dropped` is
    /// this window's count (a page's is the session's).
    pub fn window(&mut self, window: u32, cancel: Option<&CancelToken>) -> Result<HybridWindowStep> {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        let asked = window;
        let capped = window > MAX_HYBRID_WINDOW;
        let window = window.min(MAX_HYBRID_WINDOW);
        if self.set.is_none() {
            let (all, total, truncated) = match self.fallback_ranked(cancel)? {
                Ok(r) => r,
                Err(need) => return Ok(HybridWindowStep::NeedTexts(need)),
            };
            let hits: Vec<FusedHit> =
                all.iter().take(window as usize).enumerate().map(|(rank, c)| fallback_hit(c, rank)).collect();
            let meta = self.fallback_meta(total, truncated);
            let done = self.options.retrieval == RetrievalMode::SemanticOnly || hits.len() < window as usize;
            let hits = if self.options.retrieval == RetrievalMode::SemanticOnly { Vec::new() } else { hits };
            return Ok(HybridWindowStep::Window(HybridWindow {
                fused_count: hits.len() as u64,
                hits,
                window,
                lexical_done: done,
                semantic_done: true,
                meta,
            }));
        }
        let raw = match self.expand(window, cancel)? {
            Ok(raw) => raw,
            Err(need) => return Ok(HybridWindowStep::NeedTexts(need)),
        };
        if capped {
            self.capped_request = Some(u64::from(asked));
        }
        let stale = raw.hits.iter().filter(|f| raw.stale.contains(&f.line_id)).count() as u32;
        let hits: Vec<FusedHit> = raw.hits.into_iter().filter(|f| !raw.stale.contains(&f.line_id)).collect();
        let mut meta = self.output_meta();
        meta.stale_dropped = stale;
        meta.fallback_reason = self.reason_with_notes(stale);
        Ok(HybridWindowStep::Window(HybridWindow {
            hits,
            window,
            lexical_done: raw.lexical_done,
            semantic_done: raw.semantic_done,
            fused_count: raw.fused_count,
            meta,
        }))
    }

    /// The session's state as Otzaria's response reports it.
    fn output_meta(&self) -> HybridMeta {
        HybridMeta {
            fallback_reason: self.reason_with_notes(self.meta.stale_dropped),
            fallback_kind: self.failure.as_ref().map(|_| "query_failed"),
            candidate_window_truncated: self.capped_request.is_some(),
            counts_are_exact: false,
            ..self.meta.clone()
        }
    }

    /// Otzaria's `fallback_reason`: the semantic side's failure, then the
    /// session's notes, joined by `"; "`.
    fn reason_with_notes(&self, stale: u32) -> Option<String> {
        let mut reason = self.failure.clone();
        let mut note = |text: String| {
            reason = Some(match reason.take() {
                Some(r) => format!("{r}; {text}"),
                None => text,
            });
        };
        if stale > 0 {
            note(format!(
                "dropped {stale} stale semantic result(s) missing from Tantivy; candidate counts may still include stale records; rebuild or reconcile the semantic index"
            ));
        }
        if let Some(requested) = self.capped_request {
            note(format!("semantic candidate window capped at {MAX_HYBRID_WINDOW} (requested {requested})"));
        }
        reason
    }

    /// Every lexical line, ranked (exact: BM25; fuzzy: Otzaria's fuzzy
    /// score), its total and truncated flag.
    fn lexical_all(&mut self, cancel: Option<&CancelToken>) -> Result<std::result::Result<(Vec<crate::relevance::LexicalCandidate>, u64, bool), Vec<u64>>> {
        if cancel.is_some_and(|c| c.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        match self.lexical_window(usize::MAX)? {
            Ok(r) => Ok(Ok(r)),
            Err(need) => Ok(Err(need)),
        }
    }

    /// The best `window` lexical lines, the total and the truncated flag.
    fn lexical_window(&mut self, window: usize) -> Result<std::result::Result<(Vec<crate::relevance::LexicalCandidate>, u64, bool), Vec<u64>>> {
        match self.lexical_mode {
            LexicalMode::Exact => {
                match crate::relevance::hybrid_lexical_window(
                    &self.source,
                    self.book_field,
                    self.position_field,
                    &self.query,
                    self.options.books.as_deref(),
                    window,
                    &self.texts,
                    self.vocalized,
                )? {
                    Ok((c, total)) => Ok(Ok((c, total, false))),
                    Err(crate::relevance::NeedTexts(ids)) => Ok(Err(ids)),
                }
            }
            LexicalMode::Fuzzy => {
                if let Err(need) = self.rank_fuzzy()? {
                    return Ok(Err(need));
                }
                let f = self.fuzzy.as_ref().expect("fuzzy mode");
                let ranked = f.ranked.as_ref().expect("ranked");
                Ok(Ok((ranked.iter().take(window).copied().collect(), ranked.len() as u64, f.truncated)))
            }
        }
    }

    /// Ranks the fuzzy search's hits once: the lines that pass (an index
    /// without positions decides a phrase from the texts: asked for first),
    /// by score (higher first), ties in catalog order (TopDocs).
    fn rank_fuzzy(&mut self) -> Result<std::result::Result<(), Vec<u64>>> {
        let source = &self.source;
        let texts = &self.texts;
        let f = self.fuzzy.as_mut().expect("fuzzy mode");
        if f.ranked.is_some() {
            return Ok(Ok(()));
        }
        let Some(result) = f.result.as_mut() else {
            f.ranked = Some(Vec::new());
            return Ok(Ok(()));
        };
        let mut scores: Vec<f32> = result.scores().map(|s| s.to_vec()).unwrap_or_else(|| vec![0.0; result.hits.len()]);
        let mut pass = vec![true; result.hits.len()];
        // an index without positions: Otzaria's adjacency boost from the texts
        if let Some(words) = result.text_phrase().map(|w| w.to_vec()) {
            let mut missing: Vec<u64> = result.hits.iter().map(|h| h.line_id).filter(|id| !texts.contains_key(id)).collect();
            if !missing.is_empty() {
                missing.sort_unstable();
                missing.dedup();
                return Ok(Err(missing));
            }
            for (h, score) in result.hits.iter().zip(scores.iter_mut()) {
                if crate::relevance::text_holds_phrase(
                    &texts[&h.line_id],
                    &words,
                    crate::relevance::FUZZY_PHRASE_SLOP,
                    source.marked_terms,
                ) {
                    *score += crate::relevance::FUZZY_BOOST_PHRASE;
                }
            }
        }
        if result.count_needs_text() {
            let mut missing: Vec<u64> = result.hits.iter().map(|h| h.line_id).filter(|id| !texts.contains_key(id)).collect();
            if !missing.is_empty() {
                missing.sort_unstable();
                missing.dedup();
                return Ok(Err(missing));
            }
            let hits = result.hits.clone();
            for (i, h) in hits.iter().enumerate() {
                pass[i] = result.session.locate(&texts[&h.line_id]).is_some();
            }
        }
        let ids: Vec<u64> = result.hits.iter().zip(&pass).filter(|(_, &p)| p).map(|(h, _)| h.line_id).collect();
        let sections = crate::relevance::sections_of(source, &ids)?;
        let mut ranked: Vec<crate::relevance::LexicalCandidate> = result
            .hits
            .iter()
            .zip(&scores)
            .zip(&pass)
            .filter(|(_, &p)| p)
            .map(|((h, &s), _)| crate::relevance::LexicalCandidate {
                line_id: h.line_id,
                book_id: h.book_id,
                position: h.position,
                section_id: sections.get(&h.line_id).copied().unwrap_or(0),
                bm25: s,
            })
            .collect();
        // TopDocs order_by_score: the higher total first, ties in doc (catalog) order
        ranked.sort_by(|a, b| b.bm25.total_cmp(&a.bm25).then(a.position.cmp(&b.position)));
        f.ranked = Some(ranked);
        // the search is no longer needed
        f.result = None;
        Ok(Ok(()))
    }

    /// The fallback's ranked lexical lines (computed once).
    fn fallback_ranked(&mut self, cancel: Option<&CancelToken>) -> Result<std::result::Result<(Vec<crate::relevance::LexicalCandidate>, u64, bool), Vec<u64>>> {
        if self.fallback.is_none() {
            match self.lexical_all(cancel)? {
                Ok(r) => self.fallback = Some(r),
                Err(need) => return Ok(Err(need)),
            }
        }
        Ok(Ok(self.fallback.clone().expect("just set")))
    }

    fn fallback_meta(&self, total: u64, truncated: bool) -> HybridMeta {
        let semantic_only = self.options.retrieval == RetrievalMode::SemanticOnly;
        HybridMeta {
            lexical_total: total,
            lexical_truncated: truncated,
            total_count: if semantic_only { 0 } else { total },
            counts_are_exact: semantic_only || !truncated,
            requested_mode: self.options.retrieval,
            executed_mode: if semantic_only { RetrievalMode::SemanticOnly } else { RetrievalMode::LexicalOnly },
            semantic_available: false,
            fallback_reason: Some(NOT_CONFIGURED.to_string()),
            fallback_kind: Some("not_configured"),
            candidate_window_truncated: false,
            stale_dropped: 0,
        }
    }

    /// otzaria_search_engine `semantic_lexical_fallback_response`: no vector
    /// set. Semantic only: no result, the lexical count. Otherwise the
    /// lexical search's page (its relevance order), each result's fused
    /// score 1 / (its rank on the page + 1), no lexical / semantic score.
    fn fallback_page(&mut self, offset: u32, limit: u32, cancel: Option<&CancelToken>) -> Result<HybridStep> {
        let (all, total, truncated) = match self.fallback_ranked(cancel)? {
            Ok(r) => r,
            Err(need) => return Ok(HybridStep::NeedTexts(need)),
        };
        let meta = self.fallback_meta(total, truncated);
        if self.options.retrieval == RetrievalMode::SemanticOnly {
            return Ok(HybridStep::Page(HybridPage { hits: Vec::new(), more: false, window: 0, exhausted: true, meta }));
        }
        let start = (offset as usize).min(all.len());
        let end = (start + limit as usize).min(all.len());
        let hits: Vec<FusedHit> = all[start..end].iter().enumerate().map(|(rank, c)| fallback_hit(c, rank)).collect();
        let more = (offset as u64) + (hits.len() as u64) < total;
        Ok(HybridStep::Page(HybridPage { hits, more, window: 0, exhausted: true, meta }))
    }

    /// One window fused (otzaria_search_engine `expand_semantic_session` +
    /// otzaria-semantic-search `HybridCoordinator::prepare_with` / `fuse`, with
    /// `limit = window`, offset 0); Err = texts needed.
    fn expand(&mut self, window: u32, cancel: Option<&CancelToken>) -> Result<std::result::Result<RawWindow, Vec<u64>>> {
        let profile = self.options.ranking.clone();
        let requested = self.options.retrieval;
        let books = self.options.books.clone();
        let books = books.as_deref();
        // the lexical phase: semantic only discards the candidates and counts once a session
        let (candidates, lexical) = if requested == RetrievalMode::SemanticOnly {
            if self.lexical_counted {
                (Vec::new(), None)
            } else {
                match self.lexical_window(1)? {
                    Ok((_, total, truncated)) => (Vec::new(), Some((total, truncated))),
                    Err(need) => return Ok(Err(need)),
                }
            }
        } else {
            match self.lexical_window(window as usize)? {
                Ok((c, total, truncated)) => (c, Some((total, truncated))),
                Err(need) => return Ok(Err(need)),
            }
        };
        let lexical_done = requested == RetrievalMode::SemanticOnly || candidates.len() < window as usize;
        // prepare_with: the semantic half (skipped for lexical only and for an exact query: alpha >= 1)
        let top_k = fusion::semantic_top_k(window as usize, 0, &profile);
        let foundational: std::collections::HashSet<u64> = self.options.foundational_books.iter().flatten().copied().collect();
        let mut semantic: Vec<SemanticHit> = Vec::new();
        let mut healthy = false;
        let mut failure: Option<String> = None;
        let mut semantic_hits = 0usize;
        let skip_for_exact = requested == RetrievalMode::Hybrid && self.alpha >= 1.0;
        if requested != RetrievalMode::LexicalOnly && !skip_for_exact {
            if let Some((set, map)) = &self.set {
                if self.vector.is_empty() {
                    failure = Some("no query vector was given".to_string());
                } else {
                    let flag = cancel.map(|c| c.flag());
                    let opts = SemanticOptions {
                        k: top_k,
                        books: self.options.books.clone(),
                        threads: self.options.threads,
                        window_penalty: profile.window_penalty,
                        alias_penalty: profile.alias_penalty,
                    };
                    match set.search_lines_all(&self.vector, map, &opts, flag.as_deref()) {
                        Err(seforim_vectors::Error::Cancelled) => return Err(Error::Cancelled),
                        // degraded, not failed: the lexical results are served
                        Err(e) => failure = Some(e.to_string()),
                        Ok(r) => {
                            semantic_hits = r.vectors as usize;
                            semantic = r.hits.into_iter().map(|h| SemanticHit { line_id: h.line_id, book_id: h.book_id, score: h.score, chunk: h.chunk }).collect();
                            healthy = true;
                            // append_foundational: a share of top_k over the foundational books, the lines main lacks
                            let share = profile.foundational_candidate_share;
                            let user_foundational = books.is_some_and(|b| !b.is_empty() && b.iter().all(|x| foundational.contains(x)));
                            if !share.is_nan() && share > 0.0 && !foundational.is_empty() && !user_foundational {
                                let base_k = (top_k as f64 * f64::from(share.min(1.0))).ceil() as usize;
                                let base_books: Vec<u64> = match books {
                                    Some(b) => b.iter().copied().filter(|x| foundational.contains(x)).collect(),
                                    None => foundational.iter().copied().collect(),
                                };
                                let opts = SemanticOptions { k: base_k, books: Some(base_books), ..opts };
                                let extras = set.search_lines_all(&self.vector, map, &opts, flag.as_deref()).map_err(map_err)?;
                                let mut seen: std::collections::HashSet<u64> = semantic.iter().map(|h| h.line_id).collect();
                                for h in extras.hits {
                                    if seen.insert(h.line_id) {
                                        semantic.push(SemanticHit { line_id: h.line_id, book_id: h.book_id, score: h.score, chunk: h.chunk });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let semantic_done = !healthy || semantic_hits < top_k;
        // what ranking reads beyond the scores: sections, and (only for the bonuses / penalty that
        // read them, weighted fusion) the line texts
        let weighted = !matches!(profile.fusion_strategy, fusion::FusionStrategy::Rrf { .. });
        let want_text = weighted && (profile.phrase_match_bonus > 0.0 || profile.rare_term_bonus > 0.0 || profile.duplicate_penalty > 0.0);
        if want_text {
            let mut need: Vec<u64> = candidates.iter().map(|c| c.line_id).collect();
            if profile.duplicate_penalty > 0.0 {
                need.extend(semantic.iter().map(|h| h.line_id));
            }
            need.retain(|id| !self.texts.contains_key(id));
            need.sort_unstable();
            need.dedup();
            if !need.is_empty() {
                return Ok(Err(need));
            }
        }
        // the semantic lines' sections; a line the index lacks is stale (Otzaria's hydration miss)
        let sections = crate::relevance::sections_of(&self.source, &semantic.iter().map(|h| h.line_id).collect::<Vec<_>>())?;
        let plain = |id: u64| self.texts.get(&id).map(|t| strip_tags(t)).unwrap_or_default();
        let hash = |id: u64| -> u64 {
            if profile.duplicate_penalty > 0.0 {
                line_hash(&plain(id))
            } else {
                0
            }
        };
        let section_bonus = weighted && profile.section_coverage_bonus > 0.0;
        // fuse: the mode that ran; semantic only discards the lexical candidates
        let mode = match requested {
            RetrievalMode::LexicalOnly => RetrievalMode::LexicalOnly,
            RetrievalMode::SemanticOnly => RetrievalMode::SemanticOnly,
            RetrievalMode::Hybrid if healthy => RetrievalMode::Hybrid,
            RetrievalMode::Hybrid => RetrievalMode::LexicalOnly,
        };
        let (fusion_mode, alpha) = match mode {
            RetrievalMode::LexicalOnly => (fusion::Mode::LexicalOnly, 1.0),
            RetrievalMode::SemanticOnly => (fusion::Mode::SemanticOnly, 0.0),
            RetrievalMode::Hybrid => (fusion::Mode::Hybrid, self.alpha),
        };
        let lex: Vec<fusion::Lexical> = if mode == RetrievalMode::SemanticOnly {
            Vec::new()
        } else {
            candidates
                .iter()
                .map(|c| fusion::Lexical {
                    line_id: c.line_id,
                    book_id: c.book_id,
                    bm25: c.bm25,
                    section_id: c.section_id,
                    line_hash: hash(c.line_id),
                    text: if want_text { plain(c.line_id) } else { String::new() },
                    foundational: foundational.contains(&c.book_id),
                })
                .collect()
        };
        let sem: Vec<fusion::Semantic> = semantic
            .iter()
            .map(|h| fusion::Semantic {
                line_id: h.line_id,
                book_id: h.book_id,
                score: h.score,
                section_id: if section_bonus { sections.get(&h.line_id).copied().unwrap_or(0) } else { 0 },
                line_hash: hash(h.line_id),
                foundational: foundational.contains(&h.book_id),
            })
            .collect();
        let mut fused = fusion::fuse_candidates(lex, sem, alpha, fusion_mode, &profile, &self.features);
        let fused_count = fused.len();
        fused.truncate(window as usize);
        let exhausted = (lexical_done && semantic_done && fused_count <= window as usize) || window >= MAX_HYBRID_WINDOW;
        let places: std::collections::HashMap<u64, (u64, u64)> =
            candidates.iter().map(|c| (c.line_id, (c.position, c.section_id))).collect();
        let chunks: std::collections::HashMap<u64, u32> = semantic.iter().rev().map(|h| (h.line_id, h.chunk)).collect();
        let mut stale = std::collections::HashSet::new();
        let hits = fused
            .into_iter()
            .map(|f| {
                let lexical_place = if mode == RetrievalMode::SemanticOnly { None } else { places.get(&f.line_id) };
                if lexical_place.is_none() && !sections.contains_key(&f.line_id) {
                    stale.insert(f.line_id);
                }
                FusedHit {
                    line_id: f.line_id,
                    book_id: f.book_id,
                    position: lexical_place.map(|p| p.0),
                    fused: f.fused,
                    lexical: f.lexical,
                    semantic: f.semantic,
                    source: match f.source {
                        fusion::Source::Lexical => "lexical",
                        fusion::Source::Semantic => "semantic",
                        fusion::Source::Both => "both",
                    },
                    matched_window: chunks.get(&f.line_id).copied().unwrap_or(0),
                    section_id: lexical_place.map(|p| p.1).or_else(|| sections.get(&f.line_id).copied()).unwrap_or(0),
                }
            })
            .collect();
        // absorb (Otzaria's SessionState::absorb)
        if let Some((total, truncated)) = lexical {
            self.meta.lexical_total = total;
            self.meta.lexical_truncated = truncated;
            self.lexical_counted = true;
        }
        self.meta.total_count = fused_count as u64;
        self.meta.executed_mode = mode;
        self.meta.semantic_available = healthy;
        self.failure = failure;
        Ok(Ok(RawWindow { hits, stale, lexical_done, semantic_done, fused_count: fused_count as u64, exhausted }))
    }
}

/// A fallback result (`semantic_lexical_fallback_response`): fused score
/// 1 / (rank + 1), no lexical or semantic score.
fn fallback_hit(c: &crate::relevance::LexicalCandidate, rank: usize) -> FusedHit {
    FusedHit {
        line_id: c.line_id,
        book_id: c.book_id,
        position: Some(c.position),
        fused: 1.0 / (rank.saturating_add(1) as f32),
        lexical: None,
        semantic: None,
        source: "lexical",
        matched_window: 0,
        section_id: c.section_id,
    }
}

/// The line's plain text for the profile's text bonuses (Otzaria reads the
/// index's stored plain text): tags removed, whitespace collapsed.
fn strip_tags(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A line's text hash for the duplicate penalty (Otzaria's `line_hash`: equal
/// texts, equal hashes; 0 = none): FNV-1a of the plain text.
fn line_hash(plain: &str) -> u64 {
    if plain.is_empty() {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in plain.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h.max(1)
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
        let balanced = HybridOptions { ranking: RankingProfile::balanced(), ..Default::default() };
        let mut h = index.hybrid_session("alpha", &axis(3), balanced).unwrap();
        let p = page(&mut h, 0, 10, &[]);
        let mut got: Vec<u64> = p.hits.iter().map(|f| f.line_id).collect();
        got.sort_unstable();
        assert!(got.contains(&1) && got.contains(&4) && got.contains(&3), "{got:?}");
        let three = p.hits.iter().find(|f| f.line_id == 3).unwrap();
        assert_eq!((three.source, three.position, three.book_id), ("semantic", None, 20));
        let one = p.hits.iter().find(|f| f.line_id == 1).unwrap();
        let both = if one.semantic.is_some() { 0.10 } else { 0.0 };
        let want = 0.7 * fusion::normalize_bm25(one.lexical.unwrap(), 10.0) + 0.3 * one.semantic.map_or(0.0, fusion::normalize_cosine) + both;
        assert!((one.fused - want).abs() < 1e-6);
        // Otzaria's defaults (RRF k 60, threshold 0.55): the lexical lines 1 and 4 and line 3
        // (cosine 1); the other semantic lines (cosine 0 -> 0.5 < 0.55) left out
        let p = page(&mut index.hybrid_session("alpha", &axis(3), HybridOptions::default()).unwrap(), 0, 10, &[]);
        assert_eq!(p.hits.iter().filter(|f| f.lexical.is_some()).count(), 2);
        let three = p.hits.iter().find(|f| f.line_id == 3).unwrap();
        assert!((three.fused - 1.0 / 61.0).abs() < 1e-7, "{three:?}");
        // the foundational books (book 20): its line gets the profile's 0.002, and the
        // foundational query (share 0.5 of top_k) adds its lines the main query lacked
        let f = HybridOptions { foundational_books: Some(vec![20]), ..Default::default() };
        let p = page(&mut index.hybrid_session("alpha", &axis(3), f).unwrap(), 0, 10, &[]);
        let three = p.hits.iter().find(|f| f.line_id == 3).unwrap();
        assert!((three.fused - (1.0 / 61.0 + 0.002)).abs() < 1e-7, "{three:?}");
        assert!(p.exhausted, "5 vectors < top_k 42: the semantic side ran out");
        // an exact query (a quoted phrase: alpha 1) skips the semantic side: lexical only
        let p = page(&mut index.hybrid_session("\"alpha\"", &axis(3), HybridOptions::default()).unwrap(), 0, 10, &[]);
        assert!(p.hits.iter().all(|f| f.source == "lexical") && !p.hits.is_empty(), "{:?}", p.hits);

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
        // a vector set (never scanned by the lexical-only sessions below)
        let oxv = tmp.path().join("vec.oxv");
        seforim_vectors::oxv::write_test_segment(&oxv, 8, false, "v1", &[("id:2", vec![(0, 0, axis(1))])]).unwrap();
        index.open_vectors(&oxv).unwrap();
        index.set_vector_mapping(vec![OxvBookMap { name: "id:2".into(), book_id: 2, line_ids: vec![6] }], true);
        (tmp, index)
    }

    fn lexonly() -> HybridOptions {
        HybridOptions { retrieval: RetrievalMode::LexicalOnly, ..Default::default() }
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

        // one word: TermQuery without frequencies, best first; lexical only: alpha 1
        let want = tantivy_scores(&index, &TermQuery::new(term("alpha"), IndexRecordOption::Basic));
        let balanced = || HybridOptions { ranking: RankingProfile::balanced(), ..lexonly() };
        let mut s = index.hybrid_session("alpha", &[], balanced()).unwrap();
        let p = page(&mut s, 0, 10, &[]);
        assert_eq!((p.hits.len(), p.meta.lexical_total, p.more, p.window, p.exhausted), (4, 4, false, 21, true));
        let mut by_score: Vec<(u64, f32)> = want.iter().map(|(&k, &v)| (k, v)).collect();
        by_score.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        assert_eq!(p.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), by_score.iter().map(|x| x.0).collect::<Vec<_>>());
        for h in &p.hits {
            let bm25 = h.lexical.unwrap();
            assert!((bm25 - want[&h.line_id]).abs() < 1e-5);
            // the mode that ran is lexical only: alpha 1 (coordinator.rs `fuse`)
            assert!((h.fused - fusion::normalize_bm25(bm25, 10.0)).abs() < 1e-6);
            assert_eq!(h.source, "lexical");
        }

        // two words: the exact phrase's BM25 (PhraseQuery), lines 1, 3, 6
        let want = tantivy_scores(&index, &PhraseQuery::new(vec![term("alpha"), term("beta")]));
        assert_eq!(want.len(), 3);
        let mut s = index.hybrid_session("alpha beta", &[], lexonly()).unwrap();
        let all = page(&mut s, 0, 10, &[]);
        assert_eq!(all.hits.len(), 3);
        for h in &all.hits {
            assert!((h.lexical.unwrap() - want[&h.line_id]).abs() < 1e-5);
        }
        // pages: limit 1 asks a window of 3 (2 * (0 + 1) + 1), then 2 * 3
        let mut s = index.hybrid_session("alpha beta", &[], lexonly()).unwrap();
        let p0 = page(&mut s, 0, 1, &[]);
        assert_eq!((p0.window, p0.more, p0.hits[0].line_id), (3, true, all.hits[0].line_id));
        let p2 = page(&mut s, 2, 1, &[]);
        assert_eq!((p2.window, p2.more, p2.hits[0].line_id), (7, false, all.hits[2].line_id));
        // the book filter
        let mut s = index.hybrid_session("alpha beta", &[], HybridOptions { books: Some(vec![2]), ..lexonly() }).unwrap();
        let p = page(&mut s, 0, 10, &[]);
        assert_eq!(p.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![6]);
        assert_eq!(p.meta.lexical_total, 1);

        // an index without positions: the same phrase scores, from the texts
        let (_t2, flat) = build(false);
        let mut s = flat.hybrid_session("alpha beta", &[], lexonly()).unwrap();
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

    fn opts(retrieval: RetrievalMode) -> HybridOptions {
        HybridOptions { retrieval, ..Default::default() }
    }

    /// otzaria_search_engine `semantic_lexical_fallback_response`: no vector
    /// set open. Hybrid / lexical only: the lexical search's page, fused
    /// 1 / (rank on the page + 1), no lexical score, exact counts; semantic
    /// only: no result, the lexical count.
    #[test]
    fn hybrid_fallback_without_a_vector_set() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index_with_vectors(tmp.path());
        index.close_vectors();
        // "alpha": lines 1 ("alpha beta") and 4 ("alpha"), line 4 first (shorter: higher BM25)
        let mut s = index.hybrid_session("alpha", &axis(3), HybridOptions::default()).unwrap();
        let p0 = page(&mut s, 0, 1, &[]);
        assert_eq!(p0.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![4]);
        let h = p0.hits[0];
        assert_eq!((h.fused, h.lexical, h.semantic, h.source, h.position), (1.0, None, None, "lexical", Some(3)));
        assert!(p0.more);
        let m = &p0.meta;
        assert_eq!((m.lexical_total, m.total_count, m.counts_are_exact, m.semantic_available), (2, 2, true, false));
        assert_eq!((m.requested_mode, m.executed_mode), (RetrievalMode::Hybrid, RetrievalMode::LexicalOnly));
        assert_eq!((m.fallback_kind, m.fallback_reason.as_deref()), (Some("not_configured"), Some(NOT_CONFIGURED)));
        // the rank is the page's: the second page's first result is 1 again
        let p1 = page(&mut s, 1, 1, &[]);
        assert_eq!((p1.hits[0].line_id, p1.hits[0].fused, p1.more), (1, 1.0, false));
        // semantic only: nothing shown, the lexical side counted
        let p = page(&mut index.hybrid_session("alpha", &axis(3), opts(RetrievalMode::SemanticOnly)).unwrap(), 0, 10, &[]);
        assert!(p.hits.is_empty() && !p.more);
        assert_eq!((p.meta.lexical_total, p.meta.total_count, p.meta.executed_mode), (2, 0, RetrievalMode::SemanticOnly));
        assert_eq!(p.meta.fallback_kind, Some("not_configured"));
    }

    /// Otzaria's semantic search covers user documents (its one index holds
    /// them): the hybrid session's lexical side reads the attached user index
    /// with the library, and a book filter reaches user documents too.
    #[test]
    fn hybrid_covers_user_documents() {
        use crate::user_docs::{DocLine, USER_ID_BASE};
        let (tmp, tmp2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let index = index_with_vectors(tmp.path());
        index.close_vectors();
        let user = std::sync::Arc::new(LineIndex::create_user(tmp2.path(), IndexOptions::default()).unwrap());
        {
            let mut w = user.writer(50_000_000, 1).unwrap();
            let line = DocLine { text: "alpha gamma".into(), locator: 3, section_id: None, no_cross: false };
            w.put_document("c:/u.pdf", "s1", &[line]).unwrap();
            w.commit(None).unwrap();
        }
        index.attach_user_index(Some(user)).unwrap();
        let ids = |options: HybridOptions| -> Vec<u64> {
            let mut s = index.hybrid_session("alpha", &axis(3), options).unwrap();
            page(&mut s, 0, 10, &[]).hits.iter().map(|h| h.line_id).collect()
        };
        let all = ids(HybridOptions::default());
        assert!(all.contains(&4) && all.contains(&1), "{all:?}");
        assert!(all.contains(&USER_ID_BASE), "{all:?}");
        let only_user = ids(HybridOptions { books: Some(vec![USER_ID_BASE]), ..Default::default() });
        assert_eq!(only_user, vec![USER_ID_BASE]);
    }

    /// The retrieval modes (Otzaria's coordinator `fuse`): semantic only
    /// discards the lexical candidates (counted once), lexical only runs no
    /// scan; a failed semantic side degrades hybrid to lexical only.
    #[test]
    fn hybrid_retrieval_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index_with_vectors(tmp.path());
        // semantic only: every result the semantic side's, line 3 first, the lexical count kept
        let mut s = index.hybrid_session("alpha", &axis(3), opts(RetrievalMode::SemanticOnly)).unwrap();
        let p = page(&mut s, 0, 10, &[]);
        assert!(!p.hits.is_empty() && p.hits.iter().all(|h| h.source == "semantic" && h.lexical.is_none()), "{:?}", p.hits);
        assert_eq!(p.hits[0].line_id, 3);
        assert_eq!((p.meta.lexical_total, p.meta.executed_mode, p.meta.semantic_available), (2, RetrievalMode::SemanticOnly, true));
        assert_eq!(p.meta.fallback_reason, None);
        // lexical only: lines 1 and 4 alone, alpha 1, nothing semantic
        let p = page(&mut index.hybrid_session("alpha", &axis(3), opts(RetrievalMode::LexicalOnly)).unwrap(), 0, 10, &[]);
        assert_eq!(p.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![4, 1]);
        // Otzaria's default RRF (k 60) over the lexical ranks alone
        for (rank, h) in p.hits.iter().enumerate() {
            assert!(h.source == "lexical" && h.lexical.is_some() && (h.fused - 1.0 / (61.0 + rank as f32)).abs() < 1e-7, "{h:?}");
        }
        assert_eq!((p.meta.executed_mode, p.meta.semantic_available, p.meta.fallback_kind), (RetrievalMode::LexicalOnly, false, None));
        // no query vector with a set open: the semantic side failed, hybrid ran lexical only
        let p = page(&mut index.hybrid_session("alpha", &[], HybridOptions::default()).unwrap(), 0, 10, &[]);
        assert_eq!(p.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![4, 1]);
        assert_eq!((p.meta.executed_mode, p.meta.fallback_kind), (RetrievalMode::LexicalOnly, Some("query_failed")));
        assert_eq!(p.meta.fallback_reason.as_deref(), Some("no query vector was given"));
    }

    /// Each hit carries its semantic match's window (Otzaria's
    /// `matched_window`) and its section; a semantic line the index lacks is
    /// dropped and counted (Otzaria's stale primaries); a window past the
    /// maximum is capped and noted.
    #[test]
    fn hybrid_matched_window_stale_and_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index_with_vectors(tmp.path());
        // axis 5 is line 4's second window
        let p = page(&mut index.hybrid_session("zzz", &axis(5), HybridOptions::default()).unwrap(), 0, 10, &[]);
        let four = p.hits.iter().find(|h| h.line_id == 4).unwrap();
        assert_eq!((four.matched_window, four.source), (1, "semantic"));
        assert!(p.hits.iter().filter(|h| h.line_id != 4).all(|h| h.matched_window == 0));
        // the page JSON carries the new fields
        let j = serde_json::to_value(&p).unwrap();
        assert_eq!(j["executed_mode"], "hybrid");
        assert!(j["hits"][0].get("matched_window").is_some() && j.get("lexical_total").is_some());
        // book 20's ordinal 1 mapped to line 99, not in the index: stale
        index.set_vector_mapping(vec![OxvBookMap { name: "id:20".into(), book_id: 20, line_ids: vec![3, 99] }], false);
        let p = page(&mut index.hybrid_session("zzz", &axis(4), HybridOptions::default()).unwrap(), 0, 10, &[]);
        assert!(p.hits.iter().all(|h| h.line_id != 99));
        assert_eq!(p.meta.stale_dropped, 1);
        assert!(p.meta.fallback_reason.as_deref().unwrap().starts_with("dropped 1 stale semantic result(s)"));
        assert_eq!(p.meta.fallback_kind, None, "a note has no kind");
        // a page that asks past the maximum window: capped at 10000, noted
        let p = page(&mut index.hybrid_session("alpha", &axis(3), HybridOptions::default()).unwrap(), 6000, 10, &[]);
        assert!(p.hits.is_empty() && p.meta.candidate_window_truncated && p.window == MAX_HYBRID_WINDOW);
        assert!(p.meta.fallback_reason.as_deref().unwrap().contains("capped at 10000 (requested 12021)"));
        // window(): the fused window for a host that groups, stale lines left out
        let mut s = index.hybrid_session("alpha", &axis(4), HybridOptions::default()).unwrap();
        let HybridWindowStep::Window(w) = s.window(5, None).unwrap() else { panic!("texts") };
        assert!(w.hits.len() <= 5 && w.semantic_done && w.hits.iter().all(|h| h.line_id != 99));
        assert_eq!(w.meta.stale_dropped, 1);
        assert_eq!(w.hits.iter().find(|h| h.line_id == 1).map(|h| h.section_id), Some(0));
    }

    /// Otzaria's fuzzy lexical side (`semantic_fuzzy_lexical_candidates`):
    /// the smart fuzzy query's hits and scores (the engine's relevance order
    /// for `word~N`), a quoted phrase literal; a whole-query quotation is
    /// exact.
    #[test]
    fn hybrid_fuzzy_lexical_side() {
        let (_t, index) = build(true);
        let fuzzy = |d: u8| HybridOptions { lexical_mode: LexicalMode::Fuzzy, fuzzy_max_distance: d, ..lexonly() };
        // a swap of two letters is one edit: the alpha lines
        let p = page(&mut index.hybrid_session("alpah", &[], fuzzy(1)).unwrap(), 0, 10, &[]);
        let want = index
            .search_options(&crate::SearchOptions { query: "alpah~1".into(), order: crate::ResultsOrder::Relevance, ..Default::default() })
            .unwrap();
        let scores: std::collections::HashMap<u64, f32> =
            want.hits.iter().zip(want.scores().unwrap()).map(|(h, &s)| (h.line_id, s)).collect();
        let mut got: Vec<u64> = p.hits.iter().map(|h| h.line_id).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3, 6]);
        for h in &p.hits {
            assert_eq!(h.lexical, Some(scores[&h.line_id]));
        }
        assert!(p.hits.windows(2).all(|w| w[0].lexical >= w[1].lexical));
        assert_eq!(p.meta.lexical_total, 4);
        // a quoted word is also literal: "alphabet" is not
        let p = page(&mut index.hybrid_session("alpah \"beta\"", &[], fuzzy(1)).unwrap(), 0, 10, &[]);
        let mut got: Vec<u64> = p.hits.iter().map(|h| h.line_id).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 3, 6]);
        // the whole query quoted: the exact phrase (lines 1, 3, 6)
        let p = page(&mut index.hybrid_session("\"alpha beta\"", &[], fuzzy(2)).unwrap(), 0, 10, &[]);
        let mut got: Vec<u64> = p.hits.iter().map(|h| h.line_id).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 3, 6]);
        assert!(whole_query_quoted("\"alpha beta\"") && !whole_query_quoted("alpha \"beta\""));
        // semantic only counts the fuzzy side
        let p = page(&mut index.hybrid_session("alpah", &axis(1), HybridOptions { retrieval: RetrievalMode::SemanticOnly, ..fuzzy(1) }).unwrap(), 0, 10, &[]);
        assert_eq!(p.meta.lexical_total, 4);
        // a distance past 2 is refused (Otzaria's limit)
        assert!(index.hybrid_session("alpah", &[], fuzzy(3)).is_err());
        assert!(index.hybrid_session("alpah", &[], fuzzy(0)).is_ok());
    }

    /// Fuzzy distance 0 (Otzaria's smart search, `build_smart_fuzzy_query`
    /// at distance 0): each word exact (quotes zero-width) or its quote-free
    /// form, the fuzzy-mode score; several words on an index without
    /// positions decide the adjacency boost from the line texts, equal to
    /// the positions index.
    #[test]
    fn hybrid_fuzzy_distance_zero() {
        let fuzzy = |d: u8| HybridOptions { lexical_mode: LexicalMode::Fuzzy, fuzzy_max_distance: d, ..lexonly() };
        let (_t, index) = build(true);
        let ids = |p: &HybridPage| {
            let mut v: Vec<u64> = p.hits.iter().map(|h| h.line_id).collect();
            v.sort_unstable();
            v
        };
        // exact: no edit allowed ("alphabet" and a swap are not)
        let p = page(&mut index.hybrid_session("alpha", &[], fuzzy(0)).unwrap(), 0, 10, &[]);
        assert_eq!(ids(&p), vec![1, 2, 3, 6]);
        assert!(p.hits.iter().all(|h| h.lexical == Some(1.0)), "the distance-0 automaton's constant");
        let p = page(&mut index.hybrid_session("alpah", &[], fuzzy(0)).unwrap(), 0, 10, &[]);
        assert!(p.hits.is_empty());
        // two words: both required, the adjacent pair boosted
        let texts: Vec<(u64, &str)> = LINES.to_vec();
        let with_pos = page(&mut index.hybrid_session("alpha beta", &[], fuzzy(0)).unwrap(), 0, 10, &[]);
        assert_eq!(ids(&with_pos), vec![1, 2, 3, 6]);
        let line = |p: &HybridPage, id: u64| p.hits.iter().find(|h| h.line_id == id).unwrap().lexical.unwrap();
        assert!(line(&with_pos, 1) >= 2000.0, "adjacent: the boost");
        // an index without positions: the same lines and scores, from the texts
        let (_t2, plain) = build(false);
        assert!(plain.hybrid_session("alpha beta", &[], fuzzy(1)).is_err(), "above 0: term frequencies");
        let mut s = plain.hybrid_session("alpha beta", &[], fuzzy(0)).unwrap();
        assert!(matches!(s.page(0, 10, None).unwrap(), HybridStep::NeedTexts(_)), "texts first");
        let got = page(&mut s, 0, 10, &texts);
        let pairs = |p: &HybridPage| p.hits.iter().map(|h| (h.line_id, h.lexical)).collect::<Vec<_>>();
        assert_eq!(pairs(&got), pairs(&with_pos));
    }

    /// Fuzzy distance 0 on a word with an inner quote (otzaria_search_engine
    /// 7.2.0 `build_fuzzy_query_from_terms` at distance 0, its test
    /// `test_lexical_fuzzy_distance_zero_keeps_quote_free_spelling`): the
    /// quoted spelling and the quote-free one match, scored as Otzaria's
    /// token clause summed: the distance-0 automaton (constant 1, the quoted
    /// spelling only) and `quote_expansion::term_query` of the quote-free
    /// form (BM25 over its spellings, the quoted one included, document
    /// frequencies summed: `SpellingsTermQuery`).
    #[test]
    fn hybrid_fuzzy_distance_zero_inner_quote() {
        use tantivy::fieldnorm::FieldNormReader;
        use tantivy::query::{Bm25Weight, FuzzyTermQuery};
        use tantivy::Term;
        let lines: [(u64, &str); 4] =
            [(1, "dvri rmbm"), (2, "dvri rmb\"m"), (3, "dvri rmbn"), (4, "rmbm rmbm dvri sof")];
        let tmp = tempfile::tempdir().unwrap();
        let index = LineIndex::create(tmp.path(), IndexOptions::default()).unwrap();
        {
            let mut w = index.writer(50_000_000, 1).unwrap();
            for (i, &(id, text)) in lines.iter().enumerate() {
                let doc = LineDoc { line_id: id, position: i as u64, book_id: 1, text: text.into(), section_id: None, no_cross: false };
                w.add(&doc).unwrap();
            }
            w.commit(None).unwrap();
        }
        let oxv = tmp.path().join("vec.oxv");
        seforim_vectors::oxv::write_test_segment(&oxv, 8, false, "v1", &[("id:1", vec![(0, 0, axis(1))])]).unwrap();
        index.open_vectors(&oxv).unwrap();
        index.set_vector_mapping(vec![OxvBookMap { name: "id:1".into(), book_id: 1, line_ids: vec![1] }], true);

        let field = index.fields.text;
        let term = |w: &str| Term::from_field_text(field, w);
        // the automaton: the quoted spelling only
        let automaton = tantivy_scores(&index, &FuzzyTermQuery::new(term("rmb\"m"), 0, true));
        assert_eq!(automaton.keys().copied().collect::<Vec<_>>(), vec![2]);
        // the spellings' BM25: df 2 + 1 of 4 lines, 10 words (term frequency 1, `Basic`)
        let bm25 = Bm25Weight::for_one_term(3, 4, 2.5);
        let spelling = |words: u32| bm25.score(FieldNormReader::fieldnorm_to_id(words), 1);
        let want = std::collections::HashMap::from([(1u64, spelling(2)), (2, 1.0 + spelling(2)), (4, spelling(4))]);

        let o = HybridOptions { lexical_mode: LexicalMode::Fuzzy, fuzzy_max_distance: 0, ..lexonly() };
        let p = page(&mut index.hybrid_session("rmb\"m", &[], o).unwrap(), 0, 10, &[]);
        let mut got: Vec<u64> = p.hits.iter().map(|h| h.line_id).collect();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2, 4], "not the edit-distance neighbour");
        for h in &p.hits {
            assert!((h.lexical.unwrap() - want[&h.line_id]).abs() < 1e-5, "{} {:?} {}", h.line_id, h.lexical, want[&h.line_id]);
        }
    }

    /// The low-level fuzzy group keeps distance 0 (the word itself) rather
    /// than turning it into 1: the query syntax never gives 0 (`~` alone is
    /// 1, `~0` is no operator), so FtsLib parity is untouched; Otzaria-mode
    /// hosts reach 0 only through the word option.
    #[test]
    fn fuzzy_group_distance_zero_is_the_word() {
        let (_t, index) = build(true);
        let ids = |word: &str, d: u8| -> Vec<u64> {
            let json = format!("{{\"groups\":[{{\"kind\":\"fuzzy\",\"word\":\"{word}\",\"distance\":{d}}}]}}");
            let request: crate::SearchRequest = serde_json::from_str(&json).unwrap();
            index.search(&request).unwrap().hits.iter().map(|h| h.line_id).collect()
        };
        assert_eq!(ids("alpha", 0), vec![1, 2, 3, 6]);
        assert!(ids("alpxa", 0).is_empty(), "one edit away: not at distance 0");
        assert_eq!(ids("alpxa", 1), vec![1, 2, 3, 6]);
        // the query syntax: `~` is 1, `~0` is not an operator
        let parsed = |q: &str| hebrew_fts::tokenize_query(q, false);
        assert!(matches!(&parsed("alpxa~")[0], hebrew_fts::QueryToken::Word { term: Some(hebrew_fts::QueryTerm::Fuzzy { distance: 1, .. }), .. }));
        assert!(!parsed("alpxa~0").iter().any(|t| matches!(t, hebrew_fts::QueryToken::Word { term: Some(hebrew_fts::QueryTerm::Fuzzy { .. }), .. })));
    }

    /// The text side of the adjacency boost runs tantivy's phrase scorer on
    /// the line's own positions.
    #[test]
    fn text_phrase_within_slop() {
        let w = |a: &str, b: &str| vec![vec![a.to_string()], vec![b.to_string()]];
        let holds = |t: &str, words: &[Vec<String>], slop| crate::relevance::text_holds_phrase(t, words, slop, false);
        assert!(holds("alpha beta", &w("alpha", "beta"), 0));
        assert!(!holds("alpha gamma beta", &w("alpha", "beta"), 0));
        assert!(holds("alpha gamma beta", &w("alpha", "beta"), 1));
        assert!(!holds("alpha gamma delta beta", &w("alpha", "beta"), 1));
        assert!(!holds("alpha", &w("alpha", "beta"), 1));
    }

    /// match_nikud / match_teamim on the lexical side (Otzaria's vocalized
    /// smart exact query): typed nikud required, other marks free, only the
    /// lines carrying a mark; an index without marks refuses.
    #[test]
    fn hybrid_vocalized_lexical_side() {
        let (shin, lamed, vav, mem) = ('\u{05E9}', '\u{05DC}', '\u{05D5}', '\u{05DD}');
        let (qamats, patah, holam) = ('\u{05B8}', '\u{05B7}', '\u{05B9}');
        let with = |v: char| format!("{shin}{v}{lamed}{vav}{holam}{mem}");
        let plain = format!("{shin}{lamed}{vav}{mem}");
        let lines = [(1u64, with(qamats)), (2, with(patah)), (3, plain.clone())];
        let tmp = tempfile::tempdir().unwrap();
        let index = LineIndex::create(tmp.path(), IndexOptions { keep_marks: true, ..IndexOptions::default() }).unwrap();
        {
            let mut w = index.writer(50_000_000, 1).unwrap();
            for (i, (id, text)) in lines.iter().enumerate() {
                w.add(&LineDoc { line_id: *id, position: i as u64, book_id: 1, text: text.clone(), section_id: None, no_cross: false })
                    .unwrap();
            }
            w.commit(None).unwrap();
        }
        let oxv = tmp.path().join("vec.oxv");
        seforim_vectors::oxv::write_test_segment(&oxv, 8, false, "v1", &[("id:1", vec![(0, 0, axis(1))])]).unwrap();
        index.open_vectors(&oxv).unwrap();
        index.set_vector_mapping(vec![OxvBookMap { name: "id:1".into(), book_id: 1, line_ids: vec![1] }], true);
        let ids = |q: &str, nikud: bool| -> Vec<u64> {
            let o = HybridOptions { match_nikud: nikud, ..lexonly() };
            let mut v: Vec<u64> = page(&mut index.hybrid_session(q, &[], o).unwrap(), 0, 10, &[]).hits.iter().map(|h| h.line_id).collect();
            v.sort_unstable();
            v
        };
        assert_eq!(ids(&plain, false), vec![1, 2, 3]);
        assert_eq!(ids(&format!("{shin}{qamats}{lamed}{vav}{mem}"), true), vec![1]);
        assert_eq!(ids(&plain, true), vec![1, 2], "only the lines carrying a mark");
        // fuzzy + match_nikud runs too
        let o = HybridOptions { match_nikud: true, lexical_mode: LexicalMode::Fuzzy, ..lexonly() };
        assert!(!page(&mut index.hybrid_session(&plain, &[], o).unwrap(), 0, 10, &[]).hits.is_empty());
        // an index without marks cannot match them
        let (_t, flat) = build(true);
        assert!(flat.hybrid_session("alpha", &[], HybridOptions { match_teamim: true, ..lexonly() }).is_err());
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
