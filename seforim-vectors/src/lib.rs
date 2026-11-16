//! The vector sidecar of a line index, for semantic search (issue #18).
//!
//! The host supplies the vectors of each line (`line_id`, `book_id`, chunk,
//! `f32[dims]`): one per line, or several when its model embeds a long line in
//! chunks (Otzaria's windows); the engine never runs a model. A line scores as
//! its best chunk, a chunk past the first (or a vector borrowed from a
//! neighbour) losing `chunk_penalty` first (Otzaria's window penalty, 0.04);
//! hits are lines. A store is a directory (the engine puts it
//! at `<index>/vectors/`) holding generations; each generation is two files:
//!
//! - `vectors-<gen>.usearch`: a usearch HNSW graph (cosine, vectors quantized
//!   to i8), keyed by line id (u64, so the 64-bit virtual note ids fit; a
//!   multi-key index: a line's chunks share its key), memory-mapped as a
//!   read-only view by searches.
//! - `vectors-<gen>.sev`: our file, written last (its presence publishes the
//!   generation): a versioned header, the per-book row ranges (rows sorted by
//!   book, line, chunk), the line id and chunk of each row, a line -> row index, and the
//!   full-precision vectors as **f16**. f16, not f32: the rows are only read
//!   to rescore candidates and for the exact filtered scan, where an f16
//!   cosine is within ~1e-3 of the f32 one (far below the i8 graph's error,
//!   which the rescore exists to remove) at half the disk and page cache
//!   (e.g. 5 M lines x 768 dims: 7.7 GB instead of 15.4 GB).
//!
//! What usearch lacks and this crate adds (Lucene's design): an EXACT scan
//! when a book filter leaves few lines, and a RESCORE of an oversampled
//! candidate list with the full vectors. Vectors are L2-normalized on input,
//! so a score is the cosine similarity (-1..1).
//!
//! Writes ([`apply`]) build a new generation (temp files + rename, Windows
//! retry) and then delete older generations (best effort: a file a reader
//! still maps is retried at the next write).

pub mod chunker;
pub mod f16;
pub mod fusion;
pub mod oxv;

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use memmap2::Mmap;

const MAGIC: &[u8; 8] = b"SEVECTS\0";
/// Bump whenever the `.sev` layout changes.
/// 2: several vectors (chunks) per line.
pub const FORMAT_VERSION: u32 = 2;
const HEADER_LEN: usize = 256;
const ALIGN: u64 = 64;

/// Errors of the vector store.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// usearch reported an error.
    Index(String),
    /// A bad request (wrong dimensions, zero vector, duplicate line id...).
    Invalid(String),
    /// A store file is damaged or of another format version.
    Format(String),
    /// Stopped through the cancel flag.
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "vector store I/O: {e}"),
            Error::Index(e) => write!(f, "vector index: {e}"),
            Error::Invalid(e) => write!(f, "vector request: {e}"),
            Error::Format(e) => write!(f, "vector store file: {e}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<cxx::Exception> for Error {
    fn from(e: cxx::Exception) -> Self {
        Error::Index(e.what().to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// [`VectorRecord::chunk`] bit: the vector is a neighbour's, borrowed by a
/// short line (Otzaria's alias); penalized like a later chunk.
pub const CHUNK_BORROWED: u32 = 1 << 31;

/// Otzaria's window penalty (`config/profiles.rs` DEFAULT_WINDOW_PENALTY,
/// also its alias penalty): taken off a chunk's cosine unless the chunk is
/// the line's own first one.
pub const DEFAULT_CHUNK_PENALTY: f32 = 0.04;

/// One vector of a line, as the host supplies it.
#[derive(Debug, Clone)]
pub struct VectorRecord {
    pub line_id: u64,
    pub book_id: u64,
    /// 0: the line's first (or only) vector; 1, 2, ...: its further chunks
    /// (Otzaria's windows); [`CHUNK_BORROWED`] may be set on top. Unique per
    /// line; every chunk of a line is in one book.
    pub chunk: u32,
    pub vector: Vec<f32>,
}

/// A chunk's penalty under `penalty`: none for a line's own first vector.
fn chunk_penalty(chunk: u32, penalty: f32) -> f32 {
    if chunk == 0 {
        0.0
    } else {
        penalty
    }
}

/// One write: removals first, then additions. A line id added again replaces
/// every old vector of it (and its old book). Follows the per-book index updates
/// (issue #29): an updated book = `remove_books: [b]` + its new lines.
#[derive(Debug, Clone, Default)]
pub struct VectorUpdate {
    pub remove_books: Vec<u64>,
    pub remove_lines: Vec<u64>,
    pub add: Vec<VectorRecord>,
}

/// HNSW build parameters (usearch's names). Kept by the store: an update
/// reuses the parameters the store was built with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildParams {
    pub dims: usize,
    /// M: edges per node (usearch default 16).
    pub connectivity: usize,
    /// efConstruction (usearch default 128).
    pub expansion_add: usize,
    /// efSearch (usearch's default is 64; 128 here: on the synthetic
    /// recall test 64 gave recall@10 0.90, 128 gives >= 0.95). usearch widens
    /// it to the candidate count when that is larger.
    pub expansion_search: usize,
    /// Worker threads for adding to the graph (0 = one per core).
    pub threads: usize,
}

impl BuildParams {
    pub fn new(dims: usize) -> BuildParams {
        BuildParams { dims, connectivity: 16, expansion_add: 128, expansion_search: 128, threads: 0 }
    }
}

/// Thread hooks for the build's worker threads (the engine passes its
/// background-priority enter / leave, issue #19).
#[derive(Clone, Copy, Default)]
pub struct ThreadHooks {
    pub start: Option<fn()>,
    pub end: Option<fn()>,
}

/// A semantic search request.
#[derive(Debug, Clone)]
pub struct SearchParams {
    /// How many results.
    pub k: usize,
    /// Only lines of these books (the lexical search's `books` filter).
    /// None = every book.
    pub books: Option<Vec<u64>>,
    /// The graph is asked for `ceil(k * oversample)` candidates, which are
    /// rescored with the full vectors (>= 1; default 3).
    pub oversample: f32,
    /// A filter leaving at most this many lines is served by an exact scan
    /// of just those lines (Lucene's rule: an exact scan is cheaper than a
    /// filtered graph walk that visits mostly rejected nodes). Default 20,000
    /// (~10 ms at 768 dims on one core). 0 = never exact unless the graph
    /// came back short.
    pub exact_threshold: usize,
    /// Taken off the cosine of a chunk past a line's first one (and of a
    /// borrowed vector) before the line's best chunk is chosen: Otzaria's
    /// window penalty. Default [`DEFAULT_CHUNK_PENALTY`]; 0 = cosine alone.
    pub chunk_penalty: f32,
}

impl SearchParams {
    pub fn new(k: usize) -> SearchParams {
        SearchParams { k, books: None, oversample: 3.0, exact_threshold: 20_000, chunk_penalty: DEFAULT_CHUNK_PENALTY }
    }
}

/// One semantic result, a line: the score of its best chunk (cosine minus
/// that chunk's penalty), best first, ties by line id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorHit {
    pub line_id: u64,
    pub book_id: u64,
    pub score: f32,
    /// The chunk that scored it.
    pub chunk: u32,
}

/// How a search was served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchPath {
    /// True: exact scan over the filtered lines; false: HNSW + rescore.
    pub exact: bool,
    /// Vectors (chunks) the filter allows.
    pub allowed: u64,
}

// ---------------------------------------------------------------------------
// file layout

#[derive(Debug, Clone, Copy)]
struct Header {
    dims: u64,
    /// Rows (vectors).
    count: u64,
    books: u64,
    connectivity: u64,
    expansion_add: u64,
    expansion_search: u64,
    off_books: u64,
    off_lines: u64,
    off_by_line: u64,
    off_vectors: u64,
    graph_len: u64,
    file_len: u64,
    generation: u64,
    /// Graph nodes removed since the graph was last built afresh (all
    /// updates together decide the rebuild, not each one alone).
    removed_since_rebuild: u64,
    off_chunks: u64,
    /// Distinct lines.
    lines: u64,
}

impl Header {
    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..8].copy_from_slice(MAGIC);
        out[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        let fields = [
            self.dims,
            self.count,
            self.books,
            self.connectivity,
            self.expansion_add,
            self.expansion_search,
            self.off_books,
            self.off_lines,
            self.off_by_line,
            self.off_vectors,
            self.graph_len,
            self.file_len,
            self.generation,
            self.removed_since_rebuild,
            self.off_chunks,
            self.lines,
        ];
        for (i, v) in fields.iter().enumerate() {
            out[16 + i * 8..24 + i * 8].copy_from_slice(&v.to_le_bytes());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Header> {
        if bytes.len() < HEADER_LEN || &bytes[..8] != MAGIC {
            return Err(Error::Format("not a vector store file".into()));
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(Error::Format(format!("format version {version}, this library reads {FORMAT_VERSION}")));
        }
        let f = |i: usize| u64::from_le_bytes(bytes[16 + i * 8..24 + i * 8].try_into().unwrap());
        let h = Header {
            dims: f(0),
            count: f(1),
            books: f(2),
            connectivity: f(3),
            expansion_add: f(4),
            expansion_search: f(5),
            off_books: f(6),
            off_lines: f(7),
            off_by_line: f(8),
            off_vectors: f(9),
            graph_len: f(10),
            file_len: f(11),
            generation: f(12),
            removed_since_rebuild: f(13),
            off_chunks: f(14),
            lines: f(15),
        };
        // checked: a damaged file must not overflow into a passing check
        let end = |off: u64, n: u64, size: u64| n.checked_mul(size).and_then(|b| off.checked_add(b));
        let fits = |off: u64, n: u64, size: u64, limit: u64| end(off, n, size).is_some_and(|e| e <= limit);
        let ok = h.file_len == bytes.len() as u64
            && h.dims > 0
            && h.count <= u32::MAX as u64
            && h.off_books >= HEADER_LEN as u64
            && fits(h.off_books, h.books, 24, h.off_lines)
            && fits(h.off_lines, h.count, 8, h.off_by_line)
            && fits(h.off_by_line, h.count, 4, h.off_chunks)
            && fits(h.off_chunks, h.count, 4, h.off_vectors)
            && h.lines <= h.count
            && h.count.checked_mul(h.dims).is_some_and(|n| fits(h.off_vectors, n, 2, h.file_len))
            && h.off_vectors % 2 == 0;
        if !ok {
            return Err(Error::Format("damaged header".into()));
        }
        Ok(h)
    }
}

fn align(x: u64) -> u64 {
    x.div_ceil(ALIGN) * ALIGN
}

fn sev_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("vectors-{generation:010}.sev"))
}

fn graph_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("vectors-{generation:010}.usearch"))
}

/// The generations present in `dir` with a published `.sev`, ascending.
fn generations(dir: &Path) -> Vec<u64> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(gen) = name.strip_prefix("vectors-").and_then(|n| n.strip_suffix(".sev")) {
                if let Ok(g) = gen.parse::<u64>() {
                    out.push(g);
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// The newest published generation in `dir` (None: no store).
pub fn latest_generation(dir: &Path) -> Option<u64> {
    generations(dir).last().copied()
}

/// True when `dir` holds a published store.
pub fn exists(dir: &Path) -> bool {
    !generations(dir).is_empty()
}

fn index_options(params: &BuildParams) -> usearch::IndexOptions {
    usearch::IndexOptions {
        dimensions: params.dims,
        metric: usearch::MetricKind::Cos,
        quantization: usearch::ScalarKind::I8,
        connectivity: params.connectivity,
        expansion_add: params.expansion_add,
        expansion_search: params.expansion_search,
        multi: true,
    }
}

// ---------------------------------------------------------------------------
// reading

/// One published generation, memory-mapped. Searches share it (`Sync`).
pub struct VectorStore {
    // declared first: dropped before the maps it views
    graph: Option<usearch::Index>,
    map: Mmap,
    header: Header,
    path: PathBuf,
}

// SAFETY: usearch's index is safe for concurrent searches (its own thread
// slots); the maps are read-only.
unsafe impl Send for VectorStore {}
unsafe impl Sync for VectorStore {}

impl VectorStore {
    /// Opens the newest generation in `dir`. None: no store yet. A writer
    /// that publishes a newer generation and deletes this one meanwhile
    /// (another handle or process) makes it retry on the newer one.
    pub fn open(dir: &Path) -> Result<Option<VectorStore>> {
        let mut attempt = 0;
        loop {
            let Some(&generation) = generations(dir).last() else { return Ok(None) };
            match Self::open_generation(dir, generation) {
                Err(e) if attempt < 3 && latest_generation(dir) != Some(generation) => {
                    attempt += 1;
                    let _ = e;
                }
                other => return other.map(Some),
            }
        }
    }

    fn open_generation(dir: &Path, generation: u64) -> Result<VectorStore> {
        let path = sev_path(dir, generation);
        let file = File::open(&path)?;
        // SAFETY: a published generation file is never written again; it is
        // only deleted (after a newer one exists), which a live map survives
        // on every OS (Windows refuses the delete instead).
        let map = unsafe { Mmap::map(&file)? };
        let header = Header::decode(&map)?;
        let graph = if header.count > 0 {
            let gpath = graph_path(dir, generation);
            let len = std::fs::metadata(&gpath)?.len();
            if len != header.graph_len {
                return Err(Error::Format("the graph file does not belong to this generation".into()));
            }
            let index = usearch::Index::restore_view(gpath.to_str().ok_or_else(|| Error::Invalid("non UTF-8 path".into()))?)?;
            if index.dimensions() != header.dims as usize {
                return Err(Error::Format("the graph has other dimensions".into()));
            }
            index.change_expansion_search(header.expansion_search as usize);
            Some(index)
        } else {
            None
        };
        Ok(VectorStore { graph, map, header, path })
    }

    pub fn dims(&self) -> usize {
        self.header.dims as usize
    }

    /// Number of vectors (chunks).
    pub fn len(&self) -> usize {
        self.header.count as usize
    }

    /// Number of distinct lines.
    pub fn line_count(&self) -> u64 {
        self.header.lines
    }

    pub fn is_empty(&self) -> bool {
        self.header.count == 0
    }

    pub fn generation(&self) -> u64 {
        self.header.generation
    }

    /// The `.sev` file of this generation.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The parameters the graph was built with.
    pub fn build_params(&self) -> BuildParams {
        BuildParams {
            dims: self.header.dims as usize,
            connectivity: self.header.connectivity as usize,
            expansion_add: self.header.expansion_add as usize,
            expansion_search: self.header.expansion_search as usize,
            threads: 0,
        }
    }

    fn u64_at(&self, off: u64, i: u64) -> u64 {
        let at = (off + i * 8) as usize;
        u64::from_le_bytes(self.map[at..at + 8].try_into().unwrap())
    }

    /// (book id, first row, row count) of book slot `i`.
    fn book(&self, i: u64) -> (u64, u64, u64) {
        let at = self.header.off_books + i * 24;
        (self.u64_at(at, 0), self.u64_at(at, 1), self.u64_at(at, 2))
    }

    fn line_of(&self, row: u64) -> u64 {
        self.u64_at(self.header.off_lines, row)
    }

    fn row_by_line_slot(&self, i: u64) -> u64 {
        let at = (self.header.off_by_line + i * 4) as usize;
        u32::from_le_bytes(self.map[at..at + 4].try_into().unwrap()) as u64
    }

    fn chunk_of(&self, row: u64) -> u32 {
        let at = (self.header.off_chunks + row * 4) as usize;
        u32::from_le_bytes(self.map[at..at + 4].try_into().unwrap())
    }

    /// The rows of `line_id` (contiguous: rows are sorted by book, line, chunk).
    fn rows_of(&self, line_id: u64) -> Option<std::ops::Range<u64>> {
        let (mut lo, mut hi) = (0u64, self.header.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let row = self.row_by_line_slot(mid);
            match self.line_of(row).cmp(&line_id) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => {
                    let (mut a, mut b) = (row, row + 1);
                    while a > 0 && self.line_of(a - 1) == line_id {
                        a -= 1;
                    }
                    while b < self.header.count && self.line_of(b) == line_id {
                        b += 1;
                    }
                    return Some(a..b);
                }
            }
        }
        None
    }

    /// The line's score: its best chunk's cosine minus that chunk's penalty
    /// (Otzaria's representative occurrence), and that chunk.
    fn line_score(&self, query: &[f32], rows: std::ops::Range<u64>, penalty: f32, t: &[f32]) -> (f32, u32) {
        let mut best = (f32::NEG_INFINITY, 0u32);
        for row in rows {
            let chunk = self.chunk_of(row);
            let score = f16::dot(query, self.row_vector(row), t) - chunk_penalty(chunk, penalty);
            if score > best.0 || (score == best.0 && chunk < best.1) {
                best = (score, chunk);
            }
        }
        best
    }

    /// The book slot holding `row`.
    fn book_slot_of_row(&self, row: u64) -> u64 {
        let (mut lo, mut hi) = (0u64, self.header.books);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if self.book(mid).1 <= row {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }

    fn book_slot(&self, book_id: u64) -> Option<u64> {
        let (mut lo, mut hi) = (0u64, self.header.books);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.book(mid).0.cmp(&book_id) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    /// The stored f16 vector of `row`.
    fn row_vector(&self, row: u64) -> &[u16] {
        let dims = self.header.dims as usize;
        let at = (self.header.off_vectors + row * self.header.dims * 2) as usize;
        let bytes = &self.map[at..at + dims * 2];
        // SAFETY: the vectors start 64-aligned in a page-aligned map; u16
        // has no invalid bit patterns.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u16, dims) }
    }

    /// The first vector stored for `line_id` (f16 widened to f32; normalized).
    pub fn vector(&self, line_id: u64) -> Option<Vec<f32>> {
        self.chunks(line_id).into_iter().next().map(|(_, v)| v)
    }

    /// Every (chunk, vector) stored for `line_id`, by chunk.
    pub fn chunks(&self, line_id: u64) -> Vec<(u32, Vec<f32>)> {
        let t = f16::table();
        self.rows_of(line_id)
            .into_iter()
            .flatten()
            .map(|row| (self.chunk_of(row), self.row_vector(row).iter().map(|&h| t[h as usize]).collect()))
            .collect()
    }

    /// Vector (chunk) count per book: (book id, vectors), ascending by book id.
    pub fn book_counts(&self) -> Vec<(u64, u64)> {
        (0..self.header.books).map(|i| self.book(i)).map(|(b, _, n)| (b, n)).collect()
    }

    /// Every (line id, book id) once, in row order (book, then line).
    pub fn lines(&self) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(self.header.lines as usize);
        for i in 0..self.header.books {
            let (book, start, n) = self.book(i);
            for row in start..start + n {
                let line = self.line_of(row);
                if out.last() != Some(&(line, book)) {
                    out.push((line, book));
                }
            }
        }
        out
    }

    /// The top `params.k` lines by cosine similarity to `query`.
    pub fn search(&self, query: &[f32], params: &SearchParams, cancel: Option<&AtomicBool>) -> Result<Vec<VectorHit>> {
        self.search_with_path(query, params, cancel).map(|(hits, _)| hits)
    }

    /// [`search`](Self::search) that also says how it was served.
    pub fn search_with_path(
        &self,
        query: &[f32],
        params: &SearchParams,
        cancel: Option<&AtomicBool>,
    ) -> Result<(Vec<VectorHit>, SearchPath)> {
        let cancelled = || cancel.is_some_and(|c| c.load(AtomicOrdering::Relaxed));
        if query.len() != self.dims() {
            return Err(Error::Invalid(format!("query has {} dims, the store {}", query.len(), self.dims())));
        }
        let query = normalized(query)?;
        if params.k == 0 || self.is_empty() {
            return Ok((Vec::new(), SearchPath::default()));
        }
        // the filter as row ranges, ascending (a book id twice counts once)
        let ranges: Option<Vec<(u64, u64)>> = params.books.as_ref().map(|books| {
            let mut slots: Vec<u64> = books.iter().filter_map(|&b| self.book_slot(b)).collect();
            slots.sort_unstable();
            slots.dedup();
            slots.iter().map(|&s| self.book(s)).map(|(_, start, n)| (start, start + n)).collect()
        });
        let allowed = match &ranges {
            Some(r) => r.iter().map(|(a, b)| b - a).sum(),
            None => self.header.count,
        };
        if allowed == 0 {
            return Ok((Vec::new(), SearchPath { exact: true, allowed }));
        }
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let all = [(0, self.header.count)];
        let penalty = params.chunk_penalty;
        let scan = |ranges: &[(u64, u64)]| self.exact(&query, ranges, params.k, penalty, cancel);
        if allowed as usize <= params.exact_threshold || self.graph.is_none() {
            let hits = scan(ranges.as_deref().unwrap_or(&all))?;
            return Ok((hits, SearchPath { exact: true, allowed }));
        }
        let graph = self.graph.as_ref().unwrap();
        let want = ((params.k as f64) * (params.oversample.max(1.0) as f64)).ceil() as usize;
        let want = want.max(params.k);
        // a line's chunks share its key: ask for enough vectors to bring `want` lines
        let per_line = (self.header.count as f64 / self.header.lines.max(1) as f64).max(1.0);
        let want = ((want as f64) * per_line).ceil() as usize;
        // efSearch is the store's (set at open; never changed under shared
        // searches); usearch widens it to `want` when `want` is larger
        let matches = match &ranges {
            None => graph.search(&query, want)?,
            Some(r) => graph.filtered_search(&query, want, |key| {
                self.rows_of(key).is_some_and(|rows| in_ranges(r, rows.start))
            })?,
        };
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let t = f16::table();
        let mut keys = matches.keys.clone();
        keys.sort_unstable();
        keys.dedup();
        // every candidate line rescored over ALL its chunks (the graph found it by one)
        let mut hits: Vec<VectorHit> = keys
            .iter()
            .filter_map(|&line| {
                let rows = self.rows_of(line)?;
                let (book, _, _) = self.book(self.book_slot_of_row(rows.start));
                let (score, chunk) = self.line_score(&query, rows, penalty, t);
                Some(VectorHit { line_id: line, book_id: book, score, chunk })
            })
            .collect();
        // the graph came back short of what the filter allows: exact (Lucene's fallback)
        if hits.len() < params.k.min(allowed as usize) {
            let hits = scan(ranges.as_deref().unwrap_or(&all))?;
            return Ok((hits, SearchPath { exact: true, allowed }));
        }
        hits.sort_by(best_first);
        hits.truncate(params.k);
        Ok((hits, SearchPath { exact: false, allowed }))
    }

    /// Exact top-k lines over the rows in `ranges` (whole books, so a line's
    /// chunks are all inside).
    fn exact(&self, query: &[f32], ranges: &[(u64, u64)], k: usize, penalty: f32, cancel: Option<&AtomicBool>) -> Result<Vec<VectorHit>> {
        let t = f16::table();
        let mut heap: BinaryHeap<Worst> = BinaryHeap::with_capacity(k + 1);
        let mut seen = 0u64;
        let push = |hit: VectorHit, heap: &mut BinaryHeap<Worst>| {
            if heap.len() < k {
                heap.push(Worst(hit));
            } else if best_first(&hit, &heap.peek().unwrap().0) == Ordering::Less {
                heap.pop();
                heap.push(Worst(hit));
            }
        };
        let mut current: Option<VectorHit> = None;
        for &(start, end) in ranges {
            let mut slot = self.book_slot_of_row(start);
            let (mut book, mut bstart, mut bn) = self.book(slot);
            for row in start..end {
                while row >= bstart + bn {
                    slot += 1;
                    (book, bstart, bn) = self.book(slot);
                }
                seen += 1;
                if seen % 4096 == 0 && cancel.is_some_and(|c| c.load(AtomicOrdering::Relaxed)) {
                    return Err(Error::Cancelled);
                }
                let chunk = self.chunk_of(row);
                let hit = VectorHit {
                    line_id: self.line_of(row),
                    book_id: book,
                    score: f16::dot(query, self.row_vector(row), t) - chunk_penalty(chunk, penalty),
                    chunk,
                };
                match &mut current {
                    Some(c) if c.line_id == hit.line_id => {
                        if hit.score > c.score {
                            *c = hit;
                        }
                    }
                    _ => {
                        if let Some(done) = current.replace(hit) {
                            push(done, &mut heap);
                        }
                    }
                }
            }
        }
        if let Some(done) = current {
            push(done, &mut heap);
        }
        let mut hits: Vec<VectorHit> = heap.into_iter().map(|w| w.0).collect();
        hits.sort_by(best_first);
        Ok(hits)
    }
}

fn in_ranges(ranges: &[(u64, u64)], row: u64) -> bool {
    let i = ranges.partition_point(|&(start, _)| start <= row);
    i > 0 && row < ranges[i - 1].1
}

/// Best first: higher score, then lower line id (deterministic ties).
pub fn best_first(a: &VectorHit, b: &VectorHit) -> Ordering {
    b.score.total_cmp(&a.score).then_with(|| a.line_id.cmp(&b.line_id))
}

/// Heap entry whose top is the WORST kept hit.
struct Worst(VectorHit);

impl PartialEq for Worst {
    fn eq(&self, other: &Self) -> bool {
        best_first(&self.0, &other.0) == Ordering::Equal
    }
}
impl Eq for Worst {}
impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        best_first(&self.0, &other.0)
    }
}

/// `v` scaled to unit length; a zero or non-finite vector is refused.
pub fn normalized(v: &[f32]) -> Result<Vec<f32>> {
    let norm = v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
    if !(norm.is_finite() && norm > 0.0) {
        return Err(Error::Invalid("a zero or non-finite vector".into()));
    }
    Ok(v.iter().map(|x| (*x as f64 / norm) as f32).collect())
}

// ---------------------------------------------------------------------------
// writing

/// Where a row of the new generation comes from.
#[derive(Clone, Copy)]
enum Source {
    Old(u64),
    New(usize),
}

/// What [`apply`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    pub generation: u64,
    /// Distinct lines in the new generation.
    pub lines: u64,
    /// Vectors (chunks) in it.
    pub vectors: u64,
    pub books: u64,
    pub removed: u64,
    pub added: u64,
    /// True: the graph was rebuilt from all vectors (first build, or too
    /// many removed nodes); false: the old graph was updated in place.
    pub rebuilt: bool,
}

/// Graph rebuild threshold: when this share of the old nodes was removed,
/// the graph is built afresh instead of carrying tombstones.
const REBUILD_SHARE: f64 = 0.25;

/// Applies `update` to the store in `dir` (created when missing, with
/// `params`; an existing store keeps its own parameters and `params.dims`
/// must match) and publishes it as a new generation. Older generations are
/// then removed (best effort). `cancel` stops it before publishing.
pub fn apply(
    dir: &Path,
    params: &BuildParams,
    update: &VectorUpdate,
    hooks: ThreadHooks,
    cancel: Option<&AtomicBool>,
) -> Result<ApplyStats> {
    let cancelled = || cancel.is_some_and(|c| c.load(AtomicOrdering::Relaxed));
    std::fs::create_dir_all(dir)?;
    // one writer per store, across handles and processes (held to the end)
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("write.lock"))?;
    if !fs4::fs_std::FileExt::try_lock_exclusive(&lock)? {
        return Err(Error::Invalid("another writer is updating this vector store".into()));
    }
    remove_leftovers(dir);
    let old = VectorStore::open(dir)?;
    let mut params = match &old {
        Some(store) => {
            if store.dims() != params.dims {
                return Err(Error::Invalid(format!("the store has {} dims, the update {}", store.dims(), params.dims)));
            }
            BuildParams { threads: params.threads, ..store.build_params() }
        }
        None => *params,
    };
    if params.dims == 0 {
        return Err(Error::Invalid("dims must be > 0".into()));
    }
    if params.threads == 0 {
        params.threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    }

    // SEFORIM_VECTORS_PROFILE=1: the time of each stage on stderr (CI build measurements)
    let profile = std::env::var_os("SEFORIM_VECTORS_PROFILE").is_some();
    let clock = std::time::Instant::now();
    let stage = |name: &str| {
        if profile {
            eprintln!("vectors-profile: {name} at {} ms", clock.elapsed().as_millis());
        }
    };
    // the new rows, validated and normalized
    let mut new_rows: Vec<(u64, u64, u32, Vec<u16>, Vec<f32>)> = Vec::with_capacity(update.add.len());
    let mut new_chunks = HashSet::with_capacity(update.add.len());
    let mut new_lines: std::collections::HashMap<u64, u64> = std::collections::HashMap::with_capacity(update.add.len());
    for r in &update.add {
        if r.vector.len() != params.dims {
            return Err(Error::Invalid(format!("line {} has {} dims, the store {}", r.line_id, r.vector.len(), params.dims)));
        }
        if !new_chunks.insert((r.line_id, r.chunk)) {
            return Err(Error::Invalid(format!("line {} chunk {} added twice", r.line_id, r.chunk)));
        }
        if *new_lines.entry(r.line_id).or_insert(r.book_id) != r.book_id {
            return Err(Error::Invalid(format!("line {} added in two books", r.line_id)));
        }
        let v = normalized(&r.vector).map_err(|_| Error::Invalid(format!("line {}: a zero or non-finite vector", r.line_id)))?;
        let h = v.iter().map(|&x| f16::f32_to_f16(x)).collect();
        new_rows.push((r.book_id, r.line_id, r.chunk, h, v));
    }
    drop(new_chunks);
    let remove_books: HashSet<u64> = update.remove_books.iter().copied().collect();
    let remove_lines: HashSet<u64> = update.remove_lines.iter().copied().collect();

    // the rows kept from the old generation, and the old keys that go
    let mut rows: Vec<(u64, u64, u32, Source)> = Vec::new();
    let mut gone: Vec<u64> = Vec::new();
    if let Some(store) = &old {
        for i in 0..store.header.books {
            let (book, start, n) = store.book(i);
            for row in start..start + n {
                let line = store.line_of(row);
                if remove_books.contains(&book) || remove_lines.contains(&line) || new_lines.contains_key(&line) {
                    gone.push(line);
                } else {
                    rows.push((book, line, store.chunk_of(row), Source::Old(row)));
                }
            }
        }
    }
    for (i, (book, line, chunk, _, _)) in new_rows.iter().enumerate() {
        rows.push((*book, *line, *chunk, Source::New(i)));
    }
    rows.sort_unstable_by_key(|&(book, line, chunk, _)| (book, line, chunk));
    let line_count = {
        let mut lines: Vec<u64> = rows.iter().map(|r| r.1).collect();
        lines.sort_unstable();
        lines.dedup();
        lines.len() as u64
    };
    if rows.len() > u32::MAX as usize {
        return Err(Error::Invalid("more than 4G vectors".into()));
    }
    if cancelled() {
        return Err(Error::Cancelled);
    }

    stage("rows normalized + old rows listed");
    let generation = old.as_ref().map_or(1, |s| s.generation() + 1);
    let old_count = old.as_ref().map_or(0, |s| s.len());
    let removed_before = old.as_ref().map_or(0, |s| s.header.removed_since_rebuild);
    let rebuild = old.as_ref().and_then(|s| s.graph.as_ref()).is_none()
        || (old_count > 0 && (removed_before + gone.len() as u64) as f64 >= REBUILD_SHARE * old_count as f64);
    let removed_since_rebuild = if rebuild { 0 } else { removed_before + gone.len() as u64 };
    let graph_file = graph_path(dir, generation);
    let mut graph_len = 0u64;
    if !rows.is_empty() {
        let options = index_options(&params);
        let index = usearch::Index::new(&options)?;
        let mut to_add: Vec<(u64, Source)> = Vec::new();
        if rebuild {
            to_add.extend(rows.iter().map(|&(_, line, _, src)| (line, src)));
        } else {
            let store = old.as_ref().unwrap();
            let gpath = graph_path(dir, store.generation());
            index.load(gpath.to_str().ok_or_else(|| Error::Invalid("non UTF-8 path".into()))?)?;
            // remove() drops every vector of a key (multi-key index)
            let mut keys = gone.clone();
            keys.sort_unstable();
            keys.dedup();
            for &line in &keys {
                index.remove(line)?;
            }
            to_add.extend(new_rows.iter().enumerate().map(|(i, r)| (r.1, Source::New(i))));
        }
        stage(if rebuild { "graph created" } else { "old graph loaded, removals done" });
        index.reserve_capacity_and_threads(index.size() + to_add.len(), params.threads)?;
        let t = f16::table();
        let error: std::sync::Mutex<Option<Error>> = std::sync::Mutex::new(None);
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..params.threads.min(to_add.len().max(1)) {
                scope.spawn(|| {
                    if let Some(f) = hooks.start {
                        f();
                    }
                    let mut buf = vec![0f32; params.dims];
                    loop {
                        let i = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if i >= to_add.len() || cancelled() || error.lock().unwrap().is_some() {
                            break;
                        }
                        let (line, src) = to_add[i];
                        let v: &[f32] = match src {
                            Source::New(j) => &new_rows[j].4,
                            Source::Old(row) => {
                                let store = old.as_ref().unwrap();
                                for (d, &h) in buf.iter_mut().zip(store.row_vector(row)) {
                                    *d = t[h as usize];
                                }
                                &buf
                            }
                        };
                        if let Err(e) = index.add(line, v) {
                            *error.lock().unwrap() = Some(e.into());
                            break;
                        }
                    }
                    if let Some(f) = hooks.end {
                        f();
                    }
                });
            }
        });
        if let Some(e) = error.into_inner().unwrap() {
            return Err(e);
        }
        if cancelled() {
            return Err(Error::Cancelled);
        }
        stage("graph vectors added");
        let tmp = with_tmp(&graph_file);
        let _ = std::fs::remove_file(&tmp);
        index.save(tmp.to_str().ok_or_else(|| Error::Invalid("non UTF-8 path".into()))?)?;
        drop(index);
        std::fs::OpenOptions::new().write(true).open(&tmp)?.sync_all()?;
        replace_file(&tmp, &graph_file)?;
        graph_len = std::fs::metadata(&graph_file)?.len();
    }
    if cancelled() {
        let _ = std::fs::remove_file(&graph_file);
        return Err(Error::Cancelled);
    }

    stage("graph saved");
    // the .sev, streamed
    let mut books: Vec<(u64, u64, u64)> = Vec::new();
    for (row, &(book, _, _, _)) in rows.iter().enumerate() {
        match books.last_mut() {
            Some(last) if last.0 == book => last.2 += 1,
            _ => books.push((book, row as u64, 1)),
        }
    }
    let count = rows.len() as u64;
    let off_books = HEADER_LEN as u64;
    let off_lines = align(off_books + books.len() as u64 * 24);
    let off_by_line = align(off_lines + count * 8);
    let off_chunks = align(off_by_line + count * 4);
    let off_vectors = align(off_chunks + count * 4);
    let file_len = off_vectors + count * params.dims as u64 * 2;
    let header = Header {
        dims: params.dims as u64,
        count,
        books: books.len() as u64,
        connectivity: params.connectivity as u64,
        expansion_add: params.expansion_add as u64,
        expansion_search: params.expansion_search as u64,
        off_books,
        off_lines,
        off_by_line,
        off_vectors,
        graph_len,
        file_len,
        generation,
        removed_since_rebuild,
        off_chunks,
        lines: line_count,
    };
    let sev = sev_path(dir, generation);
    let tmp = with_tmp(&sev);
    let result = (|| -> Result<()> {
        let file = File::create(&tmp)?;
        let mut w = BufWriter::with_capacity(1 << 20, file);
        let mut at = 0u64;
        let put = |w: &mut BufWriter<File>, bytes: &[u8], at: &mut u64| -> Result<()> {
            w.write_all(bytes)?;
            *at += bytes.len() as u64;
            Ok(())
        };
        let pad = |w: &mut BufWriter<File>, at: &mut u64, to: u64| -> Result<()> {
            let zeros = vec![0u8; (to - *at) as usize];
            w.write_all(&zeros)?;
            *at = to;
            Ok(())
        };
        put(&mut w, &header.encode(), &mut at)?;
        for &(b, s, n) in &books {
            put(&mut w, &b.to_le_bytes(), &mut at)?;
            put(&mut w, &s.to_le_bytes(), &mut at)?;
            put(&mut w, &n.to_le_bytes(), &mut at)?;
        }
        pad(&mut w, &mut at, off_lines)?;
        for &(_, line, _, _) in &rows {
            put(&mut w, &line.to_le_bytes(), &mut at)?;
        }
        pad(&mut w, &mut at, off_by_line)?;
        let mut by_line: Vec<u32> = (0..count as u32).collect();
        by_line.sort_unstable_by_key(|&r| (rows[r as usize].1, rows[r as usize].2));
        for r in by_line {
            put(&mut w, &r.to_le_bytes(), &mut at)?;
        }
        drop(new_lines);
        pad(&mut w, &mut at, off_chunks)?;
        for &(_, _, chunk, _) in &rows {
            put(&mut w, &chunk.to_le_bytes(), &mut at)?;
        }
        pad(&mut w, &mut at, off_vectors)?;
        let mut bytes = Vec::with_capacity(params.dims * 2);
        for (i, &(_, _, _, src)) in rows.iter().enumerate() {
            if i % 4096 == 0 && cancelled() {
                return Err(Error::Cancelled);
            }
            let v: &[u16] = match src {
                Source::Old(row) => old.as_ref().unwrap().row_vector(row),
                Source::New(j) => &new_rows[j].3,
            };
            bytes.clear();
            for h in v {
                bytes.extend_from_slice(&h.to_le_bytes());
            }
            put(&mut w, &bytes, &mut at)?;
        }
        debug_assert_eq!(at, file_len);
        let file = w.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        file.sync_all()?;
        drop(file);
        replace_file(&tmp, &sev)?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&graph_file);
        return Err(e);
    }
    stage("store (.sev) written");
    drop(old);
    remove_older_generations(dir, generation);
    Ok(ApplyStats {
        generation,
        lines: line_count,
        vectors: count,
        books: books.len() as u64,
        removed: gone.len() as u64,
        added: new_rows.len() as u64,
        rebuilt: rebuild,
    })
}

fn with_tmp(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Deletes generations older than `keep` (best effort: a file still mapped
/// on Windows stays until a later write).
pub fn remove_older_generations(dir: &Path, keep: u64) {
    for g in generations(dir) {
        if g < keep {
            // the .sev first: a generation without it is never opened
            if std::fs::remove_file(sev_path(dir, g)).is_ok() || !sev_path(dir, g).exists() {
                let _ = std::fs::remove_file(graph_path(dir, g));
            }
        }
    }
    // graph files whose .sev is gone (an earlier delete that failed half-way)
    let live = generations(dir);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(g) = name.strip_prefix("vectors-").and_then(|n| n.strip_suffix(".usearch")) {
                if g.parse::<u64>().is_ok_and(|g| !live.contains(&g) && g < keep) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Temp files of a write that never finished.
fn remove_leftovers(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().is_some_and(|n| n.starts_with("vectors-") && n.ends_with(".tmp")) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    // a graph newer than every .sev was never published
    let newest = generations(dir).last().copied().unwrap_or(0);
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(g) = name.strip_prefix("vectors-").and_then(|n| n.strip_suffix(".usearch")) {
                if g.parse::<u64>().is_ok_and(|g| g > newest) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

/// Moves `from` over `to`; on Windows an access-denied or sharing-violation
/// error (a scanner holding the file a moment) is retried for ~500 ms (#7).
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        const DELAYS_MS: [u64; 7] = [5, 10, 20, 40, 75, 150, 200];
        let mut attempt = 0;
        loop {
            match std::fs::rename(from, to) {
                Ok(()) => return Ok(()),
                Err(err) if matches!(err.raw_os_error(), Some(5) | Some(32)) && attempt < DELAYS_MS.len() => {
                    std::thread::sleep(std::time::Duration::from_millis(DELAYS_MS[attempt]));
                    attempt += 1;
                }
                Err(err) => return Err(err),
            }
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

#[cfg(test)]
mod tests;
