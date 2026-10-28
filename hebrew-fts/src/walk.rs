//! The Otzaria walk: one configurable automaton covering every search option,
//! driving a single focused pass over tantivy's term dictionary.
//!
//! Structure: [optional prefix phase] → core word variants → [optional suffix
//! phase], where each phase is either a closed trie (grammatical
//! prefixes/suffixes, incl. Aramaic) or a counted any-letter run (advanced /
//! part-of-word). Core variants come from the enabled generators (ktiv
//! מלא/חסר, Aramaic endings); the typo option adds edit states to the core
//! phase. The walk visits only branches a candidate shape allows; the index
//! itself verifies existence.
//!
//! Options (per the Otzaria spec):
//! - חלק ממילה: word inside a larger word, ≤3 any-letters per side (≤2 when
//!   the word has ≥4 letters). ספר ↔ מספרים.
//! - כתיב מלא/חסר: Otzaria's י/ו rules (`variants::ktiv_male_haser_variants_limited`):
//!   מצות ↔ מצוות but not מצת, אותו ↛ אות, שבת ↛ שבות (≤32 variants, 16
//!   with grammatical options).
//! - קידומות דקדוקיות: Otzaria's three optional slots (`prefix_group`):
//!   one of ו מ דא כש כ ב ש ל ה ד, then one of כש כ ב ש ל ה ד, then ה
//!   (והתורה, ושבכשה…); with Aramaic the first slot also takes א
//!   (ודמלכתא). A root under 3 letters takes none (ית ↛ בית) unless an
//!   abbreviation (ר' → ור'). Composes with the advanced prefix window
//!   (Otzaria's lead is the union of both).
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
//! - שגיאות כתיב (Otzaria's typo option, `generate_typo_variations` /
//!   its Levenshtein-1 scan): a typed word of ≥4 letters (quotes and marks
//!   not counted) takes edit distance 1 — one substitution, deletion,
//!   insertion or adjacent transposition — as edit states of this walk;
//!   a 3-letter word only Otzaria's letter-confusion substitutions
//!   (`typo_substitutions`); ≤2 letters nothing.

use std::collections::BTreeSet;
use std::sync::Arc;

use tantivy::query::{
    EnableScoring, MatchedTermsSink, Query, SpanTermSource, Weight,
};
use tantivy::schema::Field;
use tantivy_fst::Automaton;

use crate::quote_free::QuoteFree;
use crate::set_automaton::SetAutomaton;
use crate::variants;

/// A word with ≤ this many letters counts as "short" (raises the advanced
/// prefix/suffix allowances).
pub const SHORT_WORD_MAX_LETTERS: usize = 3;

/// Which rule set the ktiv male / haser option uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KtivRules {
    /// Otzaria's vav / yod rules (`variants::ktiv_male_haser_variants_limited`).
    #[default]
    Otzaria,
    /// FtsLib's `KetivExpander` rules (`ftslib_ketiv::ftslib_ketiv_variants`,
    /// cap 40): the word and its variants as core forms.
    FtsLib,
}

/// How the typo option (שגיאות כתיב) expands a word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TypoMode {
    /// Exactly Otzaria (default): edit distance 1 (its Levenshtein-1 scan)
    /// when the word has ≥4 letters, typo is its only expansion option and
    /// it is the query's only word; otherwise Otzaria's sampled list
    /// (`generate_typo_variations`: confusions, transpositions, deletions,
    /// insertions of 13 letters; 3 letters: confusions only; cap 128, 64 in
    /// a phrase), composed with the other options.
    #[default]
    Otzaria,
    /// Edit distance 1 for every word of ≥4 letters in every combination
    /// (confusions only for 3 letters, nothing for ≤2).
    FullEdit1,
}

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
    /// Which rules [`Self::ktiv_male_haser`] uses.
    pub ktiv_rules: KtivRules,
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
    /// שגיאות כתיב (Otzaria's typo option), expanded per [`TypoMode`].
    pub spelling_errors: bool,
    /// Which typo behaviour [`Self::spelling_errors`] uses.
    pub typo_mode: TypoMode,
    /// The word is one of several in the query (Otzaria's phrase budget,
    /// and no Levenshtein scan in [`TypoMode::Otzaria`]). Set by the engine.
    pub phrase_word: bool,
    /// Otzaria's `match_nikud` (its nikud option): every nikud mark typed in the word
    /// (vowels, dagesh, rafe, shin / sin dots, qamatz qatan) is required on
    /// its letter; marks not typed are free. Off (default): typed nikud is
    /// dropped and every mark is free. Matters on an index that keeps marks.
    pub match_nikud: bool,
    /// Otzaria's `match_taamim` (its te'amim option): the same for te'amim (cantillation,
    /// meteg, upper / lower dots), independently of nikud.
    pub match_teamim: bool,
    /// Set by the engine: the index keeps marks in its terms (its
    /// `keep_marks` layout). The walk then reads every mark as zero width
    /// ([`crate::marks::MarkFree`]) unless the word requires typed marks.
    pub marked_terms: bool,

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
    /// קידומות דקדוקיות: optional cap on the letters of a stacked prefix.
    /// None (default) = Otzaria's full three-slot set (up to 5 letters).
    pub grammar_prefix_max_letters: Option<u32>,
}

/// First slot of the grammatical prefix (Otzaria `GRAM_PREFIX_ALTS_A`).
const GRAM_PREFIX_A: [&str; 10] = ["ו", "מ", "דא", "כש", "כ", "ב", "ש", "ל", "ה", "ד"];
/// First slot under Aramaic prefixes (Otzaria `ARAMAIC_PREFIX_ALTS_A`).
const ARAMAIC_PREFIX_A: [&str; 11] = ["ו", "מ", "דא", "א", "כש", "כ", "ב", "ש", "ל", "ה", "ד"];
/// Second slot (Otzaria `GRAM_PREFIX_ALTS_B`).
const GRAM_PREFIX_B: [&str; 7] = ["כש", "כ", "ב", "ש", "ל", "ה", "ד"];
/// Last slot, the definite article (Otzaria `HE_ALTS`).
const HE_PREFIX: [&str; 1] = ["ה"];

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

/// The closed prefix set: Otzaria's `prefix_group`, three optional slots
/// `(A)?(B)?(ה)?` with A = [`GRAM_PREFIX_A`] (or [`ARAMAIC_PREFIX_A`]),
/// B = [`GRAM_PREFIX_B`]. The bare (empty) prefix is the walk's direct path
/// to the core. `max_letters`: the caller's optional cap.
fn closed_prefixes(aramaic: bool, max_letters: Option<usize>) -> BTreeSet<String> {
    let first: &[&str] = if aramaic { &ARAMAIC_PREFIX_A } else { &GRAM_PREFIX_A };
    let slot = |alts: &'static [&'static str]| std::iter::once("").chain(alts.iter().copied());
    let mut out = BTreeSet::new();
    for a in std::iter::once("").chain(first.iter().copied()) {
        for b in slot(&GRAM_PREFIX_B) {
            for h in slot(&HE_PREFIX) {
                let p = format!("{a}{b}{h}");
                if !p.is_empty() && max_letters.is_none_or(|m| p.chars().count() <= m) {
                    out.insert(p);
                }
            }
        }
    }
    out
}

/// Otzaria's typo tolerance by the typed word's length
/// (`MIN_TYPO_SUBSTITUTION_LETTERS` / `MIN_TYPO_EDIT_LETTERS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Typo {
    Off,
    /// 3 letters: one [`typo_substitutions`] substitution.
    Confusions,
    /// ≥4 letters: edit distance 1 (substitution, deletion, insertion,
    /// adjacent transposition; Otzaria's `FuzzyTermQuery(.., 1, true)`).
    Full,
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
        .filter(|&c| !(options.ignore_quotes && crate::chars::is_term_quote(c)))
        .collect();
    // Vocalized matching (typed marks required): the variants come from the
    // letters; the typed form itself keeps its marks (Otzaria's
    // `vocalized_token_pattern`), every other variant changes letters and
    // matches with free marks (its `vocalized_free_pattern`).
    let stripped = crate::chars::strip_term_marks(&word);
    if stripped.len() != word.len() {
        let mut set = core_variants(&stripped, options);
        set.remove(stripped.as_ref());
        set.insert(word);
        return set;
    }
    let mut set = BTreeSet::new();
    // Otzaria's order: the typo roots first, each through the other options.
    match typo_plan(&word, options) {
        TypoPlan::List(cap) => set.extend(variants::otzaria_typo_variations(&word, cap)),
        _ => {
            set.insert(word);
        }
    }
    if options.aramaic_suffixes {
        set = variants::expand_set(&set, variants::aramaic_ending_variants);
    }
    if options.ktiv_male_haser && options.ktiv_rules == KtivRules::FtsLib {
        // FtsLib's ExpandGroup: the word, then KetivExpander.Expand(word).
        set = variants::expand_set(&set, |w| {
            let mut forms: BTreeSet<String> = crate::ftslib_ketiv::ftslib_ketiv_variants(
                w,
                crate::ftslib_ketiv::FTSLIB_KTIV_MAX_VARIANTS,
            )
            .into_iter()
            .collect();
            forms.insert(w.to_string());
            forms
        });
    } else if options.ktiv_male_haser {
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

/// How the typo option expands this word.
enum TypoPlan {
    Off,
    /// Edit states in the walk's core phase.
    Automaton(Typo),
    /// Otzaria's sampled list (cap) as core forms of the walk's trie.
    List(usize),
}

/// Otzaria `WordFlags::expands_beyond_typo`.
fn expands_beyond_typo(o: &WalkOptions) -> bool {
    o.advanced_prefixes
        || o.advanced_suffixes
        || o.grammar_prefixes
        || o.grammar_suffixes
        || o.ktiv_male_haser
        || o.part_of_word
        || o.aramaic_prefixes
        || o.aramaic_suffixes
}

fn typo_plan(word: &str, options: &WalkOptions) -> TypoPlan {
    if !options.spelling_errors {
        return TypoPlan::Off;
    }
    let letters = variants::typo_letter_count(word);
    match options.typo_mode {
        TypoMode::FullEdit1 => match letters {
            0..=2 => TypoPlan::Off,
            3 => TypoPlan::Automaton(Typo::Confusions),
            _ => TypoPlan::Automaton(Typo::Full),
        },
        TypoMode::Otzaria => {
            if letters < 3 {
                TypoPlan::Off
            } else if letters >= 4 && !options.phrase_word && !expands_beyond_typo(options) {
                TypoPlan::Automaton(Typo::Full)
            } else {
                // SINGLE_WORD_BUDGET / PHRASE_BUDGET `typo_variations`.
                TypoPlan::List(if options.phrase_word { 64 } else { 128 })
            }
        }
    }
}

// ── Composite automaton ──────────────────────────────────────────────────

/// NFA configuration, packed: tag(3) | pending-kind(3) | value(32).
/// Tags: 0 = closed-prefix trie node, 1 = any-prefix letter count,
/// 2 = core trie node, 3 = closed-suffix trie node, 4 = any-suffix count.
/// Pending kinds: 0 = none; 1 = saw a Hebrew letter lead byte (0xD7) in an
/// any-letter phase; 2 = saw a possible mark lead 0xD6 (skipping a
/// nikud/ta'am); 3 = saw a possible 0xD7-encoded nikud (shin/sin dots,
/// qamatz qatan).
pub type Config = u64;

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
/// Vocalized matching: read the lead byte (data bits) of a possible free
/// mark; its tail decides.
const PEND_MARK: u64 = 2;
/// Typo edit in the core: read the lead byte (kept in the data bits) of a
/// two-byte character.
const PEND_EDIT_LEAD: u64 = 4;
/// Typo transposition: the swapped-out character (data bits, as a
/// [`char_key`]) must come next, then the walk resumes at the value node.
const PEND_OWE: u64 = 5;
/// The same, its lead byte already read: the tail byte (data bits) is owed.
const PEND_OWE_TAIL: u64 = 6;

/// A core config that has used its one typo edit (bit 39).
const EDITED: u64 = 1 << 39;
/// Data bits of the typo pending kinds (bits 40..56).
const DATA_SHIFT: u32 = 40;
/// Vocalized matching with a typo edit (bit 56): a core config that passed
/// one of the typed form's required marks without reading it. Such a path
/// matches only once edited (Otzaria matches its typo variants with free
/// marks); unedited it is the typed form, whose marks are required.
const FREE: u64 = 1 << 56;
/// The bits a core config carries through every step.
const CORE_BITS: u64 = PREFIXED | EDITED | FREE;

#[inline]
fn data(c: Config) -> u16 {
    (c >> DATA_SHIFT) as u16
}
#[inline]
fn with_data(c: Config, d: u16) -> Config {
    c | ((d as u64) << DATA_SHIFT)
}

/// A one- or two-byte UTF-8 character as one number (lead << 8 | tail).
#[inline]
fn char_key(lead: u8, tail: Option<u8>) -> u16 {
    match tail {
        None => lead as u16,
        Some(tail) => ((lead as u16) << 8) | tail as u16,
    }
}
#[inline]
fn is_two_byte_lead(b: u8) -> bool {
    (0xC2..=0xDF).contains(&b)
}
#[inline]
fn is_continuation(b: u8) -> bool {
    (0x80..=0xBF).contains(&b)
}
fn key_char(key: u16) -> Option<char> {
    let bytes = if key < 0x80 {
        vec![key as u8]
    } else {
        vec![(key >> 8) as u8, key as u8]
    };
    std::str::from_utf8(&bytes).ok()?.chars().next()
}

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

/// Otzaria `AFFIX_LETTER` (`hebrew_query.rs`), the single-byte part: a
/// geresh / gershayim in the index form (`'`, `"`). An any-letter window
/// adds only these and Hebrew letters ([`is_affix_tail`]): never a Latin
/// letter, a digit or another script.
#[inline]
fn is_affix_quote(b: u8) -> bool {
    b == b'\'' || b == b'"'
}
/// Lead byte of a UTF-8 Hebrew letter (U+05D0..U+05EA = 0xD7 0x90..0xAA,
/// U+05F0..U+05F2 = 0xD7 0xB0..0xB2).
const HEBREW_LEAD: u8 = 0xD7;
/// Otzaria `AFFIX_LETTER`, the two-byte part (after [`HEBREW_LEAD`]): the
/// Hebrew letters U+05D0..U+05EA and the Yiddish ligatures U+05F0..U+05F2.
#[inline]
fn is_affix_tail(b: u8) -> bool {
    (0x90..=0xAA).contains(&b) || (0xB0..=0xB2).contains(&b)
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
    /// שגיאות כתיב: the one edit the core may take.
    typo: Typo,
    /// No phase, edit or transparency besides the core trie: walk the trie
    /// alone ([`WalkState::Trie`]).
    trie_only: bool,
    transparent_quotes: bool,
    /// Vocalized matching: the word carries required marks (its typed form
    /// in the core trie keeps them) and every mark of the term the trie does
    /// not ask for is free (a zero-width self-loop, in every phase).
    free_marks: bool,
}

/// The marks of `word` a vocalized search requires: the typed nikud when
/// `match_nikud`, the typed te'amim when `match_teamim` (Otzaria's
/// `VocalizedFlags::requires`); every other mark is dropped (Otzaria drops a
/// typed mark of a disabled class and never requires a general combining
/// mark U+0300..U+036F). The word keeps its letters and quotes, its marks
/// in canonical order.
pub fn required_marks_form(word: &str, match_nikud: bool, match_teamim: bool) -> String {
    // the index keeps marks in canonical order; typed input may not be
    crate::chars::canonical_marks(word)
        .chars()
        .filter(|&c| {
            !crate::chars::is_term_mark(c)
                || (match_nikud && crate::chars::is_nikud(c))
                || (match_teamim && crate::chars::is_taam(c))
        })
        .collect()
}

impl HebrewWalkAutomaton {
    /// Builds the automaton for `word` under `options`. `word` must already be
    /// normalized the way the index normalizes (lowercase, canonical '"',
    /// marks in canonical order). Marks left in `word` are REQUIRED on their
    /// letters (vocalized matching; see [`required_marks_form`]) and every
    /// other mark of a term is free. A word without marks matches mark-free
    /// terms only: on an index that keeps marks, wrap it in
    /// [`MarkFree`](crate::marks::MarkFree) ([`HebrewWalkQuery`] does).
    pub fn new(word: &str, options: &WalkOptions) -> HebrewWalkAutomaton {
        let free_marks = word.chars().any(crate::chars::is_term_mark);
        let letters = word.chars().filter(|&c| crate::chars::starts_word(c)).count(); // letters and digits, marks not
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
        let grammar_prefix_max = options.grammar_prefix_max_letters.map(|m| m as usize);
        let typo = match typo_plan(word, options) {
            TypoPlan::Automaton(t) => t,
            TypoPlan::Off | TypoPlan::List(_) => Typo::Off,
        };

        // Otzaria's lead is the union of the prefix window and the
        // grammatical prefix group (`affix_arms`), so both may run.
        let prefixes = if grammar_prefixes {
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

        let prefixes_none = prefixes.is_none();
        HebrewWalkAutomaton {
            core: SetAutomaton::from_tagged(tagged),
            prefixes,
            any_prefix_max: any_pre,
            suffixes,
            verb_suffixes,
            any_suffix_max: any_post,
            gate_prefixes: !options.aramaic_prefixes,
            typo,
            trie_only: prefixes_none
                && any_pre == 0
                && !options.grammar_suffixes
                && any_post == 0
                && typo == Typo::Off
                && !free_marks
                && !options.ignore_quotes,
            transparent_quotes: options.ignore_quotes,
            free_marks,
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
                // Typo deletion: skip one character of the core form.
                if self.typo == Typo::Full && config & EDITED == 0 {
                    let gate = config & PREFIXED;
                    self.for_each_child_char(value(config), |_, target| {
                        self.close_into(pack(TAG_CORE, PEND_NONE, target) | gate | EDITED, set);
                    });
                }
                // Vocalized matching with typos: a required mark of the
                // typed form may be passed unread (FREE), for an edited path
                // (letters changed: its marks are free).
                if self.free_marks && self.typo != Typo::Off {
                    let bits = config & CORE_BITS;
                    self.for_each_child_mark(value(config), |target| {
                        self.close_into(pack(TAG_CORE, PEND_NONE, target) | bits | FREE, set);
                    });
                }
                if config & FREE != 0 && config & EDITED == 0 {
                    // unedited and missing a required mark: no suffix may follow
                    return;
                }
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

    /// Calls `f(char_key, node)` for every one- or two-byte character the
    /// core trie spells from `node`.
    fn for_each_child_char(&self, node: u32, mut f: impl FnMut(u16, u32)) {
        for &(b, t) in self.core.edges(node) {
            if b < 0x80 {
                f(char_key(b, None), t);
            } else if is_two_byte_lead(b) {
                for &(b2, t2) in self.core.edges(t) {
                    // a typed (required) mark is not a character to edit
                    if !crate::marks::is_mark_char(b, b2) {
                        f(char_key(b, Some(b2)), t2);
                    }
                }
            }
        }
    }

    /// Calls `f(node)` for every mark the core trie spells from `node`.
    fn for_each_child_mark(&self, node: u32, mut f: impl FnMut(u32)) {
        for &(b, t) in self.core.edges(node) {
            if crate::marks::is_mark_lead(b) {
                for &(b2, t2) in self.core.edges(t) {
                    if crate::marks::is_mark_char(b, b2) {
                        f(t2);
                    }
                }
            }
        }
    }

    /// The typo edits once the term has shown a whole character `read` at
    /// the unedited core config `config`: insertion (the character is extra),
    /// substitution (it replaces one of the form's), transposition (it is
    /// the form's next-but-one, swapped with the next).
    fn typo_after_char(&self, config: Config, read: u16, out: &mut Vec<Config>) {
        let node = value(config);
        let gate = config & PREFIXED;
        match self.typo {
            Typo::Off => {}
            Typo::Confusions => {
                let Some(r) = key_char(read) else { return };
                self.for_each_child_char(node, |k, target| {
                    if key_char(k).is_some_and(|k| variants::typo_substitutions(k).contains(&r)) {
                        self.close_into(pack(TAG_CORE, PEND_NONE, target) | gate | EDITED, out);
                    }
                });
            }
            Typo::Full => {
                self.close_into(pack(TAG_CORE, PEND_NONE, node) | gate | EDITED, out);
                self.for_each_child_char(node, |k, target| {
                    if k == read {
                        return;
                    }
                    self.close_into(pack(TAG_CORE, PEND_NONE, target) | gate | EDITED, out);
                    let mut after = None;
                    self.for_each_child_char(target, |k2, t2| {
                        if k2 == read {
                            after = Some(t2);
                        }
                    });
                    if let Some(t2) = after {
                        let owe = with_data(pack(TAG_CORE, PEND_OWE, t2) | gate | EDITED, k);
                        if !out.contains(&owe) {
                            out.push(owe);
                        }
                    }
                });
            }
        }
    }

    fn step_config(&self, config: Config, byte: u8, out: &mut Vec<Config>) {
        let t = tag(config);
        // The prefix gate, the used typo edit and a passed required mark
        // travel with the core configs.
        let gate = config & CORE_BITS;
        match pending(config) {
            PEND_EDIT_LEAD => {
                // a free mark is never a typo edit (Otzaria counts letters)
                if is_continuation(byte) && !(self.free_marks && crate::marks::is_mark_char(data(config) as u8, byte)) {
                    let base = pack(TAG_CORE, PEND_NONE, value(config)) | gate;
                    self.typo_after_char(base, char_key(data(config) as u8, Some(byte)), out);
                }
                return;
            }
            PEND_OWE => {
                let owed = data(config);
                let resume = pack(TAG_CORE, PEND_NONE, value(config)) | gate | EDITED;
                if owed < 0x80 {
                    if byte as u16 == owed {
                        self.close_into(resume, out);
                    }
                } else if byte as u16 == owed >> 8 {
                    let next = with_data(
                        pack(TAG_CORE, PEND_OWE_TAIL, value(config)) | gate | EDITED,
                        owed & 0xFF,
                    );
                    if !out.contains(&next) {
                        out.push(next);
                    }
                }
                return;
            }
            PEND_OWE_TAIL => {
                if byte as u16 == data(config) {
                    self.close_into(pack(TAG_CORE, PEND_NONE, value(config)) | gate | EDITED, out);
                }
                return;
            }
            // Resolving a possible free mark: <lead> <tail>.
            PEND_MARK => {
                if crate::marks::is_mark_char(data(config) as u8, byte) {
                    // Mark skipped; back to where we were.
                    self.close_into(pack(t, PEND_NONE, value(config)) | gate, out);
                }
                return;
            }
            // Second byte of a Hebrew letter in an any-letter phase.
            PEND_LETTER => {
                if is_affix_tail(byte) {
                    self.close_into(pack(t, PEND_NONE, value(config) + 1), out);
                }
                return;
            }
            _ => {}
        }

        // Free marks apply in every phase once a letter was read: a mark may begin here (the core
        // trie may also read it: a typed, required mark). A free mark
        // follows a letter: never where no letter has been read yet (the
        // start of a phase that begins the term, the window before the root
        // before its first letter, the core root). Otzaria's vocalized
        // window is `[0-9]*(?:letter marks*)*`, so a mark never sits before,
        // between or right after the glued footnote digits. A letter that
        // was read (a prefix, a window letter) keeps its marks through its
        // own phase, which then closes into the core again.
        let no_letter_yet = match t {
            TAG_PRE_ANY => value(config) == 0,
            TAG_PRE => value(config) == SetAutomaton::ROOT,
            TAG_CORE => value(config) == SetAutomaton::ROOT && config & EDITED == 0,
            _ => false,
        };
        if self.free_marks && !no_letter_yet && crate::marks::is_mark_lead(byte) {
            let pend = with_data(pack(t, PEND_MARK, value(config)) | gate, byte as u16);
            if !out.contains(&pend) {
                out.push(pend);
            }
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
                    if is_affix_quote(byte) {
                        self.close_into(pack(t, PEND_NONE, count + 1), out);
                    } else if byte == HEBREW_LEAD {
                        self.close_into(pack(t, PEND_LETTER, count), out);
                    }
                }
                // Otzaria `NOTE_DIGITS` (`[0-9]*` before the window's
                // letters): a footnote number glued to the word, only in the
                // window before the root, free of the letter budget.
                if t == TAG_PRE_ANY && count == 0 && byte.is_ascii_digit() {
                    self.close_into(pack(t, PEND_NONE, 0), out);
                }
            }
            TAG_CORE => {
                let edited = config & EDITED;
                if let Some(next) = self.core.step(value(config), byte) {
                    self.close_into(pack(TAG_CORE, PEND_NONE, next) | gate | edited, out);
                }
                // The typo edits, as walk states: one per core path.
                if self.typo != Typo::Off && edited == 0 {
                    if byte < 0x80 {
                        self.typo_after_char(config, char_key(byte, None), out);
                    } else if is_two_byte_lead(byte) {
                        let lead = with_data(
                            pack(TAG_CORE, PEND_EDIT_LEAD, value(config)) | gate,
                            byte as u16,
                        );
                        if !out.contains(&lead) {
                            out.push(lead);
                        }
                    }
                }
                if self.transparent_quotes && crate::chars::is_term_quote_byte(byte) {
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
            // Zero suffix: the word may simply end here (unless it passed a
            // required mark unread without an edit).
            TAG_CORE => {
                self.core_flags(config) & CORE_BARE != 0 && (config & FREE == 0 || config & EDITED != 0)
            }
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

/// A walk state. A query whose options only add core forms (none, ktiv,
/// Aramaic endings, Otzaria's typo list) walks the core trie alone, as a
/// plain term set: no option checks and no config sets per byte. Every
/// other query walks the composite NFA.
#[derive(Clone, Debug)]
pub enum WalkState {
    /// Core trie node; None = dead.
    Trie(Option<u32>),
    /// Sorted NFA config set; empty = dead.
    Nfa(Vec<Config>),
}

impl Automaton for HebrewWalkAutomaton {
    type State = WalkState;

    fn start(&self) -> WalkState {
        if self.trie_only {
            WalkState::Trie(Some(SetAutomaton::ROOT))
        } else {
            WalkState::Nfa(self.start_configs())
        }
    }

    fn is_match(&self, state: &WalkState) -> bool {
        match state {
            WalkState::Trie(node) => node.is_some_and(|n| self.core.node_flags(n) & CORE_BARE != 0),
            WalkState::Nfa(configs) => configs.iter().any(|&c| self.config_is_match(c)),
        }
    }

    fn accept(&self, state: &WalkState, byte: u8) -> WalkState {
        match state {
            WalkState::Trie(node) => WalkState::Trie(node.and_then(|n| self.core.step(n, byte))),
            WalkState::Nfa(configs) => {
                let mut out = Vec::with_capacity(configs.len());
                for &config in configs {
                    self.step_config(config, byte, &mut out);
                }
                out.sort_unstable();
                out.dedup();
                WalkState::Nfa(out)
            }
        }
    }

    fn can_match(&self, state: &WalkState) -> bool {
        match state {
            WalkState::Trie(node) => node.is_some(),
            WalkState::Nfa(configs) => !configs.is_empty(),
        }
    }

    fn will_always_match(&self, _state: &WalkState) -> bool {
        false
    }
}

/// FtsLib's cap on how many index terms one query word may expand to
/// (`HebrewWildcardExpander.DefaultMaxExpandedTerms`,
/// `FuzzyExpander.DefaultMaxExpandedTerms`).
pub const DEFAULT_MAX_EXPANDED_TERMS: usize = 2000;

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

    /// The word's automaton, and whether it must read marks as zero width
    /// ([`MarkFree`](crate::marks::MarkFree)): on an index that keeps marks,
    /// a word with no required mark (see [`required_marks_form`]) matches
    /// every vocalization, exactly what the mark-stripped index matches; a
    /// word with required marks walks them itself (vocalized matching).
    fn automaton(&self) -> (QuoteFree<HebrewWalkAutomaton>, bool) {
        let o = &self.options;
        let typed = required_marks_form(&self.word, o.match_nikud, o.match_teamim);
        let required = typed.chars().any(crate::chars::is_term_mark);
        let automaton = QuoteFree::new(HebrewWalkAutomaton::new(&typed, o), !o.exact_quotes);
        (automaton, o.marked_terms && !required)
    }

    fn plan(&self, mark_free: bool) -> crate::marks::WeightPlan<'_> {
        crate::marks::WeightPlan {
            field: self.field,
            sink: self.matched_terms_sink.as_ref(),
            cap: Some(self.max_expansions),
            marked_terms: self.options.marked_terms,
            mark_free,
        }
    }

    /// Whether the walk matches the dictionary term `term` (tests, checks).
    pub fn accepts(&self, term: &[u8]) -> bool {
        let (automaton, mark_free) = self.automaton();
        if mark_free {
            run(&crate::marks::MarkFree::new(automaton), term)
        } else {
            run(&automaton, term)
        }
    }

    /// This walk as a slot of a [`SpanQuery`](tantivy::query::SpanQuery):
    /// "any word this walk matches", with positions. The sink, if set, still
    /// collects the matched forms. The expansion cap applies per
    /// segment.
    pub fn span_slot(&self, _searcher: &tantivy::Searcher) -> tantivy::Result<Arc<dyn SpanTermSource>> {
        let (automaton, mark_free) = self.automaton();
        Ok(self.plan(mark_free).span(automaton))
    }
}

/// Runs `automaton` over `term`.
pub(crate) fn run<A: Automaton>(automaton: &A, term: &[u8]) -> bool {
    let mut state = automaton.start();
    for &b in term {
        if !automaton.can_match(&state) {
            return false;
        }
        state = automaton.accept(&state, b);
    }
    automaton.is_match(&state)
}

impl Query for HebrewWalkQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let (automaton, mark_free) = self.automaton();
        Ok(self.plan(mark_free).boxed(automaton))
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
    fn core_only_options_walk_the_trie_alone() {
        for o in [
            WalkOptions::default(),
            WalkOptions { ktiv_male_haser: true, aramaic_suffixes: true, ..Default::default() },
            WalkOptions { spelling_errors: true, phrase_word: true, ..Default::default() },
        ] {
            assert!(matches!(walk("תורה", o).start(), WalkState::Trie(_)), "{o:?}");
        }
        for o in [
            WalkOptions { grammar_prefixes: true, ..Default::default() },
            WalkOptions { ignore_quotes: true, ..Default::default() },
            WalkOptions { spelling_errors: true, ..Default::default() }, // lone word: edit states
        ] {
            assert!(matches!(walk("תורה", o).start(), WalkState::Nfa(_)), "{o:?}");
        }
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
        assert!(matches(&a, "דתורה")); // ד is in Otzaria's group
        assert!(!matches(&a, "אתורה")); // bare א only with Aramaic
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
    fn affix_windows_add_only_hebrew_letters_and_quotes() {
        // Otzaria hebrew_query.rs affix_windows_add_only_hebrew_letters_and_quotes
        const SHABBAT: &str = "\u{05E9}\u{05D1}\u{05EA}";
        let prefix = WalkOptions { advanced_prefixes: true, ..Default::default() };
        let suffix = WalkOptions { advanced_suffixes: true, ..Default::default() };
        let partial = WalkOptions { part_of_word: true, ..Default::default() };
        for (o, is_suffix) in [(&prefix, false), (&suffix, true), (&partial, false)] {
            let a = walk(SHABBAT, o.clone());
            assert!(
                matches(&a, &format!("\u{05D1}{SHABBAT}")) || matches(&a, &format!("{SHABBAT}\u{05D5}"))
            );
            for foreign in [
                format!("a{SHABBAT}"),
                format!("{SHABBAT}1"),
                format!("{SHABBAT}x"),
                format!("\u{0301}{SHABBAT}"),
                format!("a1{SHABBAT}"),
            ] {
                assert!(!matches(&a, &foreign), "accepted {}", foreign.escape_unicode());
            }
            // a footnote number glued before the word: only the window before the root
            assert_eq!(matches(&a, &format!("28{SHABBAT}")), !is_suffix);
        }
        let p = |w: &str, o: &WalkOptions, t: &str| matches(&walk(w, o.clone()), t);
        assert!(p("\u{05E8}\u{05DE}\u{05D1}", &partial, "\u{05D4}\u{05E8}\u{05DE}\u{05D1}\"\u{05DD}"));
        assert!(p(SHABBAT, &partial, "\u{05DE}\u{05D7}\u{05E9}\u{05D1}\u{05EA}\u{05D5}"));
        assert!(p("\u{05D1}\u{05E8}\u{05DB}", &partial, "448\u{05D1}\u{05E8}\u{05DB}\u{05D5}\u{05EA}"));
        let amar = "\u{05D0}\u{05DE}\u{05E8}";
        assert!(p(amar, &prefix, &format!("70\u{05D5}{amar}")));
        assert!(!p(amar, &prefix, &format!("7a0{amar}")));
        // digits only before the window's letters, never after one
        assert!(!p(amar, &prefix, &format!("\u{05D5}7{amar}")));
        // grammatical prefixes take no digits
        let gram = WalkOptions { grammar_prefixes: true, ..Default::default() };
        assert!(!p(SHABBAT, &gram, &format!("28{SHABBAT}")));
        // the Yiddish ligatures U+05F0..U+05F2 are affix letters
        assert!(p(SHABBAT, &prefix, &format!("\u{05F0}{SHABBAT}")));
        assert!(p(SHABBAT, &suffix, &format!("{SHABBAT}\u{05F2}")));
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

    fn typo() -> WalkOptions {
        WalkOptions {
            spelling_errors: true,
            typo_mode: TypoMode::FullEdit1,
            ..Default::default()
        }
    }

    fn otz(o: WalkOptions) -> WalkOptions {
        WalkOptions { spelling_errors: true, typo_mode: TypoMode::Otzaria, ..o }
    }

    // TypoMode::Otzaria, Otzaria's typo_tolerance_scales_with_word_length
    // and advanced_typo_only_single_word_covers_full_edit_distance: a lone
    // 4+-letter word with typo only gets the Levenshtein-1 scan.
    #[test]
    fn otzaria_typo_lone_word_is_edit_distance_1() {
        let a = walk("ברכה", otz(WalkOptions::default()));
        for v in ["ברכה", "ברכ", "רבכה", "בברכה", "ברקה", "ברכגה", "ברזה"] {
            assert!(matches(&a, v), "{v}");
        }
        assert!(!matches(&a, "תורה"));
        let a = walk("שבת", otz(WalkOptions::default()));
        for v in ["שבת", "סבת", "שות", "שבט"] {
            assert!(matches(&a, v), "{v}");
        }
        for v in ["שב", "שבתי", "בשת", "ששבת"] {
            assert!(!matches(&a, v), "{v}");
        }
        assert!(!matches(&walk("אב", otz(WalkOptions::default())), "עב"));
    }

    // TypoMode::Otzaria: with another option, or inside a phrase, the
    // sampled list (generate_typo_variations) replaces the scan.
    #[test]
    fn otzaria_typo_with_options_uses_the_sampled_list() {
        let a = walk("ברכה", otz(WalkOptions { grammar_prefixes: true, ..Default::default() }));
        for v in ["והברקה", "ברכ", "רבכה", "בברכה", "וברכה"] {
            assert!(matches(&a, v), "{v}");
        }
        assert!(!matches(&a, "ברכגה")); // ג is not an insertion letter
        assert!(!matches(&a, "ברזה")); // כ↔ז is not a confusion
        let a = walk("ברכה", otz(WalkOptions { phrase_word: true, ..Default::default() }));
        assert!(matches(&a, "ברקה"));
        assert!(!matches(&a, "ברכגה"));
        // Otzaria advanced_typo_partial_single_word_finds_document.
        let a = walk("משה", otz(WalkOptions { part_of_word: true, ..Default::default() }));
        assert!(matches(&a, "משה"));
        assert!(matches(&a, "ממשה"));
    }

    // Otzaria typo_tolerance_scales_with_word_length.
    #[test]
    fn typo_tolerance_scales_with_word_length() {
        // ≤2 letters: no typo at all.
        let a = walk("אב", typo());
        assert!(matches(&a, "אב"));
        for v in ["עב", "אבג", "א", "בא", "אה"] {
            assert!(!matches(&a, v), "{v}");
        }
        // 3 letters: only the confusion-table substitutions.
        let a = walk("שבת", typo());
        for v in ["שבת", "סבת", "שות", "שבט"] {
            assert!(matches(&a, v), "{v}");
        }
        for v in ["שב", "שבתי", "בשת", "ששבת", "שבר"] {
            assert!(!matches(&a, v), "{v}");
        }
        // ≥4 letters: full edit distance 1.
        let a = walk("ברכה", typo());
        for v in ["ברכה", "ברכ", "רבכה", "בברכה", "ברקה", "ברכגה", "ברזה"] {
            assert!(matches(&a, v), "{v}");
        }
        for v in ["רבקה", "בר", "ברכהות", "הברכות"] {
            assert!(!matches(&a, v), "{v}");
        }
        // Quotes are not counted as letters (ר"ת: 2 letters, no typo).
        let a = walk("ר\"ת", typo());
        assert!(!matches(&a, "ר\"ט"));
    }

    // Otzaria common_typo_covers_substitution_and_transposition (on a
    // 4-letter word; Otzaria's length rule gives a 2-letter word none).
    #[test]
    fn typo_edit_kinds() {
        let a = walk("משפט", typo());
        assert!(matches(&a, "מספט")); // substitution
        assert!(matches(&a, "שמפט")); // transposition
        assert!(matches(&a, "משפטt")); // insertion of a one-byte char
        assert!(matches(&a, "מפט")); // deletion
        assert!(matches(&a, "משפ\"ט")); // the quote counts as a char
        assert!(!matches(&a, "שמפת")); // two edits
        assert!(!matches(&a, "ממשפטי")); // two insertions
    }

    // Otzaria advanced_typo_only_single_word_covers_full_edit_distance.
    #[test]
    fn typo_covers_any_insertion_letter() {
        let a = walk("ברכה", typo());
        assert!(matches(&a, "ברכגה")); // ג is not in INSERTION_LETTERS
        assert!(!matches(&a, "תורה"));
    }

    #[test]
    fn typo_composes_with_affixes() {
        let a = walk(
            "ברכה",
            WalkOptions {
                spelling_errors: true,
                typo_mode: TypoMode::FullEdit1,
                grammar_prefixes: true,
                grammar_suffixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "והברקה")); // prefix + substituted core
        assert!(matches(&a, "וברכות")); // exact core + suffix
        assert!(matches(&a, "ברקתו")); // edited core + suffix
        assert!(!matches(&a, "רבקתו")); // two edits
        // An edit never spends inside a mark: on an index that keeps marks
        // they are zero width, one edit stays one edit.
        let q = marked(
            "\u{05D1}\u{05E8}\u{05DB}\u{05D4}",
            WalkOptions { spelling_errors: true, typo_mode: TypoMode::FullEdit1, ..Default::default() },
        );
        assert!(q.accepts("\u{05D1}\u{05B0}\u{05BC}\u{05E8}\u{05B8}\u{05E7}\u{05B8}\u{05D4}".as_bytes()));
        assert!(!q.accepts("\u{05D1}\u{05B0}\u{05BC}\u{05E8}\u{05B8}\u{05E7}\u{05B8}\u{05D7}".as_bytes()));
    }

    // Otzaria grammatical_prefix_uses_full_rich_group:
    // (?:ו|מ|דא|כש|כ|ב|ש|ל|ה|ד)?(?:כש|כ|ב|ש|ל|ה|ד)?(?:ה)?ספר
    #[test]
    fn grammar_prefix_group_is_otzarias() {
        let set = closed_prefixes(false, None);
        assert!(set.contains("דאכשה")); // 5 letters: A + B + ה
        assert!(set.contains("ההה"));
        assert!(set.contains("שש"));
        assert!(!set.contains("א"));
        assert!(!set.contains("כשו")); // ו only in the first slot
        assert!(closed_prefixes(true, None).contains("א"));
        assert!(closed_prefixes(true, None).contains("אכשה"));
        let gp = WalkOptions { grammar_prefixes: true, ..Default::default() };
        let a = walk("ספר", gp);
        for v in ["ספר", "הספר", "ושהספר", "דאכשהספר", "לספר", "דספר"] {
            assert!(matches(&a, v), "{v}");
        }
        for v in ["אספר", "שוספר", "וווספר", "ושבהספר"] {
            assert!(!matches(&a, v), "{v}");
        }
    }

    // Otzaria grammatical_morphology_covers_verbs_and_stacked_prefixes
    // (prefix part) and grammatical_prefixes_need_a_root_and_skip_bare_alef.
    #[test]
    fn grammar_prefix_stacking_and_alef() {
        let gp = WalkOptions { grammar_prefixes: true, ..Default::default() };
        let a = walk("אמר", gp);
        for v in ["כשאמר", "וכשאמר", "דאמר", "שאמר", "ואמר"] {
            assert!(matches(&a, v), "{v}");
        }
        assert!(!matches(&a, "ושכשאמר"));
        assert!(!matches(&a, "אאמר"));
        assert!(!matches(&walk("שבת", gp), "אשבת"));
        let ar = WalkOptions { aramaic_prefixes: true, ..Default::default() };
        assert!(matches(&walk("שבת", ar), "אשבת"));
    }

    // Otzaria affix_arms: the lead is the union of the prefix window and the
    // grammatical group, so a 5-letter grammatical prefix still matches with
    // a 3-letter window.
    #[test]
    fn grammar_prefixes_compose_with_the_window() {
        let a = walk(
            "תורה",
            WalkOptions {
                grammar_prefixes: true,
                advanced_prefixes: true,
                ..Default::default()
            },
        );
        assert!(matches(&a, "קקקתורה")); // window
        assert!(matches(&a, "וכשהתורה")); // grammatical, 4 letters
        assert!(!matches(&a, "קקקקתורה"));
    }

    /// The walk as the engine runs it on an index that keeps marks.
    fn marked(word: &str, options: WalkOptions) -> HebrewWalkQuery {
        HebrewWalkQuery::new(Field::from_field_id(0), word, WalkOptions { marked_terms: true, ..options })
    }

    fn acc(q: &HebrewWalkQuery, term: &str) -> bool {
        q.accepts(term.as_bytes())
    }

    // bet resh alef shin yod tav, plain and fully vocalized (canonical order)
    const BERESHIT: &str = "\u{05D1}\u{05E8}\u{05D0}\u{05E9}\u{05D9}\u{05EA}";
    const BERESHIT_VOC: &str = "\u{05D1}\u{05B0}\u{05BC}\u{05E8}\u{05B5}\u{05D0}\u{05E9}\u{05B4}\u{05C1}\u{05D9}\u{05EA}";

    #[test]
    fn marks_are_free_unless_matched() {
        // default: every mark is zero width, typed marks are dropped
        let q = marked(BERESHIT, WalkOptions::default());
        assert!(acc(&q, BERESHIT) && acc(&q, BERESHIT_VOC));
        assert!(acc(&q, "\u{05D1}\u{05E8}\u{05D0}\u{0591}\u{05E9}\u{05D9}\u{05EA}\u{0301}"));
        let q = marked(BERESHIT_VOC, WalkOptions::default());
        assert!(acc(&q, BERESHIT) && acc(&q, BERESHIT_VOC));
        // the plain fast path stays the trie (wrapped)
        assert!(matches!(walk(BERESHIT, WalkOptions::default()).start(), WalkState::Trie(_)));
        // a mark-free index: no wrapper, a plain word matches plain terms
        let plain = HebrewWalkQuery::new(Field::from_field_id(0), BERESHIT, WalkOptions::default());
        assert!(acc(&plain, BERESHIT) && !acc(&plain, BERESHIT_VOC));
    }

    #[test]
    fn vocalized_affix_windows() {
        // Otzaria hebrew_query.rs affix_windows_add_only_hebrew_letters_and_quotes,
        // the vocalized part (no required marks: a plain word on a marks index)
        const SHABBAT: &str = "\u{05E9}\u{05D1}\u{05EA}";
        const SHABBAT_VOC: &str = "\u{05E9}\u{05B7}\u{05C1}\u{05D1}\u{05B8}\u{05BC}\u{05EA}";
        let partial = WalkOptions { part_of_word: true, ..Default::default() };
        let q = marked(SHABBAT, partial.clone());
        assert!(acc(&q, &format!("\u{05D1}\u{05B0}\u{05BC}{SHABBAT_VOC}")));
        assert!(acc(&q, &format!("1{SHABBAT_VOC}")));
        assert!(!acc(&q, &format!("a{SHABBAT_VOC}")));
        // free marks on (a typed, required patah on the shin): marks follow an
        // affix letter, never before or between the leading digits
        let typed = "\u{05E9}\u{05B7}\u{05D1}\u{05EA}";
        for o in [partial, WalkOptions { advanced_prefixes: true, ..Default::default() }] {
            let q = marked(typed, WalkOptions { match_nikud: true, ..o });
            assert!(acc(&q, SHABBAT_VOC));
            assert!(acc(&q, &format!("\u{05D1}\u{05B0}\u{05BC}{SHABBAT_VOC}")));
            assert!(acc(&q, &format!("12{SHABBAT_VOC}")));
            assert!(acc(&q, &format!("12\u{05D1}\u{05B0}{SHABBAT_VOC}")));
            assert!(!acc(&q, &format!("a{SHABBAT_VOC}")));
            assert!(!acc(&q, &format!("\u{05B0}12{SHABBAT_VOC}")), "mark before the digits");
            assert!(!acc(&q, &format!("1\u{05B0}2{SHABBAT_VOC}")), "mark between the digits");
            assert!(!acc(&q, &format!("12\u{05B0}{SHABBAT_VOC}")), "mark after the digits, no letter");
            assert!(!acc(&q, &format!("\u{05B0}{SHABBAT_VOC}")), "leading mark");
        }
        // a grammatical prefix letter keeps its marks
        let q = marked(typed, WalkOptions { match_nikud: true, grammar_prefixes: true, ..Default::default() });
        assert!(acc(&q, &format!("\u{05D5}\u{05B0}{SHABBAT_VOC}")));
        assert!(acc(&q, &format!("\u{05D5}\u{05B0}\u{05D1}\u{05B7}\u{05BC}{SHABBAT_VOC}")));
        assert!(!acc(&q, &format!("\u{05B0}\u{05D5}{SHABBAT_VOC}")), "leading mark");
    }

    #[test]
    fn typed_marks_required_untyped_free() {
        // bet + qamats, resh: the qamats is required on the bet
        let typed = "\u{05D1}\u{05B8}\u{05E8}";
        let q = marked(typed, WalkOptions { match_nikud: true, ..Default::default() });
        assert!(acc(&q, typed));
        assert!(acc(&q, "\u{05D1}\u{05B8}\u{05BC}\u{05E8}\u{05B0}")); // untyped dagesh and sheva free
        assert!(acc(&q, "\u{05D1}\u{05B8}\u{0591}\u{05E8}\u{0301}")); // te'amim, combining accent free
        assert!(!acc(&q, "\u{05D1}\u{05E8}")); // typed mark missing
        assert!(!acc(&q, "\u{05D1}\u{05B7}\u{05E8}")); // another vowel
        assert!(!acc(&q, "\u{05D1}\u{05E8}\u{05B8}")); // on another letter
        assert!(!matches!(walk(typed, WalkOptions::default()).start(), WalkState::Trie(_)));
        // two typed marks of one letter, canonical order in term and query
        let q = marked("\u{05D1}\u{05B8}\u{05BC}\u{05E8}", WalkOptions { match_nikud: true, ..Default::default() });
        assert!(acc(&q, "\u{05D1}\u{05B8}\u{05BC}\u{05E8}"));
        assert!(acc(&q, "\u{05D1}\u{05B8}\u{05BC}\u{0591}\u{05E8}"));
        assert!(!acc(&q, "\u{05D1}\u{05B8}\u{05E8}"));
        assert!(!acc(&q, "\u{05D1}\u{05BC}\u{05E8}"));
    }

    #[test]
    fn nikud_and_teamim_are_independent_classes() {
        // a typed ta'am (etnahta) and a typed vowel (qamats)
        let typed = "\u{05D1}\u{05B8}\u{05E8}\u{0591}";
        let nikud = marked(typed, WalkOptions { match_nikud: true, ..Default::default() });
        let teamim = marked(typed, WalkOptions { match_teamim: true, ..Default::default() });
        let both = marked(typed, WalkOptions { match_nikud: true, match_teamim: true, ..Default::default() });
        let vowel_only = "\u{05D1}\u{05B8}\u{05E8}";
        let taam_only = "\u{05D1}\u{05E8}\u{0591}";
        assert!(acc(&nikud, vowel_only) && !acc(&nikud, taam_only));
        assert!(!acc(&teamim, vowel_only) && acc(&teamim, taam_only));
        assert!(acc(&both, typed) && !acc(&both, vowel_only) && !acc(&both, taam_only));
        // Otzaria's classes: meteg (U+05BD) and the upper dot (U+05C4) are te'amim
        let meteg = "\u{05D1}\u{05BD}\u{05E8}";
        assert!(acc(&marked(meteg, WalkOptions { match_nikud: true, ..Default::default() }), "\u{05D1}\u{05E8}"));
        assert!(!acc(&marked(meteg, WalkOptions { match_teamim: true, ..Default::default() }), "\u{05D1}\u{05E8}"));
        assert!(crate::chars::is_taam('\u{05BD}') && crate::chars::is_taam('\u{05C4}') && crate::chars::is_taam('\u{05C5}'));
        assert!(crate::chars::is_nikud('\u{05BC}') && crate::chars::is_nikud('\u{05C7}') && !crate::chars::is_nikud('\u{05BD}'));
    }

    #[test]
    fn required_marks_compose_with_affixes_and_variants() {
        // grammatical prefixes: the prefix letters take any marks
        let typed = "\u{05EA}\u{05BC}\u{05D5}\u{05E8}\u{05D4}"; // dagesh typed on the tav
        let o = WalkOptions { match_nikud: true, grammar_prefixes: true, grammar_suffixes: true, ..Default::default() };
        let q = marked(typed, o);
        assert!(acc(&q, "\u{05D4}\u{05B7}\u{05EA}\u{05BC}\u{05D5}\u{05B9}\u{05E8}\u{05B8}\u{05D4}"));
        assert!(acc(&q, "\u{05D5}\u{05B0}\u{05D4}\u{05B7}\u{05EA}\u{05BC}\u{05D5}\u{05E8}\u{05EA}\u{05D5}\u{05B9}"));
        assert!(!acc(&q, "\u{05D4}\u{05B7}\u{05EA}\u{05D5}\u{05B9}\u{05E8}\u{05B8}\u{05D4}")); // dagesh missing
        // ktiv male/haser: a variant whose letters differ matches with free marks
        let q = marked(typed, WalkOptions { match_nikud: true, ktiv_male_haser: true, ..Default::default() });
        assert!(acc(&q, "\u{05EA}\u{05BC}\u{05D5}\u{05E8}\u{05D4}"));
        assert!(!acc(&q, "\u{05EA}\u{05D5}\u{05E8}\u{05D4}"));
        assert!(acc(&q, "\u{05EA}\u{05D5}\u{05D5}\u{05E8}\u{05D4}")); // letters changed: marks free
    }

    #[test]
    fn required_marks_with_typo_edits() {
        // 4 letters, typo as the only option: edit distance 1 in the walk;
        // the unedited word needs its typed mark, an edited one does not
        let typed = "\u{05D1}\u{05B0}\u{05E8}\u{05DB}\u{05D4}";
        let q = marked(typed, WalkOptions { match_nikud: true, spelling_errors: true, ..Default::default() });
        assert!(acc(&q, typed));
        assert!(acc(&q, "\u{05D1}\u{05B0}\u{05BC}\u{05E8}\u{05B8}\u{05DB}\u{05B8}\u{05D4}"));
        assert!(!acc(&q, "\u{05D1}\u{05E8}\u{05DB}\u{05D4}")); // unedited, mark missing
        assert!(acc(&q, "\u{05D1}\u{05E8}\u{05E7}\u{05D4}")); // substitution: marks free
        assert!(acc(&q, "\u{05D1}\u{05B8}\u{05E8}\u{05DB}\u{05D4}\u{05D4}")); // insertion: marks free
        assert!(!acc(&q, "\u{05D1}\u{05E8}\u{05E7}\u{05D7}")); // two edits
        // a mark is never an edit: an extra mark on the unedited word is free
        assert!(acc(&q, "\u{05D1}\u{05B0}\u{05E8}\u{05B8}\u{05DB}\u{05D4}"));
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

    /// With `KtivRules::FtsLib` the walk accepts exactly the word and FtsLib's
    /// `KetivExpander` output (checked over a candidate universe: FtsLib's
    /// forms, Otzaria's forms, every single vav / yod insertion / deletion).
    #[test]
    fn ktiv_ftslib_rules_accept_exactly_ftslib_forms() {
        let k = WalkOptions {
            ktiv_male_haser: true,
            ktiv_rules: KtivRules::FtsLib,
            ..Default::default()
        };
        for &(word, expected) in crate::ftslib_ketiv::FTSLIB_CASES {
            let a = walk(word, k);
            let want: BTreeSet<String> =
                expected.iter().map(|s| s.to_string()).chain([word.to_string()]).collect();
            let mut universe = want.clone();
            universe.extend(variants::ktiv_male_haser_variants_limited(word, 1000));
            let chars: Vec<char> = word.chars().collect();
            for i in 0..=chars.len() {
                for ins in ['\u{05D5}', '\u{05D9}'] {
                    let mut c = chars.clone();
                    c.insert(i, ins);
                    universe.insert(c.into_iter().collect());
                }
                if i < chars.len() {
                    let mut c = chars.clone();
                    c.remove(i);
                    universe.insert(c.into_iter().collect());
                }
            }
            for cand in &universe {
                assert_eq!(
                    matches(&a, cand),
                    want.contains(cand),
                    "word {} cand {}",
                    word.escape_unicode(),
                    cand.escape_unicode()
                );
            }
        }
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
