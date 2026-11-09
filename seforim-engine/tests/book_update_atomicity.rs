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
