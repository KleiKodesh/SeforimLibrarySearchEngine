//! The search engine proper, with no database code: the host owns the
//! corpus and feeds it.
//!
//! - [`LineIndex`] / [`LineWriter`]: create or open an index, add / delete
//!   lines, commit with a host payload (resume watermark, provenance)
//!   stored atomically with the documents. Positions are optional
//!   ([`IndexOptions`]).
//! - [`LineIndex::search`]: every matching line in catalog order, plus a
//!   [`SnippetSession`] that locates FtsLib snippets in the texts the host
//!   fetches, applying the word-distance / order filters, and widens short
//!   windows over neighbour lines the host supplies.

pub mod index;
pub mod search;

pub use index::{IndexOptions, LineDoc, LineIndex, LineWriter};
pub use search::{
    GroupSpec, PlacedHit, SearchRequest, SearchResult, Snippet, SnippetSession, WalkOptionsSpec,
    SNIPPET_FLAG_LEADING_ELLIPSIS, SNIPPET_FLAG_TRAILING_ELLIPSIS,
};

/// Engine errors.
#[derive(Debug)]
pub enum Error {
    Tantivy(tantivy::TantivyError),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Tantivy(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

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

/// Engine result.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: u64, position: u64, book: u64, idx: u64, text: &str) -> LineDoc {
        LineDoc {
            line_id: id,
            position,
            book_id: book,
            line_index: idx,
            toc_entry_id: 100 + idx,
            text: text.into(),
        }
    }

    fn word(w: &str, prefixes: bool) -> GroupSpec {
        GroupSpec::Word {
            word: w.into(),
            options: WalkOptionsSpec {
                grammar_prefixes: prefixes,
                ..Default::default()
            },
        }
    }

    fn build(dir: &std::path::Path, options: IndexOptions, lines: &[LineDoc]) -> LineIndex {
        let index = LineIndex::create(dir, options).unwrap();
        let mut w = index.writer(50_000_000, 2).unwrap();
        for l in lines {
            w.add(l).unwrap();
        }
        w.commit(Some("watermark=3")).unwrap();
        index
    }

    #[test]
    fn catalog_order_matched_words_snippets_and_payload() {
        let dir = tempfile::tempdir().unwrap();
        // ids out of catalog order: positions decide the order
        let index = build(
            dir.path(),
            IndexOptions::default(),
            &[
                line(1, 2, 2, 0, "פירוש על התורה"),
                line(4, 0, 1, 1, "והתורה ניתנה למשה"),
                line(6, 1, 1, 3, "תורה צוה לנו משה"),
            ],
        );
        assert_eq!(index.committed_payload().unwrap().as_deref(), Some("watermark=3"));

        let mut result = index
            .search(&SearchRequest {
                groups: vec![word("תורה", true)],
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<u64> = result.hits.iter().map(|h| h.line_id).collect();
        assert_eq!(ids, vec![4, 6, 1]);
        assert_eq!(result.hits[0].toc_entry_id, 101);

        let text = "והתורה ניתנה למשה";
        let s = result.session.locate(text).unwrap();
        assert_eq!(s.marks.len(), 3);
        let u16s: Vec<u16> = text.encode_utf16().collect();
        let (st, len, term) = (s.marks[0] as usize, s.marks[1] as usize, s.marks[2] as usize);
        assert_eq!(String::from_utf16(&u16s[st..st + len]).unwrap(), "והתורה");
        assert_eq!(result.session.all_terms()[term], "והתורה");
        assert!(s.is_short(result.session.context_words()));

        // widening over host-supplied neighbours keeps score/distance
        let w = result
            .session
            .locate_with_neighbours("בראשית ברא", text, "תורה צוה", &s)
            .unwrap();
        assert_eq!(w.score, s.score);
        assert!(w.window_word_count > s.window_word_count);
        assert!(!result.session.take_new_terms().is_empty());
    }

    #[test]
    fn distance_filter_without_positions() {
        // No positions stored: distance is validated from the text.
        let dir = tempfile::tempdir().unwrap();
        let index = build(
            dir.path(),
            IndexOptions {
                positions: false,
                keep_marks: false,
            },
            &[
                line(1, 0, 1, 0, "משה קבל תורה מסיני"),
                line(2, 1, 1, 1, "משה אמר דבר אחד ואחר כך תורה"),
            ],
        );
        let reopened = LineIndex::open(dir.path()).unwrap();
        assert!(!reopened.options().positions);
        let mut result = reopened
            .search(&SearchRequest {
                groups: vec![word("משה", false), word("תורה", false)],
                max_word_distance: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 2); // both contain both words
        assert!(result.session.locate("משה קבל תורה מסיני").is_some());
        assert!(result.session.locate("משה אמר דבר אחד ואחר כך תורה").is_none());
        drop(index);
    }

    #[test]
    fn update_and_wildcard_slots() {
        let dir = tempfile::tempdir().unwrap();
        let index = build(
            dir.path(),
            IndexOptions::default(),
            &[line(1, 0, 1, 0, "שלום עליכם"), line(2, 1, 1, 1, "שלומכם רב")],
        );
        let wildcard = |p: &str| GroupSpec::Wildcard { pattern: p.into() };
        let result = index
            .search(&SearchRequest {
                groups: vec![wildcard("שלו*")],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 2);

        // update line 2: delete + add new version
        let mut w = index.writer(50_000_000, 1).unwrap();
        w.delete(2);
        w.add(&line(2, 1, 1, 1, "ברכה רבה")).unwrap();
        w.commit(None).unwrap();
        let result = index
            .search(&SearchRequest {
                groups: vec![wildcard("שלו*")],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 1);

        // OR group: either word satisfies the slot; marks name the one present
        let mut result = index
            .search(&SearchRequest {
                groups: vec![GroupSpec::Any {
                    alternatives: vec![word("ברכה", false), word("שלום", false)],
                }],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 2);
        let s = result.session.locate("ברכה רבה").unwrap();
        assert_eq!(result.session.all_terms()[s.marks[2] as usize], "ברכה");

        // fuzzy slot: שלם~1 reaches שלום (one insertion)
        let result = index
            .search(&SearchRequest {
                groups: vec![GroupSpec::Fuzzy {
                    word: "שלם".into(),
                    distance: 1,
                }],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 1);

        // locate_many = locate one by one, term ids included
        let texts: Vec<String> = (0..100)
            .map(|i| if i % 2 == 0 { "ברכה רבה".to_string() } else { "רבה ברכה".to_string() })
            .collect();
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let mut a = index
            .search(&SearchRequest {
                groups: vec![word("ברכה", false)],
                ..Default::default()
            })
            .unwrap();
        let mut b = index
            .search(&SearchRequest {
                groups: vec![word("ברכה", false)],
                ..Default::default()
            })
            .unwrap();
        let many = a.session.locate_many(&refs);
        let one: Vec<_> = refs.iter().map(|t| b.session.locate(t)).collect();
        assert_eq!(many, one);
        assert_eq!(a.session.all_terms(), b.session.all_terms());

        // a rejected wildcard skips its slot rather than failing the search
        let result = index
            .search(&SearchRequest {
                groups: vec![wildcard("ש*"), word("שלום", false)],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 1);
    }
}
