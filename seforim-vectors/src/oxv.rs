//! Reader for Otzaria's prebuilt vector files (`.oxv`, segment formats 2 and
//! 3 of `otzaria-semantic-search`, `src/semantic/oxv/format.rs`), for the
//! import into this store (issue #18). Format only: no model, no database.
//!
//! A segment holds, per slot, one encoded vector, the key of the text it was
//! embedded from and a hint: the 0-based ordinal of the line in its book
//! (Otzaria's index counts a book's lines in seforim.db `lineIndex, id`
//! order). Records say which book and line hold a slot's vector:
//! - primary: the book whose slot range holds the slot, at the slot's hint;
//! - extra: a further occurrence of the slot's key `{slot, hint}`;
//! - occurrences (format 3 windows): a complete snapshot for a key, replacing
//!   its primary and extra records; a row's window is the chunk of a long
//!   line, and an alias row is a short line borrowing a neighbour's vector.
//!
//! Book names are Otzaria's index `filePath` (`id:<bookId>` for a library
//! book). The HOST maps each name to its book id and the ordinal -> line id
//! list ([`OxvBookMap`]); records it cannot map are counted, never guessed.
//! Only base / compacted segments are read (a delta needs its base).
//!
//! The header's CRC and the CRCs of the small sections are checked; the
//! block-summed sections (keys, hints, vectors) are mapped unchecked, as
//! Otzaria does at open.

use std::collections::HashMap;
use std::fs::File;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};

use crate::{Error, Result, VectorRecord, VectorUpdate, CHUNK_BORROWED};

const MAGIC: &[u8; 8] = b"OXVSEG\r\n";
const HEADER_LEN: usize = 4096;
const HEADER_CRC_OFFSET: usize = 4088;
const DIRECTORY_OFFSET: usize = 224;
const DIRECTORY_ENTRY_LEN: usize = 40;
const MAX_SECTIONS: usize = 32;
const RELEASE_TAG_OFFSET: usize = 160;
const RELEASE_TAG_LEN: usize = 64;
const BOOK_ENTRY_LEN: usize = 40;
const HINT_MASK: u32 = !(1 << 31);
const SECTION_CRITICAL: u16 = 1;
const SECTION_BLOCKSUMMED: u16 = 2;
const FLAG_BASE: u32 = 1;
const FLAG_DELTA: u32 = 2;
const FLAG_COMPACTED: u32 = 4;
const ALIAS_WINDOW: u8 = u8::MAX;
const ROLE_ALIAS: u8 = 1;

// section kinds (Otzaria's numbering)
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
    S_BOOKS, S_BOOK_NAMES, S_CODEC_PARAMS, S_BOOK_HASHES, S_SLOT_KEYS, S_SLOT_HINTS, S_VECTORS, S_VECTOR_SCALES,
    S_SLOT_WINDOWS, S_EXTRAS, S_EXTRAS_BY_SLOT, S_FOREIGN, S_TOMBSTONES, S_OCCURRENCES, S_TIER0_SIGN, S_TIER0_PQ,
    S_BLOCK_CRCS,
];

/// Codec ids (Otzaria's `oxv/codec.rs`).
pub const CODEC_F32: u16 = 0x0001;
pub const CODEC_I8_SYM_DIM: u16 = 0x0101;
pub const CODEC_I8_SYM_VEC: u16 = 0x0102;
pub const CODEC_ROT_I8I4: u16 = 0x0103;
const PARAMS_VERSION: u32 = 1;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
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
enum Codec {
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

    fn bytes_per_vector(&self, dim: usize) -> usize {
        match self {
            Codec::F32 => 4 * dim,
            Codec::I8SymVec | Codec::I8SymDim { .. } => dim,
            Codec::Rot { head, tail, .. } => head + tail / 2,
        }
    }

    fn has_scales(&self) -> bool {
        matches!(self, Codec::I8SymVec | Codec::Rot { .. })
    }

    fn name(&self) -> &'static str {
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

#[derive(Debug, Clone)]
struct Book {
    name: String,
    slots: Range<u32>,
    extras: Range<u32>,
}

/// What a `.oxv` holds (JSON at the C ABI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OxvInfo {
    /// Segment format: 2 (16-byte keys) or 3 (8-byte keys).
    pub format: u32,
    /// "base" or "compacted" (a "delta" is refused at open).
    pub kind: String,
    pub dims: usize,
    pub codec: String,
    pub library_release_tag: String,
    pub from_library_version: u32,
    pub to_library_version: u32,
    pub slots: u64,
    pub extras: u64,
    pub occurrences: u64,
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

/// The host's mapping of one book of the file (JSON at the C ABI).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OxvBookMap {
    /// The book's name in the file.
    pub name: String,
    /// The host's book id.
    pub book_id: u64,
    /// The host's line id of each line ordinal (0-based), as the vectors
    /// count the book's lines.
    pub line_ids: Vec<u64>,
}

/// What an import did (JSON at the C ABI).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OxvImportReport {
    /// Mapped books found in the file and imported.
    pub books_imported: u64,
    /// Mapped names the file does not hold.
    pub books_not_in_file: Vec<String>,
    /// Books of the file with no mapping in this call.
    pub books_unmapped: u64,
    /// Records read from the imported books.
    pub records: u64,
    /// Vectors added (records of mapped lines).
    pub vectors: u64,
    /// Distinct lines given vectors.
    pub lines: u64,
    /// Records whose ordinal is past the host's line list (counted, dropped).
    pub out_of_range: u64,
    /// Records naming a (line, chunk) already given (counted, dropped).
    pub duplicates: u64,
    /// Store generation written (0: nothing written).
    pub generation: u64,
}

/// One record: a line ordinal of a book holds a slot's vector as `chunk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OxvRecord {
    pub slot: u32,
    pub ordinal: u32,
    /// 0 first window, n the n-th window, [`CHUNK_BORROWED`] an alias.
    pub chunk: u32,
}

/// An open `.oxv` file.
pub struct OxvFile {
    map: Mmap,
    format: u32,
    kind: &'static str,
    dims: usize,
    codec: Codec,
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
    occurrences: Option<Range<usize>>,
}

impl OxvFile {
    /// Maps and checks a `.oxv` (plain; expand a `.oxv.zst` with [`expand_zst`] first).
    pub fn open(path: &Path) -> Result<OxvFile> {
        let file = File::open(path)?;
        // SAFETY: read only; the host must not rewrite the file while it is open.
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
            FLAG_DELTA => return Err(bad("a delta segment: import the base of the same release")),
            _ => return Err(bad("no single segment kind")),
        };
        let dims = u16_at(b, 20) as usize;
        let codec_id = u16_at(b, 22);
        let sections = u32_at(b, 28) as usize;
        if sections > MAX_SECTIONS || dims == 0 {
            return Err(bad("header fields"));
        }
        // a record whose vector an older segment holds cannot be read from this file alone
        if u64_at(b, 128) != 0 {
            return Err(bad("the segment has foreign records (vectors held by an older segment): import a base"));
        }
        let slots64 = u64_at(b, 112);
        let extra_count = u64_at(b, 120);
        let book_count = u32_at(b, 144) as usize;
        let tag_bytes = &b[RELEASE_TAG_OFFSET..RELEASE_TAG_OFFSET + RELEASE_TAG_LEN];
        let tag_len = tag_bytes.iter().position(|&c| c == 0).unwrap_or(RELEASE_TAG_LEN);
        let tag = String::from_utf8(tag_bytes[..tag_len].to_vec()).map_err(|_| bad("release tag"))?;
        let (from, to) = (u32_at(b, 148), u32_at(b, 152));
        if slots64 > u32::MAX as u64 || extra_count > u32::MAX as u64 {
            return Err(bad("counts"));
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
        let params_r = section(S_CODEC_PARAMS, true)?.unwrap();
        let keys = section(S_SLOT_KEYS, true)?.unwrap();
        let hints = section(S_SLOT_HINTS, true)?.unwrap();
        let vectors = section(S_VECTORS, true)?.unwrap();
        let scales = section(S_VECTOR_SCALES, false)?;
        let windows = section(S_SLOT_WINDOWS, false)?;
        let extras = section(S_EXTRAS, false)?.unwrap_or(0..0);
        let occurrences = section(S_OCCURRENCES, false)?;
        let codec = Codec::parse(codec_id, dims, &b[params_r])?;
        need(&books_r, book_count as u64, BOOK_ENTRY_LEN as u64, "BOOKS")?;
        need(&keys, slots64, key_len as u64, "SLOT_KEYS")?;
        need(&hints, slots64, 4, "SLOT_HINTS")?;
        need(&vectors, slots64, codec.bytes_per_vector(dims) as u64, "VECTORS")?;
        need(&extras, extra_count, 8, "EXTRAS")?;
        match (&scales, codec.has_scales()) {
            (Some(r), true) => need(r, slots64, 4, "VECTOR_SCALES")?,
            (None, true) => return Err(bad("VECTOR_SCALES missing for a scaled codec")),
            _ => {}
        }
        if let Some(r) = &windows {
            need(r, slots64, 1, "SLOT_WINDOWS")?;
        }
        if let Some(r) = &occurrences {
            if r.len() % (2 * key_len + 12) != 0 {
                return Err(bad("OCCURRENCES section size"));
            }
        }
        let names = &b[names_r];
        let mut books = Vec::with_capacity(book_count);
        let (mut next_slot, mut next_extra) = (0u32, 0u32);
        for i in 0..book_count {
            let e = &b[books_r.start + i * BOOK_ENTRY_LEN..books_r.start + (i + 1) * BOOK_ENTRY_LEN];
            let (noff, nlen) = (u32_at(e, 0) as usize, u16_at(e, 4) as usize);
            let name = names
                .get(noff..noff + nlen)
                .and_then(|n| std::str::from_utf8(n).ok())
                .ok_or_else(|| bad("book name"))?
                .to_string();
            let (ss, sc, es, ec) = (u32_at(e, 8), u32_at(e, 12), u32_at(e, 16), u32_at(e, 20));
            // the ranges tile their sections, in book order
            if ss != next_slot || es != next_extra {
                return Err(bad("book ranges do not tile the slots / extras"));
            }
            next_slot = ss.checked_add(sc).ok_or_else(|| bad("slot range"))?;
            next_extra = es.checked_add(ec).ok_or_else(|| bad("extra range"))?;
            if let Some(prev) = books.last() {
                let prev: &Book = prev;
                if prev.name.as_bytes() >= name.as_bytes() {
                    return Err(bad("books are not sorted by name"));
                }
            }
            books.push(Book { name, slots: ss..next_slot, extras: es..next_extra });
        }
        if next_slot != slots || next_extra as u64 != extra_count {
            return Err(bad("book ranges do not cover the slots / extras"));
        }
        Ok(OxvFile {
            map,
            format,
            kind,
            dims,
            codec,
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
            occurrences,
        })
    }

    pub fn dims(&self) -> usize {
        self.dims
    }

    fn hint(&self, slot: u32) -> u32 {
        u32_at(&self.map, self.hints.start + slot as usize * 4) & HINT_MASK
    }

    fn window(&self, slot: u32) -> u8 {
        self.windows.as_ref().map_or(0, |w| self.map[w.start + slot as usize])
    }

    fn key(&self, slot: u32) -> &[u8] {
        let at = self.keys.start + slot as usize * self.key_len;
        &self.map[at..at + self.key_len]
    }

    fn extra(&self, i: u32) -> (u32, u32) {
        let at = self.extras.start + i as usize * 8;
        (u32_at(&self.map, at), u32_at(&self.map, at + 4))
    }

    fn occurrence_rows(&self) -> &[u8] {
        self.occurrences.as_ref().map_or(&[][..], |r| &self.map[r.clone()])
    }

    /// The occurrence snapshot rows of `key` (sorted by key), as
    /// (book, hint, chunk).
    fn key_occurrences(&self, key: &[u8]) -> Option<Vec<(u32, u32, u32)>> {
        let rows = self.occurrence_rows();
        let len = 2 * self.key_len + 12;
        let n = rows.len() / len;
        if n == 0 {
            return None;
        }
        let row = |i: usize| &rows[i * len..(i + 1) * len];
        let start = partition(n, |i| &row(i)[..self.key_len] < key);
        let end = partition(n, |i| &row(i)[..self.key_len] <= key);
        if start == end {
            return None;
        }
        Some(
            (start..end)
                .map(|i| {
                    let r = row(i);
                    let k = self.key_len;
                    let window = if r[2 * k + 9] == ROLE_ALIAS { ALIAS_WINDOW } else { r[2 * k + 8] };
                    (u32_at(r, k), u32_at(r, k + 4) & HINT_MASK, chunk_of_window(window))
                })
                .collect(),
        )
    }

    /// Every record of book `b`: primary and extra records, a key with an
    /// occurrence snapshot taking its snapshot rows for this book instead.
    pub fn records_of(&self, b: usize) -> Vec<OxvRecord> {
        let book = &self.books[b];
        let mut out = Vec::new();
        let mut snapshot_done: HashMap<&[u8], ()> = HashMap::new();
        let has_occ = !self.occurrence_rows().is_empty();
        let mut push_slot = |slot: u32, hint: u32, out: &mut Vec<OxvRecord>| {
            if has_occ {
                let key = self.key(slot);
                if let Some(rows) = self.key_occurrences(key) {
                    if snapshot_done.insert(key, ()).is_none() {
                        for (rb, rh, chunk) in rows {
                            if rb as usize == b {
                                out.push(OxvRecord { slot, ordinal: rh, chunk });
                            }
                        }
                    }
                    return;
                }
            }
            out.push(OxvRecord { slot, ordinal: hint, chunk: chunk_of_window(self.window(slot)) });
        };
        for slot in book.slots.clone() {
            push_slot(slot, self.hint(slot), &mut out);
        }
        for i in book.extras.clone() {
            let (slot, hint) = self.extra(i);
            if slot < self.slots {
                push_slot(slot, hint & HINT_MASK, &mut out);
            }
        }
        out.sort_unstable_by_key(|r| (r.ordinal, r.chunk, r.slot));
        out
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
                    lines: recs.iter().map(|r| r.ordinal as u64 + 1).max().unwrap_or(0),
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
            extras: self.books.last().map_or(0, |b| b.extras.end as u64),
            occurrences: (self.occurrence_rows().len() / (2 * self.key_len + 12)) as u64,
            books,
        }
    }

    /// The book index of `name`.
    fn book_index(&self, name: &str) -> Option<usize> {
        self.books.binary_search_by(|b| b.name.as_bytes().cmp(name.as_bytes())).ok()
    }

    /// The store update importing the mapped books: each mapped book's old
    /// vectors removed, every chunk of every mapped line added.
    pub fn update_for(&self, maps: &[OxvBookMap], cancel: Option<&AtomicBool>) -> Result<(VectorUpdate, OxvImportReport)> {
        let mut report = OxvImportReport::default();
        let mut update = VectorUpdate::default();
        let mut seen_books = std::collections::HashSet::new();
        let mut lines = std::collections::HashSet::new();
        for m in maps {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(Error::Cancelled);
            }
            let Some(b) = self.book_index(&m.name) else {
                report.books_not_in_file.push(m.name.clone());
                continue;
            };
            if !seen_books.insert(b) {
                return Err(Error::Invalid(format!("book {} mapped twice", m.name)));
            }
            report.books_imported += 1;
            update.remove_books.push(m.book_id);
            let mut given = std::collections::HashSet::new();
            for r in self.records_of(b) {
                report.records += 1;
                let Some(&line) = m.line_ids.get(r.ordinal as usize) else {
                    report.out_of_range += 1;
                    continue;
                };
                if !given.insert((line, r.chunk)) {
                    report.duplicates += 1;
                    continue;
                }
                lines.insert(line);
                update.add.push(VectorRecord { line_id: line, book_id: m.book_id, chunk: r.chunk, vector: self.vector(r.slot) });
            }
        }
        report.books_unmapped = (self.books.len() - seen_books.len()) as u64;
        report.vectors = update.add.len() as u64;
        report.lines = lines.len() as u64;
        Ok((update, report))
    }
}

fn chunk_of_window(window: u8) -> u32 {
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

/// Writes a small segment in Otzaria's layout, for tests (here and in the
/// engine, the C ABI and the hosts' self-tests): one base segment, codec
/// f32 (`rot` false) or rot192-i8i4 with the identity rotation (`rot` true,
/// head = dims - tail, tail = 4). `books`: (name, records as (ordinal,
/// chunk, vector)); each distinct vector is one slot; a record of window >
/// 0 or an alias makes the file format 3 with occurrence snapshots.
#[doc(hidden)]
pub fn write_test_segment(path: &Path, dims: usize, rot: bool, tag: &str, books: &[(&str, Vec<(u32, u32, Vec<f32>)>)]) -> Result<()> {
    let format: u32 = if books.iter().any(|(_, r)| r.iter().any(|x| x.1 != 0)) { 3 } else { 2 };
    let key_len = if format == 3 { 8 } else { 16 };
    let mut sorted: Vec<&(&str, Vec<(u32, u32, Vec<f32>)>)> = books.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    // slots: one per record (keys distinct), primary in its book
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
        book_rows.push((names.len() as u32, name.len() as u16, start, n));
        names.extend_from_slice(name.as_bytes());
    }
    let slots = hints.len();
    let tail = if rot { 4usize } else { 0 };
    let head = dims - tail;
    let mut params = Vec::new();
    params.extend_from_slice(&PARAMS_VERSION.to_le_bytes());
    params.extend_from_slice(&(dims as u32).to_le_bytes());
    let codec_id;
    let mut vbytes = Vec::new();
    let mut scales = Vec::new();
    if rot {
        codec_id = CODEC_ROT_I8I4;
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
        for v in &vecs {
            let max = v[..head].iter().fold(0f32, |m, x| m.max(x.abs()));
            let s = max / 127.0;
            scales.push(s);
            for x in &v[..head] {
                vbytes.push(if s > 0.0 { (x / s).round().clamp(-127.0, 127.0) as i8 as u8 } else { 0 });
            }
            for j in 0..tail / 2 {
                let code = |x: f32| ((x + 1.0) / 2.0 * 15.0).round().clamp(0.0, 15.0) as u8;
                vbytes.push(code(v[head + 2 * j]) | (code(v[head + 2 * j + 1]) << 4));
            }
        }
    } else {
        codec_id = CODEC_F32;
        for v in &vecs {
            for x in v {
                vbytes.extend_from_slice(&x.to_le_bytes());
            }
        }
    }
    occ.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    let mut occ_bytes = Vec::new();
    for (key, book, hint, window, alias) in &occ {
        occ_bytes.extend_from_slice(key);
        occ_bytes.extend_from_slice(&book.to_le_bytes());
        occ_bytes.extend_from_slice(&hint.to_le_bytes());
        occ_bytes.extend_from_slice(key);
        occ_bytes.extend_from_slice(&if *alias { [0, ROLE_ALIAS, 0, 0] } else { [*window, 0, 0, 0] });
    }
    let mut books_bytes = Vec::new();
    for (noff, nlen, start, n) in &book_rows {
        books_bytes.extend_from_slice(&noff.to_le_bytes());
        books_bytes.extend_from_slice(&nlen.to_le_bytes());
        books_bytes.extend_from_slice(&0u16.to_le_bytes());
        books_bytes.extend_from_slice(&start.to_le_bytes());
        books_bytes.extend_from_slice(&n.to_le_bytes());
        books_bytes.extend_from_slice(&0u32.to_le_bytes());
        books_bytes.extend_from_slice(&0u32.to_le_bytes());
        books_bytes.extend_from_slice(&[0u8; 16]);
    }
    let hint_bytes: Vec<u8> = hints.iter().flat_map(|h| h.to_le_bytes()).collect();
    let scale_bytes: Vec<u8> = scales.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut sections: Vec<(u16, u16, u32, u64, Vec<u8>)> = vec![
        (S_BOOKS, SECTION_CRITICAL, BOOK_ENTRY_LEN as u32, book_rows.len() as u64, books_bytes),
        (S_BOOK_NAMES, SECTION_CRITICAL, 0, 0, names),
        (S_CODEC_PARAMS, SECTION_CRITICAL, 0, 0, params),
        (S_SLOT_KEYS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, key_len as u32, slots as u64, keys),
        (S_SLOT_HINTS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, 4, slots as u64, hint_bytes),
        (S_VECTORS, SECTION_CRITICAL | SECTION_BLOCKSUMMED, 0, slots as u64, vbytes),
        (S_SLOT_WINDOWS, 0, 1, slots as u64, windows),
    ];
    if rot {
        sections.push((S_VECTOR_SCALES, SECTION_CRITICAL, 4, slots as u64, scale_bytes));
    }
    if format == 3 {
        sections.push((S_OCCURRENCES, SECTION_CRITICAL | SECTION_BLOCKSUMMED, (2 * key_len + 12) as u32, occ.len() as u64, occ_bytes));
    }
    let mut header = vec![0u8; HEADER_LEN];
    header[..8].copy_from_slice(MAGIC);
    header[8..12].copy_from_slice(&format.to_le_bytes());
    header[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
    header[16..20].copy_from_slice(&FLAG_BASE.to_le_bytes());
    header[20..22].copy_from_slice(&(dims as u16).to_le_bytes());
    header[22..24].copy_from_slice(&codec_id.to_le_bytes());
    header[24..28].copy_from_slice(&(1u32 << 20).to_le_bytes());
    header[28..32].copy_from_slice(&(sections.len() as u32).to_le_bytes());
    header[112..120].copy_from_slice(&(slots as u64).to_le_bytes());
    header[144..148].copy_from_slice(&(book_rows.len() as u32).to_le_bytes());
    header[152..156].copy_from_slice(&1u32.to_le_bytes());
    header[RELEASE_TAG_OFFSET..RELEASE_TAG_OFFSET + tag.len()].copy_from_slice(tag.as_bytes());
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
