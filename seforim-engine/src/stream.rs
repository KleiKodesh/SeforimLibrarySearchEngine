//! Hits decided from the index positions during the search, streamed out in
//! batches in the search's order (issue #21).
//!
//! On an index built with positions the postings already say where every
//! matched word of a hit sits, so the snippet locator's own rules (the
//! FtsLib distance / order filters, Otzaria's proximity, partial matching,
//! the phrase alternatives, the text-level negatives and required phrases)
//! run on those positions during the search
//! ([`SnippetWindowLocator::tokenize_occurrences`]): a hit that fails is
//! dropped before the host ever sees it, so the hit list, the totals and the
//! per-book counts are exact with no text read, and any page (hit 100 000)
//! can be fetched at once. The window is the word-tightest one
//! ([`SnippetWindowLocator::set_word_span`]); the session's locate calls use
//! the same rule on the displayed rows' texts, so they find the same window.
//!
//! The work runs on a producer thread that sends the hits in batches as they
//! are decided (the first one small, for a fast first page): a catalog-order
//! scan of the position-sorted index streams straight from the query's
//! scorer; any other order (relevance, generation, an index of several
//! segments) collects and sorts first, then streams the decision. The
//! search's cancel token stops the producer at its next chunk.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use hebrew_fts::SnippetWindowLocator;
use tantivy::fastfield::AliveBitSet;
use tantivy::query::Scorer;
use tantivy::{DocId, DocSet, Score, SegmentReader, TERMINATED};

use crate::positioned::{DocKey, PositionSource};
use crate::search::{passes_tokenized, CancelToken, Matcher, NegativeFilter, PlacedHit, PlacedSegmentCollector, TermMaps};
use crate::{Error, Result};

/// Candidate hits in the first chunk (a first page arrives fast), doubling
/// per chunk up to [`LAST_CHUNK`].
const FIRST_CHUNK: usize = 1024;
const LAST_CHUNK: usize = 65_536;

/// One batch of decided hits: the hits, each one's `(line id, segment,
/// doc)`, and their relevance scores (relevance order only).
pub(crate) struct Batch {
    pub hits: Vec<PlacedHit>,
    pub docs: Vec<(u64, u32, DocId)>,
    pub scores: Option<Vec<Score>>,
}

/// Where the producer takes its candidate hits from.
pub(crate) enum Source {
    /// A catalog-order scan of the one position-sorted segment: the query's
    /// scorer (its walks already done), and the collector's filters (books,
    /// sections, resume bound).
    Scorer {
        child: PlacedSegmentCollector,
        scorer: Box<dyn Scorer>,
        alive: Option<AliveBitSet>,
        max_doc: DocId,
    },
    /// Hits already collected and put in the requested order.
    Hits {
        hits: Vec<PlacedHit>,
        docs: Vec<(u64, u32, DocId)>,
        scores: Option<Vec<Score>>,
        at: usize,
    },
}

impl Source {
    pub(crate) fn scan(
        child: PlacedSegmentCollector,
        mut scorer: Box<dyn Scorer>,
        segment: &SegmentReader,
        start: DocId,
    ) -> Source {
        let max_doc = segment.max_doc();
        if scorer.doc() < start && start < max_doc {
            scorer.seek(start);
        }
        let alive = segment.alive_bitset().cloned();
        Source::Scorer { child, scorer, alive, max_doc: if start >= max_doc && start > 0 { 0 } else { max_doc } }
    }

    /// Up to `chunk` candidates (whole), and whether the source is spent.
    fn next(&mut self, chunk: usize, cancel: Option<&CancelToken>) -> Result<(Batch, bool)> {
        match self {
            Source::Scorer { child, scorer, alive, max_doc } => {
                let mut doc = if *max_doc == 0 { TERMINATED } else { scorer.doc() };
                let mut seen = 0u32;
                while doc != TERMINATED && child.collected() < chunk {
                    if alive.as_ref().is_none_or(|a| a.is_alive(doc)) {
                        child.collect_doc(doc);
                    }
                    seen = seen.wrapping_add(1);
                    if seen % 4096 == 0 {
                        CancelToken::check(cancel)?;
                    }
                    doc = scorer.advance();
                }
                if doc == TERMINATED {
                    *max_doc = 0;
                }
                let (hits, docs) = child.drain();
                // segment 0: a scan runs only on the one position-sorted
                // segment (`LineIndex::catalog_ordered`)
                let docs = hits.iter().zip(docs).map(|(h, d)| (h.line_id, 0, d)).collect();
                Ok((Batch { hits, docs, scores: None }, doc == TERMINATED))
            }
            Source::Hits { hits, docs, scores, at } => {
                let end = (*at + chunk).min(hits.len());
                let batch = Batch {
                    hits: hits[*at..end].to_vec(),
                    docs: docs[*at..end].to_vec(),
                    scores: scores.as_ref().map(|s| s[*at..end].to_vec()),
                };
                *at = end;
                Ok((batch, end == hits.len()))
            }
        }
    }
}

/// The session's matchers and filters, run on each hit's positions.
pub(crate) struct HitFilter {
    pub matchers: Vec<Matcher>,
    pub negative: Vec<NegativeFilter>,
    pub source: PositionSource,
    /// The `marked` line flag, when a vocalized negative must know whether
    /// a line carries a mark (Otzaria's `contains_attached_marks`).
    pub marked: Option<tantivy::schema::Field>,
    pub locator: SnippetWindowLocator,
}

impl HitFilter {
    /// Whether each of `docs` passes, from its positions alone.
    fn passes(&mut self, docs: &[(u64, u32, DocId)]) -> Result<Vec<bool>> {
        let keys: Vec<Option<DocKey>> = docs.iter().map(|&(_, s, d)| Some((s, d))).collect();
        let occurrences = self.source.occurrences(&keys)?;
        let marked = match self.marked {
            Some(field) => Some(self.source.marked_docs(field, &keys)?),
            None => None,
        };
        let maps = TermMaps { matchers: &self.source.matcher_maps, negatives: &self.source.negative_maps };
        let mut out = Vec::with_capacity(docs.len());
        for (i, o) in occurrences.iter().enumerate() {
            self.locator.tokenize_occurrences(o);
            let line_marked = marked.as_ref().is_none_or(|m| m[i]);
            out.push(passes_tokenized(&self.matchers, &self.negative, &mut self.locator, line_marked, maps));
        }
        Ok(out)
    }
}

/// Runs the producer: chunks of candidates, each decided (when `filter` is
/// given; otherwise every hit passes) and sent as one batch. Ends when the
/// source is spent, the receiver is gone, or the token is cancelled (an
/// error batch).
pub(crate) fn produce(
    mut source: Source,
    mut filter: Option<HitFilter>,
    cancel: Option<CancelToken>,
    tx: Sender<Result<Batch>>,
) {
    let timing = std::env::var_os("SEFORIM_ENGINE_TIMING").is_some();
    let t = Instant::now();
    let mut chunk = FIRST_CHUNK;
    let (mut candidates, mut kept) = (0usize, 0usize);
    loop {
        let step = (|| -> Result<(Batch, bool)> {
            CancelToken::check(cancel.as_ref())?;
            let (mut batch, done) = source.next(chunk, cancel.as_ref())?;
            candidates += batch.hits.len();
            if let Some(f) = filter.as_mut().filter(|_| !batch.hits.is_empty()) {
                let pass = f.passes(&batch.docs)?;
                let mut keep = pass.iter();
                batch.hits.retain(|_| *keep.next().unwrap());
                let mut keep = pass.iter();
                batch.docs.retain(|_| *keep.next().unwrap());
                if let Some(s) = batch.scores.as_mut() {
                    let mut keep = pass.iter();
                    s.retain(|_| *keep.next().unwrap());
                }
            }
            kept += batch.hits.len();
            Ok((batch, done))
        })();
        match step {
            Ok((batch, done)) => {
                if tx.send(Ok(batch)).is_err() || done {
                    break;
                }
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                break;
            }
        }
        chunk = (chunk * 2).min(LAST_CHUNK);
    }
    if timing {
        eprintln!(
            "  [timing] stream: {candidates} candidates, {kept} hits, {:.1} ms",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// The receiving end, held by the search result.
pub(crate) struct HitStream {
    pub rx: Receiver<Result<Batch>>,
}

impl HitStream {
    /// The next batch: Ok(Some) a batch, Ok(None) the stream is complete,
    /// Err(None) the wait timed out.
    pub(crate) fn recv(&self, deadline: Option<Instant>) -> std::result::Result<Option<Batch>, Option<Error>> {
        let got = match deadline {
            None => self.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(d) => self.rx.recv_timeout(d.saturating_duration_since(Instant::now()).max(Duration::ZERO)),
        };
        match got {
            Ok(Ok(batch)) => Ok(Some(batch)),
            Ok(Err(e)) => Err(Some(e)),
            Err(RecvTimeoutError::Disconnected) => Ok(None),
            Err(RecvTimeoutError::Timeout) => Err(None),
        }
    }
}
