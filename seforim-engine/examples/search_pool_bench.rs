//! Issue #24 micro benchmark: the per-batch text work (line regex feed,
//! full scan, count, cross-line pairs) at several batch sizes, short and
//! long lines, on a synthetic in-temp-dir index.
//!
//!     cargo run --release -p seforim-engine --example search_pool_bench -- [rounds]
//!
//! Prints, per row, the best of `rounds` (alternated across rows) in ms and
//! the batch's text bytes. Run it once per build to compare the sequential
//! path, the per-call scoped threads and the persistent search pool (the
//! cut-offs in `pools::PARALLEL_MIN_BYTES` and
//! `pools::LINE_REGEX_PARALLEL_MIN_BYTES` decide which one a batch takes).

use std::time::Instant;

use seforim_engine::{IndexOptions, LineDoc, LineIndex, PlacedHit, ProximitySpec, SearchOptions};

const W: [&str; 8] = [
    "\u{05d0}\u{05dc}\u{05e4}\u{05d4}\u{05d0}",
    "\u{05d2}\u{05de}\u{05dc}\u{05d4}\u{05d0}",
    "\u{05d1}\u{05d9}\u{05ea}\u{05d0}",
    "\u{05d3}\u{05dc}\u{05ea}\u{05d0}\u{05d4}",
    "\u{05e9}\u{05dc}\u{05d5}\u{05dd}",
    "\u{05de}\u{05e9}\u{05d4}",
    "\u{05ea}\u{05d5}\u{05e8}\u{05d4}",
    "\u{05d9}\u{05e6}\u{05d7}\u{05e7}",
];

fn corpus(lines: usize, words: usize) -> Vec<String> {
    let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = |n: usize| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        (s % n as u64) as usize
    };
    (0..lines)
        .map(|_| {
            let mut v: Vec<&str> = (0..words).map(|_| W[2 + next(6)]).collect();
            // most lines hold W0 ... W1 (candidates and hits for every row)
            if next(8) != 0 {
                let a = next(words / 2);
                v[a] = W[0];
                v[a + 1 + next(2)] = W[1];
            }
            v.join(" ")
        })
        .collect()
}

fn build(texts: &[String]) -> (tempfile::TempDir, LineIndex) {
    let dir = tempfile::tempdir().unwrap();
    let index = LineIndex::create(dir.path(), IndexOptions { positions: false, keep_marks: false }).unwrap();
    let mut w = index.writer(100_000_000, 1).unwrap();
    for (i, t) in texts.iter().enumerate() {
        w.add(&LineDoc { line_id: i as u64, position: i as u64, book_id: 1, text: t.clone(), section_id: None, no_cross: false })
            .unwrap();
    }
    w.commit(None).unwrap();
    (dir, index)
}

struct Case {
    name: String,
    bytes: usize,
    run: Box<dyn Fn() -> f64>,
    best: f64,
}

fn cases(label: &str, texts: &'static [String], index: &'static LineIndex, sizes: &[usize]) -> Vec<Case> {
    let mut out = Vec::new();
    let bytes = |n: usize| texts[..n].iter().map(String::len).sum::<usize>();
    let cand = SearchOptions { query: format!("/{} .* {}/", W[0], W[1]), user_regex: true, ..Default::default() };
    let scan = SearchOptions { query: "/\\w[ ]\\w/".into(), user_regex: true, ..Default::default() };
    let near = SearchOptions {
        query: format!("{} {}", W[0], W[1]),
        proximity: Some(ProximitySpec::AnyOrder { distance: 2 }),
        ..Default::default()
    };
    let pairs = SearchOptions { cross_line: true, ..near.clone() };
    for &n in sizes {
        let o = cand.clone();
        out.push(Case {
            name: format!("{label} regex feed {n}"),
            bytes: 0,
            run: Box::new(move || {
                let mut r = index.search_options(&o).unwrap();
                assert!(!r.line_regex().unwrap().full_scan && r.hits.len() >= n);
                let t: Vec<&str> = r.hits[..n].iter().map(|h| texts[h.line_id as usize].as_str()).collect();
                let t0 = Instant::now();
                std::hint::black_box(r.line_match_batch(&t).unwrap());
                t0.elapsed().as_secs_f64() * 1e3
            }),
            best: f64::MAX,
        });
        let o = scan.clone();
        out.push(Case {
            name: format!("{label} full scan {n}"),
            bytes: bytes(n),
            run: Box::new(move || {
                let mut r = index.search_options(&o).unwrap();
                assert!(r.line_regex().unwrap().full_scan);
                let lines: Vec<PlacedHit> = (0..n as u64)
                    .map(|i| PlacedHit { position: i, line_id: i, book_id: 1, next_line_id: None })
                    .collect();
                let t: Vec<&str> = texts[..n].iter().map(String::as_str).collect();
                let t0 = Instant::now();
                std::hint::black_box(r.line_scan_batch(&lines, &t).unwrap());
                t0.elapsed().as_secs_f64() * 1e3
            }),
            best: f64::MAX,
        });
        let o = near.clone();
        out.push(Case {
            name: format!("{label} count {n}"),
            bytes: 0,
            run: Box::new(move || {
                let mut r = index.search_options(&o).unwrap();
                assert!(r.count_needs_text() && r.hits.len() >= n);
                let t: Vec<&str> = r.hits[..n].iter().map(|h| texts[h.line_id as usize].as_str()).collect();
                let t0 = Instant::now();
                std::hint::black_box(r.count_batch(&t).unwrap());
                t0.elapsed().as_secs_f64() * 1e3
            }),
            best: f64::MAX,
        });
        let o = pairs.clone();
        out.push(Case {
            name: format!("{label} cross-line pairs {n}"),
            bytes: 0,
            run: Box::new(move || {
                let mut r = index.search_options(&o).unwrap();
                assert!(r.count_needs_text() && r.hits.len() >= n);
                let t: Vec<(&str, Option<&str>)> = r.hits[..n]
                    .iter()
                    .map(|h| {
                        let next = h.next_line_id.or(Some(h.line_id + 1)).filter(|&i| (i as usize) < texts.len());
                        (texts[h.line_id as usize].as_str(), next.map(|i| texts[i as usize].as_str()))
                    })
                    .collect();
                let t0 = Instant::now();
                std::hint::black_box(r.count_batch_pairs(&t).unwrap());
                t0.elapsed().as_secs_f64() * 1e3
            }),
            best: f64::MAX,
        });
        // the candidate-based rows feed the candidates' texts: about the same bytes
        let b = bytes(n);
        for c in out.iter_mut().rev().take(4) {
            c.bytes = b;
        }
    }
    out
}

fn main() {
    let rounds: usize = std::env::args().nth(1).and_then(|a| a.parse().ok()).unwrap_or(15);
    let sizes = [16, 32, 64, 128, 256, 512, 1024, 2048, 4096];
    let short: &'static [String] = Box::leak(corpus(12_000, 10).into_boxed_slice());
    let long: &'static [String] = Box::leak(corpus(5_000, 120).into_boxed_slice());
    let (_ds, si) = build(short);
    let (_dl, li) = build(long);
    let si: &'static LineIndex = Box::leak(Box::new(si));
    let li: &'static LineIndex = Box::leak(Box::new(li));
    let mut all = cases("short", short, si, &sizes);
    all.extend(cases("long", long, li, &sizes));
    for c in &all {
        (c.run)(); // warm-up (pools, caches)
    }
    for _ in 0..rounds {
        for c in all.iter_mut() {
            c.best = c.best.min((c.run)());
        }
    }
    println!("row,bytes,best_ms");
    for c in &all {
        println!("{},{},{:.3}", c.name, c.bytes, c.best);
    }
}
