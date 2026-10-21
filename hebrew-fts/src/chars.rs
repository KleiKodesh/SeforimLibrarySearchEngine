//! The per-character text rules shared by every path that normalizes text:
//! the indexing/snippet scanner and the query parser. Change a rule here and
//! every path picks it up: the index, the highlighter and the query parser
//! stay in lockstep.
//!
//! The word rules are Otzaria's tokenizer (`hebrew_tokenizer.rs`,
//! `hebrew_query.rs` of otzaria_search_engine v5.0.0), which FtsLib follows
//! too (`HebrewChars.cs`). Non-ASCII code points are written numerically.

/// Hebrew letters alef-tav (U+05D0..U+05EA, final forms included).
#[inline(always)]
pub fn is_hebrew_letter(c: char) -> bool {
    ('\u{05D0}'..='\u{05EA}').contains(&c)
}

/// Otzaria `is_attached_mark`: Hebrew points and te'amim (U+0591..U+05C7)
/// except maqaf (U+05BE), paseq (U+05C0), sof pasuq (U+05C3) and nun
/// hafukha (U+05C6).
#[inline(always)]
pub fn is_attached_mark(c: char) -> bool {
    ('\u{0591}'..='\u{05C7}').contains(&c) && !matches!(c, '\u{05BE}' | '\u{05C0}' | '\u{05C3}' | '\u{05C6}')
}

/// Otzaria `is_word_mark`: an attached Hebrew mark, a general combining mark
/// (U+0300..U+036F) or U+FB1E. Marks belong to the word they sit on but never
/// start one; the text keeps them only on an index that keeps marks (U+FB1E
/// never).
#[inline(always)]
pub fn is_word_mark(c: char) -> bool {
    is_attached_mark(c) || ('\u{0300}'..='\u{036F}').contains(&c) || c == '\u{FB1E}'
}

/// Otzaria `is_invisible_char`: zero-width and bidi controls (U+200B..U+200F,
/// U+202A..U+202E, U+2066..U+2069, U+FEFF).
#[inline(always)]
pub fn is_invisible(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Otzaria `is_transparent`: chars that neither break a word nor belong to
/// it (letter, period, letter, period, letter is one word of three letters):
/// `* [ ] ^ $ \ + . ~`, backtick, and the invisible chars.
#[inline(always)]
pub fn is_transparent(c: char) -> bool {
    matches!(c, '*' | '[' | ']' | '^' | '$' | '\\' | '+' | '.' | '~' | '`') || is_invisible(c)
}

/// Otzaria `is_geresh`: `'`, geresh U+05F3 and the typographic single
/// quotes U+2018 / U+2019. Normalized to `'`.
#[inline(always)]
pub fn is_geresh(c: char) -> bool {
    matches!(c, '\'' | '\u{05F3}' | '\u{2018}' | '\u{2019}')
}

/// Otzaria `is_gershayim`: `"`, gershayim U+05F4 and the typographic double
/// quotes U+201C / U+201D. Normalized to `"`.
#[inline(always)]
pub fn is_gershayim(c: char) -> bool {
    matches!(c, '"' | '\u{05F4}' | '\u{201C}' | '\u{201D}')
}

/// A geresh or a gershayim ([`is_geresh`], [`is_gershayim`]).
#[inline(always)]
pub fn is_quote(c: char) -> bool {
    is_geresh(c) || is_gershayim(c)
}

/// The quote bytes a normalized term can hold (`'` and `"`).
#[inline(always)]
pub fn is_term_quote_byte(b: u8) -> bool {
    b == b'\'' || b == b'"'
}

/// The quote chars a normalized term can hold (`'` and `"`).
#[inline(always)]
pub fn is_term_quote(c: char) -> bool {
    c == '\'' || c == '"'
}

/// Otzaria `is_word_char`: a Hebrew letter, an ASCII letter or digit, or any
/// other non-ASCII alphanumeric char (letters and digits of every script:
/// no Hebrew/Latin split) or word mark.
#[inline(always)]
pub fn is_word_char(c: char) -> bool {
    if is_hebrew_letter(c) || c.is_ascii_alphanumeric() {
        return true;
    }
    !c.is_ascii() && (c.is_alphanumeric() || is_word_mark(c))
}

/// Otzaria `starts_token`: a word char that may start a word (not a mark).
/// Also what counts as a letter of a word (marks and quotes do not).
#[inline(always)]
pub fn starts_word(c: char) -> bool {
    if is_hebrew_letter(c) || c.is_ascii_alphanumeric() {
        return true;
    }
    !c.is_ascii() && c.is_alphanumeric() && !is_word_mark(c)
}

/// Otzaria `fold_presentation_form`: Hebrew presentation forms (U+FB1D..U+FB4F)
/// as their letters and marks.
pub fn fold_presentation_form(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{FB1D}' => "\u{05D9}\u{05B4}",
        '\u{FB1E}' => "",
        '\u{FB1F}' => "\u{05F2}\u{05B7}",
        '\u{FB20}' => "\u{05E2}",
        '\u{FB21}' => "\u{05D0}",
        '\u{FB22}' => "\u{05D3}",
        '\u{FB23}' => "\u{05D4}",
        '\u{FB24}' => "\u{05DB}",
        '\u{FB25}' => "\u{05DC}",
        '\u{FB26}' => "\u{05DD}",
        '\u{FB27}' => "\u{05E8}",
        '\u{FB28}' => "\u{05EA}",
        '\u{FB2A}' => "\u{05E9}\u{05C1}",
        '\u{FB2B}' => "\u{05E9}\u{05C2}",
        '\u{FB2C}' => "\u{05E9}\u{05BC}\u{05C1}",
        '\u{FB2D}' => "\u{05E9}\u{05BC}\u{05C2}",
        '\u{FB2E}' => "\u{05D0}\u{05B7}",
        '\u{FB2F}' => "\u{05D0}\u{05B8}",
        '\u{FB30}' => "\u{05D0}\u{05BC}",
        '\u{FB31}' => "\u{05D1}\u{05BC}",
        '\u{FB32}' => "\u{05D2}\u{05BC}",
        '\u{FB33}' => "\u{05D3}\u{05BC}",
        '\u{FB34}' => "\u{05D4}\u{05BC}",
        '\u{FB35}' => "\u{05D5}\u{05BC}",
        '\u{FB36}' => "\u{05D6}\u{05BC}",
        '\u{FB38}' => "\u{05D8}\u{05BC}",
        '\u{FB39}' => "\u{05D9}\u{05BC}",
        '\u{FB3A}' => "\u{05DA}\u{05BC}",
        '\u{FB3B}' => "\u{05DB}\u{05BC}",
        '\u{FB3C}' => "\u{05DC}\u{05BC}",
        '\u{FB3E}' => "\u{05DE}\u{05BC}",
        '\u{FB40}' => "\u{05E0}\u{05BC}",
        '\u{FB41}' => "\u{05E1}\u{05BC}",
        '\u{FB43}' => "\u{05E3}\u{05BC}",
        '\u{FB44}' => "\u{05E4}\u{05BC}",
        '\u{FB46}' => "\u{05E6}\u{05BC}",
        '\u{FB47}' => "\u{05E7}\u{05BC}",
        '\u{FB48}' => "\u{05E8}\u{05BC}",
        '\u{FB49}' => "\u{05E9}\u{05BC}",
        '\u{FB4A}' => "\u{05EA}\u{05BC}",
        '\u{FB4B}' => "\u{05D5}\u{05B9}",
        '\u{FB4C}' => "\u{05D1}\u{05BF}",
        '\u{FB4D}' => "\u{05DB}\u{05BF}",
        '\u{FB4E}' => "\u{05E4}\u{05BF}",
        '\u{FB4F}' => "\u{05D0}\u{05DC}",
        _ => return None,
    })
}

/// Otzaria's `MAX_TOKEN_BYTES` (its `RemoveLongFilter::limit(128)`, FtsLib
/// `HebrewChars.MaxTermBytes`): a word whose term (normalized, before
/// lowercasing, quotes included) has this many UTF-8 bytes or more is not
/// indexed. It is still a word: it takes its position.
pub const MAX_TERM_BYTES: usize = 128;

/// True when a term of `bytes` UTF-8 bytes is indexed (tantivy's
/// `RemoveLongFilter` keeps `len < limit`).
#[inline(always)]
pub fn is_indexable_len(bytes: usize) -> bool {
    bytes < MAX_TERM_BYTES
}

/// Nikud + cantillation (U+0591..U+05C7) except the separator marks paseq
/// (U+05C0), sof pasuq (U+05C3), nun hafukha (U+05C6).
#[inline(always)]
pub fn is_strippable_mark(c: char) -> bool {
    ('\u{0591}'..='\u{05C7}').contains(&c) && c != '\u{05C0}' && c != '\u{05C3}' && c != '\u{05C6}'
}

/// Cantillation marks: U+0591..U+05AF.
#[inline(always)]
pub fn is_taam(c: char) -> bool {
    ('\u{0591}'..='\u{05AF}').contains(&c)
}

/// Vowel points: the strippable marks that are not te'amim (U+05B0..U+05BD,
/// U+05BF, U+05C1/U+05C2, U+05C4/U+05C5, U+05C7).
#[inline(always)]
pub fn is_nikud(c: char) -> bool {
    is_strippable_mark(c) && !is_taam(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_chars() {
        assert!(is_word_char('\u{05D0}') && starts_word('\u{05D0}'));
        assert!(is_word_char('a') && is_word_char('Z') && is_word_char('7'));
        // other scripts are word chars (Otzaria: no Hebrew/Latin split)
        assert!(starts_word('\u{00E9}') && starts_word('\u{03B1}') && starts_word('\u{0436}'));
        assert!(starts_word('\u{0663}')); // Arabic-Indic digit
        // marks continue a word but never start one
        assert!(is_word_char('\u{05B0}') && !starts_word('\u{05B0}'));
        assert!(is_word_char('\u{0301}') && !starts_word('\u{0301}'));
        assert!(is_word_char('\u{FB1E}') && !starts_word('\u{FB1E}'));
        // separators
        for c in ['\u{05BE}', '\u{05C0}', '\u{05C3}', '\u{05C6}', ' ', ',', '(', '-', '_'] {
            assert!(!is_word_char(c), "{c:?}");
        }
    }

    #[test]
    fn transparent_and_quotes() {
        for c in ['.', '[', ']', '*', '~', '\u{200E}', '\u{200F}', '\u{202B}', '\u{2067}', '\u{FEFF}', '\u{200B}'] {
            assert!(is_transparent(c), "{c:?}");
        }
        assert!(!is_transparent('(') && !is_transparent('?'));
        for c in ['\'', '\u{05F3}', '\u{2018}', '\u{2019}'] {
            assert!(is_geresh(c));
        }
        for c in ['"', '\u{05F4}', '\u{201C}', '\u{201D}'] {
            assert!(is_gershayim(c));
        }
    }

    #[test]
    fn presentation_forms_and_limits() {
        assert_eq!(fold_presentation_form('\u{FB2B}'), Some("\u{05E9}\u{05C2}"));
        assert_eq!(fold_presentation_form('\u{FB4F}'), Some("\u{05D0}\u{05DC}"));
        assert_eq!(fold_presentation_form('\u{05D0}'), None);
        assert!(is_indexable_len(127) && !is_indexable_len(128));
    }

    #[test]
    fn strippable_marks() {
        assert!(is_strippable_mark('\u{0591}'));
        assert!(is_strippable_mark('\u{05C7}'));
        assert!(!is_strippable_mark('\u{05C0}'));
        assert!(!is_strippable_mark('\u{05C3}'));
        assert!(!is_strippable_mark('\u{05C6}'));
        assert!(!is_attached_mark('\u{05BE}'));
    }
}
