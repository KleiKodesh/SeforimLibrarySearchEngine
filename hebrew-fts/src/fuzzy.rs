//! Fuzzy (Levenshtein) query with FtsLib's expansion rules.
//!
//! The same Levenshtein DFA tantivy's `FuzzyTermQuery` uses, run through an
//! [`AutomatonWeight`] configured the FtsLib way: capped at
//! [`DEFAULT_MAX_EXPANDED_TERMS`] per segment, closest (edit distance) first,
//! then shortest (the intra-word quote is zero-width, see
//! [`ZERO_WIDTH_BYTES`]), then most frequent. With a [`MatchedTermsSink`]
//! attached the matched forms come back with the search.
//!
//! tantivy's own DFA wrapper cannot be built from outside the crate, so this
//! module carries its own (identical) one.

use std::sync::{Arc, OnceLock};

use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA};
use tantivy::query::{
    AutomatonWeight, EnableScoring, MatchedTermsSink, Query, SpanTermSource, Weight,
};
use tantivy::schema::Field;
use tantivy_fst::Automaton;

use crate::walk::{DEFAULT_MAX_EXPANDED_TERMS, ZERO_WIDTH_BYTES};

/// A Levenshtein DFA as a tantivy automaton.
pub struct LevenshteinDfa(DFA);

impl LevenshteinDfa {
    /// The edit distance of `term` from the query word (`AtLeast` for
    /// non-matches).
    pub fn distance(&self, term: &[u8]) -> Distance {
        self.0.eval(term)
    }
}

impl Automaton for LevenshteinDfa {
    type State = u32;

    fn start(&self) -> Self::State {
        self.0.initial_state()
    }

    fn is_match(&self, state: &Self::State) -> bool {
        match self.0.distance(*state) {
            Distance::Exact(_) => true,
            Distance::AtLeast(_) => false,
        }
    }

    fn can_match(&self, state: &u32) -> bool {
        *state != levenshtein_automata::SINK_STATE
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        self.0.transition(*state, byte)
    }
}

/// Fuzzy query for one word: every indexed term within `distance` plain
/// Levenshtein edits (insert, delete, substitute; a transposition is two
/// edits, as in FtsLib's `Levenshtein.Distance`).
#[derive(Debug, Clone)]
pub struct HebrewFuzzyQuery {
    field: Field,
    word: String,
    distance: u8,
    matched_terms_sink: Option<MatchedTermsSink>,
    max_expansions: usize,
}

impl HebrewFuzzyQuery {
    /// `distance`: 1 or 2 (clamped). The expansion is capped at
    /// [`DEFAULT_MAX_EXPANDED_TERMS`] per segment, closest first.
    pub fn new(field: Field, word: impl Into<String>, distance: u8) -> HebrewFuzzyQuery {
        HebrewFuzzyQuery {
            field,
            word: word.into(),
            distance: distance.clamp(1, 2),
            matched_terms_sink: None,
            max_expansions: DEFAULT_MAX_EXPANDED_TERMS,
        }
    }

    /// Collects the indexed forms this query matches into `sink` during the
    /// search.
    pub fn set_matched_terms_sink(&mut self, sink: MatchedTermsSink) {
        self.matched_terms_sink = Some(sink);
    }

    /// Overrides the expansion cap (per segment).
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
        static BUILDERS: [OnceLock<LevenshteinAutomatonBuilder>; 3] =
            [OnceLock::new(), OnceLock::new(), OnceLock::new()];
        let builder = BUILDERS[self.distance as usize]
            .get_or_init(|| LevenshteinAutomatonBuilder::new(self.distance, false));
        LevenshteinDfa(builder.build_dfa(&self.word))
    }

    fn automaton_weight(&self) -> AutomatonWeight<LevenshteinDfa> {
        let automaton = Arc::new(self.automaton());
        let mut weight = AutomatonWeight::new(self.field, automaton.clone());
        if let Some(sink) = &self.matched_terms_sink {
            weight.set_matched_terms_sink(sink.clone());
        }
        weight.set_max_expansions(self.max_expansions);
        weight.set_zero_width_bytes(ZERO_WIDTH_BYTES);
        // Closest first: the edit distance of the matched term.
        weight.set_expansion_rank(Arc::new(move |key: &[u8]| match automaton.distance(key) {
            Distance::Exact(d) | Distance::AtLeast(d) => d as u32,
        }));
        weight
    }

    /// This query as a slot of a [`SpanQuery`](tantivy::query::SpanQuery):
    /// "any word within the distance", with positions. Same cap, rank and
    /// sink as the plain query.
    pub fn span_slot(&self, _searcher: &tantivy::Searcher) -> tantivy::Result<Arc<dyn SpanTermSource>> {
        Ok(Arc::new(self.automaton_weight()))
    }
}

impl Query for HebrewFuzzyQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(self.automaton_weight()))
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
        // distance 0 beats distance 1; among distance 1, רש"י (3 letters +
        // quote) is as short as רשי and shorter than רשיי
        let (index, field) = index_of(&["רשי", "רש\"י", "רשיי", "רשיים"]);
        let searcher = index.reader().unwrap().searcher();
        let mut q = HebrewFuzzyQuery::new(field, "רשי", 2);
        let sink = MatchedTermsSink::new();
        q.set_matched_terms_sink(sink.clone());
        q.set_max_expansions(2);
        assert_eq!(searcher.search(&q, &Count).unwrap(), 2);
        assert_eq!(sink.take_strings(), vec!["רש\"י", "רשי"]);
    }
}
