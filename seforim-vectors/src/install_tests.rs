use std::fs;
use std::path::{Path, PathBuf};

use super::{install, sha256_hex, verify_segment};
use crate::oxv::{self, OxvBookMap, OxvFile, TestBook, TestCodec, TestSegment, TestSlot};
use crate::set::{self, LineMap, LineSearch, SegmentSet};

fn slot(key: u64, hint: u32, vector: Vec<f32>) -> TestSlot {
    TestSlot { key, hint, window: 0, vector }
}

fn axis(dims: usize, a: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    v[a] = 1.0;
    v
}

/// Gives a written segment an id (header bytes 32..48) and recomputes the header CRC.
fn stamp(path: &Path, id: u8) {
    let mut b = fs::read(path).unwrap();
    b[32..48].copy_from_slice(&[id; 16]);
    let crc = crc32fast::hash(&b[..4088]);
    b[4088..4092].copy_from_slice(&crc.to_le_bytes());
    fs::write(path, b).unwrap();
}

/// The release manifest Otzaria publishes beside a segment.
fn manifest(path: &Path) -> String {
    let seg = OxvFile::open(path).unwrap();
    let b = seg.bytes();
    let hex = |r: std::ops::Range<usize>| super::hex(&b[r]);
    let u64_at = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    let (from, to) = seg.library_versions();
    serde_json::json!({
        "format": "otzaria-vectors-release", "formatVersion": 1, "kind": seg.kind(),
        "identity": {"text": {"key_version": 1}, "model": {"family_id": "f", "chunking_identity": 7,
            "query_packages": [{"checksum": "c", "quantization": "fp32"}]},
            "store": {"backend_id": "otzaria-oxv", "store_format_version": 3, "vector_precision": seg.codec().name()}},
        "identityDigest": hex(48..80), "codecParamsSha256": hex(80..112),
        "fromLibraryVersion": from, "toLibraryVersion": to, "libraryReleaseTag": seg.release_tag(),
        "segmentId": hex(32..48),
        "counts": {"books": u32::from_le_bytes(b[144..148].try_into().unwrap()), "slots": u64_at(112),
            "extras": u64_at(120), "foreign": u64_at(128), "tombstones": u64_at(136)},
        "segment": {"sha256": sha256_hex(b), "size": b.len()},
        "packageDigest": "p", "provenance": {"worker": "test"}, "createdAt": "", "files": [],
    })
    .to_string()
}

const D: usize = 8;

fn segments(dir: &Path) -> (PathBuf, PathBuf) {
    let base = TestSegment {
        dims: D,
        codec: TestCodec::I8Vec,
        delta: false,
        from: 0,
        to: 1,
        tag: "v1".into(),
        books: vec![
            TestBook { name: "id:1".into(), slots: vec![slot(1, 0, axis(D, 0)), slot(2, 1, axis(D, 1))], ..Default::default() },
            TestBook {
                name: "id:2".into(),
                slots: vec![slot(4, 0, axis(D, 3)), slot(5, 1, axis(D, 4))],
                extras: vec![(0, 2)],
                ..Default::default()
            },
        ],
        occurrences: vec![],
        tombstones: vec![],
    };
    let delta = TestSegment {
        dims: D,
        codec: TestCodec::I8Vec,
        delta: true,
        from: 1,
        to: 2,
        tag: "v2".into(),
        books: vec![
            // key 6 replaces key 5 (tombstone); key 2 shipped again (the base's copy a
            // duplicate); line 3 of id:2 holds key 4's vector, key 99 no segment holds
            TestBook {
                name: "id:2".into(),
                slots: vec![slot(6, 1, axis(D, 5)), slot(2, 4, axis(D, 1))],
                foreign: vec![(4, 3), (99, 5)],
                ..Default::default()
            },
        ],
        occurrences: vec![],
        tombstones: vec![5],
    };
    let (b, x) = (dir.join("base.oxv"), dir.join("delta.oxv"));
    oxv::write_segment(&b, &base).unwrap();
    oxv::write_segment(&x, &delta).unwrap();
    stamp(&b, 0x11);
    stamp(&x, 0x22);
    (b, x)
}

fn mapping() -> LineMap {
    let mut map = LineMap::default();
    map.set(
        vec![
            OxvBookMap { name: "id:1".into(), book_id: 1, line_ids: vec![10, 11] },
            OxvBookMap { name: "id:2".into(), book_id: 2, line_ids: vec![20, 21, 22, 23, 24, 25] },
        ],
        true,
    );
    map
}

#[test]
fn verify_checks_blocks_and_the_manifest() {
    let tmp = tempfile::tempdir().unwrap();
    let (b, _) = segments(tmp.path());
    let m = manifest(&b);
    let r = verify_segment(&b, Some(&m)).unwrap();
    assert_eq!((r.kind.as_str(), r.slots, r.manifest_checked), ("base", 4, true));
    assert!(r.blocks >= 3);
    let wrong = m.replace("\"toLibraryVersion\":1", "\"toLibraryVersion\":5");
    assert_ne!(wrong, m);
    assert!(verify_segment(&b, Some(&wrong)).is_err());
    // a flipped byte in a block-summed section (the vectors) is refused
    let seg = OxvFile::open(&b).unwrap();
    let dir_at = |kind: u16| {
        let bytes = seg.bytes();
        (0..u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize)
            .map(|i| 224 + i * 40)
            .find(|&at| u16::from_le_bytes([bytes[at], bytes[at + 1]]) == kind)
            .map(|at| u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize)
            .unwrap()
    };
    let at = dir_at(0x0012);
    drop(seg);
    let mut bytes = fs::read(&b).unwrap();
    bytes[at] ^= 0x40;
    let bad = tmp.path().join("bad.oxv");
    fs::write(&bad, bytes).unwrap();
    let e = verify_segment(&bad, None);
    assert!(e.is_err(), "{e:?}");
}

#[test]
fn base_then_delta_installs_as_otzaria_and_the_reader_opens_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (b, x) = segments(tmp.path());
    let dir = tmp.path().join("vectors");
    // a delta first: refused
    assert!(install(&dir, &x, &manifest(&x), None).is_err());
    // the base from incoming/ (moved), the delta from elsewhere (copied)
    fs::create_dir_all(dir.join("incoming")).unwrap();
    let inc = dir.join("incoming/base.oxv");
    let mb = manifest(&b);
    fs::copy(&b, &inc).unwrap();
    assert!(install(&dir, &inc, &mb, Some(&"0".repeat(64))).is_err());
    let r = install(&dir, &inc, &mb, Some(&sha256_hex(mb.as_bytes()))).unwrap();
    assert_eq!((r.kind.as_str(), r.generation, r.library_version, r.segments), ("base", 1, 1, 1));
    assert!(!inc.exists());
    let r = install(&dir, &x, &manifest(&x), None).unwrap();
    assert_eq!((r.generation, r.library_version, r.segments), (2, 2, 2));
    assert_eq!((r.tombstones_applied, r.duplicates_removed, r.foreign_unresolved), (1, 1, 1));
    assert!(install(&dir, &x, &manifest(&x), None).unwrap().already_applied);

    let prev: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("PREVIOUS")).unwrap()).unwrap();
    assert_eq!(prev["generation"], 1);
    let id2 = "22".repeat(16);
    assert!(dir.join(format!("gen-000002/{id2}.links")).exists());
    assert!(dir.join(format!("segments/{id2}.package.json")).exists());
    let doc: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("gen-000002/set.json")).unwrap()).unwrap();
    assert_eq!(doc["stats"]["slots_dead"], 2);
    assert_eq!(doc["stats"]["foreign"], 2);
    assert_eq!(doc["stats"]["foreign_unresolved"], 1);

    // the set reader opens it, and it searches as the same set written by hand
    let set = SegmentSet::open(&dir).unwrap();
    let info = set.info(None);
    assert_eq!((info.generation, info.slots_live, info.slots_dead), (2, 4, 2));
    let hand_dir = tmp.path().join("hand");
    set::write_test_set(&hand_dir, 2, &[(&b, vec![1, 3], None), (&x, vec![], Some(vec![Some((0, 2)), None]))]).unwrap();
    let hand = SegmentSet::open(&hand_dir).unwrap();
    let map = mapping();
    for a in 0..6 {
        let req = LineSearch { k: 8, threads: 1, ..Default::default() };
        let ours = set.search_lines(&axis(D, a), &map, &req, None).unwrap();
        let theirs = hand.search_lines(&axis(D, a), &map, &req, None).unwrap();
        assert_eq!(ours.hits, theirs.hits, "axis {a}");
    }
    let r = set.search_lines(&axis(D, 3), &map, &LineSearch { k: 2, threads: 1, ..Default::default() }, None).unwrap();
    let mut lines: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
    lines.sort();
    assert_eq!(lines, vec![20, 23]);
    drop(set);

    // a new base: generation 3; generation 1 and the base it alone named are collected
    let b3 = tmp.path().join("b3.oxv");
    let s = TestSegment {
        dims: D,
        codec: TestCodec::I8Vec,
        delta: false,
        from: 0,
        to: 3,
        tag: "v3".into(),
        books: vec![TestBook { name: "id:1".into(), slots: vec![slot(1, 0, axis(D, 0))], ..Default::default() }],
        occurrences: vec![],
        tombstones: vec![],
    };
    oxv::write_segment(&b3, &s).unwrap();
    stamp(&b3, 0x33);
    assert_eq!(install(&dir, &b3, &manifest(&b3), None).unwrap().generation, 3);
    assert!(!dir.join("gen-000001").exists());
    assert!(dir.join(format!("segments/{id2}.oxv")).exists());
}

/// The real installed Otzaria set, verified read-only (run by hand:
/// `OTZARIA_VECTORS=%APPDATA%\otzaria\vectors cargo test -- --ignored real_`).
#[test]
#[ignore]
fn real_otzaria_segments_verify() {
    let Ok(dir) = std::env::var("OTZARIA_VECTORS") else { return };
    for e in fs::read_dir(Path::new(&dir).join("segments")).unwrap().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "oxv") {
            let m = fs::read_to_string(p.with_extension("package.json")).unwrap();
            let r = verify_segment(&p, Some(&m)).unwrap();
            eprintln!("{} {} blocks", r.segment_id, r.blocks);
        }
    }
}

/// The hosts' install fixtures (`tests/data/install/`, used by the Dart test and
/// the C# self-test): base v1, deltas v1-v2 and v2-v3, base v3, each as
/// `<name>.oxv.zst` + `<name>.manifest.json`. `REGEN_OXV=1` rewrites them.
#[test]
fn the_hosts_install_fixtures_are_current() {
    let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/install");
    let tmp = tempfile::tempdir().unwrap();
    let book = |name: &str, slots: Vec<TestSlot>, foreign: Vec<(u64, u32)>| TestBook { name: name.into(), slots, foreign, ..Default::default() };
    let seg = |delta: bool, from: u32, to: u32, tag: &str, books: Vec<TestBook>, tombstones: Vec<u64>| TestSegment {
        dims: 4,
        codec: TestCodec::I8Vec,
        delta,
        from,
        to,
        tag: tag.into(),
        books,
        occurrences: vec![],
        tombstones,
    };
    let all = [
        ("base-v1", 0x11, seg(false, 0, 1, "v1-20261101", vec![book("id:5", vec![slot(1, 0, axis(4, 0)), slot(2, 1, axis(4, 1))], vec![]), book("id:7", vec![slot(3, 0, axis(4, 2))], vec![])], vec![])),
        ("delta-v1-v2", 0x22, seg(true, 1, 2, "v2-20261102", vec![book("id:7", vec![slot(4, 0, axis(4, 3))], vec![(1, 1)])], vec![3])),
        ("delta-v2-v3", 0x33, seg(true, 2, 3, "v3-20261103", vec![book("id:5", vec![slot(5, 1, axis(4, 2))], vec![])], vec![2])),
        ("base-v3", 0x44, seg(false, 0, 3, "v3-20261103", vec![book("id:5", vec![slot(1, 0, axis(4, 0)), slot(5, 1, axis(4, 2))], vec![]), book("id:7", vec![slot(4, 0, axis(4, 3))], vec![])], vec![])),
    ];
    for (name, id, s) in &all {
        let p = tmp.path().join(format!("{name}.oxv"));
        oxv::write_segment(&p, s).unwrap();
        stamp(&p, *id);
        let zst = zstd::stream::encode_all(&fs::read(&p).unwrap()[..], 3).unwrap();
        let m = manifest(&p);
        let (zp, mp) = (out.join(format!("{name}.oxv.zst")), out.join(format!("{name}.manifest.json")));
        if std::env::var("REGEN_OXV").is_ok() {
            fs::create_dir_all(&out).unwrap();
            fs::write(&zp, &zst).unwrap();
            fs::write(&mp, &m).unwrap();
        }
        assert_eq!(fs::read(&zp).unwrap(), zst, "{name}");
        assert_eq!(fs::read_to_string(&mp).unwrap(), m, "{name}");
    }
    // the chain installs: base v1, then both deltas
    let dir = tmp.path().join("set");
    for name in ["base-v1", "delta-v1-v2", "delta-v2-v3"] {
        let p = tmp.path().join(format!("{name}.oxv"));
        install(&dir, &p, &fs::read_to_string(out.join(format!("{name}.manifest.json"))).unwrap(), None).unwrap();
    }
    let info = SegmentSet::open(&dir).unwrap().info(None);
    assert_eq!((info.library_version, info.segments.len(), info.slots_live), (3, 3, 3));
}
