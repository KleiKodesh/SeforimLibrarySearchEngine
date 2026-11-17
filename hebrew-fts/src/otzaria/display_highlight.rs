// Ported verbatim from otzaria_search_engine 7.2.0 (MIT, Copyright (c) 2026 otzaria),
// rust/src/display_highlight.rs; see THIRD-PARTY-NOTICES.md. Changes: module paths, the test
// module dropped (golden tests live in hebrew-fts/tests/otzaria_helpers.rs), `is_alphanumeric`
// read from the 7.2.0 binary's pinned table (unicode_alnum.rs).
//! Display-highlight patterns and a prepared bounded matcher.
//!
//! Display word patterns tolerate nikud and quote forms. The prepared matcher
//! normalizes displayed HTML with source offsets, uses the index tokenizer,
//! and resolves phrase gaps with dynamic programming. It compiles each word
//! once; no full-phrase backtracking regex runs in the rendering isolate.
//!
//! `combined_pattern` remains compatible for a single word. For multiple words
//! it is the ECMAScript never-match sentinel `(?!)`; callers must use `matcher`.
//! Morphological bounds belong to each normalized spelling/alternative branch,
//! rather than the typed word's length. A waived boundary lets a word carry
//! its own affixes — at the phrase's outer endpoints as inside it. A caller
//! waiving a plain word's boundary gets the root anywhere in its token.

use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;

use crate::otzaria::hebrew_query::{
    affix_arms, aramaic_root_variants, generate_spelling_variations, is_word_mark,
    normalize_for_index, split_query_words, word_flags_at, WordFlags, BREAKING_TAG_NAMES,
    MAX_SPELLING_BRANCHES, SCRIPT_TAG_NAMES,
};
use crate::otzaria::hebrew_tokenizer::continues_token;

// ── Output type ────────────────────────────────────────────────────────────

/// Everything the Dart layer needs to highlight matches in displayed text.
pub struct DisplayHighlight {
    /// Single-word compatibility pattern, or `(?!)` for multiple words.
    pub combined_pattern: String,
    /// Per-word regex (alternation-wrapped when a word has several branches),
    /// used to locate each word's sub-range inside a combined match.
    pub word_patterns: Vec<String>,
    /// `true` when the word carries no morphological expansion option and the
    /// UI may therefore require token boundaries around its match.
    pub word_boundary_eligible: Vec<bool>,
    pub matcher: crate::otzaria::highlight_matcher::PreparedHighlightMatcher,
}

// ── Pattern fragments (Dart-RegExp dialect) ────────────────────────────────

/// Character class of nikud/cantillation marks *attached to a letter* —
/// U+0591–U+05C7 minus the separators in that range: maqaf (U+05BE), paseq
/// (U+05C0), sof pasuq (U+05C3) and nun hafukha (U+05C6) — the same set
/// `hebrew_query::is_attached_mark` excludes. Were they included, the `*`
/// after a letter would swallow a separator between words and break
/// word-boundary detection (e.g. "אשר־שמע") or let a one-word pattern
/// highlight across a verse boundary (e.g. "אב" matching "א׃ב").
/// Also covers the general combining range U+0300–U+036F (Judeo-Arabic
/// transliteration dots: `כלת̇ום`), which the tokenizer treats like attached
/// marks — so a mark-free query term highlights the marked display form.
pub(crate) const ATTACHED_MARKS_CLASS: &str =
    "[\u{0300}-\u{036F}\u{0591}-\u{05BD}\u{05BF}\u{05C1}\u{05C2}\u{05C4}\u{05C5}\u{05C7}]*";

/// אותה מחלקה ללא הכמת — לבניית `+`/`*` מפורשים ב-fragment הגבול.
const ATTACHED_MARKS_SET: &str =
    "[\u{0300}-\u{036F}\u{0591}-\u{05BD}\u{05BF}\u{05C1}\u{05C2}\u{05C4}\u{05C5}\u{05C7}]";

/// טווחי התווים שמהם נגזרות מחלקות המפרידים. מחוץ להם ברירת המחדל היא
/// "תו מילה" — נכון לאותיות של כתבים אחרים (`is_alphanumeric`), ומפספס רק
/// סימני פיסוק אקזוטיים. הטווחים: ASCII + Latin-1, סימנים משולבים והבלוק
/// העברי, ערבית (תעתיק ערבית-יהודית), פיסוק כללי עד Supplemental Punctuation
/// (מקפים טיפוגרפיים כמו `⸗` U+2E17 ו-`−` U+2212), צורות תצוגה עבריות,
/// ופיסוק fullwidth.
const SEPARATOR_DOMAIN: &[(u32, u32)] = &[
    (0x0009, 0x00FF),
    (0x0300, 0x06FF),
    (0x2000, 0x2E7F),
    (0xFB00, 0xFB4F),
    (0xFF00, 0xFFEF),
];

/// גוף מחלקת תווים (בלי הסוגריים) של כל תו בטווחי [`SEPARATOR_DOMAIN`]
/// שעבורו `keep` מחזיר `false`. הטווחים מכווצים ל-`a-b` והתווים
/// נמלטים כ-`\uXXXX`, כדי שהתבנית תישאר קצרה וחוקית ב-`RegExp` של Dart.
fn class_body_excluding(keep: impl Fn(char) -> bool) -> String {
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &(start, end) in SEPARATOR_DOMAIN {
        for code in start..=end {
            let Some(c) = char::from_u32(code) else {
                continue;
            };
            if keep(c) {
                continue;
            }
            match runs.last_mut() {
                Some(run) if run.1 + 1 == code => run.1 = code,
                _ => runs.push((code, code)),
            }
        }
    }

    let mut body = String::new();
    for (start, end) in runs {
        body.push_str(&escape_in_class(start));
        // טווח בן שני תווים אינו חוסך דבר על פני שניהם בנפרד.
        if end > start + 1 {
            body.push('-');
        } else if end == start + 1 {
            body.push_str(&escape_in_class(end));
            continue;
        } else {
            continue;
        }
        body.push_str(&escape_in_class(end));
    }
    body
}

fn escape_in_class(code: u32) -> String {
    format!("\\u{code:04X}")
}

/// שלוש מחלקות תווים **זרות זו לזו** שמכסות יחד כל תו, ונגזרות מהפרדיקטים
/// של הטוקנייזר. הזרות היא תנאי הכרחי: כשמחלקת המילה ומחלקת המפריד חופפות,
/// `SEP(?:WORD SEP){0,n}` הופך דו-משמעי, ו-`RegExp` של Dart (מנוע עם
/// backtracking) נתקע על שורה שאינה מתאימה — עשרות שניות ב-thread הראשי.
///
/// * [ALNUM_CLASS] — אות או ספרה; רק כאן טוקן יכול להתחיל ולהסתיים.
/// * [SOFT_CHAR_CLASS] — סימן צמוד, פיסוק שקוף או גרש/גרשיים: ממשיך טוקן
///   אך לעולם אינו פותח אותו.
/// * [BREAK_CHAR_CLASS] — כל השאר: שובר טוקן.
#[cfg(test)]
static ALNUM_CLASS: Lazy<String> =
    Lazy::new(|| format!("[^{}]", class_body_excluding(is_alnum_char)));

static SOFT_CHAR_CLASS: Lazy<String> = Lazy::new(|| {
    format!(
        "[{}]",
        class_body_excluding(|c| !(continues_token(c) && !is_alnum_char(c)))
    )
});

static BREAK_CHAR_CLASS: Lazy<String> = Lazy::new(|| {
    format!(
        "[{}]",
        class_body_excluding(|c| continues_token(c) || is_markup_char(c))
    )
});

/// אות או ספרה — לא סימן צמוד. סימני ניקוד מסווגים ב-Unicode כ-alphabetic,
/// ובלי ההחרגה הם היו נחשבים פותחי-טוקן.
fn is_alnum_char(c: char) -> bool {
    super::unicode_alnum::is_alphanumeric(c) && !is_word_mark(c)
}

/// `<` ו-`>` מטופלים רק כתג שלם ([`INLINE_TAG`]/[`BREAKING_TAG`]) ולא כתווים
/// בודדים בשום מחלקה, כדי שהחלופות יישארו זרות וכדי שאותיות שבתוך תג לא
/// ייחשבו למילה.
fn is_markup_char(c: char) -> bool {
    c == '<' || c == '>'
}

/// גוף תג שבירה, בלי `<`: `/` פותח אופציונלי, שם מהרשימה המשותפת עם
/// האינדוקס, ואחרי השם `>` מיד או תו לא-אלפאנומרי ואז שאר התג. הארוכים
/// קודם, שגם `pre` וגם `p` ייבחנו בסדר שממצה את החלופות. sup/sub תמיד שוברים
/// כאן — חריג האות התלויה (`מְ<sup>נַ</sup>שֶּׁה`) של האינדוקס אינו ממודל.
static BREAKING_TAG_BODY: Lazy<String> = Lazy::new(|| {
    let mut names: Vec<&str> = [BREAKING_TAG_NAMES, SCRIPT_TAG_NAMES].concat();
    names.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    format!(
        "/?(?:{names})(?:[^>a-zA-Z0-9][^>]*)?>",
        names = names.join("|")
    )
});

/// תג inline (`<b>`, `<span>`…) — אינו שובר טוקן: האינדוקס מוחק אותו בלי
/// רווח, ולכן `מי<b>לה` הוא טוקן אחד. lookahead שלילי (נתמך ב-RegExp של
/// Dart) מוציא ממנו את תגי השבירה, כך ששתי חלופות התג נשארות זרות —
/// הזרות היא מה שמונע התפוצצות backtracking.
static INLINE_TAG: Lazy<String> =
    Lazy::new(|| format!("<(?!{body})[^>]*>", body = *BREAKING_TAG_BODY));

/// תג שבירה (`<br>`, `</p>`…) — האינדוקס ממיר אותו לרווח
/// ([`BREAKING_TAG_NAMES`]), ולכן בהדגשה הוא שובר טוקן.
static BREAKING_TAG: Lazy<String> = Lazy::new(|| format!("<{body}", body = *BREAKING_TAG_BODY));

/// מפריד בין שתי מילות שאילתה סמוכות: חייב לכלול לפחות שובר-טוקן אחד
/// (תו שובר או תג שבירה), ולפניו ואחריו מותרים סימנים רכים ותגי inline.
/// `תדע. זרעך` ו-`תדע<br>זרעך` מתאימים, ואילו `תדע<b>זרעך` אינו מתאים —
/// האינדקס רואה שם טוקן אחד ולכן גם החיפוש אינו מוצא אותו.
static WORD_SEPARATOR: Lazy<String> = Lazy::new(|| {
    format!(
        "(?:{soft}|{tag})*(?:{brk}|{btag})(?:{soft}|{brk}|{tag}|{btag})*",
        soft = *SOFT_CHAR_CLASS,
        brk = *BREAK_CHAR_CLASS,
        tag = *INLINE_TAG,
        btag = *BREAKING_TAG,
    )
});

/// Pattern-body budget for each source (query shape or matched terms).
/// A hybrid affix+typo word has one bounded source of each kind, so spelling
/// branches cannot consume the matched-term allowance. Display patterns are
/// ~3× longer than index-term patterns (each letter carries a marks class), so this is
/// looser than `hebrew_query::MAX_PATTERN_CHARS`; at least one branch is
/// always kept. Raised with the search-side budgets (parity), but capped
/// short of a strict 3× of the index budget — past ~12k chars the app-side
/// `RegExp` gets slow while extra branches stop adding visible highlights.
const MAX_DISPLAY_PATTERN_CHARS: usize = 12_000;

// ── Spacing resolution ─────────────────────────────────────────────────────

/// Resolves the allowed intermediate-word count for the gap after word `i` —
/// the shared [`hebrew_query::resolve_gaps`] (mirroring the historical Dart
/// `_highlightSeparatorForIndex`), so the book highlight and the engine's
/// per-pair phrase verification agree gap-by-gap.
fn spacing_for_gaps(
    custom_spacing: &HashMap<String, String>,
    distance: u32,
    word_count: usize,
) -> Vec<u32> {
    crate::otzaria::hebrew_query::resolve_gaps(custom_spacing, distance, word_count)
}

// ── Per-word pattern building ──────────────────────────────────────────────

/// גרש/גרשיים אופציונליים בין שתי אותיות עבריות בטקסט התצוגה. האינדקס
/// מטמיע לכל מילה עם גרשיים גם את צורתה הנקייה (הטוקן-התאום), כך שטרם
/// נטול-גרשיים ("רמבם") מתאים בדין לטקסט מודפס `רמב"ם` — וההדגשה חייבת
/// לכסות זאת. עד שני תווים: זוג-גרשים ≡ גרשיים (מוסכמת `רמב''ם`).
/// כולל את הצורות הטיפוגרפיות (U+2018/U+2019/U+201C/U+201D) שהטוקנייזר
/// מקפל — `רמח”ל` בדפוס מודגש ע"י `רמח"ל`/`רמחל`.
pub(crate) const OPTIONAL_QUOTES: &str = "[\"'\\u05F3\\u05F4\\u2018\\u2019\\u201C\\u201D]{0,2}";

/// גרש בטקסט תצוגה: ASCII, עברי, או ציטוט-יחיד טיפוגרפי — הצורות
/// שהטוקנייזר מקפל ל-`'` (ראו `hebrew_tokenizer::is_geresh`).
const GERESH_DISPLAY_CLASS: &str = "['\\u05F3\\u2018\\u2019]";

/// גרשיים בטקסט תצוגה: ", ״, צורה טיפוגרפית, או זוג גרשים מודפס
/// (`רמב''ם`) — הצורות שהטוקנייזר מקפל ל-`"`.
const GERSHAYIM_DISPLAY_CLASS: &str = "(?:[\"\\u05F4\\u201C\\u201D]|['\\u05F3\\u2018\\u2019]{2})";

/// Builds the display pattern for one literal term: each Hebrew base letter
/// may be followed by attached marks, and geresh/gershayim match both the
/// ASCII and Hebrew forms. The term itself is expected to be nikud-free.
fn charwise_display_pattern(term: &str) -> String {
    let mut out = String::with_capacity(term.len() * 4);
    let mut prev_was_letter = false;
    for ch in term.chars() {
        // בין שתי אותיות עבריות רצופות בטרם — גרשיים אופציונליים בתצוגה.
        if prev_was_letter && matches!(ch, 'א'..='ת') {
            out.push_str(OPTIONAL_QUOTES);
        }
        prev_was_letter = matches!(ch, 'א'..='ת');
        match ch {
            'א'..='ת' => {
                out.push(ch);
                out.push_str(ATTACHED_MARKS_CLASS);
            }
            // `"` בטרם ≡ גרשיים בדפוס: ", ״, צורה טיפוגרפית (“ ”), או זוג
            // גרשים (מוסכמת `רמב''ם` בקבצים ישנים — הטוקנייזר מאחד אותו
            // ל-`"` אבל טקסט התצוגה נשמר כפי שנדפס).
            '"' => out.push_str(GERSHAYIM_DISPLAY_CLASS),
            '\'' => out.push_str(GERESH_DISPLAY_CLASS),
            _ => push_escaped_char(&mut out, ch),
        }
    }
    out
}

/// דוחף תו בודד ל-`out` עם escape של מטא-תווי רגקס — בלי הקצאות ביניים
/// (בניגוד ל-`escape_regex` שמקבל מחרוזת). אותה קבוצת תווים כמו
/// `hebrew_query::escape_regex`, תקפה גם ל-RegExp של Dart.
#[inline]
fn push_escaped_char(out: &mut String, ch: char) {
    if matches!(
        ch,
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
    ) {
        out.push('\\');
    }
    out.push(ch);
}

/// Expands one query word (plus its alternatives) into display branches:
/// spelling variants when כתיב מלא/חסר is on, then one charwise pattern per
/// term, deduplicated in insertion order and kept within the length budget.
fn display_terms(word: &str, flags: &WordFlags, alternatives: &[String]) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    let mut seen_terms: HashSet<String> = HashSet::new();

    let push_term = |terms: &mut Vec<String>, seen: &mut HashSet<String>, raw: &str| {
        let mut stripped = normalize_for_index(raw);
        // התעלם מגרשיים: התבנית נבנית מהצורה הנקייה — הגרשיים האופציונליים
        // ש-charwise_display_pattern מתיר בין אותיות מכסים את שתי הצורות.
        if flags.ignore_quotes {
            stripped = crate::otzaria::hebrew_query::strip_quote_chars(&stripped);
        }
        if stripped.trim().is_empty() {
            return;
        }
        // סיומות ארמיות: גם וריאנט השקילות הסופית (מלכה↔מלכא, חכמים↔חכמין)
        // מודגש; קידומות ארמיות מכוסות בכך שהמילה מאבדת את זכאות גבול-המילה
        // (הדפוס מדגיש גם בתוך צורה עם קידומת) — כמו שאר אפשרויות המורפולוגיה.
        let bases = if flags.aramaic_suffix {
            aramaic_root_variants(&stripped)
        } else {
            vec![stripped]
        };
        for base in bases {
            if flags.spelling {
                for variant in generate_spelling_variations(&base, MAX_SPELLING_BRANCHES) {
                    if seen.insert(variant.clone()) {
                        terms.push(variant);
                    }
                }
            } else if seen.insert(base.clone()) {
                terms.push(base);
            }
        }
    };

    push_term(&mut terms, &mut seen_terms, word);
    for alt in alternatives {
        push_term(&mut terms, &mut seen_terms, alt);
    }
    terms
}

fn bounded_charwise_display_pattern(term: &str) -> Option<String> {
    // Count characters before allocating a potentially huge display regex.
    // The same pieces are appended by `charwise_display_pattern` below.
    let marks = ATTACHED_MARKS_CLASS.chars().count();
    let mut previous_hebrew = false;
    let mut length = 0;
    for c in term.chars() {
        let hebrew = matches!(c, 'א'..='ת');
        if previous_hebrew && hebrew {
            length += OPTIONAL_QUOTES.len();
        }
        previous_hebrew = hebrew;
        length += match c {
            'א'..='ת' => 1 + marks,
            '"' => GERSHAYIM_DISPLAY_CLASS.len(),
            '\'' => GERESH_DISPLAY_CLASS.len(),
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' => 2,
            _ => 1,
        };
        if length > MAX_DISPLAY_PATTERN_CHARS {
            return None;
        }
    }
    Some(charwise_display_pattern(term))
}

fn build_word_display_pattern(
    word: &str,
    flags: &WordFlags,
    alternatives: &[String],
) -> Option<String> {
    let terms = display_terms(word, flags, alternatives);

    // Length budget: keep branches while the cumulative size stays under the
    // cap. An oversized first branch fails generation rather than truncating
    // the typed root or blocking the rendering isolate during compilation.
    let mut branches: Vec<String> = Vec::new();
    let mut total = 0usize;
    for term in &terms {
        let Some(branch) = bounded_charwise_display_pattern(term) else {
            if branches.is_empty() {
                return None;
            }
            break;
        };
        let len = branch.chars().count();
        if !branches.is_empty() && total + len > MAX_DISPLAY_PATTERN_CHARS {
            break;
        }
        total += len;
        branches.push(branch);
    }

    Some(match branches.len() {
        0 => String::new(),
        1 => branches.into_iter().next().unwrap(),
        _ => format!("(?:{})", branches.join("|")),
    })
}

fn matcher_branches(
    word: &str,
    flags: &WordFlags,
    alternatives: &[String],
) -> Vec<(String, String, String)> {
    let mut branches = Vec::new();
    let mut total = 0;
    for root in display_terms(word, flags, alternatives) {
        let pattern = charwise_display_pattern(&root);
        let len = pattern.chars().count();
        if !branches.is_empty() && total + len > MAX_DISPLAY_PATTERN_CHARS {
            break;
        }
        total += len;
        for arm in affix_arms(&root, flags, false) {
            let core = if arm.core == root {
                pattern.clone()
            } else {
                charwise_display_pattern(&arm.core)
            };
            branches.push((arm.lead, core, arm.trail));
        }
    }
    branches
}

// ── Public entry point ─────────────────────────────────────────────────────

/// Builds the display-highlight patterns for a search query.
///
/// # Parameters
///
/// - `query` — the raw query string as typed (nikud and punctuation allowed).
/// - `distance` — default intermediate-word allowance between adjacent words
///   when `custom_spacing` is empty.
/// - `custom_spacing` — per-pair overrides keyed `"i-(i+1)"` → word count.
/// - `alternative_words` — synonyms per word position (0-indexed).
/// - `search_options` — per-word option checkboxes, keyed `"{word}_{index}"`,
///   with the same word tokenization used for engine queries.
/// - `any_order` — the words in any order within one window (see
///   [`crate::otzaria::hebrew_query::resolve_proximity`]).
///
/// Returns `None` when the query contains no highlightable words.
pub fn build_display_highlight(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
    any_order: bool,
) -> Option<DisplayHighlight> {
    let words = split_query_words(query);
    if words.is_empty() {
        return None;
    }

    let mut word_patterns: Vec<String> = Vec::new();
    let mut word_boundary_eligible: Vec<bool> = Vec::new();
    let mut plans = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let flags = word_flags_at(&words, i, search_options);
        let alts = alternative_words
            .get(&(i as u32))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let pattern = build_word_display_pattern(word, &flags, alts)?;
        if pattern.is_empty() {
            // Word vanished under normalization (e.g. a nikud-only token);
            // skip it rather than emit an empty branch that matches anywhere.
            continue;
        }
        // קידומות ארמיות שוברות זכאות גבול-מילה (הדגשה גם בתוך צורה עם
        // קידומת); סיומות ארמיות הן וריאנטים שלמים ואינן שוברות.
        let has_expansion = flags.prefix
            || flags.suffix
            || flags.gram_prefix
            || flags.gram_suffix
            || flags.partial
            || flags.aramaic_prefix;
        word_patterns.push(pattern);
        word_boundary_eligible.push(!has_expansion);
        plans.push(matcher_branches(word, &flags, alts));
    }

    assemble_display_highlight(
        word_patterns,
        word_boundary_eligible,
        plans,
        distance,
        custom_spacing,
        any_order,
    )
}

/// Prepares per-word token regexes and the bounded phrase matcher. Single-word
/// compatibility patterns are retained; multiword patterns are deliberately a
/// safe never-match sentinel because ECMAScript phrase gaps can backtrack.
fn assemble_display_highlight(
    word_patterns: Vec<String>,
    word_boundary_eligible: Vec<bool>,
    plans: Vec<Vec<(String, String, String)>>,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    any_order: bool,
) -> Option<DisplayHighlight> {
    if word_patterns.is_empty() {
        return None;
    }

    let proximity = crate::otzaria::hebrew_query::resolve_proximity(
        spacing_for_gaps(custom_spacing, distance, word_patterns.len()),
        distance,
        custom_spacing,
        any_order,
    );
    let combined_pattern = if word_patterns.len() == 1 {
        word_patterns[0].clone()
    } else {
        // ECMAScript backtracking over multiple independent word gaps can
        // freeze a rendering isolate. Multiword callers must use `matcher`.
        "(?!)".to_string()
    };

    let matcher = crate::otzaria::highlight_matcher::PreparedHighlightMatcher::new(
        plans,
        proximity,
        word_boundary_eligible.clone(),
    )
    .ok()?;
    Some(DisplayHighlight {
        combined_pattern,
        word_patterns,
        word_boundary_eligible,
        matcher,
    })
}

// ── Matched-terms entry point ──────────────────────────────────────────────

/// Longest-first charwise branches for a set of matched index terms.
///
/// Alternation in Dart's `RegExp` is leftmost-first, so a shorter term that
/// prefixes a longer one (`ספר` vs `ספרים`) must come after it or it would
/// truncate the longer word's highlight. Budget-capped like
/// [`build_word_display_pattern`]: the budget keeps `word` itself first, then
/// the terms closest to it in length — cutting longest-first dropped the typed
/// word whenever it had thousands of variants.
fn build_terms_display_pattern(terms: &[String], word: &str, budget: usize) -> String {
    let word_len = word.chars().count();
    // Count UTF-8 characters once per term, not for every sort comparison.
    let mut by_priority: Vec<(&str, usize)> = terms
        .iter()
        .map(|term| (term.as_str(), term.chars().count()))
        .collect();
    by_priority.sort_unstable_by_key(|&(term, len)| (term != word, len.abs_diff(word_len), term));
    by_priority.dedup();

    let mut kept: Vec<(&str, usize, String)> = Vec::new();
    let mut total = 0usize;
    for (term, term_len) in by_priority {
        let branch = charwise_display_pattern(term);
        let len = branch.chars().count();
        if !kept.is_empty() && total + len > budget {
            break;
        }
        total += len;
        kept.push((term, term_len, branch));
    }
    kept.sort_unstable_by(|(a, a_len, _), (b, b_len, _)| b_len.cmp(a_len).then_with(|| a.cmp(b)));
    let branches: Vec<String> = kept.into_iter().map(|(_, _, branch)| branch).collect();

    match branches.len() {
        0 => String::new(),
        1 => branches.into_iter().next().unwrap(),
        _ => format!("(?:{})", branches.join("|")),
    }
}

/// The display branches applied to normalized index terms. Mark classes and
/// typographic quote forms cannot occur in those terms; keeping them in this
/// temporary classifier multiplies compilation work without adding coverage.
fn normalized_term_coverage_pattern(plan: &[(String, String, String)]) -> String {
    let branches: Vec<String> = plan
        .iter()
        .map(|(lead, root, trail)| format!("(?:{lead})(?:{root})(?:{trail})"))
        .collect();
    format!("\\A(?:{})\\z", branches.join("|"))
        .replace(ATTACHED_MARKS_CLASS, "")
        .replace(OPTIONAL_QUOTES, "[\"']{0,2}")
        .replace(GERSHAYIM_DISPLAY_CLASS, "(?:\"|'{2})")
        .replace(GERESH_DISPLAY_CLASS, "'")
}

/// Builds display-highlight patterns from the *index terms the query actually
/// matches* — one `Vec<String>` per query word, aligned with
/// [`split_query_words`] over the engine-normalized query (the same order
/// `hebrew_query::prepare_advanced_query` builds `regex_terms` in).
///
/// Per word, the pattern source depends on the active options:
///
/// - **Typo tolerance without expansion**: the matched index terms —
///   Levenshtein variants the query-shape builder cannot reproduce — are
///   painted as whole tokens, boundaries kept.
/// - **Boundary-breaking expansion** (prefix / suffix / partial): the compact
///   query-shape pattern highlights only the typed substring/root with
///   boundaries waived. Unlike the term list it is not budget-truncated, so
///   every occurrence highlights — the term list can drop visible words when
///   the option matches thousands of index tokens. With typo tolerance, the
///   matched terms the word's own branches do not accept are added as whole
///   tokens, with their own matched-term budget. Each source is bounded;
///   combining them can use up to twice the per-source allowance.
/// - **No expansion** (plain / spelling): the matched whole tokens, boundaries
///   kept.
///
/// A word with an empty term list (nothing in this index matched, or its
/// automatons failed to compile) falls back to the query-shape
/// [`build_word_display_pattern`] with its original boundary rules — the
/// result is never worse than [`build_display_highlight`].
pub fn build_display_highlight_from_terms(
    query: &str,
    distance: u32,
    custom_spacing: &HashMap<String, String>,
    alternative_words: &HashMap<u32, Vec<String>>,
    search_options: &HashMap<String, HashMap<String, bool>>,
    per_word_terms: &[Vec<String>],
    any_order: bool,
) -> Option<DisplayHighlight> {
    let normalized = normalize_for_index(query);
    let words = split_query_words(&normalized);
    if words.is_empty() {
        return None;
    }

    let mut word_patterns: Vec<String> = Vec::new();
    let mut word_boundary_eligible: Vec<bool> = Vec::new();
    let mut plans = Vec::new();
    for (i, word) in words.iter().enumerate() {
        let flags = word_flags_at(&words, i, search_options);
        let alts = alternative_words
            .get(&(i as u32))
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let matched = per_word_terms.get(i).map(Vec::as_slice).unwrap_or(&[]);

        // הרחבה ששוברת גבול-מילה (קידומת/סיומת/חלק-ממילה): ה-query-shape
        // קומפקטי, מכסה כל התאמה ומדגיש רק את החלק שהוקלד — בניגוד למונחי-
        // האינדקס שחסומים בתקציב ומדגישים מילים שלמות. מונחי-האינדקס נשמרים
        // לשגיאות-כתיב (וריאנטים שה-query-shape אינו יודע להפיק).
        let has_expansion = flags.prefix
            || flags.suffix
            || flags.gram_prefix
            || flags.gram_suffix
            || flags.partial
            || flags.aramaic_prefix;
        // מונחי-האינדקס הם טוקנים שלמים, כולל הקידומת/הסיומת — בלי חלון.
        let (pattern, boundary_eligible, plan) = if !matched.is_empty() && !has_expansion {
            let pattern = build_terms_display_pattern(matched, word, MAX_DISPLAY_PATTERN_CHARS);
            let plan = vec![(String::new(), pattern.clone(), String::new())];
            (pattern, true, plan)
        } else {
            let mut pattern = build_word_display_pattern(word, &flags, alts)?;
            let mut plan = matcher_branches(word, &flags, alts);
            if flags.typo && !pattern.is_empty() {
                // מונח שענפי המילה כבר תופסים היה ממלא את התקציב ודוחק את וריאנטי-הטעות.
                let covered_pattern = normalized_term_coverage_pattern(&plan);
                let covered = regex::RegexBuilder::new(&covered_pattern)
                    .case_insensitive(true)
                    .build()
                    .ok();
                let typo_terms: Vec<String> = matched
                    .iter()
                    .filter(|term| !covered.as_ref().is_some_and(|re| re.is_match(term)))
                    .cloned()
                    .collect();
                // Keep the original matched-term allowance independently of
                // the query shape. Full/defective spelling can fill the shape
                // budget even when the index contains only two typo tokens.
                // Covered forms need no term slots; every typo kept by the old
                // terms-only builder can therefore still fit here.
                let typo_pattern =
                    build_terms_display_pattern(&typo_terms, word, MAX_DISPLAY_PATTERN_CHARS);
                if !typo_pattern.is_empty() {
                    pattern = format!("(?:{pattern}|{typo_pattern})");
                    plan.push((String::new(), typo_pattern, String::new()));
                }
            }
            (pattern, !has_expansion, plan)
        };
        if pattern.is_empty() {
            continue;
        }
        word_patterns.push(pattern);
        word_boundary_eligible.push(boundary_eligible);
        plans.push(plan);
    }

    assemble_display_highlight(
        word_patterns,
        word_boundary_eligible,
        plans,
        distance,
        custom_spacing,
        any_order,
    )
}

// ── Literal in-book pattern ────────────────────────────────────────────────

/// מחלקת האותיות העבריות לבדיקת גבולות מילה בתבנית הליטרלית: אותיות בסיס
/// (U+05D0–U+05EA), ליגטורות יידיש (U+05F0–U+05F2) וצורות תצוגה
/// (U+FB1D–U+FB4F) — תואם את `_isHebrewLetter` של החיפוש המקומי בספר.
const HEBREW_LETTER_CLASS: &str = r"א-תװ-ײיִ-ﭏ";

/// fragment גרש/גרשיים לבדיקת הגבול, בסמנטיקת הטוקנייזר: רצף-ציטוט חוקי
/// הוא `Q = גרשיים | גרש אחד/שניים`, וכמה רצפים חוקיים המופרדים בסימנים
/// צמודים משתרשרים — `Q (סימן+ Q)*` (רמב''ְ"ם הוא טוקן אחד; ראו
/// test_quote_runs_split_by_marks_dedupe_to_single_gershayim). רצפים
/// צמודים לא-חוקיים ("", ''', '") אינם חיבור — נשארים גבול.
fn quote_boundary_fragment() -> String {
    let q = format!(
        "(?:[{g2}]|[{g1}]{{1,2}})",
        g2 = "\"\u{05F4}\u{201C}\u{201D}",
        g1 = "'\u{05F3}\u{2018}\u{2019}",
    );
    format!("(?:{q}(?:{m}+{q})*)", m = ATTACHED_MARKS_SET)
}

/// Builds the regex for highlighting *literal* in-book search matches (the
/// simple/exact mode that scans the open book locally): the query phrase
/// as-typed, whitespace-joined, nikud-tolerant after every character,
/// geresh/gershayim matching both ASCII and Hebrew forms, and word-boundary
/// lookarounds so `אמר` does not light up inside `ויאמר` — mirroring the
/// local search's `_containsWholeWord` semantics.
///
/// The query is deliberately NOT sanitized or tokenized like engine queries:
/// the local scan matches the text as typed, and the highlight must mirror
/// that scan, not the engine's tokenizer.
///
/// Dart-RegExp dialect; compile with `caseSensitive: false, unicode: true`.
/// Returns `None` for a whitespace-only query.
pub fn build_literal_pattern(query: &str) -> Option<String> {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return None;
    }
    let last = words.len() - 1;
    // המפריד בין המילים הוא אותו [`WORD_SEPARATOR`] של החיפוש המתקדם: מה
    // שהאינדקס רואה כשני טוקנים סמוכים חייב להיות מודגש גם במסלול המקומי.
    // רשימה מצומצמת (רווח/מקף/פסק) החטיאה פיסוק דבוק — `תדע, זרעך` נמצא
    // בחיפוש הגלובלי ולא הודגש בספר.
    let phrase = words
        .iter()
        .enumerate()
        .map(|(i, w)| literal_charwise_pattern(w, i == last))
        .collect::<Vec<_>>()
        .join(&WORD_SEPARATOR);
    // גבול תלוי-הקשר: התאמה נפסלת אם לפניה אות (או אות+גרש/גרשיים — המילה
    // ממשיכה מתוך רש״י), או אם אחריה אות (או גרש/גרשיים+אות). גרשיים שאחריו
    // לא-אות (סוגר ציטוט) נשאר גבול — ״הרעתי״ מודגש. ה-lookahead מדלג בעצמו
    // על סימנים צמודים שנותרו: [marks]* שבתבנית נסוג ב-backtracking ומשאיר
    // ניקוד בין ההתאמה לאות הבאה (אמר בתוך אָמַרְתִּי) — הדילוג סוגר את הפרצה.
    let qf = quote_boundary_fragment();
    Some(format!(
        "(?<![{cls}]{marks}{qf}?{marks})(?:{phrase})(?!{marks}{qf}?{marks}[{cls}])",
        cls = HEBREW_LETTER_CLASS,
        marks = ATTACHED_MARKS_CLASS,
    ))
}

/// תבנית תו-אחר-תו לביטוי ליטרלי: אחרי כל תו מותרים סימני ניקוד/טעמים
/// (הטקסט המוצג מנוקד; השאילתה בדרך כלל לא), וגרש/גרשיים תופסים את שתי
/// הצורות — הלועזית והעברית.
fn is_query_quote(ch: char) -> bool {
    matches!(
        ch,
        '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}' | '\'' | '\u{05F3}' | '\u{2018}' | '\u{2019}'
    )
}

/// גרשיים בלבד (לא גרש): גרש סוגר יחיד (תוס׳) הוא חלק מהטוקן לפי
/// הטוקנייזר ולפי generate_highlight_pattern, ולכן נשאר נצרך ומודגש;
/// רק גרשיים סוגר של ציטוט הופך ל-lookahead.
fn is_query_gershayim(ch: char) -> bool {
    matches!(ch, '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}')
}

fn push_quote_class(out: &mut String, ch: char) {
    match ch {
        '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}' => out.push_str(GERSHAYIM_DISPLAY_CLASS),
        _ => out.push_str(GERESH_DISPLAY_CLASS),
    }
}

fn literal_charwise_pattern(word: &str, is_last_word: bool) -> String {
    let total_chars = word.chars().count();
    // רק במילה האחרונה, ורק גרשיים (לא גרש): גרשיים נגרר הופך ל-lookahead
    // (מאומת אך לא נצרך/נצבע) — "הרעתי\u{05F4}" מדגיש "הרעתי" בלבד, כמו
    // generate_highlight_pattern. גרש סוגר (תוס\u{05F3}) הוא חלק מהטוקן —
    // נשאר נצרך. במילים לא-אחרונות הכל נצרך, אחרת המפריד שאחריהן היה פוגש
    // גרש במקום רווח ו"ר\u{05F3} עקיבא" לא היה נמצא.
    let mut trailing_quotes = 0;
    if is_last_word {
        for ch in word.chars().rev() {
            if is_query_gershayim(ch) {
                trailing_quotes += 1;
            } else {
                break;
            }
        }
    }
    // מילה שכולה גרש/גרשיים — אין ליבה; משאירים הכל נצרך (מקרה קצה).
    let core_len = if trailing_quotes == total_chars {
        total_chars
    } else {
        total_chars - trailing_quotes
    };

    // כל תו מוסיף את ATTACHED_MARKS_CLASS (~25 בתים) ואולי מחלקת גרש/גרשיים;
    // הקצאה מראש נדיבה מונעת reallocations בלולאה.
    let mut out = String::with_capacity(word.len() * 30);
    let mut prev_was_letter = false;
    for ch in word.chars().take(core_len) {
        // בין שתי אותיות עבריות רצופות — גרשיים אופציונליים, בדיוק כמו
        // charwise_display_pattern: בלעדיהם "רשי" הדגיש את רש״י בגוף הספר
        // אך החלונית הציגה "אין תוצאות" (Otzaria/otzaria#1054).
        if prev_was_letter && matches!(ch, 'א'..='ת') {
            out.push_str(OPTIONAL_QUOTES);
        }
        prev_was_letter = matches!(ch, 'א'..='ת');
        if is_query_quote(ch) {
            push_quote_class(&mut out, ch);
        } else {
            push_escaped_char(&mut out, ch);
        }
        out.push_str(ATTACHED_MARKS_CLASS);
    }
    if core_len < total_chars {
        out.push_str("(?=");
        for ch in word.chars().skip(core_len) {
            push_quote_class(&mut out, ch);
        }
        out.push(')');
    }
    out
}

// ── Tests ──────────────────────────────────────────────────────────────────

