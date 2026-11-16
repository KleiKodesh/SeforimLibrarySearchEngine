//! Installing a downloaded vector segment into an Otzaria vector set (issue #18):
//! a port of the install half of Otzaria's sidecar (`otzaria-semantic-search`
//! d2a782a, `src/semantic/segment_set/{install,files,mod}.rs` and
//! `oxv/reader.rs` `verify_blocks`), format only (no network, no database).
//!
//! - [`verify_segment`]: the segment's header and directory (checked at open by
//!   [`OxvFile::open`]), every 1 MiB block of its block-summed sections against
//!   `BLOCK_CRCS`, and, given its release manifest, the sidecar's `check_header`.
//! - [`install`]: `install_package`: a base starts a new set, a delta goes on the
//!   current generation (its tombstones and the keys it ships mark older slots
//!   dead, its foreign records are linked to the live older slot holding their
//!   key), written as a new generation exactly as the sidecar writes it
//!   (`gen-NNNNNN/set.json`, `.del` per segment, `.links` per delta, `PREVIOUS`
//!   then `CURRENT`, then the garbage of unnamed generations and segments).
//!
//! Not ported: the sidecar's recovery of an interrupted install (`recover`),
//! the directory fsyncs (`sync_set_dir`; files are flushed, renames are not),
//! the expected model identity (the host checks it when it picks the release)
//! and the recomputation of the identity and package digests (canonical hashing
//! of the sidecar's typed identity); the manifest's published SHA-256 is checked
//! when the host passes it.
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::oxv::{Key, OxvFile};
use crate::scan::{Link, LINK_UNRESOLVED};
use crate::set::{encode_deleted_words, encode_link_list, SegmentSet};
use crate::{Error, Result};

const RELEASE_FORMAT: &str = "otzaria-vectors-release";
const SET_FORMAT: &str = "otzaria-vector-set";
const BLOCK_SIZE: usize = 1 << 20;
const S_BLOCK_CRCS: u16 = 0x007F;
const SECTION_BLOCKSUMMED: u16 = 2;

fn bad(msg: impl Into<String>) -> Error {
    Error::Format(format!("vector install: {}", msg.into()))
}

/// install.rs `ReleaseManifest` (the fields an install reads; identity and
/// provenance kept as they are).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReleaseManifest {
    format: String,
    format_version: u32,
    kind: String,
    identity: Value,
    identity_digest: String,
    codec_params_sha256: String,
    #[serde(default)]
    from_library_version: u32,
    to_library_version: u32,
    library_release_tag: String,
    segment_id: String,
    counts: Counts,
    segment: ReleaseSegment,
    package_digest: String,
    #[serde(default)]
    provenance: Value,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct Counts {
    #[serde(default)]
    books: u64,
    slots: u64,
    #[serde(default)]
    extras: u64,
    #[serde(default)]
    foreign: u64,
    #[serde(default)]
    tombstones: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseSegment {
    sha256: String,
    size: u64,
}

/// files.rs `Pointer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Pointer {
    generation: u64,
    set: String,
    set_sha256: String,
}

/// files.rs `SetDocument`, field for field (serialized in the sidecar's order).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SetDocument {
    format: String,
    format_version: u32,
    generation: u64,
    identity: Value,
    identity_digest: String,
    codec_params_sha256: String,
    library_version: u32,
    library_release_tag: String,
    segments: Vec<SetSegment>,
    stats: SetStats,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SetSegment {
    id: String,
    file: String,
    kind: String,
    from: u32,
    to: u32,
    sha256: String,
    size: u64,
    slots: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    package_digest: Option<String>,
    #[serde(default)]
    provenance: Value,
    del: DerivedFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    links: Option<DerivedFile>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DerivedFile {
    file: String,
    crc32: u32,
    count: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct SetStats {
    slots: u64,
    slots_dead: u64,
    foreign: u64,
    foreign_unresolved: u64,
    bytes: u64,
}

/// What [`verify_segment`] checked (JSON at the C ABI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub segment_id: String,
    pub kind: String,
    pub from_library_version: u32,
    pub to_library_version: u32,
    pub slots: u64,
    /// Blocks whose CRC was checked.
    pub blocks: u64,
    /// True when a manifest was given and the header agrees with it.
    pub manifest_checked: bool,
}

/// What [`install`] did (install.rs `ApplyReport`; JSON at the C ABI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReport {
    pub kind: String,
    pub library_version: u32,
    pub generation: u64,
    pub segments: u32,
    pub slots_added: u64,
    pub tombstones_applied: u64,
    pub duplicates_removed: u64,
    pub foreign_unresolved: u64,
    pub bytes_on_disk: u64,
    pub already_applied: bool,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex(&sha2::Sha256::digest(bytes))
}

fn hash_file(path: &Path) -> Result<String> {
    use sha2::Digest;
    let mut file = File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// reader.rs `verify_blocks`: each 1 MiB block of the block-summed sections, in
/// directory order, against `BLOCK_CRCS`. Returns the blocks checked.
fn verify_blocks(b: &[u8]) -> Result<u64> {
    let sections = u32_at(b, 28) as usize;
    let mut summed = Vec::new();
    let mut crcs = None;
    for i in 0..sections {
        let at = 224 + i * 40;
        let (kind, flags) = (u16_at(b, at), u16_at(b, at + 2));
        let (off, len, count) = (u64_at(b, at + 8) as usize, u64_at(b, at + 16) as usize, u64_at(b, at + 24));
        if kind == S_BLOCK_CRCS {
            crcs = Some((off..off + len, count));
        } else if flags & SECTION_BLOCKSUMMED != 0 {
            summed.push((kind, off..off + len));
        }
    }
    let blocks: u64 = summed.iter().map(|(_, r)| r.len().div_ceil(BLOCK_SIZE) as u64).sum();
    let Some((crcs, count)) = crcs else {
        return Err(bad("the segment has no BLOCK_CRCS section"));
    };
    if count != blocks || crcs.len() as u64 != blocks * 4 {
        return Err(bad(format!("it carries {count} block CRC(s) for {blocks} block(s)")));
    }
    let crcs = &b[crcs];
    let mut index = 0;
    for (kind, range) in summed {
        for (n, block) in b[range].chunks(BLOCK_SIZE).enumerate() {
            let (actual, expected) = (crc32fast::hash(block), u32_at(crcs, index * 4));
            if actual != expected {
                return Err(bad(format!(
                    "block {n} of section {kind:#06x} hashes to CRC {actual:08x}, and the segment declares {expected:08x}"
                )));
            }
            index += 1;
        }
    }
    Ok(blocks)
}

/// install.rs `check_header`.
fn check_header(m: &ReleaseManifest, seg: &OxvFile) -> Result<()> {
    let b = seg.bytes();
    let disagree = |what: &str, header: String, declared: String| {
        Err(bad(format!("the segment's {what} is {header}, and its manifest declares {declared}")))
    };
    if hex(&b[32..48]) != m.segment_id {
        return disagree("id", hex(&b[32..48]), m.segment_id.clone());
    }
    if hex(&b[48..80]) != m.identity_digest {
        return disagree("identity digest", hex(&b[48..80]), m.identity_digest.clone());
    }
    if hex(&b[80..112]) != m.codec_params_sha256 {
        return disagree("codec epoch", hex(&b[80..112]), m.codec_params_sha256.clone());
    }
    if let Some(p) = m.identity.pointer("/store/vector_precision").and_then(Value::as_str) {
        if seg.codec().name() != p {
            return disagree("codec", seg.codec().name().into(), p.into());
        }
    }
    if let Some(v) = m.identity.pointer("/store/store_format_version").and_then(Value::as_u64) {
        if seg.format() as u64 != v {
            return disagree("segment format", seg.format().to_string(), v.to_string());
        }
    }
    let (from, to) = seg.library_versions();
    if seg.kind() != m.kind || from != m.from_library_version || to != m.to_library_version {
        return disagree(
            "place in the chain",
            format!("{} {from}-{to}", seg.kind()),
            format!("{} {}-{}", m.kind, m.from_library_version, m.to_library_version),
        );
    }
    let counts = (u32_at(b, 144) as u64, u64_at(b, 112), u64_at(b, 120), u64_at(b, 128), u64_at(b, 136));
    let declared = (m.counts.books, m.counts.slots, m.counts.extras, m.counts.foreign, m.counts.tombstones);
    if counts != declared {
        return disagree("counts", format!("{counts:?}"), format!("{declared:?}"));
    }
    if seg.release_tag() != m.library_release_tag {
        return disagree("release tag", seg.release_tag().into(), m.library_release_tag.clone());
    }
    Ok(())
}

fn parse_manifest(json: &str) -> Result<ReleaseManifest> {
    let m: ReleaseManifest =
        serde_json::from_str(json).map_err(|e| bad(format!("its release manifest is not one this build reads: {e}")))?;
    if m.format != RELEASE_FORMAT || m.format_version != 1 {
        return Err(bad(format!("it is {} version {}, and this build reads {RELEASE_FORMAT} version 1", m.format, m.format_version)));
    }
    if m.segment_id.len() != 32 || !m.segment_id.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return Err(bad("a segment id is 32 lowercase hex digits"));
    }
    match m.kind.as_str() {
        "base" | "delta" => Ok(m),
        "compacted" => Err(bad("a compacted segment is made on a device and never published")),
        k => Err(bad(format!("kind {k}"))),
    }
}

/// Verifies a plain `.oxv`: header and directory, every block CRC and, given
/// its release manifest, that the header is the one the manifest describes.
pub fn verify_segment(path: &Path, manifest_json: Option<&str>) -> Result<VerifyReport> {
    let seg = OxvFile::open(path)?;
    let blocks = verify_blocks(seg.bytes())?;
    if let Some(json) = manifest_json {
        check_header(&parse_manifest(json)?, &seg)?;
    }
    let (from, to) = seg.library_versions();
    Ok(VerifyReport {
        segment_id: hex(&seg.bytes()[32..48]),
        kind: seg.kind().into(),
        from_library_version: from,
        to_library_version: to,
        slots: seg.slot_count() as u64,
        blocks,
        manifest_checked: manifest_json.is_some(),
    })
}

/// files.rs `SetLock`: one install at a time, never waiting behind another.
struct SetLock(File);

impl SetLock {
    fn take(dir: &Path) -> Result<SetLock> {
        fs::create_dir_all(dir)?;
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(".lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(SetLock(file)),
            Err(fs::TryLockError::WouldBlock) => {
                Err(bad("another install or compaction of this vector set is running"))
            }
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

impl Drop for SetLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// files.rs `write_atomically`: a temporary file, flushed, renamed into place.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    let placed = (|| -> std::io::Result<()> {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir(path)?;
        }
        fs::rename(&tmp, path)
    })();
    if placed.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(placed?)
}

fn read_pointer(dir: &Path, name: &str) -> std::result::Result<Option<Pointer>, ()> {
    match fs::read(dir.join(name)) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|_| ()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(()),
    }
}

fn read_generation(dir: &Path, p: &Pointer) -> Option<SetDocument> {
    let bytes = fs::read(dir.join(&p.set)).ok()?;
    if sha256_hex(&bytes) != p.set_sha256.to_ascii_lowercase() {
        return None;
    }
    let d: SetDocument = serde_json::from_slice(&bytes).ok()?;
    (d.format == SET_FORMAT && d.generation == p.generation).then_some(d)
}

fn gen_dir(generation: u64) -> String {
    format!("gen-{generation:06}")
}

/// files.rs `next_generation`.
fn next_generation(dir: &Path) -> Result<u64> {
    let mut highest = 0;
    for e in fs::read_dir(dir)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if let Some(n) = name.strip_prefix("gen-").filter(|d| !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit())) {
            highest = highest.max(n.parse::<u64>().unwrap_or(0));
        }
    }
    for name in ["CURRENT", "PREVIOUS"] {
        if let Ok(Some(p)) = read_pointer(dir, name) {
            highest = highest.max(p.generation);
        }
    }
    highest.checked_add(1).ok_or_else(|| bad("no generation number is left"))
}

/// install.rs `falls_back_to`: a host opening a set of `newer` can fall back to
/// one of `older` (a query package in common, the fp32 passage package kept when
/// `newer` has a query length, otherwise only query packages or the store differ).
fn falls_back_to(newer: &Value, older: &Value) -> bool {
    let empty = Vec::new();
    let pk = |v: &Value| v.pointer("/model/query_packages").and_then(Value::as_array).cloned().unwrap_or_else(|| empty.clone());
    let (np, op) = (pk(newer), pk(older));
    let shares = np.iter().any(|a| op.iter().any(|b| a.get("checksum") == b.get("checksum")));
    let passage = newer.pointer("/model/query_max_tokens").is_none_or(Value::is_null)
        || np.iter().any(|a| a.get("quantization").and_then(Value::as_str) == Some("fp32") && op.contains(a));
    let strip = |v: &Value| {
        let mut v = v.clone();
        if let Some(o) = v.as_object_mut() {
            o.remove("store");
            if let Some(m) = o.get_mut("model").and_then(Value::as_object_mut) {
                m.remove("query_packages");
            }
        }
        v
    };
    shares && passage && strip(newer) == strip(older)
}

/// mod.rs `collect_garbage`: generations and segment files neither pointer names.
fn collect_garbage(dir: &Path) {
    let pointers = [read_pointer(dir, "CURRENT"), read_pointer(dir, "PREVIOUS")];
    if matches!(pointers, [Ok(None), Ok(Some(_))]) {
        return;
    }
    let (mut gens, mut segs) = (BTreeSet::new(), BTreeSet::new());
    for p in pointers {
        match p {
            Ok(Some(p)) => match read_generation(dir, &p) {
                Some(d) => {
                    gens.insert(gen_dir(p.generation));
                    segs.extend(d.segments.into_iter().map(|s| s.id));
                }
                None => return,
            },
            Ok(None) => {}
            Err(()) => return,
        }
    }
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with("gen-") && !gens.contains(&name) {
            let _ = fs::remove_dir_all(e.path());
        }
    }
    let Ok(entries) = fs::read_dir(dir.join("segments")) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !segs.contains(name.split('.').next().unwrap_or_default()) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// install.rs `install_package`: installs the plain `.oxv` at `segment` (as
/// `manifest_json` describes it; `published_sha256`: the manifest's published
/// SHA-256, checked when given) into the set at `dir`. A segment inside
/// `<dir>/incoming/` is moved into the set, any other copied.
pub fn install(dir: &Path, segment: &Path, manifest_json: &str, published_sha256: Option<&str>) -> Result<InstallReport> {
    let _lock = SetLock::take(dir)?;
    if let Some(p) = published_sha256 {
        let actual = sha256_hex(manifest_json.as_bytes());
        if actual != p {
            return Err(bad(format!("the manifest is {actual}, and the published one {p}")));
        }
    }
    let m = parse_manifest(manifest_json)?;

    // The generation it goes on (CURRENT, else PREVIOUS, as the set opens).
    let base_pointer = [read_pointer(dir, "CURRENT"), read_pointer(dir, "PREVIOUS")]
        .into_iter()
        .filter_map(|p| p.ok().flatten())
        .find(|p| read_generation(dir, p).is_some());
    let current = match (&base_pointer, m.kind.as_str()) {
        (None, "delta") => return Err(bad("a delta needs an installed base for it to apply to")),
        (Some(p), "delta") => Some((read_generation(dir, p).unwrap(), SegmentSet::open(dir)?)),
        _ => None,
    };
    if let Some((doc, set)) = &current {
        if set.generation() != doc.generation {
            return Err(bad("the set opened another generation than its pointer names"));
        }
        if m.identity_digest != doc.identity_digest {
            return Err(bad(format!("its identity digest is {}, and the set's is {}", m.identity_digest, doc.identity_digest)));
        }
        if m.codec_params_sha256 != doc.codec_params_sha256 {
            return Err(bad(format!("it is of codec epoch {}, and the set of {}", m.codec_params_sha256, doc.codec_params_sha256)));
        }
        if m.to_library_version <= doc.library_version {
            return Ok(InstallReport {
                kind: m.kind.clone(),
                library_version: doc.library_version,
                generation: doc.generation,
                segments: doc.segments.len() as u32,
                slots_added: 0,
                tombstones_applied: 0,
                duplicates_removed: 0,
                foreign_unresolved: 0,
                bytes_on_disk: doc.stats.bytes,
                already_applied: true,
            });
        }
        if m.from_library_version != doc.library_version {
            return Err(bad(format!(
                "it goes from version {} to {}, and the set stands at {}",
                m.from_library_version, m.to_library_version, doc.library_version
            )));
        }
    }

    // Stage: take it from incoming/, else copy it; then size, hash, header, blocks.
    let incoming = dir.join("incoming");
    let taking = segment.parent().and_then(|p| p.canonicalize().ok()).zip(incoming.canonicalize().ok()).is_some_and(|(a, b)| a == b);
    let staged: PathBuf = if taking {
        segment.to_path_buf()
    } else {
        let staging = dir.join("staging");
        fs::create_dir_all(&staging)?;
        let staged = staging.join(format!("{}.oxv", m.segment_id));
        fs::copy(segment, &staged)?;
        staged
    };
    let checked = (|| -> Result<(String, OxvFile)> {
        let size = fs::metadata(&staged)?.len();
        if size != m.segment.size {
            return Err(bad(format!("the segment is {size} bytes, and its manifest declares {}", m.segment.size)));
        }
        let sha = hash_file(&staged)?;
        if sha != m.segment.sha256 {
            return Err(bad(format!("the segment hashes to {sha}, and its manifest declares {}", m.segment.sha256)));
        }
        let new_seg = OxvFile::open(&staged)?;
        check_header(&m, &new_seg)?;
        verify_blocks(new_seg.bytes())?;
        Ok((sha, new_seg))
    })();
    let (sha, new_seg) = match checked {
        Ok(v) => v,
        Err(e) => {
            if !taking {
                let _ = fs::remove_file(&staged);
            }
            return Err(e);
        }
    };

    // The new generation's segments, dead slots and links (NewGeneration::push).
    let mut entries: Vec<SetSegment> = Vec::new();
    let mut deleted: Vec<Vec<u64>> = Vec::new();
    let mut links: Vec<Option<Vec<Link>>> = Vec::new();
    let (mut tombstones_applied, mut duplicates_removed, mut foreign_unresolved) = (0u64, 0u64, 0u64);
    let identity;
    if let Some((doc, set)) = &current {
        identity = doc.identity.clone();
        if doc.segments.iter().any(|s| s.id == m.segment_id) {
            return Err(bad(format!("segment {} is in the generation already", m.segment_id)));
        }
        let (older, old_deleted, old_links) = set.parts();
        entries = doc.segments.clone();
        deleted = old_deleted.to_vec();
        links = doc.segments.iter().zip(old_links).map(|(e, l)| e.links.as_ref().map(|_| l.clone())).collect();
        #[derive(Default)]
        struct Want {
            tombstone: bool,
            shipped: bool,
            foreign: Vec<u32>,
        }
        let mut wants: HashMap<Key, Want> = HashMap::new();
        for i in 0..new_seg.tombstone_count() {
            wants.entry(new_seg.tombstone(i)).or_default().tombstone = true;
        }
        for slot in 0..new_seg.slot_count() {
            let want = wants.entry(new_seg.key(slot)).or_default();
            if want.shipped {
                return Err(bad(format!("segment {} ships key {} twice", m.segment_id, hex(&new_seg.key(slot)))));
            }
            want.shipped = true;
        }
        let foreign = new_seg.foreign_count();
        for i in 0..foreign {
            wants.entry(new_seg.foreign_record(i).0).or_default().foreign.push(i);
        }
        let mut new_links = vec![Link { seg: 0, flags: LINK_UNRESOLVED, slot: 0 }; foreign as usize];
        for (seg, older) in older.iter().enumerate() {
            let dead = &mut deleted[seg];
            for slot in 0..older.slot_count() {
                let (w, bit) = (slot as usize / 64, 1u64 << (slot % 64));
                if dead[w] & bit != 0 {
                    continue;
                }
                let Some(want) = wants.get(&older.key(slot)) else { continue };
                if want.tombstone {
                    dead[w] |= bit;
                    tombstones_applied += 1;
                } else if want.shipped {
                    dead[w] |= bit;
                    duplicates_removed += 1;
                }
                if dead[w] & bit == 0 {
                    for &i in &want.foreign {
                        new_links[i as usize] = Link { seg: seg as u16, flags: 0, slot };
                    }
                }
            }
        }
        foreign_unresolved = new_links.iter().filter(|l| l.flags & LINK_UNRESOLVED != 0).count() as u64;
        links.push(Some(new_links));
    } else {
        identity = m.identity.clone();
        links.push(None);
    }
    deleted.push(vec![0u64; (new_seg.slot_count() as usize).div_ceil(64)]);
    entries.push(SetSegment {
        id: m.segment_id.clone(),
        file: format!("segments/{}.oxv", m.segment_id),
        kind: m.kind.clone(),
        from: m.from_library_version,
        to: m.to_library_version,
        sha256: m.segment.sha256.clone(),
        size: m.segment.size,
        slots: m.counts.slots,
        package_digest: Some(m.package_digest.clone()),
        provenance: m.provenance.clone(),
        del: DerivedFile::default(),
        links: None,
    });
    let (identity_digest, codec_params) = match &current {
        Some((doc, _)) => (doc.identity_digest.clone(), doc.codec_params_sha256.clone()),
        None => (m.identity_digest.clone(), m.codec_params_sha256.clone()),
    };
    drop(new_seg);
    drop(current);

    // Place the segment (files.rs `place_segment`) and its manifest.
    fs::create_dir_all(dir.join("segments"))?;
    let target = dir.join(format!("segments/{}.oxv", m.segment_id));
    let kept = target.exists() && hash_file(&target)? == sha;
    if !kept {
        if target.exists() {
            let held = hash_file(&target)?;
            let live = ["CURRENT", "PREVIOUS"].iter().filter_map(|n| read_pointer(dir, n).ok().flatten()).filter_map(|p| read_generation(dir, &p)).any(|d| {
                d.segments.iter().any(|s| s.id == m.segment_id && s.sha256 == held)
            });
            if live {
                return Err(bad(format!("segment id {} is taken by other bytes ({held})", m.segment_id)));
            }
        }
        fs::rename(&staged, &target)?;
    }
    // Published, or the segment goes back where the host left it (install.rs).
    let published = (|| -> Result<SetDocument> {
        write_atomically(&dir.join(format!("segments/{}.package.json", m.segment_id)), manifest_json.as_bytes())?;

        // NewGeneration::commit.
        let generation = next_generation(dir)?;
        let gen_name = gen_dir(generation);
        let gen_path = dir.join(&gen_name);
        fs::create_dir(&gen_path)?;
        let mut stats = SetStats::default();
        for ((entry, dead), l) in entries.iter_mut().zip(&deleted).zip(&links) {
            let (bytes, crc, count) = encode_deleted_words(entry.slots, dead);
            let file = format!("{}.del", entry.id);
            write_atomically(&gen_path.join(&file), &bytes)?;
            entry.del = DerivedFile { file, crc32: crc, count };
            entry.links = match l {
                Some(l) => {
                    let (bytes, crc, unresolved) = encode_link_list(l);
                    let file = format!("{}.links", entry.id);
                    write_atomically(&gen_path.join(&file), &bytes)?;
                    stats.foreign += l.len() as u64;
                    stats.foreign_unresolved += unresolved;
                    Some(DerivedFile { file, crc32: crc, count: unresolved })
                }
                None => None,
            };
            stats.slots += entry.slots;
            stats.slots_dead += entry.del.count;
            stats.bytes += entry.size;
        }
        let document = SetDocument {
            format: SET_FORMAT.into(),
            format_version: 1,
            generation,
            identity,
            identity_digest,
            codec_params_sha256: codec_params,
            library_version: m.to_library_version,
            library_release_tag: m.library_release_tag.clone(),
            segments: entries,
            stats,
            created_at: utc_timestamp(std::time::SystemTime::now()),
        };
        let bytes = serde_json::to_vec_pretty(&document).map_err(|e| bad(e.to_string()))?;
        write_atomically(&gen_path.join("set.json"), &bytes)?;
        let pointer = Pointer { generation, set: format!("{gen_name}/set.json"), set_sha256: sha256_hex(&bytes) };
        let usable = |p: &Pointer| read_generation(dir, p).is_some_and(|older| falls_back_to(&document.identity, &older.identity));
        if let Some(previous) = base_pointer.clone().filter(usable) {
            if read_pointer(dir, "PREVIOUS").ok().flatten().as_ref() != Some(&previous) {
                write_atomically(&dir.join("PREVIOUS"), &serde_json::to_vec(&previous).unwrap())?;
            }
        }
        write_atomically(&dir.join("CURRENT"), &serde_json::to_vec(&pointer).unwrap())?;
        Ok(document)
    })();
    let document = match published {
        Ok(v) => v,
        Err(e) => {
            if taking && !kept {
                let _ = fs::rename(&target, segment);
            }
            return Err(e);
        }
    };
    if kept {
        let _ = fs::remove_file(&staged);
    }
    let usable = |p: &Pointer| read_generation(dir, p).is_some_and(|older| falls_back_to(&document.identity, &older.identity));
    let discard = match read_pointer(dir, "PREVIOUS") {
        Ok(Some(p)) => !usable(&p),
        Ok(None) => false,
        Err(()) => true,
    };
    if discard {
        let path = dir.join("PREVIOUS");
        let removed = if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) { fs::remove_dir(&path) } else { fs::remove_file(&path) };
        if removed.is_err() {
            return Ok(report(&m, &document, tombstones_applied, duplicates_removed, foreign_unresolved));
        }
    }
    collect_garbage(dir);
    let _ = fs::remove_dir(dir.join("staging"));
    Ok(report(&m, &document, tombstones_applied, duplicates_removed, foreign_unresolved))
}

fn report(m: &ReleaseManifest, d: &SetDocument, tombstones: u64, duplicates: u64, unresolved: u64) -> InstallReport {
    InstallReport {
        kind: m.kind.clone(),
        library_version: d.library_version,
        generation: d.generation,
        segments: d.segments.len() as u32,
        slots_added: m.counts.slots,
        tombstones_applied: tombstones,
        duplicates_removed: duplicates,
        foreign_unresolved: unresolved,
        bytes_on_disk: d.stats.bytes,
        already_applied: false,
    }
}

/// package.rs `utc_timestamp`: `YYYY-MM-DDTHH:MM:SSZ`.
fn utc_timestamp(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}


#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
