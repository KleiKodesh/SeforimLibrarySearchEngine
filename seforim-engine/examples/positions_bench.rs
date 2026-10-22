//! Times snippet location for a search's first hits: the text path
//! (`locate_many`) against `locate_lines` (snippets from the index
//! positions), checking both give the same snippets.
//!
//! ```text
//! positions_bench <index-dir> @req.json ids <n> <out.txt>          write the first n hit line ids
//! positions_bench <index-dir> @req.json bench <texts.jsonl>        text path vs locate_lines (automatic)
//! positions_bench <index-dir> @req.json bench-forced <texts.jsonl> the same, positions read for every batch
//! ```
//!
//! `texts.jsonl`: one `[line_id, "text"]` per line (the host's SQLite texts).

use std::collections::HashMap;
use std::time::Instant;

use seforim_engine::{LineIndex, SearchRequest};

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let index = LineIndex::open(std::path::Path::new(&args[0])).expect("open");
    let json = std::fs::read_to_string(args[1].trim_start_matches('@')).expect("request file");
    let request: SearchRequest = serde_json::from_str(&json).expect("request json");
    match args[2].as_str() {
        "ids" => {
            let n: usize = args[3].parse().unwrap();
            let result = index.search(&request).expect("search");
            let ids: Vec<String> = result.hits.iter().take(n).map(|h| h.line_id.to_string()).collect();
            std::fs::write(&args[4], ids.join("\n")).unwrap();
            println!("{} hits, wrote {}", result.hits.len(), ids.len());
        }
        "bench" | "bench-forced" => {
            let force = args[2] == "bench-forced";
            let texts: HashMap<u64, String> = std::fs::read_to_string(&args[3])
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str::<(u64, String)>(l).unwrap())
                .collect();
            let mut runs = Vec::new();
            let mut scan_runs = Vec::new();
            let mut single_runs = Vec::new();
            let mut tok_runs = Vec::new();
            let mut new_runs = Vec::new();
            let (mut old1, mut new1) = (Vec::new(), Vec::new());
            let mut found = 0;
            let mut digest = 0u64;
            for _ in 0..15 {
                let mut result = index.search(&request).expect("search");
                let hit_index: Vec<usize> =
                    (0..result.hits.len()).filter(|&i| texts.contains_key(&result.hits[i].line_id)).collect();
                let hits: Vec<_> = hit_index.iter().map(|&i| result.hits[i]).collect();
                let refs: Vec<&str> = hits.iter().map(|h| texts[&h.line_id].as_str()).collect();
                let t = Instant::now();
                let located = result.session.locate_many(&refs);
                runs.push(ms(t));
                let lines: Vec<[(u64, &str); 1]> = hits.iter().zip(&refs).map(|(h, r)| [(h.line_id, *r)]).collect();
                let items: Vec<seforim_engine::HitLines<'_>> = lines
                    .iter()
                    .zip(&hit_index)
                    .map(|(l, &i)| seforim_engine::HitLines { hit: Some(i), lines: &l[..] })
                    .collect();
                let mut fresh = index.search(&request).expect("search");
                fresh.session.force_positions(force);
                let t = Instant::now();
                let positioned = fresh.session.locate_lines(&items);
                new_runs.push(ms(t));
                assert!(fresh.session.locates_from_positions());
                // production-sized batches (FtsLib plan starts at 256)
                let t = Instant::now();
                for c in refs.chunks(256) {
                    let _ = result.session.locate_many(c);
                }
                old1.push(ms(t));
                let t = Instant::now();
                for c in items.chunks(256) {
                    let _ = fresh.session.locate_lines(c);
                }
                new1.push(ms(t));
                let same = located == positioned
                    && result.session.all_terms() == fresh.session.all_terms();
                if !same {
                    let bad = located.iter().zip(&positioned).position(|(a, b)| a != b);
                    println!("MISMATCH at {:?} of {}", bad, located.len());
                    std::process::exit(2);
                }
                found = located.iter().filter(|s| s.is_some()).count();
                digest = located.iter().flatten().fold(0u64, |a, s| {
                    a.wrapping_mul(31).wrapping_add((s.start * 7 + s.end * 13 + s.score + s.marks.len()) as u64)
                });
                let t = Instant::now();
                let mut words = 0;
                for r in &refs {
                    hebrew_fts::scan(r, |_, _| words += 1);
                }
                scan_runs.push(ms(t));
                let _ = words;
                let t = Instant::now();
                for r in &refs {
                    let _ = result.session.locate(r);
                }
                single_runs.push(ms(t));
                let mut loc = hebrew_fts::SnippetWindowLocator::new(10);
                let t = Instant::now();
                for r in &refs {
                    loc.tokenize(r);
                }
                tok_runs.push(ms(t));
            }
            let min = |v: &[f64]| v.iter().cloned().fold(f64::MAX, f64::min);
            println!("FIRST old {:.2} positioned {:.2} (first round: the .pos pages first touched)", runs[0], new_runs[0]);
            println!(
                "RESULT old median {:.2} min {:.2} | positioned median {:.2} min {:.2}",
                median(runs.clone()),
                min(&runs),
                median(new_runs.clone()),
                min(&new_runs)
            );
            println!(
                "RESULT256 batches of 256 old median {:.2} min {:.2} | positioned median {:.2} min {:.2}",
                median(old1.clone()),
                min(&old1),
                median(new1.clone()),
                min(&new1)
            );
            println!(
                "texts {} located {} digest {digest} locate median {:.2} ms (runs {:?}) single-thread scan median {:.2} ms, single-thread locate median {:.2} ms, tokenize {:.2} ms",
                texts.len(),
                found,
                median(runs.clone()),
                runs,
                median(scan_runs),
                median(single_runs),
                median(tok_runs)
            );
        }
        _ => panic!("mode"),
    }
}
