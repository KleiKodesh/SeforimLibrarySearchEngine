//! MatchMode::AllMatches and find_all_matches (issues #14 / #23). Mirrors
//! FtsLib's FtsLibTest `allmatches`.

use hebrew_fts::{MatchMode, PreparedQueryGroups, Proximity, SnippetWindowLocator};

const A: &str = "\u{05d0}\u{05d1}";
const B: &str = "\u{05d2}\u{05d3}";
const C: &str = "\u{05d4}\u{05d5}";

fn starts(m: &[usize]) -> Vec<usize> {
    m.chunks(3).map(|c| c[0]).collect()
}
fn ords(m: &[usize]) -> Vec<usize> {
    m.chunks(3).map(|c| c[2]).collect()
}
fn find(text: &str, words: &[&str], phrase: bool, quotes: bool) -> Vec<usize> {
    let p = PreparedQueryGroups::from_text_words(words, false, quotes);
    SnippetWindowLocator::new(8).find_all_matches(text, &p, phrase, quotes).to_vec()
}
/// UTF-16 starts of the space-separated words of `line` equal to one of `words`.
fn occ(line: &str, words: &[&str]) -> Vec<usize> {
    let mut at = 0;
    let mut out = Vec::new();
    for w in line.split(' ') {
        if words.contains(&w) {
            out.push(at);
        }
        at += w.encode_utf16().count() + 1;
    }
    out
}

#[test]
fn all_matches_locate_returns_the_whole_text() {
    let line = format!("{A} x x x x x x x x x x x x {B} {A} {B} y {A}");
    let prepared = PreparedQueryGroups::from_groups(&[vec![A], vec![B]]);
    let mut l = SnippetWindowLocator::new(1);
    let best = l.locate(&line, &prepared, false, 0);
    let (bs, be, bm, bscore, bdist) =
        (best.snap_start, best.snap_end, best.mark_ranges.to_vec(), best.score, best.word_distance);
    l.set_match_mode(MatchMode::AllMatches);
    let all = l.locate(&line, &prepared, false, 0);
    assert!(all.is_match);
    assert_eq!((all.snap_start, all.snap_end), (0, line.encode_utf16().count()));
    assert!(!all.leading_ellipsis && !all.trailing_ellipsis);
    assert_eq!(starts(all.mark_ranges), occ(&line, &[A, B]));
    assert_eq!((all.score, all.word_distance), (bscore, bdist));
    assert!(!l.locate(&format!("{A} only"), &prepared, false, 0).is_match);
    // back to the default: byte-identical
    l.set_match_mode(MatchMode::BestWindow);
    let again = l.locate(&line, &prepared, false, 0);
    assert_eq!((again.snap_start, again.snap_end, again.mark_ranges.to_vec()), (bs, be, bm));
}

#[test]
fn all_matches_proximity() {
    let line = format!("{A} {B} z z z z z z z z {A} {B} {A}");
    let prepared = PreparedQueryGroups::from_groups(&[vec![A], vec![B]]);
    let mut l = SnippetWindowLocator::new(0);
    l.set_match_mode(MatchMode::AllMatches);
    l.tokenize(&line);
    let d = l.locate_proximity_tokenized(&prepared, &Proximity::Ordered { gaps: vec![0] });
    assert!(d.is_match);
    assert_eq!(starts(d.mark_ranges), occ(&line, &[A, B]));
}

#[test]
fn all_matches_find_words_phrase_html_entities() {
    let t1 = format!("{A} {B} {A} <b>{B}</b> {A}");
    let m = find(&t1, &[A], false, false);
    assert_eq!(starts(&m), vec![0, 6, 19]);
    assert_eq!((m[1], m[2]), (2, 0));
    let m = find(&t1, &[A, B], false, false);
    assert_eq!(starts(&m), vec![0, 3, 6, 12, 19]);
    assert_eq!(ords(&m), vec![0, 1, 0, 1, 0]);
    let m = find(&t1, &[&format!("{A} {B}")], true, false);
    assert_eq!(starts(&m), vec![0, 3, 6, 12]);
    // overlapping / repeated phrase
    let m = find(&format!("{A} {A} {A} {C}"), &[A, A], true, false);
    assert_eq!(starts(&m), vec![0, 3, 6]);
    // a gap breaks the phrase; a paired reading's second reading shares its position
    assert!(find(&format!("{A} x {B}"), &[&format!("{A} {B}")], true, false).is_empty());
    let m = find(&format!("({A}) [{C}] {B}"), &[&format!("{C} {B}")], true, false);
    assert_eq!(ords(&m), vec![0, 1]);
    assert!(find(&t1, &[] as &[&str], false, false).is_empty());
    let m = find(&format!("{A}&nbsp;{B}"), &[B], false, false);
    assert_eq!(starts(&m), vec![8]);
}

#[test]
fn all_matches_marks_quotes_utf16() {
    // nikud stripped on both sides by default
    let m = find("\u{05d0}\u{05b8}\u{05d1} x", &[A], false, false);
    assert_eq!((starts(&m), m[1]), (vec![0], 3));
    // marks kept: the vocalized query finds only the vocalized word
    let voc = "\u{05d0}\u{05b8}\u{05d1}";
    let p = PreparedQueryGroups::from_text_words(&[voc], true, false);
    let mut l = SnippetWindowLocator::keeping_marks(8);
    assert_eq!(starts(l.find_all_matches(&format!("{A} {voc}"), &p, false, false)), vec![3]);
    let q = "\u{05d0}\u{05d1}\"\u{05d2}";
    let plain = "\u{05d0}\u{05d1}\u{05d2}";
    let quoted = find(&format!("x {q}"), &[plain], false, false);
    let ignored = find(&format!("x {q}"), &[plain], false, true);
    assert_eq!((starts(&ignored), ignored[1]), (vec![2], 4));
    // without the option the quote is part of the token (Otzaria's terms keep it)
    assert!(quoted.is_empty());
    // UTF-16 offsets: an astral char before the word counts 2 units
    let m = find(&format!("\u{1F600} {A}"), &[A], false, false);
    assert_eq!(starts(&m), vec![3]);
}
