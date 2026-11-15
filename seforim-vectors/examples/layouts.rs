//! Single-copy vector layouts measured on one Otzaria `.oxv` file (issue #18, CI measurement):
//! the same vectors (the file's slots, decoded) and the same queries for each layout:
//! - `otz`: Otzaria's own codec as the only copy, searched as Otzaria does (every slot
//!   scored, no index): this is also the reference (exact over the stored vectors);
//! - `i8`: usearch HNSW, i8, no rescore;
//! - `f16`: usearch HNSW, f16, no rescore;
//! - `i8r`: usearch HNSW, i8, oversampled 3x and rescored with usearch's own i8 copy.
//!
//! Prints per layout: build time, size, recall@10 vs the exact scan, mean / p95 latency.
//!
//! cargo run --release -p seforim-vectors --example layouts -- <file.oxv> <work dir>
//!     [--queries q.bin (f32 little-endian, dims per query)] [--n 200] [--layouts otz,i8,f16,i8r]
//!     [--threads N]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use seforim_vectors::oxv::OxvFile;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn norm(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
}

fn fmt_ms(ms: f64) -> String {
    if ms < 1000.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{:.2} min", ms / 60000.0)
    }
}

fn stats(lat: &mut [f64]) -> (f64, f64) {
    lat.sort_by(|a, b| a.total_cmp(b));
    let mean = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
    let p95 = lat.get((lat.len() as f64 * 0.95) as usize).copied().unwrap_or(mean);
    (mean, p95)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let file = OxvFile::open(std::path::Path::new(&args[1])).expect("open .oxv");
    let work = PathBuf::from(&args[2]);
    std::fs::create_dir_all(&work).unwrap();
    let dims = file.dims();
    let slots = file.slot_count();
    let threads: usize = arg(&args, "--threads").and_then(|t| t.parse().ok()).unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let otz_threads = (threads / 2).clamp(1, 8); // Otzaria's default_scan_threads
    let n: usize = arg(&args, "--n").and_then(|t| t.parse().ok()).unwrap_or(200);
    let layouts = arg(&args, "--layouts").unwrap_or_else(|| "otz,i8,f16,i8r".into());
    let file_len = std::fs::metadata(&args[1]).unwrap().len();
    println!("file: {} slots, {} dims, {} bytes; threads {threads} (Otzaria scan {otz_threads})", slots, dims, file_len);

    // queries: real embedded queries when given, else decoded vectors of spread slots
    let mut queries: Vec<Vec<f32>> = match arg(&args, "--queries") {
        Some(q) => {
            let b = std::fs::read(q).unwrap();
            b.chunks_exact(4 * dims).map(|c| c.chunks_exact(4).map(|x| f32::from_le_bytes(x.try_into().unwrap())).collect()).collect()
        }
        None => (0..n).map(|i| file.vector(((i as u64 * 2_654_435_761) % slots as u64) as u32)).collect(),
    };
    queries.truncate(n);
    queries.iter_mut().for_each(|q| norm(q));
    println!("queries: {}", queries.len());

    // the reference: Otzaria's exact scan over its own codec
    let t = Instant::now();
    let mut lat = Vec::new();
    let truth: Vec<Vec<u32>> = queries
        .iter()
        .map(|q| {
            let s = Instant::now();
            let hits = file.scan_exact(q, 10, otz_threads, 0.0);
            lat.push(s.elapsed().as_secs_f64() * 1000.0);
            hits.into_iter().map(|h| h.0).collect()
        })
        .collect();
    if layouts.contains("otz") {
        let (mean, p95) = stats(&mut lat);
        println!(
            "layout otz (Otzaria codec, exact scan, the only copy): build none, size {} bytes, recall@10 1.000 (reference), latency mean {} p95 {} ({} queries in {})",
            file_len,
            fmt_ms(mean),
            fmt_ms(p95),
            queries.len(),
            fmt_ms(t.elapsed().as_secs_f64() * 1000.0)
        );
    }

    for (name, kind, rescore) in [("i8", usearch::ScalarKind::I8, false), ("f16", usearch::ScalarKind::F16, false), ("i8r", usearch::ScalarKind::I8, true)] {
        if !layouts.split(',').any(|l| l == name) {
            continue;
        }
        let options = usearch::IndexOptions {
            dimensions: dims,
            metric: usearch::MetricKind::Cos,
            quantization: kind,
            connectivity: 16,
            expansion_add: 128,
            expansion_search: 128,
            multi: false,
        };
        let index = usearch::Index::new(&options).unwrap();
        index.reserve_capacity_and_threads(slots as usize, threads).unwrap();
        let t = Instant::now();
        let next = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= slots as usize {
                        break;
                    }
                    let mut v = file.vector(i as u32);
                    norm(&mut v);
                    index.add(i as u64, &v).unwrap();
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if d % 500_000 == 0 {
                        eprintln!("  {name}: {d} added, {}", fmt_ms(t.elapsed().as_secs_f64() * 1000.0));
                    }
                });
            }
        });
        let build_ms = t.elapsed().as_secs_f64() * 1000.0;
        let path = work.join(format!("{name}.usearch"));
        let t = Instant::now();
        index.save(path.to_str().unwrap()).unwrap();
        let save_ms = t.elapsed().as_secs_f64() * 1000.0;
        let size = std::fs::metadata(&path).unwrap().len();
        let mem = index.memory_usage();
        let mut lat = Vec::new();
        let mut found = 0usize;
        let wanted: usize = truth.iter().map(|t| t.len()).sum();
        let mut buf = vec![0f32; dims];
        for (q, want) in queries.iter().zip(&truth) {
            let s = Instant::now();
            let got: Vec<u32> = if rescore {
                let m = index.search(q, 30).unwrap();
                let mut scored: Vec<(u32, f32)> = m
                    .keys
                    .iter()
                    .map(|&key| {
                        index.get(key, &mut buf).unwrap();
                        (key as u32, q.iter().zip(&buf).map(|(a, b)| a * b).sum::<f32>())
                    })
                    .collect();
                scored.sort_by(|a, b| b.1.total_cmp(&a.1));
                scored.into_iter().take(10).map(|x| x.0).collect()
            } else {
                index.search(q, 10).unwrap().keys.iter().map(|&k| k as u32).collect()
            };
            lat.push(s.elapsed().as_secs_f64() * 1000.0);
            found += got.iter().filter(|g| want.contains(g)).count();
        }
        let (mean, p95) = stats(&mut lat);
        println!(
            "layout {name}: build {} (+ save {}), file {} bytes, in memory {} bytes, recall@10 vs exact {:.3}, latency mean {} p95 {}",
            fmt_ms(build_ms),
            fmt_ms(save_ms),
            size,
            mem,
            found as f64 / wanted.max(1) as f64,
            fmt_ms(mean),
            fmt_ms(p95)
        );
        drop(index);
        let _ = std::fs::remove_file(&path);
    }
}
