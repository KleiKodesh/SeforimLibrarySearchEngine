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
//!   the AutomatonWeight over the whole index.
//!
//! Where FtsLib ran LIKE / range / FM-index scans and filtered afterwards,
//! here one automaton walks the term dictionary and never leaves the budget.

use std::sync::Arc;

use tantivy::query::{AutomatonWeight, EnableScoring, MatchedTermsSink, Query, SpanTermSource, Weight};
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
}

fn anchor_length(pattern: &str) -> usize {
    pattern.chars().filter(|&c| c != '*' && c != '?').count()
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
    // exclude it
    let saved = current.pop().unwrap();
    expand_optionals(chars, pos + 1, current, out);
    current.push(saved);
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
        Ok(WildcardPattern { variants, budget })
    }
}

// ── The automaton ───────────────────────────────────────────────────────
//
// NFA config packed in a u64: variant(8) | token(16) | literal offset(16) |
// star chars used(8) | pending continuation bytes(8).

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
        (c >> 48) as usize,
        ((c >> 32) & 0xFFFF) as usize,
        ((c >> 16) & 0xFFFF) as usize,
        ((c >> 8) & 0xFF) as u32,
        (c & 0xFF) as u32,
    )
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
        if pending > 0 {
            // continuation byte of a char a star is consuming
            if byte & 0xC0 == 0x80 {
                self.close_into(pack(v, t, 0, used, pending - 1), out);
            }
            return;
        }
        let Some(token) = tokens.get(t) else { return };
        match token {
            Token::Literal(lit) => {
                if lit[off] == byte {
                    if off + 1 == lit.len() {
                        self.close_into(pack(v, t + 1, 0, used, 0), out);
                    } else {
                        self.close_into(pack(v, t, off + 1, used, 0), out);
                    }
                }
            }
            Token::Star => {
                if byte & 0xC0 == 0x80 {
                    return; // stray continuation byte
                }
                // The canonical quote is not a letter: FtsLib's terms carried none.
                if byte == b'"' {
                    self.close_into(config, out);
                    return;
                }
                if used < self.pattern.budget {
                    let len = utf8_len(byte);
                    self.close_into(pack(v, t, 0, used + 1, len - 1), out);
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
/// [`DEFAULT_MAX_EXPANDED_TERMS`] over the whole index (shortest first).
#[derive(Debug, Clone)]
pub struct HebrewWildcardQuery {
    field: Field,
    pattern: WildcardPattern,
    matched_terms_sink: Option<MatchedTermsSink>,
    max_expansions: usize,
}

impl HebrewWildcardQuery {
    /// Parses `pattern`; a rejected pattern means "skip this AND slot".
    pub fn new(field: Field, pattern: &str) -> Result<HebrewWildcardQuery, WildcardRejected> {
        Ok(HebrewWildcardQuery {
            field,
            pattern: WildcardPattern::parse(pattern)?,
            matched_terms_sink: None,
            max_expansions: DEFAULT_MAX_EXPANDED_TERMS,
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

    fn automaton_weight(&self) -> AutomatonWeight<WildcardAutomaton> {
        let mut weight =
            AutomatonWeight::new(self.field, WildcardAutomaton::new(self.pattern.clone()));
        if let Some(sink) = &self.matched_terms_sink {
            weight.set_matched_terms_sink(sink.clone());
        }
        weight.set_max_expansions(self.max_expansions);
        weight
    }

    /// This pattern as a span slot, capped over `searcher`'s whole index.
    pub fn span_slot(&self, searcher: &tantivy::Searcher) -> tantivy::Result<Arc<dyn SpanTermSource>> {
        let mut weight = self.automaton_weight();
        weight.apply_global_cap(searcher)?;
        Ok(Arc::new(weight))
    }
}

impl Query for HebrewWildcardQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let mut weight = self.automaton_weight();
        if let Some(searcher) = enable_scoring.searcher() {
            weight.apply_global_cap(searcher)?;
        }
        Ok(Box::new(weight))
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

    #[test]
    fn quotes_do_not_count_against_budget() {
        // רש"י via רש* : 2 letters + quote, budget counts letters only
        assert!(matches("רש*", "רש\"י"));
    }
}
