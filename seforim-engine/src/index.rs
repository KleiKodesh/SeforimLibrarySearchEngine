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

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tantivy::directory::MmapDirectory;
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED,
};
use tantivy::{doc, Index, IndexWriter, Term};

use crate::Result;

/// How the index is built. Fixed at creation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
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
}

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
    (
        b.build(),
        LineFields {
            line_id,
            position,
            book_id,
            text,
            marked,
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
}

/// An open line index.
pub struct LineIndex {
    pub(crate) index: Index,
    pub(crate) fields: LineFields,
    pub(crate) options: IndexOptions,
    pub(crate) dir: PathBuf,
    /// The FM-index sidecar, opened on first use (see `fm_sidecar`).
    pub(crate) fm: crate::fm_sidecar::FmCache,
    /// TEST ONLY (compiled out of every non-test build): forces leading-`*`
    /// and fuzzy to run as FST walks although a sidecar exists, so a test
    /// can assert the sidecar and the walk give identical terms.
    #[cfg(test)]
    pub(crate) test_only_disable_fm: std::sync::atomic::AtomicBool,
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
        let index = Index::create(MmapDirectory::open(dir)?, schema, Default::default())?;
        hebrew_fts::register_hebrew_tokenizer(index.tokenizers());
        fs::write(
            dir.join(OPTIONS_FILE),
            serde_json_lite::to_string(&options),
        )?;
        Ok(Self::wrap(index, fields, options, dir))
    }

    fn wrap(index: Index, fields: LineFields, options: IndexOptions, dir: &Path) -> LineIndex {
        LineIndex {
            index,
            fields,
            options,
            dir: dir.to_path_buf(),
            fm: Default::default(),
            #[cfg(test)]
            test_only_disable_fm: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Opens an existing index.
    pub fn open(dir: &Path) -> Result<LineIndex> {
        let options = fs::read_to_string(dir.join(OPTIONS_FILE))
            .ok()
            .and_then(|s| serde_json_lite::from_str(&s))
            .unwrap_or_default();
        let index = Index::open(MmapDirectory::open(dir)?)?;
        hebrew_fts::register_hebrew_tokenizer(index.tokenizers());
        let (_, mut fields) = line_schema(options);
        // a marks index built before the line flag has no `marked` field
        fields.marked = index.schema().get_field("marked").ok();
        Ok(Self::wrap(index, fields, options, dir))
    }

    /// Builds the FM-index sidecar over the term dictionary (FtsLib's
    /// `BuildFmSidecar`). It serves leading-`*` wildcards and fuzzy words
    /// without walking the whole dictionary. Needs a single segment: call
    /// after [`LineWriter::merge_all`]. Returns false when the index has
    /// several segments or no terms (nothing built; searches still work
    /// through the FST walk).
    pub fn build_fm_sidecar(&self) -> Result<bool> {
        crate::fm_sidecar::build(&self.index, self.fields.text, &self.dir)
    }

    /// True when a sidecar matching the current single segment exists.
    pub fn has_fm_sidecar(&self) -> bool {
        let Ok(reader) = self.index.reader() else { return false };
        self.fm.get(&self.dir, &reader.searcher(), self.fields.text).is_some()
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

    /// A reader over the index (for tools and statistics).
    pub fn reader(&self) -> Result<tantivy::IndexReader> {
        Ok(self.index.reader()?)
    }

    /// The text field.
    pub fn text_field(&self) -> Field {
        self.fields.text
    }

    /// Number of lines in the index (as of the last commit).
    pub fn num_lines(&self) -> Result<u64> {
        let reader = self.index.reader()?;
        Ok(reader.searcher().num_docs())
    }

    /// Starts a writer. `heap_bytes` is the total budget; tantivy needs at
    /// least 15 MB per thread, so threads are capped by it.
    pub fn writer(&self, heap_bytes: usize, num_threads: usize) -> Result<LineWriter> {
        const MIN_HEAP_PER_THREAD: usize = 15_000_000;
        let heap = heap_bytes.max(MIN_HEAP_PER_THREAD);
        let threads = num_threads.max(1).min(heap / MIN_HEAP_PER_THREAD).max(1);
        Ok(LineWriter {
            writer: self.index.writer_with_num_threads(threads, heap)?,
            fields: self.fields,
        })
    }
}

/// Adds, deletes and commits lines.
pub struct LineWriter {
    writer: IndexWriter,
    fields: LineFields,
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
        self.writer.add_document(document)?;
        Ok(())
    }

    /// Queues the deletion of every doc with this line id. An update is a
    /// delete followed by an add of the new version.
    pub fn delete(&mut self, line_id: u64) {
        self.writer
            .delete_term(Term::from_field_u64(self.fields.line_id, line_id));
    }

    /// Commits everything queued, with an optional host payload stored
    /// atomically with it (see [`LineIndex::committed_payload`]).
    pub fn commit(&mut self, payload: Option<&str>) -> Result<()> {
        let mut prepared = self.writer.prepare_commit()?;
        if let Some(p) = payload {
            prepared.set_payload(p);
        }
        prepared.commit()?;
        Ok(())
    }

    /// Drops everything queued since the last commit.
    pub fn rollback(&mut self) -> Result<()> {
        self.writer.rollback()?;
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
    pub fn merge_all(mut self) -> Result<()> {
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
        self.writer.wait_merging_threads()?;
        Ok(())
    }
}

/// Minimal JSON for the two-bool options file (avoids a serde_json
/// dependency in the engine; the FFI crate owns JSON).
mod serde_json_lite {
    use super::IndexOptions;

    pub fn to_string(o: &IndexOptions) -> String {
        format!(
            "{{\"positions\":{},\"keep_marks\":{}}}",
            o.positions, o.keep_marks
        )
    }

    pub fn from_str(s: &str) -> Option<IndexOptions> {
        let flag = |name: &str| -> Option<bool> {
            let at = s.find(&format!("\"{name}\""))?;
            let rest = &s[at..];
            let colon = rest.find(':')?;
            let value = rest[colon + 1..].trim_start();
            Some(value.starts_with("true"))
        };
        Some(IndexOptions {
            positions: flag("positions")?,
            keep_marks: flag("keep_marks")?,
        })
    }
}
