//! Find in an open text (issues #14 / #23): `find_all_matches` on a ~1 Mi
//! UTF-16 book-like text (vocalized words, inline and block tags, entities,
//! quotes), one call per page as a host would make it.
//!
//! ```text
//! cargo run --release -p hebrew-fts --example find_all_bench [out_text_file]
//! ```
//! `out_text_file`: also writes the text (UTF-8) there, for the C# / Dart
//! benchmarks to read the same page.

use std::time::Instant;

use hebrew_fts::{PreparedQueryGroups, SnippetWindowLocator};

/// Words of the page (escapes: plain and vocalized forms, a quoted acronym).
const WORDS: &[&str] = &[
    "\u{05de}\u{05e9}\u{05d4}",                                         // the searched word
    "\u{05de}\u{05b9}\u{05e9}\u{05b6}\u{05c1}\u{05d4}",                 // its vocalized form
    "\u{05d5}\u{05b7}\u{05d9}\u{05b9}\u{05bc}\u{05d0}\u{05de}\u{05b6}\u{05e8}",
    "\u{05d0}\u{05dc}",
    "\u{05d4}\u{05e2}\u{05dd}",
    "\u{05e8}\u{05e9}\"\u{05d9}",
    "\u{05d4}\u{05b7}\u{05d3}\u{05b8}\u{05bc}\u{05d1}\u{05b8}\u{05e8}",
    "\u{05d4}\u{05d6}\u{05d4}",
    "\u{05e0}\u{05d0}\u{05de}\u{05e8}",
    "\u{05dc}\u{05b8}\u{05d4}\u{05b6}\u{05dd}",
    "\u{05d1}\u{05b0}\u{05bc}\u{05e1}\u{05b4}\u{05d9}\u{05e0}\u{05b7}\u{05d9}",
    "\u{05d5}\u{05db}\u{05d5}'",
    "\u{05d0}\u{05de}\u{05e8}",
    "\u{05db}\u{05ea}\u{05d9}\u{05d1}",
    "\u{05d4}\u{05ea}\u{05d5}\u{05e8}\u{05d4}",
    "\u{05dc}\u{05de}\u{05e9}\u{05d4}",
];

/// A deterministic page of about `units` UTF-16 units.
pub fn page(units: usize) -> String {
    let mut s = String::new();
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut n16 = 0;
    let mut i = 0usize;
    while n16 < units {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let w = WORDS[(x % WORDS.len() as u64) as usize];
        let piece = match i % 23 {
            5 => format!("<b>{w}</b> "),
            11 => format!("{w}&nbsp;"),
            17 => format!("{w}, "),
            22 => format!("{w}<br>\n"),
            _ => format!("{w} "),
        };
        n16 += piece.encode_utf16().count();
        s.push_str(&piece);
        i += 1;
    }
    s
}

fn main() {
    let text = page(1 << 20);
    if let Some(path) = std::env::args().nth(1) {
        std::fs::write(path, &text).expect("write text");
    }
    let units = text.encode_utf16().count();
    let mut locator = SnippetWindowLocator::new(8);
    let run = |label: &str, locator: &mut SnippetWindowLocator, words: &[&str], phrase: bool, quotes: bool| {
        let mut found = 0;
        let mut best = f64::MAX;
        let mut total = 0.0;
        let rounds = 30;
        for _ in 0..rounds {
            let t = Instant::now();
            let prepared = PreparedQueryGroups::from_text_words(words, false, quotes);
            found = locator.find_all_matches(&text, &prepared, phrase, quotes).len() / 3;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            best = best.min(ms);
            total += ms;
        }
        println!("{label:<34} {found:>7} matches  best {best:7.2} ms  mean {:7.2} ms", total / rounds as f64);
    };
    println!("page: {units} UTF-16 units, {} bytes", text.len());
    run("one word", &mut locator, &[WORDS[0]], false, false);
    run("one word, quotes ignored", &mut locator, &[WORDS[0]], false, true);
    run("two-word phrase", &mut locator, &[WORDS[3], WORDS[4]], true, false);
}
