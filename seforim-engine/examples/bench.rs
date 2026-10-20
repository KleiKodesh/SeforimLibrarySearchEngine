//! Times the stages of a search on an existing index.
//!
//! ```text
//! cargo run --release -p seforim-engine --example bench -- <index-dir> '<request json>'
//! cargo run --release -p seforim-engine --example bench -- <index-dir> --build-fm
//! ```
//!
//! `--build-fm` builds (or rebuilds) the FM-index sidecar over the index's
//! single segment and reports the time and size.

use std::time::Instant;

use seforim_engine::{LineIndex, SearchRequest};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let index = LineIndex::open(std::path::Path::new(&args[0])).expect("open");
    if args[1] == "--build-fm" {
        let t = Instant::now();
        let built = index.build_fm_sidecar().expect("build fm sidecar");
        let size: u64 = std::fs::read_dir(index.dir())
            .expect("dir")
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "fmi"))
            .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
            .sum();
        println!(
            "fm sidecar built={built} in {:.1} s, {:.1} MB, present={}",
            t.elapsed().as_secs_f64(),
            size as f64 / 1e6,
            index.has_fm_sidecar()
        );
        return;
    }
    // A request JSON, or @file holding one (shells mangle quotes in arguments).
    let json = match args[1].strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path).expect("read request file"),
        None => args[1].clone(),
    };
    let request: SearchRequest = serde_json_lite(&json);
    for round in 0..3 {
        let t = Instant::now();
        let result = index.search(&request).expect("search");
        println!(
            "round {round}: {} hits, {} matched terms, search {:.1} ms",
            result.hits.len(),
            result.session.group_count(),
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
}

fn serde_json_lite(s: &str) -> SearchRequest {
    // the engine crate has serde derives; parse via serde's own json-free path
    // by round-tripping through the FFI crate is overkill: use a tiny shim
    serde_json::from_str(s).expect("request json")
}
