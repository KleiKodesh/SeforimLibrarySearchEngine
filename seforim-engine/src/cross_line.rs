//! Cross-line matching (Otzaria's `cross_line.rs`, issue #1703): a match
//! that starts at the end of line `L` and ends at the start of line `L + 1`
//! of the same book. An engine option (`cross_line`) for every search type
//! (ordered and any order, FtsLib's distance, words anywhere, partial
//! matching, phrase alternatives, user-regex sequences); at most one line
//! break; a hit is reported on `L` with the next line's id attached.
//!
//! The two lines are one virtual line: `L`'s positions, then `L + 1`'s
//! shifted by `L`'s word count (a small fast field written at indexing; the
//! scanner that numbers the positions counts them). The rule of the search
//! must hold on the joined positions with a matched word on each line.
//! Lines carrying the host's no-cross flag (headings, Otzaria's rule) take
//! part in no cross-line match, into or out of them.
//!
//! How (per way of matching: the main query, each phrase alternative):
//!
//! 1. Each slot's lines as a bitmap over catalog positions, from the terms
//!    its walk already matched (the sinks): postings only, no second walk.
//! 2. Candidates by shift-and-AND: in order, for each split `s` the lines
//!    holding slots `0..s` AND (the lines holding slots `s..` shifted down
//!    one); in any order, each slot on `L` or `L + 1` with a slot on each
//!    line (a count per line for partial matching).
//! 3. Line checks: `L + 1` alive in the same book, no flag on either line,
//!    the book filter and the negative query on `L` (Otzaria applies its
//!    filters to the attributed line).
//! 4. The in-line hit wins: a line already found in-line is not a
//!    cross-line hit (when the in-line hits are exact; otherwise the host's
//!    text check tries the line alone first).
//! 5. Exact check: words anywhere (and partial matching) are exact from the
//!    bitmaps; Otzaria's proximity (and per-pair gaps, phrases, regex
//!    sequences) is checked on the postings' positions of both lines on an
//!    index with positions, with the locator's own window rules; without
//!    positions, and for FtsLib's distance / order, the host's texts are
//!    checked by the locator over the joined text (like the in-line rule).

use std::collections::{HashMap, HashSet};

use hebrew_fts::{PreparedQueryGroups, Proximity, SnippetWindowLocator};
use tantivy::columnar::Column;
use tantivy::postings::Postings;
use tantivy::query::Weight;
use tantivy::schema::IndexRecordOption;
use tantivy::{DocId, DocSet, Searcher, Term, TERMINATED};

use crate::index::{LineIndex, LINE_FLAG_NO_CROSS};
use crate::search::{CancelToken, PlacedHit};
use crate::section_scope::{SectionColumns, SectionKey};
use crate::{Error, Result};

/// The rule a way of matching applies across the break.
#[derive(Debug, Clone)]
pub(crate) enum CrossRule {
    /// Otzaria's proximity (ordered gaps, or an any-order window): exact on
    /// positions, checked on the texts without them.
    Proximity(Proximity),
    /// Words anywhere in the two lines, `min` of the slots (all of them, or
    /// partial matching): exact from the bitmaps.
    Cover { min: usize },
    /// FtsLib's word distance / order: checked on the texts.
    Text { ordered: bool },
}

/// One way of matching: its slots' matched terms, the prepared map built
/// from them (marks and ordinals) and its rule.
pub(crate) struct CrossMatcher<'a> {
    pub terms: &'a [Vec<String>],
    pub prepared: &'a PreparedQueryGroups,
    pub rule: CrossRule,
}

/// What the cross-line pass needs from the search.
pub(crate) struct CrossInput<'a> {
    pub matchers: Vec<CrossMatcher<'a>>,
    /// The in-line hits, and whether every one of them is exact (no text
    /// check left).
    pub inline: &'a [PlacedHit],
    pub inline_exact: bool,
    pub books: Option<&'a HashSet<u64>>,
    /// The query-level negative query (lines it finds are dropped).
    pub negative: Option<&'a dyn Weight>,
    /// Sections the negative query's section scope dropped.
    pub drop_sections: Option<&'a HashSet<SectionKey>>,
    pub cancel: Option<&'a CancelToken>,
}

/// The cross-line pass's result.
#[derive(Default)]
pub(crate) struct CrossOutput {
    /// New hits (lines not found in-line), each with its next line's id,
    /// and its (segment ordinal, doc).
    pub hits: Vec<(PlacedHit, (u32, DocId))>,
    /// In-line hits still to be checked on the text that may also continue
    /// onto the next line: position -> next line id.
    pub inline_next: HashMap<u64, u64>,
    /// Some cross-line hits are candidates the texts must confirm.
    pub needs_text: bool,
}

/// Bits over catalog positions.
#[derive(Clone)]
struct Bits(Vec<u64>);

impl Bits {
    fn new(n: usize) -> Bits {
        Bits(vec![0; n.div_ceil(64)])
    }
    fn set(&mut self, i: u64) {
        self.0[(i / 64) as usize] |= 1 << (i % 64);
    }
    fn get(&self, i: u64) -> bool {
        self.0.get((i / 64) as usize).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }
    fn and(&mut self, o: &Bits) {
        self.0.iter_mut().zip(&o.0).for_each(|(a, b)| *a &= b);
    }
    fn or(&mut self, o: &Bits) {
        self.0.iter_mut().zip(&o.0).for_each(|(a, b)| *a |= b);
    }
    /// Bit `i` of the result = bit `i + 1` of self (the next line's bit on `L`).
    fn shifted_down(&self) -> Bits {
        let w = &self.0;
        Bits((0..w.len()).map(|i| (w[i] >> 1) | w.get(i + 1).map_or(0, |n| n << 63)).collect())
    }
    fn ones(&self) -> impl Iterator<Item = u64> + '_ {
        self.0.iter().enumerate().flat_map(|(i, &w)| {
            let mut w = w;
            std::iter::from_fn(move || {
                if w == 0 {
                    return None;
                }
                let b = w.trailing_zeros() as u64;
                w &= w - 1;
                Some(i as u64 * 64 + b)
            })
        })
    }
}

/// One segment's per-line columns.
struct LineColumns {
    position: Column<u64>,
    line_id: Column<u64>,
    book_id: Column<u64>,
    word_count: Column<u64>,
    flags: Column<u64>,
    sections: Option<SectionColumns>,
}

/// One slot's lines: the bitmap and each line's (position, segment, doc).
struct SlotLines {
    bits: Bits,
    docs: Vec<(u64, u32, DocId)>,
}

/// A candidate pair: `L` and `L + 1` as (position, segment, doc).
#[derive(Clone, Copy)]
struct Pair {
    position: u64,
    left: (u32, DocId),
    right: (u32, DocId),
}

/// The cross-line pass (see the module docs).
pub(crate) fn find(index: &LineIndex, searcher: &Searcher, input: &CrossInput<'_>) -> Result<CrossOutput> {
    let (Some(wc_field), Some(flags_field)) = (index.fields.word_count, index.fields.line_flags) else {
        return Err(Error::Unsupported(
            "cross-line matching needs an index built with per-line data (word count, line flags): rebuild the index"
                .into(),
        ));
    };
    let field_name = |f| searcher.schema().get_field_name(f).to_string();
    let (wc_name, flags_name) = (field_name(wc_field), field_name(flags_field));
    let segments = searcher.segment_readers();
    let mut columns = Vec::with_capacity(segments.len());
    let mut max_position = 0u64;
    for segment in segments {
        let ff = segment.fast_fields();
        let position = ff.u64("position")?;
        max_position = max_position.max(position.max_value());
        columns.push(LineColumns {
            position,
            line_id: ff.u64("line_id")?,
            book_id: ff.u64("book_id")?,
            word_count: ff.u64(&wc_name)?,
            flags: ff.u64(&flags_name)?,
            sections: match input.drop_sections {
                Some(_) => Some(SectionColumns::open(segment)?),
                None => None,
            },
        });
    }
    let nbits = max_position as usize + 2;
    let inline: HashSet<u64> = input.inline.iter().map(|h| h.position).collect();
    let mut locator = SnippetWindowLocator::new(0);

    // Per line L: (exact, left, right) over every way of matching.
    let mut found: HashMap<u64, (bool, Pair)> = HashMap::new();
    for m in &input.matchers {
        let k = m.terms.len();
        if k < 2 {
            continue; // one slot never crosses a line
        }
        check(input.cancel)?;
        let slots: Vec<SlotLines> =
            m.terms.iter().map(|t| slot_lines(index, searcher, &columns, t, nbits)).collect::<Result<_>>()?;
        let Some(candidates) = candidates(&slots, &m.rule, nbits) else { continue };
        let pairs = pairs(&slots, &candidates, &columns, &inline, input)?;
        let pairs = drop_negative(pairs, searcher, input)?;
        check(input.cancel)?;
        match &m.rule {
            CrossRule::Cover { .. } => mark(&mut found, pairs.into_iter().map(|p| (p, true))),
            CrossRule::Proximity(proximity) if index.options.positions => {
                let passed = verify_positions(index, searcher, &columns, m, proximity, &pairs, &mut locator, input.cancel)?;
                mark(&mut found, passed.into_iter().map(|p| (p, true)));
            }
            _ => mark(&mut found, pairs.into_iter().map(|p| (p, false))),
        }
    }

    let mut out = CrossOutput::default();
    for (position, (exact, pair)) in found {
        let left = &columns[pair.left.0 as usize];
        let next_line = columns[pair.right.0 as usize].line_id.first(pair.right.1).unwrap_or(0);
        if inline.contains(&position) {
            // the text check tries the line alone first (in-line wins)
            out.inline_next.insert(position, next_line);
            continue;
        }
        out.needs_text |= !exact;
        out.hits.push((
            PlacedHit {
                position,
                line_id: left.line_id.first(pair.left.1).unwrap_or(0),
                book_id: left.book_id.first(pair.left.1).unwrap_or(0),
                next_line_id: Some(next_line),
            },
            pair.left,
        ));
    }
    Ok(out)
}

fn check(cancel: Option<&CancelToken>) -> Result<()> {
    if cancel.is_some_and(|t| t.is_cancelled()) {
        return Err(Error::Cancelled);
    }
    Ok(())
}

/// Records pairs; a line passed exactly by one way stays exact.
fn mark(found: &mut HashMap<u64, (bool, Pair)>, pairs: impl Iterator<Item = (Pair, bool)>) {
    for (pair, exact) in pairs {
        let e = found.entry(pair.position).or_insert((exact, pair));
        e.0 |= exact;
    }
}

/// The alive lines holding any of `terms`, from their postings.
fn slot_lines(
    index: &LineIndex,
    searcher: &Searcher,
    columns: &[LineColumns],
    terms: &[String],
    nbits: usize,
) -> Result<SlotLines> {
    let field = index.fields.text;
    let mut bits = Bits::new(nbits);
    let mut docs = Vec::new();
    for (ord, segment) in searcher.segment_readers().iter().enumerate() {
        let inverted = segment.inverted_index(field)?;
        let mut seen = vec![0u64; (segment.max_doc() as usize).div_ceil(64)];
        for t in terms {
            let term = Term::from_field_text(field, t);
            let Some(info) = inverted.get_term_info(&term)? else { continue };
            let mut postings = inverted.read_postings_from_terminfo(&info, IndexRecordOption::Basic)?;
            let mut doc = postings.doc();
            while doc != TERMINATED {
                seen[doc as usize / 64] |= 1 << (doc % 64);
                doc = postings.advance();
            }
        }
        let alive = segment.alive_bitset();
        for (i, &w) in seen.iter().enumerate() {
            let mut w = w;
            while w != 0 {
                let doc = (i * 64) as DocId + w.trailing_zeros();
                w &= w - 1;
                if alive.is_some_and(|a| !a.is_alive(doc)) {
                    continue;
                }
                let Some(position) = columns[ord].position.first(doc) else { continue };
                bits.set(position);
                docs.push((position, ord as u32, doc));
            }
        }
    }
    Ok(SlotLines { bits, docs })
}

/// The candidate lines `L` of one way of matching (bitmaps only; None when
/// it can never cross: fewer than two slots required).
fn candidates(slots: &[SlotLines], rule: &CrossRule, nbits: usize) -> Option<Bits> {
    let k = slots.len();
    let ordered = matches!(rule, CrossRule::Proximity(Proximity::Ordered { .. }) | CrossRule::Text { ordered: true });
    if ordered {
        // split s: slots 0..s end L, slots s.. start L + 1
        let mut out = Bits::new(nbits);
        for s in 1..k {
            let mut left = slots[0].bits.clone();
            slots[1..s].iter().for_each(|x| left.and(&x.bits));
            let mut right = slots[s].bits.clone();
            slots[s + 1..].iter().for_each(|x| right.and(&x.bits));
            left.and(&right.shifted_down());
            out.or(&left);
        }
        return Some(out);
    }
    let min = match rule {
        CrossRule::Cover { min } => (*min).min(k),
        _ => k,
    };
    if min < 2 {
        return None; // one word is always on one line: in-line or nothing
    }
    let either: Vec<Bits> = slots
        .iter()
        .map(|s| {
            let mut b = s.bits.clone();
            b.or(&s.bits.shifted_down());
            b
        })
        .collect();
    let mut any_left = Bits::new(nbits);
    slots.iter().for_each(|s| any_left.or(&s.bits));
    let any_right = any_left.shifted_down();
    let mut out = if min >= k {
        let mut all = either[0].clone();
        either[1..].iter().for_each(|b| all.and(b));
        all
    } else {
        let mut union = Bits::new(nbits);
        either.iter().for_each(|b| union.or(b));
        let mut counted = Bits::new(nbits);
        for line in union.ones() {
            if either.iter().filter(|b| b.get(line)).count() >= min {
                counted.set(line);
            }
        }
        counted
    };
    out.and(&any_left);
    out.and(&any_right);
    Some(out)
}

/// The candidates as pairs passing the line checks: `L + 1` exists in the
/// same book, neither line flagged, `L` not an exact in-line hit, `L`'s book
/// in the filter and its section not dropped.
fn pairs(
    slots: &[SlotLines],
    candidates: &Bits,
    columns: &[LineColumns],
    inline: &HashSet<u64>,
    input: &CrossInput<'_>,
) -> Result<Vec<Pair>> {
    let mut docs: HashMap<u64, (u32, DocId)> = HashMap::new();
    for slot in slots {
        for &(position, seg, doc) in &slot.docs {
            if candidates.get(position) || (position > 0 && candidates.get(position - 1)) {
                docs.insert(position, (seg, doc));
            }
        }
    }
    let mut out = Vec::new();
    for position in candidates.ones() {
        if input.inline_exact && inline.contains(&position) {
            continue;
        }
        let (Some(&left), Some(&right)) = (docs.get(&position), docs.get(&(position + 1))) else { continue };
        let (lc, rc) = (&columns[left.0 as usize], &columns[right.0 as usize]);
        let book = lc.book_id.first(left.1).unwrap_or(0);
        if rc.book_id.first(right.1) != Some(book) {
            continue;
        }
        let flagged = |c: &LineColumns, d: DocId| c.flags.first(d).unwrap_or(0) & LINE_FLAG_NO_CROSS != 0;
        if flagged(lc, left.1) || flagged(rc, right.1) || lc.word_count.first(left.1).unwrap_or(0) == 0 {
            continue;
        }
        if input.books.is_some_and(|b| !b.contains(&book)) {
            continue;
        }
        if let (Some(drop), Some(sections)) = (input.drop_sections, &lc.sections) {
            if drop.contains(&sections.key(left.1)) {
                continue;
            }
        }
        out.push(Pair { position, left, right });
    }
    Ok(out)
}

/// Drops the pairs whose line `L` the negative query finds.
fn drop_negative(pairs: Vec<Pair>, searcher: &Searcher, input: &CrossInput<'_>) -> Result<Vec<Pair>> {
    let Some(negative) = input.negative else { return Ok(pairs) };
    let mut excluded: HashSet<(u32, DocId)> = HashSet::new();
    for (ord, segment) in searcher.segment_readers().iter().enumerate() {
        let mut docs: Vec<DocId> = pairs.iter().filter(|p| p.left.0 == ord as u32).map(|p| p.left.1).collect();
        if docs.is_empty() {
            continue;
        }
        docs.sort_unstable();
        docs.dedup();
        let mut scorer = negative.scorer(segment, 1.0)?;
        for doc in docs {
            let at = if scorer.doc() < doc { scorer.seek(doc) } else { scorer.doc() };
            if at == TERMINATED {
                break;
            }
            if at == doc {
                excluded.insert((ord as u32, doc));
            }
        }
    }
    Ok(pairs.into_iter().filter(|p| !excluded.contains(&p.left)).collect())
}

/// The pairs whose joined positions satisfy `proximity` with a matched
/// word on each line (an index with positions).
fn verify_positions(
    index: &LineIndex,
    searcher: &Searcher,
    columns: &[LineColumns],
    m: &CrossMatcher<'_>,
    proximity: &Proximity,
    pairs: &[Pair],
    locator: &mut SnippetWindowLocator,
    cancel: Option<&CancelToken>,
) -> Result<Vec<Pair>> {
    let field = index.fields.text;
    // every term of the way of matching once, with its ordinal
    let mut terms: Vec<(&str, u32)> = Vec::new();
    let mut seen = HashSet::new();
    for t in m.terms.iter().flatten() {
        if seen.insert(t.as_str()) {
            if let Some(p) = m.prepared.get(t) {
                terms.push((t.as_str(), p.ordinal as u32));
            }
        }
    }
    let mut occurrences: HashMap<(u32, DocId), Vec<(usize, u32)>> = HashMap::new();
    let mut buffer = Vec::new();
    for (ord, segment) in searcher.segment_readers().iter().enumerate() {
        let mut docs: Vec<DocId> = pairs
            .iter()
            .flat_map(|p| [p.left, p.right])
            .filter(|d| d.0 == ord as u32)
            .map(|d| d.1)
            .collect();
        if docs.is_empty() {
            continue;
        }
        docs.sort_unstable();
        docs.dedup();
        let inverted = segment.inverted_index(field)?;
        for &(t, ordinal) in &terms {
            let Some(info) = inverted.get_term_info(&Term::from_field_text(field, t))? else { continue };
            let mut postings = inverted.read_postings_from_terminfo(&info, IndexRecordOption::WithFreqsAndPositions)?;
            let mut i = 0;
            while i < docs.len() {
                let target = docs[i];
                let at = if postings.doc() < target { postings.seek(target) } else { postings.doc() };
                if at == TERMINATED {
                    break;
                }
                if at == target {
                    postings.positions(&mut buffer);
                    occurrences
                        .entry((ord as u32, target))
                        .or_default()
                        .extend(buffer.iter().map(|&p| (p as usize, ordinal)));
                    i += 1;
                } else {
                    i += docs[i..].partition_point(|&d| d < at);
                }
            }
        }
        check(cancel)?;
    }
    let mut passed = Vec::new();
    let mut joined = Vec::new();
    for pair in pairs {
        let words = columns[pair.left.0 as usize].word_count.first(pair.left.1).unwrap_or(0) as usize;
        joined.clear();
        if let Some(left) = occurrences.get(&pair.left) {
            joined.extend_from_slice(left);
        }
        if let Some(right) = occurrences.get(&pair.right) {
            joined.extend(right.iter().map(|&(p, o)| (p + words, o)));
        }
        if locator.proximity_across_positions(m.prepared, proximity, &mut joined, words) {
            passed.push(*pair);
        }
    }
    Ok(passed)
}
