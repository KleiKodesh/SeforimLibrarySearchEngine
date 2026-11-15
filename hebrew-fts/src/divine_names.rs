//! Placeholder spellings of the divine names (issue #39): a query word written
//! in one spelling of a name also searches its other spellings. Started as a
//! port of Otzaria's smart lexical rule (`otzaria_search_engine` 5babbce,
//! `rust/src/smart_query.rs` `alternative`), made symmetric and extended from
//! a survey of seforim.db (user's decision). Two classes:
//!
//! - Tetragrammaton: U+05D9 U+05D4 U+05D5 U+05D4 (full), U+05D9 U+05E7 U+05D5
//!   U+05E7, U+05D9 U+05D3 U+05D5 U+05D3 and the double yod U+05D9 U+05D9.
//! - Elohim: U+05D0 U+05DC [U+05D5] X U+05D9 + suffix, X = U+05D4 (full) or
//!   U+05E7 (placeholder), suffix one of [`ELOHIM_SUFFIXES`]; the optional vav
//!   stays as typed.
//!
//! A whole word only, after up to two prefix letters (U+05D5 U+05D1 U+05DB
//! U+05DC U+05DE U+05E9 U+05D4); marks stay on their letters. Some prefix +
//! form pairs are other real words and are excluded both ways
//! ([`EXCLUDED`]). Same table as FtsLib's `DivineNames.cs`.
//!
//! Always on (user's decision): the query words get these as alternatives in
//! their slot, so walks, phrases, snippets and highlights follow with no
//! index change.

use crate::chars::is_term_mark;

const PREFIXES: [char; 7] = ['\u{05D5}', '\u{05D1}', '\u{05DB}', '\u{05DC}', '\u{05DE}', '\u{05E9}', '\u{05D4}'];
const YOD: char = '\u{05D9}';
const HE: char = '\u{05D4}';
const VAV: char = '\u{05D5}';
const QOF: char = '\u{05E7}';
const DALET: char = '\u{05D3}';

/// The tetragrammaton's spellings, in output order.
const TETRAGRAMMATON: [&[char]; 4] = [&[YOD, HE, VAV, HE], &[YOD, QOF, VAV, QOF], &[YOD, DALET, VAV, DALET], &[YOD, YOD]];

/// The Elohim suffixes after U+05D9 (none, final mem, final kaf, nun-vav,
/// kaf-final mem, he-final mem, vav, he, he-final nun, kaf-final nun).
pub const ELOHIM_SUFFIXES: [&str; 10] = [
    "",
    "\u{05DD}",
    "\u{05DA}",
    "\u{05E0}\u{05D5}",
    "\u{05DB}\u{05DD}",
    "\u{05D4}\u{05DD}",
    "\u{05D5}",
    "\u{05D4}",
    "\u{05D4}\u{05DF}",
    "\u{05DB}\u{05DF}",
];

/// (prefix letter right before the name, spelling): another real word, from
/// the survey: shin + the dalet placeholder; he / kaf + the double yod.
const EXCLUDED: [(char, &[char]); 3] = [('\u{05E9}', &[YOD, DALET, VAV, DALET]), (HE, &[YOD, YOD]), ('\u{05DB}', &[YOD, YOD])];

fn excluded(prefix: &[(char, &str)], core: &[char]) -> bool {
    prefix.last().is_some_and(|&(p, _)| EXCLUDED.iter().any(|&(q, form)| q == p && form == core))
}

/// The other spellings of `word` when it is a divine name in one spelling of
/// its class (with up to two prefixes), the word itself left out; empty
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
    for p in 0..=2.min(clusters.len()) {
        if p > 0 && !PREFIXES.contains(&clusters[p - 1].0) {
            break;
        }
        let (prefix, core) = clusters.split_at(p);
        let letters: Vec<char> = core.iter().map(|c| c.0).collect();
        let found = tetragrammaton(prefix, core, &letters).or_else(|| elohim(prefix, core, &letters));
        if let Some(found) = found {
            return found;
        }
    }
    Vec::new()
}

fn render(prefix: &[(char, &str)], core: impl Iterator<Item = (char, String)>) -> String {
    let mut s: String = prefix.iter().map(|&(c, m)| format!("{c}{m}")).collect();
    for (c, m) in core {
        s.push(c);
        s.push_str(&m);
    }
    s
}

fn tetragrammaton(prefix: &[(char, &str)], core: &[(char, &str)], letters: &[char]) -> Option<Vec<String>> {
    if !TETRAGRAMMATON.contains(&letters) || excluded(prefix, letters) {
        return None;
    }
    let last = core.len() - 1;
    Some(
        TETRAGRAMMATON
            .iter()
            .filter(|&&form| form != letters && !excluded(prefix, form))
            .map(|&form| {
                let n = form.len() - 1;
                render(
                    prefix,
                    form.iter().enumerate().map(|(i, &c)| {
                        // marks by place; between lengths the first and last
                        // letters keep theirs
                        let m = if form.len() == core.len() {
                            core[i].1
                        } else if i == 0 {
                            core[0].1
                        } else if i == n {
                            core[last].1
                        } else {
                            ""
                        };
                        (c, m.to_string())
                    }),
                )
            })
            .collect(),
    )
}

fn elohim(prefix: &[(char, &str)], core: &[(char, &str)], letters: &[char]) -> Option<Vec<String>> {
    if letters.len() < 4 || letters[0] != '\u{05D0}' || letters[1] != '\u{05DC}' {
        return None;
    }
    let x = if letters[2] == VAV { 3 } else { 2 };
    if letters.len() < x + 2 || letters[x + 1] != YOD || !(letters[x] == HE || letters[x] == QOF) {
        return None;
    }
    let suffix: String = letters[x + 2..].iter().collect();
    if !ELOHIM_SUFFIXES.contains(&suffix.as_str()) {
        return None;
    }
    let other = if letters[x] == HE { QOF } else { HE };
    Some(vec![render(
        prefix,
        core.iter().enumerate().map(|(i, &(c, m))| (if i == x { other } else { c }, m.to_string())),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "\u{05D9}\u{05D4}\u{05D5}\u{05D4}";
    const Q: &str = "\u{05D9}\u{05E7}\u{05D5}\u{05E7}";
    const D: &str = "\u{05D9}\u{05D3}\u{05D5}\u{05D3}";
    const YY: &str = "\u{05D9}\u{05D9}";
    const ELOHIM: &str = "\u{05D0}\u{05DC}\u{05D4}\u{05D9}\u{05DD}";
    const ELOKIM: &str = "\u{05D0}\u{05DC}\u{05E7}\u{05D9}\u{05DD}";

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn tetragrammaton_spellings_give_the_others() {
        assert_eq!(divine_name_alternatives(Q), s(&[FULL, D, YY]));
        assert_eq!(divine_name_alternatives(D), s(&[FULL, Q, YY]));
        assert_eq!(divine_name_alternatives(FULL), s(&[Q, D, YY]));
        assert_eq!(divine_name_alternatives(YY), s(&[FULL, Q, D]));
    }

    /// Otzaria's smart_query cases: up to two prefixes, marks kept on their
    /// letters, mixed letters and three prefixes are no name.
    #[test]
    fn prefixes_and_marks() {
        let v = '\u{05D5}';
        let l = '\u{05DC}';
        assert_eq!(divine_name_alternatives(&format!("{v}{Q}"))[0], format!("{v}{FULL}"));
        assert_eq!(divine_name_alternatives(&format!("{v}{l}{D}"))[0], format!("{v}{l}{FULL}"));
        assert!(divine_name_alternatives(&format!("{v}{v}{l}{Q}")).is_empty());
        let marked = format!("{l}\u{05B0}\u{05D9}\u{05E7}\u{05D5}\u{05E7}\u{05B8}");
        assert_eq!(divine_name_alternatives(&marked)[0], format!("{l}\u{05B0}\u{05D9}\u{05D4}\u{05D5}\u{05D4}\u{05B8}"));
        assert!(divine_name_alternatives("\u{05D9}\u{05E7}\u{05D5}\u{05D3}").is_empty());
        assert!(divine_name_alternatives("\u{05D9}\u{05DB}\u{05D5}\u{05DB}").is_empty());
        assert!(divine_name_alternatives(&format!("\u{05D0}{Q}")).is_empty());
        assert!(divine_name_alternatives("\u{05D9}\u{05E7}\u{05D5}").is_empty());
        assert!(divine_name_alternatives("\u{05D9}").is_empty());
    }

    /// Prefix + form pairs that are other words: excluded both ways.
    #[test]
    fn excluded_pairs() {
        let shin = '\u{05E9}';
        assert!(divine_name_alternatives(&format!("{shin}{D}")).is_empty());
        assert!(divine_name_alternatives(&format!("\u{05D5}{shin}{D}")).is_empty());
        assert_eq!(divine_name_alternatives(&format!("{shin}{FULL}")), s(&[&format!("{shin}{Q}"), &format!("{shin}{YY}")]));
        assert!(divine_name_alternatives(&format!("\u{05D4}{YY}")).is_empty());
        assert!(divine_name_alternatives(&format!("\u{05DB}{YY}")).is_empty());
        assert_eq!(divine_name_alternatives(&format!("\u{05D4}{Q}")).len(), 2);
        assert_eq!(divine_name_alternatives(&format!("\u{05DC}{YY}"))[0], format!("\u{05DC}{FULL}"));
    }

    #[test]
    fn elohim_family() {
        assert_eq!(divine_name_alternatives(ELOHIM), s(&[ELOKIM]));
        assert_eq!(divine_name_alternatives(ELOKIM), s(&[ELOHIM]));
        // with vav, suffix nun-vav, prefix he
        assert_eq!(
            divine_name_alternatives("\u{05D4}\u{05D0}\u{05DC}\u{05D5}\u{05E7}\u{05D9}\u{05E0}\u{05D5}"),
            s(&["\u{05D4}\u{05D0}\u{05DC}\u{05D5}\u{05D4}\u{05D9}\u{05E0}\u{05D5}"])
        );
        // no suffix (construct form)
        assert_eq!(divine_name_alternatives("\u{05D0}\u{05DC}\u{05E7}\u{05D9}"), s(&["\u{05D0}\u{05DC}\u{05D4}\u{05D9}"]));
        // another suffix, another third letter, a missing yod: no name
        assert!(divine_name_alternatives("\u{05D0}\u{05DC}\u{05E7}\u{05D9}\u{05EA}").is_empty());
        assert!(divine_name_alternatives("\u{05D0}\u{05DC}\u{05D3}\u{05D9}\u{05DD}").is_empty());
        assert!(divine_name_alternatives("\u{05D0}\u{05DC}\u{05E7}\u{05DD}").is_empty());
        // marks stay on their letters
        assert_eq!(
            divine_name_alternatives("\u{05D0}\u{05B1}\u{05DC}\u{05D4}\u{05B4}\u{05D9}\u{05DD}"),
            s(&["\u{05D0}\u{05B1}\u{05DC}\u{05E7}\u{05B4}\u{05D9}\u{05DD}"])
        );
    }
}
