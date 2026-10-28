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

/// Otzaria `is_taam_mark` (vocalized search classes): every attached mark
/// that is not nikud ([`is_nikud`]): the cantillation marks U+0591..U+05AF,
/// meteg U+05BD and the upper / lower dots U+05C4 / U+05C5.
#[inline(always)]
pub fn is_taam(c: char) -> bool {
    is_attached_mark(c) && !is_nikud(c)
}

/// Otzaria `is_nikud_mark` (vocalized search classes): the vowel points
/// U+05B0..U+05BB, dagesh / mappiq U+05BC, rafe U+05BF, the shin / sin dots
/// U+05C1 / U+05C2 and qamatz qatan U+05C7.
#[inline(always)]
pub fn is_nikud(c: char) -> bool {
    matches!(c, '\u{05B0}'..='\u{05BC}' | '\u{05BF}' | '\u{05C1}' | '\u{05C2}' | '\u{05C7}')
}

/// The general combining marks U+0300..U+036F: kept in the terms of an index
/// that keeps marks, never nikud or te'amim (Otzaria: always free in a
/// vocalized search).
#[inline(always)]
pub fn is_combining_mark(c: char) -> bool {
    ('\u{0300}'..='\u{036F}').contains(&c)
}

/// A mark an index that keeps marks holds inside its terms: an attached
/// Hebrew mark or a general combining mark ([`is_word_mark`] without
/// U+FB1E, which the word rule drops).
#[inline(always)]
pub fn is_term_mark(c: char) -> bool {
    is_attached_mark(c) || is_combining_mark(c)
}

/// The term marks ([`is_term_mark`]) as code point ranges: the zero-width
/// characters of a marks-keeping dictionary's walks and caps. The separators
/// inside U+0591..U+05C7 (maqaf, paseq, sof pasuq, nun hafukha) never occur
/// inside a term, so the whole range can be given.
pub const TERM_MARK_RANGES: [(char, char); 2] = [('\u{0300}', '\u{036F}'), ('\u{0591}', '\u{05C7}')];

/// Otzaria `contains_attached_marks`: the line carries at least one attached
/// mark (only such lines are in Otzaria's vocalized field, `textVocalized`).
pub fn contains_attached_marks(text: &str) -> bool {
    text.chars().any(is_attached_mark)
}

/// `term` without its term marks ([`is_term_mark`]).
pub fn strip_term_marks(term: &str) -> std::borrow::Cow<'_, str> {
    if term.chars().any(is_term_mark) {
        std::borrow::Cow::Owned(term.chars().filter(|&c| !is_term_mark(c)).collect())
    } else {
        std::borrow::Cow::Borrowed(term)
    }
}

/// The Unicode canonical combining class of a term mark (Unicode 16
/// `UnicodeData.txt`); 0 for every other char (letters, digits, quotes).
/// Marks are kept in canonical order (stable by this class) in the terms of
/// an index that keeps marks and in the typed words, so the order a text
/// or a user put two marks of one letter in does not matter.
pub fn combining_class(c: char) -> u8 {
    match c {
        '\u{0300}'..='\u{0314}' => 230,
        '\u{0315}' => 232,
        '\u{0316}'..='\u{0319}' => 220,
        '\u{031A}' => 232,
        '\u{031B}' => 216,
        '\u{031C}'..='\u{0320}' => 220,
        '\u{0321}'..='\u{0322}' => 202,
        '\u{0323}'..='\u{0326}' => 220,
        '\u{0327}'..='\u{0328}' => 202,
        '\u{0329}'..='\u{0333}' => 220,
        '\u{0334}'..='\u{0338}' => 1,
        '\u{0339}'..='\u{033C}' => 220,
        '\u{033D}'..='\u{0344}' => 230,
        '\u{0345}' => 240,
        '\u{0346}' => 230,
        '\u{0347}'..='\u{0349}' => 220,
        '\u{034A}'..='\u{034C}' => 230,
        '\u{034D}'..='\u{034E}' => 220,
        '\u{034F}' => 0,
        '\u{0350}'..='\u{0352}' => 230,
        '\u{0353}'..='\u{0356}' => 220,
        '\u{0357}' => 230,
        '\u{0358}' => 232,
        '\u{0359}'..='\u{035A}' => 220,
        '\u{035B}' => 230,
        '\u{035C}' => 233,
        '\u{035D}'..='\u{035E}' => 234,
        '\u{035F}' => 233,
        '\u{0360}'..='\u{0361}' => 234,
        '\u{0362}' => 233,
        '\u{0363}'..='\u{036F}' => 230,
        '\u{0591}' => 220,
        '\u{0592}'..='\u{0595}' => 230,
        '\u{0596}' => 220,
        '\u{0597}'..='\u{0599}' => 230,
        '\u{059A}' => 222,
        '\u{059B}' => 220,
        '\u{059C}'..='\u{05A1}' => 230,
        '\u{05A2}'..='\u{05A7}' => 220,
        '\u{05A8}'..='\u{05A9}' => 230,
        '\u{05AA}' => 220,
        '\u{05AB}'..='\u{05AC}' => 230,
        '\u{05AD}' => 222,
        '\u{05AE}' => 228,
        '\u{05AF}' => 230,
        '\u{05B0}' => 10,
        '\u{05B1}' => 11,
        '\u{05B2}' => 12,
        '\u{05B3}' => 13,
        '\u{05B4}' => 14,
        '\u{05B5}' => 15,
        '\u{05B6}' => 16,
        '\u{05B7}' => 17,
        '\u{05B8}' => 18,
        '\u{05B9}'..='\u{05BA}' => 19,
        '\u{05BB}' => 20,
        '\u{05BC}' => 21,
        '\u{05BD}' => 22,
        '\u{05BF}' => 23,
        '\u{05C1}' => 24,
        '\u{05C2}' => 25,
        '\u{05C4}' => 230,
        '\u{05C5}' => 220,
        '\u{05C7}' => 18,
        _ => 0,
    }
}

/// Appends the term mark `mark` to `text` in canonical order: before the
/// marks at the end of `text` whose combining class is greater (a stable
/// insertion sort of each mark run, Unicode's canonical ordering). Returns
/// nothing; the caller counts bytes. The combining grapheme joiner
/// ([`is_grapheme_joiner`]) is dropped: it only blocks canonical reordering
/// in the source text, so a term (and a typed word) never holds it and the
/// two mark orders around it give one term. It stays inside the word's span
/// (the scanner's offsets count it).
pub fn push_mark_canonical(text: &mut String, mark: char) {
    if is_grapheme_joiner(mark) {
        return;
    }
    let class = combining_class(mark);
    if class == 0 {
        text.push(mark);
        return;
    }
    let mut at = text.len();
    for (i, c) in text.char_indices().rev() {
        let k = combining_class(c);
        if k <= class || k == 0 {
            break;
        }
        at = i;
    }
    text.insert(at, mark);
}

/// U+034F COMBINING GRAPHEME JOINER: a general combining mark of class 0
/// that texts put between two marks to keep their order (issue #2 H02).
/// Dropped from every term and typed word ([`push_mark_canonical`]).
#[inline(always)]
pub fn is_grapheme_joiner(c: char) -> bool {
    c == '\u{034F}'
}

/// `term` with the marks of each letter in canonical order (see
/// [`push_mark_canonical`]); a grapheme joiner is dropped.
pub fn canonical_marks(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for c in term.chars() {
        if is_term_mark(c) {
            push_mark_canonical(&mut out, c);
        } else {
            out.push(c);
        }
    }
    out
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
    fn mark_classes_and_canonical_order() {
        // Otzaria's classes: nikud U+05B0..U+05BC, U+05BF, U+05C1, U+05C2, U+05C7; te'amim the rest
        for c in ['\u{05B0}', '\u{05BB}', '\u{05BC}', '\u{05BF}', '\u{05C1}', '\u{05C2}', '\u{05C7}'] {
            assert!(is_nikud(c) && !is_taam(c) && is_term_mark(c), "{c:?}");
        }
        for c in ['\u{0591}', '\u{05AF}', '\u{05BD}', '\u{05C4}', '\u{05C5}'] {
            assert!(is_taam(c) && !is_nikud(c) && is_term_mark(c), "{c:?}");
        }
        assert!(is_term_mark('\u{0301}') && !is_nikud('\u{0301}') && !is_taam('\u{0301}'));
        assert!(!is_term_mark('\u{05BE}') && !is_term_mark('\u{05D0}') && !is_term_mark('\u{FB1E}'));
        // canonical order is stable by class: dagesh (21) after a vowel, a
        // ta'am (220) after both, two marks of one class keep their order
        assert_eq!(canonical_marks("\u{05D1}\u{05BC}\u{0591}\u{05B8}"), "\u{05D1}\u{05B8}\u{05BC}\u{0591}");
        assert_eq!(canonical_marks("\u{05D1}\u{0596}\u{0591}"), "\u{05D1}\u{0596}\u{0591}");
        // never across a letter
        assert_eq!(canonical_marks("\u{05D1}\u{05BC}\u{05D2}\u{05B8}"), "\u{05D1}\u{05BC}\u{05D2}\u{05B8}");
        assert!(contains_attached_marks("a\u{05B8}") && !contains_attached_marks("a\u{0301}\u{05BE}"));
        // the grapheme joiner is dropped: both mark orders around it give one term
        assert_eq!(canonical_marks("\u{05D1}\u{05BC}\u{034F}\u{05B4}"), "\u{05D1}\u{05B4}\u{05BC}");
        assert_eq!(canonical_marks("\u{05D1}\u{05B4}\u{034F}\u{05BC}"), "\u{05D1}\u{05B4}\u{05BC}");
        assert_eq!(strip_term_marks("\u{05D1}\u{05B8}\u{0301}\""), "\u{05D1}\"");
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
