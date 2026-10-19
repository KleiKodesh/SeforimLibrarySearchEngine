//! Port of `SeforimDbFtsCorpus.cs` (+ the SQL from `SeforimDbSqlStrings.cs`).
//!
//! The seforim database served to the index builder and the searcher. ONE
//! CONNECTION PER INSTANCE, ONE INSTANCE PER OPERATION: concurrent searches
//! never share a connection. The schema is probed once and the zstd
//! dictionary loaded once per instance.

use std::collections::HashMap;
use std::ops::ControlFlow;
use std::path::Path;

use rusqlite::{params_from_iter, Connection, OpenFlags};

use crate::content::LineContentCodec;
use crate::error::Result;
use crate::schema::SeforimDbSchema;
use crate::tree_order;

/// SQLite's default variable limit is 999; staying under it caps the IN-lists.
const SQLITE_IN_LIST_CHUNK_SIZE: usize = 999;

/// One line as the catalog-order feed yields it.
#[derive(Debug, Clone)]
pub struct LineDoc {
    pub id: i64,
    pub text: String,
    pub book_id: i64,
    pub line_index: i64,
    pub toc_entry_id: i64,
}

/// One search-result line with its placement (FtsLib `FtsDocumentPlacement`).
#[derive(Debug, Clone)]
pub struct PlacedLine {
    pub id: i64,
    pub text: String,
    pub book_id: i64,
    pub line_index: i64,
    pub toc_entry_id: i64,
}

/// One line of the library with its position.
#[derive(Debug, Clone)]
pub struct LineAtPosition {
    pub id: i64,
    pub book_id: i64,
    pub line_index: i64,
    pub text: String,
}

/// A read-only view of one seforim database.
pub struct SeforimDbCorpus {
    conn: Connection,
    schema: SeforimDbSchema,
    codec: LineContentCodec,
}

impl SeforimDbCorpus {
    /// Opens `database_path` read-only with the corpus-scan tuning FtsLib
    /// uses (64 MB page cache, temp tables in memory).
    pub fn open(database_path: &Path) -> Result<SeforimDbCorpus> {
        let conn = Connection::open_with_flags(
            database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let schema = SeforimDbSchema::probe(&conn)?;
        let codec = LineContentCodec::create(&conn)?;
        conn.execute_batch("PRAGMA cache_size=-65536; PRAGMA temp_store=MEMORY;")?;
        Ok(SeforimDbCorpus {
            conn,
            schema,
            codec,
        })
    }

    /// The probed schema.
    pub fn schema(&self) -> &SeforimDbSchema {
        &self.schema
    }

    /// `SELECT COUNT(*) FROM line`.
    pub fn count_lines(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM line", [], |r| r.get(0))?)
    }

    /// Lines per book (`FtsLineCountsByBook`).
    pub fn line_counts_by_book(&self) -> Result<HashMap<i64, i64>> {
        let mut stmt = self.conn.prepare("SELECT bookId, COUNT(*) FROM line GROUP BY bookId")?;
        let mut rows = stmt.query([])?;
        let mut counts = HashMap::new();
        while let Some(row) = rows.next()? {
            counts.insert(row.get::<_, i64>(0)?, row.get::<_, i64>(1)?);
        }
        Ok(counts)
    }

    /// Every line in the order the app shows the library: books in catalog
    /// tree order, each book's lines in reading order (`lineIndex`, id as tie
    /// break). Lines of a book the catalog does not list follow at the end, by
    /// book id. `skip` counts lines in this order: whole books are skipped by
    /// their line counts. `on_line` may stop the feed early.
    ///
    /// Compared with FtsLib's feed (`FtsReadBookLines`), each row also carries
    /// `lineIndex` and the line's TOC entry, read off the same `line` row, so
    /// the index can store placement as fast fields.
    pub fn read_documents_in_result_order(
        &mut self,
        mut skip: i64,
        mut on_line: impl FnMut(LineDoc) -> Result<ControlFlow<()>>,
    ) -> Result<()> {
        let tree_order = tree_order::load(&self.conn)?;
        let line_counts = self.line_counts_by_book()?;

        let mut book_order: Vec<i64> = line_counts.keys().copied().collect();
        book_order.sort_by(|a, b| {
            let (ra, rb) = (tree_order.get(a), tree_order.get(b));
            match (ra, rb) {
                (Some(x), Some(y)) => x.cmp(y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.cmp(b),
            }
        });

        let (toc_expr, toc_join) = self.schema.toc_expression_and_join();
        let sql = format!(
            "SELECT l.id, {content}, l.lineIndex, {toc} FROM line l{cjoin}{tjoin} WHERE l.bookId = ?1 \
             ORDER BY l.lineIndex, l.id LIMIT -1 OFFSET ?2",
            content = self.schema.content_column("l"),
            toc = toc_expr,
            cjoin = self.schema.content_join("l"),
            tjoin = toc_join,
        );
        let mut stmt = self.conn.prepare(&sql)?;
        for book_id in book_order {
            let lines = line_counts[&book_id];
            if skip >= lines {
                skip -= lines;
                continue;
            }
            let mut rows = stmt.query(rusqlite::params![book_id, skip])?;
            skip = 0;
            while let Some(row) = rows.next()? {
                let doc = LineDoc {
                    id: row.get(0)?,
                    text: self.codec.decode_or_empty(row.get_ref(1)?)?,
                    book_id,
                    line_index: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    toc_entry_id: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                };
                if on_line(doc)?.is_break() {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Search-result content plus each line's placement, from the one `line`
    /// row the content fetch reads (`FtsFetchPlacedLines`). Rows come back in
    /// SQLite's order; callers key by id.
    pub fn fetch_placed_lines(&mut self, ids: &[i64]) -> Result<Vec<PlacedLine>> {
        let mut out = Vec::with_capacity(ids.len());
        let (toc_expr, toc_join) = self.schema.toc_expression_and_join();
        for chunk in ids.chunks(SQLITE_IN_LIST_CHUNK_SIZE) {
            let sql = format!(
                "SELECT l.id, {content}, l.bookId, l.lineIndex, {toc} FROM line l{cjoin}{tjoin} WHERE \
                 l.id IN ({ph})",
                content = self.schema.content_column("l"),
                toc = toc_expr,
                cjoin = self.schema.content_join("l"),
                tjoin = toc_join,
                ph = placeholders(chunk.len()),
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let mut rows = stmt.query(params_from_iter(chunk.iter()))?;
            while let Some(row) = rows.next()? {
                out.push(PlacedLine {
                    id: row.get(0)?,
                    text: self.codec.decode_or_empty(row.get_ref(1)?)?,
                    book_id: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    line_index: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    toc_entry_id: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                });
            }
        }
        Ok(out)
    }

    /// The lines at the given (bookId, lineIndex) positions
    /// (`FtsFetchLinesAtBookPositions`): one statement per 499 positions. A
    /// position with no line is absent; empty lines are returned (the caller
    /// skips them).
    pub fn fetch_lines_at_book_positions(
        &mut self,
        positions: &[(i64, i64)],
    ) -> Result<Vec<LineAtPosition>> {
        let mut out = Vec::with_capacity(positions.len());
        const PER_STATEMENT: usize = SQLITE_IN_LIST_CHUNK_SIZE / 2;
        for chunk in positions.chunks(PER_STATEMENT) {
            let values: Vec<String> = (0..chunk.len())
                .map(|i| format!("(?{},?{})", 2 * i + 1, 2 * i + 2))
                .collect();
            let sql = format!(
                "SELECT l.id, l.bookId, l.lineIndex, {content} FROM line l{cjoin} WHERE (l.bookId, \
                 l.lineIndex) IN (VALUES {values})",
                content = self.schema.content_column("l"),
                cjoin = self.schema.content_join("l"),
                values = values.join(","),
            );
            let flat: Vec<i64> = chunk.iter().flat_map(|&(b, i)| [b, i]).collect();
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let mut rows = stmt.query(params_from_iter(flat.iter()))?;
            while let Some(row) = rows.next()? {
                out.push(LineAtPosition {
                    id: row.get(0)?,
                    book_id: row.get(1)?,
                    line_index: row.get(2)?,
                    text: self.codec.decode_or_empty(row.get_ref(3)?)?,
                });
            }
        }
        Ok(out)
    }
}

fn placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}
