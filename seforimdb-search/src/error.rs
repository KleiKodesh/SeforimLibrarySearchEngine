//! Crate error type.

use std::fmt;

/// Anything that can go wrong reading the seforim DB, building the index or
/// searching it.
#[derive(Debug)]
pub enum Error {
    /// SQLite error.
    Sqlite(rusqlite::Error),
    /// Tantivy error.
    Tantivy(tantivy::TantivyError),
    /// File system error.
    Io(std::io::Error),
    /// A line's stored content could not be turned into text. Always a
    /// problem with the database file or with which database was opened,
    /// never with the query (FtsLib `SeforimDbContentCodecException`).
    ContentCodec(String),
    /// The seforim DB exists but could not be read right now (locked or
    /// busy). Transient; never evidence of a changed DB.
    DbUnreadable(String),
    /// A resumed build found the corpus no longer matches the index
    /// (FtsLib `IndexNoLongerMatchesCorpusException`). The caller deletes the
    /// index and builds from scratch.
    IndexNoLongerMatchesCorpus(String),
    /// Build state file could not be parsed.
    State(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sqlite(e) => write!(f, "sqlite: {e}"),
            Error::Tantivy(e) => write!(f, "tantivy: {e}"),
            Error::Io(e) => write!(f, "io: {e}"),
            Error::ContentCodec(m) => write!(f, "line content: {m}"),
            Error::DbUnreadable(p) => {
                write!(f, "seforim DB could not be read right now (locked or busy): {p}")
            }
            Error::IndexNoLongerMatchesCorpus(m) => write!(f, "index no longer matches corpus: {m}"),
            Error::State(m) => write!(f, "build state: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Sqlite(e)
    }
}
impl From<tantivy::TantivyError> for Error {
    fn from(e: tantivy::TantivyError) -> Self {
        Error::Tantivy(e)
    }
}
impl From<tantivy::directory::error::OpenDirectoryError> for Error {
    fn from(e: tantivy::directory::error::OpenDirectoryError) -> Self {
        Error::Tantivy(e.into())
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Crate result.
pub type Result<T> = std::result::Result<T, Error>;
