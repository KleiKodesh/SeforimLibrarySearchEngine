//! Single-thread throughput of the snippet path, split by stage, on
//! realistic seforim.db-like lines (HTML tags, nikud, quotes, entities).
//!
//! ```text
//! cargo run --release -p hebrew-fts --example locate_bench [lines_file]
//! ```
//! `lines_file`: optional UTF-8 file, one line text per line (real texts);
//! without it a synthetic ~2.2 KB line is used.

use std::time::Instant;

use hebrew_fts::{scan_with, PreparedQueryGroups, Proximity, SnippetWindowLocator, TextTokens};

fn synthetic_line() -> String {
    // ~2.2 KB: vocalised words, inline and block tags, an entity, quotes.
    let chunk = "<b>וַיֹּאמֶר</b> משה אל העם, אמר רש\"י: <i>הַדָּבָר</i> הזה&nbsp;נאמר לָהֶם בְּסִינַי וכו' ";
    let mut s = String::new();
    while s.len() < 2200 {
        s.push_str(chunk);
    }
    s
}

fn main() {
    let lines: Vec<String> = match std::env::args().nth(1) {
        Some(path) => std::fs::read_to_string(path).expect("read lines").lines().map(str::to_string).collect(),
        None => (0..2000).map(|_| synthetic_line()).collect(),
    };
    let bytes: usize = lines.iter().map(|l| l.len()).sum();
    let mb = bytes as f64 / 1e6;
    let rounds = (200.0 / mb).ceil().max(1.0) as usize; // ~200 MB per stage
    println!("{} lines, {:.1} MB, {} rounds", lines.len(), mb, rounds);

    let rate = |label: &str, f: &mut dyn FnMut()| {
        let t = Instant::now();
        for _ in 0..rounds {
            f();
        }
        let s = t.elapsed().as_secs_f64();
        println!("{label:<40} {:8.1} MB/s  ({:.2} µs/line)", mb * rounds as f64 / s,
                 s * 1e6 / (rounds * lines.len()) as f64);
    };

    // 1. raw byte scan (lower bound: touching every byte)
    let mut sink = 0u64;
    rate("baseline: sum bytes", &mut || {
        for l in &lines {
            sink = sink.wrapping_add(l.bytes().map(u64::from).sum::<u64>());
        }
    });
    // 2. scanner only (word callback does nothing)
    rate("scan_with (scanner only)", &mut || {
        for l in &lines {
            let n = scan_with(l, false, |_w, _s| {});
            sink = sink.wrapping_add(n as u64);
        }
    });
    // 3. TextTokens::tokenize (scanner + token records)
    let mut tt = TextTokens::default();
    rate("TextTokens::tokenize", &mut || {
        for l in &lines {
            sink = sink.wrapping_add(tt.tokenize(l).len() as u64);
        }
    });
    // 4. tokenize + prepared lookup per token
    let prepared = PreparedQueryGroups::from_groups(&[vec!["אמר"], vec!["משה"]]);
    rate("tokenize + prepared.get per token", &mut || {
        for l in &lines {
            let toks = tt.tokenize(l).to_vec();
            for t in &toks {
                if prepared.get(tt.norm_of(t)).is_some() {
                    sink += 1;
                }
            }
        }
    });
    // 5. full locate (window + expansion + marks)
    let mut loc = SnippetWindowLocator::new(8);
    rate("SnippetWindowLocator::locate (full)", &mut || {
        for l in &lines {
            let d = loc.locate(l, &prepared, false, 0);
            sink = sink.wrapping_add(d.score as u64);
        }
    });
    // 5b. FtsLib's distance filter: marks only of covering windows within it (issue #4)
    rate("tokenize + locate_tokenized_within (d=8)", &mut || {
        for l in &lines {
            loc.tokenize(l);
            sink = sink.wrapping_add(loc.locate_tokenized_within(&prepared, &[], false, 0, 8).score as u64);
        }
    });
    // 6. Otzaria proximity on the same tokens (ordered per-pair gaps / any order)
    let ordered = Proximity::Ordered { gaps: vec![2] };
    rate("tokenize + locate_proximity (ordered)", &mut || {
        for l in &lines {
            loc.tokenize(l);
            sink = sink.wrapping_add(loc.locate_proximity_tokenized(&prepared, &ordered).score as u64);
        }
    });
    let any = Proximity::AnyOrder { max_span: 3 };
    rate("tokenize + locate_proximity (any order)", &mut || {
        for l in &lines {
            loc.tokenize(l);
            sink = sink.wrapping_add(loc.locate_proximity_tokenized(&prepared, &any).score as u64);
        }
    });
    // 7. locate only, on tokens scanned once per line (the marks' share)
    let reps = 20;
    let only = |label: &str, loc: &mut SnippetWindowLocator, sink: &mut u64, f: &dyn Fn(&mut SnippetWindowLocator) -> usize| {
        let t = Instant::now();
        for _ in 0..rounds.div_ceil(reps) {
            for l in &lines {
                loc.tokenize(l);
                for _ in 0..reps {
                    *sink = sink.wrapping_add(f(loc) as u64);
                }
            }
        }
        let s = t.elapsed().as_secs_f64();
        println!("{label:<40} {:8.3} µs/locate", s * 1e6 / (rounds.div_ceil(reps) * reps * lines.len()) as f64);
    };
    only("locate only: ftslib", &mut loc, &mut sink, &|l| l.locate_tokenized(&prepared, false, 0).mark_ranges.len());
    only("locate only: ftslib d=8", &mut loc, &mut sink, &|l| {
        l.locate_tokenized_within(&prepared, &[], false, 0, 8).mark_ranges.len()
    });
    only("locate only: ftslib in order", &mut loc, &mut sink, &|l| l.locate_tokenized(&prepared, true, 0).mark_ranges.len());
    only("locate only: ftslib in order d=8", &mut loc, &mut sink, &|l| {
        l.locate_tokenized_within(&prepared, &[], true, 0, 8).mark_ranges.len()
    });
    only("locate only: ordered", &mut loc, &mut sink, &|l| l.locate_proximity_tokenized(&prepared, &ordered).mark_ranges.len());
    only("locate only: any order", &mut loc, &mut sink, &|l| l.locate_proximity_tokenized(&prepared, &any).mark_ranges.len());
    println!("(sink {sink})");
}
