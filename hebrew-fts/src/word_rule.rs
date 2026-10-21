//! Otzaria's word rule on plain text, ported verbatim from
//! otzaria_search_engine v5.0.0 `hebrew_tokenizer.rs` (`next_token_boundaries`,
//! `paired_reading_after`, the token normalization of `advance`) and
//! `hebrew_query.rs` (`keep_second_of_paired_readings`).
//!
//! The production text scanner ([`crate::scanner`]) is a single-pass,
//! HTML-aware form of the same rule; this module is what the query parser
//! uses, and the test oracle of the scanner (`scanner_reference.rs`) runs it
//! on the scanner's visible text.
//!
//! `wildcards`: the query parser's mode, where FtsLib's wildcard chars `*`
//! and `?` are word chars (in text `*` is transparent and `?` a separator).

use crate::chars::{
    fold_presentation_form, is_attached_mark, is_geresh, is_gershayim, is_term_quote, is_transparent, is_word_mark,
    starts_word,
};

#[inline]
fn is_wild(c: char, wildcards: bool) -> bool {
    wildcards && (c == '*' || c == '?')
}

/// Otzaria `is_word_char` (plus the wildcard chars in query mode).
#[inline]
pub fn word_char(c: char, wildcards: bool) -> bool {
    crate::chars::is_word_char(c) || is_wild(c, wildcards)
}

/// Otzaria `starts_token` (plus the wildcard chars in query mode).
#[inline]
pub fn starts(c: char, wildcards: bool) -> bool {
    starts_word(c) || is_wild(c, wildcards)
}

/// Otzaria `is_transparent` (`*` is a word char in query mode).
#[inline]
pub fn transparent(c: char, wildcards: bool) -> bool {
    is_transparent(c) && !is_wild(c, wildcards)
}

/// Otzaria `next_token_boundaries`: the next word at or after `start_byte`,
/// as byte offsets. A word starts at a [`starts`] char and runs over word
/// chars; a run of quotes and transparent chars between two word chars joins
/// them when it holds no quote, one or two gereshes, or one gershayim. At the
/// end of a word a geresh right after it (transparent chars allowed between)
/// belongs to it (a trailing geresh is kept); a gershayim does not.
pub fn next_token_boundaries(text: &str, start_byte: usize, wildcards: bool) -> Option<(usize, usize)> {
    let slice = &text[start_byte..];
    let tok_start_rel = slice.char_indices().find(|(_, c)| starts(*c, wildcards)).map(|(i, _)| i)?;
    let tok_start = start_byte + tok_start_rel;
    let s = &text[tok_start..];
    let mut pos = 0;
    let tok_end_rel = loop {
        pos += s[pos..].char_indices().find(|(_, c)| !word_char(*c, wildcards)).map(|(i, _)| i).unwrap_or(s.len() - pos);
        let mut geresh_n = 0usize;
        let mut gershayim_n = 0usize;
        let mut first_is_geresh = false;
        let mut first_quote_end = 0usize;
        let mut next: Option<(usize, char)> = None;
        for (i, c) in s[pos..].char_indices() {
            if is_geresh(c) || is_gershayim(c) {
                if geresh_n + gershayim_n == 0 {
                    first_quote_end = pos + i + c.len_utf8();
                    first_is_geresh = is_geresh(c);
                }
                if is_geresh(c) {
                    geresh_n += 1;
                } else {
                    gershayim_n += 1;
                }
            } else if !transparent(c, wildcards) {
                next = Some((pos + i, c));
                break;
            }
        }
        match next {
            Some((i, c)) if word_char(c, wildcards) => match (geresh_n, gershayim_n) {
                (0, 0) | (1, 0) | (2, 0) | (0, 1) => pos = i,
                _ => break pos,
            },
            _ => break if first_is_geresh { first_quote_end } else { pos },
        }
    };
    Some((tok_start, tok_start + tok_end_rel))
}

/// A paired reading `(X) [Y]` found after X: Y's span and the byte after `]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairedReading {
    pub second_start: usize,
    pub second_end: usize,
    pub after_pair: usize,
}

/// Otzaria `is_reading_suffix`: what may follow a reading inside its bracket
/// (punctuation, quotes, spaces), anything but a word char or a bracket.
#[inline]
pub fn is_reading_suffix(c: char) -> bool {
    !crate::chars::is_word_char(c) && !matches!(c, '(' | ')' | '[' | ']' | '<' | '>')
}

/// Otzaria `paired_reading_after`: when the word X at `tok_start..tok_end`
/// opens a paired reading `(X) [Y]` (ketiv and qere), Y's span. Y then takes
/// X's position.
pub fn paired_reading_after(text: &str, tok_start: usize, tok_end: usize) -> Option<PairedReading> {
    if !text[..tok_start].ends_with('(') {
        return None;
    }
    let after_close = text[tok_end..].trim_start_matches(is_reading_suffix).strip_prefix(')')?;
    let after_open = after_close.trim_start().strip_prefix('[')?;
    let y_start = text.len() - after_open.len();
    let (start, end) = next_token_boundaries(text, y_start, false)?;
    if start != y_start {
        return None;
    }
    let after_pair = text[end..].trim_start_matches(is_reading_suffix).strip_prefix(']')?;
    Some(PairedReading { second_start: start, second_end: end, after_pair: text.len() - after_pair.len() })
}

/// Otzaria `keep_second_of_paired_readings` (its query sanitizer): every
/// `(X) [Y]` in the query becomes ` Y `.
pub fn keep_second_of_paired_readings(query: &str) -> std::borrow::Cow<'_, str> {
    let mut out = String::new();
    let mut copied = 0;
    let mut pos = 0;
    while let Some((start, end)) = next_token_boundaries(query, pos, false) {
        pos = end;
        if let Some(pair) = paired_reading_after(query, start, end) {
            out.push_str(&query[copied..start - 1]);
            out.push(' ');
            out.push_str(&query[pair.second_start..pair.second_end]);
            out.push(' ');
            copied = pair.after_pair;
            pos = copied;
        }
    }
    if copied == 0 {
        return std::borrow::Cow::Borrowed(query);
    }
    out.push_str(&query[copied..]);
    std::borrow::Cow::Owned(out)
}

/// A word's normalized forms.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Normalized {
    /// The term: Otzaria's token text, lowercased (its `LowerCaser`).
    pub text: String,
    /// UTF-8 bytes of the token text before lowercasing, marks not counted:
    /// what Otzaria's `RemoveLongFilter` measures on its plain field. An
    /// index that keeps marks measures the same bytes, so it indexes exactly
    /// the words the mark-stripped index does (Otzaria's vocalized field
    /// measures with the marks: a named deviation, words of 128 bytes or
    /// more only).
    pub term_bytes: usize,
    /// UTF-16 length of the quote-free, mark-free term (FtsLib's term and
    /// `NormLen`): the word's visible width.
    pub letters: usize,
}

/// Builds a word's normalized forms one source char at a time: Otzaria's
/// `advance` (marks dropped unless kept, transparent chars dropped, a geresh
/// `'`, two gereshes `"`, a gershayim `"`, presentation forms folded), then
/// its `LowerCaser` (per char `to_lowercase`).
#[derive(Debug, Default)]
pub struct Normalizer {
    pub keep_marks: bool,
    pub wildcards: bool,
}

impl Normalizer {
    /// Appends source char `c` to `out`.
    #[inline]
    pub fn push(&self, out: &mut Normalized, c: char) {
        if is_word_mark(c) && c != '\u{FB1E}' {
            if self.keep_marks {
                out.push_mark(c);
            }
            return;
        }
        if transparent(c, self.wildcards) {
            return;
        }
        if is_geresh(c) {
            if out.text.ends_with('\'') {
                out.text.pop();
                out.term_bytes -= 1;
                push_gershayim_deduped(out);
            } else {
                out.text.push('\'');
                out.term_bytes += 1;
            }
        } else if is_gershayim(c) {
            push_gershayim_deduped(out);
        } else if let Some(folded) = fold_presentation_form(c) {
            for f in folded.chars() {
                if is_attached_mark(f) {
                    if self.keep_marks {
                        out.push_mark(f);
                    }
                } else {
                    push_lower(out, f);
                }
            }
        } else {
            push_lower(out, c);
        }
    }

    /// The normalized forms of a whole token text.
    pub fn normalize(&self, token: &str) -> Normalized {
        let mut out = Normalized { text: String::with_capacity(token.len()), ..Normalized::default() };
        for c in token.chars() {
            self.push(&mut out, c);
        }
        out
    }
}

impl Normalized {
    /// Appends a kept mark in canonical order among the marks of its letter
    /// ([`crate::chars::push_mark_canonical`]); marks count neither in
    /// [`Normalized::term_bytes`] nor in [`Normalized::letters`].
    #[inline]
    pub fn push_mark(&mut self, mark: char) {
        crate::chars::push_mark_canonical(&mut self.text, mark);
    }
}

#[inline]
fn push_gershayim_deduped(out: &mut Normalized) {
    if !out.text.ends_with('"') {
        out.text.push('"');
        out.term_bytes += 1;
    }
}

/// Pushes `c` lowercased (Otzaria's `LowerCaser`), counting its source bytes
/// and its lowercased UTF-16 width.
#[inline]
fn push_lower(out: &mut Normalized, c: char) {
    out.term_bytes += c.len_utf8();
    if c.is_ascii() {
        out.text.push(c.to_ascii_lowercase());
        out.letters += 1;
    } else if crate::chars::is_hebrew_letter(c) {
        out.text.push(c);
        out.letters += 1;
    } else {
        for l in c.to_lowercase() {
            out.text.push(l);
            if !is_term_quote(l) {
                out.letters += l.len_utf16();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        let n = Normalizer::default();
        let mut out = Vec::new();
        let mut pos = 0;
        while let Some((s, e)) = next_token_boundaries(text, pos, false) {
            out.push(n.normalize(&text[s..e]).text);
            pos = e;
        }
        out
    }

    #[test]
    fn otzaria_boundaries() {
        // letter . letter . letter: one word
        assert_eq!(words("\u{05E4}.\u{05D1}.\u{05D9}"), ["\u{05E4}\u{05D1}\u{05D9}"]);
        // brackets and bidi marks join
        assert_eq!(words("\u{05D0}[\u{05D1}]\u{05D2} \u{05D3}\u{200F}\u{05D4}"), ["\u{05D0}\u{05D1}\u{05D2}", "\u{05D3}\u{05D4}"]);
        // trailing geresh kept, trailing gershayim not
        assert_eq!(words("\u{05EA}\u{05D5}\u{05E1}' \u{05D9}\u{05E6}\u{05D7}\u{05E7}\""), ["\u{05EA}\u{05D5}\u{05E1}'", "\u{05D9}\u{05E6}\u{05D7}\u{05E7}"]);
        // two gereshes = gershayim; three quotes split
        assert_eq!(words("\u{05D0}''\u{05D1} \u{05D2}'\"\u{05D3}"), ["\u{05D0}\"\u{05D1}", "\u{05D2}", "\u{05D3}"]);
        // typographic quotes fold
        assert_eq!(words("\u{05E8}\u{05E9}\u{201D}\u{05D9} \u{05D2}\u{2019}"), ["\u{05E8}\u{05E9}\"\u{05D9}", "\u{05D2}'"]);
        // other scripts, no Hebrew/Latin split, lowercased
        assert_eq!(words("abc\u{05D3}\u{05D4} CAF\u{00C9}"), ["abc\u{05D3}\u{05D4}", "caf\u{00E9}"]);
        // presentation forms
        assert_eq!(words("\u{FB2B}\u{05DC}\u{05D5}\u{05DD}"), ["\u{05E9}\u{05DC}\u{05D5}\u{05DD}"]);
    }

    #[test]
    fn paired_readings() {
        let q = "\u{05D0} (\u{05D1}\u{05D2}) [\u{05D3}\u{05D4}] \u{05D5}";
        assert_eq!(keep_second_of_paired_readings(q), "\u{05D0}  \u{05D3}\u{05D4}  \u{05D5}");
        assert_eq!(keep_second_of_paired_readings("(\u{05D0}) \u{05D1}"), "(\u{05D0}) \u{05D1}");
    }

    #[test]
    fn term_bytes_and_letters() {
        let n = Normalizer::default();
        let w = n.normalize("\u{05E8}\u{05E9}\u{05F4}\u{05D9}");
        assert_eq!((w.term_bytes, w.letters), (7, 3));
        let w = n.normalize("A\u{05B0}B");
        assert_eq!((w.text.as_str(), w.term_bytes, w.letters), ("ab", 2, 2));
        let m = Normalizer { keep_marks: true, wildcards: false };
        let w = m.normalize("\u{05D0}\u{05B0}");
        assert_eq!((w.text.as_str(), w.term_bytes, w.letters), ("\u{05D0}\u{05B0}", 2, 1));
        // marks in canonical order: dagesh (class 21) after qamats (18),
        // whatever the typed order
        let a = m.normalize("\u{05D1}\u{05BC}\u{05B8}\u{05D2}");
        let b = m.normalize("\u{05D1}\u{05B8}\u{05BC}\u{05D2}");
        assert_eq!(a.text, "\u{05D1}\u{05B8}\u{05BC}\u{05D2}");
        assert_eq!(a, b);
        // a presentation form's mark joins the canonical order too
        let c = m.normalize("\u{FB31}\u{05B8}");
        assert_eq!(c.text, "\u{05D1}\u{05B8}\u{05BC}");
    }
}
