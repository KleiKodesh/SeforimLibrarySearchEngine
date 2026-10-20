//! The pre-optimisation scanner, kept verbatim as a test oracle: the
//! production [`crate::scanner::scan_with`] must produce identical words and
//! [`WordSpan`]s for every input (see the property test in `scanner.rs`).
#![cfg(test)]

use unicode_general_category::{get_general_category, GeneralCategory};

use crate::chars;
use crate::html_tags;
use crate::scanner::WordSpan;

const TAG_NAME_CAP: usize = 16;

#[allow(unused_assignments)] // the final flush!'s buffer reset is never read
pub fn scan_with(text: &str, keep_marks: bool, mut on_word: impl FnMut(&str, WordSpan)) -> usize {
    if text.is_empty() {
        return 0;
    }

    // (byte offset, utf16 offset, char) for every char — lets the main loop use
    // plain indices and arbitrary forward jumps, exactly like the C# for-loop.
    // The table is reused per thread: allocating ~24 bytes per char for every
    // line (e.g. 33 KB for a 2.2 KB line) was a large share of the scan cost.
    thread_local! {
        static CHAR_TABLE: std::cell::RefCell<Vec<(usize, usize, char)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }
    let mut char_table: Vec<(usize, usize, char)> = CHAR_TABLE.with(|t| std::mem::take(&mut *t.borrow_mut()));
    char_table.clear();
    char_table.reserve(text.len());
    let mut u16_pos = 0usize;
    for (byte_pos, c) in text.char_indices() {
        char_table.push((byte_pos, u16_pos, c));
        u16_pos += c.len_utf16();
    }
    // Hand the table back to the thread when the scan ends (also on early return).
    struct GiveBack(Vec<(usize, usize, char)>);
    impl Drop for GiveBack {
        fn drop(&mut self) {
            let v = std::mem::take(&mut self.0);
            CHAR_TABLE.with(|t| *t.borrow_mut() = v);
        }
    }
    let table_guard = GiveBack(char_table);
    let char_table = &table_guard.0;
    let total_bytes = text.len();
    let total_utf16 = u16_pos;
    let len = char_table.len();
    let byte_at = |i: usize| if i < len { char_table[i].0 } else { total_bytes };
    let utf16_at = |i: usize| if i < len { char_table[i].1 } else { total_utf16 };

    // Normalized word buffer — reused across words.
    let mut buf = String::with_capacity(64);
    let mut buf_chars = 0usize; // chars in buf (letters are BMP: chars == UTF-16 units)
    let mut buf_first_latin = false; // is_latin_lower(buf[0])

    // Tag name buffer.
    let mut tag_name: Vec<char> = Vec::with_capacity(TAG_NAME_CAP);
    let mut in_tag = false;
    let mut tag_name_done = false; // first space seen — attribute chars must not join the name

    let mut word_start_byte = 0usize;
    let mut word_start_utf16 = 0usize;
    let mut word_start_visible = 0usize;
    let mut visible = 0usize; // cumulative visible chars up to current position

    macro_rules! flush {
        ($i:expr) => {{
            if buf_chars > 1 && buf_chars < 30 {
                // A quote is part of the word only BETWEEN letters (רש"י). One
                // after the last letter is quotation punctuation (יצחק"): FtsLib
                // drops it, so it must not become part of the term.
                let last_letter_end = buf
                    .char_indices()
                    .filter(|&(_, c)| chars::is_letter(c))
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                if buf[last_letter_end..].contains('"') {
                    let tail: String = buf[last_letter_end..].chars().filter(|&c| c != '"').collect();
                    buf.truncate(last_letter_end);
                    buf.push_str(&tail);
                }
                let span = WordSpan {
                    byte_start: word_start_byte,
                    byte_end: byte_at($i),
                    utf16_start: word_start_utf16,
                    utf16_end: utf16_at($i),
                    visible_start: word_start_visible,
                    letters: buf_chars,
                };
                on_word(&buf, span);
            }
            buf.clear();
            buf_chars = 0;
        }};
    }

    let mut i = 0usize;
    while i < len {
        let c = char_table[i].2;

        // ── HTML TAGS ────────────────────────────────────────
        if in_tag {
            if c == '>' {
                if html_tags::is_word_break_tag(&tag_name) {
                    flush!(i);
                }
                in_tag = false;
                tag_name.clear();
            } else if !tag_name_done {
                // Stop accumulating at the first space — everything after it is
                // attributes, which must not corrupt the tag name the flush
                // decision above depends on.
                if c == ' ' || c == '\t' {
                    tag_name_done = true;
                } else if tag_name.len() < TAG_NAME_CAP && c != '/' {
                    tag_name.push(c);
                }
            }
            i += 1;
            continue;
        }

        if c == '<' {
            // Word building continues ACROSS the tag — no flush here. Inline
            // formatting (<b>, <i>, <small>, <span>…) interrupts words
            // mid-letter (emphasised letters, e.g. ורא<b>ה</b>); flushing on
            // every '<' indexed those words as unfindable fragments. The flush
            // decision is deferred to '>' above, once the tag name is known:
            // block tags and sup/sub break the word, everything else is
            // transparent.
            in_tag = true;
            tag_name.clear();
            tag_name_done = false;
            i += 1;
            continue;
        }

        // ── HTML ENTITIES ────────────────────────────────────
        if c == '&' {
            // Every entity — well-formed or a bare '&' — is a word separator.
            // Flush at the '&' (the entity is the separator, so it is NOT part
            // of the word's raw span); the whole entity stays outside every
            // token span.
            flush!(i);
            html_tags::skip_entity(&char_table, len, &mut i);
            visible += 1; // the entity renders as one visible char
            i += 1;
            continue;
        }

        // ── MAQAF ─ word-joining hyphen acts as separator ────
        if c == '\u{05BE}' {
            flush!(i);
            visible += 1;
            i += 1;
            continue;
        }

        // ── NIKUD + CANTILLATION: strip or keep ──────────────
        if chars::is_strippable_mark(c) {
            if keep_marks && buf_chars > 0 {
                // Kept in the token text (never counted as a letter).
                buf.push(c);
            }
            i += 1;
            continue;
        }

        // The Unicode category lookup is a table search; Hebrew and ASCII letters
        // (the vast majority of chars) are never non-spacing marks, so skip it
        // for them. Same result, ~1 lookup per non-letter char instead of per char.
        if c as u32 > 127 && !chars::is_letter(c) && get_general_category(c) == GeneralCategory::NonspacingMark {
            i += 1;
            continue;
        }

        // ── WORD BUILDING ────────────────────────────────────
        if chars::is_letter(c) {
            let c = chars::to_lower_ascii(c);

            // Script-boundary split: a Hebrew↔Latin transition inside a letter
            // run is a word boundary. Prevents mixed-script blobs — OCR bleed
            // (bדה), a Hebrew prefix on a foreign word (הOU), or entity residue
            // (בשבת&amp) — from being indexed as one unsearchable token.
            if buf_chars > 0 && chars::is_latin_lower(c) != buf_first_latin {
                flush!(i); // emits the completed run (if len 2..29), resets the buffer
            }

            if buf_chars == 0 {
                word_start_byte = char_table[i].0;
                word_start_utf16 = char_table[i].1;
                word_start_visible = visible;
                buf_first_latin = chars::is_latin_lower(c);
            }

            buf.push(c);
            buf_chars += 1;
            visible += 1;
        } else if chars::is_intra_word_quote(c) && buf_chars > 0 {
            // Hebrew geresh/gershayim and ASCII quotes appearing inside a word
            // (e.g. רשב"א, רש"י) are KEPT in the token, normalized to a
            // canonical '"' (all four forms: " ' ׳ ״) — the index preserves
            // them and the walks optionally treat them as transparent
            // (ignore-gershayim option). They do not count toward the 2..29
            // letter length rule (buf_chars counts letters only).
            buf.push('"');
            visible += 1;
        } else {
            flush!(i);
            // Non-letter separators (space, punctuation, etc.) count as
            // visible. The C# original counts UTF-16 units.
            visible += c.len_utf16();
        }

        i += 1;
    }

    flush!(len);
    total_utf16
}
