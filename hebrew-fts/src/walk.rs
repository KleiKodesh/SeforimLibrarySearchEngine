//! The Otzaria walk: one configurable automaton covering every search option,
//! driving a single focused pass over tantivy's term dictionary.
//!
//! Structure: [optional prefix phase] → core word variants → [optional suffix
//! phase], where each phase is either a closed trie (grammatical
//! prefixes/suffixes, incl. Aramaic) or a counted any-letter run (advanced /
//! part-of-word). Core variants come from the enabled generators (ktiv
//! מלא/חסר, Aramaic endings, spelling/OCR confusions). The walk visits only
//! branches a candidate shape allows; the index itself verifies existence.
//!
//! Options (per the Otzaria spec):
//! - חלק ממילה: word inside a larger word, ≤3 any-letters per side (≤2 when
//!   the word has ≥4 letters). ספר ↔ מספרים.
//! - כתיב מלא/חסר: insert/remove י/ו inside the word only (≤32 variants).
//! - קידומות דקדוקיות: combos of ו מ כש כ ב ש ל ה up to 3 letters; with
//!   Aramaic also ד דא א (ודמלכתא). תורה ↔ והתורה.
//! - סיומות דקדוקיות: plural/possessive/pronoun endings (incl. ותיהם); full
//!   morphology combo adds rare יא תא.
//! - סיומות ארמיות: ending equivalence ה↔א, ם↔ן, applied to the base word and
//!   to every suffix alternative.
//! - קידומות/סיומות מתקדם: ANY letters — ≤3 before (≤5 for a short word),
//!   ≤5 after (≤7 for a short word); both together ≡ חלק ממילה.
//! - התעלם מגרשיים: the canonical '"' is transparent anywhere.
//! - שגיאות כתיב/OCR: single confusion-pair substitutions.

use std::collections::BTreeSet;

use tantivy::query::{AutomatonWeight, EnableScoring, MatchedTermsSink, Query, Weight};
use tantivy::schema::Field;
use tantivy_fst::Automaton;

use crate::grammar;
use crate::set_automaton::SetAutomaton;
use crate::variants;

/// A word with ≤ this many letters counts as "short" (raises the advanced
/// prefix/suffix allowances).
pub const SHORT_WORD_MAX_LETTERS: usize = 3;

/// Search options, mirroring Otzaria's checkboxes. All off = exact word.
#[derive(Debug, Clone, Copy, Default)]
pub struct WalkOptions {
    /// קידומות דקדוקיות
    pub grammar_prefixes: bool,
    /// סיומות דקדוקיות
    pub grammar_suffixes: bool,
    /// קידומות ארמיות (implies grammar prefixes, with ד/דא/א forms)
    pub aramaic_prefixes: bool,
    /// סיומות ארמיות (ending equivalence ה↔א, ם↔ן; adds rare יא/תא suffixes)
    pub aramaic_suffixes: bool,
    /// כתיב מלא/חסר
    pub ktiv_male_haser: bool,
    /// חלק ממילה
    pub part_of_word: bool,
    /// קידומות (מתקדם) — any letters before the word
    pub advanced_prefixes: bool,
    /// סיומות (מתקדם) — any letters after the word
    pub advanced_suffixes: bool,
    /// התעלם מגרשיים
    pub ignore_quotes: bool,
    /// שגיאות כתיב/OCR מצויות
    pub spelling_errors: bool,
}

/// Prefix units for the closed grammatical prefix phase.
const PREFIX_UNITS: [&str; 8] = ["ו", "מ", "כש", "כ", "ב", "ש", "ל", "ה"];
const ARAMAIC_PREFIX_UNITS: [&str; 3] = ["דא", "ד", "א"];

/// Rare suffixes added in full-morphology / Aramaic modes.
const RARE_SUFFIXES: [&str; 2] = ["יא", "תא"];
/// Suffix from the Otzaria spec missing from the FtsLib table.
const EXTRA_SUFFIXES: [&str; 1] = ["ותיהם"];

/// All closed-list prefixes: sequences of distinct units, total ≤3 letters.
fn closed_prefixes(aramaic: bool) -> BTreeSet<String> {
    let mut units: Vec<&str> = PREFIX_UNITS.to_vec();
    if aramaic {
        units.extend(ARAMAIC_PREFIX_UNITS);
    }
    let mut out = BTreeSet::new();
    // depth ≤ 3 units is more than enough since total length is capped at 3.
    fn recurse(units: &[&str], current: String, used: u32, out: &mut BTreeSet<String>) {
        for (i, unit) in units.iter().enumerate() {
            if used & (1 << i) != 0 {
                continue;
            }
            let combined = format!("{current}{unit}");
            if combined.chars().count() > 3 {
                continue;
            }
            out.insert(combined.clone());
            recurse(units, combined, used | (1 << i), out);
        }
    }
    recurse(&units, String::new(), 0, &mut out);
    out
}

/// All closed-list suffixes for the given options, with Aramaic ending
/// equivalence applied to every alternative when requested.
fn closed_suffixes(options: &WalkOptions) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = grammar::SUFFIXES.iter().map(|s| s.to_string()).collect();
    out.extend(EXTRA_SUFFIXES.iter().map(|s| s.to_string()));
    let full_morphology = options.grammar_prefixes || options.aramaic_prefixes;
    if options.aramaic_suffixes {
        if full_morphology {
            out.extend(RARE_SUFFIXES.iter().map(|s| s.to_string()));
        }
        out = variants::expand_set(&out, variants::aramaic_ending_variants);
    }
    out
}

/// Builds the core word-variant set from the enabled generators.
fn core_variants(word: &str, options: &WalkOptions) -> BTreeSet<String> {
    let word: String = if options.ignore_quotes {
        word.chars().filter(|&c| c != '"').collect()
    } else {
        word.to_string()
    };
    let mut set = BTreeSet::new();
    set.insert(word);
    if options.ktiv_male_haser {
        set = variants::expand_set(&set, variants::ktiv_male_haser_variants);
    }
    if options.spelling_errors {
        set = variants::expand_set(&set, variants::spelling_variants);
    }
    if options.aramaic_suffixes {
        set = variants::expand_set(&set, variants::aramaic_ending_variants);
    }
    set
}

// ── Composite automaton ──────────────────────────────────────────────────

/// NFA configuration, packed: tag(3) | pending-lead-byte(1) | value(32).
/// Tags: 0 = closed-prefix trie node, 1 = any-prefix letter count,
/// 2 = core trie node, 3 = closed-suffix trie node, 4 = any-suffix count.
type Config = u64;

const TAG_PRE: u64 = 0;
const TAG_PRE_ANY: u64 = 1;
const TAG_CORE: u64 = 2;
const TAG_POST: u64 = 3;
const TAG_POST_ANY: u64 = 4;

#[inline]
fn pack(tag: u64, pending: bool, value: u32) -> Config {
    (tag << 33) | ((pending as u64) << 32) | value as u64
}
#[inline]
fn tag(c: Config) -> u64 {
    c >> 33
}
#[inline]
fn pending(c: Config) -> bool {
    (c >> 32) & 1 == 1
}
#[inline]
fn value(c: Config) -> u32 {
    c as u32
}

/// Is `b` a single-byte indexed letter (lowercased ASCII)?
#[inline]
fn is_ascii_letter(b: u8) -> bool {
    b.is_ascii_lowercase()
}
/// Lead byte of a UTF-8 Hebrew letter (א..ת = 0xD7 0x90..0xAA).
const HEBREW_LEAD: u8 = 0xD7;
#[inline]
fn is_hebrew_tail_letter(b: u8) -> bool {
    (0x90..=0xAA).contains(&b)
}

/// The composite walk automaton. State = sorted set of NFA configs; empty =
/// dead.
#[derive(Debug)]
pub struct HebrewWalkAutomaton {
    core: SetAutomaton,
    prefixes: Option<SetAutomaton>,
    any_prefix_max: u32,
    suffixes: Option<SetAutomaton>,
    any_suffix_max: u32,
    transparent_quotes: bool,
}

impl HebrewWalkAutomaton {
    /// Builds the automaton for `word` under `options`. `word` must already be
    /// normalized the way the index normalizes (lowercase ASCII, nikud
    /// stripped, canonical '"').
    pub fn new(word: &str, options: &WalkOptions) -> HebrewWalkAutomaton {
        let letters = word.chars().filter(|&c| c != '"').count();
        let short = letters <= SHORT_WORD_MAX_LETTERS;

        let grammar_prefixes = options.grammar_prefixes || options.aramaic_prefixes;

        // Advanced any-letter sides; both together ≡ part-of-word per spec.
        let part_of_word_max = if letters >= 4 { 2 } else { 3 };
        let (mut any_pre, mut any_post) = (0u32, 0u32);
        if options.part_of_word || (options.advanced_prefixes && options.advanced_suffixes) {
            any_pre = part_of_word_max;
            any_post = part_of_word_max;
        } else {
            if options.advanced_prefixes {
                any_pre = if short { 5 } else { 3 };
            }
            if options.advanced_suffixes {
                any_post = if short { 7 } else { 5 };
            }
        }

        let prefixes = if any_pre == 0 && grammar_prefixes {
            Some(SetAutomaton::from_strings(
                &closed_prefixes(options.aramaic_prefixes),
            ))
        } else {
            None
        };
        let suffixes = if any_post == 0 && options.grammar_suffixes {
            Some(SetAutomaton::from_strings(&closed_suffixes(options)))
        } else {
            None
        };

        HebrewWalkAutomaton {
            core: SetAutomaton::from_strings(&core_variants(word, options)),
            prefixes,
            any_prefix_max: any_pre,
            suffixes,
            any_suffix_max: any_post,
            transparent_quotes: options.ignore_quotes,
        }
    }

    /// ε-closure: phase-boundary spawns.
    fn close_into(&self, config: Config, set: &mut Vec<Config>) {
        if set.contains(&config) {
            return;
        }
        set.push(config);
        match tag(config) {
            TAG_PRE => {
                if !pending(config) {
                    if let Some(prefixes) = &self.prefixes {
                        if prefixes.node_is_match(value(config)) {
                            self.close_into(pack(TAG_CORE, false, SetAutomaton::ROOT), set);
                        }
                    }
                }
            }
            TAG_PRE_ANY => {
                if !pending(config) {
                    self.close_into(pack(TAG_CORE, false, SetAutomaton::ROOT), set);
                }
            }
            TAG_CORE => {
                if !pending(config) && self.core.node_is_match(value(config)) {
                    if self.suffixes.is_some() {
                        self.close_into(pack(TAG_POST, false, SetAutomaton::ROOT), set);
                    }
                    if self.any_suffix_max > 0 {
                        self.close_into(pack(TAG_POST_ANY, false, 0), set);
                    }
                }
            }
            _ => {}
        }
    }

    fn start_configs(&self) -> Vec<Config> {
        let mut set = Vec::new();
        if self.prefixes.is_some() {
            self.close_into(pack(TAG_PRE, false, SetAutomaton::ROOT), &mut set);
        }
        if self.any_prefix_max > 0 {
            self.close_into(pack(TAG_PRE_ANY, false, 0), &mut set);
        }
        self.close_into(pack(TAG_CORE, false, SetAutomaton::ROOT), &mut set);
        set.sort_unstable();
        set
    }

    /// Successors of one config on `byte`, with closure, into `out`.
    fn step_config(&self, config: Config, byte: u8, out: &mut Vec<Config>) {
        let t = tag(config);
        match t {
            TAG_PRE => {
                if let Some(prefixes) = &self.prefixes {
                    if let Some(next) = prefixes.step(value(config), byte) {
                        self.close_into(pack(TAG_PRE, false, next), out);
                    }
                }
            }
            TAG_PRE_ANY | TAG_POST_ANY => {
                let (count, max) = (
                    value(config),
                    if t == TAG_PRE_ANY {
                        self.any_prefix_max
                    } else {
                        self.any_suffix_max
                    },
                );
                if pending(config) {
                    // Second byte of a Hebrew letter.
                    if is_hebrew_tail_letter(byte) {
                        self.close_into(pack(t, false, count + 1), out);
                    }
                } else if count < max {
                    if is_ascii_letter(byte) {
                        self.close_into(pack(t, false, count + 1), out);
                    } else if byte == HEBREW_LEAD {
                        self.close_into(pack(t, true, count), out);
                    }
                }
            }
            TAG_CORE => {
                if let Some(next) = self.core.step(value(config), byte) {
                    self.close_into(pack(TAG_CORE, false, next), out);
                }
                if self.transparent_quotes && byte == b'"' {
                    self.close_into(config, out);
                }
            }
            TAG_POST => {
                if let Some(suffixes) = &self.suffixes {
                    if let Some(next) = suffixes.step(value(config), byte) {
                        self.close_into(pack(TAG_POST, false, next), out);
                    }
                }
            }
            _ => {}
        }
    }

    fn config_is_match(&self, config: Config) -> bool {
        if pending(config) {
            return false;
        }
        match tag(config) {
            // Zero suffix: the word may simply end here.
            TAG_CORE => self.core.node_is_match(value(config)),
            TAG_POST => self
                .suffixes
                .as_ref()
                .is_some_and(|s| s.node_is_match(value(config))),
            TAG_POST_ANY => value(config) >= 1,
            _ => false,
        }
    }
}

impl Automaton for HebrewWalkAutomaton {
    type State = Vec<Config>;

    fn start(&self) -> Vec<Config> {
        self.start_configs()
    }

    fn is_match(&self, state: &Vec<Config>) -> bool {
        state.iter().any(|&c| self.config_is_match(c))
    }

    fn accept(&self, state: &Vec<Config>, byte: u8) -> Vec<Config> {
        let mut out = Vec::with_capacity(state.len());
        for &config in state {
            self.step_config(config, byte, &mut out);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    fn can_match(&self, state: &Vec<Config>) -> bool {
        !state.is_empty()
    }

    fn will_always_match(&self, _state: &Vec<Config>) -> bool {
        false
    }
}

/// Tantivy query running the Otzaria walk for one word.
#[derive(Debug, Clone)]
pub struct HebrewWalkQuery {
    field: Field,
    word: String,
    options: WalkOptions,
    matched_terms_sink: Option<MatchedTermsSink>,
}

impl HebrewWalkQuery {
    /// `word`: normalized base word. `options`: the enabled walk options.
    pub fn new(field: Field, word: impl Into<String>, options: WalkOptions) -> HebrewWalkQuery {
        HebrewWalkQuery {
            field,
            word: word.into(),
            options,
            matched_terms_sink: None,
        }
    }

    /// Collects the indexed forms this walk matches into `sink` during the
    /// search.
    pub fn set_matched_terms_sink(&mut self, sink: MatchedTermsSink) {
        self.matched_terms_sink = Some(sink);
    }
}

impl Query for HebrewWalkQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let automaton = HebrewWalkAutomaton::new(&self.word, &self.options);
        let mut weight = AutomatonWeight::new(self.field, automaton);
        if let Some(sink) = &self.matched_terms_sink {
            weight.set_matched_terms_sink(sink.clone());
        }
        Ok(Box::new(weight))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(a: &HebrewWalkAutomaton, s: &str) -> bool {
        let mut state = a.start();
        for &b in s.as_bytes() {
            if !a.can_match(&state) {
                return false;
            }
            state = a.accept(&state, b);
        }
        a.is_match(&state)
    }

    fn walk(word: &str, options: WalkOptions) -> HebrewWalkAutomaton {
        HebrewWalkAutomaton::new(word, &options)
    }

    #[test]
    fn exact_by_default() {
        let a = walk("תורה", WalkOptions::default());
        assert!(matches(&a, "תורה"));
        assert!(!matches(&a, "התורה"));
        assert!(!matches(&a, "תורהה"));
    }

    #[test]
    fn grammar_prefixes_combo() {
        let a = walk(
            "תורה",
            WalkOptions {
                grammar_prefixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "תורה"));
        assert!(matches(&a, "התורה"));
        assert!(matches(&a, "בתורה"));
        assert!(matches(&a, "והתורה")); // combo ו+ה
        assert!(!matches(&a, "דתורה")); // Aramaic off
        assert!(!matches(&a, "קתורה")); // not a prefix unit
    }

    #[test]
    fn aramaic_prefixes() {
        let a = walk(
            "מלכתא",
            WalkOptions {
                aramaic_prefixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "דמלכתא"));
        assert!(matches(&a, "ודמלכתא")); // ו+ד
        assert!(matches(&a, "דאמלכתא"));
    }

    #[test]
    fn grammar_suffixes() {
        let a = walk(
            "ספר",
            WalkOptions {
                grammar_suffixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "ספר"));
        assert!(matches(&a, "ספרים"));
        assert!(matches(&a, "ספריו"));
        assert!(matches(&a, "ספרותיהם")); // extra suffix from the spec
        assert!(!matches(&a, "הספרים")); // prefixes off
    }

    #[test]
    fn aramaic_suffix_equivalence() {
        let a = walk(
            "מלכה",
            WalkOptions {
                aramaic_suffixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "מלכה"));
        assert!(matches(&a, "מלכא"));
        let a = walk(
            "חכמים",
            WalkOptions {
                aramaic_suffixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "חכמין"));
    }

    #[test]
    fn ktiv_male_haser() {
        let a = walk(
            "מצות",
            WalkOptions {
                ktiv_male_haser: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "מצות"));
        assert!(matches(&a, "מצוות"));
    }

    #[test]
    fn part_of_word() {
        // ספר has 3 letters → up to 3 any-letters per side
        let a = walk(
            "ספר",
            WalkOptions {
                part_of_word: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "ספר"));
        assert!(matches(&a, "מספרים")); // 1 before, 2 after
        assert!(matches(&a, "הספר"));
        assert!(!matches(&a, "ספ")); // the whole word must appear
        // 4+-letter word → only 2 per side
        let a4 = walk(
            "תורות",
            WalkOptions {
                part_of_word: true,
                ..Default::default()
            },
        );
        assert!(matches(&a4, "בתורות"));
        assert!(!matches(&a4, "אבגתורות")); // 3 before > 2
    }

    #[test]
    fn advanced_prefixes_any_letter() {
        let a = walk(
            "תורה", // 4 letters, not short → max 3 before
            WalkOptions {
                advanced_prefixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "קקקתורה")); // any letters, not just units
        assert!(!matches(&a, "קקקקתורה")); // 4 > 3
        assert!(!matches(&a, "תורהק")); // suffixes off
    }

    #[test]
    fn advanced_suffixes_any_letter() {
        let a = walk(
            "תורה",
            WalkOptions {
                advanced_suffixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "תורהקקקקק")); // 5 after
        assert!(!matches(&a, "תורהקקקקקק")); // 6 > 5
    }

    #[test]
    fn ignore_quotes() {
        let a = walk(
            "רמבם",
            WalkOptions {
                ignore_quotes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "רמבם"));
        assert!(matches(&a, "רמב\"ם")); // indexed with canonical '"'
        // and a quoted query matches an unquoted indexed term
        let a2 = walk(
            "רמב\"ם",
            WalkOptions {
                ignore_quotes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a2, "רמבם"));
        assert!(matches(&a2, "רמב\"ם"));
        // off → exact
        let a3 = walk("רמבם", WalkOptions::default());
        assert!(!matches(&a3, "רמב\"ם"));
    }

    #[test]
    fn spelling_errors() {
        let a = walk(
            "אבד",
            WalkOptions {
                spelling_errors: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "עבד")); // א↔ע
        assert!(matches(&a, "אבר")); // ד↔ר OCR
    }

    #[test]
    fn full_morphology_combo() {
        // grammatical prefixes + suffixes together, with Aramaic
        let a = walk(
            "ספר",
            WalkOptions {
                grammar_prefixes: true,
                grammar_suffixes: true,
                aramaic_prefixes: true,
                aramaic_suffixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "הספרים")); // prefix + suffix
        assert!(matches(&a, "וספריו"));
        assert!(matches(&a, "דספרין")); // Aramaic prefix + ים↔ין suffix variant
        assert!(matches(&a, "ספריא")); // rare Aramaic suffix in full morphology
        assert!(!matches(&a, "ספרקים")); // junk between word and suffix
    }
}
