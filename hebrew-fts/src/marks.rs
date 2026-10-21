//! Marks (nikud, te'amim, combining accents) as zero-width characters of a
//! term automaton: the walks over an index that keeps marks.
//!
//! An index built with `keep_marks` stores every term with its marks, in
//! canonical order ([`crate::chars::push_mark_canonical`]). A search that
//! does not match marks must return exactly what the mark-stripped index
//! returns, so every walk reads the marks of a term as zero-width: it walks
//! the term dictionary as if each term were its mark-free form. That is
//! [`MarkFree`]: it wraps ANY term automaton (word walk, wildcard, `%`,
//! fuzzy DFA, user regex) built on mark-free text and skips the mark
//! characters of the term it reads, as walk states (the wrapper's state is
//! the inner state plus a held lead byte). The expansion cap counts the
//! mark-free forms too: [`zero_width_chars`] gives tantivy the marks as
//! zero-width characters, so a vocalized term is as short as its letters and
//! all the vocalized spellings of one word share one slot.
//!
//! A search that matches marks (Otzaria's `match_nikud` / `match_teamim`)
//! is the word walk itself with the typed marks required on their letters
//! and every other mark free ([`crate::walk`]).

use tantivy::termdict::ZeroWidthChars;
use tantivy_fst::Automaton;

use crate::chars::{is_term_mark, TERM_MARK_RANGES};

/// The quote bytes the tokenizer keeps inside terms (`'` and `"`): zero
/// width on every index (\u{05E8}\u{05E9}"\u{05D9} is as short as
/// \u{05E8}\u{05E9}\u{05D9}, as in FtsLib whose terms carried no quotes).
pub const ZERO_WIDTH_QUOTES: ZeroWidthChars = ZeroWidthChars::new(b"\"'");

/// The quotes and the term marks ([`TERM_MARK_RANGES`]): the zero-width
/// characters of an index that keeps marks.
pub const ZERO_WIDTH_QUOTES_AND_MARKS: ZeroWidthChars = ZERO_WIDTH_QUOTES.with_ranges(&TERM_MARK_RANGES);

/// The zero-width characters of a dictionary: quotes, plus the marks when
/// its terms keep them.
pub fn zero_width_chars(marked_terms: bool) -> ZeroWidthChars {
    if marked_terms {
        ZERO_WIDTH_QUOTES_AND_MARKS
    } else {
        ZERO_WIDTH_QUOTES
    }
}

/// How a term automaton is turned into a tantivy weight: its field, sink,
/// cap, and whether the dictionary's terms keep marks.
#[derive(Clone, Copy)]
pub(crate) struct WeightPlan<'a> {
    pub field: tantivy::schema::Field,
    pub sink: Option<&'a tantivy::query::MatchedTermsSink>,
    /// The expansion cap (per segment); None = uncapped.
    pub cap: Option<usize>,
    /// The dictionary's terms keep marks: the cap counts mark-free forms.
    pub marked_terms: bool,
    /// Wrap the automaton in [`MarkFree`] (a mark-free automaton over a
    /// dictionary that keeps marks).
    pub mark_free: bool,
}

impl WeightPlan<'_> {
    fn weight<A>(&self, automaton: A) -> tantivy::query::AutomatonWeight<A>
    where
        A: Automaton + Send + Sync + 'static,
        A::State: Clone,
    {
        let mut weight = tantivy::query::AutomatonWeight::new(self.field, automaton);
        if let Some(sink) = self.sink {
            weight.set_matched_terms_sink(sink.clone());
        }
        if let Some(cap) = self.cap {
            weight.set_max_expansions(cap);
        }
        weight.set_zero_width_chars(zero_width_chars(self.marked_terms));
        weight
    }

    /// The query weight of `automaton` (wrapped in [`MarkFree`] when asked).
    pub fn boxed<A>(&self, automaton: A) -> Box<dyn tantivy::query::Weight>
    where
        A: Automaton + Send + Sync + 'static,
        A::State: Clone,
    {
        if self.mark_free {
            Box::new(self.weight(MarkFree::new(automaton)))
        } else {
            Box::new(self.weight(automaton))
        }
    }

    /// The span slot of `automaton` (wrapped in [`MarkFree`] when asked).
    pub fn span<A>(&self, automaton: A) -> std::sync::Arc<dyn tantivy::query::SpanTermSource>
    where
        A: Automaton + Send + Sync + 'static,
        A::State: Clone,
    {
        if self.mark_free {
            std::sync::Arc::new(self.weight(MarkFree::new(automaton)))
        } else {
            std::sync::Arc::new(self.weight(automaton))
        }
    }
}

/// A UTF-8 lead byte that can start a term mark: U+0300..U+036F (0xCC,
/// 0xCD) and U+0591..U+05C7 (0xD6, 0xD7).
#[inline]
pub(crate) fn is_mark_lead(byte: u8) -> bool {
    matches!(byte, 0xCC | 0xCD | 0xD6 | 0xD7)
}

/// Whether the two-byte character `lead`, `tail` is a term mark.
#[inline]
pub(crate) fn is_mark_char(lead: u8, tail: u8) -> bool {
    if tail & 0xC0 != 0x80 || !is_mark_lead(lead) {
        return false;
    }
    let code = (u32::from(lead & 0x1F) << 6) | u32::from(tail & 0x3F);
    char::from_u32(code).is_some_and(is_term_mark)
}

/// `term` (UTF-8 bytes) without its term marks.
pub fn strip_mark_bytes(term: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let mut out: Option<Vec<u8>> = None;
    let mut i = 0;
    while i < term.len() {
        let mark = i + 1 < term.len() && is_mark_char(term[i], term[i + 1]);
        let len = if mark { 2 } else { 1 };
        match (&mut out, mark) {
            (None, true) => out = Some(term[..i].to_vec()),
            (Some(out), false) => out.push(term[i]),
            _ => {}
        }
        i += len;
    }
    match out {
        Some(out) => std::borrow::Cow::Owned(out),
        None => std::borrow::Cow::Borrowed(term),
    }
}

/// Wraps `A` (built on mark-free text): accepts a term when `A` accepts it
/// with its term marks removed. Each mark is a zero-width step: the lead
/// byte of a possible mark is held until its tail shows whether it is a
/// mark (skipped) or another character (fed to `A`).
#[derive(Debug, Clone)]
pub struct MarkFree<A> {
    inner: A,
}

impl<A> MarkFree<A> {
    pub fn new(inner: A) -> Self {
        MarkFree { inner }
    }

    /// The wrapped automaton.
    pub fn inner(&self) -> &A {
        &self.inner
    }
}

/// The inner state, and the held lead byte of a possible mark (0: none).
#[derive(Clone, Debug)]
pub struct MarkFreeState<S> {
    inner: S,
    held: u8,
}

impl<A: Automaton> Automaton for MarkFree<A>
where
    A::State: Clone,
{
    type State = MarkFreeState<A::State>;

    fn start(&self) -> Self::State {
        MarkFreeState { inner: self.inner.start(), held: 0 }
    }

    fn is_match(&self, state: &Self::State) -> bool {
        state.held == 0 && self.inner.is_match(&state.inner)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        // With a held lead the inner state is the one before it: alive (a
        // mark keeps it), so the term may still match.
        self.inner.can_match(&state.inner)
    }

    fn will_always_match(&self, state: &Self::State) -> bool {
        state.held == 0 && self.inner.will_always_match(&state.inner)
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        if state.held != 0 {
            if is_mark_char(state.held, byte) {
                return MarkFreeState { inner: state.inner.clone(), held: 0 };
            }
            let after_lead = self.inner.accept(&state.inner, state.held);
            return MarkFreeState { inner: self.inner.accept(&after_lead, byte), held: 0 };
        }
        if is_mark_lead(byte) {
            return MarkFreeState { inner: state.inner.clone(), held: byte };
        }
        MarkFreeState { inner: self.inner.accept(&state.inner, byte), held: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::set_automaton::SetAutomaton;

    fn matches<A: Automaton>(a: &A, s: &str) -> bool
    where
        A::State: Clone,
    {
        let mut state = a.start();
        for &b in s.as_bytes() {
            if !a.can_match(&state) {
                return false;
            }
            state = a.accept(&state, b);
        }
        a.is_match(&state)
    }

    const TORAH: &str = "\u{05EA}\u{05D5}\u{05E8}\u{05D4}";

    #[test]
    fn marks_are_zero_width() {
        let a = MarkFree::new(SetAutomaton::from_strings([TORAH]));
        assert!(matches(&a, TORAH));
        // nikud, te'amim, meteg, the upper dot and a combining accent: skipped
        assert!(matches(&a, "\u{05EA}\u{05BC}\u{05D5}\u{05B9}\u{05E8}\u{05B8}\u{0591}\u{05D4}"));
        assert!(matches(&a, "\u{05EA}\u{05BD}\u{05D5}\u{05C4}\u{05E8}\u{0301}\u{05D4}\u{05C5}"));
        // letters still count: no extra or missing letter
        assert!(!matches(&a, "\u{05EA}\u{05D5}\u{05E8}"));
        assert!(!matches(&a, "\u{05EA}\u{05D5}\u{05E8}\u{05D4}\u{05D4}"));
        // a held lead byte at the end of a term is no match
        let mut state = a.start();
        for &b in TORAH.as_bytes().iter().chain(&[0xD6]) {
            state = a.accept(&state, b);
        }
        assert!(!a.is_match(&state) && a.can_match(&state));
    }

    #[test]
    fn strip_and_classify() {
        assert!(is_mark_char(0xD6, 0xB8) && is_mark_char(0xD7, 0x81) && is_mark_char(0xCC, 0x81));
        assert!(!is_mark_char(0xD7, 0x90)); // alef
        assert!(!is_mark_char(0xD6, 0xBE)); // maqaf is a separator
        assert!(!is_mark_char(0xD7, 0x80)); // paseq
        let vocalized = "\u{05EA}\u{05BC}\u{05D5}\u{05B9}\"\u{05E8}\u{0301}";
        assert_eq!(strip_mark_bytes(vocalized.as_bytes()).as_ref(), "\u{05EA}\u{05D5}\"\u{05E8}".as_bytes());
        assert!(matches!(strip_mark_bytes(TORAH.as_bytes()), std::borrow::Cow::Borrowed(_)));
        let zw = zero_width_chars(true);
        assert_eq!(zw.term_len(vocalized.as_bytes()), 3);
        assert_eq!(zw.strip(vocalized.as_bytes()).as_ref(), "\u{05EA}\u{05D5}\u{05E8}".as_bytes());
        assert_eq!(zero_width_chars(false).term_len(vocalized.as_bytes()), 6);
    }
}
