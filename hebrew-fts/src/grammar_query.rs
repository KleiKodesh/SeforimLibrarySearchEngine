//! A tantivy query for Hebrew grammar expansion (`%word`, `word%`, `%word%`).
//!
//! The candidate forms ([`crate::grammar::build_candidates`]) are compiled
//! into a byte-trie automaton; tantivy's term-dictionary walk then visits only
//! branches spelling a candidate, so the index itself verifies which forms
//! exist — FtsLib's generate-then-verify, in one focused walk. Attach a
//! [`MatchedTermsSink`] to also receive the surviving forms with the results.

use tantivy::query::{AutomatonWeight, EnableScoring, MatchedTermsSink, Query, Weight};
use tantivy::schema::Field;

use crate::grammar::build_candidates;
use crate::set_automaton::SetAutomaton;

/// Matches every indexed form of a Hebrew base word under grammatical
/// prefix/suffix expansion (תחיליות/סופיות), per FtsLib's GrammarExpander.
#[derive(Debug, Clone)]
pub struct HebrewGrammarQuery {
    field: Field,
    word: String,
    expand_prefixes: bool,
    expand_suffixes: bool,
    matched_terms_sink: Option<MatchedTermsSink>,
    ignore_quotes: bool,
}

impl HebrewGrammarQuery {
    /// Makes the canonical quote transparent (התעלם מגרשיים): the word's own
    /// quotes are dropped and indexed terms match with or without them, as
    /// FtsLib's quote-free terms did.
    pub fn set_ignore_quotes(&mut self, ignore: bool) {
        self.ignore_quotes = ignore;
    }

    /// `word` is the normalized base word (no `%` markers).
    /// `expand_prefixes` ⇔ leading `%`, `expand_suffixes` ⇔ trailing `%`.
    /// The bare word itself always matches too.
    pub fn new(
        field: Field,
        word: impl Into<String>,
        expand_prefixes: bool,
        expand_suffixes: bool,
    ) -> HebrewGrammarQuery {
        HebrewGrammarQuery {
            field,
            word: word.into(),
            expand_prefixes,
            expand_suffixes,
            matched_terms_sink: None,
            ignore_quotes: false,
        }
    }

    /// Collects the indexed forms this query matches into `sink` while the
    /// search runs.
    pub fn set_matched_terms_sink(&mut self, sink: MatchedTermsSink) {
        self.matched_terms_sink = Some(sink);
    }
}

impl Query for HebrewGrammarQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let word: String = if self.ignore_quotes {
            self.word.chars().filter(|&c| c != '"').collect()
        } else {
            self.word.clone()
        };
        let candidates = build_candidates(&word, self.expand_prefixes, self.expand_suffixes);
        let mut automaton = SetAutomaton::from_strings(&candidates);
        if self.ignore_quotes {
            automaton = automaton.with_transparent_quotes();
        }
        let mut weight = AutomatonWeight::new(self.field, automaton);
        if let Some(sink) = &self.matched_terms_sink {
            weight.set_matched_terms_sink(sink.clone());
        }
        weight.set_max_expansions(crate::walk::DEFAULT_MAX_EXPANDED_TERMS);
        if let Some(searcher) = enable_scoring.searcher() {
            weight.apply_global_cap(searcher)?;
        }
        Ok(Box::new(weight))
    }
}

#[cfg(test)]
mod tests {
    use tantivy::collector::DocSetCollector;
    use tantivy::query::MatchedTermsSink;
    use tantivy::schema::{Schema, TEXT};
    use tantivy::{doc, Index, IndexWriter};

    use super::HebrewGrammarQuery;

    fn build_index(lines: &[&str]) -> tantivy::Result<(Index, tantivy::schema::Field)> {
        let mut schema_builder = Schema::builder();
        let text = schema_builder.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema_builder.build());
        let mut writer: IndexWriter = index.writer(15_000_000)?;
        for line in lines {
            writer.add_document(doc!(text => *line))?;
        }
        writer.commit()?;
        Ok((index, text))
    }

    #[test]
    fn grammar_query_matches_only_candidate_forms() -> tantivy::Result<()> {
        let (index, field) = build_index(&[
            "תורה",   // doc 0: bare word
            "התורה",  // doc 1: prefix ה
            "ולכשתורה", // doc 2: quad prefix
            "מתורתו", // doc 3: NOT a candidate (internal change)
            "תור",    // doc 4: different word
        ])?;
        let searcher = index.reader()?.searcher();

        let mut query = HebrewGrammarQuery::new(field, "תורה", true, false);
        let sink = MatchedTermsSink::new();
        query.set_matched_terms_sink(sink.clone());
        let docs = searcher.search(&query, &DocSetCollector)?;
        let mut doc_ids: Vec<u32> = docs.into_iter().map(|addr| addr.doc_id).collect();
        doc_ids.sort_unstable();
        assert_eq!(doc_ids, vec![0, 1, 2]);
        assert_eq!(
            sink.take_strings(),
            vec!["התורה".to_string(), "ולכשתורה".to_string(), "תורה".to_string()]
        );
        Ok(())
    }

    #[test]
    fn suffix_expansion_with_sofit_stem() -> tantivy::Result<()> {
        let (index, field) = build_index(&["זקן", "זקנה", "זקנים", "הזקנים", "זקןה"])?;
        let searcher = index.reader()?.searcher();

        // word% — suffixes only: bare word, stem+suffix; no prefix forms.
        let mut query = HebrewGrammarQuery::new(field, "זקן", false, true);
        let sink = MatchedTermsSink::new();
        query.set_matched_terms_sink(sink.clone());
        let docs = searcher.search(&query, &DocSetCollector)?;
        assert_eq!(docs.len(), 3); // זקן, זקנה, זקנים — not הזקנים, not זקןה
        assert_eq!(
            sink.take_strings(),
            vec!["זקן".to_string(), "זקנה".to_string(), "זקנים".to_string()]
        );

        // %word% — full expansion also reaches הזקנים.
        let query = HebrewGrammarQuery::new(field, "זקן", true, true);
        let docs = searcher.search(&query, &DocSetCollector)?;
        assert_eq!(docs.len(), 4);
        Ok(())
    }
}
