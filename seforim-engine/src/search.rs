//! Search: one tantivy BooleanQuery of per-group walks, every matching line
//! with its placement in catalog order (or the requested order), and a [`SnippetSession`] holding the
//! groups' matched words (from the sinks) for FtsLib snippet location over
//! the texts the host fetches.

use hebrew_fts::{
    ExactTermsQuery, HebrewFuzzyQuery, HebrewRegexQuery, HebrewWalkQuery, HebrewWildcardQuery, PreparedQueryGroups,
    Proximity, SnippetWindowLocator, WalkOptions, DEFAULT_CONTEXT_WORDS,
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
use crate::relevance::{self, ResultsOrder};
use crate::{Error, Result};

/// Cancels a running search from another thread (Otzaria's
/// `cancel_lexical_search`). Cheap to clone; every clone shares one flag. A
/// token cannot be reset: a new search takes a new token. Checked before
/// the query weight (the walks), per segment, every 4096 collected docs,
/// and per chunk of every snippet locate / count batch; a cancelled call
/// returns [`Error::Cancelled`].
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    /// A token that is not cancelled.
    pub fn new() -> CancelToken {
        CancelToken::default()
    }

    /// Asks the search holding this token to stop. Returns at once.
    pub fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether [`cancel`](Self::cancel) has been called.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn check(token: Option<&CancelToken>) -> Result<()> {
        if token.is_some_and(|t| t.is_cancelled()) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }
}

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
    /// A literal word too long to be indexed (its term has
    /// `MAX_TERM_BYTES` bytes or more): it matches no term. It keeps its
    /// one position: a skipped slot that still counts toward word distance
    /// (as a rejected wildcard); a proximity rule counts it as one word
    /// between its neighbours (`SearchOptions` lowers it).
    Unindexed { word: String },
    /// One word of a user regex `/.../` (Otzaria's term kinds): a whole-term
    /// regex walked over the dictionary, quote transparent, capped shortest
    /// first. `pattern` is a word of [`hebrew_fts::UserRegex`].
    Regex { pattern: String },
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
    /// Otzaria's `match_nikud`: typed nikud required on its letter, untyped
    /// marks free (needs an index that keeps marks).
    pub match_nikud: bool,
    /// Otzaria's `match_taamim`: the same for te'amim.
    pub match_teamim: bool,
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
            match_nikud: s.match_nikud,
            match_teamim: s.match_teamim,
            marked_terms: false,
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
    /// The order of the hits: catalog (default, costs nothing), Otzaria's
    /// relevance, or Otzaria's generation order (see `relevance`).
    pub order: ResultsOrder,
    /// For the generation order: `(book_id, generation_order)` per book,
    /// from the host (Otzaria's `generationOrder`); books absent come last.
    pub book_generations: Vec<(u64, u32)>,
    /// Otzaria's vocalized mode (`match_nikud` / `match_taamim` on, for the
    /// query or for any of its words): only lines that carry an attached
    /// mark can match, as Otzaria then searches its vocalized field
    /// (`textVocalized`), which holds only those lines. Applies to the main
    /// query and the phrase alternatives. Needs an index built with
    /// `keep_marks` (it flags those lines).
    pub vocalized: bool,
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
            proximity: None,
            exclude: None,
            phrase_alternatives: Vec::new(),
            books: None,
            order: ResultsOrder::Catalog,
            book_generations: Vec::new(),
            vocalized: false,
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
    /// Whole phrases ORed with the negative query (Otzaria's acronym
    /// expansion of the negative words), each as
    /// [`SearchRequest::phrase_alternatives`]: adjacent, in order, all
    /// words. A line carrying one is dropped: in the query (one word, or a
    /// span query with positions) or, without positions, on the text.
    pub phrase_alternatives: Vec<Vec<GroupSpec>>,
    /// Vocalized negative (Otzaria builds the negative on its vocalized
    /// field when nikud / te'amim matching is on for it): only a line that
    /// carries an attached mark is excluded (its phrase alternatives too).
    /// Needs `keep_marks`.
    pub vocalized: bool,
}

/// One matching line (hits come in the requested order, catalog by default): only the ids the index keeps. The
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
    /// Text-level negative filters (see [`ExcludeSpec`]): the negative
    /// query and its phrase alternatives; a line any of them finds is dropped.
    negative: Vec<NegativeFilter>,
    terms: TermTable,
    locator: SnippetWindowLocator,
    /// One locator per `locate_many` worker, kept across batches.
    pool: Vec<SnippetWindowLocator>,
    /// True when some hits may fail the text-level checks (FtsLib's
    /// distance / order, proximity without positions, a looser phrase
    /// alternative, the text-level negative): an exact count then needs
    /// the lines' texts. False: every hit passes, the hit list is exact.
    count_needs_text: bool,
    /// The search's cancel token (None: not cancellable).
    cancel: Option<CancelToken>,
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
    negative: &[NegativeFilter],
    locator: &mut SnippetWindowLocator,
    text: &str,
) -> Option<(usize, Snippet)> {
    if text.is_empty() {
        return None; // the locator never matches an empty text
    }
    // Scan the text once; the negative filter and every matcher reuse the
    // tokens (they depend on the text only, not on the query).
    locator.tokenize(text);
    if negative.iter().any(|n| {
        // a vocalized negative sees only lines that carry a mark (Otzaria's
        // vocalized field)
        (!n.vocalized || hebrew_fts::chars::contains_attached_marks(text)) && n.excludes(locator, 0)
    }) {
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
    /// Vocalized (see [`ExcludeSpec::vocalized`]): only a line carrying a
    /// mark can be excluded.
    vocalized: bool,
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
        let (i, mut s) = locate_with(&self.matchers, &self.negative, &mut self.locator, text)?;
        let prepared = &self.matchers[i].prepared;
        for m in s.marks.chunks_exact_mut(3) {
            m[2] = self.terms.id_of(prepared.term_at(m[2] as usize));
        }
        Some(s)
    }

    /// Locates a batch of matched lines across all cores (one locator per
    /// thread; the prepared map is shared read-only), then numbers the marks'
    /// terms in document order. Same results as calling [`locate`](Self::locate)
    /// on each text in turn, term ids included. Once the search's cancel
    /// token is cancelled every entry is None; use
    /// [`try_locate_many`](Self::try_locate_many) to see the cancellation.
    pub fn locate_many(&mut self, texts: &[&str]) -> Vec<Option<Snippet>> {
        let n = texts.len();
        self.try_locate_many(texts).unwrap_or_else(|_| vec![None; n])
    }

    /// [`locate_many`](Self::locate_many), stopping with
    /// [`Error::Cancelled`] when the search's token is cancelled (checked
    /// per chunk of work).
    pub fn try_locate_many(&mut self, texts: &[&str]) -> Result<Vec<Option<Snippet>>> {
        if self.matchers.is_empty() {
            return Ok(vec![None; texts.len()]);
        }
        // Snippets with marks still holding the query's term ORDINALS.
        let located = self.run_many(texts, locate_with)?;
        // Ordinals -> search-wide term ids, in document order (deterministic).
        Ok(located
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
            .collect())
    }

    /// Whether each text passes (the same test as [`locate`](Self::locate)
    /// returning Some), across all cores; the term table is not touched.
    pub fn passes_many(&mut self, texts: &[&str]) -> Result<Vec<bool>> {
        if self.matchers.is_empty() {
            return Ok(vec![false; texts.len()]);
        }
        self.run_many(texts, |m, n, l, t| locate_with(m, n, l, t).is_some())
    }

    /// True when an exact count needs the hits' texts (see
    /// [`SearchResult::count_batch`]); false when every hit passes.
    pub fn count_needs_text(&self) -> bool {
        self.count_needs_text
    }

    /// Runs `f` on every text, in parallel for large batches (one locator
    /// per worker, kept across calls), results in input order. Stops with
    /// [`Error::Cancelled`] when the token is cancelled (checked per chunk).
    fn run_many<T: Send>(
        &mut self,
        texts: &[&str],
        f: impl Fn(&[Matcher], &[NegativeFilter], &mut SnippetWindowLocator, &str) -> T + Sync,
    ) -> Result<Vec<T>> {
        let cancel = self.cancel.as_ref();
        CancelToken::check(cancel)?;
        let matchers = &self.matchers;
        let negative = self.negative.as_slice();
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(texts.len().max(1));
        let mut located: Vec<T> = Vec::with_capacity(texts.len());
        if threads <= 1 || texts.len() < 32 {
            for t in texts {
                located.push(f(matchers, negative, &mut self.locator, t));
            }
            CancelToken::check(cancel)?;
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
            let (context, keep_marks) = (self.context_words, self.keep_marks);
            while self.pool.len() < threads {
                self.pool.push(if keep_marks {
                    SnippetWindowLocator::keeping_marks(context)
                } else {
                    SnippetWindowLocator::new(context)
                });
            }
            let mut parts: Vec<Option<Vec<T>>> = (0..chunks).map(|_| None).collect();
            let f = &f;
            std::thread::scope(|scope| {
                let (bounds, next) = (&bounds, &next);
                let handles: Vec<_> = self.pool[..threads]
                    .iter_mut()
                    .map(|locator| {
                        scope.spawn(move || {
                            let mut done = Vec::new();
                            loop {
                                let c = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                if c >= chunks || cancel.is_some_and(|t| t.is_cancelled()) {
                                    break;
                                }
                                let part = &texts[bounds[c]..bounds[c + 1]];
                                let res: Vec<_> =
                                    part.iter().map(|t| f(matchers, negative, locator, t)).collect();
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
                // a chunk left undone: the token was cancelled
                located.extend(p.ok_or(Error::Cancelled)?);
            }
        }
        Ok(located)
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

/// What a search returns: every matching line in the requested order
/// (catalog order by default), and the session to locate their snippets.
pub struct SearchResult {
    pub hits: Vec<PlacedHit>,
    pub session: SnippetSession,
    /// Text validation progress for the exact count (see `count_batch`).
    tally: CountTally,
    /// Relevance order only: each hit's score, aligned with `hits`.
    scores: Option<Vec<Score>>,
    /// Notices about how the search was served (see [`notices`](Self::notices)).
    notices: Vec<String>,
}

/// How a search was served (see [`SearchResult::info`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchInfo {
    /// See [`SearchResult::notices`].
    pub notices: Vec<String>,
}

/// Exact totals of a search: the lines that pass every filter (including
/// the distance / order / proximity / negative checks), in all and per
/// book (Otzaria's `count` / `count_by_book`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitCounts {
    /// Lines that passed, among the hits counted so far.
    pub total: u64,
    /// `(book_id, lines)`, ascending book id; books with no line absent.
    pub books: Vec<(u64, u64)>,
    /// Hits counted so far (from the first, in hit order).
    pub validated: u64,
    /// True when every hit is counted: `total` and `books` are final.
    pub exact: bool,
}

#[derive(Debug, Default)]
struct CountTally {
    validated: usize,
    books: std::collections::BTreeMap<u64, u64>,
    total: u64,
}

impl SearchResult {
    fn new(hits: Vec<PlacedHit>, session: SnippetSession) -> SearchResult {
        SearchResult { hits, session, tally: CountTally::default(), scores: None, notices: Vec::new() }
    }

    fn with_notices(mut self, notices: Vec<String>) -> SearchResult {
        self.notices = notices;
        self
    }

    /// Notices about how the search was served, not errors. Today one:
    /// [`NOTICE_RELEVANCE_NEEDS_POSITIONS`](crate::relevance::NOTICE_RELEVANCE_NEEDS_POSITIONS)
    /// (relevance order asked for a phrase / proximity / multi-word fuzzy
    /// query on an index without positions: the hits are in catalog order).
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// How the search was served (the C ABI's `se_search_info`).
    pub fn info(&self) -> SearchInfo {
        SearchInfo { notices: self.notices.clone() }
    }

    /// The relevance score of each hit (aligned with `hits`), when the
    /// search was ordered by relevance.
    pub fn scores(&self) -> Option<&[Score]> {
        self.scores.as_deref()
    }

    /// True when the exact count needs the hits' texts: feed them in
    /// hit order with [`count_batch`](Self::count_batch). False: every
    /// hit passes and [`counts`](Self::counts) is exact at once.
    pub fn count_needs_text(&self) -> bool {
        self.session.count_needs_text && !self.hits.is_empty()
    }

    /// Validates the texts of the next `texts.len()` hits (hit order,
    /// continuing from the previous batch) across all cores and adds the
    /// passing ones to the counts. Returns how many passed. The test is
    /// exactly [`SnippetSession::locate`]'s. When no text is needed the
    /// texts are not read (every hit passes).
    pub fn count_batch(&mut self, texts: &[&str]) -> Result<usize> {
        let start = self.tally.validated;
        let end = start + texts.len();
        if end > self.hits.len() {
            return Err(Error::Unsupported(format!(
                "count_batch: {} texts past the last hit ({} of {} counted)",
                texts.len(),
                start,
                self.hits.len()
            )));
        }
        let pass = if self.session.count_needs_text {
            self.session.passes_many(texts)?
        } else {
            CancelToken::check(self.session.cancel.as_ref())?;
            vec![true; texts.len()]
        };
        let mut passed = 0;
        for (hit, ok) in self.hits[start..end].iter().zip(pass) {
            if ok {
                *self.tally.books.entry(hit.book_id).or_default() += 1;
                passed += 1;
            }
        }
        self.tally.total += passed as u64;
        self.tally.validated = end;
        Ok(passed)
    }

    /// The counts so far. Exact at once when no text is needed (every hit
    /// passes: counted from the hits' book ids, no text read); otherwise
    /// exact once [`count_batch`](Self::count_batch) has seen every hit.
    pub fn counts(&self) -> HitCounts {
        if !self.count_needs_text() {
            let mut books = std::collections::BTreeMap::<u64, u64>::new();
            for h in &self.hits {
                *books.entry(h.book_id).or_default() += 1;
            }
            return HitCounts {
                total: self.hits.len() as u64,
                books: books.into_iter().collect(),
                validated: self.hits.len() as u64,
                exact: true,
            };
        }
        HitCounts {
            total: self.tally.total,
            books: self.tally.books.iter().map(|(&b, &n)| (b, n)).collect(),
            validated: self.tally.validated as u64,
            exact: self.tally.validated == self.hits.len(),
        }
    }
}

// ── Collector: every matching doc with its placement, from fast fields ───

struct PlacedCollector {
    /// Book filter (see `SearchRequest::books`).
    books: Option<Arc<std::collections::HashSet<u64>>>,
    /// Keep each hit's doc id (relevance order scores them after the search).
    keep_docs: bool,
}

struct PlacedSegmentCollector {
    columns: [Column<u64>; 3],
    books: Option<Arc<std::collections::HashSet<u64>>>,
    out: Vec<PlacedHit>,
    /// Relevance order only: each kept hit's doc id (aligned with `out`).
    docs: Option<Vec<DocId>>,
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
            docs: self.keep_docs.then(Vec::new),
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
        if let Some(docs) = &mut self.docs {
            docs.push(doc);
        }
    }

    fn harvest(self) -> Vec<PlacedHit> {
        self.out
    }
}

impl PlacedSegmentCollector {
    /// The doc ids kept (relevance order), aligned with the hits.
    fn take_docs(&mut self) -> Vec<DocId> {
        self.docs.take().unwrap_or_default()
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
                let mut q = HebrewWalkQuery::new(field, word.clone(), self.walk_options(options));
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
                q.set_marked_terms(self.options.keep_marks);
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Grammar { word, prefixes, suffixes, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = hebrew_fts::HebrewGrammarQuery::new(field, word.clone(), *prefixes, *suffixes);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_marked_terms(self.options.keep_marks);
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Fuzzy { word, distance, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewFuzzyQuery::new(field, word.clone(), *distance);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_marked_terms(self.options.keep_marks);
                q.set_matched_terms_sink(sink.clone());
                Some(q.span_slot(searcher)?)
            }
            GroupSpec::Unindexed { .. } => None,
            GroupSpec::Regex { pattern } => {
                let mut q = HebrewRegexQuery::new(field, pattern).map_err(|e| Error::Unsupported(e.to_string()))?;
                q.set_marked_terms(self.options.keep_marks);
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
        let first_sink = sinks.len();
        for (i, group) in groups.iter().enumerate() {
            let sink = MatchedTermsSink::new();
            let Some(slot) = self.group_slot(group, &sink, searcher, fm)? else {
                // no partial matcher over the groups before this one
                sinks.truncate(first_sink);
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
                let mut q = HebrewWalkQuery::new(self.fields.text, word.clone(), self.walk_options(options));
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
                q.set_marked_terms(self.options.keep_marks);
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
                q.set_marked_terms(self.options.keep_marks);
                q.set_matched_terms_sink(sink.clone());
                Some(Box::new(q))
            }
            GroupSpec::Fuzzy { word, distance, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewFuzzyQuery::new(self.fields.text, word.clone(), *distance);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_marked_terms(self.options.keep_marks);
                q.set_matched_terms_sink(sink.clone());
                Some(Box::new(q))
            }
            GroupSpec::Unindexed { .. } => None,
            GroupSpec::Regex { pattern } => {
                let mut q =
                    HebrewRegexQuery::new(self.fields.text, pattern).map_err(|e| Error::Unsupported(e.to_string()))?;
                q.set_marked_terms(self.options.keep_marks);
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

    /// `query` restricted to the lines that carry an attached mark (the
    /// `marked` flag of a `keep_marks` index).
    fn marked_lines_only(&self, query: Box<dyn Query>) -> Result<Box<dyn Query>> {
        let field = self.marked_field()?;
        let marked = tantivy::query::TermQuery::new(
            tantivy::Term::from_field_u64(field, crate::index::MARKED_LINE),
            tantivy::schema::IndexRecordOption::Basic,
        );
        Ok(Box::new(BooleanQuery::new(vec![(Occur::Must, query), (Occur::Must, Box::new(marked))])))
    }

    /// The `marked` line flag, or the error a vocalized search gets on an
    /// index that cannot serve it.
    fn marked_field(&self) -> Result<tantivy::schema::Field> {
        if !self.options.keep_marks {
            return Err(Error::Unsupported(
                "nikud / te'amim matching (match_nikud, match_teamim) needs an index built with keep_marks".into(),
            ));
        }
        self.fields.marked.ok_or_else(|| {
            Error::Unsupported("this keep_marks index predates the vocalized line flag: rebuild it".into())
        })
    }

    /// Errors when `request` asks for vocalized matching the index cannot serve.
    fn check_vocalized(&self, request: &SearchRequest) -> Result<()> {
        fn wants(g: &GroupSpec) -> bool {
            match g {
                GroupSpec::Word { options, .. } => options.match_nikud || options.match_teamim,
                GroupSpec::Any { alternatives } => alternatives.iter().any(wants),
                _ => false,
            }
        }
        let ex = request.exclude.as_ref();
        let any = request.vocalized
            || ex.is_some_and(|e| e.vocalized || e.groups.iter().any(wants) || e.phrase_alternatives.iter().flatten().any(wants))
            || request.groups.iter().any(wants)
            || request.phrase_alternatives.iter().flatten().any(wants);
        if any {
            self.marked_field()?;
        }
        Ok(())
    }

    /// A word's walk options, with the index's mark layout.
    fn walk_options(&self, spec: &WalkOptionsSpec) -> WalkOptions {
        WalkOptions { marked_terms: self.options.keep_marks, ..(*spec).into() }
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
                    self.options.keep_marks,
                )?;
                Ok(Some(terms))
            }
            GroupSpec::Fuzzy { word, distance, ignore_quotes } => {
                if word.is_empty() {
                    return Ok(None);
                }
                let mut q = HebrewFuzzyQuery::new(field, word.clone(), *distance);
                q.set_ignore_quotes(*ignore_quotes);
                q.set_marked_terms(self.options.keep_marks);
                let inverted = dict()?;
                let terms = if *ignore_quotes {
                    fm_expand::expand_fuzzy_quote_free(
                        fm,
                        inverted.terms(),
                        q.word(),
                        q.distance_limit(),
                        q.max_expansions(),
                        self.options.keep_marks,
                    )?
                } else {
                    fm_expand::expand_fuzzy(
                        fm,
                        inverted.terms(),
                        &q.automaton(),
                        q.word(),
                        q.distance_limit(),
                        q.max_expansions(),
                        self.options.keep_marks,
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

    /// Runs `request`. Every matching line comes back in `request.order`
    /// (catalog order by default);
    /// the host fetches texts (in its own batches) and locates snippets
    /// through the returned session, which applies the word-distance and
    /// order filters.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResult> {
        self.search_cancellable(request, None)
    }

    /// [`search_options`](Self::search_options) with a cancel token: the
    /// search, and the returned session's locate / count calls, stop with
    /// [`Error::Cancelled`] once `cancel` is cancelled.
    pub fn search_options_cancellable(
        &self,
        options: &SearchOptions,
        cancel: Option<CancelToken>,
    ) -> Result<SearchResult> {
        self.search_cancellable(&options.to_request(self.options.keep_marks)?, cancel)
    }

    /// [`search`](Self::search) with a cancel token (see
    /// [`search_options_cancellable`](Self::search_options_cancellable)).
    pub fn search_cancellable(&self, request: &SearchRequest, cancel: Option<CancelToken>) -> Result<SearchResult> {
        CancelToken::check(cancel.as_ref())?;
        self.check_vocalized(request)?;
        // Otzaria's word options know whether the word stands alone (its
        // single-word vs phrase budgets): mark the words of a multi-word query.
        // The negative query is built the same way (Otzaria builds it with
        // the positive's builder): its words are phrase words when it has several.
        let phrase;
        let negative_phrase = request.exclude.as_ref().is_some_and(|e| e.groups.len() > 1);
        let request = if request.groups.len() > 1 || negative_phrase {
            let mut r = request.clone();
            if r.groups.len() > 1 {
                r.groups.iter_mut().for_each(mark_phrase_word);
            }
            if let Some(e) = r.exclude.as_mut().filter(|e| e.groups.len() > 1) {
                e.groups.iter_mut().for_each(mark_phrase_word);
            }
            phrase = r;
            &phrase
        } else {
            request
        };
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
            min_groups: 0,
            negative: Vec::new(),
            terms: TermTable::default(),
            locator,
            pool: Vec::new(),
            count_needs_text: false,
            cancel: cancel.clone(),
        };

        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let fm = self.fm_sidecar_for(&searcher);
        let fm = fm.as_deref();
        let mut sinks = Vec::new();
        let mut relevance = request.order == ResultsOrder::Relevance;
        // The request groups the main sinks belong to, when not all of them
        // (the plain AND skips the slots that cannot search).
        let mut main_slot_ids: Option<Vec<usize>> = None;

        let main_query: Option<Box<dyn Query>> = if let Some(prox) = &proximity {
            if self.options.positions && request.groups.len() > 1 {
                // One span query: the rule checked on each candidate's positions.
                let (gaps, window) = match prox {
                    Proximity::Ordered { gaps } => (gaps.clone(), None),
                    Proximity::AnyOrder { max_span } => (Vec::new(), Some(*max_span)),
                };
                self.span_query(&request.groups, &gaps, true, window, &searcher, fm, &mut sinks)?
                    .map(|q| Box::new(q) as Box<dyn Query>)
            } else {
                // Without positions: every group present; the locator
                // validates the rule on the text. All groups must search.
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                let mut complete = true;
                for group in &request.groups {
                    let sink = MatchedTermsSink::new();
                    match self.group_query(group, &sink, &searcher, fm)? {
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
            self.span_query(&request.groups, gaps, request.require_ordered, None, &searcher, fm, &mut sinks)?
                .map(|q| Box::new(q) as Box<dyn Query>)
        } else {
            // One query + one sink per searchable slot. Rejected wildcards skip
            // their slot (it still counts in original_group_count).
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            let mut searched = Vec::new();
            for (gi, group) in request.groups.iter().enumerate() {
                let sink = MatchedTermsSink::new();
                if let Some(q) = self.group_query(group, &sink, &searcher, fm)? {
                    clauses.push((Occur::Must, q));
                    sinks.push(sink);
                    searched.push(gi);
                }
            }
            main_slot_ids = Some(searched);
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
        let mut alternative_groups: Vec<&[GroupSpec]> = Vec::new();
        for phrase in &request.phrase_alternatives {
            if phrase.is_empty() {
                continue;
            }
            let mut alt_sinks = Vec::new();
            let q: Option<Box<dyn Query>> = if self.options.positions && phrase.len() > 1 {
                self.span_query(phrase, &vec![0; phrase.len() - 1], true, None, &searcher, fm, &mut alt_sinks)?
                    .map(|q| Box::new(q) as Box<dyn Query>)
            } else {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                let mut complete = true;
                for group in phrase {
                    let sink = MatchedTermsSink::new();
                    match self.group_query(group, &sink, &searcher, fm)? {
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
                alternative_groups.push(phrase.as_slice());
            }
        }
        // Relevance order: the main query's slots, the shape of its score,
        // and a copy of it to tell its hits from the phrases' (the score adds
        // the main query's part only to its own hits).
        let main_slots: Vec<&GroupSpec> = match &main_slot_ids {
            Some(ids) => ids.iter().map(|&i| &request.groups[i]).collect(),
            None => request.groups.iter().collect(),
        };
        let shape = relevance::Shape::of(
            main_slots.len(),
            partial,
            proximity.as_ref(),
            request.gaps.as_deref(),
            request.require_ordered,
        );
        // A relevance score that needs positions the index lacks: the hits
        // come back in catalog order, with a notice (not an error).
        let mut notices: Vec<String> = Vec::new();
        if relevance
            && relevance::lacks_positions(
                self.options.positions,
                !request.vocalized,
                main_query.is_some().then_some((shape, main_slots.as_slice())),
                &alternative_groups,
            )
        {
            relevance = false;
            notices.push(relevance::NOTICE_RELEVANCE_NEEDS_POSITIONS.to_string());
        }
        let main_membership: Option<Box<dyn Query>> = if relevance && !alternatives.is_empty() {
            main_query.as_ref().map(|q| q.box_clone())
        } else {
            None
        };
        let query: Box<dyn Query> = match (main_query, alternatives.is_empty()) {
            (None, true) => return Ok(SearchResult::new(Vec::new(), session).with_notices(notices)),
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

        // Otzaria's vocalized mode: only the lines that carry a mark.
        let query = if request.vocalized { self.marked_lines_only(query)? } else { query };

        // The negative query (see ExcludeSpec) and its phrase alternatives:
        // each excluded in the query itself, or on the text at locate time.
        // Text-level parts: their sinks and limits (distance, order, proximity).
        let mut text_negatives: Vec<(Vec<MatchedTermsSink>, usize, bool, Option<Proximity>)> = Vec::new();
        // A vocalized negative (and its phrases) sees only lines that carry a mark.
        let negative_vocalized = request.exclude.as_ref().is_some_and(|e| e.vocalized);
        let mut query_negatives: Vec<Box<dyn Query>> = Vec::new();
        // Builds the scorers once so the walks report the excluded forms of
        // a text-level part; the docs themselves are not needed.
        let fill_sinks = |q: &dyn Query| -> Result<()> {
            let w = q.weight(tantivy::query::EnableScoring::disabled_from_searcher(&searcher))?;
            for segment in searcher.segment_readers() {
                let _ = w.scorer(segment, 1.0)?;
            }
            Ok(())
        };
        if let Some(ex) = &request.exclude {
            let mut negative_sinks: Vec<MatchedTermsSink> = Vec::new();
            let ex_proximity = match &ex.proximity {
                Some(p) if ex.groups.len() > 1 => Some(p.resolve(ex.groups.len())?),
                _ => None,
            };
            let negative: Option<Box<dyn Query>> = if let (Some(prox), true) = (&ex_proximity, self.options.positions) {
                let (gaps, window) = match prox {
                    Proximity::Ordered { gaps } => (gaps.clone(), None),
                    Proximity::AnyOrder { max_span } => (Vec::new(), Some(*max_span)),
                };
                self.span_query(&ex.groups, &gaps, true, window, &searcher, fm, &mut negative_sinks)?
                    .map(|q| Box::new(q) as Box<dyn Query>)
            } else if let (Some(gaps), None) = (&ex.gaps, &ex.proximity) {
                self.span_query(&ex.groups, gaps, ex.require_ordered, None, &searcher, fm, &mut negative_sinks)?
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
                let needs_text = if ex_proximity.is_some() {
                    // without positions the rule is checked on the text; a
                    // skipped word would shift the pairs: no exclusion then
                    !self.options.positions && negative_sinks.len() == ex.groups.len()
                } else {
                    let bound = ex.max_word_distance.is_some() || ex.require_ordered;
                    ex.gaps.is_none() && negative_sinks.len() > 1 && bound
                };
                let skip = ex_proximity.is_some() && !self.options.positions && !needs_text;
                if needs_text {
                    fill_sinks(negative.as_ref())?;
                    text_negatives.push((
                        negative_sinks,
                        ex.max_word_distance.unwrap_or(usize::MAX),
                        ex.require_ordered,
                        ex_proximity.clone(),
                    ));
                } else if !skip {
                    query_negatives.push(negative);
                }
            }
            // Phrase alternatives of the negative query, built as the
            // positive's: a span query with positions; without, an AND
            // checked on the text (ordered, distance 0). A phrase with a
            // word that matches no term can never be present: it excludes
            // nothing.
            for phrase in &ex.phrase_alternatives {
                if phrase.is_empty() {
                    continue;
                }
                let mut sinks = Vec::new();
                if self.options.positions && phrase.len() > 1 {
                    if let Some(q) =
                        self.span_query(phrase, &vec![0; phrase.len() - 1], true, None, &searcher, fm, &mut sinks)?
                    {
                        query_negatives.push(Box::new(q));
                    }
                    continue;
                }
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                let mut complete = true;
                for group in phrase {
                    let sink = MatchedTermsSink::new();
                    match self.group_query(group, &sink, &searcher, fm)? {
                        Some(q) => {
                            clauses.push((Occur::Must, q));
                            sinks.push(sink);
                        }
                        None => complete = false,
                    }
                }
                if !complete || clauses.is_empty() {
                    continue;
                }
                let q: Box<dyn Query> = Box::new(BooleanQuery::new(clauses));
                if phrase.len() > 1 {
                    fill_sinks(q.as_ref())?;
                    text_negatives.push((sinks, 0, true, None));
                } else {
                    query_negatives.push(q);
                }
            }
        }
        let query: Box<dyn Query> = if query_negatives.is_empty() {
            query
        } else {
            let mut clauses = vec![(Occur::Must, query)];
            for q in query_negatives {
                let q = if negative_vocalized { self.marked_lines_only(q)? } else { q };
                clauses.push((Occur::MustNot, q));
            }
            Box::new(BooleanQuery::new(clauses))
        };

        let timing = std::env::var_os("SEFORIM_ENGINE_TIMING").is_some();
        let t = std::time::Instant::now();
        CancelToken::check(cancel.as_ref())?;
        let weight = query.weight(tantivy::query::EnableScoring::disabled_from_searcher(&searcher))?;
        if timing {
            eprintln!("  [timing] weight (walks + caps): {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
        let t = std::time::Instant::now();
        let mut hits = Vec::new();
        // Relevance order only: each hit's (segment, doc), aligned with `hits`.
        let mut keys: Vec<(u32, DocId)> = Vec::new();
        let collector = PlacedCollector {
            books: request.books.as_ref().map(|b| Arc::new(b.iter().copied().collect())),
            keep_docs: relevance,
        };
        for (ord, segment) in searcher.segment_readers().iter().enumerate() {
            CancelToken::check(cancel.as_ref())?;
            let mut child = collector.for_segment(ord as SegmentOrdinal, segment)?;
            if let Some(token) = &cancel {
                // Cancellable: walk the scorer, checking every 4096 docs.
                let mut scorer = weight.scorer(segment, 1.0)?;
                let alive = segment.alive_bitset();
                let mut doc = scorer.doc();
                let mut seen = 0u32;
                while doc != tantivy::TERMINATED {
                    if alive.is_none_or(|a| a.is_alive(doc)) {
                        child.collect(doc, 0.0);
                    }
                    seen = seen.wrapping_add(1);
                    if seen % 4096 == 0 && token.is_cancelled() {
                        return Err(Error::Cancelled);
                    }
                    doc = scorer.advance();
                }
                keys.extend(child.take_docs().into_iter().map(|doc| (ord as u32, doc)));
                hits.extend(child.harvest());
                continue;
            }
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
            keys.extend(child.take_docs().into_iter().map(|doc| (ord as u32, doc)));
            hits.extend(child.harvest());
        }
        if timing {
            eprintln!("  [timing] collect {} hits: {:.1} ms", hits.len(), t.elapsed().as_secs_f64() * 1000.0);
        }
        // Relevance keeps the collection order (aligned with `keys`) until
        // the hits are scored, below.
        if !relevance {
            // (relevance turned off for want of positions sorts by catalog order)
            let order = if request.order == ResultsOrder::Relevance { ResultsOrder::Catalog } else { request.order };
            relevance::sort_hits(&mut hits, order, &request.book_generations);
        }
        for (sinks, max_word_distance, require_ordered, proximity) in text_negatives {
            let negative_terms: Vec<Vec<String>> = sinks.iter().map(|s| s.take_strings()).collect();
            // a word with no indexed form can never be present: no exclusion
            if negative_terms.iter().all(|g| !g.is_empty()) {
                session.negative.push(NegativeFilter {
                    prepared: PreparedQueryGroups::from_groups(&negative_terms),
                    max_word_distance,
                    require_ordered,
                    proximity,
                    vocalized: negative_vocalized,
                });
            }
        }

        // The main query's matcher, if it can match at all.
        let mut group_terms: Vec<Vec<String>> = sinks.iter().map(|s| s.take_strings()).collect();
        // every slot's terms, empty ones included (aligned with `main_slots`)
        let main_terms = relevance.then(|| group_terms.clone());
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
        let mut phrase_plans: Vec<(&[GroupSpec], Vec<Vec<String>>)> = Vec::new();
        for ((alt_sinks, _), groups) in alternatives.iter().zip(&alternative_groups) {
            let terms: Vec<Vec<String>> = alt_sinks.iter().map(|s| s.take_strings()).collect();
            if terms.iter().all(|g| !g.is_empty()) {
                if relevance {
                    phrase_plans.push((groups, terms.clone()));
                }
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
        // Which hits the locator can still reject (see `count_needs_text`).
        // One group: no distance or order to check.
        let main_needs_text = main_ok
            && request.groups.len() > 1
            && match &proximity {
                    // the span query already checked the rule on the positions
                    Some(_) => !self.options.positions,
                    None => {
                        !partial
                            && (request.max_word_distance.is_some()
                                || (request.require_ordered && request.gaps.is_none()))
                    }
                };
        let phrase_needs_text =
            !self.options.positions && request.phrase_alternatives.iter().any(|p| p.len() > 1);
        session.count_needs_text = main_needs_text || phrase_needs_text || !session.negative.is_empty();
        if session.matchers.is_empty() {
            return Ok(SearchResult::new(Vec::new(), session).with_notices(notices));
        }
        if let Some(main_terms) = main_terms {
            CancelToken::check(cancel.as_ref())?;
            let t = std::time::Instant::now();
            let main_slots_len = main_slots.len();
            let plan = relevance::ScorePlan {
                field: self.fields.text,
                vocalized: request.vocalized,
                marked_terms: keep_marks,
                main: (main_ok && main_terms.len() == main_slots.len()).then(|| relevance::MainPlan {
                    shape,
                    slots: main_slots,
                    terms: main_terms,
                }),
                main_membership: match &main_membership {
                    Some(q) => Some(q.weight(tantivy::query::EnableScoring::disabled_from_searcher(&searcher))?),
                    None => None,
                },
                phrases_as_count: relevance::phrases_as_count(shape, main_slots_len),
                phrases: phrase_plans,
            };
            let scores = relevance::scores(&searcher, &plan, &keys, cancel.as_ref())?;
            let (hits, scores) = relevance::sort_by_score(hits, scores);
            if timing {
                eprintln!("  [timing] relevance {} hits: {:.1} ms", hits.len(), t.elapsed().as_secs_f64() * 1000.0);
            }
            let mut result = SearchResult::new(hits, session).with_notices(notices);
            result.scores = Some(scores);
            return Ok(result);
        }
        Ok(SearchResult::new(hits, session).with_notices(notices))
    }
}
