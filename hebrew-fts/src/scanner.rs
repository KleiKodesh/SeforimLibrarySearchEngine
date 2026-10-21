//! Port of FtsLib `HtmlWordScanner.cs` + `TokenStream.cs` (KitveiHakodesh, git HEAD).
//!
//! Single-pass HTML-aware word scanner. Handles tag detection, entity skipping,
//! nikud/cantillation stripping, and word boundary detection. Every complete
//! word is reported with its raw source span (byte AND UTF-16 offsets into the
//! original string) and its normalized form.
//!
//! The C# original indexes strings by UTF-16 code units; this port walks the
//! UTF-8 bytes carrying both byte and UTF-16 offsets, so the control flow mirrors the
//! original index-based loop exactly, while emitting byte offsets (natural for
//! Rust slicing / tantivy) alongside UTF-16 offsets (what the JS frontend and
//! the C# descriptors use).

use unicode_general_category::{get_general_category, GeneralCategory};

use crate::chars;
use crate::html_tags;

/// Maximum number of chars accumulated for a tag name (as in the C# original).
const TAG_NAME_CAP: usize = 16;

/// A word's location, reported to the scan callback. The normalized form is
/// passed separately as `&str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordSpan {
    /// Byte index of the first letter of the word in the original string
    /// (points into the raw HTML, before any nikud was stripped).
    pub byte_start: usize,
    /// Byte index just past the separator that ended the word.
    pub byte_end: usize,
    /// Same span in UTF-16 code units (C#/JS string indices).
    pub utf16_start: usize,
    /// UTF-16 end, exclusive.
    pub utf16_end: usize,
    /// Cumulative count of visible characters up to but not including the first
    /// letter of this word. Used by the snippet window locator to measure
    /// window length without re-scanning.
    pub visible_start: usize,
    /// Number of letters and digits in the word (1+). Quotes and kept marks
    /// in the normalized text are not counted. This is FtsLib's `NormLen`:
    /// the word's visible width.
    pub letters: usize,
}

/// Scans `text` and calls `on_word(normalized, span)` for each complete word,
/// in order.
///
/// Rules (identical to the C# scanner; its word model is Otzaria's):
/// - A word is a run of letters and ASCII digits, of any length, one letter
///   included (`ו`, `12`, `ב5`). A word whose term is over
///   [`MAX_TERM_BYTES`](crate::chars::MAX_TERM_BYTES) is still reported (it
///   is a word for positions and distances); the index tokenizer leaves it
///   out of the terms.
/// - Inline tags (`<b>`, `<i>`, `<span>`, …) are transparent: a word continues
///   across them. Block tags and `sup`/`sub` end the word.
/// - Every HTML entity (well-formed or a bare `&`) is a word separator and is
///   kept outside the word's raw span.
/// - Maqaf (U+05BE) is a word separator.
/// - Nikud + cantillation (and any other non-spacing mark) are stripped.
/// - Letters are Hebrew alef-tav and ASCII (lowercased); a Hebrew↔Latin
///   transition inside a letter run is a word boundary. Digits belong to no
///   script (`5ab`, `ב5`).
/// - Geresh/gershayim and ASCII quotes inside a word are transparent (רש"י → רשי).
#[allow(unused_assignments)] // the final flush!'s buffer reset is never read
pub fn scan(text: &str, on_word: impl FnMut(&str, WordSpan)) -> usize {
    scan_with(text, false, on_word)
}

/// Like [`scan`], but when `keep_marks` is true, nikud and te'amim inside a
/// word are KEPT in the normalized token text (they never count as letters). Use for fields that index vocalized text; the walks
/// can then optionally treat the marks as transparent.
///
/// Returns the text's length in UTF-16 code units (C#'s `string.Length`),
/// known from the same pass.
///
/// Implementation: the C# original is an index loop over UTF-16 units. This
/// port walks the UTF-8 bytes directly, carrying the current char's byte AND
/// UTF-16 offsets (`b`, `u`) — every place the C# reads `i` reads one of
/// them. The control flow (branch order, flush points, entity skip, visible
/// counting) is the C# loop's, step for step; the pre-optimisation version
/// that materialised a `(byte, utf16, char)` table per line is kept in
/// `scanner_reference.rs` and a property test asserts identical output.
#[allow(unused_assignments)] // the final flush!'s buffer reset is never read
pub fn scan_with(text: &str, keep_marks: bool, mut on_word: impl FnMut(&str, WordSpan)) -> usize {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return 0;
    }

    // Normalized word buffer — reused across words.
    let mut buf = String::with_capacity(64);
    let mut buf_chars = 0usize; // letters + digits in buf (BMP: chars == UTF-16 units)
    let mut run_latin: Option<bool> = None; // script of the word's letters; None = digits only so far
    // Byte length of `buf` up to and including its last letter, and whether a
    // quote was pushed after that letter. Tracked while building so flush!
    // need not re-scan the word for its last letter (same result: buf only
    // ever holds letters, kept marks and '"').
    let mut last_letter_end = 0usize;
    let mut quote_in_tail = false;

    // Tag name buffer (fixed array: the name is capped at TAG_NAME_CAP chars).
    let mut tag_name = ['\0'; TAG_NAME_CAP];
    let mut tag_len = 0usize;
    let mut in_tag = false;
    let mut tag_name_done = false; // first space seen — attribute chars must not join the name

    let mut word_start_byte = 0usize;
    let mut word_start_utf16 = 0usize;
    let mut word_start_visible = 0usize;
    let mut visible = 0usize; // cumulative visible chars up to current position

    // Current char position: byte offset and UTF-16 offset.
    let mut b = 0usize;
    let mut u = 0usize;

    macro_rules! flush {
        ($end_b:expr, $end_u:expr) => {{
            if buf_chars > 0 {
                // A quote is part of the word only BETWEEN letters (רש"י). One
                // after the last letter is quotation punctuation (יצחק"): FtsLib
                // drops it, so it must not become part of the term. The tail
                // after the last letter holds only kept marks and quotes.
                if quote_in_tail {
                    let tail: String = buf[last_letter_end..].chars().filter(|&c| c != '"').collect();
                    buf.truncate(last_letter_end);
                    buf.push_str(&tail);
                }
                let span = WordSpan {
                    byte_start: word_start_byte,
                    byte_end: $end_b,
                    utf16_start: word_start_utf16,
                    utf16_end: $end_u,
                    visible_start: word_start_visible,
                    letters: buf_chars,
                };
                on_word(&buf, span);
            }
            buf.clear();
            buf_chars = 0;
            run_latin = None;
            last_letter_end = 0;
            quote_in_tail = false;
        }};
    }

    while b < n {
        // ── FAST PATHS (outside tags) ───────────────────────
        // The two commonest chars by far, handled before the general decode.
        // Each does exactly what the general branches below do for that char
        // (the branches are disjoint, so testing these first changes nothing).
        if !in_tag {
            let x = bytes[b];
            // Hebrew letter alef..tav = U+05D0..U+05EA = D7 90..D7 AA.
            if x == 0xD7 && (0x90..=0xAA).contains(&bytes[b + 1]) {
                // = the WORD BUILDING branch for a non-Latin letter.
                if run_latin == Some(true) {
                    flush!(b, u); // script boundary
                }
                run_latin = Some(false);
                if buf_chars == 0 {
                    word_start_byte = b;
                    word_start_utf16 = u;
                    word_start_visible = visible;
                }
                buf.push_str(&text[b..b + 2]);
                buf_chars += 1;
                last_letter_end = buf.len();
                quote_in_tail = false;
                visible += 1;
                b += 2;
                u += 1;
                continue;
            }
            // Nikud / te'amim U+0591..U+05BF = D6 91..D6 BF, minus the maqaf
            // U+05BE (D6 BE) = the strip-or-keep branch. (The few strippable
            // marks above U+05BF take the general path.)
            if x == 0xD6 && bytes[b + 1] >= 0x91 && bytes[b + 1] != 0xBE {
                if keep_marks && buf_chars > 0 {
                    buf.push_str(&text[b..b + 2]);
                }
                b += 2;
                u += 1;
                continue;
            }
            // ASCII space = the final separator branch.
            if x == b' ' {
                flush!(b, u);
                visible += 1;
                b += 1;
                u += 1;
                continue;
            }
        }

        let (c, clen) = decode_char(bytes, b);
        // A 4-byte UTF-8 char is a surrogate pair in UTF-16; all others are one unit.
        let ulen = if clen == 4 { 2 } else { 1 };

        // ── HTML TAGS ────────────────────────────────────────
        if in_tag {
            if c == '>' {
                if html_tags::is_word_break_tag(&tag_name[..tag_len]) {
                    flush!(b, u);
                }
                in_tag = false;
                tag_len = 0;
            } else if !tag_name_done {
                // Stop accumulating at the first space — everything after it is
                // attributes, which must not corrupt the tag name the flush
                // decision above depends on.
                if c == ' ' || c == '\t' {
                    tag_name_done = true;
                } else if tag_len < TAG_NAME_CAP && c != '/' {
                    tag_name[tag_len] = c;
                    tag_len += 1;
                }
            } else {
                // Attributes: nothing happens until '>'. Jump straight to it
                // ('>' is ASCII, so it never occurs inside a multi-byte char),
                // counting the skipped UTF-16 units.
                let rest = &bytes[b..];
                let skip = rest.iter().position(|&x| x == b'>').unwrap_or(rest.len());
                u += utf16_len(&rest[..skip]);
                b += skip;
                continue;
            }
            b += clen;
            u += ulen;
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
            tag_len = 0;
            tag_name_done = false;
            b += 1;
            u += 1;
            continue;
        }

        // ── HTML ENTITIES ────────────────────────────────────
        if c == '&' {
            // Every entity — well-formed or a bare '&' — is a word separator.
            // Flush at the '&' (the entity is the separator, so it is NOT part
            // of the word's raw span); the whole entity stays outside every
            // token span.
            flush!(b, u);
            skip_entity(bytes, &mut b, &mut u);
            visible += 1; // the entity renders as one visible char
            b += 1; // past the '&' (bare) or the ';' (well-formed)
            u += 1;
            continue;
        }

        // ── MAQAF ─ word-joining hyphen acts as separator ────
        if c == '\u{05BE}' {
            flush!(b, u);
            visible += 1;
            b += clen;
            u += ulen;
            continue;
        }

        // ── NIKUD + CANTILLATION: strip or keep ──────────────
        if chars::is_strippable_mark(c) {
            if keep_marks && buf_chars > 0 {
                // Kept in the token text (never counted as a letter).
                buf.push(c);
            }
            b += clen;
            u += ulen;
            continue;
        }

        // The Unicode category lookup is a table search; Hebrew and ASCII letters
        // (the vast majority of chars) are never non-spacing marks, so skip it
        // for them. Same result, ~1 lookup per non-letter char instead of per char.
        if c as u32 > 127 && !chars::is_letter(c) && get_general_category(c) == GeneralCategory::NonspacingMark {
            b += clen;
            u += ulen;
            continue;
        }

        // ── WORD BUILDING ────────────────────────────────────
        if chars::is_word_char(c) {
            let c = chars::to_lower_ascii(c);

            // Script-boundary split: a Hebrew↔Latin transition inside a letter
            // run is a word boundary. Prevents mixed-script blobs — OCR bleed
            // (bדה), a Hebrew prefix on a foreign word (הOU), or entity residue
            // (בשבת&amp) — from being indexed as one unsearchable token.
            // Digits belong to no script: they never split and join the run.
            if !chars::is_digit(c) {
                let latin = chars::is_latin_lower(c);
                if run_latin.is_some_and(|l| l != latin) {
                    flush!(b, u); // emits the completed run, resets the buffer
                }
                run_latin = Some(latin);
            }

            if buf_chars == 0 {
                word_start_byte = b;
                word_start_utf16 = u;
                word_start_visible = visible;
            }

            buf.push(c);
            buf_chars += 1;
            last_letter_end = buf.len();
            quote_in_tail = false;
            visible += 1;
        } else if chars::is_intra_word_quote(c) && buf_chars > 0 {
            // Hebrew geresh/gershayim and ASCII quotes appearing inside a word
            // (e.g. רשב"א, רש"י) are KEPT in the token, normalized to a
            // canonical '"' (all four forms: " ' ׳ ״) — the index preserves
            // them and the walks optionally treat them as transparent
            // (ignore-gershayim option). They are not letters (buf_chars
            // counts letters and digits only).
            buf.push('"');
            quote_in_tail = true;
            visible += 1;
        } else {
            flush!(b, u);
            // Non-letter separators (space, punctuation, etc.) count as
            // visible. The C# original counts UTF-16 units.
            visible += ulen;
        }

        b += clen;
        u += ulen;
    }

    flush!(n, u);
    u
}

/// Decodes the char starting at byte `b` of valid UTF-8, with its byte length.
#[inline(always)]
fn decode_char(bytes: &[u8], b: usize) -> (char, usize) {
    let x = bytes[b];
    if x < 0x80 {
        return (x as char, 1);
    }
    let (cp, len) = if x < 0xE0 {
        (((x as u32 & 0x1F) << 6) | (bytes[b + 1] as u32 & 0x3F), 2)
    } else if x < 0xF0 {
        (
            ((x as u32 & 0x0F) << 12) | ((bytes[b + 1] as u32 & 0x3F) << 6) | (bytes[b + 2] as u32 & 0x3F),
            3,
        )
    } else {
        (
            ((x as u32 & 0x07) << 18)
                | ((bytes[b + 1] as u32 & 0x3F) << 12)
                | ((bytes[b + 2] as u32 & 0x3F) << 6)
                | (bytes[b + 3] as u32 & 0x3F),
            4,
        )
    };
    // SAFETY: `bytes` comes from a `&str` and `b` is always on a char
    // boundary, so this is a valid scalar value.
    (unsafe { char::from_u32_unchecked(cp) }, len)
}

/// UTF-16 length of a run of whole UTF-8 chars: one unit per char (count of
/// non-continuation bytes) plus one more per 4-byte char (a surrogate pair).
#[inline]
fn utf16_len(bytes: &[u8]) -> usize {
    bytes.iter().map(|&x| usize::from((x & 0xC0) != 0x80) + usize::from(x >= 0xF0)).sum()
}

/// FtsLib's entity skip on the byte walk. `(b, u)` is the `&`. A well-formed
/// entity is `&`, then 1+ chars starting less than 10 UTF-16 units after the
/// `&`'s successor, then `;` — the first char at offset ≥10 or the first `;`
/// ends the probe, and the entity is accepted only if that char is a `;` and
/// not the `&`'s direct successor. On success `(b, u)` moves to the `;` (the
/// caller then steps past it); otherwise it stays on the `&`. Identical to the
/// table form kept in `html_tags::skip_entity` for the reference scanner.
#[inline]
fn skip_entity(bytes: &[u8], b: &mut usize, u: &mut usize) {
    let n = bytes.len();
    let start_b = *b + 1;
    if start_b >= n {
        return;
    }
    let start_u = *u + 1;
    let (mut eb, mut eu) = (start_b, start_u);
    loop {
        if eb >= n {
            return; // ran off the end: malformed
        }
        let (c, clen) = decode_char(bytes, eb);
        if eu - start_u >= 10 || c == ';' {
            if c == ';' && eb > start_b {
                *b = eb;
                *u = eu;
            }
            return;
        }
        eb += clen;
        eu += if clen == 4 { 2 } else { 1 };
    }
}

/// A single word token produced by [`TextTokens::tokenize`].
///
/// The normalized form is stored as a slice into the producing [`TextTokens`]'
/// shared, reused buffer — resolve it with [`TextTokens::norm_of`]. It is only
/// valid until the next `tokenize` call (the buffer is reused) — the same
/// lifetime as the token list.
#[derive(Debug, Clone, Copy)]
pub struct TextToken {
    /// Byte index of the first letter of the word in the original raw string.
    pub raw_start: usize,
    /// Byte index just past the separator that ended the word.
    pub raw_end: usize,
    /// Same span in UTF-16 code units.
    pub utf16_start: usize,
    /// UTF-16 end, exclusive.
    pub utf16_end: usize,
    /// Cumulative count of visible characters up to but not including the
    /// first letter of this token.
    pub visible_start: usize,
    /// Letters in the word (FtsLib `NormLen`): its visible width, excluding
    /// quotes and kept marks.
    pub letters: usize,
    norm_start: usize,
    norm_len: usize,
}

/// Produces [`TextToken`]s from an HTML string, preserving each word's raw
/// character positions alongside its normalized form. The normalized text of
/// every token is packed into one reused buffer, so tokenizing a line
/// allocates no per-token strings. Used by the highlighter to locate match
/// spans in the original source.
#[derive(Default)]
pub struct TextTokens {
    norm: String,
    tokens: Vec<TextToken>,
    keep_marks: bool,
    raw_utf16_len: usize,
}

impl TextTokens {
    /// A tokenizer that keeps nikud/te'amim in the normalized token text
    /// (see [`scan_with`]).
    pub fn keeping_marks() -> TextTokens {
        TextTokens {
            keep_marks: true,
            ..TextTokens::default()
        }
    }

    /// Tokenizes `text`. The tokens AND every token's normalized view are
    /// reused on the next call — copy what you keep.
    pub fn tokenize(&mut self, text: &str) -> &[TextToken] {
        self.norm.clear();
        self.tokens.clear();
        let norm = &mut self.norm;
        let tokens = &mut self.tokens;
        self.raw_utf16_len = scan_with(text, self.keep_marks, |word, span| {
            let norm_start = norm.len();
            norm.push_str(word);
            tokens.push(TextToken {
                raw_start: span.byte_start,
                raw_end: span.byte_end,
                utf16_start: span.utf16_start,
                utf16_end: span.utf16_end,
                visible_start: span.visible_start,
                letters: span.letters,
                norm_start,
                norm_len: word.len(),
            });
        });
        &self.tokens
    }

    /// The tokens from the last [`tokenize`](Self::tokenize) call.
    pub fn tokens(&self) -> &[TextToken] {
        &self.tokens
    }

    /// Length in UTF-16 code units of the text last tokenized (C#'s
    /// `rawHtml.Length`).
    pub fn raw_utf16_len(&self) -> usize {
        self.raw_utf16_len
    }

    /// Normalized form (nikud stripped, ASCII lowercased) of a token from the
    /// last `tokenize` call.
    pub fn norm_of(&self, token: &TextToken) -> &str {
        &self.norm[token.norm_start..token.norm_start + token.norm_len]
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    fn words(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        scan(text, |w, _| out.push(w.to_string()));
        out
    }

    fn spans(text: &str) -> Vec<(String, WordSpan)> {
        let mut out = Vec::new();
        scan(text, |w, s| out.push((w.to_string(), s)));
        out
    }

    #[test]
    fn plain_words() {
        assert_eq!(words("שלום עולם"), ["שלום", "עולם"]);
    }

    #[test]
    fn nikud_stripped() {
        // בְּרֵאשִׁית → בראשית
        assert_eq!(words("בְּרֵאשִׁית בָּרָא"), ["בראשית", "ברא"]);
    }

    #[test]
    fn marks_kept_when_requested() {
        let mut out = Vec::new();
        scan_with("בְּרֵאשִׁית בָּרָא", true, |w, _| out.push(w.to_string()));
        assert_eq!(out, ["בְּרֵאשִׁית", "בָּרָא"]);
        // a one-letter word keeps its marks too
        let mut out2 = Vec::new();
        scan_with("בְּ שם", true, |w, _| out2.push(w.to_string()));
        assert_eq!(out2, ["בְּ", "שם"]);
    }

    #[test]
    fn intra_word_quotes_kept_canonical() {
        // Quotes are kept in the token, normalized to '"'; they don't count
        // as letters.
        assert_eq!(words("רש\"י אמר"), ["רש\"י", "אמר"]);
        assert_eq!(words("רשב\u{05F4}א"), ["רשב\"א"]); // gershayim → '"'
        assert_eq!(words("ר' עקיבא"), ["ר", "עקיבא"]); // trailing quote is punctuation
        // a quote AFTER the last letter is punctuation, not part of the word
        assert_eq!(words("\"יצחק\" אמר"), ["יצחק", "אמר"]);
        assert_eq!(words("תורה'' שלמה"), ["תורה", "שלמה"]);
        // its raw span still ends at the separator (FtsLib's rawEnd)
        let s = spans("יצחק\" אמר");
        assert_eq!(s[0].1.utf16_end, 5);
    }

    #[test]
    fn maqaf_splits() {
        assert_eq!(words("על־פני"), ["על", "פני"]);
    }

    #[test]
    fn inline_tag_transparent() {
        assert_eq!(words("ורא<b>ה</b> טוב"), ["וראה", "טוב"]);
    }

    #[test]
    fn block_tag_splits() {
        assert_eq!(words("אב<br>גד"), ["אב", "גד"]);
        assert_eq!(words("אב<div class=\"x\">גד</div>"), ["אב", "גד"]);
    }

    #[test]
    fn sup_sub_split() {
        // the footnote number is a word of its own (digits are word chars)
        assert_eq!(words("אב<sup>1</sup>גד"), ["אב", "1", "גד"]);
    }

    #[test]
    fn entity_splits() {
        assert_eq!(words("אב&nbsp;גד"), ["אב", "גד"]);
        assert_eq!(words("אב&גד"), ["אב", "גד"]); // bare '&'
    }

    #[test]
    fn script_boundary_splits() {
        assert_eq!(words("abcדה"), ["abc", "דה"]);
        assert_eq!(words("דהabc"), ["דה", "abc"]);
    }

    #[test]
    fn ascii_lowercased() {
        assert_eq!(words("HeLLo"), ["hello"]);
    }

    #[test]
    fn every_word_any_length() {
        // one-letter words are words (Otzaria's word model)
        assert_eq!(words("ב של"), ["ב", "של"]);
        // so are long words: the scanner reports them, the index tokenizer
        // decides on the term
        let long = "א".repeat(70);
        assert_eq!(words(&long), [long.clone()]);
    }

    #[test]
    fn digits_are_word_chars() {
        // a digit run is a word; digits join the letters around them
        assert_eq!(words("סימן 12 ב5 אב3גד"), ["סימן", "12", "ב5", "אב3גד"]);
        // digits are script-neutral: only letters of the other script split
        assert_eq!(words("5ab a5ב 3.14"), ["5ab", "a5", "ב", "3", "14"]);
        // a quote between digits / letters stays inside, a trailing one does not
        assert_eq!(words("ה'3 12\""), ["ה\"3", "12"]);
        // the visible width counts digits
        let s = spans("ב5 אב");
        assert_eq!((s[0].1.letters, s[1].1.visible_start), (2, 3));
    }

    #[test]
    fn raw_spans_point_into_source() {
        let text = "שָׁלוֹם עולם";
        let s = spans(text);
        assert_eq!(s.len(), 2);
        let (w0, sp0) = &s[0];
        assert_eq!(w0, "שלום");
        // the raw span covers the pointed word incl. nikud, up to the separator
        assert_eq!(&text[sp0.byte_start..sp0.byte_end], "שָׁלוֹם");
        // utf16 offsets: C#/JS indices
        assert_eq!(sp0.utf16_start, 0);
        assert_eq!(sp0.utf16_end, 7); // 7 UTF-16 units in שָׁלוֹם
        let (w1, sp1) = &s[1];
        assert_eq!(w1, "עולם");
        assert_eq!(&text[sp1.byte_start..sp1.byte_end], "עולם");
        assert_eq!(sp1.utf16_end, 12);
    }

    #[test]
    fn visible_counts() {
        // "אב גד": visible chars are letters + separator
        let s = spans("אבג דהו");
        assert_eq!(s[0].1.visible_start, 0);
        assert_eq!(s[1].1.visible_start, 4); // 3 letters + 1 space
                                             // nikud is invisible
        let s = spans("אָבָג דהו");
        assert_eq!(s[1].1.visible_start, 4);
        // entity counts as one visible char
        let s = spans("אבג&nbsp;דהו");
        assert_eq!(s[1].1.visible_start, 4);
        // intra-word quote is visible but must not shift the word's own start
        let s = spans("רש\"י דהו");
        assert_eq!(s[0].1.visible_start, 0);
        assert_eq!(s[1].1.visible_start, 5); // 3 letters + quote + space
    }

    #[test]
    fn text_tokens_reuse() {
        let mut tt = TextTokens::default();
        let toks: Vec<_> = tt.tokenize("רש\"י על־התורה").to_vec();
        assert_eq!(toks.len(), 3);
        assert_eq!(tt.norm_of(&toks[0]), "רש\"י");
        assert_eq!(tt.norm_of(&toks[1]), "על");
        assert_eq!(tt.norm_of(&toks[2]), "התורה");
        let toks2: Vec<_> = tt.tokenize("שלום").to_vec();
        assert_eq!(toks2.len(), 1);
        assert_eq!(tt.norm_of(&toks2[0]), "שלום");
    }

    #[test]
    fn word_continues_across_inline_tag_span() {
        // raw span of a word interrupted by an inline tag includes the tag
        let text = "ורא<b>ה</b> טוב";
        let s = spans(text);
        assert_eq!(s[0].0, "וראה");
        assert_eq!(&text[s[0].1.byte_start..s[0].1.byte_end], "ורא<b>ה</b>");
    }

    /// Everything the scanner reports, for comparing two implementations.
    fn scan_all(f: fn(&str, bool, &mut dyn FnMut(&str, WordSpan)) -> usize, text: &str, keep: bool) -> (Vec<(String, WordSpan)>, usize) {
        let mut out = Vec::new();
        let n = f(text, keep, &mut |w, s| out.push((w.to_string(), s)));
        (out, n)
    }
    fn new_impl(t: &str, k: bool, cb: &mut dyn FnMut(&str, WordSpan)) -> usize {
        crate::scanner::scan_with(t, k, cb)
    }
    fn old_impl(t: &str, k: bool, cb: &mut dyn FnMut(&str, WordSpan)) -> usize {
        crate::scanner_reference::scan_with(t, k, cb)
    }

    fn assert_same(text: &str) {
        for keep in [false, true] {
            assert_eq!(scan_all(new_impl, text, keep), scan_all(old_impl, text, keep), "keep_marks={keep} text={text:?}");
        }
    }

    #[test]
    fn matches_reference_on_edge_cases() {
        let long_heb = "א".repeat(40);
        let long_lat = "x".repeat(31);
        let cases = [
            "", "א", "אב", "&", "&;", "&a;", "אב&", "אב&;גד", "<", ">", "<b", "אב<b", "אב<div class=\"x\"",
            "אב<sup>1</sup>גד", "אב<SUB>x</SUB>גד", "אב</p>גד", "אב<p/>גד", "אב<br />גד", "אב<!-- c -->גד",
            "אב<span\tstyle='a>b'>גד", "אב<abcdefghijklmnopqrstuvwxyz>גד", "אב<divx>גד", "אב<דיב>גד",
            "אב&nbsp;גד", "אב&#1234;גד", "אב&abcdefghij;גד", "אב&abcdefghi;גד", "אב&abcdefgh😀;גד",
            "אב&abcdefgh😀x;גד", "אב&😀;גד", "&&&;;", "על־פני", "רש\"י", "רש\"\"", "יצחק\"", "יצחק\u{05B0}\"\u{05B0}",
            "ר' עקיבא", "תורה'' שלמה", "רשב\u{05F4}א", "ג\u{05F3}", "\"אב\"", "בְּרֵאשִׁית בָּרָא", "בְּ שם",
            "אב\u{0300}גד", "אב\u{05C0}גד", "אב\u{05C3}גד", "abcדה", "דהabc", "HeLLo WoRLD", "a\u{05B0}b",
            "😀אב😀גד😀", "אב\u{10000}", &long_heb, &long_lat, "אב\u{1F600}&nbsp;\u{1F600}גד", "\u{200F}אב\u{200E}",
            "אב<", "אב<b>", "אב\r\nגד\tהו", "ﬠאב", "אבּגד", "\u{05BE}\u{05BE}אב\u{05BE}",
        ];
        for c in cases {
            assert_same(c);
        }
    }

    #[test]
    fn matches_reference_on_generated_strings() {
        let pieces = [
            "א", "ב", "ג", "ש", "ת", "ך", "ם", "a", "Z", "q", "\u{05B0}", "\u{05B4}", "\u{0591}", "\u{05AF}",
            "\u{05BC}", "\u{05C1}", "\u{05C7}", "\u{05C0}", "\u{05C3}", "\u{05C6}", "\u{05BE}", "\"", "'", "\u{05F3}",
            "\u{05F4}", " ", "  ", ",", ".", ":", "1", "<b>", "</b>", "<i>", "<span class=\"x\">", "</span>", "<br>",
            "<br/>", "<p>", "</p>", "<div id='a'>", "<sup>", "</sup>", "<sub>", "</sub>", "<SUP>", "<small>", "<",
            ">", "/", "&nbsp;", "&amp;", "&#160;", "&", ";", "&abcdefghijkl;", "&x", "😀", "\u{10FFFF}", "é",
            "\u{0300}", "\u{200F}", "\t", "\n", "אבגדהוזחטיכלמנסעפצקרשת", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef",
        ];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 40) as usize;
            let mut s = String::new();
            for _ in 0..len {
                s.push_str(pieces[(next() % pieces.len() as u64) as usize]);
            }
            assert_same(&s);
        }
    }
}
