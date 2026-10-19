//! The line index: tantivy schema for seforim lines, the builder (full build
//! from a seforim DB, resumable, batched commits) and line updates.
//!
//! One doc per line. The doc stores no text: content is fetched from the DB
//! at result time, as FtsLib does. It stores the line's PLACEMENT as fast
//! fields, and its catalog `position` (FtsLib's FTS rowid): results are
//! returned in that order.

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tantivy::collector::DocSetCollector;
use tantivy::directory::MmapDirectory;
use tantivy::query::TermQuery;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED,
};
use tantivy::{doc, Index, IndexWriter, Term};

use crate::corpus::SeforimDbCorpus;
use crate::error::{Error, Result};
use crate::fingerprint;

/// Bumped when the index pipeline changes shape; part of the provenance
/// stamp, so an older index rebuilds.
pub const INDEX_FORMAT_VERSION: &str = "tantivy1";

/// Build state / provenance file inside the index directory.
pub const STATE_FILE_NAME: &str = "build_state.json";

/// Field handles of the line schema.
#[derive(Debug, Clone, Copy)]
pub struct LineFields {
    /// seforim `line.id`. Indexed (for deletes/lookups) and fast.
    pub line_id: Field,
    /// Catalog position: the line's rank in the app's library order.
    pub position: Field,
    pub book_id: Field,
    pub line_index: Field,
    pub toc_entry_id: Field,
    /// The line text, tokenized with the `hebrew` tokenizer, with positions.
    pub text: Field,
}

/// Builds the schema.
pub fn line_schema() -> (Schema, LineFields) {
    let mut b = Schema::builder();
    let line_id = b.add_u64_field("line_id", INDEXED | FAST);
    let position = b.add_u64_field("position", FAST);
    let book_id = b.add_u64_field("book_id", FAST);
    let line_index = b.add_u64_field("line_index", FAST);
    let toc_entry_id = b.add_u64_field("toc_entry_id", FAST);
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(hebrew_fts::HEBREW_TOKENIZER_NAME)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text = b.add_text_field("text", TextOptions::default().set_indexing_options(indexing));
    (
        b.build(),
        LineFields {
            line_id,
            position,
            book_id,
            line_index,
            toc_entry_id,
            text,
        },
    )
}

/// An open line index.
pub struct LineIndex {
    pub index: Index,
    pub fields: LineFields,
    pub dir: PathBuf,
}

impl LineIndex {
    /// Opens an existing index directory.
    pub fn open(dir: &Path) -> Result<LineIndex> {
        let index = Index::open(MmapDirectory::open(dir)?)?;
        hebrew_fts::register_hebrew_tokenizer(index.tokenizers());
        let (_, fields) = line_schema();
        Ok(LineIndex {
            index,
            fields,
            dir: dir.to_path_buf(),
        })
    }

    /// Creates a fresh index, wiping whatever the directory held.
    pub fn create(dir: &Path) -> Result<LineIndex> {
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
        let (schema, fields) = line_schema();
        let index = Index::create(MmapDirectory::open(dir)?, schema, Default::default())?;
        hebrew_fts::register_hebrew_tokenizer(index.tokenizers());
        Ok(LineIndex {
            index,
            fields,
            dir: dir.to_path_buf(),
        })
    }

    /// Reads the build state, if any.
    pub fn read_state(dir: &Path) -> Result<Option<BuildState>> {
        let path = dir.join(STATE_FILE_NAME);
        if !path.exists() {
            return Ok(None);
        }
        let text = fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| Error::State(e.to_string()))
    }

    fn write_state(dir: &Path, state: &BuildState) -> Result<()> {
        let text = serde_json::to_string_pretty(state).map_err(|e| Error::State(e.to_string()))?;
        let tmp = dir.join(format!("{STATE_FILE_NAME}.tmp"));
        fs::write(&tmp, text)?;
        fs::rename(tmp, dir.join(STATE_FILE_NAME))?;
        Ok(())
    }
}

/// Where a build stands (FtsLib `build_state` + `fts.src`/`fts.ver`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildState {
    /// Id of the last committed line.
    pub last_line_id: i64,
    /// Lines committed so far, in catalog order; also the next position.
    pub lines_done: u64,
    /// Corpus size recorded at build start.
    pub total_lines: u64,
    /// True once the whole corpus is in.
    pub complete: bool,
    /// Provenance stamp of the DB this index was built from.
    pub fingerprint: String,
}

/// Build tuning.
pub struct BuildOptions {
    /// Total indexer heap budget in bytes. Tantivy needs ≥15 MB per thread,
    /// so the thread count is capped at `heap_bytes / 15 MB`.
    pub heap_bytes: usize,
    /// Indexing threads (upper bound; see `heap_bytes`).
    pub num_threads: usize,
    /// Lines per commit. Each commit is a resume point.
    pub lines_per_commit: u64,
    /// Progress callback: (lines done, total lines).
    pub on_progress: Option<Box<dyn FnMut(u64, u64)>>,
    /// Stop after this many lines (catalog order). The index is then left
    /// resumable, not complete. For smoke tests on a big corpus.
    pub max_lines: Option<u64>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        BuildOptions {
            heap_bytes: 50_000_000,
            num_threads: std::thread::available_parallelism()
                .map(|n| n.get().min(4))
                .unwrap_or(1),
            lines_per_commit: 20_000,
            on_progress: None,
            max_lines: None,
        }
    }
}

/// What a build call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildOutcome {
    /// The index was complete and built from this DB; nothing done.
    AlreadyBuilt,
    /// Continued an interrupted build.
    Resumed,
    /// Built from scratch.
    Built,
}

/// Builds the index from `db_path` into `index_dir`: resumes an interrupted
/// build of the same DB, keeps a complete one, otherwise builds from scratch.
/// A resume whose corpus no longer matches is wiped and rebuilt.
pub fn build(db_path: &Path, index_dir: &Path, mut options: BuildOptions) -> Result<BuildOutcome> {
    match build_or_resume(db_path, index_dir, &mut options) {
        Err(Error::IndexNoLongerMatchesCorpus(_)) => {
            LineIndex::create(index_dir)?;
            build_or_resume(db_path, index_dir, &mut options)
        }
        other => other,
    }
}

fn build_or_resume(
    db_path: &Path,
    index_dir: &Path,
    options: &mut BuildOptions,
) -> Result<BuildOutcome> {
    let current_fp = fingerprint::compute(db_path, INDEX_FORMAT_VERSION)?;
    let state = if index_dir.exists() {
        LineIndex::read_state(index_dir)?
    } else {
        None
    };

    let (line_index, mut state, outcome) = match state {
        Some(s) if s.fingerprint == current_fp && s.complete => return Ok(BuildOutcome::AlreadyBuilt),
        Some(s) if s.fingerprint == current_fp && s.lines_done > 0 => {
            (LineIndex::open(index_dir)?, s, BuildOutcome::Resumed)
        }
        _ => {
            let li = LineIndex::create(index_dir)?;
            let corpus = SeforimDbCorpus::open(db_path)?;
            let total = corpus.count_lines()? as u64;
            let s = BuildState {
                last_line_id: 0,
                lines_done: 0,
                total_lines: total,
                complete: false,
                fingerprint: current_fp.clone(),
            };
            LineIndex::write_state(index_dir, &s)?;
            drop(corpus);
            (li, s, BuildOutcome::Built)
        }
    };

    let fields = line_index.fields;
    // heap_bytes is the TOTAL budget; tantivy needs ≥15 MB per thread.
    const MIN_HEAP_PER_THREAD: usize = 15_000_000;
    let heap = options.heap_bytes.max(MIN_HEAP_PER_THREAD);
    let threads = options.num_threads.max(1).min(heap / MIN_HEAP_PER_THREAD).max(1);
    let mut writer: IndexWriter = line_index.index.writer_with_num_threads(threads, heap)?;
    let mut corpus = SeforimDbCorpus::open(db_path)?;

    // Resume re-reads the feed from lines_done - 1: the first row must be the
    // last committed line, otherwise the corpus changed under the index.
    let resuming = outcome == BuildOutcome::Resumed;
    let skip = if resuming { state.lines_done as i64 - 1 } else { 0 };
    let mut verify_first = resuming;
    let mut saw_first = false;
    let mut since_commit = 0u64;
    let total = state.total_lines;
    let lines_per_commit = options.lines_per_commit.max(1);

    let mut pending_error: Option<Error> = None;
    let max_lines = options.max_lines;
    let mut capped = false;
    {
        let state = &mut state;
        let on_progress = &mut options.on_progress;
        corpus.read_documents_in_result_order(skip, |line| {
            if max_lines.is_some_and(|m| state.lines_done >= m) {
                capped = true;
                return Ok(ControlFlow::Break(()));
            }
            if verify_first {
                verify_first = false;
                saw_first = true;
                if line.id != state.last_line_id {
                    pending_error = Some(Error::IndexNoLongerMatchesCorpus(format!(
                        "position {} was line {}, now line {}",
                        state.lines_done, state.last_line_id, line.id
                    )));
                    return Ok(ControlFlow::Break(()));
                }
                return Ok(ControlFlow::Continue(()));
            }
            writer.add_document(doc!(
                fields.line_id => line.id as u64,
                fields.position => state.lines_done,
                fields.book_id => line.book_id as u64,
                fields.line_index => line.line_index as u64,
                fields.toc_entry_id => line.toc_entry_id as u64,
                fields.text => line.text,
            ))?;
            state.lines_done += 1;
            state.last_line_id = line.id;
            since_commit += 1;
            if since_commit >= lines_per_commit {
                writer.commit()?;
                LineIndex::write_state(index_dir, state)?;
                since_commit = 0;
                if let Some(cb) = on_progress {
                    cb(state.lines_done, total);
                }
            }
            Ok(ControlFlow::Continue(()))
        })?;
    }
    if let Some(e) = pending_error {
        return Err(e);
    }
    if resuming && !saw_first {
        return Err(Error::IndexNoLongerMatchesCorpus(format!(
            "the corpus has fewer than {} lines",
            state.lines_done
        )));
    }

    writer.commit()?;
    state.complete = !capped;
    LineIndex::write_state(index_dir, &state)?;
    if let Some(cb) = &mut options.on_progress {
        cb(state.lines_done, total);
    }
    Ok(outcome)
}

/// Re-indexes the given lines from the DB: each is deleted by id and added
/// again with its current content. A line already in the index keeps its
/// catalog position; a new line is appended after the last position.
pub fn replace_lines(db_path: &Path, index_dir: &Path, line_ids: &[i64]) -> Result<()> {
    if line_ids.is_empty() {
        return Ok(());
    }
    let line_index = LineIndex::open(index_dir)?;
    let fields = line_index.fields;
    let mut state = LineIndex::read_state(index_dir)?.ok_or_else(|| {
        Error::State("no build state; build the index before updating lines".into())
    })?;

    // Existing positions, by id.
    let reader = line_index.index.reader()?;
    let searcher = reader.searcher();
    let mut existing_position = std::collections::HashMap::new();
    for &id in line_ids {
        let term = Term::from_field_u64(fields.line_id, id as u64);
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        for addr in searcher.search(&query, &DocSetCollector)? {
            let segment = searcher.segment_reader(addr.segment_ord);
            let column = segment.fast_fields().u64("position")?;
            if let Some(pos) = column.first(addr.doc_id) {
                existing_position.insert(id, pos);
            }
        }
    }

    let mut corpus = SeforimDbCorpus::open(db_path)?;
    let lines = corpus.fetch_placed_lines(line_ids)?;
    let mut writer: IndexWriter = line_index.index.writer(50_000_000)?;
    for &id in line_ids {
        writer.delete_term(Term::from_field_u64(fields.line_id, id as u64));
    }
    for line in lines {
        let position = match existing_position.get(&line.id) {
            Some(&p) => p,
            None => {
                let p = state.lines_done;
                state.lines_done += 1;
                p
            }
        };
        writer.add_document(doc!(
            fields.line_id => line.id as u64,
            fields.position => position,
            fields.book_id => line.book_id as u64,
            fields.line_index => line.line_index as u64,
            fields.toc_entry_id => line.toc_entry_id as u64,
            fields.text => line.text,
        ))?;
    }
    writer.commit()?;
    LineIndex::write_state(index_dir, &state)?;
    Ok(())
}
