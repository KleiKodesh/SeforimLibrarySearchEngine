//! Port of FtsLib `GrammarExpander.cs` (KitveiHakodesh, git HEAD) — candidate
//! generation only.
//!
//! FtsLib generates prefix/suffix candidate forms and verifies each with an
//! exact lookup against its term index. Here the candidates feed a byte-trie
//! automaton instead ([`crate::set_automaton::SetAutomaton`]), and the
//! verification is the FST walk itself: the term dictionary yields exactly the
//! candidates that exist in the index.
//!
//! Syntax (as parsed by FtsLib's QueryParser):
//!   `%word`  — prefix expansion only:  {prefix+word} ∪ {word}
//!   `word%`  — suffix expansion only:  {word+suffix} ∪ {word}
//!   `%word%` — full expansion: {prefix+word} ∪ {word+suffix} ∪
//!              {prefix+word+suffix} ∪ {word}

use std::collections::BTreeSet;

/// All valid stacked Hebrew grammatical prefixes (תחיליות), longest-first.
///
/// Sources: standard Biblical/Rabbinic Hebrew grammar.
///   Single: ו ב כ ל מ ש ה + ד for Aramaic; doubles, triples and quads as in
///   FtsLib. The definite article ה assimilates in speech but is written
///   separately in unvocalised text, so all ה-combinations are included.
pub const PREFIXES: [&str; 34] = [
    // quad
    "ולכש", "ומהש", "ובשה", // triple
    "ובש", "ולש", "וכש", "ומש", "ולכ", "ומה", "ושה", "ולה", "וכה", "מהש", "לכש",
    // double
    "וב", "וכ", "ול", "ום", "וש", "וה", "מה", "שה", "לה", "כש", "מש", "לכ",
    // single
    "ד", "מ", "ש", "ה", "ו", "כ", "ל", "ב",
];

/// Pronominal, plural/gender, construct and verbal suffixes (סופיות),
/// longest-first (as in FtsLib).
pub const SUFFIXES: [&str; 32] = [
    // length 4 (bytes: 3 letters)
    "יכם", "יכן", "יהם", "יהן", // length 3
    "ימו", "ינו", "ותי", "ותם", "ותן", "יות", "כם", "כן", "הם", "הן", "תם", "תן",
    // length 2
    "ים", "ות", "ין", "ית", "ון", "יו", "יה", "יך", "יי", "תי", "נו", // length 1
    "י", "ך", "ו", "ה", "ת",
];

/// If the last character is a Hebrew sofit (final) letter, replaces it with
/// the corresponding regular form: ך→כ ם→מ ן→נ ף→פ ץ→צ. A suffix always
/// continues the word — e.g. זקן + ה = זקנה (not זקןה).
pub fn normalize_final_letter(word: &str) -> String {
    let mut chars: Vec<char> = word.chars().collect();
    if let Some(last) = chars.last_mut() {
        let replacement = match *last {
            '\u{05DA}' => '\u{05DB}', // ך → כ
            '\u{05DD}' => '\u{05DE}', // ם → מ
            '\u{05DF}' => '\u{05E0}', // ן → נ
            '\u{05E3}' => '\u{05E4}', // ף → פ
            '\u{05E5}' => '\u{05E6}', // ץ → צ
            _ => return word.to_string(),
        };
        *last = replacement;
    }
    chars.into_iter().collect()
}

/// Generates all candidate forms. Always includes the bare word itself.
///
/// When appending suffixes, the word's final letter is normalised from its
/// sofit form to its regular form before concatenation (see
/// [`normalize_final_letter`]); prefix-only forms keep the original word.
pub fn build_candidates(
    word: &str,
    expand_prefixes: bool,
    expand_suffixes: bool,
) -> BTreeSet<String> {
    let mut candidates = BTreeSet::new();
    if word.is_empty() {
        return candidates;
    }
    candidates.insert(word.to_string());

    // Stem used when appending suffixes.
    let stem = if expand_suffixes {
        normalize_final_letter(word)
    } else {
        word.to_string()
    };

    if expand_prefixes {
        for prefix in PREFIXES {
            candidates.insert(format!("{prefix}{word}"));
        }
    }
    if expand_suffixes {
        for suffix in SUFFIXES {
            candidates.insert(format!("{stem}{suffix}"));
        }
    }
    if expand_prefixes && expand_suffixes {
        for prefix in PREFIXES {
            for suffix in SUFFIXES {
                candidates.insert(format!("{prefix}{stem}{suffix}"));
            }
        }
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_word_always_included() {
        for (p, s) in [(false, false), (true, false), (false, true), (true, true)] {
            assert!(build_candidates("תורה", p, s).contains("תורה"));
        }
    }

    #[test]
    fn no_flags_only_bare_word() {
        let c = build_candidates("תורה", false, false);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn prefix_only() {
        let c = build_candidates("תורה", true, false);
        assert_eq!(c.len(), 1 + PREFIXES.len());
        assert!(c.contains("התורה"));
        assert!(c.contains("ולכשתורה"));
    }

    #[test]
    fn suffix_uses_sofit_normalized_stem() {
        let c = build_candidates("זקן", false, true);
        assert!(c.contains("זקן")); // bare word keeps sofit
        assert!(c.contains("זקנה")); // ן → נ before suffix
        assert!(!c.contains("זקןה"));
    }

    #[test]
    fn full_expansion_prefix_keeps_original_word() {
        let c = build_candidates("זקן", true, true);
        assert!(c.contains("הזקן")); // prefix + original (sofit kept)
        assert!(c.contains("הזקנים")); // prefix + stem + suffix
        assert!(c.contains("זקנים")); // stem + suffix
        // counts: bare + prefixes + suffixes + prefix×suffix, minus overlaps
        assert!(c.len() <= 1 + PREFIXES.len() + SUFFIXES.len() + PREFIXES.len() * SUFFIXES.len());
    }

    #[test]
    fn empty_word_no_candidates() {
        assert!(build_candidates("", true, true).is_empty());
    }
}
