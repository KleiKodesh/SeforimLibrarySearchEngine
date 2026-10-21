//! Which words are words: FtsLib's model or Otzaria's.
//!
//! The index holds every word of one letter or more (Otzaria's tokenizer;
//! a term over [`MAX_TERM_BYTES`] is left out but keeps its position, as
//! Otzaria's `RemoveLongFilter`). A search picks how it reads that index:
//!
//! - [`WordModel::FtsLib`]: FtsLib's index has only words of 2..=29 letters.
//!   Every expansion admits only such terms, inside the walk
//!   ([`WordBounds`]), so the expansion cap counts exactly FtsLib's terms;
//!   the query parser drops other literals and the snippet locator's scanner
//!   counts only these words.
//! - [`WordModel::Otzaria`]: every word is a word: one-letter words are
//!   searched, matched and counted in distances (Otzaria's tokenizer).

use tantivy_fst::Automaton;

/// Otzaria's `MAX_TOKEN_BYTES`: a longer normalized word is not a term but
/// still takes its position.
pub const MAX_TERM_BYTES: usize = 128;

/// See the module docs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum WordModel {
    /// Words of 2..=29 letters (FtsLib).
    #[default]
    FtsLib,
    /// Every word of 1+ letters (Otzaria).
    Otzaria,
}

impl WordModel {
    /// The letters a word may have to be a word in this model (quotes and
    /// kept marks do not count), inclusive.
    #[inline]
    pub fn letter_bounds(self) -> (usize, usize) {
        match self {
            WordModel::FtsLib => (2, 29),
            WordModel::Otzaria => (1, usize::MAX),
        }
    }

    /// True when an index term is a word of this model.
    pub fn admits_term(self, term: &[u8]) -> bool {
        let (min, max) = self.letter_bounds();
        let n = term_letters(term);
        n >= min && n <= max
    }
}

/// Letters in an index term's bytes: lowercase ASCII and Hebrew alef-tav
/// (U+05D0..U+05EA = D7 90..D7 AA). Quotes and kept marks (D6 xx, D7 80..87)
/// are not letters.
pub fn term_letters(term: &[u8]) -> usize {
    let mut n = 0;
    let mut after_d7 = false;
    for &b in term {
        n += usize::from(is_letter_byte(b, after_d7));
        after_d7 = b == 0xD7;
    }
    n
}

#[inline(always)]
fn is_letter_byte(b: u8, after_d7: bool) -> bool {
    b.is_ascii_lowercase() || (after_d7 && (0x90..=0xAA).contains(&b))
}

/// Wraps an automaton so it accepts only terms whose letter count is within
/// `bounds` (see [`WordModel::letter_bounds`]); prunes the walk once a term
/// has more letters than allowed. With the Otzaria model it is the inner
/// automaton itself.
#[derive(Debug, Clone)]
pub struct WordBounds<A> {
    inner: A,
    min: u8,
    max: u8,
    enabled: bool,
}

impl<A> WordBounds<A> {
    pub fn new(inner: A, model: WordModel) -> Self {
        let (min, max) = model.letter_bounds();
        WordBounds {
            inner,
            min: min.min(255) as u8,
            max: max.min(254) as u8,
            enabled: model != WordModel::Otzaria,
        }
    }

    /// The wrapped automaton.
    pub fn inner(&self) -> &A {
        &self.inner
    }
}

/// The inner state plus the letters read so far (capped past `max`).
#[derive(Clone, Debug)]
pub struct WordBoundsState<S> {
    inner: S,
    letters: u8,
    after_d7: bool,
}

impl<A: Automaton> Automaton for WordBounds<A>
where
    A::State: Clone,
{
    type State = WordBoundsState<A::State>;

    fn start(&self) -> Self::State {
        WordBoundsState { inner: self.inner.start(), letters: 0, after_d7: false }
    }

    fn is_match(&self, state: &Self::State) -> bool {
        (!self.enabled || (state.letters >= self.min && state.letters <= self.max)) && self.inner.is_match(&state.inner)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        (!self.enabled || state.letters <= self.max) && self.inner.can_match(&state.inner)
    }

    fn will_always_match(&self, state: &Self::State) -> bool {
        !self.enabled && self.inner.will_always_match(&state.inner)
    }

    #[inline]
    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        let letter = self.enabled && is_letter_byte(byte, state.after_d7);
        WordBoundsState {
            inner: self.inner.accept(&state.inner, byte),
            letters: state.letters.saturating_add(u8::from(letter)),
            after_d7: byte == 0xD7,
        }
    }
}

/// Runs `automaton` over `term` (for candidate verification).
pub fn accepts<A: Automaton>(automaton: &A, term: &[u8]) -> bool {
    let mut state = automaton.start();
    for &b in term {
        if !automaton.can_match(&state) {
            return false;
        }
        state = automaton.accept(&state, b);
    }
    automaton.is_match(&state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SetAutomaton;

    #[test]
    fn letters_skip_quotes_and_marks() {
        assert_eq!(term_letters("רש\"י".as_bytes()), 3);
        assert_eq!(term_letters("בְּרֵאשִׁית".as_bytes()), 6);
        assert_eq!(term_letters("ו".as_bytes()), 1);
        assert_eq!(term_letters(b"abc"), 3);
        assert!(!WordModel::FtsLib.admits_term("ו".as_bytes()));
        assert!(WordModel::Otzaria.admits_term("ו".as_bytes()));
        assert!(WordModel::FtsLib.admits_term("א".repeat(29).as_bytes()));
        assert!(!WordModel::FtsLib.admits_term("א".repeat(30).as_bytes()));
        assert!(WordModel::Otzaria.admits_term("א".repeat(30).as_bytes()));
    }

    #[test]
    fn bounds_filter_inside_the_walk() {
        let long = "א".repeat(30);
        let words = ["ו", "וב", "ו\"ב", long.as_str()];
        let ftslib = WordBounds::new(SetAutomaton::from_strings(words), WordModel::FtsLib);
        let otzaria = WordBounds::new(SetAutomaton::from_strings(words), WordModel::Otzaria);
        let got = |a: &WordBounds<SetAutomaton>| words.iter().filter(|w| accepts(a, w.as_bytes())).count();
        assert_eq!(got(&ftslib), 2);
        assert_eq!(got(&otzaria), 4);
    }
}
