//! Core-word variant generators for the Otzaria walk options.
//!
//! Each generator expands a set of word variants; the walk composes the
//! enabled generators and feeds the result into the core trie. The index walk
//! itself verifies existence, so over-generation only costs trie size.

use std::collections::{BTreeSet, HashSet};

/// Cap on כתיב מלא/חסר variants of one form (Otzaria `MAX_SPELLING_BRANCHES`).
pub const KTIV_VARIANTS_CAP: usize = 32;

/// The same cap when the variants fan through grammatical prefixes/suffixes or
/// Aramaic prefixes (Otzaria `MAX_GRAMMATICAL_SPELLING_BRANCHES`).
pub const KTIV_GRAMMATICAL_VARIANTS_CAP: usize = 16;

/// The quote bytes a word can carry (the index folds ׳/״ to them).
#[inline]
fn is_quote(c: char) -> bool {
    matches!(c, '\'' | '"' | '\u{05F3}' | '\u{05F4}')
}

/// כתיב מלא/חסר, a port of Otzaria's `generate_spelling_variations`
/// (hebrew_query.rs): up to `limit` variants by rule-based edits of י/ו, the
/// word first, then one edit, two edits, … (deletions before insertions,
/// in the word's order), so a cap keeps the closest forms.
///
/// * Deletion: only a י/ו that is not at the edge of the word, at most one
///   per run of the same letter (מצוות→מצות, never מצת); in a run touching
///   the edge its inner member may go (וורד→ורד). No deletion of the י/ו
///   before a final ה (מצוה ↛ מצה).
/// * Insertion: י or ו inside the word only, at most one per gap, and two
///   inserted letters never end up adjacent (מצת ↛ מצוות; מצות → מצוות is
///   fine: the new ו is next to an existing one). A word of three letters
///   only takes ו after its first letter (כהן→כוהן; שבת ↛ שבות/שבית);
///   shorter words take none.
/// * At most 3 edits of י and 3 of ו per variant; never the same letter both
///   deleted and inserted (קודש ↛ קדוש); a deletion never leaves fewer than 3
///   letters (quotes not counted).
/// * A geresh/gershayim may be dropped anywhere, uncounted (רמב"ם → רמבם).
pub fn ktiv_male_haser_variants_limited(word: &str, limit: usize) -> Vec<String> {
    const MIN_SPELLING_LETTERS: usize = 3;
    const MIN_SPELLING_INSERTION_LETTERS: usize = 4;
    const MAX_EDITS_PER_LETTER: usize = 3;
    const MAX_EDIT_CANDIDATES: usize = 24;

    #[derive(Clone, Copy)]
    enum Edit {
        Del(usize),
        Ins(usize, char),
    }

    if word.is_empty() {
        return vec![String::new()];
    }
    if limit == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = word.chars().collect();
    let n = chars.len();
    if n >= 64 {
        return vec![word.to_string()];
    }
    let letters = chars.iter().filter(|&&c| !is_quote(c)).count();

    let mut edits: Vec<Edit> = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            'י' | 'ו' => {
                let run_start = i == 0 || chars[i - 1] != c;
                if run_start {
                    if let Some(del_at) = (i..n)
                        .take_while(|&j| chars[j] == c)
                        .find(|&j| j > 0 && j + 1 < n)
                        .filter(|&j| !(j + 2 == n && chars[n - 1] == 'ה'))
                    {
                        edits.push(Edit::Del(del_at));
                    }
                }
            }
            c if is_quote(c) => edits.push(Edit::Del(i)),
            _ => {}
        }
    }
    if letters >= MIN_SPELLING_INSERTION_LETTERS {
        for gap in 1..n {
            edits.push(Edit::Ins(gap, 'ו'));
            edits.push(Edit::Ins(gap, 'י'));
        }
    } else if letters == MIN_SPELLING_LETTERS && n == letters {
        edits.push(Edit::Ins(1, 'ו'));
    }
    edits.truncate(MAX_EDIT_CANDIDATES);
    let m = edits.len();

    let valid = |idx: &[usize]| -> bool {
        let (mut yod, mut vav) = (0usize, 0usize);
        let mut deleted = [false; 64];
        let mut deleted_letters = 0usize;
        let mut gaps: Vec<usize> = Vec::new();
        let mut removed: Vec<char> = Vec::new();
        let mut added: Vec<char> = Vec::new();
        for &i in idx {
            match edits[i] {
                Edit::Del(pos) => {
                    deleted[pos] = true;
                    removed.push(chars[pos]);
                    match chars[pos] {
                        'י' => yod += 1,
                        'ו' => vav += 1,
                        _ => {}
                    }
                    if !is_quote(chars[pos]) {
                        deleted_letters += 1;
                    }
                }
                Edit::Ins(gap, letter) => {
                    gaps.push(gap);
                    added.push(letter);
                    if letter == 'י' {
                        yod += 1;
                    } else {
                        vav += 1;
                    }
                }
            }
        }
        if yod > MAX_EDITS_PER_LETTER || vav > MAX_EDITS_PER_LETTER {
            return false;
        }
        if removed.iter().any(|c| added.contains(c)) {
            return false;
        }
        if letters - deleted_letters < MIN_SPELLING_LETTERS {
            return false;
        }
        gaps.windows(2).all(|pair| {
            let (g1, g2) = (pair[0], pair[1]);
            g1 != g2 && !(g1..g2).all(|p| deleted[p])
        })
    };

    let apply = |idx: &[usize]| -> String {
        let mut deleted = [false; 64];
        let mut inserts: Vec<(usize, char)> = Vec::new();
        for &i in idx {
            match edits[i] {
                Edit::Del(pos) => deleted[pos] = true,
                Edit::Ins(gap, letter) => inserts.push((gap, letter)),
            }
        }
        let mut out = String::with_capacity(word.len() + inserts.len() * 2);
        for i in 0..=n {
            for &(gap, letter) in &inserts {
                if gap == i {
                    out.push(letter);
                }
            }
            if i < n && !deleted[i] {
                out.push(chars[i]);
            }
        }
        out
    };

    let mut out = vec![word.to_string()];
    let mut seen: HashSet<String> = HashSet::from([word.to_string()]);
    let max_k = m.min(2 * MAX_EDITS_PER_LETTER + 2);
    'sizes: for k in 1..=max_k {
        if out.len() >= limit {
            break;
        }
        let mut idx: Vec<usize> = (0..k).collect();
        loop {
            if valid(&idx) {
                let v = apply(&idx);
                if seen.insert(v.clone()) {
                    out.push(v);
                    if out.len() >= limit {
                        break 'sizes;
                    }
                }
            }
            let mut i = k;
            loop {
                if i == 0 {
                    continue 'sizes;
                }
                i -= 1;
                if idx[i] < m - k + i {
                    idx[i] += 1;
                    for j in i + 1..k {
                        idx[j] = idx[j - 1] + 1;
                    }
                    break;
                }
            }
        }
    }
    out
}

/// [`ktiv_male_haser_variants_limited`] at [`KTIV_VARIANTS_CAP`], as a set.
pub fn ktiv_male_haser_variants(word: &str) -> BTreeSet<String> {
    ktiv_male_haser_variants_limited(word, KTIV_VARIANTS_CAP)
        .into_iter()
        .collect()
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

// ── Otzaria's sampled typo list (hebrew_query.rs :1686-1820) ──

/// Otzaria `typo_substitutions`: the letter confusions a typo may make.
pub fn typo_substitutions(c: char) -> &'static [char] {
    match c {
        'א' => &['ע', 'ה'],
        'ע' => &['א', 'ה'],
        'ה' => &['א', 'ע', 'ח'],
        'ח' => &['ה'],
        'ו' => &['ב'],
        'ב' => &['ו'],
        'כ' => &['ק'],
        'ק' => &['כ'],
        'ט' => &['ת'],
        'ת' => &['ט'],
        'ס' => &['ש'],
        'ש' => &['ס'],
        'צ' => &['ז'],
        'ז' => &['צ'],
        _ => &[],
    }
}

/// Otzaria `INSERTION_LETTERS`: the letters its sampled list inserts
/// (ordered by frequency).
const TYPO_INSERTION_LETTERS: [char; 13] =
    ['ו', 'י', 'א', 'ה', 'פ', 'ל', 'מ', 'נ', 'ב', 'כ', 'ש', 'ת', 'ר'];

/// Otzaria `typo_letter_count`: letters a typo may change, quotes and
/// marks not counted.
pub fn typo_letter_count(word: &str) -> usize {
    word.chars()
        .filter(|&c| !is_quote(c) && !crate::chars::is_nikud(c) && !crate::chars::is_taam(c))
        .count()
}

/// Otzaria `generate_common_typo_variations`: the word, its letter
/// confusions, then its adjacent transpositions.
fn common_typo_variations(chars: &[char], out: &mut Vec<String>, seen: &mut HashSet<String>) {
    let mut push = |v: String| {
        if seen.insert(v.clone()) {
            out.push(v);
        }
    };
    push(chars.iter().collect());
    for (i, &c) in chars.iter().enumerate() {
        for &sub in typo_substitutions(c) {
            let mut v = chars.to_vec();
            v[i] = sub;
            push(v.iter().collect());
        }
    }
    for i in 0..chars.len().saturating_sub(1) {
        if chars[i] != chars[i + 1] {
            let mut v = chars.to_vec();
            v.swap(i, i + 1);
            push(v.iter().collect());
        }
    }
}

/// Otzaria `generate_typo_variations`, original first, capped at `max`:
/// nothing for ≤2 letters, the letter confusions for 3, and for ≥4 the
/// confusions and transpositions, then single deletions, then single
/// insertions of [`TYPO_INSERTION_LETTERS`] (word start and end first, then
/// the interior). Quotes are characters a deletion may drop (רמב"ם → רמבם).
/// Its characters are Otzaria's graphemes on a mark-free word.
pub fn otzaria_typo_variations(word: &str, max: usize) -> Vec<String> {
    if word.is_empty() {
        return vec![String::new()];
    }
    let letters = typo_letter_count(word);
    if letters < 3 {
        return vec![word.to_string()];
    }
    let chars: Vec<char> = word.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    if letters < 4 {
        out.push(word.to_string());
        seen.insert(word.to_string());
        for (i, &c) in chars.iter().enumerate() {
            for &sub in typo_substitutions(c) {
                let mut v = chars.clone();
                v[i] = sub;
                let v: String = v.iter().collect();
                if seen.insert(v.clone()) {
                    out.push(v);
                }
            }
        }
        out.truncate(max.max(1));
        return out;
    }
    let mut common = Vec::new();
    common_typo_variations(&chars, &mut common, &mut HashSet::new());
    let mut add = |v: String, out: &mut Vec<String>| -> bool {
        if v.is_empty() || seen.contains(&v) {
            return true;
        }
        seen.insert(v.clone());
        out.push(v);
        out.len() < max
    };
    for v in common {
        if !add(v, &mut out) {
            return out;
        }
    }
    for i in 0..chars.len() {
        let mut v = chars.clone();
        v.remove(i);
        if !v.is_empty() && !add(v.iter().collect(), &mut out) {
            return out;
        }
    }
    let mut positions = vec![0, chars.len()];
    positions.extend(1..chars.len());
    for pos in positions {
        for letter in TYPO_INSERTION_LETTERS {
            let mut v = chars.clone();
            v.insert(pos, letter);
            if !add(v.iter().collect(), &mut out) {
                return out;
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

    fn has(word: &str, variant: &str) -> bool {
        ktiv_male_haser_variants_limited(word, 1000).contains(&variant.to_string())
    }

    #[test]
    fn ktiv_insert_and_remove() {
        let v = ktiv_male_haser_variants("מצות");
        assert!(v.contains("מצות"));
        assert!(v.contains("מצוות")); // insertion of ו
        let v2 = ktiv_male_haser_variants("מצוות");
        assert!(v2.contains("מצות")); // removal of ו
        assert!(v.len() <= KTIV_VARIANTS_CAP);
    }

    // Otzaria's spelling_variations_original_first_then_single_edits.
    #[test]
    fn ktiv_original_first_then_single_edits() {
        let v = ktiv_male_haser_variants_limited("שלום", 100);
        assert_eq!(v[0], "שלום");
        assert_eq!(v[1], "שלם");
        assert!(v.contains(&"שלוום".to_string()));
        assert_eq!(ktiv_male_haser_variants_limited("שלום", 1), vec!["שלום"]);
    }

    // Otzaria's spelling_variations_follow_the_male_haser_rules.
    #[test]
    fn ktiv_follows_the_male_haser_rules() {
        assert!(has("מצות", "מצוות"));
        assert!(has("מצוות", "מצות"));
        assert!(!has("מצת", "מצוות"));
        assert!(!has("מצוות", "מצת"));
        // Otzaria's rules DO give מצות → מצת: one internal ו deleted, 3 letters left.
        assert!(has("מצות", "מצת"));
        assert!(!has("בוא", "בייא"));
        assert!(!has("אותו", "אות"));
        assert!(ktiv_male_haser_variants_limited("ויהי", 1000)
            .iter()
            .all(|v| v.starts_with('ו')));
        assert!(has("וורד", "ורד"));
        assert!(!ktiv_male_haser_variants_limited("גמל", 1000)
            .iter()
            .any(|v| v.starts_with('ו') || v.starts_with('י') || v.ends_with('ו') || v.ends_with('י')));
        assert!(has("רמב\"ם", "רמבם"));
        assert!(has("תוס'", "תוס"));
    }

    // Otzaria's spelling_variations_never_make_another_word.
    #[test]
    fn ktiv_never_makes_another_word() {
        for v in ["שבות", "שבית"] {
            assert!(!has("שבת", v), "{v}");
        }
        assert!(has("שבת", "שובת"));
        assert!(has("כהן", "כוהן"));
        assert!(has("קדש", "קודש"));
        for v in ["כיהן", "כהון", "כוהון", "כויהן"] {
            assert!(!has("כהן", v), "{v}");
        }
        assert!(!has("שלום", "שולים"));
        assert!(!has("שור", "שר"));
        assert!(!has("מצוה", "מצה"));
        assert!(!has("שתיה", "שתה"));
        assert!(has("מצוה", "מצווה"));
        assert!(!has("קודש", "קדוש"));
        assert!(has("קודש", "קדש"));
        assert!(has("תפלה", "תפילה"));
        assert!(has("תפילה", "תפלה"));
    }

    // Otzaria's spelling_variations_cap_edits_per_letter.
    #[test]
    fn ktiv_caps_edits_per_letter() {
        for v in ktiv_male_haser_variants_limited("אבגדהזחט", 10_000) {
            assert!(v.matches('ו').count() <= 3, "{v}");
            assert!(v.matches('י').count() <= 3, "{v}");
        }
    }

    #[test]
    fn ktiv_never_touches_edges() {
        let v = ktiv_male_haser_variants("ושב");
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

    // Otzaria typo_tolerance_scales_with_word_length (the list part).
    #[test]
    fn otzaria_typo_list_scales_with_word_length() {
        assert_eq!(otzaria_typo_variations("אב", 100), vec!["אב"]);
        let three = otzaria_typo_variations("שבת", 100);
        for v in ["שבת", "סבת", "שות", "שבט"] {
            assert!(three.contains(&v.to_string()), "{v}");
        }
        for v in ["שב", "שבתי", "בשת", "ששבת"] {
            assert!(!three.contains(&v.to_string()), "{v}");
        }
        let four = otzaria_typo_variations("ברכה", 500);
        for v in ["ברכ", "רבכה", "בברכה", "ברקה"] {
            assert!(four.contains(&v.to_string()), "{v}");
        }
        assert!(!four.contains(&"ברכגה".to_string())); // ג is not an insertion letter
        assert!(!four.contains(&"ברזה".to_string())); // כ↔ז is not a confusion
        assert_eq!(typo_letter_count("ר\"ת"), 2);
    }

    // Otzaria common_typo_covers_substitution_and_transposition.
    #[test]
    fn otzaria_common_typos() {
        let chars: Vec<char> = "בא".chars().collect();
        let mut v = Vec::new();
        common_typo_variations(&chars, &mut v, &mut HashSet::new());
        assert_eq!(v[0], "בא");
        for w in ["וא", "בע", "בה", "אב"] {
            assert!(v.contains(&w.to_string()), "{w}");
        }
    }

    // Otzaria typo_variations_respect_cap; original first.
    #[test]
    fn otzaria_typo_list_respects_cap() {
        let v = otzaria_typo_variations("ברכות", 10);
        assert_eq!(v.len(), 10);
        assert_eq!(v[0], "ברכות");
        assert!(otzaria_typo_variations("ברכות", 128).len() <= 128);
        assert!(otzaria_typo_variations("רמב\"ם", 128).contains(&"רמבם".to_string()));
    }
}
