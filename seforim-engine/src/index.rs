//! The line index: one tantivy doc per line, fed by the host.
//!
//! The doc stores no text (the host has it). It holds the line's placement
//! as fast fields and its catalog `position`: results come back in that
//! order. Positions in the text postings are optional: without them the
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
    /// Keep nikud/te'amim in the indexed terms (`hebrew_marked` tokenizer),
    /// for vocalized text searched with the ignore-nikud/te'amim options.
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
    pub book_id: Field,
    pub line_index: Field,
    pub toc_entry_id: Field,
    /// The line text, Hebrew-tokenized.
    pub text: Field,
}

const OPTIONS_FILE: &str = "engine_options.json";

fn line_schema(options: IndexOptions) -> (Schema, LineFields) {
    let mut b = Schema::builder();
    let line_id = b.add_u64_field("line_id", INDEXED | FAST);
    let position = b.add_u64_field("position", FAST);
    let book_id = b.add_u64_field("book_id", FAST);
    let line_index = b.add_u64_field("line_index", FAST);
    let toc_entry_id = b.add_u64_field("toc_entry_id", FAST);
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

/// One line as the host feeds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineDoc {
    pub line_id: u64,
    /// Rank in the library's catalog order.
    pub position: u64,
    pub book_id: u64,
    pub line_index: u64,
    pub toc_entry_id: u64,
    /// Raw line text (HTML allowed).
    pub text: String,
}

/// An open line index.
pub struct LineIndex {
    pub(crate) index: Index,
    pub(crate) fields: LineFields,
    pub(crate) options: IndexOptions,
    dir: PathBuf,
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
        Ok(LineIndex {
            index,
            fields,
            options,
            dir: dir.to_path_buf(),
        })
    }

    /// Opens an existing index.
    pub fn open(dir: &Path) -> Result<LineIndex> {
        let options = fs::read_to_string(dir.join(OPTIONS_FILE))
            .ok()
            .and_then(|s| serde_json_lite::from_str(&s))
            .unwrap_or_default();
        let index = Index::open(MmapDirectory::open(dir)?)?;
        hebrew_fts::register_hebrew_tokenizer(index.tokenizers());
        let (_, fields) = line_schema(options);
        Ok(LineIndex {
            index,
            fields,
            options,
            dir: dir.to_path_buf(),
        })
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
        self.writer.add_document(doc!(
            f.line_id => line.line_id,
            f.position => line.position,
            f.book_id => line.book_id,
            f.line_index => line.line_index,
            f.toc_entry_id => line.toc_entry_id,
            f.text => line.text.as_str(),
        ))?;
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
    pub fn merge_all(mut self) -> Result<()> {
        let reader = self.writer.index().reader()?;
        let ids = reader.searcher().segment_readers().iter().map(|s| s.segment_id()).collect::<Vec<_>>();
        if ids.len() > 1 {
            self.writer.merge(&ids).wait()?;
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
