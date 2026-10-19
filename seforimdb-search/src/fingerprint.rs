//! Port of `SeforimDbContentFingerprint.cs`: a fingerprint of the seforim
//! database's CONTENT, used to decide whether an index was built from the
//! database it is now asked to answer for. Ignores everything about the file
//! except the rows (a false positive here is a full rebuild).

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::error::{Error, Result};
use crate::schema::{table_exists, SeforimDbSchema};

/// Fingerprint format version.
const FORMAT_TAG: &str = "c1";

/// Fingerprints `database_path`. `prefix` is prepended verbatim (normally the
/// index's own format version), so changing the index pipeline still forces
/// a rebuild. A missing file yields a stable "missing" value; a file that
/// cannot be read right now is an error (never "different database").
pub fn compute(database_path: &Path, prefix: &str) -> Result<String> {
    let path_str = database_path.to_string_lossy().to_lowercase();
    let mut head = String::new();
    if !prefix.is_empty() {
        head.push_str(prefix);
        head.push('|');
    }
    head.push_str(FORMAT_TAG);
    head.push('|');
    head.push_str(&path_str);
    head.push('|');

    if !database_path.exists() {
        return Ok(head + "missing");
    }
    match try_read_content(database_path) {
        Some(content) => Ok(head + &content + &release_stamp(database_path)),
        None => Err(Error::DbUnreadable(database_path.display().to_string())),
    }
}

/// Why `recorded` differs from `current`, in words for a log line. None when
/// they are equal.
pub fn describe_change(recorded: Option<&str>, current: &str) -> Option<String> {
    let Some(recorded) = recorded else {
        return Some("the existing index has no provenance stamp".into());
    };
    if recorded.eq_ignore_ascii_case(current) {
        return None;
    }
    if is_legacy(recorded) {
        return Some(format!("the existing index has a pre-{FORMAT_TAG} stamp"));
    }
    Some("seforim DB content or path changed".into())
}

/// True for a value written before this FORMAT existed.
pub fn is_legacy(fingerprint: &str) -> bool {
    !fingerprint.starts_with(&format!("{FORMAT_TAG}|"))
        && !fingerprint.contains(&format!("|{FORMAT_TAG}|"))
}

fn try_read_content(database_path: &Path) -> Option<String> {
    let conn = Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let schema_version: i64 = conn.query_row("PRAGMA schema_version", [], |r| r.get(0)).ok()?;
    let schema = SeforimDbSchema::probe(&conn).ok()?;
    let (line_min, line_max) = id_range(&conn, "line")?;
    let (book_min, book_max) = id_range(&conn, "book")?;
    let tail_sql = if schema.has_split_line_content {
        "SELECT length(content) FROM line_content WHERE id = ?1"
    } else {
        "SELECT length(content) FROM line WHERE id = ?1"
    };
    let tail: i64 = conn
        .query_row(tail_sql, [line_max], |r| r.get::<_, Option<i64>>(0))
        .ok()
        .flatten()
        .unwrap_or(-1);
    let dbv = read_db_version(&conn)?;
    Some(format!(
        "schema={schema_version}|line={line_min}:{line_max}|book={book_min}:{book_max}|tail={tail}|dbv={dbv}"
    ))
}

fn read_db_version(conn: &Connection) -> Option<String> {
    if !table_exists(conn, "schema_meta").ok()? {
        return Some(String::new());
    }
    Some(
        conn.query_row(
            "SELECT value FROM schema_meta WHERE key = 'db_version'",
            [],
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .unwrap_or_default(),
    )
}

fn release_stamp(database_path: &Path) -> String {
    let Some(folder) = database_path.parent() else {
        return String::new();
    };
    match std::fs::read_to_string(folder.join("release_info.txt")) {
        Ok(text) => format!("|release={}", text.trim()),
        Err(_) => String::new(),
    }
}

fn id_range(conn: &Connection, table: &str) -> Option<(i64, i64)> {
    conn.query_row(&format!("SELECT MIN(id), MAX(id) FROM {table}"), [], |r| {
        Ok((
            r.get::<_, Option<i64>>(0)?.unwrap_or(0),
            r.get::<_, Option<i64>>(1)?.unwrap_or(0),
        ))
    })
    .ok()
}
