//! Issue #19 micro benchmark: indexing a synthetic corpus with the writer's
//! auto mode vs fixed settings, and the watcher's overhead.
//!
//!     cargo run --release -p seforim-engine --example writer_resources_bench -- [lines] [rounds]
//!
//! Modes (each round runs every mode once, interleaved):
//! - fixed-200MB: the demos' old default (200 MB total, min(cores, 4) threads, normal priority)
//! - fixed-auto-config: the auto mode's threads, budget and background priority, NO watcher
//! - auto-idle: `writer_auto` with the load held at Idle (watcher thread running)
//! - auto: `writer_auto` (watcher thread running, OS signals live)
//! - auto-pressure: `writer_auto`, a simulated memory-pressure signal after half the lines
//! - auto-active: `writer_auto`, the user active (simulated) from the start: moderate tier
//! - active-<N>MB: the auto mode's threads and background priority at a fixed N MB per
//!   thread (15 = the old active tier, the minimum); used to choose the moderate budget
//!
//! A third argument runs only the modes whose names contain it (one mode per process
//! lets a caller read the process's peak memory).
//!
//! The watcher's overhead = auto-idle vs fixed-auto-config.

use std::time::Instant;

use seforim_engine::index::{IndexOptions, LineDoc, LineIndex, LineWriter};
use seforim_engine::resources::{self, Load};

fn corpus(lines: usize) -> Vec<String> {
    // deterministic pseudo-random words over a 26-letter alphabet
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    (0..lines)
        .map(|_| {
            let words = 8 + (next() % 10) as usize;
            (0..words)
                .map(|_| {
                    let len = 2 + (next() % 7) as usize;
                    (0..len).map(|_| (b'a' + (next() % 26) as u8) as char).collect::<String>()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn feed(w: &mut LineWriter, texts: &[String], from: usize, to: usize) {
    for (i, text) in texts.iter().enumerate().take(to).skip(from) {
        let line = LineDoc {
            line_id: i as u64,
            position: i as u64,
            book_id: (i / 1000) as u64,
            text: text.clone(),
            section_id: None,
            no_cross: false,
        };
        w.add(&line).unwrap();
    }
}

struct Run {
    ms: f64,
    segments: usize,
    budget_end: usize,
}

fn run(mode: &str, texts: &[String]) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
    let start = Instant::now();
    let threads = resources::auto_threads();
    let mut w = match mode {
        "fixed-200MB" => {
            let t = std::thread::available_parallelism().map_or(1, |n| n.get()).min(4);
            index.writer(200_000_000, t).unwrap()
        }
        "fixed-auto-config" => index
            .writer_with(resources::idle_budget_per_thread(threads) * threads, threads, true)
            .unwrap(),
        m if m.starts_with("active-") => {
            let mb: usize = m["active-".len()..m.len() - 2].parse().unwrap();
            index.writer_with(mb * 1_000_000 * threads, threads, true).unwrap()
        }
        _ => index.writer_auto().unwrap(),
    };
    if mode == "auto-idle" {
        w.simulate_load(Some(Load::Idle));
    }
    if mode == "auto-active" {
        w.simulate_load(Some(Load::Active));
    }
    let half = texts.len() / 2;
    feed(&mut w, texts, 0, half);
    if mode == "auto-pressure" {
        w.simulate_load(Some(Load::Pressure));
    }
    feed(&mut w, texts, half, texts.len());
    let budget_end = w.memory_budget_per_thread();
    w.commit(None).unwrap();
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    drop(w);
    let segments = index.reader().unwrap().searcher().segment_readers().len();
    Run { ms, segments, budget_end }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let lines: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(300_000);
    let rounds: usize = args.get(2).and_then(|a| a.parse().ok()).unwrap_or(3);
    let texts = corpus(lines);
    let threads = resources::auto_threads();
    println!(
        "lines {lines}, rounds {rounds}, cores {}, auto threads {threads}, idle budget/thread {} MB, free RAM {} MB, watcher {}",
        std::thread::available_parallelism().map_or(1, |n| n.get()),
        resources::idle_budget_per_thread(threads) / 1_000_000,
        resources::available_memory().map_or(0, |b| b / 1_000_000),
        resources::HAS_WATCHER
    );
    let filter = args.get(3).cloned().unwrap_or_default();
    let modes: Vec<&str> = [
        "fixed-200MB",
        "fixed-auto-config",
        "auto-idle",
        "auto",
        "auto-pressure",
        "auto-active",
        "active-15MB",
        "active-32MB",
        "active-64MB",
        "active-128MB",
    ]
    .into_iter()
    .filter(|m| m.contains(filter.as_str()))
    .collect();
    let mut all: Vec<Vec<Run>> = modes.iter().map(|_| Vec::new()).collect();
    for round in 0..rounds {
        for (m, mode) in modes.iter().enumerate() {
            let r = run(mode, &texts);
            println!(
                "round {round} {mode:<18} {:>8.0} ms  segments {:>3}  budget/thread at end {} MB",
                r.ms,
                r.segments,
                r.budget_end / 1_000_000
            );
            all[m].push(r);
        }
    }
    println!("summary (median of {rounds}):");
    for (m, mode) in modes.iter().enumerate() {
        let mut ms: Vec<f64> = all[m].iter().map(|r| r.ms).collect();
        ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let (min, max) = (ms[0], ms[ms.len() - 1]);
        println!(
            "  {mode:<18} median {:>8.0} ms  (min {min:.0}, max {max:.0})  segments {}",
            ms[ms.len() / 2],
            all[m][0].segments
        );
    }
}
