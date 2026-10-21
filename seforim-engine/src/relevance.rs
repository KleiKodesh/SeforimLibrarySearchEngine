//! Result order: catalog (the default), Otzaria's relevance, or Otzaria's
//! generation order (its `ResultsOrder`, otzaria_search_engine v5.0.0).
//!
//! The default stays the catalog order and costs nothing: the hits are
//! sorted by position as always, no score is computed and no doc id kept.
//!
//! **Generation** is Otzaria's `OrderKind::Generation` (`book_order.rs`):
//! the books by the generation order the host gives per book (from
//! seforim.db, as Otzaria's app pushes its `generationOrder`), then by
//! catalog order, then by line. A book the host gave no generation comes
//! after every book it did.
//!
//! **Relevance** is Otzaria's `order_by_score()` over the query Otzaria
//! builds for the same search, ported per query shape (its
//! `api/search_engine.rs`, `gap_phrase.rs`, `quote_expansion.rs`,
//! `hebrew_query.rs::match_tier`). Ties go to the catalog order (Otzaria's
//! `TopDocs` breaks them by ascending doc address). The score is computed
//! after the search, only for the hits, from the terms the walks matched:
//!
//! - one slot: Otzaria's relevance tiers (`ranked_term_query`): each matched
//!   term is put in a tier by [`match_tier`] against the typed word and its
//!   alternatives (0 the typed form, 1 that form inside a longer word, 2 a
//!   ktiv male/haser variant, 3 the rest), and a line scores the sum of
//!   1000 / 100 / 10 / 1 over the tiers it holds (`ADVANCED_TIER_BOOSTS`).
//! - a fuzzy slot (`word~N`, Otzaria's fuzzy mode, its plain builder
//!   `build_fuzzy_query_from_terms`): 1000 + BM25 for the exact word (its
//!   spellings: the word and every term that is the word with quotes
//!   inserted, one BM25 term over their summed frequencies), the same for
//!   the word without its quotes when it has some, + 1 for the fuzzy match.
//!   Several fuzzy words with no proximity: + 2000 when the typed words
//!   stand within one word of each other (`exact_phrase_boost`, slop 1).
//! - several slots with an ordered proximity (or per-pair `gaps`): BM25 of
//!   the phrase, tantivy's own `PhraseScorer` with the slop the sum of the
//!   gaps, as Otzaria's `RegexPhraseQuery` (+ `GapVerifiedPhraseQuery`).
//! - several slots in any order: the word count + 1 / (1 + the tightest
//!   span), every word at its own position (`TermListPhraseQuery`'s
//!   `rank_span`).
//! - several slots with no proximity (words anywhere in the line) and
//!   partial matching: 1 per slot the line holds (Otzaria's per-word
//!   `TermSetQuery`s, `scoped_words_query`); a fuzzy slot scores as above.
//! - each phrase alternative (Otzaria's acronym expansion, ORed): its
//!   phrase BM25 (slop 0) when it has several words, 1 for one word, added
//!   to the main query's score when the line matches both.
//!
//! Index data: the tiers and the counts need only the postings (both index
//! flavours); fuzzy BM25 uses the term frequencies and field norms (an index
//! without positions has no frequencies: tantivy reads every one as 1, as
//! for Otzaria's own frequency-less terms); phrases and spans need positions,
//! so those shapes return an error on an index without positions.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use serde::{Deserialize, Serialize};
use tantivy::postings::{Postings, SegmentPostings};
use tantivy::query::{Bm25StatisticsProvider, Bm25Weight, PhraseScorer, Weight};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocId, DocSet, InvertedIndexReader, Score, Searcher, SegmentReader, Term, TERMINATED};

use crate::search::{CancelToken, GroupSpec, PlacedHit};
use crate::{Error, Result};

/// The order of the hits (Otzaria's `ResultsOrder`). JSON `"catalog"`
/// (default; `"catalogue"` accepted) / `"relevance"` / `"generation"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultsOrder {
    /// The catalog order (the index's `position`), as FtsLib.
    #[default]
    #[serde(alias = "catalogue")]
    Catalog,
    /// Otzaria's relevance: best score first, ties in catalog order.
    Relevance,
    /// Otzaria's generation order: by the books' generation order (given by
    /// the host per book), then catalog order.
    Generation,
}

/// Otzaria's `ADVANCED_TIER_BOOSTS`: typed form, affixed, spelling variant, rest.
const TIER_BOOSTS: [Score; 4] = [1000.0, 100.0, 10.0, 1.0];
/// Otzaria's `FUZZY_BOOST_EXACT`: the constant floor of an exact fuzzy-mode hit.
const FUZZY_BOOST_EXACT: Score = 1000.0;
/// Otzaria's `FUZZY_BOOST_EXACT_REL`: the BM25 add-on of the exact word.
const FUZZY_BOOST_EXACT_REL: Score = 1.0;
/// Otzaria's `FUZZY_BOOST_FUZZY`: an edit-distance match.
const FUZZY_BOOST_FUZZY: Score = 1.0;
/// Otzaria's `FUZZY_BOOST_PHRASE`: the typed words adjacent.
const FUZZY_BOOST_PHRASE: Score = 2000.0;
/// Otzaria's `LEXICAL_FUZZY_PHRASE_SLOP`.
const FUZZY_PHRASE_SLOP: u32 = 1;

/// Sorts `hits` (any order) by catalog position, or by `generations`
/// (`(book_id, generation_order)`; a later pair for the same book wins)
/// then catalog position.
pub(crate) fn sort_hits(hits: &mut [PlacedHit], order: ResultsOrder, generations: &[(u64, u32)]) {
    match order {
        ResultsOrder::Generation => {
            let rank: HashMap<u64, u32> = generations.iter().copied().collect();
            hits.sort_unstable_by_key(|h| (rank.get(&h.book_id).map_or(u64::MAX, |&g| u64::from(g)), h.position));
        }
        _ => hits.sort_unstable_by_key(|h| h.position),
    }
}

/// Which of Otzaria's scores a main query takes (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    /// One slot: tiers (fuzzy: the fuzzy tiers).
    Single,
    /// Several slots, words anywhere in the line, or partial matching.
    Count { partial: bool },
    /// Several slots in order: phrase BM25 with this slop.
    Phrase { slop: u32 },
    /// Several slots in any order: count + span bonus.
    AnyOrder,
}

impl Shape {
    /// The shape of a main query of `slots` searchable slots.
    /// `gaps` with `ordered` false (per-pair spacing in either order) has no
    /// Otzaria counterpart; it scores as the any-order shape, Otzaria's only
    /// order-free proximity.
    pub(crate) fn of(
        slots: usize,
        partial: bool,
        proximity: Option<&hebrew_fts::Proximity>,
        gaps: Option<&[u32]>,
        ordered: bool,
    ) -> Shape {
        if partial {
            return Shape::Count { partial: true };
        }
        if slots <= 1 {
            return Shape::Single;
        }
        let sum = |g: &[u32]| g.iter().fold(0u32, |a, &x| a.saturating_add(x));
        match (proximity, gaps) {
            (Some(hebrew_fts::Proximity::Ordered { gaps }), _) => Shape::Phrase { slop: sum(gaps) },
            (Some(hebrew_fts::Proximity::AnyOrder { .. }), _) => Shape::AnyOrder,
            (None, Some(_)) if !ordered => Shape::AnyOrder,
            (None, Some(g)) => Shape::Phrase { slop: sum(g) },
            (None, None) => Shape::Count { partial: false },
        }
    }
}

/// The main query's part of the score.
pub(crate) struct MainPlan<'a> {
    pub shape: Shape,
    /// The searchable slots, in query order.
    pub slots: Vec<&'a GroupSpec>,
    /// The terms each slot's walk matched (aligned with `slots`).
    pub terms: Vec<Vec<String>>,
}

/// Everything a relevance score is made of.
pub(crate) struct ScorePlan<'a> {
    pub field: Field,
    /// The index keeps marks: Otzaria's vocalized field, which it never ranks in tiers.
    pub keep_marks: bool,
    /// None: the main query cannot match.
    pub main: Option<MainPlan<'a>>,
    /// The main query's doc set, when phrase alternatives make a hit's main
    /// match uncertain (scoring disabled).
    pub main_membership: Option<Box<dyn Weight>>,
    /// Per phrase alternative: its slots and the terms each matched.
    pub phrases: Vec<(&'a [GroupSpec], Vec<Vec<String>>)>,
    /// The main query is several words anywhere in the line (Otzaria's
    /// paragraph scope): Otzaria builds each alternative in that scope too,
    /// so it scores 1 per word (all required), not a phrase BM25.
    pub phrases_as_count: bool,
}

/// True when the phrase alternatives score as a word count (see
/// [`ScorePlan::phrases_as_count`]): several main slots, words anywhere.
pub(crate) fn phrases_as_count(shape: Shape, slots: usize) -> bool {
    shape == Shape::Count { partial: false } && slots > 1
}

/// True when every slot is a fuzzy word (Otzaria's fuzzy mode).
fn all_fuzzy(slots: &[&GroupSpec]) -> bool {
    slots.iter().all(|g| matches!(g, GroupSpec::Fuzzy { .. }))
}

/// Errors when a part of the relevance score needs positions the index
/// lacks: a phrase or span (main query or phrase alternative), or fuzzy
/// mode's adjacency boost. Checked before the search runs.
pub(crate) fn check_positions(
    positions: bool,
    ranked: bool,
    main: Option<(Shape, &[&GroupSpec])>,
    phrases: &[&[GroupSpec]],
) -> Result<()> {
    let phrases_as_count = main.is_some_and(|(shape, slots)| phrases_as_count(shape, slots.len()));
    if positions {
        return Ok(());
    }
    let main = main.is_some_and(|(shape, slots)| match shape {
        Shape::Phrase { .. } | Shape::AnyOrder => true,
        Shape::Count { partial: false } => ranked && slots.len() > 1 && all_fuzzy(slots),
        _ => false,
    });
    let phrase = !phrases_as_count && phrases.iter().any(|groups| groups.len() > 1);
    if main || phrase {
        return Err(Error::Tantivy(tantivy::TantivyError::InvalidArgument(
            "relevance order of a multi-word phrase, proximity or fuzzy query needs an index built with positions"
                .into(),
        )));
    }
    Ok(())
}

/// The typed words of a slot (its roots for the tiers).
fn roots(g: &GroupSpec, out: &mut Vec<String>) {
    match g {
        GroupSpec::Word { word, .. }
        | GroupSpec::Grammar { word, .. }
        | GroupSpec::Fuzzy { word, .. }
        | GroupSpec::Unindexed { word } => out.push(word.clone()),
        // a user regex's pattern is no typed form: its terms all fall in the last tier
        GroupSpec::Wildcard { pattern, .. } | GroupSpec::Regex { pattern } => out.push(pattern.clone()),
        GroupSpec::Any { alternatives } => alternatives.iter().for_each(|a| roots(a, out)),
    }
}

/// The first typed word of a slot: its term for the phrase's IDF.
fn first_root(g: &GroupSpec) -> String {
    let mut r = Vec::new();
    roots(g, &mut r);
    r.into_iter().next().unwrap_or_default()
}

/// Otzaria's final letters in their base form (`base_form_of_final`).
fn base_form_of_final(c: char) -> Option<char> {
    match c {
        '\u{05DA}' => Some('\u{05DB}'),
        '\u{05DD}' => Some('\u{05DE}'),
        '\u{05DF}' => Some('\u{05E0}'),
        '\u{05E3}' => Some('\u{05E4}'),
        '\u{05E5}' => Some('\u{05E6}'),
        _ => None,
    }
}

/// Otzaria's `match_tier` (`hebrew_query.rs`), verbatim: how closely a
/// matched term reproduces what was typed. 0 the typed form (or an
/// alternative) itself, 1 that form inside a longer word (prefixes /
/// suffixes, a final letter in its base form, final he to tav / vav-tav),
/// 2 a ktiv male/haser variant (vav / yod and quotes aside), 3 the rest.
pub(crate) fn match_tier(term: &str, roots: &[String]) -> u8 {
    fn unquoted(s: &str) -> String {
        s.chars().filter(|c| !matches!(c, '\'' | '"')).collect()
    }
    fn skeleton(s: &str) -> String {
        s.chars()
            .filter(|c| !matches!(c, '\'' | '"' | '\u{05D5}' | '\u{05D9}'))
            .map(|c| base_form_of_final(c).unwrap_or(c))
            .collect()
    }
    let roots: Vec<String> = roots.iter().map(|r| unquoted(r)).filter(|r| !r.is_empty()).collect();
    let term = unquoted(term);
    if roots.contains(&term) {
        return 0;
    }
    let inside = |root: &str| {
        let Some(last) = root.chars().last() else {
            return false;
        };
        let head = &root[..root.len() - last.len_utf8()];
        let stems: Vec<String> = match (base_form_of_final(last), last) {
            (Some(base), _) => vec![format!("{head}{base}")],
            (None, '\u{05D4}') => vec![format!("{head}\u{05EA}"), format!("{head}\u{05D5}\u{05EA}")],
            _ => Vec::new(),
        };
        term.contains(root) || stems.iter().any(|stem| term.contains(stem.as_str()))
    };
    if roots.iter().any(|r| inside(r)) {
        return 1;
    }
    let term_skeleton = skeleton(&term);
    if roots.iter().any(|r| {
        let root = skeleton(r);
        !root.is_empty() && term_skeleton.contains(&root)
    }) {
        return 2;
    }
    3
}

/// Otzaria's `quoteless_variant`: the word without its quotes, when it has some.
fn quoteless(word: &str) -> Option<String> {
    if !word.contains(['"', '\'']) {
        return None;
    }
    let clean: String = word.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
    (!clean.is_empty()).then_some(clean)
}

/// Otzaria's `quote_expansion::spellings`: `word` first (indexed or not),
/// then every term of the index that is `word` with quotes inserted (none
/// when `word` has a quote itself).
fn spellings(searcher: &Searcher, field: Field, word: &str) -> Result<Vec<String>> {
    let mut out = vec![word.to_string()];
    if word.contains(['"', '\'']) {
        return Ok(out);
    }
    let automaton = hebrew_fts::quote_free::QuoteFree::new(hebrew_fts::SetAutomaton::from_strings([word]), true);
    for reader in searcher.segment_readers() {
        let inverted = reader.inverted_index(field)?;
        let mut stream = inverted.terms().search(&automaton).into_stream()?;
        while stream.advance() {
            if let Ok(term) = std::str::from_utf8(stream.key()) {
                if !out.iter().any(|known| known == term) {
                    out.push(term.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// Otzaria's `SharedStatistics`: the spellings of one word count as one
/// term, their doc frequencies summed (at most every doc).
struct SharedStatistics<'a> {
    inner: &'a Searcher,
    group: &'a [Term],
}

impl Bm25StatisticsProvider for SharedStatistics<'_> {
    fn total_num_tokens(&self, field: Field) -> tantivy::Result<u64> {
        self.inner.total_num_tokens(field)
    }

    fn total_num_docs(&self) -> tantivy::Result<u64> {
        self.inner.total_num_docs()
    }

    fn doc_freq(&self, term: &Term) -> tantivy::Result<u64> {
        if self.group.contains(term) {
            let mut sum = 0u64;
            for t in self.group {
                sum += self.inner.doc_freq(t)?;
            }
            Ok(sum.min(self.inner.total_num_docs()?))
        } else {
            self.inner.doc_freq(term)
        }
    }
}

/// The union of several terms' postings in one segment, positioned on one
/// doc at a time: frequencies summed, positions merged (sorted, deduped, as
/// tantivy's own posting unions).
struct TermUnion {
    postings: Vec<SegmentPostings>,
    /// Postings past the current doc, smallest doc on top.
    heap: BinaryHeap<Reverse<(DocId, usize)>>,
    /// Postings on the current doc.
    on_doc: Vec<usize>,
    doc: DocId,
}

impl TermUnion {
    /// None when no term is in this segment.
    fn open<S: AsRef<str>>(
        inverted: &InvertedIndexReader,
        field: Field,
        terms: &[S],
        option: IndexRecordOption,
    ) -> Result<Option<TermUnion>> {
        let mut postings = Vec::new();
        for t in terms {
            if let Some(p) = inverted.read_postings(&Term::from_field_text(field, t.as_ref()), option)? {
                postings.push(p);
            }
        }
        if postings.is_empty() {
            return Ok(None);
        }
        let heap = postings
            .iter()
            .enumerate()
            .filter(|(_, p)| p.doc() != TERMINATED)
            .map(|(i, p)| Reverse((p.doc(), i)))
            .collect();
        let mut u = TermUnion { postings, heap, on_doc: Vec::new(), doc: TERMINATED };
        u.settle();
        Ok(Some(u))
    }

    /// Moves the smallest doc's postings out of the heap.
    fn settle(&mut self) {
        self.on_doc.clear();
        let Some(&Reverse((doc, _))) = self.heap.peek() else {
            self.doc = TERMINATED;
            return;
        };
        self.doc = doc;
        while let Some(&Reverse((d, i))) = self.heap.peek() {
            if d != doc {
                break;
            }
            self.heap.pop();
            self.on_doc.push(i);
        }
    }

    fn reinsert(&mut self, i: usize) {
        let d = self.postings[i].doc();
        if d != TERMINATED {
            self.heap.push(Reverse((d, i)));
        }
    }

    /// True when the union holds `doc` (docs asked in ascending order).
    fn holds(&mut self, doc: DocId) -> bool {
        self.seek(doc) == doc
    }
}

impl DocSet for TermUnion {
    fn advance(&mut self) -> DocId {
        for i in std::mem::take(&mut self.on_doc) {
            self.postings[i].advance();
            self.reinsert(i);
        }
        self.settle();
        self.doc
    }

    fn seek(&mut self, target: DocId) -> DocId {
        if self.doc >= target {
            return self.doc;
        }
        for i in std::mem::take(&mut self.on_doc) {
            self.postings[i].seek(target);
            self.reinsert(i);
        }
        while let Some(&Reverse((d, i))) = self.heap.peek() {
            if d >= target {
                break;
            }
            self.heap.pop();
            self.postings[i].seek(target);
            self.reinsert(i);
        }
        self.settle();
        self.doc
    }

    fn doc(&self) -> DocId {
        self.doc
    }

    fn size_hint(&self) -> u32 {
        self.postings.iter().fold(0u32, |a, p| a.saturating_add(p.size_hint()))
    }
}

impl Postings for TermUnion {
    fn term_freq(&self) -> u32 {
        self.on_doc.iter().map(|&i| self.postings[i].term_freq()).sum()
    }

    fn append_positions_with_offset(&mut self, offset: u32, output: &mut Vec<u32>) {
        for &i in &self.on_doc {
            self.postings[i].append_positions_with_offset(offset, output);
        }
        output.sort_unstable();
        output.dedup();
    }
}

/// One part of a score, opened on one segment, asked docs in ascending order.
enum Part {
    /// `boost` when the union holds the doc.
    Const { union: TermUnion, boost: Score },
    /// Fuzzy mode's exact word: floor + BM25 over the spellings' summed frequencies.
    Exact { union: TermUnion, bm25: Bm25Weight, fieldnorms: tantivy::fieldnorm::FieldNormReader },
    /// Phrase BM25 (`similarity` Some) or a constant when it exists.
    Phrase { scorer: Box<PhraseScorer<TermUnion>>, constant: Option<Score> },
    /// The union count when every union holds the doc (Otzaria's all-required
    /// const-1 words), else 0.
    AllOf { unions: Vec<TermUnion> },
    /// Any order: slot count + 1 / (1 + tightest span).
    Span { unions: Vec<TermUnion>, positions: Vec<Vec<u32>> },
}

impl Part {
    fn score(&mut self, doc: DocId) -> Score {
        match self {
            Part::Const { union, boost } => {
                if union.holds(doc) {
                    *boost
                } else {
                    0.0
                }
            }
            Part::Exact { union, bm25, fieldnorms } => {
                if !union.holds(doc) {
                    return 0.0;
                }
                FUZZY_BOOST_EXACT
                    + FUZZY_BOOST_EXACT_REL * bm25.score(fieldnorms.fieldnorm_id(doc), union.term_freq().max(1))
            }
            Part::Phrase { scorer, constant } => {
                if scorer.doc() < doc {
                    scorer.seek(doc);
                }
                if scorer.doc() != doc {
                    return 0.0;
                }
                match constant {
                    Some(c) => *c,
                    None => tantivy::query::Scorer::score(scorer.as_mut()),
                }
            }
            Part::AllOf { unions } => {
                let mut all = true;
                for u in unions.iter_mut() {
                    // every union is asked, so each stays positioned for the next doc
                    all &= u.holds(doc);
                }
                if all {
                    unions.len() as Score
                } else {
                    0.0
                }
            }
            Part::Span { unions, positions } => {
                for (u, p) in unions.iter_mut().zip(positions.iter_mut()) {
                    if !u.holds(doc) {
                        return 0.0;
                    }
                    u.positions(p);
                }
                let Some(span) = tightest_span(positions) else {
                    return 0.0;
                };
                // Otzaria: the driver's k const-1 clauses + the span bonus.
                unions.len() as Score + 1.0 / (1.0 + span as Score)
            }
        }
    }
}

/// The smallest `last - first` over windows where every slot takes its own
/// position (each slot's positions sorted). None when the slots cannot all
/// take distinct positions.
fn tightest_span(slots: &[Vec<u32>]) -> Option<u32> {
    let k = slots.len();
    // distinct positions in order, with the slots each can fill
    let mut entries: Vec<(u32, usize)> = Vec::new();
    for (s, ps) in slots.iter().enumerate() {
        entries.extend(ps.iter().map(|&p| (p, s)));
    }
    entries.sort_unstable();
    let mut positions: Vec<u32> = Vec::new();
    let mut fill: Vec<Vec<usize>> = Vec::new();
    for (p, s) in entries {
        if positions.last() != Some(&p) {
            positions.push(p);
            fill.push(Vec::new());
        }
        let f = fill.last_mut().unwrap();
        if !f.contains(&s) {
            f.push(s);
        }
    }
    let feasible = |l: usize, r: usize| -> bool {
        if r + 1 - l < k {
            return false;
        }
        // Kuhn's matching of slots to distinct positions in l..=r
        let mut owner: Vec<Option<usize>> = vec![None; r + 1 - l];
        fn augment(
            slot: usize,
            l: usize,
            r: usize,
            fill: &[Vec<usize>],
            owner: &mut [Option<usize>],
            seen: &mut [bool],
        ) -> bool {
            for e in l..=r {
                if !fill[e].contains(&slot) || seen[e - l] {
                    continue;
                }
                seen[e - l] = true;
                let prev = owner[e - l];
                if prev.is_none() || augment(prev.unwrap(), l, r, fill, owner, seen) {
                    owner[e - l] = Some(slot);
                    return true;
                }
            }
            false
        }
        (0..k).all(|slot| {
            let mut seen = vec![false; r + 1 - l];
            augment(slot, l, r, &fill, &mut owner, &mut seen)
        })
    };
    let mut best: Option<u32> = None;
    let mut l = 0;
    for r in 0..positions.len() {
        if !feasible(l, r) {
            continue;
        }
        while l < r && feasible(l + 1, r) {
            l += 1;
        }
        let span = positions[r] - positions[l];
        best = Some(best.map_or(span, |b| b.min(span)));
    }
    best
}

/// The scores of `keys` (`(segment ordinal, doc)`, segments ascending and
/// docs ascending within one): Otzaria's relevance (see the module docs).
/// [`Error::Cancelled`] once `cancel` is cancelled (checked every 4096 hits).
pub(crate) fn scores(
    searcher: &Searcher,
    plan: &ScorePlan<'_>,
    keys: &[(u32, DocId)],
    cancel: Option<&CancelToken>,
) -> Result<Vec<Score>> {
    let field = plan.field;
    // Per-search, segment-independent pieces: the tiers and the weights.
    let main = match &plan.main {
        Some(m) => Some(MainWeights::new(searcher, plan, m)?),
        None => None,
    };
    let phrase_weights: Vec<Option<Bm25Weight>> = plan
        .phrases
        .iter()
        .map(|(groups, _)| -> Result<Option<Bm25Weight>> {
            if groups.len() < 2 || plan.phrases_as_count {
                return Ok(None);
            }
            let terms: Vec<Term> = groups.iter().map(|g| Term::from_field_text(field, &first_root(g))).collect();
            Ok(Some(Bm25Weight::for_terms(searcher, &terms)?))
        })
        .collect::<Result<_>>()?;

    // the unions and the phrase scorers only seek forward
    debug_assert!(keys.windows(2).all(|w| w[0] < w[1]), "keys must ascend");
    let mut out = Vec::with_capacity(keys.len());
    let mut start = 0;
    while start < keys.len() {
        let ord = keys[start].0;
        let end = start + keys[start..].iter().take_while(|k| k.0 == ord).count();
        let segment = searcher.segment_reader(ord);
        let inverted = segment.inverted_index(field)?;
        let mut main_parts = match (&plan.main, &main) {
            (Some(m), Some(w)) => w.open(segment, &inverted, field, m)?,
            _ => Vec::new(),
        };
        let mut membership = match &plan.main_membership {
            Some(w) => Some(w.scorer(segment, 1.0)?),
            None => None,
        };
        let mut phrase_parts: Vec<Vec<Part>> = Vec::with_capacity(plan.phrases.len());
        for ((_, terms), weight) in plan.phrases.iter().zip(&phrase_weights) {
            phrase_parts.push(match weight {
                None => {
                    // one word: 1; several words anywhere: 1 per word, all required
                    let mut unions = Vec::with_capacity(terms.len());
                    for t in terms {
                        match TermUnion::open(&inverted, field, t, IndexRecordOption::Basic)? {
                            Some(u) => unions.push(u),
                            None => {
                                unions.clear();
                                break;
                            }
                        }
                    }
                    if unions.is_empty() {
                        Vec::new()
                    } else {
                        vec![Part::AllOf { unions }]
                    }
                }
                Some(bm25) => phrase_part(segment, &inverted, field, terms, Some(bm25.clone()), 0, None)?
                    .into_iter()
                    .collect(),
            });
        }
        for (n, &(_, doc)) in keys[start..end].iter().enumerate() {
            if n % 4096 == 4095 && cancel.is_some_and(|t| t.is_cancelled()) {
                return Err(Error::Cancelled);
            }
            let in_main = match membership.as_mut() {
                Some(s) => {
                    if s.doc() < doc {
                        s.seek(doc);
                    }
                    s.doc() == doc
                }
                None => true,
            };
            let mut score: Score = 0.0;
            if in_main {
                for p in &mut main_parts {
                    score += p.score(doc);
                }
            }
            for parts in &mut phrase_parts {
                for p in parts {
                    score += p.score(doc);
                }
            }
            out.push(score);
        }
        start = end;
    }
    Ok(out)
}

/// A phrase of `terms` (one term list per slot, offsets 0..k) on one
/// segment; None when a slot has no term there.
fn phrase_part(
    segment: &SegmentReader,
    inverted: &InvertedIndexReader,
    field: Field,
    terms: &[Vec<String>],
    bm25: Option<Bm25Weight>,
    slop: u32,
    constant: Option<Score>,
) -> Result<Option<Part>> {
    let mut slots = Vec::with_capacity(terms.len());
    for (i, t) in terms.iter().enumerate() {
        match TermUnion::open(inverted, field, t, IndexRecordOption::WithFreqsAndPositions)? {
            Some(u) => slots.push((i, u)),
            None => return Ok(None),
        }
    }
    let fieldnorms = segment.get_fieldnorms_reader(field)?;
    Ok(Some(Part::Phrase { scorer: Box::new(PhraseScorer::new(slots, bm25, fieldnorms, slop)), constant }))
}

/// The main query's segment-independent pieces.
struct MainWeights {
    /// Single advanced slot: the matched terms by tier (None: flat 1).
    tiers: Option<[Vec<String>; 4]>,
    /// Per fuzzy slot (aligned with the slots; None for other slots):
    /// the exact words' spellings with their BM25 weights.
    fuzzy: Vec<Option<Vec<(Vec<String>, Bm25Weight)>>>,
    /// Phrase shape's BM25.
    phrase: Option<Bm25Weight>,
    /// Fuzzy mode's adjacency boost: the typed words' spellings.
    fuzzy_phrase: Option<Vec<Vec<String>>>,
}

impl MainWeights {
    fn new(searcher: &Searcher, plan: &ScorePlan<'_>, m: &MainPlan<'_>) -> Result<MainWeights> {
        let field = plan.field;
        let ranked = !plan.keep_marks;
        let mut w = MainWeights { tiers: None, fuzzy: vec![None; m.slots.len()], phrase: None, fuzzy_phrase: None };
        let fuzzy_slot = |g: &GroupSpec| -> Result<Option<Vec<(Vec<String>, Bm25Weight)>>> {
            let GroupSpec::Fuzzy { word, distance, .. } = g else {
                return Ok(None);
            };
            // Otzaria ranks fuzzy words only above distance 0, and never on its vocalized field.
            if !ranked || *distance == 0 {
                return Ok(None);
            }
            let mut exact = Vec::new();
            for x in std::iter::once(word.clone()).chain(quoteless(word)) {
                let s = spellings(searcher, field, &x)?;
                let group: Vec<Term> = s.iter().map(|t| Term::from_field_text(field, t)).collect();
                let stats = SharedStatistics { inner: searcher, group: &group };
                let bm25 = Bm25Weight::for_terms(&stats, &group[..1])?;
                exact.push((s, bm25));
            }
            Ok(Some(exact))
        };
        match m.shape {
            Shape::Single => {
                if let GroupSpec::Fuzzy { .. } = m.slots[0] {
                    w.fuzzy[0] = fuzzy_slot(m.slots[0])?;
                } else if ranked {
                    let mut r = Vec::new();
                    roots(m.slots[0], &mut r);
                    let mut tiers: [Vec<String>; 4] = Default::default();
                    for t in &m.terms[0] {
                        tiers[usize::from(match_tier(t, &r))].push(t.clone());
                    }
                    w.tiers = Some(tiers);
                }
            }
            Shape::Count { partial } => {
                for (i, g) in m.slots.iter().enumerate() {
                    w.fuzzy[i] = fuzzy_slot(g)?;
                }
                if ranked && !partial && m.slots.len() > 1 && all_fuzzy(&m.slots) {
                    let mut words = Vec::with_capacity(m.slots.len());
                    for g in &m.slots {
                        words.push(spellings(searcher, field, &first_root(g))?);
                    }
                    w.fuzzy_phrase = Some(words);
                }
            }
            Shape::Phrase { .. } => {
                let terms: Vec<Term> = m.slots.iter().map(|g| Term::from_field_text(field, &first_root(g))).collect();
                w.phrase = Some(Bm25Weight::for_terms(searcher, &terms)?);
            }
            Shape::AnyOrder => {}
        }
        Ok(w)
    }

    fn open(&self, segment: &SegmentReader, inverted: &InvertedIndexReader, field: Field, m: &MainPlan<'_>) -> Result<Vec<Part>> {
        let mut parts = Vec::new();
        let basic = IndexRecordOption::Basic;
        let slot_const = |terms: &[String], boost: Score, parts: &mut Vec<Part>| -> Result<()> {
            if let Some(union) = TermUnion::open(inverted, field, terms, basic)? {
                parts.push(Part::Const { union, boost });
            }
            Ok(())
        };
        let fuzzy_parts = |exact: &[(Vec<String>, Bm25Weight)], terms: &[String], parts: &mut Vec<Part>| -> Result<()> {
            let fieldnorms = segment.get_fieldnorms_reader(field)?;
            for (spellings, bm25) in exact {
                if let Some(union) = TermUnion::open(inverted, field, spellings, IndexRecordOption::WithFreqs)? {
                    parts.push(Part::Exact { union, bm25: bm25.clone(), fieldnorms: fieldnorms.clone() });
                }
            }
            slot_const(terms, FUZZY_BOOST_FUZZY, parts)
        };
        match m.shape {
            Shape::Single => match (&self.tiers, &self.fuzzy[0]) {
                (Some(tiers), _) => {
                    for (terms, boost) in tiers.iter().zip(TIER_BOOSTS) {
                        if !terms.is_empty() {
                            slot_const(terms, boost, &mut parts)?;
                        }
                    }
                }
                (None, Some(exact)) => fuzzy_parts(exact, &m.terms[0], &mut parts)?,
                (None, None) => slot_const(&m.terms[0], 1.0, &mut parts)?,
            },
            Shape::Count { .. } => {
                for (i, terms) in m.terms.iter().enumerate() {
                    match &self.fuzzy[i] {
                        Some(exact) => fuzzy_parts(exact, terms, &mut parts)?,
                        None => slot_const(terms, 1.0, &mut parts)?,
                    }
                }
                if let Some(words) = &self.fuzzy_phrase {
                    if let Some(p) = phrase_part(segment, inverted, field, words, None, FUZZY_PHRASE_SLOP, Some(FUZZY_BOOST_PHRASE))? {
                        parts.push(p);
                    }
                }
            }
            Shape::Phrase { slop } => {
                if let Some(p) = phrase_part(segment, inverted, field, &m.terms, self.phrase.clone(), slop, None)? {
                    parts.push(p);
                }
            }
            Shape::AnyOrder => {
                let mut unions = Vec::with_capacity(m.terms.len());
                for t in &m.terms {
                    match TermUnion::open(inverted, field, t, IndexRecordOption::WithFreqsAndPositions)? {
                        Some(u) => unions.push(u),
                        None => return Ok(parts),
                    }
                }
                let positions = vec![Vec::new(); unions.len()];
                parts.push(Part::Span { unions, positions });
            }
        }
        Ok(parts)
    }
}

/// Orders `hits` (aligned with `scores`) best first, ties by catalog position.
pub(crate) fn sort_by_score(hits: Vec<PlacedHit>, scores: Vec<Score>) -> (Vec<PlacedHit>, Vec<Score>) {
    let mut pairs: Vec<(PlacedHit, Score)> = hits.into_iter().zip(scores).collect();
    pairs.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.position.cmp(&b.0.position)));
    pairs.into_iter().unzip()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_tiers_as_otzaria() {
        // Otzaria's own test (`match_tiers_order_exact_affix_spelling_typo`), its words escaped.
        let roots = vec!["\u{05EA}\u{05D5}\u{05E8}\u{05D4}".to_string(), "\u{05E1}\u{05E4}\u{05E8}".to_string()];
        for (term, tier) in [
            ("\u{05EA}\u{05D5}\u{05E8}\u{05D4}", 0),
            ("\u{05E1}\u{05E4}\u{05E8}", 0),
            ("\u{05D5}\u{05D1}\u{05EA}\u{05D5}\u{05E8}\u{05D4}", 1),
            ("\u{05EA}\u{05D5}\u{05E8}\u{05EA}\u{05D5}", 1),
            ("\u{05EA}\u{05D5}\u{05E8}\u{05D5}\u{05EA}", 1),
            ("\u{05E1}\u{05E4}\u{05E8}\u{05D9}\u{05DD}", 1),
            ("\u{05EA}\u{05D5}\u{05E8}\u{05D9}\u{05D4}", 2),
            ("\u{05D5}\u{05D1}\u{05EA}\u{05D5}\u{05E8}\u{05D9}\u{05D4}", 2),
            ("\u{05E1}\u{05D5}\u{05E4}\u{05E8}", 2),
            ("\u{05EA}\u{05D5}\u{05D3}\u{05D4}", 3),
            ("\u{05E1}\u{05E4}\u{05D8}", 3),
        ] {
            assert_eq!(match_tier(term, &roots), tier, "{}", term.escape_unicode());
        }
        let roots = vec!["\u{05E8}\u{05DE}\u{05D1}\"\u{05DD}".to_string(), "\u{05DE}\u{05DC}\u{05DA}".to_string()];
        assert_eq!(match_tier("\u{05E8}\u{05DE}\u{05D1}\u{05DD}", &roots), 0);
        assert_eq!(match_tier("\u{05DC}\u{05D4}\u{05E8}\u{05DE}\u{05D1}\"\u{05DD}", &roots), 1);
        assert_eq!(match_tier("\u{05DE}\u{05DC}\u{05DB}\u{05D9}\u{05DD}", &roots), 1);
    }

    use crate::search::{ProximitySpec, SearchRequest, WalkOptionsSpec};
    use crate::{IndexOptions, LineDoc, LineIndex, SearchOptions};

    // words (escaped: no Hebrew in the sources' test output)
    const A: &str = "\u{05D1}\u{05D9}\u{05EA}";
    const B: &str = "\u{05D4}\u{05DE}\u{05E7}\u{05D3}\u{05E9}";
    const C: &str = "\u{05D0}\u{05D1}\u{05D2}";
    const D: &str = "\u{05D3}\u{05D4}\u{05D5}";
    const E: &str = "\u{05D6}\u{05D7}\u{05D8}";
    const X: &str = "\u{05DB}\u{05DC}\u{05DD}";
    const Y: &str = "\u{05E0}\u{05E1}\u{05E2}";

    fn build(options: IndexOptions, texts: &[(u64, &str)]) -> (tempfile::TempDir, LineIndex) {
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), options).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        for (i, (book, text)) in texts.iter().enumerate() {
            w.add(&LineDoc { line_id: 100 + i as u64, position: i as u64, book_id: *book, text: (*text).into() })
                .unwrap();
        }
        w.commit(None).unwrap();
        (dir, index)
    }

    fn word(w: &str) -> GroupSpec {
        GroupSpec::Word { word: w.into(), options: WalkOptionsSpec::default() }
    }

    fn ids(r: &crate::SearchResult) -> Vec<u64> {
        r.hits.iter().map(|h| h.line_id - 100).collect()
    }

    fn relevance(groups: Vec<GroupSpec>) -> SearchRequest {
        SearchRequest { groups, order: ResultsOrder::Relevance, ..Default::default() }
    }

    /// tantivy's own phrase scores (BM25 over the phrase count) for these
    /// exact words: the parity reference for the phrase shape.
    fn tantivy_phrase_scores(index: &LineIndex, words: &[&str], slop: u32) -> Vec<Score> {
        let field = index.fields.text;
        let terms: Vec<Term> = words.iter().map(|w| Term::from_field_text(field, w)).collect();
        let mut q = tantivy::query::PhraseQuery::new(terms);
        q.set_slop(slop);
        let searcher = index.index.reader().unwrap().searcher();
        let top = searcher.search(&q, &tantivy::collector::TopDocs::with_limit(100).order_by_score()).unwrap();
        top.into_iter().map(|(s, _)| s).collect()
    }

    #[test]
    fn catalog_is_the_default_and_scores_nothing() {
        let (_d, index) = build(IndexOptions::default(), &[(1, A), (1, A)]);
        let r = index.search(&SearchRequest { groups: vec![word(A)], ..Default::default() }).unwrap();
        assert_eq!(ids(&r), vec![0, 1]);
        assert!(r.scores().is_none());
    }

    #[test]
    fn generation_order_by_book_then_catalog() {
        let (_d, index) = build(IndexOptions::default(), &[(1, A), (2, A), (3, A), (1, A), (3, A)]);
        let r = index
            .search(&SearchRequest {
                groups: vec![word(A)],
                order: ResultsOrder::Generation,
                // book 3 first, then book 1; book 2 has no generation: last
                book_generations: vec![(1, 7), (3, 2)],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(ids(&r), vec![2, 4, 0, 3, 1]);
        assert!(r.scores().is_none());
    }

    #[test]
    fn single_word_tiers() {
        // mitzvot: the typed form, with a vav prefix, its ktiv male form
        let typed = "\u{05DE}\u{05E6}\u{05D5}\u{05EA}";
        let prefixed = "\u{05D5}\u{05DE}\u{05E6}\u{05D5}\u{05EA}";
        let male = "\u{05DE}\u{05E6}\u{05D5}\u{05D5}\u{05EA}";
        let (_d, index) = build(
            IndexOptions::default(),
            &[(1, male), (1, prefixed), (1, typed), (1, &format!("{typed} {prefixed}")), (1, C)],
        );
        let g = GroupSpec::Word {
            word: typed.into(),
            options: WalkOptionsSpec { grammar_prefixes: true, ktiv_male_haser: true, ..Default::default() },
        };
        let r = index.search(&relevance(vec![g])).unwrap();
        assert_eq!(ids(&r), vec![3, 2, 1, 0]);
        assert_eq!(r.scores().unwrap(), &[1100.0, 1000.0, 100.0, 10.0]);

        // the cancellable collection keeps the doc ids too: same order, same scores
        let g = GroupSpec::Word {
            word: typed.into(),
            options: WalkOptionsSpec { grammar_prefixes: true, ktiv_male_haser: true, ..Default::default() },
        };
        let token = crate::search::CancelToken::new();
        let r = index.search_cancellable(&relevance(vec![g]), Some(token)).unwrap();
        assert_eq!(ids(&r), vec![3, 2, 1, 0]);
        assert_eq!(r.scores().unwrap(), &[1100.0, 1000.0, 100.0, 10.0]);

        // on an index without positions the tiers are the same
        let (_d2, flat) = build(
            IndexOptions { positions: false, keep_marks: false },
            &[(1, male), (1, prefixed), (1, typed)],
        );
        let g = GroupSpec::Word {
            word: typed.into(),
            options: WalkOptionsSpec { grammar_prefixes: true, ktiv_male_haser: true, ..Default::default() },
        };
        let r = flat.search(&relevance(vec![g])).unwrap();
        assert_eq!(ids(&r), vec![2, 1, 0]);
    }

    #[test]
    fn ordered_phrase_is_tantivys_bm25() {
        let texts = [
            (1, format!("{A} {B} {C} {D} {E} {C}")), // once, long line
            (1, format!("{A} {B}")),                 // once, short line
            (1, format!("{A} {B} {C} {A} {B}")),     // twice
            (1, format!("{A} {C} {B}")),             // one word between
        ];
        let texts: Vec<(u64, &str)> = texts.iter().map(|(b, t)| (*b, t.as_str())).collect();
        let (_d, index) = build(IndexOptions::default(), &texts);
        for gap in [0u32, 1] {
            let mut req = relevance(vec![word(A), word(B)]);
            req.proximity = Some(ProximitySpec::Ordered { gaps: vec![gap] });
            let r = index.search(&req).unwrap();
            let expected = tantivy_phrase_scores(&index, &[A, B], gap);
            assert_eq!(r.scores().unwrap(), expected.as_slice(), "gap {gap}");
            if gap == 0 {
                assert_eq!(ids(&r), vec![2, 1, 0]);
            } else {
                assert_eq!(ids(&r).len(), 4);
            }
        }
        // per-pair `gaps` (span query) scores the same way
        let mut req = relevance(vec![word(A), word(B)]);
        req.gaps = Some(vec![0]);
        req.require_ordered = true;
        let r = index.search(&req).unwrap();
        assert_eq!(r.scores().unwrap(), tantivy_phrase_scores(&index, &[A, B], 0).as_slice());
    }

    #[test]
    fn any_order_counts_words_plus_span_bonus() {
        let texts = [format!("{B} {C} {A}"), format!("{A} {B}"), format!("{B} {A}"), format!("{A} {C} {D} {E} {B}")];
        let texts: Vec<(u64, &str)> = texts.iter().map(|t| (1, t.as_str())).collect();
        let (_d, index) = build(IndexOptions::default(), &texts);
        let mut req = relevance(vec![word(A), word(B)]);
        req.proximity = Some(ProximitySpec::AnyOrder { distance: 2 });
        let r = index.search(&req).unwrap();
        // span 1 (ties in catalog order), then span 2; span 4 is outside the window
        assert_eq!(ids(&r), vec![1, 2, 0]);
        assert_eq!(r.scores().unwrap(), &[2.0 + 1.0 / 2.0, 2.0 + 1.0 / 2.0, 2.0 + 1.0 / 3.0]);
    }

    #[test]
    fn words_anywhere_and_partial_count_the_words() {
        let texts = [format!("{A} {C}"), format!("{A} {B} {C}"), format!("{B} {D}"), format!("{C} {A} {B}")];
        let texts: Vec<(u64, &str)> = texts.iter().map(|t| (1, t.as_str())).collect();
        let (_d, index) = build(IndexOptions { positions: false, keep_marks: false }, &texts);
        // all words, anywhere: every hit scores 2, catalog order
        let r = index.search(&relevance(vec![word(A), word(B)])).unwrap();
        assert_eq!(ids(&r), vec![1, 3]);
        assert_eq!(r.scores().unwrap(), &[2.0, 2.0]);
        // any word of three: the lines holding more words first
        let mut req = relevance(vec![word(A), word(B), word(D)]);
        req.min_groups = Some(1);
        let r = index.search(&req).unwrap();
        assert_eq!(ids(&r), vec![1, 2, 3, 0]);
        assert_eq!(r.scores().unwrap(), &[2.0, 2.0, 2.0, 1.0]);
    }

    #[test]
    fn fuzzy_exact_floor_plus_bm25() {
        let shalom = "\u{05E9}\u{05DC}\u{05D5}\u{05DD}";
        let near = "\u{05E9}\u{05DC}\u{05DD}"; // one deletion
        let (_d, index) = build(IndexOptions::default(), &[(1, near), (1, &format!("{shalom} {C}")), (1, C)]);
        let fuzzy = GroupSpec::Fuzzy { word: shalom.into(), distance: 1, ignore_quotes: true };
        let r = index.search(&relevance(vec![fuzzy])).unwrap();
        assert_eq!(ids(&r), vec![1, 0]);
        // tantivy's TermQuery BM25 for the exact word, as Otzaria's exact clause
        let searcher = index.index.reader().unwrap().searcher();
        let tq = tantivy::query::TermQuery::new(
            Term::from_field_text(index.fields.text, shalom),
            IndexRecordOption::WithFreqs,
        );
        let top = searcher.search(&tq, &tantivy::collector::TopDocs::with_limit(1).order_by_score()).unwrap();
        let s = r.scores().unwrap();
        assert_eq!(s[0], 1000.0 + top[0].0 + 1.0);
        assert_eq!(s[1], 1.0);

        // two fuzzy words, adjacent within one word: + 2000
        let texts = [format!("{shalom} {C} {D} {A}"), format!("{shalom} {C} {A}")];
        let texts: Vec<(u64, &str)> = texts.iter().map(|t| (1, t.as_str())).collect();
        let (_d2, index) = build(IndexOptions::default(), &texts);
        let f = |w: &str| GroupSpec::Fuzzy { word: w.into(), distance: 1, ignore_quotes: true };
        let r = index.search(&relevance(vec![f(shalom), f(A)])).unwrap();
        assert_eq!(ids(&r), vec![1, 0]);
        let s = r.scores().unwrap();
        assert!(s[0] > 4000.0 && s[0] - s[1] > 1999.0, "{s:?}");
    }

    #[test]
    fn phrase_alternative_adds_only_to_its_own_hits() {
        let texts = [format!("{X} {Y}"), format!("{X} {A} {B}"), format!("{C} {A} {B}")];
        let texts: Vec<(u64, &str)> = texts.iter().map(|t| (1, t.as_str())).collect();
        let (_d, index) = build(IndexOptions::default(), &texts);
        // main query a phrase: each alternative is a phrase BM25 too
        let mut req = relevance(vec![word(X), word(Y)]);
        req.proximity = Some(ProximitySpec::Ordered { gaps: vec![0] });
        req.phrase_alternatives = vec![vec![word(A), word(B)]];
        let r = index.search(&req).unwrap();
        let s = r.scores().unwrap().to_vec();
        let main = tantivy_phrase_scores(&index, &[X, Y], 0);
        let phrase = tantivy_phrase_scores(&index, &[A, B], 0);
        let by_id: std::collections::HashMap<u64, Score> = ids(&r).into_iter().zip(s).collect();
        assert_eq!(by_id[&0], main[0]);
        assert_eq!(by_id[&1], phrase[0]);
        assert_eq!(by_id[&2], phrase[1]);

        // main query words anywhere (paragraph scope): the alternative is a
        // word count; line 1 holds X but not the main query, so only the
        // alternative's 2 (the main query's part would have added 1)
        let mut req = relevance(vec![word(X), word(Y)]);
        req.phrase_alternatives = vec![vec![word(A), word(B)]];
        let r = index.search(&req).unwrap();
        assert_eq!(ids(&r), vec![0, 1, 2]);
        assert_eq!(r.scores().unwrap(), &[2.0, 2.0, 2.0]);
        // ... which needs no positions
        let (_d2, flat) = build(IndexOptions { positions: false, keep_marks: false }, &texts);
        let r = flat.search(&req).unwrap();
        assert_eq!(r.scores().unwrap(), &[2.0, 2.0, 2.0]);
    }

    #[test]
    fn unordered_gaps_score_as_any_order() {
        let texts = [format!("{A} {B}"), format!("{B} {A}"), format!("{B} {C} {A}")];
        let texts: Vec<(u64, &str)> = texts.iter().map(|t| (1, t.as_str())).collect();
        let (_d, index) = build(IndexOptions::default(), &texts);
        let mut req = relevance(vec![word(A), word(B)]);
        req.gaps = Some(vec![1]);
        req.require_ordered = false;
        let r = index.search(&req).unwrap();
        assert_eq!(ids(&r), vec![0, 1, 2]);
        assert_eq!(r.scores().unwrap(), &[2.5, 2.5, 2.0 + 1.0 / 3.0]);
    }

    #[test]
    fn positions_needed_for_phrases_on_a_flat_index() {
        let (_d, flat) = build(IndexOptions { positions: false, keep_marks: false }, &[(1, &format!("{A} {B}"))]);
        let mut req = relevance(vec![word(A), word(B)]);
        req.proximity = Some(ProximitySpec::Ordered { gaps: vec![0] });
        let err = flat.search(&req).err().expect("needs positions");
        assert!(err.to_string().contains("positions"), "{err}");
        // the same query in catalog order is fine
        req.order = ResultsOrder::Catalog;
        assert_eq!(flat.search(&req).unwrap().hits.len(), 1);
    }

    #[test]
    fn options_json() {
        let o: SearchOptions =
            serde_json::from_str(r#"{"query":"x","order":"relevance","book_generations":[[5,1],[6,0]]}"#).unwrap();
        assert_eq!(o.order, ResultsOrder::Relevance);
        assert_eq!(o.book_generations, vec![(5, 1), (6, 0)]);
        let r = o.to_request(false).unwrap();
        assert_eq!((r.order, r.book_generations.len()), (ResultsOrder::Relevance, 2));
        let o: SearchOptions = serde_json::from_str(r#"{"query":"x","order":"catalogue"}"#).unwrap();
        assert_eq!(o.order, ResultsOrder::Catalog);
        let o: SearchOptions = serde_json::from_str(r#"{"query":"x"}"#).unwrap();
        assert_eq!(o.order, ResultsOrder::Catalog);
    }

    #[test]
    fn tightest_span_takes_distinct_positions() {
        assert_eq!(tightest_span(&[vec![1, 9], vec![4, 10]]), Some(1));
        assert_eq!(tightest_span(&[vec![5], vec![5]]), None);
        // a repeated word needs its own occurrence: 2 and 3 for the pair, 7 for c
        assert_eq!(tightest_span(&[vec![2, 3], vec![2, 3], vec![7]]), Some(5));
        assert_eq!(tightest_span(&[vec![0, 20], vec![21], vec![3, 19]]), Some(2));
    }
}
