//! Search: one tantivy BooleanQuery of per-group walks, every matching line
//! with its placement in catalog order, and a [`SnippetSession`] holding the
//! groups' matched words (from the sinks) for FtsLib snippet location over
//! the texts the host fetches.

use hebrew_fts::{
    HebrewWalkQuery, HebrewWildcardQuery, PreparedQueryGroups, SnippetWindowLocator, WalkOptions,
    DEFAULT_CONTEXT_WORDS, MARK_STRIDE,
};
use serde::{Deserialize, Serialize};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::query::{BooleanQuery, MatchedTermsSink, Occur, Query};
use tantivy::{DocId, Score, SegmentOrdinal, SegmentReader};

use crate::index::LineIndex;
use crate::Result;

/// One AND slot of a query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GroupSpec {
    /// A word with Hebrew walk options (prefixes, suffixes, ktiv, …).
    Word {
        word: String,
        #[serde(default)]
        options: WalkOptionsSpec,
    },
    /// A wildcard pattern (`*`, `?`). A rejected pattern (anchor too short,
    /// too many `?`) skips the slot but still counts toward word distance.
    Wildcard { pattern: String },
}

/// Serializable mirror of [`WalkOptions`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WalkOptionsSpec {
    pub grammar_prefixes: bool,
    pub grammar_suffixes: bool,
    pub aramaic_prefixes: bool,
    pub aramaic_suffixes: bool,
    pub ktiv_male_haser: bool,
    pub part_of_word: bool,
    pub advanced_prefixes: bool,
    pub advanced_suffixes: bool,
    pub ignore_quotes: bool,
    pub spelling_errors: bool,
    pub ignore_nikud: bool,
    pub ignore_teamim: bool,
}

impl From<WalkOptionsSpec> for WalkOptions {
    fn from(s: WalkOptionsSpec) -> WalkOptions {
        WalkOptions {
            grammar_prefixes: s.grammar_prefixes,
            grammar_suffixes: s.grammar_suffixes,
            aramaic_prefixes: s.aramaic_prefixes,
            aramaic_suffixes: s.aramaic_suffixes,
            ktiv_male_haser: s.ktiv_male_haser,
            part_of_word: s.part_of_word,
            advanced_prefixes: s.advanced_prefixes,
            advanced_suffixes: s.advanced_suffixes,
            ignore_quotes: s.ignore_quotes,
            spelling_errors: s.spelling_errors,
            ignore_nikud: s.ignore_nikud,
            ignore_teamim: s.ignore_teamim,
        }
    }
}

/// A search.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchRequest {
    /// AND across groups, OR within a group's expansion.
    pub groups: Vec<GroupSpec>,
    /// Drop hits whose tightest window has more words between the matched
    /// slots than this (מרחק בין מילים). None = no limit.
    pub max_word_distance: Option<usize>,
    /// Only windows where the groups appear in query order.
    pub require_ordered: bool,
    /// Words of context per side of the match.
    pub context_words: usize,
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            groups: Vec::new(),
            max_word_distance: None,
            require_ordered: false,
            context_words: DEFAULT_CONTEXT_WORDS,
        }
    }
}

/// One matching line, in catalog order. The host fetches its text and
/// locates the snippet through the session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlacedHit {
    pub position: u64,
    pub line_id: u64,
    pub book_id: u64,
    pub line_index: u64,
    pub toc_entry_id: u64,
}

/// Snippet flag: "…" before the window.
pub const SNIPPET_FLAG_LEADING_ELLIPSIS: u32 = 1;
/// Snippet flag: "…" after the window.
pub const SNIPPET_FLAG_TRAILING_ELLIPSIS: u32 = 2;

/// A located snippet that passed the match and word-distance filters
/// (FtsLib's descriptor, as copied onto `FtsHit`). Offsets are UTF-16 units
/// into the text the host passed in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Snippet {
    /// Raw span of the tightest window (smaller = closer).
    pub score: usize,
    /// Words between the matched slots in the tightest window.
    pub word_distance: usize,
    /// Words the window spans after context expansion; fewer than the
    /// context means "short": widen over neighbour lines.
    pub window_word_count: usize,
    pub start: usize,
    pub end: usize,
    /// (start, length, termId) triples; termId indexes the session's term table.
    pub marks: Vec<u32>,
    pub flags: u32,
}

impl Snippet {
    /// True when the window holds fewer words than `context_words`.
    pub fn is_short(&self, context_words: usize) -> bool {
        self.window_word_count < context_words
    }
}

/// One search's matched-term strings, numbered in first-seen order.
#[derive(Debug, Default)]
struct TermTable {
    ids: std::collections::HashMap<String, u32>,
    terms: Vec<String>,
    sent: usize,
}

impl TermTable {
    fn id_of(&mut self, term: &str) -> u32 {
        if let Some(&id) = self.ids.get(term) {
            return id;
        }
        let id = self.terms.len() as u32;
        self.terms.push(term.to_string());
        self.ids.insert(term.to_string(), id);
        id
    }
}

/// The per-search state for snippet location: the prepared term→group map
/// (built once from the groups' sinks), the filters, and the term table.
pub struct SnippetSession {
    prepared: Option<PreparedQueryGroups>,
    original_group_count: usize,
    require_ordered: bool,
    max_word_distance: usize,
    context_words: usize,
    keep_marks: bool,
    terms: TermTable,
    locator: SnippetWindowLocator,
    joined: String,
}

impl SnippetSession {
    /// Locates the snippet in one matched line. None when the line does not
    /// pass (not all groups present, wrong order, or too far apart).
    pub fn locate(&mut self, text: &str) -> Option<Snippet> {
        let prepared = self.prepared.as_ref()?;
        let d = self
            .locator
            .locate(text, prepared, self.require_ordered, self.original_group_count);
        Self::finish(d, prepared, &mut self.terms, self.max_word_distance)
    }

    /// Re-locates a short line's snippet over the line joined with its
    /// neighbours: `previous + ' ' + text + ' ' + next`, empty sides dropped.
    /// Offsets are into that joined text. Score and word distance stay those
    /// of `matched_line_snippet` (relevance is the matched line's; the
    /// neighbours are presentation). None if the re-locate fails.
    pub fn locate_with_neighbours(
        &mut self,
        previous: &str,
        text: &str,
        next: &str,
        matched_line_snippet: &Snippet,
    ) -> Option<Snippet> {
        let prepared = self.prepared.as_ref()?;
        let d = self.locator.locate_over_neighbour_lines(
            previous,
            text,
            next,
            prepared,
            self.require_ordered,
            self.original_group_count,
            &mut self.joined,
        );
        let mut s = Self::finish(d, prepared, &mut self.terms, usize::MAX)?;
        s.score = matched_line_snippet.score;
        s.word_distance = matched_line_snippet.word_distance;
        Some(s)
    }

    fn finish(
        d: hebrew_fts::SnippetDescriptor<'_>,
        prepared: &PreparedQueryGroups,
        terms: &mut TermTable,
        max_word_distance: usize,
    ) -> Option<Snippet> {
        if !d.is_match || d.word_distance > max_word_distance {
            return None;
        }
        let triples = d.mark_ranges.len() / MARK_STRIDE;
        let mut marks = Vec::with_capacity(triples * MARK_STRIDE);
        for t in 0..triples {
            let i = t * MARK_STRIDE;
            marks.push(d.mark_ranges[i] as u32);
            marks.push(d.mark_ranges[i + 1] as u32);
            marks.push(terms.id_of(prepared.term_at(d.mark_ranges[i + 2])));
        }
        Some(Snippet {
            score: d.score,
            word_distance: d.word_distance,
            window_word_count: d.window_word_count,
            start: d.snap_start,
            end: d.snap_end,
            marks,
            flags: (if d.leading_ellipsis { SNIPPET_FLAG_LEADING_ELLIPSIS } else { 0 })
                | (if d.trailing_ellipsis { SNIPPET_FLAG_TRAILING_ELLIPSIS } else { 0 }),
        })
    }

    /// Words of context per side this session uses.
    pub fn context_words(&self) -> usize {
        self.context_words
    }

    /// True when the index keeps marks (the session's tokenizer does too).
    pub fn keeps_marks(&self) -> bool {
        self.keep_marks
    }

    /// Terms added to the term table since the previous call, in id order.
    pub fn take_new_terms(&mut self) -> Vec<String> {
        let fresh = self.terms.terms[self.terms.sent..].to_vec();
        self.terms.sent = self.terms.terms.len();
        fresh
    }

    /// Every term so far, by id.
    pub fn all_terms(&self) -> &[String] {
        &self.terms.terms
    }

    /// Every group's matched words, in query order (skipped slots absent).
    pub fn group_count(&self) -> usize {
        self.prepared.as_ref().map(|p| p.group_count()).unwrap_or(0)
    }
}

/// What a search returns: every matching line in catalog order, and the
/// session to locate their snippets.
pub struct SearchResult {
    pub hits: Vec<PlacedHit>,
    pub session: SnippetSession,
}

// ── Collector: every matching doc with its placement, from fast fields ───

struct PlacedCollector;

struct PlacedSegmentCollector {
    columns: [Column<u64>; 5],
    out: Vec<PlacedHit>,
}

impl Collector for PlacedCollector {
    type Fruit = Vec<PlacedHit>;
    type Child = PlacedSegmentCollector;

    fn for_segment(
        &self,
        _segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<PlacedSegmentCollector> {
        let ff = segment.fast_fields();
        Ok(PlacedSegmentCollector {
            columns: [
                ff.u64("position")?,
                ff.u64("line_id")?,
                ff.u64("book_id")?,
                ff.u64("line_index")?,
                ff.u64("toc_entry_id")?,
            ],
            out: Vec::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Vec<PlacedHit>>) -> tantivy::Result<Vec<PlacedHit>> {
        Ok(fruits.into_iter().flatten().collect())
    }
}

impl SegmentCollector for PlacedSegmentCollector {
    type Fruit = Vec<PlacedHit>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        let get = |c: &Column<u64>| c.first(doc).unwrap_or(0);
        self.out.push(PlacedHit {
            position: get(&self.columns[0]),
            line_id: get(&self.columns[1]),
            book_id: get(&self.columns[2]),
            line_index: get(&self.columns[3]),
            toc_entry_id: get(&self.columns[4]),
        });
    }

    fn harvest(self) -> Vec<PlacedHit> {
        self.out
    }
}

impl LineIndex {
    /// Runs `request`. Every matching line comes back in catalog order;
    /// the host fetches texts (in its own batches) and locates snippets
    /// through the returned session, which applies the word-distance and
    /// order filters.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResult> {
        let keep_marks = self.options.keep_marks;
        let locator = if keep_marks {
            SnippetWindowLocator::keeping_marks(request.context_words)
        } else {
            SnippetWindowLocator::new(request.context_words)
        };
        let mut session = SnippetSession {
            prepared: None,
            original_group_count: request.groups.len(),
            require_ordered: request.require_ordered,
            max_word_distance: request.max_word_distance.unwrap_or(usize::MAX),
            context_words: request.context_words,
            keep_marks,
            terms: TermTable::default(),
            locator,
            joined: String::new(),
        };

        // One query + one sink per searchable slot. Rejected wildcards skip
        // their slot (it still counts in original_group_count).
        let mut sinks = Vec::new();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for group in &request.groups {
            let sink = MatchedTermsSink::new();
            match group {
                GroupSpec::Word { word, options } => {
                    if word.is_empty() {
                        continue;
                    }
                    let mut q = HebrewWalkQuery::new(self.fields.text, word.clone(), (*options).into());
                    q.set_matched_terms_sink(sink.clone());
                    clauses.push((Occur::Must, Box::new(q)));
                }
                GroupSpec::Wildcard { pattern } => {
                    let Ok(mut q) = HebrewWildcardQuery::new(self.fields.text, pattern) else {
                        continue;
                    };
                    q.set_matched_terms_sink(sink.clone());
                    clauses.push((Occur::Must, Box::new(q)));
                }
            }
            sinks.push(sink);
        }
        if clauses.is_empty() {
            return Ok(SearchResult {
                hits: Vec::new(),
                session,
            });
        }

        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let mut hits = searcher.search(&BooleanQuery::new(clauses), &PlacedCollector)?;
        hits.sort_unstable_by_key(|h| h.position);

        let group_terms: Vec<Vec<String>> = sinks.iter().map(|s| s.take_strings()).collect();
        if group_terms.iter().any(|g| g.is_empty()) {
            // a group matched no indexed word: nothing can match
            return Ok(SearchResult {
                hits: Vec::new(),
                session,
            });
        }
        session.prepared = Some(PreparedQueryGroups::from_groups(&group_terms));
        Ok(SearchResult { hits, session })
    }
}
