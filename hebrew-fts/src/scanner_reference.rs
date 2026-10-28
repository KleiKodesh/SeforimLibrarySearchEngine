//! Test oracle of the production scanner ([`crate::scanner::scan_with`]): the
//! same output computed the slow, obvious way, so a property test can check
//! the single-pass scanner against it on every input.
//!
//! 1. FtsLib's HTML layer turns the raw text into visible elements: every
//!    char outside a tag, a break for each entity (at its `&`) and for each
//!    word-breaking tag (at its `>`). Inline tags vanish, and so does a
//!    `<sup>` that `html_tags::is_transparent_sup` accepts.
//! 2. The elements become a plain string (a break is a space) and Otzaria's
//!    own functions, verbatim ([`crate::word_rule`]), cut it into words,
//!    find the paired readings and normalize each word.
//! 3. Offsets map back to the raw text: a word's raw end is where the
//!    element after its last char starts (trailing inline tags included).
#![cfg(test)]

use crate::chars;
use crate::html_tags;
use crate::scanner::WordSpan;
use crate::word_rule::{next_token_boundaries, paired_reading_after, Normalizer};

const TAG_NAME_CAP: usize = 16;

/// One visible element: its char (a break is `' '`), raw byte and UTF-16
/// offsets, and visible width.
struct Elem {
    c: char,
    byte: usize,
    utf16: usize,
    width: usize,
}

fn elements(text: &str) -> (Vec<Elem>, usize) {
    let mut table: Vec<(usize, usize, char)> = Vec::new();
    let mut u = 0usize;
    for (b, c) in text.char_indices() {
        table.push((b, u, c));
        u += c.len_utf16();
    }
    let total_utf16 = u;
    let len = table.len();
    let mut out = Vec::new();
    let mut tag_name: Vec<char> = Vec::new();
    let mut in_tag = false;
    let mut tag_name_done = false;
    let mut tag_lt = 0usize;
    let mut sup_transparent = false;
    let mut i = 0usize;
    while i < len {
        let (b, u, c) = table[i];
        if in_tag {
            if c == '>' {
                let breaks = if html_tags::is_sup(&tag_name) {
                    if text[tag_lt + 1..].starts_with('/') {
                        !std::mem::take(&mut sup_transparent)
                    } else {
                        sup_transparent = html_tags::is_transparent_sup(text, tag_lt, b);
                        !sup_transparent
                    }
                } else {
                    html_tags::is_word_break_tag(&tag_name)
                };
                if breaks {
                    out.push(Elem { c: ' ', byte: b, utf16: u, width: 0 });
                }
                in_tag = false;
                tag_name.clear();
            } else if !tag_name_done {
                if c == ' ' || c == '\t' {
                    tag_name_done = true;
                } else if tag_name.len() < TAG_NAME_CAP && c != '/' {
                    tag_name.push(c);
                }
            }
            i += 1;
            continue;
        }
        // unterminated '<' (no '>' after it): a plain character
        if c == '<' && table[i + 1..len].iter().any(|e| e.2 == '>') {
            in_tag = true;
            tag_name.clear();
            tag_name_done = false;
            tag_lt = b;
            i += 1;
            continue;
        }
        if c == '&' {
            out.push(Elem { c: ' ', byte: b, utf16: u, width: 1 });
            html_tags::skip_entity(&table, len, &mut i);
            i += 1;
            continue;
        }
        let width = if chars::is_word_mark(c) { 0 } else { c.len_utf16() };
        out.push(Elem { c, byte: b, utf16: u, width });
        i += 1;
    }
    (out, total_utf16)
}

pub fn scan_with(text: &str, keep_marks: bool, mut on_word: impl FnMut(&str, WordSpan)) -> usize {
    let (elems, total_utf16) = elements(text);
    // the visible string and, per element, its byte offset in it
    let mut s = String::new();
    let mut s_byte = Vec::with_capacity(elems.len() + 1);
    let mut visible_before = Vec::with_capacity(elems.len() + 1);
    let mut v = 0usize;
    for e in &elems {
        s_byte.push(s.len());
        visible_before.push(v);
        s.push(e.c);
        v += e.width;
    }
    s_byte.push(s.len());
    visible_before.push(v);
    let elem_at = |byte: usize| s_byte.binary_search(&byte).expect("element boundary");
    let raw_byte = |k: usize| if k < elems.len() { elems[k].byte } else { text.len() };
    let raw_utf16 = |k: usize| if k < elems.len() { elems[k].utf16 } else { total_utf16 };

    let normalizer = Normalizer { keep_marks, wildcards: false };
    let mut pos = 0usize;
    let mut token_count = 0usize;
    let mut share_next = false;
    while let Some((start, end)) = next_token_boundaries(&s, pos, false) {
        let w = normalizer.normalize(&s[start..end]);
        let position = if share_next {
            token_count - 1
        } else {
            token_count += 1;
            token_count - 1
        };
        share_next = paired_reading_after(&s, start, end).is_some();
        let (ks, ke) = (elem_at(start), elem_at(end));
        on_word(
            &w.text,
            WordSpan {
                byte_start: raw_byte(ks),
                byte_end: raw_byte(ke),
                utf16_start: raw_utf16(ks),
                utf16_end: raw_utf16(ke),
                visible_start: visible_before[ks],
                letters: w.letters,
                term_bytes: w.term_bytes,
                position,
            },
        );
        pos = end;
    }
    total_utf16
}
