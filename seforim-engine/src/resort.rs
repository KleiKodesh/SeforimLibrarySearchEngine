//! Re-sort an index into a new catalog order (optional; not part of a build).
//!
//! When the host's catalog order changes (books move in the tree), the index
//! can be brought to the new order without re-indexing: ONE reordering merge
//! (tantivy `IndexWriter::merge_reordered`) writes the lines in the new order
//! and rewrites each line's `position`. The host gives the new order of the
//! books; every book keeps its lines in their old order and their old spacing
//! (`position - first position of the book`), and the books are packed in the
//! new order from position 0; indexed books the host does not list follow,
//! in their old order. When the old positions were the host's dense
//! catalog numbering, the new ones are the numbering a fresh build in the new
//! order would give. See [`crate::LineWriter::resort_and_close`].

use std::collections::HashMap;

use tantivy::indexer::ReorderPlan;
use tantivy::{DocAddress, SegmentReader};

use crate::{Error, Result};

/// What a re-sort is doing (the `stage` of its progress reports).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ResortStage {
    /// Reading the lines' books and positions, building the new order.
    Plan = 1,
    /// Writing the postings (`done` / `total` terms).
    Postings = 2,
    /// Writing the doc store.
    Store = 3,
    /// Writing the per-line fields (`position` rewritten).
    FastFields = 4,
    /// Publishing the new segment and removing the old one.
    Publish = 5,
    /// Rebuilding the FM-index sidecar for the new segment.
    FmSidecar = 6,
    /// Finished.
    Done = 7,
}

impl From<tantivy::indexer::ReorderStage> for ResortStage {
    fn from(stage: tantivy::indexer::ReorderStage) -> ResortStage {
        use tantivy::indexer::ReorderStage as S;
        match stage {
            S::Plan => ResortStage::Plan,
            S::Postings => ResortStage::Postings,
            S::Store => ResortStage::Store,
            S::FastFields => ResortStage::FastFields,
        }
    }
}

/// Progress callback of a re-sort: `(stage, done, total)`, called from the
/// merge thread.
pub type ResortProgressFn = std::sync::Arc<dyn Fn(ResortStage, u64, u64) + Send + Sync>;

/// The reorder plan: lines sorted by (new book rank, old position), each
/// line's new position = its book's new start + its offset in the book.
///
/// Memory: the plan holds one address per line (the order tantivy needs) and
/// a few entries per book; the new positions are computed per line while
/// tantivy writes them (`fast_field_values`), never held at once. So the
/// re-sort needs no more memory than a plain merge.
pub(crate) fn plan(readers: &[SegmentReader], book_order: &[u64]) -> Result<ReorderPlan> {
    let mut books_col = Vec::with_capacity(readers.len());
    let mut positions_col = Vec::with_capacity(readers.len());
    for reader in readers {
        books_col.push(reader.fast_fields().u64("book_id")?);
        positions_col.push(reader.fast_fields().u64("position")?);
    }
    let value = |col: &[tantivy::columnar::Column<u64>], addr: DocAddress| {
        col[addr.segment_ord as usize].first(addr.doc_id)
            .ok_or_else(|| Error::Unsupported("a line without book_id or position".into()))
    };
    // pass 1: each book's span (first, last position) and line count
    let mut books: HashMap<u64, Book> = HashMap::new();
    for (ord, reader) in readers.iter().enumerate() {
        for doc in reader.doc_ids_alive() {
            let addr = DocAddress::new(ord as u32, doc);
            let (b, p) = (value(&books_col, addr)?, value(&positions_col, addr)?);
            let book = books.entry(b).or_insert(Book { first: p, last: p, lines: 0, start: None, slot: 0 });
            book.first = book.first.min(p);
            book.last = book.last.max(p);
            book.lines += 1;
        }
    }
    // the new order of the books: the host's, then the unlisted ones in their old order
    let mut ordered: Vec<u64> = Vec::with_capacity(books.len());
    let mut listed: HashMap<u64, ()> = HashMap::with_capacity(book_order.len());
    for &book in book_order {
        if listed.insert(book, ()).is_some() {
            return Err(Error::Unsupported(format!("book {book} appears twice in the new order")));
        }
        // a book of the catalog with no indexed line: nothing to place
        if books.contains_key(&book) {
            ordered.push(book);
        }
    }
    let mut missing: Vec<(u64, u64)> =
        books.iter().filter(|(b, _)| !listed.contains_key(b)).map(|(&b, book)| (book.first, b)).collect();
    drop(listed);
    missing.sort_unstable();
    ordered.extend(missing.into_iter().map(|(_, b)| b));
    // each book's new start position and its slice of the new doc order
    let (mut cursor, mut slot) = (0u64, 0usize);
    for b in &ordered {
        let book = books.get_mut(b).expect("listed above");
        book.start = Some(cursor);
        book.slot = slot;
        cursor += book.last - book.first + 1;
        slot += book.lines as usize;
    }
    // pass 2: each line into its book's slice, then each slice in old position order
    // (stable on equal positions: segment / doc order). In an index already in catalog
    // order the slices come out sorted and the sort is a check.
    let mut fill: HashMap<u64, usize> = books.iter().map(|(&b, book)| (b, book.slot)).collect();
    let mut new_to_old = vec![DocAddress::new(0, 0); slot];
    for (ord, reader) in readers.iter().enumerate() {
        for doc in reader.doc_ids_alive() {
            let addr = DocAddress::new(ord as u32, doc);
            let next = fill.get_mut(&value(&books_col, addr)?).expect("seen in pass 1");
            new_to_old[*next] = addr;
            *next += 1;
        }
    }
    drop(fill);
    let key = |addr: &DocAddress| {
        (positions_col[addr.segment_ord as usize].first(addr.doc_id).unwrap_or(0), addr.segment_ord, addr.doc_id)
    };
    for b in &ordered {
        let book = &books[b];
        let slice = &mut new_to_old[book.slot..book.slot + book.lines as usize];
        if !slice.windows(2).all(|w| key(&w[0]) <= key(&w[1])) {
            slice.sort_by_cached_key(key);
        }
    }
    // the new position of a line: old position + its book's shift
    let shift: HashMap<u64, i64> =
        books.iter().map(|(&b, book)| (b, book.start.expect("placed above") as i64 - book.first as i64)).collect();
    drop(books);
    let new_position: tantivy::indexer::FastFieldValueFn = Box::new(move |addr: DocAddress| {
        let seg = addr.segment_ord as usize;
        let (b, p) = (books_col[seg].first(addr.doc_id).unwrap_or(0), positions_col[seg].first(addr.doc_id).unwrap_or(0));
        (p as i64 + shift.get(&b).copied().unwrap_or(0)) as u64
    });
    Ok(ReorderPlan { new_to_old, fast_field_values: vec![("position".to_string(), new_position)] })
}

/// A book of the plan.
struct Book {
    first: u64,
    last: u64,
    lines: u64,
    /// new first position
    start: Option<u64>,
    /// first new doc of the book
    slot: usize,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::ResortStage;
    use crate::{CancelToken, Error, GroupSpec, IndexOptions, LineDoc, LineIndex, SearchRequest};

    fn line(id: u64, position: u64, book: u64, text: &str) -> LineDoc {
        LineDoc { line_id: id, position, book_id: book, text: text.into(), section_id: None, no_cross: false }
    }

    /// Books 1..=4, 50 lines each in catalog order 1, 2, 3, 4 (dense
    /// positions), in two commits (two segments); line 7 deleted.
    fn index(dir: &std::path::Path, positions: bool) -> LineIndex {
        let options = IndexOptions { positions, ..IndexOptions::default() };
        let index = LineIndex::create(dir, options).unwrap();
        let mut w = index.writer(60_000_000, 1).unwrap();
        for p in 0..200u64 {
            let text = if p % 3 == 0 { "alpha beta" } else { "gamma alpha" };
            w.add(&line(1000 + p, p, 1 + p / 50, text)).unwrap();
            if p == 120 {
                w.commit(Some("payload-1")).unwrap();
            }
        }
        w.commit(Some("payload-2")).unwrap();
        w.delete(1007);
        w.commit(Some("payload-3")).unwrap();
        index
    }

    fn hits(index: &LineIndex, word: &str) -> Vec<(u64, u64, u64)> {
        let request = SearchRequest {
            groups: vec![GroupSpec::Wildcard { pattern: format!("{word}*"), ignore_quotes: false }],
            ..Default::default()
        };
        index.search(&request).unwrap().hits.iter().map(|h| (h.position, h.line_id, h.book_id)).collect()
    }

    #[test]
    fn resort_moves_books_and_keeps_every_hit() {
        for positions in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let index = index(dir.path(), positions);
            let before = hits(&index, "alph");
            let stamp = index.order_stamp().unwrap();
            let stages = Arc::new(Mutex::new(Vec::new()));
            let seen = stages.clone();
            let progress: super::ResortProgressFn = Arc::new(move |stage, _, _| seen.lock().unwrap().push(stage));
            // book 3 to the front, book 1 to the end; 9 has no lines
            let w = index.writer(60_000_000, 1).unwrap();
            let lines = w.resort_and_close(&[3, 9, 2, 4, 1], None, Some(progress)).unwrap();
            assert_eq!(lines, 199);
            let after = hits(&index, "alph");
            // same lines, new order: positions strictly increasing, books 3, 2, 4, 1
            let mut a: Vec<u64> = before.iter().map(|h| h.1).collect();
            let mut b: Vec<u64> = after.iter().map(|h| h.1).collect();
            a.sort();
            b.sort();
            assert_eq!(a, b);
            assert!(after.windows(2).all(|w| w[0].0 < w[1].0), "0 inversions");
            let books: Vec<u64> = after.iter().map(|h| h.2).fold(Vec::new(), |mut v, b| {
                if v.last() != Some(&b) {
                    v.push(b);
                }
                v
            });
            assert_eq!(books, vec![3, 2, 4, 1]);
            // a line keeps its offset in its book: book 3 starts at 0, book 1 at 150
            for &(p, id, book) in &after {
                let old = id - 1000;
                let start = match book { 3 => 0, 2 => 50, 4 => 100, _ => 150 };
                assert_eq!(p, start + old % 50);
            }
            let searcher = index.reader().unwrap().searcher();
            assert!(index.catalog_ordered(&searcher));
            assert_ne!(index.order_stamp().unwrap(), stamp, "a resume from the old order fails");
            assert_eq!(index.committed_payload().unwrap().as_deref(), Some("payload-3"));
            if positions {
                assert!(index.has_fm_sidecar());
            }
            // one segment's files only (old segments and sidecars removed)
            let live = searcher.segment_readers()[0].segment_id().uuid_string();
            for entry in std::fs::read_dir(dir.path()).unwrap() {
                let name = entry.unwrap().file_name().into_string().unwrap();
                if name.len() > 32 && name.as_bytes()[32] == b'.' {
                    assert!(name.starts_with(&live), "leftover file {name}");
                }
            }
            let stages = stages.lock().unwrap();
            for s in [ResortStage::Plan, ResortStage::Postings, ResortStage::FastFields, ResortStage::Publish, ResortStage::Done] {
                assert!(stages.contains(&s), "{s:?} reported");
            }
        }
    }

    #[test]
    fn books_missing_from_the_order_go_last_and_duplicates_fail() {
        let dir = tempfile::tempdir().unwrap();
        let index = index(dir.path(), true);
        let before = hits(&index, "alph");
        let w = index.writer(60_000_000, 1).unwrap();
        assert!(matches!(w.resort_and_close(&[3, 2, 1, 4, 2], None, None), Err(Error::Unsupported(_))));
        assert_eq!(hits(&index, "alph"), before);
        // 2 and 4 are not listed: after 3 and 1, in their old order (2 before 4)
        let w = index.writer(60_000_000, 1).unwrap();
        assert_eq!(w.resort_and_close(&[3, 1], None, None).unwrap(), 199);
        let after = hits(&index, "alph");
        let books: Vec<u64> = after.iter().map(|h| h.2).fold(Vec::new(), |mut v, b| {
            if v.last() != Some(&b) {
                v.push(b);
            }
            v
        });
        assert_eq!(books, vec![3, 1, 2, 4]);
        assert!(after.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn cancelled_resort_leaves_the_old_index() {
        let dir = tempfile::tempdir().unwrap();
        let index = index(dir.path(), true);
        let before = hits(&index, "alph");
        let stamp = index.order_stamp().unwrap();
        let files = |d: &std::path::Path| {
            let mut v: Vec<String> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
            v.sort();
            v
        };
        // cancelled before it starts, and from inside the merge
        let token = CancelToken::new();
        token.cancel();
        let w = index.writer(60_000_000, 1).unwrap();
        assert!(matches!(w.resort_and_close(&[4, 3, 2, 1], Some(token), None), Err(Error::Cancelled)));
        let listed = files(dir.path());
        let token = CancelToken::new();
        let inside = token.clone();
        let calls = Arc::new(AtomicU64::new(0));
        let counted = calls.clone();
        let progress: super::ResortProgressFn = Arc::new(move |stage, _, _| {
            counted.fetch_add(1, Ordering::Relaxed);
            if stage == ResortStage::Plan {
                inside.cancel();
            }
        });
        let w = index.writer(60_000_000, 1).unwrap();
        assert!(matches!(w.resort_and_close(&[4, 3, 2, 1], Some(token), Some(progress)), Err(Error::Cancelled)));
        assert!(calls.load(Ordering::Relaxed) > 0);
        assert_eq!(hits(&index, "alph"), before);
        assert_eq!(index.order_stamp().unwrap(), stamp);
        // the cancelled merge's partial files are gone
        let reopened = LineIndex::open(dir.path()).unwrap();
        assert_eq!(hits(&reopened, "alph"), before);
        drop(index.writer(60_000_000, 1).unwrap());
        assert_eq!(files(dir.path()), listed);
    }
}
