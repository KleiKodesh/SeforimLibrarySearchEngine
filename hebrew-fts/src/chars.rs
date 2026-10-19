//! Port of FtsLib `HebrewChars.cs` (KitveiHakodesh, git HEAD).
//!
//! Single source of truth for the per-character text rules shared by every path
//! that normalizes text: the indexing/snippet scanner and the query normalizer.
//! Change a rule here and every path picks it up — the index, the highlighter,
//! and the query parser stay in lockstep.
//!
//! Non-ASCII code points are written numerically to keep the rules unambiguous.

/// Hebrew letters (alef-tav U+05D0..U+05EA, incl. final forms) and ASCII a-z / A-Z.
#[inline(always)]
pub fn is_letter(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_uppercase() || ('\u{05D0}'..='\u{05EA}').contains(&c)
}

/// ASCII lowercase (Hebrew and other chars pass through unchanged).
#[inline(always)]
pub fn to_lower_ascii(c: char) -> char {
    if c.is_ascii_uppercase() {
        (c as u8 | 32) as char
    } else {
        c
    }
}

/// Lowercase ASCII letter — call AFTER [`to_lower_ascii`]. For script-boundary split.
#[inline(always)]
pub fn is_latin_lower(c: char) -> bool {
    c.is_ascii_lowercase()
}

/// Nikud + cantillation stripped from words (U+0591..U+05C7), EXCEPT the three
/// separator marks paseq (U+05C0), sof pasuq (U+05C3), nun hafukha (U+05C6).
#[inline(always)]
pub fn is_strippable_mark(c: char) -> bool {
    ('\u{0591}'..='\u{05C7}').contains(&c)
        && c != '\u{05C0}'
        && c != '\u{05C3}'
        && c != '\u{05C6}'
}

/// Quote chars INSIDE a Hebrew word (rashi/gershayim/gematria) — transparent
/// connectors, not separators. dquote U+0022, gershayim U+05F4, apostrophe U+0027,
/// geresh U+05F3.
#[inline(always)]
pub fn is_intra_word_quote(c: char) -> bool {
    matches!(c, '\u{0022}' | '\u{05F4}' | '\u{0027}' | '\u{05F3}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters() {
        assert!(is_letter('א'));
        assert!(is_letter('ת'));
        assert!(is_letter('ך')); // final kaf, inside 05D0..05EA
        assert!(is_letter('a'));
        assert!(is_letter('Z'));
        assert!(!is_letter('1'));
        assert!(!is_letter('־')); // maqaf
        assert!(!is_letter('\u{05B0}')); // sheva
    }

    #[test]
    fn strippable_marks() {
        assert!(is_strippable_mark('\u{0591}')); // etnahta
        assert!(is_strippable_mark('\u{05B0}')); // sheva
        assert!(is_strippable_mark('\u{05C7}')); // qamats qatan
        assert!(!is_strippable_mark('\u{05C0}')); // paseq
        assert!(!is_strippable_mark('\u{05C3}')); // sof pasuq
        assert!(!is_strippable_mark('\u{05C6}')); // nun hafukha
        assert!(!is_strippable_mark('א'));
    }

    #[test]
    fn lower_ascii() {
        assert_eq!(to_lower_ascii('A'), 'a');
        assert_eq!(to_lower_ascii('z'), 'z');
        assert_eq!(to_lower_ascii('א'), 'א');
    }

    #[test]
    fn intra_word_quotes() {
        assert!(is_intra_word_quote('"'));
        assert!(is_intra_word_quote('\''));
        assert!(is_intra_word_quote('\u{05F3}')); // geresh
        assert!(is_intra_word_quote('\u{05F4}')); // gershayim
        assert!(!is_intra_word_quote('`'));
    }
}
