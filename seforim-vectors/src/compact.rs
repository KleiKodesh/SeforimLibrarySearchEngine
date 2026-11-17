//! Compaction: a set's segments merged into one, on the device, by file operations alone.
//! A port of Otzaria's sidecar `segment_set/compact.rs` (`otzaria-semantic-search`
//! d2a782a) step for step, with `SegmentSet::book_occurrences` and
//! `book_source_hash` (segment_set/mod.rs), on this crate's reader, writer
//! ([`crate::writer`], a port of `oxv/writer.rs`) and install machinery (lock,
//! `recover`, generation commit, garbage).
//!
//! Nothing is embedded: vectors are copied byte for byte (every segment of a set shares
//! one codec epoch), and every block of every source is verified before it is copied.
//! Records are gathered book by book from the newest complete snapshot of each key;
//! vectors stay deduplicated: the first appearance takes a slot, later books an extra.
//!
//! Re-anchoring on the live index goes through [`LiveKeySource`] (resolve.rs), which the
//! host implements; without one (or on another library version) records are kept as
//! they are, as the sidecar does.
//!
//! The report's byte counts are the segments' sizes (set.json `stats.bytes`); the
//! sidecar's `bytes_on_disk` also counts the small derived files (`.del`, `.links`).
//! A full disk while writing is an I/O error tagged `no_space`, as the sidecar's
//! `InsufficientSpace`.
//!
//! Named deviation: the sidecar condemns a source segment whose block fails its CRC
//! (a verdict file its set reader honours). This crate's set has no verdicts (nor has
//! its install), so the compaction refuses with `corrupt` and writes nothing.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::{
    collect_garbage, encode_deleted_words, fail, falls_back_to, gen_dir, hex, next_generation, read_generation, read_pointer,
    recover, sha256_hex, sync_dir, utc_timestamp, verify_blocks, write_atomically, DerivedFile, Kind, Pointer, SetDocument,
    SetLock, SetSegment, SetStats, SET_FORMAT,
};
use crate::chunker::ChunkKey;
use crate::oxv::{Key, OxvFile};
use crate::set::SegmentSet;
use crate::writer::{CodecSpec, Occurrence, PackageKind, SegmentBuilder, SegmentSpec, ALIAS_WINDOW};
use crate::{Error, Result};

/// compact.rs `CompactionPolicy` (JSON at the C ABI; every field optional, the
/// sidecar's defaults).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionPolicy {
    /// Compact once the deltas together are more than this fraction of the base.
    pub max_delta_ratio: f64,
    /// Compact once the set has more segments than this.
    pub max_segments: u32,
    /// Compact once more than this fraction of the slots is dead.
    pub max_dead_ratio: f64,
    /// Refuse to start without this many times the output's size free.
    pub min_free_space_factor: f64,
    /// Re-anchor every record on the live index, when it holds the set's library version.
    pub refresh_hints: bool,
    /// Compact whatever the thresholds say.
    pub force: bool,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self { max_delta_ratio: 0.20, max_segments: 4, max_dead_ratio: 0.05, min_free_space_factor: 1.15, refresh_hints: true, force: false }
    }
}

impl CompactionPolicy {
    /// compact.rs `wants`: why this policy would compact a set of these segment sizes
    /// (base first) and live / dead slots, or None.
    pub fn wants(&self, sizes: &[u64], slots_live: u64, slots_dead: u64) -> Option<String> {
        if self.force {
            return Some("forced".to_string());
        }
        if sizes.len() > self.max_segments as usize {
            return Some(format!("{} segments, more than {}", sizes.len(), self.max_segments));
        }
        let base = sizes.first().copied().unwrap_or(0);
        let deltas: u64 = sizes.iter().skip(1).sum();
        if base > 0 && deltas as f64 > self.max_delta_ratio * base as f64 {
            return Some(format!("the deltas hold {deltas} bytes, more than {} of the base's {base}", self.max_delta_ratio));
        }
        let slots = slots_live + slots_dead;
        if slots > 0 && slots_dead as f64 > self.max_dead_ratio * slots as f64 {
            return Some(format!("{slots_dead} of {slots} slots are dead, more than {}", self.max_dead_ratio));
        }
        None
    }
}

/// compact.rs `CompactionReport` (JSON at the C ABI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionReport {
    pub compacted: bool,
    pub reason: String,
    pub generation: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub slots_before: u64,
    pub slots_after: u64,
    pub records_pruned: u64,
    pub hints_refreshed: u64,
    pub elapsed_ms: u64,
}

/// resolve.rs `RecordRef`: one record of a key in a book.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecordRef {
    pub book: String,
    pub hint: u32,
    pub window: u8,
    pub anchor: Option<Key>,
}

/// resolve.rs `LiveKeySource`: the application's live index as compaction asks it.
pub trait LiveKeySource {
    /// The library version the index holds.
    fn library_version(&self) -> u32;

    /// Resolve and verify one occurrence within its book against the live index. None:
    /// the source does not verify fully; Some(empty): no live occurrence remains.
    fn resolve_record(&self, _key: Key, _record: &RecordRef, _cancel: Option<&AtomicBool>) -> Result<Option<Vec<RecordRef>>> {
        Ok(None)
    }

    /// Fill `out` with `(ordinal, chunk key column value)` for every live line of `book`
    /// (`0` for a line that is not embedded); `false` when the index holds no such book.
    fn book_keys(&self, book: &str, out: &mut Vec<(u32, u64)>) -> Result<bool>;
}

const UNASSIGNED: u32 = u32::MAX;
const STAGING_DIR: &str = "staging";

fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|c| c.load(Ordering::Relaxed))
}

/// segment_set/mod.rs `book_source_hash`: the newest segment holding the book decides.
fn book_source_hash(segments: &[OxvFile], name: &str) -> Option<u64> {
    segments
        .iter()
        .rev()
        .find(|s| s.books().binary_search_by(|b| b.name.as_str().cmp(name)).is_ok())
        .and_then(|s| s.book_source_hash(name))
}

/// segment_set/mod.rs `book_occurrences`: effective complete occurrences in a book;
/// newest per-key snapshots replace every older location.
fn book_occurrences(set: &SegmentSet, name: &str, out: &mut Vec<(Key, RecordRef)>) {
    out.clear();
    let (segments, _, links) = set.parts();
    let mut records: HashMap<Key, RecordRef> = HashMap::new();
    for (seg, segment) in segments.iter().enumerate() {
        let Ok(index) = segment.books().binary_search_by(|b| b.name.as_str().cmp(name)) else { continue };
        let book = &segment.books()[index];
        let mut fresh: HashMap<Key, RecordRef> = HashMap::new();
        let mut add = |key: Key, hint: u32, window: u8| {
            let record = fresh.entry(key).or_insert(RecordRef { book: book.name.clone(), hint, window, anchor: None });
            if hint < record.hint {
                record.hint = hint;
                record.window = window;
            }
        };
        for slot in book.slots.clone() {
            if set.is_live(seg as u16, slot) {
                add(segment.key(slot), segment.hint(slot), segment.slot_window(slot));
            }
        }
        for extra in book.extras.clone() {
            let (slot, hint) = segment.extra(extra);
            if set.is_live(seg as u16, slot) {
                add(segment.key(slot), hint, segment.slot_window(slot));
            }
        }
        for foreign in book.foreign.clone() {
            let (key, _, hint) = segment.foreign_record(foreign);
            if let Some((ts, tslot)) = links[seg].get(foreign as usize).and_then(|l| l.target()) {
                if set.is_live(ts, tslot) {
                    add(key, hint, segments[ts as usize].slot_window(tslot));
                }
            }
        }
        records.extend(fresh);
    }
    for (key, legacy) in records {
        let mut found = false;
        for segment in segments.iter().rev() {
            if let Some(snapshot) = segment.key_occurrences(&key) {
                out.extend(snapshot.into_iter().filter(|r| segment.books()[r.book as usize].name == name).map(|r| {
                    (key, RecordRef { book: segment.books()[r.book as usize].name.clone(), hint: r.hint, window: r.window, anchor: Some(r.anchor) })
                }));
                found = true;
                break;
            }
        }
        if !found {
            out.push((key, legacy));
        }
    }
    sort_records(out);
    out.dedup();
}

fn sort_records(records: &mut [(Key, RecordRef)]) {
    records.sort_unstable_by(|(ka, a), (kb, b)| a.hint.cmp(&b.hint).then(ka.cmp(kb)).then(a.window.cmp(&b.window)).then(a.anchor.cmp(&b.anchor)));
}

/// compact.rs `compact`: merge the set at `dir` into one segment when `policy` asks.
/// Locked throughout; the new generation replaces the old in one flip. Cancellable
/// between blocks, books and every 4096 vectors; a cancelled or failed compaction
/// leaves the set as it was.
pub fn compact(dir: &Path, policy: &CompactionPolicy, live: Option<&dyn LiveKeySource>, cancel: Option<&AtomicBool>) -> Result<CompactionReport> {
    let started = Instant::now();
    let _lock = SetLock::take(dir)?;
    recover(dir)?;
    let r = compact_locked(dir, policy, live, cancel, started);
    if r.is_err() {
        let _ = recover(dir);
    }
    r
}

fn compact_locked(
    dir: &Path,
    policy: &CompactionPolicy,
    live: Option<&dyn LiveKeySource>,
    cancel: Option<&AtomicBool>,
    started: Instant,
) -> Result<CompactionReport> {
    let set = SegmentSet::open(dir)?;
    let base_pointer = [read_pointer(dir, "CURRENT"), read_pointer(dir, "PREVIOUS")]
        .into_iter()
        .filter_map(|p| p.ok().flatten())
        .find(|p| p.generation == set.generation())
        .ok_or_else(|| fail(Kind::Corrupt, "the set's pointer is gone"))?;
    let document = read_generation(dir, &base_pointer).ok_or_else(|| fail(Kind::Corrupt, "the set's generation does not read"))?;
    let sizes: Vec<u64> = document.segments.iter().map(|s| s.size).collect();
    let slots_dead = document.stats.slots_dead;
    let slots_live = document.stats.slots - slots_dead;
    let slots_before = slots_live + slots_dead;
    let bytes_before = document.stats.bytes;
    let Some(reason) = policy.wants(&sizes, slots_live, slots_dead) else {
        return Ok(CompactionReport {
            compacted: false,
            reason: "no threshold of the policy is reached".to_string(),
            generation: document.generation,
            bytes_before,
            bytes_after: bytes_before,
            slots_before,
            slots_after: slots_before,
            records_pruned: 0,
            hints_refreshed: 0,
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    };
    let (segments, deleted, _) = set.parts();
    let (bytes_per_vector, has_scales) = segments[0].codec_layout();
    let width = bytes_per_vector as u64 + if has_scales { 4 } else { 0 };
    let estimate = slots_live * (width + 20) + (64 << 10);
    let needed = (estimate as f64 * policy.min_free_space_factor) as u64;
    if let Some(available) = space::available(dir) {
        if available < needed {
            return Err(fail(Kind::NoSpace, format!("{needed} bytes are needed and {available} are free")));
        }
    }

    // 0. Every block of every source.
    for segment in segments {
        verify_blocks(segment.bytes(), cancel)?;
    }

    // 1. Every live slot, by key, the newest segment's copy first.
    let mut live_slots: Vec<(Key, u16, u32)> = Vec::with_capacity(slots_live as usize);
    for (seg, segment) in segments.iter().enumerate() {
        for slot in 0..segment.slot_count() {
            let dead = deleted[seg].get(slot as usize / 64).is_some_and(|w| w & (1 << (slot % 64)) != 0);
            if !dead {
                live_slots.push((segment.key(slot), seg as u16, slot));
            }
        }
    }
    live_slots.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    live_slots.dedup_by(|later, earlier| later.0 == earlier.0);
    let mut new_slot = vec![UNASSIGNED; live_slots.len()];

    // 2. and 3. Records book by book, slots assigned as a build assigns them.
    let mut books: Vec<(&str, u16, usize)> = segments
        .iter()
        .enumerate()
        .flat_map(|(seg, s)| s.books().iter().enumerate().map(move |(i, b)| (b.name.as_str(), seg as u16, i)))
        .collect();
    books.sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));

    let refresh = policy.refresh_hints && live.is_some_and(|l| l.library_version() == document.library_version);
    let (codec_id, params_sha256) = segments[0].codec_header();
    let codec = CodecSpec {
        id: codec_id,
        dim: segments[0].dims() as u16,
        params: segments[0].codec_params().to_vec(),
        params_sha256,
        bytes_per_vector,
        has_vector_scales: has_scales,
        default_format_version: segments[0].format(),
    };
    let mut builder = SegmentBuilder::new(
        SegmentSpec {
            kind: PackageKind::Compacted,
            identity_digest: segments[0].identity_digest(),
            from_library_version: 0,
            to_library_version: document.library_version,
            library_release_tag: document.library_release_tag.clone(),
        },
        codec,
    );
    builder.set_format_version(segments[0].format())?;
    let mut order: Vec<(u16, u32)> = Vec::with_capacity(live_slots.len());
    let (mut pruned, mut refreshed) = (0u64, 0u64);
    let mut live_lines: Vec<(u32, u64)> = Vec::new();
    let mut start = 0;
    while start < books.len() {
        if cancelled(cancel) {
            return Err(Error::Cancelled);
        }
        let name = books[start].0;
        let end = start + books[start..].iter().take_while(|b| b.0 == name).count();
        let mut records = Vec::new();
        book_occurrences(&set, name, &mut records);
        start = end;
        if refresh {
            let source = live.expect("refresh implies a source");
            let mut resolved = Vec::new();
            live_lines.clear();
            let has_keys = source.book_keys(name, &mut live_lines)?;
            let mut by_key: HashMap<u64, Vec<u32>> = HashMap::new();
            if has_keys {
                for &(ordinal, value) in &live_lines {
                    if value != 0 {
                        by_key.entry(value).or_default().push(ordinal);
                    }
                }
            }
            for (key, mut record) in records {
                if let Some(verified) = source.resolve_record(key, &record, cancel)? {
                    if verified.is_empty() {
                        pruned += 1;
                    }
                    for mut fresh in verified {
                        if record.anchor.is_none() {
                            fresh.anchor = None;
                        }
                        if fresh.hint != record.hint {
                            refreshed += 1;
                        }
                        resolved.push((key, fresh));
                    }
                    continue;
                }
                let lookup = ChunkKey(record.anchor.unwrap_or(key)).column_value();
                match by_key.get(&lookup) {
                    Some(ordinals) => {
                        let closest = *ordinals.iter().min_by_key(|o| (o.abs_diff(record.hint), **o)).expect("nonempty");
                        if closest != record.hint {
                            refreshed += 1;
                            record.hint = closest;
                        }
                        resolved.push((key, record));
                    }
                    None if has_keys && (record.window == 0 || record.window == ALIAS_WINDOW) => pruned += 1,
                    None => resolved.push((key, record)),
                }
            }
            records = resolved;
        }
        sort_records(&mut records);
        records.dedup();
        let mut primary = Vec::new();
        let mut windows = Vec::new();
        let mut extras = Vec::new();
        let mut occurrences = Vec::new();
        let mut least = HashMap::<Key, u8>::new();
        for (key, record) in &records {
            least.entry(*key).and_modify(|w| *w = (*w).min(record.window)).or_insert(record.window);
        }
        let mut recorded = HashSet::new();
        let mut placed = HashSet::new();
        for (key, record) in records {
            if let Some(anchor) = record.anchor {
                occurrences.push(Occurrence { key, book: 0, hint: record.hint, anchor, window: record.window });
            }
            if !recorded.insert(key) {
                continue;
            }
            let Ok(position) = live_slots.binary_search_by(|probe| probe.0.cmp(&key)) else { continue };
            if new_slot[position] == UNASSIGNED {
                new_slot[position] = order.len() as u32;
                let (_, seg, slot) = live_slots[position];
                order.push((seg, slot));
                primary.push((key, record.hint));
                windows.push(least[&key]);
            } else {
                builder.minimize_slot_window(new_slot[position], least[&key]);
                extras.push((record.hint, new_slot[position]));
            }
            placed.insert(key);
        }
        occurrences.retain(|row| !row.is_alias() || placed.contains(&row.key));
        builder.add_book_windowed_hashed(name, &primary, &windows, &extras, &[], book_source_hash(segments, name))?;
        builder.add_occurrences(name, &occurrences)?;
    }
    drop(new_slot);
    drop(live_slots);
    if order.is_empty() {
        return Err(fail(Kind::Corrupt, format!("{}: compacting would leave no record, which no set describes", dir.display())));
    }

    // 4. The segment, vectors copied in the new slot order.
    let staging = dir.join(STAGING_DIR);
    fs::create_dir_all(&staging)?;
    let partial = staging.join("compacted.oxv.partial");
    let written = (|| -> Result<crate::writer::WrittenSegment> {
        let mut sink = builder.write(&partial)?;
        for (index, (seg, slot)) in order.iter().enumerate() {
            if index % 4096 == 0 && cancelled(cancel) {
                return Err(Error::Cancelled);
            }
            let (bytes, scale) = segments[*seg as usize].encoded_vector(*slot);
            sink.push_encoded(bytes, scale)?;
        }
        Ok(sink.finish()?)
    })();
    let written = match written {
        Ok(w) => w,
        Err(e) => {
            let _ = fs::remove_file(&partial);
            return Err(e);
        }
    };

    // 5. Re-open it and compare a sample with its sources.
    {
        let output = OxvFile::open(&partial)?;
        if output.slot_count() as usize != order.len() {
            return Err(fail(Kind::Corrupt, "the compacted segment does not hold the slots it was given"));
        }
        let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ document.generation;
        for _ in 0..1000.min(order.len()) {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let slot = (state >> 33) as usize % order.len();
            let (seg, source) = order[slot];
            let original = &segments[seg as usize];
            if output.encoded_vector(slot as u32) != original.encoded_vector(source) || output.key(slot as u32) != original.key(source) {
                return Err(fail(Kind::Corrupt, format!("slot {slot} of the compacted segment is not segment {seg}'s slot {source}")));
            }
        }
    }

    // 6. Publish it as a generation of its own; its id names its content.
    let id = hex(&written.segment_id);
    let sha = hex(&written.sha256);
    let file = format!("segments/{id}.oxv");
    fs::create_dir_all(dir.join("segments"))?;
    let target = dir.join(&file);
    if target.exists() {
        let held = sha256_file(&target)?;
        if held != sha {
            return Err(fail(Kind::Corrupt, format!("segment {id} is served with SHA-256 {held}, and a compaction wrote other bytes under its name")));
        }
        fs::remove_file(&partial)?;
    } else {
        fs::rename(&partial, &target)?;
        sync_dir(&dir.join("segments"))?;
    }
    let entry = SetSegment {
        id: id.clone(),
        file,
        kind: PackageKind::Compacted.name().into(),
        from: 0,
        to: document.library_version,
        sha256: sha,
        size: written.size,
        slots: written.counts.slots,
        package_digest: None,
        provenance: document.segments[0].provenance.clone(),
        del: DerivedFile::default(),
        links: None,
    };
    drop(set);
    let committed = commit(dir, &base_pointer, &document, entry)?;
    let _ = fs::remove_dir(dir.join(STAGING_DIR));
    Ok(CompactionReport {
        compacted: true,
        reason,
        generation: committed.generation,
        bytes_before,
        bytes_after: written.size,
        slots_before,
        slots_after: written.counts.slots,
        records_pruned: pruned,
        hints_refreshed: refreshed,
        elapsed_ms: started.elapsed().as_millis() as u64,
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    super::hash_file(path, None)
}

/// install.rs `NewGeneration::commit` for one compacted segment on `base`.
fn commit(dir: &Path, base: &Pointer, old: &SetDocument, mut entry: SetSegment) -> Result<SetDocument> {
    let generation = next_generation(dir)?;
    let gen_name = gen_dir(generation);
    let gen_path = dir.join(&gen_name);
    fs::create_dir(&gen_path)?;
    sync_dir(dir)?;
    let words = vec![0u64; (entry.slots as usize).div_ceil(64)];
    let (bytes, crc, count) = encode_deleted_words(entry.slots, &words);
    let del = format!("{}.del", entry.id);
    write_atomically(&gen_path.join(&del), &bytes)?;
    entry.del = DerivedFile { file: del, crc32: crc, count };
    let stats = SetStats { slots: entry.slots, slots_dead: count, foreign: 0, foreign_unresolved: 0, bytes: entry.size };
    let document = SetDocument {
        format: SET_FORMAT.into(),
        format_version: 1,
        generation,
        identity: old.identity.clone(),
        identity_digest: old.identity_digest.clone(),
        codec_params_sha256: old.codec_params_sha256.clone(),
        library_version: old.library_version,
        library_release_tag: old.library_release_tag.clone(),
        segments: vec![entry],
        stats,
        created_at: utc_timestamp(std::time::SystemTime::now()),
    };
    let bytes = serde_json::to_vec_pretty(&document).map_err(|e| fail(Kind::Internal, e.to_string()))?;
    write_atomically(&gen_path.join("set.json"), &bytes)?;
    let pointer = Pointer { generation, set: format!("{gen_name}/set.json"), set_sha256: sha256_hex(&bytes) };
    let usable = |p: &Pointer| read_generation(dir, p).is_some_and(|older| falls_back_to(&document.identity, &older.identity));
    if usable(base) && read_pointer(dir, "PREVIOUS").ok().flatten().as_ref() != Some(base) {
        write_atomically(&dir.join("PREVIOUS"), &serde_json::to_vec(base).unwrap())?;
    }
    write_atomically(&dir.join("CURRENT"), &serde_json::to_vec(&pointer).unwrap())?;
    // as install's commit: a PREVIOUS the new identity cannot fall back to goes
    if read_pointer(dir, "PREVIOUS").ok().flatten().is_some_and(|p| !usable(&p)) {
        let _ = fs::remove_file(dir.join("PREVIOUS"));
        let _ = sync_dir(dir);
    }
    collect_garbage(dir);
    Ok(document)
}

/// segment_set/space.rs: the free space of a directory's filesystem, when known.
mod space {
    use std::path::Path;

    pub(super) fn available(path: &Path) -> Option<u64> {
        imp::available(path)
    }

    #[cfg(windows)]
    mod imp {
        use std::os::windows::ffi::OsStrExt;
        use std::path::Path;

        #[link(name = "kernel32")]
        extern "system" {
            fn GetDiskFreeSpaceExW(directory: *const u16, free_to_caller: *mut u64, total: *mut u64, free: *mut u64) -> i32;
        }

        pub(super) fn available(path: &Path) -> Option<u64> {
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let (mut free_to_caller, mut total, mut free) = (0u64, 0u64, 0u64);
            // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call; the
            // out-pointers are valid u64s (ULARGE_INTEGER).
            let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free_to_caller, &mut total, &mut free) };
            (ok != 0).then_some(free_to_caller)
        }
    }

    #[cfg(any(all(any(target_os = "linux", target_os = "android"), target_pointer_width = "64"), target_os = "macos", target_os = "ios"))]
    mod imp {
        use std::ffi::CString;
        use std::os::raw::{c_char, c_int};
        use std::os::unix::ffi::OsStrExt;
        use std::path::Path;

        extern "C" {
            fn statvfs(path: *const c_char, buf: *mut u8) -> c_int;
        }

        pub(super) fn available(path: &Path) -> Option<u64> {
            let path = CString::new(path.as_os_str().as_bytes()).ok()?;
            let mut buf = [0u64; 64];
            // SAFETY: a NUL-terminated path and a buffer larger than struct statvfs.
            let result = unsafe { statvfs(path.as_ptr(), buf.as_mut_ptr().cast::<u8>()) };
            if result != 0 {
                return None;
            }
            let (fragment, available) = fields(&buf);
            fragment.checked_mul(available)
        }

        #[cfg(any(target_os = "linux", target_os = "android"))]
        fn fields(buf: &[u64; 64]) -> (u64, u64) {
            (buf[1], buf[4])
        }

        #[cfg(any(target_os = "macos", target_os = "ios"))]
        fn fields(buf: &[u64; 64]) -> (u64, u64) {
            (buf[1], buf[3] & 0xFFFF_FFFF)
        }
    }

    #[cfg(not(any(all(any(target_os = "linux", target_os = "android"), target_pointer_width = "64"), target_os = "macos", target_os = "ios", windows)))]
    mod imp {
        use std::path::Path;

        pub(super) fn available(_path: &Path) -> Option<u64> {
            None
        }
    }
}
