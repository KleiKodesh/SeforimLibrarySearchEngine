//! Term-dictionary statistics of an index: terms per segment and how many
//! terms contain a given substring (the trigram-candidate count for `*x*`).
//!
//! ```text
//! cargo run --release -p seforim-engine --example termstats -- <index-dir> [core ...]
//! ```

use std::time::Instant;

use seforim_engine::LineIndex;
use tantivy::schema::Field;

fn main() -> tantivy::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let index = LineIndex::open(std::path::Path::new(&args[0])).expect("open");
    let searcher = index.reader().expect("reader").searcher();
    let text: Field = index.text_field();
    for (ord, segment) in searcher.segment_readers().iter().enumerate() {
        let inverted = segment.inverted_index(text)?;
        let dict = inverted.terms();
        println!("segment {ord}: {} terms, {} docs", dict.num_terms(), segment.num_docs());
        if args.len() > 1 {
            let t = Instant::now();
            let mut stream = dict.stream()?;
            let mut total_bytes = 0usize;
            let mut counts = vec![0usize; args.len() - 1];
            while stream.advance() {
                let key = stream.key();
                total_bytes += key.len();
                if let Ok(s) = std::str::from_utf8(key) {
                    for (i, core) in args[1..].iter().enumerate() {
                        if s.contains(core.as_str()) {
                            counts[i] += 1;
                        }
                    }
                }
            }
            println!(
                "  full stream in {:.0} ms, {:.1} MB of term bytes",
                t.elapsed().as_secs_f64() * 1000.0,
                total_bytes as f64 / 1e6
            );
            for (i, core) in args[1..].iter().enumerate() {
                println!("  terms containing {core:?}: {}", counts[i]);
            }
        }
    }
    Ok(())
}
