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
    group_count: Vec<usize>,
    mark_ranges: Vec<usize>,
    /// Groups a window must cover; 0 = all of them (FtsLib). See
    /// [`set_min_groups`](Self::set_min_groups).
    min_groups: usize,
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
            group_count: vec![0; 8],
            mark_ranges: Vec::with_capacity(24),
            min_groups: 0,
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

        let SnippetWindowLocator {
            context_words,
            text_tokens,
            group_count,
            mark_ranges,
            min_groups,
        } = self;
        text_tokens.tokenize(raw_html);
        let tokens = text_tokens.tokens();
        if tokens.is_empty() {
            return SnippetDescriptor::NO_MATCH;
        }
        let raw_len = text_tokens.raw_utf16_len();

        let all_groups = prepared.group_count();
        let partial = *min_groups > 0 && *min_groups < all_groups;
        let required = if partial { *min_groups } else { all_groups };
        let (i_left, i_right, score) = find_window(text_tokens, prepared, group_count, required);
        if score == usize::MAX {
            return SnippetDescriptor::NO_MATCH;
        }

        let (snap_start, snap_end, s_idx, e_idx) =
            expand_window(tokens, raw_len, i_left, i_right, *context_words);
        collect_mark_ranges(text_tokens, prepared, snap_start, snap_end, mark_ranges);

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
        let word_dist = (i_right - i_left).saturating_sub(denominator - 1);

        let is_match = partial
            || !(require_ordered && all_groups > 1 && !has_ordered_match(text_tokens, prepared));

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
    text_tokens: &TextTokens,
    prepared: &PreparedQueryGroups,
    group_count: &mut Vec<usize>,
    required: usize,
) -> (usize, usize, usize) {
    let tokens = text_tokens.tokens();
    let all_groups = prepared.group_count();
    if group_count.len() < all_groups {
        group_count.resize(all_groups * 2, 0);
    }
    group_count[..all_groups].fill(0);

    let mut covered = 0usize;
    let (mut best_i_left, mut best_i_right, mut best_score) = (usize::MAX, usize::MAX, usize::MAX);
    let mut l = 0usize;

    for r in 0..tokens.len() {
        if let Some(r_term) = prepared.get(text_tokens.norm_of(&tokens[r])) {
            for &rg in &r_term.groups {
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
            if let Some(l_term) = prepared.get(text_tokens.norm_of(&tokens[l])) {
                for &lg in &l_term.groups {
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

// ── Ordered-match validation ──────────────────────────────────────────

/// True when there exists a position where each query group is satisfied by
/// a token appearing strictly after the token satisfying the previous group.
/// Greedy forward scan.
fn has_ordered_match(text_tokens: &TextTokens, prepared: &PreparedQueryGroups) -> bool {
    let tokens = text_tokens.tokens();
    let num_groups = prepared.group_count();
    if num_groups <= 1 {
        return true;
    }
    let in_group = |tok: &TextToken, g: usize| {
        prepared
            .get(text_tokens.norm_of(tok))
            .is_some_and(|t| t.groups.contains(&g))
    };

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
    text_tokens: &TextTokens,
    prepared: &PreparedQueryGroups,
    snap_start: usize,
    snap_end: usize,
    mark_ranges: &mut Vec<usize>,
) {
    mark_ranges.clear();
    for tok in text_tokens.tokens() {
        if tok.utf16_end <= snap_start {
            continue;
        }
        if tok.utf16_start >= snap_end {
            break;
        }
        let Some(term) = prepared.get(text_tokens.norm_of(tok)) else {
            continue;
        };
        let tok_end = tok.utf16_end.min(snap_end);
        mark_ranges.push(tok.utf16_start);
        mark_ranges.push(tok_end - tok.utf16_start);
        mark_ranges.push(term.ordinal);
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
