//! The user index: host documents that are not in the library (a user's
//! text files, PDFs, attached libraries), kept in a SEPARATE line index
//! with its own directory (issue #44). The library index stays prebuilt and
//! is never written by them.
//!
//! A document is a key chosen by the host (e.g. a file path), a host stamp
//! (e.g. a metadata hash, to tell whether the file changed) and its lines,
//! each with a host locator (e.g. a PDF page). The engine assigns the ids:
//! book id `USER_ID_BASE + n`, line id and position
//! `USER_ID_BASE + (n << USER_LINE_BITS) + line`, where `n` is the
//! document's number in the user index. The library's own ids must stay
//! below [`USER_ID_BASE`] (adding such a line is an error), so a hit's id
//! says where it comes from, and the user documents sort after the library.
//!
//! The registry (key -> number, stamp, line count, locators) is stored in
//! the user index's commit payload, so it lands atomically with the lines.
//! The line texts are stored in the user index (field `doc_text`): no
//! database holds them.
//!
//! A search runs over the library, the user index or both
//! ([`Sources`]): "both" is one search over the union of the two indexes'
//! segments (`tantivy::Searcher::union`), so every option, cap, filter and
//! count works as on one index.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The first id of the user index (2^51). Library line and book ids stay below.
pub const USER_ID_BASE: u64 = 1 << 51;
/// Bits of the line number inside a document's ids (16M lines per document).
pub const USER_LINE_BITS: u32 = 24;
/// The most documents the user index numbers (2^27).
pub const USER_MAX_DOCS: u64 = 1 << (51 - USER_LINE_BITS);

/// True for an id of the user index range.
pub fn is_user_id(id: u64) -> bool {
    (USER_ID_BASE..USER_ID_BASE << 1).contains(&id)
}

/// Which indexes a search reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sources {
    /// The library and, when one is attached, the user index (library hits first).
    #[default]
    All,
    /// The library index only.
    Library,
    /// The attached user index only.
    User,
}

/// One line of a host document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocLine {
    pub text: String,
    /// The host's locator of the line (e.g. a 1-based PDF page).
    #[serde(default)]
    pub locator: u64,
    /// Same-section scope, as [`crate::LineDoc::section_id`].
    #[serde(default)]
    pub section_id: Option<u64>,
    /// As [`crate::LineDoc::no_cross`].
    #[serde(default)]
    pub no_cross: bool,
}

/// A document of the user index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocInfo {
    pub key: String,
    pub stamp: String,
    pub book_id: u64,
    pub lines: u64,
}

/// Where a user line comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineSource {
    pub key: String,
    pub locator: u64,
    /// The line's number in its document (0-based).
    pub line: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub no: u64,
    pub stamp: String,
    pub lines: u64,
    /// Locator runs: `(first line, locator)`, ascending.
    pub loc: Vec<(u64, u64)>,
}

impl Entry {
    fn locator(&self, line: u64) -> u64 {
        match self.loc.partition_point(|r| r.0 <= line) {
            0 => 0,
            i => self.loc[i - 1].1,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Registry {
    pub next: u64,
    pub docs: BTreeMap<String, Entry>,
    /// Document number -> key (rebuilt on load).
    #[serde(skip)]
    pub by_no: BTreeMap<u64, String>,
}

impl Registry {
    pub(crate) fn reindex(mut self) -> Self {
        self.by_no = self.docs.iter().map(|(k, e)| (e.no, k.clone())).collect();
        self
    }

    pub(crate) fn infos(&self) -> Vec<DocInfo> {
        let mut out: Vec<DocInfo> = self
            .docs
            .iter()
            .map(|(k, e)| DocInfo { key: k.clone(), stamp: e.stamp.clone(), book_id: USER_ID_BASE + e.no, lines: e.lines })
            .collect();
        out.sort_by_key(|d| d.book_id);
        out
    }

    pub(crate) fn resolve(&self, line_id: u64) -> Option<LineSource> {
        if !is_user_id(line_id) {
            return None;
        }
        let rel = line_id - USER_ID_BASE;
        let (no, line) = (rel >> USER_LINE_BITS, rel & ((1 << USER_LINE_BITS) - 1));
        let key = self.by_no.get(&no)?;
        let e = &self.docs[key];
        (line < e.lines).then(|| LineSource { key: key.clone(), locator: e.locator(line), line })
    }

    /// Records `key` with its lines; returns its number and whether it
    /// replaces an indexed document.
    pub(crate) fn put(&mut self, key: &str, stamp: &str, lines: &[DocLine]) -> crate::Result<(u64, bool)> {
        if lines.len() as u64 >= 1 << USER_LINE_BITS {
            return Err(crate::Error::Unsupported(format!("a document has at most {} lines", (1u64 << USER_LINE_BITS) - 1)));
        }
        let (no, replaced) = match self.docs.get(key) {
            Some(e) => (e.no, true),
            None => {
                if self.next >= USER_MAX_DOCS {
                    return Err(crate::Error::Unsupported("the user index is full (document numbers used up)".into()));
                }
                self.next += 1;
                (self.next - 1, false)
            }
        };
        let mut loc: Vec<(u64, u64)> = Vec::new();
        for (i, l) in lines.iter().enumerate() {
            if loc.last().is_none_or(|r| r.1 != l.locator) {
                loc.push((i as u64, l.locator));
            }
        }
        self.docs.insert(key.to_string(), Entry { no, stamp: stamp.to_string(), lines: lines.len() as u64, loc });
        self.by_no.insert(no, key.to_string());
        Ok((no, replaced))
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<u64> {
        let e = self.docs.remove(key)?;
        self.by_no.remove(&e.no);
        Some(e.no)
    }
}

pub(crate) fn line_id(no: u64, line: u64) -> u64 {
    USER_ID_BASE + (no << USER_LINE_BITS) + line
}

/// The commit payload of an index holding documents: this prefix, then JSON
/// `{"host": <the host payload or null>, "docs": <registry>}`.
const PAYLOAD_PREFIX: &str = "\u{1}se-user-docs:";

#[derive(Serialize, Deserialize)]
struct Wrapped {
    host: Option<String>,
    docs: Registry,
}

/// Splits a raw commit payload into the host part and the registry.
pub(crate) fn split_payload(raw: Option<String>) -> crate::Result<(Option<String>, Option<Registry>)> {
    match raw {
        Some(p) if p.starts_with(PAYLOAD_PREFIX) => {
            let w: Wrapped = serde_json::from_str(&p[PAYLOAD_PREFIX.len()..])
                .map_err(|e| crate::Error::Unsupported(format!("malformed user documents registry: {e}")))?;
            Ok((w.host, Some(w.docs.reindex())))
        }
        other => Ok((other, None)),
    }
}

pub(crate) fn join_payload(host: Option<&str>, docs: Option<&Registry>) -> Option<String> {
    match docs {
        None => host.map(str::to_string),
        Some(d) => {
            let w = Wrapped { host: host.map(str::to_string), docs: d.clone() };
            Some(format!("{PAYLOAD_PREFIX}{}", serde_json::to_string(&w).expect("registry serializes")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GroupSpec, IndexOptions, LineDoc, LineIndex, SearchOptions, SearchRequest, WalkOptionsSpec};
    use std::sync::Arc;

    fn word(w: &str) -> GroupSpec {
        GroupSpec::Word { word: w.into(), options: WalkOptionsSpec::default() }
    }

    fn lines(texts: &[(&str, u64)]) -> Vec<DocLine> {
        texts.iter().map(|&(t, page)| DocLine { text: t.into(), locator: page, section_id: None, no_cross: false }).collect()
    }

    fn library(dir: &std::path::Path) -> LineIndex {
        let index = LineIndex::create(dir, IndexOptions::default()).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        for (i, t) in ["alpha beta", "gamma alpha", "delta"].iter().enumerate() {
            let i = i as u64;
            w.add(&LineDoc { line_id: 10 + i, position: i, book_id: 1, text: (*t).into(), section_id: None, no_cross: false }).unwrap();
        }
        w.commit(Some("lib")).unwrap();
        index
    }

    fn ids(index: &LineIndex, request: SearchRequest) -> Vec<u64> {
        index.search(&request).unwrap().hits.iter().map(|h| h.line_id).collect()
    }

    #[test]
    fn user_documents_are_searched_with_the_library_or_alone() {
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let lib = library(d1.path());
        let user = Arc::new(LineIndex::create_user(d2.path(), IndexOptions::default()).unwrap());
        {
            let mut w = user.writer(50_000_000, 1).unwrap();
            assert!(w.add(&LineDoc { line_id: USER_ID_BASE, position: 0, book_id: 1, text: "x".into(), section_id: None, no_cross: false }).is_err());
            let a = w.put_document("c:/a.pdf", "s1", &lines(&[("alpha one", 1), ("two", 1), ("alpha three", 2)])).unwrap();
            let b = w.put_document("c:/b.txt", "s2", &lines(&[("beta alpha", 0)])).unwrap();
            assert_eq!((a, b), (USER_ID_BASE, USER_ID_BASE + 1));
            w.commit(Some("host")).unwrap();
        }
        assert_eq!(user.committed_payload().unwrap().as_deref(), Some("host"));
        // the library never takes documents
        assert!(lib.writer(50_000_000, 1).unwrap().put_document("k", "s", &[]).is_err());

        let q = |sources| SearchRequest { groups: vec![word("alpha")], sources, ..Default::default() };
        assert_eq!(ids(&lib, q(Sources::All)), vec![10, 11]); // nothing attached
        lib.attach_user_index(Some(user.clone())).unwrap();
        let all = ids(&lib, q(Sources::All));
        assert_eq!(all, vec![10, 11, line_id(0, 0), line_id(0, 2), line_id(1, 0)]);
        assert_eq!(ids(&lib, q(Sources::Library)), vec![10, 11]);
        assert_eq!(ids(&lib, q(Sources::User)), vec![line_id(0, 0), line_id(0, 2), line_id(1, 0)]);
        // per-book counts and a book filter work across both
        let r = lib.search(&SearchRequest { books: Some(vec![USER_ID_BASE + 1, 1]), ..q(Sources::All) }).unwrap();
        assert_eq!(r.counts().books, vec![(1, 2), (USER_ID_BASE + 1, 1)]);
        // options go through SearchOptions too
        let o = SearchOptions { query: "alpha".into(), sources: Sources::User, ..Default::default() };
        assert_eq!(lib.search_options(&o).unwrap().hits.len(), 3);

        let src = lib.line_sources(&[line_id(0, 2), line_id(0, 1), 10, line_id(1, 0)]).unwrap();
        assert_eq!(src[0], Some(LineSource { key: "c:/a.pdf".into(), locator: 2, line: 2 }));
        assert_eq!(src[1].as_ref().unwrap().locator, 1);
        assert_eq!(src[2], None);
        assert_eq!(src[3].as_ref().unwrap().key, "c:/b.txt");
        assert_eq!(lib.line_texts(&[line_id(0, 2), 10]).unwrap(), vec![Some("alpha three".into()), None]);

        // replace keeps the book id, delete drops it, the list follows
        {
            let mut w = user.writer(50_000_000, 1).unwrap();
            assert_eq!(w.put_document("c:/a.pdf", "s3", &lines(&[("gamma", 5)])).unwrap(), USER_ID_BASE);
            assert_eq!(w.delete_documents(&["c:/b.txt".into(), "nope".into()]).unwrap(), 1);
            w.commit(None).unwrap();
        }
        let docs = lib.documents().unwrap();
        assert_eq!(docs, vec![DocInfo { key: "c:/a.pdf".into(), stamp: "s3".into(), book_id: USER_ID_BASE, lines: 1 }]);
        assert_eq!(ids(&lib, q(Sources::All)), vec![10, 11]);
        let g = SearchRequest { groups: vec![word("gamma")], ..Default::default() };
        assert_eq!(ids(&lib, g), vec![11, line_id(0, 0)]);
        // a merge keeps the registry (and the host payload)
        user.writer(50_000_000, 1).unwrap().merge_all().unwrap();
        assert_eq!(user.documents().unwrap().len(), 1);
        assert_eq!(user.committed_payload().unwrap(), None);
        // a rollback drops queued document changes
        {
            let mut w = user.writer(50_000_000, 1).unwrap();
            w.put_document("c:/z", "s", &lines(&[("zeta", 0)])).unwrap();
            w.rollback().unwrap();
            w.commit(None).unwrap();
        }
        assert_eq!(user.documents().unwrap().len(), 1);
        lib.attach_user_index(None).unwrap();
        assert!(lib.search(&q(Sources::User)).is_err());
    }

    /// Cross-line over the library and a user index: the user positions sit
    /// at 2^51 + (document << 24) + line, so the cross-line bits over a range
    /// reaching them must not be dense (it aborted on a 2^48-byte allocation).
    #[test]
    fn cross_line_reaches_user_documents() {
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let lib = library(d1.path());
        let user = Arc::new(LineIndex::create_user(d2.path(), IndexOptions::default()).unwrap());
        {
            let mut w = user.writer(50_000_000, 1).unwrap();
            w.put_document("c:/a.txt", "s", &lines(&[("x delta", 0), ("alpha y", 0)])).unwrap();
            w.put_document("c:/b.txt", "s", &lines(&[("delta", 0), ("z", 0), ("alpha", 0)])).unwrap();
            w.commit(None).unwrap();
        }
        lib.attach_user_index(Some(user)).unwrap();
        let q = |sources| SearchOptions { query: "delta alpha".into(), cross_line: true, sources, ..Default::default() };
        let hits = |sources| -> Vec<(u64, Option<u64>)> {
            lib.search_options(&q(sources)).unwrap().hits.iter().map(|h| (h.line_id, h.next_line_id)).collect()
        };
        // library lines 11 + 12 pair; its last line "delta" never continues
        // into a user document, nor one document into the next
        let user = (line_id(0, 0), Some(line_id(0, 1)));
        assert_eq!(hits(Sources::All), vec![(11, Some(12)), user]);
        assert_eq!(hits(Sources::User), vec![user]);
        assert_eq!(hits(Sources::Library), vec![(11, Some(12))]);
    }

    #[test]
    fn fm_expansions_merge_both_indexes() {
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let lib = library(d1.path());
        lib.build_fm_sidecar().unwrap();
        let user = Arc::new(LineIndex::create_user(d2.path(), IndexOptions::default()).unwrap());
        let mut w = user.writer(50_000_000, 1).unwrap();
        w.put_document("k", "s", &lines(&[("malpha", 3)])).unwrap();
        w.merge_all().unwrap();
        assert!(user.build_fm_sidecar().unwrap());
        lib.attach_user_index(Some(user.clone())).unwrap();
        let req = SearchRequest { groups: vec![GroupSpec::Wildcard { pattern: "*lpha".into(), ignore_quotes: false }], ..Default::default() };
        let with_fm = ids(&lib, req.clone());
        assert_eq!(with_fm, vec![10, 11, line_id(0, 0)]);
        lib.test_only_disable_fm.store(true, std::sync::atomic::Ordering::Relaxed);
        user.test_only_disable_fm.store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(ids(&lib, req.clone()), with_fm);
        // one cap over both indexes: the sidecars agree with the walk
        let capped = SearchRequest { max_expanded_terms: 1, ..req };
        let walked = ids(&lib, capped.clone());
        lib.test_only_disable_fm.store(false, std::sync::atomic::Ordering::Relaxed);
        user.test_only_disable_fm.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(ids(&lib, capped), walked);
        assert!(walked.len() < with_fm.len());
    }
}
