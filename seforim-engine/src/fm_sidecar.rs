//! The FM-index sidecar of a line index: `<segment-id>.fmi` next to the
//! segment it was built from, over the dictionary's quote-stripped term
//! forms (rowid = ordinal + 1). Built after the merge into one segment;
//! loaded lazily and memory-mapped on the first leading-`*` or fuzzy search.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tantivy::schema::Field;
use tantivy::index::SegmentId;
use tantivy::Index;

use crate::fm_index::{self, FmIndex};
use crate::Result;

/// File name of the sidecar for a segment.
pub fn sidecar_path(dir: &Path, segment: SegmentId) -> PathBuf {
    dir.join(format!("{}.fmi", segment.uuid_string()))
}

/// The FM text form of a dictionary term: its quotes dropped (FtsLib's terms
/// carried none). Candidates found through it are verified against the real
/// term, so this only widens the candidate set, never the result.
pub fn text_form(term: &str) -> String {
    term.chars().filter(|&c| c != '"').collect()
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
/// use the walk) when the index has several segments, has no terms, or its
/// dictionary cannot be FM-indexed (a `keep_marks` index: terms with nikud
/// or te'amim, which the FM candidate search does not model; or more than
/// 128 distinct characters). Err only on I/O failure.
pub fn build(index: &Index, text: Field, dir: &Path) -> Result<bool> {
    let reader = index.reader()?;
    let searcher = reader.searcher();
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
        let Ok(term) = std::str::from_utf8(stream.key()) else { return Ok(false) };
        if term.chars().any(hebrew_fts::chars::is_strippable_mark) {
            return Ok(false); // a vocalized dictionary: searches use the walk
        }
        forms.push(text_form(term));
    }
    let Ok(bytes) = fm_index::build(forms.iter().map(|s| s.as_str())) else {
        return Ok(false);
    };

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

/// Lazily opened sidecar, keyed by the segment it belongs to.
#[derive(Default)]
pub struct FmCache {
    inner: Mutex<Option<(SegmentId, Arc<FmIndex>)>>,
}

impl FmCache {
    /// The sidecar for the index's current single segment, if the index has
    /// one segment, the file exists, and it was built over this dictionary.
    pub fn get(&self, dir: &Path, searcher: &tantivy::Searcher, text: Field) -> Option<Arc<FmIndex>> {
        let segments = searcher.segment_readers();
        if segments.len() != 1 {
            return None;
        }
        let segment = &segments[0];
        let id = segment.segment_id();
        if let Some((cached_id, fm)) = &*self.inner.lock().ok()? {
            if *cached_id == id {
                return Some(fm.clone());
            }
        }
        let path = sidecar_path(dir, id);
        let file = std::fs::File::open(&path).ok()?;
        // SAFETY: the file is only ever replaced through rename of a new file;
        // an open mapping keeps the old inode alive.
        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        let fm = FmIndex::open(Box::new(map)).ok()?;
        let num_terms = segment.inverted_index(text).ok()?.terms().num_terms();
        if fm.term_count() as usize != num_terms {
            return None;
        }
        let fm = Arc::new(fm);
        *self.inner.lock().ok()? = Some((id, fm.clone()));
        Some(fm)
    }
}
