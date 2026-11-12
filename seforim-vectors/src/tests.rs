use super::*;

/// splitmix64: deterministic synthetic vectors.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
    fn vector(&mut self, dims: usize) -> Vec<f32> {
        (0..dims).map(|_| self.unit()).collect()
    }
}

const DIMS: usize = 48;

/// `books` books of `per_book` lines; line id = book * 100_000 + i.
fn records(seed: u64, books: u64, per_book: u64) -> Vec<VectorRecord> {
    let mut rng = Rng(seed);
    let mut out = Vec::new();
    for b in 1..=books {
        for i in 0..per_book {
            out.push(VectorRecord { line_id: b * 100_000 + i, book_id: b, vector: rng.vector(DIMS) });
        }
    }
    out
}

/// Brute force in f32 over the given records (the reference).
fn brute(records: &[VectorRecord], query: &[f32], k: usize, books: Option<&[u64]>) -> Vec<VectorHit> {
    let q = normalized(query).unwrap();
    let mut hits: Vec<VectorHit> = records
        .iter()
        .filter(|r| books.is_none_or(|b| b.contains(&r.book_id)))
        .map(|r| {
            let v = normalized(&r.vector).unwrap();
            VectorHit { line_id: r.line_id, book_id: r.book_id, score: q.iter().zip(&v).map(|(a, b)| a * b).sum() }
        })
        .collect();
    hits.sort_by(best_first);
    hits.truncate(k);
    hits
}

fn build(dir: &Path, recs: &[VectorRecord]) -> ApplyStats {
    let update = VectorUpdate { add: recs.to_vec(), ..Default::default() };
    apply(dir, &BuildParams::new(DIMS), &update, ThreadHooks::default(), None).unwrap()
}

fn ids(h: &[VectorHit]) -> Vec<u64> {
    h.iter().map(|h| h.line_id).collect()
}

#[test]
fn exact_scan_equals_brute_force() {
    let tmp = tempfile::tempdir().unwrap();
    let recs = records(1, 4, 300);
    build(tmp.path(), &recs);
    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    let mut rng = Rng(99);
    for _ in 0..20 {
        let q = rng.vector(DIMS);
        let mut p = SearchParams::new(10);
        p.exact_threshold = usize::MAX;
        let (got, path) = store.search_with_path(&q, &p, None).unwrap();
        assert!(path.exact);
        let want = brute(&recs, &q, 10, None);
        assert_eq!(ids(&got), ids(&want));
        for (g, w) in got.iter().zip(&want) {
            assert!((g.score - w.score).abs() < 2e-3, "f16 rescore close to f32");
            assert_eq!(g.book_id, w.book_id);
        }
    }
}

#[test]
fn hnsw_recall_at_10_is_at_least_095() {
    let tmp = tempfile::tempdir().unwrap();
    let recs = records(2, 10, 1000);
    build(tmp.path(), &recs);
    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    let mut rng = Rng(7);
    let (mut found, mut total) = (0usize, 0usize);
    for _ in 0..50 {
        let q = rng.vector(DIMS);
        let mut p = SearchParams::new(10);
        p.exact_threshold = 0;
        let (got, path) = store.search_with_path(&q, &p, None).unwrap();
        assert!(!path.exact);
        let want = ids(&brute(&recs, &q, 10, None));
        found += ids(&got).iter().filter(|id| want.contains(id)).count();
        total += 10;
    }
    let recall = found as f64 / total as f64;
    eprintln!("recall@10 = {recall}");
    assert!(recall >= 0.95, "recall@10 = {recall}");
}

#[test]
fn rescore_keeps_or_improves_the_order() {
    let tmp = tempfile::tempdir().unwrap();
    let recs = records(3, 10, 1000);
    build(tmp.path(), &recs);
    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    let mut rng = Rng(8);
    let (mut once, mut over) = (0usize, 0usize);
    for _ in 0..30 {
        let q = rng.vector(DIMS);
        let want = ids(&brute(&recs, &q, 10, None));
        let mut p = SearchParams::new(10);
        p.exact_threshold = 0;
        p.oversample = 1.0;
        once += ids(&store.search(&q, &p, None).unwrap()).iter().filter(|i| want.contains(i)).count();
        p.oversample = 3.0;
        let got = store.search(&q, &p, None).unwrap();
        over += ids(&got).iter().filter(|i| want.contains(i)).count();
        // rescored hits are in exact score order
        assert!(got.windows(2).all(|w| best_first(&w[0], &w[1]) != Ordering::Greater));
    }
    assert!(over >= once, "oversample 3 found {over}, 1 found {once}");
}

#[test]
fn filtered_search_small_uses_the_exact_scan_large_the_graph() {
    let tmp = tempfile::tempdir().unwrap();
    let recs = records(4, 10, 1000);
    build(tmp.path(), &recs);
    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    let mut rng = Rng(9);
    let q = rng.vector(DIMS);

    let mut p = SearchParams::new(10);
    p.books = Some(vec![3, 999]);
    p.exact_threshold = 2000;
    let (got, path) = store.search_with_path(&q, &p, None).unwrap();
    assert!(path.exact);
    assert_eq!(path.allowed, 1000);
    assert_eq!(ids(&got), ids(&brute(&recs, &q, 10, Some(&[3]))));

    p.books = Some(vec![2, 5, 6, 7, 8]);
    let (got, path) = store.search_with_path(&q, &p, None).unwrap();
    assert!(!path.exact);
    assert_eq!(path.allowed, 5000);
    assert!(got.iter().all(|h| [2, 5, 6, 7, 8].contains(&h.book_id)));
    let want = ids(&brute(&recs, &q, 10, Some(&[2, 5, 6, 7, 8])));
    let found = ids(&got).iter().filter(|i| want.contains(i)).count();
    assert!(found >= 8, "filtered recall {found}/10");

    p.books = Some(vec![]);
    assert!(store.search(&q, &p, None).unwrap().is_empty());
}

#[test]
fn add_remove_update_by_book_and_line() {
    let tmp = tempfile::tempdir().unwrap();
    let recs = records(5, 4, 200);
    let s = build(tmp.path(), &recs);
    assert_eq!((s.generation, s.lines, s.books, s.rebuilt), (1, 800, 4, true));

    // book 2 replaced, a line of book 3 removed, a new book 9
    let mut rng = Rng(55);
    let new_b2: Vec<VectorRecord> =
        (0..50).map(|i| VectorRecord { line_id: 2 * 100_000 + 5000 + i, book_id: 2, vector: rng.vector(DIMS) }).collect();
    let b9: Vec<VectorRecord> = (0..30).map(|i| VectorRecord { line_id: 900_000 + i, book_id: 9, vector: rng.vector(DIMS) }).collect();
    let mut add = new_b2.clone();
    add.extend(b9.clone());
    let update = VectorUpdate { remove_books: vec![2], remove_lines: vec![300_007], add };
    let s = apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), None).unwrap();
    // 201 of 800 removed: past the 25 % share, the graph is rebuilt
    assert_eq!((s.generation, s.removed, s.added, s.lines, s.rebuilt), (2, 201, 80, 679, true));

    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    assert_eq!(store.book_counts(), vec![(1, 200), (2, 50), (3, 199), (4, 200), (9, 30)]);
    assert!(store.vector(200_001).is_none());
    assert!(store.vector(300_007).is_none());
    assert!(store.vector(900_003).is_some());
    // the reference set after the update
    let mut now: Vec<VectorRecord> = recs.iter().filter(|r| r.book_id != 2 && r.line_id != 300_007).cloned().collect();
    now.extend(new_b2);
    now.extend(b9);
    let q = rng.vector(DIMS);
    let mut p = SearchParams::new(10);
    p.exact_threshold = usize::MAX;
    assert_eq!(ids(&store.search(&q, &p, None).unwrap()), ids(&brute(&now, &q, 10, None)));
    // the graph: removed lines never come back
    p.exact_threshold = 0;
    p.k = 100;
    let got = store.search(&q, &p, None).unwrap();
    assert!(got.iter().all(|h| h.book_id != 2 || h.line_id >= 205_000));
    assert!(got.iter().all(|h| h.line_id != 300_007));

    // a line added again moves to its new book
    let moved = VectorRecord { line_id: 100_001, book_id: 4, vector: rng.vector(DIMS) };
    let update = VectorUpdate { add: vec![moved], ..Default::default() };
    let s = apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), None).unwrap();
    assert!(!s.rebuilt, "one line: the graph is updated in place");
    drop(store);
    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    assert_eq!(store.book_counts()[0], (1, 199));
    assert!(store.lines().contains(&(100_001, 4)));
}

#[test]
fn save_load_mmap_and_generations() {
    let tmp = tempfile::tempdir().unwrap();
    let recs = records(6, 2, 100);
    build(tmp.path(), &recs);
    let first = VectorStore::open(tmp.path()).unwrap().unwrap();
    assert_eq!((first.len(), first.dims(), first.generation()), (200, DIMS, 1));
    let q = Rng(1).vector(DIMS);
    let before = first.search(&q, &SearchParams::new(5), None).unwrap();

    // a new generation while the first is still mapped
    let update = VectorUpdate { remove_books: vec![1], ..Default::default() };
    apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), None).unwrap();
    // the old map still answers
    assert_eq!(first.search(&q, &SearchParams::new(5), None).unwrap(), before);
    let second = VectorStore::open(tmp.path()).unwrap().unwrap();
    assert_eq!((second.len(), second.generation()), (100, 2));
    drop(first);
    // the next write collects generation 1 (still mapped at the last one on Windows)
    let update = VectorUpdate { remove_lines: vec![200_000], ..Default::default() };
    apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), None).unwrap();
    // generation 2 may survive while `second` maps it (Windows), 1 is gone
    let gens = generations(tmp.path());
    assert!(!gens.contains(&1) && gens.last() == Some(&3), "{gens:?}");

    // wrong dims refused, the store untouched
    let bad = VectorUpdate { add: vec![VectorRecord { line_id: 1, book_id: 1, vector: vec![1.0; 3] }], ..Default::default() };
    assert!(matches!(apply(tmp.path(), &BuildParams::new(3), &bad, ThreadHooks::default(), None), Err(Error::Invalid(_))));
    // a damaged file is a format error
    let path = second.path().to_path_buf();
    drop(second);
    let mut bytes = std::fs::read(sev_path(tmp.path(), 3)).unwrap();
    bytes[8] = 99;
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("vectors-0000000001.sev"), &bytes).unwrap();
    assert!(matches!(VectorStore::open(other.path()), Err(Error::Format(_))));
    let _ = path;
}

#[test]
fn removing_everything_leaves_an_empty_store() {
    let tmp = tempfile::tempdir().unwrap();
    build(tmp.path(), &records(10, 1, 20));
    let update = VectorUpdate { remove_books: vec![1], ..Default::default() };
    let s = apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), None).unwrap();
    assert_eq!(s.lines, 0);
    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    assert!(store.is_empty());
    assert!(store.search(&vec![1.0; DIMS], &SearchParams::new(3), None).unwrap().is_empty());
}

#[test]
fn cancel_stops_search_and_build() {
    let tmp = tempfile::tempdir().unwrap();
    let recs = records(11, 2, 3000);
    let stop = AtomicBool::new(true);
    let update = VectorUpdate { add: recs.clone(), ..Default::default() };
    assert!(matches!(apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), Some(&stop)), Err(Error::Cancelled)));
    assert!(!exists(tmp.path()));
    build(tmp.path(), &recs);
    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    let mut p = SearchParams::new(5);
    p.exact_threshold = usize::MAX;
    assert!(matches!(store.search(&vec![1.0; DIMS], &p, Some(&stop)), Err(Error::Cancelled)));
    p.exact_threshold = 0;
    assert!(matches!(store.search(&vec![1.0; DIMS], &p, Some(&stop)), Err(Error::Cancelled)));
}

#[test]
fn bad_input_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dup = VectorUpdate {
        add: vec![
            VectorRecord { line_id: 1, book_id: 1, vector: vec![1.0; DIMS] },
            VectorRecord { line_id: 1, book_id: 1, vector: vec![1.0; DIMS] },
        ],
        ..Default::default()
    };
    assert!(matches!(apply(tmp.path(), &BuildParams::new(DIMS), &dup, ThreadHooks::default(), None), Err(Error::Invalid(_))));
    let zero = VectorUpdate { add: vec![VectorRecord { line_id: 1, book_id: 1, vector: vec![0.0; DIMS] }], ..Default::default() };
    assert!(matches!(apply(tmp.path(), &BuildParams::new(DIMS), &zero, ThreadHooks::default(), None), Err(Error::Invalid(_))));
    build(tmp.path(), &records(12, 1, 10));
    let store = VectorStore::open(tmp.path()).unwrap().unwrap();
    assert!(matches!(store.search(&[1.0; 3], &SearchParams::new(3), None), Err(Error::Invalid(_))));
}

#[test]
fn small_updates_add_up_to_a_rebuild() {
    let tmp = tempfile::tempdir().unwrap();
    build(tmp.path(), &records(13, 1, 100));
    let mut rebuilt = Vec::new();
    for i in 0..4u64 {
        // 10 lines each: under 25 % alone, past it together
        let update = VectorUpdate { remove_lines: (0..10).map(|j| 100_000 + i * 10 + j).collect(), ..Default::default() };
        rebuilt.push(apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), None).unwrap().rebuilt);
    }
    // removed so far vs 25 % of the store: 10 < 25, 20 < 22.5, 30 >= 20 rebuilds, then 10 < 17.5
    assert_eq!(rebuilt, vec![false, false, true, false]);
}

#[test]
fn a_second_writer_is_refused_and_overflowing_headers_are_format_errors() {
    let tmp = tempfile::tempdir().unwrap();
    build(tmp.path(), &records(14, 1, 10));
    let lock = std::fs::OpenOptions::new().write(true).open(tmp.path().join("write.lock")).unwrap();
    assert!(fs4::fs_std::FileExt::try_lock_exclusive(&lock).unwrap());
    let update = VectorUpdate { remove_lines: vec![100_000], ..Default::default() };
    assert!(matches!(apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), None), Err(Error::Invalid(_))));
    drop(lock);
    apply(tmp.path(), &BuildParams::new(DIMS), &update, ThreadHooks::default(), None).unwrap();

    // a count that overflows u64 arithmetic
    let mut bytes = std::fs::read(sev_path(tmp.path(), 2)).unwrap();
    bytes[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("vectors-0000000001.sev"), &bytes).unwrap();
    assert!(matches!(VectorStore::open(other.path()), Err(Error::Format(_))));
}
