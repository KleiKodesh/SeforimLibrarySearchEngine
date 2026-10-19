//! Seforim-DB specific search layer on top of `tantivy` + `hebrew-fts`.
//!
//! Everything that knows the seforim database lives here:
//! - [`schema`] / [`content`]: the DB's shape (three in the wild) and its
//!   zstd-dictionary line content.
//! - [`tree_order`] (+ [`catalog_title`]): the library's catalog order.
//! - [`corpus`]: the reads the builder and searcher need.
//! - [`fingerprint`]: index provenance.
//! - [`index`]: the tantivy line index, its builder (resumable, batched
//!   commits) and line updates.
//! - [`searcher`]: catalog-order search in batches with FtsLib snippets.
//!
//! Paths (seforim DB, index directory) are always passed in by the caller.

pub mod abbreviations;
pub mod catalog_title;
pub mod content;
pub mod corpus;
pub mod error;
pub mod fingerprint;
pub mod index;
pub mod schema;
pub mod searcher;
pub mod tree_order;

pub use corpus::SeforimDbCorpus;
pub use error::{Error, Result};
pub use index::{build, replace_lines, BuildOptions, BuildOutcome, BuildState, LineIndex};
pub use searcher::{search, Hit, HitBatch, SearchRequest, TermTable, WordGroup};

#[cfg(test)]
mod tests {
    //! End-to-end on a synthetic seforim DB in the split+zstd shape (schema 6).

    use std::ops::ControlFlow;

    use hebrew_fts::WalkOptions;
    use rusqlite::Connection;

    use super::*;

    /// Builds a tiny schema-6 database: categories, books, lines with
    /// zstd-dictionary content in `line_content`, `line_toc`.
    fn make_db(path: &std::path::Path, lines: &[(i64, i64, i64, &str)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE category(id INTEGER PRIMARY KEY, parentId INTEGER, title TEXT, level INTEGER, orderIndex INTEGER);
             CREATE TABLE book(id INTEGER PRIMARY KEY, categoryId INTEGER, title TEXT, orderIndex INTEGER, isBaseBook INTEGER, dependenceType TEXT);
             CREATE TABLE author(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE book_author(bookId INTEGER, authorId INTEGER);
             CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER);
             CREATE TABLE line_content(id INTEGER PRIMARY KEY, content TEXT);
             CREATE TABLE line_toc(lineId INTEGER PRIMARY KEY, tocEntryId INTEGER);
             CREATE TABLE zstd_dict(id INTEGER PRIMARY KEY, dict BLOB);
             INSERT INTO category VALUES (1, NULL, 'תנך', 0, 0);
             INSERT INTO book VALUES (1, 1, 'בראשית', 0, 1, NULL);
             INSERT INTO book VALUES (2, 1, 'פירוש', 1, 0, 'commentary');",
        )
        .unwrap();
        // zstd needs enough sample data to train on; pad the few test lines.
        let mut samples: Vec<Vec<u8>> = lines.iter().map(|l| l.3.as_bytes().to_vec()).collect();
        samples.extend((0..128).map(|i| format!("ויאמר אלהים יהי אור ויהי אור {i} בראשית ברא").into_bytes()));
        let dict = zstd::dict::from_samples(&samples, 4096).unwrap();
        conn.execute("INSERT INTO zstd_dict(dict) VALUES (?1)", [&dict]).unwrap();
        let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
        for (id, book, idx, text) in lines {
            conn.execute(
                "INSERT INTO line(id, bookId, lineIndex) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, book, idx],
            )
            .unwrap();
            let frame = compressor.compress(text.as_bytes()).unwrap();
            conn.execute(
                "INSERT INTO line_content(id, content) VALUES (?1, ?2)",
                rusqlite::params![id, frame],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO line_toc(lineId, tocEntryId) VALUES (?1, ?2)",
                rusqlite::params![id, 100 + idx],
            )
            .unwrap();
        }
    }

    #[test]
    fn build_and_search_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("seforim.db");
        let index_dir = dir.path().join("index");
        // Book 2 (commentary) gets ids first, but catalog order puts book 1
        // (base text) first: results must come back book 1 then book 2.
        make_db(
            &db,
            &[
                (1, 2, 0, "פירוש על התורה שניתנה"),
                (2, 2, 1, "עוד פירוש"),
                (3, 1, 0, "בראשית ברא אלהים"),
                (4, 1, 1, "והתורה ניתנה למשה"),
                (5, 1, 2, ""),
                (6, 1, 3, "תורה צוה לנו משה"),
            ],
        );

        assert_eq!(build(&db, &index_dir, BuildOptions::default()).unwrap(), BuildOutcome::Built);
        assert_eq!(
            build(&db, &index_dir, BuildOptions::default()).unwrap(),
            BuildOutcome::AlreadyBuilt
        );

        let index = LineIndex::open(&index_dir).unwrap();
        let mut corpus = SeforimDbCorpus::open(&db).unwrap();
        let request = SearchRequest {
            groups: vec![WordGroup {
                word: "תורה".into(),
                options: WalkOptions {
                    grammar_prefixes: true,
                    ..Default::default()
                },
            }],
            ..Default::default()
        };
        let mut batches = Vec::new();
        let terms = search(&index, &mut corpus, &request, |b| {
            batches.push(b);
            ControlFlow::Continue(())
        })
        .unwrap();

        let hits: Vec<&Hit> = batches.iter().flat_map(|b| b.hits.iter()).collect();
        let ids: Vec<i64> = hits.iter().map(|h| h.line_id).collect();
        // catalog order: book 1 lines (4, 6) before book 2 line (1)
        assert_eq!(ids, vec![4, 6, 1]);
        assert_eq!(hits[0].book_id, 1);
        assert_eq!(hits[0].toc_entry_id, 101);
        // matched words came from the sinks: והתורה, התורה, תורה
        let all: Vec<&str> = terms.all().iter().map(|s| s.as_str()).collect();
        assert!(all.contains(&"והתורה") && all.contains(&"תורה") && all.contains(&"התורה"));
        // short line (3 words < 8 context) was widened over same-book neighbours
        let h = hits[0];
        assert!(h.snippet_line_ids.len() > 1);
        assert!(h.snippet_line_ids.contains(&4));
        assert!(!h.snippet_line_ids.contains(&5)); // empty line skipped
        // marks are offsets into the snippet lines' texts joined by single
        // spaces (what the page re-joins); every mark is a matched term
        let joined: Vec<String> = h
            .snippet_line_ids
            .iter()
            .map(|id| batches[0].line_texts[id].clone())
            .collect();
        let joined = joined.join(" ");
        let u16s: Vec<u16> = joined.encode_utf16().collect();
        assert!(h.snippet_marks.len() >= 3);
        for m in h.snippet_marks.chunks_exact(3) {
            let (s, l, term_id) = (m[0] as usize, m[1] as usize, m[2] as usize);
            let marked = String::from_utf16(&u16s[s..s + l]).unwrap();
            assert_eq!(marked, terms.all()[term_id]);
        }
        // the matched line's own word is among the marks
        assert!(h.snippet_marks.chunks_exact(3).any(|m| {
            let (s, l) = (m[0] as usize, m[1] as usize);
            String::from_utf16(&u16s[s..s + l]).unwrap() == "והתורה"
        }));
    }

    #[test]
    fn word_distance_filter_and_update() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("seforim.db");
        let index_dir = dir.path().join("index");
        make_db(
            &db,
            &[
                (1, 1, 0, "משה קבל תורה מסיני"),
                (2, 1, 1, "משה אמר דבר אחד ואחר כך תורה"),
            ],
        );
        build(&db, &index_dir, BuildOptions::default()).unwrap();
        let index = LineIndex::open(&index_dir).unwrap();
        let mut corpus = SeforimDbCorpus::open(&db).unwrap();
        let mut request = SearchRequest {
            groups: vec![
                WordGroup {
                    word: "משה".into(),
                    options: WalkOptions::default(),
                },
                WordGroup {
                    word: "תורה".into(),
                    options: WalkOptions::default(),
                },
            ],
            max_word_distance: Some(1),
            ..Default::default()
        };
        let mut ids = Vec::new();
        search(&index, &mut corpus, &request, |b| {
            ids.extend(b.hits.iter().map(|h| h.line_id));
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(ids, vec![1]); // line 2 has 5 words between

        request.max_word_distance = None;
        let mut ids = Vec::new();
        search(&index, &mut corpus, &request, |b| {
            ids.extend(b.hits.iter().map(|h| h.line_id));
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(ids, vec![1, 2]);

        // Update: rewrite line 2 in the DB, re-index it, it drops out.
        drop(corpus);
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute("UPDATE line_content SET content = 'שלום' WHERE id = 2", []).unwrap();
        }
        replace_lines(&db, &index_dir, &[2]).unwrap();
        let index = LineIndex::open(&index_dir).unwrap();
        let mut corpus = SeforimDbCorpus::open(&db).unwrap();
        let mut ids = Vec::new();
        search(&index, &mut corpus, &request, |b| {
            ids.extend(b.hits.iter().map(|h| h.line_id));
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(ids, vec![1]);
    }
}
