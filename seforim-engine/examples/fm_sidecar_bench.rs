//! Issue #20: rebuilds the FM-index sidecar of an existing (merged) index, to measure the
//! build's time and peak memory from outside (e.g. a private-bytes sampler on this process).
//!
//!     cargo run --release -p seforim-engine --example fm_sidecar_bench -- <index dir>
//!
//! Removes the index's `.fmi` files first (work on a COPY of the index), then builds.

use std::time::Instant;

use seforim_engine::index::LineIndex;

fn main() {
    let dir = std::path::PathBuf::from(
        std::env::args()
            .nth(1)
            .expect("usage: fm_sidecar_bench <index dir>"),
    );
    for entry in std::fs::read_dir(&dir).expect("index dir").flatten() {
        if entry.path().extension().is_some_and(|e| e == "fmi") {
            std::fs::remove_file(entry.path()).expect("remove old sidecar");
        }
    }
    let index = LineIndex::open(&dir).expect("open index");
    let t = Instant::now();
    let built = index.build_fm_sidecar().expect("FM sidecar build");
    let ms = t.elapsed().as_millis();
    let size: u64 = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "fmi"))
        .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
        .sum();
    let time = if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} min", ms as f64 / 60000.0)
    };
    println!(
        "FM sidecar: built {built} in {time}, {:.1} MB",
        size as f64 / 1e6
    );
}
