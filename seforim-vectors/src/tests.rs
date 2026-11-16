use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::oxv::{self, OxvBookMap, OxvFile, TestBook, TestCodec, TestSegment, TestSlot};
use crate::scan::{ScanPenalties, ScanRequest};
use crate::set::{self, LineMap, LineSearch, SegmentSet};
use crate::{normalized, Error, CHUNK_BORROWED};

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
    fn unit(&mut self, dims: usize) -> Vec<f32> {
        let v: Vec<f32> = (0..dims).map(|_| (self.next() >> 11) as f32 / (1u64 << 53) as f32 * 2.0 - 1.0).collect();
        normalized(&v).unwrap()
    }
}

fn axis(dims: usize, a: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    v[a] = 1.0;
    v
}

fn slot(key: u64, hint: u32, window: u8, vector: Vec<f32>) -> TestSlot {
    TestSlot { key, hint, window, vector }
}

fn one_book_segment(codec: TestCodec, vectors: &[Vec<f32>]) -> TestSegment {
    let dims = vectors[0].len();
    TestSegment {
        dims,
        codec,
        delta: false,
        from: 0,
        to: 1,
        tag: "t".into(),
        books: vec![TestBook {
            name: "id:1".into(),
            slots: vectors.iter().enumerate().map(|(i, v)| slot(i as u64 + 1, i as u32, 0, v.clone())).collect(),
            ..Default::default()
        }],
        occurrences: vec![],
        tombstones: vec![],
    }
}

const NO_PENALTY: ScanPenalties = ScanPenalties { window: 0.0, alias: 0.0 };

/// Every codec's integer score tracks the float dot product of the query with
/// the decoded vector, and the scan's order is the order of those scores.
#[test]
fn kernels_score_as_the_decoded_float_dot_product() {
    let tmp = tempfile::tempdir().unwrap();
    for (codec, tol) in [(TestCodec::F32, 1e-5f32), (TestCodec::I8Vec, 3e-3), (TestCodec::Rot, 3e-3)] {
        let dims = 40;
        let mut rng = Rng(7);
        let vectors: Vec<Vec<f32>> = (0..300).map(|_| rng.unit(dims)).collect();
        let path = tmp.path().join(format!("{codec:?}.oxv"));
        oxv::write_segment(&path, &one_book_segment(codec, &vectors)).unwrap();
        let set = SegmentSet::open(&path).unwrap();
        let file = OxvFile::open(&path).unwrap();
        for _ in 0..5 {
            let q = rng.unit(dims);
            let hits = set.scan(&q, &ScanRequest { top_k: 300, books: None, threads: 1 }, NO_PENALTY, None).unwrap();
            assert_eq!(hits.len(), 300);
            for h in &hits {
                let decoded = file.vector(h.slot);
                let float: f32 = q.iter().zip(&decoded).map(|(a, b)| a * b).sum();
                assert!((h.score - float).abs() < tol, "{codec:?}: {} vs {float}", h.score);
            }
            assert!(hits.windows(2).all(|w| w[0].score >= w[1].score), "{codec:?}");
        }
    }
}

/// The same hits in the same order at any thread count (a total order).
#[test]
fn the_scan_is_the_same_at_any_thread_count() {
    let tmp = tempfile::tempdir().unwrap();
    let mut rng = Rng(11);
    let dims = 16;
    let vectors: Vec<Vec<f32>> = (0..20_000).map(|_| rng.unit(dims)).collect();
    for codec in [TestCodec::I8Vec, TestCodec::Rot] {
        let path = tmp.path().join(format!("t{codec:?}.oxv"));
        oxv::write_segment(&path, &one_book_segment(codec, &vectors)).unwrap();
        let set = SegmentSet::open(&path).unwrap();
        let q = rng.unit(dims);
        let run = |threads| {
            set.scan(&q, &ScanRequest { top_k: 50, books: None, threads }, NO_PENALTY, None)
                .unwrap()
                .iter()
                .map(|h| (h.slot, h.score.to_bits()))
                .collect::<Vec<_>>()
        };
        let one = run(1);
        assert_eq!(one.len(), 50);
        assert_eq!(run(3), one);
        assert_eq!(run(8), one);
        // cancelled
        let flag = AtomicBool::new(true);
        assert!(matches!(set.scan(&q, &ScanRequest { top_k: 5, books: None, threads: 2 }, NO_PENALTY, Some(&flag)), Err(Error::Cancelled)));
    }
}

/// A base + delta set in Otzaria's layout: deleted slots never return, a
/// delta's foreign record and a base extra add lines, windows and aliases
/// take their penalty, the book filter scans the admitted books' ranges and
/// their extra / foreign slots, unmapped records are counted.
fn build_set(dir: &Path, codec: TestCodec) -> SegmentSet {
    let d = 8;
    let base = TestSegment {
        dims: d,
        codec,
        delta: false,
        from: 0,
        to: 1,
        tag: "v1".into(),
        books: vec![
            TestBook {
                name: "id:1".into(),
                slots: vec![slot(1, 0, 0, axis(d, 0)), slot(2, 1, 0, axis(d, 1)), slot(3, 1, 1, axis(d, 2))],
                ..Default::default()
            },
            TestBook {
                name: "id:2".into(),
                slots: vec![slot(4, 0, 0, axis(d, 3)), slot(5, 1, 0, axis(d, 4))],
                // line 2 of id:2 holds the vector of slot 0 (key 1)
                extras: vec![(0, 2)],
                ..Default::default()
            },
        ],
        occurrences: vec![
            // key 2: line 1 of id:1, and line 1 of id:2 borrowing it (alias)
            (2, 0, 1, 0, false),
            (2, 1, 1, 0, true),
            (3, 0, 1, 1, false),
        ],
        tombstones: vec![],
    };
    let delta = TestSegment {
        dims: d,
        codec,
        delta: true,
        from: 1,
        to: 2,
        tag: "v2".into(),
        books: vec![
            // key 6 replaces key 5 (line 1 of id:2); line 3 of id:2 now also holds key 4's vector
            TestBook { name: "id:2".into(), slots: vec![slot(6, 1, 0, axis(d, 5))], foreign: vec![(4, 3)], ..Default::default() },
            TestBook { name: "id:3".into(), slots: vec![slot(7, 0, 0, axis(d, 6))], ..Default::default() },
        ],
        occurrences: vec![],
        tombstones: vec![5],
    };
    let (b, x) = (dir.join("base.oxv"), dir.join("delta.oxv"));
    oxv::write_segment(&b, &base).unwrap();
    oxv::write_segment(&x, &delta).unwrap();
    let set_dir = dir.join("vectors");
    set::write_test_set(&set_dir, 2, &[(&b, vec![4], None), (&x, vec![], Some(vec![Some((0, 3))]))]).unwrap();
    SegmentSet::open(&set_dir).unwrap()
}

fn mapping() -> LineMap {
    let mut map = LineMap::default();
    map.set(
        vec![
            OxvBookMap { name: "id:1".into(), book_id: 1, line_ids: vec![10, 11] },
            OxvBookMap { name: "id:2".into(), book_id: 2, line_ids: vec![20, 21, 22, 23] },
            OxvBookMap { name: "id:9".into(), book_id: 9, line_ids: vec![90] },
        ],
        true,
    );
    map
}

#[test]
fn a_base_and_delta_set_searches_by_the_hosts_lines() {
    for codec in [TestCodec::I8Vec, TestCodec::Rot, TestCodec::F32] {
        let tmp = tempfile::tempdir().unwrap();
        let set = build_set(tmp.path(), codec);
        let map = mapping();
        let info = set.info(Some(&map));
        assert_eq!((info.generation, info.segments.len(), info.slots_live, info.slots_dead), (2, 2, 6, 1), "{codec:?}");
        assert_eq!((info.books, info.books_mapped, info.books_unmapped), (3, 3, 1));
        assert_eq!(info.segments[1].foreign, 1);
        let d = 8;
        let search = |q: Vec<f32>, k: usize, books: Option<Vec<u64>>| {
            set.search_lines(&q, &map, &LineSearch { k, books, threads: 2, ..Default::default() }, None).unwrap()
        };
        let lines = |r: &set::LineResult| r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>();
        let near = |a: f32, b: f32| (a - b).abs() < 3e-3;

        // the deleted slot never returns: the best for e4 is not line 21 at score 1
        let r = search(axis(d, 4), 7, None);
        assert!(r.hits.iter().all(|h| h.score < 0.5), "{codec:?} {r:?}");
        // a window past the first: penalized 0.04, chunk 1
        let r = search(axis(d, 2), 1, None);
        assert_eq!((r.hits[0].line_id, r.hits[0].chunk), (11, 1));
        assert!(near(r.hits[0].score, 0.96), "{codec:?} {r:?}");
        // a primary record and an extra: both lines, score 1
        let r = search(axis(d, 0), 2, None);
        assert_eq!(lines(&r), vec![10, 22], "{codec:?}");
        assert!(near(r.hits[1].score, 1.0));
        // the alias: line 21 borrows line 11's vector, 0.04 less, chunk bit 31
        let r = search(axis(d, 1), 2, None);
        assert_eq!(lines(&r), vec![11, 21]);
        assert_eq!(r.hits[1].chunk, CHUNK_BORROWED);
        assert!(near(r.hits[1].score, 0.96));
        // a delta's foreign record resolves through its link
        let r = search(axis(d, 3), 2, None);
        assert_eq!(lines(&r), vec![20, 23], "{codec:?} {r:?}");
        // a delta slot
        let r = search(axis(d, 5), 1, None);
        assert_eq!(lines(&r), vec![21]);
        // unmapped records are counted, never guessed
        let r = search(axis(d, 6), 1, None);
        assert!(r.hits.is_empty() || r.hits[0].score < 0.5);
        let r = set.search_lines(&axis(d, 6), &map, &LineSearch { k: 1, ..Default::default() }, None).unwrap();
        assert_eq!((r.vectors, r.unmapped), (1, 1));
        // the book filter: book 2 only; the extra of key 1 is gathered from id:1's range
        let r = search(axis(d, 0), 1, Some(vec![2]));
        assert_eq!(lines(&r), vec![22], "{codec:?}");
        assert!(r.hits.iter().all(|h| h.book_id == 2));
        let r = search(axis(d, 2), 3, Some(vec![2]));
        assert!(r.hits.iter().all(|h| h.book_id == 2));
        // a filter of unmapped books scans nothing
        assert!(search(axis(d, 0), 3, Some(vec![77])).hits.is_empty());
        // wrong width
        assert!(set.search_lines(&[1.0; 3], &map, &LineSearch::default(), None).is_err());
    }
}

/// A damaged `.del` (CRC) or a pointer to another set document is refused;
/// PREVIOUS is read when CURRENT does not open.
#[test]
fn a_damaged_set_is_refused_and_previous_is_the_fallback() {
    let tmp = tempfile::tempdir().unwrap();
    build_set(tmp.path(), TestCodec::I8Vec);
    let dir = tmp.path().join("vectors");
    let del = std::fs::read_dir(dir.join("gen-000002")).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|e| e == "del")).unwrap();
    let good = std::fs::read(&del).unwrap();
    let mut bad = good.clone();
    bad[30] ^= 1;
    std::fs::write(&del, &bad).unwrap();
    assert!(matches!(SegmentSet::open(&dir), Err(Error::Format(_))));
    std::fs::write(&del, &good).unwrap();
    std::fs::copy(dir.join("CURRENT"), dir.join("PREVIOUS")).unwrap();
    std::fs::write(dir.join("CURRENT"), r#"{"generation":3,"set":"gen-000003/set.json","set_sha256":"00"}"#).unwrap();
    let set = SegmentSet::open(&dir).unwrap();
    assert!(set.info(None).recovered_from_previous);
    // a delta alone does not open
    assert!(SegmentSet::open(&tmp.path().join("delta.oxv")).is_err());
}

#[test]
fn an_otzaria_segment_reads_back() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("seg.oxv");
    let mut rng = Rng(5);
    let dims = 8;
    let v: Vec<Vec<f32>> = (0..6).map(|_| rng.unit(dims)).collect();
    oxv::write_test_segment(
        &path,
        dims,
        false,
        "v1-test",
        &[
            ("id:20", vec![(0, 0, v[0].clone()), (1, 0, v[1].clone()), (1, 1, v[2].clone()), (2, CHUNK_BORROWED, v[3].clone())]),
            ("id:10", vec![(0, 0, v[4].clone()), (5, 0, v[5].clone())]),
        ],
    )
    .unwrap();
    let file = OxvFile::open(&path).unwrap();
    let info = file.info();
    assert_eq!((info.format, info.kind.as_str(), info.dims, info.codec.as_str(), info.slots), (3, "base", dims, "f32", 6));
    assert_eq!(info.library_release_tag, "v1-test");
    assert_eq!(info.books.iter().map(|b| (b.name.as_str(), b.records, b.lines)).collect::<Vec<_>>(), vec![("id:10", 2, 6), ("id:20", 4, 3)]);
    assert_eq!(file.vector(0), v[4]);

    // damage is refused
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[100] ^= 1;
    let bad = tmp.path().join("bad.oxv");
    std::fs::write(&bad, &bytes).unwrap();
    assert!(matches!(OxvFile::open(&bad), Err(Error::Format(_))));

    // the download form expands
    let zst = tmp.path().join("seg.oxv.zst");
    std::fs::write(&zst, zstd::encode_all(&std::fs::read(&path).unwrap()[..], 3).unwrap()).unwrap();
    let out = tmp.path().join("x.oxv");
    assert_eq!(oxv::expand_zst(&zst, &out).unwrap(), std::fs::metadata(&path).unwrap().len());
    assert_eq!(OxvFile::open(&out).unwrap().info(), info);
}

/// A real Otzaria set or file, read-only (both codecs): `OXV_SET=<vectors dir
/// or .oxv> cargo test -p seforim-vectors --release --lib real_oxv -- --ignored --nocapture`.
/// Prints the set, then the latency of a few queries (random unit vectors and
/// stored vectors; a stored vector must find itself first).
#[test]
#[ignore]
fn real_oxv_set() {
    let Ok(path) = std::env::var("OXV_SET") else { return };
    let t = std::time::Instant::now();
    let set = SegmentSet::open(Path::new(&path)).unwrap();
    let info = set.info(None);
    println!(
        "open {} ms: generation {} codec {} dims {} segments {} live {} dead {} books {} tag {}",
        t.elapsed().as_millis(),
        info.generation,
        info.codec,
        info.dims,
        info.segments.len(),
        info.slots_live,
        info.slots_dead,
        info.books,
        info.library_release_tag
    );
    let file = OxvFile::open(&std::path::PathBuf::from(&path).join(&info.segments[0].file)).or_else(|_| OxvFile::open(Path::new(&path))).unwrap();
    let mut rng = Rng(1);
    let threads = crate::scan::default_scan_threads();
    let mut times = Vec::new();
    for i in 0..12 {
        let q = if i % 2 == 0 { rng.unit(info.dims) } else { normalized(&file.vector((rng.next() % file.slot_count() as u64) as u32)).unwrap() };
        let t = std::time::Instant::now();
        let hits = set.scan(&q, &ScanRequest { top_k: 50, books: None, threads }, ScanPenalties { window: 0.04, alias: 0.04 }, None).unwrap();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        times.push(ms);
        println!("query {i}: {ms:.0} ms, top score {:.4}, records {}", hits[0].score, hits[0].records.len());
    }
    times.sort_by(f64::total_cmp);
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    println!("threads {threads}: mean {mean:.0} ms, p95 {:.0} ms", times[(times.len() * 95).div_ceil(100) - 1]);
}

/// The hosts' fixture (`seforim-vectors/tests/data/synthetic.oxv`, used by
/// the C# self-test and the Dart tests) is what [`oxv::write_test_segment`]
/// writes: books `id:5` (line 0: chunks 0 and 1, line 1) and `id:7`, 4 dims.
/// `REGEN_OXV=1` rewrites it.
#[test]
fn the_hosts_oxv_fixture_is_current() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/synthetic.oxv");
    let tmp = tempfile::tempdir().unwrap();
    let fresh = tmp.path().join("f.oxv");
    let books = [("id:5", vec![(0, 0, axis(4, 1)), (0, 1, axis(4, 2)), (1, 0, axis(4, 0))]), ("id:7", vec![(0, 0, axis(4, 3))])];
    oxv::write_test_segment(&fresh, 4, false, "v1-synthetic", &books).unwrap();
    if std::env::var("REGEN_OXV").is_ok() {
        std::fs::copy(&fresh, &fixture).unwrap();
    }
    assert_eq!(std::fs::read(&fixture).unwrap(), std::fs::read(&fresh).unwrap());
    // the fixture opens as a set and searches by line
    let set = SegmentSet::open(&fixture).unwrap();
    let mut map = LineMap::default();
    map.set(vec![OxvBookMap { name: "id:5".into(), book_id: 5, line_ids: vec![1, 2] }], true);
    let r = set.search_lines(&axis(4, 2), &map, &LineSearch { k: 3, ..Default::default() }, None).unwrap();
    assert_eq!((r.hits[0].line_id, r.hits[0].chunk), (1, 1));
}

/// The text check (`map_book_rows`): keys recomputed from the host's raw rows; a row
/// holding a newline gives two lines of one line id; a tampered row's records are
/// dropped and counted; lines shifted by an inserted row are found again.
#[test]
fn a_book_mapped_from_its_rows_is_verified_against_the_keys() {
    use crate::chunker::{book_lines, BookLine, Chunker, ChunkerConfig, LineRef};
    let identity = ChunkerConfig::production().identity();
    let chunker = Chunker::production();
    let rows: Vec<&[u8]> = vec![
        b"<h1>Title of the book</h1>",
        b"first line that is long enough to stand",
        b"second\nthird line long enough to stand alone too",
        b"fourth line long enough to stand alone here",
    ];
    let lines = book_lines(&rows, &[]);
    assert_eq!(lines.len(), 5);
    let refs: Vec<LineRef<'_>> = lines.iter().map(BookLine::as_ref).collect();
    let d = 8;
    let slots: Vec<TestSlot> = (0..5)
        .map(|p| {
            let key = chunker.window_keys(&refs, p)[0].1;
            slot(u64::from_be_bytes(key.0[..8].try_into().unwrap()), p as u32, 0, axis(d, p))
        })
        .collect();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("book.oxv");
    let seg = TestSegment {
        dims: d,
        codec: TestCodec::I8Vec,
        delta: false,
        from: 0,
        to: 1,
        tag: "v1".into(),
        books: vec![TestBook { name: "id:3".into(), slots, ..Default::default() }],
        occurrences: vec![],
        tombstones: vec![],
    };
    oxv::write_segment(&path, &seg).unwrap();
    let set = SegmentSet::open(&path).unwrap();
    let ids = [100u64, 101, 102, 103];
    // no identity: refused
    assert!(set.map_book_rows("id:3", 3, &ids, &rows, &[], None).is_err());
    let (map, v) = set.map_book_rows("id:3", 3, &ids, &rows, &[], Some(identity)).unwrap();
    assert_eq!((v.lines, v.records, v.verified, v.moved, v.unverified), (5, 5, 5, 0, 0));
    assert_eq!(map.line_ids, vec![100, 101, 102, 102, 103]);
    let mut lm = LineMap::default();
    lm.set(vec![map], true);
    let r = set.search_lines(&axis(d, 3), &lm, &LineSearch { k: 1, ..Default::default() }, None).unwrap();
    assert_eq!(r.hits[0].line_id, 102);

    // a tampered row: its records are dropped and counted
    let mut tampered = rows.clone();
    tampered[3] = b"fourth line CHANGED enough to stand alone here";
    let (map, v) = set.map_book_rows("id:3", 3, &ids, &tampered, &[], Some(identity)).unwrap();
    assert!(v.unverified >= 1 && map.line_ids[4] == set::UNVERIFIED, "{v:?}");
    lm.set(vec![map], true);
    let r = set.search_lines(&axis(d, 4), &lm, &LineSearch { k: 1, ..Default::default() }, None).unwrap();
    assert!(r.hits.is_empty() && r.unmapped == 1);

    // a row inserted above: the long lines are found one line down
    let mut shifted = vec![&b"<h1>Title of the book</h1>"[..], b"an inserted line long enough to stand alone"];
    shifted.extend_from_slice(&rows[1..]);
    let (map, v) = set.map_book_rows("id:3", 3, &[100, 99, 101, 102, 103], &shifted, &[], Some(identity)).unwrap();
    assert!(v.moved >= 3, "{v:?}");
    assert_eq!((map.line_ids[1], map.line_ids[4]), (101, 103));
}

/// The hosts' rows-verify fixture (`seforim-vectors/tests/data/verified.oxv`, used by
/// the C# self-test and the Dart tests): book `id:3` with the 4 rows of
/// [`a_book_mapped_from_its_rows_is_verified_against_the_keys`], real chunk keys of the
/// production chunker, F32, 8 dims, line `p` = axis `p`. `REGEN_OXV=1` rewrites it.
#[test]
fn the_hosts_verified_fixture_is_current() {
    use crate::chunker::{book_lines, BookLine, Chunker, ChunkerConfig, LineRef};
    let chunker = Chunker::production();
    let rows: Vec<&[u8]> = vec![
        b"<h1>Title of the book</h1>",
        b"first line that is long enough to stand",
        b"second\nthird line long enough to stand alone too",
        b"fourth line long enough to stand alone here",
    ];
    let lines = book_lines(&rows, &[]);
    let refs: Vec<LineRef<'_>> = lines.iter().map(BookLine::as_ref).collect();
    let d = 8;
    let slots: Vec<TestSlot> = (0..lines.len())
        .map(|p| {
            let key = chunker.window_keys(&refs, p)[0].1;
            slot(u64::from_be_bytes(key.0[..8].try_into().unwrap()), p as u32, 0, axis(d, p))
        })
        .collect();
    let seg = TestSegment {
        dims: d,
        codec: TestCodec::F32,
        delta: false,
        from: 0,
        to: 1,
        tag: "v1-verified".into(),
        books: vec![TestBook { name: "id:3".into(), slots, ..Default::default() }],
        occurrences: vec![],
        tombstones: vec![],
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/verified.oxv");
    let tmp = tempfile::tempdir().unwrap();
    let fresh = tmp.path().join("v.oxv");
    oxv::write_segment(&fresh, &seg).unwrap();
    if std::env::var("REGEN_OXV").is_ok() {
        std::fs::copy(&fresh, &fixture).unwrap();
    }
    assert_eq!(std::fs::read(&fixture).unwrap(), std::fs::read(&fresh).unwrap());
    let set = SegmentSet::open(&fixture).unwrap();
    let identity = ChunkerConfig::production().identity();
    assert_eq!(identity, 320_260_894_818_558_297, "the hosts' tests pass this identity");
    let (_, v) = set.map_book_rows("id:3", 3, &[100, 101, 102, 103], &rows, &[], Some(identity)).unwrap();
    assert_eq!((v.lines, v.records, v.verified, v.unverified), (5, 5, 5, 0));
}
