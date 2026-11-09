//! The FM-index sidecar of a line index: `<segment-id>.fmi` next to the
//! segment it was built from, over the dictionary's quote- and mark-stripped
//! term forms (rowid = ordinal + 1; on an index that keeps marks the
//! vocalized spellings of a word are rows of the same text). Built after the merge into one segment;
//! loaded lazily and memory-mapped on the first leading-`*` or fuzzy search.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tantivy::schema::Field;
use tantivy::index::SegmentId;

use crate::fm_index::{self, FmIndex};
use crate::{Error, Result};

/// File name of the sidecar for a segment.
pub fn sidecar_path(dir: &Path, segment: SegmentId) -> PathBuf {
    dir.join(format!("{}.fmi", segment.uuid_string()))
}

/// The FM text form of a dictionary term: its quotes and marks dropped
/// (FtsLib's terms carried neither). Candidates found through it are
/// verified against the real term, so this only widens the candidate set,
/// never the result.
pub fn text_form(term: &str) -> String {
    term.chars()
        .filter(|&c| !hebrew_fts::chars::is_term_quote(c) && !hebrew_fts::chars::is_term_mark(c))
        .collect()
}

/// True when `path` holds a sidecar built over `num_terms` terms.
fn sidecar_matches(path: &Path, num_terms: usize) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    // SAFETY: see `FmCache::get`.
    let Ok(map) = (unsafe { memmap2::Mmap::map(&file) }) else { return false };
    FmIndex::open(Box::new(map)).is_ok_and(|fm| fm.term_count() as usize == num_terms)
}

/// Builds the sidecar for `index`'s single segment and removes stale ones.
/// Idempotent: a sidecar that already matches the segment is kept as is.
/// Returns true when a sidecar is in place; false (nothing built, searches
/// use the walk) when the index has several segments or has no terms. Err
/// when the sidecar cannot be built (I/O failure, a dictionary the FM-index
/// cannot hold): never a silent skip, the host reports it.
pub fn build(searcher: &tantivy::Searcher, text: Field, dir: &Path) -> Result<bool> {
    let segments = searcher.segment_readers();
    if segments.len() != 1 {
        return Ok(false);
    }
    let segment = &segments[0];
    let dict = segment.inverted_index(text)?;
    let dict = dict.terms();
    if dict.num_terms() == 0 {
        return Ok(false);
    }
    let path = sidecar_path(dir, segment.segment_id());
    if sidecar_matches(&path, dict.num_terms()) {
        return Ok(true);
    }
    let mut forms: Vec<String> = Vec::with_capacity(dict.num_terms());
    let mut stream = dict.stream()?;
    while stream.advance() {
        let term = std::str::from_utf8(stream.key())
            .map_err(|_| Error::Unsupported("FM sidecar: a dictionary term is not UTF-8".into()))?;
        forms.push(text_form(term));
    }
    let bytes = fm_index::build(forms.iter().map(|s| s.as_str()))
        .map_err(|e| Error::Unsupported(format!("FM sidecar not built: {e}")))?;

    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "fmi") {
            let _ = std::fs::remove_file(path);
        }
    }
    let tmp = path.with_extension("fmi.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, path)?;
    Ok(true)
}

/// The sidecar state of an index, as a search sees it.
pub enum FmLookup {
    /// A sidecar built over the current single segment's dictionary.
    Ready(Arc<FmIndex>),
    /// No sidecar (none built, or the index has several segments).
    Missing,
    /// A sidecar file for the segment exists but cannot be used (corrupt,
    /// truncated, an unknown format, or built over another dictionary):
    /// searches fall back to the walk and carry
    /// [`NOTICE_FM_SIDECAR_UNUSABLE`]; rebuilding the sidecar fixes it.
    Unusable(String),
}

/// The notice a search carries when the index's FM sidecar exists but cannot
/// be used: leading-`*` and fuzzy words ran as walks (same results, slower).
/// Rebuild the sidecar (`build_fm_sidecar`) to fix it.
pub const NOTICE_FM_SIDECAR_UNUSABLE: &str = "fm_sidecar_unusable";

/// What the cache knows about a segment's sidecar file.
enum Cached {
    Ready(Arc<FmIndex>),
    /// Found unusable; valid while the file keeps this identity (size,
    /// modification time), so a broken file is not re-opened on every search.
    Unusable { identity: (u64, Option<std::time::SystemTime>), reason: String },
}

/// The identity of a file for the unusable cache: its size and mtime.
fn file_identity(path: &Path) -> std::io::Result<(u64, Option<std::time::SystemTime>)> {
    let meta = std::fs::metadata(path)?;
    Ok((meta.len(), meta.modified().ok()))
}

/// Lazily opened sidecar, keyed by the segment it belongs to (a reload to
/// another segment re-checks).
#[derive(Default)]
pub struct FmCache {
    inner: Mutex<Option<(SegmentId, Cached)>>,
}

impl FmCache {
    /// The sidecar for the index's current single segment, if the index has
    /// one segment, the file exists, and it was built over this dictionary.
    pub fn get(&self, dir: &Path, searcher: &tantivy::Searcher, text: Field) -> Option<Arc<FmIndex>> {
        match self.lookup(dir, searcher, text) {
            FmLookup::Ready(fm) => Some(fm),
            FmLookup::Missing | FmLookup::Unusable(_) => None,
        }
    }

    /// Forgets what is cached (after a sidecar build).
    pub fn clear(&self) {
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// [`get`](Self::get), telling a missing sidecar from an unusable one.
    /// An unusable file is remembered with its identity (size, mtime) and
    /// checked again only when the file changes, the index moves to another
    /// segment, or a sidecar build clears the cache.
    pub fn lookup(&self, dir: &Path, searcher: &tantivy::Searcher, text: Field) -> FmLookup {
        let segments = searcher.segment_readers();
        if segments.len() != 1 {
            return FmLookup::Missing;
        }
        let segment = &segments[0];
        let id = segment.segment_id();
        let path = sidecar_path(dir, id);
        if let Some((cached_id, cached)) = &*self.inner.lock().unwrap_or_else(|e| e.into_inner()) {
            if *cached_id == id {
                match cached {
                    Cached::Ready(fm) => return FmLookup::Ready(fm.clone()),
                    Cached::Unusable { identity, reason } => match file_identity(&path) {
                        Ok(now) if now == *identity => return FmLookup::Unusable(reason.clone()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FmLookup::Missing,
                        _ => {}
                    },
                }
            }
        }
        let result = self.open(&path, segment, text);
        let cached = match &result {
            FmLookup::Ready(fm) => Some(Cached::Ready(fm.clone())),
            FmLookup::Unusable(reason) => file_identity(&path)
                .ok()
                .map(|identity| Cached::Unusable { identity, reason: reason.clone() }),
            FmLookup::Missing => None,
        };
        *self.inner.lock().unwrap_or_else(|e| e.into_inner()) = cached.map(|c| (id, c));
        result
    }

    /// Opens and checks the sidecar file of `segment`.
    fn open(&self, path: &Path, segment: &tantivy::SegmentReader, text: Field) -> FmLookup {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FmLookup::Missing,
            Err(e) => return FmLookup::Unusable(format!("cannot open {}: {e}", path.display())),
        };
        // SAFETY: the file is only ever replaced through rename of a new file;
        // an open mapping keeps the old inode alive.
        let map = match unsafe { memmap2::Mmap::map(&file) } {
            Ok(map) => map,
            Err(e) => return FmLookup::Unusable(format!("cannot map {}: {e}", path.display())),
        };
        let fm = match FmIndex::open(Box::new(map)) {
            Ok(fm) => fm,
            Err(e) => return FmLookup::Unusable(format!("{}: {e}", path.display())),
        };
        let num_terms = match segment.inverted_index(text) {
            Ok(inverted) => inverted.terms().num_terms(),
            Err(e) => return FmLookup::Unusable(format!("dictionary: {e}")),
        };
        if fm.term_count() as usize != num_terms {
            return FmLookup::Unusable(format!(
                "{}: built over {} terms, the dictionary has {num_terms}",
                path.display(),
                fm.term_count()
            ));
        }
        FmLookup::Ready(Arc::new(fm))
    }
}
