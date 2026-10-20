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

use rustc_hash::FxHashMap;

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
    /// FxHash: the snippet locator probes this once per token of every
    /// located line, so hashing speed matters; the map is built from query
    /// terms only (no untrusted-key DoS concern). Lookup results are the same
    /// as with any hasher.
    term_to_groups: FxHashMap<String, PreparedTerm>,
    /// Bit `min(len, 63)` set for every term's byte length: a token whose
    /// length has no bit cannot be a term, so most tokens skip the hash.
    len_mask: u64,
    /// 256-bit sets of every term's second byte (its first Hebrew letter's
    /// distinguishing byte) and last byte. Same idea: a cheap necessary
    /// condition checked before hashing. Empty terms are kept out of these
    /// filters (`has_empty` sends empty probes straight to the map).
    second_byte: [u64; 4],
    last_byte: [u64; 4],
    has_empty: bool,
    /// Each term's groups by ordinal (a copy of `term_to_groups`' values),
    /// so a caller holding an ordinal reads them without hashing.
    groups_by_ordinal: Vec<Vec<usize>>,
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
        let mut term_to_groups = FxHashMap::default();
        term_to_groups.reserve(building.len());
        let mut terms = Vec::with_capacity(building.len());
        let mut len_mask = 0u64;
        let mut second_byte = [0u64; 4];
        let mut last_byte = [0u64; 4];
        let mut has_empty = false;
        let mut groups_by_ordinal = Vec::with_capacity(building.len());
        for (ordinal, (term, groups)) in building.into_iter().enumerate() {
            len_mask |= 1u64 << term.len().min(63);
            match Self::probe_bytes(&term) {
                Some((a, z)) => {
                    second_byte[a as usize >> 6] |= 1u64 << (a & 63);
                    last_byte[z as usize >> 6] |= 1u64 << (z & 63);
                }
                None => has_empty = true,
            }
            terms.push(term.clone());
            groups_by_ordinal.push(groups.clone());
            term_to_groups.insert(term, PreparedTerm { ordinal, groups });
        }
        PreparedQueryGroups {
            term_to_groups,
            len_mask,
            second_byte,
            last_byte,
            has_empty,
            groups_by_ordinal,
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
        if self.len_mask & (1u64 << term.len().min(63)) == 0 {
            return None; // no term has this byte length
        }
        match Self::probe_bytes(term) {
            Some((a, z)) => {
                let has = |set: &[u64; 4], x: u8| set[x as usize >> 6] & (1u64 << (x & 63)) != 0;
                if !has(&self.second_byte, a) || !has(&self.last_byte, z) {
                    return None; // no term has these bytes there
                }
            }
            None if !self.has_empty => return None,
            None => {}
        }
        self.term_to_groups.get(term)
    }

    /// The bytes the pre-filters key on: the second byte (the first byte when
    /// the string has one) and the last byte. None for the empty string.
    #[inline(always)]
    fn probe_bytes(s: &str) -> Option<(u8, u8)> {
        let b = s.as_bytes();
        let last = *b.last()?;
        Some((b[1.min(b.len() - 1)], last))
    }

    /// The groups of the term with the given ordinal.
    pub fn groups_at(&self, ordinal: usize) -> &[usize] {
        &self.groups_by_ordinal[ordinal]
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
