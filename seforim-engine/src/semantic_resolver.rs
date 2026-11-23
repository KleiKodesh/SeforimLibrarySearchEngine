//! The live index as a vector set's resolver (issue #18): a port of
//! otzaria_search_engine 7.2.0 `semantic_resolver.rs` (`LiveResolver`,
//! `ResolverCache`, `resolve`). A stored vector knows the key of the text it was
//! embedded from and where that text was when the vectors were built (a book and
//! the ordinal of a line in it, a hint); the index knows where the text is now.
//! Nothing is mapped at open: each search resolves its own hits, lazily.
//!
//! * **The `chunk_key` column** (Otzaria's `chunkKey`, written at index build,
//!   [`crate::LineWriter::set_book_chunk_keys`]), when the index has one under
//!   the production recipe and the query model's recipe keys it: every record is
//!   tried at its hint; a book where one is not is searched once, every line of
//!   it, nearest the hint first; a hit that resolves in none of its books is
//!   looked for once more, with every other such hit of the search, in one pass
//!   over the whole column (where each value was found is kept for the
//!   generation).
//! * **Recomputed from the text** for a window past a line's first (no column
//!   holds those) and for a recipe the column was not written under: the key of
//!   the line at the hint, then within [`RECOMPUTE_REACH`]. The engine has no
//!   database: the rows are asked from the host ([`Resolution::NeedRows`]) and
//!   kept for the generation.
//!
//! Deviations (named): a line found by its column is not re-checked against
//! the key recomputed from its text (Otzaria's `verified`: our column is written
//! in the same pass as the text and every per-book update rewrites it); the
//! filtered scan plans / arrivals (`plan`, `unreached`) are not ported; without
//! the column a book's other lines of a text are not searched (Otzaria finds
//! them by its `lineHash` column, which this index does not hold); a window
//! record whose line moved to another book is not searched for
//! (`moved_places`).
//!
//! A line's text is read as Otzaria's indexing path read it: from its whole
//! book ([`seforim_vectors::chunker::book_lines`] over every row of the book
//! and all its notes), so a note placed by an earlier row that holds `\n` and
//! the first row's BOM dropped when a `data:` is anywhere in the book come out
//! as Otzaria's. The host gives the book's rows in one `need_rows` round; the
//! prepared book is kept for the generation.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, PoisonError};

use lru::LruCache;
use rayon::prelude::*;
use seforim_vectors::chunker::{ChunkKey, Chunker, ChunkerConfig, LineRef};
use seforim_vectors::oxv::{chunk_of_window, ALIAS_WINDOW};
use seforim_vectors::scan::{BookSet, RecordRef, VectorHit, MAX_RECORDS_PER_HIT};
use seforim_vectors::set::{CandidateResolver, LineHit, LineSearch, Resolution};
use tantivy::columnar::Column;
use tantivy::{DocAddress, Searcher};

/// How many books' line maps are kept between searches, and how many lines
/// they hold at most (Otzaria's).
const BOOK_CACHE: usize = 4096;
const BOOK_LINES: usize = 2_000_000;
/// How many books' prepared texts are kept between searches (as the book maps).
const TEXT_BOOKS: usize = BOOK_CACHE;
/// How many values' places a pass over the whole column found are kept.
const MOVED_CACHE: usize = 1024;
/// The most places of one value a pass over the whole column keeps.
const MOVED_PLACES: usize = 1024;
/// How far from its hint a text is looked for when its key is recomputed.
pub const RECOMPUTE_REACH: usize = 16;
/// The most live lines one hit is resolved to.
const MAX_LINES_PER_HIT: usize = MAX_RECORDS_PER_HIT;
/// How many repeat candidates may be recomputed and found not to hold a key.
const MAX_FAILED_REPEATS: usize = 16;

/// One host row: its line id, raw content (as stored, zstd decoded) and its
/// `line_note` rows (`line_index` = the database `lineIndex` of the row, as
/// [`seforim_vectors::chunker::book_lines`] reads it).
#[derive(Debug, Clone)]
pub struct HostRow {
    pub line_id: u64,
    pub raw: Vec<u8>,
    pub notes: Vec<seforim_vectors::chunker::BookNote>,
}

/// A book's lines as the recipe reads them, by (line id, piece of the row):
/// the text and its section (headings counted from the book's start).
type BookTexts = Arc<HashMap<(u64, u32), (String, u64)>>;

/// What a resolver remembers from one search to the next, for one generation
/// of the index (Otzaria's `ResolverCache`).
#[derive(Default)]
pub(crate) struct ResolverCache {
    generation: u64,
    directory: Option<Arc<Directory>>,
    lines: Option<LruCache<u64, Arc<BookLines>>>,
    book_lines: usize,
    texts: Option<LruCache<u64, BookTexts>>,
    text_lines: usize,
    /// Host rows given since the last resolution, by line id, until their
    /// book is prepared.
    pending: HashMap<u64, HostRow>,
    moved: Option<LruCache<u64, Arc<[(DocAddress, u32)]>>>,
    /// Passes over the whole column made in this generation (tests).
    pub(crate) passes: u64,
}

impl ResolverCache {
    fn at(&mut self, generation: u64) -> &mut Self {
        if self.generation != generation {
            *self = Self { generation, ..Self::default() };
        }
        self
    }

    fn lines(&mut self) -> &mut LruCache<u64, Arc<BookLines>> {
        self.lines.get_or_insert_with(|| LruCache::new(NonZeroUsize::new(BOOK_CACHE).unwrap()))
    }

    fn keep_book(&mut self, id: u64, lines: Arc<BookLines>) {
        self.book_lines += lines.docs.len();
        if let Some((_, evicted)) = self.lines().push(id, lines) {
            self.book_lines -= evicted.docs.len();
        }
        while self.book_lines > BOOK_LINES && self.lines().len() > 1 {
            let Some((_, evicted)) = self.lines().pop_lru() else { break };
            self.book_lines -= evicted.docs.len();
        }
    }

    fn texts(&mut self) -> &mut LruCache<u64, BookTexts> {
        self.texts.get_or_insert_with(|| LruCache::new(NonZeroUsize::new(TEXT_BOOKS).unwrap()))
    }

    fn keep_texts(&mut self, id: u64, texts: BookTexts) {
        self.text_lines += texts.len();
        if let Some((_, evicted)) = self.texts().push(id, texts) {
            self.text_lines -= evicted.len();
        }
        while self.text_lines > BOOK_LINES && self.texts().len() > 1 {
            let Some((_, evicted)) = self.texts().pop_lru() else { break };
            self.text_lines -= evicted.len();
        }
    }

    fn moved(&mut self) -> &mut LruCache<u64, Arc<[(DocAddress, u32)]>> {
        self.moved.get_or_insert_with(|| LruCache::new(NonZeroUsize::new(MOVED_CACHE).unwrap()))
    }
}

/// Every live book of a generation: its runs of documents per segment.
struct Directory {
    books: HashMap<u64, Vec<(u32, u32, u32)>>,
}

/// One book's live lines in ordinal order (dense: ordinal = position), each
/// a (document, piece of the row) with its column value.
struct BookLines {
    id: u64,
    docs: Vec<DocAddress>,
    subs: Vec<u32>,
    line_ids: Vec<u64>,
    values: Vec<Option<u64>>,
    repeats: std::sync::OnceLock<HashMap<u64, Box<[u32]>>>,
    anchors: std::sync::OnceLock<Vec<(u64, u32)>>,
}

impl BookLines {
    fn position(&self, ordinal: u32) -> Option<usize> {
        ((ordinal as usize) < self.docs.len()).then_some(ordinal as usize)
    }

    fn by_distance(&self, centre: usize) -> impl Iterator<Item = usize> + '_ {
        let len = self.docs.len();
        (0..len.max(1)).flat_map(move |step| {
            let before = centre.checked_sub(step).filter(|_| step > 0);
            let after = Some(centre + step).filter(|&at| at < len);
            before.into_iter().chain(after)
        })
    }
}

/// The set's name of a host book (Otzaria's `filePath` of a library book).
pub fn book_name(id: u64) -> String {
    format!("id:{id}")
}

/// The host book a set name names.
pub fn book_id_of(name: &str) -> Option<u64> {
    name.strip_prefix("id:")?.parse().ok()
}

/// One value's places, each book's first lines before any book's second
/// (Otzaria's `FairPlaces`).
#[derive(Default)]
struct FairPlaces {
    books: Vec<Vec<(u64, (DocAddress, u32))>>,
    order: HashMap<u64, usize>,
}

impl FairPlaces {
    fn push(&mut self, book: u64, rank: u64, place: (DocAddress, u32)) {
        let at = match self.order.get(&book) {
            Some(&at) => at,
            None if self.books.len() >= MOVED_PLACES => return,
            None => {
                self.books.push(Vec::new());
                self.order.insert(book, self.books.len() - 1);
                self.books.len() - 1
            }
        };
        let places = &mut self.books[at];
        places.push((rank, place));
        if places.len() >= 2 * MAX_LINES_PER_HIT {
            Self::first_lines(places);
        }
    }

    fn first_lines(places: &mut Vec<(u64, (DocAddress, u32))>) {
        places.sort_unstable();
        places.truncate(MAX_LINES_PER_HIT);
    }

    fn into_places(self) -> Vec<(DocAddress, u32)> {
        let mut ranked = Vec::new();
        for (order, mut places) in self.books.into_iter().enumerate() {
            Self::first_lines(&mut places);
            ranked.extend(places.into_iter().enumerate().map(|(rank, (_, place))| (rank, order, place)));
        }
        ranked.sort_unstable_by_key(|&(rank, order, _)| (rank, order));
        ranked.truncate(MOVED_PLACES);
        ranked.into_iter().map(|(_, _, place)| place).collect()
    }
}

struct SegmentColumns {
    line_id: Column<u64>,
    book_id: Column<u64>,
    position: Column<u64>,
    chunk_key: Option<Column<u64>>,
}

/// The resolver over one searcher of the index (Otzaria's `LiveResolver`).
pub(crate) struct LiveResolver<'a> {
    searcher: Searcher,
    chunker: Chunker,
    generation: u64,
    /// Whether the column keys this recipe; otherwise keys are recomputed.
    column: bool,
    columns: Vec<SegmentColumns>,
    cache: &'a Mutex<ResolverCache>,
    /// Rows a text check needed and the cache did not hold.
    missing: Mutex<BTreeSet<u64>>,
}

fn err(e: impl std::fmt::Display) -> seforim_vectors::Error {
    seforim_vectors::Error::Invalid(format!("live index: {e}"))
}

impl<'a> LiveResolver<'a> {
    /// A resolver for `chunking` (the query model's recipe) over `searcher`;
    /// `column_identity`: the recipe the index's column was written under.
    pub(crate) fn new(
        searcher: Searcher,
        chunking: ChunkerConfig,
        column_identity: Option<u64>,
        cache: &'a Mutex<ResolverCache>,
    ) -> seforim_vectors::Result<Self> {
        use std::hash::{Hash, Hasher};
        let production = ChunkerConfig::production();
        let mut geometry = chunking.clone();
        geometry.embedding_text_version = production.embedding_text_version;
        geometry.normalization_version = production.normalization_version;
        if geometry != production && geometry != production.anchor() {
            return Err(err("the live resolver does not support this chunk geometry"));
        }
        let column = column_identity == Some(production.identity()) && chunking.anchor().identity() == production.anchor().identity();
        let chunker = Chunker::new(chunking.clone()).map_err(|e| err(e.0))?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for reader in searcher.segment_readers() {
            (reader.segment_id().uuid_string(), reader.num_deleted_docs()).hash(&mut hasher);
        }
        (chunking.identity(), column).hash(&mut hasher);
        let columns = searcher
            .segment_readers()
            .iter()
            .map(|reader| {
                let fast = reader.fast_fields();
                Ok(SegmentColumns {
                    line_id: fast.u64("line_id").map_err(err)?,
                    book_id: fast.u64("book_id").map_err(err)?,
                    position: fast.u64("position").map_err(err)?,
                    chunk_key: fast.column_opt::<u64>("chunk_key").map_err(err)?,
                })
            })
            .collect::<seforim_vectors::Result<_>>()?;
        Ok(Self { generation: hasher.finish(), searcher, chunker, column, columns, cache, missing: Default::default() })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ResolverCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The live books, built once per generation (one pass over `book_id`).
    fn directory(&self) -> Arc<Directory> {
        if let Some(d) = self.lock().at(self.generation).directory.clone() {
            return d;
        }
        let mut books: HashMap<u64, Vec<(u32, u32, u32)>> = HashMap::new();
        for (seg, reader) in self.searcher.segment_readers().iter().enumerate() {
            let column = &self.columns[seg].book_id;
            let mut run: Option<(u64, u32)> = None;
            for doc in 0..reader.max_doc() {
                let book = column.first(doc).unwrap_or(u64::MAX);
                match run {
                    Some((b, _)) if b == book => {}
                    Some((b, start)) => {
                        books.entry(b).or_default().push((seg as u32, start, doc));
                        run = Some((book, doc));
                    }
                    None => run = Some((book, doc)),
                }
            }
            if let Some((b, start)) = run {
                books.entry(b).or_default().push((seg as u32, start, reader.max_doc()));
            }
        }
        let d = Arc::new(Directory { books });
        self.lock().at(self.generation).directory = Some(d.clone());
        d
    }

    /// Whether the index holds book `name`.
    pub(crate) fn holds_book(&self, name: &str) -> bool {
        book_id_of(name).is_some_and(|id| self.directory().books.contains_key(&id))
    }

    /// A book's live lines; None for a book the index does not hold.
    fn book(&self, name: &str) -> Option<Arc<BookLines>> {
        let id = book_id_of(name)?;
        if let Some(lines) = self.lock().at(self.generation).lines().get(&id) {
            return Some(lines.clone());
        }
        let directory = self.directory();
        let runs = directory.books.get(&id)?;
        let mut found: Vec<(u64, DocAddress)> = Vec::new();
        for &(seg, start, end) in runs {
            let reader = &self.searcher.segment_readers()[seg as usize];
            let position = &self.columns[seg as usize].position;
            for doc in start..end {
                if !reader.is_deleted(doc) {
                    found.push((position.first(doc).unwrap_or(0), DocAddress::new(seg, doc)));
                }
            }
        }
        if found.is_empty() {
            return None;
        }
        found.sort_unstable();
        let mut lines = BookLines {
            id,
            docs: Vec::with_capacity(found.len()),
            subs: Vec::new(),
            line_ids: Vec::new(),
            values: Vec::new(),
            repeats: Default::default(),
            anchors: Default::default(),
        };
        for (_, address) in found {
            let columns = &self.columns[address.segment_ord as usize];
            let line_id = columns.line_id.first(address.doc_id).unwrap_or(0);
            let values: Vec<u64> = columns.chunk_key.as_ref().map(|c| c.values_for_doc(address.doc_id).collect()).unwrap_or_default();
            if values.is_empty() {
                lines.docs.push(address);
                lines.subs.push(0);
                lines.line_ids.push(line_id);
                lines.values.push(None);
            }
            for (sub, value) in values.into_iter().enumerate() {
                lines.docs.push(address);
                lines.subs.push(sub as u32);
                lines.line_ids.push(line_id);
                lines.values.push(Some(value));
            }
        }
        let lines = Arc::new(lines);
        self.lock().at(self.generation).keep_book(id, lines.clone());
        Some(lines)
    }

    fn column_value(&self, book: &BookLines, position: usize) -> Option<u64> {
        if !self.column {
            return None;
        }
        book.values[position].filter(|&v| v != 0)
    }

    /// The texts of `book`'s positions `range` (text, section), read from the
    /// whole book; a book not prepared yet has every row recorded for the host
    /// and None returned.
    fn texts(&self, book: &BookLines, range: std::ops::Range<usize>) -> Option<Vec<(String, u64)>> {
        let texts = self.book_texts(book)?;
        Some(range.map(|p| texts.get(&(book.line_ids[p], book.subs[p])).cloned().unwrap_or_default()).collect())
    }

    /// `book`'s prepared lines: from the cache, or prepared from the host rows
    /// given for every row of the book (Otzaria's whole-book read).
    fn book_texts(&self, book: &BookLines) -> Option<BookTexts> {
        let mut ids: Vec<u64> = book.line_ids.clone();
        ids.dedup();
        let mut guard = self.lock();
        let cache = guard.at(self.generation);
        if let Some(texts) = cache.texts().get(&book.id) {
            return Some(texts.clone());
        }
        if !ids.iter().all(|id| cache.pending.contains_key(id)) {
            drop(guard);
            self.missing.lock().unwrap_or_else(PoisonError::into_inner).extend(ids);
            return None;
        }
        let rows: Vec<HostRow> = ids.iter().map(|id| cache.pending.remove(id).expect("checked above")).collect();
        let raw: Vec<&[u8]> = rows.iter().map(|r| r.raw.as_slice()).collect();
        let mut notes: Vec<seforim_vectors::chunker::BookNote> = rows.iter().flat_map(|r| r.notes.iter().cloned()).collect();
        // the notes query's order: `lineIndex`, then as given (`id`, `noteIndex`)
        notes.sort_by_key(|n| n.line_index);
        let texts: HashMap<(u64, u32), (String, u64)> = seforim_vectors::chunker::book_lines(&raw, &notes)
            .into_iter()
            .map(|l| ((ids[l.row], l.sub as u32), (l.text, l.section)))
            .collect();
        let texts = Arc::new(texts);
        cache.keep_texts(book.id, texts.clone());
        Some(texts)
    }

    /// The window-0 key of the line at `position`, recomputed from its text and
    /// its neighbours' within the recipe's context (`key_at`).
    fn key_at(&self, book: &BookLines, position: usize) -> Option<ChunkKey> {
        let reach = self.chunker.config().context_window_lines;
        let start = position.saturating_sub(reach);
        let end = (position + reach + 1).min(book.docs.len());
        let texts = self.texts(book, start..end)?;
        let refs: Vec<LineRef<'_>> = texts.iter().map(|(text, section)| LineRef { text, section: *section }).collect();
        self.chunker.embedded_text(&refs, position - start).map(|t| ChunkKey::of(&t))
    }

    /// Which window past the first of the line at `position` holds `key`
    /// (`window_of`); None when none does or its row is not here yet.
    fn window_of(&self, book: &BookLines, position: usize, key: ChunkKey) -> Option<u8> {
        let texts = self.texts(book, position..position + 1)?;
        let refs = [LineRef { text: &texts[0].0, section: 0 }];
        self.chunker.window_keys(&refs, 0).into_iter().find(|&(w, k)| w > 0 && k.same_text(key)).map(|(w, _)| w)
    }

    /// Whether line `position` holds `key` as its first window.
    fn holds0(&self, book: &BookLines, position: usize, key: ChunkKey) -> bool {
        if self.column {
            self.column_value(book, position) == Some(key.column_value())
        } else {
            self.key_at(book, position).is_some_and(|k| k.same_text(key))
        }
    }

    /// `record_at`: the window of line `position` that holds the record's key.
    fn record_at(&self, book: &BookLines, position: usize, key: ChunkKey, record: &RecordRef) -> Option<u8> {
        if record.window == ALIAS_WINDOW {
            let anchor = ChunkKey(record.anchor?);
            return self.holds0(book, position, anchor).then_some(ALIAS_WINDOW);
        }
        if record.window == 0 {
            return self.holds0(book, position, key).then_some(0);
        }
        self.window_of(book, position, key)
    }

    /// `anchor_candidates`: the positions whose column holds `value`, nearest
    /// `hint` first.
    fn anchor_candidates(&self, book: &BookLines, value: u64, hint: u32) -> Vec<usize> {
        let values = book.anchors.get_or_init(|| {
            let mut v: Vec<(u64, u32)> = (0..book.docs.len()).filter_map(|p| Some((self.column_value(book, p)?, p as u32))).collect();
            v.sort_unstable();
            v
        });
        let start = values.partition_point(|&(held, _)| held < value);
        let mut out: Vec<usize> = values[start..].iter().take_while(|&&(held, _)| held == value).map(|&(_, p)| p as usize).collect();
        out.sort_unstable_by_key(|&p| ((p as u32).abs_diff(hint), p));
        out
    }

    /// `search_record`: up to `limit` lines of `book` not `taken` holding the
    /// record's key, nearest its hint first, with their window.
    fn search_record(&self, book: &BookLines, key: ChunkKey, record: &RecordRef, limit: usize, taken: &HashSet<(DocAddress, u32)>) -> Vec<(usize, u8)> {
        if record.window == ALIAS_WINDOW {
            let Some(anchor) = record.anchor else { return Vec::new() };
            let line = RecordRef { window: 0, anchor: Some(anchor), ..record.clone() };
            return self.search_record(book, ChunkKey(anchor), &line, limit, taken).into_iter().map(|(p, _)| (p, ALIAS_WINDOW)).collect();
        }
        let place = |p: usize| (book.docs[p], book.subs[p]);
        if record.window == 0 && self.column {
            return self
                .anchor_candidates(book, key.column_value(), record.hint)
                .into_iter()
                .filter(|&p| !taken.contains(&place(p)))
                .take(limit)
                .map(|p| (p, 0))
                .collect();
        }
        if let Some(anchor) = record.anchor.filter(|_| record.window > 0 && self.column) {
            let mut candidates = self.anchor_candidates(book, ChunkKey(anchor).column_value(), record.hint);
            if let Some(p) = book.position(record.hint) {
                if !candidates.contains(&p) {
                    candidates.push(p);
                }
            }
            candidates.sort_unstable_by_key(|&p| ((p as u32).abs_diff(record.hint), p));
            let found: Vec<(usize, u8)> = candidates
                .into_iter()
                .filter(|&p| !taken.contains(&place(p)))
                .filter_map(|p| self.record_at(book, p, key, record).map(|w| (p, w)))
                .take(limit)
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        self.search_book(book, key, record.window, record.hint, limit, taken)
    }

    /// `search_book`: by column over the whole book, or within the reach
    /// from the text (a window, or no column).
    fn search_book(&self, book: &BookLines, key: ChunkKey, window: u8, hint: u32, limit: usize, taken: &HashSet<(DocAddress, u32)>) -> Vec<(usize, u8)> {
        let centre = (hint as usize).min(book.docs.len() - 1);
        let place = |p: usize| (book.docs[p], book.subs[p]);
        if window > 0 {
            let near: Vec<usize> = book.by_distance(centre).take(2 * RECOMPUTE_REACH + 1).filter(|&p| !taken.contains(&place(p))).collect();
            // ask for the whole reach at once: one round to the host
            let _ = self.texts(book, near.iter().copied().min().unwrap_or(0)..near.iter().copied().max().map_or(0, |m| m + 1));
            return near.into_iter().filter_map(|p| self.window_of(book, p, key).map(|w| (p, w))).take(limit).collect();
        }
        if self.column {
            let wanted = key.column_value();
            return book
                .by_distance(centre)
                .filter(|&p| !taken.contains(&place(p)) && self.column_value(book, p) == Some(wanted))
                .take(limit)
                .map(|p| (p, 0))
                .collect();
        }
        let start = centre.saturating_sub(RECOMPUTE_REACH);
        let end = (centre + RECOMPUTE_REACH + 1).min(book.docs.len());
        let reach = self.chunker.config().context_window_lines;
        let _ = self.texts(book, start.saturating_sub(reach)..(end + reach).min(book.docs.len()));
        let mut found: Vec<(usize, u8)> =
            (start..end).filter(|&p| !taken.contains(&place(p)) && self.key_at(book, p).is_some_and(|k| k.same_text(key))).map(|p| (p, 0)).collect();
        found.sort_by_key(|&(p, _)| (p.abs_diff(centre), p));
        found.truncate(limit);
        found
    }

    /// `repeats`: every column value more than one line of the book holds.
    fn repeats<'b>(&self, book: &'b BookLines) -> &'b HashMap<u64, Box<[u32]>> {
        book.repeats.get_or_init(|| {
            let mut values: Vec<(u64, u32)> = (0..book.docs.len()).filter_map(|p| Some((self.column_value(book, p)?, p as u32))).collect();
            values.sort_unstable();
            values.chunk_by(|a, b| a.0 == b.0).filter(|run| run.len() > 1).map(|run| (run[0].0, run.iter().map(|&(_, p)| p).collect())).collect()
        })
    }

    /// `repeats_of` (with the column): the other lines of `book` holding the
    /// key as `position` does; a window's are held to its windows within
    /// `budget` (`window_repeats`).
    fn repeats_of(&self, book: &BookLines, position: usize, key: ChunkKey, window: u8, limit: usize, taken: &HashSet<(DocAddress, u32)>, budget: &mut usize) -> Vec<usize> {
        let Some(value) = self.column_value(book, position) else { return Vec::new() };
        let Some(positions) = self.repeats(book).get(&value) else { return Vec::new() };
        let others = positions.iter().map(|&p| p as usize).filter(|&p| p != position && !taken.contains(&(book.docs[p], book.subs[p])));
        if window == 0 {
            return others.take(limit).collect();
        }
        let mut found = Vec::new();
        for other in others {
            if found.len() == limit || *budget == 0 {
                break;
            }
            if self.window_of(book, other, key).is_some() {
                found.push(other);
            } else {
                *budget -= 1;
            }
        }
        found
    }

    /// `prefetch`: the books the hits name, read in parallel; the rows the
    /// text checks will need asked for together.
    fn prefetch(&self, hits: &[VectorHit], admitted: &dyn Fn(&str) -> bool) -> HashMap<String, Arc<BookLines>> {
        let mut names: Vec<&str> = hits.iter().flat_map(|h| h.records.iter().map(|r| r.book.as_str())).filter(|n| admitted(n)).collect();
        names.sort_unstable();
        names.dedup();
        let books: HashMap<String, Arc<BookLines>> = names.into_par_iter().filter_map(|n| Some((n.to_string(), self.book(n)?))).collect();
        for hit in hits {
            for record in &hit.records {
                let Some(book) = books.get(&record.book) else { continue };
                let Some(p) = book.position(record.hint) else { continue };
                if record.window > 0 && record.window != ALIAS_WINDOW {
                    let start = p.saturating_sub(RECOMPUTE_REACH);
                    let _ = self.texts(book, start..(p + RECOMPUTE_REACH + 1).min(book.docs.len()));
                } else if !self.column {
                    let reach = RECOMPUTE_REACH + self.chunker.config().context_window_lines;
                    let _ = self.texts(book, p.saturating_sub(reach)..(p + reach + 1).min(book.docs.len()));
                }
            }
        }
        books
    }

    /// `find_everywhere`: one pass over every column for `wanted`.
    fn find_everywhere(&self, wanted: &HashSet<u64>, cancel: Option<&AtomicBool>) -> seforim_vectors::Result<HashMap<u64, Vec<(DocAddress, u32)>>> {
        let mut found: HashMap<u64, FairPlaces> = HashMap::new();
        for (seg, reader) in self.searcher.segment_readers().iter().enumerate() {
            let columns = &self.columns[seg];
            let Some(column) = &columns.chunk_key else { continue };
            for (checked, doc) in reader.doc_ids_alive().enumerate() {
                if checked % 65_536 == 0 && cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
                    return Err(seforim_vectors::Error::Cancelled);
                }
                for (sub, value) in column.values_for_doc(doc).enumerate() {
                    if value == 0 || !wanted.contains(&value) {
                        continue;
                    }
                    let book = columns.book_id.first(doc).unwrap_or(u64::MAX);
                    let rank = (columns.position.first(doc).unwrap_or(0) << 8) | sub.min(255) as u64;
                    found.entry(value).or_default().push(book, rank, (DocAddress::new(seg as u32, doc), sub as u32));
                }
            }
        }
        Ok(found.into_iter().map(|(v, p)| (v, p.into_places())).collect())
    }

    /// `locate`: the book of a place and its position there.
    fn locate(&self, place: (DocAddress, u32)) -> Option<(Arc<BookLines>, usize)> {
        let book = self.columns[place.0.segment_ord as usize].book_id.first(place.0.doc_id)?;
        let book = self.book(&book_name(book))?;
        let p = book.docs.iter().zip(&book.subs).position(|(d, s)| *d == place.0 && *s == place.1)?;
        Some((book, p))
    }

    /// The rows the last resolution lacked.
    fn take_missing(&self) -> Vec<u64> {
        std::mem::take(&mut *self.missing.lock().unwrap_or_else(PoisonError::into_inner)).into_iter().collect()
    }
}

/// A line found for hit `hit`.
struct Found {
    hit: usize,
    line_id: u64,
    book_id: u64,
    window: u8,
}

impl CandidateResolver for LiveResolver<'_> {
    fn admissible_books(&self, ids: &[u64]) -> BookSet {
        ids.iter().map(|&id| book_name(id)).collect()
    }

    /// `resolve` (semantic_resolver.rs:2802), step for step.
    fn resolve(&self, hits: &[VectorHit], books: Option<&BookSet>, options: &LineSearch, cancel: Option<&AtomicBool>) -> seforim_vectors::Result<Resolution> {
        let cancelled = || cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed));
        let admitted = |name: &str| books.is_none_or(|b| b.contains(name));
        let loaded = self.prefetch(hits, &admitted);
        let book = |name: &str| loaded.get(name).cloned().or_else(|| self.book(name));
        let mut lines: Vec<Found> = Vec::new();
        let mut taken: HashSet<(DocAddress, u32)> = HashSet::new();
        let mut unresolved: Vec<usize> = Vec::new();
        let mut unresolved_records = 0u64;
        for (hit_index, hit) in hits.iter().enumerate() {
            if cancelled() {
                return Err(seforim_vectors::Error::Cancelled);
            }
            taken.clear();
            let key = ChunkKey(hit.key);
            let mut emitted = 0usize;
            let mut found: Vec<(Arc<BookLines>, usize, u8)> = Vec::new();
            let mut missed: Vec<(Arc<BookLines>, &RecordRef)> = Vec::new();
            let push = |lines: &mut Vec<Found>, book: &BookLines, p: usize, window: u8| {
                lines.push(Found { hit: hit_index, line_id: book.line_ids[p], book_id: book.id, window });
            };
            // one line for each record: at its hint, or the nearest of its book
            for record in &hit.records {
                if emitted == MAX_LINES_PER_HIT {
                    break;
                }
                if !admitted(&record.book) {
                    continue;
                }
                let Some(b) = book(&record.book) else { continue };
                let at = b.position(record.hint).and_then(|p| self.record_at(&b, p, key, record).map(|w| (p, w)));
                let Some((p, window)) = at else {
                    missed.push((b, record));
                    continue;
                };
                if !taken.insert((b.docs[p], b.subs[p])) {
                    continue;
                }
                push(&mut lines, &b, p, window);
                emitted += 1;
                found.push((b, p, window));
            }
            let mut searched: HashSet<u64> = HashSet::new();
            for (b, record) in &missed {
                if emitted == MAX_LINES_PER_HIT {
                    break;
                }
                if self.column && record.window == 0 && !searched.insert(b.id) {
                    continue;
                }
                if let Some(&(p, window)) = self.search_record(b, key, record, 1, &taken).first() {
                    taken.insert((b.docs[p], b.subs[p]));
                    push(&mut lines, b, p, window);
                    emitted += 1;
                    found.push((b.clone(), p, window));
                }
            }
            // each book's other lines of the text, in the order its first was found
            let mut budget = MAX_FAILED_REPEATS;
            let mut room = MAX_LINES_PER_HIT - emitted;
            let mut repeats: Vec<Vec<usize>> = Vec::new();
            for (b, p, window) in &found {
                if room == 0 {
                    break;
                }
                let held: Vec<usize> = if *window == ALIAS_WINDOW {
                    match self.column_value(b, *p) {
                        Some(anchor) => self
                            .anchor_candidates(b, anchor, *p as u32)
                            .into_iter()
                            .filter(|&o| !taken.contains(&(b.docs[o], b.subs[o])))
                            .take(room)
                            .collect(),
                        None => Vec::new(),
                    }
                } else {
                    self.repeats_of(b, *p, key, *window, room, &taken, &mut budget)
                };
                taken.extend(held.iter().map(|&o| (b.docs[o], b.subs[o])));
                room -= held.len();
                repeats.push(held);
            }
            for ((b, _, window), mut held) in found.iter().zip(repeats) {
                held.sort_unstable();
                for other in held {
                    let w = match *window {
                        0 => 0,
                        ALIAS_WINDOW => ALIAS_WINDOW,
                        _ => self.window_of(b, other, key).unwrap_or(*window),
                    };
                    push(&mut lines, b, other, w);
                    emitted += 1;
                }
            }
            if emitted == 0 {
                unresolved_records += hit.records.iter().filter(|r| admitted(&r.book)).count() as u64;
                if hit.records.iter().any(|r| r.window == 0) {
                    unresolved.push(hit_index);
                }
            }
        }
        let missing = self.take_missing();
        if !missing.is_empty() {
            return Ok(Resolution::NeedRows(missing));
        }
        // the text moved to another book: one pass over the whole column
        if self.column && !unresolved.is_empty() {
            let mut places: HashMap<u64, Arc<[(DocAddress, u32)]>> = HashMap::new();
            let mut wanted: HashSet<u64> = HashSet::new();
            {
                let mut cache = self.lock();
                let moved = cache.at(self.generation).moved();
                for &h in &unresolved {
                    let value = ChunkKey(hits[h].key).column_value();
                    if value == 0 || places.contains_key(&value) {
                        continue;
                    }
                    match moved.get(&value) {
                        Some(p) => {
                            places.insert(value, p.clone());
                        }
                        None => {
                            wanted.insert(value);
                        }
                    }
                }
            }
            if !wanted.is_empty() {
                let mut passed = self.find_everywhere(&wanted, cancel)?;
                let mut cache = self.lock();
                let cache = cache.at(self.generation);
                cache.passes += 1;
                for value in wanted {
                    let p: Arc<[(DocAddress, u32)]> = passed.remove(&value).unwrap_or_default().into();
                    cache.moved().put(value, p.clone());
                    places.insert(value, p);
                }
            }
            for &h in &unresolved {
                taken.clear();
                let Some(at) = places.get(&ChunkKey(hits[h].key).column_value()) else { continue };
                let mut emitted = 0usize;
                let mut seen_books: HashSet<u64> = HashSet::new();
                let mut others: Vec<(Arc<BookLines>, usize)> = Vec::new();
                for &place in at.iter() {
                    if emitted == MAX_LINES_PER_HIT {
                        break;
                    }
                    if taken.contains(&place) {
                        continue;
                    }
                    let Some((b, p)) = self.locate(place) else { continue };
                    if !admitted(&book_name(b.id)) {
                        continue;
                    }
                    if seen_books.contains(&b.id) {
                        others.push((b, p));
                        continue;
                    }
                    taken.insert(place);
                    lines.push(Found { hit: h, line_id: b.line_ids[p], book_id: b.id, window: 0 });
                    emitted += 1;
                    seen_books.insert(b.id);
                }
                others.sort_by_key(|(b, p)| (b.id, *p));
                for (b, p) in others {
                    if emitted == MAX_LINES_PER_HIT {
                        break;
                    }
                    if taken.insert((b.docs[p], b.subs[p])) {
                        lines.push(Found { hit: h, line_id: b.line_ids[p], book_id: b.id, window: 0 });
                        emitted += 1;
                    }
                }
            }
        }
        // occurrence penalties decide which hit is best for a line
        let score = |f: &Found| {
            let hit = &hits[f.hit];
            hit.score + hit.applied_penalty
                - match f.window {
                    0 => 0.0,
                    ALIAS_WINDOW => options.alias_penalty,
                    _ => hit.window_penalty,
                }
        };
        lines.sort_by(|a, b| score(b).total_cmp(&score(a)));
        let mut seen = HashSet::new();
        let out = lines
            .iter()
            .filter(|f| seen.insert(f.line_id))
            .map(|f| LineHit { line_id: f.line_id, book_id: f.book_id, score: score(f), chunk: chunk_of_window(f.window) })
            .collect();
        Ok(Resolution::Lines { lines: out, unresolved: unresolved_records })
    }
}

impl ResolverCache {
    /// Keeps host rows for the generation `generation` until their book is
    /// prepared (the next resolution).
    pub(crate) fn add_rows(&mut self, generation: u64, rows: Vec<HostRow>) {
        let cache = self.at(generation);
        for row in rows {
            cache.pending.insert(row.line_id, row);
        }
    }
}

impl LiveResolver<'_> {
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
mod tests {
    use crate::semantic::{SemanticOptions, SemanticResult};
    use crate::{IndexOptions, LineDoc, LineIndex};
    use std::path::Path;

    /// The rows of the hosts' `verified.oxv` (book `id:3`, line p = axis p).
    const ROWS: [&str; 4] = [
        "<h1>Title of the book</h1>",
        "first line that is long enough to stand",
        "second\nthird line long enough to stand alone too",
        "fourth line long enough to stand alone here",
    ];

    fn fixture() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../seforim-vectors/tests/data/verified.oxv")
    }

    fn axis(a: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; 8];
        v[a] = 1.0;
        v
    }

    /// Writes `books` (book id, rows as (line id, text)) with their chunk keys.
    fn write(index: &LineIndex, books: &[(u64, Vec<(u64, &str)>)], first_position: u64) {
        let mut w = index.writer(50_000_000, 1).unwrap();
        let mut position = first_position;
        for (book, rows) in books {
            w.delete_book(*book).unwrap();
            let ids: Vec<u64> = rows.iter().map(|r| r.0).collect();
            let raw: Vec<&[u8]> = rows.iter().map(|r| r.1.as_bytes()).collect();
            w.set_book_chunk_keys(&ids, &raw, &[]).unwrap();
            for &(id, text) in rows {
                w.add(&LineDoc { line_id: id, position, book_id: *book, text: text.into(), section_id: None, no_cross: false }).unwrap();
                position += 1;
            }
        }
        w.commit(None).unwrap();
    }

    fn index(dir: &Path, books: &[(u64, Vec<(u64, &str)>)]) -> LineIndex {
        let index = LineIndex::create(dir, IndexOptions::default()).unwrap();
        write(&index, books, 0);
        index
    }

    fn book3(ids: [u64; 4]) -> Vec<(u64, &'static str)> {
        ids.into_iter().zip(ROWS).collect()
    }

    fn search(index: &LineIndex, a: usize, k: usize) -> SemanticResult {
        let r = index.semantic_search(&axis(a), &SemanticOptions { k, ..Default::default() }, None).unwrap();
        assert!(r.need_rows.is_empty(), "window-0 records resolve by the column alone");
        r
    }

    fn top(index: &LineIndex, a: usize) -> Option<u64> {
        search(index, a, 1).hits.first().map(|h| h.line_id)
    }

    /// The lazy resolver gives the hits the old eager per-book mapping gave:
    /// ordinal p of book 3 -> its row's line id (a row holding a newline is two
    /// ordinals of one line id), opening maps nothing.
    #[test]
    fn hits_equal_the_eager_mapping() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index(tmp.path(), &[(3, book3([100, 101, 102, 103]))]);
        let info = index.open_vectors(&fixture()).unwrap();
        assert_eq!((info.books, info.books_mapped, info.books_unmapped), (1, 1, 0));
        let eager = [100u64, 101, 102, 102, 103];
        for (p, want) in eager.iter().enumerate() {
            assert_eq!(top(&index, p), Some(*want), "axis {p}");
        }
        let r = search(&index, 2, 5);
        assert_eq!(r.unmapped, 0);
        // the newline row's two ordinals are one line: once, at its best
        let lines: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
        assert_eq!(lines.iter().filter(|&&l| l == 102).count(), 1);
    }

    /// A row inserted above: every line is found by its column in its book;
    /// a book moved to another book id: one pass over the whole column, kept
    /// for the generation (a second search makes none).
    #[test]
    fn moved_lines_and_books_and_the_caches() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rows = vec![(99u64, "an inserted line long enough to stand alone")];
        rows.extend(book3([100, 101, 102, 103]));
        rows.swap(0, 1); // the heading stays first
        let shifted = index(tmp.path(), &[(3, rows)]);
        shifted.open_vectors(&fixture()).unwrap();
        assert_eq!(top(&shifted, 1), Some(101));
        assert_eq!(top(&shifted, 4), Some(103));

        let tmp = tempfile::tempdir().unwrap();
        let index = index(tmp.path(), &[(7, book3([200, 201, 202, 203]))]);
        let info = index.open_vectors(&fixture()).unwrap();
        assert_eq!(info.books_unmapped, 1, "the set's book 3 is not in the index");
        let passes = |i: &LineIndex| i.vectors.resolver_passes();
        assert_eq!(top(&index, 1), Some(201));
        assert_eq!(passes(&index), 1);
        assert_eq!(top(&index, 1), Some(201));
        assert_eq!(passes(&index), 1, "where a value is was kept");
        // a filter of the book's id: not admitted (the set names book 3)
        let r = index.semantic_search(&axis(1), &SemanticOptions { k: 1, books: Some(vec![7]), ..Default::default() }, None).unwrap();
        assert!(r.hits.is_empty());
    }

    /// A text a book holds twice: the vector set records it once, both lines
    /// come back (`repeats_of`).
    #[test]
    fn a_repeated_text_resolves_to_every_line() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rows = book3([100, 101, 102, 103]);
        rows.push((104, ROWS[1]));
        let index = index(tmp.path(), &[(3, rows)]);
        index.open_vectors(&fixture()).unwrap();
        let r = search(&index, 1, 5);
        let at_one: Vec<u64> = r.hits.iter().filter(|h| (h.score - 1.0).abs() < 1e-5).map(|h| h.line_id).collect();
        assert_eq!(at_one, vec![101, 104], "{r:?}");
    }

    /// A per-book update rewrites the book's keys: a changed line no longer
    /// holds its vector (counted), the others resolve on the new generation.
    #[test]
    fn a_per_book_update_rewrites_the_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index(tmp.path(), &[(3, book3([100, 101, 102, 103]))]);
        index.open_vectors(&fixture()).unwrap();
        assert_eq!(top(&index, 4), Some(103));
        let mut rows = book3([110, 111, 112, 113]);
        rows[3].1 = "fourth line CHANGED enough to stand alone here";
        write(&index, &[(3, rows)], 10);
        let r = search(&index, 4, 1);
        assert!(r.hits.is_empty() && r.unmapped == 1, "{r:?}");
        assert_eq!(top(&index, 1), Some(111));
    }

    /// An index without keys (built before the column, or by a host that
    /// sets none) resolves nothing: the records are counted.
    #[test]
    fn no_keys_no_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let index = LineIndex::create(tmp.path(), IndexOptions::default()).unwrap();
        {
            let mut w = index.writer(50_000_000, 1).unwrap();
            for (i, (id, text)) in book3([100, 101, 102, 103]).into_iter().enumerate() {
                w.add(&LineDoc { line_id: id, position: i as u64, book_id: 3, text: text.into(), section_id: None, no_cross: false }).unwrap();
            }
            w.commit(None).unwrap();
        }
        index.open_vectors(&fixture()).unwrap();
        let r = index.semantic_search(&axis(1), &SemanticOptions { k: 1, ..Default::default() }, None).unwrap();
        assert!(r.hits.is_empty());
    }

    /// Each line's `lineHash` from the index (identical-text grouping): equal
    /// texts in two books sign alike, a short line has none, a missing line 0.
    #[test]
    fn line_hashes_from_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "a line of text that is long enough";
        let heb: String = (0..14).map(|i| char::from_u32(0x05D0 + i).unwrap()).collect();
        let a = format!("{heb} {long}");
        let index = index(tmp.path(), &[(3, vec![(100, a.as_str()), (101, "short")]), (4, vec![(200, a.as_str())])]);
        let h = index.line_hashes(&[100, 101, 200, 999]).unwrap();
        assert_ne!(h[0], 0);
        assert_eq!((h[1], h[2], h[3]), (0, h[0], 0));
    }

    /// Each Otzaria line's `lineCheck` is written with the book rows: CRC-32 of
    /// every newline piece as prepared (one value per piece, as `chunk_key`).
    #[test]
    fn line_checks_in_the_index() {
        let tmp = tempfile::tempdir().unwrap();
        let index = index(tmp.path(), &[(3, book3([100, 101, 102, 103]))]);
        let searcher = index.reader.searcher();
        let seg = searcher.segment_reader(0);
        let checks = seg.fast_fields().column_opt::<u64>("line_check").unwrap().unwrap();
        let ids = seg.fast_fields().u64("line_id").unwrap();
        let doc = (0..seg.max_doc()).find(|&d| ids.first(d) == Some(102)).unwrap();
        let got: Vec<u64> = checks.values_for_doc(doc).collect();
        let want: Vec<u64> = ROWS[2].split('\n').map(|p| u64::from(seforim_vectors::chunker::line_check(p))).collect();
        assert_eq!(got, want);
    }

}
