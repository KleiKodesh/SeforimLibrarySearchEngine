//! Port of `SeforimDbFtsSearcher.cs` on top of tantivy: a search yields
//! batches of finished hits in catalog order, each with its FtsLib snippet
//! descriptor (offsets + marks, no HTML), the word-distance filter applied,
//! short windows widened over neighbour lines of the same book.
//!
//! Where FtsLib ran FTS5 + its own expansion, this runs one tantivy
//! `BooleanQuery` of per-group Hebrew walks. Each group's `MatchedTermsSink`
//! fills during the search with exactly the indexed words that group
//! matched, which is the `PreparedQueryGroups` the snippet locator needs,
//! built once per search.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

use hebrew_fts::{
    HebrewWalkQuery, PreparedQueryGroups, SnippetWindowLocator, WalkOptions, DEFAULT_CONTEXT_WORDS,
    MARK_STRIDE,
};
use tantivy::collector::{Collector, SegmentCollector};
use tantivy::columnar::Column;
use tantivy::query::{BooleanQuery, MatchedTermsSink, Occur, Query};
use tantivy::{DocId, Score, SegmentOrdinal, SegmentReader};

use crate::corpus::{PlacedLine, SeforimDbCorpus};
use crate::error::Result;
use crate::index::LineIndex;

// ── Batch plan (SeforimDbFtsSearcher.cs:42-64) ───────────────────────────

/// Lines per batch for the first batches.
pub const FIRST_BATCH_SIZE: usize = 256;
/// How many batches stay at FIRST_BATCH_SIZE.
pub const FIRST_BATCH_COUNT: usize = 16;
/// Batch size the doubling grows to.
pub const GROWN_BATCH_SIZE: usize = 2048;
/// Hard ceiling.
pub const MAX_BATCH_SIZE: usize = 4096;
/// Lines of context to pull per side when extending a short window (same
/// book only).
pub const NEIGHBOR_LINE_RADIUS: i64 = 2;
/// How many leading hits get their snippet line texts sent along.
pub const SEED_LINE_TEXT_HIT_COUNT: usize = 100;

/// The batch plan: 256 lines for the first 16 batches, then doubling to 2048.
pub fn batch_size_at(batch_index: usize) -> usize {
    if batch_index < FIRST_BATCH_COUNT {
        return FIRST_BATCH_SIZE;
    }
    let mut size = FIRST_BATCH_SIZE;
    let mut i = FIRST_BATCH_COUNT;
    while i <= batch_index && size < GROWN_BATCH_SIZE {
        size *= 2;
        i += 1;
    }
    size.min(MAX_BATCH_SIZE)
}

// ── Request / response ──────────────────────────────────────────────────

/// One AND slot of the query: a word and the walk options applied to it.
#[derive(Debug, Clone)]
pub struct WordGroup {
    /// Normalized the way the index normalizes (lowercase ASCII, canonical `"`).
    pub word: String,
    pub options: WalkOptions,
}

/// A search.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    /// AND across groups, OR within a group's expansion.
    pub groups: Vec<WordGroup>,
    /// Hits whose tightest window is looser than this are dropped (מרחק בין
    /// מילים). None = no limit.
    pub max_word_distance: Option<usize>,
    /// Only match windows where the query words appear in query order.
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

/// Snippet flag: "…" before the window.
pub const SNIPPET_FLAG_LEADING_ELLIPSIS: u32 = 1;
/// Snippet flag: "…" after the window.
pub const SNIPPET_FLAG_TRAILING_ELLIPSIS: u32 = 2;

/// One finished hit (FtsLib `FtsHit`). Snippet offsets are UTF-16 units into
/// the texts of `snippet_line_ids` joined by single spaces.
#[derive(Debug, Clone)]
pub struct Hit {
    pub line_id: i64,
    pub book_id: i64,
    pub toc_entry_id: i64,
    /// Raw span of the tightest window (smaller = closer).
    pub score: usize,
    /// Words between the matched slots in the tightest window.
    pub word_distance: usize,
    /// The lines the snippet text is joined from (the matched line, or
    /// previous ids, matched id, next ids).
    pub snippet_line_ids: Vec<i64>,
    pub snippet_start: usize,
    pub snippet_end: usize,
    /// (start, length, termId) triples; termId indexes the search's term table.
    pub snippet_marks: Vec<u32>,
    pub snippet_flags: u32,
}

/// One batch of finished hits, in catalog order.
#[derive(Debug, Default)]
pub struct HitBatch {
    pub hits: Vec<Hit>,
    /// Line texts for the first hits of the search (matched lines and the
    /// neighbour lines their windows were extended over).
    pub line_texts: HashMap<i64, String>,
    /// Terms added to the term table since the previous batch, in id order.
    pub new_terms: Vec<String>,
}

/// One search's matched-term strings, numbered in first-seen order (FtsLib
/// `FtsTermTable`).
#[derive(Debug, Default)]
pub struct TermTable {
    ids: HashMap<String, u32>,
    terms: Vec<String>,
    sent: usize,
}

impl TermTable {
    /// The id of `term`, assigning the next one on first sight.
    pub fn id_of(&mut self, term: &str) -> u32 {
        if let Some(&id) = self.ids.get(term) {
            return id;
        }
        let id = self.terms.len() as u32;
        self.terms.push(term.to_string());
        self.ids.insert(term.to_string(), id);
        id
    }

    /// The terms added since the previous call, in id order.
    pub fn take_new_terms(&mut self) -> Vec<String> {
        let fresh = self.terms[self.sent..].to_vec();
        self.sent = self.terms.len();
        fresh
    }

    /// Every term so far, indexed by id.
    pub fn all(&self) -> &[String] {
        &self.terms
    }
}

// ── Collector: every matching doc with its placement, from fast fields ───

#[derive(Debug, Clone, Copy)]
struct Placed {
    position: u64,
    line_id: i64,
    book_id: i64,
    line_index: i64,
    toc_entry_id: i64,
}

struct PlacedCollector;

struct PlacedSegmentCollector {
    position: Column<u64>,
    line_id: Column<u64>,
    book_id: Column<u64>,
    line_index: Column<u64>,
    toc_entry_id: Column<u64>,
    out: Vec<Placed>,
}

impl Collector for PlacedCollector {
    type Fruit = Vec<Placed>;
    type Child = PlacedSegmentCollector;

    fn for_segment(
        &self,
        _segment_local_id: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<PlacedSegmentCollector> {
        let ff = segment.fast_fields();
        Ok(PlacedSegmentCollector {
            position: ff.u64("position")?,
            line_id: ff.u64("line_id")?,
            book_id: ff.u64("book_id")?,
            line_index: ff.u64("line_index")?,
            toc_entry_id: ff.u64("toc_entry_id")?,
            out: Vec::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Vec<Placed>>) -> tantivy::Result<Vec<Placed>> {
        Ok(fruits.into_iter().flatten().collect())
    }
}

impl SegmentCollector for PlacedSegmentCollector {
    type Fruit = Vec<Placed>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        self.out.push(Placed {
            position: self.position.first(doc).unwrap_or(0),
            line_id: self.line_id.first(doc).unwrap_or(0) as i64,
            book_id: self.book_id.first(doc).unwrap_or(0) as i64,
            line_index: self.line_index.first(doc).unwrap_or(0) as i64,
            toc_entry_id: self.toc_entry_id.first(doc).unwrap_or(0) as i64,
        });
    }

    fn harvest(self) -> Vec<Placed> {
        self.out
    }
}

// ── The search ──────────────────────────────────────────────────────────

/// A located window, copied out of the locator's scratch.
#[derive(Debug, Clone)]
struct Window {
    is_match: bool,
    score: usize,
    word_distance: usize,
    window_word_count: usize,
    snap_start: usize,
    snap_end: usize,
    leading_ellipsis: bool,
    trailing_ellipsis: bool,
    marks: Vec<usize>,
}

impl Window {
    fn from(d: &hebrew_fts::SnippetDescriptor<'_>) -> Window {
        Window {
            is_match: d.is_match,
            score: d.score,
            word_distance: d.word_distance,
            window_word_count: d.window_word_count,
            snap_start: d.snap_start,
            snap_end: d.snap_end,
            leading_ellipsis: d.leading_ellipsis,
            trailing_ellipsis: d.trailing_ellipsis,
            marks: d.mark_ranges.to_vec(),
        }
    }
}

/// Runs `request` and calls `on_batch` for each finished batch, in catalog
/// order, until the results run out or the callback breaks. Returns the
/// search's term table.
pub fn search(
    index: &LineIndex,
    corpus: &mut SeforimDbCorpus,
    request: &SearchRequest,
    mut on_batch: impl FnMut(HitBatch) -> ControlFlow<()>,
) -> Result<TermTable> {
    let mut terms = TermTable::default();
    if request.groups.is_empty() || request.groups.iter().any(|g| g.word.is_empty()) {
        return Ok(terms);
    }

    // One walk + one sink per AND group.
    let mut sinks = Vec::with_capacity(request.groups.len());
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(request.groups.len());
    for group in &request.groups {
        let sink = MatchedTermsSink::new();
        let mut walk = HebrewWalkQuery::new(index.fields.text, group.word.clone(), group.options);
        walk.set_matched_terms_sink(sink.clone());
        sinks.push(sink);
        clauses.push((Occur::Must, Box::new(walk)));
    }
    let query = BooleanQuery::new(clauses);

    let reader = index.index.reader()?;
    let searcher = reader.searcher();
    let mut placed = searcher.search(&query, &PlacedCollector)?;
    placed.sort_unstable_by_key(|p| p.position);

    // The prepared term→group map: each group's matched words, from its sink.
    let group_terms: Vec<Vec<String>> = sinks.iter().map(|s| s.take_strings()).collect();
    if group_terms.iter().any(|g| g.is_empty()) {
        return Ok(terms); // hard miss: a group matched no indexed word
    }
    let prepared = PreparedQueryGroups::from_groups(&group_terms);
    let original_group_count = request.groups.len();

    let mut seed_left = SEED_LINE_TEXT_HIT_COUNT;
    let mut batch_index = 0usize;
    let mut start = 0usize;
    while start < placed.len() {
        let size = batch_size_at(batch_index);
        let end = (start + size).min(placed.len());
        let batch = build_batch(
            corpus,
            &placed[start..end],
            &prepared,
            original_group_count,
            request,
            &mut terms,
            seed_left,
        )?;
        seed_left = seed_left.saturating_sub(batch.hits.len());
        start = end;
        batch_index += 1;
        if !batch.hits.is_empty() && on_batch(batch).is_break() {
            break;
        }
    }
    Ok(terms)
}

/// One batch: engine matches → finished hits (window located, word-distance
/// filter, short windows extended over neighbour lines, marks mapped to the
/// term table, first hits' line texts seeded).
fn build_batch(
    corpus: &mut SeforimDbCorpus,
    results: &[Placed],
    prepared: &PreparedQueryGroups,
    original_group_count: usize,
    request: &SearchRequest,
    terms: &mut TermTable,
    seed_left: usize,
) -> Result<HitBatch> {
    // Content for the batch, from the DB.
    let ids: Vec<i64> = results.iter().map(|p| p.line_id).collect();
    let fetched = corpus.fetch_placed_lines(&ids)?;
    let mut content_by_id: HashMap<i64, PlacedLine> =
        fetched.into_iter().map(|l| (l.id, l)).collect();
    let contents: Vec<String> = results
        .iter()
        .map(|p| content_by_id.remove(&p.line_id).map(|l| l.text).unwrap_or_default())
        .collect();

    // Locate every window, across threads (one locator per thread).
    let context_words = request.context_words;
    let require_ordered = request.require_ordered;
    let windows: Vec<Option<Window>> = locate_all(
        &contents,
        prepared,
        require_ordered,
        original_group_count,
        context_words,
    );

    // Match + word-distance filters.
    let max_distance = request.max_word_distance.unwrap_or(usize::MAX);
    let mut built: Vec<Option<(Hit, Window)>> = Vec::with_capacity(results.len());
    for (p, window) in results.iter().zip(windows) {
        let Some(window) = window else {
            built.push(None);
            continue;
        };
        if !window.is_match || window.word_distance > max_distance {
            built.push(None);
            continue;
        }
        let hit = Hit {
            line_id: p.line_id,
            book_id: p.book_id,
            toc_entry_id: p.toc_entry_id,
            score: window.score,
            word_distance: window.word_distance,
            snippet_line_ids: vec![p.line_id],
            snippet_start: 0,
            snippet_end: 0,
            snippet_marks: Vec::new(),
            snippet_flags: 0,
        };
        built.push(Some((hit, window)));
    }

    // Short windows → neighbour lines of the same book.
    let neighbour_texts = extend_short_windows(
        corpus,
        results,
        &contents,
        &mut built,
        prepared,
        require_ordered,
        original_group_count,
        context_words,
    )?;

    let mut batch = HitBatch::default();
    for (i, entry) in built.into_iter().enumerate() {
        let Some((mut hit, window)) = entry else { continue };
        copy_descriptor(&mut hit, &window, prepared, terms);
        if batch.hits.len() < seed_left {
            for &id in &hit.snippet_line_ids {
                if batch.line_texts.contains_key(&id) {
                    continue;
                }
                if id == hit.line_id {
                    batch.line_texts.insert(id, contents[i].clone());
                } else if let Some(text) = neighbour_texts.get(&id) {
                    batch.line_texts.insert(id, text.clone());
                }
            }
        }
        batch.hits.push(hit);
    }
    batch.new_terms = terms.take_new_terms();
    Ok(batch)
}

/// Locates the windows of `contents` in parallel.
fn locate_all(
    contents: &[String],
    prepared: &PreparedQueryGroups,
    require_ordered: bool,
    original_group_count: usize,
    context_words: usize,
) -> Vec<Option<Window>> {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(contents.len().max(1));
    let chunk = contents.len().div_ceil(threads).max(1);
    let mut out: Vec<Option<Window>> = Vec::with_capacity(contents.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = contents
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || {
                    let mut locator = SnippetWindowLocator::new(context_words);
                    part.iter()
                        .map(|text| {
                            let d = locator.locate(text, prepared, require_ordered, original_group_count);
                            if d.is_match {
                                Some(Window::from(&d))
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for h in handles {
            out.extend(h.join().expect("locator thread panicked"));
        }
    });
    out
}

/// Re-locates the batch's SHORT windows (fewer words than the per-side
/// context) over the matched line joined with its same-book neighbours at
/// lineIndex ± 1..=NEIGHBOR_LINE_RADIUS. Neighbours are served from the
/// batch's own content first, the rest fetched in one call. Only the snippet
/// descriptor is swapped in: score and word distance stay as computed on the
/// matched line. Returns the neighbour texts by line id.
#[allow(clippy::too_many_arguments)]
fn extend_short_windows(
    corpus: &mut SeforimDbCorpus,
    results: &[Placed],
    contents: &[String],
    built: &mut [Option<(Hit, Window)>],
    prepared: &PreparedQueryGroups,
    require_ordered: bool,
    original_group_count: usize,
    context_words: usize,
) -> Result<HashMap<i64, String>> {
    let mut short: Vec<usize> = Vec::new();
    for (i, entry) in built.iter().enumerate() {
        let Some((_, window)) = entry else { continue };
        if window.window_word_count >= context_words {
            continue;
        }
        if results[i].book_id == 0 {
            continue; // no placement: nothing to extend over
        }
        short.push(i);
    }
    if short.is_empty() {
        return Ok(HashMap::new());
    }

    // Wanted (bookId, lineIndex) positions; serve from the batch, fetch the rest.
    let mut wanted: HashSet<(i64, i64)> = HashSet::new();
    for &i in &short {
        let at = results[i];
        for delta in -NEIGHBOR_LINE_RADIUS..=NEIGHBOR_LINE_RADIUS {
            if delta != 0 {
                wanted.insert((at.book_id, at.line_index + delta));
            }
        }
    }
    let mut by_position: HashMap<(i64, i64), (i64, String)> = HashMap::new();
    for (p, text) in results.iter().zip(contents) {
        let key = (p.book_id, p.line_index);
        if wanted.contains(&key) {
            by_position.insert(key, (p.line_id, text.clone()));
        }
    }
    if by_position.len() < wanted.len() {
        let missing: Vec<(i64, i64)> = wanted
            .iter()
            .filter(|k| !by_position.contains_key(k))
            .copied()
            .collect();
        for line in corpus.fetch_lines_at_book_positions(&missing)? {
            by_position.insert((line.book_id, line.line_index), (line.id, line.text));
        }
    }

    let mut locator = SnippetWindowLocator::new(context_words);
    let mut joined = String::new();
    for &i in &short {
        let at = results[i];
        let mut previous_ids = Vec::new();
        let mut next_ids = Vec::new();
        let previous = join_side(&by_position, at, -NEIGHBOR_LINE_RADIUS, -1, &mut previous_ids);
        let next = join_side(&by_position, at, 1, NEIGHBOR_LINE_RADIUS, &mut next_ids);
        if previous.is_empty() && next.is_empty() {
            continue;
        }
        let extended = locator.locate_over_neighbour_lines(
            &previous,
            &contents[i],
            &next,
            prepared,
            require_ordered,
            original_group_count,
            &mut joined,
        );
        if !extended.is_match {
            continue; // keep the original window rather than blanking it
        }
        let extended = Window::from(&extended);
        if let Some((hit, window)) = &mut built[i] {
            let mut ids = Vec::with_capacity(previous_ids.len() + 1 + next_ids.len());
            ids.extend(previous_ids);
            ids.push(at.line_id);
            ids.extend(next_ids);
            hit.snippet_line_ids = ids;
            // score / word_distance stay from the matched line
            window.snap_start = extended.snap_start;
            window.snap_end = extended.snap_end;
            window.leading_ellipsis = extended.leading_ellipsis;
            window.trailing_ellipsis = extended.trailing_ellipsis;
            window.marks = extended.marks;
            window.window_word_count = extended.window_word_count;
        }
    }

    Ok(by_position.into_values().collect())
}

/// One side of a short hit's neighbours: the non-empty lines at lineIndex +
/// first..=last (document order), joined by single spaces, ids appended.
fn join_side(
    by_position: &HashMap<(i64, i64), (i64, String)>,
    at: Placed,
    first_delta: i64,
    last_delta: i64,
    ids_out: &mut Vec<i64>,
) -> String {
    let mut joined = String::new();
    for delta in first_delta..=last_delta {
        let Some((id, text)) = by_position.get(&(at.book_id, at.line_index + delta)) else {
            continue;
        };
        if text.is_empty() {
            continue;
        }
        ids_out.push(*id);
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(text);
    }
    joined
}

/// Copies the located window onto the hit; mark term ordinals become
/// search-wide term-table ids.
fn copy_descriptor(hit: &mut Hit, window: &Window, prepared: &PreparedQueryGroups, terms: &mut TermTable) {
    hit.snippet_start = window.snap_start;
    hit.snippet_end = window.snap_end;
    let triples = window.marks.len() / MARK_STRIDE;
    let mut marks = Vec::with_capacity(triples * MARK_STRIDE);
    for t in 0..triples {
        let i = t * MARK_STRIDE;
        marks.push(window.marks[i] as u32);
        marks.push(window.marks[i + 1] as u32);
        marks.push(terms.id_of(prepared.term_at(window.marks[i + 2])));
    }
    hit.snippet_marks = marks;
    hit.snippet_flags = (if window.leading_ellipsis { SNIPPET_FLAG_LEADING_ELLIPSIS } else { 0 })
        | (if window.trailing_ellipsis { SNIPPET_FLAG_TRAILING_ELLIPSIS } else { 0 });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_plan_matches_ftslib() {
        assert_eq!(batch_size_at(0), 256);
        assert_eq!(batch_size_at(15), 256);
        assert_eq!(batch_size_at(16), 512);
        assert_eq!(batch_size_at(17), 1024);
        assert_eq!(batch_size_at(18), 2048);
        assert_eq!(batch_size_at(19), 2048);
        assert_eq!(batch_size_at(100), 2048);
    }

    #[test]
    fn term_table_ids_and_deltas() {
        let mut t = TermTable::default();
        assert_eq!(t.id_of("a"), 0);
        assert_eq!(t.id_of("b"), 1);
        assert_eq!(t.id_of("a"), 0);
        assert_eq!(t.take_new_terms(), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(t.id_of("c"), 2);
        assert_eq!(t.take_new_terms(), vec!["c".to_string()]);
        assert!(t.take_new_terms().is_empty());
    }
}
