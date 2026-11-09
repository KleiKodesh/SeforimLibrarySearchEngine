//! Issue #32: book atomicity is host policy. One book update = delete its
//! lines + add its new lines + ONE commit (with the host's resume payload);
//! no commit or merge in between. A crash before that commit leaves the old
//! book whole, because a tantivy commit is atomic.

use seforim_engine::{IndexOptions, LineDoc, LineIndex, SearchOptions};

fn line(id: u64, text: &str) -> LineDoc {
    LineDoc {
        line_id: id,
        position: id,
        book_id: 7,
        text: text.into(),
        section_id: None,
        no_cross: false,
    }
}

/// Segments whose files are all on disk (`.idx`, `.term`, `.store`,
/// `.fast`): a flushed segment, whether committed or not.
fn finished_segments(dir: &std::path::Path) -> usize {
    let names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names
        .iter()
        .filter_map(|n| n.strip_suffix(".idx"))
        .filter(|seg| {
            ["term", "store", "fast"]
                .iter()
                .all(|x| names.contains(&format!("{seg}.{x}")))
        })
        .count()
}

fn hits(index: &LineIndex, q: &str) -> usize {
    let o = SearchOptions { query: q.into(), ..Default::default() };
    index.search_options(&o).unwrap().hits.len()
}

#[test]
fn uncommitted_book_update_leaves_the_old_book_whole() {
    let dir = tempfile::tempdir().unwrap();
    {
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        for id in 1..=3 {
            w.add(&line(id, "oldword")).unwrap();
        }
        w.commit(Some("book7:v1")).unwrap();
    }
    let update = |index: &LineIndex| {
        let mut w = index.writer(50_000_000, 1).unwrap();
        for id in 1..=3 {
            w.delete(id);
        }
        for id in 10..=11 {
            w.add(&line(id, "newword")).unwrap();
        }
        w
    };
    // simulated crash: the whole book update is queued, then the writer is
    // dropped without a commit
    {
        let index = LineIndex::open(dir.path()).unwrap();
        drop(update(&index));
    }
    let index = LineIndex::open(dir.path()).unwrap();
    assert_eq!(hits(&index, "oldword"), 3, "old book must be intact");
    assert_eq!(hits(&index, "newword"), 0);
    assert_eq!(index.num_lines().unwrap(), 3);
    assert_eq!(index.committed_payload().unwrap().as_deref(), Some("book7:v1"));
    // resume: redo the book from the payload and commit once
    let mut w = update(&index);
    w.commit(Some("book7:v2")).unwrap();
    drop(w);
    assert_eq!(hits(&index, "oldword"), 0);
    assert_eq!(hits(&index, "newword"), 2);
    assert_eq!(index.num_lines().unwrap(), 2);
    let reopened = LineIndex::open(dir.path()).unwrap();
    assert_eq!(reopened.committed_payload().unwrap().as_deref(), Some("book7:v2"));
    assert_eq!(hits(&reopened, "newword"), 2);
}

/// Same crash, but the queued update is large enough that tantivy flushes
/// segments to disk before the drop (minimum budget, many unique words):
/// flushed but uncommitted segments are not in meta.json, so the reopen
/// still sees the old book whole.
#[test]
fn flushed_but_uncommitted_book_update_leaves_the_old_book_whole() {
    let dir = tempfile::tempdir().unwrap();
    {
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(15_000_000, 1).unwrap();
        for id in 1..=3 {
            w.add(&line(id, "oldword")).unwrap();
        }
        w.commit(Some("book7:v1")).unwrap();
    }
    {
        let index = LineIndex::open(dir.path()).unwrap();
        let before = finished_segments(dir.path());
        let mut w = index.writer(15_000_000, 1).unwrap();
        for id in 1..=3 {
            w.delete(id);
        }
        let mut text = String::new();
        let mut id = 10;
        // add lines until a segment reaches the disk (bounded)
        while finished_segments(dir.path()) <= before && id < 400_000 {
            text.clear();
            for k in 0..40 {
                text.push_str(&format!("n{} ", id * 40 + k));
            }
            w.add(&line(id, &format!("newword {text}"))).unwrap();
            id += 1;
        }
        // the flush runs on the indexing thread: wait for it to finish
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while finished_segments(dir.path()) <= before && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(finished_segments(dir.path()) > before, "no segment was flushed");
        drop(w); // simulated crash: no commit
    }
    let index = LineIndex::open(dir.path()).unwrap();
    assert_eq!(hits(&index, "oldword"), 3, "old book must be intact");
    assert_eq!(hits(&index, "newword"), 0);
    assert_eq!(index.num_lines().unwrap(), 3);
    assert_eq!(index.committed_payload().unwrap().as_deref(), Some("book7:v1"));
}
