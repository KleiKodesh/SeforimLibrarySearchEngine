//! Writing a segment (`.oxv`): a port of Otzaria's sidecar `oxv/writer.rs` and the
//! encoding half of `oxv/format.rs` (`otzaria-semantic-search` d2a782a), step for step,
//! for what a device writes: a compacted segment (`crate::install::compact`). Vectors
//! arrive already encoded (copied byte for byte from the segments being merged), so the
//! float encoder (`push_f32`, `set_codec`) is not ported: nothing here embeds or encodes.
//!
//! [`SegmentBuilder`] collects the tables (books in name order, each with its primary
//! records, which take the next slots, its extras and foreign records; occurrences; book
//! source hashes), [`SegmentBuilder::write`] lays the file out and writes them, and the
//! returned [`VectorSink`] takes the vectors in slot order; [`VectorSink::finish`] writes
//! the block checksums and the header, flushes, and hashes the file.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::oxv::Key;

const MAGIC: [u8; 8] = *b"OXVSEG\r\n";
const READABLE_FORMATS: [u32; 2] = [2, 3];
const HEADER_LEN: usize = 4096;
pub(crate) const BLOCK_SIZE: usize = 1 << 20;
const MAX_SECTIONS: usize = 32;
const DIRECTORY_OFFSET: usize = 224;
const DIRECTORY_ENTRY_LEN: usize = 40;
const HEADER_CRC_OFFSET: usize = 4088;
const RELEASE_TAG_OFFSET: usize = 160;
const RELEASE_TAG_LEN: usize = 64;
const SECTION_ALIGN: u64 = 64;
const VECTORS_ALIGN: u64 = 4096;
const HAS_EXTRAS: u32 = 1 << 31;
const HINT_MASK: u32 = !HAS_EXTRAS;
const FLAG_BASE: u32 = 1 << 0;
const FLAG_DELTA: u32 = 1 << 1;
const FLAG_COMPACTED: u32 = 1 << 2;
const FLAG_HAS_FOREIGN: u32 = 1 << 3;
const FLAG_HAS_TOMBSTONES: u32 = 1 << 4;
const SECTION_CRITICAL: u16 = 1 << 0;
const SECTION_BLOCKSUMMED: u16 = 1 << 1;
const BOOK_ENTRY_LEN: usize = 40;
const EXTRA_LEN: usize = 8;

// format.rs SectionKind codes
const S_BOOKS: u16 = 0x0001;
const S_BOOK_NAMES: u16 = 0x0002;
const S_CODEC_PARAMS: u16 = 0x0003;
const S_BOOK_HASHES: u16 = 0x0004;
const S_SLOT_KEYS: u16 = 0x0010;
const S_SLOT_HINTS: u16 = 0x0011;
const S_VECTORS: u16 = 0x0012;
const S_VECTOR_SCALES: u16 = 0x0013;
const S_SLOT_WINDOWS: u16 = 0x0014;
const S_EXTRAS: u16 = 0x0020;
const S_EXTRAS_BY_SLOT: u16 = 0x0021;
const S_FOREIGN: u16 = 0x0022;
const S_TOMBSTONES: u16 = 0x0023;
const S_OCCURRENCES: u16 = 0x0024;
const S_BLOCK_CRCS: u16 = 0x007F;

/// occurrence.rs `ALIAS_WINDOW` / `ROLE_ALIAS`.
pub(crate) const ALIAS_WINDOW: u8 = u8::MAX;
const ROLE_ALIAS: u8 = 1;

fn key_len(format_version: u32) -> usize {
    if format_version >= 3 {
        8
    } else {
        16
    }
}
fn foreign_len(version: u32) -> usize {
    key_len(version) + 8
}
fn occurrence_len(version: u32) -> usize {
    2 * key_len(version) + 12
}
fn align_up(offset: u64, align: u64) -> u64 {
    offset.div_ceil(align) * align
}

/// distribution/package.rs `PackageKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Base,
    Delta,
    Compacted,
}

impl PackageKind {
    fn flag(self) -> u32 {
        match self {
            PackageKind::Base => FLAG_BASE,
            PackageKind::Delta => FLAG_DELTA,
            PackageKind::Compacted => FLAG_COMPACTED,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            PackageKind::Base => "base",
            PackageKind::Delta => "delta",
            PackageKind::Compacted => "compacted",
        }
    }
}

/// The vectors' encoding, as the segments being copied carry it: codec id, width, the
/// CODEC_PARAMS bytes and their SHA-256 (the header's codec epoch), bytes per vector and
/// whether each vector has its own scale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecSpec {
    pub id: u16,
    pub dim: u16,
    pub params: Vec<u8>,
    pub params_sha256: [u8; 32],
    pub bytes_per_vector: usize,
    pub has_vector_scales: bool,
    /// format.rs `default_format_version`: 3 for rot192-i8i4, else 2.
    pub default_format_version: u32,
}

/// occurrence.rs `Occurrence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Occurrence {
    pub key: Key,
    pub book: u32,
    pub hint: u32,
    pub anchor: Key,
    pub window: u8,
}

impl Occurrence {
    pub fn is_alias(&self) -> bool {
        self.window == ALIAS_WINDOW
    }
    fn encode_with(&self, key_len: usize, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.key[..key_len]);
        out.extend_from_slice(&self.book.to_le_bytes());
        out.extend_from_slice(&self.hint.to_le_bytes());
        out.extend_from_slice(&self.anchor[..key_len]);
        if self.is_alias() {
            out.extend_from_slice(&[0, ROLE_ALIAS, 0, 0]);
        } else {
            out.extend_from_slice(&[self.window, 0, 0, 0]);
        }
    }
}

/// writer.rs `SegmentSpec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentSpec {
    pub kind: PackageKind,
    pub identity_digest: [u8; 32],
    pub from_library_version: u32,
    pub to_library_version: u32,
    pub library_release_tag: String,
}

#[derive(Debug, Clone, Copy)]
struct BookEntry {
    name_off: u32,
    name_len: u16,
    slot_start: u32,
    slot_count: u32,
    extra_start: u32,
    extra_count: u32,
    foreign_start: u32,
    foreign_count: u32,
}

impl BookEntry {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name_off.to_le_bytes());
        out.extend_from_slice(&self.name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&self.slot_start.to_le_bytes());
        out.extend_from_slice(&self.slot_count.to_le_bytes());
        out.extend_from_slice(&self.extra_start.to_le_bytes());
        out.extend_from_slice(&self.extra_count.to_le_bytes());
        out.extend_from_slice(&self.foreign_start.to_le_bytes());
        out.extend_from_slice(&self.foreign_count.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
    }
}

#[derive(Debug, Clone, Copy)]
struct DirectoryEntry {
    kind: u16,
    flags: u16,
    elem_size: u32,
    offset: u64,
    length: u64,
    elem_count: u64,
    crc32: u32,
}

impl DirectoryEntry {
    fn encode(&self, out: &mut [u8]) {
        out[0..2].copy_from_slice(&self.kind.to_le_bytes());
        out[2..4].copy_from_slice(&self.flags.to_le_bytes());
        out[4..8].copy_from_slice(&self.elem_size.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.length.to_le_bytes());
        out[24..32].copy_from_slice(&self.elem_count.to_le_bytes());
        out[32..36].copy_from_slice(&self.crc32.to_le_bytes());
        out[36..40].fill(0);
    }
}

/// format.rs `Header`.
#[derive(Debug, Clone)]
struct Header {
    format_version: u32,
    flags: u32,
    dim: u16,
    codec_id: u16,
    block_size: u32,
    segment_id: [u8; 16],
    identity_digest: [u8; 32],
    codec_params_sha256: [u8; 32],
    slot_count: u64,
    extra_count: u64,
    foreign_count: u64,
    tombstone_count: u64,
    book_count: u32,
    from_library_version: u32,
    to_library_version: u32,
    library_release_tag: String,
    sections: Vec<DirectoryEntry>,
}

impl Header {
    fn encode(&self) -> Vec<u8> {
        assert!(self.sections.len() <= MAX_SECTIONS, "the directory holds 32");
        let mut out = vec![0u8; HEADER_LEN];
        out[0..8].copy_from_slice(&MAGIC);
        assert!(READABLE_FORMATS.contains(&self.format_version), "a format this build writes");
        out[8..12].copy_from_slice(&self.format_version.to_le_bytes());
        out[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        out[16..20].copy_from_slice(&self.flags.to_le_bytes());
        out[20..22].copy_from_slice(&self.dim.to_le_bytes());
        out[22..24].copy_from_slice(&self.codec_id.to_le_bytes());
        out[24..28].copy_from_slice(&self.block_size.to_le_bytes());
        out[28..32].copy_from_slice(&(self.sections.len() as u32).to_le_bytes());
        out[32..48].copy_from_slice(&self.segment_id);
        out[48..80].copy_from_slice(&self.identity_digest);
        out[80..112].copy_from_slice(&self.codec_params_sha256);
        out[112..120].copy_from_slice(&self.slot_count.to_le_bytes());
        out[120..128].copy_from_slice(&self.extra_count.to_le_bytes());
        out[128..136].copy_from_slice(&self.foreign_count.to_le_bytes());
        out[136..144].copy_from_slice(&self.tombstone_count.to_le_bytes());
        out[144..148].copy_from_slice(&self.book_count.to_le_bytes());
        out[148..152].copy_from_slice(&self.from_library_version.to_le_bytes());
        out[152..156].copy_from_slice(&self.to_library_version.to_le_bytes());
        let tag = self.library_release_tag.as_bytes();
        assert!(tag.len() <= RELEASE_TAG_LEN, "a release tag fits 64 bytes");
        out[RELEASE_TAG_OFFSET..RELEASE_TAG_OFFSET + tag.len()].copy_from_slice(tag);
        for (index, entry) in self.sections.iter().enumerate() {
            let at = DIRECTORY_OFFSET + index * DIRECTORY_ENTRY_LEN;
            entry.encode(&mut out[at..at + DIRECTORY_ENTRY_LEN]);
        }
        let crc = crc32fast::hash(&out[..HEADER_CRC_OFFSET]);
        out[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        out
    }
}

/// package.rs `validate_release_tag`: at most 64 bytes, no control characters.
fn validate_release_tag(tag: &str) -> io::Result<()> {
    if tag.len() > RELEASE_TAG_LEN || tag.chars().any(char::is_control) {
        return Err(invalid(format!("a release tag is at most {RELEASE_TAG_LEN} bytes with no control characters, and {tag:?} is not")));
    }
    Ok(())
}

/// package.rs `PackageCounts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PackageCounts {
    pub books: u32,
    pub slots: u64,
    pub extras: u64,
    pub foreign: u64,
    pub tombstones: u64,
}

/// writer.rs `WrittenSegment`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenSegment {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: [u8; 32],
    pub segment_id: [u8; 16],
    pub spec: SegmentSpec,
    pub counts: PackageCounts,
}

/// writer.rs `SegmentBuilder`.
pub struct SegmentBuilder {
    spec: SegmentSpec,
    codec: CodecSpec,
    format_version: Option<u32>,
    books: Vec<BookEntry>,
    book_hashes: Vec<u64>,
    has_book_hashes: bool,
    names: Vec<u8>,
    keys: Vec<Key>,
    hints: Vec<u32>,
    windows: Vec<u8>,
    occurrences: Vec<Occurrence>,
    occurrence_slots: Vec<u64>,
    extras: Vec<(u32, u32)>,
    foreign: Vec<(Key, u32, u32)>,
    tombstones: Vec<Key>,
}

impl SegmentBuilder {
    pub fn new(spec: SegmentSpec, codec: CodecSpec) -> Self {
        Self {
            spec,
            codec,
            format_version: None,
            books: Vec::new(),
            book_hashes: Vec::new(),
            has_book_hashes: false,
            names: Vec::new(),
            keys: Vec::new(),
            hints: Vec::new(),
            windows: Vec::new(),
            occurrences: Vec::new(),
            occurrence_slots: Vec::new(),
            extras: Vec::new(),
            foreign: Vec::new(),
            tombstones: Vec::new(),
        }
    }

    /// writer.rs `add_book_windowed_hashed`: the next book, in strictly increasing byte
    /// order of names. Returns the book's first slot.
    pub fn add_book_windowed_hashed(
        &mut self,
        name: &str,
        primary: &[(Key, u32)],
        windows: &[u8],
        extras: &[(u32, u32)],
        foreign: &[(Key, u32)],
        source_hash: Option<u64>,
    ) -> io::Result<u32> {
        if !windows.is_empty() && windows.len() != primary.len() {
            return Err(invalid(format!("book {name:?} has {} primary record(s) and {} window(s)", primary.len(), windows.len())));
        }
        let first_slot = self.keys.len() as u32;
        if primary.is_empty() && extras.is_empty() && foreign.is_empty() && source_hash.is_none() {
            return Ok(first_slot);
        }
        if name.is_empty() || name.len() > u16::MAX as usize {
            return Err(invalid(format!("a book name is 1 to 65535 bytes, and {name:?} is {}", name.len())));
        }
        if let Some(last) = self.books.last() {
            let previous = &self.names[last.name_off as usize..][..last.name_len as usize];
            if name.as_bytes() <= previous {
                return Err(invalid(format!(
                    "books are added in increasing byte order, and {name:?} follows {:?}",
                    String::from_utf8_lossy(previous)
                )));
            }
        }
        let too_large = |hint: u32| hint > HINT_MASK;
        if primary.iter().any(|(_, h)| too_large(*h)) || extras.iter().any(|(h, _)| too_large(*h)) || foreign.iter().any(|(_, h)| too_large(*h)) {
            return Err(invalid(format!("a hint of book {name:?} is past {HINT_MASK}, which bit 31 of a slot's hint cannot share")));
        }
        let total = |count: usize| u32::try_from(count).map_err(|_| invalid("too many records"));
        let entry = BookEntry {
            name_off: total(self.names.len())?,
            name_len: name.len() as u16,
            slot_start: first_slot,
            slot_count: total(primary.len())?,
            extra_start: total(self.extras.len())?,
            extra_count: total(extras.len())?,
            foreign_start: total(self.foreign.len())?,
            foreign_count: total(foreign.len())?,
        };
        total(self.keys.len() + primary.len())?;
        total(self.extras.len() + extras.len())?;
        total(self.foreign.len() + foreign.len())?;
        total(self.names.len() + name.len())?;

        self.names.extend_from_slice(name.as_bytes());
        for (key, hint) in primary {
            self.keys.push(*key);
            self.hints.push(*hint);
        }
        if windows.is_empty() {
            self.windows.resize(self.keys.len(), 0);
        } else {
            self.windows.extend_from_slice(windows);
        }
        let mut sorted: Vec<(u32, u32)> = extras.to_vec();
        sorted.sort_unstable();
        self.extras.extend(sorted.into_iter().map(|(hint, slot)| (slot, hint)));
        let mut sorted: Vec<(u32, Key)> = foreign.iter().map(|(k, h)| (*h, *k)).collect();
        sorted.sort_unstable();
        let book = self.books.len() as u32;
        self.foreign.extend(sorted.into_iter().map(|(hint, key)| (key, book, hint)));
        self.books.push(entry);
        self.book_hashes.push(source_hash.unwrap_or(0));
        self.has_book_hashes |= source_hash.is_some();
        Ok(first_slot)
    }

    /// writer.rs `add_occurrences`: this book's complete snapshots, after its records.
    pub fn add_occurrences(&mut self, name: &str, occurrences: &[Occurrence]) -> io::Result<()> {
        if occurrences.is_empty() {
            return Ok(());
        }
        let book = self.books.len().checked_sub(1).ok_or_else(|| invalid("occurrences need a book"))?;
        let entry = self.books[book];
        if &self.names[entry.name_off as usize..][..entry.name_len as usize] != name.as_bytes() {
            return Err(invalid("occurrences must follow their book"));
        }
        let mut referenced: HashMap<Key, Option<u32>> = HashMap::new();
        for slot in entry.slot_start..entry.slot_start + entry.slot_count {
            referenced.insert(self.keys[slot as usize], Some(slot));
        }
        for (slot, _) in &self.extras[entry.extra_start as usize..][..entry.extra_count as usize] {
            referenced.insert(self.keys[*slot as usize], Some(*slot));
        }
        for (key, _, _) in &self.foreign[entry.foreign_start as usize..][..entry.foreign_count as usize] {
            referenced.entry(*key).or_insert(None);
        }
        self.occurrence_slots.resize(self.keys.len().div_ceil(64), 0);
        for record in occurrences {
            let Some(slot) = referenced.get(&record.key) else {
                return Err(invalid("an occurrence needs a vector record in its book"));
            };
            if record.is_alias() {
                continue;
            }
            if let Some(slot) = slot {
                let slot = *slot as usize;
                let (word, bit) = (slot / 64, slot % 64);
                self.windows[slot] = if self.occurrence_slots[word] & (1u64 << bit) != 0 {
                    self.windows[slot].min(record.window)
                } else {
                    record.window
                };
                self.occurrence_slots[word] |= 1u64 << bit;
            }
        }
        self.occurrences.extend(occurrences.iter().map(|record| {
            let mut record = *record;
            record.book = book as u32;
            record
        }));
        Ok(())
    }

    /// writer.rs `set_tombstones`.
    pub fn set_tombstones(&mut self, mut keys: Vec<Key>) {
        keys.sort_unstable();
        keys.dedup();
        self.tombstones = keys;
    }

    /// writer.rs `minimize_slot_window`.
    pub fn minimize_slot_window(&mut self, slot: u32, window: u8) {
        self.windows[slot as usize] = self.windows[slot as usize].min(window);
    }

    /// writer.rs `set_format_version`.
    pub fn set_format_version(&mut self, version: u32) -> io::Result<()> {
        if !READABLE_FORMATS.contains(&version) {
            return Err(invalid(format!("segment format {version} is not one this build writes")));
        }
        self.format_version = Some(version);
        Ok(())
    }

    pub fn slot_count(&self) -> u32 {
        self.keys.len() as u32
    }

    /// writer.rs `write`: lay the file out at `path` (which must not exist), write every
    /// table, and return the sink the vectors go through.
    pub fn write(mut self, path: &Path) -> io::Result<VectorSink> {
        validate_release_tag(&self.spec.library_release_tag)?;
        let slot_count = self.keys.len() as u32;
        if let Some((slot, _)) = self.extras.iter().find(|(slot, _)| *slot >= slot_count) {
            return Err(invalid(format!("an extra record names slot {slot}, and the segment has {slot_count}")));
        }
        let format_version = self.format_version.unwrap_or(self.codec.default_format_version);
        let key_len = key_len(format_version);
        let has_foreign = !self.foreign.is_empty();
        let has_tombstones = !self.tombstones.is_empty();
        if self.spec.kind != PackageKind::Delta && (has_foreign || has_tombstones) {
            return Err(invalid(format!("a {} carries no foreign records and no tombstones", self.spec.kind.name())));
        }

        let mut hints = std::mem::take(&mut self.hints);
        for (slot, _) in &self.extras {
            hints[*slot as usize] |= HAS_EXTRAS;
        }
        let mut by_slot: Vec<u32> = (0..self.extras.len() as u32).collect();
        by_slot.sort_unstable_by_key(|index| (self.extras[*index as usize].0, *index));

        let mut books = Vec::with_capacity(self.books.len() * BOOK_ENTRY_LEN);
        for book in &self.books {
            book.encode(&mut books);
        }
        let mut extras = Vec::with_capacity(self.extras.len() * EXTRA_LEN);
        for (slot, hint) in &self.extras {
            extras.extend_from_slice(&slot.to_le_bytes());
            extras.extend_from_slice(&hint.to_le_bytes());
        }
        let mut extras_by_slot = Vec::with_capacity(by_slot.len() * 4);
        for index in &by_slot {
            extras_by_slot.extend_from_slice(&index.to_le_bytes());
        }
        let mut foreign = Vec::with_capacity(self.foreign.len() * foreign_len(format_version));
        for (key, book, hint) in &self.foreign {
            foreign.extend_from_slice(&key[..key_len]);
            foreign.extend_from_slice(&book.to_le_bytes());
            foreign.extend_from_slice(&hint.to_le_bytes());
        }
        let mut narrow: Vec<&[u8]> = self.tombstones.iter().map(|key| &key[..key_len]).collect();
        narrow.dedup();
        let tombstone_count = narrow.len();
        let tombstones: Vec<u8> = narrow.concat();
        let hint_bytes: Vec<u8> = hints.iter().flat_map(|hint| hint.to_le_bytes()).collect();
        let key_bytes: Vec<u8> = self.keys.iter().flat_map(|key| &key[..key_len]).copied().collect();

        let vector_bytes = self.codec.bytes_per_vector as u64;
        let vectors_len = u64::from(slot_count) * vector_bytes;
        let scales_len = if self.codec.has_vector_scales { u64::from(slot_count) * 4 } else { 0 };
        let windows = std::mem::take(&mut self.windows);
        let window_bytes = windows.iter().any(|w| *w != 0).then_some(windows);
        let windows_len = window_bytes.as_ref().map_or(0, |b| b.len() as u64);
        // Rows sort as the sidecar's 44-byte rows decode: key, book, hint, anchor, window.
        self.occurrences.sort_unstable();
        let mut current = None;
        let mut parents = HashSet::new();
        self.occurrences.retain(|record| {
            let group = (record.key, record.book);
            if current != Some(group) {
                current = Some(group);
                parents.clear();
            }
            parents.insert((record.anchor, record.window))
        });
        let mut occurrence_bytes = Vec::with_capacity(self.occurrences.len() * occurrence_len(format_version));
        for row in &self.occurrences {
            if row.is_alias() && key_len == 16 {
                return Err(invalid("an alias occurrence needs segment format 3"));
            }
            row.encode_with(key_len, &mut occurrence_bytes);
        }
        let occurrence_count = self.occurrences.len() as u64;
        let occurrences_len = occurrence_bytes.len() as u64;
        let book_hash_bytes: Vec<u8> =
            if self.has_book_hashes { self.book_hashes.iter().flat_map(|h| h.to_le_bytes()).collect() } else { Vec::new() };
        let book_hashes_len = book_hash_bytes.len() as u64;
        let blocks = |len: u64| len.div_ceil(BLOCK_SIZE as u64);
        let crc_count = blocks(hint_bytes.len() as u64)
            + blocks(key_bytes.len() as u64)
            + blocks(vectors_len)
            + blocks(scales_len)
            + blocks(windows_len)
            + blocks(occurrences_len)
            + blocks(book_hashes_len);

        // The layout: small metadata first, vectors last (kind, elem size, length, count, summed).
        let params_len = self.codec.params.len() as u64;
        let mut plan: Vec<(u16, u32, u64, u64, bool)> = vec![
            (S_BOOKS, BOOK_ENTRY_LEN as u32, books.len() as u64, self.books.len() as u64, false),
            (S_BOOK_NAMES, 1, self.names.len() as u64, self.names.len() as u64, false),
            (S_CODEC_PARAMS, 1, params_len, params_len, false),
            (S_EXTRAS, EXTRA_LEN as u32, extras.len() as u64, self.extras.len() as u64, false),
            (S_EXTRAS_BY_SLOT, 4, extras_by_slot.len() as u64, by_slot.len() as u64, false),
            (S_FOREIGN, foreign_len(format_version) as u32, foreign.len() as u64, self.foreign.len() as u64, false),
            (S_TOMBSTONES, key_len as u32, tombstones.len() as u64, tombstone_count as u64, false),
            (S_BLOCK_CRCS, 4, crc_count * 4, crc_count, false),
            (S_SLOT_HINTS, 4, hint_bytes.len() as u64, u64::from(slot_count), true),
            (S_SLOT_KEYS, key_len as u32, key_bytes.len() as u64, u64::from(slot_count), true),
            (S_VECTORS, vector_bytes as u32, vectors_len, u64::from(slot_count), true),
        ];
        if self.codec.has_vector_scales {
            plan.push((S_VECTOR_SCALES, 4, scales_len, u64::from(slot_count), true));
        }
        if window_bytes.is_some() {
            plan.push((S_SLOT_WINDOWS, 1, windows_len, u64::from(slot_count), true));
        }
        if occurrence_count > 0 {
            plan.push((S_OCCURRENCES, occurrence_len(format_version) as u32, occurrences_len, occurrence_count, true));
        }
        if self.has_book_hashes {
            plan.push((S_BOOK_HASHES, 8, book_hashes_len, self.books.len() as u64, true));
        }
        let mut offset = HEADER_LEN as u64;
        let mut sections = Vec::with_capacity(plan.len());
        for (kind, elem_size, length, elem_count, blocksummed) in plan {
            let align = if kind == S_VECTORS { VECTORS_ALIGN } else { SECTION_ALIGN };
            offset = align_up(offset, align);
            sections.push(DirectoryEntry {
                kind,
                flags: SECTION_CRITICAL | if blocksummed { SECTION_BLOCKSUMMED } else { 0 },
                elem_size,
                offset,
                length,
                elem_count,
                crc32: 0,
            });
            offset += length;
        }

        let compacted = self.spec.kind == PackageKind::Compacted;
        let segment_id = if compacted {
            [0; 16]
        } else {
            segment_id(
                &self.spec.identity_digest,
                self.spec.kind,
                self.spec.from_library_version,
                self.spec.to_library_version,
                &Sha256::digest(&key_bytes).into(),
            )
        };
        let mut flags = self.spec.kind.flag();
        if has_foreign {
            flags |= FLAG_HAS_FOREIGN;
        }
        if has_tombstones {
            flags |= FLAG_HAS_TOMBSTONES;
        }
        let mut header = Header {
            format_version,
            flags,
            dim: self.codec.dim,
            codec_id: self.codec.id,
            block_size: BLOCK_SIZE as u32,
            segment_id,
            identity_digest: self.spec.identity_digest,
            codec_params_sha256: self.codec.params_sha256,
            slot_count: u64::from(slot_count),
            extra_count: self.extras.len() as u64,
            foreign_count: self.foreign.len() as u64,
            tombstone_count: tombstone_count as u64,
            book_count: self.books.len() as u32,
            from_library_version: self.spec.from_library_version,
            to_library_version: self.spec.to_library_version,
            library_release_tag: self.spec.library_release_tag.clone(),
            sections,
        };

        let file = OpenOptions::new().write(true).read(true).create_new(true).open(path)?;
        let mut out = Positioned { file: BufWriter::with_capacity(4 << 20, file), at: 0 };
        out.write_all(&[0u8; HEADER_LEN])?;
        let contents: [&[u8]; 7] = [&books, &self.names, &self.codec.params, &extras, &extras_by_slot, &foreign, &tombstones];
        let mut section_sha256 = compacted.then(|| vec![[0u8; 32]; header.sections.len()]);
        for (index, bytes) in contents.into_iter().enumerate() {
            out.pad_to(header.sections[index].offset)?;
            out.write_all(bytes)?;
            header.sections[index].crc32 = crc32fast::hash(bytes);
            if let Some(digests) = &mut section_sha256 {
                digests[index] = Sha256::digest(bytes).into();
            }
        }
        if let Some(digests) = &mut section_sha256 {
            digests[8] = Sha256::digest(&hint_bytes).into();
            digests[9] = Sha256::digest(&key_bytes).into();
        }
        let mut block_crcs = Vec::with_capacity(crc_count as usize);
        block_crcs.extend(hint_bytes.chunks(BLOCK_SIZE).map(crc32fast::hash));
        block_crcs.extend(key_bytes.chunks(BLOCK_SIZE).map(crc32fast::hash));
        out.pad_to(header.sections[7].offset)?;
        out.write_all(&vec![0u8; header.sections[7].length as usize])?;
        out.pad_to(header.sections[8].offset)?;
        out.write_all(&hint_bytes)?;
        out.pad_to(header.sections[9].offset)?;
        out.write_all(&key_bytes)?;
        out.pad_to(header.sections[10].offset)?;

        let counts = PackageCounts {
            books: header.book_count,
            slots: header.slot_count,
            extras: header.extra_count,
            foreign: header.foreign_count,
            tombstones: header.tombstone_count,
        };
        let has_scales = self.codec.has_vector_scales;
        Ok(VectorSink {
            out,
            path: path.to_path_buf(),
            header,
            spec: self.spec,
            bytes_per_vector: self.codec.bytes_per_vector,
            counts,
            expected: u64::from(slot_count),
            written: 0,
            block: crc32fast::Hasher::new(),
            in_block: 0,
            block_crcs,
            vectors_sha256: compacted.then(Sha256::new),
            section_sha256,
            scales: has_scales.then(|| Vec::with_capacity(scales_len as usize)),
            windows: window_bytes,
            occurrences: occurrence_bytes,
            book_hashes: book_hash_bytes,
        })
    }
}

/// writer.rs `VectorSink`: the vectors in slot order, then [`Self::finish`].
pub struct VectorSink {
    out: Positioned,
    path: PathBuf,
    header: Header,
    spec: SegmentSpec,
    bytes_per_vector: usize,
    counts: PackageCounts,
    expected: u64,
    written: u64,
    block: crc32fast::Hasher,
    in_block: usize,
    block_crcs: Vec<u32>,
    section_sha256: Option<Vec<[u8; 32]>>,
    vectors_sha256: Option<Sha256>,
    scales: Option<Vec<u8>>,
    windows: Option<Vec<u8>>,
    occurrences: Vec<u8>,
    book_hashes: Vec<u8>,
}

impl VectorSink {
    /// writer.rs `push_encoded`.
    pub fn push_encoded(&mut self, encoded: &[u8], scale: Option<f32>) -> io::Result<()> {
        match (&self.scales, scale) {
            (Some(_), Some(scale)) if scale.is_finite() && scale >= 0.0 => {}
            (Some(_), _) => return Err(invalid("this codec needs each vector's scale, finite and not negative")),
            (None, Some(_)) => return Err(invalid("this codec has no scale per vector")),
            (None, None) => {}
        }
        if encoded.len() != self.bytes_per_vector {
            return Err(invalid(format!("a vector of this codec is {} bytes, and this one is {}", self.bytes_per_vector, encoded.len())));
        }
        if self.written == self.expected {
            return Err(invalid(format!("the segment has {} slot(s), and every one has its vector", self.expected)));
        }
        self.out.write_all(encoded)?;
        if let Some(hasher) = &mut self.vectors_sha256 {
            hasher.update(encoded);
        }
        if let (Some(scales), Some(scale)) = (&mut self.scales, scale) {
            scales.extend_from_slice(&scale.to_le_bytes());
        }
        let mut rest = encoded;
        while !rest.is_empty() {
            let take = rest.len().min(BLOCK_SIZE - self.in_block);
            self.block.update(&rest[..take]);
            self.in_block += take;
            rest = &rest[take..];
            if self.in_block == BLOCK_SIZE {
                let full = std::mem::replace(&mut self.block, crc32fast::Hasher::new());
                self.block_crcs.push(full.finalize());
                self.in_block = 0;
            }
        }
        self.written += 1;
        Ok(())
    }

    fn put_section(&mut self, kind: u16, bytes: &[u8]) -> io::Result<()> {
        let entry = self.header.sections.iter().position(|e| e.kind == kind).expect("the section is laid out");
        self.out.pad_to(self.header.sections[entry].offset)?;
        self.out.write_all(bytes)?;
        self.block_crcs.extend(bytes.chunks(BLOCK_SIZE).map(crc32fast::hash));
        if let Some(digests) = &mut self.section_sha256 {
            digests[entry] = Sha256::digest(bytes).into();
        }
        Ok(())
    }

    /// writer.rs `finish`.
    pub fn finish(mut self) -> io::Result<WrittenSegment> {
        if self.written != self.expected {
            return Err(invalid(format!("the segment has {} slot(s), and {} vector(s) arrived", self.expected, self.written)));
        }
        if self.in_block > 0 {
            self.block_crcs.push(self.block.clone().finalize());
        }
        if let Some(scales) = self.scales.take() {
            self.put_section(S_VECTOR_SCALES, &scales)?;
        }
        if let Some(windows) = self.windows.take() {
            self.put_section(S_SLOT_WINDOWS, &windows)?;
        }
        if !self.occurrences.is_empty() {
            let occurrences = std::mem::take(&mut self.occurrences);
            self.put_section(S_OCCURRENCES, &occurrences)?;
        }
        if self.header.sections.iter().any(|e| e.kind == S_BOOK_HASHES) {
            let hashes = std::mem::take(&mut self.book_hashes);
            self.put_section(S_BOOK_HASHES, &hashes)?;
        }
        let crc_bytes: Vec<u8> = self.block_crcs.iter().flat_map(|crc| crc.to_le_bytes()).collect();
        let crc_entry = &mut self.header.sections[7];
        if crc_bytes.len() as u64 != crc_entry.length {
            return Err(invalid("the block checksums do not fill their section"));
        }
        crc_entry.crc32 = crc32fast::hash(&crc_bytes);
        let crc_offset = crc_entry.offset;
        if let (Some(mut digests), Some(vectors)) = (self.section_sha256.take(), self.vectors_sha256.take()) {
            digests[7] = Sha256::digest(&crc_bytes).into();
            digests[10] = vectors.finalize().into();
            self.header.segment_id = compacted_segment_id(&self.header, &digests);
        }
        let mut file = self.out.file.into_inner().map_err(|e| e.into_error())?;
        file.seek(SeekFrom::Start(crc_offset))?;
        file.write_all(&crc_bytes)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&self.header.encode())?;
        file.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        let mut size = 0u64;
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size += read as u64;
        }
        Ok(WrittenSegment {
            path: self.path,
            size,
            sha256: hasher.finalize().into(),
            segment_id: self.header.segment_id,
            spec: self.spec,
            counts: self.counts,
        })
    }
}

/// writer.rs `segment_id`: the id a release is installed under.
pub fn segment_id(identity_digest: &[u8; 32], kind: PackageKind, from: u32, to: u32, keys_sha256: &[u8; 32]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(b"oxv-segment-id");
    hasher.update(identity_digest);
    hasher.update([kind.flag() as u8]);
    hasher.update(from.to_le_bytes());
    hasher.update(to.to_le_bytes());
    hasher.update(keys_sha256);
    hasher.finalize()[..16].try_into().expect("16 bytes")
}

/// writer.rs `compacted_segment_id`: the id a segment compacted on a device goes under.
fn compacted_segment_id(header: &Header, section_sha256: &[[u8; 32]]) -> [u8; 16] {
    debug_assert_eq!(header.sections.len(), section_sha256.len());
    let mut unnamed = header.clone();
    unnamed.segment_id = [0; 16];
    let mut hasher = Sha256::new();
    hasher.update(b"oxv-compacted-segment-id\n");
    hasher.update(&unnamed.encode()[..HEADER_CRC_OFFSET]);
    for digest in section_sha256 {
        hasher.update(digest);
    }
    hasher.finalize()[..16].try_into().expect("16 bytes")
}

struct Positioned {
    file: BufWriter<File>,
    at: u64,
}

impl Positioned {
    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.file.write_all(bytes)?;
        self.at += bytes.len() as u64;
        Ok(())
    }
    fn pad_to(&mut self, offset: u64) -> io::Result<()> {
        debug_assert!(offset >= self.at, "sections are laid out in file order");
        let zeros = [0u8; 4096];
        while self.at < offset {
            let take = ((offset - self.at) as usize).min(zeros.len());
            self.write_all(&zeros[..take])?;
        }
        Ok(())
    }
}

fn invalid(reason: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, reason.into())
}
