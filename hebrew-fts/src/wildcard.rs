//! Port of FtsLib `HebrewWildcardExpander.cs` as an FST walk.
//!
//! - `*` matches zero or more characters (prefix / suffix / infix).
//! - `?` makes the immediately preceding character optional
//!   (שלו?ם → שלום, שלם). A `?` with no preceding letter (position 0, or
//!   after another `?` or `*`) is dropped; a run of `?`s is one toggle.
//!
//! Limits, exactly as FtsLib:
//! - MIN_ANCHOR_LENGTH (2): the non-wildcard part must be ≥ 2 chars, else the
//!   pattern is REJECTED (the caller skips that AND slot).
//! - MAX_OPTIONAL_CHARS (4): more effective `?`s → rejected.
//! - Affix budget, total chars all `*`s may match: leading `*` only → 3
//!   (max Hebrew prefix stack); leading AND trailing → 3 + 4 = 7; otherwise
//!   (trailing or internal) → 4 (max pronominal suffix).
//! - The 2000-term cap (shortest first, then most frequent) is applied by
//!   the AutomatonWeight, one cap across all segments.
//!
//! Where FtsLib ran LIKE / range / FM-index scans and filtered afterwards,
//! here one automaton walks the term dictionary and never leaves the budget.

use std::sync::Arc;

use tantivy::query::{EnableScoring, MatchedTermsSink, Query, SpanTermSource, Weight};
use tantivy::schema::Field;
use tantivy_fst::Automaton;

use crate::walk::DEFAULT_MAX_EXPANDED_TERMS;

/// Minimum number of non-wildcard characters a pattern must contain.
pub const MIN_ANCHOR_LENGTH: usize = 2;
/// Maximum chars a leading `*` may match (Hebrew prefixes stack to 3).
pub const MAX_PREFIX_WILDCARD_CHARS: usize = 3;
/// Maximum chars a trailing `*` may match (pronominal suffixes reach 4).
pub const MAX_SUFFIX_WILDCARD_CHARS: usize = 4;
/// Maximum number of effective `?` operators in one pattern.
pub const MAX_OPTIONAL_CHARS: usize = 4;

/// Why a pattern is not searchable. The caller skips the AND slot (FtsLib's
/// `rejected`), unlike a supported pattern that matches nothing, which makes
/// the whole query empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WildcardRejected {
    /// Fewer than MIN_ANCHOR_LENGTH non-wildcard chars.
    AnchorTooShort,
    /// More than MAX_OPTIONAL_CHARS effective `?`s.
    TooManyOptionals,
}

#[derive(Debug, Clone)]
enum Token {
    Literal(Vec<u8>),
    Star,
}

/// A parsed, supported wildcard pattern.
#[derive(Debug, Clone)]
pub struct WildcardPattern {
    /// One token list per `?` variant whose anchor is long enough.
    variants: Vec<Vec<Token>>,
    /// Total chars all `*`s together may match.
    budget: u32,
    /// The canonical quote `"` is transparent everywhere (התעלם מגרשיים):
    /// `*לום` then matches the indexed `שלו"ם`, as FtsLib's quote-free terms did.
    ignore_quotes: bool,
    /// Vocalized matching (`match_nikud` / `match_teamim` on an index that
    /// keeps marks): the pattern carries the typed marks, required on their
    /// letters; every other mark of a term is free (zero width), and a char
    /// a `*` matches may carry any marks or none. False: the pattern has no
    /// mark (on a marks-keeping index the walk reads it through
    /// [`MarkFree`](crate::marks::MarkFree)).
    free_marks: bool,
}

fn anchor_length(pattern: &str) -> usize {
    // a typed (required) mark is no character of its own
    pattern.chars().filter(|&c| c != '*' && c != '?' && !crate::chars::is_term_mark(c)).count()
}

/// `?`s with a real preceding letter: the ones that branch.
fn count_effective_optionals(chars: &[char]) -> usize {
    (0..chars.len())
        .filter(|&i| chars[i] == '?' && i > 0 && chars[i - 1] != '*' && chars[i - 1] != '?')
        .count()
}

/// All sub-patterns from including/excluding each optional char.
fn expand_optionals(chars: &[char], pos: usize, current: &mut Vec<char>, out: &mut Vec<String>) {
    if pos == chars.len() {
        let s: String = current.iter().collect();
        if !out.contains(&s) {
            out.push(s);
        }
        return;
    }
    let c = chars[pos];
    if c != '?' {
        current.push(c);
        expand_optionals(chars, pos + 1, current, out);
        current.pop();
        return;
    }
    let has_target = pos > 0 && chars[pos - 1] != '?' && chars[pos - 1] != '*' && !current.is_empty();
    if !has_target {
        expand_optionals(chars, pos + 1, current, out);
        return;
    }
    // include the optional char
    expand_optionals(chars, pos + 1, current, out);
    // exclude it, with the marks typed on it (vocalized patterns)
    let mut saved = Vec::new();
    while let Some(c) = current.pop() {
        saved.push(c);
        if !crate::chars::is_term_mark(c) {
            break;
        }
    }
    expand_optionals(chars, pos + 1, current, out);
    current.extend(saved.into_iter().rev());
}

fn tokenize(pattern: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut literal = String::new();
    for c in pattern.chars() {
        if c == '*' {
            if !literal.is_empty() {
                tokens.push(Token::Literal(std::mem::take(&mut literal).into_bytes()));
            }
            if !matches!(tokens.last(), Some(Token::Star)) {
                tokens.push(Token::Star);
            }
        } else {
            literal.push(c);
        }
    }
    if !literal.is_empty() {
        tokens.push(Token::Literal(literal.into_bytes()));
    }
    tokens
}

impl WildcardPattern {
    /// Parses `pattern` (already normalized like an index term).
    pub fn parse(pattern: &str) -> Result<WildcardPattern, WildcardRejected> {
        let leading = pattern.starts_with('*');
        let trailing = pattern.ends_with('*');
        let budget = if leading && trailing {
            MAX_PREFIX_WILDCARD_CHARS + MAX_SUFFIX_WILDCARD_CHARS
        } else if leading {
            MAX_PREFIX_WILDCARD_CHARS
        } else {
            MAX_SUFFIX_WILDCARD_CHARS
        } as u32;

        let chars: Vec<char> = pattern.chars().collect();
        let sub_patterns = if pattern.contains('?') {
            if count_effective_optionals(&chars) > MAX_OPTIONAL_CHARS {
                return Err(WildcardRejected::TooManyOptionals);
            }
            let mut out = Vec::new();
            expand_optionals(&chars, 0, &mut Vec::with_capacity(chars.len()), &mut out);
            out
        } else {
            vec![pattern.to_string()]
        };

        // A variant under the anchor floor is individually unsupported.
        let variants: Vec<Vec<Token>> = sub_patterns
            .iter()
            .filter(|s| anchor_length(s) >= MIN_ANCHOR_LENGTH)
            .map(|s| tokenize(s))
            .collect();
        if variants.is_empty() {
            return Err(WildcardRejected::AnchorTooShort);
        }
        Ok(WildcardPattern {
            variants,
            budget,
            ignore_quotes: false,
            free_marks: pattern.chars().any(crate::chars::is_term_mark),
        })
    }

    /// True when the pattern carries typed marks it requires (vocalized
    /// matching, see [`crate::walk::required_marks_form`]): its automaton
    /// then reads a term's marks itself, so it runs on the marked term.
    pub fn requires_marks(&self) -> bool {
        self.free_marks
    }

    /// True when the quote is transparent.
    pub fn ignore_quotes(&self) -> bool {
        self.ignore_quotes
    }

    /// Parses `pattern` with the quote transparent: quotes in the pattern are
    /// dropped and quotes in indexed terms are skipped.
    pub fn parse_ignoring_quotes(pattern: &str) -> Result<WildcardPattern, WildcardRejected> {
        let stripped: String = pattern.chars().filter(|&c| !crate::chars::is_term_quote(c)).collect();
        let mut parsed = WildcardPattern::parse(&stripped)?;
        parsed.ignore_quotes = true;
        Ok(parsed)
    }
}

// ── The automaton ───────────────────────────────────────────────────────
//
// NFA config packed in a u64: held lead byte(8) | variant(8) | token(16) |
// literal offset(16) | star chars used(8) | pending continuation bytes(8).
// The held byte (vocalized patterns only) is the lead byte of a two-byte
// char read but not resolved yet: with `pending` = [`HOLD_MARK`] a possible
// free mark, otherwise the char a star is consuming (a mark is not one).

/// `pending` value of a config holding the lead byte of a possible free mark.
const HOLD_MARK: u32 = 0xFE;
const HELD_SHIFT: u32 = 56;

#[inline]
fn pack(variant: usize, token: usize, offset: usize, used: u32, pending: u32) -> u64 {
    ((variant as u64) << 48)
        | ((token as u64) << 32)
        | ((offset as u64) << 16)
        | ((used as u64) << 8)
        | pending as u64
}
#[inline]
fn unpack(c: u64) -> (usize, usize, usize, u32, u32) {
    (
        ((c >> 48) & 0xFF) as usize,
        ((c >> 32) & 0xFFFF) as usize,
        ((c >> 16) & 0xFFFF) as usize,
        ((c >> 8) & 0xFF) as u32,
        (c & 0xFF) as u32,
    )
}
#[inline]
fn held(c: u64) -> u8 {
    (c >> HELD_SHIFT) as u8
}
#[inline]
fn with_held(c: u64, lead: u8) -> u64 {
    (c & !(0xFF << HELD_SHIFT)) | (u64::from(lead) << HELD_SHIFT)
}

/// UTF-8 sequence length from a lead byte (1 for ASCII).
#[inline]
fn utf8_len(lead: u8) -> u32 {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Byte automaton for a [`WildcardPattern`].
#[derive(Debug, Clone)]
pub struct WildcardAutomaton {
    pattern: WildcardPattern,
}

impl WildcardAutomaton {
    /// Wraps a parsed pattern.
    pub fn new(pattern: WildcardPattern) -> WildcardAutomaton {
        WildcardAutomaton { pattern }
    }

    /// True when the pattern requires typed marks
    /// ([`WildcardPattern::requires_marks`]): it reads a term's marks itself.
    pub fn requires_marks(&self) -> bool {
        self.pattern.free_marks
    }

    /// True when the whole `term` matches the pattern (budgets included).
    pub fn accepts(&self, term: &[u8]) -> bool {
        let mut state = self.start();
        for &b in term {
            if !self.can_match(&state) {
                return false;
            }
            state = self.accept(&state, b);
        }
        self.is_match(&state)
    }

    fn close_into(&self, config: u64, set: &mut Vec<u64>) {
        if set.contains(&config) {
            return;
        }
        set.push(config);
        let (v, t, _, used, pending) = unpack(config);
        let tokens = &self.pattern.variants[v];
        // A star with no char in progress may also match nothing more.
        if pending == 0 && t < tokens.len() && matches!(tokens[t], Token::Star) {
            self.close_into(pack(v, t + 1, 0, used, 0), set);
        }
    }

    fn step(&self, config: u64, byte: u8, out: &mut Vec<u64>) {
        let (v, t, off, used, pending) = unpack(config);
        let tokens = &self.pattern.variants[v];
        if pending == HOLD_MARK {
            // a free mark: skipped, back where it began
            if crate::marks::is_mark_char(held(config), byte) {
                self.close_into(pack(v, t, off, used, 0), out);
            }
            return;
        }
        if pending > 0 {
            // continuation byte of a char a star is consuming (a mark is
            // zero width, never a star's char: the free-mark path reads it)
            if byte & 0xC0 == 0x80 && !crate::marks::is_mark_char(held(config), byte) {
                self.close_into(pack(v, t, 0, used, pending - 1), out);
            }
            return;
        }
        // Vocalized: any mark of the term may be free (a typed one is also
        // read by its literal below, as required).
        if self.pattern.free_marks && crate::marks::is_mark_lead(byte) {
            let hold = with_held(pack(v, t, off, used, HOLD_MARK), byte);
            if !out.contains(&hold) {
                out.push(hold);
            }
        }
        let Some(token) = tokens.get(t) else {
            // A quote after the whole pattern (a trailing geresh is part of
            // its word) is zero-width when quotes are transparent.
            if crate::chars::is_term_quote_byte(byte) && self.pattern.ignore_quotes {
                self.close_into(config, out);
            }
            return;
        };
        match token {
            Token::Literal(lit) => {
                if lit[off] == byte {
                    if off + 1 == lit.len() {
                        self.close_into(pack(v, t + 1, 0, used, 0), out);
                    } else {
                        self.close_into(pack(v, t, off + 1, used, 0), out);
                    }
                } else if crate::chars::is_term_quote_byte(byte) && self.pattern.ignore_quotes {
                    self.close_into(config, out); // zero-width quote inside the literal
                }
            }
            Token::Star => {
                if byte & 0xC0 == 0x80 {
                    return; // stray continuation byte
                }
                // The canonical quote is not a letter: FtsLib's terms carried none.
                if crate::chars::is_term_quote_byte(byte) {
                    self.close_into(config, out);
                    return;
                }
                if used < self.pattern.budget {
                    let len = utf8_len(byte);
                    let next = pack(v, t, 0, used + 1, len - 1);
                    let next = if self.pattern.free_marks && len == 2 {
                        with_held(next, byte)
                    } else {
                        next
                    };
                    self.close_into(next, out);
                }
            }
        }
    }
}

impl Automaton for WildcardAutomaton {
    type State = Vec<u64>;

    fn start(&self) -> Vec<u64> {
        let mut set = Vec::new();
        for v in 0..self.pattern.variants.len() {
            self.close_into(pack(v, 0, 0, 0, 0), &mut set);
        }
        set.sort_unstable();
        set
    }

    fn is_match(&self, state: &Vec<u64>) -> bool {
        state.iter().any(|&c| {
            let (v, t, _, _, pending) = unpack(c);
            pending == 0 && t == self.pattern.variants[v].len()
        })
    }

    fn can_match(&self, state: &Vec<u64>) -> bool {
        !state.is_empty()
    }

    fn accept(&self, state: &Vec<u64>, byte: u8) -> Vec<u64> {
        let mut out = Vec::with_capacity(state.len());
        for &c in state {
            self.step(c, byte, &mut out);
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Tantivy query for one wildcard pattern, capped at
/// [`DEFAULT_MAX_EXPANDED_TERMS`] across all segments (shortest first).
#[derive(Debug, Clone)]
pub struct HebrewWildcardQuery {
    field: Field,
    pattern: WildcardPattern,
    matched_terms_sink: Option<MatchedTermsSink>,
    max_expansions: usize,
    /// The index keeps marks in its terms: they are read as zero width.
    marked_terms: bool,
}

impl HebrewWildcardQuery {
    /// Parses `pattern`; a rejected pattern means "skip this AND slot".
    pub fn new(field: Field, pattern: &str) -> Result<HebrewWildcardQuery, WildcardRejected> {
        Ok(HebrewWildcardQuery {
            field,
            pattern: WildcardPattern::parse(pattern)?,
            matched_terms_sink: None,
            max_expansions: DEFAULT_MAX_EXPANDED_TERMS,
            marked_terms: false,
        })
    }

    /// As [`new`](Self::new), with the quote transparent (התעלם מגרשיים).
    pub fn new_ignoring_quotes(field: Field, pattern: &str) -> Result<HebrewWildcardQuery, WildcardRejected> {
        Ok(HebrewWildcardQuery {
            field,
            pattern: WildcardPattern::parse_ignoring_quotes(pattern)?,
            matched_terms_sink: None,
            max_expansions: DEFAULT_MAX_EXPANDED_TERMS,
            marked_terms: false,
        })
    }

    /// Collects the matched forms into `sink` during the search.
    pub fn set_matched_terms_sink(&mut self, sink: MatchedTermsSink) {
        self.matched_terms_sink = Some(sink);
    }

    /// Overrides the expansion cap.
    pub fn set_max_expansions(&mut self, cap: usize) {
        self.max_expansions = cap;
    }

    /// The expansion cap.
    pub fn max_expansions(&self) -> usize {
        self.max_expansions
    }

    /// The parsed pattern.
    pub fn pattern(&self) -> &WildcardPattern {
        &self.pattern
    }

    /// The automaton the walk runs (for verifying candidates found another way).
    pub fn automaton(&self) -> WildcardAutomaton {
        WildcardAutomaton::new(self.pattern.clone())
    }

    /// The index keeps marks in its terms (`keep_marks`): the pattern then
    /// reads them as zero width ([`crate::marks::MarkFree`]), so it matches
    /// what it matches on the mark-stripped index. A pattern with typed
    /// (required) marks reads them itself ([`WildcardPattern::requires_marks`]).
    pub fn set_marked_terms(&mut self, marked_terms: bool) {
        self.marked_terms = marked_terms;
    }

    fn plan<'s>(&'s self, segments: Option<&'s [tantivy::SegmentReader]>) -> crate::marks::WeightPlan<'s> {
        crate::marks::WeightPlan {
            field: self.field,
            sink: self.matched_terms_sink.as_ref(),
            cap: Some(self.max_expansions),
            segments,
            pinned: Vec::new(),
            marked_terms: self.marked_terms,
            // a vocalized pattern reads the marks itself
            mark_free: self.marked_terms && !self.pattern.free_marks,
            rank: None,
        }
    }

    /// This pattern as a span slot, capped across all segments.
    pub fn span_slot(&self, searcher: &tantivy::Searcher) -> tantivy::Result<Arc<dyn SpanTermSource>> {
        Ok(self.plan(Some(searcher.segment_readers())).span(self.automaton()))
    }
}

impl Query for HebrewWildcardQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(self.plan(enable_scoring.searcher().map(|s| s.segment_readers())).boxed(self.automaton()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, term: &str) -> bool {
        let a = WildcardAutomaton::new(WildcardPattern::parse(pattern).unwrap());
        let mut s = a.start();
        for &b in term.as_bytes() {
            if !a.can_match(&s) {
                return false;
            }
            s = a.accept(&s, b);
        }
        a.is_match(&s)
    }

    #[test]
    fn prefix_wildcard_budget_4() {
        assert!(matches("שלו*", "שלו"));
        assert!(matches("שלו*", "שלום"));
        assert!(matches("שלו*", "שלומכם")); // +3
        assert!(!matches("שלו*", "שלומיהםא")); // +5
    }

    #[test]
    fn budgets_exact() {
        assert!(matches("אב*", "אבגדהו")); // +4 trailing ok
        assert!(!matches("אב*", "אבגדהוז")); // +5 trailing too many
        assert!(matches("*אב", "גדהאב")); // +3 leading ok
        assert!(!matches("*אב", "גדהואב")); // +4 leading too many
        assert!(matches("*אב*", "גדהאבוזחט")); // 3 + 4 = 7
        assert!(!matches("*אב*", "גדהאבוזחטי")); // 8
        assert!(matches("*אב*", "אבגדהוזחט")); // all 7 after: total budget
        assert!(matches("א*ב", "אגדב")); // internal star, budget 4
    }

    #[test]
    fn literal_must_match() {
        assert!(!matches("שלו*", "של"));
        assert!(!matches("*לום", "שלוםם"));
        assert!(matches("*לום", "שלום"));
    }

    #[test]
    fn optional_chars() {
        assert!(matches("שלו?ם", "שלום"));
        assert!(matches("שלו?ם", "שלם"));
        assert!(!matches("שלו?ם", "שלוום"));
        // a run of ?s is one toggle, not two
        assert!(!matches("אבגד??", "אב"));
        assert!(matches("אבגד??", "אבג"));
    }

    #[test]
    fn rejections() {
        assert_eq!(WildcardPattern::parse("*ל").unwrap_err(), WildcardRejected::AnchorTooShort);
        assert_eq!(WildcardPattern::parse("מ*").unwrap_err(), WildcardRejected::AnchorTooShort);
        assert_eq!(
            WildcardPattern::parse("א?ב?ג?ד?ה?").unwrap_err(),
            WildcardRejected::TooManyOptionals
        );
        // ? at position 0 and after * are dropped (no-ops)
        assert!(WildcardPattern::parse("?שלום").is_ok());
        assert!(WildcardPattern::parse("של*?ום").is_ok());
    }

    fn matches_ignoring_quotes(pattern: &str, term: &str) -> bool {
        let a = WildcardAutomaton::new(WildcardPattern::parse_ignoring_quotes(pattern).unwrap());
        let mut s = a.start();
        for &b in term.as_bytes() {
            if !a.can_match(&s) {
                return false;
            }
            s = a.accept(&s, b);
        }
        a.is_match(&s)
    }

    #[test]
    fn quote_transparent_inside_literals() {
        assert!(!matches("*לום", "שלו\"ם")); // off: the quote breaks the literal
        assert!(matches_ignoring_quotes("*לום", "שלו\"ם"));
        assert!(matches_ignoring_quotes("*לום", "שלום"));
        assert!(matches_ignoring_quotes("רמב\"*", "רמבם")); // quote in the pattern dropped
        assert!(!matches_ignoring_quotes("*לום", "שלוםם"));
    }

    #[test]
    fn trailing_geresh_is_zero_width_when_quotes_are_ignored() {
        // a trailing geresh is part of its word's term (Otzaria)
        assert!(matches_ignoring_quotes("\u{05E9}\u{05DC}\u{05D5}?\u{05DD}", "\u{05E9}\u{05DC}\u{05DD}'"));
        assert!(matches_ignoring_quotes("*\u{05DC}\u{05D5}\u{05DD}", "\u{05E9}\u{05DC}\u{05D5}\u{05DD}'"));
        assert!(!matches_ignoring_quotes("\u{05E9}\u{05DC}\u{05D5}?\u{05DD}", "\u{05E9}\u{05DC}\u{05DD}'\u{05D0}"));
    }

    /// A vocalized pattern (typed marks left, see
    /// [`crate::walk::required_marks_form`]) over marked terms.
    fn matches_marked(pattern: &str, term: &str) -> bool {
        let p = WildcardPattern::parse(&crate::chars::canonical_marks(pattern)).unwrap();
        assert!(p.requires_marks());
        WildcardAutomaton::new(p).accepts(crate::chars::canonical_marks(term).as_bytes())
    }

    #[test]
    fn typed_marks_are_required_on_typed_letters_only() {
        let (a, b, g, d, he) = ('\u{05D0}', '\u{05D1}', '\u{05D2}', '\u{05D3}', '\u{05D4}');
        let (qamatz, patah, dagesh, etnahta) = ('\u{05B8}', '\u{05B7}', '\u{05BC}', '\u{0591}');
        let w = |cs: &[char]| cs.iter().collect::<String>();
        let pat = format!("{}*", w(&[a, b, qamatz, g]));
        assert!(matches_marked(&pat, &w(&[a, b, qamatz, g])));
        // other marks are free, on typed letters and on star letters
        assert!(matches_marked(&pat, &w(&[a, patah, b, dagesh, qamatz, etnahta, g, qamatz, d, patah])));
        assert!(!matches_marked(&pat, &w(&[a, b, g])));
        assert!(!matches_marked(&pat, &w(&[a, b, patah, g])));
        // a star letter's marks do not count against the budget (4 letters)
        assert!(matches_marked(&pat, &w(&[a, b, qamatz, g, d, qamatz, he, patah, d, qamatz, he, etnahta])));
        assert!(!matches_marked(&pat, &w(&[a, b, qamatz, g, d, qamatz, he, d, he, d])));
        // leading star
        let pat = format!("*{}", w(&[b, qamatz, g, d]));
        assert!(matches_marked(&pat, &w(&[he, patah, b, qamatz, g, d])));
        assert!(!matches_marked(&pat, &w(&[he, b, g, d])));
        // `?` drops its letter together with the letter's typed marks
        let pat = w(&[he, qamatz, '?', b, g]);
        assert!(matches_marked(&pat, &w(&[b, g])));
        assert!(matches_marked(&pat, &w(&[he, qamatz, b, patah, g])));
        assert!(!matches_marked(&pat, &w(&[he, b, g])));
        // te'amim alike: a typed taam is required, nikud free
        let pat = format!("{}*", w(&[a, etnahta, b]));
        assert!(matches_marked(&pat, &w(&[a, qamatz, etnahta, b, d])));
        assert!(!matches_marked(&pat, &w(&[a, qamatz, b, d])));
        // the anchor counts letters, not marks
        assert_eq!(
            WildcardPattern::parse(&format!("*{}", w(&[a, qamatz]))).unwrap_err(),
            WildcardRejected::AnchorTooShort
        );
    }

    #[test]
    fn quotes_do_not_count_against_budget() {
        // רש"י via רש* : 2 letters + quote, budget counts letters only
        assert!(matches("רש*", "רש\"י"));
    }
}
