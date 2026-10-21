//! Search: one tantivy BooleanQuery of per-group walks, every matching line
//! with its placement in catalog order, and a [`SnippetSession`] holding the
//! groups' matched words (from the sinks) for FtsLib snippet location over
//! the texts the host fetches.

use hebrew_fts::{
    ExactTermsQuery, HebrewFuzzyQuery, HebrewWalkQuery, HebrewWildcardQuery, PreparedQueryGroups,
    Proximity, SnippetWindowLocator, WalkOptions, WordModel, DEFAULT_CONTEXT_WORDS,
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
use crate::options::SearchOptions;
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
    Fuzzy {
        word: String,
        distance: u8,
        /// The quote is transparent (התעלם מגרשיים): distances on the
        /// quote-free forms, as FtsLib's quote-free terms.
        #[serde(default)]
        ignore_quotes: bool,
    },
    /// One slot satisfied by any of its alternatives (FtsLib `a | b`).
    /// Alternatives that cannot search (empty word, rejected wildcard) are
    /// dropped; if none remain, the slot is skipped.
    Any { alternatives: Vec<GroupSpec> },
}

/// Serializable mirror of [`hebrew_fts::TypoMode`]: which typo behaviour
/// `spelling_errors` uses. JSON `"otzaria"` (default) / `"full_edit1"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypoModeSpec {
    /// Exactly Otzaria: the Levenshtein-1 scan for a lone 4+-letter word
    /// with typo as its only option, Otzaria's sampled list otherwise.
    #[default]
    Otzaria,
    /// Edit distance 1 in every combination (≥4 letters).
    #[serde(rename = "full_edit1")]
    FullEdit1,
}

/// Which words are words (see [`hebrew_fts::WordModel`]). JSON `"ftslib"`
/// (default) / `"otzaria"`.
///
/// - `ftslib`: FtsLib's words of 2..=29 letters. One-letter (and 30+
///   letter) query words are dropped, every expansion admits only such
///   index terms, and distances count only such words (byte-identical with
///   FtsLib on either index flavour).
/// - `otzaria`: every word of 1+ letters is a word, as in Otzaria: a
///   one-letter query word is a slot matched at its own position, and
///   distances count every word. Rules evaluated on index positions (span
///   queries) need this model, since the positions count every word: under
///   `ftslib` proximity, phrase alternatives and the negative proximity run
///   as an AND checked on the text instead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WordModelSpec {
    #[default]
    #[serde(rename = "ftslib")]
    FtsLib,
    Otzaria,
}

impl From<WordModelSpec> for WordModel {
    fn from(s: WordModelSpec) -> WordModel {
        match s {
            WordModelSpec::FtsLib => WordModel::FtsLib,
            WordModelSpec::Otzaria => WordModel::Otzaria,
        }
    }
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
    /// Index terms must match as written (off: also by their quote-free form).
    pub exact_quotes: bool,
    pub spelling_errors: bool,
    /// Which typo behaviour `spelling_errors` uses.
    pub typo_mode: TypoModeSpec,
    /// Set by the engine: the word is one of several in the query.
    #[serde(skip)]
    pub phrase_word: bool,
    pub ignore_nikud: bool,
    pub ignore_teamim: bool,
    /// Caller-set limits (Otzaria). Absent = the walk's defaults.
    pub prefix_max_letters: Option<u32>,
    pub suffix_max_letters: Option<u32>,
    pub part_of_word_max_letters: Option<u32>,
    pub grammar_prefix_max_letters: Option<u32>,
}

/// Sets `phrase_word` on every word of `group`.
fn mark_phrase_word(group: &mut GroupSpec) {
    match group {
        GroupSpec::Word { options, .. } => options.phrase_word = true,
        GroupSpec::Any { alternatives } => alternatives.iter_mut().for_each(mark_phrase_word),
        _ => {}
    }
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
            exact_quotes: s.exact_quotes,
            spelling_errors: s.spelling_errors,
            typo_mode: match s.typo_mode {
                TypoModeSpec::Otzaria => hebrew_fts::TypoMode::Otzaria,
                TypoModeSpec::FullEdit1 => hebrew_fts::TypoMode::FullEdit1,
            },
            phrase_word: s.phrase_word,
            ignore_nikud: s.ignore_nikud,
            ignore_teamim: s.ignore_teamim,
            prefix_max_letters: s.prefix_max_letters,
            suffix_max_letters: s.suffix_max_letters,
            part_of_word_max_letters: s.part_of_word_max_letters,
            grammar_prefix_max_letters: s.grammar_prefix_max_letters,
        }
    }
}

/// Otzaria's word proximity (its `Proximity`, `gap_phrase.rs`), an engine
/// rule of its own, next to FtsLib's `max_word_distance` / `require_ordered`.
/// Evaluated with positions on an index that has them (one span query; the
/// rule is checked on each candidate's positions) and validated on the line
/// text by the snippet locator on an index without them (the query is then
/// the AND of the groups). Both give the same lines: the locator checks the
/// rule on the positions-index hits too, and the index positions are the
/// locator's tokens (same scanner).
///
/// JSON: `{"mode":"ordered","gaps":[2,0]}` / `{"mode":"any_order","distance":2}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ProximitySpec {
    /// Groups in query order, at most `gaps[i]` words between group i and
    /// group i+1 (0 = adjacent). One entry per consecutive pair. Otzaria:
    /// `distance` for every pair, or its `customSpacing` per pair.
    Ordered { gaps: Vec<u32> },
    /// Groups in any order, each at its own word, all inside one window of
    /// `(k - 1) * (distance + 1)` positions for k groups (last minus first
    /// position): the span an ordered match with `distance` words between
    /// each pair can take (Otzaria's `Proximity::any_order`). Otzaria
    /// ignores any order when customSpacing is set; the host sends
    /// `Ordered` then.
    AnyOrder { distance: u32 },
    /// Groups in any order, each at its own word, all inside one window of
    /// `max_span` positions (last minus first): what `AnyOrder` resolves
    /// to. [`SearchOptions`](crate::SearchOptions) lowers its typed-word
    /// `AnyOrder` to this when the query has words the engine drops.
    Window { max_span: u32 },
}

impl ProximitySpec {
    /// The locator's rule for `group_count` groups. Errors when `gaps` does
    /// not have one entry per consecutive pair.
    fn resolve(&self, group_count: usize) -> Result<Proximity> {
        Ok(match self {
            ProximitySpec::Ordered { gaps } => {
                if gaps.len() + 1 != group_count {
                    return Err(Error::Tantivy(tantivy::TantivyError::InvalidArgument(format!(
                        "proximity gaps must have one entry per consecutive pair: {} groups, {} gaps",
                        group_count,
                        gaps.len()
                    ))));
                }
                Proximity::Ordered { gaps: gaps.clone() }
            }
            ProximitySpec::AnyOrder { distance } => {
                let pairs = u32::try_from(group_count.saturating_sub(1)).unwrap_or(u32::MAX);
                Proximity::AnyOrder {
                    max_span: pairs.saturating_mul(distance.saturating_add(1)),
                }
            }
            ProximitySpec::Window { max_span } => Proximity::AnyOrder { max_span: *max_span },
        })
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
    /// needs an index with positions. Order follows `require_ordered`. The
    /// positions count every indexed word (one-letter words too), whatever
    /// `word_model`.
    pub gaps: Option<Vec<u32>>,
    /// Which words are words (see [`WordModelSpec`]). Default FtsLib's.
    pub word_model: WordModelSpec,
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
    /// Whole-phrase alternatives ORed with `groups` (Otzaria's acronym
    /// expansion, `acronym_alternatives`: רמב"ם ↔ רבי משה בן מיימון). Each
    /// is a phrase: its words adjacent and in order, all required, whatever
    /// `min_groups` / distance / order the main query uses. A line matching
    /// the main query or any phrase is a hit; `locate` tries the main query
    /// first, then each phrase. With positions a phrase runs as a span
    /// query (strict). Without positions it runs as an AND checked on the
    /// text with FtsLib's distance rule (ordered, distance 0), which is
    /// looser: the tightest window covering the words must be gap-free, so
    /// a repeated word can fill a gap (רבי משה בן רבי נחמן passes).
    pub phrase_alternatives: Vec<Vec<GroupSpec>>,
    /// Otzaria's word proximity (see [`ProximitySpec`]). When set it is the
    /// main query's distance rule and `max_word_distance`,
    /// `require_ordered` and `gaps` are ignored for the main query (they
    /// are FtsLib's rule, unchanged when this is None). Partial matching
    /// (`min_groups` below the group count) wins over it, as in Otzaria.
    /// Every group must be searchable (a skipped slot would shift the
    /// pairs): otherwise the main query matches nothing. (`SearchOptions`
    /// takes the rule per typed word and lowers it here with the dropped
    /// words already cut out.) Phrase
    /// alternatives and the negative query keep their own rules.
    pub proximity: Option<ProximitySpec>,
    /// Filtered search by book: only lines whose `book_id` is in this set
    /// come back (an empty set = no hits). None = every book. Checked per
    /// doc against the `book_id` fast column during collection, so it
    /// applies to the main query, the phrase alternatives and the negative
    /// query alike.
    pub books: Option<Vec<u64>>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            groups: Vec::new(),
            max_word_distance: None,
            require_ordered: false,
            context_words: DEFAULT_CONTEXT_WORDS,
            gaps: None,
            word_model: WordModelSpec::default(),
            min_groups: None,
            proximity: None,
            exclude: None,
            phrase_alternatives: Vec::new(),
            books: None,
        }
    }
}

/// Otzaria's negative query (`apply_advanced_negative_query`): the words to
/// exclude, run as a query of their own with its own distance, order and
/// spacing, always with every word required (a partial negative would veto
/// any line carrying one of its words). A line matching it is dropped.
///
/// How it is applied: one word, or several with no distance/order limit,
/// or per-pair `gaps` (span query, needs positions) are excluded in the
/// query itself. Several words bound by `max_word_distance` /
/// `require_ordered` are checked on the line text when a snippet is
/// located (the same way the positive filters work), so such lines are
/// still in `hits` and come back as `None` from `locate`.
///
/// Section-scoped exclusion (Otzaria's SameSection scope) is the HOST's job,
/// since the index holds no sections: run each negative word as its own
/// search (it returns line ids), map those to sections with SQLite
/// (`line_toc`), intersect the section sets, and drop the positive hits in
/// the remaining sections.
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
    /// Otzaria's proximity for the excluded words (its negative distance /
    /// customSpacing / any order). When set, `max_word_distance`,
    /// `require_ordered` and `gaps` are ignored. With positions it is
    /// excluded in the query (span query); without, on the line text.
    pub proximity: Option<ProximitySpec>,
}

/// One matching line, in catalog order: only the ids the index keeps. The
/// host fetches its text (and any placement: line index, section) from
/// SQLite by `line_id` and locates the snippet through the session.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlacedHit {
    pub position: u64,
    pub line_id: u64,
    pub book_id: u64,
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
    /// The main query's matcher (None when it cannot match), then one per
    /// phrase alternative. `locate` takes the first that passes.
    matchers: Vec<Matcher>,
    /// Main-query settings, used to build its matcher.
    original_group_count: usize,
    require_ordered: bool,
    max_word_distance: usize,
    /// Groups a window must cover (0 = all). See `SearchRequest::min_groups`.
    min_groups: usize,
    context_words: usize,
    keep_marks: bool,
    /// Which words the locators count (the request's word model).
    model: WordModel,
    /// Text-level negative filter (see [`ExcludeSpec`]).
    negative: Option<NegativeFilter>,
    terms: TermTable,
    locator: SnippetWindowLocator,
    /// One locator per `locate_many` worker, kept across batches.
    pool: Vec<SnippetWindowLocator>,
}

/// One way a line can match: prepared terms plus the filters to apply.
struct Matcher {
    prepared: PreparedQueryGroups,
    original_group_count: usize,
    require_ordered: bool,
    max_word_distance: usize,
    min_groups: usize,
    /// Otzaria's proximity: replaces the three FtsLib filters above.
    proximity: Option<Proximity>,
}

impl Matcher {
    /// A passing snippet with marks holding THIS matcher's term ordinals,
    /// over the text the locator last tokenized.
    fn locate(&self, locator: &mut SnippetWindowLocator) -> Option<Snippet> {
        if let Some(proximity) = &self.proximity {
            let d = locator.locate_proximity_tokenized(&self.prepared, proximity);
            return SnippetSession::raw(d, usize::MAX);
        }
        locator.set_min_groups(self.min_groups);
        let d = locator.locate_tokenized(&self.prepared, self.require_ordered, self.original_group_count);
        SnippetSession::raw(d, self.max_word_distance)
    }
}

/// First matcher that passes, with its index.
fn locate_with(
    matchers: &[Matcher],
    negative: Option<&NegativeFilter>,
    locator: &mut SnippetWindowLocator,
    text: &str,
) -> Option<(usize, Snippet)> {
    if text.is_empty() {
        return None; // the locator never matches an empty text
    }
    // Scan the text once; the negative filter and every matcher reuse the
    // tokens (they depend on the text only, not on the query).
    locator.tokenize(text);
    if negative.is_some_and(|n| n.excludes(locator, 0)) {
        return None;
    }
    matchers
        .iter()
        .enumerate()
        .find_map(|(i, m)| m.locate(locator).map(|s| (i, s)))
}

/// The excluded words checked on the line text: a line where they appear
/// within the limits is dropped.
struct NegativeFilter {
    prepared: PreparedQueryGroups,
    max_word_distance: usize,
    require_ordered: bool,
    /// Otzaria's proximity: replaces the two FtsLib limits above.
    proximity: Option<Proximity>,
}

impl NegativeFilter {
    /// True when `text` carries the excluded words within the limits.
    /// Reads the text the locator last tokenized.
    fn excludes(&self, locator: &mut SnippetWindowLocator, min_groups: usize) -> bool {
        if let Some(proximity) = &self.proximity {
            return locator.locate_proximity_tokenized(&self.prepared, proximity).is_match;
        }
        // the negative always needs ALL its words, whatever the positive's mode
        locator.set_min_groups(0);
        let d = locator.locate_tokenized(&self.prepared, self.require_ordered, 0);
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
        let (i, mut s) = locate_with(&self.matchers, self.negative.as_ref(), &mut self.locator, text)?;
        let prepared = &self.matchers[i].prepared;
        for m in s.marks.chunks_exact_mut(3) {
            m[2] = self.terms.id_of(prepared.term_at(m[2] as usize));
        }
        Some(s)
    }

    /// Locates a batch of matched lines across all cores (one locator per
    /// thread; the prepared map is shared read-only), then numbers the marks'
    /// terms in document order. Same results as calling [`locate`](Self::locate)
    /// on each text in turn, term ids included.
    pub fn locate_many(&mut self, texts: &[&str]) -> Vec<Option<Snippet>> {
        if self.matchers.is_empty() {
            return vec![None; texts.len()];
        }
        let matchers = &self.matchers;
        let negative = self.negative.as_ref();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(texts.len().max(1));
        // Snippets with marks still holding the query's term ORDINALS.
        let mut located: Vec<Option<(usize, Snippet)>> = Vec::with_capacity(texts.len());
        if threads <= 1 || texts.len() < 32 {
            for t in texts {
                located.push(locate_with(matchers, negative, &mut self.locator, t));
            }
        } else {
            // Work is split by BYTES, not by text count: locate cost is linear
            // in the text length, and count-based chunks left one thread with
            // the long lines. Contiguous chunks of ~1/8 of a thread's share are
            // handed out through an atomic counter (dynamic balancing), and
            // their results are reassembled by chunk index, so the output
            // order — and with it the term-id numbering below — is exactly
            // the sequential one.
            let total: usize = texts.iter().map(|t| t.len()).sum();
            let target = (total / (threads * 8)).max(1);
            let mut bounds = vec![0usize];
            let mut acc = 0usize;
            for (i, t) in texts.iter().enumerate() {
                acc += t.len();
                if acc >= target && i + 1 < texts.len() {
                    bounds.push(i + 1);
                    acc = 0;
                }
            }
            bounds.push(texts.len());
            let chunks = bounds.len() - 1;
            let next = std::sync::atomic::AtomicUsize::new(0);
            // Locators are kept across calls (one per worker): their scratch
            // buffers are already grown, so a batch allocates nothing per line.
            let (context, keep_marks, model) = (self.context_words, self.keep_marks, self.model);
            while self.pool.len() < threads {
                self.pool.push(SnippetWindowLocator::with_model(context, keep_marks, model));
            }
            let mut parts: Vec<Option<Vec<Option<(usize, Snippet)>>>> = vec![None; chunks];
            std::thread::scope(|scope| {
                let (bounds, next) = (&bounds, &next);
                let handles: Vec<_> = self.pool[..threads]
                    .iter_mut()
                    .map(|locator| {
                        scope.spawn(move || {
                            let mut done = Vec::new();
                            loop {
                                let c = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if c >= chunks {
                                    break;
                                }
                                let part = &texts[bounds[c]..bounds[c + 1]];
                                let res: Vec<_> =
                                    part.iter().map(|t| locate_with(matchers, negative, locator, t)).collect();
                                done.push((c, res));
                            }
                            done
                        })
                    })
                    .collect();
                for h in handles {
                    for (c, res) in h.join().expect("snippet thread panicked") {
                        parts[c] = Some(res);
                    }
                }
            });
            for p in parts {
                located.extend(p.expect("every chunk is located"));
            }
        }
        // Ordinals → search-wide term ids, in document order (deterministic).
        located
            .into_iter()
            .map(|r| {
                r.map(|(i, mut s)| {
                    let prepared = &self.matchers[i].prepared;
                    for m in s.marks.chunks_exact_mut(3) {
                        m[2] = self.terms.id_of(prepared.term_at(m[2] as usize));
                    }
                    s
                })
            })
            .collect()
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
        self.matchers.first().map(|m| m.prepared.group_count()).unwrap_or(0)
    }
}

/// What a search returns: every matching line in catalog order, and the
/// session to locate their snippets.
pub struct SearchResult {
    pub hits: Vec<PlacedHit>,
    pub session: SnippetSession,
}

// ── Collector: every matching doc with its placement, from fast fields ───

struct PlacedCollector {
    /// Book filter (see `SearchRequest::books`).
    books: Option<Arc<std::collections::HashSet<u64>>>,
}

struct PlacedSegmentCollector {
    columns: [Column<u64>; 3],
    books: Option<Arc<std::collections::HashSet<u64>>>,
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
            ],
            books: self.books.clone(),
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
        let book_id = get(&self.columns[2]);
        if self.books.as_ref().is_some_and(|b| !b.contains(&book_id)) {
            return;
        }
        self.out.push(PlacedHit {
            position: get(&self.columns[0]),
            line_id: get(&self.columns[1]),
            book_id,
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

impl LineIndex {
    /// The span slot for one group, reporting into `sink`. None when the
    /// group cannot search.
    fn group_slot(
        &self,
        group: &GroupSpec,
        sink: &MatchedTermsSink,
        searcher: &tantivy::Searcher,
        fm: Option<&FmIndex>,
        model: WordModel,
    ) -> Result<Option<Arc<dyn SpanTermSource>>> {
        let field = self.fields.text;
        if let Some(terms) = self.fm_terms(fm, searcher, group, model)? {
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
                q.set_word_model(model);
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
                q.set_word_model(model);
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Grammar { word, prefixes, suffixes, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = hebrew_fts::HebrewGrammarQuery::new(field, word.clone(), *prefixes, *suffixes);
                q.set_word_model(model);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Fuzzy { word, distance, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewFuzzyQuery::new(field, word.clone(), *distance);
                q.set_word_model(model);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Any { alternatives } => {
                let mut sources = Vec::new();
                for a in alternatives {
                    if let Some(s) = self.group_slot(a, sink, searcher, fm, model)? {
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

    /// The span query over `groups` with per-pair `gaps` (`ordered` or
    /// pairwise either order), or, with `window`, all groups in any order
    /// inside one window of that many positions (gaps then unused). Every
    /// group must be searchable (a skipped group would shift the gaps);
    /// None when one is not.
    fn span_query(
        &self,
        groups: &[GroupSpec],
        gaps: &[u32],
        ordered: bool,
        window: Option<u32>,
        searcher: &tantivy::Searcher,
        fm: Option<&FmIndex>,
        model: WordModel,
        sinks: &mut Vec<MatchedTermsSink>,
    ) -> Result<Option<SpanQuery>> {
        if !self.options.positions {
            return Err(Error::Tantivy(tantivy::TantivyError::InvalidArgument(
                "per-pair word spacing needs an index built with positions".into(),
            )));
        }
        if window.is_none() && gaps.len() + 1 != groups.len() {
            return Err(Error::Tantivy(tantivy::TantivyError::InvalidArgument(format!(
                "gaps must have one entry per consecutive pair: {} groups, {} gaps",
                groups.len(),
                gaps.len()
            ))));
        }
        let mut span: Option<SpanQuery> = None;
        for (i, group) in groups.iter().enumerate() {
            let sink = MatchedTermsSink::new();
            let Some(slot) = self.group_slot(group, &sink, searcher, fm, model)? else {
                return Ok(None);
            };
            sinks.push(sink);
            span = Some(match span {
                None => SpanQuery::new(self.fields.text, slot),
                Some(s) => s.then_within(gaps.get(i - 1).copied().unwrap_or(0), slot),
            });
        }
        let mut span = span.expect("at least one group");
        span.set_ordered(ordered);
        span.set_window(window);
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
        model: WordModel,
    ) -> Result<Option<Box<dyn Query>>> {
        if let Some(terms) = self.fm_terms(fm, searcher, group, model)? {
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
                q.set_word_model(model);
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
                q.set_word_model(model);
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
                q.set_word_model(model);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_matched_terms_sink(sink.clone());
                Some(Box::new(q))
            }
            GroupSpec::Fuzzy { word, distance, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewFuzzyQuery::new(self.fields.text, word.clone(), *distance);
                q.set_word_model(model);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_matched_terms_sink(sink.clone());
                Some(Box::new(q))
            }
            GroupSpec::Any { alternatives } => {
                let mut parts: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for a in alternatives {
                    if let Some(q) = self.group_query(a, sink, searcher, fm, model)? {
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
        model: WordModel,
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
                    model,
                )?;
                Ok(Some(terms))
            }
            GroupSpec::Fuzzy { word, distance, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewFuzzyQuery::new(field, word.clone(), *distance);
                q.set_word_model(model);
                q.set_ignore_quotes(*ignore_quotes);
                let inverted = dict()?;
                let terms = if *ignore_quotes {
                    fm_expand::expand_fuzzy_quote_free(
                        fm,
                        inverted.terms(),
                        q.word(),
                        q.distance_limit(),
                        q.max_expansions(),
                        model,
                    )?
                } else {
                    fm_expand::expand_fuzzy(
                        fm,
                        inverted.terms(),
                        &q.automaton(),
                        q.word(),
                        q.distance_limit(),
                        q.max_expansions(),
                        model,
                    )?
                };
                Ok(Some(terms))
            }
            _ => Ok(None),
        }
    }

    /// Runs a search given as [`SearchOptions`] (the public API): the query
    /// string is parsed by FtsLib's rules and each word becomes one walk with
    /// its options (global, overridden per word).
    pub fn search_options(&self, options: &SearchOptions) -> Result<SearchResult> {
        self.search(&options.to_request(self.options.keep_marks)?)
    }

    /// Runs `request`. Every matching line comes back in catalog order;
    /// the host fetches texts (in its own batches) and locates snippets
    /// through the returned session, which applies the word-distance and
    /// order filters.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResult> {
        // Otzaria's word options know whether the word stands alone (its
        // single-word vs phrase budgets): mark the words of a multi-word query.
        let phrase;
        let request = if request.groups.len() > 1 {
            let mut r = request.clone();
            r.groups.iter_mut().for_each(mark_phrase_word);
            phrase = r;
            &phrase
        } else {
            request
        };
        let keep_marks = self.options.keep_marks;
        let model: WordModel = request.word_model.into();
        let locator = SnippetWindowLocator::with_model(request.context_words, keep_marks, model);
        // Index positions count every word; the text counts the model's
        // words. Span queries (rules read off the positions) therefore run
        // only when the two agree: Otzaria's model. Otherwise those rules
        // run as an AND checked on the text, as without positions.
        let spans = self.options.positions && model == WordModel::Otzaria;
        // Partial matching: at least `min` of the searchable groups. Clamped
        // to 1..n like Otzaria's WordMatch::min_words; n or more = all. (n
        // counts the request's groups; Otzaria counts distinct materialized
        // words, which differs only for duplicate words or rejected wildcards.)
        let partial_min = request
            .min_groups
            .filter(|&k| k < request.groups.len())
            .map(|k| k.max(1));
        let partial = partial_min.is_some();
        // Otzaria's proximity (partial matching wins over it).
        let proximity = match (&request.proximity, partial) {
            (Some(p), false) if !request.groups.is_empty() => Some(p.resolve(request.groups.len())?),
            _ => None,
        };
        let mut session = SnippetSession {
            matchers: Vec::new(),
            original_group_count: request.groups.len(),
            require_ordered: request.require_ordered && !partial,
            max_word_distance: if partial { usize::MAX } else { request.max_word_distance.unwrap_or(usize::MAX) },
            context_words: request.context_words,
            keep_marks,
            model,
            min_groups: 0,
            negative: None,
            terms: TermTable::default(),
            locator,
            pool: Vec::new(),
        };

        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let fm = self.fm_sidecar_for(&searcher);
        let fm = fm.as_deref();
        let mut sinks = Vec::new();

        let main_query: Option<Box<dyn Query>> = if let Some(prox) = &proximity {
            if spans && request.groups.len() > 1 {
                // One span query: the rule checked on each candidate's positions.
                let (gaps, window) = match prox {
                    Proximity::Ordered { gaps } => (gaps.clone(), None),
                    Proximity::AnyOrder { max_span } => (Vec::new(), Some(*max_span)),
                };
                self.span_query(&request.groups, &gaps, true, window, &searcher, fm, model, &mut sinks)?
                    .map(|q| Box::new(q) as Box<dyn Query>)
            } else {
                // Without positions: every group present; the locator
                // validates the rule on the text. All groups must search.
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                let mut complete = true;
                for group in &request.groups {
                    let sink = MatchedTermsSink::new();
                    match self.group_query(group, &sink, &searcher, fm, model)? {
                        Some(q) => {
                            clauses.push((Occur::Must, q));
                            sinks.push(sink);
                        }
                        None => complete = false,
                    }
                }
                if !complete {
                    sinks.clear();
                }
                (complete && !clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
            }
        } else if let (Some(gaps), false) = (&request.gaps, partial) {
            // Per-pair spacing: a span query, one slot per group, each gap its own limit.
            self.span_query(&request.groups, gaps, request.require_ordered, None, &searcher, fm, model, &mut sinks)?
                .map(|q| Box::new(q) as Box<dyn Query>)
        } else {
            // One query + one sink per searchable slot. Rejected wildcards skip
            // their slot (it still counts in original_group_count).
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for group in &request.groups {
                let sink = MatchedTermsSink::new();
                if let Some(q) = self.group_query(group, &sink, &searcher, fm, model)? {
                    clauses.push((Occur::Must, q));
                    sinks.push(sink);
                }
            }
            if clauses.is_empty() {
                None
            } else {
                Some(match partial_min {
                    Some(min) if min < clauses.len() => {
                        let should: Vec<(Occur, Box<dyn Query>)> =
                            clauses.into_iter().map(|(_, q)| (Occur::Should, q)).collect();
                        Box::new(BooleanQuery::with_minimum_required_clauses(should, min))
                            as Box<dyn Query>
                    }
                    _ => Box::new(BooleanQuery::new(clauses)),
                })
            }
        };

        // Phrase alternatives (Otzaria's acronym expansions): each an
        // adjacent, ordered, all-words phrase, ORed with the main query.
        let mut alternatives: Vec<(Vec<MatchedTermsSink>, Box<dyn Query>)> = Vec::new();
        for phrase in &request.phrase_alternatives {
            if phrase.is_empty() {
                continue;
            }
            let mut alt_sinks = Vec::new();
            let q: Option<Box<dyn Query>> = if spans && phrase.len() > 1 {
                self.span_query(phrase, &vec![0; phrase.len() - 1], true, None, &searcher, fm, model, &mut alt_sinks)?
                    .map(|q| Box::new(q) as Box<dyn Query>)
            } else {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                let mut complete = true;
                for group in phrase {
                    let sink = MatchedTermsSink::new();
                    match self.group_query(group, &sink, &searcher, fm, model)? {
                        Some(q) => {
                            clauses.push((Occur::Must, q));
                            alt_sinks.push(sink);
                        }
                        None => complete = false,
                    }
                }
                (complete && !clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
            };
            if let Some(q) = q {
                alternatives.push((alt_sinks, q));
            }
        }
        let query: Box<dyn Query> = match (main_query, alternatives.is_empty()) {
            (None, true) => return Ok(SearchResult { hits: Vec::new(), session }),
            (Some(q), true) => q,
            (main, false) => {
                let mut should: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                if let Some(q) = main {
                    should.push((Occur::Should, q));
                }
                for (_, q) in &alternatives {
                    should.push((Occur::Should, q.box_clone()));
                }
                Box::new(BooleanQuery::new(should))
            }
        };

        // The negative query (see ExcludeSpec): excluded in the query itself,
        // or on the text at locate time.
        let mut negative_sinks: Vec<MatchedTermsSink> = Vec::new();
        let mut text_level_negative: Option<(usize, bool, Option<Proximity>)> = None;
        let mut query = query;
        if let Some(ex) = &request.exclude {
            let ex_proximity = match &ex.proximity {
                Some(p) if ex.groups.len() > 1 => Some(p.resolve(ex.groups.len())?),
                _ => None,
            };
            let negative: Option<Box<dyn Query>> = if let (Some(prox), true) = (&ex_proximity, spans) {
                let (gaps, window) = match prox {
                    Proximity::Ordered { gaps } => (gaps.clone(), None),
                    Proximity::AnyOrder { max_span } => (Vec::new(), Some(*max_span)),
                };
                self.span_query(&ex.groups, &gaps, true, window, &searcher, fm, model, &mut negative_sinks)?
                    .map(|q| Box::new(q) as Box<dyn Query>)
            } else if let (Some(gaps), None) = (&ex.gaps, &ex.proximity) {
                self.span_query(&ex.groups, gaps, ex.require_ordered, None, &searcher, fm, model, &mut negative_sinks)?
                    .map(|q| Box::new(q) as Box<dyn Query>)
            } else {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for group in &ex.groups {
                    let sink = MatchedTermsSink::new();
                    if let Some(q) = self.group_query(group, &sink, &searcher, fm, model)? {
                        clauses.push((Occur::Must, q));
                        negative_sinks.push(sink);
                    }
                }
                (!clauses.is_empty()).then(|| Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>)
            };
            if let Some(negative) = negative {
                let needs_text = if ex_proximity.is_some() {
                    // without positions the rule is checked on the text; a
                    // skipped word would shift the pairs: no exclusion then
                    !spans && negative_sinks.len() == ex.groups.len()
                } else {
                    let bound = ex.max_word_distance.is_some() || ex.require_ordered;
                    ex.gaps.is_none() && negative_sinks.len() > 1 && bound
                };
                let skip = ex_proximity.is_some() && !spans && !needs_text;
                if needs_text {
                    // Build the scorers once so the walks report the excluded
                    // forms; the docs themselves are not needed.
                    let w = negative.weight(tantivy::query::EnableScoring::disabled_from_searcher(&searcher))?;
                    for segment in searcher.segment_readers() {
                        let _ = w.scorer(segment, 1.0)?;
                    }
                    text_level_negative =
                        Some((ex.max_word_distance.unwrap_or(usize::MAX), ex.require_ordered, ex_proximity.clone()));
                } else if !skip {
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
        let collector = PlacedCollector {
            books: request.books.as_ref().map(|b| Arc::new(b.iter().copied().collect())),
        };
        for (ord, segment) in searcher.segment_readers().iter().enumerate() {
            let mut child = collector.for_segment(ord as SegmentOrdinal, segment)?;
            // `for_each_no_score` does NOT skip deleted docs (tantivy's own
            // collectors check the alive bitset themselves). Without this an
            // updated line (delete + add) came back in its OLD version too,
            // whenever its segment kept other live docs.
            match segment.alive_bitset() {
                Some(alive) => weight.for_each_no_score(segment, &mut |docs| {
                    for &doc in docs {
                        if alive.is_alive(doc) {
                            child.collect(doc, 0.0);
                        }
                    }
                })?,
                None => weight.for_each_no_score(segment, &mut |docs| {
                    for &doc in docs {
                        child.collect(doc, 0.0);
                    }
                })?,
            }
            hits.extend(child.harvest());
        }
        if timing {
            eprintln!("  [timing] collect {} hits: {:.1} ms", hits.len(), t.elapsed().as_secs_f64() * 1000.0);
        }
        hits.sort_unstable_by_key(|h| h.position);
        if let Some((max_word_distance, require_ordered, proximity)) = text_level_negative {
            let negative_terms: Vec<Vec<String>> = negative_sinks.iter().map(|s| s.take_strings()).collect();
            // a word with no indexed form can never be present: no exclusion
            if negative_terms.iter().all(|g| !g.is_empty()) {
                session.negative = Some(NegativeFilter {
                    prepared: PreparedQueryGroups::from_groups(&negative_terms),
                    max_word_distance,
                    require_ordered,
                    proximity,
                });
            }
        }

        // The main query's matcher, if it can match at all.
        let mut group_terms: Vec<Vec<String>> = sinks.iter().map(|s| s.take_strings()).collect();
        let main_ok = if group_terms.is_empty() {
            false
        } else if let Some(min) = partial_min {
            // a group that matched no indexed word simply cannot be one of
            // the `min`; the others still can
            group_terms.retain(|g| !g.is_empty());
            session.min_groups = min;
            group_terms.len() >= min
        } else {
            // a group that matched no indexed word: the main query cannot match
            group_terms.iter().all(|g| !g.is_empty())
        };
        if main_ok {
            session.matchers.push(Matcher {
                prepared: PreparedQueryGroups::from_groups(&group_terms),
                original_group_count: session.original_group_count,
                require_ordered: session.require_ordered,
                max_word_distance: session.max_word_distance,
                min_groups: session.min_groups,
                proximity: proximity.clone(),
            });
        }
        for (alt_sinks, _) in &alternatives {
            let terms: Vec<Vec<String>> = alt_sinks.iter().map(|s| s.take_strings()).collect();
            if terms.iter().all(|g| !g.is_empty()) {
                session.matchers.push(Matcher {
                    original_group_count: terms.len(),
                    prepared: PreparedQueryGroups::from_groups(&terms),
                    require_ordered: true,
                    max_word_distance: 0,
                    min_groups: 0,
                    proximity: None,
                });
            }
        }
        if session.matchers.is_empty() {
            return Ok(SearchResult { hits: Vec::new(), session });
        }
        Ok(SearchResult { hits, session })
    }
}
