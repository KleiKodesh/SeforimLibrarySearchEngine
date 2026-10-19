//! Port of FtsLib `HtmlWordScanner.cs` + `TokenStream.cs` (KitveiHakodesh, git HEAD).
//!
//! Single-pass HTML-aware word scanner. Handles tag detection, entity skipping,
//! nikud/cantillation stripping, and word boundary detection. Every complete
//! word is reported with its raw source span (byte AND UTF-16 offsets into the
//! original string) and its normalized form.
//!
//! The C# original indexes strings by UTF-16 code units; this port walks a
//! precomputed `(byte, utf16, char)` table so the control flow mirrors the
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
}

/// Scans `text` and calls `on_word(normalized, span)` for each complete word
/// of normalized length 2..=29, in order.
///
/// Rules (identical to the C# scanner):
/// - Inline tags (`<b>`, `<i>`, `<span>`, …) are transparent: a word continues
///   across them. Block tags and `sup`/`sub` end the word.
/// - Every HTML entity (well-formed or a bare `&`) is a word separator and is
///   kept outside the word's raw span.
/// - Maqaf (U+05BE) is a word separator.
/// - Nikud + cantillation (and any other non-spacing mark) are stripped.
/// - Letters are Hebrew alef-tav and ASCII (lowercased); a Hebrew↔Latin
///   transition inside a letter run is a word boundary.
/// - Geresh/gershayim and ASCII quotes inside a word are transparent (רש"י → רשי).
#[allow(unused_assignments)] // the final flush!'s buffer reset is never read
pub fn scan(text: &str, mut on_word: impl FnMut(&str, WordSpan)) {
    if text.is_empty() {
        return;
    }

    // (byte offset, utf16 offset, char) for every char — lets the main loop use
    // plain indices and arbitrary forward jumps, exactly like the C# for-loop.
    let mut char_table: Vec<(usize, usize, char)> = Vec::with_capacity(text.len());
    let mut u16_pos = 0usize;
    for (byte_pos, c) in text.char_indices() {
        char_table.push((byte_pos, u16_pos, c));
        u16_pos += c.len_utf16();
    }
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
                let span = WordSpan {
                    byte_start: word_start_byte,
                    byte_end: byte_at($i),
                    utf16_start: word_start_utf16,
                    utf16_end: utf16_at($i),
                    visible_start: word_start_visible,
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

        // ── NIKUD + CANTILLATION REMOVAL ─────────────────────
        if chars::is_strippable_mark(c) {
            i += 1;
            continue;
        }

        if c as u32 > 127 && get_general_category(c) == GeneralCategory::NonspacingMark {
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
}

impl TextTokens {
    /// Tokenizes `text`. The tokens AND every token's normalized view are
    /// reused on the next call — copy what you keep.
    pub fn tokenize(&mut self, text: &str) -> &[TextToken] {
        self.norm.clear();
        self.tokens.clear();
        let norm = &mut self.norm;
        let tokens = &mut self.tokens;
        scan(text, |word, span| {
            let norm_start = norm.len();
            norm.push_str(word);
            tokens.push(TextToken {
                raw_start: span.byte_start,
                raw_end: span.byte_end,
                utf16_start: span.utf16_start,
                utf16_end: span.utf16_end,
                visible_start: span.visible_start,
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
    fn intra_word_quotes_kept_canonical() {
        // Quotes are kept in the token, normalized to '"'; they don't count
        // toward the 2..29 letter rule.
        assert_eq!(words("רש\"י אמר"), ["רש\"י", "אמר"]);
        assert_eq!(words("רשב\u{05F4}א"), ["רשב\"א"]); // gershayim → '"'
        assert_eq!(words("ר' עקיבא"), ["עקיבא"]); // ר' is 1 letter → dropped
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
        assert_eq!(words("אב<sup>1</sup>גד"), ["אב", "גד"]);
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
    fn length_filter() {
        // 1-letter words dropped
        assert_eq!(words("ב של"), ["של"]);
        // 30+ letter words dropped
        let long = "א".repeat(30);
        assert_eq!(words(&long), Vec::<String>::new());
        let ok = "א".repeat(29);
        assert_eq!(words(&ok), [ok.clone()]);
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
}
