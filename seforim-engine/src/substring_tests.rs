//! Issue G-K1: FtsLib's `FindSubstringCandidates` (KitveiHakodesh's in-book
//! and commentary find) as engine options: the substring query, the
//! catalog-position ranges and the decline over the cap. The expected
//! answers follow FtsLib's steps (`Fts5SearchEngine.FindSubstringCandidates`):
//! every whitespace word cut into the indexer's terms, each term the group of
//! EVERY dictionary term containing it (FM-index, no affix budget), null when
//! a word has no term, the index has no FM-index or a group has more terms
//! than the cap, AND across the groups, rows restricted to the ranges.

use crate::index::{IndexOptions, LineDoc, LineIndex};
use crate::options::SearchOptions;

fn line(id: u64, position: u64, book: u64, text: &str) -> LineDoc {
    LineDoc { line_id: id, position, book_id: book, text: text.into(), section_id: None, no_cross: false }
}

fn corpus() -> Vec<LineDoc> {
    vec![
        line(10, 0, 1, "alpha beta"),
        line(11, 1, 1, "xxxxxxxxfghyyyyyyyy gamma"),
        line(12, 2, 1, "fgh"),
        line(13, 3, 2, "zzfghzz omega"),
        line(14, 4, 2, "delta qrsfgh"),
        line(15, 5, 2, "nothing here"),
        line(16, 6, 3, "omega fghij"),
    ]
}

fn index(positions: bool, merged: bool, lines: &[LineDoc]) -> (tempfile::TempDir, LineIndex) {
    let dir = tempfile::tempdir().unwrap();
    let index = LineIndex::create(dir.path(), IndexOptions { positions, keep_marks: false }).unwrap();
    let mut w = index.writer(60_000_000, 1).unwrap();
    for l in lines {
        w.add(l).unwrap();
    }
    w.commit(None).unwrap();
    if merged {
        w.merge_all().unwrap();
        assert!(index.build_fm_sidecar().unwrap());
    } else {
        drop(w);
    }
    (dir, index)
}

fn substring(query: &str) -> SearchOptions {
    SearchOptions { query: query.into(), substring: true, decline_over_cap: true, ..Default::default() }
}

fn ids(index: &LineIndex, o: &SearchOptions) -> Option<Vec<u64>> {
    let r = index.search_options(o).unwrap();
    if r.declined() {
        assert!(r.hits.is_empty() && r.info().declined);
        return None;
    }
    Some(r.hits.iter().map(|h| h.line_id).collect())
}

#[test]
fn substring_matches_every_term_containing_the_fragment() {
    for positions in [true, false] {
        let (_d, ix) = index(positions, true, &corpus());
        // no affix budget: 8 letters before and 8 after still match
        assert_eq!(ids(&ix, &substring("fgh")), Some(vec![11, 12, 13, 14, 16]));
        // a one-letter fragment (below the wildcard anchor) is answered too
        assert_eq!(ids(&ix, &substring("q")), Some(vec![14]));
        // words AND, in any order and distance
        assert_eq!(ids(&ix, &substring("ega fgh")), Some(vec![13, 16]));
        assert_eq!(ids(&ix, &substring("fgh ega")), Some(vec![13, 16]));
        // a fragment no term contains: no line
        assert_eq!(ids(&ix, &substring("fgh nope")), Some(vec![]));
        // one whitespace word cut into two indexer terms: two groups
        assert_eq!(ids(&ix, &substring("ega-fgh")), Some(vec![13, 16]));
        // case folded as the indexer folds
        assert_eq!(ids(&ix, &substring("FGH")), Some(vec![11, 12, 13, 14, 16]));
    }
}

#[test]
fn substring_declines_like_ftslib() {
    let (_d, ix) = index(true, true, &corpus());
    // a word with no indexable term, or no word at all
    assert_eq!(ids(&ix, &substring("!!!")), None);
    assert_eq!(ids(&ix, &substring("fgh !!!")), None);
    assert_eq!(ids(&ix, &substring("   ")), None);
    // more terms than the cap: declined (5 terms hold fgh)
    let capped = |cap| SearchOptions { max_expanded_terms: cap, ..substring("fgh") };
    assert_eq!(ids(&ix, &capped(4)), None);
    assert_eq!(ids(&ix, &capped(5)), Some(vec![11, 12, 13, 14, 16]));
    // without decline_over_cap: the capped terms' hits and the truncated flag
    let r = ix.search_options(&SearchOptions { decline_over_cap: false, ..capped(4) }).unwrap();
    assert!(!r.declined() && r.truncated() && r.hits.len() == 4);
    // no FM-index (several segments, nothing merged): declined, as FtsLib
    let (_d2, raw) = index(true, false, &corpus());
    assert_eq!(ids(&raw, &substring("fgh")), None);
}

#[test]
fn decline_over_cap_applies_to_every_capped_walk() {
    let lines: Vec<LineDoc> =
        ["abq", "abqx", "abqxy", "abqxyz"].iter().enumerate().map(|(i, t)| line(i as u64, i as u64, 1, t)).collect();
    for merged in [true, false] {
        let (_d, ix) = index(true, merged, &lines);
        let o = |decline| SearchOptions { query: "abq*".into(), max_expanded_terms: 2, decline_over_cap: decline, ..Default::default() };
        let r = ix.search_options(&o(false)).unwrap();
        assert!(r.truncated() && !r.declined() && r.hits.len() == 2);
        let r = ix.search_options(&o(true)).unwrap();
        assert!(r.declined() && r.hits.is_empty());
        let r = ix.search_options(&SearchOptions { max_expanded_terms: 4, ..o(true) }).unwrap();
        assert!(!r.declined() && r.hits.len() == 4);
    }
}

#[test]
fn position_ranges_filter_every_search() {
    for positions in [true, false] {
        for merged in [true, false] {
            let (_d, ix) = index(positions, merged, &corpus());
            let word = |ranges: Option<Vec<(u64, u64)>>| SearchOptions {
                query: "omega".into(),
                position_ranges: ranges,
                ..Default::default()
            };
            assert_eq!(ids(&ix, &word(None)), Some(vec![13, 16]));
            assert_eq!(ids(&ix, &word(Some(vec![(3, 3)]))), Some(vec![13]));
            // unsorted, overlapping, an inverted range ignored
            assert_eq!(ids(&ix, &word(Some(vec![(6, 9), (2, 4), (3, 3), (5, 1)]))), Some(vec![13, 16]));
            assert_eq!(ids(&ix, &word(Some(vec![(4, 5)]))), Some(vec![]));
            assert_eq!(ids(&ix, &word(Some(vec![]))), Some(vec![]));
            // with the book filter: both must admit the line
            let both = SearchOptions { books: Some(vec![3]), ..word(Some(vec![(0, 6)])) };
            assert_eq!(ids(&ix, &both), Some(vec![16]));
            // with a resume bound
            let after = SearchOptions { after_position: Some(3), ..word(Some(vec![(0, 6)])) };
            assert_eq!(ids(&ix, &after), Some(vec![16]));
            // counts follow the scope
            let r = ix.search_options(&word(Some(vec![(0, 3)]))).unwrap();
            assert_eq!(r.counts().total, 1);
            if merged {
                let s = SearchOptions { position_ranges: Some(vec![(0, 2), (6, 6)]), ..substring("fgh") };
                assert_eq!(ids(&ix, &s), Some(vec![11, 12, 16]));
            }
        }
    }
}

#[test]
fn position_ranges_filter_a_line_regex_and_cross_line() {
    let (_d, ix) = index(true, true, &corpus());
    let o = SearchOptions {
        query: "/^.*fgh.*$/".into(),
        user_regex: true,
        position_ranges: Some(vec![(2, 3)]),
        ..Default::default()
    };
    let mut r = ix.search_options(&o).unwrap();
    // a line regex's lines are fed by the host; check the scope only
    assert!(r.line.is_some() || r.hits.iter().all(|h| (2..=3).contains(&h.position)));
    let _ = r.pull(usize::MAX, None);
    let cross = SearchOptions {
        query: "gamma fgh".into(),
        cross_line: true,
        require_ordered: true,
        position_ranges: Some(vec![(2, 6)]),
        ..Default::default()
    };
    // the pair (1, 2) starts at position 1: out of scope
    assert_eq!(ids(&ix, &cross), Some(vec![]));
    let cross_in = SearchOptions { position_ranges: Some(vec![(1, 1)]), ..cross };
    assert_eq!(ids(&ix, &cross_in), Some(vec![11]));
}

#[test]
fn a_quoted_twin_counts_once_against_the_cap() {
    // FtsLib's dictionary is quote-free: `ab"q` and `abq` are one term there
    let lines = [line(1, 0, 1, "abq"), line(2, 1, 1, "ab\"q"), line(3, 2, 1, "xbqx")];
    let (_d, ix) = index(true, true, &lines);
    let o = |cap| SearchOptions { max_expanded_terms: cap, ..substring("bq") };
    assert_eq!(ids(&ix, &o(2)), Some(vec![1, 2, 3]));
    assert_eq!(ids(&ix, &o(1)), None);
}
