//! Placeholder spellings of the divine name (issue #39): a query word written
//! in one spelling of the tetragrammaton also searches the others. Port of
//! Otzaria's smart lexical rule (`otzaria_search_engine` 5babbce,
//! `rust/src/smart_query.rs` `alternative`, the app's
//! `holyNameForPlaceholderWord`), made symmetric: the full spelling
//! U+05D9 U+05D4 U+05D5 U+05D4 and the placeholders U+05D9 U+05E7 U+05D5 U+05E7
//! and U+05D9 U+05D3 U+05D5 U+05D3 are one class. A whole word only, after up
//! to two prefix letters (U+05D5 U+05D1 U+05DB U+05DC U+05DE U+05E9 U+05D4);
//! marks stay on their letters. Same table as FtsLib's `DivineNames.cs`.
//!
//! Always on (user's decision): the hosts' query words get these as
//! alternatives in their slot, so walks, phrases, snippets and highlights
//! follow with no index change.

use crate::chars::is_term_mark;

const PREFIXES: [char; 7] = ['\u{05D5}', '\u{05D1}', '\u{05DB}', '\u{05DC}', '\u{05DE}', '\u{05E9}', '\u{05D4}'];
/// The letter repeated in the 2nd and 4th place: the full spelling's U+05D4
/// and the placeholders U+05E7 / U+05D3.
const CLASS: [char; 3] = ['\u{05D4}', '\u{05E7}', '\u{05D3}'];

/// The other spellings of `word` when it is the divine name in one spelling
/// of the class (with up to two prefixes), the word itself left out; empty
/// otherwise.
pub fn divine_name_alternatives(word: &str) -> Vec<String> {
    // letter clusters: a letter and the marks after it
    let mut clusters: Vec<(char, &str)> = Vec::new();
    let mut iter = word.char_indices().peekable();
    while let Some((i, c)) = iter.next() {
        if is_term_mark(c) {
            return Vec::new(); // a mark with no letter before it
        }
        let start = i + c.len_utf8();
        let mut end = start;
        while let Some(&(j, m)) = iter.peek() {
            if !is_term_mark(m) {
                break;
            }
            end = j + m.len_utf8();
            iter.next();
        }
        clusters.push((c, &word[start..end]));
    }
    let n = clusters.len();
    if !(4..=6).contains(&n) || !clusters[..n - 4].iter().all(|(c, _)| PREFIXES.contains(c)) {
        return Vec::new();
    }
    let core = &clusters[n - 4..];
    let x = core[1].0;
    if core[0].0 != '\u{05D9}' || core[2].0 != '\u{05D5}' || core[3].0 != x || !CLASS.contains(&x) {
        return Vec::new();
    }
    CLASS
        .iter()
        .filter(|&&y| y != x)
        .map(|&y| {
            clusters
                .iter()
                .enumerate()
                .map(|(i, &(c, marks))| {
                    let c = if i == n - 3 || i == n - 1 { y } else { c };
                    format!("{c}{marks}")
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "\u{05D9}\u{05D4}\u{05D5}\u{05D4}";
    const QOF: &str = "\u{05D9}\u{05E7}\u{05D5}\u{05E7}";
    const DALET: &str = "\u{05D9}\u{05D3}\u{05D5}\u{05D3}";

    #[test]
    fn each_spelling_gives_the_other_two() {
        assert_eq!(divine_name_alternatives(QOF), vec![FULL.to_string(), DALET.to_string()]);
        assert_eq!(divine_name_alternatives(DALET), vec![FULL.to_string(), QOF.to_string()]);
        assert_eq!(divine_name_alternatives(FULL), vec![QOF.to_string(), DALET.to_string()]);
    }

    /// Otzaria's smart_query tests: up to two prefixes, marks kept on their
    /// letters, mixed letters and three prefixes are no name.
    #[test]
    fn prefixes_and_marks() {
        let v = '\u{05D5}';
        let l = '\u{05DC}';
        assert_eq!(divine_name_alternatives(&format!("{v}{QOF}"))[0], format!("{v}{FULL}"));
        assert_eq!(divine_name_alternatives(&format!("{v}{l}{DALET}"))[0], format!("{v}{l}{FULL}"));
        assert!(divine_name_alternatives(&format!("{v}{v}{l}{QOF}")).is_empty());
        // shva under the lamed prefix, qamats under the last letter
        let marked = format!("{l}\u{05B0}\u{05D9}\u{05E7}\u{05D5}\u{05E7}\u{05B8}");
        assert_eq!(divine_name_alternatives(&marked)[0], format!("{l}\u{05B0}\u{05D9}\u{05D4}\u{05D5}\u{05D4}\u{05B8}"));
        // mixed placeholder letters, another letter, a prefix outside the set
        assert!(divine_name_alternatives("\u{05D9}\u{05E7}\u{05D5}\u{05D3}").is_empty());
        assert!(divine_name_alternatives("\u{05D9}\u{05DB}\u{05D5}\u{05DB}").is_empty());
        assert!(divine_name_alternatives(&format!("\u{05D0}{QOF}")).is_empty());
        assert!(divine_name_alternatives("\u{05D9}\u{05E7}\u{05D5}").is_empty());
    }
}
