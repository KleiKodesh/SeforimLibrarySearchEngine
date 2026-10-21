//! FtsLib's ktiv male / haser rules: a port, step for step, of
//! `FtsLib/Search/KetivExpander.cs` (KitveiHakodesh `71162c83`), the rule
//! set the walk uses with [`crate::KtivRules::FtsLib`]. The forms become core
//! forms of the walk's trie, exactly as Otzaria's ktiv forms do.
//!
//! Steps (`Expand(term, maxVariants = 40)`), on UTF-16 units as .NET:
//! 0. A term under 3 units has no variants.
//! 1. Strip the first preserved suffix (table order, longest first) that the
//!    term ends with and is longer than.
//! 2. A vav / yod at the stem's first or last index is consonantal.
//! 3. Skeleton = the stem without its non-consonantal vav / yod; the
//!    skeleton's consonant positions are its letters alef..tav other than
//!    vav / yod, plus the consonantal vav / yod.
//! 4. Each non-consonantal vav / yod deleted from the stem alone, + suffix
//!    (no cap check), if the result has >= 2 units and is not the term.
//! 5. Under 2 skeleton consonants: go to step 8.
//! 6. The bare skeleton + suffix (no cap check), then for each base-3 mask
//!    over the first min(gaps, 4) gaps (0 none, 1 vav, 2 yod), while the set
//!    holds fewer than the cap: skip a mask with no insertion or with two
//!    insertions after adjacent skeleton positions; insert after each chosen
//!    consonant, + suffix.
//! 7. (fall through)
//! 8. For each stem unit but the last that is a consonant, for vav then yod,
//!    while under the cap: unless the next unit is that same letter, insert it
//!    after the unit, + suffix, if not the term and >= 2 units.
//!
//! The set never holds the term itself; the cap counts distinct forms.

use std::collections::HashSet;

const YOD: u16 = 0x05D9;
const VAV: u16 = 0x05D5;

/// FtsLib's `KetivExpander.MaxVariants`.
pub const FTSLIB_KTIV_MAX_VARIANTS: usize = 40;

/// FtsLib's `PreservedSuffixes`, in its order (from the code's `\u` escapes).
pub const FTSLIB_PRESERVED_SUFFIXES: [&str; 15] = [
    "\u{05D5}\u{05D9}\u{05D5}\u{05EA}",
    "\u{05D9}\u{05D5}\u{05EA}",
    "\u{05D5}\u{05EA}",
    "\u{05D9}\u{05DF}",
    "\u{05D9}\u{05DD}",
    "\u{05D9}\u{05EA}",
    "\u{05D5}\u{05DF}",
    "\u{05EA}\u{05D9}",
    "\u{05E0}\u{05D5}",
    "\u{05DB}\u{05DD}",
    "\u{05DB}\u{05DF}",
    "\u{05D4}\u{05DD}",
    "\u{05D4}\u{05DF}",
    "\u{05DA}",
    "\u{05D4}",
];

fn is_hebrew_consonant(c: u16) -> bool {
    (0x05D0..=0x05EA).contains(&c) && c != VAV && c != YOD
}

/// Insertion-ordered set whose length is FtsLib's `HashSet.Count`.
struct Forms {
    seen: HashSet<Vec<u16>>,
    list: Vec<Vec<u16>>,
}

impl Forms {
    fn add(&mut self, v: Vec<u16>) {
        if self.seen.insert(v.clone()) {
            self.list.push(v);
        }
    }
    fn len(&self) -> usize {
        self.list.len()
    }
}

/// FtsLib's `KetivExpander.Expand(term, max)`: the variants of `term`, the
/// term itself left out, in generation order.
pub fn ftslib_ketiv_variants(term: &str, max_variants: usize) -> Vec<String> {
    let term_u: Vec<u16> = term.encode_utf16().collect();
    if term_u.len() < 3 {
        return Vec::new();
    }
    // Step 1
    let mut stem: &[u16] = &term_u;
    let mut suffix: Vec<u16> = Vec::new();
    for s in FTSLIB_PRESERVED_SUFFIXES {
        let s: Vec<u16> = s.encode_utf16().collect();
        if term_u.ends_with(&s) && term_u.len() > s.len() {
            stem = &term_u[..term_u.len() - s.len()];
            suffix = s;
            break;
        }
    }
    let with_suffix = |mut v: Vec<u16>| {
        v.extend_from_slice(&suffix);
        v
    };
    // Step 2
    let protected =
        |i: usize| (stem[i] == VAV || stem[i] == YOD) && (i == 0 || i == stem.len() - 1);
    // Step 3
    let mut skeleton: Vec<u16> = Vec::with_capacity(stem.len());
    let mut consonants: Vec<usize> = Vec::new();
    for (i, &c) in stem.iter().enumerate() {
        let vowel_letter = (c == VAV || c == YOD) && !protected(i);
        if !vowel_letter {
            let pos = skeleton.len();
            skeleton.push(c);
            if is_hebrew_consonant(c) || protected(i) {
                consonants.push(pos);
            }
        }
    }
    let mut forms = Forms {
        seen: HashSet::new(),
        list: Vec::new(),
    };
    // Step 4
    for i in 0..stem.len() {
        let c = stem[i];
        if (c != VAV && c != YOD) || protected(i) {
            continue;
        }
        let mut buf = stem[..i].to_vec();
        buf.extend_from_slice(&stem[i + 1..]);
        let deletion = with_suffix(buf);
        if deletion.len() >= 2 && deletion != term_u {
            forms.add(deletion);
        }
    }
    if consonants.len() >= 2 {
        let bare = with_suffix(skeleton.clone());
        if bare != term_u {
            forms.add(bare);
        }
        // Step 5 (FtsLib's numbering): skeleton insertion pass.
        let effective_gaps = (consonants.len() - 1).min(4);
        let total = 3usize.pow(effective_gaps as u32);
        let mut mask = 0;
        while mask < total && forms.len() < max_variants {
            let mut insertions: Vec<(usize, u16)> = Vec::with_capacity(effective_gaps);
            let mut m = mask;
            for g in 0..effective_gaps {
                match m % 3 {
                    1 => insertions.push((consonants[g], VAV)),
                    2 => insertions.push((consonants[g], YOD)),
                    _ => {}
                }
                m /= 3;
            }
            mask += 1;
            if insertions.is_empty() || insertions.windows(2).any(|w| w[1].0 == w[0].0 + 1) {
                continue;
            }
            let mut sb = skeleton.clone();
            for (offset, &(after, ch)) in insertions.iter().enumerate() {
                sb.insert(after + 1 + offset, ch);
            }
            let variant = with_suffix(sb);
            if variant != term_u {
                forms.add(variant);
            }
        }
    }
    // Step 6 (FtsLib's numbering): original stem insertion pass.
    let mut i = 0;
    while i + 1 < stem.len() && forms.len() < max_variants {
        if is_hebrew_consonant(stem[i]) {
            for ch in [VAV, YOD] {
                if forms.len() >= max_variants {
                    break;
                }
                if stem[i + 1] == ch {
                    continue;
                }
                let mut sb = stem.to_vec();
                sb.insert(i + 1, ch);
                let variant = with_suffix(sb);
                if variant != term_u && variant.len() >= 2 {
                    forms.add(variant);
                }
            }
        }
        i += 1;
    }
    forms
        .list
        .into_iter()
        .map(|v| String::from_utf16_lossy(&v))
        .collect()
}

/// (word, FtsLib's `KetivExpander.Expand(word)` output), generated by
/// running FtsLib's own code.
#[cfg(test)]
pub(crate) const FTSLIB_CASES: &[(&str, &[&str])] = include!("ftslib_ketiv_cases.in");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equals_ftslib_output_in_order() {
        for &(word, expected) in FTSLIB_CASES {
            let got = ftslib_ketiv_variants(word, FTSLIB_KTIV_MAX_VARIANTS);
            assert_eq!(got, expected.to_vec(), "word {:?}", word.escape_unicode().to_string());
        }
    }

    #[test]
    fn suffix_table_is_longest_first_as_ftslib() {
        assert_eq!(FTSLIB_PRESERVED_SUFFIXES.len(), 15);
        assert!(ftslib_ketiv_variants("\u{05D0}\u{05D1}", 40).is_empty());
    }
}
