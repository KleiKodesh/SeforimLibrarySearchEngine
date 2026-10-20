//! A query over an explicit list of index terms (already expanded and
//! capped elsewhere, e.g. through the FM-index sidecar). Runs as the same
//! [`AutomatonWeight`] the walks use, over a [`SetAutomaton`] of the terms,
//! so the sink reports exactly these terms and it serves as a span slot.

use std::sync::Arc;

use tantivy::query::{AutomatonWeight, EnableScoring, MatchedTermsSink, Query, SpanTermSource, Weight};
use tantivy::schema::Field;

use crate::SetAutomaton;

/// "Any of these exact terms".
#[derive(Debug, Clone)]
pub struct ExactTermsQuery {
    field: Field,
    terms: Vec<Vec<u8>>,
    matched_terms_sink: Option<MatchedTermsSink>,
}

impl ExactTermsQuery {
    /// `terms`: index term bytes (UTF-8 of the normalized words).
    pub fn new(field: Field, terms: Vec<Vec<u8>>) -> ExactTermsQuery {
        ExactTermsQuery {
            field,
            terms,
            matched_terms_sink: None,
        }
    }

    /// Collects the terms present in the index into `sink` during the search.
    pub fn set_matched_terms_sink(&mut self, sink: MatchedTermsSink) {
        self.matched_terms_sink = Some(sink);
    }

    /// The terms.
    pub fn terms(&self) -> &[Vec<u8>] {
        &self.terms
    }

    fn automaton_weight(&self) -> AutomatonWeight<SetAutomaton> {
        let automaton = SetAutomaton::from_bytes(self.terms.iter().map(|t| t.as_slice()));
        let mut weight = AutomatonWeight::new(self.field, automaton);
        if let Some(sink) = &self.matched_terms_sink {
            weight.set_matched_terms_sink(sink.clone());
        }
        weight
    }

    /// This list as a span slot.
    pub fn span_slot(&self) -> Arc<dyn SpanTermSource> {
        Arc::new(self.automaton_weight())
    }
}

impl Query for ExactTermsQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(self.automaton_weight()))
    }
}
