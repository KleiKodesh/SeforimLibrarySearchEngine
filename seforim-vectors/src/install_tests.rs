use std::fs;
use std::path::{Path, PathBuf};

use super::{compaction_wanted, error_kind, install, Kind, install_with, sha256_hex, verify_segment, InstallOptions};
use crate::oxv::{self, OxvFile, TestBook, TestCodec, TestSegment, TestSlot};
use crate::set::{self, LineSearch, SegmentSet};
use crate::tests::{search, HintMap};

fn slot(key: u64, hint: u32, vector: Vec<f32>) -> TestSlot {
    TestSlot { key, hint, window: 0, vector }
}

fn axis(dims: usize, a: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    v[a] = 1.0;
    v
}

/// The identity every test segment is of.
fn identity(precision: &str) -> serde_json::Value {
    serde_json::json!({
        "text": {"line_text_version": 1, "key_version": 1},
        "model": {"family_id": "f", "tokenizer_checksum": "t", "embedding_dim": 4, "pooling": "in-graph", "max_tokens": 256,
            "embedding_text_version": 2, "normalization_version": 1, "chunking_identity": 7,
            "query_packages": [{"checksum": "c", "quantization": "fp32"}]},
        "store": {"backend_id": "otzaria-oxv", "store_format_version": 3, "vector_precision": precision}
    })
}

/// Gives a written segment an id (header bytes 32..48) and the test identity's
/// digest (48..80), and recomputes the header CRC.
fn stamp(path: &Path, id: u8) {
    let precision = OxvFile::open(path).unwrap().codec().name();
    let digest = super::identity_digest(&identity(precision)).unwrap();
    let mut b = fs::read(path).unwrap();
    b[32..48].copy_from_slice(&[id; 16]);
    for i in 0..32 {
        b[48 + i] = u8::from_str_radix(&digest[2 * i..2 * i + 2], 16).unwrap();
    }
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
    let mut m = serde_json::json!({
        "format": "otzaria-vectors-release", "formatVersion": 1, "kind": seg.kind(),
        "identity": identity(seg.codec().name()),
        "identityDigest": hex(48..80), "codecParamsSha256": hex(80..112),
        "fromLibraryVersion": from, "toLibraryVersion": to, "libraryReleaseTag": seg.release_tag(),
        "segmentId": hex(32..48),
        "counts": {"books": u32::from_le_bytes(b[144..148].try_into().unwrap()), "slots": u64_at(112),
            "extras": u64_at(120), "foreign": u64_at(128), "tombstones": u64_at(136)},
        "segment": {"sha256": sha256_hex(b), "size": b.len()},
        "packageDigest": "", "createdAt": "", "files": [],
        "provenance": {"passage_package": {"checksum": "c", "quantization": "fp32"}, "worker": {"backend": "test", "device": "cpu"}},
    });
    let parsed: super::ReleaseManifest = serde_json::from_value(m.clone()).unwrap();
    m["packageDigest"] = super::package_digest(&parsed).unwrap().into();
    m.to_string()
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

fn mapping() -> HintMap {
    HintMap::new(&[("id:1", 1, vec![10, 11]), ("id:2", 2, vec![20, 21, 22, 23, 24, 25])])
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
    assert_eq!(error_kind(&e.unwrap_err()), Some(Kind::Corrupt));
}

#[test]
fn base_then_delta_installs_as_otzaria_and_the_reader_opens_it() {
    let tmp = tempfile::tempdir().unwrap();
    let (b, x) = segments(tmp.path());
    let dir = tmp.path().join("vectors");
    // a delta first: refused
    assert_eq!(error_kind(&install(&dir, &x, &manifest(&x), None).unwrap_err()), Some(Kind::Incompatible));
    // the base from incoming/ (moved), the delta from elsewhere (copied)
    fs::create_dir_all(dir.join("incoming")).unwrap();
    let inc = dir.join("incoming/base.oxv");
    let mb = manifest(&b);
    fs::copy(&b, &inc).unwrap();
    assert_eq!(error_kind(&install(&dir, &inc, &mb, Some(&"0".repeat(64))).unwrap_err()), Some(Kind::NotPublished));
    let r = install(&dir, &inc, &mb, Some(&sha256_hex(mb.as_bytes()))).unwrap();
    assert_eq!((r.kind.as_str(), r.generation, r.library_version, r.segments), ("base", 1, 1, 1));
    assert!(!inc.exists());
    let r = install(&dir, &x, &manifest(&x), None).unwrap();
    assert_eq!((r.generation, r.library_version, r.segments), (2, 2, 2));
    assert_eq!((r.tombstones_applied, r.duplicates_removed, r.foreign_unresolved), (1, 1, 1));
    assert!(install(&dir, &x, &manifest(&x), None).unwrap().already_applied);
    // the report carries the sidecar's compaction rule on the installed set
    let cur: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("CURRENT")).unwrap()).unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&fs::read(dir.join(cur["set"].as_str().unwrap())).unwrap()).unwrap();
    let sizes: Vec<u64> = doc["segments"].as_array().unwrap().iter().map(|s| s["size"].as_u64().unwrap()).collect();
    let want = compaction_wanted(&sizes, doc["stats"]["slots"].as_u64().unwrap(), doc["stats"]["slots_dead"].as_u64().unwrap());
    assert_eq!(r.needs_compaction, want);

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
        let ours = search(&set, &axis(D, a), &map, &req).unwrap();
        let theirs = search(&hand, &axis(D, a), &map, &req).unwrap();
        assert_eq!(ours.hits, theirs.hits, "axis {a}");
    }
    let r = search(&set, &axis(D, 3), &map, &LineSearch { k: 2, threads: 1, ..Default::default() }).unwrap();
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

#[test]
fn digests_are_recomputed_and_a_crash_is_recovered() {
    let tmp = tempfile::tempdir().unwrap();
    let (b, _) = segments(tmp.path());
    let m = manifest(&b);
    // another identity (the digest no longer its own), another package digest: refused
    let other = m.replace("\"family_id\":\"f\"", "\"family_id\":\"g\"");
    assert_ne!(other, m);
    let e = verify_segment(&b, Some(&other)).unwrap_err().to_string();
    assert!(e.contains("identity digest"), "{e}");
    let v: serde_json::Value = serde_json::from_str(&m).unwrap();
    let pd = v["packageDigest"].as_str().unwrap();
    let e = verify_segment(&b, Some(&m.replace(pd, &"0".repeat(64)))).unwrap_err().to_string();
    assert!(e.contains("package digest"), "{e}");

    // a crash left staging/, a temporary pointer and an unnamed generation: recovered
    let dir = tmp.path().join("v");
    install(&dir, &b, &m, None).unwrap();
    fs::create_dir_all(dir.join("staging")).unwrap();
    fs::write(dir.join("staging/x.oxv"), b"half").unwrap();
    fs::write(dir.join("CURRENT.123.tmp"), b"{").unwrap();
    fs::create_dir_all(dir.join("gen-000009")).unwrap();
    assert!(install(&dir, &b, &m, None).is_ok());
    assert!(!dir.join("staging").exists());
    assert!(!dir.join("CURRENT.123.tmp").exists());
    assert!(!dir.join("gen-000009").exists());
    assert!(SegmentSet::open(&dir).is_ok());
}

#[test]
fn the_model_identity_types_and_cancellation_are_checked() {
    let tmp = tempfile::tempdir().unwrap();
    let (b, x) = segments(tmp.path());
    let (mb, mx) = (manifest(&b), manifest(&x));
    let dir = tmp.path().join("v");
    let with = |seg: &Path, m: &str, model: Option<&str>, cancel: Option<&std::sync::atomic::AtomicBool>| {
        install_with(&dir, seg, m, &InstallOptions { expected_model_json: model, cancel, ..Default::default() })
    };
    // the query model's model.json: another family or chunking is refused
    let e = with(&b, &mb, Some(r#"{"family_id":"g","chunking_identity":7}"#), None).unwrap_err();
    assert_eq!(error_kind(&e), Some(Kind::Incompatible));
    let e = e.to_string();
    assert!(e.contains("model.family_id"), "{e}");
    let e = with(&b, &mb, Some(r#"{"family_id":"f","chunking_identity":8}"#), None).unwrap_err().to_string();
    assert!(e.contains("model.chunking_identity"), "{e}");
    let e = with(&b, &mb, Some(r#"{"family_id":"f","chunking_identity":7,"query_packages":[{"checksum":"other","quantization":"int8"}]}"#), None);
    assert!(e.is_err());
    let e = with(&b, &mb, Some(r#"{"chunking_identity":7}"#), None).unwrap_err();
    assert_eq!(error_kind(&e), Some(Kind::Invalid), "family_id required");
    assert!(!dir.join("CURRENT").exists());
    // a number written as text is refused, as the sidecar's typed identity does
    let texted = mb.replace("\"embedding_dim\":4", "\"embedding_dim\":\"4\"");
    assert_ne!(texted, mb);
    let e = verify_segment(&b, Some(&texted)).unwrap_err().to_string();
    assert!(e.contains("model.embedding_dim"), "{e}");
    // the right model installs
    let model = r#"{"family_id":"f","chunking_identity":7,"embedding_dim":4,"query_packages":[{"checksum":"c","quantization":"fp32"}]}"#;
    with(&b, &mb, Some(model), None).unwrap();
    // a cancelled delta install leaves the set as it was, nothing staged
    let stop = std::sync::atomic::AtomicBool::new(true);
    assert!(matches!(with(&x, &mx, Some(model), Some(&stop)), Err(crate::Error::Cancelled)));
    assert!(!dir.join("staging").exists());
    assert!(!dir.join(format!("segments/{}.oxv", "22".repeat(16))).exists());
    assert_eq!(SegmentSet::open(&dir).unwrap().info(None).library_version, 1);
    let stop = std::sync::atomic::AtomicBool::new(true);
    assert!(matches!(super::verify_segment_cancellable(&b, None, Some(&stop)), Err(crate::Error::Cancelled)));
    // not cancelled: it applies
    assert_eq!(with(&x, &mx, Some(model), None).unwrap().library_version, 2);
}

#[test]
fn compaction_rule_is_the_sidecars_default_policy() {
    assert!(!compaction_wanted(&[], 0, 0));
    assert!(!compaction_wanted(&[1000, 100, 100], 100, 5));
    assert!(compaction_wanted(&[1000, 150, 51], 100, 0)); // deltas over 20 % of the base
    assert!(!compaction_wanted(&[1000, 200], 100, 0)); // exactly 20 %: not over
    assert!(compaction_wanted(&[1000, 1, 1, 1, 1], 100, 0)); // 5 segments, more than 4
    assert!(compaction_wanted(&[1000], 100, 6)); // over 5 % dead
    assert!(!compaction_wanted(&[1000], 100, 5));
}

/// Installs the base and the delta of [`segments`] into `<tmp>/vectors`.
fn installed(tmp: &Path) -> PathBuf {
    fs::create_dir_all(tmp).unwrap();
    let (b, x) = segments(tmp);
    let dir = tmp.join("vectors");
    install(&dir, &b, &manifest(&b), None).unwrap();
    install(&dir, &x, &manifest(&x), None).unwrap();
    dir
}

fn all_hits(dir: &Path, map: &HintMap) -> Vec<Vec<set::LineHit>> {
    let set = SegmentSet::open(dir).unwrap();
    (0..D)
        .map(|a| search(&set, &axis(D, a), map, &LineSearch { k: 16, threads: 1, ..Default::default() }).unwrap().hits)
        .collect()
}

#[test]
fn compaction_merges_the_set_into_one_segment_that_searches_the_same() {
    use super::compact::{compact, CompactionPolicy};
    let tmp = tempfile::tempdir().unwrap();
    let dir = installed(tmp.path());
    let map = mapping();
    let before = all_hits(&dir, &map);

    // the default policy: 2 segments, deltas well under... whatever it says, the
    // report says the same as the install's rule
    let doc = |dir: &Path| -> serde_json::Value {
        let cur: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("CURRENT")).unwrap()).unwrap();
        serde_json::from_slice(&fs::read(dir.join(cur["set"].as_str().unwrap())).unwrap()).unwrap()
    };
    let d = doc(&dir);
    let sizes: Vec<u64> = d["segments"].as_array().unwrap().iter().map(|s| s["size"].as_u64().unwrap()).collect();
    let (slots, dead) = (d["stats"]["slots"].as_u64().unwrap(), d["stats"]["slots_dead"].as_u64().unwrap());
    assert_eq!(CompactionPolicy::default().wants(&sizes, slots - dead, dead).is_some(), compaction_wanted(&sizes, slots, dead));
    let quiet = CompactionPolicy { max_segments: 10, max_delta_ratio: 100.0, max_dead_ratio: 1.0, ..Default::default() };
    let r = compact(&dir, &quiet, None, None).unwrap();
    assert!(!r.compacted);
    assert_eq!(r.generation, 2);

    // a cancelled compaction leaves the set as it was
    let stop = std::sync::atomic::AtomicBool::new(true);
    let forced = CompactionPolicy { force: true, ..Default::default() };
    assert!(matches!(compact(&dir, &forced, None, Some(&stop)), Err(crate::Error::Cancelled)));
    assert_eq!(doc(&dir)["generation"], 2);
    assert!(!dir.join("staging").exists());

    let r = compact(&dir, &forced, None, None).unwrap();
    assert!(r.compacted, "{r:?}");
    assert_eq!((r.generation, r.slots_before, r.slots_after, r.records_pruned, r.hints_refreshed), (3, 6, 4, 0, 0));
    let d = doc(&dir);
    assert_eq!(d["segments"].as_array().unwrap().len(), 1);
    assert_eq!(d["segments"][0]["kind"], "compacted");
    assert_eq!((d["library_version"].as_u64(), d["stats"]["slots_dead"].as_u64()), (Some(2), Some(0)));
    let id = d["segments"][0]["id"].as_str().unwrap().to_string();
    let seg = OxvFile::open(&dir.join(format!("segments/{id}.oxv"))).unwrap();
    assert_eq!((seg.kind(), seg.library_versions()), ("compacted", (0, 2)));
    assert!(super::verify_blocks(seg.bytes(), None).is_ok());
    drop(seg);
    let prev: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("PREVIOUS")).unwrap()).unwrap();
    assert_eq!(prev["generation"], 2);
    // Otzaria's rule (segment_set/mod.rs `book_occurrences`): a newer segment's records
    // of a key in a book replace the older ones there. The delta lists key 4 in id:2 at
    // line 3 only, so its base record at line 0 (line id 20) is not carried over; every
    // other hit is the same.
    let without_20 = |hits: Vec<Vec<set::LineHit>>| -> Vec<Vec<set::LineHit>> {
        hits.into_iter().map(|h| h.into_iter().filter(|x| x.line_id != 20).collect()).collect()
    };
    let after = all_hits(&dir, &map);
    assert!(after.iter().flatten().all(|h| h.line_id != 20));
    assert_eq!(after, without_20(before));

    // the same compaction again names the same bytes (its id is its content)
    let again = tmp.path().join("again");
    let dir2 = installed(&again);
    compact(&dir2, &forced, None, None).unwrap();
    assert_eq!(doc(&dir2)["segments"][0]["id"], id.as_str());
}

/// A live index of the set's version: in id:1 key 1 is gone; in id:2 key 4 moved
/// from line 3 to line 7 (keys 6, 1 and 2 where they were).
struct Live;

impl super::compact::LiveKeySource for Live {
    fn library_version(&self) -> u32 {
        2
    }
    fn book_keys(&self, book: &str, out: &mut Vec<(u32, u64)>) -> crate::Result<bool> {
        out.clear();
        match book {
            "id:1" => out.extend([(0, 0), (1, 0)]),
            "id:2" => out.extend([(0, 0), (1, 6), (2, 1), (3, 0), (4, 2), (7, 4)]),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

#[test]
fn compaction_reanchors_on_the_live_index_of_the_sets_version() {
    use super::compact::{compact, CompactionPolicy};
    let tmp = tempfile::tempdir().unwrap();
    let dir = installed(tmp.path());
    let forced = CompactionPolicy { force: true, ..Default::default() };
    let r = compact(&dir, &forced, Some(&Live), None).unwrap();
    assert_eq!((r.records_pruned, r.hints_refreshed), (1, 1));
    let map = HintMap::new(&[("id:1", 1, vec![10, 11]), ("id:2", 2, vec![20, 21, 22, 23, 24, 25, 26, 27])]);
    let set = SegmentSet::open(&dir).unwrap();
    let search = |a: usize| crate::tests::search(&set, &axis(D, a), &map, &LineSearch { k: 16, threads: 1, ..Default::default() }).unwrap().hits;
    let top = |a: usize| -> Vec<u64> { search(a).iter().filter(|h| h.score > 0.5).map(|h| h.line_id).collect() };
    // key 4 re-anchored on line 7
    assert_eq!(top(3), vec![27]);
    // key 1 pruned from id:1 (line id 10), kept in id:2 at line 2
    assert_eq!(top(0), vec![22]);

    // another library version: records kept as they are
    let other = tmp.path().join("other");
    let dir2 = installed(&other);
    struct Old;
    impl super::compact::LiveKeySource for Old {
        fn library_version(&self) -> u32 {
            1
        }
        fn book_keys(&self, _: &str, _: &mut Vec<(u32, u64)>) -> crate::Result<bool> {
            panic!("not asked on another version")
        }
    }
    let r = compact(&dir2, &forced, Some(&Old), None).unwrap();
    assert_eq!((r.records_pruned, r.hints_refreshed), (0, 0));
}

#[test]
fn compaction_refuses_a_damaged_source_and_writes_nothing() {
    use super::compact::{compact, CompactionPolicy};
    let tmp = tempfile::tempdir().unwrap();
    let dir = installed(tmp.path());
    // flip a byte in the vectors of the installed delta
    let id2 = "22".repeat(16);
    let path = dir.join(format!("segments/{id2}.oxv"));
    let mut bytes = fs::read(&path).unwrap();
    let at = (0..u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize)
        .map(|i| 224 + i * 40)
        .find(|&at| u16::from_le_bytes([bytes[at], bytes[at + 1]]) == 0x0012)
        .map(|at| u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap()) as usize)
        .unwrap();
    bytes[at] ^= 0x40;
    fs::write(&path, bytes).unwrap();
    let e = compact(&dir, &CompactionPolicy { force: true, ..Default::default() }, None, None).unwrap_err();
    assert_eq!(error_kind(&e), Some(Kind::Corrupt));
    let cur: serde_json::Value = serde_json::from_slice(&fs::read(dir.join("CURRENT")).unwrap()).unwrap();
    assert_eq!(cur["generation"], 2);
    assert!(!dir.join("gen-000003").exists() && !dir.join("staging").exists());
    assert_eq!(fs::read_dir(dir.join("segments")).unwrap().filter(|e| e.as_ref().unwrap().path().extension().is_some_and(|x| x == "oxv")).count(), 2);
}

#[test]
fn a_zstd_segment_is_expanded_into_incoming_and_installed() {
    // otzaria_search_engine `expand_segment`: Otzaria's app downloads `.oxv.zst`
    let tmp = tempfile::tempdir().unwrap();
    let (b, x) = segments(tmp.path());
    let dir = tmp.path().join("v");
    let zst = |plain: &Path| {
        let out = plain.with_extension("oxv.zst");
        fs::write(&out, zstd::encode_all(&fs::read(plain).unwrap()[..], 3).unwrap()).unwrap();
        out
    };
    let (bz, xz) = (zst(&b), zst(&x));
    // a damaged stream: corrupt, nothing installed, nothing left in incoming/
    let bad = tmp.path().join("bad.oxv.zst");
    let mut bytes = fs::read(&bz).unwrap();
    let mid = bytes.len() / 2;
    bytes.truncate(mid);
    fs::write(&bad, &bytes).unwrap();
    let e = install(&dir, &bad, &manifest(&b), None).unwrap_err();
    assert_eq!(error_kind(&e), Some(Kind::Corrupt), "{e}");
    fs::write(&bad, b"not zstd at all").unwrap();
    assert_eq!(error_kind(&install(&dir, &bad, &manifest(&b), None).unwrap_err()), Some(Kind::Corrupt));
    assert!(!dir.join("CURRENT").exists());
    let left = || fs::read_dir(dir.join("incoming")).unwrap().count();
    assert_eq!(left(), 0);
    // a cancelled expansion leaves nothing either
    let stop = std::sync::atomic::AtomicBool::new(true);
    let opts = InstallOptions { cancel: Some(&stop), ..Default::default() };
    assert!(matches!(install_with(&dir, &bz, &manifest(&b), &opts), Err(crate::Error::Cancelled)));
    assert_eq!(left(), 0);
    // an abandoned expansion (lock file nothing holds) is cleaned by the next install
    fs::write(dir.join("incoming/.expanding-dead.lock"), b"").unwrap();
    fs::write(dir.join("incoming/.expanding-dead.oxv"), b"junk").unwrap();
    // base then delta, both compressed; the downloads stay, incoming/ is empty after
    let r = install(&dir, &bz, &manifest(&b), None).unwrap();
    assert_eq!((r.generation, r.library_version), (1, 1));
    let r = install(&dir, &xz, &manifest(&x), None).unwrap();
    assert_eq!((r.generation, r.library_version, r.segments), (2, 2, 2));
    assert!(bz.exists() && xz.exists());
    assert_eq!(left(), 0);
    assert!(dir.join(format!("segments/{}.oxv", "22".repeat(16))).exists());
    // a zst whose content is not the manifest's segment: corrupt
    let e = install(&tmp.path().join("v2"), &xz, &manifest(&b), None).unwrap_err();
    assert_eq!(error_kind(&e), Some(Kind::Corrupt), "{e}");
    assert_eq!(fs::read_dir(tmp.path().join("v2/incoming")).unwrap().count(), 0);
}
