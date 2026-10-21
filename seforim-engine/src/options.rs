//! The public search API: one [`SearchOptions`] object.
//!
//! The query is a string in FtsLib's syntax (`*` `?` `%` `~N` `|` stay in
//! it); every word option is a plain field that applies to every word, and
//! [`SearchOptions::words`] overrides any of them for one word (by its place
//! in the query, from 0) and adds alternative words for its slot: Otzaria's
//! `wordOptions` / `alternativeWords`. The engine builds the walks itself, one
//! per word with all its options; callers never pick walks or slot kinds.
//!
//! [`SearchOptions::to_request`] lowers it to the low-level
//! [`SearchRequest`] (kept for hosts that build slots themselves).

use std::collections::BTreeMap;

use hebrew_fts::{tokenize_query, QueryTerm, QueryToken, DEFAULT_CONTEXT_WORDS};
use serde::{Deserialize, Serialize};

use crate::search::{ExcludeSpec, GroupSpec, ProximitySpec, SearchRequest, TypoModeSpec, WalkOptionsSpec};

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
    /// Ignore nikud (on an index that keeps marks).
    pub ignore_nikud: bool,
    /// Ignore te'amim (on an index that keeps marks).
    pub ignore_teamim: bool,
    pub typo_mode: TypoModeSpec,
    /// Limits (Otzaria). None = the walk's defaults.
    pub prefix_max_letters: Option<u32>,
    pub suffix_max_letters: Option<u32>,
    pub part_of_word_max_letters: Option<u32>,
    pub grammar_prefix_max_letters: Option<u32>,

    /// Per-word overrides by word index (the n-th word token of `query`,
    /// from 0; `|` is not a word). JSON keys are strings: `{"1": {...}}`.
    pub words: BTreeMap<usize, WordOptions>,

    // ── Query-level ──
    /// FtsLib's word distance: max words between the matched words. None = no limit.
    pub max_word_distance: Option<usize>,
    /// FtsLib: the words in query order.
    pub require_ordered: bool,
    /// Partial matching (Otzaria's wordMatchMode): a line needs this many of
    /// the slots. None = all.
    pub min_groups: Option<usize>,
    /// Words of context per side of the snippet.
    pub context_words: usize,
    /// Otzaria's proximity (in order with per-pair gaps, or any order). When
    /// set, `max_word_distance` / `require_ordered` are not used.
    pub proximity: Option<ProximitySpec>,
    /// Otzaria's negative query: lines with these words are dropped.
    pub exclude: Option<ExcludeOptions>,
    /// Filtered search: only these book ids. None = every book.
    pub books: Option<Vec<u64>>,
    /// Whole phrases ORed with the query (Otzaria's acronym expansion).
    pub phrase_alternatives: Vec<PhraseOptions>,
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
            ignore_nikud: false,
            ignore_teamim: false,
            typo_mode: TypoModeSpec::default(),
            prefix_max_letters: None,
            suffix_max_letters: None,
            part_of_word_max_letters: None,
            grammar_prefix_max_letters: None,
            words: BTreeMap::new(),
            max_word_distance: None,
            require_ordered: false,
            min_groups: None,
            context_words: DEFAULT_CONTEXT_WORDS,
            proximity: None,
            exclude: None,
            books: None,
            phrase_alternatives: Vec::new(),
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
    pub ignore_nikud: Option<bool>,
    pub ignore_teamim: Option<bool>,
    pub typo_mode: Option<TypoModeSpec>,
    pub prefix_max_letters: Option<u32>,
    pub suffix_max_letters: Option<u32>,
    pub part_of_word_max_letters: Option<u32>,
    pub grammar_prefix_max_letters: Option<u32>,
    /// Extra words for this word's slot (Otzaria's alternativeWords), each
    /// in the query syntax, searched with this word's options.
    pub alternatives: Vec<String>,
}

/// The negative query: its words use the global word options.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExcludeOptions {
    /// Words (query syntax) that must not appear; all of them, together.
    pub query: String,
    pub max_word_distance: Option<usize>,
    pub require_ordered: bool,
    pub proximity: Option<ProximitySpec>,
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
            ignore_nikud: self.ignore_nikud,
            ignore_teamim: self.ignore_teamim,
            prefix_max_letters: self.prefix_max_letters,
            suffix_max_letters: self.suffix_max_letters,
            part_of_word_max_letters: self.part_of_word_max_letters,
            grammar_prefix_max_letters: self.grammar_prefix_max_letters,
        }
    }

    /// Lowers to the low-level request. `keep_marks`: the index keeps
    /// nikud / te'amim, so plain words keep the marks typed.
    pub fn to_request(&self, keep_marks: bool) -> SearchRequest {
        let global = self.global();
        let groups = slots(&self.query, keep_marks, |i| match self.words.get(&i) {
            Some(w) => (w.apply(global), w.alternatives.as_slice()),
            None => (global, &[]),
        });
        let exclude = self.exclude.as_ref().map(|e| ExcludeSpec {
            groups: slots(&e.query, keep_marks, |_| (global, &[])),
            max_word_distance: e.max_word_distance,
            require_ordered: e.require_ordered,
            gaps: None,
            proximity: e.proximity.clone(),
        });
        let phrase_alternatives = self
            .phrase_alternatives
            .iter()
            .map(|p| {
                let o = WalkOptionsSpec { ignore_quotes: p.ignore_quotes, ..Default::default() };
                slots(&p.query, keep_marks, |_| (o, &[]))
            })
            .filter(|p| !p.is_empty())
            .collect();
        SearchRequest {
            groups,
            max_word_distance: self.max_word_distance,
            require_ordered: self.require_ordered,
            context_words: self.context_words,
            gaps: None,
            min_groups: self.min_groups,
            exclude,
            phrase_alternatives,
            proximity: self.proximity.clone(),
            books: self.books.clone(),
        }
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
            ignore_nikud: self.ignore_nikud.unwrap_or(base.ignore_nikud),
            ignore_teamim: self.ignore_teamim.unwrap_or(base.ignore_teamim),
            prefix_max_letters: self.prefix_max_letters.or(base.prefix_max_letters),
            suffix_max_letters: self.suffix_max_letters.or(base.suffix_max_letters),
            part_of_word_max_letters: self.part_of_word_max_letters.or(base.part_of_word_max_letters),
            grammar_prefix_max_letters: self.grammar_prefix_max_letters.or(base.grammar_prefix_max_letters),
        }
    }
}

/// One slot for a parsed term with its word's options. Wildcard, fuzzy and
/// `%` words take only the quote option (as FtsLib's parser).
fn group(term: QueryTerm, o: WalkOptionsSpec) -> GroupSpec {
    let ignore_quotes = o.ignore_quotes;
    match term {
        QueryTerm::Word(word) => GroupSpec::Word { word, options: o },
        QueryTerm::Wildcard(pattern) => GroupSpec::Wildcard { pattern, ignore_quotes },
        QueryTerm::Fuzzy { word, distance } => GroupSpec::Fuzzy { word, distance, ignore_quotes },
        QueryTerm::Grammar { word, prefixes, suffixes } => GroupSpec::Grammar { word, prefixes, suffixes, ignore_quotes },
    }
}

/// FtsLib's grouping of the tokens (`QueryParser.Parse`): a `|` joins its
/// neighbours into one OR slot; a word that cannot search is skipped
/// without breaking a pending `|`. `word(i)` gives word i's options and
/// alternatives; the alternatives join the word's slot (and stand for it
/// when the word itself cannot search).
fn slots<'a>(
    query: &str,
    keep_marks: bool,
    word: impl Fn(usize) -> (WalkOptionsSpec, &'a [String]),
) -> Vec<GroupSpec> {
    let mut groups = Vec::new();
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
        pending.extend(members);
        last_was_pipe = false;
    }
    flush(&mut pending, &mut groups);
    groups
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
        let r = o.to_request(false);
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
        let r = o.to_request(false);
        let GroupSpec::Any { alternatives } = &r.groups[0] else { panic!() };
        assert!(!word_of(&alternatives[0]).1.ktiv_male_haser);
        assert!(word_of(&r.groups[1]).1.ktiv_male_haser);
        assert_eq!(r.exclude.unwrap().groups.len(), 1);
        assert_eq!(r.phrase_alternatives[0].len(), 2);
        assert_eq!(r.context_words, DEFAULT_CONTEXT_WORDS);
    }
}
