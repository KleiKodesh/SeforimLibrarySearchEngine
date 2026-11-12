//! The background full merge after an update (issue #29).
//!
//! A per-book update (delete the book's lines, add its new ones, commit)
//! leaves extra segments: every walk then repeats per segment, the hits need
//! a sort and the FM-index sidecar is off. So after an update the host asks
//! for a FULL merge to one segment, run on a background thread at low
//! priority ([`LineIndex::start_background_merge`]); searches keep working on
//! the committed segments meanwhile and switch to the merged one when it is
//! published (atomically). Then the FM sidecar is rebuilt.
//!
//! Two kinds:
//! - a plain merge (no book order): the lines keep their positions;
//! - a REPACK (a book order given): one reordering merge (the re-sort of
//!   [`crate::resort`]) that renumbers the positions densely in that book
//!   order. Needed when an update changed a book's line count, added or
//!   removed a book: the host gives the changed books' new lines their
//!   catalog positions in the CURRENT seforim.db numbering, which can
//!   overlap the old numbering of the books after them until the repack
//!   rewrites every position (the hits are sorted by position meanwhile,
//!   since the index has several segments).
//!
//! Restart: the request is recorded in `background_merge.json` beside the
//! index before it starts and removed when it finished. [`LineIndex::open`]
//! restarts it when that file exists or the index has more than one segment
//! (a merge is atomic: a process that ended mid-way left the old segments
//! whole; the partial files go at the next writer's garbage collection).
//!
//! Memory: a plain merge needs what [`crate::LineWriter::merge_all`] needs;
//! a repack the same (see [`crate::resort`]). Disk: old and new segment
//! coexist until the publish (about twice the index).
//!
//! One writer per index: [`LineIndex::writer_with`] / [`LineIndex::writer_auto`]
//! stop a running background merge first (a repack stops at its next check;
//! a plain merge already running is waited for, it cannot be interrupted).

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use serde::{Deserialize, Serialize};

use crate::index::WriterParts;
use crate::search::CancelToken;
use crate::{Error, LineIndex, Result};

const PENDING_FILE: &str = "background_merge.json";

/// The recorded request.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
struct Pending {
    /// The repack's book order; None = a plain merge.
    book_order: Option<Vec<u64>>,
}

fn read_pending(dir: &Path) -> Option<Pending> {
    let text = std::fs::read_to_string(dir.join(PENDING_FILE)).ok()?;
    // an unreadable record still means "a merge was pending": plain merge
    Some(serde_json::from_str(&text).unwrap_or_default())
}

fn write_pending(dir: &Path, pending: &Pending) -> Result<()> {
    let text = serde_json::to_string(pending).map_err(|e| Error::Unsupported(e.to_string()))?;
    let tmp = dir.join(format!("{PENDING_FILE}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, dir.join(PENDING_FILE))?;
    Ok(())
}

fn remove_pending(dir: &Path) {
    let _ = std::fs::remove_file(dir.join(PENDING_FILE));
}

struct Running {
    cancel: CancelToken,
    done: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// The background merge of one index DIRECTORY, shared by every
/// [`LineIndex`] handle on it in this process: a merge left running by a
/// dropped handle (a plain merge cannot be interrupted) is still stopped or
/// waited for by the next handle's writer, never a lock error.
pub(crate) fn scheduler_for(dir: &Path) -> Arc<Scheduler> {
    use std::collections::HashMap;
    use std::sync::Weak;
    static ALL: Mutex<Option<HashMap<std::path::PathBuf, Weak<Scheduler>>>> = Mutex::new(None);
    let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut all = ALL.lock().unwrap_or_else(|e| e.into_inner());
    let map = all.get_or_insert_with(HashMap::new);
    map.retain(|_, w| w.strong_count() > 0);
    if let Some(s) = map.get(&key).and_then(Weak::upgrade) {
        return s;
    }
    let s = Arc::new(Scheduler::default());
    map.insert(key, Arc::downgrade(&s));
    s
}

/// The background merge of an index directory (see [`scheduler_for`]).
#[derive(Default)]
pub(crate) struct Scheduler {
    running: Mutex<Option<Running>>,
    /// The last finished run's error (None = ok or none ran).
    last_error: Arc<Mutex<Option<String>>>,
}

impl Scheduler {
    /// Cancels a running merge and waits for its thread.
    pub(crate) fn stop(&self) {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(r) = running {
            r.cancel.cancel();
            let _ = r.handle.join();
        }
    }

    /// Cancels without waiting (the index is being dropped).
    pub(crate) fn cancel_detached(&self) {
        if let Some(r) = self.running.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            r.cancel.cancel();
        }
    }

    fn is_running(&self) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|r| !r.done.load(Ordering::Acquire))
    }

    fn wait(&self) -> Option<String> {
        let running = self.running.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(r) = running {
            let _ = r.handle.join();
        }
        self.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn spawn(self: &Arc<Self>, parts: WriterParts, order: Option<Vec<u64>>) -> Result<()> {
        let keep = self.clone();
        let cancel = CancelToken::new();
        let done = Arc::new(AtomicBool::new(false));
        let last_error = self.last_error.clone();
        *last_error.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let (c, d) = (cancel.clone(), done.clone());
        let handle = std::thread::Builder::new()
            .name("seforim background merge".into())
            .spawn(move || {
                let result = run(&parts, order.as_deref(), &c);
                match result {
                    Ok(()) => remove_pending(parts.dir()),
                    // cancelled: the record stays, the next open or request restarts it
                    Err(Error::Cancelled) => {}
                    Err(e) => *last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(e.to_string()),
                }
                d.store(true, Ordering::Release);
                // keeps the scheduler (and so the per-directory entry) alive while it runs
                drop(keep);
            })?;
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running { cancel, done, handle });
        Ok(())
    }
}

/// The merge itself, on the background thread.
fn run(parts: &WriterParts, order: Option<&[u64]>, cancel: &CancelToken) -> Result<()> {
    // this thread (the sidecar build) and the writer's merge thread run in
    // the OS background mode
    let _background = tantivy::indexer::BackgroundPriority::enter();
    CancelToken::check(Some(cancel))?;
    const HEAP: usize = 15_000_000;
    let writer = parts.writer_with(HEAP, 1, true)?;
    match order {
        // the re-sort commits, merges, publishes and rebuilds the sidecar
        Some(order) => {
            writer.resort_and_close(order, Some(cancel.clone()), None)?;
        }
        None => {
            if parts.num_segments()? > 1 {
                writer.merge_all_in_background()?;
                // the merged-away files: a writer collects them when it opens
                drop(parts.writer_with(HEAP, 1, true)?);
            } else {
                drop(writer);
            }
            if parts.num_segments()? == 1 {
                parts.build_fm_sidecar()?;
            }
        }
    }
    Ok(())
}

impl LineIndex {
    /// Starts the background full merge (issue #29; see
    /// [`crate::background_merge`]). `book_order` = None: a plain merge to one
    /// segment (nothing to do on one segment); Some: a repack in that book
    /// order (the re-sort's rules: every book keeps its lines' order and
    /// spacing, books not listed go last in their old order), run even on one
    /// segment. A repack recorded earlier and not finished is kept when a
    /// plain merge is asked. A merge already running is stopped first (a
    /// repack at its next check, a plain merge waited for). Returns at once;
    /// false when there is nothing to do. The request is recorded beside the
    /// index, so an interrupted merge restarts at the next open.
    pub fn start_background_merge(&self, book_order: Option<Vec<u64>>) -> Result<bool> {
        self.bg.stop();
        let order = book_order.or_else(|| read_pending(&self.dir).and_then(|p| p.book_order));
        if order.is_none() && self.index.searchable_segment_ids()?.len() <= 1 {
            // nothing to merge; the sidecar repair covers a missing sidecar
            remove_pending(&self.dir);
            return Ok(false);
        }
        write_pending(&self.dir, &Pending { book_order: order.clone() })?;
        self.bg.spawn(self.parts(), order)?;
        Ok(true)
    }

    /// Records a background merge WITHOUT starting it (issue #29): a host that
    /// is about to write an update needing a repack records it first, so a
    /// process that ends mid-update still repacks at the next open (or at the
    /// next [`Self::start_background_merge`], which picks the record up).
    /// `book_order` None keeps a recorded order (or records a plain merge).
    pub fn record_background_merge(&self, book_order: Option<Vec<u64>>) -> Result<()> {
        // a merge finishing now would remove the new record
        self.bg.stop();
        let order = book_order.or_else(|| read_pending(&self.dir).and_then(|p| p.book_order));
        write_pending(&self.dir, &Pending { book_order: order })
    }

    /// Restarts a background merge that did not finish (its record exists)
    /// or one the index needs (more than one segment). [`LineIndex::open`]
    /// calls it. Returns true when one started.
    pub fn resume_background_merge(&self) -> Result<bool> {
        let pending = read_pending(&self.dir).is_some();
        if !pending && self.index.searchable_segment_ids()?.len() <= 1 {
            return Ok(false);
        }
        self.start_background_merge(None)
    }

    /// True while the background merge runs.
    pub fn background_merge_running(&self) -> bool {
        self.bg.is_running()
    }

    /// True when a background merge is recorded and not finished (running,
    /// cancelled, failed, or interrupted by the end of the process).
    pub fn background_merge_pending(&self) -> bool {
        read_pending(&self.dir).is_some()
    }

    /// Waits for the background merge (if any) and returns its error.
    pub fn wait_background_merge(&self) -> Result<()> {
        match self.bg.wait() {
            None => Ok(()),
            Some(e) => Err(Error::Unsupported(format!("background merge failed: {e}"))),
        }
    }

    /// Stops the background merge (a repack at its next check; a plain merge
    /// already running is waited for). Its record stays: the next open or
    /// request restarts it.
    pub fn stop_background_merge(&self) {
        self.bg.stop();
    }
}

#[cfg(test)]
mod tests {
    use crate::{IndexOptions, LineDoc, LineIndex};

    fn line(id: u64, position: u64, book: u64, text: &str) -> LineDoc {
        LineDoc {
            line_id: id,
            position,
            book_id: book,
            text: text.into(),
            section_id: None,
            no_cross: false,
        }
    }

    fn positions(index: &LineIndex) -> Vec<(u64, u64, u64)> {
        let searcher = index.reader.searcher();
        let mut out = Vec::new();
        for segment in searcher.segment_readers() {
            let ids = segment.fast_fields().u64("line_id").unwrap();
            let pos = segment.fast_fields().u64("position").unwrap();
            let books = segment.fast_fields().u64("book_id").unwrap();
            for doc in segment.doc_ids_alive() {
                out.push((pos.first(doc).unwrap(), books.first(doc).unwrap(), ids.first(doc).unwrap()));
            }
        }
        out.sort_unstable();
        out
    }

    /// Three books of 3 lines each, one merged segment.
    fn three_books(dir: &std::path::Path) -> LineIndex {
        let index = LineIndex::create(dir, IndexOptions { positions: true, keep_marks: false }).unwrap();
        let mut w = index.writer(30_000_000, 1).unwrap();
        let mut pos = 0;
        for book in 1..=3u64 {
            for i in 0..3u64 {
                w.add(&line(book * 100 + i, pos, book, "alpha beta")).unwrap();
                pos += 1;
            }
        }
        w.commit(Some("built")).unwrap();
        w.merge_all().unwrap();
        index
    }

    /// Book 2 updated with the same line count: only its lines change, every
    /// other line keeps its position, and a plain background merge brings the
    /// index back to one sorted segment with its sidecar.
    #[test]
    fn delete_book_then_plain_background_merge() {
        let dir = tempfile::tempdir().unwrap();
        let index = three_books(dir.path());
        let before = positions(&index);
        let mut w = index.writer(30_000_000, 1).unwrap();
        w.delete_book(2).unwrap();
        for i in 0..3u64 {
            w.add(&line(250 + i, 3 + i, 2, "gamma")).unwrap();
        }
        w.commit(Some("book 2 done")).unwrap();
        drop(w);
        assert_eq!(index.reader.searcher().segment_readers().len(), 2);
        let after = positions(&index);
        assert_eq!(after.len(), 9);
        for (b, a) in before.iter().zip(&after) {
            assert_eq!(a.0, b.0, "positions unchanged");
            if b.1 != 2 {
                assert_eq!(a, b, "other books untouched");
            } else {
                assert!(a.2 >= 250, "book 2 replaced");
            }
        }
        assert!(index.start_background_merge(None).unwrap());
        index.wait_background_merge().unwrap();
        assert!(!index.background_merge_pending());
        let searcher = index.reader.searcher();
        assert_eq!(searcher.segment_readers().len(), 1);
        assert!(index.catalog_ordered(&searcher));
        assert!(index.has_fm_sidecar());
        assert_eq!(positions(&index), after);
        assert_eq!(index.committed_payload().unwrap().as_deref(), Some("book 2 done"));
    }

    /// Book 1 grows from 3 to 5 lines and a new book 4 arrives between books
    /// 2 and 3: the host writes their lines with the fresh catalog numbering
    /// (overlapping the old numbering of the books after them) and asks for
    /// a repack in catalog order; the repack renumbers every line densely.
    #[test]
    fn grown_and_new_books_are_repacked() {
        let dir = tempfile::tempdir().unwrap();
        let index = three_books(dir.path());
        let mut w = index.writer(30_000_000, 1).unwrap();
        w.delete_book(1).unwrap();
        for i in 0..5u64 {
            w.add(&line(150 + i, i, 1, "delta")).unwrap();
        }
        w.commit(Some("book 1")).unwrap();
        for i in 0..2u64 {
            // fresh numbering: book 1 (5) + book 2 (3) = 8
            w.add(&line(400 + i, 8 + i, 4, "epsilon")).unwrap();
        }
        w.commit(Some("book 4")).unwrap();
        drop(w);
        assert!(index.start_background_merge(Some(vec![1, 2, 4, 3])).unwrap());
        index.wait_background_merge().unwrap();
        let got: Vec<(u64, u64)> = positions(&index).iter().map(|&(p, b, _)| (p, b)).collect();
        let want: Vec<(u64, u64)> = [(1, 5), (2, 3), (4, 2), (3, 3)]
            .iter()
            .flat_map(|&(b, n)| std::iter::repeat(b).take(n))
            .enumerate()
            .map(|(p, b)| (p as u64, b))
            .collect();
        assert_eq!(got, want);
        let searcher = index.reader.searcher();
        assert_eq!(searcher.segment_readers().len(), 1);
        assert!(index.catalog_ordered(&searcher));
        assert!(index.has_fm_sidecar());
        assert_eq!(index.committed_payload().unwrap().as_deref(), Some("book 4"));
    }

    /// A removed book: deletes alone keep one segment, a repack still runs.
    #[test]
    fn removed_book_repack_closes_the_gap() {
        let dir = tempfile::tempdir().unwrap();
        let index = three_books(dir.path());
        let mut w = index.writer(30_000_000, 1).unwrap();
        w.delete_book(2).unwrap();
        w.commit(None).unwrap();
        drop(w);
        // deletes alone keep the one segment: a plain merge has nothing to do
        assert!(!index.start_background_merge(None).unwrap());
        assert!(index.start_background_merge(Some(vec![1, 3])).unwrap());
        index.wait_background_merge().unwrap();
        let got: Vec<(u64, u64)> = positions(&index).iter().map(|&(p, b, _)| (p, b)).collect();
        assert_eq!(got, vec![(0, 1), (1, 1), (2, 1), (3, 3), (4, 3), (5, 3)]);
    }

    /// The process ends between the update's commit and the merge (the
    /// record was written, the merge never ran): opening the index restarts
    /// it. A repack request survives a later plain request.
    #[test]
    fn interrupted_merge_restarts_on_open() {
        let dir = tempfile::tempdir().unwrap();
        {
            let index = three_books(dir.path());
            let mut w = index.writer(30_000_000, 1).unwrap();
            w.delete_book(3).unwrap();
            for i in 0..4u64 {
                w.add(&line(350 + i, 6 + i, 3, "zeta")).unwrap();
            }
            w.commit(Some("book 3")).unwrap();
            drop(w);
            // the host records the repack before its update, then "crashes"
            index.record_background_merge(Some(vec![3, 1, 2])).unwrap();
            assert!(!index.background_merge_running());
        }
        let index = LineIndex::open(dir.path()).unwrap();
        assert!(index.background_merge_pending());
        assert_eq!(index.reader.searcher().segment_readers().len(), 2);
        // what open does outside unit tests
        assert!(index.resume_background_merge().unwrap());
        index.wait_background_merge().unwrap();
        assert!(!index.background_merge_pending());
        let got: Vec<(u64, u64)> = positions(&index).iter().map(|&(p, b, _)| (p, b)).collect();
        let want: Vec<(u64, u64)> = [(3, 4), (1, 3), (2, 3)]
            .iter()
            .flat_map(|&(b, n)| std::iter::repeat(b).take(n))
            .enumerate()
            .map(|(p, b)| (p as u64, b))
            .collect();
        assert_eq!(got, want);
        // one segment, nothing recorded: nothing to resume
        assert!(!index.resume_background_merge().unwrap());
    }

    /// Several segments with no record (an update that never asked for a
    /// merge, or a crash before the record): open's resume merges them.
    #[test]
    fn several_segments_without_record_are_merged_on_resume() {
        let dir = tempfile::tempdir().unwrap();
        let index = three_books(dir.path());
        let mut w = index.writer(30_000_000, 1).unwrap();
        w.add(&line(999, 9, 9, "eta")).unwrap();
        w.commit(None).unwrap();
        drop(w);
        assert!(index.resume_background_merge().unwrap());
        index.wait_background_merge().unwrap();
        assert_eq!(index.reader.searcher().segment_readers().len(), 1);
        assert_eq!(positions(&index).len(), 10);
    }

    /// A writer asked for while a background merge runs waits for it (one
    /// writer per index) instead of failing with the lock error.
    #[test]
    fn writer_stops_the_background_merge() {
        let dir = tempfile::tempdir().unwrap();
        let index = three_books(dir.path());
        let mut w = index.writer(30_000_000, 1).unwrap();
        w.delete_book(1).unwrap();
        w.add(&line(160, 0, 1, "theta")).unwrap();
        w.commit(None).unwrap();
        drop(w);
        assert!(index.start_background_merge(Some(vec![1, 2, 3])).unwrap());
        let mut w = index.writer(30_000_000, 1).unwrap();
        w.add(&line(170, 1, 1, "iota")).unwrap();
        w.commit(None).unwrap();
        drop(w);
        assert!(!index.background_merge_running());
        // the host's update is followed by its repack again
        assert!(index.start_background_merge(Some(vec![1, 2, 3])).unwrap());
        index.wait_background_merge().unwrap();
        assert!(!index.background_merge_pending());
        let got: Vec<(u64, u64)> = positions(&index).iter().map(|&(p, b, _)| (p, b)).collect();
        assert_eq!(got, vec![(0, 1), (1, 1), (2, 2), (3, 2), (4, 2), (5, 3), (6, 3), (7, 3)]);
    }
}
