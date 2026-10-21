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
//! - כתיב מלא/חסר: Otzaria's י/ו rules (`variants::ktiv_male_haser_variants_limited`):
//!   מצות ↔ מצוות but not מצת, אותו ↛ אות, שבת ↛ שבות (≤32 variants, 16
//!   with grammatical options).
//! - קידומות דקדוקיות: combos of ו מ כש כ ב ש ל ה up to 3 letters; with
//!   Aramaic also ד דא א (ודמלכתא). תורה ↔ והתורה. A root under 3 letters
//!   takes none (ית ↛ בית) unless an abbreviation (ר' → ור').
//! - סיומות דקדוקיות: Otzaria's table (`GRAM_SUFFIXES`), past-tense endings
//!   for 3–4 letter roots; a final letter takes its base form (מלך → מלכים) and
//!   a final ה turns to ת/ות (תורה → תורתו); a root under 3 letters takes
//!   none (אב ↛ אבות) unless an abbreviation (ר' → ר'ים).
//! - סיומות ארמיות: ending equivalence ה↔א, ם↔ן on the root.
//! - Always (unless `exact_quotes`): an index term also matches by its
//!   quote-free form, רמבם ↔ רמב"ם (Otzaria's quote-free twin, at query time).
//! - קידומות/סיומות מתקדם: ANY letters — ≤3 before (≤5 for a short word),
//!   ≤5 after (≤7 for a short word); both together ≡ חלק ממילה.
//! - התעלם מגרשיים: the canonical '"' is transparent anywhere.
//! - שגיאות כתיב/OCR: single confusion-pair substitutions, or one quote
//!   dropped (רמב"ם → רמבם, as Otzaria's typo deletion).

use std::collections::BTreeSet;
use std::sync::Arc;

use tantivy::query::{
    AutomatonWeight, EnableScoring, MatchedTermsSink, Query, SpanTermSource, Weight,
};
use tantivy::schema::Field;
use tantivy::termdict::ZeroWidthBytes;
use tantivy_fst::Automaton;

use crate::quote_free::QuoteFree;
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
    /// Off (default, Otzaria): an index term also matches by its quote-free
    /// form, so `רמבם` finds `רמב"ם` (Otzaria's quote-free twin token, done at
    /// query time by [`crate::quote_free::QuoteFree`]). On: index terms must
    /// match as written.
    pub exact_quotes: bool,
    /// שגיאות כתיב/OCR מצויות
    pub spelling_errors: bool,
    /// Ignore nikud: בראשית matches בְּרֵאשִׁית (needs a marks-keeping field,
    /// `hebrew_marked`). Off: vocalization must match exactly.
    pub ignore_nikud: bool,
    /// Ignore te'amim (cantillation), independently of nikud.
    pub ignore_teamim: bool,

    // ── Limits, set by the caller (Otzaria). None = the defaults below. ──
    /// קידומות (מתקדם): max letters before the word. Default 3, or 5 for a
    /// word of ≤3 letters.
    pub prefix_max_letters: Option<u32>,
    /// סיומות (מתקדם): max letters after the word. Default 5, or 7 for a
    /// word of ≤3 letters.
    pub suffix_max_letters: Option<u32>,
    /// חלק ממילה: max letters on each side. Default 3, or 2 for a word of
    /// ≥4 letters.
    pub part_of_word_max_letters: Option<u32>,
    /// קידומות דקדוקיות: max total letters of a stacked prefix. Default 3.
    pub grammar_prefix_max_letters: Option<u32>,
}

/// Prefix units for the closed grammatical prefix phase.
const PREFIX_UNITS: [&str; 8] = ["ו", "מ", "כש", "כ", "ב", "ש", "ל", "ה"];
const ARAMAIC_PREFIX_UNITS: [&str; 3] = ["דא", "ד", "א"];

/// סיומות דקדוקיות: Otzaria's `GRAM_SUFFIX_ALTS` (hebrew_query.rs), possessive,
/// plural and the rare יות/יא/תא endings. Not FtsLib's `%` table
/// ([`grammar::SUFFIXES`](crate::grammar::SUFFIXES)), which stays FtsLib's.
const GRAM_SUFFIXES: [&str; 32] = [
    "ותי", "ותיך", "ותיו", "ותיה", "ותינו", "ותיכם", "ותיכן", "ותיהם", "ותיהן", "יות", "יי",
    "יך", "יו", "יה", "יא", "תא", "ינו", "יכם", "יכן", "יהם", "יהן", "י", "ך", "ו", "ה", "נו",
    "כם", "כן", "ם", "ן", "ים", "ות",
];
/// Past-tense person endings (Otzaria `VERB_SUFFIX_ALTS`), only for a
/// verb-sized root: see [`takes_verb_suffixes`].
const VERB_SUFFIXES: [&str; 4] = ["תי", "ת", "תם", "תן"];
/// A root ending in ה drops it before a suffix: ת or ות (Otzaria `HE_STEM_ALTS`).
const HE_STEMS: [&str; 2] = ["ות", "ת"];
/// Grammatical prefixes and suffixes need a root of this many letters
/// (Otzaria `MIN_GRAM_PREFIX_ROOT_LETTERS` / `MIN_GRAM_SUFFIX_ROOT_LETTERS`);
/// abbreviations (a quote inside: ר', רמב"ם) are exempt.
const MIN_GRAM_ROOT_LETTERS: usize = 3;

// Tags of a core form (its trie node's flags): what may follow it.
/// The form may end the term.
const CORE_BARE: u8 = SetAutomaton::MATCH;
/// Grammatical suffixes may follow.
const CORE_GRAM: u8 = 2;
/// The past-tense endings may follow too.
const CORE_VERB: u8 = 4;
/// The advanced any-letter suffix window may follow.
const CORE_WINDOW: u8 = 8;
/// The form takes grammatical prefixes (root long enough, or an abbreviation).
const CORE_PREFIXABLE: u8 = 16;

/// All closed-list prefixes: sequences of distinct units, total ≤ `max_letters`.
fn closed_prefixes(aramaic: bool, max_letters: usize) -> BTreeSet<String> {
    let mut units: Vec<&str> = PREFIX_UNITS.to_vec();
    if aramaic {
        units.extend(ARAMAIC_PREFIX_UNITS);
    }
    let mut out = BTreeSet::new();
    // Each unit is used at most once, so depth is bounded by the unit count.
    fn recurse(units: &[&str], current: String, used: u32, max: usize, out: &mut BTreeSet<String>) {
        for (i, unit) in units.iter().enumerate() {
            if used & (1 << i) != 0 {
                continue;
            }
            let combined = format!("{current}{unit}");
            if combined.chars().count() > max {
                continue;
            }
            out.insert(combined.clone());
            recurse(units, combined, used | (1 << i), max, out);
        }
    }
    recurse(&units, String::new(), 0, max_letters, &mut out);
    out
}

fn is_quote_char(c: char) -> bool {
    matches!(c, '\'' | '"')
}

/// Letters of a form, marks not counted (Otzaria `base_len`).
fn form_letters(form: &str) -> usize {
    form.chars()
        .filter(|&c| !crate::chars::is_nikud(c) && !crate::chars::is_taam(c))
        .count()
}

/// Byte index and char of the form's last letter, marks skipped.
fn last_letter(form: &str) -> Option<(usize, char)> {
    form.char_indices()
        .rev()
        .find(|&(_, c)| !crate::chars::is_nikud(c) && !crate::chars::is_taam(c))
}

/// The base form of a final letter (מלך → מלכ־ before a suffix).
fn base_form_of_final(c: char) -> Option<char> {
    match c {
        'ך' => Some('כ'),
        'ם' => Some('מ'),
        'ן' => Some('נ'),
        'ף' => Some('פ'),
        'ץ' => Some('צ'),
        _ => None,
    }
}

/// Otzaria `takes_verb_suffixes`: a 3–4 letter root, not an abbreviation,
/// not ending in ה or ת (whose past forms change shape).
fn takes_verb_suffixes(form: &str, letters: usize) -> bool {
    (3..=4).contains(&letters)
        && !form.contains(is_quote_char)
        && last_letter(form).is_some_and(|(_, c)| c != 'ה' && c != 'ת')
}

/// The tagged core forms of one root form: a port of Otzaria's
/// `affix_arms` root-length rule and `suffix_sides` (hebrew_query.rs).
/// Before a suffix a final letter takes its base form (מלך → מלכים, never
/// מלךים) and a final ה gives way to ת/ות (תורה → תורתו/תורות, never
/// תורהו); a root of fewer than 3 letters takes no grammatical affix
/// (אב ↛ אבות, מה ↛ מות, ית ↛ בית) unless it is an abbreviation (ר' → ר'ים).
fn tagged_forms(form: &str, options: &WalkOptions, window: bool, out: &mut Vec<(String, u8)>) {
    let letters = form_letters(form);
    let Some((at, last)) = last_letter(form) else {
        return;
    };
    let long_enough = form.contains(is_quote_char) || letters >= MIN_GRAM_ROOT_LETTERS;
    let prefixable = if long_enough { CORE_PREFIXABLE } else { 0 };
    let gram = options.grammar_suffixes && long_enough;
    let gram_tags = if gram {
        CORE_GRAM
            | if takes_verb_suffixes(form, letters) {
                CORE_VERB
            } else {
                0
            }
    } else {
        0
    };
    let window_tag = if window { CORE_WINDOW } else { 0 };
    if !window && !gram {
        out.push((form.to_string(), CORE_BARE | prefixable));
        return;
    }
    let head = &form[..at];
    let tail = &form[at + last.len_utf8()..];
    if let Some(base) = base_form_of_final(last) {
        out.push((form.to_string(), CORE_BARE | prefixable));
        out.push((format!("{head}{base}{tail}"), window_tag | gram_tags | prefixable));
        return;
    }
    if last == 'ה' && gram {
        out.push((form.to_string(), CORE_BARE | window_tag | prefixable));
        for stem in HE_STEMS {
            out.push((format!("{head}{stem}"), CORE_BARE | gram_tags | prefixable));
        }
        return;
    }
    out.push((form.to_string(), CORE_BARE | window_tag | gram_tags | prefixable));
}

/// Builds the core word-variant set from the enabled generators, in
/// Otzaria's order (`build_word_regex` → `word_to_pattern`): typo variants,
/// each with its Aramaic final-letter swap, each fanned into its כתיב
/// מלא/חסר variants.
fn core_variants(word: &str, options: &WalkOptions) -> BTreeSet<String> {
    // Strip from the query word whatever the walk will treat as transparent:
    // the self-loops then do the matching against the index.
    let word: String = word
        .chars()
        .filter(|&c| {
            !(options.ignore_quotes && c == '"')
                && !(options.ignore_nikud && crate::chars::is_nikud(c))
                && !(options.ignore_teamim && crate::chars::is_taam(c))
        })
        .collect();
    let mut set = BTreeSet::new();
    set.insert(word);
    if options.spelling_errors {
        set = variants::expand_set(&set, variants::spelling_variants);
    }
    if options.aramaic_suffixes {
        set = variants::expand_set(&set, variants::aramaic_ending_variants);
    }
    if options.ktiv_male_haser {
        // Otzaria `spelling_limit`: a tighter cap through the grammatical arms.
        let limit = if options.grammar_prefixes || options.grammar_suffixes || options.aramaic_prefixes
        {
            variants::KTIV_GRAMMATICAL_VARIANTS_CAP
        } else {
            variants::KTIV_VARIANTS_CAP
        };
        set = variants::expand_set(&set, |w| {
            variants::ktiv_male_haser_variants_limited(w, limit)
                .into_iter()
                .collect()
        });
    }
    set
}

// ── Composite automaton ──────────────────────────────────────────────────

/// NFA configuration, packed: tag(3) | pending-kind(3) | value(32).
/// Tags: 0 = closed-prefix trie node, 1 = any-prefix letter count,
/// 2 = core trie node, 3 = closed-suffix trie node, 4 = any-suffix count.
/// Pending kinds: 0 = none; 1 = saw a Hebrew letter lead byte (0xD7) in an
/// any-letter phase; 2 = saw a possible mark lead 0xD6 (skipping a
/// nikud/ta'am); 3 = saw a possible 0xD7-encoded nikud (shin/sin dots,
/// qamatz qatan).
type Config = u64;

const TAG_PRE: u64 = 0;
const TAG_PRE_ANY: u64 = 1;
const TAG_CORE: u64 = 2;
const TAG_POST: u64 = 3;
const TAG_POST_ANY: u64 = 4;
/// Closed-suffix trie node of the grammatical + past-tense suffix set.
const TAG_POST_VERB: u64 = 5;

/// A core config reached through a grammatical prefix (bit 38): its form
/// must be tagged [`CORE_PREFIXABLE`] to end or take a suffix.
const PREFIXED: u64 = 1 << 38;

const PEND_NONE: u64 = 0;
const PEND_LETTER: u64 = 1;
const PEND_MARK_D6: u64 = 2;
const PEND_MARK_D7: u64 = 3;

#[inline]
fn pack(tag: u64, pending: u64, value: u32) -> Config {
    (tag << 35) | (pending << 32) | value as u64
}
#[inline]
fn tag(c: Config) -> u64 {
    (c >> 35) & 0b111
}
#[inline]
fn pending(c: Config) -> u64 {
    (c >> 32) & 0b111
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
/// Lead byte of most Hebrew marks (U+0591..U+05BF = 0xD6 0x91..0xBF).
const MARK_LEAD: u8 = 0xD6;
#[inline]
fn is_hebrew_tail_letter(b: u8) -> bool {
    (0x90..=0xAA).contains(&b)
}
/// Tail byte of a ta'am after 0xD6 (U+0591..U+05AF).
#[inline]
fn is_taam_tail(b: u8) -> bool {
    (0x91..=0xAF).contains(&b)
}
/// Tail byte of a nikud mark after 0xD6 (U+05B0..U+05BF).
#[inline]
fn is_nikud_d6_tail(b: u8) -> bool {
    (0xB0..=0xBF).contains(&b)
}
/// Tail byte of a 0xD7-encoded nikud: shin/sin dots (05C1/05C2), upper/lower
/// dots (05C4/05C5), qamatz qatan (05C7).
#[inline]
fn is_nikud_d7_tail(b: u8) -> bool {
    matches!(b, 0x81 | 0x82 | 0x84 | 0x85 | 0x87)
}

/// The composite walk automaton. State = sorted set of NFA configs; empty =
/// dead.
#[derive(Debug)]
pub struct HebrewWalkAutomaton {
    core: SetAutomaton,
    prefixes: Option<SetAutomaton>,
    any_prefix_max: u32,
    suffixes: Option<SetAutomaton>,
    /// Grammatical suffixes plus the past-tense endings (verb-sized roots).
    verb_suffixes: Option<SetAutomaton>,
    any_suffix_max: u32,
    /// A grammatical prefix needs a core form tagged [`CORE_PREFIXABLE`].
    /// Off under Aramaic prefixes: Otzaria's Aramaic arm has no root floor.
    gate_prefixes: bool,
    transparent_quotes: bool,
    ignore_nikud: bool,
    ignore_teamim: bool,
}

impl HebrewWalkAutomaton {
    /// Builds the automaton for `word` under `options`. `word` must already be
    /// normalized the way the index normalizes (lowercase ASCII, nikud
    /// stripped, canonical '"').
    pub fn new(word: &str, options: &WalkOptions) -> HebrewWalkAutomaton {
        let letters = word.chars().filter(|&c| crate::chars::is_letter(c)).count();
        let short = letters <= SHORT_WORD_MAX_LETTERS;

        let grammar_prefixes = options.grammar_prefixes || options.aramaic_prefixes;

        // Advanced any-letter sides; both together ≡ part-of-word per spec.
        // Every limit is the caller's when set; the defaults are Otzaria's.
        let part_of_word_max = options
            .part_of_word_max_letters
            .unwrap_or(if letters >= 4 { 2 } else { 3 });
        let (mut any_pre, mut any_post) = (0u32, 0u32);
        if options.part_of_word || (options.advanced_prefixes && options.advanced_suffixes) {
            any_pre = part_of_word_max;
            any_post = part_of_word_max;
        } else {
            if options.advanced_prefixes {
                any_pre = options
                    .prefix_max_letters
                    .unwrap_or(if short { 5 } else { 3 });
            }
            if options.advanced_suffixes {
                any_post = options
                    .suffix_max_letters
                    .unwrap_or(if short { 7 } else { 5 });
            }
        }
        let grammar_prefix_max = options.grammar_prefix_max_letters.unwrap_or(3) as usize;

        let prefixes = if any_pre == 0 && grammar_prefixes {
            Some(SetAutomaton::from_strings(
                &closed_prefixes(options.aramaic_prefixes, grammar_prefix_max),
            ))
        } else {
            None
        };
        // Otzaria composes the suffix window with the grammatical suffixes
        // (`affix_arms`: the trail is the union of the selected options).
        let (suffixes, verb_suffixes) = if options.grammar_suffixes {
            (
                Some(SetAutomaton::from_strings(GRAM_SUFFIXES)),
                Some(SetAutomaton::from_strings(GRAM_SUFFIXES.iter().chain(&VERB_SUFFIXES))),
            )
        } else {
            (None, None)
        };
        let mut tagged = Vec::new();
        for form in core_variants(word, options) {
            tagged_forms(&form, options, any_post > 0, &mut tagged);
        }

        HebrewWalkAutomaton {
            core: SetAutomaton::from_tagged(tagged),
            prefixes,
            any_prefix_max: any_pre,
            suffixes,
            verb_suffixes,
            any_suffix_max: any_post,
            gate_prefixes: !options.aramaic_prefixes,
            transparent_quotes: options.ignore_quotes,
            ignore_nikud: options.ignore_nikud,
            ignore_teamim: options.ignore_teamim,
        }
    }

    /// ε-closure: phase-boundary spawns.
    fn close_into(&self, config: Config, set: &mut Vec<Config>) {
        if set.contains(&config) {
            return;
        }
        set.push(config);
        if pending(config) != PEND_NONE {
            return;
        }
        match tag(config) {
            TAG_PRE => {
                if let Some(prefixes) = &self.prefixes {
                    if prefixes.node_is_match(value(config)) {
                        let gate = if self.gate_prefixes { PREFIXED } else { 0 };
                        self.close_into(
                            pack(TAG_CORE, PEND_NONE, SetAutomaton::ROOT) | gate,
                            set,
                        );
                    }
                }
            }
            TAG_PRE_ANY => {
                self.close_into(pack(TAG_CORE, PEND_NONE, SetAutomaton::ROOT), set);
            }
            TAG_CORE => {
                let flags = self.core_flags(config);
                if flags & CORE_GRAM != 0 {
                    if flags & CORE_VERB != 0 && self.verb_suffixes.is_some() {
                        self.close_into(pack(TAG_POST_VERB, PEND_NONE, SetAutomaton::ROOT), set);
                    } else if self.suffixes.is_some() {
                        self.close_into(pack(TAG_POST, PEND_NONE, SetAutomaton::ROOT), set);
                    }
                }
                if flags & CORE_WINDOW != 0 && self.any_suffix_max > 0 {
                    self.close_into(pack(TAG_POST_ANY, PEND_NONE, 0), set);
                }
            }
            _ => {}
        }
    }

    fn start_configs(&self) -> Vec<Config> {
        let mut set = Vec::new();
        if self.prefixes.is_some() {
            self.close_into(pack(TAG_PRE, PEND_NONE, SetAutomaton::ROOT), &mut set);
        }
        if self.any_prefix_max > 0 {
            self.close_into(pack(TAG_PRE_ANY, PEND_NONE, 0), &mut set);
        }
        self.close_into(pack(TAG_CORE, PEND_NONE, SetAutomaton::ROOT), &mut set);
        set.sort_unstable();
        set
    }

    /// Successors of one config on `byte`, with closure, into `out`.
    /// The tags of the core form ending at a core config (0: none ends
    /// here, or it was reached through a grammatical prefix it cannot take).
    fn core_flags(&self, config: Config) -> u8 {
        let flags = self.core.node_flags(value(config));
        if config & PREFIXED != 0 && flags & CORE_PREFIXABLE == 0 {
            0
        } else {
            flags
        }
    }

    fn step_config(&self, config: Config, byte: u8, out: &mut Vec<Config>) {
        let t = tag(config);
        // The prefix gate travels with the core configs.
        let gate = config & PREFIXED;
        match pending(config) {
            // Resolving a possible skipped mark: 0xD6 <tail>.
            PEND_MARK_D6 => {
                if (self.ignore_teamim && is_taam_tail(byte))
                    || (self.ignore_nikud && is_nikud_d6_tail(byte))
                {
                    // Mark skipped; back to where we were.
                    self.close_into(pack(t, PEND_NONE, value(config)) | gate, out);
                }
                return;
            }
            // Resolving a possible skipped 0xD7-encoded nikud.
            PEND_MARK_D7 => {
                if self.ignore_nikud && is_nikud_d7_tail(byte) {
                    self.close_into(pack(t, PEND_NONE, value(config)) | gate, out);
                }
                return;
            }
            // Second byte of a Hebrew letter in an any-letter phase.
            PEND_LETTER => {
                if is_hebrew_tail_letter(byte) {
                    self.close_into(pack(t, PEND_NONE, value(config) + 1), out);
                }
                return;
            }
            _ => {}
        }

        // Mark transparency applies in every phase: a mark may begin here.
        if byte == MARK_LEAD && (self.ignore_nikud || self.ignore_teamim) {
            self.close_into(pack(t, PEND_MARK_D6, value(config)) | gate, out);
        }
        if byte == HEBREW_LEAD && self.ignore_nikud {
            self.close_into(pack(t, PEND_MARK_D7, value(config)) | gate, out);
        }

        match t {
            TAG_PRE => {
                if let Some(prefixes) = &self.prefixes {
                    if let Some(next) = prefixes.step(value(config), byte) {
                        self.close_into(pack(TAG_PRE, PEND_NONE, next), out);
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
                if count < max {
                    if is_ascii_letter(byte) {
                        self.close_into(pack(t, PEND_NONE, count + 1), out);
                    } else if byte == HEBREW_LEAD {
                        self.close_into(pack(t, PEND_LETTER, count), out);
                    }
                }
            }
            TAG_CORE => {
                if let Some(next) = self.core.step(value(config), byte) {
                    self.close_into(pack(TAG_CORE, PEND_NONE, next) | gate, out);
                }
                if self.transparent_quotes && byte == b'"' {
                    self.close_into(config, out);
                }
            }
            TAG_POST | TAG_POST_VERB => {
                let suffixes = if t == TAG_POST {
                    &self.suffixes
                } else {
                    &self.verb_suffixes
                };
                if let Some(suffixes) = suffixes {
                    if let Some(next) = suffixes.step(value(config), byte) {
                        self.close_into(pack(t, PEND_NONE, next), out);
                    }
                }
            }
            _ => {}
        }
    }

    fn config_is_match(&self, config: Config) -> bool {
        if pending(config) != PEND_NONE {
            return false;
        }
        match tag(config) {
            // Zero suffix: the word may simply end here.
            TAG_CORE => self.core_flags(config) & CORE_BARE != 0,
            TAG_POST => self
                .suffixes
                .as_ref()
                .is_some_and(|s| s.node_is_match(value(config))),
            TAG_POST_VERB => self
                .verb_suffixes
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

/// FtsLib's cap on how many index terms one query word may expand to
/// (`HebrewWildcardExpander.DefaultMaxExpandedTerms`,
/// `FuzzyExpander.DefaultMaxExpandedTerms`).
pub const DEFAULT_MAX_EXPANDED_TERMS: usize = 2000;

/// Bytes that do not count towards a term's length when the cap prefers
/// shorter terms: the intra-word quote the tokenizer keeps (רש"י is as
/// short as רשי, as in FtsLib whose terms carried no quotes).
pub const ZERO_WIDTH_BYTES: ZeroWidthBytes = ZeroWidthBytes::new(b"\"");

/// Tantivy query running the Otzaria walk for one word.
#[derive(Debug, Clone)]
pub struct HebrewWalkQuery {
    field: Field,
    word: String,
    options: WalkOptions,
    matched_terms_sink: Option<MatchedTermsSink>,
    max_expansions: usize,
}

impl HebrewWalkQuery {
    /// `word`: normalized base word. `options`: the enabled walk options.
    /// The expansion is capped at [`DEFAULT_MAX_EXPANDED_TERMS`] (shortest
    /// forms first, then most frequent), as in FtsLib.
    pub fn new(field: Field, word: impl Into<String>, options: WalkOptions) -> HebrewWalkQuery {
        HebrewWalkQuery {
            field,
            word: word.into(),
            options,
            matched_terms_sink: None,
            max_expansions: DEFAULT_MAX_EXPANDED_TERMS,
        }
    }

    /// Collects the indexed forms this walk matches into `sink` during the
    /// search.
    pub fn set_matched_terms_sink(&mut self, sink: MatchedTermsSink) {
        self.matched_terms_sink = Some(sink);
    }

    /// Overrides the expansion cap (per segment).
    pub fn set_max_expansions(&mut self, cap: usize) {
        self.max_expansions = cap;
    }

    fn automaton_weight(&self) -> AutomatonWeight<QuoteFree<HebrewWalkAutomaton>> {
        let automaton = QuoteFree::new(
            HebrewWalkAutomaton::new(&self.word, &self.options),
            !self.options.exact_quotes,
        );
        let mut weight = AutomatonWeight::new(self.field, automaton);
        if let Some(sink) = &self.matched_terms_sink {
            weight.set_matched_terms_sink(sink.clone());
        }
        weight.set_max_expansions(self.max_expansions);
        weight.set_zero_width_bytes(ZERO_WIDTH_BYTES);
        weight
    }

    /// This walk as a slot of a [`SpanQuery`](tantivy::query::SpanQuery):
    /// "any word this walk matches", with positions. The sink, if set, still
    /// collects the matched forms. The expansion cap applies per
    /// segment.
    pub fn span_slot(&self, _searcher: &tantivy::Searcher) -> tantivy::Result<Arc<dyn SpanTermSource>> {
        Ok(Arc::new(self.automaton_weight()))
    }
}

impl Query for HebrewWalkQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(self.automaton_weight()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches<A: Automaton>(a: &A, s: &str) -> bool {
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
    fn caller_set_limits_override_defaults() {
        // advanced prefixes: default 3 for a 4-letter word; caller sets 1
        let a = walk(
            "תורה",
            WalkOptions {
                advanced_prefixes: true,
                prefix_max_letters: Some(1),
                ..Default::default()
            },
        );
        assert!(matches(&a, "קתורה"));
        assert!(!matches(&a, "קקתורה"));
        // advanced suffixes: caller sets 2
        let a = walk(
            "תורה",
            WalkOptions {
                advanced_suffixes: true,
                suffix_max_letters: Some(2),
                ..Default::default()
            },
        );
        assert!(matches(&a, "תורהקק"));
        assert!(!matches(&a, "תורהקקק"));
        // part of word: caller sets 1 per side
        let a = walk(
            "ספר",
            WalkOptions {
                part_of_word: true,
                part_of_word_max_letters: Some(1),
                ..Default::default()
            },
        );
        assert!(matches(&a, "מספרי"));
        assert!(!matches(&a, "מספרים"));
        // grammatical prefixes: caller sets 1 letter → no stacking
        let a = walk(
            "תורה",
            WalkOptions {
                grammar_prefixes: true,
                grammar_prefix_max_letters: Some(1),
                ..Default::default()
            },
        );
        assert!(matches(&a, "התורה"));
        assert!(!matches(&a, "והתורה"));
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
    fn ignore_nikud() {
        let a = walk(
            "בראשית",
            WalkOptions {
                ignore_nikud: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "בראשית"));
        assert!(matches(&a, "בְּרֵאשִׁית")); // vocalized indexed form
        // off → exact only
        let a2 = walk("בראשית", WalkOptions::default());
        assert!(!matches(&a2, "בְּרֵאשִׁית"));
        // a vocalized QUERY with the flag matches the bare indexed form
        let a3 = walk(
            "בְּרֵאשִׁית",
            WalkOptions {
                ignore_nikud: true,
                ..Default::default()
            },
        );
        assert!(matches(&a3, "בראשית"));
    }

    #[test]
    fn ignore_teamim_independent_of_nikud() {
        // word with an etnahta (ta'am) only
        let with_taam = "ברא\u{0591}שית";
        let a = walk(
            "בראשית",
            WalkOptions {
                ignore_teamim: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, with_taam));
        // but nikud is NOT ignored by the teamim flag
        assert!(!matches(&a, "בְּרֵאשִׁית"));
        // and vice versa: nikud flag alone doesn't skip teamim
        let a2 = walk(
            "בראשית",
            WalkOptions {
                ignore_nikud: true,
                ..Default::default()
            },
        );
        assert!(!matches(&a2, with_taam));
        // both flags: fully vocalized + cantillated matches
        let a3 = walk(
            "בראשית",
            WalkOptions {
                ignore_nikud: true,
                ignore_teamim: true,
                ..Default::default()
            },
        );
        assert!(matches(&a3, "בְּרֵ\u{0591}אשִׁית"));
    }

    #[test]
    fn nikud_transparency_composes_with_prefixes() {
        let a = walk(
            "תורה",
            WalkOptions {
                grammar_prefixes: true,
                ignore_nikud: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "הַתּוֹרָה")); // vocalized prefix + word
    }

    #[test]
    fn walks_as_span_slots() -> tantivy::Result<()> {
        use tantivy::collector::DocSetCollector;
        use tantivy::query::SpanQuery;
        use tantivy::schema::{IndexRecordOption, Schema, TextFieldIndexing, TextOptions};
        use tantivy::{doc, Index, IndexWriter};

        let mut schema_builder = Schema::builder();
        let indexing = TextFieldIndexing::default()
            .set_tokenizer(crate::HEBREW_TOKENIZER_NAME)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions);
        let text =
            schema_builder.add_text_field("text", TextOptions::default().set_indexing_options(indexing));
        let index = Index::create_in_ram(schema_builder.build());
        crate::register_hebrew_tokenizer(index.tokenizers());
        let mut writer: IndexWriter = index.writer(15_000_000)?;
        writer.add_document(doc!(text => "וְהַתּוֹרָה נִתְּנָה מִסִּינַי"))?; // 0: התורה, 0 between
        writer.add_document(doc!(text => "בתורה כתוב שלא ניתנה"))?; // 1: 2 between
        writer.add_document(doc!(text => "תורה אחת ואחת ושתיים ושלוש נתנה"))?; // 2: 4 between
        writer.commit()?;
        let searcher = index.reader()?.searcher();

        let prefixes = WalkOptions {
            grammar_prefixes: true,
            ..Default::default()
        };
        let torah = HebrewWalkQuery::new(text, "תורה", prefixes);
        let natna = HebrewWalkQuery::new(
            text,
            "נתנה",
            WalkOptions {
                ktiv_male_haser: true, // נתנה ↔ ניתנה
                ..Default::default()
            },
        );
        let q = SpanQuery::new(text, torah.span_slot(&searcher)?)
            .then_within(2, natna.span_slot(&searcher)?);
        let mut docs: Vec<u32> = searcher
            .search(&q, &DocSetCollector)?
            .into_iter()
            .map(|a| a.doc_id)
            .collect();
        docs.sort_unstable();
        assert_eq!(docs, vec![0, 1]); // doc 2 has 4 words between
        Ok(())
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
        assert!(matches(&a, "דספרים")); // Aramaic prefix + suffix
        assert!(matches(&a, "ספריא")); // rare suffix in Otzaria's table
        assert!(!matches(&a, "דספרין")); // Otzaria swaps the root's ending only
        assert!(!matches(&a, "ספרקים")); // junk between word and suffix
    }

    fn quote_free(word: &str, options: WalkOptions) -> QuoteFree<HebrewWalkAutomaton> {
        QuoteFree::new(HebrewWalkAutomaton::new(word, &options), !options.exact_quotes)
    }

    // Otzaria quote_free_indexing_and_ignore_quotes_option.
    #[test]
    fn plain_word_matches_quoted_terms() {
        let a = quote_free("רמבם", WalkOptions::default());
        assert!(matches(&a, "רמבם"));
        assert!(matches(&a, "רמב\"ם"));
        let q = quote_free("רמב\"ם", WalkOptions::default());
        assert!(matches(&q, "רמב\"ם"));
        assert!(!matches(&q, "רמבם"));
        let q = quote_free("רמב\"ם", WalkOptions { ignore_quotes: true, ..Default::default() });
        assert!(matches(&q, "רמבם") && matches(&q, "רמב\"ם"));
        // composes with prefixes: the quote-free form goes through the walk
        let a = quote_free("רמבם", WalkOptions { grammar_prefixes: true, ..Default::default() });
        assert!(matches(&a, "להרמב\"ם"));
        // exact_quotes: as written only
        let a = quote_free("רמבם", WalkOptions { exact_quotes: true, ..Default::default() });
        assert!(!matches(&a, "רמב\"ם"));
    }

    // Otzaria advanced_typo_and_prefix_flags_work_on_gershayim_tokens.
    #[test]
    fn typos_and_prefixes_on_gershayim_words() {
        let a = quote_free("רמב\"ם", WalkOptions { spelling_errors: true, ..Default::default() });
        assert!(matches(&a, "רמבם"));
        let a = quote_free("רמב\"ם", WalkOptions { grammar_prefixes: true, ..Default::default() });
        assert!(matches(&a, "הרמב\"ם"));
    }

    #[test]
    fn ktiv_follows_otzaria_rules() {
        let k = WalkOptions { ktiv_male_haser: true, ..Default::default() };
        let a = walk("מצות", k);
        assert!(matches(&a, "מצוות") && matches(&a, "מצות"));
        assert!(matches(&a, "מצת")); // Otzaria: one internal ו may go
        assert!(!matches(&walk("מצוות", k), "מצת")); // but one per run
        assert!(!matches(&walk("מצת", k), "מצוות")); // no adjacent insertions
        let a = walk("אותו", k);
        assert!(matches(&a, "אותו"));
        assert!(!matches(&a, "אות"));
        let a = walk("שבת", k);
        assert!(!matches(&a, "שבות"));
        assert!(matches(&a, "שובת"));
        assert!(!matches(&walk("מצוה", k), "מצה"));
    }

    // Otzaria suffixes_follow_final_letters_and_final_he.
    #[test]
    fn grammar_suffixes_follow_final_letters_and_final_he() {
        let gs = WalkOptions { grammar_suffixes: true, ..Default::default() };
        for (root, forms) in [
            ("מלך", &["מלך", "מלכים", "מלכו", "מלכיהם", "מלכם"][..]),
            ("ארץ", &["ארץ", "ארצות", "ארצו"]),
            ("תורה", &["תורה", "תורות", "תורתו", "תורתי", "תורותיו", "תורתם"]),
            ("מצוה", &["מצות", "מצוות", "מצותיו"]),
        ] {
            let a = walk(root, gs);
            for form in forms {
                assert!(matches(&a, form), "{root} → {form}");
            }
        }
        for (root, form) in [("מלך", "מלךים"), ("ארץ", "ארץות"), ("תורה", "תורהו"), ("תורה", "תורהה")] {
            assert!(!matches(&walk(root, gs), form), "{root} ↛ {form}");
        }
        // the suffix window too follows the final letter
        let w = WalkOptions { advanced_suffixes: true, ..Default::default() };
        assert!(matches(&walk("מלך", w), "מלכים"));
        assert!(!matches(&walk("מלך", w), "מלךים"));
    }

    // Otzaria grammatical_suffixes_need_a_root.
    #[test]
    fn grammar_suffixes_need_a_root() {
        let gs = WalkOptions { grammar_suffixes: true, ..Default::default() };
        for (word, term) in [("מה", "מות"), ("מה", "מתי"), ("זה", "זות"), ("אב", "אבות")] {
            assert!(!matches(&walk(word, gs), term), "{word} {term}");
            assert!(matches(&walk(word, gs), word), "{word}");
        }
        assert!(matches(&walk("תורה", gs), "תורתו"));
        assert!(matches(&walk("ר\"", gs), "ר\"ים")); // abbreviation exempt
    }

    // Otzaria grammatical_prefixes_need_a_root_and_skip_bare_alef (root part).
    #[test]
    fn grammar_prefixes_need_a_root() {
        let gp = WalkOptions { grammar_prefixes: true, ..Default::default() };
        assert!(!matches(&walk("ית", gp), "בית"));
        assert!(matches(&walk("ית", gp), "ית"));
        assert!(matches(&walk("ר\"", gp), "ור\""));
        assert!(matches(&walk("רמב\"ם", gp), "להרמב\"ם"));
    }

    // Otzaria grammatical_morphology_covers_verbs_and_stacked_prefixes (suffix part).
    #[test]
    fn verb_suffixes_for_verb_sized_roots() {
        let both = WalkOptions { grammar_prefixes: true, grammar_suffixes: true, ..Default::default() };
        for form in ["אמרתי", "אמרת", "ואמרתם", "אמרתן", "אמרנו", "ואמרו"] {
            assert!(matches(&walk("אמר", both), form), "{form}");
        }
        assert!(!matches(&walk("ירושלים", both), "ירושלימתי"));
        assert!(!matches(&walk("שבת", both), "שבתתי"));
    }
}
