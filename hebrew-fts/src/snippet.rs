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
//!      the matched ones.
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
/// occurrence; marks are every query-term token inside the expanded window.
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
        }
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
    }

    /// [`locate`](Self::locate) over the text last passed to
    /// [`tokenize`](Self::tokenize).
    pub fn locate_tokenized(
        &mut self,
        prepared: &PreparedQueryGroups,
        require_ordered: bool,
        original_group_count: usize,
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
            ..
        } = self;
        let tokens = text_tokens.tokens();
        if tokens.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }
        let raw_len = text_tokens.raw_utf16_len();

        ordinals.clear();
        ordinals.extend(tokens.iter().map(|t| {
            prepared.get(text_tokens.norm_of(t)).map_or(NO_TERM, |p| p.ordinal as u32)
        }));
        let ordinals = &ordinals[..];

        let all_groups = prepared.group_count();
        let partial = *min_groups > 0 && *min_groups < all_groups;
        let required = if partial { *min_groups } else { all_groups };
        let (i_left, i_right, score) = find_window(tokens, ordinals, prepared, group_count, required);
        if score == usize::MAX {
            return SnippetDescriptor::NO_MATCH;
        }

        let (snap_start, snap_end, s_idx, e_idx) =
            expand_window(tokens, raw_len, i_left, i_right, *context_words);
        collect_mark_ranges(tokens, ordinals, snap_start, snap_end, mark_ranges);

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
        // Words between the ends, by position (a paired reading's second
        // reading shares its first reading's position).
        let word_dist = (tokens[i_right].position - tokens[i_left].position).saturating_sub(denominator - 1);

        let is_match = partial
            || !(require_ordered && all_groups > 1 && !has_ordered_match(ordinals, prepared));

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
        if prepared.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }
        let SnippetWindowLocator {
            context_words,
            text_tokens,
            ordinals,
            mark_ranges,
            proximity: scratch,
            ..
        } = self;
        let tokens = text_tokens.tokens();
        if tokens.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }
        let raw_len = text_tokens.raw_utf16_len();
        ordinals.clear();
        ordinals.extend(tokens.iter().map(|t| {
            prepared.get(text_tokens.norm_of(t)).map_or(NO_TERM, |p| p.ordinal as u32)
        }));
        let ordinals = &ordinals[..];
        let k = prepared.group_count();
        let found = match proximity {
            Proximity::Ordered { gaps } => ordered_window(tokens, ordinals, prepared, gaps, scratch),
            Proximity::AnyOrder { max_span } => any_order_window(tokens, ordinals, prepared, *max_span as usize, scratch),
        };
        let Some((i_left, i_right, score)) = found else {
            return SnippetDescriptor::NO_MATCH;
        };
        let (snap_start, snap_end, s_idx, e_idx) =
            expand_window(tokens, raw_len, i_left, i_right, *context_words);
        collect_mark_ranges(tokens, ordinals, snap_start, snap_end, mark_ranges);
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

// ── Window finding ────────────────────────────────────────────────────
// Returns (i_left, i_right, score): token indices, not raw positions.

/// `required`: distinct groups a window must cover (the group count, or
/// fewer for partial matching).
fn find_window(
    tokens: &[TextToken],
    ordinals: &[u32],
    prepared: &PreparedQueryGroups,
    group_count: &mut Vec<usize>,
    required: usize,
) -> (usize, usize, usize) {
    let all_groups = prepared.group_count();
    if group_count.len() < all_groups {
        group_count.resize(all_groups * 2, 0);
    }
    group_count[..all_groups].fill(0);

    let mut covered = 0usize;
    let (mut best_i_left, mut best_i_right, mut best_score) = (usize::MAX, usize::MAX, usize::MAX);
    let mut l = 0usize;

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
            let span = tokens[r].utf16_end - tokens[l].utf16_start;
            if span < best_score {
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
    }

    (best_i_left, best_i_right, best_score)
}

// ── Proximity windows ─────────────────────────────────────────────────

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
    scratch: &mut ProximityScratch,
) -> Option<(usize, usize, usize)> {
    let k = prepared.group_count();
    let ProximityScratch { chain, next, deque, .. } = scratch;
    chain.clear();
    chain.extend((0..tokens.len()).filter(|&t| in_group(ordinals, prepared, t, 0)).map(|t| (t, t)));
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
        let span = tokens[end].utf16_end - tokens[start].utf16_start;
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
    scratch: &mut ProximityScratch,
) -> Option<(usize, usize, usize)> {
    let k = prepared.group_count();
    scratch.reset_any_order(tokens.len(), k);
    let groups_of = |t: usize| -> &[usize] {
        if ordinals[t] == NO_TERM {
            &[]
        } else {
            prepared.groups_at(ordinals[t] as usize)
        }
    };
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
        let span = tokens[r].utf16_end - tokens[l].utf16_start;
        if best.is_none_or(|b| span < b.2) {
            best = Some((l, r, span));
        }
    }
    best
}

const FREE: usize = usize::MAX;

// ── Ordered-match validation ──────────────────────────────────────────

/// True when there exists a position where each query group is satisfied by
/// a token appearing strictly after the token satisfying the previous group.
/// Greedy forward scan.
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
) {
    mark_ranges.clear();
    for (tok, &ordinal) in tokens.iter().zip(ordinals) {
        if tok.utf16_end <= snap_start {
            continue;
        }
        if tok.utf16_start >= snap_end {
            break;
        }
        if ordinal == NO_TERM {
            continue;
        }
        let tok_end = tok.utf16_end.min(snap_end);
        mark_ranges.push(tok.utf16_start);
        mark_ranges.push(tok_end - tok.utf16_start);
        mark_ranges.push(ordinal as usize);
    }
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
}
