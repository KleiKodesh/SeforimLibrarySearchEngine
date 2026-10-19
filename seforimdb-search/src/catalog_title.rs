//! Port of the index-side `SeforimDbCatalogTextNormalizer.Tokenize` and its
//! helpers, as far as `SeforimDbBookTreeOrder.IsBaseText` needs them (book
//! title vs folder title equality).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::abbreviations::MAP;

fn abbrev() -> &'static HashMap<&'static str, &'static [&'static [&'static str]]> {
    static ABBREV: OnceLock<HashMap<&'static str, &'static [&'static [&'static str]]>> =
        OnceLock::new();
    ABBREV.get_or_init(|| MAP.iter().copied().collect())
}

/// Largest key word-count in the map: the lookahead window for greedy
/// multi-word matching.
fn max_key_words() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        MAP.iter()
            .map(|(k, _)| k.split(' ').count())
            .max()
            .unwrap_or(1)
    })
}

/// Non-abbreviation spelling normalizations (כתיב חסר → מלא).
fn fold_spelling(clean: &str) -> &str {
    match clean {
        "שלחן" => "שולחן",
        other => other,
    }
}

/// Counter words: a 1-4 Hebrew-letter token after one of these is a numeral.
const COUNTER_WORDS: &[&str] = &[
    "פרק", "סימן", "דף", "הלכה", "סעיף", "עמוד", "אות", "פסוק", "משנה", "סק", "קטן", "כלל", "שער",
    "חלק", "מצוה", "מאמר", "רמז", "דין", "דרוש", "מזמור", "פסקא", "פסקה", "תלמוד",
];

fn counter_words() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| COUNTER_WORDS.iter().copied().collect())
}

/// The " / " joiner of a stored TOC path, as the whitespace split sees it.
const PATH_SEPARATOR: &str = "/";

fn is_quote_glyph(c: char) -> bool {
    matches!(c, '"' | '\'' | '״' | '׳' | '“' | '”' | '‘' | '’')
}

/// Remove every quote glyph from a token.
pub fn strip_quote_glyphs(s: &str) -> String {
    s.chars().filter(|&c| !is_quote_glyph(c)).collect()
}

fn has_quote_glyph(raw: &str) -> bool {
    raw.chars().any(is_quote_glyph)
}

fn is_hebrew_letter(c: char) -> bool {
    ('א'..='ת').contains(&c)
}

fn is_word_char(c: char) -> bool {
    c.is_alphabetic() || c.is_numeric()
}

fn trim_edge_non_word(s: &str) -> &str {
    s.trim_matches(|c: char| !is_word_char(c))
}

/// `ה` + a remainder of ≥2 chars → the remainder.
fn strip_he_prefix(word: &str) -> Option<String> {
    let mut chars = word.chars();
    if chars.next() != Some('ה') {
        return None;
    }
    let remainder: String = chars.collect();
    if remainder.chars().count() >= 2 {
        Some(remainder)
    } else {
        None
    }
}

/// True when raw token `i` is a numeral position: it follows a counter word
/// (with or without a leading he) and is 1-4 Hebrew letters once quote glyphs
/// and edge punctuation are stripped; or it carries a daf/reference mark.
fn is_numeral_after_counter(raws: &[&str], i: usize) -> bool {
    let raw = raws[i];
    let tok = strip_quote_glyphs(trim_edge_non_word(raw));
    let len = tok.chars().count();
    if len == 0 || len > 4 {
        return false;
    }
    if !tok.chars().all(is_hebrew_letter) {
        return false;
    }
    let quoted = has_quote_glyph(raw);
    if len <= (if quoted { 2 } else { 3 })
        && (raw.ends_with('.') || raw.ends_with(':') || raw.starts_with('(') || raw.ends_with(')'))
    {
        return true;
    }
    if i == 0 {
        return false;
    }
    let mut prev = strip_quote_glyphs(trim_edge_non_word(raws[i - 1]));
    if prev.chars().count() > 1 && prev.starts_with('ה') {
        let rest: String = prev.chars().skip(1).collect();
        if counter_words().contains(rest.as_str()) {
            prev = rest;
        }
    }
    counter_words().contains(prev.as_str())
}

fn try_lookup(candidate: &str) -> Option<&'static [&'static [&'static str]]> {
    abbrev().get(strip_quote_glyphs(candidate).as_str()).copied()
}

/// Greedy longest-match abbreviation lookup starting at raw token `start`.
/// Index side: a lone word is an abbreviation only when it carries a quote
/// glyph. Returns (alternatives, raw tokens consumed).
fn try_match_abbrev(raws: &[&str], start: usize) -> Option<(&'static [&'static [&'static str]], usize)> {
    if raws[start] == PATH_SEPARATOR {
        return None;
    }
    let mut max_window = max_key_words().min(raws.len() - start);
    for i in 1..max_window {
        if raws[start + i] == PATH_SEPARATOR {
            max_window = i;
            break;
        }
    }
    for w in (1..=max_window).rev() {
        if w == 1 && !has_quote_glyph(raws[start]) {
            continue;
        }
        let candidate = raws[start..start + w].join(" ");
        if let Some(alts) = try_lookup(&candidate) {
            return Some((alts, w));
        }
        let trimmed = trim_edge_non_word(&candidate);
        if !trimmed.is_empty() && trimmed.len() != candidate.len() {
            if let Some(alts) = try_lookup(trimmed) {
                return Some((alts, w));
            }
        }
        if let Some(he_stripped) = strip_he_prefix(raws[start]) {
            let he_candidate = if w == 1 {
                he_stripped
            } else {
                format!("{} {}", he_stripped, raws[start + 1..start + w].join(" "))
            };
            if let Some(alts) = try_lookup(&he_candidate) {
                return Some((alts, w));
            }
            let he_trimmed = trim_edge_non_word(&he_candidate);
            if !he_trimmed.is_empty() && he_trimmed.len() != he_candidate.len() {
                if let Some(alts) = try_lookup(he_trimmed) {
                    return Some((alts, w));
                }
            }
        }
    }
    None
}

/// The amud + strip + spelling-fold steps for a single non-abbreviation
/// token.
fn emit_normalized_token(raw: &str, tokens: &mut Vec<String>) {
    let mut tok = raw;
    let mut amud: Option<&str> = None;
    // Right after a דף / תלמוד token, a trailing "." means עמוד א and ":" עמוד ב.
    if let Some(last) = tokens.last() {
        if (last == "דף" || last == "תלמוד") && tok.chars().count() > 1 {
            if tok.ends_with('.') {
                amud = Some("א");
            } else if tok.ends_with(':') {
                amud = Some("ב");
            }
            if amud.is_some() {
                tok = &tok[..tok.len() - 1];
            }
        }
    }
    let clean: String = tok
        .chars()
        .filter(|&c| is_word_char(c))
        .flat_map(|c| c.to_lowercase())
        .collect();
    if !clean.is_empty() {
        tokens.push(fold_spelling(&clean).to_string());
    } else {
        amud = None;
    }
    if let Some(a) = amud {
        tokens.push("עמוד".to_string());
        tokens.push(a.to_string());
    }
}

/// The index-side tokenizer: whitespace split, abbreviation expansion to the
/// first alternative, amud marks, non-word stripping, lowercasing, spelling
/// fold.
pub fn tokenize(text: &str) -> Vec<String> {
    let raws: Vec<&str> = text.split_whitespace().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < raws.len() {
        if !is_numeral_after_counter(&raws, i) {
            if let Some((alts, consumed)) = try_match_abbrev(&raws, i) {
                tokens.extend(alts[0].iter().map(|s| s.to_string()));
                i += consumed;
                continue;
            }
        }
        emit_normalized_token(raws[i], &mut tokens);
        i += 1;
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words() {
        assert_eq!(tokenize("שולחן ערוך"), ["שולחן", "ערוך"]);
        assert_eq!(tokenize("שלחן ערוך"), ["שולחן", "ערוך"]); // spelling fold
    }

    #[test]
    fn quoted_abbreviation_expands_unquoted_does_not() {
        assert_eq!(tokenize("שו\"ע"), ["שולחן", "ערוך"]);
        assert_eq!(tokenize("שוע"), ["שוע"]);
        assert_eq!(tokenize("יד החזקה"), ["משנה", "תורה"]); // multi-word key
    }

    #[test]
    fn numeral_after_counter_is_not_an_abbreviation() {
        assert_eq!(tokenize("סימן ט\"ז"), ["סימן", "טז"]);
    }

    #[test]
    fn amud_marks() {
        assert_eq!(tokenize("דף ב."), ["דף", "ב", "עמוד", "א"]);
        assert_eq!(tokenize("דף ב:"), ["דף", "ב", "עמוד", "ב"]);
    }

    #[test]
    fn punctuation_stripped_and_lowercased() {
        assert_eq!(tokenize("(בראשית) Rabbah"), ["בראשית", "rabbah"]);
    }
}
