//! Core-word variant generators for the Otzaria walk options.
//!
//! Each generator expands a set of word variants; the walk composes the
//! enabled generators and feeds the result into the core trie. The index walk
//! itself verifies existence, so over-generation only costs trie size.

use std::collections::BTreeSet;

/// Cap on variants produced by ktiv male/haser expansion (per the spec: עד 32).
pub const KTIV_VARIANTS_CAP: usize = 32;

/// Cap on variants produced by the spelling/OCR confusion expansion.
pub const SPELLING_VARIANTS_CAP: usize = 64;

/// כתיב מלא/חסר: insertion or removal of י/ו strictly INSIDE the word (never
/// at the first or last letter), up to [`KTIV_VARIANTS_CAP`] variants,
/// breadth-first by edit count so single edits win the cap. מצות ↔ מצוות.
pub fn ktiv_male_haser_variants(word: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let chars: Vec<char> = word.chars().collect();
    if chars.len() < 2 {
        out.insert(word.to_string());
        return out;
    }
    let mut frontier: std::collections::VecDeque<Vec<char>> = std::collections::VecDeque::new();
    frontier.push_back(chars);
    out.insert(word.to_string());
    while let Some(current) = frontier.pop_front() {
        if out.len() >= KTIV_VARIANTS_CAP {
            break;
        }
        let n = current.len();
        // Removal of an internal י/ו (positions 1..n-1).
        for i in 1..n.saturating_sub(1) {
            if current[i] == 'י' || current[i] == 'ו' {
                let mut v = current.clone();
                v.remove(i);
                let s: String = v.iter().collect();
                if out.insert(s) {
                    frontier.push_back(v);
                    if out.len() >= KTIV_VARIANTS_CAP {
                        return out;
                    }
                }
            }
        }
        // Insertion of י/ו at internal gaps (between letters — never before
        // the first letter or after the last).
        for i in 1..n {
            for c in ['י', 'ו'] {
                let mut v = current.clone();
                v.insert(i, c);
                let s: String = v.iter().collect();
                if out.insert(s) {
                    frontier.push_back(v);
                    if out.len() >= KTIV_VARIANTS_CAP {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// סיומות ארמיות: ending equivalence — final ה↔א (מלכה↔מלכא) and final ם↔ן
/// (חכמים↔חכמין). Returns the word plus its alternate ending when applicable.
pub fn aramaic_ending_variants(word: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.insert(word.to_string());
    let mut chars: Vec<char> = word.chars().collect();
    if let Some(last) = chars.last().copied() {
        let alt = match last {
            'ה' => Some('א'),
            'א' => Some('ה'),
            'ם' => Some('ן'),
            'ן' => Some('ם'),
            _ => None,
        };
        if let Some(alt) = alt {
            *chars.last_mut().unwrap() = alt;
            out.insert(chars.iter().collect());
        }
    }
    out
}

/// Common Hebrew misspelling / OCR confusion pairs. Tunable; each pair works
/// in both directions, one substitution per variant.
///
/// Spelling (phonetic): א↔ע, ת↔ט, כ↔ק, ס↔ש, ח↔כ
/// OCR (shape): ד↔ר, ה↔ח, ו↔ן, ב↔כ, י↔ו
pub const CONFUSION_PAIRS: [(char, char); 10] = [
    // phonetic
    ('א', 'ע'),
    ('ת', 'ט'),
    ('כ', 'ק'),
    ('ס', 'ש'),
    ('ח', 'כ'),
    // OCR shape
    ('ד', 'ר'),
    ('ה', 'ח'),
    ('ו', 'ן'),
    ('ב', 'כ'),
    ('י', 'ו'),
];

/// שגיאות כתיב/OCR: single-substitution variants over [`CONFUSION_PAIRS`],
/// capped at [`SPELLING_VARIANTS_CAP`]. The original word is always included.
pub fn spelling_variants(word: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    out.insert(word.to_string());
    let chars: Vec<char> = word.chars().collect();
    'outer: for i in 0..chars.len() {
        for (a, b) in CONFUSION_PAIRS {
            let alt = if chars[i] == a {
                Some(b)
            } else if chars[i] == b {
                Some(a)
            } else {
                None
            };
            if let Some(alt) = alt {
                let mut v = chars.clone();
                v[i] = alt;
                out.insert(v.iter().collect());
                if out.len() >= SPELLING_VARIANTS_CAP {
                    break 'outer;
                }
            }
        }
    }
    out
}

/// Applies a generator to every member of a set, unioning the results.
pub fn expand_set(
    set: &BTreeSet<String>,
    generator: impl Fn(&str) -> BTreeSet<String>,
) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for word in set {
        out.extend(generator(word));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ktiv_insert_and_remove() {
        let v = ktiv_male_haser_variants("מצות");
        assert!(v.contains("מצות"));
        assert!(v.contains("מצוות")); // insertion of ו
        let v2 = ktiv_male_haser_variants("מצוות");
        assert!(v2.contains("מצות")); // removal of ו
        assert!(v.len() <= KTIV_VARIANTS_CAP);
    }

    #[test]
    fn ktiv_never_touches_edges() {
        let v = ktiv_male_haser_variants("ושב");
        // no removal of the leading ו, no insertion before it or after ב...
        assert!(!v.contains("שב"));
        for s in &v {
            assert!(s.starts_with('ו'), "{s}");
            assert!(s.ends_with('ב'), "{s}");
        }
    }

    #[test]
    fn aramaic_endings() {
        assert!(aramaic_ending_variants("מלכה").contains("מלכא"));
        assert!(aramaic_ending_variants("מלכא").contains("מלכה"));
        assert!(aramaic_ending_variants("חכמים").contains("חכמין"));
        assert!(aramaic_ending_variants("חכמין").contains("חכמים"));
        assert_eq!(aramaic_ending_variants("ספר").len(), 1);
    }

    #[test]
    fn spelling_substitutions() {
        let v = spelling_variants("אבד");
        assert!(v.contains("עבד")); // א↔ע
        assert!(v.contains("אבר")); // ד↔ר
        assert!(v.contains("אכד")); // ב↔כ
        assert!(v.contains("אבד"));
    }
}
