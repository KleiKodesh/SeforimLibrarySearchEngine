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
    println!("(sink {sink})");
}
