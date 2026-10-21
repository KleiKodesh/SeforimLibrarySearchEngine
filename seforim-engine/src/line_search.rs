//! Otzaria's user regex of the `Line` kind (`user_regex.rs`
//! `run_line_search` / `candidate_query` / `verify_candidates` /
//! `scan_books`), ported as Otzaria does it, under the bare-minimum rule
//! (the index holds no text: the HOST supplies the lines' texts):
//!
//! 1. The pattern's required fixed words ([`LinePlan::clauses`]) become
//!    candidate lines from the index: a whole fragment is its term, a
//!    partial one every term containing it (`.*frag.*`, quote transparent,
//!    walked over the dictionary). A clause whose terms cover more than
//!    3,000,000 postings, or a partial fragment matching more than 50,000
//!    terms, narrows nothing worth reading and is dropped (Otzaria's
//!    `CLAUSE_POSTINGS_CAP` / `CLAUSE_MAX_TERMS`). The rest are ANDed,
//!    rarest first, inside the book filter. The candidates are the
//!    search's `hits`, in catalog order.
//! 2. No selective clause (or an index that keeps marks, whose vocalized
//!    terms the folded fragments cannot name): a FULL SCAN. `hits` is
//!    empty; the host streams every line in scope (in catalog order, as
//!    Otzaria reads the books) with [`SearchResult::line_scan_batch`];
//!    `LineRegexStatus::scope_lines` says how many lines that is.
//! 3. The engine runs the regex on Otzaria's line view of each text
//!    (`LineView`: markup, marks and quotes folded, a word boundary implied
//!    at the pattern's word edges), across all cores, and returns each
//!    line's matches as UTF-16 `(start, end)` ranges into the host's text.
//! 4. Matching lines are kept in feed order (catalog order when the host
//!    feeds so) up to 100,000 (Otzaria's `MAX_LINE_HITS`); past that the
//!    search is truncated (`SearchResult::truncated`). Every batch checks
//!    the search's cancel token.
//!
//! Drive a search through ONE feed: `line_match_batch` (candidates) or
//! `line_scan_batch` (full scan); `count_batch` is the candidate feed under
//! another name (same cursor, same cap), so do not mix it with
//! `line_match_batch`. A full scan is fed only through `line_scan_batch`.
//! `SnippetSession::locate` also works on a candidate (the whole line as the
//! window, every match marked) but feeds nothing: no cap, no progress.
//! `done` compares the lines fed with the index's lines in scope: a host
//! whose DB differs from its index also stops at the end of its own stream.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use hebrew_fts::line_regex::{LinePlan, LineView, CLAUSE_MAX_TERMS, CLAUSE_POSTINGS_CAP, MAX_LINE_HITS};
use hebrew_fts::ExactTermsQuery;
use serde::{Deserialize, Serialize};
use tantivy::query::{BooleanQuery, Occur, Query};
use tantivy::{Searcher, Term};

use crate::search::{CancelToken, HitCounts, PlacedCollector, PlacedHit, SearchRequest, SearchResult, Snippet, SnippetSession};
use crate::{Error, LineIndex, Result};

/// Where a line-regex search stands ([`SearchResult::line_regex`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineRegexStatus {
    /// No selective fixed word: the host streams every line in scope
    /// ([`SearchResult::line_scan_batch`]); otherwise it feeds the
    /// candidates' texts ([`SearchResult::line_match_batch`]).
    pub full_scan: bool,
    /// Lines to feed: the candidates, or every live line in scope.
    pub scope_lines: u64,
    /// Lines in scope fed so far (a full-scan line outside the book filter
    /// is not counted).
    pub fed: u64,
    /// Matching lines kept so far.
    pub matched: u64,
    /// The 100,000-line cap was reached: later lines are not matched.
    pub truncated: bool,
    /// Every line in scope was fed, or the cap was reached.
    pub done: bool,
}

/// The state of a line-regex search, kept in its [`SearchResult`].
pub(crate) struct LineRun {
    plan: Arc<LinePlan>,
    full_scan: bool,
    scope_lines: u64,
    fed: u64,
    matched: Vec<PlacedHit>,
    pub(crate) truncated: bool,
    books: Option<HashSet<u64>>,
    cancel: Option<CancelToken>,
}

impl LineRun {
    pub(crate) fn done(&self) -> bool {
        self.truncated || self.fed >= self.scope_lines
    }

    fn status(&self) -> LineRegexStatus {
        LineRegexStatus {
            full_scan: self.full_scan,
            scope_lines: self.scope_lines,
            fed: self.fed,
            matched: self.matched.len() as u64,
            truncated: self.truncated,
            done: self.done(),
        }
    }

    /// The counts of the lines matched so far (exact once done).
    pub(crate) fn counts(&self) -> HitCounts {
        let mut books = BTreeMap::<u64, u64>::new();
        for h in &self.matched {
            *books.entry(h.book_id).or_default() += 1;
        }
        HitCounts {
            total: self.matched.len() as u64,
            books: books.into_iter().collect(),
            validated: self.fed,
            exact: self.done(),
        }
    }

    /// Matches `texts` (one per `lines` entry) in parallel, then keeps the
    /// matching lines in order under the cap. Lines outside the book filter
    /// never match.
    fn feed(&mut self, lines: &[PlacedHit], texts: &[&str]) -> Result<Vec<Vec<(u32, u32)>>> {
        CancelToken::check(self.cancel.as_ref())?;
        let mut found = match_texts(&self.plan, texts, self.cancel.as_ref())?;
        for (hit, ranges) in lines.iter().zip(found.iter_mut()) {
            // a line outside the book filter is out of scope: not counted as fed
            if self.books.as_ref().is_some_and(|b| !b.contains(&hit.book_id)) {
                ranges.clear();
                continue;
            }
            self.fed += 1;
            if self.truncated {
                ranges.clear();
                continue;
            }
            if ranges.is_empty() {
                continue;
            }
            if self.matched.len() >= MAX_LINE_HITS {
                self.truncated = true;
                ranges.clear();
                continue;
            }
            self.matched.push(*hit);
        }
        Ok(found)
    }
}

/// The matches of each text, across all cores (contiguous chunks, results
/// in input order). Checks the cancel token per chunk.
fn match_texts(plan: &LinePlan, texts: &[&str], cancel: Option<&CancelToken>) -> Result<Vec<Vec<(u32, u32)>>> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(texts.len().max(1));
    if threads <= 1 || texts.len() < 32 {
        let mut view = LineView::default();
        return Ok(texts.iter().map(|t| plan.match_ranges(t, &mut view)).collect());
    }
    let chunk = texts.len().div_ceil(threads * 4);
    let parts: Vec<Result<Vec<Vec<(u32, u32)>>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = texts
            .chunks(chunk)
            .map(|part| {
                scope.spawn(move || -> Result<Vec<Vec<(u32, u32)>>> {
                    CancelToken::check(cancel)?;
                    let mut view = LineView::default();
                    Ok(part.iter().map(|t| plan.match_ranges(t, &mut view)).collect())
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("line regex worker panicked")).collect()
    });
    let mut out = Vec::with_capacity(texts.len());
    for p in parts {
        out.extend(p?);
    }
    CancelToken::check(cancel)?;
    Ok(out)
}

thread_local! {
    static VIEW: std::cell::RefCell<LineView> = std::cell::RefCell::new(LineView::default());
}

/// The snippet of a line-regex match: the whole line is the window (never
/// short: no widening), every match marked (`start`, `length`, term id 0 to
/// be numbered by the session). None when the line does not match.
pub(crate) fn line_snippet(plan: &LinePlan, text: &str) -> Option<Snippet> {
    let ranges = VIEW.with(|v| plan.match_ranges(text, &mut v.borrow_mut()));
    if ranges.is_empty() {
        return None;
    }
    let mut marks = Vec::with_capacity(ranges.len() * 3);
    for (s, e) in ranges {
        marks.extend_from_slice(&[s, e - s, 0]);
    }
    Some(Snippet {
        score: 0,
        word_distance: 0,
        window_word_count: u32::MAX as usize,
        start: 0,
        end: text.encode_utf16().count(),
        marks,
        flags: 0,
        break_at: None,
    })
}

/// True when the line matches (the count's test).
pub(crate) fn line_passes(plan: &LinePlan, text: &str) -> bool {
    VIEW.with(|v| {
        let mut v = v.borrow_mut();
        v.fill(text, false);
        plan.is_match(&v.text)
    })
}

impl SearchResult {
    /// The state of a user-regex search of Otzaria's `Line` kind; None for
    /// every other search.
    pub fn line_regex(&self) -> Option<LineRegexStatus> {
        self.line.as_ref().map(LineRun::status)
    }

    /// The matching lines kept so far (feed order), line-regex searches only.
    pub fn line_hits(&self) -> &[PlacedHit] {
        self.line.as_ref().map_or(&[], |l| l.matched.as_slice())
    }

    /// Candidate path: matches the texts of the next `texts.len()`
    /// candidates (`hits`, continuing from the previous batch). Returns each
    /// line's matches as UTF-16 `(start, end)` ranges (empty: no match, or
    /// past the cap).
    pub fn line_match_batch(&mut self, texts: &[&str]) -> Result<Vec<Vec<(u32, u32)>>> {
        let hits = &self.hits;
        let Some(line) = self.line.as_mut() else {
            return Err(Error::Unsupported("not a line-regex search".into()));
        };
        if line.full_scan {
            return Err(Error::Unsupported("a full-scan line regex takes its lines through line_scan_batch".into()));
        }
        let start = line.fed as usize;
        let end = start + texts.len();
        if end > hits.len() {
            return Err(Error::Unsupported(format!(
                "line_match_batch: {} texts past the last candidate ({} of {} fed)",
                texts.len(),
                start,
                hits.len()
            )));
        }
        let lines: Vec<PlacedHit> = hits[start..end].to_vec();
        line.feed(&lines, texts)
    }

    /// Full-scan path: matches lines the host streams itself (every line in
    /// scope, in catalog order), each with its placement. Lines outside the
    /// book filter never match.
    pub fn line_scan_batch(&mut self, lines: &[PlacedHit], texts: &[&str]) -> Result<Vec<Vec<(u32, u32)>>> {
        let Some(line) = self.line.as_mut() else {
            return Err(Error::Unsupported("not a line-regex search".into()));
        };
        if !line.full_scan {
            return Err(Error::Unsupported("a narrowed line regex takes its candidates through line_match_batch".into()));
        }
        if lines.len() != texts.len() {
            return Err(Error::Unsupported("line_scan_batch: one text per line".into()));
        }
        line.feed(lines, texts)
    }
}

impl LineIndex {
    /// Runs a user regex of Otzaria's `Line` kind (see the module docs).
    pub(crate) fn line_regex_search(
        &self,
        pattern: &str,
        request: &SearchRequest,
        cancel: Option<CancelToken>,
    ) -> Result<SearchResult> {
        let plan = Arc::new(hebrew_fts::user_regex::parse_line(pattern).map_err(|e| Error::Unsupported(e.to_string()))?);
        let reader = self.index.reader()?;
        let searcher = reader.searcher();
        let books: Option<HashSet<u64>> = request.books.as_ref().map(|b| b.iter().copied().collect());
        let candidates = if self.options.keep_marks {
            None
        } else {
            self.line_candidates(&plan, &searcher, cancel.as_ref())?
        };
        CancelToken::check(cancel.as_ref())?;
        let (hits, full_scan, scope_lines) = match candidates {
            Some(query) => {
                let collector = PlacedCollector { books: books.clone().map(Arc::new), drop_sections: None, keep_docs: false };
                let mut hits = searcher.search(query.as_ref(), &collector)?;
                hits.sort_unstable_by_key(|h| h.position);
                let n = hits.len() as u64;
                (hits, false, n)
            }
            None => (Vec::new(), true, scope_line_count(&searcher, books.as_ref())?),
        };
        let session =
            SnippetSession::for_line(plan.clone(), request.context_words, self.options.keep_marks, cancel.clone());
        let mut result = SearchResult::new(hits, session);
        result.line = Some(LineRun { plan, full_scan, scope_lines, fed: 0, matched: Vec::new(), truncated: false, books, cancel });
        Ok(result)
    }

    /// Otzaria's `candidate_query`: the lines holding every selective clause;
    /// None when no clause narrows the search.
    fn line_candidates(
        &self,
        plan: &LinePlan,
        searcher: &Searcher,
        cancel: Option<&CancelToken>,
    ) -> Result<Option<Box<dyn Query>>> {
        let field = self.fields.text;
        let mut clauses: Vec<(u64, Box<dyn Query>)> = Vec::new();
        for clause in &plan.clauses {
            let mut terms: BTreeSet<Vec<u8>> = BTreeSet::new();
            let mut postings = 0u64;
            let mut selective = true;
            for fragment in clause {
                if fragment.whole {
                    let term = Term::from_field_text(field, &fragment.text);
                    for reader in searcher.segment_readers() {
                        postings += u64::from(reader.inverted_index(field)?.doc_freq(&term)?);
                    }
                    terms.insert(fragment.text.as_bytes().to_vec());
                } else {
                    let automaton = hebrew_fts::user_regex::contains_automaton(&fragment.text)
                        .map_err(Error::Unsupported)?;
                    let (mut found, mut fragment_postings) = (BTreeSet::new(), 0u64);
                    let mut stopped = false;
                    'segments: for reader in searcher.segment_readers() {
                        let inverted = reader.inverted_index(field)?;
                        let mut stream = inverted.terms().search(&automaton).into_stream()?;
                        let mut scanned = 0u64;
                        while stream.advance() {
                            scanned += 1;
                            if scanned % 4096 == 0 {
                                CancelToken::check(cancel)?;
                            }
                            found.insert(stream.key().to_vec());
                            fragment_postings += u64::from(stream.value().doc_freq);
                            if found.len() >= CLAUSE_MAX_TERMS || fragment_postings >= CLAUSE_POSTINGS_CAP {
                                stopped = true;
                                break 'segments;
                            }
                        }
                    }
                    if stopped {
                        selective = false;
                        break;
                    }
                    postings += fragment_postings;
                    terms.extend(found);
                }
                if postings > CLAUSE_POSTINGS_CAP {
                    selective = false;
                    break;
                }
            }
            if selective {
                clauses.push((postings, Box::new(ExactTermsQuery::new(field, terms.into_iter().collect()))));
            }
        }
        if clauses.is_empty() {
            return Ok(None);
        }
        clauses.sort_by_key(|(postings, _)| *postings);
        let must: Vec<(Occur, Box<dyn Query>)> = clauses.into_iter().map(|(_, q)| (Occur::Must, q)).collect();
        Ok(Some(Box::new(BooleanQuery::new(must))))
    }
}

/// Live lines in scope (the book filter, or every line).
fn scope_line_count(searcher: &Searcher, books: Option<&HashSet<u64>>) -> Result<u64> {
    let Some(books) = books else { return Ok(searcher.num_docs()) };
    let mut n = 0u64;
    for segment in searcher.segment_readers() {
        let column = segment.fast_fields().u64("book_id")?;
        let alive = segment.alive_bitset();
        for doc in 0..segment.max_doc() {
            if alive.is_none_or(|a| a.is_alive(doc)) && column.first(doc).is_some_and(|b| books.contains(&b)) {
                n += 1;
            }
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use crate::{IndexOptions, LineDoc, LineIndex, SearchOptions};

    fn build(positions: bool, texts: &[(u64, &str)]) -> (tempfile::TempDir, LineIndex) {
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions { positions, keep_marks: false }).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        for (i, (book, text)) in texts.iter().enumerate() {
            w.add(&LineDoc {
                line_id: 100 + i as u64,
                position: i as u64,
                book_id: *book,
                text: (*text).into(),
                section_id: None,
                no_cross: false,
            })
                .unwrap();
        }
        w.commit(None).unwrap();
        (dir, index)
    }

    fn regex(q: &str) -> SearchOptions {
        SearchOptions { query: q.into(), user_regex: true, ..Default::default() }
    }

    #[test]
    fn candidates_then_text_matching() {
        let texts = [(1, "alpha beta gamma"), (1, "alpha gamma"), (2, "beta <b>alpha</b> x gamma"), (2, "delta")];
        for positions in [true, false] {
            let (_d, index) = build(positions, &texts);
            // two whole fixed words: candidates are lines holding both
            let mut r = index.search_options(&regex("/alpha .* gamma/")).unwrap();
            let status = r.line_regex().unwrap();
            assert!(!status.full_scan);
            let ids: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
            assert_eq!(ids, vec![100, 101, 102]);
            let t: Vec<&str> = r.hits.iter().map(|h| texts[(h.line_id - 100) as usize].1).collect();
            let found = r.line_match_batch(&t).unwrap();
            assert_eq!(found[0], vec![(0, 16)]);
            assert!(found[1].is_empty()); // the view keeps one space: ".*" between two spaces needs a word
            assert_eq!(found[2], vec![(8, 25)]); // UTF-16 range into the source, markup inside
            let status = r.line_regex().unwrap();
            assert!(status.done && !status.truncated);
            assert_eq!(r.counts().total, 2);
            // the session's locate marks the matches, the whole line as window
            let s = r.session.locate(texts[0].1).unwrap();
            assert_eq!((s.start, s.end, s.marks[0], s.marks[1]), (0, 16, 0, 16));
        }
    }

    #[test]
    fn full_scan_when_nothing_selective() {
        let texts = [(1, "a c"), (2, "x b c")];
        let (_d, index) = build(true, &texts);
        // one-letter fixed words are not fragments: a full scan
        let mut r = index.search_options(&regex("/\\w[ ]c/")).unwrap();
        let status = r.line_regex().unwrap();
        assert!(status.full_scan);
        assert_eq!(status.scope_lines, 2);
        assert!(r.hits.is_empty());
        let lines = vec![
            crate::PlacedHit { position: 0, line_id: 100, book_id: 1, next_line_id: None },
            crate::PlacedHit { position: 1, line_id: 101, book_id: 2, next_line_id: None },
        ];
        let found = r.line_scan_batch(&lines, &[texts[0].1, texts[1].1]).unwrap();
        assert_eq!(found[0], vec![(0, 3)]);
        assert_eq!(found[1], vec![(2, 5)]);
        assert_eq!(r.line_hits().len(), 2);
        assert!(r.line_regex().unwrap().done);
        // the book filter limits the scope
        let mut o = regex("/\\w[ ]c/");
        o.books = Some(vec![2]);
        let r = index.search_options(&o).unwrap();
        assert_eq!(r.line_regex().unwrap().scope_lines, 1);
    }
}
