//! Port of FtsLib `SnippetWindowLocator.cs` + `SnippetDescriptor.cs`
//! (KitveiHakodesh, current), step for step.
//!
//! Locates the snippet window in a line of raw HTML for a prepared query and
//! describes it as offsets. Emits NO text: no HTML, no stripped copy.
//! Rendering is the consumer's job (the page).
//!
//! Single pass; the raw HTML is scanned exactly once:
//!   1. [`TextTokens`] tokenizes it, producing tokens with raw positions AND
//!      cumulative visible-char offsets.
//!   2. A sliding window finds the tightest token window covering all query
//!      groups.
//!   3. `expand_window` widens it by context words, reading the token list
//!      only.
//!   4. `collect_mark_ranges` walks the tokens inside the window and records
//!      the matched ones: those of a full occurrence of the query inside the
//!      window (`occurrence_marks`, issue #4).
//!
//! All offsets are UTF-16 code units into the source string, as in FtsLib,
//! so the page's existing renderer consumes them unchanged.
//!
//! Query state (the term→group map) lives in [`PreparedQueryGroups`]: built
//! once per query, shared read-only. The locator holds only per-call scratch,
//! reused across calls with no per-call heap allocation; the descriptor it
//! returns views that scratch. One locator per thread.

use crate::prepared::PreparedQueryGroups;
use crate::scanner::{TextToken, TextTokens};

/// Words of context on each side of the match (FtsLib
/// `SnippetDescriptorPipeline.DefaultContextWords`).
pub const DEFAULT_CONTEXT_WORDS: usize = 8;

/// Safety ceiling in visible chars. Only fires for pathologically long lines
/// (e.g. a single paragraph with no word breaks).
const SAFETY_CEILING: usize = 4000;

/// [`SnippetWindowLocator`]'s per-token ordinal for a token that is no query term.
const NO_TERM: u32 = u32::MAX;

/// Otzaria's word proximity, checked on the line's tokens (the index's word
/// positions: both come from the same scanner, one position per token,
/// except a paired reading's second reading, which shares its first
/// reading's position).
///
/// - `Ordered { gaps }`: groups in query order at positions
///   `p0 < p1 < ...`, with `p[i+1] - p[i] - 1 <= gaps[i]` (words BETWEEN
///   each consecutive pair).
/// - `AnyOrder { max_span }`: every group at its own token, in any order,
///   the last at most `max_span` positions after the first.
///
/// The located window is the TIGHTEST window (raw char span, ties = the
/// earliest) that satisfies the rule, so the snippet always shows a valid
/// occurrence; marks are the tokens of every full occurrence (this rule)
/// inside the expanded window (issue #4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proximity {
    /// One limit per consecutive pair of groups (`group_count - 1` entries).
    Ordered { gaps: Vec<u32> },
    /// One window for all groups.
    AnyOrder { max_span: u32 },
}

/// Scratch for [`SnippetWindowLocator::locate_proximity_tokenized`], reused
/// across lines (no per-line allocation once grown).
#[derive(Default)]
struct ProximityScratch {
    /// Ordered: (token, latest chain start) for the groups so far / next.
    chain: Vec<(usize, usize)>,
    next: Vec<(usize, usize)>,
    /// Ordered: sliding maximum of chain starts.
    deque: std::collections::VecDeque<(usize, usize)>,
    /// Any order: term tokens per group inside the window.
    coverage: Vec<u32>,
    /// Any order: per group, the window's tokens it can take, in order.
    in_window: Vec<std::collections::VecDeque<usize>>,
    /// Any order matching, kept incrementally: group matched to each token
    /// (or FREE), token matched to each group (or FREE).
    owner: Vec<usize>,
    taken: Vec<usize>,
    /// Visit marks of the current augmenting search (`generation`).
    stamp: Vec<u32>,
    generation: u32,
    /// Augmenting path: (group, next candidate index in its `in_window`).
    stack: Vec<(usize, usize)>,
}

impl ProximityScratch {
    /// Readies the any-order state for `n` tokens and `k` groups.
    fn reset_any_order(&mut self, n: usize, k: usize) {
        self.coverage.clear();
        self.coverage.resize(k, 0);
        self.in_window.resize_with(k, Default::default);
        self.in_window.iter_mut().for_each(|d| d.clear());
        self.owner.clear();
        self.owner.resize(n, FREE);
        self.taken.clear();
        self.taken.resize(k, FREE);
        self.stamp.clear();
        self.stamp.resize(n, 0);
        self.generation = 0;
    }

    /// Token `t` (groups `gs`) enters the window at its right (`front`:
    /// back at its left, undoing [`Self::remove_front`]).
    fn add(&mut self, t: usize, gs: &[usize], front: bool, w: &mut WindowCounts) {
        w.terms += 1;
        for &g in gs {
            w.covered += usize::from(self.coverage[g] == 0);
            self.coverage[g] += 1;
            if front {
                self.in_window[g].push_front(t);
            } else {
                self.in_window[g].push_back(t);
            }
        }
    }

    /// The window's leftmost token `t` leaves; its group, if matched, is freed.
    fn remove_front(&mut self, t: usize, gs: &[usize], w: &mut WindowCounts) {
        if gs.is_empty() {
            return;
        }
        w.terms -= 1;
        for &g in gs {
            self.coverage[g] -= 1;
            w.covered -= usize::from(self.coverage[g] == 0);
            let front = self.in_window[g].pop_front();
            debug_assert_eq!(front, Some(t));
        }
        let holder = self.owner[t];
        if holder != FREE {
            self.owner[t] = FREE;
            self.taken[holder] = FREE;
            w.matched -= 1;
        }
    }

    /// Whether every group can take its own token in the window: augments
    /// only the free groups of the kept matching.
    fn complete(&mut self, k: usize, w: &mut WindowCounts) -> bool {
        if w.covered < k || w.terms < k {
            return false;
        }
        for g in 0..k {
            if self.taken[g] == FREE {
                if !self.augment(g) {
                    return false;
                }
                w.matched += 1;
            }
        }
        true
    }

    /// Kuhn's augmenting path from the free group `start`, iterative; on
    /// success the path is flipped and `start` is matched.
    fn augment(&mut self, start: usize) -> bool {
        self.generation += 1;
        let generation = self.generation;
        self.stack.clear();
        self.stack.push((start, 0));
        while let Some(&(g, i)) = self.stack.last() {
            let Some(&t) = self.in_window[g].get(i) else {
                self.stack.pop();
                continue;
            };
            self.stack.last_mut().expect("non-empty").1 = i + 1;
            if self.stamp[t] == generation {
                continue;
            }
            self.stamp[t] = generation;
            let holder = self.owner[t];
            if holder == FREE {
                for &(h, next) in &self.stack {
                    let token = self.in_window[h][next - 1];
                    self.owner[token] = h;
                    self.taken[h] = token;
                }
                return true;
            }
            self.stack.push((holder, 0));
        }
        false
    }
}

/// Any-order window counters: groups covered, term tokens, matched groups.
#[derive(Default)]
struct WindowCounts {
    covered: usize,
    terms: usize,
    matched: usize,
}

/// What a locate returns for a text that passes (issues #14 / #23; FtsLib
/// `SnippetMatchMode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MatchMode {
    /// The best window and the marks of its occurrences (the default,
    /// unchanged).
    #[default]
    BestWindow,
    /// The whole text: the window is `[0, text length)` and the marks are
    /// the tokens of EVERY full occurrence in the text, by the best window's
    /// own rule (issue #4: distance / order, proximity, a phrase, crossing
    /// the break; partial matching and no distance: every query-term token).
    /// The pass decision, `score` and `word_distance` are the best window's.
    AllMatches,
}

/// Ints per entry in [`SnippetDescriptor::mark_ranges`]: start, length, term
/// ordinal.
pub const MARK_STRIDE: usize = 3;

/// WHERE a snippet is in its source text. Offsets are UTF-16 units into the
/// string the locator was given.
#[derive(Debug, Clone, Copy)]
pub struct SnippetDescriptor<'a> {
    /// True when every query group was found in the source (and, if ordered
    /// matching was required, in query order).
    pub is_match: bool,
    /// Raw char span of the tightest window covering all query groups.
    /// Smaller = terms closer together. `usize::MAX` = at least one group
    /// absent.
    pub score: usize,
    /// Words between the leftmost and rightmost matched tokens in the
    /// tightest window. 0 = adjacent. `usize::MAX` = no match.
    pub word_distance: usize,
    /// Visible words the window spans after context expansion. 0 when there
    /// is no match.
    pub window_word_count: usize,
    /// The window as raw offsets `[snap_start, snap_end)`. 0/0 when no match.
    pub snap_start: usize,
    /// End of the window, exclusive.
    pub snap_end: usize,
    /// "…" belongs before the window (it starts mid-text).
    pub leading_ellipsis: bool,
    /// "…" belongs after the window (it ends before the source does).
    pub trailing_ellipsis: bool,
    /// Every highlighted token as flat (rawStart, rawLength, termOrdinal)
    /// triples, document order, length clipped to `snap_end`. A view over
    /// the locator's reused scratch: valid until its next `locate`.
    pub mark_ranges: &'a [usize],
}

impl SnippetDescriptor<'_> {
    const NO_MATCH: SnippetDescriptor<'static> = SnippetDescriptor {
        is_match: false,
        score: usize::MAX,
        word_distance: usize::MAX,
        window_word_count: 0,
        snap_start: 0,
        snap_end: 0,
        leading_ellipsis: false,
        trailing_ellipsis: false,
        mark_ranges: &[],
    };

    /// The marks as (start, length, term ordinal).
    pub fn marks(&self) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        self.mark_ranges
            .chunks_exact(MARK_STRIDE)
            .map(|m| (m[0], m[1], m[2]))
    }
}

/// The locator. Holds per-call scratch only; see module docs.
pub struct SnippetWindowLocator {
    context_words: usize,
    text_tokens: TextTokens,
    /// Per token of the current text: its term ordinal in the prepared query,
    /// or [`NO_TERM`]. Looked up ONCE per token; the window search, the order
    /// check and the marks all read it instead of re-hashing the token
    /// (FtsLib looks the same token up in each stage; same answers).
    ordinals: Vec<u32>,
    group_count: Vec<usize>,
    mark_ranges: Vec<usize>,
    /// Groups a window must cover; 0 = all of them (FtsLib). See
    /// [`set_min_groups`](Self::set_min_groups).
    min_groups: usize,
    proximity: ProximityScratch,
    /// Cross-line: the index of the next line's first token after
    /// [`tokenize_pair`](Self::tokenize_pair) (None after
    /// [`tokenize`](Self::tokenize)). While set, every window must cross
    /// the break: a matched token on each line.
    break_token: Option<usize>,
    /// UTF-16 offset of the break (the joining `\n`) in the joined text.
    break_utf16: usize,
    /// Synthetic tokens for [`proximity_across_positions`](Self::proximity_across_positions).
    synthetic: Vec<TextToken>,
    /// After [`tokenize_positioned`](Self::tokenize_positioned): each
    /// token's term id in the caller's term list (or `NO_TERM`), read off
    /// the index positions instead of hashing the token. The locate calls
    /// then need a term map (`*_mapped`).
    token_terms: Vec<u32>,
    positioned: bool,
    /// Scratch for a matched token's word in `tokenize_positioned`.
    word: String,
    /// Scratch for the occurrence marks (issue #4).
    marks: MarkScratch,
    /// See [`set_match_mode`](Self::set_match_mode).
    match_mode: MatchMode,
    /// Scratch for [`find_all_matches`](Self::find_all_matches).
    unquoted: String,
    run: Vec<usize>,
    /// See [`set_word_span`](Self::set_word_span).
    word_span: bool,
}

impl Default for SnippetWindowLocator {
    fn default() -> Self {
        Self::new(DEFAULT_CONTEXT_WORDS)
    }
}

impl SnippetWindowLocator {
    /// A locator with `context_words` of context per side, tokenizing with
    /// marks stripped (the `hebrew` field convention).
    pub fn new(context_words: usize) -> SnippetWindowLocator {
        Self::with_tokens(context_words, TextTokens::default())
    }

    /// A locator whose tokenizer keeps nikud/te'amim (for `hebrew_marked`
    /// fields; the prepared terms then carry marks too).
    pub fn keeping_marks(context_words: usize) -> SnippetWindowLocator {
        Self::with_tokens(context_words, TextTokens::keeping_marks())
    }

    fn with_tokens(context_words: usize, text_tokens: TextTokens) -> SnippetWindowLocator {
        SnippetWindowLocator {
            context_words,
            text_tokens,
            ordinals: Vec::new(),
            group_count: vec![0; 8],
            mark_ranges: Vec::with_capacity(24),
            min_groups: 0,
            proximity: ProximityScratch::default(),
            break_token: None,
            break_utf16: 0,
            synthetic: Vec::new(),
            token_terms: Vec::new(),
            positioned: false,
            word: String::new(),
            marks: MarkScratch::default(),
            match_mode: MatchMode::BestWindow,
            unquoted: String::new(),
            run: Vec::new(),
            word_span: false,
        }
    }

    /// What the locate calls return for a passing text (see [`MatchMode`]).
    /// All matches needs the whole text tokenized: use it with
    /// [`tokenize`](Self::tokenize) / [`tokenize_pair`](Self::tokenize_pair),
    /// not [`tokenize_positioned`](Self::tokenize_positioned) (which stops early).
    pub fn set_match_mode(&mut self, mode: MatchMode) {
        self.match_mode = mode;
    }

    /// The tightest-window rule (issue #21). False (the default): FtsLib's
    /// rule, the window spanning the fewest UTF-16 units. True: the window
    /// spanning the fewest WORD positions, the earliest on a tie: the rule an
    /// index with positions applies during the search (from the postings,
    /// see [`tokenize_occurrences`](Self::tokenize_occurrences)), so a text
    /// located afterwards picks the same window. The two rules differ only
    /// in the occurrence chosen (measured on the full library: 0.1-3% of
    /// the hits) and, through it, rarely in a distance filter: that filter
    /// reads the chosen window's words, so a line whose character-tightest
    /// window is too far apart in words but whose word-tightest one is not
    /// passes under the word rule only.
    pub fn set_word_span(&mut self, on: bool) {
        self.word_span = on;
    }

    /// The current [`MatchMode`].
    pub fn match_mode(&self) -> MatchMode {
        self.match_mode
    }

    /// Every occurrence of `prepared`'s terms in `text` (find in an open
    /// text, no index; issues #14 / #23, FtsLib `FindAllMatches`) as flat
    /// (start, length, term ordinal) triples, document order, UTF-16
    /// offsets; the same word rules as the locate calls (HTML, entities,
    /// marks as this locator's tokenizer treats them).
    ///
    /// `phrase`: only tokens of a run of consecutive positions holding
    /// groups 0, 1, ... in order (repeated and overlapping runs all count;
    /// a token is listed once); else every term token. Quotes as the walks
    /// read them: a token matches as written or without its quotes (a
    /// quote-free word finds `X"Y`); `ignore_quotes`: also a typed quote is
    /// ignored (the prepared terms must then be quote-free, as
    /// `from_text_words` makes them). The view is valid until the next call.
    pub fn find_all_matches(
        &mut self,
        text: &str,
        prepared: &PreparedQueryGroups,
        phrase: bool,
        ignore_quotes: bool,
    ) -> &[usize] {
        self.mark_ranges.clear();
        if text.is_empty() || prepared.is_empty() {
            return &self.mark_ranges;
        }
        let k = prepared.group_count();
        let keep = self.text_tokens.keeps_marks();
        let SnippetWindowLocator { text_tokens, ordinals, mark_ranges, marks, unquoted, run, break_token, positioned, .. } =
            self;
        // Quotes as the walks read them (Otzaria's QuoteTransparent, the
        // index terms keep them): a token matches as written, or with its
        // quotes removed (a quote-free word finds the quoted token; a typed
        // quote still needs it). `ignore_quotes`: the words are quote-free
        // too, so only the second form can match.
        let mut lookup = |norm: &str| -> u32 {
            if !ignore_quotes {
                if let Some(p) = prepared.get(norm) {
                    return p.ordinal as u32;
                }
            }
            if !norm.bytes().any(crate::chars::is_term_quote_byte) {
                return if ignore_quotes { prepared.get(norm).map_or(NO_TERM, |p| p.ordinal as u32) } else { NO_TERM };
            }
            unquoted.clear();
            unquoted.extend(norm.chars().filter(|&c| !crate::chars::is_term_quote(c)));
            if unquoted.is_empty() {
                return NO_TERM;
            }
            prepared.get(unquoted.as_str()).map_or(NO_TERM, |p| p.ordinal as u32)
        };
        if !phrase || k <= 1 {
            // every term token: straight off the scan, no token list kept
            // (the same words, offsets and marks as `tokenize`)
            crate::scanner::scan_with(text, keep, |word, span| {
                let ordinal = lookup(word);
                if ordinal != NO_TERM {
                    mark_ranges.extend([span.utf16_start, span.utf16_end - span.utf16_start, ordinal as usize]);
                }
            });
            return mark_ranges;
        }
        // as `tokenize`: the locate calls then read this text
        text_tokens.tokenize(text);
        *break_token = None;
        *positioned = false;
        let tokens = text_tokens.tokens();
        ordinals.clear();
        ordinals.extend(tokens.iter().map(|t| lookup(text_tokens.norm_of(t))));
        let n = tokens.len();
        let mask = &mut marks.mask;
        mask.clear();
        mask.resize(n, false);
        for t in 0..n {
            if !in_group(ordinals, prepared, t, 0) {
                continue;
            }
            let p = tokens[t].position;
            run.clear();
            run.push(t);
            let mut u = t;
            for g in 1..k {
                // the token holding group g at position p + g (a paired
                // reading puts two tokens at one position)
                while u < n && tokens[u].position < p + g {
                    u += 1;
                }
                let mut v = u;
                while v < n && tokens[v].position == p + g && !in_group(ordinals, prepared, v, g) {
                    v += 1;
                }
                if v < n && tokens[v].position == p + g {
                    run.push(v);
                } else {
                    break;
                }
            }
            if run.len() == k {
                run.iter().for_each(|&v| mask[v] = true);
            }
        }
        collect_mark_ranges(tokens, ordinals, 0, usize::MAX, mark_ranges, Some((&mask[..], 0)));
        mark_ranges
    }

    /// Partial matching (Otzaria's wordMatchMode anyWord / mostWords /
    /// atLeast): a window needs only `min_groups` distinct query groups, the
    /// tightest such window wins, and the order check is skipped (Otzaria:
    /// with words allowed to be missing, order and distance have no
    /// meaning). 0, or a value of at least the group count, restores FtsLib's
    /// all-groups rule.
    pub fn set_min_groups(&mut self, min_groups: usize) {
        self.min_groups = min_groups;
    }

    /// The core locate. `prepared` is the query's immutable term→group map.
    /// `require_ordered`: also demand the groups appear in query order.
    /// `original_group_count`: overrides the WordDistance denominator so
    /// skipped wildcards still count as a slot (0 = use the prepared count).
    pub fn locate(
        &mut self,
        raw_html: &str,
        prepared: &PreparedQueryGroups,
        require_ordered: bool,
        original_group_count: usize,
    ) -> SnippetDescriptor<'_> {
        if raw_html.is_empty() || prepared.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }
        self.tokenize(raw_html);
        self.locate_tokenized(prepared, require_ordered, original_group_count)
    }

    /// Tokenizes `raw_html` for [`locate_tokenized`](Self::locate_tokenized):
    /// a caller locating several prepared queries over the same text (phrase
    /// alternatives, a negative filter) scans it once. The tokens depend only
    /// on the text, so the result equals calling [`locate`](Self::locate)
    /// for each query.
    pub fn tokenize(&mut self, raw_html: &str) {
        self.text_tokens.tokenize(raw_html);
        self.break_token = None;
        self.positioned = false;
    }

    /// Tokenizes `text` for the `*_mapped` locate calls, taking each token's
    /// term from the index positions instead of hashing it: one scan that
    /// keeps no normalized text and stops once the words after the last
    /// matched position can no longer reach a window or its context.
    ///
    /// `text` is one or more lines joined (`line_starts`: each line's byte
    /// start, the first 0); `occurrences[k]` holds line k's matched
    /// `(position, term id)` pairs from the postings (positions local to
    /// the line, sorted), `terms[id]` the term's text. Every occurrence is
    /// checked against the word the scanner finds there (paired readings
    /// share a position: the word picks its own term). False when the text
    /// and the positions disagree (a text that is not the indexed one): the
    /// caller then falls back to [`tokenize`](Self::tokenize).
    pub fn tokenize_positioned<T: AsRef<str>>(
        &mut self,
        text: &str,
        line_starts: &[usize],
        occurrences: &[&[(u32, u32)]],
        terms: &[T],
    ) -> bool {
        debug_assert_eq!(line_starts.len(), occurrences.len());
        self.break_token = None;
        self.positioned = false;
        let ctx = self.context_words;
        let last_line = occurrences.iter().rposition(|o| !o.is_empty());
        // the last position a window or its context can use, in the last
        // line holding an occurrence (one more word keeps the end exact)
        let limit = last_line.map(|k| occurrences[k].last().unwrap().0 as usize + ctx + 1);
        let token_terms = &mut self.token_terms;
        token_terms.clear();
        let mut bad = false;
        let (mut k, mut ptr, mut seen) = (0usize, 0usize, 0u64);
        let mut base: Option<usize> = None;
        let tt = &mut self.text_tokens;
        let word = &mut self.word;
        let keep = tt.keeps_marks();
        tt.tokenize_light(text, |tok| {
            while k + 1 < line_starts.len() && tok.raw_start >= line_starts[k + 1] {
                if !close_rest(occurrences[k], &mut ptr, &mut seen) {
                    bad = true;
                    token_terms.push(NO_TERM);
                    return false;
                }
                k += 1;
                ptr = 0;
                seen = 0;
                base = None;
            }
            let local = tok.position - *base.get_or_insert(tok.position);
            let o = occurrences[k];
            while ptr < o.len() && (o[ptr].0 as usize) < local {
                if !close_group(o, &mut ptr, &mut seen) || (ptr < o.len() && (o[ptr].0 as usize) < local) {
                    bad = true; // an occurrence no word matched
                    token_terms.push(NO_TERM);
                    return false;
                }
            }
            let mut id = NO_TERM;
            let mut j = ptr;
            if j < o.len() && o[j].0 as usize == local {
                // the word, only where a matched term sits
                if !word_of(text, tok, keep, word) {
                    bad = true;
                    token_terms.push(NO_TERM);
                    return false;
                }
            }
            while j < o.len() && o[j].0 as usize == local {
                if terms[o[j].1 as usize].as_ref() == word.as_str() {
                    id = o[j].1;
                    seen |= 1u64 << (j - ptr).min(63);
                    break;
                }
                j += 1;
            }
            token_terms.push(id);
            !(Some(k) == last_line && limit.is_some_and(|l| local > l))
        });
        if bad || !close_rest(occurrences.get(k).copied().unwrap_or(&[]), &mut ptr, &mut seen) {
            return false;
        }
        // lines the scan did not reach must hold no occurrence
        if occurrences.iter().skip(k + 1).any(|o| !o.is_empty()) {
            return false;
        }
        self.positioned = true;
        true
    }

    /// Loads a line's matched words straight from the index positions, with
    /// no text: `occurrences` are the line's sorted `(position, term id)`
    /// pairs (as for [`tokenize_positioned`](Self::tokenize_positioned)),
    /// each one a token at its position. The `*_mapped` locate calls then
    /// decide the match and its tightest window exactly as on the text
    /// (only matched words can bound a window; a window's span is read in
    /// word positions, see [`set_word_span`](Self::set_word_span)); the
    /// descriptor's offsets are word positions, not text offsets.
    pub fn tokenize_occurrences(&mut self, occurrences: &[(u32, u32)]) {
        self.break_token = None;
        self.text_tokens.load_positions(occurrences.iter().map(|o| o.0 as usize));
        self.token_terms.clear();
        self.token_terms.extend(occurrences.iter().map(|o| o.1));
        self.positioned = true;
    }

    /// [`tokenize_occurrences`](Self::tokenize_occurrences) for two lines
    /// joined at a line break: `occurrences` are the joined lines' sorted
    /// `(position, term id)` pairs (the next line's positions shifted past
    /// the first line's words), `break_position` the next line's first
    /// position. Until the next tokenize call every window the locate calls
    /// find must cross the break, as after [`tokenize_pair`](Self::tokenize_pair)
    /// (cross-line matching decided from the index positions, issue #21).
    pub fn tokenize_occurrences_across(&mut self, occurrences: &[(u32, u32)], break_position: usize) {
        self.tokenize_occurrences(occurrences);
        self.break_token = Some(occurrences.partition_point(|o| (o.0 as usize) < break_position));
    }

    /// Tokenizes two consecutive lines joined as `left + "\n" + right`
    /// (see [`TextTokens::tokenize_pair`]) for a cross-line locate: until
    /// the next [`tokenize`](Self::tokenize), every window the locate calls
    /// find must cross the break (at least one matched token on each line),
    /// and offsets are UTF-16 units into the joined text. Returns the UTF-16
    /// offset of the break.
    pub fn tokenize_pair(&mut self, left: &str, right: &str) -> usize {
        self.positioned = false;
        let split = self.text_tokens.tokenize_pair(left, right);
        self.break_token = Some(split);
        self.break_utf16 = left.encode_utf16().count();
        self.break_utf16
    }

    /// The break's UTF-16 offset after [`tokenize_pair`](Self::tokenize_pair);
    /// None after [`tokenize`](Self::tokenize).
    pub fn break_utf16(&self) -> Option<usize> {
        self.break_token.map(|_| self.break_utf16)
    }

    /// Otzaria's proximity checked on index positions across a line break:
    /// `occurrences` are `(position, term ordinal of prepared)` of the two
    /// lines joined (the next line's positions shifted by the first line's
    /// word count, `break_position`). True when a match crosses the break,
    /// with the same window rules [`locate_proximity_tokenized`](Self::locate_proximity_tokenized)
    /// applies to a text joined by [`tokenize_pair`](Self::tokenize_pair):
    /// the index positions ARE the locator's tokens (same scanner), so both
    /// index flavours agree.
    pub fn proximity_across_positions(
        &mut self,
        prepared: &PreparedQueryGroups,
        proximity: &Proximity,
        occurrences: &mut Vec<(usize, u32)>,
        break_position: usize,
    ) -> bool {
        occurrences.sort_unstable();
        occurrences.dedup();
        let SnippetWindowLocator { ordinals, proximity: scratch, synthetic, .. } = self;
        synthetic.clear();
        synthetic.extend(occurrences.iter().map(|&(p, _)| TextToken::at_position(p)));
        ordinals.clear();
        ordinals.extend(occurrences.iter().map(|&(_, o)| o));
        let split = occurrences.partition_point(|&(p, _)| p < break_position);
        let found = match proximity {
            Proximity::Ordered { gaps } => ordered_window(synthetic, ordinals, prepared, gaps, Some(split), scratch, true),
            Proximity::AnyOrder { max_span } => {
                any_order_window(synthetic, ordinals, prepared, *max_span as usize, Some(split), scratch, true)
            }
        };
        found.is_some()
    }

    /// [`locate`](Self::locate) over the text last passed to
    /// [`tokenize`](Self::tokenize).
    pub fn locate_tokenized(
        &mut self,
        prepared: &PreparedQueryGroups,
        require_ordered: bool,
        original_group_count: usize,
    ) -> SnippetDescriptor<'_> {
        self.locate_tokenized_mapped(prepared, &[], require_ordered, original_group_count)
    }

    /// [`locate_tokenized`](Self::locate_tokenized) that also reads a text
    /// tokenized by [`tokenize_positioned`](Self::tokenize_positioned):
    /// `term_map[id]` is the ordinal in `prepared` of term id `id` (or
    /// `u32::MAX`). The map is ignored after [`tokenize`](Self::tokenize).
    pub fn locate_tokenized_mapped(
        &mut self,
        prepared: &PreparedQueryGroups,
        term_map: &[u32],
        require_ordered: bool,
        original_group_count: usize,
    ) -> SnippetDescriptor<'_> {
        self.locate_tokenized_within(prepared, term_map, require_ordered, original_group_count, usize::MAX)
    }

    /// [`locate_tokenized_mapped`](Self::locate_tokenized_mapped) for a
    /// caller that keeps only hits whose `word_distance` is at most
    /// `max_word_distance` (FtsLib's filter; `usize::MAX` = none): the marks
    /// are then only the tokens of covering windows inside the snippet
    /// that keep that distance (issue #4). Partial matching
    /// (`min_groups`) marks every query term in the snippet, as before.
    pub fn locate_tokenized_within(
        &mut self,
        prepared: &PreparedQueryGroups,
        term_map: &[u32],
        require_ordered: bool,
        original_group_count: usize,
        max_word_distance: usize,
    ) -> SnippetDescriptor<'_> {
        if prepared.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }

        let SnippetWindowLocator {
            context_words,
            text_tokens,
            ordinals,
            group_count,
            mark_ranges,
            min_groups,
            break_token,
            token_terms,
            positioned,
            marks,
            match_mode,
            word_span,
            ..
        } = self;
        let brk = *break_token;
        let tokens = text_tokens.tokens();
        if tokens.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }
        let raw_len = text_tokens.raw_utf16_len();

        fill_ordinals(ordinals, text_tokens, prepared, positioned.then_some((&token_terms[..], term_map)));
        let ordinals = &ordinals[..];

        let all_groups = prepared.group_count();
        let partial = *min_groups > 0 && *min_groups < all_groups;
        let required = if partial { *min_groups } else { all_groups };
        // In order (FtsLib's "in order" option): the best window is the
        // tightest ORDERED covering window and the marks the tokens of ordered
        // occurrences; the order check is this window's existence. Order off:
        // unchanged.
        let ordered = !partial && require_ordered && all_groups > 1;
        let ordered_best = if ordered { ordered_cover_window(tokens, ordinals, prepared, brk, marks, *word_span) } else { None };
        let (i_left, i_right, score) = match ordered_best {
            Some(w) => w,
            None => find_window(tokens, ordinals, prepared, group_count, required, brk, *word_span),
        };
        if score == usize::MAX {
            return SnippetDescriptor::NO_MATCH;
        }

        let (snap_start, snap_end, s_idx, e_idx) =
            expand_window(tokens, raw_len, i_left, i_right, *context_words);

        // Visible words actually inside the window: lets the host detect a
        // "short" snippet for free (it may then re-locate over a longer text).
        let window_words = e_idx - s_idx + 1;

        // WordDistance = extra words between matched slots (0 = all
        // consecutive). original_group_count overrides so skipped wildcards
        // still count as a slot.
        let denominator = if partial {
            required
        } else if original_group_count > 0 {
            original_group_count
        } else {
            all_groups
        };
        // Marks: the tokens of full occurrences (issue #4). Without a
        // distance every term token in the snippet is in a covering window
        // (crossing the break too, when the best one does), so only a
        // distance narrows them; partial matching marks every term.
        let max_span = max_word_distance.saturating_add(denominator - 1);
        let masked = !partial
            && (max_word_distance != usize::MAX || ordered_best.is_some())
            && occurrence_marks(
                tokens,
                ordinals,
                prepared,
                &if ordered_best.is_some() { MarkRule::OrderedCover { max_span } } else { MarkRule::Cover { max_span } },
                (s_idx, e_idx),
                (i_left, i_right),
                brk,
                marks,
            );
        collect_mark_ranges(tokens, ordinals, snap_start, snap_end, mark_ranges, masked.then_some((&marks.mask[..], s_idx)));
        // Words between the ends, by position (a paired reading's second
        // reading shares its first reading's position).
        let word_dist = (tokens[i_right].position - tokens[i_left].position).saturating_sub(denominator - 1);

        let is_match = !ordered || ordered_best.is_some();

        if *match_mode == MatchMode::AllMatches && is_match {
            // every full occurrence in the text, by the best window's own
            // rule (issue #4): the whole text is the window
            let last = tokens.len() - 1;
            let masked = !partial
                && (max_word_distance != usize::MAX || ordered_best.is_some())
                && occurrence_marks(
                    tokens,
                    ordinals,
                    prepared,
                    &if ordered_best.is_some() { MarkRule::OrderedCover { max_span } } else { MarkRule::Cover { max_span } },
                    (0, last),
                    (i_left, i_right),
                    brk,
                    marks,
                );
            collect_mark_ranges(tokens, ordinals, 0, raw_len, mark_ranges, masked.then_some((&marks.mask[..], 0)));
            return SnippetDescriptor {
                is_match,
                score,
                word_distance: word_dist,
                window_word_count: tokens.len(),
                snap_start: 0,
                snap_end: raw_len,
                leading_ellipsis: false,
                trailing_ellipsis: false,
                mark_ranges,
            };
        }

        SnippetDescriptor {
            is_match,
            score,
            word_distance: word_dist,
            window_word_count: window_words,
            snap_start,
            snap_end,
            leading_ellipsis: snap_start > 0,
            trailing_ellipsis: snap_end < raw_len,
            mark_ranges,
        }
    }

    /// Otzaria's word proximity over the text last passed to
    /// [`tokenize`](Self::tokenize): a match only when every group sits
    /// where `proximity` allows (see [`Proximity`]). Reuses the per-token
    /// term ordinals (one hash probe per token, as `locate_tokenized`) and
    /// runs one sweep: ordered = one pass per group with a sliding maximum,
    /// any order = one two-pointer pass (a bipartite matching only when a
    /// token fills several groups). `word_distance` = words between the
    /// matched slots of the located window. Ignores `min_groups`.
    pub fn locate_proximity_tokenized(
        &mut self,
        prepared: &PreparedQueryGroups,
        proximity: &Proximity,
    ) -> SnippetDescriptor<'_> {
        self.locate_proximity_tokenized_mapped(prepared, &[], proximity)
    }

    /// [`locate_proximity_tokenized`](Self::locate_proximity_tokenized)
    /// with a term map (see [`locate_tokenized_mapped`](Self::locate_tokenized_mapped)).
    pub fn locate_proximity_tokenized_mapped(
        &mut self,
        prepared: &PreparedQueryGroups,
        term_map: &[u32],
        proximity: &Proximity,
    ) -> SnippetDescriptor<'_> {
        if prepared.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }
        let SnippetWindowLocator {
            context_words,
            text_tokens,
            ordinals,
            mark_ranges,
            proximity: scratch,
            break_token,
            token_terms,
            positioned,
            marks,
            match_mode,
            word_span,
            ..
        } = self;
        let brk = *break_token;
        let tokens = text_tokens.tokens();
        if tokens.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }
        let raw_len = text_tokens.raw_utf16_len();
        fill_ordinals(ordinals, text_tokens, prepared, positioned.then_some((&token_terms[..], term_map)));
        let ordinals = &ordinals[..];
        let k = prepared.group_count();
        let found = match proximity {
            Proximity::Ordered { gaps } => ordered_window(tokens, ordinals, prepared, gaps, brk, scratch, *word_span),
            Proximity::AnyOrder { max_span } => any_order_window(tokens, ordinals, prepared, *max_span as usize, brk, scratch, *word_span),
        };
        let Some((i_left, i_right, score)) = found else {
            return SnippetDescriptor::NO_MATCH;
        };
        if *match_mode == MatchMode::AllMatches {
            // every full occurrence in the text (issue #4 rule, whole text)
            let rule = match proximity {
                Proximity::Ordered { gaps } => MarkRule::Ordered(gaps),
                Proximity::AnyOrder { max_span } => MarkRule::AnyOrder(*max_span as usize),
            };
            let last = tokens.len() - 1;
            let masked = occurrence_marks(tokens, ordinals, prepared, &rule, (0, last), (i_left, i_right), brk, marks);
            collect_mark_ranges(tokens, ordinals, 0, raw_len, mark_ranges, masked.then_some((&marks.mask[..], 0)));
            return SnippetDescriptor {
                is_match: true,
                score,
                word_distance: (tokens[i_right].position - tokens[i_left].position).saturating_sub(k.saturating_sub(1)),
                window_word_count: tokens.len(),
                snap_start: 0,
                snap_end: raw_len,
                leading_ellipsis: false,
                trailing_ellipsis: false,
                mark_ranges,
            };
        }
        let (snap_start, snap_end, s_idx, e_idx) =
            expand_window(tokens, raw_len, i_left, i_right, *context_words);
        // Marks: only the tokens of full occurrences inside the snippet (issue #4).
        let rule = match proximity {
            Proximity::Ordered { gaps } => MarkRule::Ordered(gaps),
            Proximity::AnyOrder { max_span } => MarkRule::AnyOrder(*max_span as usize),
        };
        let masked = occurrence_marks(tokens, ordinals, prepared, &rule, (s_idx, e_idx), (i_left, i_right), brk, marks);
        collect_mark_ranges(tokens, ordinals, snap_start, snap_end, mark_ranges, masked.then_some((&marks.mask[..], s_idx)));
        SnippetDescriptor {
            is_match: true,
            score,
            word_distance: (tokens[i_right].position - tokens[i_left].position).saturating_sub(k.saturating_sub(1)),
            window_word_count: e_idx - s_idx + 1,
            snap_start,
            snap_end,
            leading_ellipsis: snap_start > 0,
            trailing_ellipsis: snap_end < raw_len,
            mark_ranges,
        }
    }

    /// Convenience for one-off literal-term locates (each term occurrence is
    /// its own AND slot). Prepares per call.
    pub fn locate_literal<T: AsRef<str>>(
        &mut self,
        raw_html: &str,
        query_terms: &[T],
    ) -> SnippetDescriptor<'_> {
        let prepared = PreparedQueryGroups::from_literal_terms(query_terms);
        self.locate(raw_html, &prepared, false, 0)
    }

    /// The per-side context this locator expands by.
    pub fn context_words(&self) -> usize {
        self.context_words
    }
}

/// A window's span for picking the tightest one: UTF-16 units from its
/// first token's start to its last token's end (FtsLib's rule), or, with
/// `word` (an index with positions, issue #21), word positions from its
/// first to its last token. Ties keep the earliest window either way.
#[inline]
fn span_of(tokens: &[TextToken], l: usize, r: usize, word: bool) -> usize {
    if word {
        tokens[r].position - tokens[l].position
    } else {
        tokens[r].utf16_end - tokens[l].utf16_start
    }
}

// ── Window finding ────────────────────────────────────────────────────
// Returns (i_left, i_right, score): token indices, not raw positions.

/// `required`: distinct groups a window must cover (the group count, or
/// fewer for partial matching). `brk` (cross-line): the windows must cross
/// the break at that token: a term token on each side. For each right end
/// on the next line, the tightest such window starts at the latest
/// covering left end if that is before the break, else at the first
/// line's last term token (a covering window stays covering when widened).
fn find_window(
    tokens: &[TextToken],
    ordinals: &[u32],
    prepared: &PreparedQueryGroups,
    group_count: &mut Vec<usize>,
    required: usize,
    brk: Option<usize>,
    word: bool,
) -> (usize, usize, usize) {
    let all_groups = prepared.group_count();
    if group_count.len() < all_groups {
        group_count.resize(all_groups * 2, 0);
    }
    group_count[..all_groups].fill(0);

    let mut covered = 0usize;
    let (mut best_i_left, mut best_i_right, mut best_score) = (usize::MAX, usize::MAX, usize::MAX);
    let mut l = 0usize;
    let last_left_term = match brk {
        Some(b) => match (0..b.min(tokens.len())).rev().find(|&t| ordinals[t] != NO_TERM) {
            Some(t) => Some(t),
            None => return (best_i_left, best_i_right, best_score),
        },
        None => None,
    };

    for r in 0..tokens.len() {
        if ordinals[r] != NO_TERM {
            for &rg in prepared.groups_at(ordinals[r] as usize) {
                group_count[rg] += 1;
                if group_count[rg] == 1 {
                    covered += 1;
                }
            }
        }

        // `>=`: with partial matching one token may belong to several groups
        // and push `covered` past `required` in one step.
        while covered >= required {
            // Score in raw chars (used only for picking the tightest window).
            let span = span_of(tokens, l, r, word);
            if brk.is_none() && span < best_score {
                best_score = span;
                best_i_left = l;
                best_i_right = r;
            }
            if ordinals[l] != NO_TERM {
                for &lg in prepared.groups_at(ordinals[l] as usize) {
                    group_count[lg] -= 1;
                    if group_count[lg] == 0 {
                        covered -= 1;
                    }
                }
            }
            l += 1;
        }
        // Cross-line: the last left end removed (l - 1, a term token) was
        // the latest covering one for this right end.
        if let (Some(b), Some(last)) = (brk, last_left_term) {
            if r >= b && ordinals[r] != NO_TERM && l > 0 {
                let x = if l - 1 < b { l - 1 } else { last };
                let span = span_of(tokens, x, r, word);
                if span < best_score {
                    best_score = span;
                    best_i_left = x;
                    best_i_right = r;
                }
            }
        }
    }

    (best_i_left, best_i_right, best_score)
}

// ── Proximity windows ─────────────────────────────────────────────────

/// A token's normalized word, scanned from its own span (see
/// [`TextTokens::word_at`]); false when the span is not one word.
fn word_of(text: &str, token: &TextToken, keep_marks: bool, out: &mut String) -> bool {
    out.clear();
    // plain Hebrew letters (the common word): the word is the span itself
    let raw = &text.as_bytes()[token.raw_start..token.raw_end];
    if raw.len() % 2 == 0 && raw.chunks_exact(2).all(|c| c[0] == 0xD7 && (0x90..=0xAA).contains(&c[1])) {
        out.push_str(&text[token.raw_start..token.raw_end]);
        return true;
    }
    let mut words = 0;
    crate::scanner::scan_with(&text[token.raw_start..token.raw_end], keep_marks, |w, _| {
        words += 1;
        if words == 1 {
            out.push_str(w);
        }
    });
    words == 1
}

/// Closes the occurrence group at `ptr` (one position): every occurrence
/// there must have met its word (`seen`, one bit per occurrence).
fn close_group(o: &[(u32, u32)], ptr: &mut usize, seen: &mut u64) -> bool {
    let at = o[*ptr].0;
    let mut n = 0;
    while *ptr < o.len() && o[*ptr].0 == at {
        if n >= 64 || *seen & (1u64 << n) == 0 {
            return false;
        }
        *ptr += 1;
        n += 1;
    }
    *seen = 0;
    true
}

/// Closes the group at `ptr` (if any) and demands no occurrence after it.
fn close_rest(o: &[(u32, u32)], ptr: &mut usize, seen: &mut u64) -> bool {
    *ptr >= o.len() || (close_group(o, ptr, seen) && *ptr == o.len())
}

/// Each token's ordinal in `prepared`: hashed from its normalized text, or
/// (`positioned`: token term ids and the term map) read off the ids.
fn fill_ordinals(
    ordinals: &mut Vec<u32>,
    text_tokens: &TextTokens,
    prepared: &PreparedQueryGroups,
    positioned: Option<(&[u32], &[u32])>,
) {
    ordinals.clear();
    match positioned {
        Some((ids, map)) => ordinals.extend(
            ids.iter()
                .map(|&i| if i == NO_TERM { NO_TERM } else { map.get(i as usize).copied().unwrap_or(NO_TERM) }),
        ),
        None => ordinals.extend(
            text_tokens
                .tokens()
                .iter()
                .map(|t| prepared.get(text_tokens.norm_of(t)).map_or(NO_TERM, |p| p.ordinal as u32)),
        ),
    }
}

fn in_group(ordinals: &[u32], prepared: &PreparedQueryGroups, t: usize, g: usize) -> bool {
    ordinals[t] != NO_TERM && prepared.groups_at(ordinals[t] as usize).contains(&g)
}

/// Ordered proximity: the tightest (i_left, i_right, char span) chain
/// `p0 < p1 < ...` with `p[i+1] - p[i] - 1 <= gaps[i]`. Per group, every
/// token where a valid chain can end keeps its LATEST start (the tightest
/// chain ending there); a sliding maximum over the previous group's ends
/// within `gap + 1` tokens gives it in one pass. None without a chain.
fn ordered_window(
    tokens: &[TextToken],
    ordinals: &[u32],
    prepared: &PreparedQueryGroups,
    gaps: &[u32],
    brk: Option<usize>,
    scratch: &mut ProximityScratch,
    word: bool,
) -> Option<(usize, usize, usize)> {
    let k = prepared.group_count();
    let ProximityScratch { chain, next, deque, .. } = scratch;
    chain.clear();
    // Cross-line (`brk`): a chain starts on the first line and ends on the next.
    let starts = brk.unwrap_or(usize::MAX).min(tokens.len());
    chain.extend((0..starts).filter(|&t| in_group(ordinals, prepared, t, 0)).map(|t| (t, t)));
    for g in 1..k {
        if chain.is_empty() {
            return None;
        }
        let reach = (*gaps.get(g - 1)? as usize).saturating_add(1);
        next.clear();
        deque.clear();
        let mut j = 0;
        for t in 0..tokens.len() {
            if !in_group(ordinals, prepared, t, g) {
                continue;
            }
            while j < chain.len() && tokens[chain[j].0].position < tokens[t].position {
                let e = chain[j];
                while deque.back().is_some_and(|b| b.1 <= e.1) {
                    deque.pop_back();
                }
                deque.push_back(e);
                j += 1;
            }
            while deque.front().is_some_and(|f| tokens[f.0].position.saturating_add(reach) < tokens[t].position) {
                deque.pop_front();
            }
            if let Some(&(_, start)) = deque.front() {
                next.push((t, start));
            }
        }
        std::mem::swap(chain, next);
    }
    let mut best: Option<(usize, usize, usize)> = None;
    for &(end, start) in chain.iter() {
        if brk.is_some_and(|b| end < b) {
            continue;
        }
        let span = span_of(tokens, start, end, word);
        if best.is_none_or(|b| span < b.2) {
            best = Some((start, end, span));
        }
    }
    best
}

/// Any-order proximity: the tightest (i_left, i_right, char span) window of
/// at most `max_span` positions where every group gets its own token. One
/// two-pointer pass: for each right end the latest feasible left end (the
/// feasible lefts only move right). Feasibility is a group/token matching
/// kept up to date as tokens enter and leave the window: only freed groups
/// are re-augmented, never the whole matching.
fn any_order_window(
    tokens: &[TextToken],
    ordinals: &[u32],
    prepared: &PreparedQueryGroups,
    max_span: usize,
    brk: Option<usize>,
    scratch: &mut ProximityScratch,
    word: bool,
) -> Option<(usize, usize, usize)> {
    let k = prepared.group_count();
    let groups_of = |t: usize| -> &[usize] {
        if ordinals[t] == NO_TERM {
            &[]
        } else {
            prepared.groups_at(ordinals[t] as usize)
        }
    };
    // positions never decrease along the tokens: compare each matched
    // token with the previous MATCHED one (an unmatched token may sit between)
    let shared = word && {
        let mut prev: Option<usize> = None;
        (0..tokens.len()).filter(|&t| ordinals[t] != NO_TERM).any(|t| {
            let same = prev == Some(tokens[t].position);
            prev = Some(tokens[t].position);
            same
        })
    };
    if !shared {
        return any_order_window_on(tokens, &groups_of, k, max_span, brk, scratch, word);
    }
    // The word rule with a paired reading among the matched words (two
    // readings at one position): one position fills one group, as on the
    // index positions, so each position becomes one token holding the union
    // of its readings' groups (issue #21).
    let mut merged: Vec<TextToken> = Vec::new();
    let mut first: Vec<usize> = Vec::new();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (t, tok) in tokens.iter().enumerate() {
        let gs = groups_of(t);
        if gs.is_empty() {
            continue;
        }
        if merged.last().is_some_and(|m: &TextToken| m.position == tok.position) {
            let g = groups.last_mut().unwrap();
            g.extend_from_slice(gs);
            g.sort_unstable();
            g.dedup();
        } else {
            merged.push(*tok);
            first.push(t);
            groups.push(gs.to_vec());
        }
    }
    let mbrk = brk.map(|b| first.partition_point(|&t| t < b));
    let (l, r, span) = any_order_window_on(&merged, &|t: usize| groups[t].as_slice(), k, max_span, mbrk, scratch, word)?;
    Some((first[l], first[r], span))
}

/// [`any_order_window`] over tokens whose groups `groups_of` gives (empty:
/// not a matched word).
fn any_order_window_on<'g>(
    tokens: &[TextToken],
    groups_of: &dyn Fn(usize) -> &'g [usize],
    k: usize,
    max_span: usize,
    brk: Option<usize>,
    scratch: &mut ProximityScratch,
    word: bool,
) -> Option<(usize, usize, usize)> {
    // Cross-line (`brk`): a feasible window crosses the break when it holds
    // a term token on each line (k >= 2: a group can then move its token to
    // the other side of a matching without breaking it).
    let last_left_term = match brk {
        Some(b) => {
            if k < 2 {
                return None;
            }
            Some((0..b.min(tokens.len())).rev().find(|&t| !groups_of(t).is_empty())?)
        }
        None => None,
    };
    scratch.reset_any_order(tokens.len(), k);
    let mut w = WindowCounts::default();
    let mut best: Option<(usize, usize, usize)> = None;
    let mut l = 0usize;
    for r in 0..tokens.len() {
        let gs = groups_of(r);
        if gs.is_empty() {
            continue; // a window never needs to end on a non-term token
        }
        scratch.add(r, gs, false, &mut w);
        while tokens[r].position - tokens[l].position > max_span {
            scratch.remove_front(l, groups_of(l), &mut w);
            l += 1;
        }
        if !scratch.complete(k, &mut w) {
            continue;
        }
        // Shrink from the left while the rest still gives every group a token.
        while l < r {
            let gs = groups_of(l);
            if gs.is_empty() {
                l += 1;
                continue;
            }
            if w.terms - 1 < k || gs.iter().any(|&g| scratch.coverage[g] == 1) {
                break;
            }
            scratch.remove_front(l, gs, &mut w);
            if scratch.complete(k, &mut w) {
                l += 1;
            } else {
                // put it back: the window was feasible with it
                scratch.add(l, gs, true, &mut w);
                let restored = scratch.complete(k, &mut w);
                debug_assert!(restored);
                break;
            }
        }
        let x = match (brk, last_left_term) {
            (Some(b), Some(last)) => {
                if r < b {
                    continue;
                }
                if l <= last {
                    l
                } else if tokens[r].position - tokens[last].position <= max_span {
                    last // widened back over the break: still feasible
                } else {
                    continue;
                }
            }
            _ => l,
        };
        let span = span_of(tokens, x, r, word);
        if best.is_none_or(|b| span < b.2) {
            best = Some((x, r, span));
        }
    }
    best
}

const FREE: usize = usize::MAX;

/// FtsLib in order: the tightest (i_left, i_right, char span) window holding
/// one token per group in query order (by token index, a token in one slot),
/// the earliest on a tie. Per group the latest chain start so far; a chain
/// ending at a token of the last group starts there. Cross-line (`brk`): the
/// chain starts on the first line and ends on the next. None without one.
/// `word` (the word rule, see [`span_of`]): the slots also follow in
/// strictly increasing positions, so the two readings of a paired reading
/// (one position) never chain: the rule reads positions only, the same on
/// the text and on the index positions.
#[inline(never)]
fn ordered_cover_window(
    tokens: &[TextToken],
    ordinals: &[u32],
    prepared: &PreparedQueryGroups,
    brk: Option<usize>,
    sc: &mut MarkScratch,
    word: bool,
) -> Option<(usize, usize, usize)> {
    let k = prepared.group_count();
    let MarkScratch { last, starts, ends: pending, .. } = sc;
    last.clear();
    last.resize(k, FREE);
    pending.clear();
    let mut at = usize::MAX; // the position whose chain starts are pending
    let mut best: Option<(usize, usize, usize)> = None;
    for t in 0..tokens.len() {
        if ordinals[t] == NO_TERM {
            continue;
        }
        if word && tokens[t].position != at {
            // a new position: the previous one's starts take effect
            for p in pending.chunks_exact(2) {
                let (g, v) = (p[0], p[1]);
                if last[g] == FREE || v > last[g] {
                    last[g] = v;
                }
            }
            pending.clear();
            at = tokens[t].position;
        }
        let gs = prepared.groups_at(ordinals[t] as usize);
        starts.clear();
        starts.extend(gs.iter().map(|&g| {
            if g == 0 {
                if brk.is_none_or(|b| t < b) { t } else { FREE }
            } else {
                last[g - 1]
            }
        }));
        for (&g, &v) in gs.iter().zip(starts.iter()) {
            if v == FREE {
                continue;
            }
            if g == k - 1 {
                if brk.is_none_or(|b| t >= b) {
                    let span = span_of(tokens, v, t, word);
                    if best.is_none_or(|b| span < b.2) {
                        best = Some((v, t, span));
                    }
                }
            } else if word {
                pending.extend([g, v]);
            } else if last[g] == FREE || v > last[g] {
                last[g] = v;
            }
        }
    }
    best
}

// ── Ordered-match validation ──────────────────────────────────────────

/// True when there exists a position where each query group is satisfied by
/// a token appearing strictly after the token satisfying the previous group.
/// Greedy forward scan.
#[cfg(test)]
fn has_ordered_match(ordinals: &[u32], prepared: &PreparedQueryGroups) -> bool {
    let num_groups = prepared.group_count();
    if num_groups <= 1 {
        return true;
    }
    let in_group = |ord: &u32, g: usize| *ord != NO_TERM && prepared.groups_at(*ord as usize).contains(&g);
    let tokens = ordinals;

    // Try every starting token that belongs to group 0.
    for start in 0..tokens.len() {
        if !in_group(&tokens[start], 0) {
            continue;
        }
        // Greedily advance through groups 1..num_groups.
        let mut pos = start + 1;
        let mut next_group = 1;
        while next_group < num_groups && pos < tokens.len() {
            if in_group(&tokens[pos], next_group) {
                next_group += 1;
            }
            pos += 1;
        }
        if next_group == num_groups {
            return true;
        }
    }
    false
}

// ── Window expansion ──────────────────────────────────────────────────

/// Expands the match window (token indices i_left..=i_right) by
/// `context_words` tokens on each side, with the visible-char safety ceiling.
/// Returns raw positions (snap_start, snap_end) plus the token indices
/// (s_idx, e_idx) of the window bounds.
fn expand_window(
    tokens: &[TextToken],
    raw_len: usize,
    i_left: usize,
    i_right: usize,
    context_words: usize,
) -> (usize, usize, usize, usize) {
    if tokens.is_empty() {
        return (0, raw_len, 0, 0);
    }

    // Expand by word count on each side: exact, reads from the token list.
    let mut s_idx = i_left.saturating_sub(context_words);
    let mut e_idx = (i_right + context_words).min(tokens.len() - 1);

    // Safety ceiling: trim from the outside only when the expanded window
    // exceeds SAFETY_CEILING visible chars. Never trims past the match
    // boundaries (i_left..=i_right must stay inside).
    while s_idx < e_idx {
        let vis_start = tokens[s_idx].visible_start;
        let vis_end = tokens[e_idx].visible_start + tokens[e_idx].letters;
        if vis_end - vis_start <= SAFETY_CEILING {
            break;
        }
        let can_trim_left = s_idx < i_left;
        let can_trim_right = e_idx > i_right;
        if !can_trim_left && !can_trim_right {
            break; // match itself exceeds ceiling: show it anyway
        }
        let trim_left = if can_trim_left {
            tokens[s_idx + 1].visible_start - vis_start
        } else {
            usize::MAX
        };
        let trim_right = if can_trim_right {
            vis_end - (tokens[e_idx - 1].visible_start + tokens[e_idx - 1].letters)
        } else {
            usize::MAX
        };
        if trim_left <= trim_right {
            s_idx += 1;
        } else {
            e_idx -= 1;
        }
    }

    // snap_start: showing from the first token of the line → 0, so no
    // ellipsis is prepended and no leading tag/whitespace is skipped.
    // Mid-line → the first letter of the first context word.
    let snap_start = if s_idx == 0 {
        0
    } else {
        tokens[s_idx].utf16_start
    };

    // snap_end: RawEnd of the last context token (includes its trailing
    // separator) rather than the next token's start.
    let snap_end = if e_idx + 1 < tokens.len() {
        tokens[e_idx].utf16_end
    } else {
        raw_len
    };

    (snap_start, snap_end.min(raw_len), s_idx, e_idx)
}

// ── Mark ranges ───────────────────────────────────────────────────────

/// Records every query-term token inside [snap_start, snap_end) as a flat
/// (rawStart, rawLength, termOrdinal) triple, document order, the length
/// clipped to snap_end.
fn collect_mark_ranges(
    tokens: &[TextToken],
    ordinals: &[u32],
    snap_start: usize,
    snap_end: usize,
    mark_ranges: &mut Vec<usize>,
    occurrence: Option<(&[bool], usize)>,
) {
    mark_ranges.clear();
    for (t, (tok, &ordinal)) in tokens.iter().zip(ordinals).enumerate() {
        if tok.utf16_end <= snap_start {
            continue;
        }
        if tok.utf16_start >= snap_end {
            break;
        }
        if ordinal == NO_TERM {
            continue;
        }
        // only tokens of a full occurrence inside the window (issue #4)
        if let Some((mask, s)) = occurrence {
            if t < s || !mask.get(t - s).copied().unwrap_or(false) {
                continue;
            }
        }
        let tok_end = tok.utf16_end.min(snap_end);
        mark_ranges.push(tok.utf16_start);
        mark_ranges.push(tok_end - tok.utf16_start);
        mark_ranges.push(ordinal as usize);
    }
}

// ── Occurrence marks (issue #4) ───────────────────────────────────────

/// The rule a full occurrence follows, the locate's own.
#[derive(Clone, Copy)]
enum MarkRule<'a> {
    /// FtsLib: a token range covering every group whose ends are at most
    /// `max_span` positions apart (`max_word_distance + groups - 1`).
    Cover { max_span: usize },
    /// FtsLib in order: an ordered chain (one token per group, in query
    /// order, by token index) whose ends are at most `max_span` positions apart.
    OrderedCover { max_span: usize },
    /// Otzaria's ordered proximity (per-pair gaps).
    Ordered(&'a [u32]),
    /// Otzaria's any-order proximity.
    AnyOrder(usize),
}

/// Scratch for [`occurrence_marks`], reused across lines.
#[derive(Default)]
struct MarkScratch {
    /// Per token of the snippet window (index - s): in a full occurrence.
    mask: Vec<bool>,
    counts: Vec<usize>,
    /// Ordered: per (group, token) chain prefix / suffix reachability.
    fwd: Vec<bool>,
    bwd: Vec<bool>,
    /// In order: per group the latest chain start so far; per (group,
    /// token) the latest prefix start / earliest suffix end (`FREE` = none).
    last: Vec<usize>,
    starts: Vec<usize>,
    ends: Vec<usize>,
    /// Any order: group of each token, token of each group, visit stamps.
    owner: Vec<usize>,
    taken: Vec<usize>,
    stamp: Vec<u32>,
    generation: u32,
}

/// Marks in `scratch.mask` every term token of tokens `s..=e` that belongs
/// to a full occurrence of the query lying inside `s..=e` under `rule`
/// (cross-line, `brk`: an occurrence with a token on each line). False when
/// every term token qualifies anyway (no mask needed): the cheap check that
/// keeps the common hit free, all term tokens inside the best window
/// `best` and, for proximity, exactly one per group.
#[allow(clippy::too_many_arguments)]
fn occurrence_marks(
    tokens: &[TextToken],
    ordinals: &[u32],
    prepared: &PreparedQueryGroups,
    rule: &MarkRule<'_>,
    (s, e): (usize, usize),
    best: (usize, usize),
    brk: Option<usize>,
    sc: &mut MarkScratch,
) -> bool {
    let k = prepared.group_count();
    let mut n_terms = 0usize;
    let mut outside = false;
    for t in s..=e {
        if ordinals[t] != NO_TERM {
            n_terms += 1;
            outside |= t < best.0 || t > best.1;
        }
    }
    if !outside && (matches!(rule, MarkRule::Cover { .. }) || n_terms == k) {
        // (in order: the best window is an ordered chain; with exactly k
        // term tokens every one of them is in it)
        return false;
    }
    let w = e - s + 1;
    sc.mask.clear();
    sc.mask.resize(w, false);
    let groups_of = |t: usize| -> &[usize] {
        if ordinals[t] == NO_TERM {
            &[]
        } else {
            prepared.groups_at(ordinals[t] as usize)
        }
    };
    let pos = |t: usize| tokens[t].position;
    // cross-line: the first line's last term token in the window
    let last_left = match brk {
        Some(b) => match (s..b.min(e + 1)).rev().find(|&t| ordinals[t] != NO_TERM) {
            Some(t) => Some(t),
            None => return true, // no crossing occurrence in the window
        },
        None => None,
    };
    // Any order with one group per token and no break: a window covering
    // every group IS an occurrence (its tokens are distinct), so the cheap
    // covering sweep decides it.
    let rule = match *rule {
        MarkRule::AnyOrder(m) if brk.is_none() && (s..=e).all(|t| groups_of(t).len() <= 1) => {
            MarkRule::Cover { max_span: m }
        }
        r => r,
    };
    match rule {
        MarkRule::Cover { max_span } => {
            // Per right end r (a term token): the covering windows [l', r]
            // are those with l' <= l - 1 (the latest covering left end);
            // their union within the span is [lo, r].
            sc.counts.clear();
            sc.counts.resize(k, 0);
            let (mut covered, mut l, mut lo, mut marked_to) = (0usize, s, s, s);
            for r in s..=e {
                for &g in groups_of(r) {
                    sc.counts[g] += 1;
                    covered += usize::from(sc.counts[g] == 1);
                }
                while covered >= k {
                    for &g in groups_of(l) {
                        sc.counts[g] -= 1;
                        covered -= usize::from(sc.counts[g] == 0);
                    }
                    l += 1;
                }
                if ordinals[r] == NO_TERM || l == s {
                    continue;
                }
                let mut x = l - 1;
                if let (Some(b), Some(last)) = (brk, last_left) {
                    if r < b {
                        continue;
                    }
                    x = x.min(last);
                }
                while pos(r) - pos(lo) > max_span {
                    lo += 1;
                }
                if lo <= x {
                    for t in lo.max(marked_to)..=r {
                        sc.mask[t - s] = true;
                    }
                    marked_to = r + 1;
                }
            }
        }
        MarkRule::OrderedCover { max_span } => ordered_cover_marks(tokens, ordinals, prepared, (s, e), brk, max_span, sc),
        MarkRule::Ordered(gaps) => {
            // A chain through t at slot g = a prefix chain ending at t and
            // a suffix chain starting at t.
            sc.fwd.clear();
            sc.fwd.resize(k * w, false);
            sc.bwd.clear();
            sc.bwd.resize(k * w, false);
            let in_g = |t: usize, g: usize| groups_of(t).contains(&g);
            let reach = |g: usize| gaps.get(g).map_or(0, |&x| x as usize + 1);
            for t in s..=e {
                if in_g(t, 0) && brk.is_none_or(|b| t < b) {
                    sc.fwd[t - s] = true;
                }
                if in_g(t, k - 1) && brk.is_none_or(|b| t >= b) {
                    sc.bwd[(k - 1) * w + t - s] = true;
                }
            }
            for g in 1..k {
                let (r, mut last, mut j) = (reach(g - 1), None::<usize>, s);
                for t in s..=e {
                    if !in_g(t, g) {
                        continue;
                    }
                    while j <= e && pos(j) < pos(t) {
                        if sc.fwd[(g - 1) * w + j - s] {
                            last = Some(pos(j));
                        }
                        j += 1;
                    }
                    if last.is_some_and(|p| pos(t) - p <= r) {
                        sc.fwd[g * w + t - s] = true;
                    }
                }
            }
            for g in (0..k.saturating_sub(1)).rev() {
                let (r, mut next, mut j) = (reach(g), None::<usize>, e + 1);
                for t in (s..=e).rev() {
                    if !in_g(t, g) {
                        continue;
                    }
                    while j > s && pos(j - 1) > pos(t) {
                        j -= 1;
                        if sc.bwd[(g + 1) * w + j - s] {
                            next = Some(pos(j));
                        }
                    }
                    if next.is_some_and(|p| p - pos(t) <= r) {
                        sc.bwd[g * w + t - s] = true;
                    }
                }
            }
            for t in s..=e {
                sc.mask[t - s] = (0..k).any(|g| sc.fwd[g * w + t - s] && sc.bwd[g * w + t - s]);
            }
        }
        MarkRule::AnyOrder(max_span) => {
            sc.owner.clear();
            sc.owner.resize(w, FREE);
            sc.taken.clear();
            sc.taken.resize(k, FREE);
            sc.stamp.clear();
            sc.stamp.resize(w, 0);
            for t in s..=e {
                if ordinals[t] == NO_TERM || sc.mask[t - s] {
                    continue;
                }
                // windows [a, hi] of at most max_span positions holding t
                'starts: for a in s..=t {
                    if ordinals[a] == NO_TERM || pos(t) - pos(a) > max_span {
                        continue;
                    }
                    let mut hi = t;
                    while hi < e && pos(hi + 1) - pos(a) <= max_span {
                        hi += 1;
                    }
                    for &gt in groups_of(t) {
                        let found = match brk {
                            None => any_order_match(groups_of, (a, hi), s, &[(t, gt)], k, sc),
                            // cross-line: also force a token on the other line
                            Some(b) => (a..=hi).filter(|&u| u != t && ((u < b) != (t < b))).any(|u| {
                                groups_of(u)
                                    .iter()
                                    .any(|&gu| gu != gt && any_order_match(groups_of, (a, hi), s, &[(t, gt), (u, gu)], k, sc))
                            }),
                        };
                        if found {
                            for g in 0..k {
                                sc.mask[sc.taken[g] - s] = true;
                            }
                            break 'starts;
                        }
                    }
                }
            }
        }
    }
    true
}

/// [`MarkRule::OrderedCover`] marks (see [`occurrence_marks`]); out of line so
/// the order-off paths keep their code layout.
#[inline(never)]
fn ordered_cover_marks(
    tokens: &[TextToken],
    ordinals: &[u32],
    prepared: &PreparedQueryGroups,
    (s, e): (usize, usize),
    brk: Option<usize>,
    max_span: usize,
    sc: &mut MarkScratch,
) {
    let k = prepared.group_count();
    let w = e - s + 1;
    let groups_of = |t: usize| -> &[usize] {
        if ordinals[t] == NO_TERM {
            &[]
        } else {
            prepared.groups_at(ordinals[t] as usize)
        }
    };
    let pos = |t: usize| tokens[t].position;
    // t in slot g is in an occurrence when the latest prefix chain
    // start ending at t and the earliest suffix chain end starting
    // at t are at most max_span positions apart.
    let MarkScratch { last, starts, ends, mask, .. } = sc;
    starts.clear();
    starts.resize(k * w, FREE);
    ends.clear();
    ends.resize(k * w, FREE);
    last.clear();
    last.resize(k, FREE);
    for t in s..=e {
        // slots read the previous group's state before this token
        // updates it (a token fills one slot of a chain)
        for &g in groups_of(t).iter() {
            let v = if g == 0 { if brk.is_none_or(|b| t < b) { t } else { FREE } } else { last[g - 1] };
            starts[g * w + t - s] = v;
        }
        for &g in groups_of(t).iter() {
            let v = starts[g * w + t - s];
            if v != FREE && (last[g] == FREE || v > last[g]) {
                last[g] = v;
            }
        }
    }
    last.fill(FREE);
    for t in (s..=e).rev() {
        for &g in groups_of(t).iter() {
            let v = if g == k - 1 { if brk.is_none_or(|b| t >= b) { t } else { FREE } } else { last[g + 1] };
            ends[g * w + t - s] = v;
        }
        for &g in groups_of(t).iter() {
            let v = ends[g * w + t - s];
            if v != FREE && (last[g] == FREE || v < last[g]) {
                last[g] = v;
            }
        }
    }
    for t in s..=e {
        mask[t - s] = groups_of(t).iter().any(|&g| {
            let (a, z) = (starts[g * w + t - s], ends[g * w + t - s]);
            a != FREE && z != FREE && pos(z) - pos(a) <= max_span
        });
    }
}

/// Whether every group gets its own token of `a..=hi`, the `forced`
/// (token, group) pairs fixed. On success `sc.taken` holds the matching.
fn any_order_match<'g>(
    groups_of: impl Fn(usize) -> &'g [usize] + Copy,
    (a, hi): (usize, usize),
    s: usize,
    forced: &[(usize, usize)],
    k: usize,
    sc: &mut MarkScratch,
) -> bool {
    for t in a..=hi {
        sc.owner[t - s] = FREE;
    }
    sc.taken.iter_mut().for_each(|x| *x = FREE);
    for &(t, g) in forced.iter() {
        sc.owner[t - s] = g;
        sc.taken[g] = t;
    }
    fn kuhn<'g>(
        g: usize,
        groups_of: impl Fn(usize) -> &'g [usize] + Copy,
        (a, hi): (usize, usize),
        s: usize,
        forced: &[(usize, usize)],
        sc: &mut MarkScratch,
    ) -> bool {
        for t in a..=hi {
            if sc.stamp[t - s] == sc.generation || !groups_of(t).contains(&g) || forced.iter().any(|f| f.0 == t) {
                continue;
            }
            sc.stamp[t - s] = sc.generation;
            let holder = sc.owner[t - s];
            if holder == FREE || kuhn(holder, groups_of, (a, hi), s, forced, sc) {
                sc.owner[t - s] = g;
                sc.taken[g] = t;
                return true;
            }
        }
        false
    }
    for g in 0..k {
        if sc.taken[g] != FREE {
            continue;
        }
        sc.generation = sc.generation.wrapping_add(1);
        if sc.generation == 0 {
            sc.stamp.iter_mut().for_each(|x| *x = 0);
            sc.generation = 1;
        }
        if !kuhn(g, groups_of, (a, hi), s, forced, sc) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }
    fn slice16(s: &str, start: usize, end: usize) -> String {
        String::from_utf16(&utf16(s)[start..end]).unwrap()
    }

    #[test]
    fn no_match_when_a_group_is_absent() {
        let mut loc = SnippetWindowLocator::default();
        let p = PreparedQueryGroups::from_groups(&[vec!["אב"], vec!["זז"]]);
        let d = loc.locate("אב גד הו", &p, false, 0);
        assert!(!d.is_match);
        assert_eq!(d.score, usize::MAX);
        assert!(d.mark_ranges.is_empty());
    }

    #[test]
    fn tightest_window_and_tie_keeps_earliest() {
        let mut loc = SnippetWindowLocator::new(0);
        // "אב" appears twice; the window אב..גד at the end is tighter than אב(0)..גד
        let text = "אב קק קק קק אב גד";
        let p = PreparedQueryGroups::from_groups(&[vec!["אב"], vec!["גד"]]);
        let d = loc.locate(text, &p, false, 0);
        assert!(d.is_match);
        assert_eq!(slice16(text, d.snap_start, d.snap_end), "אב גד");
        assert_eq!(d.word_distance, 0);
        // tie: two equal windows → the earlier one wins (strict <)
        let text2 = "אב גד קק אב גד";
        let d2 = loc.locate(text2, &p, false, 0);
        assert_eq!(d2.snap_start, 0);
        assert_eq!(slice16(text2, d2.snap_start, d2.snap_end), "אב גד");
    }

    #[test]
    fn context_snap_and_ellipsis() {
        let mut loc = SnippetWindowLocator::new(1);
        let text = "אא בב גג דד הה";
        let p = PreparedQueryGroups::from_groups(&[vec!["גג"]]);
        let d = loc.locate(text, &p, false, 0);
        // 1 word of context each side: "בב גג דד"
        assert_eq!(slice16(text, d.snap_start, d.snap_end), "בב גג דד");
        assert!(d.leading_ellipsis);
        assert!(d.trailing_ellipsis);
        assert_eq!(d.window_word_count, 3);
        // at the line start: snap_start = 0 and no leading ellipsis
        let p0 = PreparedQueryGroups::from_groups(&[vec!["אא"]]);
        let d0 = loc.locate(text, &p0, false, 0);
        assert_eq!(d0.snap_start, 0);
        assert!(!d0.leading_ellipsis);
        // at the line end: snap_end = raw length, no trailing ellipsis
        let p4 = PreparedQueryGroups::from_groups(&[vec!["הה"]]);
        let d4 = loc.locate(text, &p4, false, 0);
        assert_eq!(d4.snap_end, utf16(text).len());
        assert!(!d4.trailing_ellipsis);
    }

    #[test]
    fn marks_every_query_token_in_window_with_ordinal() {
        let mut loc = SnippetWindowLocator::new(8);
        let text = "גג אב קק גג אב";
        let p = PreparedQueryGroups::from_groups(&[vec!["אב"], vec!["גג"]]);
        let d = loc.locate(text, &p, false, 0);
        let marks: Vec<_> = d.marks().collect();
        // all 4 query tokens are in the (whole-line) window; ordinals: אב=0, גג=1
        assert_eq!(marks.len(), 4);
        assert_eq!(marks[0], (0, 2, 1));
        assert_eq!(marks[1], (3, 2, 0));
        assert_eq!(marks[2], (9, 2, 1));
        assert_eq!(marks[3], (12, 2, 0));
        for (start, len, _) in &marks {
            let w = slice16(text, *start, start + len);
            assert!(w == "אב" || w == "גג", "{w}");
        }
    }

    #[test]
    fn word_distance_and_original_group_count() {
        let mut loc = SnippetWindowLocator::new(0);
        let text = "אב קק קק גד";
        let p = PreparedQueryGroups::from_groups(&[vec!["אב"], vec!["גד"]]);
        assert_eq!(loc.locate(text, &p, false, 0).word_distance, 2);
        // a third (skipped-wildcard) slot counted in the denominator
        assert_eq!(loc.locate(text, &p, false, 3).word_distance, 1);
    }

    #[test]
    fn paired_reading_counts_one_position() {
        // (X) [Y] Z: Y shares X's position, so Y..Z and X..Z are adjacent.
        let text = "\u{05D0}\u{05D1} (\u{05D2}\u{05D3}) [\u{05D4}\u{05D5}] \u{05D6}\u{05D7}";
        let mut loc = SnippetWindowLocator::new(0);
        let p = PreparedQueryGroups::from_groups(&[vec!["\u{05D0}\u{05D1}"], vec!["\u{05D6}\u{05D7}"]]);
        assert_eq!(loc.locate(text, &p, false, 0).word_distance, 1);
        let ordered = Proximity::Ordered { gaps: vec![0] };
        assert!(prox_lines(text, &[&["\u{05D4}\u{05D5}"], &["\u{05D6}\u{05D7}"]], &ordered).is_some());
        assert!(prox_lines(text, &[&["\u{05D2}\u{05D3}"], &["\u{05D6}\u{05D7}"]], &ordered).is_some());
        assert!(prox_lines(text, &[&["\u{05D0}\u{05D1}"], &["\u{05D4}\u{05D5}"]], &ordered).is_some());
        assert!(prox_lines(text, &[&["\u{05D0}\u{05D1}"], &["\u{05D6}\u{05D7}"]], &ordered).is_none());
        let window = Proximity::AnyOrder { max_span: 1 };
        assert!(prox_lines(text, &[&["\u{05D6}\u{05D7}"], &["\u{05D4}\u{05D5}"]], &window).is_some());
    }

    #[test]
    fn token_in_two_groups_covers_both() {
        let mut loc = SnippetWindowLocator::new(0);
        let p = PreparedQueryGroups::from_groups(&[vec!["אב"], vec!["אב", "גד"]]);
        let d = loc.locate("קק אב קק", &p, false, 0);
        assert!(d.is_match);
        assert_eq!(d.window_word_count, 1);
    }

    #[test]
    fn ordered_match() {
        let mut loc = SnippetWindowLocator::new(0);
        let p = PreparedQueryGroups::from_groups(&[vec!["אב"], vec!["גד"]]);
        assert!(loc.locate("גד אב", &p, false, 0).is_match);
        assert!(!loc.locate("גד אב", &p, true, 0).is_match);
        assert!(loc.locate("אב גד", &p, true, 0).is_match);
    }

    #[test]
    fn safety_ceiling_never_trims_into_the_match() {
        let mut loc = SnippetWindowLocator::new(8);
        // two groups 300 long words apart: the match itself exceeds 4000
        // visible chars → context is trimmed away, match kept whole
        let long = "א".repeat(29);
        let mut words = vec!["אב".to_string()];
        words.extend(std::iter::repeat(long.clone()).take(300));
        words.push("גד".to_string());
        let text = format!("{} {} {}", long, words.join(" "), long);
        let p = PreparedQueryGroups::from_groups(&[vec!["אב"], vec!["גד"]]);
        let d = loc.locate(&text, &p, false, 0);
        assert!(d.is_match);
        let shown = slice16(&text, d.snap_start, d.snap_end);
        assert!(shown.starts_with("אב"));
        assert!(shown.ends_with("גד"));
        assert_eq!(d.window_word_count, 302);
    }

    #[test]
    fn partial_matching_min_groups() {
        let mut loc = SnippetWindowLocator::new(0);
        // one token in two groups: covered jumps 0 -> 2 with min_groups 1
        let p = PreparedQueryGroups::from_groups(&[vec!["תורה"], vec!["תורה", "משה"], vec!["סיני"]]);
        loc.set_min_groups(1);
        let d = loc.locate("אב תורה גד", &p, true, 0);
        assert!(d.is_match);
        assert_eq!(d.word_distance, 0);
        // min 2 of 3: the tightest window holding two groups; order ignored
        loc.set_min_groups(2);
        let d = loc.locate("סיני אב משה גד תורה", &p, true, 0);
        assert!(d.is_match);
        assert_eq!(d.word_distance, 0); // תורה alone is groups 0+1: a one-token window
        let d = loc.locate("סיני אב משה גד", &p, true, 0);
        assert!(d.is_match);
        assert_eq!(d.word_distance, 1); // "סיני אב משה": groups 2 and 1
        // all groups (0): FtsLib's rule, order enforced
        loc.set_min_groups(0);
        let d = loc.locate("סיני אב משה גד תורה", &p, true, 0);
        assert!(!d.is_match);
    }

    fn prox_lines(text: &str, groups: &[&[&str]], proximity: &Proximity) -> Option<(String, usize)> {
        let mut loc = SnippetWindowLocator::new(0);
        let p = PreparedQueryGroups::from_groups(groups);
        loc.tokenize(text);
        let d = loc.locate_proximity_tokenized(&p, proximity);
        d.is_match.then(|| (slice16(text, d.snap_start, d.snap_end).trim_end().to_string(), d.word_distance))
    }

    #[test]
    fn proximity_ordered_per_pair_gaps() {
        let ordered = |gaps: &[u32]| Proximity::Ordered { gaps: gaps.to_vec() };
        let g: &[&[&str]] = &[&["ויאמר"], &["אל"], &["משה"]];
        // Otzaria's advanced_custom_spacing_is_enforced_per_pair
        let spacing = ordered(&[2, 0]);
        assert!(prox_lines("ויאמר ה' צבאות אל משה", g, &spacing).is_some());
        assert!(prox_lines("ויאמר אל העם ואל משה", g, &spacing).is_none());
        // exactly at the limit / one past it
        assert!(prox_lines("ויאמר אא בב אל משה", g, &spacing).is_some());
        assert!(prox_lines("ויאמר אא בב גג אל משה", g, &spacing).is_none());
        // order matters
        assert!(prox_lines("משה אל ויאמר", g, &ordered(&[5, 5])).is_none());
        // a later start is the only one that reaches the next word
        let (w, d) = prox_lines("ויאמר אא בב גג ויאמר אל משה", g, &spacing).unwrap();
        assert_eq!((w.as_str(), d), ("ויאמר אל משה", 0));
        // the tightest satisfying window, not the tightest cover
        let two: &[&[&str]] = &[&["אב"], &["גד"]];
        let (w, _) = prox_lines("גד אב קק גד", two, &ordered(&[3])).unwrap();
        assert_eq!(w, "אב קק גד");
    }

    #[test]
    fn proximity_any_order_window() {
        let any = |max_span| Proximity::AnyOrder { max_span };
        let g: &[&[&str]] = &[&["חייב"], &["להחזיר"]];
        // Otzaria's any_order_finds_reversed_words_within_the_window_only:
        // two words, distance d → max_span d + 1
        let lines = ["חייב להחזיר", "להחזיר חייב", "להחזיר את החפץ חייב", "להחזיר את החפץ הזה חייב", "חייב לשלם ולא להחזיר"];
        let hits = |span| (0..lines.len()).filter(|&i| prox_lines(lines[i], g, &any(span)).is_some()).collect::<Vec<_>>();
        assert_eq!(hits(1), [0, 1]);
        assert_eq!(hits(3), [0, 1, 2, 4]);
        assert_eq!(hits(4), [0, 1, 2, 3, 4]);
        // repeated words each need their own token
        let twice: &[&[&str]] = &[&["אמר"], &["אמר"]];
        assert!(prox_lines("אמר רבי יוחנן", twice, &any(5)).is_none());
        assert!(prox_lines("אמר רבי אמר", twice, &any(1)).is_none());
        assert!(prox_lines("אמר רבי אמר", twice, &any(2)).is_some());
        let three: &[&[&str]] = &[&["רבי"], &["אמר"], &["רבי"]];
        assert!(prox_lines("אמר רבי אמר", three, &any(10)).is_none());
        // a token in two groups: the matching gives each group its own
        let shared: &[&[&str]] = &[&["אב", "גד"], &["אב"]];
        assert!(prox_lines("אב קק", shared, &any(5)).is_none());
        let (w, _) = prox_lines("אב קק גד", shared, &any(5)).unwrap();
        assert_eq!(w, "אב קק גד");
        // the tightest window wins
        let (w, d) = prox_lines("להחזיר את חייב להחזיר", g, &any(5)).unwrap();
        assert_eq!((w.as_str(), d), ("חייב להחזיר", 0));
    }

    /// Brute force: the tightest window (token span, then earliest) where
    /// every group takes its own token, last minus first <= max_span.
    fn any_order_reference(groups_per_token: &[Vec<usize>], k: usize, max_span: usize) -> Option<(usize, usize)> {
        fn place(g: usize, k: usize, l: usize, r: usize, gpt: &[Vec<usize>], used: &mut Vec<usize>) -> bool {
            if g == k {
                return true;
            }
            for t in l..=r {
                if !used.contains(&t) && gpt[t].contains(&g) {
                    used.push(t);
                    if place(g + 1, k, l, r, gpt, used) {
                        return true;
                    }
                    used.pop();
                }
            }
            false
        }
        let n = groups_per_token.len();
        let mut best: Option<(usize, usize)> = None;
        for r in 0..n {
            for l in (r.saturating_sub(max_span)..=r).rev() {
                if place(0, k, l, r, groups_per_token, &mut Vec::new()) {
                    if best.is_none_or(|b| r - l < b.1 - b.0) {
                        best = Some((l, r));
                    }
                    break;
                }
            }
        }
        best
    }

    #[test]
    fn proximity_any_order_equals_brute_force() {
        // words of equal width, so the char span orders like the token span
        let vocab = ["אב", "גד", "הו", "זח", "קק"];
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut loc = SnippetWindowLocator::new(0);
        for _ in 0..3000 {
            let k = 1 + next(4) as usize;
            // each group: one or two of the first four words (shared tokens)
            let groups: Vec<Vec<&str>> = (0..k)
                .map(|_| {
                    let a = vocab[next(4) as usize];
                    let b = vocab[next(4) as usize];
                    if next(2) == 0 { vec![a] } else { vec![a, b] }
                })
                .collect();
            let n = 1 + next(12) as usize;
            let words: Vec<&str> = (0..n).map(|_| vocab[next(5) as usize]).collect();
            let text = words.join(" ");
            let max_span = next(6) as usize;
            let gpt: Vec<Vec<usize>> = words
                .iter()
                .map(|w| (0..k).filter(|&g| groups[g].contains(w)).collect())
                .collect();
            let want = any_order_reference(&gpt, k, max_span);
            let p = PreparedQueryGroups::from_groups(&groups);
            loc.tokenize(&text);
            let d = loc.locate_proximity_tokenized(&p, &Proximity::AnyOrder { max_span: max_span as u32 });
            let got = d.is_match.then(|| {
                let start = (0..n).position(|i| words[..i].iter().map(|w| w.chars().count() + 1).sum::<usize>() == d.snap_start).unwrap();
                (start, start + d.word_distance + k - 1)
            });
            // the located window's token span must equal the reference's
            assert_eq!(got.map(|(l, r)| r - l), want.map(|(l, r)| r - l), "{text} {groups:?} span {max_span}");
            assert_eq!(got.is_some(), want.is_some());
        }
    }

    #[test]
    fn proximity_any_order_dense_broad_group_is_fast() {
        // 2,000 tokens; three groups take every token (a broad walk), two
        // take a word found every sixth token: no window of span 4 holds two
        // of those, so every window is matched and fails.
        let mut words = Vec::new();
        for i in 0..2000 {
            words.push(if i % 6 == 0 { "בב" } else { "אא" });
        }
        let text = words.join(" ");
        let broad = vec!["אא", "בב"];
        let groups = vec![broad.clone(), broad.clone(), broad, vec!["בב"], vec!["בב"]];
        let p = PreparedQueryGroups::from_groups(&groups);
        let mut loc = SnippetWindowLocator::new(0);
        let t = std::time::Instant::now();
        loc.tokenize(&text);
        assert!(!loc.locate_proximity_tokenized(&p, &Proximity::AnyOrder { max_span: 4 }).is_match);
        assert!(loc.locate_proximity_tokenized(&p, &Proximity::AnyOrder { max_span: 6 }).is_match);
        assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?}", t.elapsed());
    }

    #[test]
    fn inline_tags_inside_window_offsets() {
        let mut loc = SnippetWindowLocator::new(0);
        let text = "קק ורא<b>ה</b> טוב";
        let p = PreparedQueryGroups::from_groups(&[vec!["וראה"]]);
        let d = loc.locate(text, &p, false, 0);
        let (start, len, _) = d.marks().next().unwrap();
        assert_eq!(slice16(text, start, start + len), "ורא<b>ה</b>");
    }

    /// The index's view of a text: each word's (position, term id) for the
    /// words that are `terms`, as the postings give them (one per position
    /// and term).
    fn postings_of(text: &str, terms: &[String]) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        crate::scanner::scan_with(text, false, |w, s| {
            if let Some(i) = terms.iter().position(|t| t == w) {
                out.push((s.position as u32, i as u32));
            }
        });
        out.sort_unstable();
        out.dedup();
        out
    }

    #[test]
    fn positioned_locate_equals_hashed_locate() {
        let a = "\u{05E9}\u{05DC}\u{05D5}\u{05DD}";
        let b = "\u{05D1}\u{05D9}\u{05EA}";
        let c = "\u{05DE}\u{05E9}\u{05D4}";
        let texts = [
            format!("{a} x {b} {c} {a} y y {b}"),
            format!("({a}) [{a}] {b} <b>{c}</b> &nbsp; {a}\u{05B8} {b}"),
            format!("{c} {}", "z ".repeat(40) + b),
            format!("{a}\"{b} {a}' {b},{c}. {a}"),
            format!("{}{a} {b}", "w ".repeat(30)),
            format!("{a} {b} {}", "q ".repeat(50)),
            String::new(),
            "x y z".to_string(),
        ];
        let groups = [vec![a.to_string()], vec![b.to_string()]];
        let p = PreparedQueryGroups::from_groups(&groups);
        let terms: Vec<String> = vec![a.into(), b.into(), c.into()];
        let map: Vec<u32> = terms.iter().map(|t| p.get(t).map_or(u32::MAX, |x| x.ordinal as u32)).collect();
        for ctx in [0, 1, 3, 10] {
            for t in &texts {
                let occ = postings_of(t, &terms);
                let mut x = SnippetWindowLocator::new(ctx);
                let mut y = SnippetWindowLocator::new(ctx);
                x.tokenize(t);
                assert!(y.tokenize_positioned(t, &[0], &[&occ], &terms), "{}", t.escape_unicode());
                for ordered in [false, true] {
                    let dx = x.locate_tokenized(&p, ordered, 0);
                    let ex = (dx.is_match, dx.score, dx.word_distance, dx.window_word_count, dx.snap_start, dx.snap_end, dx.mark_ranges.to_vec());
                    let dy = y.locate_tokenized_mapped(&p, &map, ordered, 0);
                    let ey = (dy.is_match, dy.score, dy.word_distance, dy.window_word_count, dy.snap_start, dy.snap_end, dy.mark_ranges.to_vec());
                    assert_eq!(ex, ey, "ctx {ctx} {}", t.escape_unicode());
                }
                for prox in [Proximity::Ordered { gaps: vec![1] }, Proximity::AnyOrder { max_span: 3 }] {
                    let dx = x.locate_proximity_tokenized(&p, &prox);
                    let ex = (dx.is_match, dx.score, dx.snap_start, dx.snap_end, dx.mark_ranges.to_vec());
                    let dy = y.locate_proximity_tokenized_mapped(&p, &map, &prox);
                    let ey = (dy.is_match, dy.score, dy.snap_start, dy.snap_end, dy.mark_ranges.to_vec());
                    assert_eq!(ex, ey, "ctx {ctx} {}", t.escape_unicode());
                }
            }
        }
        // two lines joined by a space: positions local to each line
        let (l1, l2) = (&texts[0], &texts[2]);
        let joined = format!("{l1} {l2}");
        let (o1, o2) = (postings_of(l1, &terms), postings_of(l2, &terms));
        let mut x = SnippetWindowLocator::new(3);
        let mut y = SnippetWindowLocator::new(3);
        x.tokenize(&joined);
        assert!(y.tokenize_positioned(&joined, &[0, l1.len() + 1], &[&o1, &o2], &terms));
        let dx = x.locate_tokenized(&p, false, 0);
        let ex = (dx.score, dx.snap_start, dx.snap_end, dx.mark_ranges.to_vec());
        let dy = y.locate_tokenized_mapped(&p, &map, false, 0);
        assert_eq!(ex, (dy.score, dy.snap_start, dy.snap_end, dy.mark_ranges.to_vec()));
        // positions that do not match the text: refused (the caller falls back)
        let wrong = postings_of(&texts[1], &terms);
        assert!(!y.tokenize_positioned(&texts[0], &[0], &[&wrong], &terms));
    }

    #[test]
    fn light_tokens_equal_full_tokens_and_the_scan_stops() {
        let text = "\u{05D0}\u{05D1} (\u{05D2}) [\u{05D3}] <b>\u{05D4}</b>\u{05B8} &amp; 12 \u{05D5}\"\u{05D6} end";
        let mut full = TextTokens::default();
        let a: Vec<_> = full.tokenize(text).iter().map(|t| (t.utf16_start, t.utf16_end, t.visible_start, t.letters, t.position)).collect();
        let len = full.raw_utf16_len();
        let mut light = TextTokens::default();
        let b: Vec<_> =
            light.tokenize_light(text, |_| true).iter().map(|t| (t.utf16_start, t.utf16_end, t.visible_start, t.letters, t.position)).collect();
        assert_eq!(a, b);
        assert_eq!(len, light.raw_utf16_len());
        let mut seen = 0;
        let c = light.tokenize_light(text, |_| {
            seen += 1;
            seen < 3
        });
        assert_eq!(c.len(), 3);
        assert_eq!(light.raw_utf16_len(), len, "the whole text's length even after a stop");
    }
    // ── Issue #4: marks only the tokens of full occurrences ──────────────

    /// The marked words of a descriptor, in order.
    fn marked(text: &str, d: &SnippetDescriptor<'_>) -> Vec<String> {
        d.marks().map(|(s, l, _)| slice16(text, s, s + l)).collect()
    }

    /// The issue's line (Moshe and Aharon said shalom, and after that Moshe
    /// the priest went) and its query words, as escapes.
    const MOSHE: &str = "\u{05DE}\u{05E9}\u{05D4}";
    const AHARON: &str = "\u{05D5}\u{05D0}\u{05D4}\u{05E8}\u{05DF}";
    fn issue4_line() -> String {
        format!(
            "{MOSHE} {AHARON} \u{05D0}\u{05DE}\u{05E8}\u{05D5} \u{05E9}\u{05DC}\u{05D5}\u{05DD} \
             \u{05D5}\u{05D0}\u{05D7}\u{05E8} \u{05DB}\u{05DA} {MOSHE} \u{05DB}\u{05D4}\u{05DF} \u{05D4}\u{05DC}\u{05DA}"
        )
    }

    #[test]
    fn issue4_marks_only_the_full_occurrence() {
        let text = issue4_line();
        let p = PreparedQueryGroups::from_groups(&[vec![MOSHE], vec![AHARON]]);
        let terms: Vec<String> = vec![MOSHE.into(), AHARON.into()];
        let map: Vec<u32> = terms.iter().map(|t| p.get(t).map_or(u32::MAX, |x| x.ordinal as u32)).collect();
        let occ = postings_of(&text, &terms);
        let want = vec![MOSHE.to_string(), AHARON.to_string()];
        for ctx in [0, 8] {
            for positioned in [false, true] {
                let mut loc = SnippetWindowLocator::new(ctx);
                if positioned {
                    assert!(loc.tokenize_positioned(&text, &[0], &[&occ], &terms));
                } else {
                    loc.tokenize(&text);
                }
                let m: &[u32] = if positioned { &map } else { &[] };
                // Otzaria ordered gap 0
                let d = loc.locate_proximity_tokenized_mapped(&p, m, &Proximity::Ordered { gaps: vec![0] });
                assert!(d.is_match);
                assert_eq!(marked(&text, &d), want, "ordered ctx {ctx} positioned {positioned}");
                // any order, span 1
                let d = loc.locate_proximity_tokenized_mapped(&p, m, &Proximity::AnyOrder { max_span: 1 });
                assert_eq!(marked(&text, &d), want, "any order ctx {ctx} positioned {positioned}");
                // FtsLib distance 0
                let d = loc.locate_tokenized_within(&p, m, true, 0, 0);
                assert_eq!(marked(&text, &d), want, "ftslib ctx {ctx} positioned {positioned}");
                // FtsLib without a distance: every query word in the snippet (unchanged)
                let d = loc.locate_tokenized_within(&p, m, false, 0, usize::MAX);
                assert_eq!(d.marks().count(), if ctx == 0 { 2 } else { 3 });
                // in order: only the ordered occurrence (the last word has no
                // second word after it)
                let d = loc.locate_tokenized_within(&p, m, true, 0, usize::MAX);
                assert_eq!(marked(&text, &d), want, "in order ctx {ctx} positioned {positioned}");
            }
        }
    }

    #[test]
    fn issue4_repeated_phrase_marks_both_and_alternatives_do_not_mix() {
        let (a, b, c) = ("\u{05D0}\u{05D1}", "\u{05D2}\u{05D3}", "\u{05D4}\u{05D5}");
        let mut loc = SnippetWindowLocator::new(8);
        // two full occurrences: both marked
        let text = format!("{a} {b} x {a} {b} y {a}");
        let p = PreparedQueryGroups::from_groups(&[vec![a], vec![b]]);
        loc.tokenize(&text);
        for prox in [Proximity::Ordered { gaps: vec![0] }, Proximity::AnyOrder { max_span: 1 }] {
            let d = loc.locate_proximity_tokenized(&p, &prox);
            assert_eq!(marked(&text, &d), [a, b, a, b], "{prox:?}");
        }
        let d = loc.locate_tokenized_within(&p, &[], false, 0, 0);
        assert_eq!(marked(&text, &d), [a, b, a, b]);
        // a group's alternative (a or c) that completes no occurrence stays unmarked
        let text = format!("{a} {b} {c} x {c} {b}");
        let p = PreparedQueryGroups::from_groups(&[vec![a, c], vec![b]]);
        loc.tokenize(&text);
        let d = loc.locate_proximity_tokenized(&p, &Proximity::Ordered { gaps: vec![0] });
        assert_eq!(marked(&text, &d), [a, b, c, b]);
        let text = format!("{a} {b} {c} x y {b}");
        loc.tokenize(&text);
        let d = loc.locate_proximity_tokenized(&p, &Proximity::Ordered { gaps: vec![0] });
        assert_eq!(marked(&text, &d), [a, b]);
    }

    #[test]
    fn issue4_partial_keeps_every_query_word() {
        let (a, b) = ("\u{05D0}\u{05D1}", "\u{05D2}\u{05D3}");
        let mut loc = SnippetWindowLocator::new(8);
        loc.set_min_groups(1);
        let text = format!("{a} x y z {b}");
        let p = PreparedQueryGroups::from_groups(&[vec![a], vec![b]]);
        loc.tokenize(&text);
        let d = loc.locate_tokenized_within(&p, &[], false, 0, usize::MAX);
        assert_eq!(marked(&text, &d), [a, b]);
    }

    #[test]
    fn issue4_cross_line_marks_only_the_crossing_occurrence() {
        let (a, b) = ("\u{05D0}\u{05D1}", "\u{05D2}\u{05D3}");
        let p = PreparedQueryGroups::from_groups(&[vec![a], vec![b]]);
        let mut loc = SnippetWindowLocator::new(8);
        // ordered: a on the first line, b on the next
        let (l1, l2) = (format!("{b} q {a}"), format!("{b} z {a}"));
        let joined = format!("{l1}\n{l2}");
        loc.tokenize_pair(&l1, &l2);
        let d = loc.locate_proximity_tokenized(&p, &Proximity::Ordered { gaps: vec![5] });
        assert!(d.is_match);
        assert_eq!(d.marks().map(|m| m.0).collect::<Vec<_>>(), [5, 8]);
        assert_eq!(marked(&joined, &d), [a, b]);
        // distance / any order: only the pair at the break
        let (l1, l2) = (format!("{a} q q q {a}"), format!("{b} q q q {b}"));
        let joined = format!("{l1}\n{l2}");
        let at = l1.chars().count();
        loc.tokenize_pair(&l1, &l2);
        let d = loc.locate_proximity_tokenized(&p, &Proximity::AnyOrder { max_span: 1 });
        assert_eq!(d.marks().map(|m| m.0).collect::<Vec<_>>(), [at - 2, at + 1]);
        let d = loc.locate_tokenized_within(&p, &[], false, 0, 0);
        assert_eq!(d.marks().map(|m| m.0).collect::<Vec<_>>(), [at - 2, at + 1]);
        assert_eq!(marked(&joined, &d), [a, b]);
    }

    #[test]
    fn in_order_window_and_marks_follow_the_query_order() {
        // "B A B", query A B in order: the window and the marks are the
        // ordered "A B", not the equally tight (and earlier) "B A".
        let (a, b) = ("\u{05D0}\u{05D1}", "\u{05D2}\u{05D3}");
        let text = format!("{b} {a} {b}");
        let p = PreparedQueryGroups::from_groups(&[vec![a], vec![b]]);
        let terms: Vec<String> = vec![a.into(), b.into()];
        let map: Vec<u32> = terms.iter().map(|t| p.get(t).map_or(u32::MAX, |x| x.ordinal as u32)).collect();
        let occ = postings_of(&text, &terms);
        for ctx in [0, 8] {
            for positioned in [false, true] {
                let mut loc = SnippetWindowLocator::new(ctx);
                if positioned {
                    assert!(loc.tokenize_positioned(&text, &[0], &[&occ], &terms));
                } else {
                    loc.tokenize(&text);
                }
                let m: &[u32] = if positioned { &map } else { &[] };
                for dist in [usize::MAX, 0, 5] {
                    let d = loc.locate_tokenized_within(&p, m, true, 0, dist);
                    assert!(d.is_match);
                    assert_eq!(d.word_distance, 0);
                    assert_eq!(marked(&text, &d), [a, b], "ctx {ctx} positioned {positioned} dist {dist}");
                    assert_eq!(d.marks().next().unwrap().0, 3, "the window starts at the ordered a");
                }
                // order off: unchanged (the earlier "B A" window)
                let d = loc.locate_tokenized_within(&p, m, false, 0, usize::MAX);
                assert_eq!(d.marks().next().unwrap().0, 0);
                assert_eq!(d.marks().count(), if ctx == 0 { 2 } else { 3 });
            }
        }
        // reversed only: no ordered match
        let text = format!("{b} {a}");
        let mut loc = SnippetWindowLocator::new(8);
        assert!(!loc.locate(&text, &p, true, 0).is_match);
        assert!(loc.locate(&text, &p, false, 0).is_match);
    }

    #[test]
    fn in_order_equals_brute_force() {
        let words = ["\u{05D0}\u{05D1}", "\u{05D2}\u{05D3}", "\u{05D4}\u{05D5}", "\u{05D6}\u{05D7}"];
        let groups: Vec<Vec<&str>> = vec![vec![words[0]], vec![words[1], words[2]], vec![words[2]]];
        let k = groups.len();
        let p = PreparedQueryGroups::from_groups(&groups);
        let mut seed = 11u64;
        let mut loc = SnippetWindowLocator::new(2);
        let mut checked = 0;
        fn rec(g: usize, k: usize, from: usize, n: usize, a: &mut Vec<usize>, gs: &dyn Fn(usize) -> Vec<usize>, out: &mut Vec<Vec<usize>>) {
            if g == k {
                out.push(a.clone());
                return;
            }
            for t in from..n {
                if gs(t).contains(&g) {
                    a.push(t);
                    rec(g + 1, k, t + 1, n, a, gs, out);
                    a.pop();
                }
            }
        }
        for case in 0..4000usize {
            let n = 3 + (case % 10);
            let picks: Vec<usize> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    ((seed >> 33) % 4) as usize
                })
                .collect();
            let text = picks.iter().map(|&i| words[i]).collect::<Vec<_>>().join(" ");
            let starts: Vec<usize> = (0..n).map(|t| t * 3).collect();
            let gs = |t: usize| -> Vec<usize> { (0..k).filter(|&g| groups[g].contains(&words[picks[t]])).collect() };
            // every ordered chain (strictly increasing tokens)
            let mut chains: Vec<Vec<usize>> = Vec::new();
            rec(0, k, 0, n, &mut Vec::new(), &gs, &mut chains);
            let dist = [usize::MAX, 0, 1, 3][case % 4];
            loc.tokenize(&text);
            let ords: Vec<u32> = (0..n).map(|t| p.get(words[picks[t]]).map_or(NO_TERM, |x| x.ordinal as u32)).collect();
            assert_eq!(has_ordered_match(&ords, &p), !chains.is_empty());
            let d = loc.locate_tokenized_within(&p, &[], true, 0, dist);
            assert_eq!(d.is_match, !chains.is_empty(), "case {case}");
            if chains.is_empty() {
                continue;
            }
            checked += 1;
            // tightest chain (equal-width words: tokens), earliest end on a tie
            let best = chains.iter().map(|c| (c[k - 1] - c[0], c[k - 1], c[0])).min().unwrap();
            assert_eq!(d.word_distance, best.0 - (k - 1), "case {case} picks {picks:?}");
            let s_idx = starts.iter().rposition(|&x| x <= d.snap_start).unwrap();
            let e_idx = starts.iter().rposition(|&x| x < d.snap_end).unwrap();
            assert_eq!((s_idx, e_idx), (best.2.saturating_sub(2), (best.1 + 2).min(n - 1)), "case {case} picks {picks:?}");
            if d.word_distance > dist {
                continue; // the caller drops the hit
            }
            let mut want = vec![false; n];
            for c in &chains {
                if c[0] >= s_idx && c[k - 1] <= e_idx && c[k - 1] - c[0] <= dist.saturating_add(k - 1) {
                    for &t in c {
                        want[t] = true;
                    }
                }
            }
            let got: Vec<usize> = d.marks().map(|(s, _, _)| starts.iter().position(|&x| x == s).unwrap()).collect();
            let want: Vec<usize> = (0..n).filter(|&t| want[t]).collect();
            assert_eq!(got, want, "case {case} dist {dist} picks {picks:?}");
        }
        assert!(checked > 1000);
    }

    /// Brute force: a token is marked iff some assignment of the groups to
    /// distinct window tokens holding it satisfies the rule.
    #[test]
    fn issue4_marks_equal_brute_force() {
        let words = ["\u{05D0}\u{05D1}", "\u{05D2}\u{05D3}", "\u{05D4}\u{05D5}", "\u{05D6}\u{05D7}"];
        let groups: Vec<Vec<&str>> = vec![vec![words[0]], vec![words[1], words[2]], vec![words[2]]];
        let k = groups.len();
        let p = PreparedQueryGroups::from_groups(&groups);
        let mut seed = 7u64;
        let mut loc = SnippetWindowLocator::new(3);
        let mut checked = 0;
        for case in 0..3000usize {
            let n = 4 + (case % 9);
            let picks: Vec<usize> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    ((seed >> 33) % 4) as usize
                })
                .collect();
            let text = picks.iter().map(|&i| words[i]).collect::<Vec<_>>().join(" ");
            let starts: Vec<usize> = (0..n).map(|t| t * 3).collect(); // two letters + a space
            let gs = |t: usize| -> Vec<usize> { (0..k).filter(|&g| groups[g].contains(&words[picks[t]])).collect() };
            let rules = [
                Proximity::Ordered { gaps: vec![(case % 3) as u32, 1] },
                Proximity::AnyOrder { max_span: 2 + (case % 4) as u32 },
            ];
            loc.tokenize(&text);
            for prox in &rules {
                let d = loc.locate_proximity_tokenized(&p, prox);
                if !d.is_match {
                    continue;
                }
                checked += 1;
                let s_idx = starts.iter().rposition(|&x| x <= d.snap_start).unwrap();
                let e_idx = starts.iter().rposition(|&x| x < d.snap_end).unwrap();
                let mut want = vec![false; n];
                let mut assign = vec![0usize; k];
                #[allow(clippy::type_complexity)]
                fn rec(
                    g: usize,
                    k: usize,
                    a: &mut Vec<usize>,
                    cand: &dyn Fn(usize) -> Vec<usize>,
                    ok: &dyn Fn(&[usize]) -> bool,
                    want: &mut Vec<bool>,
                ) {
                    if g == k {
                        if ok(a) {
                            for &t in a.iter() {
                                want[t] = true;
                            }
                        }
                        return;
                    }
                    for t in cand(g) {
                        if a[..g].contains(&t) {
                            continue;
                        }
                        a[g] = t;
                        rec(g + 1, k, a, cand, ok, want);
                    }
                }
                let cand = |g: usize| (s_idx..=e_idx).filter(|&t| gs(t).contains(&g)).collect::<Vec<_>>();
                let ok = |a: &[usize]| match prox {
                    Proximity::Ordered { gaps } => {
                        (1..k).all(|g| a[g] > a[g - 1] && a[g] - a[g - 1] - 1 <= gaps[g - 1] as usize)
                    }
                    Proximity::AnyOrder { max_span } => {
                        a.iter().max().unwrap() - a.iter().min().unwrap() <= *max_span as usize
                    }
                };
                rec(0, k, &mut assign, &cand, &ok, &mut want);
                let got: Vec<usize> = d.marks().map(|(s, _, _)| starts.iter().position(|&x| x == s).unwrap()).collect();
                let want: Vec<usize> = (0..n).filter(|&t| want[t]).collect();
                assert_eq!(got, want, "case {case} {prox:?} picks {picks:?}");
            }
        }
        assert!(checked > 1000);
    }

}
