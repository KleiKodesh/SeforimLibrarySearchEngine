//! Fuzzy (Levenshtein) query with FtsLib's expansion rules.
//!
//! The same Levenshtein DFA tantivy's `FuzzyTermQuery` uses, run through an
//! [`AutomatonWeight`] configured the FtsLib way: capped at
//! [`DEFAULT_MAX_EXPANDED_TERMS`] across all segments, closest (edit distance) first,
//! then shortest (the intra-word quote is zero-width, and the marks on an
//! index that keeps them, see [`crate::marks::zero_width_chars`]), then most
//! frequent. With a [`MatchedTermsSink`]
//! attached the matched forms come back with the search.
//!
//! tantivy's own DFA wrapper cannot be built from outside the crate, so this
//! module carries its own (identical) one.

use std::sync::{Arc, OnceLock};

pub use levenshtein_automata::Distance;
use levenshtein_automata::{LevenshteinAutomatonBuilder, DFA};
use tantivy::query::{
    AutomatonWeight, EnableScoring, MatchedTermsSink, Query, SpanTermSource, Weight,
};
use tantivy::schema::Field;
use tantivy_fst::Automaton;

use crate::marks::MarkFree;
use crate::walk::DEFAULT_MAX_EXPANDED_TERMS;

/// A Levenshtein DFA as a tantivy automaton. With `transparent_quotes` the
/// canonical quote byte `"` is skipped (the state does not move), so an
/// indexed רש"י is as far from the word as רשי: FtsLib's terms carried no
/// quotes, and the ignore-gershayim option makes every other walk do the same.
pub struct LevenshteinDfa {
    dfa: DFA,
    transparent_quotes: bool,
    /// [`Self::distance`] skips the term's marks (an index that keeps marks;
    /// the walk itself runs the DFA inside [`MarkFree`]).
    transparent_marks: bool,
    /// The fewest edits a match takes: 1 for a vocalized fuzzy word (its
    /// exact form is matched with its typed marks by a word walk, see
    /// [`HebrewFuzzyQuery`]), else 0.
    min_distance: u8,
}

impl LevenshteinDfa {
    /// The edit distance of `term` from the query word (`AtLeast` for
    /// non-matches), quotes skipped when they are transparent, marks when
    /// the index keeps them (FtsLib's Levenshtein ran on mark-free terms).
    pub fn distance(&self, term: &[u8]) -> Distance {
        let unmarked;
        let term = if self.transparent_marks {
            unmarked = crate::marks::strip_mark_bytes(term);
            unmarked.as_ref()
        } else {
            term
        };
        if self.transparent_quotes && term.iter().any(|&b| crate::chars::is_term_quote_byte(b)) {
            let stripped: Vec<u8> = term.iter().copied().filter(|&b| !crate::chars::is_term_quote_byte(b)).collect();
            self.dfa.eval(stripped)
        } else {
            self.dfa.eval(term)
        }
    }

    /// The fewest edits a match takes (1: typo variants only).
    pub fn min_distance(&self) -> u8 {
        self.min_distance
    }
}

impl Automaton for LevenshteinDfa {
    type State = u32;

    fn start(&self) -> Self::State {
        self.dfa.initial_state()
    }

    fn is_match(&self, state: &Self::State) -> bool {
        match self.dfa.distance(*state) {
            Distance::Exact(d) => d >= self.min_distance,
            Distance::AtLeast(_) => false,
        }
    }

    fn can_match(&self, state: &u32) -> bool {
        *state != levenshtein_automata::SINK_STATE
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        if self.transparent_quotes && crate::chars::is_term_quote_byte(byte) {
            return *state;
        }
        self.dfa.transition(*state, byte)
    }
}

/// One DFA shared by the walk (inside [`MarkFree`]) and the cap's rank.
struct SharedDfa(Arc<LevenshteinDfa>);

impl Automaton for SharedDfa {
    type State = u32;

    fn start(&self) -> u32 {
        self.0.start()
    }

    fn is_match(&self, state: &u32) -> bool {
        self.0.is_match(state)
    }

    fn can_match(&self, state: &u32) -> bool {
        self.0.can_match(state)
    }

    fn accept(&self, state: &u32, byte: u8) -> u32 {
        self.0.accept(state, byte)
    }
}

/// Fuzzy query for one word: every indexed term within `distance` edits
/// (insert, delete, substitute, and a swap of two adjacent letters as ONE
/// edit: Otzaria's `FuzzyTermQuery(.., true)`, FtsLib's `Levenshtein.Distance`).
///
/// A word that carries marks is a VOCALIZED fuzzy word (`match_nikud` /
/// `match_teamim` on an index that keeps marks; the marks left are the
/// required ones, see [`crate::walk::required_marks_form`]): Otzaria's rule,
/// the typo variants match mark-free (marks never count as edits), and the
/// exact typed form needs its typed marks. This query then matches the typo
/// variants only (one edit or more, measured on the mark-free forms); the
/// exact form is the word walk with the typed marks required, ORed with it
/// in the same slot by the engine.
#[derive(Debug, Clone)]
pub struct HebrewFuzzyQuery {
    field: Field,
    word: String,
    distance: u8,
    matched_terms_sink: Option<MatchedTermsSink>,
    max_expansions: usize,
    ignore_quotes: bool,
    /// The index keeps marks in its terms: they are read as zero width.
    marked_terms: bool,
}

/// FtsLib's `FuzzyExpander.MaxAllowedDistance`.
pub const MAX_FUZZY_DISTANCE: u8 = 3;

impl HebrewFuzzyQuery {
    /// `distance`: 1..=[`MAX_FUZZY_DISTANCE`] (clamped, as FtsLib's
    /// `ExpandFuzzy`). The expansion is capped at
    /// [`DEFAULT_MAX_EXPANDED_TERMS`] across all segments, closest first.
    pub fn new(field: Field, word: impl Into<String>, distance: u8) -> HebrewFuzzyQuery {
        HebrewFuzzyQuery {
            field,
            word: word.into(),
            distance: distance.clamp(1, MAX_FUZZY_DISTANCE),
            matched_terms_sink: None,
            max_expansions: DEFAULT_MAX_EXPANDED_TERMS,
            ignore_quotes: false,
            marked_terms: false,
        }
    }

    /// The index keeps marks in its terms (`keep_marks`): distances are
    /// measured on the mark-free forms ([`MarkFree`]).
    pub fn set_marked_terms(&mut self, marked_terms: bool) {
        self.marked_terms = marked_terms;
    }

    /// Makes the quote transparent (התעלם מגרשיים): distances are measured
    /// on the quote-free forms, as FtsLib's quote-free terms were.
    pub fn set_ignore_quotes(&mut self, ignore: bool) {
        self.ignore_quotes = ignore;
    }

    /// True when the quote is transparent.
    pub fn ignore_quotes(&self) -> bool {
        self.ignore_quotes
    }

    /// Collects the indexed forms this query matches into `sink` during the
    /// search.
    pub fn set_matched_terms_sink(&mut self, sink: MatchedTermsSink) {
        self.matched_terms_sink = Some(sink);
    }

    /// Overrides the expansion cap (one cap across all segments; 0 = uncapped).
    pub fn set_max_expansions(&mut self, cap: usize) {
        self.max_expansions = cap;
    }

    /// The query word.
    pub fn word(&self) -> &str {
        &self.word
    }

    /// The (clamped) edit distance.
    pub fn distance_limit(&self) -> u8 {
        self.distance
    }

    /// The expansion cap.
    pub fn max_expansions(&self) -> usize {
        self.max_expansions
    }

    /// The Levenshtein DFA for this word (builder cached per distance, as
    /// in tantivy).
    pub fn automaton(&self) -> LevenshteinDfa {
        static BUILDERS: [OnceLock<LevenshteinAutomatonBuilder>; 4] =
            [OnceLock::new(), OnceLock::new(), OnceLock::new(), OnceLock::new()];
        let builder = BUILDERS[self.distance as usize]
            .get_or_init(|| LevenshteinAutomatonBuilder::new(self.distance, true));
        let vocalized = self.is_vocalized();
        let word: String = self
            .word
            .chars()
            .filter(|&c| !(self.ignore_quotes && crate::chars::is_term_quote(c)) && !crate::chars::is_term_mark(c))
            .collect();
        LevenshteinDfa {
            dfa: builder.build_dfa(&word),
            transparent_quotes: self.ignore_quotes,
            transparent_marks: self.marked_terms,
            min_distance: u8::from(vocalized),
        }
    }

    /// True for a vocalized fuzzy word (it carries required marks): only
    /// its typo variants match here (see [`HebrewFuzzyQuery`]).
    pub fn is_vocalized(&self) -> bool {
        self.word.chars().any(crate::chars::is_term_mark)
    }

    /// The word without marks: the form the distances are measured from.
    pub fn mark_free_word(&self) -> String {
        self.word.chars().filter(|&c| !crate::chars::is_term_mark(c)).collect()
    }

    fn configure<A>(&self, mut weight: AutomatonWeight<A>, dfa: Arc<LevenshteinDfa>, segments: Option<&[tantivy::SegmentReader]>) -> AutomatonWeight<A>
    where
        A: Automaton + Send + Sync + 'static,
        A::State: Clone,
    {
        if let Some(sink) = &self.matched_terms_sink {
            weight.set_matched_terms_sink(sink.clone());
        }
        if self.max_expansions > 0 {
            weight.set_max_expansions(self.max_expansions);
            weight.set_pinned_terms(crate::marks::exact_forms(&self.mark_free_word()));
            if let Some(segments) = segments {
                weight.set_cap_across_segments(segments);
            }
        }
        weight.set_zero_width_chars(crate::marks::zero_width_chars(self.marked_terms));
        // Closest first: the edit distance of the matched term.
        weight.set_expansion_rank(Arc::new(move |key: &[u8]| match dfa.distance(key) {
            Distance::Exact(d) | Distance::AtLeast(d) => d as u32,
        }));
        weight
    }

    /// This query as a slot of a [`SpanQuery`](tantivy::query::SpanQuery):
    /// "any word within the distance", with positions. Same cap, rank and
    /// sink as the plain query.
    pub fn span_slot(
        &self,
        searcher: &tantivy::Searcher,
    ) -> tantivy::Result<Arc<dyn SpanTermSource>> {
        let dfa = Arc::new(self.automaton());
        Ok(if self.marked_terms {
            Arc::new(self.configure(AutomatonWeight::<MarkFree<SharedDfa>>::new(self.field, MarkFree::new(SharedDfa(dfa.clone()))), dfa, Some(searcher.segment_readers())))
        } else {
            Arc::new(self.configure(AutomatonWeight::<LevenshteinDfa>::new(self.field, dfa.clone()), dfa, Some(searcher.segment_readers())))
        })
    }
}

impl Query for HebrewFuzzyQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let dfa = Arc::new(self.automaton());
        Ok(if self.marked_terms {
            Box::new(self.configure(AutomatonWeight::<MarkFree<SharedDfa>>::new(self.field, MarkFree::new(SharedDfa(dfa.clone()))), dfa, enable_scoring.searcher().map(|s| s.segment_readers())))
        } else {
            Box::new(self.configure(AutomatonWeight::<LevenshteinDfa>::new(self.field, dfa.clone()), dfa, enable_scoring.searcher().map(|s| s.segment_readers())))
        })
    }
}

#[cfg(test)]
mod tests {
    use tantivy::collector::Count;
    use tantivy::schema::{Schema, STRING};
    use tantivy::{doc, Index, IndexWriter};

    use super::*;

    fn index_of(words: &[&str]) -> (Index, Field) {
        let mut schema = Schema::builder();
        let field = schema.add_text_field("t", STRING);
        let index = Index::create_in_ram(schema.build());
        let mut writer: IndexWriter = index.writer(15_000_000).unwrap();
        for w in words {
            writer.add_document(doc!(field => *w)).unwrap();
        }
        writer.commit().unwrap();
        (index, field)
    }

    #[test]
    fn adjacent_swap_is_one_edit() {
        // Otzaria's FuzzyTermQuery(.., transposition = true); FtsLib's Levenshtein.Distance
        let (index, field) = index_of(&["bacd", "abdc", "badc", "acbd"]);
        let searcher = index.reader().unwrap().searcher();
        let mut q = HebrewFuzzyQuery::new(field, "abcd", 1);
        let sink = MatchedTermsSink::new();
        q.set_matched_terms_sink(sink.clone());
        assert_eq!(searcher.search(&q, &Count).unwrap(), 3); // badc is two swaps away
        let dfa = q.automaton();
        assert!(matches!(dfa.distance(b"bacd"), Distance::Exact(1)));
        assert!(matches!(dfa.distance(b"badc"), Distance::AtLeast(_)));
    }

    #[test]
    fn matches_within_distance_and_reports_terms() {
        let (index, field) = index_of(&["שלום", "שלומ", "שלוםם", "ברכה"]);
        let searcher = index.reader().unwrap().searcher();
        let mut q = HebrewFuzzyQuery::new(field, "שלום", 1);
        let sink = MatchedTermsSink::new();
        q.set_matched_terms_sink(sink.clone());
        assert_eq!(searcher.search(&q, &Count).unwrap(), 3);
        assert_eq!(sink.take_strings(), vec!["שלום", "שלוםם", "שלומ"]);
    }

    #[test]
    fn cap_keeps_closest_then_shortest_quote_zero_width() {
        // רשי and רש"י are ONE slot (FtsLib's index held only the quote-free
        // רשי), at the best member's distance 0; the second slot goes to the
        // next closest, רשיי (distance 1, shorter than רשיים)
        let (index, field) = index_of(&["רשי", "רש\"י", "רשיי", "רשיים"]);
        let searcher = index.reader().unwrap().searcher();
        let mut q = HebrewFuzzyQuery::new(field, "רשי", 2);
        let sink = MatchedTermsSink::new();
        q.set_matched_terms_sink(sink.clone());
        q.set_max_expansions(2);
        assert_eq!(searcher.search(&q, &Count).unwrap(), 3);
        assert_eq!(sink.take_strings(), vec!["רש\"י", "רשי", "רשיי"]);
    }
}
