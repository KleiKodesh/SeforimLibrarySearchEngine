//! Port of `SeforimDbSchema.cs` + `SeforimDbLineContentSql.cs`.
//!
//! Which shape of seforim database we are talking to, decided by looking at
//! the file rather than by trusting a version number. Three shapes are in the
//! wild and all must keep working:
//!
//! - Zayit: `line.content` is TEXT. No `version_line` table.
//! - Otzaria pre-6: `line.content` is TEXT.
//! - Otzaria 6+: `line` has NO content column; the text lives in
//!   `line_content`, compressed with a dictionary from `zstd_dict`.

use rusqlite::Connection;

use crate::error::Result;

/// The probed shape of a seforim database.
#[derive(Debug, Clone)]
pub struct SeforimDbSchema {
    /// Line text lives in the `line_content` sidecar and `line` itself has
    /// no `content` column. Both halves matter: a database part-way through
    /// a rebuild could have the sidecar while `line` still carries the old
    /// column, and then the old queries are the correct ones.
    pub has_split_line_content: bool,
    /// The database ships zstd dictionaries (`zstd_dict`), i.e. stored
    /// content is compressed.
    pub has_compressed_content: bool,
    /// Has a `version_line` table (alternate editions).
    pub has_version_lines: bool,
    /// A line's own TOC entry lives in `line_toc` (authoritative).
    pub has_line_toc_table: bool,
    /// `line` has a `tocEntryId` column (fallback when there is no `line_toc`).
    pub has_line_toc_entry_id_column: bool,
}

impl SeforimDbSchema {
    /// Reads the shape straight from the connection.
    pub fn probe(conn: &Connection) -> Result<SeforimDbSchema> {
        let has_line_content_table = table_exists(conn, "line_content")?;
        let line_still_has_content_column = column_exists(conn, "line", "content")?;
        Ok(SeforimDbSchema {
            has_split_line_content: has_line_content_table && !line_still_has_content_column,
            has_compressed_content: table_exists(conn, "zstd_dict")?,
            has_version_lines: table_exists(conn, "version_line")?,
            has_line_toc_table: table_exists(conn, "line_toc")?,
            has_line_toc_entry_id_column: column_exists(conn, "line", "tocEntryId")?,
        })
    }

    /// The text expression for a line, aliased `content`.
    pub fn content_column(&self, line_alias: &str) -> String {
        if self.has_split_line_content {
            format!("COALESCE({}_content.content, '') AS content", line_alias)
        } else {
            format!("{line_alias}.content")
        }
    }

    /// The same expression without the alias (scalar reads).
    pub fn content_expression(&self, line_alias: &str) -> String {
        if self.has_split_line_content {
            format!("COALESCE({}_content.content, '')", line_alias)
        } else {
            format!("{line_alias}.content")
        }
    }

    /// The JOIN that brings the text into scope, or "" when the text is a
    /// column of `line`. LEFT JOIN: a line with no sidecar row must still come
    /// back, as an empty line rather than a vanished one.
    pub fn content_join(&self, line_alias: &str) -> String {
        if self.has_split_line_content {
            format!(
                " LEFT JOIN line_content {a}_content ON {a}_content.id = {a}.id",
                a = line_alias
            )
        } else {
            String::new()
        }
    }

    /// The expression for a line's own TOC entry (0 when none), and the JOIN
    /// it needs (FtsLib `FtsFetchPlacedLines`).
    pub fn toc_expression_and_join(&self) -> (&'static str, &'static str) {
        if self.has_line_toc_table {
            (
                "COALESCE(lt.tocEntryId, 0)",
                " LEFT JOIN line_toc lt ON lt.lineId = l.id",
            )
        } else if self.has_line_toc_entry_id_column {
            ("COALESCE(l.tocEntryId, 0)", "")
        } else {
            ("0", "")
        }
    }
}

/// True if the database has a table with this name.
pub fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let mut stmt =
        conn.prepare_cached("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1")?;
    Ok(stmt.exists([table])?)
}

/// True if the named table has a column with this name. The table name is
/// always one of our own literals, never caller input.
pub fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}
