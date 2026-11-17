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
//! Ported too: `recover` (staging, temporary pointers, garbage) before every
//! install, the directory flushes (`sync_dir`, Unix only as in the sidecar), and
//! the identity and package digests recomputed from the manifest
//! (`IndexVersion::identity_digest`, `IndexPackage::digest`) and checked;
//! the expected model identity (`InstallExpectation`, from the query model's
//! model.json as Otzaria's app passes it), a manifest whose identity holds a value
//! of the wrong JSON type refused (the sidecar's typed `IndexVersion`), and
//! cancellation (between blocks, hashed chunks and slot ranges; a cancelled install
//! leaves the set as it was, `recover` clearing what it staged).
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

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

/// Why an install or verify refused, as Otzaria's sidecar classifies its errors for the
/// app (otzaria_search_engine `semantic_errors.rs` `artifact_kind` / `store_kind`): a
/// damaged package or set (`corrupt`), a sound one this set or build cannot take
/// (`incompatible`, also a release of another model, as the sidecar's IdentityMismatch),
/// a manifest that is not the published one (`not_published`), the set's lock held
/// (`busy`), a full disk (`no_space`), a bad argument (`invalid`), anything else
/// (`internal`). The tag is in the message: `vector install [<kind>]: ...`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Corrupt,
    Incompatible,
    NotPublished,
    Busy,
    NoSpace,
    Invalid,
    Internal,
    /// Nothing is installed where a set was expected (Otzaria `ArtifactMissing`).
    Missing,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Corrupt => "corrupt",
            Kind::Incompatible => "incompatible",
            Kind::NotPublished => "not_published",
            Kind::Busy => "busy",
            Kind::NoSpace => "no_space",
            Kind::Invalid => "invalid",
            Kind::Internal => "internal",
            Kind::Missing => "missing",
        }
    }
}

fn fail(kind: Kind, msg: impl Into<String>) -> Error {
    Error::Format(format!("vector install [{}]: {}", kind.tag(), msg.into()))
}

/// The kind of an install / verify error: the tag of a [`fail`], a full disk or other
/// I/O failure, and an untagged format error (a segment file that does not open as
/// one) as damage, as the sidecar's `VectorStoreError::Corrupted` on a vector set.
pub fn error_kind(e: &Error) -> Option<Kind> {
    match e {
        Error::Cancelled => None,
        Error::Io(io) if io.kind() == std::io::ErrorKind::StorageFull => Some(Kind::NoSpace),
        Error::Io(_) => Some(Kind::Internal),
        Error::Invalid(_) => Some(Kind::Invalid),
        Error::Format(m) => Some(
            [Kind::Corrupt, Kind::Incompatible, Kind::NotPublished, Kind::Busy, Kind::NoSpace, Kind::Invalid, Kind::Internal, Kind::Missing]
                .into_iter()
                .find(|k| m.starts_with(&format!("vector install [{}]", k.tag())))
                .unwrap_or(Kind::Corrupt),
        ),
    }
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
    /// The sidecar's `CompactionPolicy::default().wants`: more than 4 segments,
    /// deltas over 20 % of the base's bytes, or more than 5 % of the slots dead.
    pub needs_compaction: bool,
    pub already_applied: bool,
}

/// Otzaria's default compaction policy (segment_set/compact.rs `CompactionPolicy::default`).
const COMPACT_MAX_SEGMENTS: usize = 4;
const COMPACT_MAX_DELTA_RATIO: f64 = 0.20;
const COMPACT_MAX_DEAD_RATIO: f64 = 0.05;

/// `CompactionPolicy::default().wants(&info).is_some()` on the set's document.
fn needs_compaction(d: &SetDocument) -> bool {
    let sizes: Vec<u64> = d.segments.iter().map(|s| s.size).collect();
    compaction_wanted(&sizes, d.stats.slots, d.stats.slots_dead)
}

/// The policy's rule on segment sizes (base first) and slot counts.
pub(crate) fn compaction_wanted(sizes: &[u64], slots: u64, dead: u64) -> bool {
    if sizes.len() > COMPACT_MAX_SEGMENTS {
        return true;
    }
    let base = sizes.first().copied().unwrap_or(0);
    let deltas: u64 = sizes.iter().skip(1).sum();
    if base > 0 && deltas as f64 > COMPACT_MAX_DELTA_RATIO * base as f64 {
        return true;
    }
    slots > 0 && dead as f64 > COMPACT_MAX_DEAD_RATIO * slots as f64
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex(&sha2::Sha256::digest(bytes))
}

/// cancellation.rs `checkpoint`: stop when the host cancelled.
fn checkpoint(cancel: Option<&AtomicBool>) -> Result<()> {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

fn hash_file(path: &Path, cancel: Option<&AtomicBool>) -> Result<String> {
    use sha2::Digest;
    let mut file = File::open(path)?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        checkpoint(cancel)?;
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

/// install.rs `copy_hashing` without the hash: a copy checked for cancellation.
fn copy_checked(from: &Path, to: &Path, cancel: Option<&AtomicBool>) -> Result<()> {
    let mut input = File::open(from)?;
    let mut output = File::create(to)?;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        checkpoint(cancel)?;
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
    }
    output.sync_all()?;
    Ok(())
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
fn verify_blocks(b: &[u8], cancel: Option<&AtomicBool>) -> Result<u64> {
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
        return Err(fail(Kind::Corrupt, "the segment has no BLOCK_CRCS section"));
    };
    if count != blocks || crcs.len() as u64 != blocks * 4 {
        return Err(fail(Kind::Corrupt, format!("it carries {count} block CRC(s) for {blocks} block(s)")));
    }
    let crcs = &b[crcs];
    let mut index = 0;
    for (kind, range) in summed {
        for (n, block) in b[range].chunks(BLOCK_SIZE).enumerate() {
            checkpoint(cancel)?;
            let (actual, expected) = (crc32fast::hash(block), u32_at(crcs, index * 4));
            if actual != expected {
                return Err(fail(Kind::Corrupt, format!(
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
        Err(fail(Kind::Corrupt, format!("the segment's {what} is {header}, and its manifest declares {declared}")))
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

/// versioning.rs `IdentityField::ALL`: (path, JSON pointer).
const IDENTITY_FIELDS: [(&str, &str); 15] = [
    ("text.line_text_version", "/text/line_text_version"),
    ("text.key_version", "/text/key_version"),
    ("model.family_id", "/model/family_id"),
    ("model.tokenizer_checksum", "/model/tokenizer_checksum"),
    ("model.embedding_dim", "/model/embedding_dim"),
    ("model.pooling", "/model/pooling"),
    ("model.max_tokens", "/model/max_tokens"),
    ("model.query_max_tokens", "/model/query_max_tokens"),
    ("model.embedding_text_version", "/model/embedding_text_version"),
    ("model.normalization_version", "/model/normalization_version"),
    ("model.chunking_identity", "/model/chunking_identity"),
    ("model.query_packages", "/model/query_packages"),
    ("store.backend_id", "/store/backend_id"),
    ("store.store_format_version", "/store/store_format_version"),
    ("store.vector_precision", "/store/vector_precision"),
];

/// versioning.rs `IndexVersion::value`: a field's canonical text (None: an
/// absent `query_max_tokens`, which the digests skip).
fn identity_value(identity: &Value, path: &str, pointer: &str) -> Result<Option<String>> {
    let v = identity.pointer(pointer);
    if path == "model.query_max_tokens" && v.is_none_or(Value::is_null) {
        return Ok(None);
    }
    let missing = || fail(Kind::Corrupt, format!("its identity has no {path} of the type the sidecar reads"));
    let text = matches!(
        path,
        "model.family_id" | "model.tokenizer_checksum" | "model.pooling" | "store.backend_id" | "store.vector_precision"
    );
    Ok(Some(match v.ok_or_else(missing)? {
        Value::String(s) if text => s.clone(),
        Value::Number(n) if !text && n.is_u64() && path != "model.query_packages" => n.to_string(),
        Value::Array(packages) if path == "model.query_packages" => {
            let mut all = Vec::new();
            for p in packages {
                let (Some(c), Some(q)) = (p.get("checksum").and_then(Value::as_str), p.get("quantization").and_then(Value::as_str)) else {
                    return Err(missing());
                };
                all.push(format!("{c} {q}"));
            }
            all.sort();
            all.join(",")
        }
        _ => return Err(missing()),
    }))
}

fn identity_lines(identity: &Value, text: &mut String) -> Result<()> {
    for (path, pointer) in IDENTITY_FIELDS {
        if let Some(v) = identity_value(identity, path, pointer)? {
            text.push_str(&format!("{path}={v}
"));
        }
    }
    Ok(())
}

/// versioning.rs `identity_digest_hex`: SHA-256 over `otzaria-vector-identity-v1`
/// and one `path=value` line per field.
pub fn identity_digest(identity: &Value) -> Result<String> {
    let mut text = String::from("otzaria-vector-identity-v1
");
    identity_lines(identity, &mut text)?;
    Ok(sha256_hex(text.as_bytes()))
}

/// package.rs `IndexPackage::digest` of the package a release manifest describes
/// (install.rs `ReleaseManifest::package`: metadata version 3, one payload
/// `segment.oxv`, total size = the segment's).
fn package_digest(m: &ReleaseManifest) -> Result<String> {
    let mut text = String::from("otzaria-artifact-digest-v2
metadata_version=3
");
    identity_lines(&m.identity, &mut text)?;
    let s = |p: &str| m.provenance.pointer(p).and_then(Value::as_str).map(str::to_string).ok_or_else(|| fail(Kind::Corrupt, format!("its provenance has no {p}")));
    let c = &m.counts;
    text.push_str(&format!("kind={}
", m.kind));
    text.push_str(&format!("from_library_version={}
", m.from_library_version));
    text.push_str(&format!("to_library_version={}
", m.to_library_version));
    text.push_str(&format!("library_release_tag={}
", m.library_release_tag));
    text.push_str(&format!("counts books={} slots={} extras={} foreign={} tombstones={}
", c.books, c.slots, c.extras, c.foreign, c.tombstones));
    text.push_str(&format!(
        "provenance passage_package={} {} worker={} {}
",
        s("/passage_package/checksum")?,
        s("/passage_package/quantization")?,
        s("/worker/backend")?,
        s("/worker/device")?
    ));
    text.push_str(&format!("total_size_bytes={}
", m.segment.size));
    text.push_str(&format!("payload segment.oxv={} {}
", m.segment.sha256, m.segment.size));
    Ok(sha256_hex(text.as_bytes()))
}

/// package.rs `sync_dir`: flush a directory entry (Unix; Windows cannot open a
/// directory as a file, as in the sidecar).
#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    Ok(File::open(path)?.sync_all()?)
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<()> {
    Ok(())
}

/// mod.rs `recover`: after a crash, `staging/`, temporary pointer files and the
/// garbage go. Called with the lock held.
fn recover(dir: &Path) -> Result<()> {
    let staging = dir.join("staging");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    for e in fs::read_dir(dir)?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let temporary = ["CURRENT", "PREVIOUS"].iter().any(|p| name.strip_prefix(p).is_some_and(|rest| rest.starts_with('.') && rest.ends_with(".tmp")));
        if temporary {
            fs::remove_file(e.path())?;
        }
    }
    collect_garbage(dir);
    Ok(())
}

/// versioning.rs `mismatches_against` for the model group the host knows: the
/// query model's model.json (as Otzaria's app passes `modelIdentityJson`). Each
/// identity field it names must be equal; its query packages must all be ones the
/// release accepts (membership, by checksum). `family_id` and `chunking_identity`
/// are required.
fn check_expected_model(identity: &Value, model_json: &str) -> Result<()> {
    let expected: Value = serde_json::from_str(model_json).map_err(|e| fail(Kind::Invalid, format!("model.json: {e}")))?;
    let mut mismatches = Vec::new();
    for (path, _) in IDENTITY_FIELDS {
        let Some(key) = path.strip_prefix("model.") else { continue };
        let wanted = expected.get(key);
        if wanted.is_none() && matches!(key, "family_id" | "chunking_identity") {
            return Err(fail(Kind::Invalid, format!("model.json has no {key}")));
        }
        let Some(wanted) = wanted.filter(|w| !w.is_null()) else { continue };
        let artifact = identity.pointer(&format!("/model/{key}"));
        let agrees = if key == "query_packages" {
            let accepted: Vec<&Value> = artifact.and_then(Value::as_array).map(|a| a.iter().filter_map(|p| p.get("checksum")).collect()).unwrap_or_default();
            wanted.as_array().is_some_and(|w| w.iter().all(|p| p.get("checksum").is_some_and(|c| accepted.contains(&c))))
        } else {
            artifact == Some(wanted)
        };
        if !agrees {
            mismatches.push(format!("{path}: release {}, model {wanted}", artifact.map_or("none".to_string(), Value::to_string)));
        }
    }
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(fail(Kind::Incompatible, format!("the release is not of the query model: {}", mismatches.join("; "))))
    }
}

fn parse_manifest(json: &str) -> Result<ReleaseManifest> {
    let m: ReleaseManifest =
        serde_json::from_str(json).map_err(|e| fail(Kind::Incompatible, format!("its release manifest is not one this build reads: {e}")))?;
    if m.format != RELEASE_FORMAT || m.format_version != 1 {
        return Err(fail(Kind::Incompatible, format!("it is {} version {}, and this build reads {RELEASE_FORMAT} version 1", m.format, m.format_version)));
    }
    if m.segment_id.len() != 32 || !m.segment_id.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
        return Err(fail(Kind::Corrupt, "a segment id is 32 lowercase hex digits"));
    }
    let digest = identity_digest(&m.identity)?;
    if digest != m.identity_digest {
        return Err(fail(Kind::Corrupt, format!("its identity digest is {}, and its identity's is {digest}", m.identity_digest)));
    }
    if package_digest(&m)? != m.package_digest {
        return Err(fail(Kind::Corrupt, "its package digest is not the digest of the package it describes"));
    }
    match m.kind.as_str() {
        "base" | "delta" => Ok(m),
        "compacted" => Err(fail(Kind::Incompatible, "a compacted segment is made on a device and never published")),
        k => Err(fail(Kind::Incompatible, format!("kind {k}"))),
    }
}

/// Verifies a plain `.oxv`: header and directory, every block CRC and, given
/// its release manifest, that the header is the one the manifest describes.
pub fn verify_segment(path: &Path, manifest_json: Option<&str>) -> Result<VerifyReport> {
    verify_segment_cancellable(path, manifest_json, None)
}

/// [`verify_segment`], stopping with [`Error::Cancelled`] between blocks.
pub fn verify_segment_cancellable(path: &Path, manifest_json: Option<&str>, cancel: Option<&AtomicBool>) -> Result<VerifyReport> {
    let seg = OxvFile::open(path)?;
    let blocks = verify_blocks(seg.bytes(), cancel)?;
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
                Err(fail(Kind::Busy, "another install or compaction of this vector set is running"))
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
    placed?;
    match path.parent() {
        Some(parent) => sync_dir(parent),
        None => Ok(()),
    }
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

/// files.rs `read_verdict` + `verdict_against`: whether a scrub's verdict condemns segment
/// `id` as the generation names it (`segments/<id>.corrupt`; an empty or unreadable verdict
/// condemns any bytes, one naming other bytes than `sha256` too).
fn condemned(dir: &Path, id: &str, sha256: &str) -> bool {
    let path = dir.join("segments").join(format!("{id}.corrupt"));
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    let named = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("sha256").and_then(|s| s.as_str()).map(str::to_string))
        .unwrap_or_default();
    named.is_empty() || named != sha256
}

/// segment_set::info's `usable` + `refuse_a_v1_artifact`: the readable, uncondemned
/// generation (CURRENT's, else PREVIOUS's) and whether it is PREVIOUS's; None when nothing
/// is installed.
fn readable_generation(dir: &Path) -> Result<Option<(SetDocument, bool)>> {
    let current = read_pointer(dir, "CURRENT");
    let previous = read_pointer(dir, "PREVIOUS");
    if matches!((&current, &previous), (Ok(None), Ok(None))) {
        if dir.join("manifest.json").is_file() {
            return Err(fail(Kind::Incompatible, format!("{} holds a version-1 artifact, which this build does not read", dir.display())));
        }
        return Ok(None);
    }
    let usable = |p: &std::result::Result<Option<Pointer>, ()>| -> Option<SetDocument> {
        let p = p.as_ref().ok()?.as_ref()?;
        let d = read_generation(dir, p)?;
        (!d.segments.iter().any(|s| condemned(dir, &s.id, &s.sha256))).then_some(d)
    };
    if let Some(d) = usable(&current) {
        return Ok(Some((d, false)));
    }
    if let Some(d) = usable(&previous) {
        return Ok(Some((d, true)));
    }
    Err(fail(Kind::Corrupt, format!("{}: neither CURRENT nor PREVIOUS names a generation that can be read", dir.display())))
}

/// [`crate::coverage::dir_info`]: segment_set::info + `info_of` from set.json alone.
pub(crate) fn dir_info(dir: &Path) -> Result<crate::coverage::VectorsDirInfo> {
    let Some((d, recovered)) = readable_generation(dir)? else {
        return Ok(Default::default());
    };
    let segments: Vec<crate::coverage::DirSegmentInfo> = d
        .segments
        .iter()
        .map(|s| crate::coverage::DirSegmentInfo {
            id: s.id.clone(),
            kind: s.kind.clone(),
            from_library_version: s.from,
            to_library_version: s.to,
            slots: s.slots,
            slots_dead: s.del.count,
            foreign_unresolved: s.links.as_ref().map_or(0, |l| l.count),
            size: s.size,
        })
        .collect();
    let slots: u64 = segments.iter().map(|s| s.slots).sum();
    let dead: u64 = segments.iter().map(|s| s.slots_dead).sum();
    let sizes: Vec<u64> = segments.iter().map(|s| s.size).collect();
    Ok(crate::coverage::VectorsDirInfo {
        present: true,
        identity_digest: d.identity_digest.clone(),
        model_family_id: d.identity.pointer("/model/family_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        library_version: d.library_version,
        library_release_tag: d.library_release_tag.clone(),
        generation: d.generation,
        bytes_on_disk: sizes.iter().sum(),
        needs_compaction: compaction_wanted(&sizes, slots, dead),
        slots_live: slots.saturating_sub(dead),
        slots_dead: dead,
        segments,
        recovered_from_previous: recovered,
        chunking_identity: d.identity.pointer("/model/chunking_identity").and_then(|v| v.as_u64()).unwrap_or(0),
    })
}

/// The segment files (relative to `dir`) of the generation [`dir_info`] reads.
pub(crate) fn live_segment_files(dir: &Path) -> Result<Vec<String>> {
    Ok(readable_generation(dir)?.map_or_else(Vec::new, |(d, _)| d.segments.into_iter().map(|s| s.file).collect()))
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
    highest.checked_add(1).ok_or_else(|| fail(Kind::Internal, "no generation number is left"))
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

/// What an install expects besides the manifest (install.rs `InstallExpectation`).
#[derive(Default)]
pub struct InstallOptions<'a> {
    /// The manifest's published SHA-256.
    pub published_sha256: Option<&'a str>,
    /// The query model's model.json: the release must be of it.
    pub expected_model_json: Option<&'a str>,
    /// Set by the host to stop the install (between blocks, hashed chunks and slot ranges).
    pub cancel: Option<&'a AtomicBool>,
}

/// [`install_with`] with only the published SHA-256.
pub fn install(dir: &Path, segment: &Path, manifest_json: &str, published_sha256: Option<&str>) -> Result<InstallReport> {
    install_with(dir, segment, manifest_json, &InstallOptions { published_sha256, ..Default::default() })
}

/// install.rs `install_package`: installs the plain `.oxv` at `segment` (as
/// `manifest_json` describes it) into the set at `dir`, as [`InstallOptions`] expect.
/// A segment inside `<dir>/incoming/` is moved into the set, any other copied.
pub fn install_with(dir: &Path, segment: &Path, manifest_json: &str, opts: &InstallOptions) -> Result<InstallReport> {
    let _lock = SetLock::take(dir)?;
    recover(dir)?;
    let r = install_locked(dir, segment, manifest_json, opts);
    if matches!(r, Err(Error::Cancelled)) {
        // a cancelled install changed nothing the set names; what it staged goes
        let _ = recover(dir);
    }
    r
}

fn install_locked(dir: &Path, segment: &Path, manifest_json: &str, opts: &InstallOptions) -> Result<InstallReport> {
    let (published_sha256, cancel) = (opts.published_sha256, opts.cancel);
    if let Some(p) = published_sha256 {
        let actual = sha256_hex(manifest_json.as_bytes());
        if actual != p {
            return Err(fail(Kind::NotPublished, format!("the manifest is {actual}, and the published one {p}")));
        }
    }
    let m = parse_manifest(manifest_json)?;
    if let Some(model) = opts.expected_model_json {
        check_expected_model(&m.identity, model)?;
    }

    // The generation it goes on (CURRENT, else PREVIOUS, as the set opens).
    let base_pointer = [read_pointer(dir, "CURRENT"), read_pointer(dir, "PREVIOUS")]
        .into_iter()
        .filter_map(|p| p.ok().flatten())
        .find(|p| read_generation(dir, p).is_some());
    let current = match (&base_pointer, m.kind.as_str()) {
        (None, "delta") => return Err(fail(Kind::Incompatible, "a delta needs an installed base for it to apply to")),
        (Some(p), "delta") => Some((read_generation(dir, p).unwrap(), SegmentSet::open(dir)?)),
        _ => None,
    };
    if let Some((doc, set)) = &current {
        if set.generation() != doc.generation {
            return Err(fail(Kind::Internal, "the set opened another generation than its pointer names"));
        }
        if m.identity_digest != doc.identity_digest {
            return Err(fail(Kind::Incompatible, format!("its identity digest is {}, and the set's is {}", m.identity_digest, doc.identity_digest)));
        }
        if m.codec_params_sha256 != doc.codec_params_sha256 {
            return Err(fail(Kind::Incompatible, format!("it is of codec epoch {}, and the set of {}", m.codec_params_sha256, doc.codec_params_sha256)));
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
                needs_compaction: needs_compaction(&doc),
                already_applied: true,
            });
        }
        if m.from_library_version != doc.library_version {
            return Err(fail(Kind::Incompatible, format!(
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
        if let Err(e) = copy_checked(segment, &staged, cancel) {
            let _ = fs::remove_file(&staged);
            return Err(e);
        }
        staged
    };
    let checked = (|| -> Result<(String, OxvFile)> {
        let size = fs::metadata(&staged)?.len();
        if size != m.segment.size {
            return Err(fail(Kind::Corrupt, format!("the segment is {size} bytes, and its manifest declares {}", m.segment.size)));
        }
        let sha = hash_file(&staged, cancel)?;
        if sha != m.segment.sha256 {
            return Err(fail(Kind::Corrupt, format!("the segment hashes to {sha}, and its manifest declares {}", m.segment.sha256)));
        }
        let new_seg = OxvFile::open(&staged)?;
        check_header(&m, &new_seg)?;
        verify_blocks(new_seg.bytes(), cancel)?;
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
            return Err(fail(Kind::Corrupt, format!("segment {} is in the generation already", m.segment_id)));
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
                return Err(fail(Kind::Corrupt, format!("segment {} ships key {} twice", m.segment_id, hex(&new_seg.key(slot)))));
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
                if slot % (1 << 20) == 0 {
                    checkpoint(cancel)?;
                }
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
    let kept = target.exists() && hash_file(&target, cancel)? == sha;
    if !kept {
        if target.exists() {
            let held = hash_file(&target, cancel)?;
            let live = ["CURRENT", "PREVIOUS"].iter().filter_map(|n| read_pointer(dir, n).ok().flatten()).filter_map(|p| read_generation(dir, &p)).any(|d| {
                d.segments.iter().any(|s| s.id == m.segment_id && s.sha256 == held)
            });
            if live {
                return Err(fail(Kind::Incompatible, format!("segment id {} is taken by other bytes ({held})", m.segment_id)));
            }
        }
        fs::rename(&staged, &target)?;
        sync_dir(&dir.join("segments"))?;
    }
    // Published, or the segment goes back where the host left it (install.rs).
    let published = (|| -> Result<SetDocument> {
        write_atomically(&dir.join(format!("segments/{}.package.json", m.segment_id)), manifest_json.as_bytes())?;

        // NewGeneration::commit.
        let generation = next_generation(dir)?;
        let gen_name = gen_dir(generation);
        let gen_path = dir.join(&gen_name);
        fs::create_dir(&gen_path)?;
        sync_dir(dir)?;
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
        let bytes = serde_json::to_vec_pretty(&document).map_err(|e| fail(Kind::Internal, e.to_string()))?;
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
        let removed = if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) { fs::remove_dir(&path) } else { fs::remove_file(&path) }
            .map_err(Error::from)
            .map(|()| {
                let _ = sync_dir(dir);
            });
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
        needs_compaction: needs_compaction(d),
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


#[path = "compact.rs"]
pub mod compact;

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
