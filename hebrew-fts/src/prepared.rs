//! Port of FtsLib `PreparedQueryGroups.cs` (KitveiHakodesh, current).
//!
//! Immutable term→group lookup for one query, prepared ONCE and shared by
//! every snippet build over that query's results, across result lines and
//! threads. Preparing once keeps per-line snippet cost independent of how
//! many terms the query expanded to.
//!
//! With tantivy, each AND group's expansion comes straight from that group's
//! `MatchedTermsSink` after the search, so no re-expansion is needed.

use std::collections::HashMap;

/// One query term: its ordinal (0..term_count, first-seen order over the
/// groups) and the indices of every group that contains it.
#[derive(Debug, Clone)]
pub struct PreparedTerm {
    /// Position in the first-seen term list; what a mark's term ordinal means.
    pub ordinal: usize,
    /// Every group (AND slot) this term satisfies.
    pub groups: Vec<usize>,
}

/// Frozen term→group map for one query. Safe to share across threads.
#[derive(Debug, Clone)]
pub struct PreparedQueryGroups {
    term_to_groups: HashMap<String, PreparedTerm>,
    terms: Vec<String>,
    group_count: usize,
}

impl PreparedQueryGroups {
    /// AND-group semantics: OR within each group, AND across groups (the
    /// shape produced by query expansion: one group per query token). A term
    /// may appear in several groups and maps to all of them.
    pub fn from_groups<G, T>(query_groups: &[G]) -> PreparedQueryGroups
    where
        G: AsRef<[T]>,
        T: AsRef<str>,
    {
        let mut building: Vec<(String, Vec<usize>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (gi, group) in query_groups.iter().enumerate() {
            for term in group.as_ref() {
                let slot = Self::slot_for(term.as_ref(), &mut building, &mut index);
                let groups = &mut building[slot].1;
                if !groups.contains(&gi) {
                    groups.push(gi);
                }
            }
        }
        Self::freeze(building, query_groups.len())
    }

    /// Literal-term semantics: every term occurrence is its own AND group, so
    /// a term repeated N times must appear at least N times in the window.
    pub fn from_literal_terms<T: AsRef<str>>(query_terms: &[T]) -> PreparedQueryGroups {
        let mut building: Vec<(String, Vec<usize>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for (g, term) in query_terms.iter().enumerate() {
            let slot = Self::slot_for(term.as_ref(), &mut building, &mut index);
            building[slot].1.push(g);
        }
        Self::freeze(building, query_terms.len())
    }

    fn slot_for(
        term: &str,
        building: &mut Vec<(String, Vec<usize>)>,
        index: &mut HashMap<String, usize>,
    ) -> usize {
        if let Some(&i) = index.get(term) {
            return i;
        }
        index.insert(term.to_string(), building.len());
        building.push((term.to_string(), Vec::with_capacity(1)));
        building.len() - 1
    }

    fn freeze(building: Vec<(String, Vec<usize>)>, group_count: usize) -> PreparedQueryGroups {
        let mut term_to_groups = HashMap::with_capacity(building.len());
        let mut terms = Vec::with_capacity(building.len());
        for (ordinal, (term, groups)) in building.into_iter().enumerate() {
            terms.push(term.clone());
            term_to_groups.insert(term, PreparedTerm { ordinal, groups });
        }
        PreparedQueryGroups {
            term_to_groups,
            terms,
            group_count,
        }
    }

    /// Number of AND groups the sliding window must cover.
    pub fn group_count(&self) -> usize {
        self.group_count
    }

    /// True when the query has no terms at all; no snippet possible.
    pub fn is_empty(&self) -> bool {
        self.group_count == 0
    }

    /// Number of distinct terms; ordinals are 0..term_count.
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    /// The term with the given ordinal.
    pub fn term_at(&self, ordinal: usize) -> &str {
        &self.terms[ordinal]
    }

    /// The term's ordinal and groups, or None when it is not part of the
    /// query. Allocation-free: probes the map by `&str`.
    pub fn get(&self, term: &str) -> Option<&PreparedTerm> {
        self.term_to_groups.get(term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_share_terms_and_keep_first_seen_order() {
        let p = PreparedQueryGroups::from_groups(&[vec!["a", "b"], vec!["b", "c"]]);
        assert_eq!(p.group_count(), 2);
        assert_eq!(p.term_count(), 3);
        assert_eq!(p.term_at(0), "a");
        assert_eq!(p.term_at(1), "b");
        assert_eq!(p.get("b").unwrap().groups, vec![0, 1]);
        assert_eq!(p.get("b").unwrap().ordinal, 1);
        assert!(p.get("zzz").is_none());
    }

    #[test]
    fn literal_duplicates_become_separate_groups() {
        let p = PreparedQueryGroups::from_literal_terms(&["a", "a", "b"]);
        assert_eq!(p.group_count(), 3);
        assert_eq!(p.get("a").unwrap().groups, vec![0, 1]);
        assert_eq!(p.get("b").unwrap().groups, vec![2]);
    }

    #[test]
    fn empty() {
        let p = PreparedQueryGroups::from_groups::<Vec<&str>, &str>(&[]);
        assert!(p.is_empty());
    }
}
