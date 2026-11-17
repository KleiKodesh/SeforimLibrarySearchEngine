//! Reader for Otzaria's vector segments (`.oxv`, segment formats 2 and 3 of
//! `otzaria-semantic-search`, `src/semantic/oxv/format.rs` + `reader.rs`):
//! base, compacted and delta segments, read-only. Format only: no model, no
//! database. The set they form (base + deltas, `.del` bitmaps, links) is
//! [`crate::set`]; the exact scan over them is [`crate::scan`].
//!
//! A segment holds, per slot, one encoded vector, the key of the text it was
//! embedded from and a hint: the 0-based ordinal of the line in its book
//! (Otzaria's index counts a book's lines in seforim.db `lineIndex, id`
//! order). Records say which book and line hold a slot's vector:
//! - primary: the book whose slot range holds the slot, at the slot's hint;
//! - extra: a further occurrence of the slot's key `{slot, hint}`;
//! - foreign (delta only): `{key, book, hint}`, a record whose vector an
//!   older segment of the set holds (resolved by the set's links);
//! - occurrences (format 3 windows): a complete snapshot for a key, replacing
//!   its other records; a row's window is the chunk of a long line, and an
//!   alias row is a short line borrowing a neighbour's vector.
//!
//! Book names are Otzaria's index `filePath` (`id:<bookId>` for a library
//! book). The HOST maps each name to its book id and line ids
//! ([`OxvBookMap`], kept by [`crate::set::LineMap`]).
//!
//! The header's CRC and the CRCs of the small sections are checked; the
//! block-summed sections (keys, hints, vectors) are mapped unchecked, as
//! Otzaria does at open.

use std::collections::HashMap;
use std::fs::File;
use std::ops::Range;
use std::path::Path;

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::{Error, Result, CHUNK_BORROWED};

const MAGIC: &[u8; 8] = b"OXVSEG\r\n";
const HEADER_LEN: usize = 4096;
const HEADER_CRC_OFFSET: usize = 4088;
const DIRECTORY_OFFSET: usize = 224;
const DIRECTORY_ENTRY_LEN: usize = 40;
const MAX_SECTIONS: usize = 32;
const RELEASE_TAG_OFFSET: usize = 160;
const RELEASE_TAG_LEN: usize = 64;
const BOOK_ENTRY_LEN: usize = 40;
/// format.rs: bit 31 of a hint says the slot's key has extra records.
const HAS_EXTRAS: u32 = 1 << 31;
const HINT_MASK: u32 = !HAS_EXTRAS;
const SECTION_CRITICAL: u16 = 1;
const SECTION_BLOCKSUMMED: u16 = 2;
const FLAG_BASE: u32 = 1;
const FLAG_DELTA: u32 = 2;
const FLAG_COMPACTED: u32 = 4;
/// occurrence.rs: the window of an alias occurrence.
pub(crate) const ALIAS_WINDOW: u8 = u8::MAX;
const ROLE_ALIAS: u8 = 1;

// section kinds (format.rs SectionKind)
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
const S_TIER0_SIGN: u16 = 0x0030;
const S_TIER0_PQ: u16 = 0x0031;
const S_BLOCK_CRCS: u16 = 0x007F;
const KNOWN: [u16; 17] = [
    S_BOOKS, S_BOOK_NAMES, S_CODEC_PARAMS, S_BOOK_HASHES, S_SLOT_KEYS, S_SLOT_HINTS, S_VECTORS, S_VECTOR_SCALES, S_SLOT_WINDOWS,
    S_EXTRAS, S_EXTRAS_BY_SLOT, S_FOREIGN, S_TOMBSTONES, S_OCCURRENCES, S_TIER0_SIGN, S_TIER0_PQ, S_BLOCK_CRCS,
];

// codec ids (codec.rs)
pub const CODEC_F32: u16 = 0x0001;
pub const CODEC_I8_SYM_DIM: u16 = 0x0101;
pub const CODEC_I8_SYM_VEC: u16 = 0x0102;
pub const CODEC_ROT_I8I4: u16 = 0x0103;
const PARAMS_VERSION: u32 = 1;

/// A chunk key: 16 bytes (format 2), or 8 bytes padded with zeros (format 3,
/// Otzaria's `ChunkKey::narrowed`); compared as bytes.
pub(crate) type Key = [u8; 16];

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn f32_at(b: &[u8], at: usize) -> f32 {
    f32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn bad(msg: impl Into<String>) -> Error {
    Error::Format(format!("oxv: {}", msg.into()))
}

/// How a slot's bytes decode to a vector.
#[derive(Debug, Clone)]
pub(crate) enum Codec {
    F32,
    I8SymVec,
    I8SymDim { scales: Vec<f32> },
    Rot { head: usize, tail: usize, scale: Vec<f32>, lo: Vec<f32>, hi: Vec<f32>, rotation: Vec<f32> },
}

impl Codec {
    fn parse(id: u16, dim: usize, p: &[u8]) -> Result<Codec> {
        if p.len() < 8 || u32_at(p, 0) != PARAMS_VERSION || u32_at(p, 4) as usize != dim {
            return Err(bad("codec parameters of another version or width"));
        }
        let floats = |from: usize| -> Vec<f32> { (from..p.len()).step_by(4).map(|at| f32_at(p, at)).collect() };
        match id {
            CODEC_F32 if p.len() == 8 => Ok(Codec::F32),
            CODEC_I8_SYM_VEC if p.len() == 8 => Ok(Codec::I8SymVec),
            CODEC_I8_SYM_DIM if p.len() == 12 + 4 * dim => Ok(Codec::I8SymDim { scales: floats(12) }),
            CODEC_ROT_I8I4 if p.len() >= 20 => {
                let (out, head, tail) = (u32_at(p, 8) as usize, u32_at(p, 12) as usize, u32_at(p, 16) as usize);
                if head + tail != out || out > dim || tail % 2 != 0 || p.len() != 20 + 4 * head + 8 * tail + 4 * out * dim {
                    return Err(bad("rot192-i8i4 parameters do not fit their layout"));
                }
                let f = floats(20);
                let (scale, rest) = f.split_at(head);
                let (lo, rest) = rest.split_at(tail);
                let (hi, rotation) = rest.split_at(tail);
                Ok(Codec::Rot { head, tail, scale: scale.into(), lo: lo.into(), hi: hi.into(), rotation: rotation.into() })
            }
            other => Err(bad(format!("codec {other:#06x} with {} parameter bytes is not supported", p.len()))),
        }
    }

    pub(crate) fn bytes_per_vector(&self, dim: usize) -> usize {
        match self {
            Codec::F32 => 4 * dim,
            Codec::I8SymVec | Codec::I8SymDim { .. } => dim,
            Codec::Rot { head, tail, .. } => head + tail / 2,
        }
    }

    fn has_scales(&self) -> bool {
        matches!(self, Codec::I8SymVec | Codec::Rot { .. })
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Codec::F32 => "f32",
            Codec::I8SymVec => "i8-sym-vec",
            Codec::I8SymDim { .. } => "i8-sym-dim",
            Codec::Rot { .. } => "rot192-i8i4",
        }
    }

    /// Otzaria's `Codec::decode`, step for step.
    fn decode(&self, bytes: &[u8], scale: f32, out: &mut [f32]) {
        match self {
            Codec::F32 => {
                for (d, v) in out.iter_mut().enumerate() {
                    *v = f32_at(bytes, 4 * d);
                }
            }
            Codec::I8SymVec => {
                for (v, &b) in out.iter_mut().zip(bytes) {
                    *v = f32::from(b as i8) * scale;
                }
            }
            Codec::I8SymDim { scales } => {
                for ((v, &b), s) in out.iter_mut().zip(bytes).zip(scales) {
                    *v = f32::from(b as i8) * s / 127.0;
                }
            }
            Codec::Rot { head, tail, scale: spread, lo, hi, rotation } => {
                let mut y: Vec<f64> = Vec::with_capacity(head + tail);
                y.extend(bytes[..*head].iter().zip(spread).map(|(&b, s)| f64::from(f32::from(b as i8) * scale * s)));
                for (j, &b) in bytes[*head..].iter().enumerate() {
                    for (half, code) in [b & 0x0F, b >> 4].into_iter().enumerate() {
                        let d = 2 * j + half;
                        y.push(f64::from(lo[d] + f32::from(code) * ((hi[d] - lo[d]) / 15.0)));
                    }
                }
                let dim = out.len();
                for (d, v) in out.iter_mut().enumerate() {
                    let mut sum = 0f64;
                    for (row, yv) in rotation.chunks_exact(dim).zip(&y) {
                        sum += f64::from(row[d]) * yv;
                    }
                    *v = sum as f32;
                }
            }
        }
    }
}

/// One book of a segment (reader.rs `SegmentBook`).
#[derive(Debug, Clone)]
pub(crate) struct Book {
    pub name: String,
    pub slots: Range<u32>,
    pub extras: Range<u32>,
    pub foreign: Range<u32>,
}

/// One occurrence row (occurrence.rs `Occurrence`, without the anchor).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Occ {
    pub book: u32,
    pub hint: u32,
    pub window: u8,
    /// An alias row's own key (the short line's); the row's key otherwise.
    pub anchor: Key,
}

/// What a `.oxv` holds (JSON at the C ABI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OxvInfo {
    /// Segment format: 2 (16-byte keys) or 3 (8-byte keys).
    pub format: u32,
    /// "base", "compacted" or "delta".
    pub kind: String,
    pub dims: usize,
    pub codec: String,
    pub library_release_tag: String,
    pub from_library_version: u32,
    pub to_library_version: u32,
    pub slots: u64,
    pub extras: u64,
    pub occurrences: u64,
    /// Foreign records (a delta's records whose vector an older segment holds).
    #[serde(default)]
    pub foreign: u64,
    /// Keys a delta removes.
    #[serde(default)]
    pub tombstones: u64,
    pub books: Vec<OxvBookInfo>,
}

/// One book of a `.oxv`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OxvBookInfo {
    /// Otzaria's index filePath (`id:<bookId>` for a library book).
    pub name: String,
    /// Records (line occurrences of a vector, chunks included).
    pub records: u64,
    /// Highest line ordinal + 1 (the line count the host's mapping must cover).
    pub lines: u64,
}

/// The host's mapping of one Otzaria book (JSON at the C ABI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OxvBookMap {
    /// The book's name in the vectors (`id:<bookId>` for a library book).
    pub name: String,
    /// The host's book id.
    pub book_id: u64,
    /// The host's line id of each line ordinal (0-based), as the vectors
    /// count the book's lines (seforim.db `lineIndex, id` order).
    pub line_ids: Vec<u64>,
}

/// An open `.oxv` segment.
pub struct OxvFile {
    map: Mmap,
    format: u32,
    kind: &'static str,
    dims: usize,
    codec: Codec,
    params: Range<usize>,
    key_len: usize,
    slots: u32,
    tag: String,
    from: u32,
    to: u32,
    books: Vec<Book>,
    keys: Range<usize>,
    hints: Range<usize>,
    vectors: Range<usize>,
    scales: Option<Range<usize>>,
    windows: Option<Range<usize>>,
    extras: Range<usize>,
    extras_by_slot: Range<usize>,
    foreign: Range<usize>,
    foreign_count: u32,
    tombstone_count: u64,
    tombstones: Range<usize>,
    occurrences: Option<Range<usize>>,
    book_hashes: Option<Range<usize>>,
}

impl OxvFile {
    /// Maps and checks a `.oxv` (plain; expand a `.oxv.zst` with [`expand_zst`] first).
    pub fn open(path: &Path) -> Result<OxvFile> {
        let file = File::open(path)?;
        // SAFETY: read only; the host must not rewrite the file while it is open
        // (Otzaria's segments never change once installed).
        let map = unsafe { Mmap::map(&file)? };
        Self::parse(map)
    }

    fn parse(map: Mmap) -> Result<OxvFile> {
        let b = &map[..];
        if b.len() < HEADER_LEN || &b[..8] != MAGIC {
            return Err(bad("not a segment file (magic)"));
        }
        let format = u32_at(b, 8);
        if format != 2 && format != 3 {
            return Err(bad(format!("segment format {format}; formats 2 and 3 are read")));
        }
        if u32_at(b, 12) as usize != HEADER_LEN {
            return Err(bad("header length"));
        }
        if crc32fast::hash(&b[..HEADER_CRC_OFFSET]) != u32_at(b, HEADER_CRC_OFFSET) {
            return Err(bad("header CRC"));
        }
        let flags = u32_at(b, 16);
        let kind = match flags & (FLAG_BASE | FLAG_DELTA | FLAG_COMPACTED) {
            FLAG_BASE => "base",
            FLAG_COMPACTED => "compacted",
            FLAG_DELTA => "delta",
            _ => return Err(bad("no single segment kind")),
        };
        let dims = u16_at(b, 20) as usize;
        let codec_id = u16_at(b, 22);
        let sections = u32_at(b, 28) as usize;
        if sections > MAX_SECTIONS || dims == 0 {
            return Err(bad("header fields"));
        }
        let slots64 = u64_at(b, 112);
        let extra_count = u64_at(b, 120);
        let foreign_count = u64_at(b, 128);
        let tombstone_count = u64_at(b, 136);
        let book_count = u32_at(b, 144) as usize;
        let tag_bytes = &b[RELEASE_TAG_OFFSET..RELEASE_TAG_OFFSET + RELEASE_TAG_LEN];
        let tag_len = tag_bytes.iter().position(|&c| c == 0).unwrap_or(RELEASE_TAG_LEN);
        let tag = String::from_utf8(tag_bytes[..tag_len].to_vec()).map_err(|_| bad("release tag"))?;
        let (from, to) = (u32_at(b, 148), u32_at(b, 152));
        if slots64 > u32::MAX as u64 || extra_count > u32::MAX as u64 || foreign_count > u32::MAX as u64 {
            return Err(bad("counts"));
        }
        // reader.rs: only a delta holds foreign records or tombstones
        if kind != "delta" && (foreign_count > 0 || tombstone_count > 0) {
            return Err(bad(format!("a {kind} holds foreign records or tombstones")));
        }
        let slots = slots64 as u32;
        let key_len = if format >= 3 { 8 } else { 16 };

        let mut dir: HashMap<u16, (Range<usize>, u64, u32, u16)> = HashMap::new();
        for i in 0..sections {
            let at = DIRECTORY_OFFSET + i * DIRECTORY_ENTRY_LEN;
            let kind = u16_at(b, at);
            let sflags = u16_at(b, at + 2);
            let off = u64_at(b, at + 8);
            let len = u64_at(b, at + 16);
            let count = u64_at(b, at + 24);
            let crc = u32_at(b, at + 32);
            let end = off.checked_add(len).ok_or_else(|| bad("section range"))?;
            if end > b.len() as u64 || off < HEADER_LEN as u64 {
                return Err(bad(format!("section {kind:#06x} lies outside the file")));
            }
            if !KNOWN.contains(&kind) {
                if sflags & SECTION_CRITICAL != 0 {
                    return Err(bad(format!("unknown critical section {kind:#06x}")));
                }
                continue;
            }
            dir.insert(kind, (off as usize..end as usize, count, crc, sflags));
        }
        let section = |kind: u16, required: bool| -> Result<Option<Range<usize>>> {
            match dir.get(&kind) {
                None if required => Err(bad(format!("section {kind:#06x} missing"))),
                None => Ok(None),
                Some((range, _, crc, sflags)) => {
                    if sflags & SECTION_BLOCKSUMMED == 0 && crc32fast::hash(&b[range.clone()]) != *crc {
                        return Err(bad(format!("section {kind:#06x} CRC")));
                    }
                    Ok(Some(range.clone()))
                }
            }
        };
        let need = |range: &Range<usize>, n: u64, size: u64, what: &str| -> Result<()> {
            if n.checked_mul(size) != Some(range.len() as u64) {
                return Err(bad(format!("{what} section size")));
            }
            Ok(())
        };
        let books_r = section(S_BOOKS, true)?.unwrap();
        let names_r = section(S_BOOK_NAMES, true)?.unwrap();
        let params = section(S_CODEC_PARAMS, true)?.unwrap();
        let keys = section(S_SLOT_KEYS, true)?.unwrap();
        let hints = section(S_SLOT_HINTS, true)?.unwrap();
        let vectors = section(S_VECTORS, true)?.unwrap();
        let scales = section(S_VECTOR_SCALES, false)?;
        let windows = section(S_SLOT_WINDOWS, false)?;
        let extras = section(S_EXTRAS, extra_count > 0)?.unwrap_or(0..0);
        let extras_by_slot = section(S_EXTRAS_BY_SLOT, extra_count > 0)?.unwrap_or(0..0);
        let foreign = section(S_FOREIGN, foreign_count > 0)?.unwrap_or(0..0);
        let tombstones = section(S_TOMBSTONES, tombstone_count > 0)?.unwrap_or(0..0);
        let occurrences = section(S_OCCURRENCES, false)?;
        let book_hashes = section(S_BOOK_HASHES, false)?;
        let codec = Codec::parse(codec_id, dims, &b[params.clone()])?;
        need(&books_r, book_count as u64, BOOK_ENTRY_LEN as u64, "BOOKS")?;
        need(&keys, slots64, key_len as u64, "SLOT_KEYS")?;
        need(&hints, slots64, 4, "SLOT_HINTS")?;
        need(&vectors, slots64, codec.bytes_per_vector(dims) as u64, "VECTORS")?;
        need(&extras, extra_count, 8, "EXTRAS")?;
        need(&extras_by_slot, extra_count, 4, "EXTRAS_BY_SLOT")?;
        need(&foreign, foreign_count, key_len as u64 + 8, "FOREIGN")?;
        need(&tombstones, tombstone_count, key_len as u64, "TOMBSTONES")?;
        match (&scales, codec.has_scales()) {
            (Some(r), true) => need(r, slots64, 4, "VECTOR_SCALES")?,
            (None, true) => return Err(bad("VECTOR_SCALES missing for a scaled codec")),
            _ => {}
        }
        if let Some(r) = &windows {
            need(r, slots64, 1, "SLOT_WINDOWS")?;
        }
        if let Some(r) = &book_hashes {
            need(r, book_count as u64, 8, "BOOK_HASHES")?;
        }
        if let Some(r) = &occurrences {
            if r.len() % (2 * key_len + 12) != 0 {
                return Err(bad("OCCURRENCES section size"));
            }
        }
        let names = &b[names_r];
        let mut books = Vec::with_capacity(book_count);
        let (mut next_slot, mut next_extra, mut next_foreign) = (0u32, 0u32, 0u32);
        for i in 0..book_count {
            let e = &b[books_r.start + i * BOOK_ENTRY_LEN..books_r.start + (i + 1) * BOOK_ENTRY_LEN];
            let (noff, nlen) = (u32_at(e, 0) as usize, u16_at(e, 4) as usize);
            let name = names
                .get(noff..noff + nlen)
                .and_then(|n| std::str::from_utf8(n).ok())
                .ok_or_else(|| bad("book name"))?
                .to_string();
            let (ss, sc, es, ec, fs, fc) = (u32_at(e, 8), u32_at(e, 12), u32_at(e, 16), u32_at(e, 20), u32_at(e, 24), u32_at(e, 28));
            // the ranges tile their sections, in book order
            if ss != next_slot || es != next_extra || fs != next_foreign {
                return Err(bad("book ranges do not tile the slots / extras / foreign records"));
            }
            next_slot = ss.checked_add(sc).ok_or_else(|| bad("slot range"))?;
            next_extra = es.checked_add(ec).ok_or_else(|| bad("extra range"))?;
            next_foreign = fs.checked_add(fc).ok_or_else(|| bad("foreign range"))?;
            if let Some(prev) = books.last() {
                let prev: &Book = prev;
                if prev.name.as_bytes() >= name.as_bytes() {
                    return Err(bad("books are not sorted by name"));
                }
            }
            books.push(Book { name, slots: ss..next_slot, extras: es..next_extra, foreign: fs..next_foreign });
        }
        if next_slot != slots || next_extra as u64 != extra_count || next_foreign as u64 != foreign_count {
            return Err(bad("book ranges do not cover the slots / extras / foreign records"));
        }
        Ok(OxvFile {
            map,
            format,
            kind,
            dims,
            codec,
            params,
            key_len,
            slots,
            tag,
            from,
            to,
            books,
            keys,
            hints,
            vectors,
            scales,
            windows,
            extras,
            extras_by_slot,
            foreign,
            foreign_count: foreign_count as u32,
            tombstone_count,
            tombstones,
            occurrences,
            book_hashes,
        })
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    /// "base", "compacted" or "delta".
    pub fn kind(&self) -> &'static str {
        self.kind
    }

    pub fn slot_count(&self) -> u32 {
        self.slots
    }

    pub fn release_tag(&self) -> &str {
        &self.tag
    }

    pub fn library_versions(&self) -> (u32, u32) {
        (self.from, self.to)
    }

    /// The file's size in bytes.
    pub fn size(&self) -> u64 {
        self.map.len() as u64
    }

    pub(crate) fn codec(&self) -> &Codec {
        &self.codec
    }

    /// The codec parameters as stored (equal bytes = one codec epoch).
    pub(crate) fn codec_params(&self) -> &[u8] {
        &self.map[self.params.clone()]
    }

    pub(crate) fn books(&self) -> &[Book] {
        &self.books
    }

    /// reader.rs `book_of_slot`.
    pub(crate) fn book_of_slot(&self, slot: u32) -> usize {
        self.books.partition_point(|book| book.slots.end <= slot)
    }

    /// reader.rs `book_of_extra`.
    pub(crate) fn book_of_extra(&self, index: u32) -> usize {
        self.books.partition_point(|book| book.extras.end <= index)
    }

    fn raw_hint(&self, slot: u32) -> u32 {
        u32_at(&self.map, self.hints.start + slot as usize * 4)
    }

    pub(crate) fn hint(&self, slot: u32) -> u32 {
        self.raw_hint(slot) & HINT_MASK
    }

    pub(crate) fn has_extras(&self, slot: u32) -> bool {
        self.raw_hint(slot) & HAS_EXTRAS != 0
    }

    pub(crate) fn slot_window(&self, slot: u32) -> u8 {
        self.windows.as_ref().map_or(0, |w| self.map[w.start + slot as usize])
    }

    fn key_at(&self, at: usize) -> Key {
        let mut key = [0u8; 16];
        key[..self.key_len].copy_from_slice(&self.map[at..at + self.key_len]);
        key
    }

    fn key_at_row(&self, row: &[u8], at: usize) -> Key {
        let mut key = [0u8; 16];
        key[..self.key_len].copy_from_slice(&row[at..at + self.key_len]);
        key
    }

    pub(crate) fn key(&self, slot: u32) -> Key {
        self.key_at(self.keys.start + slot as usize * self.key_len)
    }

    pub(crate) fn vector_bytes(&self) -> &[u8] {
        &self.map[self.vectors.clone()]
    }

    pub(crate) fn vector_scale_bytes(&self) -> Option<&[u8]> {
        self.scales.as_ref().map(|r| &self.map[r.clone()])
    }

    /// Extra record `index`: `(slot, hint)`.
    pub(crate) fn extra(&self, i: u32) -> (u32, u32) {
        let at = self.extras.start + i as usize * 8;
        (u32_at(&self.map, at), u32_at(&self.map, at + 4) & HINT_MASK)
    }

    fn extra_count(&self) -> u32 {
        (self.extras.len() / 8) as u32
    }

    fn extra_by_slot(&self, position: u32) -> u32 {
        u32_at(&self.map, self.extras_by_slot.start + position as usize * 4)
    }

    /// reader.rs `extras_of_slot`: the extra records of one slot's key.
    pub(crate) fn extras_of_slot(&self, slot: u32) -> impl Iterator<Item = u32> + '_ {
        let count = self.extra_count();
        let (mut low, mut high) = (0u32, count);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.extra(self.extra_by_slot(middle)).0 < slot {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        (low..count).map(move |p| self.extra_by_slot(p)).take_while(move |i| self.extra(*i).0 == slot)
    }

    /// The whole mapped file (install: header fields, block CRCs).
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.map
    }

    /// The segment format (2 or 3).
    pub(crate) fn format(&self) -> u32 {
        self.format
    }

    pub(crate) fn tombstone_count(&self) -> u32 {
        self.tombstone_count as u32
    }

    /// Tombstone `index`: the key a delta retires.
    pub(crate) fn tombstone(&self, index: u32) -> Key {
        self.key_at(self.tombstones.start + index as usize * self.key_len)
    }

    pub(crate) fn foreign_count(&self) -> u32 {
        self.foreign_count
    }

    /// Foreign record `index`: `(key, book, hint)`.
    pub(crate) fn foreign_record(&self, index: u32) -> (Key, u32, u32) {
        let at = self.foreign.start + index as usize * (self.key_len + 8);
        (self.key_at(at), u32_at(&self.map, at + self.key_len), u32_at(&self.map, at + self.key_len + 4) & HINT_MASK)
    }

    fn occurrence_rows(&self) -> &[u8] {
        self.occurrences.as_ref().map_or(&[][..], |r| &self.map[r.clone()])
    }

    pub(crate) fn has_occurrences(&self) -> bool {
        !self.occurrence_rows().is_empty()
    }

    /// reader.rs `key_occurrences`: the complete snapshot for `key` (rows in
    /// key order, then as stored), or None when this segment does not update it.
    pub(crate) fn key_occurrences(&self, key: &Key) -> Option<Vec<Occ>> {
        let rows = self.occurrence_rows();
        if rows.is_empty() {
            return None;
        }
        let k = self.key_len;
        // a key of the other width finds nothing (a set holds one width)
        if k < 16 && key[8..] != [0u8; 8] {
            return None;
        }
        let len = 2 * k + 12;
        let n = rows.len() / len;
        let row = |i: usize| &rows[i * len..(i + 1) * len];
        let start = partition(n, |i| row(i)[..k] < key[..k]);
        let end = partition(n, |i| row(i)[..k] <= key[..k]);
        if start == end {
            return None;
        }
        Some(
            (start..end)
                .map(|i| {
                    let r = row(i);
                    let window = if r[2 * k + 9] == ROLE_ALIAS { ALIAS_WINDOW } else { r[2 * k + 8] };
                    Occ { book: u32_at(r, k), hint: u32_at(r, k + 4) & HINT_MASK, window, anchor: self.key_at_row(r, k + 8) }
                })
                .collect(),
        )
    }

    /// Every record of book `b` as (ordinal, window), this segment alone:
    /// primary and extra records (a key with a snapshot taking its snapshot
    /// rows for this book instead) and foreign records. For [`Self::info`].
    fn records_of(&self, b: usize) -> Vec<(u32, u8)> {
        let book = &self.books[b];
        let mut out = Vec::new();
        let mut snapshot_done: std::collections::HashSet<Key> = std::collections::HashSet::new();
        let has_occ = self.has_occurrences();
        let mut push_slot = |slot: u32, hint: u32, out: &mut Vec<(u32, u8)>| {
            if has_occ {
                let key = self.key(slot);
                if let Some(rows) = self.key_occurrences(&key) {
                    if snapshot_done.insert(key) {
                        out.extend(rows.into_iter().filter(|r| r.book as usize == b).map(|r| (r.hint, r.window)));
                    }
                    return;
                }
            }
            out.push((hint, self.slot_window(slot)));
        };
        for slot in book.slots.clone() {
            push_slot(slot, self.hint(slot), &mut out);
        }
        for i in book.extras.clone() {
            let (slot, hint) = self.extra(i);
            if slot < self.slots {
                push_slot(slot, hint, &mut out);
            }
        }
        for i in book.foreign.clone() {
            out.push((self.foreign_record(i).2, 0));
        }
        out
    }

    /// reader.rs `book_source_hash`: book `name`'s source text hash, or None (no such
    /// book, no BOOK_HASHES, or a hash of 0 = unknown).
    pub(crate) fn book_source_hash(&self, name: &str) -> Option<u64> {
        let index = self.books.binary_search_by(|b| b.name.as_str().cmp(name)).ok()?;
        let range = self.book_hashes.as_ref()?;
        let hash = u64_at(&self.map, range.start + index * 8);
        (hash != 0).then_some(hash)
    }

    /// The header's identity digest (bytes 48..80).
    pub(crate) fn identity_digest(&self) -> [u8; 32] {
        self.map[48..80].try_into().unwrap()
    }

    /// The header's codec id and codec epoch (CODEC_PARAMS SHA-256).
    pub(crate) fn codec_header(&self) -> (u16, [u8; 32]) {
        (u16_at(&self.map, 22), self.map[80..112].try_into().unwrap())
    }

    /// Slot `slot`'s encoded vector bytes and its own scale (codecs with one).
    pub(crate) fn encoded_vector(&self, slot: u32) -> (&[u8], Option<f32>) {
        let n = self.codec.bytes_per_vector(self.dims);
        let at = self.vectors.start + slot as usize * n;
        let scale = self.scales.as_ref().map(|r| f32_at(&self.map, r.start + slot as usize * 4));
        (&self.map[at..at + n], scale)
    }

    /// The codec's bytes per vector and whether each vector has a scale.
    pub(crate) fn codec_layout(&self) -> (usize, bool) {
        (self.codec.bytes_per_vector(self.dims), self.codec.has_scales())
    }

    /// The decoded (not normalized) vector of `slot`.
    pub fn vector(&self, slot: u32) -> Vec<f32> {
        let n = self.codec.bytes_per_vector(self.dims);
        let at = self.vectors.start + slot as usize * n;
        let scale = self.scales.as_ref().map_or(1.0, |r| f32_at(&self.map, r.start + slot as usize * 4));
        let mut out = vec![0f32; self.dims];
        self.codec.decode(&self.map[at..at + n], scale, &mut out);
        out
    }

    /// What the file holds.
    pub fn info(&self) -> OxvInfo {
        let books = (0..self.books.len())
            .map(|b| {
                let recs = self.records_of(b);
                OxvBookInfo {
                    name: self.books[b].name.clone(),
                    records: recs.len() as u64,
                    lines: recs.iter().map(|r| r.0 as u64 + 1).max().unwrap_or(0),
                }
            })
            .collect();
        OxvInfo {
            format: self.format,
            kind: self.kind.to_string(),
            dims: self.dims,
            codec: self.codec.name().to_string(),
            library_release_tag: self.tag.clone(),
            from_library_version: self.from,
            to_library_version: self.to,
            slots: self.slots as u64,
            extras: self.extra_count() as u64,
            occurrences: (self.occurrence_rows().len() / (2 * self.key_len + 12)) as u64,
            foreign: self.foreign_count as u64,
            tombstones: self.tombstone_count,
            books,
        }
    }
}

/// The chunk number of a window: 0 a line's first window, n its n-th,
/// [`CHUNK_BORROWED`] an alias (a short line borrowing a neighbour's vector).
pub(crate) fn chunk_of_window(window: u8) -> u32 {
    if window == ALIAS_WINDOW {
        CHUNK_BORROWED
    } else {
        window as u32
    }
}

/// The first index in `0..n` where `pred` is false (pred true then false).
fn partition(n: usize, pred: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pred(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// Expands Otzaria's download form (`.oxv.zst`) to a plain `.oxv` at `dst`
/// (written to `dst.tmp`, then renamed). Returns the bytes written.
pub fn expand_zst(src: &Path, dst: &Path) -> Result<u64> {
    let mut tmp = dst.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let result = (|| -> Result<u64> {
        let mut input = zstd::stream::read::Decoder::new(std::io::BufReader::new(File::open(src)?))?;
        let mut out = std::io::BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        let n = std::io::copy(&mut input, &mut out)?;
        let file = out.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        file.sync_all()?;
        Ok(n)
    })();
    match result {
        Ok(n) => {
            std::fs::rename(&tmp, dst)?;
            Ok(n)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// test writers (here, the engine, the C ABI and the hosts' self-tests)

/// Writes a small segment in Otzaria's layout, for tests: one base segment,
/// codec f32 (`rot` false) or rot192-i8i4 with the identity rotation (`rot`
/// true, head = dims - tail, tail = 4). `books`: (name, records as (ordinal,
/// chunk, vector)); each record is one slot; a record of window > 0 or an
/// alias makes the file format 3 with occurrence snapshots.
#[doc(hidden)]
pub fn write_test_segment(path: &Path, dims: usize, rot: bool, tag: &str, books: &[(&str, Vec<(u32, u32, Vec<f32>)>)]) -> Result<()> {
    let format: u32 = if books.iter().any(|(_, r)| r.iter().any(|x| x.1 != 0)) { 3 } else { 2 };
    let key_len = if format == 3 { 8 } else { 16 };
    let mut sorted: Vec<&(&str, Vec<(u32, u32, Vec<f32>)>)> = books.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut names = Vec::new();
    let mut book_rows = Vec::new();
    let mut keys = Vec::new();
    let mut hints = Vec::new();
    let mut vecs: Vec<Vec<f32>> = Vec::new();
    let mut windows = Vec::new();
    let mut occ: Vec<(Vec<u8>, u32, u32, u8, bool)> = Vec::new();
    for (bi, (name, recs)) in sorted.iter().enumerate() {
        let start = hints.len() as u32;
        for (ordinal, chunk, v) in recs {
            let slot = hints.len() as u64;
            let mut key = vec![0u8; key_len];
            key[..8].copy_from_slice(&(slot + 1).to_be_bytes());
            let (window, alias) = if *chunk == CHUNK_BORROWED { (0u8, true) } else { (*chunk as u8, false) };
            keys.extend_from_slice(&key);
            hints.push(*ordinal);
            windows.push(if alias { 0 } else { window });
            vecs.push(v.clone());
            if format == 3 {
                occ.push((key, bi as u32, *ordinal, window, alias));
            }
        }
        let n = hints.len() as u32 - start;
        book_rows.push((names.len() as u32, name.len() as u16, start, n, 0u32, 0u32, 0u32, 0u32));
        names.extend_from_slice(name.as_bytes());
    }
    let codec = if rot { TestCodec::Rot } else { TestCodec::F32 };
    let (codec_id, params, vbytes, scales) = encode_vectors(codec, dims, &vecs);
    let occ_bytes = occurrence_bytes(&mut occ);
    let hint_bytes: Vec<u8> = hints.iter().flat_map(|h| h.to_le_bytes()).collect();
    let mut sections: Vec<(u16, u16, u32, u64, Vec<u8>)> = vec![
        (S_BOOKS, SECTION_CRITICAL, BOOK_ENTRY_LEN as u32, book_rows.len() as u64, book_bytes(&book_rows)),
        (S_BOOK_NAMES, SECTION_CRITICAL, 0, 0, names),
        (S_CODEC_PARAMS, SECTION_CRITICAL, 0, 0, params),
        (S_SLOT_KEYS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, key_len as u32, hints.len() as u64, keys),
        (S_SLOT_HINTS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, 4, hints.len() as u64, hint_bytes),
        (S_VECTORS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, 0, hints.len() as u64, vbytes),
        (S_SLOT_WINDOWS, 0, 1, hints.len() as u64, windows),
    ];
    if let Some(s) = scales {
        sections.push((S_VECTOR_SCALES, SECTION_CRITICAL, 4, hints.len() as u64, s));
    }
    if format == 3 {
        sections.push((S_OCCURRENCES, SECTION_CRITICAL | SECTION_BLOCKSUMMED, (2 * key_len + 12) as u32, occ.len() as u64, occ_bytes));
    }
    let header = Head { format, flags: FLAG_BASE, dims, codec_id, slots: hints.len() as u64, extras: 0, foreign: 0, tombstones: 0, books: book_rows.len() as u32, from: 0, to: 1, tag };
    assemble(path, &header, &sections)
}

/// The codec a test segment is written with.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestCodec {
    F32,
    /// i8-sym-vec: one scale per vector (max |x| / 127).
    I8Vec,
    /// rot192-i8i4, identity rotation, tail 4 (lo -1, hi 1), head spreads 1.
    Rot,
}

/// One slot of a test segment: key (written big-endian in 8 bytes, format 3),
/// hint (line ordinal), window and vector.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct TestSlot {
    pub key: u64,
    pub hint: u32,
    pub window: u8,
    pub vector: Vec<f32>,
}

/// One book of a test segment. Books must be given sorted by name; slots
/// are numbered across books in order. `extras`: (global slot, hint);
/// `foreign`: (key, hint), a delta's records of older slots.
#[doc(hidden)]
#[derive(Debug, Clone, Default)]
pub struct TestBook {
    pub name: String,
    pub slots: Vec<TestSlot>,
    pub extras: Vec<(u32, u32)>,
    pub foreign: Vec<(u64, u32)>,
}

/// A format-3 test segment (base or delta): see [`write_segment`].
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct TestSegment {
    pub dims: usize,
    pub codec: TestCodec,
    pub delta: bool,
    pub from: u32,
    pub to: u32,
    pub tag: String,
    pub books: Vec<TestBook>,
    /// Occurrence snapshot rows: (key, book index, hint, window, alias).
    pub occurrences: Vec<(u64, u32, u32, u8, bool)>,
    pub tombstones: Vec<u64>,
}

/// Writes a format-3 segment from `seg` (tests: deltas, extras, foreign
/// records, tombstones, the three codecs).
#[doc(hidden)]
pub fn write_segment(path: &Path, seg: &TestSegment) -> Result<()> {
    let key_len = 8usize;
    let key8 = |k: u64| k.to_be_bytes().to_vec();
    let mut names = Vec::new();
    let mut rows = Vec::new();
    let (mut keys, mut hints, mut windows, mut vecs) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut extras: Vec<(u32, u32)> = Vec::new();
    let mut foreign = Vec::new();
    let with_extras: std::collections::HashSet<u32> = seg.books.iter().flat_map(|b| b.extras.iter().map(|e| e.0)).collect();
    for book in &seg.books {
        let (ss, es, fs) = (hints.len() as u32, extras.len() as u32, (foreign.len() / (key_len + 8)) as u32);
        for s in &book.slots {
            let slot = hints.len() as u32;
            keys.extend_from_slice(&key8(s.key));
            hints.push(s.hint | if with_extras.contains(&slot) { HAS_EXTRAS } else { 0 });
            windows.push(s.window);
            vecs.push(s.vector.clone());
        }
        extras.extend_from_slice(&book.extras);
        for (k, h) in &book.foreign {
            foreign.extend_from_slice(&key8(*k));
            foreign.extend_from_slice(&(rows.len() as u32).to_le_bytes());
            foreign.extend_from_slice(&h.to_le_bytes());
        }
        let fc = (foreign.len() / (key_len + 8)) as u32 - fs;
        rows.push((names.len() as u32, book.name.len() as u16, ss, hints.len() as u32 - ss, es, extras.len() as u32 - es, fs, fc));
        names.extend_from_slice(book.name.as_bytes());
    }
    let slots = hints.len() as u64;
    let (codec_id, params, vbytes, scales) = encode_vectors(seg.codec, seg.dims, &vecs);
    let mut by_slot: Vec<u32> = (0..extras.len() as u32).collect();
    by_slot.sort_by_key(|&i| (extras[i as usize].0, i));
    let extra_bytes: Vec<u8> = extras.iter().flat_map(|(s, h)| [s.to_le_bytes(), h.to_le_bytes()].concat()).collect();
    let by_slot_bytes: Vec<u8> = by_slot.iter().flat_map(|i| i.to_le_bytes()).collect();
    let mut occ: Vec<(Vec<u8>, u32, u32, u8, bool)> = seg.occurrences.iter().map(|&(k, b, h, w, a)| (key8(k), b, h, w, a)).collect();
    let occ_bytes = occurrence_bytes(&mut occ);
    let mut tomb = seg.tombstones.clone();
    tomb.sort_unstable();
    let tomb_bytes: Vec<u8> = tomb.iter().flat_map(|k| key8(*k)).collect();
    let hint_bytes: Vec<u8> = hints.iter().flat_map(|h| h.to_le_bytes()).collect();
    let fcount = (foreign.len() / (key_len + 8)) as u64;
    let mut sections: Vec<(u16, u16, u32, u64, Vec<u8>)> = vec![
        (S_BOOKS, SECTION_CRITICAL, BOOK_ENTRY_LEN as u32, rows.len() as u64, book_bytes(&rows)),
        (S_BOOK_NAMES, SECTION_CRITICAL, 0, 0, names),
        (S_CODEC_PARAMS, SECTION_CRITICAL, 0, 0, params),
        (S_SLOT_KEYS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, key_len as u32, slots, keys),
        (S_SLOT_HINTS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, 4, slots, hint_bytes),
        (S_VECTORS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, 0, slots, vbytes),
        (S_SLOT_WINDOWS, 0, 1, slots, windows),
    ];
    if let Some(s) = scales {
        sections.push((S_VECTOR_SCALES, SECTION_CRITICAL, 4, slots, s));
    }
    if !extras.is_empty() {
        sections.push((S_EXTRAS, SECTION_CRITICAL, 8, extras.len() as u64, extra_bytes));
        sections.push((S_EXTRAS_BY_SLOT, SECTION_CRITICAL, 4, extras.len() as u64, by_slot_bytes));
    }
    if fcount > 0 {
        sections.push((S_FOREIGN, SECTION_CRITICAL, (key_len + 8) as u32, fcount, foreign));
    }
    if !tomb.is_empty() {
        sections.push((S_TOMBSTONES, SECTION_CRITICAL, key_len as u32, tomb.len() as u64, tomb_bytes));
    }
    if !occ.is_empty() {
        sections.push((S_OCCURRENCES, SECTION_CRITICAL | SECTION_BLOCKSUMMED, (2 * key_len + 12) as u32, occ.len() as u64, occ_bytes));
    }
    let header = Head {
        format: 3,
        flags: if seg.delta { FLAG_DELTA } else { FLAG_BASE },
        dims: seg.dims,
        codec_id,
        slots,
        extras: extras.len() as u64,
        foreign: fcount,
        tombstones: tomb.len() as u64,
        books: rows.len() as u32,
        from: seg.from,
        to: seg.to,
        tag: &seg.tag,
    };
    assemble(path, &header, &sections)
}

type BookRow = (u32, u16, u32, u32, u32, u32, u32, u32);

fn book_bytes(rows: &[BookRow]) -> Vec<u8> {
    let mut out = Vec::new();
    for &(noff, nlen, ss, sc, es, ec, fs, fc) in rows {
        out.extend_from_slice(&noff.to_le_bytes());
        out.extend_from_slice(&nlen.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        for v in [ss, sc, es, ec, fs, fc] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&[0u8; 8]);
    }
    out
}

fn occurrence_bytes(occ: &mut [(Vec<u8>, u32, u32, u8, bool)]) -> Vec<u8> {
    occ.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut out = Vec::new();
    for (key, book, hint, window, alias) in occ.iter() {
        out.extend_from_slice(key);
        out.extend_from_slice(&book.to_le_bytes());
        out.extend_from_slice(&hint.to_le_bytes());
        out.extend_from_slice(key);
        out.extend_from_slice(&if *alias { [0, ROLE_ALIAS, 0, 0] } else { [*window, 0, 0, 0] });
    }
    out
}

/// (codec id, params, vector bytes, scale bytes) of `vecs` under `codec`.
fn encode_vectors(codec: TestCodec, dims: usize, vecs: &[Vec<f32>]) -> (u16, Vec<u8>, Vec<u8>, Option<Vec<u8>>) {
    let mut params = Vec::new();
    params.extend_from_slice(&PARAMS_VERSION.to_le_bytes());
    params.extend_from_slice(&(dims as u32).to_le_bytes());
    let mut vbytes = Vec::new();
    let mut scales = Vec::new();
    let i8_of = |x: f32, s: f32| if s > 0.0 { (x / s).round().clamp(-127.0, 127.0) as i8 as u8 } else { 0 };
    match codec {
        TestCodec::F32 => {
            for v in vecs {
                for x in v {
                    vbytes.extend_from_slice(&x.to_le_bytes());
                }
            }
            (CODEC_F32, params, vbytes, None)
        }
        TestCodec::I8Vec => {
            for v in vecs {
                let s = v.iter().fold(0f32, |m, x| m.max(x.abs())) / 127.0;
                scales.push(s);
                vbytes.extend(v.iter().map(|x| i8_of(*x, s)));
            }
            (CODEC_I8_SYM_VEC, params, vbytes, Some(scales.iter().flat_map(|s| s.to_le_bytes()).collect()))
        }
        TestCodec::Rot => {
            let tail = 4usize;
            let head = dims - tail;
            for v in [dims as u32, head as u32, tail as u32] {
                params.extend_from_slice(&v.to_le_bytes());
            }
            let spread = vec![1.0f32; head];
            let (lo, hi) = (vec![-1.0f32; tail], vec![1.0f32; tail]);
            let mut rotation = vec![0f32; dims * dims];
            for d in 0..dims {
                rotation[d * dims + d] = 1.0;
            }
            for x in spread.iter().chain(&lo).chain(&hi).chain(&rotation) {
                params.extend_from_slice(&x.to_le_bytes());
            }
            for v in vecs {
                let s = v[..head].iter().fold(0f32, |m, x| m.max(x.abs())) / 127.0;
                scales.push(s);
                vbytes.extend(v[..head].iter().map(|x| i8_of(*x, s)));
                for j in 0..tail / 2 {
                    let code = |x: f32| ((x + 1.0) / 2.0 * 15.0).round().clamp(0.0, 15.0) as u8;
                    vbytes.push(code(v[head + 2 * j]) | (code(v[head + 2 * j + 1]) << 4));
                }
            }
            (CODEC_ROT_I8I4, params, vbytes, Some(scales.iter().flat_map(|s| s.to_le_bytes()).collect()))
        }
    }
}

struct Head<'a> {
    format: u32,
    flags: u32,
    dims: usize,
    codec_id: u16,
    slots: u64,
    extras: u64,
    foreign: u64,
    tombstones: u64,
    books: u32,
    from: u32,
    to: u32,
    tag: &'a str,
}

fn assemble(path: &Path, h: &Head, sections: &[(u16, u16, u32, u64, Vec<u8>)]) -> Result<()> {
    // reader.rs: one CRC per 1 MiB block of the block-summed sections, in order.
    let mut crcs = Vec::new();
    for (_, flags, _, _, bytes) in sections {
        if flags & SECTION_BLOCKSUMMED != 0 {
            crcs.extend(bytes.chunks(1 << 20).map(crc32fast::hash));
        }
    }
    let mut owned = sections.to_vec();
    if !crcs.is_empty() {
        let n = crcs.len() as u64;
        owned.push((S_BLOCK_CRCS, SECTION_CRITICAL, 4, n, crcs.iter().flat_map(|c| c.to_le_bytes()).collect()));
    }
    let sections = &owned[..];
    let mut header = vec![0u8; HEADER_LEN];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&h.format.to_le_bytes());
    header[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    header[16..20].copy_from_slice(&h.flags.to_le_bytes());
    header[20..22].copy_from_slice(&(h.dims as u16).to_le_bytes());
    header[22..24].copy_from_slice(&h.codec_id.to_le_bytes());
    header[24..28].copy_from_slice(&(1u32 << 20).to_le_bytes());
    header[28..32].copy_from_slice(&(sections.len() as u32).to_le_bytes());
    header[112..120].copy_from_slice(&h.slots.to_le_bytes());
    header[120..128].copy_from_slice(&h.extras.to_le_bytes());
    header[128..136].copy_from_slice(&h.foreign.to_le_bytes());
    header[136..144].copy_from_slice(&h.tombstones.to_le_bytes());
    header[144..148].copy_from_slice(&h.books.to_le_bytes());
    header[148..152].copy_from_slice(&h.from.to_le_bytes());
    header[152..156].copy_from_slice(&h.to.to_le_bytes());
    header[RELEASE_TAG_OFFSET..RELEASE_TAG_OFFSET + h.tag.len()].copy_from_slice(h.tag.as_bytes());
    let mut body = Vec::new();
    let mut off = HEADER_LEN as u64;
    for (i, (kind, flags, elem, count, bytes)) in sections.iter().enumerate() {
        let pad = (off.div_ceil(64) * 64 - off) as usize;
        body.extend(std::iter::repeat_n(0u8, pad));
        off += pad as u64;
        let at = DIRECTORY_OFFSET + i * DIRECTORY_ENTRY_LEN;
        header[at..at + 2].copy_from_slice(&kind.to_le_bytes());
        header[at + 2..at + 4].copy_from_slice(&flags.to_le_bytes());
        header[at + 4..at + 8].copy_from_slice(&elem.to_le_bytes());
        header[at + 8..at + 16].copy_from_slice(&off.to_le_bytes());
        header[at + 16..at + 24].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        header[at + 24..at + 32].copy_from_slice(&count.to_le_bytes());
        let crc = if flags & SECTION_BLOCKSUMMED != 0 { 0 } else { crc32fast::hash(bytes) };
        header[at + 32..at + 36].copy_from_slice(&crc.to_le_bytes());
        body.extend_from_slice(bytes);
        off += bytes.len() as u64;
    }
    let crc = crc32fast::hash(&header[..HEADER_CRC_OFFSET]);
    header[HEADER_CRC_OFFSET..HEADER_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
    let mut all = header;
    all.extend_from_slice(&body);
    std::fs::write(path, all)?;
    Ok(())
}
