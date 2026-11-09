//! FM sidecar == walk (issue #28): every query shape the sidecar serves
//! gives the walk's hits, matched terms and truncated flag, on indexes with
//! and without marks, for all 8 match_nikud / match_teamim / ignore_quotes
//! combinations, astral chars included; a corrupt or stale sidecar falls
//! back to the walk with a notice; cancel stops an FM expansion.
//! Test data is built from `\u` escapes only.

use std::sync::atomic::Ordering;

use crate::index::{IndexOptions, LineDoc, LineIndex};
use crate::options::SearchOptions;
use crate::{CancelToken, Error, NOTICE_FM_SIDECAR_UNUSABLE};

fn line(id: u64, text: &str) -> LineDoc {
    LineDoc { line_id: id, position: id, book_id: 1, text: text.into(), section_id: None, no_cross: false }
}

const LETTERS: [char; 22] = [
    '\u{05D0}', '\u{05D1}', '\u{05D2}', '\u{05D3}', '\u{05D4}', '\u{05D5}', '\u{05D6}', '\u{05D7}', '\u{05D8}',
    '\u{05D9}', '\u{05DB}', '\u{05DC}', '\u{05DE}', '\u{05E0}', '\u{05E1}', '\u{05E2}', '\u{05E4}', '\u{05E6}',
    '\u{05E7}', '\u{05E8}', '\u{05E9}', '\u{05EA}',
];
const NIK: char = '\u{05B8}';
const NIK2: char = '\u{05B4}';
const TAAM: char = '\u{0591}';
const TAAM2: char = '\u{05A5}';
const ALEF: char = '\u{05D0}';
const BET: char = '\u{05D1}';
const SHIN: char = '\u{05E9}';
const LAMED: char = '\u{05DC}';
/// MATHEMATICAL BOLD CAPITAL A: a letter outside the BMP.
const ASTRAL: char = '\u{1D400}';

/// A word with nikud, cantillation, an ASCII or a U+05F4 quote, by variant.
fn variant(w: &[char], v: usize) -> String {
    let mut s = String::new();
    for (i, &c) in w.iter().enumerate() {
        if (v == 3 || v == 6) && i == w.len() - 1 {
            s.push(if v == 6 { '\u{05F4}' } else { '"' });
        }
        s.push(c);
        match (v, i) {
            (1, 0) | (3, 0) | (5, 2) => s.push(NIK),
            (2, 1) | (5, 3) => s.push(TAAM),
            (4, 0) => {
                s.push(NIK2);
                s.push(TAAM2);
            }
            _ => {}
        }
    }
    s
}

fn index_over(lines: &[LineDoc], keep_marks: bool) -> (tempfile::TempDir, LineIndex) {
    let dir = tempfile::tempdir().unwrap();
    let index = LineIndex::create(dir.path(), IndexOptions { keep_marks, ..Default::default() }).unwrap();
    let mut w = index.writer(60_000_000, 4).unwrap();
    for l in lines {
        w.add(l).unwrap();
    }
    w.commit(None).unwrap();
    w.merge_all().unwrap();
    (dir, index)
}

/// About 1000 five-letter words around one core, in every variant.
fn corpus() -> Vec<LineDoc> {
    let mut lines = Vec::new();
    let mut n = 0u64;
    for i in 0..22 {
        for j in 0..22 {
            for k in (0..22).step_by(5) {
                let w = [LETTERS[i], LETTERS[j], SHIN, LAMED, LETTERS[k]];
                let v = (i + 2 * j + k) % 7;
                for vv in [0, v] {
                    lines.push(line(n, &variant(&w, vv)));
                    n += 1;
                }
                if (i + j) % 5 == 0 {
                    lines.push(line(n, &variant(&w, (v + 3) % 7)));
                    n += 1;
                }
            }
        }
    }
    lines
}

/// Hits, matched terms (from the located marks) and the truncated flag.
type Outcome = (Vec<u64>, Vec<String>, bool);

fn run(index: &LineIndex, lines: &[LineDoc], o: &SearchOptions, disable_fm: bool) -> Outcome {
    index.test_only_disable_fm.store(disable_fm, Ordering::Relaxed);
    let r = index.search_options(o);
    index.test_only_disable_fm.store(false, Ordering::Relaxed);
    let mut r = match r {
        Ok(r) => r,
        Err(e) => return (vec![], vec![format!("ERR {e}")], false),
    };
    let ids: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
    let mut terms = Vec::new();
    for h in &r.hits {
        let text = &lines.iter().find(|l| l.line_id == h.line_id).unwrap().text;
        if let Some(s) = r.session.locate(text) {
            for m in s.marks.chunks_exact(3) {
                terms.push(r.session.all_terms()[m[2] as usize].clone());
            }
        }
    }
    terms.sort();
    terms.dedup();
    (ids, terms, r.truncated())
}

fn s(v: &[char]) -> String {
    v.iter().collect()
}

#[test]
fn fm_equals_walk_on_marks_and_quotes() {
    let g = ALEF;
    let queries: Vec<String> = vec![
        format!("*{}", s(&[SHIN, LAMED])),
        format!("*{}*", s(&[SHIN, LAMED])),
        format!("*{}", s(&[SHIN, LAMED, g])),
        format!("*{}*", s(&[SHIN, NIK, LAMED])),
        format!("*{}", s(&[SHIN, TAAM, LAMED, g])),
        format!("*{}", s(&[SHIN, LAMED, '"', g])),
        format!("*{}", s(&[BET, SHIN, LAMED, '"', g])),
        format!("{}~1", s(&[ALEF, BET, SHIN, LAMED, g])),
        format!("{}~2", s(&[ALEF, BET, SHIN, LAMED, g])),
        format!("{}~1", s(&[ALEF, NIK, BET, SHIN, LAMED, g])),
        format!("{}~2", s(&[ALEF, NIK, BET, TAAM, SHIN, LAMED, g])),
        format!("{}~1", s(&[ALEF, BET, SHIN, LAMED, '"', g])),
        format!("{}~2", s(&[ALEF, BET, SHIN, LAMED, '"', g])),
        format!("{}~1", s(&[BET, ALEF, SHIN, LAMED, g])),
    ];
    let lines = corpus();
    let (mut cases, mut capped) = (0, 0);
    for keep_marks in [true, false] {
        let (_dir, index) = index_over(&lines, keep_marks);
        assert!(index.build_fm_sidecar().unwrap());
        assert!(index.has_fm_sidecar());
        for (qi, q) in queries.iter().enumerate() {
            for mask in 0..8u8 {
                let mut o = SearchOptions {
                    query: q.clone(),
                    match_nikud: mask & 1 != 0,
                    match_teamim: mask & 2 != 0,
                    ignore_quotes: mask & 4 != 0,
                    ..Default::default()
                };
                let fm = run(&index, &lines, &o, false);
                let walk = run(&index, &lines, &o, true);
                assert_eq!(fm, walk, "keep_marks={keep_marks} query #{qi} mask={mask}");
                cases += 1;
                // the cap hit: a tiny cap keeps the same closest terms on both paths
                o.max_expanded_terms = 3;
                let fm = run(&index, &lines, &o, false);
                let walk = run(&index, &lines, &o, true);
                assert_eq!(fm, walk, "capped: keep_marks={keep_marks} query #{qi} mask={mask}");
                capped += usize::from(fm.2);
            }
        }
    }
    assert_eq!(cases, 224);
    assert!(capped > 0, "the cap was never hit");
}

#[test]
fn astral_chars_are_fm_indexed() {
    let mut lines = Vec::new();
    for i in 0..50u64 {
        lines.push(line(i, &s(&[ALEF, SHIN, LAMED, LETTERS[(i % 22) as usize]])));
    }
    lines.push(line(50, &s(&[ALEF, ASTRAL, SHIN, LAMED])));
    lines.push(line(51, &s(&[BET, SHIN, LAMED, ASTRAL])));
    lines.push(line(52, &s(&[ASTRAL, ALEF, BET, SHIN])));
    for keep_marks in [false, true] {
        let (_dir, index) = index_over(&lines, keep_marks);
        // one astral term no longer drops the sidecar
        assert!(index.build_fm_sidecar().unwrap());
        assert!(index.has_fm_sidecar());
        let queries = [
            format!("*{}", s(&[SHIN, LAMED])),
            format!("*{}*", s(&[ASTRAL, SHIN])),
            format!("*{}", s(&[LAMED, ASTRAL])),
            format!("*{}*", s(&[ALEF, BET])),
            format!("{}~1", s(&[ALEF, SHIN, LAMED, BET])),
            format!("{}~1", s(&[ALEF, ASTRAL, SHIN, LAMED])),
            format!("{}~1", s(&[ASTRAL, BET, SHIN, LAMED])),
            format!("{}~2", s(&[ASTRAL, ALEF, SHIN])),
        ];
        for q in &queries {
            for mask in 0..8u8 {
                let o = SearchOptions {
                    query: q.clone(),
                    match_nikud: mask & 1 != 0,
                    match_teamim: mask & 2 != 0,
                    ignore_quotes: mask & 4 != 0,
                    ..Default::default()
                };
                assert_eq!(run(&index, &lines, &o, false), run(&index, &lines, &o, true), "mask={mask}");
            }
        }
        // the astral terms are found through the sidecar
        let o = SearchOptions { query: format!("*{}*", s(&[ASTRAL, SHIN])), ..Default::default() };
        assert_eq!(run(&index, &lines, &o, false).0, vec![50]);
        let o = SearchOptions { query: format!("*{}", s(&[LAMED, ASTRAL])), ..Default::default() };
        assert_eq!(run(&index, &lines, &o, false).0, vec![51]);
    }
}

fn fmi_path(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "fmi"))
        .expect("a sidecar")
}

#[test]
fn unusable_sidecar_falls_back_to_the_walk_with_a_notice() {
    let lines = corpus();
    let (dir, index) = index_over(&lines, false);
    assert!(index.build_fm_sidecar().unwrap());
    let queries = [format!("*{}*", s(&[SHIN, LAMED])), format!("{}~1", s(&[ALEF, BET, SHIN, LAMED, ALEF]))];
    let expected: Vec<Outcome> = queries
        .iter()
        .map(|q| run(&index, &lines, &SearchOptions { query: q.clone(), ..Default::default() }, true))
        .collect();
    let notices = |index: &LineIndex| {
        index.search_options(&SearchOptions { query: queries[0].clone(), ..Default::default() }).unwrap().notices().to_vec()
    };
    assert!(notices(&index).is_empty());
    let path = fmi_path(dir.path());
    // the open index maps the sidecar (Windows cannot replace a mapped file)
    drop(index);

    // corrupt (garbage), truncated, and stale (built over another dictionary)
    let good = std::fs::read(&path).unwrap();
    let stale = crate::fm_index::build([s(&[ALEF, BET]).as_str()]).unwrap();
    for bad in [vec![7u8; 100], good[..good.len() / 2].to_vec(), stale] {
        // a fresh index object: no cached sidecar
        let index = LineIndex::open(dir.path()).unwrap();
        std::fs::write(&path, &bad).unwrap();
        assert!(!index.has_fm_sidecar());
        for (q, want) in queries.iter().zip(&expected) {
            let o = SearchOptions { query: q.clone(), ..Default::default() };
            assert_eq!(&run(&index, &lines, &o, false), want);
        }
        assert_eq!(notices(&index), [NOTICE_FM_SIDECAR_UNUSABLE.to_string()]);
        let reason = index.fm_sidecar_problem().expect("a reason");
        assert!(reason.contains(".fmi"), "{reason}");
        // remembered: the same broken file gives the cached reason
        assert_eq!(index.fm_sidecar_problem().as_deref(), Some(reason.as_str()));
        // rebuilding repairs it, and the notice goes
        assert!(index.build_fm_sidecar().unwrap());
        assert!(index.has_fm_sidecar());
        assert!(notices(&index).is_empty());
        assert_eq!(index.fm_sidecar_problem(), None);
    }
}

#[test]
fn a_broken_sidecar_is_remembered_until_the_file_changes() {
    let lines = corpus();
    let (dir, index) = index_over(&lines, false);
    assert!(index.build_fm_sidecar().unwrap());
    let path = fmi_path(dir.path());
    drop(index);
    let index = LineIndex::open(dir.path()).unwrap();
    std::fs::write(&path, vec![7u8; 100]).unwrap();
    let first = index.fm_sidecar_problem().expect("unusable");
    assert!(first.contains("bad magic"), "{first}");
    // the cache answers while the file keeps its identity
    let searcher = index.reader.searcher();
    assert!(matches!(
        index.fm.lookup(&index.dir, &searcher, index.fields.text),
        crate::fm_sidecar::FmLookup::Unusable(r) if r == first
    ));
    // another broken content (other size): re-checked, a new reason
    let mut bytes = vec![0u8; 50];
    bytes[..4].copy_from_slice(&crate::fm_index::MAGIC.to_le_bytes());
    std::fs::write(&path, &bytes).unwrap();
    let second = index.fm_sidecar_problem().expect("unusable");
    assert_ne!(second, first);
    // the file removed: missing, no problem
    std::fs::remove_file(&path).unwrap();
    assert_eq!(index.fm_sidecar_problem(), None);
    assert!(!index.has_fm_sidecar());
}

#[test]
fn cancel_stops_an_fm_expansion() {
    let lines = corpus();
    let (_dir, index) = index_over(&lines, false);
    assert!(index.build_fm_sidecar().unwrap());
    let searcher = index.reader.searcher();
    let fm = index.fm.get(&index.dir, &searcher, index.fields.text).unwrap();
    let inverted = searcher.segment_readers()[0].inverted_index(index.fields.text).unwrap();
    let token = CancelToken::new();
    token.cancel();
    let core = s(&[SHIN, LAMED]);
    let q = hebrew_fts::wildcard::HebrewWildcardQuery::new(index.fields.text, &format!("*{core}*")).unwrap();
    let r = crate::fm_expand::expand_wildcard(&fm, inverted.terms(), &q.automaton(), &core, false, 2000, false, Some(&token));
    assert!(matches!(r, Err(Error::Cancelled)));
    let word = s(&[ALEF, BET, SHIN, LAMED, ALEF]);
    let r = crate::fm_expand::expand_fuzzy_quote_free(&fm, inverted.terms(), &word, 1, 2000, false, Some(&token));
    assert!(matches!(r, Err(Error::Cancelled)));
    // uncancelled, the same expansion runs
    let r = crate::fm_expand::expand_wildcard(&fm, inverted.terms(), &q.automaton(), &core, false, 2000, false, None);
    assert!(!r.unwrap().0.is_empty());
    // and a search cancelled up front stops on the FM path too
    let o = SearchOptions { query: format!("*{core}*"), ..Default::default() };
    assert!(matches!(index.search_options_cancellable(&o, Some(token)), Err(Error::Cancelled)));
}

