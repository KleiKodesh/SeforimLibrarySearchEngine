//! The line index: one tantivy doc per line, fed by the host.
//!
//! Bare-minimum rule: the engine works in tandem with seforim.db (SQLite),
//! so a doc holds only the postings (with freqs/positions when the index is
//! built with positions, for span/slop queries) plus the ids the engine
//! itself needs: `line_id`, the catalog `position` (results come back in that
//! order) and `book_id` (for filtered search by book). No text, nothing the
//! host can read from SQLite by line id (line index, sections, references):
//! grouping and placement are the host's job on the returned line ids. An
//! index that keeps marks also flags the lines that carry a mark (`marked`,
//! one term, those lines only): Otzaria's vocalized search sees only such
//! lines. Positions in the text postings are optional: without them the
//! index is smaller and word distance is validated by the snippet locator
//! on the line text (FtsLib's way); with them, span queries also work.
//!
//! Three small per-line fast fields serve cross-line matching and the
//! same-section scope, read only when those options are used: the line's
//! word count (computed here, from the same scanner that numbers the
//! positions), its line flags (bit 0: no phrase continues into or out of
//! the line; the host sets it for headings) and its section id (the host's,
//! from seforim.db; absent = the line is a section of its own).

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tantivy::directory::MmapDirectory;
use tantivy::indexer::IndexWriterOptions;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED,
};
use tantivy::{doc, Index, IndexReader, IndexSettings, IndexSortByField, IndexWriter, Order, ReloadPolicy, Searcher, Term};

use crate::Result;

/// How the index is built. Fixed at creation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IndexOptions {
    /// Store word positions in the postings. Off: smaller index, distance is
    /// validated from the text only.
    pub positions: bool,
    /// Keep nikud / te'amim (and combining accents) in the indexed terms,
    /// in canonical order (`hebrew_marked` tokenizer), and flag the lines
    /// that carry an attached mark. Every walk reads the marks as zero
    /// width, so a search returns exactly what the mark-stripped index
    /// returns; the `match_nikud` / `match_teamim` options (Otzaria's
    /// vocalized search) need it. Off (default): today's mark-stripped
    /// index, smaller and with faster broad walks.
    pub keep_marks: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            positions: true,
            keep_marks: false,
        }
    }
}

/// Field handles.
#[derive(Debug, Clone, Copy)]
pub struct LineFields {
    /// Host line id. Indexed (deletes/lookups) and fast.
    pub line_id: Field,
    /// Catalog position: results come back ordered by it.
    pub position: Field,
    /// Book id: the one placement field kept, for filtered search by book.
    pub book_id: Field,
    /// The line text, Hebrew-tokenized.
    pub text: Field,
    /// `keep_marks` only: the lines carrying an attached mark hold the term
    /// [`MARKED_LINE`] (Otzaria's `contains_attached_marks`, the lines its
    /// vocalized field `textVocalized` holds). None on a mark-stripped index
    /// or a marks index built before the flag existed.
    pub marked: Option<Field>,
    /// Position + 1 of the line's first CONTENT word, after a leading
    /// enumerator such as `(x)` (Otzaria's `lineFirst`; 0: no content word).
    /// Cross-line matching. None: an index built before the field existed.
    pub line_first: Option<Field>,
    /// Position + 1 of the line's last content word, before a trailing
    /// enumerator such as `{x}` or `(49)` (Otzaria's `lineLast`; 0: none).
    pub line_last: Option<Field>,
    /// [`LINE_FLAG_NO_CROSS`] and future per-line bits.
    pub line_flags: Option<Field>,
    /// The host's section id (same-section scope); absent on a line = a
    /// section of its own.
    pub section_id: Option<Field>,
}

/// Line flag: no phrase continues into or out of this line (Otzaria: a
/// heading, `<h...>`, interrupts the text).
pub const LINE_FLAG_NO_CROSS: u64 = 1;

/// Line flag: the line starts with an enumerator (`line_first` skips it).
pub const LINE_FLAG_LEADING_MARKER: u64 = 2;

/// Line flag: the line ends with an enumerator (`line_last` stops before it).
pub const LINE_FLAG_TRAILING_MARKER: u64 = 4;

/// Line flag: the line holds a word too long to be a term (128 bytes or
/// more). A line regex's candidate path adds these lines, since no term of
/// the dictionary names that word.
pub const LINE_FLAG_LONG_WORD: u64 = 8;

/// The `marked` field's one term.
pub const MARKED_LINE: u64 = 1;

const OPTIONS_FILE: &str = "engine_options.json";

fn line_schema(options: IndexOptions) -> (Schema, LineFields) {
    let mut b = Schema::builder();
    let line_id = b.add_u64_field("line_id", INDEXED | FAST);
    let position = b.add_u64_field("position", FAST);
    let book_id = b.add_u64_field("book_id", FAST);
    let tokenizer = if options.keep_marks {
        hebrew_fts::tokenizer::HEBREW_MARKED_TOKENIZER_NAME
    } else {
        hebrew_fts::HEBREW_TOKENIZER_NAME
    };
    let record = if options.positions {
        IndexRecordOption::WithFreqsAndPositions
    } else {
        IndexRecordOption::Basic
    };
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(tokenizer)
        .set_index_option(record);
    let text = b.add_text_field("text", TextOptions::default().set_indexing_options(indexing));
    let marked = options.keep_marks.then(|| b.add_u64_field("marked", INDEXED));
    // after `text`: the older fields keep their ids in an older index
    let line_first = b.add_u64_field("line_first", FAST);
    let line_last = b.add_u64_field("line_last", FAST);
    let line_flags = b.add_u64_field("line_flags", FAST);
    let section_id = b.add_u64_field("section_id", FAST);
    (
        b.build(),
        LineFields {
            line_id,
            position,
            book_id,
            text,
            marked,
            line_first: Some(line_first),
            line_last: Some(line_last),
            line_flags: Some(line_flags),
            section_id: Some(section_id),
        },
    )
}

/// One line as the host feeds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineDoc {
    pub line_id: u64,
    /// Rank in the library's catalog order.
    pub position: u64,
    pub book_id: u64,
    /// Raw line text (HTML allowed).
    pub text: String,
    /// The host's section of the line (same-section scope): one id per
    /// heading block (seforim.db: its TOC entry). Ids may repeat across
    /// books (the engine keys sections by book). None: the line is a
    /// section of its own.
    #[serde(default)]
    pub section_id: Option<u64>,
    /// No phrase continues into or out of this line (cross-line matching):
    /// set by the host for headings (Otzaria's rule). Empty lines need no
    /// flag: they have no words.
    #[serde(default)]
    pub no_cross: bool,
}

/// An open line index.
pub struct LineIndex {
    pub(crate) index: Index,
    pub(crate) fields: LineFields,
    pub(crate) options: IndexOptions,
    pub(crate) dir: PathBuf,
    /// The ONE reader every search, count and sidecar check takes its
    /// searcher from (issue #22). Manual reload policy, no file watcher:
    /// the engine reloads it after every commit, rollback and merge its own
    /// [`LineWriter`] performs. A running search keeps the searcher (the
    /// snapshot) it started with. When ANOTHER process writes the index,
    /// call [`LineIndex::reload`] (or reopen).
    pub(crate) reader: SharedReader,
    /// The FM-index sidecar, opened on first use (see `fm_sidecar`).
    pub(crate) fm: std::sync::Arc<crate::fm_sidecar::FmCache>,
    /// The sidecar's automatic background rebuild (see `fm_sidecar::Repair`).
    pub(crate) fm_repair: std::sync::Arc<crate::fm_sidecar::Repair>,
    /// TEST ONLY (compiled out of every non-test build): forces leading-`*`
    /// and fuzzy to run as FST walks although a sidecar exists, so a test
    /// can assert the sidecar and the walk give identical terms.
    #[cfg(test)]
    pub(crate) test_only_disable_fm: std::sync::atomic::AtomicBool,
    /// TEST ONLY: an index with positions leaves its hits undecided (the
    /// text decides them, as before issue #21), so a test can compare the
    /// two decisions.
    #[cfg(test)]
    pub(crate) test_only_text_decision: std::sync::atomic::AtomicBool,
    /// TEST ONLY: a streamed search collects every hit first (no scan from
    /// the scorer, one cross-line pass over every line).
    #[cfg(test)]
    pub(crate) test_only_no_scan: std::sync::atomic::AtomicBool,
    /// The background full merge after an update (issue #29, see
    /// [`crate::background_merge`]).
    pub(crate) bg: std::sync::Arc<crate::background_merge::Scheduler>,
    /// The vector sidecar (semantic search, issue #18), opened on first use.
    pub(crate) vectors: crate::semantic::VectorCache,
}

impl Drop for LineIndex {
    /// Stops a running background sidecar build at its next check (never
    /// waits for it).
    fn drop(&mut self) {
        self.fm_repair.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        // a background merge stops at its next check (a plain merge already
        // running finishes; it publishes atomically)
        self.bg.cancel_detached();
    }
}

impl LineIndex {
    /// Creates a fresh index in `dir`, wiping whatever it held.
    pub fn create(dir: &Path, options: IndexOptions) -> Result<LineIndex> {
        if dir.exists() {
            for entry in fs::read_dir(dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    fs::remove_dir_all(&path)?;
                } else {
                    fs::remove_file(&path)?;
                }
            }
        } else {
            fs::create_dir_all(dir)?;
        }
        let (schema, fields) = line_schema(options);
        // Doc order = catalog order: every segment is sorted by `position`
        // when it is written and the merge keeps that order, so the merged
        // single segment needs no sort at search time (see
        // [`LineIndex::catalog_ordered`]). Recorded in meta.json's settings.
        let settings = IndexSettings {
            sort_by_field: Some(IndexSortByField { field: "position".to_string(), order: Order::Asc }),
            ..Default::default()
        };
        let index = Index::create(MmapDirectory::open(dir)?, schema, settings)?;
        hebrew_fts::register_hebrew_tokenizer(index.tokenizers());
        fs::write(
            dir.join(OPTIONS_FILE),
            serde_json::to_string(&options).map_err(|e| crate::Error::Unsupported(e.to_string()))?,
        )?;
        Self::wrap(index, fields, options, dir)
    }

    fn wrap(index: Index, fields: LineFields, options: IndexOptions, dir: &Path) -> Result<LineIndex> {
        let reader = SharedReader {
            reader: index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?,
            pending: Default::default(),
            #[cfg(test)]
            test_fail_reload: Default::default(),
        };
        Ok(LineIndex {
            index,
            reader,
            fields,
            options,
            dir: dir.to_path_buf(),
            fm: Default::default(),
            // unit tests opt in (`set_fm_auto_repair`), so a test that
            // checks a missing sidecar is not raced by a background build
            fm_repair: std::sync::Arc::new(crate::fm_sidecar::Repair::new(!cfg!(test))),
            #[cfg(test)]
            test_only_disable_fm: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            test_only_text_decision: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            test_only_no_scan: std::sync::atomic::AtomicBool::new(false),
            bg: crate::background_merge::scheduler_for(dir),
            vectors: Default::default(),
        })
    }

    /// Opens an existing index.
    pub fn open(dir: &Path) -> Result<LineIndex> {
        let options = read_options(dir)?;
        let index = Index::open(MmapDirectory::open(dir)?)?;
        // a sidecar build stopped mid-way (the process ended) left its temp file
        crate::fm_sidecar::remove_leftover_temps(dir);
        hebrew_fts::register_hebrew_tokenizer(index.tokenizers());
        check_schema(&index.schema(), options)?;
        let (_, mut fields) = line_schema(options);
        // a marks index built before the line flag has no `marked` field
        fields.marked = index.schema().get_field("marked").ok();
        // an index built before the per-line fields has none of them
        let schema = index.schema();
        fields.line_first = schema.get_field("line_first").ok();
        fields.line_last = schema.get_field("line_last").ok();
        fields.line_flags = schema.get_field("line_flags").ok();
        fields.section_id = schema.get_field("section_id").ok();
        let index = Self::wrap(index, fields, options, dir)?;
        // issue #29: an update's merge that did not finish (the process
        // ended) restarts in the background (unit tests start it explicitly)
        // (best effort: a read-only or locked index still opens for search)
        if !cfg!(test) {
            let _ = index.resume_background_merge();
        }
        Ok(index)
    }

    /// Builds the FM-index sidecar over the term dictionary (FtsLib's
    /// `BuildFmSidecar`). It serves leading-`*` wildcards and fuzzy words
    /// without walking the whole dictionary. Needs a single segment: call
    /// after [`LineWriter::merge_all`]. Returns false when the index has
    /// several segments or no terms (nothing built; searches still work
    /// through the FST walk). Err when it cannot be built (reported, never
    /// a silent skip).
    pub fn build_fm_sidecar(&self) -> Result<bool> {
        // the single-threaded tail of a build: normal priority even when the
        // calling thread opted into background mode (throttled, it drags)
        let _normal = tantivy::indexer::NormalPrioritySection::enter();
        // another LineIndex over this dir may have merged: see the last commit
        self.reader.reload()?;
        // one build at a time (a background rebuild may be running)
        let _build = self.fm_repair.build_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.fm.clear();
        let built = crate::fm_sidecar::build(&self.reader.searcher(), self.fields.text, &self.dir);
        // a search during the build may have cached the file being replaced
        self.fm.clear();
        // an explicit build is a new chance for the automatic one too
        self.fm_repair.reset();
        built
    }

    /// True when a sidecar matching the current single segment exists.
    pub fn has_fm_sidecar(&self) -> bool {
        self.fm.get(&self.dir, &self.reader.searcher(), self.fields.text).is_some()
    }

    /// Why the index has no usable sidecar although it could, or None when
    /// it is usable, or absent with nothing tried. One of:
    /// - `"rebuilding: <reason>"`: the engine's background rebuild is running
    ///   (searches use the walk until it is published);
    /// - the file's problem (corrupt, truncated, an unknown format, built
    ///   over another dictionary), followed by `"; FM sidecar rebuild
    ///   failed: ..."` when the automatic rebuild failed;
    /// - `"FM sidecar rebuild failed: ..."` alone when no file exists and the
    ///   rebuild failed (e.g. a read-only index directory).
    ///
    /// A search that met an unusable file carries
    /// [`NOTICE_FM_SIDECAR_UNUSABLE`](crate::NOTICE_FM_SIDECAR_UNUSABLE).
    pub fn fm_sidecar_problem(&self) -> Option<String> {
        let searcher = self.reader.searcher();
        let file = match self.fm.lookup(&self.dir, &searcher, self.fields.text) {
            crate::fm_sidecar::FmLookup::Ready(_) => return None,
            crate::fm_sidecar::FmLookup::Unusable(reason) => Some(reason),
            crate::fm_sidecar::FmLookup::Missing => None,
        };
        if self.fm_repair.running() {
            return Some(format!("rebuilding: {}", file.as_deref().unwrap_or("no sidecar")));
        }
        let failed = match searcher.segment_readers() {
            [segment] => self.fm_repair.failure(segment.segment_id()),
            _ => None,
        };
        match (file, failed) {
            (Some(file), Some(failed)) => Some(format!("{file}; {failed}")),
            (file, failed) => file.or(failed),
        }
    }

    /// Turns the engine's automatic FM sidecar rebuild on or off (default
    /// on): when on, a search that finds the sidecar of a one-segment index
    /// missing or unusable starts one background rebuild (see
    /// [`fm_sidecar_problem`](Self::fm_sidecar_problem) for its state).
    pub fn set_fm_auto_repair(&self, on: bool) {
        self.fm_repair.set_auto(on);
    }

    /// True while the engine's background sidecar rebuild runs.
    pub fn fm_sidecar_rebuilding(&self) -> bool {
        self.fm_repair.running()
    }

    /// True when the docs of `searcher` are in ascending catalog `position`
    /// order: the index is sorted by position (meta.json settings) and has a
    /// single segment. An update (delete + add) adds a segment, which turns
    /// this off until the next [`LineWriter::merge_all`]; deletes alone keep
    /// it. An empty index (no segment) is not. Older indexes (no sort
    /// setting) are never catalog-ordered.
    pub fn catalog_ordered(&self, searcher: &Searcher) -> bool {
        let sorted = self.index.settings().sort_by_field.as_ref().is_some_and(|s| {
            s.field == "position" && s.order == Order::Asc
        });
        sorted && searcher.segment_readers().len() == 1
    }

    /// The first doc of `segment` whose position is after `after`, when the
    /// index is sorted by position (every segment then is); 0 otherwise
    /// (the caller filters by position anyway). Deleted docs keep their
    /// position, so the column stays sorted.
    pub(crate) fn first_doc_after(&self, segment: &tantivy::SegmentReader, after: u64) -> Result<tantivy::DocId> {
        let sorted = self
            .index
            .settings()
            .sort_by_field
            .as_ref()
            .is_some_and(|s| s.field == "position" && s.order == Order::Asc);
        if !sorted {
            return Ok(0);
        }
        let column = segment.fast_fields().u64("position")?;
        let (mut lo, mut hi) = (0u32, segment.max_doc());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if column.first(mid).unwrap_or(0) <= after {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// True when `dir` holds an index.
    pub fn exists(dir: &Path) -> bool {
        dir.join("meta.json").exists()
    }

    /// The options the index was created with.
    pub fn options(&self) -> IndexOptions {
        self.options
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The host payload saved with the last commit (e.g. a resume watermark
    /// and provenance stamp). Committed atomically with the documents, so it
    /// always describes exactly what the index holds.
    pub fn committed_payload(&self) -> Result<Option<String>> {
        Ok(self.index.load_metas()?.payload)
    }

    /// The index's shared reader (for tools and statistics; see the
    /// `reader` field: reloaded after this engine's own writes).
    pub fn reader(&self) -> Result<tantivy::IndexReader> {
        let _ = self.reader.searcher(); // retries a pending reload first
        Ok(self.reader.reader.clone())
    }

    /// True when a write through this index's writer committed but the
    /// reload after it failed: searches still see the previous snapshot and
    /// the next search (or count) retries the reload.
    pub fn reload_pending(&self) -> bool {
        self.reader.pending.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Reloads the shared reader to the last commit on disk. Needed only
    /// when another process (or another `LineIndex` over the same dir) wrote
    /// the index; this engine's own writer reloads it itself. Searches
    /// already running keep their snapshot.
    pub fn reload(&self) -> Result<()> {
        self.reader.reload()
    }

    /// The text field.
    pub fn text_field(&self) -> Field {
        self.fields.text
    }

    /// True when the index has the per-line fields (word count, line flags,
    /// section id) that cross-line matching and the same-section scope read.
    pub fn has_line_data(&self) -> bool {
        self.fields.line_first.is_some()
            && self.fields.line_last.is_some()
            && self.fields.line_flags.is_some()
            && self.fields.section_id.is_some()
    }

    /// Number of lines in the index (as of the last commit).
    pub fn num_lines(&self) -> Result<u64> {
        Ok(self.reader.searcher().num_docs())
    }

    /// Starts a writer. `heap_bytes` is the total budget; tantivy needs at
    /// least 15 MB per thread, so threads are capped by it.
    pub fn writer(&self, heap_bytes: usize, num_threads: usize) -> Result<LineWriter> {
        self.writer_with(heap_bytes, num_threads, false)
    }

    /// [`Self::writer`] with `low_priority`: tantivy's indexing and merge
    /// threads run below normal OS priority (a background build that keeps
    /// the machine responsive; best effort per OS).
    pub fn writer_with(
        &self,
        heap_bytes: usize,
        num_threads: usize,
        low_priority: bool,
    ) -> Result<LineWriter> {
        // one writer per index: a running background merge holds it, so it
        // stops first (a plain merge already running is waited for)
        self.bg.stop();
        self.parts().writer_with(heap_bytes, num_threads, low_priority)
    }

    /// What a writer needs from the index, cloneable to another thread (the
    /// background merge).
    pub(crate) fn parts(&self) -> WriterParts {
        WriterParts {
            index: self.index.clone(),
            fields: self.fields,
            reader: self.reader.clone(),
            fm_repair: self.fm_repair.clone(),
            fm: self.fm.clone(),
            dir: self.dir.clone(),
        }
    }

    /// Starts a writer in the auto mode (issue #19, see [`crate::resources`]):
    /// one thread per core in the OS background mode, and a memory budget
    /// that follows the machine (large while idle, the minimum under memory
    /// pressure or while the user is active; lowering it flushes the
    /// workers' segments at their next batch). A running background merge
    /// stops first, as in [`Self::writer_with`].
    pub fn writer_auto(&self) -> Result<LineWriter> {
        self.bg.stop();
        self.parts().writer_auto()
    }
}

/// The parts of a [`LineIndex`] a writer uses.
#[derive(Clone)]
pub(crate) struct WriterParts {
    index: Index,
    fields: LineFields,
    reader: SharedReader,
    fm_repair: std::sync::Arc<crate::fm_sidecar::Repair>,
    fm: std::sync::Arc<crate::fm_sidecar::FmCache>,
    dir: PathBuf,
}

impl WriterParts {
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn num_segments(&self) -> Result<usize> {
        Ok(self.index.searchable_segment_ids()?.len())
    }

    /// [`LineIndex::build_fm_sidecar`] for the background merge (same lock
    /// and cache rules, at the calling thread's priority).
    pub(crate) fn build_fm_sidecar(&self) -> Result<bool> {
        self.reader.reload()?;
        let _build = self.fm_repair.build_lock.lock().unwrap_or_else(|e| e.into_inner());
        self.fm.clear();
        let built = crate::fm_sidecar::build(&self.reader.searcher(), self.fields.text, &self.dir);
        self.fm.clear();
        self.fm_repair.reset();
        built
    }

    pub(crate) fn writer_with(
        &self,
        heap_bytes: usize,
        num_threads: usize,
        low_priority: bool,
    ) -> Result<LineWriter> {
        const MIN_HEAP_PER_THREAD: usize = 15_000_000;
        let heap = heap_bytes.max(MIN_HEAP_PER_THREAD);
        let threads = num_threads.max(1).min(heap / MIN_HEAP_PER_THREAD).max(1);
        let options = IndexWriterOptions::builder()
            .num_worker_threads(threads)
            .memory_budget_per_thread(heap / threads)
            .low_priority_threads(low_priority)
            .build();
        let mut writer = LineWriter {
            writer: self.index.writer_with_options(options)?,
            fields: self.fields,
            threads,
            watcher: None,
            reader: self.reader.clone(),
            _repair: self.fm_repair.writer_guard(),
            fm: self.fm.clone(),
            dir: self.dir.clone(),
        };
        // files a previous merge could not delete (still mapped then)
        writer.collect_garbage();
        Ok(writer)
    }

    fn writer_auto(&self) -> Result<LineWriter> {
        use crate::resources;
        let threads = resources::auto_threads();
        let options = IndexWriterOptions::builder()
            .num_worker_threads(threads)
            .memory_budget_per_thread(resources::idle_budget_per_thread(threads))
            .low_priority_threads(true)
            .build();
        let writer = self.index.writer_with_options(options)?;
        let watcher = resources::Watcher::start(writer.memory_budget(), threads);
        let mut writer = LineWriter {
            writer,
            fields: self.fields,
            threads,
            watcher: Some(watcher),
            reader: self.reader.clone(),
            _repair: self.fm_repair.writer_guard(),
            fm: self.fm.clone(),
            dir: self.dir.clone(),
        };
        writer.collect_garbage();
        Ok(writer)
    }
}

/// Adds, deletes and commits lines.
pub struct LineWriter {
    writer: IndexWriter,
    fields: LineFields,
    threads: usize,
    /// The auto mode's resource watcher (stops when the writer drops).
    watcher: Option<crate::resources::Watcher>,
    /// The index's shared reader, reloaded after every commit, rollback
    /// and merge (issue #22).
    reader: SharedReader,
    /// Holds off the automatic FM sidecar rebuild while the writer is open.
    _repair: crate::fm_sidecar::WriterGuard,
    /// The index's sidecar cache and directory, for [`Self::collect_garbage`].
    fm: std::sync::Arc<crate::fm_sidecar::FmCache>,
    dir: PathBuf,
}

/// The index's one reader (issue #22), shared by the index and its writers.
#[derive(Clone)]
pub(crate) struct SharedReader {
    reader: IndexReader,
    /// Set when a write committed but the reload after it failed; the next
    /// `searcher()` retries the reload.
    pending: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// TEST ONLY: makes the reload after a write fail.
    #[cfg(test)]
    pub(crate) test_fail_reload: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SharedReader {
    /// The current snapshot. When a reload after a write is pending it is
    /// retried first; if it fails again the previous snapshot is returned
    /// (never an error: the data it shows is a committed state).
    pub(crate) fn searcher(&self) -> Searcher {
        use std::sync::atomic::Ordering;
        // clear BEFORE reloading: a write that fails its reload meanwhile
        // sets the flag again and is not lost
        if self.pending.swap(false, Ordering::AcqRel) && self.try_reload().is_err() {
            self.pending.store(true, Ordering::Release);
        }
        self.reader.searcher()
    }

    /// Explicit reload (errors are returned; clears a pending retry).
    pub(crate) fn reload(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        let was = self.pending.swap(false, Ordering::AcqRel);
        let result = self.try_reload();
        if result.is_err() && was {
            self.pending.store(true, Ordering::Release);
        }
        result
    }

    /// After a write that is already durable: reload, and on failure mark it
    /// pending instead of failing the write.
    fn after_write(&self) {
        let ok = self.try_reload().is_ok();
        self.pending.store(!ok, std::sync::atomic::Ordering::Release);
    }

    fn try_reload(&self) -> Result<()> {
        #[cfg(test)]
        if self.test_fail_reload.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::Error::Unsupported("test: reload fails".into()));
        }
        Ok(self.reader.reload()?)
    }
}

impl LineWriter {
    /// Queues a line. Not visible until `commit`.
    pub fn add(&mut self, line: &LineDoc) -> Result<()> {
        let f = self.fields;
        let mut document = doc!(
            f.line_id => line.line_id,
            f.position => line.position,
            f.book_id => line.book_id,
            f.text => line.text.as_str(),
        );
        if let Some(marked) = f.marked {
            if hebrew_fts::chars::contains_attached_marks(&line.text) {
                document.add_u64(marked, MARKED_LINE);
            }
        }
        if let (Some(first), Some(last), Some(flags)) = (f.line_first, f.line_last, f.line_flags) {
            // the content words a phrase may continue from / into (Otzaria's
            // content_range: line-edge enumerators skipped)
            let edges = hebrew_fts::line_edges::line_edges(&line.text);
            document.add_u64(first, edges.first.map_or(0, |p| p as u64 + 1));
            document.add_u64(last, edges.last.map_or(0, |p| p as u64 + 1));
            let mut bits = if line.no_cross { LINE_FLAG_NO_CROSS } else { 0 };
            if edges.first.is_some_and(|p| p > 0) {
                bits |= LINE_FLAG_LEADING_MARKER;
            }
            if edges.last.is_some_and(|p| p + 1 < edges.words) {
                bits |= LINE_FLAG_TRAILING_MARKER;
            }
            if edges.long_word {
                bits |= LINE_FLAG_LONG_WORD;
            }
            document.add_u64(flags, bits);
        }
        if let (Some(field), Some(section)) = (f.section_id, line.section_id) {
            document.add_u64(field, section);
        }
        self.writer.add_document(document)?;
        Ok(())
    }

    /// Queues the deletion of every doc with this line id. An update is a
    /// delete followed by an add of the new version.
    pub fn delete(&mut self, line_id: u64) {
        self.writer
            .delete_term(Term::from_field_u64(self.fields.line_id, line_id));
    }

    /// Queues the deletion of every line of a book (issue #29, a per-book
    /// update: delete the book, add its new lines, then ONE commit, never a
    /// commit in between; issue #32). Reads the `book_id` fast field (no
    /// extra index field). Lines added after this call in the same batch
    /// are kept (a delete applies to the lines added before it).
    pub fn delete_book(&mut self, book_id: u64) -> Result<()> {
        use std::ops::Bound;
        let term = Term::from_field_u64(self.fields.book_id, book_id);
        let query = tantivy::query::RangeQuery::new(Bound::Included(term.clone()), Bound::Included(term));
        self.writer.delete_query(Box::new(query))?;
        Ok(())
    }

    /// Commits everything queued, with an optional host payload stored
    /// atomically with it (see [`LineIndex::committed_payload`]).
    ///
    /// Ok means the commit is durable. The index's shared reader is reloaded
    /// right after; if that reload fails the commit still returns Ok, searches
    /// keep the previous snapshot, [`LineIndex::reload_pending`] turns true and
    /// the next search or count retries the reload.
    pub fn commit(&mut self, payload: Option<&str>) -> Result<()> {
        // a commit is waited for (readers reload after it): normal priority
        // even when the calling thread opted into background mode
        let _normal = tantivy::indexer::NormalPrioritySection::enter();
        let mut prepared = self.writer.prepare_commit()?;
        if let Some(p) = payload {
            prepared.set_payload(p);
        }
        prepared.commit()?;
        self.reader.after_write();
        self.collect_garbage();
        Ok(())
    }

    /// The current memory budget per indexing thread, in bytes (it moves in
    /// the auto mode).
    pub fn memory_budget_per_thread(&self) -> usize {
        self.writer.memory_budget().per_thread()
    }

    /// The number of indexing threads.
    pub fn num_threads(&self) -> usize {
        self.threads
    }

    /// Forces the auto mode's load (for tests and benchmarks; `None` returns
    /// to the OS signals). No effect on a fixed writer.
    pub fn simulate_load(&self, load: Option<crate::resources::Load>) {
        if let Some(w) = &self.watcher {
            w.simulate(load);
        }
    }

    /// Drops everything queued since the last commit (the shared reader is
    /// reloaded as after [`Self::commit`], same failure rule).
    pub fn rollback(&mut self) -> Result<()> {
        self.writer.rollback()?;
        self.reader.after_write();
        self.collect_garbage();
        Ok(())
    }

    /// Deletes the files no snapshot uses any more (issue #29). tantivy
    /// collects garbage when a commit or merge lands, but our shared reader
    /// still held the old segments then (it reloads right AFTER), so they
    /// were kept, and on Windows a file that is still memory-mapped cannot
    /// be deleted at all. So, after every write and when a writer opens:
    /// drop the cached sidecar of a dead segment, run tantivy's garbage
    /// collection (segments still held by a running search's searcher stay
    /// live; a file that fails to delete stays managed and is retried at the
    /// next write or writer open), and delete the sidecars of dead segments.
    /// Best effort: it never fails the write.
    fn collect_garbage(&mut self) {
        let live: Vec<_> = self.reader.reader.searcher().segment_readers().iter().map(|s| s.segment_id()).collect();
        self.fm.forget_unless(&live);
        let _ = self.writer.garbage_collect_files().wait();
        // sidecars: keep the current snapshot's and every committed segment's
        let mut keep = live;
        if let Ok(ids) = self.writer.index().searchable_segment_ids() {
            keep.extend(ids);
        }
        crate::fm_sidecar::remove_dead_sidecars(&self.dir, &keep);
        // temps of interrupted sidecar builds (same locked / young rule as open)
        crate::fm_sidecar::remove_leftover_temps(&self.dir);
    }

    /// Re-sorts the index into a new catalog order and CLOSES the writer
    /// (optional; a build never needs it). `book_order` = every indexed book
    /// id in the new catalog order (books with no lines may appear; indexed
    /// books it does not list go last, in their old order; a book listed
    /// twice is an error).
    /// Each book keeps its lines in their old order and spacing; the books
    /// are packed in the new order from position 0 (see [`crate::resort`]).
    ///
    /// Pending adds and deletes are committed first (keeping the payload).
    /// Then ONE reordering merge writes every line in the new order into a
    /// single segment with `position` rewritten, so the index stays
    /// catalog-ordered (no sort at search time). It publishes atomically:
    /// searches see the old order until the new segment is complete, then
    /// the new one; the order stamp changes, so a resume from a page of the
    /// old order fails with `StaleResume`. The old segment's files and
    /// sidecar are removed and the FM-index sidecar is rebuilt.
    ///
    /// `cancel` stops it before the new segment is published
    /// ([`crate::Error::Cancelled`]; the index keeps its old order and files); once
    /// published only the sidecar rebuild remains (not cancellable; the
    /// automatic repair also covers it). `progress` is called from the merge
    /// thread. Runs at background priority when the writer does
    /// ([`LineIndex::writer_with`] low_priority, [`LineIndex::writer_auto`]).
    /// Disk: the old and the new segment exist together until the publish
    /// (about twice the index size at the peak).
    pub fn resort_and_close(
        mut self,
        book_order: &[u64],
        cancel: Option<crate::CancelToken>,
        progress: Option<crate::resort::ResortProgressFn>,
    ) -> Result<u64> {
        use crate::resort::ResortStage;
        let report = |stage: ResortStage, done: u64, total: u64| {
            if let Some(p) = &progress {
                p(stage, done, total);
            }
        };
        crate::search::CancelToken::check(cancel.as_ref())?;
        let payload = self.writer.index().load_metas()?.payload;
        self.commit(payload.as_deref())?;
        self.writer.set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
        let flag = cancel.clone().unwrap_or_default();
        let book_order: std::sync::Arc<Vec<u64>> = std::sync::Arc::new(book_order.to_vec());
        let plan_error: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let mut attempt = 0;
        let lines = loop {
            let ids = self.writer.index().searchable_segment_ids()?;
            if ids.is_empty() {
                break 0;
            }
            let order = book_order.clone();
            let failed = plan_error.clone();
            let plan: tantivy::indexer::ReorderPlanFn = Box::new(move |readers| {
                crate::resort::plan(readers, &order).map_err(|e| {
                    *failed.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string());
                    tantivy::TantivyError::InvalidArgument(e.to_string())
                })
            });
            let mut control = tantivy::indexer::ReorderControl::new().with_cancel_flag(flag.flag());
            if let Some(p) = progress.clone() {
                control = control.on_progress(move |stage, done, total| p(stage.into(), done, total));
            }
            match self.writer.merge_reordered(&ids, plan, control).wait() {
                Ok(meta) => break meta.map_or(0, |m| m.num_docs() as u64),
                Err(_) if flag.is_cancelled() => {
                    self.reader.after_write();
                    self.collect_garbage();
                    return Err(crate::Error::Cancelled);
                }
                // the host's order is not usable (the plan said why), or the
                // plan broke tantivy's checks: no retry
                Err(e) if plan_error.lock().unwrap_or_else(|p| p.into_inner()).is_some()
                    || e.to_string().contains("reorder plan") =>
                {
                    let m = plan_error.lock().unwrap_or_else(|p| p.into_inner()).take();
                    return Err(crate::Error::Unsupported(m.unwrap_or_else(|| e.to_string())));
                }
                // a background merge that was already running finished first
                // (its segments are gone): let it land, retry on the new list
                Err(e) if attempt < 20 && e.to_string().contains("not all uncommitted or committed") => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => {
                    // the failed merge's partial files go now, not at the next writer open
                    self.reader.after_write();
                    self.collect_garbage();
                    return Err(e.into());
                }
            }
        };
        report(ResortStage::Publish, 0, 1);
        self.reader.after_write();
        self.collect_garbage();
        let reader = self.reader.clone();
        let (fm, dir, text) = (self.fm.clone(), self.dir.clone(), self.fields.text);
        let waited = self.writer.wait_merging_threads();
        reader.after_write();
        waited?;
        report(ResortStage::Publish, 1, 1);
        report(ResortStage::FmSidecar, 0, 1);
        {
            // the single-threaded tail: normal priority (as build_fm_sidecar)
            let _normal = tantivy::indexer::NormalPrioritySection::enter();
            fm.clear();
            crate::fm_sidecar::build(&reader.searcher(), text, &dir)?;
            fm.clear();
        }
        report(ResortStage::FmSidecar, 1, 1);
        report(ResortStage::Done, lines, lines);
        Ok(lines)
    }

    /// [`Self::merge_all`] at background priority (issue #29): a merge policy
    /// that merges every segment into one schedules the merge at the commit,
    /// so it runs on tantivy's merge thread in the background mode (this
    /// writer must be low priority; an explicit merge always runs at normal
    /// priority). Same memory as [`Self::merge_all`]. Not cancellable once
    /// started (it publishes atomically). The merged-away files are
    /// collected by the caller with a new writer (the merge lands while this
    /// one waits).
    pub(crate) fn merge_all_in_background(mut self) -> Result<()> {
        let payload = self.writer.index().load_metas()?.payload;
        self.writer.set_merge_policy(Box::new(MergeEverything));
        self.commit(payload.as_deref())?;
        let reader = self.reader.clone();
        let waited = self.writer.wait_merging_threads();
        reader.after_write();
        waited?;
        Ok(())
    }

    /// Merges all segments into one (for a shipped, read-mostly index) and
    /// waits for it.
    ///
    /// During a build tantivy's default merge policy keeps merging segments in
    /// the background, so a segment list read earlier can be stale by the
    /// time we merge ("segments that were merged could not be found"; a full
    /// build failed this way). So: no new background merges from here on, read
    /// the CURRENT committed segment ids from the index meta, and if a merge
    /// that was already running still beats us, re-read and retry.
    ///
    /// Pending adds and deletes are committed first (as Otzaria's optimize and
    /// FtsLib do), keeping the last committed payload, so nothing queued is
    /// lost when the writer closes.
    pub fn merge_all(mut self) -> Result<()> {
        let payload = self.writer.index().load_metas()?.payload;
        self.commit(payload.as_deref())?;
        self.writer
            .set_merge_policy(Box::new(tantivy::indexer::NoMergePolicy));
        let mut attempt = 0;
        loop {
            let ids = self.writer.index().searchable_segment_ids()?;
            if ids.len() <= 1 {
                break;
            }
            match self.writer.merge(&ids).wait() {
                Ok(_) => break,
                Err(e) if attempt < 20 => {
                    // a background merge finished first: let it land, retry
                    attempt += 1;
                    let _ = e;
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(e) => return Err(e.into()),
            }
        }
        // our merge has landed (waited above): reload, then delete the
        // merged-away files (issue #29) while we still hold the writer
        self.reader.after_write();
        self.collect_garbage();
        let reader = self.reader.clone();
        let waited = self.writer.wait_merging_threads();
        // a background merge that was still running may land while waiting:
        // reload anyway; its leftovers go at the next writer open
        reader.after_write();
        waited?;
        Ok(())
    }
}

/// Merges every segment into one (the background full merge, issue #29).
#[derive(Debug)]
struct MergeEverything;

impl tantivy::merge_policy::MergePolicy for MergeEverything {
    fn compute_merge_candidates(&self, segments: &[tantivy::SegmentMeta]) -> Vec<tantivy::merge_policy::MergeCandidate> {
        if segments.len() < 2 {
            return Vec::new();
        }
        vec![tantivy::merge_policy::MergeCandidate(segments.iter().map(|s| s.id()).collect())]
    }
}

/// The build options recorded next to the index. Strict: a missing or
/// unreadable file, malformed JSON, a missing or unknown key is an error,
/// never the defaults (an index opened with the wrong options would search
/// with the wrong tokenizer or rules).
fn read_options(dir: &Path) -> Result<IndexOptions> {
    let path = dir.join(OPTIONS_FILE);
    let text = fs::read_to_string(&path).map_err(|e| {
        crate::Error::Unsupported(format!("cannot read the index options {}: {e}", path.display()))
    })?;
    serde_json::from_str(&text)
        .map_err(|e| crate::Error::Unsupported(format!("malformed index options {}: {e}", path.display())))
}

/// The index's schema must be the one its recorded options build: same
/// `text` tokenizer and record option, and every field the options imply.
fn check_schema(schema: &Schema, options: IndexOptions) -> Result<()> {
    let bad = |what: String| Err(crate::Error::Unsupported(format!("index schema does not match its options {options:?}: {what}")));
    let Ok(text) = schema.get_field("text") else { return bad("no text field".into()) };
    let tantivy::schema::FieldType::Str(text_options) = schema.get_field_entry(text).field_type() else {
        return bad("text is not a text field".into());
    };
    let Some(indexing) = text_options.get_indexing_options() else { return bad("text is not indexed".into()) };
    let (expected, _) = line_schema(options);
    let want = expected.get_field_entry(expected.get_field("text").expect("text")).field_type();
    let tantivy::schema::FieldType::Str(want) = want else { unreachable!() };
    let want = want.get_indexing_options().expect("indexed");
    if indexing.tokenizer() != want.tokenizer() {
        return bad(format!("tokenizer {}", indexing.tokenizer()));
    }
    if indexing.index_option() != want.index_option() {
        return bad(format!("record option {:?}", indexing.index_option()));
    }
    for name in ["line_id", "position", "book_id"] {
        if schema.get_field(name).is_err() {
            return bad(format!("no {name} field"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(id: u64, text: &str) -> LineDoc {
        LineDoc {
            line_id: id,
            position: id,
            book_id: 1,
            text: text.into(),
            section_id: None,
            no_cross: false,
        }
    }

    fn hits(index: &LineIndex, q: &str) -> usize {
        let o = crate::SearchOptions { query: q.into(), ..Default::default() };
        index.search_options(&o).unwrap().hits.len()
    }

    /// Issue #31: one writer per index directory. A second writer, on the
    /// same handle or on another handle opened on the same directory, fails
    /// with tantivy's lockfile error (`LockFailure(LockBusy, ..)`); once the
    /// first writer is dropped a new one opens.
    #[test]
    fn second_writer_fails_with_lock_error() {
        // the threading contract (README "Threading"): an index is shared by
        // threads (concurrent searches and counts), a writer moves between them
        fn shared<T: Send + Sync>() {}
        fn movable<T: Send>() {}
        shared::<LineIndex>();
        movable::<LineWriter>();
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let other = LineIndex::open(dir.path()).unwrap();
        let first = index.writer(50_000_000, 1).unwrap();
        for second in [index.writer(50_000_000, 1), other.writer(50_000_000, 1)] {
            let error = second.err().expect("second writer must fail");
            assert!(
                matches!(&error, crate::Error::Tantivy(tantivy::TantivyError::LockFailure(_, _))),
                "{error}"
            );
            assert!(error.to_string().contains("LockBusy"), "{error}");
        }
        drop(first);
        let mut w = other.writer(50_000_000, 1).unwrap();
        w.add(&doc(1, "alpha")).unwrap();
        w.commit(None).unwrap();
        // the other handle sees it after an explicit reload
        assert_eq!(hits(&index, "alpha"), 0);
        index.reload().unwrap();
        assert_eq!(hits(&index, "alpha"), 1);
    }

    /// An index with no segment (created, nothing committed yet) returns no
    /// hits instead of panicking (the catalog-order scan read segment 0),
    /// both flavours, blocking and streaming.
    #[test]
    fn search_on_an_empty_index_returns_no_hits() {
        for positions in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let options = IndexOptions { positions, ..IndexOptions::default() };
            let index = LineIndex::create(dir.path(), options).unwrap();
            assert_eq!(hits(&index, "alpha"), 0, "positions={positions}");
            let o = crate::SearchOptions { query: "alpha".into(), ..Default::default() };
            let mut streamed = index.search_options_streaming(&o, None).unwrap();
            assert!(streamed.pull(usize::MAX, None).unwrap());
            assert!(streamed.hits.is_empty(), "positions={positions}");
        }
    }

    /// Issue #22: one reader for the index's life, reloaded by our writer.
    #[test]
    fn shared_reader_reloads_after_our_writes() {
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        w.add(&doc(1, "alpha beta")).unwrap();
        w.commit(None).unwrap();
        // reused: no commit, same generation across searches and counts
        let g = index.reader.searcher().generation().generation_id();
        assert_eq!(hits(&index, "alpha"), 1);
        assert_eq!(index.num_lines().unwrap(), 1);
        assert_eq!(index.reader.searcher().generation().generation_id(), g);
        // a search started before a commit keeps its snapshot
        let before = index.search_options(&crate::SearchOptions { query: "alpha".into(), ..Default::default() }).unwrap();
        let snapshot = index.reader.searcher();
        w.add(&doc(2, "alpha gamma")).unwrap();
        w.commit(None).unwrap();
        assert_eq!(before.hits.len(), 1);
        assert_eq!(snapshot.num_docs(), 1);
        // the next search and the count see the commit
        assert_eq!(hits(&index, "alpha"), 2);
        assert_eq!(index.num_lines().unwrap(), 2);
        // a delete, a rollback and the merge reload too
        w.delete(1);
        w.commit(None).unwrap();
        assert_eq!(index.num_lines().unwrap(), 1);
        w.add(&doc(3, "alpha")).unwrap();
        w.rollback().unwrap();
        assert_eq!(hits(&index, "alpha"), 1);
        w.add(&doc(4, "alpha delta")).unwrap();
        w.merge_all().unwrap();
        assert_eq!(hits(&index, "alpha"), 2);
        assert_eq!(index.reader.searcher().segment_readers().len(), 1);
        assert!(index.build_fm_sidecar().unwrap());
        assert!(index.has_fm_sidecar());
    }

    /// Bytes in `dir` and the segment ids that own files there (`<uuid>.*`,
    /// sidecars included).
    fn dir_state(dir: &Path) -> (u64, std::collections::BTreeSet<String>) {
        let mut bytes = 0;
        let mut ids = std::collections::BTreeSet::new();
        for e in fs::read_dir(dir).unwrap().flatten() {
            bytes += e.metadata().map_or(0, |m| m.len());
            let name = e.file_name().to_string_lossy().to_string();
            let stem = name.split('.').next().unwrap_or("");
            if stem.len() == 32 && stem.chars().all(|c| c.is_ascii_hexdigit()) {
                ids.insert(stem.to_string());
            }
        }
        (bytes, ids)
    }

    fn uuid(id: tantivy::index::SegmentId) -> String {
        id.uuid_string()
    }

    /// Issue #29: a merge left the old segments' files (and the old
    /// segment's FM sidecar) on disk for good: the shared reader still held
    /// them when tantivy collected garbage, and on Windows a mapped file
    /// cannot be deleted. Now the merge deletes them once no snapshot uses
    /// them; a search still holding an old snapshot keeps them until the
    /// next write or writer open after it ends.
    #[test]
    fn merge_deletes_the_old_segment_files() {
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let text: String = (0..400).map(|i| format!("word{i} alpha{} ", i % 37)).collect();
        // one merged segment with its sidecar, mapped by a search
        let mut w = index.writer(50_000_000, 1).unwrap();
        for i in 0..300 {
            w.add(&doc(i, &text)).unwrap();
            if i % 100 == 99 {
                w.commit(None).unwrap();
            }
        }
        w.merge_all().unwrap();
        assert!(index.build_fm_sidecar().unwrap());
        assert!(index.has_fm_sidecar()); // the sidecar is now mapped
        let old = uuid(index.reader.searcher().segment_readers()[0].segment_id());
        assert_eq!(hits(&index, "*lpha3"), 300);
        // more lines, then a merge while a search holds the old snapshot
        let mut w = index.writer(50_000_000, 1).unwrap();
        for i in 300..400 {
            w.add(&doc(i, &text)).unwrap();
        }
        w.commit(None).unwrap();
        let held = index.reader.searcher();
        let (before, _) = dir_state(dir.path());
        w.merge_all().unwrap();
        let (peak, ids) = dir_state(dir.path());
        let new = uuid(index.reader.searcher().segment_readers()[0].segment_id());
        // a held snapshot keeps working: its files are kept while it is
        // live (tantivy's inventory), or, once deleted, stay readable
        // through their open mappings (deleted while mapped on every OS)
        let count = held.search(&tantivy::query::AllQuery, &tantivy::collector::Count).unwrap();
        assert_eq!(count, 400);
        assert!(ids.contains(&new));
        drop(held);
        // the next writer open collects what the held search kept
        drop(index.writer(50_000_000, 1).unwrap());
        assert!(index.build_fm_sidecar().unwrap());
        let (after, ids) = dir_state(dir.path());
        assert!(!ids.contains(&old), "the old segment and its sidecar are gone");
        assert_eq!(ids, [new.clone()].into_iter().collect(), "only the merged segment remains");
        eprintln!("issue #29 fixture: before merge {before} B, after merge {peak} B, after cleanup {after} B");
        // without a held search the merge itself cleans up
        let mut w = index.writer(50_000_000, 1).unwrap();
        w.add(&doc(500, "alpha")).unwrap();
        w.commit(None).unwrap();
        w.merge_all().unwrap();
        let (_, ids) = dir_state(dir.path());
        let newest = uuid(index.reader.searcher().segment_readers()[0].segment_id());
        assert_eq!(ids, [newest].into_iter().collect(), "merge_all deletes the merged-away files and sidecar");
        // and a reopen sees a clean index
        drop(index);
        let index = LineIndex::open(dir.path()).unwrap();
        assert_eq!(hits(&index, "alpha"), 1);
    }

    /// Issue #22: a failed reload after a durable commit does not fail the
    /// commit; the old snapshot stays and the next search retries.
    #[test]
    fn failed_reload_after_commit_is_retried() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        w.add(&doc(1, "alpha")).unwrap();
        w.commit(None).unwrap();
        index.reader.test_fail_reload.store(true, Ordering::Release);
        w.add(&doc(2, "alpha")).unwrap();
        w.commit(Some("p2")).unwrap(); // durable: Ok although the reload failed
        assert!(index.reload_pending());
        assert_eq!(index.num_lines().unwrap(), 1); // previous snapshot, no error
        assert_eq!(hits(&index, "alpha"), 1);
        assert_eq!(index.committed_payload().unwrap().as_deref(), Some("p2"));
        assert!(index.reload().is_err()); // the explicit reload reports it
        assert!(index.reload_pending()); // and the retry stays armed
        w.add(&doc(3, "alpha")).unwrap();
        w.rollback().unwrap(); // the rollback path follows the same rule
        assert!(index.reload_pending());
        index.reader.test_fail_reload.store(false, Ordering::Release);
        assert_eq!(hits(&index, "alpha"), 2); // retried and caught up
        assert!(!index.reload_pending());
        assert_eq!(index.num_lines().unwrap(), 2);
    }

    /// Issue #22: a write through ANOTHER handle is seen after `reload`.
    #[test]
    fn reload_sees_another_handles_commit() {
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let other = LineIndex::open(dir.path()).unwrap();
        let mut w = other.writer(50_000_000, 1).unwrap();
        w.add(&doc(1, "alpha")).unwrap();
        w.commit(None).unwrap();
        assert_eq!(other.num_lines().unwrap(), 1);
        assert_eq!(index.num_lines().unwrap(), 0); // no watcher
        index.reload().unwrap();
        assert_eq!(index.num_lines().unwrap(), 1);
        assert_eq!(hits(&index, "alpha"), 1);
    }

    /// Issue #7 A11: merge_all commits pending adds and deletes first and
    /// keeps the last committed payload.
    #[test]
    fn merge_all_commits_pending_changes() {
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        w.add(&doc(1, "alpha beta")).unwrap();
        w.add(&doc(2, "gamma")).unwrap();
        w.commit(Some("p1")).unwrap();
        w.add(&doc(3, "delta")).unwrap();
        w.delete(2);
        w.merge_all().unwrap(); // no commit before it
        drop(index);
        let index = LineIndex::open(dir.path()).unwrap();
        assert_eq!(index.num_lines().unwrap(), 2);
        assert_eq!(index.committed_payload().unwrap().as_deref(), Some("p1"));
    }

    /// Issue #19: the auto writer indexes, and a pressure signal drops its
    /// budget to the minimum (the flush itself is tantivy's
    /// `test_lowering_the_memory_budget_flushes_the_segment`).
    #[test]
    fn auto_writer_follows_the_load() {
        use crate::resources::{Load, MIN_BUDGET_PER_THREAD};
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer_auto().unwrap();
        assert!(w.num_threads() >= 1);
        w.simulate_load(Some(Load::Idle));
        assert!(w.memory_budget_per_thread() >= MIN_BUDGET_PER_THREAD);
        w.add(&doc(1, "alpha beta")).unwrap();
        w.simulate_load(Some(Load::Pressure));
        assert_eq!(w.memory_budget_per_thread(), MIN_BUDGET_PER_THREAD);
        w.add(&doc(2, "gamma")).unwrap();
        w.commit(None).unwrap();
        w.merge_all().unwrap();
        assert_eq!(index.num_lines().unwrap(), 2);
    }
}
