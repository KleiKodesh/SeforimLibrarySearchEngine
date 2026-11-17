//! Semantic search over Otzaria's vector set (issue #18), read-only and
//! exact, as Otzaria does it.
//!
//! Otzaria's downloaded set (`.oxv` segments: a base plus deltas, `.del`
//! bitmaps, `set.json` generations; e.g. `%APPDATA%\otzaria\vectors`) is the
//! ONLY copy of the vectors: no conversion, no graph, nothing built on the
//! user's machine. A search is an exact scan over every live slot
//! ([`scan`], a port of Otzaria's `oxv/scan.rs`) with Otzaria's integer
//! kernels ([`kernel`]: `i8-sym-vec`, `i8-sym-dim`, `rot192-i8i4` with the
//! query rotated once; scalar, AVX2 and NEON computing the same integers, so
//! results are bit-identical on every CPU).
//!
//! The host keeps the line ids: it registers, per Otzaria book name
//! (`id:<bookId>`), its book id and the line id of each line ordinal
//! ([`set::LineMap`]); [`set::SegmentSet::search_lines`] maps the scanned
//! vectors to lines, one result per line. The query vector comes from the
//! host's model (Otzaria's query model); no model runs here.
//!
//! [`fusion`] is the hybrid ranking (lexical + semantic lines).

pub mod chunker;
pub mod fusion;
pub mod install;
pub mod kernel;
pub mod oxv;
pub mod scan;
pub mod set;
pub mod writer;

/// Errors of the vector set.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A bad request (wrong dimensions, zero vector...).
    Invalid(String),
    /// A segment or set file is damaged or of another format version.
    Format(String),
    /// Stopped through the cancel flag.
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "vector set I/O: {e}"),
            Error::Invalid(e) => write!(f, "vector request: {e}"),
            Error::Format(e) => write!(f, "vector set file: {e}"),
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

pub type Result<T> = std::result::Result<T, Error>;

/// A hit's `chunk` bit: the line was found through a neighbour's vector (a
/// short line borrowing it, Otzaria's alias).
pub const CHUNK_BORROWED: u32 = 1 << 31;

/// `v` scaled to unit length; a zero or non-finite vector is refused.
pub fn normalized(v: &[f32]) -> Result<Vec<f32>> {
    let norm = v.iter().map(|x| (*x as f64) * (*x as f64)).sum::<f64>().sqrt();
    if !(norm.is_finite() && norm > 0.0) {
        return Err(Error::Invalid("a zero or non-finite vector".into()));
    }
    Ok(v.iter().map(|x| (*x as f64 / norm) as f32).collect())
}

#[cfg(test)]
mod tests;
