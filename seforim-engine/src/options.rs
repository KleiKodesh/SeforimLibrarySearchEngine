//! The public search API: one [`SearchOptions`] object.
//!
//! The query is a string in FtsLib's syntax (`*` `?` `%` `~N` `|` stay in
//! it); every word option is a plain field that applies to every word, and
//! [`SearchOptions::words`] overrides any of them for one word (by its place
//! in the query as typed, from 0) and adds alternative words for its slot: Otzaria's
//! `wordOptions` / `alternativeWords`. The engine builds the walks itself, one
//! per word with all its options; callers never pick walks or slot kinds.
//!
//! [`SearchOptions::to_request`] lowers it to the low-level
//! [`SearchRequest`] (kept for hosts that build slots themselves).

use std::collections::BTreeMap;

use hebrew_fts::{tokenize_query, QueryTerm, QueryToken, DEFAULT_CONTEXT_WORDS};
use serde::{Deserialize, Serialize};

use crate::relevance::ResultsOrder;
use crate::search::{ExcludeSpec, GroupSpec, ProximitySpec, SearchRequest, TypoModeSpec, WalkOptionsSpec};
use crate::{Error, Result};

/// A search: the query string and the options of every word.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchOptions {
    /// FtsLib syntax: words AND-ed; `a | b` one slot; `*` `?` wildcard;
    /// `word~N` fuzzy (N ≤ 2); `%word` / `word%` FtsLib's grammar tables.
    pub query: String,

    // ── Word options: every word, unless `words` overrides ──
    /// קידומות דקדוקיות.
    pub grammar_prefixes: bool,
    /// סיומות דקדוקיות.
    pub grammar_suffixes: bool,
    /// קידומות ארמיות.
    pub aramaic_prefixes: bool,
    /// סיומות ארמיות.
    pub aramaic_suffixes: bool,
    /// כתיב מלא/חסר.
    pub ktiv_male_haser: bool,
    /// חלק ממילה.
    pub part_of_word: bool,
    /// קידומות (any letters before).
    pub advanced_prefixes: bool,
    /// סיומות (any letters after).
    pub advanced_suffixes: bool,
    /// התעלם מגרשיים (also applies to wildcard / fuzzy / `%` words).
    pub ignore_quotes: bool,
    /// Index terms must match as written (off: also by their quote-free form).
    pub exact_quotes: bool,
    /// שגיאות כתיב, expanded per `typo_mode`.
    pub spelling_errors: bool,
    /// Otzaria's `match_nikud`: the nikud typed in a word is required on its
    /// letter, marks not typed are free; the search then sees only lines
    /// that carry a mark (Otzaria's vocalized field). Off: every mark is
    /// ignored. Needs an index built with `keep_marks`.
    pub match_nikud: bool,
    /// Otzaria's `match_taamim`: the same for te'amim.
    pub match_teamim: bool,
    pub typo_mode: TypoModeSpec,
    /// Limits (Otzaria). None = the walk's defaults.
    pub prefix_max_letters: Option<u32>,
    pub suffix_max_letters: Option<u32>,
    pub part_of_word_max_letters: Option<u32>,
    pub grammar_prefix_max_letters: Option<u32>,

    /// Per-word overrides by word index: the place, from 0, of the word as
    /// the user typed it (`query` cut on space / tab / CR / LF and `|`; `|`
    /// is not a word; a word the engine drops keeps its place). A typed word
    /// the separators split (maqaf, punctuation, Hebrew/Latin) gives its
    /// override and alternatives to every part. JSON keys are strings:
    /// `{"1": {...}}`.
    pub words: BTreeMap<usize, WordOptions>,

    // ── Query-level ──
    /// FtsLib's word distance: max words between the matched words. None = no limit.
    pub max_word_distance: Option<usize>,
    /// FtsLib: the words in query order.
    pub require_ordered: bool,
    /// Partial matching as a slot count: a line needs this many of the
    /// slots. None = all. `word_match` wins when set.
    pub min_groups: Option<usize>,
    /// Partial matching (Otzaria's wordMatchMode), counted by the engine
    /// over the slots it searches (see [`WordMatchSpec`]). None = `min_groups`.
    pub word_match: Option<WordMatchSpec>,
    /// Words of context per side of the snippet.
    pub context_words: usize,
    /// Otzaria's proximity (in order with per-pair gaps, or any order),
    /// given per TYPED word: `gaps[i]` is between typed words i and i+1,
    /// dropped words included; the engine cuts the dropped words out and
    /// merges the gaps around them (see `Slots::lower`). When set,
    /// `max_word_distance` / `require_ordered` are not used.
    pub proximity: Option<ProximitySpec>,
    /// Otzaria's negative query: lines with these words are dropped. Its
    /// words take the word options above as their default and have their
    /// own per-word overrides (`ExcludeOptions::words`).
    pub exclude: Option<ExcludeOptions>,
    /// Filtered search: only these book ids. None = every book.
    pub books: Option<Vec<u64>>,
    /// Whole phrases ORed with the query (Otzaria's acronym expansion).
    pub phrase_alternatives: Vec<PhraseOptions>,
    /// Otzaria's user regex: a query wrapped in `/.../` is a regex (see
    /// `hebrew_fts::user_regex`). Off (the default, FtsLib's behaviour): the
    /// query is plain text whatever its slashes.
    pub user_regex: bool,
    /// The order of the hits (Otzaria's `ResultsOrder`): `"catalog"`
    /// (default, costs nothing), `"relevance"` or `"generation"`.
    pub order: ResultsOrder,
    /// For `"generation"`: `[[book_id, generation_order], ...]` from the
    /// host (Otzaria's per-book `generationOrder`); books absent come last.
    pub book_generations: Vec<(u64, u32)>,
    /// Cross-line matching: a match may continue from the end of a line onto
    /// the start of the next line of the same book (see
    /// `SearchRequest::cross_line`). Off by default (FtsLib).
    pub cross_line: bool,
    /// Otzaria's SameSection scope for the query (see
    /// `SearchRequest::same_section`). Off by default.
    pub same_section: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            query: String::new(),
            grammar_prefixes: false,
            grammar_suffixes: false,
            aramaic_prefixes: false,
            aramaic_suffixes: false,
            ktiv_male_haser: false,
            part_of_word: false,
            advanced_prefixes: false,
            advanced_suffixes: false,
            ignore_quotes: false,
            exact_quotes: false,
            spelling_errors: false,
            match_nikud: false,
            match_teamim: false,
            typo_mode: TypoModeSpec::default(),
            prefix_max_letters: None,
            suffix_max_letters: None,
            part_of_word_max_letters: None,
            grammar_prefix_max_letters: None,
            words: BTreeMap::new(),
            max_word_distance: None,
            require_ordered: false,
            min_groups: None,
            word_match: None,
            context_words: DEFAULT_CONTEXT_WORDS,
            proximity: None,
            exclude: None,
            books: None,
            phrase_alternatives: Vec::new(),
            user_regex: false,
            order: ResultsOrder::Catalog,
            book_generations: Vec::new(),
            cross_line: false,
            same_section: false,
        }
    }
}

/// One word's overrides: the same fields as [`SearchOptions`]' word options,
/// each None = inherit the global value, plus alternative words for the slot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WordOptions {
    pub grammar_prefixes: Option<bool>,
    pub grammar_suffixes: Option<bool>,
    pub aramaic_prefixes: Option<bool>,
    pub aramaic_suffixes: Option<bool>,
    pub ktiv_male_haser: Option<bool>,
    pub part_of_word: Option<bool>,
    pub advanced_prefixes: Option<bool>,
    pub advanced_suffixes: Option<bool>,
    pub ignore_quotes: Option<bool>,
    pub exact_quotes: Option<bool>,
    pub spelling_errors: Option<bool>,
    pub match_nikud: Option<bool>,
    pub match_teamim: Option<bool>,
    pub typo_mode: Option<TypoModeSpec>,
    pub prefix_max_letters: Option<u32>,
    pub suffix_max_letters: Option<u32>,
    pub part_of_word_max_letters: Option<u32>,
    pub grammar_prefix_max_letters: Option<u32>,
    /// Extra words for this word's slot (Otzaria's alternativeWords), each
    /// in the query syntax, searched with this word's options.
    pub alternatives: Vec<String>,
}

/// The negative query (Otzaria's negativeQuery). Options are per word: a
/// negative word is a word like any other, with the global word options of
/// [`SearchOptions`] as its default and its own overrides and alternatives
/// in `words` (Otzaria's negativeSearchOptions / negativeAlternativeWords).
/// There is no separate global option set for the negative query: a host
/// with one (Otzaria's negativeGlobalSearchOptions) writes it onto every
/// negative word.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExcludeOptions {
    /// Words (query syntax) that must not appear; all of them, together.
    pub query: String,
    /// Per-word overrides of `query`'s words, by typed-word index, with the
    /// same rule and fields as [`SearchOptions::words`] (alternatives join
    /// the word's slot).
    pub words: BTreeMap<usize, WordOptions>,
    /// Whole phrases ORed with the negative query (Otzaria's acronym
    /// expansion of the negative words): a line carrying any of them, as
    /// a phrase, is dropped too.
    pub phrase_alternatives: Vec<PhraseOptions>,
    pub max_word_distance: Option<usize>,
    pub require_ordered: bool,
    /// Per typed word of `query`, as [`SearchOptions::proximity`].
    pub proximity: Option<ProximitySpec>,
    /// Otzaria's SameSection negative scope: a section holding every
    /// negative word is dropped whole (see `ExcludeSpec::same_section`).
    pub same_section: bool,
}

/// A whole-phrase alternative: its words adjacent, in order, all required,
/// matched as written (plus the quote option).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PhraseOptions {
    pub query: String,
    pub ignore_quotes: bool,
}

impl SearchOptions {
    /// The global word options as walk options.
    fn global(&self) -> WalkOptionsSpec {
        WalkOptionsSpec {
            grammar_prefixes: self.grammar_prefixes,
            grammar_suffixes: self.grammar_suffixes,
            aramaic_prefixes: self.aramaic_prefixes,
            aramaic_suffixes: self.aramaic_suffixes,
            ktiv_male_haser: self.ktiv_male_haser,
            part_of_word: self.part_of_word,
            advanced_prefixes: self.advanced_prefixes,
            advanced_suffixes: self.advanced_suffixes,
            ignore_quotes: self.ignore_quotes,
            exact_quotes: self.exact_quotes,
            spelling_errors: self.spelling_errors,
            typo_mode: self.typo_mode,
            phrase_word: false,
            match_nikud: self.match_nikud,
            match_teamim: self.match_teamim,
            prefix_max_letters: self.prefix_max_letters,
            suffix_max_letters: self.suffix_max_letters,
            part_of_word_max_letters: self.part_of_word_max_letters,
            grammar_prefix_max_letters: self.grammar_prefix_max_letters,
        }
    }

    /// Lowers to the low-level request. `keep_marks`: the index keeps
    /// nikud / te'amim, so plain words keep the marks typed (the walk drops
    /// those of a class not matched). Errors when a proximity's `gaps` does
    /// not have one entry per consecutive pair of typed words.
    ///
    /// Vocalized mode (Otzaria's `voc_mode`): `match_nikud` / `match_teamim`
    /// on globally or for any word makes the whole search see only lines
    /// that carry a mark (`SearchRequest::vocalized`); the negative query is
    /// vocalized when the global flags are on (Otzaria passes the global
    /// flags to both builds).
    pub fn to_request(&self, keep_marks: bool) -> Result<SearchRequest> {
        // A user regex `/.../` (Otzaria): the whole query is the regex, on
        // its own: no word options, negative query, distance or grouping.
        // One word = a term set; several = an adjacent ordered phrase. An
        // invalid pattern is searched as plain text, as Otzaria's app does.
        if self.user_regex && hebrew_fts::user_regex::is_wrapped(&self.query) {
            match hebrew_fts::user_regex::parse(&self.query) {
                Ok(re) => {
                    let n = re.words.len();
                    return Ok(SearchRequest {
                        groups: re.words.into_iter().map(|pattern| GroupSpec::Regex { pattern }).collect(),
                        context_words: self.context_words,
                        proximity: (n > 1).then(|| ProximitySpec::Ordered { gaps: vec![0; n - 1] }),
                        books: self.books.clone(),
                        order: self.order,
                        book_generations: self.book_generations.clone(),
                        cross_line: self.cross_line,
                        ..Default::default()
                    });
                }
                Err(e @ hebrew_fts::UserRegexError::LineKind) => return Err(Error::Unsupported(e.to_string())),
                Err(hebrew_fts::UserRegexError::Invalid(_)) => {}
            }
        }
        let global = self.global();
        let global_vocalized = global.match_nikud || global.match_teamim;
        let vocalized = global_vocalized
            || self
                .words
                .values()
                .any(|w| w.match_nikud == Some(true) || w.match_teamim == Some(true));
        let main = slots(&self.query, keep_marks, |i| match self.words.get(&i) {
            Some(w) => (w.apply(global), w.alternatives.as_slice()),
            None => (global, &[]),
        });
        // With a proximity rule the too long words leave the query and keep
        // their positions in the rule; without, they stay as skipped slots
        // that count toward word distance (FtsLib's originalGroupCount).
        let (groups, proximity) = match &self.proximity {
            Some(p) => (main.searched_groups(), Some(main.lower(p)?)),
            None => (main.groups.clone(), None),
        };
        let min_groups = match self.word_match {
            Some(m) => m.min_groups(main.searched_count()),
            None => self.min_groups,
        };
        let exclude = match &self.exclude {
            Some(e) => {
                // per word, exactly as the positive words: the global
                // options are the default, `e.words` the word's own
                let ex = slots(&e.query, keep_marks, |i| match e.words.get(&i) {
                    Some(w) => (w.apply(global), w.alternatives.as_slice()),
                    None => (global, &[]),
                });
                let (groups, proximity) = match &e.proximity {
                    Some(p) => (ex.searched_groups(), Some(ex.lower(p)?)),
                    None => (ex.groups.clone(), None),
                };
                Some(ExcludeSpec {
                    proximity,
                    groups,
                    max_word_distance: e.max_word_distance,
                    require_ordered: e.require_ordered,
                    gaps: None,
                    phrase_alternatives: phrases(&e.phrase_alternatives, keep_marks),
                    // Otzaria: `voc.or(options_vocalized_flags(negative_search_options))`
                    vocalized: global_vocalized
                        || e.words.values().any(|w| w.match_nikud == Some(true) || w.match_teamim == Some(true)),
                    same_section: e.same_section,
                })
            }
            None => None,
        };
        let phrase_alternatives = phrases(&self.phrase_alternatives, keep_marks);
        Ok(SearchRequest {
            groups,
            max_word_distance: self.max_word_distance,
            require_ordered: self.require_ordered,
            context_words: self.context_words,
            gaps: None,
            min_groups,
            exclude,
            phrase_alternatives,
            proximity,
            books: self.books.clone(),
            order: self.order,
            book_generations: self.book_generations.clone(),
            vocalized,
            cross_line: self.cross_line,
            same_section: self.same_section,
        })
    }
}

/// Otzaria's `WordMatchMode`: how many of the query's slots a line needs.
/// The engine counts the slots it searches: a word it drops is not
/// counted, and a typed word the separators split counts each part (as
/// Otzaria counts its own split words).
///
/// JSON: `{"mode":"all"}` / `{"mode":"any_word"}` / `{"mode":"most_words"}` /
/// `{"mode":"at_least","count":2}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum WordMatchSpec {
    All,
    AnyWord,
    MostWords,
    AtLeast { count: usize },
}

impl WordMatchSpec {
    /// Otzaria `WordMatch::min_words` for `n` slots (all = n, any = 1, most
    /// = n/2 + 1, at least = count but at least 1; never more than n), as
    /// `min_groups`: None when it is all of them.
    pub fn min_groups(self, n: usize) -> Option<usize> {
        let min = match self {
            WordMatchSpec::All => n,
            WordMatchSpec::AnyWord => 1,
            WordMatchSpec::MostWords => n / 2 + 1,
            WordMatchSpec::AtLeast { count } => count.max(1),
        };
        (min < n).then_some(min)
    }
}

impl WordOptions {
    /// `base` with this word's overrides.
    fn apply(&self, base: WalkOptionsSpec) -> WalkOptionsSpec {
        WalkOptionsSpec {
            grammar_prefixes: self.grammar_prefixes.unwrap_or(base.grammar_prefixes),
            grammar_suffixes: self.grammar_suffixes.unwrap_or(base.grammar_suffixes),
            aramaic_prefixes: self.aramaic_prefixes.unwrap_or(base.aramaic_prefixes),
            aramaic_suffixes: self.aramaic_suffixes.unwrap_or(base.aramaic_suffixes),
            ktiv_male_haser: self.ktiv_male_haser.unwrap_or(base.ktiv_male_haser),
            part_of_word: self.part_of_word.unwrap_or(base.part_of_word),
            advanced_prefixes: self.advanced_prefixes.unwrap_or(base.advanced_prefixes),
            advanced_suffixes: self.advanced_suffixes.unwrap_or(base.advanced_suffixes),
            ignore_quotes: self.ignore_quotes.unwrap_or(base.ignore_quotes),
            exact_quotes: self.exact_quotes.unwrap_or(base.exact_quotes),
            spelling_errors: self.spelling_errors.unwrap_or(base.spelling_errors),
            typo_mode: self.typo_mode.unwrap_or(base.typo_mode),
            phrase_word: base.phrase_word,
            match_nikud: self.match_nikud.unwrap_or(base.match_nikud),
            match_teamim: self.match_teamim.unwrap_or(base.match_teamim),
            prefix_max_letters: self.prefix_max_letters.or(base.prefix_max_letters),
            suffix_max_letters: self.suffix_max_letters.or(base.suffix_max_letters),
            part_of_word_max_letters: self.part_of_word_max_letters.or(base.part_of_word_max_letters),
            grammar_prefix_max_letters: self.grammar_prefix_max_letters.or(base.grammar_prefix_max_letters),
        }
    }
}

/// Phrase alternatives as slot lists: each phrase's words matched as
/// written (plus its quote option).
fn phrases(list: &[PhraseOptions], keep_marks: bool) -> Vec<Vec<GroupSpec>> {
    list.iter()
        .map(|p| {
            let o = WalkOptionsSpec { ignore_quotes: p.ignore_quotes, ..Default::default() };
            slots(&p.query, keep_marks, |_| (o, &[])).groups
        })
        // A phrase with a word too long to be indexed can never match
        // (Otzaria: its literal matches no term).
        .filter(|p: &Vec<GroupSpec>| !p.is_empty() && !p.iter().any(is_long))
        .collect()
}

/// One slot for a parsed term with its word's options. Wildcard, fuzzy and
/// `%` words take only the quote option (as FtsLib's parser).
fn group(term: QueryTerm, o: WalkOptionsSpec) -> GroupSpec {
    let ignore_quotes = o.ignore_quotes;
    match term {
        QueryTerm::Word(word) => GroupSpec::Word { word, options: o },
        QueryTerm::Unindexed(word) => GroupSpec::Unindexed { word },
        QueryTerm::Wildcard(pattern) => GroupSpec::Wildcard { pattern, ignore_quotes },
        QueryTerm::Fuzzy { word, distance } => GroupSpec::Fuzzy { word, distance, ignore_quotes },
        QueryTerm::Grammar { word, prefixes, suffixes } => GroupSpec::Grammar { word, prefixes, suffixes, ignore_quotes },
    }
}

/// True for a slot of words too long to be indexed only
/// ([`GroupSpec::Unindexed`], or an OR slot of nothing else).
fn is_long(g: &GroupSpec) -> bool {
    match g {
        GroupSpec::Unindexed { .. } => true,
        GroupSpec::Any { alternatives } => !alternatives.is_empty() && alternatives.iter().all(is_long),
        _ => false,
    }
}

/// The slots of a query and where each typed word went.
struct Slots {
    groups: Vec<GroupSpec>,
    /// Per typed word (see [`tokenize_query`]): the first and last slot its
    /// sub-words joined, or None when the engine dropped it.
    typed: Vec<Option<(usize, usize)>>,
}

impl Slots {
    /// The slots a proximity rule searches: a slot of words too long to be
    /// indexed has no term, so it leaves the query and its position goes
    /// into the rule ([`Self::lower`]).
    fn searched_groups(&self) -> Vec<GroupSpec> {
        self.groups.iter().filter(|g| !is_long(g)).cloned().collect()
    }

    /// Slots that search (words too long to be indexed not counted).
    fn searched_count(&self) -> usize {
        self.groups.iter().filter(|g| !is_long(g)).count()
    }

    /// A proximity given per typed word, as the proximity of
    /// [`Self::searched_groups`].
    ///
    /// Every word is a slot of its own, matched at its position (one-letter
    /// words and digit runs included). A dropped word is a typed word with
    /// no letters or digits (`,`), which the text tokenizer gives no
    /// position either: it is cut out of the rule and the gaps around it
    /// merge, `a x b` with gaps `[g0, g1]` -> `a b` with `g0 + g1`. A word too
    /// long to be indexed keeps its one position (the text keeps the
    /// position of such a word): `a x b` -> `a b` with `g0 + 1 + g1`. Sub-words
    /// of one typed word (two words joined by a maqaf, typed as one word) are
    /// adjacent: gap 0. The gap before an OR slot (`a | b`) is the one
    /// before its first word, the gap after it the one after its last word.
    /// A leading or trailing removed word takes its gap with it. Any order:
    /// Otzaria's window for its `n` words is `(n - 1) * (distance + 1)`
    /// positions; each dropped or too long word took one of them, so the
    /// window over the searched words is that minus their count.
    fn lower(&self, p: &ProximitySpec) -> Result<ProximitySpec> {
        let n = self.typed.len();
        let k = self.groups.len();
        Ok(match p {
            ProximitySpec::Ordered { gaps } => {
                if gaps.len() + 1 != n.max(1) {
                    return Err(Error::Tantivy(tantivy::TantivyError::InvalidArgument(format!(
                        "proximity gaps must have one entry per consecutive pair of typed words: {} words, {} gaps",
                        n,
                        gaps.len()
                    ))));
                }
                // first / last typed word of each slot
                let mut span = vec![(usize::MAX, 0usize); k];
                for (t, r) in self.typed.iter().enumerate() {
                    if let Some((first, last)) = *r {
                        for s in &mut span[first..=last] {
                            s.0 = s.0.min(t);
                            s.1 = s.1.max(t);
                        }
                    }
                }
                let slot_gap = |g: usize| -> u32 {
                    let (a, b) = (span[g - 1].1, span[g].0);
                    if a >= b {
                        0 // sub-words of one typed word
                    } else {
                        gaps[a..b].iter().fold(0u32, |acc, &x| acc.saturating_add(x))
                    }
                };
                // too long slots leave: their gaps and their one position merge
                let mut lowered = Vec::new();
                let mut carry = 0u32;
                let mut seen = false;
                for g in 0..k {
                    if g > 0 {
                        carry = carry.saturating_add(slot_gap(g));
                    }
                    if is_long(&self.groups[g]) {
                        if seen {
                            carry = carry.saturating_add(1);
                        }
                        continue;
                    }
                    if seen {
                        lowered.push(carry);
                    }
                    seen = true;
                    carry = 0;
                }
                ProximitySpec::Ordered { gaps: lowered }
            }
            ProximitySpec::AnyOrder { distance } => {
                let dropped = self.typed.iter().filter(|t| t.is_none()).count() + (k - self.searched_count());
                if dropped == 0 {
                    p.clone()
                } else {
                    let words = u32::try_from(self.searched_count() + dropped).unwrap_or(u32::MAX);
                    let window = words.saturating_sub(1).saturating_mul(distance.saturating_add(1));
                    ProximitySpec::Window {
                        max_span: window.saturating_sub(u32::try_from(dropped).unwrap_or(u32::MAX)),
                    }
                }
            }
            ProximitySpec::Window { .. } => p.clone(),
        })
    }
}

/// FtsLib's grouping of the tokens (`QueryParser.Parse`): a `|` joins its
/// neighbours into one OR slot; a word that cannot search is skipped
/// without breaking a pending `|`. `word(i)` gives typed word i's options
/// and alternatives, which go to every sub-word cut from it; the
/// alternatives join the sub-word's slot (and stand for the word when it
/// cannot search itself).
fn slots<'a>(query: &str, keep_marks: bool, word: impl Fn(usize) -> (WalkOptionsSpec, &'a [String])) -> Slots {
    let mut groups = Vec::new();
    let mut typed: Vec<Option<(usize, usize)>> = Vec::new();
    let mut pending: Vec<GroupSpec> = Vec::new();
    let mut last_was_pipe = false;
    let flush = |pending: &mut Vec<GroupSpec>, groups: &mut Vec<GroupSpec>| match pending.len() {
        0 => {}
        1 => groups.push(pending.pop().unwrap()),
        _ => groups.push(GroupSpec::Any { alternatives: std::mem::take(pending) }),
    };
    for token in tokenize_query(query, keep_marks) {
        let (index, term) = match token {
            QueryToken::Pipe => {
                last_was_pipe = true;
                continue;
            }
            QueryToken::Word { index, term } => (index, term),
        };
        if typed.len() <= index {
            typed.resize(index + 1, None);
        }
        let (o, alternatives) = word(index);
        let mut members: Vec<GroupSpec> = term.into_iter().map(|t| group(t, o)).collect();
        for alt in alternatives {
            for t in tokenize_query(alt, keep_marks) {
                if let QueryToken::Word { term: Some(t), .. } = t {
                    members.push(group(t, o));
                }
            }
        }
        if members.is_empty() {
            continue;
        }
        if !last_was_pipe {
            flush(&mut pending, &mut groups);
        }
        // the slot `pending` becomes
        let slot = groups.len();
        typed[index] = Some(match typed[index] {
            Some((first, _)) => (first, slot),
            None => (slot, slot),
        });
        pending.extend(members);
        last_was_pipe = false;
    }
    flush(&mut pending, &mut groups);
    Slots { groups, typed }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word_of(g: &GroupSpec) -> (&str, WalkOptionsSpec) {
        match g {
            GroupSpec::Word { word, options } => (word, *options),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn globals_overrides_and_alternatives() {
        let mut words = BTreeMap::new();
        words.insert(
            1,
            WordOptions {
                grammar_prefixes: Some(false),
                part_of_word: Some(true),
                alternatives: vec!["ספר".into()],
                ..Default::default()
            },
        );
        let o = SearchOptions {
            query: "משה תורה שלו* | אבג~".into(),
            grammar_prefixes: true,
            ignore_quotes: true,
            words,
            ..Default::default()
        };
        let r = o.to_request(false).unwrap();
        assert_eq!(r.groups.len(), 3);
        let (w, opts) = word_of(&r.groups[0]);
        assert_eq!(w, "משה");
        assert!(opts.grammar_prefixes && opts.ignore_quotes && !opts.part_of_word);
        let GroupSpec::Any { alternatives } = &r.groups[1] else { panic!() };
        let (w, opts) = word_of(&alternatives[0]);
        assert_eq!(w, "תורה");
        assert!(!opts.grammar_prefixes && opts.part_of_word && opts.ignore_quotes);
        let (w, alt_opts) = word_of(&alternatives[1]);
        assert_eq!(w, "ספר");
        assert!(alt_opts.part_of_word);
        let GroupSpec::Any { alternatives } = &r.groups[2] else { panic!() };
        assert!(matches!(&alternatives[0], GroupSpec::Wildcard { pattern, ignore_quotes: true } if pattern == "שלו*"));
        assert!(matches!(&alternatives[1], GroupSpec::Fuzzy { distance: 1, ignore_quotes: true, .. }));
    }

    #[test]
    fn json_shape() {
        let o: SearchOptions = serde_json::from_str(
            r#"{"query":"משה תורה","ktiv_male_haser":true,"words":{"0":{"ktiv_male_haser":false,"alternatives":["משֶה"]}},
               "proximity":{"mode":"any_order","distance":2},"exclude":{"query":"סיני"},
               "phrase_alternatives":[{"query":"רבי משה"}]}"#,
        )
        .unwrap();
        let r = o.to_request(false).unwrap();
        let GroupSpec::Any { alternatives } = &r.groups[0] else { panic!() };
        assert!(!word_of(&alternatives[0]).1.ktiv_male_haser);
        assert!(word_of(&r.groups[1]).1.ktiv_male_haser);
        assert_eq!(r.exclude.unwrap().groups.len(), 1);
        assert_eq!(r.phrase_alternatives[0].len(), 2);
        assert_eq!(r.context_words, DEFAULT_CONTEXT_WORDS);
    }

    #[test]
    fn typed_word_index_reaches_every_sub_word() {
        // word 1 is typed as one word and split in two (maqaf); word 2 is
        // split at a comma; the override of word 3 must
        // not shift onto a sub-word
        let mut words = BTreeMap::new();
        words.insert(1, WordOptions { part_of_word: Some(true), ..Default::default() });
        words.insert(3, WordOptions { ktiv_male_haser: Some(true), ..Default::default() });
        let o = SearchOptions { query: "משה אשר־שמע abc,שלום תורה".into(), words, ..Default::default() };
        let r = o.to_request(false).unwrap();
        let got: Vec<_> = r.groups.iter().map(|g| { let (w, o) = word_of(g); (w.to_string(), o.part_of_word, o.ktiv_male_haser) }).collect();
        assert_eq!(
            got,
            [
                ("משה".into(), false, false),
                ("אשר".into(), true, false),
                ("שמע".into(), true, false),
                ("abc".into(), false, false),
                ("שלום".into(), false, false),
                ("תורה".into(), false, true),
            ]
        );
    }

    #[test]
    fn dropped_words_merge_their_gaps() {
        let prox = |q: &str, p: ProximitySpec| {
            SearchOptions { query: q.into(), proximity: Some(p), ..Default::default() }.to_request(false).map(|r| r.proximity.unwrap())
        };
        // a no-letter word in the middle: g0 + g1
        assert_eq!(
            prox("משה , תורה", ProximitySpec::Ordered { gaps: vec![2, 3] }).unwrap(),
            ProximitySpec::Ordered { gaps: vec![5] }
        );
        // one-letter words and digit runs are slots of their own
        assert_eq!(
            prox("משה ו 12 תורה", ProximitySpec::Ordered { gaps: vec![2, 3, 4] }).unwrap(),
            ProximitySpec::Ordered { gaps: vec![2, 3, 4] }
        );
        // leading / trailing dropped words take their gap with them; a split
        // typed word is adjacent; an OR slot uses the gap after its last word
        assert_eq!(
            prox(". משה|תורה אשר־שמע , ספר ;", ProximitySpec::Ordered { gaps: vec![9, 9, 1, 4, 2, 9] }).unwrap(),
            ProximitySpec::Ordered { gaps: vec![1, 0, 6] }
        );
        // wrong gap count (per typed word) is an error
        assert!(prox("משה , תורה", ProximitySpec::Ordered { gaps: vec![2] }).is_err());
        // any order: Otzaria's window for 3 words minus the dropped word
        assert_eq!(
            prox("משה , תורה", ProximitySpec::AnyOrder { distance: 2 }).unwrap(),
            ProximitySpec::Window { max_span: 2 * 3 - 1 }
        );
        assert_eq!(
            prox("משה תורה", ProximitySpec::AnyOrder { distance: 2 }).unwrap(),
            ProximitySpec::AnyOrder { distance: 2 }
        );
        // a word too long to be indexed keeps its one position: g0 + 1 + g1
        let long = "\u{05D0}".repeat(64);
        assert_eq!(
            prox(&format!("\u{05DE}\u{05E9}\u{05D4} {long} \u{05EA}\u{05D5}\u{05E8}\u{05D4}"), ProximitySpec::Ordered { gaps: vec![2, 3] }).unwrap(),
            ProximitySpec::Ordered { gaps: vec![6] }
        );
        // leading / trailing: its gap leaves with it
        assert_eq!(
            prox(&format!("{long} \u{05DE}\u{05E9}\u{05D4} \u{05EA}\u{05D5}\u{05E8}\u{05D4} {long}"), ProximitySpec::Ordered { gaps: vec![5, 2, 7] }).unwrap(),
            ProximitySpec::Ordered { gaps: vec![2] }
        );
        assert_eq!(
            prox(&format!("\u{05DE}\u{05E9}\u{05D4} {long} \u{05EA}\u{05D5}\u{05E8}\u{05D4}"), ProximitySpec::AnyOrder { distance: 2 }).unwrap(),
            ProximitySpec::Window { max_span: 2 * 3 - 1 }
        );
        // without a proximity it stays a skipped slot (counted in word distance)
        let r = SearchOptions { query: format!("\u{05DE}\u{05E9}\u{05D4} {long} \u{05EA}\u{05D5}\u{05E8}\u{05D4}"), ..Default::default() }
            .to_request(false)
            .unwrap();
        assert_eq!(r.groups.len(), 3);
        assert!(matches!(r.groups[1], GroupSpec::Unindexed { .. }));
        let r = SearchOptions {
            query: format!("\u{05DE}\u{05E9}\u{05D4} {long} \u{05EA}\u{05D5}\u{05E8}\u{05D4}"),
            proximity: Some(ProximitySpec::Ordered { gaps: vec![0, 0] }),
            ..Default::default()
        }
        .to_request(false)
        .unwrap();
        assert_eq!(r.groups.len(), 2);
        // the negative query lowers its own proximity the same way
        let o = SearchOptions {
            query: "משה".into(),
            exclude: Some(ExcludeOptions {
                query: "סיני - הר".into(),
                proximity: Some(ProximitySpec::Ordered { gaps: vec![0, 1] }),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(o.to_request(false).unwrap().exclude.unwrap().proximity, Some(ProximitySpec::Ordered { gaps: vec![1] }));
    }

    #[test]
    fn exclude_words_are_words_with_their_own_options() {
        // typed word 1 of the negative query is split by a maqaf: its
        // override and alternative reach both parts; word 2 inherits
        let o: SearchOptions = serde_json::from_str(
            "{\"query\":\"\u{05D0}\u{05D1}\",\"grammar_prefixes\":true,\"exclude\":{\"query\":\"\u{05D2}\u{05D3} \u{05D4}\u{05D5}\u{05D6}\u{05BE}\u{05D7}\u{05D8} \u{05D9}\u{05DB}\",\
             \"words\":{\"1\":{\"grammar_prefixes\":false,\"part_of_word\":true,\"alternatives\":[\"\u{05DC}\u{05DE}\"]}},\
             \"phrase_alternatives\":[{\"query\":\"\u{05E0}\u{05E1} \u{05E2}\u{05E4}\",\"ignore_quotes\":true},{\"query\":\"\"}]}}",
        )
        .unwrap();
        let ex = o.to_request(false).unwrap().exclude.unwrap();
        assert_eq!(ex.groups.len(), 4);
        let (w, opts) = word_of(&ex.groups[0]);
        assert_eq!(w, "\u{05D2}\u{05D3}");
        assert!(opts.grammar_prefixes && !opts.part_of_word);
        let GroupSpec::Any { alternatives } = &ex.groups[1] else { panic!("{:?}", ex.groups[1]) };
        assert_eq!(word_of(&alternatives[0]).0, "\u{05D4}\u{05D5}\u{05D6}");
        assert_eq!(word_of(&alternatives[1]).0, "\u{05DC}\u{05DE}");
        let GroupSpec::Any { alternatives } = &ex.groups[2] else { panic!("{:?}", ex.groups[2]) };
        assert_eq!(word_of(&alternatives[0]).0, "\u{05D7}\u{05D8}");
        for a in alternatives {
            let o = word_of(a).1;
            assert!(!o.grammar_prefixes && o.part_of_word);
        }
        let (w, opts) = word_of(&ex.groups[3]);
        assert_eq!(w, "\u{05D9}\u{05DB}");
        assert!(opts.grammar_prefixes && !opts.part_of_word);
        // phrase alternatives: literal words with their quote option; an
        // empty phrase is dropped
        assert_eq!(ex.phrase_alternatives.len(), 1);
        let (w, opts) = word_of(&ex.phrase_alternatives[0][1]);
        assert_eq!(w, "\u{05E2}\u{05E4}");
        assert!(opts.ignore_quotes && !opts.grammar_prefixes);
    }

    #[test]
    fn word_match_counts_searched_slots() {
        let min = |q: &str, m: WordMatchSpec| {
            SearchOptions { query: q.into(), word_match: Some(m), ..Default::default() }.to_request(false).unwrap().min_groups
        };
        // a no-letter word is not a slot: most of 3 slots = 2
        assert_eq!(min("משה , תורה ספר", WordMatchSpec::MostWords), Some(2));
        assert_eq!(min("משה , תורה ספר", WordMatchSpec::AnyWord), Some(1));
        assert_eq!(min("משה , תורה ספר", WordMatchSpec::All), None);
        assert_eq!(min("משה , תורה ספר", WordMatchSpec::AtLeast { count: 3 }), None);
        assert_eq!(min("משה , תורה ספר", WordMatchSpec::AtLeast { count: 0 }), Some(1));
        // a one-letter word is a slot: most of 4 slots = 3
        assert_eq!(min("משה ו תורה ספר", WordMatchSpec::MostWords), Some(3));
        let j: SearchOptions = serde_json::from_str(r#"{"query":"א ב","word_match":{"mode":"at_least","count":2}}"#).unwrap();
        assert_eq!(j.word_match, Some(WordMatchSpec::AtLeast { count: 2 }));
    }
}
