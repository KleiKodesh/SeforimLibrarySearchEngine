//! Search: one tantivy BooleanQuery of per-group walks, every matching line
//! with its placement in catalog order, and a [`SnippetSession`] holding the
//! groups' matched words (from the sinks) for FtsLib snippet location over
//! the texts the host fetches.

use hebrew_fts::{
    ExactTermsQuery, HebrewFuzzyQuery, HebrewWalkQuery, HebrewWildcardQuery, PreparedQueryGroups,
    SnippetWindowLocator, WalkOptions, DEFAULT_CONTEXT_WORDS, MARK_STRIDE,
};
use serde::{Deserialize, Serialize};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use std::sync::Arc;

use tantivy::query::{
    BooleanQuery, MatchedTermsSink, Occur, Query, SpanQuery, SpanTermSource,
};
use tantivy::{DocId, Score, SegmentOrdinal, SegmentReader};

use crate::fm_expand;
use crate::fm_index::FmIndex;
use crate::index::LineIndex;
use crate::{Error, Result};

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
    Wildcard {
        pattern: String,
        /// The quote is transparent (FtsLib's quote-free terms).
        #[serde(default)]
        ignore_quotes: bool,
    },
    /// FtsLib's `%word` / `word%` / `%word%`: its GrammarExpander tables
    /// (34 stacked prefixes, 32 suffixes, sofit stem).
    Grammar {
        word: String,
        prefixes: bool,
        suffixes: bool,
        #[serde(default)]
        ignore_quotes: bool,
    },
    /// Levenshtein neighbours of a word (FtsLib `word~N`), closest first,
    /// capped at 2000 terms. Distance is clamped to 2 (tantivy's DFA limit;
    /// FtsLib allowed 3).
    Fuzzy { word: String, distance: u8 },
    /// One slot satisfied by any of its alternatives (FtsLib `a | b`).
    /// Alternatives that cannot search (empty word, rejected wildcard) are
    /// dropped; if none remain, the slot is skipped.
    Any { alternatives: Vec<GroupSpec> },
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
    /// Caller-set limits (Otzaria). Absent = the walk's defaults.
    pub prefix_max_letters: Option<u32>,
    pub suffix_max_letters: Option<u32>,
    pub part_of_word_max_letters: Option<u32>,
    pub grammar_prefix_max_letters: Option<u32>,
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
            prefix_max_letters: s.prefix_max_letters,
            suffix_max_letters: s.suffix_max_letters,
            part_of_word_max_letters: s.part_of_word_max_letters,
            grammar_prefix_max_letters: s.grammar_prefix_max_letters,
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
    /// Per-pair spacing (Otzaria's customSpacing): `gaps[i]` = max words
    /// between group i and group i+1 (0 = adjacent). Runs as a span query;
    /// needs an index with positions. Order follows `require_ordered`.
    pub gaps: Option<Vec<u32>>,
    /// Partial matching (Otzaria's wordMatchMode): a line needs only this
    /// many of the searchable groups (anyWord = 1, mostWords = n/2 + 1,
    /// atLeast = N; clamped to 1..n). None = all groups (FtsLib). When it
    /// is below the group count, `max_word_distance`, `require_ordered` and
    /// `gaps` are not applied (Otzaria: with words allowed to be missing,
    /// order and distance have no meaning); the snippet is the tightest
    /// window holding that many groups.
    pub min_groups: Option<usize>,
    /// Otzaria's negative query: lines carrying these words are dropped.
    pub exclude: Option<ExcludeSpec>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            groups: Vec::new(),
            max_word_distance: None,
            require_ordered: false,
            context_words: DEFAULT_CONTEXT_WORDS,
            gaps: None,
            min_groups: None,
            exclude: None,
        }
    }
}

/// Otzaria's negative query (`apply_advanced_negative_query`): the words to
/// exclude, run as a query of their own with its own distance, order and
/// spacing, always with every word required (a partial negative would veto
/// any line carrying one of its words). A line matching it is dropped;
/// with `same_section`, every line of a section (`toc_entry_id`) holding a
/// match is dropped.
///
/// How it is applied: one word, or several with no distance/order limit,
/// or per-pair `gaps` (span query, needs positions) are excluded in the
/// query itself. Several words bound by `max_word_distance` /
/// `require_ordered` are checked on the line text when a snippet is
/// located (the same way the positive filters work), so such lines are
/// still in `hits` and come back as `None` from `locate`. With
/// `same_section` (Otzaria's `scoped_words_query`) the words are matched
/// at section resolution: the sections (`toc_entry_id`) holding word 1,
/// intersected with those holding word 2, and so on, are dropped whole;
/// distance, order and spacing do not apply there, and lines without a
/// section (`toc_entry_id` 0) are never dropped, as in Otzaria.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExcludeSpec {
    /// AND across groups, OR within a group's expansion (as `groups`).
    pub groups: Vec<GroupSpec>,
    /// Words allowed between the excluded words. None = anywhere in the line.
    pub max_word_distance: Option<usize>,
    /// The excluded words must appear in this order.
    pub require_ordered: bool,
    /// Per-pair spacing for the excluded words (needs positions).
    pub gaps: Option<Vec<u32>>,
    /// Drop every line of a section that holds a match, not just the line.
    pub same_section: bool,
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
    /// context means "short". What to do then is the host's business (the
    /// KitveiHakodesh host joins the line with its neighbours and locates
    /// again); the engine only locates the text it is given.
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
    /// Groups a window must cover (0 = all). See `SearchRequest::min_groups`.
    min_groups: usize,
    /// Text-level negative filter (see [`ExcludeSpec`]).
    negative: Option<NegativeFilter>,
    terms: TermTable,
    locator: SnippetWindowLocator,
}

/// The excluded words checked on the line text: a line where they appear
/// within the limits is dropped.
struct NegativeFilter {
    prepared: PreparedQueryGroups,
    max_word_distance: usize,
    require_ordered: bool,
}

impl NegativeFilter {
    /// True when `text` carries the excluded words within the limits.
    fn excludes(&self, locator: &mut SnippetWindowLocator, min_groups: usize, text: &str) -> bool {
        // the negative always needs ALL its words, whatever the positive's mode
        locator.set_min_groups(0);
        let d = locator.locate(text, &self.prepared, self.require_ordered, 0);
        let hit = d.is_match && d.word_distance <= self.max_word_distance;
        locator.set_min_groups(min_groups);
        hit
    }
}

impl SnippetSession {
    /// Locates the snippet in one matched line. None when the line does not
    /// pass (not all groups present, wrong order, too far apart, or the
    /// excluded words present).
    pub fn locate(&mut self, text: &str) -> Option<Snippet> {
        let prepared = self.prepared.as_ref()?;
        if let Some(n) = &self.negative {
            if n.excludes(&mut self.locator, self.min_groups, text) {
                return None;
            }
        }
        let d = self
            .locator
            .locate(text, prepared, self.require_ordered, self.original_group_count);
        Self::finish(d, prepared, &mut self.terms, self.max_word_distance)
    }

    /// Locates a batch of matched lines across all cores (one locator per
    /// thread; the prepared map is shared read-only), then numbers the marks'
    /// terms in document order. Same results as calling [`locate`](Self::locate)
    /// on each text in turn, term ids included.
    pub fn locate_many(&mut self, texts: &[&str]) -> Vec<Option<Snippet>> {
        let Some(prepared) = self.prepared.as_ref() else {
            return vec![None; texts.len()];
        };
        let (require_ordered, original, max_distance, context, keep_marks, min_groups) = (
            self.require_ordered,
            self.original_group_count,
            self.max_word_distance,
            self.context_words,
            self.keep_marks,
            self.min_groups,
        );
        let negative = self.negative.as_ref();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(texts.len().max(1));
        // Snippets with marks still holding the query's term ORDINALS.
        let mut located: Vec<Option<Snippet>> = Vec::with_capacity(texts.len());
        if threads <= 1 || texts.len() < 32 {
            for t in texts {
                if negative.is_some_and(|n| n.excludes(&mut self.locator, min_groups, t)) {
                    located.push(None);
                    continue;
                }
                let d = self.locator.locate(t, prepared, require_ordered, original);
                located.push(Self::raw(d, max_distance));
            }
        } else {
            let chunk = texts.len().div_ceil(threads);
            std::thread::scope(|scope| {
                let handles: Vec<_> = texts
                    .chunks(chunk)
                    .map(|part| {
                        scope.spawn(move || {
                            let mut locator = if keep_marks {
                                SnippetWindowLocator::keeping_marks(context)
                            } else {
                                SnippetWindowLocator::new(context)
                            };
                            locator.set_min_groups(min_groups);
                            part.iter()
                                .map(|t| {
                                    if negative.is_some_and(|n| n.excludes(&mut locator, min_groups, t)) {
                                        return None;
                                    }
                                    let d = locator.locate(t, prepared, require_ordered, original);
                                    Self::raw(d, max_distance)
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                for h in handles {
                    located.extend(h.join().expect("snippet thread panicked"));
                }
            });
        }
        // Ordinals → search-wide term ids, in document order (deterministic).
        for s in located.iter_mut().flatten() {
            for m in s.marks.chunks_exact_mut(3) {
                m[2] = self.terms.id_of(prepared.term_at(m[2] as usize));
            }
        }
        located
    }

    /// A passing descriptor as a Snippet whose marks carry term ordinals.
    fn raw(d: hebrew_fts::SnippetDescriptor<'_>, max_word_distance: usize) -> Option<Snippet> {
        if !d.is_match || d.word_distance > max_word_distance {
            return None;
        }
        Some(Snippet {
            score: d.score,
            word_distance: d.word_distance,
            window_word_count: d.window_word_count,
            start: d.snap_start,
            end: d.snap_end,
            marks: d.mark_ranges.iter().map(|&v| v as u32).collect(),
            flags: (if d.leading_ellipsis { SNIPPET_FLAG_LEADING_ELLIPSIS } else { 0 })
                | (if d.trailing_ellipsis { SNIPPET_FLAG_TRAILING_ELLIPSIS } else { 0 }),
        })
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

/// A span slot satisfied by any of several sources (FtsLib `a | b`).
struct AnySource(Vec<Arc<dyn SpanTermSource>>);

impl SpanTermSource for AnySource {
    fn match_term_infos(&self, reader: &SegmentReader) -> tantivy::Result<Vec<tantivy::postings::TermInfo>> {
        let mut all = Vec::new();
        for source in &self.0 {
            all.extend(source.match_term_infos(reader)?);
        }
        Ok(all)
    }
}

/// The sections (`toc_entry_id`, 0 = none, skipped) of every line `query`
/// matches, read straight off the fast column.
fn section_ids(query: &dyn Query, searcher: &tantivy::Searcher) -> Result<std::collections::HashSet<u64>> {
    let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(searcher))?;
    let mut sections = std::collections::HashSet::new();
    for segment in searcher.segment_readers() {
        let column: Column<u64> = segment.fast_fields().u64("toc_entry_id")?;
        weight.for_each_no_score(segment, &mut |docs| {
            for &doc in docs {
                if let Some(toc) = column.first(doc) {
                    if toc != 0 {
                        sections.insert(toc);
                    }
                }
            }
        })?;
    }
    Ok(sections)
}

impl LineIndex {
    /// The span slot for one group, reporting into `sink`. None when the
    /// group cannot search.
    fn group_slot(
        &self,
        group: &GroupSpec,
        sink: &MatchedTermsSink,
        searcher: &tantivy::Searcher,
        fm: Option<&FmIndex>,
    ) -> Result<Option<Arc<dyn SpanTermSource>>> {
        let field = self.fields.text;
        if let Some(terms) = self.fm_terms(fm, searcher, group)? {
            let mut q = ExactTermsQuery::new(field, terms);
            q.set_matched_terms_sink(sink.clone());
            return Ok(Some(q.span_slot()));
        }
        Ok(match group {
            GroupSpec::Word { word, options } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewWalkQuery::new(field, word.clone(), (*options).into());
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Wildcard { pattern, ignore_quotes } => {
                let q = if *ignore_quotes {
                    HebrewWildcardQuery::new_ignoring_quotes(field, pattern)
                } else {
                    HebrewWildcardQuery::new(field, pattern)
                };
                let Ok(mut q) = q else { return Ok(None) };
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Grammar { word, prefixes, suffixes, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = hebrew_fts::HebrewGrammarQuery::new(field, word.clone(), *prefixes, *suffixes);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Fuzzy { word, distance } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewFuzzyQuery::new(field, word.clone(), *distance);
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Any { alternatives } => {
                let mut sources = Vec::new();
                for a in alternatives {
                    if let Some(s) = self.group_slot(a, sink, searcher, fm)? {
                        sources.push(s);
                    }
                }
                match sources.len() {
                    0 => None,
                    1 => sources.pop(),
                    _ => Some(Arc::new(AnySource(sources))),
                }
            }
        })
    }

    /// The span query for a request with per-pair `gaps`. Every group must be
    /// searchable (a skipped group would shift the gaps); None when one is not.
    fn span_query(
        &self,
        request: &SearchRequest,
        gaps: &[u32],
        searcher: &tantivy::Searcher,
        fm: Option<&FmIndex>,
        sinks: &mut Vec<MatchedTermsSink>,
    ) -> Result<Option<SpanQuery>> {
        if !self.options.positions {
            return Err(Error::Tantivy(tantivy::TantivyError::InvalidArgument(
                "per-pair word spacing needs an index built with positions".into(),
            )));
        }
        if gaps.len() + 1 != request.groups.len() {
            return Err(Error::Tantivy(tantivy::TantivyError::InvalidArgument(format!(
                "gaps must have one entry per consecutive pair: {} groups, {} gaps",
                request.groups.len(),
                gaps.len()
            ))));
        }
        let mut span: Option<SpanQuery> = None;
        for (i, group) in request.groups.iter().enumerate() {
            let sink = MatchedTermsSink::new();
            let Some(slot) = self.group_slot(group, &sink, searcher, fm)? else {
                return Ok(None);
            };
            sinks.push(sink);
            span = Some(match span {
                None => SpanQuery::new(self.fields.text, slot),
                Some(s) => s.then_within(gaps[i - 1], slot),
            });
        }
        let mut span = span.expect("at least one group");
        span.set_ordered(request.require_ordered);
        Ok(Some(span))
    }

    /// The query for one slot, reporting into `sink`; None when the slot
    /// cannot search (skipped).
    fn group_query(
        &self,
        group: &GroupSpec,
        sink: &MatchedTermsSink,
        searcher: &tantivy::Searcher,
        fm: Option<&FmIndex>,
    ) -> Result<Option<Box<dyn Query>>> {
        if let Some(terms) = self.fm_terms(fm, searcher, group)? {
            let mut q = ExactTermsQuery::new(self.fields.text, terms);
            q.set_matched_terms_sink(sink.clone());
            return Ok(Some(Box::new(q)));
        }
        Ok(match group {
            GroupSpec::Word { word, options } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewWalkQuery::new(self.fields.text, word.clone(), (*options).into());
                q.set_matched_terms_sink(sink.clone());
                Some(Box::new(q))
            }
            GroupSpec::Wildcard {
                pattern,
                ignore_quotes,
            } => {
                let parsed = if *ignore_quotes {
                    HebrewWildcardQuery::new_ignoring_quotes(self.fields.text, pattern)
                } else {
                    HebrewWildcardQuery::new(self.fields.text, pattern)
                };
                let Ok(mut q) = parsed else { return Ok(None) };
                q.set_matched_terms_sink(sink.clone());
                Some(Box::new(q))
            }
            GroupSpec::Grammar {
                word,
                prefixes,
                suffixes,
                ignore_quotes,
            } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = hebrew_fts::HebrewGrammarQuery::new(self.fields.text, word.clone(), *prefixes, *suffixes);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_matched_terms_sink(sink.clone());
                Some(Box::new(q))
            }
            GroupSpec::Fuzzy { word, distance } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewFuzzyQuery::new(self.fields.text, word.clone(), *distance);
                q.set_matched_terms_sink(sink.clone());
                Some(Box::new(q))
            }
            GroupSpec::Any { alternatives } => {
                let mut parts: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for a in alternatives {
                    if let Some(q) = self.group_query(a, sink, searcher, fm)? {
                        parts.push((Occur::Should, q));
                    }
                }
                match parts.len() {
                    0 => None,
                    1 => parts.into_iter().next().map(|(_, q)| q),
                    _ => Some(Box::new(BooleanQuery::new(parts))),
                }
            }
        })
    }

    /// The FM-index sidecar to expand with, if the index has one for its
    /// current single segment.
    fn fm_sidecar_for(&self, searcher: &tantivy::Searcher) -> Option<Arc<FmIndex>> {
        #[cfg(test)]
        if self.test_only_disable_fm.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        self.fm.get(&self.dir, searcher, self.fields.text)
    }

    /// The exact terms for a group the sidecar can expand (a leading-`*`
    /// wildcard of FtsLib's FM shape, or a fuzzy word), verified and capped
    /// like the walk. None: the group runs as a walk.
    fn fm_terms(
        &self,
        fm: Option<&FmIndex>,
        searcher: &tantivy::Searcher,
        group: &GroupSpec,
    ) -> Result<Option<Vec<Vec<u8>>>> {
        let Some(fm) = fm else { return Ok(None) };
        let field = self.fields.text;
        let dict = || -> Result<_> { Ok(searcher.segment_readers()[0].inverted_index(field)?) };
        match group {
            GroupSpec::Wildcard { pattern, ignore_quotes } => {
                let Some((core, suffix_only)) = fm_expand::fm_wildcard_shape(pattern, *ignore_quotes) else {
                    return Ok(None);
                };
                let parsed = if *ignore_quotes {
                    HebrewWildcardQuery::new_ignoring_quotes(field, pattern)
                } else {
                    HebrewWildcardQuery::new(field, pattern)
                };
                // A rejected pattern is rejected on both paths (the slot is skipped).
                let Ok(q) = parsed else { return Ok(None) };
                let inverted = dict()?;
                let terms = fm_expand::expand_wildcard(
                    fm,
                    inverted.terms(),
                    &q.automaton(),
                    &core,
                    suffix_only,
                    q.max_expansions(),
                )?;
                Ok(Some(terms))
            }
            GroupSpec::Fuzzy { word, distance } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let q = HebrewFuzzyQuery::new(field, word.clone(), *distance);
                let inverted = dict()?;
                let terms = fm_expand::expand_fuzzy(
                    fm,
                    inverted.terms(),
                    &q.automaton(),
                    q.word(),
                    q.distance_limit(),
                    q.max_expansions(),
                )?;
                Ok(Some(terms))
            }
            _ => Ok(None),
        }
    }

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
        // Partial matching: at least `min` of the searchable groups. Clamped
        // to 1..n like Otzaria's WordMatch::min_words; n or more = all. (n
        // counts the request's groups; Otzaria counts distinct materialized
        // words, which differs only for duplicate words or rejected wildcards.)
        let partial_min = request
            .min_groups
            .filter(|&k| k < request.groups.len())
            .map(|k| k.max(1));
        let partial = partial_min.is_some();
        let mut session = SnippetSession {
            prepared: None,
            original_group_count: request.groups.len(),
            require_ordered: request.require_ordered && !partial,
            max_word_distance: if partial { usize::MAX } else { request.max_word_distance.unwrap_or(usize::MAX) },
            context_words: request.context_words,
            keep_marks,
            min_groups: 0,
            negative: None,
            terms: TermTable::default(),
            locator,
        };

        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let fm = self.fm_sidecar_for(&searcher);
        let fm = fm.as_deref();
        let mut sinks = Vec::new();

        let query: Box<dyn Query> = if let (Some(gaps), false) = (&request.gaps, partial) {
            // Per-pair spacing: a span query, one slot per group, each gap its own limit.
            match self.span_query(request, gaps, &searcher, fm, &mut sinks)? {
                Some(q) => Box::new(q),
                None => return Ok(SearchResult { hits: Vec::new(), session }),
            }
        } else {
            // One query + one sink per searchable slot. Rejected wildcards skip
            // their slot (it still counts in original_group_count).
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for group in &request.groups {
                let sink = MatchedTermsSink::new();
                if let Some(q) = self.group_query(group, &sink, &searcher, fm)? {
                    clauses.push((Occur::Must, q));
                    sinks.push(sink);
                }
            }
            if clauses.is_empty() {
                return Ok(SearchResult {
                    hits: Vec::new(),
                    session,
                });
            }
            match partial_min {
                Some(min) if min < clauses.len() => {
                    let should: Vec<(Occur, Box<dyn Query>)> =
                        clauses.into_iter().map(|(_, q)| (Occur::Should, q)).collect();
                    Box::new(BooleanQuery::with_minimum_required_clauses(should, min))
                }
                _ => Box::new(BooleanQuery::new(clauses)),
            }
        };

        // The negative query (see ExcludeSpec): excluded in the query itself,
        // or by section, or on the text at locate time.
        let mut negative_sinks: Vec<MatchedTermsSink> = Vec::new();
        let mut section_words: Vec<Box<dyn Query>> = Vec::new();
        let mut text_level_negative: Option<(usize, bool)> = None;
        let mut query = query;
        if let Some(ex) = &request.exclude {
            if ex.same_section {
                // Otzaria's scoped_words_query: one query per word; the
                // sections are intersected below, proximity does not apply.
                for group in &ex.groups {
                    let sink = MatchedTermsSink::new();
                    if let Some(q) = self.group_query(group, &sink, &searcher, fm)? {
                        section_words.push(q);
                    }
                }
            }
            let negative: Option<Box<dyn Query>> = if ex.same_section {
                None
            } else if let Some(gaps) = &ex.gaps {
                let req = SearchRequest {
                    groups: ex.groups.clone(),
                    require_ordered: ex.require_ordered,
                    gaps: Some(gaps.clone()),
                    ..Default::default()
                };
                self.span_query(&req, gaps, &searcher, fm, &mut negative_sinks)?
                    .map(|q| Box::new(q) as Box<dyn Query>)
            } else {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for group in &ex.groups {
                    let sink = MatchedTermsSink::new();
                    if let Some(q) = self.group_query(group, &sink, &searcher, fm)? {
                        clauses.push((Occur::Must, q));
                        negative_sinks.push(sink);
                    }
                }
                (!clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
            };
            if let Some(negative) = negative {
                let bound = ex.max_word_distance.is_some() || ex.require_ordered;
                let needs_text = ex.gaps.is_none() && negative_sinks.len() > 1 && bound;
                if needs_text {
                    // Build the scorers once so the walks report the excluded
                    // forms; the docs themselves are not needed.
                    let w = negative.weight(tantivy::query::EnableScoring::disabled_from_searcher(&searcher))?;
                    for segment in searcher.segment_readers() {
                        let _ = w.scorer(segment, 1.0)?;
                    }
                    text_level_negative =
                        Some((ex.max_word_distance.unwrap_or(usize::MAX), ex.require_ordered));
                } else {
                    query = Box::new(BooleanQuery::new(vec![(Occur::Must, query), (Occur::MustNot, negative)]));
                }
            }
        }

        let timing = std::env::var_os("SEFORIM_ENGINE_TIMING").is_some();
        let t = std::time::Instant::now();
        let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(&searcher))?;
        if timing {
            eprintln!("  [timing] weight (walks + caps): {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
        let t = std::time::Instant::now();
        let mut hits = Vec::new();
        for (ord, segment) in searcher.segment_readers().iter().enumerate() {
            let mut child = PlacedCollector.for_segment(ord as SegmentOrdinal, segment)?;
            weight.for_each_no_score(segment, &mut |docs| {
                for &doc in docs {
                    child.collect(doc, 0.0);
                }
            })?;
            hits.extend(child.harvest());
        }
        if timing {
            eprintln!("  [timing] collect {} hits: {:.1} ms", hits.len(), t.elapsed().as_secs_f64() * 1000.0);
        }
        if !section_words.is_empty() {
            // Otzaria's SameSection scope: the sections holding every
            // excluded word (each anywhere in the section) are blocked whole.
            let mut blocked: Option<std::collections::HashSet<u64>> = None;
            for word in &section_words {
                let sections = section_ids(word.as_ref(), &searcher)?;
                blocked = Some(match blocked {
                    None => sections,
                    Some(prior) => prior.intersection(&sections).copied().collect(),
                });
                if blocked.as_ref().is_some_and(|b| b.is_empty()) {
                    break;
                }
            }
            if let Some(blocked) = blocked {
                hits.retain(|h| !blocked.contains(&h.toc_entry_id));
            }
        }
        hits.sort_unstable_by_key(|h| h.position);
        if let Some((max_word_distance, require_ordered)) = text_level_negative {
            let negative_terms: Vec<Vec<String>> = negative_sinks.iter().map(|s| s.take_strings()).collect();
            // a word with no indexed form can never be present: no exclusion
            if negative_terms.iter().all(|g| !g.is_empty()) {
                session.negative = Some(NegativeFilter {
                    prepared: PreparedQueryGroups::from_groups(&negative_terms),
                    max_word_distance,
                    require_ordered,
                });
            }
        }

        let mut group_terms: Vec<Vec<String>> = sinks.iter().map(|s| s.take_strings()).collect();
        if let Some(min) = partial_min {
            // a group that matched no indexed word simply cannot be one of
            // the `min`; the others still can
            group_terms.retain(|g| !g.is_empty());
            if group_terms.len() < min {
                return Ok(SearchResult { hits: Vec::new(), session });
            }
            session.min_groups = min;
            session.locator.set_min_groups(min);
        } else if group_terms.iter().any(|g| g.is_empty()) {
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
