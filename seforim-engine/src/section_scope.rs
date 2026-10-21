//! The same-section scope (Otzaria's `SearchScope::SameSection`,
//! `section_scope.rs`): every query word somewhere under one heading, the
//! words spread over the section's lines.
//!
//! The section of a line is the host's (`LineDoc::section_id`, from
//! seforim.db); the index keeps it as a small fast field, read only when the
//! scope is used. Sections are keyed by `(book_id, section_id)`, so section
//! ids may repeat across books; a line without a section id is a section of
//! its own (Otzaria: "under the same heading" then behaves as "in the same
//! line").
//!
//! One pass: each query word's walk yields its lines (the scorer that runs
//! the walk also fills its matched-terms sink); each word's lines become the
//! set of sections they fall in; the sets are intersected (all words) or
//! counted (Otzaria's word match: at least N of the words per section). The
//! hits are the lines that hold a query word inside a surviving section.
//! The negative query's section scope uses the same sets: a section holding
//! every negative word is dropped whole.

use std::collections::{HashMap, HashSet};

use tantivy::columnar::Column;
use tantivy::query::{ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, DocSet, Score, Searcher, SegmentReader, TERMINATED};

use crate::index::LineIndex;
use crate::search::CancelToken;
use crate::{Error, Result};

/// A section: `(book_id, section id, false)`, or `(book_id, line_id, true)`
/// for a line with no section id (a section of its own).
pub(crate) type SectionKey = (u64, u64, bool);

/// The fast columns a section key is read from.
pub(crate) struct SectionColumns {
    book: Column<u64>,
    line: Column<u64>,
    section: Column<u64>,
}

impl SectionColumns {
    pub(crate) fn open(reader: &SegmentReader) -> Result<SectionColumns> {
        let ff = reader.fast_fields();
        Ok(SectionColumns { book: ff.u64("book_id")?, line: ff.u64("line_id")?, section: ff.u64("section_id")? })
    }

    pub(crate) fn key(&self, doc: DocId) -> SectionKey {
        let book = self.book.first(doc).unwrap_or(0);
        match self.section.first(doc) {
            Some(section) => (book, section, false),
            None => (book, self.line.first(doc).unwrap_or(0), true),
        }
    }
}

/// An index built before the section field cannot serve the scope.
pub(crate) fn require(index: &LineIndex) -> Result<()> {
    if index.fields.section_id.is_none() {
        return Err(Error::Unsupported(
            "the same-section scope needs an index built with section ids: rebuild the index".into(),
        ));
    }
    Ok(())
}

/// The alive docs of `weight`, per segment ordinal, ascending. Building the
/// scorers runs the walks (and fills their sinks).
pub(crate) fn docs_of(weight: &dyn Weight, searcher: &Searcher, cancel: Option<&CancelToken>) -> Result<Vec<Vec<DocId>>> {
    let mut out = Vec::with_capacity(searcher.segment_readers().len());
    for segment in searcher.segment_readers() {
        check(cancel)?;
        let mut scorer = weight.scorer(segment, 1.0)?;
        let alive = segment.alive_bitset();
        let mut docs = Vec::new();
        let mut doc = scorer.doc();
        let mut seen = 0u32;
        while doc != TERMINATED {
            if alive.is_none_or(|a| a.is_alive(doc)) {
                docs.push(doc);
            }
            seen = seen.wrapping_add(1);
            if seen % 4096 == 0 {
                check(cancel)?;
            }
            doc = scorer.advance();
        }
        out.push(docs);
    }
    Ok(out)
}

fn check(cancel: Option<&CancelToken>) -> Result<()> {
    if cancel.is_some_and(|t| t.is_cancelled()) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

/// The sections a word's lines fall in.
pub(crate) fn sections_of(docs: &[Vec<DocId>], columns: &[SectionColumns]) -> HashSet<SectionKey> {
    let mut out = HashSet::new();
    for (segment, docs) in docs.iter().enumerate() {
        for &doc in docs {
            out.insert(columns[segment].key(doc));
        }
    }
    out
}

/// The sections holding at least `min` of the words (all of them when
/// `min` is their count): an intersection, or a count per section.
pub(crate) fn surviving(per_word: Vec<HashSet<SectionKey>>, min: usize) -> HashSet<SectionKey> {
    let n = per_word.len();
    if n == 0 {
        return HashSet::new();
    }
    if min >= n {
        let mut iter = per_word.into_iter();
        let mut all = iter.next().unwrap_or_default();
        for set in iter {
            all.retain(|k| set.contains(k));
        }
        return all;
    }
    let mut counts: HashMap<SectionKey, usize> = HashMap::new();
    for set in per_word {
        for key in set {
            *counts.entry(key).or_default() += 1;
        }
    }
    counts.into_iter().filter(|&(_, c)| c >= min).map(|(k, _)| k).collect()
}

/// The lines (per segment ordinal, ascending) that hold a word inside an
/// allowed section; every line holding a word when `allowed` is None.
pub(crate) fn lines_in(
    per_word: &[Vec<Vec<DocId>>],
    columns: &[SectionColumns],
    allowed: Option<&HashSet<SectionKey>>,
    segments: usize,
) -> Vec<Vec<DocId>> {
    let mut out: Vec<Vec<DocId>> = vec![Vec::new(); segments];
    for word in per_word {
        for (segment, docs) in word.iter().enumerate() {
            match allowed {
                Some(allowed) => out[segment].extend(docs.iter().copied().filter(|&d| allowed.contains(&columns[segment].key(d)))),
                None => out[segment].extend_from_slice(docs),
            }
        }
    }
    for docs in &mut out {
        docs.sort_unstable();
        docs.dedup();
    }
    out
}

/// A precomputed doc set as a query: the section scope's lines, per
/// segment, served as constant-score docs so the rest of the search
/// (phrase alternatives, the negative query, the book filter, collection)
/// composes with it as with any query.
#[derive(Clone)]
pub(crate) struct DocSetQuery {
    docs: std::sync::Arc<HashMap<tantivy::index::SegmentId, Vec<DocId>>>,
}

impl std::fmt::Debug for DocSetQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n: usize = self.docs.values().map(Vec::len).sum();
        write!(f, "DocSetQuery({n} docs)")
    }
}

impl DocSetQuery {
    /// `docs` per segment ordinal of `searcher`, ascending and distinct.
    pub(crate) fn new(searcher: &Searcher, docs: Vec<Vec<DocId>>) -> DocSetQuery {
        let map = searcher.segment_readers().iter().map(|r| r.segment_id()).zip(docs).collect();
        DocSetQuery { docs: std::sync::Arc::new(map) }
    }
}

impl Query for DocSetQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(DocSetWeight { docs: self.docs.clone() }))
    }
}

struct DocSetWeight {
    docs: std::sync::Arc<HashMap<tantivy::index::SegmentId, Vec<DocId>>>,
}

impl Weight for DocSetWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let docs = self.docs.get(&reader.segment_id()).cloned().unwrap_or_default();
        Ok(Box::new(ConstScorer::new(SortedDocs { docs, cursor: 0 }, boost)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let found = self.docs.get(&reader.segment_id()).is_some_and(|d| d.binary_search(&doc).is_ok());
        if !found {
            return Err(tantivy::TantivyError::InvalidArgument(format!("doc {doc} is not in the section scope")));
        }
        Ok(Explanation::new("DocSetQuery", 1.0))
    }
}

/// A sorted, distinct doc list as a doc set.
struct SortedDocs {
    docs: Vec<DocId>,
    cursor: usize,
}

impl DocSet for SortedDocs {
    fn advance(&mut self) -> DocId {
        self.cursor = (self.cursor + 1).min(self.docs.len());
        self.doc()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        if self.doc() < target {
            self.cursor += self.docs[self.cursor..].partition_point(|&d| d < target);
        }
        self.doc()
    }

    fn doc(&self) -> DocId {
        self.docs.get(self.cursor).copied().unwrap_or(TERMINATED)
    }

    fn size_hint(&self) -> u32 {
        (self.docs.len() - self.cursor) as u32
    }
}
