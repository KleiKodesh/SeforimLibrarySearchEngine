//! Snippets located from the index positions (an index built with
//! positions): the engine already knows, from the postings, at which word
//! positions of a line the query's matched terms sit, so the locator need
//! not hash every word of the text to find them.
//!
//! Per batch, each line's doc is known (a hit's doc is kept from the
//! search; any other line is found by its `line_id`), and every matched term
//! of the session (the sinks' terms of the main query, the phrase
//! alternatives and the text-level negatives, numbered once) is read from
//! the postings ONCE per segment, seeking through the batch's sorted doc
//! ids. Each line then gets its sorted `(position, term id)` list, and the
//! locator scans the text once, without normalized strings or hashing,
//! stopping after the last word a window can use
//! ([`SnippetWindowLocator::tokenize_positioned`](hebrew_fts::SnippetWindowLocator::tokenize_positioned)).
//! The window rules are the same code as the text path, so the snippets are
//! identical; a text that disagrees with its positions falls back to the
//! text path.

use std::collections::HashMap;
use std::sync::Arc;

use hebrew_fts::PreparedQueryGroups;
use tantivy::postings::{Postings, TermInfo};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocId, DocSet, Searcher, Term, TERMINATED};

use crate::Result;

/// `u32::MAX`: no ordinal.
pub(crate) const NONE: u32 = u32::MAX;

/// [`PositionSource::pays_off`]: reading the positions pays off on a batch
/// whose texts average more than `MIN_LINE_BYTES + BYTES_PER_TERM * terms`
/// bytes. The text work it saves (hashing every word, the scan past the last
/// matched word) grows with the line; reading the postings grows with the
/// matched terms. Measured on the full library (README, "Snippets from
/// positions"): long lines with up to a few hundred terms win (15-28% at
/// 4000 lines, up to 14% at 256), short lines (a common single word) and
/// broad expansions (a capped walk, a word with all its prefixes) lose.
const MIN_LINE_BYTES: usize = 1500;
const BYTES_PER_TERM: usize = 2;

/// A line's live doc: (segment ordinal, doc id).
pub(crate) type DocKey = (u32, DocId);

/// One line's matched words: sorted `(position, term id)`.
pub(crate) type Occurrences = Vec<(u32, u32)>;

/// A search's matched terms and the postings to read their positions from.
#[derive(Clone)]
pub(crate) struct PositionSource {
    searcher: Searcher,
    text: Field,
    line_id: Field,
    /// Every matched term of the session, by term id.
    pub terms: Arc<[String]>,
    /// Per matcher: term id -> ordinal in its prepared groups (or NONE).
    pub matcher_maps: Arc<[Vec<u32>]>,
    /// Per negative filter, the same.
    pub negative_maps: Arc<[Vec<u32>]>,
    /// Each hit's (line id, segment, doc), aligned with the hits.
    pub hit_docs: Vec<(u64, u32, DocId)>,
    /// Hits located from the text because their positions could not be
    /// used (a line not in the index, a text that disagrees with them).
    pub fallbacks: Arc<std::sync::atomic::AtomicUsize>,
    /// Per segment, each term's info (looked up on first use).
    term_infos: Vec<Option<Arc<[Option<TermInfo>]>>>,
}

impl PositionSource {
    pub(crate) fn new<'a>(
        searcher: Searcher,
        text: Field,
        line_id: Field,
        matchers: impl Iterator<Item = &'a PreparedQueryGroups>,
        negatives: impl Iterator<Item = &'a PreparedQueryGroups>,
    ) -> PositionSource {
        let matchers: Vec<&PreparedQueryGroups> = matchers.collect();
        let negatives: Vec<&PreparedQueryGroups> = negatives.collect();
        let mut ids: HashMap<&str, u32> = HashMap::new();
        let mut terms: Vec<String> = Vec::new();
        for p in matchers.iter().chain(&negatives) {
            for o in 0..p.term_count() {
                let t = p.term_at(o);
                if !ids.contains_key(t) {
                    ids.insert(t, terms.len() as u32);
                    terms.push(t.to_string());
                }
            }
        }
        let map = |p: &PreparedQueryGroups| {
            let mut m = vec![NONE; terms.len()];
            for o in 0..p.term_count() {
                m[ids[p.term_at(o)] as usize] = o as u32;
            }
            m
        };
        let matcher_maps: Vec<Vec<u32>> = matchers.iter().map(|p| map(p)).collect();
        let negative_maps: Vec<Vec<u32>> = negatives.iter().map(|p| map(p)).collect();
        let segments = searcher.segment_readers().len();
        PositionSource {
            searcher,
            text,
            line_id,
            terms: terms.into(),
            matcher_maps: matcher_maps.into(),
            negative_maps: negative_maps.into(),
            hit_docs: Vec::new(),
            fallbacks: Arc::default(),
            term_infos: vec![None; segments],
        }
    }

    /// Each line's live doc, or None (not in the index). `lines`: (line id,
    /// the hit index the line is known to be, if any).
    pub(crate) fn docs_of(&self, lines: &[(u64, Option<usize>)]) -> Result<Vec<Option<DocKey>>> {
        let mut out: Vec<Option<DocKey>> = lines
            .iter()
            .map(|&(id, hit)| hit.and_then(|h| self.hit_docs.get(h)).filter(|d| d.0 == id).map(|d| (d.1, d.2)))
            .collect();
        let mut order: Vec<usize> = (0..lines.len()).filter(|&i| out[i].is_none()).collect();
        if order.is_empty() {
            return Ok(out);
        }
        // dictionary order: neighbouring lookups share the cached blocks
        order.sort_unstable_by_key(|&i| lines[i].0);
        for (ord, seg) in self.searcher.segment_readers().iter().enumerate() {
            let inv = seg.inverted_index(self.line_id)?;
            let alive = seg.alive_bitset();
            for &i in &order {
                if out[i].is_some() {
                    continue;
                }
                let term = Term::from_field_u64(self.line_id, lines[i].0);
                if let Some(mut p) = inv.read_postings(&term, IndexRecordOption::Basic)? {
                    let mut d = p.doc();
                    while d != TERMINATED {
                        if alive.is_none_or(|a| a.is_alive(d)) {
                            out[i] = Some((ord as u32, d));
                            break;
                        }
                        d = p.advance();
                    }
                }
            }
        }
        Ok(out)
    }

    /// Each term's info in segment `ord` (looked up once per search).
    fn term_infos(&mut self, ord: usize) -> Result<Arc<[Option<TermInfo>]>> {
        if let Some(infos) = &self.term_infos[ord] {
            return Ok(infos.clone());
        }
        let inv = self.searcher.segment_readers()[ord].inverted_index(self.text)?;
        let mut infos = Vec::with_capacity(self.terms.len());
        for t in self.terms.iter() {
            infos.push(inv.get_term_info(&Term::from_field_text(self.text, t))?);
        }
        let infos: Arc<[Option<TermInfo>]> = infos.into();
        self.term_infos[ord] = Some(infos.clone());
        Ok(infos)
    }

    /// Whether reading the positions should be faster than the text path
    /// for a batch of `lines` texts of `bytes` in all (see [`MIN_LINE_BYTES`]).
    pub(crate) fn pays_off(&self, lines: usize, bytes: usize) -> bool {
        bytes > lines.max(1) * (MIN_LINE_BYTES + BYTES_PER_TERM * self.terms.len())
    }

    /// Whether each of `docs` carries the `marked` line flag
    /// ([`MARKED_LINE`](crate::index::MARKED_LINE): the line holds an
    /// attached mark), read from the flag's postings (false for None).
    pub(crate) fn marked_docs(&self, field: Field, docs: &[Option<DocKey>]) -> Result<Vec<bool>> {
        let mut out = vec![false; docs.len()];
        for (ord, seg) in self.searcher.segment_readers().iter().enumerate() {
            let mut wanted: Vec<(DocId, usize)> = docs
                .iter()
                .enumerate()
                .filter_map(|(i, d)| d.filter(|d| d.0 == ord as u32).map(|d| (d.1, i)))
                .collect();
            if wanted.is_empty() {
                continue;
            }
            wanted.sort_unstable();
            let inv = seg.inverted_index(field)?;
            let term = Term::from_field_u64(field, crate::index::MARKED_LINE);
            let Some(mut p) = inv.read_postings(&term, IndexRecordOption::Basic)? else { continue };
            for &(doc, slot) in &wanted {
                let mut d = p.doc();
                if d < doc {
                    d = p.seek(doc);
                }
                if d == TERMINATED {
                    break;
                }
                out[slot] = d == doc;
            }
        }
        Ok(out)
    }

    /// Every matched term's positions in each of `docs`: per doc, sorted
    /// `(position, term id)` pairs (empty for None). Per segment, each
    /// term's postings are walked once over the batch's sorted docs; many
    /// terms are split across threads by term.
    pub(crate) fn occurrences(&mut self, docs: &[Option<DocKey>]) -> Result<Vec<Occurrences>> {
        let mut out: Vec<Occurrences> = vec![Vec::new(); docs.len()];
        let segments = self.searcher.segment_readers().len();
        for ord in 0..segments {
            // this segment's docs, sorted, with the slots wanting them
            let mut wanted: Vec<(DocId, usize)> = docs
                .iter()
                .enumerate()
                .filter_map(|(i, d)| d.filter(|d| d.0 == ord as u32).map(|d| (d.1, i)))
                .collect();
            if wanted.is_empty() {
                continue;
            }
            wanted.sort_unstable();
            let seg = &self.searcher.segment_readers()[ord];
            let inv = seg.inverted_index(self.text)?;
            let infos = self.term_infos(ord)?;
            let present: Vec<(u32, &TermInfo)> =
                infos.iter().enumerate().filter_map(|(id, i)| i.as_ref().map(|i| (id as u32, i))).collect();
            // Work split across cores in two ways: by term (opening a posting
            // list is the fixed cost: at least 64 terms a part) and by doc
            // range (each part reopens its terms: at least 512 docs a part).
            let threads = crate::pools::search_pool().current_num_threads();
            // (a small batch reads on the caller's thread; a big one on the
            // persistent search pool)
            let term_parts = if present.len() < 128 { 1 } else { threads.min(present.len() / 64) };
            let doc_parts = if wanted.len() < 1024 { 1 } else { (threads / term_parts).min(wanted.len() / 512).max(1) };
            let inv = &inv;
            let read = |terms: &[(u32, &TermInfo)], wanted: &[(DocId, usize)]| -> Result<Vec<(usize, u32, u32)>> {
                let mut found: Vec<(usize, u32, u32)> = Vec::new();
                let mut buf: Vec<u32> = Vec::new();
                for &(id, info) in terms {
                    let mut p = inv.read_postings_from_terminfo(info, IndexRecordOption::WithFreqsAndPositions)?;
                    let mut w = 0;
                    while w < wanted.len() {
                        let target = wanted[w].0;
                        let mut d = p.doc();
                        if d < target {
                            d = p.seek(target);
                        }
                        if d == TERMINATED {
                            break;
                        }
                        if d == target {
                            p.positions(&mut buf);
                            // the same doc may be wanted by several slots
                            while w < wanted.len() && wanted[w].0 == target {
                                found.extend(buf.iter().map(|&pos| (wanted[w].1, pos, id)));
                                w += 1;
                            }
                        } else {
                            // skip the wanted docs the postings jumped over
                            w += wanted[w..].partition_point(|x| x.0 < d);
                        }
                    }
                }
                Ok(found)
            };
            let found: Vec<Vec<(usize, u32, u32)>> = if term_parts * doc_parts <= 1 {
                vec![read(&present, &wanted)?]
            } else {
                let term_chunk = present.len().div_ceil(term_parts);
                let doc_chunk = wanted.len().div_ceil(doc_parts);
                let read = &read;
                let wanted = &wanted;
                let parts: Vec<_> = present
                    .chunks(term_chunk)
                    .flat_map(|t| wanted.chunks(doc_chunk).map(move |d| (t, d)))
                    .collect();
                use rayon::prelude::*;
                crate::pools::search_pool()
                    .install(|| parts.par_iter().map(|&(t, d)| read(t, d)).collect::<Result<Vec<_>>>())?
            };
            for (slot, pos, id) in found.into_iter().flatten() {
                out[slot].push((pos, id));
            }
        }
        for o in &mut out {
            // a term twice at one position (a paired reading repeating its
            // word) is one occurrence: each word there checks it
            o.sort_unstable();
            o.dedup();
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::{HitLines, IndexOptions, LineDoc, LineIndex, SearchRequest, WordWindow};

    const W: [&str; 6] = [
        "\u{05E9}\u{05DC}\u{05D5}\u{05DD}",
        "\u{05D1}\u{05D9}\u{05EA}",
        "\u{05DE}\u{05E9}\u{05D4}",
        "\u{05EA}\u{05D5}\u{05E8}\u{05D4}",
        "\u{05D9}\u{05E6}\u{05D7}\u{05E7}",
        "\u{05D0}\u{05D1}",
    ];

    /// A deterministic generator (no external crate).
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    /// One word in one of the shapes the scanner treats specially.
    fn token(r: &mut Lcg) -> String {
        let w = W[r.next(W.len())];
        let first: String = w.chars().take(1).collect();
        let rest: String = w.chars().skip(1).collect();
        match r.next(16) {
            0 => format!("\u{05D5}{w}"),
            1 => format!("\u{05D4}{w}"),
            2 => format!("{first}\u{05B8}{rest}"),
            3 => format!("{first}\"{rest}"),
            4 => format!("<b>{w}</b>"),
            5 => format!("{first}<i>{rest}</i>"),
            6 => format!("({w}) [{}]", W[r.next(W.len())]),
            7 => "&nbsp;".into(),
            8 => format!("{w},"),
            9 => "12".into(),
            10 => "\u{05D0}".repeat(70), // a word over 128 bytes: not indexed, keeps its position
            11 => format!("{w}\u{05BE}{}", W[r.next(W.len())]),
            12 => format!("{w}'"),
            13 => "<br>".into(),
            _ => w.into(),
        }
    }

    fn corpus(seed: u64, n: usize) -> Vec<LineDoc> {
        let mut r = Lcg(seed);
        (0..n)
            .map(|i| {
                let words = r.next(40);
                let text: Vec<String> = (0..words).map(|_| token(&mut r)).collect();
                LineDoc {
                    line_id: 1000 + i as u64,
                    position: i as u64,
                    book_id: 1 + (i / 50) as u64,
                    text: text.join(if r.next(5) == 0 { "  " } else { " " }),
                    section_id: None,
                    no_cross: false,
                }
            })
            .collect()
    }

    fn requests() -> Vec<String> {
        let w = |i: usize| format!("{{\"kind\":\"word\",\"word\":\"{}\"}}", W[i]);
        let wp = |i: usize| format!("{{\"kind\":\"word\",\"word\":\"{}\",\"options\":{{\"grammar_prefixes\":true}}}}", W[i]);
        let mut out = vec![
            format!("{{\"groups\":[{}]}}", wp(0)),
            format!("{{\"groups\":[{},{}]}}", w(0), w(1)),
            format!("{{\"groups\":[{},{}],\"max_word_distance\":2}}", wp(0), w(1)),
            format!("{{\"groups\":[{},{}],\"require_ordered\":true}}", w(2), w(3)),
            "{\"groups\":[{\"kind\":\"wildcard\",\"pattern\":\"\u{05D9}\u{05E6}*\"}]}".to_string(),
            "{\"groups\":[{\"kind\":\"wildcard\",\"pattern\":\"*\u{05D5}\u{05DD}\"}]}".to_string(),
            format!("{{\"groups\":[{{\"kind\":\"fuzzy\",\"word\":\"{}\",\"distance\":1}}]}}", W[3]),
            format!("{{\"groups\":[{{\"kind\":\"any\",\"alternatives\":[{},{}]}},{}]}}", w(2), w(5), w(1)),
            format!("{{\"groups\":[{},{},{}],\"min_groups\":2}}", w(0), w(1), w(4)),
            format!("{{\"groups\":[{},{}],\"proximity\":{{\"mode\":\"ordered\",\"gaps\":[1]}}}}", w(0), w(1)),
            format!("{{\"groups\":[{},{}],\"proximity\":{{\"mode\":\"any_order\",\"distance\":2}}}}", w(1), w(3)),
            format!("{{\"groups\":[{}],\"exclude\":{{\"groups\":[{}]}}}}", w(0), w(2)),
            format!("{{\"groups\":[{}],\"exclude\":{{\"groups\":[{},{}],\"max_word_distance\":1}}}}", wp(1), w(2), w(3)),
            format!("{{\"groups\":[{}],\"phrase_alternatives\":[[{},{}]]}}", w(5), w(2), w(3)),
            format!("{{\"groups\":[{},{}],\"gaps\":[2],\"require_ordered\":true}}", w(0), w(4)),
            format!("{{\"groups\":[{},{},{}],\"max_word_distance\":0}}", w(2), w(0), w(1)),
        ];
        // a short context (the scan stops sooner) and a long one
        let more: Vec<String> = out
            .iter()
            .flat_map(|r| {
                let body = &r[..r.len() - 1];
                [format!("{body},\"context_words\":1}}"), format!("{body},\"context_words\":25}}")]
            })
            .collect();
        out.extend(more);
        out
    }

    fn check(options: IndexOptions, seed: u64) {
        let dir = tempfile::tempdir().unwrap();
        let lines = corpus(seed, 400);
        let index = LineIndex::create(dir.path(), options).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        for l in &lines {
            w.add(l).unwrap();
        }
        w.commit(None).unwrap();
        let text_of = |id: u64| lines[(id - 1000) as usize].text.as_str();
        let mut used = 0;
        for json in requests() {
            let request: SearchRequest = serde_json::from_str(&json).unwrap();
            let mut a = index.search(&request).unwrap();
            let mut b = index.search(&request).unwrap();
            b.session.force_positions(true);
            assert!(b.session.locates_from_positions(), "{}", json.escape_unicode());
            let texts: Vec<&str> = a.hits.iter().map(|h| text_of(h.line_id)).collect();
            let old = a.session.locate_many(&texts);
            let one: Vec<[(u64, &str); 1]> = b.hits.iter().map(|h| [(h.line_id, text_of(h.line_id))]).collect();
            let items: Vec<HitLines<'_>> =
                one.iter().enumerate().map(|(i, l)| HitLines { hit: Some(i), lines: &l[..] }).collect();
            let new = b.session.locate_lines(&items);
            assert_eq!(old, new, "{}", json.escape_unicode());
            assert_eq!(a.session.all_terms(), b.session.all_terms());
            assert_eq!(b.session.positioned_fallbacks(), 0, "{}", json.escape_unicode());
            used += new.iter().flatten().count();

            // widening: the hit with up to two lines each side (non-hits looked up by id)
            let around: Vec<Vec<(u64, &str)>> = b
                .hits
                .iter()
                .map(|h| {
                    let at = (h.line_id - 1000) as usize;
                    (at.saturating_sub(2)..(at + 3).min(lines.len()))
                        .map(|k| (lines[k].line_id, lines[k].text.as_str()))
                        .collect()
                })
                .collect();
            let joined: Vec<String> = around
                .iter()
                .map(|l| l.iter().map(|x| x.1).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" "))
                .collect();
            let refs: Vec<&str> = joined.iter().map(|s| s.as_str()).collect();
            let old = a.session.locate_many(&refs);
            let items: Vec<HitLines<'_>> =
                around.iter().enumerate().map(|(i, l)| HitLines { hit: Some(i), lines: l }).collect();
            let new = b.session.locate_lines(&items);
            assert_eq!(old, new, "widened {}", json.escape_unicode());
            // hit indexes not given: every line looked up by id
            let items: Vec<HitLines<'_>> = around.iter().map(|l| HitLines { hit: None, lines: l }).collect();
            assert_eq!(old, b.session.locate_lines(&items), "by id {}", json.escape_unicode());
            assert_eq!(a.session.all_terms(), b.session.all_terms());
        }
        assert!(used > 500, "too few snippets to compare: {used}");
    }

    #[test]
    fn positioned_snippets_equal_text_snippets() {
        check(IndexOptions { positions: true, keep_marks: false }, 7);
        check(IndexOptions { positions: true, keep_marks: false }, 99);
    }

    #[test]
    fn positioned_snippets_equal_text_snippets_keep_marks() {
        check(IndexOptions { positions: true, keep_marks: true }, 11);
    }

    /// Issue #21: on an index with positions every hit is decided from its
    /// positions during the search. The hits are exactly the candidates
    /// (the hits the text decided before, every text-level check still
    /// open) whose text the session's locator passes, for every request
    /// shape; and the streamed search gives the blocking one's hits.
    #[test]
    fn hits_decided_from_positions_equal_the_text_decision() {
        for (seed, keep_marks) in [(5u64, false), (13, true)] {
            let lines = corpus(seed, 400);
            let text_of = |id: u64| lines[(id - 1000) as usize].text.as_str();
            let build = |positions: bool| {
                let dir = tempfile::tempdir().unwrap();
                let index = LineIndex::create(dir.path(), IndexOptions { positions, keep_marks }).unwrap();
                let mut w = index.writer(50_000_000, 1).unwrap();
                for l in &lines {
                    w.add(l).unwrap();
                }
                w.commit(None).unwrap();
                (dir, index)
            };
            let (_d1, index) = build(true);
            let mut decided = 0;
            let mut crossed = 0;
            // every request, and each also matching across lines
            let crossing: Vec<String> =
                requests().iter().map(|r| format!("{},\"cross_line\":true}}", &r[..r.len() - 1])).collect();
            for json in requests().into_iter().chain(crossing) {
                let request: SearchRequest = serde_json::from_str(&json).unwrap();
                let mut r = match index.search(&request) {
                    Ok(r) => r,
                    // a shape cross-line refuses (gaps without order)
                    Err(_) if request.cross_line => continue,
                    Err(e) => panic!("{e}"),
                };
                assert!(r.is_complete() && !r.count_needs_text(), "{}", json.escape_unicode());
                crossed += r.hits.iter().filter(|h| h.next_line_id.is_some()).count();
                // the candidates: the same index, the text deciding
                index.test_only_text_decision.store(true, std::sync::atomic::Ordering::Relaxed);
                let candidates = index.search(&request).unwrap();
                index.test_only_text_decision.store(false, std::sync::atomic::Ordering::Relaxed);
                let want: Vec<u64> = candidates
                    .hits
                    .iter()
                    .filter(|h| r.session.locate_pair(text_of(h.line_id), h.next_line_id.map(text_of)).is_some())
                    .map(|h| h.line_id)
                    .collect();
                let got: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
                if got != want {
                    let extra: Vec<String> = got.iter().filter(|i| !want.contains(i)).map(|&i| format!("{i}:{}", text_of(i).escape_unicode())).collect();
                    let missing: Vec<String> = want.iter().filter(|i| !got.contains(i)).map(|&i| format!("{i}:{}", text_of(i).escape_unicode())).collect();
                    panic!("{} extra {extra:?} missing {missing:?}", json.escape_unicode());
                }
                let c = r.counts();
                assert!(c.exact);
                assert_eq!(c.total, want.len() as u64);
                decided += candidates.hits.len() - want.len();
                // streamed: the same hits, batch by batch
                let mut s = index.search_streaming(&request, None).unwrap();
                while !s.pull(s.hits.len() + 1, Some(std::time::Duration::from_secs(10))).unwrap() {}
                assert_eq!(s.hits, r.hits);
            }
            assert!(decided > 20, "too few hits dropped by their positions: {decided}");
            assert!(crossed > 20, "too few cross-line hits: {crossed}");
        }
    }

    /// Word-level descriptors (`word_windows`): the window decided from
    /// the positions during the search, turned into a snippet with the line
    /// text by `finish_windows` (no window search), equals the snippet the
    /// session locates on the same text, for every request shape, with and
    /// without matching across lines (a cross-line pair has no word-level
    /// descriptor and is located from its two lines). Not asked: none kept.
    #[test]
    fn word_windows_finished_equal_located_snippets() {
        for (seed, keep_marks) in [(5u64, false), (13, true)] {
            let lines = corpus(seed, 400);
            let text_of = |id: u64| lines[(id - 1000) as usize].text.as_str();
            let dir = tempfile::tempdir().unwrap();
            let index = LineIndex::create(dir.path(), IndexOptions { positions: true, keep_marks }).unwrap();
            let mut w = index.writer(50_000_000, 1).unwrap();
            for l in &lines {
                w.add(l).unwrap();
            }
            w.commit(None).unwrap();
            let (mut finished, mut with_window) = (0, 0);
            let crossing: Vec<String> =
                requests().iter().map(|r| format!("{},\"cross_line\":true}}", &r[..r.len() - 1])).collect();
            for json in requests().into_iter().chain(crossing) {
                let plain: SearchRequest = serde_json::from_str(&json).unwrap();
                let Ok(r0) = index.search(&plain) else { continue };
                assert!(r0.word_windows(0, usize::MAX).is_empty(), "kept without being asked");
                let asked = format!("{},\"word_windows\":true}}", &json[..json.len() - 1]);
                let request: SearchRequest = serde_json::from_str(&asked).unwrap();
                let mut r = index.search(&request).unwrap();
                assert_eq!(r.hits, r0.hits, "{}", json.escape_unicode());
                let windows = r.word_windows(0, r.hits.len()).to_vec();
                assert_eq!(windows.len(), r.hits.len());
                let hits = r.hits.clone();
                let items: Vec<(WordWindow, &str, Option<&str>)> =
                    hits.iter().zip(&windows).map(|(h, &w)| (w, text_of(h.line_id), h.next_line_id.map(text_of))).collect();
                let got = r.finish_windows(&items).unwrap();
                for ((h, w), g) in hits.iter().zip(&windows).zip(got) {
                    let want = r.session.locate_pair(text_of(h.line_id), h.next_line_id.map(text_of));
                    assert!(want.is_some());
                    assert_eq!(g, want, "{} line {} window {w:?}", json.escape_unicode(), h.line_id);
                    with_window += w.is_some() as usize;
                    finished += 1;
                }
            }
            assert!(with_window > 200, "too few word-level descriptors: {with_window} of {finished}");
        }
    }

    /// Issue #21: a paired reading's two readings share one position, so on
    /// an index with positions they never fill two groups of an any-order
    /// proximity: the span query and the locator (word rule) agree. An
    /// index without positions keeps its text rule.
    #[test]
    fn paired_reading_fills_one_proximity_group() {
        let text = format!("{} ({}) [{}] {}", W[5], W[0], W[1], W[5]);
        let lines = vec![LineDoc {
            line_id: 1,
            position: 0,
            book_id: 1,
            text: text.clone(),
            section_id: None,
            no_cross: false,
        }];
        let json = format!(
            "{{\"groups\":[{{\"kind\":\"word\",\"word\":\"{}\"}},{{\"kind\":\"word\",\"word\":\"{}\"}}],\"proximity\":{{\"mode\":\"any_order\",\"distance\":2}}}}",
            W[0], W[1]
        );
        let request: SearchRequest = serde_json::from_str(&json).unwrap();
        for positions in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let index = LineIndex::create(dir.path(), IndexOptions { positions, keep_marks: false }).unwrap();
            let mut w = index.writer(50_000_000, 1).unwrap();
            w.add(&lines[0]).unwrap();
            w.commit(None).unwrap();
            let mut r = index.search(&request).unwrap();
            if positions {
                assert!(r.hits.is_empty());
                assert!(r.session.locate(&text).is_none());
            } else {
                // the text rule without positions: unchanged
                assert_eq!(r.hits.len(), 1);
                assert!(r.session.locate(&text).is_some());
            }
        }
    }

    /// Issue #21: the hits stream in batches in catalog order, the counts
    /// grow with them and are exact once complete; cancelling stops it.
    #[test]
    fn streamed_hits_arrive_in_batches_and_cancel() {
        let lines = corpus(21, 6000);
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        for l in &lines {
            w.add(l).unwrap();
        }
        w.merge_all().unwrap();
        let request: SearchRequest = serde_json::from_str(&format!(
            "{{\"groups\":[{{\"kind\":\"word\",\"word\":\"{}\"}},{{\"kind\":\"word\",\"word\":\"{}\"}}],\"max_word_distance\":3}}",
            W[0], W[1]
        ))
        .unwrap();
        let all = index.search(&request).unwrap();
        assert!(all.hits.len() > 1100, "{}", all.hits.len());
        let mut s = index.search_streaming(&request, None).unwrap();
        let mut sizes = vec![s.hits.len()];
        while !s.pull(s.hits.len() + 1, None).unwrap() {
            assert!(!s.counts().exact);
            sizes.push(s.hits.len());
        }
        sizes.push(s.hits.len());
        assert!(sizes[0] < all.hits.len(), "the first batch is not all: {sizes:?}");
        assert_eq!(s.hits, all.hits);
        assert!(s.hits.windows(2).all(|w| w[0].position < w[1].position));
        assert_eq!(s.counts(), all.counts());
        // cross-line (issue #21): each chunk's pairs decided with it, in
        // catalog order, the same hits as one pass over every line
        for json in [
            format!("{{\"groups\":[{{\"kind\":\"word\",\"word\":\"{}\"}},{{\"kind\":\"word\",\"word\":\"{}\"}}],\"max_word_distance\":3,\"cross_line\":true}}", W[0], W[1]),
            format!("{{\"groups\":[{{\"kind\":\"word\",\"word\":\"{}\"}},{{\"kind\":\"word\",\"word\":\"{}\"}}],\"require_ordered\":true,\"cross_line\":true}}", W[2], W[3]),
            format!("{{\"groups\":[{{\"kind\":\"word\",\"word\":\"{}\"}},{{\"kind\":\"word\",\"word\":\"{}\"}}],\"cross_line\":true,\"after_position\":1234}}", W[0], W[4]),
        ] {
            let request: SearchRequest = serde_json::from_str(&json).unwrap();
            index.test_only_no_scan.store(true, std::sync::atomic::Ordering::Relaxed);
            let whole = index.search(&request).unwrap();
            index.test_only_no_scan.store(false, std::sync::atomic::Ordering::Relaxed);
            let mut s = index.search_streaming(&request, None).unwrap();
            let first = s.hits.len();
            while !s.pull(s.hits.len() + 1, None).unwrap() {}
            assert!(first < s.hits.len() || s.hits.len() < 1024, "streamed in one batch: {first}");
            assert!(s.hits.iter().any(|h| h.next_line_id.is_some()));
            assert_eq!(s.hits, whole.hits, "{}", json.escape_unicode());
            assert_eq!(s.counts(), whole.counts());
        }
        // cancelled mid-stream: the next pull says so
        let token = crate::CancelToken::new();
        let mut s = index.search_streaming(&request, Some(token.clone())).unwrap();
        token.cancel();
        match s.pull(usize::MAX, None) {
            Err(crate::Error::Cancelled) => {}
            Ok(true) => assert_eq!(s.hits, all.hits), // finished before the cancel
            other => panic!("{:?}", other.map_err(|e| e.to_string())),
        }
    }

    #[test]
    fn a_text_that_is_not_the_indexed_one_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let lines = corpus(3, 100);
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        for l in &lines {
            w.add(l).unwrap();
        }
        w.commit(None).unwrap();
        let request: SearchRequest =
            serde_json::from_str(&format!("{{\"groups\":[{{\"kind\":\"word\",\"word\":\"{}\"}}]}}", W[0])).unwrap();
        let mut a = index.search(&request).unwrap();
        let mut b = index.search(&request).unwrap();
            b.session.force_positions(true);
        let n = b.hits.len();
        assert!(n > 3);
        // a line id the index does not hold: located from the text, same snippet
        let texts: Vec<&str> = b.hits.iter().map(|h| lines[(h.line_id - 1000) as usize].text.as_str()).collect();
        let one: Vec<[(u64, &str); 1]> = (0..n).map(|i| [(if i == 0 { 1 } else { b.hits[i].line_id }, texts[i])]).collect();
        let items: Vec<HitLines<'_>> = one.iter().map(|l| HitLines { hit: None, lines: &l[..] }).collect();
        assert_eq!(a.session.locate_many(&texts), b.session.locate_lines(&items));
        assert_eq!(b.session.positioned_fallbacks(), 1);
        // every hit given the NEXT hit's text: the texts disagree with the
        // positions, caught where a matched position holds another word
        let swapped: Vec<&str> = (0..n).map(|i| texts[(i + 1) % n]).collect();
        let one: Vec<[(u64, &str); 1]> = (0..n).map(|i| [(b.hits[i].line_id, swapped[i])]).collect();
        let items: Vec<HitLines<'_>> =
            one.iter().enumerate().map(|(i, l)| HitLines { hit: Some(i), lines: &l[..] }).collect();
        let _ = b.session.locate_lines(&items);
        assert!(b.session.positioned_fallbacks() > n / 2);
        let items: Vec<HitLines<'_>> =
            one.iter().enumerate().map(|(i, l)| HitLines { hit: Some(i), lines: &l[..] }).collect();
        // no positions: the text path, same answers
        let dir2 = tempfile::tempdir().unwrap();
        let flat = LineIndex::create(dir2.path(), IndexOptions { positions: false, keep_marks: false }).unwrap();
        let mut w = flat.writer(50_000_000, 1).unwrap();
        for l in &lines {
            w.add(l).unwrap();
        }
        w.commit(None).unwrap();
        let mut c = flat.search(&request).unwrap();
        assert!(!c.session.locates_from_positions());
        // (the windows may differ: positions pick the word-tightest one,
        // FtsLib's text path the character-tightest, issue #21)
        let pass = |v: Vec<Option<crate::Snippet>>| v.iter().map(|s| s.is_some()).collect::<Vec<_>>();
        assert_eq!(pass(a.session.locate_many(&swapped)), pass(c.session.locate_lines(&items)));
    }
}
