//! Quote-free term matching at query time: Otzaria's `QuoteTransparent`
//! (otzaria_search_engine `rust/src/quote_expansion.rs`), ported.
//!
//! Otzaria's indexer once emitted, beside every word carrying a geresh or
//! gershayim (`רמב"ם`), a quote-free twin at the same position (`רמבם`), so a
//! query for `רמבם` matched both. Its schema 6 dropped the twin and gives the
//! same answers at query time: every term automaton is wrapped so that it
//! accepts an index term as written OR with all its quotes removed. Our index
//! never had a twin (bare-minimum index), so this wrapper is the whole feature.
//!
//! Semantics (exactly Otzaria's): a term is accepted when the inner automaton
//! accepts it as written, or accepts it with every quote byte removed, provided
//! that quote-free form has at least one letter. So `רמבם` finds `רמב"ם`, while
//! `רמב"ם` (quote typed) still needs the quote in the term, unless התעלם
//! מגרשיים strips it from the query. It composes with every other option: the
//! inner automaton (prefixes, suffixes, ktiv, …) runs on the quote-free bytes.

use tantivy_fst::Automaton;

/// The quote bytes an index term can hold (the tokenizer folds ׳/״ to '"').
#[inline]
fn is_quote(byte: u8) -> bool {
    byte == b'\'' || byte == b'"'
}

/// Wraps `A`: accepts a term `A` accepts as written or without its quotes.
/// With `enabled == false` it is `A` itself.
#[derive(Debug)]
pub struct QuoteFree<A> {
    inner: A,
    enabled: bool,
}

impl<A> QuoteFree<A> {
    pub fn new(inner: A, enabled: bool) -> Self {
        QuoteFree { inner, enabled }
    }
}

/// The literal run and the quote-free run of the inner automaton; a single
/// run until the first quote byte, which is the common case.
#[derive(Clone, Debug)]
pub enum QuoteFreeState<S> {
    /// No quote yet; `letters`: a non-quote byte was consumed.
    Same { state: S, letters: bool },
    Split {
        literal: Option<S>,
        bare: Option<S>,
        /// The quote-free run consumed a letter: an all-quote term has none.
        bare_letters: bool,
    },
}

impl<A: Automaton> Automaton for QuoteFree<A>
where
    A::State: Clone,
{
    type State = QuoteFreeState<A::State>;

    fn start(&self) -> Self::State {
        QuoteFreeState::Same {
            state: self.inner.start(),
            letters: false,
        }
    }

    fn is_match(&self, state: &Self::State) -> bool {
        match state {
            QuoteFreeState::Same { state, .. } => self.inner.is_match(state),
            QuoteFreeState::Split {
                literal,
                bare,
                bare_letters,
            } => {
                literal.as_ref().is_some_and(|s| self.inner.is_match(s))
                    || (*bare_letters && bare.as_ref().is_some_and(|s| self.inner.is_match(s)))
            }
        }
    }

    fn can_match(&self, state: &Self::State) -> bool {
        match state {
            QuoteFreeState::Same { state, .. } => self.inner.can_match(state),
            QuoteFreeState::Split { literal, bare, .. } => {
                literal.as_ref().is_some_and(|s| self.inner.can_match(s))
                    || bare.as_ref().is_some_and(|s| self.inner.can_match(s))
            }
        }
    }

    fn will_always_match(&self, state: &Self::State) -> bool {
        // Before any quote the term is read as written: the literal run alone
        // decides, so the inner automaton's answer holds.
        match state {
            QuoteFreeState::Same { state, .. } => self.inner.will_always_match(state),
            QuoteFreeState::Split { .. } => false,
        }
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        let alive = |s: A::State| self.inner.can_match(&s).then_some(s);
        match state {
            QuoteFreeState::Same { state, .. } if !self.enabled || !is_quote(byte) => {
                QuoteFreeState::Same {
                    state: self.inner.accept(state, byte),
                    letters: true,
                }
            }
            QuoteFreeState::Same { state, letters } => QuoteFreeState::Split {
                literal: alive(self.inner.accept(state, byte)),
                bare: alive(state.clone()),
                bare_letters: *letters,
            },
            QuoteFreeState::Split {
                literal,
                bare,
                bare_letters,
            } => QuoteFreeState::Split {
                literal: literal.as_ref().and_then(|s| alive(self.inner.accept(s, byte))),
                bare: if is_quote(byte) {
                    bare.clone()
                } else {
                    bare.as_ref().and_then(|s| alive(self.inner.accept(s, byte)))
                },
                bare_letters: *bare_letters || !is_quote(byte),
            },
        }
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

    #[test]
    fn quote_free_form_or_as_written() {
        let plain = QuoteFree::new(SetAutomaton::from_strings(["רמבם"]), true);
        assert!(matches(&plain, "רמבם"));
        assert!(matches(&plain, "רמב\"ם"));
        assert!(matches(&plain, "ר\"מבם"));
        let quoted = QuoteFree::new(SetAutomaton::from_strings(["רמב\"ם"]), true);
        assert!(matches(&quoted, "רמב\"ם"));
        assert!(!matches(&quoted, "רמבם"));
        // all-quote terms have no quote-free spelling
        let empty = QuoteFree::new(SetAutomaton::from_strings([""]), true);
        assert!(!matches(&empty, "\""));
    }

    #[test]
    fn disabled_is_the_inner_automaton() {
        let a = QuoteFree::new(SetAutomaton::from_strings(["רמבם"]), false);
        assert!(matches(&a, "רמבם"));
        assert!(!matches(&a, "רמב\"ם"));
    }
}
